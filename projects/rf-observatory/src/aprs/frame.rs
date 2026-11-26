//! AX.25 receive framing for the APRS unnumbered-information profile.
//!
//! The [AX.25 specification](https://tapr.org/pdf/AX25.2.2.pdf) defines HDLC
//! framing, de-stuffing, FCS and address layout in §§3.1–3.10. Chapter 3 of
//! the [APRS protocol reference](https://www.aprs.org/doc/APRS101.PDF)
//! defines the UI control, PID, path and information lengths accepted here.

use data_encoding::HEXLOWER;
use serde::Serialize;

use crate::dsp::hdlc::fcs;

const FLAG: u8 = 0x7e;
const ADDRESS_BYTES: usize = 7;
const MAX_DIGIPEATERS: usize = 8;
const MAX_INFORMATION_BYTES: usize = 256;
const UI_NO_LAYER_3: [u8; 2] = [0x03, 0xf0];
const MAX_FRAME_BYTES: usize = (2 + MAX_DIGIPEATERS) * ADDRESS_BYTES
    + UI_NO_LAYER_3.len()
    + MAX_INFORMATION_BYTES
    + size_of::<u16>();

/// Shortest frame with two flags and no inserted zero bits.
pub(super) const MIN_WIRE_BITS: u64 =
    ((2 * ADDRESS_BYTES + UI_NO_LAYER_3.len() + 1 + size_of::<u16>() + 2) * 8) as u64;

#[derive(Debug, Serialize)]
struct Address {
    callsign: String,
    ssid: u8,
}

impl Address {
    fn decode(raw: &[u8; ADDRESS_BYTES]) -> Option<Self> {
        let &[ref shifted @ .., flags] = raw;
        if shifted.iter().any(|&octet| octet & 1 != 0) {
            return None;
        }
        let mut characters = [0; ADDRESS_BYTES - 1];
        for (character, octet) in characters.iter_mut().zip(shifted) {
            *character = octet >> 1;
        }
        let callsign = std::str::from_utf8(&characters).ok()?.trim_end_matches(' ');
        if callsign.is_empty()
            || !callsign
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            callsign: callsign.to_owned(),
            ssid: (flags >> 1) & 0x0f,
        })
    }
}

#[derive(Debug, Serialize)]
struct Digipeater {
    #[serde(flatten)]
    address: Address,
    repeated: bool,
}

/// An FCS-valid UI frame in the AX.25 profile used by APRS.
///
/// Addresses and information encode the received link-layer data. The
/// information field remains opaque, including its application message type.
#[derive(Debug, Serialize)]
pub(super) struct Frame {
    destination: Address,
    source: Address,
    digipeaters: Vec<Digipeater>,
    information_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    information_text: Option<String>,
    fcs: u16,
    /// Complete de-stuffed AX.25 octets, including the little-endian FCS.
    pub(super) frame_hex: String,
}

impl Frame {
    fn decode(bytes: &[u8]) -> Option<Self> {
        let (body, &wire_fcs) = bytes.split_last_chunk::<2>()?;
        let expected_fcs = u16::from_le_bytes(wire_fcs);
        if fcs(body) != expected_fcs {
            return None;
        }

        let (destination, rest) = body.split_first_chunk::<ADDRESS_BYTES>()?;
        if destination[ADDRESS_BYTES - 1] & 1 != 0 {
            return None;
        }
        let (source, mut rest) = rest.split_first_chunk::<ADDRESS_BYTES>()?;
        let mut digipeaters = Vec::new();
        let mut extension = source[ADDRESS_BYTES - 1] & 1 != 0;
        while !extension {
            if digipeaters.len() == MAX_DIGIPEATERS {
                return None;
            }
            let (address, tail) = rest.split_first_chunk::<ADDRESS_BYTES>()?;
            let flags = address[ADDRESS_BYTES - 1];
            digipeaters.push(Digipeater {
                address: Address::decode(address)?,
                repeated: flags & 0x80 != 0,
            });
            extension = flags & 1 != 0;
            rest = tail;
        }

        let information = rest.strip_prefix(&UI_NO_LAYER_3)?;
        if !(1..=MAX_INFORMATION_BYTES).contains(&information.len()) {
            return None;
        }
        let information_text = std::str::from_utf8(information)
            .ok()
            .filter(|text| {
                text.bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            })
            .map(str::to_owned);
        Some(Self {
            destination: Address::decode(destination)?,
            source: Address::decode(source)?,
            digipeaters,
            information_hex: HEXLOWER.encode(information),
            information_text,
            fcs: expected_fcs,
            frame_hex: HEXLOWER.encode(bytes),
        })
    }
}

struct Candidate {
    bytes: [u8; MAX_FRAME_BYTES],
    length: usize,
    partial: u8,
    bits: u8,
    ones: u8,
}

impl Default for Candidate {
    fn default() -> Self {
        Self {
            bytes: [0; MAX_FRAME_BYTES],
            length: 0,
            partial: 0,
            bits: 0,
            ones: 0,
        }
    }
}

impl Candidate {
    fn reset(&mut self) {
        self.length = 0;
        self.partial = 0;
        self.bits = 0;
        self.ones = 0;
    }

    fn push(&mut self, bit: bool) -> bool {
        if bit {
            if self.ones == 5 {
                return false;
            }
            self.ones += 1;
            self.partial |= 1 << self.bits;
        } else if std::mem::take(&mut self.ones) == 5 {
            return true;
        }
        self.bits += 1;
        if self.bits == 8 {
            let Some(byte) = self.bytes.get_mut(self.length) else {
                return false;
            };
            *byte = self.partial;
            self.length += 1;
            self.partial = 0;
            self.bits = 0;
        }
        true
    }

    fn finish(&self) -> Option<Frame> {
        if self.bits != 0 || self.ones == 5 {
            return None;
        }
        Frame::decode(&self.bytes[..self.length])
    }
}

/// Streaming HDLC de-stuffing and bounded AX.25 frame validation.
#[derive(Default)]
pub(super) struct Decoder {
    recent: u8,
    buffered_bits: u8,
    in_frame: bool,
    candidate: Candidate,
}

impl Decoder {
    /// Consume the next chronological bit after NRZI decoding.
    ///
    /// A valid closing flag emits a frame and also starts the next one.
    /// Incomplete, unsupported and malformed frames produce no output.
    pub(super) fn push(&mut self, bit: bool) -> Option<Frame> {
        // Hold a full flag's worth of bits until their framing is known.
        if self.buffered_bits == 8 {
            self.in_frame = self.in_frame && self.candidate.push(self.recent & 0x80 != 0);
        } else {
            self.buffered_bits += 1;
        }
        self.recent = (self.recent << 1) | u8::from(bit);
        if self.buffered_bits < 8 || self.recent != FLAG {
            return None;
        }

        let frame = if self.in_frame {
            self.candidate.finish()
        } else {
            None
        };
        self.in_frame = true;
        self.buffered_bits = 0;
        self.candidate.reset();
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Independent CRC-16 using the non-reflected 0x1021 polynomial, with
    // per-octet input reflection, final reflection and xorout 0xffff.
    const TEXT_FRAME: &str = "82a0a4a64040609c608682989862ae92888a6240e303f03e6c61622193e7";
    const BINARY_FRAME: &str =
        "82a0a4a64040609c608682989862ae92888a6240e303f03e6c616221fff87e008857";
    const TRAILING_STUFFED_FRAME: &str = "82a0a4a64040609c60868298986303f03e746573742036ccf8";

    fn wire_bits(byte: u8) -> impl Iterator<Item = bool> {
        (0..8).map(move |bit| byte & (1 << bit) != 0)
    }

    fn push_stuffed(stream: &mut Vec<bool>, bytes: &[u8]) {
        let mut run = 0;
        for bit in bytes.iter().copied().flat_map(wire_bits) {
            stream.push(bit);
            if bit {
                run += 1;
                if run == 5 {
                    stream.push(false);
                    run = 0;
                }
            } else {
                run = 0;
            }
        }
    }

    fn from_body(mut body: Vec<u8>) -> Vec<u8> {
        body.extend(fcs(&body).to_le_bytes());
        body
    }

    fn receive(bits: impl IntoIterator<Item = bool>) -> Vec<Frame> {
        let mut decoder = Decoder::default();
        bits.into_iter()
            .filter_map(|bit| decoder.push(bit))
            .collect()
    }

    #[test]
    fn crc_matches_standard_check_value_and_golden_frame() {
        assert_eq!(fcs(b"123456789"), 0x906e);
        let bytes = HEXLOWER.decode(TEXT_FRAME.as_bytes()).unwrap();
        assert_eq!(fcs(&bytes[..bytes.len() - 2]), 0xe793);
    }

    #[test]
    fn parses_shared_flags_and_preserves_binary_information() {
        let mut bits = vec![true, false, true];
        bits.extend(wire_bits(FLAG));
        bits.extend(wire_bits(FLAG));
        for fixture in [TEXT_FRAME, BINARY_FRAME, TRAILING_STUFFED_FRAME] {
            push_stuffed(&mut bits, &HEXLOWER.decode(fixture.as_bytes()).unwrap());
            bits.extend(wire_bits(FLAG));
        }

        let frames = receive(bits);
        assert_eq!(frames.len(), 3);
        let text = &frames[0];
        assert_eq!(text.frame_hex, TEXT_FRAME);
        assert_eq!(text.destination.callsign, "APRS");
        assert_eq!(text.source.callsign, "N0CALL");
        assert_eq!(text.source.ssid, 1);
        assert_eq!(text.digipeaters.len(), 1);
        assert_eq!(text.digipeaters[0].address.callsign, "WIDE1");
        assert_eq!(text.digipeaters[0].address.ssid, 1);
        assert!(text.digipeaters[0].repeated);
        assert_eq!(text.information_text.as_deref(), Some(">lab!"));
        assert_eq!(frames[1].frame_hex, BINARY_FRAME);
        assert_eq!(frames[1].information_hex, "3e6c616221fff87e00");
        assert!(frames[1].information_text.is_none());
        assert_eq!(frames[2].frame_hex, TRAILING_STUFFED_FRAME);
        assert!(frames[2].digipeaters.is_empty());
    }

    #[test]
    fn resynchronizes_after_bad_fcs_invalid_bit_extent_and_oversized_frame() {
        let bytes = HEXLOWER.decode(TEXT_FRAME.as_bytes()).unwrap();
        let mut corrupt = bytes.clone();
        corrupt[25] ^= 1;
        let mut bits: Vec<_> = wire_bits(FLAG).collect();
        push_stuffed(&mut bits, &corrupt);
        bits.extend(wire_bits(FLAG));
        push_stuffed(&mut bits, &bytes);
        bits.push(false);
        bits.extend(wire_bits(FLAG));
        push_stuffed(&mut bits, &[0; MAX_FRAME_BYTES + 1]);
        bits.extend(wire_bits(FLAG));
        bits.extend([true; 7]);
        bits.extend(wire_bits(FLAG));
        push_stuffed(
            &mut bits,
            &HEXLOWER.decode(TRAILING_STUFFED_FRAME.as_bytes()).unwrap(),
        );
        assert_eq!(bits.pop(), Some(false));
        bits.extend(wire_bits(FLAG));
        push_stuffed(&mut bits, &bytes);
        bits.extend(wire_bits(FLAG));
        push_stuffed(&mut bits, &bytes);

        let frames = receive(bits);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_hex, TEXT_FRAME);
    }

    #[test]
    fn bounds_path_and_information_even_with_valid_fcs() {
        let bytes = HEXLOWER.decode(TEXT_FRAME.as_bytes()).unwrap();
        let mut header = bytes[..ADDRESS_BYTES * 2].to_vec();
        let mut digipeater: [u8; ADDRESS_BYTES] = bytes[ADDRESS_BYTES * 2..ADDRESS_BYTES * 3]
            .try_into()
            .unwrap();
        digipeater[ADDRESS_BYTES - 1] &= !1;
        for _ in 1..MAX_DIGIPEATERS {
            header.extend(digipeater);
        }
        digipeater[ADDRESS_BYTES - 1] |= 1;
        header.extend(digipeater);
        header.extend(UI_NO_LAYER_3);
        let mut body = header.clone();
        body.extend([b'a'; MAX_INFORMATION_BYTES]);

        let max = from_body(body);
        let mut stream: Vec<_> = wire_bits(FLAG).collect();
        push_stuffed(&mut stream, &max);
        stream.extend(wire_bits(FLAG));
        let frames = receive(stream);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].digipeaters.len(), MAX_DIGIPEATERS);

        let mut too_long = header.clone();
        too_long.extend([b'a'; MAX_INFORMATION_BYTES + 1]);
        assert!(Frame::decode(&from_body(too_long)).is_none());
        assert!(Frame::decode(&from_body(header)).is_none());

        let address_bytes = (2 + MAX_DIGIPEATERS) * ADDRESS_BYTES;
        let mut nine = max[..address_bytes].to_vec();
        nine[address_bytes - 1] &= !1;
        nine.extend(digipeater);
        nine.extend(UI_NO_LAYER_3);
        nine.push(b'a');
        assert!(Frame::decode(&from_body(nine)).is_none());
    }

    #[test]
    fn rejects_valid_fcs_with_unsupported_control_pid_or_malformed_addresses() {
        let bytes = HEXLOWER.decode(TEXT_FRAME.as_bytes()).unwrap();
        for (index, value) in [(5, 0x41), (6, 0x61), (20, 0xe2), (21, 0x13), (22, 0xcc)] {
            let mut body = bytes[..bytes.len() - 2].to_vec();
            body[index] = value;
            assert!(Frame::decode(&from_body(body)).is_none(), "octet {index}");
        }
    }
}
