//! Interpret the LTE BCCH-BCH master information block after CRC validation.
//!
//! [TS 36.331 v18.5.0] §§6.2.1, 6.2.2, 6.3.2 and 8 define the unaligned PER
//! layout. [TS 36.211 v18.0.1] §6.6.1 provides the two implicit frame bits.
//!
//! [TS 36.331 v18.5.0]: https://www.etsi.org/deliver/etsi_ts/136300_136399/136331/18.05.00_60/ts_136331v180500p.pdf
//! [TS 36.211 v18.0.1]: https://www.etsi.org/deliver/etsi_ts/136200_136299/136211/18.00.01_60/ts_136211v180001p.pdf

use serde::Serialize;

const DOWNLINK_RESOURCE_BLOCKS: [u8; 6] = [6, 15, 25, 50, 75, 100];

/// Cell configuration and timing carried by one ordinary LTE MIB.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Mib {
    downlink_resource_blocks: u8,
    phich: Phich,
    system_frame_number: u16,
    scheduling_info_sib1_br: u8,
    system_info_unchanged_br: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    earfcn_lsb: Option<u8>,
}

impl Mib {
    /// Return the ten-bit frame number of this PBCH observation.
    pub(super) fn system_frame_number(&self) -> u16 {
        self.system_frame_number
    }

    /// Compare bandwidth and PHICH settings between two observations.
    ///
    /// Frame timing and system-information indications may change between
    /// observations of the same cell.
    pub(super) fn same_configuration(self, other: Self) -> bool {
        self.downlink_resource_blocks == other.downlink_resource_blocks && self.phich == other.phich
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct Phich {
    duration: PhichDuration,
    resource: PhichResource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PhichDuration {
    Normal,
    Extended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PhichResource {
    OneSixth,
    Half,
    One,
    Two,
}

/// Decode the 24 BCH information bits in transmitted order.
///
/// The caller verifies the CRC and supplies the frame's zero-based phase
/// within the 40 ms PBCH interval. Returns `None` for a nonbinary payload,
/// invalid phase, or unused three-bit downlink bandwidth encoding.
pub(super) fn decode(bits: [u8; 24], phase: usize) -> Option<Mib> {
    if phase >= 4 || bits.iter().any(|&bit| bit > 1) {
        return None;
    }

    let bandwidth_index = usize::from(read_bits(&bits[..3]));
    let downlink_resource_blocks = *DOWNLINK_RESOURCE_BLOCKS.get(bandwidth_index)?;
    let duration = if bits[3] == 0 {
        PhichDuration::Normal
    } else {
        PhichDuration::Extended
    };
    let resource = [
        PhichResource::OneSixth,
        PhichResource::Half,
        PhichResource::One,
        PhichResource::Two,
    ][usize::from(read_bits(&bits[4..6]))];
    let system_frame_number = (u16::from(read_bits(&bits[6..14])) << 2) | phase as u16;

    // Releases 13, 15 and 17 assigned these positions from the original
    // ten spare bits. Ignore the remaining spare bits in either CHOICE.
    Some(Mib {
        downlink_resource_blocks,
        phich: Phich { duration, resource },
        system_frame_number,
        scheduling_info_sib1_br: read_bits(&bits[14..19]),
        system_info_unchanged_br: bits[19] == 1,
        earfcn_lsb: (bits[20] == 1).then(|| read_bits(&bits[21..23])),
    })
}

fn read_bits(bits: &[u8]) -> u8 {
    bits.iter().fold(0, |value, &bit| (value << 1) | bit)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn bits(bytes: [u8; 3]) -> [u8; 24] {
        std::array::from_fn(|index| (bytes[index / 8] >> (7 - index % 8)) & 1)
    }

    #[test]
    fn decodes_release_18_fields() {
        // PER fields: 100 | 1 | 01 | 10101011 | 10010 | 1 | 1 | 10 | 0.
        let mib = decode(bits([0x96, 0xae, 0x5c]), 2).unwrap();
        assert_eq!(mib.system_frame_number(), 686);
        assert_eq!(
            serde_json::to_value(mib).unwrap(),
            json!({
                "downlink_resource_blocks": 75,
                "phich": {"duration": "extended", "resource": "half"},
                "system_frame_number": 686,
                "scheduling_info_sib1_br": 18,
                "system_info_unchanged_br": true,
                "earfcn_lsb": 2,
            })
        );
    }

    #[test]
    fn derives_every_frame_phase_and_accepts_legacy_spares() {
        // Legacy MIB with 50 RBs, normal PHICH, Ng = 1, and SFN high byte 255.
        let bytes = [0x6b, 0xfc, 0x00];
        let first = decode(bits(bytes), 0).unwrap();
        for phase in 0..4 {
            let frame = decode(bits(bytes), phase).unwrap();
            assert_eq!(usize::from(frame.system_frame_number()), 1020 + phase);
            assert!(first.same_configuration(frame));
        }
        assert_eq!(decode(bits([0x6b, 0xfc, 0x07]), 0), Some(first));
        let wrapped = decode(bits([0x68, 0, 0]), 0).unwrap();
        assert_eq!(wrapped.system_frame_number(), 0);
        assert!(first.same_configuration(wrapped));
        let different_phich = decode(bits([0x64, 0, 0]), 0).unwrap();
        assert!(!first.same_configuration(different_phich));
    }

    #[test]
    fn rejects_invalid_phase_bandwidth_and_bit_values() {
        assert!(decode([0; 24], 4).is_none());
        assert!(decode(bits([0xc0, 0, 0]), 0).is_none());
        assert!(decode(bits([0xe0, 0, 0]), 0).is_none());
        let mut invalid = [0; 24];
        invalid[8] = 2;
        assert!(decode(invalid, 0).is_none());
    }
}
