//! Audio decoding.
//!
//! Symphonia is pure Rust and covers what the pinball world uses: wav and adpcm
//! for AltSound packs and table samples, mp4/aac for PUP pack videos, mp3, ogg
//! and flac for the music folder.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// What a decoder reports about the stream it produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioSpec {
    /// Frames per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u32,
}

/// Decodes a media file into interleaved `f32` frames, which is what the
/// loudness meter eats.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    spec: AudioSpec,
    samples: Vec<f32>,
}

impl Decoder {
    /// Open a media file and read enough of it to know its layout.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }

        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .with_context(|| format!("probing {}", path.display()))?;

        let track = format
            .first_track(TrackType::Audio)
            .with_context(|| format!("{} has no audio track", path.display()))?;
        let track_id = track.id;

        let Some(CodecParameters::Audio(params)) = track.codec_params.as_ref() else {
            bail!("{} has no audio codec parameters", path.display());
        };
        let sample_rate = params.sample_rate.context("unknown sample rate")?;
        let channels = params
            .channels
            .as_ref()
            .context("unknown channel layout")?
            .count() as u32;

        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .with_context(|| format!("no decoder for {}", path.display()))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            spec: AudioSpec {
                sample_rate,
                channels,
            },
            samples: Vec::new(),
        })
    }

    /// The stream layout.
    pub fn spec(&self) -> AudioSpec {
        self.spec
    }

    /// Next block of interleaved samples, or `None` at end of stream.
    pub fn next_block(&mut self) -> Result<Option<&[f32]>> {
        loop {
            let Some(packet) = self.format.next_packet()? else {
                return Ok(None);
            };

            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    decoded.copy_to_vec_interleaved(&mut self.samples);
                    return Ok(Some(&self.samples));
                }
                // A damaged packet is worth skipping, not worth failing on: one
                // bad frame in a 500-file pack should not lose the measurement.
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
