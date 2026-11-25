//! Strict validation for recurrence comparison inputs.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::{CENTER_SELECTION, MAX_REPORT_BYTES, RowState};
use crate::{orchestrate, report};

const MAX_ATTEMPTS: u64 = 1_000;
pub(super) const INVENTORY_OPERATION: &str = "inventory_burst_family_recurrence";
pub(super) const THRESHOLD_RULE: &str = "max(3,ceil(eligible_captures/5))";
pub(super) const INPUT_INTERPRETATION: &str = "Rows are stratified by configured center. One eligible capture contributes at most one support vote per center/family/frequency row, and streaks advance only across scheduled opportunities for that center. Exact same-family coordinate matches compare recurring rows without combining their per-center support or establishing absolute-frequency comparability. Absolute or center-relative coincidence is not proof of a signal, receiver artifact, waveform, transmitter, protocol, occupancy or calibrated response.";

#[derive(Debug)]
pub(super) struct ParsedStratum {
    pub(super) center_frequency_hz: u64,
    pub(super) nominal_band_hz: [u64; 2],
    pub(super) eligible_captures: u64,
    pub(super) minimum_support_captures: u64,
    pub(super) observed_rows: u64,
    pub(super) recurring_rows: u64,
    pub(super) rows: BTreeMap<(usize, u64), RowState>,
}

#[derive(Debug)]
pub(super) struct ParsedInventory {
    pub(super) recurrence_sha256: String,
    pub(super) source_run_sha256: String,
    pub(super) watch_schema_version: u64,
    pub(super) schedule: [u64; 2],
    pub(super) center_selection: String,
    pub(super) strata: [ParsedStratum; 2],
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

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .with_context(|| format!("recurrence report lacks boolean {field}"))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("recurrence report lacks string {field}"))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(super) fn threshold(eligible_captures: u64) -> u64 {
    let fifth = eligible_captures / 5 + u64::from(!eligible_captures.is_multiple_of(5));
    3.max(fifth)
}

fn parse_pair(value: &Value, description: &str) -> Result<[u64; 2]> {
    let values = value
        .as_array()
        .with_context(|| format!("{description} is not an array"))?;
    ensure!(values.len() == 2, "{description} is not length two");
    Ok([
        values[0]
            .as_u64()
            .with_context(|| format!("{description} contains a non-counter"))?,
        values[1]
            .as_u64()
            .with_context(|| format!("{description} contains a non-counter"))?,
    ])
}

fn validate_distribution(value: &Value, support: u64, require_positive: bool) -> Result<()> {
    exact_object(
        value,
        &[
            "total",
            "minimum_per_supporting_capture",
            "maximum_per_supporting_capture",
            "lower_median_per_supporting_capture",
            "upper_median_per_supporting_capture",
        ],
        "recurrence count distribution",
    )?;
    let total = report::required_u64(value, "total")?;
    let minimum = report::required_u64(value, "minimum_per_supporting_capture")?;
    let maximum = report::required_u64(value, "maximum_per_supporting_capture")?;
    let lower = report::required_u64(value, "lower_median_per_supporting_capture")?;
    let upper = report::required_u64(value, "upper_median_per_supporting_capture")?;
    ensure!(
        (!require_positive || minimum > 0)
            && minimum <= lower
            && lower <= upper
            && upper <= maximum
            && minimum
                .checked_mul(support)
                .is_some_and(|bound| bound <= total)
            && maximum
                .checked_mul(support)
                .is_some_and(|bound| total <= bound),
        "recurrence count distribution is inconsistent"
    );
    Ok(())
}

pub(super) fn expected_alignment(strata: &[ParsedStratum; 2]) -> Result<Value> {
    let mut absolute = Vec::new();
    let mut relative = Vec::new();
    for (&(first_family, first_frequency), first_state) in
        strata[0].rows.iter().filter(|(_, state)| state.recurring)
    {
        let first_hz = first_frequency
            .checked_mul(1_000_000)
            .context("recurrence frequency overflows hertz")?;
        let first_offset =
            i64::try_from(i128::from(first_hz) - i128::from(strata[0].center_frequency_hz))?;
        for (&(second_family, second_frequency), second_state) in strata[1]
            .rows
            .iter()
            .filter(|((family, _), state)| *family == first_family && state.recurring)
        {
            debug_assert_eq!(first_family, second_family);
            let center_rows = json!([
                {
                    "center_frequency_hz":strata[0].center_frequency_hz,
                    "frequency_mhz":first_frequency,
                    "support_captures":first_state.support_captures,
                    "eligible_captures":first_state.eligible_captures
                },
                {
                    "center_frequency_hz":strata[1].center_frequency_hz,
                    "frequency_mhz":second_frequency,
                    "support_captures":second_state.support_captures,
                    "eligible_captures":second_state.eligible_captures
                }
            ]);
            if first_frequency == second_frequency {
                absolute.push(json!({
                    "candidate_family":orchestrate::BURST_FAMILY_LABELS[first_family],
                    "frequency_mhz":first_frequency,
                    "center_rows":center_rows
                }));
            }
            let second_hz = second_frequency
                .checked_mul(1_000_000)
                .context("recurrence frequency overflows hertz")?;
            let second_offset =
                i64::try_from(i128::from(second_hz) - i128::from(strata[1].center_frequency_hz))?;
            if first_offset == second_offset {
                relative.push(json!({
                    "candidate_family":orchestrate::BURST_FAMILY_LABELS[first_family],
                    "center_relative_offset_hz":first_offset,
                    "center_rows":center_rows
                }));
            }
        }
    }
    Ok(json!({
        "same_family_absolute_frequency_recurring_pairs":absolute,
        "same_family_center_relative_offset_recurring_pairs":relative
    }))
}

pub(super) fn parse_stratum(
    value: &Value,
    center_frequency_hz: u64,
    planned_attempts: u64,
) -> Result<ParsedStratum> {
    exact_object(
        value,
        &[
            "center_frequency_hz",
            "nominal_half_open_frequency_band_hz",
            "input",
            "threshold",
            "accounting",
            "rows",
        ],
        "recurrence center stratum",
    )?;
    ensure!(
        report::required_u64(value, "center_frequency_hz")? == center_frequency_hz,
        "recurrence center strata differ from configured order"
    );
    let band = parse_pair(
        value
            .get("nominal_half_open_frequency_band_hz")
            .context("recurrence stratum lacks nominal band")?,
        "recurrence nominal band",
    )?;
    ensure!(
        band == [
            center_frequency_hz - 10_000_000,
            center_frequency_hz + 10_000_000,
        ],
        "recurrence nominal band differs from its configured center"
    );

    let input = value
        .get("input")
        .context("recurrence stratum lacks input")?;
    exact_object(
        input,
        &[
            "planned_attempts",
            "attempts_present",
            "eligible_captures",
            "excluded_attempts",
            "unstarted_attempts",
        ],
        "recurrence stratum input",
    )?;
    let present = report::required_u64(input, "attempts_present")?;
    let eligible = report::required_u64(input, "eligible_captures")?;
    let excluded = report::required_u64(input, "excluded_attempts")?;
    let unstarted = report::required_u64(input, "unstarted_attempts")?;
    ensure!(
        report::required_u64(input, "planned_attempts")? == planned_attempts
            && present == planned_attempts
            && eligible == planned_attempts
            && excluded == 0
            && unstarted == 0,
        "recurrence stratum is not a successful completed plan"
    );

    let threshold_value = value
        .get("threshold")
        .context("recurrence stratum lacks threshold")?;
    exact_object(
        threshold_value,
        &["rule", "minimum_support_captures"],
        "recurrence threshold",
    )?;
    let minimum_support = report::required_u64(threshold_value, "minimum_support_captures")?;
    ensure!(
        required_str(threshold_value, "rule")? == THRESHOLD_RULE
            && minimum_support == threshold(eligible),
        "recurrence threshold differs from its eligible-capture count"
    );

    let rows_value = value
        .get("rows")
        .and_then(Value::as_array)
        .context("recurrence stratum lacks rows")?;
    ensure!(
        rows_value.len() <= 395,
        "recurrence stratum has too many rows"
    );
    let mut rows = BTreeMap::new();
    let mut previous = None;
    for row in rows_value {
        exact_object(
            row,
            &[
                "candidate_family",
                "frequency_mhz",
                "support_captures",
                "eligible_captures",
                "recurring",
                "longest_consecutive_center_opportunity_streak",
                "group_incidence",
                "records",
                "complex_samples",
            ],
            "recurrence row",
        )?;
        let family_name = required_str(row, "candidate_family")?;
        let family_index = orchestrate::BURST_FAMILY_LABELS
            .iter()
            .position(|family| *family == family_name)
            .context("recurrence row has an unknown candidate family")?;
        let frequency_mhz = report::required_u64(row, "frequency_mhz")?;
        let key = (family_index, frequency_mhz);
        ensure!(
            previous.is_none_or(|prior| key > prior),
            "recurrence rows are not in canonical unique order"
        );
        previous = Some(key);
        let frequency_hz = frequency_mhz
            .checked_mul(1_000_000)
            .context("recurrence frequency overflows hertz")?;
        ensure!(
            (band[0]..band[1]).contains(&frequency_hz),
            "recurrence row lies outside its nominal band"
        );
        let support = report::required_u64(row, "support_captures")?;
        let row_eligible = report::required_u64(row, "eligible_captures")?;
        let recurring = required_bool(row, "recurring")?;
        let streak = report::required_u64(row, "longest_consecutive_center_opportunity_streak")?;
        ensure!(
            support > 0
                && support <= eligible
                && row_eligible == eligible
                && recurring == (support >= minimum_support)
                && streak > 0
                && streak <= support,
            "recurrence row support accounting differs"
        );
        validate_distribution(
            row.get("group_incidence")
                .context("recurrence row lacks group incidence")?,
            support,
            true,
        )?;
        validate_distribution(
            row.get("records")
                .context("recurrence row lacks record distribution")?,
            support,
            true,
        )?;
        validate_distribution(
            row.get("complex_samples")
                .context("recurrence row lacks sample distribution")?,
            support,
            false,
        )?;
        rows.insert(
            key,
            RowState {
                support_captures: support,
                eligible_captures: eligible,
                recurring,
            },
        );
    }

    let accounting = value
        .get("accounting")
        .context("recurrence stratum lacks accounting")?;
    exact_object(
        accounting,
        &["observed_rows", "recurring_rows"],
        "recurrence stratum accounting",
    )?;
    let observed_rows = report::required_u64(accounting, "observed_rows")?;
    let recurring_rows = report::required_u64(accounting, "recurring_rows")?;
    ensure!(
        observed_rows == rows.len() as u64
            && recurring_rows == rows.values().filter(|row| row.recurring).count() as u64,
        "recurrence stratum row accounting differs"
    );
    Ok(ParsedStratum {
        center_frequency_hz,
        nominal_band_hz: band,
        eligible_captures: eligible,
        minimum_support_captures: minimum_support,
        observed_rows,
        recurring_rows,
        rows,
    })
}

pub(super) fn parse_inventory(path: &Path) -> Result<ParsedInventory> {
    let (value, recurrence_sha256) = report::read_json_with_sha256_limit(path, MAX_REPORT_BYTES)?;
    exact_object(
        &value,
        &[
            "schema_version",
            "operation",
            "source_run_sha256",
            "input",
            "family_order",
            "center_strata",
            "cross_center_coordinate_alignment",
            "accounting",
            "cross_center_aggregation_performed",
            "absolute_frequency_comparability_established",
            "protocol_identity_established",
            "host_only",
            "radio_opened",
            "transmit_requested",
            "interpretation",
        ],
        "mixed recurrence report",
    )?;
    ensure!(
        report::required_u64(&value, "schema_version")? == 3
            && required_str(&value, "operation")? == INVENTORY_OPERATION,
        "comparison requires mixed recurrence schema 3"
    );
    let source_run_sha256 = required_str(&value, "source_run_sha256")?.to_owned();
    ensure!(
        valid_sha256(&source_run_sha256),
        "recurrence source-run digest is not lowercase SHA-256"
    );
    ensure!(
        !required_bool(&value, "cross_center_aggregation_performed")?
            && !required_bool(&value, "absolute_frequency_comparability_established")?
            && !required_bool(&value, "protocol_identity_established")?
            && required_bool(&value, "host_only")?
            && !required_bool(&value, "radio_opened")?
            && !required_bool(&value, "transmit_requested")?
            && required_str(&value, "interpretation")? == INPUT_INTERPRETATION,
        "recurrence report contains unsafe or unsupported interpretation flags"
    );

    let family_order = value
        .get("family_order")
        .and_then(Value::as_array)
        .context("recurrence report lacks family order")?;
    ensure!(
        family_order.len() == 5
            && family_order
                .iter()
                .zip(orchestrate::BURST_FAMILY_LABELS)
                .all(|(actual, expected)| actual.as_str() == Some(expected)),
        "recurrence family order differs from the fixed contract"
    );

    let input = value
        .get("input")
        .context("recurrence report lacks input")?;
    exact_object(
        input,
        &[
            "watch_schema_version",
            "finished",
            "plan_completed",
            "successful",
            "cycles_requested",
            "attempts_present",
            "attempts_completed",
            "eligible_captures",
            "excluded_attempts",
            "center_frequency_schedule_hz",
            "center_selection",
        ],
        "mixed recurrence input",
    )?;
    let watch_schema_version = report::required_u64(input, "watch_schema_version")?;
    ensure!(
        matches!(watch_schema_version, 4 | 5) && required_bool(input, "finished")?,
        "recurrence input is not a terminal mixed watch"
    );
    let plan_completed = required_bool(input, "plan_completed")?;
    let successful = required_bool(input, "successful")?;
    let cycles = report::required_u64(input, "cycles_requested")?;
    let attempts_present = report::required_u64(input, "attempts_present")?;
    let attempts_completed = report::required_u64(input, "attempts_completed")?;
    let eligible = report::required_u64(input, "eligible_captures")?;
    let excluded = report::required_u64(input, "excluded_attempts")?;
    ensure!(
        (2..=MAX_ATTEMPTS).contains(&cycles)
            && plan_completed
            && successful
            && attempts_present == cycles
            && attempts_completed == cycles
            && eligible == cycles
            && excluded == 0,
        "recurrence input is not a successful completed plan"
    );
    let schedule = parse_pair(
        input
            .get("center_frequency_schedule_hz")
            .context("recurrence input lacks center schedule")?,
        "recurrence center schedule",
    )?;
    ensure!(
        schedule[0] != schedule[1]
            && schedule.iter().all(|center| {
                (2_433_000_000..=2_437_000_000).contains(center) && center.is_multiple_of(1_000_000)
            }),
        "recurrence center schedule is outside the mixed-watch contract"
    );
    let center_selection = required_str(input, "center_selection")?.to_owned();
    ensure!(
        center_selection == CENTER_SELECTION,
        "recurrence center selection differs from the fixed contract"
    );

    let strata_values = value
        .get("center_strata")
        .and_then(Value::as_array)
        .context("recurrence report lacks center strata")?;
    ensure!(
        strata_values.len() == 2,
        "recurrence report is not two-center"
    );
    let planned = [
        cycles / 2 + u64::from(!cycles.is_multiple_of(2)),
        cycles / 2,
    ];
    let strata = [
        parse_stratum(&strata_values[0], schedule[0], planned[0])?,
        parse_stratum(&strata_values[1], schedule[1], planned[1])?,
    ];
    ensure!(
        strata
            .iter()
            .map(|stratum| stratum.eligible_captures)
            .sum::<u64>()
            == eligible,
        "recurrence stratum eligible counts differ from input"
    );

    let accounting = value
        .get("accounting")
        .context("recurrence report lacks accounting")?;
    exact_object(
        accounting,
        &["center_strata", "observed_rows", "recurring_rows"],
        "mixed recurrence accounting",
    )?;
    ensure!(
        report::required_u64(accounting, "center_strata")? == 2
            && report::required_u64(accounting, "observed_rows")?
                == strata
                    .iter()
                    .map(|stratum| stratum.observed_rows)
                    .sum::<u64>()
            && report::required_u64(accounting, "recurring_rows")?
                == strata
                    .iter()
                    .map(|stratum| stratum.recurring_rows)
                    .sum::<u64>(),
        "mixed recurrence row accounting differs"
    );
    ensure!(
        value
            .get("cross_center_coordinate_alignment")
            .context("recurrence report lacks coordinate alignment")?
            == &expected_alignment(&strata)?,
        "recurrence coordinate alignment differs from its recurring rows"
    );
    Ok(ParsedInventory {
        recurrence_sha256,
        source_run_sha256,
        watch_schema_version,
        schedule,
        center_selection,
        strata,
    })
}
