//! The sounds a table carries inside its `.vpx`, taken out as plain files.
//!
//! These are the samples a table script plays with `PlaySound`: solenoids,
//! ball rolls, the music of an original table. They have no file of their own
//! to correct in place, so the first step is to get them out untouched, with a
//! manifest the next steps can trust: one file per sound, named after it, and
//! an exact frame count for each.
//!
//! Nothing is re-encoded on the way out. A WAV sound is stored in the table as
//! a bare `WAVEFORMATEX` followed by its samples, and gets a RIFF header rebuilt
//! around those very bytes. Any other format is the file that was imported,
//! written back byte for byte. The extracted file is then decoded once, only to
//! count its frames.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use vpin::vpx::sound::{self as vpx_sound, SoundData};

use crate::decode::Decoder;
use crate::stamp::Stamp;

/// Name of the manifest written beside the extracted sounds.
pub const MANIFEST_FILE: &str = "sounds.json";

/// Layout of the manifest. Bumped whenever a field changes meaning, so a later
/// step can refuse a directory it would misread.
pub const MANIFEST_SCHEMA: u32 = 1;

/// Version of vpin this build reads tables with, as locked in `Cargo.lock`.
pub const VPIN_VERSION: &str = env!("VPIN_VERSION");

/// Longest file stem written, in bytes. Every file system involved stops at
/// 255 bytes for a whole name; this leaves room for a collision suffix and an
/// extension, and a sound name that long is a sentence, not a name.
const MAX_STEM_BYTES: usize = 120;

/// What an extraction produced, written as [`MANIFEST_FILE`] in the folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_SCHEMA`] at the time of writing.
    pub schema: u32,
    /// File name of the table the sounds come from, without its folder.
    pub table: String,
    /// BLAKE3 of the whole `.vpx`, as `blake3:<hex>`. A table that changed
    /// after the extraction no longer matches it.
    pub table_blake3: String,
    /// File format version stored in the table, `1080` for 10.8.0.
    pub vpx_version: u32,
    /// Version of vpin that read the table.
    pub vpin: String,
    /// Version of this tool.
    pub loudness: String,
    /// When the extraction ran, in UTC, RFC 3339.
    pub extracted_at: String,
    /// Every sound of the table, in table order.
    pub sounds: Vec<SoundEntry>,
}

/// One sound of the table, and the file it was written to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoundEntry {
    /// Position in the table, the `N` of its `GameStg/SoundN` stream.
    pub index: usize,
    /// Name the script passes to `PlaySound`.
    pub name: String,
    /// File written for it, in the extraction folder.
    pub file: String,
    /// Path the sound was imported from, as stored in the table.
    pub path: String,
    /// Format of the written file, judged by its content.
    pub format: Format,
    /// `wFormatTag` of the stored `WAVEFORMATEX`: `1` for PCM, `3` for IEEE
    /// float, `2` and `17` for the ADPCMs.
    ///
    /// Present only for a sound stored as samples, whose RIFF header was
    /// rebuilt. A WAV imported under another extension is stored as a file
    /// and comes out as it went in, with `None` here.
    pub wav_format_tag: Option<u16>,
    /// Channel count, from the decoder, or else from the stored header.
    pub channels: Option<u32>,
    /// Frames per second, from the decoder, or else from the stored header.
    pub sample_rate: Option<u32>,
    /// Bits per sample, from the decoder, or else from the stored header.
    /// `None` for formats without one, such as MP3 and Vorbis.
    pub bits_per_sample: Option<u32>,
    /// Frames the stored header accounts for: the sample bytes divided by the
    /// block size. Only for PCM and float samples, where that division holds.
    pub declared_frames: Option<u64>,
    /// Frames the decoder produced, counted over the whole file.
    ///
    /// Exact or absent: when a single packet fails to decode, this is `None`
    /// and [`SoundEntry::decode_error`] says why.
    pub frames: Option<u64>,
    /// `frames` divided by `sample_rate`, in seconds, to the microsecond.
    /// For reading: `frames` is the exact figure.
    pub duration_s: Option<f64>,
    /// Why the file could not be decoded in full. The file is written anyway.
    pub decode_error: Option<String>,
    /// Volume set in the table's sound manager, in percent, -100 to 100.
    ///
    /// Not a gain of its own: vpinball divides it by 100 and adds it to the
    /// volume the script passes to `PlaySound`. The sum is clamped to 0..1, and
    /// the amplitude applied is its square root.
    pub volume: i32,
    /// Left/right pan set in the sound manager, in percent, -100 (left) to 100
    /// (right), added to the pan passed to `PlaySound` the same way.
    pub balance: i32,
    /// Rear/front fade set in the sound manager, in percent, -100 (rear) to
    /// 100 (front), added to the fade passed to `PlaySound`. Only used for
    /// sounds played on the table output.
    pub fade: i32,
    /// Device the sound plays on.
    pub output_target: OutputTarget,
    /// Index of an earlier sound with the same name, ignoring ASCII case.
    ///
    /// vpinball looks a sound up by name and takes the first match, so a sound
    /// shadowed this way is never played by `PlaySound`.
    pub shadowed_by: Option<usize>,
    /// Size of the written file, in bytes.
    pub bytes: u64,
    /// BLAKE3 of the written file, as `blake3:<hex>`.
    pub blake3: String,
}

/// Format of an extracted file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// RIFF WAVE.
    Wav,
    /// MPEG audio layer III.
    Mp3,
    /// Ogg container, Vorbis in practice.
    Ogg,
    /// Free Lossless Audio Codec.
    Flac,
    /// Nothing recognisable: written as `.bin`, under its own name, rather
    /// than under an extension it may not deserve.
    Unknown,
}

impl Format {
    /// Lower case name, as the manifest spells it.
    pub fn name(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
            Format::Flac => "flac",
            Format::Unknown => "unknown",
        }
    }

    /// Extension given to a file of this format.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Unknown => "bin",
            known => known.name(),
        }
    }

    /// Format a file's signature names. The four formats vpinball decodes are
    /// the only ones looked for.
    pub fn from_content(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
            Format::Wav
        } else if bytes.starts_with(b"OggS") {
            Format::Ogg
        } else if bytes.starts_with(b"fLaC") {
            Format::Flac
        } else if bytes.starts_with(b"ID3")
            || (bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0)
        {
            // An ID3 tag, or straight into the sync word of the first frame.
            Format::Mp3
        } else {
            Format::Unknown
        }
    }

    /// Format the extension of an import path names, if any.
    ///
    /// Splits on both separators, whatever the platform: these paths were
    /// written on Windows.
    pub fn from_path(path: &str) -> Option<Self> {
        let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let (stem, extension) = file.rsplit_once('.')?;
        if stem.is_empty() {
            return None;
        }
        match extension.to_ascii_lowercase().as_str() {
            "wav" => Some(Format::Wav),
            "mp3" => Some(Format::Mp3),
            "ogg" => Some(Format::Ogg),
            "flac" => Some(Format::Flac),
            _ => None,
        }
    }
}

/// Audio device a sound plays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputTarget {
    /// The playfield speakers.
    Table,
    /// The backglass speakers, where the music usually goes.
    Backglass,
}

impl From<&vpx_sound::OutputTarget> for OutputTarget {
    fn from(target: &vpx_sound::OutputTarget) -> Self {
        match target {
            vpx_sound::OutputTarget::Backglass => OutputTarget::Backglass,
            // vpinball loads any value it does not know as the table.
            vpx_sound::OutputTarget::Table | vpx_sound::OutputTarget::Other(_) => {
                OutputTarget::Table
            }
        }
    }
}

impl SoundEntry {
    /// Short description of the format, such as `wav pcm 16-bit` or `mp3`.
    pub fn describe_format(&self) -> String {
        let Some(tag) = self.wav_format_tag else {
            return self.format.name().to_string();
        };
        let codec = match tag {
            1 => "pcm".to_string(),
            2 => "adpcm-ms".to_string(),
            3 => "float".to_string(),
            6 => "a-law".to_string(),
            7 => "mu-law".to_string(),
            17 => "adpcm-ima".to_string(),
            other => format!("tag {other:#06x}"),
        };
        match self.bits_per_sample {
            Some(bits) => format!("wav {codec} {bits}-bit"),
            None => format!("wav {codec}"),
        }
    }

    /// Whether the import path names a format the content is not.
    pub fn path_disagrees(&self) -> bool {
        Format::from_path(&self.path).is_some_and(|named| named != self.format)
    }
}

impl Manifest {
    /// Read the manifest of an extraction folder.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the manifest into an extraction folder.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(MANIFEST_FILE);
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// Where a table's sounds go when no folder is named: `<stem>.sounds`, next
/// to the table.
pub fn default_out_dir(table: &Path) -> PathBuf {
    let stem = table.file_stem().unwrap_or(OsStr::new("table"));
    let mut name = stem.to_os_string();
    name.push(".sounds");
    table.with_file_name(name)
}

/// Extract every sound of a table into `out_dir`, and write its manifest.
///
/// A folder that is not empty is refused unless `force` is set. With it, the
/// files a previous manifest lists are removed first, so a sound that left
/// the table does not linger; anything else in the folder is left alone.
///
/// The manifest is written last, so a folder without one is an extraction
/// that did not finish.
pub fn extract(table: &Path, out_dir: &Path, force: bool) -> Result<Manifest> {
    let mut vpx = vpin::vpx::open(table).with_context(|| format!("opening {}", table.display()))?;
    let vpx_version = vpx
        .read_version()
        .with_context(|| format!("reading the version of {}", table.display()))?;
    let sounds = vpx
        .read_sounds()
        .with_context(|| format!("reading the sounds of {}", table.display()))?;
    drop(vpx);

    prepare_out_dir(out_dir, force)?;

    let shadowed = shadowed_by(&sounds);
    let mut names = FileNamer::default();
    let mut entries = Vec::with_capacity(sounds.len());
    for (index, sound) in sounds.iter().enumerate() {
        let written = sound_file(sound);
        let file = names.assign(&sound.name, written.format);
        let path = out_dir.join(&file);
        fs::write(&path, &written.bytes).with_context(|| format!("writing {}", path.display()))?;
        entries.push(describe(
            index,
            sound,
            &written,
            file,
            &path,
            shadowed[index],
        ));
    }

    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        table: table
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        table_blake3: hash_file(table)?,
        vpx_version: vpx_version.u32(),
        vpin: VPIN_VERSION.to_string(),
        loudness: env!("CARGO_PKG_VERSION").to_string(),
        extracted_at: utc_timestamp(Stamp::now()),
        sounds: entries,
    };
    manifest.save(out_dir)?;
    Ok(manifest)
}

/// Make sure `out_dir` exists and may be written into.
fn prepare_out_dir(out_dir: &Path, force: bool) -> Result<()> {
    let mut entries = match fs::read_dir(out_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return fs::create_dir_all(out_dir)
                .with_context(|| format!("creating {}", out_dir.display()));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", out_dir.display())),
    };
    if entries.next().is_none() {
        return Ok(());
    }
    if !force {
        bail!(
            "{} is not empty, pass --force to extract into it anyway",
            out_dir.display()
        );
    }

    // Only what a previous extraction wrote is ours to remove, and only under
    // a bare file name: a manifest is a file anyone can edit.
    if let Ok(previous) = Manifest::load(out_dir) {
        for entry in &previous.sounds {
            if Path::new(&entry.file).file_name() == Some(OsStr::new(&entry.file)) {
                remove_if_present(&out_dir.join(&entry.file))?;
            }
        }
        remove_if_present(&out_dir.join(MANIFEST_FILE))?;
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// A sound as it is written to disk.
struct SoundFile {
    bytes: Vec<u8>,
    format: Format,
    /// Stored in the table as bare samples, so the RIFF header is rebuilt.
    rebuilt: bool,
}

/// The file a sound is written as.
///
/// [`vpx_sound::write_sound`] returns the stored bytes unchanged for a sound
/// stored as a file, and a header followed by those bytes for one stored as
/// samples, which is how the two are told apart.
fn sound_file(sound: &SoundData) -> SoundFile {
    let mut bytes = vpx_sound::write_sound(sound);
    if bytes.len() == sound.data.len() {
        let format = Format::from_content(&bytes);
        return SoundFile {
            bytes,
            format,
            rebuilt: false,
        };
    }
    declare_all_samples(&mut bytes, sound.data.len());
    SoundFile {
        bytes,
        format: Format::Wav,
        rebuilt: true,
    }
}

/// Make the rebuilt header announce every stored sample byte.
///
/// That is what vpinball does when it loads a table (`Sound::CreateFromStream`).
/// vpin agrees for PCM, but for any other format it announces the `cbSize` of
/// the stored `WAVEFORMATEX` instead, a field vpinball always saves as 0: an
/// IEEE float sound would come out as a WAV announcing no audio at all.
fn declare_all_samples(file: &mut [u8], data_len: usize) {
    let data_size = file.len() - data_len - 4;
    file[data_size..data_size + 4].copy_from_slice(&(data_len as u32).to_le_bytes());
    let riff_size = (file.len() - 8) as u32;
    file[4..8].copy_from_slice(&riff_size.to_le_bytes());
}

/// For each sound, the earlier sound `PlaySound` finds first under its name.
fn shadowed_by(sounds: &[SoundData]) -> Vec<Option<usize>> {
    let mut first: HashMap<String, usize> = HashMap::new();
    sounds
        .iter()
        .enumerate()
        .map(|(index, sound)| {
            let key = sound.name.to_ascii_lowercase();
            match first.get(&key) {
                Some(&earlier) => Some(earlier),
                None => {
                    first.insert(key, index);
                    None
                }
            }
        })
        .collect()
}

/// Gather what the manifest says about one written sound.
fn describe(
    index: usize,
    sound: &SoundData,
    written: &SoundFile,
    file: String,
    path: &Path,
    shadowed_by: Option<usize>,
) -> SoundEntry {
    // Only a sound stored as samples carries a header; the one vpin returns
    // for any other sound is a placeholder.
    let header = written.rebuilt.then_some(&sound.wave_form);

    let declared_frames = header
        .filter(|h| matches!(h.format_tag, 1 | 3) && h.block_align > 0)
        .map(|h| (sound.data.len() / h.block_align as usize) as u64);

    // The root cause alone: the rest of the chain repeats the path of the
    // file, which the entry already names, and which means nothing elsewhere.
    let (decoded, decode_error) = match count_frames(path) {
        Ok(decoded) => (Some(decoded), None),
        Err(e) => (None, Some(e.root_cause().to_string())),
    };

    let channels = decoded
        .map(|d| d.channels)
        .or(header.map(|h| u32::from(h.channels)));
    let sample_rate = decoded
        .map(|d| d.sample_rate)
        .or(header.map(|h| h.samples_per_sec));
    let bits_per_sample = decoded
        .and_then(|d| d.bits_per_sample)
        .or(header.map(|h| u32::from(h.bits_per_sample)));
    let frames = decoded.map(|d| d.frames);
    // Rounded so that it reads well and survives a trip through JSON unchanged.
    let duration_s =
        decoded.map(|d| (d.frames as f64 / f64::from(d.sample_rate) * 1e6).round() / 1e6);

    SoundEntry {
        index,
        name: sound.name.clone(),
        file,
        path: sound.path.clone(),
        format: written.format,
        wav_format_tag: header.map(|h| h.format_tag),
        channels,
        sample_rate,
        bits_per_sample,
        declared_frames,
        frames,
        duration_s,
        decode_error,
        // The table stores these as signed 32-bit integers; vpin hands over
        // their bits, which this puts back.
        volume: sound.volume as i32,
        balance: sound.balance as i32,
        fade: sound.fade as i32,
        output_target: OutputTarget::from(&sound.output_target),
        shadowed_by,
        bytes: written.bytes.len() as u64,
        blake3: format!("blake3:{}", blake3::hash(&written.bytes).to_hex()),
    }
}

/// What decoding an extracted file in full gave.
#[derive(Debug, Clone, Copy)]
struct Decoded {
    channels: u32,
    sample_rate: u32,
    bits_per_sample: Option<u32>,
    frames: u64,
}

/// Decode a file to the end and count its frames.
///
/// Fails rather than return a count short of a damaged packet: the manifest
/// promises exact counts or none.
fn count_frames(path: &Path) -> Result<Decoded> {
    let mut decoder = Decoder::open(path)?;
    let spec = decoder.spec();
    let channels = u64::from(spec.channels.max(1));
    let mut frames = 0u64;
    while let Some(block) = decoder.next_block()? {
        frames += block.len() as u64 / channels;
    }
    if decoder.damaged_packets() > 0 {
        bail!(
            "{} packets failed to decode, the frame count would be short",
            decoder.damaged_packets()
        );
    }
    if spec.sample_rate == 0 {
        bail!("sample rate of 0");
    }
    Ok(Decoded {
        channels: spec.channels,
        sample_rate: spec.sample_rate,
        bits_per_sample: decoder.bits_per_sample(),
        frames,
    })
}

/// BLAKE3 of a file, read in a stream: tables run to hundreds of megabytes.
fn hash_file(path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(file)
        .with_context(|| format!("hashing {}", path.display()))?;
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// Hands out file names in table order, unique on any file system.
///
/// Names are compared ignoring case, since Windows and macOS do, and on the
/// stem alone, so that a later step converting everything to one format does
/// not make two of them collide. The first sound keeps its name, which is also
/// the one `PlaySound` reaches; the next ones get `~2`, `~3`, and so on. Same
/// table, same names.
#[derive(Default)]
struct FileNamer {
    taken: HashSet<String>,
}

impl FileNamer {
    /// File name for the next sound of the table.
    fn assign(&mut self, name: &str, format: Format) -> String {
        let stem = sanitize(name);
        let mut candidate = stem.clone();
        let mut n = 1;
        while !self.taken.insert(candidate.to_lowercase()) {
            n += 1;
            candidate = format!("{stem}~{n}");
        }
        format!("{candidate}.{}", format.extension())
    }
}

/// A file stem every file system accepts, as close to `name` as possible.
fn sanitize(name: &str) -> String {
    // Reserved on Windows, and the path separator everywhere else.
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();

    let mut stem = String::with_capacity(replaced.len());
    for c in replaced.chars() {
        if stem.len() + c.len_utf8() > MAX_STEM_BYTES {
            break;
        }
        stem.push(c);
    }

    // Windows drops trailing dots and spaces, so `a.` and `a` would be the same
    // file there. A leading dot hides the file everywhere else.
    let mut stem = stem
        .trim_start_matches(' ')
        .trim_end_matches(['.', ' '])
        .to_string();
    if stem.starts_with('.') {
        stem.replace_range(..1, "_");
    }
    if stem.is_empty() {
        return "sound".to_string();
    }
    if is_reserved_on_windows(&stem) {
        stem.insert(0, '_');
    }
    stem
}

/// Device names Windows will not create a file under, whatever the extension.
fn is_reserved_on_windows(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or(stem).trim_end_matches(' ');
    let upper = base.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let mut chars = upper.chars();
            let prefix: String = chars.by_ref().take(3).collect();
            let rest: String = chars.collect();
            (prefix == "COM" || prefix == "LPT")
                && rest.chars().count() == 1
                && rest
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³'))
        }
    }
}

/// Seconds since the epoch as an RFC 3339 UTC timestamp.
fn utc_timestamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Calendar date of a day count since 1970-01-01, proleptic Gregorian.
///
/// Howard Hinnant's `civil_from_days`, which saves a dependency for the one
/// date this crate writes.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_survive_when_they_can() {
        assert_eq!(sanitize("fx_flipperup"), "fx_flipperup");
        assert_eq!(sanitize("Flapping Wings 3"), "Flapping Wings 3");
        // A dot inside a name is not an extension and stays.
        assert_eq!(sanitize("sfx-penguin-hit1.0"), "sfx-penguin-hit1.0");
        assert_eq!(sanitize("Défi"), "Défi");
    }

    #[test]
    fn names_lose_what_a_file_system_refuses() {
        assert_eq!(sanitize(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("tab\there"), "tab_here");
        assert_eq!(sanitize("  trailing. . "), "trailing");
        assert_eq!(sanitize(".hidden"), "_hidden");
        assert_eq!(sanitize(""), "sound");
        assert_eq!(sanitize("..."), "sound");
    }

    #[test]
    fn windows_device_names_are_dodged() {
        assert_eq!(sanitize("con"), "_con");
        assert_eq!(sanitize("NUL.backup"), "_NUL.backup");
        assert_eq!(sanitize("com1"), "_com1");
        assert_eq!(sanitize("LPT9"), "_LPT9");
        assert_eq!(sanitize("console"), "console");
        assert_eq!(sanitize("com10"), "com10");
    }

    #[test]
    fn long_names_are_cut_on_a_character() {
        let name = "é".repeat(100);
        let stem = sanitize(&name);
        assert!(stem.len() <= MAX_STEM_BYTES);
        assert_eq!(stem, "é".repeat(MAX_STEM_BYTES / 2));
    }

    #[test]
    fn collisions_ignore_case_and_extension() {
        let mut namer = FileNamer::default();
        let names: Vec<String> = [
            ("Sine", Format::Wav),
            ("SINE", Format::Wav),
            ("sine", Format::Mp3),
            ("sine~2", Format::Wav),
            ("a:b", Format::Wav),
            ("a?b", Format::Ogg),
        ]
        .into_iter()
        .map(|(name, format)| namer.assign(name, format))
        .collect();
        assert_eq!(
            names,
            [
                "Sine.wav",
                "SINE~2.wav",
                "sine~3.mp3",
                "sine~2~2.wav",
                "a_b.wav",
                "a_b~2.ogg"
            ]
        );
    }

    #[test]
    fn formats_are_told_by_their_signature() {
        assert_eq!(Format::from_content(b"RIFF\0\0\0\0WAVEfmt "), Format::Wav);
        assert_eq!(Format::from_content(b"RIFF\0\0\0\0AVI "), Format::Unknown);
        assert_eq!(Format::from_content(b"OggS\0"), Format::Ogg);
        assert_eq!(Format::from_content(b"fLaC"), Format::Flac);
        assert_eq!(Format::from_content(b"ID3\x04"), Format::Mp3);
        assert_eq!(Format::from_content(&[0xFF, 0xFB, 0x90]), Format::Mp3);
        assert_eq!(Format::from_content(b""), Format::Unknown);
    }

    #[test]
    fn import_paths_are_read_as_windows_paths() {
        assert_eq!(Format::from_path(r"C:\snd\a.WAV"), Some(Format::Wav));
        assert_eq!(Format::from_path(r"C:\snd.v2\music"), None);
        assert_eq!(Format::from_path("* Backglass Output *"), None);
        assert_eq!(Format::from_path(".ogg"), None);
        assert_eq!(Format::from_path("theme.mp3"), Some(Format::Mp3));
    }

    #[test]
    fn default_folder_sits_next_to_the_table() {
        assert_eq!(
            default_out_dir(Path::new("/t/X-Files 1.2.vpx")),
            Path::new("/t/X-Files 1.2.sounds")
        );
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_790_000_000), "2026-09-21T14:13:20Z");
    }
}
