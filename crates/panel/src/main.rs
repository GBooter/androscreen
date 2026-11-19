//! Desktop settings window for `AndroScreen`.
//!
//! The window is the only way to turn the service off without a terminal, and
//! the only way to hand the USB port back to the file manager while a phone is
//! plugged in. It talks to the daemon through the files described in
//! [`androscreen_core::control`], so it works whether or not the daemon was
//! started by the session or by hand.

use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Duration;

use androscreen_core::control::{self, Config, Request, State};
use slint::{ComponentHandle, Timer, TimerMode};

mod apk;
mod cable;
mod secure_boot;
mod ui;

use cable::{button_labels, screen_action, transfer_action, Cable};
use ui::SettingsWindow;

/// How often the window picks up what the daemon last published.
const REFRESH: Duration = Duration::from_millis(900);

/// How long a replaced copy of this window is given to close.
///
/// The old process may still hold the single-instance lock, so this has to
/// finish before the lock is claimed.
const GHOST_GRACE: Duration = Duration::from_millis(200);

/// What a background action left for the window to show.
enum Answer {
    /// A sentence for the feedback line.
    Text(String),
    /// The outcome of the key-enrollment switch, which also moves that switch
    /// and replaces the two lines of its card.
    Mok {
        /// Whether the request is now waiting for the next boot.
        requested: bool,
        title: String,
        detail: String,
        text: String,
    },
}

/// A slot a background thread fills and the window picks up on its next tick.
type SharedAnswer = std::sync::Arc<std::sync::Mutex<Option<Answer>>>;

#[expect(
    clippy::print_stdout,
    reason = "a window started twice says so on stdout and exits"
)]
fn main() -> Result<(), slint::PlatformError> {
    // The same button from a terminal, for a machine where the window cannot be
    // opened: `androscreen-settings --download-apk` prints where it went.
    if std::env::args().skip(1).any(|arg| arg == "--download-apk") {
        return save_apk();
    }

    // A copy left over from an older release can be sitting on the desktop with
    // the same title. Stacked with this one it looks like a single window with
    // two title bars, which is exactly how it was reported.
    if !retire_other_copies() {
        println!("AndroScreen 设置窗口已经在运行。");
        return Ok(());
    }
    let _lock = claim_single_window();

    let window = SettingsWindow::new()?;
    window.set_apk_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
    // The window has to say which application it belongs to: the compositor
    // matches that name against the desktop entries, and a window without one
    // is a stranger it decorates differently from the rest of the system.
    // An error here would mean there is no platform left to talk to, which
    // cannot happen before the window exists.
    let _app_id = slint::set_xdg_app_id(slint::SharedString::from("androscreen-settings"));

    // The firmware's verdict does not change while this window is open, so it
    // is read once; only the daemon's numbers are polled. Reading it here also
    // means the window explains the missing display before the user wonders
    // why the phone shows nothing.
    let secure = std::sync::Arc::new(secure_boot::Status::read());
    window.set_evdi_blocked(secure.blocked());
    window.set_mok_requested(secure.pending);
    if secure.key.is_none() {
        window.set_mok_title("找不到 DKMS 签名密钥".into());
        window.set_mok_detail(
            "请先安装内核头文件并重新编译 evdi（sudo dkms autoinstall），或关闭安全启动。".into(),
        );
    } else if secure.pending {
        apply_mok_texts(
            &window,
            secure_boot::Action::Allow,
            secure.password.as_deref(),
        );
    } else {
        apply_mok_texts(&window, secure_boot::Action::Withdraw, None);
    }
    if secure.blocked() {
        // Readable before the reboot, because afterwards the window may be
        // the one thing the user cannot reach.
        window.set_feedback(BOOT_RECOVERY.into());
    }
    refresh(&window, &secure);

    let weak = window.as_weak();
    window.on_auto_toggled(move |value| {
        let mut config = Config::load();
        config.auto_takeover = value;
        let message = if config.save().is_err() {
            "设置没能保存，请检查 ~/.config/androscreen 的权限。"
        } else if value {
            "已开启自动接管：插线后手机自动变成副屏。"
        } else {
            "已关闭自动接管：插线不再抢占手机，可以用文件传输。"
        };
        set_feedback(&weak, message);
    });

    let weak = window.as_weak();
    window.on_release_clicked(move || {
        let (request, done) = transfer_action(cable_now());
        send(&weak, request, done);
    });

    let weak = window.as_weak();
    window.on_enable_clicked(move || {
        let (request, done) = screen_action(cable_now());
        send(&weak, request, done);
    });

    let weak = window.as_weak();
    window.on_restart_clicked(move || {
        let result = std::process::Command::new("systemctl")
            .args(["--user", "restart", "androscreen"])
            .status();
        let message = match result {
            Ok(status) if status.success() => "服务已重启。".to_owned(),
            Ok(status) => format!("服务重启返回 {status}"),
            Err(_) => "找不到 systemctl，无法重启服务。".to_owned(),
        };
        set_feedback(&weak, &message);
    });

    let answer: SharedAnswer = std::sync::Arc::default();
    wire_download(&window, std::sync::Arc::clone(&answer));
    wire_mok(
        &window,
        std::sync::Arc::clone(&answer),
        secure.key.as_deref(),
    );
    let _poll = wire_feedback(&window, std::sync::Arc::clone(&answer));

    let status = std::sync::Arc::clone(&secure);
    let weak = window.as_weak();
    let ticker = Timer::default();
    ticker.start(TimerMode::Repeated, REFRESH, move || {
        if let Some(window) = weak.upgrade() {
            refresh(&window, &status);
        }
    });

    window.show()?;
    window.run()
}

/// Wires the download button.
///
/// The save dialog blocks until the user answers, and the window has to keep
/// drawing behind it, so the work happens on a thread and the feedback line
/// picks the answer up on the next tick.
fn wire_download(window: &SettingsWindow, answer: SharedAnswer) {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let weak = window.as_weak();
    let slot = answer;
    let guard = std::sync::Arc::clone(&busy);
    window.on_download_clicked(move || {
        if guard.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        set_feedback(&weak, "请在弹出的对话框里选择保存位置…");
        let slot = std::sync::Arc::clone(&slot);
        let guard = std::sync::Arc::clone(&guard);
        std::thread::spawn(move || {
            let message = match apk::ask() {
                Ok(apk::SaveOutcome::Saved(path)) => {
                    format!("安装包已保存到 {}", path.display())
                }
                Ok(apk::SaveOutcome::Cancelled) => "已取消保存。".to_owned(),
                // No dialog on this desktop: put it where downloads go and say
                // exactly where, which is the next best thing to asking.
                Ok(apk::SaveOutcome::NoDialog) => match apk::save() {
                    Ok(path) => format!("安装包已保存到 {}", path.display()),
                    Err(reason) => reason,
                },
                Err(reason) => reason,
            };
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(Answer::Text(message));
            }
            guard.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    });
}

/// Wires the switch that asks the firmware to trust the module's key.
///
/// The authorization dialog must not block the window, and the switch only
/// records a request for the *next* boot: saying so before and after is the
/// whole point, because the blue screen that answers it appears before the
/// desktop and cannot be dismissed from here.
fn wire_mok(window: &SettingsWindow, answer: SharedAnswer, key: Option<&std::path::Path>) {
    let missing = key.is_none();
    let weak = window.as_weak();
    window.on_mok_toggled(move |requested| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if missing {
            window.set_mok_requested(false);
            window.set_mok_title("找不到 DKMS 签名密钥".into());
            window.set_mok_detail(
                "请先安装内核头文件并重新编译 evdi（sudo dkms autoinstall），或关闭安全启动。"
                    .into(),
            );
            set_feedback(&weak, "这台机器上没有可用于登记的签名密钥。");
            return;
        }
        if requested {
            window.set_mok_title("正在登记签名密钥…".into());
            window.set_mok_detail(
                "系统会弹出授权对话框，请输入密码。完成后这里会显示重启时要在蓝屏输入的 MOK 密码。"
                    .into(),
            );
        }
        let action = if requested {
            secure_boot::Action::Allow
        } else {
            secure_boot::Action::Withdraw
        };
        let slot = std::sync::Arc::clone(&answer);
        std::thread::spawn(move || {
            let outcome = match secure_boot::enroll(action) {
                Ok(secure_boot::Outcome::Enrolled { password }) => {
                    let (title, detail) = mok_texts(secure_boot::Action::Allow, Some(&password));
                    Answer::Mok {
                        requested: true,
                        title: title.to_owned(),
                        detail,
                        text: format!("已登记。{BOOT_RECOVERY}"),
                    }
                }
                Ok(secure_boot::Outcome::Withdrawn) => {
                    let (title, detail) = mok_texts(secure_boot::Action::Withdraw, None);
                    Answer::Mok {
                        requested: false,
                        title: title.to_owned(),
                        detail,
                        text: "已撤回登记请求，重启后不会再出现 MOK 界面。".to_owned(),
                    }
                }
                // The dialog was dismissed, mokutil refused, or there was no
                // key: say why in the card instead of silently flipping back.
                Err(reason) => Answer::Mok {
                    requested: false,
                    title: "登记没有完成".to_owned(),
                    detail: reason.clone(),
                    text: reason,
                },
            };
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(outcome);
            }
        });
    });
}

/// Applies what a background action reported.
///
/// Shared by the download and the enrollment: both hand their answer over from
/// a thread, and the window may only be touched from this one.
fn wire_feedback(window: &SettingsWindow, answer: SharedAnswer) -> Timer {
    let weak = window.as_weak();
    let poller = Timer::default();
    poller.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let picked = answer.lock().ok().and_then(|mut slot| slot.take());
        let Some(window) = weak.upgrade() else {
            return;
        };
        match picked {
            Some(Answer::Text(text)) => window.set_feedback(text.into()),
            Some(Answer::Mok {
                requested,
                title,
                detail,
                text,
            }) => {
                window.set_mok_requested(requested);
                window.set_mok_title(title.into());
                window.set_mok_detail(detail.into());
                window.set_feedback(text.into());
            }
            None => {}
        }
    });
    poller
}

/// What to do when the firmware refuses to start after the request is recorded.
///
/// This has to be readable before the user reboots: the window is the only
/// place that knows why the machine may not come back.
const BOOT_RECOVERY: &str = "无法开机时：进 BIOS 恢复默认安全启动密钥（Restore Factory Keys）；或关闭安全启动后运行 sudo mokutil --revoke-import。";

/// The switch's two states, in words that say what happens next.
///
/// Enrolling a key changes what the firmware will accept at boot, and some
/// machines refuse to start afterwards until their default keys are restored.
/// The recovery steps therefore sit in the same sentence as the password, not
/// in a help page the user has no way to open once the machine will not boot.
fn mok_texts(action: secure_boot::Action, password: Option<&str>) -> (&'static str, String) {
    if action == secure_boot::Action::Allow {
        let detail = password.map_or_else(
            || {
                "重启后蓝屏选 Enroll MOK → Continue → Yes，输入登记时设置的 MOK 密码（忘了就关掉开关再打开）。若报错无法开机：进 BIOS 恢复默认安全启动密钥。"
                    .to_owned()
            },
            |password| {
                format!(
                    "重启后蓝屏选 Enroll MOK → Continue → Yes，密码 {password}。若报错无法开机：进 BIOS 恢复默认安全启动密钥。"
                )
            },
        );
        ("签名密钥已登记，重启后生效", detail)
    } else {
        (
            "允许登记签名密钥",
            "打开后登记本机的 DKMS 公钥，重启才生效。若重启后报错、无法开机：进 BIOS 恢复默认安全启动密钥。"
                .to_owned(),
        )
    }
}

/// Writes the switch's card from [`mok_texts`].
fn apply_mok_texts(window: &SettingsWindow, action: secure_boot::Action, password: Option<&str>) {
    let (title, detail) = mok_texts(action, password);
    window.set_mok_title(title.into());
    window.set_mok_detail(detail.into());
}

/// Copies what the daemon last published into the window.
///
/// While the firmware is refusing the virtual-display module, the status card
/// explains that instead of the cable: the phone would otherwise sit there
/// saying the screen failed for no stated reason.
fn refresh(window: &SettingsWindow, secure: &secure_boot::Status) {
    let running = control::is_running();
    let state = live_state();
    let cable = cable_of(state.as_ref());
    window.set_running(running);
    window.set_attached(cable == Cable::WithUs);
    window.set_streaming(state.as_ref().is_some_and(|state| state.streaming));
    window.set_auto_takeover(Config::load().auto_takeover);
    let (enable_label, transfer_label) = button_labels(cable);
    window.set_enable_label(enable_label.into());
    window.set_transfer_label(transfer_label.into());

    let look = Look::of(state.as_ref());
    if secure.blocked() {
        window.set_accent(rgb(180, 57, 58));
        window.set_state_title("安全启动拦住了虚拟显示".into());
        window.set_state_detail(
            "副屏依赖 evdi 虚拟显示模块，而安全启动只允许受信任的模块加载。在下面登记本机的签名密钥并重启，副屏才能工作。"
                .into(),
        );
    } else {
        window.set_accent(look.accent);
        window.set_state_title(look.title.into());
        window.set_state_detail(look.detail.into());
    }

    let metrics = Metrics::of(state.as_ref());
    window.set_cpu_text(metrics.cpu_text.into());
    window.set_cpu_ratio(metrics.cpu_ratio);
    window.set_mem_text(metrics.mem_text.into());
    window.set_mem_ratio(metrics.mem_ratio);
    window.set_temp_text(metrics.temp_text.into());
    window.set_temp_label(metrics.temp_label.into());
    window.set_temp_ratio(metrics.temp_ratio);
    window.set_temp_tint(metrics.temp_tint);
}

/// The colours and words the top card uses.
struct Look {
    accent: slint::Color,
    title: String,
    detail: String,
}

impl Look {
    fn of(state: Option<&State>) -> Self {
        match state {
            None => Self {
                accent: rgb(180, 57, 58),
                title: "后台服务没有响应".to_owned(),
                detail: "点下面的“重启副屏服务”就能拉起；一直失败可以看 journalctl --user -u androscreen。".to_owned(),
            },
            Some(state) if state.streaming => Self {
                accent: rgb(15, 123, 108),
                title: "手机正在作为副屏".to_owned(),
                detail: format!(
                    "正在推送 {} 的画面（{}）。一根数据线同一时刻只能有一种用途，要换回文件传输就点“改用文件传输”，也可以在手机上点“退出副屏”。",
                    if state.screen.is_empty() {
                        "电脑".to_owned()
                    } else {
                        state.screen.clone()
                    },
                    stream_summary(state)
                ),
            },
            Some(state) if state.phone => Self {
                accent: rgb(42, 93, 176),
                title: "已连接手机".to_owned(),
                detail: "手机已连上，但还没开始投屏；在手机上点“用作扩展屏”即可。".to_owned(),
            },
            Some(_) => Self {
                accent: rgb(107, 120, 144),
                title: "等待手机".to_owned(),
                detail: "插上数据线并保持解锁。手机若在“文件传输”模式，副屏不会抢占它；要强行接管就点“启用副屏”。".to_owned(),
            },
        }
    }
}

/// The last second of streaming, as a clause short enough for the status card.
///
/// Showing it beside the picture is what lets the user tell a slow phone from a
/// slow link without reading a log.
fn stream_summary(state: &State) -> String {
    let Some(fps) = state.fps else {
        return "正在测量帧率".to_owned();
    };
    let mut parts = vec![format!("{fps:.1} 帧/秒")];
    if let Some(mbps) = state.mbps {
        parts.push(format!("{mbps:.1} Mbps"));
    }
    if let Some(dropped) = state.dropped.filter(|dropped| *dropped > 0) {
        parts.push(format!("丢帧 {dropped}"));
    }
    if let Some(behind) = state.behind.filter(|behind| *behind > 0) {
        parts.push(format!("落后 {behind} 次"));
    }
    parts.join("，")
}

/// The three numbers under the top card.
struct Metrics {
    cpu_text: String,
    cpu_ratio: f32,
    mem_text: String,
    mem_ratio: f32,
    temp_text: String,
    temp_label: String,
    temp_ratio: f32,
    temp_tint: slint::Color,
}

impl Metrics {
    fn of(state: Option<&State>) -> Self {
        let (cpu_text, cpu_ratio) = percent(state.and_then(|state| state.cpu_percent));
        let (mem_text, mem_ratio) = percent(state.and_then(|state| state.mem_percent));
        let hottest = state.and_then(State::hottest);
        Self {
            cpu_text,
            cpu_ratio,
            mem_text,
            mem_ratio,
            temp_text: hottest
                .map_or_else(|| "—".to_owned(), |temp| format!("{:.0}°C", temp.celsius)),
            temp_label: hottest
                .map_or_else(|| "温度".to_owned(), |temp| format!("{} 温度", temp.label)),
            temp_ratio: hottest.map_or(0.0, |temp| (temp.celsius / 100.0).clamp(0.0, 1.0)),
            temp_tint: hottest.map_or_else(|| rgb(152, 163, 181), |temp| heat(temp.celsius)),
        }
    }
}

fn percent(value: Option<f32>) -> (String, f32) {
    value.map_or_else(
        || ("—".to_owned(), 0.0),
        |value| (format!("{value:.0}%"), (value / 100.0).clamp(0.0, 1.0)),
    )
}

/// Green below a warm idle, amber where a fan is audible, red where it matters.
fn heat(celsius: f32) -> slint::Color {
    if celsius >= 85.0 {
        rgb(180, 57, 58)
    } else if celsius >= 70.0 {
        rgb(180, 83, 9)
    } else {
        rgb(15, 123, 108)
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> slint::Color {
    slint::Color::from_rgb_u8(red, green, blue)
}

/// Shows what the last click did.
fn set_feedback(window: &slint::Weak<SettingsWindow>, text: &str) {
    if let Some(window) = window.upgrade() {
        window.set_feedback(text.into());
    }
}

/// Hands one request to the daemon and reports whether it got through.
fn send(window: &slint::Weak<SettingsWindow>, request: Request, done: &str) {
    let message = if !control::is_running() {
        "副屏服务没有运行，请先点“重启副屏服务”。"
    } else if control::request(request).is_ok() {
        done
    } else {
        "请求没能送到后台服务，请先点“重启副屏服务”。"
    };
    set_feedback(window, message);
}

/// Where the cable sits right now. A daemon that is not answering cannot be
/// holding the port, so the cable counts as the phone's.
fn cable_now() -> Cable {
    cable_of(live_state().as_ref())
}

fn cable_of(state: Option<&State>) -> Cable {
    if state.is_some_and(|state| state.phone) {
        Cable::WithUs
    } else {
        Cable::WithPhone
    }
}

/// The daemon's last report, dropped once it stops answering.
fn live_state() -> Option<State> {
    control::read_state().filter(|_| control::is_running())
}

/// Saves the phone app and exits with a status a script can read.
#[expect(
    clippy::print_stdout,
    reason = "the command-line form of the download button reports the path"
)]
#[expect(
    clippy::print_stderr,
    reason = "and says on stderr why it could not save"
)]
fn save_apk() -> Result<(), slint::PlatformError> {
    match apk::save() {
        Ok(path) => {
            println!("{}", path.display());
            std::process::exit(0);
        }
        Err(reason) => {
            eprintln!("{reason}");
            std::process::exit(1);
        }
    }
}

/// Asks copies of this window from earlier runs to close.
///
/// Returns `false` when one of them is still alive: it owns the lock, and
/// opening a second window over it is what the user sees as two title bars.
fn retire_other_copies() -> bool {
    let others = other_copies();
    if others.is_empty() {
        return true;
    }
    for pid in &others {
        // SAFETY: `kill` only reads the two integers it is given.
        unsafe { libc::kill(*pid, libc::SIGTERM) };
    }
    std::thread::sleep(GHOST_GRACE);
    other_copies().is_empty()
}

/// Processes running this same window, other than this one.
///
/// Matching is on the program name rather than the file behind it: an upgrade
/// replaces the binary, so the copy that is still on screen from the previous
/// release lives in a file that no longer has a name, and comparing devices and
/// inodes would miss exactly the window that has to go.
fn other_copies() -> Vec<i32> {
    let Some(own) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(std::ffi::OsString::from))
    else {
        return Vec::new();
    };
    let me = i32::try_from(std::process::id()).unwrap_or(-1);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // A zombie has an empty command line, which never matches, so a copy
        // that has just been asked to quit is not counted twice.
        if pid == me || pid <= 1 {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let program = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
        let program = String::from_utf8_lossy(program);
        let program = std::path::Path::new(program.as_ref());
        if program.file_name() == Some(own.as_os_str()) {
            pids.push(pid);
        }
    }
    pids
}

/// Keeps one copy of the window on screen at a time.
///
/// The descriptor is handed back to the caller, which holds it for the life of
/// the process: dropping it would release the lock and let the next click open
/// a second window.
fn claim_single_window() -> Option<std::fs::File> {
    let path = lock_path()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    // SAFETY: `file` owns a live descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Some(file);
    }
    None
}

fn lock_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(dir).join("androscreen/settings.lock"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/androscreen/settings.lock"))
}

/// Presses each button on a window drawn without a display.
///
/// A `TouchArea` reaches its component's callback only when the markup says
/// so, and a button that looks right while forwarding nothing was the whole of
/// "启用副屏 does nothing". The layout is the one the preview renders, so these
/// coordinates are the buttons' centres as the pointer sees them.
#[cfg(test)]
mod press_test {
    use super::ui::SettingsWindow;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::PointerEventButton;
    use slint::platform::{Platform, PlatformError, WindowAdapter, WindowEvent};
    use slint::{ComponentHandle, LogicalPosition, PhysicalSize};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SCREEN_PRESSES: AtomicUsize = AtomicUsize::new(0);
    static TRANSFER_PRESSES: AtomicUsize = AtomicUsize::new(0);

    /// The row of two buttons, from the layout in `ui.rs`.
    const BUTTON_ROW_Y: f32 = 340.0;
    const TRANSFER_X: f32 = 121.0;
    const SCREEN_X: f32 = 338.0;

    struct Software {
        window: Rc<MinimalSoftwareWindow>,
    }

    impl Platform for Software {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            let window = Rc::clone(&self.window);
            Ok(window)
        }
    }

    #[test]
    fn a_press_on_each_button_reaches_the_window() {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Software {
            window: Rc::clone(&window),
        }))
        .expect("no other test installs a backend");

        let ui = SettingsWindow::new().expect("the software backend always builds a window");
        ui.on_enable_clicked(|| {
            SCREEN_PRESSES.fetch_add(1, Ordering::SeqCst);
        });
        ui.on_release_clicked(|| {
            TRANSFER_PRESSES.fetch_add(1, Ordering::SeqCst);
        });
        window.set_size(PhysicalSize::new(460, 554));
        ui.show().expect("the window shows off-screen");
        slint::platform::update_timers_and_animations();

        press(&ui, LogicalPosition::new(SCREEN_X, BUTTON_ROW_Y));
        assert_eq!(
            SCREEN_PRESSES.load(Ordering::SeqCst),
            1,
            "the secondary-screen button never reached its callback"
        );
        press(&ui, LogicalPosition::new(TRANSFER_X, BUTTON_ROW_Y));
        assert_eq!(
            TRANSFER_PRESSES.load(Ordering::SeqCst),
            1,
            "the file-transfer button never reached its callback"
        );
    }

    fn press(ui: &SettingsWindow, at: LogicalPosition) {
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: at,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: at,
            button: PointerEventButton::Left,
        });
    }
}
