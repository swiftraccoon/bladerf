//! Combine per-dwell evidence on a fixed grid for one requested center.

use std::cmp::Ordering;
use std::ops::Range;

use serde::Serialize;

use crate::spectrum::{CELL_WIDTH_HZ, CENTER_GUARD_HZ, Event, EventBand};
use crate::triage::MAX_NARROW_WIDTH_HZ;

const HALF_SPAN_HZ: u64 = EventBand::Inner.half_span_hz();
const CELL_HZ: u64 = CELL_WIDTH_HZ as u64;
const CELL_COUNT: usize = EventBand::Inner.cell_count();
const GUARD_HZ: u64 = CENTER_GUARD_HZ as u64;

/// A nominal band and its supporting dwell evidence.
#[derive(Debug, Serialize)]
pub(super) struct Band {
    requested_center_hz: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tune_directory: Option<String>,
    lower_hz: u64,
    upper_hz: u64,
    passes: Vec<PassVote>,
}

impl Band {
    /// Sort by greater pass support and stronger weakest supporting score.
    pub(super) fn compare(a: &Self, b: &Self) -> Ordering {
        fn weakest(band: &Band) -> f64 {
            band.passes
                .iter()
                .map(|vote| vote.score_db)
                .min_by(f64::total_cmp)
                .expect("reported band has supporting passes")
        }

        b.passes
            .len()
            .cmp(&a.passes.len())
            .then_with(|| weakest(b).total_cmp(&weakest(a)))
            .then_with(|| {
                (a.requested_center_hz, a.lower_hz, a.upper_hz).cmp(&(
                    b.requested_center_hz,
                    b.lower_hz,
                    b.upper_hz,
                ))
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
struct PassVote {
    pass_index: usize,
    score_db: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_window: Option<NarrowWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
struct NarrowWindow {
    lower_hz: u64,
    upper_hz: u64,
    start_sample: u64,
    end_sample_exclusive: u64,
}

impl NarrowWindow {
    fn aliases(self, other: Self) -> bool {
        self.lower_hz
            .midpoint(self.upper_hz)
            .abs_diff(other.lower_hz.midpoint(other.upper_hz))
            <= CELL_HZ
            && shares_three_quarters(self.lower_hz..self.upper_hz, other.lower_hz..other.upper_hz)
            && shares_three_quarters(
                self.start_sample..self.end_sample_exclusive,
                other.start_sample..other.end_sample_exclusive,
            )
    }
}

fn shares_three_quarters(a: Range<u64>, b: Range<u64>) -> bool {
    let overlap = a.end.min(b.end).saturating_sub(a.start.max(b.start));
    let shorter = (a.end - a.start).min(b.end - b.start);
    overlap * 4 >= shorter * 3
}

/// Record no more than one representative per pass per nominal frequency cell.
pub(super) struct Votes {
    requested_center_hz: u64,
    pass_count: usize,
    requested_range: Option<Range<u64>>,
    power: Vec<Vec<PassVote>>,
    narrow: Vec<Vec<PassVote>>,
}

impl Votes {
    pub(super) fn new(
        requested_center_hz: u64,
        pass_count: usize,
        requested_range: Option<&Range<u64>>,
    ) -> Self {
        Self {
            requested_center_hz,
            pass_count,
            requested_range: requested_range.cloned(),
            power: vec![Vec::new(); CELL_COUNT],
            narrow: vec![Vec::new(); CELL_COUNT],
        }
    }

    /// Record mean-power triggers for one zero-based pass index.
    pub(super) fn add_power(
        &mut self,
        pass: usize,
        observed_center_hz: u64,
        cells: impl Iterator<Item = (usize, f64)>,
    ) {
        assert!(pass < self.pass_count);
        for (index, score_db) in cells {
            if score_db.is_finite() && self.accepts_cell(observed_center_hz, index) {
                record(
                    &mut self.power[index],
                    PassVote {
                        pass_index: pass + 1,
                        score_db,
                        narrow_window: None,
                    },
                );
            }
        }
    }

    /// Record the strongest narrow representative per cell for one zero-based pass index.
    pub(super) fn add_narrow(
        &mut self,
        pass: usize,
        observed_center_hz: u64,
        candidates: &[Event],
    ) {
        assert!(pass < self.pass_count);
        let mut ranked: Vec<_> = candidates
            .iter()
            .filter_map(|event| {
                event
                    .narrow_contrast_db
                    .filter(|score| score.is_finite())
                    .map(|score| (event, score))
            })
            .collect();
        ranked.sort_by(|(a, a_score), (b, b_score)| {
            b_score
                .total_cmp(a_score)
                .then_with(|| b.peak_excess_db.total_cmp(&a.peak_excess_db))
                .then_with(|| event_coordinates(a).cmp(&event_coordinates(b)))
        });
        let mut accepted: Vec<NarrowWindow> = Vec::new();
        for (event, score_db) in ranked {
            let Some(cells) = event_cells(observed_center_hz, event) else {
                continue;
            };
            let window = NarrowWindow {
                lower_hz: event.lower_frequency_hz,
                upper_hz: event.upper_frequency_hz_exclusive,
                start_sample: event.start_sample,
                end_sample_exclusive: event.end_sample_exclusive,
            };
            if accepted.iter().any(|other| window.aliases(*other)) {
                continue;
            }
            let mut recorded = false;
            for index in cells {
                if self.accepts_cell(observed_center_hz, index) {
                    record(
                        &mut self.narrow[index],
                        PassVote {
                            pass_index: pass + 1,
                            score_db,
                            narrow_window: Some(window),
                        },
                    );
                    recorded = true;
                }
            }
            if recorded {
                accepted.push(window);
            }
        }
    }

    fn accepts_cell(&self, observed_center_hz: u64, index: usize) -> bool {
        if index >= CELL_COUNT {
            return false;
        }
        let nominal = cell_frequency(self.requested_center_hz, index);
        (nominal.end <= self.requested_center_hz - GUARD_HZ
            || self.requested_center_hz + GUARD_HZ <= nominal.start)
            && self.requested_range.as_ref().is_none_or(|request| {
                intersects(&nominal, request)
                    && intersects(&cell_frequency(observed_center_hz, index), request)
            })
    }

    /// Return the mean-power bands followed by narrow contrast bands.
    pub(super) fn finish(
        self,
        min_passes: usize,
        tune_directory: Option<&str>,
    ) -> (Vec<Band>, Vec<Band>) {
        (
            bands(
                self.requested_center_hz,
                tune_directory,
                self.power,
                min_passes,
                true,
            ),
            bands(
                self.requested_center_hz,
                tune_directory,
                self.narrow,
                min_passes,
                false,
            ),
        )
    }
}

fn event_coordinates(event: &Event) -> (u64, u64, u64, u64) {
    (
        event.lower_frequency_hz,
        event.upper_frequency_hz_exclusive,
        event.start_sample,
        event.end_sample_exclusive,
    )
}

fn event_cells(observed_center_hz: u64, event: &Event) -> Option<Range<usize>> {
    let origin = observed_center_hz.checked_sub(HALF_SPAN_HZ)?;
    let start = event.lower_frequency_hz.checked_sub(origin)?;
    let end = event.upper_frequency_hz_exclusive.checked_sub(origin)?;
    (start < end
        && end <= 2 * HALF_SPAN_HZ
        && end - start <= MAX_NARROW_WIDTH_HZ
        && start.is_multiple_of(CELL_HZ)
        && end.is_multiple_of(CELL_HZ)
        && event.start_sample < event.end_sample_exclusive)
        .then_some((start / CELL_HZ) as usize..(end / CELL_HZ) as usize)
}

fn cell_frequency(center_hz: u64, index: usize) -> Range<u64> {
    let start = center_hz - HALF_SPAN_HZ + index as u64 * CELL_HZ;
    start..start + CELL_HZ
}

fn intersects(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

fn record(votes: &mut Vec<PassVote>, vote: PassVote) {
    match votes.binary_search_by_key(&vote.pass_index, |candidate| candidate.pass_index) {
        Ok(index) if vote.score_db > votes[index].score_db => votes[index] = vote,
        Ok(_) => (),
        Err(index) => votes.insert(index, vote),
    }
}

fn bands(
    requested_center_hz: u64,
    tune_directory: Option<&str>,
    cells: Vec<Vec<PassVote>>,
    min_passes: usize,
    power: bool,
) -> Vec<Band> {
    let mut bands: Vec<Band> = Vec::new();
    for (index, votes) in cells.into_iter().enumerate() {
        if votes.len() < min_passes || votes.is_empty() {
            continue;
        }
        let frequency = cell_frequency(requested_center_hz, index);
        if let Some(previous) = bands.last_mut()
            && previous.upper_hz == frequency.start
            && previous.passes.len() == votes.len()
            && previous.passes.iter().zip(&votes).all(|(a, b)| {
                if power {
                    a.pass_index == b.pass_index
                } else {
                    a == b
                }
            })
        {
            previous.upper_hz = frequency.end;
            for (a, b) in previous.passes.iter_mut().zip(votes) {
                a.score_db = a.score_db.min(b.score_db);
            }
        } else {
            bands.push(Band {
                requested_center_hz,
                tune_directory: tune_directory.map(str::to_owned),
                lower_hz: frequency.start,
                upper_hz: frequency.end,
                passes: votes,
            });
        }
    }
    bands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_power_votes_use_both_grids_at_requested_boundaries() {
        let center = 100_000_000;
        let cell = CELL_COUNT / 2 + 1;
        let boundary = cell_frequency(center, cell).start;
        let request = boundary..boundary + 5;
        let mut votes = Votes::new(center, 3, Some(&request));
        votes.add_power(0, center + 7, [(cell, 30.0)].into_iter());
        votes.add_power(1, center - 7, [(cell, 9.0), (cell, 12.0)].into_iter());
        votes.add_power(2, center, [(cell, 8.0)].into_iter());

        let (power, narrow) = votes.finish(2, None);
        assert!(narrow.is_empty());
        assert_eq!(power.len(), 1);
        assert_eq!(power[0].lower_hz, boundary);
        assert_eq!(
            power[0]
                .passes
                .iter()
                .map(|vote| (vote.pass_index, vote.score_db))
                .collect::<Vec<_>>(),
            [(2, 12.0), (3, 8.0)]
        );
    }
}
