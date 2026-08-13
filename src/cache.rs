//! Measurement cache.
//!
//! Deliberately not an "already normalised" flag: a boolean forces a remeasure
//! the day the target moves, and says nothing when a pack is updated behind
//! our back. Storing the figures lets us tell what is still valid.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Bumped whenever the measurement or the gain rules change, so old entries
/// are recomputed instead of silently trusted.
pub const ALGORITHM_VERSION: u32 = 1;

/// What was measured for one source, and what was done about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Rom name, pack folder — whatever identifies the source.
    pub source_id: String,
    /// `altsound`, `pup`, `pinmame`, `table`.
    pub kind: String,
    /// Integrated loudness measured, in LUFS.
    pub lufs: f64,
    /// Loudness range, in LU.
    pub lra: f64,
    /// Loudest true peak, in dBTP.
    pub true_peak_dbtp: f64,
    /// Target that was aimed at, in LUFS.
    pub target_lufs: f64,
    /// True peak ceiling that was respected, in dBTP.
    pub ceiling_dbtp: f64,
    /// Offset written into the source itself, in dB.
    pub written_db: f64,
    /// Offset left for the audio bus, in dB.
    pub residual_db: f64,
    /// Files measured, to notice a pack that has grown or shrunk.
    pub file_count: usize,
    /// Rules that produced these numbers.
    pub algorithm_version: u32,
}

/// Every measurement we have kept.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    /// One entry per measured source.
    pub entries: Vec<CacheEntry>,
}

impl Cache {
    /// Read the cache, or start an empty one if the file does not exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Write the cache out, creating the parent folder if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Entry for a source, if it was measured under the current rules.
    pub fn get(&self, source_id: &str) -> Option<&CacheEntry> {
        self.entries
            .iter()
            .find(|e| e.source_id == source_id && e.algorithm_version == ALGORITHM_VERSION)
    }

    /// Add an entry, replacing any previous one for the same source.
    pub fn put(&mut self, entry: CacheEntry) {
        self.entries.retain(|e| e.source_id != entry.source_id);
        self.entries.push(entry);
    }
}
