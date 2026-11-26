//! Validate Basic and Long UAT ADS-B codewords and extract their address fields.
//!
//! [ICAO Annex 10, Volume III] §12.4.4.1.3 specifies parity. ICAO working
//! paper ACP-WGW01/WP09 appendices E and G describe the type check and
//! provide example codewords. [FlightAware dump978] documents the address
//! qualifier values used to interpret the 24-bit address field.
//!
//! [ICAO Annex 10, Volume III]: https://www.bazl.admin.ch/dam/en/sd-web/QQ3WlrAoWJRb/icao_annex_10_aeronauticaltelecommunicationsvolumeiii-communicat.pdf
//! [FlightAware dump978]: https://github.com/flightaware/dump978/blob/f40aa9a0d2d0a0067c0c628b44ba316f7049b8ce/uat_message.h#L155-L156

use data_encoding::HEXLOWER;
use serde::{Serialize, Serializer};

use crate::dsp::reed_solomon::ReedSolomon;

pub(super) const BASIC_BYTES: usize = 30;
pub(super) const LONG_BYTES: usize = 48;
static REED_SOLOMON: ReedSolomon = ReedSolomon::new(0x187, 120);

struct Codeword {
    bytes: [u8; LONG_BYTES],
    len: usize,
}

impl Codeword {
    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_hex(self.bytes(), serializer)
    }
}

/// A UAT ADS-B observation backed by a corrected and syndrome-checked word.
#[derive(Serialize)]
pub(super) struct Frame {
    #[serde(rename = "frame_hex", serialize_with = "Codeword::serialize")]
    codeword: Codeword,
    /// Number of message bytes preceding the parity bytes.
    pub(super) message_bytes: usize,
    /// Number of changed symbols across the message and parity bytes.
    pub(super) corrected_symbols: u8,
    /// Type code from the first five bits of the message.
    pub(super) payload_type: u8,
    /// Qualifier from the last three bits of the first byte.
    pub(super) address_qualifier: u8,
    address_qualifier_kind: &'static str,
    /// Wire address in the namespace selected by the qualifier.
    #[serde(serialize_with = "serialize_hex")]
    pub(super) address_field: [u8; 3],
}

impl Frame {
    /// Check one complete Basic or Long ADS-B codeword, excluding synchronization.
    ///
    /// The caller establishes the ADS-B sync pattern and supplies bytes in
    /// transmission order. The type and address checks use corrected bytes.
    pub(super) fn parse(received: &[u8]) -> Option<Self> {
        let message_bytes = match received.len() {
            BASIC_BYTES => 18,
            LONG_BYTES => 34,
            _ => return None,
        };
        let mut codeword = Codeword {
            bytes: [0; LONG_BYTES],
            len: received.len(),
        };
        let word = &mut codeword.bytes[..codeword.len];
        // On-air bytes start at the highest polynomial degree.
        for (symbol, &byte) in word.iter_mut().zip(received.iter().rev()) {
            *symbol = byte;
        }
        let corrected_symbols = REED_SOLOMON.correct(word, received.len() - message_bytes)?;
        word.reverse();

        let payload_type = word[0] >> 3;
        if (payload_type == 0) != (received.len() == BASIC_BYTES) {
            return None;
        }
        let address_qualifier = word[0] & 7;
        let address_field = [word[1], word[2], word[3]];
        if matches!(address_qualifier, 0 | 2) && address_field == [0; 3] {
            return None;
        }
        Some(Self {
            codeword,
            message_bytes,
            corrected_symbols: u8::try_from(corrected_symbols).ok()?,
            payload_type,
            address_qualifier,
            address_qualifier_kind: address_qualifier_kind(address_qualifier),
            address_field,
        })
    }

    /// Corrected bytes in transmission order, including parity.
    pub(super) fn bytes(&self) -> &[u8] {
        self.codeword.bytes()
    }
}

fn address_qualifier_kind(qualifier: u8) -> &'static str {
    match qualifier {
        0 => "adsb_icao",
        // Includes self-assigned temporary addresses.
        1 => "adsb_other",
        2 => "tisb_icao",
        3 => "tisb_trackfile",
        4 => "surface_vehicle",
        5 => "fixed_beacon",
        6 => "adsr_other",
        _ => "reserved",
    }
}

fn serialize_hex<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&HEXLOWER.encode(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // ICAO UAT Implementation Manual, Appendix G, sections G.2 and G.3.
    const BASIC: &str = "00faa123555555c000000354064432c02800fe97c434e1ff5365cf8fafe4";
    const LONG: &str = concat!(
        "08faa123555555c000000354064432c0280e1b4d090cfc0488740000000350000000",
        "0e1985ff9c6b33066ca9f43d12e8",
    );

    fn wire(hex: &str) -> Vec<u8> {
        HEXLOWER.decode(hex.as_bytes()).unwrap()
    }

    #[test]
    fn validates_both_published_codewords_and_corrects_through_the_bound() {
        for (hex, payload_type, message_bytes, errors) in [(BASIC, 0, 18, 6), (LONG, 1, 34, 7)] {
            let expected = wire(hex);
            let mut received = expected.clone();
            let clean = Frame::parse(&received).unwrap();
            assert_eq!(clean.bytes(), expected);
            assert_eq!(clean.corrected_symbols, 0);

            // Exercise type, address, message and parity errors in the same word.
            let positions = [0, 1, 3, 7, 16, received.len() - 3, received.len() - 1];
            for (index, position) in positions.into_iter().take(errors).enumerate() {
                received[position] ^= (53 * index + 29) as u8;
            }
            let corrected = Frame::parse(&received).unwrap();
            assert_eq!(corrected.bytes(), expected);
            assert_eq!(
                serde_json::to_value(corrected).unwrap(),
                json!({
                    "frame_hex": hex,
                    "message_bytes": message_bytes,
                    "corrected_symbols": errors,
                    "payload_type": payload_type,
                    "address_qualifier": 0,
                    "address_qualifier_kind": "adsb_icao",
                    "address_field": "faa123",
                }),
            );
        }
    }

    #[test]
    fn rejects_incomplete_uncorrectable_and_wrong_size_types() {
        let mut basic = wire(BASIC);
        assert!(Frame::parse(&basic[..BASIC_BYTES - 1]).is_none());
        basic.push(0);
        assert!(Frame::parse(&basic).is_none());
        basic.pop();
        for (index, symbol) in basic.iter_mut().step_by(4).take(7).enumerate() {
            *symbol ^= (53 * index + 29) as u8;
        }
        assert!(Frame::parse(&basic).is_none());
        assert!(Frame::parse(&[0; BASIC_BYTES]).is_none());

        // Valid syndromes but length-incompatible type from polynomial division.
        let wrong_basic = "08faa1230000000000000000000000000000b5004f7f576aa173810f9bee";
        let wrong_long = concat!(
            "00faa123000000000000000000000000000000000000000000000000000000000000",
            "0608e61c5ac89fe3f7bde428b72b",
        );
        assert!(Frame::parse(&wire(wrong_basic)).is_none());
        assert!(Frame::parse(&wire(wrong_long)).is_none());
    }

    #[test]
    fn preserves_other_and_reserved_address_namespaces() {
        // Synthetic Basic codewords from independent polynomial division.
        for (hex, qualifier, kind) in [
            (
                "010012340000000000000000000000000000cbbdcaf0e16d083fa00b8035",
                1,
                "adsb_other",
            ),
            (
                "0700123400000000000000000000000000009452e77a35e9fcb9ecb179fc",
                7,
                "reserved",
            ),
        ] {
            let frame = serde_json::to_value(Frame::parse(&wire(hex)).unwrap()).unwrap();
            assert_eq!(frame["address_field"], "001234");
            assert_eq!(frame["address_qualifier"], qualifier);
            assert_eq!(frame["address_qualifier_kind"], kind);
        }
    }
}
