//! Recover the LTE broadcast transport block from soft PBCH bits.
//!
//! [TS 36.212 v18.0.0] §§5.1.1, 5.1.3.1, 5.1.4.2 and 5.3.1 define the
//! CRC, antenna masks, convolutional code and rate matching.
//! [TS 36.211 v18.0.1] §§6.6 and 7.2 specify PBCH scrambling.
//!
//! [TS 36.212 v18.0.0]: https://www.etsi.org/deliver/etsi_ts/136200_136299/136212/18.00.00_60/ts_136212v180000p.pdf
//! [TS 36.211 v18.0.1]: https://www.etsi.org/deliver/etsi_ts/136200_136299/136211/18.00.01_60/ts_136211v180001p.pdf

use crate::dsp::gold;

const MIB_BITS: usize = 24;
const CRC_BITS: usize = 16;
const CODE_INPUT_BITS: usize = MIB_BITS + CRC_BITS;
const FRAME_BITS: usize = 480;
const FRAMES_PER_BLOCK: usize = 4;
const STATE_COUNT: usize = 64;
const CRC16_POLYNOMIAL: u16 = 0x1021;
// Figure 5.1.3-1 represents the current input at the left of the delay line.
// Here it is the least significant register bit.
const GENERATORS: [usize; 3] = [0x6d, 0x4f, 0x57];
const PERMUTED_COLUMNS: [usize; 32] = [
    1, 17, 9, 25, 5, 21, 13, 29, 3, 19, 11, 27, 7, 23, 15, 31, 0, 16, 8, 24, 4, 20, 12, 28, 2, 18,
    10, 26, 6, 22, 14, 30,
];
const STREAM_ROWS: usize = CODE_INPUT_BITS.div_ceil(PERMUTED_COLUMNS.len());
const STREAM_PADDING: usize = STREAM_ROWS * PERMUTED_COLUMNS.len() - CODE_INPUT_BITS;
const STREAM_ORDER: [usize; CODE_INPUT_BITS] = {
    let mut order = [0; CODE_INPUT_BITS];
    let mut output = 0;
    let mut column = 0;
    while column < PERMUTED_COLUMNS.len() {
        let mut row = 0;
        while row < STREAM_ROWS {
            let padded = row * PERMUTED_COLUMNS.len() + PERMUTED_COLUMNS[column];
            if padded >= STREAM_PADDING {
                order[output] = padded - STREAM_PADDING;
                output += 1;
            }
            row += 1;
        }
        column += 1;
    }
    order
};

// The two predecessors for a destination differ in the oldest register bit.
// Every generator uses that bit, so their three output bits are complements.
const PARITY_FROM_LOWER_PREDECESSOR: [usize; STATE_COUNT] = {
    let mut parities = [0; STATE_COUNT];
    let mut state = 0;
    while state < STATE_COUNT {
        let mut stream = 0;
        while stream < GENERATORS.len() {
            let parity = (state & GENERATORS[stream]).count_ones() as usize & 1;
            parities[state] |= parity << stream;
            stream += 1;
        }
        state += 1;
    }
    parities
};

/// Decode public MIB bits from consecutive normal-prefix PBCH quarters.
///
/// Positive soft metrics favor zero. The quarters must fit in the same 40 ms
/// block at the hypothesized phase. Returns the 24 bits in transmission order
/// when the best tail-biting path passes the CRC for the given antenna count.
/// Erased, non-finite or invalid inputs produce no result.
pub(super) fn seen_bits(
    llrs: &[[f32; FRAME_BITS]],
    first_phase: usize,
    pci: u16,
    antenna_ports: u8,
) -> Option<[u8; MIB_BITS]> {
    if first_phase >= FRAMES_PER_BLOCK
        || llrs.is_empty()
        || llrs.len() > FRAMES_PER_BLOCK - first_phase
        || pci > 503
    {
        return None;
    }
    let mask = match antenna_ports {
        1 => 0,
        2 => u16::MAX,
        4 => 0x5555,
        _ => return None,
    };
    let mut mother = [[0.0; GENERATORS.len()]; CODE_INPUT_BITS];
    let offset = first_phase * FRAME_BITS;
    let signs = gold(u32::from(pci)).skip(offset);
    for (index, (&llr, bit)) in llrs.iter().flatten().zip(signs).enumerate() {
        if !llr.is_finite() {
            return None;
        }
        // The circular buffer repeats the three streams after removing padding.
        let circular = (offset + index) % (CODE_INPUT_BITS * GENERATORS.len());
        let stream = circular / CODE_INPUT_BITS;
        let bit_index = STREAM_ORDER[circular % CODE_INPUT_BITS];
        let llr = f64::from(llr);
        mother[bit_index][stream] += if bit == 0 { llr } else { -llr };
    }
    if mother.iter().flatten().all(|&llr| llr == 0.0) {
        return None;
    }

    let path = decode_tail_biting(&mother);
    let mib = std::array::from_fn(|index| ((path >> (CODE_INPUT_BITS - 1 - index)) & 1) as u8);
    ((path as u16 ^ mask) == crc16(&mib)).then_some(mib)
}

fn crc16(bits: &[u8; MIB_BITS]) -> u16 {
    let mut crc = 0;
    for &bit in bits {
        let feedback = (crc >> (CRC_BITS - 1)) ^ u16::from(bit);
        crc <<= 1;
        if feedback != 0 {
            crc ^= CRC16_POLYNOMIAL;
        }
    }
    crc
}

#[derive(Clone, Copy)]
struct Path {
    metric: f64,
    // The initial state precedes the decisions in chronological bit order.
    bits: u64,
}

impl Path {
    const UNREACHABLE: Self = Self {
        metric: f64::NEG_INFINITY,
        bits: 0,
    };

    fn from_state(state: usize) -> Self {
        Self {
            metric: 0.0,
            bits: state as u64,
        }
    }
}

type BranchMetrics = [f64; 1 << GENERATORS.len()];

fn advance_trellis(
    steps: &[BranchMetrics; CODE_INPUT_BITS],
    mut paths: [Path; STATE_COUNT],
) -> [Path; STATE_COUNT] {
    for branches in steps {
        paths = std::array::from_fn(|state| {
            let lower = paths[state / 2];
            let upper = paths[state / 2 + STATE_COUNT / 2];
            let branch = branches[PARITY_FROM_LOWER_PREDECESSOR[state]];
            let lower_metric = lower.metric + branch;
            let upper_metric = upper.metric - branch;
            let (metric, bits) = if lower_metric >= upper_metric {
                (lower_metric, lower.bits)
            } else {
                (upper_metric, upper.bits)
            };
            Path {
                metric,
                bits: (bits << 1) | (state as u64 & 1),
            }
        });
    }
    paths
}

fn decode_tail_biting(llrs: &[[f64; 3]; CODE_INPUT_BITS]) -> u64 {
    let steps = llrs.map(|row| {
        std::array::from_fn(|parities| {
            row.into_iter()
                .enumerate()
                .map(|(stream, llr)| {
                    if parities & (1 << stream) == 0 {
                        llr
                    } else {
                        -llr
                    }
                })
                .sum()
        })
    });
    // An unrestricted path supplies an exact upper bound at each end state.
    // It is also a candidate if its initial state already equals its end state.
    let unrestricted = advance_trellis(&steps, std::array::from_fn(Path::from_state));
    let mut end_states: [_; STATE_COUNT] = std::array::from_fn(|state| state);
    end_states.sort_unstable_by(|&a, &b| {
        unrestricted[b]
            .metric
            .total_cmp(&unrestricted[a].metric)
            .then_with(|| a.cmp(&b))
    });
    let mut best = Path::UNREACHABLE;
    for state in end_states {
        let bound = unrestricted[state];
        if bound.metric <= best.metric {
            break;
        }
        let path = if bound.bits >> CODE_INPUT_BITS == state as u64 {
            bound
        } else {
            let mut initial = [Path::UNREACHABLE; STATE_COUNT];
            initial[state] = Path::from_state(state);
            advance_trellis(&steps, initial)[state]
        };
        if path.metric > best.metric {
            best = path;
        }
    }
    best.bits
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // Packed MSB first. A separate forward encoder applies the encoder diagram
    // directly to cyclic indices, using CRC-16/XMODEM with initial value zero.
    // MIB 0x6a7000, masked CRC 0xab0c, PCI 235, two antenna ports.
    pub(in crate::lte::pbch) const REFERENCE: [u8; FRAME_BITS * FRAMES_PER_BLOCK / 8] = [
        0xe0, 0x2e, 0xef, 0xa3, 0x2d, 0xa7, 0xf8, 0x70, 0x3d, 0x90, 0x82, 0xa9, 0xb7, 0x2d, 0xba,
        0x32, 0x92, 0x5f, 0x45, 0x1d, 0xeb, 0xbd, 0x56, 0xc7, 0xdb, 0x59, 0x6b, 0xb2, 0xbd, 0x98,
        0xd0, 0xcb, 0x92, 0x07, 0x38, 0x1d, 0x1a, 0xd6, 0x22, 0xd9, 0xc3, 0xea, 0x99, 0xfb, 0x5c,
        0x79, 0x13, 0xf9, 0x47, 0x3a, 0xc8, 0xbb, 0x7c, 0x09, 0x42, 0x3b, 0x10, 0x51, 0xf3, 0xfc,
        0xab, 0x88, 0x6b, 0xd0, 0x6b, 0x94, 0x50, 0x9c, 0x41, 0x62, 0x45, 0x2c, 0x4f, 0x8f, 0x53,
        0x42, 0xd9, 0xa7, 0x57, 0x43, 0xed, 0x08, 0x60, 0x74, 0xc1, 0x9a, 0x1c, 0x0a, 0x06, 0x40,
        0xf1, 0xd8, 0xa0, 0x96, 0x26, 0xac, 0x0a, 0x00, 0xee, 0x14, 0x0c, 0x0c, 0xed, 0x23, 0x5c,
        0x4b, 0x67, 0x87, 0x14, 0x7e, 0x00, 0x90, 0x8e, 0x92, 0x48, 0xcb, 0x9a, 0x02, 0x58, 0x02,
        0xd7, 0x12, 0x7c, 0x79, 0x4d, 0x51, 0x02, 0x41, 0xc2, 0xdd, 0x96, 0x35, 0x8b, 0x4b, 0x73,
        0x8b, 0x4b, 0xa4, 0x64, 0x4b, 0xdb, 0x56, 0x22, 0xd9, 0xd0, 0x51, 0x29, 0x14, 0x27, 0xa9,
        0xe8, 0x81, 0xa1, 0xde, 0x72, 0xd2, 0xf4, 0xcd, 0x85, 0xe5, 0xa4, 0xdf, 0xf6, 0xb5, 0x40,
        0x93, 0x6f, 0x28, 0x67, 0x6b, 0xd4, 0x4a, 0x00, 0x45, 0x83, 0x52, 0xe8, 0x0c, 0x13, 0xd0,
        0x26, 0x18, 0x75, 0x0b, 0xf3, 0x7a, 0x6f, 0xd8, 0x5a, 0x7c, 0x22, 0xa5, 0x73, 0x80, 0x35,
        0x4e, 0x9a, 0x40, 0x8a, 0x03, 0x26, 0x15, 0xbe, 0x94, 0x76, 0x1b, 0x6b, 0x18, 0xf5, 0x4c,
        0xdf, 0x49, 0x4f, 0x4f, 0xa2, 0xa1, 0xe6, 0x0b, 0x84, 0xf9, 0x23, 0x6f, 0x69, 0x19, 0x86,
        0x1c, 0x24, 0x46, 0x95, 0x94, 0x4a, 0x0b, 0x71, 0xc8, 0xca, 0x48, 0xee, 0x1e, 0xe1, 0xe4,
    ];
    const PCI: u16 = 235;
    const MIB: u32 = 0x6a_7000;

    fn expected() -> [u8; MIB_BITS] {
        std::array::from_fn(|i| ((MIB >> (MIB_BITS - 1 - i)) & 1) as u8)
    }

    fn unpack_quarter(bytes: &[u8]) -> [f32; FRAME_BITS] {
        std::array::from_fn(|i| {
            if bytes[i / 8] & (0x80 >> (i % 8)) == 0 {
                1.0
            } else {
                -1.0
            }
        })
    }

    fn reference() -> [[f32; FRAME_BITS]; FRAMES_PER_BLOCK] {
        std::array::from_fn(|phase| unpack_quarter(&REFERENCE[phase * (FRAME_BITS / 8)..]))
    }

    #[test]
    fn recovers_known_mib_from_each_phase_and_complete_block() {
        let llrs = reference();
        for phase in 0..FRAMES_PER_BLOCK {
            assert_eq!(
                seen_bits(&llrs[phase..phase + 1], phase, PCI, 2),
                Some(expected())
            );
        }
        assert_eq!(seen_bits(&llrs, 0, PCI, 2), Some(expected()));
    }

    #[test]
    fn recovers_one_and_four_port_crc_masks() {
        let one_port = [
            0xa8, 0xfc, 0x7b, 0x07, 0x44, 0xbf, 0x3e, 0xe4, 0xb0, 0xb9, 0x9a, 0x7b, 0x23, 0xa0,
            0x93, 0x7a, 0x40, 0xcb, 0xe1, 0x74, 0xf3, 0x7b, 0xc2, 0x4a, 0xf2, 0x41, 0xb9, 0x26,
            0x30, 0xb1, 0x98, 0x19, 0x06, 0xa3, 0x51, 0x05, 0xdc, 0x42, 0xaf, 0xf0, 0xdb, 0x38,
            0x0d, 0x76, 0x75, 0x31, 0xc1, 0x6d, 0xe3, 0x53, 0xd0, 0x7d, 0xe8, 0x84, 0x6b, 0x23,
            0xc2, 0xc5, 0x7e, 0xd5,
        ];
        let four_ports = [
            0xe0, 0x3a, 0xeb, 0x2f, 0x44, 0xf7, 0xf8, 0x74, 0xb1, 0xf9, 0xc8, 0xfb, 0x22, 0x05,
            0xba, 0x32, 0x86, 0x5b, 0xc9, 0x74, 0xbb, 0xbd, 0x52, 0x4b, 0xb2, 0x13, 0x39, 0x27,
            0x95, 0x98, 0xd0, 0xdf, 0x96, 0x8b, 0x51, 0x4d, 0x1a, 0xd2, 0xae, 0xb0, 0x89, 0xb8,
            0x0c, 0xd3, 0x5c, 0x79, 0x07, 0xfd, 0xcb, 0x53, 0x98, 0xbb, 0x78, 0x85, 0x2b, 0x71,
            0x42, 0xc4, 0xdb, 0xfc,
        ];
        for (ports, packed) in [(1, one_port), (4, four_ports)] {
            assert_eq!(
                seen_bits(&[unpack_quarter(&packed)], 0, PCI, ports),
                Some(expected())
            );
        }
    }

    #[test]
    fn finds_best_closed_path_when_the_open_path_has_different_endpoints() {
        // The same 40 input bits encoded from state 63 instead of state 12.
        // The observed bits match that open path exactly, but the mismatched
        // initial state only affects the first six steps, which are attenuated.
        let streams = [0x005b_5840_935a_u64, 0x0015_33c0_c384, 0x000e_dac0_dc55];
        let llrs = std::array::from_fn(|i| {
            let weight = if i < 6 { 0.125 } else { 1.0 };
            streams.map(|stream| {
                if stream & (1 << (CODE_INPUT_BITS - 1 - i)) == 0 {
                    weight
                } else {
                    -weight
                }
            })
        });
        let closed = (12_u64 << CODE_INPUT_BITS) | 0x006a_7000_ab0c;
        assert_eq!(decode_tail_biting(&llrs), closed);
    }

    #[test]
    fn weak_conflicting_quarter_can_be_resolved_by_soft_combination() {
        let mut llrs = reference();
        llrs[1] = llrs[1].map(|llr| -llr * 0.25);
        assert_eq!(seen_bits(&llrs[1..2], 1, PCI, 2), None);
        assert_eq!(seen_bits(&llrs[1..3], 1, PCI, 2), Some(expected()));
    }

    #[test]
    fn rejects_mismatched_crc_scrambling_and_invalid_input() {
        let mut llrs = reference();
        assert_eq!(seen_bits(&llrs[..1], 0, PCI, 1), None);
        assert_eq!(seen_bits(&llrs[..1], 0, PCI, 4), None);
        assert_eq!(seen_bits(&llrs[..1], 1, PCI, 2), None);
        assert_eq!(seen_bits(&llrs[..1], 0, PCI + 1, 2), None);
        assert_eq!(seen_bits(&llrs, 1, PCI, 2), None);
        assert_eq!(seen_bits(&[], 0, PCI, 2), None);
        assert_eq!(seen_bits(&llrs[..1], 0, PCI, 3), None);
        assert_eq!(seen_bits(&[[0.0; FRAME_BITS]], 0, PCI, 1), None);
        llrs[0][0] = f32::INFINITY;
        assert_eq!(seen_bits(&llrs[..1], 0, PCI, 2), None);
    }
}
