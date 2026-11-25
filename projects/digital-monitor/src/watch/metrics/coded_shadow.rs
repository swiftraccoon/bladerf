use super::bluetooth::bluetooth_duplicate_accounting;
use super::{
    BluetoothCodedShadowMetrics, Context, MAX_NESTED_DECODER_BYTES, NestedDecoderProgram, Path,
    Result, SourceReport, Value, ensure, report, required_bool, required_sha256,
    stable_regular_file_len,
};
use crate::input::NORMALIZED_SC16_Q11_FORMAT;

const MAX_PCAP_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BURST_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_SIDECAR_BYTES: u64 = 65_536;
const CHANNELIZER_STEP_COMPLEX_SAMPLES: u64 = 10;
const DECODER_FILE_BLOCK_COMPLEX_SAMPLES: u64 = 65_530;
const CODED_SCAN_STEP_CHANNEL_SAMPLES: u64 = 32_768;

fn decoder_program(value: &Value, identity: &Value) -> Result<NestedDecoderProgram> {
    let bytes = report::required_u64(value, "bytes")?;
    ensure!(
        report::required_u64(value, "schema_version")? == 1
            && value.get("name").and_then(Value::as_str) == Some("blue-dragon")
            && (1..=MAX_NESTED_DECODER_BYTES).contains(&bytes)
            && report::required_u64(identity, "length")? == bytes,
        "coded-shadow nested decoder provenance contract differs"
    );
    Ok(NestedDecoderProgram {
        schema_version: 1,
        name: "blue-dragon",
        bytes,
        sha256: required_sha256(value, "sha256")?,
    })
}

fn required_safe_false(value: &Value, field: &str) -> Result<()> {
    ensure!(
        !required_bool(value, field)?,
        "coded-shadow report requested unsafe operation {field}"
    );
    Ok(())
}

fn validate_scan_request(value: &Value) -> Result<()> {
    ensure!(
        required_bool(value, "coded_scan_requested")?,
        "coded-shadow report did not request coded scanning"
    );
    let frequencies = value
        .get("coded_scan_primary_frequencies_mhz")
        .and_then(Value::as_array)
        .context("coded-shadow report lacks a frequency list")?;
    ensure!(
        frequencies.len() == 1 && frequencies[0].as_u64() == Some(2426),
        "coded-shadow report searched a different primary frequency set"
    );
    Ok(())
}

fn expected_scan_window_records(source: &SourceReport) -> Result<u64> {
    ensure!(
        source
            .complex_samples
            .is_multiple_of(CHANNELIZER_STEP_COMPLEX_SAMPLES),
        "coded-shadow input is not aligned to the channelizer step"
    );
    Ok(source.complex_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES / CODED_SCAN_STEP_CHANNEL_SAMPLES)
}

fn validated_phy_counts(packet: &Value, validated_packets: u64) -> Result<[u64; 4]> {
    let value = packet
        .get("validated_packets_by_phy")
        .context("coded-shadow packet summary lacks PHY accounting")?;
    let object = value
        .as_object()
        .context("coded-shadow PHY accounting is not an object")?;
    let fields = ["unknown", "le_1m", "le_2m", "le_coded_s8", "le_coded_s2"];
    ensure!(
        object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field)),
        "coded-shadow PHY-accounting schema differs"
    );
    let unknown = report::required_u64(value, "unknown")?;
    let counts = [
        report::required_u64(value, "le_1m")?,
        report::required_u64(value, "le_2m")?,
        report::required_u64(value, "le_coded_s8")?,
        report::required_u64(value, "le_coded_s2")?,
    ];
    let accounted = counts
        .into_iter()
        .try_fold(unknown, u64::checked_add)
        .context("coded-shadow PHY packet total overflow")?;
    ensure!(
        unknown == 0 && accounted == validated_packets,
        "coded-shadow PHY accounting does not partition validated packets"
    );
    Ok(counts)
}

fn validate_request(
    request: &Value,
    source: &SourceReport,
    ordinary_decoder: &NestedDecoderProgram,
) -> Result<()> {
    ensure!(
        report::required_u64(request, "schema_version")? == 1
            && request.get("operation").and_then(Value::as_str) == Some("offline_sc16q11_replay")
            && request.get("engine").and_then(Value::as_str) == Some("blue-dragon")
            && required_bool(request, "full_input_required")?
            && report::required_u64(request, "max_samples")? == source.complex_samples
            && report::required_u64(request, "burst_limit_mib")? == 128
            && report::required_u64(request, "decoder_output_soft_limit_mib")? == 512
            && report::required_u64(request, "channelizer_step_complex_samples")?
                == CHANNELIZER_STEP_COMPLEX_SAMPLES
            && report::required_u64(request, "decoder_file_block_complex_samples")?
                == DECODER_FILE_BLOCK_COMPLEX_SAMPLES,
        "coded-shadow request contract differs"
    );
    validate_scan_request(request)?;
    for field in ["radio_opened", "network_requested", "transmit_requested"] {
        required_safe_false(request, field)?;
    }
    ensure!(
        request.get("input").and_then(Value::as_str) == source.input.to_str()
            && request.get("sidecar").and_then(Value::as_str) == source.sidecar.to_str(),
        "coded-shadow request input paths differ from the accepted capture"
    );
    let acquisition = request
        .get("acquisition")
        .context("coded-shadow request lacks acquisition settings")?;
    ensure!(
        acquisition.get("format").and_then(Value::as_str) == Some("SC16_Q11")
            && acquisition.get("dtype").and_then(Value::as_str) == Some("<i2")
            && acquisition.get("interleave").and_then(Value::as_str) == Some("I,Q")
            && report::required_u64(acquisition, "complex_scale")? == 2048
            && report::required_u64(acquisition, "num_samples")? == source.complex_samples
            && acquisition.get("frequency_hz").and_then(Value::as_f64)
                == Some(source.center_frequency_hz as f64)
            && acquisition.get("sample_rate_hz").and_then(Value::as_f64)
                == Some(source.sample_rate_hz as f64)
            && acquisition.get("bandwidth_hz").and_then(Value::as_f64)
                == Some(source.bandwidth_hz as f64),
        "coded-shadow request acquisition settings differ from the accepted capture"
    );
    ensure!(
        request
            .get("input_identity")
            .and_then(|identity| identity.get("length"))
            .and_then(Value::as_u64)
            == Some(source.bytes),
        "coded-shadow request input extent differs"
    );
    let observed_decoder = decoder_program(
        request
            .get("decoder_program")
            .context("coded-shadow request lacks nested decoder provenance")?,
        request
            .get("decoder_executable_identity")
            .context("coded-shadow request lacks nested decoder file identity")?,
    )?;
    ensure!(
        observed_decoder == *ordinary_decoder,
        "coded-shadow nested decoder differs from ordinary replay"
    );
    Ok(())
}

pub(in crate::watch) fn bluetooth_coded_shadow_metrics(
    root: &Path,
    source: &SourceReport,
    ordinary_decoder: &NestedDecoderProgram,
) -> Result<BluetoothCodedShadowMetrics> {
    ensure!(
        source.format == NORMALIZED_SC16_Q11_FORMAT && source.center_frequency_hz == 2_435_000_000,
        "coded-shadow metrics require normalized SC16_Q11 from an eligible 2435 MHz capture"
    );
    let source_digest = report::hash_direct_regular_file(&source.input, source.bytes)?;
    let sidecar_digest = report::hash_direct_regular_file(&source.sidecar, MAX_SIDECAR_BYTES)?;
    ensure!(
        source_digest.bytes == source.bytes
            && source_digest.sha256 == source.sha256
            && sidecar_digest.sha256 == source.sidecar_sha256,
        "coded-shadow source or sidecar changed after ordinary analysis"
    );

    let (request, request_sha256) = report::read_json_with_sha256(&root.join("request.json"))?;
    validate_request(&request, source, ordinary_decoder)?;
    let (replay, replay_sha256) = report::read_json_with_sha256(&root.join("replay.json"))?;
    let (packet, packet_summary_sha256) =
        report::read_json_with_sha256(&root.join("summary.json"))?;
    let (bursts, burst_summary_sha256) = report::read_json_with_sha256(&root.join("bursts.json"))?;

    ensure!(
        report::required_u64(&replay, "schema_version")? == 2,
        "coded-shadow replay schema differs"
    );
    validate_scan_request(&replay)?;
    for field in ["radio_opened", "network_requested", "transmit_requested"] {
        required_safe_false(&replay, field)?;
    }
    ensure!(
        required_bool(&replay, "processed_full_input")?
            && report::required_u64(&replay, "input_complex_samples")? == source.complex_samples
            && report::required_u64(&replay, "decoder_processed_complex_samples")?
                == source.complex_samples
            && report::required_u64(&replay, "channelizer_step_complex_samples")?
                == CHANNELIZER_STEP_COMPLEX_SAMPLES
            && report::required_u64(&replay, "channelizer_steps_processed")?
                == source.complex_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES
            && report::required_u64(&replay, "decoder_file_block_complex_samples")?
                == DECODER_FILE_BLOCK_COMPLEX_SAMPLES
            && replay.get("decoder_exit_code").and_then(Value::as_i64) == Some(0),
        "coded-shadow replay did not process the complete accepted input"
    );
    let decoder_program = decoder_program(
        replay
            .get("decoder_program")
            .context("coded-shadow replay lacks nested decoder provenance")?,
        replay
            .get("decoder_executable_identity")
            .context("coded-shadow replay lacks nested decoder file identity")?,
    )?;
    ensure!(
        decoder_program == *ordinary_decoder,
        "coded-shadow nested decoder differs from ordinary replay"
    );

    ensure!(
        report::required_u64(&packet, "input_linktype")? == 192,
        "coded-shadow packet summary link type differs from Blue Dragon PPI"
    );
    let validated_advertising_packets =
        report::required_u64(&packet, "validated_advertising_packets")?;
    let validated_data_packets = report::required_u64(&packet, "validated_data_packets")?;
    let validated_ble_packets = validated_advertising_packets
        .checked_add(validated_data_packets)
        .context("coded-shadow validated packet total overflow")?;
    ensure!(
        report::required_u64(&replay, "validated_ble_packets")? == validated_ble_packets,
        "coded-shadow validated packet counts differ"
    );
    let _duplicate_accounting = bluetooth_duplicate_accounting(&packet, validated_ble_packets)?;
    let [
        validated_le_1m_packets,
        validated_le_2m_packets,
        validated_coded_s8_packets,
        validated_coded_s2_packets,
    ] = validated_phy_counts(&packet, validated_ble_packets)?;
    let candidate_records = report::required_u64(&replay, "candidate_records")?;
    let invalid_crc_packets = report::required_u64(&replay, "invalid_crc_packets")?;
    let unknown_crc_packets = report::required_u64(&replay, "unknown_crc_packets")?;
    let malformed_packets = report::required_u64(&replay, "malformed_packets")?;
    let truncated_records = report::required_u64(&packet, "truncated_records")?;
    let unsupported_records = report::required_u64(&packet, "unsupported_records")?;
    let records_exceeding_snaplen = report::required_u64(&packet, "records_exceeding_snaplen")?;
    let classic_candidate_records = report::required_u64(&replay, "classic_candidate_records")?;
    let classic_upstream_crc_claims = report::required_u64(&replay, "classic_upstream_crc_claims")?;
    let expected_outcome = if validated_ble_packets > 0 {
        "completed_with_validated_ble"
    } else if candidate_records > 0 {
        "completed_candidates_without_validated_ble"
    } else {
        "completed_zero_candidates"
    };
    ensure!(
        report::required_u64(&packet, "total_records")? == candidate_records
            && report::required_u64(&packet, "invalid_crc_packets")? == invalid_crc_packets
            && report::required_u64(&packet, "unknown_crc_packets")? == unknown_crc_packets
            && report::required_u64(&packet, "malformed_packets")? == malformed_packets
            && report::required_u64(&packet, "classic_records")? == classic_candidate_records
            && report::required_u64(&packet, "classic_upstream_crc_claims")?
                == classic_upstream_crc_claims
            && replay.get("outcome").and_then(Value::as_str) == Some(expected_outcome)
            && required_bool(
                &replay,
                "classic_candidates_are_not_independently_validated"
            )?
            && required_bool(&replay, "zero_candidates_does_not_establish_absence")?,
        "coded-shadow replay counts differ from the packet summary"
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
    .try_fold(0u64, u64::checked_add)
    .context("coded-shadow packet partition overflow")?;
    ensure!(
        classified == candidate_records
            && classic_upstream_crc_claims <= classic_candidate_records
            && records_exceeding_snaplen <= candidate_records,
        "coded-shadow packet partition or overlapping diagnostics differ"
    );

    ensure!(
        report::required_u64(&bursts, "schema_version")? == 1
            && bursts.get("source_format").and_then(Value::as_str) == Some("blue_dragon_burst_v1")
            && report::required_u64(&bursts, "channel_sample_rate_hz")? == 2_000_000,
        "coded-shadow burst summary contract differs"
    );
    let burst_archive_records = report::required_u64(&replay, "burst_archive_records")?;
    let squelch_records = report::required_u64(&bursts, "squelch_record_count")?;
    let scan_window_records = report::required_u64(&bursts, "scan_window_record_count")?;
    let burst_complex_samples = report::required_u64(&bursts, "complex_sample_count")?;
    let burst_source_bytes = report::required_u64(&bursts, "source_bytes")?;
    let expected_scan_windows = expected_scan_window_records(source)?;
    ensure!(
        report::required_u64(&bursts, "archive_record_count")? == burst_archive_records
            && squelch_records.checked_add(scan_window_records) == Some(burst_archive_records)
            && scan_window_records == expected_scan_windows
            && expected_scan_windows > 0,
        "coded-shadow burst or scan-window accounting differs"
    );
    let expected_burst_bytes = 16u64
        .checked_add(
            burst_archive_records
                .checked_mul(48)
                .context("coded-shadow burst header extent overflow")?,
        )
        .and_then(|bytes| {
            burst_complex_samples
                .checked_mul(4)
                .and_then(|samples| bytes.checked_add(samples))
        })
        .context("coded-shadow burst archive extent overflow")?;
    ensure!(
        burst_source_bytes == expected_burst_bytes
            && stable_regular_file_len(&root.join("bursts.bdb"))? == burst_source_bytes,
        "coded-shadow burst archive extent differs"
    );
    let burst_archive_truncated_by_limit =
        required_bool(&replay, "burst_archive_truncated_by_limit")?;
    ensure!(
        !burst_archive_truncated_by_limit,
        "coded-shadow burst archive reached its configured limit"
    );
    let burst_digest =
        report::hash_direct_regular_file(&root.join("bursts.bdb"), MAX_BURST_ARCHIVE_BYTES)?;
    ensure!(
        burst_digest.bytes == burst_source_bytes,
        "coded-shadow burst archive digest extent differs"
    );
    let candidate_pcap =
        report::hash_direct_regular_file(&root.join("candidates.pcap"), MAX_PCAP_BYTES)?;
    let validated_pcap =
        report::hash_direct_regular_file(&root.join("validated.pcap"), MAX_PCAP_BYTES)?;

    Ok(BluetoothCodedShadowMetrics {
        request_sha256,
        replay_sha256,
        packet_summary_sha256,
        burst_summary_sha256,
        candidate_pcap_sha256: candidate_pcap.sha256,
        validated_pcap_sha256: validated_pcap.sha256,
        burst_archive_sha256: burst_digest.sha256,
        decoder_program,
        input_complex_samples: source.complex_samples,
        decoder_processed_complex_samples: source.complex_samples,
        channelizer_steps_processed: source.complex_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES,
        candidate_records,
        validated_ble_packets,
        validated_le_1m_packets,
        validated_le_2m_packets,
        validated_coded_s8_packets,
        validated_coded_s2_packets,
        invalid_crc_packets,
        unknown_crc_packets,
        malformed_packets,
        unsupported_records,
        truncated_records,
        records_exceeding_snaplen,
        classic_candidate_records,
        classic_upstream_crc_claims,
        scan_window_records,
        burst_archive_truncated_by_limit,
        processed_full_input: true,
        radio_opened: false,
        network_requested: false,
        transmit_requested: false,
    })
}

#[cfg(test)]
mod tests;
