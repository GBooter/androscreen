//! Desktop settings window for `AndroScreen`.
//!
//! The window is the only way to turn the service off without a terminal, and
//! the only way to hand the USB port back to the file manager while a phone is
//! plugged in. It talks to the daemon through the files described in
//! [`androscreen_core::control`], so it works whether or not the daemon was
//! started by the session or by hand.
//!
//! Every word it draws comes from [`i18n`], and the language is a piece of
//! state like any other: it starts from the desktop's locale or the last
//! choice, and the switch at the bottom of the window changes it in place.

use std::cell::Cell;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use androscreen_core::control::{self, Config, Request, State};
use slint::{ComponentHandle, Timer, TimerMode};

mod apk;
mod cable;
mod i18n;
mod secure_boot;
mod ui;

use cable::{button_labels, screen_action, transfer_action, Cable};
use i18n::{Lang, Words};
use ui::SettingsWindow;

/// How often the window picks up what the daemon last published.
const REFRESH: Duration = Duration::from_millis(900);

/// How long a line of "what the last click did" stays on the page.
///
/// The strip is news, not a status: left there it turns into a standing claim
/// that quietly stops being true ("the cable was released" outlives the cable).
const FEEDBACK_LIFE: Duration = Duration::from_secs(8);

/// When the strip empties. The window has one, so one is enough.
static FEEDBACK_UNTIL: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// How long a replaced copy of this window is given to close.
///
/// The old process may still hold the single-instance lock, so this has to
/// finish before the lock is claimed.
const GHOST_GRACE: Duration = Duration::from_millis(200);

/// What a background action left for the window to show.
enum Answer {
    /// A sentence for the feedback line.
    Text(String),
    /// The outcome of the key enrollment, which also replaces the two lines of
    /// the firmware card.
    Mok {
        title: String,
        detail: String,
        text: String,
    },
}

/// A slot a background thread fills and the window picks up on its next tick.
type SharedAnswer = std::sync::Arc<std::sync::Mutex<Option<Answer>>>;

/// The language the window is drawing in right now.
///
/// It is shared rather than copied so that the ticker and every callback read
/// the same value: a callback that captured its own copy would keep repainting
/// the status card in the language the window was opened in.
type SharedLang = Rc<Cell<Lang>>;

#[expect(
    clippy::print_stdout,
    reason = "a window started twice says so on stdout and exits"
)]
fn main() -> Result<(), slint::PlatformError> {
    let lang: SharedLang = Rc::new(Cell::new(Lang::start()));

    // The same button from a terminal, for a machine where the window cannot be
    // opened: `androscreen-settings --download-apk` prints where it went.
    if std::env::args().skip(1).any(|arg| arg == "--download-apk") {
        return save_apk(lang.get());
    }

    // A copy left over from an older release can be sitting on the desktop with
    // the same title. Stacked with this one it looks like a single window with
    // two title bars, which is exactly how it was reported.
    if !retire_other_copies() {
        println!("{}", Words::of(lang.get()).already_running);
        return Ok(());
    }
    let _lock = claim_single_window();

    let window = SettingsWindow::new()?;
    window.set_apk_version(format!("v{}", apk_version()).into());
    // The footer carries the version of this package. It used to be drawn from
    // a literal in the markup, and the rewrite to translated properties left
    // the property empty, so the footer showed a gap where the number belongs.
    window.set_version(format!("v{}", env!("CARGO_PKG_VERSION")).into());
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
    apply_language(&window, lang.get(), &secure);
    refresh(&window, &secure, lang.get());

    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    window.on_auto_toggled(move |value| {
        let words = Words::of(current.get());
        let mut config = Config::load();
        config.auto_takeover = value;
        let message = if config.save().is_err() {
            words.settings_unsaved
        } else if value {
            words.auto_on
        } else {
            words.auto_off
        };
        set_feedback(&weak, message);
    });

    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    window.on_release_clicked(move || {
        let (request, done) = transfer_action(current.get(), cable_now());
        send(&weak, current.get(), request, done);
    });

    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    window.on_enable_clicked(move || {
        // Without a loaded card a takeover request can only fail inside the
        // daemon, where the answer is a log line the user never sees.
        if weak
            .upgrade()
            .is_some_and(|window| window.get_reboot_required())
        {
            set_feedback(&weak, Words::of(current.get()).reboot_first);
            return;
        }
        let (request, done) = screen_action(current.get(), cable_now());
        send(&weak, current.get(), request, done);
    });

    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    window.on_restart_clicked(move || {
        let words = Words::of(current.get());
        let result = std::process::Command::new("systemctl")
            .args(["--user", "restart", "androscreen"])
            .status();
        let message = match result {
            Ok(status) if status.success() => words.service_restarted.to_owned(),
            Ok(status) => i18n::restart_status(current.get(), &status.to_string()),
            Err(_) => words.no_systemctl.to_owned(),
        };
        set_feedback(&weak, &message);
    });

    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    window.on_reboot_clicked(move || {
        let words = Words::of(current.get());
        set_feedback(&weak, words.rebooting);
        // An active local session may reboot without a password
        // (org.freedesktop.login1.reboot: allow_active=yes), which is the path
        // the desktop's own power menu takes.
        let result = std::process::Command::new("systemctl")
            .arg("reboot")
            .status();
        let message = match result {
            Ok(status) if status.success() => words.rebooted.to_owned(),
            Ok(status) => i18n::reboot_status(current.get(), &status.to_string()),
            Err(_) => words.reboot_by_hand.to_owned(),
        };
        set_feedback(&weak, &message);
    });

    // The action the conclusion area offers. It dispatches rather than doing
    // the work: each of the four already has a handler above, and repeating
    // their bodies here is how the two copies drift apart.
    let weak = window.as_weak();
    window.on_hero_action_clicked(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        match window.get_hero_action() {
            1 => window.set_mok_confirm_open(true),
            2 => window.invoke_mok_toggled(false),
            3 => window.invoke_reboot_clicked(),
            4 => window.invoke_restart_clicked(),
            _ => {}
        }
    });

    // The switch that changes the language, and the only callback that has to
    // redraw more than one line: every label on the page moves at once.
    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    let status = std::sync::Arc::clone(&secure);
    window.on_lang_clicked(move || {
        let next = current.get().toggled();
        current.set(next);
        next.remember();
        let Some(window) = weak.upgrade() else {
            return;
        };
        apply_language(&window, next, &status);
        // The status card carries sentences as well, so it is written again
        // rather than left in the language the page used to speak. Whatever
        // the strip was saying was said in the other language, so it goes.
        window.set_feedback(slint::SharedString::default());
        refresh(&window, &status, next);
    });

    let answer: SharedAnswer = std::sync::Arc::default();
    wire_download(&window, std::sync::Arc::clone(&answer), Rc::clone(&lang));
    wire_mok(
        &window,
        std::sync::Arc::clone(&answer),
        secure.key.as_deref(),
        Rc::clone(&lang),
    );
    let _poll = wire_feedback(&window, std::sync::Arc::clone(&answer));

    let status = std::sync::Arc::clone(&secure);
    let weak = window.as_weak();
    let current = Rc::clone(&lang);
    let ticker = Timer::default();
    ticker.start(TimerMode::Repeated, REFRESH, move || {
        if let Some(window) = weak.upgrade() {
            refresh(&window, &status, current.get());
        }
    });

    window.show()?;
    window.run()
}

/// Writes every label that does not depend on the service's state.
///
/// Called once when the window opens and again when the language switch moves.
/// The two lines of the key-enrollment card do depend on the firmware, so they
/// are re-derived here from the status rather than remembered as text.
fn apply_language(window: &SettingsWindow, lang: Lang, secure: &secure_boot::Status) {
    i18n::apply(window, lang, &apk_version());
    let words = Words::of(lang);
    if secure.key.is_none() {
        window.set_mok_title(words.key_missing_title.into());
        window.set_mok_detail(words.key_missing_detail.into());
    } else if secure.pending {
        apply_mok_texts(
            window,
            lang,
            secure_boot::Action::Allow,
            secure.password.as_deref(),
        );
    } else {
        apply_mok_texts(window, lang, secure_boot::Action::Withdraw, None);
    }
}

/// Wires the download button.
///
/// The save dialog blocks until the user answers, and the window has to keep
/// drawing behind it, so the work happens on a thread and the feedback line
/// picks the answer up on the next tick.
fn wire_download(window: &SettingsWindow, answer: SharedAnswer, lang: SharedLang) {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let weak = window.as_weak();
    let slot = answer;
    let guard = std::sync::Arc::clone(&busy);
    window.on_download_clicked(move || {
        if guard.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // The sentence the user is about to read is the one for the language
        // the window is in when they press the button.
        let chosen = lang.get();
        set_feedback(&weak, Words::of(chosen).choosing_where);
        let slot = std::sync::Arc::clone(&slot);
        let guard = std::sync::Arc::clone(&guard);
        std::thread::spawn(move || {
            let message = match apk::ask(chosen) {
                Ok(apk::SaveOutcome::Saved(path)) => i18n::saved_to(chosen, &path),
                Ok(apk::SaveOutcome::Cancelled) => Words::of(chosen).save_cancelled.to_owned(),
                // No dialog on this desktop: put it where downloads go and say
                // exactly where, which is the next best thing to asking.
                Ok(apk::SaveOutcome::NoDialog) => match apk::save(chosen) {
                    Ok(path) => i18n::saved_to(chosen, &path),
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
fn wire_mok(
    window: &SettingsWindow,
    answer: SharedAnswer,
    key: Option<&std::path::Path>,
    lang: SharedLang,
) {
    let missing = key.is_none();
    let weak = window.as_weak();
    window.on_mok_toggled(move |requested| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let chosen = lang.get();
        let words = Words::of(chosen);
        if missing {
            window.set_mok_title(words.key_missing_title.into());
            window.set_mok_detail(words.key_missing_detail.into());
            set_feedback(&weak, words.no_key_here);
            return;
        }
        if requested {
            window.set_mok_title(words.enrolling_title.into());
            window.set_mok_detail(words.enrolling_detail.into());
        }
        let action = if requested {
            secure_boot::Action::Allow
        } else {
            secure_boot::Action::Withdraw
        };
        let slot = std::sync::Arc::clone(&answer);
        std::thread::spawn(move || {
            let outcome = match secure_boot::enroll(action, chosen) {
                Ok(secure_boot::Outcome::Enrolled { password }) => {
                    let texts = Words::of(chosen);
                    let (title, detail) =
                        mok_texts(chosen, secure_boot::Action::Allow, Some(&password));
                    Answer::Mok {
                        title: title.to_owned(),
                        detail,
                        text: format!("{}{}", texts.enrolled, texts.boot_recovery),
                    }
                }
                Ok(secure_boot::Outcome::Withdrawn) => {
                    let (title, detail) = mok_texts(chosen, secure_boot::Action::Withdraw, None);
                    Answer::Mok {
                        title: title.to_owned(),
                        detail,
                        text: Words::of(chosen).enroll_withdrawn.to_owned(),
                    }
                }
                // The dialog was dismissed, mokutil refused, or there was no
                // key: say why in the card instead of silently flipping back.
                Err(reason) => Answer::Mok {
                    title: Words::of(chosen).enroll_failed_title.to_owned(),
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
            Some(Answer::Text(text)) => {
                arm_feedback();
                window.set_feedback(text.into());
            }
            Some(Answer::Mok { title, detail, text }) => {
                window.set_mok_title(title.into());
                window.set_mok_detail(detail.into());
                arm_feedback();
                window.set_feedback(text.into());
            }
            None => {}
        }
    });
    poller
}

/// The switch's two states, in words that say what happens next.
///
/// Enrolling a key changes what the firmware will accept at boot, and some
/// machines refuse to start afterwards until their default keys are restored.
/// The recovery steps therefore sit in the same sentence as the password, not
/// in a help page the user has no way to open once the machine will not boot.
fn mok_texts(lang: Lang, action: secure_boot::Action, password: Option<&str>) -> (&'static str, String) {
    let words = Words::of(lang);
    if action == secure_boot::Action::Allow {
        (words.mok_allow_title, i18n::mok_allow_detail(lang, password))
    } else {
        (words.mok_withdraw_title, words.mok_withdraw_detail.to_owned())
    }
}

/// Writes the switch's card from [`mok_texts`].
fn apply_mok_texts(
    window: &SettingsWindow,
    lang: Lang,
    action: secure_boot::Action,
    password: Option<&str>,
) {
    let (title, detail) = mok_texts(lang, action, password);
    window.set_mok_title(title.into());
    window.set_mok_detail(detail.into());
}

/// The phone app's version, as stamped by the package beside the bundled APK.
///
/// A release that only touches the desktop keeps the previous phone app, so the
/// label has to follow the APK rather than this crate's version. A source build
/// has no such file and falls back to its own version.
fn apk_version() -> String {
    std::fs::read_to_string("/usr/share/androscreen/apk-version")
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// Copies what the daemon last published into the window.
///
/// While the firmware is refusing the virtual-display module, the status card
/// explains that instead of the cable: the phone would otherwise sit there
/// saying the screen failed for no stated reason.
fn refresh(window: &SettingsWindow, secure: &secure_boot::Status, lang: Lang) {
    let words = Words::of(lang);
    expire_feedback(window);
    let running = control::is_running();
    let state = live_state();
    let cable = cable_of(state.as_ref());
    window.set_running(running);
    window.set_attached(cable == Cable::WithUs);
    window.set_streaming(state.as_ref().is_some_and(|state| state.streaming));
    window.set_auto_takeover(Config::load().auto_takeover);
    let (enable_label, transfer_label) = button_labels(lang, cable);
    window.set_enable_label(enable_label.into());
    window.set_transfer_label(transfer_label.into());

    // The three preconditions. Each is a colour and a word: the colour is what
    // a glance catches, the word is what still says it to a reader who cannot
    // tell the two greens apart.
    let (ok, attention, broken, idle) = (
        rgb(15, 123, 108),
        rgb(180, 83, 9),
        rgb(180, 57, 58),
        rgb(148, 158, 175),
    );
    let (driver_state, driver_tint) = if secure.usable() {
        (words.state_ok, ok)
    } else if secure.reboot_required() {
        (words.state_attention, attention)
    } else {
        (words.state_attention, broken)
    };
    let (secure_state, secure_tint) = if secure.blocked() {
        (words.state_attention, attention)
    } else {
        (words.state_ok, ok)
    };
    // The cable, not the picture: a phone that is plugged in has a link even
    // before it is drawing anything, and saying "off" there contradicted the
    // sentence at the top of the same page.
    let (link_state, link_tint) = if cable == Cable::WithUs {
        (words.state_ok, ok)
    } else {
        (words.state_off, idle)
    };
    window.set_driver_state(driver_state.into());
    window.set_driver_tint(driver_tint);
    window.set_secure_state(secure_state.into());
    window.set_secure_tint(secure_tint);
    window.set_link_state(link_state.into());
    window.set_link_tint(link_tint);

    // What the conclusion area offers next. The two cable actions have their
    // own buttons further down the page, so they are never repeated here: this
    // is for the three things that have no button of their own.
    let (action, label) = if secure.blocked() {
        if secure.key.is_none() {
            (0, "")
        } else if secure.pending {
            (2, words.action_withdraw)
        } else {
            (1, words.action_enroll)
        }
    } else if secure.reboot_required() {
        (3, words.reboot_button)
    } else if !running {
        (4, words.restart_service)
    } else {
        (0, "")
    };
    window.set_hero_action(action);
    window.set_hero_action_label(label.into());

    let look = Look::of(state.as_ref(), lang);
    window.set_reboot_required(secure.reboot_required());
    if secure.blocked() {
        window.set_accent(rgb(180, 57, 58));
        window.set_state_title(words.secure_blocked_title.into());
        window.set_state_detail(words.secure_blocked_detail.into());
    } else if secure.reboot_required() {
        // The normal first-install state: the driver is on disk but was left
        // for the next boot, and saying so here is what keeps "副屏没反应" from
        // reading like a broken app.
        window.set_accent(rgb(180, 83, 9));
        window.set_state_title(words.reboot_required_title.into());
        window.set_state_detail(words.reboot_required_detail.into());
    } else if !secure.usable() {
        window.set_accent(rgb(180, 57, 58));
        window.set_state_title(words.module_missing_title.into());
        window.set_state_detail(words.module_missing_detail.into());
    } else {
        window.set_accent(look.accent);
        window.set_state_title(look.title.into());
        window.set_state_detail(look.detail.into());
    }

    let metrics = Metrics::of(state.as_ref(), lang);
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
    fn of(state: Option<&State>, lang: Lang) -> Self {
        let words = Words::of(lang);
        match state {
            None => Self {
                accent: rgb(180, 57, 58),
                title: words.service_down_title.to_owned(),
                detail: words.service_down_detail.to_owned(),
            },
            Some(state) if state.streaming => Self {
                accent: rgb(15, 123, 108),
                title: words.streaming_title.to_owned(),
                detail: i18n::streaming_detail(
                    lang,
                    if state.screen.is_empty() {
                        words.this_computer
                    } else {
                        state.screen.as_str()
                    },
                    &i18n::stream_summary(
                        lang,
                        state.fps,
                        state.mbps,
                        state.dropped,
                        state.behind,
                    ),
                ),
            },
            Some(state) if state.phone => Self {
                accent: rgb(42, 93, 176),
                title: words.phone_title.to_owned(),
                detail: words.phone_detail.to_owned(),
            },
            Some(_) => Self {
                accent: rgb(107, 120, 144),
                title: words.waiting_title.to_owned(),
                detail: words.waiting_detail.to_owned(),
            },
        }
    }
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
    fn of(state: Option<&State>, lang: Lang) -> Self {
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
            temp_label: i18n::temperature_label(lang, hottest.map(|temp| temp.label.as_str())),
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

/// Shows what the last click did, and starts the clock that takes it away.
fn set_feedback(window: &slint::Weak<SettingsWindow>, text: &str) {
    arm_feedback();
    if let Some(window) = window.upgrade() {
        window.set_feedback(text.into());
    }
}

/// Gives the feedback strip its eight seconds again.
fn arm_feedback() {
    if let Ok(mut slot) = FEEDBACK_UNTIL.lock() {
        *slot = Some(Instant::now() + FEEDBACK_LIFE);
    }
}

/// Empties the strip once the sentence has had its time.
fn expire_feedback(window: &SettingsWindow) {
    let Ok(mut slot) = FEEDBACK_UNTIL.lock() else {
        return;
    };
    if slot.is_some_and(|deadline| Instant::now() >= deadline) {
        *slot = None;
        window.set_feedback(slint::SharedString::default());
    }
}

/// Hands one request to the daemon and reports whether it got through.
fn send(window: &slint::Weak<SettingsWindow>, lang: Lang, request: Request, done: &str) {
    let words = Words::of(lang);
    let message = if !control::is_running() {
        words.service_not_running
    } else if control::request(request).is_ok() {
        done
    } else {
        words.request_lost
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
fn save_apk(lang: Lang) -> Result<(), slint::PlatformError> {
    match apk::save(lang) {
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
/// "启用副屏 does nothing". The buttons are *found* rather than addressed by
/// remembered pixel coordinates: every layout change used to move them, and a
/// stale number silently pressed the neighbouring control instead. Scanning is
/// safe here because only the three callbacks this test wires are registered —
/// the download, restart and reboot handlers are unset, so a press that lands
/// on them does nothing at all.
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
    static LANG_PRESSES: AtomicUsize = AtomicUsize::new(0);

    /// The window's two edges, from `ui.rs`.
    const WIDTH: u32 = 520;
    const HEIGHT: u32 = 660;

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
        // The language switch is a label on a rectangle of its own, and a press
        // that lands on the text instead of the box would leave the window in
        // one language with no way out.
        ui.on_lang_clicked(|| {
            LANG_PRESSES.fetch_add(1, Ordering::SeqCst);
        });
        window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
        ui.show().expect("the window shows off-screen");
        slint::platform::update_timers_and_animations();

        assert!(
            hit(&ui, &SCREEN_PRESSES).is_some(),
            "the secondary-screen button never reached its callback"
        );
        assert!(
            hit(&ui, &TRANSFER_PRESSES).is_some(),
            "the file-transfer button never reached its callback"
        );
        assert!(
            hit(&ui, &LANG_PRESSES).is_some(),
            "the language switch never reached its callback"
        );
    }

    /// Scans the page for a point whose press moves `count`.
    ///
    /// Coarse first, then one pixel at a time around the hit: the point has to
    /// be inside the control, not merely near it, or the assertion would pass
    /// on a press that grazed a neighbouring element.
    fn hit(ui: &SettingsWindow, count: &AtomicUsize) -> Option<LogicalPosition> {
        let mut coarse = None;
        'scan: for y in (60..HEIGHT).step_by(9) {
            for x in (10..WIDTH).step_by(11) {
                let at = LogicalPosition::new(x as f32 + 1.0, y as f32 + 1.0);
                if fires(ui, count, at) {
                    coarse = Some((x, y));
                    break 'scan;
                }
            }
        }
        let (x, y) = coarse?;
        for dy in -3..=3 {
            for dx in -3..=3 {
                let at = LogicalPosition::new(x as f32 + dx as f32, y as f32 + dy as f32);
                if fires(ui, count, at) {
                    return Some(at);
                }
            }
        }
        Some(LogicalPosition::new(x as f32, y as f32))
    }

    fn fires(ui: &SettingsWindow, count: &AtomicUsize, at: LogicalPosition) -> bool {
        let before = count.load(Ordering::SeqCst);
        press(ui, at);
        count.load(Ordering::SeqCst) > before
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
