//! Measuring a PUP pack, trigger by trigger.
//!
//! A correction is written per row of `triggers.pup`, so that is the unit that
//! has to be measured — not the file. The same clip can be played by two rows
//! at two different volumes, and a row that names only a playlist can play any
//! file of a folder.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::decode::NoAudioTrack;
use crate::measure::{Measurement, SourceMeter};
use crate::pup::PupPack;

/// One row of `triggers.pup`, measured.
#[derive(Debug, Clone)]
pub struct TriggerLevel {
    /// Row in `triggers.pup`.
    pub row: usize,
    /// What the row plays, for reporting.
    pub label: String,
    /// Files it can play.
    pub files: usize,
    /// Loudness of what it plays, before the pack's own volume, in LUFS.
    ///
    /// For a row naming a file, that file. For a row naming a playlist, the
    /// median of the folder — a random pick has to be judged on its middle,
    /// not on its loudest or quietest member.
    pub lufs: f64,
    /// Worst true peak among those files, in dBTP.
    pub true_peak_dbtp: f64,
    /// Volume the pack applies, as a percentage, trigger times playlist.
    pub volume: f64,
    /// Volume written on the trigger row itself, as a percentage.
    pub trigger_volume: f64,
}

impl TriggerLevel {
    /// Loudness as actually heard, once the pack's volume is applied.
    pub fn effective_lufs(&self) -> f64 {
        self.lufs + 20.0 * (self.volume / 100.0).log10()
    }
}

/// What was skipped, and why.
///
/// Triggers and files are counted apart on purpose: mixing them gives totals
/// that do not add up against the number of rows in the file.
#[derive(Debug, Default, Clone, Copy)]
pub struct SkipCounts {
    /// Trigger rows sitting at volume zero — muted by the pack author.
    pub muted_triggers: usize,
    /// Trigger rows whose files could none of them be measured.
    pub empty_triggers: usize,
    /// Files holding nothing but silence.
    pub silent_files: usize,
    /// Files with no audio track at all — normal for a decorative video.
    pub no_audio: usize,
    /// Files that failed to decode, which is the only kind worth reporting.
    pub unreadable: usize,
}

/// Measure every trigger of a pack.
///
/// Each file is decoded once even when several rows play it.
pub fn measure(pack: &PupPack) -> Result<(Vec<TriggerLevel>, SkipCounts)> {
    let mut cache: HashMap<PathBuf, Option<Measurement>> = HashMap::new();
    let mut skipped = SkipCounts::default();
    let mut levels = Vec::new();

    for trigger in &pack.triggers {
        let volume = trigger.volume * pack.playlist_volume(&trigger.playlist) / 100.0;
        if volume <= 0.0 {
            skipped.muted_triggers += 1;
            continue;
        }

        let files = pack.trigger_files(trigger);
        let mut lufs = Vec::new();
        let mut worst_peak = f64::NEG_INFINITY;

        for path in &files {
            let entry = match cache.get(path) {
                Some(entry) => *entry,
                None => {
                    let mut meter = SourceMeter::new();
                    let measured = match meter.add_file(path) {
                        Ok(m) if m.lufs.is_finite() => Some(m),
                        Ok(_) => {
                            skipped.silent_files += 1;
                            None
                        }
                        Err(e) if e.downcast_ref::<NoAudioTrack>().is_some() => {
                            skipped.no_audio += 1;
                            None
                        }
                        Err(e) => {
                            skipped.unreadable += 1;
                            eprintln!("  unreadable {}: {e:#}", path.display());
                            None
                        }
                    };
                    cache.insert(path.clone(), measured);
                    measured
                }
            };

            if let Some(m) = entry {
                lufs.push(m.lufs);
                if m.true_peak_dbtp > worst_peak {
                    worst_peak = m.true_peak_dbtp;
                }
            }
        }

        if lufs.is_empty() {
            skipped.empty_triggers += 1;
            continue;
        }
        lufs.sort_by(f64::total_cmp);

        levels.push(TriggerLevel {
            row: trigger.row,
            label: trigger
                .play_file
                .clone()
                .unwrap_or_else(|| format!("{}/*", trigger.playlist)),
            files: lufs.len(),
            lufs: lufs[lufs.len() / 2],
            true_peak_dbtp: worst_peak,
            volume,
            trigger_volume: trigger.volume,
        });
    }

    Ok((levels, skipped))
}

/// Median of a set of levels. The slice must not be empty.
pub fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
