//! Assemble Programme Service and RadioText from validated RDS groups.
//!
//! Group types, segment addressing and the text A/B flag follow IEC
//! 62106-2:2021, sections 6.1 and 6.3.

use std::collections::BTreeMap;

use data_encoding::HEXLOWER;
use serde::Serialize;

const VERSION_B: u16 = 1 << 11;
const TEXT_AB: u16 = 1 << 4;
const END_OF_TEXT: u8 = 0x0d;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Field {
    ProgrammeService,
    RadioText,
}

#[derive(Serialize)]
pub(super) struct Observation {
    programme_identification: u16,
    field: Field,
    decision_input_sample: u64,
    bytes_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

struct Segments<const N: usize> {
    bytes: [u8; N],
    received: u16,
    previous: Option<Vec<u8>>,
}

impl<const N: usize> Default for Segments<N> {
    fn default() -> Self {
        Self {
            bytes: [0; N],
            received: 0,
            previous: None,
        }
    }
}

impl<const N: usize> Segments<N> {
    fn insert(&mut self, address: usize, segment: &[u8]) {
        let start = address * segment.len();
        let range = start..start + segment.len();
        let bit = 1 << address;
        if self.received & bit != 0 && &self.bytes[range.clone()] != segment {
            // Conflicting bytes end the preceding assembly, including any holes.
            self.received = 0;
        }
        self.bytes[range].copy_from_slice(segment);
        self.received |= bit;
    }

    fn prefix_length(&self, segment_width: usize) -> usize {
        self.received.trailing_ones() as usize * segment_width
    }

    fn emit_changed(&mut self, length: usize) -> Option<&[u8]> {
        let bytes = &self.bytes[..length];
        if self.previous.as_deref() == Some(bytes) {
            return None;
        }
        self.previous = Some(bytes.to_vec());
        Some(bytes)
    }
}

#[derive(Default)]
struct Station {
    programme_service: Segments<8>,
    radio_text: Segments<64>,
    radio_text_mode: Option<u16>,
}

impl Station {
    fn push(&mut self, [_, control, c, d]: [u16; 4]) -> Option<(Field, &[u8])> {
        match control >> 12 {
            0 => self
                .programme_service(control, d)
                .map(|bytes| (Field::ProgrammeService, bytes)),
            2 => self
                .radio_text(control, c, d)
                .map(|bytes| (Field::RadioText, bytes)),
            _ => None,
        }
    }

    fn programme_service(&mut self, control: u16, d: u16) -> Option<&[u8]> {
        let segments = &mut self.programme_service;
        segments.insert(usize::from(control & 3), &d.to_be_bytes());
        (segments.prefix_length(2) == 8)
            .then(|| segments.emit_changed(8))
            .flatten()
    }

    fn radio_text(&mut self, control: u16, c: u16, d: u16) -> Option<&[u8]> {
        let mode = control & (VERSION_B | TEXT_AB);
        let segments = &mut self.radio_text;
        if self.radio_text_mode != Some(mode) {
            segments.received = 0;
            self.radio_text_mode = Some(mode);
        }
        let [first, second] = c.to_be_bytes();
        let [third, fourth] = d.to_be_bytes();
        let octets = [first, second, third, fourth];
        let segment = if mode & VERSION_B == 0 {
            &octets[..]
        } else {
            &octets[2..]
        };
        segments.insert(usize::from(control & 15), segment);
        let prefix = segments.prefix_length(segment.len());
        let end = segments.bytes[..prefix]
            .iter()
            .position(|&byte| byte == END_OF_TEXT)
            .or_else(|| (prefix == 16 * segment.len()).then_some(prefix))?;
        segments.emit_changed(end)
    }
}

fn basic_text(bytes: &[u8]) -> Option<String> {
    // NRSC-4-A Annex E, figure E.1 includes some non-ASCII positions below 0x80.
    bytes
        .iter()
        .map(|&byte| match byte {
            0x0a => Some('\n'),
            0x20..=0x23 | 0x25..=0x5d | 0x61..=0x7b | 0x7d => Some(char::from(byte)),
            _ => None,
        })
        .collect()
}

/// Assemble complete changed fields independently per programme identifier.
/// The original text octets are retained even outside the printable subset.
pub(super) fn decode(groups: &[super::Observation]) -> Vec<Observation> {
    let mut stations = BTreeMap::<u16, Station>::new();
    groups
        .iter()
        .filter_map(|group| {
            let programme_identification = group.blocks[0];
            let (field, bytes) = stations
                .entry(programme_identification)
                .or_default()
                .push(group.blocks)?;
            Some(Observation {
                programme_identification,
                field,
                decision_input_sample: group.decision_input_sample,
                bytes_hex: HEXLOWER.encode(bytes),
                text: basic_text(bytes),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_service_name_once_with_all_four_addresses() {
        let mut station = Station::default();
        for (address, text) in [(2, *b" F"), (0, *b"LA"), (1, *b"B ")] {
            assert!(
                station
                    .programme_service(address, u16::from_be_bytes(text))
                    .is_none()
            );
        }
        assert_eq!(
            station.programme_service(3, u16::from_be_bytes(*b"M ")),
            Some(b"LAB  FM ".as_slice())
        );
        assert!(
            station
                .programme_service(3, u16::from_be_bytes(*b"M "))
                .is_none()
        );
        assert!(
            station
                .programme_service(3, u16::from_be_bytes(*b"M2"))
                .is_none()
        );
    }

    #[test]
    fn radio_text_waits_for_contiguous_bytes_and_resets_on_mode_change() {
        let mut station = Station::default();
        let encode = u16::from_be_bytes;
        assert!(
            station
                .radio_text(0x2001, encode(*b"O\r"), encode(*b"ZZ"))
                .is_none()
        );
        assert_eq!(
            station.radio_text(0x2000, encode(*b"HE"), encode(*b"LL")),
            Some(b"HELLO".as_slice())
        );
        assert!(station.radio_text(0x2011, encode(*b"!\r"), 0).is_none());
        assert!(station.radio_text(0x2810, 0x4512, encode(*b"OK")).is_none());
        assert_eq!(
            station.radio_text(0x2811, 0x4512, encode(*b"\rS")),
            Some(b"OK".as_slice())
        );
    }

    #[test]
    fn preserves_text_bytes_per_station_and_omits_ambiguous_rendering() {
        let groups = [
            super::super::Observation {
                decision_input_sample: 1,
                blocks: [1, 0x2001, 0x0d00, 0],
            },
            super::super::Observation {
                decision_input_sample: 2,
                blocks: [2, 0x2000, 0x4f4b, 0x0d00],
            },
            super::super::Observation {
                decision_input_sample: 3,
                blocks: [1, 0x2000, 0x4849, 0x2480],
            },
        ];
        let fields = decode(&groups);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].programme_identification, 2);
        assert_eq!(fields[0].text.as_deref(), Some("OK"));
        assert_eq!(fields[1].programme_identification, 1);
        assert_eq!(fields[1].bytes_hex, "48492480");
        assert!(fields[1].text.is_none());
    }
}
