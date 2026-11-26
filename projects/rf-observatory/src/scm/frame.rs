//! Decode the SCM+ endpoint fields using the [pinned framing reference].
//!
//! [pinned framing reference]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/scmplus.c#L25-L82

/// Number of octets from sync through the stored CRC.
pub(super) const FRAME_BYTES: usize = 16;
/// Decoded sync octets, including the protocol identifier.
pub(super) const SYNC: [u8; 3] = [0x16, 0xa3, 0x1e];

const CRC_BODY_START: usize = 2;
const CRC_START: usize = 14;
const CRC_POLYNOMIAL: u16 = 0x1021;
const CRC_INITIAL: u16 = 0x0971;

/// Endpoint fields from an SCM+ frame that passed its integrity check.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    /// Unsigned 32-bit endpoint identifier.
    pub(super) id: u32,
    /// Full endpoint type octet carried in the frame.
    pub(super) endpoint_type: u8,
    /// Stored CRC-16, interpreted as a big-endian value.
    pub(super) crc: u16,
}

/// Validate the complete sync and CRC before extracting the endpoint fields.
/// A sync or CRC mismatch returns `None`.
pub(super) fn decode(frame: &[u8; FRAME_BYTES]) -> Option<Endpoint> {
    if !frame.starts_with(&SYNC) {
        return None;
    }
    let crc = u16::from_be_bytes([frame[CRC_START], frame[CRC_START + 1]]);
    if crc16(&frame[CRC_BODY_START..CRC_START]) != crc {
        return None;
    }
    Some(Endpoint {
        id: u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]),
        endpoint_type: frame[3],
        crc,
    })
}

fn crc16(payload: &[u8]) -> u16 {
    let mut crc = CRC_INITIAL;
    for &byte in payload {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            let feedback = if crc & 0x8000 != 0 { CRC_POLYNOMIAL } else { 0 };
            crc = (crc << 1) ^ feedback;
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic fields with a checksum verified by Python's binascii.crc_hqx.
    const VALID_FRAME: [u8; FRAME_BYTES] = [
        0x16, 0xa3, 0x1e, 0xab, 0x89, 0xab, 0xcd, 0xef, 0x13, 0x57, 0x9b, 0xdf, 0x24, 0x68, 0xa7,
        0xbf,
    ];

    #[test]
    fn extracts_the_full_id_and_type() {
        assert_eq!(
            decode(&VALID_FRAME),
            Some(Endpoint {
                id: 0x89ab_cdef,
                endpoint_type: 0xab,
                crc: 0xa7bf,
            })
        );
    }

    #[test]
    fn rejects_a_single_bit_error_in_every_frame_octet() {
        for index in 0..FRAME_BYTES {
            for bit in 0..u8::BITS {
                let mut corrupted = VALID_FRAME;
                corrupted[index] ^= 1 << bit;
                assert!(decode(&corrupted).is_none(), "octet {index}, bit {bit}");
            }
        }
    }
}
