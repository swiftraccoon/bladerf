//! Identify the reporting station in an AIS HDLC body after NRZI decoding.
//!
//! [ITU-R M.1371-6] §§ A2-3.2.2.6 and A2-3.3.7 specify the FCS and
//! bit order. Annex 7 defines the message lengths and fields used here.
//!
//! [ITU-R M.1371-6]: https://www.itu.int/dms_pubrec/itu-r/rec/m/R-REC-M.1371-6-202602-I%21%21PDF-E.pdf

use serde::{Serialize, Serializer};

use crate::dsp::hdlc::fcs;

/// An FCS-valid AIS station observation. The MMSI belongs to the source field.
#[derive(Debug, Serialize)]
pub(super) struct Frame {
    message_type: u8,
    repeat_indicator: u8,
    #[serde(serialize_with = "serialize_mmsi")]
    mmsi: u32,
    fcs: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    class_b_unit: Option<ClassBUnit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    static_data: Option<StaticData>,
}

fn serialize_mmsi<S: Serializer>(mmsi: &u32, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("{mmsi:09}"))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClassBUnit {
    Sotdma,
    CarrierSense,
}

#[derive(Debug, Serialize)]
#[serde(tag = "part", rename_all = "snake_case")]
enum StaticData {
    A,
    B {
        #[serde(skip_serializing_if = "Option::is_none")]
        equipment: Option<EquipmentIdentity>,
    },
}

#[derive(Debug, Serialize)]
struct EquipmentIdentity {
    manufacturer_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_code: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    serial_number: Option<u32>,
}

/// Decode de-stuffed bytes through their little-endian HDLC FCS.
///
/// The caller has assembled each over-the-air byte least-significant bit
/// first. Numeric payload fields are read most-significant bit first.
/// Unknown types, invalid lengths, and invalid FCS or source IDs emit nothing.
pub(super) fn decode(bytes: &[u8]) -> Option<Frame> {
    let (body, wire_fcs) = bytes.split_last_chunk::<2>()?;
    let &[header, a, b, c, d] = body.first_chunk::<5>()?;
    let message_type = header >> 2;
    let mmsi = u32::from_be_bytes([a, b, c, d]) >> 2;
    let expected_fcs = u16::from_le_bytes(*wire_fcs);
    if !valid_length(message_type, body)
        || !(1..=999_999_999).contains(&mmsi)
        || fcs(body) != expected_fcs
    {
        return None;
    }
    let class_b_unit = (message_type == 18).then(|| match body[17] & 0x04 {
        0 => ClassBUnit::Sotdma,
        _ => ClassBUnit::CarrierSense,
    });
    let static_data = (message_type == 24).then(|| match d & 0x03 {
        0 => StaticData::A,
        _ => StaticData::B {
            equipment: equipment_identity(body),
        },
    });
    Some(Frame {
        message_type,
        repeat_indicator: header & 0x03,
        mmsi,
        fcs: expected_fcs,
        class_b_unit,
        static_data,
    })
}

fn valid_length(message_type: u8, body: &[u8]) -> bool {
    let length = body.len();
    match message_type {
        1..=4 | 9 | 11 | 18 | 22 | 28 => length == 21,
        5 => length == 53,
        6 => (11..=126).contains(&length),
        7 | 13 => matches!(length, 9 | 13 | 17 | 21),
        8 => (7..=126).contains(&length),
        10 => length == 9,
        12 => (9..=126).contains(&length),
        14 => (5..=126).contains(&length),
        15 => matches!(length, 11 | 14 | 20),
        16 => matches!(length, 12 | 18),
        17 => {
            length == 10
                || body.get(14).is_some_and(|byte| {
                    let words = usize::from(byte >> 3);
                    words <= 29 && length == 15 + 3 * words
                })
        }
        19 => length == 39,
        20 => matches!(length, 9 | 13 | 17 | 20),
        21 => (34..=45).contains(&length),
        23 => length == 20,
        24 => matches!((body[4] & 0x03, length), (0, 20) | (1, 21)),
        25 | 26 => {
            let addressed = body[4] & 0x02 != 0;
            let has_application = body[4] & 0x01 != 0;
            let header = 5 + 4 * usize::from(addressed) + 2 * usize::from(has_application);
            if message_type == 25 {
                (header..=21).contains(&length)
            } else {
                (header + 3..=133).contains(&length)
            }
        }
        27 => length == 12,
        _ => false,
    }
}

fn equipment_identity(body: &[u8]) -> Option<EquipmentIdentity> {
    // Table 78 defines the first three six-bit characters as an NMEA code.
    // Model zero marks manufacturer-only reports, including those from Class A.
    let manufacturer_code = [48, 54, 60]
        .into_iter()
        .map(|start| match payload_bits(body, start, 6) as u8 {
            value @ 1..=26 => Some(char::from(b'A' + value - 1)),
            value @ b'0'..=b'9' => Some(char::from(value)),
            _ => None,
        })
        .collect::<Option<String>>()?;
    let model_code = payload_bits(body, 66, 4) as u8;
    Some(EquipmentIdentity {
        manufacturer_code,
        model_code: (model_code != 0).then_some(model_code),
        serial_number: (model_code != 0).then(|| payload_bits(body, 70, 20)),
    })
}

fn payload_bits(body: &[u8], start: usize, width: usize) -> u32 {
    (start..start + width).fold(0, |value, bit| {
        (value << 1) | u32::from((body[bit / 8] >> (7 - bit % 8)) & 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER;
    use serde_json::json;

    fn decode_hex(hex: &str) -> Frame {
        decode(&HEXLOWER.decode(hex.as_bytes()).unwrap()).unwrap()
    }

    // Fictitious IDs and independently generated CRCs from CCITT with
    // explicit input/output reflection. The vectors contain complete octets.
    const BASE_STATION: &str = "1202f185380000000000000000000000000000000085d3";
    const CLASS_B: &str = "491d6f345400000000000000000000000004000000f815";
    const AUXILIARY_B: &str = "60e9a67405000420d6048d000000000001d6f34540a260";

    #[test]
    fn checks_msb_first_fields_fcs_and_leading_zero_source_id() {
        let report = serde_json::to_value(decode_hex(BASE_STATION)).unwrap();
        assert_eq!(
            report,
            json!({
                "message_type": 4,
                "repeat_indicator": 2,
                "mmsi": "012345678",
                "fcs": 0xd385,
            })
        );
        let mut damaged = HEXLOWER.decode(BASE_STATION.as_bytes()).unwrap();
        damaged[10] ^= 0x01;
        assert!(decode(&damaged).is_none());
    }

    #[test]
    fn reads_the_class_b_unit_flag_and_equipment_fields() {
        let position = serde_json::to_value(decode_hex(CLASS_B)).unwrap();
        assert_eq!(position["class_b_unit"], "carrier_sense");
        let static_data = serde_json::to_value(decode_hex(AUXILIARY_B)).unwrap();
        // The old auxiliary-vessel form placed a parent MMSI at bit 132.
        // In either form, the source field still identifies the reporter.
        assert_eq!(static_data["mmsi"], "980000001");
        assert_eq!(
            static_data["static_data"],
            json!({
                "part": "b",
                "equipment": {
                    "manufacturer_code": "ABC",
                    "model_code": 5,
                    "serial_number": 0x81234,
                },
            })
        );
    }

    #[test]
    fn rejects_wrong_lengths_and_reserved_message_types_even_with_valid_fcs() {
        let mut body = [0u8; 21];
        body[4] = 4;
        for (header, length) in [(4 << 2, 20), (24 << 2, 21), (29 << 2, 21)] {
            body[0] = header;
            let mut frame = body[..length].to_vec();
            frame.extend_from_slice(&fcs(&frame).to_le_bytes());
            assert!(decode(&frame).is_none());
        }
        body[0] = 26 << 2;
        body[4] |= 0x03;
        // Addressed Type 26 with application ID needs 14 body octets.
        assert!(!valid_length(26, &body[..13]));
        assert!(valid_length(26, &body[..14]));
    }
}
