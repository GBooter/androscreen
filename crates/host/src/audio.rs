//! Desktop audio capture through `PipeWire`'s `PulseAudio` compatibility layer.
//!
//! `parec` records the monitor of the default sink, so the phone hears exactly
//! what the desktop plays without touching the user's audio routing. The
//! stream is 48 kHz stereo `s16le`, sliced into 20 ms chunks for low latency.

use std::io::{self, Read};
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::Duration;

use thiserror::Error;

/// Samples per second on the wire.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channels.
pub(crate) const CHANNELS: u8 = 2;
/// Bytes per sample.
pub(crate) const BYTES_PER_SAMPLE: usize = 2;
/// Frames in one chunk: 20 ms.
pub(crate) const FRAMES_PER_CHUNK: usize = 960;
/// Bytes in one chunk.
pub(crate) const CHUNK_BYTES: usize = FRAMES_PER_CHUNK * (CHANNELS as usize) * BYTES_PER_SAMPLE;
/// Chunks queued before the reader blocks; keeps audio latency bounded.
const CHUNK_QUEUE: usize = 4;
/// Time the first chunk may take before the capture is declared broken.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
/// `PulseAudio`'s name for the monitor of the current default sink.
const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";
const PAREC: &str = "parec";

/// A running `parec` process feeding fixed-size PCM chunks.
#[derive(Debug)]
pub(crate) struct AudioCapture {
    child: Child,
    chunks: Receiver<Vec<u8>>,
    pending: Option<Vec<u8>>,
}

impl AudioCapture {
    /// Starts capturing the default sink's monitor.
    ///
    /// Startup is validated by waiting for the first chunk, so a missing
    /// server or sink is reported here instead of mid-stream.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::Unavailable`] when `parec` is not installed,
    /// [`AudioError::Spawn`] when it cannot be started,
    /// [`AudioError::StartupTimeout`] when no audio arrives, and
    /// [`AudioError::Exited`] when it quits during startup.
    pub(crate) fn spawn() -> Result<Self, AudioError> {
        let mut child = parec_command().spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                AudioError::Unavailable
            } else {
                AudioError::Spawn(error)
            }
        })?;
        let Some(stdout) = child.stdout.take() else {
            stop(&mut child);
            return Err(AudioError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "parec stdout was not piped",
            )));
        };

        let (sender, chunks) = mpsc::sync_channel(CHUNK_QUEUE);
        thread::spawn(move || read_chunks(stdout, &sender));

        let mut capture = Self {
            child,
            chunks,
            pending: None,
        };
        match capture.chunks.recv_timeout(STARTUP_TIMEOUT) {
            Ok(chunk) => {
                capture.pending = Some(chunk);
                log::info!(
                    "audio capture ready: {SAMPLE_RATE} Hz, {CHANNELS} channels, \
                     {FRAMES_PER_CHUNK} frames per chunk"
                );
                Ok(capture)
            }
            Err(RecvTimeoutError::Timeout) => {
                stop(&mut capture.child);
                Err(AudioError::StartupTimeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = capture.child.try_wait().ok().flatten();
                stop(&mut capture.child);
                Err(AudioError::Exited { status })
            }
        }
    }

    /// Returns the next chunk, waiting at most `timeout`.
    ///
    /// `Ok(None)` means the timeout expired; [`AudioError::Ended`] means the
    /// capture process is gone and should be restarted.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::Ended`] once `parec` stopped producing audio.
    pub(crate) fn next_chunk(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, AudioError> {
        if let Some(chunk) = self.pending.take() {
            return Ok(Some(chunk));
        }
        match self.chunks.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(AudioError::Ended),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        stop(&mut self.child);
    }
}

fn parec_command() -> Command {
    let mut command = Command::new(PAREC);
    command
        .arg(format!("--device={DEFAULT_MONITOR}"))
        .arg("--format=s16le")
        .arg(format!("--rate={SAMPLE_RATE}"))
        .arg(format!("--channels={CHANNELS}"))
        .arg("--latency-msec=20")
        .arg("--client-name=androscreen")
        .arg("--raw")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn read_chunks(mut stdout: ChildStdout, sender: &SyncSender<Vec<u8>>) {
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        match stdout.read_exact(&mut buffer) {
            Ok(()) => {
                if sender.send(buffer.clone()).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Failure while recording the default sink.
#[derive(Debug, Error)]
pub(crate) enum AudioError {
    /// `parec` is not installed; `PipeWire` or `PulseAudio` tools are needed.
    #[error("`parec` not found; install pulseaudio-utils or pipewire-pulse")]
    Unavailable,
    /// The process could not be started.
    #[error("starting parec failed: {0}")]
    Spawn(#[source] io::Error),
    /// No audio arrived while starting up, usually because no server or sink
    /// is available.
    #[error("no audio from the default sink after {STARTUP_TIMEOUT:?}")]
    StartupTimeout,
    /// `parec` quit while starting up.
    #[error("audio capture exited during startup ({status:?})")]
    Exited {
        /// Exit status, when the process could be reaped.
        status: Option<ExitStatus>,
    },
    /// `parec` stopped while streaming.
    #[error("audio capture stopped")]
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_covers_twenty_milliseconds() {
        assert_eq!(CHUNK_BYTES, 3_840);
        assert_eq!(
            FRAMES_PER_CHUNK as f64 / f64::from(SAMPLE_RATE),
            Duration::from_millis(20).as_secs_f64()
        );
    }

    #[test]
    fn command_records_the_default_monitor() {
        let command = parec_command();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--device=@DEFAULT_MONITOR@");
        assert!(args.iter().any(|arg| arg == "--format=s16le"));
        assert!(args.iter().any(|arg| arg == "--rate=48000"));
        assert!(args.iter().any(|arg| arg == "--channels=2"));
        assert!(args.iter().any(|arg| arg == "--raw"));
    }

    #[test]
    fn captures_audio_when_the_server_is_available() {
        let mut capture = match AudioCapture::spawn() {
            Ok(capture) => capture,
            Err(error) => {
                eprintln!("skipping: {error}");
                return;
            }
        };
        let chunk = capture
            .next_chunk(Duration::from_secs(3))
            .expect("capture is alive")
            .expect("a chunk arrives");
        assert_eq!(chunk.len(), CHUNK_BYTES);
    }
}
