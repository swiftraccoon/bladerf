//! VHT signaling records from the pinned native Wi-Fi replay receiver.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

pub(super) const VHT_SIGNAL_A_PREFIX_SAMPLES: u64 = 560;
const DELIMITER_BYTES: usize = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SignalA {
    bandwidth_code: u8,
    group_id: u8,
    stbc: bool,
    short_guard_interval: bool,
    short_gi_disambiguation: bool,
    ldpc_extra_symbol: bool,
    txop_ps_not_allowed: bool,
    users: Users,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
enum Users {
    Su {
        space_time_streams: u8,
        partial_aid: u16,
        mcs: u8,
        ldpc: bool,
        beamformed: bool,
    },
    Mu {
        space_time_streams: [u8; 4],
        ldpc: [Option<bool>; 4],
    },
}

impl SignalA {
    pub(super) fn valid_fields(&self) -> bool {
        self.bandwidth_code <= 3
            && match &self.users {
                Users::Su {
                    space_time_streams,
                    partial_aid,
                    mcs,
                    ..
                } => valid_su(self.group_id, *space_time_streams, *partial_aid, *mcs),
                Users::Mu {
                    space_time_streams,
                    ldpc,
                } => {
                    (1..=62).contains(&self.group_id)
                        && !self.stbc
                        && space_time_streams
                            .iter()
                            .zip(ldpc)
                            .all(|(&streams, coding)| {
                                streams <= 4 && (streams == 0) == coding.is_none()
                            })
                }
            }
    }

    pub(super) fn su20_fields(&self) -> Option<SuFields> {
        let Users::Su {
            space_time_streams,
            partial_aid,
            mcs,
            ldpc,
            beamformed,
        } = self.users
        else {
            return None;
        };
        if self.bandwidth_code != 0 || !self.valid_fields() {
            return None;
        }
        Some(SuFields {
            mcs,
            space_time_streams,
            stbc: self.stbc,
            coding: if ldpc { Coding::Ldpc } else { Coding::Bcc },
            guard_interval_ns: if self.short_guard_interval { 400 } else { 800 },
            short_gi_disambiguation: self.short_gi_disambiguation,
            txop_ps_not_allowed: self.txop_ps_not_allowed,
            ldpc_extra_symbol: self.ldpc_extra_symbol,
            group_id: self.group_id,
            partial_aid,
            beamformed,
        })
    }
}

fn valid_su(group_id: u8, space_time_streams: u8, partial_aid: u16, mcs: u8) -> bool {
    matches!(group_id, 0 | 63)
        && (1..=8).contains(&space_time_streams)
        && partial_aid <= 511
        && mcs <= 15
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SuFields {
    mcs: u8,
    space_time_streams: u8,
    stbc: bool,
    coding: Coding,
    guard_interval_ns: u16,
    short_gi_disambiguation: bool,
    txop_ps_not_allowed: bool,
    ldpc_extra_symbol: bool,
    group_id: u8,
    partial_aid: u16,
    beamformed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Coding {
    Bcc,
    Ldpc,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    mcs: u8,
    bandwidth_code: u8,
    space_time_streams: u8,
    stbc: bool,
    coding: Coding,
    guard_interval_ns: u16,
    short_gi_disambiguation: bool,
    txop_ps_not_allowed: bool,
    ldpc_extra_symbol: bool,
    group_id: u8,
    partial_aid: u16,
    beamformed: bool,
    apep_length_bounds: Option<[u32; 2]>,
    service_crc_verified: bool,
    preamble_sample_index: u64,
}

impl SuFields {
    pub(super) fn read(
        frame: &Value,
        start: u64,
        end: u64,
        delimiter_offset: usize,
        mac_len: usize,
    ) -> Result<Option<Self>> {
        let fields = Frame::deserialize(frame).context("invalid VHT frame metadata")?;
        ensure!(
            fields.preamble_sample_index == start
                && start < end
                && fields.bandwidth_code <= 3
                && matches!(fields.guard_interval_ns, 400 | 800)
                && valid_su(
                    fields.group_id,
                    fields.space_time_streams,
                    fields.partial_aid,
                    fields.mcs,
                ),
            "invalid VHT frame signaling identity or sample extent"
        );
        if fields.bandwidth_code != 0 {
            return Ok(None);
        }
        ensure!(
            fields.mcs <= 8
                && fields.space_time_streams == if fields.stbc { 2 } else { 1 }
                && (fields.coding == Coding::Ldpc || !fields.ldpc_extra_symbol),
            "VHT20 SU frame claims a mode unsupported by the pinned receiver"
        );
        let member_end = delimiter_offset
            .checked_add(DELIMITER_BYTES)
            .and_then(|payload_start| payload_start.checked_add(mac_len))
            .context("VHT aggregate member extent overflow")?;
        ensure!(
            fields.service_crc_verified
                && fields.apep_length_bounds.is_some_and(|[lower, upper]| {
                    (4..=0x1ffff * 4).contains(&upper)
                        && upper.is_multiple_of(4)
                        && lower == upper - 3
                        && member_end <= upper as usize
                }),
            "VHT20 SU frame lacks consistent SIG-B SERVICE or aggregate extent evidence"
        );
        // A single DATA stream uses one VHT-LTF, or two with STBC. At 20 MSps
        // DATA follows the resulting 40 or 44 microsecond preamble.
        let preamble_samples = if fields.stbc { 880 } else { 800 };
        let symbol_samples = if fields.guard_interval_ns == 400 {
            72
        } else {
            80
        };
        let data_samples = (end - start)
            .checked_sub(preamble_samples)
            .context("VHT20 SU frame ends before DATA")?;
        let data_symbols = data_samples / symbol_samples;
        ensure!(
            data_samples.is_multiple_of(symbol_samples)
                && data_symbols > 0
                && (!fields.stbc || data_symbols.is_multiple_of(2))
                && fields.short_gi_disambiguation
                    == (fields.guard_interval_ns == 400 && data_symbols % 10 == 9),
            "VHT20 SU frame has an inconsistent DATA symbol extent"
        );
        Ok(Some(Self {
            mcs: fields.mcs,
            space_time_streams: fields.space_time_streams,
            stbc: fields.stbc,
            coding: fields.coding,
            guard_interval_ns: fields.guard_interval_ns,
            short_gi_disambiguation: fields.short_gi_disambiguation,
            txop_ps_not_allowed: fields.txop_ps_not_allowed,
            ldpc_extra_symbol: fields.ldpc_extra_symbol,
            group_id: fields.group_id,
            partial_aid: fields.partial_aid,
            beamformed: fields.beamformed,
        }))
    }
}
