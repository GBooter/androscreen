//! H.264 encoding through an ffmpeg subprocess.
//!
//! Frames enter ffmpeg as raw pixels on stdin and come back as an Annex-B
//! elementary stream on stdout. A reader thread reframes that stream into
//! individual NAL units so the session can forward them as protocol frames
//! while the encoder keeps running.
//!
//! The encoder is started with two synthetic black frames so the SPS/PPS pair
//! is available before the first real frame reaches the phone — a picture only
//! leaves once the next one begins — and so a backend that dies on startup is
//! detected immediately instead of after the first desktop update.

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use androscreen_core::protocol::Size;

use crate::evdi::Mode;

/// VAAPI render node used for hardware encoding.
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// How long the self-test waits for SPS/PPS and a keyframe.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the self-test waits for a frame before feeding another one.
const HANDSHAKE_PEEK: Duration = Duration::from_millis(250);

/// Encoder that ffmpeg provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// `h264_vaapi` on the first render node.
    Vaapi,
    /// `libx264` with zero-latency tuning.
    Software,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vaapi => f.write_str("h264_vaapi"),
            Self::Software => f.write_str("libx264"),
        }
    }
}

/// One coded picture, ready for the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Nal {
    /// Start codes and NAL bodies of the whole picture.
    pub bytes: Vec<u8>,
    keyframe: bool,
}

impl Nal {
    /// Whether this picture is an IDR, which a decoder can start from.
    #[must_use]
    pub(crate) const fn is_keyframe(&self) -> bool {
        self.keyframe
    }
}

/// Something the encoder produced that the session needs.
#[derive(Debug)]
enum Coded {
    /// The SPS/PPS pair, already formatted as Annex-B with start codes.
    Csd(Vec<u8>),
    /// One whole coded picture.
    Picture(Nal),
}

/// Splits ffmpeg's FLV output into pictures and the decoder configuration.
///
/// Raw H.264 over a pipe has no packet boundaries: a picture is only proved
/// complete by the start code of the next one, which holds every frame on the
/// wire for a whole frame interval. FLV tags carry their length, so a picture
/// leaves as soon as the encoder finishes it. The payload is AVCC, with
/// length-prefixed NALs, which this reader turns back into the Annex-B the
/// phone's decoder is configured for.
#[derive(Debug, Default)]
struct FlvReader {
    buffer: Vec<u8>,
    started: bool,
    /// Bytes per NAL length prefix, from the sequence header.
    nal_length_size: usize,
}

/// Bytes of the FLV file header, before its first tag.
const FLV_HEADER_LEN: usize = 9;
/// Bytes of a tag header, before its body.
const FLV_TAG_HEADER_LEN: usize = 11;
/// Video tag carrying the decoder configuration.
const AVC_SEQUENCE_HEADER: u8 = 0;
/// Video tag carrying one access unit.
const AVC_NALU: u8 = 1;

impl FlvReader {
    /// Feeds freshly read bytes and returns everything that became whole.
    fn push(&mut self, data: &[u8]) -> Vec<Coded> {
        self.buffer.extend_from_slice(data);
        let mut out = Vec::new();
        let mut at = 0_usize;
        if !self.started {
            if self.buffer.len() < FLV_HEADER_LEN + 4 {
                return out;
            }
            if self.buffer.get(0..3) != Some(&b"FLV"[..]) {
                return out;
            }
            let offset = u32::from_be_bytes([
                self.buffer[5],
                self.buffer[6],
                self.buffer[7],
                self.buffer[8],
            ]);
            let start = usize::try_from(offset)
                .unwrap_or(FLV_HEADER_LEN)
                .max(FLV_HEADER_LEN)
                + 4;
            if self.buffer.len() < start {
                return out;
            }
            at = start;
            self.started = true;
        }
        while self.buffer.len() >= at + FLV_TAG_HEADER_LEN + 4 {
            let kind = self.buffer[at];
            let size = u24(&self.buffer[at + 1..at + 4]);
            let end = at + FLV_TAG_HEADER_LEN + size;
            if self.buffer.len() < end + 4 {
                break;
            }
            if kind == 9 {
                Self::video_tag(
                    &self.buffer[at + FLV_TAG_HEADER_LEN..end],
                    &mut self.nal_length_size,
                    &mut out,
                );
            }
            at = end + 4;
        }
        if at > 0 {
            self.buffer.drain(..at);
        }
        out
    }

    /// Turns one video tag into the configuration or a picture.
    fn video_tag(body: &[u8], length_size: &mut usize, out: &mut Vec<Coded>) {
        if body.len() < 5 {
            return;
        }
        let frame_type = body[0] >> 4;
        match body[1] {
            AVC_SEQUENCE_HEADER => {
                if let Some((csd, recorded)) = sequence_header(&body[5..]) {
                    *length_size = recorded;
                    out.push(Coded::Csd(csd));
                }
            }
            AVC_NALU => {
                let length_size = if *length_size == 0 { 4 } else { *length_size };
                let mut bytes = Vec::with_capacity(body.len());
                let mut at = 5_usize;
                while let Some((length, used)) = nal_length(&body[at..], length_size) {
                    at += used;
                    let Some(nal) = body.get(at..at.saturating_add(length)) else {
                        break;
                    };
                    bytes.extend_from_slice(&[0, 0, 0, 1]);
                    bytes.extend_from_slice(nal);
                    at += length;
                }
                if !bytes.is_empty() {
                    out.push(Coded::Picture(Nal {
                        bytes,
                        keyframe: frame_type == 1,
                    }));
                }
            }
            _ => {}
        }
    }
}

/// Reads a three-byte big-endian length, as FLV tag headers carry them.
fn u24(bytes: &[u8]) -> usize {
    let mut value = 0_usize;
    for byte in bytes.iter().take(3) {
        value = (value << 8) | usize::from(*byte);
    }
    value
}

/// Reads one AVCC NAL length prefix.
fn nal_length(bytes: &[u8], length_size: usize) -> Option<(usize, usize)> {
    if length_size == 0 || bytes.len() < length_size {
        return None;
    }
    let mut length = 0_usize;
    for byte in bytes.iter().take(length_size) {
        length = (length << 8) | usize::from(*byte);
    }
    if length == 0 {
        return None;
    }
    Some((length, length_size))
}

/// Extracts SPS/PPS from an `AVCDecoderConfigurationRecord` as Annex-B.
fn sequence_header(record: &[u8]) -> Option<(Vec<u8>, usize)> {
    if record.len() < 7 {
        return None;
    }
    let length_size = usize::from(record[4] & 0x03) + 1;
    let mut at = 5_usize;
    let mut csd = Vec::new();
    let sps_count = usize::from(*record.get(at)? & 0x1F);
    at += 1;
    for _ in 0..sps_count {
        let pair = record.get(at..at + 2)?;
        let length = usize::from(u16::from_be_bytes([pair[0], pair[1]]));
        at += 2;
        let nal = record.get(at..at + length)?;
        csd.extend_from_slice(&[0, 0, 0, 1]);
        csd.extend_from_slice(nal);
        at += length;
    }
    let pps_count = usize::from(*record.get(at)?);
    at += 1;
    for _ in 0..pps_count {
        let pair = record.get(at..at + 2)?;
        let length = usize::from(u16::from_be_bytes([pair[0], pair[1]]));
        at += 2;
        let nal = record.get(at..at + length)?;
        csd.extend_from_slice(&[0, 0, 0, 1]);
        csd.extend_from_slice(nal);
        at += length;
    }
    if csd.is_empty() {
        return None;
    }
    Some((csd, length_size))
}

/// The picture the phone is handed to decode.
///
/// A desktop-sized display is grabbed whole, but the phone only has its own
/// screen for it, so the picture it receives is normally the smaller one it
/// asked for. Encoding at the display's size would make the phone decode pixels
/// it then throws away, and a tall phone display can exceed what its decoder
/// accepts.
///
/// The shrink keeps the proportions of the picture that was grabbed instead of
/// pulling it into the phone's box on both axes at once. A display that has been
/// asked to change shape takes a moment to catch up, and a wide desktop squeezed
/// into a tall box for that moment arrives on the phone smeared sideways; fitted
/// the same on both axes it arrives whole, only smaller.
#[must_use]
pub(crate) fn coded_size(mode: Mode, width: u32, height: u32) -> Size {
    if width >= mode.width && height >= mode.height {
        return Size {
            width: mode.width,
            height: mode.height,
        };
    }
    // Whichever axis runs out of room first decides the scale; `widescreen`
    // below says the phone's width is the tighter of the two.
    let tight_by_width =
        u64::from(width) * u64::from(mode.height) <= u64::from(height) * u64::from(mode.width);
    let (width, height) = if tight_by_width {
        let long_edge = width * mode.height / mode.width.max(1);
        (width, long_edge)
    } else {
        let long_edge = height * mode.width / mode.height.max(1);
        (long_edge, height)
    };
    Size {
        width: even_at_least_two(width),
        height: even_at_least_two(height),
    }
}

/// H.264 in 4:2:0, which is what the phone decodes, wants both dimensions even,
/// and a picture a couple of pixels short of the box is still a picture.
const fn even_at_least_two(side: u32) -> u32 {
    let even = side - side % 2;
    if even == 0 {
        2
    } else {
        even
    }
}

/// Whether `coded` is the desktop's own size, so nothing has to be resized.
#[must_use]
const fn leaves_the_desktop_untouched(mode: Mode, coded: Size) -> bool {
    coded.width == mode.width && coded.height == mode.height
}

/// A running ffmpeg encoder.
#[derive(Debug)]
pub(crate) struct Encoder {
    child: Child,
    stdin: ChildStdin,
    nals: Receiver<Coded>,
    pending: std::collections::VecDeque<Nal>,
    csd: Vec<u8>,
    backend: Backend,
    width: u32,
    height: u32,
    frame_len: usize,
}

impl Encoder {
    /// Starts an encoder that grabs `mode` and delivers `coded` at `fps`.
    ///
    /// Hardware encoding is probed once; when VAAPI cannot encode, libx264 is
    /// used instead.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderError`] when ffmpeg is missing, the geometry is not
    /// encodable, or the chosen backend fails to produce a stream.
    pub(crate) fn spawn(mode: Mode, coded: Size, fps: u32) -> Result<Self, EncoderError> {
        let backend = preferred_backend();
        match Self::spawn_with_backend(mode, coded, fps, backend) {
            Ok(encoder) => Ok(encoder),
            Err(error) if backend == Backend::Vaapi => {
                log::info!("hardware encoding unavailable ({error}); using libx264");
                Self::spawn_with_backend(mode, coded, fps, Backend::Software)
            }
            Err(error) => Err(error),
        }
    }

    /// Starts an encoder on a specific backend.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderError`] under the same conditions as [`Self::spawn`].
    pub(crate) fn spawn_with_backend(
        mode: Mode,
        coded: Size,
        fps: u32,
        backend: Backend,
    ) -> Result<Self, EncoderError> {
        let frame_len = frame_len(mode).ok_or(EncoderError::Geometry {
            width: mode.width,
            height: mode.height,
        })?;
        let mut command = ffmpeg_command(mode, coded, fps, backend);
        let mut child = command.spawn().map_err(EncoderError::Spawn)?;

        let Some(stdin) = child.stdin.take() else {
            abort(&mut child);
            return Err(EncoderError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ffmpeg stdin was not piped",
            )));
        };
        let Some(stdout) = child.stdout.take() else {
            abort(&mut child);
            return Err(EncoderError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ffmpeg stdout was not piped",
            )));
        };

        let (sender, nals) = mpsc::channel();
        thread::spawn(move || read_flv(stdout, &sender));

        let mut encoder = Self {
            child,
            stdin,
            nals,
            pending: std::collections::VecDeque::new(),
            csd: Vec::new(),
            backend,
            width: mode.width,
            height: mode.height,
            frame_len,
        };
        if let Err(error) = encoder.handshake() {
            encoder.shutdown();
            return Err(error);
        }
        let shrunk = if leaves_the_desktop_untouched(mode, coded) {
            String::new()
        } else {
            format!(", shrunk to {}x{}", coded.width, coded.height)
        };
        log::info!(
            "{} encoder ready at {}x{}@{}{shrunk} ({} bytes of SPS/PPS)",
            backend,
            mode.width,
            mode.height,
            fps,
            encoder.csd.len()
        );
        Ok(encoder)
    }

    fn handshake(&mut self) -> Result<(), EncoderError> {
        // A picture only leaves the encoder once the ones behind it push it
        // out, and hardware pipelines keep several in flight, so frames are
        // fed one at a time until the configuration and an IDR come back.
        let black = self.black_frame();
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let mut saw_idr = false;
        while self.csd.is_empty() || !saw_idr {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(EncoderError::HandshakeTimeout {
                    backend: self.backend,
                });
            }
            self.stdin.write_all(&black).map_err(EncoderError::Feed)?;
            match self.nals.recv_timeout(remaining.min(HANDSHAKE_PEEK)) {
                Ok(Coded::Csd(csd)) => self.csd = csd,
                Ok(Coded::Picture(nal)) => {
                    saw_idr |= nal.is_keyframe();
                    self.pending.push_back(nal);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(EncoderError::Exited {
                        backend: self.backend,
                    });
                }
            }
        }
        Ok(())
    }

    /// The SPS/PPS pair needed to configure the decoder.
    #[must_use]
    pub(crate) fn csd(&self) -> &[u8] {
        &self.csd
    }

    /// A frame of black in NV12: limited-range black luma and neutral chroma.
    fn black_frame(&self) -> Vec<u8> {
        let luma = usize::try_from(self.width)
            .unwrap_or(0)
            .saturating_mul(usize::try_from(self.height).unwrap_or(0));
        let mut frame = vec![0_u8; self.frame_len];
        frame[..luma].fill(16);
        frame[luma..].fill(128);
        frame
    }

    /// The backend in use.
    #[must_use]
    pub(crate) const fn backend(&self) -> Backend {
        self.backend
    }

    /// The ffmpeg process itself.
    ///
    /// The encoder lives on its own thread, so a screen that is tearing down
    /// has no handle to it; a process id is enough to end it if the thread
    /// cannot be reached any other way.
    #[must_use]
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Feeds one raw frame matching the mode the encoder was created for.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderError::WrongFrameSize`] for a mismatched buffer and
    /// [`EncoderError::Feed`] when ffmpeg is gone or the pipe broke.
    pub(crate) fn feed(&mut self, pixels: &[u8]) -> Result<(), EncoderError> {
        if pixels.len() != self.frame_len {
            return Err(EncoderError::WrongFrameSize {
                width: self.width,
                height: self.height,
                expected: self.frame_len,
                provided: pixels.len(),
            });
        }
        self.stdin.write_all(pixels).map_err(EncoderError::Feed)
    }

    /// Waits up to `timeout` for the next encoded NAL.
    ///
    /// # Errors
    ///
    /// Returns [`EncoderError::Exited`] when ffmpeg stopped producing output.
    pub(crate) fn next_nal(&mut self, timeout: Duration) -> Result<Option<Nal>, EncoderError> {
        if let Some(nal) = self.pending.pop_front() {
            return Ok(Some(nal));
        }
        loop {
            match self.nals.recv_timeout(timeout) {
                Ok(Coded::Csd(csd)) => self.csd = csd,
                Ok(Coded::Picture(nal)) => return Ok(Some(nal)),
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(EncoderError::Exited {
                        backend: self.backend,
                    });
                }
            }
        }
    }

    /// Stops ffmpeg and waits for it to exit.
    pub(crate) fn shutdown(&mut self) {
        abort(&mut self.child);
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        abort(&mut self.child);
    }
}

fn abort(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_flv(mut stdout: impl Read, sender: &Sender<Coded>) {
    let mut reader = FlvReader::default();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                for coded in reader.push(&buffer[..count]) {
                    if sender.send(coded).is_err() {
                        return;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// One keyframe every second, in frame intervals. Recovery from a lost frame
/// means waiting for an IDR, so this is also the longest the picture can
/// freeze; the bitrate cap keeps the extra keyframes from costing too much.
const GOP_SECONDS: u32 = 1;

/// The bitrate media is encoded at, and the buffer it may fill, in bits.
///
/// Desktop content at the phone's size runs well under this; the ceiling is
/// aimed at keyframes and at keeping the encoder from chasing detail the
/// phone cannot show.
const MEDIA_BITRATE: &str = "24M";
const MEDIA_BUFSIZE: &str = "12M";

fn ffmpeg_command(mode: Mode, coded: Size, fps: u32, backend: Backend) -> Command {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-f",
        "rawvideo",
        // The host converts grabbed BGRA to NV12 before it reaches the pipe:
        // six bytes per two pixels instead of eight, which is the single
        // largest saving on a display that is mostly still. It also lets the
        // hardware encoder skip its own colour conversion.
        "-pix_fmt",
        "nv12",
        "-s",
        &format!("{}x{}", mode.width, mode.height),
        "-r",
        &fps.to_string(),
        "-i",
        "pipe:0",
    ]);
    match backend {
        Backend::Vaapi => {
            // The phone's own size is what it decodes, so the shrink happens on
            // the way to the encoder rather than on the phone's panel.
            //
            // The frame arrives as NV12 already, so `hwupload` only has to move
            // it onto the GPU. `scale_vaapi` still spells the format out: with
            // a bare `hwupload` the surface format follows the system memory
            // frame, and an identity-sized `scale_vaapi` passes it through rather
            // than converting.
            let filters = if leaves_the_desktop_untouched(mode, coded) {
                "hwupload,scale_vaapi=format=nv12".to_owned()
            } else {
                format!(
                    "hwupload,scale_vaapi=w={}:h={}:format=nv12",
                    coded.width, coded.height
                )
            };
            command.args([
                "-vaapi_device",
                VAAPI_DEVICE,
                "-vf",
                &filters,
                "-c:v",
                "h264_vaapi",
                // The fixed-function encoder ("low power") measured 24.6ms per
                // 2960x1344 frame against 28.0ms for the shader-based one, on
                // the same pipe and the same real desktop content — the whole
                // stream runs from a full feed, so that is the streaming rate:
                // about 40fps instead of 35.
                //
                // It only offers a constant quantiser on this driver (the
                // bitrate-capped modes were refused with "supported modes:
                // CQP"), so the cap that came with the shorter GOP had to go.
                // At qp 23 a moving desktop measured 9.6 Mbps, far below the
                // 296 Mbps the link carried, and a keyframe at the same quantiser
                // is not large enough to stall a write either.
                "-low_power",
                "1",
                "-rc_mode",
                "CQP",
                "-qp",
                "23",
                // Submitting frames asynchronously is what lets the
                // fixed-function encoder overlap its work. At depth one it
                // finished every frame before accepting the next and measured
                // 16.9ms per 2560x1162 frame; depth two measured 13.4ms (74fps)
                // on the same moving content, and deeper (3, 4, 6, 8) queued no
                // faster. Two is the smallest depth that reaches that rate, so
                // it adds the least latency: one frame.
                //
                // Measured again on 2026-11-08, because "less queue, less
                // latency" is wrong here: depth one serialised the pipe write
                // (9ms → 15ms per frame) and the stream settled with a backlog,
                // taking grab→NAL from 62ms to 143ms and the frame rate from
                // 41.4 to 39.9. Two stays.
                "-async_depth",
                "2",
            ]);
        }
        Backend::Software => {
            let filters = if leaves_the_desktop_untouched(mode, coded) {
                "format=yuv420p".to_owned()
            } else {
                format!("format=yuv420p,scale={}:{}", coded.width, coded.height)
            };
            command.args([
                "-vf",
                &filters,
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                // The keyframe interval lives in the shared `-g` below, so
                // this list must not carry a second, disagreeing one.
                "-x264-params",
                "scenecut=0",
                "-crf",
                "23",
                "-maxrate",
                MEDIA_BITRATE,
                "-bufsize",
                MEDIA_BUFSIZE,
            ]);
        }
    }
    command
        .args([
            // One IDR every GOP_SECONDS, and this is the only place that says
            // so. A dropped frame can only be recovered at an IDR, so the GOP
            // length is the worst case the picture can freeze for.
            "-g",
            &fps.saturating_mul(GOP_SECONDS).to_string(),
            "-bf",
            "0",
            // FLV only carries the pictures here: its tags are length-prefixed,
            // so a picture can leave the moment it is encoded instead of
            // waiting for the next one's start code. The phone still receives
            // Annex-B access units; the reader converts them back.
            "-f",
            "flv",
            "-flvflags",
            "no_duration_filesize",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

fn frame_len(mode: Mode) -> Option<usize> {
    let width = usize::try_from(mode.width).ok()?;
    let height = usize::try_from(mode.height).ok()?;
    // NV12 (4:2:0) is one luma sample per pixel plus one U/V pair per 2x2
    // block, and ffmpeg refuses a picture whose chroma cannot be halved.
    if width % 2 != 0 || height % 2 != 0 {
        return None;
    }
    width.checked_mul(height)?.checked_mul(3)?.checked_div(2)
}

fn preferred_backend() -> Backend {
    static BACKEND: std::sync::OnceLock<Backend> = std::sync::OnceLock::new();
    *BACKEND.get_or_init(probe_backend)
}

fn probe_backend() -> Backend {
    if !Path::new(VAAPI_DEVICE).exists() {
        return Backend::Software;
    }
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-vaapi_device",
            VAAPI_DEVICE,
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=320x180:d=0.1",
            "-frames:v",
            "1",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if status.is_ok_and(|status| status.success()) {
        Backend::Vaapi
    } else {
        Backend::Software
    }
}

/// Failure while encoding frames.
#[derive(Debug, Error)]
pub(crate) enum EncoderError {
    /// ffmpeg could not be started.
    #[error("starting ffmpeg failed: {0}")]
    Spawn(#[source] io::Error),
    /// ffmpeg stopped producing output.
    #[error("ffmpeg ({backend}) exited")]
    Exited {
        /// Backend that died.
        backend: Backend,
    },
    /// The startup self-test never saw a decodable stream.
    #[error("ffmpeg ({backend}) did not produce SPS/PPS and a keyframe in time")]
    HandshakeTimeout {
        /// Backend under test.
        backend: Backend,
    },
    /// Writing a frame to ffmpeg failed.
    #[error("sending a frame to ffmpeg failed: {0}")]
    Feed(#[source] io::Error),
    /// The caller passed a buffer of the wrong size.
    #[error("frame has {provided} bytes, expected {expected} for {width}x{height}")]
    WrongFrameSize {
        /// Encoder width.
        width: u32,
        /// Encoder height.
        height: u32,
        /// Required byte count.
        expected: usize,
        /// Actual byte count.
        provided: usize,
    },
    /// The mode cannot be encoded.
    #[error("display geometry {width}x{height} cannot be encoded")]
    Geometry {
        /// Rejected width.
        width: u32,
        /// Rejected height.
        height: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evdi::PixelFormat;

    fn flv_header() -> Vec<u8> {
        let mut stream = b"FLV\x01\x01\x00\x00\x00\x09".to_vec();
        stream.extend_from_slice(&0_u32.to_be_bytes());
        stream
    }

    fn flv_tag(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut stream = Vec::with_capacity(11 + body.len() + 4);
        stream.push(kind);
        stream.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        // Timestamp and stream id: seven bytes of the eleven-byte tag header.
        stream.extend_from_slice(&[0; 7]);
        stream.extend_from_slice(body);
        stream.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
        stream
    }

    fn video_tag(frame_header: u8, avc_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![frame_header, avc_type, 0, 0, 0];
        body.extend_from_slice(payload);
        flv_tag(9, &body)
    }

    fn avc_sequence_header(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut record = vec![1, 0x64, 0, 0x1f, 0xff, 0xe1];
        record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        record.extend_from_slice(sps);
        record.push(1);
        record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        record.extend_from_slice(pps);
        video_tag(0x17, 0, &record)
    }

    fn avcc_nalus(nalus: &[&[u8]]) -> Vec<u8> {
        let mut payload = Vec::new();
        for nal in nalus {
            payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            payload.extend_from_slice(nal);
        }
        payload
    }

    #[test]
    fn flv_reader_waits_for_a_whole_tag() {
        let mut stream = flv_header();
        stream.extend_from_slice(&avc_sequence_header(&[0x67, 0xAA], &[0x68, 0xBB]));
        stream.extend_from_slice(&video_tag(0x27, 1, &avcc_nalus(&[&[0x41, 0x01]])));

        let mut reader = FlvReader::default();
        let mut events = Vec::new();
        for byte in &stream[..stream.len() - 1] {
            events.extend(reader.push(std::slice::from_ref(byte)));
        }
        assert_eq!(
            events.len(),
            1,
            "the configuration is whole but the picture's tag is not"
        );
        events.extend(reader.push(&stream[stream.len() - 1..]));
        assert_eq!(events.len(), 2, "the configuration and the picture");
    }

    #[test]
    fn flv_reader_yields_the_configuration_and_whole_pictures() {
        let mut stream = flv_header();
        stream.extend_from_slice(&flv_tag(18, b"metadata"));
        stream.extend_from_slice(&avc_sequence_header(&[0x67, 0xAA], &[0x68, 0xBB]));
        stream.extend_from_slice(&video_tag(
            0x17,
            1,
            &avcc_nalus(&[&[0x06, 0x01], &[0x65, 0x80]]),
        ));
        stream.extend_from_slice(&video_tag(0x27, 1, &avcc_nalus(&[&[0x41, 0x80]])));

        let events = FlvReader::default().push(&stream);
        assert_eq!(events.len(), 3, "configuration and two pictures");
        let Coded::Csd(csd) = &events[0] else {
            panic!("expected the configuration, got {:?}", events[0]);
        };
        assert_eq!(csd, &[0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB]);
        let Coded::Picture(keyframe) = &events[1] else {
            panic!("expected a picture, got {:?}", events[1]);
        };
        assert!(keyframe.is_keyframe());
        assert_eq!(
            keyframe.bytes,
            vec![0, 0, 0, 1, 0x06, 0x01, 0, 0, 0, 1, 0x65, 0x80]
        );
        let Coded::Picture(inter) = &events[2] else {
            panic!("expected a picture, got {:?}", events[2]);
        };
        assert!(!inter.is_keyframe());
    }

    #[test]
    fn flv_reader_assembles_tags_split_across_reads() {
        let mut stream = flv_header();
        stream.extend_from_slice(&avc_sequence_header(&[0x67, 0xAA], &[0x68, 0xBB]));
        stream.extend_from_slice(&video_tag(0x17, 1, &avcc_nalus(&[&[0x65, 0x80]])));

        let mut reader = FlvReader::default();
        let mut events = Vec::new();
        for chunk in stream.chunks(7) {
            events.extend(reader.push(chunk));
        }
        assert_eq!(events.len(), 2);
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn software_encoder_emits_csd_and_keyframe() {
        if !ffmpeg_available() {
            log::warn!("ffmpeg not installed; skipping encoder smoke test");
            return;
        }
        let mode = Mode {
            width: 320,
            height: 180,
            refresh_hz: 30,
            format: PixelFormat::Bgra,
        };
        let mut encoder = Encoder::spawn_with_backend(
            mode,
            coded_size(mode, mode.width, mode.height),
            30,
            Backend::Software,
        )
        .expect("encoder starts");
        assert!(encoder.csd().len() > 8, "SPS/PPS must be captured");
        let mut saw_keyframe = false;
        for _ in 0..16 {
            let nal = encoder
                .next_nal(Duration::from_secs(2))
                .expect("stream alive")
                .expect("self-test frame is queued");
            saw_keyframe |= nal.is_keyframe();
            if saw_keyframe {
                break;
            }
        }
        assert!(saw_keyframe, "self-test frame must carry an IDR slice");

        let frame = vec![0x40_u8; 320 * 180 * 3 / 2];
        encoder.feed(&frame).expect("frame is accepted");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_slice = false;
        while Instant::now() < deadline && !saw_slice {
            if let Some(nal) = encoder
                .next_nal(Duration::from_millis(200))
                .expect("stream alive")
            {
                saw_slice |= nal.bytes.windows(4).any(|window| {
                    window[0] == 0
                        && window[1] == 0
                        && window[2] == 1
                        && matches!(window[3] & 0x1f, 1 | 5)
                });
            }
        }
        assert!(saw_slice, "encoder must produce a coded slice");
    }

    #[test]
    fn hardware_encoder_emits_csd_and_keyframe() {
        if !ffmpeg_available() || !Path::new(VAAPI_DEVICE).exists() {
            return;
        }
        let mode = Mode {
            width: 2460,
            height: 1080,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let encoder = Encoder::spawn_with_backend(
            mode,
            coded_size(mode, mode.width, mode.height),
            60,
            Backend::Vaapi,
        )
        .expect("vaapi encoder starts");
        assert!(encoder.csd().len() > 8, "SPS/PPS must be captured");
    }

    #[test]
    fn encoder_rejects_mismatched_frame_size() {
        if !ffmpeg_available() {
            return;
        }
        let mode = Mode {
            width: 320,
            height: 180,
            refresh_hz: 30,
            format: PixelFormat::Bgra,
        };
        let mut encoder = Encoder::spawn_with_backend(
            mode,
            coded_size(mode, mode.width, mode.height),
            30,
            Backend::Software,
        )
        .expect("encoder starts");
        assert!(matches!(
            encoder.feed(&[0_u8; 16]),
            Err(EncoderError::WrongFrameSize { .. })
        ));
    }

    #[test]
    fn a_phone_larger_than_the_display_is_clamped_to_the_display() {
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        assert_eq!(
            coded_size(mode, 1920, 1080),
            Size {
                width: 1280,
                height: 720
            }
        );
        assert!(leaves_the_desktop_untouched(
            mode,
            coded_size(mode, 1280, 720)
        ));
        assert!(!leaves_the_desktop_untouched(
            mode,
            coded_size(mode, 1080, 2460)
        ));
    }

    #[test]
    fn a_shrunk_picture_is_scaled_before_the_encoder() {
        let mode = Mode {
            width: 1344,
            height: 3064,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let coded = coded_size(mode, 1080, 2460);
        assert_eq!(
            vf_filter(&ffmpeg_command(mode, coded, 60, Backend::Software)),
            "format=yuv420p,scale=1078:2460"
        );
        assert_eq!(
            vf_filter(&ffmpeg_command(mode, coded, 60, Backend::Vaapi)),
            "hwupload,scale_vaapi=w=1078:h=2460:format=nv12"
        );
    }

    #[test]
    fn a_picture_from_the_wrong_shape_is_fitted_not_squashed() {
        // The phone has turned and the display has not caught up, so the desktop
        // being grabbed is wide and the panel it is going to is tall. Pulling
        // both axes to the panel's box would hand the phone the desktop on one
        // axis and stretched on the other; fitting leaves it whole and small.
        let mode = Mode {
            width: 3064,
            height: 1344,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let coded = coded_size(mode, 1080, 2460);
        assert_eq!(
            coded,
            Size {
                width: 1080,
                height: 472
            }
        );
        let grabbed = f64::from(mode.width) / f64::from(mode.height);
        let handed = f64::from(coded.width) / f64::from(coded.height);
        assert!(
            (grabbed - handed).abs() < 0.01,
            "the picture keeps the shape it was grabbed in: {grabbed} against {handed}",
        );
    }

    #[test]
    fn a_picture_fitted_to_the_panel_stays_even() {
        let mode = Mode {
            width: 1343,
            height: 3063,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let coded = coded_size(mode, 1080, 2460);
        assert_eq!(coded.width % 2, 0);
        assert_eq!(coded.height % 2, 0);
        assert!(coded.width <= 1080 && coded.height <= 2460);
    }

    #[test]
    fn a_full_size_picture_is_not_scaled() {
        let mode = Mode {
            width: 1080,
            height: 2460,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let coded = coded_size(mode, mode.width, mode.height);
        assert_eq!(
            vf_filter(&ffmpeg_command(mode, coded, 60, Backend::Software)),
            "format=yuv420p"
        );
        assert_eq!(
            vf_filter(&ffmpeg_command(mode, coded, 60, Backend::Vaapi)),
            "hwupload,scale_vaapi=format=nv12"
        );
    }

    /// The value of the `-vf` argument ffmpeg was handed.
    fn vf_filter(command: &Command) -> String {
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let position = args
            .iter()
            .position(|arg| arg == "-vf")
            .expect("-vf is set");
        args[position + 1].clone()
    }
}
