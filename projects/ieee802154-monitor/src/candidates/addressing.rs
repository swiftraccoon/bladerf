//! Address fields of ordinary IEEE 802.15.4 MAC frames.
//!
//! IEEE 802.15.4-2006 §7.2.1.1.5 specifies legacy PAN ID compression.
//! IEEE 802.15.4-2015 §7.2.1.5 and Table 7-2 specify the version 2 cases.
//! [OpenThread's PAN presence checks] provide a versioned implementation reference.
//!
//! [OpenThread's PAN presence checks]: https://fuchsia.googlesource.com/third_party/openthread/+/436aa6980d5ac1f484f091aa956a9d30ec97ffc1/src/core/mac/mac_frame.cpp

use serde::{Serialize, Serializer};

/// Addressing fields physically carried by one frame, or its parsing outcome.
#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Observation {
    Observed {
        #[serde(skip_serializing_if = "Option::is_none")]
        destination_pan_id: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        destination_address: Option<Address>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_pan_id: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        source_address: Option<Address>,
    },
    UnsupportedFrameType,
    ReservedFrameVersion,
    ReservedAddressMode,
    InvalidLegacyAddressing,
    InvalidLegacyAckAddressing,
    InvalidPanIdCompression,
    TruncatedSequenceNumber,
    TruncatedAddressingFields,
}

/// A literal short or extended address from the MAC header.
#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub(super) enum Address {
    Short(u16),
    Extended(#[serde(serialize_with = "serialize_extended")] u64),
}

fn serialize_extended<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("{value:016x}"))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Absent,
    Short,
    Extended,
}

impl Mode {
    fn from_bits(bits: u16) -> Option<Self> {
        match bits {
            0 => Some(Self::Absent),
            2 => Some(Self::Short),
            3 => Some(Self::Extended),
            _ => None,
        }
    }

    fn read(self, fields: &mut &[u8]) -> Option<Option<Address>> {
        match self {
            Self::Absent => Some(None),
            Self::Short => Some(Some(Address::Short(u16::from_le_bytes(take(fields)?)))),
            Self::Extended => Some(Some(Address::Extended(u64::from_le_bytes(take(fields)?)))),
        }
    }
}

fn take<const N: usize>(fields: &mut &[u8]) -> Option<[u8; N]> {
    let (field, remaining) = fields.split_first_chunk()?;
    *fields = remaining;
    Some(*field)
}

fn endpoint(
    fields: &mut &[u8],
    pan_present: bool,
    mode: Mode,
) -> Option<(Option<u16>, Option<Address>)> {
    let pan_id = if pan_present {
        Some(u16::from_le_bytes(take(fields)?))
    } else {
        None
    };
    Some((pan_id, mode.read(fields)?))
}

fn modern_pan_presence(destination: Mode, source: Mode, compressed: bool) -> (bool, bool) {
    match (destination, source) {
        (Mode::Absent, Mode::Absent) => (compressed, false),
        (Mode::Absent, _) => (false, !compressed),
        (_, Mode::Absent) | (Mode::Extended, Mode::Extended) => (!compressed, false),
        _ => (true, !compressed),
    }
}

/// Observe addressing fields after the caller validates the FCS.
///
/// `fields` follows the two Frame Control bytes and excludes the FCS.
/// `control` is the decoded Frame Control value. A complete observation
/// describes the layout through the last address field.
pub(super) fn observe(mut fields: &[u8], control: u16) -> Observation {
    let frame_type = control & 0x7;
    if frame_type > 3 {
        return Observation::UnsupportedFrameType;
    }
    let version = (control >> 12) & 0x3;
    if version == 3 {
        return Observation::ReservedFrameVersion;
    }
    let (Some(destination), Some(source)) = (
        Mode::from_bits((control >> 10) & 0x3),
        Mode::from_bits((control >> 14) & 0x3),
    ) else {
        return Observation::ReservedAddressMode;
    };

    let compressed = control & (1 << 6) != 0;
    let modern = version == 2;
    let (destination_pan, source_pan) = if modern {
        modern_pan_presence(destination, source, compressed)
    } else {
        let destination_present = destination != Mode::Absent;
        let source_present = source != Mode::Absent;
        if compressed && !(destination_present && source_present) {
            return Observation::InvalidPanIdCompression;
        }
        // Legacy Beacon and Immediate ACK layouts are fixed by frame type.
        match frame_type {
            0 if destination_present || !source_present => {
                return Observation::InvalidLegacyAddressing;
            }
            2 if destination_present || source_present => {
                return Observation::InvalidLegacyAckAddressing;
            }
            1 | 3 if !destination_present && !source_present => {
                return Observation::InvalidLegacyAddressing;
            }
            _ => {}
        }
        (destination_present, source_present && !compressed)
    };

    // Bit 8 is reserved in legacy frames, where the sequence octet is present.
    let sequence_suppressed = modern && control & (1 << 8) != 0;
    if !sequence_suppressed && take::<1>(&mut fields).is_none() {
        return Observation::TruncatedSequenceNumber;
    }
    let Some((destination_pan_id, destination_address)) =
        endpoint(&mut fields, destination_pan, destination)
    else {
        return Observation::TruncatedAddressingFields;
    };
    let Some((source_pan_id, source_address)) = endpoint(&mut fields, source_pan, source) else {
        return Observation::TruncatedAddressingFields;
    };
    Observation::Observed {
        destination_pan_id,
        destination_address,
        source_pan_id,
        source_address,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DESTINATION_PAN: u16 = 0x5678;
    const SOURCE_PAN: u16 = 0xabcd;
    const DESTINATION_EXTENDED: u64 = 0xfedc_ba98_7654_3210;
    const SOURCE_EXTENDED: u64 = 0x0012_3456_789a_bcde;

    fn check_layout(control: u16, sequence_present: bool, destination_pan: bool, source_pan: bool) {
        let mut fields = Vec::new();
        if sequence_present {
            fields.push(0x53);
        }
        let mut append = |has_pan: bool, pan: u16, mode, short: u16, extended: u64| {
            let pan_id = has_pan.then_some(pan);
            if let Some(pan_id) = pan_id {
                fields.extend_from_slice(&pan_id.to_le_bytes());
            }
            let address = match mode {
                0 => None,
                2 => {
                    fields.extend_from_slice(&short.to_le_bytes());
                    Some(Address::Short(short))
                }
                3 => {
                    fields.extend_from_slice(&extended.to_le_bytes());
                    Some(Address::Extended(extended))
                }
                _ => unreachable!("test uses only declared address modes"),
            };
            (pan_id, address)
        };
        let (destination_pan_id, destination_address) = append(
            destination_pan,
            DESTINATION_PAN,
            (control >> 10) & 3,
            0xab12,
            DESTINATION_EXTENDED,
        );
        let (source_pan_id, source_address) = append(
            source_pan,
            SOURCE_PAN,
            (control >> 14) & 3,
            0xcd34,
            SOURCE_EXTENDED,
        );
        fields.extend_from_slice(&[0x79, 0x03]);
        assert_eq!(
            observe(&fields, control),
            Observation::Observed {
                destination_pan_id,
                destination_address,
                source_pan_id,
                source_address,
            },
            "FCF {control:04x}"
        );
    }

    #[test]
    fn observes_every_modern_pan_layout_with_or_without_a_sequence_number() {
        for (destination, source, compression, destination_pan, source_pan) in [
            (0, 0, 0, false, false),
            (0, 0, 1, true, false),
            (2, 0, 0, true, false),
            (2, 0, 1, false, false),
            (3, 0, 0, true, false),
            (3, 0, 1, false, false),
            (0, 2, 0, false, true),
            (0, 2, 1, false, false),
            (0, 3, 0, false, true),
            (0, 3, 1, false, false),
            (3, 3, 0, true, false),
            (3, 3, 1, false, false),
            (2, 2, 0, true, true),
            (2, 2, 1, true, false),
            (2, 3, 0, true, true),
            (2, 3, 1, true, false),
            (3, 2, 0, true, true),
            (3, 2, 1, true, false),
        ] {
            // Enhanced ACK exercises the ordinary layout with an optional sequence.
            let control = 0x2002 | destination << 10 | source << 14 | compression << 6;
            check_layout(control, true, destination_pan, source_pan);
            check_layout(control | 0x0100, false, destination_pan, source_pan);
        }
    }

    #[test]
    fn observes_legacy_layouts_with_reserved_bits_ignored_for_offsets() {
        for version in [0, 0x1000] {
            check_layout(version | 0x0382, true, false, false);
            check_layout(version | 0x8380, true, false, true);
            check_layout(version | 0x0b81, true, true, false);
            for destination in 2..=3 {
                for source in 2..=3 {
                    let control = version | destination << 10 | source << 14 | 0x0383;
                    check_layout(control, true, true, true);
                    check_layout(control | 0x0040, true, true, false);
                }
            }
        }
    }

    #[test]
    fn rejects_reserved_and_invalid_address_layouts() {
        for (control, expected) in [
            (0x2005u16, Observation::UnsupportedFrameType),
            (0x3001, Observation::ReservedFrameVersion),
            (0x2401, Observation::ReservedAddressMode),
            (0x6001, Observation::ReservedAddressMode),
            (0x1042, Observation::InvalidPanIdCompression),
            (0x0841, Observation::InvalidPanIdCompression),
            (0x9041, Observation::InvalidPanIdCompression),
            (0x0001, Observation::InvalidLegacyAddressing),
            (0x1003, Observation::InvalidLegacyAddressing),
            (0x0800, Observation::InvalidLegacyAddressing),
            (0x8800, Observation::InvalidLegacyAddressing),
            (0x0802, Observation::InvalidLegacyAckAddressing),
            (0x9002, Observation::InvalidLegacyAckAddressing),
        ] {
            assert_eq!(observe(&[0xaa; 30], control), expected, "FCF {control:04x}");
        }
    }

    #[test]
    fn withholds_values_from_any_truncated_address_field() {
        let mut fields = vec![0x53];
        fields.extend_from_slice(&DESTINATION_PAN.to_le_bytes());
        fields.extend_from_slice(&DESTINATION_EXTENDED.to_le_bytes());
        fields.extend_from_slice(&SOURCE_PAN.to_le_bytes());
        fields.extend_from_slice(&SOURCE_EXTENDED.to_le_bytes());
        assert_eq!(observe(&[], 0xcc01), Observation::TruncatedSequenceNumber);
        for end in 1..fields.len() {
            assert_eq!(
                observe(&fields[..end], 0xcc01),
                Observation::TruncatedAddressingFields,
                "length {end}"
            );
        }
        assert_eq!(observe(&[], 0xe901), Observation::TruncatedAddressingFields);
    }

    #[test]
    fn serializes_only_transmitted_fields_with_exact_extended_address_width() {
        let mut fields = vec![0x53];
        fields.extend_from_slice(&DESTINATION_PAN.to_le_bytes());
        fields.extend_from_slice(&0xffffu16.to_le_bytes());
        fields.extend_from_slice(&SOURCE_EXTENDED.to_le_bytes());
        assert_eq!(
            serde_json::to_value(observe(&fields, 0xd841)).unwrap(),
            json!({
                "status": "observed",
                "destination_pan_id": 0x5678,
                "destination_address": { "mode": "short", "value": 65535 },
                "source_address": { "mode": "extended", "value": "00123456789abcde" }
            })
        );
    }
}
