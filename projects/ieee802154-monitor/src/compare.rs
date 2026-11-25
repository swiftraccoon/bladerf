//! Bounded exact-identity comparison for hard- and soft-decoder candidate archives.
//!
//! The comparison is deliberately narrower than over-air deduplication. Two
//! candidates overlap only when their recovered-chip completion position and
//! complete PSDU bytes are identical. The archive header binds both inputs to
//! the same IEEE 802.15.4 channel and rates before records are compared.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

const FILE_MAGIC: &[u8; 8] = b"I154RAW\0";
const RECORD_MAGIC: &[u8; 4] = b"PSDU";
const VERSION: u16 = 1;
const FILE_HEADER_LEN: usize = 24;
const RECORD_HEADER_LEN: usize = 20;
const CHANNEL_SAMPLE_RATE_HZ: u32 = 4_000_000;
const RECOVERED_CHIP_RATE_HZ: u32 = 2_000_000;
const MAX_PSDU_BYTES: usize = 127;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RECORDS: u64 = 10_000_000;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    max_input_bytes: u64,
    max_records: u64,
}

impl Limits {
    pub(crate) fn from_cli(max_input_mib: u32, max_records: u64) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&max_input_mib),
            "candidate input limit must be 1..=8192 MiB per lane"
        );
        ensure!(
            (1..=MAX_RECORDS).contains(&max_records),
            "candidate record limit must be 1..={MAX_RECORDS} per lane"
        );
        Ok(Self {
            max_input_bytes: u64::from(max_input_mib) * 1024 * 1024,
            max_records,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) hard_input: PathBuf,
    pub(crate) soft_input: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) limits: Limits,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    operation: &'static str,
    hard_source_format: &'static str,
    hard_source_bytes: u64,
    hard_source_sha256: String,
    soft_source_format: &'static str,
    soft_source_bytes: u64,
    soft_source_sha256: String,
    ieee802154_channel: u8,
    channel_sample_rate_hz: u32,
    recovered_chip_rate_hz: u32,
    hard_candidate_record_count: u64,
    soft_candidate_record_count: u64,
    hard_valid_fcs_record_count: u64,
    soft_valid_fcs_record_count: u64,
    exact_identity_overlap_record_count: u64,
    hard_only_record_count: u64,
    soft_only_record_count: u64,
    identity_union_record_count: u64,
    exact_identity_overlap_valid_fcs_record_count: u64,
    hard_only_valid_fcs_record_count: u64,
    soft_only_valid_fcs_record_count: u64,
    identity_union_valid_fcs_record_count: u64,
    configured_max_input_bytes: u64,
    configured_max_records: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArchiveHeader {
    channel: u8,
}

#[derive(Debug)]
struct Candidate {
    chip_count: u64,
    psdu: [u8; MAX_PSDU_BYTES],
    psdu_length: usize,
    valid_fcs: bool,
}

impl Candidate {
    fn psdu(&self) -> &[u8] {
        &self.psdu[..self.psdu_length]
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    bytes: u64,
    modified: SystemTime,
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct SourceReport {
    bytes: u64,
    sha256: String,
    records: u64,
    valid_fcs_records: u64,
}

#[derive(Debug)]
struct ArchiveReader {
    path: PathBuf,
    reader: BufReader<File>,
    before: Snapshot,
    digest: Sha256,
    header: ArchiveHeader,
    consumed: u64,
    records: u64,
    valid_fcs_records: u64,
    last_chip_count: Option<u64>,
    max_records: u64,
}

impl ArchiveReader {
    fn open(path: &Path, limits: Limits) -> Result<Self> {
        let path_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect candidate archive path {}", path.display()))?;
        ensure!(
            path_metadata.file_type().is_file(),
            "candidate archive must be a direct regular file"
        );
        let file = File::open(path)
            .with_context(|| format!("open candidate archive {}", path.display()))?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "candidate archive must be a regular file"
        );
        ensure!(
            metadata.dev() == path_metadata.dev() && metadata.ino() == path_metadata.ino(),
            "candidate archive path changed while opening"
        );
        ensure!(
            (FILE_HEADER_LEN as u64..=limits.max_input_bytes).contains(&metadata.len()),
            "candidate archive byte length is outside the configured per-lane limit"
        );
        let before = Snapshot {
            bytes: metadata.len(),
            modified: metadata
                .modified()
                .context("read candidate archive modification time")?,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, file);
        let mut digest = Sha256::new();
        let mut bytes = [0u8; FILE_HEADER_LEN];
        read_exact_hashed(
            &mut reader,
            &mut bytes,
            &mut digest,
            "read candidate archive header",
        )?;
        let header = parse_file_header(&bytes)?;
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            before,
            digest,
            header,
            consumed: FILE_HEADER_LEN as u64,
            records: 0,
            valid_fcs_records: 0,
            last_chip_count: None,
            max_records: limits.max_records,
        })
    }

    fn next_candidate(&mut self) -> Result<Option<Candidate>> {
        if self.consumed == self.before.bytes {
            return Ok(None);
        }
        ensure!(
            self.before.bytes - self.consumed >= RECORD_HEADER_LEN as u64,
            "truncated candidate record header"
        );
        ensure!(
            self.records < self.max_records,
            "candidate record limit exceeded"
        );
        let mut header = [0u8; RECORD_HEADER_LEN];
        read_exact_hashed(
            &mut self.reader,
            &mut header,
            &mut self.digest,
            "read candidate record header",
        )?;
        self.consumed = checked_add(self.consumed, RECORD_HEADER_LEN as u64, "input bytes")?;
        ensure!(
            &header[..4] == RECORD_MAGIC,
            "invalid candidate record magic"
        );
        let record_length = u64::from(u32_le(&header, 4));
        let chip_count = u64_le(&header, 8);
        ensure!(
            chip_count > 0,
            "candidate recovered chip count must be positive"
        );
        ensure!(
            self.last_chip_count.is_none_or(|prior| chip_count > prior),
            "candidate recovered chip counts must be strictly increasing"
        );
        let psdu_length = usize::from(u16_le(&header, 16));
        ensure!(
            (1..=MAX_PSDU_BYTES).contains(&psdu_length),
            "candidate PSDU length must be 1..={MAX_PSDU_BYTES}"
        );
        ensure!(u16_le(&header, 18) == 0, "unknown candidate record flags");
        ensure!(
            record_length == (RECORD_HEADER_LEN + psdu_length) as u64,
            "candidate record length does not match PSDU length"
        );
        ensure!(
            u64::try_from(psdu_length)? <= self.before.bytes - self.consumed,
            "truncated candidate PSDU"
        );
        let mut psdu = [0u8; MAX_PSDU_BYTES];
        read_exact_hashed(
            &mut self.reader,
            &mut psdu[..psdu_length],
            &mut self.digest,
            "read candidate PSDU",
        )?;
        self.consumed = checked_add(self.consumed, u64::try_from(psdu_length)?, "input bytes")?;
        self.records = checked_add(self.records, 1, "candidate records")?;
        self.last_chip_count = Some(chip_count);
        let valid_fcs = valid_fcs(&psdu[..psdu_length]);
        if valid_fcs {
            self.valid_fcs_records =
                checked_add(self.valid_fcs_records, 1, "valid-FCS candidate records")?;
        }
        Ok(Some(Candidate {
            chip_count,
            psdu,
            psdu_length,
            valid_fcs,
        }))
    }

    fn finish(mut self) -> Result<SourceReport> {
        ensure!(
            self.consumed == self.before.bytes,
            "candidate archive length changed while reading"
        );
        let mut trailing = [0u8; 1];
        ensure!(
            self.reader.read(&mut trailing)? == 0,
            "candidate archive grew while reading"
        );
        let metadata = self.reader.get_ref().metadata()?;
        let path_metadata = fs::symlink_metadata(&self.path)
            .context("reinspect candidate archive path after comparison")?;
        ensure!(
            path_metadata.file_type().is_file(),
            "candidate archive path is no longer a direct regular file"
        );
        ensure!(
            metadata.len() == self.before.bytes
                && metadata.modified()? == self.before.modified
                && metadata.dev() == self.before.device
                && metadata.ino() == self.before.inode
                && path_metadata.dev() == self.before.device
                && path_metadata.ino() == self.before.inode,
            "candidate archive changed while reading"
        );
        Ok(SourceReport {
            bytes: self.before.bytes,
            sha256: format!("{:x}", self.digest.finalize()),
            records: self.records,
            valid_fcs_records: self.valid_fcs_records,
        })
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct Partitions {
    overlap: u64,
    hard_only: u64,
    soft_only: u64,
    overlap_valid: u64,
    hard_only_valid: u64,
    soft_only_valid: u64,
}

impl Partitions {
    fn add_overlap(&mut self, valid: bool) -> Result<()> {
        self.overlap = checked_add(self.overlap, 1, "overlap records")?;
        if valid {
            self.overlap_valid = checked_add(self.overlap_valid, 1, "valid overlap records")?;
        }
        Ok(())
    }

    fn add_hard_only(&mut self, valid: bool) -> Result<()> {
        self.hard_only = checked_add(self.hard_only, 1, "hard-only records")?;
        if valid {
            self.hard_only_valid = checked_add(self.hard_only_valid, 1, "valid hard-only records")?;
        }
        Ok(())
    }

    fn add_soft_only(&mut self, valid: bool) -> Result<()> {
        self.soft_only = checked_add(self.soft_only, 1, "soft-only records")?;
        if valid {
            self.soft_only_valid = checked_add(self.soft_only_valid, 1, "valid soft-only records")?;
        }
        Ok(())
    }
}

fn checked_add(left: u64, right: u64, what: &'static str) -> Result<u64> {
    left.checked_add(right)
        .with_context(|| format!("{what} count overflow"))
}

fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed header"))
}

fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

fn u64_le(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed header"))
}

fn read_exact_hashed(
    reader: &mut impl Read,
    bytes: &mut [u8],
    digest: &mut Sha256,
    context: &'static str,
) -> Result<()> {
    reader.read_exact(bytes).context(context)?;
    digest.update(bytes);
    Ok(())
}

fn parse_file_header(bytes: &[u8; FILE_HEADER_LEN]) -> Result<ArchiveHeader> {
    ensure!(&bytes[..8] == FILE_MAGIC, "invalid candidate archive magic");
    ensure!(
        u16_le(bytes, 8) == VERSION,
        "unsupported candidate archive version"
    );
    ensure!(
        usize::from(u16_le(bytes, 10)) == FILE_HEADER_LEN,
        "unsupported candidate archive header length"
    );
    let channel = bytes[12];
    ensure!(
        (11..=26).contains(&channel),
        "candidate archive channel must be 11..=26"
    );
    ensure!(bytes[13] == 0, "unknown candidate archive flags");
    ensure!(
        u16_le(bytes, 14) == 0,
        "candidate archive reserved field is nonzero"
    );
    ensure!(
        u32_le(bytes, 16) == CHANNEL_SAMPLE_RATE_HZ,
        "candidate archive channel sample rate must be {CHANNEL_SAMPLE_RATE_HZ} Hz"
    );
    ensure!(
        u32_le(bytes, 20) == RECOVERED_CHIP_RATE_HZ,
        "candidate archive recovered chip rate must be {RECOVERED_CHIP_RATE_HZ} chips/s"
    );
    Ok(ArchiveHeader { channel })
}

fn dot15d4_fcs(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn valid_fcs(psdu: &[u8]) -> bool {
    if psdu.len() < 2 {
        return false;
    }
    let payload_end = psdu.len() - 2;
    let received = u16::from_le_bytes([psdu[payload_end], psdu[payload_end + 1]]);
    received == dot15d4_fcs(&psdu[..payload_end])
}

fn compare_readers(hard: &mut ArchiveReader, soft: &mut ArchiveReader) -> Result<Partitions> {
    let mut partitions = Partitions::default();
    let mut hard_candidate = hard.next_candidate()?;
    let mut soft_candidate = soft.next_candidate()?;
    while hard_candidate.is_some() || soft_candidate.is_some() {
        match (&hard_candidate, &soft_candidate) {
            (Some(hard_value), Some(soft_value)) => {
                match hard_value.chip_count.cmp(&soft_value.chip_count) {
                    std::cmp::Ordering::Less => {
                        partitions.add_hard_only(hard_value.valid_fcs)?;
                        hard_candidate = hard.next_candidate()?;
                    }
                    std::cmp::Ordering::Greater => {
                        partitions.add_soft_only(soft_value.valid_fcs)?;
                        soft_candidate = soft.next_candidate()?;
                    }
                    std::cmp::Ordering::Equal => {
                        if hard_value.psdu() == soft_value.psdu() {
                            ensure!(
                                hard_value.valid_fcs == soft_value.valid_fcs,
                                "identical candidates have inconsistent FCS classification"
                            );
                            partitions.add_overlap(hard_value.valid_fcs)?;
                        } else {
                            partitions.add_hard_only(hard_value.valid_fcs)?;
                            partitions.add_soft_only(soft_value.valid_fcs)?;
                        }
                        hard_candidate = hard.next_candidate()?;
                        soft_candidate = soft.next_candidate()?;
                    }
                }
            }
            (Some(hard_value), None) => {
                partitions.add_hard_only(hard_value.valid_fcs)?;
                hard_candidate = hard.next_candidate()?;
            }
            (None, Some(soft_value)) => {
                partitions.add_soft_only(soft_value.valid_fcs)?;
                soft_candidate = soft.next_candidate()?;
            }
            (None, None) => break,
        }
    }
    Ok(partitions)
}

fn new_output_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output parent {}", parent.display()))?;
    }
    DirBuilder::new().mode(0o700).create(path).with_context(|| {
        format!(
            "create new output directory {} (existing outputs are preserved)",
            path.display()
        )
    })
}

fn write_summary(output: &Path, summary: &Summary) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(summary)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_SUMMARY_BYTES,
        "comparison summary exceeds the fixed {MAX_SUMMARY_BYTES}-byte limit"
    );
    let partial = output.join("summary.json.partial");
    let final_path = output.join("summary.json");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    let mut writer = BufWriter::with_capacity(IO_BUFFER_BYTES, file);
    writer.write_all(&bytes)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::hard_link(&partial, &final_path).with_context(|| {
        format!(
            "publish {} without replacing an existing output",
            final_path.display()
        )
    })?;
    fs::remove_file(&partial)?;
    File::open(output)?.sync_all()?;
    Ok(())
}

pub(crate) fn run(request: Request) -> Result<Summary> {
    ensure!(
        (FILE_HEADER_LEN as u64..=MAX_INPUT_BYTES).contains(&request.limits.max_input_bytes),
        "candidate input byte limit must be {FILE_HEADER_LEN}..={MAX_INPUT_BYTES} per lane"
    );
    ensure!(
        (1..=MAX_RECORDS).contains(&request.limits.max_records),
        "candidate record limit must be 1..={MAX_RECORDS} per lane"
    );
    let mut hard = ArchiveReader::open(&request.hard_input, request.limits)?;
    let mut soft = ArchiveReader::open(&request.soft_input, request.limits)?;
    ensure!(
        hard.header == soft.header,
        "hard and soft archives must have the same channel and rate contract"
    );
    let channel = hard.header.channel;
    let partitions = compare_readers(&mut hard, &mut soft)?;
    let hard_report = hard.finish()?;
    let soft_report = soft.finish()?;

    ensure!(
        checked_add(partitions.overlap, partitions.hard_only, "hard partition")?
            == hard_report.records,
        "hard comparison partition does not match its archive"
    );
    ensure!(
        checked_add(partitions.overlap, partitions.soft_only, "soft partition")?
            == soft_report.records,
        "soft comparison partition does not match its archive"
    );
    ensure!(
        checked_add(
            partitions.overlap_valid,
            partitions.hard_only_valid,
            "valid hard partition",
        )? == hard_report.valid_fcs_records,
        "valid hard comparison partition does not match its archive"
    );
    ensure!(
        checked_add(
            partitions.overlap_valid,
            partitions.soft_only_valid,
            "valid soft partition",
        )? == soft_report.valid_fcs_records,
        "valid soft comparison partition does not match its archive"
    );
    let identity_union_record_count = checked_add(
        checked_add(partitions.overlap, partitions.hard_only, "identity union")?,
        partitions.soft_only,
        "identity union",
    )?;
    let identity_union_valid_fcs_record_count = checked_add(
        checked_add(
            partitions.overlap_valid,
            partitions.hard_only_valid,
            "valid identity union",
        )?,
        partitions.soft_only_valid,
        "valid identity union",
    )?;
    let summary = Summary {
        schema_version: 1,
        operation: "compare_ieee802154_candidate_lanes",
        hard_source_format: "ieee802154_raw_candidates_v1",
        hard_source_bytes: hard_report.bytes,
        hard_source_sha256: hard_report.sha256,
        soft_source_format: "ieee802154_raw_candidates_v1",
        soft_source_bytes: soft_report.bytes,
        soft_source_sha256: soft_report.sha256,
        ieee802154_channel: channel,
        channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
        recovered_chip_rate_hz: RECOVERED_CHIP_RATE_HZ,
        hard_candidate_record_count: hard_report.records,
        soft_candidate_record_count: soft_report.records,
        hard_valid_fcs_record_count: hard_report.valid_fcs_records,
        soft_valid_fcs_record_count: soft_report.valid_fcs_records,
        exact_identity_overlap_record_count: partitions.overlap,
        hard_only_record_count: partitions.hard_only,
        soft_only_record_count: partitions.soft_only,
        identity_union_record_count,
        exact_identity_overlap_valid_fcs_record_count: partitions.overlap_valid,
        hard_only_valid_fcs_record_count: partitions.hard_only_valid,
        soft_only_valid_fcs_record_count: partitions.soft_only_valid,
        identity_union_valid_fcs_record_count,
        configured_max_input_bytes: request.limits.max_input_bytes,
        configured_max_records: request.limits.max_records,
    };
    new_output_directory(&request.output)?;
    write_summary(&request.output, &summary)?;
    crate::print_summary(&summary, "Candidate comparison output", &request.output)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn archive_header(channel: u8) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(FILE_HEADER_LEN);
        bytes.extend_from_slice(FILE_MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(FILE_HEADER_LEN as u16).to_le_bytes());
        bytes.push(channel);
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&CHANNEL_SAMPLE_RATE_HZ.to_le_bytes());
        bytes.extend_from_slice(&RECOVERED_CHIP_RATE_HZ.to_le_bytes());
        bytes
    }

    fn append_record(bytes: &mut Vec<u8>, chip_count: u64, psdu: &[u8]) {
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.extend_from_slice(&((RECORD_HEADER_LEN + psdu.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&chip_count.to_le_bytes());
        bytes.extend_from_slice(&(psdu.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(psdu);
    }

    fn ack() -> Vec<u8> {
        vec![0x02, 0x00, 0x53, 0xa6, 0xd5]
    }

    fn invalid_ack() -> Vec<u8> {
        let mut bytes = ack();
        bytes[4] ^= 1;
        bytes
    }

    fn compare_bytes(hard: &[u8], soft: &[u8], max_records: u64) -> (Summary, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let hard_input = directory.path().join("hard.i154raw");
        let soft_input = directory.path().join("soft.i154raw");
        fs::write(&hard_input, hard).unwrap();
        fs::write(&soft_input, soft).unwrap();
        let summary = run(Request {
            hard_input,
            soft_input,
            output: directory.path().join("out"),
            limits: Limits {
                max_input_bytes: 1024 * 1024,
                max_records,
            },
        })
        .unwrap();
        (summary, directory)
    }

    #[test]
    fn empty_and_identical_archives_have_exact_partitions() {
        let empty = archive_header(17);
        let (summary, _) = compare_bytes(&empty, &empty, 1);
        assert_eq!(summary.hard_candidate_record_count, 0);
        assert_eq!(summary.soft_candidate_record_count, 0);
        assert_eq!(summary.identity_union_record_count, 0);

        let mut identical = archive_header(17);
        append_record(&mut identical, 704, &ack());
        let (summary, directory) = compare_bytes(&identical, &identical, 1);
        assert_eq!(summary.hard_candidate_record_count, 1);
        assert_eq!(summary.soft_candidate_record_count, 1);
        assert_eq!(summary.hard_valid_fcs_record_count, 1);
        assert_eq!(summary.soft_valid_fcs_record_count, 1);
        assert_eq!(summary.exact_identity_overlap_record_count, 1);
        assert_eq!(summary.exact_identity_overlap_valid_fcs_record_count, 1);
        assert_eq!(summary.hard_only_record_count, 0);
        assert_eq!(summary.soft_only_record_count, 0);
        assert_eq!(summary.identity_union_record_count, 1);
        let serialized: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("out/summary.json")).unwrap())
                .unwrap();
        assert_eq!(serialized["schema_version"], 1);
        assert_eq!(
            serialized["operation"],
            "compare_ieee802154_candidate_lanes"
        );
        let mut fields = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "channel_sample_rate_hz",
                "configured_max_input_bytes",
                "configured_max_records",
                "exact_identity_overlap_record_count",
                "exact_identity_overlap_valid_fcs_record_count",
                "hard_candidate_record_count",
                "hard_only_record_count",
                "hard_only_valid_fcs_record_count",
                "hard_source_bytes",
                "hard_source_format",
                "hard_source_sha256",
                "hard_valid_fcs_record_count",
                "identity_union_record_count",
                "identity_union_valid_fcs_record_count",
                "ieee802154_channel",
                "operation",
                "recovered_chip_rate_hz",
                "schema_version",
                "soft_candidate_record_count",
                "soft_only_record_count",
                "soft_only_valid_fcs_record_count",
                "soft_source_bytes",
                "soft_source_format",
                "soft_source_sha256",
                "soft_valid_fcs_record_count",
            ]
        );
        let serialized_text = serde_json::to_string(&serialized).unwrap();
        assert!(!serialized_text.contains("psdu"));
        assert!(!serialized_text.contains("recovered_chip_count_at_end"));
        assert!(!serialized_text.contains("source_file"));
    }

    #[test]
    fn exact_identity_requires_both_position_and_psdu() {
        let mut hard = archive_header(17);
        append_record(&mut hard, 100, &ack());
        append_record(&mut hard, 300, &invalid_ack());
        let mut soft = archive_header(17);
        append_record(&mut soft, 100, &invalid_ack());
        append_record(&mut soft, 200, &ack());
        let (summary, _) = compare_bytes(&hard, &soft, 2);
        assert_eq!(summary.exact_identity_overlap_record_count, 0);
        assert_eq!(summary.hard_only_record_count, 2);
        assert_eq!(summary.soft_only_record_count, 2);
        assert_eq!(summary.identity_union_record_count, 4);
        assert_eq!(summary.hard_only_valid_fcs_record_count, 1);
        assert_eq!(summary.soft_only_valid_fcs_record_count, 1);
        assert_eq!(summary.identity_union_valid_fcs_record_count, 2);
    }

    #[test]
    fn valid_invalid_and_length_invalid_records_partition_independently() {
        let mut hard = archive_header(18);
        append_record(&mut hard, 10, &ack());
        append_record(&mut hard, 20, &invalid_ack());
        append_record(&mut hard, 30, &[0x00]);
        let mut soft = archive_header(18);
        append_record(&mut soft, 10, &ack());
        append_record(&mut soft, 40, &ack());
        let (summary, _) = compare_bytes(&hard, &soft, 3);
        assert_eq!(summary.hard_candidate_record_count, 3);
        assert_eq!(summary.soft_candidate_record_count, 2);
        assert_eq!(summary.hard_valid_fcs_record_count, 1);
        assert_eq!(summary.soft_valid_fcs_record_count, 2);
        assert_eq!(summary.exact_identity_overlap_record_count, 1);
        assert_eq!(summary.hard_only_record_count, 2);
        assert_eq!(summary.soft_only_record_count, 1);
        assert_eq!(summary.exact_identity_overlap_valid_fcs_record_count, 1);
        assert_eq!(summary.hard_only_valid_fcs_record_count, 0);
        assert_eq!(summary.soft_only_valid_fcs_record_count, 1);
        assert_eq!(summary.identity_union_valid_fcs_record_count, 2);
    }

    #[test]
    fn swapping_lane_roles_only_swaps_role_bound_fields() {
        let mut hard = archive_header(16);
        append_record(&mut hard, 10, &ack());
        let mut soft = archive_header(16);
        append_record(&mut soft, 20, &invalid_ack());
        let (forward, _) = compare_bytes(&hard, &soft, 1);
        let (reverse, _) = compare_bytes(&soft, &hard, 1);
        assert_eq!(
            forward.hard_only_record_count,
            reverse.soft_only_record_count
        );
        assert_eq!(
            forward.soft_only_record_count,
            reverse.hard_only_record_count
        );
        assert_eq!(
            forward.identity_union_record_count,
            reverse.identity_union_record_count
        );
        assert_eq!(forward.hard_source_sha256, reverse.soft_source_sha256);
        assert_eq!(forward.soft_source_sha256, reverse.hard_source_sha256);
    }

    #[test]
    fn rejects_malformed_mismatched_symlinked_and_over_limit_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let hard = directory.path().join("hard.i154raw");
        let soft = directory.path().join("soft.i154raw");
        fs::write(&hard, archive_header(16)).unwrap();
        fs::write(&soft, archive_header(17)).unwrap();
        let request = |hard_input: PathBuf, soft_input: PathBuf, output: &str, records| Request {
            hard_input,
            soft_input,
            output: directory.path().join(output),
            limits: Limits {
                max_input_bytes: 1024,
                max_records: records,
            },
        };
        assert!(run(request(hard.clone(), soft.clone(), "mismatch", 1)).is_err());

        fs::write(&soft, archive_header(16)).unwrap();
        let alias = directory.path().join("alias.i154raw");
        symlink(&hard, &alias).unwrap();
        assert!(run(request(alias, soft.clone(), "symlink", 1)).is_err());

        let mut nonmonotonic = archive_header(16);
        append_record(&mut nonmonotonic, 2, &ack());
        append_record(&mut nonmonotonic, 2, &ack());
        fs::write(&hard, nonmonotonic).unwrap();
        assert!(run(request(hard.clone(), soft.clone(), "nonmonotonic", 2)).is_err());

        let mut two = archive_header(16);
        append_record(&mut two, 1, &ack());
        append_record(&mut two, 2, &ack());
        fs::write(&hard, two).unwrap();
        assert!(run(request(hard.clone(), soft.clone(), "record-limit", 1)).is_err());

        fs::write(&hard, archive_header(16)).unwrap();
        let mut too_small = request(hard, soft, "byte-limit", 1);
        too_small.limits.max_input_bytes = (FILE_HEADER_LEN - 1) as u64;
        assert!(run(too_small).is_err());
        assert!(Limits::from_cli(0, 1).is_err());
        assert!(Limits::from_cli(8193, 1).is_err());
        assert!(Limits::from_cli(1, 0).is_err());
        assert!(Limits::from_cli(1, MAX_RECORDS + 1).is_err());
        assert!(checked_add(u64::MAX, 1, "test").is_err());
    }

    #[test]
    fn rejects_path_replacement_after_the_archive_was_read() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input.i154raw");
        let replacement = directory.path().join("replacement.i154raw");
        fs::write(&input, archive_header(17)).unwrap();
        let limits = Limits {
            max_input_bytes: 1024,
            max_records: 1,
        };
        let mut reader = ArchiveReader::open(&input, limits).unwrap();
        assert!(reader.next_candidate().unwrap().is_none());
        fs::write(&replacement, archive_header(17)).unwrap();
        fs::rename(&replacement, &input).unwrap();
        assert!(reader.finish().is_err());
    }

    #[test]
    fn publishes_private_summary_without_overwrite() {
        let archive = archive_header(17);
        let (summary, directory) = compare_bytes(&archive, &archive, 1);
        let output = directory.path().join("out");
        assert_eq!(summary.identity_union_record_count, 0);
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(output.join("summary.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!output.join("summary.json.partial").exists());
        let before = fs::read(output.join("summary.json")).unwrap();
        assert!(
            run(Request {
                hard_input: directory.path().join("hard.i154raw"),
                soft_input: directory.path().join("soft.i154raw"),
                output: output.clone(),
                limits: Limits {
                    max_input_bytes: 1024,
                    max_records: 1,
                },
            })
            .is_err()
        );
        assert_eq!(fs::read(output.join("summary.json")).unwrap(), before);
    }
}
