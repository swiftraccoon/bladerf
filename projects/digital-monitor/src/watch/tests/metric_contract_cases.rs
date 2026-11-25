use super::*;

#[test]
fn protocol_metrics_reject_source_schema_count_and_outcome_mismatches() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let output = directory.path().join("analysis");
    write_metric_fixture(&output, &accepted);
    let terminal = metric_terminal();
    let valid = protocol_metrics(&output, &accepted, &terminal, 1).unwrap();
    assert_eq!(valid.metrics_schema_version, 14);
    assert_eq!(valid.wifi.validated_frames, 5);
    assert_eq!(valid.wifi.eapol.frame_count, 1);
    assert_eq!(valid.wifi.eapol.packet_types.key, 1);
    assert_eq!(valid.wifi.eapol.parsed_key_header_count, 1);
    assert_eq!(valid.wifi.eapol.ack_key_count, 1);
    assert_eq!(valid.wifi.eapol.mic_key_count, 0);
    assert_eq!(valid.wifi.advertisements.eligible_frame_count, 3);
    assert_eq!(valid.wifi.advertisements.beacon_frames, 3);
    assert_eq!(
        valid
            .wifi
            .advertisements
            .phy
            .eht_capabilities_element_frames,
        3
    );
    assert_eq!(valid.wifi.advertisements.security.pmf_capable_frames, 3);
    assert_eq!(valid.wifi.frame_types.management.beacon, 3);
    assert_eq!(valid.wifi.frame_types.control.block_ack, 1);
    assert_eq!(valid.wifi.block_ack.frame_count, 1);
    assert_eq!(valid.wifi.block_ack.complete_compressed_single_tid_count, 1);
    assert_eq!(valid.wifi.block_ack.tids.tid_4, 1);
    assert_eq!(valid.wifi.block_ack.set_bitmap_bits_total, 5);
    assert_eq!(valid.wifi.block_ack.nonzero_bitmap_frames, 1);
    assert_eq!(valid.wifi.frame_types.data.data, 1);
    assert_eq!(valid.wifi.decoder_health.ofdm.valid_frames, 2);
    assert_eq!(valid.wifi.decoder_health.ofdm.invalid_fcs, 1);
    assert_eq!(valid.wifi.decoder_health.ofdm.rejected_frames, 7);
    assert_eq!(
        valid
            .wifi
            .decoder_health
            .ofdm
            .rejections
            .signal_or_probe_rejected,
        2
    );
    assert_eq!(
        valid.wifi.decoder_health.ofdm.rejections.data_decode_failed,
        3
    );
    assert_eq!(valid.wifi.decoder_health.ofdm.data_failures.legacy, 1);
    assert_eq!(valid.wifi.decoder_health.ofdm.data_failures.ht_bcc, 1);
    assert_eq!(valid.wifi.decoder_health.ofdm.data_failures.ht_ldpc, 1);
    assert_eq!(
        valid
            .wifi
            .decoder_health
            .ofdm
            .strong_long_training
            .strong_long_reference_not_found,
        1
    );
    assert_eq!(valid.wifi.decoder_health.dsss.valid_frames, 3);
    assert!(valid.wifi.decoder_health.diagnostic_counts_are_lower_bounds);
    assert_eq!(
        valid.wifi.decoder_health.pending_frame_limit,
        WIFI_PENDING_FRAME_LIMIT
    );
    assert_eq!(
        valid.wifi.decoder_health.buffer_sample_limit,
        WIFI_BUFFER_SAMPLE_LIMIT
    );
    let serialized_metrics = serde_json::to_value(&valid).unwrap();
    assert_eq!(serialized_metrics["metrics_schema_version"], json!(14));
    assert_eq!(
        serialized_metrics["bluetooth"]["duplicate_accounting"],
        json!({
            "normalized_frame_body_capacity": BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
            "distinct_validated_frame_bodies": 0,
            "repeated_validated_packets": 0,
            "unresolved_validated_packets": 0,
            "capacity_reached": false,
            "exact": true
        })
    );
    assert_eq!(serialized_metrics["wifi"]["eapol"]["frame_count"], json!(1));
    assert_eq!(
        serialized_metrics["wifi"]["eapol"]["packet_types"]["key"],
        json!(1)
    );
    assert_eq!(
        serialized_metrics["wifi"]["advertisements"]["phy"]["eht_capabilities_element_frames"],
        json!(3)
    );
    assert_eq!(
        serialized_metrics["wifi"]["frame_types"]["control"]["block_ack"],
        json!(1)
    );
    assert_eq!(
        serialized_metrics["wifi"]["block_ack"]["tids"]["tid_4"],
        json!(1)
    );
    assert_eq!(
        serialized_metrics["wifi"]["decoder_health"]["ofdm"]["rejected_frames"],
        json!(7)
    );
    assert_eq!(
        serialized_metrics["wifi"]["decoder_health"]["ofdm"]["rejections"],
        json!({
            "signal_or_probe_rejected": 2,
            "greenfield_header_unsupported": 0,
            "greenfield_probe_unrecognized": 0,
            "mixed_header_unsupported": 0,
            "legacy_fallback_unreservable": 0,
            "data_decode_failed": 3,
            "pending_capacity_rejected": 0,
            "strong_long_training_not_found": 2
        })
    );
    assert_eq!(
        serialized_metrics["wifi"]["decoder_health"]["ofdm"]["data_failures"],
        json!({"legacy": 1, "ht_bcc": 1, "ht_ldpc": 1})
    );
    assert_eq!(
        serialized_metrics["wifi"]["decoder_health"]["ofdm"]["strong_long_training"],
        json!({
            "strong_short_periodicity_timeout": 1,
            "strong_long_repeat_not_found": 0,
            "strong_long_reference_not_found": 1,
            "strong_long_coordinates_unavailable": 0
        })
    );
    assert_eq!(valid.rf_survey.region_count, 1);
    assert_eq!(valid.ieee802154.channels.len(), 3);
    assert_eq!(valid.ieee802154.channels[0].channelized_input_bytes, 16_000);
    assert_eq!(valid.ieee802154.channels[0].sources.len(), 6);
    assert_eq!(valid.ieee802154.channels[0].sources[0].decoder_lane, "hard");
    assert_eq!(valid.ieee802154.channels[0].sources[1].decoder_lane, "soft");
    assert_eq!(
        valid.ieee802154.channels[0].sources[2].carrier_offset_hz,
        -100_000
    );
    assert_eq!(
        valid.ieee802154.channels[0].sources[5].carrier_offset_hz,
        100_000
    );
    assert_eq!(
        valid.ieee802154.channels[0].sources[0].candidate_archive_bytes,
        24
    );
    assert_eq!(
        valid.ieee802154.channels[0].channelized_input_sha256.len(),
        64
    );
    assert_eq!(
        valid.ieee802154.channels[0].sources[0]
            .candidate_archive_sha256
            .len(),
        64
    );
    assert_eq!(
        valid.ieee802154.channels[0].comparison_summary_sha256.len(),
        64
    );
    assert_eq!(valid.bluetooth.unknown_crc_packets, 1);
    assert_eq!(valid.bluetooth.decoder_program.name, "blue-dragon");
    assert_eq!(valid.bluetooth.decoder_program.sha256, "d".repeat(64));
    let serialized = serde_json::to_string(&valid.bluetooth.duplicate_accounting).unwrap();
    for forbidden in [
        "body_hex",
        "body_sha256",
        "address",
        "payload",
        "timestamp",
        "path",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    assert_eq!(valid.wifi.summary_sha256.len(), 64);
    assert_eq!(valid.burst_families.accounting.group_count, 1);
    assert_eq!(valid.burst_families.families[4].complex_sample_count, 4);
    assert_eq!(valid.burst_families.frequency_bins[0].frequency_mhz, 2437);
    assert_eq!(valid.burst_families.frequency_bins[0].record_count[4], 2);

    let serialized = serde_json::to_string(&valid.bluetooth.decoder_program).unwrap();
    assert!(!serialized.contains("path"));
    assert!(!serialized.contains("device"));
    assert!(!serialized.contains("inode"));
    let serialized = serde_json::to_string(&valid.burst_families).unwrap();
    for forbidden in ["path", "timestamp", "device", "inode"] {
        assert!(!serialized.contains(forbidden));
    }
    let serialized = serde_json::to_string(&valid.wifi.decoder_health).unwrap();
    assert!(!serialized.contains("diagnostic_counts\""));
    for forbidden in ["path", "timestamp", "device", "inode"] {
        assert!(!serialized.contains(forbidden));
    }
    let serialized = serde_json::to_string(&valid.wifi.eapol).unwrap();
    for forbidden in [
        "address",
        "payload",
        "body_hex",
        "nonce",
        "mic_bytes",
        "replay_counter",
        "handshake",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    let serialized = serde_json::to_string(&valid.wifi.advertisements).unwrap();
    for forbidden in [
        "ssid",
        "bssid",
        "address",
        "data_hex",
        "suite",
        "oui",
        "channel",
        "information_element_ids",
        "payload",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    let block_ack = serde_json::to_value(&valid.wifi.block_ack).unwrap();
    let mut block_ack_fields = block_ack
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    block_ack_fields.sort_unstable();
    assert_eq!(
        block_ack_fields,
        [
            "complete_compressed_single_tid_count",
            "frame_count",
            "full_bitmap_frames",
            "nonzero_bitmap_frames",
            "other_variant_count",
            "set_bitmap_bits_total",
            "tids",
            "trailing_compressed_single_tid_count",
            "truncated_compressed_single_tid_count",
            "truncated_control_count",
            "zero_bitmap_frames",
        ]
    );
    let mut block_ack_tid_fields = block_ack["tids"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    block_ack_tid_fields.sort_unstable();
    assert_eq!(
        block_ack_tid_fields,
        [
            "tid_0", "tid_1", "tid_10", "tid_11", "tid_12", "tid_13", "tid_14", "tid_15", "tid_2",
            "tid_3", "tid_4", "tid_5", "tid_6", "tid_7", "tid_8", "tid_9",
        ]
    );
    let serialized = serde_json::to_string(&block_ack).unwrap();
    for forbidden in [
        "address",
        "starting_sequence",
        "sequence_control",
        "raw_bitmap",
        "bitmap_hex",
        "payload",
        "identifier_like_key",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    let frame_types = serde_json::to_value(&valid.wifi.frame_types).unwrap();
    let mut groups = frame_types
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    groups.sort_unstable();
    assert_eq!(groups, ["control", "data", "management"]);
    let mut management_fields = frame_types["management"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    management_fields.sort_unstable();
    assert_eq!(
        management_fields,
        [
            "action",
            "action_no_ack",
            "association_request",
            "association_response",
            "atim",
            "authentication",
            "beacon",
            "deauthentication",
            "disassociation",
            "probe_request",
            "probe_response",
            "reassociation_request",
            "reassociation_response",
            "reserved_management",
        ]
    );
    let mut control_fields = frame_types["control"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    control_fields.sort_unstable();
    assert_eq!(
        control_fields,
        [
            "ack",
            "block_ack",
            "block_ack_request",
            "cf_end",
            "cf_end_ack",
            "control_wrapper",
            "cts",
            "other_control",
            "ps_poll",
            "rts",
        ]
    );
    let mut data_fields = frame_types["data"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    data_fields.sort_unstable();
    assert_eq!(
        data_fields,
        ["data", "null_data", "other_data", "qos_data", "qos_null"]
    );
    let serialized = serde_json::to_string(&frame_types).unwrap();
    for forbidden in [
        "address",
        "ssid",
        "bssid",
        "payload",
        "source_file",
        "timestamp",
        "identifier_like_key",
    ] {
        assert!(!serialized.contains(forbidden));
    }
    let serialized = serde_json::to_string(&valid.ieee802154).unwrap();
    for forbidden in ["path", "source_file", "output_file", "timestamp", "inode"] {
        assert!(!serialized.contains(forbidden));
    }
    let ieee = serde_json::to_value(&valid.ieee802154).unwrap();
    let mut ieee_fields = ieee
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    ieee_fields.sort_unstable();
    assert_eq!(ieee_fields, ["channelize_summary_sha256", "channels"]);
    let mut channel_fields = ieee["channels"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    channel_fields.sort_unstable();
    assert_eq!(
        channel_fields,
        [
            "channel",
            "channelized_input_bytes",
            "channelized_input_sha256",
            "comparison_summary_sha256",
            "duplicate_invalid_fcs_records",
            "duplicate_invalid_length_records",
            "duplicate_records",
            "duplicate_valid_fcs_records",
            "identity_union_invalid_fcs_records",
            "identity_union_invalid_length_records",
            "identity_union_records",
            "identity_union_valid_fcs_records",
            "membership_partitions",
            "source_candidate_records_sum",
            "source_invalid_fcs_records_sum",
            "source_invalid_length_records_sum",
            "source_valid_fcs_records_sum",
            "sources",
        ]
    );
    let mut source_fields = ieee["channels"][0]["sources"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    source_fields.sort_unstable();
    assert_eq!(
        source_fields,
        [
            "candidate_archive_bytes",
            "candidate_archive_sha256",
            "candidate_records",
            "carrier_offset_hz",
            "decoder_lane",
            "inspection_summary_sha256",
            "invalid_fcs_records",
            "invalid_length_records",
            "source_index",
            "source_role",
            "valid_fcs_records",
            "validated_psdu_bytes",
        ]
    );
    let membership = serde_json::to_value(IeeeMembershipPartitionMetrics {
        source_membership_mask: 3,
        source_membership_count: 2,
        identity_records: 1,
        valid_fcs_records: 1,
        invalid_fcs_records: 0,
        invalid_length_records: 0,
    })
    .unwrap();
    let mut membership_fields = membership
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    membership_fields.sort_unstable();
    assert_eq!(
        membership_fields,
        [
            "identity_records",
            "invalid_fcs_records",
            "invalid_length_records",
            "source_membership_count",
            "source_membership_mask",
            "valid_fcs_records",
        ]
    );

    let wifi_path = output.join("wifi/summary.json");
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["schema_version"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["frames_by_type"]["identifier_like_key"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["frames_by_type"]["block_ack"] = json!(2);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["block_ack"]["complete_compressed_single_tid_count"] = json!(0);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["block_ack"]["tids"]["tid_0"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["block_ack"]["set_bitmap_bits_total"] = json!(65);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["block_ack"]["starting_sequence_control"] = json!(0);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["block_ack"]["tids"]["arbitrary"] = json!(0);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["eapol"]["complete_header_count"] = json!(0);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["eapol"]["complete_body_count"] = json!(0);
    wifi["eapol"]["truncated_body_count"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["eapol"]["packet_types"]["other"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["eapol"]["parsed_key_header_count"] = json!(0);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["eapol"]["nonce"] = json!("not-retained");
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["advertisements"]["probe_response_frames"] = json!(1);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["advertisements"]["security"]["pmf_capable_frames"] = json!(4);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wifi = report::read_json(&wifi_path).unwrap();
    wifi["advertisements"]["raw_information_elements"] = json!([]);
    write_json(&wifi_path, &wifi);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    let rf_path = output.join("rf-survey/spectrum.json");
    let mut rf = report::read_json(&rf_path).unwrap();
    rf["activity_detection"]["cells"][10]["activity_triggered"] = json!(false);
    write_json(&rf_path, &rf);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut rf = report::read_json(&rf_path).unwrap();
    rf["activity_detection"]["regions"][0]["maximum_mean_excess_db"] = json!(6.5);
    write_json(&rf_path, &rf);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let channel_path = output.join("ieee802154/channelized/channel-16.cf32");
    let mut channel_bytes = fs::read(&channel_path).unwrap();
    channel_bytes[0] ^= 1;
    fs::write(&channel_path, channel_bytes).unwrap();
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let archive_path = output.join("ieee802154/channel-16-soft-cfo-pos100000.i154raw");
    let mut archive_bytes = fs::read(&archive_path).unwrap();
    archive_bytes[23] ^= 1;
    fs::write(&archive_path, archive_bytes).unwrap();
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let ieee_summary_path =
        output.join("ieee802154/channel-16-soft-cfo-pos100000-inspection/summary.json");
    let mut ieee = report::read_json(&ieee_summary_path).unwrap();
    ieee["source_sha256"] = json!("0".repeat(64));
    write_json(&ieee_summary_path, &ieee);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let comparison_path = output.join("ieee802154/channel-16-comparison/summary.json");
    let mut comparison = report::read_json(&comparison_path).unwrap();
    comparison["sources"][5]["source_sha256"] = json!("0".repeat(64));
    write_json(&comparison_path, &comparison);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut comparison = report::read_json(&comparison_path).unwrap();
    comparison["identity_union_record_count"] = json!(1);
    write_json(&comparison_path, &comparison);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let decode_path = output.join("wifi/candidates.decode.json");
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["processed_samples"] = json!(accepted.complex_samples - 1);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["valid_frames"] = json!(3);
    decode["ofdm"]["dropped_frames"] = json!(1);
    write_json(&decode_path, &decode);
    let with_drop = protocol_metrics(&output, &accepted, &terminal, 1).unwrap();
    assert_eq!(with_drop.wifi.decoder_health.ofdm.valid_frames, 3);
    assert_eq!(with_drop.wifi.decoder_health.ofdm.dropped_frames, 1);

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["dropped_frames"] = json!(3);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["dsss"]["valid_frames"] = json!(4);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]
        .as_object_mut()
        .unwrap()
        .remove("invalid_fcs");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"].as_object_mut().unwrap().remove("rejections");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["rejections"]["unexpected"] = json!(0);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["rejections"]["data_decode_failed"] = json!(4);
    decode["ofdm"]["data_failures"]["legacy"] = json!(2);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["rejected_frames"] = json!(u64::MAX);
    decode["ofdm"]["rejections"]["signal_or_probe_rejected"] = json!(u64::MAX);
    decode["ofdm"]["rejections"]["data_decode_failed"] = json!(1);
    decode["ofdm"]["data_failures"] = json!({"legacy":1,"ht_bcc":0,"ht_ldpc":0});
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]
        .as_object_mut()
        .unwrap()
        .remove("data_failures");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["data_failures"]["legacy"] = json!(2);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]
        .as_object_mut()
        .unwrap()
        .remove("strong_long_training");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["strong_long_training"]["unexpected"] = json!(0);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["strong_long_training"]["strong_long_repeat_not_found"] = json!("not-a-counter");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["strong_long_training"]["strong_long_reference_not_found"] = json!(2);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["ofdm"]["rejected_frames"] = json!(u64::MAX);
    decode["ofdm"]["rejections"]["signal_or_probe_rejected"] = json!(0);
    decode["ofdm"]["rejections"]["data_decode_failed"] = json!(0);
    decode["ofdm"]["rejections"]["strong_long_training_not_found"] = json!(u64::MAX);
    decode["ofdm"]["data_failures"] = json!({"legacy":0,"ht_bcc":0,"ht_ldpc":0});
    decode["ofdm"]["strong_long_training"]["strong_short_periodicity_timeout"] = json!(u64::MAX);
    decode["ofdm"]["strong_long_training"]["strong_long_repeat_not_found"] = json!(1);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["dsss"]["rejected_frames"] = json!("not-a-counter");
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["pending_frame_limit"] = json!(WIFI_PENDING_FRAME_LIMIT - 1);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["clipped_samples"] = json!(accepted.complex_samples + 1);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["diagnostic_counts"]["invalid-fcs"] = json!(-1);
    write_json(&decode_path, &decode);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let packet_path = output.join("bluetooth/summary.json");
    let mut packet = report::read_json(&packet_path).unwrap();
    packet["unsupported_records"] = json!(1);
    write_json(&packet_path, &packet);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut packet = report::read_json(&packet_path).unwrap();
    packet
        .as_object_mut()
        .unwrap()
        .remove("duplicate_accounting");
    write_json(&packet_path, &packet);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut packet = report::read_json(&packet_path).unwrap();
    packet["duplicate_accounting"]["normalized_body_hash"] = json!("not-retained");
    write_json(&packet_path, &packet);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut packet = report::read_json(&packet_path).unwrap();
    packet["duplicate_accounting"]["repeated_validated_packets"] = json!(1);
    write_json(&packet_path, &packet);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let replay_path = output.join("bluetooth/replay.json");
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["decoder_program"]["sha256"] = json!("D".repeat(64));
    write_json(&replay_path, &replay);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["decoder_executable_identity"]["length"] = json!(124);
    write_json(&replay_path, &replay);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["decoder_program"]["name"] = json!("private-decoder-name");
    write_json(&replay_path, &replay);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["schema_version"] = json!(2);
    replay["coded_scan_requested"] = json!(false);
    replay["coded_scan_primary_frequencies_mhz"] = json!([]);
    write_json(&replay_path, &replay);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_ok());

    for (field, value) in [
        ("coded_scan_requested", json!(true)),
        ("coded_scan_requested", json!("false")),
        ("coded_scan_primary_frequencies_mhz", json!([2426])),
        ("coded_scan_primary_frequencies_mhz", json!(false)),
    ] {
        write_metric_fixture(&output, &accepted);
        let mut replay = report::read_json(&replay_path).unwrap();
        replay["schema_version"] = json!(2);
        replay["coded_scan_requested"] = json!(false);
        replay["coded_scan_primary_frequencies_mhz"] = json!([]);
        replay[field] = value;
        write_json(&replay_path, &replay);
        assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());
    }

    write_metric_fixture(&output, &accepted);
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["schema_version"] = json!(3);
    write_json(&replay_path, &replay);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let archive_path = output.join("bluetooth/bursts.bdb");
    fs::write(&archive_path, vec![1u8; 128]).unwrap();
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let family_path = output.join("burst-families/burst-families.json");
    let mut family = report::read_json(&family_path).unwrap();
    family["families"][4]["complex_sample_count"] = json!(3);
    write_json(&family_path, &family);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut family = report::read_json(&family_path).unwrap();
    family["frequency_family_counts"][0]["record_count"] = json!(1);
    write_json(&family_path, &family);
    assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());

    for frequency_mhz in [2427, 2446] {
        write_metric_fixture(&output, &accepted);
        let mut family = report::read_json(&family_path).unwrap();
        family["frequency_family_counts"][0]["frequency_mhz"] = json!(frequency_mhz);
        write_json(&family_path, &family);
        assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_ok());
    }

    for frequency_mhz in [2426, 2447] {
        write_metric_fixture(&output, &accepted);
        let mut family = report::read_json(&family_path).unwrap();
        family["frequency_family_counts"][0]["frequency_mhz"] = json!(frequency_mhz);
        write_json(&family_path, &family);
        assert!(protocol_metrics(&output, &accepted, &terminal, 1).is_err());
    }

    write_metric_fixture(&output, &accepted);
    let mut wrong_burst_outcome = metric_terminal();
    wrong_burst_outcome.stages[4].outcome = Some("no_unclassified_activity_detected".into());
    assert!(protocol_metrics(&output, &accepted, &wrong_burst_outcome, 1).is_err());

    write_metric_fixture(&output, &accepted);
    let mut wrong_outcome = metric_terminal();
    wrong_outcome.stages[0].outcome = Some("no_valid_candidates".into());
    assert!(protocol_metrics(&output, &accepted, &wrong_outcome, 1).is_err());
}
