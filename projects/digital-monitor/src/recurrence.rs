//! Bounded host-only recurrence inventory for compact watch metrics.
//!
//! A recurrence row describes only repeated observation of one provisional
//! waveform-family label in one channelizer frequency bin. It is not a signal,
//! transmitter, protocol, or occupancy identity.

mod coded_shadow;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{orchestrate, report};
use coded_shadow::{
    validate_coded_shadow, validate_coded_shadow_capture_plan, validate_coded_shadow_request,
};

const MAX_ATTEMPTS: usize = 1_000;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const OUTPUT_NAME: &str = "recurrence.json";
const OPERATION: &str = "inventory_burst_family_recurrence";
const MIXED_CENTER_SELECTION: &str = "round_robin_by_attempt_index";
const MIN_WATCH_CENTER_HZ: u64 = 2_433_000_000;
const MAX_WATCH_CENTER_HZ: u64 = 2_437_000_000;
const WATCH_SAMPLE_RATE_HZ: u64 = 20_000_000;

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) run_json: PathBuf,
    pub(crate) output: PathBuf,
}

#[derive(Debug)]
pub(crate) struct InventoryResult {
    pub(crate) output: PathBuf,
}

#[derive(Debug, Serialize)]
struct InputAccounting {
    watch_schema_version: u64,
    finished: bool,
    plan_completed: bool,
    successful: bool,
    cycles_requested: u64,
    attempts_present: u64,
    attempts_completed: u64,
    eligible_captures: u64,
    excluded_attempts: u64,
}

#[derive(Debug, Serialize)]
struct Threshold {
    rule: &'static str,
    minimum_support_captures: u64,
}

#[derive(Debug, Serialize)]
struct OutputAccounting {
    observed_rows: u64,
    recurring_rows: u64,
}

#[derive(Debug, Serialize)]
struct MixedInputAccounting {
    watch_schema_version: u64,
    finished: bool,
    plan_completed: bool,
    successful: bool,
    cycles_requested: u64,
    attempts_present: u64,
    attempts_completed: u64,
    eligible_captures: u64,
    excluded_attempts: u64,
    center_frequency_schedule_hz: [u64; 2],
    center_selection: &'static str,
}

#[derive(Debug, Serialize)]
struct CenterInputAccounting {
    planned_attempts: u64,
    attempts_present: u64,
    eligible_captures: u64,
    excluded_attempts: u64,
    unstarted_attempts: u64,
}

#[derive(Debug, Serialize)]
struct MixedOutputAccounting {
    center_strata: u64,
    observed_rows: u64,
    recurring_rows: u64,
}

#[derive(Debug, Serialize)]
struct CountDistribution {
    total: u64,
    minimum_per_supporting_capture: u64,
    maximum_per_supporting_capture: u64,
    lower_median_per_supporting_capture: u64,
    upper_median_per_supporting_capture: u64,
}

#[derive(Debug, Serialize)]
struct RecurrenceRow {
    candidate_family: &'static str,
    frequency_mhz: u64,
    support_captures: u64,
    eligible_captures: u64,
    recurring: bool,
    longest_consecutive_attempt_index_streak: u64,
    group_incidence: CountDistribution,
    records: CountDistribution,
    complex_samples: CountDistribution,
}

#[derive(Debug, Serialize)]
struct MixedRecurrenceRow {
    candidate_family: &'static str,
    frequency_mhz: u64,
    support_captures: u64,
    eligible_captures: u64,
    recurring: bool,
    longest_consecutive_center_opportunity_streak: u64,
    group_incidence: CountDistribution,
    records: CountDistribution,
    complex_samples: CountDistribution,
}

#[derive(Debug, Serialize)]
struct InventoryReport {
    schema_version: u32,
    operation: &'static str,
    source_run_sha256: String,
    input: InputAccounting,
    threshold: Threshold,
    family_order: [&'static str; 5],
    accounting: OutputAccounting,
    rows: Vec<RecurrenceRow>,
    protocol_identity_established: bool,
    host_only: bool,
    radio_opened: bool,
    transmit_requested: bool,
    interpretation: &'static str,
}

#[derive(Debug, Serialize)]
struct CenterStratum {
    center_frequency_hz: u64,
    nominal_half_open_frequency_band_hz: [u64; 2],
    input: CenterInputAccounting,
    threshold: Threshold,
    accounting: OutputAccounting,
    rows: Vec<MixedRecurrenceRow>,
}

#[derive(Debug, Serialize)]
struct AlignmentCenterRow {
    center_frequency_hz: u64,
    frequency_mhz: u64,
    support_captures: u64,
    eligible_captures: u64,
}

#[derive(Debug, Serialize)]
struct AbsoluteFrequencyMatch {
    candidate_family: &'static str,
    frequency_mhz: u64,
    center_rows: [AlignmentCenterRow; 2],
}

#[derive(Debug, Serialize)]
struct CenterRelativeOffsetMatch {
    candidate_family: &'static str,
    center_relative_offset_hz: i64,
    center_rows: [AlignmentCenterRow; 2],
}

#[derive(Debug, Serialize)]
struct CrossCenterCoordinateAlignment {
    same_family_absolute_frequency_recurring_pairs: Vec<AbsoluteFrequencyMatch>,
    same_family_center_relative_offset_recurring_pairs: Vec<CenterRelativeOffsetMatch>,
}

#[derive(Debug, Serialize)]
struct MixedInventoryReport {
    schema_version: u32,
    operation: &'static str,
    source_run_sha256: String,
    input: MixedInputAccounting,
    family_order: [&'static str; 5],
    center_strata: Vec<CenterStratum>,
    cross_center_coordinate_alignment: CrossCenterCoordinateAlignment,
    accounting: MixedOutputAccounting,
    cross_center_aggregation_performed: bool,
    absolute_frequency_comparability_established: bool,
    protocol_identity_established: bool,
    host_only: bool,
    radio_opened: bool,
    transmit_requested: bool,
    interpretation: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct BinObservation {
    family_index: usize,
    frequency_mhz: u64,
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct FamilyTotals {
    group_count: u64,
    record_count: u64,
    complex_sample_count: u64,
}

#[derive(Debug, Default)]
struct RowAccumulator {
    support_indices: Vec<u64>,
    group_incidence: Vec<u64>,
    records: Vec<u64>,
    complex_samples: Vec<u64>,
    total_group_incidence: u64,
    total_records: u64,
    total_complex_samples: u64,
}

#[derive(Debug)]
struct CenterAccumulator {
    planned_attempts: u64,
    attempts_present: u64,
    eligible_captures: u64,
    excluded_attempts: u64,
    rows: BTreeMap<(usize, u64), RowAccumulator>,
}

impl CenterAccumulator {
    fn new(planned_attempts: u64) -> Self {
        Self {
            planned_attempts,
            attempts_present: 0,
            eligible_captures: 0,
            excluded_attempts: 0,
            rows: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct FinishedRow {
    support_captures: u64,
    longest_streak: u64,
    group_incidence: CountDistribution,
    records: CountDistribution,
    complex_samples: CountDistribution,
}

impl RowAccumulator {
    fn observe(&mut self, attempt_index: u64, observation: BinObservation) -> Result<()> {
        ensure!(
            self.support_indices.last().copied() != Some(attempt_index),
            "one capture contains a duplicate family/frequency row"
        );
        self.total_group_incidence = self
            .total_group_incidence
            .checked_add(observation.group_incidence_count)
            .context("recurrence group-incidence total overflow")?;
        self.total_records = self
            .total_records
            .checked_add(observation.record_count)
            .context("recurrence record total overflow")?;
        self.total_complex_samples = self
            .total_complex_samples
            .checked_add(observation.complex_sample_count)
            .context("recurrence sample total overflow")?;
        self.support_indices.push(attempt_index);
        self.group_incidence.push(observation.group_incidence_count);
        self.records.push(observation.record_count);
        self.complex_samples.push(observation.complex_sample_count);
        Ok(())
    }

    fn finish(
        self,
        family_index: usize,
        frequency_mhz: u64,
        eligible_captures: u64,
        threshold: u64,
    ) -> Result<RecurrenceRow> {
        let finished = self.finish_counts(eligible_captures)?;
        Ok(RecurrenceRow {
            candidate_family: orchestrate::BURST_FAMILY_LABELS[family_index],
            frequency_mhz,
            support_captures: finished.support_captures,
            eligible_captures,
            recurring: finished.support_captures >= threshold,
            longest_consecutive_attempt_index_streak: finished.longest_streak,
            group_incidence: finished.group_incidence,
            records: finished.records,
            complex_samples: finished.complex_samples,
        })
    }

    fn finish_mixed(
        self,
        family_index: usize,
        frequency_mhz: u64,
        eligible_captures: u64,
        threshold: u64,
    ) -> Result<MixedRecurrenceRow> {
        let finished = self.finish_counts(eligible_captures)?;
        Ok(MixedRecurrenceRow {
            candidate_family: orchestrate::BURST_FAMILY_LABELS[family_index],
            frequency_mhz,
            support_captures: finished.support_captures,
            eligible_captures,
            recurring: finished.support_captures >= threshold,
            longest_consecutive_center_opportunity_streak: finished.longest_streak,
            group_incidence: finished.group_incidence,
            records: finished.records,
            complex_samples: finished.complex_samples,
        })
    }

    fn finish_counts(mut self, eligible_captures: u64) -> Result<FinishedRow> {
        let support_captures = u64::try_from(self.support_indices.len())?;
        ensure!(
            support_captures > 0 && support_captures <= eligible_captures,
            "recurrence support count is outside the eligible-capture count"
        );
        Ok(FinishedRow {
            support_captures,
            longest_streak: longest_streak(&self.support_indices),
            group_incidence: summarize_counts(
                &mut self.group_incidence,
                self.total_group_incidence,
            )?,
            records: summarize_counts(&mut self.records, self.total_records)?,
            complex_samples: summarize_counts(
                &mut self.complex_samples,
                self.total_complex_samples,
            )?,
        })
    }
}

fn longest_streak(indices: &[u64]) -> u64 {
    let mut longest = 0u64;
    let mut current = 0u64;
    let mut previous: Option<u64> = None;
    for index in indices {
        current = if previous.is_some_and(|prior| prior.checked_add(1) == Some(*index)) {
            current + 1
        } else {
            1
        };
        longest = longest.max(current);
        previous = Some(*index);
    }
    longest
}

fn summarize_counts(values: &mut [u64], total: u64) -> Result<CountDistribution> {
    ensure!(
        !values.is_empty(),
        "cannot summarize an empty recurrence row"
    );
    values.sort_unstable();
    let recomputed = values.iter().try_fold(0u64, |sum, value| {
        sum.checked_add(*value)
            .context("recurrence distribution total overflow")
    })?;
    ensure!(
        recomputed == total,
        "recurrence distribution total differs from its accumulator"
    );
    Ok(CountDistribution {
        total,
        minimum_per_supporting_capture: values[0],
        maximum_per_supporting_capture: values[values.len() - 1],
        lower_median_per_supporting_capture: values[(values.len() - 1) / 2],
        upper_median_per_supporting_capture: values[values.len() / 2],
    })
}

fn recurrence_threshold(eligible_captures: u64) -> u64 {
    let fifth = eligible_captures / 5 + u64::from(!eligible_captures.is_multiple_of(5));
    3.max(fifth)
}

fn alignment_center_row(stratum: &CenterStratum, row: &MixedRecurrenceRow) -> AlignmentCenterRow {
    AlignmentCenterRow {
        center_frequency_hz: stratum.center_frequency_hz,
        frequency_mhz: row.frequency_mhz,
        support_captures: row.support_captures,
        eligible_captures: row.eligible_captures,
    }
}

fn center_relative_offset_hz(center_frequency_hz: u64, frequency_mhz: u64) -> Result<i64> {
    let frequency_hz = frequency_mhz
        .checked_mul(1_000_000)
        .context("recurrence frequency overflows hertz")?;
    i64::try_from(i128::from(frequency_hz) - i128::from(center_frequency_hz))
        .context("center-relative recurrence frequency does not fit i64")
}

fn cross_center_coordinate_alignment(
    center_strata: &[CenterStratum],
) -> Result<CrossCenterCoordinateAlignment> {
    let [first, second] = center_strata else {
        bail!("cross-center alignment requires exactly two center strata");
    };
    ensure!(
        first.center_frequency_hz != second.center_frequency_hz,
        "cross-center alignment requires distinct configured centers"
    );
    let mut absolute = Vec::new();
    let mut relative = Vec::new();
    for first_row in first.rows.iter().filter(|row| row.recurring) {
        let first_offset =
            center_relative_offset_hz(first.center_frequency_hz, first_row.frequency_mhz)?;
        for second_row in second
            .rows
            .iter()
            .filter(|row| row.recurring && row.candidate_family == first_row.candidate_family)
        {
            let rows = || {
                [
                    alignment_center_row(first, first_row),
                    alignment_center_row(second, second_row),
                ]
            };
            if first_row.frequency_mhz == second_row.frequency_mhz {
                absolute.push(AbsoluteFrequencyMatch {
                    candidate_family: first_row.candidate_family,
                    frequency_mhz: first_row.frequency_mhz,
                    center_rows: rows(),
                });
            }
            let second_offset =
                center_relative_offset_hz(second.center_frequency_hz, second_row.frequency_mhz)?;
            if first_offset == second_offset {
                relative.push(CenterRelativeOffsetMatch {
                    candidate_family: first_row.candidate_family,
                    center_relative_offset_hz: first_offset,
                    center_rows: rows(),
                });
            }
        }
    }
    Ok(CrossCenterCoordinateAlignment {
        same_family_absolute_frequency_recurring_pairs: absolute,
        same_family_center_relative_offset_recurring_pairs: relative,
    })
}

fn nominal_half_open_band(center_frequency_hz: u64, sample_rate_hz: u64) -> Result<[u64; 2]> {
    ensure!(
        sample_rate_hz > 0 && sample_rate_hz.is_multiple_of(2),
        "watch sample rate cannot define a half-open capture band"
    );
    let half_rate = sample_rate_hz / 2;
    Ok([
        center_frequency_hz
            .checked_sub(half_rate)
            .context("watch capture band underflows zero")?,
        center_frequency_hz
            .checked_add(half_rate)
            .context("watch capture band overflows")?,
    ])
}

fn mixed_center_schedule(run: &Value) -> Result<[u64; 2]> {
    ensure!(
        run.get("center_frequency_hz").is_none(),
        "schema-4 watch report contains a legacy root center"
    );
    ensure!(
        required_str(run, "center_selection")? == MIXED_CENTER_SELECTION,
        "schema-4 watch center selection differs from the round-robin contract"
    );
    let values = run
        .get("center_frequency_schedule_hz")
        .and_then(Value::as_array)
        .context("schema-4 watch report lacks a center schedule")?;
    ensure!(
        values.len() == 2,
        "schema-4 watch center schedule is not length two"
    );
    let schedule = [
        values[0]
            .as_u64()
            .context("schema-4 watch center schedule contains a non-counter")?,
        values[1]
            .as_u64()
            .context("schema-4 watch center schedule contains a non-counter")?,
    ];
    ensure!(
        schedule[0] != schedule[1],
        "schema-4 watch center schedule contains a duplicate center"
    );
    ensure!(
        schedule.iter().all(|center| {
            (MIN_WATCH_CENTER_HZ..=MAX_WATCH_CENTER_HZ).contains(center)
                && center.is_multiple_of(1_000_000)
        }),
        "schema-4 watch center schedule lies outside the supported integer-MHz range"
    );
    Ok(schedule)
}

fn planned_attempts_for_slot(cycles_requested: u64, slot: usize) -> u64 {
    cycles_requested / 2 + u64::from(slot == 0 && !cycles_requested.is_multiple_of(2))
}

fn center_opportunity(index: u64) -> u64 {
    (index - 1) / 2 + 1
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .with_context(|| format!("watch report lacks boolean {field}"))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("watch report lacks string {field}"))
}

fn exact_object(value: &Value, fields: &[&str], description: &str) -> Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("{description} is not an object"))?;
    ensure!(
        object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field)),
        "{description} schema differs"
    );
    Ok(())
}

fn five_counts(value: &Value, field: &str) -> Result<[u64; 5]> {
    let array = value
        .get(field)
        .and_then(Value::as_array)
        .with_context(|| format!("burst-family bin lacks array {field}"))?;
    ensure!(
        array.len() == 5,
        "burst-family {field} array is not length five"
    );
    let mut counts = [0u64; 5];
    for (index, item) in array.iter().enumerate() {
        counts[index] = item
            .as_u64()
            .with_context(|| format!("burst-family {field} contains a non-counter"))?;
    }
    Ok(counts)
}

fn expected_settings() -> Value {
    json!({
        "channel_sample_rate_hz":2_000_000,
        "sample_period_ns":500,
        "group_connectivity_gap_ns":50_000,
        "group_frequency_step_mhz":1,
        "simultaneous_interval":"half_open_unexpanded_record_interval",
        "active_power_quantile":[4,5],
        "active_power_divisor":16,
        "minimum_usable_phase_differences":128,
        "maximum_envelope_cv":0.25,
        "minimum_cluster_balance":0.15,
        "minimum_variance_reduction":0.60,
        "minimum_mode_contrast":4.0,
        "mode_contrast_std_floor_hz":1.0,
        "minimum_phase_concentration":0.05,
        "separation_hz":[40_000.0,900_000.0],
        "minimum_transitions":6,
        "kmeans_iterations":8,
        "adjacent_multibin_minimum_width":2,
        "wideband_multibin_minimum_width":4
    })
}

fn validate_family_order(value: &Value) -> Result<()> {
    let order = value
        .get("family_order")
        .and_then(Value::as_array)
        .context("burst-family metrics lack family_order")?;
    ensure!(
        order.len() == orchestrate::BURST_FAMILY_LABELS.len()
            && order
                .iter()
                .zip(orchestrate::BURST_FAMILY_LABELS)
                .all(|(actual, expected)| actual.as_str() == Some(expected)),
        "burst-family order differs from the fixed five-family contract"
    );
    Ok(())
}

fn checked_add(target: &mut u64, value: u64, description: &str) -> Result<()> {
    *target = target
        .checked_add(value)
        .with_context(|| format!("{description} overflow"))?;
    Ok(())
}

fn validate_burst_families(
    value: &Value,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    frozen_settings: &mut Option<Value>,
) -> Result<Vec<BinObservation>> {
    ensure!(
        !required_bool(value, "protocol_identity_established")?,
        "burst-family metrics claim protocol identity"
    );
    validate_family_order(value)?;
    let settings = value
        .get("settings")
        .context("burst-family metrics lack settings")?;
    ensure!(
        *settings == expected_settings(),
        "burst-family settings differ from the schema-11 contract"
    );
    if let Some(frozen) = frozen_settings {
        ensure!(
            settings == frozen,
            "burst-family settings changed between captures"
        );
    } else {
        *frozen_settings = Some(settings.clone());
    }

    let families = value
        .get("families")
        .and_then(Value::as_array)
        .context("burst-family metrics lack family totals")?;
    ensure!(
        families.len() == 5,
        "burst-family total array is not length five"
    );
    let mut totals = [FamilyTotals::default(); 5];
    for (index, family) in families.iter().enumerate() {
        exact_object(
            family,
            &["group_count", "record_count", "complex_sample_count"],
            "burst-family total",
        )?;
        let group_count = report::required_u64(family, "group_count")?;
        let record_count = report::required_u64(family, "record_count")?;
        let complex_sample_count = report::required_u64(family, "complex_sample_count")?;
        let minimum_records_per_group = match index {
            0 => 4,
            1 => 2,
            _ => 1,
        };
        ensure!(
            group_count
                .checked_mul(minimum_records_per_group)
                .is_some_and(|minimum| minimum <= record_count)
                && (group_count == 0) == (record_count == 0),
            "burst-family total has impossible group/record counts"
        );
        totals[index] = FamilyTotals {
            group_count,
            record_count,
            complex_sample_count,
        };
    }

    let accounting = value
        .get("accounting")
        .context("burst-family metrics lack accounting")?;
    exact_object(
        accounting,
        &[
            "archive_record_count",
            "squelch_record_count",
            "scan_window_record_count",
            "archive_complex_sample_count",
            "squelch_complex_sample_count",
            "scan_window_complex_sample_count",
            "feature_eligible_record_count",
            "feature_insufficient_record_count",
            "group_count",
            "grouped_squelch_record_count",
        ],
        "burst-family accounting",
    )?;
    let archive_records = report::required_u64(accounting, "archive_record_count")?;
    let squelch_records = report::required_u64(accounting, "squelch_record_count")?;
    let scan_records = report::required_u64(accounting, "scan_window_record_count")?;
    let archive_samples = report::required_u64(accounting, "archive_complex_sample_count")?;
    let squelch_samples = report::required_u64(accounting, "squelch_complex_sample_count")?;
    let scan_samples = report::required_u64(accounting, "scan_window_complex_sample_count")?;
    let eligible_records = report::required_u64(accounting, "feature_eligible_record_count")?;
    let insufficient_records =
        report::required_u64(accounting, "feature_insufficient_record_count")?;
    let group_count = report::required_u64(accounting, "group_count")?;
    let grouped_records = report::required_u64(accounting, "grouped_squelch_record_count")?;
    ensure!(
        squelch_records.checked_add(scan_records) == Some(archive_records)
            && squelch_samples.checked_add(scan_samples) == Some(archive_samples)
            && eligible_records.checked_add(insufficient_records) == Some(squelch_records)
            && grouped_records == squelch_records,
        "burst-family accounting partitions differ"
    );
    let family_groups = totals.iter().try_fold(0u64, |sum, family| {
        sum.checked_add(family.group_count)
            .context("burst-family group total overflow")
    })?;
    let family_records = totals.iter().try_fold(0u64, |sum, family| {
        sum.checked_add(family.record_count)
            .context("burst-family record total overflow")
    })?;
    let family_samples = totals.iter().try_fold(0u64, |sum, family| {
        sum.checked_add(family.complex_sample_count)
            .context("burst-family sample total overflow")
    })?;
    ensure!(
        family_groups == group_count
            && family_records == grouped_records
            && family_samples == squelch_samples,
        "burst-family totals differ from accounting"
    );

    let [lowest_hz, highest_hz] = nominal_half_open_band(center_frequency_hz, sample_rate_hz)?;

    let bins = value
        .get("frequency_bins")
        .and_then(Value::as_array)
        .context("burst-family metrics lack frequency bins")?;
    ensure!(bins.len() <= 79, "too many burst-family frequency bins");
    let mut previous_frequency = None;
    let mut bin_incidence = [0u64; 5];
    let mut bin_records = [0u64; 5];
    let mut bin_samples = [0u64; 5];
    let mut observations = Vec::new();
    for bin in bins {
        exact_object(
            bin,
            &[
                "frequency_mhz",
                "group_incidence_count",
                "record_count",
                "complex_sample_count",
            ],
            "burst-family frequency bin",
        )?;
        let frequency_mhz = report::required_u64(bin, "frequency_mhz")?;
        ensure!(
            previous_frequency.is_none_or(|previous| frequency_mhz > previous),
            "burst-family frequencies are not strictly increasing"
        );
        previous_frequency = Some(frequency_mhz);
        let frequency_hz = frequency_mhz
            .checked_mul(1_000_000)
            .context("burst-family frequency overflows hertz")?;
        ensure!(
            (lowest_hz..highest_hz).contains(&frequency_hz),
            "burst-family frequency lies outside the watch capture band"
        );
        let incidence = five_counts(bin, "group_incidence_count")?;
        let records = five_counts(bin, "record_count")?;
        let samples = five_counts(bin, "complex_sample_count")?;
        ensure!(
            incidence.iter().any(|count| *count > 0),
            "burst-family frequency bin contains no observations"
        );
        for family_index in 0..5 {
            ensure!(
                (incidence[family_index] == 0) == (records[family_index] == 0)
                    && (records[family_index] != 0 || samples[family_index] == 0)
                    && incidence[family_index] <= records[family_index]
                    && incidence[family_index] <= totals[family_index].group_count,
                "burst-family frequency-bin counts are inconsistent"
            );
            checked_add(
                &mut bin_incidence[family_index],
                incidence[family_index],
                "burst-family bin incidence",
            )?;
            checked_add(
                &mut bin_records[family_index],
                records[family_index],
                "burst-family bin records",
            )?;
            checked_add(
                &mut bin_samples[family_index],
                samples[family_index],
                "burst-family bin samples",
            )?;
            if incidence[family_index] > 0 {
                observations.push(BinObservation {
                    family_index,
                    frequency_mhz,
                    group_incidence_count: incidence[family_index],
                    record_count: records[family_index],
                    complex_sample_count: samples[family_index],
                });
            }
        }
    }
    for family_index in 0..5 {
        ensure!(
            bin_records[family_index] == totals[family_index].record_count
                && bin_samples[family_index] == totals[family_index].complex_sample_count
                && bin_incidence[family_index] >= totals[family_index].group_count
                && bin_incidence[family_index] <= totals[family_index].record_count,
            "burst-family frequency-bin partitions differ from family totals"
        );
    }
    Ok(observations)
}

fn known_step_state(state: &str) -> bool {
    matches!(
        state,
        "not_started" | "in_progress" | "complete" | "failed" | "interrupted"
    )
}

fn publish<T: Serialize>(output: &Path, inventory: &T, stop: &AtomicBool) -> Result<()> {
    let mut encoded = serde_json::to_vec_pretty(inventory)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() <= MAX_OUTPUT_BYTES,
        "recurrence report exceeds {MAX_OUTPUT_BYTES} bytes"
    );
    let partial = output.join(".recurrence.json.partial");
    let final_path = output.join(OUTPUT_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    if stop.load(Ordering::Relaxed) {
        drop(file);
        fs::remove_file(&partial)?;
        bail!("recurrence inventory interrupted before publication");
    }
    if let Err(error) = fs::hard_link(&partial, &final_path) {
        let _ = fs::remove_file(&partial);
        return Err(error).with_context(|| {
            format!(
                "publish {} without replacing an existing file",
                final_path.display()
            )
        });
    }
    fs::remove_file(&partial)?;
    File::open(output)?.sync_all()?;
    Ok(())
}

fn inventory_mixed(
    run: &Value,
    watch_schema_version: u64,
    source_run_sha256: String,
    request: &Request,
    stop: &AtomicBool,
) -> Result<InventoryResult> {
    ensure!(
        matches!(watch_schema_version, 4 | 5),
        "mixed-center watch schema is unsupported"
    );
    validate_coded_shadow_request(run, watch_schema_version)?;
    ensure!(
        run.get("operation").and_then(Value::as_str) == Some("finite_rx1_multi_protocol_watch"),
        "unsupported watch report contract"
    );
    ensure!(
        required_bool(run, "finished")?,
        "watch report is not terminal"
    );
    let plan_completed = required_bool(run, "plan_completed")?;
    let successful = required_bool(run, "successful")?;
    ensure!(
        required_bool(run, "receive_only")?
            && !required_bool(run, "transmit_requested")?
            && !required_bool(run, "bias_tee_requested")?,
        "watch report does not describe the receive-only contract"
    );
    let cycles_requested = report::required_u64(run, "cycles_requested")?;
    ensure!(
        (2..=MAX_ATTEMPTS as u64).contains(&cycles_requested),
        "schema-4 watch cycle count is outside 2..={MAX_ATTEMPTS}"
    );
    let attempts_completed = report::required_u64(run, "attempts_completed")?;
    let schedule = mixed_center_schedule(run)?;
    ensure!(
        watch_schema_version != 5 || schedule.contains(&2_435_000_000),
        "schema-5 center schedule lacks the coded-shadow center"
    );
    let pending_capture_limit = report::required_u64(run, "pending_capture_limit")?;
    ensure!(
        pending_capture_limit <= 1,
        "schema-4 watch pending-capture limit is outside zero or one"
    );
    let sample_rate_hz = report::required_u64(run, "sample_rate_hz")?;
    let samples_per_capture = (watch_schema_version == 5)
        .then(|| report::required_u64(run, "samples_per_capture"))
        .transpose()?
        .unwrap_or(0);
    if watch_schema_version == 5 {
        validate_coded_shadow_capture_plan(sample_rate_hz, samples_per_capture)?;
    }
    ensure!(
        sample_rate_hz == WATCH_SAMPLE_RATE_HZ
            && report::required_u64(run, "bandwidth_hz")? == WATCH_SAMPLE_RATE_HZ,
        "schema-4 watch sample rate or bandwidth differs from 20 MHz"
    );
    let attempts = run
        .get("attempts")
        .and_then(Value::as_array)
        .context("watch report lacks attempts")?;
    ensure!(
        attempts.len() <= MAX_ATTEMPTS,
        "watch report exceeds {MAX_ATTEMPTS} attempts"
    );
    ensure!(
        attempts_completed == attempts.len() as u64,
        "terminal watch report contains unresolved attempts"
    );
    ensure!(
        !plan_completed || attempts_completed == cycles_requested,
        "completed watch plan does not contain every requested attempt"
    );
    ensure!(
        !successful || plan_completed,
        "successful watch report does not describe a completed plan"
    );

    let mut indices = BTreeSet::new();
    let mut ordered = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        let index = report::required_u64(attempt, "index")?;
        ensure!(
            (1..=cycles_requested).contains(&index),
            "watch attempt index is outside the requested cycle range"
        );
        ensure!(indices.insert(index), "watch attempt index is duplicated");
        ordered.push((index, attempt));
    }
    ordered.sort_by_key(|(index, _)| *index);
    ensure!(
        ordered
            .iter()
            .enumerate()
            .all(|(position, (index, _))| *index == position as u64 + 1),
        "schema-4 watch attempts are not a contiguous prefix"
    );

    let mut centers = [
        CenterAccumulator::new(planned_attempts_for_slot(cycles_requested, 0)),
        CenterAccumulator::new(planned_attempts_for_slot(cycles_requested, 1)),
    ];
    let mut frozen_settings = None;
    for (index, attempt) in ordered {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "recurrence inventory interrupted while validating attempts"
        );
        let slot = usize::try_from((index - 1) % 2)?;
        let center_frequency_hz = schedule[slot];
        let center = &mut centers[slot];
        center.attempts_present = center
            .attempts_present
            .checked_add(1)
            .context("center attempt count overflow")?;
        ensure!(
            report::required_u64(attempt, "center_frequency_hz")? == center_frequency_hz,
            "schema-4 watch attempt center differs from its round-robin slot"
        );
        if let Some(phase) = attempt.get("phase") {
            ensure!(
                phase.as_str() == Some("finished"),
                "terminal schema-4 watch attempt has a nonterminal phase"
            );
        } else {
            ensure!(
                pending_capture_limit == 0,
                "pipelined schema-4 watch attempt lacks its terminal phase"
            );
        }
        let capture_state = required_str(attempt, "capture_state")?;
        let analysis_state = required_str(attempt, "analysis_state")?;
        ensure!(
            known_step_state(capture_state) && known_step_state(analysis_state),
            "watch attempt has an unknown step state"
        );
        ensure!(
            capture_state != "in_progress" && analysis_state != "in_progress",
            "terminal watch attempt remains in progress"
        );
        if let Some(source) = attempt.get("source") {
            ensure!(
                report::required_u64(source, "center_frequency_hz")? == center_frequency_hz,
                "mixed-center watch source center differs from its attempt"
            );
            if watch_schema_version == 5 {
                ensure!(
                    report::required_u64(source, "complex_samples")? == samples_per_capture,
                    "schema-5 watch source sample count differs from its plan"
                );
            }
        } else {
            ensure!(
                capture_state != "complete",
                "complete schema-4 watch capture lacks its compact source"
            );
        }
        let interrupted = required_bool(attempt, "interrupted")?;
        if capture_state != "complete" || analysis_state != "complete" || interrupted {
            center.excluded_attempts = center
                .excluded_attempts
                .checked_add(1)
                .context("center excluded-attempt count overflow")?;
            continue;
        }
        let analysis = attempt
            .get("analysis")
            .context("complete watch attempt lacks analysis")?;
        ensure!(
            required_str(analysis, "overall_state")? == "complete",
            "complete watch attempt has a non-complete analysis"
        );
        validate_coded_shadow(
            analysis,
            center_frequency_hz,
            samples_per_capture,
            watch_schema_version,
        )?;
        let metrics = analysis
            .get("metrics")
            .context("complete watch analysis lacks compact metrics")?;
        let metrics_schema_version = report::required_u64(metrics, "metrics_schema_version")?;
        ensure!(
            matches!(metrics_schema_version, 11..=14),
            "complete watch analysis does not use supported metric schema 11, 12, 13 or 14"
        );
        let burst_families = metrics
            .get("burst_families")
            .context("compact metrics lack burst-family evidence")?;
        let observations = validate_burst_families(
            burst_families,
            center_frequency_hz,
            sample_rate_hz,
            &mut frozen_settings,
        )?;
        center.eligible_captures = center
            .eligible_captures
            .checked_add(1)
            .context("center eligible-capture count overflow")?;
        let opportunity = center_opportunity(index);
        for observation in observations {
            center
                .rows
                .entry((observation.family_index, observation.frequency_mhz))
                .or_default()
                .observe(opportunity, observation)?;
        }
    }

    let eligible_captures = centers.iter().try_fold(0u64, |total, center| {
        total
            .checked_add(center.eligible_captures)
            .context("eligible-capture count overflow")
    })?;
    let excluded_attempts = centers.iter().try_fold(0u64, |total, center| {
        total
            .checked_add(center.excluded_attempts)
            .context("excluded-attempt count overflow")
    })?;
    ensure!(
        eligible_captures > 0,
        "watch report contains no complete supported-metric-schema capture"
    );
    ensure!(
        eligible_captures.checked_add(excluded_attempts) == Some(attempts.len() as u64),
        "watch attempt accounting differs"
    );
    ensure!(
        centers.iter().all(|center| {
            center
                .eligible_captures
                .checked_add(center.excluded_attempts)
                == Some(center.attempts_present)
                && center.attempts_present <= center.planned_attempts
        }),
        "center-stratified watch attempt accounting differs"
    );
    ensure!(
        !successful || (excluded_attempts == 0 && eligible_captures == attempts_completed),
        "successful watch report contains an excluded attempt"
    );

    let mut observed_rows = 0u64;
    let mut recurring_rows = 0u64;
    let mut center_strata = Vec::with_capacity(2);
    for (slot, center) in centers.into_iter().enumerate() {
        let threshold = recurrence_threshold(center.eligible_captures);
        let mut rows = Vec::with_capacity(center.rows.len());
        for ((family_index, frequency_mhz), accumulator) in center.rows {
            rows.push(accumulator.finish_mixed(
                family_index,
                frequency_mhz,
                center.eligible_captures,
                threshold,
            )?);
        }
        let stratum_observed = u64::try_from(rows.len())?;
        let stratum_recurring = u64::try_from(rows.iter().filter(|row| row.recurring).count())?;
        observed_rows = observed_rows
            .checked_add(stratum_observed)
            .context("mixed recurrence observed-row count overflow")?;
        recurring_rows = recurring_rows
            .checked_add(stratum_recurring)
            .context("mixed recurrence recurring-row count overflow")?;
        let unstarted_attempts = center
            .planned_attempts
            .checked_sub(center.attempts_present)
            .context("center attempts exceed the planned opportunity count")?;
        center_strata.push(CenterStratum {
            center_frequency_hz: schedule[slot],
            nominal_half_open_frequency_band_hz: nominal_half_open_band(
                schedule[slot],
                sample_rate_hz,
            )?,
            input: CenterInputAccounting {
                planned_attempts: center.planned_attempts,
                attempts_present: center.attempts_present,
                eligible_captures: center.eligible_captures,
                excluded_attempts: center.excluded_attempts,
                unstarted_attempts,
            },
            threshold: Threshold {
                rule: "max(3,ceil(eligible_captures/5))",
                minimum_support_captures: threshold,
            },
            accounting: OutputAccounting {
                observed_rows: stratum_observed,
                recurring_rows: stratum_recurring,
            },
            rows,
        });
    }
    let cross_center_coordinate_alignment = cross_center_coordinate_alignment(&center_strata)?;
    let inventory = MixedInventoryReport {
        schema_version: 3,
        operation: OPERATION,
        source_run_sha256,
        input: MixedInputAccounting {
            watch_schema_version,
            finished: true,
            plan_completed,
            successful,
            cycles_requested,
            attempts_present: attempts.len() as u64,
            attempts_completed,
            eligible_captures,
            excluded_attempts,
            center_frequency_schedule_hz: schedule,
            center_selection: MIXED_CENTER_SELECTION,
        },
        family_order: orchestrate::BURST_FAMILY_LABELS,
        center_strata,
        cross_center_coordinate_alignment,
        accounting: MixedOutputAccounting {
            center_strata: 2,
            observed_rows,
            recurring_rows,
        },
        cross_center_aggregation_performed: false,
        absolute_frequency_comparability_established: false,
        protocol_identity_established: false,
        host_only: true,
        radio_opened: false,
        transmit_requested: false,
        interpretation: "Rows are stratified by configured center. One eligible capture contributes at most one support vote per center/family/frequency row, and streaks advance only across scheduled opportunities for that center. Exact same-family coordinate matches compare recurring rows without combining their per-center support or establishing absolute-frequency comparability. Absolute or center-relative coincidence is not proof of a signal, receiver artifact, waveform, transmitter, protocol, occupancy or calibrated response.",
    };
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence inventory interrupted before publication"
    );
    let output = report::fresh_output(&request.output)?;
    publish(&output, &inventory, stop)?;
    Ok(InventoryResult { output })
}

pub(crate) fn inventory(request: Request, stop: &AtomicBool) -> Result<InventoryResult> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence inventory interrupted before input validation"
    );
    let (run, source_run_sha256) = report::read_json_with_sha256(&request.run_json)?;
    let schema_version = report::required_u64(&run, "schema_version")?;
    if schema_version == 4
        || (schema_version == 5 && run.get("center_frequency_schedule_hz").is_some())
    {
        return inventory_mixed(&run, schema_version, source_run_sha256, &request, stop);
    }
    ensure!(
        matches!(schema_version, 2 | 3 | 5)
            && run.get("operation").and_then(Value::as_str)
                == Some("finite_rx1_multi_protocol_watch"),
        "unsupported watch report contract"
    );
    validate_coded_shadow_request(&run, schema_version)?;
    ensure!(
        required_bool(&run, "finished")?,
        "watch report is not terminal"
    );
    let plan_completed = required_bool(&run, "plan_completed")?;
    let successful = required_bool(&run, "successful")?;
    ensure!(
        required_bool(&run, "receive_only")?
            && !required_bool(&run, "transmit_requested")?
            && !required_bool(&run, "bias_tee_requested")?,
        "watch report does not describe the receive-only contract"
    );
    let cycles_requested = report::required_u64(&run, "cycles_requested")?;
    ensure!(
        (1..=MAX_ATTEMPTS as u64).contains(&cycles_requested),
        "watch cycle count is outside 1..={MAX_ATTEMPTS}"
    );
    let attempts_completed = report::required_u64(&run, "attempts_completed")?;
    let center_frequency_hz = report::required_u64(&run, "center_frequency_hz")?;
    ensure!(
        schema_version != 5 || center_frequency_hz == 2_435_000_000,
        "single-center schema-5 watch is not centered at 2435 MHz"
    );
    if schema_version == 5 {
        let pending_capture_limit = run
            .get("pending_capture_limit")
            .map(|value| {
                value
                    .as_u64()
                    .context("schema-5 pending-capture limit is not a counter")
            })
            .transpose()?
            .unwrap_or(0);
        ensure!(
            pending_capture_limit <= 1,
            "schema-5 pending-capture limit is outside zero or one"
        );
    }
    let sample_rate_hz = report::required_u64(&run, "sample_rate_hz")?;
    let samples_per_capture = (schema_version == 5)
        .then(|| report::required_u64(&run, "samples_per_capture"))
        .transpose()?
        .unwrap_or(0);
    if schema_version == 5 {
        validate_coded_shadow_capture_plan(sample_rate_hz, samples_per_capture)?;
    }
    ensure!(
        report::required_u64(&run, "bandwidth_hz")? == sample_rate_hz,
        "watch bandwidth differs from its sample rate"
    );
    let attempts = run
        .get("attempts")
        .and_then(Value::as_array)
        .context("watch report lacks attempts")?;
    ensure!(
        attempts.len() <= MAX_ATTEMPTS,
        "watch report exceeds {MAX_ATTEMPTS} attempts"
    );
    ensure!(
        attempts_completed == attempts.len() as u64,
        "terminal watch report contains unresolved attempts"
    );
    ensure!(
        !plan_completed || attempts_completed == cycles_requested,
        "completed watch plan does not contain every requested attempt"
    );
    ensure!(
        !successful || plan_completed,
        "successful watch report does not describe a completed plan"
    );

    let mut indices = BTreeSet::new();
    let mut ordered = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        let index = report::required_u64(attempt, "index")?;
        ensure!(
            (1..=cycles_requested).contains(&index),
            "watch attempt index is outside the requested cycle range"
        );
        ensure!(indices.insert(index), "watch attempt index is duplicated");
        ordered.push((index, attempt));
    }
    ordered.sort_by_key(|(index, _)| *index);

    let mut frozen_settings = None;
    let mut rows = BTreeMap::<(usize, u64), RowAccumulator>::new();
    let mut eligible_captures = 0u64;
    let mut excluded_attempts = 0u64;
    for (index, attempt) in ordered {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "recurrence inventory interrupted while validating attempts"
        );
        if schema_version == 3
            || (schema_version == 5 && run.get("pending_capture_limit").is_some())
        {
            ensure!(
                required_str(attempt, "phase")? == "finished",
                "terminal pipelined watch attempt has a nonterminal phase"
            );
        }
        let capture_state = required_str(attempt, "capture_state")?;
        let analysis_state = required_str(attempt, "analysis_state")?;
        ensure!(
            known_step_state(capture_state) && known_step_state(analysis_state),
            "watch attempt has an unknown step state"
        );
        ensure!(
            capture_state != "in_progress" && analysis_state != "in_progress",
            "terminal watch attempt remains in progress"
        );
        let interrupted = required_bool(attempt, "interrupted")?;
        if capture_state != "complete" || analysis_state != "complete" || interrupted {
            excluded_attempts = excluded_attempts
                .checked_add(1)
                .context("excluded-attempt count overflow")?;
            continue;
        }
        let analysis = attempt
            .get("analysis")
            .context("complete watch attempt lacks analysis")?;
        ensure!(
            required_str(analysis, "overall_state")? == "complete",
            "complete watch attempt has a non-complete analysis"
        );
        if schema_version == 5 {
            let source = attempt
                .get("source")
                .context("complete schema-5 watch attempt lacks its compact source")?;
            ensure!(
                report::required_u64(source, "complex_samples")? == samples_per_capture,
                "schema-5 watch source sample count differs from its plan"
            );
        }
        validate_coded_shadow(
            analysis,
            center_frequency_hz,
            samples_per_capture,
            schema_version,
        )?;
        let metrics = analysis
            .get("metrics")
            .context("complete watch analysis lacks compact metrics")?;
        let metrics_schema_version = report::required_u64(metrics, "metrics_schema_version")?;
        ensure!(
            matches!(metrics_schema_version, 11..=14),
            "complete watch analysis does not use supported metric schema 11, 12, 13 or 14"
        );
        let burst_families = metrics
            .get("burst_families")
            .context("compact metrics lack burst-family evidence")?;
        let observations = validate_burst_families(
            burst_families,
            center_frequency_hz,
            sample_rate_hz,
            &mut frozen_settings,
        )?;
        eligible_captures = eligible_captures
            .checked_add(1)
            .context("eligible-capture count overflow")?;
        for observation in observations {
            rows.entry((observation.family_index, observation.frequency_mhz))
                .or_default()
                .observe(index, observation)?;
        }
    }
    ensure!(
        eligible_captures > 0,
        "watch report contains no complete supported-metric-schema capture"
    );
    ensure!(
        eligible_captures.checked_add(excluded_attempts) == Some(attempts.len() as u64),
        "watch attempt accounting differs"
    );
    ensure!(
        !successful || (excluded_attempts == 0 && eligible_captures == attempts_completed),
        "successful watch report contains an excluded attempt"
    );

    let threshold = recurrence_threshold(eligible_captures);
    let mut output_rows = Vec::with_capacity(rows.len());
    for ((family_index, frequency_mhz), accumulator) in rows {
        output_rows.push(accumulator.finish(
            family_index,
            frequency_mhz,
            eligible_captures,
            threshold,
        )?);
    }
    let recurring_rows = u64::try_from(output_rows.iter().filter(|row| row.recurring).count())?;
    let inventory = InventoryReport {
        schema_version: 1,
        operation: OPERATION,
        source_run_sha256,
        input: InputAccounting {
            watch_schema_version: schema_version,
            finished: true,
            plan_completed,
            successful,
            cycles_requested,
            attempts_present: attempts.len() as u64,
            attempts_completed,
            eligible_captures,
            excluded_attempts,
        },
        threshold: Threshold {
            rule: "max(3,ceil(eligible_captures/5))",
            minimum_support_captures: threshold,
        },
        family_order: orchestrate::BURST_FAMILY_LABELS,
        accounting: OutputAccounting {
            observed_rows: output_rows.len() as u64,
            recurring_rows,
        },
        rows: output_rows,
        protocol_identity_established: false,
        host_only: true,
        radio_opened: false,
        transmit_requested: false,
        interpretation: "A row records repeated observation of one provisional waveform-family label in one 1 MHz channelizer bin. One eligible capture contributes at most one support vote per row. Multibin groups can contribute to several overlapping rows, so recurrence is neither a unique-signal count, waveform or transmitter identity, protocol evidence, nor channel occupancy.",
    };
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence inventory interrupted before publication"
    );
    let output = report::fresh_output(&request.output)?;
    publish(&output, &inventory, stop)?;
    Ok(InventoryResult { output })
}

#[cfg(test)]
mod tests;
