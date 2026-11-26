//! Replay explicitly configured decoder channels in admitted sweep dwells.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dmr::{DEFAULT_RETAINED_FRAMES, MAX_RETAINED_FRAMES};
use crate::dsp::Iq16;
use crate::private_output::publish_json_file;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, ais, dect, dmr, halow, receiver, scm,
    uat,
};

use super::admission::CompletedCapture;
use super::aprs_sweep::MAX_TOTAL_SAMPLES;
use super::check_stop;
use super::replay::shared::{self, Request, StreamingTask, Task};
use super::replay::{BudgetUnit, Work};

const MAX_CHANNELS: usize = 32;
const _: () = assert!(DEFAULT_RETAINED_FRAMES == dect::DEFAULT_RETAINED_FRAMES);
const _: () = assert!(MAX_RETAINED_FRAMES == dect::MAX_RETAINED_FRAMES);
const _: () = assert!(DEFAULT_RETAINED_FRAMES == halow::DEFAULT_RETAINED_FRAMES);
const _: () = assert!(MAX_RETAINED_FRAMES == halow::MAX_RETAINED_FRAMES);
const _: () = assert!(DEFAULT_RETAINED_FRAMES == scm::DEFAULT_RETAINED_FRAMES);
const _: () = assert!(MAX_RETAINED_FRAMES == scm::MAX_RETAINED_FRAMES);
const _: () = assert!(DEFAULT_RETAINED_FRAMES == ais::DEFAULT_RETAINED_FRAMES);
const _: () = assert!(MAX_RETAINED_FRAMES == ais::MAX_RETAINED_FRAMES);
const _: () = assert!(DEFAULT_RETAINED_FRAMES == uat::DEFAULT_RETAINED_FRAMES);
const _: () = assert!(MAX_RETAINED_FRAMES == uat::MAX_RETAINED_FRAMES);

/// Select explicit channels and bounded replay from a completed sweep.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(flatten)]
    capture: CaptureArgs,
    /// Configured channel center in hertz. Repeat for additional channels.
    #[arg(long, required = true, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    channel_hz: Vec<u64>,
}

/// Select a saved sweep and replay limits for a fixed-frequency decoder.
#[derive(Debug, clap::Args)]
pub(crate) struct CaptureArgs {
    /// Directory containing a completed sweep or series receipt and its IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Maximum original-rate IQ samples to replay per channel in each dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this sample count across channels and dwells.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Maximum retained frame records across the entire capture.
    #[arg(long, default_value_t = DEFAULT_RETAINED_FRAMES, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_RETAINED_FRAMES as u64))]
    max_frames: usize,
    /// New private JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// Decoder used for each explicit channel in a replay plan.
#[derive(Clone, Copy)]
pub(crate) enum Protocol {
    Ais,
    Dmr,
    Dect,
    Halow,
    Scm,
    ScmPlus,
    Uat,
}

impl Protocol {
    fn name(self) -> &'static str {
        match self {
            Self::Ais => "AIS",
            Self::Dmr => "DMR",
            Self::Dect => "DECT",
            Self::Halow => "HaLow",
            Self::Scm => "SCM",
            Self::ScmPlus => "SCM+",
            Self::Uat => "UAT",
        }
    }

    fn operation(self) -> &'static str {
        match self {
            Self::Ais => "identify_ais_in_saved_sweeps",
            Self::Dmr => "identify_dmr_in_saved_sweeps",
            Self::Dect => "identify_dect_in_saved_sweeps",
            Self::Halow => "identify_halow_in_saved_sweeps",
            Self::Scm => "identify_scm_in_saved_sweeps",
            Self::ScmPlus => "identify_scm_plus_in_saved_sweeps",
            Self::Uat => "identify_uat_in_saved_sweeps",
        }
    }

    /// Normalize explicitly selected centers to a bounded ascending list.
    ///
    /// # Errors
    /// Returns an error for unsupported frequencies or excessive channels.
    pub(crate) fn requested_frequencies(self, channel_hz: &[u64]) -> Result<Vec<u64>> {
        ensure!(
            channel_hz
                .iter()
                .all(|frequency| (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(frequency)),
            "{} channel must be within {MIN_CENTER_HZ}..={MAX_CENTER_HZ} Hz",
            self.name()
        );
        let mut frequencies = channel_hz.to_vec();
        frequencies.sort_unstable();
        frequencies.dedup();
        ensure!(
            frequencies.len() <= MAX_CHANNELS,
            "select up to {MAX_CHANNELS} {} channels",
            self.name()
        );
        Ok(frequencies)
    }

    /// Minimum complete frame extent on the sweep's original-rate grid.
    pub(super) fn minimum_frame_samples(self) -> u64 {
        match self {
            Self::Ais => ais::MIN_SWEEP_SAMPLES,
            Self::Dmr => dmr::MIN_SWEEP_SAMPLES,
            Self::Dect => dect::MIN_SWEEP_SAMPLES,
            Self::Halow => halow::MIN_SWEEP_SAMPLES,
            Self::Scm => scm::LEGACY_MIN_SWEEP_SAMPLES,
            Self::ScmPlus => scm::MIN_SWEEP_SAMPLES,
            Self::Uat => uat::MIN_SWEEP_SAMPLES,
        }
    }

    /// Whether a 20 MSps dwell covers the selected decoder's channel geometry.
    pub(super) fn is_covered(self, center_hz: u64, channel_hz: u64) -> bool {
        match self {
            Self::Ais => ais::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .is_covered(),
            Self::Dmr => dmr::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .is_covered(),
            Self::Dect => dect::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .is_covered(),
            Self::Halow => halow::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .is_covered(),
            Self::Scm | Self::ScmPlus => scm::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .is_covered(),
            Self::Uat => {
                channel_hz == uat::CHANNEL_HZ
                    && uat::Channel {
                        center_hz,
                        sample_rate_hz: SAMPLE_RATE,
                    }
                    .is_covered()
            }
        }
    }

    fn start(self, route: &Route<'_>, channel_hz: u64, remaining: usize) -> Result<Decoder> {
        let Request { tune, samples, .. } = route.request;
        let center_hz = tune.observed_center_hz;
        match self {
            Self::Ais => ais::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .start(tune.samples, samples, remaining)
            .map(|stream| Decoder::Ais(Box::new(stream))),
            Self::Dmr => dmr::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .start(tune.samples, samples, remaining)
            .map(|stream| Decoder::Dmr(Box::new(stream))),
            Self::Dect => dect::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .start(tune.samples, samples, remaining)
            .map(|stream| Decoder::Dect(Box::new(stream))),
            Self::Halow => halow::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .start(tune.samples, samples, remaining)
            .map(|stream| Decoder::Halow(Box::new(stream))),
            Self::Scm | Self::ScmPlus => {
                let channel = scm::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                };
                let format = if matches!(self, Self::Scm) {
                    scm::Format::Legacy
                } else {
                    scm::Format::Plus
                };
                let stream = channel.start(format, tune.samples, samples, remaining)?;
                Ok(Decoder::ItronScm(Box::new(stream)))
            }
            Self::Uat => uat::Channel {
                center_hz,
                sample_rate_hz: SAMPLE_RATE,
            }
            .start(tune.samples, samples, remaining)
            .map(|stream| Decoder::Uat(Box::new(stream))),
        }
    }
}

/// Source-bound coverage, independent channel work and retained frames.
#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    sample_rate_hz: u32,
    minimum_frame_samples: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_frames: usize,
    decoded_samples: u64,
    validated_frames: usize,
    retained_frames: usize,
    omitted_valid_frames: usize,
    channels: Vec<ChannelReport<'a>>,
}

impl Report<'_> {
    /// Number of validated frame occurrences across the replayed channels.
    pub(super) fn validated_frames(&self) -> usize {
        self.validated_frames
    }
}

#[derive(Serialize)]
struct ChannelReport<'a> {
    channel_frequency_hz: u64,
    covered_dwells: usize,
    short_dwells: usize,
    attempts: Vec<Attempt<'a>>,
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune_directory: &'a str,
    source_sidecar_sha256: &'a str,
    replay: Decoded,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Decoded {
    Ais(ais::Report),
    Dmr(dmr::Report),
    Dect(dect::Report),
    Halow(halow::Report),
    ItronScm(scm::Report),
    Uat(uat::Report),
}

impl Decoded {
    fn counts(&self) -> (usize, usize) {
        match self {
            Self::Ais(report) => (report.validated_frames(), report.retained_frames()),
            Self::Dmr(report) => (report.validated_frames(), report.retained_frames()),
            Self::Dect(report) => (report.validated_frames(), report.retained_frames()),
            Self::Halow(report) => (report.validated_frames(), report.retained_frames()),
            Self::ItronScm(report) => (report.validated_frames(), report.retained_frames()),
            Self::Uat(report) => (report.validated_frames(), report.retained_frames()),
        }
    }
}

enum Decoder {
    Ais(Box<ais::Stream>),
    Dmr(Box<dmr::Stream>),
    Dect(Box<dect::Stream>),
    Halow(Box<halow::Stream>),
    ItronScm(Box<scm::Stream>),
    Uat(Box<uat::Stream>),
}

impl Decoder {
    fn push(&mut self, sample: Iq16, index: u64) {
        match self {
            Self::Ais(stream) => stream.push(sample, index),
            Self::Dmr(stream) => stream.push(sample, index),
            Self::Dect(stream) => stream.push(sample, index),
            Self::Halow(stream) => stream.push(sample, index),
            Self::ItronScm(stream) => stream.push(sample, index),
            Self::Uat(stream) => stream.push(sample, index),
        }
    }

    fn finish(self, digest: String, remaining: usize) -> Decoded {
        match self {
            Self::Ais(stream) => Decoded::Ais(stream.finish(digest, remaining)),
            Self::Dmr(stream) => Decoded::Dmr(stream.finish(digest, remaining)),
            Self::Dect(stream) => Decoded::Dect(stream.finish(digest, remaining)),
            Self::Halow(stream) => Decoded::Halow(stream.finish(digest, remaining)),
            Self::ItronScm(stream) => Decoded::ItronScm(stream.finish(digest, remaining)),
            Self::Uat(stream) => Decoded::Uat(stream.finish(digest, remaining)),
        }
    }
}

struct Route<'a> {
    channel_index: usize,
    request: Request<'a>,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
}

/// Ordered replay routes with an admitted per-protocol sample budget.
pub(super) struct Plan<'a> {
    protocol: Protocol,
    report: Report<'a>,
    routes: VecDeque<Route<'a>>,
    active: Vec<(Route<'a>, Decoder)>,
}

impl<'a> Plan<'a> {
    /// Summarize independent channel costs and coverage from the admitted routes.
    pub(super) fn work(&self) -> Work {
        let short_attempts = self
            .report
            .channels
            .iter()
            .map(|channel| channel.short_dwells)
            .sum();
        Work {
            replayed_iq_samples: self.report.decoded_samples,
            eligible_attempts: self.routes.len() - short_attempts,
            short_attempts,
            budget_samples: self.report.decoded_samples,
            max_budget_samples: self.report.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// List the admitted source prefixes without opening IQ payloads.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.iter().map(|route| route.request)
    }

    /// Consume the validated routes with independent file-backed prefixes.
    ///
    /// # Errors
    /// Rejects invalid or changed IQ and interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.report)
    }

    fn remaining(&self) -> usize {
        self.report.max_frames - self.report.retained_frames
    }

    fn describe(&self, route: &Route<'_>) -> String {
        let Request {
            pass_index, tune, ..
        } = route.request;
        let channel_hz = self.report.channels[route.channel_index].channel_frequency_hz;
        format!(
            "replay {} at {channel_hz} Hz from pass {pass_index}/{}",
            self.protocol.name(),
            tune.directory
        )
    }

    fn start_decoder(&self, route: &Route<'_>) -> Result<Decoder> {
        let channel_hz = self.report.channels[route.channel_index].channel_frequency_hz;
        self.protocol.start(route, channel_hz, self.remaining())
    }

    fn record(&mut self, route: Route<'a>, replay: Decoded) {
        let Request {
            pass_index, tune, ..
        } = route.request;
        let (validated, retained) = replay.counts();
        self.report.validated_frames += validated;
        self.report.retained_frames += retained;
        self.report.omitted_valid_frames =
            self.report.validated_frames - self.report.retained_frames;
        self.report.channels[route.channel_index]
            .attempts
            .push(Attempt {
                pass_index,
                pass_directory: route.pass_directory,
                source_sweep_sha256: route.source_sweep_sha256,
                tune_directory: &tune.directory,
                source_sidecar_sha256: &tune.sidecar_sha256,
                replay,
            });
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.routes.front().map(|route| route.request)
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        let route = self
            .routes
            .pop_front()
            .context("missing planned channel route")?;
        let samples = route.request.samples;
        if samples == 0 {
            return Ok(());
        }
        let replay = (|| {
            let mut decoder = self.start_decoder(&route)?;
            let digest = receiver::read_q11_prefix(input, samples, stop, |sample, index| {
                decoder.push(sample, index);
                Ok(())
            })?;
            check_stop(stop)?;
            Ok::<_, anyhow::Error>(decoder.finish(digest, self.remaining()))
        })()
        .with_context(|| self.describe(&route))?;
        check_stop(stop)?;
        self.record(route, replay);
        Ok(())
    }
}

impl<'a> StreamingTask<'a> for Plan<'a> {
    fn begin_dwell(&mut self, request: Request<'a>, stop: &AtomicBool) -> Result<(usize, usize)> {
        ensure!(
            self.active.is_empty(),
            "channel replay has an unfinished dwell"
        );
        let mut short = 0;
        while let Some(route) = self
            .routes
            .front()
            .filter(|route| route.request.key() == request.key())
        {
            check_stop(stop)?;
            ensure!(
                std::ptr::eq(route.request.tune, request.tune)
                    && (route.request.samples == 0 || route.request.samples == request.samples),
                "channel replay route differs from its planned source extent"
            );
            let route = self
                .routes
                .pop_front()
                .context("missing planned channel route")?;
            if route.request.samples == 0 {
                short += 1;
            } else {
                let decoder = self
                    .start_decoder(&route)
                    .with_context(|| self.describe(&route))?;
                self.active.push((route, decoder));
            }
        }
        Ok((self.active.len(), short))
    }

    fn push_sample(&mut self, sample: Iq16, index: u64) {
        for (_, decoder) in &mut self.active {
            decoder.push(sample, index);
        }
    }

    fn finish_dwell(&mut self, analyzed_iq_sha256: &str, stop: &AtomicBool) -> Result<()> {
        // Use admitted channel order for the sweep-wide detail limit.
        for (route, decoder) in std::mem::take(&mut self.active) {
            check_stop(stop)?;
            let replay = decoder.finish(analyzed_iq_sha256.to_owned(), self.remaining());
            self.record(route, replay);
        }
        check_stop(stop)?;
        Ok(())
    }
}

/// Prepare all eligible routes and check the aggregate budget before replay.
///
/// # Errors
/// Rejects unsupported channels, excessive work or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    protocol: Protocol,
    channel_hz: &[u64],
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_frames: usize,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let frequencies = protocol.requested_frequencies(channel_hz)?;
    let minimum_frame_samples = protocol.minimum_frame_samples();
    let mut channels: Vec<_> = frequencies
        .into_iter()
        .map(|channel_frequency_hz| ChannelReport {
            channel_frequency_hz,
            covered_dwells: 0,
            short_dwells: 0,
            attempts: Vec::new(),
        })
        .collect();
    let mut routes = Vec::new();
    let mut decoded_samples = 0;
    for (index, pass) in capture.passes().into_iter().enumerate() {
        for tune in &pass.sweep.tunes {
            check_stop(stop)?;
            let samples = tune.samples.min(max_samples_per_dwell);
            let short = samples < minimum_frame_samples;
            let samples = if short { 0 } else { samples };
            for (channel_index, report) in channels.iter_mut().enumerate() {
                if !protocol.is_covered(tune.observed_center_hz, report.channel_frequency_hz) {
                    continue;
                }
                decoded_samples += samples;
                ensure!(
                    decoded_samples <= max_total_samples,
                    "{} replay needs at least {decoded_samples} samples, exceeding --max-total-samples ({max_total_samples})",
                    protocol.name()
                );
                report.covered_dwells += 1;
                report.short_dwells += usize::from(short);
                routes.push(Route {
                    channel_index,
                    request: Request {
                        pass_index: index + 1,
                        tune,
                        samples,
                    },
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                });
            }
        }
    }
    routes.sort_unstable_by_key(|route| (route.request.key(), route.channel_index));
    Ok(Plan {
        protocol,
        report: Report {
            schema_version: 1,
            operation: protocol.operation(),
            capture_kind: capture.kind(),
            source_capture_sha256: capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            minimum_frame_samples,
            max_samples_per_dwell,
            max_total_samples,
            max_frames,
            decoded_samples,
            validated_frames: 0,
            retained_frames: 0,
            omitted_valid_frames: 0,
            channels,
        },
        routes: routes.into(),
        active: Vec::new(),
    })
}

/// Replay selected channels across a saved sweep or series.
///
/// Common source prefixes are shared between configured channels.
///
/// # Errors
/// Rejects inconsistent sources, excessive replay work, invalid IQ,
/// interruption or inability to publish a new report.
pub(crate) fn identify(
    root: &Path,
    args: Args,
    protocol: Protocol,
    stop: &AtomicBool,
) -> Result<()> {
    identify_channels(root, args.capture, protocol, &args.channel_hz, stop)
}

/// Replay the fixed UAT channel across an admitted sweep or series.
/// # Errors
/// Rejects inconsistent captures, excessive replay work or failed output.
pub(crate) fn identify_uat(root: &Path, args: CaptureArgs, stop: &AtomicBool) -> Result<()> {
    identify_channels(root, args, Protocol::Uat, &[uat::CHANNEL_HZ], stop)
}

fn identify_channels(
    root: &Path,
    args: CaptureArgs,
    protocol: Protocol,
    channel_hz: &[u64],
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let mut plan = prepare(
        &capture,
        protocol,
        channel_hz,
        args.max_samples_per_dwell,
        args.max_total_samples,
        args.max_frames,
        stop,
    )?;
    let mut sources = shared::Plan::default();
    sources.add_streaming(plan.requests())?;
    sources.prepare();
    sources.replay(&mut [], &mut [&mut plan], stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &plan.report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} {}. Output: {}",
        plan.report.validated_frames(),
        match protocol {
            Protocol::Ais => "AIS frames",
            Protocol::Dmr => "DMR Voice Link Control frames",
            Protocol::Dect => "DECT fixed-part identity frames",
            Protocol::Halow => "Wi-Fi HaLow S1G beacons",
            Protocol::Scm => "Itron SCM endpoint frames",
            Protocol::ScmPlus => "Itron SCM+ endpoint frames",
            Protocol::Uat => "UAT airborne frames",
        },
        output.display()
    );
    Ok(())
}
