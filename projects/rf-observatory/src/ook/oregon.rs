//! Recover Oregon Scientific v2.1 and v3 framing from measured OOK edges.
//!
//! The [protocol measurements] describe the Manchester timing and wire bit
//! order. Recognition uses a preamble, a version-specific sync, and complete
//! model-specific framing before passing a body to its integrity check.
//!
//! [protocol measurements]: https://www.osengr.org/Articles/OS-RF-Protocols-IV.pdf#page=28

mod frame;

use std::ops::RangeInclusive;

use super::{Pulse, Reading};
use frame::{THGR122N_NIBBLES, WGR800_NIBBLES};
pub(super) use frame::{Thgr122n, Wgr800};

// A half bit is nominally 488 µs. Threshold placement moves high and low
// edges in opposite directions. Use elapsed time since the last data edge.
const HALF_RUN_US: RangeInclusive<u32> = 240..=1_280;
const DATA_EDGE_US: u64 = 660;
const V2_PREAMBLE_BITS: usize = 16;
const V2_SYNC_BITS: usize = 16;
const V3_PREAMBLE_BITS: usize = 14;
const V3_SYNC_BITS: usize = 4;
const MIN_PULSES: usize = (V3_PREAMBLE_BITS + V3_SYNC_BITS + WGR800_NIBBLES * 4).div_ceil(2);

#[derive(Clone, Copy)]
struct Bit {
    one: bool,
    edge_us: u64,
}

/// Emit each integrity-checked copy and the times of its first sync edge and
/// last body edge. `starts_us` gives the measured rising edge of each pulse.
pub(super) fn decode(pulses: &[Pulse], starts_us: &[u64], mut emit: impl FnMut(Reading, u64, u64)) {
    if pulses.len() < MIN_PULSES {
        return;
    }
    let mut bits = Vec::with_capacity(2 * pulses.len());
    for (index, (pulse, &rise)) in pulses.iter().zip(starts_us).enumerate() {
        if !HALF_RUN_US.contains(&pulse.high_us) {
            scan_row(&bits, &mut emit);
            bits.clear();
            continue;
        }
        if index > 0 && !HALF_RUN_US.contains(&pulses[index - 1].low_us) {
            scan_row(&bits, &mut emit);
            bits.clear();
        }
        record_edge(&mut bits, rise, false);
        record_edge(&mut bits, rise + u64::from(pulse.high_us), true);
    }
    // The last stored low extends to the capture boundary or burst flush.
    // Only the rising edges observed in starts_us can contribute zero bits.
    scan_row(&bits, &mut emit);
}

fn record_edge(bits: &mut Vec<Bit>, edge_us: u64, one: bool) {
    if bits
        .last()
        .is_none_or(|previous| edge_us - previous.edge_us > DATA_EDGE_US)
    {
        bits.push(Bit { one, edge_us });
    }
}

fn scan_row(bits: &[Bit], emit: &mut impl FnMut(Reading, u64, u64)) {
    if let Some((reading, first, last)) = v21(bits) {
        emit(reading, first, last);
    }

    let mut zeros = 0;
    for (start, bit) in bits.iter().enumerate() {
        if !bit.one
            && zeros >= V3_PREAMBLE_BITS
            && let Some((sync, body)) = bits
                .get(start..start + V3_SYNC_BITS + WGR800_NIBBLES * 4)
                .map(|packet| packet.split_at(V3_SYNC_BITS))
            && wire_value(sync) == 0b0101
            && let Some(reading) = frame::wgr800(&reflect::<WGR800_NIBBLES>(body))
        {
            emit(
                Reading::OregonWgr800(reading),
                sync[0].edge_us,
                body[body.len() - 1].edge_us,
            );
        }
        zeros = if bit.one { 0 } else { zeros + 1 };
    }
}

fn v21(bits: &[Bit]) -> Option<(Reading, u64, u64)> {
    // THGR122N and THGR228N share the type and protected prefix. Require
    // the entire 76-bit THGR122N body at the end of the pulse row.
    let body_start = bits.len().checked_sub(THGR122N_NIBBLES * 8)?;
    let sync_start = body_start.checked_sub(V2_SYNC_BITS)?;
    let preamble_start = sync_start.checked_sub(V2_PREAMBLE_BITS)?;
    let body = &bits[body_start..];
    let sync = &bits[sync_start..body_start];
    let preamble = &bits[preamble_start..sync_start];
    if !matches!(wire_value(sync), 0x5599 | 0xaa99)
        || preamble.windows(2).any(|pair| pair[0].one == pair[1].one)
    {
        return None;
    }

    let mut nibbles = [0; THGR122N_NIBBLES];
    for (output, nibble) in nibbles.iter_mut().zip(body.as_chunks::<8>().0) {
        for (position, pair) in nibble.as_chunks::<2>().0.iter().enumerate() {
            if pair[0].one == pair[1].one {
                return None;
            }
            *output |= u8::from(pair[1].one) << position;
        }
    }
    let reading = frame::thgr122n(&nibbles)?;
    Some((
        Reading::OregonThgr122n(reading),
        sync[0].edge_us,
        body.last()?.edge_us,
    ))
}

fn reflect<const N: usize>(wire: &[Bit]) -> [u8; N] {
    let mut nibbles = [0; N];
    for (output, nibble) in nibbles.iter_mut().zip(wire.as_chunks::<4>().0) {
        for (position, bit) in nibble.iter().enumerate() {
            *output |= u8::from(bit.one) << position;
        }
    }
    nibbles
}

fn wire_value(bits: &[Bit]) -> u16 {
    bits.iter()
        .fold(0, |value, bit| (value << 1) | u16::from(bit.one))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Oracle bit rows recovered from the pinned public THGR122N and WGR800X IQ.
    const V21_WIRE: &str = "55555555996a659aaa6aa955aaa9a96aaaa666aa9aa6665a";
    const V3_WIRE: &str = "000000b122400e0006000000197c";
    const HALF_SYMBOL_US: u64 = 488;

    fn wire(hex: &str) -> Vec<bool> {
        hex.chars()
            .flat_map(|digit| {
                let value = digit.to_digit(16).unwrap();
                (0..4).rev().map(move |shift| value & (1 << shift) != 0)
            })
            .collect()
    }

    fn messages(bits: &[bool]) -> Vec<(Reading, u64, u64)> {
        let mut edges = Vec::new();
        let mut rise = None;
        let levels = bits.iter().flat_map(|&bit| [bit, !bit]).chain([false]);
        for (index, level) in levels.enumerate() {
            let time = index as u64 * HALF_SYMBOL_US;
            if level {
                // A later rising threshold shifts high and low widths while
                // leaving the bit clock unchanged.
                rise.get_or_insert(time + 92);
            } else if let Some(start) = rise.take() {
                edges.push((start, time));
            }
        }
        let starts: Vec<_> = edges.iter().map(|&(start, _)| start).collect();
        let pulses: Vec<_> = edges
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| Pulse {
                high_us: (end - start) as u32,
                low_us: starts
                    .get(index + 1)
                    .map_or(2_000, |&next| (next - end) as u32),
            })
            .collect();
        let mut messages = Vec::new();
        decode(&pulses, &starts, |reading, first, last| {
            messages.push((reading, first, last));
        });
        messages
    }

    #[test]
    fn v21_requires_complete_body_and_complementary_pairs() {
        let mut bits = wire(V21_WIRE);
        let decoded = messages(&bits);
        assert_eq!(decoded.len(), 1);
        assert!(matches!(decoded[0].0, Reading::OregonThgr122n(_)));

        bits[52] = bits[53];
        assert!(messages(&bits).is_empty());

        let mut bits = wire(V21_WIRE);
        bits.truncate(176);
        assert!(messages(&bits).is_empty());
    }

    #[test]
    fn v3_replays_each_physical_copy_and_skips_corrupt_or_incomplete_copies() {
        let mut bits = wire(V3_WIRE).repeat(3);
        bits.truncate(335);
        let decoded = messages(&bits);
        assert_eq!(decoded.len(), 3);
        assert!(decoded.iter().all(|&(reading, _, _)| {
            matches!(reading, Reading::OregonWgr800(_)) && reading == decoded[0].0
        }));
        assert!(decoded.windows(2).all(|pair| pair[0].2 < pair[1].1));

        bits[27 + 17 * 4] = !bits[27 + 17 * 4];
        bits.truncate(328);
        assert_eq!(messages(&bits).as_slice(), &decoded[1..2]);
    }
}
