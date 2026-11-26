//! Decode H5054 pulse-width frames and group their repeated rows.
//!
//! [CRC frames] use a six-byte body. The [legacy frames] have the same
//! symbol geometry and carry a four-bit parity check with a fixed footer.
//!
//! [CRC frames]: https://github.com/merbanan/rtl_433/issues/2265
//! [legacy frames]: https://github.com/merbanan/rtl_433/issues/1518

use std::ops::{Range, RangeInclusive};

use serde::Serialize;

use super::Pulse;

const FRAME_BYTES: usize = 6;
const FRAME_PULSES: usize = FRAME_BYTES * 8;
const SHORT_US: RangeInclusive<u32> = 180..=470;
const LONG_US: RangeInclusive<u32> = 640..=1_050;
const CELL_US: RangeInclusive<u32> = 960..=1_280;
const ROW_GAP_US: u32 = 1_030;
const MAX_SPUR_US: u32 = 100;
// The gap can contain a short marker in addition to the two low periods.
const COPY_GAP_US: u64 = 13_000;
const LEGACY_MIN_COPIES: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Integrity {
    Crc16AugCcitt,
    LegacyNibbleParity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Event {
    ButtonPress,
    WaterLeak,
    BatteryReport,
    Heartbeat,
    Unknown,
}

/// A checked H5054 frame. A null ID represents the CRC-valid all-ones value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct H5054 {
    pub(super) id: Option<u16>,
    integrity: Integrity,
    event: Event,
    event_code: u8,
    event_data: u8,
}

/// Checked copies of one body, with their first and last envelope edges.
pub(super) struct Packet {
    body: [u8; FRAME_BYTES],
    pub(super) reading: H5054,
    pub(super) first_us: u64,
    pub(super) end_us: u64,
    pub(super) copies: usize,
}

/// One pending transmission. A pulse-detector callback may end between copies.
#[derive(Default)]
pub(super) struct Decoder {
    pending: Option<Packet>,
    previous_row_end_us: Option<u64>,
}

impl Decoder {
    /// Process a complete detector callback with absolute rising-edge times.
    pub(super) fn push(
        &mut self,
        pulses: &[Pulse],
        starts_us: &[u64],
        mut emit: impl FnMut(Packet),
    ) {
        let mut row_start = 0;
        for row in pulses.split_inclusive(|pulse| pulse.low_us >= ROW_GAP_US) {
            let start = row_start;
            row_start += row.len();
            let Some((body, extent)) = slice_row(row) else {
                continue;
            };
            let first_us = starts_us[start + extent.start];
            let last = extent.end - 1;
            let end_us = starts_us[start + last] + u64::from(row[last].high_us);
            let reading = parse_body(body);
            let separated = self
                .previous_row_end_us
                .is_some_and(|end| first_us.saturating_sub(end) > COPY_GAP_US);
            let changed = reading.is_some()
                && self
                    .pending
                    .as_ref()
                    .is_some_and(|packet| packet.body != body);
            if (separated || changed)
                && let Some(packet) = self.finish()
            {
                emit(packet);
            }
            // A timed but damaged row can bridge two intact copies of a packet.
            self.previous_row_end_us = Some(end_us);
            if let Some(reading) = reading {
                let packet = self.pending.get_or_insert(Packet {
                    body,
                    reading,
                    first_us,
                    end_us,
                    copies: 0,
                });
                packet.copies += 1;
                packet.end_us = end_us;
            }
        }
    }

    /// Drain the pending packet when its copy count supports its integrity check.
    pub(super) fn finish(&mut self) -> Option<Packet> {
        self.pending.take().filter(|packet| {
            packet.reading.integrity == Integrity::Crc16AugCcitt
                || packet.copies >= LEGACY_MIN_COPIES
        })
    }
}

fn slice_row(row: &[Pulse]) -> Option<([u8; FRAME_BYTES], Range<usize>)> {
    if row.len() == FRAME_PULSES {
        return decode_cells(row).map(|body| (body, 0..row.len()));
    }
    if row.len() != FRAME_PULSES + 1 {
        return None;
    }
    // Recorded interference inserted an isolated short high before a row or
    // inside a data low. Repair only one extra high that cannot be a data mark.
    let mut spurs = row
        .iter()
        .enumerate()
        .filter(|(_, pulse)| pulse.high_us <= MAX_SPUR_US);
    let (index, spur) = spurs.next()?;
    if spurs.next().is_some() {
        return None;
    }
    let mut corrected = [row[0]; FRAME_PULSES];
    let (before, after) = corrected.split_at_mut(index);
    before.copy_from_slice(&row[..index]);
    after.copy_from_slice(&row[index + 1..]);
    if let Some(previous) = before.last_mut() {
        previous.low_us += spur.high_us + spur.low_us;
    }
    let start = usize::from(index == 0);
    let end = row.len() - usize::from(index + 1 == row.len());
    decode_cells(&corrected).map(|body| (body, start..end))
}

fn decode_cells(row: &[Pulse]) -> Option<[u8; FRAME_BYTES]> {
    let mut body = [0u8; FRAME_BYTES];
    for (index, pulse) in row.iter().enumerate() {
        let bit = if SHORT_US.contains(&pulse.high_us) {
            0
        } else if LONG_US.contains(&pulse.high_us) {
            1
        } else {
            return None;
        };
        if index + 1 < FRAME_PULSES
            && (pulse.low_us < 120 || !CELL_US.contains(&(pulse.high_us + pulse.low_us)))
        {
            return None;
        }
        body[index / 8] = (body[index / 8] << 1) | bit;
    }
    Some(body)
}

fn parse_body(body: [u8; FRAME_BYTES]) -> Option<H5054> {
    let [
        id_high,
        id_low,
        family_and_event,
        event_data,
        check_data,
        footer,
    ] = body;
    let id = u16::from_be_bytes([id_high, id_low]);
    let event_code = family_and_event & 0x0f;
    let (integrity, event) = match family_and_event >> 4 {
        0x3 if crc16(&body) == 0 => (
            Integrity::Crc16AugCcitt,
            match event_code {
                0 => Event::ButtonPress,
                1 => Event::BatteryReport,
                2 => Event::WaterLeak,
                _ => Event::Unknown,
            },
        ),
        0xf if id != u16::MAX && footer & 0xe1 == 0xa1 => {
            let parity = id_high ^ id_low ^ family_and_event ^ event_data ^ check_data;
            if (parity >> 4) ^ (parity & 0x0f) != (footer >> 1) & 0x0f {
                return None;
            }
            let event = match (event_code, event_data) {
                (0xa, 0xfa) => Event::ButtonPress,
                (0xb, 0xfb) => Event::WaterLeak,
                (0xc, 0..=100) => Event::BatteryReport,
                (0xd, 0xfd) => Event::Heartbeat,
                _ => return None,
            };
            (Integrity::LegacyNibbleParity, event)
        }
        _ => return None,
    };
    Some(H5054 {
        id: (id != u16::MAX).then_some(id),
        integrity,
        event,
        event_code,
        event_data,
    })
}

fn crc16(body: &[u8]) -> u16 {
    let mut register = 0x1d0fu16;
    for &byte in body {
        register ^= u16::from(byte) << 8;
        for _ in 0..8 {
            register = (register << 1) ^ (0x1021 * (register >> 15));
        }
    }
    register
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_the_two_wire_formats_independently() {
        assert_eq!(crc16(b"123456789"), 0xe5cc);
        let mut modern = [0x12, 0x34, 0x32, 0x07, 0xf2, 0xaa];
        let reading = parse_body(modern).unwrap();
        assert_eq!(reading.id, Some(0x1234));
        assert_eq!(reading.integrity, Integrity::Crc16AugCcitt);
        assert_eq!(reading.event, Event::WaterLeak);
        assert_eq!(reading.event_data, 7);
        modern[5] ^= 1;
        assert!(parse_body(modern).is_none());

        let mut legacy = [0x43, 0x21, 0xfa, 0xfa, 0xf8, 0xa7];
        let reading = parse_body(legacy).unwrap();
        assert_eq!(reading.event, Event::ButtonPress);
        assert_eq!(reading.integrity, Integrity::LegacyNibbleParity);
        // Inverting one fixed footer bit leaves the four-bit parity intact.
        legacy[5] ^= 0x80;
        assert!(parse_body(legacy).is_none());
        // The related contact-sensor open event carries valid parity.
        assert!(parse_body([0x12, 0x34, 0xfe, 0x7f, 0xf8, 0xb5]).is_none());
    }

    #[test]
    fn recovers_one_leading_or_internal_spur_without_moving_data_edges() {
        let body = [0x12, 0x34, 0x32, 0x07, 0xf2, 0xaa];
        let mut pulses: Vec<_> = body
            .into_iter()
            .flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1))
            .map(|bit| {
                let high_us = [336, 892][usize::from(bit)];
                Pulse {
                    high_us,
                    low_us: 1_136 - high_us,
                }
            })
            .collect();
        pulses.insert(
            0,
            Pulse {
                high_us: 44,
                low_us: 948,
            },
        );
        assert_eq!(slice_row(&pulses), Some((body, 1..49)));
        pulses.remove(0);
        pulses[0].low_us = 524;
        pulses.insert(
            1,
            Pulse {
                high_us: 32,
                low_us: 244,
            },
        );
        assert_eq!(slice_row(&pulses), Some((body, 0..49)));
        pulses[1].high_us = 101;
        assert!(slice_row(&pulses).is_none());
    }
}
