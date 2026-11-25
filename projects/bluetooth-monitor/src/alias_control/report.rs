//! Alias-control report schema and aggregate accounting.

mod gates;
mod matched;

use anyhow::{Result, ensure};
use serde::Serialize;

use super::{CONTROL_SAMPLE_RATE_HZ, ROTATION_FREQUENCIES_HZ};
use crate::burst_families::{
    DirectFeature, FeatureSettings, RecordFeature, circular_shape_gate_evaluation,
    direct_shape_gate_evaluation, two_level_shape_qualified,
};
use crate::bursts::CHANNEL_SAMPLE_RATE_HZ;

use self::gates::{GateBreakdownAccumulator, GateBreakdownReport};
pub(super) use self::matched::{MatchedRateAccumulator, MatchedRateReport};
use super::provenance::{ContentIdentity, DecoderProgram};

#[cfg(test)]
use crate::burst_families::{ShapeFirstFailure, ShapeGateEvaluation};

pub(super) const HYPOTHESIS_RELATIVE_TOLERANCE: f64 = 0.05;

#[derive(Clone, Debug, Default, Serialize)]
struct NumericSummary {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
}

#[derive(Debug, Default)]
struct NumericAccumulator {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
}

impl NumericAccumulator {
    fn observe(&mut self, value: f64) {
        debug_assert!(value.is_finite());
        self.observations += 1;
        self.min = Some(self.min.map_or(value, |current| current.min(value)));
        self.max = Some(self.max.map_or(value, |current| current.max(value)));
        self.sum += value;
    }

    fn finish(self) -> NumericSummary {
        NumericSummary {
            observations: self.observations,
            min: self.min,
            max: self.max,
            mean: (self.observations > 0).then_some(self.sum / self.observations as f64),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub(super) struct BranchControlReport {
    transformations_per_record: u64,
    evaluated_record_transformations: u64,
    baseline_fit_record_transformations: u64,
    separation_comparisons: u64,
    fit_availability_changes: u64,
    maximum_relative_separation_change: Option<f64>,
    invariant_within_one_percent: Option<bool>,
}

#[derive(Debug, Default)]
pub(super) struct BranchControlAccumulator {
    evaluated: u64,
    baseline_fits: u64,
    comparisons: u64,
    availability_changes: u64,
    maximum_relative_change: Option<f64>,
}

impl BranchControlAccumulator {
    pub(super) fn compare(
        &mut self,
        baseline: Option<RecordFeature>,
        candidate: Option<RecordFeature>,
    ) {
        self.evaluated += 1;
        let baseline_fit = baseline.and_then(|feature| feature.two_level);
        let candidate_fit = candidate.and_then(|feature| feature.two_level);
        self.baseline_fits += u64::from(baseline_fit.is_some());
        if baseline_fit.is_some() != candidate_fit.is_some() {
            self.availability_changes += 1;
        }
        if let (Some(baseline), Some(candidate)) = (baseline_fit, candidate_fit) {
            self.comparisons += 1;
            let change = (candidate.separation_hz - baseline.separation_hz).abs()
                / baseline.separation_hz.max(f64::EPSILON);
            self.maximum_relative_change = Some(
                self.maximum_relative_change
                    .map_or(change, |current| current.max(change)),
            );
        }
    }

    pub(super) fn finish(self) -> BranchControlReport {
        let invariant = if self.baseline_fits == 0 && self.availability_changes == 0 {
            None
        } else {
            Some(
                self.availability_changes == 0
                    && self.comparisons > 0
                    && self
                        .maximum_relative_change
                        .is_some_and(|change| change <= 0.01),
            )
        };
        BranchControlReport {
            transformations_per_record: ROTATION_FREQUENCIES_HZ.len() as u64 + 1,
            evaluated_record_transformations: self.evaluated,
            baseline_fit_record_transformations: self.baseline_fits,
            separation_comparisons: self.comparisons,
            fit_availability_changes: self.availability_changes,
            maximum_relative_separation_change: self.maximum_relative_change,
            invariant_within_one_percent: invariant,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct FourRateAccumulator {
    circular_gate_breakdown: GateBreakdownAccumulator,
    direct_gate_breakdown: GateBreakdownAccumulator,
    baseline_fits: u64,
    baseline_qualified_fits: u64,
    eligible: u64,
    circular_fits: u64,
    circular_qualified_fits: u64,
    diagnostic_direct_fits: u64,
    circular_separation: NumericAccumulator,
    diagnostic_direct_low_center: NumericAccumulator,
    diagnostic_direct_high_center: NumericAccumulator,
    diagnostic_direct_separation: NumericAccumulator,
    qualified_direct_fits: u64,
    qualified_direct_low_center: NumericAccumulator,
    qualified_direct_high_center: NumericAccumulator,
    qualified_direct_separation: NumericAccumulator,
    qualified_direct_at_or_below_2mhz: u64,
    qualified_direct_above_2mhz: u64,
    branch_spanning_alias_candidates: u64,
    baseline_separation: NumericAccumulator,
    paired_fits: u64,
    paired_hypothesis_observed: u64,
    paired_hypothesis_2msps_complement: u64,
    paired_hypothesis_2msps_plus: u64,
    paired_hypothesis_4msps_branch: u64,
    paired_multiple_hypotheses: u64,
    paired_no_hypothesis: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct FourRateReport {
    selected_records: u64,
    control_window_available_records: u64,
    circular_recentered_gate_breakdown: GateBreakdownReport,
    direct_unrecentered_gate_breakdown: GateBreakdownReport,
    baseline_2msps_fit_records: u64,
    baseline_2msps_qualified_two_level_shape_records: u64,
    baseline_2msps_separation_hz: NumericSummary,
    feature_eligible_records: u64,
    circular_recentered_fit_records: u64,
    circular_recentered_qualified_two_level_shape_records: u64,
    circular_recentered_separation_hz: NumericSummary,
    direct_unrecentered_diagnostic_fit_records: u64,
    direct_unrecentered_diagnostic_low_baseband_hz: NumericSummary,
    direct_unrecentered_diagnostic_high_baseband_hz: NumericSummary,
    direct_unrecentered_diagnostic_separation_hz: NumericSummary,
    qualified_direct_two_level_shape_records: u64,
    qualified_direct_low_baseband_hz: NumericSummary,
    qualified_direct_high_baseband_hz: NumericSummary,
    qualified_direct_separation_hz: NumericSummary,
    qualified_direct_separation_at_or_below_2mhz_records: u64,
    qualified_direct_separation_above_2mhz_records: u64,
    qualified_branch_spanning_alias_candidate_records: u64,
    qualified_branch_spanning_alias_candidate_observed: bool,
    paired_qualified_fit_records: u64,
    paired_hypothesis_observed_match_records: u64,
    paired_hypothesis_2msps_complement_match_records: u64,
    paired_hypothesis_2msps_plus_observed_match_records: u64,
    paired_hypothesis_4msps_branch_fold_match_records: u64,
    paired_multiple_hypothesis_match_records: u64,
    paired_no_hypothesis_match_records: u64,
    paired_hypothesis_matches_are_nonexclusive: bool,
    paired_comparison_filter_sensitive_inconclusive: bool,
    alias_candidate_requires_qualified_circular_fold_match: bool,
}

fn within_relative_tolerance(left: f64, right: f64) -> bool {
    (left - right).abs() / right.abs().max(f64::EPSILON) <= HYPOTHESIS_RELATIVE_TOLERANCE
}

fn paired_hypothesis_matches(baseline_separation_hz: f64, direct_separation_hz: f64) -> [bool; 4] {
    let two_rate = f64::from(CHANNEL_SAMPLE_RATE_HZ);
    let four_rate = f64::from(CONTROL_SAMPLE_RATE_HZ);
    [
        within_relative_tolerance(direct_separation_hz, baseline_separation_hz),
        within_relative_tolerance(direct_separation_hz, two_rate - baseline_separation_hz),
        within_relative_tolerance(direct_separation_hz, two_rate + baseline_separation_hz),
        within_relative_tolerance(direct_separation_hz, four_rate - baseline_separation_hz),
    ]
}

impl FourRateAccumulator {
    pub(super) fn observe(
        &mut self,
        baseline: Option<RecordFeature>,
        circular: Option<RecordFeature>,
        direct: Option<DirectFeature>,
    ) {
        self.circular_gate_breakdown
            .observe(circular_shape_gate_evaluation(circular));
        self.direct_gate_breakdown
            .observe(direct_shape_gate_evaluation(direct));
        let baseline_fit = baseline.and_then(|feature| feature.two_level);
        if let Some(fit) = baseline_fit {
            self.baseline_fits += 1;
            self.baseline_separation.observe(fit.separation_hz);
        }
        let baseline_qualified_fit = baseline
            .filter(|feature| {
                feature.two_level.is_some_and(|fit| {
                    two_level_shape_qualified(feature.envelope_cv, feature.phase_concentration, fit)
                })
            })
            .and_then(|feature| feature.two_level);
        self.baseline_qualified_fits += u64::from(baseline_qualified_fit.is_some());
        let circular_qualified_fit = circular
            .filter(|feature| {
                feature.two_level.is_some_and(|fit| {
                    two_level_shape_qualified(feature.envelope_cv, feature.phase_concentration, fit)
                })
            })
            .and_then(|feature| feature.two_level);
        self.circular_qualified_fits += u64::from(circular_qualified_fit.is_some());
        if let Some(circular) = circular {
            self.eligible += 1;
            if let Some(fit) = circular.two_level {
                self.circular_fits += 1;
                self.circular_separation.observe(fit.separation_hz);
            }
        }
        let diagnostic_direct_fit = direct.and_then(|feature| feature.two_level);
        if let Some(fit) = diagnostic_direct_fit {
            self.diagnostic_direct_fits += 1;
            self.diagnostic_direct_low_center.observe(fit.centers_hz[0]);
            self.diagnostic_direct_high_center
                .observe(fit.centers_hz[1]);
            self.diagnostic_direct_separation.observe(fit.separation_hz);
        }
        let qualified_direct_fit = direct
            .filter(|feature| feature.gate_evaluation.all_gates_passed)
            .and_then(|feature| feature.two_level);
        if let Some(fit) = qualified_direct_fit {
            self.qualified_direct_fits += 1;
            self.qualified_direct_low_center.observe(fit.centers_hz[0]);
            self.qualified_direct_high_center.observe(fit.centers_hz[1]);
            self.qualified_direct_separation.observe(fit.separation_hz);
            if fit.separation_hz > f64::from(CONTROL_SAMPLE_RATE_HZ) / 2.0 {
                self.qualified_direct_above_2mhz += 1;
                if circular_qualified_fit.is_some_and(|circular| {
                    within_relative_tolerance(
                        f64::from(CONTROL_SAMPLE_RATE_HZ) - fit.separation_hz,
                        circular.separation_hz,
                    )
                }) {
                    self.branch_spanning_alias_candidates += 1;
                }
            } else {
                self.qualified_direct_at_or_below_2mhz += 1;
            }
        }
        if let (Some(baseline), Some(direct)) = (baseline_qualified_fit, qualified_direct_fit) {
            self.paired_fits += 1;
            let matches = paired_hypothesis_matches(baseline.separation_hz, direct.separation_hz);
            self.paired_hypothesis_observed += u64::from(matches[0]);
            self.paired_hypothesis_2msps_complement += u64::from(matches[1]);
            self.paired_hypothesis_2msps_plus += u64::from(matches[2]);
            self.paired_hypothesis_4msps_branch += u64::from(matches[3]);
            match matches.into_iter().filter(|matched| *matched).count() {
                0 => self.paired_no_hypothesis += 1,
                1 => {}
                _ => self.paired_multiple_hypotheses += 1,
            }
        }
    }

    pub(super) fn finish(
        self,
        selected_records: u64,
        control_window_available_records: u64,
    ) -> Result<FourRateReport> {
        let circular_gate_breakdown = self.circular_gate_breakdown.finish(
            control_window_available_records,
            true,
            "circular_recentered_hz",
        )?;
        let direct_gate_breakdown = self.direct_gate_breakdown.finish(
            control_window_available_records,
            false,
            "signed_baseband_hz_relative_to_selection_frequency",
        )?;
        ensure!(
            circular_gate_breakdown
                .accounting
                .qualified_two_level_shape_records
                == self.circular_qualified_fits
                && circular_gate_breakdown.accounting.feature_eligible_records == self.eligible
                && circular_gate_breakdown.accounting.raw_two_level_fit_records
                    == self.circular_fits
                && direct_gate_breakdown
                    .accounting
                    .qualified_two_level_shape_records
                    == self.qualified_direct_fits
                && self.diagnostic_direct_fits
                    <= direct_gate_breakdown.accounting.raw_two_level_fit_records
                && self
                    .qualified_direct_at_or_below_2mhz
                    .checked_add(self.qualified_direct_above_2mhz)
                    == Some(self.qualified_direct_fits)
                && self.branch_spanning_alias_candidates <= self.qualified_direct_above_2mhz,
            "4 MSps qualification accounting differs"
        );
        Ok(FourRateReport {
            selected_records,
            control_window_available_records,
            circular_recentered_gate_breakdown: circular_gate_breakdown,
            direct_unrecentered_gate_breakdown: direct_gate_breakdown,
            baseline_2msps_fit_records: self.baseline_fits,
            baseline_2msps_qualified_two_level_shape_records: self.baseline_qualified_fits,
            baseline_2msps_separation_hz: self.baseline_separation.finish(),
            feature_eligible_records: self.eligible,
            circular_recentered_fit_records: self.circular_fits,
            circular_recentered_qualified_two_level_shape_records: self.circular_qualified_fits,
            circular_recentered_separation_hz: self.circular_separation.finish(),
            direct_unrecentered_diagnostic_fit_records: self.diagnostic_direct_fits,
            direct_unrecentered_diagnostic_low_baseband_hz: self
                .diagnostic_direct_low_center
                .finish(),
            direct_unrecentered_diagnostic_high_baseband_hz: self
                .diagnostic_direct_high_center
                .finish(),
            direct_unrecentered_diagnostic_separation_hz: self
                .diagnostic_direct_separation
                .finish(),
            qualified_direct_two_level_shape_records: self.qualified_direct_fits,
            qualified_direct_low_baseband_hz: self.qualified_direct_low_center.finish(),
            qualified_direct_high_baseband_hz: self.qualified_direct_high_center.finish(),
            qualified_direct_separation_hz: self.qualified_direct_separation.finish(),
            qualified_direct_separation_at_or_below_2mhz_records: self
                .qualified_direct_at_or_below_2mhz,
            qualified_direct_separation_above_2mhz_records: self.qualified_direct_above_2mhz,
            qualified_branch_spanning_alias_candidate_records: self
                .branch_spanning_alias_candidates,
            qualified_branch_spanning_alias_candidate_observed: self
                .branch_spanning_alias_candidates
                > 0,
            paired_qualified_fit_records: self.paired_fits,
            paired_hypothesis_observed_match_records: self.paired_hypothesis_observed,
            paired_hypothesis_2msps_complement_match_records: self
                .paired_hypothesis_2msps_complement,
            paired_hypothesis_2msps_plus_observed_match_records: self.paired_hypothesis_2msps_plus,
            paired_hypothesis_4msps_branch_fold_match_records: self.paired_hypothesis_4msps_branch,
            paired_multiple_hypothesis_match_records: self.paired_multiple_hypotheses,
            paired_no_hypothesis_match_records: self.paired_no_hypothesis,
            paired_hypothesis_matches_are_nonexclusive: true,
            paired_comparison_filter_sensitive_inconclusive: true,
            alias_candidate_requires_qualified_circular_fold_match: true,
        })
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Report {
    pub(super) schema_version: u32,
    pub(super) operation: &'static str,
    pub(super) burst_source: ContentIdentity,
    pub(super) cf32_source: ContentIdentity,
    pub(super) frequency_manifest: ContentIdentity,
    pub(super) replay_manifest: ContentIdentity,
    pub(super) replay_attested_decoder_program: ReplayAttestedDecoderProgramReport,
    pub(super) manifest_attested_acquisition_sidecar: ManifestAttestedSidecarReport,
    pub(super) manifest_attested_source_prefix: ManifestAttestedSourcePrefixReport,
    pub(super) selection: SelectionReport,
    pub(super) mapping: MappingReport,
    pub(super) settings: SettingsReport,
    pub(super) branch_control_2msps: BranchControlReport,
    pub(super) alias_control_4msps: FourRateReport,
    pub(super) bandwidth_matched_same_stream: MatchedRateReport,
    pub(super) evidence_limited: bool,
    pub(super) protocol_identity_established: bool,
    pub(super) interpretation: &'static str,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingsReport {
    pub(super) source_sample_rate_hz: u64,
    pub(super) bdb_sample_rate_hz: u32,
    pub(super) control_sample_rate_hz: u32,
    pub(super) rotation_frequencies_hz: [f64; 8],
    pub(super) conjugation_control: bool,
    pub(super) direct_frequency_reference: &'static str,
    pub(super) hypothesis_relative_tolerance: f64,
    pub(super) feature_algorithm_version: u32,
    pub(super) features: FeatureSettings,
}

#[derive(Debug, Serialize)]
pub(super) struct ReplayAttestedDecoderProgramReport {
    pub(super) schema_version: u32,
    pub(super) name: String,
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) independently_verified: bool,
}

impl From<DecoderProgram> for ReplayAttestedDecoderProgramReport {
    fn from(program: DecoderProgram) -> Self {
        Self {
            schema_version: program.schema_version,
            name: program.name,
            bytes: program.bytes,
            sha256: program.sha256,
            independently_verified: false,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ManifestAttestedSidecarReport {
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) independently_verified: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ManifestAttestedSourcePrefixReport {
    pub(super) format: String,
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) processed_complex_samples: u64,
    pub(super) independently_verified: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct SelectionReport {
    pub(super) family: &'static str,
    pub(super) frequency_mhz: u32,
    pub(super) group_count: u64,
    pub(super) record_count: u64,
    pub(super) complex_sample_count_2msps: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct MappingReport {
    pub(super) blue_dragon_revision: &'static str,
    pub(super) blue_dragon_revision_verified_from_replay: bool,
    pub(super) alignment_validation_kind: &'static str,
    pub(super) exact_timestamp_grid_validated: bool,
    pub(super) bdb_to_cf32_output_offset: u64,
    pub(super) cf32_samples_per_bdb_sample: u64,
    pub(super) frequency_channelizer_stride_source_samples: usize,
    pub(super) frequency_channelizer_corrected_origin_source_samples: i64,
    pub(super) selected_windows: u64,
    pub(super) valid_control_windows: u64,
    pub(super) startup_transient_windows: u64,
    pub(super) nonflushed_tail_windows: u64,
    pub(super) all_selected_windows_within_valid_extent: bool,
    pub(super) edr_preroll_exclusion_validated: bool,
    pub(super) pfb_burst_catcher_integration_fixture_executed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burst_families::TwoLevelFeature;

    fn fit(low_hz: f64, high_hz: f64) -> TwoLevelFeature {
        TwoLevelFeature {
            centers_hz: [low_hz, high_hz],
            separation_hz: high_hz - low_hz,
            variance_reduction: 0.99,
            mode_contrast: 20.0,
            cluster_balance: 0.5,
            transitions: 20,
        }
    }

    fn circular_record(fit: TwoLevelFeature) -> RecordFeature {
        RecordFeature {
            envelope_cv: 0.01,
            usable_phase_differences: 1024,
            phase_concentration: 0.5,
            two_level: Some(fit),
            fsk_like: false,
            constant_envelope: true,
        }
    }

    fn direct_record(fit: TwoLevelFeature) -> DirectFeature {
        DirectFeature {
            two_level: Some(fit),
            gate_evaluation: ShapeGateEvaluation {
                envelope_cv: Some(0.01),
                usable_phase_differences: Some(1024),
                phase_concentration: Some(0.5),
                two_level: Some(fit),
                first_failure: ShapeFirstFailure::AllGatesPassed,
                envelope_cv_above_maximum: false,
                phase_concentration_below_minimum: None,
                two_level_fit_unavailable: false,
                cluster_balance_below_minimum: false,
                variance_reduction_below_minimum: false,
                mode_contrast_below_minimum: false,
                separation_below_minimum: false,
                transitions_below_minimum: false,
                all_gates_passed: true,
            },
        }
    }

    #[test]
    fn paired_hypotheses_preserve_overlaps_and_degeneracy() {
        assert_eq!(
            paired_hypothesis_matches(940_000.0, 940_000.0),
            [true, false, false, false]
        );
        assert_eq!(
            paired_hypothesis_matches(940_000.0, 1_060_000.0),
            [false, true, false, false]
        );
        assert_eq!(
            paired_hypothesis_matches(940_000.0, 2_940_000.0),
            [false, false, true, true]
        );
        assert_eq!(
            paired_hypothesis_matches(940_000.0, 3_060_000.0),
            [false, false, true, true]
        );
        assert_eq!(
            paired_hypothesis_matches(1_000_000.0, 1_000_000.0),
            [true, true, false, false]
        );
        assert_eq!(
            paired_hypothesis_matches(1_000_000.0, 3_000_000.0),
            [false, false, true, true]
        );
    }

    #[test]
    fn branch_invariance_is_unsupported_without_a_baseline_fit() {
        let mut accumulator = BranchControlAccumulator::default();
        accumulator.compare(None, None);
        let report = accumulator.finish();
        assert_eq!(report.separation_comparisons, 0);
        assert_eq!(report.invariant_within_one_percent, None);
    }

    #[test]
    fn gate_breakdown_finish_rejects_an_unobserved_available_window() {
        assert!(FourRateAccumulator::default().finish(1, 1).is_err());
    }

    #[test]
    fn alias_candidate_requires_a_qualified_circular_fold() {
        let near_branch = direct_record(fit(-1_990_000.0, 1_990_000.0));
        let mut negative = FourRateAccumulator::default();
        negative.observe(
            None,
            Some(circular_record(fit(-10_000.0, 10_000.0))),
            Some(near_branch),
        );
        let negative = negative.finish(1, 1).unwrap();
        assert_eq!(negative.qualified_direct_separation_above_2mhz_records, 1);
        assert_eq!(
            negative.qualified_branch_spanning_alias_candidate_records,
            0
        );

        let branch_pair = direct_record(fit(-1_530_000.0, 1_530_000.0));
        let mut positive = FourRateAccumulator::default();
        positive.observe(
            None,
            Some(circular_record(fit(-470_000.0, 470_000.0))),
            Some(branch_pair),
        );
        let positive = positive.finish(1, 1).unwrap();
        assert_eq!(
            positive.qualified_branch_spanning_alias_candidate_records,
            1
        );
        assert_eq!(
            positive
                .circular_recentered_gate_breakdown
                .first_failure_partition
                .all_gates_passed,
            1
        );
        assert_eq!(
            positive
                .direct_unrecentered_gate_breakdown
                .first_failure_partition
                .all_gates_passed,
            1
        );
    }
}
