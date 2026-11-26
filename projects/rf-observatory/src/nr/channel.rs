//! Translate 20 MSps Q11 onto the 256-sample NR synchronization symbol grid.

use std::f64::consts::{PI, TAU};

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex32;

use crate::SAMPLE_RATE;
use crate::dsp::{Iq16, SIN_Q15, tuning_word};

const TAPS: usize = 129;
const RATE_DENOMINATOR: usize = 125;
const QUARTER_TURN: u32 = 1 << 30;
const TABLE_PHASE_BITS: u32 = 24;
const TABLE_PHASE_SCALE: u32 = 1 << TABLE_PHASE_BITS;
const _: () = assert!(SAMPLE_RATE == 20_000_000);

fn rate_numerator(scs_hz: u32) -> usize {
    match scs_hz {
        15_000 => 24,
        30_000 => 48,
        _ => 0,
    }
}

/// Count complete output intervals from a prefix of original-rate samples.
/// Unsupported subcarrier spacings have zero output intervals.
pub(super) fn output_count(input_samples: u64, scs_hz: u32) -> usize {
    let output =
        u128::from(input_samples) * rate_numerator(scs_hz) as u128 / RATE_DENOMINATOR as u128;
    usize::try_from(output).unwrap_or(usize::MAX)
}

fn fractional_lowpass(cutoff_hz: f64, delay: f64) -> [f32; TAPS] {
    let cutoff = cutoff_hz / f64::from(SAMPLE_RATE);
    let center = (TAPS - 1) as f64 / 2.0 + delay;
    let coefficients: [f64; TAPS] = std::array::from_fn(|index| {
        let distance = index as f64 - center;
        let sinc = if distance == 0.0 {
            2.0 * cutoff
        } else {
            (TAU * cutoff * distance).sin() / (PI * distance)
        };
        let angle = TAU * index as f64 / (TAPS - 1) as f64;
        let window = 0.42 - 0.5 * angle.cos() + 0.08 * (2.0 * angle).cos();
        sinc * window
    });
    let gain: f64 = coefficients.iter().sum();
    coefficients.map(|tap| (tap / gain) as f32)
}

fn sine(phase: u32) -> f32 {
    let index = (phase >> TABLE_PHASE_BITS) as usize;
    let fraction = (phase & (TABLE_PHASE_SCALE - 1)) as f32 / TABLE_PHASE_SCALE as f32;
    let left = f32::from(SIN_Q15[index]);
    let right = f32::from(SIN_Q15[(index + 1) % SIN_Q15.len()]);
    (left + (right - left) * fraction) / f32::from(i16::MAX)
}

/// Causal frequency translation and fractional low-pass resampling.
/// History starts at zero and output amplitudes retain the input Q11 scale.
pub(super) struct Channelizer {
    tuning_word: u32,
    oscillator_phase: u32,
    interval_phase: usize,
    numerator: usize,
    filters: Vec<[f32; TAPS]>,
    history: [Complex32; TAPS],
    oldest: usize,
}

impl Channelizer {
    /// Nominal FIR delay in original-rate input samples.
    pub(super) const DELAY_INPUT_SAMPLES: usize = (TAPS - 1) / 2;

    /// Prepare a channel at 3.84 MSps for 15 kHz or 7.68 MSps for 30 kHz.
    ///
    /// # Errors
    /// Rejects unsupported spacings, nonfinite offsets, and a complete output
    /// frequency span that touches or crosses the input Nyquist edge.
    pub(super) fn new(offset_hz: f64, scs_hz: u32) -> Result<Self> {
        let numerator = rate_numerator(scs_hz);
        ensure!(
            numerator > 0,
            "NR synchronization requires 15 or 30 kHz spacing"
        );
        let output_half_span_hz = f64::from(scs_hz) * 128.0;
        ensure!(
            offset_hz.is_finite()
                && offset_hz.abs() + output_half_span_hz < f64::from(SAMPLE_RATE) / 2.0,
            "NR synchronization channel must fit inside the sampled Nyquist band"
        );
        // A cut at 100 subcarrier intervals leaves the inner synchronization
        // bins in the passband and gives the FIR room to reject output aliases.
        let cutoff_hz = f64::from(scs_hz) * 100.0;
        let filters = (0..numerator)
            .map(|phase| fractional_lowpass(cutoff_hz, phase as f64 / numerator as f64))
            .collect();
        Ok(Self {
            tuning_word: tuning_word(offset_hz, SAMPLE_RATE)?,
            oscillator_phase: 0,
            interval_phase: 0,
            numerator,
            filters,
            history: [Complex32::default(); TAPS],
            oldest: 0,
        })
    }

    /// Advance one input sample, with no allocation or I/O.
    /// Output index `n` represents `(n + 1) * 125 / L - 1` on the input grid
    /// before subtracting the nominal FIR delay, where `L` is 24 or 48.
    pub(super) fn push(&mut self, sample: Iq16) -> Option<Complex32> {
        let rotation = Complex32::new(
            sine(self.oscillator_phase.wrapping_add(QUARTER_TURN)),
            -sine(self.oscillator_phase),
        );
        self.oscillator_phase = self.oscillator_phase.wrapping_add(self.tuning_word);
        self.history[self.oldest] =
            Complex32::new(f32::from(sample.i), f32::from(sample.q)) * rotation;
        self.oldest = (self.oldest + 1) % TAPS;
        self.interval_phase += self.numerator;
        if self.interval_phase < RATE_DENOMINATOR {
            return None;
        }
        self.interval_phase -= RATE_DENOMINATOR;
        let (before, after) = self.history.split_at(self.oldest);
        let mut output = Complex32::default();
        for (&sample, &coefficient) in after
            .iter()
            .chain(before)
            .rev()
            .zip(&self.filters[self.interval_phase])
        {
            output += sample * coefficient;
        }
        Some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONE_AMPLITUDE: f64 = 1_400.0;

    fn tone(frequency_hz: f64, input_index: usize) -> Iq16 {
        let phase = TAU * frequency_hz * input_index as f64 / f64::from(SAMPLE_RATE);
        Iq16 {
            i: (TONE_AMPLITUDE * phase.cos()).round() as i16,
            q: (TONE_AMPLITUDE * phase.sin()).round() as i16,
        }
    }

    #[test]
    fn cadence_matches_complete_output_intervals() {
        for (spacing, rate) in [(15_000, 3_840_000), (30_000, 7_680_000)] {
            let mut filter = Channelizer::new(0.0, spacing).unwrap();
            let mut emitted = 0;
            for input in 1..=1_000 {
                if let Some(output) = filter.push(Iq16::default()) {
                    assert_eq!(output, Complex32::default());
                    emitted += 1;
                }
                assert_eq!(emitted, (input * rate / u64::from(SAMPLE_RATE)) as usize);
                assert_eq!(emitted, output_count(input, spacing));
            }
        }
    }

    #[test]
    fn signed_translation_preserves_passband_amplitude_and_delay() {
        for (spacing, offset, residual) in [
            (15_000, -3_125_750.0, 1_100_000.0),
            (30_000, 4_681_250.0, -2_200_000.0),
        ] {
            let mut filter = Channelizer::new(offset, spacing).unwrap();
            for (index, output) in (0..10_000)
                .filter_map(|input| filter.push(tone(offset + residual, input)))
                .enumerate()
                .skip(TAPS)
            {
                let time = (index + 1) as f64 / (f64::from(spacing) * 256.0)
                    - (1 + Channelizer::DELAY_INPUT_SAMPLES) as f64 / f64::from(SAMPLE_RATE);
                let phase = TAU * residual * time;
                let expected = Complex32::new(phase.cos() as f32, phase.sin() as f32);
                let error = (output / TONE_AMPLITUDE as f32 - expected).norm();
                assert!(error < 0.01, "spacing {spacing}, sample {index}: {error}");
            }
        }
    }

    #[test]
    fn rejects_tones_near_and_beyond_output_nyquist() {
        for (spacing, offset) in [(15_000, 3_125_750.0), (30_000, -4_241_900.0)] {
            for residual in [
                f64::from(spacing) * 128.0 + 20_000.0,
                f64::from(spacing) * 256.0,
            ] {
                let mut filter = Channelizer::new(offset, spacing).unwrap();
                let peak = (0..10_000)
                    .filter_map(|input| filter.push(tone(offset + residual, input)))
                    .skip(TAPS)
                    .map(|output| output.norm())
                    .fold(0.0_f32, f32::max);
                assert!(peak < TONE_AMPLITUDE as f32 * 0.0015, "{spacing}: {peak}");
            }
        }
    }

    #[test]
    fn rejects_unsupported_spacing_and_nyquist_edges() {
        assert!(Channelizer::new(0.0, 60_000).is_err());
        for offset in [f64::NAN, f64::INFINITY, -8_080_000.0, 8_080_000.0] {
            assert!(Channelizer::new(offset, 15_000).is_err());
        }
        assert!(Channelizer::new(6_160_000.0, 30_000).is_err());
        assert!(Channelizer::new(6_159_999.0, 30_000).is_ok());
    }
}
