//! Renders the settings window into an image, without a display.
//!
//! Run with `cargo run --example preview -p androscreen-settings`. It is a
//! development tool: the window is drawn by the same markup and the same
//! software renderer the real one uses, so a layout that is wrong here is
//! wrong on screen too.
//!
//! `PREVIEW_STATE` picks which state to draw: `streaming` (default), `phone`,
//! `waiting`, or `down`.

use std::io::Write;

use slint::platform::software_renderer::{MinimalSoftwareWindow, Rgb565Pixel};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::ComponentHandle;

#[path = "../src/cable.rs"]
mod cable;
#[path = "../src/ui.rs"]
mod ui;

use cable::{button_labels, screen_action, transfer_action, Cable};
use ui::SettingsWindow;

const WIDTH: u32 = 460;
const HEIGHT: u32 = 554;

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
    let ui = SettingsWindow::new()?;
    fill(&ui, &state);

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

    let name = format!("preview-{state}.ppm");
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
/// The button labels and the feedback line come from the same mapping the real
/// window uses, so a preview can not show a word the buttons would never say.
fn fill(ui: &SettingsWindow, state: &str) {
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
    let (enable_label, transfer_label) = button_labels(cable);
    ui.set_enable_label(enable_label.into());
    ui.set_transfer_label(transfer_label.into());
    ui.set_cpu_text("22%".into());
    ui.set_cpu_ratio(0.22);
    ui.set_mem_text("53%".into());
    ui.set_mem_ratio(0.53);
    ui.set_temp_text("71°C".into());
    ui.set_temp_label("CPU 温度".into());
    ui.set_temp_ratio(0.71);
    ui.set_temp_tint(slint::Color::from_rgb_u8(180, 83, 9));

    match state {
        "phone" => {
            ui.set_accent(slint::Color::from_rgb_u8(42, 93, 176));
            ui.set_state_title("已连接手机".into());
            ui.set_state_detail("手机已连上，但还没开始投屏；在手机上点“用作扩展屏”即可。".into());
            ui.set_feedback(screen_action(cable).1.into());
        }
        "waiting" => {
            ui.set_accent(slint::Color::from_rgb_u8(107, 120, 144));
            ui.set_state_title("等待手机".into());
            ui.set_state_detail(
                "插上数据线并保持解锁。手机若在“文件传输”模式，副屏不会抢占它；要强行接管就点“启用副屏”。"
                    .into(),
            );
            ui.set_temp_text("—".into());
            ui.set_temp_label("温度".into());
            ui.set_temp_ratio(0.0);
            ui.set_temp_tint(slint::Color::from_rgb_u8(152, 163, 181));
            ui.set_feedback(screen_action(cable).1.into());
        }
        "down" => {
            ui.set_accent(slint::Color::from_rgb_u8(180, 57, 58));
            ui.set_state_title("后台服务没有响应".into());
            ui.set_state_detail(
                "点下面的“重启副屏服务”就能拉起；一直失败可以看 journalctl --user -u androscreen。"
                    .into(),
            );
            ui.set_feedback("服务已重启。".into());
        }
        _ => {
            ui.set_accent(slint::Color::from_rgb_u8(15, 123, 108));
            ui.set_state_title("手机正在作为副屏".into());
            ui.set_state_detail(
                "正在推送 1080×2460 的画面。一根数据线同一时刻只能有一种用途，要换回文件传输就点“改用文件传输”，也可以在手机上点“退出副屏”。"
                    .into(),
            );
            ui.set_feedback(transfer_action(cable).1.into());
        }
    }
}
