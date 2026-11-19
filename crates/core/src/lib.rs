//! Wire protocol and shared data model for `AndroScreen`.
//!
//! The crate has no I/O of its own: it encodes and decodes frames, control
//! messages, touch reports, EDID blobs, and `/proc` snapshots so both the host
//! daemon and the Android app can agree on one byte-level contract.

pub mod control;
pub mod edid;
pub mod protocol;
pub mod stats;
pub mod touch;
