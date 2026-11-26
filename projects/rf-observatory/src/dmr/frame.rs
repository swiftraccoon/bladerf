//! DMR voice Link Control headers and terminators from general data bursts.
//!
//! Burst layout and coding follow [ETSI TS 102 361-1 V2.7.1, clause 9 and
//! annexes B and E][air-interface]. Standard address fields follow [ETSI
//! TS 102 361-2 V2.3.1, clauses 7.1.1.1 and 7.1.1.2][voice-services].
//!
//! [air-interface]: https://www.etsi.org/deliver/etsi_ts/102300_102399/10236101/02.07.01_60/ts_10236101v020701p.pdf
//! [voice-services]: https://www.etsi.org/deliver/etsi_ts/102300_102399/10236102/02.03.01_60/ts_10236102v020301p.pdf#page=71

use std::sync::OnceLock;

use serde::Serialize;

/// Bits in the general data burst interior, including sync and slot type.
pub(super) const BURST_BITS: usize = 264;
/// Number of bits preceding the data sync pattern.
pub(super) const SYNC_START_BIT: usize = 108;
/// Wire bits in a data sync pattern.
pub(super) const SYNC_BITS: usize = 48;
const SYNC_END_BIT: usize = SYNC_START_BIT + SYNC_BITS;
const BPTC_BITS: usize = 196;
const BPTC_HALF_BITS: usize = BPTC_BITS / 2;
const SLOT_TYPE_HALF_BITS: u32 = 10;
const BPTC_RESERVED_BITS: usize = 4;
const BPTC_DATA_ROWS: usize = 9;
const BPTC_COLUMNS: usize = 15;
const BPTC_DATA_COLUMNS: usize = 11;

/// Origin or direct-mode slot conveyed by a general data sync word.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Sync {
    BaseStation,
    MobileStation,
    DirectSlot1,
    DirectSlot2,
}

/// Data sync patterns in over-the-air bit order, most significant bit first.
pub(super) const DATA_SYNC_WORDS: [(Sync, u64); 4] = [
    (Sync::BaseStation, 0xdff5_7d75_df5d),
    (Sync::MobileStation, 0xd5d7_f77f_d757),
    (Sync::DirectSlot1, 0xf7fd_d5dd_fd55),
    (Sync::DirectSlot2, 0xd755_7f5f_f7f5),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BurstKind {
    Header,
    Terminator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DestinationKind {
    Group,
    Individual,
}

/// Standard, unprotected voice Link Control addresses and coding evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct VoiceLinkControl {
    sync: Sync,
    burst_kind: BurstKind,
    destination_kind: DestinationKind,
    source_id: u32,
    destination_id: u32,
    color_code: u8,
    corrected_slot_type_bits: u8,
    corrected_bptc_bits: u8,
}

/// Decode a burst with the sync classification supplied by the symbol detector.
///
/// Each array element holds one binary wire bit. The Golay and product codes
/// correct errors within their respective three- and four-bit bounds. The
/// unmasked Reed–Solomon check must then pass without further correction.
pub(super) fn decode(bits: &[u8; BURST_BITS], sync: Sync) -> Option<VoiceLinkControl> {
    if bits.iter().any(|&bit| bit > 1) {
        return None;
    }
    let slot_word = (bit_value(&bits[BPTC_HALF_BITS..SYNC_START_BIT]) << SLOT_TYPE_HALF_BITS)
        | bit_value(&bits[SYNC_END_BIT..BURST_BITS - BPTC_HALF_BITS]);
    let (slot_type, corrected_slot_type_bits) = decode_golay(slot_word)?;
    let (burst_kind, rs_mask) = match slot_type & 0x0f {
        1 => (BurstKind::Header, 0x96),
        2 => (BurstKind::Terminator, 0x99),
        _ => return None,
    };
    let (lc, corrected_bptc_bits) = decode_bptc(bits)?;
    if lc[1] != 0 || !check_rs(lc, rs_mask) {
        return None;
    }
    // Exact first-octet matches also enforce a clear Protect Flag and reserved bit.
    let destination_kind = match lc[0] {
        0 => DestinationKind::Group,
        3 => DestinationKind::Individual,
        _ => return None,
    };
    Some(VoiceLinkControl {
        sync,
        burst_kind,
        destination_kind,
        source_id: u32::from_be_bytes([0, lc[6], lc[7], lc[8]]),
        destination_id: u32::from_be_bytes([0, lc[3], lc[4], lc[5]]),
        color_code: slot_type >> 4,
        corrected_slot_type_bits,
        corrected_bptc_bits,
    })
}

fn bit_value(bits: &[u8]) -> u32 {
    bits.iter()
        .fold(0, |value, &bit| (value << 1) | u32::from(bit))
}

// Generate the shortened Golay (20,8) codewords using polynomial B.4 and even parity.
const fn golay_words() -> [u32; 256] {
    let mut words = [0; 256];
    let mut index = 0;
    while index < words.len() {
        let data = (index as u32) << 11;
        let mut remainder = data;
        let mut bit = 19;
        while bit > 11 {
            bit -= 1;
            if remainder & (1 << bit) != 0 {
                remainder ^= 0x0c75 << (bit - 11);
            }
        }
        let word = data | remainder;
        words[index] = (word << 1) | (word.count_ones() & 1);
        index += 1;
    }
    words
}

fn decode_golay(word: u32) -> Option<(u8, u8)> {
    const WORDS: [u32; 256] = golay_words();
    // The extended code has minimum distance eight, making each match unique.
    WORDS.iter().enumerate().find_map(|(value, &candidate)| {
        let distance = (word ^ candidate).count_ones();
        (distance <= 3).then_some((value as u8, distance as u8))
    })
}

// A bit contributes the parity row in tables B.14 and B.15. Parity bits
// contribute their own position in the four-bit check.
const ROW_CHECK: [u128; 15] = [9, 13, 15, 14, 7, 10, 5, 11, 12, 6, 3, 8, 4, 2, 1];
const COLUMN_CHECK: [u128; 13] = [15, 14, 7, 10, 5, 11, 12, 6, 3, 8, 4, 2, 1];
const BPTC_SYNDROMES: [u128; BPTC_BITS] = product_syndromes();

const fn product_syndromes() -> [u128; BPTC_BITS] {
    // Assign four bits to the reserved positions, then four per row and column.
    let mut syndromes = [0; BPTC_BITS];
    syndromes[0] = 1;
    let mut position = 1;
    while position < BPTC_BITS {
        let row = (position - 1) / BPTC_COLUMNS;
        let column = (position - 1) % BPTC_COLUMNS;
        let mut checks = COLUMN_CHECK[row] << (BPTC_RESERVED_BITS + (BPTC_DATA_ROWS + column) * 4);
        if row < BPTC_DATA_ROWS {
            checks |= ROW_CHECK[column] << (BPTC_RESERVED_BITS + row * 4);
        }
        if position < BPTC_RESERVED_BITS {
            checks |= 1 << position;
        }
        syndromes[position] = checks;
        position += 1;
    }
    syndromes
}

fn decode_bptc(bits: &[u8; BURST_BITS]) -> Option<([u8; 12], u8)> {
    let mut matrix: [u8; BPTC_BITS] = std::array::from_fn(|index| {
        let wire = index * 181 % BPTC_BITS;
        bits[if wire < BPTC_HALF_BITS {
            wire
        } else {
            wire + BURST_BITS - BPTC_BITS
        }]
    });
    let syndrome = matrix
        .iter()
        .zip(&BPTC_SYNDROMES)
        .filter(|&(&bit, _)| bit != 0)
        .fold(0, |sum, (_, &check)| sum ^ check);
    let corrected = correct_bptc(&mut matrix, syndrome)?;
    let payload = std::array::from_fn(|byte| {
        (0..8).fold(0, |value, bit| {
            // Three leading reserved cells share the first row with data.
            let data_cell = byte * 8 + bit + (BPTC_RESERVED_BITS - 1);
            let cell = data_cell / BPTC_DATA_COLUMNS * BPTC_COLUMNS + data_cell % BPTC_DATA_COLUMNS;
            (value << 1) | matrix[cell + 1]
        })
    });
    Some((payload, corrected))
}

struct BitPair {
    syndrome: u128,
    positions: [u8; 2],
}

fn pair_syndromes() -> &'static [BitPair] {
    static PAIRS: OnceLock<Vec<BitPair>> = OnceLock::new();
    PAIRS.get_or_init(|| {
        let mut pairs = Vec::with_capacity(BPTC_BITS * (BPTC_BITS - 1) / 2);
        for (first, &left) in BPTC_SYNDROMES.iter().enumerate() {
            for (second, &right) in BPTC_SYNDROMES.iter().enumerate().skip(first + 1) {
                pairs.push(BitPair {
                    syndrome: left ^ right,
                    positions: [first as u8, second as u8],
                });
            }
        }
        pairs.sort_unstable_by_key(|pair| pair.syndrome);
        pairs
    })
}

fn correct_bptc(matrix: &mut [u8; BPTC_BITS], syndrome: u128) -> Option<u8> {
    if syndrome == 0 {
        return Some(0);
    }
    let mut flip = |positions: &[u8]| {
        for &position in positions {
            matrix[usize::from(position)] ^= 1;
        }
        Some(positions.len() as u8)
    };
    if let Some(position) = BPTC_SYNDROMES.iter().position(|&check| check == syndrome) {
        return flip(&[position as u8]);
    }

    // The product code has minimum distance nine. At most four errors have
    // a unique syndrome. Search singles and pairs, including their disjoint sums.
    let pairs = pair_syndromes();
    let find_pair = |target| {
        let index = pairs
            .binary_search_by_key(&target, |pair| pair.syndrome)
            .ok()?;
        Some(pairs[index].positions)
    };
    if let Some(positions) = find_pair(syndrome) {
        return flip(&positions);
    }
    for (position, &check) in BPTC_SYNDROMES.iter().enumerate() {
        let position = position as u8;
        if let Some([first, second]) = find_pair(syndrome ^ check)
            && position < first
        {
            return flip(&[position, first, second]);
        }
    }
    for pair in pairs {
        if let Some([first, second]) = find_pair(syndrome ^ pair.syndrome)
            && pair.positions[1] < first
        {
            return flip(&[pair.positions[0], pair.positions[1], first, second]);
        }
    }
    None
}

fn check_rs(mut symbols: [u8; 12], mask: u8) -> bool {
    for parity in &mut symbols[9..] {
        *parity ^= mask;
    }
    // Annex B.3.6 roots α, α², α³ in GF(256), with primitive polynomial 0x11d.
    (1..=3).all(|power| {
        symbols.iter().fold(0, |mut value, &symbol| {
            for _ in 0..power {
                value = (value << 1) ^ ((value >> 7) * 0x1d);
            }
            value ^ symbol
        }) == 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // MMDVM calibration array VH_1K, without its leading serial control octet.
    // https://github.com/g4klx/MMDVM/blob/1a3b2ec4c4401d699cf7743400ca2c63f67b7289/CalDMR.cpp#L27-L31
    const CALIBRATION_HEADER: [u8; 33] = [
        0x00, 0x20, 0x08, 0x08, 0x02, 0x38, 0x15, 0x00, 0x2c, 0xa0, 0x14, 0x60, 0x84, 0x6d, 0xff,
        0x57, 0xd7, 0x5d, 0xf5, 0xde, 0x30, 0x30, 0x01, 0x10, 0x01, 0x40, 0x03, 0xc0, 0x13, 0xc1,
        0x1e, 0x80, 0x6f,
    ];
    // Independently encoded private-call terminator with LC 030000123456654321.
    const INDIVIDUAL_TERMINATOR: [u8; 33] = [
        0x55, 0x53, 0x14, 0xc9, 0x36, 0x3e, 0x14, 0x88, 0x54, 0x10, 0xb4, 0xc3, 0x1c, 0x8f, 0x7f,
        0xdd, 0x5d, 0xdf, 0xd5, 0x54, 0x35, 0xc4, 0x22, 0x30, 0x61, 0x41, 0x4f, 0x02, 0x67, 0x10,
        0x1f, 0x07, 0x1b,
    ];
    // Valid Golay and BPTC words enclosing two changed RS parity octets.
    const WRONG_RS_PARITY: [u8; 33] = [
        0x00, 0x69, 0x08, 0x02, 0x02, 0x34, 0x15, 0x08, 0x2c, 0x80, 0x14, 0xe0, 0x84, 0x6d, 0xff,
        0x57, 0xd7, 0x5d, 0xf5, 0xde, 0x31, 0x30, 0x00, 0x30, 0x00, 0x40, 0x01, 0xc0, 0x17, 0xc1,
        0x07, 0x80, 0x6e,
    ];
    // Independently encoded headers with FID 0x10 and Protect Flag set, respectively.
    const NONSTANDARD_FEATURE_HEADER: [u8; 33] = [
        0x00, 0xa3, 0x08, 0x0c, 0x03, 0x1c, 0x13, 0x50, 0x2d, 0x20, 0x15, 0x20, 0x84, 0x6d, 0xff,
        0x57, 0xd7, 0x5d, 0xf5, 0xde, 0x30, 0x81, 0x00, 0x20, 0x02, 0xa0, 0x0e, 0x44, 0x0c, 0xc9,
        0x3e, 0x10, 0x2e,
    ];
    const PROTECTED_HEADER: [u8; 33] = [
        0x40, 0x80, 0x89, 0xd8, 0x03, 0xbc, 0x16, 0x30, 0x23, 0x90, 0x11, 0xc0, 0x84, 0x6d, 0xff,
        0x57, 0xd7, 0x5d, 0xf5, 0xde, 0x30, 0x34, 0x03, 0x38, 0x05, 0x18, 0x00, 0xe0, 0x12, 0x01,
        0x21, 0x20, 0x7f,
    ];

    fn wire_bits(burst: [u8; 33]) -> [u8; BURST_BITS] {
        std::array::from_fn(|bit| (burst[bit / 8] >> (7 - bit % 8)) & 1)
    }

    #[test]
    fn identifies_the_pinned_mmdvm_voice_header() {
        assert_eq!(
            decode(&wire_bits(CALIBRATION_HEADER), Sync::BaseStation),
            Some(VoiceLinkControl {
                sync: Sync::BaseStation,
                burst_kind: BurstKind::Header,
                destination_kind: DestinationKind::Group,
                source_id: 1,
                destination_id: 9,
                color_code: 1,
                corrected_slot_type_bits: 0,
                corrected_bptc_bits: 0,
            })
        );
    }

    #[test]
    fn identifies_an_individual_terminator_with_its_distinct_mask() {
        assert_eq!(
            decode(&wire_bits(INDIVIDUAL_TERMINATOR), Sync::DirectSlot1),
            Some(VoiceLinkControl {
                sync: Sync::DirectSlot1,
                burst_kind: BurstKind::Terminator,
                destination_kind: DestinationKind::Individual,
                source_id: 0x65_4321,
                destination_id: 0x12_3456,
                color_code: 7,
                corrected_slot_type_bits: 0,
                corrected_bptc_bits: 0,
            })
        );
    }

    #[test]
    fn repairs_bounded_golay_and_product_code_errors() {
        let mut bits = wire_bits(CALIBRATION_HEADER);
        for (index, bptc_bit) in [41, 167, 256, 0].into_iter().enumerate() {
            if let Some(&slot_type_bit) = [98, 107, 164].get(index) {
                bits[slot_type_bit] ^= 1;
            }
            bits[bptc_bit] ^= 1;
            let header = decode(&bits, Sync::BaseStation).unwrap();
            assert_eq!(
                (header.source_id, header.destination_id, header.color_code),
                (1, 9, 1)
            );
            assert_eq!(header.corrected_slot_type_bits, (index + 1).min(3) as u8);
            assert_eq!(header.corrected_bptc_bits, (index + 1) as u8);
        }
    }

    #[test]
    fn rejects_wrong_reed_solomon_parity_and_uninterpreted_addresses() {
        let bits = wire_bits(WRONG_RS_PARITY);
        assert_eq!(decode_bptc(&bits).unwrap().1, 0);
        assert_eq!(decode(&bits, Sync::BaseStation), None);
        for burst in [NONSTANDARD_FEATURE_HEADER, PROTECTED_HEADER] {
            let bits = wire_bits(burst);
            let (payload, corrected) = decode_bptc(&bits).unwrap();
            assert!(check_rs(payload, 0x96));
            assert_eq!(corrected, 0);
            assert_eq!(decode(&bits, Sync::BaseStation), None);
        }
    }
}
