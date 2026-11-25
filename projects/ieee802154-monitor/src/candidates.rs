//! Bounded validation and independent FCS classification for demodulator candidates.
//!
//! Candidate archive version 1 is little-endian. Its 24-byte file header is:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 8 | `I154RAW\0` magic |
//! | 8 | 2 | version (`1`) |
//! | 10 | 2 | file header length (`24`) |
//! | 12 | 1 | IEEE 802.15.4 channel (`11..=26`) |
//! | 13 | 1 | file flags (`0`) |
//! | 14 | 2 | reserved (`0`) |
//! | 16 | 4 | channel sample rate (`4_000_000` Hz) |
//! | 20 | 4 | recovered chip rate (`2_000_000` chips/s) |
//!
//! Each record has a 20-byte header followed by its complete PSDU, including FCS:
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 4 | `PSDU` magic |
//! | 4 | 4 | complete record length (`20 + psdu_length`) |
//! | 8 | 8 | recovered chip count at candidate completion |
//! | 16 | 2 | PSDU length (`1..=127`) |
//! | 18 | 2 | record flags/reserved (`0`) |
//!
//! Recovered-chip counts are strictly increasing ordering/timing estimates from
//! the software decoder. They are not hardware sample timestamps or RF time.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::source_index;

const FILE_MAGIC: &[u8; 8] = b"I154RAW\0";
const RECORD_MAGIC: &[u8; 4] = b"PSDU";
const VERSION: u16 = 1;
const FILE_HEADER_LEN: usize = 24;
const RECORD_HEADER_LEN: usize = 20;
pub(crate) const CHANNEL_SAMPLE_RATE_HZ: u32 = 4_000_000;
pub(crate) const RECOVERED_CHIP_RATE_HZ: u32 = 2_000_000;
const MAX_PSDU_BYTES: usize = 127;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RECORDS: u64 = 10_000_000;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const PCAP_SNAPLEN: u32 = MAX_PSDU_BYTES as u32;
const PCAP_DLT_IEEE802154_WITH_FCS: u32 = 195;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    max_input_bytes: u64,
    max_output_bytes: u64,
    max_records: u64,
}

impl Limits {
    pub(crate) fn from_cli(
        max_input_mib: u32,
        max_output_mib: u32,
        max_records: u64,
    ) -> Result<Self> {
        ensure!(
            (1..=8192).contains(&max_input_mib),
            "candidate input limit must be 1..=8192 MiB"
        );
        ensure!(
            (1..=8192).contains(&max_output_mib),
            "analysis output limit must be 1..=8192 MiB"
        );
        ensure!(
            (1..=MAX_RECORDS).contains(&max_records),
            "candidate record limit must be 1..={MAX_RECORDS}"
        );
        Ok(Self {
            max_input_bytes: u64::from(max_input_mib) * 1024 * 1024,
            max_output_bytes: u64::from(max_output_mib) * 1024 * 1024,
            max_records,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) input: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) limits: Limits,
    pub(crate) source_index: Option<source_index::Request>,
}

#[derive(Clone, Copy, Debug)]
struct ArchiveHeader {
    channel: u8,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FcsStatus {
    LengthInvalid,
    Valid,
    Invalid,
}

#[derive(Debug, Serialize)]
struct MacObservation {
    status: &'static str,
    frame_control: Option<u16>,
    frame_type_code: Option<u8>,
    frame_type: Option<&'static str>,
    security_enabled: Option<bool>,
    frame_pending: Option<bool>,
    acknowledgment_requested: Option<bool>,
    pan_id_compression: Option<bool>,
    sequence_number_suppression_bit_set: Option<bool>,
    sequence_number_suppression_status: &'static str,
    sequence_number_suppressed: Option<bool>,
    information_elements_bit_set: Option<bool>,
    information_elements_present: Option<bool>,
    destination_address_mode: Option<&'static str>,
    frame_version_code: Option<u8>,
    frame_version: Option<&'static str>,
    source_address_mode: Option<&'static str>,
    sequence_number_status: &'static str,
    sequence_number: Option<u8>,
}

#[derive(Debug, Serialize)]
struct PacketRecord {
    candidate_index: u64,
    recovered_chip_count_at_end: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    required_through_channel_sample: Option<u64>,
    relative_decoder_end_seconds: u32,
    relative_decoder_end_subsecond_nanoseconds: u32,
    psdu_length: usize,
    fcs_status: FcsStatus,
    received_fcs: Option<u16>,
    computed_fcs: Option<u16>,
    mac: MacObservation,
}

#[derive(Debug, Serialize)]
struct OutputFileReport {
    file: &'static str,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    operation: &'static str,
    source_format: &'static str,
    source_file: PathBuf,
    source_bytes: u64,
    source_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_index: Option<SourceIndexEvidence>,
    ieee802154_channel: u8,
    channel_sample_rate_hz: u32,
    recovered_chip_rate_hz: u32,
    candidate_record_count: u64,
    valid_fcs_record_count: u64,
    invalid_fcs_record_count: u64,
    invalid_length_record_count: u64,
    validated_psdu_bytes: u64,
    first_recovered_chip_count_at_end: Option<u64>,
    last_recovered_chip_count_at_end: Option<u64>,
    fcs_valid_frame_type_observations: BTreeMap<&'static str, u64>,
    packets_jsonl: OutputFileReport,
    validated_pcap: OutputFileReport,
    output_bytes_excluding_summary: u64,
    configured_max_input_bytes: u64,
    configured_max_output_bytes: u64,
    configured_max_records: u64,
    pcap_link_type: u32,
    pcap_timestamp_kind: &'static str,
    interpretation: &'static str,
}

#[derive(Debug, Serialize)]
struct SourceIndexEvidence {
    #[serde(flatten)]
    index: source_index::Report,
    #[serde(flatten)]
    outside: OutsideCounts,
}

#[derive(Debug, Default, Serialize)]
struct OutsideCounts {
    outside_record_count: u64,
    outside_valid_fcs_record_count: u64,
    outside_invalid_fcs_record_count: u64,
    outside_invalid_length_record_count: u64,
}

impl OutsideCounts {
    fn observe(&mut self, status: FcsStatus) {
        self.outside_record_count += 1;
        match status {
            FcsStatus::LengthInvalid => self.outside_invalid_length_record_count += 1,
            FcsStatus::Invalid => self.outside_invalid_fcs_record_count += 1,
            FcsStatus::Valid => self.outside_valid_fcs_record_count += 1,
        }
    }
}

#[derive(Debug)]
struct Counters {
    records: u64,
    valid: u64,
    invalid: u64,
    invalid_length: u64,
    validated_bytes: u64,
    first_chip_count: Option<u64>,
    last_chip_count: Option<u64>,
    fcs_valid_frame_type_observations: BTreeMap<&'static str, u64>,
}

impl Counters {
    fn new() -> Self {
        Self {
            records: 0,
            valid: 0,
            invalid: 0,
            invalid_length: 0,
            validated_bytes: 0,
            first_chip_count: None,
            last_chip_count: None,
            fcs_valid_frame_type_observations: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct OutputBudget {
    limit: u64,
    used: u64,
}

impl OutputBudget {
    fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    fn consume(&mut self, bytes: usize) -> Result<()> {
        let next = self
            .used
            .checked_add(u64::try_from(bytes)?)
            .context("analysis output byte count overflow")?;
        ensure!(
            next <= self.limit,
            "analysis output exceeds configured {}-byte limit",
            self.limit
        );
        self.used = next;
        Ok(())
    }
}

#[derive(Debug)]
struct StagedWriter {
    name: &'static str,
    partial_path: PathBuf,
    final_path: PathBuf,
    writer: BufWriter<File>,
    digest: Sha256,
    bytes: u64,
}

impl StagedWriter {
    fn new(output: &Path, name: &'static str) -> Result<Self> {
        let final_path = output.join(name);
        let partial_path = output.join(format!("{name}.partial"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)
            .with_context(|| format!("create {}", partial_path.display()))?;
        Ok(Self {
            name,
            partial_path,
            final_path,
            writer: BufWriter::with_capacity(IO_BUFFER_BYTES, file),
            digest: Sha256::new(),
            bytes: 0,
        })
    }

    fn write_budgeted(&mut self, bytes: &[u8], budget: &mut OutputBudget) -> Result<()> {
        budget.consume(bytes.len())?;
        self.writer.write_all(bytes)?;
        self.digest.update(bytes);
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(bytes.len())?)
            .context("output file byte count overflow")?;
        Ok(())
    }

    fn finish(mut self) -> Result<OutputFileReport> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        fs::hard_link(&self.partial_path, &self.final_path).with_context(|| {
            format!(
                "publish {} without replacing an existing output",
                self.final_path.display()
            )
        })?;
        fs::remove_file(&self.partial_path)?;
        Ok(OutputFileReport {
            file: self.name,
            bytes: self.bytes,
            sha256: format!("{:x}", self.digest.finalize()),
        })
    }
}

pub(crate) fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed header"))
}

pub(crate) fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

pub(crate) fn u64_le(bytes: &[u8], offset: usize) -> u64 {
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

fn classify_fcs(psdu: &[u8]) -> (FcsStatus, Option<u16>, Option<u16>) {
    if psdu.len() < 2 {
        return (FcsStatus::LengthInvalid, None, None);
    }
    let payload_end = psdu.len() - 2;
    let received = u16::from_le_bytes([psdu[payload_end], psdu[payload_end + 1]]);
    let computed = dot15d4_fcs(&psdu[..payload_end]);
    let status = if received == computed {
        FcsStatus::Valid
    } else {
        FcsStatus::Invalid
    };
    (status, Some(received), Some(computed))
}

fn address_mode(bits: u16) -> &'static str {
    match bits {
        0 => "none",
        1 => "reserved",
        2 => "short",
        3 => "extended",
        _ => unreachable!("two-bit field"),
    }
}

fn frame_version(bits: u8) -> &'static str {
    match bits {
        0 => "ieee802154_2003",
        1 => "ieee802154_2006",
        2 => "ieee802154_2015",
        3 => "reserved",
        _ => unreachable!("two-bit field"),
    }
}

fn frame_type(bits: u8) -> &'static str {
    match bits {
        0 => "beacon",
        1 => "data",
        2 => "acknowledgment",
        3 => "mac_command",
        4 => "reserved",
        5..=7 => "version_dependent_or_reserved",
        _ => unreachable!("three-bit field"),
    }
}

fn unavailable_mac(status: &'static str, sequence_status: &'static str) -> MacObservation {
    MacObservation {
        status,
        frame_control: None,
        frame_type_code: None,
        frame_type: None,
        security_enabled: None,
        frame_pending: None,
        acknowledgment_requested: None,
        pan_id_compression: None,
        sequence_number_suppression_bit_set: None,
        sequence_number_suppression_status: "unavailable",
        sequence_number_suppressed: None,
        information_elements_bit_set: None,
        information_elements_present: None,
        destination_address_mode: None,
        frame_version_code: None,
        frame_version: None,
        source_address_mode: None,
        sequence_number_status: sequence_status,
        sequence_number: None,
    }
}

fn observe_mac(psdu: &[u8]) -> MacObservation {
    if psdu.len() < 2 {
        return unavailable_mac("unavailable_length_below_fcs", "unavailable");
    }
    let body = &psdu[..psdu.len() - 2];
    if body.len() < 2 {
        return unavailable_mac("truncated_frame_control", "truncated");
    }
    let control = u16::from_le_bytes([body[0], body[1]]);
    let frame_type_code = u8::try_from(control & 0x7).expect("three-bit field");
    let frame_version_code = u8::try_from((control >> 12) & 0x3).expect("two-bit field");
    let suppression_bit_set = control & (1 << 8) != 0;
    let information_elements_bit_set = control & (1 << 9) != 0;
    let ordinary_type = frame_type_code <= 3;
    let modern_ordinary_type = ordinary_type && frame_version_code == 2;
    let interpretable_ordinary_header = ordinary_type && frame_version_code != 3;
    let suppressed = modern_ordinary_type && suppression_bit_set;
    let suppression_status = match (ordinary_type, frame_version_code, suppression_bit_set) {
        (false, _, _) => "uninterpreted_for_frame_type",
        (true, 2, true) => "supported_and_set",
        (true, 2, false) => "supported_not_set",
        (true, 0 | 1, true) => "reserved_bit_set_for_frame_version",
        (true, 0 | 1, false) => "reserved_bit_not_set_for_frame_version",
        (true, 3, true) => "reserved_frame_version_bit_set",
        (true, 3, false) => "reserved_frame_version_bit_not_set",
        _ => unreachable!("two-bit frame version"),
    };
    let (sequence_number_status, sequence_number) = if !ordinary_type {
        ("unavailable_for_frame_type", None)
    } else if frame_version_code == 3 {
        ("unavailable_for_reserved_frame_version", None)
    } else if suppressed {
        ("suppressed", None)
    } else if body.len() >= 3 {
        ("present", Some(body[2]))
    } else {
        ("truncated", None)
    };
    let old_ordinary_version = ordinary_type && frame_version_code <= 1;
    let reserved_suppression_bit = old_ordinary_version && suppression_bit_set;
    let reserved_information_elements_bit = old_ordinary_version && information_elements_bit_set;
    let status = if !ordinary_type {
        if frame_type_code == 4 {
            "frame_control_only_reserved_type"
        } else {
            "frame_control_only_version_dependent_or_reserved_type"
        }
    } else if frame_version_code == 3 {
        "frame_control_only_reserved_frame_version"
    } else {
        match (reserved_suppression_bit, reserved_information_elements_bit) {
            (true, true) => "observed_with_reserved_sequence_and_information_element_bits",
            (true, false) => "observed_with_reserved_sequence_suppression_bit",
            (false, true) => "observed_with_reserved_information_element_bit",
            (false, false) if !suppressed && body.len() < 3 => "truncated_sequence_number",
            (false, false) => "observed",
        }
    };
    MacObservation {
        status,
        frame_control: Some(control),
        frame_type_code: Some(frame_type_code),
        frame_type: Some(frame_type(frame_type_code)),
        security_enabled: interpretable_ordinary_header.then_some(control & (1 << 3) != 0),
        frame_pending: interpretable_ordinary_header.then_some(control & (1 << 4) != 0),
        acknowledgment_requested: interpretable_ordinary_header.then_some(control & (1 << 5) != 0),
        pan_id_compression: interpretable_ordinary_header.then_some(control & (1 << 6) != 0),
        sequence_number_suppression_bit_set: Some(suppression_bit_set),
        sequence_number_suppression_status: suppression_status,
        sequence_number_suppressed: modern_ordinary_type.then_some(suppressed),
        information_elements_bit_set: Some(information_elements_bit_set),
        information_elements_present: modern_ordinary_type.then_some(information_elements_bit_set),
        destination_address_mode: interpretable_ordinary_header
            .then_some(address_mode((control >> 10) & 0x3)),
        frame_version_code: Some(frame_version_code),
        frame_version: Some(frame_version(frame_version_code)),
        source_address_mode: interpretable_ordinary_header
            .then_some(address_mode((control >> 14) & 0x3)),
        sequence_number_status,
        sequence_number,
    }
}

fn pcap_global_header() -> [u8; 24] {
    let mut bytes = [0u8; 24];
    bytes[0..4].copy_from_slice(&0xa1b23c4du32.to_le_bytes());
    bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
    bytes[6..8].copy_from_slice(&4u16.to_le_bytes());
    bytes[16..20].copy_from_slice(&PCAP_SNAPLEN.to_le_bytes());
    bytes[20..24].copy_from_slice(&PCAP_DLT_IEEE802154_WITH_FCS.to_le_bytes());
    bytes
}

fn pcap_timestamp(chip_count: u64) -> Result<(u32, u32)> {
    let rate = u64::from(RECOVERED_CHIP_RATE_HZ);
    let seconds = chip_count / rate;
    let remainder = chip_count % rate;
    let nanoseconds = remainder
        .checked_mul(500)
        .context("relative PCAP timestamp overflow")?;
    Ok((
        u32::try_from(seconds).context("relative PCAP seconds exceed classic PCAP range")?,
        u32::try_from(nanoseconds)?,
    ))
}

fn pcap_record(chip_count: u64, psdu: &[u8]) -> Result<Vec<u8>> {
    let (seconds, nanoseconds) = pcap_timestamp(chip_count)?;
    let length = u32::try_from(psdu.len())?;
    let mut bytes = Vec::with_capacity(16 + psdu.len());
    bytes.extend_from_slice(&seconds.to_le_bytes());
    bytes.extend_from_slice(&nanoseconds.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(psdu);
    Ok(bytes)
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

fn write_summary(output: &Path, summary: &Summary, budget: &mut OutputBudget) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(summary)?;
    bytes.push(b'\n');
    let mut writer = StagedWriter::new(output, "summary.json")?;
    writer.write_budgeted(&bytes, budget)?;
    writer.finish()?;
    Ok(())
}

pub(crate) fn run(request: Request) -> Result<Summary> {
    ensure!(
        (FILE_HEADER_LEN as u64..=MAX_INPUT_BYTES).contains(&request.limits.max_input_bytes),
        "candidate input byte limit must be {FILE_HEADER_LEN}..={MAX_INPUT_BYTES}"
    );
    ensure!(
        (1..=MAX_OUTPUT_BYTES).contains(&request.limits.max_output_bytes),
        "analysis output byte limit must be 1..={MAX_OUTPUT_BYTES}"
    );
    ensure!(
        (1..=MAX_RECORDS).contains(&request.limits.max_records),
        "candidate record limit must be 1..={MAX_RECORDS}"
    );

    let file = File::open(&request.input)
        .with_context(|| format!("open candidate archive {}", request.input.display()))?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "candidate archive must be a regular file");
    ensure!(
        (FILE_HEADER_LEN as u64..=request.limits.max_input_bytes).contains(&before.len()),
        "candidate archive byte length is outside the configured limit"
    );
    let before_modified = before
        .modified()
        .context("read candidate archive modification time")?;
    let source_bytes = before.len();
    let mut reader = BufReader::with_capacity(IO_BUFFER_BYTES, file);
    let mut source_digest = Sha256::new();
    let mut file_header = [0u8; FILE_HEADER_LEN];
    read_exact_hashed(
        &mut reader,
        &mut file_header,
        &mut source_digest,
        "read candidate archive header",
    )?;
    let archive = parse_file_header(&file_header)?;
    let mut consumed = FILE_HEADER_LEN as u64;
    let mut source_index = request
        .source_index
        .map(|index| source_index::Reader::open(index, request.limits.max_records))
        .transpose()?;

    new_output_directory(&request.output)?;
    let mut budget = OutputBudget::new(request.limits.max_output_bytes);
    let mut packets = StagedWriter::new(&request.output, "packets.jsonl")?;
    let mut pcap = StagedWriter::new(&request.output, "validated.pcap")?;
    pcap.write_budgeted(&pcap_global_header(), &mut budget)?;
    let mut counters = Counters::new();
    let mut outside = OutsideCounts::default();
    let mut records_read = 0;
    let mut last_chip_count = 0;
    let mut psdu = [0u8; MAX_PSDU_BYTES];

    while consumed < source_bytes {
        ensure!(
            source_bytes - consumed >= RECORD_HEADER_LEN as u64,
            "truncated candidate record header"
        );
        ensure!(
            records_read < request.limits.max_records,
            "candidate record limit exceeded"
        );
        let mut record_header = [0u8; RECORD_HEADER_LEN];
        read_exact_hashed(
            &mut reader,
            &mut record_header,
            &mut source_digest,
            "read candidate record header",
        )?;
        consumed += RECORD_HEADER_LEN as u64;
        ensure!(
            &record_header[..4] == RECORD_MAGIC,
            "invalid candidate record magic"
        );
        let record_length = u64::from(u32_le(&record_header, 4));
        let chip_count = u64_le(&record_header, 8);
        ensure!(
            chip_count > 0,
            "candidate recovered chip count must be positive"
        );
        ensure!(
            chip_count > last_chip_count,
            "candidate recovered chip counts must be strictly increasing"
        );
        let psdu_length = usize::from(u16_le(&record_header, 16));
        ensure!(
            (1..=MAX_PSDU_BYTES).contains(&psdu_length),
            "candidate PSDU length must be 1..={MAX_PSDU_BYTES}"
        );
        ensure!(
            u16_le(&record_header, 18) == 0,
            "unknown candidate record flags"
        );
        ensure!(
            record_length == (RECORD_HEADER_LEN + psdu_length) as u64,
            "candidate record length does not match PSDU length"
        );
        ensure!(
            u64::try_from(psdu_length)? <= source_bytes - consumed,
            "truncated candidate PSDU"
        );
        read_exact_hashed(
            &mut reader,
            &mut psdu[..psdu_length],
            &mut source_digest,
            "read candidate PSDU",
        )?;
        consumed += u64::try_from(psdu_length)?;

        let candidate_index = records_read;
        records_read += 1;
        last_chip_count = chip_count;
        let candidate = &psdu[..psdu_length];
        let (relative_seconds, relative_nanoseconds) = pcap_timestamp(chip_count)?;
        let (status, received_fcs, computed_fcs) = classify_fcs(candidate);
        let required_through_channel_sample = if let Some(index) = &mut source_index {
            let required_through = index.next(chip_count)?;
            if !index.owns(required_through) {
                outside.observe(status);
                continue;
            }
            Some(required_through)
        } else {
            None
        };
        counters.records += 1;
        counters.first_chip_count.get_or_insert(chip_count);
        counters.last_chip_count = Some(chip_count);
        let mac = observe_mac(candidate);
        match status {
            FcsStatus::LengthInvalid => counters.invalid_length += 1,
            FcsStatus::Invalid => counters.invalid += 1,
            FcsStatus::Valid => {
                counters.valid += 1;
                counters.validated_bytes = counters
                    .validated_bytes
                    .checked_add(u64::try_from(candidate.len())?)
                    .context("validated PSDU byte total overflow")?;
                if let Some(kind) = mac.frame_type {
                    *counters
                        .fcs_valid_frame_type_observations
                        .entry(kind)
                        .or_default() += 1;
                }
                let record = pcap_record(chip_count, candidate)?;
                pcap.write_budgeted(&record, &mut budget)?;
            }
        }
        let packet = PacketRecord {
            candidate_index,
            recovered_chip_count_at_end: chip_count,
            required_through_channel_sample,
            relative_decoder_end_seconds: relative_seconds,
            relative_decoder_end_subsecond_nanoseconds: relative_nanoseconds,
            psdu_length,
            fcs_status: status,
            received_fcs,
            computed_fcs,
            mac,
        };
        let mut line = serde_json::to_vec(&packet)?;
        line.push(b'\n');
        packets.write_budgeted(&line, &mut budget)?;
    }

    ensure!(
        consumed == source_bytes,
        "candidate archive length changed while reading"
    );
    let mut trailing = [0u8; 1];
    ensure!(
        reader.read(&mut trailing)? == 0,
        "candidate archive grew while reading"
    );
    let after = reader.get_ref().metadata()?;
    let after_modified = after
        .modified()
        .context("read candidate archive modification time after inspection")?;
    ensure!(
        after.len() == before.len() && before_modified == after_modified,
        "candidate archive changed while reading"
    );
    let source_index = source_index
        .map(|reader| {
            reader
                .finish()
                .map(|index| SourceIndexEvidence { index, outside })
        })
        .transpose()?;

    let packets_report = packets.finish()?;
    let pcap_report = pcap.finish()?;
    let output_bytes_excluding_summary = budget.used;
    let summary = Summary {
        schema_version: if source_index.is_some() { 2 } else { 1 },
        operation: "inspect_ieee802154_candidates",
        source_format: "ieee802154_raw_candidates_v1",
        source_file: request.input,
        source_bytes,
        source_sha256: format!("{:x}", source_digest.finalize()),
        interpretation: if source_index.is_some() {
            "Candidate records are demodulator outputs. packets.jsonl includes only candidates whose required CF32 sample is in the requested half-open interval. Every archive record is independently FCS-classified, with excluded counts in source_index. Only FCS-valid candidates in the interval are copied to validated.pcap. The CF32 positions include clock recovery lookahead."
        } else {
            "Candidate records are demodulator outputs, not authenticated devices or proof of complete reception. Every candidate is classified in packets.jsonl; only independently FCS-valid PSDUs are copied to validated.pcap. Valid FCS establishes frame consistency, not Zigbee identity, decryption, sender authentication, capture continuity or channel occupancy. Basic MAC fields are conservative observations, raw frame type/version codes remain available, and version-dependent frame types or reserved frame versions are not interpreted as an ordinary MAC header."
        },
        source_index,
        ieee802154_channel: archive.channel,
        channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
        recovered_chip_rate_hz: RECOVERED_CHIP_RATE_HZ,
        candidate_record_count: counters.records,
        valid_fcs_record_count: counters.valid,
        invalid_fcs_record_count: counters.invalid,
        invalid_length_record_count: counters.invalid_length,
        validated_psdu_bytes: counters.validated_bytes,
        first_recovered_chip_count_at_end: counters.first_chip_count,
        last_recovered_chip_count_at_end: counters.last_chip_count,
        fcs_valid_frame_type_observations: counters.fcs_valid_frame_type_observations,
        packets_jsonl: packets_report,
        validated_pcap: pcap_report,
        output_bytes_excluding_summary,
        configured_max_input_bytes: request.limits.max_input_bytes,
        configured_max_output_bytes: request.limits.max_output_bytes,
        configured_max_records: request.limits.max_records,
        pcap_link_type: PCAP_DLT_IEEE802154_WITH_FCS,
        pcap_timestamp_kind: "relative recovered-chip completion time at 500 ns resolution; not host, hardware or RF time",
    };
    write_summary(&request.output, &summary, &mut budget)?;
    File::open(&request.output)?.sync_all()?;
    crate::print_summary(&summary, "Candidate inspection output", &request.output)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn archive_header(channel: u8) -> Vec<u8> {
        let mut bytes = Vec::from(*FILE_MAGIC);
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

    fn inspect_bytes(bytes: &[u8], max_records: u64) -> Result<(Summary, tempfile::TempDir)> {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("candidates.i154raw");
        fs::write(&input, bytes).unwrap();
        let output = directory.path().join("out");
        let summary = run(Request {
            input,
            output,
            limits: Limits {
                max_input_bytes: bytes.len() as u64,
                max_output_bytes: 1024 * 1024,
                max_records,
            },
            source_index: None,
        })?;
        Ok((summary, directory))
    }

    fn source_index_bytes(pairs: &[(u64, u64)]) -> Vec<u8> {
        let mut bytes = b"I154IDX\0\x01\x00\x18\x00\0\0\0\0".to_vec();
        bytes.extend_from_slice(&4_000_000u32.to_le_bytes());
        bytes.extend_from_slice(&2_000_000u32.to_le_bytes());
        for (chip_count, sample) in pairs {
            bytes.extend_from_slice(&chip_count.to_le_bytes());
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn indexed_request(
        directory: &Path,
        archive: &[u8],
        index: &[u8],
        max_records: u64,
    ) -> Request {
        let input = directory.join("candidates.i154raw");
        let file = directory.join("positions.i154idx");
        fs::write(&input, archive).unwrap();
        fs::write(&file, index).unwrap();
        Request {
            input,
            output: directory.join("out"),
            limits: Limits {
                max_input_bytes: archive.len() as u64,
                max_output_bytes: 1024 * 1024,
                max_records,
            },
            source_index: Some(source_index::Request {
                file,
                source_complex_samples: 12,
                owned_samples: 5..8,
            }),
        }
    }

    fn ack() -> Vec<u8> {
        vec![0x02, 0x00, 0x53, 0xa6, 0xd5]
    }

    fn data_header() -> Vec<u8> {
        vec![0x41, 0x88, 0x01, 0xff, 0xff, 0xff, 0xff, 0xf4, 0x84]
    }

    #[test]
    fn indexed_inspection_publishes_only_the_half_open_interval_and_audits_excluded_fcs() {
        let directory = tempfile::tempdir().unwrap();
        let ack = ack();
        let mut corrupt = ack.clone();
        corrupt[2] ^= 1;
        let mut archive = archive_header(17);
        for (chip_count, payload) in [
            (90, ack.as_slice()),
            (95, &ack),
            (100, &corrupt),
            (105, &[0]),
            (110, &ack),
            (115, &corrupt),
            (120, &[0]),
        ] {
            append_record(&mut archive, chip_count, payload, 0);
        }
        let index = source_index_bytes(&[
            (90, 4),
            (95, 5),
            (100, 5),
            (105, 7),
            (110, 8),
            (115, 9),
            (120, 10),
        ]);
        let request = indexed_request(directory.path(), &archive, &index, 7);
        let output = request.output.clone();
        let summary = serde_json::to_value(run(request).unwrap()).unwrap();
        assert_eq!(summary["schema_version"], 2);
        assert_eq!(summary["candidate_record_count"], 3);
        assert_eq!(summary["valid_fcs_record_count"], 1);
        assert_eq!(summary["invalid_fcs_record_count"], 1);
        assert_eq!(summary["invalid_length_record_count"], 1);
        assert_eq!(summary["validated_psdu_bytes"], ack.len());
        assert_eq!(summary["first_recovered_chip_count_at_end"], 95);
        assert_eq!(summary["last_recovered_chip_count_at_end"], 105);
        assert_eq!(
            summary["source_index"],
            serde_json::json!({
                "file": directory.path().join("positions.i154idx"),
                "bytes": index.len(),
                "sha256": format!("{:x}", Sha256::digest(&index)),
                "source_complex_samples": 12,
                "owned_start_channel_sample": 5,
                "owned_end_channel_sample": 8,
                "indexed_record_count": 7,
                "outside_record_count": 4,
                "outside_valid_fcs_record_count": 2,
                "outside_invalid_fcs_record_count": 1,
                "outside_invalid_length_record_count": 1,
            })
        );
        let rows = fs::read_to_string(output.join("packets.jsonl")).unwrap();
        let candidates: Vec<_> = rows
            .lines()
            .map(|row| {
                let row: serde_json::Value = serde_json::from_str(row).unwrap();
                (
                    row["candidate_index"].as_u64().unwrap(),
                    row["required_through_channel_sample"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(candidates, [(1, 5), (2, 5), (3, 7)]);
        assert_eq!(
            fs::read(output.join("validated.pcap")).unwrap(),
            [
                pcap_global_header().as_slice(),
                &pcap_record(95, &ack).unwrap()
            ]
            .concat()
        );
    }

    #[test]
    fn rejects_malformed_or_mismatched_indexes_before_publishing_outputs() {
        let mut archive = archive_header(17);
        append_record(&mut archive, 90, &ack(), 0);
        append_record(&mut archive, 95, &ack(), 0);
        let valid = source_index_bytes(&[(90, 4), (95, 5)]);
        let mut bad_header = valid.clone();
        bad_header[12] = 1;
        for (index, limit, error) in [
            (bad_header, 2, "invalid I154IDX v1 header"),
            (
                valid[..valid.len() - 1].to_vec(),
                2,
                "byte length is invalid",
            ),
            (source_index_bytes(&[(90, 4)]), 2, "fewer records"),
            (
                source_index_bytes(&[(90, 4), (95, 5), (99, 6)]),
                3,
                "more records",
            ),
            (
                source_index_bytes(&[(90, 4), (96, 5)]),
                2,
                "chip count does not match",
            ),
            (source_index_bytes(&[(90, 5), (95, 4)]), 2, "nondecreasing"),
            (
                source_index_bytes(&[(90, 4), (95, 12)]),
                2,
                "within the CF32 input",
            ),
            (valid, 1, "exceeds the record limit"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let request = indexed_request(directory.path(), &archive, &index, limit);
            let output = request.output.clone();
            assert!(
                run(request).unwrap_err().to_string().contains(error),
                "{error}"
            );
            for filename in ["summary.json", "packets.jsonl", "validated.pcap"] {
                assert!(!output.join(filename).exists());
            }
        }
    }

    #[test]
    fn source_index_rejects_symlinks_and_replacement_at_the_same_path() {
        let directory = tempfile::tempdir().unwrap();
        let index = source_index_bytes(&[]);
        let request = indexed_request(directory.path(), &archive_header(17), &index, 1);
        let index_request = request.source_index.unwrap();
        let path = index_request.file.clone();
        let reader = source_index::Reader::open(index_request, 1).unwrap();
        let displaced = path.with_extension("displaced");
        fs::rename(&path, &displaced).unwrap();
        fs::write(&path, index).unwrap();
        assert!(reader.finish().unwrap_err().to_string().contains("changed"));

        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(displaced, &path).unwrap();
        let index_request = source_index::Request {
            file: path,
            source_complex_samples: 12,
            owned_samples: 5..8,
        };
        let error = source_index::Reader::open(index_request, 1).unwrap_err();
        assert!(error.to_string().contains("direct regular file"));
    }

    #[test]
    fn archive_layout_and_fcs_vectors_are_exact() {
        let header = archive_header(17);
        assert_eq!(header.len(), FILE_HEADER_LEN);
        assert_eq!(&header[0..8], FILE_MAGIC);
        assert_eq!(u16_le(&header, 8), VERSION);
        assert_eq!(u16_le(&header, 10), FILE_HEADER_LEN as u16);
        assert_eq!(header[12], 17);
        assert_eq!(u32_le(&header, 16), CHANNEL_SAMPLE_RATE_HZ);
        assert_eq!(u32_le(&header, 20), RECOVERED_CHIP_RATE_HZ);
        assert_eq!(dot15d4_fcs(&[0x02, 0x00, 0x53]), 0xd5a6);
        assert_eq!(
            dot15d4_fcs(&[0x41, 0x88, 0x01, 0xff, 0xff, 0xff, 0xff]),
            0x84f4
        );
        assert_eq!(classify_fcs(&ack()).0, FcsStatus::Valid);
        assert_eq!(classify_fcs(&data_header()).0, FcsStatus::Valid);
        assert_eq!(dot15d4_fcs(&ack()), 0);
        assert_eq!(dot15d4_fcs(&data_header()), 0);

        let mut record = Vec::new();
        append_record(&mut record, 0x0102_0304_0506_0708, &ack(), 0);
        assert_eq!(record.len(), RECORD_HEADER_LEN + ack().len());
        assert_eq!(&record[0..4], RECORD_MAGIC);
        assert_eq!(u32_le(&record, 4), (RECORD_HEADER_LEN + ack().len()) as u32);
        assert_eq!(u64_le(&record, 8), 0x0102_0304_0506_0708);
        assert_eq!(u16_le(&record, 16), ack().len() as u16);
        assert_eq!(u16_le(&record, 18), 0);
        assert_eq!(&record[20..], ack());
        let mut corrupt = ack();
        corrupt[2] ^= 1;
        assert_eq!(classify_fcs(&corrupt).0, FcsStatus::Invalid);
        assert_eq!(classify_fcs(&[0]).0, FcsStatus::LengthInvalid);
    }

    #[test]
    fn classifies_all_candidates_and_writes_relative_dlt195_pcap() {
        let mut bytes = archive_header(17);
        append_record(&mut bytes, 2_000_000, &ack(), 0);
        let mut corrupt = data_header();
        corrupt[0] ^= 1;
        append_record(&mut bytes, 2_000_001, &corrupt, 0);
        append_record(&mut bytes, 2_500_001, &[0x42], 0);
        let (summary, directory) = inspect_bytes(&bytes, 3).unwrap();
        assert_eq!(summary.candidate_record_count, 3);
        assert_eq!(summary.valid_fcs_record_count, 1);
        assert_eq!(summary.invalid_fcs_record_count, 1);
        assert_eq!(summary.invalid_length_record_count, 1);
        assert_eq!(
            summary.fcs_valid_frame_type_observations["acknowledgment"],
            1
        );

        let output = directory.path().join("out");
        let pcap = fs::read(output.join("validated.pcap")).unwrap();
        assert_eq!(&pcap[..24], &pcap_global_header());
        assert_eq!(&pcap[..4], &[0x4d, 0x3c, 0xb2, 0xa1]);
        assert_eq!(u32_le(&pcap, 20), PCAP_DLT_IEEE802154_WITH_FCS);
        assert_eq!(u32_le(&pcap, 24), 1);
        assert_eq!(u32_le(&pcap, 28), 0);
        assert_eq!(u32_le(&pcap, 32), ack().len() as u32);
        assert_eq!(u32_le(&pcap, 36), ack().len() as u32);
        assert_eq!(&pcap[40..], ack());

        let lines = fs::read_to_string(output.join("packets.jsonl")).unwrap();
        let packets: Vec<serde_json::Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0]["fcs_status"], "valid");
        assert_eq!(packets[1]["fcs_status"], "invalid");
        assert_eq!(packets[2]["fcs_status"], "length_invalid");
        assert_eq!(packets[0]["mac"]["sequence_number"], 0x53);
        assert_eq!(packets[0]["relative_decoder_end_seconds"], 1);
        assert_eq!(packets[0]["relative_decoder_end_subsecond_nanoseconds"], 0);

        let truncated = observe_mac(&[0x02, 0x00, 0x00, 0x00]);
        assert_eq!(truncated.status, "truncated_sequence_number");
        assert_eq!(truncated.sequence_number_status, "truncated");

        let old_version_reserved_bit = observe_mac(&[0x02, 0x01, 0x53, 0x00, 0x00]);
        assert_eq!(
            old_version_reserved_bit.status,
            "observed_with_reserved_sequence_suppression_bit"
        );
        assert_eq!(
            old_version_reserved_bit.sequence_number_suppression_status,
            "reserved_bit_set_for_frame_version"
        );
        assert_eq!(old_version_reserved_bit.sequence_number_suppressed, None);
        assert_eq!(old_version_reserved_bit.sequence_number_status, "present");
        assert_eq!(old_version_reserved_bit.sequence_number, Some(0x53));

        let old_version_ie_bit = observe_mac(&[0x02, 0x02, 0x53, 0x00, 0x00]);
        assert_eq!(old_version_ie_bit.information_elements_bit_set, Some(true));
        assert_eq!(old_version_ie_bit.information_elements_present, None);
        assert_eq!(
            old_version_ie_bit.status,
            "observed_with_reserved_information_element_bit"
        );

        let suppressed = observe_mac(&[0x02, 0x21, 0x00, 0x00]);
        assert_eq!(suppressed.status, "observed");
        assert_eq!(suppressed.frame_version_code, Some(2));
        assert_eq!(suppressed.sequence_number_suppressed, Some(true));
        assert_eq!(suppressed.sequence_number_status, "suppressed");
        assert_eq!(suppressed.sequence_number, None);

        let modern_ie_bit = observe_mac(&[0x02, 0x22, 0x53, 0x00, 0x00]);
        assert_eq!(modern_ie_bit.information_elements_bit_set, Some(true));
        assert_eq!(modern_ie_bit.information_elements_present, Some(true));

        let reserved_version = observe_mac(&[0x02, 0x30, 0x53, 0x00, 0x00]);
        assert_eq!(reserved_version.frame_version_code, Some(3));
        assert_eq!(
            reserved_version.status,
            "frame_control_only_reserved_frame_version"
        );
        assert_eq!(reserved_version.security_enabled, None);
        assert_eq!(reserved_version.destination_address_mode, None);
        assert_eq!(reserved_version.sequence_number_suppressed, None);
        assert_eq!(
            reserved_version.sequence_number_status,
            "unavailable_for_reserved_frame_version"
        );
        assert_eq!(reserved_version.sequence_number, None);

        let version_dependent = observe_mac(&[0x05, 0x00, 0x53, 0x00, 0x00]);
        assert_eq!(version_dependent.frame_type_code, Some(5));
        assert_eq!(
            version_dependent.frame_type,
            Some("version_dependent_or_reserved")
        );
        assert_eq!(
            version_dependent.status,
            "frame_control_only_version_dependent_or_reserved_type"
        );
        assert_eq!(
            version_dependent.sequence_number_status,
            "unavailable_for_frame_type"
        );
        assert_eq!(version_dependent.sequence_number, None);
    }

    #[test]
    fn rejects_bad_headers_records_positions_and_limits() {
        for offset in [0usize, 8, 10, 12, 13, 14, 16, 20] {
            let mut bytes = archive_header(17);
            bytes[offset] ^= 0xff;
            assert!(inspect_bytes(&bytes, 1).is_err());
        }

        let mut bad_magic = archive_header(17);
        append_record(&mut bad_magic, 1, &ack(), 0);
        bad_magic[FILE_HEADER_LEN] ^= 0xff;
        assert!(inspect_bytes(&bad_magic, 1).is_err());

        let mut bad_length = archive_header(17);
        append_record(&mut bad_length, 1, &ack(), 0);
        bad_length[FILE_HEADER_LEN + 4..FILE_HEADER_LEN + 8]
            .copy_from_slice(&(RECORD_HEADER_LEN as u32).to_le_bytes());
        assert!(inspect_bytes(&bad_length, 1).is_err());

        let mut bad_flags = archive_header(17);
        append_record(&mut bad_flags, 1, &ack(), 1);
        assert!(inspect_bytes(&bad_flags, 1).is_err());

        let mut zero_position = archive_header(17);
        append_record(&mut zero_position, 0, &ack(), 0);
        assert!(inspect_bytes(&zero_position, 1).is_err());

        let mut zero_length = archive_header(17);
        zero_length.extend_from_slice(RECORD_MAGIC);
        zero_length.extend_from_slice(&(RECORD_HEADER_LEN as u32).to_le_bytes());
        zero_length.extend_from_slice(&1u64.to_le_bytes());
        zero_length.extend_from_slice(&0u16.to_le_bytes());
        zero_length.extend_from_slice(&0u16.to_le_bytes());
        assert!(inspect_bytes(&zero_length, 1).is_err());

        let mut oversized = archive_header(17);
        append_record(&mut oversized, 1, &[0; MAX_PSDU_BYTES + 1], 0);
        assert!(inspect_bytes(&oversized, 1).is_err());

        let mut nonmonotonic = archive_header(17);
        append_record(&mut nonmonotonic, 2, &ack(), 0);
        append_record(&mut nonmonotonic, 2, &ack(), 0);
        assert!(inspect_bytes(&nonmonotonic, 2).is_err());

        let mut truncated = archive_header(17);
        append_record(&mut truncated, 1, &ack(), 0);
        truncated.pop();
        assert!(inspect_bytes(&truncated, 1).is_err());

        let mut partial_header = archive_header(17);
        partial_header.extend_from_slice(RECORD_MAGIC);
        assert!(inspect_bytes(&partial_header, 1).is_err());

        let mut limited = archive_header(17);
        append_record(&mut limited, 1, &ack(), 0);
        append_record(&mut limited, 2, &ack(), 0);
        assert!(inspect_bytes(&limited, 1).is_err());
    }

    #[test]
    fn enforces_regular_files_and_output_budget() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            run(Request {
                input: directory.path().to_path_buf(),
                output: directory.path().join("out"),
                limits: Limits {
                    max_input_bytes: 1024,
                    max_output_bytes: 1024,
                    max_records: 1,
                },
                source_index: None,
            })
            .is_err()
        );
        let mut budget = OutputBudget::new(5);
        budget.consume(5).unwrap();
        assert!(budget.consume(1).is_err());
        assert!(Limits::from_cli(0, 1, 1).is_err());
        assert!(Limits::from_cli(8193, 1, 1).is_err());
        assert!(Limits::from_cli(1, 0, 1).is_err());
        assert!(Limits::from_cli(1, 8193, 1).is_err());
        assert!(Limits::from_cli(1, 1, 0).is_err());
        assert!(Limits::from_cli(1, 1, MAX_RECORDS + 1).is_err());

        let mut bytes = archive_header(17);
        append_record(&mut bytes, 1, &ack(), 0);
        let input = directory.path().join("bounded.i154raw");
        fs::write(&input, &bytes).unwrap();
        assert!(
            run(Request {
                input: input.clone(),
                output: directory.path().join("input-too-large"),
                limits: Limits {
                    max_input_bytes: bytes.len() as u64 - 1,
                    max_output_bytes: 1024,
                    max_records: 1,
                },
                source_index: None,
            })
            .is_err()
        );

        let output = directory.path().join("output-too-large");
        assert!(
            run(Request {
                input: input.clone(),
                output: output.clone(),
                limits: Limits {
                    max_input_bytes: bytes.len() as u64,
                    max_output_bytes: 23,
                    max_records: 1,
                },
                source_index: None,
            })
            .is_err()
        );
        assert!(output.join("packets.jsonl.partial").exists());
        assert!(output.join("validated.pcap.partial").exists());

        let successful = run(Request {
            input: input.clone(),
            output: directory.path().join("measure-output"),
            limits: Limits {
                max_input_bytes: bytes.len() as u64,
                max_output_bytes: 1024 * 1024,
                max_records: 1,
            },
            source_index: None,
        })
        .unwrap();
        let summary_limited_output = directory.path().join("summary-too-large");
        assert!(
            run(Request {
                input,
                output: summary_limited_output.clone(),
                limits: Limits {
                    max_input_bytes: bytes.len() as u64,
                    max_output_bytes: successful.output_bytes_excluding_summary,
                    max_records: 1,
                },
                source_index: None,
            })
            .is_err()
        );
        assert!(summary_limited_output.join("packets.jsonl").exists());
        assert!(summary_limited_output.join("validated.pcap").exists());
        assert!(summary_limited_output.join("summary.json.partial").exists());
        assert!(!summary_limited_output.join("summary.json").exists());
    }

    #[test]
    fn publishes_private_deterministic_outputs_without_overwrite() {
        let mut bytes = archive_header(18);
        append_record(&mut bytes, 1_000_001, &ack(), 0);
        append_record(&mut bytes, 1_000_003, &data_header(), 0);
        let (first, directory) = inspect_bytes(&bytes, 2).unwrap();
        let first_output = directory.path().join("out");
        assert_eq!(
            fs::metadata(&first_output).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["packets.jsonl", "validated.pcap", "summary.json"] {
            assert_eq!(
                fs::metadata(first_output.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert!(!first_output.join(format!("{name}.partial")).exists());
        }

        let input = directory.path().join("candidates.i154raw");
        let second_output = directory.path().join("second");
        let second = run(Request {
            input: input.clone(),
            output: second_output,
            limits: Limits {
                max_input_bytes: bytes.len() as u64,
                max_output_bytes: 1024 * 1024,
                max_records: 2,
            },
            source_index: None,
        })
        .unwrap();
        assert_eq!(first.source_sha256, second.source_sha256);
        assert_eq!(first.packets_jsonl.sha256, second.packets_jsonl.sha256);
        assert_eq!(first.validated_pcap.sha256, second.validated_pcap.sha256);
        let summary_before = fs::read(first_output.join("summary.json")).unwrap();
        assert!(
            run(Request {
                input,
                output: first_output.clone(),
                limits: Limits {
                    max_input_bytes: bytes.len() as u64,
                    max_output_bytes: 1024 * 1024,
                    max_records: 2,
                },
                source_index: None,
            })
            .is_err()
        );
        assert_eq!(
            fs::read(first_output.join("summary.json")).unwrap(),
            summary_before
        );
    }

    #[test]
    fn pcap_timestamp_bounds_are_explicit() {
        assert_eq!(pcap_timestamp(2_500_000).unwrap(), (1, 250_000_000));
        let too_large = (u64::from(u32::MAX) + 1) * u64::from(RECOVERED_CHIP_RATE_HZ);
        assert!(pcap_timestamp(too_large).is_err());
    }
}
