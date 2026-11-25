//! Pure envelope and phase-feature extraction for bounded burst samples.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    ACTIVE_POWER_DIVISOR, ACTIVE_POWER_QUANTILE_DENOMINATOR, ACTIVE_POWER_QUANTILE_NUMERATOR,
    CHANNEL_SAMPLE_RATE_HZ, KMEANS_ITERATIONS, MAX_ENVELOPE_CV, MAX_SEPARATION_HZ,
    MIN_CLUSTER_BALANCE, MIN_MODE_CONTRAST, MIN_PHASE_CONCENTRATION, MIN_SEPARATION_HZ,
    MIN_TRANSITIONS, MIN_USABLE_PHASE_DIFFERENCES, MIN_VARIANCE_REDUCTION,
    MODE_CONTRAST_STD_FLOOR_HZ, RecordMetadata,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TwoLevelFeature {
    pub(crate) centers_hz: [f64; 2],
    pub(crate) separation_hz: f64,
    pub(crate) variance_reduction: f64,
    pub(crate) mode_contrast: f64,
    pub(crate) cluster_balance: f64,
    pub(crate) transitions: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RecordFeature {
    pub(crate) envelope_cv: f64,
    pub(crate) usable_phase_differences: usize,
    pub(crate) phase_concentration: f64,
    pub(crate) two_level: Option<TwoLevelFeature>,
    pub(crate) fsk_like: bool,
    pub(crate) constant_envelope: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DirectFeature {
    pub(crate) two_level: Option<TwoLevelFeature>,
    pub(crate) gate_evaluation: ShapeGateEvaluation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShapeFirstFailure {
    InsufficientFeature,
    EnvelopeCvAboveMaximum,
    PhaseConcentrationBelowMinimum,
    TwoLevelFitUnavailable,
    ClusterBalanceBelowMinimum,
    VarianceReductionBelowMinimum,
    ModeContrastBelowMinimum,
    SeparationBelowMinimum,
    TransitionsBelowMinimum,
    AllGatesPassed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShapeGateEvaluation {
    pub(crate) envelope_cv: Option<f64>,
    pub(crate) usable_phase_differences: Option<u64>,
    pub(crate) phase_concentration: Option<f64>,
    pub(crate) two_level: Option<TwoLevelFeature>,
    pub(crate) first_failure: ShapeFirstFailure,
    pub(crate) envelope_cv_above_maximum: bool,
    pub(crate) phase_concentration_below_minimum: Option<bool>,
    pub(crate) two_level_fit_unavailable: bool,
    pub(crate) cluster_balance_below_minimum: bool,
    pub(crate) variance_reduction_below_minimum: bool,
    pub(crate) mode_contrast_below_minimum: bool,
    pub(crate) separation_below_minimum: bool,
    pub(crate) transitions_below_minimum: bool,
    pub(crate) all_gates_passed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ComplexSample {
    pub(crate) re: f32,
    pub(crate) im: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ComplexControlFeatures {
    pub(crate) circular: Option<RecordFeature>,
    pub(crate) direct: Option<DirectFeature>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FeatureSettings {
    active_power_quantile: [u64; 2],
    active_power_divisor: u64,
    minimum_usable_phase_differences: u64,
    maximum_envelope_cv: f64,
    minimum_cluster_balance: f64,
    minimum_variance_reduction: f64,
    minimum_mode_contrast: f64,
    mode_contrast_std_floor_hz: f64,
    minimum_phase_concentration: f64,
    fsk_like_separation_hz: [f64; 2],
    direct_qualified_minimum_phase_concentration: Option<f64>,
    direct_qualified_minimum_separation_hz: f64,
    direct_qualified_maximum_separation_hz: Option<f64>,
    minimum_transitions: u64,
    kmeans_iterations: u64,
}

pub(crate) fn feature_settings() -> FeatureSettings {
    FeatureSettings {
        active_power_quantile: [
            ACTIVE_POWER_QUANTILE_NUMERATOR as u64,
            ACTIVE_POWER_QUANTILE_DENOMINATOR as u64,
        ],
        active_power_divisor: ACTIVE_POWER_DIVISOR,
        minimum_usable_phase_differences: MIN_USABLE_PHASE_DIFFERENCES as u64,
        maximum_envelope_cv: MAX_ENVELOPE_CV,
        minimum_cluster_balance: MIN_CLUSTER_BALANCE,
        minimum_variance_reduction: MIN_VARIANCE_REDUCTION,
        minimum_mode_contrast: MIN_MODE_CONTRAST,
        mode_contrast_std_floor_hz: MODE_CONTRAST_STD_FLOOR_HZ,
        minimum_phase_concentration: MIN_PHASE_CONCENTRATION,
        fsk_like_separation_hz: [MIN_SEPARATION_HZ, MAX_SEPARATION_HZ],
        direct_qualified_minimum_phase_concentration: None,
        direct_qualified_minimum_separation_hz: MIN_SEPARATION_HZ,
        direct_qualified_maximum_separation_hz: None,
        minimum_transitions: MIN_TRANSITIONS,
        kmeans_iterations: KMEANS_ITERATIONS as u64,
    }
}

fn wrap_angle(mut angle: f64) -> f64 {
    if angle >= std::f64::consts::PI {
        angle -= std::f64::consts::TAU;
    }
    if angle < -std::f64::consts::PI {
        angle += std::f64::consts::TAU;
    }
    angle
}

fn quantile(sorted: &[f64], numerator: usize, denominator: usize) -> f64 {
    sorted[(sorted.len() - 1) * numerator / denominator]
}

fn fit_two_levels_without_concentration(
    values: &[f64],
    continues_previous: &[bool],
) -> Option<TwoLevelFeature> {
    debug_assert_eq!(values.len(), continues_previous.len());
    if values.len() < MIN_USABLE_PHASE_DIFFERENCES {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mut centers = [quantile(&sorted, 1, 4), quantile(&sorted, 3, 4)];
    if centers[0] == centers[1] {
        return None;
    }
    for _ in 0..KMEANS_ITERATIONS {
        let mut counts = [0usize; 2];
        let mut sums = [0.0, 0.0];
        for &value in values {
            let cluster = usize::from((value - centers[1]).abs() < (value - centers[0]).abs());
            counts[cluster] += 1;
            sums[cluster] += value;
        }
        if counts.contains(&0) {
            return None;
        }
        centers = [sums[0] / counts[0] as f64, sums[1] / counts[1] as f64];
    }
    if centers[0] > centers[1] {
        centers.swap(0, 1);
    }

    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let total_variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>();
    if !total_variance.is_finite() || total_variance <= f64::EPSILON {
        return None;
    }
    let mut counts = [0usize; 2];
    let mut within_by_cluster = [0.0, 0.0];
    let mut transitions = 0u64;
    let mut previous = None;
    for (index, &value) in values.iter().enumerate() {
        let cluster = usize::from((value - centers[1]).abs() < (value - centers[0]).abs());
        counts[cluster] += 1;
        within_by_cluster[cluster] += (value - centers[cluster]).powi(2);
        if continues_previous[index] && previous.is_some_and(|last| last != cluster) {
            transitions += 1;
        }
        previous = Some(cluster);
    }
    if counts.contains(&0) {
        return None;
    }
    let within = within_by_cluster.iter().sum::<f64>();
    let pooled_within_std = (within / values.len() as f64).sqrt();
    let separation_hz = centers[1] - centers[0];
    // A one-hertz floor keeps perfect synthetic clusters finite and JSON-safe.
    let mode_contrast = separation_hz / pooled_within_std.max(MODE_CONTRAST_STD_FLOOR_HZ);
    Some(TwoLevelFeature {
        centers_hz: centers,
        separation_hz,
        variance_reduction: (1.0 - within / total_variance).clamp(0.0, 1.0),
        mode_contrast,
        cluster_balance: counts[0].min(counts[1]) as f64 / values.len() as f64,
        transitions,
    })
}

pub(super) fn fit_two_levels(
    values: &[f64],
    continues_previous: &[bool],
    concentration: f64,
) -> Option<TwoLevelFeature> {
    if concentration < MIN_PHASE_CONCENTRATION {
        return None;
    }
    fit_two_levels_without_concentration(values, continues_previous)
}

fn evaluate_shape_gates(
    envelope_cv: Option<f64>,
    usable_phase_differences: Option<usize>,
    phase_concentration: Option<f64>,
    phase_concentration_applies: bool,
    two_level: Option<TwoLevelFeature>,
) -> ShapeGateEvaluation {
    let Some(envelope_cv) = envelope_cv else {
        return ShapeGateEvaluation {
            envelope_cv: None,
            usable_phase_differences: None,
            phase_concentration: None,
            two_level: None,
            first_failure: ShapeFirstFailure::InsufficientFeature,
            envelope_cv_above_maximum: false,
            phase_concentration_below_minimum: phase_concentration_applies.then_some(false),
            two_level_fit_unavailable: false,
            cluster_balance_below_minimum: false,
            variance_reduction_below_minimum: false,
            mode_contrast_below_minimum: false,
            separation_below_minimum: false,
            transitions_below_minimum: false,
            all_gates_passed: false,
        };
    };
    let envelope_cv_above_maximum = envelope_cv > MAX_ENVELOPE_CV;
    let phase_concentration_below_minimum = phase_concentration_applies
        .then(|| phase_concentration.is_some_and(|value| value < MIN_PHASE_CONCENTRATION));
    let two_level_fit_unavailable = two_level.is_none();
    let cluster_balance_below_minimum =
        two_level.is_some_and(|fit| fit.cluster_balance < MIN_CLUSTER_BALANCE);
    let variance_reduction_below_minimum =
        two_level.is_some_and(|fit| fit.variance_reduction < MIN_VARIANCE_REDUCTION);
    let mode_contrast_below_minimum =
        two_level.is_some_and(|fit| fit.mode_contrast < MIN_MODE_CONTRAST);
    let separation_below_minimum =
        two_level.is_some_and(|fit| fit.separation_hz < MIN_SEPARATION_HZ);
    let transitions_below_minimum = two_level.is_some_and(|fit| fit.transitions < MIN_TRANSITIONS);
    let first_failure = if envelope_cv_above_maximum {
        ShapeFirstFailure::EnvelopeCvAboveMaximum
    } else if phase_concentration_below_minimum == Some(true) {
        ShapeFirstFailure::PhaseConcentrationBelowMinimum
    } else if two_level_fit_unavailable {
        ShapeFirstFailure::TwoLevelFitUnavailable
    } else if cluster_balance_below_minimum {
        ShapeFirstFailure::ClusterBalanceBelowMinimum
    } else if variance_reduction_below_minimum {
        ShapeFirstFailure::VarianceReductionBelowMinimum
    } else if mode_contrast_below_minimum {
        ShapeFirstFailure::ModeContrastBelowMinimum
    } else if separation_below_minimum {
        ShapeFirstFailure::SeparationBelowMinimum
    } else if transitions_below_minimum {
        ShapeFirstFailure::TransitionsBelowMinimum
    } else {
        ShapeFirstFailure::AllGatesPassed
    };
    ShapeGateEvaluation {
        envelope_cv: Some(envelope_cv),
        usable_phase_differences: usable_phase_differences.map(|count| count as u64),
        phase_concentration,
        two_level,
        first_failure,
        envelope_cv_above_maximum,
        phase_concentration_below_minimum,
        two_level_fit_unavailable,
        cluster_balance_below_minimum,
        variance_reduction_below_minimum,
        mode_contrast_below_minimum,
        separation_below_minimum,
        transitions_below_minimum,
        all_gates_passed: first_failure == ShapeFirstFailure::AllGatesPassed,
    }
}

pub(crate) fn circular_shape_gate_evaluation(
    feature: Option<RecordFeature>,
) -> ShapeGateEvaluation {
    feature.map_or_else(
        || evaluate_shape_gates(None, None, None, true, None),
        |feature| {
            evaluate_shape_gates(
                Some(feature.envelope_cv),
                Some(feature.usable_phase_differences),
                Some(feature.phase_concentration),
                true,
                feature.two_level,
            )
        },
    )
}

pub(crate) fn direct_shape_gate_evaluation(feature: Option<DirectFeature>) -> ShapeGateEvaluation {
    feature.map_or_else(
        || evaluate_shape_gates(None, None, None, false, None),
        |feature| feature.gate_evaluation,
    )
}

pub(crate) fn two_level_shape_qualified(
    envelope_cv: f64,
    phase_concentration: f64,
    fit: TwoLevelFeature,
) -> bool {
    evaluate_shape_gates(
        Some(envelope_cv),
        Some(MIN_USABLE_PHASE_DIFFERENCES),
        Some(phase_concentration),
        true,
        Some(fit),
    )
    .all_gates_passed
}

fn first_circular_moment(phase_differences: &[f64]) -> (f64, f64, f64) {
    let cosine_sum = phase_differences.iter().map(|angle| angle.cos()).sum();
    let sine_sum = phase_differences.iter().map(|angle| angle.sin()).sum();
    let concentration = f64::hypot(cosine_sum, sine_sum) / phase_differences.len() as f64;
    (cosine_sum, sine_sum, concentration)
}

fn finish_record_feature(
    envelope_cv: f64,
    phase_differences: &[f64],
    continues_previous: &[bool],
    sample_rate_hz: u32,
) -> Option<RecordFeature> {
    if phase_differences.len() < MIN_USABLE_PHASE_DIFFERENCES {
        return None;
    }
    debug_assert_eq!(phase_differences.len(), continues_previous.len());
    let (circular_cos, circular_sin, concentration) = first_circular_moment(phase_differences);
    let circular_center = circular_sin.atan2(circular_cos);
    let hz_per_radian = f64::from(sample_rate_hz) / std::f64::consts::TAU;
    let recentered_hz: Vec<f64> = phase_differences
        .iter()
        .map(|&angle| wrap_angle(angle - circular_center) * hz_per_radian)
        .collect();
    let two_level = fit_two_levels(&recentered_hz, continues_previous, concentration);
    let constant_envelope = envelope_cv <= MAX_ENVELOPE_CV;
    let fsk_like = two_level.is_some_and(|fit| {
        two_level_shape_qualified(envelope_cv, concentration, fit)
            && fit.separation_hz <= MAX_SEPARATION_HZ
    });
    Some(RecordFeature {
        envelope_cv,
        usable_phase_differences: phase_differences.len(),
        phase_concentration: concentration,
        two_level,
        fsk_like,
        constant_envelope,
    })
}

pub(super) fn record_feature(
    metadata: RecordMetadata,
    packed_iq: &[u8],
    interrupted: &AtomicBool,
) -> Result<Option<RecordFeature>> {
    let expected_bytes = usize::try_from(metadata.sample_count)
        .context("sample count does not fit memory")?
        .checked_mul(4)
        .context("sample byte count overflow")?;
    ensure!(
        packed_iq.len() == expected_bytes,
        "internal burst body length mismatch"
    );
    if metadata.sample_count < 2 {
        return Ok(None);
    }
    let (packed_samples, remainder) = packed_iq.as_chunks::<4>();
    debug_assert!(remainder.is_empty());
    let mut magnitudes_squared = Vec::with_capacity(metadata.sample_count as usize);
    for (index, sample) in packed_samples.iter().enumerate() {
        if index.is_multiple_of(16_384) {
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "burst processing interrupted"
            );
        }
        let i = i64::from(i16::from_le_bytes([sample[0], sample[1]]));
        let q = i64::from(i16::from_le_bytes([sample[2], sample[3]]));
        magnitudes_squared.push((i * i + q * q) as u64);
    }
    let mut ordered_magnitudes = magnitudes_squared.clone();
    ordered_magnitudes.sort_unstable();
    let high = ordered_magnitudes[(ordered_magnitudes.len() - 1) * ACTIVE_POWER_QUANTILE_NUMERATOR
        / ACTIVE_POWER_QUANTILE_DENOMINATOR];
    // Keep samples at or above one quarter of the robust p80 envelope. Power is
    // squared here, so the threshold is p80 / 16. This rejects deep squelch
    // tails without rank-pruning a quantized constant-envelope waveform.
    let active_threshold = high / ACTIVE_POWER_DIVISOR;
    let active =
        |magnitude_squared: u64| magnitude_squared > 0 && magnitude_squared >= active_threshold;

    let mut active_count = 0usize;
    let mut envelope_sum = 0.0;
    let mut envelope_square_sum = 0.0;
    for &magnitude_squared in &magnitudes_squared {
        if active(magnitude_squared) {
            active_count += 1;
            envelope_sum += (magnitude_squared as f64).sqrt();
            envelope_square_sum += magnitude_squared as f64;
        }
    }
    if active_count < MIN_USABLE_PHASE_DIFFERENCES + 1 {
        return Ok(None);
    }
    let envelope_mean = envelope_sum / active_count as f64;
    if envelope_mean <= f64::EPSILON {
        return Ok(None);
    }
    let envelope_variance =
        (envelope_square_sum / active_count as f64 - envelope_mean * envelope_mean).max(0.0);
    let envelope_cv = envelope_variance.sqrt() / envelope_mean;

    let samples: Vec<(i64, i64)> = packed_samples
        .iter()
        .map(|sample| {
            (
                i64::from(i16::from_le_bytes([sample[0], sample[1]])),
                i64::from(i16::from_le_bytes([sample[2], sample[3]])),
            )
        })
        .collect();
    let mut phase_differences = Vec::with_capacity(samples.len().saturating_sub(1));
    let mut continues_previous = Vec::with_capacity(samples.len().saturating_sub(1));
    let mut previous_usable_pair = None;
    for index in 1..samples.len() {
        if active(magnitudes_squared[index - 1]) && active(magnitudes_squared[index]) {
            let (previous_i, previous_q) = samples[index - 1];
            let (current_i, current_q) = samples[index];
            let real = current_i * previous_i + current_q * previous_q;
            let imaginary = current_q * previous_i - current_i * previous_q;
            if real != 0 || imaginary != 0 {
                phase_differences.push((imaginary as f64).atan2(real as f64));
                continues_previous.push(previous_usable_pair == Some(index - 1));
                previous_usable_pair = Some(index);
            }
        }
    }
    Ok(finish_record_feature(
        envelope_cv,
        &phase_differences,
        &continues_previous,
        CHANNEL_SAMPLE_RATE_HZ,
    ))
}

struct ComplexMeasurements {
    envelope_cv: f64,
    phase_differences: Vec<f64>,
    continues_previous: Vec<bool>,
}

fn complex_measurements(
    samples: &[ComplexSample],
    interrupted: &AtomicBool,
) -> Result<Option<ComplexMeasurements>> {
    if samples.len() < 2 {
        return Ok(None);
    }
    let mut magnitudes_squared = Vec::with_capacity(samples.len());
    for (index, sample) in samples.iter().enumerate() {
        if index.is_multiple_of(16_384) {
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "burst processing interrupted"
            );
        }
        ensure!(
            sample.re.is_finite() && sample.im.is_finite(),
            "derived complex sample is not finite"
        );
        let re = f64::from(sample.re);
        let im = f64::from(sample.im);
        magnitudes_squared.push(re * re + im * im);
    }
    let mut ordered_magnitudes = magnitudes_squared.clone();
    ordered_magnitudes.sort_by(f64::total_cmp);
    let high = ordered_magnitudes[(ordered_magnitudes.len() - 1) * ACTIVE_POWER_QUANTILE_NUMERATOR
        / ACTIVE_POWER_QUANTILE_DENOMINATOR];
    // Match the packed-IQ p80/16 active-envelope rule for squared complex power.
    let active_threshold = high / ACTIVE_POWER_DIVISOR as f64;
    let active =
        |magnitude_squared: f64| magnitude_squared > 0.0 && magnitude_squared >= active_threshold;

    let mut active_count = 0usize;
    let mut envelope_sum = 0.0;
    let mut envelope_square_sum = 0.0;
    for &magnitude_squared in &magnitudes_squared {
        if active(magnitude_squared) {
            active_count += 1;
            envelope_sum += magnitude_squared.sqrt();
            envelope_square_sum += magnitude_squared;
        }
    }
    if active_count < MIN_USABLE_PHASE_DIFFERENCES + 1 {
        return Ok(None);
    }
    let envelope_mean = envelope_sum / active_count as f64;
    if envelope_mean <= f64::EPSILON {
        return Ok(None);
    }
    let envelope_variance =
        (envelope_square_sum / active_count as f64 - envelope_mean * envelope_mean).max(0.0);
    let envelope_cv = envelope_variance.sqrt() / envelope_mean;

    let mut phase_differences = Vec::with_capacity(samples.len().saturating_sub(1));
    let mut continues_previous = Vec::with_capacity(samples.len().saturating_sub(1));
    let mut previous_usable_pair = None;
    for index in 1..samples.len() {
        if active(magnitudes_squared[index - 1]) && active(magnitudes_squared[index]) {
            let previous = samples[index - 1];
            let current = samples[index];
            let real = f64::from(current.re) * f64::from(previous.re)
                + f64::from(current.im) * f64::from(previous.im);
            let imaginary = f64::from(current.im) * f64::from(previous.re)
                - f64::from(current.re) * f64::from(previous.im);
            if real != 0.0 || imaginary != 0.0 {
                phase_differences.push(imaginary.atan2(real));
                continues_previous.push(previous_usable_pair == Some(index - 1));
                previous_usable_pair = Some(index);
            }
        }
    }
    if phase_differences.len() < MIN_USABLE_PHASE_DIFFERENCES {
        return Ok(None);
    }
    Ok(Some(ComplexMeasurements {
        envelope_cv,
        phase_differences,
        continues_previous,
    }))
}

pub(crate) fn complex_record_feature(
    samples: &[ComplexSample],
    sample_rate_hz: u32,
    interrupted: &AtomicBool,
) -> Result<Option<RecordFeature>> {
    ensure!(sample_rate_hz > 0, "feature sample rate must be positive");
    let Some(measurements) = complex_measurements(samples, interrupted)? else {
        return Ok(None);
    };
    Ok(finish_record_feature(
        measurements.envelope_cv,
        &measurements.phase_differences,
        &measurements.continues_previous,
        sample_rate_hz,
    ))
}

fn direct_feature_from_measurements(
    measurements: &ComplexMeasurements,
    sample_rate_hz: u32,
) -> DirectFeature {
    let hz_per_radian = f64::from(sample_rate_hz) / std::f64::consts::TAU;
    let direct_hz: Vec<f64> = measurements
        .phase_differences
        .iter()
        .map(|angle| angle * hz_per_radian)
        .collect();
    // Keep diagnostic splits above the shared resolution floor, but impose no upper
    // separation bound: the >2 MHz branch-spanning case is the point of this control.
    // Circular concentration is intentionally not a precondition: antipodal modes
    // such as +/-1 MHz at 4 MSps have a zero first circular moment despite being a
    // well-defined pair in the unrecentered signed-frequency coordinates.
    let raw_two_level =
        fit_two_levels_without_concentration(&direct_hz, &measurements.continues_previous);
    let gate_evaluation = evaluate_shape_gates(
        Some(measurements.envelope_cv),
        Some(measurements.phase_differences.len()),
        Some(first_circular_moment(&measurements.phase_differences).2),
        false,
        raw_two_level,
    );
    let two_level = raw_two_level.filter(|fit| fit.separation_hz >= MIN_SEPARATION_HZ);
    DirectFeature {
        two_level,
        gate_evaluation,
    }
}

/// Extracts circular and direct controls from one shared measurement pass.
pub(crate) fn complex_control_features(
    samples: &[ComplexSample],
    sample_rate_hz: u32,
    interrupted: &AtomicBool,
) -> Result<ComplexControlFeatures> {
    ensure!(sample_rate_hz > 0, "feature sample rate must be positive");
    let Some(measurements) = complex_measurements(samples, interrupted)? else {
        return Ok(ComplexControlFeatures {
            circular: None,
            direct: None,
        });
    };
    Ok(ComplexControlFeatures {
        circular: finish_record_feature(
            measurements.envelope_cv,
            &measurements.phase_differences,
            &measurements.continues_previous,
            sample_rate_hz,
        ),
        direct: Some(direct_feature_from_measurements(
            &measurements,
            sample_rate_hz,
        )),
    })
}

/// Fits and qualifies two frequency modes without circular recentering.
pub(crate) fn complex_direct_feature(
    samples: &[ComplexSample],
    sample_rate_hz: u32,
    interrupted: &AtomicBool,
) -> Result<Option<DirectFeature>> {
    ensure!(sample_rate_hz > 0, "feature sample rate must be positive");
    let Some(measurements) = complex_measurements(samples, interrupted)? else {
        return Ok(None);
    };
    Ok(Some(direct_feature_from_measurements(
        &measurements,
        sample_rate_hz,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_fit() -> TwoLevelFeature {
        TwoLevelFeature {
            centers_hz: [-470_000.0, 470_000.0],
            separation_hz: 940_000.0,
            variance_reduction: 0.99,
            mode_contrast: 20.0,
            cluster_balance: 0.5,
            transitions: 20,
        }
    }

    fn alternating_tones(sample_rate_hz: u32, tones_hz: [f64; 2]) -> Vec<ComplexSample> {
        let mut phase = 0.0f64;
        (0..2048)
            .map(|index| {
                let frequency = tones_hz[(index / 32) % 2];
                phase += std::f64::consts::TAU * frequency / f64::from(sample_rate_hz);
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect()
    }

    #[test]
    fn direct_fit_preserves_a_pair_spanning_the_phase_branch() {
        let interrupted = AtomicBool::new(false);
        let samples = alternating_tones(4_000_000, [-1_530_000.0, 1_530_000.0]);
        let direct_feature = complex_direct_feature(&samples, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(direct_feature.gate_evaluation.all_gates_passed);
        let direct = direct_feature.two_level.unwrap();
        let circular = complex_record_feature(&samples, 4_000_000, &interrupted)
            .unwrap()
            .unwrap()
            .two_level
            .unwrap();
        assert!((direct.centers_hz[0] + 1_530_000.0).abs() < 2_000.0);
        assert!((direct.centers_hz[1] - 1_530_000.0).abs() < 2_000.0);
        assert!((direct.separation_hz - 3_060_000.0).abs() < 3_000.0);
        assert!((circular.separation_hz - 940_000.0).abs() < 3_000.0);
    }

    #[test]
    fn direct_shape_qualifies_an_ordinary_pair_without_alias_unfolding() {
        let interrupted = AtomicBool::new(false);
        let samples = alternating_tones(4_000_000, [-470_000.0, 470_000.0]);
        let feature = complex_direct_feature(&samples, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(feature.gate_evaluation.all_gates_passed);
        let fit = feature.two_level.unwrap();
        assert!((fit.centers_hz[0] + 470_000.0).abs() < 2_000.0);
        assert!((fit.centers_hz[1] - 470_000.0).abs() < 2_000.0);
        assert!((fit.separation_hz - 940_000.0).abs() < 3_000.0);
    }

    #[test]
    fn direct_shape_qualifies_an_antipodal_pair_without_circular_concentration() {
        let interrupted = AtomicBool::new(false);
        let samples = alternating_tones(4_000_000, [-1_000_000.0, 1_000_000.0]);
        let feature = complex_direct_feature(&samples, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(feature.gate_evaluation.all_gates_passed);
        let fit = feature.two_level.unwrap();
        assert!((fit.centers_hz[0] + 1_000_000.0).abs() < 2_000.0);
        assert!((fit.centers_hz[1] - 1_000_000.0).abs() < 2_000.0);
        assert!((fit.separation_hz - 2_000_000.0).abs() < 3_000.0);
        let circular = complex_record_feature(&samples, 4_000_000, &interrupted).unwrap();
        assert_eq!(
            circular_shape_gate_evaluation(circular).first_failure,
            ShapeFirstFailure::PhaseConcentrationBelowMinimum
        );
    }

    #[test]
    fn gate_evaluation_declares_each_first_failure_in_order() {
        let fit = passing_fit();
        assert_eq!(
            evaluate_shape_gates(None, None, None, true, None).first_failure,
            ShapeFirstFailure::InsufficientFeature
        );
        assert_eq!(
            evaluate_shape_gates(Some(0.3), Some(512), Some(0.5), true, Some(fit)).first_failure,
            ShapeFirstFailure::EnvelopeCvAboveMaximum
        );
        assert_eq!(
            evaluate_shape_gates(Some(0.1), Some(512), Some(0.01), true, Some(fit)).first_failure,
            ShapeFirstFailure::PhaseConcentrationBelowMinimum
        );
        assert_eq!(
            evaluate_shape_gates(Some(0.1), Some(512), Some(0.5), true, None).first_failure,
            ShapeFirstFailure::TwoLevelFitUnavailable
        );
        for (mut failed, expected) in [
            (fit, ShapeFirstFailure::ClusterBalanceBelowMinimum),
            (fit, ShapeFirstFailure::VarianceReductionBelowMinimum),
            (fit, ShapeFirstFailure::ModeContrastBelowMinimum),
            (fit, ShapeFirstFailure::SeparationBelowMinimum),
            (fit, ShapeFirstFailure::TransitionsBelowMinimum),
        ] {
            match expected {
                ShapeFirstFailure::ClusterBalanceBelowMinimum => failed.cluster_balance = 0.1,
                ShapeFirstFailure::VarianceReductionBelowMinimum => {
                    failed.variance_reduction = 0.5;
                }
                ShapeFirstFailure::ModeContrastBelowMinimum => failed.mode_contrast = 3.0,
                ShapeFirstFailure::SeparationBelowMinimum => failed.separation_hz = 20_000.0,
                ShapeFirstFailure::TransitionsBelowMinimum => failed.transitions = 5,
                _ => unreachable!(),
            }
            assert_eq!(
                evaluate_shape_gates(Some(0.1), Some(512), Some(0.5), true, Some(failed))
                    .first_failure,
                expected
            );
        }
        assert_eq!(
            evaluate_shape_gates(Some(0.1), Some(512), Some(0.0), false, Some(fit)).first_failure,
            ShapeFirstFailure::AllGatesPassed
        );
        let mut multiple_failures = fit;
        multiple_failures.variance_reduction = 0.5;
        multiple_failures.transitions = 5;
        let evaluation = evaluate_shape_gates(
            Some(0.3),
            Some(512),
            Some(0.5),
            true,
            Some(multiple_failures),
        );
        assert_eq!(
            evaluation.first_failure,
            ShapeFirstFailure::EnvelopeCvAboveMaximum
        );
        assert!(evaluation.envelope_cv_above_maximum);
        assert!(evaluation.variance_reduction_below_minimum);
        assert!(evaluation.transitions_below_minimum);
    }

    #[test]
    fn shared_measurement_controls_match_separate_extractors() {
        let interrupted = AtomicBool::new(false);
        let samples = alternating_tones(4_000_000, [-470_000.0, 470_000.0]);
        let controls = complex_control_features(&samples, 4_000_000, &interrupted).unwrap();
        assert_eq!(
            controls.circular,
            complex_record_feature(&samples, 4_000_000, &interrupted).unwrap()
        );
        assert_eq!(
            controls.direct,
            complex_direct_feature(&samples, 4_000_000, &interrupted).unwrap()
        );
    }

    #[test]
    fn direct_gate_retains_a_fit_below_the_diagnostic_resolution_floor() {
        let interrupted = AtomicBool::new(false);
        let samples = alternating_tones(4_000_000, [-10_000.0, 10_000.0]);
        let feature = complex_direct_feature(&samples, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(feature.two_level.is_none());
        assert!(feature.gate_evaluation.two_level.is_some());
        assert_eq!(
            feature.gate_evaluation.first_failure,
            ShapeFirstFailure::SeparationBelowMinimum
        );
        assert!(!feature.gate_evaluation.all_gates_passed);
    }

    #[test]
    fn direct_shape_rejects_one_tone_and_deterministic_phase_noise() {
        let interrupted = AtomicBool::new(false);
        let tone = alternating_tones(4_000_000, [470_000.0, 470_000.0]);
        let tone_feature = complex_direct_feature(&tone, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(
            tone_feature.two_level.is_none(),
            "unexpected one-tone diagnostic split: {tone_feature:?}"
        );
        assert!(!tone_feature.gate_evaluation.all_gates_passed);

        let mut state = 0x1234_5678u32;
        let mut phase = 0.0f64;
        let noise = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let unit = f64::from(state) / f64::from(u32::MAX);
                phase += (unit * 2.0 - 1.0) * std::f64::consts::PI;
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect::<Vec<_>>();
        let noise_feature = complex_direct_feature(&noise, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(!noise_feature.gate_evaluation.all_gates_passed);

        let mut spare_gaussian = None;
        let gaussian_noise = (0..4096)
            .map(|_| {
                let increment = spare_gaussian.take().unwrap_or_else(|| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let first = (f64::from(state) + 1.0) / (f64::from(u32::MAX) + 2.0);
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let second = (f64::from(state) + 1.0) / (f64::from(u32::MAX) + 2.0);
                    let radius = (-2.0 * first.ln()).sqrt() * 0.5;
                    let angle = std::f64::consts::TAU * second;
                    spare_gaussian = Some(radius * angle.sin());
                    radius * angle.cos()
                });
                phase += increment;
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect::<Vec<_>>();
        let gaussian_feature = complex_direct_feature(&gaussian_noise, 4_000_000, &interrupted)
            .unwrap()
            .unwrap();
        assert!(!gaussian_feature.gate_evaluation.all_gates_passed);
    }
}
