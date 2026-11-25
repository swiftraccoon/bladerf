//! Exact validation for the optional LE Coded watch shadow.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

use super::{exact_object, required_bool, required_str};
use crate::report;

const CODED_SHADOW_CENTER_HZ: u64 = 2_435_000_000;
const CODED_SHADOW_SAMPLE_RATE_HZ: u64 = 20_000_000;
const MIN_CODED_SHADOW_SAMPLES: u64 = 327_680;
const MAX_CODED_SHADOW_SAMPLES: u64 = 20_000_000;
const CHANNELIZER_STEP_COMPLEX_SAMPLES: u64 = 10;
const CODED_SCAN_STEP_CHANNEL_SAMPLES: u64 = 32_768;

fn required_sha256(value: &Value, field: &str) -> Result<()> {
    let digest = required_str(value, field)?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "watch report field {field} is not a lowercase SHA-256 digest"
    );
    Ok(())
}

pub(super) fn validate_coded_shadow_request(run: &Value, schema_version: u64) -> Result<()> {
    if schema_version == 5 {
        ensure!(
            required_bool(run, "bluetooth_coded_shadow_requested")?,
            "schema-5 watch report does not request the coded shadow"
        );
    } else {
        ensure!(
            run.get("bluetooth_coded_shadow_requested").is_none(),
            "legacy watch report contains schema-5 coded-shadow settings"
        );
    }
    Ok(())
}

pub(super) fn validate_coded_shadow_capture_plan(
    sample_rate_hz: u64,
    samples_per_capture: u64,
) -> Result<()> {
    ensure!(
        sample_rate_hz == CODED_SHADOW_SAMPLE_RATE_HZ
            && (MIN_CODED_SHADOW_SAMPLES..=MAX_CODED_SHADOW_SAMPLES).contains(&samples_per_capture)
            && samples_per_capture.is_multiple_of(CHANNELIZER_STEP_COMPLEX_SAMPLES),
        "schema-5 capture plan is outside the 20 MHz aligned coded-shadow bounds"
    );
    Ok(())
}

pub(super) fn validate_coded_shadow(
    analysis: &Value,
    center_frequency_hz: u64,
    expected_input_samples: u64,
    schema_version: u64,
) -> Result<()> {
    let Some(shadow) = analysis.get("coded_shadow") else {
        ensure!(
            schema_version != 5,
            "complete schema-5 watch analysis lacks coded-shadow evidence"
        );
        return Ok(());
    };
    ensure!(
        schema_version == 5,
        "legacy watch analysis contains schema-5 coded-shadow evidence"
    );
    match required_str(shadow, "state")? {
        "not_eligible" => {
            exact_object(
                shadow,
                &["state", "center_frequency_hz"],
                "coded-shadow ineligible result",
            )?;
            ensure!(
                center_frequency_hz != CODED_SHADOW_CENTER_HZ
                    && report::required_u64(shadow, "center_frequency_hz")? == center_frequency_hz,
                "coded-shadow ineligible center differs or is actually eligible"
            );
        }
        "complete" => {
            exact_object(
                shadow,
                &["state", "metrics"],
                "coded-shadow complete result",
            )?;
            ensure!(
                center_frequency_hz == CODED_SHADOW_CENTER_HZ,
                "coded-shadow completed at an ineligible center"
            );
            let metrics = shadow
                .get("metrics")
                .context("coded-shadow complete result lacks metrics")?;
            exact_object(
                metrics,
                &[
                    "request_sha256",
                    "replay_sha256",
                    "packet_summary_sha256",
                    "burst_summary_sha256",
                    "candidate_pcap_sha256",
                    "validated_pcap_sha256",
                    "burst_archive_sha256",
                    "decoder_program",
                    "input_complex_samples",
                    "decoder_processed_complex_samples",
                    "channelizer_steps_processed",
                    "processed_full_input",
                    "candidate_records",
                    "validated_ble_packets",
                    "validated_le_1m_packets",
                    "validated_le_2m_packets",
                    "validated_coded_s8_packets",
                    "validated_coded_s2_packets",
                    "invalid_crc_packets",
                    "unknown_crc_packets",
                    "malformed_packets",
                    "unsupported_records",
                    "truncated_records",
                    "records_exceeding_snaplen",
                    "classic_candidate_records",
                    "classic_upstream_crc_claims",
                    "scan_window_records",
                    "burst_archive_truncated_by_limit",
                    "radio_opened",
                    "network_requested",
                    "transmit_requested",
                ],
                "coded-shadow metrics",
            )?;
            for field in [
                "request_sha256",
                "replay_sha256",
                "packet_summary_sha256",
                "burst_summary_sha256",
                "candidate_pcap_sha256",
                "validated_pcap_sha256",
                "burst_archive_sha256",
            ] {
                required_sha256(metrics, field)?;
            }
            let decoder = metrics
                .get("decoder_program")
                .context("coded-shadow metrics lack decoder identity")?;
            exact_object(
                decoder,
                &["schema_version", "name", "bytes", "sha256"],
                "coded-shadow decoder identity",
            )?;
            let ordinary_decoder = analysis
                .get("metrics")
                .and_then(|value| value.get("bluetooth"))
                .and_then(|value| value.get("decoder_program"))
                .context("coded-shadow analysis lacks ordinary decoder identity")?;
            ensure!(
                report::required_u64(decoder, "schema_version")? == 1
                    && required_str(decoder, "name")? == "blue-dragon"
                    && report::required_u64(decoder, "bytes")? > 0
                    && decoder == ordinary_decoder,
                "coded-shadow decoder identity differs"
            );
            required_sha256(decoder, "sha256")?;
            let input_samples = report::required_u64(metrics, "input_complex_samples")?;
            let expected_scan_windows =
                input_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES / CODED_SCAN_STEP_CHANNEL_SAMPLES;
            ensure!(
                input_samples == expected_input_samples
                    && input_samples.is_multiple_of(CHANNELIZER_STEP_COMPLEX_SAMPLES)
                    && report::required_u64(metrics, "decoder_processed_complex_samples")?
                        == input_samples
                    && required_bool(metrics, "processed_full_input")?
                    && report::required_u64(metrics, "channelizer_steps_processed")?
                        == input_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES
                    && expected_scan_windows > 0
                    && report::required_u64(metrics, "scan_window_records")?
                        == expected_scan_windows,
                "coded-shadow full-input accounting differs"
            );
            let validated = report::required_u64(metrics, "validated_ble_packets")?;
            let phy_sum = [
                "validated_le_1m_packets",
                "validated_le_2m_packets",
                "validated_coded_s8_packets",
                "validated_coded_s2_packets",
            ]
            .into_iter()
            .try_fold(0u64, |sum, field| {
                sum.checked_add(report::required_u64(metrics, field)?)
                    .context("coded-shadow PHY count overflow")
            })?;
            ensure!(
                phy_sum == validated,
                "coded-shadow validated PHY partition differs"
            );
            let candidate_records = report::required_u64(metrics, "candidate_records")?;
            let classified_records = [
                "invalid_crc_packets",
                "unknown_crc_packets",
                "malformed_packets",
                "unsupported_records",
                "truncated_records",
                "classic_candidate_records",
            ]
            .into_iter()
            .try_fold(validated, |sum, field| {
                sum.checked_add(report::required_u64(metrics, field)?)
                    .context("coded-shadow packet partition overflow")
            })?;
            ensure!(
                classified_records == candidate_records
                    && report::required_u64(metrics, "records_exceeding_snaplen")?
                        <= candidate_records
                    && report::required_u64(metrics, "classic_upstream_crc_claims")?
                        <= report::required_u64(metrics, "classic_candidate_records")?
                    && !required_bool(metrics, "burst_archive_truncated_by_limit")?
                    && !required_bool(metrics, "radio_opened")?
                    && !required_bool(metrics, "network_requested")?
                    && !required_bool(metrics, "transmit_requested")?,
                "coded-shadow packet accounting differs, or metrics report truncation or an unsafe request"
            );
        }
        _ => bail!("coded-shadow result has an unknown state"),
    }
    Ok(())
}
