//! Validate and decode the WH51 family payload following its FSK sync word.
//!
//! Field layouts and integrity rules follow the [pinned rtl_433 description].
//!
//! [pinned rtl_433 description]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/fineoffset.c#L637-L715

use serde::Serialize;

use crate::ecowitt::{FrameFormat, crc8};

/// Number of bytes after sync, including both integrity bytes.
pub(super) const PAYLOAD_BYTES: usize = 14;

const FAMILY_CODE: u8 = 0x51;
const CRC_INDEX: usize = 12;
const CHECKSUM_INDEX: usize = 13;

/// Decoded fields from a payload that passed both integrity checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Frame {
    /// Unsigned 24-bit sensor identifier.
    pub(super) id: u32,
    /// Encoded battery voltage in millivolts.
    pub(super) battery_mv: u16,
    /// Moisture percentage as sent by the sensor.
    pub(super) moisture_percent: u8,
    /// Remaining transmissions at the temporarily increased reporting rate.
    pub(super) transmission_boost: u8,
    /// Raw nine-bit moisture ADC reading.
    pub(super) moisture_adc: u16,
}

/// Check the family code, MSB-first CRC-8 and wrapping byte sum before decoding.
/// A rejected payload yields `None`.
pub(super) fn decode(payload: &[u8; PAYLOAD_BYTES]) -> Option<Frame> {
    if payload[0] != FAMILY_CODE {
        return None;
    }
    let checksum = payload[..CHECKSUM_INDEX]
        .iter()
        .copied()
        .fold(0, u8::wrapping_add);
    if checksum != payload[CHECKSUM_INDEX] || crc8(&payload[..CRC_INDEX]) != payload[CRC_INDEX] {
        return None;
    }
    Some(Frame {
        id: u32::from_be_bytes([0, payload[1], payload[2], payload[3]]),
        battery_mv: u16::from(payload[4] & 0x1F) * 100,
        moisture_percent: payload[6],
        transmission_boost: payload[4] >> 5,
        moisture_adc: u16::from_be_bytes([payload[7] & 1, payload[8]]),
    })
}

impl FrameFormat<PAYLOAD_BYTES> for Frame {
    const LABEL: &'static str = "WH51";
    const MODEL: &'static str = "ecowitt_wh51";
    const OPERATION: &'static str = "identify_wh51_in_saved_iq";
    const SYNC_WORD: u32 = 0xAA_2D_D4;
    const SYNC_BITS: u32 = 24;
    const FAMILY: u8 = FAMILY_CODE;

    fn decode(payload: &[u8; PAYLOAD_BYTES]) -> Option<Self> {
        decode(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Public payload example in the pinned protocol description.
    const VALID_PAYLOAD: [u8; PAYLOAD_BYTES] = [
        0x51, 0x00, 0x6B, 0x58, 0x6E, 0x7F, 0x24, 0xF8, 0xD2, 0xFF, 0xFF, 0xFF, 0x3C, 0x28,
    ];

    #[test]
    fn decodes_the_documented_frame() {
        assert_eq!(
            decode(&VALID_PAYLOAD),
            Some(Frame {
                id: 0x006B58,
                battery_mv: 1_400,
                moisture_percent: 36,
                transmission_boost: 3,
                moisture_adc: 210,
            }),
        );
    }

    #[test]
    fn requires_both_integrity_checks() {
        let mut bad_sum = VALID_PAYLOAD;
        bad_sum[CHECKSUM_INDEX] ^= 1;
        assert_eq!(decode(&bad_sum), None);

        let mut bad_crc = VALID_PAYLOAD;
        bad_crc[CRC_INDEX] = 0x3D;
        // The CRC byte grew by one, so this keeps the wrapping sum valid.
        bad_crc[CHECKSUM_INDEX] = 0x29;
        assert_eq!(decode(&bad_crc), None);
    }

    #[test]
    fn rejects_a_different_family_with_valid_integrity_bytes() {
        let mut different_family = VALID_PAYLOAD;
        different_family[0] = 0x50;
        different_family[CRC_INDEX] = 0x5B;
        different_family[CHECKSUM_INDEX] = 0x46;
        assert_eq!(decode(&different_family), None);
    }
}
