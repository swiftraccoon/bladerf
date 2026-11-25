use std::os::unix::fs::{PermissionsExt, symlink};

use serde_json::{Value, json};

use super::input::{
    INPUT_INTERPRETATION, INVENTORY_OPERATION, THRESHOLD_RULE, expected_alignment, parse_stratum,
    threshold,
};
use super::*;

fn distribution(support: u64) -> Value {
    json!({
        "total":support,
        "minimum_per_supporting_capture":1,
        "maximum_per_supporting_capture":1,
        "lower_median_per_supporting_capture":1,
        "upper_median_per_supporting_capture":1
    })
}

fn row(family: usize, frequency_mhz: u64, support: u64, eligible: u64) -> Value {
    json!({
        "candidate_family":orchestrate::BURST_FAMILY_LABELS[family],
        "frequency_mhz":frequency_mhz,
        "support_captures":support,
        "eligible_captures":eligible,
        "recurring":support >= threshold(eligible),
        "longest_consecutive_center_opportunity_streak":support,
        "group_incidence":distribution(support),
        "records":distribution(support),
        "complex_samples":distribution(support)
    })
}

fn stratum(center: u64, planned: u64, rows: Vec<Value>) -> Value {
    let recurring = rows
        .iter()
        .filter(|row| row["recurring"] == json!(true))
        .count();
    json!({
        "center_frequency_hz":center,
        "nominal_half_open_frequency_band_hz":[center - 10_000_000, center + 10_000_000],
        "input":{
            "planned_attempts":planned,
            "attempts_present":planned,
            "eligible_captures":planned,
            "excluded_attempts":0,
            "unstarted_attempts":0
        },
        "threshold":{
            "rule":THRESHOLD_RULE,
            "minimum_support_captures":threshold(planned)
        },
        "accounting":{
            "observed_rows":rows.len(),
            "recurring_rows":recurring
        },
        "rows":rows
    })
}

fn inventory(
    watch_schema_version: u64,
    cycles: u64,
    source_digit: char,
    primary_rows: Vec<Value>,
    alternate_rows: Vec<Value>,
) -> Value {
    let schedule = [2_435_000_000u64, 2_437_000_000u64];
    let planned = [
        cycles / 2 + u64::from(!cycles.is_multiple_of(2)),
        cycles / 2,
    ];
    let strata_values = vec![
        stratum(schedule[0], planned[0], primary_rows),
        stratum(schedule[1], planned[1], alternate_rows),
    ];
    let parsed = [
        parse_stratum(&strata_values[0], schedule[0], planned[0]).unwrap(),
        parse_stratum(&strata_values[1], schedule[1], planned[1]).unwrap(),
    ];
    let observed = parsed.iter().map(|value| value.observed_rows).sum::<u64>();
    let recurring = parsed.iter().map(|value| value.recurring_rows).sum::<u64>();
    json!({
        "schema_version":3,
        "operation":INVENTORY_OPERATION,
        "source_run_sha256":source_digit.to_string().repeat(64),
        "input":{
            "watch_schema_version":watch_schema_version,
            "finished":true,
            "plan_completed":true,
            "successful":true,
            "cycles_requested":cycles,
            "attempts_present":cycles,
            "attempts_completed":cycles,
            "eligible_captures":cycles,
            "excluded_attempts":0,
            "center_frequency_schedule_hz":schedule,
            "center_selection":CENTER_SELECTION
        },
        "family_order":orchestrate::BURST_FAMILY_LABELS,
        "center_strata":strata_values,
        "cross_center_coordinate_alignment":expected_alignment(&parsed).unwrap(),
        "accounting":{
            "center_strata":2,
            "observed_rows":observed,
            "recurring_rows":recurring
        },
        "cross_center_aggregation_performed":false,
        "absolute_frequency_comparability_established":false,
        "protocol_identity_established":false,
        "host_only":true,
        "radio_opened":false,
        "transmit_requested":false,
        "interpretation":INPUT_INTERPRETATION
    })
}

fn write_inventory(directory: &Path, name: &str, value: &Value) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    path
}

fn run_comparison(baseline: &Value, current: &Value) -> (tempfile::TempDir, PathBuf, Value) {
    let directory = tempfile::tempdir().unwrap();
    let baseline_path = write_inventory(directory.path(), "baseline.json", baseline);
    let current_path = write_inventory(directory.path(), "current.json", current);
    let output = directory.path().join("comparison");
    compare(
        Request {
            baseline: baseline_path,
            current: current_path,
            output: output.clone(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let report = serde_json::from_slice(&fs::read(output.join(OUTPUT_NAME)).unwrap()).unwrap();
    (directory, output, report)
}

fn expect_rejection(baseline: &Value, current: &Value, expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let baseline_path = write_inventory(directory.path(), "baseline.json", baseline);
    let current_path = write_inventory(directory.path(), "current.json", current);
    let error = compare(
        Request {
            baseline: baseline_path,
            current: current_path,
            output: directory.path().join("comparison"),
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains(expected), "{error:#}");
}

#[test]
fn comparison_preserves_role_states_without_combining_support() {
    let baseline = inventory(
        4,
        10,
        'a',
        vec![row(0, 2428, 3, 5), row(1, 2430, 3, 5), row(4, 2440, 3, 5)],
        vec![row(4, 2444, 3, 5)],
    );
    let current = inventory(
        5,
        40,
        'b',
        vec![
            row(0, 2428, 4, 20),
            row(1, 2430, 1, 20),
            row(3, 2435, 4, 20),
        ],
        vec![row(4, 2444, 4, 20)],
    );
    let (_directory, _output, report) = run_comparison(&baseline, &current);
    assert_eq!(report["schema_version"], json!(1));
    assert_eq!(report["baseline"]["watch_schema_version"], json!(4));
    assert_eq!(report["current"]["watch_schema_version"], json!(5));
    assert_eq!(
        report["center_strata"][0]["center_frequency_hz"],
        json!(2_435_000_000u64)
    );
    assert_eq!(
        report["center_strata"][1]["center_frequency_hz"],
        json!(2_437_000_000u64)
    );
    assert_eq!(report["accounting"]["recurring_in_both"], json!(2));
    assert_eq!(report["accounting"]["current_recurring_only"], json!(1));
    assert_eq!(report["accounting"]["baseline_recurring_only"], json!(2));
    let rows = report["center_strata"][0]["rows"].as_array().unwrap();
    assert_eq!(rows[0]["classification"], json!("recurring_in_both"));
    assert_eq!(rows[0]["baseline"]["support_captures"], json!(3));
    assert_eq!(rows[0]["baseline"]["eligible_captures"], json!(5));
    assert_eq!(rows[0]["current"]["support_captures"], json!(4));
    assert_eq!(rows[0]["current"]["eligible_captures"], json!(20));
    assert_eq!(rows[1]["classification"], json!("baseline_recurring_only"));
    assert_eq!(rows[1]["current"]["recurring"], json!(false));
    assert!(rows[2].get("baseline").is_none());
    assert!(rows[3].get("current").is_none());
    for flag in [
        "support_counts_combined",
        "cross_run_support_aggregation_performed",
        "cross_center_comparison_performed",
        "cross_run_support_comparability_established",
        "temporal_comparability_established",
        "environmental_comparability_established",
        "occupancy_comparability_established",
        "protocol_identity_established",
        "radio_opened",
        "transmit_requested",
    ] {
        assert_eq!(report[flag], json!(false), "{flag}");
    }
    assert_eq!(report["host_only"], json!(true));
}

#[test]
fn self_comparison_is_deterministic_private_and_all_shared() {
    let source = inventory(
        4,
        10,
        'c',
        vec![row(0, 2428, 3, 5)],
        vec![row(4, 2444, 4, 5)],
    );
    let (directory, first_output, first) = run_comparison(&source, &source);
    assert_eq!(first["accounting"]["recurring_in_both"], json!(2));
    assert_eq!(first["accounting"]["current_recurring_only"], json!(0));
    assert_eq!(first["accounting"]["baseline_recurring_only"], json!(0));
    assert_eq!(
        fs::metadata(&first_output).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(first_output.join(OUTPUT_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let baseline = write_inventory(directory.path(), "again-baseline.json", &source);
    let current = write_inventory(directory.path(), "again-current.json", &source);
    let second_output = directory.path().join("second");
    compare(
        Request {
            baseline,
            current,
            output: second_output.clone(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        fs::read(first_output.join(OUTPUT_NAME)).unwrap(),
        fs::read(second_output.join(OUTPUT_NAME)).unwrap()
    );
}

#[test]
fn comparison_rejects_schema_safety_schedule_and_family_drift() {
    let baseline = inventory(4, 10, 'd', vec![], vec![]);

    let mut schema = baseline.clone();
    schema["schema_version"] = json!(1);
    expect_rejection(&schema, &baseline, "schema 3");

    let mut unsafe_report = baseline.clone();
    unsafe_report["radio_opened"] = json!(true);
    expect_rejection(&baseline, &unsafe_report, "unsafe or unsupported");

    let mut schedule = baseline.clone();
    schedule["input"]["center_frequency_schedule_hz"] = json!([2_434_000_000u64, 2_437_000_000u64]);
    schedule["center_strata"][0]["center_frequency_hz"] = json!(2_434_000_000u64);
    schedule["center_strata"][0]["nominal_half_open_frequency_band_hz"] =
        json!([2_424_000_000u64, 2_444_000_000u64]);
    expect_rejection(&baseline, &schedule, "incompatible center schedules");

    let mut family = baseline.clone();
    family["family_order"][0] = json!("unknown");
    expect_rejection(&baseline, &family, "family order differs");
}

#[test]
fn comparison_rejects_incomplete_or_excluded_inputs() {
    let baseline = inventory(4, 10, '7', vec![], vec![]);

    let mut incomplete = baseline.clone();
    incomplete["input"]["successful"] = json!(false);
    expect_rejection(&incomplete, &baseline, "successful completed plan");

    let mut excluded = baseline.clone();
    excluded["center_strata"][0]["input"]["eligible_captures"] = json!(4);
    excluded["center_strata"][0]["input"]["excluded_attempts"] = json!(1);
    expect_rejection(&baseline, &excluded, "successful completed plan");
}

#[test]
fn comparison_rejects_row_accounting_order_band_and_alignment_tampering() {
    let baseline = inventory(
        4,
        10,
        'e',
        vec![row(0, 2428, 3, 5), row(1, 2430, 3, 5)],
        vec![row(0, 2428, 3, 5)],
    );

    let mut accounting = baseline.clone();
    accounting["center_strata"][0]["accounting"]["recurring_rows"] = json!(1);
    expect_rejection(&baseline, &accounting, "row accounting differs");

    let mut order = baseline.clone();
    order["center_strata"][0]["rows"]
        .as_array_mut()
        .unwrap()
        .reverse();
    expect_rejection(&baseline, &order, "canonical unique order");

    let mut band = baseline.clone();
    band["center_strata"][0]["rows"][0]["frequency_mhz"] = json!(2400);
    expect_rejection(&baseline, &band, "outside its nominal band");

    let mut alignment = baseline.clone();
    alignment["cross_center_coordinate_alignment"]["same_family_absolute_frequency_recurring_pairs"] =
        json!([]);
    expect_rejection(&baseline, &alignment, "coordinate alignment differs");
}

#[test]
fn comparison_rejects_symlink_oversize_reuse_and_early_interrupt() {
    let source = inventory(4, 10, 'f', vec![], vec![]);
    let directory = tempfile::tempdir().unwrap();
    let direct = write_inventory(directory.path(), "direct.json", &source);
    let link = directory.path().join("link.json");
    symlink(&direct, &link).unwrap();
    let current = write_inventory(directory.path(), "current.json", &source);
    let error = compare(
        Request {
            baseline: link,
            current: current.clone(),
            output: directory.path().join("link-output"),
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("direct regular file"));

    let oversized = directory.path().join("oversized.json");
    fs::write(&oversized, vec![b' '; MAX_REPORT_BYTES as usize + 1]).unwrap();
    let error = compare(
        Request {
            baseline: oversized,
            current: current.clone(),
            output: directory.path().join("oversized-output"),
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("size is outside"));

    let existing = directory.path().join("existing");
    fs::create_dir(&existing).unwrap();
    let error = compare(
        Request {
            baseline: direct.clone(),
            current: current.clone(),
            output: existing,
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("existing paths are preserved"));

    let interrupted = directory.path().join("interrupted");
    let error = compare(
        Request {
            baseline: direct,
            current,
            output: interrupted.clone(),
        },
        &AtomicBool::new(true),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("interrupted before input"));
    assert!(!interrupted.exists());
}
