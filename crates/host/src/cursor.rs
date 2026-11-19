//! Pointer state the phone draws itself.
//!
//! evdi reports the compositor's cursor as a bitmap plus a position once cursor
//! events are on. Keeping that out of the video stream is what makes the
//! pointer follow the mouse at the phone's own refresh rate instead of the
//! stream's, so this module only has to do two things: hold the latest state,
//! and hand the wire one small frame at a time.

use androscreen_core::protocol::Frame;

use crate::aoa::{AoaError, AoaLink};

/// What the phone currently knows about the pointer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Sent {
    /// Nothing has been sent yet.
    #[default]
    Nothing,
    /// The phone holds a bitmap but is not drawing it.
    Hidden,
    /// The phone is drawing the bitmap.
    Visible,
}

/// The pointer as the phone should draw it.
#[derive(Debug, Default)]
pub(crate) struct CursorFeed {
    /// Bitmap width and height in virtual-display pixels.
    shape: (u16, u16),
    /// Hotspot inside the bitmap.
    hotspot: (i16, i16),
    /// Straight RGBA pixels of the bitmap.
    pixels: Vec<u8>,
    /// Whether a bitmap still has to reach the phone.
    shape_pending: bool,
    /// Size of the virtual display the bitmap is scaled against.
    screen: (u16, u16),
    /// Last position reported by the compositor, in virtual-display pixels.
    position: Option<(i32, i32)>,
    /// Whether the compositor says the pointer is visible.
    visible: bool,
    /// What the phone was last told.
    sent: Sent,
    /// Last normalized position sent, so a still pointer costs nothing.
    sent_position: Option<(u16, u16)>,
}

impl CursorFeed {
    /// Creates a feed for a display of the given size.
    #[must_use]
    pub(crate) fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            screen: (
                u16::try_from(screen_width).unwrap_or(u16::MAX),
                u16::try_from(screen_height).unwrap_or(u16::MAX),
            ),
            ..Self::default()
        }
    }

    /// Replaces the bitmap, which is sent on the next pump.
    pub(crate) fn set_shape(
        &mut self,
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        pixels: Vec<u8>,
    ) {
        self.shape = (
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
        );
        self.hotspot = (
            i16::try_from(hot_x).unwrap_or(0),
            i16::try_from(hot_y).unwrap_or(0),
        );
        self.pixels = pixels;
        self.shape_pending = true;
        self.visible = true;
    }

    /// Records where the compositor put the pointer.
    pub(crate) fn moved(&mut self, x: i32, y: i32) {
        self.position = Some((x, y));
    }

    /// Hides the pointer without forgetting its bitmap.
    pub(crate) fn hide(&mut self) {
        self.visible = false;
    }

    /// Re-points the cached bitmap at a new display shape.
    ///
    /// A turned or resized virtual display scales the same bitmap differently,
    /// so the phone is sent it again rather than left with the old size.
    pub(crate) fn set_screen(&mut self, width: u32, height: u32) {
        self.screen = (
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
        );
        if !self.pixels.is_empty() {
            self.shape_pending = true;
            self.sent_position = None;
        }
    }

    /// Whether a bitmap has arrived for this screen.
    #[must_use]
    pub(crate) fn has_shape(&self) -> bool {
        !self.pixels.is_empty()
    }

    /// Sends whatever the phone is still owed, at most one update per frame.
    ///
    /// # Errors
    ///
    /// Forwards [`AoaLink::send_lossy`] failures. A dropped update is harmless:
    /// the next position replaces it, and a dropped bitmap is sent again on the
    /// next pump.
    pub(crate) fn pump(&mut self, link: &mut AoaLink) -> Result<(), AoaError> {
        if let Some(frame) = self.next_frame() {
            link.send_lossy(&frame)?;
        }
        Ok(())
    }

    /// The next cursor frame the phone needs, if any.
    fn next_frame(&mut self) -> Option<Frame> {
        if self.shape_pending && !self.pixels.is_empty() {
            self.shape_pending = false;
            self.sent = Sent::Visible;
            self.sent_position = None;
            return Some(Frame::cursor_shape(
                self.shape.0,
                self.shape.1,
                self.hotspot.0,
                self.hotspot.1,
                self.screen.0,
                self.screen.1,
                self.pixels.clone(),
            ));
        }
        if !self.visible {
            if self.sent == Sent::Visible {
                self.sent = Sent::Hidden;
                self.sent_position = None;
                return Some(Frame::cursor_hide());
            }
            return None;
        }
        if self.sent == Sent::Nothing {
            // A position without a bitmap would draw nothing.
            return None;
        }
        let (x, y) = self.position?;
        let normalized = (normalize(x, self.screen.0), normalize(y, self.screen.1));
        if self.sent_position == Some(normalized) {
            return None;
        }
        self.sent_position = Some(normalized);
        self.sent = Sent::Visible;
        Some(Frame::cursor_move(normalized.0, normalized.1))
    }
}

/// Scales a virtual-display coordinate onto the wire's `0..=65535` range.
fn normalize(value: i32, extent: u16) -> u16 {
    if extent == 0 {
        return 0;
    }
    let value = u64::try_from(value.max(0)).unwrap_or(0);
    let extent = u64::from(extent);
    u16::try_from(value.min(extent).saturating_mul(65_535) / extent).unwrap_or(65_535)
}

#[cfg(test)]
mod tests {
    use androscreen_core::protocol::CursorUpdate;

    use super::*;

    #[test]
    fn positions_are_normalized_against_the_display() {
        assert_eq!(normalize(0, 3200), 0);
        assert_eq!(normalize(1600, 3200), 32_767);
        assert_eq!(normalize(3200, 3200), 65_535);
        assert_eq!(normalize(-40, 3200), 0);
        assert_eq!(normalize(9000, 3200), 65_535);
        assert_eq!(normalize(10, 0), 0);
    }

    #[test]
    fn a_shape_is_sent_before_its_position() {
        let mut feed = CursorFeed::new(3200, 2000);
        feed.set_shape(2, 1, 1, 0, vec![0; 8]);
        feed.moved(1600, 1000);

        let shape = feed.next_frame().expect("shape first");
        assert!(matches!(
            shape.as_cursor(),
            Some(CursorUpdate::Shape {
                width: 2,
                height: 1,
                screen_width: 3200,
                screen_height: 2000,
                ..
            })
        ));
        let moved = feed.next_frame().expect("position follows");
        assert_eq!(
            moved.as_cursor(),
            Some(CursorUpdate::Move {
                visible: true,
                x: 32_767,
                y: 32_767
            })
        );
        assert!(feed.next_frame().is_none(), "a still pointer sends nothing");
    }

    #[test]
    fn hiding_is_sent_once() {
        let mut feed = CursorFeed::new(100, 100);
        feed.set_shape(1, 1, 0, 0, vec![0; 4]);
        feed.moved(50, 50);
        assert!(feed.next_frame().is_some());
        assert!(feed.next_frame().is_some());

        feed.hide();
        let hidden = feed.next_frame().expect("hide");
        assert_eq!(
            hidden.as_cursor(),
            Some(CursorUpdate::Move {
                visible: false,
                x: 0,
                y: 0
            })
        );
        assert!(feed.next_frame().is_none());
    }

    #[test]
    fn a_new_screen_size_resends_the_bitmap() {
        let mut feed = CursorFeed::new(3200, 2000);
        feed.set_shape(2, 1, 0, 0, vec![0; 8]);
        assert!(feed.next_frame().is_some());

        feed.set_screen(2000, 3200);
        let shape = feed.next_frame().expect("shape again");
        assert!(matches!(
            shape.as_cursor(),
            Some(CursorUpdate::Shape {
                screen_width: 2000,
                screen_height: 3200,
                ..
            })
        ));
    }
}
