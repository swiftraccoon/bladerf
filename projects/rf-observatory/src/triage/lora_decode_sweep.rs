//! Decode labeled LoRa packet profiles from individual saved sweep dwells.

use std::fs::{self, DirBuilder, File, Metadata};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::lora::decode::{
    DEFAULT_MAX_FRAMES, DEFAULT_TIMEOUT_SECONDS, DEFAULT_WORKER, Decoded, DwellRequest,
    DwellSource, DwellSourceWork, MAX_FRAMES, MAX_TIMEOUT_SECONDS, PacketInterpretation,
    PacketInterpretations, PacketProfile, PreparedDwell, Worker, prepare_dwell_channel,
};
use crate::private_output::StagedDirectory;
use crate::signal_profile::{self, SignalProfile};
use crate::{MAX_SAMPLES, SAMPLE_RATE, file_pin};

use super::admission::{CompletedCapture, PassRef, Tune};
use super::check_stop;
use super::lora_sweep::MAX_TOTAL_PROFILE_SAMPLES;

const MAX_PROFILES: usize = 32;
/// Default maximum number of eligible profile and dwell pairs.
pub(crate) const DEFAULT_ATTEMPTS: u32 = 64;
/// Supported maximum number of eligible profile and dwell pairs.
pub(crate) const MAX_ATTEMPTS: u32 = 256;
const CF32_SAMPLE_BYTES: u64 = 8;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const _: () = assert!(1_000_000_000u32.is_multiple_of(SAMPLE_RATE));
const SWEEP_RECEIPT: &str = "decode-sweep.json";
const DECODE_RECEIPT: &str = "decode.json";
const WORKER_RECEIPT: &str = "worker.json";

/// Select packet profiles and worker budgets for a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Use the LoRa packet selections from a versioned signal profile.
    #[arg(long)]
    signal_profile: Option<PathBuf>,
    /// Profile with decimal sync byte. Repeat for up to 32 distinct profiles.
    #[arg(long, required_unless_present = "signal_profile", conflicts_with = "signal_profile", value_name = "CENTER_HZ:BANDWIDTH_HZ:SF:SYNC", value_parser = PacketProfile::parse)]
    profile: Vec<PacketProfile>,
    #[command(flatten)]
    interpretations: PacketInterpretations,
    #[command(flatten)]
    limits: PacketReplayLimits,
    /// Prebuilt LoRa file worker executable.
    #[arg(long, default_value = DEFAULT_WORKER)]
    worker: PathBuf,
    /// Print the decode plan without reading IQ or opening the worker.
    #[arg(long)]
    plan_only: bool,
    /// New private directory for the sweep receipt and verified attempt reports.
    #[arg(
        long,
        required_unless_present = "plan_only",
        conflicts_with = "plan_only"
    )]
    output: Option<PathBuf>,
}

/// Sample, attempt and worker limits for offline LoRa packet replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::Args)]
pub(crate) struct PacketReplayLimits {
    /// Maximum original-rate samples to decode from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    pub(crate) max_samples_per_dwell: u64,
    /// Maximum aggregate original-rate samples, charged for each eligible profile.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_PROFILE_SAMPLES))]
    pub(crate) max_total_profile_samples: u64,
    /// Maximum number of profile and dwell pairs sent to the worker.
    #[arg(long, default_value_t = DEFAULT_ATTEMPTS, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_ATTEMPTS)))]
    pub(crate) max_attempts: u32,
    /// Maximum number of verified frames to retain per worker attempt.
    #[arg(long, default_value_t = DEFAULT_MAX_FRAMES, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_FRAMES)))]
    pub(crate) max_frames: u32,
    /// Time limit in seconds for each worker process after channelization.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS, value_parser = clap::value_parser!(u64).range(1..=MAX_TIMEOUT_SECONDS))]
    pub(crate) timeout_seconds: u64,
}

impl Default for PacketReplayLimits {
    fn default() -> Self {
        Self {
            max_samples_per_dwell: MAX_SAMPLES,
            max_total_profile_samples: MAX_SAMPLES,
            max_attempts: DEFAULT_ATTEMPTS,
            max_frames: DEFAULT_MAX_FRAMES,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
        }
    }
}

/// Sort packet profiles and remove exact duplicates, retaining an empty selection.
///
/// # Errors
/// Rejects more than 32 distinct profiles.
pub(crate) fn distinct_profiles(mut profiles: Vec<PacketProfile>) -> Result<Vec<PacketProfile>> {
    profiles.sort_unstable();
    profiles.dedup();
    ensure!(
        profiles.len() <= MAX_PROFILES,
        "select 1 through {MAX_PROFILES} distinct LoRa packet profiles"
    );
    Ok(profiles)
}

#[derive(Serialize)]
struct Coverage {
    #[serde(flatten)]
    profile: PacketProfile,
    minimum_samples: u64,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    covered_samples: u64,
    replayed_samples: u64,
    covered_time_ns: u64,
    replayed_time_ns: u64,
}

impl Coverage {
    fn new(profile: PacketProfile) -> Self {
        Self {
            profile,
            minimum_samples: profile.minimum_samples(),
            covered_dwells: 0,
            short_dwells: 0,
            replayed_dwells: 0,
            covered_samples: 0,
            replayed_samples: 0,
            covered_time_ns: 0,
            replayed_time_ns: 0,
        }
    }

    fn record(&mut self, saved_samples: u64, selected_samples: u64) -> bool {
        self.covered_dwells += 1;
        self.covered_samples += saved_samples;
        self.covered_time_ns += saved_samples * NANOSECONDS_PER_SAMPLE;
        if selected_samples < self.minimum_samples {
            self.short_dwells += 1;
            return false;
        }
        self.replayed_dwells += 1;
        self.replayed_samples += selected_samples;
        self.replayed_time_ns += selected_samples * NANOSECONDS_PER_SAMPLE;
        true
    }
}

#[derive(Clone, Copy)]
struct Route<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    profile: PacketProfile,
    interpretation: PacketInterpretation,
    selected_samples: u64,
}

struct Plan<'a> {
    limits: PacketReplayLimits,
    interpretations: PacketInterpretations,
    pass_count: usize,
    dwell_count: usize,
    profile_samples: u64,
    profiles: Vec<Coverage>,
    routes: Vec<Route<'a>>,
}

#[derive(Serialize)]
struct Preview<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meshtastic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lorawan_join: Option<bool>,
    pass_count: usize,
    dwell_count: usize,
    attempt_count: usize,
    profile_samples: u64,
    channelization_count: usize,
    source_iq_bytes: u64,
    max_buffered_iq_bytes: u64,
    peak_temporary_iq_bytes: u64,
    profiles: &'a [Coverage],
}

impl<'a> Plan<'a> {
    fn build(
        passes: &[PassRef<'a>],
        profiles: Vec<PacketProfile>,
        interpretations: PacketInterpretations,
        limits: PacketReplayLimits,
        stop: &AtomicBool,
    ) -> Result<Self> {
        let mut profiles: Vec<_> = profiles.into_iter().map(Coverage::new).collect();
        let mut routes = Vec::new();
        for (index, pass) in passes.iter().enumerate() {
            for tune in &pass.sweep.tunes {
                check_stop(stop)?;
                let selected_samples = tune.samples.min(limits.max_samples_per_dwell);
                for coverage in &mut profiles {
                    let profile = coverage.profile;
                    if profile.is_covered(tune.observed_center_hz)
                        && coverage.record(tune.samples, selected_samples)
                    {
                        ensure!(
                            routes.len() < limits.max_attempts as usize,
                            "LoRa sweep decode exceeds --max-attempts ({})",
                            limits.max_attempts
                        );
                        routes.push(Route {
                            pass_index: index + 1,
                            pass_directory: pass.directory,
                            source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                            tune,
                            profile,
                            interpretation: interpretations.select(profile),
                            selected_samples,
                        });
                    }
                }
            }
        }
        let profile_samples = profiles
            .iter()
            .map(|profile| profile.replayed_samples)
            .sum();
        ensure!(
            profile_samples <= limits.max_total_profile_samples,
            "LoRa sweep decode needs {profile_samples} profile samples, exceeding --max-total-profile-samples ({})",
            limits.max_total_profile_samples
        );
        Ok(Self {
            limits,
            interpretations,
            pass_count: passes.len(),
            dwell_count: passes.iter().map(|pass| pass.sweep.tunes.len()).sum(),
            profile_samples,
            profiles,
            routes,
        })
    }

    fn dwell_groups(&self) -> impl Iterator<Item = &[Route<'a>]> {
        self.routes.chunk_by(Route::shares_dwell)
    }

    fn print(
        &self,
        capture: &CompletedCapture,
        signal_profile_sha256: Option<&str>,
        stop: &AtomicBool,
    ) -> Result<()> {
        capture.verify()?;
        check_stop(stop)?;
        let mut channelization_count = 0;
        let mut source_iq_bytes = 0;
        let mut max_buffered_iq_bytes = 0;
        let mut peak_temporary_iq_bytes = 0;
        for dwell in self.dwell_groups() {
            let mut channels = 0;
            for group in dwell.chunk_by(Route::shares_channel) {
                channels += 1;
                let first = &group[0];
                let temporary_bytes =
                    first.profile.channelized_samples(first.selected_samples) * CF32_SAMPLE_BYTES;
                peak_temporary_iq_bytes = peak_temporary_iq_bytes.max(temporary_bytes);
            }
            channelization_count += channels;
            let work = DwellSourceWork::new(dwell[0].selected_samples, channels);
            source_iq_bytes += work.source_iq_bytes;
            max_buffered_iq_bytes = max_buffered_iq_bytes.max(work.buffered_iq_bytes);
        }
        let preview = Preview {
            schema_version: 1,
            operation: "plan_lora_decode_sweep",
            capture_kind: capture.kind(),
            source_capture_sha256: capture.source_receipt_sha256(),
            signal_profile_sha256,
            meshtastic: self.interpretations.meshtastic.then_some(true),
            lorawan_join: self.interpretations.lorawan_join.then_some(true),
            pass_count: self.pass_count,
            dwell_count: self.dwell_count,
            attempt_count: self.routes.len(),
            profile_samples: self.profile_samples,
            channelization_count,
            source_iq_bytes,
            max_buffered_iq_bytes,
            peak_temporary_iq_bytes,
            profiles: &self.profiles,
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &preview)
            .context("write LoRa sweep decode plan")?;
        writeln!(stdout)?;
        stdout.flush().context("flush LoRa sweep decode plan")
    }
}

struct CompletedAttempt<'a> {
    route: Route<'a>,
    directory: String,
    directory_stamp: Metadata,
    decoded: Decoded,
}

impl<'a> Route<'a> {
    fn shares_dwell(&self, other: &Self) -> bool {
        self.pass_index == other.pass_index
            && std::ptr::eq(self.tune, other.tune)
            && self.selected_samples == other.selected_samples
    }

    fn shares_channel(&self, other: &Self) -> bool {
        self.profile.channel_key() == other.profile.channel_key()
    }

    fn prepare(
        &self,
        source: &mut DwellSource,
        stage: &Path,
        limits: PacketReplayLimits,
        stop: &AtomicBool,
    ) -> Result<PreparedDwell> {
        check_stop(stop)?;
        prepare_dwell_channel(
            source,
            stage,
            DwellRequest {
                center_hz: self.tune.observed_center_hz,
                profile: self.profile,
                source_samples: self.tune.samples,
                analyzed_samples: self.selected_samples,
                max_frames: limits.max_frames,
                timeout_seconds: limits.timeout_seconds,
            },
            |descriptor| self.tune.verify_iq(descriptor),
            stop,
        )
    }

    fn decode(
        self,
        stage: &Path,
        index: usize,
        worker: &Worker,
        prepared: &PreparedDwell,
        stop: &AtomicBool,
    ) -> Result<CompletedAttempt<'a>> {
        check_stop(stop)?;
        let directory = format!("attempt-{index:04}");
        let path = stage.join(&directory);
        DirBuilder::new().mode(0o700).create(&path)?;
        let directory_handle = File::open(&path)?;
        let decoded = worker.decode_prepared_dwell(
            prepared,
            &path,
            self.profile,
            self.interpretation,
            stop,
        )?;
        let directory_stamp = directory_handle.metadata()?;
        verify_path(&path, &directory_stamp)?;
        Ok(CompletedAttempt {
            route: self,
            directory,
            directory_stamp,
            decoded,
        })
    }
}

impl CompletedAttempt<'_> {
    fn report(&self) -> AttemptReport<'_> {
        let route = &self.route;
        AttemptReport {
            pass_index: route.pass_index,
            pass_directory: route.pass_directory,
            tune_directory: &route.tune.directory,
            source_sweep_sha256: route.source_sweep_sha256,
            source_sidecar_sha256: &route.tune.sidecar_sha256,
            observed_center_hz: route.tune.observed_center_hz,
            saved_samples: route.tune.samples,
            selected_samples: route.selected_samples,
            profile: route.profile,
            directory: &self.directory,
            decode_receipt: format!("{}/{DECODE_RECEIPT}", self.directory),
            worker_receipt: format!("{}/{WORKER_RECEIPT}", self.directory),
            decode_receipt_sha256: self.decoded.decode_receipt_sha256(),
            worker_receipt_sha256: self.decoded.worker_receipt_sha256(),
            validated_frames: self.decoded.validated_frames(),
            omitted_valid_frames: self.decoded.omitted_valid_frames(),
            meshtastic_headers: self.decoded.meshtastic_frames(),
            lorawan_join_requests: self.decoded.lorawan_join_requests(),
        }
    }

    fn verify(&self, stage: &Path) -> Result<()> {
        let path = stage.join(&self.directory);
        verify_path(&path, &self.directory_stamp)?;
        require_entry_count(&path, 2)?;
        self.decoded.verify(&path)
    }
}

#[derive(Serialize)]
struct AttemptReport<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune_directory: &'a str,
    source_sweep_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    #[serde(flatten)]
    profile: PacketProfile,
    directory: &'a str,
    decode_receipt: String,
    worker_receipt: String,
    decode_receipt_sha256: &'a str,
    worker_receipt_sha256: &'a str,
    validated_frames: usize,
    omitted_valid_frames: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    meshtastic_headers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lorawan_join_requests: Option<usize>,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<&'a str>,
    sample_rate_hz: u32,
    max_samples_per_dwell: u64,
    max_total_profile_samples: u64,
    max_attempts: u32,
    max_frames: u32,
    timeout_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_sha256: Option<&'a str>,
    pass_count: usize,
    dwell_count: usize,
    attempt_count: usize,
    profile_samples: u64,
    validated_frames: usize,
    omitted_valid_frames: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    meshtastic_headers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lorawan_join_requests: Option<usize>,
    profiles: Vec<Coverage>,
    attempts: Vec<AttemptReport<'a>>,
}

fn require_entry_count(path: &Path, expected: usize) -> Result<()> {
    let count = fs::read_dir(path)?
        .take(expected + 1)
        .try_fold(0, |count, entry| entry.map(|_| count + 1))?;
    ensure!(
        count == expected,
        "unexpected entries in LoRa decode output {}",
        path.display()
    );
    Ok(())
}

fn verify_path(path: &Path, stamp: &Metadata) -> Result<()> {
    ensure!(
        file_pin::same_file(stamp, &path.symlink_metadata()?),
        "LoRa decode output changed before publication: {}",
        path.display()
    );
    Ok(())
}

/// Plan or decode each eligible dwell and profile. Decode shares a bounded
/// source prefix across distinct channels and a translation across profiles
/// on the same channel, then publishes verified receipts together.
///
/// # Errors
/// Rejects invalid or changed sources, work beyond the supplied budgets,
/// worker, evidence or publication failures, and interruption.
pub(crate) fn decode(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let (profiles, interpretations, signal_profile_sha256) = if let Some(path) = args.signal_profile
    {
        let profile = signal_profile::read(&root.join(path))?;
        ensure!(
            !profile.lora_packet_profile.is_empty(),
            "signal profile has no lora_packet_profile selections"
        );
        let selected = profile.packet_interpretations;
        (
            profile.lora_packet_profile,
            PacketInterpretations {
                meshtastic: args.interpretations.meshtastic || selected.meshtastic,
                lorawan_join: args.interpretations.lorawan_join || selected.lorawan_join,
            },
            Some(profile.sha256),
        )
    } else {
        let profiles = distinct_profiles(args.profile)?;
        ensure!(
            !profiles.is_empty(),
            "select 1 through {MAX_PROFILES} distinct LoRa packet profiles"
        );
        (profiles, args.interpretations, None)
    };
    interpretations.validate_profiles(&profiles)?;
    let capture = CompletedCapture::open(&root.join(&args.capture_directory), args.series)?;
    let plan = Plan::build(
        &capture.passes(),
        profiles,
        interpretations,
        args.limits,
        stop,
    )?;
    if args.plan_only {
        return plan.print(&capture, signal_profile_sha256.as_deref(), stop);
    }
    let output = args
        .output
        .as_ref()
        .context("LoRa sweep decode requires --output")?;
    let worker = (!plan.routes.is_empty())
        .then(|| Worker::open(&root.join(&args.worker), stop))
        .transpose()?;
    execute(
        &capture,
        plan,
        &root.join(output),
        signal_profile_sha256.as_deref(),
        worker.as_ref(),
        stop,
    )
}

/// Publish profile-selected LoRa packet evidence inside a completed capture.
///
/// The caller admits any required worker before capture. The profile digest
/// identifies the JSON selections. A plan with no eligible attempts can run
/// without a worker.
///
/// # Errors
/// Rejects invalid or changed sources, exhausted replay budgets, a missing or
/// changed required worker, interruption, and publication failures.
pub(crate) fn run_profile(
    capture_path: &Path,
    series: bool,
    profile: &SignalProfile,
    limits: PacketReplayLimits,
    worker: Option<&Worker>,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    profile
        .packet_interpretations
        .validate_profiles(&profile.lora_packet_profile)?;
    let capture = CompletedCapture::open(capture_path, series)?;
    let plan = Plan::build(
        &capture.passes(),
        profile.lora_packet_profile.clone(),
        profile.packet_interpretations,
        limits,
        stop,
    )?;
    execute(
        &capture,
        plan,
        &capture_path.join("lora-packets"),
        Some(&profile.sha256),
        worker,
        stop,
    )
}

fn execute(
    capture: &CompletedCapture,
    plan: Plan<'_>,
    destination: &Path,
    signal_profile_sha256: Option<&str>,
    worker: Option<&Worker>,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    let worker = if plan.routes.is_empty() {
        None
    } else {
        let worker = worker.context("LoRa sweep decode requires a worker for eligible attempts")?;
        worker.verify()?;
        Some(worker)
    };
    let stage = StagedDirectory::new(destination, ".lora-decode-sweep-")?;
    let stage_path = stage.path().to_path_buf();
    let mut completed = Vec::with_capacity(plan.routes.len());
    if let Some(worker) = worker {
        for dwell in plan.dwell_groups() {
            let first = &dwell[0];
            let pass_index = first.pass_index;
            let tune_directory = &first.tune.directory;
            let channels = dwell.chunk_by(Route::shares_channel).count();
            let work = DwellSourceWork::new(first.selected_samples, channels);
            check_stop(stop)?;
            let mut source = DwellSource::new(first.tune.open_iq()?, work.buffered_samples, stop)
                .with_context(|| {
                format!("read LoRa IQ for pass {pass_index}/{tune_directory}")
            })?;
            for group in dwell.chunk_by(Route::shares_channel) {
                let prepared = group[0]
                    .prepare(&mut source, &stage_path, plan.limits, stop)
                    .with_context(|| {
                        format!("channelize LoRa for pass {pass_index}/{tune_directory}")
                    })?;
                for route in group.iter().copied() {
                    let attempt = route
                        .decode(&stage_path, completed.len() + 1, worker, &prepared, stop)
                        .with_context(|| {
                            format!("decode LoRa for pass {pass_index}/{tune_directory}")
                        })?;
                    completed.push(attempt);
                }
                prepared.close()?;
            }
        }
    }

    let attempts: Vec<_> = completed.iter().map(CompletedAttempt::report).collect();
    let validated_frames = attempts
        .iter()
        .map(|attempt| attempt.validated_frames)
        .sum();
    let omitted_valid_frames = attempts
        .iter()
        .map(|attempt| attempt.omitted_valid_frames)
        .sum();
    let meshtastic_headers = plan.interpretations.meshtastic.then(|| {
        attempts
            .iter()
            .filter_map(|attempt| attempt.meshtastic_headers)
            .sum()
    });
    let lorawan_join_requests = plan.interpretations.lorawan_join.then(|| {
        attempts
            .iter()
            .filter_map(|attempt| attempt.lorawan_join_requests)
            .sum()
    });
    let attempt_count = attempts.len();
    let report = Report {
        schema_version: 1,
        operation: "decode_lora_in_saved_sweeps",
        capture_kind: capture.kind(),
        source_capture_sha256: capture.source_receipt_sha256(),
        signal_profile_sha256,
        sample_rate_hz: SAMPLE_RATE,
        max_samples_per_dwell: plan.limits.max_samples_per_dwell,
        max_total_profile_samples: plan.limits.max_total_profile_samples,
        max_attempts: plan.limits.max_attempts,
        max_frames: plan.limits.max_frames,
        timeout_seconds: plan.limits.timeout_seconds,
        worker_sha256: worker.map(Worker::sha256),
        pass_count: plan.pass_count,
        dwell_count: plan.dwell_count,
        attempt_count,
        profile_samples: plan.profile_samples,
        validated_frames,
        omitted_valid_frames,
        meshtastic_headers,
        lorawan_join_requests,
        profiles: plan.profiles,
        attempts,
    };
    let receipt_path = stage_path.join(SWEEP_RECEIPT);
    crate::json_file(&receipt_path, &report)?;
    let receipt_stamp = receipt_path.symlink_metadata()?;
    stage.publish(|| {
        capture.verify()?;
        if let Some(worker) = worker {
            worker.verify()?;
        }
        require_entry_count(&stage_path, attempt_count + 1)?;
        for attempt in &completed {
            attempt.verify(&stage_path)?;
        }
        verify_path(&receipt_path, &receipt_stamp)?;
        check_stop(stop)
    })?;
    println!(
        "Validated {validated_frames} LoRa frames across {attempt_count} attempts. Output: {}",
        destination.display()
    );
    Ok(())
}
