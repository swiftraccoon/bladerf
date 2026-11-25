//! Survey, export and qualify active-window slices from one closed capture.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::Args;

use super::{
    CompareEventReplay, MAX_REPLAY_JOBS, Protocol, ReplayControl, plan, remaining, replay,
    replay_lane, verify_baseline,
};
use crate::{input, process, report};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_CAPTURE_SAMPLES: u64 = 20_000_000;
const LOG_BYTES: u64 = 1024 * 1024;
const EVENT_REPORT_BYTES: u64 = 4 * 1024 * 1024;
const COMPARISON_BYTES: u64 = 16 * 1024;

#[derive(Debug, Args)]
pub(crate) struct QualifyEventReplay {
    /// Complete source capture with its adjacent acquisition sidecar.
    input: PathBuf,
    /// Complete digital-monitor analysis directory for the same capture.
    #[arg(long)]
    analysis: PathBuf,
    /// New private directory for the survey, slices and both comparisons.
    #[arg(long)]
    output: PathBuf,
    /// Use half-overlapping FFT windows for event detection.
    #[arg(long)]
    overlap_events: bool,
    /// Include gaps up to this many saved samples between padded slices.
    #[arg(long, default_value_t = 32_760, value_parser = clap::value_parser!(u64).range(..=SAMPLE_RATE_HZ))]
    merge_gap_samples: u64,
    /// Join the smallest remaining slice gaps until this count is met.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=plan::MAX_SLICES as i64))]
    target_slices: Option<u32>,
    /// Prebuilt RF observatory executable.
    #[arg(long, default_value = "target/release/rf-observatory")]
    rf_observatory: PathBuf,
    /// Prebuilt Wi-Fi monitor executable matching the analysis.
    #[arg(long, default_value = "target/release/wifi-monitor")]
    wifi_monitor: PathBuf,
    /// Prebuilt Bluetooth monitor executable matching the analysis.
    #[arg(long, default_value = "target/release/bluetooth-monitor")]
    bluetooth_monitor: PathBuf,
    /// Shared wall-clock budget checked between survey, export and replay stages.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=900))]
    timeout_seconds: u64,
    /// Maximum number of slices decoded concurrently across replay lanes.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=MAX_REPLAY_JOBS))]
    jobs: u8,
    /// Run Wi-Fi and Bluetooth replay concurrently when at least two decoder slots are available.
    #[arg(long)]
    overlap_lanes: bool,
    /// Print phase durations on stderr.
    #[arg(long)]
    timings: bool,
}

fn run_rf_stage(
    program: &process::ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    stage: &str,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Duration> {
    let started = Instant::now();
    let command = process::run(
        program,
        arguments,
        output,
        stage,
        LOG_BYTES,
        remaining(deadline, stop)?,
        stop,
    )?;
    process::require_success(&command).with_context(|| format!("qualification {stage}"))?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "qualification {stage} exceeded its retained log bound"
    );
    ensure!(
        process::resolve_program(program.name, &program.path)? == *program,
        "RF observatory executable changed during qualification {stage}"
    );
    Ok(started.elapsed())
}

pub(crate) fn qualify(args: &QualifyEventReplay, stop: &AtomicBool) -> Result<PathBuf> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.timeout_seconds);
    remaining(deadline, stop)?;
    let (analysis, baseline_sha256) =
        report::read_json_with_sha256(&args.analysis.join("analysis.json"))?;
    let center_hz = report::required_u64(&analysis["source"], "center_frequency_hz")?;
    ensure!(
        center_hz.is_multiple_of(1_000_000),
        "event replay qualification requires a whole-MHz capture center"
    );
    let source = input::validate(&args.input, center_hz, SAMPLE_RATE_HZ, MAX_CAPTURE_SAMPLES)?;
    let wifi = process::resolve_program(Protocol::Wifi.program(), &args.wifi_monitor)?;
    let bluetooth =
        process::resolve_program(Protocol::Bluetooth.program(), &args.bluetooth_monitor)?;
    let lanes = [(Protocol::Wifi, wifi), (Protocol::Bluetooth, bluetooth)];
    for (protocol, program) in &lanes {
        ensure!(
            verify_baseline(&args.analysis, &source, *protocol, program)? == baseline_sha256,
            "baseline analysis changed during qualification preflight"
        );
    }
    let rf = process::resolve_program("rf-observatory", &args.rf_observatory)?;
    remaining(deadline, stop)?;
    let output = report::fresh_output(&args.output)?;
    report::private_directory(&output.join("logs"))?;
    let survey = output.join("survey");
    let mut survey_arguments = vec![
        "survey".into(),
        source.input.as_os_str().to_owned(),
        "--center-mhz".into(),
        (center_hz / 1_000_000).to_string().into(),
        "--max-samples".into(),
        source.complex_samples.to_string().into(),
        "--dense".into(),
        "--events".into(),
        "--include-band-edges".into(),
        "--active-window-ranges".into(),
        "--output".into(),
        survey.as_os_str().to_owned(),
    ];
    if args.overlap_events {
        survey_arguments.push("--overlap-events".into());
    }
    let survey_time = run_rf_stage(&rf, &survey_arguments, &output, "survey", deadline, stop)?;
    let event_report = survey.join("events.json");
    let event_report_sha256 =
        report::hash_direct_regular_file(&event_report, EVENT_REPORT_BYTES)?.sha256;

    let slices = output.join("slices");
    let mut export_arguments = vec![
        "slice-events".into(),
        source.input.as_os_str().to_owned(),
        "--events".into(),
        event_report.as_os_str().to_owned(),
        "--merge-gap-samples".into(),
        args.merge_gap_samples.to_string().into(),
        "--output".into(),
        slices.as_os_str().to_owned(),
    ];
    if let Some(target) = args.target_slices {
        export_arguments.extend([
            "--coalesce-to-limit".into(),
            "--max-slices".into(),
            target.to_string().into(),
        ]);
    }
    let export_time = run_rf_stage(&rf, &export_arguments, &output, "export", deadline, stop)?;

    let (manifest, slice_manifest_sha256) = report::read_json_with_sha256_limit(
        &slices.join(plan::MANIFEST_NAME),
        plan::MANIFEST_BYTES,
    )?;
    let stride = if args.overlap_events {
        plan::OVERLAP_STRIDE_SAMPLES
    } else {
        plan::WINDOW_SAMPLES
    };
    let gap = manifest["limits"]
        .get("merge_gap_samples")
        .map_or(Some(0), serde_json::Value::as_u64);
    ensure!(
        manifest["schema_version"].as_u64() == Some(3)
            && manifest["event_window_stride_samples"].as_u64() == Some(stride)
            && gap == Some(args.merge_gap_samples)
            && args.target_slices.is_none_or(
                |target| manifest["limits"]["max_slices"].as_u64() == Some(u64::from(target))
            ),
        "qualification slice manifest differs from the requested stride, gap or count limit"
    );
    ensure!(
        manifest["event_report_sha256"].as_str() == Some(event_report_sha256.as_str()),
        "slice manifest does not match the qualification survey"
    );

    remaining(deadline, stop)?;
    let verification_started = Instant::now();
    let replay_plan = plan::verify(&source.input, &slices, stop)?;
    let mut shared_verify = verification_started.elapsed();
    ensure!(
        replay_plan.source == source && replay_plan.manifest_sha256 == slice_manifest_sha256,
        "source capture or slice plan changed before qualification replay"
    );
    let overlap_lanes = args.overlap_lanes && args.jobs > 1;
    let slots = overlap_lanes.then(|| replay::DecodeSlots::new(args.jobs));
    let run_lane = |(protocol, program): &(Protocol, process::ProgramProvenance)| {
        remaining(deadline, stop)?;
        let lane_started = Instant::now();
        replay_plan.verify_snapshot(stop)?;
        ensure!(
            process::resolve_program(program.name, &program.path)? == *program,
            "{} monitor executable changed before qualification replay",
            protocol.stage()
        );
        let comparison = replay_lane(
            &CompareEventReplay {
                input: source.input.clone(),
                slices: slices.clone(),
                analysis: args.analysis.clone(),
                protocol: *protocol,
                monitor: Some(program.path.clone()),
                timeout_seconds: args.timeout_seconds,
                jobs: args.jobs,
                timings: args.timings,
                output: output.join(protocol.stage()),
            },
            &replay_plan,
            program,
            lane_started,
            ReplayControl {
                deadline,
                stop,
                slots: slots.as_ref(),
            },
            || replay_plan.verify_snapshot(stop),
        )
        .with_context(|| format!("qualify {} event replay", protocol.stage()))?;
        ensure!(
            comparison.result.source_sha256 == source.sha256
                && comparison.result.baseline_analysis_sha256 == baseline_sha256
                && comparison.result.monitor_sha256 == program.sha256
                && comparison.result.slice_manifest_sha256 == slice_manifest_sha256,
            "{} comparison identities differ from the qualified capture and slices",
            protocol.stage()
        );
        Ok(comparison)
    };
    let pending = if overlap_lanes {
        thread::scope(|scope| {
            let workers = lanes.each_ref().map(|lane| {
                let run_lane = &run_lane;
                let stage = lane.0.stage();
                let launched = thread::Builder::new()
                    .name(format!("qualification-{stage}"))
                    .spawn_scoped(scope, move || run_lane(lane));
                (stage, launched)
            });
            // Join every launched lane before returning the first error in lane order.
            let [wifi, bluetooth] = workers.map(|(stage, launched)| {
                launched
                    .with_context(|| format!("start {stage} qualification lane"))?
                    .join()
                    .map_err(|_| anyhow::anyhow!("{stage} qualification lane panicked"))?
            });
            Ok::<_, anyhow::Error>([wifi?, bluetooth?])
        })?
    } else {
        [run_lane(&lanes[0])?, run_lane(&lanes[1])?]
    };

    remaining(deadline, stop)?;
    let verification_started = Instant::now();
    let after = plan::verify(&source.input, &slices, stop)?;
    shared_verify += verification_started.elapsed();
    ensure!(
        after.source == source && after.manifest_sha256 == slice_manifest_sha256,
        "source capture or slice plan changed during qualification replay"
    );
    replay_plan.verify_snapshot(stop)?;
    ensure!(
        report::hash_direct_regular_file(&event_report, EVENT_REPORT_BYTES)?.sha256
            == event_report_sha256,
        "survey event report changed during qualification"
    );
    for ((protocol, program), comparison) in lanes.iter().zip(pending) {
        remaining(deadline, stop)?;
        replay_plan.verify_snapshot(stop)?;
        let published = comparison.publish(deadline, stop)?;
        let (receipt, _) = report::read_json_with_sha256_limit(
            &published.join("comparison.json"),
            COMPARISON_BYTES,
        )?;
        ensure!(
            receipt["source_sha256"].as_str() == Some(source.sha256.as_str())
                && receipt["baseline_analysis_sha256"].as_str() == Some(baseline_sha256.as_str())
                && receipt["monitor_sha256"].as_str() == Some(program.sha256.as_str())
                && receipt["slice_manifest_sha256"].as_str()
                    == Some(slice_manifest_sha256.as_str()),
            "{} published comparison identities differ from the qualified capture and slices",
            protocol.stage()
        );
    }
    remaining(deadline, stop)?;
    if args.timings {
        let _ = writeln!(
            io::stderr().lock(),
            "event replay qualification timings (seconds) survey={:.6} export={:.6} shared_verify={:.6} total={:.6} overlap_lanes={overlap_lanes} jobs={}",
            survey_time.as_secs_f64(),
            export_time.as_secs_f64(),
            shared_verify.as_secs_f64(),
            started.elapsed().as_secs_f64(),
            args.jobs,
        );
    }
    Ok(output)
}
