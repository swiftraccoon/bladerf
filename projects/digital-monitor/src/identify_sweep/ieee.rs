//! Nominal hard-lane IEEE 802.15.4 replay of one admitted sweep channel.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::event_replay::replay::EvidenceFile;
use crate::input::SourceReport;
use crate::orchestrate::ieee_nominal::{ChannelEvidence, NominalDecoder};
use crate::orchestrate::{ieee_channelize_arguments, validate_sweep_channelize};
use crate::process::{self, ProgramProvenance};
use crate::report;

const LOG_BYTES: u64 = 1024 * 1024;
const MANIFEST_BYTES: u64 = 64 * 1024;
const CHANNELIZE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct Programs {
    pub(super) monitor: ProgramProvenance,
    pub(super) worker: ProgramProvenance,
}

#[derive(Serialize)]
pub(super) struct Evidence {
    pub(super) validated_frames: u64,
    decoder_lane: &'static str,
    carrier_offset_hz: i32,
    channelize_sha256: String,
    channel: ChannelEvidence,
}

/// Channelize the complete dwell and independently inspect the worker's FCS.
/// Keep the channelization manifest, raw candidates, validated PCAP and logs.
/// Derived CF32 staging is removed after validating the channel's evidence.
pub(super) fn run(
    programs: &Programs,
    source: &SourceReport,
    channel: u8,
    output: &Path,
    stop: &AtomicBool,
) -> Result<Evidence> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep IEEE 802.15.4 replay interrupted before launch"
    );
    report::private_directory(output)?;
    report::private_directory(&output.join("logs"))?;
    let scratch = tempfile::Builder::new()
        .prefix(".ieee-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(output)?;
    let channelized = scratch.path().join("channelized");
    let arguments =
        ieee_channelize_arguments(source, &channelized, source.complex_samples, &[channel]);
    let command = process::run(
        &programs.monitor,
        &arguments,
        output,
        "ieee-channelize",
        LOG_BYTES,
        CHANNELIZE_TIMEOUT,
        stop,
    )?;
    process::require_success(&command)?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "sweep IEEE 802.15.4 channelizer logs were truncated"
    );
    let manifest_path = channelized.join("channelize.json");
    let (manifest, manifest_evidence) =
        EvidenceFile::read_json(manifest_path.clone(), MANIFEST_BYTES)?;
    let channelization = validate_sweep_channelize(&manifest, source, &channelized, channel)?;
    let decoder = NominalDecoder {
        monitor: &programs.monitor,
        worker: &programs.worker,
        executor_threads: Some(1),
    };
    let evidence = decoder
        .decode_channel(channel, &channelization, output, stop)
        .with_context(|| format!("replay IEEE 802.15.4 channel {channel}"))?;
    channelization.verify_unchanged()?;
    manifest_evidence.verify()?;
    let retained_manifest = output.join("channelize.json");
    fs::hard_link(&manifest_path, &retained_manifest)
        .context("retain the verified sweep channelization manifest")?;
    scratch
        .close()
        .context("remove temporary sweep channel input")?;
    let (_, retained_digest) =
        report::read_json_with_sha256_limit(&retained_manifest, MANIFEST_BYTES)?;
    ensure!(
        retained_digest == manifest_evidence.sha256,
        "retained sweep channelization manifest differs from the verified digest"
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep IEEE 802.15.4 replay interrupted"
    );
    Ok(Evidence {
        validated_frames: evidence.validated_frames(),
        decoder_lane: "hard",
        carrier_offset_hz: 0,
        channelize_sha256: retained_digest,
        channel: evidence,
    })
}
