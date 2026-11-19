//! Asks an X11 desktop to show the virtual output in the shape just built for it.
//!
//! The server keeps the mode an output was already showing, so a phone that
//! turns leaves the desktop in the shape it had before and the session times out
//! waiting for the one it asked for. Naming the mode is what breaks that tie,
//! and it is self-checking as well: a mode the server has not read from the new
//! EDID yet simply is not there to select, so a refused ask is retried until the
//! re-probe lands.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use androscreen_core::edid::DisplayMode;

/// How often the ask is repeated while the server notices the connector.
const INTERVAL: Duration = Duration::from_millis(200);
/// How many times the raw ask is made before the desktop is left to its own
/// answer. Each one is a modeset, and a desktop that is laying out the same
/// monitor at the same time takes a modeset as a reason to lay it out again.
const ATTEMPTS: usize = 8;
/// How many times the layout owner is asked for the mode before the raw ask is
/// tried instead. The service refuses while it is still laying a monitor it has
/// just seen arrive, and one refused ask used to be the end of the polite way.
const SERVICE_ATTEMPTS: usize = 4;
/// How long an arriving monitor is watched for the layout the desktop already
/// had on its way, before the owner is asked for the shape the phone wants.
///
/// A monitor the desktop has just seen arrive is laid out twice: our ask lands
/// first, then the shape the service prefers for that monitor lands a second or
/// two later and undoes it. Asking again the moment the listing stops wearing
/// the ask is asking after that layout, which is the ask that survives; asking
/// before it is what used to lose the monitor's shape until the session was
/// restarted.
const ARRIVED_WAIT: Duration = Duration::from_millis(6000);
/// How long the layout owner is given to wear the ask before it is asked the
/// raw way as well. It is re-laying out every monitor while it answers.
const SETTLE: Duration = Duration::from_millis(6000);
/// How long the desktop is given to list a connector that just lit up.
const NAME_WAIT: Duration = Duration::from_millis(3000);
/// How long the same `xrandr` complaint is kept out of the journal: long enough
/// that a failing poll does not bury the rest of the log, short enough that a
/// display that comes back is noticed.
const COMPLAINT_INTERVAL: Duration = Duration::from_secs(30);
/// How long the server is left to take a removed output out of its layout, and
/// how often that is asked.
const REMOVAL_INTERVAL: Duration = Duration::from_millis(100);
const REMOVAL_LIMIT: Duration = Duration::from_secs(5);
/// How long one `xrandr` call is waited for before it is killed.
///
/// An answer is normally instant, but a desktop that is laying its whole canvas
/// out again answers late — and once answered an asking process only after
/// minutes, holding the session loop it was called from for all of it. The
/// caller treats "no answer" as "did not happen" and carries on.
const CALL_LIMIT: Duration = Duration::from_secs(10);
/// How often a running `xrandr` call is checked on.
const CALL_POLL: Duration = Duration::from_millis(20);
/// How long the quiet-screen layout probe may take.
///
/// The probe is read while the session waits on a still desktop, and it runs on
/// the session loop, so it has to give up well inside the moment a phone would
/// notice that its touches and sound stopped. A desktop that is re-laying its
/// canvas out answers later than that; the next probe asks again.
const PROBE_LIMIT: Duration = Duration::from_millis(1200);

/// Which ask is the newest one.
///
/// A turn, or a display the desktop dropped, tears the virtual display down and
/// builds it again; each build starts its own ask thread. The thread of the
/// screen just replaced is still running, and the EDID it was told about is
/// already gone: its later asks can only fail, or undo the shape the new screen
/// just took. Only the newest generation may keep asking.
static NEWEST_ASK: AtomicU64 = AtomicU64::new(0);

/// Stops every ask whose screen is being taken down.
///
/// The output is about to disappear, so naming a mode on it can only fail or
/// race whatever the next screen asks for.
pub(crate) fn cancel_asks() {
    NEWEST_ASK.fetch_add(1, Ordering::Relaxed);
}

/// Why the desktop is being asked for a mode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ask {
    /// A connector just appeared: the display service has to lay the new
    /// monitor out itself, or it will keep putting its preferred mode back.
    Arrived,
    /// The phone turned. Only one output changes shape. A service-wide
    /// `ApplyChanges` re-lays out every monitor, rewrites their brightness
    /// and holds this ask for seconds while it settles; a single-output
    /// `xrandr` mode set is enough here.
    Turned,
}

/// Whether this desktop's X server lists the output a kernel connector became.
///
/// A server that has the connector in its layout is the one that will lay that
/// monitor out, whatever else the session's environment mentions.
pub(crate) fn lists_output(kernel_name: &str) -> bool {
    listing().is_some_and(|listing| output_line(&listing, kernel_name).is_some())
}

/// Waits until the X desktop has finished forgetting the connector that was
/// just taken away.
///
/// A removal and a connection in the same breath look like one event to the
/// server, and for one event it restores what it was showing rather than reading
/// the new EDID — so a phone that turned gets the shape it had before, and the
/// mode it asked for never appears. Once the output has left the layout the
/// next connection is an addition, and its EDID is read.
///
/// A desktop that does not answer at all is left alone: the wait is only ever a
/// detour on the way to the new display.
pub(crate) fn await_removal(kernel_name: &str) {
    let deadline = Instant::now() + REMOVAL_LIMIT;
    let mut waited = false;
    loop {
        let Some(listing) = listing() else { return };
        if forgotten(&listing, kernel_name) {
            if waited {
                log::debug!("{kernel_name} has left the X desktop's layout");
            }
            return;
        }
        if Instant::now() >= deadline {
            log::debug!("the X desktop still holds {kernel_name} in its layout");
            return;
        }
        waited = true;
        sleep(REMOVAL_INTERVAL);
    }
}

/// Selects the mode the virtual output should be showing, on its own thread so
/// a slow desktop cannot hold up the phone's first frame.
///
/// The desktop that owns the layout is asked first: a raw `xrandr` selection
/// works only until the display service lays out a monitor it has just seen
/// arrive, and it lays it out from the EDID rather than from what the phone
/// wanted. The raw ask is the fallback for a desktop with no service, and it
/// names the place as well as the mode because an output left at `+0+0` scans
/// out the same part of the desktop as the laptop's own panel — which is the
/// phone showing a crop of the screen it is meant to extend.
///
/// Neither ask is repeated until the listing says the output wears it, because
/// `xrandr` exits happily for a mode it has merely been told about.
pub(crate) fn request(kernel_name: String, width: u32, height: u32, how: Ask) {
    let generation = NEWEST_ASK.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let current = move || NEWEST_ASK.load(Ordering::Relaxed) == generation;
    std::thread::spawn(move || {
        let mode = format!("{width}x{height}");
        let Some(name) = wait_for_name(&kernel_name, &current) else {
            if current() {
                log::warn!("the X desktop never listed {kernel_name}");
            }
            return;
        };
        if !current() {
            return;
        }
        if wearing_now(&kernel_name, &mode) {
            return;
        }
        // Never ask a *new* output to stand at the origin: an output at +0+0
        // scans out the laptop's own panel, which is the phone showing a crop
        // of the wrong screen. A turn is different -- the output is already in
        // the layout, and setting only its mode keeps the place it has. A
        // listing that cannot be measured is no reason to drop the request:
        // doing so used to leave the phone waiting for a shape the compositor
        // was never asked to produce.
        let mut at = placement(&kernel_name);
        if at.is_none() {
            if how != Ask::Turned {
                log::warn!("the X desktop has no layout for {kernel_name} to extend");
                return;
            }
            log::debug!("no layout to measure; setting {mode} without moving the output");
        }
        let display = DisplayMode {
            width,
            height,
            refresh_hz: 60,
        };
        let mut worn = names_the_mode(&kernel_name, &name, &mode, &mut at, &current);
        if how == Ask::Arrived {
            // The desktop's own layout for this new monitor is still on its
            // way, and it undoes whatever the mode is when it lands. Naming the
            // mode again once it has landed is naming it last, which is what
            // makes it stick: the ask that arrived first always loses.
            let deadline = Instant::now() + ARRIVED_WAIT;
            let mut undone = false;
            while Instant::now() < deadline {
                if !current() {
                    return;
                }
                if !wearing_now(&kernel_name, &mode) {
                    undone = true;
                    log::debug!("the desktop laid {kernel_name} out its own way");
                    break;
                }
                sleep(INTERVAL);
            }
            if !undone {
                // The desktop took the shape as it was: nothing to repair, and
                // no reason to make it lay the whole desktop out again.
                return;
            }
            worn = names_the_mode(&kernel_name, &name, &mode, &mut at, &current);
            if worn {
                return;
            }
        } else if worn {
            // A turn that stuck needs no service ask: making the layout owner
            // re-lay out every monitor costs it a brightness pass each time.
            return;
        }
        if at.is_some_and(|at| ask_service(&name, display, at, &current))
            && worn_for(&kernel_name, &mode, SETTLE, &current)
        {
            log_the_room(&kernel_name);
            return;
        }
        if !current() {
            return;
        }
        if wearing_now(&kernel_name, &mode) {
            return;
        }
        log::warn!("the X desktop never showed {kernel_name} at {mode}");
    });
}

/// Names `mode` on the output until the desktop's own listing says it wears it.
///
/// `xrandr` exits happily for a mode it has merely been told about, so the
/// listing is the only answer that counts.
fn names_the_mode(
    kernel_name: &str,
    name: &str,
    mode: &str,
    at: &mut Option<(i32, i32)>,
    current: &dyn Fn() -> bool,
) -> bool {
    for attempt in 0..ATTEMPTS {
        if !current() {
            return false;
        }
        *at = placement(kernel_name).or(*at);
        match show(name, mode, *at) {
            Ok(()) if wearing_now(kernel_name, mode) => {
                if attempt > 0 {
                    log::debug!(
                        "the X desktop took the virtual display on attempt {}",
                        attempt + 1
                    );
                }
                log_the_room(kernel_name);
                return true;
            }
            Ok(()) => {}
            Err(problem) => log::debug!("cannot show {mode} on {kernel_name} yet: {problem}"),
        }
        sleep(INTERVAL);
    }
    false
}

/// Asks the service that owns the layout for `display`, retrying a refusal.
///
/// A refusal here is not an answer about the mode: it is what the service says
/// while it is still busy laying out the monitor it has just been told about.
fn ask_service(
    name: &str,
    display: DisplayMode,
    at: (i32, i32),
    current: &dyn Fn() -> bool,
) -> bool {
    for attempt in 0..SERVICE_ATTEMPTS {
        if !current() {
            return false;
        }
        if crate::dde::ask(name, display, at) {
            return true;
        }
        if attempt + 1 < SERVICE_ATTEMPTS {
            sleep(INTERVAL);
        }
    }
    false
}

/// Waits for the desktop to list the output this connector became.
///
/// The ask is made the moment the connector lights up, and a server that is
/// still reading the new EDID has no name for the output yet.
fn wait_for_name(kernel_name: &str, current: &dyn Fn() -> bool) -> Option<String> {
    let deadline = Instant::now() + NAME_WAIT;
    loop {
        if !current() {
            return None;
        }
        if let Some(name) = output_name(kernel_name) {
            return Some(name);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(INTERVAL);
    }
}

/// Whether the output comes to wear `mode` within `settle`.
fn worn_for(kernel_name: &str, mode: &str, settle: Duration, current: &dyn Fn() -> bool) -> bool {
    let deadline = Instant::now() + settle;
    while Instant::now() < deadline {
        if !current() {
            return false;
        }
        if wearing_now(kernel_name, mode) {
            return true;
        }
        sleep(INTERVAL);
    }
    wearing_now(kernel_name, mode)
}

/// Whether the desktop's own listing has this output wearing `mode` in a place
/// of its own, so a head that merely offers the mode is not mistaken for one
/// wearing it and a desktop that stacked it over the panel is not mistaken for
/// one that extended the desktop.
fn wearing_now(kernel_name: &str, mode: &str) -> bool {
    let Some(listing) = listing() else {
        return false;
    };
    wearing(&listing, kernel_name, mode) && !overlapping(&listing, kernel_name)
}

/// Whether the listing places the output this kernel connector became at
/// `mode`, position included.
fn wearing(listing: &str, kernel_name: &str, mode: &str) -> bool {
    let Some(line) = output_line(listing, kernel_name) else {
        return false;
    };
    let wanted = format!("{mode}+");
    line.split_whitespace()
        .any(|word| word.starts_with(&wanted))
}

/// Where the virtual output goes: flush against the right edge of everything
/// else on the desktop.
///
/// A constant screen box big enough for both phone shapes stops X from
/// reallocating the framebuffer on a turn, but it leaves a wide strip with no
/// monitor in it. The pointer vanishes in that strip and no window manager
/// will carry a window across it. Sitting beside the panel makes the two
/// monitors one continuous desktop, so windows drag across and the pointer
/// stays visible. The cost is one framebuffer reallocation per turn: a brief
/// blank of the laptop panel, not a broken extension.
fn placement(kernel_name: &str) -> Option<(i32, i32)> {
    let listing = listing()?;
    let (right, _bottom) = extent(&listing, kernel_name)?;
    Some((i32::try_from(right).unwrap_or(i32::MAX), 0))
}

/// The box the other live outputs fill, and where their right edge is.
fn extent(listing: &str, except: &str) -> Option<(u32, u32)> {
    let ours = name_of(output_line(listing, except));
    let mut right = 0_u32;
    let mut bottom = 0_u32;
    for line in listing.lines().filter(|line| !line.starts_with(' ')) {
        if name_of(Some(line)) == ours {
            continue;
        }
        let Some((x, y, width, height)) = placed(line) else {
            continue;
        };
        right = right.max(x.saturating_add(width));
        bottom = bottom.max(y.saturating_add(height));
    }
    (right > 0 && bottom > 0).then_some((right, bottom))
}

/// Whether the output this connector became shares pixels with another one: two
/// monitors laid over each other show each other's part of the desktop.
fn overlapping(listing: &str, kernel_name: &str) -> bool {
    let Some(ours) = output_line(listing, kernel_name) else {
        return false;
    };
    let Some((our_x, our_y, our_width, our_height)) = placed(ours) else {
        return false;
    };
    let our_name = name_of(Some(ours));
    listing
        .lines()
        .filter(|line| !line.starts_with(' '))
        .filter(|line| name_of(Some(line)) != our_name)
        .filter_map(placed)
        .any(|(x, y, width, height)| {
            x < our_x + our_width
                && our_x < x + width
                && y < our_y + our_height
                && our_y < y + height
        })
}

/// The X name on an output line of the listing.
fn name_of(line: Option<&str>) -> Option<&str> {
    line?.split_whitespace().next()
}

/// The corner and size an output was given, whether or not the desktop also
/// calls it connected: an output a CRTC is still driving keeps its place in the
/// layout, and a box measured without it is a box the desktop outgrew the moment
/// it lights up again.
fn placed(line: &str) -> Option<(u32, u32, u32, u32)> {
    line.split_whitespace().find_map(parse_geometry)
}

/// The arguments `xrandr` is given: a position only when the layout gave one.
///
/// A turn sets the mode of an output the layout already holds, so leaving
/// `--pos` out keeps the place it has. A newly arrived output must always be
/// placed, or X puts it at the origin over the laptop's own panel.
fn show_args(name: &str, mode: &str, at: Option<(i32, i32)>) -> Vec<String> {
    let mut args = vec![
        "--output".to_owned(),
        name.to_owned(),
        "--mode".to_owned(),
        mode.to_owned(),
    ];
    if let Some((x, y)) = at {
        args.push("--pos".to_owned());
        args.push(format!("{x}x{y}"));
    }
    args
}

/// Names the mode for the X server, or says why the desktop is not ready yet.
fn show(kernel_name: &str, mode: &str, at: Option<(i32, i32)>) -> Result<(), String> {
    let name = output_name(kernel_name).ok_or("the desktop has not listed the output")?;
    let answer = run(&show_args(&name, mode, at), CALL_LIMIT)
        .ok_or_else(|| "xrandr did not answer".to_owned())?;
    if answer.status.success() {
        return Ok(());
    }
    Err(String::from_utf8_lossy(&answer.stderr).trim().to_owned())
}

/// The screen's current box, from the first line of `xrandr --query`.
fn screen_size(listing: &str) -> Option<(u32, u32)> {
    let current = listing.lines().next()?.split("current ").nth(1)?;
    let mut numbers = current
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty());
    let width = numbers.next()?.parse().ok()?;
    let height = numbers.next()?.parse().ok()?;
    Some((width, height))
}

/// Says how big a screen the desktop is holding now that the output has taken
/// the shape asked for.
///
/// A turn that lands in a box the same size as the one before it is only a mode
/// set; one that outgrows it makes X reallocate the whole framebuffer, and that
/// is what goes dark on every monitor. The two read the same in every other
/// respect, so this is the one line that says which of them happened.
fn log_the_room(kernel_name: &str) {
    let Some(listing) = listing() else { return };
    if let Some((width, height)) = screen_size(&listing) {
        log::debug!("the desktop holds {kernel_name} in a {width}x{height} screen");
    }
}

/// The corner and size an output occupies, as `1344x3064+3200+0`.
fn parse_geometry(token: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = token.split('+');
    let (width, height) = parts.next()?.split_once('x')?;
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    Some((x, y, width.parse().ok()?, height.parse().ok()?))
}

/// The name the X server gives this connector, which is not the kernel's: the
/// evdi output is `DVI-I-1` in `/sys` and `DVI-I-1-1` on a desktop whose first
/// card has taken the plain name. `xrandr` answers an unknown output with a
/// warning and a successful exit, so asking with the kernel's name would look
/// like it worked.
fn output_name(kernel_name: &str) -> Option<String> {
    let listing = listing()?;
    let line = output_line(&listing, kernel_name)?;
    line.split_whitespace().next().map(str::to_owned)
}

/// What `xrandr --query` printed, if this desktop has an X server answering.
///
/// A failure is reported instead of swallowed: a daemon started without
/// `DISPLAY` fails here on every single call, and a silent `None` reads exactly
/// like "the desktop has not listed the connector yet" — which is what made
/// that outage look like a timing problem for three rounds of debugging.
fn listing() -> Option<String> {
    listing_within(CALL_LIMIT)
}

/// The same read, but given up on after `limit`.
fn listing_within(limit: Duration) -> Option<String> {
    let output = run(&["--query".to_owned()], limit)?;
    if !output.status.success() {
        complain(&format!(
            "xrandr --query failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `xrandr` with a bound on how long its answer is waited for.
///
/// `Command::output` waits for as long as the server takes, and this server has
/// been seen taking minutes while a display service re-laid the canvas out. The
/// process is killed at the deadline instead, and the caller reads `None` as
/// "the desktop did not do it".
fn run(args: &[String], limit: Duration) -> Option<std::process::Output> {
    let mut child = match Command::new("xrandr")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            complain(&format!("could not run xrandr: {error}"));
            return None;
        }
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Err(_) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    complain(&format!("xrandr did not answer within {limit:?}"));
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                sleep(CALL_POLL);
            }
        }
    }
}

/// Says why the listing failed, at most once per [`COMPLAINT_INTERVAL`].
///
/// The name poll asks five times a second, so an unreachable display would
/// otherwise fill the journal with the same line and hide everything else.
fn complain(why: &str) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().expect("the complaint clock is not poisoned");
    if last.is_some_and(|at| at.elapsed() < COMPLAINT_INTERVAL) {
        return;
    }
    *last = Some(Instant::now());
    log::warn!("{why}");
}

/// The line the listing has for the output this kernel connector became.
fn output_line<'a>(listing: &'a str, kernel_name: &str) -> Option<&'a str> {
    let prefixed = format!("{kernel_name}-");
    listing
        .lines()
        .filter(|line| !line.starts_with(' '))
        .find(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|name| name == kernel_name || name.starts_with(&prefixed))
        })
}

/// Whether the desktop has let the output go: the listing either no longer has
/// it, or says it is disconnected without the mode and position it wore before.
///
/// A bare `disconnected` is the state the server reaches once it has finished
/// taking the head out of the layout; `disconnected 1080x2460+3200+0` is the
/// same output still waiting for that, and a connection made while it is there
/// reads as the old head coming back rather than a new one arriving.
fn forgotten(listing: &str, kernel_name: &str) -> bool {
    let Some(line) = output_line(listing, kernel_name) else {
        return true;
    };
    let mut words = line.split_whitespace().skip(1);
    let gone = words.next() == Some("disconnected");
    let bare = words.next().is_none_or(|shape| shape.starts_with('('));
    gone && bare
}

/// Whether the desktop took the output out of its layout while the connector
/// stayed attached.
///
/// The display service lays the monitors it knows about out a moment after a
/// session starts, and a monitor it has no saved place for leaves the layout
/// even though the connector and its EDID never went away. Nothing on the evdi
/// card reports that, so the session has to read it from the desktop's own
/// listing.
pub(crate) fn dropped(kernel_name: &str) -> bool {
    listing_within(PROBE_LIMIT).is_some_and(|listing| dropped_in(&listing, kernel_name))
}

/// Whether the listing drives no pixels for this output.
///
/// A desktop that switches a head off leaves it in the listing as
/// `connected` or `disconnected` with no corner and size, which is not a
/// monitor anyone is looking at.
fn dropped_in(listing: &str, kernel_name: &str) -> bool {
    match output_line(listing, kernel_name) {
        Some(line) => placed(line).is_none(),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dropped_in, extent, forgotten, output_line, overlapping, screen_size, show_args, wearing,
    };

    const STALE: &str = "Screen 0: minimum 320 x 200, current 3200 x 2000\n\
        eDP-1 connected primary 3200x2000+0+0 (normal) 336mm x 210mm\n   3200x2000 90.00*+\n\
        DVI-I-1-1 disconnected 1080x2460+3200+0 (normal) 0mm x 0mm\n";
    const GONE: &str = "Screen 0: minimum 320 x 200, current 3200 x 2000\n\
        eDP-1 connected primary 3200x2000+0+0 (normal) 336mm x 210mm\n\
        DVI-I-1-1 disconnected (normal left inverted right x axis y axis)\n";
    const PORTRAIT: &str = "Screen 0: minimum 320 x 200, current 4544 x 3064, \
        maximum 16384 x 16384\n\
        eDP-1 connected primary 3200x2000+0+0 (normal) 336mm x 210mm\n   3200x2000 90.00*+\n\
        DVI-I-1-1 connected 1344x3064+3200+0 (normal) 149mm x 340mm\n";
    const STACKED: &str = "Screen 0: minimum 320 x 200, current 6264 x 2000, \
        maximum 16384 x 16384\n\
        eDP-1 connected primary 3200x2000+0+0 (normal) 336mm x 210mm\n\
        DVI-I-1-1 disconnected 3064x1344+0+0 (normal) 149mm x 340mm\n";
    /// The listing after the desktop switched the head off with the connector
    /// still attached.
    const SWITCHED_OFF: &str = "Screen 0: minimum 320 x 200, current 3200 x 2000\n\
        eDP-1 connected primary 3200x2000+0+0 (normal) 336mm x 210mm\n\
        DVI-I-1-1 connected (normal left inverted right x axis y axis)\n";

    #[test]
    fn a_head_that_drives_no_pixels_is_dropped() {
        assert!(
            dropped_in(SWITCHED_OFF, "DVI-I-1"),
            "a head the desktop switched off drives nothing",
        );
        assert!(
            dropped_in(GONE, "DVI-I-1"),
            "a head taken out of the layout is dropped",
        );
        assert!(
            !dropped_in(PORTRAIT, "DVI-I-1"),
            "a head beside the panel is held",
        );
        assert!(
            !dropped_in(STACKED, "DVI-I-1"),
            "a head still at its old place is held until the desktop lets it go",
        );
    }

    #[test]
    fn reads_the_prefixed_name_of_the_kernel_connector() {
        let line = output_line(STALE, "DVI-I-1").expect("the desktop lists the output");
        assert!(
            line.starts_with("DVI-I-1-1 "),
            "the plain kernel name belongs to another card",
        );
    }

    #[test]
    fn a_head_that_is_only_switched_off_is_not_forgotten() {
        assert!(
            !forgotten(STALE, "DVI-I-1"),
            "the layout still carries the old shape",
        );
        assert!(forgotten(GONE, "DVI-I-1"), "the head has left the layout");
        assert!(
            forgotten("eDP-1 connected 3200x2000+0+0 (normal)\n", "DVI-I-1"),
            "an output the desktop never listed is forgotten already",
        );
    }

    #[test]
    fn the_screen_box_is_read_from_the_head_of_the_listing() {
        assert_eq!(screen_size(PORTRAIT), Some((4544, 3064)));
        assert_eq!(screen_size("Screen 0: minimum 320 x 200\n"), None);
    }

    #[test]
    fn a_head_is_only_worn_once_the_listing_places_it() {
        assert!(wearing(PORTRAIT, "DVI-I-1", "1344x3064"));
        assert!(
            wearing(PORTRAIT, "eDP-1", "3200x2000"),
            "the primary flag sits between the state and the place",
        );
        assert!(
            !wearing(PORTRAIT, "DVI-I-1", "3064x1344"),
            "a mode the head merely offers is not the one it wears",
        );
        assert!(!wearing(GONE, "DVI-I-1", "1344x3064"));
    }

    #[test]
    fn a_phone_that_sits_beside_the_panel_does_not_stack_on_it() {
        // PORTRAIT has the virtual head at +3200+0, flush with the panel's
        // right edge. There is no gap for the pointer to disappear into and
        // no overlap that would show the laptop's own desktop on the phone.
        assert_eq!(extent(PORTRAIT, "DVI-I-1"), Some((3200, 2000)));
        assert!(!overlapping(PORTRAIT, "DVI-I-1"));
    }

    #[test]
    fn a_head_laid_over_another_shows_that_screen_s_desktop() {
        assert!(
            overlapping(STACKED, "DVI-I-1"),
            "the virtual output sits on top of the laptop's own panel",
        );
        assert!(!overlapping(PORTRAIT, "DVI-I-1"));
        assert!(
            !overlapping(GONE, "DVI-I-1"),
            "an unplaced head overlaps nothing"
        );
    }

    #[test]
    fn the_ask_places_the_output_beyond_the_desktop_it_extends() {
        assert_eq!(extent(PORTRAIT, "DVI-I-1"), Some((3200, 2000)));
        assert_eq!(extent(GONE, "DVI-I-1"), Some((3200, 2000)));
    }

    #[test]
    fn a_turn_without_a_measured_layout_still_selects_the_mode() {
        let args = show_args("DVI-I-1-1", "3064x1344", None);
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--mode" && pair[1] == "3064x1344"),
            "the turn must still ask the desktop for the other mode",
        );
        assert!(
            !args.iter().any(|arg| arg == "--pos"),
            "an output already in the layout must not be moved to the origin",
        );
    }

    #[test]
    fn a_measured_layout_places_the_output_beside_the_panel() {
        let args = show_args("DVI-I-1-1", "3064x1344", Some((3200, 0)));
        assert_eq!(args.last().map(String::as_str), Some("3200x0"));
    }
}
