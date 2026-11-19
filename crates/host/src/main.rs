//! `AndroScreen` host daemon.
//!
//! The daemon waits for a phone to be plugged in over USB, takes it over with
//! the Android Open Accessory protocol, creates a virtual output with evdi and
//! streams the captured frames back to the phone.

use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::time::Duration;

use androscreen_core::protocol::{Control, Size, PROTOCOL_VERSION};
use anyhow::{Context, Result};

mod aoa;
mod audio;
mod brightness;
mod bus;
mod color;
mod cursor;
mod dde;
mod encoder;
mod evdi;
mod requests;
mod sampler;
mod scale;
mod session;
mod touchmap;
mod uinput;
mod wlr;
mod x11;

const USAGE: &str = "\
androscreen — use an Android phone as a secondary display

USAGE:
    androscreen [OPTIONS]

Without options the daemon waits for a phone, shows the statistics dashboard,
and turns the phone into an extended display on request.

OPTIONS:
    --stats        Print host system stats as JSON and exit
    --usb-scan     List attached USB devices and their accessory status
    --usb-probe    Switch a phone to accessory mode and wait for its greeting
    --evdi-scan    List evdi virtual display cards
    --layout-scan  Print the desktop layout the compositor reports
    --version      Print the version and exit
    -h, --help     Print this help and exit

ENVIRONMENT:
    RUST_LOG       Log filter, defaults to `info`
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

#[expect(
    clippy::print_stdout,
    reason = "a one-shot CLI reports its results on stdout"
)]
fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // A service unit starts outside the desktop's environment, and the
    // compositor's socket may carry the compositor's name rather than the
    // default one; the layout protocol is worth looking for either way.
    wlr::adopt_session();

    let mut want_stats = false;
    let mut want_scan = false;
    let mut want_probe = false;
    let mut want_evdi = false;
    let mut want_layout = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--version" => {
                println!("androscreen {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--stats" => want_stats = true,
            "--usb-scan" => want_scan = true,
            "--usb-probe" => want_probe = true,
            "--evdi-scan" => want_evdi = true,
            "--layout-scan" => want_layout = true,
            other => anyhow::bail!("unknown argument `{other}`; try --help"),
        }
    }

    if want_layout {
        print_layout();
        return Ok(());
    }

    if want_evdi {
        print_evdi_cards();
        return Ok(());
    }

    if want_scan {
        return print_usb_scan();
    }

    if want_probe {
        return probe_accessory();
    }

    if want_stats {
        let mut sampler = sampler::StatsSampler::new().context("reading host statistics")?;
        std::thread::sleep(Duration::from_millis(250));
        let stats = sampler.sample().context("reading host statistics")?;
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }

    let Some(_lock) = instance_lock()? else {
        log::info!("another androscreen instance is already running");
        return Ok(());
    };

    log::info!(
        "androscreen {} ready; waiting for a phone",
        env!("CARGO_PKG_VERSION")
    );
    requests::spawn_watcher();
    let mut sampler = sampler::StatsSampler::new().context("reading host statistics")?;
    session::run(&mut sampler)
}

/// Prints the desktop layout the compositor reports.
#[expect(
    clippy::print_stdout,
    reason = "a one-shot CLI reports its results on stdout"
)]
fn print_layout() {
    match wlr::scan() {
        Ok(text) => println!("{text}"),
        Err(error) => println!("no compositor layout: {error}"),
    }
}

/// Lists the evdi cards the kernel has, virtual displays included.
#[expect(
    clippy::print_stdout,
    reason = "a one-shot CLI reports its results on stdout"
)]
fn print_evdi_cards() {
    match evdi::EvdiCard::scan() {
        Ok(cards) if cards.is_empty() => println!("no evdi virtual display cards found"),
        Ok(cards) => {
            for card in cards {
                println!(
                    "{}  evdi {}  {}",
                    card.path.display(),
                    card.version,
                    card.connector.as_deref().unwrap_or("no connector"),
                );
            }
        }
        Err(error) => println!("evdi unavailable: {error}"),
    }
}

/// Lists attached USB devices and whether each one answers the AOA probe.
#[expect(
    clippy::print_stdout,
    reason = "a one-shot CLI reports its results on stdout"
)]
fn print_usb_scan() -> Result<()> {
    for entry in aoa::scan()? {
        println!(
            "bus {:>3} addr {:>3}  {}  class {:02x}  {:9}  {}",
            entry.bus,
            entry.address,
            entry.id(),
            entry.class,
            if entry.accessory { "accessory" } else { "" },
            if entry.readable {
                "readable"
            } else {
                "no access"
            },
        );
    }
    Ok(())
}

/// Switches a phone to accessory mode, answers its greeting and lets go.
#[expect(
    clippy::print_stdout,
    reason = "a one-shot CLI reports its results on stdout"
)]
fn probe_accessory() -> Result<()> {
    let mut link = aoa::connect(aoa::Takeover::Automatic)?;
    println!("accessory link: {:?}", link.info());
    let Some(hello) = aoa::wait_for_hello(&mut link, Duration::from_secs(20), || false)? else {
        anyhow::bail!("gave up waiting for the phone");
    };
    println!(
        "hello: protocol v{} app {} capabilities {:?}",
        hello.protocol, hello.app_version, hello.capabilities
    );
    link.send_control(&Control::Welcome {
        protocol: PROTOCOL_VERSION,
        host_version: env!("CARGO_PKG_VERSION").to_owned(),
        audio: false,
        max_screen: Size {
            width: 1920,
            height: 1080,
        },
    })?;
    println!("welcome sent; closing the link");
    Ok(())
}

/// Holds an exclusive lock so one user session never runs two daemons.
///
/// The lock deliberately does not live in the session runtime directory: `/run/user/<uid>`
/// is removed when the last session of that user ends, so a daemon that was still
/// shutting down kept a lock on an inode nobody could reach any more, a freshly
/// started instance locked a brand new file, and both then took the USB port.
/// The phone answered neither of them.
///
/// Returns `Ok(None)` when another instance owns the lock.
fn instance_lock() -> Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    // SAFETY: `getuid` only reads the calling process' user id.
    let uid = unsafe { libc::getuid() };
    let path = std::env::temp_dir().join(format!("androscreen-{uid}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: `file` owns a live descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(error).with_context(|| format!("locking {}", path.display()))
}
