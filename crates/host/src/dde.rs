//! Asking the desktop's own display service to lay the virtual monitor out.
//!
//! `xrandr` can name a mode, but on a DDE desktop it is not the program that
//! owns the layout. The display service re-reads the EDID of a connector it has
//! just seen arrive, applies the mode it prefers for that monitor and puts the
//! monitor where it thinks it belongs — so a raw ask that lands first is undone
//! a moment later, while the desktop's own memory of the monitor keeps the
//! shape the raw ask never wrote down. Asking the service that does the laying
//! out is what makes an answer stick, and what keeps the layout's owner from
//! being surprised by it.
//!
//! A desktop without the service — a bare X session, or one where `startdde` is
//! not running — gets no ask at all, and the caller falls back to `xrandr`.

use std::time::Duration;

use androscreen_core::edid::DisplayMode;

use crate::bus;

const SERVICE: &str = "org.deepin.dde.Display1";
const ROOT: &str = "/org/deepin/dde/Display1";
const MONITOR: &str = "org.deepin.dde.Display1.Monitor";
/// How long one property read or mode ask may take.
const CALL: Duration = Duration::from_millis(3000);
/// Laying the whole desktop out goes through every monitor, and the service is
/// allowed to take a moment over it. The ask still lands if only its answer is
/// late.
const APPLY: Duration = Duration::from_secs(12);

/// Asks the desktop to wear `display` at `at`, with the service that owns the
/// layout doing it.
///
/// Returns whether the service took the ask. Whether the monitor now wears it
/// is a question for the desktop's own listing, which is what the caller
/// watches.
pub(crate) fn ask(output: &str, display: DisplayMode, at: (i32, i32)) -> bool {
    let Some(monitor) = monitor_path(output) else {
        log::debug!("the display service has not listed {output}");
        return false;
    };
    let size = [display.width.to_string(), display.height.to_string()];
    if bus::call_limited(
        SERVICE,
        &monitor,
        MONITOR,
        "SetModeBySize",
        &["qq", &size[0], &size[1]],
        CALL,
    )
    .is_none()
    {
        log::debug!(
            "the display service will not set {output} to {}x{}",
            display.width,
            display.height
        );
        return false;
    }
    let place = [at.0.to_string(), at.1.to_string()];
    if bus::call_limited(
        SERVICE,
        &monitor,
        MONITOR,
        "SetPosition",
        &["nn", &place[0], &place[1]],
        CALL,
    )
    .is_none()
    {
        log::debug!(
            "the display service will not move {output} to {}+{}",
            at.0,
            at.1
        );
        return false;
    }
    // A late answer is not a refusal: the service is laying the desktop out and
    // the listing will say when it is wearing the ask.
    if bus::call_limited(SERVICE, ROOT, SERVICE, "ApplyChanges", &[], APPLY).is_none() {
        log::debug!("the display service is still laying {output} out");
    }
    true
}

/// The object the display service has for the monitor this X output became.
fn monitor_path(output: &str) -> Option<String> {
    bus::children(SERVICE, &format!("{ROOT}/Monitor_"), CALL)
        .into_iter()
        .find(|path| {
            bus::property_limited(SERVICE, path, MONITOR, "Name", CALL)
                .is_some_and(|name| value(&name) == output)
        })
}

/// The text a `busctl` property reply carries, without its variant type or the
/// quotes around a string.
fn value(reply: &str) -> &str {
    reply
        .trim()
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or_default()
        .trim_matches('"')
}

#[cfg(test)]
mod tests {
    use super::value;

    #[test]
    fn a_string_property_is_read_by_its_contents() {
        assert_eq!(value("s \"DVI-I-1-1\"\n"), "DVI-I-1-1");
        assert_eq!(value("s \"eDP-1\""), "eDP-1");
    }

    #[test]
    fn a_property_that_is_not_a_string_keeps_its_own_text() {
        assert_eq!(value("b true\n"), "true");
        assert_eq!(value(""), "");
    }
}
