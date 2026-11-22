//! Secure Boot, evdi, and the one action the window can offer.
//!
//! The kernel refuses any module whose signature it does not trust. evdi is
//! built and signed by DKMS on this machine, and its public key is trusted only
//! once the firmware has been told about it. Telling the firmware writes a
//! request that is answered at the next boot, on a blue screen that appears
//! before the desktop — never something to do behind the user's back. The
//! window says what will happen, and the switch is the consent.
//!
//! The blue screen asks for a password. `--root-pw` would use the root
//! account's password, but a locked root account (the default on many
//! distributions, where `sudo` does the work) has no hash to hand over and
//! `mokutil` fails with "Failed to get root password hash". So the password is
//! generated here and kept for this user until the next boot, because it has to
//! be typed on a screen that cannot read this file.

use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::i18n::{self, Lang, Words};

/// Where DKMS keeps the public half of the key that signs its modules.
const DKMS_KEY: &str = "/var/lib/dkms/mok.pub";

/// The EFI variable shim reads at boot for keys waiting to be enrolled.
///
/// Root-only to read, but its presence is listed to everyone, and that is
/// enough to know whether a request is still waiting.
const MOK_NEW: &str = "/sys/firmware/efi/efivars/MokNew-605dab50-e046-4300-abb6-3dd810dd8b23";

/// The generated MOK password, kept beside the daemon's settings.
const MOK_PASSWORD: &str = "mok-password";

/// Left by the package when it deliberately did not load evdi.
///
/// The installer refuses to load a display driver into a running X11 session
/// because deepin 25.3 freezes there; it leaves this note instead, and `/run`
/// clears it on the reboot that makes the module live.
const PENDING_REBOOT: &str = "/run/androscreen/pending-reboot";

/// What the firmware and the kernel say about the module this app needs.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact reported by the firmware, the kernel, or Xorg"
)]
pub(crate) struct Status {
    /// Secure Boot is on, so unsigned and untrusted modules are refused.
    pub(crate) secure_boot: bool,
    /// evdi is loaded, which means nothing is being refused right now.
    pub(crate) loaded: bool,
    /// A virtual display card exists, which is what the daemon opens.
    pub(crate) has_card: bool,
    /// evdi is built for the running kernel, so an enrollment could help.
    pub(crate) built: bool,
    /// The public key that would have to be enrolled, when there is one.
    pub(crate) key: Option<PathBuf>,
    /// An enrollment is already waiting for the next boot.
    pub(crate) pending: bool,
    /// The password that request expects, when it was generated here.
    pub(crate) password: Option<String>,
    /// The installer left the module for the next boot on purpose.
    pub(crate) pending_reboot: bool,
}

impl Status {
    pub(crate) fn read() -> Self {
        let secure_boot = secure_boot_enabled();
        let loaded = Path::new("/sys/module/evdi").exists();
        let has_card = evdi_card_present();
        let built = module_built();
        let pending = enrollment_pending();
        let blocked = secure_boot && !loaded && built;
        // A remembered password outlives its request only when the request was
        // answered — the module loads, or shim drops the variable. Either way
        // the next boot has nothing to ask, so the password is no longer useful.
        if !blocked || !pending {
            forget_password();
        }
        Self {
            secure_boot,
            loaded,
            has_card,
            built,
            key: Path::new(DKMS_KEY)
                .is_file()
                .then(|| PathBuf::from(DKMS_KEY)),
            pending,
            password: if blocked && pending {
                remembered_password()
            } else {
                None
            },
            // A usable module already answered the question, so a stale note
            // from an earlier install is not a reason to ask for a reboot. A
            // module that was never built is not one either: a reboot cannot
            // fix a missing build.
            pending_reboot: built
                && !(loaded && has_card)
                && Path::new(PENDING_REBOOT).exists(),
        }
    }

    /// Whether the daemon has a virtual display card to open.
    pub(crate) fn usable(&self) -> bool {
        self.loaded && self.has_card
    }

    /// Whether the only thing between the user and a working display is a reboot.
    ///
    /// This is the normal state right after a first install: the package did not
    /// load the driver into the running X11 session (the desktop would freeze),
    /// and the boot-time `modules-load.d` entry will do it next time.
    pub(crate) fn reboot_required(&self) -> bool {
        self.pending_reboot
    }

    /// Whether the module is missing *because the firmware will not take it*.
    ///
    /// A module that was never built is a different problem with a different
    /// fix, and enrolling its key would not help. Neither does a machine where
    /// evdi already loads, because nothing is being refused. The switch is
    /// therefore offered only where it can actually change the answer.
    pub(crate) fn blocked(&self) -> bool {
        self.secure_boot && !self.loaded && self.built
    }
}

/// Asks the tool rather than reading efivars, because it also understands
/// setup mode and the machines where the variable is not where it used to be.
fn secure_boot_enabled() -> bool {
    Command::new("mokutil")
        .arg("--sb-state")
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("SecureBoot enabled"))
}

/// Whether an evdi module exists for the kernel that is running now.
///
/// `modinfo` searches the kernel's own module directory, so a module left over
/// from another kernel does not count: enrolling a key for it would change
/// nothing at the next boot.
fn module_built() -> bool {
    Command::new("modinfo")
        .arg("evdi")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Whether a key is still waiting for the next boot's prompt.
fn enrollment_pending() -> bool {
    Path::new(MOK_NEW).exists()
}

/// Whether evdi created a card, which is what `initial_device_count` controls.
///
/// evdi's only connector is DVI-I, so a `card*-DVI-I-*` entry under
/// `/sys/class/drm` is a card the daemon can open; a module loaded with a count
/// of zero leaves the daemon with nothing to use.
fn evdi_card_present() -> bool {
    std::fs::read_dir("/sys/class/drm").is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("card") && name.contains("-DVI-I-")
        })
    })
}

/// What the switch asks the firmware to do at the next boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Record the request to trust the DKMS key.
    Allow,
    /// Withdraw a request that has not been answered yet.
    Withdraw,
}

/// What a successful switch action left behind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The request is recorded; the returned password is typed at the next boot.
    Enrolled {
        /// What to type on the blue screen.
        password: String,
    },
    /// The request was withdrawn and the next boot is quiet again.
    Withdrawn,
}

/// Records, or withdraws, the request to trust the DKMS key.
///
/// The administrator is asked through the desktop's own authorization dialog,
/// which is the moment the user agreed to in the window. The MOK password is
/// handed to the tool on standard input because the dialog has no terminal to
/// prompt in; the same password is typed on the blue screen later, so it is
/// remembered until then. Everything that can go wrong is reported in `lang`,
/// because it is read in the window like any other sentence.
///
/// # Errors
///
/// Returns the sentence to show in the window when there is no key to enroll or
/// when the authorization or the tool fails.
pub(crate) fn enroll(action: Action, lang: Lang) -> Result<Outcome, String> {
    let key = Path::new(DKMS_KEY);
    let password = match action {
        Action::Allow => {
            if !key.is_file() {
                return Err(match lang {
                    Lang::Zh => {
                        "找不到 DKMS 签名密钥（/var/lib/dkms/mok.pub），请先重新编译 evdi 模块。"
                    }
                    Lang::En => {
                        "No DKMS signing key at /var/lib/dkms/mok.pub; rebuild the evdi module first."
                    }
                }
                .to_owned());
            }
            Some(new_password())
        }
        Action::Withdraw => None,
    };

    let mut command = Command::new("pkexec");
    command.arg("mokutil");
    match action {
        Action::Allow => {
            command.arg("--import").arg(key);
        }
        Action::Withdraw => {
            command.arg("--revoke-import");
        }
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if password.is_some() {
        command.stdin(Stdio::piped());
    }

    let mut child = command
        .spawn()
        .map_err(|_| i18n::manual_instruction(lang, None))?;
    if let Some(password) = &password {
        // mokutil reads the password twice from standard input, which is what
        // lets this work without a terminal to prompt in.
        let piped = child
            .stdin
            .take()
            .is_some_and(|mut stdin| write!(stdin, "{password}\n{password}\n").is_ok());
        if !piped {
            let _ = child.wait();
            return Err(Words::of(lang).password_lost.to_owned());
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|_| i18n::manual_instruction(lang, None))?;

    if output.status.success() {
        if let Some(password) = password {
            remember_password(&password);
            return Ok(Outcome::Enrolled { password });
        }
        forget_password();
        return Ok(Outcome::Withdrawn);
    }

    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    // pkexec exits 126/127 when the dialog was dismissed or refused; it has
    // nothing to say on stderr, and the user should hear that nothing changed.
    if matches!(output.status.code(), Some(126 | 127)) {
        return Err(Words::of(lang).enrollment_cancelled.to_owned());
    }
    Err(i18n::manual_instruction(
        lang,
        (!detail.is_empty()).then_some(detail.as_str()),
    ))
}

/// Eight digits from the system's random state: short enough to type on the
/// blue screen, long enough not to be guessed off the top of one's head.
fn new_password() -> String {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u8(0);
    format!("{:08}", hasher.finish() % 100_000_000)
}

/// Where the generated password waits for the next boot.
fn password_path() -> Option<PathBuf> {
    androscreen_core::control::config_dir().map(|dir| dir.join(MOK_PASSWORD))
}

/// Keeps the password for this user only, until the window or the boot uses it.
fn remember_password(password: &str) {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = password_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&path, password).is_ok() {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Drops a password that no longer has a request to answer.
fn forget_password() {
    if let Some(path) = password_path() {
        let _ = std::fs::remove_file(path);
    }
}

/// The password written by the last successful enrollment, if it is still there.
fn remembered_password() -> Option<String> {
    let text = std::fs::read_to_string(password_path()?).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
