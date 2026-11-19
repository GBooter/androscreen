//! Enabling the virtual output on a Wayland session.
//!
//! X11 desktops re-probe the evdi connector as soon as the daemon forces it
//! connected. Treeland does not: the head shows up in the compositor's output
//! list still disabled and stays that way until a client asks
//! `zwlr_output_manager_v1` to enable it. The DDE display service is not on the
//! bus of a Wayland session, so the daemon speaks the protocol itself.
//!
//! Two habits of the compositor shape how that is done. A client is told about
//! the outputs that existed when it introduced itself and never about one bound
//! later, so each ask re-introduces itself; and a compositor in the middle of a
//! modeset answers a new client seconds late, so the asking happens on a thread
//! of its own rather than in front of the phone waiting for its first frame.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

/// Refresh rates travel in milli-Hertz.
const MILLI_HZ: i32 = 1000;

/// One mode the compositor listed for a head.
#[derive(Debug)]
struct Mode {
    proxy: ZwlrOutputModeV1,
    width: i32,
    height: i32,
    refresh_mhz: i32,
    preferred: bool,
}

/// One output the compositor knows about.
#[derive(Debug)]
struct Head {
    proxy: ZwlrOutputHeadV1,
    name: String,
    enabled: bool,
    x: i32,
    y: i32,
    scale: f64,
    /// `wl_output_transform`, so that a turned monitor is measured turned.
    transform: u32,
    modes: Vec<Mode>,
    /// The mode in use, only remembered while the head is enabled.
    current: Option<ZwlrOutputModeV1>,
}

impl Head {
    /// The mode being shown, once the compositor has named it.
    fn current_mode(&self) -> Option<&Mode> {
        self.current
            .as_ref()
            .and_then(|mode| self.modes.iter().find(|known| &known.proxy == mode))
    }

    /// The rectangle this head occupies in the compositor's layout, in logical
    /// pixels, or `None` while it is dark.
    fn layout_size(&self) -> Option<(i32, i32)> {
        let mode = self.current_mode()?;
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let width = (f64::from(mode.width) / scale).round() as i32;
        let height = (f64::from(mode.height) / scale).round() as i32;
        // A quarter turn swaps the edges the layout measures.
        Some(if self.transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        })
    }

    /// The mode that matches the requested picture, if the head offers one.
    fn find_mode(&self, width: i32, height: i32, refresh_hz: u32) -> Option<&Mode> {
        self.modes.iter().find(|mode| {
            mode.width == width
                && mode.height == height
                && mode.refresh_mhz / MILLI_HZ == i32::try_from(refresh_hz).unwrap_or(i32::MAX)
        })
    }
}

/// What the compositor answered to the configuration we applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The configuration is live.
    Succeeded,
    /// The compositor refused it.
    Failed,
    /// Someone else reconfigured the outputs first.
    Cancelled,
}

/// Everything the daemon needs to know about the compositor's outputs.
#[derive(Debug, Default)]
struct State {
    manager: Option<ZwlrOutputManagerV1>,
    heads: Vec<Head>,
    /// Serial of the last `done`, required by `create_configuration`.
    serial: Option<u32>,
    outcome: Option<Outcome>,
}

impl State {
    fn head_mut(&mut self, proxy: &ZwlrOutputHeadV1) -> Option<&mut Head> {
        self.heads.iter_mut().find(|head| &head.proxy == proxy)
    }

    /// The head named `name`, once the compositor has described it fully.
    fn find(&self, name: &str) -> Option<&Head> {
        self.heads.iter().find(|head| head.name == name)
    }

    /// What the compositor listed, for a failure worth explaining.
    fn summary(&self) -> String {
        if self.heads.is_empty() {
            return "no heads at all".to_owned();
        }
        self.heads
            .iter()
            .map(|head| {
                format!(
                    "{:?} {}{} ({} modes)",
                    head.name,
                    if head.enabled { "on" } else { "off" },
                    if head.current.is_some() {
                        ""
                    } else {
                        ", no mode"
                    },
                    head.modes.len(),
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Right edge of the layout the compositor currently shows.
    fn right_edge(&self, except: &str) -> i32 {
        self.heads
            .iter()
            .filter(|head| head.enabled && head.name != except)
            .filter_map(|head| {
                head.layout_size()
                    .map(|(width, _)| head.x.saturating_add(width))
            })
            .max()
            .unwrap_or(0)
            .max(0)
    }

    /// Where the compositor draws `connector`, and how far the desktop reaches.
    ///
    /// A compositor that spreads an unmapped absolute device over the whole
    /// layout needs both: the panel is told the desktop's size and the offset
    /// the virtual output sits at inside it.
    fn placement(&self, connector: &str) -> Option<Placement> {
        let target = self.find(connector)?;
        let (output_width, output_height) = target.layout_size()?;
        let mut left = i32::MAX;
        let mut top = i32::MAX;
        let mut right = i32::MIN;
        let mut bottom = i32::MIN;
        for head in self.heads.iter().filter(|head| head.enabled) {
            let Some((width, height)) = head.layout_size() else {
                continue;
            };
            left = left.min(head.x);
            top = top.min(head.y);
            right = right.max(head.x.saturating_add(width));
            bottom = bottom.max(head.y.saturating_add(height));
        }
        if left == i32::MAX || right <= left || bottom <= top {
            return None;
        }
        let span = |edge: i32| u32::try_from(edge.max(0)).unwrap_or(u32::MAX);
        Some(Placement {
            left,
            top,
            span_width: span(right.saturating_sub(left)),
            span_height: span(bottom.saturating_sub(top)),
            output_left: target.x,
            output_top: target.y,
            output_width: span(output_width),
            output_height: span(output_height),
        })
    }
}

/// Where a screen sits in the compositor's global space, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    /// Left edge of the whole desktop.
    pub left: i32,
    /// Top edge of the whole desktop.
    pub top: i32,
    /// Width of the whole desktop.
    pub span_width: u32,
    /// Height of the whole desktop.
    pub span_height: u32,
    /// Left edge of the virtual output.
    pub output_left: i32,
    /// Top edge of the virtual output.
    pub output_top: i32,
    /// Width of the virtual output.
    pub output_width: u32,
    /// Height of the virtual output.
    pub output_height: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        log::debug!("compositor global {interface} v{version}");
        if interface != zwlr_output_manager_v1::ZwlrOutputManagerV1::interface().name {
            return;
        }
        // Binding a newer version than the compositor offers would abort the
        // connection, so cap what the local copy asks for.
        let wanted = ZwlrOutputManagerV1::interface().version;
        let manager: ZwlrOutputManagerV1 = registry.bind(name, version.min(wanted), qh, ());
        // A compositor that already handed out its heads does not always announce
        // a hot-plugged one to the manager a client bound beforehand, so each
        // rescan keeps the newest binding and the heads it lists.
        state.manager = Some(manager);
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    wayland_client::event_created_child!(Self, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(Head {
                proxy: head,
                name: String::new(),
                enabled: false,
                x: 0,
                y: 0,
                scale: 1.0,
                transform: 0,
                modes: Vec::new(),
                current: None,
            }),
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            zwlr_output_manager_v1::Event::Finished => state.manager = None,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    wayland_client::event_created_child!(Self, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);

    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_head_v1::Event::Name { name } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.name = name;
                }
            }
            zwlr_output_head_v1::Event::Enabled { enabled } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.enabled = enabled != 0;
                    if !head.enabled {
                        head.current = None;
                    }
                }
            }
            zwlr_output_head_v1::Event::Mode { mode } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.modes.push(Mode {
                        proxy: mode,
                        width: 0,
                        height: 0,
                        refresh_mhz: 0,
                        preferred: false,
                    });
                }
                let _ = qh;
            }
            zwlr_output_head_v1::Event::CurrentMode { mode } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.current = Some(mode);
                }
            }
            zwlr_output_head_v1::Event::Position { x, y } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.x = x;
                    head.y = y;
                }
            }
            zwlr_output_head_v1::Event::Transform { transform } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.transform = transform.into();
                }
            }
            zwlr_output_head_v1::Event::Scale { scale } => {
                if let Some(head) = state.head_mut(proxy) {
                    head.scale = scale;
                }
            }
            zwlr_output_head_v1::Event::Finished => {
                state.heads.retain(|head| &head.proxy != proxy);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let index = state
            .heads
            .iter()
            .position(|candidate| candidate.modes.iter().any(|known| &known.proxy == proxy));
        let Some(index) = index else { return };
        let Some(head) = state.heads.get_mut(index) else {
            return;
        };
        let Some(mode) = head.modes.iter_mut().find(|known| known.proxy == *proxy) else {
            return;
        };
        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                mode.width = width;
                mode.height = height;
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => mode.refresh_mhz = refresh,
            zwlr_output_mode_v1::Event::Preferred => mode.preferred = true,
            zwlr_output_mode_v1::Event::Finished => head.modes.retain(|m| &m.proxy != proxy),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => {
                state.outcome = Some(Outcome::Succeeded);
            }
            zwlr_output_configuration_v1::Event::Failed => state.outcome = Some(Outcome::Failed),
            zwlr_output_configuration_v1::Event::Cancelled => {
                state.outcome = Some(Outcome::Cancelled);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        event: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let _ = event;
    }
}

/// Runs the connection until `ready` says so, or the budget is spent.
///
/// `ready` is asked after every batch of events, so a caller can stop at the
/// first moment the state it waits for is complete.
fn pump(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    budget: Duration,
    mut ready: impl FnMut(&mut State) -> bool,
) -> Result<bool, String> {
    let deadline = Instant::now() + budget;
    loop {
        if ready(state) {
            return Ok(true);
        }
        let dispatched = queue
            .dispatch_pending(state)
            .map_err(|error| format!("the compositor broke the link: {error}"))?;
        if dispatched > 0 {
            continue;
        }
        conn.flush()
            .map_err(|error| format!("cannot reach the compositor: {error}"))?;
        if !wait_for_data(conn, deadline)? {
            return Ok(false);
        }
    }
}

/// Waits for the compositor socket to carry something, in short polls.
///
/// A compositor that is busy re-probing a hot-plugged output answers late but
/// still answers, so one quiet poll means nothing; only the deadline does.
fn wait_for_data(conn: &Connection, deadline: Instant) -> Result<bool, String> {
    loop {
        // The backend only reads through a guard, so poll the descriptor the
        // guard names and hand it back before asking for the bytes.
        let Some(guard) = conn.prepare_read() else {
            // Another reader holds the socket; give it a moment and look again.
            std::thread::sleep(POLL_SLICE);
            continue;
        };
        let mut poll_fd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN | libc::POLLERR,
            revents: 0,
        };
        let millis = i32::try_from(POLL_SLICE.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: the descriptor comes from a live `Connection` and one `pollfd`
        // entry is passed with a matching count.
        let ready = unsafe { libc::poll(&raw mut poll_fd, 1, millis) };
        if ready > 0 {
            guard
                .read()
                .map_err(|error| format!("the compositor closed the link: {error}"))?;
            return Ok(true);
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("cannot wait for the compositor: {error}"));
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
    }
}

/// Asks the compositor, in the background, to place the new output.
///
/// The session cannot wait for this: the daemon is being paid to stream, and
/// the picture already moves as soon as the compositor enables the connector
/// its own way. This is the ask that makes the result deliberate instead of
/// lucky, and it keeps trying until the compositor takes it.
pub(crate) fn request(connector: String, width: u32, height: u32, refresh_hz: u32) {
    // Whatever the compositor said about the screen before this one is now
    // wrong, and a touch panel built on it would reach for the wrong pixels.
    forget_layout();
    let generation = NEWEST_LAYOUT
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let current = move || NEWEST_LAYOUT.load(Ordering::Relaxed) == generation;
    let named = connector.clone();
    if std::thread::Builder::new()
        .name("androscreen-layout".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + PLACE_WINDOW;
            let mut attempts = 0;
            let mut last = String::from("the compositor stopped answering");
            while Instant::now() < deadline {
                // A newer screen has taken over the output; the shape this ask
                // was built for is gone and asking for it again can only undo
                // what the newer screen is arranging.
                if !current() {
                    return;
                }
                attempts += 1;
                let outcome = match Planner::new() {
                    Ok(mut planner) => {
                        planner.place(&connector, width, height, refresh_hz, PLACE_BUDGET)
                    }
                    Err(error) => Err(error),
                };
                let Err(error) = outcome else { return };
                log::debug!("{connector} is not on the layout yet (attempt {attempts}): {error}");
                last = error;
                std::thread::sleep(PLACE_RETRY);
            }
            if current() {
                log::warn!(
                    "the compositor never took our layout for {connector}, after {attempts} attempts: {last}"
                );
            }
        })
        .is_err()
    {
        log::warn!("no thread to ask the compositor about {named}");
    }
}

/// Which layout ask is the newest one.
///
/// The screen that was replaced leaves its ask running for up to
/// [`PLACE_WINDOW`]; only the newest generation may keep asking, or it would
/// place the old shape over the new screen.
static NEWEST_LAYOUT: AtomicU64 = AtomicU64::new(0);

/// Stops every layout ask whose screen is being taken down.
pub(crate) fn cancel_asks() {
    NEWEST_LAYOUT.fetch_add(1, Ordering::Relaxed);
}

/// How long the compositor gets to answer the first registry read.
const FIRST_CONTACT: Duration = Duration::from_millis(2500);
/// How long the compositor's own word on a layout trails its answer.
const LAYOUT_SETTLE: Duration = Duration::from_millis(500);

/// Whether this session runs on a Wayland compositor at all.
#[must_use]
pub(crate) fn on_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("WAYLAND_SOCKET").is_some()
}

/// Whether the compositor, and not an X server, will lay this output out.
///
/// A compositor can answer a socket in the runtime directory of a session that
/// is being drawn by X: a greeter that has not gone, a sandbox, a crash that
/// left the socket behind. Talking its protocol in that session asks it to
/// enable an output it does not have, while the X server that does have it is
/// never told, and the phone is left with no screen. What the display manager
/// opened the session as outranks any socket, and after that the server listing
/// the connector is the one whose answer matters.
#[must_use]
pub(crate) fn owns_layout(output: &str) -> bool {
    if !on_wayland() || session_says_x11() {
        return false;
    }
    !crate::x11::lists_output(output)
}

/// Whether the desktop said which kind of session it opened.
fn session_says_x11() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind.eq_ignore_ascii_case("x11"))
}

/// Finds this session's compositor when the environment did not say.
///
/// A systemd user service starts without the desktop's environment, and a
/// wlroots compositor is free to name its socket after itself rather than
/// `wayland-0`. The runtime directory is searched and each candidate is tried
/// for real, so the daemon reaches the layout protocol in a session it was
/// never told about.
///
/// A session the display manager already named as X is not searched: the
/// compositor that answers one of those sockets is not drawing the desktop, and
/// pretending otherwise sends the daemon's asks to the wrong program.
pub(crate) fn adopt_session() {
    if on_wayland() || session_says_x11() {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    for name in candidates(std::path::Path::new(&runtime)) {
        std::env::set_var("WAYLAND_DISPLAY", &name);
        if Connection::connect_to_env().is_ok() {
            log::info!("the compositor answers on {name}");
            return;
        }
        std::env::remove_var("WAYLAND_DISPLAY");
    }
}

/// What the compositor says its layout is, for `--layout-scan`.
///
/// # Errors
///
/// Reports a session the daemon cannot reach a compositor from, or one that
/// never finished listing its heads.
pub(crate) fn scan() -> Result<String, String> {
    let mut planner = Planner::new()?;
    if !planner.listen_for(FIRST_CONTACT, |state| !state.heads.is_empty())? {
        return Err("the compositor never finished listing its heads".to_owned());
    }
    let mut lines = vec![planner.state.summary()];
    for head in planner.state.heads.iter().filter(|head| head.enabled) {
        let name = head.name.clone();
        let placed = planner.state.placement(&name).map_or_else(
            || format!("{name}: not placed"),
            |place| {
                let (mode_width, mode_height) = head
                    .current_mode()
                    .map_or((0, 0), |mode| (mode.width, mode.height));
                format!(
                    "{name}: {}x{} turned {}, at {},{} of a {}x{} desktop",
                    mode_width,
                    mode_height,
                    head.transform,
                    place.output_left,
                    place.output_top,
                    place.span_width,
                    place.span_height,
                )
            },
        );
        lines.push(placed);
    }
    Ok(lines.join("\n"))
}

/// Sockets in `runtime` that could be a display, the likeliest first.
fn candidates(runtime: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(runtime) else {
        return Vec::new();
    };
    let mut found: Vec<(u8, String)> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_type()
                .is_ok_and(|kind| std::os::unix::fs::FileTypeExt::is_socket(&kind))
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // An X11 socket answers to the Wayland handshake with garbage.
            if name.contains("xwayland") {
                return None;
            }
            let rank = if let Some(tail) = name.strip_prefix("wayland-") {
                tail.parse::<u8>().ok()?
            } else if name.ends_with(".socket") {
                100
            } else {
                return None;
            };
            Some((rank, name))
        })
        .collect();
    found.sort();
    found.truncate(8);
    found.into_iter().map(|(_, name)| name).collect()
}

/// The layout the daemon last heard from the compositor.
static LAYOUT: Mutex<Option<Placement>> = Mutex::new(None);

fn shared() -> std::sync::MutexGuard<'static, Option<Placement>> {
    LAYOUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The layout the compositor described, once the asking thread has read it.
///
/// Never blocks, because the session is busy streaming when it asks; an answer
/// that has not arrived yet simply means trying again beside the next frame.
#[must_use]
pub(crate) fn layout() -> Option<Placement> {
    *shared()
}

/// Forgets a layout that belongs to a screen that is going away.
pub(crate) fn forget_layout() {
    *shared() = None;
}
/// How long one layout ask waits for the head to be listed.
const PLACE_BUDGET: Duration = Duration::from_millis(3000);
/// Pause between layout asks.
const PLACE_RETRY: Duration = Duration::from_millis(700);
/// How long the compositor is pursued before the layout is left to it.
const PLACE_WINDOW: Duration = Duration::from_secs(40);
/// How long a re-introduction waits for the compositor to offer output management.
const REOPEN_CONTACT: Duration = Duration::from_millis(3000);
/// How long the socket is polled at a time.
const POLL_SLICE: Duration = Duration::from_millis(50);

/// The daemon's standing ask that the compositor lay out the virtual output.
///
/// Placing an output is a conversation rather than a call: the connector only
/// becomes a head some moments after the daemon forces it connected, so one
/// connection keeps listening instead of reconnecting per attempt, and it asks
/// for the global list again when the compositor has said nothing about it.
pub(crate) struct Planner {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
}

impl Planner {
    /// Connects to this session's compositor and binds output management.
    ///
    /// # Errors
    ///
    /// Reports a session without a compositor, or one that does not offer the
    /// output-management protocol.
    pub(crate) fn new() -> Result<Self, String> {
        Self::open(FIRST_CONTACT)
    }

    /// Connects, and waits up to `contact` for the compositor to offer output
    /// management.
    fn open(contact: Duration) -> Result<Self, String> {
        let conn = Connection::connect_to_env()
            .map_err(|error| format!("no Wayland compositor: {error}"))?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut planner = Self {
            conn,
            queue,
            qh,
            state: State::default(),
        };
        let offered = planner.listen_for(contact, |state| state.manager.is_some())?;
        if !offered {
            return Err("the compositor offered no output management protocol".to_owned());
        }
        Ok(planner)
    }

    /// Asks the compositor to place `connector` at the requested mode.
    ///
    /// # Errors
    ///
    /// Reports a compositor that broke the link, never listed the connector
    /// within `budget`, or refused the layout.
    pub(crate) fn place(
        &mut self,
        connector: &str,
        width: u32,
        height: u32,
        refresh_hz: u32,
        budget: Duration,
    ) -> Result<(), String> {
        // The compositor tells a client about outputs that existed when that
        // client introduced itself, and stays silent about ones bound later, so
        // every ask starts from a fresh introduction. Opening during a
        // modeset is what the rest of the budget is for: the compositor
        // answers late then, but it does answer.
        *self = Self::open(REOPEN_CONTACT.min(budget))?;
        // The introduction has its own allowance; what is left is for the head
        // list and the answer to the layout.
        let deadline = Instant::now() + budget;
        let listed = self.listen(deadline, |state| {
            state
                .find(connector)
                .is_some_and(|head| !head.modes.is_empty())
        })?;
        if !listed {
            return Err(format!(
                "the compositor never listed {connector}; it reported {}",
                self.state.summary()
            ));
        }
        let outcome = self.apply(connector, width, height, refresh_hz, deadline);
        self.publish(connector);
        outcome
    }

    /// Keeps the layout the compositor describes, where the session can read it.
    ///
    /// A head's own word on where it landed trails the answer to the layout, so
    /// it gets a moment. The panel cannot wait for that: it is built while the
    /// phone is waiting for its first frame, and a panel that measures only the
    /// virtual output misses every finger on a desktop that will not map it.
    fn publish(&mut self, connector: &str) {
        let settled = self
            .listen_for(LAYOUT_SETTLE, |state| state.placement(connector).is_some())
            .unwrap_or(false);
        if !settled {
            return;
        }
        let found = self.state.placement(connector);
        if let Some(at) = found {
            log::debug!(
                "the desktop spans {}x{} from {},{} and puts {connector} at {},{}",
                at.span_width,
                at.span_height,
                at.left,
                at.top,
                at.output_left,
                at.output_top
            );
        }
        *shared() = found;
    }

    /// Runs the connection until `ready` is satisfied or the deadline passes.
    fn listen(
        &mut self,
        deadline: Instant,
        ready: impl FnMut(&mut State) -> bool,
    ) -> Result<bool, String> {
        pump(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            deadline.saturating_duration_since(Instant::now()),
            ready,
        )
    }

    /// Like [`Self::listen`], measured from now.
    fn listen_for(
        &mut self,
        budget: Duration,
        ready: impl FnMut(&mut State) -> bool,
    ) -> Result<bool, String> {
        self.listen(Instant::now() + budget, ready)
    }

    /// Sends one layout and waits for the compositor's verdict on it.
    ///
    /// Every other head travels unchanged: leaving one out is a protocol
    /// error, and the compositor would close the connection on the spot.
    fn apply(
        &mut self,
        connector: &str,
        width: u32,
        height: u32,
        refresh_hz: u32,
        deadline: Instant,
    ) -> Result<(), String> {
        let serial = self
            .state
            .serial
            .ok_or_else(|| format!("{connector} appeared without a configuration serial"))?;
        let target = self
            .state
            .find(connector)
            .ok_or_else(|| format!("lost {connector}"))?
            .proxy
            .clone();
        let wanted = (
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
        );
        let mode = self
            .state
            .find(connector)
            .and_then(|head| head.find_mode(wanted.0, wanted.1, refresh_hz))
            .map(|mode| mode.proxy.clone());
        let edge = self.state.right_edge(connector);
        let manager = self
            .state
            .manager
            .as_ref()
            .ok_or_else(|| "output management went away".to_owned())?;

        self.state.outcome = None;
        let configuration = manager.create_configuration(serial, &self.qh, ());
        // Enabling the virtual head re-writes the whole layout, and the
        // compositor fills in every property we do not name with a default.
        // Leaving the other screens' scale, turn, place and mode unstated is
        // exactly how a user's scaling got silently reset to 100% on the next
        // login: pass each one through so "keep it as it was" means what it
        // says.
        for head in self
            .state
            .heads
            .iter()
            .filter(|head| head.name != connector)
        {
            if !head.enabled {
                configuration.disable_head(&head.proxy);
                continue;
            }
            let keep = configuration.enable_head(&head.proxy, &self.qh, ());
            if let Some(mode) = &head.current {
                keep.set_mode(mode);
            }
            keep.set_position(head.x, head.y);
            if let Ok(transform) = wl_output::Transform::try_from(head.transform) {
                keep.set_transform(transform);
            }
            keep.set_scale(head.scale.max(0.0));
        }
        let head_configuration = configuration.enable_head(&target, &self.qh, ());
        match mode {
            Some(mode) => head_configuration.set_mode(&mode),
            None => head_configuration.set_custom_mode(
                wanted.0,
                wanted.1,
                i32::try_from(refresh_hz)
                    .unwrap_or(i32::MAX)
                    .saturating_mul(MILLI_HZ),
            ),
        }
        head_configuration.set_position(edge, 0);
        configuration.apply();

        let answered = self.listen(deadline, |state| state.outcome.is_some())?;
        match self.state.outcome {
            Some(Outcome::Succeeded) => {
                log::info!(
                    "{connector} enabled at {width}x{height}@{refresh_hz}Hz, right of {edge}"
                );
                Ok(())
            }
            Some(outcome) => Err(format!("the compositor refused the layout ({outcome:?})")),
            None if answered => unreachable!("pump only settles with an outcome"),
            None => Err(format!("the compositor never answered for {connector}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::candidates;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};

    /// A directory of sockets that only this run of the suite sees.
    fn runtime_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("as-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a directory to hold test sockets");
        path
    }

    /// Makes one socket exist, or says why this machine cannot.
    ///
    /// A unix socket path has to fit the kernel's `sun_path`, so a `TMPDIR`
    /// nested deep in a checkout makes binding fail. Ignoring that leaves the
    /// directory empty, and the assertion below then reads as a sorting bug in
    /// code that was never reached.
    fn bind_socket(runtime: &Path, name: &str) {
        let path = runtime.join(name);
        UnixListener::bind(&path)
            .unwrap_or_else(|error| panic!("{} is not bindable here: {error}", path.display()));
    }

    #[test]
    fn a_service_finds_the_compositor_it_was_never_told_about() {
        let runtime = runtime_dir("pick");
        // A `bus` and a `pipewire-0` sit beside the display in a real session;
        // neither is a candidate, and neither may be opened.
        for name in [
            "wayland-1",
            "wayland-0",
            "treeland.socket",
            "treeland-xwayland",
            "pipewire-0",
            "bus",
        ] {
            bind_socket(&runtime, name);
        }
        assert_sockets(&runtime, 6);

        assert_eq!(
            candidates(&runtime),
            vec![
                "wayland-0".to_owned(),
                "wayland-1".to_owned(),
                "treeland.socket".to_owned(),
            ],
            "the numbered sockets go first, and an X11 socket never"
        );

        std::fs::remove_dir_all(&runtime).expect("runtime directory goes away");
    }

    #[test]
    fn a_directory_without_a_display_offers_nothing() {
        let runtime = runtime_dir("none");
        bind_socket(&runtime, "nothing-to-see");
        assert_sockets(&runtime, 1);

        assert_eq!(
            candidates(&runtime),
            Vec::<String>::new(),
            "a socket that is not a display is never a candidate"
        );

        std::fs::remove_dir_all(&runtime).expect("runtime directory goes away");
    }

    /// Confirms the fixtures are there before the code under test is judged.
    fn assert_sockets(runtime: &Path, expected: usize) {
        let bound = std::fs::read_dir(runtime)
            .expect("the directory just created")
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_socket()))
            .count();
        assert_eq!(
            bound,
            expected,
            "the fixtures did not appear in {}",
            runtime.display()
        );
    }
}
