//! Plan offset revisits to recurring narrow-cell bands from saved series triage.

pub(super) mod activity;
mod input;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    offset,
    plan::{self, Plan},
};

#[derive(Debug, Serialize)]
pub(super) struct SeriesFollowUp {
    narrow_band_count: usize,
    reported_band_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_reported_band_count: Option<usize>,
    targets: Vec<Target>,
    unplaced_rows: Vec<usize>,
}

#[derive(Debug, Serialize)]
struct Target {
    source_row: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlap_source_row: Option<usize>,
    requested_center_hz: u64,
    nominal_lower_hz: u64,
    nominal_upper_hz: u64,
    lower_hz: u64,
    upper_hz: u64,
    target_hz: u64,
    source_centers_hz: [u64; 2],
    support_count: usize,
    centers_hz: [u64; 2],
}

fn merge_bands(primary: &mut Vec<input::Band>, overlap: Vec<input::Band>) -> Result<()> {
    let key = |band: &input::Band| (band.tune_index, band.lower_hz, band.upper_hz);
    let primary_count = primary.len();
    let mut positions = BTreeMap::new();
    for (index, band) in primary.iter().enumerate() {
        ensure!(
            positions.insert(key(band), index).is_none(),
            "duplicate narrow band in primary report at row {}",
            band.row_index
        );
    }
    for mut band in overlap {
        let identity = key(&band);
        if let Some(&index) = positions.get(&identity) {
            let existing = &mut primary[index];
            ensure!(
                existing.overlap_row_index.is_none(),
                "duplicate narrow band in overlap report at row {}",
                band.row_index
            );
            existing.overlap_row_index = Some(band.row_index);
            existing.source_low_hz = existing.source_low_hz.min(band.source_low_hz);
            existing.source_high_hz = existing.source_high_hz.max(band.source_high_hz);
            existing.support_mask |= band.support_mask;
        } else {
            positions.insert(identity, primary.len());
            band.overlap_row_index = Some(band.row_index);
            band.row_index += primary_count;
            primary.push(band);
        }
    }
    Ok(())
}

/// Schedule sparse follow-up dwells using the recurring cells and their readback shifts.
/// Paired reports contribute independent witnesses from the same acquisition.
pub(super) fn read_plan(
    path: &Path,
    overlap: Option<&Path>,
    dwell_ms: u64,
    max_iq_bytes: u64,
) -> Result<Plan> {
    let mut source = input::read(path)?;
    let primary_count = source.bands.len();
    let mut narrow_band_count = source.narrow_band_count;
    let mut reported_band_count = primary_count;
    let mut overlap_sha256 = None;
    if let Some(path) = overlap {
        let supplementary = input::read(path).context("read overlap series triage report")?;
        source.validate_overlap(&supplementary)?;
        narrow_band_count += supplementary.narrow_band_count;
        reported_band_count += supplementary.bands.len();
        merge_bands(&mut source.bands, supplementary.bands)?;
        overlap_sha256 = Some(supplementary.report_sha256);
    }
    ensure!(
        reported_band_count > 0,
        "series triage report has no recurring narrow bands"
    );
    let mut centers = BTreeSet::new();
    let mut targets = Vec::with_capacity(reported_band_count);
    let mut unplaced_rows = Vec::new();
    for band in source.bands {
        // Keep both the nominal band and its actual cell shifts. A witness's
        // detector window can be broader than the cells that recurred.
        let lower_hz = band.lower_hz - band.requested_center_hz.saturating_sub(band.source_low_hz);
        let upper_hz = band.upper_hz + band.source_high_hz.saturating_sub(band.requested_center_hz);
        let target_hz = band.lower_hz.midpoint(band.upper_hz);
        let geometry = offset::Band {
            lower_hz,
            upper_hz,
            target_hz,
            source_low_hz: band.source_low_hz,
            source_high_hz: band.source_high_hz,
        };
        let Some(pair) = offset::place_pair(&geometry, &centers) else {
            unplaced_rows.push(band.row_index);
            if band.row_index <= primary_count
                && let Some(row) = band.overlap_row_index
            {
                unplaced_rows.push(primary_count + row);
            }
            continue;
        };
        centers.extend(pair);
        targets.push(Target {
            source_row: band.row_index,
            overlap_source_row: band.overlap_row_index,
            requested_center_hz: band.requested_center_hz,
            nominal_lower_hz: band.lower_hz,
            nominal_upper_hz: band.upper_hz,
            lower_hz,
            upper_hz,
            target_hz,
            source_centers_hz: [band.source_low_hz, band.source_high_hz],
            support_count: band.support_mask.count_ones() as usize,
            centers_hz: pair,
        });
    }
    ensure!(
        !targets.is_empty(),
        "no recurring narrow band fits two offset tuning centers"
    );
    let mut plan = plan::targeted(
        &centers.into_iter().collect::<Vec<_>>(),
        dwell_ms,
        max_iq_bytes,
    )?;
    plan.source_series_triage_sha256 = Some(source.report_sha256);
    plan.source_series_triage_overlap_sha256 = overlap_sha256;
    plan.source_series_sha256 = Some(source.source_series_sha256);
    unplaced_rows.sort_unstable();
    plan.series_follow_up = Some(SeriesFollowUp {
        narrow_band_count,
        reported_band_count,
        primary_reported_band_count: overlap.map(|_| primary_count),
        targets,
        unplaced_rows,
    });
    Ok(plan)
}
