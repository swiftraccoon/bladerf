//! Validate stdout from libcrafter 00f1fed7's serial modern file receiver.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

use super::modern_vht;
use crate::ModernPhy;

const MAX_LINE_BYTES: usize = 128 * 1024;
const MAX_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORDS: u64 = 131_072;
const MAX_FRAMES: u64 = 65_536;
const CHUNK_SAMPLES: u64 = 65_536;
const FRAME_BYTES: usize = 16_383;
const HE_SU_PREFIX_SAMPLES: u64 = 640;
const SCHEMA: &str = "crafter.radio.receive/v2";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Stats {
    pub(super) selected_frames: u64,
    pub(super) total_frames: u64,
    pub(super) selected_headers: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    sample_rate_hz: u64,
    center_frequency_hz: u64,
    max_chunk_samples: u64,
    max_buffer_samples: u64,
    max_frame_bytes: usize,
    max_pending_frames: u64,
    max_capture_samples: u64,
    max_duration_ns: u64,
}

impl Config {
    fn verify(self, frequency_hz: u64, samples: u64) -> Result<()> {
        let limit = samples
            .checked_add(1)
            .context("modern Wi-Fi replay sample limit overflow")?;
        ensure!(
            self.sample_rate_hz == u64::from(super::SAMPLE_RATE_HZ)
                && self.center_frequency_hz == frequency_hz
                && self.max_chunk_samples == CHUNK_SAMPLES
                && self.max_buffer_samples == 16_777_216
                && self.max_frame_bytes == FRAME_BYTES
                && self.max_pending_frames == 1_024
                && self.max_capture_samples == limit
                && u128::from(self.max_duration_ns) * u128::from(self.sample_rate_hz)
                    >= u128::from(limit) * 1_000_000_000,
            "modern Wi-Fi replay header differs from the requested modern source geometry"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    epoch: u64,
    sequence: u64,
    sample_index: u64,
    #[serde(rename = "anchor")]
    _anchor: (),
    #[serde(rename = "gap_reason")]
    _gap_reason: (),
    #[serde(rename = "lost_samples")]
    _lost_samples: (),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Aggregate {
    delimiter_offset: usize,
    control_bits: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecoderCounts {
    valid_frames: u64,
    dropped_frames: u64,
    #[serde(rename = "invalid_fcs")]
    _invalid_fcs: u64,
    #[serde(rename = "rejected_frames")]
    _rejected_frames: u64,
    #[serde(rename = "truncated_frames")]
    _truncated_frames: u64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Header {
        schema: String,
        decoder: String,
        dispatch: String,
        time_basis: String,
        config: Config,
    },
    Chunk {
        config: Config,
        position: Position,
        samples: u64,
        verified_prefix: bool,
        #[serde(rename = "software_time_bracket_ns")]
        _software_time_bracket_ns: (),
    },
    Frame {
        ordinal: u64,
        original_mac_hex: String,
        fcs: String,
        phy: String,
        config: Config,
        position: Position,
        end_sample_index: u64,
        rate_bps: u32,
        ampdu: Option<Aggregate>,
        he: Option<Value>,
        #[serde(rename = "ht")]
        _ht: Option<Value>,
        vht: Option<Value>,
        #[serde(rename = "preamble")]
        _preamble: Option<String>,
        #[serde(rename = "diagnostics")]
        _diagnostics: Vec<String>,
    },
    HeSignal {
        epoch: u64,
        phy: String,
        preamble_sample_index: u64,
        he: Value,
        mac_integrity: Option<String>,
    },
    HeSigB {
        epoch: u64,
        phy: String,
        preamble_sample_index: u64,
        he: Value,
        mac_integrity: String,
    },
    HtSignal {
        epoch: u64,
        preamble_sample_index: u64,
        ht: Value,
        mac_integrity: String,
    },
    VhtSignalA {
        epoch: u64,
        preamble_sample_index: u64,
        signal_a: modern_vht::SignalA,
        mac_integrity: String,
    },
    Terminal {
        reason: String,
    },
    ParserError {
        error: String,
    },
    Summary {
        complete: bool,
        terminal: String,
        emitted_frames: u64,
        parsed_packets: u64,
        parser_failures: u64,
        decoder: DecoderCounts,
        #[serde(rename = "error")]
        _error: (),
        #[serde(rename = "recording_error")]
        _recording_error: (),
    },
}

#[derive(Deserialize)]
struct HePosition {
    format: HeFormat,
    bandwidth_code: u8,
    preamble_sample_index: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HeFormat {
    Su,
    ErSu,
    Mu,
    Tb,
}

impl HePosition {
    fn read(value: &Value, preamble: u64) -> Result<Self> {
        let position = Self::deserialize(value).context("invalid HE signaling identity")?;
        ensure!(
            position.preamble_sample_index == preamble && position.bandwidth_code <= 3,
            "HE signaling position or bandwidth differs from its record"
        );
        Ok(position)
    }

    fn is_he20_su(&self) -> bool {
        self.format == HeFormat::Su && self.bandwidth_code == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
struct SuFields {
    mcs: u8,
    space_time_streams: u8,
    guard_interval_ns: u16,
    ltf_size: u8,
    coding: Coding,
    bss_color: u8,
    stbc: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Signal {
    He(SuFields),
    Vht(modern_vht::SuFields),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Coding {
    Bcc,
    Ldpc,
}

impl SuFields {
    fn read(value: &Value, start: u64, end: u64) -> Result<Self> {
        let fields = Self::deserialize(value).context("invalid HE20 SU signaling fields")?;
        ensure!(
            fields.mcs <= 11
                && (1..=8).contains(&fields.space_time_streams)
                && matches!(
                    (fields.ltf_size, fields.guard_interval_ns),
                    (1 | 2 | 4, 800) | (2, 1600) | (4, 3200)
                )
                && fields.bss_color <= 63
                && start
                    .checked_add(HE_SU_PREFIX_SAMPLES)
                    .is_some_and(|prefix_end| prefix_end <= end),
            "HE20 SU fields or complete preamble extent are invalid"
        );
        Ok(fields)
    }
}

fn read_record(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    bytes_left: &mut u64,
) -> Result<Option<Record>> {
    line.clear();
    let limit = (*bytes_left + 1).min((MAX_LINE_BYTES + 1) as u64);
    let bytes = reader.take(limit).read_until(b'\n', line)?;
    if bytes == 0 {
        return Ok(None);
    }
    ensure!(
        bytes <= MAX_LINE_BYTES && bytes as u64 <= *bytes_left,
        "modern Wi-Fi replay records exceed their byte bound"
    );
    ensure!(
        line.last() == Some(&b'\n'),
        "incomplete modern Wi-Fi replay record"
    );
    *bytes_left -= bytes as u64;
    serde_json::from_slice(line)
        .context("invalid modern Wi-Fi replay JSONL record")
        .map(Some)
}

fn mac_bytes(hex: &str) -> Result<Vec<u8>> {
    ensure!(
        hex.len().is_multiple_of(2) && (8..=FRAME_BYTES * 2).contains(&hex.len()),
        "modern Wi-Fi replay MAC extent is outside the frame bound"
    );
    let digit = |byte| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let (pairs, _) = hex.as_bytes().as_chunks::<2>();
    pairs
        .iter()
        .map(|pair| {
            digit(pair[0])
                .zip(digit(pair[1]))
                .map(|(high, low)| high << 4 | low)
                .context("invalid modern Wi-Fi replay MAC hex")
        })
        .collect()
}

fn check_signal_position(
    epoch: u64,
    preamble: u64,
    metadata: &Value,
    processed: u64,
) -> Result<()> {
    ensure!(
        epoch == 0
            && preamble < processed
            && metadata["preamble_sample_index"].as_u64() == Some(preamble),
        "modern Wi-Fi replay signaling record lies outside its verified source position"
    );
    Ok(())
}

/// Only call `emit` for the selected PHY with a matching checked preamble.
/// The caller owns FCS validation and must stage any output until `parse` succeeds.
pub(super) fn parse(
    mut reader: impl BufRead,
    frequency_hz: u64,
    samples: u64,
    selected_phy: ModernPhy,
    mut emit: impl FnMut(&[u8], u64, u64) -> Result<()>,
) -> Result<Stats> {
    ensure!(
        (1..=super::MAX_SAMPLES).contains(&samples),
        "invalid modern Wi-Fi replay sample extent"
    );
    let mut line = Vec::new();
    let mut bytes_left = MAX_OUTPUT_BYTES;
    let Some(Record::Header {
        schema,
        decoder,
        dispatch,
        time_basis,
        config,
    }) = read_record(&mut reader, &mut line, &mut bytes_left)?
    else {
        bail!("modern Wi-Fi replay lacks the modern receiver header");
    };
    ensure!(
        schema == SCHEMA
            && decoder == "wifi"
            && dispatch == "serial"
            && time_basis == "recorded_anchor_or_unknown",
        "unsupported modern Wi-Fi replay header"
    );
    config.verify(frequency_hz, samples)?;

    let mut stats = Stats::default();
    let mut processed = 0;
    let mut chunks = 0;
    let mut parser_errors = 0;
    let mut terminal = false;
    let mut summary = false;
    let mut headers = BTreeMap::<u64, BTreeSet<Signal>>::new();
    let mut emitted_members = BTreeSet::new();
    let mut records = 1;
    while let Some(record) = read_record(&mut reader, &mut line, &mut bytes_left)? {
        ensure!(
            records < MAX_RECORDS,
            "modern Wi-Fi replay exceeds its record count bound"
        );
        records += 1;
        ensure!(
            !summary
                && (!terminal
                    || matches!(record, Record::ParserError { .. } | Record::Summary { .. })),
            "unexpected modern Wi-Fi replay record after the terminal"
        );
        match record {
            Record::Header { .. } => bail!("duplicate modern Wi-Fi replay receiver header"),
            Record::Chunk {
                config: chunk_config,
                position,
                samples: count,
                verified_prefix,
                ..
            } => {
                ensure!(
                    chunk_config == config
                        && position.epoch == 0
                        && position.sequence == chunks
                        && position.sample_index == processed
                        && count > 0
                        && count == (samples - processed).min(CHUNK_SAMPLES)
                        && verified_prefix,
                    "modern Wi-Fi replay source chunks are not a contiguous verified prefix"
                );
                processed += count;
                chunks += 1;
            }
            Record::Frame {
                ordinal,
                original_mac_hex,
                fcs,
                phy,
                config: frame_config,
                position,
                end_sample_index: end,
                rate_bps,
                ampdu,
                he,
                vht,
                ..
            } => {
                let start = position.sample_index;
                ensure!(
                    stats.total_frames < MAX_FRAMES
                        && ordinal == stats.total_frames + 1
                        && frame_config == config
                        && position.epoch == 0
                        && start < end
                        && end <= processed
                        && rate_bps > 0
                        && fcs == "present_valid"
                        && matches!(
                            phy.as_str(),
                            "dsss" | "cck" | "legacy_ofdm" | "ht" | "vht" | "he" | "unknown"
                        )
                        && (phy == "he") == he.is_some(),
                    "invalid modern Wi-Fi replay frame identity or source extent"
                );
                let mac = mac_bytes(&original_mac_hex)?;
                let offset = ampdu
                    .map(|aggregate| {
                        ensure!(
                            aggregate.delimiter_offset.is_multiple_of(4)
                                && aggregate.delimiter_offset as u64 <= config.max_buffer_samples
                                && aggregate.control_bits <= 15,
                            "invalid modern Wi-Fi replay aggregate member identity"
                        );
                        Ok(aggregate.delimiter_offset)
                    })
                    .transpose()?;
                stats.total_frames += 1;
                let signal = match selected_phy {
                    ModernPhy::He20Su => match he {
                        Some(he) if HePosition::read(&he, start)?.is_he20_su() => {
                            Some(Signal::He(SuFields::read(&he, start, end)?))
                        }
                        _ => None,
                    },
                    ModernPhy::Vht20Su => {
                        ensure!(
                            (phy == "vht") == vht.is_some(),
                            "invalid VHT frame metadata"
                        );
                        if let Some(vht) = vht {
                            let delimiter_offset =
                                offset.context("VHT frame has no aggregate member identity")?;
                            modern_vht::SuFields::read(
                                &vht,
                                start,
                                end,
                                delimiter_offset,
                                mac.len(),
                            )?
                            .map(Signal::Vht)
                        } else {
                            None
                        }
                    }
                };
                if let Some(signal) = signal {
                    ensure!(
                        headers
                            .get(&start)
                            .is_some_and(|seen| seen.contains(&signal)),
                        "selected Wi-Fi frame has no matching checked header"
                    );
                    ensure!(
                        emitted_members.insert((start, end, offset)),
                        "duplicate selected Wi-Fi frame occurrence"
                    );
                    emit(&mac, start, end)?;
                    stats.selected_frames += 1;
                }
            }
            Record::HeSignal {
                epoch,
                phy,
                preamble_sample_index,
                he,
                mac_integrity,
            } => {
                check_signal_position(epoch, preamble_sample_index, &he, processed)?;
                let position = HePosition::read(&he, preamble_sample_index)?;
                let expected_integrity = matches!(position.format, HeFormat::Mu | HeFormat::Tb)
                    .then_some("not_established_by_header");
                ensure!(
                    phy == "he" && mac_integrity.as_deref() == expected_integrity,
                    "invalid modern Wi-Fi replay header classification"
                );
                if selected_phy == ModernPhy::He20Su && position.is_he20_su() {
                    let fields = SuFields::read(&he, preamble_sample_index, processed)?;
                    headers
                        .entry(preamble_sample_index)
                        .or_default()
                        .insert(Signal::He(fields));
                    stats.selected_headers += 1;
                }
            }
            Record::HtSignal {
                epoch,
                preamble_sample_index,
                ht: fields,
                mac_integrity,
            } => {
                check_signal_position(epoch, preamble_sample_index, &fields, processed)?;
                ensure!(
                    mac_integrity == "not_established_by_header",
                    "invalid signaling integrity claim"
                );
            }
            Record::VhtSignalA {
                epoch,
                preamble_sample_index,
                signal_a,
                mac_integrity,
            } => {
                ensure!(
                    epoch == 0
                        && preamble_sample_index
                            .checked_add(modern_vht::VHT_SIGNAL_A_PREFIX_SAMPLES)
                            .is_some_and(|prefix_end| prefix_end <= processed)
                        && mac_integrity == "not_established_by_header"
                        && signal_a.valid_fields(),
                    "invalid VHT-SIG-A fields, integrity claim, or source extent"
                );
                if selected_phy == ModernPhy::Vht20Su
                    && let Some(fields) = signal_a.su20_fields()
                {
                    headers
                        .entry(preamble_sample_index)
                        .or_default()
                        .insert(Signal::Vht(fields));
                    stats.selected_headers += 1;
                }
            }
            Record::HeSigB {
                epoch,
                phy,
                preamble_sample_index,
                he,
                mac_integrity,
            } => {
                check_signal_position(epoch, preamble_sample_index, &he, processed)?;
                ensure!(
                    phy == "he"
                        && HePosition::read(&he, preamble_sample_index)?.format == HeFormat::Mu
                        && mac_integrity == "not_established_by_header",
                    "invalid modern Wi-Fi replay MU signaling record"
                );
            }
            Record::Terminal { reason } => {
                ensure!(
                    reason == "Eof" && processed == samples,
                    "modern Wi-Fi replay did not reach EOF after consuming the exact staged sample extent"
                );
                terminal = true;
            }
            Record::ParserError { error } => {
                ensure!(
                    !error.is_empty() && parser_errors < stats.total_frames,
                    "modern Wi-Fi replay parser errors exceed the emitted frame inventory"
                );
                parser_errors += 1;
            }
            Record::Summary {
                complete,
                terminal: reason,
                emitted_frames,
                parsed_packets,
                parser_failures,
                decoder,
                ..
            } => {
                ensure!(
                    terminal
                        && complete
                        && reason == "Eof"
                        && parser_failures == parser_errors
                        && emitted_frames == stats.total_frames
                        && parsed_packets.checked_add(parser_failures) == Some(emitted_frames)
                        && decoder.valid_frames == stats.total_frames
                        && decoder.dropped_frames == 0,
                    "modern Wi-Fi replay summary does not close the validated frame accounting"
                );
                summary = true;
            }
        }
    }
    ensure!(
        summary,
        "modern Wi-Fi replay ended before its complete terminal summary"
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FREQUENCY: u64 = 2_437_000_000;
    const SAMPLES: u64 = 2_741;

    fn parse_he(
        reader: impl BufRead,
        frequency_hz: u64,
        samples: u64,
        emit: impl FnMut(&[u8], u64, u64) -> Result<()>,
    ) -> Result<Stats> {
        parse(reader, frequency_hz, samples, ModernPhy::He20Su, emit)
    }

    // Record geometry and counters from the pinned HE20 SU MCS0 A-MPDU
    // example. The emitted bytes here exercise the callback, not its FCS check.
    fn records() -> Vec<Value> {
        let config = json!({
            "sample_rate_hz":20_000_000,"center_frequency_hz":FREQUENCY,
            "max_chunk_samples":65_536,"max_buffer_samples":16_777_216,
            "max_frame_bytes":16_383,"max_pending_frames":1_024,
            "max_capture_samples":SAMPLES+1,"max_duration_ns":1_000_000_000
        });
        let position = |sample_index, sequence| {
            json!({"epoch":0,"sequence":sequence,"sample_index":sample_index,
                   "anchor":null,"gap_reason":null,"lost_samples":null})
        };
        let he = json!({
            "format":"su","bandwidth_code":0,"coding":"bcc","mcs":0,
            "space_time_streams":1,"guard_interval_ns":800,"ltf_size":1,
            "bss_color":0,"stbc":false,"preamble_sample_index":37
        });
        let frame = |ordinal, offset| {
            json!({
                "kind":"frame","ordinal":ordinal,"original_mac_hex":"010203040506",
                "fcs":"present_valid","phy":"he","position":position(37,2),
                "end_sample_index":SAMPLES,"config":config,"he":he,"rate_bps":8_602_941,
                "ampdu":{"delimiter_offset":offset,"control_bits":0},
                "ht":null,"preamble":null,"diagnostics":[]
            })
        };
        vec![
            json!({"kind":"header","schema":SCHEMA,"decoder":"wifi","dispatch":"serial",
                   "time_basis":"recorded_anchor_or_unknown","config":config}),
            json!({"kind":"chunk","config":config,"position":position(0,0),"samples":SAMPLES,
                   "verified_prefix":true,"software_time_bracket_ns":null}),
            json!({"kind":"he_signal","epoch":0,"phy":"he","preamble_sample_index":37,"he":he}),
            frame(1, 0),
            frame(2, 44),
            json!({"kind":"terminal","reason":"Eof"}),
            json!({"kind":"summary","complete":true,"terminal":"Eof","error":null,
                   "recording_error":null,"emitted_frames":2,"parsed_packets":2,
                   "parser_failures":0,"decoder":{"valid_frames":2,"invalid_fcs":0,
                       "rejected_frames":1,"truncated_frames":0,"dropped_frames":0}}),
        ]
    }

    fn jsonl(records: &[Value]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record).unwrap();
            bytes.push(b'\n');
        }
        bytes
    }

    fn vht_signal(preamble_sample_index: u64) -> Value {
        json!({
            "kind":"vht_signal_a","epoch":0,"preamble_sample_index":preamble_sample_index,
            "mac_integrity":"not_established_by_header",
            "signal_a":{
                "bandwidth_code":0,"group_id":63,"stbc":false,
                "short_guard_interval":false,"short_gi_disambiguation":false,
                "ldpc_extra_symbol":false,"txop_ps_not_allowed":true,
                "users":{"mode":"su","space_time_streams":1,"partial_aid":0,
                         "mcs":0,"ldpc":false,"beamformed":false}
            }
        })
    }

    #[test]
    fn reader_emits_only_matching_he20_su_aggregate_members() {
        let input = jsonl(&records());
        let mut spans = Vec::new();
        let stats = parse_he(input.as_slice(), FREQUENCY, SAMPLES, |mac, start, end| {
            assert_eq!(mac, [1, 2, 3, 4, 5, 6]);
            spans.push((start, end));
            Ok(())
        })
        .unwrap();
        assert_eq!(spans, [(37, SAMPLES); 2]);
        assert_eq!(
            stats,
            Stats {
                selected_frames: 2,
                total_frames: 2,
                selected_headers: 1
            }
        );
    }

    #[test]
    fn reader_accounts_for_non_he_and_other_he_modes_without_emitting_them() {
        let mut records = records();
        records[3]["phy"] = json!("legacy_ofdm");
        records[3].as_object_mut().unwrap().remove("he");
        records[4]["he"]["format"] = json!("er_su");
        let stats = parse_he(jsonl(&records).as_slice(), FREQUENCY, SAMPLES, |_, _, _| {
            panic!("out-of-scope frame is not emitted")
        })
        .unwrap();
        assert_eq!(
            stats,
            Stats {
                selected_frames: 0,
                total_frames: 2,
                selected_headers: 1
            }
        );
    }

    #[test]
    fn reader_selects_requested_phy_from_mixed_records() {
        // Pinned VHT BCC MCS0 case0, 1024 zero samples, then the HE fixture.
        let samples = 6_986;
        let he_start = 4_282;
        let mut records = records();
        for index in [0, 1, 3, 4] {
            records[index]["config"]["max_capture_samples"] = json!(samples + 1);
        }
        records[1]["samples"] = json!(samples);
        records[2]["preamble_sample_index"] = json!(he_start);
        records[2]["he"]["preamble_sample_index"] = json!(he_start);
        let mut vht_frame = records[3].clone();
        vht_frame["phy"] = json!("vht");
        vht_frame["original_mac_hex"] = json!("0a0b0c0d0e0f");
        vht_frame["end_sample_index"] = json!(3_157);
        vht_frame["rate_bps"] = json!(6_500_000);
        vht_frame["ampdu"]["control_bits"] = json!(1);
        vht_frame.as_object_mut().unwrap().remove("he");
        vht_frame["vht"] = json!({
            "apep_length_bounds":[89,92],"bandwidth_code":0,"beamformed":false,
            "coding":"bcc","group_id":63,"guard_interval_ns":800,"ldpc_extra_symbol":false,
            "mcs":0,"partial_aid":0,"preamble_sample_index":37,"service_crc_verified":true,
            "short_gi_disambiguation":false,"space_time_streams":1,"stbc":false,
            "txop_ps_not_allowed":true
        });
        for (index, frame) in records[3..5].iter_mut().enumerate() {
            frame["ordinal"] = json!(index + 2);
            frame["position"]["sample_index"] = json!(he_start);
            frame["end_sample_index"] = json!(samples);
            frame["he"]["preamble_sample_index"] = json!(he_start);
        }
        records[6]["emitted_frames"] = json!(3);
        records[6]["parsed_packets"] = json!(3);
        records[6]["decoder"]["valid_frames"] = json!(3);
        records.splice(2..2, [vht_signal(37), vht_frame]);

        let mut spans = Vec::new();
        let stats = parse_he(
            jsonl(&records).as_slice(),
            FREQUENCY,
            samples,
            |mac, start, end| {
                assert_eq!(mac, [1, 2, 3, 4, 5, 6]);
                spans.push((start, end));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(spans, [(he_start, samples); 2]);
        assert_eq!(
            stats,
            Stats {
                selected_frames: 2,
                total_frames: 3,
                selected_headers: 1
            }
        );
        let mut spans = Vec::new();
        let stats = parse(
            jsonl(&records).as_slice(),
            FREQUENCY,
            samples,
            ModernPhy::Vht20Su,
            |mac, start, end| {
                assert_eq!(mac, [10, 11, 12, 13, 14, 15]);
                spans.push((start, end));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(spans, [(37, 3_157)]);
        assert_eq!(
            stats,
            Stats {
                selected_frames: 1,
                total_frames: 3,
                selected_headers: 1,
            }
        );
        for (index, path, value) in [
            (2, "/signal_a/users/mcs", json!(1)),
            (3, "/vht/service_crc_verified", json!(false)),
            (3, "/end_sample_index", json!(3_156)),
            (3, "/ampdu", Value::Null),
            (3, "/ampdu/delimiter_offset", json!(92)),
        ] {
            let mut malformed = records.clone();
            *malformed[index].pointer_mut(path).unwrap() = value;
            assert!(
                parse(
                    jsonl(&malformed).as_slice(),
                    FREQUENCY,
                    samples,
                    ModernPhy::Vht20Su,
                    |_, _, _| panic!("invalid VHT frame reached publication"),
                )
                .is_err(),
                "accepted invalid VHT field {path}",
            );
        }
    }

    #[test]
    fn reader_rejects_malformed_or_truncated_vht_signal_a() {
        let check = |vht| {
            let mut records = records();
            records.insert(2, vht);
            parse_he(jsonl(&records).as_slice(), FREQUENCY, SAMPLES, |_, _, _| {
                Ok(())
            })
        };
        let last_complete_start = SAMPLES - modern_vht::VHT_SIGNAL_A_PREFIX_SAMPLES;
        let valid = vht_signal(last_complete_start);
        assert!(check(valid.clone()).is_ok());
        for (path, value) in [
            ("/preamble_sample_index", json!(last_complete_start + 1)),
            ("/preamble_sample_index", json!(u64::MAX)),
            ("/epoch", json!(1)),
            ("/mac_integrity", json!("present_valid")),
            ("/signal_a/bandwidth_code", json!(4)),
            ("/signal_a/short_guard_interval", json!(0)),
            ("/signal_a/group_id", json!(1)),
            ("/signal_a/users/space_time_streams", json!(0)),
            ("/signal_a/users/mcs", json!(16)),
        ] {
            let mut malformed = valid.clone();
            *malformed.pointer_mut(path).unwrap() = value;
            assert!(
                check(malformed).is_err(),
                "accepted invalid VHT field {path}"
            );
        }
        let mut legacy_shape = valid.clone();
        legacy_shape["vht"] = legacy_shape
            .as_object_mut()
            .unwrap()
            .remove("signal_a")
            .unwrap();
        assert!(check(legacy_shape).is_err());

        let mut mu = valid;
        mu["signal_a"]["group_id"] = json!(1);
        mu["signal_a"]["users"] = json!({
            "mode":"mu","space_time_streams":[1,0,2,0],"ldpc":[false,null,true,null]
        });
        assert!(check(mu.clone()).is_ok());
        mu["signal_a"]["users"]["ldpc"][1] = json!(false);
        assert!(check(mu).is_err());
    }

    #[test]
    fn reader_accounts_for_parser_failures_without_rejecting_other_frames() {
        let mut records = records();
        records[3]["phy"] = json!("legacy_ofdm");
        records[3].as_object_mut().unwrap().remove("he");
        records[6]["parser_failures"] = json!(1);
        records[6]["parsed_packets"] = json!(1);
        // Buffered frames can reach the MAC parser after its decoder emits EOF.
        records.insert(
            6,
            json!({"kind":"parser_error","error":"test MAC parser failure"}),
        );
        let stats = parse_he(jsonl(&records).as_slice(), FREQUENCY, SAMPLES, |_, _, _| {
            Ok(())
        })
        .unwrap();
        assert_eq!(stats.total_frames, 2);
        assert_eq!(stats.selected_frames, 1);
    }

    #[test]
    fn reader_rejects_malformed_geometry_frames_or_summary() {
        for (index, path, value) in [
            (0, "/decoder", json!("legacy_wifi")),
            (0, "/config/max_capture_samples", json!(SAMPLES)),
            (1, "/position/sample_index", json!(1)),
            (1, "/samples", json!(SAMPLES - 1)),
            (2, "/he/bss_color", json!(1)),
            (3, "/position/epoch", json!(1)),
            (3, "/end_sample_index", json!(SAMPLES + 1)),
            (3, "/original_mac_hex", json!("not hex")),
            (3, "/fcs", json!("present_invalid")),
            (4, "/ordinal", json!(1)),
            (4, "/ampdu/delimiter_offset", json!(0)),
            (5, "/reason", json!("LimitReached")),
            (6, "/decoder/dropped_frames", json!(1)),
            (6, "/decoder/valid_frames", json!(1)),
            (6, "/parser_failures", json!(1)),
        ] {
            let mut records = records();
            *records[index].pointer_mut(path).unwrap() = value;
            assert!(
                parse_he(
                    jsonl(&records).as_slice(),
                    FREQUENCY,
                    SAMPLES,
                    |_, _, _| Ok(())
                )
                .is_err(),
                "accepted malformed record {index} at {path}"
            );
        }
        let mut records = records();
        records.remove(2);
        assert!(
            parse_he(
                jsonl(&records).as_slice(),
                FREQUENCY,
                SAMPLES,
                |_, _, _| Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn reader_rejects_partial_extra_and_overlong_records() {
        let records = records();
        let mut no_newline = jsonl(&records);
        no_newline.pop();
        let mut extra_terminal = records.clone();
        extra_terminal.push(records[5].clone());
        for invalid in [
            no_newline,
            jsonl(&records[..6]),
            jsonl(&extra_terminal),
            vec![b' '; MAX_LINE_BYTES + 1],
        ] {
            assert!(parse_he(invalid.as_slice(), FREQUENCY, SAMPLES, |_, _, _| Ok(())).is_err());
        }
    }
}
