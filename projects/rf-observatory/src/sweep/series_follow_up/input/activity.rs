//! Admit activity selections from the report paired with series triage.

use std::io::Read;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::file_pin;
use crate::triage::series::MAX_ACTIVITY_PER_DWELL;

use super::super::activity::{Kind, Selection};
use super::{CELL_HZ, Source, WINDOW_SAMPLES, cell_offsets, within_sideband};

// At most 2,000 dwells and 16 rows of each kind per dwell.
const MAX_REPORT_BYTES: u64 = 32 * 1024 * 1024;
const MINIMUM_PEAK_EXCESS_DB: f64 = 6.0;

/// Activity receipt digest and selected rows in caller order.
pub(in crate::sweep::series_follow_up) struct ActivitySource<'a> {
    pub(in crate::sweep::series_follow_up) report_sha256: String,
    pub(in crate::sweep::series_follow_up) targets: Vec<TargetEvidence<'a>>,
}

/// Selected geometry and the paired triage evidence for its saved dwell.
pub(in crate::sweep::series_follow_up) struct TargetEvidence<'a> {
    pub(in crate::sweep::series_follow_up) selection: Selection,
    pub(in crate::sweep::series_follow_up) requested_center_hz: u64,
    pub(in crate::sweep::series_follow_up) observed_center_hz: u64,
    pub(in crate::sweep::series_follow_up) saved_samples: u64,
    pub(in crate::sweep::series_follow_up) rf: Range<u64>,
    pub(in crate::sweep::series_follow_up) samples: Range<u64>,
    pub(in crate::sweep::series_follow_up) analyzed_samples: Range<u64>,
    pub(in crate::sweep::series_follow_up) analyzed_iq_sha256: &'a str,
    pub(in crate::sweep::series_follow_up) source_sweep_sha256: &'a str,
    pub(in crate::sweep::series_follow_up) source_sidecar_sha256: &'a str,
}

#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    operation: String,
    source_series_sha256: String,
    series_triage_sha256: String,
    max_activity_per_dwell: usize,
    passes: Vec<Pass>,
}

#[derive(Deserialize)]
struct Pass {
    directory: String,
    tunes: Vec<Tune>,
}

#[derive(Deserialize)]
struct Tune {
    directory: String,
    observed_center_hz: u64,
    transient_event_count: usize,
    transient_detector_omitted_count: usize,
    transient_eligible_retained_count: usize,
    mean_power_region_count: usize,
    transients: Vec<Transient>,
    mean_power_regions: Vec<MeanPower>,
}

#[derive(Deserialize)]
struct Transient {
    #[serde(flatten)]
    geometry: Geometry,
    hit_tile_count: u64,
}

#[derive(Deserialize)]
struct MeanPower {
    #[serde(flatten)]
    geometry: Geometry,
    cell_count: u64,
}

#[derive(Deserialize)]
struct Geometry {
    start_sample: u64,
    end_sample_exclusive: u64,
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    peak_excess_db: f64,
}

impl Report {
    fn validate(&self, triage: &Source) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.operation == "offline_rf_sweep_series_activity",
            "unsupported sweep series activity report"
        );
        ensure!(
            self.source_series_sha256 == triage.source_series_sha256
                && self.series_triage_sha256 == triage.report_sha256,
            "activity report does not match the source series and series triage digests"
        );
        let limit = self.max_activity_per_dwell;
        ensure!(
            (1..=MAX_ACTIVITY_PER_DWELL).contains(&limit)
                && self.passes.len() == triage.passes.len(),
            "invalid activity report pass count or row limit"
        );
        for (pass, source) in self.passes.iter().zip(&triage.passes) {
            ensure!(
                pass.directory == source.directory && pass.tunes.len() == source.tunes.len(),
                "activity report pass differs from series triage at {}",
                source.directory
            );
            for (tune, source_tune) in pass.tunes.iter().zip(&source.tunes) {
                ensure!(
                    tune.directory == source_tune.directory
                        && tune.observed_center_hz == source_tune.observed_center_hz,
                    "activity report tune differs from series triage at {}/{}",
                    source.directory,
                    source_tune.directory
                );
                ensure!(
                    tune.transient_event_count
                        .checked_sub(tune.transient_detector_omitted_count)
                        .is_some_and(|retained| retained >= tune.transient_eligible_retained_count)
                        && tune.transients.len()
                            == tune.transient_eligible_retained_count.min(limit)
                        && tune.mean_power_regions.len() == tune.mean_power_region_count.min(limit),
                    "invalid activity report row counts at {}/{}",
                    source.directory,
                    source_tune.directory
                );
            }
        }
        Ok(())
    }

    fn target<'a>(&self, triage: &'a Source, selection: &Selection) -> Result<TargetEvidence<'a>> {
        let pass_index = selection
            .pass
            .checked_sub(1)
            .context("pass indices start at 1")?;
        let tune_index = selection
            .tune
            .checked_sub(1)
            .context("tune indices start at 1")?;
        let row_index = selection
            .row
            .checked_sub(1)
            .context("row indices start at 1")?;
        let pass = self
            .passes
            .get(pass_index)
            .context("unknown activity pass")?;
        let tune = pass
            .tunes
            .get(tune_index)
            .context("unknown activity tune")?;
        let source_pass = &triage.passes[pass_index];
        let source_tune = &source_pass.tunes[tune_index];
        let (row, hit_tiles, mean_cells) = match selection.kind {
            Kind::Transient => {
                let row = tune
                    .transients
                    .get(row_index)
                    .context("unknown transient row")?;
                (&row.geometry, Some(row.hit_tile_count), None)
            }
            Kind::MeanPower => {
                let row = tune
                    .mean_power_regions
                    .get(row_index)
                    .context("unknown mean-power row")?;
                (&row.geometry, None, Some(row.cell_count))
            }
        };
        let rf = row.lower_frequency_hz..row.upper_frequency_hz_exclusive;
        let cells = cell_offsets(tune.observed_center_hz, rf.start, rf.end)
            .context("invalid activity frequency bounds")?;
        let requested = triage
            .requested_range
            .as_ref()
            .filter(|_| triage.visit_centers_hz.is_none());
        ensure!(
            within_sideband(&cells)
                && requested.is_none_or(|range| rf.start < range.end && range.start < rf.end),
            "activity frequencies do not fit a guarded sideband of the requested scope"
        );
        ensure!(
            row.peak_excess_db.is_finite() && row.peak_excess_db >= MINIMUM_PEAK_EXCESS_DB,
            "invalid activity peak excess"
        );
        let samples = row.start_sample..row.end_sample_exclusive;
        ensure!(samples.start < samples.end, "empty activity sample extent");
        let (analyzed_samples, analyzed_iq_sha256) = source_tune
            .analyzed_extents()
            .find(|(extent, _)| extent.start <= samples.start && samples.end <= extent.end)
            .context("activity samples do not fit within one analyzed interval")?;
        let cell_count = (cells.end - cells.start) / CELL_HZ;
        if let Some(hits) = hit_tiles {
            let stride = triage.stride.samples() as u64;
            let extension = (samples.end - samples.start)
                .checked_sub(WINDOW_SAMPLES)
                .context("transient extent is shorter than one FFT window")?;
            ensure!(
                (samples.start - analyzed_samples.start).is_multiple_of(stride)
                    && extension.is_multiple_of(stride)
                    && (1..=cell_count * (1 + extension / stride)).contains(&hits),
                "invalid transient window alignment or hit count"
            );
        }
        if let Some(count) = mean_cells {
            ensure!(
                samples == analyzed_samples && count == cell_count,
                "mean-power row must describe one complete analyzed interval and its cells"
            );
        }
        Ok(TargetEvidence {
            selection: *selection,
            requested_center_hz: source_tune.requested_center_hz,
            observed_center_hz: tune.observed_center_hz,
            saved_samples: source_tune.saved_samples,
            rf,
            samples,
            analyzed_samples,
            analyzed_iq_sha256,
            source_sweep_sha256: &source_pass.source_sweep_sha256,
            source_sidecar_sha256: &source_tune.source_sidecar_sha256,
        })
    }
}

/// Read one bounded activity report, link it to triage, and admit selected rows.
/// Returns the analyzed intervals and hashes reported by the paired triage.
///
/// # Errors
/// Rejects unstable or mismatched inputs and invalid selected row geometry.
pub(in crate::sweep::series_follow_up) fn read<'a>(
    path: &Path,
    triage: &'a Source,
    selections: &[Selection],
) -> Result<ActivitySource<'a>> {
    let (file, length) = file_pin::open_regular(path, MAX_REPORT_BYTES)?;
    let before = file.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&file).take(length + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == length
            && before.len() == length
            && file_pin::same_file(&before, &file.metadata()?),
        "series activity report changed while reading"
    );
    let report: Report = serde_json::from_slice(&bytes).context("parse series activity report")?;
    report.validate(triage)?;
    let targets = selections
        .iter()
        .map(|selection| {
            report.target(triage, selection).with_context(|| {
                format!(
                    "validate activity target pass {} tune {} row {}",
                    selection.pass, selection.tune, selection.row
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ActivitySource {
        report_sha256: HEXLOWER.encode(&Sha256::digest(bytes)),
        targets,
    })
}
