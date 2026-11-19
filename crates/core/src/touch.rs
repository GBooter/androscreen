//! Compact multi-touch reports sent from the phone to the host.
//!
//! Coordinates are normalised to `0..=65535` so the phone never needs to know
//! the virtual display resolution. The host maps them onto the evdi output.

use thiserror::Error;

/// Maximum simultaneous contacts carried in one report.
pub const MAX_POINTS: usize = 10;

/// Encoded size of one contact: id, action, x, y, pressure.
pub const POINT_SIZE: usize = 7;

/// What happened to one contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum TouchAction {
    /// Contact appeared.
    Down = 0,
    /// Contact moved.
    Move = 1,
    /// Contact lifted.
    Up = 2,
    /// Gesture was cancelled by the system.
    Cancel = 3,
}

impl TouchAction {
    /// Decodes the on-wire action byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Down),
            1 => Some(Self::Move),
            2 => Some(Self::Up),
            3 => Some(Self::Cancel),
            _ => None,
        }
    }

    /// The on-wire action byte.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// One contact in a [`TouchFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPoint {
    /// Stable pointer id assigned by Android.
    pub id: u8,
    /// What happened to this contact.
    pub action: TouchAction,
    /// Horizontal position, `0..=65535`.
    pub x: u16,
    /// Vertical position, `0..=65535`.
    pub y: u16,
    /// Pressure, `0..=255`; zero when the device does not measure it.
    pub pressure: u8,
}

impl TouchPoint {
    /// Builds a contact.
    #[must_use]
    pub const fn new(id: u8, action: TouchAction, x: u16, y: u16, pressure: u8) -> Self {
        Self {
            id,
            action,
            x,
            y,
            pressure,
        }
    }
}

/// A validated set of contacts for one moment in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchFrame {
    points: Vec<TouchPoint>,
}

impl TouchFrame {
    /// Validates and builds a report.
    ///
    /// # Errors
    ///
    /// Returns [`TouchError::Empty`], [`TouchError::TooManyPoints`], or
    /// [`TouchError::DuplicateId`] when the contacts cannot describe a real
    /// gesture.
    pub fn new(points: Vec<TouchPoint>) -> Result<Self, TouchError> {
        if points.is_empty() {
            return Err(TouchError::Empty);
        }
        if points.len() > MAX_POINTS {
            return Err(TouchError::TooManyPoints {
                count: points.len(),
            });
        }
        let mut seen = [false; 256];
        for point in &points {
            let slot = usize::from(point.id);
            if seen[slot] {
                return Err(TouchError::DuplicateId { id: point.id });
            }
            seen[slot] = true;
        }
        Ok(Self { points })
    }

    /// The contacts in this report.
    #[must_use]
    pub fn points(&self) -> &[TouchPoint] {
        &self.points
    }

    /// Encodes the report for a [`crate::protocol::Kind::Touch`] frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.points.len() * POINT_SIZE);
        out.push(self.points.len() as u8);
        for point in &self.points {
            out.push(point.id);
            out.push(point.action.as_byte());
            out.extend_from_slice(&point.x.to_le_bytes());
            out.extend_from_slice(&point.y.to_le_bytes());
            out.push(point.pressure);
        }
        out
    }

    /// Decodes and validates a touch payload.
    ///
    /// # Errors
    ///
    /// Returns [`TouchError`] for a wrong length, unknown action byte, empty
    /// report, too many contacts, or duplicate ids.
    pub fn decode(payload: &[u8]) -> Result<Self, TouchError> {
        let Some((&count, body)) = payload.split_first() else {
            return Err(TouchError::LengthMismatch {
                expected: 1,
                actual: 0,
            });
        };
        let count = usize::from(count);
        if count == 0 {
            return Err(TouchError::Empty);
        }
        if count > MAX_POINTS {
            return Err(TouchError::TooManyPoints { count });
        }
        let expected = 1 + count * POINT_SIZE;
        if payload.len() != expected {
            return Err(TouchError::LengthMismatch {
                expected,
                actual: payload.len(),
            });
        }

        let mut points = Vec::with_capacity(count);
        for chunk in body.chunks_exact(POINT_SIZE) {
            let action =
                TouchAction::from_byte(chunk[1]).ok_or(TouchError::UnknownAction(chunk[1]))?;
            points.push(TouchPoint {
                id: chunk[0],
                action,
                x: u16::from_le_bytes([chunk[2], chunk[3]]),
                y: u16::from_le_bytes([chunk[4], chunk[5]]),
                pressure: chunk[6],
            });
        }
        Self::new(points)
    }
}

/// A malformed touch report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum TouchError {
    /// A report must contain at least one contact.
    #[error("touch report is empty")]
    Empty,
    /// More than [`MAX_POINTS`] contacts were present.
    #[error("touch report has {count} contacts, maximum is {MAX_POINTS}")]
    TooManyPoints {
        /// Number of contacts supplied.
        count: usize,
    },
    /// Two contacts shared a pointer id.
    #[error("duplicate touch id {id}")]
    DuplicateId {
        /// The repeated id.
        id: u8,
    },
    /// The action byte is not part of the protocol.
    #[error("unknown touch action {0}")]
    UnknownAction(u8),
    /// The payload length did not match the contact count.
    #[error("touch payload length {actual}, expected {expected}")]
    LengthMismatch {
        /// Bytes required by the count prefix.
        expected: usize,
        /// Bytes supplied.
        actual: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> TouchFrame {
        TouchFrame::new(vec![
            TouchPoint::new(0, TouchAction::Down, 0, 0, 128),
            TouchPoint::new(1, TouchAction::Move, 65_535, 32_768, 255),
        ])
        .expect("valid frame")
    }

    #[test]
    fn round_trip() {
        let original = frame();
        let bytes = original.encode();
        assert_eq!(bytes.len(), 1 + 2 * POINT_SIZE);
        assert_eq!(TouchFrame::decode(&bytes).expect("decodes"), original);
    }

    #[test]
    fn maximum_contacts_round_trip() {
        let points = (0..MAX_POINTS)
            .map(|index| {
                TouchPoint::new(
                    u8::try_from(index).expect("small index"),
                    TouchAction::Move,
                    100,
                    200,
                    50,
                )
            })
            .collect();
        let original = TouchFrame::new(points).expect("valid frame");
        assert_eq!(
            TouchFrame::decode(&original.encode()).expect("decodes"),
            original
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(TouchFrame::new(Vec::new()), Err(TouchError::Empty));
    }

    #[test]
    fn rejects_too_many() {
        let points = (0..=MAX_POINTS)
            .map(|index| {
                TouchPoint::new(
                    u8::try_from(index).expect("small index"),
                    TouchAction::Down,
                    0,
                    0,
                    0,
                )
            })
            .collect();
        assert_eq!(
            TouchFrame::new(points),
            Err(TouchError::TooManyPoints {
                count: MAX_POINTS + 1
            })
        );
    }

    #[test]
    fn rejects_duplicate_ids() {
        assert_eq!(
            TouchFrame::new(vec![
                TouchPoint::new(3, TouchAction::Down, 0, 0, 0),
                TouchPoint::new(3, TouchAction::Move, 1, 1, 0),
            ]),
            Err(TouchError::DuplicateId { id: 3 })
        );
    }

    #[test]
    fn rejects_unknown_action() {
        let mut bytes = frame().encode();
        bytes[2] = 9;
        assert_eq!(
            TouchFrame::decode(&bytes),
            Err(TouchError::UnknownAction(9))
        );
    }

    #[test]
    fn rejects_wrong_length() {
        let bytes = frame().encode();
        assert_eq!(
            TouchFrame::decode(&bytes[..bytes.len() - 1]),
            Err(TouchError::LengthMismatch {
                expected: 1 + 2 * POINT_SIZE,
                actual: bytes.len() - 1,
            })
        );
        assert!(matches!(
            TouchFrame::decode(&[]),
            Err(TouchError::LengthMismatch { .. })
        ));
    }
}
