//! Minimal evdi client built directly on libc ioctls.
//!
//! evdi exposes one virtual DRM card per display. The host connects a card
//! with an EDID describing the phone panel, then pulls frames with
//! `EVDI_GRABPIX` in dirty-rectangle mode. The caller owns a persistent
//! [`FrameBuffer`] because the kernel only overwrites the rectangles that
//! changed since the previous grab.
//!
//! Cursor events are enabled, so the compositor's pointer reaches the host as
//! a bitmap and a position instead of being blended into the grabbed pixels.
//! The phone draws it locally, which keeps the pointer at its own refresh rate
//! instead of the video stream's.

use std::ffi::c_ulong;
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use thiserror::Error;

/// Highest `/dev/dri/cardN` index probed during discovery.
const MAX_CARD_INDEX: i32 = 63;

/// Minimum evdi module version whose ioctl layout this module understands.
const COMPAT_MAJOR: i32 = 1;
const COMPAT_MINOR: i32 = 9;

/// evdi caps the dirty list at sixteen entries and collapses beyond that.
pub(crate) const MAX_DIRTY_RECTS: usize = 16;

const DRM_IOCTL_BASE: u32 = 0x64; // 'd'
const DRM_COMMAND_BASE: u32 = 0x40;
const DRM_IOCTL_VERSION_NR: u32 = 0x00;
/// `DRM_IOCTL_MODE_MAP_DUMB`, which asks the driver for an mmap offset.
const DRM_IOCTL_MODE_MAP_DUMB_NR: u32 = 0xB3;
/// `DRM_EVDI_ENABLE_CURSOR_EVENTS`, the fifth driver-specific command.
const DRM_EVDI_ENABLE_CURSOR_EVENTS_NR: u32 = DRM_COMMAND_BASE + 4;

const GRABPIX_MODE_DIRTY: i32 = 1;

const EVENT_UPDATE_READY: u32 = 0x8000_0000;
const EVENT_DPMS: u32 = 0x8000_0001;
const EVENT_MODE_CHANGED: u32 = 0x8000_0002;
const EVENT_CRTC_STATE: u32 = 0x8000_0003;
const EVENT_CURSOR_SET: u32 = 0x8000_0004;
const EVENT_CURSOR_MOVE: u32 = 0x8000_0005;
const EVENT_DDCCI: u32 = 0x8000_0006;

const fn drm_iowr(driver_nr: u32, size: usize) -> u32 {
    0xC000_0000 | ((size as u32) << 16) | (DRM_IOCTL_BASE << 8) | driver_nr
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');

/// `DRM_IOWR(0x00, struct drm_version)`.
const DRM_IOCTL_VERSION: u32 = drm_iowr(DRM_IOCTL_VERSION_NR, size_of::<DrmVersion>());
/// `DRM_IOWR(DRM_COMMAND_BASE + 0, struct drm_evdi_connect)`.
const DRM_IOCTL_EVDI_CONNECT: u32 = drm_iowr(DRM_COMMAND_BASE, size_of::<EvdiConnect>());
/// `DRM_IOWR(DRM_COMMAND_BASE + 1, struct drm_evdi_request_update)`.
const DRM_IOCTL_EVDI_REQUEST_UPDATE: u32 =
    drm_iowr(DRM_COMMAND_BASE + 1, size_of::<EvdiRequestUpdate>());
/// `DRM_IOWR(DRM_COMMAND_BASE + 2, struct drm_evdi_grabpix)`.
const DRM_IOCTL_EVDI_GRABPIX: u32 = drm_iowr(DRM_COMMAND_BASE + 2, size_of::<EvdiGrabpix>());
/// `DRM_IOWR(DRM_COMMAND_BASE + 4, struct drm_evdi_enable_cursor_events)`.
const DRM_IOCTL_EVDI_ENABLE_CURSOR_EVENTS: u32 = drm_iowr(
    DRM_EVDI_ENABLE_CURSOR_EVENTS_NR,
    size_of::<EvdiEnableCursorEvents>(),
);
/// `DRM_IOCTL_MODE_MAP_DUMB`, which turns a GEM handle into an mmap offset.
///
/// The ioctl is named for dumb buffers, but evdi routes it through the same
/// `dumb_map_offset` hook that makes any of its GEM objects mappable, and the
/// older `DRM_IOCTL_GEM_MMAP` no longer exists on current kernels.
const DRM_IOCTL_MODE_MAP_DUMB: u32 = drm_iowr(DRM_IOCTL_MODE_MAP_DUMB_NR, size_of::<DrmMapDumb>());

#[repr(C)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

#[repr(C)]
struct EvdiConnect {
    connected: i32,
    dev_index: i32,
    edid: *const u8,
    edid_length: u32,
    pixel_area_limit: u32,
    pixel_per_second_limit: u32,
}

#[repr(C)]
struct EvdiRequestUpdate {
    reserved: i32,
}

#[repr(C)]
struct EvdiGrabpix {
    mode: i32,
    buf_width: i32,
    buf_height: i32,
    buf_byte_stride: i32,
    buffer: *mut u8,
    num_rects: i32,
    rects: *mut DrmClipRect,
}

/// `struct drm_evdi_enable_cursor_events`; the base fields are unused.
#[repr(C)]
struct EvdiEnableCursorEvents {
    event_type: u32,
    length: u32,
    enable: u8,
}

/// `struct drm_mode_map_dumb`.
#[repr(C)]
struct DrmMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DrmClipRect {
    x1: u16,
    y1: u16,
    x2: u16,
    y2: u16,
}

/// Driver version reported by `DRM_IOCTL_VERSION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DriverVersion {
    /// Major version.
    pub major: i32,
    /// Minor version.
    pub minor: i32,
    /// Patch level.
    pub patch: i32,
}

impl fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Summary of one usable evdi card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CardInfo {
    /// Card index, the `N` in `/dev/dri/cardN`.
    pub index: i32,
    /// Device node.
    pub path: PathBuf,
    /// evdi module version.
    pub version: DriverVersion,
    /// Physical output port the connector is attached to.
    pub connector: Option<String>,
}

/// Pixel memory layout of grabbed frames, ready for ffmpeg's `-pix_fmt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    /// Bytes are B, G, R, X; ffmpeg name `bgra`.
    Bgra,
    /// Bytes are R, G, B, X; ffmpeg name `rgba`.
    Rgba,
}

impl PixelFormat {
    /// Bytes per pixel; every supported format is a packed 32-bit word.
    pub(crate) const BYTES_PER_PIXEL: usize = 4;

    /// Maps a DRM fourcc to the byte order seen in memory.
    ///
    /// DRM formats are little-endian packed words, so `XRGB8888`
    /// (`0x00RRGGBB`) reaches memory as B, G, R, X.
    #[must_use]
    pub(crate) const fn from_drm(format: u32) -> Option<Self> {
        match format {
            DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 => Some(Self::Bgra),
            DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888 => Some(Self::Rgba),
            _ => None,
        }
    }
}

/// A geometry and pixel layout announced by the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh rate the compositor picked.
    pub refresh_hz: u32,
    /// Pixel layout of the scanout buffer.
    pub format: PixelFormat,
}

/// One event read from the evdi card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// Pixels changed and the display may be grabbed.
    UpdateReady,
    /// Monitor power state changed; non-zero means on.
    Dpms {
        /// DPMS mode reported by the kernel.
        mode: i32,
    },
    /// The compositor configured a new mode.
    ModeChanged {
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Refresh rate in hertz.
        refresh_hz: u32,
        /// Bits per pixel of the scanout buffer.
        bits_per_pixel: u32,
        /// DRM fourcc of the scanout buffer.
        pixel_format: u32,
    },
    /// The CRTC was enabled or disabled.
    CrtcState {
        /// Non-zero when the CRTC is active.
        state: i32,
    },
    /// The pointer bitmap changed; the handle names its GEM buffer.
    CursorSet {
        /// Whether the pointer is visible at all.
        enabled: bool,
        /// Hotspot column inside the bitmap.
        hot_x: i32,
        /// Hotspot row inside the bitmap.
        hot_y: i32,
        /// Bitmap width in pixels.
        width: u32,
        /// Bitmap height in pixels.
        height: u32,
        /// GEM handle of the bitmap, valid on the card that sent the event.
        handle: u32,
        /// Size of that GEM buffer in bytes.
        buffer_length: u32,
        /// DRM fourcc of the bitmap pixels.
        pixel_format: u32,
        /// Bytes between the start of two bitmap rows.
        stride: u32,
    },
    /// The pointer moved; coordinates are the bitmap's top-left corner.
    CursorMove {
        /// Horizontal position on the virtual display.
        x: i32,
        /// Vertical position on the virtual display.
        y: i32,
    },
    /// DDC/CI channel activity; ignored.
    Ddcci,
    /// Event kind this build does not handle.
    Unknown {
        /// Raw event type.
        kind: u32,
        /// Total event length including the eight-byte header.
        length: u32,
    },
}

impl Event {
    /// The compositor mode carried by [`Event::ModeChanged`], when the pixel
    /// format is one this pipeline can encode.
    #[must_use]
    pub(crate) const fn mode(self) -> Option<Mode> {
        match self {
            Self::ModeChanged {
                width,
                height,
                refresh_hz,
                pixel_format,
                ..
            } => match PixelFormat::from_drm(pixel_format) {
                Some(format) => Some(Mode {
                    width,
                    height,
                    refresh_hz,
                    format,
                }),
                None => None,
            },
            _ => None,
        }
    }
}

/// One rectangle of pixels that the last grab refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClipRect {
    /// Left edge, inclusive.
    pub x1: u16,
    /// Top edge, inclusive.
    pub y1: u16,
    /// Right edge, exclusive.
    pub x2: u16,
    /// Bottom edge, exclusive.
    pub y2: u16,
}

impl From<DrmClipRect> for ClipRect {
    fn from(rect: DrmClipRect) -> Self {
        Self {
            x1: rect.x1,
            y1: rect.y1,
            x2: rect.x2,
            y2: rect.y2,
        }
    }
}

/// A persistent frame buffer kept in sync by dirty-rectangle grabs.
#[derive(Debug)]
pub(crate) struct FrameBuffer {
    width: u32,
    height: u32,
    stride: usize,
    pixels: Vec<u8>,
}

impl FrameBuffer {
    /// Allocates a zeroed buffer matching `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::InvalidMode`] when the geometry does not fit the
    /// kernel's signed 32-bit stride fields.
    pub(crate) fn new(mode: Mode) -> Result<Self, EvdiError> {
        let bytes_per_pixel = PixelFormat::BYTES_PER_PIXEL;
        let stride = usize::try_from(mode.width)
            .ok()
            .and_then(|width| width.checked_mul(bytes_per_pixel))
            .filter(|stride| i32::try_from(*stride).is_ok())
            .ok_or(EvdiError::InvalidMode {
                width: mode.width,
                height: mode.height,
            })?;
        let length = usize::try_from(mode.height)
            .ok()
            .and_then(|height| height.checked_mul(stride))
            .ok_or(EvdiError::InvalidMode {
                width: mode.width,
                height: mode.height,
            })?;
        Ok(Self {
            width: mode.width,
            height: mode.height,
            stride,
            pixels: vec![0; length],
        })
    }

    /// Width in pixels.
    #[must_use]
    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes between the start of two rows.
    #[must_use]
    pub(crate) const fn stride(&self) -> usize {
        self.stride
    }

    /// The whole frame, row-major with `stride` bytes per row.
    #[must_use]
    pub(crate) fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Copies the whole picture from another buffer of the same display.
    pub(crate) fn copy_all_from(&mut self, source: &Self) {
        debug_assert_eq!(
            (source.width(), source.height(), source.stride()),
            (self.width, self.height, self.stride),
            "both buffers belong to the same display"
        );
        self.pixels.copy_from_slice(&source.pixels);
    }

    /// Copies the rectangles a grab just refreshed from another buffer.
    ///
    /// The rectangles arrive aligned to whole 2x2 blocks, so every row is one
    /// run of pixels; whatever they do not cover keeps the frame before.
    ///
    /// The copy runs on several threads because the session cannot grab the
    /// next picture until it is done, and it would otherwise sit in front of
    /// that grab as a whole frame of memory traffic.
    pub(crate) fn copy_rects_from(&mut self, source: &Self, rects: &[ClipRect]) {
        debug_assert_eq!(
            (source.width(), source.height(), source.stride()),
            (self.width, self.height, self.stride),
            "both buffers belong to the same display"
        );
        debug_assert!(
            rects.iter().all(|rect| rect.x1 % 2 == 0
                && rect.y1 % 2 == 0
                && rect.x2 % 2 == 0
                && rect.y2 % 2 == 0),
            "rectangles are rounded out before they are copied"
        );
        let threads = copy_threads(rects);
        let stride = self.stride;
        let rows = usize::try_from(self.height).unwrap_or(0);
        let source_pixels = &source.pixels;
        if threads == 1 {
            copy_rows(&mut self.pixels, 0, stride, source_pixels, rects, 0, rows);
            return;
        }
        let pixels = &mut self.pixels;
        thread::scope(|scope| {
            let mut rest: &mut [u8] = pixels;
            for index in 0..threads {
                let y1 = index * rows / threads;
                let y2 = (index + 1) * rows / threads;
                let (band, tail) = rest.split_at_mut((y2 - y1) * stride);
                rest = tail;
                scope.spawn(move || copy_rows(band, y1, stride, source_pixels, rects, y1, y2));
            }
        });
    }

    /// The whole frame, mutable; only tests paint a buffer this way.
    #[cfg(test)]
    pub(crate) fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }
}

/// Pixels below which copying on the calling thread beats waking others.
const COPY_PARALLEL_MIN_PIXELS: usize = 256 * 1024;

/// Threads a rectangle copy is split across. The encoder thread is converting
/// the picture before this one at the same time and already uses several cores,
/// so the copy does not take all of them.
const COPY_MAX_THREADS: usize = 4;

/// How many threads this rectangle copy deserves.
fn copy_threads(rects: &[ClipRect]) -> usize {
    let pixels: usize = rects
        .iter()
        .map(|rect| {
            usize::from(rect.x2.saturating_sub(rect.x1))
                .saturating_mul(usize::from(rect.y2.saturating_sub(rect.y1)))
        })
        .sum();
    if pixels < COPY_PARALLEL_MIN_PIXELS {
        return 1;
    }
    let cores = thread::available_parallelism().map_or(1, std::num::NonZero::get);
    cores.clamp(1, COPY_MAX_THREADS)
}

/// Copies the rows of every rectangle that fall inside this thread's band.
///
/// `dst` starts at row `origin`; the band covers rows `y1..y2`.
fn copy_rows(
    dst: &mut [u8],
    origin: usize,
    stride: usize,
    src: &[u8],
    rects: &[ClipRect],
    y1: usize,
    y2: usize,
) {
    let bytes = PixelFormat::BYTES_PER_PIXEL;
    for rect in rects {
        let first = usize::from(rect.y1).clamp(y1, y2);
        let last = usize::from(rect.y2).clamp(y1, y2);
        let x1 = usize::from(rect.x1) * bytes;
        let x2 = usize::from(rect.x2) * bytes;
        for y in first..last {
            let (start, end) = (y * stride + x1, y * stride + x2);
            if start >= end || end > src.len() {
                continue;
            }
            let base = (y - origin) * stride;
            if base + x2 > dst.len() {
                continue;
            }
            dst[base + x1..base + x2].copy_from_slice(&src[start..end]);
        }
    }
}

/// An opened evdi card.
#[derive(Debug)]
pub(crate) struct EvdiCard {
    file: File,
    path: PathBuf,
    index: i32,
}

impl EvdiCard {
    /// Opens the first evdi card that supports this client.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::NoDevice`] when no evdi card exists,
    /// [`EvdiError::PermissionDenied`] when every candidate node was
    /// unreadable, and [`EvdiError::Incompatible`] when the module is older
    /// than the layout this client speaks.
    pub(crate) fn open_first() -> Result<Self, EvdiError> {
        let mut denied = Vec::new();
        for index in 0..=MAX_CARD_INDEX {
            let path = PathBuf::from(format!("/dev/dri/card{index}"));
            match probe(&path) {
                Ok(Some((file, version))) => {
                    if version.major != COMPAT_MAJOR || version.minor < COMPAT_MINOR {
                        return Err(EvdiError::Incompatible { path, version });
                    }
                    let card = Self {
                        file,
                        path: path.clone(),
                        index,
                    };
                    return Ok(card);
                }
                Ok(None) => {}
                Err(error) if error.kind() == ErrorKind::PermissionDenied => denied.push(path),
                Err(error) => {
                    log::debug!("skipping {}: {error}", path.display());
                }
            }
        }
        if !denied.is_empty() {
            return Err(EvdiError::PermissionDenied { paths: denied });
        }
        Err(EvdiError::NoDevice)
    }

    /// Lists every usable evdi card without keeping any of them open.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::PermissionDenied`] when candidate nodes exist but
    /// none could be read, and [`EvdiError::Incompatible`] when a card speaks
    /// an older evdi layout.
    pub(crate) fn scan() -> Result<Vec<CardInfo>, EvdiError> {
        let mut cards = Vec::new();
        let mut denied = Vec::new();
        for index in 0..=MAX_CARD_INDEX {
            let path = PathBuf::from(format!("/dev/dri/card{index}"));
            match probe(&path) {
                Ok(Some((_file, version))) => {
                    if version.major != COMPAT_MAJOR || version.minor < COMPAT_MINOR {
                        return Err(EvdiError::Incompatible { path, version });
                    }
                    cards.push(CardInfo {
                        index,
                        path: path.clone(),
                        version,
                        connector: connector_name(index),
                    });
                }
                Ok(None) => {}
                Err(error) if error.kind() == ErrorKind::PermissionDenied => denied.push(path),
                Err(error) => {
                    log::debug!("skipping {}: {error}", path.display());
                }
            }
        }
        if cards.is_empty() && !denied.is_empty() {
            return Err(EvdiError::PermissionDenied { paths: denied });
        }
        Ok(cards)
    }

    /// Connector name from sysfs when the kernel exposes one.
    ///
    /// This is the name the compositor and the desktop settings use for the
    /// virtual monitor.
    pub(crate) fn name(&self) -> Option<String> {
        connector_name(self.index)
    }

    /// Publishes `edid` as the monitor of this virtual display.
    ///
    /// The compositor reacts by sending a [`Event::ModeChanged`] once it
    /// configures the connector.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::BadEdid`] for a blob shorter than a base block and
    /// [`EvdiError::Ioctl`] when the kernel rejects the connection.
    pub(crate) fn connect(
        &self,
        edid: &[u8],
        pixel_area_limit: u32,
        pixel_per_second_limit: u32,
    ) -> Result<(), EvdiError> {
        if edid.len() < 128 {
            return Err(EvdiError::BadEdid { length: edid.len() });
        }
        let edid_length =
            u32::try_from(edid.len()).map_err(|_| EvdiError::BadEdid { length: edid.len() })?;
        let mut cmd = EvdiConnect {
            connected: 1,
            dev_index: self.index,
            edid: edid.as_ptr(),
            edid_length,
            pixel_area_limit,
            pixel_per_second_limit,
        };
        self.ioctl(DRM_IOCTL_EVDI_CONNECT, &mut cmd).map(|_| ())
    }

    /// Detaches this client from the virtual display.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Ioctl`] when the kernel rejects the request; the
    /// link is still torn down when the card is closed or dropped.
    pub(crate) fn disconnect(&self) -> Result<(), EvdiError> {
        let mut cmd = EvdiConnect {
            connected: 0,
            dev_index: self.index,
            edid: std::ptr::null(),
            edid_length: 0,
            pixel_area_limit: 0,
            pixel_per_second_limit: 0,
        };
        self.ioctl(DRM_IOCTL_EVDI_CONNECT, &mut cmd).map(|_| ())
    }

    /// Asks the driver to report the pointer instead of blending it in.
    ///
    /// The bitmap then arrives as [`Event::CursorSet`] and the position as
    /// [`Event::CursorMove`], which lets the phone draw the pointer itself.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Ioctl`] when the module predates the ioctl or the
    /// kernel rejects it. The caller treats that as "keep the blended pointer".
    pub(crate) fn enable_cursor_events(&self) -> Result<(), EvdiError> {
        self.set_cursor_events(1)
    }

    /// Gives the pointer back to the compositor, which blends it into grabs.
    ///
    /// # Errors
    ///
    /// Like [`EvdiCard::enable_cursor_events`].
    pub(crate) fn disable_cursor_events(&self) -> Result<(), EvdiError> {
        self.set_cursor_events(0)
    }

    fn set_cursor_events(&self, enable: u8) -> Result<(), EvdiError> {
        let mut cmd = EvdiEnableCursorEvents {
            event_type: 0,
            length: 0,
            enable,
        };
        self.ioctl(DRM_IOCTL_EVDI_ENABLE_CURSOR_EVENTS, &mut cmd)
            .map(|_| ())
    }

    /// Copies one cursor bitmap out of the kernel and converts it to RGBA.
    ///
    /// The handle and geometry come straight from [`Event::CursorSet`]. The
    /// handle is left open: evdi reuses it for every queued event naming the
    /// same bitmap, and closing it would make the next one fail to map.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Cursor`] for a format or geometry this code cannot
    /// read, and [`EvdiError::CursorMap`] when the GEM buffer cannot be mapped.
    pub(crate) fn cursor_rgba(
        &self,
        handle: u32,
        buffer_length: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixel_format: u32,
    ) -> Result<Vec<u8>, EvdiError> {
        let Some(order) = PixelFormat::from_drm(pixel_format) else {
            return Err(EvdiError::Cursor {
                reason: format!("unsupported pixel format 0x{pixel_format:08x}"),
            });
        };
        let width = usize::try_from(width).unwrap_or(0);
        let height = usize::try_from(height).unwrap_or(0);
        let length = usize::try_from(buffer_length).unwrap_or(0);
        let stride = usize::try_from(stride).unwrap_or(0);
        let row = width.saturating_mul(PixelFormat::BYTES_PER_PIXEL);
        if row == 0 || height == 0 || stride < row || stride.saturating_mul(height) > length {
            return Err(EvdiError::Cursor {
                reason: format!("{width}x{height} at stride {stride} does not fit {length} bytes"),
            });
        }

        let mut map = DrmMapDumb {
            handle,
            pad: 0,
            offset: 0,
        };
        self.ioctl(DRM_IOCTL_MODE_MAP_DUMB, &mut map)?;
        let Ok(offset) = libc::off_t::try_from(map.offset) else {
            return Err(EvdiError::Cursor {
                reason: format!("mmap offset {} is out of range", map.offset),
            });
        };
        // SAFETY: the offset names this handle's GEM object, which the kernel
        // reports as `length` bytes long; the mapping is read-only and lives
        // only until the copy below is finished.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                offset,
            )
        };
        if mapped == libc::MAP_FAILED {
            let source = io::Error::last_os_error();
            return Err(EvdiError::CursorMap { source });
        }
        // SAFETY: `mapped` is a live read-only mapping of at least `length`
        // bytes, which the slice borrows for the duration of the copy.
        let source = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), length) };
        let keep_alpha = matches!(pixel_format, DRM_FORMAT_ARGB8888 | DRM_FORMAT_ABGR8888);
        let mut pixels = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
        for row_pixels in source.chunks_exact(stride).take(height) {
            for pixel in row_pixels[..row].chunks_exact(PixelFormat::BYTES_PER_PIXEL) {
                // Both layouts are little-endian words; the memory order is
                // what `PixelFormat::from_drm` just named.
                let (red, green, blue, alpha) = match order {
                    PixelFormat::Bgra => (pixel[2], pixel[1], pixel[0], pixel[3]),
                    PixelFormat::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
                };
                pixels.extend_from_slice(&[
                    red,
                    green,
                    blue,
                    if keep_alpha { alpha } else { u8::MAX },
                ]);
            }
        }
        // SAFETY: the mapping was made above with exactly these bounds.
        unsafe {
            libc::munmap(mapped, length);
        }
        // The handle stays open on purpose: evdi reuses one handle for every
        // cursor event that names the same GEM object, so closing it here makes
        // the events already queued behind this one unreadable. The file close
        // releases them all when the session ends.
        Ok(pixels)
    }

    /// Asks the kernel whether a frame can be grabbed right now.
    ///
    /// Returns `true` when dirty rectangles are already pending, `false` when
    /// the caller must wait for [`Event::UpdateReady`] first.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Ioctl`] when the kernel rejects the request.
    pub(crate) fn request_update(&self) -> Result<bool, EvdiError> {
        let mut cmd = EvdiRequestUpdate { reserved: 0 };
        let result = self.ioctl(DRM_IOCTL_EVDI_REQUEST_UPDATE, &mut cmd)?;
        Ok(result == 1)
    }

    /// Copies the pixels that changed since the previous grab into `frame`.
    ///
    /// `frame` must match the current compositor mode; geometry changes cause
    /// [`EvdiError::ModeMismatch`] so the caller can allocate a new buffer.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::NotReady`] when no scanout buffer is configured
    /// yet, [`EvdiError::ModeMismatch`] for stale geometry,
    /// [`EvdiError::StaleRects`] for the first grab after the display changed
    /// shape, and [`EvdiError::Ioctl`] for other kernel failures.
    pub(crate) fn grab(&self, frame: &mut FrameBuffer) -> Result<Vec<ClipRect>, EvdiError> {
        let buf_width = i32::try_from(frame.width).map_err(|_| EvdiError::InvalidMode {
            width: frame.width,
            height: frame.height,
        })?;
        let buf_height = i32::try_from(frame.height).map_err(|_| EvdiError::InvalidMode {
            width: frame.width,
            height: frame.height,
        })?;
        let stride = i32::try_from(frame.stride).map_err(|_| EvdiError::InvalidMode {
            width: frame.width,
            height: frame.height,
        })?;
        let mut rects = [DrmClipRect::default(); MAX_DIRTY_RECTS];
        let mut cmd = EvdiGrabpix {
            mode: GRABPIX_MODE_DIRTY,
            buf_width,
            buf_height,
            buf_byte_stride: stride,
            buffer: frame.pixels.as_mut_ptr(),
            num_rects: i32::try_from(MAX_DIRTY_RECTS).unwrap_or(i32::MAX),
            rects: rects.as_mut_ptr(),
        };
        let result = self.ioctl(DRM_IOCTL_EVDI_GRABPIX, &mut cmd);
        match result {
            Ok(_) => {}
            Err(EvdiError::Ioctl { source, .. }) => {
                return match source.raw_os_error() {
                    Some(libc::EAGAIN) => Err(EvdiError::NotReady),
                    Some(libc::EFAULT) => Err(EvdiError::StaleRects),
                    Some(libc::EINVAL) => Err(EvdiError::ModeMismatch {
                        width: frame.width,
                        height: frame.height,
                    }),
                    _ => Err(EvdiError::Ioctl {
                        path: self.path.clone(),
                        request: "grabpix",
                        source,
                    }),
                };
            }
            Err(other) => return Err(other),
        }
        let count = usize::try_from(cmd.num_rects)
            .unwrap_or(0)
            .min(MAX_DIRTY_RECTS);
        Ok(rects[..count].iter().copied().map(ClipRect::from).collect())
    }

    /// Waits until the card has an event to read.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Poll`] when the wait fails for a reason other than
    /// a signal interrupt.
    pub(crate) fn wait_readable(&self, timeout: Duration) -> Result<bool, EvdiError> {
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut poll_fd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: `poll_fd` is one live, properly aligned `pollfd` and the
            // kernel only reads it and writes its `revents` field.
            let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd), 1, millis) };
            if ready < 0 {
                let source = io::Error::last_os_error();
                if source.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(EvdiError::Poll { source });
            }
            return Ok(ready > 0 && poll_fd.revents & libc::POLLIN != 0);
        }
    }

    /// Drains all queued events without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::Read`] when the card reports a failure other than
    /// "no data yet".
    pub(crate) fn read_events(&self) -> Result<Vec<Event>, EvdiError> {
        let mut buf = [0_u8; 1024];
        let count = match (&self.file).read(&mut buf) {
            Ok(0) => return Ok(Vec::new()),
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(Vec::new()),
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(source) => return Err(EvdiError::Read { source }),
        };
        Ok(parse_events(&buf[..count]))
    }

    fn ioctl(&self, request: u32, arg: &mut impl Sized) -> Result<i32, EvdiError> {
        let fd = self.file.as_raw_fd();
        // SAFETY: `fd` is the live evdi card descriptor and `arg` points to a
        // live, properly aligned `#[repr(C)]` value sized for `request`; the
        // kernel only reads and writes the documented fields of that value.
        let result = unsafe {
            libc::ioctl(
                fd,
                c_ulong::from(request),
                std::ptr::from_mut(arg).cast::<libc::c_void>(),
            )
        };
        if result < 0 {
            return Err(EvdiError::Ioctl {
                path: self.path.clone(),
                request: ioctl_name(request),
                source: io::Error::last_os_error(),
            });
        }
        Ok(result)
    }
}

const fn ioctl_name(request: u32) -> &'static str {
    match request {
        DRM_IOCTL_VERSION => "version",
        DRM_IOCTL_EVDI_CONNECT => "connect",
        DRM_IOCTL_EVDI_REQUEST_UPDATE => "request_update",
        DRM_IOCTL_EVDI_GRABPIX => "grabpix",
        DRM_IOCTL_EVDI_ENABLE_CURSOR_EVENTS => "enable_cursor_events",
        DRM_IOCTL_MODE_MAP_DUMB => "map_dumb",
        _ => "ioctl",
    }
}

/// Opens `path` and reports whether it is an evdi card.
///
/// `Ok(None)` means the node is missing or belongs to another DRM driver.
fn probe(path: &Path) -> io::Result<Option<(File, DriverVersion)>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let (name, version) = read_version(file.as_raw_fd())?;
    if name != "evdi" {
        return Ok(None);
    }
    Ok(Some((file, version)))
}

fn read_version(fd: RawFd) -> io::Result<(String, DriverVersion)> {
    let mut name = [0_u8; 64];
    let mut version = DrmVersion {
        version_major: 0,
        version_minor: 0,
        version_patchlevel: 0,
        name_len: name.len(),
        name: name.as_mut_ptr(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    // SAFETY: `fd` is a live DRM descriptor and `version` outlives the call;
    // `name` is a valid buffer whose length is declared in `name_len`.
    let result = unsafe {
        libc::ioctl(
            fd,
            c_ulong::from(DRM_IOCTL_VERSION),
            std::ptr::from_mut(&mut version).cast::<libc::c_void>(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let length = version.name_len.min(name.len());
    let name = String::from_utf8_lossy(&name[..length]).into_owned();
    Ok((
        name,
        DriverVersion {
            major: version.version_major,
            minor: version.version_minor,
            patch: version.version_patchlevel,
        },
    ))
}

/// Connector name from sysfs when the kernel exposes one.
fn connector_name(index: i32) -> Option<String> {
    let dir = std::fs::read_dir(format!("/sys/class/drm/card{index}")).ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(port) = name.strip_prefix(&format!("card{index}-")) {
            if entry.path().join("status").exists() {
                return Some(port.to_owned());
            }
        }
    }
    None
}

/// Parses one read of the DRM event stream.
///
/// A read always returns whole events; a truncated tail is dropped rather than
/// misinterpreted.
pub(crate) fn parse_events(bytes: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut offset = 0_usize;
    while let Some(header) = bytes.get(offset..offset + 8) {
        let kind = u32::from_ne_bytes([header[0], header[1], header[2], header[3]]);
        let raw_length = u32::from_ne_bytes([header[4], header[5], header[6], header[7]]);
        let length = usize::try_from(raw_length).unwrap_or(0);
        if length < 8 || offset + length > bytes.len() {
            break;
        }
        let body = &bytes[offset + 8..offset + length];
        events.push(parse_event(kind, raw_length, body));
        offset += length;
    }
    events
}

fn parse_event(kind: u32, length: u32, body: &[u8]) -> Event {
    match kind {
        EVENT_UPDATE_READY => Event::UpdateReady,
        EVENT_DPMS => {
            read_i32(body, 0).map_or(Event::Unknown { kind, length }, |mode| Event::Dpms { mode })
        }
        EVENT_MODE_CHANGED => {
            let fields = (
                read_i32(body, 0),
                read_i32(body, 4),
                read_i32(body, 8),
                read_i32(body, 12),
                read_u32(body, 16),
            );
            match fields {
                (Some(width), Some(height), Some(refresh), Some(bpp), Some(format)) => {
                    Event::ModeChanged {
                        width: u32::try_from(width).unwrap_or(0),
                        height: u32::try_from(height).unwrap_or(0),
                        refresh_hz: u32::try_from(refresh).unwrap_or(0),
                        bits_per_pixel: u32::try_from(bpp).unwrap_or(0),
                        pixel_format: format,
                    }
                }
                _ => Event::Unknown { kind, length },
            }
        }
        EVENT_CRTC_STATE => read_i32(body, 0).map_or(Event::Unknown { kind, length }, |state| {
            Event::CrtcState { state }
        }),
        EVENT_CURSOR_SET => {
            // hot_x, hot_y, width, height, enabled, handle, length, format,
            // stride, with the base header already stripped.
            if body.len() < 36 {
                return Event::Unknown { kind, length };
            }
            let width = read_i32(body, 8).unwrap_or(0);
            let height = read_i32(body, 12).unwrap_or(0);
            Event::CursorSet {
                enabled: body[16] != 0,
                hot_x: read_i32(body, 0).unwrap_or(0),
                hot_y: read_i32(body, 4).unwrap_or(0),
                width: u32::try_from(width).unwrap_or(0),
                height: u32::try_from(height).unwrap_or(0),
                handle: read_u32(body, 20).unwrap_or(0),
                buffer_length: read_u32(body, 24).unwrap_or(0),
                pixel_format: read_u32(body, 28).unwrap_or(0),
                stride: read_u32(body, 32).unwrap_or(0),
            }
        }
        EVENT_CURSOR_MOVE => match (read_i32(body, 0), read_i32(body, 4)) {
            (Some(x), Some(y)) => Event::CursorMove { x, y },
            _ => Event::Unknown { kind, length },
        },
        EVENT_DDCCI => Event::Ddcci,
        _ => Event::Unknown { kind, length },
    }
}

fn read_i32(body: &[u8], offset: usize) -> Option<i32> {
    let bytes = body.get(offset..offset + 4)?;
    Some(i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u32(body: &[u8], offset: usize) -> Option<u32> {
    let bytes = body.get(offset..offset + 4)?;
    Some(u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Failure while talking to the evdi module.
#[derive(Debug, Error)]
pub(crate) enum EvdiError {
    /// No evdi card exists, usually because the module is not loaded.
    #[error(
        "no evdi virtual display found; load the evdi kernel module with \
         `initial_device_count` greater than zero"
    )]
    NoDevice,
    /// Candidate DRM nodes exist but the daemon cannot open them.
    #[error("permission denied for evdi candidates: {paths:?}")]
    PermissionDenied {
        /// Nodes that could not be opened.
        paths: Vec<PathBuf>,
    },
    /// The module predates the ioctl layout used here.
    #[error(
        "evdi card {path} reports version {version}, but {COMPAT_MAJOR}.{COMPAT_MINOR}+ is required",
        path = path.display()
    )]
    Incompatible {
        /// Device node.
        path: PathBuf,
        /// Reported version.
        version: DriverVersion,
    },
    /// The EDID blob cannot be presented to the kernel.
    #[error("invalid EDID blob of {length} bytes")]
    BadEdid {
        /// Length of the rejected blob.
        length: usize,
    },
    /// The geometry does not fit the kernel's stride fields.
    #[error("display geometry {width}x{height} cannot be grabbed")]
    InvalidMode {
        /// Rejected width.
        width: u32,
        /// Rejected height.
        height: u32,
    },
    /// No scanout buffer is attached to the virtual display yet.
    #[error("evdi frame not ready")]
    NotReady,
    /// The frame buffer does not match the compositor mode.
    #[error("frame buffer {width}x{height} does not match the evdi mode")]
    ModeMismatch {
        /// Width of the stale buffer.
        width: u32,
        /// Height of the stale buffer.
        height: u32,
    },
    /// Rectangles left over from the display's previous shape.
    ///
    /// The kernel keeps dirty rectangles across a mode change and hands them to
    /// the first grab of the new one, which then refuses to copy what no longer
    /// fits. The rejected attempt clears them, so the grab after it sees only
    /// the new geometry.
    #[error("evdi offered rectangles from before the display changed shape")]
    StaleRects,
    /// An ioctl was rejected.
    #[error("evdi {request} on {path} failed: {source}", path = path.display())]
    Ioctl {
        /// Device node.
        path: PathBuf,
        /// Operation name.
        request: &'static str,
        /// Kernel error.
        source: io::Error,
    },
    /// Waiting for events failed.
    #[error("polling evdi events failed: {source}")]
    Poll {
        /// Kernel error.
        source: io::Error,
    },
    /// Reading events failed.
    #[error("reading evdi events failed: {source}")]
    Read {
        /// Kernel error.
        source: io::Error,
    },
    /// A cursor bitmap the kernel offered cannot be interpreted.
    #[error("cannot use the cursor bitmap: {reason}")]
    Cursor {
        /// What about it is unusable.
        reason: String,
    },
    /// A cursor bitmap could not be mapped into this process.
    #[error("cannot map the cursor bitmap: {source}")]
    CursorMap {
        /// Kernel error.
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_bytes(kind: u32, body: &[u8]) -> Vec<u8> {
        let total = u32::try_from(8 + body.len()).expect("event fits");
        let mut bytes = Vec::with_capacity(8 + body.len());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        bytes.extend_from_slice(&total.to_ne_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn mode_event(width: i32, height: i32, refresh: i32, format: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&width.to_ne_bytes());
        body.extend_from_slice(&height.to_ne_bytes());
        body.extend_from_slice(&refresh.to_ne_bytes());
        body.extend_from_slice(&32_i32.to_ne_bytes());
        body.extend_from_slice(&format.to_ne_bytes());
        event_bytes(EVENT_MODE_CHANGED, &body)
    }

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(size_of::<DrmVersion>(), 64, "drm_version layout");
        assert_eq!(size_of::<EvdiConnect>(), 32, "drm_evdi_connect layout");
        assert_eq!(size_of::<EvdiRequestUpdate>(), 4, "request_update layout");
        assert_eq!(size_of::<EvdiGrabpix>(), 40, "drm_evdi_grabpix layout");
        assert_eq!(size_of::<DrmClipRect>(), 8, "drm_clip_rect layout");
        assert_eq!(DRM_IOCTL_VERSION, 0xc040_6400);
        assert_eq!(DRM_IOCTL_EVDI_CONNECT, 0xc020_6440);
        assert_eq!(DRM_IOCTL_EVDI_REQUEST_UPDATE, 0xc004_6441);
        assert_eq!(DRM_IOCTL_EVDI_GRABPIX, 0xc028_6442);
    }

    #[test]
    fn drm_formats_map_to_memory_order() {
        assert_eq!(
            PixelFormat::from_drm(u32::from_le_bytes(*b"XR24")),
            Some(PixelFormat::Bgra)
        );
        assert_eq!(
            PixelFormat::from_drm(u32::from_le_bytes(*b"AR24")),
            Some(PixelFormat::Bgra)
        );
        assert_eq!(
            PixelFormat::from_drm(u32::from_le_bytes(*b"XB24")),
            Some(PixelFormat::Rgba)
        );
        assert_eq!(
            PixelFormat::from_drm(u32::from_le_bytes(*b"AB24")),
            Some(PixelFormat::Rgba)
        );
        assert_eq!(PixelFormat::from_drm(u32::from_le_bytes(*b"RG24")), None);
    }

    #[test]
    fn frame_buffer_layout_is_tightly_packed() {
        let frame = FrameBuffer::new(Mode {
            width: 2400,
            height: 1080,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        })
        .expect("geometry fits");
        assert_eq!(frame.stride(), 9600);
        assert_eq!(frame.pixels().len(), 9600 * 1080);
        assert_eq!(frame.width(), 2400);
        assert_eq!(frame.height(), 1080);
    }

    #[test]
    fn frame_buffer_rejects_overflowing_geometry() {
        assert!(matches!(
            FrameBuffer::new(Mode {
                width: u32::MAX,
                height: 2,
                refresh_hz: 60,
                format: PixelFormat::Bgra,
            }),
            Err(EvdiError::InvalidMode { .. })
        ));
        assert!(FrameBuffer::new(Mode {
            width: 4096,
            height: 4096,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        })
        .is_ok());
    }

    #[test]
    fn rect_copy_leaves_untouched_pixels_alone() {
        let shape = Mode {
            width: 4,
            height: 4,
            refresh_hz: 60,
            format: PixelFormat::Bgra,
        };
        let mut source = FrameBuffer::new(shape).expect("geometry fits");
        let mut mirror = FrameBuffer::new(shape).expect("geometry fits");
        source.pixels_mut().fill(1);
        mirror.pixels_mut().fill(1);
        // Only the right-hand 2x2 block of the top two rows changes.
        let stride = source.stride();
        let bpp = PixelFormat::BYTES_PER_PIXEL;
        for y in 0..2 {
            source.pixels_mut()[y * stride + 2 * bpp..y * stride + 4 * bpp].fill(2);
        }
        mirror.copy_rects_from(
            &source,
            &[ClipRect {
                x1: 2,
                y1: 0,
                x2: 4,
                y2: 2,
            }],
        );
        let pixels = mirror.pixels();
        assert_eq!(&pixels[..2 * bpp], [1; 8], "the left half is untouched");
        assert_eq!(
            &pixels[2 * bpp..4 * bpp],
            [2; 8],
            "the right half was copied"
        );
        assert!(
            pixels[2 * stride..].iter().all(|pixel| *pixel == 1),
            "other rows are untouched"
        );
    }

    #[test]
    fn decodes_update_ready_and_dpms() {
        let mut bytes = event_bytes(EVENT_UPDATE_READY, &[]);
        bytes.extend(event_bytes(EVENT_DPMS, &1_i32.to_ne_bytes()));
        let events = parse_events(&bytes);
        assert_eq!(events, vec![Event::UpdateReady, Event::Dpms { mode: 1 }]);
    }

    #[test]
    fn decodes_mode_changed() {
        let bytes = mode_event(2400, 1080, 60, u32::from_le_bytes(*b"XR24"));
        let events = parse_events(&bytes);
        assert_eq!(
            events,
            vec![Event::ModeChanged {
                width: 2400,
                height: 1080,
                refresh_hz: 60,
                bits_per_pixel: 32,
                pixel_format: u32::from_le_bytes(*b"XR24"),
            }]
        );
        let mode = events[0].mode().expect("supported format");
        assert_eq!(mode.width, 2400);
        assert_eq!(mode.format, PixelFormat::Bgra);
    }

    #[test]
    fn mode_changed_with_unsupported_format_has_no_mode() {
        let bytes = mode_event(1920, 1080, 60, u32::from_le_bytes(*b"RG24"));
        let events = parse_events(&bytes);
        assert!(matches!(events[0], Event::ModeChanged { .. }));
        assert!(events[0].mode().is_none());
    }

    #[test]
    fn decodes_crtc_state_and_ignores_unknown() {
        let mut bytes = event_bytes(EVENT_CRTC_STATE, &1_i32.to_ne_bytes());
        bytes.extend(event_bytes(0x8000_00ff, &[1, 2, 3, 4]));
        let events = parse_events(&bytes);
        assert_eq!(events[0], Event::CrtcState { state: 1 });
        assert_eq!(
            events[1],
            Event::Unknown {
                kind: 0x8000_00ff,
                length: 12
            }
        );
    }

    #[test]
    fn cursor_ioctls_use_the_kernel_layout() {
        assert_eq!(size_of::<EvdiEnableCursorEvents>(), 12);
        assert_eq!(size_of::<DrmMapDumb>(), 16);
        assert_eq!(DRM_IOCTL_EVDI_ENABLE_CURSOR_EVENTS, 0xC00C_6444);
        assert_eq!(DRM_IOCTL_MODE_MAP_DUMB, 0xC010_64B3);
    }

    #[test]
    fn decodes_cursor_events() {
        let mut body = Vec::new();
        body.extend_from_slice(&12_i32.to_ne_bytes());
        body.extend_from_slice(&(-4_i32).to_ne_bytes());
        body.extend_from_slice(&16_i32.to_ne_bytes());
        body.extend_from_slice(&24_i32.to_ne_bytes());
        body.push(1);
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&7_u32.to_ne_bytes());
        body.extend_from_slice(&1536_u32.to_ne_bytes());
        body.extend_from_slice(&u32::from_le_bytes(*b"AR24").to_ne_bytes());
        body.extend_from_slice(&64_u32.to_ne_bytes());
        let mut bytes = event_bytes(EVENT_CURSOR_SET, &body);
        let mut moved = Vec::new();
        moved.extend_from_slice(&100_i32.to_ne_bytes());
        moved.extend_from_slice(&200_i32.to_ne_bytes());
        bytes.extend(event_bytes(EVENT_CURSOR_MOVE, &moved));

        let events = parse_events(&bytes);
        assert_eq!(
            events[0],
            Event::CursorSet {
                enabled: true,
                hot_x: 12,
                hot_y: -4,
                width: 16,
                height: 24,
                handle: 7,
                buffer_length: 1536,
                pixel_format: u32::from_le_bytes(*b"AR24"),
                stride: 64,
            }
        );
        assert_eq!(events[1], Event::CursorMove { x: 100, y: 200 });
    }

    #[test]
    fn truncated_cursor_set_is_unknown() {
        let bytes = event_bytes(EVENT_CURSOR_SET, &[0; 20]);
        assert_eq!(
            parse_events(&bytes)[0],
            Event::Unknown {
                kind: EVENT_CURSOR_SET,
                length: 28
            }
        );
    }

    #[test]
    fn truncated_event_tail_is_dropped() {
        let mut bytes = event_bytes(EVENT_UPDATE_READY, &[]);
        let mut partial = mode_event(1920, 1080, 60, u32::from_le_bytes(*b"XR24"));
        partial.truncate(partial.len() - 3);
        bytes.extend(partial);
        let events = parse_events(&bytes);
        assert_eq!(events, vec![Event::UpdateReady]);
    }

    #[test]
    fn garbage_event_length_stops_parsing() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&EVENT_UPDATE_READY.to_ne_bytes());
        bytes.extend_from_slice(&4_u32.to_ne_bytes());
        let events = parse_events(&bytes);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn parses_multiple_events_in_one_read() {
        let mut bytes = mode_event(1920, 1080, 60, u32::from_le_bytes(*b"AR24"));
        bytes.extend(event_bytes(EVENT_CRTC_STATE, &1_i32.to_ne_bytes()));
        bytes.extend(event_bytes(EVENT_UPDATE_READY, &[]));
        let events = parse_events(&bytes);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], Event::ModeChanged { .. }));
        assert_eq!(events[1], Event::CrtcState { state: 1 });
        assert_eq!(events[2], Event::UpdateReady);
    }
}
