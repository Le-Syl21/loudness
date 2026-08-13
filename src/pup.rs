//! PUP packs: reading the trigger and playlist tables.
//!
//! A pack already carries the field we need. `triggers.pup` holds a `Volume`
//! per played file and `playlists.pup` one per folder, both percentages that
//! the player applies as `volume / 100`. So correcting a pack means writing
//! numbers into columns that already exist, and re-encoding nothing.
//!
//! One quirk matters more than it looks: most triggers sit at `Volume = 0`.
//! Those are the overlays, backglasses and toppers, silent on purpose. A
//! multiplicative gain leaves them alone for free, where an offset in dB would
//! wake every one of them up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One line of `triggers.pup` that plays something.
#[derive(Debug, Clone)]
pub struct Trigger {
    /// Row in the file, so a correction can be written back in place.
    pub row: usize,
    /// Folder the clip lives in, which is also the playlist name.
    pub playlist: String,
    /// File played, relative to the playlist folder. `None` when the trigger
    /// plays the whole folder — which is how most game events are wired, so
    /// ignoring those would miss the loudest part of a pack.
    pub play_file: Option<String>,
    /// Declared volume, as a percentage.
    pub volume: f64,
}

/// One line of `playlists.pup`.
#[derive(Debug, Clone)]
pub struct Playlist {
    /// Row in the file.
    pub row: usize,
    /// Folder name, matching what triggers refer to.
    pub folder: String,
    /// Declared volume, as a percentage.
    pub volume: f64,
}

/// A clip to measure, with the volume the pack already applies to it.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Where the media file is.
    pub path: PathBuf,
    /// Trigger volume, as a percentage.
    pub trigger_volume: f64,
    /// Volume of the folder it belongs to, as a percentage.
    pub playlist_volume: f64,
}

impl Clip {
    /// Combined percentage the player ends up applying.
    pub fn effective_volume(&self) -> f64 {
        self.trigger_volume * self.playlist_volume / 100.0
    }

    /// The same, in dB, so it can be added to a loudness measurement.
    ///
    /// A silent clip has no level at all, hence minus infinity rather than a
    /// very negative number: it must never weigh on a median.
    pub fn volume_db(&self) -> f64 {
        let v = self.effective_volume();
        if v > 0.0 {
            20.0 * (v / 100.0).log10()
        } else {
            f64::NEG_INFINITY
        }
    }
}

/// A parsed PUP pack.
pub struct PupPack {
    /// Folder holding `triggers.pup` and the clip folders.
    pub dir: PathBuf,
    /// Every trigger that names a file to play.
    pub triggers: Vec<Trigger>,
    /// Playlists, by folder name in lowercase.
    pub playlists: HashMap<String, Playlist>,
    /// Every line of `triggers.pup` as it was read, terminator excluded.
    ///
    /// Kept as text rather than as parsed records so a rewrite changes the
    /// bytes we mean to change and nothing else. Round-tripping through a csv
    /// writer would drop the quotes the author put around every description and
    /// turn CRLF into LF — 143 rewritten lines to correct four volumes, on a
    /// file that belongs to someone else.
    trigger_lines: Vec<String>,
    /// Line terminator the file uses, reused verbatim when writing it back.
    trigger_eol: &'static str,
}

/// Column indices, fixed by the PuP Pack Editor format.
mod columns {
    /// `ID,Active,Descript,Trigger,ScreenNum,PlayList,PlayFile,Volume,...`
    pub const TRIGGER_PLAYLIST: usize = 5;
    /// See above.
    pub const TRIGGER_PLAYFILE: usize = 6;
    /// See above.
    pub const TRIGGER_VOLUME: usize = 7;
    /// `ScreenNum,Folder,Des,AlphaSort,RestSeconds,Volume,Priority`
    pub const PLAYLIST_FOLDER: usize = 1;
    /// See above.
    pub const PLAYLIST_VOLUME: usize = 5;
}

/// Ceiling for a written volume.
///
/// The format allows more — packs in the wild use 200 — but a correction that
/// needs a bigger boost than this is telling us the clip is simply too quiet to
/// rescue by gain, and pushing further would only add clipping.
pub const MAX_VOLUME: f64 = 400.0;

/// Read a percentage, defaulting to full scale when the field is empty.
fn volume_of(record: &csv::StringRecord, index: usize) -> f64 {
    record
        .get(index)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(100.0)
}

impl PupPack {
    /// Read `triggers.pup` and `playlists.pup` from a pack folder.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut playlists = HashMap::new();
        let playlists_path = dir.join("playlists.pup");
        if playlists_path.exists() {
            let mut reader = csv::ReaderBuilder::new()
                .flexible(true)
                .from_path(&playlists_path)
                .with_context(|| format!("reading {}", playlists_path.display()))?;
            for (row, record) in reader.records().enumerate() {
                let record = record?;
                let Some(folder) = record.get(columns::PLAYLIST_FOLDER).map(str::trim) else {
                    continue;
                };
                if folder.is_empty() {
                    continue;
                }
                playlists.insert(
                    folder.to_lowercase(),
                    Playlist {
                        row,
                        folder: folder.to_string(),
                        volume: volume_of(&record, columns::PLAYLIST_VOLUME),
                    },
                );
            }
        }

        let triggers_path = dir.join("triggers.pup");
        let mut reader = csv::ReaderBuilder::new()
            .flexible(true)
            .from_path(&triggers_path)
            .with_context(|| format!("reading {}", triggers_path.display()))?;

        let trigger_records = reader.records().collect::<Result<Vec<_>, _>>()?;

        let text = std::fs::read_to_string(&triggers_path)
            .with_context(|| format!("reading {}", triggers_path.display()))?;
        let trigger_eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
        let trigger_lines: Vec<String> = text.split(trigger_eol).map(str::to_string).collect();

        let mut triggers = Vec::new();
        for (row, record) in trigger_records.iter().enumerate() {
            let playlist = record.get(columns::TRIGGER_PLAYLIST).unwrap_or("").trim();
            let play_file = record.get(columns::TRIGGER_PLAYFILE).unwrap_or("").trim();
            // A trigger with no playlist is a comment or a separator.
            if playlist.is_empty() {
                continue;
            }
            triggers.push(Trigger {
                row,
                playlist: playlist.to_string(),
                play_file: (!play_file.is_empty()).then(|| play_file.to_string()),
                volume: volume_of(record, columns::TRIGGER_VOLUME),
            });
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            triggers,
            playlists,
            trigger_lines,
            trigger_eol,
        })
    }

    /// Media files a trigger can play, existing on disk.
    pub fn trigger_files(&self, trigger: &Trigger) -> Vec<PathBuf> {
        let folder = self.dir.join(&trigger.playlist);
        match &trigger.play_file {
            Some(name) => {
                let path = folder.join(name);
                if path.exists() {
                    vec![path]
                } else {
                    Vec::new()
                }
            }
            None => media_files(&folder),
        }
    }

    /// Volume a folder carries, as a percentage.
    pub fn playlist_volume(&self, playlist: &str) -> f64 {
        self.playlists
            .get(&playlist.to_lowercase())
            .map(|p| p.volume)
            .unwrap_or(100.0)
    }

    /// Set the volume of one trigger row.
    ///
    /// The floor is 1, never 0: zero is how a pack says "silent", and turning a
    /// quiet clip into a silent one is not a correction, it is a deletion.
    ///
    /// Returns false when the line could not be edited in place, which is the
    /// signal to leave it alone rather than to rewrite it some other way.
    pub fn set_trigger_volume(&mut self, row: usize, volume: f64) -> bool {
        let clamped = volume.round().clamp(1.0, MAX_VOLUME);
        // Row 0 of the records is line 1 of the file, after the header.
        let Some(line) = self.trigger_lines.get(row + 1) else {
            return false;
        };
        match replace_field(line, columns::TRIGGER_VOLUME, &format!("{clamped:.0}")) {
            Some(edited) => {
                self.trigger_lines[row + 1] = edited;
                true
            }
            None => false,
        }
    }

    /// Write `triggers.pup` back, keeping a `.bak` of the original.
    pub fn save_triggers(&self) -> Result<()> {
        let path = self.dir.join("triggers.pup");
        let backup = self.dir.join("triggers.pup.bak");
        if !backup.exists() {
            std::fs::copy(&path, &backup)
                .with_context(|| format!("backing up to {}", backup.display()))?;
        }

        std::fs::write(&path, self.trigger_lines.join(self.trigger_eol))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Clips worth measuring: those that exist on disk and are not silenced.
    ///
    /// A trigger naming a file yields that file; a trigger naming only a
    /// playlist yields every media file of the folder, since any of them can
    /// be the one that plays.
    pub fn clips(&self) -> Vec<Clip> {
        let mut clips = Vec::new();
        for trigger in &self.triggers {
            let playlist_volume = self
                .playlists
                .get(&trigger.playlist.to_lowercase())
                .map(|p| p.volume)
                .unwrap_or(100.0);
            let folder = self.dir.join(&trigger.playlist);

            let paths: Vec<PathBuf> = match &trigger.play_file {
                Some(name) => vec![folder.join(name)],
                None => media_files(&folder),
            };

            for path in paths {
                if path.exists() {
                    clips.push(Clip {
                        path,
                        trigger_volume: trigger.volume,
                        playlist_volume,
                    });
                }
            }
        }
        clips
    }
}

/// Media files directly inside a folder, sorted so a run is reproducible.
fn media_files(folder: &Path) -> Vec<PathBuf> {
    const EXTENSIONS: [&str; 8] = ["mp4", "mkv", "avi", "webm", "mp3", "wav", "m4a", "ogg"];
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| EXTENSIONS.iter().any(|k| k.eq_ignore_ascii_case(e)))
        })
        .collect();
    files.sort();
    files
}

/// Replace one comma-separated field of a line, leaving every byte around it
/// untouched — quoting, spacing and all.
///
/// Returns `None` if the line has no such field, so the caller can skip it
/// instead of writing something it did not intend.
fn replace_field(line: &str, index: usize, value: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut field = 0;
    let mut start = 0;
    let mut quoted = false;

    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                if field == index {
                    return Some(format!("{}{}{}", &line[..start], value, &line[i..]));
                }
                field += 1;
                start = i + 1;
            }
            _ => {}
        }
    }

    (field == index).then(|| format!("{}{}", &line[..start], value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_replaced_without_touching_the_rest() {
        let line = r#"18,1,"Ball Lost",E100,4,"Ball Lost",,200,20,,,,SkipSamePrty,0"#;
        let edited = replace_field(line, 7, "150").unwrap();
        assert_eq!(
            edited,
            r#"18,1,"Ball Lost",E100,4,"Ball Lost",,150,20,,,,SkipSamePrty,0"#
        );
    }

    #[test]
    fn a_comma_inside_quotes_does_not_shift_the_columns() {
        let line = r#"3,1,"Watch out, they are coming",D0,2,Backglass,"a.mp4",100,1"#;
        let edited = replace_field(line, 7, "42").unwrap();
        assert!(edited.contains(r#""Watch out, they are coming""#));
        assert!(edited.ends_with(",42,1"));
    }

    #[test]
    fn a_missing_field_is_refused_rather_than_invented() {
        assert!(replace_field("1,2,3", 7, "100").is_none());
    }
}
