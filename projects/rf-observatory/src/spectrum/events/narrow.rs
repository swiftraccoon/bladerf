//! Find bounded fixed-frequency bursts in the temporal excess grid.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use serde::Serialize;

use super::{
    ACTIVITY_THRESHOLD_DB, CELL_WIDTH_HZ, CandidateSet, CellBaseline, Event, FFT, NARROW_MAX_CELLS,
    NARROW_WINDOWS,
};

const CANDIDATE_LIMIT: usize = 128;
const MIN_CONTRAST_DB: f64 = 3.0;

#[derive(Debug, Serialize)]
pub(super) struct SearchReport {
    method: &'static str,
    candidate_limit: usize,
    candidate_count: usize,
    candidates: Vec<Event>,
}

impl SearchReport {
    pub(super) fn into_candidates(self) -> CandidateSet {
        CandidateSet {
            count: self.candidate_count,
            retained: self.candidates,
        }
    }
}

pub(super) struct Search {
    bands: Vec<Band>,
    lower_edge_hz: u64,
    window_stride: usize,
    candidates: Candidates,
}

impl Search {
    pub(super) fn new(
        baselines: &[CellBaseline],
        lower_edge_hz: u64,
        window_stride: usize,
    ) -> Self {
        let mut bands = Vec::new();
        for cells in 1..=NARROW_MAX_CELLS {
            for (left_start, span) in baselines.windows(3 * cells).enumerate() {
                if span.iter().all(|cell| cell.eligible) {
                    bands.push(Band {
                        start_cell: left_start + cells,
                        cells,
                        run: None,
                    });
                }
            }
        }
        Self {
            bands,
            lower_edge_hz,
            window_stride,
            candidates: Candidates {
                count: 0,
                ranked: BinaryHeap::with_capacity(CANDIDATE_LIMIT + 1),
            },
        }
    }

    pub(super) fn observe(&mut self, window: usize, excess_db: &[f64]) {
        for band in &mut self.bands {
            if let Some((target, contrast_db)) = band.qualifying_contrast(excess_db) {
                band.run
                    .get_or_insert(Run {
                        start_window: window,
                        peak_window: window,
                        contrast_sum_db: 0.0,
                        peak_excess_db: ACTIVITY_THRESHOLD_DB,
                        hit_tile_count: 0,
                    })
                    .add(window, target, contrast_db);
            } else if let Some(event) =
                band.finish_run(window, self.lower_edge_hz, self.window_stride)
            {
                self.candidates.add(event);
            }
        }
    }

    pub(super) fn finish(mut self, windows: usize) -> SearchReport {
        for band in &mut self.bands {
            if let Some(event) = band.finish_run(windows, self.lower_edge_hz, self.window_stride) {
                self.candidates.add(event);
            }
        }
        let mut candidates: Vec<_> = self
            .candidates
            .ranked
            .into_iter()
            .map(|rank| rank.0)
            .collect();
        candidates.sort_unstable_by_key(coordinates);
        SearchReport {
            method: "fixed_band_runs",
            candidate_limit: CANDIDATE_LIMIT,
            candidate_count: self.candidates.count,
            candidates,
        }
    }
}

struct Band {
    start_cell: usize,
    cells: usize,
    run: Option<Run>,
}

impl Band {
    fn qualifying_contrast<'a>(&self, excess_db: &'a [f64]) -> Option<(&'a [f64], f64)> {
        let start = self.start_cell;
        let end = self.start_cell + self.cells;
        let target = &excess_db[start..end];
        let target_sum = target.iter().sum::<f64>();
        let cells = self.cells as f64;
        if target_sum < cells * ACTIVITY_THRESHOLD_DB {
            return None;
        }
        let left_sum = excess_db[start - self.cells..start].iter().sum::<f64>();
        let right_sum = excess_db[end..end + self.cells].iter().sum::<f64>();
        let contrast_db = (target_sum - left_sum.max(right_sum)) / cells;
        (contrast_db >= MIN_CONTRAST_DB && target.iter().any(|&db| db >= ACTIVITY_THRESHOLD_DB))
            .then_some((target, contrast_db))
    }

    fn finish_run(
        &mut self,
        end_window: usize,
        lower_edge_hz: u64,
        stride: usize,
    ) -> Option<Event> {
        let run = self.run.take()?;
        let windows = end_window - run.start_window;
        if !NARROW_WINDOWS.contains(&windows) {
            return None;
        }
        let lower_frequency_hz = lower_edge_hz + self.start_cell as u64 * CELL_WIDTH_HZ as u64;
        Some(Event {
            start_sample: (run.start_window * stride) as u64,
            end_sample_exclusive: ((end_window - 1) * stride + FFT) as u64,
            lower_frequency_hz,
            upper_frequency_hz_exclusive: lower_frequency_hz
                + self.cells as u64 * CELL_WIDTH_HZ as u64,
            peak_excess_db: run.peak_excess_db,
            peak_window_start_sample: (run.peak_window * stride) as u64,
            hit_tile_count: run.hit_tile_count,
            narrow_contrast_db: Some(run.contrast_sum_db / windows as f64),
        })
    }
}

struct Run {
    start_window: usize,
    peak_window: usize,
    contrast_sum_db: f64,
    peak_excess_db: f64,
    hit_tile_count: usize,
}

impl Run {
    fn add(&mut self, window: usize, target: &[f64], contrast_db: f64) {
        self.contrast_sum_db += contrast_db;
        for &excess_db in target {
            if excess_db > self.peak_excess_db {
                self.peak_excess_db = excess_db;
                self.peak_window = window;
            }
            self.hit_tile_count += usize::from(excess_db >= ACTIVITY_THRESHOLD_DB);
        }
    }
}

fn coordinates(event: &Event) -> (u64, u64, u64, u64) {
    (
        event.start_sample,
        event.lower_frequency_hz,
        event.upper_frequency_hz_exclusive,
        event.end_sample_exclusive,
    )
}

struct Candidates {
    count: usize,
    ranked: BinaryHeap<RankedCandidate>,
}

impl Candidates {
    fn add(&mut self, event: Event) {
        self.count += 1;
        self.ranked.push(RankedCandidate(event));
        if self.ranked.len() > CANDIDATE_LIMIT {
            self.ranked.pop();
        }
    }
}

struct RankedCandidate(Event);

impl Ord for RankedCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = &self.0;
        let b = &other.0;
        // The heap exposes the weakest contrast, then peak, then latest coordinates.
        let contrast = |event: &Event| event.narrow_contrast_db.unwrap_or(f64::NEG_INFINITY);
        contrast(b)
            .total_cmp(&contrast(a))
            .then_with(|| b.peak_excess_db.total_cmp(&a.peak_excess_db))
            .then_with(|| coordinates(a).cmp(&coordinates(b)))
    }
}

impl PartialOrd for RankedCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankedCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedCandidate {}
