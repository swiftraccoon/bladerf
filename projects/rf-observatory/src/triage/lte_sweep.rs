//! Route LTE carrier grids to individual dwells from a completed sweep.

mod plan;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::SAMPLE_RATE;
use crate::dsp::LteSyncChannel;
use crate::lte::raster::{
    self, CarrierReport, DEFAULT_ANALYSIS_SAMPLES, MAX_PREFIX_SAMPLES, MIN_SWEEP_SAMPLES,
};
use crate::private_output::publish_json_file;

use self::plan::{Dwell, FrequencyRange};
use super::admission::CompletedCapture;
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

/// Default aggregate replay budget for all LTE carriers and dwells.
pub(super) const DEFAULT_TOTAL_CHANNELIZED_SAMPLES: u64 = 256_000_000;
/// Maximum configurable aggregate replay budget for LTE channelized samples.
pub(super) const MAX_TOTAL_CHANNELIZED_SAMPLES: u64 = 2_000_000_000;

/// Select a carrier grid and analysis budgets for a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// First candidate carrier in hertz, aligned to 100 kHz.
    #[arg(long)]
    lower_hz: u64,
    /// Last candidate carrier in hertz, aligned to 100 kHz.
    #[arg(long)]
    upper_hz: u64,
    /// Verify public LTE broadcast information for each synchronized cell.
    #[arg(long)]
    lte_pbch: bool,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = DEFAULT_ANALYSIS_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_PREFIX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this total output sample count across carriers and dwells.
    #[arg(long, default_value_t = DEFAULT_TOTAL_CHANNELIZED_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_CHANNELIZED_SAMPLES))]
    max_total_channelized_samples: u64,
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
    lower_hz: u64,
    upper_hz: u64,
    raster_step_hz: u64,
    sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    channel_filter_delay_input_samples: usize,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    replayed_iq_samples: u64,
    planned_channelized_samples: u64,
    carrier_attempts: usize,
    /// Confirmed cell occurrences across the replayed carriers and dwells.
    pub(super) cell_reports: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pbch_cell_reports: Option<usize>,
    uncovered_ranges: Vec<FrequencyRange>,
    short_only_ranges: Vec<FrequencyRange>,
    dwells: Vec<Attempt<'a>>,
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune_directory: &'a str,
    source_sweep_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    lower_hz: u64,
    upper_hz: u64,
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
    Replayed { replay: DwellReplay },
}

#[derive(Serialize)]
struct DwellReplay {
    analyzed_iq_sha256: String,
    channelized_samples_per_channel: usize,
    pss_candidates: usize,
    sss_matches: usize,
    carriers: Vec<CarrierReport>,
}

impl<'a> Dwell<'a> {
    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.selected_samples < MIN_SWEEP_SAMPLES {
                0
            } else {
                self.selected_samples
            },
        }
    }
}

impl<'a> Attempt<'a> {
    fn run(
        dwell: &Dwell<'a>,
        input: &mut dyn Read,
        decode_pbch: bool,
        stop: &AtomicBool,
    ) -> Result<Self> {
        check_stop(stop)?;
        let tune = dwell.tune;
        let outcome = if dwell.selected_samples < MIN_SWEEP_SAMPLES {
            Outcome::ShortDwell
        } else {
            let grid = raster::Grid::new(tune.observed_center_hz, dwell.lower_hz, dwell.upper_hz)?;
            let mut analyzed = grid.replay(input, dwell.selected_samples, decode_pbch, stop)?;
            let pss_candidates = analyzed
                .channels
                .iter()
                .map(|carrier| carrier.evidence.pss_candidates)
                .sum();
            let sss_matches = analyzed
                .channels
                .iter()
                .map(|carrier| carrier.evidence.sss_matches)
                .sum();
            analyzed
                .channels
                .retain(|carrier| !carrier.evidence.cells.is_empty());
            Outcome::Replayed {
                replay: DwellReplay {
                    analyzed_iq_sha256: analyzed.analyzed_iq_sha256,
                    channelized_samples_per_channel: analyzed.channelized_samples_per_channel,
                    pss_candidates,
                    sss_matches,
                    carriers: analyzed.channels,
                },
            }
        };
        Ok(Self {
            pass_index: dwell.pass_index,
            pass_directory: dwell.pass_directory,
            tune_directory: &tune.directory,
            source_sweep_sha256: dwell.source_sweep_sha256,
            source_sidecar_sha256: &tune.sidecar_sha256,
            lower_hz: dwell.lower_hz,
            upper_hz: dwell.upper_hz,
            observed_center_hz: tune.observed_center_hz,
            saved_samples: tune.samples,
            selected_samples: dwell.selected_samples,
            outcome,
        })
    }
}

/// Replay each eligible dwell and publish detections with their sweep provenance.
///
/// # Errors
/// Returns an error for invalid or changed sources, inadmissible geometry or
/// budgets, invalid IQ, interruption, or publication failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    // Reject malformed raster arguments before opening the capture.
    raster::validate_endpoints(args.lower_hz, args.upper_hz)?;
    let directory = root.join(args.capture_directory);
    let capture = CompletedCapture::open(&directory, args.series)?;
    let report = prepare(
        &capture,
        args.lower_hz,
        args.upper_hz,
        args.max_samples_per_dwell,
        args.max_total_channelized_samples,
        stop,
    )?
    .with_pbch(args.lte_pbch)
    .replay(stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Recorded {} LTE cell reports across {} carrier attempts. Output: {}",
        report.cell_reports,
        report.carrier_attempts,
        output.display()
    );
    if let Some(count) = report.pbch_cell_reports {
        println!("Validated public LTE broadcast information for {count} cell reports.");
    }
    Ok(())
}

/// Prepare the selected LTE carrier grid without reading IQ.
///
/// # Errors
/// Returns an error for invalid geometry, work beyond the replay budget,
/// or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    lower_hz: u64,
    upper_hz: u64,
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let routing = plan::Plan::build(
        &capture.passes(),
        lower_hz,
        upper_hz,
        max_samples_per_dwell,
        max_total_channelized_samples,
    )?;
    Ok(Plan {
        capture,
        lower_hz,
        upper_hz,
        max_samples_per_dwell,
        max_total_channelized_samples,
        decode_pbch: false,
        routing,
        attempts: Vec::new(),
    })
}

/// LTE replay work and the admitted capture it references.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    lower_hz: u64,
    upper_hz: u64,
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    decode_pbch: bool,
    routing: plan::Plan<'a>,
    attempts: Vec<Attempt<'a>>,
}

impl<'a> Plan<'a> {
    /// Enable or disable PBCH checks within the same admitted prefixes.
    pub(super) fn with_pbch(mut self, decode_pbch: bool) -> Self {
        self.decode_pbch = decode_pbch;
        self
    }

    /// Return the source-prefix work and carrier attempts selected by this plan.
    pub(super) fn work(&self) -> Work {
        let short_attempts = self
            .routing
            .dwells
            .iter()
            .filter(|dwell| dwell.selected_samples < MIN_SWEEP_SAMPLES)
            .map(|dwell| ((dwell.upper_hz - dwell.lower_hz) / raster::GRID_SPACING_HZ + 1) as usize)
            .sum();
        Work {
            replayed_iq_samples: self.routing.replayed_iq_samples,
            eligible_attempts: self.routing.carrier_attempts,
            short_attempts,
            budget_samples: self.routing.planned_channelized_samples,
            max_budget_samples: self.max_total_channelized_samples,
            budget_unit: BudgetUnit::Channelized,
        }
    }

    /// Return the remaining planned requests in pass and tune order.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routing.dwells[self.attempts.len()..]
            .iter()
            .map(Dwell::request)
    }

    /// Replay the admitted prefixes. Reverify the capture before publication.
    ///
    /// # Errors
    /// Returns an error for invalid or changed IQ, or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Assemble the standalone receipt after all replay requests complete.
    fn finish(self) -> Report<'a> {
        debug_assert!(self.next_request().is_none());
        let cells = self
            .attempts
            .iter()
            .filter_map(|dwell| match &dwell.outcome {
                Outcome::ShortDwell => None,
                Outcome::Replayed { replay } => Some(&replay.carriers),
            })
            .flatten()
            .flat_map(|carrier| &carrier.evidence.cells);
        let cell_reports = cells.clone().count();
        let pbch_cell_reports = self
            .decode_pbch
            .then(|| cells.filter(|cell| cell.pbch_confirmed()).count());
        Report {
            schema_version: 1,
            operation: "identify_lte_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            lower_hz: self.lower_hz,
            upper_hz: self.upper_hz,
            raster_step_hz: raster::GRID_SPACING_HZ,
            sample_rate_hz: SAMPLE_RATE,
            channel_sample_rate_hz: LteSyncChannel::OUTPUT_RATE_HZ,
            channel_filter_delay_input_samples: LteSyncChannel::DELAY_INPUT_SAMPLES,
            minimum_dwell_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_channelized_samples: self.max_total_channelized_samples,
            replayed_iq_samples: self.routing.replayed_iq_samples,
            planned_channelized_samples: self.routing.planned_channelized_samples,
            carrier_attempts: self.routing.carrier_attempts,
            cell_reports,
            pbch_cell_reports,
            uncovered_ranges: self.routing.uncovered_ranges,
            short_only_ranges: self.routing.short_only_ranges,
            dwells: self.attempts,
        }
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.requests().next()
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        let dwell = self
            .routing
            .dwells
            .get(self.attempts.len())
            .context("LTE replay has no pending dwell")?;
        let attempt = Attempt::run(dwell, input, self.decode_pbch, stop).with_context(|| {
            format!(
                "replay LTE for pass {}/{}",
                dwell.pass_index, dwell.tune.directory
            )
        })?;
        self.attempts.push(attempt);
        Ok(())
    }
}
