//! Frequency-plateau measurements from baseband phase increments.

use std::f64::consts::TAU;

use rustfft::num_complex::Complex;
use serde::Serialize;

use super::{BASEBAND_RATE, MAX_CARRIER_OFFSET_HZ};

const SMOOTHING_SAMPLES: usize = 5;
const MIN_PLATEAU_SAMPLES: usize = 10;
const MIN_SEPARATION_HZ: f64 = 40_000.0;
const LEVEL_TOLERANCE: f64 = 0.17;
const MIN_LEVEL_FRACTION: f64 = 0.30;
const MAX_GAP_FRACTION: f64 = 0.15;
const MIN_ENVELOPE_POWER_RATIO: f64 = 0.25;
const MIN_PLATEAU_FRACTION: f64 = 0.25;
const MIN_PLATEAU_SWITCHES: usize = 6;

#[derive(Serialize)]
pub(super) struct FrequencyPlateaus {
    level_offsets_hz: [f64; 2],
    level_fractions: [f64; 2],
    gap_fraction: f64,
    envelope_p20_p80_power_ratio: f64,
    plateau_window_fraction: f64,
    plateau_switches: usize,
    binary_fsk_hint: bool,
}

impl FrequencyPlateaus {
    pub(super) fn qualified_levels(&self) -> Option<[f64; 2]> {
        self.binary_fsk_hint.then_some(self.level_offsets_hz)
    }
}

/// Compare ordered phase increments with two sustained frequency levels.
pub(super) fn measure(samples: &[Complex<f64>]) -> Option<FrequencyPlateaus> {
    let phase_scale = f64::from(BASEBAND_RATE) / TAU;
    let frequencies: Vec<_> = samples
        .windows(2)
        .map(|pair| (pair[0].conj() * pair[1]).arg() * phase_scale)
        .collect();
    let smoothed: Vec<_> = frequencies
        .windows(SMOOTHING_SAMPLES)
        .map(|window| window.iter().sum::<f64>() / SMOOTHING_SAMPLES as f64)
        .collect();
    let mut sorted = smoothed.clone();
    sorted.sort_unstable_by(f64::total_cmp);
    let last = sorted.len().checked_sub(1)?;
    let low = sorted[last / 5];
    let high = sorted[last * 4 / 5];
    let separation = high - low;
    if separation <= f64::EPSILON {
        return None;
    }
    let mut power: Vec<_> = samples.iter().map(|sample| sample.norm_sqr()).collect();
    power.sort_unstable_by(f64::total_cmp);
    let last_power = power.len() - 1;
    let envelope_ratio = power[last_power / 5] / power[last_power * 4 / 5].max(f64::MIN_POSITIVE);

    let tolerance = separation * LEVEL_TOLERANCE;
    let gap = (low + 0.30 * separation)..=(high - 0.30 * separation);
    let mut level_counts = [0, 0];
    let mut gap_count = 0;
    let mut run_level = None;
    let mut run_len = 0;
    let mut previous_plateau = None;
    let mut plateau_windows = 0;
    let mut plateau_switches = 0;
    for &frequency in &smoothed {
        gap_count += usize::from(gap.contains(&frequency));
        let level = [low, high]
            .iter()
            .position(|offset| (frequency - offset).abs() <= tolerance);
        if level != run_level {
            run_level = level;
            run_len = 0;
        }
        if let Some(level) = level {
            level_counts[level] += 1;
            run_len += 1;
            if run_len >= MIN_PLATEAU_SAMPLES {
                plateau_windows += 1;
                plateau_switches += usize::from(previous_plateau.is_some_and(|last| last != level));
                previous_plateau = Some(level);
            }
        }
    }

    let level_fractions = level_counts.map(|count| count as f64 / smoothed.len() as f64);
    let gap_fraction = gap_count as f64 / smoothed.len() as f64;
    let possible_windows = smoothed.len().checked_sub(MIN_PLATEAU_SAMPLES - 1)?;
    let plateau_window_fraction = plateau_windows as f64 / possible_windows.max(1) as f64;
    let binary_fsk_hint = separation >= MIN_SEPARATION_HZ
        && low >= -MAX_CARRIER_OFFSET_HZ
        && high <= MAX_CARRIER_OFFSET_HZ
        && level_fractions
            .iter()
            .all(|&value| value >= MIN_LEVEL_FRACTION)
        && gap_fraction <= MAX_GAP_FRACTION
        && envelope_ratio >= MIN_ENVELOPE_POWER_RATIO
        && plateau_window_fraction >= MIN_PLATEAU_FRACTION
        && plateau_switches >= MIN_PLATEAU_SWITCHES;
    Some(FrequencyPlateaus {
        level_offsets_hz: [low, high],
        level_fractions,
        gap_fraction,
        envelope_p20_p80_power_ratio: envelope_ratio,
        plateau_window_fraction,
        plateau_switches,
        binary_fsk_hint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_keyed_frequency_plateaus_and_rejects_swept_frequency() {
        let samples = |frequency: fn(u32) -> f64| {
            let mut phase = 0.0;
            (0..4096)
                .map(|index| {
                    phase += TAU * frequency(index) / f64::from(BASEBAND_RATE);
                    Complex::from_polar(0.25, phase)
                })
                .collect::<Vec<_>>()
        };
        let keyed = samples(|index| {
            if index / 40 % 2 == 0 {
                -17_000.0
            } else {
                93_000.0
            }
        });
        let swept = samples(|index| 38_000.0 + 55_000.0 * (TAU * f64::from(index) / 160.0).sin());
        let keyed = measure(&keyed).unwrap();
        let swept = measure(&swept).unwrap();
        assert!(keyed.binary_fsk_hint);
        assert!(keyed.gap_fraction < 0.10);
        assert!(!swept.binary_fsk_hint);
        assert!(swept.gap_fraction > MAX_GAP_FRACTION);
    }

    #[test]
    fn a_gated_tone_does_not_supply_a_second_frequency() {
        let samples = (0..4096u32)
            .map(|index| {
                let gate = f64::from(index / 50 % 2);
                let phase = TAU * f64::from(index) * 80_000.0 / f64::from(BASEBAND_RATE);
                Complex::from_polar(gate, phase)
            })
            .collect::<Vec<_>>();
        let result = measure(&samples).unwrap();
        assert_eq!(result.envelope_p20_p80_power_ratio, 0.0);
        assert!(!result.binary_fsk_hint);
    }
}
