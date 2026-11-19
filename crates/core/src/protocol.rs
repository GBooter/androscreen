//! Length-prefixed frame protocol shared by the host daemon and the phone app.
//!
//! Every frame starts with a twelve-byte header:
//!
//! ```text
//! 0..4   magic "ASCR"
//! 4      protocol version
//! 5      frame kind
//! 6      flags
//! 7      reserved (zero)
//! 8..12  payload length, little endian
//! ```
//!
//! Control-plane messages are JSON so they can evolve without a wire change;
//! video, audio, and touch payloads stay binary for throughput and latency.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Magic bytes at the start of every frame.
pub const MAGIC: [u8; 4] = *b"ASCR";

/// Version of the frame layout and control vocabulary.
pub const PROTOCOL_VERSION: u8 = 1;

/// Size of the fixed frame header.
pub const HEADER_LEN: usize = 12;

/// Largest payload accepted from the peer.
pub const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// Video flag: the payload contains an H.264 keyframe (IDR).
pub const FLAG_KEYFRAME: u8 = 0x01;

/// AOA accessory strings; the Android manifest must declare the same values.
pub const AOA_MANUFACTURER: &str = "AndroScreen";
/// AOA accessory model string.
pub const AOA_MODEL: &str = "VirtualDisplay";
/// AOA accessory version string.
pub const AOA_VERSION: &str = "1";
/// AOA accessory description.
pub const AOA_DESCRIPTION: &str = "AndroScreen USB display";
/// AOA accessory serial number.
pub const AOA_SERIAL: &str = "AS-0001";
/// AOA accessory URI; intentionally empty, the app opens from the USB intent.
pub const AOA_URI: &str = "";

/// Frame kind, occupying the sixth header byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Kind {
    /// JSON-encoded [`Control`] message.
    Control = 1,
    /// H.264 Annex-B chunk.
    Video = 2,
    /// Raw PCM audio chunk.
    Audio = 3,
    /// Binary touch report.
    Touch = 4,
    /// Liveness request.
    Ping = 5,
    /// Liveness reply.
    Pong = 6,
    /// Pointer position or shape, drawn locally by the phone.
    Cursor = 7,
}

impl Kind {
    /// Decodes a header byte, returning `None` for unknown kinds.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Control),
            2 => Some(Self::Video),
            3 => Some(Self::Audio),
            4 => Some(Self::Touch),
            5 => Some(Self::Ping),
            6 => Some(Self::Pong),
            7 => Some(Self::Cursor),
            _ => None,
        }
    }

    /// Returns the wire value of this kind.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Cursor payload tag: the pointer moved.
pub const CURSOR_MOVE: u8 = 0;
/// Cursor payload tag: the pointer shape changed.
pub const CURSOR_SHAPE: u8 = 1;

/// Bytes in a cursor move payload, tag included.
pub const CURSOR_MOVE_LEN: usize = 6;
/// Bytes in a cursor shape payload before its pixels.
pub const CURSOR_SHAPE_HEADER_LEN: usize = 14;

/// One decoded cursor update.
///
/// Positions are normalized to `0..=65535` of the virtual display, so the
/// phone can place the pointer without knowing the desktop's resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorUpdate<'a> {
    /// The pointer moved, or was shown/hidden at its last position.
    Move {
        /// Whether the pointer should be drawn.
        visible: bool,
        /// Horizontal position across the virtual display.
        x: u16,
        /// Vertical position across the virtual display.
        y: u16,
    },
    /// The pointer bitmap changed.
    Shape {
        /// Whether the pointer should be drawn.
        visible: bool,
        /// Bitmap width in pixels.
        width: u16,
        /// Bitmap height in pixels.
        height: u16,
        /// Hotspot column measured from the bitmap's left edge.
        hot_x: i16,
        /// Hotspot row measured from the bitmap's top edge.
        hot_y: i16,
        /// Width of the virtual display the bitmap is scaled against.
        screen_width: u16,
        /// Height of the virtual display the bitmap is scaled against.
        screen_height: u16,
        /// Straight RGBA pixels, row-major and tightly packed.
        pixels: &'a [u8],
    },
}

/// What the host answering a ping knows about the phone that asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Session {
    /// A greeting arrived, so the link has a session behind it.
    Known,
    /// No greeting has arrived on this host yet.
    ///
    /// Restarting the daemon with the cable still in leaves both halves of the
    /// link up and answering pings, so without this the phone would keep
    /// waiting for a command that no longer has anyone to send it.
    AwaitingHello,
}

impl Session {
    /// The payload byte of a pong carrying this state.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Known => 0,
            Self::AwaitingHello => 1,
        }
    }
}

/// One wire frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    kind: Kind,
    flags: u8,
    payload: Vec<u8>,
}

impl Frame {
    /// Builds a frame without flags.
    #[must_use]
    pub fn new(kind: Kind, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            flags: 0,
            payload: payload.into(),
        }
    }

    /// Builds a frame with explicit flags.
    #[must_use]
    pub fn with_flags(kind: Kind, flags: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            kind,
            flags,
            payload: payload.into(),
        }
    }

    /// Builds the reply to a ping, carrying what the host thinks of this phone.
    #[must_use]
    pub fn pong(session: Session) -> Self {
        Self::new(Kind::Pong, vec![session.as_byte()])
    }

    /// Builds a pointer position update in normalized display coordinates.
    #[must_use]
    pub fn cursor_move(x: u16, y: u16) -> Self {
        let mut payload = Vec::with_capacity(CURSOR_MOVE_LEN);
        payload.push(CURSOR_MOVE);
        payload.push(1);
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        Self::new(Kind::Cursor, payload)
    }

    /// Builds a request to stop drawing the pointer.
    #[must_use]
    pub fn cursor_hide() -> Self {
        Self::new(Kind::Cursor, vec![CURSOR_MOVE, 0, 0, 0, 0, 0])
    }

    /// Builds a pointer bitmap with its hotspot and display scale.
    ///
    /// `pixels` is straight RGBA, row-major and tightly packed.
    #[must_use]
    pub fn cursor_shape(
        width: u16,
        height: u16,
        hot_x: i16,
        hot_y: i16,
        screen_width: u16,
        screen_height: u16,
        pixels: impl Into<Vec<u8>>,
    ) -> Self {
        let pixels = pixels.into();
        let mut payload = Vec::with_capacity(CURSOR_SHAPE_HEADER_LEN + pixels.len());
        payload.push(CURSOR_SHAPE);
        payload.push(1);
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.extend_from_slice(&hot_x.to_le_bytes());
        payload.extend_from_slice(&hot_y.to_le_bytes());
        payload.extend_from_slice(&screen_width.to_le_bytes());
        payload.extend_from_slice(&screen_height.to_le_bytes());
        payload.extend_from_slice(&pixels);
        Self::new(Kind::Cursor, payload)
    }

    /// Decodes this frame as a cursor update, when it is one.
    #[must_use]
    pub fn as_cursor(&self) -> Option<CursorUpdate<'_>> {
        if self.kind != Kind::Cursor {
            return None;
        }
        let payload = &self.payload;
        let tag = *payload.first()?;
        let visible = payload.get(1)? & 1 != 0;
        match tag {
            CURSOR_MOVE => {
                if payload.len() < CURSOR_MOVE_LEN {
                    return None;
                }
                Some(CursorUpdate::Move {
                    visible,
                    x: u16::from_le_bytes([payload[2], payload[3]]),
                    y: u16::from_le_bytes([payload[4], payload[5]]),
                })
            }
            CURSOR_SHAPE => {
                if payload.len() < CURSOR_SHAPE_HEADER_LEN {
                    return None;
                }
                let width = u16::from_le_bytes([payload[2], payload[3]]);
                let height = u16::from_le_bytes([payload[4], payload[5]]);
                let pixels = &payload[CURSOR_SHAPE_HEADER_LEN..];
                let expected = usize::from(width)
                    .saturating_mul(usize::from(height))
                    .saturating_mul(4);
                if pixels.len() < expected {
                    return None;
                }
                Some(CursorUpdate::Shape {
                    visible,
                    width,
                    height,
                    hot_x: i16::from_le_bytes([payload[6], payload[7]]),
                    hot_y: i16::from_le_bytes([payload[8], payload[9]]),
                    screen_width: u16::from_le_bytes([payload[10], payload[11]]),
                    screen_height: u16::from_le_bytes([payload[12], payload[13]]),
                    pixels: &pixels[..expected],
                })
            }
            _ => None,
        }
    }

    /// Whether a pong says the host has no session with this phone.
    ///
    /// A payload from an older host is empty, which means `Session::Known`.
    #[must_use]
    pub fn session(&self) -> Session {
        if self.kind != Kind::Pong {
            return Session::Known;
        }
        match self.payload.first().copied() {
            Some(1) => Session::AwaitingHello,
            _ => Session::Known,
        }
    }

    /// Encodes a JSON control message into a [`Kind::Control`] frame.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Json`] when the message cannot be serialized.
    pub fn control(control: &Control) -> Result<Self, ControlError> {
        Ok(Self::new(Kind::Control, control.encode()?))
    }

    /// Decodes this frame as a JSON control message.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::WrongKind`] for non-control frames and
    /// [`ControlError::Json`] for malformed JSON.
    pub fn as_control(&self) -> Result<Control, ControlError> {
        if self.kind != Kind::Control {
            return Err(ControlError::WrongKind {
                expected: Kind::Control,
                found: self.kind,
            });
        }
        Control::decode(&self.payload)
    }

    /// The frame kind.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// The frame flags.
    #[must_use]
    pub const fn flags(&self) -> u8 {
        self.flags
    }

    /// The payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the frame and returns the payload.
    #[must_use]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// Whether [`FLAG_KEYFRAME`] is set.
    #[must_use]
    pub const fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }
}

/// One parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Kind of the frame that follows.
    pub kind: Kind,
    /// Frame flags.
    pub flags: u8,
    /// Payload length in bytes.
    pub payload_len: usize,
}

/// Validates and decodes a fixed frame header.
///
/// Transports that buffer bytes themselves (USB) use this to find frame
/// boundaries without going through [`read_frame`].
///
/// # Errors
///
/// Returns [`ProtocolError`] for a bad magic, an unsupported version, an
/// unknown kind, or a payload length above [`MAX_PAYLOAD`].
pub fn parse_header(header: &[u8; HEADER_LEN]) -> Result<FrameHeader, ProtocolError> {
    if header[0..4] != MAGIC {
        return Err(ProtocolError::BadMagic {
            found: [header[0], header[1], header[2], header[3]],
        });
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion { found: header[4] });
    }
    let kind = Kind::from_byte(header[5]).ok_or(ProtocolError::UnknownKind(header[5]))?;
    let payload_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    if payload_len > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge {
            length: payload_len,
        });
    }
    Ok(FrameHeader {
        kind,
        flags: header[6],
        payload_len,
    })
}

/// Writes one frame to `writer`.
///
/// # Errors
///
/// Returns [`ProtocolError::PayloadTooLarge`] when the payload exceeds
/// [`MAX_PAYLOAD`], and [`ProtocolError::Io`] on write failure.
pub fn write_frame(writer: &mut (impl Write + ?Sized), frame: &Frame) -> Result<(), ProtocolError> {
    let length =
        u32::try_from(frame.payload.len()).map_err(|_| ProtocolError::PayloadTooLarge {
            length: frame.payload.len(),
        })?;
    if frame.payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge {
            length: frame.payload.len(),
        });
    }

    let mut header = [0_u8; HEADER_LEN];
    header[0..4].copy_from_slice(&MAGIC);
    header[4] = PROTOCOL_VERSION;
    header[5] = frame.kind.as_byte();
    header[6] = frame.flags;
    header[8..12].copy_from_slice(&length.to_le_bytes());

    writer.write_all(&header)?;
    writer.write_all(&frame.payload)?;
    Ok(())
}

/// Reads one frame from `reader`.
///
/// A clean end of stream before the header yields
/// [`ProtocolError::Disconnected`], which callers treat as an unplug rather
/// than an error.
///
/// # Errors
///
/// Returns [`ProtocolError`] for a truncated header or payload, bad magic, an
/// unsupported version, an unknown kind, or an oversized payload.
pub fn read_frame(reader: &mut (impl Read + ?Sized)) -> Result<Frame, ProtocolError> {
    let mut header = [0_u8; HEADER_LEN];
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Err(ProtocolError::Disconnected),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
    reader.read_exact(&mut header[1..])?;
    let header = parse_header(&header)?;

    let mut payload = vec![0_u8; header.payload_len];
    reader.read_exact(&mut payload)?;
    Ok(Frame {
        kind: header.kind,
        flags: header.flags,
        payload,
    })
}

/// A control-plane message.
///
/// Unknown JSON fields are ignored so a newer peer can add data without
/// breaking an older one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Control {
    /// Phone greeting; must be the first message after the USB link opens.
    Hello {
        /// Wire protocol version implemented by the phone.
        protocol: u8,
        /// Human-readable app version.
        app_version: String,
        /// Features the phone supports.
        capabilities: Vec<Capability>,
    },
    /// Host reply to [`Control::Hello`].
    Welcome {
        /// Wire protocol version implemented by the host.
        protocol: u8,
        /// Human-readable daemon version.
        host_version: String,
        /// Whether the host can forward desktop audio.
        audio: bool,
        /// Largest virtual display the host accepts.
        max_screen: Size,
    },
    /// Phone asks the host to create a virtual display and start streaming.
    StartScreen {
        /// Requested display geometry.
        request: ScreenRequest,
    },
    /// Phone asks the host to tear the virtual display down.
    StopScreen,
    /// Phone hands the USB port back.
    ///
    /// Accessory mode and file transfer cannot share a port: while the link is
    /// open the phone's storage is not offered to the desktop at all. A phone
    /// that would rather be a memory stick says so here, and the host lets the
    /// port go instead of holding it for a screen nobody asked for.
    Release {
        /// Human-readable reason, for the host log.
        reason: String,
    },
    /// The virtual display exists and encoding has started.
    ScreenStarted {
        /// Width of the virtual display.
        width: u32,
        /// Height of the virtual display.
        height: u32,
        /// Encoder description, for diagnostics.
        encoder: String,
    },
    /// The virtual display was removed or streaming failed.
    ScreenStopped {
        /// Empty when stopped on request, otherwise the failure reason.
        reason: String,
    },
    /// Describes the H.264 stream that follows.
    VideoConfig {
        /// Codec of [`Kind::Video`] payloads.
        codec: VideoCodec,
        /// Encoded width.
        width: u32,
        /// Encoded height.
        height: u32,
        /// Encoded frame rate.
        fps: u32,
        /// Annex-B SPS and PPS the decoder needs before the first frame.
        csd: Vec<u8>,
    },
    /// Describes the PCM stream that follows.
    AudioConfig {
        /// Samples per second.
        sample_rate: u32,
        /// Interleaved channel count.
        channels: u8,
        /// Sample format.
        format: AudioFormat,
    },
    /// One-per-second host statistics sample.
    Stats {
        /// The sample.
        stats: crate::stats::SystemStats,
    },
    /// Non-fatal problem the phone should show to the user.
    Error {
        /// Stable machine-readable code.
        code: String,
        /// Human-readable detail.
        message: String,
    },
}

impl Control {
    /// Serializes this message to JSON bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Json`] if serialization fails.
    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// Deserializes a JSON control message.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Json`] for malformed or unknown messages.
    pub fn decode(payload: &[u8]) -> Result<Self, ControlError> {
        Ok(serde_json::from_slice(payload)?)
    }
}

/// A display geometry, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A phone request for a virtual display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenRequest {
    /// Width in pixels; must be even.
    pub width: u32,
    /// Height in pixels; must be even.
    pub height: u32,
    /// Requested refresh rate.
    pub refresh_hz: u32,
    /// Whether desktop audio should be forwarded.
    pub audio: bool,
    /// Whether the display must be the size asked for and nothing larger.
    ///
    /// A desktop enlarges a narrow display so that its own scale keeps its
    /// meaning. A phone whose decoder cannot take that enlargement (or whose
    /// link cannot carry it) sets this and gets the size it asked for.
    /// Hosts from before this field existed ignore it and enlarge anyway.
    #[serde(default)]
    pub exact: bool,
}

impl ScreenRequest {
    /// Smallest accepted width or height.
    pub const MIN_DIMENSION: u32 = 320;
    /// Largest accepted width or height.
    pub const MAX_DIMENSION: u32 = 3840;
    /// Smallest accepted refresh rate.
    pub const MIN_REFRESH_HZ: u32 = 30;
    /// Largest accepted refresh rate.
    pub const MAX_REFRESH_HZ: u32 = 120;

    /// Validates the request against the limits the host and H.264 enforce.
    ///
    /// # Errors
    ///
    /// Returns a [`RequestError`] describing the first violated limit.
    pub fn validate(&self) -> Result<(), RequestError> {
        if !(Self::MIN_DIMENSION..=Self::MAX_DIMENSION).contains(&self.width) {
            return Err(RequestError::Dimension {
                width: self.width,
                height: self.height,
            });
        }
        if !(Self::MIN_DIMENSION..=Self::MAX_DIMENSION).contains(&self.height) {
            return Err(RequestError::Dimension {
                width: self.width,
                height: self.height,
            });
        }
        if self.width % 2 != 0 || self.height % 2 != 0 {
            return Err(RequestError::OddDimension {
                width: self.width,
                height: self.height,
            });
        }
        if !(Self::MIN_REFRESH_HZ..=Self::MAX_REFRESH_HZ).contains(&self.refresh_hz) {
            return Err(RequestError::Refresh {
                refresh_hz: self.refresh_hz,
            });
        }
        Ok(())
    }

    /// The geometry as a [`Size`].
    #[must_use]
    pub const fn size(&self) -> Size {
        Size {
            width: self.width,
            height: self.height,
        }
    }
}

/// A capability advertised by the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Can display a video stream.
    Screen,
    /// Can send multi-touch input.
    Touch,
    /// Can play forwarded audio.
    Audio,
}

/// Video codecs the protocol can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum VideoCodec {
    /// H.264 Annex-B.
    H264,
}

/// PCM sample formats the protocol can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioFormat {
    /// Signed 16-bit little-endian.
    S16Le,
}

/// A screen request that violates host limits.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RequestError {
    /// Width or height outside the accepted range.
    #[error("display {width}x{height} outside accepted range")]
    Dimension {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Width or height is not even; H.264 4:2:0 requires even dimensions.
    #[error("display {width}x{height} has an odd dimension")]
    OddDimension {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Refresh rate outside the accepted range.
    #[error("refresh rate {refresh_hz} Hz outside accepted range")]
    Refresh {
        /// Requested refresh rate.
        refresh_hz: u32,
    },
}

/// Failure while reading or writing a frame.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The peer closed the link between frames.
    #[error("connection closed")]
    Disconnected,
    /// Underlying stream error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// The header did not start with [`MAGIC`].
    #[error("bad frame magic {found:02x?}")]
    BadMagic {
        /// The bytes that were read.
        found: [u8; 4],
    },
    /// The peer speaks another protocol version.
    #[error("unsupported protocol version {found}")]
    UnsupportedVersion {
        /// Version found in the header.
        found: u8,
    },
    /// The frame kind is not known to this build.
    #[error("unknown frame kind {0}")]
    UnknownKind(u8),
    /// The payload length exceeds [`MAX_PAYLOAD`].
    #[error("payload too large: {length} bytes")]
    PayloadTooLarge {
        /// Declared length.
        length: usize,
    },
}

/// Failure while encoding or decoding a control message.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ControlError {
    /// JSON serialization failed.
    #[error("control json: {0}")]
    Json(#[from] serde_json::Error),
    /// A control message was expected but another frame kind arrived.
    #[error("expected {expected:?} frame, found {found:?}")]
    WrongKind {
        /// The required kind.
        expected: Kind,
        /// The observed kind.
        found: Kind,
    },
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn sample_control() -> Control {
        Control::Hello {
            protocol: PROTOCOL_VERSION,
            app_version: "0.1.0".to_owned(),
            capabilities: vec![Capability::Screen, Capability::Touch],
        }
    }

    #[test]
    fn frame_round_trip() {
        let frame = Frame::with_flags(Kind::Video, FLAG_KEYFRAME, vec![0, 1, 2, 3]);
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).expect("writes");
        let decoded = read_frame(&mut Cursor::new(bytes)).expect("reads");
        assert_eq!(decoded, frame);
        assert!(decoded.is_keyframe());
    }

    #[test]
    fn empty_payload_round_trip() {
        let frame = Frame::new(Kind::Ping, Vec::new());
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).expect("writes");
        assert_eq!(read_frame(&mut Cursor::new(bytes)).expect("reads"), frame);
    }

    #[test]
    fn cursor_move_round_trip() {
        let frame = Frame::cursor_move(1234, 56_789);
        assert_eq!(
            frame.as_cursor(),
            Some(CursorUpdate::Move {
                visible: true,
                x: 1234,
                y: 56_789
            })
        );
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &frame).expect("writes");
        let decoded = read_frame(&mut Cursor::new(bytes)).expect("reads");
        assert_eq!(decoded.kind(), Kind::Cursor);
        assert_eq!(decoded.as_cursor(), frame.as_cursor());
    }

    #[test]
    fn cursor_shape_round_trip() {
        let pixels = vec![1_u8, 2, 3, 4, 5, 6, 7, 8];
        let frame = Frame::cursor_shape(2, 1, -3, 4, 3200, 2000, pixels.clone());
        match frame.as_cursor() {
            Some(CursorUpdate::Shape {
                visible,
                width,
                height,
                hot_x,
                hot_y,
                screen_width,
                screen_height,
                pixels: decoded,
            }) => {
                assert!(visible);
                assert_eq!((width, height), (2, 1));
                assert_eq!((hot_x, hot_y), (-3, 4));
                assert_eq!((screen_width, screen_height), (3200, 2000));
                assert_eq!(decoded, pixels.as_slice());
            }
            other => panic!("expected a shape, got {other:?}"),
        }
    }

    #[test]
    fn cursor_shape_without_its_pixels_is_rejected() {
        let mut frame = Frame::cursor_shape(2, 1, 0, 0, 3200, 2000, vec![0_u8; 8]);
        let payload = frame.payload.split_off(CURSOR_SHAPE_HEADER_LEN + 4);
        assert_eq!(payload.len(), 4);
        assert!(frame.as_cursor().is_none());
    }

    #[test]
    fn cursor_frame_of_another_kind_decodes_to_nothing() {
        assert!(Frame::cursor_move(0, 0).as_cursor().is_some());
        assert!(Frame::cursor_hide().as_cursor().is_some());
        assert!(Frame::new(Kind::Ping, Vec::new()).as_cursor().is_none());
    }

    #[test]
    fn clean_eof_is_disconnected() {
        assert!(matches!(
            read_frame(&mut Cursor::new(Vec::new())),
            Err(ProtocolError::Disconnected)
        ));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::new(Kind::Ping, Vec::new())).expect("writes");
        bytes[0] = b'X';
        assert!(matches!(
            read_frame(&mut Cursor::new(bytes)),
            Err(ProtocolError::BadMagic { .. })
        ));
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &Frame::new(Kind::Ping, Vec::new())).expect("writes");
        bytes[5] = 0x7f;
        assert!(matches!(
            read_frame(&mut Cursor::new(bytes)),
            Err(ProtocolError::UnknownKind(0x7f))
        ));
    }

    #[test]
    fn rejects_oversized_payload_before_allocating() {
        let mut bytes = vec![0_u8; HEADER_LEN];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4] = PROTOCOL_VERSION;
        bytes[5] = Kind::Ping.as_byte();
        let huge = u32::try_from(MAX_PAYLOAD + 1).expect("fits u32");
        bytes[8..12].copy_from_slice(&huge.to_le_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(bytes)),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn control_round_trip() {
        let control = sample_control();
        let frame = Frame::control(&control).expect("encodes");
        assert_eq!(frame.kind(), Kind::Control);
        assert_eq!(frame.as_control().expect("decodes"), control);
    }

    #[test]
    fn all_control_variants_round_trip() {
        let variants = [
            Control::Welcome {
                protocol: PROTOCOL_VERSION,
                host_version: "0.1.0".to_owned(),
                audio: true,
                max_screen: Size {
                    width: 1920,
                    height: 1080,
                },
            },
            Control::StartScreen {
                request: ScreenRequest {
                    width: 2400,
                    height: 1080,
                    refresh_hz: 60,
                    audio: false,
                    exact: false,
                },
            },
            Control::StopScreen,
            Control::Release {
                reason: "the user wants the file manager".to_owned(),
            },
            Control::ScreenStarted {
                width: 1920,
                height: 1080,
                encoder: "h264_vaapi".to_owned(),
            },
            Control::ScreenStopped {
                reason: String::new(),
            },
            Control::VideoConfig {
                codec: VideoCodec::H264,
                width: 1920,
                height: 1080,
                fps: 60,
                csd: vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68],
            },
            Control::AudioConfig {
                sample_rate: 48_000,
                channels: 2,
                format: AudioFormat::S16Le,
            },
            Control::Error {
                code: "screen_busy".to_owned(),
                message: "another phone is streaming".to_owned(),
            },
        ];
        for control in variants {
            let frame = Frame::control(&control).expect("encodes");
            assert_eq!(frame.as_control().expect("decodes"), control);
        }
    }

    #[test]
    fn control_rejects_wrong_kind() {
        assert!(matches!(
            Frame::new(Kind::Ping, Vec::new()).as_control(),
            Err(ControlError::WrongKind { .. })
        ));
    }

    #[test]
    fn control_ignores_unknown_fields() {
        let json = br#"{"type":"stop_screen","future_field":42}"#;
        assert_eq!(Control::decode(json).expect("decodes"), Control::StopScreen);
    }

    #[test]
    fn screen_request_validation() {
        let valid = ScreenRequest {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            audio: true,
            exact: false,
        };
        assert_eq!(valid.validate(), Ok(()));
        assert!(matches!(
            ScreenRequest { width: 0, ..valid }.validate(),
            Err(RequestError::Dimension { .. })
        ));
        assert!(matches!(
            ScreenRequest {
                width: 1921,
                ..valid
            }
            .validate(),
            Err(RequestError::OddDimension { .. })
        ));
        assert!(matches!(
            ScreenRequest {
                refresh_hz: 240,
                ..valid
            }
            .validate(),
            Err(RequestError::Refresh { .. })
        ));
    }
}
