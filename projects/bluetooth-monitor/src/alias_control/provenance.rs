//! Stable parsing and content identities for alias-control inputs.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: u64 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestSidecar {
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestSource {
    pub(super) format: String,
    pub(super) sample_rate_hz: u64,
    pub(super) bandwidth_hz: u64,
    pub(super) processed_complex_samples: u64,
    pub(super) processed_prefix_bytes: u64,
    pub(super) processed_prefix_sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestFilter {
    pub(super) taps: usize,
    pub(super) decimation: usize,
    pub(super) group_delay_source_samples: usize,
    pub(super) first_output_source_sample_index: usize,
    pub(super) startup_transient_output_samples: usize,
    pub(super) tail_flushed: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestMapping {
    pub(super) output_stride_source_samples: usize,
    pub(super) latest_source_index_for_output_zero: usize,
    pub(super) group_delay_corrected_source_index_for_output_zero: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestTarget {
    pub(super) center_frequency_hz: u64,
    pub(super) output_file: String,
    pub(super) output_format: String,
    pub(super) output_sample_rate_hz: u64,
    pub(super) output_complex_samples: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct FrequencyManifest {
    pub(super) schema_version: u32,
    pub(super) operation: String,
    pub(super) acquisition_sidecar: ManifestSidecar,
    pub(super) source: ManifestSource,
    pub(super) filter: ManifestFilter,
    pub(super) sample_index_mapping: ManifestMapping,
    pub(super) target: ManifestTarget,
    pub(super) continuity_guaranteed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct DecoderProgram {
    pub(super) schema_version: u32,
    pub(super) name: String,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReplayManifest {
    pub(super) schema_version: u32,
    pub(super) processed_full_input: bool,
    pub(super) decoder_processed_complex_samples: Option<u64>,
    pub(super) channelizer_step_complex_samples: u64,
    pub(super) channelizer_steps_processed: u64,
    pub(super) decoder_program: DecoderProgram,
    pub(super) input_complex_samples: u64,
    pub(super) burst_archive_records: u64,
    pub(super) burst_archive_truncated_by_limit: bool,
    pub(super) radio_opened: bool,
    pub(super) network_requested: bool,
    pub(super) transmit_requested: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ContentIdentity {
    pub(super) format: &'static str,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

pub(super) fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn read_stable_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    label: &str,
    format: &'static str,
) -> Result<(T, ContentIdentity)> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    ensure!(
        before.file_type().is_file(),
        "{label} must be a direct regular file"
    );
    ensure!(
        before.len() <= MAX_MANIFEST_BYTES,
        "{label} exceeds {MAX_MANIFEST_BYTES} bytes"
    );
    let before_identity = FileIdentity::from_metadata(&before);
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.file_type().is_file() && FileIdentity::from_metadata(&opened) == before_identity,
        "{label} changed while opening"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "{label} exceeds its limit while reading"
    );
    let after_file = file.metadata()?;
    let after_path = fs::symlink_metadata(path)?;
    ensure!(
        after_file.file_type().is_file()
            && FileIdentity::from_metadata(&after_file) == before_identity
            && after_path.file_type().is_file()
            && FileIdentity::from_metadata(&after_path) == before_identity,
        "{label} changed while reading"
    );
    let value = serde_json::from_slice(&bytes).with_context(|| format!("parse {label}"))?;
    Ok((
        value,
        ContentIdentity {
            format,
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
    ))
}

pub(super) fn read_cf32(path: &Path, maximum_bytes: u64, expected_sha256: &str) -> Result<Vec<u8>> {
    ensure!(
        valid_sha256(expected_sha256),
        "CF32 manifest digest is malformed"
    );
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect CF32 input {}", path.display()))?;
    ensure!(
        before.file_type().is_file(),
        "CF32 input must be a direct regular file"
    );
    ensure!(
        before.len() <= maximum_bytes,
        "CF32 input exceeds its byte limit"
    );
    let before_identity = FileIdentity::from_metadata(&before);
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.file_type().is_file() && FileIdentity::from_metadata(&opened) == before_identity,
        "CF32 input changed while opening"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(before.len())?);
    (&mut file)
        .take(maximum_bytes + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "CF32 input length changed while reading"
    );
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == expected_sha256.to_ascii_lowercase(),
        "CF32 content digest differs from its manifest"
    );
    let after_file = file.metadata()?;
    let after_path = fs::symlink_metadata(path)?;
    ensure!(
        after_file.file_type().is_file()
            && FileIdentity::from_metadata(&after_file) == before_identity
            && after_path.file_type().is_file()
            && FileIdentity::from_metadata(&after_path) == before_identity,
        "CF32 input changed while reading"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn stable_json_rejects_malformed_input_and_direct_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let malformed = directory.path().join("malformed.json");
        fs::write(&malformed, b"{").unwrap();
        assert!(
            read_stable_json::<serde_json::Value>(&malformed, "test manifest", "test").is_err()
        );

        let regular = directory.path().join("regular.json");
        fs::write(&regular, b"{}\n").unwrap();
        let link = directory.path().join("link.json");
        symlink(&regular, &link).unwrap();
        assert!(read_stable_json::<serde_json::Value>(&link, "test manifest", "test").is_err());
    }

    #[test]
    fn cf32_reader_rejects_tampering_symlinks_and_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples.cf32");
        let bytes = [0u8; 16];
        fs::write(&path, bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(read_cf32(&path, 16, &digest).unwrap(), bytes);
        assert!(read_cf32(&path, 15, &digest).is_err());
        assert!(read_cf32(&path, 16, &"0".repeat(64)).is_err());

        let link = directory.path().join("samples-link.cf32");
        symlink(&path, &link).unwrap();
        assert!(read_cf32(&link, 16, &digest).is_err());

        fs::write(&path, [1u8; 16]).unwrap();
        assert!(read_cf32(&path, 16, &digest).is_err());
    }
}
