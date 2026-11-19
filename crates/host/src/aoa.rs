//! Android Open Accessory (AOA) transport over USB.
//!
//! The host probes every attached device with the AOA `GET_PROTOCOL` vendor
//! request, switches the first phone that answers to accessory mode, waits for
//! it to re-enumerate as a Google accessory and then exchanges
//! [`androscreen_core::protocol`] frames over the accessory bulk endpoints.
//!
//! No USB debugging or app-side authorization is involved: Android launches
//! the phone app itself when an accessory that matches its filter attaches.

use std::fmt;
use std::time::{Duration, Instant};

use androscreen_core::protocol::{
    self, Capability, Control, ControlError, Frame, Kind, ProtocolError, Session, AOA_DESCRIPTION,
    AOA_MANUFACTURER, AOA_MODEL, AOA_SERIAL, AOA_URI, AOA_VERSION, HEADER_LEN,
};
use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext};
use thiserror::Error;

/// Vendor id Android reports after switching to accessory mode.
pub(crate) const ACCESSORY_VENDOR_ID: u16 = 0x18D1;

/// Interface class of the accessory data interface.
const ACCESSORY_CLASS: u8 = 0xFF;
/// Interface subclass of the accessory data interface.
const ACCESSORY_SUBCLASS: u8 = 0xFF;
/// USB hub device class, skipped while probing.
const HUB_CLASS: u8 = 0x09;
/// HID device class; keyboards and mice never speak AOA, and probing them
/// costs a control transfer timeout on every scan.
const HID_CLASS: u8 = 0x03;
/// Wireless controller class, covering Bluetooth adapters. They never speak
/// AOA either, and probing them stalls the scan for seconds.
const WIRELESS_CLASS: u8 = 0xE0;
/// Still-image class: the interface Android exposes for MTP/PTP, which is what
/// a phone offers when the user picks "file transfer" on its screen.
const STILL_IMAGE_CLASS: u8 = 0x06;
/// Mass-storage class, still used by some phones for an SD card slot.
const MASS_STORAGE_CLASS: u8 = 0x08;
/// `_IO('U', 20)` from `linux/usbdevice_fs.h`: reset the port, which makes the
/// phone re-enumerate and offer its normal functions again.
const USBDEVFS_RESET: libc::c_ulong = 0x5514;

// AOA vendor control requests, see https://source.android.com/docs/core/interaction/accessories/protocol.
const REQ_GET_PROTOCOL: u8 = 51;
const REQ_SEND_STRING: u8 = 52;
const REQ_START: u8 = 53;
const CONTROL_IN: u8 = 0xC0;
const CONTROL_OUT: u8 = 0x40;

const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
/// A control write gets longer than a live frame: during a turn the phone is
/// rebuilding its decoder and stops draining the bulk endpoint for a second or
/// two. A one-second deadline turned that ordinary pause into a fatal link
/// error, which dropped the virtual display and blanked every monitor. Media
/// frames are still lossy; a control frame is not worth a torn-down connector.
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
/// How many unsolicited writes in a row may time out before the link is given
/// up on. A turn explains one pause; a phone that never drains its endpoint
/// for a minute is gone, and the virtual display it kept alive only made the
/// desktop paint into a monitor nobody watched while every write timed out.
const SILENT_WRITE_LIMIT: u8 = 20;
/// How many frame intervals one media write attempt may spend on the wire
/// before it is abandoned. Three intervals leave room for ordinary USB jitter
/// while still bounding a frame by something the loop cares about.
const MEDIA_WRITE_INTERVALS: u32 = 3;
/// How many frame intervals a whole media frame may spend before it is
/// dropped. This is the deadline that really bounds the streaming loop: the
/// per-attempt timeout was handed to libusb again for every partial write, so
/// a frame that kept accepting a few bytes could take forever and hold touch
/// input behind it.
const MEDIA_FRAME_INTERVALS: u32 = 6;
/// Floor for both deadlines: libusb resolves timeouts in milliseconds, and a
/// deadline that rounds down to zero means "wait forever".
const MEDIA_DEADLINE_FLOOR: Duration = Duration::from_millis(10);
/// The interval the starting deadlines are computed for, until the phone says
/// what rate it asked for.
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_millis(17);
/// How long the phone may leave *every* media frame timing out before the link
/// is taken for gone.
///
/// A phone that stops reading for a moment is normal — a turn rebuilds its
/// surface — and losing frames is the right answer there. A phone that has not
/// read a byte for this long is not coming back: the app was killed and the
/// accessory endpoint stayed behind, so every write times out forever. Without
/// this the session retried audio chunks for minutes, and the port could not be
/// handed back because nothing ever decided the link was dead.
const MEDIA_STALL_LIMIT: Duration = Duration::from_secs(6);
/// A write slower than this says the phone is behind, so the next desktop
/// frame is skipped rather than queued behind everything already on the wire.
const BEHIND_AFTER: Duration = Duration::from_millis(120);
/// How long the phone is given to drain before another frame is tried again.
/// A deadline rather than a "cleared by the next fast write" flag, because a
/// frozen desktop produces no writes at all and must not stay frozen forever.
const BEHIND_GRACE: Duration = Duration::from_millis(200);
const REENUMERATE_TIMEOUT: Duration = Duration::from_secs(8);
const REENUMERATE_POLL: Duration = Duration::from_millis(150);
const SCRATCH_LEN: usize = 64 * 1024;
/// Buffered bytes are dropped eagerly once this much of the buffer is consumed.
const COMPACT_THRESHOLD: usize = 64 * 1024;

/// Accessory descriptor strings, keyed by AOA string index.
const AOA_STRINGS: [(u16, &str); 6] = [
    (0, AOA_MANUFACTURER),
    (1, AOA_MODEL),
    (2, AOA_DESCRIPTION),
    (3, AOA_VERSION),
    (4, AOA_URI),
    (5, AOA_SERIAL),
];

/// Whether `vendor_id:product_id` is an Android device in accessory mode.
#[must_use]
pub(crate) const fn is_accessory(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == ACCESSORY_VENDOR_ID && matches!(product_id, 0x2D00..=0x2D05)
}

/// One USB device seen on the bus, for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanEntry {
    /// Bus number.
    pub bus: u8,
    /// Device address on the bus.
    pub address: u8,
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// USB device class.
    pub class: u8,
    /// Whether the device is already in Android accessory mode.
    pub accessory: bool,
    /// Whether this process may open the device node.
    pub readable: bool,
}

impl ScanEntry {
    /// `vvvv:pppp`, the form used by udev rules and `lsusb`.
    #[must_use]
    pub(crate) fn id(&self) -> String {
        format!("{:04x}:{:04x}", self.vendor_id, self.product_id)
    }
}

/// Identity and endpoints of an open accessory link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkInfo {
    /// Bus number.
    pub bus: u8,
    /// Device address.
    pub address: u8,
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Bulk IN endpoint address.
    pub endpoint_in: u8,
    /// Bulk OUT endpoint address.
    pub endpoint_out: u8,
}

/// Lists every attached USB device with its AOA status.
///
/// Reading descriptors and testing readability does not send vendor requests,
/// so it is safe to call while another phone session is running.
///
/// # Errors
///
/// Returns [`AoaError::Usb`] when libusb cannot enumerate the bus.
pub(crate) fn scan() -> Result<Vec<ScanEntry>, AoaError> {
    let context = Context::new()?;
    let mut entries: Vec<ScanEntry> = context
        .devices()?
        .iter()
        .filter_map(|device| {
            let descriptor = device.device_descriptor().ok()?;
            Some(ScanEntry {
                bus: device.bus_number(),
                address: device.address(),
                vendor_id: descriptor.vendor_id(),
                product_id: descriptor.product_id(),
                class: descriptor.class_code(),
                accessory: is_accessory(descriptor.vendor_id(), descriptor.product_id()),
                readable: device.open().is_ok(),
            })
        })
        .collect();
    entries.sort_by_key(|entry| (entry.bus, entry.address));
    Ok(entries)
}

/// Opens the phone app link, switching the phone to accessory mode if needed.
///
/// Already-connected accessories are reused, so re-running the daemon after a
/// restart does not disturb the phone.
///
/// # Errors
///
/// Returns [`AoaError::NoDevice`] when no phone is attached,
/// [`AoaError::PermissionDenied`] when a phone is present but its device node is
/// not writable, and [`AoaError::ReenumerationTimeout`] when the phone did not
/// come back as an accessory.
pub(crate) fn connect(policy: Takeover) -> Result<AoaLink, AoaError> {
    let context = Context::new()?;

    if let Some(device) = find_accessory(&context)? {
        log::info!(
            "accessory already attached on bus {} address {}",
            device.bus_number(),
            device.address()
        );
        return AoaLink::open(&device);
    }

    if policy == Takeover::OnlyAttached {
        return Err(AoaError::TakeoverDisabled);
    }

    take_over(&context, policy)
}

/// Whether a phone that is not yet an accessory may be switched over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Takeover {
    /// Switch a phone over when it is plugged in, unless it is sharing storage.
    Automatic,
    /// Switch it over even if it is sharing storage: the user asked for it.
    Forced,
    /// Serve only a phone that is already in accessory mode.
    OnlyAttached,
}

/// Switches the first phone that will have us and opens the accessory link.
fn take_over(context: &Context, policy: Takeover) -> Result<AoaLink, AoaError> {
    let mut denied: Vec<String> = Vec::new();
    let mut sharing: Vec<String> = Vec::new();
    let mut handshake_failure: Option<AoaError> = None;

    for device in context.devices()?.iter() {
        let Ok(descriptor) = device.device_descriptor() else {
            continue;
        };
        if matches!(
            descriptor.class_code(),
            HUB_CLASS | HID_CLASS | WIRELESS_CLASS
        ) {
            continue;
        }
        if has_only_hid_interfaces(&device) {
            continue;
        }
        if is_accessory(descriptor.vendor_id(), descriptor.product_id()) {
            continue;
        }

        let vendor_id = descriptor.vendor_id();
        let product_id = descriptor.product_id();
        let handle = match device.open() {
            Ok(handle) => handle,
            Err(rusb::Error::Access) => {
                denied.push(format!("{vendor_id:04x}:{product_id:04x}"));
                continue;
            }
            Err(error) => {
                log::debug!("cannot open {vendor_id:04x}:{product_id:04x}: {error}");
                continue;
            }
        };

        let protocol_version = match read_protocol(&handle) {
            Ok(version) => version,
            Err(error) => {
                log::debug!("{vendor_id:04x}:{product_id:04x} is not an AOA device: {error}");
                continue;
            }
        };
        drop(handle);

        if protocol_version == 0 {
            log::debug!("{vendor_id:04x}:{product_id:04x} answered AOA version 0");
            continue;
        }

        // The settings window saying "use this phone" outranks the phone's own
        // storage: the file manager can wait until the port is handed back.
        if policy != Takeover::Forced {
            if let Some(function) = storage_function(&device) {
                log::debug!(
                    "{vendor_id:04x}:{product_id:04x} is offering {function}; \
                     the file manager keeps it"
                );
                sharing.push(format!("{vendor_id:04x}:{product_id:04x} ({function})"));
                continue;
            }
        }

        let bus = device.bus_number();
        log::info!(
            "switching {vendor_id:04x}:{product_id:04x} on bus {bus} to accessory mode (AOA {protocol_version})"
        );
        match switch_to_accessory(&device) {
            Ok(()) => {}
            Err(error) => {
                log::warn!("{vendor_id:04x}:{product_id:04x} refused accessory mode: {error}");
                handshake_failure = Some(error);
                continue;
            }
        }
        drop(device);

        match wait_for_accessory(context, Some(bus)) {
            Ok(accessory) => {
                log::info!(
                    "accessory ready on bus {} address {}",
                    accessory.bus_number(),
                    accessory.address()
                );
                return AoaLink::open(&accessory);
            }
            Err(error) => {
                handshake_failure = Some(error);
            }
        }
    }

    if let Some(error) = handshake_failure {
        return Err(error);
    }
    if !sharing.is_empty() {
        return Err(AoaError::SharingStorage {
            devices: sharing.join(", "),
        });
    }
    if !denied.is_empty() {
        return Err(AoaError::PermissionDenied {
            devices: denied.join(", "),
        });
    }
    Err(AoaError::NoDevice)
}

fn find_accessory(context: &Context) -> Result<Option<Device<Context>>, AoaError> {
    for device in context.devices()?.iter() {
        let Ok(descriptor) = device.device_descriptor() else {
            continue;
        };
        if is_accessory(descriptor.vendor_id(), descriptor.product_id()) {
            return Ok(Some(device));
        }
    }
    Ok(None)
}

/// Whether every interface of the device is HID.
///
/// Composite keyboards report device class 0, so the class check alone misses
/// them; probing those stalls the scan for the control timeout.
fn has_only_hid_interfaces(device: &Device<Context>) -> bool {
    let Ok(config) = device
        .active_config_descriptor()
        .or_else(|_| device.config_descriptor(0))
    else {
        return false;
    };
    let mut seen = false;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            seen = true;
            if descriptor.class_code() != HID_CLASS {
                return false;
            }
        }
    }
    seen
}

/// Which storage function the phone is offering, if any.
///
/// Accessory mode replaces every other USB function, so taking over a phone
/// that is being used as a memory stick would end the file transfer the user
/// just asked for. The desktop's own file manager is the better answer there,
/// and the phone stays untouched until it stops advertising storage.
fn storage_function(device: &Device<Context>) -> Option<&'static str> {
    let config = device
        .active_config_descriptor()
        .or_else(|_| device.config_descriptor(0))
        .ok()?;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            match descriptor.class_code() {
                STILL_IMAGE_CLASS => return Some("mtp"),
                MASS_STORAGE_CLASS => return Some("mass storage"),
                _ => {}
            }
        }
    }
    None
}

/// Asks the phone to re-enumerate, so it offers its normal functions again.
///
/// The kernel resets the port; the phone cannot be talked out of accessory
/// mode any other way. Whether it comes back as storage or as charging only is
/// the phone's choice, which is why the caller only logs the outcome.
pub(crate) fn reset_port(bus: u8, address: u8) -> Result<(), AoaError> {
    use std::os::fd::AsRawFd;

    let path = format!("/dev/bus/usb/{bus:03}/{address:03}");
    let file = match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => file,
        // The node disappears the moment the phone leaves accessory mode, which
        // is exactly what this call is for: nothing left to reset.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(AoaError::PortReset {
                path,
                error: error.to_string(),
            });
        }
    };
    // SAFETY: `USBDEVFS_RESET` takes no argument beyond the request number, and
    // the descriptor stays open across the call.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_RESET) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        // The same race as above: the phone may have re-enumerated between the
        // open and the reset, leaving no device to reset.
        if error.raw_os_error() == Some(libc::ENODEV) {
            return Ok(());
        }
        return Err(AoaError::PortReset {
            path,
            error: error.to_string(),
        });
    }
    Ok(())
}

fn read_protocol(handle: &DeviceHandle<Context>) -> Result<u16, rusb::Error> {
    let mut buffer = [0_u8; 2];
    let read = handle.read_control(
        CONTROL_IN,
        REQ_GET_PROTOCOL,
        0,
        0,
        &mut buffer,
        CONTROL_TIMEOUT,
    )?;
    if read != buffer.len() {
        return Err(rusb::Error::Pipe);
    }
    Ok(u16::from_le_bytes(buffer))
}

fn switch_to_accessory(device: &Device<Context>) -> Result<(), AoaError> {
    let handle = device.open()?;
    // AOA carries the string id in wIndex, not wValue; the gadget stores the
    // payload under `strings[wIndex]`.
    for (index, text) in AOA_STRINGS {
        let bytes = null_terminated(text);
        handle.write_control(
            CONTROL_OUT,
            REQ_SEND_STRING,
            0,
            index,
            &bytes,
            CONTROL_TIMEOUT,
        )?;
    }
    match handle.write_control(CONTROL_OUT, REQ_START, 0, 0, &[], CONTROL_TIMEOUT) {
        Ok(_) => Ok(()),
        // START resets the link on many phones before libusb sees completion;
        // treat that as success and rely on the re-enumeration wait.
        Err(error @ (rusb::Error::NoDevice | rusb::Error::Pipe | rusb::Error::Io)) => {
            log::debug!("AOA START reported {error}, waiting for the phone to re-enumerate");
            Ok(())
        }
        Err(error) => Err(AoaError::Usb(error)),
    }
}

fn null_terminated(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
    bytes
}

fn wait_for_accessory(
    context: &Context,
    preferred_bus: Option<u8>,
) -> Result<Device<Context>, AoaError> {
    let deadline = Instant::now() + REENUMERATE_TIMEOUT;
    loop {
        let mut fallback = None;
        for device in context.devices()?.iter() {
            let Ok(descriptor) = device.device_descriptor() else {
                continue;
            };
            if !is_accessory(descriptor.vendor_id(), descriptor.product_id()) {
                continue;
            }
            if preferred_bus == Some(device.bus_number()) {
                return Ok(device);
            }
            fallback.get_or_insert(device);
        }
        if let Some(device) = fallback {
            return Ok(device);
        }
        if Instant::now() >= deadline {
            return Err(AoaError::ReenumerationTimeout { bus: preferred_bus });
        }
        std::thread::sleep(REENUMERATE_POLL);
    }
}

/// An open accessory link that exchanges protocol frames.
pub(crate) struct AoaLink {
    handle: DeviceHandle<Context>,
    info: LinkInfo,
    rx: RxBuffer,
    tx: Vec<u8>,
    scratch: Vec<u8>,
    flow: Flow,
    /// The last write timed out before any byte left the pipe.
    write_was_clean: bool,
    /// How many unsolicited writes in a row timed out before writing a byte.
    silent_writes: u8,
    /// How long one media write attempt may take, from the frame interval.
    media_write_timeout: Duration,
    /// How long a whole media frame may take before it is abandoned.
    media_frame_budget: Duration,
}

impl AoaLink {
    fn open(device: &Device<Context>) -> Result<Self, AoaError> {
        let descriptor = device.device_descriptor()?;
        let vendor_id = descriptor.vendor_id();
        let product_id = descriptor.product_id();
        let info = LinkInfo {
            bus: device.bus_number(),
            address: device.address(),
            vendor_id,
            product_id,
            endpoint_in: 0,
            endpoint_out: 0,
        };

        let handle = device.open().map_err(|error| match error {
            rusb::Error::Access => AoaError::PermissionDenied {
                devices: format!("{vendor_id:04x}:{product_id:04x}"),
            },
            other => AoaError::Usb(other),
        })?;

        let config = active_config(device, &handle)?;
        let (interface, endpoint_in, endpoint_out) =
            find_bulk_pair(&config).ok_or(AoaError::MissingEndpoints)?;
        handle
            .claim_interface(interface)
            .map_err(|error| match error {
                rusb::Error::Busy => AoaError::Busy,
                other => AoaError::Usb(other),
            })?;

        let (media_write_timeout, media_frame_budget) = media_deadlines(DEFAULT_FRAME_INTERVAL);
        Ok(Self {
            handle,
            info: LinkInfo {
                endpoint_in,
                endpoint_out,
                ..info
            },
            rx: RxBuffer::default(),
            tx: Vec::with_capacity(SCRATCH_LEN),
            scratch: vec![0_u8; SCRATCH_LEN],
            flow: Flow::default(),
            write_was_clean: false,
            silent_writes: 0,
            media_write_timeout,
            media_frame_budget,
        })
    }

    /// Identity and endpoints of this link.
    #[must_use]
    pub(crate) const fn info(&self) -> LinkInfo {
        self.info
    }

    /// Returns a frame that was already buffered, without touching USB.
    ///
    /// Use this to drain frames in a burst before blocking in [`AoaLink::poll`].
    ///
    /// # Errors
    ///
    /// Returns [`AoaError::Protocol`] when the buffered bytes are corrupted.
    pub(crate) fn buffered(&mut self) -> Result<Option<Frame>, AoaError> {
        Ok(self.rx.next_frame()?)
    }

    /// Waits up to `timeout` for the next frame from the phone.
    ///
    /// Returns `Ok(None)` when the phone sent nothing in time. A timeout is
    /// normal: the daemon uses it to interleave outgoing frames.
    ///
    /// # Errors
    ///
    /// Returns [`AoaError::Disconnected`] when the phone was unplugged and
    /// [`AoaError::Protocol`] when the stream is corrupted.
    pub(crate) fn poll(&mut self, timeout: Duration) -> Result<Option<Frame>, AoaError> {
        if let Some(frame) = self.rx.next_frame()? {
            return Ok(Some(frame));
        }

        // libusb treats a zero timeout as an infinite block.
        let timeout = timeout.max(Duration::from_millis(1));
        let read = self
            .handle
            .read_bulk(self.info.endpoint_in, &mut self.scratch, timeout);
        match read {
            Ok(read) => {
                self.rx.extend(&self.scratch[..read]);
                Ok(self.rx.next_frame()?)
            }
            Err(rusb::Error::Timeout | rusb::Error::Interrupted) => Ok(None),
            Err(error) => Err(disconnected_or(error)),
        }
    }

    /// Sends one frame that has to arrive.
    ///
    /// # Errors
    ///
    /// Returns [`AoaError::Disconnected`] when the write cannot complete, since
    /// a partially written frame would desynchronize the stream.
    pub(crate) fn send(&mut self, frame: &Frame) -> Result<(), AoaError> {
        self.write(frame, WRITE_TIMEOUT, None)
    }

    /// Sends a frame the phone can do without, such as a dashboard sample or a
    /// pong. A turn makes the app stop reading USB for a moment; a frame that
    /// timed out before any byte was written is not a broken link, and ending
    /// the session over it used to tear down the whole virtual display and
    /// force a reconnect that took far longer than the turn itself.
    ///
    /// # Errors
    ///
    /// Returns [`AoaError`] only when bytes were already written (the frame is
    /// then out of sync) or the device really went away.
    pub(crate) fn send_lossy(&mut self, frame: &Frame) -> Result<(), AoaError> {
        let outcome = self.write(frame, WRITE_TIMEOUT, None);
        match outcome {
            Ok(()) => {
                self.silent_writes = 0;
                Ok(())
            }
            Err(_) if self.write_was_clean => {
                self.silent_writes = self.silent_writes.saturating_add(1);
                if self.silent_writes >= SILENT_WRITE_LIMIT {
                    log::warn!(
                        "the phone has not drained its USB endpoint for {} writes; \
                         ending the session instead of keeping a display nobody sees",
                        self.silent_writes
                    );
                    return Err(AoaError::Disconnected);
                }
                log::debug!(
                    "a {:?} frame never reached the phone; keeping the display",
                    frame.kind(),
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// The lossy counterpart of [`AoaLink::send_control`].
    ///
    /// # Errors
    ///
    /// Forwards [`AoaLink::send_lossy`] failures.
    pub(crate) fn send_control_lossy(&mut self, control: &Control) -> Result<(), AoaError> {
        self.send_lossy(&Frame::control(control)?)
    }

    /// Sends one frame the phone can do without.
    ///
    /// A congested pipe is not a broken one, so a stalled write ends nothing:
    /// the frame is lost with the bytes it left behind and the link keeps
    /// working. Video then holds its breath until a keyframe, because a picture
    /// missing its middle is not decodable, and audio is skipped outright while
    /// the phone catches up.
    ///
    /// A stall that outlasts [`MEDIA_STALL_LIMIT`] is a different thing: the
    /// phone stopped reading for good, which is what a killed app behind a live
    /// accessory endpoint looks like. That one ends the link, so the port can be
    /// handed back and the next phone gets a screen.
    ///
    /// # Errors
    ///
    /// Like [`AoaLink::send`], but only for a phone that really went away.
    pub(crate) fn send_media(&mut self, frame: &Frame) -> Result<(), AoaError> {
        let now = Instant::now();
        if !self.flow.takes(frame, now) {
            self.flow.note_dropped(frame);
            return Ok(());
        }
        let timeout = self.media_write_timeout;
        let budget = self.media_frame_budget;
        match self.write(frame, timeout, Some(budget)) {
            Ok(()) => {
                self.flow.note_written(frame, now.elapsed(), now);
                Ok(())
            }
            Err(AoaError::Usb(rusb::Error::Timeout)) => {
                self.flow.note_lost(frame, now.elapsed(), now);
                if self.write_was_clean {
                    self.flow.note_silence(now);
                }
                if self.flow.stalled_for(now) >= MEDIA_STALL_LIMIT {
                    log::warn!(
                        "the phone has taken no frame for {:.0}s; giving the port back",
                        self.flow.stalled_for(now).as_secs_f64()
                    );
                    return Err(AoaError::Disconnected);
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Whether the phone is still draining what it was already sent.
    ///
    /// Desktop frames are not grabbed while this holds: they would only queue
    /// behind everything on the wire, and a held picture costs the decoder
    /// nothing while a queued one costs the link everything it was carrying.
    pub(crate) fn is_behind(&self) -> bool {
        self.flow.behind(Instant::now())
    }

    /// Whether video is waiting for a keyframe after a lost picture.
    ///
    /// Feeding the encoder through this is what makes it arrive: keyframes come
    /// every so many *frames*, so a still desktop has to be pushed through the
    /// encoder even though there is nothing new in it.
    pub(crate) fn is_awaiting_keyframe(&self) -> bool {
        self.flow.keyframe_pending
    }

    /// Recomputes the media write deadlines for the rate the stream runs at.
    ///
    /// The old deadlines were fixed, so a frame accepting a few bytes per
    /// attempt could hold the streaming loop — and the touch reports behind it
    /// — for as long as it liked. Deriving them from the frame interval bounds
    /// that while still leaving ordinary USB jitter room to clear.
    pub(crate) fn set_media_pacing(&mut self, frame_interval: Duration) {
        let (attempt, budget) = media_deadlines(frame_interval);
        self.media_write_timeout = attempt;
        self.media_frame_budget = budget;
    }

    /// A snapshot of what this link has streamed so far.
    ///
    /// Counters are cumulative; the caller turns deltas into rates.
    #[must_use]
    pub(crate) fn stream_counters(&self, now: Instant) -> StreamCounters {
        let waiting = self
            .flow
            .keyframe_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        StreamCounters {
            videos: self.flow.videos,
            video_bytes: self.flow.video_bytes,
            audios: self.flow.audios,
            dropped: u64::from(self.flow.dropped),
            lost: self.flow.lost,
            behind_marks: self.flow.behind_marks,
            keyframe_waited: self.flow.keyframe_waited.saturating_add(waiting),
            write_time: self.flow.write_time,
            write_max: self.flow.write_max,
        }
    }

    /// Tells the link the phone has no decoder for what is coming next.
    ///
    /// A rebuilt decoder refuses everything but an IDR, and `grab_desktop`
    /// stops feeding a link the phone has not drained — which is exactly what a
    /// phone rebuilding its surface looks like. Saying it out loud is what keeps
    /// the desktop flowing through the encoder until the IDR it needs exists;
    /// without it a still desktop goes quiet and the phone waits for a picture
    /// nobody is producing.
    pub(crate) fn expect_keyframe(&mut self) {
        self.flow.keyframe_pending = true;
        self.flow.keyframe_since.get_or_insert(Instant::now());
    }

    /// Writes one encoded frame out in full, giving up after `timeout` per
    /// attempt and, when a `budget` is given, after that much time for the
    /// whole frame. A failed attempt can leave part of the frame at the phone,
    /// so callers that cannot survive that must treat the error as fatal.
    fn write(
        &mut self,
        frame: &Frame,
        timeout: Duration,
        budget: Option<Duration>,
    ) -> Result<(), AoaError> {
        self.write_was_clean = false;
        self.tx.clear();
        protocol::write_frame(&mut self.tx, frame)?;
        let started = Instant::now();
        let mut written = 0;
        while written < self.tx.len() {
            let attempt = match budget {
                None => timeout,
                Some(budget) => {
                    let left = budget.saturating_sub(started.elapsed());
                    if left < Duration::from_millis(1) {
                        log::debug!(
                            "a {:?} frame of {} bytes gave up {} bytes short after {:.0}ms",
                            frame.kind(),
                            self.tx.len(),
                            self.tx.len() - written,
                            started.elapsed().as_secs_f64() * 1000.0
                        );
                        self.write_was_clean = written == 0;
                        return Err(AoaError::Usb(rusb::Error::Timeout));
                    }
                    left.min(timeout)
                }
            };
            match self
                .handle
                .write_bulk(self.info.endpoint_out, &self.tx[written..], attempt)
            {
                Ok(0) => return Err(AoaError::Disconnected),
                Ok(count) => written += count,
                Err(rusb::Error::Interrupted) => {}
                Err(error) => {
                    log::debug!(
                        "sending a {:?} frame of {} bytes failed after {} bytes: {error}",
                        frame.kind(),
                        self.tx.len(),
                        written
                    );
                    // Nothing left the pipe, so the phone cannot have consumed
                    // half a frame: losing this one keeps the stream in sync.
                    self.write_was_clean = written == 0 && matches!(error, rusb::Error::Timeout);
                    return Err(disconnected_or(error));
                }
            }
        }
        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(200) {
            log::debug!(
                "a {:?} frame of {} bytes took {:.1}s to leave",
                frame.kind(),
                self.tx.len(),
                elapsed.as_secs_f64()
            );
        }
        Ok(())
    }

    /// Sends a control message as a [`protocol::Kind::Control`] frame.
    ///
    /// # Errors
    ///
    /// Returns [`AoaError::Control`] when the message cannot be serialized and
    /// forwards [`AoaLink::send`] failures.
    pub(crate) fn send_control(&mut self, control: &Control) -> Result<(), AoaError> {
        self.send(&Frame::control(control)?)
    }
}

impl fmt::Debug for AoaLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AoaLink")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// The greeting the phone app sends once it owns the accessory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hello {
    /// Wire protocol version implemented by the phone.
    pub(crate) protocol: u8,
    /// App version string.
    pub(crate) app_version: String,
    /// Features the phone advertises.
    pub(crate) capabilities: Vec<Capability>,
}

/// Waits for the phone app greeting, answering pings meanwhile.
///
/// # Errors
///
/// Returns [`AoaError::HelloTimeout`] when the app does not greet in time, and
/// forwards link or protocol failures.
pub(crate) fn wait_for_hello(
    link: &mut AoaLink,
    timeout: Duration,
    mut abandoned: impl FnMut() -> bool,
) -> Result<Option<Hello>, AoaError> {
    let deadline = Instant::now() + timeout;
    loop {
        // A wait that nobody wants any more has no business running out its
        // full timeout: the user may be asking for the port back right now.
        if abandoned() {
            return Ok(None);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AoaError::HelloTimeout { timeout });
        }
        let Some(frame) = link.poll(remaining.min(Duration::from_millis(200)))? else {
            continue;
        };
        match frame.kind() {
            Kind::Ping => link.send(&Frame::pong(Session::AwaitingHello))?,
            Kind::Control => {
                if let Control::Hello {
                    protocol,
                    app_version,
                    capabilities,
                } = frame.as_control()?
                {
                    return Ok(Some(Hello {
                        protocol,
                        app_version,
                        capabilities,
                    }));
                }
            }
            _ => {}
        }
    }
}

fn active_config(
    device: &Device<Context>,
    handle: &DeviceHandle<Context>,
) -> Result<rusb::ConfigDescriptor, AoaError> {
    if let Ok(config) = device.active_config_descriptor() {
        return Ok(config);
    }
    let first = device.config_descriptor(0)?;
    handle.set_active_configuration(first.number())?;
    Ok(device.active_config_descriptor()?)
}

/// Finds the accessory interface and its bulk endpoints.
///
/// Prefers the vendor-specific interface (class `0xFF`, subclass `0xFF`) and
/// falls back to any interface that has both bulk directions.
fn find_bulk_pair(config: &rusb::ConfigDescriptor) -> Option<(u8, u8, u8)> {
    let mut fallback = None;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            let mut endpoint_in = None;
            let mut endpoint_out = None;
            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match endpoint.direction() {
                    Direction::In => endpoint_in = Some(endpoint.address()),
                    Direction::Out => endpoint_out = Some(endpoint.address()),
                }
            }
            let (Some(endpoint_in), Some(endpoint_out)) = (endpoint_in, endpoint_out) else {
                continue;
            };
            let candidate = (descriptor.interface_number(), endpoint_in, endpoint_out);
            if descriptor.class_code() == ACCESSORY_CLASS
                && descriptor.sub_class_code() == ACCESSORY_SUBCLASS
            {
                return Some(candidate);
            }
            fallback.get_or_insert(candidate);
        }
    }
    fallback
}

fn disconnected_or(error: rusb::Error) -> AoaError {
    match error {
        rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe => AoaError::Disconnected,
        other => AoaError::Usb(other),
    }
}

/// How far the phone is behind, and what that costs it.
///
/// The link never ends because of this: losing a frame is cheap, losing the
/// session makes the phone blink back to its own screen.
#[derive(Debug, Default)]
struct Flow {
    /// Live frames are held back until this moment has passed.
    behind_until: Option<Instant>,
    /// A picture was lost in the middle, so video waits for a whole one.
    keyframe_pending: bool,
    /// When the pipe last accepted nothing at all: every media write since has
    /// timed out before its first byte, which is what a killed app looks like
    /// from here. Cleared the moment any frame leaves whole.
    stalled_since: Option<Instant>,
    /// Frames abandoned since the link last kept up, for the log.
    dropped: u32,
    /// Video frames that left whole, and the bytes in them.
    videos: u64,
    video_bytes: u64,
    /// Audio chunks that left whole.
    audios: u64,
    /// Media writes that timed out, whether cleanly or with bytes left behind.
    lost: u64,
    /// How often the phone was noticed falling behind.
    behind_marks: u64,
    /// Time already spent waiting for a keyframe.
    keyframe_waited: Duration,
    /// When the keyframe wait in progress began, if one is running.
    keyframe_since: Option<Instant>,
    /// Time the pipe spent accepting media, and the worst single frame.
    write_time: Duration,
    write_max: Duration,
}

impl Flow {
    /// Whether this media frame is worth writing right now.
    fn takes(&self, frame: &Frame, now: Instant) -> bool {
        if frame.kind() != Kind::Video {
            // Sound is continuous: half a chunk is worse than none of it, and
            // the phone would only run out of samples mid-word.
            return !self.behind(now) && !self.keyframe_pending;
        }
        // A picture is only worth sending whole, or from a keyframe on.
        !self.keyframe_pending || frame.is_keyframe()
    }

    /// Records a frame that left the host, and how long the pipe made it wait.
    fn note_written(&mut self, frame: &Frame, elapsed: Duration, now: Instant) {
        match frame.kind() {
            Kind::Video => {
                self.videos = self.videos.saturating_add(1);
                self.video_bytes = self
                    .video_bytes
                    .saturating_add(frame.payload().len() as u64);
            }
            Kind::Audio => {
                self.audios = self.audios.saturating_add(1);
            }
            _ => {}
        }
        self.note_write_time(elapsed);
        if frame.is_keyframe() {
            self.keyframe_pending = false;
            if let Some(since) = self.keyframe_since.take() {
                self.keyframe_waited = self
                    .keyframe_waited
                    .saturating_add(now.saturating_duration_since(since));
            }
        }
        // The pipe moved, so whatever silence was building is over.
        self.stalled_since = None;
        self.rate(elapsed, frame.kind(), now);
    }

    /// Records a frame the phone never took, and how long it wasted on it.
    fn note_lost(&mut self, frame: &Frame, elapsed: Duration, now: Instant) {
        self.lost = self.lost.saturating_add(1);
        self.note_write_time(elapsed);
        if frame.kind() == Kind::Video {
            log::debug!("a video frame was lost whole; waiting for the next keyframe");
            self.keyframe_pending = true;
            self.keyframe_since.get_or_insert(now);
        }
        self.behind_until = Some(now + BEHIND_GRACE);
    }

    /// Adds one media write to the running time, keeping the worst one.
    fn note_write_time(&mut self, elapsed: Duration) {
        self.write_time = self.write_time.saturating_add(elapsed);
        self.write_max = self.write_max.max(elapsed);
    }

    /// Records a write that timed out before putting a single byte on the wire.
    ///
    /// Only those count towards a stall: a frame that was half written costs the
    /// phone the stream's sync, which the caller ends the link over anyway.
    fn note_silence(&mut self, now: Instant) {
        if self.stalled_since.is_none() {
            self.stalled_since = Some(now);
        }
    }

    /// How long the pipe has been taking nothing at all.
    fn stalled_for(&self, now: Instant) -> Duration {
        self.stalled_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    /// Records a frame that was not written at all.
    fn note_dropped(&mut self, frame: &Frame) {
        self.dropped = self.dropped.saturating_add(1);
        if self.dropped % 120 == 1 {
            log::debug!(
                "the phone cannot keep up; {} {:?} frames dropped so far",
                self.dropped,
                frame.kind()
            );
        }
    }

    /// Whether the phone has not caught up yet.
    fn behind(&self, now: Instant) -> bool {
        self.behind_until.is_some_and(|until| now < until)
    }

    /// Decides whether the next frame has to wait, from how long this one took.
    fn rate(&mut self, elapsed: Duration, kind: Kind, now: Instant) {
        if elapsed < BEHIND_AFTER {
            self.behind_until = None;
            return;
        }
        if !self.behind(now) {
            self.behind_marks = self.behind_marks.saturating_add(1);
            log::debug!(
                "the phone is behind: a {kind:?} frame took {:.0}ms",
                elapsed.as_secs_f64() * 1000.0
            );
        }
        self.behind_until = Some(now + BEHIND_GRACE);
    }
}

/// What this link has streamed so far, for the dashboard and the log.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StreamCounters {
    /// Video frames that left whole.
    pub(crate) videos: u64,
    /// Bytes of those video frames.
    pub(crate) video_bytes: u64,
    /// Audio chunks that left whole.
    pub(crate) audios: u64,
    /// Media frames skipped without being written.
    pub(crate) dropped: u64,
    /// Media writes that timed out.
    pub(crate) lost: u64,
    /// Times the phone was seen falling behind.
    pub(crate) behind_marks: u64,
    /// Total time spent waiting for keyframes.
    pub(crate) keyframe_waited: Duration,
    /// Total time the pipe spent accepting media.
    pub(crate) write_time: Duration,
    /// Worst single media write.
    pub(crate) write_max: Duration,
}

/// Turns a frame interval into the two deadlines a media write gets.
fn media_deadlines(frame_interval: Duration) -> (Duration, Duration) {
    let attempt = frame_interval
        .saturating_mul(MEDIA_WRITE_INTERVALS)
        .max(MEDIA_DEADLINE_FLOOR);
    let budget = frame_interval
        .saturating_mul(MEDIA_FRAME_INTERVALS)
        .max(MEDIA_DEADLINE_FLOOR);
    (attempt, budget)
}

/// Bytes read from USB, buffered until they form whole frames.
#[derive(Debug, Default)]
struct RxBuffer {
    bytes: Vec<u8>,
    start: usize,
}

impl RxBuffer {
    fn extend(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
    }

    fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        let pending = &self.bytes[self.start..];
        if pending.len() < HEADER_LEN {
            return Ok(None);
        }
        let mut header_bytes = [0_u8; HEADER_LEN];
        header_bytes.copy_from_slice(&pending[..HEADER_LEN]);
        let header = protocol::parse_header(&header_bytes)?;
        let total = HEADER_LEN + header.payload_len;
        if pending.len() < total {
            return Ok(None);
        }
        let payload = pending[HEADER_LEN..total].to_vec();
        self.start += total;
        self.compact();
        Ok(Some(Frame::with_flags(header.kind, header.flags, payload)))
    }

    fn compact(&mut self) {
        if self.start == self.bytes.len() {
            self.bytes.clear();
            self.start = 0;
        } else if self.start >= COMPACT_THRESHOLD {
            self.bytes.drain(..self.start);
            self.start = 0;
        }
    }
}

/// Failure while probing, switching, or talking to an accessory.
#[derive(Debug, Error)]
#[non_exhaustive]
pub(crate) enum AoaError {
    /// libusb reported an error.
    #[error("usb: {0}")]
    Usb(#[from] rusb::Error),
    /// No attached device answers the AOA protocol request.
    #[error("no attached phone supports the Android Open Accessory protocol")]
    NoDevice,
    /// A phone is attached but this user may not open its device node.
    #[error("permission denied for USB device {devices}; the udev rules are missing")]
    PermissionDenied {
        /// Comma-separated `vvvv:pppp` identifiers.
        devices: String,
    },
    /// A phone is offering its storage, so it was left to the file manager.
    #[error("phone {devices} is sharing its storage; leaving it for the file manager")]
    SharingStorage {
        /// Comma-separated `vvvv:pppp (function)` identifiers.
        devices: String,
    },
    /// Automatic takeover is switched off in the settings window.
    #[error("automatic takeover is off; switch it on to use the phone as a screen")]
    TakeoverDisabled,
    /// The port could not be reset to hand the phone back.
    #[error("cannot reset {path}: {error}")]
    PortReset {
        /// Device node the reset was tried on.
        path: String,
        /// Why it failed.
        error: String,
    },
    /// Another process holds the accessory interface.
    #[error("accessory interface is busy; is another AndroScreen daemon running?")]
    Busy,
    /// The phone accepted the switch but did not come back as an accessory.
    #[error("phone on bus {bus:?} did not re-enumerate as an accessory")]
    ReenumerationTimeout {
        /// Bus the handshake was sent on, when known.
        bus: Option<u8>,
    },
    /// The accessory has no usable bulk endpoint pair.
    #[error("accessory exposes no bulk endpoint pair")]
    MissingEndpoints,
    /// The phone was unplugged or stopped responding.
    #[error("accessory disconnected")]
    Disconnected,
    /// The phone app did not greet the host.
    #[error("phone app did not send Hello within {timeout:?}")]
    HelloTimeout {
        /// How long the host waited.
        timeout: Duration,
    },
    /// The frame stream is corrupted.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    /// A control message could not be encoded.
    #[error("control: {0}")]
    Control(#[from] ControlError),
}

/// Reads the buffered header fields; used by tests to assert resynchronization.
#[cfg(test)]
fn buffered_len(rx: &RxBuffer) -> usize {
    rx.bytes.len() - rx.start
}

#[cfg(test)]
mod tests {
    use super::*;
    use androscreen_core::protocol::{Kind, MAGIC, MAX_PAYLOAD, PROTOCOL_VERSION};

    fn frame(kind: Kind, payload: &[u8]) -> Frame {
        Frame::new(kind, payload.to_vec())
    }

    fn encoded(frame: &Frame) -> Vec<u8> {
        let mut bytes = Vec::new();
        protocol::write_frame(&mut bytes, frame).expect("encodes");
        bytes
    }

    fn raw_header(kind: u8, payload_len: u32) -> [u8; HEADER_LEN] {
        let mut header = [0_u8; HEADER_LEN];
        header[0..4].copy_from_slice(&MAGIC);
        header[4] = PROTOCOL_VERSION;
        header[5] = kind;
        header[8..12].copy_from_slice(&payload_len.to_le_bytes());
        header
    }

    #[test]
    fn accessory_ids_are_recognised() {
        assert!(is_accessory(ACCESSORY_VENDOR_ID, 0x2D00));
        assert!(is_accessory(ACCESSORY_VENDOR_ID, 0x2D05));
        assert!(!is_accessory(ACCESSORY_VENDOR_ID, 0x2D06));
        assert!(!is_accessory(0x2717, 0x2D00));
    }

    #[test]
    fn strings_are_null_terminated() {
        for (_, text) in AOA_STRINGS {
            let bytes = null_terminated(text);
            assert_eq!(bytes.len(), text.len() + 1);
            assert_eq!(bytes.last(), Some(&0));
        }
    }

    #[test]
    fn strings_cover_all_indices() {
        for (position, (index, _)) in AOA_STRINGS.iter().enumerate() {
            assert_eq!(usize::from(*index), position);
        }
    }

    #[test]
    fn rx_waits_for_complete_frame() {
        let original = frame(Kind::Touch, &[1, 2, 3, 4]);
        let bytes = encoded(&original);
        let mut rx = RxBuffer::default();

        rx.extend(&bytes[..HEADER_LEN - 1]);
        assert_eq!(rx.next_frame().expect("partial header"), None);
        rx.extend(&bytes[HEADER_LEN - 1..HEADER_LEN + 2]);
        assert_eq!(rx.next_frame().expect("partial payload"), None);
        rx.extend(&bytes[HEADER_LEN + 2..]);
        assert_eq!(rx.next_frame().expect("complete"), Some(original));
        assert_eq!(buffered_len(&rx), 0);
    }

    #[test]
    fn rx_yields_frames_back_to_back() {
        let first = frame(Kind::Control, b"{\"type\":\"stop_screen\"}");
        let second = frame(Kind::Ping, &[]);
        let mut bytes = encoded(&first);
        bytes.extend_from_slice(&encoded(&second));

        let mut rx = RxBuffer::default();
        rx.extend(&bytes);
        assert_eq!(rx.next_frame().expect("first"), Some(first));
        assert_eq!(rx.next_frame().expect("second"), Some(second));
        assert_eq!(rx.next_frame().expect("empty"), None);
    }

    #[test]
    fn rx_rejects_bad_magic() {
        let mut bytes = encoded(&frame(Kind::Ping, &[]));
        bytes[0] = b'X';
        let mut rx = RxBuffer::default();
        rx.extend(&bytes);
        assert!(matches!(
            rx.next_frame(),
            Err(ProtocolError::BadMagic { .. })
        ));
    }

    #[test]
    fn rx_rejects_oversized_frame_before_waiting() {
        let mut rx = RxBuffer::default();
        rx.extend(&raw_header(
            Kind::Video.as_byte(),
            u32::try_from(MAX_PAYLOAD + 1).expect("fits u32"),
        ));
        assert!(matches!(
            rx.next_frame(),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn rx_compacts_consumed_bytes() {
        let big = frame(Kind::Video, &vec![7_u8; COMPACT_THRESHOLD + 4096]);
        let small = frame(Kind::Ping, &[]);
        let small_bytes = encoded(&small);
        let mut bytes = encoded(&big);
        bytes.extend_from_slice(&small_bytes);

        let mut rx = RxBuffer::default();
        rx.extend(&bytes);
        assert_eq!(rx.next_frame().expect("big"), Some(big));
        assert_eq!(
            rx.bytes.len(),
            small_bytes.len(),
            "consumed bytes must be dropped"
        );
        assert_eq!(rx.next_frame().expect("small"), Some(small));
        assert_eq!(buffered_len(&rx), 0);
    }

    fn video(flags: u8) -> Frame {
        Frame::with_flags(Kind::Video, flags, vec![0_u8; 8])
    }

    fn key_frame() -> Frame {
        video(protocol::FLAG_KEYFRAME)
    }

    fn delta_frame() -> Frame {
        video(0)
    }

    fn late() -> Duration {
        BEHIND_AFTER + Duration::from_millis(1)
    }

    #[test]
    fn a_behind_phone_gives_up_on_sound_but_not_on_picture() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_written(&key_frame(), late(), now);

        assert!(flow.behind(now), "a late write means the phone is behind");
        assert!(
            flow.takes(&delta_frame(), now),
            "video already encoded is still worth sending"
        );
        assert!(
            !flow.takes(&frame(Kind::Audio, &[0; 4]), now),
            "half a sound chunk is worse than none of it"
        );
    }

    #[test]
    fn the_link_forgives_a_slow_write_once_the_phone_drains() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_written(&key_frame(), late(), now);
        flow.note_written(&delta_frame(), Duration::from_millis(2), now);

        assert!(!flow.behind(now), "a frame that left quickly caught up");
        assert!(flow.takes(&frame(Kind::Audio, &[0; 4]), now));
    }

    #[test]
    fn a_quiet_link_opens_again_on_its_own() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_written(&key_frame(), late(), now);

        assert!(
            flow.behind(now + BEHIND_GRACE / 2),
            "the phone is still draining"
        );
        assert!(
            !flow.behind(now + BEHIND_GRACE * 2),
            "a frozen desktop produces no writes to clear the backlog, so the \
             deadline has to expire by itself"
        );
    }

    #[test]
    fn a_picture_lost_in_the_middle_waits_for_a_whole_one() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_lost(&delta_frame(), Duration::ZERO, now);

        assert!(
            !flow.takes(&delta_frame(), now),
            "a stream missing its middle cannot be decoded"
        );
        assert!(
            flow.takes(&key_frame(), now),
            "a keyframe is where the picture starts again"
        );

        flow.note_written(&key_frame(), Duration::from_millis(2), now);
        assert!(flow.takes(&delta_frame(), now), "the stream resumed");
    }

    #[test]
    fn lost_sound_does_not_hold_the_picture() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_lost(&frame(Kind::Audio, &[0; 4]), Duration::ZERO, now);

        assert!(!flow.keyframe_pending, "each audio chunk stands on its own");
        assert!(flow.takes(&delta_frame(), now + BEHIND_GRACE * 2));
    }

    #[test]
    fn a_keyframe_wait_is_counted_until_the_keyframe_arrives() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_lost(&delta_frame(), Duration::ZERO, now);
        assert_eq!(flow.lost, 1);
        assert!(flow.keyframe_since.is_some(), "a lost frame starts a wait");

        flow.note_written(
            &key_frame(),
            Duration::ZERO,
            now + Duration::from_millis(120),
        );
        assert_eq!(flow.keyframe_waited, Duration::from_millis(120));
        assert!(flow.keyframe_since.is_none(), "the wait ended with the IDR");
        assert_eq!(flow.videos, 1);
    }

    #[test]
    fn a_pipe_that_takes_nothing_is_timed_from_the_first_silent_write() {
        let now = Instant::now();
        let mut flow = Flow::default();
        assert_eq!(flow.stalled_for(now), Duration::ZERO);

        flow.note_silence(now);
        flow.note_silence(now + MEDIA_STALL_LIMIT / 2);
        assert_eq!(
            flow.stalled_for(now + MEDIA_STALL_LIMIT),
            MEDIA_STALL_LIMIT,
            "later silent writes must not restart the clock, or a phone that \
             died would look busy again on every retry"
        );
    }

    #[test]
    fn a_frame_that_leaves_clears_the_silence() {
        let now = Instant::now();
        let mut flow = Flow::default();
        flow.note_silence(now);
        flow.note_written(&key_frame(), Duration::from_millis(2), now);

        assert_eq!(
            flow.stalled_for(now + MEDIA_STALL_LIMIT * 2),
            Duration::ZERO,
            "a phone that drained one frame is only catching up"
        );
    }
}
