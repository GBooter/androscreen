//! Keeps the panel brightness across the virtual display coming and going.
//!
//! deepin's display daemon stores a brightness per output and reapplies the
//! whole map whenever the output list changes. A virtual monitor has no stored
//! level, so it arrives with the default `1.0` and the rewrite drags the laptop
//! panel to full brightness a few seconds after the mode is configured.
//! Snapshot the levels before the output appears, give the new output the
//! panel's own level, and write the saved levels back while the desktop reacts.
//!
//! Everything here goes through the session bus as the signed-in user, so it
//! works without privileges and does nothing on a desktop that has no
//! `org.deepin.dde.Display1`.

use std::collections::BTreeMap;
use std::thread::sleep;
use std::time::Duration;

use crate::bus;

/// The level the desktop gives an output that has no stored brightness.
const DEFAULT_LEVEL: f64 = 1.0;

/// Brightness per output name, as the desktop reports it.
pub(crate) type Levels = BTreeMap<String, f64>;

/// The rewrite that drags the panel up lands within a moment of the connector
/// changing, so the first rounds watch closely enough to cut it off before the
/// user sees a flash. The rest guard the level for as long as the desktop is
/// still settling the new output.
const QUICK_ROUNDS: usize = 40;
const QUICK_INTERVAL: Duration = Duration::from_millis(50);
const STEADY_ROUNDS: usize = 50;
const STEADY_INTERVAL: Duration = Duration::from_millis(200);

/// Reads the current per-output brightness, if the desktop exposes it.
#[must_use]
pub(crate) fn levels() -> Option<Levels> {
    let reply = bus::call("GetBrightness", &[])?;
    parse(&reply)
}

/// Watches the levels `saved` describes while the desktop reacts to an output
/// change, without holding up the caller.
///
/// Start this before the output appears: the panel is brightest in the moment
/// between the desktop's rewrite and the first correction.
pub(crate) fn settle(saved: &Levels) {
    let saved = saved.clone();
    let builtin = builtin_output();
    std::thread::spawn(move || {
        let mut targets = saved;
        for round in 0..(QUICK_ROUNDS + STEADY_ROUNDS) {
            sleep(if round < QUICK_ROUNDS {
                QUICK_INTERVAL
            } else {
                STEADY_INTERVAL
            });
            let Some(current) = levels() else { return };
            // An output that appeared with the virtual display has no stored
            // level of its own; the panel's is the one the user would expect.
            let fresh = match &builtin {
                Some(name) => targets.get(name).copied(),
                None => None,
            };
            for (output, now) in &current {
                if !targets.contains_key(output) {
                    let level = fresh.unwrap_or(*now);
                    if level < DEFAULT_LEVEL {
                        set(output.as_str(), level);
                    }
                    targets.insert(output.clone(), level);
                }
            }
            for (output, level) in drifted(&current, &targets) {
                set(&output, level);
            }
        }
    });
}

/// The eye-care (colour temperature) setting the desktop applies to the whole
/// screen, whichever output is in front.
#[derive(Clone, Copy)]
pub(crate) struct ColorTemperature {
    enabled: bool,
    mode: i32,
}

/// Puts back the desktop's eye-care setting if the virtual display turns it off.
///
/// The service keeps a colour-temperature entry per monitor identity, and every
/// new phone — or every new shape of the same phone — is a new identity. A
/// fresh entry arrives with eye care off, and the rewrite takes the whole
/// desktop with it, exactly as the brightness rewrite does. The switch and the
/// mode both change, so remember them before the output appears and put the
/// mode back when it does.
///
/// A desktop whose eye care is already off has nothing to put back: mode `0`
/// and a disabled switch look the same. The mode is the one the user chose, and
/// the manual temperature is left alone by the rewrite, so restoring the mode
/// is enough.
pub(crate) fn keep_color_temperature() {
    let Some(saved) = color_temperature() else {
        return;
    };
    if !saved.enabled || saved.mode == 0 {
        return;
    }
    std::thread::spawn(move || {
        for round in 0..(QUICK_ROUNDS + STEADY_ROUNDS) {
            sleep(if round < QUICK_ROUNDS {
                QUICK_INTERVAL
            } else {
                STEADY_INTERVAL
            });
            let Some(now) = color_temperature() else {
                return;
            };
            // On with a method means the user's setting survived this round.
            if now.enabled && now.mode != 0 {
                continue;
            }
            let mode = saved.mode.to_string();
            if bus::call("SetMethodAdjustCCT", &["i", mode.as_str()]).is_some() {
                log::debug!("kept the eye-care colour temperature at {mode}");
            }
        }
    });
}

/// Reads the eye-care switch and method, if the desktop reports them.
fn color_temperature() -> Option<ColorTemperature> {
    let enabled = bus::property("ColorTemperatureEnabled")?;
    let mode = bus::property("ColorTemperatureMode")?;
    Some(ColorTemperature {
        enabled: parse_boolean(&enabled)?,
        mode: parse_integer(&mode)?,
    })
}

/// The output name of the built-in panel, if the desktop reports one.
fn builtin_output() -> Option<String> {
    let reply = bus::call("GetBuiltinMonitor", &[])?;
    parse_builtin(&reply)
}

/// Outputs the desktop moved to the default level although they should keep a
/// lower one.
///
/// Only a jump to exactly the default counts, so a level the user set by hand
/// in the meantime is left alone.
fn drifted(current: &Levels, targets: &Levels) -> Vec<(String, f64)> {
    targets
        .iter()
        .filter_map(|(output, want)| {
            if *want >= DEFAULT_LEVEL {
                return None;
            }
            let now = current.get(output)?;
            ((now - DEFAULT_LEVEL).abs() < f64::EPSILON).then(|| (output.clone(), *want))
        })
        .collect()
}

fn set(output: &str, value: f64) {
    let value = format!("{value}");
    if bus::call("SetBrightness", &["sd", output, value.as_str()]).is_some() {
        log::debug!("kept {output} brightness at {value}");
    }
}

/// Parses the `a{sd}` reply `busctl` prints for `GetBrightness`.
fn parse(reply: &str) -> Option<Levels> {
    let mut words = reply.split_whitespace();
    if words.next() != Some("a{sd}") {
        return None;
    }
    let entries: usize = words.next()?.parse().ok()?;
    let mut levels = Levels::new();
    for _ in 0..entries {
        let output = words.next()?.trim_matches('"');
        let value: f64 = words.next()?.parse().ok()?;
        levels.insert(output.to_owned(), value);
    }
    Some(levels)
}

/// Parses the `(so)` reply of `GetBuiltinMonitor`: the first quoted name.
fn parse_builtin(reply: &str) -> Option<String> {
    let start = reply.find('"')?;
    let rest = &reply[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Parses the `b` reply `busctl` prints for a boolean property.
fn parse_boolean(reply: &str) -> Option<bool> {
    let mut words = reply.split_whitespace();
    if words.next() != Some("b") {
        return None;
    }
    match words.next()? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parses the `i` reply `busctl` prints for an integer property.
fn parse_integer(reply: &str) -> Option<i32> {
    let mut words = reply.split_whitespace();
    if words.next() != Some("i") {
        return None;
    }
    words.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{drifted, parse, parse_boolean, parse_builtin, parse_integer, Levels};

    #[test]
    fn reads_every_output_of_the_reply() {
        let levels = parse("a{sd} 2 \"eDP-1\" 0.1 \"DVI-I-1-1\" 1\n").expect("reply parses");
        assert_eq!(levels.get("eDP-1"), Some(&0.1));
        assert_eq!(levels.get("DVI-I-1-1"), Some(&1.0));
        assert_eq!(levels.len(), 2);
    }

    #[test]
    fn accepts_an_empty_reply() {
        assert!(parse("a{sd} 0\n").is_some_and(|levels| levels.is_empty()));
    }

    #[test]
    fn ignores_anything_that_is_not_a_brightness_map() {
        assert!(parse("").is_none());
        assert!(parse("a{sb} 1 \"eDP-1\" true\n").is_none());
        assert!(parse("a{sd} 2 \"eDP-1\" 0.1\n").is_none(), "short reply");
    }

    #[test]
    fn reads_the_builtin_output_of_the_reply() {
        let reply = "so \"eDP-1\" \"/org/deepin/dde/Display1/Monitor_67\"\n";
        assert_eq!(parse_builtin(reply).as_deref(), Some("eDP-1"));
        assert_eq!(parse_builtin("a{sd} 0\n"), None);
    }

    #[test]
    fn reads_the_eye_care_properties_of_the_display_service() {
        assert_eq!(parse_boolean("b true\n"), Some(true));
        assert_eq!(parse_boolean("b false\n"), Some(false));
        assert_eq!(parse_boolean("b maybe\n"), None);
        assert_eq!(parse_boolean("i 2\n"), None, "not a boolean reply");
        assert_eq!(parse_integer("i 2\n"), Some(2));
        assert_eq!(parse_integer("i 0\n"), Some(0));
        assert_eq!(parse_integer("i not-a-number\n"), None);
        assert_eq!(parse_integer("b true\n"), None, "not an integer reply");
    }

    #[test]
    fn only_a_jump_to_the_default_is_a_drift() {
        let mut targets = Levels::new();
        targets.insert("eDP-1".to_owned(), 0.1);
        targets.insert("DVI-I-1-1".to_owned(), 0.1);

        let mut current = Levels::new();
        current.insert("eDP-1".to_owned(), 1.0);
        current.insert("DVI-I-1-1".to_owned(), 0.4);

        assert_eq!(
            drifted(&current, &targets),
            [("eDP-1".to_owned(), 0.1)],
            "only the panel jumped to the default",
        );
    }

    #[test]
    fn leaves_a_full_level_of_a_monitor_that_lives_at_full() {
        let mut targets = Levels::new();
        targets.insert("HDMI-1".to_owned(), 1.0);
        let mut current = Levels::new();
        current.insert("HDMI-1".to_owned(), 1.0);
        assert_eq!(
            drifted(&current, &targets),
            Vec::new(),
            "a monitor that lives at full is not drifting",
        );
    }
}
