//! Bounded exact-identity comparison across decoder carrier-offset hypotheses.
//!
//! This comparison is deliberately narrower than over-air frame deduplication.
//! An identity contains the archive channel, exact recovered-chip completion
//! position and complete PSDU bytes. The caller-provided lane and carrier offset
//! remain provenance for each source but are not part of the identity.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail, ensure};
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
const MAX_ABS_CARRIER_OFFSET_HZ: i32 = 100_000;
const SOURCE_COUNT: usize = 6;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecoderLane {
    Hard,
    Soft,
}

impl DecoderLane {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "hard" => Ok(Self::Hard),
            "soft" => Ok(Self::Soft),
            _ => bail!("decoder lane must be hard or soft"),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Soft => "soft",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SourceSpec {
    pub(crate) decoder_lane: DecoderLane,
    pub(crate) carrier_offset_hz: i32,
    pub(crate) input: PathBuf,
}

impl SourceSpec {
    fn new(decoder_lane: DecoderLane, carrier_offset_hz: i32, input: PathBuf) -> Result<Self> {
        ensure!(
            (-MAX_ABS_CARRIER_OFFSET_HZ..=MAX_ABS_CARRIER_OFFSET_HZ).contains(&carrier_offset_hz),
            "carrier offset must be within +/-{MAX_ABS_CARRIER_OFFSET_HZ} Hz"
        );
        Ok(Self {
            decoder_lane,
            carrier_offset_hz,
            input,
        })
    }
}

pub(crate) fn sources_from_cli(values: &[OsString]) -> Result<Vec<SourceSpec>> {
    ensure!(
        values.len() == SOURCE_COUNT * 3,
        "compare-hypotheses requires exactly {SOURCE_COUNT} --source LANE OFFSET_HZ PATH groups"
    );
    let mut sources = Vec::with_capacity(SOURCE_COUNT);
    for fields in values.as_chunks::<3>().0 {
        let lane = fields[0]
            .to_str()
            .context("decoder lane must be valid UTF-8")?;
        let offset = fields[1]
            .to_str()
            .context("carrier offset must be valid UTF-8")?
            .parse::<i32>()
            .context("carrier offset must be a signed integer in Hz")?;
        sources.push(SourceSpec::new(
            DecoderLane::parse(lane)?,
            offset,
            PathBuf::from(&fields[2]),
        )?);
    }
    validate_descriptors(&sources)?;
    Ok(sources)
}

fn validate_descriptors(sources: &[SourceSpec]) -> Result<()> {
    ensure!(
        sources.len() == SOURCE_COUNT,
        "hypothesis comparison requires exactly {SOURCE_COUNT} sources"
    );
    let mut descriptors = BTreeSet::new();
    for source in sources {
        ensure!(
            (-MAX_ABS_CARRIER_OFFSET_HZ..=MAX_ABS_CARRIER_OFFSET_HZ)
                .contains(&source.carrier_offset_hz),
            "carrier offset must be within +/-{MAX_ABS_CARRIER_OFFSET_HZ} Hz"
        );
        ensure!(
            descriptors.insert((source.carrier_offset_hz, source.decoder_lane)),
            "hypothesis source descriptors must be unique"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    max_input_bytes: u64,
    max_records: u64,
}

impl Limits {
    pub(crate) fn from_cli(max_input_mib: u32, max_records: u64) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&max_input_mib),
            "candidate input limit must be 1..=8192 MiB per source"
        );
        ensure!(
            (1..=MAX_RECORDS).contains(&max_records),
            "candidate record limit must be 1..={MAX_RECORDS} per source"
        );
        Ok(Self {
            max_input_bytes: u64::from(max_input_mib) * 1024 * 1024,
            max_records,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) sources: Vec<SourceSpec>,
    pub(crate) output: PathBuf,
    pub(crate) limits: Limits,
}

#[derive(Debug, Serialize)]
struct SourceSummary {
    source_index: usize,
    decoder_lane: &'static str,
    carrier_offset_hz: i32,
    source_format: &'static str,
    source_bytes: u64,
    source_sha256: String,
    candidate_record_count: u64,
    valid_fcs_record_count: u64,
    invalid_fcs_record_count: u64,
    invalid_length_record_count: u64,
}

#[derive(Debug, Serialize)]
struct MembershipPartition {
    source_membership_mask: u8,
    source_membership_count: u32,
    identity_record_count: u64,
    valid_fcs_record_count: u64,
    invalid_fcs_record_count: u64,
    invalid_length_record_count: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    operation: &'static str,
    source_format: &'static str,
    source_count: usize,
    sources: Vec<SourceSummary>,
    ieee802154_channel: u8,
    channel_sample_rate_hz: u32,
    recovered_chip_rate_hz: u32,
    source_candidate_record_count_sum: u64,
    source_valid_fcs_record_count_sum: u64,
    source_invalid_fcs_record_count_sum: u64,
    source_invalid_length_record_count_sum: u64,
    identity_union_record_count: u64,
    identity_union_valid_fcs_record_count: u64,
    identity_union_invalid_fcs_record_count: u64,
    identity_union_invalid_length_record_count: u64,
    duplicate_record_count: u64,
    duplicate_valid_fcs_record_count: u64,
    duplicate_invalid_fcs_record_count: u64,
    duplicate_invalid_length_record_count: u64,
    membership_partitions: Vec<MembershipPartition>,
    configured_max_input_bytes_per_source: u64,
    configured_max_records_per_source: u64,
    interpretation: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArchiveHeader {
    channel: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FcsStatus {
    Valid,
    Invalid,
    LengthInvalid,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Counts {
    records: u64,
    valid: u64,
    invalid: u64,
    invalid_length: u64,
}

impl Counts {
    fn add_status(&mut self, status: FcsStatus) -> Result<()> {
        self.records = checked_add(self.records, 1, "record")?;
        match status {
            FcsStatus::Valid => self.valid = checked_add(self.valid, 1, "valid-FCS record")?,
            FcsStatus::Invalid => {
                self.invalid = checked_add(self.invalid, 1, "invalid-FCS record")?;
            }
            FcsStatus::LengthInvalid => {
                self.invalid_length = checked_add(self.invalid_length, 1, "invalid-length record")?;
            }
        }
        Ok(())
    }

    fn checked_add(self, other: Self, what: &'static str) -> Result<Self> {
        Ok(Self {
            records: checked_add(self.records, other.records, what)?,
            valid: checked_add(self.valid, other.valid, what)?,
            invalid: checked_add(self.invalid, other.invalid, what)?,
            invalid_length: checked_add(self.invalid_length, other.invalid_length, what)?,
        })
    }

    fn checked_sub(self, other: Self, what: &'static str) -> Result<Self> {
        Ok(Self {
            records: self
                .records
                .checked_sub(other.records)
                .with_context(|| format!("{what} record count underflow"))?,
            valid: self
                .valid
                .checked_sub(other.valid)
                .with_context(|| format!("{what} valid count underflow"))?,
            invalid: self
                .invalid
                .checked_sub(other.invalid)
                .with_context(|| format!("{what} invalid count underflow"))?,
            invalid_length: self
                .invalid_length
                .checked_sub(other.invalid_length)
                .with_context(|| format!("{what} invalid-length count underflow"))?,
        })
    }

    fn ensure_closed(self, what: &'static str) -> Result<()> {
        ensure!(
            checked_add(
                checked_add(self.valid, self.invalid, what)?,
                self.invalid_length,
                what,
            )? == self.records,
            "{what} FCS partition does not close"
        );
        Ok(())
    }
}

#[derive(Debug)]
struct Candidate {
    chip_count: u64,
    psdu: [u8; MAX_PSDU_BYTES],
    psdu_length: usize,
    status: FcsStatus,
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
    counts: Counts,
}

#[derive(Debug)]
struct ArchiveReader {
    path: PathBuf,
    reader: BufReader<File>,
    before: Snapshot,
    digest: Sha256,
    header: ArchiveHeader,
    consumed: u64,
    counts: Counts,
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
            "candidate archive byte length is outside the configured per-source limit"
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
            counts: Counts::default(),
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
            self.counts.records < self.max_records,
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
        let status = classify_fcs(&psdu[..psdu_length]);
        self.counts.add_status(status)?;
        self.last_chip_count = Some(chip_count);
        Ok(Some(Candidate {
            chip_count,
            psdu,
            psdu_length,
            status,
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
        self.counts.ensure_closed("source")?;
        Ok(SourceReport {
            bytes: self.before.bytes,
            sha256: format!("{:x}", self.digest.finalize()),
            counts: self.counts,
        })
    }
}

#[derive(Debug)]
struct IdentityGroup {
    psdu: [u8; MAX_PSDU_BYTES],
    psdu_length: usize,
    status: FcsStatus,
    source_membership_mask: u8,
}

impl IdentityGroup {
    fn from_candidate(source_index: usize, candidate: Candidate) -> Self {
        Self {
            psdu: candidate.psdu,
            psdu_length: candidate.psdu_length,
            status: candidate.status,
            source_membership_mask: 1u8 << source_index,
        }
    }

    fn psdu(&self) -> &[u8] {
        &self.psdu[..self.psdu_length]
    }
}

#[derive(Debug, Default)]
struct Aggregate {
    union: Counts,
    memberships: BTreeMap<u8, Counts>,
}

impl Aggregate {
    fn add(&mut self, group: IdentityGroup) -> Result<()> {
        ensure!(
            group.source_membership_mask > 0
                && group.source_membership_mask < (1u8 << SOURCE_COUNT),
            "identity source membership mask is outside the source set"
        );
        self.union.add_status(group.status)?;
        self.memberships
            .entry(group.source_membership_mask)
            .or_default()
            .add_status(group.status)
    }
}

fn checked_add(left: u64, right: u64, what: &'static str) -> Result<u64> {
    left.checked_add(right)
        .with_context(|| format!("{what} count overflow"))
}

fn checked_weight(value: u64, weight: u32, what: &'static str) -> Result<u64> {
    value
        .checked_mul(u64::from(weight))
        .with_context(|| format!("{what} weighted count overflow"))
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

fn classify_fcs(psdu: &[u8]) -> FcsStatus {
    if psdu.len() < 2 {
        return FcsStatus::LengthInvalid;
    }
    let payload_end = psdu.len() - 2;
    let received = u16::from_le_bytes([psdu[payload_end], psdu[payload_end + 1]]);
    if received == dot15d4_fcs(&psdu[..payload_end]) {
        FcsStatus::Valid
    } else {
        FcsStatus::Invalid
    }
}

fn compare_readers(
    readers: &mut [ArchiveReader],
    candidates: &mut [Option<Candidate>],
) -> Result<Aggregate> {
    let mut aggregate = Aggregate::default();
    let mut groups = Vec::<IdentityGroup>::with_capacity(SOURCE_COUNT);
    while let Some(chip_count) = candidates
        .iter()
        .filter_map(|candidate| candidate.as_ref().map(|value| value.chip_count))
        .min()
    {
        groups.clear();
        for source_index in 0..candidates.len() {
            if candidates[source_index]
                .as_ref()
                .is_some_and(|candidate| candidate.chip_count == chip_count)
            {
                let candidate = candidates[source_index]
                    .take()
                    .expect("candidate presence was checked");
                if let Some(group) = groups
                    .iter_mut()
                    .find(|group| group.psdu() == candidate.psdu())
                {
                    ensure!(
                        group.status == candidate.status,
                        "identical candidates have inconsistent FCS classification"
                    );
                    group.source_membership_mask |= 1u8 << source_index;
                } else {
                    groups.push(IdentityGroup::from_candidate(source_index, candidate));
                }
                candidates[source_index] = readers[source_index].next_candidate()?;
            }
        }
        for group in groups.drain(..) {
            aggregate.add(group)?;
        }
    }
    Ok(aggregate)
}

fn validate_closure(
    reports: &[SourceReport],
    union: Counts,
    memberships: &BTreeMap<u8, Counts>,
) -> Result<()> {
    union.ensure_closed("identity union")?;
    let mut observed_union = Counts::default();
    let mut weighted = Counts::default();
    let mut per_source = [Counts::default(); SOURCE_COUNT];
    for (mask, counts) in memberships {
        ensure!(*mask > 0, "membership mask must be nonzero");
        counts.ensure_closed("membership")?;
        observed_union = observed_union.checked_add(*counts, "membership union")?;
        let weight = mask.count_ones();
        weighted = weighted.checked_add(
            Counts {
                records: checked_weight(counts.records, weight, "membership")?,
                valid: checked_weight(counts.valid, weight, "membership")?,
                invalid: checked_weight(counts.invalid, weight, "membership")?,
                invalid_length: checked_weight(counts.invalid_length, weight, "membership")?,
            },
            "weighted membership",
        )?;
        for (source_index, source_counts) in per_source.iter_mut().enumerate() {
            if mask & (1u8 << source_index) != 0 {
                *source_counts = source_counts.checked_add(*counts, "source membership")?;
            }
        }
    }
    ensure!(
        observed_union == union,
        "membership partitions differ from identity union"
    );
    let raw = reports
        .iter()
        .try_fold(Counts::default(), |total, report| {
            total.checked_add(report.counts, "source record sum")
        })?;
    ensure!(
        weighted == raw,
        "weighted membership partitions differ from source totals"
    );
    ensure!(
        per_source
            .iter()
            .zip(reports)
            .all(|(observed, report)| *observed == report.counts),
        "membership partitions differ from a source total"
    );
    Ok(())
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
        "hypothesis-comparison summary exceeds the fixed {MAX_SUMMARY_BYTES}-byte limit"
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
        "candidate input byte limit must be {FILE_HEADER_LEN}..={MAX_INPUT_BYTES} per source"
    );
    ensure!(
        (1..=MAX_RECORDS).contains(&request.limits.max_records),
        "candidate record limit must be 1..={MAX_RECORDS} per source"
    );
    validate_descriptors(&request.sources)?;

    let mut readers = request
        .sources
        .iter()
        .map(|source| ArchiveReader::open(&source.input, request.limits))
        .collect::<Result<Vec<_>>>()?;
    let mut source_file_identities = BTreeSet::new();
    for reader in &readers {
        ensure!(
            source_file_identities.insert((reader.before.device, reader.before.inode)),
            "hypothesis sources must be distinct direct files"
        );
    }
    let archive_header = readers[0].header;
    ensure!(
        readers.iter().all(|reader| reader.header == archive_header),
        "hypothesis archives must have the same channel and rate contract"
    );
    let mut candidates = readers
        .iter_mut()
        .map(ArchiveReader::next_candidate)
        .collect::<Result<Vec<_>>>()?;
    let aggregate = compare_readers(&mut readers, &mut candidates)?;
    let reports = readers
        .into_iter()
        .map(ArchiveReader::finish)
        .collect::<Result<Vec<_>>>()?;
    validate_closure(&reports, aggregate.union, &aggregate.memberships)?;

    let raw = reports
        .iter()
        .try_fold(Counts::default(), |total, report| {
            total.checked_add(report.counts, "source record sum")
        })?;
    raw.ensure_closed("source record sum")?;
    let duplicate = raw.checked_sub(aggregate.union, "duplicate")?;
    duplicate.ensure_closed("duplicate")?;
    let sources = request
        .sources
        .iter()
        .zip(&reports)
        .enumerate()
        .map(|(source_index, (source, report))| SourceSummary {
            source_index,
            decoder_lane: source.decoder_lane.as_str(),
            carrier_offset_hz: source.carrier_offset_hz,
            source_format: "ieee802154_raw_candidates_v1",
            source_bytes: report.bytes,
            source_sha256: report.sha256.clone(),
            candidate_record_count: report.counts.records,
            valid_fcs_record_count: report.counts.valid,
            invalid_fcs_record_count: report.counts.invalid,
            invalid_length_record_count: report.counts.invalid_length,
        })
        .collect();
    let membership_partitions = aggregate
        .memberships
        .iter()
        .map(|(mask, counts)| MembershipPartition {
            source_membership_mask: *mask,
            source_membership_count: mask.count_ones(),
            identity_record_count: counts.records,
            valid_fcs_record_count: counts.valid,
            invalid_fcs_record_count: counts.invalid,
            invalid_length_record_count: counts.invalid_length,
        })
        .collect();
    let summary = Summary {
        schema_version: 1,
        operation: "compare_ieee802154_candidate_hypotheses",
        source_format: "ieee802154_raw_candidates_v1",
        source_count: SOURCE_COUNT,
        sources,
        ieee802154_channel: archive_header.channel,
        channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
        recovered_chip_rate_hz: RECOVERED_CHIP_RATE_HZ,
        source_candidate_record_count_sum: raw.records,
        source_valid_fcs_record_count_sum: raw.valid,
        source_invalid_fcs_record_count_sum: raw.invalid,
        source_invalid_length_record_count_sum: raw.invalid_length,
        identity_union_record_count: aggregate.union.records,
        identity_union_valid_fcs_record_count: aggregate.union.valid,
        identity_union_invalid_fcs_record_count: aggregate.union.invalid,
        identity_union_invalid_length_record_count: aggregate.union.invalid_length,
        duplicate_record_count: duplicate.records,
        duplicate_valid_fcs_record_count: duplicate.valid,
        duplicate_invalid_fcs_record_count: duplicate.invalid,
        duplicate_invalid_length_record_count: duplicate.invalid_length,
        membership_partitions,
        configured_max_input_bytes_per_source: request.limits.max_input_bytes,
        configured_max_records_per_source: request.limits.max_records,
        interpretation: "The union deduplicates only exact decoder-output identity: channel, recovered-chip completion position, PSDU length and complete PSDU bytes. It is not unique over-air frame, transmitter, RF-time, occupancy, sensitivity, authentication, decryption, continuity or completeness evidence. The caller-provided lane and carrier-offset descriptors are provenance claims bound to source digests, not fields embedded in version-1 archives.",
    };
    new_output_directory(&request.output)?;
    write_summary(&request.output, &summary)?;
    crate::print_summary(
        &summary,
        "Candidate hypothesis comparison output",
        &request.output,
    )?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
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

    fn append_record(bytes: &mut Vec<u8>, chip_count: u64, psdu: &[u8], flags: u16) {
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.extend_from_slice(&((RECORD_HEADER_LEN + psdu.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(&chip_count.to_le_bytes());
        bytes.extend_from_slice(&(psdu.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes.extend_from_slice(psdu);
    }

    fn valid_psdu(body: &[u8]) -> Vec<u8> {
        let mut psdu = body.to_vec();
        psdu.extend_from_slice(&dot15d4_fcs(body).to_le_bytes());
        psdu
    }

    fn ack() -> Vec<u8> {
        valid_psdu(&[0x02, 0x00, 0x53])
    }

    fn invalid_ack() -> Vec<u8> {
        let mut bytes = ack();
        bytes[4] ^= 1;
        bytes
    }

    fn descriptors(paths: &[PathBuf]) -> Vec<SourceSpec> {
        let values = [
            (DecoderLane::Hard, 0),
            (DecoderLane::Soft, 0),
            (DecoderLane::Hard, -100_000),
            (DecoderLane::Soft, -100_000),
            (DecoderLane::Hard, 100_000),
            (DecoderLane::Soft, 100_000),
        ];
        values
            .into_iter()
            .zip(paths)
            .map(|((lane, offset), path)| SourceSpec::new(lane, offset, path.clone()).unwrap())
            .collect()
    }

    fn compare_archives(archives: &[Vec<u8>]) -> (Summary, tempfile::TempDir) {
        assert_eq!(archives.len(), SOURCE_COUNT);
        let directory = tempfile::tempdir().unwrap();
        let paths = archives
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                let path = directory.path().join(format!("source-{index}.i154raw"));
                fs::write(&path, bytes).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let summary = run(Request {
            sources: descriptors(&paths),
            output: directory.path().join("out"),
            limits: Limits {
                max_input_bytes: 1024 * 1024,
                max_records: 16,
            },
        })
        .unwrap();
        (summary, directory)
    }

    fn empty_archives(channel: u8) -> Vec<Vec<u8>> {
        (0..SOURCE_COUNT).map(|_| archive_header(channel)).collect()
    }

    fn json_keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("summary value must be an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn empty_source_bank_has_closed_empty_partitions() {
        let (summary, _) = compare_archives(&empty_archives(16));
        assert_eq!(summary.source_candidate_record_count_sum, 0);
        assert_eq!(summary.source_valid_fcs_record_count_sum, 0);
        assert_eq!(summary.source_invalid_fcs_record_count_sum, 0);
        assert_eq!(summary.source_invalid_length_record_count_sum, 0);
        assert_eq!(summary.identity_union_record_count, 0);
        assert_eq!(summary.identity_union_valid_fcs_record_count, 0);
        assert_eq!(summary.identity_union_invalid_fcs_record_count, 0);
        assert_eq!(summary.identity_union_invalid_length_record_count, 0);
        assert_eq!(summary.duplicate_record_count, 0);
        assert!(summary.membership_partitions.is_empty());
        assert_eq!(summary.sources.len(), SOURCE_COUNT);
        assert!(
            summary
                .sources
                .iter()
                .all(|source| source.candidate_record_count == 0)
        );
    }

    #[test]
    fn all_source_overlap_has_one_union_identity_and_exact_membership() {
        let mut archive = archive_header(17);
        append_record(&mut archive, 100, &ack(), 0);
        let (summary, directory) = compare_archives(&vec![archive; SOURCE_COUNT]);
        assert_eq!(summary.source_candidate_record_count_sum, 6);
        assert_eq!(summary.identity_union_record_count, 1);
        assert_eq!(summary.identity_union_valid_fcs_record_count, 1);
        assert_eq!(summary.duplicate_record_count, 5);
        assert_eq!(summary.duplicate_valid_fcs_record_count, 5);
        assert_eq!(summary.membership_partitions.len(), 1);
        assert_eq!(
            summary.membership_partitions[0].source_membership_mask,
            0x3f
        );
        assert_eq!(summary.membership_partitions[0].source_membership_count, 6);
        assert_eq!(summary.membership_partitions[0].identity_record_count, 1);
        assert_eq!(summary.sources[0].decoder_lane, "hard");
        assert_eq!(summary.sources[0].carrier_offset_hz, 0);
        assert_eq!(summary.sources[2].carrier_offset_hz, -100_000);
        assert_eq!(summary.sources[5].decoder_lane, "soft");

        let path = directory.path().join("out/summary.json");
        let serialized: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(serialized["schema_version"], 1);
        assert_eq!(
            serialized["operation"],
            "compare_ieee802154_candidate_hypotheses"
        );
        assert_eq!(
            json_keys(&serialized),
            BTreeSet::from([
                "channel_sample_rate_hz",
                "configured_max_input_bytes_per_source",
                "configured_max_records_per_source",
                "duplicate_invalid_fcs_record_count",
                "duplicate_invalid_length_record_count",
                "duplicate_record_count",
                "duplicate_valid_fcs_record_count",
                "identity_union_invalid_fcs_record_count",
                "identity_union_invalid_length_record_count",
                "identity_union_record_count",
                "identity_union_valid_fcs_record_count",
                "ieee802154_channel",
                "interpretation",
                "membership_partitions",
                "operation",
                "recovered_chip_rate_hz",
                "schema_version",
                "source_candidate_record_count_sum",
                "source_count",
                "source_format",
                "source_invalid_fcs_record_count_sum",
                "source_invalid_length_record_count_sum",
                "source_valid_fcs_record_count_sum",
                "sources",
            ])
        );
        for source in serialized["sources"].as_array().unwrap() {
            assert_eq!(
                json_keys(source),
                BTreeSet::from([
                    "candidate_record_count",
                    "carrier_offset_hz",
                    "decoder_lane",
                    "invalid_fcs_record_count",
                    "invalid_length_record_count",
                    "source_bytes",
                    "source_format",
                    "source_index",
                    "source_sha256",
                    "valid_fcs_record_count",
                ])
            );
        }
        for partition in serialized["membership_partitions"].as_array().unwrap() {
            assert_eq!(
                json_keys(partition),
                BTreeSet::from([
                    "identity_record_count",
                    "invalid_fcs_record_count",
                    "invalid_length_record_count",
                    "source_membership_count",
                    "source_membership_mask",
                    "valid_fcs_record_count",
                ])
            );
        }
        let text = serde_json::to_string(&serialized).unwrap();
        assert!(!text.contains("source_file"));
        assert!(!text.contains("psdu"));
        assert!(!text.contains("chip_count"));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(directory.path().join("out"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn repeated_shifted_and_same_position_different_psdus_remain_distinct() {
        let mut archives = empty_archives(17);
        append_record(&mut archives[0], 100, &ack(), 0);
        append_record(&mut archives[0], 200, &ack(), 0);
        append_record(&mut archives[1], 100, &ack(), 0);
        append_record(&mut archives[1], 200, &ack(), 0);
        let (repeated, _) = compare_archives(&archives);
        assert_eq!(repeated.source_candidate_record_count_sum, 4);
        assert_eq!(repeated.identity_union_record_count, 2);
        assert_eq!(repeated.duplicate_record_count, 2);
        assert_eq!(
            repeated.membership_partitions[0].source_membership_mask,
            0x03
        );
        assert_eq!(repeated.membership_partitions[0].identity_record_count, 2);

        let mut shifted = empty_archives(17);
        append_record(&mut shifted[0], 100, &ack(), 0);
        append_record(&mut shifted[1], 101, &ack(), 0);
        let (shifted, _) = compare_archives(&shifted);
        assert_eq!(shifted.identity_union_record_count, 2);
        assert_eq!(shifted.duplicate_record_count, 0);
        assert_eq!(shifted.membership_partitions.len(), 2);

        let mut different = empty_archives(17);
        append_record(&mut different[0], 100, &ack(), 0);
        append_record(&mut different[1], 100, &valid_psdu(&[0x02, 0x00, 0x54]), 0);
        let (different, _) = compare_archives(&different);
        assert_eq!(different.identity_union_record_count, 2);
        assert_eq!(different.duplicate_record_count, 0);
    }

    #[test]
    fn mixed_fcs_states_close_source_union_duplicate_and_membership_counts() {
        let mut archives = empty_archives(18);
        for index in [0, 1] {
            append_record(&mut archives[index], 10, &ack(), 0);
            append_record(&mut archives[index], 20, &invalid_ack(), 0);
            append_record(&mut archives[index], 30, &[0x00], 0);
        }
        let (summary, _) = compare_archives(&archives);
        assert_eq!(summary.source_candidate_record_count_sum, 6);
        assert_eq!(summary.source_valid_fcs_record_count_sum, 2);
        assert_eq!(summary.source_invalid_fcs_record_count_sum, 2);
        assert_eq!(summary.source_invalid_length_record_count_sum, 2);
        assert_eq!(summary.identity_union_record_count, 3);
        assert_eq!(summary.identity_union_valid_fcs_record_count, 1);
        assert_eq!(summary.identity_union_invalid_fcs_record_count, 1);
        assert_eq!(summary.identity_union_invalid_length_record_count, 1);
        assert_eq!(summary.duplicate_record_count, 3);
        assert_eq!(summary.duplicate_valid_fcs_record_count, 1);
        assert_eq!(summary.duplicate_invalid_fcs_record_count, 1);
        assert_eq!(summary.duplicate_invalid_length_record_count, 1);
        assert_eq!(summary.membership_partitions.len(), 1);
        assert_eq!(
            summary.membership_partitions[0].source_membership_mask,
            0x03
        );
        assert_eq!(summary.membership_partitions[0].identity_record_count, 3);
    }

    #[test]
    fn membership_histogram_is_sparse_sorted_and_exact() {
        let mut archives = empty_archives(18);
        append_record(&mut archives[0], 10, &ack(), 0);
        append_record(&mut archives[0], 20, &invalid_ack(), 0);
        append_record(&mut archives[1], 10, &ack(), 0);
        append_record(&mut archives[2], 20, &invalid_ack(), 0);
        append_record(&mut archives[5], 30, &[0x00], 0);
        let (summary, _) = compare_archives(&archives);
        assert_eq!(summary.source_candidate_record_count_sum, 5);
        assert_eq!(summary.identity_union_record_count, 3);
        assert_eq!(summary.duplicate_record_count, 2);
        assert_eq!(summary.membership_partitions.len(), 3);
        assert_eq!(
            summary
                .membership_partitions
                .iter()
                .map(|partition| partition.source_membership_mask)
                .collect::<Vec<_>>(),
            [0x03, 0x05, 0x20]
        );
        assert_eq!(summary.membership_partitions[0].valid_fcs_record_count, 1);
        assert_eq!(summary.membership_partitions[1].invalid_fcs_record_count, 1);
        assert_eq!(
            summary.membership_partitions[2].invalid_length_record_count,
            1
        );
    }

    #[test]
    fn cli_sources_preserve_caller_order_and_reject_bad_descriptors() {
        let values = [
            "soft",
            "100000",
            "positive-soft",
            "hard",
            "0",
            "zero-hard",
            "soft",
            "0",
            "zero-soft",
            "hard",
            "-100000",
            "negative-hard",
            "soft",
            "-100000",
            "negative-soft",
            "hard",
            "100000",
            "positive-hard",
        ]
        .map(OsString::from);
        let sources = sources_from_cli(&values).unwrap();
        assert_eq!(sources[0].decoder_lane, DecoderLane::Soft);
        assert_eq!(sources[0].carrier_offset_hz, 100_000);
        assert_eq!(sources[0].input, PathBuf::from("positive-soft"));
        assert_eq!(sources[5].decoder_lane, DecoderLane::Hard);
        assert_eq!(sources[5].carrier_offset_hz, 100_000);

        assert!(sources_from_cli(&values[..15]).is_err());
        let mut duplicate = values.clone();
        duplicate[15] = OsString::from("soft");
        assert!(sources_from_cli(&duplicate).is_err());
        let mut bad_lane = values.clone();
        bad_lane[0] = OsString::from("other");
        assert!(sources_from_cli(&bad_lane).is_err());
        let mut bad_offset = values.clone();
        bad_offset[1] = OsString::from("100001");
        assert!(sources_from_cli(&bad_offset).is_err());
        assert!(Limits::from_cli(0, 1).is_err());
        assert!(Limits::from_cli(8193, 1).is_err());
        assert!(Limits::from_cli(1, 0).is_err());
        assert!(Limits::from_cli(1, MAX_RECORDS + 1).is_err());
    }

    #[test]
    fn malformed_mismatched_symlinked_limited_and_colliding_inputs_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..SOURCE_COUNT {
            let path = directory.path().join(format!("source-{index}.i154raw"));
            fs::write(&path, archive_header(if index == 5 { 18 } else { 17 })).unwrap();
            paths.push(path);
        }
        let request = |sources: Vec<SourceSpec>, output: &str, max_records| Request {
            sources,
            output: directory.path().join(output),
            limits: Limits {
                max_input_bytes: 1024,
                max_records,
            },
        };
        assert!(run(request(descriptors(&paths), "mismatch", 1)).is_err());
        assert!(!directory.path().join("mismatch").exists());

        fs::write(&paths[5], archive_header(17)).unwrap();
        let alias = directory.path().join("alias.i154raw");
        symlink(&paths[0], &alias).unwrap();
        let mut aliased = descriptors(&paths);
        aliased[0].input = alias;
        assert!(run(request(aliased, "symlink", 1)).is_err());

        let hard_link = directory.path().join("hard-link.i154raw");
        fs::hard_link(&paths[0], &hard_link).unwrap();
        let mut duplicated = descriptors(&paths);
        duplicated[1].input = hard_link;
        assert!(run(request(duplicated, "duplicate-file", 1)).is_err());

        let mut two = archive_header(17);
        append_record(&mut two, 1, &ack(), 0);
        append_record(&mut two, 2, &ack(), 0);
        fs::write(&paths[0], &two).unwrap();
        assert!(run(request(descriptors(&paths), "record-limit", 1)).is_err());

        fs::write(&paths[0], vec![0u8; 1025]).unwrap();
        assert!(run(request(descriptors(&paths), "byte-limit", 2)).is_err());

        let mut truncated = archive_header(17);
        append_record(&mut truncated, 1, &ack(), 0);
        truncated.pop();
        fs::write(&paths[0], truncated).unwrap();
        assert!(run(request(descriptors(&paths), "truncated", 2)).is_err());

        let mut nonmonotonic = archive_header(17);
        append_record(&mut nonmonotonic, 2, &ack(), 0);
        append_record(&mut nonmonotonic, 2, &ack(), 0);
        fs::write(&paths[0], nonmonotonic).unwrap();
        assert!(run(request(descriptors(&paths), "nonmonotonic", 2)).is_err());

        let mut bad_flags = archive_header(17);
        append_record(&mut bad_flags, 1, &ack(), 1);
        fs::write(&paths[0], bad_flags).unwrap();
        assert!(run(request(descriptors(&paths), "bad-flags", 2)).is_err());

        fs::write(&paths[0], archive_header(17)).unwrap();
        let collision = directory.path().join("collision");
        fs::create_dir(&collision).unwrap();
        fs::write(collision.join("marker"), b"preserve").unwrap();
        assert!(run(request(descriptors(&paths), "collision", 1)).is_err());
        assert_eq!(fs::read(collision.join("marker")).unwrap(), b"preserve");
    }

    #[test]
    fn source_path_replacement_and_in_place_growth_are_detected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.i154raw");
        let moved = directory.path().join("moved.i154raw");
        let mut archive = archive_header(17);
        append_record(&mut archive, 1, &ack(), 0);
        fs::write(&path, &archive).unwrap();
        let mut reader = ArchiveReader::open(
            &path,
            Limits {
                max_input_bytes: 1024,
                max_records: 1,
            },
        )
        .unwrap();
        assert!(reader.next_candidate().unwrap().is_some());
        assert!(reader.next_candidate().unwrap().is_none());
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, archive).unwrap();
        assert!(reader.finish().is_err());

        let growing = directory.path().join("growing.i154raw");
        let mut archive = archive_header(17);
        append_record(&mut archive, 1, &ack(), 0);
        fs::write(&growing, archive).unwrap();
        let mut reader = ArchiveReader::open(
            &growing,
            Limits {
                max_input_bytes: 1024,
                max_records: 1,
            },
        )
        .unwrap();
        assert!(reader.next_candidate().unwrap().is_some());
        assert!(reader.next_candidate().unwrap().is_none());
        OpenOptions::new()
            .append(true)
            .open(&growing)
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert!(reader.finish().is_err());
    }
}
