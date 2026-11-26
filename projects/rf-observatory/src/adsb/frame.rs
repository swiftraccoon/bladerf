//! Validate 112-bit Mode S extended squitters from their original wire bytes.
//!
//! [ICAO Annex 10, Volume IV] sections 3.1.2.3.2–3.1.2.3.3 specify parity.
//! Sections 3.1.2.8.6–3.1.2.8.7 describe DF17 and DF18 message formats.
//!
//! [ICAO Annex 10, Volume IV]: https://www.bazl.admin.ch/dam/en/sd-web/SLpAhvMKSY6k/icao_annex_10_aeronauticaltelecommunicationsvolumeiv-surveillanc.pdf

use data_encoding::HEXLOWER;
use serde::{Serialize, Serializer};

const WIRE_BYTES: usize = 14;
const GENERATOR: u32 = 0x01ff_f409;
const PARITY_TOP_BIT: u32 = 1 << 24;

/// A DF17 or DF18 word with exact, zero-overlay parity.
#[derive(Serialize)]
pub(super) struct Frame {
    #[serde(rename = "frame_hex", serialize_with = "serialize_hex")]
    bytes: [u8; WIRE_BYTES],
    /// Validated downlink format from wire bits 1 through 5.
    pub(super) downlink_format: u8,
    /// Wire bits 9 through 32. Their address interpretation depends on the format.
    #[serde(serialize_with = "serialize_hex")]
    address_field: [u8; 3],
    #[serde(skip_serializing_if = "Option::is_none")]
    control_field: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_code: Option<u8>,
}

impl Frame {
    /// Validate the wire format and full CRC before extracting fields.
    pub(super) fn parse(bytes: [u8; WIRE_BYTES]) -> Option<Self> {
        let downlink_format = bytes[0] >> 3;
        if !matches!(downlink_format, 17 | 18) || remainder(&bytes) != 0 {
            return None;
        }
        let control_field = (downlink_format == 18).then_some(bytes[0] & 0b111);
        // DF18 CF0 and CF6 carry the same extended-squitter type-code field.
        let type_code = matches!(control_field, None | Some(0 | 6)).then_some(bytes[4] >> 3);
        Some(Self {
            bytes,
            downlink_format,
            address_field: [bytes[1], bytes[2], bytes[3]],
            control_field,
            type_code,
        })
    }

    /// Return the exact validated wire bytes, including parity.
    pub(super) fn bytes(&self) -> &[u8; WIRE_BYTES] {
        &self.bytes
    }
}

fn remainder(bytes: &[u8; WIRE_BYTES]) -> u32 {
    let mut remainder = 0;
    // Feed the information and parity bits together, most-significant bit first.
    for &byte in bytes {
        for bit in (0..8).rev() {
            remainder = (remainder << 1) | u32::from((byte >> bit) & 1);
            if remainder & PARITY_TOP_BIT != 0 {
                remainder ^= GENERATOR;
            }
        }
    }
    remainder
}

fn serialize_hex<const N: usize, S: Serializer>(
    bytes: &[u8; N],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&HEXLOWER.encode(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Sun, The 1090 Megahertz Riddle, 2nd ed., section 5.3, first message.
    const DF17: &str = "8d40621d58c382d690c8ac2863a7";

    fn wire(hex: &str) -> [u8; WIRE_BYTES] {
        HEXLOWER.decode(hex.as_bytes()).unwrap().try_into().unwrap()
    }

    #[test]
    fn matches_the_published_df17_and_keeps_its_wire_bytes() {
        let frame = Frame::parse(wire(DF17)).unwrap();
        assert_eq!(frame.bytes(), &wire(DF17));
        assert_eq!(
            serde_json::to_value(frame).unwrap(),
            json!({"frame_hex": DF17, "downlink_format": 17, "address_field": "40621d", "type_code": 11})
        );
    }

    #[test]
    fn preserves_df18_control_and_only_labels_supported_type_codes() {
        // Synthetic codewords obtained by independently dividing the padded
        // information field by the Annex 10 polynomial.
        for (hex, control_field, has_type_code) in [
            ("9000abcd2050102030405008ff46", 0, true),
            ("9600abcd20501020304050262c5f", 6, true),
            ("9300abcd20501020304050e06cce", 3, false),
        ] {
            let frame = Frame::parse(wire(hex)).unwrap();
            let record = serde_json::to_value(frame).unwrap();
            assert_eq!(record["frame_hex"], hex);
            assert_eq!(record["downlink_format"], 18);
            assert_eq!(record["address_field"], "00abcd");
            assert_eq!(record["control_field"], control_field);
            if has_type_code {
                assert_eq!(record["type_code"], 4);
            } else {
                assert!(record.get("type_code").is_none());
            }
        }
    }

    #[test]
    fn rejects_every_single_bit_error_and_unsupported_downlink_formats() {
        for original in [wire(DF17), wire("9300abcd20501020304050e06cce")] {
            for bit in 0..8 * WIRE_BYTES {
                let mut corrupted = original;
                corrupted[bit / 8] ^= 1 << (bit % 8);
                assert!(Frame::parse(corrupted).is_none());
            }
        }
        // A synthetic DF20 with a mathematically valid plain remainder still
        // belongs to an unsupported downlink format.
        let unsupported = wire("a000abcd20501020304050819387");
        assert_eq!(remainder(&unsupported), 0);
        assert!(Frame::parse(unsupported).is_none());
    }
}
