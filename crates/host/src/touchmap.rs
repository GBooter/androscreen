//! Maps the injected touchscreen onto the virtual monitor.
//!
//! The desktop has to be told: an absolute touch device whose monitor is
//! unknown gets spread over the whole X screen, so a finger on the phone drags
//! the pointer across the laptop's own windows and holds the button down there.
//! deepin remembers the mapping by the identity the device reports, which is
//! the hash of the physical path `uinput` sets, and applies it every time the
//! device appears.
//!
//! A desktop without the display service keeps the device unmapped, which is
//! what it would do anyway.

use std::thread::sleep;
use std::time::Duration;

use crate::bus;
use crate::uinput;

/// The desktop notices a new input device through a udev event, which can trail
/// the device by a moment.
const LOOKUP_ROUNDS: usize = 12;
const LOOKUP_INTERVAL: Duration = Duration::from_millis(250);

/// Associates the injected touchscreen with the virtual monitor.
///
/// `connector` is the name the kernel gives the DRM connector; the desktop is
/// told the name it prints itself. Both the device and the monitor reach the
/// desktop through events that trail the session, so the lookup runs beside it
/// and retries rather than holding up the first frame.
pub(crate) fn associate(connector: &str) {
    let connector = connector.to_owned();
    std::thread::spawn(move || {
        for round in 0..LOOKUP_ROUNDS {
            let uuid = bus::property("Touchscreens")
                .as_deref()
                .and_then(device_uuid);
            let listed = bus::call("ListOutputNames", &[])
                .as_deref()
                .and_then(output_names);
            let mapped = match (uuid, listed) {
                (Some(uuid), Some(names)) => {
                    output_named(&connector, &names).map(|output| (uuid, output))
                }
                _ => None,
            };
            if let Some((uuid, output)) = mapped {
                if bus::call("AssociateTouchByUUID", &["ss", &output, &uuid]).is_some() {
                    log::debug!("mapped {output} to touch device {uuid}");
                    return;
                }
            }
            if round + 1 < LOOKUP_ROUNDS {
                sleep(LOOKUP_INTERVAL);
            }
        }
        log::debug!("the desktop would not map the injected touchscreen");
    });
}

/// The monitors the desktop knows, from the `as` reply of `ListOutputNames`.
fn output_names(reply: &str) -> Option<Vec<String>> {
    let mut words = reply.split_whitespace();
    if words.next() != Some("as") {
        return None;
    }
    let entries: usize = words.next()?.parse().ok()?;
    let mut names = Vec::with_capacity(entries);
    for _ in 0..entries {
        names.push(words.next()?.trim_matches('"').to_owned());
    }
    Some(names)
}

/// The name the desktop prints for the DRM `connector`.
///
/// X appends an index to a connector name, so the virtual monitor the kernel
/// calls `DVI-I-1` is listed as `DVI-I-1-1`.
fn output_named(connector: &str, names: &[String]) -> Option<String> {
    if let Some(exact) = names.iter().find(|name| name.as_str() == connector) {
        return Some(exact.clone());
    }
    let mut matches = names.iter().filter(|name| name.starts_with(connector));
    let only = matches.next()?;
    matches.next().is_none().then(|| only.clone())
}

/// Reads the identity of our device from the `a(issss)` reply of `Touchscreens`.
///
/// Each entry is an id and then five strings: name, node, serial, uuid.
fn device_uuid(reply: &str) -> Option<String> {
    let quoted = format!("\"{}\"", uinput::DEVICE_NAME);
    let (_, tail) = reply.split_once(&quoted)?;
    let mut fields = strings(tail);
    fields.next()?; // /dev/input/eventN
    fields.next()?; // serial
    fields.next()
}

/// Yields the double-quoted fields of `text`, in order.
fn strings(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest = text;
    std::iter::from_fn(move || {
        let start = rest.find('"')?;
        rest = &rest[start + 1..];
        let end = rest.find('"')?;
        let value = rest[..end].to_owned();
        rest = &rest[end + 1..];
        Some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::{device_uuid, output_named, output_names};

    #[test]
    fn prints_the_connector_under_the_name_the_desktop_uses() {
        let names = output_names("as 2 \"eDP-1\" \"DVI-I-1-1\"\n").expect("reply parses");
        assert_eq!(
            output_named("DVI-I-1", &names).as_deref(),
            Some("DVI-I-1-1")
        );
        assert_eq!(output_named("eDP-1", &names).as_deref(), Some("eDP-1"));
        assert_eq!(output_named("DP-2", &names), None);
    }

    #[test]
    fn refuses_to_guess_between_two_connectors() {
        let names = vec!["DVI-I-1-1".to_owned(), "DVI-I-1-2".to_owned()];
        assert_eq!(output_named("DVI-I-1", &names), None);
    }

    #[test]
    fn reads_the_output_list() {
        assert!(output_names("as 0\n").is_some_and(|names| names.is_empty()));
        assert!(output_names("a{sd} 1 \"eDP-1\" 1\n").is_none());
        assert!(output_names("as 2 \"eDP-1\"\n").is_none(), "short reply");
    }

    const REPLY: &str =
        "a(issss) 1 17 \"AndroScreen Touch\" \"/dev/input/event18\" \"\" \"d41d8cd9\"\n";

    #[test]
    fn reads_the_uuid_of_our_device() {
        assert_eq!(device_uuid(REPLY).as_deref(), Some("d41d8cd9"));
    }

    #[test]
    fn skips_a_device_that_is_not_ours() {
        let reply = "a(issss) 2 5 \"elan-ts\" \"/dev/input/event4\" \"abc\" \"1\"\
                     \n17 \"AndroScreen Touch\" \"/dev/input/event18\" \"\" \"mine\"\n";
        assert_eq!(device_uuid(reply).as_deref(), Some("mine"));
    }

    #[test]
    fn needs_a_complete_entry() {
        assert_eq!(
            device_uuid("a(issss) 1 17 \"AndroScreen Touch\" \"/dev/input/event18\" \"\"\n"),
            None
        );
        assert_eq!(device_uuid("a(issss) 0\n"), None);
    }
}
