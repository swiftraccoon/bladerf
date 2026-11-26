//! Validate Toyota TPMS payloads after differential Manchester decoding.
//!
//! The field layouts and integrity checks follow the pinned rtl_433 decoders
//! for [PMV-C210] and [PMV-107J].
//!
//! [PMV-C210]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/tpms_toyota.c
//! [PMV-107J]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/tpms_pmv107j.c

use serde::Serialize;

/// Bytes in the PMV-C210 payload, including the trailing CRC-8.
pub(super) const C210_PAYLOAD_BYTES: usize = 9;
/// Bytes in the PMV-107J payload, including six leading zero bits and the CRC-8.
pub(super) const PMV107J_PAYLOAD_BYTES: usize = 9;

/// Sensor fields admitted by the protocol's integrity checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "model")]
pub(super) enum Frame {
    /// The PMV-C210 payload contains a 32-bit sensor identifier.
    #[serde(rename = "pmv_c210")]
    PmvC210 {
        /// The identifier transmitted by the pressure sensor.
        id: u32,
        /// Status bits assembled from the packed payload.
        status: u8,
        /// Reported tire pressure in quarter-PSI units after applying the encoding offset.
        pressure_quarter_psi: i16,
        /// Reported sensor temperature in whole degrees Celsius.
        temperature_c: i16,
    },
    /// The PMV-107J payload contains a 28-bit sensor identifier.
    #[serde(rename = "pmv_107j")]
    Pmv107j {
        /// The identifier transmitted by the pressure sensor.
        id: u32,
        /// The six status bits from the payload, including the reserved bit.
        status: u8,
        /// Whether the sensor reports sufficient battery voltage.
        battery_ok: bool,
        /// The two-bit repeat counter from the sensor.
        repeat_counter: u8,
        /// Whether the sensor reports a rapid pressure change.
        rapid_change: bool,
        /// Whether the sensor reports a failure.
        failed: bool,
        /// Reported tire pressure in pascals after applying the encoding offset.
        pressure_pa: i32,
        /// Reported sensor temperature in whole degrees Celsius.
        temperature_c: i16,
    },
}

/// Decode a PMV-C210 payload with a valid CRC-8 and complementary pressure byte.
pub(super) fn decode_c210(payload: &[u8; C210_PAYLOAD_BYTES]) -> Option<Frame> {
    let [
        id0,
        id1,
        id2,
        id3,
        pressure_status,
        temperature_pressure,
        status_temperature,
        inverse_pressure,
        expected_crc,
    ] = *payload;
    let pressure = ((pressure_status & 0x7f) << 1) | (temperature_pressure >> 7);
    if pressure != !inverse_pressure
        || crc8(&payload[..C210_PAYLOAD_BYTES - 1], 0x07, 0x80) != expected_crc
    {
        return None;
    }

    let temperature = ((temperature_pressure & 0x7f) << 1) | (status_temperature >> 7);
    Some(Frame::PmvC210 {
        id: u32::from_be_bytes([id0, id1, id2, id3]),
        status: (pressure_status & 0x80) | (status_temperature & 0x7f),
        pressure_quarter_psi: i16::from(pressure) - 28,
        temperature_c: i16::from(temperature) - 40,
    })
}

/// Decode a PMV-107J payload with a valid CRC-8 and complementary pressure byte.
/// The first six bits must be zero to align the 66-bit payload in nine bytes.
pub(super) fn decode_107j(payload: &[u8; PMV107J_PAYLOAD_BYTES]) -> Option<Frame> {
    let [
        id0,
        id1,
        id2,
        id3,
        status_id,
        pressure,
        inverse_pressure,
        temperature,
        expected_crc,
    ] = *payload;
    if id0 & !0x03 != 0
        || pressure != !inverse_pressure
        || crc8(&payload[..PMV107J_PAYLOAD_BYTES - 1], 0x13, 0) != expected_crc
    {
        return None;
    }

    let id = (u32::from_be_bytes([id0, id1, id2, id3]) << 2) | u32::from(status_id >> 6);
    let status = status_id & 0x3f;
    Some(Frame::Pmv107j {
        id,
        status,
        battery_ok: status & 0x20 == 0,
        repeat_counter: (status >> 3) & 3,
        rapid_change: status & 0x02 != 0,
        failed: status & 0x01 != 0,
        pressure_pa: (i32::from(pressure) - 40) * 2480,
        temperature_c: i16::from(temperature) - 40,
    })
}

fn crc8(bytes: &[u8], polynomial: u8, initial: u8) -> u8 {
    let mut remainder = initial;
    for &byte in bytes {
        remainder ^= byte;
        for _ in 0..u8::BITS {
            let feedback = if remainder & 0x80 == 0 { 0 } else { polynomial };
            remainder = (remainder << 1) ^ feedback;
        }
    }
    remainder
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic sensor fields encoded using the documented layout. The CRCs
    // were calculated separately by polynomial long division.
    const PMV_C210: [u8; C210_PAYLOAD_BYTES] =
        [0x12, 0x34, 0xab, 0xcd, 0xce, 0xa1, 0x85, 0x62, 0x72];
    const PMV_107J: [u8; PMV107J_PAYLOAD_BYTES] =
        [0x02, 0xaf, 0x37, 0xbc, 0x73, 0x84, 0x7b, 0x0c, 0x65];

    #[test]
    fn decodes_c210_fields_split_across_byte_boundaries() {
        assert_eq!(
            decode_c210(&PMV_C210),
            Some(Frame::PmvC210 {
                id: 0x1234_abcd,
                status: 0x85,
                pressure_quarter_psi: 129,
                temperature_c: 27,
            })
        );
    }

    #[test]
    fn requires_c210_integrity_checks() {
        let mut bad_crc = PMV_C210;
        bad_crc[8] ^= 1;
        assert_eq!(decode_c210(&bad_crc), None);

        let mut inconsistent_pressure = PMV_C210;
        inconsistent_pressure[7] = 0x63;
        inconsistent_pressure[8] = 0x75;
        assert_eq!(decode_c210(&inconsistent_pressure), None);
    }

    #[test]
    fn preserves_signed_encoding_offsets() {
        assert_eq!(
            decode_c210(&[0, 0, 0, 0, 0, 0, 0x7f, 0xff, 0x2d]),
            Some(Frame::PmvC210 {
                id: 0,
                status: 0x7f,
                pressure_quarter_psi: -28,
                temperature_c: -40,
            })
        );
    }

    #[test]
    fn decodes_107j_identifier_and_status_bits() {
        assert_eq!(
            decode_107j(&PMV_107J),
            Some(Frame::Pmv107j {
                id: 0x0abc_def1,
                status: 0x33,
                battery_ok: false,
                repeat_counter: 2,
                rapid_change: true,
                failed: true,
                pressure_pa: 228_160,
                temperature_c: -28,
            })
        );
    }

    #[test]
    fn requires_107j_integrity_checks_and_padding() {
        let mut bad_crc = PMV_107J;
        bad_crc[8] ^= 1;
        assert_eq!(decode_107j(&bad_crc), None);

        let mut inconsistent_pressure = PMV_107J;
        inconsistent_pressure[6] = 0x7a;
        inconsistent_pressure[8] = 0x73;
        assert_eq!(decode_107j(&inconsistent_pressure), None);

        let mut nonzero_padding = PMV_107J;
        nonzero_padding[0] = 0x42;
        nonzero_padding[8] = 0x7a;
        assert_eq!(decode_107j(&nonzero_padding), None);
    }
}
