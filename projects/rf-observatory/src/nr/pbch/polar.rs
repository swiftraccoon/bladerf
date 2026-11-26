//! BCH channel decoding for the fixed NR PBCH format.
//!
//! [TS 38.212 v17.8.0] §§5.1, 5.3.1, 5.4.1 and 7.1 specify the CRC,
//! polar code and rate matching.
//!
//! [TS 38.212 v17.8.0]: https://www.etsi.org/deliver/etsi_ts/138200_138299/138212/17.08.00_60/ts_138212v170800p.pdf

const PAYLOAD_BITS: usize = 32;
const CRC_BITS: usize = 24;
const INFORMATION_BITS: usize = PAYLOAD_BITS + CRC_BITS;
const POLAR_LENGTH: usize = 512;
const RATE_MATCHED_LENGTH: usize = 864;
const CRC24C_POLYNOMIAL: u32 = 0x01b2_b117;

// The 56 most reliable positions below 512 in Table 5.3.1.2-1, in index order.
const INFORMATION_POSITIONS: [usize; INFORMATION_BITS] = [
    247, 253, 254, 255, 367, 375, 379, 381, 382, 383, 415, 431, 439, 441, 443, 444, 445, 446, 447,
    463, 469, 470, 471, 473, 474, 475, 476, 477, 478, 479, 483, 485, 486, 487, 489, 490, 491, 492,
    493, 494, 495, 497, 498, 499, 500, 501, 502, 503, 504, 505, 506, 507, 508, 509, 510, 511,
];

// Table 5.3.1.1-1 restricted to entries >= 108, then shifted down by 108.
const INPUT_INTERLEAVER: [usize; INFORMATION_BITS] = [
    0, 2, 3, 5, 7, 10, 11, 12, 14, 15, 18, 19, 21, 24, 26, 30, 31, 32, 1, 4, 6, 8, 13, 16, 20, 22,
    25, 27, 33, 9, 17, 23, 28, 34, 29, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49,
    50, 51, 52, 53, 54, 55,
];

const SUBBLOCK_INTERLEAVER: [usize; 32] = [
    0, 1, 2, 4, 3, 5, 6, 7, 8, 16, 9, 17, 10, 18, 11, 19, 12, 20, 13, 21, 14, 22, 15, 23, 24, 25,
    26, 28, 27, 29, 30, 31,
];
const SUBBLOCK_LENGTH: usize = POLAR_LENGTH / SUBBLOCK_INTERLEAVER.len();

const INFORMATION_MASK: [bool; POLAR_LENGTH] = {
    let mut mask = [false; POLAR_LENGTH];
    let mut index = 0;
    while index < INFORMATION_BITS {
        mask[INFORMATION_POSITIONS[index]] = true;
        index += 1;
    }
    mask
};

/// Recover the scrambled BCH payload if soft polar decoding passes CRC24C.
///
/// The input is in PBCH bit order after physical-layer descrambling. Positive
/// likelihood ratios favor zero. Non-finite values and a fully erased codeword
/// yield `None`. The returned bits retain the BCH payload scrambling and order
/// used for CRC attachment.
pub(super) fn decode(llrs: &[f32; RATE_MATCHED_LENGTH]) -> Option<[u8; PAYLOAD_BITS]> {
    // Accumulate repetitions into their original polar subblocks. Widen to
    // keep every finite input representable when combining soft evidence.
    let mut mother = [0.0; POLAR_LENGTH];
    for (position, &llr) in llrs.iter().enumerate() {
        if !llr.is_finite() {
            return None;
        }
        let circular = position % POLAR_LENGTH;
        let subblock = SUBBLOCK_INTERLEAVER[circular / SUBBLOCK_LENGTH];
        let original = subblock * SUBBLOCK_LENGTH + circular % SUBBLOCK_LENGTH;
        mother[original] += f64::from(llr);
    }
    if mother.iter().all(|&llr| llr == 0.0) {
        return None;
    }

    let mut decisions = [0; POLAR_LENGTH];
    decode_node(
        &mother,
        &mut [0.0; POLAR_LENGTH - 1],
        0,
        &mut [0; POLAR_LENGTH],
        &mut decisions,
    );

    let mut attached = [0; INFORMATION_BITS];
    for (&position, output) in INFORMATION_POSITIONS.iter().zip(INPUT_INTERLEAVER) {
        attached[output] = decisions[position];
    }
    crc24c_valid(&attached).then(|| std::array::from_fn(|index| attached[index]))
}

fn decode_node(
    llrs: &[f64],
    scratch: &mut [f64],
    start: usize,
    partial_sums: &mut [u8],
    decisions: &mut [u8; POLAR_LENGTH],
) {
    if llrs.len() == 1 {
        let bit = u8::from(INFORMATION_MASK[start] && llrs[0] < 0.0);
        decisions[start] = bit;
        partial_sums[0] = bit;
        return;
    }

    let half = llrs.len() / 2;
    let (left, right) = llrs.split_at(half);
    let (child, child_scratch) = scratch.split_at_mut(half);
    let (left_partial, right_partial) = partial_sums.split_at_mut(half);

    // Min-sum approximates the odds of the XOR. The right child then uses
    // the left child's decoded partial sums to combine its two observations.
    for (index, value) in child.iter_mut().enumerate() {
        let magnitude = left[index].abs().min(right[index].abs());
        *value = if left[index].is_sign_negative() == right[index].is_sign_negative() {
            magnitude
        } else {
            -magnitude
        };
    }
    decode_node(child, child_scratch, start, left_partial, decisions);

    for (index, value) in child.iter_mut().enumerate() {
        let signed_left = if left_partial[index] == 0 {
            left[index]
        } else {
            -left[index]
        };
        *value = right[index] + signed_left;
    }
    decode_node(child, child_scratch, start + half, right_partial, decisions);

    for (left_bit, &right_bit) in left_partial.iter_mut().zip(right_partial.iter()) {
        *left_bit ^= right_bit;
    }
}

fn crc24c_valid(bits: &[u8; INFORMATION_BITS]) -> bool {
    let mut remainder = 0;
    for &bit in bits {
        remainder = (remainder << 1) | u32::from(bit);
        if remainder & (1 << CRC_BITS) != 0 {
            remainder ^= CRC24C_POLYNOMIAL;
        }
    }
    remainder == 0
}

#[cfg(test)]
pub(super) mod tests {
    use super::{PAYLOAD_BITS, POLAR_LENGTH, RATE_MATCHED_LENGTH, decode};

    // MSB-packed result of independently applying polynomial division,
    // the Kronecker generator matrix and rate matching to 0xe909f98b.
    pub(in crate::nr::pbch) const CODEWORD: [u8; RATE_MATCHED_LENGTH / 8] = [
        0x44, 0x6c, 0x72, 0xcc, 0x24, 0x0c, 0x3f, 0x17, 0xed, 0x53, 0xf6, 0x48, 0xa0, 0x88, 0x96,
        0x28, 0x88, 0xa0, 0x77, 0xa0, 0x41, 0xff, 0x41, 0x00, 0xe8, 0xc0, 0x17, 0xc0, 0xde, 0x60,
        0xde, 0x9f, 0xf3, 0xdb, 0x0c, 0xdb, 0xc5, 0x7b, 0xc5, 0x84, 0x6c, 0x44, 0x93, 0x44, 0xa5,
        0x1b, 0xa5, 0xe4, 0xbb, 0x6c, 0x72, 0x33, 0xdb, 0x0c, 0xc0, 0x17, 0xed, 0xac, 0xf6, 0xb7,
        0x5f, 0x88, 0x96, 0xd7, 0x44, 0x6c, 0x72, 0xcc, 0x24, 0x0c, 0x3f, 0x17, 0xed, 0x53, 0xf6,
        0x48, 0xa0, 0x88, 0x96, 0x28, 0x88, 0xa0, 0x77, 0xa0, 0x41, 0xff, 0x41, 0x00, 0xe8, 0xc0,
        0x17, 0xc0, 0xde, 0x60, 0xde, 0x9f, 0xf3, 0xdb, 0x0c, 0xdb, 0xc5, 0x7b, 0xc5, 0x84, 0x6c,
        0x44, 0x93, 0x44,
    ];

    fn payload() -> [u8; PAYLOAD_BITS] {
        std::array::from_fn(|index| ((0xe909_f98b_u32 >> (PAYLOAD_BITS - 1 - index)) & 1) as u8)
    }

    fn fixture() -> [f32; RATE_MATCHED_LENGTH] {
        std::array::from_fn(|index| {
            if CODEWORD[index / 8] & (0x80 >> (index % 8)) == 0 {
                1.0
            } else {
                -1.0
            }
        })
    }

    #[test]
    fn decodes_independent_standard_vector() {
        assert_eq!(decode(&fixture()), Some(payload()));
    }

    #[test]
    fn combines_repetitions_as_soft_evidence() {
        let mut llrs = fixture();
        for llr in &mut llrs[..RATE_MATCHED_LENGTH - POLAR_LENGTH] {
            *llr *= -0.5;
        }
        assert_eq!(decode(&llrs), Some(payload()));
    }

    #[test]
    fn rejects_a_polar_codeword_with_corrupt_crc() {
        // Flipping the last CRC bit adds generator row 511, which is all ones.
        assert_eq!(decode(&fixture().map(|llr| -llr)), None);
    }

    #[test]
    fn rejects_erased_and_non_finite_evidence() {
        assert_eq!(decode(&[0.0; RATE_MATCHED_LENGTH]), None);

        let mut llrs = fixture();
        llrs[0] = f32::NAN;
        assert_eq!(decode(&llrs), None);
    }
}
