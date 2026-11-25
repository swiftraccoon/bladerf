use super::*;

#[test]
fn wifi_metrics_accept_full_duration_sample_limit_completion() {
    for samples in [20_000_000, 40_000_000] {
        let directory = tempfile::tempdir().unwrap();
        let mut accepted = source(directory.path());
        accepted.complex_samples = samples;
        accepted.bytes = samples * 4;
        let output = directory.path().join("analysis");
        write_metric_fixture(&output, &accepted);
        let decode_path = output.join("wifi/candidates.decode.json");
        let mut decode = report::read_json(&decode_path).unwrap();
        decode["sample_limit"] = json!(samples);
        decode["sample_limit_reached"] = json!(true);
        decode["input_eof_observed"] = json!(false);
        write_json(&decode_path, &decode);

        let metrics = wifi_metrics(&output, &accepted, &metric_terminal()).unwrap();
        assert_eq!(metrics.validated_frames, 5);
        assert_eq!(metrics.decoder_health.ofdm.valid_frames, 2);
        assert_eq!(metrics.decoder_health.dsss.valid_frames, 3);

        decode["sample_limit"] = json!(samples + 10);
        write_json(&decode_path, &decode);
        assert!(wifi_metrics(&output, &accepted, &metric_terminal()).is_err());

        decode["sample_limit"] = json!(samples);
        decode["input_eof_observed"] = json!(true);
        write_json(&decode_path, &decode);
        assert!(wifi_metrics(&output, &accepted, &metric_terminal()).is_err());
    }
}

#[test]
fn bluetooth_metrics_accept_truncated_and_unsupported_records() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let output = directory.path().join("analysis");
    write_metric_fixture(&output, &accepted);

    let replay_path = output.join("bluetooth/replay.json");
    let mut replay = report::read_json(&replay_path).unwrap();
    replay["candidate_records"] = json!(3);
    write_json(&replay_path, &replay);

    let packet_path = output.join("bluetooth/summary.json");
    let mut packet = report::read_json(&packet_path).unwrap();
    packet["total_records"] = json!(3);
    packet["truncated_records"] = json!(1);
    packet["unsupported_records"] = json!(1);
    write_json(&packet_path, &packet);

    let metrics = bluetooth_metrics(&output, &accepted, &metric_terminal()).unwrap();
    assert_eq!(metrics.candidate_records, 3);
    assert_eq!(metrics.unknown_crc_packets, 1);
    assert_eq!(metrics.truncated_records, 1);
    assert_eq!(metrics.unsupported_records, 1);
}

#[test]
fn burst_family_metrics_accept_mixed_squelch_and_scan_records() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let output = directory.path().join("analysis");
    write_metric_fixture(&output, &accepted);

    let bursts_path = output.join("bluetooth/bursts.json");
    let mut bursts = report::read_json(&bursts_path).unwrap();
    bursts["squelch_record_count"] = json!(1);
    bursts["scan_window_record_count"] = json!(1);
    write_json(&bursts_path, &bursts);

    let family_path = output.join("burst-families/burst-families.json");
    let mut family = report::read_json(&family_path).unwrap();
    family["accounting"]["squelch_record_count"] = json!(1);
    family["accounting"]["scan_window_record_count"] = json!(1);
    family["accounting"]["squelch_complex_sample_count"] = json!(2);
    family["accounting"]["scan_window_complex_sample_count"] = json!(2);
    family["accounting"]["feature_insufficient_record_count"] = json!(0);
    family["accounting"]["grouped_squelch_record_count"] = json!(1);
    family["families"][4]["record_count"] = json!(1);
    family["families"][4]["complex_sample_count"] = json!(2);
    family["frequency_family_counts"][0]["record_count"] = json!(1);
    family["frequency_family_counts"][0]["complex_sample_count"] = json!(2);
    write_json(&family_path, &family);

    let metrics = protocol_metrics(&output, &accepted, &metric_terminal(), 1).unwrap();
    assert_eq!(metrics.bluetooth.burst_archive_squelch_records, 1);
    assert_eq!(metrics.bluetooth.burst_archive_scan_window_records, 1);
    assert_eq!(metrics.burst_families.accounting.squelch_record_count, 1);
    assert_eq!(
        metrics.burst_families.accounting.scan_window_record_count,
        1
    );
    assert_eq!(metrics.burst_families.families[4].record_count, 1);
    assert_eq!(metrics.burst_families.families[4].complex_sample_count, 2);
}

#[test]
fn wifi_decoder_health_promotes_bad_fcs_only_to_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let output = directory.path().join("analysis");
    write_metric_fixture(&output, &accepted);
    let summary_path = output.join("wifi/summary.json");
    let mut summary = report::read_json(&summary_path).unwrap();
    for field in [
        "total_records",
        "validated_frames",
        "invalid_fcs_frames",
        "unknown_fcs_frames",
        "absent_fcs_frames",
        "malformed_frames",
        "truncated_records",
        "records_exceeding_snaplen",
        "upstream_bad_fcs_claims",
        "management_frames",
        "control_frames",
        "data_frames",
        "protected_frames",
    ] {
        summary[field] = json!(0);
    }
    summary["frames_by_type"] = json!({});
    summary["eapol"] = json!({
        "frame_count":0,
        "complete_header_count":0,
        "truncated_header_count":0,
        "complete_body_count":0,
        "truncated_body_count":0,
        "packet_types":{
            "eap_packet":0,"start":0,"logoff":0,"key":0,
            "asf_alert":0,"other":0
        },
        "parsed_key_header_count":0,
        "pairwise_key_count":0,
        "install_key_count":0,
        "ack_key_count":0,
        "mic_key_count":0,
        "secure_key_count":0
    });
    summary["block_ack"] = empty_wifi_block_ack();
    summary["advertisements"] = empty_wifi_advertisements();
    write_json(&summary_path, &summary);

    let decode_path = output.join("wifi/candidates.decode.json");
    let mut decode = report::read_json(&decode_path).unwrap();
    decode["frames_written"] = json!(0);
    decode["ofdm"] = json!({
        "valid_frames":0,"invalid_fcs":4,"rejected_frames":163,
        "truncated_frames":0,"dropped_frames":0,
        "rejections":{
            "signal_or_probe_rejected":100,
            "greenfield_header_unsupported":0,
            "greenfield_probe_unrecognized":0,
            "mixed_header_unsupported":0,
            "legacy_fallback_unreservable":0,
            "data_decode_failed":63,
            "pending_capacity_rejected":0,
            "strong_long_training_not_found":0
        },
        "data_failures":{"legacy":21,"ht_bcc":21,"ht_ldpc":21},
        "strong_long_training":{
            "strong_short_periodicity_timeout":0,
            "strong_long_repeat_not_found":0,
            "strong_long_reference_not_found":0,
            "strong_long_coordinates_unavailable":0
        }
    });
    decode["dsss"] = json!({
        "valid_frames":0,"invalid_fcs":1,"rejected_frames":0,
        "truncated_frames":0,"dropped_frames":0
    });
    decode["diagnostic_counts"] = json!({"invalid-fcs":5,"invalid-header":2,"reset":2});
    write_json(&decode_path, &decode);

    let mut terminal = metric_terminal();
    terminal.stages[0].outcome = Some("candidates_without_valid_frames".into());
    let health = wifi_metrics(&output, &accepted, &terminal).unwrap();
    assert_eq!(health.validated_frames, 0);
    assert_eq!(health.decoder_health.ofdm.valid_frames, 0);
    assert_eq!(health.decoder_health.ofdm.invalid_fcs, 4);
    assert_eq!(health.decoder_health.ofdm.rejected_frames, 163);
    assert_eq!(health.decoder_health.dsss.invalid_fcs, 1);
}

#[test]
fn wifi_decoder_health_allows_fcs_valid_malformed_mac_records() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let output = directory.path().join("analysis");
    write_metric_fixture(&output, &accepted);
    let summary_path = output.join("wifi/summary.json");
    let mut summary = report::read_json(&summary_path).unwrap();
    summary["validated_frames"] = json!(4);
    summary["malformed_frames"] = json!(1);
    summary["management_frames"] = json!(2);
    summary["frames_by_type"] = json!({"beacon":2,"block_ack":1,"data":1});
    summary["advertisements"] = empty_wifi_advertisements();
    write_json(&summary_path, &summary);

    let health = wifi_metrics(&output, &accepted, &metric_terminal()).unwrap();
    assert_eq!(health.total_records, 5);
    assert_eq!(health.validated_frames, 4);
    assert_eq!(health.malformed_frames, 1);
    assert_eq!(health.decoder_health.ofdm.valid_frames, 2);
    assert_eq!(health.decoder_health.dsss.valid_frames, 3);
}
