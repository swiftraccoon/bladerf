//! Publish one witnessed rolling RX segment as a directory.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use num_complex::Complex;
use ring::digest::{self, SHA256};
use serde::Deserialize;
use serde_json::json;

use super::segment_stream::{SAMPLE_PERIOD_NS, SegmentTiming};
use super::{CHUNK_SAMPLES, CaptureFiles, CaptureMetadata, encode_iq};

const MAX_RETIRED_CHECKPOINT_BYTES: u64 = 64 * 1024;

pub(super) struct Segment {
    pub(super) index: usize,
    pub(super) timing: SegmentTiming,
    pub(super) finished_on_request: bool,
    pub(super) samples: Vec<Complex<i16>>,
}

pub(super) fn publish_segment(
    output: &Path,
    settings: &CaptureMetadata,
    segment: &Segment,
    spool_segments: Option<usize>,
    retire_prefix: bool,
) -> Result<()> {
    let ordinal = segment
        .index
        .checked_add(1)
        .context("segment ordinal overflow")?;
    let num_samples = segment.samples.len();
    let start_sample = segment
        .index
        .checked_mul(num_samples)
        .context("segment sample offset overflow")?;
    let duration_ns = i64::try_from(num_samples)?
        .checked_mul(SAMPLE_PERIOD_NS)
        .context("segment timestamp duration overflow")?;
    ensure!(
        settings.sample_rate_hz == 20_000_000.0
            && num_samples > 0
            && num_samples == settings.num_samples
            && segment.timing.first_sample_ns.checked_add(duration_ns)
                == Some(segment.timing.end_sample_exclusive_ns),
        "rolling segment extent differs from its 20 MSps timestamps"
    );
    if let Some(limit) = spool_segments {
        if retire_prefix {
            require_retiring_spool_slot(output, segment.index, limit)?;
        } else {
            require_spool_slot(output, segment.index, limit)?;
        }
    }
    let mut metadata = settings.clone();
    if segment.index != 0 {
        metadata.discarded_initial_samples = None;
    }

    let parent = File::open(output).context("open rolling output directory")?;
    let stage = tempfile::Builder::new()
        .prefix(".segment-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(output)
        .context("create private segment staging directory")?;
    let mut capture = CaptureFiles::create(&stage.path().join("iq.sc16q11"))?;
    let mut iq_digest = digest::Context::new(&SHA256);
    let mut encoded = Vec::with_capacity(CHUNK_SAMPLES * 4);
    for samples in segment.samples.chunks(CHUNK_SAMPLES) {
        encode_iq(samples, &mut encoded)?;
        capture
            .raw
            .write_all(&encoded)
            .context("write segment IQ")?;
        iq_digest.update(&encoded);
    }
    capture.complete(&metadata)?;
    let sidecar = fs::read(stage.path().join("iq.sc16q11.json")).context("read segment sidecar")?;
    let mut receipt = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage.path().join("segment.json"))
        .context("create segment receipt")?;
    let mut receipt_body = json!({
        "schema_version": 1,
        "index": segment.index,
        "start_sample": start_sample,
        "num_samples": num_samples,
        "first_sample_ns": segment.timing.first_sample_ns,
        "end_sample_exclusive_ns": segment.timing.end_sample_exclusive_ns,
        "witness_start_ns": segment.timing.end_sample_exclusive_ns,
        "iq_sha256": HEXLOWER.encode(iq_digest.finish().as_ref()),
        "sidecar_sha256": HEXLOWER.encode(digest::digest(&SHA256, &sidecar).as_ref()),
    });
    if segment.finished_on_request {
        receipt_body["finished_on_request"] = json!(true);
    }
    serde_json::to_writer_pretty(&mut receipt, &receipt_body)?;
    receipt.write_all(b"\n")?;
    receipt.sync_all().context("sync segment receipt")?;
    File::open(stage.path())?
        .sync_all()
        .context("sync staged segment directory")?;

    let stage_name = stage
        .path()
        .file_name()
        .context("segment stage needs a name")?;
    let destination = format!("{ordinal:06}");
    publish_directory(&parent, Path::new(stage_name), Path::new(&destination))
        .with_context(|| format!("publish segment {destination} without replacement"))?;
    let _ = stage.keep();
    parent
        .sync_all()
        .context("segment was published, but syncing the output directory failed")
}

fn require_spool_slot(output: &Path, published: usize, limit: usize) -> Result<()> {
    let mut occupied = 0;
    let mut directories = 0;
    for entry in fs::read_dir(output).context("scan segment IQ spool")? {
        directories += 1;
        ensure!(
            directories <= published,
            "unexpected entry in segment IQ spool"
        );
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("segment IQ spool name must be UTF-8")?;
        let ordinal: usize = name.parse().context("invalid segment IQ spool ordinal")?;
        ensure!(
            (1..=published).contains(&ordinal)
                && name == format!("{ordinal:06}")
                && entry.file_type()?.is_dir(),
            "segment IQ spool requires direct numbered directories"
        );
        occupied += usize::from(retained_iq(&entry.path())?);
    }
    ensure!(
        directories == published,
        "segment IQ spool lost a published directory"
    );
    ensure!(occupied < limit, "segment IQ spool is full");
    Ok(())
}

#[derive(Deserialize)]
struct RetiredPrefix {
    schema_version: u8,
    operation: String,
    retired_segments: usize,
}

fn retired_prefix(output: &Path, published: usize) -> Result<usize> {
    let checkpoint = output
        .parent()
        .context("segment IQ spool has no checkpoint directory")?
        .join("retired.json");
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(&checkpoint)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("open segment retirement checkpoint"),
    };
    ensure!(
        file.metadata()?.is_file(),
        "segment retirement checkpoint must be a direct regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_RETIRED_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read segment retirement checkpoint")?;
    ensure!(
        bytes.len() as u64 <= MAX_RETIRED_CHECKPOINT_BYTES,
        "segment retirement checkpoint exceeds its size bound"
    );
    let checkpoint: RetiredPrefix =
        serde_json::from_slice(&bytes).context("parse segment retirement checkpoint")?;
    ensure!(
        checkpoint.schema_version == 1
            && checkpoint.operation == "rolling_rx1_retired_prefix"
            && checkpoint.retired_segments <= published,
        "invalid segment retirement checkpoint"
    );
    Ok(checkpoint.retired_segments)
}

fn require_retiring_spool_slot(output: &Path, published: usize, limit: usize) -> Result<()> {
    let mut occupied = 0;
    let mut ordinals = BTreeSet::new();
    for entry in fs::read_dir(output).context("scan segment IQ spool")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .context("segment IQ spool name must be UTF-8")?;
        let ordinal: usize = name.parse().context("invalid segment IQ spool ordinal")?;
        ensure!(
            (1..=published).contains(&ordinal) && name == format!("{ordinal:06}"),
            "segment IQ spool requires direct numbered directories"
        );
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("inspect segment IQ spool directory"),
        };
        ensure!(
            kind.is_dir(),
            "segment IQ spool requires direct numbered directories"
        );
        let directory = entry.path();
        let retained = match retained_iq(&directory) {
            Ok(retained) => retained,
            Err(_)
                if fs::symlink_metadata(&directory)
                    .is_err_and(|error| error.kind() == ErrorKind::NotFound) =>
            {
                // Retirement can rename the directory out from under this scan.
                continue;
            }
            Err(error) => return Err(error),
        };
        ordinals.insert(ordinal);
        occupied += usize::from(retained);
    }
    // A removal observed above must be covered by a checkpoint read afterwards.
    let retired = retired_prefix(output, published)?;
    ensure!(
        ordinals
            .range(retired + 1..)
            .copied()
            .eq(retired + 1..=published),
        "segment IQ spool lost a published directory outside the retired prefix"
    );
    ensure!(occupied < limit, "segment IQ spool is full");
    Ok(())
}

fn retained_iq(directory: &Path) -> Result<bool> {
    let mut metadata_files = 0;
    for file in fs::read_dir(directory)? {
        let file = file?;
        match file.file_name().as_encoded_bytes() {
            b"iq.sc16q11" => {}
            b"segment.json" | b"iq.sc16q11.json" => {
                ensure!(
                    file.file_type()?.is_file(),
                    "segment IQ spool metadata must be a direct regular file"
                );
                metadata_files += 1;
            }
            _ => anyhow::bail!("unexpected file in a published segment IQ spool directory"),
        }
    }
    ensure!(
        metadata_files == 2,
        "segment IQ spool lost published metadata"
    );
    // The cooperating consumer can only remove payloads during this scan.
    // Treat a concurrent removal as a freed slot and reject indirect IQ paths.
    match fs::symlink_metadata(directory.join("iq.sc16q11")) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "segment IQ spool payload must be a direct regular file"
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).context("inspect segment IQ spool payload"),
    }
}

fn publish_directory(parent: &File, stage: &Path, destination: &Path) -> Result<()> {
    use nix::fcntl::{RenameFlags, renameat2};

    renameat2(
        parent,
        stage,
        parent,
        destination,
        RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn captured_directory(output: &Path, ordinal: usize, iq: bool) -> PathBuf {
        let directory = output.join(format!("{ordinal:06}"));
        fs::create_dir(&directory).unwrap();
        for name in ["iq.sc16q11.json", "segment.json"] {
            fs::write(directory.join(name), b"evidence").unwrap();
        }
        if iq {
            fs::write(directory.join("iq.sc16q11"), b"samples").unwrap();
        }
        directory
    }

    fn write_checkpoint(root: &Path, count: usize) {
        fs::write(
            root.join("retired.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "operation": "rolling_rx1_retired_prefix",
                "retired_segments": count,
                "extra": "allowed",
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn needs_checkpoint_for_disappeared_prefix_but_requires_all_later_directories() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("capture");
        fs::create_dir(&output).unwrap();
        require_retiring_spool_slot(&output, 0, 1).unwrap();
        captured_directory(&output, 3, false);
        let latest = captured_directory(&output, 4, false);
        assert!(require_retiring_spool_slot(&output, 4, 1).is_err());
        write_checkpoint(root.path(), 2);
        require_retiring_spool_slot(&output, 4, 1).unwrap();
        fs::rename(latest, root.path().join("unproven-removal")).unwrap();
        let lost = require_retiring_spool_slot(&output, 4, 1).unwrap_err();
        assert!(lost.to_string().contains("outside the retired prefix"));
    }

    #[test]
    fn retirement_still_counts_extant_iq_and_rejects_indirect_directories() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("capture");
        fs::create_dir(&output).unwrap();
        let first = captured_directory(&output, 1, true);
        captured_directory(&output, 2, false);
        write_checkpoint(root.path(), 1);
        let full = require_retiring_spool_slot(&output, 2, 1).unwrap_err();
        assert_eq!(full.to_string(), "segment IQ spool is full");
        fs::remove_file(first.join("iq.sc16q11")).unwrap();
        require_retiring_spool_slot(&output, 2, 1).unwrap();
        std::os::unix::fs::symlink(first, output.join("000003")).unwrap();
        assert!(require_retiring_spool_slot(&output, 3, 1).is_err());
    }

    #[test]
    fn rejects_retirement_checkpoints_ahead_of_publication_or_indirect() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("capture");
        fs::create_dir(&output).unwrap();
        write_checkpoint(root.path(), 1);
        assert!(require_retiring_spool_slot(&output, 0, 1).is_err());
        let checkpoint = root.path().join("retired.json");
        let target = root.path().join("checkpoint");
        fs::rename(&checkpoint, &target).unwrap();
        std::os::unix::fs::symlink(target, checkpoint).unwrap();
        assert!(require_retiring_spool_slot(&output, 1, 1).is_err());
    }

    #[test]
    fn consumer_can_free_a_slot_without_removing_metadata() {
        let root = tempfile::tempdir().unwrap();
        require_spool_slot(root.path(), 0, 1).unwrap();
        let first = root.path().join("000001");
        fs::create_dir(&first).unwrap();
        for name in ["iq.sc16q11", "iq.sc16q11.json", "segment.json"] {
            fs::write(first.join(name), b"evidence").unwrap();
        }
        let error = require_spool_slot(root.path(), 1, 1).unwrap_err();
        assert_eq!(error.to_string(), "segment IQ spool is full");

        fs::remove_file(first.join("iq.sc16q11")).unwrap();
        require_spool_slot(root.path(), 1, 1).unwrap();
        assert!(first.join("segment.json").is_file());
        assert!(first.join("iq.sc16q11.json").is_file());

        fs::write(first.join("unexpected"), b"extra").unwrap();
        assert!(require_spool_slot(root.path(), 1, 1).is_err());
    }

    #[test]
    fn rejects_an_existing_empty_segment_directory() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("stage")).unwrap();
        fs::create_dir(directory.path().join("000001")).unwrap();
        let parent = File::open(directory.path()).unwrap();
        assert!(publish_directory(&parent, Path::new("stage"), Path::new("000001")).is_err());
        assert!(directory.path().join("stage").is_dir());
        assert!(directory.path().join("000001").is_dir());
    }
}
