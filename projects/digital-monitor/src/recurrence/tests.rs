use std::os::unix::fs::{PermissionsExt, symlink};

use super::*;

fn burst_metrics(observations: &[(usize, u64, u64, u64, u64)]) -> Value {
    let mut families = [[0u64; 3]; 5];
    let mut bins = BTreeMap::<u64, [[u64; 3]; 5]>::new();
    for &(family, frequency, incidence, records, samples) in observations {
        families[family][0] += incidence;
        families[family][1] += records;
        families[family][2] += samples;
        let bin = bins.entry(frequency).or_insert([[0; 3]; 5]);
        bin[family] = [incidence, records, samples];
    }
    let group_count = families.iter().map(|family| family[0]).sum::<u64>();
    let record_count = families.iter().map(|family| family[1]).sum::<u64>();
    let sample_count = families.iter().map(|family| family[2]).sum::<u64>();
    json!({
        "protocol_identity_established":false,
        "settings":expected_settings(),
        "family_order":orchestrate::BURST_FAMILY_LABELS,
        "accounting":{
            "archive_record_count":record_count,
            "squelch_record_count":record_count,
            "scan_window_record_count":0,
            "archive_complex_sample_count":sample_count,
            "squelch_complex_sample_count":sample_count,
            "scan_window_complex_sample_count":0,
            "feature_eligible_record_count":record_count,
            "feature_insufficient_record_count":0,
            "group_count":group_count,
            "grouped_squelch_record_count":record_count
        },
        "families":families.map(|family| json!({
            "group_count":family[0],
            "record_count":family[1],
            "complex_sample_count":family[2]
        })),
        "frequency_bins":bins.into_iter().map(|(frequency, counts)| json!({
            "frequency_mhz":frequency,
            "group_incidence_count":counts.map(|family| family[0]),
            "record_count":counts.map(|family| family[1]),
            "complex_sample_count":counts.map(|family| family[2])
        })).collect::<Vec<_>>()
    })
}

fn complete_attempt(index: u64, observations: &[(usize, u64, u64, u64, u64)]) -> Value {
    json!({
        "index":index,
        "phase":"finished",
        "capture_state":"complete",
        "analysis_state":"complete",
        "interrupted":false,
        "analysis":{
            "overall_state":"complete",
            "metrics":{
                "metrics_schema_version":14,
                "burst_families":burst_metrics(observations)
            }
        }
    })
}

fn failed_attempt(index: u64) -> Value {
    json!({
        "index":index,
        "phase":"finished",
        "capture_state":"complete",
        "analysis_state":"failed",
        "interrupted":false
    })
}

fn run_report(attempts: Vec<Value>) -> Value {
    let cycles = attempts
        .iter()
        .filter_map(|attempt| attempt.get("index").and_then(Value::as_u64))
        .max()
        .unwrap_or(1);
    let successful = attempts.iter().all(|attempt| {
        attempt.get("capture_state").and_then(Value::as_str) == Some("complete")
            && attempt.get("analysis_state").and_then(Value::as_str) == Some("complete")
            && attempt.get("interrupted").and_then(Value::as_bool) == Some(false)
    });
    json!({
        "schema_version":3,
        "operation":"finite_rx1_multi_protocol_watch",
        "finished":true,
        "plan_completed":true,
        "successful":successful,
        "cycles_requested":cycles,
        "attempts_completed":attempts.len(),
        "center_frequency_hz":2_437_000_000u64,
        "sample_rate_hz":20_000_000u64,
        "bandwidth_hz":20_000_000u64,
        "receive_only":true,
        "bias_tee_requested":false,
        "transmit_requested":false,
        "attempts":attempts
    })
}

fn mixed_complete_attempt(index: u64, observations: &[(usize, u64, u64, u64, u64)]) -> Value {
    let center = if index % 2 == 1 {
        2_435_000_000u64
    } else {
        2_437_000_000u64
    };
    let mut attempt = complete_attempt(index, observations);
    attempt["center_frequency_hz"] = json!(center);
    attempt["source"] = json!({"center_frequency_hz":center});
    attempt
}

fn mixed_failed_attempt(index: u64) -> Value {
    let center = if index % 2 == 1 {
        2_435_000_000u64
    } else {
        2_437_000_000u64
    };
    let mut attempt = failed_attempt(index);
    attempt["center_frequency_hz"] = json!(center);
    attempt["source"] = json!({"center_frequency_hz":center});
    attempt
}

fn mixed_run_report(attempts: Vec<Value>) -> Value {
    let mut report = run_report(attempts);
    report["schema_version"] = json!(4);
    report
        .as_object_mut()
        .unwrap()
        .remove("center_frequency_hz");
    report["center_frequency_schedule_hz"] = json!([2_435_000_000u64, 2_437_000_000u64]);
    report["center_selection"] = json!(MIXED_CENTER_SELECTION);
    report["pending_capture_limit"] = json!(1);
    report
}

fn coded_shadow_complete(s8: u64, s2: u64) -> Value {
    json!({
        "state":"complete",
        "metrics":{
            "request_sha256":"0".repeat(64),
            "replay_sha256":"1".repeat(64),
            "packet_summary_sha256":"2".repeat(64),
            "burst_summary_sha256":"3".repeat(64),
            "candidate_pcap_sha256":"4".repeat(64),
            "validated_pcap_sha256":"5".repeat(64),
            "burst_archive_sha256":"7".repeat(64),
            "decoder_program":coded_shadow_decoder(),
            "input_complex_samples":327680,
            "decoder_processed_complex_samples":327680,
            "channelizer_steps_processed":32768,
            "processed_full_input":true,
            "candidate_records":s8 + s2,
            "validated_ble_packets":s8 + s2,
            "validated_le_1m_packets":0,
            "validated_le_2m_packets":0,
            "validated_coded_s8_packets":s8,
            "validated_coded_s2_packets":s2,
            "invalid_crc_packets":0,
            "unknown_crc_packets":0,
            "malformed_packets":0,
            "unsupported_records":0,
            "truncated_records":0,
            "records_exceeding_snaplen":0,
            "classic_candidate_records":0,
            "classic_upstream_crc_claims":0,
            "scan_window_records":1,
            "burst_archive_truncated_by_limit":false,
            "radio_opened":false,
            "network_requested":false,
            "transmit_requested":false
        }
    })
}

fn coded_shadow_decoder() -> Value {
    json!({
        "schema_version":1,
        "name":"blue-dragon",
        "bytes":123,
        "sha256":"6".repeat(64)
    })
}

fn schema_five_single_run() -> Value {
    let mut attempt = complete_attempt(1, &[]);
    attempt["source"] = json!({"complex_samples":327680});
    attempt["analysis"]["metrics"]["bluetooth"]["decoder_program"] = coded_shadow_decoder();
    attempt["analysis"]["coded_shadow"] = coded_shadow_complete(0, 0);
    let mut report = run_report(vec![attempt]);
    report["schema_version"] = json!(5);
    report["center_frequency_hz"] = json!(2_435_000_000u64);
    report["samples_per_capture"] = json!(327680);
    report["bluetooth_coded_shadow_requested"] = json!(true);
    report
}

fn schema_five_mixed_run() -> Value {
    let mut eligible = mixed_complete_attempt(1, &[]);
    eligible["source"]["complex_samples"] = json!(327680);
    eligible["analysis"]["metrics"]["bluetooth"]["decoder_program"] = coded_shadow_decoder();
    eligible["analysis"]["coded_shadow"] = coded_shadow_complete(0, 0);
    let mut ineligible = mixed_complete_attempt(2, &[]);
    ineligible["source"]["complex_samples"] = json!(327680);
    ineligible["analysis"]["coded_shadow"] = json!({
        "state":"not_eligible",
        "center_frequency_hz":2_437_000_000u64
    });
    let mut report = mixed_run_report(vec![eligible, ineligible]);
    report["schema_version"] = json!(5);
    report["samples_per_capture"] = json!(327680);
    report["bluetooth_coded_shadow_requested"] = json!(true);
    report
}

fn run_inventory(value: &Value) -> (tempfile::TempDir, PathBuf, Value) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("run.json");
    fs::write(&input, serde_json::to_vec(value).unwrap()).unwrap();
    let output = directory.path().join("inventory");
    inventory(
        Request {
            run_json: input,
            output: output.clone(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let report = serde_json::from_slice(&fs::read(output.join(OUTPUT_NAME)).unwrap()).unwrap();
    (directory, output, report)
}

fn expect_rejection(value: &Value, expected: &str) {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("run.json");
    fs::write(&input, serde_json::to_vec(value).unwrap()).unwrap();
    let error = inventory(
        Request {
            run_json: input,
            output: directory.path().join("inventory"),
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains(expected), "{error:#}");
}

fn representative_run() -> Value {
    let mut attempts = Vec::new();
    for index in 1..=10 {
        let attempt = match index {
            1 => complete_attempt(index, &[(4, 2446, 5, 5, 50)]),
            2 => complete_attempt(index, &[(4, 2446, 1, 1, 10)]),
            3 => failed_attempt(index),
            4 => complete_attempt(index, &[(0, 2428, 1, 4, 40), (4, 2446, 3, 3, 30)]),
            _ => complete_attempt(index, &[]),
        };
        attempts.push(attempt);
    }
    run_report(attempts)
}

#[test]
fn threshold_uses_three_or_one_fifth_rounded_up() {
    for (eligible, expected) in [(0, 3), (1, 3), (14, 3), (15, 3), (16, 4), (20, 4)] {
        assert_eq!(recurrence_threshold(eligible), expected);
    }
}

#[test]
fn count_summary_preserves_even_odd_and_singleton_medians() {
    let mut singleton = [7];
    let one = summarize_counts(&mut singleton, 7).unwrap();
    assert_eq!(one.lower_median_per_supporting_capture, 7);
    assert_eq!(one.upper_median_per_supporting_capture, 7);

    let mut even = [9, 1, 7, 3];
    let four = summarize_counts(&mut even, 20).unwrap();
    assert_eq!(four.lower_median_per_supporting_capture, 3);
    assert_eq!(four.upper_median_per_supporting_capture, 7);

    let mut odd = [5, 1, 3];
    let three = summarize_counts(&mut odd, 9).unwrap();
    assert_eq!(three.lower_median_per_supporting_capture, 3);
    assert_eq!(three.upper_median_per_supporting_capture, 3);
}

#[test]
fn schema_five_recurrence_validates_separate_coded_shadow_evidence() {
    let (_directory, _output, single) = run_inventory(&schema_five_single_run());
    assert_eq!(single["input"]["watch_schema_version"], 5);
    assert_eq!(single["input"]["eligible_captures"], 1);

    let (_directory, _output, mixed) = run_inventory(&schema_five_mixed_run());
    assert_eq!(mixed["input"]["watch_schema_version"], 5);
    assert_eq!(mixed["input"]["eligible_captures"], 2);
    assert_eq!(mixed["center_strata"].as_array().unwrap().len(), 2);
}

#[test]
fn schema_five_recurrence_rejects_missing_or_unsafe_shadow_evidence() {
    let mut missing = schema_five_single_run();
    missing["attempts"][0]["analysis"]
        .as_object_mut()
        .unwrap()
        .remove("coded_shadow");
    expect_rejection(&missing, "lacks coded-shadow evidence");

    let mut unsafe_report = schema_five_single_run();
    unsafe_report["attempts"][0]["analysis"]["coded_shadow"]["metrics"]["radio_opened"] =
        json!(true);
    expect_rejection(&unsafe_report, "truncation or an unsafe request");

    let mut wrong_phy_partition = schema_five_single_run();
    wrong_phy_partition["attempts"][0]["analysis"]["coded_shadow"]["metrics"]["validated_coded_s8_packets"] =
        json!(1);
    expect_rejection(&wrong_phy_partition, "validated PHY partition differs");

    let mut decoder_mismatch = schema_five_single_run();
    decoder_mismatch["attempts"][0]["analysis"]["coded_shadow"]["metrics"]["decoder_program"]["sha256"] =
        json!("9".repeat(64));
    expect_rejection(&decoder_mismatch, "decoder identity differs");

    let mut input_mismatch = schema_five_single_run();
    input_mismatch["attempts"][0]["analysis"]["coded_shadow"]["metrics"]["input_complex_samples"] =
        json!(655_360);
    expect_rejection(&input_mismatch, "full-input accounting differs");

    let mut wrong_rate = schema_five_single_run();
    wrong_rate["sample_rate_hz"] = json!(10_000_000);
    wrong_rate["bandwidth_hz"] = json!(10_000_000);
    expect_rejection(
        &wrong_rate,
        "outside the 20 MHz aligned coded-shadow bounds",
    );

    let mut short_plan = schema_five_single_run();
    short_plan["samples_per_capture"] = json!(327_670);
    expect_rejection(
        &short_plan,
        "outside the 20 MHz aligned coded-shadow bounds",
    );

    let mut unaligned_plan = schema_five_single_run();
    unaligned_plan["samples_per_capture"] = json!(327_681);
    expect_rejection(
        &unaligned_plan,
        "outside the 20 MHz aligned coded-shadow bounds",
    );

    let mut oversized_plan = schema_five_single_run();
    oversized_plan["samples_per_capture"] = json!(20_000_010);
    expect_rejection(
        &oversized_plan,
        "outside the 20 MHz aligned coded-shadow bounds",
    );
}

#[test]
fn inventory_counts_one_vote_per_capture_and_breaks_streaks() {
    let (_directory, _output, report) = run_inventory(&representative_run());
    assert_eq!(report["input"]["eligible_captures"], json!(9));
    assert_eq!(report["input"]["excluded_attempts"], json!(1));
    assert_eq!(report["threshold"]["minimum_support_captures"], json!(3));
    assert_eq!(report["rows"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["rows"][0]["candidate_family"],
        json!("wideband_multibin_candidate")
    );
    let recurring = &report["rows"][1];
    assert_eq!(recurring["support_captures"], json!(3));
    assert_eq!(recurring["recurring"], json!(true));
    assert_eq!(
        recurring["longest_consecutive_attempt_index_streak"],
        json!(2)
    );
    assert_eq!(recurring["group_incidence"]["total"], json!(9));
    assert_eq!(
        recurring["group_incidence"]["minimum_per_supporting_capture"],
        json!(1)
    );
    assert_eq!(
        recurring["group_incidence"]["maximum_per_supporting_capture"],
        json!(5)
    );
    assert_eq!(
        recurring["group_incidence"]["lower_median_per_supporting_capture"],
        json!(3)
    );
}

#[test]
fn schema_two_and_input_order_produce_the_same_inventory_rows() {
    let baseline = representative_run();
    let (_first_directory, _first_output, first) = run_inventory(&baseline);
    let mut reordered = baseline;
    reordered["schema_version"] = json!(2);
    let attempts = reordered["attempts"].as_array_mut().unwrap();
    attempts.reverse();
    for attempt in attempts {
        attempt.as_object_mut().unwrap().remove("phase");
    }
    let (_second_directory, _second_output, second) = run_inventory(&reordered);
    assert_eq!(first["schema_version"], json!(1));
    assert_eq!(second["schema_version"], json!(1));
    assert!(first.get("center_strata").is_none());
    assert!(second.get("center_strata").is_none());
    assert_eq!(first["threshold"], second["threshold"]);
    assert_eq!(first["accounting"], second["accounting"]);
    assert_eq!(first["rows"], second["rows"]);
}

#[test]
fn mixed_center_inventory_stratifies_denominators_and_opportunity_streaks() {
    let mut attempts = Vec::new();
    for index in 1..=10 {
        let attempt = match index {
            1 | 3 | 5 => mixed_complete_attempt(index, &[(4, 2435, 1, 1, 10)]),
            2 | 4 | 6 | 10 => mixed_complete_attempt(index, &[(4, 2435, 2, 2, 20)]),
            8 => mixed_failed_attempt(index),
            _ => mixed_complete_attempt(index, &[]),
        };
        attempts.push(attempt);
    }
    let (_directory, _output, report) = run_inventory(&mixed_run_report(attempts));
    assert_eq!(report["schema_version"], json!(3));
    assert_eq!(report["input"]["watch_schema_version"], json!(4));
    assert_eq!(
        report["input"]["center_frequency_schedule_hz"],
        json!([2_435_000_000u64, 2_437_000_000u64])
    );
    assert_eq!(report["accounting"]["center_strata"], json!(2));
    assert_eq!(report["accounting"]["observed_rows"], json!(2));
    assert_eq!(report["accounting"]["recurring_rows"], json!(2));
    assert_eq!(
        report["center_strata"][0]["center_frequency_hz"],
        json!(2_435_000_000u64)
    );
    assert_eq!(
        report["center_strata"][0]["input"]["eligible_captures"],
        json!(5)
    );
    assert_eq!(
        report["center_strata"][0]["input"]["excluded_attempts"],
        json!(0)
    );
    assert_eq!(
        report["center_strata"][0]["threshold"]["minimum_support_captures"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][0]["rows"][0]["support_captures"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][0]["rows"][0]["longest_consecutive_center_opportunity_streak"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][1]["center_frequency_hz"],
        json!(2_437_000_000u64)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["eligible_captures"],
        json!(4)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["excluded_attempts"],
        json!(1)
    );
    assert_eq!(
        report["center_strata"][1]["rows"][0]["support_captures"],
        json!(4)
    );
    assert_eq!(
        report["center_strata"][1]["rows"][0]["longest_consecutive_center_opportunity_streak"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][0]["rows"][0]["records"]["total"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][1]["rows"][0]["records"]["total"],
        json!(8)
    );
    assert_eq!(report["cross_center_aggregation_performed"], json!(false));
    assert_eq!(
        report["absolute_frequency_comparability_established"],
        json!(false)
    );
    assert_eq!(
        report["cross_center_coordinate_alignment"]
            ["same_family_absolute_frequency_recurring_pairs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn mixed_center_alignment_separates_absolute_and_center_relative_coordinates() {
    let attempts = (1..=10)
        .map(|index| {
            let observations = match index {
                1 | 3 | 5 => vec![
                    (0, 2434, 1, 4, 40),
                    (1, 2433, 1, 2, 20),
                    (4, 2426, 1, 1, 10),
                ],
                2 | 4 | 6 => vec![
                    (0, 2434, 1, 4, 40),
                    (2, 2433, 1, 1, 10),
                    (4, 2428, 1, 1, 10),
                ],
                7 | 9 => vec![(3, 2440, 1, 1, 10)],
                8 => vec![(3, 2440, 1, 1, 10), (4, 2428, 1, 1, 10)],
                10 => vec![(3, 2440, 1, 1, 10)],
                _ => unreachable!(),
            };
            mixed_complete_attempt(index, &observations)
        })
        .collect();
    let (_directory, _output, report) = run_inventory(&mixed_run_report(attempts));
    let alignment = &report["cross_center_coordinate_alignment"];
    let absolute = alignment["same_family_absolute_frequency_recurring_pairs"]
        .as_array()
        .unwrap();
    assert_eq!(absolute.len(), 1);
    assert_eq!(
        absolute[0]["candidate_family"],
        json!(orchestrate::BURST_FAMILY_LABELS[0])
    );
    assert_eq!(absolute[0]["frequency_mhz"], json!(2434));
    assert_eq!(absolute[0]["center_rows"][0]["support_captures"], json!(3));
    assert_eq!(absolute[0]["center_rows"][0]["eligible_captures"], json!(5));
    assert_eq!(absolute[0]["center_rows"][1]["support_captures"], json!(3));
    assert_eq!(absolute[0]["center_rows"][1]["eligible_captures"], json!(5));

    let relative = alignment["same_family_center_relative_offset_recurring_pairs"]
        .as_array()
        .unwrap();
    assert_eq!(relative.len(), 1);
    assert_eq!(
        relative[0]["candidate_family"],
        json!(orchestrate::BURST_FAMILY_LABELS[4])
    );
    assert_eq!(relative[0]["center_relative_offset_hz"], json!(-9_000_000));
    assert_eq!(relative[0]["center_rows"][0]["frequency_mhz"], json!(2426));
    assert_eq!(relative[0]["center_rows"][1]["frequency_mhz"], json!(2428));
    assert_eq!(relative[0]["center_rows"][0]["support_captures"], json!(3));
    assert_eq!(relative[0]["center_rows"][1]["support_captures"], json!(4));
    assert_eq!(report["cross_center_aggregation_performed"], json!(false));
    assert_eq!(
        report["absolute_frequency_comparability_established"],
        json!(false)
    );
}

#[test]
fn mixed_center_threshold_uses_only_that_centers_eligible_captures() {
    let attempts = (1..=20)
        .map(|index| {
            if matches!(index, 1 | 3 | 5) {
                mixed_complete_attempt(index, &[(4, 2435, 1, 1, 10)])
            } else {
                mixed_complete_attempt(index, &[])
            }
        })
        .collect();
    let (_directory, _output, report) = run_inventory(&mixed_run_report(attempts));
    let primary = &report["center_strata"][0];
    let alternate = &report["center_strata"][1];
    assert_eq!(primary["input"]["eligible_captures"], json!(10));
    assert_eq!(primary["threshold"]["minimum_support_captures"], json!(3));
    assert_eq!(primary["rows"][0]["support_captures"], json!(3));
    assert_eq!(primary["rows"][0]["recurring"], json!(true));
    assert_eq!(alternate["input"]["eligible_captures"], json!(10));
    assert_eq!(alternate["accounting"]["observed_rows"], json!(0));
}

#[test]
fn mixed_center_same_center_absence_breaks_only_that_opportunity_streak() {
    let attempts = (1..=10)
        .map(|index| {
            if matches!(index, 1 | 5 | 9) {
                mixed_complete_attempt(index, &[(4, 2435, 1, 1, 10)])
            } else {
                mixed_complete_attempt(index, &[])
            }
        })
        .collect();
    let (_directory, _output, report) = run_inventory(&mixed_run_report(attempts));
    let row = &report["center_strata"][0]["rows"][0];
    assert_eq!(row["support_captures"], json!(3));
    assert_eq!(
        row["longest_consecutive_center_opportunity_streak"],
        json!(1)
    );
}

#[test]
fn mixed_center_odd_plan_counts_schedule_slots_exactly() {
    let attempts = (1..=5)
        .map(|index| mixed_complete_attempt(index, &[]))
        .collect();
    let (_directory, _output, report) = run_inventory(&mixed_run_report(attempts));
    assert_eq!(
        report["center_strata"][0]["input"]["planned_attempts"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][0]["input"]["attempts_present"],
        json!(3)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["planned_attempts"],
        json!(2)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["attempts_present"],
        json!(2)
    );
}

#[test]
fn mixed_center_sequential_attempts_do_not_require_pipeline_phase_fields() {
    let mut mixed = mixed_run_report(vec![
        mixed_complete_attempt(1, &[]),
        mixed_complete_attempt(2, &[]),
    ]);
    mixed["pending_capture_limit"] = json!(0);
    for attempt in mixed["attempts"].as_array_mut().unwrap() {
        attempt.as_object_mut().unwrap().remove("phase");
    }
    let (_directory, _output, report) = run_inventory(&mixed);
    assert_eq!(report["schema_version"], json!(3));
    assert_eq!(report["input"]["eligible_captures"], json!(2));
}

#[test]
fn mixed_center_incomplete_run_retains_an_empty_stratum_and_unstarted_counts() {
    let mut first = mixed_complete_attempt(1, &[(4, 2435, 1, 1, 10)]);
    first["analysis"]["metrics"]["metrics_schema_version"] = json!(11);
    let mut mixed = mixed_run_report(vec![first, mixed_failed_attempt(2)]);
    mixed["cycles_requested"] = json!(4);
    mixed["plan_completed"] = json!(false);
    mixed["successful"] = json!(false);
    let (_directory, _output, report) = run_inventory(&mixed);
    assert_eq!(report["input"]["eligible_captures"], json!(1));
    assert_eq!(report["input"]["excluded_attempts"], json!(1));
    assert_eq!(
        report["center_strata"][0]["input"]["planned_attempts"],
        json!(2)
    );
    assert_eq!(
        report["center_strata"][0]["input"]["unstarted_attempts"],
        json!(1)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["planned_attempts"],
        json!(2)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["eligible_captures"],
        json!(0)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["excluded_attempts"],
        json!(1)
    );
    assert_eq!(
        report["center_strata"][1]["input"]["unstarted_attempts"],
        json!(1)
    );
    assert_eq!(report["center_strata"][1]["rows"], json!([]));
    let alignment = &report["cross_center_coordinate_alignment"];
    assert_eq!(
        alignment["same_family_absolute_frequency_recurring_pairs"],
        json!([])
    );
    assert_eq!(
        alignment["same_family_center_relative_offset_recurring_pairs"],
        json!([])
    );
}

#[test]
fn mixed_center_contract_rejects_schedule_attempt_source_and_band_drift() {
    let baseline = mixed_run_report(vec![
        mixed_complete_attempt(1, &[(4, 2435, 1, 1, 10)]),
        mixed_complete_attempt(2, &[(4, 2435, 1, 1, 10)]),
    ]);

    let mut duplicate_schedule = baseline.clone();
    duplicate_schedule["center_frequency_schedule_hz"] =
        json!([2_435_000_000u64, 2_435_000_000u64]);
    expect_rejection(&duplicate_schedule, "duplicate center");

    let mut short_schedule = baseline.clone();
    short_schedule["center_frequency_schedule_hz"] = json!([2_435_000_000u64]);
    expect_rejection(&short_schedule, "not length two");

    let mut out_of_range = baseline.clone();
    out_of_range["center_frequency_schedule_hz"] = json!([2_432_000_000u64, 2_437_000_000u64]);
    expect_rejection(&out_of_range, "outside the supported integer-MHz range");

    let mut selection = baseline.clone();
    selection["center_selection"] = json!("capture_defined");
    expect_rejection(&selection, "selection differs");

    let mut legacy_center = baseline.clone();
    legacy_center["center_frequency_hz"] = json!(2_435_000_000u64);
    expect_rejection(&legacy_center, "legacy root center");

    let mut attempt_center = baseline.clone();
    attempt_center["attempts"][1]["center_frequency_hz"] = json!(2_435_000_000u64);
    expect_rejection(&attempt_center, "attempt center differs");

    let mut source_center = baseline.clone();
    source_center["attempts"][1]["source"]["center_frequency_hz"] = json!(2_435_000_000u64);
    expect_rejection(&source_center, "source center differs");

    let mut missing_source = baseline.clone();
    missing_source["attempts"][0]
        .as_object_mut()
        .unwrap()
        .remove("source");
    expect_rejection(&missing_source, "capture lacks its compact source");

    let mut pending = baseline.clone();
    pending["pending_capture_limit"] = json!(2);
    expect_rejection(&pending, "outside zero or one");

    let mut sample_rate = baseline.clone();
    sample_rate["sample_rate_hz"] = json!(10_000_000u64);
    sample_rate["bandwidth_hz"] = json!(10_000_000u64);
    expect_rejection(&sample_rate, "differs from 20 MHz");

    let mut phase = baseline.clone();
    phase["attempts"][0]
        .as_object_mut()
        .unwrap()
        .remove("phase");
    expect_rejection(&phase, "lacks its terminal phase");

    let mut non_prefix = baseline.clone();
    non_prefix["attempts"][1]["index"] = json!(3);
    non_prefix["attempts"][1]["center_frequency_hz"] = json!(2_435_000_000u64);
    non_prefix["attempts"][1]["source"]["center_frequency_hz"] = json!(2_435_000_000u64);
    non_prefix["cycles_requested"] = json!(3);
    non_prefix["plan_completed"] = json!(false);
    non_prefix["successful"] = json!(false);
    expect_rejection(&non_prefix, "not a contiguous prefix");

    let wrong_band = mixed_run_report(vec![
        mixed_complete_attempt(1, &[]),
        mixed_complete_attempt(2, &[(4, 2426, 1, 1, 10)]),
    ]);
    expect_rejection(&wrong_band, "outside the watch capture band");
}

#[test]
fn strict_contract_rejects_family_settings_arrays_indices_and_terminal_drift() {
    let baseline = representative_run();

    let mut family_order = baseline.clone();
    family_order["attempts"][0]["analysis"]["metrics"]["burst_families"]["family_order"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    expect_rejection(&family_order, "family order differs");

    let mut settings = baseline.clone();
    settings["attempts"][0]["analysis"]["metrics"]["burst_families"]["settings"]["minimum_transitions"] =
        json!(7);
    expect_rejection(&settings, "settings differ");

    let mut arrays = baseline.clone();
    arrays["attempts"][0]["analysis"]["metrics"]["burst_families"]
        ["frequency_bins"][0]["record_count"]
        .as_array_mut()
        .unwrap()
        .pop();
    expect_rejection(&arrays, "array is not length five");

    let mut duplicate = baseline.clone();
    duplicate["attempts"][1]["index"] = duplicate["attempts"][0]["index"].clone();
    expect_rejection(&duplicate, "index is duplicated");

    let mut unfinished = baseline.clone();
    unfinished["finished"] = json!(false);
    expect_rejection(&unfinished, "not terminal");

    let mut phase = baseline;
    phase["attempts"][0]["phase"] = json!("analysis_running");
    expect_rejection(&phase, "nonterminal phase");

    let mut false_success = representative_run();
    false_success["successful"] = json!(true);
    expect_rejection(&false_success, "contains an excluded attempt");
}

#[test]
fn complete_attempt_requires_a_supported_metric_schema() {
    let mut missing = representative_run();
    missing["attempts"][0]["analysis"]
        .as_object_mut()
        .unwrap()
        .remove("metrics");
    expect_rejection(&missing, "lacks compact metrics");

    let mut wrong_schema = representative_run();
    wrong_schema["attempts"][0]["analysis"]["metrics"]["metrics_schema_version"] = json!(10);
    expect_rejection(
        &wrong_schema,
        "does not use supported metric schema 11, 12, 13 or 14",
    );

    for schema_version in [11, 12, 13, 14] {
        let mut compatible = representative_run();
        for attempt in compatible["attempts"].as_array_mut().unwrap() {
            if let Some(metrics) = attempt
                .get_mut("analysis")
                .and_then(|analysis| analysis.get_mut("metrics"))
            {
                metrics["metrics_schema_version"] = json!(schema_version);
            }
        }
        let (_directory, _output, report) = run_inventory(&compatible);
        assert_eq!(report["input"]["eligible_captures"], json!(9));
    }
}

#[test]
fn checked_aggregation_rejects_overflow() {
    let observation = BinObservation {
        family_index: 4,
        frequency_mhz: 2446,
        group_incidence_count: u64::MAX,
        record_count: u64::MAX,
        complex_sample_count: u64::MAX,
    };
    let mut accumulator = RowAccumulator::default();
    accumulator.observe(1, observation).unwrap();
    assert!(accumulator.observe(2, observation).is_err());
}

#[test]
fn publication_is_private_path_free_no_replace_and_interruptible() {
    let run = representative_run();
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("private-run-name.json");
    fs::write(&input, serde_json::to_vec(&run).unwrap()).unwrap();
    let output = directory.path().join("inventory");
    inventory(
        Request {
            run_json: input.clone(),
            output: output.clone(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let report_path = output.join(OUTPUT_NAME);
    assert_eq!(
        fs::metadata(&report_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let before = fs::read(&report_path).unwrap();
    let text = String::from_utf8(before.clone()).unwrap();
    assert!(!text.contains("private-run-name"));
    assert!(!text.contains("timestamp"));
    assert!(!text.contains("serial"));
    assert!(
        inventory(
            Request {
                run_json: input.clone(),
                output: output.clone(),
            },
            &AtomicBool::new(false),
        )
        .is_err()
    );
    assert_eq!(fs::read(&report_path).unwrap(), before);

    let interrupted_output = directory.path().join("interrupted");
    assert!(
        inventory(
            Request {
                run_json: input,
                output: interrupted_output.clone(),
            },
            &AtomicBool::new(true),
        )
        .is_err()
    );
    assert!(!interrupted_output.exists());
}

#[test]
fn mixed_publication_is_private_and_path_free() {
    let run = mixed_run_report(vec![
        mixed_complete_attempt(1, &[(4, 2435, 1, 1, 10)]),
        mixed_complete_attempt(2, &[(4, 2435, 1, 1, 10)]),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("private-mixed-run-name.json");
    fs::write(&input, serde_json::to_vec(&run).unwrap()).unwrap();
    let output = directory.path().join("inventory");
    inventory(
        Request {
            run_json: input,
            output: output.clone(),
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    let report_path = output.join(OUTPUT_NAME);
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&report_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = fs::read_to_string(report_path).unwrap();
    assert!(!text.contains("private-mixed-run-name"));
    assert!(!text.contains("timestamp"));
    assert!(!text.contains("serial"));
    assert!(!text.contains("session"));
}

#[test]
fn stable_reader_rejects_a_symlinked_run_report() {
    let directory = tempfile::tempdir().unwrap();
    let direct = directory.path().join("direct.json");
    fs::write(&direct, serde_json::to_vec(&representative_run()).unwrap()).unwrap();
    let linked = directory.path().join("linked.json");
    symlink(&direct, &linked).unwrap();
    let error = inventory(
        Request {
            run_json: linked,
            output: directory.path().join("inventory"),
        },
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("direct regular file"));
}
