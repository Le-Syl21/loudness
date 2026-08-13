//! Command line front end: measure an AltSound pack, and optionally correct it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use loudness::altsound::AltsoundPack;
use loudness::cache::{ALGORITHM_VERSION, Cache, CacheEntry};
use loudness::decode::NoAudioTrack;
use loudness::engine;
use loudness::gain::{DEFAULT_CEILING_DBTP, DEFAULT_TARGET_LUFS, plan};
use loudness::measure::SourceMeter;
use loudness::pup::PupPack;
use loudness::stamp::{self, Stamp};

#[derive(Parser)]
#[command(version, about = "EBU R128 loudness normalization for Visual Pinball")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure an AltSound pack and report the gain it needs.
    Scan {
        /// Path to altsound-<game>.csv, or the folder holding it.
        path: PathBuf,
        /// Loudness target, in LUFS.
        #[arg(long, default_value_t = DEFAULT_TARGET_LUFS)]
        target: f64,
        /// True peak ceiling, in dBTP.
        #[arg(long, default_value_t = DEFAULT_CEILING_DBTP)]
        ceiling: f64,
        /// Also list every sound measured.
        #[arg(long)]
        verbose: bool,
    },
    /// Measure a pack and write the gain into its csv.
    Apply {
        /// Path to altsound-<game>.csv, or the folder holding it.
        path: PathBuf,
        /// Loudness target, in LUFS.
        #[arg(long, default_value_t = DEFAULT_TARGET_LUFS)]
        target: f64,
        /// True peak ceiling, in dBTP.
        #[arg(long, default_value_t = DEFAULT_CEILING_DBTP)]
        ceiling: f64,
        /// Where to keep the measurements.
        #[arg(long, default_value = "loudness-cache.json")]
        cache: PathBuf,
        /// Correct the pack even though some of its sounds are missing.
        #[arg(long)]
        allow_missing: bool,
        /// Measure and write again even if nothing has changed.
        #[arg(long)]
        force: bool,
    },
    /// Measure a PUP pack and report how spread out its clips are. Reads only.
    PupScan {
        /// Folder holding triggers.pup.
        path: PathBuf,
        /// Half-width of the band around the median, in LU, outside of which a
        /// clip is called an outlier.
        #[arg(long, default_value_t = 6.0)]
        band: f64,
        /// List every clip measured.
        #[arg(long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Scan {
            path,
            target,
            ceiling,
            verbose,
        } => scan(&path, target, ceiling, verbose),
        Command::Apply {
            path,
            target,
            ceiling,
            cache,
            allow_missing,
            force,
        } => apply(&path, target, ceiling, &cache, allow_missing, force),
        Command::PupScan {
            path,
            band,
            verbose,
        } => pup_scan(&path, band, verbose),
    }
}

/// Measure every clip of a PUP pack and describe the spread.
///
/// The number that matters is not a clip's own loudness but the loudness it
/// reaches once the pack's own `Volume` is applied — that is what the player
/// hears, and what an author has already tuned by hand.
fn pup_scan(dir: &Path, band: f64, verbose: bool) -> Result<()> {
    let pack = PupPack::load(dir)?;
    let clips = pack.clips();
    println!(
        "{}: {} triggers, {} clips on disk, {} playlists",
        dir.display(),
        pack.triggers.len(),
        clips.len(),
        pack.playlists.len()
    );

    let mut seen: HashMap<PathBuf, f64> = HashMap::new();
    let mut measured: Vec<(PathBuf, f64, f64, f64)> = Vec::new();
    let mut silent = 0;
    let mut no_audio = 0;
    let mut unreadable = 0;

    for clip in &clips {
        if clip.effective_volume() <= 0.0 {
            silent += 1;
            continue;
        }
        if let Some(previous) = seen.get(&clip.path) {
            if (*previous - clip.effective_volume()).abs() > 0.01 {
                println!(
                    "  note: {} is played at {:.0}% and at {:.0}%",
                    clip.path.file_name().unwrap_or_default().to_string_lossy(),
                    previous,
                    clip.effective_volume()
                );
            }
            continue;
        }
        seen.insert(clip.path.clone(), clip.effective_volume());

        let mut meter = SourceMeter::new();
        match meter.add_file(&clip.path) {
            // A clip whose audio track holds nothing but silence measures as
            // minus infinity. It carries no level, so it belongs with the
            // silent ones rather than dragging the median down.
            Ok(m) if !m.lufs.is_finite() => silent += 1,
            Ok(m) => {
                let effective = m.lufs + clip.volume_db();
                measured.push((
                    clip.path.clone(),
                    m.lufs,
                    clip.effective_volume(),
                    effective,
                ));
            }
            Err(e) if e.downcast_ref::<NoAudioTrack>().is_some() => no_audio += 1,
            Err(e) => {
                unreadable += 1;
                eprintln!("  unreadable {}: {e:#}", clip.path.display());
            }
        }
    }

    if measured.is_empty() {
        bail!("no clip could be measured in {}", dir.display());
    }

    measured.sort_by(|a, b| a.3.total_cmp(&b.3));
    if verbose {
        println!();
        for (path, lufs, volume, effective) in &measured {
            println!(
                "  {:<44} {:>7.1} LUFS  x{:>4.0}%  ->{:>7.1}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                lufs,
                volume,
                effective
            );
        }
    }

    let levels: Vec<f64> = measured.iter().map(|m| m.3).collect();
    let median = levels[levels.len() / 2];
    let outliers: Vec<&(PathBuf, f64, f64, f64)> = measured
        .iter()
        .filter(|m| (m.3 - median).abs() > band)
        .collect();

    println!(
        "\n{} clips measured, {silent} silent by design, {no_audio} without an audio track, {unreadable} unreadable",
        levels.len()
    );
    println!("  quietest      {:>8.1} LUFS", levels[0]);
    println!("  median        {:>8.1} LUFS", median);
    println!("  loudest       {:>8.1} LUFS", levels[levels.len() - 1]);
    println!(
        "  spread        {:>8.1} LU",
        levels[levels.len() - 1] - levels[0]
    );
    println!("  outside ±{band:.0} LU  {:>5} clips", outliers.len());

    for (path, _, volume, effective) in outliers.iter().take(10) {
        println!(
            "    {:<44} {:>7.1} LUFS at {:.0}%  ({:+.1} LU)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            effective,
            volume,
            effective - median
        );
    }
    Ok(())
}

/// Resolve a folder to the AltSound csv it contains.
fn locate_csv(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("reading {}", path.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")))
        .collect();
    found.sort();
    match found.len() {
        0 => bail!("no csv in {}", path.display()),
        1 => Ok(found.remove(0)),
        _ => bail!(
            "{} holds several csv files, name the one to use",
            path.display()
        ),
    }
}

/// What measuring a pack produced: the meter, and how many entries it covers.
struct PackMeasurement {
    meter: SourceMeter,
    measured: usize,
    listed: usize,
}

/// Measure every sound a pack refers to.
fn measure_pack(csv_path: &Path, verbose: bool) -> Result<PackMeasurement> {
    let pack = AltsoundPack::load(csv_path)?;
    let files = pack.sound_files();
    let listed = files.len();
    let mut meter = SourceMeter::new();
    let mut measured = 0;

    for file in &files {
        if !file.exists() {
            eprintln!("  missing: {}", file.display());
            continue;
        }
        match meter.add_file(file) {
            Ok(m) => {
                measured += 1;
                if verbose {
                    println!(
                        "  {:<28} {:>8.1} LUFS  {:>6.1} LU  {:>6.1} dBTP",
                        file.file_name().unwrap_or_default().to_string_lossy(),
                        m.lufs,
                        m.lra,
                        m.true_peak_dbtp
                    );
                }
            }
            Err(e) => eprintln!("  skipped {}: {e:#}", file.display()),
        }
    }

    if measured == 0 {
        bail!("nothing could be measured in {}", csv_path.display());
    }
    Ok(PackMeasurement {
        meter,
        measured,
        listed,
    })
}

fn scan(path: &Path, target: f64, ceiling: f64, verbose: bool) -> Result<()> {
    let csv_path = locate_csv(path)?;
    println!("{}", csv_path.display());
    let PackMeasurement {
        meter,
        measured,
        listed,
    } = measure_pack(&csv_path, verbose)?;
    if measured < listed {
        println!(
            "\n{} of the {listed} listed sounds could not be read",
            listed - measured
        );
    }

    let Some(source) = meter.source()? else {
        bail!("the pack mixes sample rates or channel layouts, cannot measure it as one source");
    };
    let plan = plan(source.lufs, meter.worst_true_peak(), target, ceiling);

    println!("\n{measured} sounds measured");
    println!(
        "  loudness      {:>8.1} LUFS  (target {target:.1})",
        source.lufs
    );
    println!("  range         {:>8.1} LU", source.lra);
    println!(
        "  worst peak    {:>8.1} dBTP (ceiling {ceiling:.1})",
        meter.worst_true_peak()
    );
    println!("  gain wanted   {:>+8.1} dB", plan.wanted_db);
    println!("  gain possible {:>+8.1} dB", plan.applied_db);
    if plan.is_capped() {
        println!(
            "  capped by the true peak ceiling, {:.1} dB of headroom left",
            plan.headroom_db
        );
    }
    Ok(())
}

fn apply(
    path: &Path,
    target: f64,
    ceiling: f64,
    cache_path: &Path,
    allow_missing: bool,
    force: bool,
) -> Result<()> {
    let csv_path = locate_csv(path)?;
    let dir = csv_path.parent().unwrap_or(Path::new(".")).to_path_buf();

    // Decide before measuring, not after: the whole point is to skip the scan.
    let fingerprint = stamp::fingerprint(&AltsoundPack::load(&csv_path)?.sound_files())?;
    let engine = engine::signature();
    if !force
        && let Some(previous) = Stamp::load(&dir)?
        && previous.is_current(&fingerprint, engine, target, ceiling)
    {
        println!(
            "{} is already normalized ({:+.1} dB in the pack, {:+.1} dB on the bus)",
            dir.display(),
            previous.written_db,
            previous.residual_db
        );
        return Ok(());
    }

    let PackMeasurement {
        meter,
        measured,
        listed,
    } = measure_pack(&csv_path, false)?;

    // Correcting a pack from a fraction of its sounds would write a gain that
    // looks authoritative and is simply wrong, so it takes saying so out loud.
    if measured < listed && !allow_missing {
        bail!(
            "only {measured} of {listed} sounds could be measured; \
             the gain would not describe this pack. Pass --allow-missing to write it anyway"
        );
    }

    let Some(source) = meter.source()? else {
        bail!("the pack mixes sample rates or channel layouts, cannot measure it as one source");
    };
    let plan = plan(source.lufs, meter.worst_true_peak(), target, ceiling);

    let mut pack = AltsoundPack::load(&csv_path)?;
    let report = pack.apply_gain(plan.linear())?;
    pack.save()?;

    let source_id = csv_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| csv_path.display().to_string());

    let mut cache = Cache::load(cache_path)?;
    cache.put(CacheEntry {
        source_id: source_id.clone(),
        kind: "altsound".to_string(),
        lufs: source.lufs,
        lra: source.lra,
        true_peak_dbtp: meter.worst_true_peak(),
        target_lufs: target,
        ceiling_dbtp: ceiling,
        written_db: report.written_db,
        residual_db: report.residual_db,
        file_count: measured,
        algorithm_version: ALGORITHM_VERSION,
    });
    cache.save(cache_path)?;

    Stamp {
        fingerprint,
        engine: engine.to_string(),
        target_lufs: target,
        ceiling_dbtp: ceiling,
        lufs: source.lufs,
        lra: source.lra,
        true_peak_dbtp: meter.worst_true_peak(),
        written_db: report.written_db,
        residual_db: report.residual_db,
        at: Stamp::now(),
    }
    .save(&dir)?;

    println!("{source_id}: {measured} sounds, {:.1} LUFS", source.lufs);
    println!(
        "  written to the csv  {:>+6.1} dB on {} entries",
        report.written_db, report.adjusted
    );
    if report.residual_db > 0.01 {
        println!(
            "  left for the bus    {:>+6.1} dB — set AudioSource.altsound.Gain accordingly",
            report.residual_db
        );
    }
    if plan.is_capped() {
        println!("  the target was not reached: true peak ceiling reached first");
    }
    Ok(())
}
