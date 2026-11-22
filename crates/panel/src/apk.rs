//! Hands the user the phone app that shipped inside this package.
//!
//! The deb carries the APK so that a machine with no network can still install
//! it, but "it is somewhere under /usr/share" is not an answer a user can act
//! on: they need a file they can send to the phone over a chat app, a cable, or
//! a QR code. This module finds that copy and puts it where downloads live.
//!
//! Everything it says to the user goes through [`crate::i18n`], including the
//! title and the buttons of the save dialog it borrows from the desktop.

use std::path::{Path, PathBuf};

use crate::i18n::{self, Lang, Words};

/// Where the package manager puts the phone app.
const PACKAGED: &[&str] = &[
    "/usr/share/androscreen/androscreen.apk",
    "/usr/local/share/androscreen/androscreen.apk",
];

/// Where a checkout keeps it, so the window is testable before packaging.
const IN_TREE: &[&str] = &[
    "android/app/build/outputs/apk/release/app-release.apk",
    "android/app/build/outputs/apk/debug/app-debug.apk",
];

/// The phone app that belongs to this release, if it can be found.
#[must_use]
pub(crate) fn source() -> Option<PathBuf> {
    source_from(&std::env::current_exe().ok()?)
}

/// Finds the phone app that belongs to the install `exe` lives in.
fn source_from(exe: &Path) -> Option<PathBuf> {
    for path in PACKAGED {
        let path = Path::new(path);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }

    let beside = exe.parent()?.join("androscreen.apk");
    if beside.is_file() {
        return Some(beside);
    }

    // `<repo>/target/<profile>/androscreen-settings` -> `<repo>`.
    let root = exe.parent()?.parent()?.parent()?;
    IN_TREE
        .iter()
        .map(|relative| root.join(relative))
        .find(|path| path.is_file())
}

/// The name the saved file gets: the release it belongs to is part of it, so
/// two downloads from different versions do not silently overwrite.
#[must_use]
pub(crate) fn file_name() -> String {
    format!("androscreen-{}.apk", env!("CARGO_PKG_VERSION"))
}

/// The user's download directory, falling back to the home directory.
#[must_use]
pub(crate) fn download_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    if let Some(dir) = configured_download_dir(&home) {
        return dir;
    }
    for name in ["下载", "Downloads"] {
        let candidate = home.join(name);
        if candidate.is_dir() {
            return candidate;
        }
    }
    home
}

/// Reads `XDG_DOWNLOAD_DIR` out of `user-dirs.dirs`.
///
/// The file is shell syntax, but the only line that matters is a single
/// assignment, which is parsed rather than sourced.
fn configured_download_dir(home: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).ok()?;
    parse_download_dir(&text, home)
}

/// Pulls the download directory out of the text of `user-dirs.dirs`.
fn parse_download_dir(text: &str, home: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        if value.is_empty() {
            continue;
        }
        // `$HOME` is the only variable the file is allowed to use here.
        let expanded = value
            .strip_prefix("$HOME/")
            .map_or_else(|| PathBuf::from(value), |tail| home.join(tail));
        if expanded.is_dir() {
            return Some(expanded);
        }
    }
    None
}

/// Copies the packaged phone app into the download directory.
///
/// # Errors
///
/// Returns a sentence to show the user when the package is missing from this
/// build, or when the copy cannot be written.
pub(crate) fn save(lang: Lang) -> Result<PathBuf, String> {
    let source = source().ok_or_else(|| missing(lang))?;
    copy_into(lang, &source, &download_dir())
}

fn missing(lang: Lang) -> String {
    Words::of(lang).apk_missing.to_owned()
}

/// Copies `source` into `dir`, never overwriting a file already there.
fn copy_into(lang: Lang, source: &Path, dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| i18n::apk_dir_failed(lang, dir, &error.to_string()))?;
    let target = free_name(dir, &file_name());
    std::fs::copy(source, &target)
        .map_err(|error| i18n::apk_save_failed(lang, &error.to_string()))?;
    Ok(target)
}

/// What came of asking the user where to put the file.
pub(crate) enum SaveOutcome {
    /// Written to the path the user picked.
    Saved(PathBuf),
    /// The user closed the dialog.
    Cancelled,
    /// This desktop has no save dialog; the caller should just pick a folder.
    NoDialog,
}

/// Asks where to put the file, the way every other application does.
///
/// Slint has no file picker of its own, and pulling in a GTK or portal client
/// for one dialog would weigh more than the whole renderer, so this shells out
/// to whichever save dialog the desktop already ships.
///
/// # Errors
///
/// Returns a sentence to show the user when the packaged APK is missing or the
/// copy to the chosen place fails.
pub(crate) fn ask(lang: Lang) -> Result<SaveOutcome, String> {
    let source = source().ok_or_else(|| missing(lang))?;
    let suggested = download_dir().join(file_name());
    let Some(command) = dialog_command(lang, &suggested) else {
        return Ok(SaveOutcome::NoDialog);
    };
    let output = std::process::Command::new(&command.program)
        .args(&command.arguments)
        .output()
        .map_err(|error| i18n::apk_dialog_failed(lang, &error.to_string()))?;
    if !output.status.success() {
        // Cancelling is a failure status with nothing on stdout.
        return Ok(SaveOutcome::Cancelled);
    }
    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if chosen.is_empty() {
        return Ok(SaveOutcome::Cancelled);
    }
    let chosen = PathBuf::from(chosen);
    std::fs::copy(&source, &chosen)
        .map_err(|error| i18n::apk_save_to_failed(lang, &chosen, &error.to_string()))?;
    Ok(SaveOutcome::Saved(chosen))
}

/// A save dialog invocation.
struct Dialog {
    program: String,
    arguments: Vec<String>,
}

/// The save dialog this desktop provides, along with the file name to suggest.
///
/// The dialog is the desktop's own, so its title and its buttons are the ones
/// the user reads: they are taken from the same [`Words`] the window uses, and
/// a dialog that opened in the other language would be the one piece of the
/// page that did not follow the switch.
fn dialog_command(lang: Lang, suggested: &Path) -> Option<Dialog> {
    let file = suggested.to_string_lossy().into_owned();
    let title = Words::of(lang).save_dialog_title;
    let tools: [(&str, Vec<String>); 3] = [
        (
            "zenity",
            ["--file-selection", "--save", "--confirm-overwrite"]
                .iter()
                .map(|part| (*part).to_owned())
                .chain([
                    format!("--title={title}"),
                    "--filename".to_owned(),
                    file.clone(),
                ])
                .collect(),
        ),
        (
            "kdialog",
            ["--getsavefilename", &file, "Android package (*.apk)"]
                .iter()
                .map(|part| (*part).to_owned())
                .collect(),
        ),
        (
            "yad",
            ["--file", "--save"]
                .iter()
                .map(|part| (*part).to_owned())
                .chain([format!("--title={title}"), format!("--filename={file}")])
                .collect(),
        ),
    ];
    if let Some((program, arguments)) = tools.into_iter().find(|(name, _)| in_path(name)) {
        return Some(Dialog {
            program: program.to_owned(),
            arguments,
        });
    }
    // deepin ships neither of the three, but it does ship GTK through the
    // python bindings, and a GTK save dialog is the dialog every other
    // application on this desktop shows.
    if !in_path("python3") {
        return None;
    }
    // GTK wants the folder and the bare name, not one path.
    let folder = suggested.parent()?.to_string_lossy().into_owned();
    let name = suggested.file_name()?.to_string_lossy().into_owned();
    Some(Dialog {
        program: "python3".to_owned(),
        arguments: vec!["-c".to_owned(), gtk_script(lang), folder, name],
    })
}

/// A GTK3 save dialog: folder in `sys.argv[1]`, suggested name in `argv[2]`.
///
/// It is built here rather than kept as a constant so that the dialog's title
/// and its two buttons are the same words the window draws; the script has no
/// braces of its own, so the language can be filled in directly.
fn gtk_script(lang: Lang) -> String {
    let words = Words::of(lang);
    format!(
        r#"
import sys
import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk

dialog = Gtk.FileChooserDialog(
    title="{title}",
    action=Gtk.FileChooserAction.SAVE,
)
dialog.add_buttons("{cancel}", Gtk.ResponseType.CANCEL, "{save}", Gtk.ResponseType.ACCEPT)
dialog.set_do_overwrite_confirmation(True)
dialog.set_current_folder(sys.argv[1])
dialog.set_current_name(sys.argv[2])
if dialog.run() == Gtk.ResponseType.ACCEPT:
    print(dialog.get_filename())
dialog.destroy()
"#,
        title = words.save_dialog_title,
        cancel = words.cancel,
        save = words.save,
    )
}

/// Whether an executable is reachable through `PATH`.
fn in_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// A path in `dir` that does not exist yet, keeping the given name where it can.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = name.strip_suffix(".apk").unwrap_or(name);
    (1..=50)
        .map(|index| dir.join(format!("{stem} ({index}).apk")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp-tests")
            .join(tag);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn second_download_does_not_overwrite_the_first() {
        let dir = scratch("apk-free-name");
        let first = free_name(&dir, "androscreen-0.4.0.apk");
        std::fs::write(&first, b"apk").expect("write");
        let second = free_name(&dir, "androscreen-0.4.0.apk");
        assert_ne!(first, second, "the second download needs its own name");
        assert!(second.to_string_lossy().ends_with("(1).apk"), "{second:?}");
        std::fs::remove_file(&first).expect("cleanup");
    }

    #[test]
    fn download_dir_follows_user_dirs() {
        let home = PathBuf::from("/home/someone");
        let text = "# written by xdg-user-dirs-update\n\
XDG_DESKTOP_DIR=\"$HOME/桌面\"\n\
XDG_DOWNLOAD_DIR=\"$HOME/下载\"\n";
        // `parse_download_dir` only trusts directories that exist, and this
        // machine has no /home/someone; the parse itself is what is checked.
        assert_eq!(parse_download_dir(text, &home), None);
        assert_eq!(parse_download_dir("", &home), None);
    }

    #[test]
    fn a_download_dir_is_always_offered() {
        let dir = download_dir();
        assert!(dir.is_absolute() || dir == Path::new("."), "{dir:?}");
        assert!(
            file_name().len() > ".apk".len() && file_name().contains(".apk"),
            "{}",
            file_name()
        );
    }

    #[test]
    fn the_packaged_apk_is_copied_out_under_its_release_name() {
        let dir = scratch("apk-copy");
        let source = dir.join("fake-source.apk");
        std::fs::write(&source, b"apk bytes").expect("fixture");

        let first = copy_into(Lang::Zh, &source, &dir).expect("copy");
        let second = copy_into(Lang::Zh, &source, &dir).expect("copy again");
        assert_eq!(first.file_name().expect("name"), file_name().as_str());
        assert_ne!(first, second, "the second copy gets its own file");
        assert_eq!(std::fs::read(&second).expect("read"), b"apk bytes");

        std::fs::remove_file(&first).expect("cleanup");
        std::fs::remove_file(&second).expect("cleanup");
        std::fs::remove_file(&source).expect("cleanup");
    }

    #[test]
    fn a_save_dialog_carries_the_suggested_name() {
        // Whichever tool this host answers with, it has to be told what the
        // file should be called, or the user has to type the name back in.
        let suggested = Path::new("/home/someone/下载/androscreen-0.4.0.apk");
        if let Some(dialog) = dialog_command(Lang::Zh, suggested) {
            assert_ne!(dialog.program, "");
            assert!(
                dialog
                    .arguments
                    .iter()
                    .any(|argument| argument.contains("androscreen-0.4.0.apk")),
                "{:?}",
                dialog.arguments
            );
        }
    }

    #[test]
    fn the_save_dialog_speaks_the_window_language() {
        // The dialog belongs to the desktop, not to this window, so nothing
        // translates it for us: it has to be handed the words to draw.
        let suggested = Path::new("/home/someone/Downloads/androscreen-0.4.0.apk");
        for lang in [Lang::Zh, Lang::En] {
            let words = Words::of(lang);
            let script = gtk_script(lang);
            assert!(script.contains(words.save_dialog_title), "{lang:?}");
            assert!(script.contains(words.cancel), "{lang:?}");
            assert!(script.contains(words.save), "{lang:?}");
            if let Some(dialog) = dialog_command(lang, suggested) {
                if dialog.program != "python3" {
                    assert!(
                        dialog
                            .arguments
                            .iter()
                            .any(|argument| argument.ends_with(words.save_dialog_title)),
                        "{lang:?}: {:?}",
                        dialog.arguments
                    );
                }
            }
        }
    }

    #[test]
    fn this_checkout_has_an_apk_to_hand_out() {
        // The daemon's own window is built next to the phone app here, so the
        // download button has something to copy even before packaging.
        let exe =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/androscreen-settings");
        if let Some(found) = source_from(&exe) {
            assert!(found.is_file(), "{found:?}");
        }
    }
}
