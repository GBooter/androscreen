//! Chooses a virtual-display size that does not ask DDE to change its scale.
//!
//! The desktop treats the shorter edge of a connected monitor as evidence of how
//! much scale it can support. A narrow phone display can therefore make DDE call
//! its scaling service on every connection, even when the resulting value is the
//! same one the user already chose.

use androscreen_core::edid::DisplayMode;
use androscreen_core::protocol::ScreenRequest;

use crate::bus;

const XSETTINGS_SERVICE: &str = "org.deepin.dde.XSettings1";
const XSETTINGS_PATH: &str = "/org/deepin/dde/XSettings1";

/// The desktop accepts the sizes its own scaling rounds to, and rounding up
/// keeps the threshold the fit is chasing satisfied.
const DIMENSION_ALIGNMENT: u32 = 8;

/// Reads the scale currently applied to the desktop, if the service answers.
#[must_use]
pub(crate) fn current_global_scale() -> Option<f64> {
    let reply = bus::call_service(XSETTINGS_SERVICE, XSETTINGS_PATH, "GetScaleFactor", &[])?;
    parse_scale(&reply)
}

/// Returns a display at least as large as DDE's current scale requires.
///
/// The requested aspect ratio is retained. A mode that cannot satisfy its
/// threshold without exceeding the wire protocol's largest display is left
/// unchanged; an unnecessary enlargement would be worse than the original
/// desktop behavior for that uncommon configuration.
#[must_use]
pub(crate) fn fit_to_scale(mode: DisplayMode, scale: Option<f64>) -> DisplayMode {
    let Some(scale) = scale.filter(|scale| *scale > 1.0) else {
        return mode;
    };
    if !(mode.width..=ScreenRequest::MAX_DIMENSION).contains(&mode.width)
        || !(mode.height..=ScreenRequest::MAX_DIMENSION).contains(&mode.height)
    {
        return mode;
    }

    let required_short = required_short_edge(scale);
    let required_long = required_long_edge(scale);
    let current_short = mode.width.min(mode.height);
    let current_long = mode.width.max(mode.height);
    if current_short >= required_short && current_long >= required_long {
        return mode;
    }

    let short_factor = f64::from(required_short) / f64::from(current_short);
    let long_factor = f64::from(required_long) / f64::from(current_long);
    let factor = short_factor.max(long_factor);

    let width = align_up((f64::from(mode.width) * factor).ceil() as u32);
    let height = align_up((f64::from(mode.height) * factor).ceil() as u32);
    let fitted = DisplayMode {
        width,
        height,
        refresh_hz: mode.refresh_hz,
    };

    if fitted.width > ScreenRequest::MAX_DIMENSION
        || fitted.height > ScreenRequest::MAX_DIMENSION
        || !satisfies_scale(fitted, scale)
    {
        return mode;
    }

    if fitted == mode {
        return mode;
    }
    log::debug!(
        "using {}x{} for a {scale:.2} desktop scale instead of {}x{}",
        fitted.width,
        fitted.height,
        mode.width,
        mode.height
    );
    fitted
}

/// Whether the requested size can be worn exactly without asking DDE to change
/// the desktop's scale.
///
/// A monitor below the threshold makes DDE lower the scale of every screen and
/// asks the user to log out for it to take effect, so an exact size is only
/// worth keeping when the desktop's scale already fits inside it.
#[must_use]
pub(crate) fn pin_to_request(mode: DisplayMode, scale: Option<f64>) -> bool {
    fit_to_scale(mode, scale) == mode
}

fn satisfies_scale(mode: DisplayMode, scale: f64) -> bool {
    let short = mode.width.min(mode.height);
    let long = mode.width.max(mode.height);
    short >= required_short_edge(scale) && long >= required_long_edge(scale)
}

fn required_short_edge(scale: f64) -> u32 {
    scale_required(scale, 768.0)
}

fn required_long_edge(scale: f64) -> u32 {
    scale_required(scale, 1024.0)
}

fn scale_required(scale: f64, baseline: f64) -> u32 {
    (baseline * scale).ceil().max(1.0) as u32
}

fn align_up(value: u32) -> u32 {
    let remainder = value % DIMENSION_ALIGNMENT;
    if remainder == 0 {
        value
    } else {
        value + (DIMENSION_ALIGNMENT - remainder)
    }
}

fn parse_scale(reply: &str) -> Option<f64> {
    let mut words = reply.split_whitespace();
    if words.next() != Some("d") {
        return None;
    }
    let scale: f64 = words.next()?.parse().ok()?;
    scale.is_finite().then_some(scale)
}

#[cfg(test)]
mod tests {
    use super::{align_up, current_global_scale, fit_to_scale, parse_scale, pin_to_request};
    use androscreen_core::edid::DisplayMode;

    fn mode(width: u32, height: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_hz: 60,
        }
    }

    #[test]
    fn reads_the_variant_reply_of_get_scale_factor() {
        assert_eq!(parse_scale("d 1.75\n"), Some(1.75));
        assert_eq!(parse_scale("q 1.75\n"), None);
        assert_eq!(parse_scale("d not-a-number\n"), None);
    }

    #[test]
    fn a_narrow_phone_is_enlarged_only_as_far_as_the_current_scale_requires() {
        let fitted = fit_to_scale(mode(1080, 2460), Some(1.5));
        assert_eq!((fitted.width, fitted.height), (1152, 2624));
        assert!(fitted.width % 8 == 0 && fitted.height % 8 == 0);

        let fitted = fit_to_scale(mode(1080, 2460), Some(1.75));
        assert_eq!((fitted.width, fitted.height), (1344, 3064));
    }

    #[test]
    fn a_display_that_already_satisfies_the_scale_is_left_alone() {
        let requested = mode(1200, 2730);
        assert_eq!(fit_to_scale(requested, Some(1.5)), requested);
        assert_eq!(fit_to_scale(requested, Some(1.0)), requested);
        assert_eq!(fit_to_scale(requested, None), requested);
    }

    #[test]
    fn an_unreachable_aspect_ratio_is_not_distorted_to_satisfy_the_scale() {
        let requested = mode(1080, 2460);
        assert_eq!(fit_to_scale(requested, Some(2.25)), requested);
    }

    #[test]
    fn an_exact_size_is_kept_only_when_the_desktop_scale_can_carry_it() {
        assert!(pin_to_request(mode(1080, 1920), Some(1.0)));
        assert!(pin_to_request(mode(1080, 1920), Some(1.25)));
        assert!(!pin_to_request(mode(1080, 1920), Some(1.5)));
        assert!(!pin_to_request(mode(1080, 1920), Some(1.75)));
        assert!(pin_to_request(mode(1344, 2392), Some(1.75)));
        assert!(pin_to_request(mode(1080, 1920), None));
    }

    #[test]
    fn alignment_only_moves_edges_up() {
        assert_eq!(align_up(1079), 1080);
        assert_eq!(align_up(1080), 1080);
        assert_eq!(align_up(1081), 1088);
    }

    #[test]
    #[ignore = "requires the signed-in deepin desktop's XSettings service"]
    fn desktop_scale_reply_parses() {
        assert!(current_global_scale().is_some());
    }
}
