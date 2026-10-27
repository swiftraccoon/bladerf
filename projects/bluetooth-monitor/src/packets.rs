//! Bounded PCAP reading and independent BLE integrity checks.
//!
//! Packet records are streamed. Per-packet JSON may contain addresses and payloads;
//! [`Summary`] contains aggregate counts only. CRC validity is not authentication,
//! decryption, receiver calibration, or evidence of complete channel coverage.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_PACKET_BYTES: usize = 65_536;
const MAX_CONNECTIONS: usize = 1024;
const DLT_PPI: u32 = 192;
const DLT_BLE: u32 = 251;
const DLT_CLASSIC: u32 = 255;
const DLT_BLE_PHDR: u32 = 256;
const ADVERTISING_AA: u32 = 0x8e89_bed6;
const DEWHITENED: u16 = 0x0001;
const POWER_VALID: u16 = 0x0006;
const CHANNEL_ALIASED: u16 = 0x0040;
const CRC_CHECKED_VALID: u16 = 0x0c00;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Aggregate capture information without device addresses, names or payloads.
#[derive(Debug, Default, Serialize)]
pub(crate) struct Summary {
    pub(crate) input_linktype: u32,
    pub(crate) total_records: u64,
    pub(crate) validated_advertising_packets: u64,
    pub(crate) validated_data_packets: u64,
    pub(crate) invalid_crc_packets: u64,
    pub(crate) unknown_crc_packets: u64,
    pub(crate) malformed_packets: u64,
    pub(crate) truncated_records: u64,
    pub(crate) records_exceeding_snaplen: u64,
    pub(crate) classic_records: u64,
    pub(crate) classic_upstream_crc_claims: u64,
    pub(crate) unsupported_records: u64,
    pub(crate) connections_learned: u64,
    pub(crate) connection_context_evictions: u64,
    pub(crate) packets_by_logical_channel: BTreeMap<String, u64>,
    pub(crate) packets_by_pdu_type: BTreeMap<String, u64>,
    pub(crate) advertising_data_types: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, bytes: &[u8]) -> u16 {
        let raw = [bytes[0], bytes[1]];
        match self {
            Self::Little => u16::from_le_bytes(raw),
            Self::Big => u16::from_be_bytes(raw),
        }
    }

    fn u32(self, bytes: &[u8]) -> u32 {
        let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(raw),
            Self::Big => u32::from_be_bytes(raw),
        }
    }
}

#[derive(Debug)]
struct Header {
    byte_order: ByteOrder,
    nanoseconds: bool,
    snaplen: u32,
    linktype: u32,
}

impl Header {
    fn read(reader: &mut impl Read) -> Result<Self> {
        let mut bytes = [0; 24];
        reader
            .read_exact(&mut bytes)
            .context("read PCAP global header")?;
        let (byte_order, nanoseconds) = match &bytes[..4] {
            [0xd4, 0xc3, 0xb2, 0xa1] => (ByteOrder::Little, false),
            [0xa1, 0xb2, 0xc3, 0xd4] => (ByteOrder::Big, false),
            [0x4d, 0x3c, 0xb2, 0xa1] => (ByteOrder::Little, true),
            [0xa1, 0xb2, 0x3c, 0x4d] => (ByteOrder::Big, true),
            _ => bail!("expected classic PCAP; PCAPNG and other formats are unsupported"),
        };
        ensure!(
            byte_order.u16(&bytes[4..]) == 2 && byte_order.u16(&bytes[6..]) == 4,
            "unsupported PCAP version"
        );
        let snaplen = byte_order.u32(&bytes[16..]);
        ensure!(
            snaplen > 0 && snaplen <= MAX_PACKET_BYTES as u32,
            "PCAP snaplen must be 1..={MAX_PACKET_BYTES}"
        );
        let linktype = byte_order.u32(&bytes[20..]);
        ensure!(
            matches!(linktype, DLT_PPI | DLT_BLE | DLT_CLASSIC | DLT_BLE_PHDR),
            "unsupported PCAP link type {linktype}"
        );
        Ok(Self {
            byte_order,
            nanoseconds,
            snaplen,
            linktype,
        })
    }

    fn write(&self, writer: &mut impl Write, linktype: u32) -> Result<()> {
        let magic = if self.nanoseconds {
            0xa1b2_3c4d_u32
        } else {
            0xa1b2_c3d4_u32
        };
        writer.write_all(&magic.to_le_bytes())?;
        writer.write_all(&2_u16.to_le_bytes())?;
        writer.write_all(&4_u16.to_le_bytes())?;
        writer.write_all(&[0; 8])?;
        writer.write_all(&(MAX_PACKET_BYTES as u32).to_le_bytes())?;
        writer.write_all(&linktype.to_le_bytes())?;
        Ok(())
    }
}

/// A private temporary file published without replacing an existing path.
#[derive(Debug)]
struct Output {
    destination: PathBuf,
    temporary: PathBuf,
    writer: BufWriter<File>,
}

impl Output {
    fn new(destination: &Path) -> Result<Self> {
        ensure!(
            !destination.exists(),
            "output already exists: {}",
            destination.display()
        );
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for _ in 0..128 {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".bluetooth-packets-{}-{sequence}.tmp",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temporary) {
                Ok(file) => {
                    return Ok(Self {
                        destination: destination.to_owned(),
                        temporary,
                        writer: BufWriter::new(file),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create private output in {}", parent.display()));
                }
            }
        }
        bail!("could not reserve a temporary output file")
    }

    fn prepare(&mut self) -> Result<()> {
        self.writer.flush().context("flush packet output")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("sync packet output")
    }

    fn publish(&self) -> Result<()> {
        fs::hard_link(&self.temporary, &self.destination)
            .with_context(|| format!("publish {} without overwriting", self.destination.display()))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

#[derive(Debug, Serialize)]
struct PacketRecord {
    index: u64,
    timestamp_seconds: u32,
    timestamp_fraction: u32,
    timestamp_precision: &'static str,
    encapsulated_linktype: u32,
    physical_channel: Option<u8>,
    logical_channel: Option<u8>,
    frequency_mhz: Option<u16>,
    upstream_phy_bits: Option<u8>,
    signal_relative_estimate: Option<i8>,
    noise_relative_estimate: Option<i8>,
    access_address: Option<String>,
    validation: &'static str,
    pdu_type: Option<String>,
    packet_hex: String,
    ble_dewhitened_hex: Option<String>,
    details: Option<Value>,
    warnings: Vec<&'static str>,
}

impl PacketRecord {
    fn new(index: u64, seconds: u32, fraction: u32, header: &Header) -> Self {
        Self {
            index,
            timestamp_seconds: seconds,
            timestamp_fraction: fraction,
            timestamp_precision: if header.nanoseconds {
                "nanoseconds"
            } else {
                "microseconds"
            },
            encapsulated_linktype: header.linktype,
            physical_channel: None,
            logical_channel: None,
            frequency_mhz: None,
            upstream_phy_bits: None,
            signal_relative_estimate: None,
            noise_relative_estimate: None,
            access_address: None,
            validation: "malformed",
            pdu_type: None,
            packet_hex: String::new(),
            ble_dewhitened_hex: None,
            details: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
struct Connections(VecDeque<(u32, u32)>);

impl Connections {
    fn get(&self, address: u32) -> Option<u32> {
        self.0
            .iter()
            .find_map(|&(aa, init)| (aa == address).then_some(init))
    }

    fn learn(&mut self, address: u32, init: u32, summary: &mut Summary) {
        if address == ADVERTISING_AA {
            return;
        }
        if let Some(position) = self.0.iter().position(|&(aa, _)| aa == address) {
            self.0.remove(position);
        } else if self.0.len() == MAX_CONNECTIONS {
            self.0.pop_front();
            summary.connection_context_evictions += 1;
        }
        self.0.push_back((address, init));
        summary.connections_learned += 1;
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}
fn le24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn logical_channel(physical: u8) -> Option<u8> {
    match physical {
        0 => Some(37),
        12 => Some(38),
        39 => Some(39),
        1..=11 => Some(physical - 1),
        13..=38 => Some(physical - 2),
        _ => None,
    }
}

/// Core Vol 6 Part B 3.1: polynomial x^24+x^10+x^9+x^6+x^4+x^3+x+1.
/// Input octets are processed least-significant bit first; CRC output is emitted
/// from register bit 23, represented below as transmission-order octets.
fn crc24(pdu: &[u8], initial: u32) -> [u8; 3] {
    let mut state = initial & 0x00ff_ffff;
    for &byte in pdu {
        for bit in 0..8 {
            let feedback = ((state >> 23) & 1) ^ u32::from((byte >> bit) & 1);
            state = (state << 1) & 0x00ff_ffff;
            if feedback != 0 {
                state ^= 0x0000_065b;
            }
        }
    }
    [
        ((state >> 16) as u8).reverse_bits(),
        ((state >> 8) as u8).reverse_bits(),
        (state as u8).reverse_bits(),
    ]
}

fn dewhiten(bytes: &mut [u8], channel: u8) {
    let mut state = channel | 0x40;
    for byte in bytes {
        for bit in 0..8 {
            let feedback = state & 1;
            *byte ^= feedback << bit;
            state = (state >> 1) ^ if feedback != 0 { 0x44 } else { 0 };
        }
    }
}

fn address(bytes: &[u8], random: bool) -> Value {
    let formatted = bytes
        .iter()
        .rev()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    let kind = if !random {
        "public"
    } else {
        match bytes[5] >> 6 {
            0 => "random_non_resolvable",
            1 => "random_resolvable",
            3 => "random_static",
            _ => "random_reserved",
        }
    };
    json!({"address": formatted, "address_type": kind})
}

fn uuid(bytes: &[u8]) -> String {
    let reversed: Vec<_> = bytes.iter().rev().copied().collect();
    let raw = hex(&reversed);
    if bytes.len() == 16 {
        format!(
            "{}-{}-{}-{}-{}",
            &raw[..8],
            &raw[8..12],
            &raw[12..16],
            &raw[16..20],
            &raw[20..]
        )
    } else {
        raw
    }
}

fn advertising_data(bytes: &[u8], record: &mut PacketRecord, summary: &mut Summary) -> Vec<Value> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = usize::from(bytes[offset]);
        offset += 1;
        if length == 0 {
            if bytes[offset..].iter().any(|&b| b != 0) {
                record.warnings.push("nonzero_bytes_after_ad_terminator");
            }
            break;
        }
        if length > bytes.len() - offset {
            record.warnings.push("truncated_ad_structure");
            break;
        }
        let kind = bytes[offset];
        let data = &bytes[offset + 1..offset + length];
        let mut field = json!({"type_id": kind, "data_hex": hex(data)});
        *summary
            .advertising_data_types
            .entry(format!("0x{kind:02x}"))
            .or_default() += 1;
        match kind {
            0x01 if data.len() == 1 => field["flags"] = json!(data[0]),
            0x02..=0x07 => {
                let width = match kind {
                    0x02 | 0x03 => 2,
                    0x04 | 0x05 => 4,
                    _ => 16,
                };
                if data.len().is_multiple_of(width) {
                    field["service_uuids"] =
                        json!(data.chunks_exact(width).map(uuid).collect::<Vec<_>>());
                    field["complete_list"] = json!(kind % 2 == 1);
                } else {
                    record.warnings.push("malformed_service_uuid_list");
                }
            }
            0x08 | 0x09 => {
                field["name_utf8"] = json!(std::str::from_utf8(data).ok());
                field["complete_name"] = json!(kind == 0x09);
            }
            0x0a if data.len() == 1 => field["advertised_tx_power_dbm"] = json!(data[0] as i8),
            0x16 | 0x20 | 0x21 => {
                let width = match kind {
                    0x16 => 2,
                    0x20 => 4,
                    _ => 16,
                };
                if data.len() >= width {
                    field["service_uuid"] = json!(uuid(&data[..width]));
                    field["service_data_hex"] = json!(hex(&data[width..]));
                } else {
                    record.warnings.push("short_service_data");
                }
            }
            0x19 if data.len() == 2 => field["appearance"] = json!(le16(data)),
            0xff if data.len() >= 2 => {
                field["company_id"] = json!(le16(data));
                field["manufacturer_data_hex"] = json!(hex(&data[2..]));
            }
            0x31 => field["interpretation"] = json!("encrypted_advertising_data_not_decrypted"),
            _ => {}
        }
        fields.push(field);
        offset += length;
    }
    fields
}

fn extended_advertising(
    payload: &[u8],
    tx_random: bool,
    rx_random: bool,
    record: &mut PacketRecord,
    summary: &mut Summary,
) -> Value {
    if payload.is_empty() {
        record.warnings.push("missing_extended_header");
        return Value::Null;
    }
    let length = usize::from(payload[0] & 0x3f);
    let mut details =
        json!({"advertising_mode": payload[0] >> 6, "extended_header_length": length});
    if length > payload.len() - 1 {
        record.warnings.push("truncated_extended_header");
        return details;
    }
    if length == 0 {
        details["advertising_data"] = json!(advertising_data(&payload[1..], record, summary));
        return details;
    }
    let flags = payload[1];
    details["extended_header_flags"] = json!(flags);
    let header = &payload[2..1 + length];
    let mut position = 0;
    for (mask, width, name) in [
        (1, 6, "advertiser"),
        (2, 6, "target"),
        (4, 1, "cte_info"),
        (8, 2, "advertising_data_info"),
        (16, 3, "aux_pointer"),
        (32, 18, "sync_info"),
        (64, 1, "tx_power"),
    ] {
        if flags & mask == 0 {
            continue;
        }
        if width > header.len() - position {
            record.warnings.push("extended_field_exceeds_header");
            return details;
        }
        let value = &header[position..position + width];
        details[name] = match mask {
            1 => address(value, tx_random),
            2 => address(value, rx_random),
            8 => json!({"did": le16(value) & 0x0fff, "sid": le16(value) >> 12}),
            16 => {
                json!({"logical_channel": value[0] & 0x3f, "clock_accuracy": value[0] & 0x40 != 0, "offset_units_us": if value[0] & 0x80 != 0 { 300 } else { 30 }, "offset": le16(&value[1..]) & 0x1fff, "phy_bits": value[2] >> 5})
            }
            64 => json!({"advertised_dbm": value[0] as i8}),
            _ => json!({"raw_hex": hex(value)}),
        };
        position += width;
    }
    details["extended_header_remaining_hex"] = json!(hex(&header[position..]));
    details["advertising_data"] = json!(advertising_data(&payload[1 + length..], record, summary));
    details
}

fn decode_advertising(
    pdu: &[u8],
    record: &mut PacketRecord,
    summary: &mut Summary,
    connections: &mut Connections,
) {
    let kind = pdu[0] & 15;
    let payload = &pdu[2..];
    let tx_random = pdu[0] & 0x40 != 0;
    let rx_random = pdu[0] & 0x80 != 0;
    let name = match kind {
        0 => "ADV_IND",
        1 => "ADV_DIRECT_IND",
        2 => "ADV_NONCONN_IND",
        3 => "SCAN_REQ",
        4 => "SCAN_RSP",
        5 => "CONNECT_IND",
        6 => "ADV_SCAN_IND",
        7 => "ADV_EXT_IND_OR_AUX_ADVERTISING",
        _ => "reserved_advertising_type",
    };
    record.pdu_type = Some(name.to_owned());
    *summary
        .packets_by_pdu_type
        .entry(name.to_owned())
        .or_default() += 1;
    record.details = match kind {
        0 | 2 | 4 | 6 if (6..=37).contains(&payload.len()) => Some(
            json!({"advertiser": address(&payload[..6], tx_random), "advertising_data": advertising_data(&payload[6..], record, summary)}),
        ),
        1 if payload.len() == 12 => Some(
            json!({"advertiser": address(&payload[..6],tx_random), "target": address(&payload[6..],rx_random)}),
        ),
        3 if payload.len() == 12 => Some(
            json!({"scanner": address(&payload[..6],tx_random), "advertiser": address(&payload[6..],rx_random)}),
        ),
        5 if payload.len() == 34 => {
            let aa = le32(&payload[12..]);
            let init = le24(&payload[16..]);
            connections.learn(aa, init, summary);
            Some(
                json!({"initiator": address(&payload[..6],tx_random), "advertiser": address(&payload[6..12],rx_random), "connection_access_address": format!("0x{aa:08x}"), "crc_init": format!("0x{init:06x}"), "window_size_1_25ms_units": payload[19], "window_offset_1_25ms_units": le16(&payload[20..]), "interval_1_25ms_units": le16(&payload[22..]), "latency_events": le16(&payload[24..]), "supervision_timeout_10ms_units": le16(&payload[26..]), "channel_map_hex": hex(&payload[28..33]), "hop_increment": payload[33] & 0x1f, "sleep_clock_accuracy_bits": payload[33] >> 5}),
            )
        }
        7 => Some(extended_advertising(
            payload, tx_random, rx_random, record, summary,
        )),
        _ => {
            record
                .warnings
                .push("advertising_type_or_payload_length_not_decoded");
            None
        }
    };
}

fn unwrap_record(bytes: &[u8], linktype: u32) -> Result<(u32, &[u8])> {
    if linktype != DLT_PPI {
        return Ok((linktype, bytes));
    }
    ensure!(
        bytes.len() >= 8 && bytes[0] == 0,
        "invalid or unsupported PPI header"
    );
    let length = usize::from(le16(&bytes[2..]));
    ensure!(
        (8..=bytes.len()).contains(&length),
        "invalid PPI header length"
    );
    Ok((le32(&bytes[4..]), &bytes[length..]))
}

fn analyze(
    bytes: &[u8],
    source_linktype: u32,
    record: &mut PacketRecord,
    summary: &mut Summary,
    connections: &mut Connections,
) -> Option<Vec<u8>> {
    let (linktype, bytes) = match unwrap_record(bytes, source_linktype) {
        Ok(value) => value,
        Err(_) => {
            summary.malformed_packets += 1;
            record.warnings.push("invalid_ppi_header");
            return None;
        }
    };
    record.encapsulated_linktype = linktype;
    record.packet_hex = hex(bytes);
    if linktype == DLT_CLASSIC {
        if bytes.len() < 22 {
            summary.malformed_packets += 1;
            record.warnings.push("short_classic_pseudoheader");
            return None;
        }
        summary.classic_records += 1;
        let flags = le16(&bytes[20..]);
        if flags & CRC_CHECKED_VALID == CRC_CHECKED_VALID {
            summary.classic_upstream_crc_claims += 1;
        }
        record.validation = "classic_not_independently_validated";
        record.frequency_mhz = (bytes[0] <= 78).then_some(2402 + u16::from(bytes[0]));
        record.signal_relative_estimate = (flags & 2 != 0).then_some(bytes[1] as i8);
        record.noise_relative_estimate = (flags & 4 != 0).then_some(bytes[2] as i8);
        record.details = Some(
            json!({"lap": format!("0x{:06x}",le32(&bytes[8..]) & 0xffffff), "reference_lap_uap": format!("0x{:08x}",le32(&bytes[12..])), "decoded_header_bits": format!("0x{:08x}",le32(&bytes[16..])), "upstream_flags": flags, "decoded_payload_hex": hex(&bytes[22..]), "upstream_crc_valid_claim": flags & CRC_CHECKED_VALID == CRC_CHECKED_VALID}),
        );
        return None;
    }
    let offset = match linktype {
        DLT_BLE_PHDR => 10,
        DLT_BLE if source_linktype != DLT_PPI => 0,
        _ => {
            record.validation = "unsupported_encapsulation";
            summary.unsupported_records += 1;
            return None;
        }
    };
    if bytes.len() < offset + 9 {
        summary.malformed_packets += 1;
        record.warnings.push("short_ble_packet");
        return None;
    }
    let mut packet = bytes.to_vec();
    if offset != 0 {
        let flags = le16(&packet[8..]);
        record.physical_channel = Some(packet[0]);
        record.logical_channel = logical_channel(packet[0]);
        record.frequency_mhz = record
            .logical_channel
            .map(|_| 2402 + u16::from(packet[0]) * 2);
        record.upstream_phy_bits = Some((flags >> 14) as u8);
        record.signal_relative_estimate = (flags & 2 != 0).then_some(packet[1] as i8);
        record.noise_relative_estimate = (flags & 4 != 0).then_some(packet[2] as i8);
        if flags & CHANNEL_ALIASED != 0 {
            record.warnings.push("rf_channel_marked_aliased");
        }
        if flags & DEWHITENED == 0 {
            if let Some(channel) = record.logical_channel {
                dewhiten(&mut packet[offset + 4..], channel);
            } else {
                record.validation = "unknown_whitening_channel";
                summary.unsupported_records += 1;
                return None;
            }
        }
    }
    let data = &packet[offset..];
    let aa = le32(data);
    record.access_address = Some(format!("0x{aa:08x}"));
    let pdu_header_length = if aa != ADVERTISING_AA && data[4] & 0x20 != 0 {
        3
    } else {
        2
    };
    let expected = 4 + pdu_header_length + usize::from(data[5]) + 3;
    if data.len() != expected {
        summary.malformed_packets += 1;
        record.warnings.push("ble_length_does_not_match_record");
        return None;
    }
    let init = if aa == ADVERTISING_AA {
        Some(0x555555)
    } else {
        connections.get(aa)
    };
    let Some(init) = init else {
        record.validation = "crc_unknown_connection_context";
        summary.unknown_crc_packets += 1;
        return None;
    };
    let pdu = &data[4..data.len() - 3];
    if crc24(pdu, init) != data[data.len() - 3..] {
        record.validation = "crc_invalid";
        summary.invalid_crc_packets += 1;
        return None;
    }
    record.ble_dewhitened_hex = Some(hex(data));
    if let Some(channel) = record.logical_channel {
        *summary
            .packets_by_logical_channel
            .entry(channel.to_string())
            .or_default() += 1;
    }
    if aa == ADVERTISING_AA {
        record.validation = "advertising_crc_valid";
        summary.validated_advertising_packets += 1;
        decode_advertising(pdu, record, summary, connections);
    } else {
        record.validation = "connection_crc_valid";
        summary.validated_data_packets += 1;
        record.pdu_type = Some("LL_DATA_OPAQUE".to_owned());
        *summary
            .packets_by_pdu_type
            .entry("LL_DATA_OPAQUE".to_owned())
            .or_default() += 1;
        record.details = Some(
            json!({"llid": pdu[0] & 3, "nesn": pdu[0] & 4 != 0, "sn": pdu[0] & 8 != 0, "md": pdu[0] & 16 != 0, "cte_info": if pdu_header_length == 3 { Some(pdu[2]) } else { None }, "payload_hex": hex(&pdu[pdu_header_length..]), "payload_visibility": "opaque_encryption_not_determined", "crc_init_source": "previous_crc_valid_connection_request"}),
        );
    }
    if offset != 0 {
        // Liquid/SDR estimates are not calibrated dBm. Independently checked CRC
        // flags describe this validation layer, not an upstream validity claim.
        let flags = (le16(&packet[8..]) | DEWHITENED | CRC_CHECKED_VALID) & !POWER_VALID;
        packet[8..10].copy_from_slice(&flags.to_le_bytes());
    }
    Some(packet)
}

fn output_identity(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path.file_name().context("output needs a filename")?;
    Ok(parent.canonicalize()?.join(name))
}

/// Inspect classic PCAP streams without opening a radio or fetching any source.
///
/// Outputs are published only after the entire input parses. Existing paths are
/// preserved, including concurrent creations. PCAP and JSONL publication is not
/// a two-file transaction: failure of the second publication can leave the first
/// completed file. Temporary-file cleanup is best effort on failure.
pub(crate) fn inspect(
    input: &Path,
    validated_output: Option<&Path>,
    jsonl_output: Option<&Path>,
) -> Result<Summary> {
    if let (Some(pcap), Some(jsonl)) = (validated_output, jsonl_output) {
        ensure!(
            output_identity(pcap)? != output_identity(jsonl)?,
            "PCAP and JSONL outputs must be different paths"
        );
    }
    let mut reader =
        BufReader::new(File::open(input).with_context(|| format!("open {}", input.display()))?);
    let header = Header::read(&mut reader)?;
    let trusted_linktype = if header.linktype == DLT_BLE {
        DLT_BLE
    } else {
        DLT_BLE_PHDR
    };
    let mut trusted = validated_output.map(Output::new).transpose()?;
    let mut jsonl = jsonl_output.map(Output::new).transpose()?;
    if let Some(output) = &mut trusted {
        header.write(&mut output.writer, trusted_linktype)?;
    }
    let mut summary = Summary {
        input_linktype: header.linktype,
        ..Summary::default()
    };
    let mut connections = Connections::default();
    loop {
        let mut raw = [0; 16];
        if reader
            .read(&mut raw[..1])
            .context("read PCAP record boundary")?
            == 0
        {
            break;
        }
        reader
            .read_exact(&mut raw[1..])
            .context("truncated PCAP record header")?;
        let seconds = header.byte_order.u32(&raw);
        let fraction = header.byte_order.u32(&raw[4..]);
        ensure!(
            fraction
                < if header.nanoseconds {
                    1_000_000_000
                } else {
                    1_000_000
                },
            "invalid PCAP timestamp fraction"
        );
        let included = header.byte_order.u32(&raw[8..]);
        let original = header.byte_order.u32(&raw[12..]);
        ensure!(
            included <= MAX_PACKET_BYTES as u32,
            "PCAP record exceeds {MAX_PACKET_BYTES}-byte allocation cap"
        );
        ensure!(
            original >= included,
            "PCAP original length is smaller than captured length"
        );
        let mut bytes = vec![0; included as usize];
        reader
            .read_exact(&mut bytes)
            .context("truncated PCAP record body")?;
        summary.total_records += 1;
        if included > header.snaplen {
            summary.records_exceeding_snaplen += 1;
        }
        let mut record = PacketRecord::new(summary.total_records, seconds, fraction, &header);
        let valid = if original != included {
            summary.truncated_records += 1;
            record.validation = "capture_truncated";
            record.packet_hex = hex(&bytes);
            None
        } else {
            analyze(
                &bytes,
                header.linktype,
                &mut record,
                &mut summary,
                &mut connections,
            )
        };
        if let (Some(output), Some(packet)) = (&mut trusted, valid) {
            output.writer.write_all(&seconds.to_le_bytes())?;
            output.writer.write_all(&fraction.to_le_bytes())?;
            let length = u32::try_from(packet.len())?;
            output.writer.write_all(&length.to_le_bytes())?;
            output.writer.write_all(&length.to_le_bytes())?;
            output.writer.write_all(&packet)?;
        }
        if let Some(output) = &mut jsonl {
            serde_json::to_writer(&mut output.writer, &record)?;
            output.writer.write_all(b"\n")?;
        }
    }
    if let Some(output) = &mut trusted {
        output.prepare()?;
    }
    if let Some(output) = &mut jsonl {
        output.prepare()?;
    }
    if let Some(output) = &trusted {
        output.publish()?;
    }
    if let Some(output) = &jsonl {
        output.publish()?;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            for _ in 0..128 {
                let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "bluetooth-packet-test-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create isolated test directory: {error}"),
                }
            }
            panic!("no isolated test directory available")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pcap(order: ByteOrder, nanoseconds: bool, linktype: u32, packets: &[Vec<u8>]) -> Vec<u8> {
        fn put16(bytes: &mut Vec<u8>, value: u16, order: ByteOrder) {
            bytes.extend_from_slice(&match order {
                ByteOrder::Little => value.to_le_bytes(),
                ByteOrder::Big => value.to_be_bytes(),
            });
        }
        fn put32(bytes: &mut Vec<u8>, value: u32, order: ByteOrder) {
            bytes.extend_from_slice(&match order {
                ByteOrder::Little => value.to_le_bytes(),
                ByteOrder::Big => value.to_be_bytes(),
            });
        }
        let mut bytes = Vec::new();
        put32(
            &mut bytes,
            if nanoseconds { 0xa1b23c4d } else { 0xa1b2c3d4 },
            order,
        );
        put16(&mut bytes, 2, order);
        put16(&mut bytes, 4, order);
        put32(&mut bytes, 0, order);
        put32(&mut bytes, 0, order);
        put32(&mut bytes, 264, order);
        put32(&mut bytes, linktype, order);
        for packet in packets {
            put32(&mut bytes, 1, order);
            put32(&mut bytes, 1234, order);
            put32(&mut bytes, packet.len() as u32, order);
            put32(&mut bytes, packet.len() as u32, order);
            bytes.extend_from_slice(packet);
        }
        bytes
    }

    // Independent Core 6.2 Vol 6 Part C 4.2.1/4.3.1 sample data. These are
    // specification examples, not captured devices or generated CRC fixtures.
    const SPEC_ADVERTISING_PDU: &[u8] = &[0x42, 0x09, 0xa6, 0xa5, 0xa4, 0xa3, 0xa2, 0xc1, 1, 2, 3];

    #[test]
    fn official_crc_and_whitening_vectors() {
        assert_eq!(crc24(SPEC_ADVERTISING_PDU, 0x555555), [0xad, 0xb4, 0xeb]);
        assert_eq!(
            crc24(&[0x16, 5, 1, 2, 3, 4, 5], 0xc4c181),
            [0x45, 0xd0, 0xd2]
        );
        let mut raw = [
            0x94, 0xcc, 0xe2, 0x85, 0xfd, 0x7d, 0x43, 0x4e, 0x1a, 0xa7, 0xac, 0xef, 0xcf, 0xa5,
        ];
        dewhiten(&mut raw, 38);
        assert_eq!(&raw[..11], SPEC_ADVERTISING_PDU);
        assert_eq!(&raw[11..], &[0xad, 0xb4, 0xeb]);
    }

    fn ble_record(pdu: &[u8], init: u32, aa: u32) -> Vec<u8> {
        let mut output = vec![12, 0xd0, 0xa0, 0, 0, 0, 0, 0, 7, 0];
        output.extend_from_slice(&aa.to_le_bytes());
        output.extend_from_slice(pdu);
        output.extend_from_slice(&crc24(pdu, init));
        output
    }

    fn packet_record() -> PacketRecord {
        PacketRecord::new(
            1,
            0,
            0,
            &Header {
                byte_order: ByteOrder::Little,
                nanoseconds: false,
                snaplen: 65536,
                linktype: DLT_BLE_PHDR,
            },
        )
    }

    #[test]
    fn validates_spec_packet_and_clears_uncalibrated_power_flags() {
        let packet = ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA);
        let mut record = packet_record();
        let mut summary = Summary::default();
        let mut connections = Connections::default();
        let output = analyze(
            &packet,
            DLT_BLE_PHDR,
            &mut record,
            &mut summary,
            &mut connections,
        )
        .unwrap();
        assert_eq!(summary.validated_advertising_packets, 1);
        assert_eq!(record.logical_channel, Some(38));
        assert_eq!(le16(&output[8..]) & POWER_VALID, 0);
        assert_eq!(le16(&output[8..]) & CRC_CHECKED_VALID, CRC_CHECKED_VALID);
        assert_eq!(
            record.details.as_ref().unwrap()["advertiser"]["address"],
            "c1:a2:a3:a4:a5:a6"
        );
    }

    #[test]
    fn rejects_crc_noise_and_unknown_connection_context() {
        let mut packet = ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA);
        packet[16] ^= 1;
        let mut summary = Summary::default();
        let mut connections = Connections::default();
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_none()
        );
        assert_eq!(summary.invalid_crc_packets, 1);
        let packet = ble_record(&[0x16, 5, 1, 2, 3, 4, 5], 0xc4c181, 0xaa08192b);
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_none()
        );
        assert_eq!(summary.unknown_crc_packets, 1);
    }

    #[test]
    fn learns_connection_only_from_valid_crc_request() {
        let mut pdu = vec![5, 34];
        let mut payload = vec![0; 34];
        payload[12..16].copy_from_slice(&0xaa08192b_u32.to_le_bytes());
        payload[16..19].copy_from_slice(&[0x81, 0xc1, 0xc4]);
        pdu.extend_from_slice(&payload);
        let packet = ble_record(&pdu, 0x555555, ADVERTISING_AA);
        let mut summary = Summary::default();
        let mut connections = Connections::default();
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_some()
        );
        let packet = ble_record(&[0x16, 5, 1, 2, 3, 4, 5], 0xc4c181, 0xaa08192b);
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_some()
        );
        assert_eq!(summary.validated_data_packets, 1);
        let mut packet = ble_record(&pdu, 0x555555, ADVERTISING_AA);
        packet[14] ^= 1;
        let mut untouched = Connections::default();
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut packet_record(),
                &mut summary,
                &mut untouched
            )
            .is_none()
        );
        assert!(untouched.0.is_empty());
    }

    #[test]
    fn reads_both_pcap_orders_and_precisions_and_preserves_existing_outputs() {
        let dir = TestDirectory::new();
        let input = dir.0.join("input.pcap");
        let output = dir.0.join("validated.pcap");
        let jsonl = dir.0.join("packets.jsonl");
        let packet = ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA);
        for order in [ByteOrder::Little, ByteOrder::Big] {
            for nano in [false, true] {
                fs::write(
                    &input,
                    pcap(order, nano, DLT_BLE_PHDR, std::slice::from_ref(&packet)),
                )
                .unwrap();
                let summary = inspect(&input, None, None).unwrap();
                assert_eq!(summary.validated_advertising_packets, 1);
            }
        }
        let summary = inspect(&input, Some(&output), Some(&jsonl)).unwrap();
        assert_eq!(summary.total_records, 1);
        let expected = fs::read(&output).unwrap();
        assert!(inspect(&input, Some(&output), None).is_err());
        assert_eq!(fs::read(&output).unwrap(), expected);
        let record: Value =
            serde_json::from_str(fs::read_to_string(&jsonl).unwrap().trim()).unwrap();
        assert_eq!(record["validation"], "advertising_crc_valid");
        assert_eq!(record["timestamp_precision"], "nanoseconds");
        assert_eq!(
            inspect(&output, None, None)
                .unwrap()
                .validated_advertising_packets,
            1
        );
    }

    #[test]
    fn malformed_pcap_does_not_publish_or_leave_temporary_outputs() {
        let dir = TestDirectory::new();
        let input = dir.0.join("input.pcap");
        let output = dir.0.join("validated.pcap");
        let packet = ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA);
        let mut bytes = pcap(ByteOrder::Little, false, DLT_BLE_PHDR, &[packet]);
        bytes.pop();
        fs::write(&input, &bytes).unwrap();
        assert!(inspect(&input, Some(&output), None).is_err());
        assert!(!output.exists());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        bytes[32..36].copy_from_slice(&65537_u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&65537_u32.to_le_bytes());
        fs::write(&input, &bytes).unwrap();
        assert!(
            inspect(&input, None, None)
                .unwrap_err()
                .to_string()
                .contains("allocation cap")
        );
    }

    #[test]
    fn mixed_ppi_classic_remains_separate_from_independently_verified_ble() {
        let dir = TestDirectory::new();
        let input = dir.0.join("mixed.pcap");
        let output = dir.0.join("validated.pcap");
        let mut ble = vec![0, 0, 8, 0];
        ble.extend_from_slice(&DLT_BLE_PHDR.to_le_bytes());
        ble.extend_from_slice(&ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA));
        let mut classic = vec![0, 0, 8, 0];
        classic.extend_from_slice(&DLT_CLASSIC.to_le_bytes());
        let mut pseudoheader = [0; 22];
        pseudoheader[20..22].copy_from_slice(&CRC_CHECKED_VALID.to_le_bytes());
        classic.extend_from_slice(&pseudoheader);
        fs::write(
            &input,
            pcap(ByteOrder::Little, false, DLT_PPI, &[ble, classic]),
        )
        .unwrap();
        let summary = inspect(&input, Some(&output), None).unwrap();
        assert_eq!(summary.total_records, 2);
        assert_eq!(summary.classic_records, 1);
        assert_eq!(summary.classic_upstream_crc_claims, 1);
        let verified = inspect(&output, None, None).unwrap();
        assert_eq!(verified.input_linktype, DLT_BLE_PHDR);
        assert_eq!(verified.total_records, 1);
        assert_eq!(verified.validated_advertising_packets, 1);
    }

    #[test]
    fn handles_ppi_and_extended_header_bounds() {
        let mut ppi = vec![0, 0, 8, 0];
        ppi.extend_from_slice(&DLT_BLE_PHDR.to_le_bytes());
        ppi.extend_from_slice(&ble_record(SPEC_ADVERTISING_PDU, 0x555555, ADVERTISING_AA));
        let mut summary = Summary::default();
        let mut connections = Connections::default();
        assert!(
            analyze(
                &ppi,
                DLT_PPI,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_some()
        );
        ppi[2] = 255;
        assert!(
            analyze(
                &ppi,
                DLT_PPI,
                &mut packet_record(),
                &mut summary,
                &mut connections
            )
            .is_none()
        );
        let packet = ble_record(&[7, 2, 1, 0x01], 0x555555, ADVERTISING_AA);
        let mut record = packet_record();
        assert!(
            analyze(
                &packet,
                DLT_BLE_PHDR,
                &mut record,
                &mut summary,
                &mut connections
            )
            .is_some()
        );
        assert!(record.warnings.contains(&"extended_field_exceeds_header"));
    }
}
