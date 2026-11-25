//! Supervised replay of verified IQ slices against a complete capture decode.

mod packets;
mod plan;
mod qualification;
pub(crate) mod replay;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::Value;

use crate::{input::SourceReport, process, report};

pub(crate) use qualification::{QualifyEventReplay, qualify};

const MAX_REPLAY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_VALIDATED_PACKETS: usize = 100_000;
const MAX_REPLAY_JOBS: i64 = 4;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Protocol {
    Wifi,
    Bluetooth,
}

impl Protocol {
    pub(crate) fn stage(self) -> &'static str {
        match self {
            Self::Wifi => "wifi",
            Self::Bluetooth => "bluetooth",
        }
    }

    pub(crate) fn program(self) -> &'static str {
        match self {
            Self::Wifi => "wifi-monitor",
            Self::Bluetooth => "bluetooth-monitor",
        }
    }

    fn tolerance(self) -> u64 {
        match self {
            Self::Wifi => 1,
            Self::Bluetooth => 400,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct CompareEventReplay {
    /// Complete source capture with its adjacent acquisition sidecar.
    input: PathBuf,
    /// Directory published by rf-observatory slice-events.
    #[arg(long)]
    slices: PathBuf,
    /// Trusted, closed digital-monitor analysis directory for the same source.
    #[arg(long)]
    analysis: PathBuf,
    /// Decoder lane to compare.
    #[arg(long, value_enum)]
    protocol: Protocol,
    /// Prebuilt monitor executable. Defaults to target/release for the selected lane.
    #[arg(long)]
    monitor: Option<PathBuf>,
    /// Comparison deadline in seconds, checked between verification and replay stages.
    #[arg(long, default_value_t = 300, value_parser=clap::value_parser!(u64).range(1..=900))]
    timeout_seconds: u64,
    /// Maximum number of slices decoded concurrently.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=MAX_REPLAY_JOBS))]
    jobs: u8,
    /// Print wall-clock phase durations to stderr after successful publication.
    #[arg(long)]
    timings: bool,
    /// New private output directory for decoder artifacts and comparison.json.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Comparison {
    schema_version: u32,
    operation: &'static str,
    protocol: Protocol,
    center_frequency_hz: u64,
    source_sha256: String,
    baseline_analysis_sha256: String,
    slice_manifest_sha256: String,
    monitor_sha256: String,
    bluetooth_decoder_sha256: Option<String>,
    input_samples: u64,
    analyzed_samples: u64,
    selected_samples: u64,
    slices_replayed: usize,
    omitted_component_count: u64,
    reference_validated_packets: usize,
    reference_positions_covered: usize,
    replay_validated_packets: usize,
    matched_packets: u64,
    missing_reference_packets: u64,
    extra_replay_packets: u64,
    match_tolerance_samples: u64,
    matched_delta_samples_min: Option<i64>,
    matched_delta_samples_max: Option<i64>,
    retained_replay_bytes: u64,
    elapsed_wall_seconds: f64,
}

struct ReplayTimings {
    preflight: Duration,
    replay: Duration,
    supervised: Duration,
    reverify: Duration,
    matching: Duration,
    jobs: u8,
}

struct PendingComparison {
    output: PathBuf,
    result: Comparison,
    timings: Option<ReplayTimings>,
}

struct ReplayControl<'a> {
    deadline: Instant,
    stop: &'a AtomicBool,
    slots: Option<&'a replay::DecodeSlots>,
}

impl PendingComparison {
    fn publish(self, deadline: Instant, stop: &AtomicBool) -> Result<PathBuf> {
        remaining(deadline, stop)?;
        let publication_started = Instant::now();
        publish_comparison(&self.output, &self.result, stop)?;
        if let Some(times) = self.timings {
            let publication = publication_started.elapsed();
            // The comparison is already published, so a closed diagnostic
            // stream must not turn this into a failed comparison.
            let _ = writeln!(
                std::io::stderr().lock(),
                "event replay timings (seconds) preflight={:.6} replay={:.6} supervised_decoders={:.6} reverify={:.6} match={:.6} publish={:.6} jobs={}",
                times.preflight.as_secs_f64(),
                times.replay.as_secs_f64(),
                times.supervised.as_secs_f64(),
                times.reverify.as_secs_f64(),
                times.matching.as_secs_f64(),
                publication.as_secs_f64(),
                times.jobs,
            );
        }
        Ok(self.output)
    }
}

fn remaining(deadline: Instant, stop: &AtomicBool) -> Result<Duration> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "event replay comparison interrupted"
    );
    let left = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !left.is_zero(),
        "event replay comparison exceeded its time budget"
    );
    Ok(left)
}

fn verify_baseline(
    root: &Path,
    source: &SourceReport,
    protocol: Protocol,
    program: &process::ProgramProvenance,
) -> Result<String> {
    let (analysis, digest) = report::read_json_with_sha256(&root.join("analysis.json"))?;
    let matches_text =
        |field, expected| analysis.get(field).and_then(Value::as_str) == Some(expected);
    ensure!(
        analysis.get("schema_version").and_then(Value::as_u64) == Some(2)
            && matches_text("operation", "offline_multi_protocol_analysis")
            && matches_text("overall_state", "complete"),
        "event replay requires a complete schema 2 baseline analysis"
    );
    let baseline = analysis
        .get("source")
        .context("baseline analysis lacks source identity")?;
    ensure!(
        baseline["sha256"].as_str() == Some(source.sha256.as_str())
            && baseline["sidecar_sha256"].as_str() == Some(source.sidecar_sha256.as_str())
            && baseline["center_frequency_hz"].as_u64() == Some(source.center_frequency_hz)
            && baseline["sample_rate_hz"].as_u64() == Some(source.sample_rate_hz)
            && baseline["complex_samples"].as_u64() == Some(source.complex_samples),
        "baseline analysis and verified capture have different identities"
    );
    let stages = analysis["stages"]
        .as_array()
        .context("baseline analysis lacks stage states")?;
    for name in [protocol.stage(), "terminal_integrity"] {
        let mut entries = stages
            .iter()
            .filter(|stage| stage["name"].as_str() == Some(name));
        ensure!(
            entries
                .next()
                .is_some_and(|stage| stage["state"].as_str() == Some("complete"))
                && entries.next().is_none(),
            "baseline stage {name} must be complete and unique"
        );
    }
    let programs = analysis["programs"]
        .as_array()
        .context("baseline analysis lacks program identities")?;
    let mut matches = programs
        .iter()
        .filter(|entry| entry["name"].as_str() == Some(program.name));
    ensure!(
        matches
            .next()
            .is_some_and(|entry| entry["bytes"].as_u64() == Some(program.bytes)
                && entry["sha256"].as_str() == Some(program.sha256.as_str()))
            && matches.next().is_none(),
        "slice replay monitor must match a unique baseline executable"
    );
    Ok(digest)
}

fn publish_comparison(output: &Path, result: &Comparison, stop: &AtomicBool) -> Result<()> {
    let temporary = output.join(".comparison.json.partial");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, result)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "event replay comparison interrupted before publication"
    );
    fs::hard_link(&temporary, output.join("comparison.json"))?;
    fs::remove_file(temporary)?;
    File::open(output)?.sync_all()?;
    Ok(())
}

pub(crate) fn compare(args: &CompareEventReplay, stop: &AtomicBool) -> Result<PathBuf> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.timeout_seconds);
    remaining(deadline, stop)?;
    let plan = plan::verify(&args.input, &args.slices, stop)?;
    let fallback = Path::new("target/release").join(args.protocol.program());
    let program = process::resolve_program(
        args.protocol.program(),
        args.monitor.as_deref().unwrap_or(&fallback),
    )?;
    let control = ReplayControl {
        deadline,
        stop,
        slots: None,
    };
    replay_lane(args, &plan, &program, started, control, || {
        let after = plan::verify(&args.input, &args.slices, stop)?;
        ensure!(
            after.source == plan.source && after.manifest_sha256 == plan.manifest_sha256,
            "source capture or slice plan changed during replay"
        );
        Ok(())
    })?
    .publish(deadline, stop)
}

fn replay_lane(
    args: &CompareEventReplay,
    plan: &plan::Plan,
    program: &process::ProgramProvenance,
    started: Instant,
    control: ReplayControl<'_>,
    reverify_input: impl FnOnce() -> Result<()>,
) -> Result<PendingComparison> {
    let ReplayControl {
        deadline,
        stop,
        slots,
    } = control;
    remaining(deadline, stop)?;
    let source = &plan.source;
    let analysis_sha256 = verify_baseline(&args.analysis, source, args.protocol, program)?;
    let reference = replay::read(
        &args.analysis.join(args.protocol.stage()),
        args.protocol,
        source,
        0,
        replay::WifiReplayOptions::default(),
        stop,
    )?;
    let mut selected = Vec::new();
    remaining(deadline, stop)?;
    let output = report::fresh_output(&args.output)?;
    let runner = replay::Runner {
        program,
        protocol: args.protocol,
        source,
        slice_root: &args.slices,
        stop,
        slots,
    };
    let mut replay_bytes = 0u64;
    let mut supervised_decoders = Duration::ZERO;
    let replay_started = Instant::now();
    for group in plan.slices.chunks(usize::from(args.jobs)) {
        for result in runner.run_group(group, &output, deadline)? {
            supervised_decoders += result.supervised;
            ensure!(
                result.replay.decoder_sha256 == reference.decoder_sha256,
                "nested decoder differs from reference"
            );
            ensure!(
                selected.len() + result.replay.packets.len() <= MAX_VALIDATED_PACKETS,
                "validated replay packet limit exceeded"
            );
            selected.extend(result.replay.packets);
            replay_bytes = replay_bytes
                .checked_add(result.retained_bytes)
                .context("event replay artifact extent overflow")?;
            ensure!(
                replay_bytes <= MAX_REPLAY_BYTES,
                "event replay output exceeds 512 MiB"
            );
        }
    }
    let verification_started = Instant::now();
    remaining(deadline, stop)?;
    reverify_input()?;
    ensure!(
        process::resolve_program(program.name, &program.path)? == *program,
        "monitor executable changed during replay"
    );
    ensure!(
        verify_baseline(&args.analysis, source, args.protocol, program)? == analysis_sha256,
        "baseline analysis manifest changed during replay"
    );
    let matching_started = Instant::now();
    let matches = packets::match_packets(&reference.packets, &selected, args.protocol)?;
    let covered = reference
        .packets
        .iter()
        .filter(|packet| {
            let index = plan
                .slices
                .partition_point(|slice| slice.end_sample_exclusive <= packet.sample);
            plan.slices
                .get(index)
                .is_some_and(|slice| slice.start_sample <= packet.sample)
        })
        .count();
    let matching_finished = Instant::now();
    let result = Comparison {
        schema_version: 1,
        operation: "compare_event_slice_replay",
        protocol: args.protocol,
        center_frequency_hz: source.center_frequency_hz,
        source_sha256: source.sha256.clone(),
        baseline_analysis_sha256: analysis_sha256,
        slice_manifest_sha256: plan.manifest_sha256.clone(),
        monitor_sha256: program.sha256.clone(),
        bluetooth_decoder_sha256: reference.decoder_sha256,
        input_samples: source.complex_samples,
        analyzed_samples: plan.analyzed_samples,
        selected_samples: plan.exported_samples,
        slices_replayed: plan.slices.len(),
        omitted_component_count: plan.omitted_component_count,
        reference_validated_packets: reference.packets.len(),
        reference_positions_covered: covered,
        replay_validated_packets: selected.len(),
        matched_packets: matches.matched,
        missing_reference_packets: reference.packets.len() as u64 - matches.matched,
        extra_replay_packets: selected.len() as u64 - matches.matched,
        match_tolerance_samples: args.protocol.tolerance(),
        matched_delta_samples_min: matches.delta_samples_min,
        matched_delta_samples_max: matches.delta_samples_max,
        retained_replay_bytes: replay_bytes,
        elapsed_wall_seconds: (matching_finished - started).as_secs_f64(),
    };
    Ok(PendingComparison {
        output,
        result,
        timings: args.timings.then_some(ReplayTimings {
            preflight: replay_started - started,
            replay: verification_started - replay_started,
            supervised: supervised_decoders,
            reverify: matching_started - verification_started,
            matching: matching_finished - matching_started,
            jobs: args.jobs,
        }),
    })
}
