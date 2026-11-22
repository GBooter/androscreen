//! Renders the settings window into an image, without a display.
//!
//! Run with `cargo run --example preview -p androscreen-settings`. It is a
//! development tool: the window is drawn by the same markup and the same
//! software renderer the real one uses, so a layout that is wrong here is
//! wrong on screen too.
//!
//! `PREVIEW_STATE` picks which state to draw: `streaming` (default), `phone`,
//! `waiting`, `down`, `secure` (the firmware is holding the module back) or
//! `reboot` (the module is installed and waiting for the next boot).
//! `PREVIEW_LANG` picks the language: `zh` (default) or `en`. Both matter,
//! because the two languages put different amounts of text in the same cards
//! and the longer one is what a layout regression shows up in first.

// The example pulls `i18n.rs` in by path, so everything the window uses but
// this tool does not — the language memory, the daemon's restart sentences —
// would be reported as dead code here. It is not dead; it is simply not on
// this tool's path through the words.
#![allow(dead_code)]

use std::io::Write;

use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::ComponentHandle;

#[path = "../src/cable.rs"]
mod cable;
#[path = "../src/i18n.rs"]
mod i18n;
#[path = "../src/ui.rs"]
mod ui;

use cable::{button_labels, screen_action, transfer_action, Cable};
use i18n::{Lang, Words};
use ui::SettingsWindow;

const WIDTH: u32 = 520;
const HEIGHT: u32 = 660;

struct Preview {
    window: std::rc::Rc<MinimalSoftwareWindow>,
}

impl Platform for Preview {
    fn create_window_adapter(&self) -> Result<std::rc::Rc<dyn WindowAdapter>, PlatformError> {
        let window = std::rc::Rc::clone(&self.window);
        Ok(window)
    }
}

#[expect(
    clippy::print_stdout,
    reason = "a development tool reports the file it wrote on stdout"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let window = MinimalSoftwareWindow::new(
        slint::platform::software_renderer::RepaintBufferType::NewBuffer,
    );
    slint::platform::set_platform(Box::new(Preview {
        window: std::rc::Rc::clone(&window),
    }))?;

    let state = std::env::var("PREVIEW_STATE").unwrap_or_else(|_| "streaming".to_owned());
    let lang = match std::env::var("PREVIEW_LANG").as_deref() {
        Ok("en" | "EN" | "En") => Lang::En,
        _ => Lang::Zh,
    };
    let ui = SettingsWindow::new()?;
    fill(&ui, &state, lang);

    let size = slint::PhysicalSize::new(WIDTH, HEIGHT);
    window.set_size(size);
    ui.show()?;
    slint::platform::update_timers_and_animations();

    let width = size.width as usize;
    let height = size.height as usize;
    let mut buffer = vec![Rgb565Pixel(0xFFFF); width * height];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width);
    });

    let name = format!(
        "preview-{state}-{}.ppm",
        if lang == Lang::En { "en" } else { "zh" }
    );
    let mut out = std::fs::File::create(&name)?;
    write!(out, "P6\n{width} {height}\n255\n")?;
    for pixel in &buffer {
        let value = pixel.0;
        let r = ((value >> 11) & 0x1f) as u8;
        let g = ((value >> 5) & 0x3f) as u8;
        let b = (value & 0x1f) as u8;
        out.write_all(&[
            (r << 3) | (r >> 2),
            (g << 2) | (g >> 4),
            (b << 3) | (b >> 2),
        ])?;
    }
    println!("wrote {name} ({width}x{height})");
    Ok(())
}

/// Loads one of the states the daemon can put the window in.
///
/// The labels and the feedback line come from the same words the real window
/// uses, so a preview can not show a phrase the buttons would never say.
fn fill(ui: &SettingsWindow, state: &str, lang: Lang) {
    let words = Words::of(lang);
    i18n::apply(ui, lang, "v0.4.0");
    ui.set_version("v0.4.0".into());
    ui.set_apk_version("v0.4.0".into());
    ui.set_running(!state.eq("down"));
    let cable = if state == "streaming" || state == "phone" {
        Cable::WithUs
    } else {
        Cable::WithPhone
    };
    ui.set_attached(cable == Cable::WithUs);
    ui.set_streaming(state == "streaming");
    ui.set_auto_takeover(state == "streaming");
    let (enable_label, transfer_label) = button_labels(lang, cable);
    ui.set_enable_label(enable_label.into());
    ui.set_transfer_label(transfer_label.into());
    ui.set_cpu_text("22%".into());
    ui.set_cpu_ratio(0.22);
    ui.set_mem_text("53%".into());
    ui.set_mem_ratio(0.53);
    ui.set_temp_text("71°C".into());
    ui.set_temp_label(i18n::temperature_label(lang, Some("CPU")).into());
    ui.set_temp_ratio(0.71);
    ui.set_temp_tint(slint::Color::from_rgb_u8(180, 83, 9));

    // The three preconditions, as the real window computes them: a picture
    // needs the driver to be loaded, the firmware to allow it, and a phone on
    // the other end of the cable.
    let ok = slint::Color::from_rgb_u8(15, 123, 108);
    let attention = slint::Color::from_rgb_u8(180, 83, 9);
    let idle = slint::Color::from_rgb_u8(148, 158, 175);
    let mut driver = (words.state_ok, ok);
    let mut secure = (words.state_ok, ok);
    if state == "secure" {
        driver = (words.state_attention, attention);
        secure = (words.state_attention, attention);
    } else if state == "reboot" {
        driver = (words.state_attention, attention);
    }
    let link = if state == "streaming" || state == "phone" {
        (words.state_ok, ok)
    } else {
        (words.state_off, idle)
    };
    ui.set_driver_state(driver.0.into());
    ui.set_driver_tint(driver.1);
    ui.set_secure_state(secure.0.into());
    ui.set_secure_tint(secure.1);
    ui.set_link_state(link.0.into());
    ui.set_link_tint(link.1);

    match state {
        "phone" => {
            ui.set_accent(slint::Color::from_rgb_u8(42, 93, 176));
            ui.set_state_title(words.phone_title.into());
            ui.set_state_detail(words.phone_detail.into());
            ui.set_feedback(screen_action(lang, cable).1.into());
        }
        "waiting" => {
            ui.set_accent(slint::Color::from_rgb_u8(107, 120, 144));
            ui.set_state_title(words.waiting_title.into());
            ui.set_state_detail(words.waiting_detail.into());
            ui.set_temp_text("—".into());
            ui.set_temp_label(i18n::temperature_label(lang, None).into());
            ui.set_temp_ratio(0.0);
            ui.set_temp_tint(slint::Color::from_rgb_u8(152, 163, 181));
            ui.set_feedback(screen_action(lang, cable).1.into());
        }
        "down" => {
            ui.set_accent(slint::Color::from_rgb_u8(180, 57, 58));
            ui.set_state_title(words.service_down_title.into());
            ui.set_state_detail(words.service_down_detail.into());
            ui.set_hero_action(4);
            ui.set_hero_action_label(words.restart_service.into());
            ui.set_feedback(words.service_restarted.into());
        }
        "secure" => {
            ui.set_evdi_blocked(true);
            ui.set_accent(slint::Color::from_rgb_u8(180, 57, 58));
            ui.set_state_title(words.secure_blocked_title.into());
            ui.set_state_detail(words.secure_blocked_detail.into());
            ui.set_mok_title(words.mok_allow_title.into());
            ui.set_mok_detail(i18n::mok_allow_detail(lang, Some("48190274")).into());
            ui.set_hero_action(2);
            ui.set_hero_action_label(words.action_withdraw.into());
            ui.set_feedback(words.enrolled.into());
        }
        "confirm" => {
            // The one dialog that writes to the firmware: it has to be legible
            // in full before anyone presses the second button.
            ui.set_evdi_blocked(true);
            ui.set_accent(slint::Color::from_rgb_u8(180, 57, 58));
            ui.set_state_title(words.secure_blocked_title.into());
            ui.set_state_detail(words.secure_blocked_detail.into());
            ui.set_mok_title(words.mok_allow_title.into());
            ui.set_mok_detail(i18n::mok_allow_detail(lang, Some("48190274")).into());
            ui.set_hero_action(1);
            ui.set_hero_action_label(words.action_enroll.into());
            ui.set_mok_confirm_open(true);
        }
        "reboot" => {
            ui.set_reboot_required(true);
            ui.set_accent(slint::Color::from_rgb_u8(180, 83, 9));
            ui.set_state_title(words.reboot_required_title.into());
            ui.set_state_detail(words.reboot_required_detail.into());
            ui.set_hero_action(3);
            ui.set_hero_action_label(words.reboot_button.into());
            ui.set_feedback(words.reboot_first.into());
        }
        _ => {
            ui.set_accent(slint::Color::from_rgb_u8(15, 123, 108));
            ui.set_state_title(words.streaming_title.into());
            ui.set_state_detail(
                i18n::streaming_detail(
                    lang,
                    "1080×2460",
                    &i18n::stream_summary(lang, Some(44.5), Some(12.0), Some(3), Some(1)),
                )
                .into(),
            );
            ui.set_feedback(transfer_action(lang, cable).1.into());
        }
    }
}
