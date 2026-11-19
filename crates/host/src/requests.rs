//! Carries the settings window's words into the session loop, and keeps the
//! state file the window reads fresh.
//!
//! The window drops a request file; this module turns it into a flag so the
//! loop never blocks on the file system mid-frame.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use androscreen_core::control::{self, Request, State};

/// The window asked for the USB port back.
static RELEASE: AtomicBool = AtomicBool::new(false);
/// The window asked for the next phone to be taken over regardless of policy.
static FORCE: AtomicBool = AtomicBool::new(false);
/// What the daemon is doing, as the window sees it.
static STATE: OnceLock<Mutex<State>> = OnceLock::new();
/// When the session loop last reached a point of its own, in milliseconds since
/// [`beat_zero`].
static BEAT: AtomicU64 = AtomicU64::new(0);

/// How often the request file is looked at.
///
/// This is the first half of the wait between a click on the settings window and
/// the cable being taken over, so it stays short; reading one small file is
/// cheaper than the user noticing nothing happened.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);
/// How often the state file is refreshed, so the window can tell the daemon is
/// alive even while a session keeps the loop busy.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// How long the loop may go without reaching any of its own points before the
/// window stops being told it is alive.
///
/// Every wait the loop owns is shorter than this — the longest is a compositor
/// being asked to place an output, at forty seconds — so a longer silence means
/// the loop is stuck somewhere that cannot answer a button. Saying "running"
/// then is what leaves a person pressing buttons that do nothing.
const LOOP_SILENCE: Duration = Duration::from_secs(60);

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn beat_zero() -> Instant {
    static ZERO: OnceLock<Instant> = OnceLock::new();
    *ZERO.get_or_init(Instant::now)
}

/// Marks the session loop as having reached a point of its own.
///
/// Called from the loop's own passes rather than from inside the screen session,
/// so a session that is streaming for hours still proves it is alive: the phone
/// sees its picture, and the window must keep seeing a daemon it can talk to.
pub(crate) fn beat() {
    let elapsed = u64::try_from(beat_zero().elapsed().as_millis()).unwrap_or(u64::MAX);
    BEAT.store(elapsed, Ordering::Relaxed);
}

/// Whether the loop has reached a point of its own recently enough.
fn beating() -> bool {
    let now = u64::try_from(beat_zero().elapsed().as_millis()).unwrap_or(u64::MAX);
    let last = BEAT.load(Ordering::Relaxed);
    now.saturating_sub(last) < u64::try_from(LOOP_SILENCE.as_millis()).unwrap_or(u64::MAX)
}

/// Changes the published state.
pub(crate) fn update(change: impl FnOnce(&mut State)) {
    let mut guard = state().lock().expect("the state lock is never poisoned");
    change(&mut guard);
}

/// Watches for requests and keeps the state file fresh for as long as the
/// daemon runs.
pub(crate) fn spawn_watcher() {
    std::thread::spawn(|| loop {
        match control::take_request() {
            Some(Request::Release) => {
                log::debug!("the settings window asked for the port back");
                RELEASE.store(true, Ordering::Relaxed);
            }
            Some(Request::Enable) => {
                log::debug!("the settings window asked for a takeover");
                FORCE.store(true, Ordering::Relaxed);
            }
            None => {}
        }
        std::thread::sleep(WATCH_INTERVAL);
    });

    std::thread::spawn(|| {
        let mut was_beating = true;
        loop {
            if beating() {
                let snapshot = state()
                    .lock()
                    .expect("the state lock is never poisoned")
                    .clone();
                control::publish(&snapshot);
                was_beating = true;
            } else if was_beating {
                // The loop is no longer reaching its own points, so what the
                // window last read is no longer true of anything: publishing it
                // again would show a phone that has gone silent and a service
                // that answers no button. Left to go stale, the window says the
                // service is not responding and points at the restart.
                log::warn!("the session loop has stopped answering; the window is told so");
                was_beating = false;
            }
            std::thread::sleep(HEARTBEAT);
        }
    });
}

/// Whether the window asked for the port to be handed back.
pub(crate) fn release_requested() -> bool {
    RELEASE.load(Ordering::Relaxed)
}

/// Clears the release request once it has been carried out.
pub(crate) fn release_done() {
    RELEASE.store(false, Ordering::Relaxed);
}

/// Whether the window asked for the next phone to be taken over.
pub(crate) fn take_force() -> bool {
    FORCE.swap(false, Ordering::Relaxed)
}

/// Whether a takeover is waiting to be carried out.
///
/// The session loop sleeps between attempts; this is how a click wakes it early
/// instead of being noticed only when the wait happens to end. A release is not
/// worth waking for: it can only mean something while a phone is connected, and
/// that path checks it on every pass.
pub(crate) fn takeover_pending() -> bool {
    FORCE.load(Ordering::Relaxed)
}
