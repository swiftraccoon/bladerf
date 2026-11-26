//! Oversampled FIR/FFT channelization on the 12.5 kHz raster.

use std::sync::Arc;

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::{FixedChannel, Iq16, lowpass, tuning_word};
use crate::SAMPLE_RATE;

const RS41_FIR_TAPS: usize = 4_801;
const RS41_CUTOFF_HZ: f64 = 12_000.0;
const DMR_FIR_TAPS: usize = 9_601;
const DMR_CUTOFF_HZ: f64 = 7_500.0;

/// Translate selected raster centers on a shared 20 MS/s input clock.
pub(crate) struct RasterBank {
    mixer: FixedChannel,
    interpolate_oscillator: bool,
    stride: usize,
    coefficients: Vec<f32>,
    // Mirror the ring to visit chronological FIR windows without a split.
    history: Vec<Complex32>,
    oldest: usize,
    pending: usize,
    newest_bin: usize,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    spectrum: [Complex32; Self::BIN_COUNT],
    bins: Vec<usize>,
    outputs: Vec<Complex32>,
}

impl RasterBank {
    /// Frequency spacing between neighboring raster centers.
    pub(crate) const GRID_SPACING_HZ: u64 = 12_500;
    /// Number of wrapped bins covering the 20 MHz input sample grid.
    pub(crate) const BIN_COUNT: usize = 1_600;
    /// Input samples per complete RS41 output interval.
    pub(crate) const RS41_STRIDE: usize = 400;
    /// RS41 complex samples per second in each selected bin.
    pub(crate) const RS41_OUTPUT_RATE_HZ: u32 = SAMPLE_RATE / Self::RS41_STRIDE as u32;
    /// Nominal RS41 group delay on the original sample grid.
    pub(crate) const RS41_DELAY_INPUT_SAMPLES: u64 = (RS41_FIR_TAPS as u64 - 1) / 2;
    /// Input samples per complete DMR output interval.
    pub(crate) const DMR_STRIDE: usize = 800;
    /// DMR complex samples per second in each selected bin.
    pub(crate) const DMR_OUTPUT_RATE_HZ: u32 = SAMPLE_RATE / Self::DMR_STRIDE as u32;
    /// Nominal DMR group delay on the original sample grid.
    pub(crate) const DMR_DELAY_INPUT_SAMPLES: u64 = (DMR_FIR_TAPS as u64 - 1) / 2;

    /// Configure the RS41 channelizer with the requested bins in output order.
    /// Bin `k` is centered at `grid_offset_hz + k * 12500` Hz, wrapped at the
    /// input sample rate.
    ///
    /// # Errors
    /// Rejects an offset outside half a grid interval, an empty or oversized
    /// selection, or a bin outside the transform.
    pub(crate) fn rs41(grid_offset_hz: i64, bins: &[usize]) -> Result<Self> {
        Self::new(
            grid_offset_hz,
            bins,
            RS41_FIR_TAPS,
            RS41_CUTOFF_HZ,
            Self::RS41_STRIDE,
            false,
        )
    }

    /// Configure the DMR channelizer with the requested bins in output order.
    /// Bin `k` is centered at `grid_offset_hz + k * 12500` Hz, wrapped at the
    /// input sample rate.
    ///
    /// # Errors
    /// Rejects an offset outside half a grid interval, an empty or oversized
    /// selection, or a bin outside the transform.
    pub(crate) fn dmr(grid_offset_hz: i64, bins: &[usize]) -> Result<Self> {
        Self::new(
            grid_offset_hz,
            bins,
            DMR_FIR_TAPS,
            DMR_CUTOFF_HZ,
            Self::DMR_STRIDE,
            true,
        )
    }

    fn new(
        grid_offset_hz: i64,
        bins: &[usize],
        taps: usize,
        cutoff_hz: f64,
        stride: usize,
        interpolate_oscillator: bool,
    ) -> Result<Self> {
        ensure!(
            grid_offset_hz.unsigned_abs() <= Self::GRID_SPACING_HZ / 2,
            "raster grid offset must be within ±6250 Hz"
        );
        ensure!(
            (1..=Self::BIN_COUNT).contains(&bins.len())
                && bins.iter().all(|&bin| bin < Self::BIN_COUNT),
            "raster requires 1 through {} valid FFT bin selections",
            Self::BIN_COUNT
        );
        let fft = FftPlanner::new().plan_fft_forward(Self::BIN_COUNT);
        let zero = Complex32::default();
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(grid_offset_hz as f64, SAMPLE_RATE)?,
                ..FixedChannel::default()
            },
            interpolate_oscillator,
            stride,
            coefficients: lowpass(taps, cutoff_hz, f64::from(SAMPLE_RATE))
                .into_iter()
                .rev()
                .map(|tap| tap as f32)
                .collect(),
            history: vec![zero; 2 * taps],
            oldest: 0,
            pending: 0,
            newest_bin: stride - 1,
            scratch: vec![zero; fft.get_inplace_scratch_len()],
            fft,
            spectrum: [zero; Self::BIN_COUNT],
            bins: bins.to_vec(),
            outputs: vec![zero; bins.len()],
        })
    }

    /// Advance one input sample and return selected channels at the end of a
    /// complete interval. Initial FIR history is zero.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<&[Complex32]> {
        let value = if self.interpolate_oscillator {
            let mixed = self.mixer.mix_interpolated(sample);
            Complex32::new(mixed.i as f32, mixed.q as f32)
        } else {
            let [i, q] = self.mixer.mix(sample);
            Complex32::new(i as f32, q as f32) / 32_768.0
        };
        let taps = self.coefficients.len();
        self.history[self.oldest] = value;
        self.history[self.oldest + taps] = value;
        self.oldest += 1;
        if self.oldest == taps {
            self.oldest = 0;
        }
        self.pending += 1;
        if self.pending < self.stride {
            return None;
        }
        self.pending = 0;

        self.spectrum.fill(Complex32::default());
        // Fold by absolute input index modulo the FFT length. The tap count
        // makes the oldest and newest indices share the same wrapped bin.
        let (before, after) = self.spectrum.split_at_mut(self.newest_bin);
        let samples = &self.history[self.oldest..self.oldest + taps];
        for (samples, taps) in samples
            .chunks(Self::BIN_COUNT)
            .zip(self.coefficients.chunks(Self::BIN_COUNT))
        {
            let split = after.len().min(samples.len());
            let (samples_high, samples_low) = samples.split_at(split);
            let (taps_high, taps_low) = taps.split_at(split);
            for (bins, samples, taps) in [
                (&mut *after, samples_high, taps_high),
                (&mut *before, samples_low, taps_low),
            ] {
                for ((slot, &sample), &tap) in bins.iter_mut().zip(samples).zip(taps) {
                    *slot += sample * tap;
                }
            }
        }
        self.newest_bin = (self.newest_bin + self.stride) % Self::BIN_COUNT;
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (output, &bin) in self.outputs.iter_mut().zip(&self.bins) {
            *output = self.spectrum[bin];
        }
        Some(&self.outputs)
    }
}

const _: () = {
    assert!((RS41_FIR_TAPS - 1).is_multiple_of(RasterBank::BIN_COUNT));
    assert!((DMR_FIR_TAPS - 1).is_multiple_of(RasterBank::BIN_COUNT));
    assert!(RasterBank::BIN_COUNT as u64 * RasterBank::GRID_SPACING_HZ == SAMPLE_RATE as u64);
    assert!(RasterBank::RS41_STRIDE as u32 * RasterBank::RS41_OUTPUT_RATE_HZ == SAMPLE_RATE);
    assert!(RasterBank::DMR_STRIDE as u32 * RasterBank::DMR_OUTPUT_RATE_HZ == SAMPLE_RATE);
};

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU;

    use rustfft::num_complex::Complex64;

    use super::*;

    fn tone(hz: f64, sample: usize) -> Complex64 {
        Complex64::from_polar(1.0, TAU * hz * sample as f64 / f64::from(SAMPLE_RATE))
    }

    fn quantize(value: Complex64) -> Iq16 {
        Iq16 {
            i: value.re.round() as i16,
            q: value.im.round() as i16,
        }
    }

    #[test]
    fn matches_scalar_sign_phase_and_timing_across_wraps() {
        let offset = 1_379;
        let centers = [offset as f64 - 12_500.0, offset as f64 + 87_500.0];
        let mut bank = RasterBank::rs41(offset, &[1_599, 7]).unwrap();
        let taps = lowpass(RS41_FIR_TAPS, RS41_CUTOFF_HZ, f64::from(SAMPLE_RATE));
        let mut inputs = Vec::new();
        let mut relative_errors = [0.0; 2];
        let mut reference_powers = [0.0; 2];
        let input_count = 12_517;
        let mut outputs = 0;
        for index in 0..input_count {
            let sample = quantize(
                (tone(centers[0] + 6_250.0, index) + tone(centers[1] - 6_250.0, index)) * 800.0,
            );
            inputs.push(Complex64::new(f64::from(sample.i), f64::from(sample.q)));
            let values = bank.push(sample);
            assert_eq!(
                values.is_some(),
                (index + 1).is_multiple_of(RasterBank::RS41_STRIDE)
            );
            if let Some(values) = values {
                outputs += 1;
                for (channel, &frequency) in centers.iter().enumerate() {
                    let expected = taps
                        .iter()
                        .zip((0..=index).rev())
                        .map(|(&tap, input)| inputs[input] * tone(-frequency, input) * tap)
                        .sum::<Complex64>();
                    let actual = Complex64::new(
                        f64::from(values[channel].re),
                        f64::from(values[channel].im),
                    );
                    relative_errors[channel] += (actual - expected).norm_sqr();
                    reference_powers[channel] += expected.norm_sqr();
                }
            }
        }
        assert_eq!(outputs, input_count / RasterBank::RS41_STRIDE);
        for (error, power) in relative_errors.into_iter().zip(reference_powers) {
            assert!((error / power).sqrt() < 0.014);
        }
    }

    #[test]
    fn retains_grid_edge_tones_and_rejects_decimation_aliases() {
        let amplitude = 1_800.0;
        for (frequency, limits) in [
            (-8_650.0, 0.79..0.83),
            (8_650.0, 0.79..0.83),
            (41_350.0, 0.0..0.0005),
        ] {
            let mut bank = RasterBank::rs41(0, &[0]).unwrap();
            let gain = (0..16_000)
                .filter_map(|index| {
                    bank.push(quantize(tone(frequency, index) * amplitude))
                        .filter(|_| index >= RS41_FIR_TAPS)
                        .map(|outputs| f64::from(outputs[0].norm()) / amplitude)
                })
                .fold(0.0_f64, f64::max);
            assert!(limits.contains(&gain), "frequency {frequency}, gain {gain}");
        }
    }

    #[test]
    fn dmr_preserves_signed_bin_phase_and_rejects_neighbor_and_alias_tones() {
        let amplitude = 1_800.0;
        let offset = 1_379;
        let center_hz = offset as f64 - 12_500.0;
        for (residual_hz, gain_limits) in [
            (6_000.0, 0.77..0.79),
            (-11_500.0, 0.013..0.016),
            (18_000.0, 0.0..0.001),
        ] {
            let mut bank = RasterBank::dmr(offset, &[1_599]).unwrap();
            for index in 0..16_000 {
                let sample = quantize(tone(center_hz + residual_hz, index) * amplitude);
                let values = bank.push(sample);
                assert_eq!(
                    values.is_some(),
                    (index + 1).is_multiple_of(RasterBank::DMR_STRIDE)
                );
                if let Some(values) = values.filter(|_| index >= DMR_FIR_TAPS) {
                    let phase_time = index - RasterBank::DMR_DELAY_INPUT_SAMPLES as usize;
                    let phase = tone(-residual_hz, phase_time);
                    let actual = Complex64::new(f64::from(values[0].re), f64::from(values[0].im));
                    let gain = actual * phase / amplitude;
                    assert!(
                        gain_limits.contains(&gain.norm()) && gain.im.abs() < 0.001,
                        "residual frequency {residual_hz}, gain {gain}"
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_input_geometry() {
        assert!(RasterBank::rs41(-6_250, &[1_599]).is_ok());
        assert!(RasterBank::rs41(6_251, &[0]).is_err());
        assert!(RasterBank::rs41(0, &[]).is_err());
        assert!(RasterBank::rs41(0, &[1_600]).is_err());
    }
}
