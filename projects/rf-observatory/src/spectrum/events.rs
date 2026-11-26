//! Connected transient cells in the dense survey's saved-sample coordinates.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    ACTIVITY_THRESHOLD_DB, CELL_WIDTH_HZ, CENTER_GUARD_HZ, CandidateSet, EventBand,
    EventCandidates, EventStride, FFT, median, power_db,
};

mod narrow;

pub(super) const MAX_EVENTS: usize = 4096;
const NARROW_MAX_CELLS: usize = 4;
const NARROW_WINDOWS: Range<usize> = 2..11;

#[derive(Debug, Serialize)]
pub(super) struct Detection {
    windows: usize,
    eligible_cells: usize,
    evaluated_tile_count: usize,
    hit_tile_count: usize,
    active_window_count: usize,
    /// Maximal half-open runs in FFT window indices.
    #[serde(skip_serializing_if = "Option::is_none")]
    active_window_ranges: Option<Vec<[usize; 2]>>,
    component_count: usize,
    omitted_component_count: usize,
    omitted_hit_tile_count: usize,
    events: Vec<Event>,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_search: Option<narrow::SearchReport>,
}

impl Detection {
    pub(super) fn into_candidates(self) -> EventCandidates {
        EventCandidates {
            components: CandidateSet {
                count: self.component_count,
                retained: self.events,
            },
            narrow: self
                .narrow_search
                .map(narrow::SearchReport::into_candidates),
        }
    }
}

/// Half-open bounds can enclose tiles below the detection threshold.
#[derive(Debug, Serialize)]
pub(crate) struct Event {
    pub(crate) start_sample: u64,
    pub(crate) end_sample_exclusive: u64,
    pub(crate) lower_frequency_hz: u64,
    pub(crate) upper_frequency_hz_exclusive: u64,
    pub(crate) peak_excess_db: f64,
    /// First sample of the FFT window containing the strongest hit.
    #[serde(skip)]
    pub(crate) peak_window_start_sample: u64,
    hit_tile_count: usize,
    /// Mean excess over the stronger adjacent band when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) narrow_contrast_db: Option<f64>,
}

struct RankedEvent {
    seed: usize,
    event: Event,
}

impl Ord for RankedEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // The heap exposes the weakest event, or the latest seed on equal peaks.
        other
            .event
            .peak_excess_db
            .total_cmp(&self.event.peak_excess_db)
            .then_with(|| self.seed.cmp(&other.seed))
    }
}

impl PartialOrd for RankedEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankedEvent {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedEvent {}

fn enqueue(hits: &mut [bool], pending: &mut VecDeque<usize>, index: usize) {
    if std::mem::take(&mut hits[index]) {
        pending.push_back(index);
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Relaxed), "spectrum event detection interrupted");
    Ok(())
}

struct CellBaseline {
    eligible: bool,
    median_db: f64,
}

pub(super) fn locate(
    cell_window_powers: &[Vec<f64>],
    band: EventBand,
    center_hz: u64,
    stride: EventStride,
    include_active_window_ranges: bool,
    stop: &AtomicBool,
) -> Result<Detection> {
    locate_limited(
        cell_window_powers,
        band,
        center_hz,
        stride,
        include_active_window_ranges,
        MAX_EVENTS,
        stop,
    )
}

fn locate_limited(
    cell_window_powers: &[Vec<f64>],
    band: EventBand,
    center_hz: u64,
    stride: EventStride,
    include_active_window_ranges: bool,
    limit: usize,
    stop: &AtomicBool,
) -> Result<Detection> {
    let cell_count = band.cell_count();
    let half_span_hz = band.half_span_hz();
    let windows = cell_window_powers.first().map_or(0, Vec::len);
    let window_stride = stride.samples();
    let max_windows = (crate::MAX_SAMPLES as usize - FFT) / window_stride + 1;
    ensure!(
        (1..=max_windows).contains(&windows)
            && cell_window_powers.len() == cell_count
            && cell_window_powers
                .iter()
                .all(|powers| powers.len() == windows),
        "invalid dense spectrum event grid"
    );
    let lower_edge = center_hz
        .checked_sub(half_span_hz)
        .context("event frequency range is below zero")?;
    center_hz
        .checked_add(half_span_hz)
        .context("event frequency range exceeds u64")?;

    let mut baselines = Vec::with_capacity(cell_count);
    let mut eligible_cells = 0;
    for (index, powers) in cell_window_powers.iter().enumerate() {
        check_stop(stop)?;
        let start_offset_hz = index as f64 * CELL_WIDTH_HZ - half_span_hz as f64;
        let end_offset_hz = start_offset_hz + CELL_WIDTH_HZ;
        let eligible = end_offset_hz <= -CENTER_GUARD_HZ || start_offset_hz >= CENTER_GUARD_HZ;
        baselines.push(CellBaseline {
            eligible,
            median_db: power_db(median(powers)),
        });
        eligible_cells += usize::from(eligible);
    }
    let mut hits = Vec::with_capacity(windows * cell_count);
    let mut hit_tile_count = 0;
    let mut active_window_count = 0;
    let mut active_window_ranges = include_active_window_ranges.then(Vec::<[usize; 2]>::new);
    let mut narrow_search = include_active_window_ranges
        .then(|| narrow::Search::new(&baselines, lower_edge, window_stride));
    let mut excess_db = [0.0; EventBand::Full.cell_count()];
    for window in 0..windows {
        check_stop(stop)?;
        let mut window_hits = 0;
        for ((cell, powers), excess) in baselines.iter().zip(cell_window_powers).zip(&mut excess_db)
        {
            *excess = if cell.eligible {
                power_db(powers[window]) - cell.median_db
            } else {
                f64::NEG_INFINITY
            };
            let hit = *excess >= ACTIVITY_THRESHOLD_DB;
            hits.push(hit);
            window_hits += usize::from(hit);
        }
        if let Some(search) = &mut narrow_search {
            search.observe(window, &excess_db);
        }
        hit_tile_count += window_hits;
        active_window_count += usize::from(window_hits > 0);
        if let Some(ranges) = &mut active_window_ranges
            && window_hits > 0
        {
            if let Some([_, end]) = ranges.last_mut()
                && *end == window
            {
                *end = window + 1;
            } else {
                ranges.push([window, window + 1]);
            }
        }
    }

    let mut retained = BinaryHeap::with_capacity(limit + 1);
    let mut pending = VecDeque::new();
    let mut component_count = 0;
    for seed in 0..hits.len() {
        if seed.is_multiple_of(cell_count) {
            check_stop(stop)?;
        }
        enqueue(&mut hits, &mut pending, seed);
        if pending.is_empty() {
            continue;
        }
        let event = component(
            &mut hits,
            &mut pending,
            cell_window_powers,
            &baselines,
            lower_edge,
            window_stride,
            stop,
        )?;
        component_count += 1;
        retained.push(RankedEvent { seed, event });
        if retained.len() > limit {
            retained.pop();
        }
    }

    let mut events = retained.into_vec();
    events.sort_unstable_by_key(|ranked| ranked.seed);
    if include_active_window_ranges {
        for ranked in &mut events {
            check_stop(stop)?;
            ranked.event.narrow_contrast_db = narrow_contrast(
                &ranked.event,
                cell_window_powers,
                &baselines,
                lower_edge,
                window_stride,
            );
        }
    }
    let omitted_component_count = component_count - events.len();
    let retained_hit_tile_count: usize = events
        .iter()
        .map(|ranked| ranked.event.hit_tile_count)
        .sum();
    Ok(Detection {
        windows,
        eligible_cells,
        evaluated_tile_count: eligible_cells * windows,
        hit_tile_count,
        active_window_count,
        active_window_ranges,
        component_count,
        omitted_component_count,
        omitted_hit_tile_count: hit_tile_count - retained_hit_tile_count,
        events: events.into_iter().map(|ranked| ranked.event).collect(),
        narrow_search: narrow_search.map(|search| search.finish(windows)),
    })
}

fn component(
    hits: &mut [bool],
    pending: &mut VecDeque<usize>,
    cell_window_powers: &[Vec<f64>],
    baselines: &[CellBaseline],
    lower_edge: u64,
    window_stride: usize,
    stop: &AtomicBool,
) -> Result<Event> {
    let cell_count = baselines.len();
    let mut first_window = usize::MAX;
    let mut last_window = 0;
    let mut first_cell = cell_count;
    let mut last_cell = 0;
    let mut peak_excess_db = f64::NEG_INFINITY;
    let mut peak_window = 0;
    let mut hit_tile_count: usize = 0;
    while let Some(index) = pending.pop_front() {
        if hit_tile_count.is_multiple_of(1024) {
            check_stop(stop)?;
        }
        let window = index / cell_count;
        let cell = index % cell_count;
        first_window = first_window.min(window);
        last_window = last_window.max(window);
        first_cell = first_cell.min(cell);
        last_cell = last_cell.max(cell);
        let excess_db = power_db(cell_window_powers[cell][window]) - baselines[cell].median_db;
        if excess_db > peak_excess_db || (excess_db == peak_excess_db && window < peak_window) {
            peak_excess_db = excess_db;
            peak_window = window;
        }
        hit_tile_count += 1;

        if cell > 0 {
            enqueue(hits, pending, index - 1);
        }
        if cell + 1 < cell_count {
            enqueue(hits, pending, index + 1);
        }
        if index >= cell_count {
            enqueue(hits, pending, index - cell_count);
        }
        if index + cell_count < hits.len() {
            enqueue(hits, pending, index + cell_count);
        }
    }

    Ok(Event {
        start_sample: (first_window * window_stride) as u64,
        end_sample_exclusive: (last_window * window_stride + FFT) as u64,
        lower_frequency_hz: lower_edge + first_cell as u64 * CELL_WIDTH_HZ as u64,
        upper_frequency_hz_exclusive: lower_edge + (last_cell + 1) as u64 * CELL_WIDTH_HZ as u64,
        peak_excess_db,
        peak_window_start_sample: (peak_window * window_stride) as u64,
        hit_tile_count,
        narrow_contrast_db: None,
    })
}

fn narrow_contrast(
    event: &Event,
    cell_window_powers: &[Vec<f64>],
    baselines: &[CellBaseline],
    lower_edge: u64,
    window_stride: usize,
) -> Option<f64> {
    let cell_width_hz = CELL_WIDTH_HZ as u64;
    let first_cell = ((event.lower_frequency_hz - lower_edge) / cell_width_hz) as usize;
    let end_cell = ((event.upper_frequency_hz_exclusive - lower_edge) / cell_width_hz) as usize;
    let cells = end_cell - first_cell;
    let first_window = event.start_sample as usize / window_stride;
    let end_window = (event.end_sample_exclusive as usize - FFT) / window_stride + 1;
    let windows = end_window - first_window;
    if cells > NARROW_MAX_CELLS || !NARROW_WINDOWS.contains(&windows) {
        return None;
    }

    let left_start = first_cell.checked_sub(cells)?;
    let right_end = end_cell + cells;
    if !baselines
        .get(left_start..right_end)?
        .iter()
        .all(|cell| cell.eligible)
    {
        return None;
    }
    let mean_excess = |indices: Range<usize>, window| {
        indices
            .map(|cell| power_db(cell_window_powers[cell][window]) - baselines[cell].median_db)
            .sum::<f64>()
            / cells as f64
    };
    let contrast = (first_window..end_window)
        .map(|window| {
            let target = mean_excess(first_cell..end_cell, window);
            let left = mean_excess(left_start..first_cell, window);
            let right = mean_excess(end_cell..right_end, window);
            target - left.max(right)
        })
        .sum::<f64>()
        / windows as f64;
    Some(contrast)
}

#[cfg(test)]
mod tests;
