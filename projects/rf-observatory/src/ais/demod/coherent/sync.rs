//! Refine a detected AIS opening using its known complex GMSK waveform.

use std::f64::consts::{PI, TAU};

use super::super::{FLAG, SYMBOL_RATE};
use super::{OPENING_SYMBOLS, TRAINING_SYMBOLS, multiply, phasor};
use crate::dsp::Iq;

const MIN_CORRELATION: f64 = 0.8;

/// The timing and carrier estimate accepted by the preamble check.
pub(super) struct Match {
    /// First training boundary on the filtered IQ sample grid.
    pub(super) training_start: u64,
    /// Signed residual carrier offset relative to the selected channel.
    pub(super) offset_hz: f64,
    /// Carrier phase in radians at `training_start`, relative to the matched CPM waveform.
    pub(super) phase: f64,
}

/// Bounded timing and carrier refinement after the discriminator sees an opening flag.
pub(super) struct Synchronizer {
    rate_hz: u32,
    opening_age: u64,
    search_radius: i64,
    stride: usize,
    template_start: usize,
    templates: [Vec<Iq>; 4],
}

impl Synchronizer {
    pub(super) fn new(rate_hz: u32) -> Self {
        debug_assert!(matches!(rate_hz, 50_000 | 96_000));
        let samples_per_symbol = f64::from(rate_hz) / f64::from(SYMBOL_RATE);
        let average_samples = (0.9 * samples_per_symbol).round() as u64;
        let opening_age = ((OPENING_SYMBOLS as f64 - 0.5) * samples_per_symbol).round() as u64
            + average_samples / 2;
        let template_start = (4.0 * samples_per_symbol).round() as usize;
        let template_end = (30.0 * samples_per_symbol).round() as usize;
        let stride = (0.4 * samples_per_symbol).round() as usize;
        let positive =
            |first_bit| template(rate_hz, template_start, template_end, stride, first_bit);
        let zero = positive(false);
        let one = positive(true);
        let inverted = |samples: &[Iq]| {
            samples
                .iter()
                .map(|sample| Iq {
                    i: sample.i,
                    q: -sample.q,
                })
                .collect()
        };
        let zero_inverted = inverted(&zero);
        let one_inverted = inverted(&one);
        Self {
            rate_hz,
            opening_age,
            search_radius: (0.7 * samples_per_symbol).round() as i64,
            stride,
            template_start,
            templates: [zero, zero_inverted, one, one_inverted],
        }
    }

    /// Find the best nearby timing on the same grid as the opening detector.
    ///
    /// Reads past IQ by absolute sample index, including the filter delay.
    /// Returns `None` if history is incomplete or no match clears the normalized
    /// correlation threshold. Carrier slope is unambiguous within about ±1,200 Hz.
    pub(super) fn fit(
        &self,
        opening_sample: u64,
        mut sample: impl FnMut(u64) -> Option<Iq>,
    ) -> Option<Match> {
        let samples_per_symbol = f64::from(self.rate_hz) / f64::from(SYMBOL_RATE);
        let lag = 4.0 * samples_per_symbol;
        let lag_whole = lag.floor() as u64;
        let lag_fraction = lag.fract();
        let start = (11.0 * samples_per_symbol).round() as u64;
        let end = (22.5 * samples_per_symbol).round() as u64;
        let mut period = Iq::default();
        // The NRZI training waveform repeats every four symbols.
        for age in start..end {
            let index = opening_sample.checked_sub(self.opening_age - age)?;
            let current = sample(index)?;
            let previous_index = index.checked_sub(lag_whole)?;
            let mut previous = sample(previous_index)?;
            if lag_fraction != 0.0 {
                let older = sample(previous_index.checked_sub(1)?)?;
                previous.i += lag_fraction * (older.i - previous.i);
                previous.q += lag_fraction * (older.q - previous.q);
            }
            let observed = multiply_conjugate(current, previous);
            period.i += observed.i;
            period.q += observed.q;
        }
        let slope = period.q.atan2(period.i) / lag;
        let mut best_squared = MIN_CORRELATION * MIN_CORRELATION;
        let mut best_match = None;
        // Sample the middle of training through six flag symbols. These samples
        // are already in history even at the latest searched boundary.
        let step = phasor(slope * self.stride as f64);
        let first_rotation = phasor(slope * self.template_start as f64);
        for correction in -self.search_radius..=self.search_radius {
            let Some(training_start) =
                opening_sample.checked_add_signed(correction - self.opening_age as i64)
            else {
                continue;
            };
            let mut rotation = first_rotation;
            let mut energy = 0.0;
            let mut correlations = [Iq::default(); 4];
            for position in 0..self.templates[0].len() {
                let offset = self.template_start + position * self.stride;
                let received = sample(training_start.checked_add(offset as u64)?)?;
                energy += received.power();
                let aligned = multiply_conjugate(received, rotation);
                rotation = multiply(rotation, step);
                for (correlation, template) in correlations.iter_mut().zip(&self.templates) {
                    let product = multiply_conjugate(aligned, template[position]);
                    correlation.i += product.i;
                    correlation.q += product.q;
                }
            }
            for correlation in correlations {
                let squared = correlation.power();
                if squared > best_squared * energy {
                    best_squared = squared / energy;
                    best_match = Some(Match {
                        training_start,
                        offset_hz: slope * f64::from(self.rate_hz) / TAU,
                        phase: correlation.q.atan2(correlation.i),
                    });
                }
            }
        }
        best_match
    }
}

fn template(rate_hz: u32, start: usize, end: usize, stride: usize, first_bit: bool) -> Vec<Iq> {
    let mut tone = 1.0;
    let tones: [f64; OPENING_SYMBOLS] = std::array::from_fn(|symbol| {
        let bit = if symbol < TRAINING_SYMBOLS {
            first_bit == (symbol % 2 == 0)
        } else {
            FLAG & (1 << (symbol - TRAINING_SYMBOLS)) != 0
        };
        if !bit {
            tone = -tone;
        }
        tone
    });
    // For Gaussian BT = 0.4, the impulse-response standard deviation is
    // sqrt(ln 2) / (2π BT) symbols. Model nominal modulation index 0.5.
    let samples_per_symbol = f64::from(rate_hz) / f64::from(SYMBOL_RATE);
    let sigma = std::f64::consts::LN_2.sqrt() * samples_per_symbol / (TAU * 0.4);
    let radius = (4.0 * sigma).round() as i64;
    let kernel: Vec<_> = (-radius..=radius)
        .map(|offset| (-0.5 * (offset as f64 / sigma).powi(2)).exp())
        .collect();
    let increment = PI / (2.0 * samples_per_symbol * kernel.iter().sum::<f64>());
    let mut phase = 0.0;
    let mut samples = Vec::with_capacity((end - start).div_ceil(stride));
    for index in 0..end {
        let shaped: f64 = kernel
            .iter()
            .enumerate()
            .map(|(tap, weight)| {
                let sample_index = index as i64 + tap as i64 - radius;
                weight * template_tone(&tones, sample_index, rate_hz)
            })
            .sum();
        phase += increment * shaped;
        if index >= start && (index - start).is_multiple_of(stride) {
            samples.push(phasor(phase));
        }
    }
    let scale = samples
        .iter()
        .map(|sample| sample.power())
        .sum::<f64>()
        .sqrt()
        .recip();
    for sample in &mut samples {
        sample.i *= scale;
        sample.q *= scale;
    }
    samples
}

fn template_tone(tones: &[f64; OPENING_SYMBOLS], index: i64, rate_hz: u32) -> f64 {
    let position = index.max(0) as u64 * u64::from(SYMBOL_RATE);
    let rate = u64::from(rate_hz);
    let symbol = (position / rate) as usize;
    if symbol >= tones.len() - 1 {
        return tones[tones.len() - 1];
    }
    let overlap = (position % rate + u64::from(SYMBOL_RATE)).saturating_sub(rate);
    let weight = overlap as f64 / f64::from(SYMBOL_RATE);
    // A sample interval can straddle a transition when samples per symbol is fractional.
    tones[symbol] + (tones[symbol + 1] - tones[symbol]) * weight
}

fn multiply_conjugate(left: Iq, right: Iq) -> Iq {
    Iq {
        i: left.i * right.i + left.q * right.q,
        q: left.q * right.i - left.i * right.q,
    }
}
