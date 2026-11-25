use super::*;

pub(super) fn write_metric_fixture(output: &Path, accepted: &SourceReport) {
    write_json(
        &output.join("wifi/summary.json"),
        &json!({
            "schema_version":4,"operation":"inspect_wifi_packets",
            "input_linktype":127,"total_records":5,"validated_frames":5,
            "invalid_fcs_frames":0,"unknown_fcs_frames":0,"absent_fcs_frames":0,
            "malformed_frames":0,"truncated_records":0,
            "records_exceeding_snaplen":0,"upstream_bad_fcs_claims":0,
            "management_frames":3,"control_frames":1,"data_frames":1,
            "protected_frames":0,
            "block_ack":{
                "frame_count":1,
                "truncated_control_count":0,
                "complete_compressed_single_tid_count":1,
                "truncated_compressed_single_tid_count":0,
                "trailing_compressed_single_tid_count":0,
                "other_variant_count":0,
                "tids":{
                    "tid_0":0,"tid_1":0,"tid_2":0,"tid_3":0,
                    "tid_4":1,"tid_5":0,"tid_6":0,"tid_7":0,
                    "tid_8":0,"tid_9":0,"tid_10":0,"tid_11":0,
                    "tid_12":0,"tid_13":0,"tid_14":0,"tid_15":0
                },
                "set_bitmap_bits_total":5,
                "zero_bitmap_frames":0,
                "nonzero_bitmap_frames":1,
                "full_bitmap_frames":0
            },
            "eapol":{
                "frame_count":1,
                "complete_header_count":1,
                "truncated_header_count":0,
                "complete_body_count":1,
                "truncated_body_count":0,
                "packet_types":{
                    "eap_packet":0,"start":0,"logoff":0,"key":1,
                    "asf_alert":0,"other":0
                },
                "parsed_key_header_count":1,
                "pairwise_key_count":1,
                "install_key_count":0,
                "ack_key_count":1,
                "mic_key_count":0,
                "secure_key_count":0
            },
            "advertisements":{
                "eligible_frame_count":3,
                "beacon_frames":3,
                "probe_response_frames":0,
                "complete_ie_layout_frames":3,
                "truncated_ie_layout_frames":0,
                "phy":{
                    "ht_capabilities_element_frames":3,
                    "ht_operation_element_frames":3,
                    "vht_capabilities_element_frames":0,
                    "vht_operation_element_frames":0,
                    "he_capabilities_element_frames":3,
                    "he_operation_element_frames":3,
                    "eht_capabilities_element_frames":3,
                    "eht_operation_element_frames":3,
                    "unrecognized_extension_element_frames":3
                },
                "security":{
                    "privacy_capability_frames":3,
                    "rsn_present_frames":3,
                    "rsn_parsed_frames":3,
                    "rsn_malformed_frames":0,
                    "pmf_capable_frames":3,
                    "pmf_required_frames":0,
                    "wpa_present_frames":0,
                    "wpa_parsed_frames":0,
                    "wpa_malformed_frames":0
                }
            },
            "frames_by_type":{"beacon":3,"block_ack":1,"data":1}
        }),
    );
    write_json(
        &output.join("wifi/candidates.decode.json"),
        &json!({
            "schema_version":4,"operation":"offline_wifi_iq_decode",
            "decoder":"libcrafter-wifi-serial","iq_format":"sc16-q11",
            "input_samples":accepted.complex_samples,
            "processed_samples":accepted.complex_samples,
            "sample_limit":DEFAULT_ANALYSIS_SAMPLE_LIMIT,
            "sample_limit_reached":false,"input_eof_observed":true,
            "center_frequency_hz":accepted.center_frequency_hz,
            "sample_rate_hz":accepted.sample_rate_hz,
            "frames_written":5,
            "ofdm":{"valid_frames":2,"invalid_fcs":1,"rejected_frames":7,
                "truncated_frames":0,"dropped_frames":0,
                "rejections":{
                    "signal_or_probe_rejected":2,
                    "greenfield_header_unsupported":0,
                    "greenfield_probe_unrecognized":0,
                    "mixed_header_unsupported":0,
                    "legacy_fallback_unreservable":0,
                    "data_decode_failed":3,
                    "pending_capacity_rejected":0,
                    "strong_long_training_not_found":2
                },
                "data_failures":{"legacy":1,"ht_bcc":1,"ht_ldpc":1},
                "strong_long_training":{
                    "strong_short_periodicity_timeout":1,
                    "strong_long_repeat_not_found":0,
                    "strong_long_reference_not_found":1,
                    "strong_long_coordinates_unavailable":0
                }},
            "dsss":{"valid_frames":3,"invalid_fcs":0,"rejected_frames":0,
                "truncated_frames":0,"dropped_frames":0},
            "diagnostic_counts":{"invalid-fcs":1},
            "diagnostic_counts_are_lower_bounds":true,
            "diagnostic_batches_at_capacity":0,
            "clipped_samples":0,
            "pending_frame_limit":WIFI_PENDING_FRAME_LIMIT,
            "buffer_sample_limit":WIFI_BUFFER_SAMPLE_LIMIT
        }),
    );
    let rf_cells: Vec<_> = (0..160)
        .map(|index| {
            let start =
                accepted.center_frequency_hz as f64 - 8_000_000.0 + f64::from(index) * 100_000.0;
            let triggered = index == 10;
            json!({
                "start_frequency_hz":start,
                "end_frequency_hz":start + 100_000.0,
                "mean_excess_over_frequency_baseline_db":if triggered { 7.0 } else { 0.0 },
                "peak_excess_over_temporal_median_db":if triggered { 8.0 } else { 0.0 },
                "windows_above_threshold":u64::from(triggered),
                "sampled_window_hit_fraction":if triggered { 1.0 } else { 0.0 },
                "excluded_reason":if matches!(index, 79 | 80) {
                    Value::String("center_guard".into())
                } else {
                    Value::Null
                },
                "activity_triggered":triggered
            })
        })
        .collect();
    write_json(
        &output.join("rf-survey/spectrum.json"),
        &json!({
            "schema_version":1,"operation":"offline_rf_spectrum_survey",
            "input_samples":accepted.complex_samples,
            "inspected_prefix_samples":accepted.complex_samples,
            "center_frequency_hz":accepted.center_frequency_hz,
            "sample_rate_hz":accepted.sample_rate_hz,
            "fft_size":8192,"windows":1,
            "bins":vec![json!(0.0); 8192],
            "activity_detection":{
                "method":"distributed_Hann_FFT_with_per_window_weighted_DC_removal_and_100_kHz_cells",
                "cell_width_hz":100000.0,"usable_half_span_hz":8000000.0,
                "center_guard_hz":100000.0,"threshold_db":6.0,
                "temporal_baseline":"per_cell_median",
                "frequency_baseline":"lower_quartile_of_usable_cell_means",
                "region_peak_rule":"cell_center_maximizing_the_larger_of_mean_frequency_excess_and_peak_temporal_excess",
                "cells":rf_cells,"regions":[{
                "lower_frequency_hz":2430000000.0,
                "upper_frequency_hz":2430100000.0,
                "cell_count":1,
                "strongest_trigger_cell_frequency_hz":2430050000.0,
                "maximum_mean_excess_db":7.0,
                "maximum_transient_excess_db":8.0,
                "maximum_sampled_window_hit_fraction":1.0,
                "classification":"unclassified_spectral_activity"
            }]}
        }),
    );
    let channelized = output.join("ieee802154/channelized");
    fs::create_dir_all(&channelized).unwrap();
    let channels: Vec<_> = [16u8, 17, 18]
        .into_iter()
        .map(|channel| {
            let name = format!("channel-{channel}.cf32");
            let bytes = vec![0u8; 16_000];
            let sha256 = format!("{:x}", Sha256::digest(&bytes));
            fs::write(channelized.join(&name), bytes).unwrap();
            json!({
                "channel":channel,"output_file":name,"output_format":"CF32_LE",
                "output_sample_rate_hz":4000000,
                "output_complex_samples":accepted.complex_samples / 5,
                "sha256":sha256
            })
        })
        .collect();
    write_json(
        &channelized.join("channelize.json"),
        &json!({
            "schema_version":1,"operation":"offline_ieee802154_channelization",
            "source":{
                "available_complex_samples":accepted.complex_samples,
                "processed_complex_samples":accepted.complex_samples,
                "processed_full_input":true,
                "center_frequency_hz":accepted.center_frequency_hz,
                "sample_rate_hz":accepted.sample_rate_hz,
                "source_bytes":accepted.bytes,"processed_prefix_bytes":accepted.bytes,
                "processed_prefix_sha256":accepted.sha256
            },
            "filter":{"decimation":5},"channels":channels
        }),
    );
    for channel in [16u8, 17, 18] {
        let mut archive = Vec::from(*b"I154RAW\0");
        archive.extend_from_slice(&1u16.to_le_bytes());
        archive.extend_from_slice(&24u16.to_le_bytes());
        archive.push(channel);
        archive.push(0);
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive.extend_from_slice(&4_000_000u32.to_le_bytes());
        archive.extend_from_slice(&2_000_000u32.to_le_bytes());
        assert_eq!(archive.len(), 24);
        let archive_sha256 = format!("{:x}", Sha256::digest(&archive));
        for spec in orchestrate::IEEE_HYPOTHESES {
            fs::write(
                output.join("ieee802154").join(spec.archive_name(channel)),
                &archive,
            )
            .unwrap();
            write_json(
                &output
                    .join("ieee802154")
                    .join(spec.inspection_name(channel))
                    .join("summary.json"),
                &json!({
                    "schema_version":1,"operation":"inspect_ieee802154_candidates",
                    "source_format":"ieee802154_raw_candidates_v1",
                    "source_bytes":24,"source_sha256":archive_sha256.clone(),
                    "ieee802154_channel":channel,"candidate_record_count":0,
                    "channel_sample_rate_hz":4000000,"recovered_chip_rate_hz":2000000,
                    "valid_fcs_record_count":0,"invalid_fcs_record_count":0,
                    "invalid_length_record_count":0,"validated_psdu_bytes":0,
                    "first_recovered_chip_count_at_end":null,
                    "last_recovered_chip_count_at_end":null,
                    "configured_max_input_bytes":orchestrate::IEEE_ARCHIVE_MAX_BYTES,
                    "configured_max_output_bytes":512u64 * 1024 * 1024,
                    "configured_max_records":1_000_000
                }),
            );
        }
        let comparison_sources = orchestrate::IEEE_HYPOTHESES
            .into_iter()
            .map(|spec| {
                json!({
                    "source_index":spec.source_index,
                    "decoder_lane":spec.decoder_lane,
                    "carrier_offset_hz":spec.carrier_offset_hz,
                    "source_format":"ieee802154_raw_candidates_v1",
                    "source_bytes":24,
                    "source_sha256":archive_sha256.clone(),
                    "candidate_record_count":0,
                    "valid_fcs_record_count":0,
                    "invalid_fcs_record_count":0,
                    "invalid_length_record_count":0
                })
            })
            .collect::<Vec<_>>();
        write_json(
            &output.join(format!(
                "ieee802154/channel-{channel}-comparison/summary.json"
            )),
            &json!({
                "schema_version":1,
                "operation":"compare_ieee802154_candidate_hypotheses",
                "source_format":"ieee802154_raw_candidates_v1",
                "source_count":6,
                "sources":comparison_sources,
                "ieee802154_channel":channel,
                "channel_sample_rate_hz":4000000,
                "recovered_chip_rate_hz":2000000,
                "source_candidate_record_count_sum":0,
                "source_valid_fcs_record_count_sum":0,
                "source_invalid_fcs_record_count_sum":0,
                "source_invalid_length_record_count_sum":0,
                "identity_union_record_count":0,
                "identity_union_valid_fcs_record_count":0,
                "identity_union_invalid_fcs_record_count":0,
                "identity_union_invalid_length_record_count":0,
                "duplicate_record_count":0,
                "duplicate_valid_fcs_record_count":0,
                "duplicate_invalid_fcs_record_count":0,
                "duplicate_invalid_length_record_count":0,
                "membership_partitions":[],
                "configured_max_input_bytes_per_source":orchestrate::IEEE_ARCHIVE_MAX_BYTES,
                "configured_max_records_per_source":1_000_000,
                "interpretation":"exact decoder-output identity union"
            }),
        );
    }
    write_json(
        &output.join("bluetooth/replay.json"),
        &json!({
            "schema_version":1,"outcome":"completed_candidates_without_validated_ble",
            "decoder_program":{"schema_version":1,"name":"blue-dragon",
                "bytes":123,"sha256":"d".repeat(64)},
            "decoder_executable_identity":{"length":123},
            "processed_full_input":true,
            "input_complex_samples":accepted.complex_samples,
            "decoder_processed_complex_samples":accepted.complex_samples,
            "channelizer_step_complex_samples":10,
            "decoder_file_block_complex_samples":65530,
            "channelizer_steps_processed":accepted.complex_samples / 10,
            "candidate_records":1,"validated_ble_packets":0,
            "invalid_crc_packets":0,"unknown_crc_packets":1,"malformed_packets":0,
            "classic_candidate_records":0,"classic_upstream_crc_claims":0,
            "burst_archive_records":2,"burst_archive_truncated_by_limit":false,
            "radio_opened":false,"network_requested":false,"transmit_requested":false
        }),
    );
    write_json(
        &output.join("bluetooth/summary.json"),
        &json!({
            "input_linktype":192,"total_records":1,"validated_advertising_packets":0,
            "validated_data_packets":0,"invalid_crc_packets":0,
            "unknown_crc_packets":1,"malformed_packets":0,"truncated_records":0,
            "records_exceeding_snaplen":0,"classic_records":0,
            "unsupported_records":0,
            "classic_upstream_crc_claims":0,
            "duplicate_accounting":{
                "normalized_frame_body_capacity":BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                "distinct_validated_frame_bodies":0,
                "repeated_validated_packets":0,
                "unresolved_validated_packets":0,
                "capacity_reached":false,
                "exact":true
            }
        }),
    );
    let burst_bytes = vec![0u8; 128];
    let burst_sha256 = format!("{:x}", Sha256::digest(&burst_bytes));
    fs::write(output.join("bluetooth/bursts.bdb"), &burst_bytes).unwrap();
    write_json(
        &output.join("bluetooth/bursts.json"),
        &json!({
            "schema_version":1,"source_format":"blue_dragon_burst_v1",
            "source_bytes":128,"channel_sample_rate_hz":2000000,
            "archive_record_count":2,"squelch_record_count":2,
            "scan_window_record_count":0,"complex_sample_count":4
        }),
    );
    let feature = |value: f64| json!({"observations":1,"min":value,"max":value,"mean":value});
    let families: Vec<_> = orchestrate::BURST_FAMILY_LABELS
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            json!({
                "candidate_family":label,
                "group_count":u64::from(index == 4),
                "record_count":if index == 4 { 2 } else { 0 },
                "complex_sample_count":if index == 4 { 4 } else { 0 }
            })
        })
        .collect();
    write_json(
        &output.join("burst-families/burst-families.json"),
        &json!({
            "schema_version":2,"operation":"classify_bursts",
            "source":{"format":"blue_dragon_burst_v1","bytes":128,
                "sha256":burst_sha256},
            "accounting":{
                "archive_record_count":2,"squelch_record_count":2,
                "scan_window_record_count":0,"archive_complex_sample_count":4,
                "squelch_complex_sample_count":4,"scan_window_complex_sample_count":0,
                "feature_eligible_record_count":1,"feature_insufficient_record_count":1,
                "group_count":1,"grouped_squelch_record_count":2
            },
            "settings":{
                "channel_sample_rate_hz":2000000,"sample_period_ns":500,
                "group_connectivity_gap_ns":50000,"group_frequency_step_mhz":1,
                "simultaneous_interval":"half_open_unexpanded_record_interval",
                "active_power_quantile":[4,5],"active_power_divisor":16,
                "minimum_usable_phase_differences":128,"maximum_envelope_cv":0.25,
                "minimum_cluster_balance":0.15,"minimum_variance_reduction":0.6,
                "minimum_mode_contrast":4.0,"mode_contrast_std_floor_hz":1.0,
                "minimum_phase_concentration":0.05,
                "separation_hz":[40000.0,900000.0],"minimum_transitions":6,
                "kmeans_iterations":8,"adjacent_multibin_minimum_width":2,
                "wideband_multibin_minimum_width":4
            },
            "families":families,
            "frequency_family_counts":[{
                "candidate_family":"unclassified_burst_group","frequency_mhz":2437,
                "group_incidence_count":1,"record_count":2,"complex_sample_count":4
            }],
            "eligible_record_features":{
                "envelope_cv":feature(0.1),
                "usable_phase_differences":feature(128.0),
                "phase_concentration":feature(0.5),
                "two_level_separation_hz":feature(500000.0),
                "two_level_variance_reduction":feature(0.8),
                "two_level_mode_contrast":feature(5.0),
                "two_level_cluster_balance":feature(0.5),
                "two_level_transitions":feature(10.0)
            },
            "protocol_identity_established":false,"interpretation":"test"
        }),
    );
}
