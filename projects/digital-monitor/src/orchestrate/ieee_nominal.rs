//! Bounded nominal hard-lane IEEE 802.15.4 inspection of one closed segment.

use std::ffi::OsString;
use std::fs::{self, File, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::event_replay::replay::EvidenceFile;
use crate::input::{NORMALIZED_SC16_Q11_FORMAT, SourceReport};

use super::{
    IEEE_CHANNELS, IEEE_HYPOTHESES, IEEE_MAX_FRAMES, INSPECTION_OUTPUT_MIB, IeeeChannelInput,
    IeeeHypothesisSpec, ProgramProvenance, path, process, report, require_file_digest,
    run_ieee_hypothesis, text, validate_channelize, validate_channelize_for,
};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const CHANNEL_DECIMATION: u64 = 5;
const FIRST_CHANNEL_OUTPUT_SAMPLE: u64 = CHANNEL_DECIMATION - 1;
const SOURCE_INDEX_HEADER_BYTES: u64 = 24;
const SOURCE_INDEX_RECORD_BYTES: u64 = 16;
const SOURCE_INDEX_MAX_BYTES: u64 =
    SOURCE_INDEX_HEADER_BYTES + SOURCE_INDEX_RECORD_BYTES * IEEE_MAX_FRAMES;
const LOG_BYTES: u64 = 1024 * 1024;
const JSON_BYTES: u64 = 64 * 1024;
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const RECEIPT_NAME: &str = "nominal.json";
const IEEE_CHANNELS_WITH_EDGE: [u8; 4] = [16, 17, 18, 19];

pub(crate) const MAX_BOUNDARY_PREFIX_SAMPLES: u64 = 409_600;

/// Connect a chronological decode input to the physical segment that owns frames.
pub(crate) struct NominalOwnership<'a> {
    pub(crate) input_start_sample: u64,
    pub(crate) segment_start_sample: u64,
    pub(crate) segment_source: &'a SourceReport,
    pub(crate) prefix_receipt_sha256: Option<&'a str>,
}

#[derive(Serialize)]
struct OwnershipReceipt<'a> {
    input_start_sample: u64,
    owned_start_sample: u64,
    owned_end_sample_exclusive: u64,
    physical_iq_sha256: &'a str,
    physical_sidecar_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix_receipt_sha256: Option<&'a str>,
}

impl<'a> NominalOwnership<'a> {
    fn for_input(self, input: &SourceReport) -> Result<OwnershipReceipt<'a>> {
        let prefix_samples = self
            .segment_start_sample
            .checked_sub(self.input_start_sample)
            .context("nominal IEEE decode input starts after its owned segment")?;
        let owned_end_sample_exclusive = self
            .segment_start_sample
            .checked_add(self.segment_source.complex_samples)
            .context("nominal IEEE owned segment end overflows")?;
        let physical = self.segment_source;
        ensure!(
            (1..=SAMPLE_RATE_HZ).contains(&physical.complex_samples)
                && prefix_samples <= MAX_BOUNDARY_PREFIX_SAMPLES
                && (prefix_samples == 0) == self.prefix_receipt_sha256.is_none()
                && prefix_samples.checked_add(physical.complex_samples)
                    == Some(input.complex_samples)
                && physical.bytes == physical.complex_samples * 4
                && physical.format == input.format
                && physical.sample_rate_hz == input.sample_rate_hz
                && physical.bandwidth_hz == input.bandwidth_hz
                && physical.center_frequency_hz == input.center_frequency_hz
                && (prefix_samples > 0 || physical == input),
            "nominal IEEE decode input and preceding context differ from the owned segment"
        );
        if let Some(digest) = self.prefix_receipt_sha256 {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "nominal IEEE preceding context lacks its SHA-256 receipt"
            );
        }
        Ok(OwnershipReceipt {
            input_start_sample: self.input_start_sample,
            owned_start_sample: self.segment_start_sample,
            owned_end_sample_exclusive,
            physical_iq_sha256: &physical.sha256,
            physical_sidecar_sha256: &physical.sidecar_sha256,
            prefix_receipt_sha256: self.prefix_receipt_sha256,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct ChannelOwnership {
    source_complex_samples: u64,
    owned_start_channel_sample: u64,
    owned_end_channel_sample: u64,
}

impl ChannelOwnership {
    fn verify_channelizer_geometry(manifest: &Value) -> Result<()> {
        let filter = &manifest["filter"];
        ensure!(
            report::required_u64(filter, "first_output_source_sample_index")?
                == FIRST_CHANNEL_OUTPUT_SAMPLE
                && filter["tail_flushed"].as_bool() == Some(false),
            "nominal IEEE ownership requires the expected decimation phase and an unflushed tail"
        );
        Ok(())
    }

    fn from_wideband(receipt: &OwnershipReceipt<'_>) -> Result<Self> {
        // Output n first exists after wideband input 5*n+4. Map a half-open
        // ownership interval using that required-through sample, without
        // subtracting the FIR's group delay.
        let to_channel_boundary = |sample: u64| {
            (sample - receipt.input_start_sample)
                .saturating_sub(FIRST_CHANNEL_OUTPUT_SAMPLE)
                .div_ceil(CHANNEL_DECIMATION)
        };
        let owned_start_channel_sample = to_channel_boundary(receipt.owned_start_sample);
        let owned_end_channel_sample = to_channel_boundary(receipt.owned_end_sample_exclusive);
        ensure!(
            owned_start_channel_sample < owned_end_channel_sample,
            "nominal IEEE owned segment has no channelized samples"
        );
        Ok(Self {
            source_complex_samples: owned_end_channel_sample,
            owned_start_channel_sample,
            owned_end_channel_sample,
        })
    }

    pub(super) fn index_for(
        self,
        archive: &Path,
        spec: IeeeHypothesisSpec,
        channel_input: &IeeeChannelInput,
    ) -> Result<SourceIndexPlan> {
        ensure!(
            spec == IEEE_HYPOTHESES[0]
                && channel_input.digest.bytes == self.source_complex_samples * 8,
            "nominal IEEE source index requires the complete hard-lane CF32 channel"
        );
        Ok(SourceIndexPlan {
            path: archive.with_extension("i154idx"),
            ownership: self,
        })
    }
}

pub(super) struct SourceIndexPlan {
    path: PathBuf,
    ownership: ChannelOwnership,
}

impl SourceIndexPlan {
    pub(super) fn add_worker_arguments(&self, arguments: &mut Vec<OsString>) {
        arguments.extend([text("--source-index-output"), path(&self.path)]);
    }

    pub(super) fn add_inspection_arguments(&self, arguments: &mut Vec<OsString>) {
        arguments.extend([
            text("--source-index"),
            path(&self.path),
            text("--source-complex-samples"),
            text(self.ownership.source_complex_samples),
            text("--owned-start-channel-sample"),
            text(self.ownership.owned_start_channel_sample),
            text("--owned-end-channel-sample"),
            text(self.ownership.owned_end_channel_sample),
        ]);
    }

    pub(super) fn digest(&self) -> Result<report::FileDigest> {
        report::hash_direct_regular_file(&self.path, SOURCE_INDEX_MAX_BYTES)
            .context("hash nominal IEEE 802.15.4 source index before inspection")
    }

    pub(super) fn verify_inspection(
        self,
        summary: &Value,
        before: &report::FileDigest,
    ) -> Result<SourceIndexEvidence> {
        let digest = require_file_digest(
            &self.path,
            SOURCE_INDEX_MAX_BYTES,
            before,
            "nominal IEEE 802.15.4 source index",
        )?;
        let index = &summary["source_index"];
        let owned = report::required_u64(summary, "candidate_record_count")?;
        let indexed = report::required_u64(index, "indexed_record_count")?;
        let outside = report::required_u64(index, "outside_record_count")?;
        let outside_valid = report::required_u64(index, "outside_valid_fcs_record_count")?;
        let outside_invalid = report::required_u64(index, "outside_invalid_fcs_record_count")?;
        let outside_short = report::required_u64(index, "outside_invalid_length_record_count")?;
        ensure!(
            index["file"].as_str().map(Path::new) == Some(self.path.as_path())
                && report::required_u64(index, "bytes")? == digest.bytes
                && index["sha256"].as_str() == Some(digest.sha256.as_str())
                && indexed <= IEEE_MAX_FRAMES
                && digest.bytes == SOURCE_INDEX_HEADER_BYTES + indexed * SOURCE_INDEX_RECORD_BYTES
                && outside.checked_add(owned) == Some(indexed)
                && outside_valid
                    .checked_add(outside_invalid)
                    .and_then(|count| count.checked_add(outside_short))
                    == Some(outside),
            "nominal IEEE inspection index identity, extent or index counts differ"
        );
        ensure!(
            report::required_u64(index, "source_complex_samples")?
                == self.ownership.source_complex_samples
                && report::required_u64(index, "owned_start_channel_sample")?
                    == self.ownership.owned_start_channel_sample
                && report::required_u64(index, "owned_end_channel_sample")?
                    == self.ownership.owned_end_channel_sample,
            "nominal IEEE inspection index ownership differs from the requested channel bounds"
        );
        let file = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .context("nominal IEEE 802.15.4 source index lacks a file name")?
            .to_owned();
        Ok(SourceIndexEvidence {
            file,
            retained: RetainedFile {
                path: self.path,
                bytes: digest.bytes,
                sha256: digest.sha256,
            },
            ownership: self.ownership,
            indexed_record_count: indexed,
            outside_record_count: outside,
            outside_valid_fcs_record_count: outside_valid,
            outside_invalid_fcs_record_count: outside_invalid,
            outside_invalid_length_record_count: outside_short,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SourceIndexEvidence {
    file: String,
    #[serde(flatten)]
    retained: RetainedFile,
    #[serde(flatten)]
    ownership: ChannelOwnership,
    pub(super) indexed_record_count: u64,
    outside_record_count: u64,
    outside_valid_fcs_record_count: u64,
    outside_invalid_fcs_record_count: u64,
    outside_invalid_length_record_count: u64,
}

pub(crate) struct NominalPlan {
    monitor: ProgramProvenance,
    worker: ProgramProvenance,
    center_hz: u64,
    include_channel_19: bool,
    executor_threads: Option<u8>,
}

pub(crate) fn prepare(
    monitor: &Path,
    worker: &Path,
    center_mhz: u32,
    include_channel_19: bool,
    executor_threads: Option<u8>,
) -> Result<NominalPlan> {
    ensure!(
        (2433..=2437).contains(&center_mhz),
        "nominal IEEE 802.15.4 decode requires a 2433..=2437 MHz center for channels 16 through 18"
    );
    ensure!(
        !include_channel_19 || center_mhz == 2437,
        "--decode-ieee802154-edge needs a 2437 MHz center for channel 19"
    );
    Ok(NominalPlan {
        monitor: process::resolve_program("ieee802154-monitor", monitor)?,
        worker: process::resolve_program("ieee802154-futuresdr-worker", worker)?,
        center_hz: u64::from(center_mhz) * 1_000_000,
        include_channel_19,
        executor_threads,
    })
}

#[derive(Serialize)]
pub(crate) struct NominalReport {
    #[serde(rename = "receipt_sha256")]
    receipt: EvidenceFile,
    #[serde(rename = "channelize_sha256")]
    channelize: EvidenceFile,
    channels: Vec<ChannelEvidence>,
}

impl NominalReport {
    pub(crate) fn validated_frames(&self) -> Result<u64> {
        self.channels.iter().try_fold(0u64, |total, channel| {
            total
                .checked_add(channel.valid_fcs_record_count)
                .context("nominal IEEE 802.15.4 frame count overflow")
        })
    }

    pub(crate) fn receipt_sha256(&self) -> &str {
        &self.receipt.sha256
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.receipt.verify()?;
        self.channelize.verify()?;
        self.channels.iter().try_for_each(ChannelEvidence::verify)
    }
}

#[derive(Clone, Debug, Serialize)]
struct RetainedFile {
    #[serde(skip)]
    path: PathBuf,
    bytes: u64,
    sha256: String,
}

impl RetainedFile {
    fn inspection_output(summary: &Value, key: &str, name: &str, directory: &Path) -> Result<Self> {
        let file = &summary[key];
        ensure!(
            file["file"].as_str() == Some(name),
            "unexpected IEEE 802.15.4 inspection file"
        );
        let bytes = report::required_u64(file, "bytes")?;
        ensure!(
            bytes <= INSPECTION_OUTPUT_MIB * 1024 * 1024,
            "nominal IEEE 802.15.4 inspection file exceeds its configured bound"
        );
        let sha256 = file["sha256"]
            .as_str()
            .context("IEEE 802.15.4 inspection file lacks a digest")?
            .to_owned();
        Ok(Self {
            path: directory.join(name),
            bytes,
            sha256,
        })
    }

    fn verify(&self) -> Result<()> {
        if self.bytes == 0 {
            let metadata = fs::symlink_metadata(&self.path)?;
            ensure!(
                metadata.is_file()
                    && metadata.len() == 0
                    && self.sha256 == format!("{:x}", Sha256::digest([])),
                "empty IEEE 802.15.4 evidence differs from its receipt"
            );
        } else {
            let digest = report::hash_direct_regular_file(&self.path, self.bytes)?;
            ensure!(
                digest.bytes == self.bytes && digest.sha256 == self.sha256,
                "retained IEEE 802.15.4 evidence differs from its receipt"
            );
        }
        Ok(())
    }
}

#[derive(Serialize)]
pub(crate) struct ChannelEvidence {
    channel: u8,
    channelized_sha256: String,
    candidate_record_count: u64,
    valid_fcs_record_count: u64,
    invalid_fcs_record_count: u64,
    invalid_length_record_count: u64,
    #[serde(rename = "inspection_sha256")]
    inspection: EvidenceFile,
    worker_stdout: RetainedFile,
    archive: RetainedFile,
    packets: RetainedFile,
    validated_pcap: RetainedFile,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_index: Option<SourceIndexEvidence>,
}

impl ChannelEvidence {
    pub(crate) fn validated_frames(&self) -> u64 {
        self.valid_fcs_record_count
    }

    fn verify(&self) -> Result<()> {
        self.inspection.verify()?;
        self.worker_stdout.verify()?;
        self.archive.verify()?;
        self.packets.verify()?;
        self.validated_pcap.verify()?;
        if let Some(index) = &self.source_index {
            index.retained.verify()?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    operation: &'static str,
    decoder_lane: &'static str,
    carrier_offset_hz: i32,
    timestamp_kind: &'static str,
    center_frequency_hz: u64,
    samples: u64,
    iq_sha256: &'a str,
    sidecar_sha256: &'a str,
    monitor_sha256: &'a str,
    worker_sha256: &'a str,
    channelize_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_nyquist_edge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executor_threads: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<OwnershipReceipt<'a>>,
    channels: &'a [ChannelEvidence],
}

impl NominalPlan {
    fn check_programs(&self) -> Result<()> {
        for program in [&self.monitor, &self.worker] {
            ensure!(
                process::resolve_program(program.name, &program.path)? == *program,
                "{} executable changed during nominal IEEE 802.15.4 decode",
                program.name
            );
        }
        Ok(())
    }
}

/// Demodulate and independently inspect one validated nominal hard-lane channel.
pub(crate) struct NominalDecoder<'a> {
    pub(crate) monitor: &'a ProgramProvenance,
    pub(crate) worker: &'a ProgramProvenance,
    pub(crate) executor_threads: Option<u8>,
}

impl NominalDecoder<'_> {
    pub(crate) fn decode_channel(
        &self,
        channel: u8,
        channelization: &super::IeeeChannelization,
        output: &Path,
        stop: &AtomicBool,
    ) -> Result<ChannelEvidence> {
        self.decode_with_ownership(channel, channelization, None, output, stop)
    }

    fn decode_with_ownership(
        &self,
        channel: u8,
        channelization: &super::IeeeChannelization,
        ownership: Option<ChannelOwnership>,
        output: &Path,
        stop: &AtomicBool,
    ) -> Result<ChannelEvidence> {
        let spec = IEEE_HYPOTHESES[0];
        let channel_input = channelization.channel(channel)?;
        let mut commands = Vec::new();
        let mut interrupted = false;
        let evidence = run_ieee_hypothesis(
            self.monitor,
            self.worker,
            spec,
            channel_input,
            channelization.worker_input_mib,
            self.executor_threads,
            ownership,
            output,
            output,
            LOG_BYTES,
            CHILD_TIMEOUT,
            stop,
            &mut commands,
            &mut interrupted,
        )?;
        ensure!(
            !interrupted
                && commands
                    .iter()
                    .all(|command| !command.stdout.truncated && !command.stderr.truncated),
            "nominal IEEE 802.15.4 command interrupted or its logs were truncated"
        );
        let directory = output.join(spec.inspection_name(channel));
        let worker_stdout = output.join(&commands[0].stdout.path);
        let completion = report::hash_direct_regular_file(&worker_stdout, 64 * 1024)?;
        let (summary, inspection) =
            EvidenceFile::read_json(directory.join("summary.json"), JSON_BYTES)?;
        ensure!(
            inspection.sha256 == evidence.inspection_sha256 && summary == evidence.inspection,
            "nominal IEEE 802.15.4 inspection summary changed"
        );
        let channel_evidence = ChannelEvidence {
            channel,
            channelized_sha256: channel_input.digest.sha256.clone(),
            candidate_record_count: report::required_u64(&summary, "candidate_record_count")?,
            valid_fcs_record_count: report::required_u64(&summary, "valid_fcs_record_count")?,
            invalid_fcs_record_count: report::required_u64(&summary, "invalid_fcs_record_count")?,
            invalid_length_record_count: report::required_u64(
                &summary,
                "invalid_length_record_count",
            )?,
            inspection,
            worker_stdout: RetainedFile {
                path: worker_stdout,
                bytes: completion.bytes,
                sha256: completion.sha256,
            },
            archive: RetainedFile {
                path: output.join(spec.archive_name(channel)),
                bytes: evidence.archive.bytes,
                sha256: evidence.archive.sha256,
            },
            packets: RetainedFile::inspection_output(
                &summary,
                "packets_jsonl",
                "packets.jsonl",
                &directory,
            )?,
            validated_pcap: RetainedFile::inspection_output(
                &summary,
                "validated_pcap",
                "validated.pcap",
                &directory,
            )?,
            source_index: evidence.source_index,
        };
        // The inspector writes one classic PCAP record per validated PSDU.
        let expected_pcap_bytes = 24
            + 16 * channel_evidence.valid_fcs_record_count
            + report::required_u64(&summary, "validated_psdu_bytes")?;
        ensure!(
            channel_evidence.validated_pcap.bytes == expected_pcap_bytes,
            "nominal IEEE validated PCAP extent differs from the validated frame count"
        );
        channel_evidence.verify()?;
        Ok(channel_evidence)
    }
}

impl NominalPlan {
    /// Channelize the complete segment and inspect one hard, zero-offset archive per channel.
    pub(crate) fn run(
        &self,
        source: &SourceReport,
        output: &Path,
        stop: &AtomicBool,
        verify_input: impl FnOnce() -> Result<()>,
    ) -> Result<NominalReport> {
        self.run_input(source, None, output, stop, verify_input)
    }

    /// Decode a segment with optional preceding context and count only
    /// completions whose last required input sample lies in the segment.
    pub(crate) fn run_with_ownership(
        &self,
        source: &SourceReport,
        ownership: NominalOwnership<'_>,
        output: &Path,
        stop: &AtomicBool,
        verify_input: impl FnOnce() -> Result<()>,
    ) -> Result<NominalReport> {
        self.run_input(source, Some(ownership), output, stop, verify_input)
    }

    fn run_input(
        &self,
        source: &SourceReport,
        ownership: Option<NominalOwnership<'_>>,
        output: &Path,
        stop: &AtomicBool,
        verify_input: impl FnOnce() -> Result<()>,
    ) -> Result<NominalReport> {
        let source_limit = if ownership.is_some() {
            SAMPLE_RATE_HZ + MAX_BOUNDARY_PREFIX_SAMPLES
        } else {
            SAMPLE_RATE_HZ
        };
        ensure!(
            source.format == NORMALIZED_SC16_Q11_FORMAT
                && source.center_frequency_hz == self.center_hz
                && source.sample_rate_hz == SAMPLE_RATE_HZ
                && source.bandwidth_hz == SAMPLE_RATE_HZ
                && (1..=source_limit).contains(&source.complex_samples)
                && source.complex_samples.is_multiple_of(CHANNEL_DECIMATION)
                && source.bytes == source.complex_samples * 4,
            "nominal IEEE 802.15.4 decode requires a complete 20 MSps SC16 Q11 segment"
        );
        let ownership = ownership.map(|value| value.for_input(source)).transpose()?;
        let channel_ownership = ownership
            .as_ref()
            .map(ChannelOwnership::from_wideband)
            .transpose()?;
        ensure!(
            !stop.load(Ordering::Relaxed),
            "nominal IEEE 802.15.4 decode interrupted"
        );
        self.check_programs()?;
        report::private_directory(output)?;
        report::private_directory(&output.join("logs"))?;
        let scratch = tempfile::Builder::new()
            .prefix(".ieee-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(output)?;
        let channelized = scratch.path().join("channelized");
        let requested_channels = if self.include_channel_19 {
            IEEE_CHANNELS_WITH_EDGE.as_slice()
        } else {
            IEEE_CHANNELS.as_slice()
        };
        let mut arguments = super::ieee_channelize_arguments(
            source,
            &channelized,
            source.complex_samples,
            requested_channels,
        );
        if self.include_channel_19 {
            arguments.push("--allow-nyquist-edge".into());
        }
        let command = process::run(
            &self.monitor,
            &arguments,
            output,
            "ieee-channelize",
            LOG_BYTES,
            CHILD_TIMEOUT,
            stop,
        )?;
        process::require_success(&command)?;
        ensure!(
            !command.stdout.truncated && !command.stderr.truncated,
            "nominal IEEE 802.15.4 channelizer logs were truncated"
        );
        let manifest_path = channelized.join("channelize.json");
        let (manifest, manifest_sha256) =
            report::read_json_with_sha256_limit(&manifest_path, JSON_BYTES)?;
        if channel_ownership.is_some() {
            ChannelOwnership::verify_channelizer_geometry(&manifest)?;
        }
        let channelization = if self.include_channel_19 {
            validate_channelize_for(
                &manifest,
                source,
                &channelized,
                IEEE_CHANNELS_WITH_EDGE,
                true,
            )?
        } else {
            validate_channelize(&manifest, source, &channelized)?
        };
        let decoder = NominalDecoder {
            monitor: &self.monitor,
            worker: &self.worker,
            executor_threads: self.executor_threads,
        };
        let channels = thread::scope(|scope| {
            let channelization = &channelization;
            let decoder = &decoder;
            let workers: Vec<_> = requested_channels
                .iter()
                .map(|&channel| {
                    thread::Builder::new()
                        .name(format!("rolling-ieee-channel-{channel}"))
                        .spawn_scoped(scope, move || {
                            decoder
                                .decode_with_ownership(
                                    channel,
                                    channelization,
                                    channel_ownership,
                                    output,
                                    stop,
                                )
                                .with_context(|| format!("nominal IEEE 802.15.4 channel {channel}"))
                        })
                })
                .collect();
            let outcomes: Vec<_> = workers
                .into_iter()
                .map(|worker| {
                    worker
                        .context("start nominal IEEE 802.15.4 channel worker")?
                        .join()
                        .map_err(|_| {
                            anyhow::anyhow!("nominal IEEE 802.15.4 channel worker panicked")
                        })?
                })
                .collect();
            outcomes.into_iter().collect::<Result<Vec<_>>>()
        })?;

        channelization.verify_unchanged()?;
        let retained_manifest = output.join("channelize.json");
        fs::hard_link(&manifest_path, &retained_manifest)
            .context("retain the validated IEEE 802.15.4 channelization manifest")?;
        scratch
            .close()
            .context("remove temporary IEEE 802.15.4 channel inputs")?;
        let (retained, channelize) = EvidenceFile::read_json(retained_manifest, JSON_BYTES)?;
        ensure!(
            channelize.sha256 == manifest_sha256 && retained == manifest,
            "retained IEEE 802.15.4 channelization changed during decode"
        );
        self.check_programs()?;
        verify_input()?;
        ensure!(
            !stop.load(Ordering::Relaxed),
            "nominal IEEE 802.15.4 decode interrupted"
        );
        let manifest = Receipt {
            schema_version: if ownership.is_some() { 2 } else { 1 },
            operation: "rolling_ieee802154_nominal",
            decoder_lane: "hard",
            carrier_offset_hz: 0,
            timestamp_kind: "relative_recovered_chip_completion",
            center_frequency_hz: source.center_frequency_hz,
            samples: source.complex_samples,
            iq_sha256: &source.sha256,
            sidecar_sha256: &source.sidecar_sha256,
            monitor_sha256: &self.monitor.sha256,
            worker_sha256: &self.worker.sha256,
            channelize_sha256: &channelize.sha256,
            allow_nyquist_edge: self.include_channel_19.then_some(true),
            executor_threads: self.executor_threads,
            ownership,
            channels: &channels,
        };
        report::publish_json(output, RECEIPT_NAME, &manifest)?;
        File::open(
            output
                .parent()
                .context("IEEE 802.15.4 output has no parent")?,
        )?
        .sync_all()?;
        let (published, receipt) = EvidenceFile::read_json(output.join(RECEIPT_NAME), JSON_BYTES)?;
        ensure!(
            published == serde_json::to_value(&manifest)?,
            "published nominal IEEE 802.15.4 receipt differs from verified evidence"
        );
        Ok(NominalReport {
            receipt,
            channelize,
            channels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_rejects_channelizers_with_different_phase_or_tail() {
        let manifest = |first_output, tail_flushed| {
            serde_json::json!({
                "filter": {
                    "first_output_source_sample_index": first_output,
                    "tail_flushed": tail_flushed,
                }
            })
        };
        assert!(ChannelOwnership::verify_channelizer_geometry(&manifest(4, false)).is_ok());
        assert!(ChannelOwnership::verify_channelizer_geometry(&manifest(0, false)).is_err());
        assert!(ChannelOwnership::verify_channelizer_geometry(&manifest(4, true)).is_err());
        assert!(ChannelOwnership::verify_channelizer_geometry(&Value::Null).is_err());
    }

    #[test]
    fn channel_bounds_follow_the_required_wideband_sample() -> Result<()> {
        let input_start = 71;
        let input_samples = 50;
        for prefix_samples in 0..25 {
            let receipt = OwnershipReceipt {
                input_start_sample: input_start,
                owned_start_sample: input_start + prefix_samples,
                owned_end_sample_exclusive: input_start + input_samples,
                physical_iq_sha256: "",
                physical_sidecar_sha256: "",
                prefix_receipt_sha256: None,
            };
            let ownership = ChannelOwnership::from_wideband(&receipt)?;
            assert_eq!(ownership.source_complex_samples, 10);
            assert_eq!(ownership.owned_end_channel_sample, 10);
            for index in 0..ownership.source_complex_samples {
                let required_wideband = input_start + 5 * index + 4;
                assert_eq!(
                    (ownership.owned_start_channel_sample..ownership.owned_end_channel_sample)
                        .contains(&index),
                    (receipt.owned_start_sample..receipt.owned_end_sample_exclusive)
                        .contains(&required_wideband)
                );
            }
        }
        Ok(())
    }

    fn source(samples: u64) -> SourceReport {
        SourceReport {
            input: "segment.sc16q11".into(),
            sidecar: "segment.sc16q11.json".into(),
            format: NORMALIZED_SC16_Q11_FORMAT,
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bandwidth_hz: SAMPLE_RATE_HZ,
            complex_samples: samples,
            bytes: samples * 4,
            sha256: format!("{samples:064x}"),
            sidecar_sha256: format!("{:064x}", samples + 1),
            continuity_guaranteed: true,
        }
    }

    #[test]
    fn owned_segment_must_match_decoded_extent_and_prefix_receipt() -> Result<()> {
        let physical = source(10);
        let staged = source(15);
        let digest = "a".repeat(64);
        let ownership = |input_start_sample, prefix_receipt_sha256| NominalOwnership {
            input_start_sample,
            segment_start_sample: 100,
            segment_source: &physical,
            prefix_receipt_sha256,
        };
        let receipt = ownership(95, Some(&digest)).for_input(&staged)?;
        assert_eq!(receipt.owned_start_sample, 100);
        assert_eq!(receipt.owned_end_sample_exclusive, 110);
        assert_eq!(receipt.physical_iq_sha256, physical.sha256);
        assert_eq!(receipt.physical_sidecar_sha256, physical.sidecar_sha256);
        assert_eq!(receipt.prefix_receipt_sha256, Some(digest.as_str()));
        assert!(ownership(100, None).for_input(&physical).is_ok());
        assert!(ownership(100, Some(&digest)).for_input(&physical).is_err());
        assert!(ownership(95, None).for_input(&staged).is_err());
        assert!(ownership(95, Some("invalid")).for_input(&staged).is_err());
        assert!(ownership(95, Some(&digest)).for_input(&physical).is_err());
        assert!(ownership(94, Some(&digest)).for_input(&staged).is_err());
        assert!(ownership(101, Some(&digest)).for_input(&staged).is_err());
        assert!(ownership(100, None).for_input(&source(15)).is_err());
        Ok(())
    }
}
