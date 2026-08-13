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

        let mut triggers = Vec::new();
        for (row, record) in reader.records().enumerate() {
            let record = record?;
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
                volume: volume_of(&record, columns::TRIGGER_VOLUME),
            });
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            triggers,
            playlists,
        })
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
