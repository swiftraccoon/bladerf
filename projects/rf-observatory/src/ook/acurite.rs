//! Decode AcuRite Iris, Tower and Atlas pulse-width frames from an OOK burst.
//!
//! The [pinned protocol description] specifies payload fields, integrity
//! checks and pulse timings. The [VN1TXC control] includes a short stop
//! pulse after each 64-bit payload.
//!
//! [pinned protocol description]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/acurite.c
//! [VN1TXC control]: https://github.com/merbanan/rtl_433_tests/tree/1bd5af52f389f0150bda566437e7d5e17e11fc7f/tests/acurite/Acurite_5-n-1

mod atlas;
mod tower;

use std::ops::{Range, RangeInclusive};

use super::{Pulse, Reading};

const ROW_GAP_US: u32 = 500;
const SHORT_HIGH_US: RangeInclusive<u32> = 120..=313;
const LONG_HIGH_US: RangeInclusive<u32> = 314..=514;
const SHORT_LOW_US: RangeInclusive<u32> = 100..=330;
const LONG_LOW_US: RangeInclusive<u32> = 275..=499;
const DATA_PERIOD_US: RangeInclusive<u32> = 500..=725;
const SYNC_US: RangeInclusive<u32> = 515..=850;

/// AcuRite formats admitted on this envelope path.
#[derive(Clone, Copy)]
pub(super) enum Models {
    Iris,
    TowerAndAtlas,
}

/// Emit independently validated frames and their end-exclusive payload spans.
/// Durations use one sample per microsecond. A complete payload may start at
/// the input boundary when the recording missed its sync pulses.
pub(super) fn decode(
    pulses: &[Pulse],
    models: Models,
    mut emit: impl FnMut(Reading, Range<usize>),
) {
    let mut start = 0;
    for row in pulses.chunk_by(|pulse, next| {
        pulse.low_us < ROW_GAP_US && data_bit(pulse).is_some() && data_bit(next).is_some()
    }) {
        let framed_start =
            start == 0 || pulses[start - 1].low_us >= ROW_GAP_US || sync_pulse(&pulses[start - 1]);
        if framed_start && let Some((reading, payload_pulses)) = decode_row(row, models) {
            emit(reading, start..start + payload_pulses);
        }
        start += row.len();
    }
}

fn sync_pulse(pulse: &Pulse) -> bool {
    SYNC_US.contains(&pulse.high_us) && (400..=850).contains(&pulse.low_us)
}

fn data_bit(pulse: &Pulse) -> Option<u8> {
    if SHORT_HIGH_US.contains(&pulse.high_us) {
        Some(0)
    } else if LONG_HIGH_US.contains(&pulse.high_us) {
        Some(1)
    } else {
        None
    }
}

fn decode_row(row: &[Pulse], models: Models) -> Option<(Reading, usize)> {
    let byte_count = row.len() / 8;
    let supported_length = match models {
        Models::Iris => byte_count == 8,
        Models::TowerAndAtlas => matches!(byte_count, 7 | 8 | 10),
    };
    if !supported_length || row.len() % 8 > 1 {
        return None;
    }
    let (payload, tail) = row.split_at(byte_count * 8);
    if let [stop] = tail
        && data_bit(stop) != Some(0)
    {
        return None;
    }
    let mut bytes = [0u8; 10];
    for (index, pulse) in payload.iter().enumerate() {
        let bit = data_bit(pulse)?;
        if index + 1 < row.len() {
            let low = if bit == 0 { LONG_LOW_US } else { SHORT_LOW_US };
            if !low.contains(&pulse.low_us)
                || !DATA_PERIOD_US.contains(&(pulse.high_us + pulse.low_us))
            {
                return None;
            }
        }
        bytes[index / 8] = (bytes[index / 8] << 1) | bit;
    }
    let frame = decode_payload(&bytes[..byte_count], models)?;
    Some((frame, payload.len()))
}

fn decode_payload(bytes: &[u8], models: Models) -> Option<Reading> {
    let (checksum, data) = bytes.split_last()?;
    let parity_bytes = data.get(2..)?;
    if data.iter().copied().fold(0, u8::wrapping_add) != *checksum
        || parity_bytes.iter().any(|byte| byte.count_ones() & 1 != 0)
    {
        return None;
    }
    let channel = match bytes[0] >> 6 {
        3 => 0,
        2 => 1,
        0 => 2,
        _ => return None,
    };
    match models {
        Models::Iris => decode_iris(bytes.try_into().ok()?, channel),
        Models::TowerAndAtlas if bytes.len() == 7 => {
            tower::decode_payload(bytes.try_into().ok()?, channel)
        }
        Models::TowerAndAtlas => atlas::decode_payload(bytes, channel),
    }
}

fn decode_iris(bytes: &[u8; 8], channel: u8) -> Option<Reading> {
    let message_type = bytes[2] & 0x3f;
    if !matches!(message_type, 0x31 | 0x38) {
        return None;
    }
    if message_type == 0x38 {
        let temperature = (u16::from(bytes[4] & 0x0f) << 7) | u16::from(bytes[5] & 0x7f);
        if temperature > 1980 || bytes[6] & 0x7f > 100 {
            return None;
        }
    }
    Some(Reading::Acurite {
        id: u16::from_be_bytes([bytes[0] & 0x0f, bytes[1]]),
        channel,
        message_type,
        sequence: (bytes[0] >> 4) & 0x03,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Channel A, sequence 1, ID 0x4a3, 72°F and 45% relative humidity.
    const VALID_PAYLOAD: [u8; 8] = [0xd4, 0xa3, 0x78, 0x03, 0x88, 0x60, 0x2d, 0x07];

    fn row(bytes: [u8; 8]) -> Vec<Pulse> {
        (0..4)
            .map(|_| Pulse {
                high_us: 620,
                low_us: 596,
            })
            .chain(bytes.into_iter().flat_map(|byte| {
                (0..8).rev().map(move |bit| {
                    let high_us = if byte & (1 << bit) == 0 { 220 } else { 408 };
                    Pulse {
                        high_us,
                        low_us: 612 - high_us,
                    }
                })
            }))
            .chain([Pulse {
                high_us: 220,
                low_us: 572,
            }])
            .collect()
    }

    fn update_checksum(bytes: &mut [u8; 8]) {
        bytes[7] = bytes[..7].iter().copied().fold(0, u8::wrapping_add);
    }

    #[test]
    fn emits_all_valid_spans_after_a_bad_copy() {
        let mut corrupted = VALID_PAYLOAD;
        corrupted[7] ^= 1;
        let mut burst = row(corrupted);
        burst.extend(row(VALID_PAYLOAD));
        burst.extend(row(VALID_PAYLOAD));

        let mut spans = Vec::new();
        decode(&burst, Models::Iris, |reading, span| {
            assert!(matches!(
                reading,
                Reading::Acurite {
                    id: 0x4a3,
                    channel: 0,
                    message_type: 0x38,
                    sequence: 1,
                }
            ));
            spans.push(span);
        });
        assert_eq!(spans, [73..137, 142..206]);
    }

    #[test]
    fn rejects_unrelated_type_reserved_channel_and_byte_parity_errors() {
        let mut unknown_type = VALID_PAYLOAD;
        unknown_type[2] = 0x72;
        let mut reserved_channel = VALID_PAYLOAD;
        reserved_channel[0] ^= 0x80;
        let mut odd_parity = VALID_PAYLOAD;
        odd_parity[3] ^= 1;
        odd_parity[4] ^= 1;
        for mut bytes in [unknown_type, reserved_channel, odd_parity] {
            update_checksum(&mut bytes);
            decode(&row(bytes), Models::Iris, |_, _| {
                panic!("invalid AcuRite row accepted")
            });
        }
    }
}
