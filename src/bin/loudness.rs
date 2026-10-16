//! Command line front end: measure an AltSound pack, and optionally correct it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
use output_decibel_meter::capture;
use output_decibel_meter::meter::{Meter, Reading};

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
    /// Watch what actually leaves the speakers, live.
    ///
    /// Captures a program or an output and prints the loudness as it plays,
    /// so a change can be judged on what is heard rather than on what a file
    /// contains. The capture itself is output-decibel-meter's.
    Monitor {
        /// Part of a source name. Defaults to the system output, captured in
        /// loopback.
        #[arg(long, conflicts_with = "pid")]
        device: Option<String>,
        /// Meter what this process plays, before the mix: VPX alone, whatever
        /// else the machine is playing.
        #[arg(long)]
        pid: Option<u32>,
        /// Stop after this many seconds. Runs until Ctrl+C when absent.
        #[arg(long)]
        seconds: Option<f64>,
        /// List the devices that can be metered and exit.
        #[arg(long)]
        list: bool,
    },
    /// Measure any audio file and print its EBU R128 figures.
    ///
    /// Used as a sound level meter on a captured output: record what the
    /// speakers receive, then read what it actually measures.
    Meter {
        /// Audio file to measure.
        path: PathBuf,
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
        Command::Meter { path } => meter(&path),
        Command::Monitor {
            device,
            pid,
            seconds,
            list,
        } => monitor_output(device.as_deref(), pid, seconds, list),
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

/// Meter what the speakers receive.
fn monitor_output(
    device: Option<&str>,
    pid: Option<u32>,
    seconds: Option<f64>,
    list: bool,
) -> Result<()> {
    if list {
        for s in capture::sources()? {
            let pid = s.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            println!(
                "  {} {}{pid}",
                if s.is_output { "[out]" } else { "[in ]" },
                s.name
            );
        }
        return Ok(());
    }

    let source = match (pid, device) {
        (Some(pid), _) => capture::from_process(pid)?
            .into_iter()
            .next()
            .with_context(|| format!("process {pid} is not playing anything that can be tapped"))?,
        (None, Some(fragment)) => capture::find(fragment)?,
        (None, None) => capture::default_output()?,
    };
    println!(
        "metering {} ({})",
        source.name,
        if source.is_output {
            "output, captured in loopback"
        } else {
            "input"
        }
    );

    let capture = source.open()?;
    let mut meter = Meter::new(capture.channels, capture.sample_rate)?;

    let stop = Arc::new(AtomicBool::new(false));
    let on_signal = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || on_signal.store(true, Ordering::Relaxed));

    // Wall clock, so a source that hands over nothing cannot hold --seconds open.
    let started = Instant::now();
    let limit = seconds.map(Duration::from_secs_f64);
    let mut shown = 0.0;
    let mut last = Reading::default();
    while !stop.load(Ordering::Relaxed) && !limit.is_some_and(|l| started.elapsed() >= l) {
        let Some(block) = capture.next_block(Duration::from_millis(200)) else {
            continue;
        };
        last = meter.add(&block)?;
        // One line per second is enough to follow by eye.
        if last.seconds - shown >= 1.0 {
            shown = last.seconds;
            println!(
                "  {:>6.1}s   M {:>6.1}   S {:>6.1}   I {:>6.1} LUFS   peak {:>6.1} dBTP",
                last.seconds, last.momentary, last.short_term, last.integrated, last.true_peak
            );
        }
    }

    if last.seconds == 0.0 {
        bail!("{} handed over no audio at all", source.name);
    }
    println!("\nover {:.1} s of audio", last.seconds);
    println!("  integrated  {:>8.1} LUFS", last.integrated);
    println!("  true peak   {:>8.1} dBTP", last.true_peak);
    Ok(())
}

/// Measure one file and print what R128 says about it.
fn meter(path: &Path) -> Result<()> {
    let mut source = SourceMeter::new();
    let m = source.add_file(path)?;
    println!("{}", path.display());
    println!("  loudness    {:>8.1} LUFS", m.lufs);
    println!("  range       {:>8.1} LU", m.lra);
    println!("  true peak   {:>8.1} dBTP", m.true_peak_dbtp);
    println!(
        "  duration    {:>8.1} s  ({} Hz)",
        m.frames as f64 / m.sample_rate.max(1) as f64,
        m.sample_rate
    );
    Ok(())
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
    let report = pack.apply_gain(plan.linear(), source.lufs)?;
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
        residual_db: report.unreachable_db,
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
        residual_db: report.unreachable_db,
        at: Stamp::now(),
    }
    .save(&dir)?;

    println!("{source_id}: {measured} sounds, {:.1} LUFS", source.lufs);
    println!(
        "  written to the csv  {:>+6.1} dB on {} entries",
        report.written_db, report.adjusted
    );
    println!(
        "  loudest possible    {:>6.1} LUFS",
        report.max_reachable_lufs
    );
    if report.unreachable_db > 0.01 {
        println!(
            "  short of the target {:>+6.1} dB — an AltSound pack can only be attenuated,",
            report.unreachable_db
        );
        println!("                      so this pack caps the target of the whole system");
    }
    if plan.is_capped() {
        println!("  the target was not reached: true peak ceiling reached first");
    }
    Ok(())
}
