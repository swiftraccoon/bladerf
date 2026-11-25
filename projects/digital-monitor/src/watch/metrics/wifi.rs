use super::{
    CompactAnalysis, Context, Path, Result, SourceReport, Value, WIFI_BUFFER_SAMPLE_LIMIT,
    WIFI_PENDING_FRAME_LIMIT, WifiDecoderHealth, WifiMetrics, WifiOfdmDecoderHealth,
    WifiPhyDecoderHealth, ensure, orchestrate, report, require_outcome, required_bool,
    terminal_outcome,
};
use crate::watch::{MAX_CAPTURE_SAMPLES, config::analysis_sample_limit};

pub(in crate::watch) fn wifi_phy_decoder_health(
    decode: &Value,
    field: &str,
) -> Result<WifiPhyDecoderHealth> {
    let stats = decode
        .get(field)
        .with_context(|| format!("Wi-Fi decoder summary lacks {field} health counters"))?;
    let health = WifiPhyDecoderHealth {
        valid_frames: report::required_u64(stats, "valid_frames")?,
        invalid_fcs: report::required_u64(stats, "invalid_fcs")?,
        rejected_frames: report::required_u64(stats, "rejected_frames")?,
        truncated_frames: report::required_u64(stats, "truncated_frames")?,
        dropped_frames: report::required_u64(stats, "dropped_frames")?,
    };
    ensure!(
        health.dropped_frames <= health.valid_frames,
        "Wi-Fi {field} dropped-frame count exceeds valid frames"
    );
    Ok(health)
}

pub(in crate::watch) fn wifi_ofdm_decoder_health(decode: &Value) -> Result<WifiOfdmDecoderHealth> {
    let health = wifi_phy_decoder_health(decode, "ofdm")?;
    let rejections = orchestrate::wifi_ofdm_rejection_metrics(decode)?;
    let data_failures = orchestrate::wifi_ofdm_data_failure_metrics(decode, &rejections)?;
    let strong_long_training =
        orchestrate::wifi_ofdm_strong_long_training_metrics(decode, &rejections)?;
    Ok(WifiOfdmDecoderHealth {
        valid_frames: health.valid_frames,
        invalid_fcs: health.invalid_fcs,
        rejected_frames: health.rejected_frames,
        truncated_frames: health.truncated_frames,
        dropped_frames: health.dropped_frames,
        rejections,
        data_failures,
        strong_long_training,
    })
}

pub(in crate::watch) fn wifi_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
) -> Result<WifiMetrics> {
    let (summary, summary_sha256) =
        report::read_json_with_sha256(&output.join("wifi/summary.json"))?;
    let (decode, decoder_summary_sha256) =
        report::read_json_with_sha256(&output.join("wifi/candidates.decode.json"))?;
    let outcome = orchestrate::wifi_outcome(&summary, &decode, source)?;
    require_outcome(outcome, terminal_outcome(terminal, "wifi")?, "Wi-Fi")?;
    ensure!(
        report::required_u64(&summary, "input_linktype")? == 127,
        "Wi-Fi summary link type differs from radiotap"
    );
    let expected_sample_limit = analysis_sample_limit(source.complex_samples);
    let sample_limit_reached = required_bool(&decode, "sample_limit_reached")?;
    let input_eof_observed = required_bool(&decode, "input_eof_observed")?;
    let expected_completion = if source.complex_samples == expected_sample_limit {
        (true, false)
    } else {
        (false, true)
    };
    ensure!(
        decode.get("decoder").and_then(Value::as_str) == Some("libcrafter-wifi-serial")
            && decode.get("iq_format").and_then(Value::as_str) == Some("sc16-q11")
            && report::required_u64(&decode, "sample_limit")? == expected_sample_limit
            && source.complex_samples <= MAX_CAPTURE_SAMPLES
            && (sample_limit_reached, input_eof_observed) == expected_completion,
        "Wi-Fi decoder identity, IQ format or full-input completion differs"
    );

    let ofdm = wifi_ofdm_decoder_health(&decode)?;
    let dsss = wifi_phy_decoder_health(&decode, "dsss")?;
    let published_valid_frames = ofdm
        .valid_frames
        .checked_sub(ofdm.dropped_frames)
        .and_then(|ofdm| {
            dsss.valid_frames
                .checked_sub(dsss.dropped_frames)
                .and_then(|dsss| ofdm.checked_add(dsss))
        })
        .context("Wi-Fi published valid-frame accounting overflow")?;
    let frames_written = report::required_u64(&decode, "frames_written")?;
    ensure!(
        published_valid_frames == frames_written,
        "Wi-Fi per-PHY valid/drop accounting differs from published frames"
    );
    let diagnostic_counts = decode
        .get("diagnostic_counts")
        .and_then(Value::as_object)
        .context("Wi-Fi decoder summary lacks the diagnostic-count map")?;
    ensure!(
        diagnostic_counts
            .values()
            .all(|count| count.as_u64().is_some()),
        "Wi-Fi diagnostic-count map contains a non-counter value"
    );
    let pending_frame_limit = report::required_u64(&decode, "pending_frame_limit")?;
    let buffer_sample_limit = report::required_u64(&decode, "buffer_sample_limit")?;
    ensure!(
        pending_frame_limit == WIFI_PENDING_FRAME_LIMIT
            && buffer_sample_limit == WIFI_BUFFER_SAMPLE_LIMIT,
        "Wi-Fi decoder capacity contract differs"
    );
    let clipped_samples = report::required_u64(&decode, "clipped_samples")?;
    ensure!(
        clipped_samples <= source.complex_samples,
        "Wi-Fi clipping diagnostic count exceeds processed samples"
    );
    let decoder_health = WifiDecoderHealth {
        ofdm,
        dsss,
        diagnostic_counts_are_lower_bounds: required_bool(
            &decode,
            "diagnostic_counts_are_lower_bounds",
        )?,
        diagnostic_batches_at_capacity: report::required_u64(
            &decode,
            "diagnostic_batches_at_capacity",
        )?,
        clipped_samples,
        pending_frame_limit,
        buffer_sample_limit,
    };

    let total_records = report::required_u64(&summary, "total_records")?;
    let validated_frames = report::required_u64(&summary, "validated_frames")?;
    let invalid_fcs_frames = report::required_u64(&summary, "invalid_fcs_frames")?;
    let unknown_fcs_frames = report::required_u64(&summary, "unknown_fcs_frames")?;
    let absent_fcs_frames = report::required_u64(&summary, "absent_fcs_frames")?;
    let malformed_frames = report::required_u64(&summary, "malformed_frames")?;
    let truncated_records = report::required_u64(&summary, "truncated_records")?;
    let classified = [
        validated_frames,
        invalid_fcs_frames,
        unknown_fcs_frames,
        absent_fcs_frames,
        malformed_frames,
        truncated_records,
    ]
    .into_iter()
    .try_fold(0u64, |sum, value| sum.checked_add(value))
    .context("Wi-Fi record partition overflow")?;
    ensure!(
        classified == total_records,
        "Wi-Fi record partition differs from total records"
    );
    let management_frames = report::required_u64(&summary, "management_frames")?;
    let control_frames = report::required_u64(&summary, "control_frames")?;
    let data_frames = report::required_u64(&summary, "data_frames")?;
    ensure!(
        management_frames
            .checked_add(control_frames)
            .and_then(|sum| sum.checked_add(data_frames))
            == Some(validated_frames),
        "Wi-Fi frame-class partition differs from validated frames"
    );
    let protected_frames = report::required_u64(&summary, "protected_frames")?;
    ensure!(
        protected_frames <= validated_frames,
        "Wi-Fi protected-frame count exceeds validated frames"
    );
    let frame_types = orchestrate::wifi_frame_type_metrics(&summary)?;
    let block_ack = orchestrate::wifi_block_ack_metrics(&summary, &frame_types)?;
    let eapol = orchestrate::wifi_eapol_metrics(&summary)?;
    let advertisements = orchestrate::wifi_advertisement_metrics(&summary)?;
    let records_exceeding_snaplen = report::required_u64(&summary, "records_exceeding_snaplen")?;
    let upstream_bad_fcs_claims = report::required_u64(&summary, "upstream_bad_fcs_claims")?;
    ensure!(
        records_exceeding_snaplen <= total_records && upstream_bad_fcs_claims <= total_records,
        "Wi-Fi overlapping diagnostic count exceeds total records"
    );
    Ok(WifiMetrics {
        summary_sha256,
        decoder_summary_sha256,
        decoder_health,
        total_records,
        validated_frames,
        invalid_fcs_frames,
        unknown_fcs_frames,
        absent_fcs_frames,
        malformed_frames,
        truncated_records,
        records_exceeding_snaplen,
        upstream_bad_fcs_claims,
        management_frames,
        control_frames,
        data_frames,
        protected_frames,
        frame_types,
        block_ack,
        eapol,
        advertisements,
    })
}
