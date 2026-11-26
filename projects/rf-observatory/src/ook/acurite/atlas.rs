//! Interpret AcuRite Atlas weather station identifiers in checked frames.
//!
//! The [pinned decoder] describes the three measurement layouts, the optional
//! lightning extension, and the measurement bounds used to reject false frames.
//!
//! [pinned decoder]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/acurite.c#L690-L817

use super::Reading;

/// Extract the identifier from a parity- and checksum-validated Atlas payload.
pub(super) fn decode_payload(bytes: &[u8], channel: u8) -> Option<Reading> {
    let &[header, id_low, type_byte, wind, high, mid, low, ..] = bytes else {
        return None;
    };
    let message_type = type_byte & 0x3f;
    let plausible = match (message_type, bytes.len()) {
        (0x05, 8) | (0x25, 10) => {
            // Temperature in tenths of a degree Fahrenheit, biased by 400.
            let temperature = (u16::from(high & 0x0f) << 7) | u16::from(mid & 0x7f);
            temperature <= 1980 && low & 0x7f <= 100
        }
        (0x06, 8) | (0x26, 10) => {
            let direction_degrees = (u16::from(high & 0x1f) << 5) | u16::from(mid >> 2 & 0x1f);
            direction_degrees <= 360
        }
        (0x07, 8) | (0x27, 10) => {
            // The light intensity field measures tens of lux.
            let light = (u16::from(mid & 0x7f) << 7) | u16::from(low & 0x7f);
            light <= 12_000
        }
        _ => return None,
    };
    let wind_mph = ((wind & 0x7f) << 1) | (high >> 6 & 1);
    (plausible && wind_mph <= 200).then_some(Reading::AcuriteAtlas {
        id: u16::from_be_bytes([header & 0x03, id_low]),
        channel,
        message_type,
        sequence: (header >> 2) & 0x03,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parent verifies framing and integrity before this parser. These
    // synthetic fields fit all three measurement layouts.
    const PAYLOAD: [u8; 10] = [0xa9, 0x42, 0x05, 0, 0x06, 0x11, 0xa3, 0, 0, 0];

    #[test]
    fn preserves_identity_in_all_atlas_variants() {
        for (message_type, length) in [(5, 8), (6, 8), (7, 8), (37, 10), (38, 10), (39, 10)] {
            let mut payload = PAYLOAD;
            payload[2] = message_type | 0xc0;
            assert_eq!(
                decode_payload(&payload[..length], 1),
                Some(Reading::AcuriteAtlas {
                    id: 0x0142,
                    channel: 1,
                    message_type,
                    sequence: 2,
                })
            );
        }
    }

    #[test]
    fn rejects_wrong_layouts_and_impossible_measurements() {
        assert_eq!(decode_payload(&PAYLOAD[..7], 1), None);
        assert_eq!(decode_payload(&PAYLOAD, 1), None);
        let mut hot = PAYLOAD;
        hot[4..6].copy_from_slice(&[0x0f, 0x7f]);
        assert_eq!(decode_payload(&hot[..8], 1), None);
        for (message_type, index, value) in [
            (0x25, 3, 0),
            (0x38, 3, 0),
            (0x05, 3, 0x7f),
            (0x05, 6, 101),
            (0x06, 4, 0x0c),
            (0x07, 5, 0x7f),
        ] {
            let mut payload = PAYLOAD;
            payload[2] = message_type;
            payload[index] = value;
            assert_eq!(decode_payload(&payload[..8], 1), None);
        }
    }
}
