//! Check the worker's bounded packet receipt before publishing it.

use std::fs::Metadata;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use serde::{Deserialize, Serialize};

use crate::file_pin;

use super::{
    DecodeRequest, PacketInterpretation, Translated, lorawan::JoinRequest, meshtastic::Header,
};

const MAX_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 255;

#[derive(Deserialize)]
struct Record {
    synchronization_sample: u64,
    code_rate_index: u8,
    observed_sync_symbols: [u16; 2],
    payload_hex: String,
    payload_crc: u16,
}

#[derive(Deserialize)]
struct Receipt {
    schema_version: u32,
    operation: String,
    source_format: String,
    source_sample_count: u64,
    source_sha256: String,
    channel_hz: u64,
    bandwidth_hz: u32,
    sample_rate_hz: u32,
    spreading_factor: u8,
    sync_word: u8,
    expected_sync_symbols: [u16; 2],
    max_frames: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    rejected_crc_absent: usize,
    rejected_invalid_crc: usize,
    rejected_wrong_sync: usize,
    records: Vec<Record>,
}

/// Optional interpretations of the retained CRC-valid LoRa records.
#[derive(Serialize)]
pub(super) struct Interpretations {
    #[serde(skip_serializing_if = "Option::is_none")]
    meshtastic: Option<Records<Header>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lorawan_join: Option<Records<JoinRequest>>,
}

impl Interpretations {
    /// Count retained frames whose clear headers fit the Meshtastic format.
    pub(super) fn meshtastic_frames(&self) -> Option<usize> {
        self.meshtastic.as_ref().map(|report| report.records.len())
    }

    /// Count retained frames with a compatible LoRaWAN Join-request layout.
    pub(super) fn lorawan_join_requests(&self) -> Option<usize> {
        self.lorawan_join
            .as_ref()
            .map(|report| report.records.len())
    }
}

#[derive(Serialize)]
struct Records<T> {
    checked_retained_frames: usize,
    incompatible_retained_frames: usize,
    records: Vec<Observation<T>>,
}

impl<T> Records<T> {
    fn new(checked_retained_frames: usize, records: Vec<Observation<T>>) -> Self {
        Self {
            checked_retained_frames,
            incompatible_retained_frames: checked_retained_frames - records.len(),
            records,
        }
    }
}

#[derive(Serialize)]
struct Observation<T> {
    worker_record_index: usize,
    synchronization_sample: u64,
    #[serde(flatten)]
    fields: T,
}

impl<T> Observation<T> {
    fn new(index: usize, record: &Record, fields: T) -> Self {
        Self {
            worker_record_index: index,
            synchronization_sample: record.synchronization_sample,
            fields,
        }
    }
}

pub(super) struct Saved {
    sha256: String,
    stamp: Metadata,
}

impl Saved {
    pub(super) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(super) fn verify(&self, path: &Path) -> Result<()> {
        verify_stamp(path, &self.stamp, "decode")
    }
}

pub(super) struct Verified {
    pub(super) validated_frames: usize,
    pub(super) omitted_valid_frames: usize,
    saved: Saved,
}

impl Verified {
    pub(super) fn sha256(&self) -> &str {
        self.saved.sha256()
    }

    pub(super) fn verify(&self, path: &Path) -> Result<()> {
        verify_stamp(path, &self.saved.stamp, "worker")
    }
}

pub(super) fn save(path: &Path, report: &impl Serialize) -> Result<Saved> {
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    let mut file = crate::new_file(path)?;
    file.write_all(&bytes)
        .context("write LoRa decode receipt")?;
    file.sync_all().context("sync LoRa decode receipt")?;
    let (saved, stamp) = read_bytes(path, bytes.len() as u64, "decode")?;
    ensure!(
        saved == bytes && file_pin::same_file(&stamp, &file.metadata()?),
        "LoRa decode receipt changed while writing"
    );
    Ok(Saved {
        sha256: sha256(&bytes),
        stamp,
    })
}

pub(super) fn read(
    path: &Path,
    request: &DecodeRequest,
    translated: &Translated,
) -> Result<(Verified, Interpretations)> {
    let (bytes, stamp) = read_bytes(path, MAX_RECEIPT_BYTES, "worker")?;
    let receipt: Receipt = serde_json::from_slice(&bytes).context("parse LoRa worker receipt")?;
    let interpretations = receipt.validate(request, translated)?;
    Ok((
        Verified {
            validated_frames: receipt.validated_frames,
            omitted_valid_frames: receipt.omitted_valid_frames,
            saved: Saved {
                sha256: sha256(&bytes),
                stamp,
            },
        },
        interpretations,
    ))
}

fn read_bytes(path: &Path, maximum: u64, kind: &str) -> Result<(Vec<u8>, Metadata)> {
    let (file, length) = file_pin::open_regular(path, maximum)?;
    let stamp = file.metadata()?;
    let mut bytes = Vec::with_capacity(length as usize);
    (&file)
        .take(length + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read bounded LoRa {kind} receipt"))?;
    ensure!(
        length == bytes.len() as u64 && file_pin::same_file(&stamp, &file.metadata()?),
        "LoRa {kind} receipt changed while reading"
    );
    verify_stamp(path, &stamp, kind)?;
    Ok((bytes, stamp))
}

fn sha256(bytes: &[u8]) -> String {
    HEXLOWER.encode(digest::digest(&SHA256, bytes).as_ref())
}

fn verify_stamp(path: &Path, stamp: &Metadata, kind: &str) -> Result<()> {
    let observed = path.symlink_metadata()?;
    ensure!(
        observed.is_file() && file_pin::same_file(stamp, &observed),
        "LoRa {kind} receipt changed after admission"
    );
    Ok(())
}

impl Receipt {
    fn validate(
        &self,
        request: &DecodeRequest,
        translated: &Translated,
    ) -> Result<Interpretations> {
        let settings = &request.channel;
        let max_frames = request.max_frames as usize;
        let expected_sync = [
            u16::from(request.sync_word >> 4) * 8,
            u16::from(request.sync_word & 15) * 8,
        ];
        ensure!(
            self.schema_version == 1
                && self.operation == "decode_lora_cf32"
                && self.source_format == "CF32_LE",
            "unsupported LoRa worker receipt"
        );
        ensure!(
            self.source_sample_count == translated.samples
                && self.source_sha256 == translated.channelized_sha256
                && self.channel_hz == settings.channel_hz
                && self.bandwidth_hz == settings.bandwidth_hz
                && self.sample_rate_hz == translated.sample_rate_hz
                && self.spreading_factor == settings.spreading_factor
                && self.sync_word == request.sync_word
                && self.expected_sync_symbols == expected_sync
                && self.max_frames == max_frames,
            "LoRa worker receipt differs from the translated source or requested channel"
        );
        let candidates = [
            self.validated_frames,
            self.rejected_crc_absent,
            self.rejected_invalid_crc,
            self.rejected_wrong_sync,
        ]
        .into_iter()
        .try_fold(0_usize, usize::checked_add);
        ensure!(
            self.records.len() == self.validated_frames.min(max_frames)
                && self.omitted_valid_frames
                    == self.validated_frames.saturating_sub(self.records.len())
                && candidates.is_some_and(|total| total as u64 <= translated.samples),
            "LoRa worker frame counts are inconsistent"
        );
        ensure!(
            self.records
                .is_sorted_by_key(|record| record.synchronization_sample),
            "LoRa worker synchronization samples are out of order"
        );
        let mut meshtastic = Vec::new();
        let mut lorawan_join = Vec::new();
        for (index, record) in self.records.iter().enumerate() {
            ensure!(
                record.synchronization_sample < translated.samples
                    && (1..=4).contains(&record.code_rate_index)
                    && record.observed_sync_symbols == expected_sync
                    && (4..=2 * MAX_PAYLOAD_BYTES).contains(&record.payload_hex.len()),
                "invalid LoRa frame metadata"
            );
            let payload = HEXLOWER
                .decode(record.payload_hex.as_bytes())
                .context("decode LoRa payload octets")?;
            ensure!(
                payload_crc(&payload) == Some(record.payload_crc),
                "LoRa worker payload CRC mismatch"
            );
            match request.interpretation {
                PacketInterpretation::None => {}
                PacketInterpretation::Meshtastic => {
                    if let Some(header) = Header::parse(&payload) {
                        meshtastic.push(Observation::new(index, record, header));
                    }
                }
                PacketInterpretation::LorawanJoin => {
                    if let Some(join) = JoinRequest::parse(&payload) {
                        lorawan_join.push(Observation::new(index, record, join));
                    }
                }
            }
        }
        let count = self.records.len();
        Ok(Interpretations {
            meshtastic: (request.interpretation == PacketInterpretation::Meshtastic)
                .then(|| Records::new(count, meshtastic)),
            lorawan_join: (request.interpretation == PacketInterpretation::LorawanJoin)
                .then(|| Records::new(count, lorawan_join)),
        })
    }
}

fn payload_crc(payload: &[u8]) -> Option<u16> {
    let (prefix, &suffix) = payload.split_last_chunk::<2>()?;
    let mut crc = 0_u16;
    for byte in prefix {
        for bit in (0..8).rev() {
            let feedback = (crc >> 15) ^ u16::from((byte >> bit) & 1);
            crc <<= 1;
            if feedback != 0 {
                crc ^= 0x1021;
            }
        }
    }
    // LoRa folds the final two payload bytes into the accumulated CRC.
    Some(crc ^ u16::from_be_bytes(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_independent_lora_fixture() {
        assert_eq!(payload_crc(b"LoRa passive IQ test"), Some(0x1e09));
        assert_ne!(payload_crc(b"LoRa passive iQ test"), Some(0x1e09));
        assert_eq!(payload_crc(b"x"), None);
    }
}
