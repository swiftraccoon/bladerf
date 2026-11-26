//! Validate the 32-byte WS90 payload described by the [pinned decoder].
//!
//! [pinned decoder]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/fineoffset_ws90.c#L66-L137

use serde::Serialize;

use crate::ecowitt::{FrameFormat, crc8};

/// Number of body bytes after sync, including both integrity bytes.
pub(super) const PAYLOAD_BYTES: usize = 32;
const CRC_INDEX: usize = 30;
const CHECKSUM_INDEX: usize = 31;
const MISSING_TEMPERATURE: i16 = 0x03ff;
const MISSING_HUMIDITY: u8 = 0xff;

/// Sensor identity and selected measurements from a checked body.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Frame {
    /// Unsigned 24-bit sensor identifier.
    id: u32,
    /// Encoded battery voltage in millivolts.
    battery_mv: u16,
    /// Temperature in tenths of a degree Celsius, if available.
    temperature_deci_c: Option<i16>,
    /// Relative humidity byte as sent by the sensor, if available.
    humidity_percent: Option<u8>,
    /// Firmware version byte as sent by the sensor.
    firmware: u8,
}

impl FrameFormat<PAYLOAD_BYTES> for Frame {
    const LABEL: &'static str = "WS90";
    const MODEL: &'static str = "ecowitt_ws90";
    const OPERATION: &'static str = "identify_ws90_in_saved_iq";
    const SYNC_WORD: u32 = 0xAAAA_2DD4;
    const SYNC_BITS: u32 = 32;
    const FAMILY: u8 = 0x90;

    fn decode(payload: &[u8; PAYLOAD_BYTES]) -> Option<Self> {
        let checksum = payload[..CHECKSUM_INDEX]
            .iter()
            .copied()
            .fold(0, u8::wrapping_add);
        if payload[0] != Self::FAMILY
            || crc8(&payload[..CRC_INDEX]) != payload[CRC_INDEX]
            || checksum != payload[CHECKSUM_INDEX]
        {
            return None;
        }
        let temperature_raw = i16::from_be_bytes([payload[7] & 3, payload[8]]);
        let humidity_raw = payload[9];
        Some(Self {
            id: u32::from_be_bytes([0, payload[1], payload[2], payload[3]]),
            battery_mv: u16::from(payload[6]) * 20,
            temperature_deci_c: (temperature_raw != MISSING_TEMPERATURE)
                .then_some(temperature_raw - 400),
            humidity_percent: (humidity_raw != MISSING_HUMIDITY).then_some(humidity_raw),
            firmware: payload[29],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Extracted from the first public WS90 recording and checked by rtl_433.
    const PAYLOAD: [u8; PAYLOAD_BYTES] = [
        0x90, 0x01, 0x2f, 0x58, 0x16, 0x32, 0x9c, 0xa2, 0xc5, 0x3f, 0x00, 0x32, 0x00, 0x28, 0x3f,
        0xff, 0xa9, 0x80, 0x02, 0x02, 0x3c, 0x35, 0xda, 0x2f, 0xff, 0xef, 0xfe, 0x00, 0x00, 0xa1,
        0x54, 0xc2,
    ];

    #[test]
    fn decodes_the_public_ws90_body() {
        assert_eq!(
            Frame::decode(&PAYLOAD),
            Some(Frame {
                id: 77_656,
                battery_mv: 3_120,
                temperature_deci_c: Some(309),
                humidity_percent: Some(63),
                firmware: 161,
            }),
        );
    }

    #[test]
    fn requires_family_crc_and_checksum_independently() {
        let mut invalid = PAYLOAD;
        invalid[CHECKSUM_INDEX] ^= 1;
        assert_eq!(Frame::decode(&invalid), None);

        let mut invalid = PAYLOAD;
        invalid[CRC_INDEX] += 1;
        invalid[CHECKSUM_INDEX] += 1;
        assert_eq!(Frame::decode(&invalid), None);

        let mut invalid = PAYLOAD;
        invalid[0] = 0x51;
        repair_integrity(&mut invalid);
        assert_eq!(Frame::decode(&invalid), None);
    }

    #[test]
    fn retains_identity_with_missing_weather_measurements() {
        let mut missing = PAYLOAD;
        missing[7] |= 3;
        missing[8] = 0xff;
        missing[9] = 0xff;
        repair_integrity(&mut missing);
        let report = serde_json::to_value(Frame::decode(&missing).unwrap()).unwrap();
        assert_eq!(report["id"], 77_656);
        assert!(report["temperature_deci_c"].is_null());
        assert!(report["humidity_percent"].is_null());
    }

    fn repair_integrity(payload: &mut [u8; PAYLOAD_BYTES]) {
        payload[CRC_INDEX] = crc8(&payload[..CRC_INDEX]);
        payload[CHECKSUM_INDEX] = payload[..CHECKSUM_INDEX]
            .iter()
            .copied()
            .fold(0, u8::wrapping_add);
    }
}
