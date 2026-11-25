//! Centered overlap-save filtering for larger software retunes.

use std::cell::Cell;
use std::f64::consts::{PI, TAU};
use std::io::{self, Read};
use std::sync::Arc;

use rustfft::num_complex::Complex64;
use rustfft::{Fft, FftPlanner};

use super::translate::{Rotation, iq_pair, quantize_cs8};
use super::{IqFormat, SAMPLE_RATE_HZ};

pub(super) const FILTER_NAME: &str = "anti-wrap-kaiser-257";
const HALF_TAPS: usize = 128;
const TAPS: usize = 2 * HALF_TAPS + 1;
const HISTORY: usize = TAPS - 1;
const FFT_SAMPLES: usize = 8_192;
const BLOCK_SAMPLES: usize = FFT_SAMPLES - HISTORY;
const KAISER_BETA: f64 = 8.0;
const GUARD_HZ: f64 = 200_000.0;

fn bessel_i0(argument: f64) -> f64 {
    let mut term = 1.0;
    let mut sum = term;
    let scaled = (argument / 2.0).powi(2);
    let mut order = 1.0_f64;
    while term > f64::EPSILON * sum {
        term *= scaled / order.powi(2);
        sum += term;
        order += 1.0;
    }
    sum
}

fn kernel(shift_hz: i64) -> [Complex64; TAPS] {
    let rate = f64::from(SAMPLE_RATE_HZ);
    let cutoff_hz = (rate - shift_hz.unsigned_abs() as f64) / 2.0 - GUARD_HZ;
    let center_hz = -(shift_hz as f64) / 2.0;
    let window_peak = bessel_i0(KAISER_BETA);
    let baseband: [f64; TAPS] = std::array::from_fn(|index| {
        let offset = index as f64 - HALF_TAPS as f64;
        let sinc = if index == HALF_TAPS {
            2.0 * cutoff_hz / rate
        } else {
            (TAU * cutoff_hz * offset / rate).sin() / (PI * offset)
        };
        let radius = offset / HALF_TAPS as f64;
        let window = bessel_i0(KAISER_BETA * (1.0 - radius * radius).sqrt()) / window_peak;
        sinc * window
    });
    let dc_gain = baseband.iter().sum::<f64>();
    std::array::from_fn(|index| {
        let offset = index as f64 - HALF_TAPS as f64;
        let phase = TAU * center_hz * offset / rate;
        Complex64::from_polar(1.0, phase) * (baseband[index] / dc_gain)
    })
}

/// Translate exactly the admitted prefix. The centered FIR uses zero context
/// at both ends and emits the same number of complex samples.
pub(super) struct FilteredReader<'a, R> {
    input: R,
    format: IqFormat,
    remaining: usize,
    remaining_padding: usize,
    remaining_delay: usize,
    buffer: Vec<u8>,
    available: std::ops::Range<usize>,
    history: [Complex64; HISTORY],
    block: Vec<Complex64>,
    scratch: Vec<Complex64>,
    spectrum: Vec<Complex64>,
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    rotation: Rotation,
    clipped: &'a Cell<u64>,
    gain_clipped: Option<&'a Cell<u64>>,
}

impl<'a, R: Read> FilteredReader<'a, R> {
    pub(super) fn new(
        input: R,
        format: IqFormat,
        shift_hz: i64,
        samples: usize,
        clipped: &'a Cell<u64>,
        gain_clipped: Option<&'a Cell<u64>>,
    ) -> Self {
        let mut planner = FftPlanner::new();
        let forward = planner.plan_fft_forward(FFT_SAMPLES);
        let inverse = planner.plan_fft_inverse(FFT_SAMPLES);
        let mut scratch = vec![
            Complex64::default();
            forward
                .get_inplace_scratch_len()
                .max(inverse.get_inplace_scratch_len())
        ];
        let mut spectrum = vec![Complex64::default(); FFT_SAMPLES];
        spectrum[..TAPS].copy_from_slice(&kernel(shift_hz));
        forward.process_with_scratch(&mut spectrum, &mut scratch);
        for value in &mut spectrum {
            *value /= FFT_SAMPLES as f64;
        }
        Self {
            input,
            format,
            remaining: samples,
            remaining_padding: if samples == 0 { 0 } else { HALF_TAPS },
            remaining_delay: HALF_TAPS,
            buffer: vec![0; BLOCK_SAMPLES * format.bytes_per_sample()],
            available: 0..0,
            history: [Complex64::default(); HISTORY],
            block: vec![Complex64::default(); FFT_SAMPLES],
            scratch,
            spectrum,
            forward,
            inverse,
            rotation: Rotation::new(shift_hz),
            clipped,
            gain_clipped,
        }
    }

    fn fill(&mut self) -> io::Result<()> {
        let samples = BLOCK_SAMPLES.min(self.remaining);
        let padding = (BLOCK_SAMPLES - samples).min(self.remaining_padding);
        let bytes_per_sample = self.format.bytes_per_sample();
        let raw = &mut self.buffer[..samples * bytes_per_sample];
        self.input.read_exact(raw)?;
        self.block[..HISTORY].copy_from_slice(&self.history);
        for (value, input) in self.block[HISTORY..HISTORY + samples]
            .iter_mut()
            .zip(raw.chunks_exact(bytes_per_sample))
        {
            let [i, q] = iq_pair(input, self.format, self.gain_clipped)?;
            *value = Complex64::new(f64::from(i), f64::from(q));
        }
        self.block[HISTORY + samples..].fill(Complex64::default());
        self.history.copy_from_slice(&self.block[BLOCK_SAMPLES..]);
        self.remaining -= samples;
        self.remaining_padding -= padding;

        self.forward
            .process_with_scratch(&mut self.block, &mut self.scratch);
        for (value, response) in self.block.iter_mut().zip(&self.spectrum) {
            *value *= response;
        }
        self.inverse
            .process_with_scratch(&mut self.block, &mut self.scratch);

        // Skip the causal FIR delay and flush the same number of samples at EOF.
        let filled = samples + padding;
        let skipped = filled.min(self.remaining_delay);
        self.remaining_delay -= skipped;
        let valid = &self.block[HISTORY + skipped..HISTORY + filled];
        let mut clipped_count = 0;
        for (pair, value) in self.buffer.as_chunks_mut::<2>().0.iter_mut().zip(valid) {
            let (quantized, clipped) = quantize_cs8(self.rotation.rotate(value.re, value.im));
            *pair = quantized;
            clipped_count += u64::from(clipped);
        }
        self.clipped.set(self.clipped.get() + clipped_count);
        self.available = 0..2 * valid.len();
        Ok(())
    }
}

impl<R: Read> Read for FilteredReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.available.is_empty() && self.remaining + self.remaining_padding > 0 {
            self.fill()?;
        }
        let count = output.len().min(self.available.len());
        let start = self.available.start;
        output[..count].copy_from_slice(&self.buffer[start..start + count]);
        self.available.start += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_the_band_that_would_wrap_at_either_shift_sign() {
        for shift in [-5_000_000_i64, -2_000_000, 2_000_000, 5_000_000] {
            let taps = kernel(shift);
            let response = |frequency_hz: f64| {
                taps.iter()
                    .enumerate()
                    .map(|(index, tap)| {
                        let offset = index as f64 - HALF_TAPS as f64;
                        let phase = -TAU * frequency_hz * offset / f64::from(SAMPLE_RATE_HZ);
                        *tap * Complex64::from_polar(1.0, phase)
                    })
                    .sum::<Complex64>()
                    .norm()
            };
            let direction = shift.signum() as f64;
            let nyquist_hz = f64::from(SAMPLE_RATE_HZ) / 2.0;
            let wrap_edge_hz = nyquist_hz - shift.unsigned_abs() as f64;
            for frequency_hz in [
                wrap_edge_hz,
                (wrap_edge_hz + nyquist_hz) / 2.0,
                nyquist_hz - 50_000.0,
            ] {
                assert!(
                    response(direction * frequency_hz) < 1e-4,
                    "shift {shift} Hz at wrap frequency {frequency_hz} Hz"
                );
            }
            assert!((response(direction * (wrap_edge_hz - 400_000.0)) - 1.0).abs() < 2e-4);
            assert!((response(-direction * 9_000_000.0) - 1.0).abs() < 2e-4);
        }
    }

    #[test]
    fn matches_centered_convolution_across_blocks_with_odd_reads() {
        // A short tail after the first block exercises history and zero padding.
        let len = BLOCK_SAMPLES + 17;
        let mut rng = 47_u32;
        let mut component = || {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (rng >> 22) as i16 - 512
        };
        let input: Vec<_> = (0..len).map(|_| [component(), component()]).collect();
        let raw: Vec<_> = input
            .iter()
            .flatten()
            .copied()
            .flat_map(i16::to_le_bytes)
            .collect();
        for shift in [-5_000_000, -2_000_000, 1_750_000, 5_000_000] {
            let clipped = Cell::new(0);
            let mut reader = FilteredReader::new(
                raw.as_slice(),
                IqFormat::Sc16Q11,
                shift,
                len,
                &clipped,
                None,
            );
            let mut actual = Vec::new();
            let mut buffer = [0; 17];
            loop {
                let count = reader.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                actual.extend_from_slice(&buffer[..count]);
            }

            let taps = kernel(shift);
            let mut rotation = Rotation::new(shift);
            let expected: Vec<_> = (0..len)
                .flat_map(|index| {
                    let filtered: Complex64 = taps
                        .iter()
                        .enumerate()
                        .filter_map(|(offset, tap)| {
                            let sample = (index + HALF_TAPS).checked_sub(offset)?;
                            let &[i, q] = input.get(sample)?;
                            Some(*tap * Complex64::new(f64::from(i), f64::from(q)))
                        })
                        .sum();
                    quantize_cs8(rotation.rotate(filtered.re, filtered.im)).0
                })
                .collect();
            assert_eq!(actual.len(), 2 * len, "shift {shift} Hz");
            assert_eq!(actual, expected, "shift {shift} Hz");
            assert_eq!(clipped.get(), 0);
        }
    }

    #[test]
    fn limits_lookahead_and_gain_to_the_admitted_prefix() {
        // The invalid suffix would fail Q11 validation if read as FIR context.
        let raw: Vec<_> = [1500_i16, -1500, 0, 0, i16::MAX, 0]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let clipped = Cell::new(0);
        let gain_clipped = Cell::new(0);
        let mut reader = FilteredReader::new(
            raw.as_slice(),
            IqFormat::Sc16Q11,
            -2_000_000,
            2,
            &clipped,
            Some(&gain_clipped),
        );
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(gain_clipped.get(), 2);

        let mut reader = FilteredReader::new(
            raw.as_slice(),
            IqFormat::Sc16Q11,
            -2_000_000,
            3,
            &clipped,
            None,
        );
        assert_eq!(
            reader.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
