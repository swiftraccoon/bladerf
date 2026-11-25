//! Require observed state transitions to support the supplied symbol clock.

use std::f64::consts::TAU;

use rustfft::num_complex::Complex;
use serde::Serialize;

pub(super) const MIN_TRANSITIONS: usize = 8;
const MIN_HALF_TRANSITIONS: usize = 3;
const MIN_ALIGNMENT: f64 = 0.75;

#[derive(Serialize)]
pub(super) struct ClockEvidence {
    transitions: usize,
    folded_phase_alignment: f64,
    early_alignment: f64,
    late_alignment: f64,
}

impl ClockEvidence {
    pub(super) fn alignment(&self) -> f64 {
        self.folded_phase_alignment
    }
}

pub(super) struct Clock {
    pub(super) evidence: ClockEvidence,
    pub(super) boundary: f64,
    pub(super) qualified: bool,
}

pub(super) fn sector_index(sample: Complex<f64>, orientation: f64, order: u32) -> u8 {
    let sector = TAU / f64::from(order);
    let slot = ((sample.arg() - orientation) / sector).round() as i32;
    slot.rem_euclid(order as i32) as u8
}

pub(super) fn transitions(samples: &[Complex<f64>], period: f64, order: u32) -> Vec<f64> {
    let width = ((period / 12.0) as usize | 1).clamp(3, 19);
    let moment: Complex<f64> = samples.iter().map(|sample| sample.powu(order)).sum();
    let orientation = moment.arg() / f64::from(order);
    let states: Vec<_> = samples
        .windows(width)
        .map(|window| sector_index(window.iter().copied().sum(), orientation, order))
        .collect();
    let mut boundaries = vec![0];
    boundaries.extend(
        states
            .windows(2)
            .enumerate()
            .filter_map(|(index, pair)| (pair[0] != pair[1]).then_some(index + 1)),
    );
    boundaries.push(states.len());
    let min_hold = (period / 5.0) as usize;
    boundaries
        .windows(3)
        .filter_map(|run| {
            // The crossing lies between the two centered smoothing windows.
            (run[1] - run[0] >= min_hold.max(3) && run[2] - run[1] >= min_hold.max(3))
                .then_some(run[1] as f64 + (width as f64 - 2.0) / 2.0)
        })
        .collect()
}

pub(super) fn fold(transitions: &[f64], midpoint: f64, period: f64) -> Clock {
    let mut halves = [Complex::<f64>::default(); 2];
    let mut counts = [0; 2];
    for &position in transitions {
        let half = usize::from(position >= midpoint);
        halves[half] += Complex::from_polar(1.0, TAU * position / period);
        counts[half] += 1;
    }
    let combined = halves[0] + halves[1];
    let alignment = (combined.norm() / transitions.len().max(1) as f64).min(1.0);
    let [early, late] =
        [0, 1].map(|half| (halves[half].norm() / counts[half].max(1) as f64).min(1.0));
    let half_agreement = (halves[0].conj() * halves[1]).re;
    let qualified = transitions.len() >= MIN_TRANSITIONS
        && counts.iter().all(|&count| count >= MIN_HALF_TRANSITIONS)
        && [alignment, early, late]
            .into_iter()
            .all(|value| value >= MIN_ALIGNMENT)
        && half_agreement >= 0.5 * halves[0].norm() * halves[1].norm();
    Clock {
        evidence: ClockEvidence {
            transitions: transitions.len(),
            folded_phase_alignment: alignment,
            early_alignment: early,
            late_alignment: late,
        },
        boundary: (combined.arg() / TAU * period + 0.5).rem_euclid(period),
        qualified,
    }
}
