//! Preserve verified segment tails and stage bounded overlapping decoder inputs.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{input::SourceReport, report};

use super::intake::{IQ_NAME, SEGMENT_SAMPLES, ValidatedSegment};

pub(super) const CONTEXT_SAMPLES: u64 = 204_800;
pub(super) const PREFIX_SAMPLES: u64 = 2 * CONTEXT_SAMPLES;
const SAMPLE_PERIOD_NS: i64 = 50;
const SAMPLE_BYTES: u64 = 4;
const PREFIX_BYTES: usize = (PREFIX_SAMPLES * SAMPLE_BYTES) as usize;
const COPY_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
const TAIL_NAME: &str = "overlap-tail.sc16q11";
const PROOF_NAME: &str = "overlap-tail.json";
const PROOF_LIMIT_BYTES: u64 = 8192;

#[derive(Clone, Serialize)]
pub(super) struct TailProof {
    schema_version: u32,
    operation: &'static str,
    pub(super) source_index: usize,
    source_iq_sha256: String,
    source_receipt_sha256: String,
    pub(super) start_sample: u64,
    pub(super) samples: u64,
    pub(super) first_sample_ns: i64,
    pub(super) end_sample_exclusive_ns: i64,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    bandwidth_hz: u64,
    tail_sha256: String,
    #[serde(skip)]
    receipt_sha256: String,
}

impl TailProof {
    pub(super) fn receipt_sha256(&self) -> &str {
        &self.receipt_sha256
    }

    /// Verify the persisted bytes and receipt before or after original IQ release.
    pub(super) fn verify(&self, output_dir: &Path) -> Result<()> {
        self.verify_receipt(output_dir)?;
        let digest =
            report::hash_direct_regular_file(&output_dir.join(TAIL_NAME), PREFIX_BYTES as u64)?;
        ensure!(
            digest.bytes == PREFIX_BYTES as u64 && digest.sha256 == self.tail_sha256,
            "persisted overlap tail differs from its verified extent or digest"
        );
        Ok(())
    }

    fn verify_receipt(&self, output_dir: &Path) -> Result<()> {
        let (published, digest) =
            report::read_json_with_sha256_limit(&output_dir.join(PROOF_NAME), PROOF_LIMIT_BYTES)?;
        ensure!(
            digest == self.receipt_sha256 && published == serde_json::to_value(self)?,
            "persisted overlap tail receipt changed"
        );
        Ok(())
    }
}

pub(super) struct Tail {
    proof: TailProof,
    output_dir: PathBuf,
}

impl Tail {
    pub(super) fn proof(&self) -> &TailProof {
        &self.proof
    }
}

pub(super) struct StagedInput {
    pub(super) source: SourceReport,
    stamps: [Metadata; 2],
    scratch: tempfile::TempDir,
}

impl StagedInput {
    /// Check that both scratch files are unchanged after decoding.
    pub(super) fn verify(&self) -> Result<()> {
        for (path, initial) in [&self.source.input, &self.source.sidecar]
            .into_iter()
            .zip(&self.stamps)
        {
            let current = fs::symlink_metadata(path)?;
            ensure!(
                current.is_file() && report::same_file_identity(initial, &current),
                "staged overlap input changed during decode"
            );
        }
        Ok(())
    }

    /// Remove scratch input on success and report cleanup failure.
    pub(super) fn close(self) -> Result<()> {
        self.scratch.close().context("remove staged overlap input")
    }
}

pub(super) fn open_admitted_iq(segment: &ValidatedSegment) -> Result<File> {
    ensure!(
        segment.receipt.num_samples == SEGMENT_SAMPLES
            && segment.source.complex_samples == SEGMENT_SAMPLES
            && segment.source.bytes == SEGMENT_SAMPLES * SAMPLE_BYTES,
        "overlap requires one complete rolling segment"
    );
    let (directory, stamp) = segment.open_for_release()?;
    let file = File::from(openat(
        &directory,
        IQ_NAME,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let opened = file.metadata()?;
    ensure!(
        opened.is_file()
            && opened.len() == segment.source.bytes
            && report::same_file_identity(&opened, stamp),
        "rolling IQ changed before copying overlap samples"
    );
    Ok(file)
}

fn open_persisted_tail(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() == PREFIX_BYTES as u64,
        "persisted overlap tail has an invalid extent"
    );
    Ok(file)
}

/// Persist an exact predecessor prefix before its original IQ can be released.
pub(super) fn read_tail(segment: &ValidatedSegment, output_dir: &Path) -> Result<Tail> {
    let offset = SEGMENT_SAMPLES - PREFIX_SAMPLES;
    let mut original = open_admitted_iq(segment)?;
    original.seek(SeekFrom::Start(offset * SAMPLE_BYTES))?;
    let mut bytes = vec![0u8; PREFIX_BYTES];
    original
        .read_exact(&mut bytes)
        .context("read the complete admitted overlap tail")?;
    segment.verify_unchanged()?;
    let mut proof = TailProof {
        schema_version: 1,
        operation: "rolling_overlap_tail",
        source_index: segment.receipt.index,
        source_iq_sha256: segment.source.sha256.clone(),
        source_receipt_sha256: segment.receipt_sha256.clone(),
        start_sample: segment
            .receipt
            .start_sample
            .checked_add(offset)
            .context("overlap tail start sample overflow")?,
        samples: PREFIX_SAMPLES,
        first_sample_ns: segment
            .receipt
            .end_sample_exclusive_ns
            .checked_sub(PREFIX_SAMPLES as i64 * SAMPLE_PERIOD_NS)
            .context("overlap tail start clock overflow")?,
        end_sample_exclusive_ns: segment.receipt.end_sample_exclusive_ns,
        center_frequency_hz: segment.source.center_frequency_hz,
        sample_rate_hz: segment.source.sample_rate_hz,
        bandwidth_hz: segment.source.bandwidth_hz,
        tail_sha256: format!("{:x}", Sha256::digest(&bytes)),
        receipt_sha256: String::new(),
    };

    let mut temporary = tempfile::NamedTempFile::new_in(output_dir)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(output_dir.join(TAIL_NAME))
        .context("publish non-replacing overlap tail IQ")?;
    report::publish_json(output_dir, PROOF_NAME, &proof)?;
    let parent = output_dir
        .parent()
        .context("overlap report has no parent")?;
    File::open(parent)?.sync_all()?;
    let (published, digest) =
        report::read_json_with_sha256_limit(&output_dir.join(PROOF_NAME), PROOF_LIMIT_BYTES)?;
    ensure!(
        published == serde_json::to_value(&proof)?,
        "published overlap tail receipt differs from verified evidence"
    );
    proof.receipt_sha256 = digest;
    Ok(Tail {
        proof,
        output_dir: output_dir.to_path_buf(),
    })
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "rolling overlap staging interrupted"
    );
    Ok(())
}

/// Join the verified predecessor tail and whole current segment in private scratch.
pub(super) fn stage(
    prefix: &Tail,
    segment: &ValidatedSegment,
    output_parent: &Path,
    stop: &AtomicBool,
) -> Result<StagedInput> {
    check_stop(stop)?;
    let proof = prefix.proof();
    ensure!(
        proof.source_index.checked_add(1) == Some(segment.receipt.index)
            && proof.start_sample.checked_add(proof.samples) == Some(segment.receipt.start_sample)
            && proof.samples == PREFIX_SAMPLES
            && proof.end_sample_exclusive_ns == segment.receipt.first_sample_ns
            && proof.center_frequency_hz == segment.source.center_frequency_hz
            && proof.sample_rate_hz == segment.source.sample_rate_hz
            && proof.bandwidth_hz == segment.source.bandwidth_hz,
        "overlap tail is not contiguous with the next admitted segment"
    );
    prefix.proof.verify_receipt(&prefix.output_dir)?;
    let mut bytes = vec![0u8; PREFIX_BYTES];
    let tail_path = prefix.output_dir.join(TAIL_NAME);
    let mut tail = open_persisted_tail(&tail_path)?;
    let initial_tail = tail.metadata()?;
    tail.read_exact(&mut bytes)?;
    let latest_tail = fs::symlink_metadata(&tail_path)?;
    ensure!(
        latest_tail.is_file()
            && report::same_file_identity(&initial_tail, &tail.metadata()?)
            && report::same_file_identity(&initial_tail, &latest_tail)
            && format!("{:x}", Sha256::digest(&bytes)) == proof.tail_sha256,
        "overlap prefix changed while reading"
    );
    let mut original = open_admitted_iq(segment)?;
    let scratch = tempfile::Builder::new()
        .prefix(".overlap-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(output_parent)
        .context("create private overlap scratch directory")?;
    let directory = scratch.path().canonicalize()?;
    let iq = directory.join(IQ_NAME);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&iq)?;
    output.write_all(&bytes)?;
    // File-to-file copy can share extents on supporting filesystems. The held
    // source descriptor and admitted stamps protect the original byte identity.
    let mut remaining = segment.source.bytes;
    while remaining != 0 {
        check_stop(stop)?;
        let len = remaining.min(COPY_CHUNK_BYTES);
        let copied = io::copy(&mut (&mut original).take(len), &mut output)?;
        ensure!(
            copied == len,
            "admitted overlap source ended before its verified extent"
        );
        remaining -= copied;
    }
    segment.verify_unchanged()?;
    check_stop(stop)?;
    let mut source = segment.source.clone();
    source.input = iq;
    source.sidecar = directory.join(format!("{IQ_NAME}.json"));
    source.complex_samples += PREFIX_SAMPLES;
    source.bytes += PREFIX_BYTES as u64;
    let (digest, iq_stamp) =
        report::hash_direct_regular_file_with_snapshot(&source.input, source.bytes)?;
    ensure!(
        digest.bytes == source.bytes && report::same_file_identity(&iq_stamp, &output.metadata()?),
        "staged overlap IQ differs from its copied extent or file identity"
    );
    check_stop(stop)?;
    source.sha256 = digest.sha256;
    let metadata = serde_json::to_vec(&serde_json::json!({
        "format": "SC16_Q11",
        "dtype": "<i2",
        "interleave": "I,Q",
        "complex_scale": 2048,
        "frequency_hz": source.center_frequency_hz,
        "sample_rate_hz": source.sample_rate_hz,
        "bandwidth_hz": source.bandwidth_hz,
        "num_samples": source.complex_samples,
    }))?;
    let mut sidecar = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&source.sidecar)?;
    sidecar.write_all(&metadata)?;
    source.sidecar_sha256 = format!("{:x}", Sha256::digest(&metadata));
    Ok(StagedInput {
        source,
        stamps: [iq_stamp, sidecar.metadata()?],
        scratch,
    })
}
