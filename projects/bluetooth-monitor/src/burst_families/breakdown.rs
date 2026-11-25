//! Bounded feature-gate diagnostics for one classified family/frequency row.

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::{
    Event, Family, FeatureAccumulators, FeatureSummaries, FrequencyAggregate,
    HIGHEST_FREQUENCY_MHZ, LOWEST_FREQUENCY_MHZ, RecordFeature, Settings, SourceIdentity,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Selection {
    family: Family,
    frequency_mhz: u32,
}

impl Selection {
    pub(super) fn new(family: Family, frequency_mhz: u32) -> Result<Self> {
        ensure!(
            (LOWEST_FREQUENCY_MHZ..=HIGHEST_FREQUENCY_MHZ).contains(&frequency_mhz),
            "feature-breakdown frequency must be within {LOWEST_FREQUENCY_MHZ}..={HIGHEST_FREQUENCY_MHZ} MHz"
        );
        Ok(Self {
            family,
            frequency_mhz,
        })
    }

    pub(super) fn family(self) -> Family {
        self.family
    }

    pub(super) fn frequency_mhz(self) -> u32 {
        self.frequency_mhz
    }
}

#[derive(Clone, Debug, Default, Serialize)]
struct FirstFailurePartition {
    insufficient_feature: u64,
    envelope_cv_above_maximum: u64,
    two_level_fit_unavailable: u64,
    cluster_balance_below_minimum: u64,
    variance_reduction_below_minimum: u64,
    mode_contrast_below_minimum: u64,
    separation_below_minimum: u64,
    separation_above_maximum: u64,
    transitions_below_minimum: u64,
    all_gates_passed: u64,
}

impl FirstFailurePartition {
    fn increment(counter: &mut u64) -> Result<()> {
        *counter = counter.checked_add(1).context("gate count overflow")?;
        Ok(())
    }

    fn observe(&mut self, feature: Option<RecordFeature>) -> Result<()> {
        let Some(feature) = feature else {
            return Self::increment(&mut self.insufficient_feature);
        };
        if !feature.constant_envelope {
            return Self::increment(&mut self.envelope_cv_above_maximum);
        }
        let Some(fit) = feature.two_level else {
            return Self::increment(&mut self.two_level_fit_unavailable);
        };
        if fit.cluster_balance < super::MIN_CLUSTER_BALANCE {
            return Self::increment(&mut self.cluster_balance_below_minimum);
        }
        if fit.variance_reduction < super::MIN_VARIANCE_REDUCTION {
            return Self::increment(&mut self.variance_reduction_below_minimum);
        }
        if fit.mode_contrast < super::MIN_MODE_CONTRAST {
            return Self::increment(&mut self.mode_contrast_below_minimum);
        }
        if fit.separation_hz < super::MIN_SEPARATION_HZ {
            return Self::increment(&mut self.separation_below_minimum);
        }
        if fit.separation_hz > super::MAX_SEPARATION_HZ {
            return Self::increment(&mut self.separation_above_maximum);
        }
        if fit.transitions < super::MIN_TRANSITIONS {
            return Self::increment(&mut self.transitions_below_minimum);
        }
        debug_assert!(feature.fsk_like);
        Self::increment(&mut self.all_gates_passed)
    }

    fn checked_total(&self) -> Option<u64> {
        [
            self.insufficient_feature,
            self.envelope_cv_above_maximum,
            self.two_level_fit_unavailable,
            self.cluster_balance_below_minimum,
            self.variance_reduction_below_minimum,
            self.mode_contrast_below_minimum,
            self.separation_below_minimum,
            self.separation_above_maximum,
            self.transitions_below_minimum,
            self.all_gates_passed,
        ]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
    }
}

#[derive(Debug)]
pub(super) struct Accumulator {
    selection: Selection,
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
    feature_eligible_record_count: u64,
    feature_insufficient_record_count: u64,
    two_level_fit_record_count: u64,
    first_failure_partition: FirstFailurePartition,
    features: FeatureAccumulators,
}

impl Accumulator {
    pub(super) fn new(selection: Selection) -> Self {
        Self {
            selection,
            group_incidence_count: 0,
            record_count: 0,
            complex_sample_count: 0,
            feature_eligible_record_count: 0,
            feature_insufficient_record_count: 0,
            two_level_fit_record_count: 0,
            first_failure_partition: FirstFailurePartition::default(),
            features: FeatureAccumulators::default(),
        }
    }

    pub(super) fn observe_group(&mut self, family: Family, events: &[Event]) -> Result<()> {
        if family != self.selection.family
            || !events
                .iter()
                .any(|event| event.frequency_mhz == self.selection.frequency_mhz)
        {
            return Ok(());
        }
        self.group_incidence_count = self
            .group_incidence_count
            .checked_add(1)
            .context("feature-breakdown group count overflow")?;
        for event in events
            .iter()
            .filter(|event| event.frequency_mhz == self.selection.frequency_mhz)
        {
            self.record_count = self
                .record_count
                .checked_add(1)
                .context("feature-breakdown record count overflow")?;
            self.complex_sample_count = self
                .complex_sample_count
                .checked_add(event.sample_count)
                .context("feature-breakdown sample count overflow")?;
            if let Some(feature) = event.feature {
                self.feature_eligible_record_count = self
                    .feature_eligible_record_count
                    .checked_add(1)
                    .context("feature-breakdown eligible count overflow")?;
                if feature.two_level.is_some() {
                    self.two_level_fit_record_count = self
                        .two_level_fit_record_count
                        .checked_add(1)
                        .context("feature-breakdown fit count overflow")?;
                }
                self.features.observe(feature);
            } else {
                self.feature_insufficient_record_count = self
                    .feature_insufficient_record_count
                    .checked_add(1)
                    .context("feature-breakdown insufficient count overflow")?;
            }
            self.first_failure_partition.observe(event.feature)?;
        }
        Ok(())
    }

    pub(super) fn finish(self, expected: FrequencyAggregate) -> Result<Data> {
        ensure!(
            expected.record_count > 0,
            "requested feature-breakdown family/frequency row is absent"
        );
        ensure!(
            self.group_incidence_count == expected.group_incidence_count
                && self.record_count == expected.record_count
                && self.complex_sample_count == expected.complex_sample_count,
            "feature-breakdown row accounting differs from classification"
        );
        ensure!(
            self.feature_eligible_record_count
                .checked_add(self.feature_insufficient_record_count)
                == Some(self.record_count),
            "feature-breakdown eligibility accounting differs"
        );
        ensure!(
            self.first_failure_partition.checked_total() == Some(self.record_count),
            "feature-breakdown first-failure partition differs"
        );
        let feature_summaries = self.features.finish();
        ensure!(
            feature_summaries.envelope_cv.observations == self.feature_eligible_record_count
                && feature_summaries.usable_phase_differences.observations
                    == self.feature_eligible_record_count
                && feature_summaries.phase_concentration.observations
                    == self.feature_eligible_record_count
                && feature_summaries.two_level_separation_hz.observations
                    == self.two_level_fit_record_count
                && feature_summaries.two_level_variance_reduction.observations
                    == self.two_level_fit_record_count
                && feature_summaries.two_level_mode_contrast.observations
                    == self.two_level_fit_record_count
                && feature_summaries.two_level_cluster_balance.observations
                    == self.two_level_fit_record_count
                && feature_summaries.two_level_transitions.observations
                    == self.two_level_fit_record_count,
            "feature-breakdown numeric-summary accounting differs"
        );
        Ok(Data {
            selection: self.selection,
            group_incidence_count: self.group_incidence_count,
            record_count: self.record_count,
            complex_sample_count: self.complex_sample_count,
            feature_eligible_record_count: self.feature_eligible_record_count,
            feature_insufficient_record_count: self.feature_insufficient_record_count,
            two_level_fit_record_count: self.two_level_fit_record_count,
            first_failure_partition: self.first_failure_partition,
            feature_summaries,
        })
    }
}

#[derive(Debug)]
pub(super) struct Data {
    selection: Selection,
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
    feature_eligible_record_count: u64,
    feature_insufficient_record_count: u64,
    two_level_fit_record_count: u64,
    first_failure_partition: FirstFailurePartition,
    feature_summaries: FeatureSummaries,
}

#[derive(Clone, Debug, Serialize)]
struct SelectionReport {
    candidate_family: &'static str,
    frequency_mhz: u32,
}

#[derive(Clone, Debug, Serialize)]
struct Accounting {
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
    feature_eligible_record_count: u64,
    feature_insufficient_record_count: u64,
    two_level_fit_record_count: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Report {
    schema_version: u32,
    operation: &'static str,
    source: SourceIdentity,
    selection: SelectionReport,
    settings: Settings,
    accounting: Accounting,
    first_failure_order: [&'static str; 10],
    first_failure_partition: FirstFailurePartition,
    selected_record_features: FeatureSummaries,
    protocol_identity_established: bool,
    interpretation: &'static str,
}

pub(super) fn report(source: SourceIdentity, settings: Settings, data: Data) -> Report {
    Report {
        schema_version: 1,
        operation: "diagnose_burst_family_feature_gates",
        source,
        selection: SelectionReport {
            candidate_family: data.selection.family.label(),
            frequency_mhz: data.selection.frequency_mhz,
        },
        settings,
        accounting: Accounting {
            group_incidence_count: data.group_incidence_count,
            record_count: data.record_count,
            complex_sample_count: data.complex_sample_count,
            feature_eligible_record_count: data.feature_eligible_record_count,
            feature_insufficient_record_count: data.feature_insufficient_record_count,
            two_level_fit_record_count: data.two_level_fit_record_count,
        },
        first_failure_order: [
            "insufficient_feature",
            "envelope_cv_above_maximum",
            "two_level_fit_unavailable",
            "cluster_balance_below_minimum",
            "variance_reduction_below_minimum",
            "mode_contrast_below_minimum",
            "separation_below_minimum",
            "separation_above_maximum",
            "transitions_below_minimum",
            "all_gates_passed",
        ],
        first_failure_partition: data.first_failure_partition,
        selected_record_features: data.feature_summaries,
        protocol_identity_established: false,
        interpretation: "Aggregate feature gates for records in one recomputed family/frequency row. The first-failure partition uses the declared order and is diagnostic, not a causal model. Counts do not identify a protocol, packet, transmitter, unique signal or occupancy. No IQ, bits, timestamps, archive indices, paths or device identifiers are emitted.",
    }
}

pub(super) fn encode(report: &Report, maximum_bytes: u64) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec_pretty(report)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() as u64 <= maximum_bytes,
        "feature-breakdown report exceeds the {maximum_bytes}-byte output limit"
    );
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::burst_families::{RecordFeature, TwoLevelFeature};

    fn feature() -> RecordFeature {
        RecordFeature {
            envelope_cv: 0.1,
            usable_phase_differences: 256,
            phase_concentration: 0.5,
            two_level: Some(TwoLevelFeature {
                centers_hz: [-125_000.0, 125_000.0],
                separation_hz: 250_000.0,
                variance_reduction: 0.8,
                mode_contrast: 8.0,
                cluster_balance: 0.4,
                transitions: 10,
            }),
            fsk_like: true,
            constant_envelope: true,
        }
    }

    #[test]
    fn first_failure_partition_covers_each_gate_once() {
        let mut partition = FirstFailurePartition::default();
        partition.observe(None).unwrap();

        let mut value = feature();
        value.constant_envelope = false;
        value.fsk_like = false;
        partition.observe(Some(value)).unwrap();

        let mut value = feature();
        value.two_level = None;
        value.fsk_like = false;
        partition.observe(Some(value)).unwrap();

        let mut values = Vec::new();
        let mut value = feature();
        value.two_level.as_mut().unwrap().cluster_balance = 0.1;
        values.push(value);
        let mut value = feature();
        value.two_level.as_mut().unwrap().variance_reduction = 0.5;
        values.push(value);
        let mut value = feature();
        value.two_level.as_mut().unwrap().mode_contrast = 3.0;
        values.push(value);
        let mut value = feature();
        value.two_level.as_mut().unwrap().separation_hz = 30_000.0;
        values.push(value);
        let mut value = feature();
        value.two_level.as_mut().unwrap().separation_hz = 1_000_000.0;
        values.push(value);
        let mut value = feature();
        value.two_level.as_mut().unwrap().transitions = 5;
        values.push(value);
        for mut value in values {
            value.fsk_like = false;
            partition.observe(Some(value)).unwrap();
        }
        partition.observe(Some(feature())).unwrap();

        assert_eq!(partition.checked_total(), Some(10));
        let encoded = serde_json::to_value(&partition).unwrap();
        for key in [
            "insufficient_feature",
            "envelope_cv_above_maximum",
            "two_level_fit_unavailable",
            "cluster_balance_below_minimum",
            "variance_reduction_below_minimum",
            "mode_contrast_below_minimum",
            "separation_below_minimum",
            "separation_above_maximum",
            "transitions_below_minimum",
            "all_gates_passed",
        ] {
            assert_eq!(encoded[key], 1, "{key}");
        }
    }
}
