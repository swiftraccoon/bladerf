//! Evaluate fixed-RF follow-up bands and their simultaneous power controls.

mod input;
mod probes;

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use clap::Args as ClapArgs;
use serde::Serialize;

use crate::private_output::StagedDirectory;
use crate::sweep::plan;
use crate::waveform::burst::{self, Detection, MeasuredBand, THRESHOLD_DB, WINDOW_SAMPLES};

use self::input::{Input, Target};
use super::admission::Tune;
use super::{MAX_TOTAL_SAMPLES, check_stop};

const MAX_SAMPLES_PER_DWELL: u64 = crate::SAMPLE_RATE as u64;

#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Directory of a completed sweep captured from a series-triage report.
    #[arg(long)]
    capture_directory: PathBuf,
    /// Original series-triage.json used to plan the capture.
    #[arg(long)]
    series_triage_report: PathBuf,
    /// Overlapping-window series triage report used as a supplemental source.
    #[arg(long)]
    overlap_series_triage_report: Option<PathBuf>,
    /// Fresh directory for the analysis report.
    #[arg(long)]
    output_directory: PathBuf,
    /// Maximum original-rate samples analyzed in each saved dwell.
    #[arg(long, default_value_t = MAX_SAMPLES_PER_DWELL, value_parser = clap::value_parser!(u64).range((2 * WINDOW_SAMPLES) as u64..=MAX_SAMPLES_PER_DWELL))]
    max_samples_per_dwell: u64,
    /// Sample budget divided across all target and dwell comparisons.
    #[arg(long, default_value_t = MAX_TOTAL_SAMPLES, value_parser = clap::value_parser!(u64).range((2 * WINDOW_SAMPLES) as u64..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Measure narrow waveforms in up to this many isolated runs.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=256))]
    waveform_probes: Option<u32>,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    source_capture_sha256: &'a str,
    source_plan_sha256: &'a str,
    source_series_sha256: &'a str,
    source_series_triage_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_triage_overlap_sha256: Option<&'a str>,
    sample_rate_hz: u32,
    window_samples: usize,
    threshold_db: f64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    used_sample_limit: usize,
    passes: Vec<PassReport<'a>>,
    targets: Vec<TargetReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_analysis: Option<probes::Analysis>,
}

#[derive(Serialize)]
struct PassReport<'a> {
    directory: Option<&'a str>,
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
    analyzed_samples: usize,
    analyzed_iq_sha256: String,
}

#[derive(Serialize)]
struct TargetReport {
    source_row: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlap_source_row: Option<usize>,
    lower_hz: u64,
    upper_hz: u64,
    target_hz: u64,
    centers_hz: [u64; 2],
    centers_with_isolated_runs: Vec<u64>,
    observations: Vec<Observation>,
}

impl TargetReport {
    fn new(target: &Target) -> Self {
        Self {
            source_row: target.source_row,
            overlap_source_row: target.overlap_source_row,
            lower_hz: target.lower_hz,
            upper_hz: target.upper_hz,
            target_hz: target.target_hz,
            centers_hz: target.centers_hz,
            centers_with_isolated_runs: Vec::new(),
            observations: Vec::new(),
        }
    }
}

#[derive(Serialize)]
struct Observation {
    pass_index: usize,
    requested_center_hz: u64,
    observed_center_hz: u64,
    target_integrated_fft_bins: usize,
    target_baseline_band_power: f64,
    target_detections: usize,
    controls: Vec<ControlReport>,
    isolated_runs: Option<Vec<Detection>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlKind {
    LowerFlank,
    UpperFlank,
    SourceIf,
    Mirror,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlStatus {
    Measured,
    ZeroBaseline,
    OutsideSideband,
}

#[derive(Serialize)]
struct ControlReport {
    kind: ControlKind,
    lower_hz: u64,
    upper_hz: u64,
    status: ControlStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_band_power: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrated_fft_bins: Option<usize>,
}

struct Control {
    kind: ControlKind,
    band: Range<u64>,
    index: Option<usize>,
}

impl Control {
    fn measured<'a>(&self, bands: &'a [MeasuredBand]) -> Option<&'a MeasuredBand> {
        self.index.map(|index| &bands[index])
    }

    fn report(&self, bands: &[MeasuredBand]) -> ControlReport {
        let measured = self.measured(bands);
        ControlReport {
            kind: self.kind,
            lower_hz: self.band.start,
            upper_hz: self.band.end,
            status: match measured {
                Some(band) if band.baseline_band_power > 0.0 => ControlStatus::Measured,
                Some(_) => ControlStatus::ZeroBaseline,
                None => ControlStatus::OutsideSideband,
            },
            baseline_band_power: measured.map(|band| band.baseline_band_power),
            integrated_fft_bins: measured.map(|band| band.integrated_fft_bins),
        }
    }
}

struct Comparison {
    target_index: usize,
    primary: usize,
    controls: [Control; 4],
}

impl Comparison {
    fn observe(&self, tune: &Tune, pass_index: usize, bands: &[MeasuredBand]) -> Observation {
        let primary = &bands[self.primary];
        let controls = self
            .controls
            .iter()
            .map(|control| control.report(bands))
            .collect();
        let masks = self
            .controls
            .iter()
            .map(|control| {
                control
                    .measured(bands)
                    .filter(|band| band.baseline_band_power > 0.0)
            })
            .collect::<Option<Vec<_>>>();
        let isolated_runs = masks
            .filter(|_| primary.baseline_band_power > 0.0)
            .map(|masks| {
                primary
                    .detections
                    .iter()
                    .filter(|run| {
                        let windows = run.start_sample / WINDOW_SAMPLES
                            ..run.end_sample_exclusive / WINDOW_SAMPLES;
                        masks.iter().all(|band| {
                            !band.above_threshold[windows.clone()].iter().any(|&hit| hit)
                        })
                    })
                    .map(|run| Detection {
                        start_sample: run.start_sample,
                        end_sample_exclusive: run.end_sample_exclusive,
                        peak_excess_db: run.peak_excess_db,
                    })
                    .collect()
            });
        Observation {
            pass_index,
            requested_center_hz: tune.requested_center_hz,
            observed_center_hz: tune.observed_center_hz,
            target_integrated_fft_bins: primary.integrated_fft_bins,
            target_baseline_band_power: primary.baseline_band_power,
            target_detections: primary.detections.len(),
            controls,
            isolated_runs,
        }
    }
}

#[derive(Default)]
struct BandSet {
    ranges: Vec<Range<u64>>,
    indexes: BTreeMap<(u64, u64), usize>,
}

impl BandSet {
    fn insert(&mut self, band: Range<u64>) -> usize {
        *self
            .indexes
            .entry((band.start, band.end))
            .or_insert_with(|| {
                let index = self.ranges.len();
                self.ranges.push(band);
                index
            })
    }
}

fn covered(center_hz: u64, band: &Range<u64>) -> bool {
    plan::covers(band.start, band.end, plan::observed_bands(center_hz, 0))
}

fn comparisons(tune: &Tune, targets: &[Target], bands: &mut BandSet) -> Result<Vec<Comparison>> {
    let center = tune.observed_center_hz;
    let mut comparisons = Vec::new();
    for (target_index, target) in targets.iter().enumerate() {
        if !target.centers_hz.contains(&tune.requested_center_hz) {
            continue;
        }
        let (lower, upper) = (target.lower_hz, target.upper_hz);
        let band = lower..upper;
        ensure!(
            covered(center, &band),
            "target row {} lies outside the observed sideband",
            target.source_row
        );
        let primary = bands.insert(band);
        let width = upper - lower;
        // The translated band tests the same offset from the LO as the source.
        let shift = center as i64 - target.requested_center_hz as i64;
        let source_if = lower
            .checked_add_signed(shift)
            .context("invalid source IF lower edge")?
            ..upper
                .checked_add_signed(shift)
                .context("invalid source IF upper edge")?;
        let controls = [
            (ControlKind::LowerFlank, lower - width..lower),
            (ControlKind::UpperFlank, upper..upper + width),
            (ControlKind::SourceIf, source_if),
            (ControlKind::Mirror, 2 * center - upper..2 * center - lower),
        ]
        .map(|(kind, band)| {
            let index = covered(center, &band).then(|| bands.insert(band.clone()));
            Control { kind, band, index }
        });
        comparisons.push(Comparison {
            target_index,
            primary,
            controls,
        });
    }
    Ok(comparisons)
}

fn analyze_tune<'a>(
    tune: &'a Tune,
    pass_index: usize,
    targets: &[Target],
    reports: &mut [TargetReport],
    sample_limit: usize,
    stop: &AtomicBool,
) -> Result<TuneReport<'a>> {
    let mut bands = BandSet::default();
    let comparisons = comparisons(tune, targets, &mut bands)?;
    ensure!(
        !comparisons.is_empty(),
        "follow-up dwell has no planned target"
    );
    let iq = tune.open_iq()?;
    let measured = burst::measure_pinned(
        &iq,
        tune.observed_center_hz,
        &bands.ranges,
        sample_limit,
        stop,
    )?;
    tune.verify_iq(&iq)?;
    for comparison in comparisons {
        reports[comparison.target_index]
            .observations
            .push(comparison.observe(tune, pass_index, &measured.bands));
    }
    Ok(TuneReport {
        directory: &tune.directory,
        requested_center_hz: tune.requested_center_hz,
        observed_center_hz: tune.observed_center_hz,
        source_sidecar_sha256: &tune.sidecar_sha256,
        saved_samples: tune.samples,
        analyzed_samples: measured.analyzed_samples,
        analyzed_iq_sha256: measured.iq_sha256,
    })
}

/// Validate the capture and publish a bounded offline comparison of its target bands.
pub(crate) fn run(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let overlap_report = args
        .overlap_series_triage_report
        .map(|path| root.join(path));
    let input = Input::open(
        &root.join(args.capture_directory),
        &root.join(args.series_triage_report),
        overlap_report.as_deref(),
    )?;
    let passes = input.passes();
    let comparisons = passes.len() * input.targets.len() * 2;
    let sample_limit =
        ((args.max_total_samples / comparisons as u64).min(args.max_samples_per_dwell)) as usize;
    ensure!(
        sample_limit >= 2 * WINDOW_SAMPLES,
        "sample budget needs two complete windows per comparison"
    );
    let destination = root.join(args.output_directory);
    let stage = StagedDirectory::new(&destination, ".follow-up-analysis-")?;
    let mut target_reports: Vec<_> = input.targets.iter().map(TargetReport::new).collect();
    let mut pass_reports = Vec::with_capacity(passes.len());
    for (index, pass) in passes.iter().enumerate() {
        let mut tunes = Vec::with_capacity(pass.sweep.tunes.len());
        for tune in &pass.sweep.tunes {
            check_stop(stop)?;
            tunes.push(
                analyze_tune(
                    tune,
                    index + 1,
                    &input.targets,
                    &mut target_reports,
                    sample_limit,
                    stop,
                )
                .with_context(|| format!("analyze pass {}/{}", index + 1, tune.directory))?,
            );
        }
        pass_reports.push(PassReport {
            directory: pass.directory,
            source_sweep_sha256: &pass.sweep.source_receipt_sha256,
            tunes,
        });
    }
    for target in &mut target_reports {
        target.centers_with_isolated_runs = target
            .centers_hz
            .into_iter()
            .filter(|&center| {
                target.observations.iter().any(|observation| {
                    observation.requested_center_hz == center
                        && observation
                            .isolated_runs
                            .as_ref()
                            .is_some_and(|runs| !runs.is_empty())
                })
            })
            .collect();
    }
    let waveform_analysis = args
        .waveform_probes
        .map(|limit| {
            probes::measure(
                &passes,
                &pass_reports,
                &target_reports,
                limit as usize,
                stop,
            )
        })
        .transpose()?;
    let report = Report {
        schema_version: 1,
        operation: "offline_rf_series_follow_up_analysis",
        source_capture_sha256: &input.source_capture_sha256,
        source_plan_sha256: &input.source_plan_sha256,
        source_series_sha256: &input.source_series_sha256,
        source_series_triage_sha256: &input.source_series_triage_sha256,
        source_series_triage_overlap_sha256: input.source_series_triage_overlap_sha256.as_deref(),
        sample_rate_hz: crate::SAMPLE_RATE,
        window_samples: WINDOW_SAMPLES,
        threshold_db: THRESHOLD_DB,
        max_samples_per_dwell: args.max_samples_per_dwell,
        max_total_samples: args.max_total_samples,
        used_sample_limit: sample_limit,
        passes: pass_reports,
        targets: target_reports,
        waveform_analysis,
    };
    crate::json_file(&stage.path().join("follow-up-analysis.json"), &report)?;
    stage.publish(|| {
        input.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Analyzed {} target rows. Isolated power at both offsets: {}. Output: {}",
        report.targets.len(),
        report
            .targets
            .iter()
            .filter(|target| target.centers_with_isolated_runs.len() == 2)
            .count(),
        destination.display()
    );
    Ok(())
}
