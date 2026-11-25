//! File-only replay of one sealed segment with verified selected-bin evidence.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::input::{NORMALIZED_SC16_Q11_FORMAT, SourceReport};
use crate::process::{self, ProgramProvenance};
use crate::report::{self, required_u64 as count};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const CHANNELIZER_STEP: u64 = 10;
const FILE_BLOCK_SAMPLES: u64 = 65_530;
const SCAN_STEP_CHANNEL_SAMPLES: u64 = 32_768;
const SCAN_OVERLAP_CHANNEL_SAMPLES: u64 = 16_384;
const COMPLETE_SCAN_OVERLAP_CHANNEL_SAMPLES: u64 = 34_080;
const BURST_LIMIT_MIB: u64 = 16;
const OUTPUT_LIMIT_MIB: u64 = 32;
const DECODER_TIMEOUT_SECONDS: u64 = 25;
const MANIFEST_BYTES: u64 = 64 * 1024;
const MIB: u64 = 1024 * 1024;
// Magic, version 1, header length 16, and a 2 MSps channel rate.
const EMPTY_BDB_HEADER: &[u8; 16] = b"BDBURST\0\x01\0\x10\0\x80\x84\x1e\0";

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Coverage {
    #[serde(skip_serializing_if = "Option::is_none")]
    window_geometry_version: Option<u8>,
    frequency_mhz: u32,
    pub(super) completed_windows: u64,
    first_start_channel_sample: u64,
    first_window_channel_samples: u64,
    last_start_channel_sample: u64,
    last_window_channel_samples: u64,
}

impl Coverage {
    fn expected(
        frequency_mhz: u32,
        complex_samples: u64,
        window_geometry_version: Option<u8>,
    ) -> Result<Self> {
        let channel_samples = complex_samples / CHANNELIZER_STEP;
        let (end_sample, overlap) = match window_geometry_version {
            None => (
                channel_samples / SCAN_STEP_CHANNEL_SAMPLES * SCAN_STEP_CHANNEL_SAMPLES,
                SCAN_OVERLAP_CHANNEL_SAMPLES,
            ),
            Some(2) => (channel_samples, COMPLETE_SCAN_OVERLAP_CHANNEL_SAMPLES),
            _ => anyhow::bail!("unsupported coded sentinel window geometry"),
        };
        let completed_windows = end_sample.div_ceil(SCAN_STEP_CHANNEL_SAMPLES);
        let last_start_channel_sample = (completed_windows.saturating_sub(1)
            * SCAN_STEP_CHANNEL_SAMPLES)
            .saturating_sub(overlap);
        Ok(Self {
            window_geometry_version,
            frequency_mhz,
            completed_windows,
            first_start_channel_sample: 0,
            first_window_channel_samples: end_sample.min(SCAN_STEP_CHANNEL_SAMPLES),
            last_start_channel_sample,
            last_window_channel_samples: end_sample - last_start_channel_sample,
        })
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectedCodedPackets {
    frequency_mhz: u32,
    pub(super) le_coded_s2: u64,
    pub(super) le_coded_s8: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Evidence {
    pub(super) coverage: Coverage,
    pub(super) selected_coded_packets: SelectedCodedPackets,
    candidate_records: u64,
    validated_ble_packets: u64,
    request_sha256: String,
    replay_sha256: String,
    packet_summary_sha256: String,
    pub(super) validated_pcap_sha256: String,
    wrapper_sha256: String,
    decoder_sha256: String,
}

fn verify_selected_mode(value: &Value, frequency_mhz: u32, omit_squelch_iq: bool) -> Result<()> {
    let ordinary_omission = value.get("coded_scan_squelch_iq_omitted");
    ensure!(
        value["coded_scan_requested"].as_bool() == Some(true)
            && value["coded_scan_iq_omitted"].as_bool() == Some(true)
            && ordinary_omission == omit_squelch_iq.then_some(&Value::Bool(true))
            && value["coded_scan_primary_frequencies_mhz"]
                .as_array()
                .is_some_and(Vec::is_empty)
            && count(value, "coded_scan_frequency_mhz")? == u64::from(frequency_mhz),
        "coded sentinel receipt differs from the requested scan mode or frequency"
    );
    for field in ["radio_opened", "network_requested", "transmit_requested"] {
        ensure!(
            value[field].as_bool() == Some(false),
            "coded sentinel receipt has unexpected {field} state"
        );
    }
    Ok(())
}

fn verify_decoder(value: &Value, decoder: &ProgramProvenance) -> Result<()> {
    let program = &value["decoder_program"];
    ensure!(
        value["decoder_executable"].as_str() == decoder.path.to_str()
            && count(&value["decoder_executable_identity"], "length")? == decoder.bytes
            && count(program, "schema_version")? == 1
            && program["name"].as_str() == Some("blue-dragon")
            && count(program, "bytes")? == decoder.bytes
            && program["sha256"].as_str() == Some(decoder.sha256.as_str()),
        "coded sentinel receipt differs from the pinned decoder"
    );
    Ok(())
}

fn verify_request(
    request: &Value,
    source: &SourceReport,
    frequency_mhz: u32,
    decoder: &ProgramProvenance,
    destination: &Path,
    input: &fs::Metadata,
    omit_squelch_iq: bool,
) -> Result<()> {
    verify_selected_mode(request, frequency_mhz, omit_squelch_iq)?;
    verify_decoder(request, decoder)?;
    ensure!(
        count(request, "schema_version")? == 1
            && request["operation"].as_str() == Some("offline_sc16q11_replay")
            && request["engine"].as_str() == Some("blue-dragon")
            && request["full_input_required"].as_bool() == Some(true)
            && request["input"].as_str() == source.input.to_str()
            && request["sidecar"].as_str() == source.sidecar.to_str()
            && count(request, "max_samples")? == source.complex_samples
            && count(request, "channelizer_step_complex_samples")? == CHANNELIZER_STEP
            && count(request, "decoder_file_block_complex_samples")? == FILE_BLOCK_SAMPLES
            && count(request, "burst_limit_mib")? == BURST_LIMIT_MIB
            && count(request, "decoder_output_soft_limit_mib")? == OUTPUT_LIMIT_MIB
            && request["decoder_timeout_seconds"].as_f64() == Some(DECODER_TIMEOUT_SECONDS as f64),
        "coded sentinel request differs from the staged replay contract"
    );
    let acquisition = &request["acquisition"];
    ensure!(
        acquisition["format"].as_str() == Some("SC16_Q11")
            && acquisition["dtype"].as_str() == Some("<i2")
            && acquisition["interleave"].as_str() == Some("I,Q")
            && count(acquisition, "complex_scale")? == 2048
            && acquisition["frequency_hz"].as_f64() == Some(source.center_frequency_hz as f64)
            && acquisition["sample_rate_hz"].as_f64() == Some(source.sample_rate_hz as f64)
            && acquisition["bandwidth_hz"].as_f64() == Some(source.bandwidth_hz as f64)
            && count(acquisition, "num_samples")? == source.complex_samples,
        "coded sentinel request differs from the staged acquisition settings"
    );
    let identity = &request["input_identity"];
    ensure!(
        count(identity, "device")? == input.dev()
            && count(identity, "inode")? == input.ino()
            && count(identity, "length")? == source.bytes
            && identity["modified_seconds"].as_i64() == Some(input.mtime())
            && identity["modified_nanoseconds"].as_i64() == Some(input.mtime_nsec())
            && identity["changed_seconds"].as_i64() == Some(input.ctime())
            && identity["changed_nanoseconds"].as_i64() == Some(input.ctime_nsec()),
        "coded sentinel request differs from the staged IQ identity"
    );
    let center_mhz = (source.center_frequency_hz / 1_000_000).to_string();
    let mut decoder_arguments = vec![
        "--file".into(),
        source.input.to_string_lossy().into_owned(),
        "--format".into(),
        "sc16q11".into(),
        "--sample-rate".into(),
        source.sample_rate_hz.to_string(),
        "--center-freq".into(),
        center_mhz,
        "--channels".into(),
        "20".into(),
        "--check-crc".into(),
        "--stats".into(),
        "--no-gpu".into(),
        "--squelch=-45".into(),
        "--write".into(),
        destination
            .join("candidates.pcap")
            .to_string_lossy()
            .into_owned(),
        "--write-bursts".into(),
        destination
            .join("bursts.bdb")
            .to_string_lossy()
            .into_owned(),
        "--burst-limit-mb".into(),
        BURST_LIMIT_MIB.to_string(),
        "--coded-scan-frequency-mhz".into(),
        frequency_mhz.to_string(),
        "--omit-scan-iq".into(),
    ];
    if omit_squelch_iq {
        decoder_arguments.push("--omit-squelch-iq".into());
    }
    ensure!(
        request["decoder_arguments"] == json!(decoder_arguments),
        "coded sentinel request has unexpected native decoder arguments"
    );
    Ok(())
}

fn verify_terminal(
    terminal: &Value,
    source: &SourceReport,
    frequency_mhz: u32,
    decoder: &ProgramProvenance,
    omit_squelch_iq: bool,
) -> Result<Coverage> {
    verify_selected_mode(terminal, frequency_mhz, omit_squelch_iq)?;
    verify_decoder(terminal, decoder)?;
    let schema_version = if omit_squelch_iq { 4 } else { 3 };
    ensure!(
        count(terminal, "schema_version")? == schema_version
            && terminal["decoder_exit_code"].as_i64() == Some(0)
            && terminal["processed_full_input"].as_bool() == Some(true)
            && count(terminal, "input_complex_samples")? == source.complex_samples
            && count(terminal, "decoder_processed_complex_samples")? == source.complex_samples
            && count(terminal, "channelizer_step_complex_samples")? == CHANNELIZER_STEP
            && count(terminal, "decoder_file_block_complex_samples")? == FILE_BLOCK_SAMPLES
            && count(terminal, "channelizer_steps_processed")?
                == source.complex_samples / CHANNELIZER_STEP
            && count(terminal, "coded_scan_window_records")? == 0
            && terminal["burst_archive_truncated_by_limit"].as_bool() == Some(false),
        "coded sentinel replay has an unexpected extent, status or scan archive"
    );
    let coverage = Coverage::deserialize(&terminal["coded_scan_coverage"])
        .context("parse coded sentinel coverage")?;
    ensure!(
        !omit_squelch_iq || coverage.window_geometry_version == Some(2),
        "coded sentinel without archived IQ requires full-tail scan coverage"
    );
    ensure!(
        coverage
            == Coverage::expected(
                frequency_mhz,
                source.complex_samples,
                coverage.window_geometry_version,
            )?,
        "coded sentinel reported different scan coverage than its full input"
    );
    Ok(coverage)
}

fn verify_counts(
    terminal: &Value,
    summary: &Value,
    frequency_mhz: u32,
) -> Result<(SelectedCodedPackets, u64, u64)> {
    let candidates = count(terminal, "candidate_records")?;
    let valid = count(terminal, "validated_ble_packets")?;
    let classified = [
        "validated_advertising_packets",
        "validated_data_packets",
        "invalid_crc_packets",
        "unknown_crc_packets",
        "malformed_packets",
        "truncated_records",
        "classic_records",
        "unsupported_records",
    ];
    let count_sum = |fields: &[&str], value: &Value| {
        fields.iter().try_fold(0_u64, |sum, field| {
            sum.checked_add(count(value, field)?)
                .context("coded sentinel packet count overflow")
        })
    };
    let outcome = if valid > 0 {
        "completed_with_validated_ble"
    } else if candidates > 0 {
        "completed_candidates_without_validated_ble"
    } else {
        "completed_zero_candidates"
    };
    ensure!(
        count(summary, "input_linktype")? == 192
            && count(summary, "total_records")? == candidates
            && count_sum(&classified, summary)? == candidates
            && count_sum(&classified[..2], summary)? == valid
            && terminal["outcome"].as_str() == Some(outcome),
        "coded sentinel replay totals disagree with the packet inspector"
    );
    for field in [
        "invalid_crc_packets",
        "unknown_crc_packets",
        "malformed_packets",
    ] {
        ensure!(
            count(terminal, field)? == count(summary, field)?,
            "coded sentinel replay disagrees with {field}"
        );
    }
    let classic = count(summary, "classic_records")?;
    ensure!(
        count(terminal, "classic_candidate_records")? == classic
            && count(terminal, "classic_upstream_crc_claims")?
                == count(summary, "classic_upstream_crc_claims")?
            && count(summary, "classic_upstream_crc_claims")? <= classic
            && count(summary, "records_exceeding_snaplen")? <= candidates,
        "coded sentinel replay disagrees with the Classic candidate count"
    );
    let phy = &summary["validated_packets_by_phy"];
    ensure!(
        count(phy, "unknown")? == 0
            && count_sum(&["le_1m", "le_2m", "le_coded_s8", "le_coded_s2"], phy)? == valid,
        "coded sentinel PHY counts do not partition validated packets"
    );
    let selected = SelectedCodedPackets::deserialize(&terminal["selected_coded_packets"])
        .context("parse selected-bin packet counts")?;
    let physical_channel = (frequency_mhz - 2402) / 2;
    let logical_channel = if physical_channel < 12 {
        physical_channel - 1
    } else {
        physical_channel - 2
    };
    let channels = summary["packets_by_logical_channel"]
        .as_object()
        .context("coded sentinel summary lacks logical channel counts")?;
    let on_channel = channels
        .get(&logical_channel.to_string())
        .map(|value| {
            value
                .as_u64()
                .context("malformed selected channel packet count")
        })
        .transpose()?
        .unwrap_or(0);
    ensure!(
        selected.frequency_mhz == frequency_mhz
            && selected.le_coded_s2 <= count(phy, "le_coded_s2")?
            && selected.le_coded_s8 <= count(phy, "le_coded_s8")?
            && selected
                .le_coded_s2
                .checked_add(selected.le_coded_s8)
                .is_some_and(|total| total <= on_channel)
            && on_channel <= valid,
        "coded sentinel selected-bin counts exceed the validated PHY or channel counts"
    );
    Ok((selected, candidates, valid))
}

fn verify_archive(destination: &Path, terminal: &Value, omit_squelch_iq: bool) -> Result<()> {
    let archive = report::read_json(&destination.join("bursts.json"))?;
    let ordinary = count(&archive, "squelch_record_count")?;
    let samples = count(&archive, "complex_sample_count")?;
    let archive_path = destination.join("bursts.bdb");
    let archive_stats = fs::symlink_metadata(&archive_path)?;
    let recorded_bytes = ordinary
        .checked_mul(48)
        .and_then(|headers| headers.checked_add(16))
        .and_then(|headers| samples.checked_mul(4)?.checked_add(headers));
    ensure!(
        count(&archive, "schema_version")? == 1
            && archive["source_format"].as_str() == Some("blue_dragon_burst_v1")
            && count(&archive, "channel_sample_rate_hz")? == 2_000_000
            && count(&archive, "scan_window_record_count")? == 0
            && count(&archive, "archive_record_count")? == ordinary
            && count(terminal, "burst_archive_records")? == ordinary
            && count(&archive, "source_bytes")? == archive_stats.len()
            && recorded_bytes == Some(archive_stats.len())
            && archive_stats.is_file()
            && archive_stats.len() <= BURST_LIMIT_MIB * MIB,
        "coded sentinel ordinary burst archive does not match its summary"
    );
    if omit_squelch_iq {
        ensure!(
            ordinary == 0 && samples == 0,
            "coded sentinel archived ordinary IQ after omission was requested"
        );
        let extent = EMPTY_BDB_HEADER.len() as u64;
        let file = report::hash_direct_regular_file(&archive_path, extent)?;
        ensure!(
            file.bytes == extent
                && file.sha256 == format!("{:x}", Sha256::digest(EMPTY_BDB_HEADER)),
            "coded sentinel empty burst archive has an invalid BDB header"
        );
    }
    Ok(())
}

/// Run a bounded wrapper in an owned private scratch directory and verify its
/// request, terminal coverage and packet accounting before returning evidence.
pub(super) fn run(
    output: &Path,
    source: &SourceReport,
    frequency_mhz: u32,
    wrapper: &ProgramProvenance,
    decoder: &ProgramProvenance,
    omit_squelch_iq: bool,
    stop: &AtomicBool,
) -> Result<Evidence> {
    ensure!(!stop.load(Ordering::Relaxed), "coded sentinel stopped");
    let center_mhz = source.center_frequency_hz / 1_000_000;
    ensure!(
        source.format == NORMALIZED_SC16_Q11_FORMAT
            && source.sample_rate_hz == SAMPLE_RATE_HZ
            && source.bandwidth_hz == SAMPLE_RATE_HZ
            && source.center_frequency_hz.is_multiple_of(1_000_000)
            && (2400..=2480).contains(&center_mhz)
            && (2404..=2478).contains(&frequency_mhz)
            && frequency_mhz != 2426
            && frequency_mhz.is_multiple_of(2)
            && u64::from(frequency_mhz).abs_diff(center_mhz) < 10
            && source.complex_samples > 0
            && source.complex_samples.is_multiple_of(CHANNELIZER_STEP)
            && source.complex_samples.checked_mul(4) == Some(source.bytes)
            && source.input.is_absolute()
            && source.sidecar.is_absolute()
            && output.is_absolute()
            && source.input.to_str().is_some()
            && source.sidecar.to_str().is_some()
            && output.to_str().is_some(),
        "coded sentinel requires staged 20 MSps SC16 Q11 and a supported interior data bin"
    );
    let input = fs::symlink_metadata(&source.input)?;
    let sidecar = fs::symlink_metadata(&source.sidecar)?;
    ensure!(
        input.is_file() && input.len() == source.bytes && sidecar.is_file(),
        "coded sentinel staged input is not a pair of direct files with the accepted IQ extent"
    );
    for program in [wrapper, decoder] {
        ensure!(
            process::resolve_program(program.name, &program.path)? == *program,
            "coded sentinel executable changed before launch"
        );
    }
    let destination = output.join("replay");
    report::private_directory(&output.join("logs"))?;
    let mut arguments = vec![
        OsString::from("--decoder"),
        decoder.path.as_os_str().to_owned(),
        "replay-iq".into(),
        source.input.as_os_str().to_owned(),
        "--center-mhz".into(),
        center_mhz.to_string().into(),
        "--max-samples".into(),
        source.complex_samples.to_string().into(),
        "--coded-scan-frequency-mhz".into(),
        frequency_mhz.to_string().into(),
        "--omit-scan-iq".into(),
        "--burst-limit-mib".into(),
        BURST_LIMIT_MIB.to_string().into(),
        "--output-limit-mib".into(),
        OUTPUT_LIMIT_MIB.to_string().into(),
        "--decoder-timeout".into(),
        DECODER_TIMEOUT_SECONDS.to_string().into(),
        "--output".into(),
        destination.as_os_str().to_owned(),
    ];
    if omit_squelch_iq {
        arguments.push("--omit-squelch-iq".into());
    }
    let command = process::run(
        wrapper,
        &arguments,
        output,
        "coded-sentinel",
        MANIFEST_BYTES,
        Duration::from_secs(35),
        stop,
    )?;
    process::require_success(&command)?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "coded sentinel wrapper exceeded its log budget"
    );
    for program in [wrapper, decoder] {
        ensure!(
            process::resolve_program(program.name, &program.path)? == *program,
            "coded sentinel executable changed during replay"
        );
    }
    for (path, initial) in [(&source.input, &input), (&source.sidecar, &sidecar)] {
        let current = fs::symlink_metadata(path)?;
        ensure!(
            current.is_file() && report::same_file_identity(initial, &current),
            "coded sentinel staged input changed during replay"
        );
    }
    let (request, request_sha256) =
        report::read_json_with_sha256_limit(&destination.join("request.json"), MANIFEST_BYTES)?;
    verify_request(
        &request,
        source,
        frequency_mhz,
        decoder,
        &destination,
        &input,
        omit_squelch_iq,
    )?;
    let (terminal, replay_sha256) =
        report::read_json_with_sha256_limit(&destination.join("replay.json"), MANIFEST_BYTES)?;
    let coverage = verify_terminal(&terminal, source, frequency_mhz, decoder, omit_squelch_iq)?;
    verify_archive(&destination, &terminal, omit_squelch_iq)?;
    let (summary, packet_summary_sha256) =
        report::read_json_with_sha256(&destination.join("summary.json"))?;
    let (selected_coded_packets, candidate_records, validated_ble_packets) =
        verify_counts(&terminal, &summary, frequency_mhz)?;
    let validated_pcap = report::hash_direct_regular_file(
        &destination.join("validated.pcap"),
        OUTPUT_LIMIT_MIB * MIB,
    )?;
    Ok(Evidence {
        coverage,
        selected_coded_packets,
        candidate_records,
        validated_ble_packets,
        request_sha256,
        replay_sha256,
        packet_summary_sha256,
        validated_pcap_sha256: validated_pcap.sha256,
        wrapper_sha256: wrapper.sha256.clone(),
        decoder_sha256: decoder.sha256.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        Capture, FAILURE_TEXT_BYTES, Job, Options, SNAPSHOT, Sentinel, WorkerFailure,
    };
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn source() -> SourceReport {
        SourceReport {
            input: "/scratch/iq.sc16q11".into(),
            sidecar: "/scratch/iq.sc16q11.json".into(),
            format: NORMALIZED_SC16_Q11_FORMAT,
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bandwidth_hz: SAMPLE_RATE_HZ,
            complex_samples: 19_988_480,
            bytes: 79_953_920,
            sha256: "a".repeat(64),
            sidecar_sha256: "b".repeat(64),
            continuity_guaranteed: false,
        }
    }

    fn decoder() -> ProgramProvenance {
        ProgramProvenance {
            name: "blue-dragon",
            path: "/decoder".into(),
            bytes: 16,
            sha256: "c".repeat(64),
        }
    }

    #[test]
    fn successful_replay_retains_only_bounded_failure_diagnostics() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let program = Path::new("/bin/true");
        let root = directory.path();
        let mut sentinel = Sentinel::prepare(
            root,
            Options {
                every: 1,
                spool_slots: 4,
                omit_squelch_iq: false,
            },
            2437,
            program,
            program,
            0,
        )?;
        let capture = |ordinal| Capture {
            ordinal,
            first_sample_ns: 0,
            end_sample_exclusive_ns: 999_424_000,
            segment_receipt_sha256: "a".repeat(64),
            source_sha256: "b".repeat(64),
            sidecar_sha256: "c".repeat(64),
        };
        let read_snapshot =
            || -> Result<Value> { Ok(serde_json::from_slice(&fs::read(root.join(SNAPSHOT))?)?) };

        let scratch = tempfile::tempdir_in(root)?;
        let failed_path = scratch.path().to_owned();
        fs::write(failed_path.join("iq.sc16q11"), b"raw IQ sample bytes")?;
        fs::create_dir(failed_path.join("logs"))?;
        fs::create_dir(failed_path.join("replay"))?;
        fs::write(
            failed_path.join("logs/coded-sentinel.stderr.log"),
            format!("{}wrapper failed", "𝄞".repeat(FAILURE_TEXT_BYTES)),
        )?;
        fs::write(
            failed_path.join("replay/decoder.log"),
            "terminal decoder cause",
        )?;
        sentinel.job = Some(Job {
            bin: 0,
            capture: capture(1),
            stop: Arc::new(AtomicBool::new(false)),
            handle: thread::spawn(move || {
                Err(WorkerFailure::from_replay(
                    anyhow::anyhow!("replay failed"),
                    scratch,
                ))
            }),
        });
        sentinel.finish(false);
        sentinel.publish_pending();
        assert!(!failed_path.exists());
        let failed = read_snapshot()?;
        assert_eq!(failed["latest"]["state"], "failed");
        assert_eq!(failed["latest"]["error"], "replay failed");
        let failure = &failed["latest_failure"];
        assert_eq!(failure["capture"]["ordinal"], 1);
        assert_eq!(failure["frequency_mhz"], 2428);
        assert_eq!(failure["error"], "replay failed");
        assert_eq!(failure["wrapper_stderr"]["truncated"], true);
        let text = failure["wrapper_stderr"]["text"].as_str().unwrap();
        assert!(text.ends_with("wrapper failed"));
        assert!(text.len() <= FAILURE_TEXT_BYTES);
        assert_eq!(failure["decoder_log"]["text"], "terminal decoder cause");
        assert_eq!(failure["decoder_log"]["truncated"], false);

        let scratch = tempfile::tempdir_in(root)?;
        let evidence = Evidence {
            coverage: Coverage::expected(2430, source().complex_samples, Some(2))?,
            selected_coded_packets: SelectedCodedPackets {
                frequency_mhz: 2430,
                le_coded_s2: 0,
                le_coded_s8: 0,
            },
            candidate_records: 0,
            validated_ble_packets: 0,
            request_sha256: "d".repeat(64),
            replay_sha256: "d".repeat(64),
            packet_summary_sha256: "d".repeat(64),
            validated_pcap_sha256: "d".repeat(64),
            wrapper_sha256: "d".repeat(64),
            decoder_sha256: "d".repeat(64),
        };
        sentinel.job = Some(Job {
            bin: 1,
            capture: capture(2),
            stop: Arc::new(AtomicBool::new(false)),
            handle: thread::spawn(move || Ok((evidence, scratch))),
        });
        sentinel.finish(false);
        sentinel.publish_pending();
        let completed = read_snapshot()?;
        assert_eq!(completed["schema_version"], 1);
        assert_eq!(completed["failed"], 1);
        assert_eq!(completed["completed"], 1);
        assert_eq!(completed["latest"]["state"], "complete");
        assert_eq!(completed["latest"]["capture"]["ordinal"], 2);
        assert_eq!(completed["latest_failure"], *failure);
        assert_eq!(fs::read_dir(root)?.count(), 1);
        Ok(())
    }

    #[test]
    fn full_selected_coverage_requires_extent_omit_mode_and_pinned_decoder() {
        let source = source();
        let decoder = decoder();
        let terminal = json!({
            "schema_version": 3,
            "coded_scan_requested": true,
            "coded_scan_iq_omitted": true,
            "coded_scan_primary_frequencies_mhz": [],
            "coded_scan_frequency_mhz": 2444,
            "radio_opened": false,
            "network_requested": false,
            "transmit_requested": false,
            "decoder_executable": decoder.path,
            "decoder_executable_identity": {"length": decoder.bytes},
            "decoder_program": {
                "schema_version": 1, "name": "blue-dragon",
                "bytes": decoder.bytes, "sha256": decoder.sha256,
            },
            "decoder_exit_code": 0,
            "processed_full_input": true,
            "input_complex_samples": 19_988_480,
            "decoder_processed_complex_samples": 19_988_480,
            "channelizer_step_complex_samples": 10,
            "decoder_file_block_complex_samples": 65530,
            "channelizer_steps_processed": 1_998_848,
            "coded_scan_window_records": 0,
            "burst_archive_truncated_by_limit": false,
            "coded_scan_coverage": {
                "frequency_mhz": 2444,
                "completed_windows": 61,
                "first_start_channel_sample": 0,
                "first_window_channel_samples": 32768,
                "last_start_channel_sample": 1949696,
                "last_window_channel_samples": 49152,
            }
        });
        let coverage = verify_terminal(&terminal, &source, 2444, &decoder, false).unwrap();
        assert_eq!(coverage.completed_windows, 61);
        let mut complete = terminal.clone();
        let geometry = &mut complete["coded_scan_coverage"];
        geometry["window_geometry_version"] = json!(2);
        geometry["last_start_channel_sample"] = json!(1932000);
        geometry["last_window_channel_samples"] = json!(66848);
        let coverage = verify_terminal(&complete, &source, 2444, &decoder, false).unwrap();
        assert_eq!(coverage.completed_windows, 61);
        let mut empty_archive = complete.clone();
        empty_archive["schema_version"] = json!(4);
        empty_archive["coded_scan_squelch_iq_omitted"] = json!(true);
        assert!(verify_terminal(&empty_archive, &source, 2444, &decoder, true).is_ok());
        assert!(verify_terminal(&empty_archive, &source, 2444, &decoder, false).is_err());
        assert!(verify_terminal(&complete, &source, 2444, &decoder, true).is_err());
        let mut incomplete = terminal.clone();
        incomplete["schema_version"] = json!(4);
        incomplete["coded_scan_squelch_iq_omitted"] = json!(true);
        assert!(verify_terminal(&incomplete, &source, 2444, &decoder, true).is_err());
        for (field, value) in [
            ("window_geometry_version", json!(3)),
            ("last_start_channel_sample", json!(1949696)),
            ("last_window_channel_samples", json!(49152)),
        ] {
            let mut wrong = complete.clone();
            wrong["coded_scan_coverage"][field] = value;
            assert!(verify_terminal(&wrong, &source, 2444, &decoder, false).is_err());
        }
        for (field, value) in [
            ("coded_scan_iq_omitted", json!(false)),
            ("coded_scan_window_records", json!(61)),
            ("burst_archive_truncated_by_limit", json!(true)),
            ("coded_scan_frequency_mhz", json!(2442)),
            ("decoder_processed_complex_samples", json!(19_988_470)),
        ] {
            let mut wrong = terminal.clone();
            wrong[field] = value;
            assert!(
                verify_terminal(&wrong, &source, 2444, &decoder, false).is_err(),
                "{field}"
            );
        }
        let mut wrong = terminal.clone();
        wrong["coded_scan_coverage"]["last_start_channel_sample"] = json!(1949697);
        assert!(verify_terminal(&wrong, &source, 2444, &decoder, false).is_err());
        let mut wrong = terminal;
        wrong["decoder_program"]["sha256"] = json!("d".repeat(64));
        assert!(verify_terminal(&wrong, &source, 2444, &decoder, false).is_err());
    }

    #[test]
    fn omitted_archive_requires_the_exact_empty_bdb_header() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let encoded = directory.path().join("bursts.bdb");
        let summary = json!({
            "schema_version": 1,
            "source_format": "blue_dragon_burst_v1",
            "channel_sample_rate_hz": 2_000_000,
            "source_bytes": 16,
            "squelch_record_count": 0,
            "scan_window_record_count": 0,
            "archive_record_count": 0,
            "complex_sample_count": 0
        });
        report::publish_json(directory.path(), "bursts.json", &summary)?;
        let terminal = json!({"burst_archive_records": 0});
        fs::write(&encoded, EMPTY_BDB_HEADER)?;
        verify_archive(directory.path(), &terminal, true)?;
        let mut corrupt = *EMPTY_BDB_HEADER;
        corrupt[12] ^= 0x01;
        fs::write(&encoded, corrupt)?;
        assert!(verify_archive(directory.path(), &terminal, true).is_err());
        fs::write(&encoded, [EMPTY_BDB_HEADER.as_slice(), &[0]].concat())?;
        assert!(verify_archive(directory.path(), &terminal, true).is_err());
        Ok(())
    }

    #[test]
    fn selected_counts_are_bounded_by_the_validated_phy_and_channel() {
        let terminal = json!({
            "candidate_records": 6,
            "validated_ble_packets": 5,
            "outcome": "completed_with_validated_ble",
            "invalid_crc_packets": 1,
            "unknown_crc_packets": 0,
            "malformed_packets": 0,
            "classic_candidate_records": 0,
            "classic_upstream_crc_claims": 0,
            "selected_coded_packets": {"frequency_mhz": 2444, "le_coded_s2": 1, "le_coded_s8": 2},
        });
        let summary = json!({
            "input_linktype": 192,
            "total_records": 6,
            "validated_advertising_packets": 3,
            "validated_data_packets": 2,
            "invalid_crc_packets": 1,
            "unknown_crc_packets": 0,
            "malformed_packets": 0,
            "truncated_records": 0,
            "classic_records": 0,
            "classic_upstream_crc_claims": 0,
            "unsupported_records": 0,
            "records_exceeding_snaplen": 0,
            "validated_packets_by_phy": {
                "unknown": 0, "le_1m": 1, "le_2m": 0, "le_coded_s8": 2, "le_coded_s2": 2,
            },
            "packets_by_logical_channel": {"19": 3, "18": 2},
        });
        let (selected, candidates, valid) = verify_counts(&terminal, &summary, 2444).unwrap();
        assert_eq!((selected.le_coded_s2, selected.le_coded_s8), (1, 2));
        assert_eq!((candidates, valid), (6, 5));
        let mut wrong = terminal.clone();
        wrong["selected_coded_packets"]["le_coded_s8"] = json!(3);
        assert!(verify_counts(&wrong, &summary, 2444).is_err());
        let mut wrong = summary.clone();
        wrong["packets_by_logical_channel"]["19"] = json!(2);
        assert!(verify_counts(&terminal, &wrong, 2444).is_err());
        let mut wrong = summary;
        wrong["invalid_crc_packets"] = json!(0);
        assert!(verify_counts(&terminal, &wrong, 2444).is_err());
    }
}
