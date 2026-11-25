use std::path::PathBuf;
use std::{fs, path::Path};

use serde_json::json;
use sha2::{Digest, Sha256};

use super::*;

fn source(complex_samples: u64) -> SourceReport {
    SourceReport {
        input: PathBuf::from("/capture/iq.sc16q11"),
        sidecar: PathBuf::from("/capture/iq.sc16q11.json"),
        format: NORMALIZED_SC16_Q11_FORMAT,
        center_frequency_hz: 2_435_000_000,
        sample_rate_hz: 20_000_000,
        bandwidth_hz: 20_000_000,
        complex_samples,
        bytes: complex_samples * 4,
        sha256: "0".repeat(64),
        sidecar_sha256: "1".repeat(64),
        continuity_guaranteed: false,
    }
}

#[test]
fn coded_phy_partition_is_closed_and_named_exactly() {
    let summary = json!({
        "validated_packets_by_phy": {
            "unknown": 0,
            "le_1m": 3,
            "le_2m": 2,
            "le_coded_s8": 1,
            "le_coded_s2": 4
        }
    });
    assert_eq!(validated_phy_counts(&summary, 10).unwrap(), [3, 2, 1, 4]);

    let mut unknown = summary.clone();
    unknown["validated_packets_by_phy"]["unknown"] = json!(1);
    assert!(validated_phy_counts(&unknown, 11).is_err());

    let mut mismatch = summary.clone();
    mismatch["validated_packets_by_phy"]["le_coded_s2"] = json!(5);
    assert!(validated_phy_counts(&mismatch, 10).is_err());

    let mut added = summary;
    added["validated_packets_by_phy"]["future_phy"] = json!(0);
    assert!(validated_phy_counts(&added, 10).is_err());
}

#[test]
fn scan_window_count_uses_channelized_step_accounting() {
    assert_eq!(expected_scan_window_records(&source(327_680)).unwrap(), 1);
    assert_eq!(
        expected_scan_window_records(&source(20_000_000)).unwrap(),
        61
    );
    assert!(expected_scan_window_records(&source(327_681)).is_err());
}

#[test]
fn coded_scan_frequency_set_is_exact() {
    validate_scan_request(&json!({
        "coded_scan_requested": true,
        "coded_scan_primary_frequencies_mhz": [2426]
    }))
    .unwrap();
    for invalid in [
        json!({"coded_scan_requested": false, "coded_scan_primary_frequencies_mhz": [2426]}),
        json!({"coded_scan_requested": true, "coded_scan_primary_frequencies_mhz": []}),
        json!({"coded_scan_requested": true, "coded_scan_primary_frequencies_mhz": [2402, 2426]}),
    ] {
        assert!(validate_scan_request(&invalid).is_err());
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_json(path: &Path, value: &Value) {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();
}

fn assert_json_mutation_rejected(
    path: &Path,
    mutate: impl FnOnce(&mut Value),
    root: &Path,
    source: &SourceReport,
    decoder: &NestedDecoderProgram,
    expected: &str,
) {
    let original = fs::read(path).unwrap();
    let mut value: Value = serde_json::from_slice(&original).unwrap();
    mutate(&mut value);
    write_json(path, &value);
    let error = bluetooth_coded_shadow_metrics(root, source, decoder).unwrap_err();
    assert!(
        format!("{error:#}").contains(expected),
        "unexpected rejection: {error:#}"
    );
    fs::write(path, original).unwrap();
}

#[test]
fn complete_shadow_fixture_binds_input_decoder_and_scan_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("shadow");
    fs::create_dir(&root).unwrap();
    let input = directory.path().join("iq.sc16q11");
    let sidecar = directory.path().join("iq.sc16q11.json");
    let input_bytes = vec![0; 327_680 * 4];
    let sidecar_bytes = b"{}\n";
    fs::write(&input, &input_bytes).unwrap();
    fs::write(&sidecar, sidecar_bytes).unwrap();
    let source = SourceReport {
        input,
        sidecar,
        format: NORMALIZED_SC16_Q11_FORMAT,
        center_frequency_hz: 2_435_000_000,
        sample_rate_hz: 20_000_000,
        bandwidth_hz: 20_000_000,
        complex_samples: 327_680,
        bytes: input_bytes.len() as u64,
        sha256: sha256(&input_bytes),
        sidecar_sha256: sha256(sidecar_bytes),
        continuity_guaranteed: false,
    };
    let decoder = NestedDecoderProgram {
        schema_version: 1,
        name: "blue-dragon",
        bytes: 123,
        sha256: "a".repeat(64),
    };
    let decoder_json = json!({
        "schema_version": 1,
        "name": "blue-dragon",
        "bytes": decoder.bytes,
        "sha256": decoder.sha256,
    });
    let decoder_identity = json!({"length": decoder.bytes});
    write_json(
        &root.join("request.json"),
        &json!({
            "schema_version": 1,
            "operation": "offline_sc16q11_replay",
            "engine": "blue-dragon",
            "input": source.input,
            "sidecar": source.sidecar,
            "input_identity": {"length": source.bytes},
            "acquisition": {
                "format": "SC16_Q11",
                "dtype": "<i2",
                "interleave": "I,Q",
                "complex_scale": 2048,
                "frequency_hz": 2_435_000_000.0,
                "sample_rate_hz": 20_000_000.0,
                "bandwidth_hz": 20_000_000.0,
                "num_samples": source.complex_samples,
            },
            "max_samples": source.complex_samples,
            "full_input_required": true,
            "channelizer_step_complex_samples": 10,
            "decoder_file_block_complex_samples": 65_530,
            "burst_limit_mib": 128,
            "decoder_output_soft_limit_mib": 512,
            "coded_scan_requested": true,
            "coded_scan_primary_frequencies_mhz": [2426],
            "decoder_program": decoder_json,
            "decoder_executable_identity": decoder_identity,
            "radio_opened": false,
            "network_requested": false,
            "transmit_requested": false,
        }),
    );
    write_json(
        &root.join("replay.json"),
        &json!({
            "schema_version": 2,
            "outcome": "completed_zero_candidates",
            "input_complex_samples": source.complex_samples,
            "decoder_processed_complex_samples": source.complex_samples,
            "processed_full_input": true,
            "channelizer_step_complex_samples": 10,
            "channelizer_steps_processed": 32_768,
            "decoder_file_block_complex_samples": 65_530,
            "decoder_exit_code": 0,
            "candidate_records": 0,
            "validated_ble_packets": 0,
            "invalid_crc_packets": 0,
            "unknown_crc_packets": 0,
            "malformed_packets": 0,
            "classic_candidate_records": 0,
            "classic_upstream_crc_claims": 0,
            "classic_candidates_are_not_independently_validated": true,
            "burst_archive_records": 1,
            "burst_archive_truncated_by_limit": false,
            "coded_scan_requested": true,
            "coded_scan_primary_frequencies_mhz": [2426],
            "decoder_program": decoder_json,
            "decoder_executable_identity": decoder_identity,
            "zero_candidates_does_not_establish_absence": true,
            "radio_opened": false,
            "network_requested": false,
            "transmit_requested": false,
        }),
    );
    write_json(
        &root.join("summary.json"),
        &json!({
            "input_linktype": 192,
            "total_records": 0,
            "validated_advertising_packets": 0,
            "validated_data_packets": 0,
            "invalid_crc_packets": 0,
            "unknown_crc_packets": 0,
            "malformed_packets": 0,
            "truncated_records": 0,
            "unsupported_records": 0,
            "records_exceeding_snaplen": 0,
            "classic_records": 0,
            "classic_upstream_crc_claims": 0,
            "validated_packets_by_phy": {
                "unknown": 0,
                "le_1m": 0,
                "le_2m": 0,
                "le_coded_s8": 0,
                "le_coded_s2": 0,
            },
            "duplicate_accounting": {
                "normalized_frame_body_capacity": 4096,
                "distinct_validated_frame_bodies": 0,
                "repeated_validated_packets": 0,
                "unresolved_validated_packets": 0,
                "capacity_reached": false,
                "exact": true,
            },
        }),
    );
    let burst_complex_samples = 32_768u64;
    let burst_bytes = 16 + 48 + burst_complex_samples * 4;
    write_json(
        &root.join("bursts.json"),
        &json!({
            "schema_version": 1,
            "source_format": "blue_dragon_burst_v1",
            "channel_sample_rate_hz": 2_000_000,
            "archive_record_count": 1,
            "squelch_record_count": 0,
            "scan_window_record_count": 1,
            "complex_sample_count": burst_complex_samples,
            "source_bytes": burst_bytes,
        }),
    );
    fs::write(root.join("bursts.bdb"), vec![0; burst_bytes as usize]).unwrap();
    fs::write(root.join("candidates.pcap"), vec![0; 24]).unwrap();
    fs::write(root.join("validated.pcap"), vec![0; 24]).unwrap();

    let metrics = bluetooth_coded_shadow_metrics(&root, &source, &decoder).unwrap();
    assert_eq!(metrics.scan_window_records, 1);
    assert_eq!(metrics.candidate_records, 0);
    assert!(metrics.processed_full_input);
    assert_eq!(metrics.decoder_program, decoder);

    let mut unnormalized_source = source.clone();
    unnormalized_source.format = "SC16_Q11";
    let error = bluetooth_coded_shadow_metrics(&root, &unnormalized_source, &decoder).unwrap_err();
    assert!(format!("{error:#}").contains("require normalized SC16_Q11"));

    let mut changed_input = input_bytes.clone();
    changed_input[0] = 1;
    fs::write(&source.input, &changed_input).unwrap();
    let error = bluetooth_coded_shadow_metrics(&root, &source, &decoder).unwrap_err();
    assert!(format!("{error:#}").contains("source or sidecar changed after ordinary analysis"));
    fs::write(&source.input, &input_bytes).unwrap();

    assert_json_mutation_rejected(
        &root.join("request.json"),
        |value| value["decoder_program"]["sha256"] = json!("b".repeat(64)),
        &root,
        &source,
        &decoder,
        "nested decoder differs from ordinary replay",
    );
    assert_json_mutation_rejected(
        &root.join("replay.json"),
        |value| value["radio_opened"] = json!(true),
        &root,
        &source,
        &decoder,
        "requested unsafe operation radio_opened",
    );
    assert_json_mutation_rejected(
        &root.join("replay.json"),
        |value| value["burst_archive_truncated_by_limit"] = json!(true),
        &root,
        &source,
        &decoder,
        "burst archive reached its configured limit",
    );
    assert_json_mutation_rejected(
        &root.join("bursts.json"),
        |value| value["scan_window_record_count"] = json!(0),
        &root,
        &source,
        &decoder,
        "burst or scan-window accounting differs",
    );
    assert_json_mutation_rejected(
        &root.join("bursts.json"),
        |value| value["source_bytes"] = json!(burst_bytes + 4),
        &root,
        &source,
        &decoder,
        "burst archive extent differs",
    );
}
