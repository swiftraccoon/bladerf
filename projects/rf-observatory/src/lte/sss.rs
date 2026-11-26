//! Match the LTE FDD secondary synchronization signal with a normal cyclic prefix.
//!
//! [ETSI TS 136 211 V18.0.1] clauses 6.11.2 and 6.12 define the sequences,
//! resource mapping and OFDM timing.
//!
//! [ETSI TS 136 211 V18.0.1]: https://www.etsi.org/deliver/etsi_ts/136200_136299/136211/18.00.01_60/ts_136211v180001p.pdf

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::pss::{self, SYMBOL_SAMPLES};

const BASE_SEQUENCE_LEN: usize = 31;
const SUBCARRIERS: usize = 2 * BASE_SEQUENCE_LEN;
const CELL_GROUPS: usize = 168;
const SUBFRAME_VARIANTS: usize = 2;
const NORMAL_CP_SAMPLES: usize = 9;
/// Separation of the two synchronization symbols on the normal-prefix grid.
pub(super) const SYMBOL_START_DELTA: usize = SYMBOL_SAMPLES + NORMAL_CP_SAMPLES;
const CHANNEL_POWER_FLOOR_FRACTION: f32 = 0.05;
const MIN_SCORE: f32 = 0.50;
const MIN_SCORE_GAP: f32 = 0.10;

type Signs = [i8; SUBCARRIERS];
type Templates = [[Signs; CELL_GROUPS * SUBFRAME_VARIANTS]; 3];

const S: [i8; BASE_SEQUENCE_LEN] = m_sequence(&[0, 2]);
const C: [i8; BASE_SEQUENCE_LEN] = m_sequence(&[0, 3]);
const Z: [i8; BASE_SEQUENCE_LEN] = m_sequence(&[0, 1, 2, 4]);
static TEMPLATES: Templates = templates();

const fn m_sequence(taps: &[usize]) -> [i8; BASE_SEQUENCE_LEN] {
    let mut signs = [1; BASE_SEQUENCE_LEN];
    signs[4] = -1;
    let mut index = 5;
    while index < signs.len() {
        // Products of bipolar signs implement binary addition modulo two.
        let mut tap = 0;
        while tap < taps.len() {
            signs[index] *= signs[index - 5 + taps[tap]];
            tap += 1;
        }
        index += 1;
    }
    signs
}

const fn sequence(group: usize, identity: usize, subframe_five: bool) -> Signs {
    let q_prime = group / 30;
    let q = (group + q_prime * (q_prime + 1) / 2) / 30;
    let m_prime = group + q * (q + 1) / 2;
    let m0 = m_prime % BASE_SEQUENCE_LEN;
    let m1 = (m0 + m_prime / BASE_SEQUENCE_LEN + 1) % BASE_SEQUENCE_LEN;
    let (even_shift, odd_shift) = if subframe_five { (m1, m0) } else { (m0, m1) };

    let mut signs = [0; SUBCARRIERS];
    let mut index = 0;
    while index < BASE_SEQUENCE_LEN {
        signs[2 * index] =
            S[(index + even_shift) % BASE_SEQUENCE_LEN] * C[(index + identity) % BASE_SEQUENCE_LEN];
        signs[2 * index + 1] = S[(index + odd_shift) % BASE_SEQUENCE_LEN]
            * C[(index + identity + 3) % BASE_SEQUENCE_LEN]
            * Z[(index + even_shift % 8) % BASE_SEQUENCE_LEN];
        index += 1;
    }
    signs
}

const fn templates() -> Templates {
    let mut result = [[[0; SUBCARRIERS]; CELL_GROUPS * SUBFRAME_VARIANTS]; 3];
    let mut identity = 0;
    while identity < result.len() {
        let mut group = 0;
        while group < CELL_GROUPS {
            result[identity][2 * group] = sequence(group, identity, false);
            result[identity][2 * group + 1] = sequence(group, identity, true);
            group += 1;
        }
        identity += 1;
    }
    result
}

/// One cell identity and its position within a radio frame.
pub(super) struct Match {
    pub(super) cell_id: u16,
    pub(super) subframe: u8,
    /// Normalized coherent correlation amplitude in `0.0..=1.0`.
    pub(super) score: f32,
}

/// Reuse the OFDM transform and scratch buffer across PSS peaks.
pub(super) struct Matcher {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl Matcher {
    pub(super) fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(SYMBOL_SAMPLES);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self { fft, scratch }
    }

    /// Match the SSS preceding a PSS useful-symbol start at 1.92 MSps.
    /// Reject incomplete symbols, weak correlation, and competing matches.
    pub(super) fn detect(&mut self, input: &[Complex32], peak: &pss::Peak) -> Option<Match> {
        let templates = TEMPLATES.get(usize::from(peak.identity))?;
        let mut primary = *input.get(peak.start..)?.first_chunk::<SYMBOL_SAMPLES>()?;
        let secondary_start = peak.start.checked_sub(SYMBOL_START_DELTA)?;
        let mut secondary = *input
            .get(secondary_start..)?
            .first_chunk::<SYMBOL_SAMPLES>()?;
        self.fft
            .process_with_scratch(&mut primary, &mut self.scratch);
        self.fft
            .process_with_scratch(&mut secondary, &mut self.scratch);

        let reference = pss::sequence(peak.identity);
        let channel: [Complex32; SUBCARRIERS] =
            std::array::from_fn(|index| primary[sync_bin(index)] * reference[index].conj());
        let mean_channel_power =
            channel.iter().map(Complex32::norm_sqr).sum::<f32>() / SUBCARRIERS as f32;
        if !mean_channel_power.is_finite() || mean_channel_power <= 0.0 {
            return None;
        }

        // Conjugate channel compensation preserves coherence across tones.
        // The floor limits the influence of deep fades and weak PSS estimates.
        let power_floor = mean_channel_power * CHANNEL_POWER_FLOOR_FRACTION;
        let mut template_energy = 0.0;
        let mut signal_energy = 0.0;
        let compensated: [Complex32; SUBCARRIERS] = std::array::from_fn(|index| {
            let estimate = channel[index];
            let received = secondary[sync_bin(index)];
            let channel_power = estimate.norm_sqr();
            let weight = (channel_power + power_floor).recip();
            template_energy += channel_power * weight;
            signal_energy += received.norm_sqr() * weight;
            received * estimate.conj() * weight
        });
        let normalizer = (template_energy * signal_energy).sqrt();
        if !normalizer.is_finite() || normalizer <= 0.0 {
            return None;
        }

        let mut best = 0.0;
        let mut runner_up = 0.0;
        let mut best_index = 0;
        for (index, candidate) in templates.iter().enumerate() {
            let correlation: Complex32 = compensated
                .iter()
                .zip(candidate)
                .map(|(&sample, &sign)| sample * f32::from(sign))
                .sum();
            let power = correlation.norm_sqr();
            if power > best {
                runner_up = best;
                best = power;
                best_index = index;
            } else if power > runner_up {
                runner_up = power;
            }
        }
        let score = (best.sqrt() / normalizer).min(1.0);
        if score < MIN_SCORE || score - runner_up.sqrt() / normalizer < MIN_SCORE_GAP {
            return None;
        }
        Some(Match {
            cell_id: (3 * (best_index / 2)) as u16 + u16::from(peak.identity),
            subframe: if best_index.is_multiple_of(2) { 0 } else { 5 },
            score,
        })
    }
}

// The 62 tones straddle the unused DC bin in the FFT output.
fn sync_bin(index: usize) -> usize {
    if index < BASE_SEQUENCE_LEN {
        SYMBOL_SAMPLES - BASE_SEQUENCE_LEN + index
    } else {
        index - BASE_SEQUENCE_LEN + 1
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;

    // Independently encoded with a shift register and (m0, m1) from Table
    // 6.11.2.1-1. A set bit marks a negative sign, starting at the lowest tone.
    const GROUP_167_SUBFRAME_0: u64 = 0x2ab5_a91f_1fb2_b722;
    const GROUP_167_SUBFRAME_5: u64 = 0x2d1c_dc1a_af5a_0c3d;
    const GROUP_59_SUBFRAME_0: u64 = 0x1819_ee50_3ecf_1290;

    fn signs(mask: u64) -> [Complex32; SUBCARRIERS] {
        std::array::from_fn(|index| {
            Complex32::new(if mask & (1 << index) == 0 { 1.0 } else { -1.0 }, 0.0)
        })
    }

    fn symbol(tones: [Complex32; SUBCARRIERS], phase: f32) -> [Complex32; SYMBOL_SAMPLES] {
        std::array::from_fn(|sample| {
            (-31..0)
                .chain(1..=31)
                .zip(tones)
                .map(|(tone, value)| {
                    let radians_per_sample = TAU * tone as f32 / SYMBOL_SAMPLES as f32;
                    // Two paths give frequency-dependent phase, attenuation, and fades.
                    let channel = Complex32::from_polar(0.65, 0.31)
                        + Complex32::from_polar(0.61, -radians_per_sample * 5.0);
                    value
                        * channel
                        * Complex32::from_polar(1.0, radians_per_sample * sample as f32 + phase)
                })
                .sum()
        })
    }

    fn waveform(sss: [Complex32; SUBCARRIERS]) -> (Vec<Complex32>, pss::Peak) {
        let mut input = vec![Complex32::default(); 17];
        input.extend(symbol(sss, 1.10));
        let pss = symbol(pss::sequence(2), -0.74);
        input.extend(&pss[SYMBOL_SAMPLES - NORMAL_CP_SAMPLES..]);
        let peak = pss::Peak {
            start: input.len(),
            identity: 2,
            score: 1.0,
        };
        input.extend(pss);
        (input, peak)
    }

    #[test]
    fn recognizes_both_subframes_under_frequency_selective_fading() {
        let mut matcher = Matcher::new();
        for (expected_subframe, expected_signs) in
            [(0, GROUP_167_SUBFRAME_0), (5, GROUP_167_SUBFRAME_5)]
        {
            let (input, peak) = waveform(signs(expected_signs));
            let found = matcher.detect(&input, &peak).unwrap();
            assert_eq!(found.cell_id, 503);
            assert_eq!(found.subframe, expected_subframe);
            assert!(found.score > 0.99);
        }
    }

    #[test]
    fn rejects_ambiguous_absent_and_incomplete_symbols() {
        let mut matcher = Matcher::new();
        let primary_candidate = signs(GROUP_167_SUBFRAME_0);
        let other_candidate = signs(GROUP_59_SUBFRAME_0);
        let mixed = std::array::from_fn(|index| primary_candidate[index] + other_candidate[index]);
        let (input, peak) = waveform(mixed);
        assert!(matcher.detect(&input, &peak).is_none());

        let (absent, peak) = waveform([Complex32::default(); SUBCARRIERS]);
        assert!(matcher.detect(&absent, &peak).is_none());
        let (complete, peak) = waveform(primary_candidate);
        assert!(
            matcher
                .detect(&complete[..complete.len() - 1], &peak)
                .is_none()
        );
    }
}
