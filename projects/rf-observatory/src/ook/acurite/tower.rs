//! Identify AcuRite Tower frames from the [family payload layout].
//!
//! [family payload layout]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/acurite.c#L837-L923

use super::Reading;

/// Check the subtype and sensor values after the shared AcuRite integrity check.
pub(super) fn decode_payload(bytes: &[u8; 7], channel: u8) -> Option<Reading> {
    if bytes[2] & 0x3f != 0x04 {
        return None;
    }
    let humidity = bytes[3] & 0x7f;
    // Temperature-only units use 127 to mark humidity as unavailable.
    if humidity > 100 && humidity != 127 {
        return None;
    }
    let temperature = (u16::from(bytes[4] & 0x7f) << 7) | u16::from(bytes[5] & 0x7f);
    // The specified -40 to 70 °C range is encoded in tenths with a 100 °C offset.
    if !(600..=1700).contains(&temperature) {
        return None;
    }
    Some(Reading::AcuriteTower {
        id: u16::from_be_bytes([bytes[0] & 0x3f, bytes[1]]),
        channel,
        body: *bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Channel A, ID 0x14a3, 45% relative humidity and 23.4 °C.
    const VALID_PAYLOAD: [u8; 7] = [0xd4, 0xa3, 0x44, 0x2d, 0x09, 0xd2, 0xc3];

    #[test]
    fn keeps_all_fourteen_id_bits_with_or_without_humidity() {
        let expected = Some(Reading::AcuriteTower {
            id: 0x14a3,
            channel: 0,
            body: VALID_PAYLOAD,
        });
        assert_eq!(decode_payload(&VALID_PAYLOAD, 0), expected);
        let mut missing_humidity = VALID_PAYLOAD;
        missing_humidity[3] = 0xff;
        assert!(matches!(
            decode_payload(&missing_humidity, 0),
            Some(Reading::AcuriteTower {
                id: 0x14a3,
                channel: 0,
                ..
            })
        ));
    }

    #[test]
    fn rejects_other_subtypes_and_unphysical_sensor_values() {
        for (index, byte) in [(2, 0x48), (3, 0x65), (4, 0), (4, 0x11)] {
            let mut invalid = VALID_PAYLOAD;
            invalid[index] = byte;
            assert!(decode_payload(&invalid, 0).is_none());
        }
    }
}
