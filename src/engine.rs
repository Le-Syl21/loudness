//! Identifying the measuring engine by what it does, not by its version.
//!
//! A pack must not be re-measured every time the tool is rebuilt. What matters
//! is whether the numbers would come out the same, and a crate version cannot
//! answer that: a patch release of the meter may change nothing at all, or may
//! change the third decimal of every measurement.
//!
//! So the engine signs itself: it measures a synthetic reference signal and
//! hashes the result. Same signature, same numbers, no reason to rescan —
//! whatever moved underneath.

use std::sync::OnceLock;

use anyhow::Result;
use ebur128::EbuR128;

use crate::measure::MODE;

/// Version of the gain rules — the target, the ceiling handling, the way the
/// offset is split between the pack and the bus. Bump when a rule changes.
pub const RULES_VERSION: u32 = 1;

const REFERENCE_RATE: u32 = 48_000;
const REFERENCE_CHANNELS: u32 = 2;

/// A reference signal built to exercise everything the measurement uses.
///
/// Five seconds at 48 kHz: a tone that the K-weighting has to weigh, a second
/// tone low enough to be weighed differently, a silent stretch so the gating
/// has something to drop, and a short transient that only true peak detection
/// sees, since it peaks between two samples.
fn reference_signal() -> Vec<f32> {
    let frames = (REFERENCE_RATE * 5) as usize;
    let mut samples = Vec::with_capacity(frames * REFERENCE_CHANNELS as usize);

    for frame in 0..frames {
        let t = frame as f64 / REFERENCE_RATE as f64;
        let (left, right) = if (2.0..3.0).contains(&t) {
            // Below the absolute gate, so these blocks must be dropped.
            (0.0, 0.0)
        } else if (4.0..4.01).contains(&t) {
            // Alternating full scale: the sample peaks read 0 dBFS while the
            // reconstructed waveform overshoots, which is the whole point of
            // measuring true peak rather than sample peak.
            let sign = if frame % 2 == 0 { 1.0 } else { -1.0 };
            (0.98 * sign, 0.98 * sign)
        } else {
            (
                0.5 * (std::f64::consts::TAU * 1000.0 * t).sin(),
                0.25 * (std::f64::consts::TAU * 120.0 * t).sin(),
            )
        };
        samples.push(left as f32);
        samples.push(right as f32);
    }
    samples
}

/// Measure the reference signal and hash the outcome.
fn compute_signature() -> Result<String> {
    let mut meter = EbuR128::new(REFERENCE_CHANNELS, REFERENCE_RATE, MODE)?;
    meter.add_frames_f32(&reference_signal())?;

    // Rounded to two decimals on purpose: a change that does not move the
    // numbers by a hundredth of a dB would not move a gain either, and should
    // not cost every user a full rescan.
    let mut hasher = blake3::Hasher::new();
    for value in [
        meter.loudness_global()?,
        meter.loudness_range()?,
        meter.true_peak(0)?,
        meter.true_peak(1)?,
    ] {
        hasher.update(format!("{value:.2};").as_bytes());
    }
    hasher.update(format!("rules={RULES_VERSION}").as_bytes());

    Ok(format!("blake3:{}", &hasher.finalize().to_hex()[..16]))
}

/// Signature of the measuring engine, computed once per run.
///
/// Falls back to a version-derived string if the reference measurement itself
/// fails, which would mean the meter is unusable anyway.
pub fn signature() -> &'static str {
    static SIGNATURE: OnceLock<String> = OnceLock::new();
    SIGNATURE.get_or_init(|| {
        compute_signature().unwrap_or_else(|_| format!("unavailable:rules={RULES_VERSION}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_stable_within_a_run() {
        assert_eq!(signature(), signature());
        assert!(signature().starts_with("blake3:"));
    }

    #[test]
    fn the_reference_signal_exercises_gating_and_true_peak() {
        let mut meter = EbuR128::new(REFERENCE_CHANNELS, REFERENCE_RATE, MODE).unwrap();
        meter.add_frames_f32(&reference_signal()).unwrap();
        // Something was measured, the silence did not swallow it,
        assert!(meter.loudness_global().unwrap() > -40.0);
        // and the inter-sample overshoot pushed true peak above full scale.
        assert!(meter.true_peak(0).unwrap() > 1.0);
    }
}
