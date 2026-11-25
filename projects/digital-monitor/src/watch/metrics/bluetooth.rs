use super::{
    BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY, BluetoothDuplicateAccounting, BluetoothMetrics,
    CompactAnalysis, Context, MAX_NESTED_DECODER_BYTES, NestedDecoderProgram, Path, Result,
    SourceReport, Value, ensure, orchestrate, report, require_outcome, required_bool,
    required_sha256, stable_regular_file_len, terminal_outcome,
};

pub(in crate::watch) fn bluetooth_duplicate_accounting(
    packet: &Value,
    validated_packets: u64,
) -> Result<BluetoothDuplicateAccounting> {
    let value = packet
        .get("duplicate_accounting")
        .context("Bluetooth packet summary lacks duplicate accounting")?;
    let object = value
        .as_object()
        .context("Bluetooth duplicate accounting is not an object")?;
    let fields = [
        "normalized_frame_body_capacity",
        "distinct_validated_frame_bodies",
        "repeated_validated_packets",
        "unresolved_validated_packets",
        "capacity_reached",
        "exact",
    ];
    ensure!(
        object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field)),
        "Bluetooth duplicate-accounting schema differs"
    );
    let accounting = BluetoothDuplicateAccounting {
        normalized_frame_body_capacity: report::required_u64(
            value,
            "normalized_frame_body_capacity",
        )?,
        distinct_validated_frame_bodies: report::required_u64(
            value,
            "distinct_validated_frame_bodies",
        )?,
        repeated_validated_packets: report::required_u64(value, "repeated_validated_packets")?,
        unresolved_validated_packets: report::required_u64(value, "unresolved_validated_packets")?,
        capacity_reached: required_bool(value, "capacity_reached")?,
        exact: required_bool(value, "exact")?,
    };
    let accounted_packets = accounting
        .distinct_validated_frame_bodies
        .checked_add(accounting.repeated_validated_packets)
        .and_then(|count| count.checked_add(accounting.unresolved_validated_packets))
        .context("Bluetooth duplicate-accounting packet total overflow")?;
    ensure!(
        accounting.normalized_frame_body_capacity == BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY
            && accounting.distinct_validated_frame_bodies
                <= accounting.normalized_frame_body_capacity
            && accounting.capacity_reached
                == (accounting.distinct_validated_frame_bodies
                    == accounting.normalized_frame_body_capacity)
            && accounting.exact == (accounting.unresolved_validated_packets == 0)
            && (accounting.repeated_validated_packets == 0
                || accounting.distinct_validated_frame_bodies > 0)
            && (accounting.unresolved_validated_packets == 0 || accounting.capacity_reached)
            && accounted_packets == validated_packets,
        "Bluetooth duplicate accounting is inconsistent"
    );
    Ok(accounting)
}

pub(in crate::watch) fn bluetooth_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
) -> Result<BluetoothMetrics> {
    let root = output.join("bluetooth");
    let (replay, replay_sha256) = report::read_json_with_sha256(&root.join("replay.json"))?;
    let (packet, packet_summary_sha256) =
        report::read_json_with_sha256(&root.join("summary.json"))?;
    let (bursts, burst_summary_sha256) = report::read_json_with_sha256(&root.join("bursts.json"))?;
    let outcome = orchestrate::bluetooth_outcome(&replay, source)?;
    require_outcome(
        outcome,
        terminal_outcome(terminal, "bluetooth")?,
        "Bluetooth",
    )?;
    ensure!(
        report::required_u64(&bursts, "schema_version")? == 1,
        "unsupported Bluetooth burst summary schema"
    );
    ensure!(
        report::required_u64(&packet, "input_linktype")? == 192,
        "Bluetooth packet summary link type differs from Blue Dragon PPI"
    );
    let decoder = replay
        .get("decoder_program")
        .context("Bluetooth replay lacks nested decoder provenance")?;
    let decoder_identity = replay
        .get("decoder_executable_identity")
        .context("Bluetooth replay lacks nested decoder file identity")?;
    let decoder_bytes = report::required_u64(decoder, "bytes")?;
    ensure!(
        report::required_u64(decoder, "schema_version")? == 1
            && decoder.get("name").and_then(Value::as_str) == Some("blue-dragon")
            && (1..=MAX_NESTED_DECODER_BYTES).contains(&decoder_bytes)
            && report::required_u64(decoder_identity, "length")? == decoder_bytes,
        "Bluetooth nested decoder provenance contract differs"
    );
    let decoder_program = NestedDecoderProgram {
        schema_version: 1,
        name: "blue-dragon",
        bytes: decoder_bytes,
        sha256: required_sha256(decoder, "sha256")?,
    };
    let validated_advertising_packets =
        report::required_u64(&packet, "validated_advertising_packets")?;
    let validated_data_packets = report::required_u64(&packet, "validated_data_packets")?;
    let packet_validated = validated_advertising_packets
        .checked_add(validated_data_packets)
        .context("Bluetooth validated packet total overflow")?;
    let duplicate_accounting = bluetooth_duplicate_accounting(&packet, packet_validated)?;
    let validated_ble_packets = report::required_u64(&replay, "validated_ble_packets")?;
    let candidate_records = report::required_u64(&replay, "candidate_records")?;
    let invalid_crc_packets = report::required_u64(&replay, "invalid_crc_packets")?;
    let unknown_crc_packets = report::required_u64(&replay, "unknown_crc_packets")?;
    let malformed_packets = report::required_u64(&replay, "malformed_packets")?;
    let truncated_records = report::required_u64(&packet, "truncated_records")?;
    let unsupported_records = report::required_u64(&packet, "unsupported_records")?;
    let records_exceeding_snaplen = report::required_u64(&packet, "records_exceeding_snaplen")?;
    let classic_candidate_records = report::required_u64(&replay, "classic_candidate_records")?;
    let classic_upstream_crc_claims = report::required_u64(&replay, "classic_upstream_crc_claims")?;
    let burst_archive_records = report::required_u64(&replay, "burst_archive_records")?;
    let burst_archive_source_bytes = report::required_u64(&bursts, "source_bytes")?;
    let burst_archive_complex_samples = report::required_u64(&bursts, "complex_sample_count")?;
    let burst_archive_squelch_records = report::required_u64(&bursts, "squelch_record_count")?;
    let burst_archive_scan_window_records =
        report::required_u64(&bursts, "scan_window_record_count")?;
    ensure!(
        report::required_u64(&packet, "total_records")? == candidate_records
            && packet_validated == validated_ble_packets
            && report::required_u64(&packet, "invalid_crc_packets")? == invalid_crc_packets
            && report::required_u64(&packet, "unknown_crc_packets")? == unknown_crc_packets
            && report::required_u64(&packet, "malformed_packets")? == malformed_packets
            && report::required_u64(&packet, "classic_records")? == classic_candidate_records
            && report::required_u64(&packet, "classic_upstream_crc_claims")?
                == classic_upstream_crc_claims
            && report::required_u64(&bursts, "archive_record_count")? == burst_archive_records,
        "Bluetooth replay counts differ from packet or burst summaries"
    );
    let classified = [
        validated_advertising_packets,
        validated_data_packets,
        invalid_crc_packets,
        unknown_crc_packets,
        malformed_packets,
        truncated_records,
        classic_candidate_records,
        unsupported_records,
    ]
    .into_iter()
    .try_fold(0u64, |sum, value| sum.checked_add(value))
    .context("Bluetooth packet partition overflow")?;
    ensure!(
        classified == candidate_records
            && classic_upstream_crc_claims <= classic_candidate_records
            && records_exceeding_snaplen <= candidate_records,
        "Bluetooth packet partition or overlapping diagnostic count differs"
    );
    ensure!(
        bursts.get("source_format").and_then(Value::as_str) == Some("blue_dragon_burst_v1")
            && report::required_u64(&bursts, "channel_sample_rate_hz")? == 2_000_000
            && burst_archive_squelch_records.checked_add(burst_archive_scan_window_records)
                == Some(burst_archive_records)
            && 16u64
                .checked_add(
                    burst_archive_records
                        .checked_mul(48)
                        .context("Bluetooth burst header extent overflow")?,
                )
                .and_then(|bytes| {
                    burst_archive_complex_samples
                        .checked_mul(4)
                        .and_then(|samples| bytes.checked_add(samples))
                })
                == Some(burst_archive_source_bytes)
            && stable_regular_file_len(&root.join("bursts.bdb"))? == burst_archive_source_bytes,
        "Bluetooth burst archive format, partition or extent differs"
    );
    Ok(BluetoothMetrics {
        replay_sha256,
        packet_summary_sha256,
        burst_summary_sha256,
        decoder_program,
        input_complex_samples: report::required_u64(&replay, "input_complex_samples")?,
        decoder_processed_complex_samples: report::required_u64(
            &replay,
            "decoder_processed_complex_samples",
        )?,
        channelizer_step_complex_samples: report::required_u64(
            &replay,
            "channelizer_step_complex_samples",
        )?,
        decoder_file_block_complex_samples: report::required_u64(
            &replay,
            "decoder_file_block_complex_samples",
        )?,
        channelizer_steps_processed: report::required_u64(&replay, "channelizer_steps_processed")?,
        candidate_records,
        validated_advertising_packets,
        validated_data_packets,
        validated_ble_packets,
        duplicate_accounting,
        invalid_crc_packets,
        unknown_crc_packets,
        malformed_packets,
        truncated_records,
        unsupported_records,
        records_exceeding_snaplen,
        classic_candidate_records,
        classic_upstream_crc_claims,
        burst_archive_records,
        burst_archive_source_bytes,
        burst_archive_complex_samples,
        burst_archive_squelch_records,
        burst_archive_scan_window_records,
        burst_archive_truncated_by_limit: required_bool(
            &replay,
            "burst_archive_truncated_by_limit",
        )?,
        radio_opened: required_bool(&replay, "radio_opened")?,
        network_requested: required_bool(&replay, "network_requested")?,
        transmit_requested: required_bool(&replay, "transmit_requested")?,
    })
}
