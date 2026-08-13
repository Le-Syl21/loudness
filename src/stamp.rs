//! The mark left next to a corrected pack.
//!
//! It answers one question at the next launch: is there anything left to do?
//! Rescanning a pack that has not changed, with an engine that would produce
//! the same numbers, under the same target, is pure waste — and a plugin update
//! is not a reason to redo any of it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Name of the mark, dropped beside the pack's own files.
///
/// A file of our own rather than a line in `altsound.ini` or `triggers.pup`:
/// those belong to other programs, which rewrite them on their own terms.
pub const STAMP_FILE: &str = "loudness.json";

/// Everything needed to decide whether the work still holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stamp {
    /// Hash of the pack's media files. Changes when the pack does — and only
    /// then, since the files we write are not part of it.
    pub fingerprint: String,
    /// Behavioural signature of the measuring engine.
    pub engine: String,
    /// Loudness target that was aimed at, in LUFS.
    pub target_lufs: f64,
    /// True peak ceiling that was respected, in dBTP.
    pub ceiling_dbtp: f64,
    /// Measured loudness of the source, in LUFS.
    pub lufs: f64,
    /// Loudness range, in LU.
    pub lra: f64,
    /// Loudest true peak, in dBTP.
    pub true_peak_dbtp: f64,
    /// Offset written into the pack itself, in dB.
    pub written_db: f64,
    /// Offset left for the audio bus, in dB.
    pub residual_db: f64,
    /// Seconds since the epoch, for the humans reading this file.
    pub at: u64,
}

impl Stamp {
    /// Seconds since the epoch, or zero on a machine with a broken clock.
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Read the mark beside a pack, if there is one.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(STAMP_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(&text).ok())
    }

    /// Write the mark beside a pack.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(STAMP_FILE);
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Whether this mark still describes what a fresh run would produce.
    ///
    /// Note what is *not* compared: the tool's own version. Updating a crate,
    /// fixing a message, adding a command — none of that changes a gain, so
    /// none of it costs a rescan.
    pub fn is_current(&self, fingerprint: &str, engine: &str, target: f64, ceiling: f64) -> bool {
        self.fingerprint == fingerprint
            && self.engine == engine
            && (self.target_lufs - target).abs() < 1e-9
            && (self.ceiling_dbtp - ceiling).abs() < 1e-9
    }
}

/// Hash a pack's media files into one identity.
///
/// Built from the sorted list of `(file name, size, content hash)`, so it holds
/// still when the pack is rezipped, moved, or when we rewrite its csv — and it
/// changes as soon as a single sound does. Hashing the bytes rather than the
/// decoded audio is deliberate: a file we cannot decode still gets an identity,
/// which is exactly the one worth reporting.
pub fn fingerprint(files: &[PathBuf]) -> Result<String> {
    let mut entries: Vec<(String, u64, [u8; 32])> = Vec::with_capacity(files.len());

    for path in files {
        let Ok(metadata) = fs::metadata(path) else {
            continue;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let bytes = fs::read(path).with_context(|| format!("hashing {}", path.display()))?;
        entries.push((name, metadata.len(), *blake3::hash(&bytes).as_bytes()));
    }

    entries.sort();

    let mut hasher = blake3::Hasher::new();
    for (name, size, hash) in entries {
        hasher.update(name.as_bytes());
        hasher.update(&size.to_le_bytes());
        hasher.update(&hash);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}
