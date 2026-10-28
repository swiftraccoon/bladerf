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

use anyhow::{Result, ensure};
use serde::Serialize;

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
    pub(crate) records: u64,
    profile_id: u16,
    profile_source_sample_rate_hz: u32,
    transport_complex_slot_rate_hz: u32,
    channel_sample_rate_hz: u32,
    phase_words: [u32; 4],
    first_source_index: Option<u64>,
    last_source_index: Option<u64>,
    receiver: Option<&'static str>,
    input_truncated_to_limit: bool,
    interpretation: &'static str,
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
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() % RECORD_BYTES as u64 == 0,
        "custom stream must be a regular file of complete records"
    );
    let available = metadata.len() / RECORD_BYTES as u64;
    ensure!(available > 0, "empty custom stream");
    let mut input = BufReader::new(File::open(path)?);
    let mut posts = (0..4)
        .map(|_| HostPost::new(1_000_000))
        .collect::<Result<Vec<_>>>()?;
    let mut files = (0..4)
        .map(|i| ChannelFiles::new(directory, i))
        .collect::<Result<Vec<_>>>()?;
    let mut first = None;
    let mut previous: Option<u64> = None;
    let mut receiver = None;
    let records = available.min(max_records);
    for _ in 0..records {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "custom stream decoding interrupted; partial output retained"
        );
        let mut raw = [0u8; RECORD_BYTES];
        input.read_exact(&mut raw)?;
        let frame = parse(&raw)?;
        if let Some(index) = previous {
            ensure!(
                Some(frame.source_index) == index.checked_add(20),
                "source index gap/reorder; refusing continuous-output claim"
            );
        }
        if let Some(rx2) = receiver {
            ensure!(
                rx2 == frame.rx2,
                "receive input changed within custom stream"
            );
        }
        previous = Some(frame.source_index);
        receiver = Some(frame.rx2);
        first.get_or_insert(frame.source_index);
        for channel in 0..4 {
            if let Some(post) = posts[channel].push(frame.samples[channel]) {
                files[channel].write(post)?;
            }
        }
    }
    for file in files {
        file.finish()?;
    }
    Ok(Summary {
        records,
        profile_id: PROFILE_ID,
        profile_source_sample_rate_hz: crate::SAMPLE_RATE,
        transport_complex_slot_rate_hz: 8_000_000,
        channel_sample_rate_hz: 1_000_000,
        phase_words: PROFILE_WORDS,
        first_source_index: first,
        last_source_index: previous,
        receiver: receiver.map(|rx2| if rx2 { "RX2" } else { "RX1" }),
        input_truncated_to_limit: available > max_records,
        interpretation: "Parsed profile-1 transport with exact source-index progression and no asserted error flags. The 20 MSps source clock is a profile assumption, not measured by framing. This alone does not prove RF continuity, fitted timing, or hardware execution.",
    })
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
}
