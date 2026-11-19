//! Minimal EDID 1.4 builder for evdi virtual displays.
//!
//! evdi only exposes the modes advertised by the EDID blob passed to
//! `evdi_connect`. Building a blob per phone request lets the compositor pick
//! exactly the aspect ratio of the phone instead of letterboxing a fixed 1080p
//! desktop.

use thiserror::Error;

/// Size of a base EDID block.
pub const EDID_LEN: usize = 128;

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
/// PNP id "ASR", big endian.
const MANUFACTURER: [u8; 2] = [0x06, 0x72];
/// Canonical sRGB chromaticity bytes.
const CHROMATICITY: [u8; 10] = [0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54];
/// Horizontal blanking used by CVT reduced blanking.
const H_BLANK: u32 = 160;
const H_FRONT_PORCH: u32 = 48;
const H_SYNC: u32 = 32;
const V_FRONT_PORCH: u32 = 3;
const V_SYNC: u32 = 5;
/// 340 mm across the panel's longest edge, near a phone held as a screen.
const LONG_EDGE_MM: u32 = 340;

/// The virtual display geometry to advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh rate in hertz.
    pub refresh_hz: u32,
}

/// The panel's size in millimetres, standing up when the mode is tall.
///
/// A compositor may pick a scale from the advertised size, so the longer edge
/// keeps a plausible measurement instead of scaling a phone out to a metre.
fn physical_size(mode: DisplayMode) -> (u32, u32) {
    let long = mode.width.max(mode.height);
    let short = mode.width.min(mode.height);
    let short_mm = (LONG_EDGE_MM * short / long).max(1);
    if mode.width >= mode.height {
        (LONG_EDGE_MM, short_mm)
    } else {
        (short_mm, LONG_EDGE_MM)
    }
}

/// The same geometry with its width and height exchanged.
#[must_use]
pub const fn rotated_mode(mode: DisplayMode) -> DisplayMode {
    DisplayMode {
        width: mode.height,
        height: mode.width,
        refresh_hz: mode.refresh_hz,
    }
}

/// Builds a 128-byte EDID block advertising a mode and its rotated counterpart.
///
/// Both shapes are advertised so a phone can turn without detaching the
/// connector, keeping the desktop from treating the rotation as a complete
/// monitor removal.
///
/// The order they are advertised in does not depend on which one the phone is
/// wearing: a desktop keys a monitor's saved settings — its eye protection and
/// its scale among them — by a hash of these bytes, so a phone that starts
/// sideways would otherwise look like a different monitor from the same phone
/// standing up, and everything the user chose for it would be lost. The taller
/// shape is put first because that is the shape a phone is connected in most of
/// the time, and a desktop that picks the preferred mode on its own then picks
/// the one the host asks for anyway.
///
/// # Errors
///
/// Returns [`EdidError`] when either orientation cannot fit the detailed timing
/// descriptor or the resulting pixel clock exceeds the 16-bit EDID field.
pub fn build_edid(mode: DisplayMode, name: &str) -> Result<[u8; EDID_LEN], EdidError> {
    if !(1..=4095).contains(&mode.width) || !(1..=4095).contains(&mode.height) {
        return Err(EdidError::Dimension {
            width: mode.width,
            height: mode.height,
        });
    }
    if !(1..=240).contains(&mode.refresh_hz) {
        return Err(EdidError::Refresh {
            refresh_hz: mode.refresh_hz,
        });
    }

    let upright = if mode.height >= mode.width {
        mode
    } else {
        rotated_mode(mode)
    };

    let mut edid = [0_u8; EDID_LEN];
    edid[0..8].copy_from_slice(&HEADER);
    edid[8..10].copy_from_slice(&MANUFACTURER);
    edid[10..12].copy_from_slice(&1_u16.to_le_bytes());
    edid[12..16].copy_from_slice(&1_u32.to_le_bytes());
    edid[16] = 1;
    edid[17] = 36;
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0x80;
    let (width_mm, height_mm) = physical_size(upright);
    edid[21] = (width_mm / 10).clamp(1, 255) as u8;
    edid[22] = (height_mm / 10).clamp(1, 255) as u8;
    edid[23] = 120;
    edid[24] = 0x4A;
    edid[25..35].copy_from_slice(&CHROMATICITY);
    edid[35..38].fill(0);
    for offset in (38..54).step_by(2) {
        edid[offset] = 0x01;
        edid[offset + 1] = 0x01;
    }

    edid[54..72].copy_from_slice(&detailed_timing(upright)?);
    edid[72..90].copy_from_slice(&range_limits());
    edid[90..108].copy_from_slice(&display_name(name));
    edid[108..126].copy_from_slice(&detailed_timing(rotated_mode(upright))?);
    edid[126] = 0;
    edid[127] = checksum(&edid);
    Ok(edid)
}

fn detailed_timing(mode: DisplayMode) -> Result<[u8; 18], EdidError> {
    let h_total = mode.width + H_BLANK;
    let v_blank = ((mode.height * 62).div_ceil(2160)).max(21);
    let v_total = mode.height + v_blank;
    let clock_hz = u64::from(h_total) * u64::from(v_total) * u64::from(mode.refresh_hz);
    let clock_10khz = clock_hz.div_ceil(10_000);
    let pixel_clock =
        u16::try_from(clock_10khz).map_err(|_| EdidError::PixelClock { hertz: clock_hz })?;

    let mut timing = [0_u8; 18];
    timing[0..2].copy_from_slice(&pixel_clock.to_le_bytes());
    timing[2] = mode.width as u8;
    timing[3] = H_BLANK as u8;
    timing[4] = (((mode.width >> 8) & 0x0F) << 4) as u8 | ((H_BLANK >> 8) & 0x0F) as u8;
    timing[5] = mode.height as u8;
    timing[6] = v_blank as u8;
    timing[7] = (((mode.height >> 8) & 0x0F) << 4 | ((v_blank >> 8) & 0x0F)) as u8;
    timing[8] = H_FRONT_PORCH as u8;
    timing[9] = H_SYNC as u8;
    timing[10] = ((V_FRONT_PORCH & 0x0F) << 4 | (V_SYNC & 0x0F)) as u8;
    timing[11] = 0;
    let (width_mm, height_mm) = physical_size(mode);
    timing[12] = width_mm as u8;
    timing[13] = height_mm as u8;
    timing[14] = (((width_mm >> 8) & 0x0F) << 4 | ((height_mm >> 8) & 0x0F)) as u8;
    timing[17] = 0x1A;
    Ok(timing)
}

fn range_limits() -> [u8; 18] {
    let mut descriptor = [0_u8; 18];
    descriptor[3] = 0xFD;
    descriptor[5] = 30;
    descriptor[6] = 240;
    descriptor[7] = 30;
    descriptor[8] = 255;
    descriptor[9] = 60;
    descriptor[11] = 0x0A;
    descriptor
}

fn display_name(name: &str) -> [u8; 18] {
    let mut descriptor = [0_u8; 18];
    descriptor[3] = 0xFC;
    let bytes: Vec<u8> = name.bytes().filter(u8::is_ascii_graphic).take(13).collect();
    if bytes.is_empty() {
        descriptor[5..13].copy_from_slice(b"Virtual");
        descriptor[13] = 0x0A;
        return descriptor;
    }
    let length = bytes.len();
    descriptor[5..5 + length].copy_from_slice(&bytes);
    if length < 13 {
        descriptor[5 + length] = 0x0A;
    }
    descriptor
}

fn checksum(block: &[u8; EDID_LEN]) -> u8 {
    block[..EDID_LEN - 1]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg()
}

/// A mode or name that cannot be represented in an EDID block.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EdidError {
    /// Width or height is zero or does not fit twelve bits.
    #[error("display dimension {width}x{height} cannot be encoded in EDID")]
    Dimension {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Refresh rate is outside `1..=240`.
    #[error("refresh rate {refresh_hz} Hz cannot be encoded in EDID")]
    Refresh {
        /// Requested refresh rate.
        refresh_hz: u32,
    },
    /// The computed pixel clock does not fit the 16-bit field.
    #[error("pixel clock {hertz} Hz exceeds the EDID limit")]
    PixelClock {
        /// Computed pixel clock in hertz.
        hertz: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_hz: 60,
        }
    }

    fn dtd_word(edid: &[u8; EDID_LEN], offset: usize) -> u32 {
        let low = u32::from(edid[offset]);
        let high = u32::from(edid[offset + 1]) << 8;
        low | high
    }

    fn dtd_geometry(edid: &[u8; EDID_LEN], offset: usize) -> (u32, u32) {
        let width = u32::from(edid[offset + 2]) | (u32::from(edid[offset + 4] >> 4) << 8);
        let height = u32::from(edid[offset + 5]) | (u32::from(edid[offset + 7] >> 4) << 8);
        (width, height)
    }

    /// The blanking the descriptor at `offset` advertises, horizontally and vertically.
    fn blanking(edid: &[u8; EDID_LEN], offset: usize) -> (u32, u32) {
        let horizontal = u32::from(edid[offset + 3]) | (u32::from(edid[offset + 4] & 0x0F) << 8);
        let vertical = u32::from(edid[offset + 6]) | (u32::from(edid[offset + 7] & 0x0F) << 8);
        (horizontal, vertical)
    }

    /// The panel size in millimetres the descriptor at `offset` advertises.
    fn millimetres(edid: &[u8; EDID_LEN], offset: usize) -> (u32, u32) {
        let width = u32::from(edid[offset + 12]) | (u32::from(edid[offset + 14] >> 4) << 8);
        let height = u32::from(edid[offset + 13]) | (u32::from(edid[offset + 14] & 0x0F) << 8);
        (width, height)
    }

    /// The refresh rate the descriptor at `offset` works out to.
    fn refresh_hz(edid: &[u8; EDID_LEN], offset: usize) -> f64 {
        let clock = dtd_word(edid, offset);
        let (width, height) = dtd_geometry(edid, offset);
        let (horizontal, vertical) = blanking(edid, offset);
        f64::from(clock) * 10_000.0 / f64::from(width + horizontal) / f64::from(height + vertical)
    }

    #[test]
    fn header_and_checksum() {
        let edid = build_edid(mode(1920, 1080), "AndroScreen").expect("builds");
        assert_eq!(edid[0..8], HEADER);
        let sum = edid.iter().fold(0_u8, |acc, byte| acc.wrapping_add(*byte));
        assert_eq!(sum, 0, "EDID checksum must make the block sum to zero");
        assert_eq!(edid[126], 0, "no extension blocks");
    }

    #[test]
    fn both_shapes_are_advertised_with_the_taller_one_first() {
        let edid = build_edid(mode(1080, 2460), "AndroScreen").expect("builds");
        assert_eq!(
            dtd_geometry(&edid, 54),
            (1080, 2460),
            "the shape a phone is connected in is the one a desktop prefers"
        );
        assert_eq!(
            dtd_geometry(&edid, 108),
            (2460, 1080),
            "turning the phone selects a mode from the monitor that is already online"
        );
    }

    #[test]
    fn a_phone_that_connects_sideways_is_the_same_monitor() {
        let standing = build_edid(mode(1080, 2460), "AndroScreen").expect("builds");
        let sideways = build_edid(mode(2460, 1080), "AndroScreen").expect("builds");
        assert_eq!(
            standing, sideways,
            "a desktop remembers a monitor by these bytes, so the same phone has to look \
             like the same monitor whichever way it is held"
        );
    }

    #[test]
    fn the_timing_carries_the_request_at_the_refresh_it_asked_for() {
        let edid = build_edid(mode(1080, 1920), "AndroScreen").expect("builds");
        let turning = 108;
        assert_eq!(dtd_word(&edid, turning), 13_866);
        assert_eq!(dtd_geometry(&edid, turning), (1920, 1080));
        assert_eq!(blanking(&edid, turning), (160, 31));
        assert_eq!(
            millimetres(&edid, turning),
            (340, 191),
            "panels are advertised in mm"
        );
        assert!((refresh_hz(&edid, turning) - 60.0).abs() < 0.1);
    }

    #[test]
    fn name_descriptor_contains_name() {
        let edid = build_edid(mode(1920, 1080), "AndroScreen").expect("builds");
        assert_eq!(edid[93], 0xFC);
        assert_eq!(&edid[95..106], b"AndroScreen");
        assert_eq!(edid[106], 0x0A);
    }

    #[test]
    fn long_names_are_truncated() {
        let edid = build_edid(mode(1280, 720), "A-very-long-display-name").expect("builds");
        let name = &edid[95..108];
        assert_eq!(&name[..13], b"A-very-long-d");
    }

    #[test]
    fn phone_aspect_ratio_is_preserved() {
        let edid = build_edid(mode(2400, 1080), "Phone").expect("builds");
        let (width, height) = dtd_geometry(&edid, 54);
        assert_eq!((width, height), (1080, 2400));
        assert_eq!(blanking(&edid, 54), (160, 69));
        assert!((refresh_hz(&edid, 54) - 60.0).abs() < 0.1);
    }

    #[test]
    fn a_tall_panel_is_advertised_standing_up() {
        let landscape = physical_size(mode(2400, 1080));
        let portrait = physical_size(mode(1080, 2400));
        assert_eq!(landscape, (340, 153));
        assert_eq!(
            portrait,
            (153, 340),
            "the long edge keeps its size whichever way the phone points"
        );

        let edid = build_edid(mode(1080, 2400), "Phone").expect("builds");
        assert_eq!(
            (edid[21], edid[22]),
            (15, 34),
            "centimetres, narrow and tall"
        );
        assert_eq!(millimetres(&edid, 54), (153, 340));
    }

    #[test]
    fn four_k_matches_evdi_sample_clock() {
        let edid = build_edid(mode(3840, 2160), "Sample").expect("builds");
        assert_eq!(dtd_word(&edid, 108), 53_328);
    }

    #[test]
    fn rejects_unencodable_modes() {
        assert!(matches!(
            build_edid(mode(0, 1080), "x"),
            Err(EdidError::Dimension { .. })
        ));
        assert!(matches!(
            build_edid(
                DisplayMode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 0
                },
                "x"
            ),
            Err(EdidError::Refresh { .. })
        ));
        assert!(matches!(
            build_edid(
                DisplayMode {
                    width: 3840,
                    height: 3840,
                    refresh_hz: 240
                },
                "x"
            ),
            Err(EdidError::PixelClock { .. })
        ));
    }
}
