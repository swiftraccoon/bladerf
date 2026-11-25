//! Keep the first complete HE20 SU clip and its evidence outside the rolling prefix.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use nix::fcntl::{RenameFlags, renameat2};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{clips::ClipReport, direct::DirectReport, intake::ValidatedSegment};
use crate::report;

const DIRECTORY: &str = "first-he-su-clip";
const CLIP_RECEIPT: &str = "clips.json";
const HE_EVENTS: &str = "candidates.he-su-signal-events.json";
const MANIFEST: &str = "archive.json";
const MIB: u64 = 1024 * 1024;
const JSON_LIMIT: u64 = 64 * 1024;

fn copy_verified(source: &Path, destination: &Path, limit: u64, sha256: &str) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(source)?;
    let initial = file.metadata()?;
    ensure!(
        initial.is_file() && (1..=limit).contains(&initial.len()),
        "HE clip evidence is not a bounded regular file"
    );
    let mut bytes = Vec::new();
    (&file).take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == initial.len()
            && report::same_file_identity(&initial, &file.metadata()?)
            && format!("{:x}", Sha256::digest(&bytes)) == sha256,
        "HE clip archive source changed or failed its expected digest"
    );
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(())
}

/// Publish one bounded archive before retirement can remove its source evidence.
pub(super) fn publish(
    output: &Path,
    segment: &ValidatedSegment,
    clips: &ClipReport,
    direct: &DirectReport,
    files: (&Path, &Path),
    stop: &AtomicBool,
) -> Result<String> {
    let (iq, sidecar) = files;
    let clips_dir = iq.parent().context("HE clip has no receipt directory")?;
    ensure!(sidecar.parent() == Some(clips_dir), "HE clip paths differ");
    let iq_name = iq
        .file_name()
        .and_then(|name| name.to_str())
        .context("HE IQ name")?;
    let sidecar_name = sidecar
        .file_name()
        .and_then(|name| name.to_str())
        .context("HE sidecar name")?;
    let clip_receipt = clips_dir.join(CLIP_RECEIPT);
    let (receipt, digest) = report::read_json_with_sha256_limit(&clip_receipt, JSON_LIMIT)?;
    ensure!(
        digest == clips.receipt_sha256()
            && receipt["source_receipt_sha256"].as_str() == Some(segment.receipt_sha256.as_str()),
        "HE clip receipt differs from the verified segment"
    );
    let clip = receipt["clips"]
        .as_array()
        .and_then(|entries| {
            entries.iter().find(|clip| {
                clip["selection_role"].as_str() == Some("wifi_he_su_signal")
                    && clip["iq_file"].as_str() == Some(iq_name)
                    && clip["sidecar_file"].as_str() == Some(sidecar_name)
            })
        })
        .context("verified HE clip is missing from its receipt")?;
    let field = |key: &str| clip[key].as_str().context("HE clip digest missing");
    let he_events_sha256 = direct
        .wifi_he_su_signal_sha256()
        .context("HE event digest missing")?;
    ensure!(
        receipt["wifi_he_su_signal_report_sha256"].as_str() == Some(he_events_sha256)
            && receipt["direct_receipt_sha256"].as_str() == Some(direct.receipt_sha256()),
        "HE clip evidence differs from the verified decoder receipts"
    );
    let direct_dir = clips_dir
        .parent()
        .context("HE clip has no segment report directory")?
        .join("direct");
    let direct_receipt = direct_dir.join("direct.json");
    let he_events_file = direct_dir.join("wifi/decoded").join(HE_EVENTS);
    let staged = tempfile::Builder::new()
        .prefix(".he-su-clip-")
        .tempdir_in(output)?;
    for (source, name, limit, digest) in [
        (iq, iq_name, MIB, field("iq_sha256")?),
        (sidecar, sidecar_name, JSON_LIMIT, field("sidecar_sha256")?),
        (
            clip_receipt.as_path(),
            CLIP_RECEIPT,
            JSON_LIMIT,
            digest.as_str(),
        ),
        (
            direct_receipt.as_path(),
            "direct.json",
            JSON_LIMIT,
            direct.receipt_sha256(),
        ),
        (
            he_events_file.as_path(),
            HE_EVENTS,
            2 * MIB,
            he_events_sha256,
        ),
    ] {
        copy_verified(source, &staged.path().join(name), limit, digest)
            .with_context(|| format!("archive HE20 SU evidence {name}"))?;
    }
    report::publish_json(
        staged.path(),
        MANIFEST,
        &json!({
            "schema_version": 1,
            "operation": "rolling_he_su_clip_archive",
            "segment": segment.receipt.index + 1,
            "iq_file": iq_name,
            "sidecar_file": sidecar_name,
            "iq_sha256": field("iq_sha256")?,
            "sidecar_sha256": field("sidecar_sha256")?,
            "he_su_preamble_stream_sample": report::required_u64(clip, "he_su_preamble_stream_sample")?,
            "clip_receipt_sha256": clips.receipt_sha256(),
            "direct_receipt_sha256": direct.receipt_sha256(),
            "he_su_signal_events_sha256": he_events_sha256,
        }),
    )?;
    let digest =
        report::hash_direct_regular_file(&staged.path().join(MANIFEST), JSON_LIMIT)?.sha256;
    ensure!(!stop.load(Ordering::Relaxed), "HE clip archive interrupted");
    let directory = File::open(output)?;
    let name = staged
        .path()
        .file_name()
        .context("HE clip archive stage has no name")?;
    renameat2(
        &directory,
        Path::new(name),
        &directory,
        Path::new(DIRECTORY),
        RenameFlags::RENAME_NOREPLACE,
    )
    .context("publish first HE20 SU clip archive")?;
    let _ = staged.keep();
    directory.sync_all()?;
    Ok(digest)
}
