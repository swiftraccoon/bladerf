//! Strict decoder for profile-1 custom gateware's framed SC16 transport.
//!
//! One record is eight complex i16 slots (32 bytes), not eight RF samples.
//! The stream rate is 8 Mslots/s for a 20 MSps source. Native Q11 assumptions
//! must not be applied to header words. Each payload holds four 1 MSps channels.
//! Sample indexes count accepted source samples; sticky ADC-loss flags are
//! needed to detect loss before that counter. This format is not metadata mode.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::dsp::{HostPost, Iq16};
use crate::receiver::ChannelFiles;

pub(crate) const RECORD_BYTES: usize = 32;
pub(crate) const PROFILE_ID: u16 = 1;
pub(crate) const PROFILE_WORDS: [u32; 4] = [0xb3333333, 0xe6666666, 0x1999999a, 0x4ccccccd];

#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub(crate) source_index: u64,
    pub(crate) rx2: bool,
    pub(crate) samples: [Iq16; 4],
}

fn word(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([raw[offset], raw[offset + 1]])
}

pub(crate) fn parse(raw: &[u8]) -> Result<Frame> {
    ensure!(
        raw.len() == RECORD_BYTES,
        "custom stream record must be 32 bytes"
    );
    ensure!(
        word(raw, 0) == 0x5246 && word(raw, 2) == 0x4f42,
        "custom stream magic mismatch; ordinary hosted IQ is not this format"
    );
    let flags = word(raw, 4);
    ensure!(
        flags & 0xf000 == 0x1000 && flags & 0x001f == 0,
        "unsupported transport version or reserved flags"
    );
    ensure!(word(raw, 6) == PROFILE_ID, "unsupported gateware profile");
    ensure!(
        flags & 0x0f00 == 0,
        "FPGA reports saturated channel samples"
    );
    ensure!(
        flags & 0x0080 == 0,
        "FPGA reports lost ADC samples or transport backpressure"
    );
    ensure!(
        flags & 0x0040 == 0,
        "FPGA reports invalid receive configuration"
    );
    let source_index = u64::from_le_bytes(raw[8..16].try_into()?);
    ensure!(
        source_index % 20 == 19,
        "invalid decimation phase in source index"
    );
    let samples = std::array::from_fn(|channel| Iq16 {
        i: word(raw, 16 + channel * 4) as i16,
        q: word(raw, 18 + channel * 4) as i16,
    });
    Ok(Frame {
        source_index,
        rx2: flags & 0x0020 != 0,
        samples,
    })
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    pub(crate) records: u64,
    available_records: u64,
    processed_bytes: u64,
    processed_sha256: String,
    profile_id: u16,
    profile_source_sample_rate_hz: u32,
    transport_complex_slot_rate_hz: u32,
    channel_sample_rate_hz: u32,
    phase_words: [u32; 4],
    first_source_index: Option<u64>,
    last_source_index: Option<u64>,
    receiver: Option<&'static str>,
    input_truncated_to_limit: bool,
    mode: &'static str,
    interpretation: &'static str,
}

impl Summary {
    pub(crate) fn receiver(&self) -> Option<&'static str> {
        self.receiver
    }

    pub(crate) fn first_source_index(&self) -> Option<u64> {
        self.first_source_index
    }

    pub(crate) fn last_source_index(&self) -> Option<u64> {
        self.last_source_index
    }
}

struct Scan {
    available: u64,
    records: u64,
    first: Option<u64>,
    previous: Option<u64>,
    receiver: Option<bool>,
    hash: Sha256,
}

impl Scan {
    fn summary(self, mode: &'static str) -> Summary {
        Summary {
            schema_version: 1,
            records: self.records,
            available_records: self.available,
            processed_bytes: self.records * RECORD_BYTES as u64,
            processed_sha256: format!("{:x}", self.hash.finalize()),
            profile_id: PROFILE_ID,
            profile_source_sample_rate_hz: crate::SAMPLE_RATE,
            transport_complex_slot_rate_hz: 8_000_000,
            channel_sample_rate_hz: 1_000_000,
            phase_words: PROFILE_WORDS,
            first_source_index: self.first,
            last_source_index: self.previous,
            receiver: self.receiver.map(|rx2| if rx2 { "RX2" } else { "RX1" }),
            input_truncated_to_limit: self.records < self.available,
            mode,
            interpretation: "Parsed profile-1 transport with exact source-index progression and no asserted error flags. The 20 MSps source clock is a profile assumption, not measured by framing. This alone does not prove RF continuity, fitted timing, or hardware execution.",
        }
    }
}

fn available_records(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() % RECORD_BYTES as u64 == 0,
        "custom stream must be a regular file of complete records"
    );
    let available = metadata.len() / RECORD_BYTES as u64;
    ensure!(available > 0, "empty custom stream");
    Ok(available)
}

fn scan(
    path: &Path,
    record_limit: Option<u64>,
    stop: &AtomicBool,
    mut consume: impl FnMut(u64, u64, Frame) -> Result<()>,
) -> Result<Scan> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() % RECORD_BYTES as u64 == 0,
        "custom stream must be a regular file of complete records"
    );
    let initial_bytes = metadata.len();
    let available = initial_bytes / RECORD_BYTES as u64;
    ensure!(available > 0, "empty custom stream");
    let records = record_limit.map_or(available, |limit| available.min(limit));
    let mut input = BufReader::new(file);
    let mut first = None;
    let mut previous: Option<u64> = None;
    let mut receiver = None;
    let mut hash = Sha256::new();
    for record in 0..records {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "custom stream processing interrupted; partial output retained"
        );
        let mut raw = [0u8; RECORD_BYTES];
        input
            .read_exact(&mut raw)
            .with_context(|| format!("read custom stream record {record}"))?;
        let frame =
            parse(&raw).with_context(|| format!("validate custom stream record {record}"))?;
        if let Some(index) = previous {
            ensure!(
                Some(frame.source_index) == index.checked_add(20),
                "source index gap/reorder at record {record}; refusing continuous-output claim"
            );
        }
        if let Some(rx2) = receiver {
            ensure!(
                rx2 == frame.rx2,
                "receive input changed at record {record} within custom stream"
            );
        }
        consume(record, records, frame)?;
        hash.update(raw);
        previous = Some(frame.source_index);
        receiver = Some(frame.rx2);
        first.get_or_insert(frame.source_index);
    }
    if records == available {
        let mut trailing = [0u8; 1];
        ensure!(
            input.read(&mut trailing)? == 0,
            "custom stream grew while it was being processed"
        );
    }
    ensure!(
        input.get_ref().metadata()?.len() == initial_bytes,
        "custom stream extent changed while it was being processed"
    );
    Ok(Scan {
        available,
        records,
        first,
        previous,
        receiver,
        hash,
    })
}

pub(crate) fn decode(
    path: &Path,
    directory: &Path,
    max_records: u64,
    stop: &AtomicBool,
) -> Result<Summary> {
    ensure!(
        (1..=1_000_000).contains(&max_records),
        "invalid record limit"
    );
    // Refuse obviously malformed inputs before creating any derived files.
    // scan() repeats the check on the descriptor it actually consumes so a
    // concurrent input replacement cannot bypass validation.
    available_records(path)?;
    let mut posts = (0..4)
        .map(|_| HostPost::new(1_000_000))
        .collect::<Result<Vec<_>>>()?;
    let mut files = (0..4)
        .map(|i| ChannelFiles::new(directory, i))
        .collect::<Result<Vec<_>>>()?;
    let scan = scan(path, Some(max_records), stop, |_, _, frame| {
        for channel in 0..4 {
            if let Some(post) = posts[channel].push(frame.samples[channel]) {
                files[channel].write(post)?;
            }
        }
        Ok(())
    })?;
    for file in files {
        file.finish()?;
    }
    Ok(scan.summary("decode_and_derive"))
}

/// Validate every complete record without allocating or publishing channel outputs.
pub(crate) fn validate(path: &Path, stop: &AtomicBool) -> Result<Summary> {
    Ok(scan(path, None, stop, |_, _, _| Ok(()))?.summary("validate_only"))
}

/// Visit a complete stream or an explicitly bounded prefix after validating
/// each consumed frame. The callback receives the zero-based record number and
/// the number of records selected from the descriptor opened by this scan.
pub(crate) fn inspect(
    path: &Path,
    max_records: Option<u64>,
    stop: &AtomicBool,
    consume: impl FnMut(u64, u64, Frame) -> Result<()>,
) -> Result<Summary> {
    if let Some(limit) = max_records {
        ensure!(limit > 0, "record limit must be positive");
    }
    Ok(scan(path, max_records, stop, consume)?.summary("relative_analysis"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(index: u64, flags: u16) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..2].copy_from_slice(&0x5246u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&0x4f42u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&(0x1000 | flags).to_le_bytes());
        bytes[6..8].copy_from_slice(&PROFILE_ID.to_le_bytes());
        bytes[8..16].copy_from_slice(&index.to_le_bytes());
        bytes[16..18].copy_from_slice(&(-1234i16).to_le_bytes());
        bytes
    }
    #[test]
    fn independent_header_layout_flags_and_sequence_are_enforced() {
        let good = frame(19, 0x20);
        let parsed = parse(&good).unwrap();
        assert_eq!(parsed.source_index, 19);
        assert!(parsed.rx2);
        assert_eq!(parsed.samples[0].i, -1234);
        for flags in [1, 0x40, 0x80, 0x100] {
            assert!(parse(&frame(19, flags)).is_err());
        }
        assert!(parse(&frame(20, 0)).is_err());
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("records.raw");
        let mut raw = Vec::from(frame(19, 0));
        raw.extend_from_slice(&frame(59, 0));
        std::fs::write(&input, raw).unwrap();
        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();
        assert!(decode(&input, &output, 100, &AtomicBool::new(false)).is_err());
        assert!(!output.join("channel-0.wav").exists());
    }

    #[test]
    fn validation_has_no_derived_outputs_or_one_million_record_limit() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("long-records.raw");
        let mut output = std::io::BufWriter::new(File::create(&input).unwrap());
        for record in 0..=1_000_000u64 {
            std::io::Write::write_all(&mut output, &frame(record * 20 + 19, 0x20)).unwrap();
        }
        std::io::Write::flush(&mut output).unwrap();
        drop(output);

        let summary = validate(&input, &AtomicBool::new(false)).unwrap();
        assert_eq!(summary.records, 1_000_001);
        assert_eq!(summary.available_records, 1_000_001);
        assert_eq!(summary.processed_bytes, 1_000_001 * RECORD_BYTES as u64);
        assert_eq!(summary.first_source_index, Some(19));
        assert_eq!(summary.last_source_index, Some(20_000_019));
        assert_eq!(summary.receiver, Some("RX2"));
        assert!(!summary.input_truncated_to_limit);
        assert_eq!(summary.mode, "validate_only");
        assert_eq!(summary.processed_sha256.len(), 64);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn malformed_extent_creates_no_derived_files() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("truncated.raw");
        std::fs::write(&input, [0u8; RECORD_BYTES - 1]).unwrap();
        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();

        assert!(decode(&input, &output, 100, &AtomicBool::new(false)).is_err());
        assert_eq!(std::fs::read_dir(output).unwrap().count(), 0);
    }

    #[test]
    fn explicit_prefix_rejects_extent_changes_on_the_open_descriptor() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("growing.raw");
        let mut raw = Vec::from(frame(19, 0));
        raw.extend_from_slice(&frame(39, 0));
        std::fs::write(&input, raw).unwrap();

        let result = inspect(&input, Some(1), &AtomicBool::new(false), |_, _, _| {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&input)?
                .write_all(&frame(59, 0))?;
            Ok(())
        });
        assert!(
            format!("{:#}", result.unwrap_err())
                .contains("extent changed while it was being processed")
        );
    }

    #[test]
    fn summary_hash_and_schema_are_stable_for_a_known_record() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("one-record.raw");
        std::fs::write(&input, frame(19, 0)).unwrap();

        let summary = validate(&input, &AtomicBool::new(false)).unwrap();
        assert_eq!(summary.schema_version, 1);
        assert_eq!(
            summary.processed_sha256,
            "2218bf4c1cd5ce66508b70649eca222ad2a0226c363328ced9b2f690588afb82"
        );
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["schema_version"], 1);
    }
}
