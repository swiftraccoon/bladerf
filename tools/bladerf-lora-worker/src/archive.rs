//! Verify payload integrity and retain bounded FutureSDR decoder output.

use std::collections::HashMap;

use anyhow::{Result as ArchiveResult, ensure};
use data_encoding::HEXLOWER;
use futuresdr::runtime::dev::prelude::*;
use serde::Serialize;

#[derive(Clone, Serialize)]
struct Record {
    synchronization_sample: u64,
    code_rate_index: u8,
    observed_sync_symbols: [u16; 2],
    payload_hex: String,
    payload_crc: u16,
}

#[derive(Clone, Serialize)]
pub(super) struct Summary {
    pub(super) validated_frames: usize,
    records: Vec<Record>,
    omitted_valid_frames: usize,
    rejected_crc_absent: usize,
    rejected_invalid_crc: usize,
    rejected_wrong_sync: usize,
}

#[derive(Block)]
#[message_inputs(r#in)]
#[null_kernel]
pub(super) struct Archive {
    expected_sync_symbols: [u16; 2],
    source_sample_count: u64,
    max_frames: usize,
    protocol_error: Option<&'static str>,
    summary: Summary,
}

impl Archive {
    pub(super) fn new(
        expected_sync_symbols: [u16; 2],
        source_sample_count: u64,
        max_frames: usize,
    ) -> Self {
        Self {
            expected_sync_symbols,
            source_sample_count,
            max_frames,
            protocol_error: None,
            summary: Summary {
                validated_frames: 0,
                records: Vec::new(),
                omitted_valid_frames: 0,
                rejected_crc_absent: 0,
                rejected_invalid_crc: 0,
                rejected_wrong_sync: 0,
            },
        }
    }

    pub(super) fn summary(&self) -> ArchiveResult<Summary> {
        ensure!(
            self.protocol_error.is_none(),
            "invalid annotated LoRa decoder output: {}",
            self.protocol_error.unwrap_or_default()
        );
        Ok(self.summary.clone())
    }

    async fn r#in(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &BlockMeta,
        message: Pmt,
    ) -> Result<Pmt> {
        let result = match message {
            Pmt::Finished => {
                io.finished = true;
                return Ok(Pmt::Ok);
            }
            Pmt::MapStrPmt(fields) => self.record(fields),
            _ => Err("expected annotated frame"),
        };
        if let Err(error) = result {
            self.protocol_error.get_or_insert(error);
            return Ok(Pmt::InvalidValue);
        }
        Ok(Pmt::Ok)
    }

    fn record(
        &mut self,
        mut fields: HashMap<String, Pmt>,
    ) -> std::result::Result<(), &'static str> {
        if fields.remove("implicit_header") != Some(Pmt::Bool(false)) {
            return Err("expected explicit header");
        }
        let Some(Pmt::Bool(has_crc)) = fields.remove("has_crc") else {
            return Err("missing payload CRC flag");
        };
        if !has_crc {
            self.summary.rejected_crc_absent += 1;
            return Ok(());
        }
        let Some(Pmt::Blob(bytes)) = fields.remove("payload") else {
            return Err("missing payload bytes");
        };
        let Some((payload, payload_crc)) = validate_crc(&bytes) else {
            self.summary.rejected_invalid_crc += 1;
            return Ok(());
        };
        let Some(Pmt::U64(synchronization_sample)) = fields.remove("timestamp_samples") else {
            return Err("missing synchronization sample");
        };
        if synchronization_sample >= self.source_sample_count {
            return Err("synchronization sample outside the source extent");
        }
        let Some(Pmt::Usize(code_rate_index @ 1..=4)) = fields.remove("code_rate") else {
            return Err("invalid code rate");
        };
        let observed_sync_symbols = [
            sync_symbol(fields.remove("observed_sync_symbol_first"))?,
            sync_symbol(fields.remove("observed_sync_symbol_second"))?,
        ];
        if observed_sync_symbols != self.expected_sync_symbols {
            self.summary.rejected_wrong_sync += 1;
            return Ok(());
        }

        self.summary.validated_frames += 1;
        if self.summary.records.len() == self.max_frames {
            self.summary.omitted_valid_frames += 1;
            return Ok(());
        }
        self.summary.records.push(Record {
            synchronization_sample,
            code_rate_index: code_rate_index as u8,
            observed_sync_symbols,
            payload_hex: HEXLOWER.encode(payload),
            payload_crc,
        });
        Ok(())
    }
}

fn sync_symbol(field: Option<Pmt>) -> std::result::Result<u16, &'static str> {
    match field {
        Some(Pmt::Usize(value)) => u16::try_from(value).map_err(|_| "invalid observed sync symbol"),
        _ => Err("missing observed sync symbol"),
    }
}

fn validate_crc(frame: &[u8]) -> Option<(&[u8], u16)> {
    if frame.len() > 257 {
        return None;
    }
    let (payload, &crc) = frame.split_last_chunk::<2>()?;
    let (prefix, &last_pair) = payload.split_last_chunk::<2>()?;
    // The LoRa calculation XORs the last payload pair into the CRC-16 state.
    let mut state = 0u16;
    for &byte in prefix {
        state ^= u16::from(byte) << 8;
        for _ in 0..8 {
            state = (state << 1) ^ if state & 0x8000 != 0 { 0x1021 } else { 0 };
        }
    }
    let actual = u16::from_le_bytes(crc);
    ((state ^ u16::from_be_bytes(last_pair)) == actual).then_some((payload, actual))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_known_lora_payload_crc_and_rejects_corruption() {
        let payload = b"LoRa passive IQ test";
        let crc = 7689u16;
        let mut frame = payload.to_vec();
        frame.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(validate_crc(&frame), Some((payload.as_slice(), crc)));
        for index in 0..frame.len() {
            frame[index] ^= 1;
            assert!(validate_crc(&frame).is_none());
            frame[index] ^= 1;
        }
        assert!(validate_crc(&[0; 3]).is_none());
        assert!(validate_crc(&[0; 258]).is_none());
    }
}
