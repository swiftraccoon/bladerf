//! Replay a known OOK channel from guard-safe dwells in a completed capture.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::Iq16;
use crate::ook::{
    self, Channel, DEFAULT_CHANNEL_HZ, DEFAULT_RETAINED_TRANSMISSIONS, MIN_SWEEP_SAMPLES,
};
use crate::private_output::publish_json_file;
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, StreamingTask, Task};
use super::replay::{BudgetUnit, Work};

/// Maximum original-rate samples charged across OOK dwells.
pub(super) const MAX_TOTAL_SAMPLES: u64 = 2_000_000_000;

/// Select a completed capture, OOK channel and replay limits.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Known OOK channel center in hertz.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_HZ, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Maximum original-rate IQ samples to replay per eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a replay plan above this total sample count across eligible dwells.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Maximum retained transmissions across the entire capture.
    #[arg(long, default_value_t = DEFAULT_RETAINED_TRANSMISSIONS, value_parser = ook::parse_retention)]
    max_transmissions: usize,
    /// New private JSON report in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_transmissions: usize,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    oversize_pulse_bursts: usize,
    validated_frame_copies: usize,
    observed_transmissions: usize,
    retained_transmissions: usize,
    omitted_transmissions: usize,
    dwells: Vec<Attempt<'a>>,
}

impl Report<'_> {
    /// Count all observed transmissions, including omitted records.
    pub(super) fn observed_transmissions(&self) -> usize {
        self.observed_transmissions
    }
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune_directory: &'a str,
    source_sidecar_sha256: &'a str,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    ShortDwell,
    Replayed { replay: ook::Report },
}

struct Route<'a> {
    channel: Channel,
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
}

impl<'a> Route<'a> {
    fn is_short(&self) -> bool {
        self.selected_samples < MIN_SWEEP_SAMPLES
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.is_short() {
                0
            } else {
                self.selected_samples
            },
        }
    }

    fn description(&self) -> String {
        format!(
            "replay OOK from pass {}/{}",
            self.pass_index, self.tune.directory
        )
    }
}

/// Ordered OOK dwell routes with an admitted sample budget.
pub(super) struct Plan<'a> {
    report: Report<'a>,
    routes: VecDeque<Route<'a>>,
    active: Option<(Route<'a>, ook::Stream)>,
}

impl<'a> Plan<'a> {
    /// Summarize prefix reads and coverage for the selected channel.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.report.replayed_iq_samples,
            eligible_attempts: self.report.replayed_dwells,
            short_attempts: self.report.short_dwells,
            budget_samples: self.report.replayed_iq_samples,
            max_budget_samples: self.report.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// List the admitted source prefixes without reading IQ payloads.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.iter().map(Route::request)
    }

    /// Replay any pending routes and return the completed report.
    ///
    /// # Errors
    /// Rejects invalid or changed IQ and interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.report)
    }

    fn remaining(&self) -> usize {
        self.report.max_transmissions - self.report.retained_transmissions
    }

    fn record(&mut self, route: Route<'a>, outcome: Outcome) {
        let tune = route.tune;
        let report = &mut self.report;
        if let Outcome::Replayed { replay } = &outcome {
            report.oversize_pulse_bursts += replay.oversize_pulse_bursts();
            report.validated_frame_copies += replay.validated_frame_copies();
            let observed = replay.observed_transmissions();
            let retained = replay.retained_transmissions();
            report.observed_transmissions += observed;
            report.retained_transmissions += retained;
            report.omitted_transmissions += observed - retained;
        }
        report.dwells.push(Attempt {
            pass_index: route.pass_index,
            pass_directory: route.pass_directory,
            source_sweep_sha256: route.source_sweep_sha256,
            tune_directory: &tune.directory,
            source_sidecar_sha256: &tune.sidecar_sha256,
            observed_center_hz: tune.observed_center_hz,
            saved_samples: tune.samples,
            selected_samples: route.selected_samples,
            outcome,
        });
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
            .context("missing planned OOK route")?;
        let outcome = if route.is_short() {
            Outcome::ShortDwell
        } else {
            let replay = route
                .channel
                .replay(
                    input,
                    route.tune.samples,
                    route.selected_samples,
                    self.remaining(),
                    stop,
                )
                .with_context(|| route.description())?;
            Outcome::Replayed { replay }
        };
        self.record(route, outcome);
        Ok(())
    }
}

impl<'a> StreamingTask<'a> for Plan<'a> {
    fn begin_dwell(&mut self, request: Request<'a>, stop: &AtomicBool) -> Result<(usize, usize)> {
        check_stop(stop)?;
        ensure!(self.active.is_none(), "OOK replay has an unfinished dwell");
        let Some(route) = self
            .next_request()
            .filter(|route| route.key() == request.key())
        else {
            return Ok((0, 0));
        };
        ensure!(
            std::ptr::eq(route.tune, request.tune)
                && (route.samples == 0 || route.samples == request.samples),
            "OOK replay route differs from its planned source extent"
        );
        let route = self
            .routes
            .pop_front()
            .context("missing planned OOK route")?;
        if route.is_short() {
            self.record(route, Outcome::ShortDwell);
            return Ok((0, 1));
        }
        let stream = route
            .channel
            .start(route.tune.samples, route.selected_samples, self.remaining())
            .with_context(|| route.description())?;
        self.active = Some((route, stream));
        Ok((1, 0))
    }

    fn push_sample(&mut self, sample: Iq16, index: u64) {
        if let Some((_, stream)) = &mut self.active {
            stream.push(sample, index);
        }
    }

    fn finish_dwell(&mut self, analyzed_iq_sha256: &str, stop: &AtomicBool) -> Result<()> {
        check_stop(stop)?;
        let (route, stream) = self.active.take().context("missing active OOK route")?;
        self.record(
            route,
            Outcome::Replayed {
                replay: stream.finish(analyzed_iq_sha256.to_owned()),
            },
        );
        Ok(())
    }
}

/// Prepare OOK replay and validate its sample budget before reading IQ.
///
/// Uncovered dwells are skipped and short dwells retain their provenance.
///
/// # Errors
/// Rejects excessive replay work and interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    channel_hz: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_transmissions: usize,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let passes = capture.passes();
    let mut report = Report {
        schema_version: 1,
        operation: "identify_ook_in_saved_sweeps",
        capture_kind: capture.kind(),
        source_capture_sha256: capture.source_receipt_sha256(),
        channel_frequency_hz: channel_hz,
        sample_rate_hz: SAMPLE_RATE,
        minimum_dwell_samples: MIN_SWEEP_SAMPLES,
        max_samples_per_dwell,
        max_total_samples,
        max_transmissions,
        pass_count: passes.len(),
        dwell_count: 0,
        covered_dwells: 0,
        short_dwells: 0,
        replayed_dwells: 0,
        replayed_iq_samples: 0,
        oversize_pulse_bursts: 0,
        validated_frame_copies: 0,
        observed_transmissions: 0,
        retained_transmissions: 0,
        omitted_transmissions: 0,
        dwells: Vec::new(),
    };
    let mut routes = VecDeque::new();
    for (index, pass) in passes.into_iter().enumerate() {
        check_stop(stop)?;
        report.dwell_count += pass.sweep.tunes.len();
        for tune in &pass.sweep.tunes {
            let channel = Channel {
                center_hz: tune.observed_center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            };
            if !channel.is_covered() {
                continue;
            }
            let route = Route {
                channel,
                pass_index: index + 1,
                pass_directory: pass.directory,
                source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                tune,
                selected_samples: tune.samples.min(max_samples_per_dwell),
            };
            report.covered_dwells += 1;
            if route.is_short() {
                report.short_dwells += 1;
            } else {
                report.replayed_dwells += 1;
                report.replayed_iq_samples += route.selected_samples;
            }
            routes.push_back(route);
        }
    }
    let total = report.replayed_iq_samples;
    ensure!(
        total <= max_total_samples,
        "OOK replay needs {total} samples, exceeding --max-total-samples ({max_total_samples})"
    );
    report.dwells.reserve(routes.len());
    Ok(Plan {
        report,
        routes,
        active: None,
    })
}

/// Verify a completed capture and publish the selected OOK channel's report.
///
/// # Errors
/// Returns an error if admission, budget validation, replay, final source
/// verification or output publication fails, or the caller interrupts.
pub(crate) fn run(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let capture = CompletedCapture::open(&root.join(&args.capture_directory), args.series)?;
    let report = prepare(
        &capture,
        args.channel_hz,
        args.max_samples_per_dwell,
        args.max_total_samples,
        args.max_transmissions,
        stop,
    )?
    .replay(stop)?;
    publish_json_file(&root.join(args.output), &report, || {
        check_stop(stop)?;
        capture.verify()
    })
}
