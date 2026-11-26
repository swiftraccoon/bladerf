//! Sequential stock-image sweeps with separate planned and completed coverage.
//!
//! Frequencies describe where a spectrum was inspected. A sparse targeted
//! sweep records explicit centers and carries no contiguous coverage claim.
//! Retunes and sparse time windows leave gaps in time.

mod follow_up;
mod native;
mod offset;
mod packet_preflight;
pub(crate) mod plan;
mod series;
mod series_follow_up;
mod signal_profile;
pub(crate) mod timing;
mod triage_follow_up;

pub(crate) use series::pass_directory_name;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};

use crate::lora::decode::{DEFAULT_WORKER, Worker};
use crate::signal_profile::SignalProfile;
use crate::triage::lora_decode_sweep::{self, DEFAULT_ATTEMPTS, MAX_ATTEMPTS, PacketReplayLimits};
use crate::triage::replay::{self, ReplayLimits};
use crate::{acquisition, directory, json_file, receiver, spectrum};

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Inclusive lower frequency of interest in hertz.
    #[arg(long, required_unless_present_any = ["center_hz", "bands", "follow_up_plan", "triage_report", "series_triage_report", "signal_profile"], requires = "upper_hz")]
    lower_hz: Option<u64>,
    /// Exclusive upper frequency of interest in hertz.
    #[arg(long, required_unless_present_any = ["center_hz", "bands", "follow_up_plan", "triage_report", "series_triage_report", "signal_profile"], requires = "lower_hz")]
    upper_hz: Option<u64>,
    /// Exact sparse tuning center in hertz. Repeat for additional centers.
    #[arg(long, conflicts_with_all = ["lower_hz", "upper_hz", "follow_up_plan"])]
    center_hz: Vec<u64>,
    /// Band to fit in one guarded receive span, with an optional millisecond minimum. Repeat up to 32 times.
    #[arg(long = "band", value_name = "LOWER_HZ:UPPER_HZ[:MS]", value_parser = plan::BandRequest::parse,
        conflicts_with_all = ["lower_hz", "upper_hz", "center_hz", "follow_up_plan", "triage_report", "series_triage_report"])]
    bands: Vec<plan::BandRequest>,
    /// Guarded band to revisit during a range sweep. Repeat for multiple bands.
    #[arg(long = "priority-band", requires_all = ["lower_hz", "upper_hz", "revisit_every"],
        value_name = "LOWER_HZ:UPPER_HZ[:MS]", value_parser = plan::BandRequest::parse)]
    priority_bands: Vec<plan::BandRequest>,
    /// Visit priority bands before each group of at most this many background visits.
    #[arg(long, requires = "priority_bands", value_name = "BACKGROUND_VISITS")]
    revisit_every: Option<usize>,
    /// Decoder follow-up plan produced by digital-monitor plan-sweep-follow-up.
    #[arg(long, conflicts_with_all = ["lower_hz", "upper_hz", "center_hz"])]
    follow_up_plan: Option<PathBuf>,
    /// Revisit probeable narrow candidates from a saved triage.json at two offset centers.
    #[arg(long, conflicts_with_all = ["lower_hz", "upper_hz", "center_hz", "follow_up_plan"])]
    triage_report: Option<PathBuf>,
    /// Recurrence report for narrow-band revisits or the companion to an activity report.
    #[arg(long, conflicts_with_all = ["lower_hz", "upper_hz", "center_hz", "follow_up_plan", "triage_report"])]
    series_triage_report: Option<PathBuf>,
    /// Include a companion overlapping-window report from the same sweep series.
    #[arg(long, requires = "series_triage_report")]
    overlap_series_triage_report: Option<PathBuf>,
    /// Revisit selected per-dwell activity rows at two offset centers.
    #[arg(long, requires_all = ["series_triage_report", "series_activity_source", "activity_targets"],
        conflicts_with = "overlap_series_triage_report")]
    series_activity_report: Option<PathBuf>,
    /// Completed sweep series referenced by the selected activity report.
    #[arg(long, requires = "series_activity_report")]
    series_activity_source: Option<PathBuf>,
    /// Select a one-based activity row by pass, tune, and transient or mean-power kind. Repeat for more rows.
    #[arg(long = "activity-target", requires = "series_activity_report", value_name = "PASS:TUNE:KIND:ROW",
        value_parser = series_follow_up::activity::Selection::parse)]
    activity_targets: Vec<series_follow_up::activity::Selection>,
    /// Versioned decoder selections to cover with one inferred tuning plan.
    #[arg(long, conflicts_with_all = ["lower_hz", "upper_hz", "center_hz", "bands", "priority_bands",
        "revisit_every", "dwell_at", "follow_up_plan", "triage_report", "series_triage_report"])]
    signal_profile: Option<PathBuf>,
    /// Replay the same signal profile after capture and write replay.json.
    #[arg(long, requires = "signal_profile")]
    replay: bool,
    #[command(flatten)]
    replay_limits: ReplayLimits,
    /// Prebuilt LoRa packet worker for signal-profile replay.
    #[arg(long, requires = "replay")]
    lora_worker: Option<PathBuf>,
    /// Maximum number of LoRa profile and dwell pairs sent to the worker.
    #[arg(long, requires = "replay", value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_ATTEMPTS)))]
    lora_max_attempts: Option<u32>,
    /// Default acquisition time at each tuning in milliseconds.
    #[arg(long, default_value_t = 40)]
    dwell_ms: u64,
    /// Override one explicit center's dwell. Repeat as CENTER_HZ:MS.
    #[arg(long, requires = "center_hz", value_name = "CENTER_HZ:MS", value_parser = plan::DwellOverride::parse)]
    dwell_at: Vec<plan::DwellOverride>,
    /// Number of passes over the same centers on one open receiver (1 through 16).
    #[arg(long, default_value_t = 1)]
    passes: usize,
    /// Maximum total raw IQ bytes across all tunings.
    #[arg(long, default_value_t = 1_073_741_824)]
    max_iq_bytes: u64,
    /// Host spectrum output after acquisition.
    #[arg(long, value_enum, default_value = "full")]
    spectrum_output: SpectrumOutput,
    /// Print the sweep plan as JSON without opening a radio or creating files.
    #[arg(long)]
    pub(crate) plan_only: bool,
    /// Receive connector carrying the antenna feed.
    #[arg(long, required_unless_present = "plan_only", value_parser = clap::value_parser!(u8).range(1..=2))]
    antenna: Option<u8>,
    /// Require hardware sample continuity and an end witness inside each stock RX1 dwell.
    #[arg(long)]
    check_continuity: bool,
    /// Manual receive gain in dB.
    #[arg(long, default_value_t = 40.0)]
    gain: f64,
    /// Fresh directory for the plan, IQ and survey reports.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Existing native acquisition executable, built from this workspace.
    #[arg(long)]
    receiver: Option<PathBuf>,
}

impl Args {
    fn packet_limits(&self) -> PacketReplayLimits {
        PacketReplayLimits {
            max_samples_per_dwell: self.replay_limits.general_prefix(),
            max_total_profile_samples: self.replay_limits.max_total_profile_samples,
            max_attempts: self.lora_max_attempts.unwrap_or(DEFAULT_ATTEMPTS),
            ..PacketReplayLimits::default()
        }
    }

    fn prepare(&self, stop: &AtomicBool) -> Result<(plan::Plan, Option<SignalProfile>)> {
        ensure!(
            self.dwell_at.is_empty() || !self.center_hz.is_empty(),
            "--dwell-at requires explicit --center-hz values"
        );
        ensure!(
            !self.check_continuity || self.antenna.is_none_or(|antenna| antenna == 1),
            "--check-continuity requires RX1"
        );
        ensure!(
            self.replay || self.replay_limits == ReplayLimits::default(),
            "sweep replay limits require --replay"
        );
        let profile = self
            .signal_profile
            .as_deref()
            .map(crate::signal_profile::read)
            .transpose()?;
        ensure!(
            (self.lora_worker.is_none() && self.lora_max_attempts.is_none())
                || profile
                    .as_ref()
                    .is_some_and(|profile| !profile.lora_packet_profile.is_empty()),
            "LoRa worker options require lora_packet_profile in the signal profile"
        );
        let mut plan = if let Some(profile) = &profile {
            let mut plan = signal_profile::plan(profile, self.dwell_ms, self.max_iq_bytes)?;
            plan.signal_profile_sha256 = Some(profile.sha256.clone());
            Ok(plan)
        } else if let Some(path) = &self.series_activity_report {
            let triage = self
                .series_triage_report
                .as_deref()
                .context("activity follow-up requires --series-triage-report")?;
            let capture = self
                .series_activity_source
                .as_deref()
                .context("activity follow-up requires --series-activity-source")?;
            let mut plan = series_follow_up::activity::read_plan(
                path,
                triage,
                capture,
                &self.activity_targets,
                series_follow_up::activity::Limits {
                    dwell_ms: self.dwell_ms,
                    passes: self.passes,
                    max_iq_bytes: self.max_iq_bytes,
                },
                stop,
            )?;
            plan.check_continuity = self.check_continuity;
            return Ok((plan, None));
        } else if let Some(path) = &self.series_triage_report {
            series_follow_up::read_plan(
                path,
                self.overlap_series_triage_report.as_deref(),
                self.dwell_ms,
                self.max_iq_bytes,
            )
        } else if let Some(path) = &self.triage_report {
            triage_follow_up::read_plan(path, self.dwell_ms, self.max_iq_bytes)
        } else if let Some(path) = &self.follow_up_plan {
            follow_up::read(path, self.dwell_ms, self.max_iq_bytes)
        } else if !self.center_hz.is_empty() {
            plan::targeted_with_dwells(
                &self.center_hz,
                self.dwell_ms,
                self.max_iq_bytes,
                &self.dwell_at,
            )
        } else if !self.bands.is_empty() {
            plan::targeted_bands(&self.bands, self.dwell_ms, self.max_iq_bytes)
        } else {
            plan::plan(
                self.lower_hz.context("sweep requires --lower-hz")?,
                self.upper_hz.context("sweep requires --upper-hz")?,
                self.dwell_ms,
                self.max_iq_bytes,
            )
        }?;
        if let Some(revisit_every) = self.revisit_every {
            plan = plan.with_priority(&self.priority_bands, revisit_every)?;
        }
        plan.check_continuity = self.check_continuity;
        let mut plan = plan.repeat(self.passes)?;
        let replay_profile = if self.replay {
            let profile = profile.context("--replay requires --signal-profile")?;
            let dwells = || {
                plan.visits()
                    .iter()
                    .map(|&center| (center, plan.samples_at(center)))
            };
            let inline = profile
                .has_inline_replay()
                .then(|| {
                    replay::preflight::bound(&profile, self.replay_limits, dwells(), plan.passes())
                })
                .transpose()?;
            let packets = (!profile.lora_packet_profile.is_empty())
                .then(|| {
                    packet_preflight::bound(
                        &profile.lora_packet_profile,
                        profile.packet_interpretations,
                        self.packet_limits(),
                        dwells(),
                        plan.passes(),
                    )
                })
                .transpose()?;
            plan.replay_preflight = inline;
            plan.packet_decode_preflight = packets;
            Some(profile)
        } else {
            None
        };
        Ok((plan, replay_profile))
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum SpectrumOutput {
    /// Write full per-tune spectra and activity summaries.
    Full,
    /// Include activity summaries in the sweep receipt only.
    Summary,
    /// Save validated IQ and metadata for later analysis.
    Skip,
}

impl SpectrumOutput {
    fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }
}

#[derive(Serialize)]
struct Tune {
    directory: String,
    requested_center_hz: u64,
    observed_center_hz: u64,
    observed_bands: [plan::Band; 2],
    #[serde(skip_serializing_if = "Option::is_none")]
    dwell_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_timing: Option<receiver::SampleTiming>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    spectrum: Option<TuneSpectrum>,
}

#[derive(Serialize)]
struct TuneSpectrum {
    spectrum_windows: usize,
    activity_regions: Vec<spectrum::ActivityRegion>,
}

struct TuneRequest {
    directory: String,
    path: PathBuf,
    settings: acquisition::Settings,
}

/// A terminal sweep operation and its frequency coverage contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) enum CompletedOperation {
    #[serde(rename = "sequential_rf_sweep")]
    Sequential,
    #[serde(rename = "targeted_rf_sweep")]
    Targeted,
}

#[derive(Serialize)]
struct CompletedSweep<'a> {
    schema_version: u32,
    operation: CompletedOperation,
    lower_hz: u64,
    upper_hz: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_bands: Option<&'a [plan::Band]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_band_dwells: Option<&'a [plan::BandDwell]>,
    antenna: u8,
    gain_db: f64,
    dwell_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    visit_centers_hz: Option<&'a [u64]>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    check_continuity: bool,
    iq_bytes: u64,
    #[serde(flatten)]
    timing: Elapsed,
    capture_cadence: CaptureCadence,
    #[serde(skip_serializing_if = "SpectrumOutput::is_full")]
    spectrum_output: SpectrumOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_triage_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_sweep_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_triage_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_triage_overlap_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_activity_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_sha256: Option<&'a str>,
    tunes: &'a [Tune],
}

impl<'a> CompletedSweep<'a> {
    fn new(
        plan: &'a plan::Plan,
        settings: acquisition::Settings,
        tunes: &'a [Tune],
        timing: Elapsed,
        native: Option<timing::Summary>,
        spectrum_output: SpectrumOutput,
    ) -> Self {
        let iq_bytes = plan.planned_iq_bytes / plan.passes() as u64;
        Self {
            schema_version: 1,
            operation: if plan.is_targeted() {
                CompletedOperation::Targeted
            } else {
                CompletedOperation::Sequential
            },
            lower_hz: plan.lower_hz,
            upper_hz: plan.upper_hz,
            requested_bands: plan.requested_bands.as_deref(),
            requested_band_dwells: plan.requested_band_dwells.as_deref(),
            antenna: settings.antenna,
            gain_db: settings.gain,
            dwell_ms: plan.dwell_ms,
            visit_centers_hz: plan.visit_centers_hz.as_deref(),
            check_continuity: plan.check_continuity,
            iq_bytes,
            timing,
            capture_cadence: CaptureCadence::new(
                iq_bytes,
                timing.acquisition_elapsed_seconds,
                native,
            ),
            spectrum_output,
            signal_profile_sha256: plan.signal_profile_sha256.as_deref(),
            source_triage_sha256: plan.source_triage_sha256.as_deref(),
            source_sweep_sha256: plan.source_sweep_sha256.as_deref(),
            source_series_triage_sha256: plan.source_series_triage_sha256.as_deref(),
            source_series_triage_overlap_sha256: plan
                .source_series_triage_overlap_sha256
                .as_deref(),
            source_series_activity_sha256: plan.source_series_activity_sha256.as_deref(),
            source_series_sha256: plan.source_series_sha256.as_deref(),
            tunes,
        }
    }
}

struct CaptureRun<'a> {
    output: &'a Path,
    plan: &'a plan::Plan,
    requests: &'a [TuneRequest],
    start: Instant,
    acquisition_elapsed: Duration,
    spectrum_output: SpectrumOutput,
}

#[derive(Clone, Copy, Serialize)]
struct Elapsed {
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acquisition_elapsed_seconds: Option<f64>,
    analysis_elapsed_seconds: f64,
}

#[derive(Serialize)]
struct CaptureCadence {
    nominal_iq_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nominal_iq_to_acquisition_wall_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native: Option<timing::Summary>,
}

impl CaptureCadence {
    fn new(
        iq_bytes: u64,
        acquisition_seconds: Option<f64>,
        native: Option<timing::Summary>,
    ) -> Self {
        let nominal_iq_seconds = iq_bytes as f64 / (4.0 * f64::from(crate::SAMPLE_RATE));
        Self {
            nominal_iq_seconds,
            nominal_iq_to_acquisition_wall_ratio: acquisition_seconds
                .filter(|&seconds| seconds > 0.0)
                .map(|seconds| nominal_iq_seconds / seconds),
            native,
        }
    }
}

/// Directory name within a sweep for a zero-based tune index.
pub(crate) fn tune_directory_name(index: usize) -> String {
    format!("tune-{:04}", index + 1)
}

pub(crate) fn print_plan(args: &Args, stop: &AtomicBool) -> Result<()> {
    let (plan, _) = args.prepare(stop)?;
    ensure!(!stop.load(Ordering::Relaxed), "sweep planning interrupted");
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &plan).context("write sweep plan")?;
    writeln!(stdout)?;
    stdout.flush().context("flush sweep plan")
}

/// Rebuild a saved series follow-up preview from its recorded inputs.
pub(crate) fn rebuild_series_follow_up_plan(
    report: &Path,
    overlap: Option<&Path>,
    dwell_ms: u64,
    max_iq_bytes: u64,
    passes: usize,
    check_continuity: bool,
) -> Result<serde_json::Value> {
    let mut plan =
        series_follow_up::read_plan(report, overlap, dwell_ms, max_iq_bytes)?.repeat(passes)?;
    plan.check_continuity = check_continuity;
    Ok(serde_json::to_value(plan)?)
}

/// Record all planned tunes, publish their terminal receipt, and replay if requested.
///
/// Validation, acquisition or cancellation failures retain the plan and any
/// completed prefix. A subsequent run needs a fresh output directory.
pub(crate) fn run(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    let (plan, replay_profile) = args.prepare(stop)?;
    let antenna = args.antenna.context("live sweep requires --antenna")?;
    ensure!(
        !plan.check_continuity || cfg!(all(target_os = "linux", target_env = "gnu")),
        "--check-continuity requires Linux GNU"
    );
    let settings = plan
        .visits()
        .iter()
        .map(|&center| {
            let duration = plan.dwell_at(center) as f64 / 1_000.0;
            let settings = acquisition::Settings::new(center, duration, antenna, args.gain)?;
            ensure!(
                settings.samples == plan.samples_at(center),
                "native acquisition count differs from sweep plan at {center} Hz"
            );
            Ok(settings)
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep interrupted before startup"
    );
    let worker = plan
        .packet_decode_preflight
        .as_ref()
        .filter(|bound| bound.maximum_attempts > 0)
        .map(|_| {
            let worker = args
                .lora_worker
                .as_deref()
                .unwrap_or(Path::new(DEFAULT_WORKER));
            Worker::open(&root.join(worker), stop)
        })
        .transpose()
        .context("admit LoRa packet worker before capture")?;
    let program = acquisition::resolve_program(root, args.receiver.as_deref())?;
    let output = directory(
        root,
        args.output.as_deref(),
        "captures/rf-observatory/sweep",
    )?;
    json_file(&output.join("plan.json"), &plan)?;
    let space = statvfs(&output).context("read sweep output filesystem capacity")?;
    let available_bytes = u128::from(space.blocks_available()) * u128::from(space.fragment_size());
    ensure!(
        u128::from(plan.planned_iq_bytes) <= available_bytes,
        "sweep needs {} bytes for IQ but output filesystem has {available_bytes} available",
        plan.planned_iq_bytes,
    );
    if let Some(packets) = &plan.packet_decode_preflight {
        let required_bytes =
            u128::from(plan.planned_iq_bytes) + u128::from(packets.peak_temporary_iq_bytes);
        ensure!(
            required_bytes <= available_bytes,
            "sweep needs {required_bytes} bytes for IQ and temporary LoRa replay but output filesystem has {available_bytes} available"
        );
    }

    let mut requests = Vec::with_capacity(settings.len() * plan.passes());
    for pass in 0..plan.passes() {
        let parent = if plan.passes() > 1 {
            output.join(series::pass_directory_name(pass))
        } else {
            output.clone()
        };
        requests.extend(
            settings
                .iter()
                .copied()
                .enumerate()
                .map(|(index, settings)| {
                    let directory = tune_directory_name(index);
                    TuneRequest {
                        path: parent.join(&directory),
                        directory,
                        settings,
                    }
                }),
        );
    }
    let start = Instant::now();
    let capture = native::capture(root, &program, &output, &requests, &plan, stop);
    let run = CaptureRun {
        output: &output,
        plan: &plan,
        requests: &requests,
        start,
        acquisition_elapsed: start.elapsed(),
        spectrum_output: args.spectrum_output,
    };
    let series = plan.passes() > 1;
    if series {
        run.finish_series(capture, stop)?;
    } else {
        finish_single(run, capture, stop)?;
    }
    if let Some(profile) = replay_profile {
        if !profile.lora_packet_profile.is_empty() {
            lora_decode_sweep::run_profile(
                &output,
                series,
                &profile,
                args.packet_limits(),
                worker.as_ref(),
                stop,
            )
            .with_context(|| format!("decode LoRa from completed sweep at {}", output.display()))?;
        }
        if profile.has_inline_replay() {
            replay::run_profile(&output, series, profile, args.replay_limits, stop)
                .with_context(|| format!("replay completed sweep at {}", output.display()))?;
        }
    }
    Ok(())
}

fn finish_single(run: CaptureRun<'_>, capture: Result<()>, stop: &AtomicBool) -> Result<()> {
    let CaptureRun {
        output,
        plan,
        requests,
        start,
        acquisition_elapsed,
        spectrum_output,
    } = run;
    let analysis_start = Instant::now();
    let analysis = analyze_prefix(requests, plan, spectrum_output, capture.is_ok(), stop);
    let analysis_elapsed_seconds = analysis_start.elapsed().as_secs_f64();
    let ((), tunes) = combine_capture_and_analysis(capture, analysis, stop)?;
    verify_coverage(plan, &tunes, output)?;
    let native = timing::read(output, requests, acquisition_elapsed, 1)
        .with_context(|| format!("validate native timing evidence in {}", output.display()))?
        .map(|evidence| evidence.summary);
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep interrupted; evidence in {}",
        output.display()
    );
    let elapsed = Elapsed {
        elapsed_seconds: Some(start.elapsed().as_secs_f64()),
        acquisition_elapsed_seconds: Some(acquisition_elapsed.as_secs_f64()),
        analysis_elapsed_seconds,
    };
    publish_receipt(
        output,
        "sweep.json",
        &CompletedSweep::new(
            plan,
            requests[0].settings,
            &tunes,
            elapsed,
            native,
            spectrum_output,
        ),
    )?;
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Completed {} sweep tunes. Output: {}",
        tunes.len(),
        output.display()
    )?;
    stdout.flush().context("flush sweep result")
}

fn combine_capture_and_analysis<C, T>(
    capture: Result<C>,
    analysis: Result<T>,
    stop: &AtomicBool,
) -> Result<(C, T)> {
    match (capture, analysis) {
        (Ok(capture), analysis) => analysis.map(|value| (capture, value)),
        (Err(primary), Err(secondary)) if !stop.load(Ordering::Relaxed) => {
            Err(primary.context(format!("analyze the completed sweep prefix: {secondary:#}")))
        }
        (Err(primary), _) => Err(primary),
    }
}

fn verify_coverage(plan: &plan::Plan, tunes: &[Tune], output: &Path) -> Result<()> {
    if let Some(follow_up) = &plan.activity_follow_up {
        follow_up
            .verify(
                tunes
                    .iter()
                    .map(|tune| (tune.requested_center_hz, tune.observed_bands)),
            )
            .with_context(|| {
                format!("verify activity follow-up geometry in {}", output.display())
            })?;
    }
    if let Some(priority) = &plan.priority {
        let priority_count = priority.centers_hz.len();
        for (index, group) in tunes
            .chunks(priority_count + priority.revisit_every)
            .enumerate()
        {
            priority
                .verify(
                    group
                        .iter()
                        .take(priority_count)
                        .map(|tune| (tune.observed_bands, tune.dwell_ms.unwrap_or(plan.dwell_ms))),
                )
                .with_context(|| {
                    format!(
                        "verify priority group {} in {}",
                        index + 1,
                        output.display()
                    )
                })?;
        }
    }
    if let Some(bands) = &plan.requested_bands {
        plan::verify_band_dwells(
            bands,
            plan.requested_band_dwells.as_deref().unwrap_or_default(),
            plan.dwell_ms,
            tunes
                .iter()
                .map(|tune| (tune.observed_bands, tune.dwell_ms.unwrap_or(plan.dwell_ms))),
        )
        .with_context(|| {
            format!(
                "verify complete requested bands; evidence in {}",
                output.display()
            )
        })?;
    }
    ensure!(
        plan.is_targeted()
            || plan::covers(
                plan.lower_hz,
                plan.upper_hz,
                tunes.iter().flat_map(|tune| tune.observed_bands)
            ),
        "observed sweep has a frequency coverage gap; evidence in {}",
        output.display()
    );
    Ok(())
}

fn analyze_prefix(
    requests: &[TuneRequest],
    plan: &plan::Plan,
    spectrum_output: SpectrumOutput,
    require_all: bool,
    stop: &AtomicBool,
) -> Result<Vec<Tune>> {
    let mut tunes = Vec::with_capacity(requests.len());
    for request in requests {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "sweep interrupted during analysis"
        );
        if request.path.try_exists()? {
            request.settings.write_request(&request.path)?;
        }
        if !acquisition::recording_ready(&request.path)? {
            ensure!(
                !require_all,
                "native sweep exited successfully without completing {} at {} Hz",
                request.directory,
                request.settings.center,
            );
            break;
        }
        tunes.push(
            request
                .inspect(plan, spectrum_output, stop)
                .with_context(|| {
                    format!(
                        "sweep {} at {} Hz",
                        request.directory, request.settings.center
                    )
                })?,
        );
    }
    Ok(tunes)
}

impl TuneRequest {
    fn inspect(
        &self,
        plan: &plan::Plan,
        output: SpectrumOutput,
        stop: &AtomicBool,
    ) -> Result<Tune> {
        let recording = acquisition::Recording::open(&self.path, self.settings)?;
        let sample_timing = recording.sample_timing();
        ensure!(
            sample_timing.is_some() == plan.check_continuity,
            "acquisition sample timing differs from the sweep continuity request"
        );
        let survey = (!matches!(output, SpectrumOutput::Skip))
            .then(|| recording.survey(stop))
            .transpose()?;
        recording.write_metadata()?;
        let spectrum = if let Some(survey) = survey {
            if output.is_full() {
                json_file(&self.path.join("spectrum.json"), &survey)?;
            }
            let activity_regions = survey
                .activity_detection
                .regions
                .into_iter()
                .filter(|region| {
                    plan.is_targeted()
                        || plan.priority.as_ref().is_some_and(|priority| {
                            priority
                                .centers_hz
                                .binary_search(&self.settings.center)
                                .is_ok()
                        })
                        || (region.lower_frequency_hz < plan.upper_hz as f64
                            && region.upper_frequency_hz > plan.lower_hz as f64)
                })
                .collect();
            Some(TuneSpectrum {
                spectrum_windows: survey.windows,
                activity_regions,
            })
        } else {
            None
        };
        let observed_center_hz = recording.center_hz();
        Ok(Tune {
            directory: self.directory.clone(),
            requested_center_hz: self.settings.center,
            observed_center_hz,
            observed_bands: plan::observed_bands(observed_center_hz, 0),
            dwell_ms: plan.dwell_ms_by_center.get(&self.settings.center).copied(),
            sample_timing,
            spectrum,
        })
    }
}

fn publish_receipt(output: &Path, name: &str, receipt: &impl Serialize) -> Result<File> {
    let mut staged = tempfile::NamedTempFile::new_in(output).context("stage sweep receipt")?;
    {
        let mut writer = BufWriter::new(staged.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, receipt)?;
        writeln!(writer)?;
        writer.flush()?;
    }
    staged
        .as_file()
        .sync_all()
        .context("sync staged sweep receipt")?;
    let file = staged
        .persist_noclobber(output.join(name))
        .context("publish sweep receipt")?;
    File::open(output)?
        .sync_all()
        .context("sync sweep output directory")?;
    Ok(file)
}
