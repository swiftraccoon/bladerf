//! Channelize the 100 kHz LTE carrier raster with a shared mixer and FIR.

use std::sync::Arc;

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::{
    FixedChannel, Iq16, LTE_FIR_TAPS, LTE_RATE_NUMERATOR, advance_lte_phase, lte_fractional_filter,
    tuning_word,
};
use crate::SAMPLE_RATE;

/// Translate selected 100 kHz-grid frequencies onto the same 1.92 MSps grid
/// as the scalar LTE channel. All bins share the input sample clock and phase origin.
pub(crate) struct LteRasterBank {
    mixer: FixedChannel,
    filters: [Vec<f32>; LTE_RATE_NUMERATOR],
    history: [Complex32; LTE_FIR_TAPS],
    oldest: usize,
    next_input_bin: usize,
    phase: usize,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    spectrum: [Complex32; Self::BIN_COUNT],
    bins: Vec<usize>,
    output: Vec<Complex32>,
}

impl LteRasterBank {
    /// Frequency difference in hertz between adjacent raster bins.
    pub(crate) const GRID_SPACING_HZ: u64 = 100_000;
    /// Number of wrapped FFT bins covering the full 20 MHz input sample rate.
    pub(crate) const BIN_COUNT: usize = (SAMPLE_RATE as u64 / Self::GRID_SPACING_HZ) as usize;

    /// Select bins in output order. Bin `k` tunes to `grid_offset_hz` plus
    /// `k * 100_000` Hz, modulo the 20 MHz input sample rate.
    ///
    /// # Errors
    /// Returns an error for a nonfinite offset, an offset outside ±50 kHz, or
    /// a missing, oversized or out-of-range bin selection.
    pub(crate) fn new(grid_offset_hz: f64, bins: &[usize]) -> Result<Self> {
        ensure!(
            grid_offset_hz.is_finite()
                && grid_offset_hz.abs() <= Self::GRID_SPACING_HZ as f64 * 0.5,
            "LTE raster offset must be within ±50 kHz"
        );
        ensure!(
            (1..=Self::BIN_COUNT).contains(&bins.len())
                && bins.iter().all(|&bin| bin < Self::BIN_COUNT),
            "LTE raster requires 1 through {} valid FFT bin selections",
            Self::BIN_COUNT
        );
        let fft = FftPlanner::new().plan_fft_forward(Self::BIN_COUNT);
        let zero = Complex32::default();
        let scratch = vec![zero; fft.get_inplace_scratch_len()];
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(grid_offset_hz, SAMPLE_RATE)?,
                ..FixedChannel::default()
            },
            filters: std::array::from_fn(|phase| {
                lte_fractional_filter(phase)
                    .into_iter()
                    .map(|tap| tap as f32)
                    .collect()
            }),
            history: [zero; LTE_FIR_TAPS],
            oldest: 0,
            next_input_bin: 0,
            phase: 0,
            fft,
            scratch,
            spectrum: [zero; Self::BIN_COUNT],
            bins: bins.to_vec(),
            output: vec![zero; bins.len()],
        })
    }

    /// Advance by one input sample, returning all selected channels when due.
    /// Startup uses zero history. The last partial output interval is withheld.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<&[Complex32]> {
        let [i, q] = self.mixer.mix(sample);
        self.history[self.oldest] = Complex32::new(i as f32, q as f32) / 32_768.0;
        self.oldest = (self.oldest + 1) % LTE_FIR_TAPS;
        let newest_bin = self.next_input_bin;
        self.next_input_bin = (newest_bin + 1) % Self::BIN_COUNT;
        let phase = advance_lte_phase(&mut self.phase)?;

        let (before, after) = self.history.split_at(self.oldest);
        let mut weighted = after
            .iter()
            .chain(before)
            .rev()
            .zip(&self.filters[phase])
            .map(|(&sample, &tap)| sample * tap);
        let (through_newest, beyond_newest) = self.spectrum.split_at_mut(newest_bin + 1);
        // Fold by absolute input index modulo the FFT length. The forward FFT
        // then applies the negative rotation for each bin at that input index.
        for (slot, value) in through_newest
            .iter_mut()
            .rev()
            .chain(beyond_newest.iter_mut().rev())
            .zip(&mut weighted)
        {
            *slot = value;
        }
        for (slot, value) in through_newest
            .iter_mut()
            .rev()
            .chain(beyond_newest.iter_mut().rev())
            .zip(weighted)
        {
            *slot += value;
        }
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (output, &bin) in self.output.iter_mut().zip(&self.bins) {
            *output = self.spectrum[bin];
        }
        Some(&self.output)
    }
}

const _: () = {
    assert!((SAMPLE_RATE as u64).is_multiple_of(LteRasterBank::GRID_SPACING_HZ));
    assert!(
        LTE_FIR_TAPS >= LteRasterBank::BIN_COUNT && LTE_FIR_TAPS <= 2 * LteRasterBank::BIN_COUNT
    );
};

#[cfg(test)]
mod tests {
    use super::super::{LTE_RATE_DENOMINATOR, LteSyncChannel};
    use super::*;
    use std::f64::consts::TAU;

    fn scalar_frequencies(offset_hz: f64, bins: [usize; 4]) -> [f64; 4] {
        let rate = f64::from(SAMPLE_RATE);
        bins.map(|bin| {
            let frequency = offset_hz + (bin as u64 * LteRasterBank::GRID_SPACING_HZ) as f64;
            (frequency + rate * 0.5).rem_euclid(rate) - rate * 0.5
        })
    }

    #[test]
    fn matches_scalar_cadence_and_phase_across_positive_and_negative_bins() {
        let bins = [176, 0, 2, 36];
        let input_count = 12_517;
        for offset_hz in [0.0, -49_999.75, 13.25] {
            let mut bank = LteRasterBank::new(offset_hz, &bins).unwrap();
            let frequencies = scalar_frequencies(offset_hz, bins);
            let mut scalar = frequencies.map(|frequency| LteSyncChannel::new(frequency).unwrap());
            let mut error = [0.0; 4];
            let mut power = [0.0; 4];
            let mut output_count = 0;
            for index in 0..input_count {
                let seconds = index as f64 / f64::from(SAMPLE_RATE);
                let mut i = 0.0;
                let mut q = 0.0;
                for (channel, frequency) in frequencies.iter().enumerate() {
                    let phase = TAU * (frequency + 23_000.0 * (channel + 1) as f64) * seconds;
                    let (sine, cosine) = phase.sin_cos();
                    i += 400.0 * cosine;
                    q += 400.0 * sine;
                }
                let sample = Iq16 {
                    i: i.round() as i16,
                    q: q.round() as i16,
                };
                let outputs = bank.push(sample);
                for (channel, receiver) in scalar.iter_mut().enumerate() {
                    let expected = receiver.push(sample);
                    assert_eq!(outputs.is_some(), expected.is_some());
                    if let (Some(outputs), Some(expected)) = (outputs, expected) {
                        let value = Complex32::new(expected.i as f32, expected.q as f32);
                        error[channel] += (value - outputs[channel]).norm_sqr();
                        power[channel] += value.norm_sqr();
                    }
                }
                output_count += usize::from(outputs.is_some());
            }
            assert_eq!(
                output_count,
                input_count * LTE_RATE_NUMERATOR / LTE_RATE_DENOMINATOR
            );
            for (error, power) in error.into_iter().zip(power) {
                // The shared mixer avoids the scalar grid NCOs' individual
                // 8-bit phase quantization. Bound their normalized RMS difference.
                assert!(
                    (error / power).sqrt() < 0.014,
                    "offset {offset_hz}, relative power error {}",
                    error / power
                );
            }
        }
    }

    #[test]
    fn uses_zero_history_and_does_not_flush_a_partial_output_interval() {
        let mut bank = LteRasterBank::new(0.0, &[25, 0]).unwrap();
        let mut outputs = 0;
        for index in 0..126 {
            let sample = if index == 0 || index == 125 {
                Iq16 { i: 1000, q: 0 }
            } else {
                Iq16::default()
            };
            let value = bank.push(sample);
            let output_count = (index + 1) * LTE_RATE_NUMERATOR / LTE_RATE_DENOMINATOR;
            assert_eq!(value.is_some(), output_count > outputs);
            if let Some(value) = value {
                let phase = (index + 1) * LTE_RATE_NUMERATOR % LTE_RATE_DENOMINATOR;
                let expected =
                    (1000.0 * 32767.0 / 32768.0 * lte_fractional_filter(phase)[index]) as f32;
                assert!(
                    value
                        .iter()
                        .all(|sample| (*sample - Complex32::new(expected, 0.0)).norm() < 0.001)
                );
            }
            outputs = output_count;
        }
        assert_eq!(outputs, 12);
        assert!(LteRasterBank::new(50_000.0, &[199]).is_ok());
        assert!(LteRasterBank::new(50_000.1, &[0]).is_err());
        assert!(LteRasterBank::new(0.0, &[200]).is_err());
    }
}
