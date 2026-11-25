//! Aggregate same-stream, bandwidth-matched 2/4 MSps comparison accounting.

use anyhow::{Result, ensure};
use serde::Serialize;

use super::gates::{GateBreakdownAccumulator, GateBreakdownReport};
use super::{NumericAccumulator, NumericSummary};
use crate::alias_control::matched_rate::FilterSettings;
use crate::burst_families::{DirectFeature, direct_shape_gate_evaluation};

const ABSOLUTE_TOLERANCE_HZ: f64 = 20_000.0;
const RELATIVE_TOLERANCE: f64 = 0.05;
const DIRECT_FREQUENCY_COORDINATE: &str = "signed_baseband_hz_relative_to_selection_frequency";

const OUTCOME_ORDER: &[&str] = &[
    "filtered_4msps_feature_insufficient",
    "filtered_4msps_fit_unavailable",
    "filtered_4msps_unqualified",
    "global_even_2msps_feature_insufficient",
    "global_even_2msps_fit_unavailable",
    "global_even_2msps_unqualified",
    "global_odd_2msps_feature_insufficient",
    "global_odd_2msps_fit_unavailable",
    "global_odd_2msps_unqualified",
    "decimation_phase_disagreement",
    "sample_rate_disagreement",
    "all_three_branches_agree",
];

#[derive(Debug, Serialize)]
struct ComparisonSettings {
    absolute_tolerance_hz: f64,
    relative_tolerance: f64,
    comparison_population: &'static str,
    phase_comparison_reference: &'static str,
    sample_rate_comparison_reference: &'static str,
    low_center_high_center_and_separation_must_all_match: bool,
}

#[derive(Debug, Serialize)]
struct MatchedRateAccounting {
    selected_records: u64,
    base_control_available_records: u64,
    base_control_unavailable_records: u64,
    start_context_unavailable_records: u64,
    tail_context_unavailable_records: u64,
    matched_context_available_records: u64,
    filtered_4msps_qualified_records: u64,
    global_even_2msps_qualified_records: u64,
    global_odd_2msps_qualified_records: u64,
    all_three_branches_qualified_records: u64,
}

#[derive(Debug, Default, Serialize)]
struct OutcomePartition {
    filtered_4msps_feature_insufficient: u64,
    filtered_4msps_fit_unavailable: u64,
    filtered_4msps_unqualified: u64,
    global_even_2msps_feature_insufficient: u64,
    global_even_2msps_fit_unavailable: u64,
    global_even_2msps_unqualified: u64,
    global_odd_2msps_feature_insufficient: u64,
    global_odd_2msps_fit_unavailable: u64,
    global_odd_2msps_unqualified: u64,
    decimation_phase_disagreement: u64,
    sample_rate_disagreement: u64,
    all_three_branches_agree: u64,
}

impl OutcomePartition {
    fn total(&self) -> Option<u64> {
        [
            self.filtered_4msps_feature_insufficient,
            self.filtered_4msps_fit_unavailable,
            self.filtered_4msps_unqualified,
            self.global_even_2msps_feature_insufficient,
            self.global_even_2msps_fit_unavailable,
            self.global_even_2msps_unqualified,
            self.global_odd_2msps_feature_insufficient,
            self.global_odd_2msps_fit_unavailable,
            self.global_odd_2msps_unqualified,
            self.decimation_phase_disagreement,
            self.sample_rate_disagreement,
            self.all_three_branches_agree,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
    }

    fn comparable_records(&self) -> Option<u64> {
        self.decimation_phase_disagreement
            .checked_add(self.sample_rate_disagreement)
            .and_then(|count| count.checked_add(self.all_three_branches_agree))
    }
}

#[derive(Debug, Serialize)]
struct SignedFitResidualSummary {
    low_center_hz: NumericSummary,
    high_center_hz: NumericSummary,
    separation_hz: NumericSummary,
}

#[derive(Debug, Default)]
struct SignedFitResidualAccumulator {
    low_center_hz: NumericAccumulator,
    high_center_hz: NumericAccumulator,
    separation_hz: NumericAccumulator,
}

impl SignedFitResidualAccumulator {
    fn observe(&mut self, observed: ComparisonFit, reference: ComparisonFit) {
        self.low_center_hz
            .observe(observed.centers_hz[0] - reference.centers_hz[0]);
        self.high_center_hz
            .observe(observed.centers_hz[1] - reference.centers_hz[1]);
        self.separation_hz
            .observe(observed.separation_hz - reference.separation_hz);
    }

    fn observations_are(&self, expected: u64) -> bool {
        self.low_center_hz.observations == expected
            && self.high_center_hz.observations == expected
            && self.separation_hz.observations == expected
    }

    fn finish(self) -> SignedFitResidualSummary {
        SignedFitResidualSummary {
            low_center_hz: self.low_center_hz.finish(),
            high_center_hz: self.high_center_hz.finish(),
            separation_hz: self.separation_hz.finish(),
        }
    }
}

#[derive(Debug, Serialize)]
struct SignedResidualSummaries {
    global_odd_2msps_minus_global_even_2msps: SignedFitResidualSummary,
    global_even_2msps_minus_filtered_4msps: SignedFitResidualSummary,
    global_odd_2msps_minus_filtered_4msps: SignedFitResidualSummary,
}

#[derive(Debug, Serialize)]
pub(in crate::alias_control) struct MatchedRateReport {
    filter: FilterSettings,
    comparison: ComparisonSettings,
    accounting: MatchedRateAccounting,
    filtered_4msps_direct_gate_breakdown: GateBreakdownReport,
    global_even_2msps_direct_gate_breakdown: GateBreakdownReport,
    global_odd_2msps_direct_gate_breakdown: GateBreakdownReport,
    outcome_order: &'static [&'static str],
    partition_uses_first_outcome_in_declared_order: bool,
    outcome_partition: OutcomePartition,
    outcome_partition_is_closed: bool,
    signed_residual_summaries: SignedResidualSummaries,
    qualified_all_three_branches_invariant_within_tolerance: Option<bool>,
    same_source_samples: bool,
    identical_prefilter: bool,
    blue_dragon_bit_exact_comparison: bool,
}

enum BranchState {
    FeatureInsufficient,
    FitUnavailable,
    Unqualified,
    Qualified(ComparisonFit),
}

#[derive(Clone, Copy)]
struct ComparisonFit {
    centers_hz: [f64; 2],
    separation_hz: f64,
}

fn branch_state(feature: Option<DirectFeature>) -> BranchState {
    let Some(feature) = feature else {
        return BranchState::FeatureInsufficient;
    };
    let Some(fit) = feature.gate_evaluation.two_level else {
        return BranchState::FitUnavailable;
    };
    if feature.gate_evaluation.all_gates_passed {
        BranchState::Qualified(ComparisonFit {
            centers_hz: fit.centers_hz,
            separation_hz: fit.separation_hz,
        })
    } else {
        BranchState::Unqualified
    }
}

fn value_within_tolerance(observed: f64, reference: f64) -> bool {
    let tolerance = ABSOLUTE_TOLERANCE_HZ.max(RELATIVE_TOLERANCE * reference.abs());
    (observed - reference).abs() <= tolerance
}

fn fits_agree(observed: ComparisonFit, reference: ComparisonFit) -> bool {
    value_within_tolerance(observed.centers_hz[0], reference.centers_hz[0])
        && value_within_tolerance(observed.centers_hz[1], reference.centers_hz[1])
        && value_within_tolerance(observed.separation_hz, reference.separation_hz)
}

fn invariant(
    comparable_records: u64,
    phase_disagreements: u64,
    rate_disagreements: u64,
) -> Option<bool> {
    (comparable_records > 0).then_some(phase_disagreements == 0 && rate_disagreements == 0)
}

#[derive(Debug, Default)]
pub(in crate::alias_control) struct MatchedRateAccumulator {
    start_context_unavailable: u64,
    tail_context_unavailable: u64,
    observed: u64,
    filtered_4msps_qualified: u64,
    global_even_2msps_qualified: u64,
    global_odd_2msps_qualified: u64,
    all_three_qualified: u64,
    filtered_4msps_gates: GateBreakdownAccumulator,
    global_even_2msps_gates: GateBreakdownAccumulator,
    global_odd_2msps_gates: GateBreakdownAccumulator,
    outcomes: OutcomePartition,
    odd_minus_even: SignedFitResidualAccumulator,
    even_minus_four: SignedFitResidualAccumulator,
    odd_minus_four: SignedFitResidualAccumulator,
}

impl MatchedRateAccumulator {
    pub(in crate::alias_control) fn observe_start_context_unavailable(&mut self) {
        self.start_context_unavailable += 1;
    }

    pub(in crate::alias_control) fn observe_tail_context_unavailable(&mut self) {
        self.tail_context_unavailable += 1;
    }

    pub(in crate::alias_control) fn observe(
        &mut self,
        filtered_4msps: Option<DirectFeature>,
        global_even_2msps: Option<DirectFeature>,
        global_odd_2msps: Option<DirectFeature>,
    ) {
        self.observed += 1;
        self.filtered_4msps_gates
            .observe(direct_shape_gate_evaluation(filtered_4msps));
        self.global_even_2msps_gates
            .observe(direct_shape_gate_evaluation(global_even_2msps));
        self.global_odd_2msps_gates
            .observe(direct_shape_gate_evaluation(global_odd_2msps));

        let four = branch_state(filtered_4msps);
        let even = branch_state(global_even_2msps);
        let odd = branch_state(global_odd_2msps);
        self.filtered_4msps_qualified += u64::from(matches!(&four, BranchState::Qualified(_)));
        self.global_even_2msps_qualified += u64::from(matches!(&even, BranchState::Qualified(_)));
        self.global_odd_2msps_qualified += u64::from(matches!(&odd, BranchState::Qualified(_)));

        let four = match four {
            BranchState::FeatureInsufficient => {
                self.outcomes.filtered_4msps_feature_insufficient += 1;
                return;
            }
            BranchState::FitUnavailable => {
                self.outcomes.filtered_4msps_fit_unavailable += 1;
                return;
            }
            BranchState::Unqualified => {
                self.outcomes.filtered_4msps_unqualified += 1;
                return;
            }
            BranchState::Qualified(fit) => fit,
        };
        let even = match even {
            BranchState::FeatureInsufficient => {
                self.outcomes.global_even_2msps_feature_insufficient += 1;
                return;
            }
            BranchState::FitUnavailable => {
                self.outcomes.global_even_2msps_fit_unavailable += 1;
                return;
            }
            BranchState::Unqualified => {
                self.outcomes.global_even_2msps_unqualified += 1;
                return;
            }
            BranchState::Qualified(fit) => fit,
        };
        let odd = match odd {
            BranchState::FeatureInsufficient => {
                self.outcomes.global_odd_2msps_feature_insufficient += 1;
                return;
            }
            BranchState::FitUnavailable => {
                self.outcomes.global_odd_2msps_fit_unavailable += 1;
                return;
            }
            BranchState::Unqualified => {
                self.outcomes.global_odd_2msps_unqualified += 1;
                return;
            }
            BranchState::Qualified(fit) => fit,
        };

        self.all_three_qualified += 1;
        self.odd_minus_even.observe(odd, even);
        self.even_minus_four.observe(even, four);
        self.odd_minus_four.observe(odd, four);
        if !fits_agree(odd, even) {
            self.outcomes.decimation_phase_disagreement += 1;
        } else if !fits_agree(even, four) || !fits_agree(odd, four) {
            self.outcomes.sample_rate_disagreement += 1;
        } else {
            self.outcomes.all_three_branches_agree += 1;
        }
    }

    pub(in crate::alias_control) fn finish(
        self,
        selected_records: u64,
        base_available_records: u64,
        filter_settings: FilterSettings,
    ) -> Result<MatchedRateReport> {
        let base_control_unavailable_records = selected_records
            .checked_sub(base_available_records)
            .ok_or_else(|| anyhow::anyhow!("matched-rate base availability exceeds selection"))?;
        let context_accounted = self
            .start_context_unavailable
            .checked_add(self.tail_context_unavailable)
            .and_then(|count| count.checked_add(self.observed));
        ensure!(
            context_accounted == Some(base_available_records),
            "matched-rate context accounting does not close"
        );
        ensure!(
            self.outcomes.total() == Some(self.observed),
            "matched-rate outcome partition does not close"
        );
        let comparable_records = self
            .outcomes
            .comparable_records()
            .ok_or_else(|| anyhow::anyhow!("matched-rate comparable count overflow"))?;
        ensure!(
            comparable_records == self.all_three_qualified
                && self.odd_minus_even.observations_are(comparable_records)
                && self.even_minus_four.observations_are(comparable_records)
                && self.odd_minus_four.observations_are(comparable_records),
            "matched-rate comparison accounting differs"
        );

        let filtered_4msps_direct_gate_breakdown =
            self.filtered_4msps_gates
                .finish(self.observed, false, DIRECT_FREQUENCY_COORDINATE)?;
        let global_even_2msps_direct_gate_breakdown = self.global_even_2msps_gates.finish(
            self.observed,
            false,
            DIRECT_FREQUENCY_COORDINATE,
        )?;
        let global_odd_2msps_direct_gate_breakdown = self.global_odd_2msps_gates.finish(
            self.observed,
            false,
            DIRECT_FREQUENCY_COORDINATE,
        )?;
        ensure!(
            filtered_4msps_direct_gate_breakdown
                .accounting
                .qualified_two_level_shape_records
                == self.filtered_4msps_qualified
                && global_even_2msps_direct_gate_breakdown
                    .accounting
                    .qualified_two_level_shape_records
                    == self.global_even_2msps_qualified
                && global_odd_2msps_direct_gate_breakdown
                    .accounting
                    .qualified_two_level_shape_records
                    == self.global_odd_2msps_qualified
                && self.all_three_qualified <= self.filtered_4msps_qualified
                && self.all_three_qualified <= self.global_even_2msps_qualified
                && self.all_three_qualified <= self.global_odd_2msps_qualified,
            "matched-rate branch qualification accounting differs"
        );

        let qualified_all_three_branches_invariant_within_tolerance = invariant(
            comparable_records,
            self.outcomes.decimation_phase_disagreement,
            self.outcomes.sample_rate_disagreement,
        );
        Ok(MatchedRateReport {
            filter: filter_settings,
            comparison: ComparisonSettings {
                absolute_tolerance_hz: ABSOLUTE_TOLERANCE_HZ,
                relative_tolerance: RELATIVE_TOLERANCE,
                comparison_population: "records_with_all_three_branches_qualified",
                phase_comparison_reference: "global_even_2msps",
                sample_rate_comparison_reference: "filtered_4msps",
                low_center_high_center_and_separation_must_all_match: true,
            },
            accounting: MatchedRateAccounting {
                selected_records,
                base_control_available_records: base_available_records,
                base_control_unavailable_records,
                start_context_unavailable_records: self.start_context_unavailable,
                tail_context_unavailable_records: self.tail_context_unavailable,
                matched_context_available_records: self.observed,
                filtered_4msps_qualified_records: self.filtered_4msps_qualified,
                global_even_2msps_qualified_records: self.global_even_2msps_qualified,
                global_odd_2msps_qualified_records: self.global_odd_2msps_qualified,
                all_three_branches_qualified_records: self.all_three_qualified,
            },
            filtered_4msps_direct_gate_breakdown,
            global_even_2msps_direct_gate_breakdown,
            global_odd_2msps_direct_gate_breakdown,
            outcome_order: OUTCOME_ORDER,
            partition_uses_first_outcome_in_declared_order: true,
            outcome_partition: self.outcomes,
            outcome_partition_is_closed: true,
            signed_residual_summaries: SignedResidualSummaries {
                global_odd_2msps_minus_global_even_2msps: self.odd_minus_even.finish(),
                global_even_2msps_minus_filtered_4msps: self.even_minus_four.finish(),
                global_odd_2msps_minus_filtered_4msps: self.odd_minus_four.finish(),
            },
            qualified_all_three_branches_invariant_within_tolerance,
            same_source_samples: true,
            identical_prefilter: true,
            blue_dragon_bit_exact_comparison: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alias_control::matched_rate::filter_settings;
    use crate::burst_families::{ShapeFirstFailure, ShapeGateEvaluation, TwoLevelFeature};

    fn fit(low_hz: f64, high_hz: f64) -> TwoLevelFeature {
        TwoLevelFeature {
            centers_hz: [low_hz, high_hz],
            separation_hz: high_hz - low_hz,
            variance_reduction: 0.9,
            mode_contrast: 8.0,
            cluster_balance: 0.45,
            transitions: 20,
        }
    }

    fn direct(raw_fit: Option<TwoLevelFeature>, qualified: bool) -> DirectFeature {
        DirectFeature {
            two_level: raw_fit,
            gate_evaluation: ShapeGateEvaluation {
                envelope_cv: Some(if qualified || raw_fit.is_none() {
                    0.1
                } else {
                    0.3
                }),
                usable_phase_differences: Some(512),
                phase_concentration: Some(0.5),
                two_level: raw_fit,
                first_failure: if qualified {
                    ShapeFirstFailure::AllGatesPassed
                } else if raw_fit.is_some() {
                    ShapeFirstFailure::EnvelopeCvAboveMaximum
                } else {
                    ShapeFirstFailure::TwoLevelFitUnavailable
                },
                envelope_cv_above_maximum: !qualified && raw_fit.is_some(),
                phase_concentration_below_minimum: None,
                two_level_fit_unavailable: raw_fit.is_none(),
                cluster_balance_below_minimum: false,
                variance_reduction_below_minimum: false,
                mode_contrast_below_minimum: false,
                separation_below_minimum: false,
                transitions_below_minimum: false,
                all_gates_passed: qualified,
            },
        }
    }

    #[test]
    fn tolerance_combines_an_absolute_floor_with_a_relative_bound() {
        assert!(value_within_tolerance(19_999.0, 0.0));
        assert!(!value_within_tolerance(20_001.0, 0.0));
        assert!(value_within_tolerance(1_049_999.0, 1_000_000.0));
        assert!(!value_within_tolerance(1_050_001.0, 1_000_000.0));
    }

    #[test]
    fn outcome_partition_is_closed_and_comparisons_have_signed_residuals() {
        let reference = fit(-470_000.0, 470_000.0);
        let phase_shifted = fit(-440_000.0, 500_000.0);
        let rate_shifted = fit(-440_000.0, 470_000.0);
        let mut accumulator = MatchedRateAccumulator::default();
        accumulator.observe(None, Some(direct(Some(reference), true)), None);
        accumulator.observe(
            Some(direct(None, false)),
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), false)),
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            None,
            Some(direct(Some(reference), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(None, false)),
            Some(direct(Some(reference), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), false)),
            Some(direct(Some(reference), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
            None,
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
            Some(direct(None, false)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), false)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
            Some(direct(Some(phase_shifted), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(rate_shifted), true)),
            Some(direct(Some(rate_shifted), true)),
        );
        accumulator.observe(
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
            Some(direct(Some(reference), true)),
        );

        assert_eq!(accumulator.outcomes.total(), Some(12));
        assert_eq!(accumulator.outcomes.filtered_4msps_feature_insufficient, 1);
        assert_eq!(accumulator.outcomes.filtered_4msps_fit_unavailable, 1);
        assert_eq!(accumulator.outcomes.filtered_4msps_unqualified, 1);
        assert_eq!(
            accumulator.outcomes.global_even_2msps_feature_insufficient,
            1
        );
        assert_eq!(accumulator.outcomes.global_even_2msps_fit_unavailable, 1);
        assert_eq!(accumulator.outcomes.global_even_2msps_unqualified, 1);
        assert_eq!(
            accumulator.outcomes.global_odd_2msps_feature_insufficient,
            1
        );
        assert_eq!(accumulator.outcomes.global_odd_2msps_fit_unavailable, 1);
        assert_eq!(accumulator.outcomes.global_odd_2msps_unqualified, 1);
        assert_eq!(accumulator.outcomes.decimation_phase_disagreement, 1);
        assert_eq!(accumulator.outcomes.sample_rate_disagreement, 1);
        assert_eq!(accumulator.outcomes.all_three_branches_agree, 1);
        assert_eq!(accumulator.outcomes.comparable_records(), Some(3));
        assert!(accumulator.odd_minus_even.observations_are(3));
        assert_eq!(invariant(3, 1, 1), Some(false));
        assert_eq!(invariant(0, 0, 0), None);

        let report = accumulator.finish(12, 12, filter_settings()).unwrap();
        assert_eq!(report.outcome_partition.total(), Some(12));
        assert_eq!(
            report
                .signed_residual_summaries
                .global_odd_2msps_minus_global_even_2msps
                .low_center_hz
                .mean,
            Some(10_000.0)
        );
        assert_eq!(
            report
                .signed_residual_summaries
                .global_even_2msps_minus_filtered_4msps
                .separation_hz
                .mean,
            Some(-10_000.0)
        );
        assert_eq!(
            report
                .signed_residual_summaries
                .global_odd_2msps_minus_filtered_4msps
                .low_center_hz
                .mean,
            Some(20_000.0)
        );
        assert_eq!(
            report.qualified_all_three_branches_invariant_within_tolerance,
            Some(false)
        );
    }

    #[test]
    fn finish_checks_context_closure_and_keeps_an_empty_comparison_nullable() {
        let mut accumulator = MatchedRateAccumulator::default();
        accumulator.observe_start_context_unavailable();
        let report = accumulator.finish(2, 1, filter_settings()).unwrap();
        assert_eq!(report.accounting.selected_records, 2);
        assert_eq!(report.accounting.base_control_unavailable_records, 1);
        assert_eq!(report.accounting.start_context_unavailable_records, 1);
        assert_eq!(report.accounting.matched_context_available_records, 0);
        assert_eq!(
            report.qualified_all_three_branches_invariant_within_tolerance,
            None
        );

        assert!(
            MatchedRateAccumulator::default()
                .finish(1, 1, filter_settings())
                .is_err()
        );
    }
}
