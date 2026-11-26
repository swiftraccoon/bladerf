//! Identify NR secondary synchronization symbols on the 256-bin FR1 grid.
//!
//! [ETSI TS 138 211 V17.8.0] clauses 5.3.1, 7.4.2 and 7.4.3 define the
//! prefix lengths, sequences and resource mapping. [ETSI TS 138 213 V17.8.0]
//! clause 4.1 gives the FR1 SS/PBCH locations for Cases A, B and C.
//!
//! [ETSI TS 138 211 V17.8.0]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138211/17.08.00_60/ts_138211v170800p.pdf
//! [ETSI TS 138 213 V17.8.0]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138213/17.08.00_60/ts_138213v170800p.pdf

use std::f32::consts::TAU;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use super::pss::{self, SYMBOL_SAMPLES, sync_bin};

const SUBCARRIERS: usize = 127;
const CELL_GROUPS: usize = 336;
const SHORT_CP_SAMPLES: usize = 18;
// Both prefixes between symbols 0 and 2 of Cases A, B and C are short.
const SYMBOL_START_DELTA: usize = 2 * (SYMBOL_SAMPLES + SHORT_CP_SAMPLES);
const PREFIX_TAIL_SAMPLES: usize = 8;
const MIN_PREFIX_COHERENCE: f32 = 0.45;
const MAX_CARRIER_REFINEMENT: f32 = 0.30;
const CHANNEL_POWER_FLOOR_FRACTION: f32 = 0.05;
const MIN_SCORE: f32 = 0.50;
const MIN_SCORE_GAP: f32 = 0.08;

type Signs = [i8; SUBCARRIERS];

const X0: Signs = m_sequence(4);
const X1: Signs = m_sequence(1);
static TEMPLATES: [[Signs; CELL_GROUPS]; 3] = templates();

const fn m_sequence(tap: usize) -> Signs {
    let mut signs = [1; SUBCARRIERS];
    signs[0] = -1;
    let mut index = 7;
    while index < SUBCARRIERS {
        // Multiplication in bipolar representation is binary addition modulo two.
        signs[index] = signs[index - 7] * signs[index - 7 + tap];
        index += 1;
    }
    signs
}

const fn templates() -> [[Signs; CELL_GROUPS]; 3] {
    let mut result = [[[0; SUBCARRIERS]; CELL_GROUPS]; 3];
    let mut identity = 0;
    while identity < result.len() {
        let mut group = 0;
        while group < CELL_GROUPS {
            let m0 = 15 * (group / 112) + 5 * identity;
            let m1 = group % 112;
            let mut tone = 0;
            while tone < SUBCARRIERS {
                result[identity][group][tone] =
                    X0[(tone + m0) % SUBCARRIERS] * X1[(tone + m1) % SUBCARRIERS];
                tone += 1;
            }
            group += 1;
        }
        identity += 1;
    }
    result
}

fn refine_carrier(input: &[Complex32], starts: [usize; 2], coarse: f32) -> f32 {
    let mut correlation = Complex32::default();
    let mut prefix_power = 0.0;
    let mut tail_power = 0.0;
    for start in starts {
        // The last prefix samples have more margin for multipath and timing
        // error than the samples at the leading edge of the prefix.
        let Some(prefix_start) = start.checked_sub(PREFIX_TAIL_SAMPLES) else {
            continue;
        };
        // The matcher has already admitted the two complete useful symbols.
        let prefix = &input[prefix_start..start];
        let tail = &input[prefix_start + SYMBOL_SAMPLES..start + SYMBOL_SAMPLES];
        for (&prefix, &tail) in prefix.iter().zip(tail) {
            correlation += prefix.conj() * tail;
            prefix_power += prefix.norm_sqr();
            tail_power += tail.norm_sqr();
        }
    }
    let energy = (prefix_power * tail_power).sqrt();
    if energy <= 0.0 || correlation.norm() < MIN_PREFIX_COHERENCE * energy {
        return coarse;
    }
    // A one-symbol separation measures fractional carrier modulo one tone.
    let wrapped = correlation.arg() / TAU;
    let refined = wrapped + (coarse - wrapped).round();
    if (refined - coarse).abs() <= MAX_CARRIER_REFINEMENT {
        refined
    } else {
        coarse
    }
}

/// Physical cell identity supported by one matching SSS symbol.
pub(super) struct Match {
    pub(super) cell_id: u16,
    /// Normalized coherent correlation amplitude in `0.0..=1.0`.
    pub(super) score: f32,
    /// Signed residual carrier displacement in units of subcarrier spacing.
    pub(super) carrier_offset: f32,
}

/// Reuse a forward transform and its scratch buffer across PSS candidates.
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

    /// Match the SSS after a useful-symbol PSS start on the FR1 grid.
    /// Returns `None` for incomplete symbols, weak matches or ambiguous matches.
    pub(super) fn detect(&mut self, input: &[Complex32], peak: &pss::Peak) -> Option<Match> {
        let templates = TEMPLATES.get(usize::from(peak.identity))?;
        let secondary_start = peak.start.checked_add(SYMBOL_START_DELTA)?;
        let mut primary = *input.get(peak.start..)?.first_chunk::<SYMBOL_SAMPLES>()?;
        let mut secondary = *input
            .get(secondary_start..)?
            .first_chunk::<SYMBOL_SAMPLES>()?;
        if !peak.carrier_offset.is_finite() {
            return None;
        }
        let carrier_offset =
            refine_carrier(input, [peak.start, secondary_start], peak.carrier_offset);
        let phase_step = Complex32::from_polar(1.0, -TAU * carrier_offset / SYMBOL_SAMPLES as f32);
        for symbol in [&mut primary, &mut secondary] {
            let mut phase = Complex32::new(1.0, 0.0);
            for sample in symbol.iter_mut() {
                *sample *= phase;
                phase *= phase_step;
            }
            self.fft.process_with_scratch(symbol, &mut self.scratch);
        }

        let reference = pss::sequence(peak.identity);
        let channel: [Complex32; SUBCARRIERS] =
            std::array::from_fn(|tone| primary[sync_bin(tone)] * reference[tone]);
        let mean_channel_power =
            channel.iter().map(Complex32::norm_sqr).sum::<f32>() / SUBCARRIERS as f32;
        if !mean_channel_power.is_finite() || mean_channel_power <= 0.0 {
            return None;
        }

        // Conjugate channel compensation aligns the tones. A regularization
        // floor keeps weak estimates from amplifying deep fades and noise.
        let floor = mean_channel_power * CHANNEL_POWER_FLOOR_FRACTION;
        let mut template_energy = 0.0;
        let mut signal_energy = 0.0;
        let compensated: [Complex32; SUBCARRIERS] = std::array::from_fn(|tone| {
            let estimate = channel[tone];
            let received = secondary[sync_bin(tone)];
            let power = estimate.norm_sqr();
            let weight = (power + floor).recip();
            template_energy += power * weight;
            signal_energy += received.norm_sqr() * weight;
            received * estimate.conj() * weight
        });
        let normalizer = (template_energy * signal_energy).sqrt();
        if !normalizer.is_finite() || normalizer <= 0.0 {
            return None;
        }

        let mut best_power = 0.0;
        let mut runner_up_power = 0.0;
        let mut group = 0;
        for (index, template) in templates.iter().enumerate() {
            let correlation: Complex32 = compensated
                .iter()
                .zip(template)
                .map(|(&sample, &sign)| sample * f32::from(sign))
                .sum();
            let power = correlation.norm_sqr();
            if power > best_power {
                runner_up_power = best_power;
                best_power = power;
                group = index;
            } else if power > runner_up_power {
                runner_up_power = power;
            }
        }
        let score = (best_power.sqrt() / normalizer).min(1.0);
        if score < MIN_SCORE || score - runner_up_power.sqrt() / normalizer < MIN_SCORE_GAP {
            return None;
        }
        Some(Match {
            cell_id: (3 * group) as u16 + u16::from(peak.identity),
            score,
            carrier_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Packed signs from an independent binary shift register. A set bit marks
    // a negative sign, starting at the lowest occupied subcarrier.
    const PSS_0: u128 = 0x42af_a51b_8fe1_de59_2044_c5d6_c19a_9cf6;
    const PSS_2: u128 = 0x43bc_b240_898b_ad83_3539_ed0a_be94_6e3f;
    const SSS_0: u128 = 0x7722_3051_f3cf_5b42_5fe8_c057_1951_0400;
    const SSS_200: u128 = 0x0c0d_f726_7308_426b_7d05_c26f_3e5c_73cb;
    const SSS_1007: u128 = 0x0211_7a44_f6c1_6ad1_2d5d_a7c5_6a3f_693d;

    fn signs(mask: u128) -> [f32; SUBCARRIERS] {
        std::array::from_fn(|tone| if mask & (1 << tone) == 0 { 1.0 } else { -1.0 })
    }

    fn symbol(chips: [f32; SUBCARRIERS], phase: f32) -> [Complex32; SYMBOL_SAMPLES] {
        std::array::from_fn(|sample| {
            (-64..=62)
                .zip(chips)
                .map(|(carrier, chip)| {
                    // A delayed path introduces frequency-dependent amplitude
                    // and phase even with an otherwise noiseless fixture.
                    let slope = TAU * carrier as f32 / SYMBOL_SAMPLES as f32;
                    let channel = Complex32::from_polar(0.65, 0.31)
                        + Complex32::from_polar(0.61, -slope * 5.0);
                    chip * channel * Complex32::from_polar(1.0, slope * sample as f32 + phase)
                })
                .sum()
        })
    }

    fn waveform(
        pss: u128,
        identity: u8,
        sss: [f32; SUBCARRIERS],
        offset: f32,
        coarse: f32,
    ) -> (Vec<Complex32>, pss::Peak) {
        let primary = symbol(signs(pss), 0.47);
        let secondary = symbol(sss, -0.93);
        let mut input = vec![Complex32::default(); 13];
        input.extend(&primary[SYMBOL_SAMPLES - SHORT_CP_SAMPLES..]);
        let start = input.len();
        input.extend(primary);
        input.resize(
            start + SYMBOL_START_DELTA - SHORT_CP_SAMPLES,
            Complex32::default(),
        );
        input.extend(&secondary[SYMBOL_SAMPLES - SHORT_CP_SAMPLES..]);
        input.extend(secondary);
        for (index, value) in input.iter_mut().enumerate() {
            *value *=
                Complex32::from_polar(1.0, TAU * offset * index as f32 / SYMBOL_SAMPLES as f32);
        }
        (
            input,
            pss::Peak {
                start,
                identity,
                score: 1.0,
                carrier_offset: coarse,
            },
        )
    }

    #[test]
    fn recognizes_cell_group_edges_and_refines_wrapped_carrier() {
        let mut matcher = Matcher::new();
        for (pss, identity, sss, cell_id, offset, coarse) in [
            (PSS_0, 0, SSS_0, 0, -0.34, -0.5),
            (PSS_2, 2, SSS_1007, 1007, 0.58, 0.5),
        ] {
            let (input, peak) = waveform(pss, identity, signs(sss), offset, coarse);
            let found = matcher.detect(&input, &peak).unwrap();
            assert_eq!(found.cell_id, cell_id);
            assert!(found.score > 0.99);
            assert!((found.carrier_offset - offset).abs() < 1e-4);
        }
    }

    #[test]
    fn rejects_ambiguous_absent_and_incomplete_secondary_symbols() {
        let mut matcher = Matcher::new();
        let high = signs(SSS_1007);
        let other = signs(SSS_200);
        let ambiguous = std::array::from_fn(|tone| high[tone] + other[tone]);
        for chips in [ambiguous, [0.0; SUBCARRIERS]] {
            let (input, peak) = waveform(PSS_2, 2, chips, 0.0, 0.0);
            assert!(matcher.detect(&input, &peak).is_none());
        }
        let (input, peak) = waveform(PSS_2, 2, high, 0.0, 0.0);
        assert!(matcher.detect(&input[..input.len() - 1], &peak).is_none());
    }
}
