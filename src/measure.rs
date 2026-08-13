//! EBU R128 measurement.
//!
//! One measurement per file, and one for the whole source. The per-file true
//! peak is what caps the gain; the source loudness is what the gain aims at.

use std::path::Path;

use anyhow::{Context, Result};
use ebur128::{EbuR128, Mode};

use crate::decode::Decoder;

/// What R128 says about one file, or about a whole set of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// Integrated loudness, gated, in LUFS.
    pub lufs: f64,
    /// Loudness range, in LU. Describes the dynamics; a gain never changes it.
    pub lra: f64,
    /// Highest true peak across channels, in dBTP.
    pub true_peak_dbtp: f64,
    /// Frames measured, so callers can tell an empty file from a silent one.
    pub frames: u64,
}

/// Accumulates R128 state across several files.
///
/// Files are measured individually *and* fed to a shared meter, so the source
/// loudness is the loudness of everything played back to back. That weights a
/// three-minute music track above a two-hundred-millisecond chime, which is
/// what we want: the gain should follow what is heard most.
pub struct SourceMeter {
    meter: Option<EbuR128>,
    spec: Option<(u32, u32)>,
    frames: u64,
    peak_dbtp: f64,
}

const MODE: Mode = Mode::I.union(Mode::LRA).union(Mode::TRUE_PEAK);

impl SourceMeter {
    /// A meter with nothing measured yet.
    pub fn new() -> Self {
        Self { meter: None, spec: None, frames: 0, peak_dbtp: f64::NEG_INFINITY }
    }

    /// Measure one file, add it to the source total, and return its own figures.
    pub fn add_file(&mut self, path: &Path) -> Result<Measurement> {
        let mut decoder = Decoder::open(path)?;
        let spec = decoder.spec();

        let mut single = EbuR128::new(spec.channels, spec.sample_rate, MODE)
            .with_context(|| format!("meter for {}", path.display()))?;

        // The shared meter is fixed to the layout of the first file it sees.
        // A pack that mixes layouts is measured per file only, and the caller
        // is told through `source()` returning None.
        if self.spec.is_none() {
            self.spec = Some((spec.channels, spec.sample_rate));
            self.meter = Some(EbuR128::new(spec.channels, spec.sample_rate, MODE)?);
        }
        let shared_matches = self.spec == Some((spec.channels, spec.sample_rate));
        if !shared_matches {
            self.meter = None;
        }

        let mut frames = 0u64;
        while let Some(block) = decoder.next_block()? {
            if block.is_empty() {
                continue;
            }
            single.add_frames_f32(block)?;
            if let Some(meter) = self.meter.as_mut() {
                meter.add_frames_f32(block)?;
            }
            frames += (block.len() as u64) / (spec.channels as u64);
        }

        let true_peak_dbtp = channel_true_peak(&single, spec.channels)?;
        self.frames += frames;
        if true_peak_dbtp > self.peak_dbtp {
            self.peak_dbtp = true_peak_dbtp;
        }

        Ok(Measurement {
            lufs: single.loudness_global()?,
            lra: single.loudness_range()?,
            true_peak_dbtp,
            frames,
        })
    }

    /// Figures for everything measured so far, or `None` if the files did not
    /// share one layout.
    pub fn source(&self) -> Result<Option<Measurement>> {
        let Some(meter) = self.meter.as_ref() else {
            return Ok(None);
        };
        Ok(Some(Measurement {
            lufs: meter.loudness_global()?,
            lra: meter.loudness_range()?,
            true_peak_dbtp: self.peak_dbtp,
            frames: self.frames,
        }))
    }

    /// Highest true peak seen across every file, in dBTP.
    pub fn worst_true_peak(&self) -> f64 {
        self.peak_dbtp
    }
}

impl Default for SourceMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Loudest true peak across channels, in dBTP. Silence reports -inf.
fn channel_true_peak(meter: &EbuR128, channels: u32) -> Result<f64> {
    let mut worst = 0.0f64;
    for channel in 0..channels {
        let peak = meter.true_peak(channel)?;
        if peak > worst {
            worst = peak;
        }
    }
    Ok(if worst > 0.0 { 20.0 * worst.log10() } else { f64::NEG_INFINITY })
}
