//! Virtual multi-touchscreen backed by `/dev/uinput`.
//!
//! The phone reports normalised contacts; this module maps them onto the
//! virtual display and injects Linux multi-touch protocol B events so the
//! compositor treats the phone exactly like a USB touch panel.
//!
//! Where the panel's coordinates come from depends on what the desktop will do
//! with them. A desktop with the display service is told that this panel drives
//! the virtual output, so the panel speaks in that output's own pixels. A
//! Wayland desktop spreads an unmapped absolute device over the whole layout
//! and offers nobody a way to say otherwise, so the panel instead measures the
//! entire desktop and reports the global point that lands inside the virtual
//! output.

use std::ffi::c_ulong;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

use androscreen_core::touch::{TouchAction, TouchFrame, MAX_POINTS};
use thiserror::Error;

use crate::wlr::Placement;

const UINPUT_PATH: &str = "/dev/uinput";

/// Kernel name of the injected touchscreen.
pub(crate) const DEVICE_NAME: &str = "AndroScreen Touch";

const UINPUT_NAME_LEN: usize = 80;
const INPUT_PROP_DIRECT: u16 = 0x01;
const BUS_USB: u16 = 0x03;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0x00;
const BTN_TOUCH: u16 = 0x14a;

const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

const TRACKING_MAX: u32 = 65_535;
const PRESSURE_MAX: i32 = 255;

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;

const fn ioc(direction: u32, kind: u8, number: u8, size: usize) -> u32 {
    (direction << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | number as u32
}

#[repr(C)]
#[derive(Debug)]
struct InputId {
    bus_type: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Debug)]
struct UinputSetup {
    id: InputId,
    name: [u8; UINPUT_NAME_LEN],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Debug)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Debug)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

const UI_DEV_CREATE: u32 = ioc(IOC_NONE, b'U', 1, 0);
const UI_DEV_DESTROY: u32 = ioc(IOC_NONE, b'U', 2, 0);
const UI_DEV_SETUP: u32 = ioc(IOC_WRITE, b'U', 3, std::mem::size_of::<UinputSetup>());
const UI_ABS_SETUP: u32 = ioc(IOC_WRITE, b'U', 4, std::mem::size_of::<UinputAbsSetup>());
const UI_SET_EVBIT: u32 = ioc(IOC_WRITE, b'U', 100, std::mem::size_of::<i32>());
const UI_SET_KEYBIT: u32 = ioc(IOC_WRITE, b'U', 101, std::mem::size_of::<i32>());
const UI_SET_ABSBIT: u32 = ioc(IOC_WRITE, b'U', 103, std::mem::size_of::<i32>());
const UI_SET_PROPBIT: u32 = ioc(IOC_WRITE, b'U', 110, std::mem::size_of::<i32>());
const UI_SET_PHYS: u32 = ioc(IOC_WRITE, b'U', 108, std::mem::size_of::<*const u8>());

/// Identity the desktop hashes to remember which screen this device drives.
///
/// Without one every touchscreen without a serial shares the same identity, and
/// the mapping meant for the phone would steal the built-in touchscreen.
const PHYS_PATH: &str = "androscreen:usb";

/// Size of `struct input_event` on this platform.
const EVENT_LEN: usize = std::mem::size_of::<libc::timeval>() + 8;
const EVENT_KIND_OFFSET: usize = std::mem::size_of::<libc::timeval>();
const EVENT_CODE_OFFSET: usize = EVENT_KIND_OFFSET + 2;
const EVENT_VALUE_OFFSET: usize = EVENT_KIND_OFFSET + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Contact {
    phone_id: u8,
    tracking_id: u32,
}

/// Maps phone pointer ids onto the ten kernel multitouch slots.
#[derive(Debug, Default)]
struct Slots {
    active: [Option<Contact>; MAX_POINTS],
    next_tracking: u32,
}

impl Slots {
    fn allocate(&mut self, phone_id: u8) -> Option<(usize, u32)> {
        let slot = self.active.iter().position(Option::is_none)?;
        self.next_tracking = self.next_tracking % TRACKING_MAX + 1;
        let tracking_id = self.next_tracking;
        self.active[slot] = Some(Contact {
            phone_id,
            tracking_id,
        });
        Some((slot, tracking_id))
    }

    fn find(&self, phone_id: u8) -> Option<(usize, Contact)> {
        self.active
            .iter()
            .enumerate()
            .find_map(|(slot, contact)| match contact {
                Some(contact) if contact.phone_id == phone_id => Some((slot, *contact)),
                _ => None,
            })
    }

    fn release(&mut self, slot: usize) -> Option<Contact> {
        self.active.get_mut(slot).and_then(Option::take)
    }

    fn count(&self) -> usize {
        self.active.iter().flatten().count()
    }

    fn active_slots(&self) -> Vec<(usize, Contact)> {
        self.active
            .iter()
            .enumerate()
            .filter_map(|(slot, contact)| contact.map(|contact| (slot, contact)))
            .collect()
    }
}

/// An injected touchscreen.
#[derive(Debug)]
pub(crate) struct TouchDevice {
    file: File,
    mapping: Mapping,
    slots: Slots,
}

/// What the device's axis ranges cover, and where a finger is put on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mapping {
    /// The virtual output's own pixels, mapped there by the desktop.
    Output { width: u32, height: u32 },
    /// The whole desktop, so a finger has to say where the output is.
    Desktop(Placement),
}

impl Mapping {
    /// The axis limits to declare for this mapping.
    const fn span(self) -> (u32, u32) {
        match self {
            Self::Output { width, height } => (width, height),
            Self::Desktop(at) => (at.span_width, at.span_height),
        }
    }

    /// Whether the desktop was told nothing and the panel has to compensate.
    const fn reports_global(self) -> bool {
        matches!(self, Self::Desktop(_))
    }

    /// The device coordinates of one normalised phone contact.
    fn point(self, x: u16, y: u16) -> (i32, i32) {
        match self {
            Self::Output { width, height } => (scale(x, width), scale(y, height)),
            Self::Desktop(at) => {
                let global_x =
                    i64::from(at.output_left - at.left) + i64::from(scale(x, at.output_width));
                let global_y =
                    i64::from(at.output_top - at.top) + i64::from(scale(y, at.output_height));
                (
                    axis(global_x, at.span_width),
                    axis(global_y, at.span_height),
                )
            }
        }
    }
}

impl TouchDevice {
    /// Creates the virtual touchscreen for a display of `width` x `height`.
    ///
    /// `desktop` is where the compositor puts that display; with one, the panel
    /// measures the whole desktop instead, which is the only way a Wayland
    /// desktop can be reached.
    ///
    /// # Errors
    ///
    /// Returns [`TouchError::Geometry`] for a zero-sized display,
    /// [`TouchError::Open`] when `/dev/uinput` is unavailable or not writable,
    /// and [`TouchError::Ioctl`] when the kernel rejects the device setup.
    pub(crate) fn create(
        width: u32,
        height: u32,
        desktop: Option<Placement>,
    ) -> Result<Self, TouchError> {
        let mapping = match desktop {
            Some(at) => Mapping::Desktop(at),
            None => Mapping::Output { width, height },
        };
        let (span_width, span_height) = mapping.span();
        if span_width < 2 || span_height < 2 {
            return Err(TouchError::Geometry {
                width: span_width,
                height: span_height,
            });
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(UINPUT_PATH)
            .map_err(TouchError::Open)?;
        let fd = file.as_raw_fd();

        ioctl_value(fd, UI_SET_EVBIT, i32::from(EV_SYN))?;
        ioctl_value(fd, UI_SET_EVBIT, i32::from(EV_KEY))?;
        ioctl_value(fd, UI_SET_EVBIT, i32::from(EV_ABS))?;
        ioctl_value(fd, UI_SET_KEYBIT, i32::from(BTN_TOUCH))?;
        for code in [
            ABS_MT_SLOT,
            ABS_MT_TRACKING_ID,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_PRESSURE,
        ] {
            ioctl_value(fd, UI_SET_ABSBIT, i32::from(code))?;
        }
        ioctl_value(fd, UI_SET_PROPBIT, i32::from(INPUT_PROP_DIRECT))?;

        abs_axis(
            fd,
            ABS_MT_SLOT,
            0,
            i32::try_from(MAX_POINTS - 1).unwrap_or(9),
        )?;
        abs_axis(
            fd,
            ABS_MT_TRACKING_ID,
            0,
            i32::try_from(TRACKING_MAX).unwrap_or(i32::MAX),
        )?;
        abs_axis(fd, ABS_MT_POSITION_X, 0, maximum(span_width))?;
        abs_axis(fd, ABS_MT_POSITION_Y, 0, maximum(span_height))?;
        abs_axis(fd, ABS_MT_PRESSURE, 0, PRESSURE_MAX)?;

        let mut setup = UinputSetup {
            id: InputId {
                bus_type: BUS_USB,
                vendor: 0x1209,
                product: 0x0001,
                version: 1,
            },
            name: [0; UINPUT_NAME_LEN],
            ff_effects_max: 0,
        };
        let name = DEVICE_NAME.as_bytes();
        setup.name[..name.len()].copy_from_slice(name);
        ioctl_ptr(fd, UI_DEV_SETUP, &mut setup)?;
        let mut phys = [0_u8; UINPUT_NAME_LEN];
        phys[..PHYS_PATH.len()].copy_from_slice(PHYS_PATH.as_bytes());
        ioctl_ptr(fd, UI_SET_PHYS, &mut phys)?;
        ioctl_no_arg(fd, UI_DEV_CREATE)?;

        Ok(Self {
            file,
            mapping,
            slots: Slots::default(),
        })
    }

    /// Whether the panel reports global desktop coordinates.
    pub(crate) const fn reports_global(&self) -> bool {
        self.mapping.reports_global()
    }

    /// Injects one touch report.
    ///
    /// # Errors
    ///
    /// Returns [`TouchError::Write`] when the kernel stops accepting events.
    pub(crate) fn apply(&mut self, frame: &TouchFrame) -> Result<(), TouchError> {
        let events = self.encode(frame);
        if events.is_empty() {
            return Ok(());
        }
        self.file.write_all(&events).map_err(TouchError::Write)
    }

    fn encode(&mut self, frame: &TouchFrame) -> Vec<u8> {
        let mut events = Vec::with_capacity(frame.points().len() * 5 * EVENT_LEN + EVENT_LEN);
        for point in frame.points() {
            match point.action {
                TouchAction::Down => {
                    if let Some((slot, _)) = self.slots.find(point.id) {
                        self.emit_up(&mut events, slot);
                    }
                    self.emit_press(&mut events, point.id, point.x, point.y, point.pressure);
                }
                TouchAction::Move => {
                    if self.slots.find(point.id).is_none() {
                        self.emit_press(&mut events, point.id, point.x, point.y, point.pressure);
                        continue;
                    }
                    let Some((slot, _)) = self.slots.find(point.id) else {
                        continue;
                    };
                    push_event(
                        &mut events,
                        EV_ABS,
                        ABS_MT_SLOT,
                        i32::try_from(slot).unwrap_or(0),
                    );
                    push_position(&mut events, self.mapping, point.x, point.y);
                    push_event(
                        &mut events,
                        EV_ABS,
                        ABS_MT_PRESSURE,
                        i32::from(point.pressure),
                    );
                }
                TouchAction::Up => {
                    if let Some((slot, _)) = self.slots.find(point.id) {
                        self.emit_up(&mut events, slot);
                    }
                }
                TouchAction::Cancel => {
                    for (slot, _) in self.slots.active_slots() {
                        self.emit_up(&mut events, slot);
                    }
                }
                // Future protocol actions are ignored rather than guessed at.
                _ => {}
            }
        }
        push_event(&mut events, EV_SYN, SYN_REPORT, 0);
        events
    }

    fn emit_press(&mut self, events: &mut Vec<u8>, phone_id: u8, x: u16, y: u16, pressure: u8) {
        let Some((slot, tracking_id)) = self.slots.allocate(phone_id) else {
            log::warn!("all touch slots are busy; dropping pointer {phone_id}");
            return;
        };
        push_event(
            events,
            EV_ABS,
            ABS_MT_SLOT,
            i32::try_from(slot).unwrap_or(0),
        );
        push_event(
            events,
            EV_ABS,
            ABS_MT_TRACKING_ID,
            i32::try_from(tracking_id).unwrap_or(0),
        );
        push_position(events, self.mapping, x, y);
        push_event(events, EV_ABS, ABS_MT_PRESSURE, i32::from(pressure));
        if self.slots.count() == 1 {
            push_event(events, EV_KEY, BTN_TOUCH, 1);
        }
    }

    fn emit_up(&mut self, events: &mut Vec<u8>, slot: usize) {
        push_event(
            events,
            EV_ABS,
            ABS_MT_SLOT,
            i32::try_from(slot).unwrap_or(0),
        );
        push_event(events, EV_ABS, ABS_MT_TRACKING_ID, -1);
        if self.slots.release(slot).is_some() && self.slots.count() == 0 {
            push_event(events, EV_KEY, BTN_TOUCH, 0);
        }
    }

    /// Removes the device from the system.
    pub(crate) fn destroy(&mut self) {
        let _ = ioctl_no_arg(self.file.as_raw_fd(), UI_DEV_DESTROY);
    }
}

impl Drop for TouchDevice {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn push_event(events: &mut Vec<u8>, kind: u16, code: u16, value: i32) {
    let mut event = [0_u8; EVENT_LEN];
    event[EVENT_KIND_OFFSET..EVENT_CODE_OFFSET].copy_from_slice(&kind.to_ne_bytes());
    event[EVENT_CODE_OFFSET..EVENT_VALUE_OFFSET].copy_from_slice(&code.to_ne_bytes());
    event[EVENT_VALUE_OFFSET..EVENT_LEN].copy_from_slice(&value.to_ne_bytes());
    events.extend_from_slice(&event);
}

fn push_position(events: &mut Vec<u8>, mapping: Mapping, x: u16, y: u16) {
    let (device_x, device_y) = mapping.point(x, y);
    push_event(events, EV_ABS, ABS_MT_POSITION_X, device_x);
    push_event(events, EV_ABS, ABS_MT_POSITION_Y, device_y);
}

/// The highest value an axis of `extent` steps can carry.
fn maximum(extent: u32) -> i32 {
    i32::try_from(extent.saturating_sub(1)).unwrap_or(i32::MAX)
}

/// Maps a global desktop point onto the axis that covers the desktop.
fn axis(global: i64, span: u32) -> i32 {
    let last = i64::from(span.saturating_sub(1));
    i32::try_from(global.clamp(0, last)).unwrap_or(i32::MAX)
}

/// Maps a normalised coordinate onto the pixel range of one axis.
///
/// The 16-bit phone coordinate is treated as a 16.16 fraction of the axis,
/// so the extremes land on the first and last pixel.
fn scale(value: u16, extent: u32) -> i32 {
    if extent < 2 {
        return 0;
    }
    let pixel = u64::from(value) * u64::from(extent) / 65_536;
    i32::try_from(pixel).unwrap_or(i32::MAX)
}

fn ioctl_no_arg(fd: RawFd, request: u32) -> Result<(), TouchError> {
    // SAFETY: `fd` is the open uinput descriptor and this request takes no
    // argument, so the kernel dereferences no user pointer.
    let result = unsafe { libc::ioctl(fd, c_ulong::from(request)) };
    if result < 0 {
        return Err(TouchError::Ioctl {
            operation: "device create/destroy",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn ioctl_value(fd: RawFd, request: u32, value: i32) -> Result<(), TouchError> {
    // SAFETY: `fd` is the open uinput descriptor; these legacy requests take
    // an integer in the variadic argument slot, never a user pointer.
    let result = unsafe { libc::ioctl(fd, c_ulong::from(request), value) };
    if result < 0 {
        return Err(TouchError::Ioctl {
            operation: "capability bit",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn ioctl_ptr<T>(fd: RawFd, request: u32, arg: &mut T) -> Result<(), TouchError> {
    // SAFETY: `fd` is the open uinput descriptor; `arg` points to a live,
    // properly aligned `#[repr(C)]` value that the kernel only reads.
    let result = unsafe {
        libc::ioctl(
            fd,
            c_ulong::from(request),
            std::ptr::from_mut(arg).cast::<libc::c_void>(),
        )
    };
    if result < 0 {
        return Err(TouchError::Ioctl {
            operation: "device or axis setup",
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn abs_axis(fd: RawFd, code: u16, minimum: i32, maximum: i32) -> Result<(), TouchError> {
    let mut setup = UinputAbsSetup {
        code,
        absinfo: InputAbsinfo {
            value: 0,
            minimum,
            maximum,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };
    ioctl_ptr(fd, UI_ABS_SETUP, &mut setup)
}

/// Failure while creating or driving the virtual touchscreen.
#[derive(Debug, Error)]
pub(crate) enum TouchError {
    /// `/dev/uinput` could not be opened.
    #[error("opening {UINPUT_PATH} failed: {0}")]
    Open(#[source] io::Error),
    /// A setup ioctl was rejected.
    #[error("uinput {operation} failed: {source}")]
    Ioctl {
        /// What was being configured.
        operation: &'static str,
        /// Kernel error.
        source: io::Error,
    },
    /// Injecting events failed.
    #[error("writing touch events failed: {0}")]
    Write(#[source] io::Error),
    /// The display has no usable pixel range.
    #[error("cannot map touch onto a {width}x{height} display")]
    Geometry {
        /// Rejected width.
        width: u32,
        /// Rejected height.
        height: u32,
    },
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use androscreen_core::touch::TouchPoint;

    use super::*;

    fn frame(points: Vec<TouchPoint>) -> TouchFrame {
        TouchFrame::new(points).expect("valid touch frame")
    }

    fn decode_events(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
        assert_eq!(bytes.len() % EVENT_LEN, 0, "whole events only");
        bytes
            .chunks_exact(EVENT_LEN)
            .map(|event| {
                let kind =
                    u16::from_ne_bytes([event[EVENT_KIND_OFFSET], event[EVENT_KIND_OFFSET + 1]]);
                let code =
                    u16::from_ne_bytes([event[EVENT_CODE_OFFSET], event[EVENT_CODE_OFFSET + 1]]);
                let value = i32::from_ne_bytes([
                    event[EVENT_VALUE_OFFSET],
                    event[EVENT_VALUE_OFFSET + 1],
                    event[EVENT_VALUE_OFFSET + 2],
                    event[EVENT_VALUE_OFFSET + 3],
                ]);
                (kind, code, value)
            })
            .collect()
    }

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<UinputAbsSetup>(), 28);
        assert_eq!(UI_DEV_CREATE, 0x5501);
        assert_eq!(UI_DEV_DESTROY, 0x5502);
        assert_eq!(UI_DEV_SETUP, 0x405c_5503);
        assert_eq!(UI_ABS_SETUP, 0x401c_5504);
        assert_eq!(UI_SET_EVBIT, 0x4004_5564);
        assert_eq!(UI_SET_KEYBIT, 0x4004_5565);
        assert_eq!(UI_SET_ABSBIT, 0x4004_5567);
        assert_eq!(UI_SET_PROPBIT, 0x4004_556e);
        assert_eq!(UI_SET_PHYS, 0x4008_556c);
    }

    #[test]
    fn normalised_coordinates_cover_the_display() {
        assert_eq!(scale(0, 2400), 0);
        assert_eq!(scale(65_535, 2400), 2399);
        assert_eq!(scale(32_768, 2400), 1200);
        assert_eq!(scale(65_535, 1080), 1079);
        assert_eq!(scale(100, 1), 0);
    }

    fn device(width: u32, height: u32) -> TouchDevice {
        TouchDevice {
            file: std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .expect("dev null opens"),
            mapping: Mapping::Output { width, height },
            slots: Slots::default(),
        }
    }

    fn desktop(width: u32, height: u32) -> Placement {
        Placement {
            left: 0,
            top: 0,
            span_width: 4289,
            span_height: 1143,
            output_left: 1829,
            output_top: 0,
            output_width: width,
            output_height: height,
        }
    }

    #[test]
    fn a_desktop_wide_panel_reaches_only_the_virtual_output() {
        let mapping = Mapping::Desktop(desktop(2460, 1080));
        assert_eq!(
            mapping.point(0, 65_535),
            (1829, 1079),
            "the phone's top-left lands on the output's top-left"
        );
        assert_eq!(
            mapping.point(65_535, 0),
            (4288, 0),
            "the far corner is the last step of the desktop"
        );
    }

    #[test]
    fn a_panel_west_of_the_desktop_origin_stays_inside_it() {
        let mapping = Mapping::Desktop(Placement {
            left: 500,
            top: 200,
            span_width: 2000,
            span_height: 1000,
            output_left: 500,
            output_top: 200,
            output_width: 1000,
            output_height: 500,
        });
        assert_eq!(mapping.point(0, 0), (0, 0));
        assert_eq!(mapping.point(65_535, 65_535), (999, 499));
    }

    #[test]
    fn down_move_up_produce_protocol_b_events() {
        let mut device = device(2400, 1080);
        let down = device.encode(&frame(vec![TouchPoint::new(
            0,
            TouchAction::Down,
            0,
            65_535,
            128,
        )]));
        let events = decode_events(&down);
        assert_eq!(events.first(), Some(&(EV_ABS, ABS_MT_SLOT, 0)));
        assert_eq!(
            events.get(1).map(|event| event.0),
            Some(EV_ABS),
            "tracking id follows the slot"
        );
        assert_eq!(
            events[1].1, ABS_MT_TRACKING_ID,
            "second event assigns tracking id"
        );
        assert_eq!(events[2], (EV_ABS, ABS_MT_POSITION_X, 0));
        assert_eq!(events[3], (EV_ABS, ABS_MT_POSITION_Y, 1079));
        assert_eq!(events[4], (EV_ABS, ABS_MT_PRESSURE, 128));
        assert_eq!(events[5], (EV_KEY, BTN_TOUCH, 1));
        assert_eq!(events.last(), Some(&(EV_SYN, SYN_REPORT, 0)));

        let moved = device.encode(&frame(vec![TouchPoint::new(
            0,
            TouchAction::Move,
            32_768,
            0,
            200,
        )]));
        let events = decode_events(&moved);
        assert_eq!(events[0], (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(events[1], (EV_ABS, ABS_MT_POSITION_X, 1200));
        assert_eq!(events[2], (EV_ABS, ABS_MT_POSITION_Y, 0));
        assert_eq!(events[3], (EV_ABS, ABS_MT_PRESSURE, 200));
        assert_eq!(events.last(), Some(&(EV_SYN, SYN_REPORT, 0)));

        let up = device.encode(&frame(vec![TouchPoint::new(
            0,
            TouchAction::Up,
            32_768,
            0,
            0,
        )]));
        let events = decode_events(&up);
        assert_eq!(events[0], (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(events[1], (EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert_eq!(events[2], (EV_KEY, BTN_TOUCH, 0));
        assert_eq!(events[3], (EV_SYN, SYN_REPORT, 0));
        assert_eq!(device.slots.count(), 0);
    }

    #[test]
    fn two_contacts_use_distinct_slots() {
        let mut device = device(1920, 1080);
        let report = device.encode(&frame(vec![
            TouchPoint::new(4, TouchAction::Down, 0, 0, 10),
            TouchPoint::new(7, TouchAction::Down, 65_535, 65_535, 20),
        ]));
        let events = decode_events(&report);
        let slots: Vec<i32> = events
            .iter()
            .filter(|event| event.0 == EV_ABS && event.1 == ABS_MT_SLOT)
            .map(|event| event.2)
            .collect();
        assert_eq!(slots, vec![0, 1]);
        let btn: Vec<i32> = events
            .iter()
            .filter(|event| event.0 == EV_KEY)
            .map(|event| event.2)
            .collect();
        assert_eq!(btn, vec![1], "BTN_TOUCH only toggles on the first contact");
        assert_eq!(device.slots.count(), 2);
    }

    #[test]
    fn cancel_lifts_every_contact() {
        let mut device = device(1920, 1080);
        device.encode(&frame(vec![
            TouchPoint::new(1, TouchAction::Down, 0, 0, 0),
            TouchPoint::new(2, TouchAction::Down, 10, 10, 0),
        ]));
        let cancel = device.encode(&frame(vec![TouchPoint::new(
            1,
            TouchAction::Cancel,
            0,
            0,
            0,
        )]));
        let events = decode_events(&cancel);
        let lifts = events
            .iter()
            .filter(|event| event.0 == EV_ABS && event.1 == ABS_MT_TRACKING_ID)
            .filter(|event| event.2 == -1)
            .count();
        assert_eq!(lifts, 2);
        assert_eq!(device.slots.count(), 0);
    }

    #[test]
    fn move_without_down_presses_first() {
        let mut device = device(1920, 1080);
        let events = decode_events(&device.encode(&frame(vec![TouchPoint::new(
            3,
            TouchAction::Move,
            0,
            0,
            0,
        )])));
        assert!(events
            .iter()
            .any(|event| event.0 == EV_ABS && event.1 == ABS_MT_TRACKING_ID && event.2 >= 0));
        assert_eq!(device.slots.count(), 1);
    }

    #[test]
    fn tracking_ids_wrap_inside_the_declared_range() {
        let mut slots = Slots {
            active: Default::default(),
            next_tracking: TRACKING_MAX - 1,
        };
        let (_, last) = slots.allocate(0).expect("slot free");
        assert_eq!(last, TRACKING_MAX);
        slots.release(0);
        let (_, wrapped) = slots.allocate(0).expect("slot free");
        assert_eq!(wrapped, 1);
    }

    #[test]
    fn device_can_be_created_when_uinput_is_writable() {
        if !Path::new(UINPUT_PATH).exists() {
            return;
        }
        let mut device = match TouchDevice::create(1920, 1080, None) {
            Ok(device) => device,
            Err(TouchError::Open(error)) if error.kind() == io::ErrorKind::PermissionDenied => {
                return;
            }
            Err(error) => panic!("uinput setup failed: {error}"),
        };
        device.destroy();
    }
}
