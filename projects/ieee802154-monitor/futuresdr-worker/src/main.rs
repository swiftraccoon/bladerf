//! Bounded file-only FutureSDR IEEE 802.15.4 demodulation worker.
//!
//! This process has no live-radio, network, transmit or control-port surface.
//! Its raw candidates are not CRC validation, protocol identity or RF timestamps.

#![forbid(unsafe_code)]

mod archive;
#[cfg(test)]
mod burst_clock;
#[cfg(test)]
mod burst_detector;
#[cfg(test)]
mod decoder_test;
mod soft_decoder;
mod source;
mod source_index;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail, ensure};
use archive::{AppendOutcome, ArchiveWriter};
#[cfg(test)]
use burst_clock::{
    BurstClockRecovery, BurstClockStats, DeferredShrRateClockStats, ShrRateClockStats,
    ShrRateDiagnosticTrace,
};
use clap::{Parser, ValueEnum};
use futuresdr::blocks::{Apply, MessagePipe};
use futuresdr::prelude::*;
use futuresdr::runtime::scheduler::SmolScheduler;
use soft_decoder::SoftDsssDecoder;
#[cfg(test)]
use soft_decoder::{PositionedDecoderTraceStats, PositionedSoftDsssDecoder};
use source::{Cf32LeSource, validate_cf32_le};
use source_index::SourceIndexWriter;
use zigbee::{ClockRecoveryMm, Decoder};

const MAX_LIMIT_MIB: u32 = 8192;
const MAX_FRAMES: u64 = 10_000_000;
const CHANNEL_SAMPLE_RATE_HZ: i32 = 4_000_000;
const MAX_ABS_CARRIER_OFFSET_HZ: i32 = 100_000;
const ESTABLISHED_CLOCK_MU: f32 = 0.5;
const ESTABLISHED_CLOCK_GAIN_OMEGA: f32 = 0.000225;
const ESTABLISHED_CLOCK_GAIN_MU: f32 = 0.03;
#[cfg(test)]
const SLOW_CLOCK_GAIN_OMEGA: f32 = 0.00005625;
#[cfg(test)]
const SLOW_CLOCK_GAIN_MU: f32 = 0.015;
#[cfg(test)]
const CLOCK_GAIN_MU_SWEEP: [f32; 4] = [0.0075, 0.015, 0.03, 0.06];
#[cfg(test)]
const CLOCK_GAIN_OMEGA_SWEEP: [f32; 4] = [0.0000140625, 0.00005625, 0.000225, 0.0009];
#[cfg(test)]
const FIXED_CLOCK_RATE_ERROR_PPM: [i32; 2] = [-100, 100];
#[cfg(test)]
const CLOCK_RATE_PPM_DENOMINATOR: f32 = 1_000_000.0;
const MESSAGE_CAPACITY: usize = 64;
const RUNTIME_BUFFER_BYTES: u64 = 64 * 1024;
const RUNTIME_QUEUE_CAPACITY: u64 = 64;
const RUNTIME_STACK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum DecoderLane {
    #[default]
    Hard,
    Soft,
}

impl DecoderLane {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CarrierOffsetHz(i32);

impl CarrierOffsetHz {
    fn new(value: i32) -> Result<Self> {
        ensure!(
            (-MAX_ABS_CARRIER_OFFSET_HZ..=MAX_ABS_CARRIER_OFFSET_HZ).contains(&value),
            "carrier-offset-hz must be within +/-{MAX_ABS_CARRIER_OFFSET_HZ}"
        );
        Ok(Self(value))
    }

    const fn get(self) -> i32 {
        self.0
    }
}

#[derive(Debug)]
struct CarrierCorrector {
    measured_offset_hz: i32,
    phase_numerator: i32,
}

impl CarrierCorrector {
    fn new(measured_offset: CarrierOffsetHz) -> Self {
        Self {
            measured_offset_hz: measured_offset.get(),
            phase_numerator: 0,
        }
    }

    fn advance_phase(&mut self) {
        self.phase_numerator =
            (self.phase_numerator - self.measured_offset_hz).rem_euclid(CHANNEL_SAMPLE_RATE_HZ);
    }

    fn correct(&mut self, input: &Complex32) -> Complex32 {
        let phase = std::f64::consts::TAU * f64::from(self.phase_numerator)
            / f64::from(CHANNEL_SAMPLE_RATE_HZ);
        let (sin, cos) = phase.sin_cos();
        let input_re = f64::from(input.re);
        let input_im = f64::from(input.im);
        self.advance_phase();
        Complex32::new(
            (input_re * cos - input_im * sin) as f32,
            (input_re * sin + input_im * cos) as f32,
        )
    }
}

#[derive(Debug, Parser)]
#[command(about = "Bounded file-only FutureSDR IEEE 802.15.4 demodulation worker")]
struct Args {
    /// Complete little-endian CF32 file at 4 MSps, centered on `--channel`.
    input: PathBuf,
    /// New raw-candidate archive path; OUTPUT.partial is retained on failure.
    #[arg(long)]
    output: PathBuf,
    /// New I154IDX sidecar with input sample horizons for hard-decoder candidates.
    #[arg(long)]
    source_index_output: Option<PathBuf>,
    /// IEEE 802.15.4 2.4 GHz channel number.
    #[arg(long)]
    channel: u8,
    /// Maximum accepted CF32 input size in MiB.
    #[arg(long)]
    max_input_mib: u32,
    /// Maximum retained raw PSDU candidates.
    #[arg(long)]
    max_frames: u64,
    /// Maximum candidate archive size in MiB.
    #[arg(long)]
    max_output_mib: u32,
    /// Recovered-chip decoder lane; hard preserves the original worker path.
    #[arg(long, value_enum, default_value_t = DecoderLane::Hard)]
    decoder_lane: DecoderLane,
    /// Measured input carrier offset in Hz; the worker applies the opposite rotation.
    #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
    carrier_offset_hz: i32,
    /// Number of unpinned executor threads for this worker.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=8))]
    executor_threads: Option<u8>,
}

impl Args {
    fn validate(&self) -> Result<(u64, u64, CarrierOffsetHz)> {
        ensure!(
            cfg!(target_endian = "little"),
            "worker requires a little-endian host"
        );
        ensure!((11..=26).contains(&self.channel), "channel must be 11..=26");
        ensure!(
            (1..=MAX_LIMIT_MIB).contains(&self.max_input_mib),
            "max-input-mib must be 1..={MAX_LIMIT_MIB}"
        );
        ensure!(
            (1..=MAX_LIMIT_MIB).contains(&self.max_output_mib),
            "max-output-mib must be 1..={MAX_LIMIT_MIB}"
        );
        ensure!(
            (1..=MAX_FRAMES).contains(&self.max_frames),
            "max-frames must be 1..={MAX_FRAMES}"
        );
        if let Some(index) = &self.source_index_output {
            ensure!(
                self.decoder_lane == DecoderLane::Hard,
                "source-index-output requires the hard decoder lane"
            );
            validate_index_paths(&self.input, &self.output, index)?;
        }
        Ok((
            u64::from(self.max_input_mib) * 1024 * 1024,
            u64::from(self.max_output_mib) * 1024 * 1024,
            CarrierOffsetHz::new(self.carrier_offset_hz)?,
        ))
    }
}

fn output_slot(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("output path needs a filename")?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(directory.canonicalize()?.join(name))
}

fn validate_index_paths(input: &Path, archive: &Path, index: &Path) -> Result<()> {
    let input = input.canonicalize().context("resolve CF32 input")?;
    let archive = output_slot(archive)?;
    let index = output_slot(index)?;
    let slots = [
        archive::append_suffix(&archive, ".partial"),
        archive,
        archive::append_suffix(&index, ".partial"),
        index,
    ];
    ensure!(
        slots.iter().collect::<BTreeSet<_>>().len() == slots.len()
            && slots.iter().all(|slot| *slot != input),
        "archive, source index, staging paths and input must be distinct"
    );
    for slot in slots {
        match slot.symlink_metadata() {
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {}", slot.display())),
            Ok(_) => bail!("output or partial already exists: {}", slot.display()),
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopReason {
    InputEof,
    FrameLimit,
    OutputLimit,
}

#[derive(Debug)]
struct CollectorResult {
    archive: ArchiveWriter,
    source_index: Option<SourceIndexWriter>,
    reason: StopReason,
}

#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    chip_count: u64,
    required_through: Option<u64>,
    psdu: Vec<u8>,
}

fn indexed_candidate(message: Pmt, indexed_source: bool) -> Result<Candidate> {
    let Pmt::VecPmt(mut fields) = message else {
        bail!("decoder indexed output was not a PMT vector");
    };
    ensure!(
        fields.len() == 2 + usize::from(indexed_source),
        "decoder indexed output field count differs from the selected lane"
    );
    let payload = fields.pop().expect("checked field count");
    let required_through = if indexed_source {
        let Some(Pmt::U64(index)) = fields.pop() else {
            bail!("decoder source horizon was not U64");
        };
        Some(index)
    } else {
        None
    };
    let position = fields.pop().expect("checked field count");
    let Pmt::U64(chip_count) = position else {
        bail!("decoder indexed position was not U64");
    };
    let Pmt::Blob(psdu) = payload else {
        bail!("decoder indexed payload was not a byte blob");
    };
    Ok(Candidate {
        chip_count,
        required_through,
        psdu,
    })
}

async fn collect_candidates(
    receiver: mpsc::Receiver<Pmt>,
    mut archive: ArchiveWriter,
    mut source_index: Option<SourceIndexWriter>,
    handle: FlowgraphHandle,
) -> Result<CollectorResult> {
    loop {
        let Some(message) = receiver.recv().await else {
            let _ = handle.stop().await;
            bail!("decoder message channel closed before Pmt::Finished");
        };
        if matches!(message, Pmt::Finished) {
            let _ = handle.stop().await;
            return Ok(CollectorResult {
                archive,
                source_index,
                reason: StopReason::InputEof,
            });
        }
        let append = (|| -> Result<AppendOutcome> {
            let candidate = indexed_candidate(message, source_index.is_some())?;
            let before = archive.records();
            let outcome = archive.append(candidate.chip_count, &candidate.psdu)?;
            if archive.records() != before
                && let Some(writer) = &mut source_index
            {
                writer.append(
                    candidate.chip_count,
                    candidate
                        .required_through
                        .context("missing source horizon")?,
                )?;
            }
            Ok(outcome)
        })();
        let outcome = match append {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = handle.stop().await;
                return Err(error);
            }
        };
        let reason = match outcome {
            AppendOutcome::Continue => continue,
            AppendOutcome::FrameLimit => StopReason::FrameLimit,
            AppendOutcome::OutputLimit => StopReason::OutputLimit,
        };
        let _ = handle.stop().await;
        return Ok(CollectorResult {
            archive,
            source_index,
            reason,
        });
    }
}

fn configure_runtime() {
    futuresdr::runtime::config::set("buffer_size", RUNTIME_BUFFER_BYTES);
    futuresdr::runtime::config::set("queue_size", RUNTIME_QUEUE_CAPACITY);
    futuresdr::runtime::config::set("stack_size", RUNTIME_STACK_BYTES);
    futuresdr::runtime::config::set("ctrlport_enable", false);
    futuresdr::runtime::config::set("log_level", "info");
}

fn run(args: Args) -> Result<()> {
    run_with_clock_mu(args, ESTABLISHED_CLOCK_MU)
}

fn run_with_clock_mu(args: Args, initial_clock_mu: f32) -> Result<()> {
    run_with_clock_parameters(
        args,
        2.0,
        initial_clock_mu,
        ESTABLISHED_CLOCK_GAIN_OMEGA,
        ESTABLISHED_CLOCK_GAIN_MU,
    )
}

#[cfg(test)]
fn run_with_fixed_clock_mu(args: Args, initial_clock_mu: f32) -> Result<()> {
    run_with_clock_parameters(args, 2.0, initial_clock_mu, 0.0, 0.0)
}

#[cfg(test)]
fn run_with_slow_clock(args: Args) -> Result<()> {
    run_with_clock_parameters(
        args,
        2.0,
        ESTABLISHED_CLOCK_MU,
        SLOW_CLOCK_GAIN_OMEGA,
        SLOW_CLOCK_GAIN_MU,
    )
}

#[cfg(test)]
fn run_with_clock_gain_sweep(args: Args, gain_index: usize) -> Result<()> {
    ensure!(
        gain_index < CLOCK_GAIN_MU_SWEEP.len(),
        "clock gain sweep index must be less than {}",
        CLOCK_GAIN_MU_SWEEP.len()
    );
    run_with_clock_parameters(
        args,
        2.0,
        ESTABLISHED_CLOCK_MU,
        CLOCK_GAIN_OMEGA_SWEEP[gain_index],
        CLOCK_GAIN_MU_SWEEP[gain_index],
    )
}

#[cfg(test)]
fn fixed_clock_omega(rate_error_ppm: i32) -> Result<f32> {
    ensure!(
        FIXED_CLOCK_RATE_ERROR_PPM.contains(&rate_error_ppm),
        "fixed clock rate error must be -100 or +100 ppm"
    );
    Ok(2.0 * (1.0 + rate_error_ppm as f32 / CLOCK_RATE_PPM_DENOMINATOR))
}

#[cfg(test)]
fn run_with_fixed_clock_rate(args: Args, rate_error_ppm: i32, initial_clock_mu: f32) -> Result<()> {
    run_with_clock_parameters(
        args,
        fixed_clock_omega(rate_error_ppm)?,
        initial_clock_mu,
        0.0,
        0.0,
    )
}

#[cfg(test)]
fn run_with_burst_soft(args: Args) -> Result<BurstClockStats> {
    match run_with_burst_soft_mode(args, BurstTestTimingMode::Immediate)? {
        BurstTestStats::Burst(stats) => Ok(stats),
        BurstTestStats::ShrRate(_) => bail!("immediate burst mode published joint SHR statistics"),
        BurstTestStats::Deferred(_) => {
            bail!("immediate burst mode published deferred SHR statistics")
        }
    }
}

#[cfg(test)]
fn run_with_burst_shr_soft(args: Args) -> Result<BurstClockStats> {
    match run_with_burst_soft_mode(args, BurstTestTimingMode::ShrInitializer)? {
        BurstTestStats::Burst(stats) => Ok(stats),
        BurstTestStats::ShrRate(_) => bail!("phase-only SHR mode published joint statistics"),
        BurstTestStats::Deferred(_) => {
            bail!("phase-only SHR mode published deferred statistics")
        }
    }
}

#[cfg(test)]
fn run_with_burst_shr_rate_soft(args: Args) -> Result<ShrRateClockStats> {
    match run_with_burst_soft_mode(args, BurstTestTimingMode::ShrRateInitializer)? {
        BurstTestStats::ShrRate(stats) => Ok(stats),
        BurstTestStats::Burst(_) => bail!("joint SHR mode published phase-only statistics"),
        BurstTestStats::Deferred(_) => bail!("joint SHR mode published deferred statistics"),
    }
}

#[cfg(test)]
fn run_with_burst_deferred_shr_rate_soft(args: Args) -> Result<DeferredShrRateClockStats> {
    match run_with_burst_soft_mode(args, BurstTestTimingMode::DeferredShrRateInitializer)? {
        BurstTestStats::Deferred(stats) => Ok(stats),
        BurstTestStats::Burst(_) => bail!("deferred SHR mode published phase-only statistics"),
        BurstTestStats::ShrRate(_) => bail!("deferred SHR mode published joint statistics"),
    }
}

#[cfg(test)]
pub(crate) struct ShrRateDiagnosticRun {
    pub(crate) stats: ShrRateClockStats,
    pub(crate) burst_trace: ShrRateDiagnosticTrace,
    pub(crate) decoder_trace: PositionedDecoderTraceStats,
}

#[cfg(test)]
fn run_with_burst_shr_rate_trace_soft(args: Args) -> Result<ShrRateDiagnosticRun> {
    let run = run_with_burst_soft_mode_observed(args, BurstTestTimingMode::ShrRateDiagnostic)?;
    let stats = match run.stats {
        BurstTestStats::ShrRate(stats) => stats,
        BurstTestStats::Burst(_) => bail!("joint SHR trace mode published phase-only statistics"),
        BurstTestStats::Deferred(_) => bail!("joint SHR trace mode published deferred statistics"),
    };
    Ok(ShrRateDiagnosticRun {
        stats,
        burst_trace: run
            .burst_trace
            .context("joint SHR trace mode did not publish its burst trace")?,
        decoder_trace: run
            .decoder_trace
            .context("joint SHR trace mode did not publish its decoder trace")?,
    })
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum BurstTestTimingMode {
    Immediate,
    ShrInitializer,
    ShrRateInitializer,
    DeferredShrRateInitializer,
    ShrRateDiagnostic,
}

#[cfg(test)]
impl BurstTestTimingMode {
    const fn lane_name(self) -> &'static str {
        match self {
            Self::Immediate => "causal-burst-soft",
            Self::ShrInitializer => "causal-burst-shr-soft",
            Self::ShrRateInitializer => "causal-burst-shr-rate-soft",
            Self::DeferredShrRateInitializer => "causal-burst-deferred-shr-rate-soft",
            Self::ShrRateDiagnostic => "causal-burst-shr-rate-trace-soft",
        }
    }
}

#[cfg(test)]
enum BurstTestStats {
    Burst(BurstClockStats),
    ShrRate(ShrRateClockStats),
    Deferred(DeferredShrRateClockStats),
}

#[cfg(test)]
struct BurstTestRun {
    stats: BurstTestStats,
    burst_trace: Option<ShrRateDiagnosticTrace>,
    decoder_trace: Option<PositionedDecoderTraceStats>,
}

#[cfg(test)]
enum BurstTestStatsObserver {
    Burst(std::sync::Arc<std::sync::Mutex<Option<BurstClockStats>>>),
    ShrRate(std::sync::Arc<std::sync::Mutex<Option<ShrRateClockStats>>>),
    Deferred(std::sync::Arc<std::sync::Mutex<Option<DeferredShrRateClockStats>>>),
}

#[cfg(test)]
impl BurstTestStatsObserver {
    fn take(self) -> Result<BurstTestStats> {
        match self {
            Self::Burst(observer) => observer
                .lock()
                .map_err(|_| anyhow::anyhow!("burst clock statistics lock was poisoned"))?
                .take()
                .map(BurstTestStats::Burst)
                .context("causal burst flowgraph did not publish statistics"),
            Self::ShrRate(observer) => observer
                .lock()
                .map_err(|_| anyhow::anyhow!("joint SHR statistics lock was poisoned"))?
                .take()
                .map(BurstTestStats::ShrRate)
                .context("joint SHR flowgraph did not publish statistics"),
            Self::Deferred(observer) => observer
                .lock()
                .map_err(|_| anyhow::anyhow!("deferred SHR statistics lock was poisoned"))?
                .take()
                .map(BurstTestStats::Deferred)
                .context("deferred SHR flowgraph did not publish statistics"),
        }
    }
}

#[cfg(test)]
fn run_with_burst_soft_mode(
    args: Args,
    timing_mode: BurstTestTimingMode,
) -> Result<BurstTestStats> {
    Ok(run_with_burst_soft_mode_observed(args, timing_mode)?.stats)
}

#[cfg(test)]
fn run_with_burst_soft_mode_observed(
    args: Args,
    timing_mode: BurstTestTimingMode,
) -> Result<BurstTestRun> {
    let (max_input_bytes, max_output_bytes, carrier_offset_hz) = args.validate()?;
    ensure!(
        args.decoder_lane == DecoderLane::Soft,
        "causal burst acquisition requires the soft decoder lane"
    );
    configure_runtime();
    let mut input = File::open(&args.input)
        .with_context(|| format!("open CF32_LE input {}", args.input.display()))?;
    let before = input.metadata()?;
    ensure!(before.is_file(), "CF32_LE input must be a regular file");
    ensure!(
        before.len() > 0 && before.len().is_multiple_of(8),
        "CF32_LE input must contain complete nonempty complex f32 samples"
    );
    ensure!(
        before.len() <= max_input_bytes,
        "CF32_LE input exceeds the configured byte limit"
    );
    let before_modified = before.modified().ok();
    let samples = before.len() / 8;
    validate_cf32_le(&mut input, samples)?;
    let input_metadata = input.try_clone()?;
    let archive = ArchiveWriter::create(
        &args.output,
        args.channel,
        args.max_frames,
        max_output_bytes,
    )?;

    let mut flowgraph = Flowgraph::new();
    let source = flowgraph.add(Cf32LeSource::new(input, samples))?;
    let (burst_clock, stats_observer, burst_trace_observer): (
        BurstClockRecovery,
        BurstTestStatsObserver,
        Option<std::sync::Arc<std::sync::Mutex<Option<ShrRateDiagnosticTrace>>>>,
    ) = match timing_mode {
        BurstTestTimingMode::Immediate => {
            let observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            (
                BurstClockRecovery::with_stats_observer(observer.clone()),
                BurstTestStatsObserver::Burst(observer),
                None,
            )
        }
        BurstTestTimingMode::ShrInitializer => {
            let observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            (
                BurstClockRecovery::with_shr_stats_observer(observer.clone()),
                BurstTestStatsObserver::Burst(observer),
                None,
            )
        }
        BurstTestTimingMode::ShrRateInitializer => {
            let observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            (
                BurstClockRecovery::with_shr_rate_stats_observer(observer.clone()),
                BurstTestStatsObserver::ShrRate(observer),
                None,
            )
        }
        BurstTestTimingMode::DeferredShrRateInitializer => {
            let observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            (
                BurstClockRecovery::with_deferred_shr_rate_stats_observer(observer.clone()),
                BurstTestStatsObserver::Deferred(observer),
                None,
            )
        }
        BurstTestTimingMode::ShrRateDiagnostic => {
            let stats_observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            let trace_observer = std::sync::Arc::new(std::sync::Mutex::new(None));
            (
                BurstClockRecovery::with_shr_rate_diagnostic_observers(
                    stats_observer.clone(),
                    trace_observer.clone(),
                ),
                BurstTestStatsObserver::ShrRate(stats_observer),
                Some(trace_observer),
            )
        }
    };
    let burst_clock = flowgraph.add(burst_clock)?;
    let decoder_trace_observer = matches!(timing_mode, BurstTestTimingMode::ShrRateDiagnostic)
        .then(|| std::sync::Arc::new(std::sync::Mutex::new(None)));
    let decoder: PositionedSoftDsssDecoder = if let Some(observer) = &decoder_trace_observer {
        PositionedSoftDsssDecoder::with_trace_observer(observer.clone())
    } else {
        PositionedSoftDsssDecoder::new()
    };
    let decoder = flowgraph.add(decoder)?;
    let (sender, receiver) = mpsc::channel::<Pmt>(MESSAGE_CAPACITY);
    let message_pipe = flowgraph.add(MessagePipe::new(sender))?;
    if carrier_offset_hz.get() == 0 {
        connect!(flowgraph, source > burst_clock > decoder);
    } else {
        let mut corrector = CarrierCorrector::new(carrier_offset_hz);
        let carrier_correction = flowgraph.add(Apply::new(move |input: &Complex32| {
            corrector.correct(input)
        }))?;
        connect!(flowgraph, source > carrier_correction > burst_clock > decoder);
    }
    connect!(flowgraph, decoder.indexed | message_pipe);

    let runtime = Runtime::new();
    let running = runtime
        .start(flowgraph)
        .context("start causal burst decoder flowgraph")?;
    let handle = running.handle();
    let collector = runtime.spawn(collect_candidates(receiver, archive, None, handle));
    let terminated = running
        .wait()
        .context("causal burst decoder flowgraph failed")?;
    drop(terminated);
    let collected = block_on(collector)?;
    let after = input_metadata.metadata()?;
    let stable_input = after.len() == before.len()
        && before_modified
            .zip(after.modified().ok())
            .is_none_or(|(old, new)| old == new);
    ensure!(stable_input, "CF32_LE input changed while decoding");
    let stats = stats_observer.take()?;
    let burst_trace = burst_trace_observer
        .map(|observer| {
            observer
                .lock()
                .map_err(|_| anyhow::anyhow!("burst diagnostic lock was poisoned"))?
                .take()
                .context("causal burst flowgraph did not publish its diagnostic trace")
        })
        .transpose()?;
    let decoder_trace = decoder_trace_observer
        .map(|observer| {
            observer
                .lock()
                .map_err(|_| anyhow::anyhow!("decoder diagnostic lock was poisoned"))?
                .take()
                .context("positioned decoder did not publish its diagnostic trace")
        })
        .transpose()?;
    let reason = collected.reason;
    let records = collected.archive.records();
    let bytes = collected.archive.bytes();
    let published = collected.archive.publish()?;
    ensure!(
        published == (records, bytes),
        "archive publication count changed"
    );
    println!(
        "Candidate archive: decoder lane {}, {} records, {} bytes, stop reason {:?}; positions are global 2 MHz source-derived ticks, not RF timestamps",
        timing_mode.lane_name(),
        records,
        bytes,
        reason
    );
    println!("Candidate output: {}", args.output.display());
    Ok(BurstTestRun {
        stats,
        burst_trace,
        decoder_trace,
    })
}

fn run_with_clock_parameters(
    args: Args,
    clock_omega: f32,
    initial_clock_mu: f32,
    clock_gain_omega: f32,
    clock_gain_mu: f32,
) -> Result<()> {
    let (max_input_bytes, max_output_bytes, carrier_offset_hz) = args.validate()?;
    ensure!(
        clock_omega.is_finite() && clock_omega > 0.0,
        "clock-recovery omega must be finite and positive"
    );
    ensure!(
        initial_clock_mu.is_finite() && initial_clock_mu > 0.0 && initial_clock_mu < 1.0,
        "clock-recovery initial mu must be finite and satisfy 0 < mu < 1"
    );
    ensure!(
        clock_gain_omega.is_finite() && clock_gain_omega >= 0.0,
        "clock-recovery omega gain must be finite and nonnegative"
    );
    ensure!(
        clock_gain_mu.is_finite() && clock_gain_mu >= 0.0,
        "clock-recovery mu gain must be finite and nonnegative"
    );
    configure_runtime();
    let mut input = File::open(&args.input)
        .with_context(|| format!("open CF32_LE input {}", args.input.display()))?;
    let before = input.metadata()?;
    ensure!(before.is_file(), "CF32_LE input must be a regular file");
    ensure!(
        before.len() > 0 && before.len().is_multiple_of(8),
        "CF32_LE input must contain complete nonempty complex f32 samples"
    );
    ensure!(
        before.len() <= max_input_bytes,
        "CF32_LE input exceeds the configured byte limit"
    );
    let before_modified = before.modified().ok();
    let samples = before.len() / 8;
    validate_cf32_le(&mut input, samples)?;
    let input_metadata = input.try_clone()?;
    let archive = ArchiveWriter::create(
        &args.output,
        args.channel,
        args.max_frames,
        max_output_bytes,
    )?;
    let source_index = args
        .source_index_output
        .as_deref()
        .map(|path| SourceIndexWriter::create(path, args.max_frames, samples))
        .transpose()?;
    let indexed_source = source_index.is_some();

    let mut flowgraph = Flowgraph::new();
    let source = flowgraph.add(Cf32LeSource::new(input, samples))?;
    let mut last = Complex32::new(0.0, 0.0);
    let mut iir = 0.0f32;
    let alpha = 0.00016f32;
    let phase_difference = flowgraph.add(Apply::new(move |input: &Complex32| -> f32 {
        let phase = (last.conj() * input).arg();
        last = *input;
        iir = (1.0 - alpha) * iir + alpha * phase;
        phase - iir
    }))?;
    let clock_recovery: ClockRecoveryMm = ClockRecoveryMm::new(
        clock_omega,
        clock_gain_omega,
        initial_clock_mu,
        clock_gain_mu,
        0.0002,
    );
    let clock_recovery = if indexed_source {
        clock_recovery.with_source_index()
    } else {
        clock_recovery
    };
    let clock = flowgraph.add(clock_recovery)?;
    let (sender, receiver) = mpsc::channel::<Pmt>(MESSAGE_CAPACITY);
    let message_pipe = flowgraph.add(MessagePipe::new(sender))?;
    if carrier_offset_hz.get() == 0 {
        // Keep the established zero-offset graph byte-for-byte unchanged.
        connect!(flowgraph, source > phase_difference > clock);
    } else {
        let mut corrector = CarrierCorrector::new(carrier_offset_hz);
        let carrier_correction = flowgraph.add(Apply::new(move |input: &Complex32| {
            corrector.correct(input)
        }))?;
        connect!(flowgraph, source > carrier_correction > phase_difference > clock);
    }
    match args.decoder_lane {
        DecoderLane::Hard => {
            let hard = Decoder::new(6);
            let decoder = flowgraph.add(if indexed_source {
                hard.with_source_index()
            } else {
                hard
            })?;
            connect!(flowgraph, clock > decoder);
            if indexed_source {
                connect!(flowgraph, decoder.indexed_source | message_pipe);
            } else {
                connect!(flowgraph, decoder.indexed | message_pipe);
            }
        }
        DecoderLane::Soft => {
            let decoder = flowgraph.add(SoftDsssDecoder::new())?;
            connect!(flowgraph, clock > decoder);
            connect!(flowgraph, decoder.indexed | message_pipe);
        }
    }

    let runtime = match args.executor_threads {
        Some(count) => {
            Runtime::with_scheduler(SmolScheduler::with_config(usize::from(count), false))
        }
        None => Runtime::new(),
    };
    let running = runtime
        .start(flowgraph)
        .context("start file-only decoder flowgraph")?;
    let handle = running.handle();
    let collector = runtime.spawn(collect_candidates(receiver, archive, source_index, handle));
    let terminated = running
        .wait()
        .context("file-only decoder flowgraph failed")?;
    // A terminated flowgraph still owns the MessagePipe sender. Drop it before
    // awaiting the collector so no completion path can retain that sender.
    drop(terminated);
    let collected = block_on(collector)?;
    let after = input_metadata.metadata()?;
    let stable_input = after.len() == before.len()
        && before_modified
            .zip(after.modified().ok())
            .is_none_or(|(old, new)| old == new);
    ensure!(stable_input, "CF32_LE input changed while decoding");
    let reason = collected.reason;
    let records = collected.archive.records();
    let bytes = collected.archive.bytes();
    let published = collected.archive.publish()?;
    ensure!(
        published == (records, bytes),
        "archive publication count changed"
    );
    if let Some(index) = collected.source_index {
        index.finish(records)?;
    }
    println!(
        "Candidate archive: decoder lane {}, {} records, {} bytes, stop reason {:?}; recovered chip positions are decoder-relative, not RF timestamps",
        args.decoder_lane.as_str(),
        records,
        bytes,
        reason
    );
    println!("Candidate output: {}", args.output.display());
    if let Some(index) = args.source_index_output {
        println!("Source index: {} ({records} records)", index.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn decoder_cli_controls_are_bounded() {
        let base = [
            "worker",
            "input.cf32",
            "--output",
            "output.i154raw",
            "--channel",
            "17",
            "--max-input-mib",
            "1",
            "--max-frames",
            "1",
            "--max-output-mib",
            "1",
        ];
        let default = Args::try_parse_from(base).unwrap();
        assert_eq!(default.decoder_lane, DecoderLane::Hard);
        assert_eq!(default.carrier_offset_hz, 0);
        assert_eq!(default.executor_threads, None);
        assert_eq!(default.validate().unwrap().2.get(), 0);

        let mut soft = base.to_vec();
        soft.extend(["--decoder-lane", "soft"]);
        assert_eq!(
            Args::try_parse_from(soft).unwrap().decoder_lane,
            DecoderLane::Soft
        );
        assert_eq!(DecoderLane::Hard.as_str(), "hard");
        assert_eq!(DecoderLane::Soft.as_str(), "soft");

        for (text, expected) in [("1", Some(1)), ("8", Some(8))] {
            let mut configured = base.to_vec();
            configured.extend(["--executor-threads", text]);
            assert_eq!(
                Args::try_parse_from(configured).unwrap().executor_threads,
                expected
            );
        }
        for text in ["0", "9", "256"] {
            let mut configured = base.to_vec();
            configured.extend(["--executor-threads", text]);
            assert!(Args::try_parse_from(configured).is_err());
        }

        for (text, expected) in [("-100000", -100_000), ("100000", 100_000)] {
            let mut configured = base.to_vec();
            configured.push("--carrier-offset-hz");
            configured.push(text);
            let configured = Args::try_parse_from(configured).unwrap();
            assert_eq!(configured.carrier_offset_hz, expected);
            assert_eq!(configured.validate().unwrap().2.get(), expected);
        }

        for invalid in [-100_001, 100_001, i32::MIN, i32::MAX] {
            let mut args = Args::try_parse_from(base).unwrap();
            args.carrier_offset_hz = invalid;
            assert!(
                args.validate().is_err(),
                "accepted carrier offset {invalid}"
            );
        }
    }

    #[test]
    fn clock_recovery_settings_are_bounded_before_input_io() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("missing.cf32");

        for (case_index, initial_clock_mu) in [
            f32::NEG_INFINITY,
            -0.25,
            0.0,
            1.0,
            1.25,
            f32::INFINITY,
            f32::NAN,
        ]
        .into_iter()
        .enumerate()
        {
            let output = directory
                .path()
                .join(format!("must-not-exist-{case_index}.i154raw"));
            let error = run_with_clock_mu(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                initial_clock_mu,
            )
            .expect_err("invalid initial mu must fail");
            assert!(
                error
                    .to_string()
                    .contains("clock-recovery initial mu must be finite"),
                "unexpected error for initial mu {initial_clock_mu:?}: {error:#}"
            );
            assert!(!output.exists());
        }

        for (case_index, (clock_gain_omega, clock_gain_mu, expected)) in [
            (-0.1, 0.03, "omega gain"),
            (f32::INFINITY, 0.03, "omega gain"),
            (f32::NAN, 0.03, "omega gain"),
            (0.000225, -0.1, "mu gain"),
            (0.000225, f32::INFINITY, "mu gain"),
            (0.000225, f32::NAN, "mu gain"),
        ]
        .into_iter()
        .enumerate()
        {
            let output = directory
                .path()
                .join(format!("must-not-exist-gain-{case_index}.i154raw"));
            let error = run_with_clock_parameters(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                2.0,
                ESTABLISHED_CLOCK_MU,
                clock_gain_omega,
                clock_gain_mu,
            )
            .expect_err("invalid clock gain must fail");
            assert!(
                error.to_string().contains(expected),
                "unexpected error for gains ({clock_gain_omega:?}, {clock_gain_mu:?}): {error:#}"
            );
            assert!(!output.exists());
        }

        for (case_index, clock_omega) in [f32::NEG_INFINITY, -1.0, 0.0, f32::INFINITY, f32::NAN]
            .into_iter()
            .enumerate()
        {
            let output = directory
                .path()
                .join(format!("must-not-exist-omega-{case_index}.i154raw"));
            let error = run_with_clock_parameters(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                clock_omega,
                ESTABLISHED_CLOCK_MU,
                ESTABLISHED_CLOCK_GAIN_OMEGA,
                ESTABLISHED_CLOCK_GAIN_MU,
            )
            .expect_err("invalid clock omega must fail");
            assert!(
                error.to_string().contains("omega must be finite"),
                "unexpected error for omega {clock_omega:?}: {error:#}"
            );
            assert!(!output.exists());
        }

        for (case_index, initial_clock_mu) in [0.25, 0.5, 0.75].into_iter().enumerate() {
            let output = directory
                .path()
                .join(format!("must-not-exist-fixed-{case_index}.i154raw"));
            let error = run_with_fixed_clock_mu(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                initial_clock_mu,
            )
            .expect_err("missing input must fail after fixed-clock validation");
            assert!(
                format!("{error:#}").contains("open CF32_LE input"),
                "fixed timing phase {initial_clock_mu} failed before input access: {error:#}"
            );
            assert!(!output.exists());
        }

        for (case_index, rate_error_ppm) in [-101, 0, 101].into_iter().enumerate() {
            let output = directory
                .path()
                .join(format!("must-not-exist-rate-invalid-{case_index}.i154raw"));
            let error = run_with_fixed_clock_rate(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                rate_error_ppm,
                ESTABLISHED_CLOCK_MU,
            )
            .expect_err("unsupported fixed clock rate must fail");
            assert!(error.to_string().contains("must be -100 or +100 ppm"));
            assert!(!output.exists());
        }

        for rate_error_ppm in FIXED_CLOCK_RATE_ERROR_PPM {
            let output = directory
                .path()
                .join(format!("must-not-exist-rate-{rate_error_ppm:+}.i154raw"));
            let expected_omega = if rate_error_ppm < 0 { 1.9998 } else { 2.0002 };
            assert!((fixed_clock_omega(rate_error_ppm).unwrap() - expected_omega).abs() < 1.0e-6);
            let error = run_with_fixed_clock_rate(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                rate_error_ppm,
                ESTABLISHED_CLOCK_MU,
            )
            .expect_err("missing input must fail after fixed-rate validation");
            assert!(format!("{error:#}").contains("open CF32_LE input"));
            assert!(!output.exists());
        }

        assert!(
            (SLOW_CLOCK_GAIN_OMEGA - SLOW_CLOCK_GAIN_MU * SLOW_CLOCK_GAIN_MU / 4.0).abs() < 1.0e-10
        );
        assert_eq!(CLOCK_GAIN_MU_SWEEP, [0.0075, 0.015, 0.03, 0.06]);
        assert_eq!(
            CLOCK_GAIN_OMEGA_SWEEP,
            [0.0000140625, 0.00005625, 0.000225, 0.0009]
        );
        assert!(CLOCK_GAIN_MU_SWEEP.windows(2).all(|pair| pair[0] < pair[1]));
        for gain_index in 0..CLOCK_GAIN_MU_SWEEP.len() {
            assert_eq!(
                CLOCK_GAIN_OMEGA_SWEEP[gain_index],
                CLOCK_GAIN_MU_SWEEP[gain_index] * CLOCK_GAIN_MU_SWEEP[gain_index] / 4.0
            );
            let output = directory
                .path()
                .join(format!("must-not-exist-clock-gain-{gain_index}.i154raw"));
            let error = run_with_clock_gain_sweep(
                Args {
                    input: input.clone(),
                    output: output.clone(),
                    source_index_output: None,
                    channel: 17,
                    max_input_mib: 1,
                    max_frames: 1,
                    max_output_mib: 1,
                    decoder_lane: DecoderLane::Hard,
                    carrier_offset_hz: 0,
                    executor_threads: None,
                },
                gain_index,
            )
            .expect_err("missing input must fail after clock-gain validation");
            assert!(
                format!("{error:#}").contains("open CF32_LE input"),
                "clock gain index {gain_index} failed before input access: {error:#}"
            );
            assert!(!output.exists());
        }
        assert_eq!(CLOCK_GAIN_MU_SWEEP[1], SLOW_CLOCK_GAIN_MU);
        assert_eq!(CLOCK_GAIN_OMEGA_SWEEP[1], SLOW_CLOCK_GAIN_OMEGA);
        assert_eq!(CLOCK_GAIN_MU_SWEEP[2], ESTABLISHED_CLOCK_GAIN_MU);
        assert_eq!(CLOCK_GAIN_OMEGA_SWEEP[2], ESTABLISHED_CLOCK_GAIN_OMEGA);

        let output = directory.path().join("must-not-exist-slow.i154raw");
        let error = run_with_slow_clock(Args {
            input: input.clone(),
            output: output.clone(),
            source_index_output: None,
            channel: 17,
            max_input_mib: 1,
            max_frames: 1,
            max_output_mib: 1,
            decoder_lane: DecoderLane::Hard,
            carrier_offset_hz: 0,
            executor_threads: None,
        })
        .expect_err("missing input must fail after slow-clock validation");
        assert!(
            format!("{error:#}").contains("open CF32_LE input"),
            "slow timing loop failed before input access: {error:#}"
        );
        assert!(!output.exists());

        let output = directory.path().join("must-not-exist-clock-gain-4.i154raw");
        let error = run_with_clock_gain_sweep(
            Args {
                input,
                output: output.clone(),
                source_index_output: None,
                channel: 17,
                max_input_mib: 1,
                max_frames: 1,
                max_output_mib: 1,
                decoder_lane: DecoderLane::Hard,
                carrier_offset_hz: 0,
                executor_threads: None,
            },
            CLOCK_GAIN_MU_SWEEP.len(),
        )
        .expect_err("out-of-range clock gain index must fail");
        assert!(error.to_string().contains("index must be less than 4"));
        assert!(!output.exists());
    }

    #[test]
    fn carrier_correction_uses_the_measured_input_offset_sign() {
        for measured_offset_hz in [-100_000, 100_000] {
            let mut matching =
                CarrierCorrector::new(CarrierOffsetHz::new(measured_offset_hz).unwrap());
            let mut mismatched =
                CarrierCorrector::new(CarrierOffsetHz::new(-measured_offset_hz).unwrap());
            for index in 0..64 {
                let input_phase =
                    std::f64::consts::TAU * f64::from(measured_offset_hz) * index as f64
                        / f64::from(CHANNEL_SAMPLE_RATE_HZ);
                let input = Complex32::new(input_phase.cos() as f32, input_phase.sin() as f32);
                let corrected = matching.correct(&input);
                assert!((corrected.re - 1.0).abs() < 2.0e-6);
                assert!(corrected.im.abs() < 2.0e-6);

                let doubled_phase = 2.0 * input_phase;
                let wrong = mismatched.correct(&input);
                assert!((wrong.re - doubled_phase.cos() as f32).abs() < 2.0e-6);
                assert!((wrong.im - doubled_phase.sin() as f32).abs() < 2.0e-6);
            }
        }
    }

    #[test]
    fn carrier_correction_integer_phase_is_periodic_without_drift() {
        for (measured_offset_hz, period) in [
            (-100_000, 40),
            (100_000, 40),
            (-1, CHANNEL_SAMPLE_RATE_HZ),
            (1, CHANNEL_SAMPLE_RATE_HZ),
        ] {
            let mut corrector =
                CarrierCorrector::new(CarrierOffsetHz::new(measured_offset_hz).unwrap());
            for _ in 0..period {
                corrector.advance_phase();
                assert!((0..CHANNEL_SAMPLE_RATE_HZ).contains(&corrector.phase_numerator));
            }
            assert_eq!(corrector.phase_numerator, 0);
        }
    }

    #[test]
    fn parses_only_the_indexed_decoder_contract() {
        let payload = Pmt::Blob(vec![1, 2, 3]);
        let ordinary = Pmt::VecPmt(vec![Pmt::U64(123), payload.clone()]);
        let indexed = Pmt::VecPmt(vec![Pmt::U64(123), Pmt::U64(247), payload]);
        assert_eq!(
            indexed_candidate(ordinary.clone(), false).unwrap(),
            Candidate {
                chip_count: 123,
                required_through: None,
                psdu: vec![1, 2, 3],
            }
        );
        assert_eq!(
            indexed_candidate(indexed.clone(), true)
                .unwrap()
                .required_through,
            Some(247)
        );
        assert!(indexed_candidate(indexed, false).is_err());
        assert!(indexed_candidate(ordinary, true).is_err());
        for invalid in [
            Pmt::Blob(vec![]),
            Pmt::VecPmt(vec![Pmt::U64(1)]),
            Pmt::VecPmt(vec![Pmt::U32(1), Pmt::Blob(vec![1])]),
            Pmt::VecPmt(vec![Pmt::U64(1), Pmt::String("x".into())]),
        ] {
            assert!(indexed_candidate(invalid, false).is_err());
        }
        assert!(
            indexed_candidate(
                Pmt::VecPmt(vec![Pmt::U64(1), Pmt::U32(3), Pmt::Blob(vec![1])]),
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_aliasing_source_index_stage_before_any_output() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.cf32");
        let index = directory.path().join("capture.i154idx");
        let archive = archive::append_suffix(&index, ".partial");
        fs::write(&input, [0; 8]).unwrap();

        assert!(validate_index_paths(&input, &archive, &index).is_err());
        assert!(validate_index_paths(&input, &index, &input).is_err());
        assert!(!archive.exists() && !index.exists());
    }

    #[test]
    fn finite_no_frame_input_handles_pmt_finished_and_publishes_header_only() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("silence.cf32");
        let output = directory.path().join("silence.i154raw");
        fs::write(&input, vec![0u8; 4096 * 8]).unwrap();

        run(Args {
            input,
            output: output.clone(),
            source_index_output: None,
            channel: 17,
            max_input_mib: 1,
            max_frames: 16,
            max_output_mib: 1,
            decoder_lane: DecoderLane::Hard,
            carrier_offset_hz: 0,
            executor_threads: None,
        })
        .unwrap();

        let bytes = fs::read(output).unwrap();
        assert_eq!(bytes.len(), archive::FILE_HEADER_BYTES as usize);
        assert_eq!(&bytes[..8], b"I154RAW\0");
    }
}
