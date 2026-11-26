//! RS41 framing, whitening, and CRC-checked STATUS fields.
//!
//! The [published frame analysis] describes the two lengths, frame marker,
//! whitening sequence, and STATUS vectors. The [reference decoder] gives
//! the full whitening mask and CRC bit order.
//!
//! [published frame analysis]: https://github.com/rs1729/RS/blob/ce448555d34da5ca01f1a526e838c4f7be464481/rs41/rs41.txt#L209-L290
//! [reference decoder]: https://github.com/rs1729/RS/blob/ce448555d34da5ca01f1a526e838c4f7be464481/demod/mod/rs41mod.c

use serde::Serialize;

/// Sync octets as transmitted, with the least significant bit first.
pub(super) const AIR_SYNC: [u8; 8] = [0x10, 0xb6, 0xca, 0x11, 0x22, 0x96, 0x12, 0xf8];
/// Decoded sync octets at the start of the frame.
pub(super) const SYNC: [u8; 8] = [0x86, 0x35, 0xf4, 0x40, 0x93, 0xdf, 0x1a, 0x60];
/// Standard frame length including sync and interleaved parity.
pub(super) const STANDARD_FRAME_BYTES: usize = 320;
/// Extended frame length including sync and interleaved parity.
pub(super) const EXTENDED_FRAME_BYTES: usize = 518;
/// The first information octet follows the eight sync and 48 parity octets.
pub(super) const FRAME_MARKER_INDEX: usize = 56;

const STATUS_INDEX: usize = FRAME_MARKER_INDEX + 1;
const STATUS_BODY_BYTES: usize = 40;
const STATUS_END: usize = STATUS_INDEX + 2 + STATUS_BODY_BYTES + 2;
const WHITENING_MASK: [u8; 64] = [
    0x96, 0x83, 0x3e, 0x51, 0xb1, 0x49, 0x08, 0x98, 0x32, 0x05, 0x59, 0x0e, 0xf9, 0x44, 0xc6, 0x26,
    0x21, 0x60, 0xc2, 0xea, 0x79, 0x5d, 0x6d, 0xa1, 0x54, 0x69, 0x47, 0x0c, 0xdc, 0xe8, 0x5c, 0xf1,
    0xf7, 0x76, 0x82, 0x7f, 0x07, 0x99, 0xa2, 0x2c, 0x93, 0x7c, 0x30, 0x63, 0xf5, 0x10, 0x2e, 0x61,
    0xd0, 0xbc, 0xb4, 0xb6, 0x06, 0xaa, 0xf4, 0x23, 0x78, 0x6e, 0x3b, 0xae, 0xbf, 0x7b, 0x4c, 0xc1,
];

/// Remove whitening at a byte index measured from the first sync octet.
pub(super) fn dewhiten_byte(index: usize, byte: u8) -> u8 {
    byte ^ WHITENING_MASK[index % WHITENING_MASK.len()]
}

/// Match the exact length marker after dewhitening and any parity correction.
pub(super) fn frame_length(marker: u8) -> Option<usize> {
    match marker {
        0x0f => Some(STANDARD_FRAME_BYTES),
        0xf0 => Some(EXTENDED_FRAME_BYTES),
        _ => None,
    }
}

/// Radiosonde identity and sequence fields protected by the STATUS CRC.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct Status {
    /// Serial text from the eight-octet STATUS field, without trailing padding.
    pub(super) serial: String,
    /// Little-endian frame number as transmitted, including wraparound.
    pub(super) frame_number: u16,
}

/// Parse a complete dewhitened frame after any requested parity correction.
///
/// The normalized sync, marker and STATUS ID and length must agree with the
/// frame layout. The STATUS body must pass CRC-16/CCITT-FALSE with its stored
/// little-endian value. No other block is used to recover the identity.
pub(super) fn status(frame: &[u8]) -> Option<Status> {
    if !frame.starts_with(&SYNC) || frame_length(*frame.get(FRAME_MARKER_INDEX)?)? != frame.len() {
        return None;
    }
    let block = frame.get(STATUS_INDEX..STATUS_END)?;
    if block[..2] != [0x79, STATUS_BODY_BYTES as u8] {
        return None;
    }
    let (body, crc) = block[2..].split_last_chunk::<2>()?;
    if crc16(body) != u16::from_le_bytes(*crc) {
        return None;
    }

    let serial = std::str::from_utf8(&body[2..10])
        .ok()?
        .trim_end_matches('\0');
    if serial.is_empty() || !serial.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    Some(Status {
        serial: serial.to_owned(),
        frame_number: u16::from_le_bytes([body[0], body[1]]),
    })
}

fn crc16(body: &[u8]) -> u16 {
    let mut crc = 0xffff;
    for &byte in body {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 == 0 { 0 } else { 0x1021 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use data_encoding::HEXLOWER;

    use super::*;

    // The published STATUS blocks include their independently obtained CRCs.
    const STANDARD_STATUS: &str = concat!(
        "792816174b313933303239331a00000300000a00002f000732",
        "2ce53e31991abf12dada3eb68468c16755d51c",
    );
    const EXTENDED_STATUS: &str = concat!(
        "792896134b343032303234341c00000100000c00007a000732",
        "0f00000000008920bac2000000000000009269",
    );

    fn frame(marker: u8, block: &str) -> Vec<u8> {
        let mut frame = vec![0; frame_length(marker).unwrap()];
        frame[..SYNC.len()].copy_from_slice(&SYNC);
        frame[FRAME_MARKER_INDEX] = marker;
        frame[STATUS_INDEX..STATUS_END]
            .copy_from_slice(&HEXLOWER.decode(block.as_bytes()).unwrap());
        frame
    }

    #[test]
    fn publishes_the_status_vectors_from_both_frame_lengths() {
        for (marker, block, serial, frame_number) in [
            (0x0f, STANDARD_STATUS, "K1930293", 5910),
            (0xf0, EXTENDED_STATUS, "K4020244", 5014),
        ] {
            assert_eq!(
                status(&frame(marker, block)),
                Some(Status {
                    serial: serial.to_owned(),
                    frame_number
                })
            );
        }
    }

    #[test]
    fn rejects_bad_headers_markers_lengths_and_status_crc() {
        let valid = frame(0x0f, STANDARD_STATUS);
        assert_eq!(status(&valid[..319]), None);
        assert_eq!(status(&valid[..STATUS_END]), None);
        for index in [
            0,
            FRAME_MARKER_INDEX,
            STATUS_INDEX,
            STATUS_INDEX + 1,
            STATUS_INDEX + 5,
            STATUS_END - 1,
        ] {
            let mut invalid = valid.clone();
            invalid[index] ^= 1;
            assert_eq!(status(&invalid), None, "corruption at {index:#04x}");
        }
    }

    #[test]
    fn dewhitens_the_air_sync_and_keeps_the_mask_phase_at_wrap() {
        for (index, byte) in AIR_SYNC.into_iter().enumerate() {
            assert_eq!(dewhiten_byte(index, byte), SYNC[index]);
        }
        // The published over-air example includes the marker at 56 and a
        // serial that crosses the mask boundary at byte 64.
        assert_eq!(dewhiten_byte(56, 0x77), 0x0f);
        assert_eq!(dewhiten_byte(63, 0xf8), b'9');
        assert_eq!(dewhiten_byte(64, 0xa5), b'3');
    }
}
