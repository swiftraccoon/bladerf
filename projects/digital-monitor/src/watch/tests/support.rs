use super::*;

pub(super) fn plan(cycles: u16, keep: usize) -> Plan {
    Plan {
        cycles,
        center_frequency_schedule_hz: vec![DEFAULT_TEST_CENTER_HZ],
        duration_seconds: 0.000_5,
        samples: 10_000,
        discard_initial_samples: 0,
        gain_db: 40.0,
        max_consecutive_failures: 3,
        keep_sessions: keep,
        retained_limit_bytes: 16 * 1024 * 1024,
        pending_captures: 0,
        bluetooth_coded_shadow: false,
        capture_timeout: Duration::from_secs(21),
        log_limit_bytes: 1024,
        receiver_identity: None,
        analysis_program_identities: vec![CompactProgram {
            name: "digital-monitor",
            bytes: 123,
            sha256: "b".repeat(64),
        }],
    }
}

pub(super) fn pipeline_plan(cycles: u16, keep: usize) -> Plan {
    let mut plan = plan(cycles, keep);
    plan.pending_captures = 1;
    plan
}

pub(super) fn alternating_plan(cycles: u16, keep: usize, pending_captures: u8) -> Plan {
    let mut plan = plan(cycles, keep);
    plan.center_frequency_schedule_hz = vec![2_435_000_000, 2_437_000_000];
    plan.pending_captures = pending_captures;
    plan
}

pub(super) fn request_for_validation() -> Request {
    Request {
        output: PathBuf::from("run"),
        cycles: 2,
        center_mhz: 2435,
        alternate_center_mhz: Some(2437),
        duration_seconds: 0.000_5,
        discard_initial_samples: 0,
        gain_db: 40.0,
        max_consecutive_failures: 3,
        keep_sessions: 2,
        retained_limit_mib: 16,
        pending_captures: 0,
        bluetooth_coded_shadow: false,
        capture_timeout_seconds: 21,
        spectrum_windows: 1,
        decoder_timeout_seconds: 1,
        child_timeout_seconds: 6,
        log_limit_mib: 1,
        receiver: PathBuf::from("receiver"),
        programs: ProgramPaths {
            wifi: PathBuf::from("wifi"),
            rf: PathBuf::from("rf"),
            ieee: PathBuf::from("ieee"),
            ieee_worker: PathBuf::from("ieee-worker"),
            bluetooth: PathBuf::from("bluetooth"),
        },
    }
}

pub(super) fn session_index(session: &Path) -> usize {
    session
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

pub(super) fn wait_for_durable_capture(directory: &Path, index: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let journal_ready = fs::read_to_string(directory.join("attempts.jsonl"))
            .ok()
            .is_some_and(|journal| {
                journal.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .ok()
                        .is_some_and(|event| {
                            event["event"] == "capture_finished"
                                && event["attempt"]["index"] == index
                        })
                })
            });
        let snapshot_ready = fs::read(directory.join("run.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|run| {
                run["attempts"]
                    .as_array()
                    .and_then(|attempts| attempts.get(index - 1))
                    .is_some_and(|attempt| attempt["phase"] == "capture_finished")
            });
        if journal_ready && snapshot_ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "capture {index} did not become durable before the test deadline"
        );
        thread::yield_now();
    }
}

pub(super) fn normalized_terminal_run(mut run: Value) -> Value {
    let root = run.as_object_mut().unwrap();
    for key in [
        "schema_version",
        "pending_capture_limit",
        "attempts_started",
        "captures_finished",
        "analyses_started",
        "attempts_finished",
        "elapsed_wall_seconds",
        "interpretation",
    ] {
        root.remove(key);
    }
    for attempt in root["attempts"].as_array_mut().unwrap() {
        let attempt = attempt.as_object_mut().unwrap();
        for key in [
            "started_unix_ns",
            "ended_unix_ns",
            "start_offset_seconds",
            "end_offset_seconds",
            "capture_ended_unix_ns",
            "capture_end_offset_seconds",
            "analysis_started_unix_ns",
            "analysis_start_offset_seconds",
            "analysis_ended_unix_ns",
            "analysis_end_offset_seconds",
            "phase",
        ] {
            attempt.remove(key);
        }
    }
    run
}

pub(super) fn source(session: &Path) -> SourceReport {
    SourceReport {
        input: session.join("iq.sc16q11"),
        sidecar: session.join("iq.sc16q11.json"),
        format: "SC16_Q11_LE",
        center_frequency_hz: DEFAULT_TEST_CENTER_HZ,
        sample_rate_hz: SAMPLE_RATE_HZ,
        bandwidth_hz: SAMPLE_RATE_HZ,
        complex_samples: 10_000,
        bytes: 40_000,
        sha256: "0".repeat(64),
        sidecar_sha256: "1".repeat(64),
        continuity_guaranteed: false,
    }
}

pub(super) fn captured(session: &Path) -> CaptureStep {
    fs::write(session.join("iq.sc16q11"), [0u8; 8]).unwrap();
    CaptureStep {
        state: StepState::Complete,
        source: Some(source(session)),
        termination: None,
        diagnostics: None,
        detail: None,
    }
}

pub(super) fn source_at_center(session: &Path, center_frequency_hz: u64) -> SourceReport {
    let mut source = source(session);
    source.center_frequency_hz = center_frequency_hz;
    source
}

pub(super) fn captured_at_center(session: &Path, center_frequency_hz: u64) -> CaptureStep {
    let mut capture = captured(session);
    capture.source = Some(source_at_center(session, center_frequency_hz));
    capture
}

pub(super) fn test_burst_family_metrics() -> BurstFamilyMetrics {
    let feature = |value| BurstFeatureSummary {
        observations: 1,
        min: Some(value),
        max: Some(value),
        mean: Some(value),
    };
    BurstFamilyMetrics {
        summary_sha256: "e".repeat(64),
        source_bytes: 128,
        source_sha256: "f".repeat(64),
        protocol_identity_established: false,
        settings: BurstFamilySettings {
            channel_sample_rate_hz: 2_000_000,
            sample_period_ns: 500,
            group_connectivity_gap_ns: 50_000,
            group_frequency_step_mhz: 1,
            simultaneous_interval: "half_open_unexpanded_record_interval".into(),
            active_power_quantile: [4, 5],
            active_power_divisor: 16,
            minimum_usable_phase_differences: 128,
            maximum_envelope_cv: 0.25,
            minimum_cluster_balance: 0.15,
            minimum_variance_reduction: 0.6,
            minimum_mode_contrast: 4.0,
            mode_contrast_std_floor_hz: 1.0,
            minimum_phase_concentration: 0.05,
            separation_hz: [40_000.0, 900_000.0],
            minimum_transitions: 6,
            kmeans_iterations: 8,
            adjacent_multibin_minimum_width: 2,
            wideband_multibin_minimum_width: 4,
        },
        accounting: BurstFamilyAccounting {
            archive_record_count: 2,
            squelch_record_count: 2,
            scan_window_record_count: 0,
            archive_complex_sample_count: 4,
            squelch_complex_sample_count: 4,
            scan_window_complex_sample_count: 0,
            feature_eligible_record_count: 1,
            feature_insufficient_record_count: 1,
            group_count: 1,
            grouped_squelch_record_count: 2,
        },
        family_order: orchestrate::BURST_FAMILY_LABELS
            .into_iter()
            .map(str::to_owned)
            .collect(),
        families: orchestrate::BURST_FAMILY_LABELS
            .into_iter()
            .enumerate()
            .map(|(index, _)| BurstFamilyCount {
                group_count: u64::from(index == 4),
                record_count: if index == 4 { 2 } else { 0 },
                complex_sample_count: if index == 4 { 4 } else { 0 },
            })
            .collect(),
        frequency_bins: vec![BurstFrequencyBin {
            frequency_mhz: 2437,
            group_incidence_count: [0, 0, 0, 0, 1],
            record_count: [0, 0, 0, 0, 2],
            complex_sample_count: [0, 0, 0, 0, 4],
        }],
        eligible_record_features: BurstFeatureSummaries {
            envelope_cv: feature(0.1),
            usable_phase_differences: feature(128.0),
            phase_concentration: feature(0.5),
            two_level_separation_hz: feature(500_000.0),
            two_level_variance_reduction: feature(0.8),
            two_level_mode_contrast: feature(5.0),
            two_level_cluster_balance: feature(0.5),
            two_level_transitions: feature(10.0),
        },
    }
}

pub(super) fn metrics() -> ProtocolMetrics {
    ProtocolMetrics {
        metrics_schema_version: 14,
        wifi: WifiMetrics {
            summary_sha256: "3".repeat(64),
            decoder_summary_sha256: "4".repeat(64),
            decoder_health: WifiDecoderHealth {
                ofdm: WifiOfdmDecoderHealth {
                    valid_frames: 2,
                    invalid_fcs: 0,
                    rejected_frames: 2,
                    truncated_frames: 0,
                    dropped_frames: 0,
                    rejections: orchestrate::WifiOfdmRejectionMetrics {
                        signal_or_probe_rejected: 2,
                        data_decode_failed: 0,
                        ..Default::default()
                    },
                    data_failures: orchestrate::WifiOfdmDataFailureMetrics::default(),
                    strong_long_training: orchestrate::WifiOfdmStrongLongTrainingMetrics::default(),
                },
                dsss: WifiPhyDecoderHealth {
                    valid_frames: 3,
                    invalid_fcs: 0,
                    rejected_frames: 0,
                    truncated_frames: 0,
                    dropped_frames: 0,
                },
                diagnostic_counts_are_lower_bounds: true,
                diagnostic_batches_at_capacity: 0,
                clipped_samples: 0,
                pending_frame_limit: WIFI_PENDING_FRAME_LIMIT,
                buffer_sample_limit: WIFI_BUFFER_SAMPLE_LIMIT,
            },
            total_records: 5,
            validated_frames: 5,
            invalid_fcs_frames: 0,
            unknown_fcs_frames: 0,
            absent_fcs_frames: 0,
            malformed_frames: 0,
            truncated_records: 0,
            records_exceeding_snaplen: 0,
            upstream_bad_fcs_claims: 0,
            management_frames: 3,
            control_frames: 1,
            data_frames: 1,
            protected_frames: 0,
            frame_types: orchestrate::WifiFrameTypeMetrics {
                management: orchestrate::WifiManagementFrameTypes {
                    beacon: 3,
                    ..Default::default()
                },
                control: orchestrate::WifiControlFrameTypes {
                    block_ack: 1,
                    ..Default::default()
                },
                data: orchestrate::WifiDataFrameTypes {
                    data: 1,
                    ..Default::default()
                },
            },
            block_ack: orchestrate::WifiBlockAckMetrics {
                frame_count: 1,
                complete_compressed_single_tid_count: 1,
                tids: orchestrate::WifiBlockAckTidCounts {
                    tid_4: 1,
                    ..Default::default()
                },
                set_bitmap_bits_total: 5,
                nonzero_bitmap_frames: 1,
                ..Default::default()
            },
            eapol: orchestrate::WifiEapolMetrics {
                frame_count: 1,
                complete_header_count: 1,
                truncated_header_count: 0,
                complete_body_count: 1,
                truncated_body_count: 0,
                packet_types: orchestrate::WifiEapolPacketTypes {
                    eap_packet: 0,
                    start: 0,
                    logoff: 0,
                    key: 1,
                    asf_alert: 0,
                    other: 0,
                },
                parsed_key_header_count: 1,
                pairwise_key_count: 1,
                install_key_count: 0,
                ack_key_count: 1,
                mic_key_count: 0,
                secure_key_count: 0,
            },
            advertisements: orchestrate::WifiAdvertisementMetrics {
                eligible_frame_count: 3,
                beacon_frames: 3,
                probe_response_frames: 0,
                complete_ie_layout_frames: 3,
                truncated_ie_layout_frames: 0,
                phy: orchestrate::WifiAdvertisementPhyMetrics {
                    ht_capabilities_element_frames: 3,
                    ht_operation_element_frames: 3,
                    vht_capabilities_element_frames: 0,
                    vht_operation_element_frames: 0,
                    he_capabilities_element_frames: 3,
                    he_operation_element_frames: 3,
                    eht_capabilities_element_frames: 3,
                    eht_operation_element_frames: 3,
                    unrecognized_extension_element_frames: 3,
                },
                security: orchestrate::WifiAdvertisementSecurityMetrics {
                    privacy_capability_frames: 3,
                    rsn_present_frames: 3,
                    rsn_parsed_frames: 3,
                    rsn_malformed_frames: 0,
                    pmf_capable_frames: 3,
                    pmf_required_frames: 0,
                    wpa_present_frames: 0,
                    wpa_parsed_frames: 0,
                    wpa_malformed_frames: 0,
                },
            },
        },
        rf_survey: RfMetrics {
            summary_sha256: "5".repeat(64),
            input_samples: 10_000,
            inspected_prefix_samples: 10_000,
            windows: 1,
            total_cells: 160,
            excluded_cells: 2,
            triggered_cells: 1,
            region_count: 1,
            region_aggregate: Some(RfRegionAggregate {
                lowest_frequency_hz: 2_430_000_000.0,
                highest_frequency_hz: 2_430_100_000.0,
                maximum_mean_excess_db: 7.0,
                maximum_transient_excess_db: 8.0,
                maximum_sampled_window_hit_fraction: 1.0,
            }),
        },
        ieee802154: IeeeMetrics {
            channelize_summary_sha256: "6".repeat(64),
            channels: [16u8, 17, 18]
                .into_iter()
                .map(|channel| IeeeChannelMetrics {
                    channel,
                    channelized_input_bytes: 16_000,
                    channelized_input_sha256: "7".repeat(64),
                    sources: orchestrate::IEEE_HYPOTHESES
                        .into_iter()
                        .map(|spec| IeeeHypothesisMetrics {
                            source_index: spec.source_index,
                            decoder_lane: spec.decoder_lane,
                            carrier_offset_hz: spec.carrier_offset_hz,
                            source_role: spec.source_role,
                            candidate_archive_bytes: 24,
                            candidate_archive_sha256: "8".repeat(64),
                            inspection_summary_sha256: "9".repeat(64),
                            candidate_records: 0,
                            valid_fcs_records: 0,
                            invalid_fcs_records: 0,
                            invalid_length_records: 0,
                            validated_psdu_bytes: 0,
                        })
                        .collect(),
                    comparison_summary_sha256: "a".repeat(64),
                    source_candidate_records_sum: 0,
                    source_valid_fcs_records_sum: 0,
                    source_invalid_fcs_records_sum: 0,
                    source_invalid_length_records_sum: 0,
                    identity_union_records: 0,
                    identity_union_valid_fcs_records: 0,
                    identity_union_invalid_fcs_records: 0,
                    identity_union_invalid_length_records: 0,
                    duplicate_records: 0,
                    duplicate_valid_fcs_records: 0,
                    duplicate_invalid_fcs_records: 0,
                    duplicate_invalid_length_records: 0,
                    membership_partitions: Vec::new(),
                })
                .collect(),
        },
        bluetooth: BluetoothMetrics {
            replay_sha256: "8".repeat(64),
            packet_summary_sha256: "9".repeat(64),
            burst_summary_sha256: "a".repeat(64),
            decoder_program: NestedDecoderProgram {
                schema_version: 1,
                name: "blue-dragon",
                bytes: 123,
                sha256: "d".repeat(64),
            },
            input_complex_samples: 10_000,
            decoder_processed_complex_samples: 10_000,
            channelizer_step_complex_samples: 10,
            decoder_file_block_complex_samples: 65_530,
            channelizer_steps_processed: 1_000,
            candidate_records: 1,
            validated_advertising_packets: 0,
            validated_data_packets: 0,
            validated_ble_packets: 0,
            duplicate_accounting: BluetoothDuplicateAccounting {
                normalized_frame_body_capacity: BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                distinct_validated_frame_bodies: 0,
                repeated_validated_packets: 0,
                unresolved_validated_packets: 0,
                capacity_reached: false,
                exact: true,
            },
            invalid_crc_packets: 0,
            unknown_crc_packets: 1,
            malformed_packets: 0,
            truncated_records: 0,
            unsupported_records: 0,
            records_exceeding_snaplen: 0,
            classic_candidate_records: 0,
            classic_upstream_crc_claims: 0,
            burst_archive_records: 2,
            burst_archive_source_bytes: 128,
            burst_archive_complex_samples: 4,
            burst_archive_squelch_records: 2,
            burst_archive_scan_window_records: 0,
            burst_archive_truncated_by_limit: false,
            radio_opened: false,
            network_requested: false,
            transmit_requested: false,
        },
        burst_families: test_burst_family_metrics(),
    }
}

pub(super) fn metric_terminal() -> CompactAnalysis {
    CompactAnalysis {
        overall_state: "complete".into(),
        manifest_sha256: "2".repeat(64),
        stages: vec![
            CompactStage {
                name: "wifi".into(),
                state: "complete".into(),
                outcome: Some("evidence_found".into()),
            },
            CompactStage {
                name: "rf_survey".into(),
                state: "complete".into(),
                outcome: Some("unclassified_activity".into()),
            },
            CompactStage {
                name: "ieee802154".into(),
                state: "complete".into(),
                outcome: Some("no_valid_candidates".into()),
            },
            CompactStage {
                name: "bluetooth".into(),
                state: "complete".into(),
                outcome: Some("candidates_without_valid_frames".into()),
            },
            CompactStage {
                name: "burst_families".into(),
                state: "complete".into(),
                outcome: Some("unclassified_activity".into()),
            },
            CompactStage {
                name: "terminal_integrity".into(),
                state: "complete".into(),
                outcome: None,
            },
        ],
        metrics: None,
        coded_shadow: None,
    }
}

pub(super) fn coded_shadow_metrics(
    validated_coded_s8_packets: u64,
    validated_coded_s2_packets: u64,
) -> BluetoothCodedShadowMetrics {
    let validated_ble_packets = validated_coded_s8_packets + validated_coded_s2_packets;
    BluetoothCodedShadowMetrics {
        request_sha256: "2".repeat(64),
        replay_sha256: "3".repeat(64),
        packet_summary_sha256: "4".repeat(64),
        burst_summary_sha256: "5".repeat(64),
        candidate_pcap_sha256: "6".repeat(64),
        validated_pcap_sha256: "7".repeat(64),
        burst_archive_sha256: "9".repeat(64),
        decoder_program: NestedDecoderProgram {
            schema_version: 1,
            name: "blue-dragon",
            bytes: 123,
            sha256: "8".repeat(64),
        },
        input_complex_samples: 327_680,
        decoder_processed_complex_samples: 327_680,
        channelizer_steps_processed: 32_768,
        processed_full_input: true,
        candidate_records: validated_ble_packets,
        validated_ble_packets,
        validated_le_1m_packets: 0,
        validated_le_2m_packets: 0,
        validated_coded_s8_packets,
        validated_coded_s2_packets,
        invalid_crc_packets: 0,
        unknown_crc_packets: 0,
        malformed_packets: 0,
        unsupported_records: 0,
        truncated_records: 0,
        records_exceeding_snaplen: 0,
        classic_candidate_records: 0,
        classic_upstream_crc_claims: 0,
        scan_window_records: 1,
        burst_archive_truncated_by_limit: false,
        radio_opened: false,
        network_requested: false,
        transmit_requested: false,
    }
}

pub(super) fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

pub(super) fn empty_wifi_block_ack() -> Value {
    json!({
        "frame_count":0,
        "truncated_control_count":0,
        "complete_compressed_single_tid_count":0,
        "truncated_compressed_single_tid_count":0,
        "trailing_compressed_single_tid_count":0,
        "other_variant_count":0,
        "tids":{
            "tid_0":0,"tid_1":0,"tid_2":0,"tid_3":0,
            "tid_4":0,"tid_5":0,"tid_6":0,"tid_7":0,
            "tid_8":0,"tid_9":0,"tid_10":0,"tid_11":0,
            "tid_12":0,"tid_13":0,"tid_14":0,"tid_15":0
        },
        "set_bitmap_bits_total":0,
        "zero_bitmap_frames":0,
        "nonzero_bitmap_frames":0,
        "full_bitmap_frames":0
    })
}

pub(super) fn empty_wifi_advertisements() -> Value {
    json!({
        "eligible_frame_count":0,
        "beacon_frames":0,
        "probe_response_frames":0,
        "complete_ie_layout_frames":0,
        "truncated_ie_layout_frames":0,
        "phy":{
            "ht_capabilities_element_frames":0,
            "ht_operation_element_frames":0,
            "vht_capabilities_element_frames":0,
            "vht_operation_element_frames":0,
            "he_capabilities_element_frames":0,
            "he_operation_element_frames":0,
            "eht_capabilities_element_frames":0,
            "eht_operation_element_frames":0,
            "unrecognized_extension_element_frames":0
        },
        "security":{
            "privacy_capability_frames":0,
            "rsn_present_frames":0,
            "rsn_parsed_frames":0,
            "rsn_malformed_frames":0,
            "pmf_capable_frames":0,
            "pmf_required_frames":0,
            "wpa_present_frames":0,
            "wpa_parsed_frames":0,
            "wpa_malformed_frames":0
        }
    })
}
