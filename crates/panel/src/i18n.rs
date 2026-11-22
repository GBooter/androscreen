//! Everything the window says, in both languages it can say it in.
//!
//! The sentences live here rather than beside the code that draws them, for the
//! same reason the window's markup lives in its own module: one file can be
//! read top to bottom and corrected, and nothing can be added to the window in
//! one language only. [`Words`] is a struct rather than a lookup so a missing
//! translation is a compile error instead of a key that silently falls back.
//!
//! Which language to draw in is the user's, not the build's: the desktop's own
//! locale decides the first time, a switch in the window decides after that,
//! and the choice is kept beside the daemon's settings so the next window opens
//! the way the last one was left.

use crate::ui::SettingsWindow;

/// The two languages the window speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lang {
    /// Simplified Chinese.
    Zh,
    /// English.
    En,
}

impl Lang {
    /// The language to open in: the last choice, else the desktop's own.
    pub(crate) fn start() -> Self {
        Self::remembered().unwrap_or_else(Self::from_environment)
    }

    /// What the session asks for, through the variables POSIX gives it.
    ///
    /// They are read in the order that decides, and an empty one — which a
    /// desktop leaves behind to mean "unset" — is skipped rather than read as
    /// a language called nothing.
    pub(crate) fn from_environment() -> Self {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            let value = value.trim().to_ascii_lowercase();
            if value.is_empty() {
                continue;
            }
            return if value.starts_with("zh") {
                Self::Zh
            } else {
                Self::En
            };
        }
        Self::En
    }

    /// The other one.
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Zh => Self::En,
            Self::En => Self::Zh,
        }
    }

    /// The name this language goes by in the file that remembers it.
    fn code(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// Reads a name, accepting the spellings a hand-edited file may carry.
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-hans" | "zh-sg" | "chinese" => Some(Self::Zh),
            "en" | "en-us" | "en-gb" | "english" => Some(Self::En),
            _ => None,
        }
    }

    /// The language of the last window, when one was chosen.
    fn remembered() -> Option<Self> {
        let path = androscreen_core::control::config_dir()?.join(LANGUAGE_FILE);
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    /// Keeps the choice for the next time the window opens.
    ///
    /// A failure is not worth a sentence in the window: it is already drawing
    /// in the language that was asked for, and the only cost is that it starts
    /// in the desktop's language next time.
    pub(crate) fn remember(self) {
        let Some(dir) = androscreen_core::control::config_dir() else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let _ = std::fs::write(dir.join(LANGUAGE_FILE), self.code());
    }
}

/// The file the choice is kept in, beside the daemon's own settings.
const LANGUAGE_FILE: &str = "language";

/// One window's worth of words.
///
/// Every field is a phrase the window draws without any part filled in; the
/// sentences that carry a number, a path or a password are built by the
/// functions below, because `&'static str` cannot hold them.
pub(crate) struct Words {
    // The window itself, and the cards that are always there.
    pub(crate) window_title: &'static str,
    pub(crate) processor: &'static str,
    pub(crate) memory: &'static str,
    pub(crate) temperature: &'static str,
    pub(crate) reboot_button: &'static str,
    // The three preconditions a picture needs, named once and checked every
    // tick: a reader who does not know what evdi is can still see that one of
    // the three is amber.
    pub(crate) capability_driver: &'static str,
    pub(crate) capability_secure: &'static str,
    pub(crate) capability_link: &'static str,
    // The verdicts. Kept to one short word each, because three of them share
    // one row with their names in both languages.
    pub(crate) state_ok: &'static str,
    pub(crate) state_attention: &'static str,
    pub(crate) state_off: &'static str,
    // What the conclusion area offers next.
    pub(crate) action_enroll: &'static str,
    pub(crate) action_withdraw: &'static str,
    pub(crate) auto_title: &'static str,
    pub(crate) auto_detail: &'static str,
    pub(crate) apk_title: &'static str,
    pub(crate) apk_button: &'static str,
    pub(crate) restart_service: &'static str,
    // The confirmation that has to come before the firmware is touched.
    pub(crate) mok_confirm_title: &'static str,
    pub(crate) mok_confirm_body: &'static str,
    pub(crate) cancel: &'static str,
    pub(crate) enroll_anyway: &'static str,
    /// The language the switch would move to, written in that language.
    pub(crate) other_language: &'static str,
    // The status card.
    pub(crate) service_down_title: &'static str,
    pub(crate) service_down_detail: &'static str,
    pub(crate) phone_title: &'static str,
    pub(crate) phone_detail: &'static str,
    pub(crate) waiting_title: &'static str,
    pub(crate) waiting_detail: &'static str,
    pub(crate) streaming_title: &'static str,
    pub(crate) this_computer: &'static str,
    /// Shown in place of the numbers until the first second has been measured.
    pub(crate) measuring: &'static str,
    pub(crate) secure_blocked_title: &'static str,
    pub(crate) secure_blocked_detail: &'static str,
    pub(crate) reboot_required_title: &'static str,
    pub(crate) reboot_required_detail: &'static str,
    pub(crate) module_missing_title: &'static str,
    pub(crate) module_missing_detail: &'static str,
    // The two buttons, in each of the two states of the cable.
    pub(crate) screen_on_ours: &'static str,
    pub(crate) screen_on_theirs: &'static str,
    pub(crate) transfer_on_ours: &'static str,
    pub(crate) transfer_on_theirs: &'static str,
    // What the last click did.
    pub(crate) screen_done_ours: &'static str,
    pub(crate) screen_done_theirs: &'static str,
    pub(crate) transfer_done_ours: &'static str,
    pub(crate) transfer_done_theirs: &'static str,
    pub(crate) auto_on: &'static str,
    pub(crate) auto_off: &'static str,
    pub(crate) settings_unsaved: &'static str,
    pub(crate) reboot_first: &'static str,
    pub(crate) service_restarted: &'static str,
    pub(crate) no_systemctl: &'static str,
    pub(crate) rebooting: &'static str,
    pub(crate) rebooted: &'static str,
    pub(crate) reboot_by_hand: &'static str,
    pub(crate) choosing_where: &'static str,
    pub(crate) save_cancelled: &'static str,
    pub(crate) service_not_running: &'static str,
    pub(crate) request_lost: &'static str,
    // The key enrollment.
    pub(crate) key_missing_title: &'static str,
    pub(crate) key_missing_detail: &'static str,
    pub(crate) no_key_here: &'static str,
    pub(crate) enrolling_title: &'static str,
    pub(crate) enrolling_detail: &'static str,
    pub(crate) enroll_withdrawn: &'static str,
    /// The one word that opens the sentence after a successful enrollment.
    pub(crate) enrolled: &'static str,
    pub(crate) enroll_failed_title: &'static str,
    pub(crate) mok_allow_title: &'static str,
    pub(crate) mok_withdraw_title: &'static str,
    pub(crate) mok_withdraw_detail: &'static str,
    pub(crate) boot_recovery: &'static str,
    pub(crate) enrollment_cancelled: &'static str,
    pub(crate) password_lost: &'static str,
    // The phone app's card, and the save dialog behind it.
    pub(crate) apk_missing: &'static str,
    pub(crate) save_dialog_title: &'static str,
    /// The word on the dialog's affirmative button.
    pub(crate) save: &'static str,
    /// Another window already has the screen.
    pub(crate) already_running: &'static str,
}

impl Words {
    /// The words this language uses.
    pub(crate) fn of(lang: Lang) -> Self {
        match lang {
            Lang::Zh => Self::chinese(),
            Lang::En => Self::english(),
        }
    }

    fn chinese() -> Self {
        Self {
            window_title: "安卓副屏",
            processor: "处理器",
            memory: "内存",
            temperature: "温度",
            reboot_button: "立即重启",
            capability_driver: "驱动",
            capability_secure: "安全启动",
            capability_link: "连接",
            state_ok: "正常",
            state_attention: "需处理",
            state_off: "未启用",
            action_enroll: "登记签名密钥",
            action_withdraw: "撤销登记",
            auto_title: "插入手机时自动接管为副屏",
            auto_detail: "关闭后插线不会抢占手机，手机上仍可选“文件传输”。",
            apk_title: "下载手机安装包",
            apk_button: "下载",
            restart_service: "重启副屏服务",
            mok_confirm_title: "登记签名密钥并不适用于每台电脑",
            mok_confirm_body: "登记后重启时会先出现蓝色 MOK 界面：依次选择 Enroll MOK → Continue → Yes，再输入窗口里显示的密码。\n\n有些电脑的固件会拒绝这个登记，重启后出现 Security Policy Violation 或 Verification failed，导致无法进入系统。\n\n如果发生这种情况：进入 BIOS，执行“恢复默认安全启动密钥”（Restore Factory Keys），保存后即可正常开机；也可以关闭安全启动，进入系统后运行 sudo mokutil --revoke-import 清理登记。",
            cancel: "取消",
            enroll_anyway: "仍然登记",
            other_language: "English",
            service_down_title: "后台服务没有响应",
            service_down_detail: "点下面的“重启副屏服务”就能拉起；一直失败可以看 journalctl --user -u androscreen。",
            phone_title: "已连接手机",
            phone_detail: "手机已连上，但还没开始投屏；在手机上点“用作扩展屏”即可。",
            waiting_title: "等待手机",
            waiting_detail: "插上数据线并保持解锁。手机若在“文件传输”模式，副屏不会抢占它；要强行接管就点“启用副屏”。",
            streaming_title: "手机正在作为副屏",
            this_computer: "电脑",
            measuring: "正在测量帧率",
            secure_blocked_title: "安全启动拦住了虚拟显示",
            secure_blocked_detail: "副屏依赖 evdi 虚拟显示模块，而安全启动只允许受信任的模块加载。在下面登记本机的签名密钥并重启，副屏才能工作。",
            reboot_required_title: "重启后即可使用副屏",
            reboot_required_detail: "虚拟显示驱动已经装好，但这次登录还没有加载它：deepin 25.3 的 X11 桌面在登录期间加载显卡驱动会卡死，安装包因此把它留到了下次开机。重启一次就能使用，之后升级不再需要重启。",
            module_missing_title: "虚拟显示驱动没有加载",
            module_missing_detail: "副屏需要 evdi 模块和一块虚拟显示卡。请确认内核头文件已安装，运行 sudo dkms autoinstall 后重启；日志见 journalctl --user -u androscreen。",
            screen_on_ours: "关闭副屏",
            screen_on_theirs: "启用副屏",
            transfer_on_ours: "改用文件传输",
            transfer_on_theirs: "关闭文件传输",
            screen_done_ours: "副屏正在关闭：数据线交还手机，手机回到它自己的 USB 用途。",
            screen_done_theirs: "已请求接管手机，副屏会在几秒内出现。",
            transfer_done_ours: "已释放数据线。在手机的下拉通知里把 USB 用途选成“文件传输”，电脑上才会出现手机存储。",
            transfer_done_theirs: "一根数据线只能有一种用途：已收回它去当副屏，手机存储会断开。",
            auto_on: "已开启自动接管：插线后手机自动变成副屏。",
            auto_off: "已关闭自动接管：插线不再抢占手机，可以用文件传输。",
            settings_unsaved: "设置没能保存，请检查 ~/.config/androscreen 的权限。",
            reboot_first: "虚拟显示驱动要重启后才生效，请先点“立即重启”。",
            service_restarted: "服务已重启。",
            no_systemctl: "找不到 systemctl，无法重启服务。",
            rebooting: "正在重启系统…",
            rebooted: "系统正在重启…",
            reboot_by_hand: "找不到 systemctl，请手动重启系统。",
            choosing_where: "请在弹出的对话框里选择保存位置…",
            save_cancelled: "已取消保存。",
            service_not_running: "副屏服务没有运行，请先点“重启副屏服务”。",
            request_lost: "请求没能送到后台服务，请先点“重启副屏服务”。",
            key_missing_title: "找不到 DKMS 签名密钥",
            key_missing_detail: "请先安装内核头文件并重新编译 evdi（sudo dkms autoinstall），或关闭安全启动。",
            no_key_here: "这台机器上没有可用于登记的签名密钥。",
            enrolling_title: "正在登记签名密钥…",
            enrolling_detail: "系统会弹出授权对话框，请输入密码。完成后这里会显示重启时要在蓝屏输入的 MOK 密码。",
            enroll_withdrawn: "已撤回登记请求，重启后不会再出现 MOK 界面。",
            enrolled: "已登记。",
            enroll_failed_title: "登记没有完成",
            mok_allow_title: "签名密钥已登记，重启后生效",
            mok_withdraw_title: "允许登记签名密钥",
            mok_withdraw_detail: "打开后登记本机的 DKMS 公钥，重启才生效。若重启后报错、无法开机：进 BIOS 恢复默认安全启动密钥。",
            boot_recovery: "无法开机时：进 BIOS 恢复默认安全启动密钥（Restore Factory Keys）；或关闭安全启动后运行 sudo mokutil --revoke-import。",
            enrollment_cancelled: "授权被取消，没有登记任何内容。",
            password_lost: "没能把 MOK 密码交给 mokutil。",
            apk_missing: "这个安装里没有手机端 APK；请从发布页下载，或在项目目录执行 packaging/build-deb.sh。",
            save_dialog_title: "保存手机安装包",
            save: "保存",
            already_running: "AndroScreen 设置窗口已经在运行。",
        }
    }

    fn english() -> Self {
        Self {
            window_title: "AndroScreen",
            processor: "Processor",
            memory: "Memory",
            temperature: "Temperature",
            reboot_button: "Reboot now",
            capability_driver: "Driver",
            capability_secure: "Secure Boot",
            capability_link: "Link",
            state_ok: "OK",
            state_attention: "Action",
            state_off: "Off",
            action_enroll: "Enroll key",
            action_withdraw: "Withdraw",
            auto_title: "Take the phone over when it is plugged in",
            auto_detail: "When off, plugging in no longer takes the phone; it can still be used for file transfer.",
            apk_title: "Download the phone app",
            apk_button: "Download",
            restart_service: "Restart the service",
            mok_confirm_title: "Enrolling a signing key does not suit every machine",
            mok_confirm_body: "After enrolling, the next boot starts on a blue MOK screen: pick Enroll MOK → Continue → Yes, then type the password shown in this window.\n\nSome firmware refuses the enrollment and the machine then stops at Security Policy Violation or Verification failed instead of starting.\n\nIf that happens: enter the BIOS, use “Restore Factory Keys”, save, and the machine boots normally again. Turning Secure Boot off and running sudo mokutil --revoke-import from the desktop also clears the request.",
            cancel: "Cancel",
            enroll_anyway: "Enroll anyway",
            other_language: "中文",
            service_down_title: "The background service is not responding",
            service_down_detail: "“Restart the service” below brings it back; if that keeps failing, read journalctl --user -u androscreen.",
            phone_title: "Phone connected",
            phone_detail: "The phone is connected but not streaming yet; tap “Use as extended display” on it.",
            waiting_title: "Waiting for a phone",
            waiting_detail: "Plug in the cable and keep the phone unlocked. A phone left in file-transfer mode is not taken over; “Enable secondary screen” takes it anyway.",
            streaming_title: "The phone is the secondary screen",
            this_computer: "this computer",
            measuring: "measuring the frame rate",
            secure_blocked_title: "Secure Boot is refusing the virtual display",
            secure_blocked_detail: "The screen needs the evdi virtual display module, and Secure Boot only loads modules it trusts. Enroll this machine's signing key below and reboot, and the screen will work.",
            reboot_required_title: "Reboot once to use the screen",
            reboot_required_detail: "The virtual display driver is installed but this session never loaded it: loading a graphics driver into a deepin 25.3 X11 session freezes the desktop, so the package left it for the next boot. One reboot and it works; upgrades never need another.",
            module_missing_title: "The virtual display driver is not loaded",
            module_missing_detail: "The screen needs the evdi module and a virtual display card. Check that the kernel headers are installed, run sudo dkms autoinstall, then reboot; the log is in journalctl --user -u androscreen.",
            screen_on_ours: "Turn the screen off",
            screen_on_theirs: "Enable secondary screen",
            transfer_on_ours: "Use file transfer",
            transfer_on_theirs: "Stop file transfer",
            screen_done_ours: "Turning the screen off: the cable goes back to the phone and its own USB use.",
            screen_done_theirs: "Asked to take the phone over; the screen appears within seconds.",
            transfer_done_ours: "The cable was released. Pick “File transfer” in the phone's notification shade and its storage appears on the computer.",
            transfer_done_theirs: "One cable carries one use: it was taken back for the screen, so the phone storage disconnects.",
            auto_on: "Automatic takeover on: plugging in turns the phone into a secondary screen.",
            auto_off: "Automatic takeover off: plugging in no longer takes the phone, so file transfer works.",
            settings_unsaved: "The setting could not be saved; check the permissions of ~/.config/androscreen.",
            reboot_first: "The virtual display driver only works after a reboot; press “Reboot now” first.",
            service_restarted: "Service restarted.",
            no_systemctl: "systemctl was not found, so the service cannot be restarted.",
            rebooting: "Rebooting…",
            rebooted: "The system is rebooting…",
            reboot_by_hand: "systemctl was not found; reboot the machine by hand.",
            choosing_where: "Choose where to save it in the dialog…",
            save_cancelled: "Save cancelled.",
            service_not_running: "The service is not running; press “Restart the service” first.",
            request_lost: "The request did not reach the service; press “Restart the service” first.",
            key_missing_title: "DKMS signing key not found",
            key_missing_detail: "Install the kernel headers and rebuild evdi (sudo dkms autoinstall), or turn Secure Boot off.",
            no_key_here: "This machine has no signing key to enroll.",
            enrolling_title: "Enrolling the signing key…",
            enrolling_detail: "An authorization dialog opens next; enter your password. The MOK password to type on the blue screen will then be shown here.",
            enroll_withdrawn: "The request was withdrawn; the next boot shows no MOK screen.",
            enrolled: "Enrolled. ",
            enroll_failed_title: "Enrollment did not finish",
            mok_allow_title: "Signing key enrolled, active after a reboot",
            mok_withdraw_title: "Allow enrolling a signing key",
            mok_withdraw_detail: "Switching this on enrolls this machine's DKMS public key; it takes effect after a reboot. If the machine then refuses to boot: restore the default Secure Boot keys in the BIOS.",
            boot_recovery: "If the machine will not boot: enter the BIOS and use “Restore Factory Keys”, or turn Secure Boot off and run sudo mokutil --revoke-import.",
            enrollment_cancelled: "Authorization was cancelled; nothing was enrolled.",
            password_lost: "The MOK password could not be handed to mokutil.",
            apk_missing: "This install carries no phone APK; download it from the releases page, or run packaging/build-deb.sh in the source tree.",
            save_dialog_title: "Save the phone app",
            save: "Save",
            already_running: "The AndroScreen settings window is already running.",
        }
    }
}

/// The streaming card's sentence: what is being sent, and how well.
pub(crate) fn streaming_detail(lang: Lang, screen: &str, summary: &str) -> String {
    match lang {
        Lang::Zh => format!(
            "正在推送 {screen} 的画面（{summary}）。一根数据线同一时刻只能有一种用途，要换回文件传输就点“改用文件传输”，也可以在手机上点“退出副屏”。"
        ),
        Lang::En => format!(
            "Streaming {screen} ({summary}). One cable carries one use at a time: to go back to file transfer press “Use file transfer”, or tap “Exit secondary screen” on the phone."
        ),
    }
}

/// The last second of streaming, as a clause short enough for the status card.
///
/// Showing it beside the picture is what lets the user tell a slow phone from a
/// slow link without reading a log.
pub(crate) fn stream_summary(
    lang: Lang,
    fps: Option<f32>,
    mbps: Option<f32>,
    dropped: Option<u32>,
    behind: Option<u32>,
) -> String {
    let Some(fps) = fps else {
        return Words::of(lang).measuring.to_owned();
    };
    let mut parts = vec![match lang {
        Lang::Zh => format!("{fps:.1} 帧/秒"),
        Lang::En => format!("{fps:.1} fps"),
    }];
    if let Some(mbps) = mbps {
        parts.push(format!("{mbps:.1} Mbps"));
    }
    if let Some(dropped) = dropped.filter(|dropped| *dropped > 0) {
        parts.push(match lang {
            Lang::Zh => format!("丢帧 {dropped}"),
            Lang::En => format!("{dropped} dropped"),
        });
    }
    if let Some(behind) = behind.filter(|behind| *behind > 0) {
        parts.push(match lang {
            Lang::Zh => format!("落后 {behind} 次"),
            Lang::En => format!("{behind} behind"),
        });
    }
    parts.join(match lang {
        Lang::Zh => "，",
        Lang::En => ", ",
    })
}

/// The third metric's caption: the sensor's own name plus what it measures.
pub(crate) fn temperature_label(lang: Lang, sensor: Option<&str>) -> String {
    match (lang, sensor) {
        (Lang::Zh, Some(sensor)) => format!("{sensor} 温度"),
        (Lang::Zh, None) => Words::of(lang).temperature.to_owned(),
        (Lang::En, Some(sensor)) => format!("{sensor} temp"),
        (Lang::En, None) => Words::of(lang).temperature.to_owned(),
    }
}

/// The phone app's card: which release it is, and how to get it onto a phone.
pub(crate) fn apk_detail(lang: Lang, version: &str) -> String {
    match lang {
        Lang::Zh => format!("安卓端 {version} · 用数据线或聊天软件装到手机"),
        Lang::En => format!("Phone app {version} · install over a cable or a chat app"),
    }
}

/// Where the phone app was saved.
pub(crate) fn saved_to(lang: Lang, path: &std::path::Path) -> String {
    match lang {
        Lang::Zh => format!("安装包已保存到 {}", path.display()),
        Lang::En => format!("Saved the APK to {}", path.display()),
    }
}

/// The download directory could not be made.
pub(crate) fn apk_dir_failed(lang: Lang, dir: &std::path::Path, error: &str) -> String {
    match lang {
        Lang::Zh => format!("无法创建下载目录 {}：{error}", dir.display()),
        Lang::En => format!("Cannot create the download directory {}: {error}", dir.display()),
    }
}

/// The copy itself failed.
pub(crate) fn apk_save_failed(lang: Lang, error: &str) -> String {
    match lang {
        Lang::Zh => format!("保存失败：{error}"),
        Lang::En => format!("Saving failed: {error}"),
    }
}

/// The desktop's save dialog could not be started.
pub(crate) fn apk_dialog_failed(lang: Lang, error: &str) -> String {
    match lang {
        Lang::Zh => format!("打不开保存对话框：{error}"),
        Lang::En => format!("Cannot open the save dialog: {error}"),
    }
}

/// The copy to the place the user picked failed.
pub(crate) fn apk_save_to_failed(lang: Lang, path: &std::path::Path, error: &str) -> String {
    match lang {
        Lang::Zh => format!("保存到 {} 失败：{error}", path.display()),
        Lang::En => format!("Cannot save to {}: {error}", path.display()),
    }
}

/// The service restart exited with a status other than success.
pub(crate) fn restart_status(lang: Lang, status: &str) -> String {
    match lang {
        Lang::Zh => format!("服务重启返回 {status}"),
        Lang::En => format!("Restarting the service returned {status}"),
    }
}

/// The reboot command exited with a status other than success.
pub(crate) fn reboot_status(lang: Lang, status: &str) -> String {
    match lang {
        Lang::Zh => format!("重启命令返回 {status}，请手动重启系统。"),
        Lang::En => format!("The reboot command returned {status}; reboot the machine by hand."),
    }
}

/// The sentence the enrolled switch's card carries, with the password to type.
pub(crate) fn mok_allow_detail(lang: Lang, password: Option<&str>) -> String {
    match (lang, password) {
        (Lang::Zh, Some(password)) => format!(
            "重启后蓝屏选 Enroll MOK → Continue → Yes，密码 {password}。若报错无法开机：进 BIOS 恢复默认安全启动密钥。"
        ),
        (Lang::Zh, None) => {
            "重启后蓝屏选 Enroll MOK → Continue → Yes，输入登记时设置的 MOK 密码（忘了就关掉开关再打开）。若报错无法开机：进 BIOS 恢复默认安全启动密钥。"
                .to_owned()
        }
        (Lang::En, Some(password)) => format!(
            "At the next boot pick Enroll MOK → Continue → Yes; the password is {password}. If the machine then refuses to boot: restore the default Secure Boot keys in the BIOS."
        ),
        (Lang::En, None) => {
            "At the next boot pick Enroll MOK → Continue → Yes and type the MOK password set when enrolling (forgot it? switch off and on again). If the machine then refuses to boot: restore the default Secure Boot keys in the BIOS."
                .to_owned()
        }
    }
}

/// What to say when the enrollment failed and the same job can be done by hand.
pub(crate) fn manual_instruction(lang: Lang, detail: Option<&str>) -> String {
    let mut message = match lang {
        Lang::Zh => String::from(
            "登记没能完成，可以在终端运行：sudo mokutil --import /var/lib/dkms/mok.pub（会提示设置两次 MOK 密码，重启后在蓝屏输入同一个密码）",
        ),
        Lang::En => String::from(
            "Enrollment did not finish. Run sudo mokutil --import /var/lib/dkms/mok.pub in a terminal (it asks for the MOK password twice; type the same one on the blue screen at the next boot).",
        ),
    };
    if let Some(detail) = detail {
        message.push_str(match lang {
            Lang::Zh => "（",
            Lang::En => " (",
        });
        message.push_str(detail.lines().next().unwrap_or(detail));
        message.push_str(match lang {
            Lang::Zh => "）",
            Lang::En => ")",
        });
    }
    message
}

/// Writes every label that does not depend on the service or the firmware.
///
/// This is the half of the window that is decided by language alone, so it can
/// be applied from one place by the window and by the preview tool alike. The
/// status card, the feedback line and the two lines of the key card are written
/// by their owners, each time they change, in the language of that moment.
pub(crate) fn apply(window: &SettingsWindow, lang: Lang, apk_version: &str) {
    let words = Words::of(lang);
    window.set_window_title(words.window_title.into());
    window.set_processor_label(words.processor.into());
    window.set_memory_label(words.memory.into());
    window.set_driver_label(words.capability_driver.into());
    window.set_secure_label(words.capability_secure.into());
    window.set_link_label(words.capability_link.into());
    window.set_auto_title(words.auto_title.into());
    window.set_auto_detail(words.auto_detail.into());
    window.set_apk_title(words.apk_title.into());
    window.set_apk_detail(apk_detail(lang, apk_version).into());
    window.set_apk_button(words.apk_button.into());
    window.set_restart_label(words.restart_service.into());
    window.set_mok_confirm_title(words.mok_confirm_title.into());
    window.set_mok_confirm_body(words.mok_confirm_body.into());
    window.set_cancel_label(words.cancel.into());
    window.set_enroll_label(words.enroll_anyway.into());
    window.set_lang_label(words.other_language.into());
}
