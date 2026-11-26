//! Validate legacy SCM frames and extract endpoint identity fields.
//!
//! [Itron's Table 1][wire layout] specifies a start bit of one followed by
//! the 20-bit preamble `0xf2a60`. The [pinned receiver][checksum reference]
//! checks the shortened BCH code as an MSB-first, zero-initialized,
//! zero-residue polynomial remainder.
//!
//! [wire layout]: https://patents.google.com/patent/US20080068213A1/en
//! [checksum reference]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/ert_scm.c#L25-L73

/// Count of decoded octets from the start bit through the stored check field.
pub(super) const FRAME_BYTES: usize = 12;
/// Count of decoded bits in the complete frame.
pub(super) const FRAME_BITS: usize = FRAME_BYTES * 8;
/// Count of decoded bits in the start bit and following preamble.
pub(super) const SYNC_BITS: usize = 21;
/// Right-aligned start bit and preamble in received bit order.
pub(super) const SYNC_WORD: u32 = 0x1f_2a60;

const BCH_POLYNOMIAL: u16 = 0x6f63;

/// Fields from a legacy SCM frame with verified sync and BCH remainder.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    /// Unsigned 26-bit endpoint identifier.
    pub(super) id: u32,
    /// Four-bit endpoint type code.
    pub(super) endpoint_type: u8,
    /// Two physical tamper bits as transmitted.
    pub(super) physical_tamper: u8,
    /// Two encoder tamper bits as transmitted.
    pub(super) encoder_tamper: u8,
    /// Stored BCH check field as a big-endian value.
    pub(super) crc: u16,
}

/// Extract the endpoint fields from one complete, MSB-first frame.
///
/// Returns `None` on a sync mismatch or a nonzero BCH remainder. Checks the
/// received bits as supplied and performs no error correction.
pub(super) fn decode(frame: &[u8; FRAME_BYTES]) -> Option<Endpoint> {
    let sync = u32::from_be_bytes([0, frame[0], frame[1], frame[2]]) >> (24 - SYNC_BITS);
    // The leading five bits of octet 2 are zero after sync validation, so
    // including them aligns the protected section without changing the remainder.
    if sync != SYNC_WORD || bch_remainder(&frame[2..]) != 0 {
        return None;
    }
    Some(Endpoint {
        id: u32::from_be_bytes([(frame[2] >> 1) & 0x03, frame[7], frame[8], frame[9]]),
        endpoint_type: (frame[3] >> 2) & 0x0f,
        physical_tamper: frame[3] >> 6,
        encoder_tamper: frame[3] & 0x03,
        crc: u16::from_be_bytes([frame[10], frame[11]]),
    })
}

fn bch_remainder(bytes: &[u8]) -> u16 {
    let mut remainder = 0;
    for &byte in bytes {
        remainder ^= u16::from(byte) << 8;
        for _ in 0..8 {
            let feedback = if remainder & 0x8000 != 0 {
                BCH_POLYNOMIAL
            } else {
                0
            };
            remainder = (remainder << 1) ^ feedback;
        }
    }
    remainder
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic endpoint with the special-use bit set and a check value
    // independently calculated by polynomial long division.
    const FRAME: [u8; FRAME_BYTES] = [
        0xf9, 0x53, 0x07, 0xa7, 0x00, 0x00, 0x00, 0x89, 0xab, 0xcd, 0x48, 0x92,
    ];

    #[test]
    fn extracts_endpoint_identity_and_tamper_bits() {
        assert_eq!(
            decode(&FRAME),
            Some(Endpoint {
                id: 0x0389_abcd,
                endpoint_type: 9,
                physical_tamper: 2,
                encoder_tamper: 3,
                crc: 0x4892,
            }),
        );
    }

    #[test]
    fn rejects_every_single_bit_error() {
        for bit in 0..FRAME_BITS {
            let mut corrupted = FRAME;
            corrupted[bit / 8] ^= 0x80 >> (bit % 8);
            assert!(decode(&corrupted).is_none(), "accepted bit error at {bit}");
        }
    }
}
