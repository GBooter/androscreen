//! One call into the deepin display service, as the signed-in user.
//!
//! The desktop owns brightness and touch-screen mapping, so both go through its
//! session-bus service instead of touching root-only sysfs nodes or the X
//! device configuration. `busctl` is a normal user command, and a desktop
//! without the service simply gets no reply.

use std::process::{Command, Stdio};
use std::time::Duration;

const SERVICE: &str = "org.deepin.dde.Display1";
const PATH: &str = "/org/deepin/dde/Display1";
const BUSCTL: &str = "busctl";

/// Calls `method` and returns what `busctl` printed, if the service answered.
pub(crate) fn call(method: &str, signature_and_args: &[&str]) -> Option<String> {
    call_service(SERVICE, PATH, method, signature_and_args)
}

/// Calls a session-bus service other than the display service.
pub(crate) fn call_service(
    service: &str,
    path: &str,
    method: &str,
    signature_and_args: &[&str],
) -> Option<String> {
    run(
        &["call", service, path, service, method],
        signature_and_args,
    )
}

/// Reads the `name` property of the display service.
#[must_use]
pub(crate) fn property(name: &str) -> Option<String> {
    run(&["get-property", SERVICE, PATH, SERVICE, name], &[])
}

fn run(command: &[&str], arguments: &[&str]) -> Option<String> {
    let mut busctl = Command::new(BUSCTL);
    busctl.arg("--user");
    for part in command.iter().chain(arguments) {
        busctl.arg(part);
    }
    let output = busctl
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Calls a method of one object of a service, giving up after `limit`.
///
/// The display service re-lays every monitor out while a layout ask is in
/// flight, and a caller that waits for it holds up the phone's picture for as
/// long as the desktop likes. The ask still reaches the service; only the wait
/// for its answer is bounded.
pub(crate) fn call_limited(
    service: &str,
    path: &str,
    interface: &str,
    method: &str,
    signature_and_args: &[&str],
    limit: Duration,
) -> Option<String> {
    let mut command = vec!["call", service, path, interface, method];
    command.extend_from_slice(signature_and_args);
    run_limited(&command, limit)
}

/// Reads a property of one object of a service, giving up after `limit`.
pub(crate) fn property_limited(
    service: &str,
    path: &str,
    interface: &str,
    name: &str,
    limit: Duration,
) -> Option<String> {
    run_limited(&["get-property", service, path, interface, name], limit)
}

/// Every object path `service` publishes under `prefix`.
pub(crate) fn children(service: &str, prefix: &str, limit: Duration) -> Vec<String> {
    let Some(listing) = run_limited(&["tree", service], limit) else {
        return Vec::new();
    };
    paths(&listing, prefix)
}

/// The object paths drawn into `busctl --user tree` output that start with
/// `prefix`, with the line art around them removed.
fn paths(listing: &str, prefix: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let tail = line.rsplit(['│', ' ', '├', '└', '─']).next()?;
            tail.starts_with(prefix).then(|| tail.to_owned())
        })
        .collect()
}

fn run_limited(command: &[&str], limit: Duration) -> Option<String> {
    let millis = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX).max(1);
    let mut busctl = Command::new(BUSCTL);
    busctl.args(["--user", &format!("--timeout={millis}")]);
    busctl.args(command);
    let output = busctl
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::paths;

    const TREE: &str = "\
-                       /org/deepin/dde/XEventMonitor1
  │ │ ├─ /org/deepin/dde/Display1/Monitor_124
  │ │ ├─ /org/deepin/dde/Display1/Monitor_67
  │ │ └─ /org/deepin/dde/Display1/Theme
";

    #[test]
    fn reads_the_object_paths_out_of_the_line_art() {
        assert_eq!(
            paths(TREE, "/org/deepin/dde/Display1/Monitor_"),
            vec![
                "/org/deepin/dde/Display1/Monitor_124",
                "/org/deepin/dde/Display1/Monitor_67",
            ]
        );
        assert_eq!(
            paths(TREE, "/org/deepin/dde/Display1/Nothing"),
            [] as [String; 0]
        );
        assert_eq!(
            paths(TREE, "/org/deepin/dde/Display1/T"),
            vec!["/org/deepin/dde/Display1/Theme"],
            "the service keeps other objects under the same branch"
        );
    }
}
