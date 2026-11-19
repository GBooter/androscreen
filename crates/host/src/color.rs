//! BGRA-to-NV12 conversion for the encoder pipe.
//!
//! ffmpeg reads raw pixels from a pipe, so every byte costs time twice: once
//! writing the frame and once reading it in the encoder process. Handing it
//! NV12 instead of the grabbed BGRA halves the payload (six bytes per two
//! pixels against eight) and lets the hardware encoder skip its own colour
//! conversion. The conversion itself is the BT.601 limited-range pair that
//! desktop compositors assume.
//!
//! Only the rectangles a grab refreshed are converted, and the result lives in
//! the buffer the encoder thread recycles, so a still desktop costs almost
//! nothing beyond the cursor.

use std::thread;

use crate::evdi::{ClipRect, EvdiError, FrameBuffer, Mode, PixelFormat};

/// Bytes per source pixel.
const BPP: usize = PixelFormat::BYTES_PER_PIXEL;

/// Work below this many pixels stays on the calling thread: waking the other
/// cores costs more than the conversion saves.
const PARALLEL_MIN_PIXELS: usize = 256 * 1024;

/// Upper bound on conversion threads. The loop is memory-bound well before a
/// machine gets this wide, and the session thread is busy grabbing and copying
/// the next picture at the same time, so the conversion does not take more
/// cores than the machine has.
const MAX_THREADS: usize = 4;

/// Pixels whose luma and chroma one pass converts.
#[derive(Debug, Clone, Copy)]
struct Band {
    x1: usize,
    x2: usize,
    y1: usize,
    y2: usize,
}

impl Band {
    /// Number of luma pixels covered.
    fn pixels(self) -> usize {
        (self.x2 - self.x1).saturating_mul(self.y2 - self.y1)
    }
}

/// A frame in the layout ffmpeg reads as `nv12`: a luma plane followed by one
/// interleaved U/V pair per 2x2 block.
#[derive(Debug)]
pub(crate) struct Nv12Frame {
    width: u32,
    height: u32,
    format: PixelFormat,
    pixels: Vec<u8>,
}

impl Nv12Frame {
    /// Allocates a zeroed frame matching `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`EvdiError::InvalidMode`] when the geometry is odd (4:2:0
    /// cannot describe half a chroma pair) or too large to allocate.
    pub(crate) fn new(mode: Mode) -> Result<Self, EvdiError> {
        let invalid = || EvdiError::InvalidMode {
            width: mode.width,
            height: mode.height,
        };
        if mode.width % 2 != 0 || mode.height % 2 != 0 {
            return Err(invalid());
        }
        let width = usize::try_from(mode.width).map_err(|_| invalid())?;
        let height = usize::try_from(mode.height).map_err(|_| invalid())?;
        let pixels = width
            .checked_mul(height)
            .and_then(|area| area.checked_mul(3))
            .and_then(|bytes| bytes.checked_div(2))
            .ok_or_else(invalid)?;
        Ok(Self {
            width: mode.width,
            height: mode.height,
            format: mode.format,
            pixels: vec![0; pixels],
        })
    }

    /// The whole frame, luma plane first, in `nv12` order.
    #[must_use]
    pub(crate) fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Converts the whole picture from another buffer of the same shape.
    pub(crate) fn convert_all_from(&mut self, source: &FrameBuffer) {
        let band = Band {
            x1: 0,
            x2: usize::try_from(self.width).unwrap_or(0),
            y1: 0,
            y2: usize::try_from(self.height).unwrap_or(0),
        };
        self.convert(source, &[band]);
    }

    /// Converts the rectangles a grab just refreshed from another buffer.
    ///
    /// A grab only refreshes what changed since the previous one, so a buffer
    /// staying in step converts exactly those parts; the rest of the frame
    /// still describes the desktop of the frame before.
    pub(crate) fn convert_rects_from(&mut self, source: &FrameBuffer, rects: &[ClipRect]) {
        let bands = self.bands(rects);
        self.convert(source, &bands);
    }

    /// Turns grabbed rectangles into even-aligned ones.
    fn bands(&self, rects: &[ClipRect]) -> Vec<Band> {
        chroma_rects(rects, self.width, self.height)
            .into_iter()
            .map(|rect| Band {
                x1: usize::from(rect.x1),
                x2: usize::from(rect.x2),
                y1: usize::from(rect.y1),
                y2: usize::from(rect.y2),
            })
            .collect()
    }

    fn convert(&mut self, source: &FrameBuffer, bands: &[Band]) {
        let width = usize::try_from(self.width).unwrap_or(0);
        let height = usize::try_from(self.height).unwrap_or(0);
        debug_assert_eq!(
            (source.width(), source.height()),
            (self.width, self.height),
            "both buffers belong to the same display"
        );
        if width == 0 || height == 0 || bands.is_empty() {
            return;
        }
        let luma_len = width * height;
        let threads = thread_budget(bands, height);
        let format = self.format;
        let (luma, uv) = self.pixels.split_at_mut(luma_len);
        match format {
            PixelFormat::Bgra => convert_with::<2, 1, 0>(source, bands, width, luma, uv, threads),
            PixelFormat::Rgba => convert_with::<0, 1, 2>(source, bands, width, luma, uv, threads),
        }
    }
}

/// Rounds grabbed rectangles out to whole 2x2 blocks and clips them to the
/// frame.
///
/// A chroma sample describes a 2x2 block, so a rectangle that covers half a
/// block would leave the sample describing pixels from the frame before. The
/// copy that mirrors the grab into the buffer the encoder thread converts from
/// walks the same rectangles, so both have to agree on their edges.
pub(crate) fn chroma_rects(rects: &[ClipRect], width: u32, height: u32) -> Vec<ClipRect> {
    let width = usize::try_from(width).unwrap_or(0);
    let height = usize::try_from(height).unwrap_or(0);
    let mut padded = Vec::with_capacity(rects.len());
    for rect in rects {
        let x1 = usize::from(rect.x1).min(width);
        let x2 = usize::from(rect.x2).min(width);
        let y1 = usize::from(rect.y1).min(height);
        let y2 = usize::from(rect.y2).min(height);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        let (x1, x2) = (x1 & !1, ((x2 + 1) & !1).min(width));
        let (y1, y2) = (y1 & !1, ((y2 + 1) & !1).min(height));
        let (Ok(x1), Ok(x2), Ok(y1), Ok(y2)) = (
            u16::try_from(x1),
            u16::try_from(x2),
            u16::try_from(y1),
            u16::try_from(y2),
        ) else {
            continue;
        };
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        padded.push(ClipRect { x1, y1, x2, y2 });
    }
    padded
}

/// How many threads this conversion deserves, never more than chroma rows.
fn thread_budget(bands: &[Band], height: usize) -> usize {
    let pixels: usize = bands.iter().map(|band| band.pixels()).sum();
    if pixels < PARALLEL_MIN_PIXELS {
        return 1;
    }
    let cores = thread::available_parallelism().map_or(1, std::num::NonZero::get);
    // Every thread needs at least one chroma row to itself.
    cores.min(MAX_THREADS).min((height / 2).max(1)).max(1)
}

/// Converts `bands` into split luma and interleaved-chroma planes, dividing the
/// rows between threads so each one owns a disjoint slab of both planes.
fn convert_with<const R: usize, const G: usize, const B: usize>(
    source: &FrameBuffer,
    bands: &[Band],
    width: usize,
    luma: &mut [u8],
    uv: &mut [u8],
    threads: usize,
) {
    debug_assert_eq!(width % 2, 0, "nv12 wants an even width");
    let chroma_width = width / 2;
    let height = luma.len() / width;

    // Slab boundaries sit on even luma rows, so the chroma rows of two slabs
    // never straddle the same 2x2 block.
    let mut luma_rest = luma;
    let mut uv_rest = uv;
    let mut jobs = Vec::with_capacity(threads);
    for index in 0..threads {
        let y1 = index * (height / 2) / threads * 2;
        let y2 = (index + 1) * (height / 2) / threads * 2;
        let rows = y2 - y1;
        let (luma_part, tail) = luma_rest.split_at_mut(rows * width);
        luma_rest = tail;
        let (uv_part, tail) = uv_rest.split_at_mut(rows / 2 * chroma_width * 2);
        uv_rest = tail;
        jobs.push((y1, luma_part, y1 / 2, uv_part));
    }

    if let [(y1, luma_part, chroma_y1, uv_part)] = jobs.as_mut_slice() {
        luma_bands::<R, G, B>(source, luma_part, *y1, width, bands);
        chroma_bands::<R, G, B>(source, uv_part, *chroma_y1, chroma_width, bands);
        return;
    }

    thread::scope(|scope| {
        for (y1, luma_part, chroma_y1, uv_part) in jobs {
            scope.spawn(move || {
                luma_bands::<R, G, B>(source, luma_part, y1, width, bands);
                chroma_bands::<R, G, B>(source, uv_part, chroma_y1, chroma_width, bands);
            });
        }
    });
}

/// Writes the luma of every band whose rows fall inside this thread's slab.
fn luma_bands<const R: usize, const G: usize, const B: usize>(
    source: &FrameBuffer,
    luma: &mut [u8],
    origin: usize,
    width: usize,
    bands: &[Band],
) {
    let rows = luma.len() / width;
    let stride = source.stride();
    let src = source.pixels();
    for band in bands {
        let y1 = band.y1.max(origin);
        let y2 = band.y2.min(origin + rows);
        for y in y1..y2 {
            let src_row = &src[y * stride + band.x1 * BPP..y * stride + band.x2 * BPP];
            let dst_row = &mut luma[(y - origin) * width + band.x1..(y - origin) * width + band.x2];
            for (pixel, out) in src_row.chunks_exact(BPP).zip(dst_row.iter_mut()) {
                *out = (((66 * u32::from(pixel[R])
                    + 129 * u32::from(pixel[G])
                    + 25 * u32::from(pixel[B])
                    + 128)
                    >> 8)
                    + 16) as u8;
            }
        }
    }
}

/// Writes the interleaved U/V pairs of every band inside this thread's slab.
///
/// Each sample averages the 2x2 block it describes, so the coefficient sum
/// divides by four on top of the usual 256 (ten bits instead of eight).
fn chroma_bands<const R: usize, const G: usize, const B: usize>(
    source: &FrameBuffer,
    uv: &mut [u8],
    origin: usize,
    chroma_width: usize,
    bands: &[Band],
) {
    let rows = uv.len() / (chroma_width * 2);
    let stride = source.stride();
    let src = source.pixels();
    for band in bands {
        let y1 = (band.y1 / 2).max(origin);
        let y2 = (band.y2 / 2).min(origin + rows);
        let block_x1 = band.x1 / 2;
        let block_x2 = band.x2 / 2;
        for y in y1..y2 {
            let upper = &src[2 * y * stride..];
            let lower = &src[(2 * y + 1) * stride..];
            let first = (y - origin) * chroma_width + block_x1;
            let last = (y - origin) * chroma_width + block_x2;
            for (block, pair) in uv[first * 2..last * 2].chunks_exact_mut(2).enumerate() {
                let offset = (block_x1 + block) * 2 * BPP;
                let red = i32::from(upper[offset + R])
                    + i32::from(upper[offset + BPP + R])
                    + i32::from(lower[offset + R])
                    + i32::from(lower[offset + BPP + R]);
                let green = i32::from(upper[offset + G])
                    + i32::from(upper[offset + BPP + G])
                    + i32::from(lower[offset + G])
                    + i32::from(lower[offset + BPP + G]);
                let blue = i32::from(upper[offset + B])
                    + i32::from(upper[offset + BPP + B])
                    + i32::from(lower[offset + B])
                    + i32::from(lower[offset + BPP + B]);
                let u = ((-38 * red - 74 * green + 112 * blue + 512) >> 10) + 128;
                let v = ((112 * red - 94 * green - 18 * blue + 512) >> 10) + 128;
                pair[0] = u.clamp(0, 255) as u8;
                pair[1] = v.clamp(0, 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, format: PixelFormat) -> Mode {
        Mode {
            width,
            height,
            refresh_hz: 60,
            format,
        }
    }

    fn solid(mode: Mode, rgba: [u8; 4]) -> FrameBuffer {
        let mut frame = FrameBuffer::new(mode).expect("geometry fits");
        for pixel in frame.pixels_mut().chunks_exact_mut(BPP) {
            pixel.copy_from_slice(&rgba);
        }
        frame
    }

    #[test]
    fn nv12_frame_holds_three_bytes_per_two_pixels() {
        let frame = Nv12Frame::new(mode(4, 2, PixelFormat::Bgra)).expect("geometry fits");
        assert_eq!(frame.pixels().len(), 4 * 2 * 3 / 2);
        assert!(Nv12Frame::new(mode(3, 2, PixelFormat::Bgra)).is_err());
        assert!(Nv12Frame::new(mode(4, 3, PixelFormat::Bgra)).is_err());
    }

    #[test]
    fn red_converts_to_bt601_limited_range() {
        // BGRA memory order for opaque red.
        let shape = mode(4, 4, PixelFormat::Bgra);
        let source = solid(shape, [0, 0, 255, 255]);
        let mut frame = Nv12Frame::new(shape).expect("geometry fits");
        frame.convert_all_from(&source);
        let luma = &frame.pixels()[..16];
        assert!(luma.iter().all(|y| *y == 82), "luma {luma:?}");
        let chroma = &frame.pixels()[16..];
        assert_eq!(chroma, [90, 240].repeat(4), "one U/V pair per block");
    }

    #[test]
    fn rgba_sources_use_the_other_byte_order() {
        let shape = mode(4, 4, PixelFormat::Rgba);
        let source = solid(shape, [255, 0, 0, 255]);
        let mut frame = Nv12Frame::new(shape).expect("geometry fits");
        frame.convert_all_from(&source);
        assert!(frame.pixels()[..16].iter().all(|y| *y == 82));
        assert_eq!(&frame.pixels()[16..24], [90, 240].repeat(4).as_slice());
    }

    #[test]
    fn a_rect_converts_to_the_block_edges_and_leaves_the_rest() {
        let shape = mode(4, 4, PixelFormat::Bgra);
        let mut source = solid(shape, [0, 0, 255, 255]);
        let mut frame = Nv12Frame::new(shape).expect("geometry fits");
        frame.convert_all_from(&source);

        // Repaint a single pixel: the 2x2 block around it owns a chroma sample
        // and the other three pixels of that block must be re-read as well.
        let stride = source.stride();
        source.pixels_mut()[stride + BPP..stride + 2 * BPP].copy_from_slice(&[0, 255, 0, 255]);
        frame.convert_rects_from(
            &source,
            &[ClipRect {
                x1: 1,
                y1: 1,
                x2: 2,
                y2: 2,
            }],
        );

        let pixels = frame.pixels();
        assert_eq!(pixels[0], 82, "red stays red");
        assert_eq!(pixels[5], 144, "the repainted pixel became green luma");
        assert_eq!(
            &pixels[16..18],
            [81, 188],
            "the block averages its four pixels"
        );
        assert_eq!(
            &pixels[18..],
            [90, 240].repeat(3).as_slice(),
            "other blocks untouched"
        );
    }

    #[test]
    fn rects_are_clipped_to_the_frame() {
        let shape = mode(4, 4, PixelFormat::Bgra);
        let source = solid(shape, [0, 0, 255, 255]);
        let mut frame = Nv12Frame::new(shape).expect("geometry fits");
        frame.convert_rects_from(
            &source,
            &[ClipRect {
                x1: 0,
                y1: 0,
                x2: 99,
                y2: 99,
            }],
        );
        assert!(frame.pixels()[..16].iter().all(|y| *y == 82));
    }
}
