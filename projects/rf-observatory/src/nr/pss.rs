//! Energy-normalized primary synchronization search on the NR symbol grid.
//!
//! [3GPP TS 38.211 v17.8.0, §§7.4.2.2 and 7.4.3.1](https://www.etsi.org/deliver/etsi_ts/138200_138299/138211/17.08.00_60/ts_138211v170800p.pdf)
//! defines the chips and their placement. [3GPP TS 38.104 v16.20.0,
//! §5.4.3.2](https://www.etsi.org/deliver/etsi_ts/138100_138199/138104/16.20.00_60/ts_138104v162000p.pdf)
//! places the synchronization raster reference on subcarrier 120.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// Number of useful OFDM samples at 256 times the subcarrier spacing.
pub(super) const SYMBOL_SAMPLES: usize = 256;

const IDENTITIES: usize = 3;
const CHIPS: usize = 127;
const FIRST_BIN: usize = SYMBOL_SAMPLES - 64;
const FFT_SIZE: usize = 4_096;
const CORRELATION_DELAY: usize = SYMBOL_SAMPLES - 1;
const PEAK_RADIUS: usize = 16;
const STARTS_PER_BLOCK: usize = FFT_SIZE - CORRELATION_DELAY - 2 * PEAK_RADIUS;
const MAX_PEAKS: usize = 4_096;
const MIN_SCORE: f32 = 0.14;
// Try zero first so an exact score tie favors the unshifted hypothesis.
const CARRIER_OFFSETS: [f32; 3] = [0.0, -0.5, 0.5];

type TemplateSpectra = [[Vec<Complex32>; CARRIER_OFFSETS.len()]; IDENTITIES];
type PeakRank = (u32, Reverse<usize>, Reverse<u8>, Reverse<usize>);

/// One primary synchronization candidate at the first useful symbol sample.
#[derive(Debug, PartialEq)]
pub(super) struct Peak {
    pub(super) start: usize,
    pub(super) identity: u8,
    /// Squared normalized correlation, between zero and one.
    pub(super) score: f32,
    /// Coarse received carrier offset in signed subcarrier spacings.
    pub(super) carrier_offset: f32,
}

/// Retained primary synchronization candidates and the excluded count.
#[derive(Debug, PartialEq)]
pub(super) struct Search {
    pub(super) peaks: Vec<Peak>,
    /// Candidates above the threshold excluded by the retention cap.
    pub(super) omitted_peaks: usize,
}

/// Generate the PSS for N_ID^(2) in 0 through 2, beginning at relative bin -64.
pub(super) fn sequence(identity: u8) -> [f32; CHIPS] {
    let mut bits = [0_u8; CHIPS];
    bits[..7].copy_from_slice(&[0, 1, 1, 0, 1, 1, 1]);
    for index in 7..CHIPS {
        bits[index] = bits[index - 3] ^ bits[index - 7];
    }
    let shift = 43 * usize::from(identity);
    std::array::from_fn(|index| 1.0 - 2.0 * f32::from(bits[(index + shift) % CHIPS]))
}

/// Map a PSS or SSS chip index in 0 through 126 to a wrapped FFT bin.
pub(super) fn sync_bin(index: usize) -> usize {
    (FIRST_BIN + index) % SYMBOL_SAMPLES
}

fn template_spectra(
    fft: &dyn Fft<f32>,
    inverse_symbol: &dyn Fft<f32>,
    scratch: &mut [Complex32],
) -> TemplateSpectra {
    std::array::from_fn(|identity| {
        let chips = sequence(identity as u8);
        let mut symbol = [Complex32::default(); SYMBOL_SAMPLES];
        for (index, chip) in chips.into_iter().enumerate() {
            symbol[sync_bin(index)].re = chip;
        }
        inverse_symbol.process_with_scratch(&mut symbol, scratch);
        let unit_energy_scale = ((SYMBOL_SAMPLES * CHIPS) as f32).sqrt().recip();
        CARRIER_OFFSETS.map(|offset| {
            let mut kernel = vec![Complex32::default(); FFT_SIZE];
            for (index, &sample) in symbol.iter().enumerate() {
                let phase = TAU * offset * index as f32 / SYMBOL_SAMPLES as f32;
                let shifted = sample * Complex32::from_polar(unit_energy_scale, phase);
                kernel[CORRELATION_DELAY - index] = shifted.conj();
            }
            fft.process_with_scratch(&mut kernel, scratch);
            kernel
        })
    })
}

fn normalization(block: &[Complex32], weights: &mut [f32]) {
    let energy = |sample: &Complex32| f64::from(sample.norm_sqr());
    // Suppress correlation leakage into windows 100 dB below block energy.
    let floor = block.iter().map(energy).sum::<f64>() * 1e-10;
    let mut window_energy: f64 = block[..SYMBOL_SAMPLES].iter().map(energy).sum();
    let fft_scale = (FFT_SIZE * FFT_SIZE) as f64;
    for (index, weight) in weights.iter_mut().enumerate() {
        *weight = if window_energy > floor {
            (fft_scale * window_energy).recip() as f32
        } else {
            0.0
        };
        if let Some(next) = block.get(index + SYMBOL_SAMPLES) {
            window_energy += energy(next) - energy(&block[index]);
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

/// Matched spectra and bounded working buffers reusable across RF hypotheses.
pub(super) struct Searcher {
    fft: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    templates: TemplateSpectra,
    spectrum: Vec<Complex32>,
    product: Vec<Complex32>,
    weights: Vec<f32>,
    scores: Vec<f32>,
    hypotheses: Vec<usize>,
    retained: BinaryHeap<Reverse<PeakRank>>,
}

impl Searcher {
    /// Prepare the matched filters at zero and ±0.5 subcarrier spacing.
    pub(super) fn new() -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let inverse = planner.plan_fft_inverse(FFT_SIZE);
        let inverse_symbol = planner.plan_fft_inverse(SYMBOL_SAMPLES);
        let scratch_size = fft
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len())
            .max(inverse_symbol.get_inplace_scratch_len());
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
            hypotheses: vec![0; FFT_SIZE],
            retained: BinaryHeap::with_capacity(MAX_PEAKS),
        }
    }

    /// Return up to 4,096 strongest candidates in time and identity order.
    ///
    /// Input is centered on the synchronization reference and sampled at 256
    /// times the subcarrier spacing. Each identity has one winning carrier
    /// hypothesis at a given time. The caller confirms SSS and repetition.
    ///
    /// # Errors
    /// Returns an error when interrupted before or during the search.
    pub(super) fn search(&mut self, input: &[Complex32], stop: &AtomicBool) -> Result<Search> {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "NR primary synchronization search interrupted"
        );
        self.retained.clear();
        if input.len() < SYMBOL_SAMPLES {
            return Ok(Search {
                peaks: Vec::new(),
                omitted_peaks: 0,
            });
        }
        let windows = input.len() - CORRELATION_DELAY;
        let mut omitted_peaks = 0;
        let Self {
            fft,
            inverse,
            scratch,
            templates,
            spectrum,
            product,
            weights,
            scores,
            hypotheses,
            retained,
        } = self;
        for core_start in (0..windows).step_by(STARTS_PER_BLOCK) {
            ensure!(
                !stop.load(Ordering::Relaxed),
                "NR primary synchronization search interrupted"
            );
            let core_end = (core_start + STARTS_PER_BLOCK).min(windows);
            // Adjacent overlap-save blocks include the peak comparison window.
            let block_start = core_start.saturating_sub(PEAK_RADIUS);
            let block_end = (core_end + PEAK_RADIUS).min(windows);
            let block = &input[block_start..block_end + CORRELATION_DELAY];
            let count = block_end - block_start;
            let weights = &mut weights[..count];
            let scores = &mut scores[..count];
            let hypotheses = &mut hypotheses[..count];
            normalization(block, weights);
            spectrum[..block.len()].copy_from_slice(block);
            spectrum[block.len()..].fill(Complex32::default());
            fft.process_with_scratch(spectrum, scratch);
            for (identity, kernels) in templates.iter().enumerate() {
                scores.fill(0.0);
                for (hypothesis, kernel) in kernels.iter().enumerate() {
                    for ((output, &signal), &matched) in
                        product.iter_mut().zip(spectrum.iter()).zip(kernel)
                    {
                        *output = signal * matched;
                    }
                    inverse.process_with_scratch(product, scratch);
                    for (((score, winner), &weight), matched) in scores
                        .iter_mut()
                        .zip(hypotheses.iter_mut())
                        .zip(weights.iter())
                        .zip(&product[CORRELATION_DELAY..])
                    {
                        let value = matched.norm_sqr() * weight;
                        if value.is_finite() && value.min(1.0) > *score {
                            *score = value.min(1.0);
                            *winner = hypothesis;
                        }
                    }
                }
                for start in core_start..core_end {
                    let index = start - block_start;
                    let score = scores[index];
                    if score < MIN_SCORE || !is_local_peak(scores, index) {
                        continue;
                    }
                    // Positive finite scores sort by their bit patterns. Equal
                    // scores retain the earlier time and lower identity.
                    let rank = (
                        score.to_bits(),
                        Reverse(start),
                        Reverse(identity as u8),
                        Reverse(hypotheses[index]),
                    );
                    if retained.len() < MAX_PEAKS {
                        retained.push(Reverse(rank));
                    } else {
                        omitted_peaks += 1;
                        if let Some(mut weakest) = retained.peek_mut()
                            && rank > weakest.0
                        {
                            *weakest = Reverse(rank);
                        }
                    }
                }
            }
        }
        let mut peaks: Vec<_> = retained
            .drain()
            .map(
                |Reverse((score, Reverse(start), Reverse(identity), Reverse(hypothesis)))| Peak {
                    start,
                    identity,
                    score: f32::from_bits(score),
                    carrier_offset: CARRIER_OFFSETS[hypothesis],
                },
            )
            .collect();
        peaks.sort_unstable_by_key(|peak| (peak.start, peak.identity));
        Ok(Search {
            peaks,
            omitted_peaks,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::TAU as TAU64;

    use super::*;

    fn noise(length: usize) -> Vec<Complex32> {
        let mut state = 0xfa83_a721_u32;
        let mut component = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f32::from((state >> 16) as u16) / 32768.0 - 1.0) * 0.001
        };
        (0..length)
            .map(|_| Complex32::new(component(), component()))
            .collect()
    }

    fn add_symbol(output: &mut [Complex32], identity: u8, carrier: f64, gain: f64) {
        // Synthesize the waveform directly with a shift register and a sum of
        // phasors to keep the fixtures independent of the receiver FFTs.
        let mut register = 0b111_0110_u8;
        let mut next_chip = || {
            let bit = register & 1;
            let feedback = (register ^ (register >> 4)) & 1;
            register = (register >> 1) | (feedback << 6);
            1.0 - 2.0 * f64::from(bit)
        };
        for _ in 0..43 * usize::from(identity) {
            next_chip();
        }
        let chips: [f64; 127] = std::array::from_fn(|_| next_chip());
        let scale = gain / (256.0_f64 * 127.0).sqrt();
        for (index, sample) in output.iter_mut().enumerate() {
            for (subcarrier, chip) in (-64..=62).zip(chips) {
                let phase = TAU64 * (f64::from(subcarrier) + carrier) * index as f64 / 256.0;
                let (sin, cos) = (phase + 0.83).sin_cos();
                *sample += Complex32::new((scale * chip * cos) as f32, (scale * chip * sin) as f32);
            }
        }
    }

    #[test]
    fn finds_all_identities_at_overlap_boundaries_with_carrier_offsets() -> Result<()> {
        let mut signal = noise(2 * STARTS_PER_BLOCK + SYMBOL_SAMPLES);
        let placements = [
            (0, 0, -0.47, -0.5, 0.5),
            (STARTS_PER_BLOCK - 1, 1, 0.18, 0.0, 1.5),
            (2 * STARTS_PER_BLOCK, 2, 0.52, 0.5, 0.8),
        ];
        for (start, identity, carrier, _, gain) in placements {
            add_symbol(
                &mut signal[start..start + SYMBOL_SAMPLES],
                identity,
                carrier,
                gain,
            );
        }
        let stop = AtomicBool::new(false);
        let mut searcher = Searcher::new();
        let found = searcher.search(&signal, &stop)?;
        assert_eq!(found.omitted_peaks, 0);
        for (start, identity, _, expected_carrier, _) in placements {
            let best = found
                .peaks
                .iter()
                .filter(|peak| peak.start.abs_diff(start) <= PEAK_RADIUS)
                .max_by(|left, right| left.score.total_cmp(&right.score));
            assert!(
                best.is_some_and(|peak| peak.start == start
                    && peak.identity == identity
                    && peak.carrier_offset == expected_carrier
                    && peak.score > 0.85),
                "expected identity {identity} at sample {start}, found {best:?}"
            );
        }
        assert_eq!(searcher.search(&signal, &stop)?, found);
        Ok(())
    }

    #[test]
    fn rejects_short_zero_noise_and_tone_and_honors_cancellation() -> Result<()> {
        let mut searcher = Searcher::new();
        let stop = AtomicBool::new(false);
        let tone: Vec<_> = (0..4 * FFT_SIZE)
            .map(|index| Complex32::from_polar(1.0, TAU * index as f32 / 29.0))
            .collect();
        let inputs = [
            noise(4 * FFT_SIZE),
            noise(SYMBOL_SAMPLES - 1),
            vec![Complex32::default(); FFT_SIZE],
            tone,
        ];
        for input in &inputs {
            let found = searcher.search(input, &stop)?;
            assert!(found.peaks.is_empty());
            assert_eq!(found.omitted_peaks, 0);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(searcher.search(&inputs[0], &stop).is_err());
        Ok(())
    }

    #[test]
    fn reports_the_exact_count_excluded_by_capacity() -> Result<()> {
        let mut cycle = [Complex32::default(); 2 * SYMBOL_SAMPLES];
        let start = SYMBOL_SAMPLES / 2;
        add_symbol(&mut cycle[start..start + SYMBOL_SAMPLES], 0, 0.0, 1.0);
        let stop = AtomicBool::new(false);
        let mut searcher = Searcher::new();
        let found = searcher.search(&cycle.repeat(MAX_PEAKS + 3), &stop)?;
        assert_eq!(found.peaks.len(), MAX_PEAKS);
        assert_eq!(found.omitted_peaks, 3);
        assert!(found.peaks.iter().all(|peak| {
            peak.identity == 0 && peak.start % cycle.len() == start && peak.score > 0.99
        }));
        Ok(())
    }
}
