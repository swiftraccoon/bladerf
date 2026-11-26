//! Energy-normalized primary synchronization search at 1.92 MSps.
//!
//! The sequence and subcarrier mapping follow [3GPP TS 36.211 v18.0.1,
//! §6.11.1](https://www.etsi.org/deliver/etsi_ts/136200_136299/136211/18.00.01_60/ts_136211v180001p.pdf).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// Useful OFDM symbol length in samples at 1.92 MSps, excluding the prefix.
pub(super) const SYMBOL_SAMPLES: usize = 128;

const ROOTS: [u32; 3] = [25, 29, 34];
const HALF_SUBCARRIERS: usize = 31;
const SUBCARRIERS: usize = 2 * HALF_SUBCARRIERS;
const FFT_SIZE: usize = 4_096;
const CORRELATION_DELAY: usize = SYMBOL_SAMPLES - 1;
const PEAK_RADIUS: usize = 16;
const STARTS_PER_BLOCK: usize = FFT_SIZE - CORRELATION_DELAY - 2 * PEAK_RADIUS;
const MAX_PEAKS: usize = 4_096;
const MIN_SCORE: f32 = 0.14;
// Signed offsets in 15 kHz subcarrier intervals.
const CARRIER_OFFSETS: [f32; 3] = [-0.5, 0.0, 0.5];

type TemplateSpectra = [[Vec<Complex32>; CARRIER_OFFSETS.len()]; ROOTS.len()];
type PeakRank = (u32, Reverse<usize>, Reverse<u8>);

/// A candidate PSS at the first useful sample of the symbol.
#[derive(Debug)]
pub(super) struct Peak {
    pub(super) start: usize,
    pub(super) identity: u8,
    /// Squared normalized correlation, between zero and one.
    pub(super) score: f32,
}

/// Return the PSS for N_ID^(2) in 0 through 2, with negative bins first.
/// The DC bin is omitted.
pub(super) fn sequence(identity: u8) -> [Complex32; SUBCARRIERS] {
    let root = ROOTS[usize::from(identity)];
    std::array::from_fn(|index| {
        let chip = (index + usize::from(index >= HALF_SUBCARRIERS)) as u32;
        let phase_index = (root * chip * (chip + 1)) % 126;
        Complex32::from_polar(1.0, -PI * phase_index as f32 / 63.0)
    })
}

fn template_spectra(
    fft: &dyn Fft<f32>,
    inverse_symbol: &dyn Fft<f32>,
    scratch: &mut [Complex32],
) -> TemplateSpectra {
    std::array::from_fn(|identity| {
        let chips = sequence(identity as u8);
        let mut symbol = [Complex32::default(); SYMBOL_SAMPLES];
        symbol[SYMBOL_SAMPLES - HALF_SUBCARRIERS..].copy_from_slice(&chips[..HALF_SUBCARRIERS]);
        symbol[1..=HALF_SUBCARRIERS].copy_from_slice(&chips[HALF_SUBCARRIERS..]);
        inverse_symbol.process_with_scratch(&mut symbol, scratch);
        let scale = ((SYMBOL_SAMPLES * SUBCARRIERS) as f32).sqrt().recip();
        CARRIER_OFFSETS.map(|offset| {
            let mut kernel = vec![Complex32::default(); FFT_SIZE];
            for (index, &sample) in symbol.iter().enumerate() {
                let phase = TAU * offset * index as f32 / SYMBOL_SAMPLES as f32;
                let shifted = sample * Complex32::from_polar(scale, phase);
                kernel[CORRELATION_DELAY - index] = shifted.conj();
            }
            fft.process_with_scratch(&mut kernel, scratch);
            kernel
        })
    })
}

fn normalization(block: &[Complex32], weights: &mut [f32]) {
    let sample_energy = |sample: &Complex32| f64::from(sample.norm_sqr());
    let block_energy: f64 = block.iter().map(sample_energy).sum();
    // Ignore numerical FFT leakage into windows over 100 dB below block energy.
    let energy_floor = block_energy * 1e-10;
    let mut window_energy: f64 = block[..SYMBOL_SAMPLES].iter().map(sample_energy).sum();
    let fft_scale = (FFT_SIZE * FFT_SIZE) as f64;
    for (index, weight) in weights.iter_mut().enumerate() {
        *weight = if window_energy > energy_floor {
            (fft_scale * window_energy).recip() as f32
        } else {
            0.0
        };
        if let Some(next) = block.get(index + SYMBOL_SAMPLES) {
            window_energy += sample_energy(next) - sample_energy(&block[index]);
        }
    }
}

fn is_local_peak(scores: &[f32], index: usize) -> bool {
    let score = scores[index];
    let low = index.saturating_sub(PEAK_RADIUS);
    let high = (index + PEAK_RADIUS + 1).min(scores.len());
    scores[low..index].iter().all(|&other| score > other)
        && scores[index + 1..high].iter().all(|&other| score >= other)
}

/// Prepared PSS templates and workspaces reusable across independent channels.
pub(super) struct Searcher {
    fft: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    templates: TemplateSpectra,
    spectrum: Vec<Complex32>,
    product: Vec<Complex32>,
    weights: Vec<f32>,
    scores: Vec<f32>,
    retained: BinaryHeap<Reverse<PeakRank>>,
}

impl Searcher {
    /// Prepare FFT plans, matched spectra, and working buffers for reuse.
    pub(super) fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let inverse_symbol = planner.plan_fft_inverse(SYMBOL_SAMPLES);
        let scratch_size = [&fft, &inverse, &inverse_symbol]
            .into_iter()
            .map(|transform| transform.get_inplace_scratch_len())
            .max()
            .unwrap_or_default();
        let mut scratch = vec![Complex32::default(); scratch_size];
        let templates = template_spectra(fft.as_ref(), inverse_symbol.as_ref(), &mut scratch);
        Self {
            fft,
            inverse,
            scratch,
            templates,
            spectrum: vec![Complex32::default(); FFT_SIZE],
            product: vec![Complex32::default(); FFT_SIZE],
            weights: vec![0.0; FFT_SIZE],
            scores: vec![0.0; FFT_SIZE],
            retained: BinaryHeap::new(),
        }
    }

    /// Find bounded, time-ordered PSS candidates in centered complex IQ at 1.92 MSps.
    ///
    /// Overlap-save correlation covers every complete symbol window. Competing
    /// carrier hypotheses share a timing peak for each identity. Repetition and
    /// SSS checks belong to the caller.
    pub(super) fn search(&mut self, input: &[Complex32]) -> Vec<Peak> {
        if input.len() < SYMBOL_SAMPLES {
            return Vec::new();
        }
        let windows = input.len() - CORRELATION_DELAY;
        let Self {
            fft,
            inverse,
            scratch,
            templates,
            spectrum,
            product,
            weights,
            scores,
            retained,
        } = self;
        for core_start in (0..windows).step_by(STARTS_PER_BLOCK) {
            let core_end = (core_start + STARTS_PER_BLOCK).min(windows);
            // Give the peak comparison a halo on both sides of a block boundary.
            let block_start = core_start.saturating_sub(PEAK_RADIUS);
            let block_end = (core_end + PEAK_RADIUS).min(windows);
            let block = &input[block_start..block_end + CORRELATION_DELAY];
            let count = block_end - block_start;
            let weights = &mut weights[..count];
            let scores = &mut scores[..count];
            normalization(block, weights);
            spectrum[..block.len()].copy_from_slice(block);
            spectrum[block.len()..].fill(Complex32::default());
            fft.process_with_scratch(spectrum, scratch);
            for (identity, kernels) in templates.iter().enumerate() {
                scores.fill(0.0);
                for kernel in kernels {
                    for ((output, &signal), &matched) in
                        product.iter_mut().zip(spectrum.iter()).zip(kernel)
                    {
                        *output = signal * matched;
                    }
                    inverse.process_with_scratch(product, scratch);
                    for ((score, &weight), match_value) in scores
                        .iter_mut()
                        .zip(weights.iter())
                        .zip(&product[CORRELATION_DELAY..])
                    {
                        *score = score.max(match_value.norm_sqr() * weight).min(1.0);
                    }
                }
                for start in core_start..core_end {
                    let index = start - block_start;
                    let score = scores[index];
                    if score < MIN_SCORE || !is_local_peak(scores, index) {
                        continue;
                    }
                    // Positive finite f32 bit patterns sort in score order. Reverse
                    // the position to retain the earliest peak when scores tie.
                    let rank = (score.to_bits(), Reverse(start), Reverse(identity as u8));
                    if retained.len() < MAX_PEAKS {
                        retained.push(Reverse(rank));
                    } else if let Some(mut weakest) = retained.peek_mut()
                        && rank > weakest.0
                    {
                        *weakest = Reverse(rank);
                    }
                }
            }
        }
        let mut peaks: Vec<_> = retained
            .drain()
            .map(|Reverse((score, Reverse(start), Reverse(identity)))| Peak {
                start,
                identity,
                score: f32::from_bits(score),
            })
            .collect();
        peaks.sort_unstable_by_key(|peak| (peak.start, peak.identity));
        peaks
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::{PI as PI64, TAU as TAU64};

    use super::*;

    fn noise(length: usize, amplitude: f32) -> Vec<Complex32> {
        let mut state = 0x4db5_29b6_u32;
        let mut component = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f32::from((state >> 16) as u16) / 32768.0 - 1.0) * amplitude
        };
        (0..length)
            .map(|_| Complex32::new(component(), component()))
            .collect()
    }

    fn add_symbol(output: &mut [Complex32], root: f64, carrier: f64, gain: f32) {
        // Direct OFDM synthesis from the 63-chip formula, with the center chip
        // omitted. This does not use the production sequence or FFT.
        let scale = gain / (128.0_f32 * 62.0).sqrt();
        for (index, sample) in output.iter_mut().enumerate() {
            let symbol: Complex32 = (-31..=31)
                .filter(|&bin| bin != 0)
                .map(|bin| {
                    let n = f64::from(bin + 31);
                    let phase = -PI64 * root * n * (n + 1.0) / 63.0
                        + TAU64 * (f64::from(bin) + carrier) * index as f64 / 128.0;
                    Complex32::from_polar(scale, (phase + 0.83) as f32)
                })
                .sum();
            *sample += symbol;
        }
    }

    #[test]
    fn finds_each_identity_at_boundaries_with_normalized_carrier_tolerance() {
        let mut signal = noise(2 * STARTS_PER_BLOCK + SYMBOL_SAMPLES, 0.001);
        let placements = [
            (0, 0, 25.0, -0.48, 0.5),
            (STARTS_PER_BLOCK - 1, 1, 29.0, 0.02, 1.5),
            (2 * STARTS_PER_BLOCK, 2, 34.0, 0.49, 0.8),
        ];
        for (start, _, root, carrier, gain) in placements {
            add_symbol(
                &mut signal[start..start + SYMBOL_SAMPLES],
                root,
                carrier,
                gain,
            );
        }
        let mut searcher = Searcher::new();
        let peaks = searcher.search(&signal);
        for (start, identity, _, _, _) in placements {
            let best = peaks
                .iter()
                .filter(|peak| peak.start.abs_diff(start) <= PEAK_RADIUS)
                .max_by(|left, right| left.score.total_cmp(&right.score));
            assert!(
                best.is_some_and(|peak| peak.start == start
                    && peak.identity == identity
                    && peak.score > 0.99),
                "expected root {identity} at sample {start}, found {best:?}"
            );
        }
        assert!(
            searcher
                .search(&[Complex32::default(); SYMBOL_SAMPLES])
                .is_empty()
        );
        let repeated = searcher.search(&signal);
        assert_eq!(peaks.len(), repeated.len());
        for (first, second) in peaks.iter().zip(repeated) {
            assert_eq!(first.start, second.start);
            assert_eq!(first.identity, second.identity);
            assert_eq!(first.score.to_bits(), second.score.to_bits());
        }
    }

    #[test]
    fn rejects_short_zero_noise_and_tone_without_pss() {
        let mut searcher = Searcher::new();
        assert!(
            searcher
                .search(&[Complex32::default(); SYMBOL_SAMPLES - 1])
                .is_empty()
        );
        assert!(
            searcher
                .search(&[Complex32::default(); FFT_SIZE])
                .is_empty()
        );
        assert!(searcher.search(&noise(5 * FFT_SIZE, 0.25)).is_empty());
        let tone: Vec<_> = (0..5 * FFT_SIZE)
            .map(|index| Complex32::from_polar(1.0, TAU * index as f32 / 29.0))
            .collect();
        assert!(searcher.search(&tone).is_empty());
    }
}
