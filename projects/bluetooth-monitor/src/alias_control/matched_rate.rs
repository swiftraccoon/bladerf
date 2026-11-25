//! Bandwidth-matched 4 MSps and 2 MSps views of one retained CF32 window.
//!
//! The fixed low-pass is derived from the pinned Blue Dragon nominal PFB
//! prototype parameters, but it is not a bit-exact reproduction of Blue
//! Dragon's quantized Type-2 polyphase implementation. Filtering is centered
//! and delay-compensated: callers provide 16 context samples on each side of
//! the requested output interval, and no zero padding is introduced.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::BLUE_DRAGON_ALIGNMENT_REVISION;
use crate::burst_families::ComplexSample;

const INPUT_SAMPLE_RATE_HZ: u32 = 4_000_000;
const NOMINAL_CUTOFF_HZ: u32 = 750_000;
const PASSBAND_EDGE_HZ: u32 = 500_000;
const STOPBAND_EDGE_HZ: u32 = 1_000_000;
const KAISER_BETA: f64 = 5.653_26;
const FILTER_TAPS: usize = 33;
const COEFFICIENT_SHA256: &str = "a7499acefb635acb0272c674b11d714d3c9ef16e4401da90322223ae09867cd3";
const INTERRUPT_CHECK_INTERVAL: usize = 16_384;

/// Input samples required before and after a delay-compensated output window.
pub(super) const FILTER_HALF_LEN: usize = 16;

// Normalized f64 coefficients from a 33-tap Kaiser-windowed sinc with
// beta=0.1102*(60-8.7) and normalized cutoff 0.75/4. The source formula is the
// nominal prototype design at the pinned Blue Dragon revision; freezing the
// coefficients here makes this independent control deterministic.
const COEFFICIENTS: [f64; FILTER_TAPS] = [
    -2.981_354_493_850_623e-19,
    -0.000_890_975_681_387_619_4,
    -0.001_296_719_153_165_689_5,
    0.001_188_669_194_660_569_6,
    0.004_888_368_744_646_244_5,
    0.002_796_114_085_705_982_4,
    -0.007_434_340_064_712_591,
    -0.013_585_176_251_710_249,
    7.400_675_305_973_39e-18,
    0.025_149_643_861_198_64,
    0.025_857_488_906_649_56,
    -0.018_867_242_382_230_417,
    -0.067_667_826_580_729_45,
    -0.037_091_748_006_948_61,
    0.108_143_698_672_428_03,
    0.291_246_680_544_355_65,
    0.375_126_728_222_479_94,
    0.291_246_680_544_355_65,
    0.108_143_698_672_428_03,
    -0.037_091_748_006_948_61,
    -0.067_667_826_580_729_45,
    -0.018_867_242_382_230_417,
    0.025_857_488_906_649_56,
    0.025_149_643_861_198_64,
    7.400_675_305_973_39e-18,
    -0.013_585_176_251_710_249,
    -0.007_434_340_064_712_591,
    0.002_796_114_085_705_982_4,
    0.004_888_368_744_646_244_5,
    0.001_188_669_194_660_569_6,
    -0.001_296_719_153_165_689_5,
    -0.000_890_975_681_387_619_4,
    -2.981_354_493_850_623e-19,
];

/// Path-free, serialized contract for the fixed matched-rate filter.
#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct FilterSettings {
    design: &'static str,
    derivation_source_revision: &'static str,
    derivation_source_location: &'static str,
    prototype_num_channels: u64,
    prototype_semi_len: u64,
    prototype_design_attenuation_db: f64,
    input_sample_rate_hz: u32,
    tap_count: u64,
    nominal_cutoff_hz: u32,
    passband_edge_hz: u32,
    maximum_passband_deviation_db: f64,
    stopband_edge_hz: u32,
    minimum_stopband_attenuation_db: f64,
    kaiser_beta: f64,
    group_delay_input_samples: u64,
    delay_compensated: bool,
    coefficient_encoding: &'static str,
    coefficient_sha256: &'static str,
    decimation: u64,
    global_output_phases: [u64; 2],
}

pub(super) fn filter_settings() -> FilterSettings {
    FilterSettings {
        design: "normalized_33_tap_kaiser_pfb_derived_low_pass",
        derivation_source_revision: BLUE_DRAGON_ALIGNMENT_REVISION,
        derivation_source_location: "crates/dsp/src/window.rs:37-75",
        prototype_num_channels: 4,
        prototype_semi_len: 4,
        prototype_design_attenuation_db: 60.0,
        input_sample_rate_hz: INPUT_SAMPLE_RATE_HZ,
        tap_count: FILTER_TAPS as u64,
        nominal_cutoff_hz: NOMINAL_CUTOFF_HZ,
        passband_edge_hz: PASSBAND_EDGE_HZ,
        maximum_passband_deviation_db: 0.05,
        stopband_edge_hz: STOPBAND_EDGE_HZ,
        minimum_stopband_attenuation_db: 55.0,
        kaiser_beta: KAISER_BETA,
        group_delay_input_samples: FILTER_HALF_LEN as u64,
        delay_compensated: true,
        coefficient_encoding: "concatenated_f64_le",
        coefficient_sha256: COEFFICIENT_SHA256,
        decimation: 2,
        global_output_phases: [0, 1],
    }
}

#[derive(Debug)]
pub(super) struct MatchedViews {
    pub(super) four_msps: Vec<ComplexSample>,
    pub(super) two_msps_global_even: Vec<ComplexSample>,
    pub(super) two_msps_global_odd: Vec<ComplexSample>,
}

/// Filters one context-backed interval and derives both global decimation phases.
///
/// `context[FILTER_HALF_LEN]` is the sample at `first_output_index`. The
/// requested output length must be positive and even, and the context must have
/// exactly `FILTER_HALF_LEN` additional samples on each side.
pub(super) fn matched_views(
    context: &[ComplexSample],
    first_output_index: u64,
    output_samples: usize,
    interrupted: &AtomicBool,
) -> Result<MatchedViews> {
    ensure!(
        output_samples > 0 && output_samples.is_multiple_of(2),
        "matched-rate output sample count must be positive and even"
    );
    let expected_context = output_samples
        .checked_add(2 * FILTER_HALF_LEN)
        .context("matched-rate context length overflow")?;
    ensure!(
        context.len() == expected_context,
        "matched-rate context must contain exactly {FILTER_HALF_LEN} samples on each side"
    );
    let output_samples_u64 = u64::try_from(output_samples)?;
    first_output_index
        .checked_add(output_samples_u64 - 1)
        .context("matched-rate absolute output index overflow")?;

    for (index, sample) in context.iter().enumerate() {
        if index.is_multiple_of(INTERRUPT_CHECK_INTERVAL) {
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "matched-rate filtering interrupted"
            );
        }
        ensure!(
            sample.re.is_finite() && sample.im.is_finite(),
            "matched-rate context contains a non-finite sample"
        );
    }

    let mut four_msps = Vec::with_capacity(output_samples);
    let mut two_msps_global_even = Vec::with_capacity(output_samples / 2);
    let mut two_msps_global_odd = Vec::with_capacity(output_samples / 2);
    for output_offset in 0..output_samples {
        if output_offset.is_multiple_of(INTERRUPT_CHECK_INTERVAL) {
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "matched-rate filtering interrupted"
            );
        }
        let mut re = 0.0f64;
        let mut im = 0.0f64;
        for (&coefficient, sample) in COEFFICIENTS
            .iter()
            .zip(&context[output_offset..output_offset + FILTER_TAPS])
        {
            re += coefficient * f64::from(sample.re);
            im += coefficient * f64::from(sample.im);
        }
        let filtered = ComplexSample {
            re: re as f32,
            im: im as f32,
        };
        ensure!(
            filtered.re.is_finite() && filtered.im.is_finite(),
            "matched-rate filter produced a non-finite sample"
        );
        four_msps.push(filtered);
        let absolute_index = first_output_index + output_offset as u64;
        if absolute_index.is_multiple_of(2) {
            two_msps_global_even.push(filtered);
        } else {
            two_msps_global_odd.push(filtered);
        }
    }
    ensure!(
        two_msps_global_even.len() == output_samples / 2
            && two_msps_global_odd.len() == output_samples / 2,
        "internal matched-rate decimation-phase accounting mismatch"
    );
    Ok(MatchedViews {
        four_msps,
        two_msps_global_even,
        two_msps_global_odd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burst_families::complex_direct_feature;
    use sha2::{Digest, Sha256};

    fn response_db(frequency_hz: u32) -> f64 {
        let radians =
            std::f64::consts::TAU * f64::from(frequency_hz) / f64::from(INPUT_SAMPLE_RATE_HZ);
        let (re, im) =
            COEFFICIENTS
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(re, im), (index, coefficient)| {
                    let phase = radians * index as f64;
                    (
                        re + coefficient * phase.cos(),
                        im - coefficient * phase.sin(),
                    )
                });
        20.0 * re.hypot(im).max(f64::MIN_POSITIVE).log10()
    }

    fn alternating_tones(sample_count: usize) -> Vec<ComplexSample> {
        let mut phase = 0.0f64;
        (0..sample_count)
            .map(|index| {
                let frequency_hz = if (index / 512) % 2 == 0 {
                    -470_000.0
                } else {
                    470_000.0
                };
                phase += std::f64::consts::TAU * frequency_hz / f64::from(INPUT_SAMPLE_RATE_HZ);
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect()
    }

    fn qualified_direct_fit(samples: &[ComplexSample], sample_rate_hz: u32) -> ([f64; 2], f64) {
        let feature = complex_direct_feature(samples, sample_rate_hz, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert!(feature.gate_evaluation.all_gates_passed);
        let fit = feature.two_level.unwrap();
        (fit.centers_hz, fit.separation_hz)
    }

    fn deterministic_noise(sample_count: usize) -> Vec<ComplexSample> {
        let mut state = 0x8f3c_7a21_5d49_b6e0u64;
        (0..sample_count)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let re = (state as u32) as f64 / f64::from(u32::MAX) * 2.0 - 1.0;
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let im = (state as u32) as f64 / f64::from(u32::MAX) * 2.0 - 1.0;
                ComplexSample {
                    re: re as f32,
                    im: im as f32,
                }
            })
            .collect()
    }

    #[test]
    fn frozen_coefficients_have_the_declared_identity_symmetry_and_gain() {
        let mut digest = Sha256::new();
        for coefficient in COEFFICIENTS {
            digest.update(coefficient.to_le_bytes());
        }
        assert_eq!(format!("{:x}", digest.finalize()), COEFFICIENT_SHA256);
        for index in 0..FILTER_HALF_LEN {
            assert_eq!(COEFFICIENTS[index], COEFFICIENTS[FILTER_TAPS - 1 - index]);
        }
        assert!((COEFFICIENTS.iter().sum::<f64>() - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn dense_response_meets_the_declared_passband_and_stopband_contract() {
        for frequency_hz in (0..=PASSBAND_EDGE_HZ).step_by(1_000) {
            let response = response_db(frequency_hz);
            assert!(
                response.abs() <= 0.05,
                "passband response at {frequency_hz} Hz was {response} dB"
            );
        }
        for frequency_hz in (STOPBAND_EDGE_HZ..=INPUT_SAMPLE_RATE_HZ / 2).step_by(1_000) {
            let response = response_db(frequency_hz);
            assert!(
                response <= -55.0,
                "stopband response at {frequency_hz} Hz was {response} dB"
            );
        }
    }

    #[test]
    fn centered_impulse_preserves_the_requested_output_index() {
        let output_samples = 32;
        let impulse_output = 11;
        let mut context = vec![ComplexSample::default(); output_samples + 2 * FILTER_HALF_LEN];
        context[FILTER_HALF_LEN + impulse_output].re = 1.0;
        let views = matched_views(&context, 100, output_samples, &AtomicBool::new(false)).unwrap();
        let peak = views
            .four_msps
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.re.abs().total_cmp(&right.re.abs()))
            .unwrap();
        assert_eq!(peak.0, impulse_output);
        assert_eq!(peak.1.re, COEFFICIENTS[FILTER_HALF_LEN] as f32);
    }

    #[test]
    fn both_global_phases_have_exact_lengths_and_membership() {
        let output_samples = 20;
        let context = alternating_tones(output_samples + 2 * FILTER_HALF_LEN);
        for first_output_index in [200, 201] {
            let views = matched_views(
                &context,
                first_output_index,
                output_samples,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(views.four_msps.len(), output_samples);
            assert_eq!(views.two_msps_global_even.len(), output_samples / 2);
            assert_eq!(views.two_msps_global_odd.len(), output_samples / 2);
            let expected_even = views
                .four_msps
                .iter()
                .enumerate()
                .filter(|(offset, _)| (first_output_index + *offset as u64).is_multiple_of(2))
                .map(|(_, sample)| *sample)
                .collect::<Vec<_>>();
            let expected_odd = views
                .four_msps
                .iter()
                .enumerate()
                .filter(|(offset, _)| !(first_output_index + *offset as u64).is_multiple_of(2))
                .map(|(_, sample)| *sample)
                .collect::<Vec<_>>();
            assert_eq!(views.two_msps_global_even, expected_even);
            assert_eq!(views.two_msps_global_odd, expected_odd);
        }
    }

    #[test]
    fn matched_views_recover_direct_tones_and_are_invariant_to_a_context_crop() {
        let first_output_index = 256usize;
        let output_samples = 8_192usize;
        let all_samples = alternating_tones(first_output_index + output_samples + FILTER_HALF_LEN);
        let context = &all_samples[first_output_index - FILTER_HALF_LEN
            ..first_output_index + output_samples + FILTER_HALF_LEN];
        let full = matched_views(
            context,
            first_output_index as u64,
            output_samples,
            &AtomicBool::new(false),
        )
        .unwrap();
        for (centers_hz, separation_hz) in [
            qualified_direct_fit(&full.four_msps, INPUT_SAMPLE_RATE_HZ),
            qualified_direct_fit(&full.two_msps_global_even, INPUT_SAMPLE_RATE_HZ / 2),
            qualified_direct_fit(&full.two_msps_global_odd, INPUT_SAMPLE_RATE_HZ / 2),
        ] {
            assert!(
                (centers_hz[0] + 470_000.0).abs() < 5_000.0
                    && (centers_hz[1] - 470_000.0).abs() < 5_000.0
                    && (separation_hz - 940_000.0).abs() < 5_000.0,
                "recovered centers were {centers_hz:?} Hz with {separation_hz} Hz separation"
            );
        }

        let crop_offset = 1_024usize;
        let crop_samples = 4_096usize;
        let cropped_context =
            &context[crop_offset..crop_offset + crop_samples + 2 * FILTER_HALF_LEN];
        let cropped = matched_views(
            cropped_context,
            (first_output_index + crop_offset) as u64,
            crop_samples,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            cropped.four_msps,
            full.four_msps[crop_offset..crop_offset + crop_samples]
        );
        let phase_offset = crop_offset / 2;
        assert_eq!(
            cropped.two_msps_global_even,
            full.two_msps_global_even[phase_offset..phase_offset + crop_samples / 2]
        );
        assert_eq!(
            cropped.two_msps_global_odd,
            full.two_msps_global_odd[phase_offset..phase_offset + crop_samples / 2]
        );
    }

    #[test]
    fn matched_views_do_not_qualify_deterministic_noise() {
        let output_samples = 8_192;
        let context = deterministic_noise(output_samples + 2 * FILTER_HALF_LEN);
        let views = matched_views(&context, 512, output_samples, &AtomicBool::new(false)).unwrap();
        for (samples, sample_rate_hz) in [
            (views.four_msps.as_slice(), INPUT_SAMPLE_RATE_HZ),
            (
                views.two_msps_global_even.as_slice(),
                INPUT_SAMPLE_RATE_HZ / 2,
            ),
            (
                views.two_msps_global_odd.as_slice(),
                INPUT_SAMPLE_RATE_HZ / 2,
            ),
        ] {
            let feature = complex_direct_feature(samples, sample_rate_hz, &AtomicBool::new(false))
                .unwrap()
                .unwrap();
            assert!(!feature.gate_evaluation.all_gates_passed);
        }
    }

    #[test]
    fn invalid_inputs_and_interruption_are_rejected() {
        let interrupted = AtomicBool::new(true);
        let context = vec![ComplexSample::default(); 64];
        assert!(matched_views(&context, 0, 32, &interrupted).is_err());
        assert!(matched_views(&context, 0, 31, &AtomicBool::new(false)).is_err());
        assert!(matched_views(&context[..63], 0, 32, &AtomicBool::new(false)).is_err());

        let mut non_finite = context;
        non_finite[0].re = f32::NAN;
        assert!(matched_views(&non_finite, 0, 32, &AtomicBool::new(false)).is_err());
    }
}
