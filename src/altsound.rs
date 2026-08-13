//! AltSound packs: measure the wavs, write the gain back into the csv.
//!
//! Nothing is re-encoded. The AltSound csv already carries a `GAIN` column per
//! entry, so normalising a pack is a matter of scaling every one of them by the
//! same factor — same factor, so the mix the pack author intended survives.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// A parsed AltSound csv, kept whole so unknown columns survive a rewrite.
pub struct AltsoundPack {
    path: PathBuf,
    headers: csv::StringRecord,
    records: Vec<csv::StringRecord>,
    gain_column: usize,
    fname_column: usize,
}

/// What applying a gain did to the pack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ApplyReport {
    /// Entries whose gain was rewritten.
    pub adjusted: usize,
    /// Offset actually written into the csv, in dB.
    pub written_db: f64,
    /// Offset that the pack cannot deliver, in dB.
    ///
    /// Not a remainder to be applied elsewhere: there is nowhere else. The
    /// column stops at 100, `group_vol` stops at 100, and both mean "the sample
    /// at its own level", never a boost. The only other gain in the chain is
    /// `AudioSource.AltSound.Gain`, which belongs to the user's mixer and is not
    /// ours to write. So a pack that needs more than this is a pack whose level
    /// is simply the ceiling of the whole system.
    pub unreachable_db: f64,
    /// Loudest this pack can ever play, in LUFS.
    ///
    /// The number that matters when choosing a target: no source can be aligned
    /// above the quietest of these.
    pub max_reachable_lufs: f64,
}

/// Upper bound of the AltSound gain column.
///
/// `100` means "unattenuated", not "+6 dB": libaltsound reads the field as
/// `val > 100 ? 1.0 : val / 100`, so an AltSound pack can only ever come down
/// from the level its samples were recorded at.
const MAX_GAIN: f64 = 100.0;

impl AltsoundPack {
    /// Read `altsound-<game>.csv` (or any csv with `GAIN` and `FNAME` columns).
    pub fn load(path: &Path) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_path(path)
            .with_context(|| format!("reading {}", path.display()))?;

        let headers = reader.headers()?.clone();
        let column = |name: &str| {
            headers
                .iter()
                .position(|h| h.trim().eq_ignore_ascii_case(name))
                .with_context(|| format!("{} has no {name} column", path.display()))
        };
        let gain_column = column("GAIN")?;
        let fname_column = column("FNAME")?;

        let records = reader.records().collect::<Result<Vec<_>, _>>()?;
        if records.is_empty() {
            bail!("{} lists no sound", path.display());
        }

        Ok(Self {
            path: path.to_path_buf(),
            headers,
            records,
            gain_column,
            fname_column,
        })
    }

    /// Paths of the wavs the pack refers to, in csv order.
    pub fn sound_files(&self) -> Vec<PathBuf> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        self.records
            .iter()
            .filter_map(|r| r.get(self.fname_column))
            .map(|name| dir.join(name.trim()))
            .collect()
    }

    /// Scale every gain by the same factor, and report what did not fit.
    ///
    /// The factor is reduced until the loudest entry still fits in the column,
    /// so all the ratios between sounds are preserved. `source_lufs` is only
    /// used to report what the pack can reach at most.
    pub fn apply_gain(&mut self, factor: f64, source_lufs: f64) -> Result<ApplyReport> {
        let mut gains = Vec::with_capacity(self.records.len());
        for record in &self.records {
            let raw = record.get(self.gain_column).unwrap_or("").trim();
            let gain: f64 = raw
                .parse()
                .with_context(|| format!("gain {raw:?} is not a number"))?;
            gains.push(gain);
        }

        let loudest = gains.iter().copied().fold(0.0_f64, f64::max);
        let ceiling_factor = if loudest > 0.0 {
            MAX_GAIN / loudest
        } else {
            f64::INFINITY
        };
        let effective = factor.min(ceiling_factor);

        for (record, gain) in self.records.iter_mut().zip(&gains) {
            let scaled = (gain * effective).round().clamp(0.0, MAX_GAIN);
            let mut fields: Vec<String> = record.iter().map(str::to_string).collect();
            fields[self.gain_column] = format!("{scaled:.0}");
            *record = csv::StringRecord::from(fields);
        }

        Ok(ApplyReport {
            adjusted: self.records.len(),
            written_db: 20.0 * effective.log10(),
            unreachable_db: 20.0 * (factor / effective).log10(),
            max_reachable_lufs: source_lufs + 20.0 * ceiling_factor.log10(),
        })
    }

    /// Write the csv back, keeping a `.bak` of what was there before.
    pub fn save(&self) -> Result<()> {
        let backup = self.path.with_extension("csv.bak");
        if !backup.exists() {
            fs::copy(&self.path, &backup)
                .with_context(|| format!("backing up to {}", backup.display()))?;
        }

        let mut writer = csv::Writer::from_path(&self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        writer.write_record(&self.headers)?;
        for record in &self.records {
            writer.write_record(record)?;
        }
        writer.flush()?;
        Ok(())
    }
}
