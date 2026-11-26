//! Match private packet observations produced by the bounded offline decoders.
//!
//! The caller checks each replay's provenance, sample extent and integrity
//! summary before loading packets. This module retains in-memory keys, packet
//! positions, and validated BLE frequencies for aggregate reporting.

use std::cmp::Ordering;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{MAX_VALIDATED_PACKETS, Protocol};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 1024 * 1024;
const MAX_RECORDS_PER_FILE: u64 = 100_000;
const MAX_VALID_PER_FILE: u64 = 10_000;
const WIFI_RADIOTAP_HEX: &str = "000009000200000010";
const MAX_WIFI_RECORD_BYTES: usize = 65_536;
const MAX_BLE_BODY_BYTES: usize = 4 + 1 + 3 + 255 + 3;

#[derive(Clone)]
pub(super) struct Packet {
    pub(super) sample: u64,
    pub(super) record_index: u64,
    pub(super) ble_frequency_mhz: Option<u16>,
    key: [u8; 32],
}

#[derive(Debug, Default, Serialize)]
pub(super) struct Matches {
    pub(super) matched: u64,
    pub(super) delta_samples_min: Option<i64>,
    pub(super) delta_samples_max: Option<i64>,
}

#[derive(Deserialize)]
struct Record<'a> {
    index: u64,
    validation: &'a str,
    timestamp_precision: &'a str,
    timestamp_seconds: u32,
    timestamp_fraction: u32,
    #[serde(borrow)]
    packet_hex: Option<&'a str>,
    #[serde(borrow)]
    ble_dewhitened_hex: Option<&'a str>,
    frequency_mhz: Option<u16>,
    upstream_phy_bits: Option<u8>,
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.is_file()
        && right.is_file()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn open_packets(path: &Path) -> Result<(File, Metadata)> {
    let pinned = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_PATH | nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("pin packet JSONL {}", path.display()))?;
    let metadata = pinned.metadata().context("inspect pinned packet JSONL")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_FILE_BYTES,
        "packet JSONL must be a direct regular file within its byte limit"
    );
    let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))
        .context("open pinned packet JSONL for reading")?;
    ensure!(
        same_file(&metadata, &file.metadata()?),
        "packet JSONL changed before reading"
    );
    Ok((file, metadata))
}

fn source_sample(record: &Record<'_>, protocol: Protocol, start: u64, samples: u64) -> Result<u64> {
    let fraction = u64::from(record.timestamp_fraction);
    let offset = match protocol {
        Protocol::Wifi => {
            ensure!(
                record.timestamp_precision == "nanoseconds"
                    && fraction < 1_000_000_000
                    && fraction.is_multiple_of(50),
                "Wi-Fi packet timestamp must be on the 20 MSps nanosecond grid"
            );
            fraction / 50
        }
        Protocol::Bluetooth => {
            ensure!(
                record.timestamp_precision == "microseconds" && fraction < 1_000_000,
                "Bluetooth packet timestamp must use classic PCAP microseconds"
            );
            fraction * 20
        }
    };
    let local = u64::from(record.timestamp_seconds) * SAMPLE_RATE_HZ + offset;
    ensure!(
        local < samples,
        "validated packet position is outside its IQ extent"
    );
    start
        .checked_add(local)
        .context("rebased packet position overflow")
}

fn canonical_hex(body: &str, min_bytes: usize, max_bytes: usize) -> bool {
    body.len().is_multiple_of(2)
        && (min_bytes..=max_bytes).contains(&(body.len() / 2))
        && body
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn validated_key(record: &Record<'_>, protocol: Protocol) -> Result<Option<[u8; 32]>> {
    let mut digest = Sha256::new();
    match (protocol, record.validation) {
        (Protocol::Wifi, "fcs_valid") => {
            let body = record
                .packet_hex
                .and_then(|hex| hex.strip_prefix(WIFI_RADIOTAP_HEX))
                .context("validated Wi-Fi record lacks the offline decoder's radiotap header")?;
            ensure!(
                canonical_hex(body, 6, MAX_WIFI_RECORD_BYTES - WIFI_RADIOTAP_HEX.len() / 2),
                "invalid validated Wi-Fi MPDU encoding or length"
            );
            digest.update(b"wifi-mpdu:");
            digest.update(body);
        }
        (Protocol::Bluetooth, "advertising_crc_valid" | "connection_crc_valid") => {
            let frequency = record
                .frequency_mhz
                .context("validated BLE record lacks frequency")?;
            let phy = record
                .upstream_phy_bits
                .context("validated BLE record lacks PHY")?;
            ensure!(
                (2402..=2480).contains(&frequency) && frequency.is_multiple_of(2) && phy <= 2,
                "validated BLE record has invalid frequency or PHY"
            );
            let body = record
                .ble_dewhitened_hex
                .context("validated BLE record lacks dewhitened bytes")?;
            ensure!(
                canonical_hex(body, if phy == 2 { 10 } else { 9 }, MAX_BLE_BODY_BYTES),
                "invalid validated BLE encoding or length"
            );
            // The complete dewhitened body includes access address, PDU and CRC,
            // and retains the coding indicator when present.
            digest.update(b"ble-body:");
            digest.update(frequency.to_le_bytes());
            digest.update([phy]);
            digest.update(body);
        }
        _ => return Ok(None),
    }
    Ok(Some(digest.finalize().into()))
}

pub(super) struct PacketFile {
    pub(super) packets: Vec<Packet>,
    pub(super) record_count: u64,
    pub(super) sha256: String,
    pub(super) stamp: Metadata,
}

pub(super) fn read_packets(
    path: &Path,
    protocol: Protocol,
    start_sample: u64,
    samples: u64,
    expected: u64,
    stop: &AtomicBool,
) -> Result<PacketFile> {
    ensure!(
        samples > 0 && start_sample.checked_add(samples).is_some(),
        "invalid packet-source sample extent"
    );
    ensure!(
        expected <= MAX_VALID_PER_FILE,
        "validated packet limit exceeded"
    );
    ensure!(
        !stop.load(AtomicOrdering::Relaxed),
        "packet comparison interrupted"
    );
    let (file, initial) = open_packets(path)?;
    let mut reader = BufReader::new(file);
    let mut packets = Vec::with_capacity(expected as usize);
    let mut digest = Sha256::new();
    let mut line = Vec::new();
    let mut consumed = 0;
    let mut records = 0;
    loop {
        ensure!(
            !stop.load(AtomicOrdering::Relaxed),
            "packet comparison interrupted"
        );
        line.clear();
        let count = (&mut reader)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .context("read bounded packet JSONL record")?;
        if count == 0 {
            break;
        }
        consumed += count as u64;
        records += 1;
        ensure!(
            consumed <= initial.len() && records <= MAX_RECORDS_PER_FILE,
            "packet JSONL grew or exceeded its record limit"
        );
        ensure!(
            count <= MAX_LINE_BYTES && line.ends_with(b"\n"),
            "oversized or unterminated packet JSONL record"
        );
        digest.update(&line);
        let record: Record<'_> = serde_json::from_slice(&line)
            .map_err(|_| anyhow::anyhow!("invalid packet JSONL record {records}"))?;
        ensure!(
            record.index == records,
            "packet JSONL record index mismatch"
        );
        if let Some(key) = validated_key(&record, protocol)? {
            ensure!(
                (packets.len() as u64) < expected,
                "validated packet count exceeds its summary"
            );
            packets.push(Packet {
                sample: source_sample(&record, protocol, start_sample, samples)?,
                record_index: record.index,
                ble_frequency_mhz: match protocol {
                    Protocol::Bluetooth => record.frequency_mhz,
                    Protocol::Wifi => None,
                },
                key,
            });
        }
    }
    ensure!(
        consumed == initial.len()
            && same_file(&initial, &reader.get_ref().metadata()?)
            && same_file(&initial, &fs::symlink_metadata(path)?),
        "packet JSONL changed while reading"
    );
    ensure!(
        packets.len() as u64 == expected,
        "validated packet count differs from its summary"
    );
    Ok(PacketFile {
        packets,
        record_count: records,
        sha256: format!("{:x}", digest.finalize()),
        stamp: initial,
    })
}

pub(super) fn match_packets(
    full: &[Packet],
    slices: &[Packet],
    protocol: Protocol,
) -> Result<Matches> {
    ensure!(
        full.len() <= MAX_VALIDATED_PACKETS && slices.len() <= MAX_VALIDATED_PACKETS,
        "packet match population exceeds its limit"
    );
    let ordered = |packets: &[Packet]| {
        let mut ordered: Vec<_> = packets
            .iter()
            .map(|packet| (packet.key, packet.sample))
            .collect();
        ordered.sort_unstable();
        ordered
    };
    let full = ordered(full);
    let slices = ordered(slices);
    let tolerance = protocol.tolerance();
    let mut matches = Matches::default();
    let (mut left, mut right) = (0, 0);
    while let (Some(&(ref full_key, full_sample)), Some(&(ref slice_key, slice_sample))) =
        (full.get(left), slices.get(right))
    {
        // Within a key, the ordered first feasible pair preserves maximum
        // cardinality. An earlier infeasible timestamp cannot match later.
        match full_key.cmp(slice_key) {
            Ordering::Less => left += 1,
            Ordering::Greater => right += 1,
            Ordering::Equal if slice_sample.abs_diff(full_sample) <= tolerance => {
                let delta = (slice_sample.abs_diff(full_sample) as i64)
                    * if slice_sample < full_sample { -1 } else { 1 };
                matches.matched += 1;
                matches.delta_samples_min = Some(
                    matches
                        .delta_samples_min
                        .map_or(delta, |min| min.min(delta)),
                );
                matches.delta_samples_max = Some(
                    matches
                        .delta_samples_max
                        .map_or(delta, |max| max.max(delta)),
                );
                left += 1;
                right += 1;
            }
            Ordering::Equal if full_sample < slice_sample => left += 1,
            Ordering::Equal => right += 1,
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests;
