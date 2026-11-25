//! Closed and overlapping qualification-gate accounting.

use anyhow::{Result, ensure};
use serde::Serialize;

use super::{NumericAccumulator, NumericSummary};
use crate::burst_families::{ShapeFirstFailure, ShapeGateEvaluation};

const CIRCULAR_FIRST_FAILURE_ORDER: &[&str] = &[
    "insufficient_feature",
    "envelope_cv_above_maximum",
    "phase_concentration_below_minimum",
    "two_level_fit_unavailable",
    "cluster_balance_below_minimum",
    "variance_reduction_below_minimum",
    "mode_contrast_below_minimum",
    "separation_below_minimum",
    "transitions_below_minimum",
    "all_gates_passed",
];

const DIRECT_FIRST_FAILURE_ORDER: &[&str] = &[
    "insufficient_feature",
    "envelope_cv_above_maximum",
    "two_level_fit_unavailable",
    "cluster_balance_below_minimum",
    "variance_reduction_below_minimum",
    "mode_contrast_below_minimum",
    "separation_below_minimum",
    "transitions_below_minimum",
    "all_gates_passed",
];

#[derive(Debug, Serialize)]
pub(super) struct GateAccounting {
    control_window_available_records: u64,
    pub(super) feature_eligible_records: u64,
    feature_insufficient_records: u64,
    pub(super) raw_two_level_fit_records: u64,
    pub(super) qualified_two_level_shape_records: u64,
}

#[derive(Debug, Default, Serialize)]
pub(super) struct FirstFailurePartition {
    insufficient_feature: u64,
    envelope_cv_above_maximum: u64,
    phase_concentration_below_minimum: Option<u64>,
    two_level_fit_unavailable: u64,
    cluster_balance_below_minimum: u64,
    variance_reduction_below_minimum: u64,
    mode_contrast_below_minimum: u64,
    separation_below_minimum: u64,
    transitions_below_minimum: u64,
    pub(super) all_gates_passed: u64,
}

impl FirstFailurePartition {
    fn total(&self) -> Option<u64> {
        [
            self.insufficient_feature,
            self.envelope_cv_above_maximum,
            self.phase_concentration_below_minimum.unwrap_or(0),
            self.two_level_fit_unavailable,
            self.cluster_balance_below_minimum,
            self.variance_reduction_below_minimum,
            self.mode_contrast_below_minimum,
            self.separation_below_minimum,
            self.transitions_below_minimum,
            self.all_gates_passed,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
    }
}

#[derive(Debug, Default, Serialize)]
struct OverlappingGateFailures {
    envelope_cv_above_maximum: u64,
    phase_concentration_below_minimum: Option<u64>,
    two_level_fit_unavailable: u64,
    cluster_balance_below_minimum: u64,
    variance_reduction_below_minimum: u64,
    mode_contrast_below_minimum: u64,
    separation_below_minimum: u64,
    transitions_below_minimum: u64,
}

#[derive(Debug, Serialize)]
struct GateFeatureSummaries {
    envelope_cv: NumericSummary,
    usable_phase_differences: NumericSummary,
    phase_concentration_diagnostic: NumericSummary,
    raw_two_level_low_hz: NumericSummary,
    raw_two_level_high_hz: NumericSummary,
    raw_two_level_separation_hz: NumericSummary,
    raw_two_level_cluster_balance: NumericSummary,
    raw_two_level_variance_reduction: NumericSummary,
    raw_two_level_mode_contrast: NumericSummary,
    raw_two_level_transitions: NumericSummary,
}

#[derive(Debug, Serialize)]
pub(super) struct GateBreakdownReport {
    frequency_coordinate: &'static str,
    phase_concentration_is_qualification_gate: bool,
    pub(super) accounting: GateAccounting,
    first_failure_order: &'static [&'static str],
    pub(super) first_failure_partition: FirstFailurePartition,
    overlapping_gate_failures: OverlappingGateFailures,
    feature_summaries: GateFeatureSummaries,
    first_failure_partition_is_closed: bool,
    overlapping_failures_are_nonexclusive: bool,
}

#[derive(Debug, Default)]
pub(super) struct GateBreakdownAccumulator {
    records: u64,
    raw_fit_records: u64,
    first_failure: FirstFailurePartition,
    overlapping: OverlappingGateFailures,
    envelope_cv: NumericAccumulator,
    usable_phase_differences: NumericAccumulator,
    phase_concentration: NumericAccumulator,
    low_hz: NumericAccumulator,
    high_hz: NumericAccumulator,
    separation_hz: NumericAccumulator,
    cluster_balance: NumericAccumulator,
    variance_reduction: NumericAccumulator,
    mode_contrast: NumericAccumulator,
    transitions: NumericAccumulator,
}

impl GateBreakdownAccumulator {
    fn increment_first_failure(&mut self, failure: ShapeFirstFailure) {
        match failure {
            ShapeFirstFailure::InsufficientFeature => {
                self.first_failure.insufficient_feature += 1;
            }
            ShapeFirstFailure::EnvelopeCvAboveMaximum => {
                self.first_failure.envelope_cv_above_maximum += 1;
            }
            ShapeFirstFailure::PhaseConcentrationBelowMinimum => {
                *self
                    .first_failure
                    .phase_concentration_below_minimum
                    .get_or_insert(0) += 1;
            }
            ShapeFirstFailure::TwoLevelFitUnavailable => {
                self.first_failure.two_level_fit_unavailable += 1;
            }
            ShapeFirstFailure::ClusterBalanceBelowMinimum => {
                self.first_failure.cluster_balance_below_minimum += 1;
            }
            ShapeFirstFailure::VarianceReductionBelowMinimum => {
                self.first_failure.variance_reduction_below_minimum += 1;
            }
            ShapeFirstFailure::ModeContrastBelowMinimum => {
                self.first_failure.mode_contrast_below_minimum += 1;
            }
            ShapeFirstFailure::SeparationBelowMinimum => {
                self.first_failure.separation_below_minimum += 1;
            }
            ShapeFirstFailure::TransitionsBelowMinimum => {
                self.first_failure.transitions_below_minimum += 1;
            }
            ShapeFirstFailure::AllGatesPassed => {
                self.first_failure.all_gates_passed += 1;
            }
        }
    }

    pub(super) fn observe(&mut self, evaluation: ShapeGateEvaluation) {
        self.records += 1;
        self.increment_first_failure(evaluation.first_failure);
        self.overlapping.envelope_cv_above_maximum +=
            u64::from(evaluation.envelope_cv_above_maximum);
        if let Some(failed) = evaluation.phase_concentration_below_minimum {
            *self
                .overlapping
                .phase_concentration_below_minimum
                .get_or_insert(0) += u64::from(failed);
        }
        self.overlapping.two_level_fit_unavailable +=
            u64::from(evaluation.two_level_fit_unavailable);
        self.overlapping.cluster_balance_below_minimum +=
            u64::from(evaluation.cluster_balance_below_minimum);
        self.overlapping.variance_reduction_below_minimum +=
            u64::from(evaluation.variance_reduction_below_minimum);
        self.overlapping.mode_contrast_below_minimum +=
            u64::from(evaluation.mode_contrast_below_minimum);
        self.overlapping.separation_below_minimum += u64::from(evaluation.separation_below_minimum);
        self.overlapping.transitions_below_minimum +=
            u64::from(evaluation.transitions_below_minimum);
        if let Some(value) = evaluation.envelope_cv {
            self.envelope_cv.observe(value);
        }
        if let Some(value) = evaluation.usable_phase_differences {
            self.usable_phase_differences.observe(value as f64);
        }
        if let Some(value) = evaluation.phase_concentration {
            self.phase_concentration.observe(value);
        }
        if let Some(fit) = evaluation.two_level {
            self.raw_fit_records += 1;
            self.low_hz.observe(fit.centers_hz[0]);
            self.high_hz.observe(fit.centers_hz[1]);
            self.separation_hz.observe(fit.separation_hz);
            self.cluster_balance.observe(fit.cluster_balance);
            self.variance_reduction.observe(fit.variance_reduction);
            self.mode_contrast.observe(fit.mode_contrast);
            self.transitions.observe(fit.transitions as f64);
        }
    }

    pub(super) fn finish(
        mut self,
        expected_records: u64,
        phase_concentration_is_qualification_gate: bool,
        frequency_coordinate: &'static str,
    ) -> Result<GateBreakdownReport> {
        if phase_concentration_is_qualification_gate {
            self.first_failure
                .phase_concentration_below_minimum
                .get_or_insert(0);
            self.overlapping
                .phase_concentration_below_minimum
                .get_or_insert(0);
        } else {
            ensure!(
                self.first_failure
                    .phase_concentration_below_minimum
                    .is_none()
                    && self.overlapping.phase_concentration_below_minimum.is_none(),
                "direct gate accounting unexpectedly used phase concentration"
            );
        }
        ensure!(
            self.records == expected_records
                && self.first_failure.total() == Some(expected_records),
            "gate first-failure partition does not close"
        );
        let feature_eligible_records = self.envelope_cv.observations;
        let feature_insufficient_records = self.first_failure.insufficient_feature;
        ensure!(
            feature_eligible_records.checked_add(feature_insufficient_records)
                == Some(expected_records)
                && self.usable_phase_differences.observations == feature_eligible_records
                && self.phase_concentration.observations == feature_eligible_records
                && self.raw_fit_records <= feature_eligible_records
                && self.first_failure.all_gates_passed <= self.raw_fit_records,
            "gate feature eligibility accounting does not close"
        );
        for observations in [
            self.low_hz.observations,
            self.high_hz.observations,
            self.separation_hz.observations,
            self.cluster_balance.observations,
            self.variance_reduction.observations,
            self.mode_contrast.observations,
            self.transitions.observations,
        ] {
            ensure!(
                observations == self.raw_fit_records,
                "gate raw-fit metric accounting differs"
            );
        }
        let first_failure_order = if phase_concentration_is_qualification_gate {
            CIRCULAR_FIRST_FAILURE_ORDER
        } else {
            DIRECT_FIRST_FAILURE_ORDER
        };
        Ok(GateBreakdownReport {
            frequency_coordinate,
            phase_concentration_is_qualification_gate,
            accounting: GateAccounting {
                control_window_available_records: expected_records,
                feature_eligible_records,
                feature_insufficient_records,
                raw_two_level_fit_records: self.raw_fit_records,
                qualified_two_level_shape_records: self.first_failure.all_gates_passed,
            },
            first_failure_order,
            first_failure_partition: self.first_failure,
            overlapping_gate_failures: self.overlapping,
            feature_summaries: GateFeatureSummaries {
                envelope_cv: self.envelope_cv.finish(),
                usable_phase_differences: self.usable_phase_differences.finish(),
                phase_concentration_diagnostic: self.phase_concentration.finish(),
                raw_two_level_low_hz: self.low_hz.finish(),
                raw_two_level_high_hz: self.high_hz.finish(),
                raw_two_level_separation_hz: self.separation_hz.finish(),
                raw_two_level_cluster_balance: self.cluster_balance.finish(),
                raw_two_level_variance_reduction: self.variance_reduction.finish(),
                raw_two_level_mode_contrast: self.mode_contrast.finish(),
                raw_two_level_transitions: self.transitions.finish(),
            },
            first_failure_partition_is_closed: true,
            overlapping_failures_are_nonexclusive: true,
        })
    }
}
