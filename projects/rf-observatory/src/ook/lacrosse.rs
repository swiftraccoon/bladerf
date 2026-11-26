//! Decode TX141TH-Bv2-compatible pulse-width frames from a burst.
//!
//! The [pinned protocol description] specifies four sync pulses, 40 data
//! pulses and a reflected eight-bit LFSR digest per frame.
//!
//! [pinned protocol description]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/lacrosse_tx141x.c#L26-L58

use std::ops::{Range, RangeInclusive};

use super::{Pulse, Reading};

const SYNC_COUNT: usize = 4;
const DATA_COUNT: usize = 40;
const FRAME_COUNT: usize = SYNC_COUNT + DATA_COUNT;
const SHORT_US: RangeInclusive<u32> = 100..=305;
const LONG_US: RangeInclusive<u32> = 310..=530;
const SYMBOL_US: RangeInclusive<u32> = 495..=790;
const SYNC_US: RangeInclusive<u32> = 650..=1030;
const MIN_LOW_US: u32 = 80;

/// Emit every independently validated frame with its end-exclusive pulse span.
///
/// Input contains the complete sync and data train at one sample per
/// microsecond. The last low period need not fit the data symbol timing.
pub(super) fn decode(pulses: &[Pulse], mut emit: impl FnMut(Reading, Range<usize>)) {
    for (start, frame) in pulses.windows(FRAME_COUNT).enumerate() {
        let (sync, data) = frame.split_at(SYNC_COUNT);
        if !sync.iter().all(sync_pulse) {
            continue;
        }
        let end = start + FRAME_COUNT;
        // Some units send an extra pulse before the next sync. Only the
        // first 40 pulses carry validated data.
        if let Some(trailer) = pulses.get(end)
            && data[DATA_COUNT - 1].low_us < *SYNC_US.start()
            && data_high(trailer.high_us).is_some()
            && trailer.low_us < *SYNC_US.start()
            && pulses
                .get(end + 1)
                .is_some_and(|next| data_high(next.high_us).is_some())
        {
            continue;
        }
        if let Some(reading) = payload(data) {
            emit(reading, start..end);
        }
    }
}

fn sync_pulse(pulse: &Pulse) -> bool {
    SYNC_US.contains(&pulse.high_us) && SYNC_US.contains(&pulse.low_us)
}

fn data_high(high_us: u32) -> Option<u8> {
    if SHORT_US.contains(&high_us) {
        Some(0)
    } else if LONG_US.contains(&high_us) {
        Some(1)
    } else {
        None
    }
}

fn payload(pulses: &[Pulse]) -> Option<Reading> {
    let mut bytes = [0u8; DATA_COUNT / 8];
    for (index, pulse) in pulses.iter().enumerate() {
        let bit = data_high(pulse.high_us)?;
        // A power threshold can move pulse edges without changing the
        // symbol period. Classify the high width and check the full period.
        if index + 1 < DATA_COUNT
            && (pulse.low_us < MIN_LOW_US
                || pulse
                    .high_us
                    .checked_add(pulse.low_us)
                    .is_none_or(|period| !SYMBOL_US.contains(&period)))
        {
            return None;
        }
        bytes[index / 8] = (bytes[index / 8] << 1) | bit;
    }

    let [id, flags_and_temperature, temperature_low, humidity, digest] = bytes;
    let temperature = u16::from_be_bytes([flags_and_temperature & 0x0f, temperature_low]);
    if id == 0 || !(1..=100).contains(&humidity) || !(100..=1900).contains(&temperature) {
        return None;
    }
    let body = [id, flags_and_temperature, temperature_low, humidity];
    (frame_digest(body) == digest).then_some(Reading::LaCrosse {
        id,
        channel: (flags_and_temperature >> 4) & 0x03,
    })
}

fn frame_digest(body: [u8; 4]) -> u8 {
    // Walk the body from least-significant bit to most-significant bit.
    let body = u32::from_be_bytes(body);
    let mut key = 0xf4u8;
    let mut digest = 0;
    for index in 0..u32::BITS {
        if body & (1 << index) != 0 {
            digest ^= key;
        }
        key = (key << 1) ^ ((key >> 7) * 0x31);
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    // First valid row in the pinned rtl_433 single-intact-copy regression.
    // Regression bytes are from its inverse-polarity slicer.
    const VALID_ROW: [u8; 5] = [0xe7, 0x11, 0x2c, 0x0a, 0x9e];

    fn frame(bytes: [u8; 5], short_us: u32, long_us: u32, sync_us: u32) -> Vec<Pulse> {
        (0..SYNC_COUNT)
            .map(|_| Pulse {
                high_us: sync_us,
                low_us: sync_us,
            })
            .chain(bytes.into_iter().flat_map(|byte| {
                (0..8).rev().map(move |bit| {
                    let high_us = if byte & (1 << bit) == 0 {
                        short_us
                    } else {
                        long_us
                    };
                    Pulse {
                        high_us,
                        low_us: short_us + long_us - high_us,
                    }
                })
            }))
            .collect()
    }

    #[test]
    fn accepts_one_intact_copy_after_a_bad_digest() {
        let mut damaged = VALID_ROW;
        damaged[3] ^= 1;
        let mut burst = frame(damaged, 208, 417, 833);
        let mut valid = frame(VALID_ROW, 208, 417, 833);
        valid[FRAME_COUNT - 1].low_us = 2_000;
        burst.extend(valid);

        let mut count = 0;
        decode(&burst, |reading, extent| {
            assert!(matches!(
                reading,
                Reading::LaCrosse {
                    id: 0xe7,
                    channel: 1
                }
            ));
            assert_eq!(extent, FRAME_COUNT..2 * FRAME_COUNT);
            count += 1;
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn accepts_the_single_trailer_seen_in_field_pulses() {
        // Body and pulse widths from the recorded TFA 30.3221 display in
        // https://github.com/merbanan/rtl_433/issues/2206.
        let mut burst = Vec::new();
        for (high_us, low_us) in [(244, 484), (484, 10_004)] {
            burst.extend(frame([0xb1, 0x82, 0xc6, 0x3b, 0x38], 244, 484, 836));
            burst.push(Pulse { high_us, low_us });
        }
        let mut count = 0;
        decode(&burst, |reading, extent| {
            assert!(matches!(
                reading,
                Reading::LaCrosse {
                    id: 0xb1,
                    channel: 0
                }
            ));
            let start = count * (FRAME_COUNT + 1);
            assert_eq!(extent, start..start + FRAME_COUNT);
            count += 1;
        });
        assert_eq!(count, 2);
    }

    #[test]
    fn accepts_edge_displacement_with_the_observed_symbol_period() {
        let mut pulses = frame([0x43, 0x02, 0x51, 0x49, 0xd8], 244, 484, 836);
        for pulse in &mut pulses[SYNC_COUNT..] {
            pulse.high_us -= 88;
            pulse.low_us += 88;
        }

        let mut count = 0;
        decode(&pulses, |reading, _| {
            assert!(matches!(
                reading,
                Reading::LaCrosse {
                    id: 0x43,
                    channel: 0
                }
            ));
            count += 1;
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn rejects_missing_sync_and_multiple_trailer_pulses() {
        let mut pulses = frame(VALID_ROW, 208, 417, 833);
        decode(&pulses[1..], |_, _| panic!("missing sync pulse accepted"));
        pulses.push(Pulse {
            high_us: 208,
            low_us: 417,
        });
        pulses.push(Pulse {
            high_us: 208,
            low_us: 2_000,
        });
        decode(&pulses, |_, _| panic!("42-bit row accepted"));
    }
}
