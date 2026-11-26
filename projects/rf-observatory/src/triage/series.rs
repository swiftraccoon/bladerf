//! Correlate recurring bands and retain optional per-dwell activity evidence.

mod activity;
pub(crate) mod activity_source;
mod bands;

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use clap::Args as ClapArgs;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::private_output::StagedDirectory;
use crate::spectrum::{CELL_WIDTH_HZ, EventStride, EventWindows, FFT};
use crate::sweep::CompletedOperation;

use self::bands::{Band, Votes};
use super::admission::{Series, Tune};
use super::sampling::{self, IntervalEvidence, MAX_TIME_SLICES};
use super::{MAX_SAMPLES_PER_TUNE, MAX_TOTAL_SAMPLES, check_stop};

#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Directory containing a completed sweep-series.json and its pass directories.
    #[arg(long)]
    series_directory: PathBuf,
    /// Fresh directory for the recurrence report.
    #[arg(long)]
    output_directory: PathBuf,
    /// Number of distinct passes required to report a band.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(2..=16))]
    min_passes: u32,
    /// Maximum number of bands per detector in the report.
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u32).range(1..=256))]
    max_bands: u32,
    /// Maximum samples analyzed per saved dwell, rounded down to complete FFT windows.
    #[arg(long, default_value_t = MAX_SAMPLES_PER_TUNE, value_parser = clap::value_parser!(u64).range(FFT as u64..=MAX_SAMPLES_PER_TUNE))]
    max_samples_per_tune: u64,
    /// Shared analysis budget, divided equally among all saved dwells.
    #[arg(long, default_value_t = MAX_TOTAL_SAMPLES, value_parser = clap::value_parser!(u64).range(FFT as u64..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Distribute the sample budget across up to this many intervals per dwell.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_TIME_SLICES)))]
    time_slices: u32,
    /// Overlap consecutive event FFT windows by half a window within each analyzed interval.
    #[arg(long)]
    overlap_events: bool,
    /// Write contiguous and overlapping event reports from the same IQ reads.
    #[arg(long, conflicts_with = "overlap_events")]
    both_event_strides: bool,
    /// Write up to this many transient and mean-power rows each per dwell in the activity reports.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=MAX_ACTIVITY_PER_DWELL as i64))]
    max_activity_per_dwell: Option<u32>,
}

/// Maximum retained rows per detector and dwell in a series activity report.
pub(crate) const MAX_ACTIVITY_PER_DWELL: usize = 16;

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    source_series_sha256: &'a str,
    source_operation: CompletedOperation,
    requested_range: &'a Option<Range<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visit_centers_hz: Option<&'a [u64]>,
    sample_format: &'static str,
    sample_rate_hz: u32,
    fft_samples: usize,
    cell_width_hz: u64,
    min_passes: u32,
    max_bands: u32,
    max_samples_per_tune: u64,
    max_total_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_slices: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_window_stride_samples: Option<usize>,
    passes: Vec<PassReport<'a>>,
    mean_power_band_count: usize,
    narrow_band_count: usize,
    mean_power_bands: Vec<Band>,
    narrow_bands: Vec<Band>,
}

#[derive(Serialize)]
struct PassReport<'a> {
    directory: &'a str,
    source_sweep_sha256: &'a str,
    tunes: Vec<TuneReport<'a>>,
}

#[derive(Serialize)]
struct TuneReport<'a> {
    directory: &'a str,
    requested_center_hz: u64,
    observed_center_hz: u64,
    source_sidecar_sha256: &'a str,
    saved_samples: u64,
    analyzed_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    analyzed_iq_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analyzed_intervals: Option<Vec<IntervalEvidence>>,
    narrow_candidate_count: usize,
    omitted_narrow_candidate_count: usize,
    #[serde(skip)]
    activity: Option<activity::Dwell>,
}

#[derive(Serialize)]
struct ActivityReport<'a> {
    schema_version: u32,
    operation: &'static str,
    source_series_sha256: &'a str,
    series_triage_sha256: String,
    max_activity_per_dwell: u32,
    passes: Vec<ActivityPass<'a>>,
}

#[derive(Serialize)]
struct ActivityPass<'a> {
    directory: &'a str,
    tunes: Vec<ActivityTune<'a>>,
}

#[derive(Serialize)]
struct ActivityTune<'a> {
    directory: &'a str,
    observed_center_hz: u64,
    #[serde(flatten)]
    activity: &'a activity::Dwell,
}

struct Survey<'a> {
    sample_limit: u64,
    time_slices: u32,
    event_windows: EventWindows,
    requested: Option<&'a Range<u64>>,
    max_activity_per_dwell: Option<u32>,
}

fn inspect<'a>(
    tune: &'a Tune,
    pass: usize,
    survey: &Survey<'_>,
    votes: &mut [Votes],
    stop: &AtomicBool,
) -> Result<Vec<TuneReport<'a>>> {
    let mut input = tune.open_iq()?;
    let ranges = sampling::intervals(tune.samples, survey.sample_limit, survey.time_slices);
    ensure!(
        !ranges.is_empty(),
        "spectrum requires at least {FFT} samples"
    );
    let mut evidence = Vec::with_capacity(ranges.len());
    let mut analyzed_samples = 0;
    let mut reports: Vec<_> = votes
        .iter()
        .map(|_| TuneReport {
            directory: &tune.directory,
            requested_center_hz: tune.requested_center_hz,
            observed_center_hz: tune.observed_center_hz,
            source_sidecar_sha256: &tune.sidecar_sha256,
            saved_samples: tune.samples,
            analyzed_samples: 0,
            analyzed_iq_sha256: None,
            analyzed_intervals: None,
            narrow_candidate_count: 0,
            omitted_narrow_candidate_count: 0,
            activity: survey
                .max_activity_per_dwell
                .map(|limit| activity::Dwell::new(limit as usize)),
        })
        .collect();
    for range in ranges {
        let (power, interval, candidates) = sampling::survey(
            &mut input,
            tune.observed_center_hz,
            range,
            survey.event_windows,
            true,
            stop,
        )?;
        analyzed_samples += interval.end_sample_exclusive - interval.start_sample;
        ensure!(
            votes.len() == 1 + usize::from(candidates.overlap.is_some()),
            "missing or unexpected overlapping detector results"
        );
        let candidates = std::iter::once(candidates.primary).chain(candidates.overlap);
        for ((votes, report), candidates) in votes.iter_mut().zip(&mut reports).zip(candidates) {
            let narrow = candidates
                .narrow
                .context("missing narrow detector results")?;
            report.narrow_candidate_count += narrow.count;
            report.omitted_narrow_candidate_count += narrow.count - narrow.retained.len();
            votes.add_power(
                pass,
                tune.observed_center_hz,
                power.activity_detection.mean_activity_cells(),
            );
            votes.add_narrow(pass, tune.observed_center_hz, &narrow.retained);
            if let Some(activity) = &mut report.activity {
                activity.observe(
                    tune.observed_center_hz,
                    survey.requested,
                    &interval,
                    &power.activity_detection,
                    candidates.components,
                );
            }
        }
        evidence.push(interval);
    }
    tune.verify_iq(&input)?;
    let (analyzed_iq_sha256, analyzed_intervals) = if survey.time_slices > 1 {
        (None, Some(evidence))
    } else {
        (
            evidence.pop().map(|interval| interval.analyzed_iq_sha256),
            None,
        )
    };
    for report in &mut reports {
        report.analyzed_samples = analyzed_samples;
        report.analyzed_iq_sha256.clone_from(&analyzed_iq_sha256);
        report.analyzed_intervals.clone_from(&analyzed_intervals);
    }
    Ok(reports)
}

fn write_activity(recurrence: &Path, path: &Path, report: &Report<'_>, limit: u32) -> Result<()> {
    let passes = report
        .passes
        .iter()
        .map(|pass| {
            let tunes = pass
                .tunes
                .iter()
                .map(|tune| {
                    Ok(ActivityTune {
                        directory: tune.directory,
                        observed_center_hz: tune.observed_center_hz,
                        activity: tune.activity.as_ref().context("missing dwell activity")?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(ActivityPass {
                directory: pass.directory,
                tunes,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let triage = fs::read(recurrence).context("read staged series triage for its digest")?;
    crate::json_file(
        path,
        &ActivityReport {
            schema_version: 1,
            operation: "offline_rf_sweep_series_activity",
            source_series_sha256: report.source_series_sha256,
            series_triage_sha256: format!("{:x}", Sha256::digest(triage)),
            max_activity_per_dwell: limit,
            passes,
        },
    )
}

fn retain_bands(retained: &mut Vec<Band>, added: Vec<Band>, limit: usize) -> usize {
    let count = added.len();
    retained.extend(added);
    retained.sort_by(Band::compare);
    retained.truncate(limit);
    count
}

/// Compare measured activity across each admitted pass on an equal sample budget.
///
/// A cell receives at most one vote per pass even when several intervals
/// contribute measurements. Optional dwell activity uses the same FFT results
/// as its recurrence report. When both strides are selected, each report ranks
/// its evidence independently using the same admitted IQ intervals.
pub(crate) fn run(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let series = Series::open(&root.join(args.series_directory))?;
    let reference = &series.passes[0].sweep;
    let pass_count = series.passes.len();
    let min_passes = args.min_passes as usize;
    ensure!(
        min_passes <= pass_count,
        "minimum pass count exceeds the completed series"
    );
    let tunes_per_pass = reference.tunes.len();
    let max_bands = args.max_bands as usize;
    let sample_limit = (args.max_total_samples / (tunes_per_pass * pass_count) as u64)
        .min(args.max_samples_per_tune);
    ensure!(
        sample_limit >= FFT as u64,
        "analysis budget needs one complete FFT window per dwell"
    );
    let primary_stride = if args.overlap_events {
        EventStride::HalfWindow
    } else {
        EventStride::Contiguous
    };
    let survey = Survey {
        sample_limit,
        time_slices: args.time_slices,
        event_windows: if args.both_event_strides {
            EventWindows::Both
        } else {
            EventWindows::Stride(primary_stride)
        },
        requested: reference.analysis_range(),
        max_activity_per_dwell: args.max_activity_per_dwell,
    };
    let destination = root.join(args.output_directory);
    let stage = StagedDirectory::new(&destination, ".series-triage-")?;
    let time_slices = (args.time_slices > 1).then_some(args.time_slices);
    let strides = std::iter::once(primary_stride)
        .chain(args.both_event_strides.then_some(EventStride::HalfWindow));
    let mut reports: Vec<_> = strides
        .map(|stride| {
            let event_window_stride_samples =
                matches!(stride, EventStride::HalfWindow).then_some(stride.samples());
            Report {
                schema_version: match (event_window_stride_samples, time_slices) {
                    (Some(_), _) => 3,
                    (None, Some(_)) => 2,
                    (None, None) => 1,
                },
                operation: "offline_rf_sweep_series_triage",
                source_series_sha256: &series.source_series_sha256,
                source_operation: reference.operation,
                requested_range: &reference.requested_range,
                visit_centers_hz: reference.visit_centers_hz.as_deref(),
                sample_format: "SC16_Q11_LE",
                sample_rate_hz: crate::SAMPLE_RATE,
                fft_samples: FFT,
                cell_width_hz: CELL_WIDTH_HZ as u64,
                min_passes: args.min_passes,
                max_bands: args.max_bands,
                max_samples_per_tune: args.max_samples_per_tune,
                max_total_samples: args.max_total_samples,
                time_slices,
                event_window_stride_samples,
                passes: series
                    .passes
                    .iter()
                    .map(|pass| PassReport {
                        directory: &pass.directory,
                        source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                        tunes: Vec::with_capacity(tunes_per_pass),
                    })
                    .collect(),
                mean_power_band_count: 0,
                narrow_band_count: 0,
                mean_power_bands: Vec::new(),
                narrow_bands: Vec::new(),
            }
        })
        .collect();
    for (index, tune) in reference.tunes.iter().enumerate() {
        let mut votes: Vec<_> = reports
            .iter()
            .map(|_| Votes::new(tune.requested_center_hz, pass_count, survey.requested))
            .collect();
        for (pass_index, pass) in series.passes.iter().enumerate() {
            check_stop(stop)?;
            let tune = &pass.sweep.tunes[index];
            let observed = inspect(tune, pass_index, &survey, &mut votes, stop)
                .with_context(|| format!("analyze {}/{}", pass.directory, tune.directory))?;
            for (report, dwell) in reports.iter_mut().zip(observed) {
                report.passes[pass_index].tunes.push(dwell);
            }
        }
        let directory = reference
            .visit_centers_hz
            .as_ref()
            .map(|_| tune.directory.as_str());
        for (report, votes) in reports.iter_mut().zip(votes) {
            let (mean_power, narrow) = votes.finish(min_passes, directory);
            report.mean_power_band_count +=
                retain_bands(&mut report.mean_power_bands, mean_power, max_bands);
            report.narrow_band_count += retain_bands(&mut report.narrow_bands, narrow, max_bands);
        }
    }
    let paths = [
        ("series-triage.json", "activity.json"),
        ("series-triage-overlap.json", "activity-overlap.json"),
    ];
    for (report, (recurrence, activity)) in reports.iter().zip(paths) {
        check_stop(stop)?;
        let recurrence = stage.path().join(recurrence);
        crate::json_file(&recurrence, report)?;
        if let Some(limit) = survey.max_activity_per_dwell {
            check_stop(stop)?;
            write_activity(&recurrence, &stage.path().join(activity), report, limit)?;
        }
    }
    stage.publish(|| {
        series.verify()?;
        check_stop(stop)
    })?;
    let primary = &reports[0];
    println!(
        "Compared {pass_count} passes. Mean-power bands: {}. Narrow bands: {}. Output: {}",
        primary.mean_power_band_count,
        primary.narrow_band_count,
        destination.display()
    );
    if let Some(overlap) = reports.get(1) {
        println!(
            "With overlapping windows, mean-power bands: {}. Narrow bands: {}.",
            overlap.mean_power_band_count, overlap.narrow_band_count
        );
    }
    Ok(())
}
