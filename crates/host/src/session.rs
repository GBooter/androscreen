//! Phone session: greeting, control plane, and display streaming.
//!
//! One session lives as long as the USB link. Inside it the phone can leave
//! the dashboard visible, or ask for a virtual display; either way the daemon
//! keeps the control channel alive and pushes statistics every second.
//!
//! Nothing in here treats an unplug as an error: a closed USB link unwinds the
//! session through ordinary `Drop` cleanup and the outer loop waits for the
//! phone to come back.

use std::collections::VecDeque;
use std::sync::mpsc::{
    self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use androscreen_core::control::SensorRead;
use androscreen_core::edid::{build_edid, rotated_mode, DisplayMode};
use androscreen_core::protocol::{
    AudioFormat, Control, Frame, Kind, ScreenRequest, Session, Size, VideoCodec, FLAG_KEYFRAME,
    PROTOCOL_VERSION,
};
use androscreen_core::stats::SystemStats;
use androscreen_core::touch::TouchFrame;
use thiserror::Error;

use crate::aoa::{self, AoaError, AoaLink, StreamCounters};
use crate::audio::{AudioCapture, CHANNELS, SAMPLE_RATE};
use crate::color::{chroma_rects, Nv12Frame};
use crate::cursor::CursorFeed;
use crate::encoder::{coded_size, Backend, Encoder, Nal};
use crate::evdi::{ClipRect, EvdiCard, EvdiError, Event, FrameBuffer, Mode};
use crate::requests;
use crate::sampler::StatsSampler;
use crate::uinput::TouchDevice;

/// How long the phone app has to greet after the accessory link opens.
const HELLO_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a desktop has to configure the virtual display.
const MODE_TIMEOUT: Duration = Duration::from_secs(5);
/// Same on Wayland, where the compositor re-probes the connector on its own schedule.
const WAYLAND_MODE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often streaming re-asks where the virtual output ended up.
const LAYOUT_PROBES: u8 = 8;
/// Longest the session sleeps between USB polls.
const POLL_SLICE: Duration = Duration::from_millis(4);
/// How many pictures may await their access unit before the oldest is given up
/// on. A real stream holds one or two; anything deeper is a stuck encoder, not a
/// delay worth measuring.
const PIPE_DEPTH: usize = 16;
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// Delay before probing again after a failed or finished connection.
const RECONNECT_DELAY: Duration = Duration::from_millis(800);
/// How long a display turned by the phone is left dark before it is rebuilt, on
/// a compositor that is not asked whether it has finished forgetting the old
/// one.
const REBUILD_PAUSE: Duration = Duration::from_millis(1500);
/// How finely [`wait_for_next_attempt`] measures its pause.
const ATTEMPT_SLICE: Duration = Duration::from_millis(100);
/// How long a reset port is given to re-enumerate before the phone is probed.
const REENUMERATE_PAUSE: Duration = Duration::from_millis(2500);
/// How many silent greetings in a row are tolerated before the port is handed
/// back, so a phone stuck in accessory mode does not stay unusable.
const SILENT_LIMIT: u8 = 2;
/// How long a phone that asked for its port back is left alone.
///
/// Long enough for the user to pick "file transfer" on the phone, after which
/// the storage check keeps the daemon away on its own. Without this the port
/// would be taken again before the dialog even appears.
const RELEASE_HOLD: Duration = Duration::from_secs(60);
/// How many times a desktop that laid the monitor out in the wrong shape is
/// asked back before the session stops asking and simply follows it.
const REASK_LIMIT: u8 = 3;
/// Real desktop frames a fresh encoder must be fed before its first IDR can
/// leave. An access unit is only emitted once its successor arrives, and a
/// still desktop never produces that successor on its own.
const FRESH_FRAMES: u8 = 2;
/// How long a turn waits for the desktop's mode event before the stream is
/// rebuilt around the shape the output still wears. A desktop that never
/// answers used to leave the phone holding a stretched frame and ignoring
/// every later turn, because its decoder was never given a new configuration.
const TURN_REBIND_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the desktop may send no picture at all before the session makes
/// sure it still holds the virtual output in its layout.
///
/// A desktop that is merely still sends no dirty rectangles for seconds at a
/// time, so this only decides when a look is harmless, not when something is
/// wrong; the layout itself is the answer.
const QUIET_LIMIT: Duration = Duration::from_secs(3);
/// How often a quiet screen reads the desktop's layout.
const REVIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Frame rate the compatibility stream is held to, whatever rate the desktop
/// ends up configuring the virtual display at.
const COMPAT_FPS: u32 = 30;
/// How many rebuilds the desktop may force in a row before the phone is told.
///
/// A display service that keeps taking the virtual output out of its layout
/// must not be able to keep the phone in a loop that tears the display down and
/// builds it again; after this many attempts the reason is reported instead.
const REVIVE_LIMIT: u8 = 3;
/// How long a drop still counts as part of the same run of drops.
///
/// A screen that streamed for longer than this starts over, so a single drop
/// after hours of use is never held against the next one.
const REVIVE_WINDOW: Duration = Duration::from_secs(60);
/// How long a rebuild waits before it starts, on top of the per-attempt share.
///
/// The display service is usually still finishing the re-layout that dropped
/// the output; a short pause keeps the rebuild from racing it.
const REVIVE_BACKOFF: Duration = Duration::from_millis(300);

/// Serves phones forever, reconnecting whenever one is plugged in.
pub(crate) fn run(sampler: &mut StatsSampler) -> ! {
    let mut last_problem: Option<String> = None;
    let mut held_until: Option<Instant> = None;
    let mut silent_attempts = 0_u8;
    loop {
        // The settings window shows live numbers, so keep them fresh even while
        // there is no phone to send them to.
        requests::beat();
        publish_idle_metrics(sampler);
        let config = androscreen_core::control::Config::load();
        let forced = requests::take_force();
        if forced {
            held_until = None;
        }
        if held_until.is_some_and(|until| Instant::now() >= until) {
            held_until = None;
        }
        let policy = if forced {
            aoa::Takeover::Forced
        } else if held_until.is_some() || !config.auto_takeover {
            aoa::Takeover::OnlyAttached
        } else {
            aoa::Takeover::Automatic
        };
        let found = aoa::connect(policy);
        if found.is_err() {
            // Nothing holds the port, so a release asked for right now has
            // nothing to release. Kept, it would fire the moment the next phone
            // answered — a reset and a minute of silence, which looks exactly
            // like a cable that does nothing at all.
            requests::release_done();
        }
        match found {
            Ok(mut link) => {
                last_problem = None;
                log::info!(
                    "phone link ready: bus {} address {} ({} -> {})",
                    link.info().bus,
                    link.info().address,
                    endpoint(link.info().endpoint_in),
                    endpoint(link.info().endpoint_out)
                );
                requests::update(|state| {
                    state.phone = true;
                    state.streaming = false;
                    state.note = String::new();
                });
                let info = link.info();
                let outcome = serve(&mut link, sampler);
                // The port can only be reset once nothing holds the accessory.
                drop(link);
                requests::update(|state| {
                    state.phone = false;
                    state.streaming = false;
                    state.screen = String::new();
                });
                if outcome.is_ok() {
                    silent_attempts = 0;
                }
                match outcome {
                    Ok(Ending::Closed) => log::info!("phone session ended"),
                    Ok(Ending::Released) => {
                        requests::release_done();
                        hand_back(info);
                        held_until = Some(Instant::now() + RELEASE_HOLD);
                    }
                    Err(error) if is_disconnect(&error) => log::info!("phone disconnected"),
                    Err(AoaError::HelloTimeout { .. }) => {
                        silent_attempts += 1;
                        if silent_attempts >= SILENT_LIMIT {
                            log::warn!(
                                "the phone is in accessory mode but its app never answered; \
                                 handing the port back so it can be used again"
                            );
                            hand_back(info);
                            held_until = Some(Instant::now() + RELEASE_HOLD);
                            silent_attempts = 0;
                        } else {
                            log::warn!(
                                "the phone did not start the AndroScreen app; \
                                 is it installed on the phone?"
                            );
                        }
                    }
                    Err(AoaError::Protocol(error)) => log::warn!("phone sent bad data: {error}"),
                    Err(error) => log::warn!("phone session failed: {error}"),
                }
            }
            Err(AoaError::NoDevice) => log::trace!("no phone attached"),
            Err(error @ (AoaError::SharingStorage { .. } | AoaError::TakeoverDisabled)) => {
                let text = error.to_string();
                if last_problem.as_deref() != Some(text.as_str()) {
                    log::info!("{text}");
                    requests::update(|state| state.note.clone_from(&text));
                    last_problem = Some(text);
                }
            }
            Err(error) => {
                let text = error.to_string();
                if last_problem.as_deref() != Some(text.as_str()) {
                    log::warn!("{text}");
                    last_problem = Some(text);
                }
            }
        }
        wait_for_next_attempt(RECONNECT_DELAY);
    }
}

/// Pauses between connection attempts, cutting the pause short when the settings
/// window asks for a screen.
///
/// The phone is not always ready the moment the port is, so the loop has to wait
/// and try again. That wait must not also become the answer to a click: the
/// person pressing the button is the one thing that makes the next attempt worth
/// starting now.
fn wait_for_next_attempt(budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        if requests::takeover_pending() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining.min(ATTEMPT_SLICE));
    }
}

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The link closed and the phone is still in accessory mode.
    Closed,
    /// The phone asked for its USB port back.
    Released,
}

/// The shape the phone asked to be shown, as a display mode.
const fn requested_mode(request: ScreenRequest) -> DisplayMode {
    DisplayMode {
        width: request.width,
        height: request.height,
        refresh_hz: request.refresh_hz,
    }
}

/// Leaves accessory mode so the phone can offer storage again.
///
/// The port reset is the only way out: accessory mode has no "stop" request.
/// The phone decides what it comes back as, so the result is only reported.
fn hand_back(info: aoa::LinkInfo) {
    log::info!("the phone asked for its port back; resetting the USB port");
    match aoa::reset_port(info.bus, info.address) {
        Ok(()) => log::info!("port reset; the phone re-enumerates with its own functions"),
        Err(error) => log::warn!("{error}; replug the cable to get file transfer back"),
    }
    requests::update(|state| "USB 已释放，手机可改用文件传输".clone_into(&mut state.note));
    // The phone needs a moment to disappear; reconnecting into the half-reset
    // device would only fail the handshake.
    std::thread::sleep(REENUMERATE_PAUSE);
}

fn endpoint(address: u8) -> String {
    format!("0x{address:02x}")
}

fn is_disconnect(error: &AoaError) -> bool {
    matches!(error, AoaError::Disconnected)
}

fn serve(link: &mut AoaLink, sampler: &mut StatsSampler) -> Result<Ending, AoaError> {
    let Some(hello) = aoa::wait_for_hello(link, HELLO_TIMEOUT, requests::release_requested)? else {
        log::info!("the settings window asked for the port back while the app was silent");
        return Ok(Ending::Released);
    };
    log::info!(
        "phone app {} speaks protocol {}; capabilities {:?}",
        hello.app_version,
        hello.protocol,
        hello.capabilities
    );
    send_welcome(link)?;
    idle(link, sampler)
}

/// Announces the host capabilities. The phone repeats its hello until this
/// arrives, so a welcome lost during an app restart is replaced on retry.
fn send_welcome(link: &mut AoaLink) -> Result<(), AoaError> {
    link.send_control(&Control::Welcome {
        protocol: PROTOCOL_VERSION,
        host_version: env!("CARGO_PKG_VERSION").to_owned(),
        audio: true,
        max_screen: Size {
            width: ScreenRequest::MAX_DIMENSION,
            height: 2160,
        },
    })?;
    log::debug!("welcome sent");
    Ok(())
}

/// Keeps the dashboard fed until the phone asks for a screen.
fn idle(link: &mut AoaLink, sampler: &mut StatsSampler) -> Result<Ending, AoaError> {
    let mut next_stats = Instant::now();
    loop {
        requests::beat();
        if requests::release_requested() {
            log::info!("the settings window asked for the port back");
            return Ok(Ending::Released);
        }
        if Instant::now() >= next_stats {
            send_stats(link, sampler)?;
            next_stats = Instant::now() + STATS_INTERVAL;
        }

        let wait = next_stats
            .saturating_duration_since(Instant::now())
            .min(STATS_INTERVAL);
        let Some(frame) = link.poll(wait)? else {
            continue;
        };
        let ending = handle_idle(link, sampler, &frame, &mut next_stats)?;
        if ending == Ending::Released {
            return Ok(ending);
        }
        while let Some(frame) = link.buffered()? {
            if handle_idle(link, sampler, &frame, &mut next_stats)? == Ending::Released {
                return Ok(Ending::Released);
            }
        }
    }
}

fn handle_idle(
    link: &mut AoaLink,
    sampler: &mut StatsSampler,
    frame: &Frame,
    next_stats: &mut Instant,
) -> Result<Ending, AoaError> {
    match frame.kind() {
        Kind::Ping => link
            .send_lossy(&Frame::pong(Session::Known))
            .map(|()| Ending::Closed),
        Kind::Control => match frame.as_control() {
            Ok(Control::StartScreen { request }) => {
                log::debug!("phone requested {request:?}");
                let ending = stream_screen(link, sampler, request)?;
                *next_stats = Instant::now() + STATS_INTERVAL;
                Ok(ending)
            }
            Ok(Control::Hello { .. }) => send_welcome(link).map(|()| Ending::Closed),
            Ok(Control::StopScreen) => Ok(Ending::Closed),
            Ok(Control::Release { reason }) => {
                log::info!("the phone released the link: {reason}");
                Ok(Ending::Released)
            }
            Ok(other) => {
                log::debug!("control received while idle: {other:?}");
                Ok(Ending::Closed)
            }
            Err(_) => {
                log::debug!("ignoring a malformed control message");
                Ok(Ending::Closed)
            }
        },
        Kind::Touch => Ok(Ending::Closed),
        other => {
            log::debug!("ignoring {other:?} frame while idle");
            Ok(Ending::Closed)
        }
    }
}

/// Counts the rebuilds the desktop forced, so a run of them can be told apart
/// from a single drop after a long healthy session.
#[derive(Default)]
struct DropStreak {
    count: u8,
    last: Option<Instant>,
}

impl DropStreak {
    /// Records a drop at `now` and returns how many are in this run.
    fn note(&mut self, now: Instant) -> u8 {
        self.count = if self
            .last
            .is_some_and(|at| now.saturating_duration_since(at) < REVIVE_WINDOW)
        {
            self.count.saturating_add(1)
        } else {
            1
        };
        self.last = Some(now);
        self.count
    }
}

fn stream_screen(
    link: &mut AoaLink,
    sampler: &mut StatsSampler,
    request: ScreenRequest,
) -> Result<Ending, AoaError> {
    let mut request = request;
    let mut rebuild = Rebuild::Fresh;
    let mut drops = DropStreak::default();
    loop {
        if let Err(error) = request.validate() {
            return report(link, "bad_request", &error.to_string()).map(|()| Ending::Closed);
        }
        let Some(mut screen) = Screen::start(link, request, rebuild)? else {
            return Ok(Ending::Closed);
        };
        match screen.run(link, sampler) {
            Ok(()) | Err(ScreenError::Stopped) => {
                return link
                    .send_control(&Control::ScreenStopped {
                        reason: String::new(),
                    })
                    .map(|()| Ending::Closed);
            }
            Err(ScreenError::Release(reason)) => {
                log::info!("the phone released the link mid-stream: {reason}");
                let _ = link.send_control(&Control::ScreenStopped { reason });
                return Ok(Ending::Released);
            }
            // The phone turned into a shape this output cannot simply switch
            // to, so the display is rebuilt around it. Its session is intact,
            // so the next iteration delivers the new shape without saying so.
            Err(ScreenError::Reconfigure(next)) => {
                log::info!(
                    "the phone asked for {}x{}; rebuilding the virtual display",
                    next.width,
                    next.height
                );
                rebuild = Rebuild::Turned;
                request = next;
            }
            // The desktop took the output out of its layout. Rebuilding is the
            // only way back, but a display service that keeps doing it must not
            // keep the phone rebuilding forever.
            Err(ScreenError::Dropped(next)) => {
                let attempts = drops.note(Instant::now());
                if attempts > REVIVE_LIMIT {
                    log::warn!(
                        "the desktop dropped the virtual display {attempts} times in a row; \
                         giving up"
                    );
                    return report(
                        link,
                        "display_dropped",
                        "显示服务反复移除副屏，已停止自动重建；\
                         请在系统显示设置中确认 AndroScreen 未被关闭",
                    )
                    .map(|()| Ending::Closed);
                }
                log::info!(
                    "the desktop dropped the virtual display; \
                     rebuilding it ({attempts}/{REVIVE_LIMIT})"
                );
                std::thread::sleep(REVIVE_BACKOFF * u32::from(attempts));
                rebuild = Rebuild::Turned;
                request = next;
            }
            Err(ScreenError::Link(error)) => return Err(error),
            Err(ScreenError::Local(message)) => {
                log::warn!("screen stopped: {message}");
                return report(link, "screen_failed", &message).map(|()| Ending::Closed);
            }
        }
    }
}

fn report(link: &mut AoaLink, code: &str, message: &str) -> Result<(), AoaError> {
    link.send_control(&Control::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    })?;
    link.send_control(&Control::ScreenStopped {
        reason: message.to_owned(),
    })
}

fn send_stats(link: &mut AoaLink, sampler: &mut StatsSampler) -> Result<(), AoaError> {
    match sampler.sample() {
        Ok(stats) => {
            publish_metrics(&stats);
            link.send_control_lossy(&Control::Stats { stats })
        }
        Err(error) => {
            log::debug!("host statistics unavailable: {error}");
            Ok(())
        }
    }
}

/// Copies the numbers the settings window shows out of a fresh sample.
///
/// The window has no way of reading `/proc` on its own schedule without
/// duplicating the sampler, and it only needs the last second's worth.
fn publish_metrics(stats: &SystemStats) {
    let temps = stats
        .temps
        .iter()
        .map(|temp| SensorRead {
            label: temp.label.clone(),
            celsius: temp.celsius(),
        })
        .collect();
    requests::update(|state| {
        state.cpu_percent = Some(stats.cpu_percent);
        state.mem_percent = Some(stats.mem_used_percent());
        state.temps = temps;
    });
}

/// Samples once and publishes, for the stretches with no phone to talk to.
fn publish_idle_metrics(sampler: &mut StatsSampler) {
    match sampler.sample() {
        Ok(stats) => publish_metrics(&stats),
        Err(error) => log::debug!("host statistics unavailable: {error}"),
    }
}

/// Lets the desktop finish forgetting the output before it is handed a new EDID.
///
/// A desktop only reads the EDID of a connector it has finished forgetting, so
/// this wait comes before the connector lights up again: without it a display
/// torn down and rebuilt in the same breath keeps the shape it wore before,
/// which is what a phone turning into looks like. `await_removal` costs nothing
/// when the desktop already let the output go, and a compositor that never says
/// when it is done gets a fixed pause instead.
///
/// The desktop also rewrites every monitor's brightness the moment the output
/// list changes, and the panel is brightest for the fraction of a second the
/// rewrite takes. Watching from before the connector lights up is what keeps
/// that fraction from being seen.
///
/// Returns whether a compositor rather than the X server owns this output's
/// layout, which is the same answer for the whole session that follows.
fn settle_before_connect(output: &str, rebuild: Rebuild) -> bool {
    let wayland = crate::wlr::owns_layout(output);
    if !wayland {
        crate::x11::await_removal(output);
    } else if matches!(rebuild, Rebuild::Turned) {
        std::thread::sleep(REBUILD_PAUSE);
    }
    if let Some(saved) = crate::brightness::levels() {
        crate::brightness::settle(&saved);
    }
    crate::brightness::keep_color_temperature();
    wayland
}

/// How long the encoder thread waits for a frame before forwarding NALs that
/// left late. Short enough to be invisible, long enough not to spin.
const FEED_IDLE: Duration = Duration::from_millis(1);
/// How long an encoder thread is given to leave before ffmpeg is ended under it.
///
/// Ending a screen must never be able to hold the daemon: the whole session
/// loop sits on this join, and a worker that cannot finish would leave the
/// service deaf to a plugged-in phone and to every button of its window until
/// someone restarted it.
const FEEDER_GRACE: Duration = Duration::from_millis(400);
/// How finely the wait above is measured.
const FEEDER_POLL: Duration = Duration::from_millis(5);

/// Allocates the buffers a screen grabs into and feeds from.
///
/// Three are needed to keep the pipeline one picture deep: one is being written
/// into ffmpeg, one waits to be handed over, and one is being brought up to date
/// by the session. With only two, the copy would have to wait for the encoder
/// thread to finish, putting it back on the critical path.
///
/// The grabbed picture stays BGRA, but what travels to ffmpeg is NV12. The
/// conversion runs on the encoder thread, so it overlaps the next grab instead
/// of sitting in the session loop; each pair therefore carries its own BGRA
/// copy, because an incremental conversion only understands the rectangles of
/// the picture the pair before it held.
fn screen_buffers(mode: Mode) -> Result<(FrameBuffer, Vec<Pane>), EvdiError> {
    let grab = FrameBuffer::new(mode)?;
    let mut free = Vec::with_capacity(2);
    for _ in 0..2 {
        free.push(Pane {
            mirror: FrameBuffer::new(mode)?,
            buffer: Nv12Frame::new(mode)?,
            synced: false,
        });
    }
    Ok((grab, free))
}

/// A picture on its way to the encoder.
///
/// The two buffers travel together because the conversion is incremental: the
/// BGRA mirror and the NV12 picture only learn about the rectangles of their
/// own frame, so they must advance in step. Having the mirror also lets the
/// grab happen while the encoder thread is still converting the one before.
///
/// `synced` reports whether both have ever held a whole picture. A fresh pair
/// is zeroed and a grab only refreshes what changed, so its first frame has to
/// be copied and converted whole.
struct Pane {
    /// BGRA copy of the grabbed desktop, refreshed with the same rectangles.
    mirror: FrameBuffer,
    /// The NV12 picture ffmpeg reads.
    buffer: Nv12Frame,
    /// Whether both halves hold a whole picture.
    synced: bool,
}

/// A pane and the rectangles that brought its mirror up to date.
struct Feed {
    pane: Pane,
    /// The rectangles the mirror was just refreshed with; empty for a pane that
    /// still has to be converted whole.
    rects: Vec<ClipRect>,
    /// When the grab that produced this picture finished. It travels with the
    /// picture so the session can report how long the desktop change took to
    /// come back as an access unit, which is the part of the end-to-end latency
    /// this side owns.
    captured: Instant,
}

/// What became of a picture offered to the encoder thread.
enum Handed {
    /// The thread took it.
    Sent,
    /// The thread was still holding the one before it.
    Full(Feed),
    /// The thread is gone.
    Gone,
}

/// One running virtual display and everything attached to it.
struct Screen {
    card: EvdiCard,
    /// The buffer grabs land in; it always holds the whole desktop picture.
    buffer: FrameBuffer,
    /// Buffers the encoder thread is not using, ready to be brought up to date
    /// with the next grab.
    free: Vec<Pane>,
    /// A picture already brought up to date, waiting for the encoder thread to
    /// take the one before it.
    ready: Option<Feed>,
    /// The rectangles the last grab refreshed, waiting to be copied into one of
    /// the free buffers. Keeping them here lets the grab happen while the
    /// encoder thread is still busy with the picture before it.
    grabbed: Option<Vec<ClipRect>>,
    /// When the grab that filled `grabbed` finished.
    grabbed_at: Option<Instant>,
    /// Pictures already handed to the encoder thread, oldest first. An access
    /// unit comes back in that same order, so this pairs one grab time with one
    /// NAL and nothing else.
    pipe: VecDeque<Instant>,
    feeder: Feeder,
    touch: Option<TouchDevice>,
    audio: Option<AudioCapture>,
    mode: Mode,
    /// The picture the phone decodes: its own screen, inside the larger
    /// desktop-sized display the output was fitted to.
    coded: Size,
    fps: u32,
    /// The shape the phone last asked for, which is not the shape it is shown:
    /// a repeat request has to read as a repeat and not as a turn.
    phone_display: DisplayMode,
    /// The shape this display advertises, along with the same shape turned.
    virtual_display: DisplayMode,
    /// The desktop scale the size was chosen against, so a turn keeps it.
    scale: Option<f64>,
    /// Whether the phone forbade enlarging the display for that scale.
    exact: bool,
    /// Whether the phone asked for a compatibility stream.
    ///
    /// Unlike [`Self::exact`], this is the request itself rather than the
    /// result: a size the desktop could not wear exactly still asked for the
    /// stream's lower rate, and a rebuild has to keep asking for it.
    compat: bool,
    /// The rate this display may stream at. A compatibility stream is held
    /// below the rate its mode runs at.
    fps_cap: u32,
    /// Whether the compositor, rather than the display service, owns this
    /// screen's input mapping.
    wayland: bool,
    /// Reads of the compositor's layout still owed to the touch panel.
    layout_probes: u8,
    /// Asks still owed to a desktop that laid the monitor out in the other shape.
    reasks: u8,
    /// Real frames still owed to a fresh encoder. Without them the phone keeps
    /// showing the last picture of the previous shape, stretched, until the
    /// user happens to move a window on the desktop.
    owed_frames: u8,
    /// When a turn still waiting for the desktop's mode event has to be
    /// given up on and the stream rebuilt anyway.
    turn_deadline: Option<Instant>,
    /// What the phone is still owed for the shape just announced.
    pending: Pending,
    /// The pointer the compositor reports, which the phone draws locally.
    cursor: CursorFeed,
    /// Pacing keeps the compositor's paint rate from flooding the encoder:
    /// every grab must be at least this far apart.
    frame_interval: Duration,
    next_frame: Instant,
    /// What the last second of streaming did, for the log and the dashboard.
    stream: StreamStats,
}

/// Runs the encoder on a thread of its own.
///
/// Filling ffmpeg's pipe with a raw desktop frame costs about as much as the
/// grab that produced it, and doing the two in the session loop put the touch
/// panel and every USB write behind that copy: a click could wait a whole
/// encode before it was read. The frame therefore travels to a worker thread,
/// which converts it to NV12 and writes it into ffmpeg while the session keeps
/// grabbing and polling the link, and comes back to the session so the next
/// grab has somewhere to land.
struct Feeder {
    /// Pictures on their way to the encoder; `None` once it is shut down.
    frames: Option<SyncSender<Feed>>,
    /// The pane the worker has finished with, waiting to be refreshed; `None`
    /// once the session has stopped taking them.
    spare: Option<Receiver<Pane>>,
    /// NALs and timing reports on their way back from the worker.
    events: Receiver<FeedEvent>,
    worker: Option<JoinHandle<()>>,
    /// ffmpeg's process id, so a teardown can end it if the worker cannot.
    pid: u32,
    /// SPS/PPS and the backend name, kept here because the encoder itself
    /// lives on the worker thread.
    csd: Vec<u8>,
    backend: Backend,
}

/// What the encoder thread reports back to the session.
enum FeedEvent {
    /// One complete access unit, ready for the phone.
    Nal(Nal),
    /// How long the picture took to reach ffmpeg, split between the conversion
    /// and the write.
    Fed { convert: Duration, feed: Duration },
    /// The encoder stopped; the session cannot go on.
    Failed(String),
}

impl Feeder {
    /// Moves the encoder onto its own thread and starts serving frames.
    fn spawn(encoder: Encoder) -> Self {
        let csd = encoder.csd().to_vec();
        let backend = encoder.backend();
        let pid = encoder.pid();
        let (frames, incoming) = mpsc::sync_channel(1);
        let (spare, returned) = mpsc::sync_channel(1);
        let (events, reports) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut encoder = encoder;
            feed_loop(&mut encoder, &incoming, &spare, &events);
        });
        Self {
            frames: Some(frames),
            spare: Some(returned),
            events: reports,
            worker: Some(worker),
            pid,
            csd,
            backend,
        }
    }

    /// The SPS/PPS pair needed to configure the decoder.
    fn csd(&self) -> &[u8] {
        &self.csd
    }

    /// The backend in use.
    const fn backend(&self) -> Backend {
        self.backend
    }

    /// Offers a picture to the encoder thread without waiting for it.
    ///
    /// Waiting would put the whole session behind the encoder again, and the
    /// thread only ever holds one frame, so the picture that does not fit is
    /// simply kept until it can.
    fn try_hand_off(&self, feed: Feed) -> Handed {
        let Some(sender) = self.frames.as_ref() else {
            return Handed::Gone;
        };
        match sender.try_send(feed) {
            Ok(()) => Handed::Sent,
            Err(TrySendError::Full(feed)) => Handed::Full(feed),
            Err(TrySendError::Disconnected(_)) => Handed::Gone,
        }
    }

    /// Takes back a pane the encoder thread has finished with, if any.
    fn spare(&self) -> Option<Pane> {
        self.spare.as_ref()?.try_recv().ok()
    }

    /// Takes the next thing the encoder thread has to say.
    ///
    /// `Err` means the thread is gone, which only happens after it has already
    /// reported why.
    fn try_event(&self) -> Result<Option<FeedEvent>, ()> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()),
        }
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        // Both directions are closed before the join. A worker that has just
        // finished converting a picture hands the pane back and would otherwise
        // wait for a session that has already stopped taking them — the state a
        // display that lost its cable mid-frame is left in. Closing the
        // receiving end turns that wait into the `Err` that lets it leave.
        self.frames = None;
        self.spare = None;
        let Some(worker) = self.worker.take() else {
            return;
        };
        let deadline = Instant::now() + FEEDER_GRACE;
        while !worker.is_finished() && Instant::now() < deadline {
            thread::sleep(FEEDER_POLL);
        }
        if !worker.is_finished() {
            // A worker stuck in ffmpeg's pipes cannot see the closed channel.
            // Ending the encoder breaks those pipes, and the join returns; the
            // screen is being torn down, so nothing is left to deliver.
            log::warn!("the encoder did not stop within {FEEDER_GRACE:?}; ending ffmpeg");
            if let Ok(pid) = i32::try_from(self.pid) {
                // SAFETY: `kill` only reads the two integers it is given.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        let _ = worker.join();
    }
}

/// Feeds the encoder until the session drops it, reporting what comes back.
fn feed_loop(
    encoder: &mut Encoder,
    frames: &Receiver<Feed>,
    spare: &SyncSender<Pane>,
    events: &Sender<FeedEvent>,
) {
    loop {
        let handoff = match frames.recv_timeout(FEED_IDLE) {
            Ok(feed) => feed,
            Err(RecvTimeoutError::Timeout) => {
                if !forward_nals(encoder, events) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // The conversion lives here rather than in the session so that it runs
        // against the session's next grab instead of in front of it.
        let convert_started = Instant::now();
        let Feed {
            mut pane,
            rects,
            captured: _,
        } = handoff;
        if pane.synced {
            pane.buffer.convert_rects_from(&pane.mirror, &rects);
        } else {
            pane.buffer.convert_all_from(&pane.mirror);
        }
        let converted = convert_started.elapsed();
        let fed_started = Instant::now();
        let fed = encoder.feed(pane.buffer.pixels());
        let elapsed = fed_started.elapsed();
        // The pane goes back first: the session cannot refresh another picture
        // until it has it, and holding it behind a report would spend the gain.
        pane.synced = true;
        if spare.send(pane).is_err() {
            return;
        }
        let _ = events.send(FeedEvent::Fed {
            convert: converted,
            feed: elapsed,
        });
        if let Err(error) = fed {
            let _ = events.send(FeedEvent::Failed(format!(
                "cannot feed the encoder: {error}"
            )));
            return;
        }
        if !forward_nals(encoder, events) {
            return;
        }
    }
}

/// Moves every encoded picture the session can take right now.
///
/// Returns `false` once nobody is listening any more.
fn forward_nals(encoder: &mut Encoder, events: &Sender<FeedEvent>) -> bool {
    loop {
        match encoder.next_nal(Duration::ZERO) {
            Ok(Some(nal)) => {
                if events.send(FeedEvent::Nal(nal)).is_err() {
                    return false;
                }
            }
            Ok(None) => return true,
            Err(error) => {
                let _ = events.send(FeedEvent::Failed(format!("encoder stopped: {error}")));
                return false;
            }
        }
    }
}

/// One second of streaming, turned into the numbers worth looking at.
///
/// The counters live on the link and are cumulative; this keeps the previous
/// snapshot so each report can be a delta instead of a lifetime average.
#[derive(Debug, Default)]
struct StreamStats {
    /// Start of the window being measured, once the first sample exists.
    window: Option<Instant>,
    /// Counters at the end of the previous window.
    last: StreamCounters,
    /// Time spent grabbing and feeding the encoder in this window.
    grab_time: Duration,
    grab_max: Duration,
    feed_time: Duration,
    feed_max: Duration,
    /// Time spent copying the grabbed rectangles into the pane's mirror, and
    /// the slowest one.
    copy_time: Duration,
    copy_max: Duration,
    /// Time the encoder thread spent converting a mirror into NV12, and the
    /// slowest one.
    conv_time: Duration,
    conv_max: Duration,
    /// Time spent in the session loop itself, and the longest single pass.
    loop_time: Duration,
    loop_max: Duration,
    /// How many passes the loop made in this window.
    loop_passes: u32,
    /// Time from the grab that produced a picture to the access unit ready to
    /// be written, its slowest, and how many were timed. This is the whole
    /// latency this side adds, encoder queue included.
    pipe_time: Duration,
    pipe_max: Duration,
    pipe_samples: u32,
    /// Time spent waiting for a phone frame, and the slowest wait.
    poll_time: Duration,
    poll_max: Duration,
    /// How many pictures were grabbed, and how many updates reported no change.
    grabs: u32,
    empty_updates: u32,
}

/// The numbers a [`StreamStats`] report hands to the log and the dashboard.
#[derive(Debug, Default)]
struct StreamRates {
    /// Video frames the phone took per second.
    fps: f64,
    /// Video bits that reached the phone per second, in millions.
    mbps: f64,
    /// Frames skipped, lost, and behind transitions in this window.
    dropped: u64,
    lost: u64,
    behind: u64,
    /// Time spent waiting for a keyframe in this window.
    waited: Duration,
    /// Time spent grabbing the desktop, and the slowest single grab.
    grab_total: Duration,
    grab_max: Duration,
    /// Time spent handing a picture to the encoder, and the slowest one.
    feed_total: Duration,
    feed_max: Duration,
    /// Time spent bringing the pane's mirror up to date, and the slowest one.
    copy_total: Duration,
    copy_max: Duration,
    /// Time spent converting that mirror to NV12, and the slowest one.
    conv_total: Duration,
    conv_max: Duration,
    /// Time spent in the session loop, and the slowest single pass.
    loop_avg: Duration,
    loop_max: Duration,
    /// Passes the loop made in this window.
    loop_passes: u32,
    /// Mean and worst time a picture took from grab to access unit, and how
    /// many pictures were timed.
    pipe_avg: Duration,
    pipe_max: Duration,
    pipe_samples: u32,
    /// Average and worst write in this window.
    write_avg: Duration,
    write_max: Duration,
    /// Time spent waiting for a phone frame, and the slowest wait.
    poll_total: Duration,
    poll_max: Duration,
    /// Pictures grabbed, and updates that reported no change.
    grabs: u32,
    empty_updates: u32,
}

impl StreamStats {
    /// Records one grab of the virtual display.
    fn note_grab(&mut self, elapsed: Duration) {
        self.grab_time = self.grab_time.saturating_add(elapsed);
        self.grab_max = self.grab_max.max(elapsed);
        self.grabs = self.grabs.saturating_add(1);
    }

    /// Records one update request that found nothing new on the desktop.
    fn note_empty(&mut self) {
        self.empty_updates = self.empty_updates.saturating_add(1);
    }

    /// Records one wait for a phone frame.
    fn note_poll(&mut self, elapsed: Duration) {
        self.poll_time = self.poll_time.saturating_add(elapsed);
        self.poll_max = self.poll_max.max(elapsed);
    }

    /// Records one write of a converted picture into ffmpeg.
    fn note_feed(&mut self, elapsed: Duration) {
        self.feed_time = self.feed_time.saturating_add(elapsed);
        self.feed_max = self.feed_max.max(elapsed);
    }

    /// Records one copy that brings the pane's mirror up to date.
    fn note_copy(&mut self, elapsed: Duration) {
        self.copy_time = self.copy_time.saturating_add(elapsed);
        self.copy_max = self.copy_max.max(elapsed);
    }

    /// Records one conversion of a mirrored picture into NV12.
    fn note_conv(&mut self, elapsed: Duration) {
        self.conv_time = self.conv_time.saturating_add(elapsed);
        self.conv_max = self.conv_max.max(elapsed);
    }

    /// Records one pass of the session loop.
    fn note_loop(&mut self, elapsed: Duration) {
        self.loop_time = self.loop_time.saturating_add(elapsed);
        self.loop_max = self.loop_max.max(elapsed);
        self.loop_passes = self.loop_passes.saturating_add(1);
    }

    /// Records one picture's trip from grab to ready access unit.
    fn note_pipe(&mut self, elapsed: Duration) {
        self.pipe_time = self.pipe_time.saturating_add(elapsed);
        self.pipe_max = self.pipe_max.max(elapsed);
        self.pipe_samples = self.pipe_samples.saturating_add(1);
    }

    /// Turns cumulative counters into a report, once per interval.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a dashboard rate is shown rounded; it never steers the stream"
    )]
    fn take(&mut self, now: Instant, counters: StreamCounters) -> Option<StreamRates> {
        let Some(window) = self.window else {
            self.window = Some(now);
            self.last = counters;
            return None;
        };
        let elapsed = now.saturating_duration_since(window);
        if elapsed < STATS_INTERVAL {
            return None;
        }
        let seconds = elapsed.as_secs_f64();
        let videos = counters.videos.saturating_sub(self.last.videos);
        let bytes = counters.video_bytes.saturating_sub(self.last.video_bytes);
        let writes = videos.saturating_add(counters.audios.saturating_sub(self.last.audios));
        let write_time = counters.write_time.saturating_sub(self.last.write_time);
        let rates = StreamRates {
            fps: videos as f64 / seconds,
            mbps: bytes as f64 * 8.0 / seconds / 1_000_000.0,
            dropped: counters.dropped.saturating_sub(self.last.dropped),
            lost: counters.lost.saturating_sub(self.last.lost),
            behind: counters.behind_marks.saturating_sub(self.last.behind_marks),
            waited: counters
                .keyframe_waited
                .saturating_sub(self.last.keyframe_waited),
            grab_total: self.grab_time,
            grab_max: self.grab_max,
            feed_total: self.feed_time,
            feed_max: self.feed_max,
            copy_total: self.copy_time,
            copy_max: self.copy_max,
            conv_total: self.conv_time,
            conv_max: self.conv_max,
            loop_avg: if self.loop_passes == 0 {
                Duration::ZERO
            } else {
                self.loop_time / self.loop_passes
            },
            loop_max: self.loop_max,
            loop_passes: self.loop_passes,
            pipe_avg: if self.pipe_samples == 0 {
                Duration::ZERO
            } else {
                self.pipe_time / self.pipe_samples
            },
            pipe_max: self.pipe_max,
            pipe_samples: self.pipe_samples,
            write_avg: if writes == 0 {
                Duration::ZERO
            } else {
                write_time / u32::try_from(writes).unwrap_or(u32::MAX)
            },
            write_max: counters.write_max,
            poll_total: self.poll_time,
            poll_max: self.poll_max,
            grabs: self.grabs,
            empty_updates: self.empty_updates,
        };
        self.last = counters;
        self.window = Some(now);
        self.grab_time = Duration::ZERO;
        self.grab_max = Duration::ZERO;
        self.feed_time = Duration::ZERO;
        self.feed_max = Duration::ZERO;
        self.copy_time = Duration::ZERO;
        self.copy_max = Duration::ZERO;
        self.conv_time = Duration::ZERO;
        self.conv_max = Duration::ZERO;
        self.loop_time = Duration::ZERO;
        self.loop_max = Duration::ZERO;
        self.loop_passes = 0;
        self.pipe_time = Duration::ZERO;
        self.pipe_max = Duration::ZERO;
        self.pipe_samples = 0;
        self.poll_time = Duration::ZERO;
        self.poll_max = Duration::ZERO;
        self.grabs = 0;
        self.empty_updates = 0;
        Some(rates)
    }
}

/// What a screen still owes the phone after announcing a new shape.
///
/// Attached on the first turn of the run loop rather than in front of the
/// picture: re-creating the touch panel means measuring the whole desktop and
/// then polling the display service, and starting audio capture waits on the
/// sound server. Both were measured at more than half a second together, spent
/// with the phone looking at the shape it had just turned away from.
#[derive(Clone, Copy, Default)]
struct Pending {
    /// The panel has to be re-created for the shape just built.
    touch: bool,
    /// Desktop audio has not been started for this screen yet.
    audio: bool,
}

/// Which screen the display being built replaces.
#[derive(Clone, Copy)]
enum Rebuild {
    /// Nothing was showing; the phone asked for a screen from scratch.
    Fresh,
    /// The phone turned, and the shape it left behind was just taken down.
    Turned,
}

impl Screen {
    /// Creates the virtual display and announces it to the phone.
    ///
    /// Returns `Ok(None)` after reporting a failure the phone can recover
    /// from by asking again with a different request.
    fn start(
        link: &mut AoaLink,
        request: ScreenRequest,
        rebuild: Rebuild,
    ) -> Result<Option<Self>, AoaError> {
        let mut screen = match Self::open_display(request, rebuild) {
            Ok(screen) => screen,
            Err(failure) => {
                log::warn!("cannot create the virtual display: {}", failure.message);
                report(link, failure.code, &failure.message)?;
                return Ok(None);
            }
        };
        // The phone is waiting for a picture, and both attachments this screen
        // still owes talk to the desktop rather than to the cable: the layout
        // round-trip and the audio capture startup each cost a visible fraction
        // of a second. Say "here it comes" first and leave them to the run
        // loop, which reaches their turn only once the announcement's first
        // frames are on the wire.
        screen.announce(link)?;
        screen.pending = Pending {
            touch: true,
            audio: request.audio,
        };
        Ok(Some(screen))
    }

    /// Builds the display pipeline, reporting which step failed.
    ///
    /// One constructor for one screen: the steps are ordered by what the phone
    /// must be told first, and splitting them would only hide that order.
    #[expect(
        clippy::too_many_lines,
        reason = "the order of these steps is what the phone sees; keeping them together is the point"
    )]
    fn open_display(request: ScreenRequest, rebuild: Rebuild) -> Result<Self, StartFailure> {
        // The desktop measures the shortest edge of every monitor to decide how
        // much scale a session can carry, so a phone-shaped display makes it
        // re-scale the whole desktop every time one connects. Giving the output
        // the size the current scale already assumes leaves it nothing to
        // correct.
        let scale = crate::scale::current_global_scale();
        let phone_display = requested_mode(request);
        // An exact request is already the size the phone can decode. It is worn
        // as-is only when the desktop's own scale can carry it: a monitor
        // shorter than that scale makes DDE lower the whole desktop's scale and
        // ask the user to log out, which costs more than the enlargement it
        // avoids. The encoder cuts the picture down to the phone's box either
        // way, so the stream is the requested size in both cases.
        let fitted = crate::scale::fit_to_scale(phone_display, scale);
        let pinned = request.exact && crate::scale::pin_to_request(phone_display, scale);
        let virtual_display = if pinned { phone_display } else { fitted };
        let edid = build_edid(virtual_display, "AndroScreen")
            .map_err(|error| StartFailure::new("bad_request", error))?;

        let card = EvdiCard::open_first()
            .map_err(|error| StartFailure::new("display_unavailable", error))?;
        let area_limit = virtual_display.width.saturating_mul(virtual_display.height);
        let rate_limit = area_limit.saturating_mul(virtual_display.refresh_hz);
        let output = card.name().unwrap_or_else(|| "DVI-I-1".to_owned());
        let wayland = settle_before_connect(&output, rebuild);
        if let Err(error) = card.connect(&edid, area_limit, rate_limit) {
            let _ = card.disconnect();
            return Err(StartFailure::new("display_unavailable", error));
        }
        // Ask for the pointer before the compositor lays the monitor out: with
        // events on, the kernel stops blending it into the picture, and the
        // phone then owns the cursor's latency instead of the stream.
        if let Err(error) = card.enable_cursor_events() {
            log::warn!("the compositor pointer stays in the picture: {error}");
        }

        // Treeland leaves a hot-plugged output dark until a client asks the
        // compositor to lay it out, and an X11 desktop keeps whatever mode the
        // output wore last time it was there. Both need telling. The ask runs
        // on its own so that a busy desktop cannot hold up the phone's first
        // frame.
        if wayland {
            crate::wlr::request(
                output,
                virtual_display.width,
                virtual_display.height,
                virtual_display.refresh_hz,
            );
        } else {
            crate::x11::request(
                output,
                virtual_display.width,
                virtual_display.height,
                crate::x11::Ask::Arrived,
            );
        }
        let (mode, worn) = match wait_for_mode(
            &card,
            if wayland {
                WAYLAND_MODE_TIMEOUT
            } else {
                MODE_TIMEOUT
            },
            virtual_display,
        ) {
            Ok(answer) => answer,
            Err(error) => {
                let _ = card.disconnect();
                return Err(StartFailure::new("display_unavailable", error));
            }
        };
        log::info!(
            "compositor configured the virtual display at {}x{}@{} {:?}",
            mode.width,
            mode.height,
            mode.refresh_hz,
            mode.format
        );
        if !worn {
            // A desktop that owns the layout puts a monitor it has just seen
            // arrive in the shape its EDID prefers, and that is not always the
            // shape the phone is wearing. The picture still moves, and the ask
            // is on its way to being repeated; a session that insisted on being
            // answered first would leave the phone with no screen at all.
            log::debug!(
                "the desktop laid the monitor out at {}x{} rather than the {}x{} asked for",
                mode.width,
                mode.height,
                virtual_display.width,
                virtual_display.height
            );
        }

        let (buffer, free) = screen_buffers(mode).map_err(|error| {
            let _ = card.disconnect();
            StartFailure::new("display_unavailable", error)
        })?;
        let fps_cap = if request.exact {
            COMPAT_FPS
        } else {
            ScreenRequest::MAX_REFRESH_HZ
        };
        let fps = mode
            .refresh_hz
            .clamp(ScreenRequest::MIN_REFRESH_HZ, ScreenRequest::MAX_REFRESH_HZ)
            .min(fps_cap);
        // The phone's decoder is sized for its own panel, and a display enlarged
        // for the desktop's scale can be taller than that panel tolerates, so
        // the picture shrinks back to what the phone asked for on its way into
        // the encoder.
        let coded = coded_size(mode, phone_display.width, phone_display.height);
        let encoder = Encoder::spawn(mode, coded, fps).map_err(|error| {
            let _ = card.disconnect();
            StartFailure::new("encoder_unavailable", error)
        })?;
        let feeder = Feeder::spawn(encoder);
        let frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        Ok(Self {
            card,
            buffer,
            free,
            ready: None,
            grabbed: None,
            grabbed_at: None,
            pipe: VecDeque::new(),
            feeder,
            touch: None,
            audio: None,
            mode,
            coded,
            fps,
            phone_display,
            virtual_display,
            scale,
            exact: pinned,
            compat: request.exact,
            fps_cap,
            wayland,
            layout_probes: if wayland { LAYOUT_PROBES } else { 0 },
            reasks: if worn { 0 } else { REASK_LIMIT },
            owed_frames: FRESH_FRAMES,
            turn_deadline: None,
            pending: Pending::default(),
            cursor: CursorFeed::new(mode.width, mode.height),
            frame_interval,
            next_frame: Instant::now(),
            stream: StreamStats::default(),
        })
    }

    /// Attaches the input and sound this screen still owes the phone.
    ///
    /// Both are deferred to here on purpose: the panel is re-created for the new
    /// shape, and both the desktop measurement and the display service's answer
    /// are slow enough to be seen. The phone gets its picture first.
    fn attach_pending(&mut self, link: &mut AoaLink) -> Result<(), ScreenError> {
        // The announcement is only a message until the encoder has been fed the
        // frames that release its handshake IDR. Attaching in front of those
        // would put the same half second back in front of the picture, so this
        // waits until the debt is paid: by then the first frame is on the wire.
        if self.owed_frames > 0 {
            return Ok(());
        }
        if self.pending.touch {
            self.pending.touch = false;
            self.attach_touch(link).map_err(ScreenError::Link)?;
        }
        if self.pending.audio {
            self.pending.audio = false;
            self.attach_audio(link).map_err(ScreenError::Link)?;
        }
        Ok(())
    }

    /// Adds touch injection when `/dev/uinput` is available.
    fn attach_touch(&mut self, link: &mut AoaLink) -> Result<(), AoaError> {
        let desktop = if self.wayland {
            crate::wlr::layout()
        } else {
            None
        };
        match TouchDevice::create(self.mode.width, self.mode.height, desktop) {
            Ok(touch) => {
                let mapped_to_desktop = touch.reports_global();
                self.touch = Some(touch);
                if !mapped_to_desktop {
                    self.ask_the_desktop_to_map_touch();
                }
            }
            Err(error) => {
                self.layout_probes = 0;
                log::warn!("touch injection unavailable: {error}");
                link.send_control(&Control::Error {
                    code: "touch_unavailable".to_owned(),
                    message: error.to_string(),
                })?;
            }
        }
        Ok(())
    }

    /// Asks a desktop with a display service to map the panel to this output.
    ///
    /// A Wayland session has no such service, which is why the panel measures
    /// the whole desktop instead whenever the layout is already known.
    fn ask_the_desktop_to_map_touch(&self) {
        if let Some(output) = self.card.name() {
            crate::touchmap::associate(&output);
        }
    }

    /// Names the shape this output should wear to the desktop that owns it.
    ///
    /// A desktop is not told once and left alone: it lays a monitor it has just
    /// seen arrive in the shape its EDID puts first, which with both shapes of a
    /// phone in one blob is not always the shape the phone is wearing.
    /// Asks the desktop for the other shape the EDID already advertises.
    ///
    /// A turn must not go through `ApplyChanges`: the display service rewrites
    /// every monitor's brightness to its default a moment after that call, and
    /// holding the ask until the whole desktop settles makes the phone wait
    /// seconds for a picture. Arm the brightness watcher before the ask, as we
    /// do when a connector first appears, and touch only this one output.
    fn ask_the_desktop_to_turn(&self, display: DisplayMode) {
        let Some(output) = self.card.name() else {
            return;
        };
        if self.wayland {
            crate::wlr::request(output, display.width, display.height, display.refresh_hz);
        } else {
            if let Some(saved) = crate::brightness::levels() {
                crate::brightness::settle(&saved);
            }
            crate::brightness::keep_color_temperature();
            crate::x11::request(
                output,
                display.width,
                display.height,
                crate::x11::Ask::Turned,
            );
        }
    }

    /// Whether the desktop has sent no picture for long enough that reading its
    /// layout cannot cost a frame.
    ///
    /// A still desktop produces no dirty rectangles for seconds at a time, so
    /// this is only a guard against asking while the phone is fed; the layout
    /// itself decides whether anything is wrong.
    fn quiet(&self) -> bool {
        !self.wayland && self.grabbed_at.is_none_or(|at| at.elapsed() >= QUIET_LIMIT)
    }

    /// Whether the desktop has taken the virtual output out of its layout.
    ///
    /// The display service lays out the monitors it knows a moment after a
    /// session starts, and a monitor it has no saved place for leaves the
    /// layout while the connector stays attached. The card reports nothing for
    /// that, so a screen that only waited for frames stayed dark with the phone
    /// still connected.
    fn display_dropped(&self) -> bool {
        self.card
            .name()
            .is_some_and(|output| crate::x11::dropped(&output))
    }

    /// The request that reproduces this screen, for a rebuild the desktop's
    /// listing left no other way out of.
    fn repeat_request(&self) -> ScreenRequest {
        ScreenRequest {
            width: self.phone_display.width,
            height: self.phone_display.height,
            refresh_hz: self.phone_display.refresh_hz,
            audio: self.audio.is_some() || self.pending.audio,
            exact: self.compat,
        }
    }

    /// Rebuilds the panel once the compositor has said where the output is.
    ///
    /// The layout arrives some moments after the connector lights up, and the
    /// session cannot wait for it in front of the phone, so a panel that was
    /// built measuring only the virtual output is replaced once the answer is
    /// in. The ask stops after a handful of rounds: a compositor that will not
    /// say leaves the panel as it is, which is what it would have been anyway.
    fn probe_layout(&mut self) {
        if self.layout_probes == 0 {
            return;
        }
        self.layout_probes -= 1;
        let Some(touch) = self.touch.take() else {
            return;
        };
        if touch.reports_global() {
            self.layout_probes = 0;
            self.touch = Some(touch);
            return;
        }
        let Some(desktop) = crate::wlr::layout() else {
            self.touch = Some(touch);
            return;
        };
        drop(touch);
        match TouchDevice::create(self.mode.width, self.mode.height, Some(desktop)) {
            Ok(reach) => {
                self.layout_probes = 0;
                self.touch = Some(reach);
                log::info!("the touch panel now reaches the virtual output");
            }
            Err(error) => {
                self.layout_probes = 0;
                log::warn!("cannot re-map touch onto the desktop layout: {error}");
            }
        }
    }

    /// Starts desktop audio forwarding when a monitor is available.
    ///
    /// The phone has been told about the picture before this runs, so the audio
    /// configuration travels as its own message instead of holding video back.
    fn attach_audio(&mut self, link: &mut AoaLink) -> Result<(), AoaError> {
        match AudioCapture::spawn() {
            Ok(audio) => {
                self.audio = Some(audio);
                link.send_control(&Control::AudioConfig {
                    sample_rate: SAMPLE_RATE,
                    channels: CHANNELS,
                    format: AudioFormat::S16Le,
                })?;
            }
            Err(error) => {
                log::warn!("audio forwarding unavailable: {error}");
                link.send_control(&Control::Error {
                    code: "audio_unavailable".to_owned(),
                    message: error.to_string(),
                })?;
            }
        }
        Ok(())
    }

    /// Tells the phone the stream is about to start and how it is encoded.
    fn announce(&mut self, link: &mut AoaLink) -> Result<(), AoaError> {
        // Media writes get deadlines derived from the rate the phone was told
        // about, so a frame cannot hold the loop for an unbounded time.
        link.set_media_pacing(self.frame_interval);
        link.send_control(&Control::ScreenStarted {
            width: self.mode.width,
            height: self.mode.height,
            encoder: self.feeder.backend().to_string(),
        })?;
        link.send_control(&Control::VideoConfig {
            codec: VideoCodec::H264,
            width: self.coded.width,
            height: self.coded.height,
            fps: self.fps,
            csd: self.feeder.csd().to_vec(),
        })?;
        requests::update(|state| {
            state.streaming = true;
            state.screen = format!("{}x{}", self.mode.width, self.mode.height);
        });
        Ok(())
    }

    /// Streams until the phone stops the screen or the link fails.
    fn run(&mut self, link: &mut AoaLink, sampler: &mut StatsSampler) -> Result<(), ScreenError> {
        let mut next_stats = Instant::now() + STATS_INTERVAL;
        let mut next_revive = Instant::now() + REVIVE_INTERVAL;
        self.forward_video(link)?;

        loop {
            requests::beat();
            let pass_started = Instant::now();
            if requests::release_requested() {
                log::info!("the settings window asked for the port back mid-stream");
                return Err(ScreenError::Release(
                    "the desktop handed the port back".to_owned(),
                ));
            }
            self.pump_display(link)?;
            // The pointer goes out before this pass's picture: it is a few
            // bytes, and queueing it behind a video frame is what the phone
            // would feel as a lagging cursor.
            self.cursor.pump(link).map_err(ScreenError::Link)?;
            self.forward_video(link)?;
            // Touch and sound join once the picture is on the wire. Both ask the
            // desktop rather than the phone, and it is the phone that is waiting.
            self.attach_pending(link)?;
            self.pump_audio(link)?;
            self.report_stream(link);

            if Instant::now() >= next_stats {
                // A dashboard sample is the least valuable frame on the wire,
                // and unlike video it has to arrive whole or not at all.
                if link.is_behind() {
                    log::debug!("skipping a statistics sample while the phone catches up");
                } else {
                    send_stats(link, sampler).map_err(ScreenError::Link)?;
                }
                self.probe_layout();
                next_stats = Instant::now() + STATS_INTERVAL;
            }
            // A desktop that had nothing to send for a while is when a monitor
            // it took out of its layout goes unnoticed: no frame arrives, and
            // nothing on the card ever says why. Looking costs one listing, and
            // only a screen with no picture pays it. The display is rebuilt
            // around the phone rather than asked to take the output back in
            // place: a mode set on a head the desktop has already given up on
            // blocks the desktop inside the commit, which froze it for ten
            // seconds at a time and never lit the phone.
            if Instant::now() >= next_revive {
                next_revive = Instant::now() + REVIVE_INTERVAL;
                // A turn that has not landed yet is already being followed: its
                // mode set can leave the listing without a place for a moment,
                // and rebuilding now would race the answer that is on its way.
                if self.turn_deadline.is_none() && self.quiet() && self.display_dropped() {
                    log::warn!(
                        "the desktop dropped the virtual display; rebuilding it around the phone"
                    );
                    return Err(ScreenError::Dropped(self.repeat_request()));
                }
            }

            // Wake for whichever comes first: a word from the phone or the
            // next desktop frame. Sleeping past the frame slot by even a few
            // milliseconds pushes every grab that follows off the rhythm the
            // compositor set, and the phone then sees frames arrive in bursts.
            // With nothing pending the slice still bounds the wait, so a still
            // desktop does not spin here.
            let until_frame = self
                .next_frame
                .saturating_duration_since(Instant::now())
                .min(POLL_SLICE);
            let wait = next_stats
                .saturating_duration_since(Instant::now())
                .min(until_frame);
            let poll_started = Instant::now();
            let polled = link.poll(wait);
            self.stream.note_poll(poll_started.elapsed());
            match polled {
                Ok(Some(frame)) => {
                    self.handle_frame(link, &frame)?;
                    while let Some(frame) = link.buffered().map_err(ScreenError::Link)? {
                        self.handle_frame(link, &frame)?;
                    }
                }
                Ok(None) => {}
                Err(error) => return Err(ScreenError::Link(error)),
            }
            self.stream.note_loop(pass_started.elapsed());
        }
    }

    fn dispatch(&mut self, link: &mut AoaLink, frame: &Frame) -> Result<(), ScreenError> {
        match frame.kind() {
            Kind::Touch => {
                self.handle_touch(frame.payload());
                Ok(())
            }
            Kind::Ping => link
                .send_lossy(&Frame::pong(Session::Known))
                .map_err(ScreenError::Link),
            Kind::Control => {
                let Ok(control) = frame.as_control() else {
                    log::debug!("ignoring a malformed control message");
                    return Ok(());
                };
                match control {
                    Control::StopScreen => Err(ScreenError::Stopped),
                    Control::Hello { .. } => {
                        // The app restarted, but the output and the stream are
                        // still here. Repeat the greeting and the announcement
                        // so its fresh pipeline can decode us. Tearing the
                        // connector down here is what used to blank every
                        // monitor for a second whenever the cable hiccuped.
                        log::info!("phone greeted mid-stream; re-announcing the live screen");
                        send_welcome(link).map_err(ScreenError::Link)?;
                        self.announce(link).map_err(ScreenError::Link)
                    }
                    Control::StartScreen { request } => {
                        let phone = requested_mode(request);
                        if phone.width == self.phone_display.width
                            && phone.height == self.phone_display.height
                        {
                            // A repeat means the first announcement was lost and
                            // the phone is still waiting for its first frame.
                            log::info!("the phone asked again for the screen it is already shown");
                            return self.announce(link).map_err(ScreenError::Link);
                        }
                        // The phone turned: switch the output to the other mode
                        // already declared in its EDID, so the picture fills the
                        // panel in the direction it is actually held. This costs
                        // one framebuffer reallocation (a brief blank of the
                        // laptop panel), which is the accepted trade for a
                        // correctly oriented, continuous extended desktop.
                        Err(ScreenError::Reconfigure(request))
                    }
                    Control::Release { reason } => Err(ScreenError::Release(reason)),
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }

    /// Follows the phone into its other orientation without letting the display
    /// go, and says what to do about the requests that cannot be followed.
    fn handle_frame(&mut self, link: &mut AoaLink, frame: &Frame) -> Result<(), ScreenError> {
        match self.dispatch(link, frame) {
            Err(ScreenError::Reconfigure(request)) => self.retune(link, request),
            outcome => outcome,
        }
    }

    /// Rebuilds the pipeline around the shape the display is actually wearing.
    ///
    /// The picture the phone decodes is its own panel, cut out of a display the
    /// desktop may have made larger for its scale, so a new shape costs a new
    /// buffer and a new encoder and nothing else: the connector, the cable and
    /// the phone's session all stay as they are.
    fn rebind(
        &mut self,
        link: &mut AoaLink,
        mode: Mode,
        phone_display: DisplayMode,
    ) -> Result<(), ScreenError> {
        let fps = mode
            .refresh_hz
            .clamp(ScreenRequest::MIN_REFRESH_HZ, ScreenRequest::MAX_REFRESH_HZ)
            .min(self.fps_cap);
        let (buffer, free) = screen_buffers(mode)
            .map_err(|error| ScreenError::Local(format!("cannot grab the display: {error}")))?;
        let coded = coded_size(mode, phone_display.width, phone_display.height);
        let encoder = Encoder::spawn(mode, coded, fps)
            .map_err(|error| ScreenError::Local(format!("cannot encode the display: {error}")))?;
        self.buffer = buffer;
        // The old buffers belong to the shape the display just left; the first
        // grab after a modeset refreshes the whole picture, so the fresh ones
        // are brought up to date by the first copies they receive.
        self.free = free;
        self.ready = None;
        self.grabbed = None;
        self.feeder = Feeder::spawn(encoder);
        // The new decoder on the phone is waiting for an IDR, and the encoder
        // only releases its handshake IDR once a successor frame is fed. Even
        // a desktop with nothing moving owes those frames.
        self.owed_frames = FRESH_FRAMES;
        // Those two frames are not enough on their own: the phone is still
        // rebuilding its surface and holds the link behind, so they can be
        // spent before any decoder exists to receive them. Recording the debt
        // on the link keeps `grab_desktop` feeding the encoder past the point
        // where a still desktop would go quiet, until an IDR is actually sent.
        link.expect_keyframe();
        self.mode = mode;
        self.coded = coded;
        self.fps = fps;
        self.phone_display = phone_display;
        self.frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        self.next_frame = Instant::now();
        // The panel measured the shape it was built for, and the desktop's answer
        // about where the output sits has changed with it.
        self.touch = None;
        self.layout_probes = if self.wayland { LAYOUT_PROBES } else { 0 };
        self.announce(link).map_err(ScreenError::Link)?;
        // Touch comes back one turn of the loop later, once the fresh
        // announcement has its first frames out. Re-mapping the panel means
        // measuring the whole desktop and then polling the display service,
        // which measured at a third of a second after the encoder was ready and
        // held the picture back for all of it.
        self.pending.touch = true;
        Ok(())
    }

    /// Reshapes the output to the phone's new turn while keeping the connector.
    ///
    /// Both shapes are in the EDID the display was connected with, so the
    /// desktop only has to select the other mode: the rest of the layout, the
    /// other monitors and the brightness map stay exactly as they are, which is
    /// what a hot-plug could not keep. A request this output was never told
    /// about still needs the rebuild, so it comes back as the same error the
    /// caller handed in and the session takes the long way round.
    ///
    /// The desktop is asked and then left to answer on its own schedule. Waiting
    /// for it was a second or two of a frozen picture and, when it did not
    /// answer, a dead screen; streaming the shape it is still wearing until the
    /// new one arrives costs a frame or two of the desktop whole and small,
    /// which the phone has room for since it stopped stretching pictures across
    /// a panel that has turned without them.
    fn retune(&mut self, link: &mut AoaLink, request: ScreenRequest) -> Result<(), ScreenError> {
        let phone_display = requested_mode(request);
        let turned = rotated_mode(self.virtual_display);
        // The scale decides the size, and the size has to be one the connector
        // advertises; a desktop that changed its scale mid-session is not. A
        // display that was pinned to the phone's own size is turned, not
        // enlarged.
        let wanted = if self.exact {
            phone_display
        } else {
            crate::scale::fit_to_scale(phone_display, self.scale)
        };
        if wanted != turned {
            return Err(ScreenError::Reconfigure(request));
        }
        if self.card.name().is_none() {
            return Err(ScreenError::Reconfigure(request));
        }
        log::info!(
            "the phone turned; switching the virtual display to {}x{}",
            turned.width,
            turned.height
        );
        self.virtual_display = turned;
        self.reasks = REASK_LIMIT;
        // The panel has turned and the desktop has not answered yet. Note the
        // phone's new shape and keep streaming the one the display is actually
        // wearing: rebinding here too would hand the phone two new decoders for
        // one turn, and each one is a freeze until the next keyframe. The event
        // that says the desktop finally landed does the rebind, once.
        self.phone_display = phone_display;
        // The ask parks the output in the far corner of the screen box that holds
        // both of its shapes, so turning is a mode set in a framebuffer X
        // already has rather than a resize of everyone's.
        self.ask_the_desktop_to_turn(turned);
        if self.mode.width == turned.width && self.mode.height == turned.height {
            // Already wearing it, so that event is not coming; the phone would
            // otherwise keep the crop of the shape it left.
            self.turn_deadline = None;
            return self.rebind(link, self.mode, phone_display);
        }
        // A desktop that never answers leaves the phone waiting on a keyframe
        // that cannot be produced, and refusing every later turn. Give it a
        // deadline: the stream can be rebuilt around the shape the output is
        // still wearing, and a later mode event still moves it to the right one.
        self.turn_deadline = Some(Instant::now() + TURN_REBIND_TIMEOUT);
        Ok(())
    }

    fn handle_touch(&mut self, payload: &[u8]) {
        let frame = match TouchFrame::decode(payload) {
            Ok(frame) => frame,
            Err(error) => {
                log::debug!("ignoring a malformed touch report: {error}");
                return;
            }
        };
        let Some(mut touch) = self.touch.take() else {
            return;
        };
        if let Err(error) = touch.apply(&frame) {
            log::warn!("touch injection failed ({error}); disabling touch for this screen");
        } else {
            self.touch = Some(touch);
        }
    }

    /// Lets a turn the desktop never answered go, so the phone is handed a
    /// fresh keyframe instead of holding the frame it turned away from.
    ///
    /// `retune` waits for a mode event before rebuilding the encoder. A
    /// desktop that never sends one leaves the phone in `reconfiguring`,
    /// ignoring every later turn, with no new configuration to rebuild its
    /// decoder. Rebinding here costs one extra decoder on that abnormal path
    /// only; `coded_size` keeps the picture's proportions, so what arrives is
    /// whole and smaller rather than smeared across the new shape.
    fn finish_turn_if_stalled(&mut self, link: &mut AoaLink) -> Result<(), ScreenError> {
        let Some(deadline) = self.turn_deadline else {
            return Ok(());
        };
        if Instant::now() < deadline {
            return Ok(());
        }
        self.turn_deadline = None;
        log::warn!(
            "the desktop never answered the turn; rebuilding the stream at {}x{}",
            self.mode.width,
            self.mode.height
        );
        let (mode, phone_display) = (self.mode, self.phone_display);
        self.rebind(link, mode, phone_display)
    }

    fn pump_display(&mut self, link: &mut AoaLink) -> Result<(), ScreenError> {
        self.finish_turn_if_stalled(link)?;
        self.grab_desktop(link)?;

        match self.card.wait_readable(Duration::from_millis(1)) {
            Ok(true) => {
                let events = self.card.read_events().map_err(|error| {
                    ScreenError::Local(format!("cannot read evdi events: {error}"))
                })?;
                for event in events {
                    self.handle_display_event(link, event)?;
                }
            }
            Ok(false) => {}
            Err(error) => {
                return Err(ScreenError::Local(format!(
                    "cannot poll evdi events: {error}"
                )));
            }
        }
        Ok(())
    }

    /// Takes a desktop picture, and hands the previous one to the encoder as
    /// soon as its thread is free.
    ///
    /// A grab costs about as much as the conversion and write of the picture
    /// before it, so the costs are kept one picture apart: the grab happens
    /// while the encoder thread is still converting the picture before it, and
    /// only the rectangle copy into a returned pane waits for that thread. The
    /// conversion and the write then run against the next grab.
    ///
    /// A phone that has not drained the pipe is not served by more of the
    /// desktop: the picture simply holds still until it catches up, which costs
    /// its decoder nothing while a queue of unsent frames costs the link
    /// everything it was carrying. Waiting for a keyframe and a freshly
    /// rebuilt encoder are the exceptions: a still desktop would otherwise
    /// never feed the successor that releases the first IDR, leaving the phone
    /// to stretch its last frame until someone moves a window.
    fn grab_desktop(&mut self, link: &AoaLink) -> Result<(), ScreenError> {
        self.deliver_desktop()?;
        if self.grabbed.is_some() || Instant::now() < self.next_frame {
            return Ok(());
        }
        let toward_keyframe = link.is_awaiting_keyframe();
        let owed = self.owed_frames > 0;
        if link.is_behind() && !toward_keyframe && !owed {
            return Ok(());
        }
        self.next_frame = Instant::now() + self.frame_interval;
        let changed = self.card.request_update().map_err(|error| {
            ScreenError::Local(format!("cannot request a display update: {error}"))
        })?;
        if !changed && !toward_keyframe && !owed {
            self.stream.note_empty();
            return Ok(());
        }
        // The first grab after the display changed shape is handed the
        // rectangles of the shape before it, and the kernel refuses them. Having
        // refused, it forgets them, so the next grab is the display as it is now.
        for attempt in 0..2 {
            let grab_started = Instant::now();
            match self.card.grab(&mut self.buffer) {
                Ok(rectangles) => {
                    self.stream.note_grab(grab_started.elapsed());
                    // The desktop's own change is done the moment the grab
                    // returns; everything after this is our pipeline.
                    self.grabbed_at = Some(Instant::now());
                    if self.owed_frames > 0 {
                        self.owed_frames -= 1;
                    }
                    self.grabbed = Some(rectangles);
                    // A freshly built screen has its encoder thread idle, so the
                    // first picture goes out without waiting for another pass.
                    self.deliver_desktop()?;
                    return Ok(());
                }
                Err(EvdiError::StaleRects) if attempt == 0 => {
                    log::debug!("the display still carried rectangles from its previous shape");
                }
                Err(EvdiError::NotReady) => return Ok(()),
                // The desktop re-shaped the output between this grab and the
                // last, and the event that says so is on its way through the same
                // card; the pipeline follows it. Failing here would end a screen
                // that is only a moment out of date.
                Err(EvdiError::ModeMismatch { .. }) => {
                    log::debug!("the display is wearing a shape this buffer was not built for");
                    return Ok(());
                }
                Err(error) => {
                    return Err(ScreenError::Local(format!(
                        "cannot grab the virtual display: {error}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Brings a free pane up to date with the last grab and offers it to the
    /// encoder thread.
    ///
    /// Only the grab's rectangles are copied into the pane's mirror, and only
    /// those are later converted; the conversion itself runs on the encoder
    /// thread, where it overlaps the session's next grab. The copy is made
    /// while that thread is still converting the previous picture, which is what
    /// keeps the three costs off each other's way.
    fn deliver_desktop(&mut self) -> Result<(), ScreenError> {
        while let Some(pane) = self.feeder.spare() {
            // A pane the thread has finished with holds a whole picture and
            // only needs the rectangles since then.
            self.free.push(pane);
        }
        if let Some(feed) = self.ready.take() {
            let captured = feed.captured;
            match self.feeder.try_hand_off(feed) {
                Handed::Sent => self.note_handed_off(captured),
                Handed::Full(feed) => {
                    self.ready = Some(feed);
                    return Ok(());
                }
                Handed::Gone => {
                    return Err(ScreenError::Local("the encoder thread is gone".to_owned()));
                }
            }
        }
        let Some(rectangles) = self.grabbed.take() else {
            return Ok(());
        };
        let Some(mut pane) = self.free.pop() else {
            self.grabbed = Some(rectangles);
            return Ok(());
        };
        let captured = self.grabbed_at.take().unwrap_or_else(Instant::now);
        // The mirror copy and the conversion walk the same rectangles, so the
        // chroma blocks they describe agree with each other.
        let rects = chroma_rects(&rectangles, self.buffer.width(), self.buffer.height());
        let copy_started = Instant::now();
        let feed = if pane.synced {
            pane.mirror.copy_rects_from(&self.buffer, &rects);
            Feed {
                pane,
                rects,
                captured,
            }
        } else {
            // A fresh pair is zeroed and a grab only refreshes what changed,
            // so the first picture it ever receives is copied whole.
            pane.mirror.copy_all_from(&self.buffer);
            Feed {
                pane,
                rects: Vec::new(),
                captured,
            }
        };
        self.stream.note_copy(copy_started.elapsed());
        match self.feeder.try_hand_off(feed) {
            Handed::Sent => {
                self.note_handed_off(captured);
                Ok(())
            }
            Handed::Full(feed) => {
                self.ready = Some(feed);
                Ok(())
            }
            Handed::Gone => Err(ScreenError::Local("the encoder thread is gone".to_owned())),
        }
    }

    /// Remembers when a picture went to the encoder thread.
    ///
    /// The queue is capped: a picture the encoder never returns as an access
    /// unit would otherwise leave this growing for the life of the session.
    fn note_handed_off(&mut self, captured: Instant) {
        if self.pipe.len() >= PIPE_DEPTH {
            self.pipe.pop_front();
        }
        self.pipe.push_back(captured);
    }

    fn handle_display_event(
        &mut self,
        link: &mut AoaLink,
        event: Event,
    ) -> Result<(), ScreenError> {
        match event {
            Event::ModeChanged { pixel_format, .. } => {
                let Some(mode) = event.mode() else {
                    return Err(ScreenError::Local(format!(
                        "the compositor chose unsupported pixel format 0x{pixel_format:08x}"
                    )));
                };
                if mode == self.mode {
                    return Ok(());
                }
                // One EDID carries both shapes of a phone, and a desktop laying
                // out a monitor it has just seen arrives takes the one that comes
                // first in the blob. Asking it back is worth a try, but the
                // picture follows whatever the display is wearing: a session that
                // stopped to wait for an answer left the phone dark, and then
                // failed with nothing to show.
                if mode.width != self.virtual_display.width
                    || mode.height != self.virtual_display.height
                {
                    if self.reasks > 0 {
                        self.reasks -= 1;
                        log::info!(
                            "the desktop laid the monitor out at {}x{}; asking for {}x{} again",
                            mode.width,
                            mode.height,
                            self.virtual_display.width,
                            self.virtual_display.height
                        );
                        self.ask_the_desktop_to_turn(self.virtual_display);
                    } else {
                        log::warn!(
                            "the desktop keeps {}x{} while the phone is wearing {}x{}",
                            mode.width,
                            mode.height,
                            self.virtual_display.width,
                            self.virtual_display.height
                        );
                    }
                }
                self.turn_deadline = None;
                self.rebind(link, mode, self.phone_display)?;
                // The same bitmap wears a different size on the new shape, so
                // the phone is sent it again rather than left scaling the old.
                self.cursor.set_screen(self.mode.width, self.mode.height);
            }
            Event::CrtcState { state: 0 } => {
                log::debug!("the compositor turned the virtual display off");
                self.cursor.hide();
            }
            Event::CursorSet { enabled: false, .. } | Event::Dpms { mode: 0 } => {
                self.cursor.hide();
            }
            Event::CursorSet {
                enabled: true,
                hot_x,
                hot_y,
                width,
                height,
                handle,
                buffer_length,
                pixel_format,
                stride,
            } => {
                match self.card.cursor_rgba(
                    handle,
                    buffer_length,
                    width,
                    height,
                    stride,
                    pixel_format,
                ) {
                    Ok(pixels) => {
                        if self.cursor.has_shape() {
                            log::debug!(
                                "cursor bitmap {width}x{height} with hotspot {hot_x},{hot_y} \
                                 on {}x{}",
                                self.mode.width,
                                self.mode.height
                            );
                        } else {
                            log::info!(
                                "the phone now draws the pointer locally \
                                 ({width}x{height} bitmap on {}x{})",
                                self.mode.width,
                                self.mode.height
                            );
                        }
                        self.cursor.set_shape(width, height, hot_x, hot_y, pixels);
                    }
                    Err(error) => {
                        // The kernel already stopped blending the pointer into
                        // the picture, so without a bitmap the phone would show
                        // none at all. Give the pointer back to the compositor.
                        log::warn!(
                            "cannot draw the compositor pointer locally ({error}); \
                             blending it into the picture again"
                        );
                        if let Err(error) = self.card.disable_cursor_events() {
                            log::warn!("cannot turn cursor blending back on: {error}");
                        }
                        // The phone may already be drawing the last bitmap; say
                        // so, or the returned video pointer would be the second
                        // one on screen.
                        self.cursor.hide();
                    }
                }
            }
            Event::CursorMove { x, y } => self.cursor.moved(x, y),
            _ => {}
        }
        Ok(())
    }

    fn forward_video(&mut self, link: &mut AoaLink) -> Result<(), ScreenError> {
        loop {
            match self.feeder.try_event() {
                Ok(Some(FeedEvent::Nal(nal))) => {
                    // An access unit comes back in the order its picture was
                    // handed over, so the oldest entry is this one's grab.
                    if let Some(captured) = self.pipe.pop_front() {
                        self.stream.note_pipe(captured.elapsed());
                    }
                    let flags = if nal.is_keyframe() { FLAG_KEYFRAME } else { 0 };
                    link.send_media(&Frame::with_flags(Kind::Video, flags, nal.bytes))
                        .map_err(ScreenError::Link)?;
                }
                Ok(Some(FeedEvent::Fed { convert, feed })) => {
                    self.stream.note_conv(convert);
                    self.stream.note_feed(feed);
                }
                Ok(Some(FeedEvent::Failed(reason))) => return Err(ScreenError::Local(reason)),
                Ok(None) => return Ok(()),
                Err(()) => {
                    return Err(ScreenError::Local("the encoder thread stopped".to_owned()));
                }
            }
        }
    }

    fn pump_audio(&mut self, link: &mut AoaLink) -> Result<(), ScreenError> {
        let Some(mut audio) = self.audio.take() else {
            return Ok(());
        };
        let mut keep = true;
        loop {
            match audio.next_chunk(Duration::ZERO) {
                Ok(Some(chunk)) => {
                    link.send_media(&Frame::new(Kind::Audio, chunk))
                        .map_err(ScreenError::Link)?;
                }
                Ok(None) => break,
                Err(error) => {
                    log::info!("desktop audio capture stopped ({error}); continuing without audio");
                    keep = false;
                    break;
                }
            }
        }
        if keep {
            self.audio = Some(audio);
        }
        Ok(())
    }

    /// Logs and publishes what the last second of streaming did.
    ///
    /// One line per second is enough to tell an encoder-limited stream from
    /// one the phone cannot drain, and far cheaper than per-frame logging, which
    /// would cost more than the streaming it describes.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a dashboard sample is rounded for display, not used as control"
    )]
    fn report_stream(&mut self, link: &AoaLink) {
        let now = Instant::now();
        let counters = link.stream_counters(now);
        let Some(rates) = self.stream.take(now, counters) else {
            return;
        };
        log::info!(
            "stream {:.1}fps {:.1}Mbps dropped {} lost {} behind {} waiting {:.0}ms \
             grab {:.0}ms max {:.0}ms copy {:.0}ms max {:.0}ms conv {:.0}ms max {:.0}ms \
             feed {:.0}ms max {:.0}ms loop {:.1}ms max {:.0}ms/{} \
             pipe {:.1}ms max {:.0}ms/{} write {:.1}ms max {:.0}ms \
             poll {:.0}ms max {:.0}ms grabs {} empty {}",
            rates.fps,
            rates.mbps,
            rates.dropped,
            rates.lost,
            rates.behind,
            rates.waited.as_secs_f64() * 1000.0,
            rates.grab_total.as_secs_f64() * 1000.0,
            rates.grab_max.as_secs_f64() * 1000.0,
            rates.copy_total.as_secs_f64() * 1000.0,
            rates.copy_max.as_secs_f64() * 1000.0,
            rates.conv_total.as_secs_f64() * 1000.0,
            rates.conv_max.as_secs_f64() * 1000.0,
            rates.feed_total.as_secs_f64() * 1000.0,
            rates.feed_max.as_secs_f64() * 1000.0,
            rates.loop_avg.as_secs_f64() * 1000.0,
            rates.loop_max.as_secs_f64() * 1000.0,
            rates.loop_passes,
            rates.pipe_avg.as_secs_f64() * 1000.0,
            rates.pipe_max.as_secs_f64() * 1000.0,
            rates.pipe_samples,
            rates.write_avg.as_secs_f64() * 1000.0,
            rates.write_max.as_secs_f64() * 1000.0,
            rates.poll_total.as_secs_f64() * 1000.0,
            rates.poll_max.as_secs_f64() * 1000.0,
            rates.grabs,
            rates.empty_updates,
        );
        requests::update(|state| {
            state.fps = Some(rates.fps as f32);
            state.mbps = Some(rates.mbps as f32);
            state.dropped = Some(u32::try_from(rates.dropped).unwrap_or(u32::MAX));
            state.behind = Some(u32::try_from(rates.behind).unwrap_or(u32::MAX));
        });
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // The screen that is leaving still has its ask threads, and the EDID
        // they name a mode from is about to disappear. Left running, they would
        // fail against the next screen or place the old shape over it.
        crate::x11::cancel_asks();
        crate::wlr::cancel_asks();
        let _ = self.card.disconnect();
        requests::update(|state| {
            state.streaming = false;
            state.screen = String::new();
        });
    }
}

/// Failure while waiting for the compositor to configure the display.
#[derive(Debug, Error)]
enum ModeError {
    /// The evdi card failed.
    #[error(transparent)]
    Evdi(#[from] EvdiError),
    /// No mode arrived in time.
    #[error("the compositor did not configure the virtual display in time")]
    Timeout,
    /// The scanout format cannot be encoded.
    #[error("unsupported scanout pixel format 0x{0:08x}")]
    UnsupportedFormat(u32),
}

/// Waits for the display to light up, preferring the shape just asked of it.
///
/// A connector that is being re-shaped answers with the geometry it wore a
/// moment first: when the phone turns, the old shape is the event that arrives,
/// and the new one follows it by a second or so. The answer that is only half
/// right is still worth taking, because a display that is wearing *a* shape can
/// be streamed and followed, while one the session refused to accept left the
/// phone with no picture and an error. So the asked-for shape is preferred for
/// the first half of the wait and any shape the display reports is taken after
/// that, with `false` saying the desktop has not answered yet.
fn wait_for_mode(
    card: &EvdiCard,
    timeout: Duration,
    wanted: DisplayMode,
) -> Result<(Mode, bool), ModeError> {
    let deadline = Instant::now() + timeout;
    let patience = Instant::now() + timeout / 2;
    let mut other: Option<Mode> = None;
    loop {
        if let Some(mode) = other {
            if Instant::now() >= patience {
                return Ok((mode, false));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return other.map(|mode| (mode, false)).ok_or(ModeError::Timeout);
        }
        if !card.wait_readable(remaining.min(Duration::from_millis(50)))? {
            continue;
        }
        for event in card.read_events()? {
            if let Event::ModeChanged { pixel_format, .. } = event {
                let Some(mode) = event.mode() else {
                    return Err(ModeError::UnsupportedFormat(pixel_format));
                };
                if mode.width == wanted.width && mode.height == wanted.height {
                    return Ok((mode, true));
                }
                log::debug!(
                    "the display reports {}x{} while {}x{} is asked of it",
                    mode.width,
                    mode.height,
                    wanted.width,
                    wanted.height
                );
                if other.is_none() {
                    other = Some(mode);
                }
            }
        }
    }
}

/// Failure that prevents a screen from starting, phrased for the phone.
struct StartFailure {
    code: &'static str,
    message: String,
}

impl StartFailure {
    fn new(code: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: error.to_string(),
        }
    }
}

/// Failure inside one streaming screen.
#[derive(Debug)]
enum ScreenError {
    /// The phone asked to stop; not a failure.
    Stopped,
    /// The phone turned and asked for another shape of the same session.
    Reconfigure(ScreenRequest),
    /// The desktop took the virtual output out of its layout.
    Dropped(ScreenRequest),
    /// The phone would rather have its USB port back than a screen.
    Release(String),
    /// The USB link failed.
    Link(AoaError),
    /// The local display pipeline failed; the phone stays connected.
    Local(String),
}

#[cfg(test)]
mod tests {
    use super::{DropStreak, REVIVE_WINDOW};
    use std::time::Instant;

    #[test]
    fn a_drop_after_a_long_healthy_run_starts_over() {
        let mut streak = DropStreak::default();
        let start = Instant::now();
        assert_eq!(streak.note(start), 1, "the first drop starts the run");
        assert_eq!(
            streak.note(start + REVIVE_WINDOW / 2),
            2,
            "a drop inside the window continues the run",
        );
        assert_eq!(
            streak.note(start + REVIVE_WINDOW * 2),
            1,
            "a drop after a long healthy run starts over",
        );
    }
}
