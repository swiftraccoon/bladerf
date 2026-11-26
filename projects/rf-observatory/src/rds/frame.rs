//! Validate groups from a stream of demodulated RDS bits.
//!
//! [NRSC-4-A] sections 2.3, 2.4 and 3.1.2 describe block coding and group
//! versions. [NRSC-4] Annex A lists the offset words.
//!
//! [NRSC-4-A]: https://www.nrscstandards.org/standards-and-guidelines/documents/archive/nrsc-4-a-standard-2005.pdf
//! [NRSC-4]: https://www.nrscstandards.org/standards-and-guidelines/documents/archive/nrsc-4-1998.pdf

const CHECK_BITS: u8 = 10;
const BLOCK_BITS: u8 = 26;
const GROUP_BITS: u8 = 4 * BLOCK_BITS;
const BLOCK_MASK: u128 = (1 << BLOCK_BITS) - 1;
const GROUP_MASK: u128 = (1 << GROUP_BITS) - 1;
const GENERATOR: u32 = 0x5b9;
const OFFSET_A: u32 = 0x0fc;
const OFFSET_B: u32 = 0x198;
const OFFSET_C: u32 = 0x168;
const OFFSET_C_PRIME: u32 = 0x350;
const OFFSET_D: u32 = 0x1b4;
const VERSION_B: u16 = 1 << 11;

/// Recognize complete groups at any bit alignment using a fixed-size window.
#[derive(Default)]
pub(super) struct Decoder {
    window: u128,
    bits: u8,
}

impl Decoder {
    /// Consume the next differential-decoded bit in wire order, MSB first.
    /// Return the four information words after all checkwords and the version
    /// B program identification repeat have passed validation.
    pub(super) fn push(&mut self, bit: bool) -> Option<[u16; 4]> {
        self.window = ((self.window << 1) | u128::from(bit)) & GROUP_MASK;
        self.bits = (self.bits + 1).min(GROUP_BITS);
        if self.bits < GROUP_BITS {
            return None;
        }

        let block = |shift| ((self.window >> shift) & BLOCK_MASK) as u32;
        // Block D rejects most windows before the other checks are needed.
        let d = checked_block(block(0), OFFSET_D)?;
        let b = checked_block(block(2 * BLOCK_BITS), OFFSET_B)?;
        let version_b = b & VERSION_B != 0;
        let offset_c = if version_b { OFFSET_C_PRIME } else { OFFSET_C };
        let c = checked_block(block(BLOCK_BITS), offset_c)?;
        let a = checked_block(block(3 * BLOCK_BITS), OFFSET_A)?;
        (!version_b || a == c).then_some([a, b, c, d])
    }
}

fn checked_block(block: u32, offset: u32) -> Option<u16> {
    // Dividing the received block leaves the position offset as the remainder.
    let mut remainder = block;
    for shift in (0..BLOCK_BITS - CHECK_BITS).rev() {
        if remainder & (1 << (shift + CHECK_BITS)) != 0 {
            remainder ^= GENERATOR << shift;
        }
    }
    (remainder == offset).then_some((block >> CHECK_BITS) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed codewords from the generator-matrix method in NRSC-4 Annex B.1.2.
    const GROUP_A: [u32; 4] = [0x012d_4d65, 0x0086_9dd4, 0x0121_94c2, 0x01b1_b27b];
    const GROUP_B: [u32; 4] = [0x0286_c99c, 0x00a7_01eb, 0x0286_ca30, 0x0151_973c];
    const WORDS_A: [u16; 4] = [0x4b53, 0x21a7, 0x4865, 0x6c6c];
    const WORDS_B: [u16; 4] = [0xa1b2, 0x29c0, 0xa1b2, 0x5465];

    fn bits(group: [u32; 4]) -> impl Iterator<Item = bool> {
        group.into_iter().flat_map(|block| {
            (0..BLOCK_BITS)
                .rev()
                .map(move |bit| block & (1 << bit) != 0)
        })
    }

    #[test]
    fn accepts_published_checkwords_and_both_group_versions() {
        assert_eq!(checked_block(0x0000_0421, OFFSET_B), Some(0x0001));
        assert_eq!(checked_block(0x03ff_fd55, OFFSET_B), Some(0xffff));

        let mut decoder = Decoder::default();
        let mut first = bits(GROUP_A);
        for bit in (&mut first).take(usize::from(GROUP_BITS - 1)) {
            assert_eq!(decoder.push(bit), None);
        }
        assert_eq!(decoder.push(first.next().unwrap()), Some(WORDS_A));
        let trailing = bits(GROUP_B)
            .filter_map(|bit| decoder.push(bit))
            .collect::<Vec<_>>();
        assert_eq!(trailing, [WORDS_B]);
    }

    #[test]
    fn rejects_every_single_bit_error() {
        for group in [GROUP_A, GROUP_B] {
            for block in 0..group.len() {
                for bit in 0..BLOCK_BITS {
                    let mut bad = group;
                    bad[block] ^= 1 << bit;
                    let mut decoder = Decoder::default();
                    assert!(bits(bad).all(|bit| decoder.push(bit).is_none()));
                }
            }
        }
    }

    #[test]
    fn rejects_wrong_c_offsets_and_a_different_repeated_pi() {
        for (mut group, wrong_c) in [
            (GROUP_A, 0x0121_96fa),
            (GROUP_B, 0x0286_c808),
            (GROUP_B, 0x0286_cf89),
        ] {
            group[2] = wrong_c;
            let mut decoder = Decoder::default();
            assert!(bits(group).all(|bit| decoder.push(bit).is_none()));
        }
    }

    #[test]
    fn acquires_from_a_partial_group_and_recovers_after_a_bit_slip() {
        let mut corrupted = GROUP_B;
        corrupted[0] ^= 1;
        let mut decoder = Decoder::default();
        let decoded = bits(GROUP_A)
            .skip(13)
            .chain(bits(GROUP_B))
            .chain(bits(corrupted))
            .chain([true, false, true])
            .chain(bits(GROUP_A))
            .filter_map(|bit| decoder.push(bit))
            .collect::<Vec<_>>();
        assert_eq!(decoded, [WORDS_B, WORDS_A]);
    }
}
