//! Loudness normalization for Visual Pinball.
//!
//! Every audio source of a cabinet — the PinMAME rom, an AltSound pack, a PUP
//! pack, the table's own samples — is measured to EBU R128 once, and corrected
//! with a single constant gain. A constant offset leaves every ratio inside the
//! source untouched, so the flipper click keeps its bite; that is the whole
//! reason for measuring loudness rather than peaks.
//!
//! Where the gain lands depends on the source: the `GAIN` column for AltSound,
//! the `Volume` columns of a PUP pack, and `AudioSource.<id>.Gain` in the VPX
//! settings for everything that cannot be corrected in place.

pub mod altsound;
pub mod cache;
pub mod decode;
pub mod engine;
pub mod gain;
pub mod measure;
pub mod pup;
pub mod stamp;
