//! The small surface the desktop settings window shares with the daemon.
//!
//! Two files in the runtime directory carry everything: the daemon publishes
//! the state it is in, and the window drops a one-word request beside it. A
//! socket would be more direct, but the daemon is a user service and the window
//! is started from the dock, and the traffic is a few words per minute.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// What the daemon is doing, as the settings window shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Daemon version.
    pub version: String,
    /// Whether a phone link is open.
    pub phone: bool,
    /// Whether a virtual display is being streamed.
    pub streaming: bool,
    /// Virtual display geometry while streaming.
    pub screen: String,
    /// Why the phone is not being used, when that is worth saying.
    pub note: String,
    /// Latest CPU utilisation, when the daemon has a sample.
    #[serde(default)]
    pub cpu_percent: Option<f32>,
    /// Latest memory utilisation, when the daemon has a sample.
    #[serde(default)]
    pub mem_percent: Option<f32>,
    /// Temperature sensors, hottest of each part.
    #[serde(default)]
    pub temps: Vec<SensorRead>,
    /// Video frames the phone took per second over the last second.
    #[serde(default)]
    pub fps: Option<f32>,
    /// Video bits that reached the phone per second, in millions.
    #[serde(default)]
    pub mbps: Option<f32>,
    /// Media frames skipped over the last second.
    #[serde(default)]
    pub dropped: Option<u32>,
    /// Times the phone was seen falling behind over the last second.
    #[serde(default)]
    pub behind: Option<u32>,
}

/// One temperature reading, already scaled for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensorRead {
    /// Short part name such as `CPU`.
    pub label: String,
    /// Degrees Celsius.
    pub celsius: f32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            phone: false,
            streaming: false,
            screen: String::new(),
            note: String::new(),
            cpu_percent: None,
            mem_percent: None,
            temps: Vec::new(),
            fps: None,
            mbps: None,
            dropped: None,
            behind: None,
        }
    }
}

impl State {
    /// Hottest sensor, if the machine reports one.
    #[must_use]
    pub fn hottest(&self) -> Option<&SensorRead> {
        self.temps
            .iter()
            .max_by(|a, b| a.celsius.total_cmp(&b.celsius))
    }
}

/// Settings the window owns and the daemon obeys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Whether a plugged-in phone is taken over without being asked.
    pub auto_takeover: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_takeover: true,
        }
    }
}

impl Config {
    /// Reads the settings file, falling back to the defaults.
    #[must_use]
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&text)
    }

    /// Reads the settings out of the file's text.
    #[must_use]
    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("auto_takeover") {
                let value = value.trim_start_matches([' ', '=', ':', '\t']);
                config.auto_takeover = !matches!(value.trim(), "false" | "0" | "no" | "off");
            }
        }
        config
    }

    /// Writes the settings file, creating the directory it lives in.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the directory or the file cannot be written.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = config_path() else {
            return Err(std::io::Error::other("no home directory"));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            format!(
                "# AndroScreen settings, written by the settings window.\n\
                 auto_takeover={}\n",
                self.auto_takeover
            ),
        )
    }
}

/// A word the settings window leaves for the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Hand the USB port back so the phone can be used as storage.
    Release,
    /// Take the next phone over even when automatic takeover is off.
    Enable,
}

/// Publishes the daemon state for the settings window.
pub fn publish(state: &State) {
    let path = runtime_dir().join("state.json");
    let Ok(text) = serde_json::to_vec(state) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // The window may read while this is written; a rename keeps it whole.
    let temporary = path.with_extension("json.tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

/// Reads the state the daemon last published.
#[must_use]
pub fn read_state() -> Option<State> {
    let text = std::fs::read(runtime_dir().join("state.json")).ok()?;
    let state: State = serde_json::from_slice(&text).ok()?;
    Some(state)
}

/// How long ago the daemon last published.
#[must_use]
pub fn state_age() -> Option<Duration> {
    let modified = std::fs::metadata(runtime_dir().join("state.json"))
        .ok()?
        .modified()
        .ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Whether the daemon has published anything recently.
#[must_use]
pub fn is_running() -> bool {
    state_age().is_some_and(|age| age < Duration::from_secs(5))
}

/// Leaves a request for the daemon.
///
/// # Errors
///
/// Returns the I/O error when the request file cannot be written.
pub fn request(request: Request) -> std::io::Result<()> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let word = match request {
        Request::Release => "release",
        Request::Enable => "enable",
    };
    std::fs::write(dir.join("command"), word)
}

/// Takes the pending request, if any, and clears it.
#[must_use]
pub fn take_request() -> Option<Request> {
    let path = runtime_dir().join("command");
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    match text.trim() {
        "release" => Some(Request::Release),
        "enable" => Some(Request::Enable),
        // Anything else is a half-written or foreign file; ignoring it is
        // better than acting on a word we do not know.
        _ => None,
    }
}

/// Where the state and request files live.
fn runtime_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("androscreen");
    }
    let uid = std::env::var("UID").unwrap_or_else(|_| "0".to_owned());
    PathBuf::from("/tmp").join(format!("androscreen-{uid}"))
}

/// Where the settings file lives.
fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("settings.conf"))
}

/// The directory that holds the daemon's settings and the window's own state.
///
/// # Returns
///
/// `None` when the environment names neither `XDG_CONFIG_HOME` nor `HOME`.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("androscreen"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/androscreen"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_taking_the_phone_over() {
        assert!(Config::parse("").auto_takeover);
        assert!(Config::parse("# nothing here\n").auto_takeover);
    }

    #[test]
    fn a_settings_file_can_turn_the_takeover_off() {
        for text in [
            "auto_takeover=false",
            "auto_takeover = false\n",
            "  auto_takeover: off  ",
            "auto_takeover=0",
            "auto_takeover=no",
        ] {
            assert!(!Config::parse(text).auto_takeover, "for {text:?}");
        }
        for text in [
            "auto_takeover=true",
            "auto_takeover = yes",
            "auto_takeover=1",
        ] {
            assert!(Config::parse(text).auto_takeover, "for {text:?}");
        }
    }

    #[test]
    fn a_saved_setting_reads_back() {
        let text = format!("# written by the window\nauto_takeover={}\n", false);
        assert!(!Config::parse(&text).auto_takeover);
    }

    #[test]
    fn state_survives_a_round_trip() {
        let state = State {
            version: "0.4.0".to_owned(),
            phone: true,
            streaming: true,
            screen: "1080x2460".to_owned(),
            note: "USB 已释放".to_owned(),
            cpu_percent: Some(12.5),
            mem_percent: Some(53.0),
            temps: vec![
                SensorRead {
                    label: "CPU".to_owned(),
                    celsius: 61.5,
                },
                SensorRead {
                    label: "SSD".to_owned(),
                    celsius: 38.0,
                },
            ],
            fps: Some(44.5),
            mbps: Some(12.0),
            dropped: Some(3),
            behind: Some(1),
        };
        let text = serde_json::to_string(&state).expect("serializes");
        let back: State = serde_json::from_str(&text).expect("parses");
        assert_eq!(back.screen, "1080x2460");
        assert!(back.streaming);
        assert_eq!(back.cpu_percent, Some(12.5));
        assert_eq!(back.hottest().map(|temp| temp.label.as_str()), Some("CPU"));
    }

    #[test]
    fn state_from_an_older_daemon_still_parses() {
        // 0.3.5 wrote neither the metrics nor the sensors; a newer window must
        // still be able to read what it left behind.
        let text = r#"{"version":"0.3.5","phone":false,"streaming":false,
"screen":"","note":""}"#;
        let back: State = serde_json::from_str(text).expect("parses");
        assert_eq!(back.cpu_percent, None);
        assert_eq!(back.temps, Vec::<SensorRead>::new(), "no sensors");
        assert!(back.hottest().is_none());
    }
}
