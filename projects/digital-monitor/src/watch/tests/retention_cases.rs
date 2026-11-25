use super::*;

fn assert_durable_prune_intent_precedes_failed_transition(root: &Path) {
    assert!(root.join("sessions/000001").is_dir());
    assert!(root.join("quarantine/session-000001.deleting").is_dir());

    let events: Vec<Value> = fs::read_to_string(root.join("attempts.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let event_position = |name: &str| {
        events
            .iter()
            .position(|event| event["event"] == name && event["index"] == 1)
    };
    let intent = event_position("prune_intent").unwrap();
    let failed = event_position("prune_failed").unwrap();
    assert!(intent < failed);
    assert!(event_position("prune_moved").is_none());
    assert!(event_position("prune_completed").is_none());
}

#[test]
fn retention_classification_uses_only_validated_rare_candidate_metrics() {
    let mut ordinary = metrics();
    ordinary.bluetooth.candidate_records = 0;
    ordinary.bluetooth.unknown_crc_packets = 0;
    assert!(retention_reasons(&ordinary).is_empty());

    let mut bluetooth = ordinary.clone();
    bluetooth.bluetooth.candidate_records = 1;
    bluetooth.bluetooth.unknown_crc_packets = 1;
    assert_eq!(
        retention_reasons(&bluetooth),
        vec![RetentionReason::BluetoothCandidateRecords]
    );

    let mut ieee = ordinary.clone();
    ieee.ieee802154.channels[0].sources[4].candidate_records = 1;
    assert_eq!(
        retention_reasons(&ieee),
        vec![RetentionReason::Ieee802154CandidateRecords]
    );

    let mut fsk = ordinary.clone();
    fsk.burst_families.families[2].group_count = 1;
    assert_eq!(
        retention_reasons(&fsk),
        vec![RetentionReason::NarrowbandBinaryFskLikeGroups]
    );

    let mut data_failure = ordinary.clone();
    data_failure
        .wifi
        .decoder_health
        .ofdm
        .rejections
        .data_decode_failed = 1;
    data_failure.wifi.decoder_health.ofdm.data_failures.legacy = 1;
    data_failure.wifi.decoder_health.ofdm.rejected_frames += 1;
    assert_eq!(
        retention_reasons(&data_failure),
        vec![RetentionReason::WifiOfdmDataDecodeFailures]
    );

    let mut invalid_fcs = ordinary.clone();
    invalid_fcs.wifi.decoder_health.ofdm.invalid_fcs = 1;
    assert_eq!(
        retention_reasons(&invalid_fcs),
        vec![RetentionReason::WifiOfdmInvalidFcsFrames]
    );

    let mut both = data_failure;
    both.wifi.decoder_health.ofdm.invalid_fcs = 1;
    assert_eq!(
        retention_reasons(&both),
        vec![
            RetentionReason::WifiOfdmDataDecodeFailures,
            RetentionReason::WifiOfdmInvalidFcsFrames
        ]
    );

    let mut training_only = ordinary;
    training_only
        .wifi
        .decoder_health
        .ofdm
        .rejections
        .strong_long_training_not_found = 1;
    training_only
        .wifi
        .decoder_health
        .ofdm
        .strong_long_training
        .strong_long_reference_not_found = 1;
    training_only.wifi.decoder_health.ofdm.rejected_frames += 1;
    assert!(retention_reasons(&training_only).is_empty());
}

#[test]
fn only_validated_coded_packets_add_the_shadow_retention_reason() {
    let root = run_root();
    let mut shadow_plan = plan(2, 2);
    shadow_plan.bluetooth_coded_shadow = true;
    shadow_plan.center_frequency_schedule_hz = vec![2_435_000_000];
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &shadow_plan,
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured_at_center(session, 2_435_000_000),
        |_, session, _| {
            analysis_index += 1;
            if analysis_index == 1 {
                analyzed_with_coded_shadow(session, 0, 0)
            } else {
                analyzed_with_coded_shadow(session, 0, 1)
            }
        },
    )
    .unwrap();
    assert!(successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["schema_version"], 5);
    assert_eq!(run["attempts"][0]["retention_priority"], "routine");
    assert!(run["attempts"][0].get("retention_reasons").is_none());
    assert_eq!(run["attempts"][1]["retention_priority"], "interesting");
    assert_eq!(
        run["attempts"][1]["retention_reasons"],
        json!(["bluetooth_coded_validated_packets"])
    );
}

#[test]
fn interesting_successes_displace_newer_routine_sessions_within_the_same_limit() {
    let root = run_root();
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &plan(5, 3),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| {
            analysis_index += 1;
            match analysis_index {
                1 | 2 => analyzed_with_binary_fsk_group(session),
                5 => analyzed_with_bluetooth_candidate(session),
                _ => analyzed(session),
            }
        },
    )
    .unwrap();
    assert!(successful);
    for index in [1, 2, 5] {
        assert!(root.path().join(format!("sessions/{index:06}")).is_dir());
    }
    for index in [3, 4] {
        assert!(!root.path().join(format!("sessions/{index:06}")).exists());
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"][0]["retention_priority"], "interesting");
    assert_eq!(
        run["attempts"][0]["retention_reasons"][0],
        "narrowband_binary_fsk_like_groups"
    );
    assert_eq!(run["attempts"][2]["retention_priority"], "routine");
    assert_eq!(run["attempts"][2]["retained"], false);
    assert_eq!(run["attempts"][4]["retention_priority"], "interesting");
    assert_eq!(
        run["attempts"][4]["retention_reasons"][0],
        "bluetooth_candidate_records"
    );
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(journal.contains("\"prune_reason\":\"successful_session_count_limit\""));
    assert!(journal.contains("\"retention_priority\":\"routine\""));
}

#[test]
fn retention_journals_intent_before_moving_or_deleting_session() {
    let root = run_root();
    let successful = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert!(successful);
    assert!(!root.path().join("sessions/000001").exists());
    assert!(root.path().join("sessions/000002").is_dir());

    let events: Vec<Value> = fs::read_to_string(root.path().join("attempts.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let position = |name: &str, index: usize| {
        events
            .iter()
            .position(|event| {
                event["event"] == name
                    && (event["index"] == index || event["attempt"]["index"] == index)
            })
            .unwrap()
    };
    let attempt_finished = position("attempt_finished", 2);
    let prune_intent = position("prune_intent", 1);
    let prune_moved = position("prune_moved", 1);
    let prune_completed = position("prune_completed", 1);
    assert!(attempt_finished < prune_intent);
    assert!(prune_intent < prune_moved);
    assert!(prune_moved < prune_completed);
}

#[test]
fn malformed_family_metrics_fail_before_watch_retention_eligibility() {
    let root = run_root();
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |source, session, _| {
            analysis_index += 1;
            if analysis_index == 1 {
                return analyzed(session);
            }

            let output = session.join("analysis");
            write_metric_fixture(&output, source);
            let family_path = output.join("burst-families/burst-families.json");
            let mut family = report::read_json(&family_path).unwrap();
            family["families"][2]["group_count"] = json!(1);
            write_json(&family_path, &family);

            match attach_protocol_metrics(&output, source, metric_terminal(), 1) {
                Ok(_) => panic!("malformed family metrics unexpectedly passed validation"),
                Err(failed) => *failed,
            }
        },
    )
    .unwrap();
    assert!(!successful);

    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    let rejected = &run["attempts"][1];
    assert_eq!(rejected["analysis_state"], "failed");
    assert!(rejected.get("retention_priority").is_none());
    assert!(rejected.get("retention_reasons").is_none());
    assert!(rejected["analysis"].get("metrics").is_none());
    assert!(
        rejected["analysis_detail"]
            .as_str()
            .unwrap()
            .contains("before retention eligibility")
    );
    assert!(root.path().join("sessions/000001").is_dir());
    assert!(root.path().join("sessions/000002").is_dir());
}

#[test]
fn sequential_prune_intent_is_durable_before_a_failed_transition() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let result = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        move |_, session, _| {
            if session_index(session) == 2 {
                fs::create_dir(root_path.join("quarantine/session-000001.deleting")).unwrap();
            }
            analyzed(session)
        },
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("quarantine target already exists"));
    assert_durable_prune_intent_precedes_failed_transition(root.path());
}

#[test]
fn depth_one_prune_intent_is_durable_before_a_failed_transition() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let result = run_pipelined_plan(
        root.path(),
        &pipeline_plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        |_, session, _| captured(session),
        move |_, session, _| {
            if session_index(session) == 2 {
                fs::create_dir(root_path.join("quarantine/session-000001.deleting")).unwrap();
            }
            analyzed(session)
        },
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("quarantine target already exists"));
    assert_durable_prune_intent_precedes_failed_transition(root.path());
}

#[test]
fn rare_wifi_ofdm_failures_displace_routine_sessions_with_exact_reasons() {
    let root = run_root();
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &plan(5, 3),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| {
            analysis_index += 1;
            match analysis_index {
                1 => analyzed_with_wifi_ofdm_data_failure(session),
                2 => analyzed_with_wifi_ofdm_invalid_fcs(session),
                _ => analyzed(session),
            }
        },
    )
    .unwrap();
    assert!(successful);
    for index in [1, 2, 5] {
        assert!(root.path().join(format!("sessions/{index:06}")).is_dir());
    }
    for index in [3, 4] {
        assert!(!root.path().join(format!("sessions/{index:06}")).exists());
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(
        run["retention_policy"]["interesting_reasons"],
        json!([
            "bluetooth_candidate_records",
            "ieee802154_candidate_records",
            "narrowband_binary_fsk_like_groups",
            "wifi_ofdm_data_decode_failures",
            "wifi_ofdm_invalid_fcs_frames"
        ])
    );
    assert_eq!(
        run["attempts"][0]["retention_reasons"],
        json!(["wifi_ofdm_data_decode_failures"])
    );
    assert_eq!(
        run["attempts"][1]["retention_reasons"],
        json!(["wifi_ofdm_invalid_fcs_frames"])
    );
    assert_eq!(run["attempts"][2]["retention_priority"], "routine");
    assert_eq!(run["attempts"][2]["retained"], false);
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(journal.contains("\"retention_reasons\":[\"wifi_ofdm_data_decode_failures\"]"));
    assert!(journal.contains("\"retention_reasons\":[\"wifi_ofdm_invalid_fcs_frames\"]"));
}

#[test]
fn wifi_ofdm_priority_composes_with_mixed_center_retention_floor() {
    let root = run_root();
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &alternating_plan(4, 2, 0),
        &AtomicBool::new(false),
        || Ok(()),
        |center_frequency_hz, session, _| captured_at_center(session, center_frequency_hz),
        |_, session, _| {
            analysis_index += 1;
            match analysis_index {
                1 => analyzed_with_wifi_ofdm_data_failure(session),
                3 => analyzed_with_wifi_ofdm_invalid_fcs(session),
                _ => analyzed(session),
            }
        },
    )
    .unwrap();
    assert!(successful);
    assert!(root.path().join("sessions/000003").is_dir());
    assert!(root.path().join("sessions/000004").is_dir());
    assert!(!root.path().join("sessions/000001").exists());
    assert!(!root.path().join("sessions/000002").exists());
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"][2]["center_frequency_hz"], 2_435_000_000u64);
    assert_eq!(run["attempts"][2]["retention_priority"], "interesting");
    assert_eq!(run["attempts"][3]["center_frequency_hz"], 2_437_000_000u64);
    assert_eq!(run["attempts"][3]["retention_priority"], "routine");
}

#[test]
fn pipelined_alternating_retention_preserves_early_2437_ofdm_failure() {
    let root = run_root();
    let successful = run_pipelined_plan(
        root.path(),
        &alternating_plan(10, 4, 1),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        |center_frequency_hz, session, _| captured_at_center(session, center_frequency_hz),
        |source, session, _| {
            let index = session_index(session);
            if source.center_frequency_hz == 2_435_000_000 {
                analyzed_with_bluetooth_candidate(session)
            } else if index == 2 {
                analyzed_with_wifi_ofdm_data_failure(session)
            } else {
                analyzed(session)
            }
        },
    )
    .unwrap();
    assert!(successful);
    for index in [2, 5, 7, 9] {
        assert!(root.path().join(format!("sessions/{index:06}")).is_dir());
    }
    for index in [1, 3, 4, 6, 8, 10] {
        assert!(!root.path().join(format!("sessions/{index:06}")).exists());
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["schema_version"], 4);
    assert_eq!(run["pending_capture_limit"], 1);
    assert_eq!(run["attempts"][1]["center_frequency_hz"], 2_437_000_000u64);
    assert_eq!(run["attempts"][1]["retained"], true);
    assert_eq!(
        run["attempts"][1]["retention_reasons"],
        json!(["wifi_ofdm_data_decode_failures"])
    );
    for index in [4usize, 6, 8] {
        assert_eq!(run["attempts"][index - 1]["retention_priority"], "routine");
        assert_eq!(run["attempts"][index - 1]["retained"], false);
    }
}

#[test]
fn pipelined_retention_preserves_early_ofdm_failure_under_routine_pressure() {
    let root = run_root();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(10, 4),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| {
            if session_index(session) == 2 {
                analyzed_with_wifi_ofdm_data_failure(session)
            } else {
                analyzed(session)
            }
        },
    )
    .unwrap();
    assert!(successful);
    for index in [2, 8, 9, 10] {
        assert!(root.path().join(format!("sessions/{index:06}")).is_dir());
    }
    for index in [1, 3, 4, 5, 6, 7] {
        assert!(!root.path().join(format!("sessions/{index:06}")).exists());
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["schema_version"], 3);
    assert_eq!(run["pending_capture_limit"], 1);
    assert_eq!(run["attempts"][1]["retained"], true);
    assert_eq!(
        run["attempts"][1]["retention_reasons"],
        json!(["wifi_ofdm_data_decode_failures"])
    );
    for index in [1usize, 3, 4, 5, 6, 7] {
        assert_eq!(run["attempts"][index - 1]["retention_priority"], "routine");
        assert_eq!(run["attempts"][index - 1]["retained"], false);
    }
}

#[test]
fn ieee_membership_compact_mapping_preserves_typed_partition() {
    let summary = json!({
        "membership_partitions":[{
            "source_membership_mask":37,
            "source_membership_count":3,
            "identity_record_count":11,
            "valid_fcs_record_count":7,
            "invalid_fcs_record_count":3,
            "invalid_length_record_count":1
        }]
    });
    let partitions = ieee_membership_metrics(&summary).unwrap();
    assert_eq!(partitions.len(), 1);
    let partition = &partitions[0];
    assert_eq!(partition.source_membership_mask, 37);
    assert_eq!(partition.source_membership_count, 3);
    assert_eq!(partition.identity_records, 11);
    assert_eq!(partition.valid_fcs_records, 7);
    assert_eq!(partition.invalid_fcs_records, 3);
    assert_eq!(partition.invalid_length_records, 1);
}

#[test]
fn all_interesting_sessions_remain_bounded_by_the_existing_success_limit() {
    let root = run_root();
    let successful = run_plan(
        root.path(),
        &plan(4, 2),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| analyzed_with_bluetooth_candidate(session),
    )
    .unwrap();
    assert!(successful);
    assert_eq!(
        fs::read_dir(root.path().join("sessions")).unwrap().count(),
        2
    );
    assert!(root.path().join("sessions/000003").is_dir());
    assert!(root.path().join("sessions/000004").is_dir());
}

#[test]
fn byte_pressure_prunes_routine_before_interesting_and_records_its_reason() {
    let root = run_root();
    let mut bounded = plan(2, 2);
    bounded.retained_limit_bytes = 400_000;
    let mut analysis_index = 0usize;
    let successful = run_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            let result = captured(session);
            fs::write(session.join("padding"), vec![0u8; 250_000]).unwrap();
            result
        },
        |_, session, _| {
            analysis_index += 1;
            if analysis_index == 1 {
                analyzed_with_bluetooth_candidate(session)
            } else {
                analyzed(session)
            }
        },
    )
    .unwrap();
    assert!(successful);
    assert!(root.path().join("sessions/000001").is_dir());
    assert!(!root.path().join("sessions/000002").exists());
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"][0]["retained"], true);
    assert_eq!(run["attempts"][1]["retained"], false);
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(journal.contains("\"prune_reason\":\"retained_byte_limit_after_analysis\""));
    assert!(journal.contains("\"retention_priority\":\"routine\""));
}
