//! Plan and replay a DMR channel raster from admitted 20 MSps dwells.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::vec::IntoIter;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dmr::raster::{self, Bounds, GRID_SPACING_HZ, Grid, Span};
use crate::dmr::{DEFAULT_RETAINED_FRAMES, MAX_RETAINED_FRAMES, MIN_SWEEP_SAMPLES};
use crate::private_output::publish_json_file;
use crate::{MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE};

use super::admission::{CompletedCapture, Tune};
use super::aprs_sweep::MAX_TOTAL_SAMPLES;
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const DEFAULT_MAX_CHANNELIZED_SAMPLES: u64 = 100_000_000;
const MAX_CHANNELIZED_SAMPLES: u64 = 2_000_000_000;
const Q11_SAMPLE_BYTES: u64 = 4;

/// Select a DMR raster and work limits for one completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// First candidate channel in hertz. This anchors the 12.5 kHz raster.
    #[arg(long, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    lower_hz: u64,
    /// Last candidate channel in hertz, inclusive and on the same raster.
    #[arg(long, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    upper_hz: u64,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this total original-rate source sample count.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Refuse a plan above this total sample count across channels and dwells.
    #[arg(long, default_value_t = DEFAULT_MAX_CHANNELIZED_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_CHANNELIZED_SAMPLES))]
    max_channelized_samples: u64,
    /// Maximum retained frame records across the entire capture.
    #[arg(long, default_value_t = DEFAULT_RETAINED_FRAMES, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_RETAINED_FRAMES as u64))]
    max_frames: usize,
    /// Print coverage and validated work without reading IQ payloads.
    #[arg(long)]
    plan_only: bool,
    /// New private JSON report file in an existing directory.
    #[arg(
        long,
        required_unless_present = "plan_only",
        conflicts_with = "plan_only"
    )]
    output: Option<PathBuf>,
}

#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    lower_hz: u64,
    upper_hz: u64,
    raster_step_hz: u64,
    sample_rate_hz: u32,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_channelized_samples: u64,
    max_frames: usize,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    uncovered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    requested_channels: u64,
    covered_channels: u64,
    uncovered_channels: u64,
    short_only_channels: u64,
    replayed_channels: u64,
    replayed_iq_samples: u64,
    source_iq_bytes: u64,
    channelized_samples: u64,
    validated_frames: usize,
    retained_frames: usize,
    omitted_valid_frames: usize,
    dwells: Vec<Dwell<'a>>,
}

impl Report<'_> {
    /// Number of verified physical bursts, including omitted details.
    pub(super) fn validated_frames(&self) -> usize {
        self.validated_frames
    }
}

#[derive(Serialize)]
struct Dwell<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune_directory: &'a str,
    source_sweep_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    requested_center_hz: u64,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    channel_spans: Vec<Span>,
    channelized_samples: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    Uncovered,
    ShortDwell,
    Scheduled,
    Replayed { replay: raster::Report },
}

struct Route<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
    grid: Grid,
}

impl<'a> Route<'a> {
    fn planned_outcome(&self) -> Outcome {
        if self.grid.channel_count() == 0 {
            Outcome::Uncovered
        } else if self.selected_samples < MIN_SWEEP_SAMPLES {
            Outcome::ShortDwell
        } else {
            Outcome::Scheduled
        }
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if matches!(self.planned_outcome(), Outcome::Scheduled) {
                self.selected_samples
            } else {
                0
            },
        }
    }

    fn dwell(self, outcome: Outcome) -> Dwell<'a> {
        Dwell {
            pass_index: self.pass_index,
            pass_directory: self.pass_directory,
            tune_directory: &self.tune.directory,
            source_sweep_sha256: self.source_sweep_sha256,
            source_sidecar_sha256: &self.tune.sidecar_sha256,
            requested_center_hz: self.tune.requested_center_hz,
            observed_center_hz: self.tune.observed_center_hz,
            saved_samples: self.tune.samples,
            selected_samples: self.selected_samples,
            channel_spans: self.grid.spans().collect(),
            channelized_samples: self.grid.channelized_samples(self.request().samples),
            outcome,
        }
    }
}

fn distinct_channels(mut spans: Vec<Span>) -> u64 {
    spans.sort_unstable_by_key(|span| span.first_hz);
    let mut next_hz = 0;
    let mut count = 0;
    for span in spans {
        let first_hz = span.first_hz.max(next_hz);
        if first_hz <= span.last_hz {
            count += (span.last_hz - first_hz) / GRID_SPACING_HZ + 1;
            next_hz = span.last_hz + GRID_SPACING_HZ;
        }
    }
    count
}

/// Sample and retention limits for a saved DMR raster replay.
pub(super) struct Limits {
    pub(super) max_samples_per_dwell: u64,
    pub(super) max_total_samples: u64,
    pub(super) max_channelized_samples: u64,
    pub(super) max_frames: usize,
}

/// Ordered dwell requests with admitted source and channelized work.
pub(super) struct Plan<'a> {
    routes: IntoIter<Route<'a>>,
    report: Report<'a>,
    eligible_channel_attempts: usize,
    short_channel_attempts: usize,
}

impl<'a> Plan<'a> {
    /// Admit a configured raster for combined replay without opening IQ payloads.
    ///
    /// # Errors
    /// Rejects invalid dwell geometry, exceeded work limits, or interruption.
    pub(super) fn prepare(
        capture: &'a CompletedCapture,
        bounds: Bounds,
        limits: Limits,
        stop: &AtomicBool,
    ) -> Result<Self> {
        Self::prepare_with_budget(
            capture,
            bounds,
            limits,
            "--max-total-channelized-samples",
            stop,
        )
    }

    fn prepare_with_budget(
        capture: &'a CompletedCapture,
        bounds: Bounds,
        limits: Limits,
        channelized_option: &str,
        stop: &AtomicBool,
    ) -> Result<Self> {
        let Bounds { lower_hz, upper_hz } = bounds;
        let passes = capture.passes();
        let mut covered_spans = Vec::new();
        let mut replayed_spans = Vec::new();
        let mut routes = Vec::new();
        let mut replayed_iq_samples = 0;
        let mut channelized_samples = 0;
        let mut short_dwells = 0;
        let mut uncovered_dwells = 0;
        let mut eligible_channel_attempts = 0;
        let mut short_channel_attempts = 0;
        for (index, pass) in passes.iter().enumerate() {
            for tune in &pass.sweep.tunes {
                check_stop(stop)?;
                let route = Route {
                    pass_index: index + 1,
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                    tune,
                    selected_samples: tune.samples.min(limits.max_samples_per_dwell),
                    grid: Grid::new(tune.observed_center_hz, lower_hz, upper_hz)?,
                };
                match (route.grid.channel_count(), route.request().samples) {
                    (0, _) => uncovered_dwells += 1,
                    (channel_count, 0) => {
                        short_dwells += 1;
                        short_channel_attempts += channel_count;
                    }
                    (channel_count, samples) => {
                        eligible_channel_attempts += channel_count;
                        replayed_iq_samples += samples;
                        ensure!(
                            replayed_iq_samples <= limits.max_total_samples,
                            "DMR raster plan needs at least {replayed_iq_samples} source samples, exceeding --max-total-samples ({})",
                            limits.max_total_samples
                        );
                        channelized_samples += route.grid.channelized_samples(samples);
                        ensure!(
                            channelized_samples <= limits.max_channelized_samples,
                            "DMR raster plan needs at least {channelized_samples} channelized samples, exceeding {channelized_option} ({})",
                            limits.max_channelized_samples
                        );
                        replayed_spans.extend(route.grid.spans());
                    }
                }
                covered_spans.extend(route.grid.spans());
                routes.push(route);
            }
        }
        let requested_channels = (upper_hz - lower_hz) / GRID_SPACING_HZ + 1;
        let covered_channels = distinct_channels(covered_spans);
        let replayed_channels = distinct_channels(replayed_spans);
        let covered_dwells = routes.len() - uncovered_dwells;
        let report = Report {
            schema_version: 1,
            operation: "identify_dmr_raster_in_saved_sweeps",
            capture_kind: capture.kind(),
            source_capture_sha256: capture.source_receipt_sha256(),
            lower_hz,
            upper_hz,
            raster_step_hz: GRID_SPACING_HZ,
            sample_rate_hz: SAMPLE_RATE,
            minimum_dwell_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: limits.max_samples_per_dwell,
            max_total_samples: limits.max_total_samples,
            max_channelized_samples: limits.max_channelized_samples,
            max_frames: limits.max_frames,
            pass_count: passes.len(),
            dwell_count: routes.len(),
            covered_dwells,
            uncovered_dwells,
            short_dwells,
            replayed_dwells: covered_dwells - short_dwells,
            requested_channels,
            covered_channels,
            uncovered_channels: requested_channels - covered_channels,
            short_only_channels: covered_channels - replayed_channels,
            replayed_channels,
            replayed_iq_samples,
            source_iq_bytes: replayed_iq_samples * Q11_SAMPLE_BYTES,
            channelized_samples,
            validated_frames: 0,
            retained_frames: 0,
            omitted_valid_frames: 0,
            dwells: Vec::with_capacity(routes.len()),
        };
        Ok(Self {
            routes: routes.into_iter(),
            report,
            eligible_channel_attempts,
            short_channel_attempts,
        })
    }

    /// Return the primary replay work, including all original IQ reads.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.report.replayed_iq_samples,
            eligible_attempts: self.eligible_channel_attempts,
            short_attempts: self.short_channel_attempts,
            budget_samples: self.report.channelized_samples,
            max_budget_samples: self.report.max_channelized_samples,
            budget_unit: BudgetUnit::Channelized,
        }
    }

    /// Report the independent source budget. The primary work counts its reads.
    pub(super) fn source_work(&self) -> Work {
        Work {
            replayed_iq_samples: 0,
            eligible_attempts: self.report.replayed_dwells,
            short_attempts: self.report.short_dwells,
            budget_samples: self.report.replayed_iq_samples,
            max_budget_samples: self.report.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// Return the remaining admitted source requests in pass and tune order.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.as_slice().iter().map(Route::request)
    }

    /// Finish the admitted requests and return the raster receipt.
    ///
    /// # Errors
    /// Returns an error for invalid or changed IQ, or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.report)
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.routes.as_slice().first().map(Route::request)
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        let route = self
            .routes
            .next()
            .context("missing DMR raster dwell route")?;
        let mut outcome = route.planned_outcome();
        if matches!(outcome, Outcome::Scheduled) {
            let remaining = self.report.max_frames - self.report.retained_frames;
            let replay = route
                .grid
                .replay(
                    input,
                    route.tune.samples,
                    route.selected_samples,
                    remaining,
                    stop,
                )
                .with_context(|| {
                    format!(
                        "replay DMR raster from pass {}/{}",
                        route.pass_index, route.tune.directory
                    )
                })?;
            self.report.validated_frames += replay.validated_frames();
            self.report.retained_frames += replay.retained_frames();
            self.report.omitted_valid_frames =
                self.report.validated_frames - self.report.retained_frames;
            outcome = Outcome::Replayed { replay };
        }
        self.report.dwells.push(route.dwell(outcome));
        Ok(())
    }
}

/// Replay a guarded DMR raster or print its plan from a completed capture.
///
/// Planning reads metadata only. Each scheduled dwell is read once, and
/// output is published only after replay and capture verification succeed.
///
/// # Errors
/// Rejects invalid or changed captures, bad raster bounds, excessive
/// replay work, invalid IQ, interruption, or failed report publication.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let bounds = Bounds::new(args.lower_hz, args.upper_hz)?;
    let capture = CompletedCapture::open(&root.join(&args.capture_directory), args.series)?;
    let limits = Limits {
        max_samples_per_dwell: args.max_samples_per_dwell,
        max_total_samples: args.max_total_samples,
        max_channelized_samples: args.max_channelized_samples,
        max_frames: args.max_frames,
    };
    let mut plan =
        Plan::prepare_with_budget(&capture, bounds, limits, "--max-channelized-samples", stop)?;
    if args.plan_only {
        capture.verify()?;
        check_stop(stop)?;
        plan.report.operation = "plan_dmr_raster_in_saved_sweeps";
        for route in plan.routes {
            let outcome = route.planned_outcome();
            plan.report.dwells.push(route.dwell(outcome));
        }
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &plan.report)?;
        writeln!(stdout)?;
        stdout.flush()?;
        return Ok(());
    }
    let report = plan.replay(stop)?;
    let output = root.join(
        args.output
            .context("supply --output for DMR raster replay")?,
    );
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} DMR frames across {} replayed dwells. Output: {}",
        report.validated_frames(),
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_coverage_counts_the_union_of_overlapping_spans() {
        let anchor = 450_006_250;
        let span = |first, last| Span {
            first_hz: anchor + first * GRID_SPACING_HZ,
            last_hz: anchor + last * GRID_SPACING_HZ,
        };
        assert_eq!(distinct_channels(Vec::new()), 0);
        assert_eq!(
            distinct_channels(vec![span(4, 5), span(1, 3), span(8, 9), span(2, 4)]),
            7
        );
    }
}
