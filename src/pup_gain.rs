//! Deciding what a PUP pack's volumes should become.
//!
//! Unlike a rom or an AltSound pack, a PUP pack gets corrected *inside*: its
//! clips come from unrelated sources and were never mixed together, so the
//! spread between them is nobody's intention. But the correction stays bounded
//! — clips are pulled back to the edge of a band around the median, not onto
//! the median. A clip the author wanted loud stays the loudest, just less
//! extreme.

use crate::pup_measure::TriggerLevel;

/// A volume change to write into `triggers.pup`.
#[derive(Debug, Clone)]
pub struct VolumeChange {
    /// Row to edit.
    pub row: usize,
    /// What the row plays, for reporting.
    pub label: String,
    /// Volume currently written on the row, as a percentage.
    pub from: f64,
    /// Volume to write, as a percentage.
    pub to: f64,
    /// How far the clip was from the median before, in LU.
    pub was_off_by: f64,
    /// Correction that could not be applied because it would clip, in dB.
    pub refused_db: f64,
}

/// Work out the volume changes that bring a pack's outliers back into the band.
///
/// `ceiling_dbtp` caps every boost: a clip already peaking near full scale
/// cannot be raised, whatever the median says. Quieter is always allowed —
/// nothing clips on the way down.
pub fn plan(
    levels: &[TriggerLevel],
    median: f64,
    band: f64,
    ceiling_dbtp: f64,
    max_volume: f64,
) -> Vec<VolumeChange> {
    let mut changes = Vec::new();

    for level in levels {
        let effective = level.effective_lufs();
        let distance = effective - median;
        if distance.abs() <= band {
            continue;
        }

        // Pull back to the edge of the band, not to the middle of it.
        let target = if distance > 0.0 {
            median + band
        } else {
            median - band
        };
        let wanted_db = target - effective;

        // Headroom is measured on the clip as it is played today, so the volume
        // already applied counts towards the peak.
        let played_peak = level.true_peak_dbtp + 20.0 * (level.volume / 100.0).log10();
        let headroom_db = ceiling_dbtp - played_peak;
        // And the column itself has a ceiling, which has to be honoured here
        // rather than silently at write time — otherwise the report announces a
        // correction that never happens.
        let column_db = 20.0 * (max_volume / level.trigger_volume.max(1.0)).log10();
        let applied_db = wanted_db.min(headroom_db.max(0.0)).min(column_db.max(0.0));

        if applied_db.abs() < 0.1 {
            continue;
        }

        changes.push(VolumeChange {
            row: level.row,
            label: level.label.clone(),
            from: level.trigger_volume,
            to: (level.trigger_volume * 10.0_f64.powf(applied_db / 20.0)).round(),
            was_off_by: distance,
            refused_db: wanted_db - applied_db,
        });
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(row: usize, lufs: f64, volume: f64, peak: f64) -> TriggerLevel {
        TriggerLevel {
            row,
            label: format!("clip{row}"),
            files: 1,
            lufs,
            true_peak_dbtp: peak,
            volume,
            trigger_volume: volume,
        }
    }

    #[test]
    fn clips_inside_the_band_are_left_alone() {
        let levels = [level(0, -20.0, 100.0, -6.0), level(1, -24.0, 100.0, -6.0)];
        assert!(plan(&levels, -22.0, 6.0, -1.0, 400.0).is_empty());
    }

    #[test]
    fn a_loud_outlier_is_pulled_back_to_the_edge_not_to_the_median() {
        // 12 LU above a -22 median, band of 6: it should come down by 6, not 12.
        let levels = [level(0, -10.0, 100.0, -20.0)];
        let changes = plan(&levels, -22.0, 6.0, -1.0, 400.0);
        assert_eq!(changes.len(), 1);
        // Rounded to a whole percent, as the file stores it.
        assert!((20.0 * (changes[0].to / changes[0].from).log10() + 6.0).abs() < 0.05);
    }

    #[test]
    fn a_boost_that_would_clip_is_refused_not_applied() {
        // Quiet in loudness but already peaking at -1 dBTP: no room to raise it.
        let levels = [level(0, -34.0, 100.0, -1.0)];
        let changes = plan(&levels, -22.0, 6.0, -1.0, 400.0);
        assert!(changes.is_empty());
    }

    #[test]
    fn a_boost_is_capped_by_the_column_ceiling() {
        // Far too quiet and with headroom to spare, but the column stops at 400.
        let levels = [level(0, -60.0, 200.0, -40.0)];
        let changes = plan(&levels, -22.0, 6.0, -1.0, 400.0);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].to, 400.0);
        assert!(changes[0].refused_db > 0.0);
    }

    #[test]
    fn a_boost_is_capped_by_the_headroom_that_exists() {
        // 12 LU too quiet, but only 3 dB of headroom before the ceiling.
        let levels = [level(0, -34.0, 100.0, -4.0)];
        let changes = plan(&levels, -22.0, 6.0, -1.0, 400.0);
        assert_eq!(changes.len(), 1);
        assert!((20.0 * (changes[0].to / changes[0].from).log10() - 3.0).abs() < 0.05);
        assert!((changes[0].refused_db - 3.0).abs() < 0.05);
    }
}
