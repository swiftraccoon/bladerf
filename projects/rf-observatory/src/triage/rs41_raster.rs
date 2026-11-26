//! Plan and replay a guarded radiosonde raster from saved 20 MSps sweeps.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::vec::IntoIter;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::private_output::publish_json_file;
use crate::rs41::raster::{self, BAND_LOWER_HZ, BAND_UPPER_HZ, Bounds, GRID_SPACING_HZ, Grid};
use crate::rs41::{DEFAULT_RETAINED_FRAMES, MAX_RETAINED_FRAMES, MIN_SWEEP_SAMPLES};
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const DEFAULT_MAX_CHANNELIZED_SAMPLES: u64 = 100_000_000;
const MAX_CHANNELIZED_SAMPLES: u64 = 2_000_000_000;
const Q11_SAMPLE_BYTES: u64 = 4;

/// Select a radiosonde raster and work budgets for one completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// First candidate channel in hertz, inclusive and aligned to the raster.
    #[arg(long, default_value_t = BAND_LOWER_HZ)]
    lower_hz: u64,
    /// Last candidate channel in hertz, inclusive and aligned to the raster.
    #[arg(long, default_value_t = BAND_UPPER_HZ)]
    upper_hz: u64,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this total output sample count across channels and dwells.
    #[arg(long, default_value_t = DEFAULT_MAX_CHANNELIZED_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_CHANNELIZED_SAMPLES))]
    max_channelized_samples: u64,
    /// Maximum retained frame records across the entire capture.
    #[arg(long, default_value_t = DEFAULT_RETAINED_FRAMES, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_RETAINED_FRAMES as u64))]
    max_frames: usize,
    /// Print coverage and the validated work plan without reading IQ payloads.
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

#[derive(Default, Serialize)]
struct ChannelCoverage {
    channel_frequency_hz: u64,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    covered_samples: u64,
    replayed_samples: u64,
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
    max_channelized_samples: u64,
    max_frames: usize,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    requested_channels: usize,
    covered_channels: usize,
    short_only_channels: usize,
    replayed_channels: usize,
    replayed_iq_samples: u64,
    source_iq_bytes: u64,
    channelized_samples: u64,
    valid_candidates: usize,
    validated_frames: usize,
    retained_frames: usize,
    omitted_valid_frames: usize,
    channels: Vec<ChannelCoverage>,
    dwells: Vec<Dwell<'a>>,
}

impl Report<'_> {
    /// Number of verified frames, including those omitted from the details.
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
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    channels_hz: Vec<u64>,
    channelized_samples: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
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

    fn channelized_samples(&self) -> u64 {
        self.grid.channelized_samples(self.request().samples)
    }

    fn dwell(self, outcome: Outcome) -> Dwell<'a> {
        Dwell {
            pass_index: self.pass_index,
            pass_directory: self.pass_directory,
            tune_directory: &self.tune.directory,
            source_sweep_sha256: self.source_sweep_sha256,
            source_sidecar_sha256: &self.tune.sidecar_sha256,
            observed_center_hz: self.tune.observed_center_hz,
            saved_samples: self.tune.samples,
            selected_samples: self.selected_samples,
            channels_hz: self.grid.channels_hz().to_vec(),
            channelized_samples: self.channelized_samples(),
            outcome,
        }
    }
}

/// Sample and retention limits for an admitted RS41 raster.
pub(super) struct Limits {
    pub(super) max_samples_per_dwell: u64,
    /// Independent source sample cap, omitted for standalone replay.
    pub(super) max_total_samples: Option<u64>,
    pub(super) max_channelized_samples: u64,
    pub(super) max_frames: usize,
}

/// Ordered dwell requests with admitted source and channelized work.
pub(super) struct Plan<'a> {
    routes: IntoIter<Route<'a>>,
    report: Report<'a>,
    max_total_samples: u64,
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
        let max_total_samples = limits.max_total_samples.unwrap_or(u64::MAX);
        let mut channels: Vec<_> = (lower_hz..=upper_hz)
            .step_by(GRID_SPACING_HZ as usize)
            .map(|channel_frequency_hz| ChannelCoverage {
                channel_frequency_hz,
                ..ChannelCoverage::default()
            })
            .collect();
        let mut routes = Vec::new();
        let mut channelized_samples = 0;
        let mut replayed_iq_samples = 0;
        let mut short_dwells = 0;
        for (index, pass) in passes.iter().enumerate() {
            for tune in &pass.sweep.tunes {
                check_stop(stop)?;
                let grid = Grid::new(tune.observed_center_hz, lower_hz, upper_hz)?;
                if grid.channels_hz().is_empty() {
                    continue;
                }
                let route = Route {
                    pass_index: index + 1,
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                    tune,
                    selected_samples: tune.samples.min(limits.max_samples_per_dwell),
                    grid,
                };
                let short = route.is_short();
                short_dwells += usize::from(short);
                if !short {
                    replayed_iq_samples += route.selected_samples;
                    ensure!(
                        replayed_iq_samples <= max_total_samples,
                        "RS41 raster plan needs at least {replayed_iq_samples} source samples, exceeding --max-total-samples ({max_total_samples})"
                    );
                    channelized_samples += route.channelized_samples();
                    ensure!(
                        channelized_samples <= limits.max_channelized_samples,
                        "RS41 raster plan needs at least {channelized_samples} channelized samples, exceeding {channelized_option} ({})",
                        limits.max_channelized_samples
                    );
                }
                for &channel_hz in route.grid.channels_hz() {
                    let position = ((channel_hz - lower_hz) / GRID_SPACING_HZ) as usize;
                    let coverage = &mut channels[position];
                    coverage.covered_dwells += 1;
                    coverage.covered_samples += tune.samples;
                    if short {
                        coverage.short_dwells += 1;
                    } else {
                        coverage.replayed_dwells += 1;
                        coverage.replayed_samples += route.selected_samples;
                    }
                }
                routes.push(route);
            }
        }
        let covered_channels = channels
            .iter()
            .filter(|channel| channel.covered_dwells != 0)
            .count();
        let replayed_channels = channels
            .iter()
            .filter(|channel| channel.replayed_dwells != 0)
            .count();
        let report = Report {
            schema_version: 1,
            operation: "identify_rs41_raster_in_saved_sweeps",
            capture_kind: capture.kind(),
            source_capture_sha256: capture.source_receipt_sha256(),
            lower_hz,
            upper_hz,
            raster_step_hz: GRID_SPACING_HZ,
            sample_rate_hz: SAMPLE_RATE,
            minimum_dwell_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: limits.max_samples_per_dwell,
            max_channelized_samples: limits.max_channelized_samples,
            max_frames: limits.max_frames,
            pass_count: passes.len(),
            dwell_count: passes.iter().map(|pass| pass.sweep.tunes.len()).sum(),
            covered_dwells: routes.len(),
            short_dwells,
            replayed_dwells: routes.len() - short_dwells,
            requested_channels: channels.len(),
            covered_channels,
            short_only_channels: covered_channels - replayed_channels,
            replayed_channels,
            replayed_iq_samples,
            source_iq_bytes: replayed_iq_samples * Q11_SAMPLE_BYTES,
            channelized_samples,
            valid_candidates: 0,
            validated_frames: 0,
            retained_frames: 0,
            omitted_valid_frames: 0,
            channels,
            dwells: Vec::with_capacity(routes.len()),
        };
        Ok(Self {
            routes: routes.into_iter(),
            report,
            max_total_samples,
        })
    }

    /// Return the channelized work and the original IQ reads for this raster.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.report.replayed_iq_samples,
            eligible_attempts: self
                .report
                .channels
                .iter()
                .map(|channel| channel.replayed_dwells)
                .sum(),
            short_attempts: self
                .report
                .channels
                .iter()
                .map(|channel| channel.short_dwells)
                .sum(),
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
            max_budget_samples: self.max_total_samples,
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
            .context("missing RS41 raster dwell route")?;
        let outcome = if route.is_short() {
            Outcome::ShortDwell
        } else {
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
                        "replay RS41 raster from pass {}/{}",
                        route.pass_index, route.tune.directory
                    )
                })?;
            self.report.valid_candidates += replay.valid_candidates();
            self.report.validated_frames += replay.validated_frames();
            self.report.retained_frames += replay.retained_frames();
            self.report.omitted_valid_frames +=
                replay.validated_frames() - replay.retained_frames();
            Outcome::Replayed { replay }
        };
        self.report.dwells.push(route.dwell(outcome));
        Ok(())
    }
}

/// Route a radiosonde raster into one admitted sweep or series.
///
/// Each eligible dwell is read once through a shared channelizer. A plan-only
/// run reads capture metadata and prints scheduled coverage to stdout.
///
/// # Errors
/// Rejects invalid or changed captures, bad raster bounds, an over-budget
/// plan, invalid IQ, interruption, or failed report publication.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let bounds = Bounds::new(args.lower_hz, args.upper_hz)?;
    let capture = CompletedCapture::open(&root.join(&args.capture_directory), args.series)?;
    let limits = Limits {
        max_samples_per_dwell: args.max_samples_per_dwell,
        max_total_samples: None,
        max_channelized_samples: args.max_channelized_samples,
        max_frames: args.max_frames,
    };
    let mut plan =
        Plan::prepare_with_budget(&capture, bounds, limits, "--max-channelized-samples", stop)?;
    if args.plan_only {
        capture.verify()?;
        check_stop(stop)?;
        plan.report.operation = "plan_rs41_raster_in_saved_sweeps";
        for route in plan.routes {
            let outcome = if route.is_short() {
                Outcome::ShortDwell
            } else {
                Outcome::Scheduled
            };
            plan.report.dwells.push(route.dwell(outcome));
        }
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &plan.report)?;
        writeln!(stdout)?;
        stdout.flush()?;
        return Ok(());
    }

    let report = plan.replay(stop)?;
    let output = root.join(args.output.context("supply --output for raster replay")?);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} RS41 frames across {} replayed dwells. Output: {}",
        report.validated_frames(),
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}
