//! Extract the NR physical broadcast channel from a synchronized FR1 block.
//!
//! [ETSI TS 138 211 V17.8.0] clauses 5.2.1, 5.3.1, 7.3.3, 7.4.1.4 and
//! 7.4.3 define the Gold sequence, OFDM timing, scrambling and resource mapping.
//!
//! [ETSI TS 138 211 V17.8.0]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138211/17.08.00_60/ts_138211v170800p.pdf

use std::f32::consts::FRAC_1_SQRT_2;
use std::f64::consts::TAU;
use std::ops::Range;

use rustfft::num_complex::Complex32;

use crate::SAMPLE_RATE;
use crate::dsp::{Iq16, gold};

const SUBCARRIERS: usize = 240;
const REFERENCE_SUBCARRIER: i32 = 120;
const SYMBOLS: usize = 3;
const CODED_BITS: usize = 864;
const SHORT_PREFIX_RATIO: f64 = 18.0 / 256.0;
const WINDOW_ADVANCE_INPUT_SAMPLES: f64 = 8.0;
const PILOT_STRIDE: usize = 4;
const MIN_PILOT_SCORE: f32 = 0.35;
const MIN_PILOT_SCORE_GAP: f32 = 0.10;
// Symbol indices start at the first symbol following PSS.
const REGIONS: [(usize, Range<usize>); 4] = [
    (0, 0..SUBCARRIERS),
    (1, 0..48),
    (1, 192..SUBCARRIERS),
    (2, 0..SUBCARRIERS),
];

type ToneGrid = [[Complex32; SUBCARRIERS]; SYMBOLS];

/// One FR1 interpretation of the detected reference sequence.
pub(super) struct Hypothesis {
    /// DM-RS sequence index before interpreting the half-frame bit.
    pub(super) dmrs_index: u8,
    /// Assumed maximum number of candidate SS/PBCH blocks per half-frame.
    pub(super) l_max: u8,
    /// Normalized coherence between adjacent reference tones.
    pub(super) score: f32,
    /// Descrambled coded-bit metrics, with positive values favoring zero.
    pub(super) llrs: [f32; CODED_BITS],
}

fn bipolar(bit: u8) -> f32 {
    1.0 - 2.0 * f32::from(bit)
}

fn tones(iq: &[Iq16], center_hz: f64, scs_hz: u32, pss_start: f64) -> Option<ToneGrid> {
    let sample_rate = f64::from(SAMPLE_RATE);
    let duration = sample_rate / f64::from(scs_hz);
    // The three prefixes following PSS in FR1 Cases A, B and C are short.
    let symbol_step = duration * (1.0 + SHORT_PREFIX_RATIO);
    let first_start = pss_start + symbol_step - WINDOW_ADVANCE_INPUT_SAMPLES;
    let advance_radians = -TAU / sample_rate;
    let mixers: [Complex32; SUBCARRIERS] = std::array::from_fn(|subcarrier| {
        let relative_bin = subcarrier as i32 - REFERENCE_SUBCARRIER;
        let frequency = center_hz + f64::from(scs_hz) * f64::from(relative_bin);
        Complex32::from_polar(1.0, (advance_radians * frequency) as f32)
    });
    let mut grid = [[Complex32::default(); SUBCARRIERS]; SYMBOLS];
    for (symbol, bins) in grid.iter_mut().enumerate() {
        // A window inside the cyclic prefix tolerates timing error. Its
        // frequency-dependent phase is estimated by the same-window DM-RS.
        let start = first_start + symbol_step * symbol as f64;
        let end = start + duration;
        let first = start.floor() as usize;
        let last = end.ceil() as usize;
        let samples = iq.get(first..last)?;
        let mut weighted: Vec<_> = samples
            .iter()
            .map(|sample| Complex32::new(f32::from(sample.i), f32::from(sample.q)))
            .collect();
        // Weight the boundary samples by the fractional overlap of one
        // useful OFDM interval with the original input sample grid.
        *weighted.first_mut()? *= (first as f64 + 1.0 - start) as f32;
        *weighted.last_mut()? *= (end - (last - 1) as f64) as f32;
        for (bin, step) in bins.iter_mut().zip(mixers) {
            let mut oscillator = Complex32::new(1.0, 0.0);
            let mut sum = Complex32::default();
            for &sample in &weighted {
                sum += oscillator * sample;
                oscillator *= step;
            }
            *bin = sum / duration as f32;
        }
    }
    Some(grid)
}

fn pilot_channels(grid: &ToneGrid, pci: u16, index: u8) -> ToneGrid {
    let pci = u32::from(pci);
    let ssb = u32::from(index) + 1;
    let seed = (2048 * (pci / 4 + 1) + 64) * ssb + pci % 4;
    let mut sequence = gold(seed);
    let shift = (pci % 4) as usize;
    let mut channels = [[Complex32::default(); SUBCARRIERS]; SYMBOLS];
    for (symbol, region) in REGIONS {
        for subcarrier in (region.start + shift..region.end).step_by(PILOT_STRIDE) {
            let reference =
                Complex32::new(bipolar(sequence.bit()), bipolar(sequence.bit())) * FRAC_1_SQRT_2;
            channels[symbol][subcarrier] = grid[symbol][subcarrier] * reference.conj();
        }
    }
    channels
}

fn pilot_coherence(channels: &ToneGrid, shift: usize) -> f32 {
    let mut correlation = Complex32::default();
    let mut left_power = 0.0;
    let mut right_power = 0.0;
    for (symbol, region) in REGIONS {
        for bin in (region.start + shift + PILOT_STRIDE..region.end).step_by(PILOT_STRIDE) {
            let left = channels[symbol][bin - PILOT_STRIDE];
            let right = channels[symbol][bin];
            correlation += left.conj() * right;
            left_power += left.norm_sqr();
            right_power += right.norm_sqr();
        }
    }
    let power = (left_power * right_power).sqrt();
    if power > 0.0 {
        (correlation.norm() / power).min(1.0)
    } else {
        0.0
    }
}

fn soft_bits(grid: &ToneGrid, channels: &ToneGrid, shift: usize) -> [f32; CODED_BITS] {
    let mut llrs = [0.0; CODED_BITS];
    let mut bit = 0;
    for (symbol, region) in REGIONS {
        let first_pilot = region.start + shift;
        let last_pilot = region.end - PILOT_STRIDE + shift;
        for subcarrier in region {
            if subcarrier % PILOT_STRIDE == shift {
                continue;
            }
            let clipped = subcarrier.clamp(first_pilot, last_pilot);
            let left = clipped - (clipped - first_pilot) % PILOT_STRIDE;
            let right = (left + PILOT_STRIDE).min(last_pilot);
            let fraction = (clipped - left) as f32 / PILOT_STRIDE as f32;
            let channel =
                channels[symbol][left] * (1.0 - fraction) + channels[symbol][right] * fraction;
            let equalized = grid[symbol][subcarrier] * channel.conj();
            llrs[bit..bit + 2].copy_from_slice(&[equalized.re, equalized.im]);
            bit += 2;
        }
    }
    debug_assert_eq!(bit, CODED_BITS);
    llrs
}

/// Test all reference sequences in three complete PBCH symbols.
///
/// The input contains native 20 MSps Q11 samples. The PSS start is the
/// first useful sample on that grid. Frequency offsets are relative to
/// the capture center and to the PSS tuning frequency, respectively.
/// Incomplete or weak blocks produce no hypotheses.
pub(super) fn hypotheses(
    iq: &[Iq16],
    input_offset_hz: f64,
    scs_hz: u32,
    pss_start_input_sample: f64,
    carrier_offset_hz: f32,
    pci: u16,
) -> Vec<Hypothesis> {
    let center_hz = input_offset_hz + f64::from(carrier_offset_hz);
    if !matches!(scs_hz, 15_000 | 30_000)
        || pci > 1007
        || !pss_start_input_sample.is_finite()
        || pss_start_input_sample < 0.0
        || !center_hz.is_finite()
        || center_hz.abs() + f64::from(scs_hz) * f64::from(REFERENCE_SUBCARRIER)
            >= f64::from(SAMPLE_RATE) / 2.0
    {
        return Vec::new();
    }
    let Some(grid) = tones(iq, center_hz, scs_hz, pss_start_input_sample) else {
        return Vec::new();
    };

    let shift = usize::from(pci) % PILOT_STRIDE;
    let mut ranked: [_; 8] = std::array::from_fn(|index| {
        let index = index as u8;
        let channels = pilot_channels(&grid, pci, index);
        (index, pilot_coherence(&channels, shift))
    });
    ranked.sort_unstable_by(|(_, left), (_, right)| right.total_cmp(left));
    let [(dmrs_index, score), (_, runner_up), ..] = ranked;
    if score < MIN_PILOT_SCORE || score - runner_up < MIN_PILOT_SCORE_GAP {
        return Vec::new();
    }

    let bits = soft_bits(&grid, &pilot_channels(&grid, pci, dmrs_index), shift);
    // When L_max is four, the upper DM-RS index bit is the half-frame flag.
    [4, 8]
        .into_iter()
        .map(|l_max| {
            let offset = usize::from(dmrs_index % l_max) * CODED_BITS;
            let mut llrs = bits;
            for (value, sign) in llrs.iter_mut().zip(gold(u32::from(pci)).skip(offset)) {
                *value *= bipolar(sign);
            }
            Hypothesis {
                dmrs_index,
                l_max,
                score,
                llrs,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nr::pbch::{payload, polar};
    use rustfft::num_complex::Complex64;
    use serde_json::json;

    fn reference_sequence(seed: u32, count: usize) -> Vec<u8> {
        let mut x1 = vec![0; 1600 + count + 31];
        let mut x2 = x1.clone();
        x1[0] = 1;
        for (index, bit) in x2[..31].iter_mut().enumerate() {
            *bit = ((seed >> index) & 1) as u8;
        }
        for index in 0..1600 + count {
            x1[index + 31] = (x1[index + 3] + x1[index]) % 2;
            x2[index + 31] = (x2[index + 3] + x2[index + 2] + x2[index + 1] + x2[index]) % 2;
        }
        x1[1600..1600 + count]
            .iter()
            .zip(&x2[1600..1600 + count])
            .map(|(first, second)| (first + second) % 2)
            .collect()
    }

    fn random_word(state: &mut u32) -> u32 {
        *state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *state
    }

    fn fixture(scs_hz: u32, tuned_hz: f64, l_max: u8, data: &[u8; CODED_BITS]) -> Vec<Iq16> {
        // PCI 321 and DM-RS index 7 give scrambling subsequences 3 and 7
        // for L_max 4 and 8, respectively.
        let pilot_bits = reference_sequence(2048 * 8 * 81 + 64 * 8 + 1, 288);
        let scrambler = reference_sequence(321, 8 * 864);
        let mut pilot = pilot_bits.as_chunks::<2>().0.iter();
        let scramble_start = if l_max == 4 { 3 * 864 } else { 7 * 864 };
        let mut coded = data
            .iter()
            .zip(&scrambler[scramble_start..])
            .map(|(&bit, &mask)| bit ^ mask);
        let duration = 20_000_000.0 / f64::from(scs_hz);
        let prefix = duration * 144.0 / 2048.0;
        let first_pss = 89.4;
        let mut input = vec![Iq16::default(); (first_pss + 4.0 * (duration + prefix)) as usize];
        for (symbol, phase) in [(1, 0.21), (2, 1.39), (3, -0.83)] {
            let start = first_pss + f64::from(symbol) * (duration + prefix);
            let mut values = Vec::new();
            for k in 0..240 {
                if symbol == 2 && (48..192).contains(&k) {
                    continue;
                }
                let (re, im) = if k % 4 == 1 {
                    let bits = pilot.next().unwrap();
                    (bits[0], bits[1])
                } else {
                    (coded.next().unwrap(), coded.next().unwrap())
                };
                let slope = TAU * f64::from(k - 120) / duration;
                let channel =
                    Complex64::new(0.73, 0.12) + Complex64::from_polar(0.41, -slope * 11.5);
                let qpsk = Complex64::new(0.5 - f64::from(re), 0.5 - f64::from(im));
                values.push((slope, 12.0 * qpsk * channel));
            }
            let from = (start - prefix).ceil() as usize;
            let until = (start + duration).ceil() as usize;
            for (n, sample) in input.iter_mut().enumerate().take(until).skip(from) {
                let time = n as f64;
                let baseband: Complex64 = values
                    .iter()
                    .map(|&(slope, tone)| tone * Complex64::from_polar(1.0, slope * (time - start)))
                    .sum();
                let carrier =
                    Complex64::from_polar(1.0, TAU * tuned_hz * time / 20_000_000.0 + phase);
                let value = baseband * carrier;
                *sample = Iq16 {
                    i: value.re.round() as i16,
                    q: value.im.round() as i16,
                };
            }
        }
        assert!(pilot.next().is_none());
        assert!(coded.next().is_none());
        input
    }

    #[test]
    fn recovers_mib_from_both_fr1_grids() {
        let carrier_offset = 834.5;
        let data =
            std::array::from_fn(|bit| (polar::tests::CODEWORD[bit / 8] >> (7 - bit % 8)) & 1);
        for (spacing, tuned, l_max) in [(15_000, -6_230_000.0, 4), (30_000, 5_622_500.0, 8)] {
            let input = fixture(spacing, tuned + f64::from(carrier_offset), l_max, &data);
            let candidates = hypotheses(&input, tuned, spacing, 91.7, carrier_offset, 321);
            assert_eq!(candidates.len(), 2);
            for candidate in candidates {
                assert_eq!(candidate.dmrs_index, 7);
                assert!(candidate.score > 0.90);
                let mismatches = candidate
                    .llrs
                    .iter()
                    .zip(&data)
                    .filter(|&(&value, &bit)| (value < 0.0) != (bit == 1))
                    .count();
                if candidate.l_max == l_max {
                    assert_eq!(mismatches, 0);
                    let bits = polar::decode(&candidate.llrs).unwrap();
                    let mib = payload::decode(bits, 321, l_max, candidate.dmrs_index).unwrap();
                    assert_eq!(
                        serde_json::to_value(mib).unwrap(),
                        json!({
                            "system_frame_number": 675,
                            "half_frame": 1,
                            "ssb_index": 7 % l_max,
                            "common_subcarrier_spacing_hz": 30_000,
                            "k_ssb": 11,
                            "dmrs_type_a_position": 2,
                            "pdcch_config_sib1": {"control_resource_set_zero": 5, "search_space_zero": 11},
                            "cell_barred": false,
                            "intra_freq_reselection_allowed": false
                        })
                    );
                } else {
                    assert!(mismatches > 350);
                    assert!(polar::decode(&candidate.llrs).is_none());
                }
            }
        }
    }

    #[test]
    fn rejects_noise_incomplete_symbols_and_aliased_frequencies() {
        let mut state = 559;
        let noise: Vec<_> = (0..6000)
            .map(|_| Iq16 {
                i: (random_word(&mut state) >> 24) as i16 - 128,
                q: (random_word(&mut state) >> 24) as i16 - 128,
            })
            .collect();
        assert!(hypotheses(&noise, 0.0, 15_000, 110.0, 0.0, 3).is_empty());
        assert!(hypotheses(&noise[..noise.len() / 2], 0.0, 15_000, 91.7, 0.0, 511).is_empty());
        assert!(hypotheses(&noise, 8_300_000.0, 15_000, 91.7, 0.0, 511).is_empty());
    }
}
