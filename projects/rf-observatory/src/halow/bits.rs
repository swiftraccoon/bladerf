//! Single-stream 2 MHz BCC bit processing for the S1G SIG and DATA fields.

use rustfft::num_complex::Complex32;

const DATA_TONES: usize = 52;
const MAX_BITS_PER_TONE: usize = 8;
const CONSTRAINT_STATES: usize = 64;
const SERVICE_BITS: usize = 8;
const BCC_TAIL_BITS: usize = 6;
const MPDU_FCS_BYTES: usize = 4;
const DELIMITER_BYTES: usize = 4;

/// Largest symbol count representable by the aggregate SIG length field.
pub(super) const MAX_DATA_SYMBOLS: usize = 511;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataLength {
    Octets(usize),
    Aggregate,
}

/// Supported fields from a CRC-verified S1G SIG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sig {
    pub(super) mcs: u8,
    pub(super) short_gi: bool,
    pub(super) traveling_pilots: bool,
    pub(super) data_symbols: usize,
    length: DataLength,
}

#[derive(Clone, Copy)]
enum CodeRate {
    Half,
    TwoThirds,
    ThreeQuarters,
    FiveSixths,
}

impl CodeRate {
    const fn puncturing(self) -> &'static [[bool; 2]] {
        // The BCC output order is A, B. A false entry is an erasure at the decoder.
        match self {
            Self::Half => &[[true, true]],
            Self::TwoThirds => &[[true, true], [true, false]],
            Self::ThreeQuarters => &[[true, true], [true, false], [false, true]],
            Self::FiveSixths => &[
                [true, true],
                [true, false],
                [false, true],
                [true, false],
                [false, true],
            ],
        }
    }

    const fn data_bits(self, coded_bits: usize) -> usize {
        match self {
            Self::Half => coded_bits / 2,
            Self::TwoThirds => coded_bits * 2 / 3,
            Self::ThreeQuarters => coded_bits * 3 / 4,
            Self::FiveSixths => coded_bits * 5 / 6,
        }
    }
}

#[derive(Clone, Copy)]
struct Mode {
    bits_per_tone: usize,
    rate: CodeRate,
}

impl Mode {
    fn coded_bits(self) -> usize {
        DATA_TONES * self.bits_per_tone
    }

    fn data_bits(self) -> usize {
        self.rate.data_bits(self.coded_bits())
    }
}

fn mode(mcs: u8) -> Option<Mode> {
    use CodeRate::{FiveSixths, Half, ThreeQuarters, TwoThirds};

    // IEEE 802.11ah Table 23-42 gives MCS 0–8 for 2 MHz and one spatial stream.
    let (bits_per_tone, rate) = match mcs {
        0 => (1, Half),
        1 => (2, Half),
        2 => (2, ThreeQuarters),
        3 => (4, Half),
        4 => (4, ThreeQuarters),
        5 => (6, TwoThirds),
        6 => (6, ThreeQuarters),
        7 => (6, FiveSixths),
        8 => (8, ThreeQuarters),
        _ => return None,
    };
    Some(Mode {
        bits_per_tone,
        rate,
    })
}

/// Decode the two SIG symbols in on-air tone order. Positive soft values favor one.
pub(super) fn decode_sig(soft: &[[f32; 48]; 2], long_format: bool) -> Option<Sig> {
    if !soft.iter().flatten().all(|sample| sample.is_finite()) {
        return None;
    }
    let mut coded = [[0.0; 48]; 2];
    for (wire, original) in soft.iter().zip(&mut coded) {
        deinterleave(wire, original, 16, 1);
    }
    let bits = decode_bcc(coded.as_flattened(), 48, CodeRate::Half)?;
    if crc_bits(bits[..38].iter().copied(), 4, 0x03) != bits_msb(bits[38..42].iter().copied())
        || bits[1] != 0
        || bits[3..7].iter().any(|&bit| bit != 0)
        || bits[17] != 0
    {
        return None;
    }

    // The format bits distinguish S1G_SHORT from S1G_LONG SU and exclude NDP and MU SIGs.
    let traveling_pilots = if long_format {
        if bits[0] != 0 || bits[36] == 0 {
            return None;
        }
        bits[37] != 0
    } else {
        if bits[0] == 0 || bits[37] != 0 {
            return None;
        }
        bits[36] != 0
    };
    let mcs = bits_lsb(&bits[19..23]);
    let data_bits = mode(mcs)?.data_bits();
    let advertised = usize::from(bits_lsb(&bits[25..33])) | (usize::from(bits[33]) << 8);
    let (length, data_symbols) = if bits[24] != 0 {
        (DataLength::Aggregate, advertised)
    } else {
        (
            DataLength::Octets(advertised),
            (SERVICE_BITS + advertised * 8 + BCC_TAIL_BITS).div_ceil(data_bits),
        )
    };
    if !(1..=MAX_DATA_SYMBOLS).contains(&data_symbols)
        || matches!(length, DataLength::Octets(octets) if octets < MPDU_FCS_BYTES)
    {
        return None;
    }
    Some(Sig {
        mcs,
        short_gi: bits[16] != 0,
        traveling_pilots,
        data_symbols,
        length,
    })
}

/// Recover DATA and visit each FCS-verified MPDU, including its four FCS octets.
///
/// Returns the count of visited MPDUs. Missing tones or an undecodable bitstream
/// return `None`. A malformed later aggregate delimiter preserves prior results.
pub(super) fn decode_data(
    sig: Sig,
    tones: &[[Complex32; DATA_TONES]],
    mut receive: impl FnMut(&[u8]),
) -> Option<usize> {
    if tones.len() != sig.data_symbols || !(1..=MAX_DATA_SYMBOLS).contains(&tones.len()) {
        return None;
    }
    let mode = mode(sig.mcs)?;
    let demapper = Demapper::new(mode.bits_per_tone);
    let mut coded = vec![0.0; mode.coded_bits() * sig.data_symbols];
    let mut wire = [0.0; DATA_TONES * MAX_BITS_PER_TONE];
    let wire = &mut wire[..mode.coded_bits()];
    for (symbol, original) in tones.iter().zip(coded.chunks_exact_mut(mode.coded_bits())) {
        for (&tone, output) in symbol.iter().zip(wire.chunks_exact_mut(mode.bits_per_tone)) {
            demapper.soft_bits(tone, output)?;
        }
        deinterleave(wire, original, 13, mode.bits_per_tone);
    }
    let bits = decode_bcc(&coded, mode.data_bits() * sig.data_symbols, mode.rate)?;
    let payload = descramble(&bits[..bits.len() - BCC_TAIL_BITS])?;
    match sig.length {
        DataLength::Octets(octets) => {
            let frame = payload.get(..octets)?;
            if fcs_valid(frame) {
                receive(frame);
                return Some(1);
            }
            if !octets.is_multiple_of(4) {
                return Some(0);
            }
            // Some implementations advertise the aligned PSDU extent in SIG.
            for pad in 1..=3 {
                let frame = &frame[..octets - pad];
                if fcs_valid(frame) {
                    receive(frame);
                    return Some(1);
                }
            }
            Some(0)
        }
        DataLength::Aggregate => visit_aggregate(&payload, receive),
    }
}

fn deinterleave(wire: &[f32], original: &mut [f32], columns: usize, bits_per_tone: usize) {
    let rows = wire.len() / columns;
    let s = (bits_per_tone / 2).max(1);
    for (index, soft) in original.iter_mut().enumerate() {
        let first = rows * (index % columns) + index / columns;
        let second = s * (first / s) + (first + wire.len() - first / rows) % s;
        *soft = wire[second];
    }
}

struct Demapper {
    axis_bits: usize,
    amplitudes: [f32; 16],
}

impl Demapper {
    fn new(bits_per_tone: usize) -> Self {
        let axis_bits = (bits_per_tone / 2).max(1);
        let levels = 1 << axis_bits;
        let normalization: f32 = match bits_per_tone {
            1 => 1.0,
            2 => 2.0,
            4 => 10.0,
            6 => 42.0,
            _ => 170.0,
        };
        let spacing = 2.0 / normalization.sqrt();
        let mut amplitudes = [0.0; 16];
        let center = (levels - 1) as f32 / 2.0;
        for (level, amplitude) in amplitudes[..levels].iter_mut().enumerate() {
            *amplitude = (level as f32 - center) * spacing;
        }
        Self {
            axis_bits,
            amplitudes,
        }
    }

    fn soft_bits(&self, tone: Complex32, output: &mut [f32]) -> Option<()> {
        let axes = [tone.re, tone.im];
        for (value, bits) in axes
            .into_iter()
            .zip(output.chunks_exact_mut(self.axis_bits))
        {
            if !value.is_finite() {
                return None;
            }
            let value = value.clamp(-64.0, 64.0);
            let mut distance = [[f32::INFINITY; 2]; 4];
            for (level, &amplitude) in self.amplitudes[..1 << self.axis_bits].iter().enumerate() {
                let gray = level ^ (level >> 1);
                let error = (value - amplitude).powi(2);
                for (bit, nearest) in distance[..self.axis_bits].iter_mut().enumerate() {
                    let label = (gray >> (self.axis_bits - 1 - bit)) & 1;
                    nearest[label] = nearest[label].min(error);
                }
            }
            for (bit, [zero, one]) in bits.iter_mut().zip(distance) {
                // In square QAM the other axis cancels from the max-log metric.
                *bit = zero - one;
            }
        }
        Some(())
    }
}

const BRANCH_LABELS: [usize; 128] = branch_labels();

const fn branch_labels() -> [usize; 128] {
    let mut labels = [0; 128];
    let mut register = 0;
    while register < labels.len() {
        let a = (register & 0o133_usize).count_ones() as usize & 1;
        let b = (register & 0o171_usize).count_ones() as usize & 1;
        labels[register] = (a << 1) | b;
        register += 1;
    }
    labels
}

fn decode_bcc(coded: &[f32], data_bits: usize, rate: CodeRate) -> Option<Vec<u8>> {
    let mut wire = coded.iter().copied();
    let mut metrics = [f32::NEG_INFINITY; CONSTRAINT_STATES];
    metrics[0] = 0.0;
    let mut decisions = Vec::with_capacity(data_bits);
    for [take_a, take_b] in rate.puncturing().iter().copied().cycle().take(data_bits) {
        let a = if take_a { wire.next()? } else { 0.0 };
        let b = if take_b { wire.next()? } else { 0.0 };
        let scores = [-a - b, b - a, a - b, a + b];
        let mut next = [0.0; CONSTRAINT_STATES];
        let mut decision = 0_u64;
        for (state, metric) in next.iter_mut().enumerate() {
            let register = state << 1;
            let predecessor = register & (CONSTRAINT_STATES - 1);
            let even = metrics[predecessor] + scores[BRANCH_LABELS[register]];
            let odd = metrics[predecessor + 1] + scores[BRANCH_LABELS[register + 1]];
            if odd > even {
                *metric = odd;
                decision |= 1 << state;
            } else {
                *metric = even;
            }
        }
        decisions.push(decision);
        metrics = next;
    }
    if wire.next().is_some() {
        return None;
    }

    // The six BCC tail bits terminate both fields in state zero.
    let mut state = 0;
    let mut bits = vec![0; data_bits];
    for (bit, decision) in bits.iter_mut().zip(decisions).rev() {
        *bit = (state >> 5) as u8;
        let oldest = ((decision >> state) & 1) as usize;
        state = ((state << 1) | oldest) & (CONSTRAINT_STATES - 1);
    }
    Some(bits)
}

fn descramble(bits: &[u8]) -> Option<Vec<u8>> {
    // SERVICE starts with seven zero bits, exposing seven successive scrambler outputs.
    let mut register = bits_msb(bits.get(..7)?.iter().copied());
    if register == 0 {
        return None;
    }
    let payload = bits.get(SERVICE_BITS..)?;
    // Advance through the remaining SERVICE bit before reading the payload.
    scrambler_step(&mut register);
    let mut octets = Vec::with_capacity(payload.len() / 8);
    for chunk in payload.as_chunks::<8>().0 {
        let mut octet = 0;
        for (bit, &scrambled) in chunk.iter().enumerate() {
            octet |= (scrambler_step(&mut register) ^ scrambled) << bit;
        }
        octets.push(octet);
    }
    Some(octets)
}

fn scrambler_step(register: &mut u8) -> u8 {
    let feedback = ((*register >> 3) ^ (*register >> 6)) & 1;
    *register = ((*register << 1) | feedback) & 0x7f;
    feedback
}

fn visit_aggregate(mut payload: &[u8], mut receive: impl FnMut(&[u8])) -> Option<usize> {
    let mut verified = 0;
    while let Some(delimiter) = payload.first_chunk::<DELIMITER_BYTES>() {
        let extent = u16::from_le_bytes([delimiter[0], delimiter[1]]);
        let length = usize::from((extent >> 4) | ((extent & 0x0c) << 10));
        let delimiter_bits = (0..16).map(|bit| ((extent >> bit) & 1) as u8);
        if extent & 2 != 0
            || delimiter[3] != b'N'
            || crc_bits(delimiter_bits, 8, 0x07) != delimiter[2].reverse_bits()
        {
            return (verified != 0).then_some(verified);
        }
        let end = DELIMITER_BYTES + length;
        let Some(frame) = payload.get(DELIMITER_BYTES..end) else {
            return (verified != 0).then_some(verified);
        };
        if fcs_valid(frame) {
            receive(frame);
            verified += 1;
        }
        if extent & 1 != 0 {
            break;
        }
        let Some(remaining) = payload.get(end.next_multiple_of(4)..) else {
            break;
        };
        payload = remaining;
    }
    Some(verified)
}

fn bits_lsb(bits: &[u8]) -> u8 {
    bits_msb(bits.iter().copied().rev())
}

fn bits_msb(bits: impl IntoIterator<Item = u8>) -> u8 {
    bits.into_iter().fold(0, |value, bit| (value << 1) | bit)
}

fn crc_bits(bits: impl IntoIterator<Item = u8>, width: u32, polynomial: u8) -> u8 {
    let mask = u8::MAX >> (8 - width);
    let mut register = mask;
    for bit in bits {
        let feedback = ((register >> (width - 1)) ^ bit) & 1;
        register = ((register << 1) & mask) ^ (feedback * polynomial);
    }
    register ^ mask
}

fn fcs_valid(frame: &[u8]) -> bool {
    let Some((body, fcs)) = frame.split_last_chunk::<MPDU_FCS_BYTES>() else {
        return false;
    };
    let mut crc = u32::MAX;
    for &octet in body {
        crc ^= u32::from(octet);
        for _ in 0..8 {
            crc = (crc >> 1) ^ ((crc & 1) * 0xedb8_8320);
        }
    }
    !crc == u32::from_le_bytes(*fcs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1g_short_sig_uses_two_independent_interleavers() {
        // Hard SIG decisions from the published 2 MHz MCS 2 capture.
        let wire = [
            [0x90_u8, 0x4b, 0x65, 0x02, 0x8b, 0x41],
            [0xbe, 0x99, 0x8a, 0x2f, 0x6c, 0x86],
        ];
        let soft = wire.map(|symbol| {
            std::array::from_fn(|bit| {
                if symbol[bit / 8] & (0x80 >> (bit % 8)) == 0 {
                    -1.0
                } else {
                    1.0
                }
            })
        });
        assert_eq!(
            decode_sig(&soft, false),
            Some(Sig {
                mcs: 2,
                short_gi: false,
                traveling_pilots: false,
                data_symbols: 12,
                length: DataLength::Octets(112),
            })
        );
        assert_eq!(decode_sig(&soft, true), None);
    }

    #[test]
    fn aggregate_visits_only_frames_with_valid_fcs() {
        let valid_frame = [b"123456789".as_slice(), &[0x26, 0x39, 0xf4, 0xcb]].concat();
        let mut corrupt_frame = valid_frame.clone();
        corrupt_frame[0] ^= 1;
        let full = [
            &[0xd0, 0, 0xfd, b'N'],
            valid_frame.as_slice(),
            &[0, 0, 0],
            &[0xd1, 0, 0x90, b'N'],
            corrupt_frame.as_slice(),
        ]
        .concat();
        let mut visited = Vec::new();
        assert_eq!(
            visit_aggregate(&full, |frame| visited.push(frame.to_owned())),
            Some(1)
        );
        assert_eq!(visited, [valid_frame]);
    }
}
