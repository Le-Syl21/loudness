//! Command line front end: measure an AltSound pack, and optionally correct it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use loudness::altsound::AltsoundPack;
use loudness::cache::{ALGORITHM_VERSION, Cache, CacheEntry};
use loudness::engine;
use loudness::gain::{DEFAULT_CEILING_DBTP, DEFAULT_TARGET_LUFS, plan};
use loudness::measure::SourceMeter;
use loudness::pup::{MAX_VOLUME, PupPack};
use loudness::pup_measure::{SkipCounts, TriggerLevel};
use loudness::stamp::{self, Stamp};
use loudness::{pup_gain, pup_measure};

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
        /// List every trigger measured.
        #[arg(long)]
        verbose: bool,
    },
    /// Measure a PUP pack and write the corrected volumes into triggers.pup.
    PupApply {
        /// Folder holding triggers.pup.
        path: PathBuf,
        /// Half-width of the band around the median, in LU. Outliers are
        /// pulled back to its edge, never onto the median.
        #[arg(long, default_value_t = 6.0)]
        band: f64,
        /// True peak ceiling, in dBTP.
        #[arg(long, default_value_t = DEFAULT_CEILING_DBTP)]
        ceiling: f64,
        /// Measure and write again even if nothing has changed.
        #[arg(long)]
        force: bool,
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
        Command::PupApply {
            path,
            band,
            ceiling,
            force,
        } => pup_apply(&path, band, ceiling, force),
    }
}

/// Measure a PUP pack and describe how spread out it is.
///
/// The number that matters is not a clip's own loudness but the loudness it
/// reaches once the pack's own `Volume` is applied — that is what the player
/// hears, and what an author has already tuned by hand.
fn pup_scan(dir: &Path, band: f64, verbose: bool) -> Result<()> {
    let pack = PupPack::load(dir)?;
    println!(
        "{}: {} triggers, {} playlists",
        dir.display(),
        pack.triggers.len(),
        pack.playlists.len()
    );

    let (levels, skipped) = pup_measure::measure(&pack)?;
    if levels.is_empty() {
        bail!("no clip could be measured in {}", dir.display());
    }

    let mut effective: Vec<f64> = levels.iter().map(TriggerLevel::effective_lufs).collect();
    let median = pup_measure::median(&mut effective);

    if verbose {
        let mut sorted = levels.clone();
        sorted.sort_by(|a, b| a.effective_lufs().total_cmp(&b.effective_lufs()));
        println!();
        for level in &sorted {
            println!(
                "  {:<44} {:>7.1} LUFS  x{:>4.0}%  ->{:>7.1}",
                level.label,
                level.lufs,
                level.volume,
                level.effective_lufs()
            );
        }
    }

    report_spread(&levels, median, band, skipped);

    let changes = pup_gain::plan(&levels, median, band, DEFAULT_CEILING_DBTP, MAX_VOLUME);
    println!("\n{} triggers would be adjusted", changes.len());
    for change in changes.iter().take(12) {
        println!(
            "    {:<44} {:>4.0}% -> {:>4.0}%  ({:+.1} LU off)",
            change.label, change.from, change.to, change.was_off_by
        );
    }
    Ok(())
}

/// Print the spread of a measured pack.
fn report_spread(levels: &[TriggerLevel], median: f64, band: f64, skipped: SkipCounts) {
    let mut effective: Vec<f64> = levels.iter().map(TriggerLevel::effective_lufs).collect();
    effective.sort_by(f64::total_cmp);
    let outliers = effective
        .iter()
        .filter(|v| (**v - median).abs() > band)
        .count();

    println!(
        "\n{} triggers measured, {} muted by the pack, {} with nothing measurable",
        levels.len(),
        skipped.muted_triggers,
        skipped.empty_triggers
    );
    println!(
        "  files skipped: {} silent, {} without an audio track, {} unreadable",
        skipped.silent_files, skipped.no_audio, skipped.unreadable
    );
    println!("  quietest      {:>8.1} LUFS", effective[0]);
    println!("  median        {:>8.1} LUFS", median);
    println!(
        "  loudest       {:>8.1} LUFS",
        effective[effective.len() - 1]
    );
    println!(
        "  spread        {:>8.1} LU",
        effective[effective.len() - 1] - effective[0]
    );
    println!("  outside ±{band:.0} LU  {outliers:>5} triggers");
}

/// Measure a PUP pack and write the corrected volumes into triggers.pup.
fn pup_apply(dir: &Path, band: f64, ceiling: f64, force: bool) -> Result<()> {
    let mut pack = PupPack::load(dir)?;

    let media: Vec<PathBuf> = pack
        .triggers
        .iter()
        .flat_map(|t| pack.trigger_files(t))
        .collect();
    let fingerprint = stamp::fingerprint(&media)?;
    let engine = engine::signature();
    if !force
        && let Some(previous) = Stamp::load(dir)?
        && previous.is_current(&fingerprint, engine, band, ceiling)
    {
        println!(
            "{} is already balanced (median {:.1} LUFS, band ±{:.0} LU)",
            dir.display(),
            previous.lufs,
            band
        );
        return Ok(());
    }

    let (levels, skipped) = pup_measure::measure(&pack)?;
    if levels.is_empty() {
        bail!("no clip could be measured in {}", dir.display());
    }
    let mut effective: Vec<f64> = levels.iter().map(TriggerLevel::effective_lufs).collect();
    let median = pup_measure::median(&mut effective);
    report_spread(&levels, median, band, skipped);

    let changes = pup_gain::plan(&levels, median, band, ceiling, MAX_VOLUME);
    let mut written = 0;
    for change in &changes {
        if !pack.set_trigger_volume(change.row, change.to) {
            eprintln!(
                "    skipped row {}: its volume field could not be edited",
                change.row
            );
            continue;
        }
        written += 1;
        println!(
            "    {:<44} {:>4.0}% -> {:>4.0}%  ({:+.1} LU off){}",
            change.label,
            change.from,
            change.to,
            change.was_off_by,
            if change.refused_db > 0.1 {
                format!(
                    ", {:.1} dB refused to stay under the ceiling",
                    change.refused_db
                )
            } else {
                String::new()
            }
        );
    }

    if written == 0 {
        println!("\nnothing to change");
    } else {
        pack.save_triggers()?;
        println!("\n{written} triggers rewritten, original kept as triggers.pup.bak");
    }

    let worst_peak = levels
        .iter()
        .map(|l| l.true_peak_dbtp)
        .fold(f64::NEG_INFINITY, f64::max);
    Stamp {
        fingerprint,
        engine: engine.to_string(),
        target_lufs: band,
        ceiling_dbtp: ceiling,
        lufs: median,
        lra: effective[effective.len() - 1] - effective[0],
        true_peak_dbtp: worst_peak,
        written_db: 0.0,
        residual_db: 0.0,
        at: Stamp::now(),
    }
    .save(dir)?;
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
