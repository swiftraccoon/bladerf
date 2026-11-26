//! FR1 BCH payload descrambling and master information block decoding.
//!
//! The bit order follows [TS 38.212 v17.8.0 §7.1][bch] and the unaligned PER
//! layout of `BCCH-BCH-Message` in [TS 38.331 v17.1.0 §6.2.2 and §8][rrc].
//! SSB indexing follows [TS 38.211 v17.8.0 §7.4.1.4.1][dmrs] and
//! [TS 38.213 v17.8.0 §4.1][ssb].
//!
//! [bch]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138212/17.08.00_60/ts_138212v170800p.pdf
//! [rrc]: https://www.etsi.org/deliver/etsi_ts/138300_138399/138331/17.01.00_60/ts_138331v170100p.pdf
//! [dmrs]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138211/17.08.00_60/ts_138211v170800p.pdf
//! [ssb]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138213/17.08.00_60/ts_138213v170800p.pdf

use serde::Serialize;

use crate::dsp::gold;

// TS 38.212 Table 7.1.1-1 arranged in incoming order of the 24 RRC bits and
// eight physical-layer bits. Entry i is the position of payload bit i after interleaving.
const INTERLEAVER: [usize; 32] = [
    1, 16, 23, 18, 17, 8, 30, 4, 9, 11, 12, 13, 14, 15, 19, 20, 21, 22, 25, 26, 27, 28, 29, 31, 10,
    6, 24, 7, 0, 5, 3, 2,
];
const HALF_FRAME_BIT: usize = 28;
const SFN_THIRD_LSB: usize = INTERLEAVER[25];
const SFN_SECOND_LSB: usize = INTERLEAVER[26];
const SCRAMBLING_EXCLUSIONS: [usize; 3] =
    [SFN_THIRD_LSB, SFN_SECOND_LSB, INTERLEAVER[HALF_FRAME_BIT]];

/// Decoded broadcast configuration and timing for one synchronization block.
///
/// `half_frame` is zero for the first half of a radio frame. `ssb_index` is
/// the candidate index within that half-frame. `k_ssb` is the complete FR1
/// subcarrier offset indicator, including the bit carried outside the MIB.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Mib {
    system_frame_number: u16,
    half_frame: u8,
    ssb_index: u8,
    common_subcarrier_spacing_hz: u32,
    k_ssb: u8,
    dmrs_type_a_position: u8,
    pdcch_config_sib1: PdcchConfigSib1,
    cell_barred: bool,
    intra_freq_reselection_allowed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct PdcchConfigSib1 {
    control_resource_set_zero: u8,
    search_space_zero: u8,
}

/// Recover the MIB from CRC-verified, still-interleaved BCH payload bits.
///
/// `dmrs_index` is the sequence index in 0–7. For `l_max == 4` it includes
/// the half-frame bit. Returns `None` for invalid bits or indices, an
/// unsupported candidate count or message choice, or inconsistent timing.
pub(super) fn decode(mut bits: [u8; 32], pci: u16, l_max: u8, dmrs_index: u8) -> Option<Mib> {
    if pci > 1007 || !matches!(l_max, 4 | 8) || dmrs_index >= 8 || bits.iter().any(|&bit| bit > 1) {
        return None;
    }

    let sequence_index = usize::from(2 * bits[SFN_THIRD_LSB] + bits[SFN_SECOND_LSB]);
    let scrambled_bit_count = bits.len() - SCRAMBLING_EXCLUSIONS.len();
    let sequence = gold(u32::from(pci)).skip(sequence_index * scrambled_bit_count);
    let scrambled = bits
        .iter_mut()
        .enumerate()
        .filter(|(index, _)| !SCRAMBLING_EXCLUSIONS.contains(index));
    for ((_, bit), mask) in scrambled.zip(sequence) {
        *bit ^= mask;
    }

    let payload = INTERLEAVER.map(|index| bits[index]);
    let half_frame = payload[HALF_FRAME_BIT];
    if payload[0] != 0 || (l_max == 4 && half_frame != dmrs_index / l_max) {
        return None;
    }

    // The MIB carries the six most significant SFN bits and four least
    // significant offset bits. The complementary bits come from the PHY.
    let system_frame_number =
        (u16::from(read_bits(&payload[1..7])) << 4) | u16::from(read_bits(&payload[24..28]));
    let k_ssb = (payload[29] << 4) | read_bits(&payload[8..12]);
    Some(Mib {
        system_frame_number,
        half_frame,
        ssb_index: dmrs_index % l_max,
        common_subcarrier_spacing_hz: 15_000 * (1 + u32::from(payload[7])),
        k_ssb,
        dmrs_type_a_position: 2 + payload[12],
        pdcch_config_sib1: PdcchConfigSib1 {
            control_resource_set_zero: read_bits(&payload[13..17]),
            search_space_zero: read_bits(&payload[17..21]),
        },
        cell_barred: payload[21] == 0,
        intra_freq_reselection_allowed: payload[22] == 0,
    })
}

fn read_bits(bits: &[u8]) -> u8 {
    bits.iter().fold(0, |value, &bit| (value << 1) | bit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(word: u32) -> [u8; 32] {
        std::array::from_fn(|index| ((word >> (31 - index)) & 1) as u8)
    }

    // A scalar forward encoder using the TS 38.212 interleaving groups and
    // TS 38.211 sequence recurrences generated these CRC-input words.
    #[test]
    fn decodes_four_candidate_second_half_frame() {
        assert_eq!(
            decode(bits(0xe909_f98b), 321, 4, 7),
            Some(Mib {
                system_frame_number: 675,
                half_frame: 1,
                ssb_index: 3,
                common_subcarrier_spacing_hz: 30_000,
                k_ssb: 11,
                dmrs_type_a_position: 2,
                pdcch_config_sib1: PdcchConfigSib1 {
                    control_resource_set_zero: 5,
                    search_space_zero: 11,
                },
                cell_barred: false,
                intra_freq_reselection_allowed: false,
            })
        );
        assert_eq!(decode(bits(0xe909_f98b), 321, 4, 3), None);
    }

    #[test]
    fn decodes_eight_candidate_first_half_frame() {
        assert_eq!(
            decode(bits(0x335d_1607), 1007, 8, 6),
            Some(Mib {
                system_frame_number: 1013,
                half_frame: 0,
                ssb_index: 6,
                common_subcarrier_spacing_hz: 30_000,
                k_ssb: 29,
                dmrs_type_a_position: 3,
                pdcch_config_sib1: PdcchConfigSib1 {
                    control_resource_set_zero: 6,
                    search_space_zero: 1,
                },
                cell_barred: true,
                intra_freq_reselection_allowed: true,
            })
        );
    }

    #[test]
    fn uses_the_first_and_last_scrambling_offsets() {
        let first = decode(bits(0x0086_844f), 0, 4, 0).unwrap();
        assert_eq!(first.system_frame_number, 0);
        assert_eq!(first.common_subcarrier_spacing_hz, 15_000);
        let last = decode(bits(0xd7b1_5fed), 500, 8, 2).unwrap();
        assert_eq!(last.system_frame_number, 118);
        assert_eq!(last.half_frame, 1);
        assert_eq!(last.ssb_index, 2);
        assert_eq!(last.k_ssb, 9);
    }

    #[test]
    fn rejects_an_extension_or_invalid_inputs() {
        assert_eq!(decode(bits(0x97b1_5fed), 500, 8, 2), None);
        assert_eq!(decode(bits(0x0086_844f), 1008, 4, 0), None);
        assert_eq!(decode(bits(0x0086_844f), 0, 64, 0), None);
        assert_eq!(decode(bits(0x0086_844f), 0, 4, 8), None);
        assert_eq!(decode([2; 32], 0, 4, 0), None);
    }
}
