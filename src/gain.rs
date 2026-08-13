//! Turning a measurement into a gain, without ever touching the dynamics.
//!
//! One constant offset for the whole source. Never a per-sound gain: bringing
//! every sound to the target would put the flipper click at the level of the
//! background music. And never a limiter: when the offset would clip, it is
//! the offset that gives way, not the transients.

/// EBU R128 broadcast target. Low on purpose — the lower the target, the more
/// headroom is left, so fewer sources end up capped. Absolute level is made up
/// once and for all at the cabinet's master volume.
pub const DEFAULT_TARGET_LUFS: f64 = -23.0;

/// EBU R128 recommends true peaks stay at or below this.
pub const DEFAULT_CEILING_DBTP: f64 = -1.0;

/// What we would like to apply, and what we can actually apply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GainPlan {
    /// Offset that would put the source exactly on target, in dB.
    pub wanted_db: f64,
    /// Offset that fits under the true peak ceiling, in dB.
    pub applied_db: f64,
    /// Room left above the loudest true peak, in dB.
    pub headroom_db: f64,
}

impl GainPlan {
    /// The offset had to be reduced to keep the peaks under the ceiling.
    pub fn is_capped(&self) -> bool {
        self.applied_db < self.wanted_db - 0.01
    }

    /// Linear multiplier for the applied offset.
    pub fn linear(&self) -> f64 {
        10.0_f64.powf(self.applied_db / 20.0)
    }
}

/// Work out the offset for a source.
///
/// `source_lufs` is the loudness of everything the source plays, `worst_dbtp`
/// the loudest true peak among its files — a single sound clipping is enough
/// to spoil the result, so the cap is set by the worst one, not by an average.
pub fn plan(source_lufs: f64, worst_dbtp: f64, target_lufs: f64, ceiling_dbtp: f64) -> GainPlan {
    let wanted_db = target_lufs - source_lufs;
    let headroom_db = ceiling_dbtp - worst_dbtp;
    // A source whose peaks already sit above the ceiling has negative headroom,
    // which correctly forces the offset down rather than up.
    let applied_db = wanted_db.min(headroom_db);
    GainPlan { wanted_db, applied_db, headroom_db }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_source_is_raised_when_there_is_room() {
        let p = plan(-31.0, -9.0, DEFAULT_TARGET_LUFS, DEFAULT_CEILING_DBTP);
        assert!((p.wanted_db - 8.0).abs() < 1e-9);
        assert!((p.applied_db - 8.0).abs() < 1e-9);
        assert!(!p.is_capped());
    }

    #[test]
    fn headroom_wins_over_the_target() {
        // Same source, but a transient already close to full scale.
        let p = plan(-31.0, -2.0, DEFAULT_TARGET_LUFS, DEFAULT_CEILING_DBTP);
        assert!((p.wanted_db - 8.0).abs() < 1e-9);
        assert!((p.applied_db - 1.0).abs() < 1e-9);
        assert!(p.is_capped());
    }

    #[test]
    fn loud_source_is_lowered_and_never_capped() {
        let p = plan(-14.0, -0.2, DEFAULT_TARGET_LUFS, DEFAULT_CEILING_DBTP);
        assert!((p.applied_db - -9.0).abs() < 1e-9);
        assert!(!p.is_capped());
    }
}
