//! Decode both interleaved RS(255,231) words in the [RS41 frame format].
//!
//! [RS41 frame format]: https://github.com/rs1729/RS/blob/ce448555d34da5ca01f1a526e838c4f7be464481/rs41/rs41.txt#L198-L221

use super::frame::{EXTENDED_FRAME_BYTES, FRAME_MARKER_INDEX, STANDARD_FRAME_BYTES, SYNC};
use crate::dsp::reed_solomon::ReedSolomon;

const WORD_BYTES: usize = 255;
const PARITY_BYTES: usize = 24;
static REED_SOLOMON: ReedSolomon = ReedSolomon::new(0x11d, 0);
const _: () = assert!(FRAME_MARKER_INDEX == SYNC.len() + 2 * PARITY_BYTES);
const _: () = assert!(EXTENDED_FRAME_BYTES == SYNC.len() + 2 * WORD_BYTES);

/// Correct both words in an exact-size, dewhitened frame.
///
/// Returns the number of corrected transmitted bytes when both words pass
/// their syndrome checks. Leaves the frame intact if either word cannot be
/// corrected. The synchronization bytes are outside both codewords.
pub(super) fn correct(frame: &mut [u8]) -> Option<usize> {
    if !matches!(frame.len(), STANDARD_FRAME_BYTES | EXTENDED_FRAME_BYTES) {
        return None;
    }

    let (header_and_parity, data) = frame.split_at_mut(FRAME_MARKER_INDEX);
    let parity = &mut header_and_parity[SYNC.len()..];
    let transmitted = PARITY_BYTES + data.len() / 2;
    let mut words = [[0; WORD_BYTES]; 2];
    for (lane, word) in words.iter_mut().enumerate() {
        let start = lane * PARITY_BYTES;
        word[..PARITY_BYTES].copy_from_slice(&parity[start..start + PARITY_BYTES]);
        for (symbol, &byte) in word[PARITY_BYTES..transmitted]
            .iter_mut()
            .zip(data[lane..].iter().step_by(2))
        {
            *symbol = byte;
        }
    }

    let [even, odd] = &mut words;
    let corrected = REED_SOLOMON.correct(&mut even[..transmitted], PARITY_BYTES)?
        + REED_SOLOMON.correct(&mut odd[..transmitted], PARITY_BYTES)?;
    for (lane, word) in words.iter().enumerate() {
        let start = lane * PARITY_BYTES;
        parity[start..start + PARITY_BYTES].copy_from_slice(&word[..PARITY_BYTES]);
        for (byte, &symbol) in data[lane..]
            .iter_mut()
            .step_by(2)
            .zip(&word[PARITY_BYTES..transmitted])
        {
            *byte = symbol;
        }
    }
    Some(corrected)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent polynomial division with the documented generator produced
    // the parity for the synthetic byte ramp at each supported length.
    const STANDARD_PARITY: [u8; 48] = [
        0x9f, 0x82, 0xd1, 0x69, 0x0b, 0x04, 0x3b, 0x2a, 0x8e, 0xf7, 0xab, 0x5d, 0xc2, 0xf8, 0x24,
        0xd9, 0x50, 0x9d, 0x04, 0x1d, 0x4f, 0x27, 0x5d, 0x12, 0x2b, 0xbc, 0xc5, 0x3d, 0x0b, 0x40,
        0x38, 0x3c, 0x20, 0xb8, 0x93, 0xd4, 0x7e, 0x5d, 0xb8, 0x2d, 0x80, 0xae, 0x63, 0x58, 0x06,
        0x28, 0xbb, 0xe1,
    ];
    const EXTENDED_PARITY: [u8; 48] = [
        0x3f, 0x73, 0xcd, 0x11, 0xdc, 0x53, 0x6b, 0x1d, 0xf5, 0x48, 0xdb, 0x3b, 0x8d, 0xf7, 0x4c,
        0xb6, 0xe2, 0xba, 0xfb, 0x9b, 0x51, 0xac, 0xac, 0x5a, 0xfd, 0x2a, 0x57, 0x82, 0x79, 0xc3,
        0x8a, 0xd3, 0x16, 0x8c, 0x4b, 0xd5, 0x75, 0x68, 0x08, 0x4d, 0x89, 0x9e, 0x0b, 0x2c, 0x91,
        0xe0, 0xea, 0x2a,
    ];

    fn fixture(length: usize, parity: &[u8; 48]) -> Vec<u8> {
        SYNC.into_iter()
            .chain(parity.iter().copied())
            .chain((0..length - FRAME_MARKER_INDEX).map(|i| (37 * i + 0x61) as u8))
            .collect()
    }

    #[test]
    fn verifies_clean_words_and_corrects_twelve_errors_per_lane() {
        for (length, parity) in [
            (STANDARD_FRAME_BYTES, &STANDARD_PARITY),
            (EXTENDED_FRAME_BYTES, &EXTENDED_PARITY),
        ] {
            let clean = fixture(length, parity);
            let mut damaged = clean.clone();
            assert_eq!(correct(&mut damaged), Some(0));
            assert_eq!(damaged, clean);
            for lane in 0..2 {
                for offset in [0, 3, 7, 13, 18, 23] {
                    damaged[SYNC.len() + lane * PARITY_BYTES + offset] ^= 0x87;
                }
                let last = (length - FRAME_MARKER_INDEX) / 2 - 1;
                for offset in [0, 1, 19, 37, 85, last] {
                    damaged[FRAME_MARKER_INDEX + 2 * offset + lane] ^= 0x42;
                }
            }
            assert_eq!(correct(&mut damaged), Some(24));
            assert_eq!(damaged, clean);
        }
    }

    #[test]
    fn leaves_both_lanes_untouched_when_one_cannot_be_corrected() {
        let mut frame = fixture(STANDARD_FRAME_BYTES, &STANDARD_PARITY);
        frame[SYNC.len()] ^= 1;
        for offset in 0..13 {
            frame[FRAME_MARKER_INDEX + 2 * offset + 1] ^= 0x63;
        }
        let received = frame.clone();
        assert_eq!(correct(&mut frame), None);
        assert_eq!(frame, received);
    }

    #[test]
    fn rejects_truncation_and_a_root_in_a_shortening_zero() {
        let mut frame = fixture(STANDARD_FRAME_BYTES, &STANDARD_PARITY);
        // This parity assumes one extra nonzero coefficient in the even lane.
        frame[SYNC.len()..SYNC.len() + PARITY_BYTES].copy_from_slice(&[
            0xbc, 0x25, 0x6d, 0x98, 0xf1, 0x94, 0xaf, 0xf6, 0xcd, 0x53, 0x5a, 0x9e, 0x14, 0x5f,
            0x6f, 0x25, 0x12, 0xce, 0xfe, 0xe7, 0xef, 0x55, 0xf3, 0x0e,
        ]);
        let received = frame.clone();
        assert_eq!(correct(&mut frame), None);
        assert_eq!(frame, received);
        assert_eq!(correct(&mut frame[..STANDARD_FRAME_BYTES - 1]), None);
    }
}
