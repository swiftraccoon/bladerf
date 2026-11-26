//! Route APRS channels to independent IQ dwells from a completed capture.

use std::collections::{BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::aprs::{self, Channel, MIN_SWEEP_SAMPLES};
use crate::private_output::publish_json_file;
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, PassRef, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const MAX_CHANNELS: usize = 32;
/// Maximum original-rate samples charged across all APRS channels and dwells.
pub(super) const MAX_TOTAL_SAMPLES: u64 = 2_000_000_000;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Known APRS channel in hertz. Repeat for additional channels.
    #[arg(long, required = true, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: Vec<u64>,
    /// Maximum original-rate IQ samples to replay per channel in each dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a replay plan above this sample count across channels and dwells.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

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
    decoded_samples: u64,
    validated_frames: usize,
    channels: Vec<ChannelReport<'a>>,
}

impl Report<'_> {
    /// Number of validated APRS frame occurrences across the replayed channels.
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
    replay: aprs::Report,
}

struct Route<'a> {
    channel: Channel,
    channel_index: usize,
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    samples: u64,
}

impl<'a> Route<'a> {
    fn is_short(&self) -> bool {
        self.samples < MIN_SWEEP_SAMPLES
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.is_short() { 0 } else { self.samples },
        }
    }

    fn replay(self, input: &mut dyn Read, stop: &AtomicBool) -> Result<Attempt<'a>> {
        let replay = self
            .channel
            .replay(input, self.tune.samples, self.samples, stop)?;
        Ok(Attempt {
            pass_index: self.pass_index,
            pass_directory: self.pass_directory,
            source_sweep_sha256: self.source_sweep_sha256,
            tune_directory: &self.tune.directory,
            source_sidecar_sha256: &self.tune.sidecar_sha256,
            replay,
        })
    }
}

impl<'a> ChannelReport<'a> {
    fn plan(
        channel_frequency_hz: u64,
        channel_index: usize,
        passes: &[PassRef<'a>],
        max_samples: u64,
        routes: &mut Vec<Route<'a>>,
    ) -> Self {
        let mut covered_dwells = 0;
        let mut short_dwells = 0;
        for (index, pass) in passes.iter().enumerate() {
            for tune in &pass.sweep.tunes {
                let channel = Channel {
                    center_hz: tune.observed_center_hz,
                    channel_hz: channel_frequency_hz,
                    sample_rate_hz: SAMPLE_RATE,
                };
                if !channel.is_covered() {
                    continue;
                }
                covered_dwells += 1;
                let samples = tune.samples.min(max_samples);
                short_dwells += usize::from(samples < MIN_SWEEP_SAMPLES);
                routes.push(Route {
                    channel,
                    channel_index,
                    pass_index: index + 1,
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                    tune,
                    samples,
                });
            }
        }
        Self {
            channel_frequency_hz,
            covered_dwells,
            short_dwells,
            attempts: Vec::new(),
        }
    }
}

pub(crate) fn requested_frequencies(channel_hz: &[u64]) -> Result<BTreeSet<u64>> {
    let frequencies: BTreeSet<_> = channel_hz.iter().copied().collect();
    ensure!(
        frequencies.len() <= MAX_CHANNELS,
        "select up to {MAX_CHANNELS} APRS channels"
    );
    Ok(frequencies)
}

/// Ordered channel and dwell routes with an admitted APRS sample budget.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    decoded_samples: u64,
    channels: Vec<ChannelReport<'a>>,
    routes: VecDeque<Route<'a>>,
}

impl<'a> Plan<'a> {
    /// Summarize prefix reads and coverage across the selected channels.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.decoded_samples,
            eligible_attempts: self
                .channels
                .iter()
                .map(|channel| channel.covered_dwells - channel.short_dwells)
                .sum(),
            short_attempts: self
                .channels
                .iter()
                .map(|channel| channel.short_dwells)
                .sum(),
            budget_samples: self.decoded_samples,
            max_budget_samples: self.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// Report the scheduled source prefixes without reading IQ payloads.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.iter().map(Route::request)
    }

    /// Consume the validated routes and replay each eligible channel and dwell.
    ///
    /// # Errors
    /// Rejects invalid or changed IQ and interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Assemble the channel-ordered report after the scheduled routes finish.
    fn finish(self) -> Report<'a> {
        debug_assert!(self.routes.is_empty());
        let validated_frames = self
            .channels
            .iter()
            .flat_map(|channel| &channel.attempts)
            .map(|attempt| attempt.replay.validated_frames)
            .sum();
        Report {
            schema_version: 1,
            operation: "identify_aprs_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            minimum_frame_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_samples: self.max_total_samples,
            decoded_samples: self.decoded_samples,
            validated_frames,
            channels: self.channels,
        }
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.routes.front().map(Route::request)
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        check_stop(stop)?;
        let route = self
            .routes
            .pop_front()
            .context("missing planned APRS route")?;
        if route.is_short() {
            return Ok(());
        }
        let channel = &mut self.channels[route.channel_index];
        let pass_index = route.pass_index;
        let tune_directory = &route.tune.directory;
        let attempt = route.replay(input, stop).with_context(|| {
            format!(
                "replay {} Hz from pass {pass_index}/{tune_directory}",
                channel.channel_frequency_hz
            )
        })?;
        channel.attempts.push(attempt);
        Ok(())
    }
}

/// Plan requested APRS channels and validate replay cost before reading IQ.
///
/// # Errors
/// Returns an error for excessive channels, excessive replay work or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    channel_hz: &[u64],
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    let frequencies = requested_frequencies(channel_hz)?;
    prepare_frequencies(
        capture,
        frequencies,
        max_samples_per_dwell,
        max_total_samples,
        stop,
    )
}

fn prepare_frequencies<'a>(
    capture: &'a CompletedCapture,
    frequencies: BTreeSet<u64>,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let passes = capture.passes();
    let mut routes = Vec::new();
    let channels = frequencies
        .into_iter()
        .enumerate()
        .map(|(index, frequency)| {
            ChannelReport::plan(
                frequency,
                index,
                &passes,
                max_samples_per_dwell,
                &mut routes,
            )
        })
        .collect();
    routes.sort_unstable_by_key(|route| (route.request().key(), route.channel_index));
    let decoded_samples = routes
        .iter()
        .map(|route| route.request().samples)
        .sum::<u64>();
    ensure!(
        decoded_samples <= max_total_samples,
        "APRS replay needs {decoded_samples} samples, exceeding --max-total-samples ({max_total_samples})"
    );
    Ok(Plan {
        capture,
        max_samples_per_dwell,
        max_total_samples,
        decoded_samples,
        channels,
        routes: routes.into(),
    })
}

/// Replay the channels from each eligible dwell and publish a source-bound report.
///
/// # Errors
/// Returns an error for inconsistent sources, a plan exceeding its sample
/// budget, invalid IQ, interruption, or failure to publish a fresh report.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let frequencies = requested_frequencies(&args.channel_hz)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let report = prepare_frequencies(
        &capture,
        frequencies,
        args.max_samples_per_dwell,
        args.max_total_samples,
        stop,
    )?
    .replay(stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} AX.25 frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
