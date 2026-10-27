//! Streaming classic-PCAP inspection with independent IEEE 802.11 FCS checking.
//!
//! Packet JSON contains transmitted addresses, SSIDs and payloads; summaries hold
//! aggregate counts only. An FCS checks consistency, not sender identity,
//! decryption, complete channel coverage or radio calibration.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

const MAX_PACKET_BYTES: usize = 65_536;
const DLT_IEEE80211: u32 = 105;
const DLT_RADIOTAP: u32 = 127;
const FCS_PRESENT: u8 = 0x10;
const DATA_PAD: u8 = 0x20;
const BAD_FCS: u8 = 0x40;
const MINIMAL_RADIOTAP: [u8; 9] = [0, 0, 9, 0, 2, 0, 0, 0, FCS_PRESENT];
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Aggregate counters; frame-type and IE maps describe independently valid frames.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Summary {
    pub(crate) input_linktype: u32,
    pub(crate) total_records: u64,
    pub(crate) validated_frames: u64,
    pub(crate) invalid_fcs_frames: u64,
    pub(crate) unknown_fcs_frames: u64,
    pub(crate) absent_fcs_frames: u64,
    pub(crate) malformed_frames: u64,
    pub(crate) truncated_records: u64,
    pub(crate) records_exceeding_snaplen: u64,
    pub(crate) upstream_bad_fcs_claims: u64,
    pub(crate) management_frames: u64,
    pub(crate) control_frames: u64,
    pub(crate) data_frames: u64,
    pub(crate) protected_frames: u64,
    pub(crate) eapol_frames: u64,
    pub(crate) frames_by_type: BTreeMap<String, u64>,
    pub(crate) information_element_ids: BTreeMap<String, u64>,
    pub(crate) frames_by_frequency_mhz: BTreeMap<u16, u64>,
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

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[derive(Debug)]
struct Header {
    order: ByteOrder,
    nanoseconds: bool,
    snaplen: u32,
    linktype: u32,
    fcs_bytes: Option<usize>,
}

impl Header {
    // A mutable reference can be passed when the caller retains the reader.
    fn read(mut reader: impl Read) -> Result<Self> {
        let mut bytes = [0; 24];
        reader
            .read_exact(&mut bytes)
            .context("read PCAP global header")?;
        let (order, nanoseconds) = match &bytes[..4] {
            [0xd4, 0xc3, 0xb2, 0xa1] => (ByteOrder::Little, false),
            [0xa1, 0xb2, 0xc3, 0xd4] => (ByteOrder::Big, false),
            [0x4d, 0x3c, 0xb2, 0xa1] => (ByteOrder::Little, true),
            [0xa1, 0xb2, 0x3c, 0x4d] => (ByteOrder::Big, true),
            _ => bail!("expected classic PCAP; PCAPNG is unsupported"),
        };
        ensure!(
            order.u16(&bytes[4..]) == 2 && order.u16(&bytes[6..]) == 4,
            "unsupported PCAP version"
        );
        let snaplen = order.u32(&bytes[16..]);
        ensure!(
            snaplen > 0 && snaplen <= MAX_PACKET_BYTES as u32,
            "PCAP snaplen must be 1..={MAX_PACKET_BYTES}"
        );
        let network = order.u32(&bytes[20..]);
        ensure!(
            network & 0x0bff_0000 == 0,
            "reserved PCAP linktype metadata bits are set"
        );
        let linktype = network & 0xffff;
        ensure!(
            matches!(linktype, DLT_IEEE80211 | DLT_RADIOTAP),
            "unsupported Wi-Fi PCAP linktype {linktype}"
        );
        let fcs_bytes = (network & 0x0400_0000 != 0).then_some(((network >> 28) * 2) as usize);
        ensure!(
            matches!(fcs_bytes, None | Some(0 | 4)),
            "only absent or 4-byte IEEE 802.11 FCS metadata is supported"
        );
        Ok(Self {
            order,
            nanoseconds,
            snaplen,
            linktype,
            fcs_bytes,
        })
    }

    fn write_trusted(&self, mut output: impl Write) -> Result<()> {
        output.write_all(
            &if self.nanoseconds {
                0xa1b2_3c4d_u32
            } else {
                0xa1b2_c3d4_u32
            }
            .to_le_bytes(),
        )?;
        output.write_all(&2_u16.to_le_bytes())?;
        output.write_all(&4_u16.to_le_bytes())?;
        output.write_all(&[0; 8])?;
        output.write_all(&(MAX_PACKET_BYTES as u32).to_le_bytes())?;
        output.write_all(&DLT_RADIOTAP.to_le_bytes())?;
        Ok(())
    }
}

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
                ".wifi-packets-{}-{sequence}.tmp",
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
        bail!("could not reserve temporary output")
    }
    fn prepare(&mut self) -> Result<()> {
        self.writer.flush()?;
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

fn output_identity(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path.canonicalize().context("resolve existing output");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(parent
        .canonicalize()?
        .join(path.file_name().context("output needs a file name")?))
}

#[derive(Debug, Serialize)]
struct PacketRecord {
    index: u64,
    timestamp_seconds: u32,
    timestamp_fraction: u32,
    timestamp_precision: &'static str,
    validation: &'static str,
    packet_hex: String,
    radiotap: Option<Value>,
    frame: Option<Value>,
    warnings: Vec<&'static str>,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(DIGITS[usize::from(byte >> 4)] as char);
        output.push(DIGITS[usize::from(byte & 15)] as char);
    }
    output
}

fn address(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// IEEE CRC-32/ISO-HDLC, as used by the 802.11 FCS; transmitted little endian.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xedb8_8320 } else { 0 };
        }
    }
    !crc
}

#[derive(Debug)]
struct Radio {
    length: usize,
    flags: Option<u8>,
    frequency: Option<u16>,
    details: Value,
}

/// Read only standardized first-namespace fields. Unknown later fields retain
/// their raw header; `it_len` still locates the frame without invented lengths.
fn radiotap(bytes: &[u8]) -> Result<Radio> {
    ensure!(bytes.len() >= 8 && bytes[0] == 0, "invalid radiotap header");
    let length = usize::from(le16(&bytes[2..]));
    ensure!(
        (8..=bytes.len()).contains(&length),
        "radiotap length exceeds record"
    );
    let mut position = 4;
    let mut masks = Vec::new();
    loop {
        ensure!(
            position + 4 <= length,
            "truncated extended radiotap presence bitmap"
        );
        let mask = le32(&bytes[position..]);
        masks.push(mask);
        position += 4;
        if mask & (1 << 31) == 0 {
            break;
        }
    }
    let mask = masks[0];
    let mut flags = None;
    let mut frequency = None;
    let mut details = json!({"header_hex":hex(&bytes[..length]),"presence_words":masks});
    // Alignment is relative to the start of the radiotap header.
    for (bit, alignment, size) in [
        (0, 8, 8),
        (1, 1, 1),
        (2, 1, 1),
        (3, 2, 4),
        (4, 2, 2),
        (5, 1, 1),
        (6, 1, 1),
        (7, 2, 2),
        (8, 2, 2),
        (9, 2, 2),
        (10, 1, 1),
        (11, 1, 1),
        (12, 1, 1),
        (13, 1, 1),
        (14, 2, 2),
        (15, 2, 2),
        (16, 1, 1),
        (17, 1, 1),
        (18, 4, 8),
        (19, 1, 3),
        (20, 4, 8),
        (21, 2, 12),
    ] {
        if mask & (1 << bit) == 0 {
            continue;
        }
        position = (position + alignment - 1) & !(alignment - 1);
        ensure!(position + size <= length, "radiotap field exceeds header");
        let data = &bytes[position..position + size];
        match bit {
            0 => {
                details["tsft_microseconds"] = json!(u64::from_le_bytes(
                    data.try_into().expect("eight-byte TSFT")
                ))
            }
            1 => {
                flags = Some(data[0]);
                details["flags"] = json!(data[0]);
            }
            2 => details["legacy_rate_500kbps_units"] = json!(data[0]),
            3 => {
                frequency = Some(le16(data));
                details["frequency_mhz"] = json!(le16(data));
                details["channel_flags"] = json!(le16(&data[2..]));
            }
            5 => details["signal_reported_dbm"] = json!(data[0] as i8),
            6 => details["noise_reported_dbm"] = json!(data[0] as i8),
            11 => details["antenna_index"] = json!(data[0]),
            19 => details["mcs_raw_hex"] = json!(hex(data)),
            21 => details["vht_raw_hex"] = json!(hex(data)),
            _ => {}
        }
        position += size;
    }
    details["power_interpretation"] = json!("upstream_report_only_not_independently_calibrated");
    Ok(Radio {
        length,
        flags,
        frequency,
        details,
    })
}

fn suite(bytes: &[u8], akm: bool) -> Value {
    let oui = hex(&bytes[..3]);
    let kind = bytes[3];
    let known_rsn = bytes[..3] == [0, 0x0f, 0xac];
    let known_wpa = bytes[..3] == [0, 0x50, 0xf2];
    let name = if known_wpa
        && ((akm && !matches!(kind, 1 | 2)) || (!akm && !matches!(kind, 0 | 1 | 2 | 4 | 5)))
    {
        "unrecognized"
    } else if known_rsn || known_wpa {
        if akm {
            match kind {
                1 => "802.1X",
                2 => "PSK",
                3 => "FT-802.1X",
                4 => "FT-PSK",
                5 => "802.1X-SHA256",
                6 => "PSK-SHA256",
                8 => "SAE",
                9 => "FT-SAE",
                11 => "802.1X-SUITE-B",
                12 => "802.1X-SUITE-B-192",
                18 => "OWE",
                _ => "unrecognized",
            }
        } else {
            match kind {
                0 => "group_or_use_group",
                1 => "WEP40",
                2 => "TKIP",
                4 => "CCMP-128",
                5 => "WEP104",
                6 => "BIP-CMAC-128",
                8 => "GCMP-128",
                9 => "GCMP-256",
                10 => "CCMP-256",
                11 => "BIP-GMAC-128",
                12 => "BIP-GMAC-256",
                13 => "BIP-CMAC-256",
                _ => "unrecognized",
            }
        }
    } else {
        "vendor_specific"
    };
    json!({"oui":oui,"type":kind,"name":name})
}

fn security_element(bytes: &[u8], wpa: bool) -> Result<Value> {
    ensure!(bytes.len() >= 8, "short security IE");
    let mut position = 0;
    let version = le16(bytes);
    ensure!(version == 1, "unsupported security IE version");
    position += 2;
    let group = suite(&bytes[position..position + 4], false);
    position += 4;
    let pair_count = usize::from(le16(&bytes[position..]));
    position += 2;
    ensure!(
        pair_count <= (bytes.len() - position) / 4,
        "pairwise suite count exceeds security IE"
    );
    let pairwise: Vec<_> = bytes[position..position + pair_count * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|s| suite(s, false))
        .collect();
    position += pair_count * 4;
    ensure!(position + 2 <= bytes.len(), "missing AKM suite count");
    let akm_count = usize::from(le16(&bytes[position..]));
    position += 2;
    ensure!(
        akm_count <= (bytes.len() - position) / 4,
        "AKM suite count exceeds security IE"
    );
    let akms: Vec<_> = bytes[position..position + akm_count * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|s| suite(s, true))
        .collect();
    position += akm_count * 4;
    let mut result = json!({"version":version,"group_cipher":group,"pairwise_ciphers":pairwise,"akm_suites":akms});
    if position + 2 <= bytes.len() {
        let capabilities = le16(&bytes[position..]);
        position += 2;
        result["capabilities"] = json!(capabilities);
        if !wpa {
            result["pmf_required"] = json!(capabilities & 0x40 != 0);
            result["pmf_capable"] = json!(capabilities & 0x80 != 0);
        }
    }
    if !wpa && position + 2 <= bytes.len() {
        let count = usize::from(le16(&bytes[position..]));
        position += 2;
        ensure!(
            count <= (bytes.len() - position) / 16,
            "PMKID count exceeds security IE"
        );
        result["pmkid_count"] = json!(count);
        result["pmkids_hex"] = json!(hex(&bytes[position..position + count * 16]));
        position += count * 16;
        if position + 4 <= bytes.len() {
            result["group_management_cipher"] = suite(&bytes[position..position + 4], false);
            position += 4;
        }
    }
    result["remaining_hex"] = json!(hex(&bytes[position..]));
    Ok(result)
}

fn information_elements(
    bytes: &[u8],
    record: &mut PacketRecord,
    summary: &mut Summary,
) -> Vec<Value> {
    let mut fields = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        if bytes.len() - position < 2 {
            record.warnings.push("truncated_information_element_header");
            break;
        }
        let id = bytes[position];
        let length = usize::from(bytes[position + 1]);
        position += 2;
        if length > bytes.len() - position {
            record.warnings.push("truncated_information_element_body");
            break;
        }
        let data = &bytes[position..position + length];
        position += length;
        *summary
            .information_element_ids
            .entry(format!("0x{id:02x}"))
            .or_default() += 1;
        let mut field = json!({"id":id,"data_hex":hex(data)});
        match id {
            0 => {
                field["ssid_utf8"] = json!(std::str::from_utf8(data).ok());
                field["ssid_hidden_or_empty"] =
                    json!(data.is_empty() || data.iter().all(|&b| b == 0));
                if data.len() > 32 {
                    record.warnings.push("ssid_exceeds_32_octets");
                }
            }
            1 | 50 => {
                field["rates"] = json!(
                data.iter()
                    .map(|&byte| {
                        let selector = match byte {
                            0xfa => Some("HE_PHY"),
                            0xfb => Some("SAE_H2E_ONLY"),
                            0xfc => Some("EPD"),
                            0xfd => Some("GLK"),
                            0xfe => Some("VHT_PHY"),
                            0xff => Some("HT_PHY"),
                            _ => None,
                        };
                        if let Some(selector) = selector {
                            json!({"raw":byte,"bss_membership_selector":selector})
                        } else {
                            json!({"raw":byte,"mbps":f64::from(byte&0x7f)/2.0,"basic":byte&0x80!=0})
                        }
                    })
                    .collect::<Vec<_>>()
            )
            }
            3 if data.len() == 1 => field["advertised_channel"] = json!(data[0]),
            7 if data.len() >= 3 => field["country_string_hex"] = json!(hex(&data[..3])),
            45 => field["interpretation"] = json!("HT_capabilities_raw"),
            48 => match security_element(data, false) {
                Ok(value) => field["rsn"] = value,
                Err(_) => record.warnings.push("malformed_rsn_information_element"),
            },
            61 if !data.is_empty() => {
                field["interpretation"] = json!("HT_operation_raw");
                field["primary_channel"] = json!(data[0]);
            }
            191 => field["interpretation"] = json!("VHT_capabilities_raw"),
            192 => field["interpretation"] = json!("VHT_operation_raw"),
            221 if data.len() >= 4 => {
                field["vendor_oui"] = json!(hex(&data[..3]));
                field["vendor_type"] = json!(data[3]);
                if data[..4] == [0, 0x50, 0xf2, 1] {
                    match security_element(&data[4..], true) {
                        Ok(value) => field["wpa"] = value,
                        Err(_) => record.warnings.push("malformed_wpa_information_element"),
                    }
                }
            }
            255 if !data.is_empty() => {
                field["extension_id"] = json!(data[0]);
                field["interpretation"] = json!(match data[0] {
                    35 => "HE_capabilities_raw",
                    36 => "HE_operation_raw",
                    106 => "EHT_operation_raw",
                    108 => "EHT_capabilities_raw",
                    _ => "unrecognized_extension",
                });
            }
            _ => {}
        }
        fields.push(field);
    }
    fields
}

fn frame_name(kind: u8, subtype: u8) -> &'static str {
    match (kind, subtype) {
        (0, 0) => "association_request",
        (0, 1) => "association_response",
        (0, 2) => "reassociation_request",
        (0, 3) => "reassociation_response",
        (0, 4) => "probe_request",
        (0, 5) => "probe_response",
        (0, 8) => "beacon",
        (0, 9) => "ATIM",
        (0, 10) => "disassociation",
        (0, 11) => "authentication",
        (0, 12) => "deauthentication",
        (0, 13) => "action",
        (0, 14) => "action_no_ack",
        (0, _) => "reserved_management",
        (1, 7) => "control_wrapper",
        (1, 8) => "block_ack_request",
        (1, 9) => "block_ack",
        (1, 10) => "PS_POLL",
        (1, 11) => "RTS",
        (1, 12) => "CTS",
        (1, 13) => "ACK",
        (1, 14) => "CF_END",
        (1, 15) => "CF_END_ACK",
        (1, _) => "other_control",
        (2, 0) => "data",
        (2, 4) => "null_data",
        (2, 8) => "qos_data",
        (2, 12) => "qos_null",
        (2, _) => "other_data",
        _ => "extension_or_reserved",
    }
}

fn mac_header_length(frame: &[u8]) -> Result<usize> {
    ensure!(frame.len() >= 2, "short frame control");
    let fc = le16(frame);
    ensure!(fc & 3 == 0, "unsupported 802.11 protocol version");
    let kind = ((fc >> 2) & 3) as u8;
    let subtype = ((fc >> 4) & 15) as u8;
    let length = match kind {
        0 => 24 + if fc & 0x8000 != 0 { 4 } else { 0 },
        1 => {
            if matches!(subtype, 12 | 13) {
                10
            } else {
                16
            }
        }
        2 => {
            24 + if fc & 0x0300 == 0x0300 { 6 } else { 0 }
                + if subtype & 8 != 0 {
                    2 + if fc & 0x8000 != 0 { 4 } else { 0 }
                } else {
                    0
                }
        }
        _ => bail!("unsupported extension MAC header"),
    };
    ensure!(frame.len() >= length, "truncated 802.11 MAC header");
    Ok(length)
}

fn decode_frame(frame: &[u8], record: &mut PacketRecord, summary: &mut Summary) -> Result<Value> {
    let length = mac_header_length(frame)?;
    let fc = le16(frame);
    let kind = ((fc >> 2) & 3) as u8;
    let subtype = ((fc >> 4) & 15) as u8;
    let protected = fc & 0x4000 != 0;
    let name = frame_name(kind, subtype);
    *summary.frames_by_type.entry(name.to_owned()).or_default() += 1;
    if protected {
        summary.protected_frames += 1;
    }
    let mut details = json!({"frame_control":fc,"type":kind,"subtype":subtype,"name":name,"duration_or_id":le16(&frame[2..]),"to_ds":fc&0x0100!=0,"from_ds":fc&0x0200!=0,"more_fragments":fc&0x0400!=0,"retry":fc&0x0800!=0,"power_management":fc&0x1000!=0,"more_data":fc&0x2000!=0,"protected":protected,"order_or_ht_control":fc&0x8000!=0,"header_length":length,"receiver":address(&frame[4..10]),"payload_hex":hex(&frame[length..])});
    if length >= 16 && !(kind == 1 && subtype == 7) {
        details["transmitter"] = json!(address(&frame[10..16]));
    }
    if kind == 1 {
        summary.control_frames += 1;
        return Ok(details);
    }
    let a1 = address(&frame[4..10]);
    let a2 = address(&frame[10..16]);
    let a3 = address(&frame[16..22]);
    let sequence = le16(&frame[22..]);
    details["sequence_number"] = json!(sequence >> 4);
    details["fragment_number"] = json!(sequence & 15);
    if kind == 0 {
        details["destination"] = json!(a1);
        details["source"] = json!(a2);
        details["bssid"] = json!(a3);
    } else {
        match (fc & 0x0100 != 0, fc & 0x0200 != 0) {
            (false, false) => {
                details["destination"] = json!(a1);
                details["source"] = json!(a2);
                details["bssid"] = json!(a3);
            }
            (true, false) => {
                details["bssid"] = json!(a1);
                details["source"] = json!(a2);
                details["destination"] = json!(a3);
            }
            (false, true) => {
                details["destination"] = json!(a1);
                details["bssid"] = json!(a2);
                details["source"] = json!(a3);
            }
            (true, true) => {
                details["destination"] = json!(a3);
                details["source"] = json!(address(&frame[24..30]));
            }
        }
    }
    // QoS and HT control are MAC header fields even when the body is encrypted.
    let qos = if kind == 2 && subtype & 8 != 0 {
        let position = 24 + if fc & 0x0300 == 0x0300 { 6 } else { 0 };
        let qos = le16(&frame[position..]);
        details["qos"] =
            json!({"tid":qos&15,"ack_policy":(qos>>5)&3,"amsdu_present":qos&0x80!=0,"raw":qos});
        Some(qos)
    } else {
        None
    };
    if fc & 0x8000 != 0 && (kind == 0 || qos.is_some()) {
        details["ht_control_raw_hex"] = json!(hex(&frame[length - 4..length]));
    }
    if protected {
        details["payload_interpretation"] = json!("protected_payload_opaque");
        if kind == 0 {
            summary.management_frames += 1;
        } else {
            summary.data_frames += 1;
        }
        return Ok(details);
    }
    if fc & 0x0400 != 0 || sequence & 15 != 0 {
        if kind == 0 {
            summary.management_frames += 1;
        } else {
            summary.data_frames += 1;
        }
        details["payload_interpretation"] = json!("fragment_not_reassembled");
        return Ok(details);
    }
    let payload = &frame[length..];
    if kind == 0 {
        summary.management_frames += 1;
        let fixed = match subtype {
            0 => 4,
            1 | 3 => 6,
            2 => 10,
            4 => 0,
            5 | 8 => 12,
            10 | 12 => 2,
            11 => 6,
            13 | 14 => 0,
            _ => 0,
        };
        if payload.len() < fixed {
            record.warnings.push("truncated_management_fixed_fields");
            return Ok(details);
        }
        match subtype {
            0 | 2 => {
                details["capabilities"] = json!(le16(payload));
                details["listen_interval"] = json!(le16(&payload[2..]));
                if subtype == 2 {
                    details["current_ap"] = json!(address(&payload[4..10]));
                }
            }
            1 | 3 => {
                details["capabilities"] = json!(le16(payload));
                details["status_code"] = json!(le16(&payload[2..]));
                details["association_id"] = json!(le16(&payload[4..]) & 0x3fff);
            }
            5 | 8 => {
                details["beacon_timestamp_us"] = json!(u64::from_le_bytes(
                    payload[..8].try_into().expect("eight-byte timestamp")
                ));
                details["beacon_interval_tu"] = json!(le16(&payload[8..]));
                let cap = le16(&payload[10..]);
                details["capabilities"] = json!(cap);
                details["privacy_capability"] = json!(cap & 0x10 != 0);
            }
            10 | 12 => details["reason_code"] = json!(le16(payload)),
            11 => {
                details["authentication_algorithm"] = json!(le16(payload));
                details["authentication_sequence"] = json!(le16(&payload[2..]));
                details["status_code"] = json!(le16(&payload[4..]));
            }
            13 | 14 => {
                if let Some(category) = payload.first() {
                    details["action_category"] = json!(category);
                }
                return Ok(details);
            }
            _ => {}
        }
        // SAE commit/confirm and other algorithm-specific authentication data
        // are not an arbitrary sequence of information elements.
        if matches!(subtype, 0..=5 | 8) || (subtype == 11 && matches!(le16(payload), 0..=2)) {
            details["information_elements"] =
                json!(information_elements(&payload[fixed..], record, summary));
        }
    } else {
        summary.data_frames += 1;
        if let Some(qos) = qos {
            if fc & 0x0300 == 0x0300 && qos & 0x0100 != 0 {
                record.warnings.push("mesh_control_not_decoded");
                details["payload_interpretation"] = json!("possible_mesh_payload_opaque");
                return Ok(details);
            }
            if qos & 0x80 != 0 {
                details["payload_interpretation"] = json!("amsdu_not_reassembled");
                return Ok(details);
            }
        }
        if fc & 0x0400 != 0 || sequence & 15 != 0 {
            details["payload_interpretation"] = json!("fragment_not_reassembled");
            return Ok(details);
        }
        if subtype & 4 != 0 {
            details["payload_interpretation"] = json!("data_subtype_without_msdu");
            return Ok(details);
        }
        if payload.len() >= 8 && payload[..3] == [0xaa, 0xaa, 3] {
            let oui = hex(&payload[3..6]);
            let ether = u16::from_be_bytes([payload[6], payload[7]]);
            details["llc_snap"] = json!({"oui":oui,"ethertype":format!("0x{ether:04x}")});
            if payload[3..6] == [0, 0, 0] && ether == 0x888e {
                summary.eapol_frames += 1;
                if payload.len() >= 12 {
                    let size = usize::from(u16::from_be_bytes([payload[10], payload[11]]));
                    let mut eapol = json!({"version":payload[8],"packet_type":payload[9],"declared_body_length":size});
                    if size > payload.len() - 12 {
                        record.warnings.push("truncated_eapol_body");
                    } else {
                        eapol["body_hex"] = json!(hex(&payload[12..12 + size]));
                        if payload[9] == 3 && size >= 3 {
                            eapol["key_descriptor_type"] = json!(payload[12]);
                            let info = u16::from_be_bytes([payload[13], payload[14]]);
                            eapol["key_information"] = json!(info);
                            eapol["pairwise"] = json!(info & 8 != 0);
                            eapol["install"] = json!(info & 0x40 != 0);
                            eapol["ack"] = json!(info & 0x80 != 0);
                            eapol["mic_present"] = json!(info & 0x100 != 0);
                            eapol["secure"] = json!(info & 0x200 != 0);
                        }
                    }
                    details["eapol"] = eapol;
                } else {
                    record.warnings.push("truncated_eapol_header");
                }
            }
        }
    }
    Ok(details)
}

fn analyze(
    bytes: &[u8],
    header: &Header,
    record: &mut PacketRecord,
    summary: &mut Summary,
) -> Option<Vec<u8>> {
    let mut frame = bytes;
    let mut indicated = header.fcs_bytes.map(|length| length == 4);
    let mut padded = false;
    let mut frequency = None;
    if header.linktype == DLT_RADIOTAP {
        let radio = match radiotap(bytes) {
            Ok(value) => value,
            Err(_) => {
                summary.malformed_frames += 1;
                record.warnings.push("malformed_radiotap");
                return None;
            }
        };
        frame = &bytes[radio.length..];
        frequency = radio.frequency;
        if let Some(flags) = radio.flags {
            let present = flags & FCS_PRESENT != 0;
            if indicated.is_some_and(|global| global != present) {
                summary.malformed_frames += 1;
                record.warnings.push("conflicting_fcs_metadata");
                return None;
            }
            indicated = Some(present);
            padded = flags & DATA_PAD != 0;
            if flags & BAD_FCS != 0 {
                summary.upstream_bad_fcs_claims += 1;
            }
        }
        record.radiotap = Some(radio.details);
    }
    let mut normalized = frame.to_vec();
    if padded {
        let length = match mac_header_length(frame) {
            Ok(value) => value,
            Err(_) => {
                summary.malformed_frames += 1;
                record.warnings.push("unsupported_padded_mac_header");
                return None;
            }
        };
        let pad = (4 - length % 4) % 4;
        if length + pad > frame.len() {
            summary.malformed_frames += 1;
            record.warnings.push("truncated_mac_padding");
            return None;
        }
        normalized.drain(length..length + pad);
    }
    let frame = normalized.as_slice();
    match indicated {
        None => {
            summary.unknown_fcs_frames += 1;
            record.validation = "fcs_unknown";
            return None;
        }
        Some(false) => {
            summary.absent_fcs_frames += 1;
            record.validation = "fcs_absent";
            return None;
        }
        Some(true) => {}
    }
    if frame.len() < 6 {
        summary.malformed_frames += 1;
        record.warnings.push("short_frame_with_fcs");
        return None;
    }
    if frame.len() + MINIMAL_RADIOTAP.len() > MAX_PACKET_BYTES {
        summary.malformed_frames += 1;
        record.warnings.push("normalized_frame_exceeds_record_cap");
        return None;
    }
    let body = &frame[..frame.len() - 4];
    if crc32(body) != le32(&frame[frame.len() - 4..]) {
        summary.invalid_fcs_frames += 1;
        record.validation = "fcs_invalid";
        return None;
    }
    let mut counted = Summary::default();
    match decode_frame(body, record, &mut counted) {
        Ok(details) => record.frame = Some(details),
        Err(_) => {
            summary.malformed_frames += 1;
            record.validation = "fcs_valid_malformed_mac";
            record.warnings.push("malformed_80211_header");
            return None;
        }
    }
    record.validation = "fcs_valid";
    summary.validated_frames += 1;
    summary.management_frames += counted.management_frames;
    summary.control_frames += counted.control_frames;
    summary.data_frames += counted.data_frames;
    summary.protected_frames += counted.protected_frames;
    summary.eapol_frames += counted.eapol_frames;
    for (key, value) in counted.frames_by_type {
        *summary.frames_by_type.entry(key).or_default() += value;
    }
    for (key, value) in counted.information_element_ids {
        *summary.information_element_ids.entry(key).or_default() += value;
    }
    if let Some(frequency) = frequency {
        *summary
            .frames_by_frequency_mhz
            .entry(frequency)
            .or_default() += 1;
    }
    let mut trusted = Vec::with_capacity(MINIMAL_RADIOTAP.len() + frame.len());
    trusted.extend_from_slice(&MINIMAL_RADIOTAP);
    trusted.extend_from_slice(frame);
    Some(trusted)
}

/// Parse a capture without opening hardware. Trusted output retains actual FCS
/// bytes and uses minimal radiotap, dropping unverified RF-power metadata.
/// Publication preserves existing paths; two outputs are not a transaction and
/// failure of the second publication may leave the first completed file.
///
/// # Errors
///
/// Rejects unsupported capture formats, invalid PCAP lengths or timestamps,
/// truncated file records, output aliases and existing destinations. I/O and
/// serialization failures propagate with context. Malformed packet contents
/// are counted and retained in JSONL rather than becoming trusted frames.
pub(crate) fn inspect(
    input: &Path,
    validated_output: Option<&Path>,
    jsonl_output: Option<&Path>,
) -> Result<Summary> {
    inspect_records(input, validated_output, jsonl_output, None, |_| Ok(()))
}

/// A view of one independently FCS-checked frame with a supported MAC layout.
/// Packet time is the PCAP timestamp, whose clock basis the caller must retain.
#[derive(Debug)]
pub(crate) struct ValidatedPacket<'a> {
    pub(crate) timestamp_ns: u64,
    pub(crate) frame: &'a Value,
    pub(crate) reported_frequency_mhz: Option<u16>,
    pub(crate) warnings: &'a [&'static str],
}

/// Visit validated records from a bounded regular capture, without writing files.
/// Malformed, missing-FCS and corrupt records contribute only to the summary.
///
/// # Errors
///
/// Returns the inspector's format/read errors, a regular-file/byte/record bound
/// error, or the visitor's error. Earlier visitor calls are not rolled back;
/// callers needing transactional aggregation should stage their own state.
pub(crate) fn visit_validated(
    input: &Path,
    max_records: u64,
    max_input_bytes: u64,
    mut visitor: impl FnMut(ValidatedPacket<'_>) -> Result<()>,
) -> Result<Summary> {
    ensure!(
        max_records > 0 && max_input_bytes > 0,
        "input bounds must be positive"
    );
    let metadata = fs::metadata(input).context("inspect inventory input")?;
    ensure!(
        metadata.is_file(),
        "inventory input must be a regular PCAP file"
    );
    ensure!(
        metadata.len() <= max_input_bytes,
        "inventory input exceeds its byte limit"
    );
    inspect_records(
        input,
        None,
        None,
        Some((max_records, max_input_bytes)),
        |record| {
            if record.validation == "fcs_valid" {
                let fraction = u64::from(record.timestamp_fraction)
                    * if record.timestamp_precision == "nanoseconds" {
                        1
                    } else {
                        1_000
                    };
                visitor(ValidatedPacket {
                    timestamp_ns: u64::from(record.timestamp_seconds) * 1_000_000_000 + fraction,
                    frame: record
                        .frame
                        .as_ref()
                        .context("validated frame lacks decoded fields")?,
                    reported_frequency_mhz: record
                        .radiotap
                        .as_ref()
                        .and_then(|radio| radio["frequency_mhz"].as_u64())
                        .and_then(|frequency| u16::try_from(frequency).ok()),
                    warnings: &record.warnings,
                })?;
            }
            Ok(())
        },
    )
}

fn inspect_records(
    input: &Path,
    validated_output: Option<&Path>,
    jsonl_output: Option<&Path>,
    input_limits: Option<(u64, u64)>,
    mut visitor: impl FnMut(&PacketRecord) -> Result<()>,
) -> Result<Summary> {
    if let (Some(pcap), Some(jsonl)) = (validated_output, jsonl_output) {
        ensure!(
            output_identity(pcap)? != output_identity(jsonl)?,
            "PCAP and JSONL outputs must differ"
        );
    }
    let file = File::open(input).with_context(|| format!("open {}", input.display()))?;
    if let Some((_, bytes)) = input_limits {
        let metadata = file.metadata().context("inspect opened inventory input")?;
        ensure!(
            metadata.is_file() && metadata.len() <= bytes,
            "inventory input must remain a regular file within its byte limit"
        );
    }
    let mut reader = BufReader::new(file);
    let header = Header::read(&mut reader)?;
    let mut trusted = validated_output.map(Output::new).transpose()?;
    let mut jsonl = jsonl_output.map(Output::new).transpose()?;
    if let Some(output) = &mut trusted {
        header.write_trusted(&mut output.writer)?;
    }
    let mut summary = Summary {
        input_linktype: header.linktype,
        ..Summary::default()
    };
    let mut consumed = 24_u64;
    loop {
        let mut raw = [0; 16];
        if reader.read(&mut raw[..1]).context("read PCAP boundary")? == 0 {
            break;
        }
        reader
            .read_exact(&mut raw[1..])
            .context("truncated PCAP record header")?;
        let seconds = header.order.u32(&raw);
        let fraction = header.order.u32(&raw[4..]);
        let included = header.order.u32(&raw[8..]);
        let original = header.order.u32(&raw[12..]);
        ensure!(
            fraction
                < if header.nanoseconds {
                    1_000_000_000
                } else {
                    1_000_000
                },
            "invalid timestamp fraction"
        );
        ensure!(
            included <= MAX_PACKET_BYTES as u32,
            "PCAP record exceeds {MAX_PACKET_BYTES}-byte allocation cap"
        );
        ensure!(
            original >= included,
            "PCAP original length smaller than captured length"
        );
        consumed = consumed
            .checked_add(16 + u64::from(included))
            .context("PCAP byte count overflow")?;
        ensure!(
            input_limits.is_none_or(|(records, bytes)| summary.total_records < records && consumed <= bytes),
            "inventory input exceeds its record or byte limit"
        );
        let mut bytes = vec![0; included as usize];
        reader
            .read_exact(&mut bytes)
            .context("truncated PCAP record body")?;
        summary.total_records += 1;
        if included > header.snaplen {
            summary.records_exceeding_snaplen += 1;
        }
        let mut record = PacketRecord {
            index: summary.total_records,
            timestamp_seconds: seconds,
            timestamp_fraction: fraction,
            timestamp_precision: if header.nanoseconds {
                "nanoseconds"
            } else {
                "microseconds"
            },
            validation: "malformed",
            packet_hex: hex(&bytes),
            radiotap: None,
            frame: None,
            warnings: Vec::new(),
        };
        let valid = if original != included {
            summary.truncated_records += 1;
            record.validation = "capture_truncated";
            None
        } else {
            analyze(&bytes, &header, &mut record, &mut summary)
        };
        visitor(&record)?;
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
                    "wifi-packet-test-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create test directory: {error}"),
                }
            }
            panic!("test directory unavailable")
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    // Synthetic locally administered addresses/SSID. Trailer was generated by
    // Python zlib, independently of this implementation, for this exact frame.
    fn beacon() -> Vec<u8> {
        unhex(
            "80000000ffffffffffff02000000000102000000000100000000000000000000640011040008746573742d6e6574010482848b9603010630140100000fac040100000fac040100000fac02c000bff035d8",
        )
    }

    fn radio(frame: &[u8], flags: u8) -> Vec<u8> {
        let mut packet = MINIMAL_RADIOTAP.to_vec();
        packet[8] = flags;
        packet.extend_from_slice(frame);
        packet
    }

    fn header(linktype: u32) -> Header {
        Header {
            order: ByteOrder::Little,
            nanoseconds: false,
            snaplen: 65536,
            linktype,
            fcs_bytes: None,
        }
    }

    fn record() -> PacketRecord {
        PacketRecord {
            index: 1,
            timestamp_seconds: 1,
            timestamp_fraction: 2,
            timestamp_precision: "microseconds",
            validation: "malformed",
            packet_hex: String::new(),
            radiotap: None,
            frame: None,
            warnings: Vec::new(),
        }
    }

    fn pcap(order: ByteOrder, nanoseconds: bool, network: u32, packets: &[Vec<u8>]) -> Vec<u8> {
        fn put16(out: &mut Vec<u8>, value: u16, order: ByteOrder) {
            out.extend_from_slice(&match order {
                ByteOrder::Little => value.to_le_bytes(),
                ByteOrder::Big => value.to_be_bytes(),
            });
        }
        fn put32(out: &mut Vec<u8>, value: u32, order: ByteOrder) {
            out.extend_from_slice(&match order {
                ByteOrder::Little => value.to_le_bytes(),
                ByteOrder::Big => value.to_be_bytes(),
            });
        }
        let mut output = Vec::new();
        put32(
            &mut output,
            if nanoseconds {
                0xa1b2_3c4d
            } else {
                0xa1b2_c3d4
            },
            order,
        );
        put16(&mut output, 2, order);
        put16(&mut output, 4, order);
        output.extend_from_slice(&[0; 8]);
        put32(&mut output, 65536, order);
        put32(&mut output, network, order);
        for packet in packets {
            put32(&mut output, 1, order);
            put32(&mut output, 2, order);
            put32(&mut output, packet.len() as u32, order);
            put32(&mut output, packet.len() as u32, order);
            output.extend_from_slice(packet);
        }
        output
    }

    #[test]
    fn independent_crc_vector_and_beacon_fields() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        let frame = beacon();
        assert_eq!(crc32(&frame[..frame.len() - 4]), 0xd835_f0bf);
        let mut summary = Summary::default();
        let mut rec = record();
        let trusted = analyze(
            &radio(&frame, FCS_PRESENT | BAD_FCS),
            &header(DLT_RADIOTAP),
            &mut rec,
            &mut summary,
        )
        .unwrap();
        assert_eq!(summary.validated_frames, 1);
        assert_eq!(summary.upstream_bad_fcs_claims, 1);
        assert_eq!(trusted, radio(&frame, FCS_PRESENT));
        assert_eq!(rec.validation, "fcs_valid");
        let details = rec.frame.unwrap();
        assert_eq!(details["name"], "beacon");
        assert_eq!(details["beacon_interval_tu"], 100);
        let fields = details["information_elements"].as_array().unwrap();
        assert_eq!(fields[0]["ssid_utf8"], "test-net");
        assert_eq!(fields[2]["advertised_channel"], 6);
        assert_eq!(fields[3]["rsn"]["akm_suites"][0]["name"], "PSK");
        assert_eq!(fields[3]["rsn"]["pmf_required"], true);
        assert_eq!(fields[3]["rsn"]["pmf_capable"], true);
    }

    #[test]
    fn missing_or_corrupt_fcs_never_becomes_trusted() {
        let frame = beacon();
        let mut summary = Summary::default();
        assert!(analyze(&frame, &header(DLT_IEEE80211), &mut record(), &mut summary).is_none());
        assert_eq!(summary.unknown_fcs_frames, 1);
        assert!(
            analyze(
                &radio(&frame, 0),
                &header(DLT_RADIOTAP),
                &mut record(),
                &mut summary
            )
            .is_none()
        );
        assert_eq!(summary.absent_fcs_frames, 1);
        let mut bad = frame;
        *bad.last_mut().unwrap() ^= 1;
        assert!(
            analyze(
                &radio(&bad, FCS_PRESENT),
                &header(DLT_RADIOTAP),
                &mut record(),
                &mut summary
            )
            .is_none()
        );
        assert_eq!(summary.invalid_fcs_frames, 1);
        let mut metadata = header(DLT_RADIOTAP);
        metadata.fcs_bytes = Some(0);
        assert!(
            analyze(
                &radio(&bad, FCS_PRESENT),
                &metadata,
                &mut record(),
                &mut summary
            )
            .is_none()
        );
        assert_eq!(summary.malformed_frames, 1);
    }

    #[test]
    fn radiotap_alignment_extensions_and_bounds() {
        // Extended presence words put TSFT at offset16; FLAGS and CHANNEL follow.
        let mut bytes = vec![0, 0, 30, 0];
        bytes.extend_from_slice(&0x8000_000bu32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&42u64.to_le_bytes());
        bytes.push(FCS_PRESENT);
        bytes.push(0);
        bytes.extend_from_slice(&2412u16.to_le_bytes());
        bytes.extend_from_slice(&0x0080u16.to_le_bytes());
        bytes.extend_from_slice(&beacon());
        let parsed = radiotap(&bytes).unwrap();
        assert_eq!(parsed.length, 30);
        assert_eq!(parsed.flags, Some(FCS_PRESENT));
        assert_eq!(parsed.frequency, Some(2412));
        assert_eq!(parsed.details["tsft_microseconds"], 42);
        for malformed in [
            vec![0; 7],
            vec![0, 0, 99, 0, 0, 0, 0, 0],
            vec![0, 0, 8, 0, 0, 0, 0, 0x80],
            vec![0, 0, 8, 0, 2, 0, 0, 0],
        ] {
            assert!(radiotap(&malformed).is_err());
        }
    }

    fn data(fc: u16, qos: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0; 24];
        frame[..2].copy_from_slice(&fc.to_le_bytes());
        frame[4..10].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        frame[10..16].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
        frame[16..22].copy_from_slice(&[2, 0, 0, 0, 0, 3]);
        if fc & 0x0300 == 0x0300 {
            frame.extend_from_slice(&[2, 0, 0, 0, 0, 4]);
        }
        if fc & 0x0080 != 0 {
            frame.extend_from_slice(&qos.to_le_bytes());
            if fc & 0x8000 != 0 {
                frame.extend_from_slice(&[0; 4]);
            }
        }
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn qos_four_address_eapol_and_protected_payload_boundaries() {
        let payload = unhex("aaaa03000000888e020300030201c0");
        let frame = data(0x0388, 5, &payload);
        let mut rec = record();
        let mut summary = Summary::default();
        let parsed = decode_frame(&frame, &mut rec, &mut summary).unwrap();
        assert_eq!(parsed["header_length"], 32);
        assert_eq!(parsed["source"], "02:00:00:00:00:04");
        assert_eq!(parsed["qos"]["tid"], 5);
        assert_eq!(parsed["eapol"]["packet_type"], 3);
        assert_eq!(summary.eapol_frames, 1);
        assert!(decode_frame(&frame[..31], &mut record(), &mut Summary::default()).is_err());
        let protected = data(0xc388, 7, &payload);
        let mut encrypted_summary = Summary::default();
        let details = decode_frame(&protected, &mut record(), &mut encrypted_summary).unwrap();
        assert!(details.get("eapol").is_none());
        assert_eq!(
            details["payload_interpretation"],
            "protected_payload_opaque"
        );
        assert_eq!(encrypted_summary.eapol_frames, 0);
        assert_eq!(details["qos"]["tid"], 7);
        assert_eq!(details["ht_control_raw_hex"], "00000000");
        let aggregated = data(0x0088, 0x80, &payload);
        let details = decode_frame(&aggregated, &mut record(), &mut Summary::default()).unwrap();
        assert!(details.get("eapol").is_none());
        assert_eq!(details["payload_interpretation"], "amsdu_not_reassembled");
        let null = data(0x00c8, 0, &payload);
        let mut null_summary = Summary::default();
        let details = decode_frame(&null, &mut record(), &mut null_summary).unwrap();
        assert!(details.get("eapol").is_none());
        assert_eq!(null_summary.eapol_frames, 0);
        assert_eq!(
            details["payload_interpretation"],
            "data_subtype_without_msdu"
        );
    }

    #[test]
    fn algorithm_specific_authentication_body_is_not_an_ssid_ie() {
        let mut frame = vec![0; 24];
        frame[..2].copy_from_slice(&0x00b0_u16.to_le_bytes());
        // SAE algorithm 3 followed by synthetic scalar-looking bytes which
        // happen to resemble a TLV; those bytes are not a generic SSID IE.
        frame.extend_from_slice(&[3, 0, 1, 0, 0, 0, 0, 4, b't', b'e', b's', b't']);
        let mut summary = Summary::default();
        let details = decode_frame(&frame, &mut record(), &mut summary).unwrap();
        assert_eq!(details["authentication_algorithm"], 3);
        assert!(details.get("information_elements").is_none());
        assert!(summary.information_element_ids.is_empty());
        assert!(security_element(&[2, 0, 0, 15, 172, 4, 0, 0, 0, 0], false).is_err());
        let fields = information_elements(
            &[1, 3, 0x82, 0xfa, 0xff],
            &mut record(),
            &mut Summary::default(),
        );
        assert_eq!(fields[0]["rates"][0]["mbps"], 1.0);
        assert_eq!(fields[0]["rates"][1]["bss_membership_selector"], "HE_PHY");
        assert!(fields[0]["rates"][1].get("mbps").is_none());
        assert_eq!(fields[0]["rates"][2]["bss_membership_selector"], "HT_PHY");
    }

    #[test]
    fn extension_ie_ids_distinguish_eht_operation_from_tid_mapping() {
        let extensions = information_elements(
            &[255, 1, 106, 255, 1, 108, 255, 1, 109],
            &mut record(),
            &mut Summary::default(),
        );
        assert_eq!(extensions[0]["interpretation"], "EHT_operation_raw");
        assert_eq!(extensions[1]["interpretation"], "EHT_capabilities_raw");
        assert_eq!(extensions[2]["interpretation"], "unrecognized_extension");
    }

    #[test]
    fn datapad_is_removed_before_fcs_check() {
        let mut frame = data(0x0088, 0, &[0xaa, 0xaa, 3, 0, 0, 0, 8, 0]);
        let checksum = crc32(&frame);
        frame.extend_from_slice(&checksum.to_le_bytes());
        let mut padded = frame.clone();
        padded.splice(26..26, [0, 0]);
        let mut summary = Summary::default();
        let trusted = analyze(
            &radio(&padded, FCS_PRESENT | DATA_PAD),
            &header(DLT_RADIOTAP),
            &mut record(),
            &mut summary,
        )
        .unwrap();
        assert_eq!(trusted, radio(&frame, FCS_PRESENT));
    }

    #[test]
    fn malformed_security_and_ie_lengths_preserve_raw_candidates() {
        let mut rec = record();
        let mut summary = Summary::default();
        let fields = information_elements(
            &[
                0, 1, b'a', 48, 2, 1, 0, 221, 4, 0, 0x50, 0xf2, 1, 255, 1, 35, 1, 9, 0,
            ],
            &mut rec,
            &mut summary,
        );
        assert_eq!(fields.len(), 4);
        assert!(rec.warnings.contains(&"malformed_rsn_information_element"));
        assert!(rec.warnings.contains(&"malformed_wpa_information_element"));
        assert!(rec.warnings.contains(&"truncated_information_element_body"));
        assert_eq!(fields[3]["interpretation"], "HE_capabilities_raw");
        assert!(security_element(&[1, 0, 0, 0x0f, 0xac, 4, 255, 255], false).is_err());
        assert_eq!(suite(&[0, 0x50, 0xf2, 8], true)["name"], "unrecognized");
    }

    #[test]
    fn pcap_orders_precisions_global_fcs_metadata_and_no_overwrite() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            for nano in [false, true] {
                let dir = TestDirectory::new();
                let input = dir.0.join("input.pcap");
                let trusted = dir.0.join("valid.pcap");
                let jsonl = dir.0.join("packets.jsonl");
                fs::write(
                    &input,
                    pcap(order, nano, DLT_IEEE80211 | 0x2400_0000, &[beacon()]),
                )
                .unwrap();
                let summary = inspect(&input, Some(&trusted), Some(&jsonl)).unwrap();
                assert_eq!(summary.validated_frames, 1);
                assert_eq!(inspect(&trusted, None, None).unwrap().validated_frames, 1);
                let parsed: Value =
                    serde_json::from_str(fs::read_to_string(&jsonl).unwrap().trim()).unwrap();
                assert_eq!(
                    parsed["timestamp_precision"],
                    if nano { "nanoseconds" } else { "microseconds" }
                );
                let saved = fs::read(&trusted).unwrap();
                assert!(inspect(&input, Some(&trusted), None).is_err());
                assert_eq!(fs::read(&trusted).unwrap(), saved);
            }
        }
    }

    #[test]
    fn truncated_and_oversized_pcap_never_publishes_partial_outputs() {
        let dir = TestDirectory::new();
        let input = dir.0.join("input.pcap");
        let trusted = dir.0.join("valid.pcap");
        let jsonl = dir.0.join("packets.jsonl");
        let mut capture = pcap(
            ByteOrder::Little,
            false,
            DLT_RADIOTAP,
            &[radio(&beacon(), FCS_PRESENT)],
        );
        capture.pop();
        fs::write(&input, &capture).unwrap();
        assert!(inspect(&input, Some(&trusted), Some(&jsonl)).is_err());
        assert!(!trusted.exists() && !jsonl.exists());
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        let mut capture = pcap(ByteOrder::Little, false, DLT_RADIOTAP, &[]);
        capture.extend_from_slice(&[0; 8]);
        capture.extend_from_slice(&65537u32.to_le_bytes());
        capture.extend_from_slice(&65537u32.to_le_bytes());
        fs::write(&input, capture).unwrap();
        assert!(inspect(&input, Some(&trusted), None).is_err());
        assert!(!trusted.exists());
    }
}
