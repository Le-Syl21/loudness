//! Extraction of a table's sounds, on a table built here with vpin's writer.

use std::fs;
use std::path::{Path, PathBuf};

use loudness::table_sounds::{self, Format, MANIFEST_FILE, Manifest, OutputTarget};
use vpin::vpx::gamedata::GameData;
use vpin::vpx::sound::{self, OutputTarget as VpxOutputTarget, SoundData, WaveForm};
use vpin::vpx::version::Version;
use vpin::vpx::{self, VPX};

/// A fresh folder for one test, under Cargo's target directory.
fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("table_sounds-{test}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Mono 16-bit samples of a 440 Hz sine at half scale.
fn pcm16_sine(frames: usize, rate: u32) -> Vec<u8> {
    (0..frames)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            (0.5 * (std::f64::consts::TAU * 440.0 * t).sin() * f64::from(i16::MAX)) as i16
        })
        .flat_map(i16::to_le_bytes)
        .collect()
}

/// Stereo 32-bit float samples, a quarter-scale ramp per channel.
fn float_stereo(frames: usize) -> Vec<u8> {
    (0..frames)
        .flat_map(|i| {
            let v = 0.25 * (i as f32 / frames as f32);
            [v, -v]
        })
        .flat_map(f32::to_le_bytes)
        .collect()
}

/// A complete 16-bit mono PCM WAV file around `samples`.
fn wav_file(samples: &[u8], rate: u32) -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&rate.to_le_bytes());
    file.extend_from_slice(&(rate * 2).to_le_bytes());
    file.extend_from_slice(&2u16.to_le_bytes());
    file.extend_from_slice(&16u16.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    file.extend_from_slice(samples);
    file
}

fn wave_form(format_tag: u16, channels: u16, rate: u32, bits: u16) -> WaveForm {
    let block_align = channels * bits / 8;
    WaveForm {
        format_tag,
        channels,
        samples_per_sec: rate,
        avg_bytes_per_sec: rate * u32::from(block_align),
        block_align,
        bits_per_sample: bits,
        // What vpinball always saves.
        cb_size: 0,
    }
}

fn sound(name: &str, path: &str, wave_form: WaveForm, data: Vec<u8>) -> SoundData {
    SoundData {
        name: name.to_string(),
        path: path.to_string(),
        wave_form,
        data,
        internal_name: String::new(),
        fade: 0,
        volume: 0,
        balance: 0,
        output_target: VpxOutputTarget::Table,
    }
}

/// The sounds of the fixture table, in table order.
fn fixture_sounds() -> Vec<SoundData> {
    let mut sine = sound(
        "Sine",
        r"C:\Sounds\sine.wav",
        wave_form(1, 1, 22_050, 16),
        pcm16_sine(2205, 22_050),
    );
    // Negative settings, stored as the bits of a signed 32-bit value.
    sine.volume = -50i32 as u32;
    sine.balance = 25;
    sine.fade = -100i32 as u32;

    // Same name but for case: PlaySound never reaches it.
    let shadowed = sound(
        "SINE",
        "sine2.wav",
        wave_form(1, 2, 11_025, 8),
        vec![0x80; 2 * 1000],
    );

    // IEEE float, whose rebuilt header would announce no data at all if it
    // trusted the cbSize vpinball saves.
    let mut float = sound(
        "Float Ding",
        "ding.wav",
        wave_form(3, 2, 48_000, 32),
        float_stereo(2400),
    );
    float.output_target = VpxOutputTarget::Backglass;

    // A WAV imported under an mp3 name: stored as a file, the path lies, and
    // the name is a Windows device.
    let mislabelled = sound(
        "con",
        "jingle.mp3",
        WaveForm::default(),
        wav_file(&pcm16_sine(1000, 8000), 8000),
    );

    // Not audio at all, under a name a file system refuses.
    let garbage = sound(
        "bad:name?",
        "music.ogg",
        WaveForm::default(),
        b"definitely not audio ".repeat(20),
    );

    // A literal name that the collision suffix would also produce.
    let literal = sound(
        "sine~2",
        "literal.wav",
        wave_form(1, 1, 22_050, 16),
        pcm16_sine(100, 22_050),
    );

    vec![sine, shadowed, float, mislabelled, garbage, literal]
}

/// Write the fixture table into `dir`.
fn fixture_table(dir: &Path) -> PathBuf {
    let sounds = fixture_sounds();
    let vpx = VPX {
        version: Version::new(1080),
        gamedata: GameData {
            sounds_size: sounds.len() as u32,
            ..Default::default()
        },
        sounds,
        ..Default::default()
    };

    let path = dir.join("Fixture Table.vpx");
    vpx::write(&path, &vpx).unwrap();
    path
}

#[test]
fn every_sound_comes_out_untouched() {
    let dir = scratch("untouched");
    let table = fixture_table(&dir);
    let out = dir.join("out");
    let manifest = table_sounds::extract(&table, &out, false).unwrap();
    let originals = fixture_sounds();

    let files: Vec<&str> = manifest.sounds.iter().map(|s| s.file.as_str()).collect();
    assert_eq!(
        files,
        [
            "Sine.wav",
            "SINE~2.wav",
            "Float Ding.wav",
            "_con.wav",
            "bad_name_.bin",
            "sine~2~2.wav"
        ]
    );

    for (entry, original) in manifest.sounds.iter().zip(&originals) {
        let bytes = fs::read(out.join(&entry.file)).unwrap();
        assert_eq!(entry.bytes, bytes.len() as u64, "{}", entry.file);
        assert_eq!(
            entry.blake3,
            format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            "{}",
            entry.file
        );

        if entry.wav_format_tag.is_some() {
            // Stored as samples: the very same bytes after a rebuilt header,
            // which vpin reads back into the very same format.
            assert!(bytes.ends_with(&original.data), "{}", entry.file);
            let mut back = sound(&original.name, &original.path, WaveForm::default(), vec![]);
            sound::read_sound(&bytes, &mut back).unwrap();
            assert_eq!(back.data, original.data, "{}", entry.file);
            let (a, b) = (&back.wave_form, &original.wave_form);
            assert_eq!(
                (a.format_tag, a.channels, a.samples_per_sec, a.block_align),
                (b.format_tag, b.channels, b.samples_per_sec, b.block_align),
                "{}",
                entry.file
            );
            assert_eq!(a.bits_per_sample, b.bits_per_sample, "{}", entry.file);
        } else {
            // Stored as a file: written back byte for byte.
            assert_eq!(bytes, original.data, "{}", entry.file);
        }
    }
}

#[test]
fn the_manifest_counts_every_frame() {
    let dir = scratch("frames");
    let table = fixture_table(&dir);
    let out = dir.join("out");
    let manifest = table_sounds::extract(&table, &out, false).unwrap();
    let s = &manifest.sounds;

    let frames: Vec<Option<u64>> = s.iter().map(|e| e.frames).collect();
    assert_eq!(
        frames,
        [
            Some(2205),
            Some(1000),
            Some(2400),
            Some(1000),
            None,
            Some(100)
        ]
    );
    let declared: Vec<Option<u64>> = s.iter().map(|e| e.declared_frames).collect();
    assert_eq!(
        declared,
        [Some(2205), Some(1000), Some(2400), None, None, Some(100)]
    );

    assert_eq!(s[0].format, Format::Wav);
    assert_eq!(s[0].wav_format_tag, Some(1));
    assert_eq!(
        (s[0].channels, s[0].sample_rate, s[0].bits_per_sample),
        (Some(1), Some(22_050), Some(16))
    );
    assert!((s[0].duration_s.unwrap() - 0.1).abs() < 1e-9);
    assert_eq!((s[0].volume, s[0].balance, s[0].fade), (-50, 25, -100));
    assert_eq!(s[0].output_target, OutputTarget::Table);
    assert_eq!(s[0].shadowed_by, None);
    assert_eq!(s[0].describe_format(), "wav pcm 16-bit");

    assert_eq!(s[1].shadowed_by, Some(0));
    assert_eq!(s[1].describe_format(), "wav pcm 8-bit");

    assert_eq!(s[2].wav_format_tag, Some(3));
    assert_eq!((s[2].channels, s[2].sample_rate), (Some(2), Some(48_000)));
    assert_eq!(s[2].output_target, OutputTarget::Backglass);
    assert_eq!(s[2].describe_format(), "wav float 32-bit");

    assert_eq!(s[3].format, Format::Wav);
    assert_eq!(s[3].wav_format_tag, None);
    assert!(s[3].path_disagrees());

    assert_eq!(s[4].format, Format::Unknown);
    let error = s[4].decode_error.as_deref().unwrap();
    assert!(!error.contains(&*out.to_string_lossy()), "{error}");
    assert_eq!((s[4].channels, s[4].duration_s), (None, None));
    // The path names ogg, the content is not.
    assert!(s[4].path_disagrees());

    assert_eq!(s[5].shadowed_by, None);

    assert_eq!(manifest.schema, table_sounds::MANIFEST_SCHEMA);
    assert_eq!(manifest.table, "Fixture Table.vpx");
    assert_eq!(manifest.vpx_version, 1080);
    assert_eq!(manifest.vpin, table_sounds::VPIN_VERSION);
    assert_ne!(manifest.vpin, "unknown");
    assert_eq!(
        manifest.table_blake3,
        format!(
            "blake3:{}",
            blake3::hash(&fs::read(&table).unwrap()).to_hex()
        )
    );
    assert_eq!(Manifest::load(&out).unwrap(), manifest);
}

#[test]
fn a_folder_in_use_takes_force() {
    let dir = scratch("force");
    let table = fixture_table(&dir);
    let out = dir.join("out");
    table_sounds::extract(&table, &out, false).unwrap();

    let error = table_sounds::extract(&table, &out, false).unwrap_err();
    assert!(error.to_string().contains("--force"), "{error:#}");

    // A file from an extraction of an older version of the table, and one
    // that has nothing to do with us.
    let mut manifest = Manifest::load(&out).unwrap();
    manifest.sounds[0].file = "gone.wav".to_string();
    manifest.save(&out).unwrap();
    fs::write(out.join("gone.wav"), b"stale").unwrap();
    fs::write(out.join("notes.txt"), b"mine").unwrap();

    let again = table_sounds::extract(&table, &out, true).unwrap();
    assert!(!out.join("gone.wav").exists());
    assert!(out.join("notes.txt").exists());
    assert_eq!(again.sounds.len(), 6);
    assert!(out.join(MANIFEST_FILE).exists());
}
