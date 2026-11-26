//! Replay the 1090 MHz channel from individual dwells in an admitted capture.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::adsb::{self, CHANNEL_HZ, Channel, MAX_FRAME_RECORDS, MIN_SWEEP_SAMPLES};
use crate::dsp::AdsbChannel;
use crate::private_output::publish_json_file;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

/// Maximum original-rate samples replayed across all ADS-B dwells.
pub(super) const MAX_TOTAL_SAMPLES: u64 = 2_000_000_000;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const SPECTRUM_DC_GUARD_HZ: u64 = CENTER_GUARD_HZ as u64;
const _: () = assert!(1_000_000_000u32.is_multiple_of(SAMPLE_RATE));

/// Select a completed capture and limits for 1090 MHz replay.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Maximum original-rate IQ samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a replay plan above this total sample count across eligible dwells.
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
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    channel_half_width_hz: u64,
    spectrum_dc_guard_hz: u64,
    minimum_frame_samples: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    dc_guard_overlap_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    covered_samples: u64,
    replayed_iq_samples: u64,
    covered_time_ns: u64,
    replayed_time_ns: u64,
    validated_frames: usize,
    omitted_valid_frames: usize,
    dwells: Vec<Attempt<'a>>,
}

impl Report<'_> {
    /// Number of validated squitter occurrences across the replayed dwells.
    pub(super) fn validated_frames(&self) -> usize {
        self.validated_frames
    }
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune_directory: &'a str,
    source_sweep_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    overlaps_spectrum_dc_guard: bool,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    ShortDwell,
    Replayed { replay: adsb::Report },
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

    fn overlaps_spectrum_dc_guard(&self) -> bool {
        self.tune.observed_center_hz.abs_diff(CHANNEL_HZ)
            < AdsbChannel::HALF_WIDTH_HZ + SPECTRUM_DC_GUARD_HZ
    }

    fn attempt(
        self,
        input: &mut dyn Read,
        remaining_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Attempt<'a>> {
        let outcome = if self.is_short() {
            Outcome::ShortDwell
        } else {
            let replay = self.channel.replay(
                input,
                self.tune.samples,
                self.selected_samples,
                remaining_frames,
                stop,
            )?;
            Outcome::Replayed { replay }
        };
        Ok(Attempt {
            pass_index: self.pass_index,
            pass_directory: self.pass_directory,
            tune_directory: &self.tune.directory,
            source_sweep_sha256: self.source_sweep_sha256,
            source_sidecar_sha256: &self.tune.sidecar_sha256,
            observed_center_hz: self.tune.observed_center_hz,
            saved_samples: self.tune.samples,
            selected_samples: self.selected_samples,
            overlaps_spectrum_dc_guard: self.overlaps_spectrum_dc_guard(),
            outcome,
        })
    }
}

/// Ordered ADS-B dwell routes with an admitted sample budget.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    short_dwells: usize,
    replayed_iq_samples: u64,
    routes: VecDeque<Route<'a>>,
    dwells: Vec<Attempt<'a>>,
    remaining_frames: usize,
}

impl<'a> Plan<'a> {
    /// Summarize prefix reads and coverage for the 1090 MHz channel.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.replayed_iq_samples,
            eligible_attempts: self.routes.len() + self.dwells.len() - self.short_dwells,
            short_attempts: self.short_dwells,
            budget_samples: self.replayed_iq_samples,
            max_budget_samples: self.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// Report the scheduled source prefixes without reading IQ payloads.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.iter().map(Route::request)
    }

    /// Consume the plan and replay each eligible dwell with fresh decoder state.
    ///
    /// # Errors
    /// Rejects invalid or changed IQ and interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Assemble the report after the scheduled routes finish.
    fn finish(self) -> Report<'a> {
        debug_assert!(self.routes.is_empty());
        let covered_dwells = self.dwells.len();
        let dc_guard_overlap_dwells = self
            .dwells
            .iter()
            .filter(|attempt| attempt.overlaps_spectrum_dc_guard)
            .count();
        let covered_samples = self
            .dwells
            .iter()
            .map(|attempt| attempt.saved_samples)
            .sum();
        let validated_frames = self
            .dwells
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                Outcome::ShortDwell => None,
                Outcome::Replayed { replay } => Some(replay.validated_frames()),
            })
            .sum::<usize>();
        Report {
            schema_version: 1,
            operation: "identify_adsb_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            channel_frequency_hz: CHANNEL_HZ,
            sample_rate_hz: SAMPLE_RATE,
            channel_half_width_hz: AdsbChannel::HALF_WIDTH_HZ,
            spectrum_dc_guard_hz: SPECTRUM_DC_GUARD_HZ,
            minimum_frame_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_samples: self.max_total_samples,
            pass_count: self.pass_count,
            dwell_count: self.dwell_count,
            covered_dwells,
            dc_guard_overlap_dwells,
            short_dwells: self.short_dwells,
            replayed_dwells: covered_dwells - self.short_dwells,
            covered_samples,
            replayed_iq_samples: self.replayed_iq_samples,
            covered_time_ns: covered_samples * NANOSECONDS_PER_SAMPLE,
            replayed_time_ns: self.replayed_iq_samples * NANOSECONDS_PER_SAMPLE,
            validated_frames,
            omitted_valid_frames: validated_frames - (MAX_FRAME_RECORDS - self.remaining_frames),
            dwells: self.dwells,
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
            .context("missing planned ADS-B route")?;
        let pass_index = route.pass_index;
        let tune_directory = &route.tune.directory;
        let attempt = route
            .attempt(input, self.remaining_frames, stop)
            .with_context(|| format!("replay ADS-B for pass {pass_index}/{tune_directory}"))?;
        if let Outcome::Replayed { replay } = &attempt.outcome {
            self.remaining_frames -= replay.retained_frames();
        }
        self.dwells.push(attempt);
        Ok(())
    }
}

/// Plan extended squitter replay and validate its cost before reading IQ.
///
/// Coverage uses the admitted center and the decoder's RF margin. The spectrum
/// DC guard overlap is reported for context.
///
/// # Errors
/// Returns an error for excessive replay work or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let passes = capture.passes();
    let dwell_count = passes.iter().map(|pass| pass.sweep.tunes.len()).sum();
    let mut routes = Vec::new();
    for (index, pass) in passes.iter().enumerate() {
        for tune in &pass.sweep.tunes {
            let channel = Channel {
                center_hz: tune.observed_center_hz,
                sample_rate_hz: SAMPLE_RATE,
            };
            if channel.is_covered() {
                routes.push(Route {
                    channel,
                    pass_index: index + 1,
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                    tune,
                    selected_samples: tune.samples.min(max_samples_per_dwell),
                });
            }
        }
    }
    let short_dwells = routes.iter().filter(|route| route.is_short()).count();
    let replayed_iq_samples = routes
        .iter()
        .filter(|route| !route.is_short())
        .map(|route| route.selected_samples)
        .sum::<u64>();
    ensure!(
        replayed_iq_samples <= max_total_samples,
        "ADS-B sweep replay needs {replayed_iq_samples} samples, exceeding --max-total-samples ({max_total_samples})"
    );

    Ok(Plan {
        capture,
        max_samples_per_dwell,
        max_total_samples,
        pass_count: passes.len(),
        dwell_count,
        short_dwells,
        replayed_iq_samples,
        dwells: Vec::new(),
        remaining_frames: MAX_FRAME_RECORDS,
        routes: routes.into(),
    })
}

/// Publish provenance-bound extended squitter evidence from a completed capture.
///
/// # Errors
/// Returns an error for an invalid or changed source, an excessive replay plan,
/// invalid IQ, interruption, or publication failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let report = prepare(
        &capture,
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
        "Validated {} extended squitters across {} replayed dwells. Output: {}",
        report.validated_frames(),
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}
