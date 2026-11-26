//! Terminal analysis manifest and bounded decoder-summary parsing.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::input::SourceReport;
use crate::process::{CommandReport, ProgramProvenance};

const MAX_SUMMARY_BYTES: u64 = 16 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileDigest {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StageState {
    Complete,
    Failed,
    Interrupted,
    Skipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    EvidenceFound,
    CandidatesWithoutValidFrames,
    NoValidCandidates,
    UnclassifiedActivity,
    NoUnclassifiedActivityDetected,
    CompletedZeroCandidates,
}

impl Outcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EvidenceFound => "evidence_found",
            Self::CandidatesWithoutValidFrames => "candidates_without_valid_frames",
            Self::NoValidCandidates => "no_valid_candidates",
            Self::UnclassifiedActivity => "unclassified_activity",
            Self::NoUnclassifiedActivityDetected => "no_unclassified_activity_detected",
            Self::CompletedZeroCandidates => "completed_zero_candidates",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct StageReport {
    pub(crate) name: &'static str,
    pub(crate) state: StageState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<Outcome>,
    pub(crate) commands: Vec<CommandReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

impl StageReport {
    pub(crate) fn skipped(name: &'static str, detail: &str) -> Self {
        Self {
            name,
            state: StageState::Skipped,
            outcome: None,
            commands: Vec::new(),
            detail: Some(detail.to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OverallState {
    Complete,
    Failed,
    Interrupted,
}

#[derive(Debug, Serialize)]
pub(crate) struct Settings {
    pub(crate) center_frequency_hz: u64,
    pub(crate) sample_rate_hz: u64,
    pub(crate) max_samples: u64,
    pub(crate) spectrum_windows: usize,
    pub(crate) decoder_timeout_seconds: u64,
    pub(crate) child_timeout_seconds: u64,
    pub(crate) retained_log_limit_bytes_per_stream: u64,
    pub(crate) ieee802154_channels: [u8; 3],
}

#[derive(Debug, Serialize)]
pub(crate) struct AnalysisReport {
    pub(crate) schema_version: u32,
    pub(crate) operation: &'static str,
    pub(crate) overall_state: OverallState,
    pub(crate) source: SourceReport,
    pub(crate) settings: Settings,
    pub(crate) programs: Vec<ProgramProvenance>,
    pub(crate) stages: Vec<StageReport>,
    pub(crate) interpretation: &'static str,
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    DirBuilder::new().mode(0o700).create(path).with_context(|| {
        format!(
            "create new private directory {} (existing paths are preserved)",
            path.display()
        )
    })
}

pub(crate) fn fresh_output(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("create private output parent {}", parent.display()))?;
    }
    private_directory(path)?;
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve output directory {}", path.display()))?;
    ensure!(
        path.to_str().is_some(),
        "output path must be valid UTF-8 for the JSON manifest"
    );
    Ok(path)
}

pub(crate) fn read_json(path: &Path) -> Result<Value> {
    read_json_with_sha256(path).map(|(value, _)| value)
}

/// Read one bounded stable JSON summary and identify the exact parsed bytes.
pub(crate) fn read_json_with_sha256(path: &Path) -> Result<(Value, String)> {
    read_json_with_sha256_limit(path, MAX_SUMMARY_BYTES)
}

/// Read one stable JSON file within a caller-selected positive size bound.
pub(crate) fn read_json_with_sha256_limit(
    path: &Path,
    maximum_bytes: u64,
) -> Result<(Value, String)> {
    read_json_with_snapshot(path, maximum_bytes).map(|(value, digest, _)| (value, digest))
}

/// Read bounded JSON and return the digest and metadata of the verified file.
pub(crate) fn read_json_with_snapshot(
    path: &Path,
    maximum_bytes: u64,
) -> Result<(Value, String, fs::Metadata)> {
    read_typed_json_with_snapshot(path, maximum_bytes)
}

/// Read a bounded structured receipt while retaining its bytes' identity.
pub(crate) fn read_typed_json_with_snapshot<T: DeserializeOwned>(
    path: &Path,
    maximum_bytes: u64,
) -> Result<(T, String, fs::Metadata)> {
    ensure!(maximum_bytes > 0, "JSON size limit must be positive");
    let read_limit = maximum_bytes
        .checked_add(1)
        .context("JSON size limit overflows read bound")?;
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect terminal summary {}", path.display()))?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "terminal summary must be a direct regular file"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open terminal summary {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        same_file_identity(&metadata, &before),
        "terminal summary identity changed while opening"
    );
    ensure!(
        (1..=maximum_bytes).contains(&metadata.len()),
        "terminal summary size is outside 1..={maximum_bytes} bytes"
    );
    let mut bytes = Vec::new();
    (&mut file).take(read_limit).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum_bytes,
        "terminal summary exceeds {maximum_bytes} bytes"
    );
    let after = file.metadata()?;
    let path_after = fs::symlink_metadata(path)?;
    ensure!(
        same_file_identity(&metadata, &after)
            && path_after.is_file()
            && !path_after.file_type().is_symlink()
            && same_file_identity(&metadata, &path_after),
        "terminal summary changed while reading"
    );
    let digest = HEXLOWER.encode(digest::digest(&SHA256, &bytes).as_ref());
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse terminal summary {}", path.display()))?;
    Ok((value, digest, metadata))
}

/// Hash one bounded direct regular file while pinning its path and open-file identity.
pub(crate) fn hash_direct_regular_file(path: &Path, maximum: u64) -> Result<FileDigest> {
    hash_direct_regular_file_with_snapshot(path, maximum).map(|(digest, _)| digest)
}

/// Hash a bounded direct regular file and return metadata from the verified descriptor.
pub(crate) fn hash_direct_regular_file_with_snapshot(
    path: &Path,
    maximum: u64,
) -> Result<(FileDigest, fs::Metadata)> {
    ensure!(maximum > 0, "file hash limit must be positive");
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect derived file {}", path.display()))?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "derived file must be a direct regular file"
    );
    ensure!(
        (1..=maximum).contains(&before.len()),
        "derived file size is outside 1..={maximum} bytes"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open derived file {}", path.display()))?;
    let opened = file.metadata()?;
    ensure!(
        same_file_identity(&before, &opened),
        "derived file changed while opening"
    );

    let mut digest = digest::Context::new(&SHA256);
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    let mut remaining = opened.len();
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut trailing = [0u8; 1];
    ensure!(
        file.read(&mut trailing)? == 0,
        "derived file grew while hashing"
    );

    let after_file = file.metadata()?;
    let after_path = fs::symlink_metadata(path)?;
    ensure!(
        same_file_identity(&opened, &after_file)
            && after_path.is_file()
            && !after_path.file_type().is_symlink()
            && same_file_identity(&opened, &after_path),
        "derived file changed while hashing"
    );
    let digest = FileDigest {
        bytes: opened.len(),
        sha256: HEXLOWER.encode(digest.finish().as_ref()),
    };
    Ok((digest, opened))
}

/// Compare inode identity, extent and modification/change timestamps.
pub(crate) fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

pub(crate) fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("terminal summary lacks unsigned integer {field}"))
}

pub(crate) fn required_array_len(value: &Value, pointer: &str) -> Result<usize> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::len)
        .with_context(|| format!("terminal summary lacks array {pointer}"))
}

pub(crate) fn publish(output: &Path, report: &AnalysisReport) -> Result<()> {
    publish_json(output, "analysis.json", report)
}

pub(crate) fn publish_json(output: &Path, name: &str, report: &impl Serialize) -> Result<()> {
    let partial = output.join(format!("{name}.partial"));
    let final_path = output.join(name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    serde_json::to_writer_pretty(&mut file, report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::hard_link(&partial, &final_path).with_context(|| {
        format!(
            "publish {} without replacing an existing file",
            final_path.display()
        )
    })?;
    fs::remove_file(&partial)?;
    File::open(output)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn output_and_terminal_report_are_private_and_no_replace() {
        let parent = tempfile::tempdir().unwrap();
        let output = fresh_output(&parent.path().join("new/output")).unwrap();
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let report = AnalysisReport {
            schema_version: 2,
            operation: "offline_multi_protocol_analysis",
            overall_state: OverallState::Complete,
            source: SourceReport {
                input: PathBuf::from("input"),
                sidecar: PathBuf::from("input.json"),
                format: "SC16_Q11_LE",
                center_frequency_hz: 2_437_000_000,
                sample_rate_hz: 20_000_000,
                bandwidth_hz: 20_000_000,
                complex_samples: 8192,
                bytes: 32768,
                sha256: "0".repeat(64),
                sidecar_sha256: "1".repeat(64),
                continuity_guaranteed: false,
            },
            settings: Settings {
                center_frequency_hz: 2_437_000_000,
                sample_rate_hz: 20_000_000,
                max_samples: 20_000_000,
                spectrum_windows: 128,
                decoder_timeout_seconds: 120,
                child_timeout_seconds: 180,
                retained_log_limit_bytes_per_stream: 1_048_576,
                ieee802154_channels: [16, 17, 18],
            },
            programs: Vec::new(),
            stages: Vec::new(),
            interpretation: "test",
        };
        publish(&output, &report).unwrap();
        assert_eq!(
            fs::metadata(output.join("analysis.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(publish(&output, &report).is_err());
    }

    #[test]
    fn hashed_json_reader_rejects_a_symlinked_summary() {
        let directory = tempfile::tempdir().unwrap();
        let direct = directory.path().join("direct.json");
        fs::write(&direct, b"{\"count\":1}").unwrap();
        let (value, digest) = read_json_with_sha256(&direct).unwrap();
        assert_eq!(value["count"], 1);
        assert_eq!(digest.len(), 64);

        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&direct, &link).unwrap();
        assert!(read_json_with_sha256(&link).is_err());
    }

    #[test]
    fn bounded_json_reader_rejects_invalid_limits_before_opening() {
        let missing = Path::new("missing-limit-test.json");
        let zero = read_json_with_sha256_limit(missing, 0).unwrap_err();
        assert!(format!("{zero:#}").contains("must be positive"));

        let maximum = read_json_with_sha256_limit(missing, u64::MAX).unwrap_err();
        assert!(format!("{maximum:#}").contains("overflows read bound"));
    }

    #[test]
    fn direct_file_hash_binds_length_digest_and_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let direct = directory.path().join("archive.bdb");
        fs::write(&direct, b"abc").unwrap();
        let digest = hash_direct_regular_file(&direct, 3).unwrap();
        assert_eq!(digest.bytes, 3);
        assert_eq!(
            digest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(hash_direct_regular_file(&direct, 2).is_err());

        let link = directory.path().join("archive-link.bdb");
        std::os::unix::fs::symlink(&direct, &link).unwrap();
        assert!(hash_direct_regular_file(&link, 3).is_err());
    }
}
