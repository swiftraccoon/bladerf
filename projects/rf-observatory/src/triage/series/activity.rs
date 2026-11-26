//! Summarize per-dwell activity on the saved sample axis.

use std::cmp::Ordering;
use std::ops::Range;

use serde::Serialize;

use crate::spectrum::{ActivityDetection, CELL_WIDTH_HZ, CandidateSet, Event, EventBand};
use crate::triage::sampling::IntervalEvidence;

const CELL_HZ: u64 = CELL_WIDTH_HZ as u64;

/// Count detector results and rank bounded evidence from the inspected intervals.
#[derive(Serialize)]
pub(super) struct Dwell {
    #[serde(skip)]
    limit: usize,
    /// Detector count before its cap or requested-band filtering.
    transient_event_count: usize,
    transient_detector_omitted_count: usize,
    /// Count after the detector cap and requested-band filtering, before ranking.
    transient_eligible_retained_count: usize,
    /// Count of intersecting regions before ranking.
    mean_power_region_count: usize,
    transients: Vec<Event>,
    mean_power_regions: Vec<MeanPowerRegion>,
}

#[derive(Serialize)]
struct MeanPowerRegion {
    start_sample: u64,
    end_sample_exclusive: u64,
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    peak_excess_db: f64,
    cell_count: usize,
}

impl Dwell {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            transient_event_count: 0,
            transient_detector_omitted_count: 0,
            transient_eligible_retained_count: 0,
            mean_power_region_count: 0,
            transients: Vec::new(),
            mean_power_regions: Vec::new(),
        }
    }

    /// Add one analyzed interval from the inner grid at the validated center.
    /// Component offsets must already use the saved dwell's sample coordinates.
    pub(super) fn observe(
        &mut self,
        center_hz: u64,
        requested: Option<&Range<u64>>,
        interval: &IntervalEvidence,
        power: &ActivityDetection,
        components: CandidateSet,
    ) {
        self.transient_event_count += components.count;
        self.transient_detector_omitted_count += components.count - components.retained.len();
        for event in components.retained {
            if intersects(
                requested,
                event.lower_frequency_hz,
                event.upper_frequency_hz_exclusive,
            ) {
                self.transient_eligible_retained_count += 1;
                retain_best(&mut self.transients, event, self.limit, compare_events);
            }
        }

        let grid_start_hz = center_hz - EventBand::Inner.half_span_hz();
        let mut current: Option<MeanPowerRegion> = None;
        for (index, excess_db) in power.mean_activity_cells() {
            let lower_hz = grid_start_hz + index as u64 * CELL_HZ;
            if let Some(region) = &mut current
                && region.upper_frequency_hz_exclusive == lower_hz
            {
                region.upper_frequency_hz_exclusive += CELL_HZ;
                region.peak_excess_db = region.peak_excess_db.max(excess_db);
                region.cell_count += 1;
                continue;
            }
            if let Some(region) = current.take() {
                self.observe_region(requested, region);
            }
            current = Some(MeanPowerRegion {
                start_sample: interval.start_sample,
                end_sample_exclusive: interval.end_sample_exclusive,
                lower_frequency_hz: lower_hz,
                upper_frequency_hz_exclusive: lower_hz + CELL_HZ,
                peak_excess_db: excess_db,
                cell_count: 1,
            });
        }
        if let Some(region) = current {
            self.observe_region(requested, region);
        }
    }

    fn observe_region(&mut self, requested: Option<&Range<u64>>, region: MeanPowerRegion) {
        if intersects(
            requested,
            region.lower_frequency_hz,
            region.upper_frequency_hz_exclusive,
        ) {
            self.mean_power_region_count += 1;
            retain_best(
                &mut self.mean_power_regions,
                region,
                self.limit,
                compare_regions,
            );
        }
    }
}

fn intersects(requested: Option<&Range<u64>>, lower_hz: u64, upper_hz: u64) -> bool {
    requested.is_none_or(|range| lower_hz < range.end && range.start < upper_hz)
}

fn retain_best<T>(rows: &mut Vec<T>, row: T, limit: usize, compare: impl Fn(&T, &T) -> Ordering) {
    let index = rows.partition_point(|existing| !compare(existing, &row).is_gt());
    if index < limit {
        if rows.len() == limit {
            rows.pop();
        }
        rows.insert(index, row);
    }
}

fn compare_events(a: &Event, b: &Event) -> Ordering {
    let geometry = |event: &Event| {
        (
            event.start_sample,
            event.end_sample_exclusive,
            event.lower_frequency_hz,
            event.upper_frequency_hz_exclusive,
        )
    };
    b.peak_excess_db
        .total_cmp(&a.peak_excess_db)
        .then_with(|| geometry(a).cmp(&geometry(b)))
}

fn compare_regions(a: &MeanPowerRegion, b: &MeanPowerRegion) -> Ordering {
    let geometry = |region: &MeanPowerRegion| {
        (
            region.start_sample,
            region.end_sample_exclusive,
            region.lower_frequency_hz,
            region.upper_frequency_hz_exclusive,
        )
    };
    b.peak_excess_db
        .total_cmp(&a.peak_excess_db)
        .then_with(|| geometry(a).cmp(&geometry(b)))
}
