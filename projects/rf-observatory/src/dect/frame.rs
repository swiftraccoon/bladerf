//! Validate ordinary fixed-part identity tails in a conventional DECT A-field.
//!
//! The channel, NT header and transmitted final check bit follow
//! [ETSI EN 300 175-3 §§ 6.2.5.2 and 7.2][mac].
//!
//! [mac]: https://www.etsi.org/deliver/etsi_en/300100_300199/30017503/02.09.01_60/en_30017503v020901p.pdf#page=104

pub(super) const S_BITS: usize = 32;
pub(super) const RFP_SYNC: u32 = 0xaaaa_e98a;
pub(super) const A_BYTES: usize = 8;
pub(super) const PREFIX_BITS: usize = S_BITS + A_BYTES * 8;

/// A checked 40-bit Radio Fixed Part Identity and its A-field header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Identity {
    pub(super) rfpi: u64,
    pub(super) a_field_header: u8,
}

/// Verify the NT header and R-CRC of an MSB-first, polarity-normalized A-field.
pub(super) fn decode(a: [u8; A_BYTES]) -> Option<Identity> {
    let [header, b1, b2, b3, b4, b5, check_high, check_low] = a;
    if header >> 5 != 0b011 || r_crc(&a[..6]) != u16::from_be_bytes([check_high, check_low]) {
        return None;
    }
    Some(Identity {
        rfpi: u64::from_be_bytes([0, 0, 0, b1, b2, b3, b4, b5]),
        a_field_header: header,
    })
}

fn r_crc(prefix: &[u8]) -> u16 {
    let mut remainder = 0u16;
    for &byte in prefix {
        remainder ^= u16::from(byte) << 8;
        for _ in 0..8 {
            let feedback = if remainder & 0x8000 == 0 { 0 } else { 0x0589 };
            remainder = (remainder << 1) ^ feedback;
        }
    }
    // The final transmitted check bit is inverted.
    remainder ^ 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_dummy_and_longer_bearer_identity_fields() {
        for (header, check) in [(0x6e, [0x8a, 0x9e]), (0x61, [0x4c, 0xa8])] {
            assert_eq!(
                decode([header, 0x00, 0x4e, 0xb4, 0xbd, 0x50, check[0], check[1]]),
                Some(Identity {
                    rfpi: 0x00_4e_b4_bd_50,
                    a_field_header: header
                }),
            );
        }
    }

    #[test]
    fn requires_wire_check_and_an_identity_tail() {
        assert_eq!(r_crc(b"123456789"), 0x007e);
        for invalid in [
            [0x6e, 0x00, 0x4e, 0xb4, 0xbd, 0x50, 0x8a, 0x9f],
            [0x6e, 0x00, 0x4e, 0xb4, 0xbd, 0x51, 0x8a, 0x9e],
            [0x6e, 0x00, 0x4e, 0xb4, 0xbd, 0x50, 0x9e, 0x8a],
            [0x6e, 0x00, 0x4e, 0xb4, 0xbd, 0x50, 0x00, 0x00],
            [0x8e, 0x00, 0x4e, 0xb4, 0xbd, 0x50, 0x89, 0xbb],
        ] {
            assert_eq!(decode(invalid), None);
        }
    }

    #[test]
    fn complementing_a_valid_portable_field_invalidates_its_check() {
        // Portable and fixed-part sync are complements. An auto-inverted
        // portable field with complemented NT header still fails the R-CRC.
        let prefix = [0x91, 0xff, 0xb1, 0x4b, 0x42, 0xaf];
        let [high, low] = r_crc(&prefix).to_be_bytes();
        let [b0, b1, b2, b3, b4, b5] = prefix;
        assert_eq!(
            decode([b0, b1, b2, b3, b4, b5, high, low].map(|byte| !byte)),
            None
        );
    }
}
