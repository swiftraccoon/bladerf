//! Demodulate the four normal-prefix LTE PBCH symbols on the 1.92 MSps grid.
//!
//! [ETSI TS 136 211 V18.0.1] clauses 6.3.4, 6.6, 6.10.1 and 6.12 define
//! transmit diversity, the broadcast resource map, CRS and symbol timing.
//!
//! [ETSI TS 136 211 V18.0.1]: https://www.etsi.org/deliver/etsi_ts/136200_136299/136211/18.00.01_60/ts_136211v180001p.pdf

use std::f32::consts::{FRAC_1_SQRT_2, TAU};
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::dsp::gold;
use crate::lte::pss::SYMBOL_SAMPLES;

const SHORT_PREFIX: usize = 9;
const LONG_PREFIX: usize = 10;
const SUBCARRIER_SPACING_HZ: f32 = 15_000.0;
const WINDOW_ADVANCE: usize = 3;
const SUBCARRIERS: usize = 72;
const HALF_SUBCARRIERS: usize = SUBCARRIERS / 2;
const REFERENCE_STRIDE: usize = 6;
const REFERENCE_COUNT: usize = SUBCARRIERS / REFERENCE_STRIDE;
const MAX_RESOURCE_BLOCKS: usize = 110;
const PBCH_SYMBOLS: usize = 4;
const DATA_TONES: usize = 240;
const CODED_BITS: usize = DATA_TONES * 2;

type Tones = [Complex32; SUBCARRIERS];
type References = [Complex32; REFERENCE_COUNT];

/// Soft metrics for one subframe-0 broadcast block.
pub(super) struct Block {
    /// One metric per transmitted bit, before physical-layer descrambling.
    /// The rows correspond to one, two and four transmit antenna ports.
    pub(super) llrs: [[f32; CODED_BITS]; 3],
    /// Carrier offset inferred from the cyclic prefixes in hertz.
    pub(super) cfo_hz: f32,
}

/// Reuse the 128-point transform across qualified synchronization observations.
pub(super) struct Demodulator {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl Demodulator {
    pub(super) fn new() -> Self {
        let fft = FftPlanner::new().plan_fft_forward(SYMBOL_SAMPLES);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        Self { fft, scratch }
    }

    /// Extract PBCH after a subframe-0 PSS at its first useful sample.
    ///
    /// The caller supplies a confirmed FDD cell ID and its channelized stream.
    /// The returned metrics include three transmit-diversity interpretations.
    /// An incomplete broadcast block, invalid cell ID or unavailable carrier
    /// estimate returns `None`.
    pub(super) fn extract(
        &mut self,
        iq: &[Complex32],
        pss_start: usize,
        pci: u16,
    ) -> Option<Block> {
        if pci > 503 {
            return None;
        }
        // PSS is the last symbol of slot 0. Slot 1 starts with the long CP.
        let first = pss_start.checked_add(SYMBOL_SAMPLES + LONG_PREFIX)?;
        let last = first.checked_add(PBCH_SYMBOLS * (SYMBOL_SAMPLES + SHORT_PREFIX))?;
        let starts = std::array::from_fn::<_, PBCH_SYMBOLS, _>(|symbol| {
            first + symbol * (SYMBOL_SAMPLES + SHORT_PREFIX)
        });
        let cfo_hz = estimate_carrier_offset(iq, &starts)?;
        let mut grid = [[Complex32::default(); SUBCARRIERS]; PBCH_SYMBOLS];
        for (row, start) in grid.iter_mut().zip(starts) {
            *row = self.transform(iq, start, first, cfo_hz)?;
        }
        let last_row = self.transform(iq, last, first, cfo_hz);
        let channels = slot_channels(&grid, last_row.as_ref(), pci);
        Some(Block {
            llrs: equalize(&grid, &channels, usize::from(pci) % REFERENCE_STRIDE),
            cfo_hz,
        })
    }

    fn transform(
        &mut self,
        iq: &[Complex32],
        useful_start: usize,
        origin: usize,
        cfo_hz: f32,
    ) -> Option<Tones> {
        // Stay inside the cyclic prefix to absorb a small timing error. The
        // resulting phase slope is present in the same-window CRS estimates.
        let start = useful_start.checked_sub(WINDOW_ADVANCE)?;
        let samples = iq.get(start..)?.first_chunk::<SYMBOL_SAMPLES>()?;
        let phase_step = -TAU * cfo_hz / (SUBCARRIER_SPACING_HZ * SYMBOL_SAMPLES as f32);
        let initial_phase = phase_step * ((useful_start - origin) as f32 - WINDOW_ADVANCE as f32);
        let mut oscillator = Complex32::from_polar(1.0, initial_phase);
        let step = Complex32::from_polar(1.0, phase_step);
        let mut spectrum = *samples;
        for sample in &mut spectrum {
            *sample *= oscillator;
            oscillator *= step;
        }
        self.fft
            .process_with_scratch(&mut spectrum, &mut self.scratch);
        Some(std::array::from_fn(|index| {
            spectrum[fft_bin(index)] / SYMBOL_SAMPLES as f32
        }))
    }
}

fn estimate_carrier_offset(iq: &[Complex32], starts: &[usize]) -> Option<f32> {
    let mut correlation = Complex32::default();
    for &start in starts {
        // Ignore the ends of each prefix, where timing error or multipath can
        // insert samples from an adjacent symbol.
        let cp_start = start.checked_sub(6)?;
        let repeat_start = cp_start.checked_add(SYMBOL_SAMPLES)?;
        let prefix = iq.get(cp_start..)?.first_chunk::<5>()?;
        let repeat = iq.get(repeat_start..)?.first_chunk::<5>()?;
        correlation += prefix
            .iter()
            .zip(repeat)
            .map(|(prefix, repeat)| prefix.conj() * repeat)
            .sum::<Complex32>();
    }
    (correlation.norm_sqr() > 0.0).then(|| correlation.arg() * SUBCARRIER_SPACING_HZ / TAU)
}

const fn signed_bin(index: usize) -> i32 {
    index as i32 - HALF_SUBCARRIERS as i32 + (index >= HALF_SUBCARRIERS) as i32
}

const fn fft_bin(index: usize) -> usize {
    signed_bin(index).rem_euclid(SYMBOL_SAMPLES as i32) as usize
}

fn references(pci: u16, symbol: u32) -> References {
    // Slot n_s=1, N_CP=1. For the central six PRBs, the first m' is
    // N_RB^max - 6 regardless of the bandwidth advertised by this cell.
    let pci = u32::from(pci);
    let seed = (1 << 10) * (14 + symbol + 1) * (2 * pci + 1) + 2 * pci + 1;
    let mut sequence = gold(seed).skip(2 * (MAX_RESOURCE_BLOCKS - 6));
    std::array::from_fn(|_| {
        // The initialized Gold generator returns an infinite bit stream.
        let re = 1.0 - 2.0 * f32::from(sequence.next().unwrap());
        let im = 1.0 - 2.0 * f32::from(sequence.next().unwrap());
        Complex32::new(re, im) * FRAC_1_SQRT_2
    })
}

fn reference_channel(row: &Tones, expected: &References, shift: usize) -> Tones {
    let samples: References =
        std::array::from_fn(|index| row[shift + index * REFERENCE_STRIDE] * expected[index].conj());
    std::array::from_fn(|index| {
        let left = (index.saturating_sub(shift) / REFERENCE_STRIDE).min(REFERENCE_COUNT - 2);
        let right = left + 1;
        // Interpolate in actual FFT-bin distance across the omitted DC bin.
        let start = signed_bin(shift + left * REFERENCE_STRIDE);
        let end = signed_bin(shift + right * REFERENCE_STRIDE);
        let weight = (signed_bin(index) - start) as f32 / (end - start) as f32;
        if (0.0..=1.0).contains(&weight) {
            return samples[left] * (1.0 - weight) + samples[right] * weight;
        }
        // Retain the edge pilot's amplitude and continue the phase slope into
        // the outer tones, where a constant phase would bias diversity decoding.
        let phase_step = (samples[left].conj() * samples[right]).arg();
        if weight < 0.0 {
            samples[left] * Complex32::from_polar(1.0, phase_step * weight)
        } else {
            samples[right] * Complex32::from_polar(1.0, phase_step * (weight - 1.0))
        }
    })
}

fn slot_channels(
    grid: &[Tones; PBCH_SYMBOLS],
    last: Option<&Tones>,
    pci: u16,
) -> [[Tones; 4]; PBCH_SYMBOLS] {
    let shift = usize::from(pci) % REFERENCE_STRIDE;
    let opposite_shift = (shift + 3) % REFERENCE_STRIDE;
    let first_refs = references(pci, 0);
    let middle_refs = references(pci, 1);
    let port_zero = reference_channel(&grid[0], &first_refs, shift);
    let port_one = reference_channel(&grid[0], &first_refs, opposite_shift);
    let port_two = reference_channel(&grid[1], &middle_refs, opposite_shift);
    let port_three = reference_channel(&grid[1], &middle_refs, shift);
    let (last_zero, last_one) = last.map_or((port_zero, port_one), |row| {
        let expected = references(pci, 4);
        (
            reference_channel(row, &expected, opposite_shift),
            reference_channel(row, &expected, shift),
        )
    });
    std::array::from_fn(|symbol| {
        let weight = symbol as f32 / PBCH_SYMBOLS as f32;
        [
            std::array::from_fn(|bin| port_zero[bin] * (1.0 - weight) + last_zero[bin] * weight),
            std::array::from_fn(|bin| port_one[bin] * (1.0 - weight) + last_one[bin] * weight),
            port_two,
            port_three,
        ]
    })
}

#[derive(Clone, Copy, Default)]
struct Resource {
    received: Complex32,
    channel: [Complex32; 4],
}

fn diversity_pair(
    first: Resource,
    second: Resource,
    port_a: usize,
    port_b: usize,
) -> [Complex32; 2] {
    let a0 = first.channel[port_a];
    let b0 = first.channel[port_b];
    let a1 = second.channel[port_a].conj();
    let b1 = second.channel[port_b].conj();
    // For a symbol pair x0,x1, the observations are
    // y0 = a0*x0 - b0*conj(x1), conj(y1) = b1*x0 + a1*conj(x1).
    // Weight the inverse by its noise variance to avoid amplifying fades.
    let determinant = (a0 * a1 + b0 * b1).conj();
    let first_noise = a1.norm_sqr() + b0.norm_sqr();
    let second_noise = a0.norm_sqr() + b1.norm_sqr();
    let second_received = second.received.conj();
    let x0 = (first.received * a1 + second_received * b0) * determinant;
    let x1_conj = (second_received * a0 - first.received * b1) * determinant;
    [
        x0 / first_noise.max(f32::MIN_POSITIVE),
        x1_conj.conj() / second_noise.max(f32::MIN_POSITIVE),
    ]
}

fn equalize(
    grid: &[Tones; PBCH_SYMBOLS],
    channels: &[[Tones; 4]; PBCH_SYMBOLS],
    shift: usize,
) -> [[f32; CODED_BITS]; 3] {
    let mut resources = [Resource::default(); DATA_TONES];
    let mut output = resources.iter_mut();
    for (symbol, row) in grid.iter().enumerate() {
        for (index, &received) in row.iter().enumerate() {
            // The first two symbols reserve the CRS positions for all four
            // ports even when a cell transmits on fewer ports.
            if symbol < 2 && index % 3 == shift % 3 {
                continue;
            }
            // Four rows have 48, 48, 72 and 72 data tones, respectively.
            *output.next().unwrap() = Resource {
                received,
                channel: std::array::from_fn(|port| channels[symbol][port][index]),
            };
        }
    }
    debug_assert!(output.next().is_none());
    let mut llrs = [[0.0; CODED_BITS]; 3];
    for (index, &[first, second, third, fourth]) in resources.as_chunks::<4>().0.iter().enumerate()
    {
        let one_port =
            [first, second, third, fourth].map(|sample| sample.received * sample.channel[0].conj());
        let [two_first, two_second] = diversity_pair(first, second, 0, 1);
        let [two_third, two_fourth] = diversity_pair(third, fourth, 0, 1);
        let [four_first, four_second] = diversity_pair(first, second, 0, 2);
        let [four_third, four_fourth] = diversity_pair(third, fourth, 1, 3);
        let two_port = [two_first, two_second, two_third, two_fourth];
        let four_port = [four_first, four_second, four_third, four_fourth];
        for (output, hypothesis) in llrs.iter_mut().zip([one_port, two_port, four_port]) {
            for (symbol, value) in hypothesis.into_iter().enumerate() {
                let offset = (index * 4 + symbol) * 2;
                output[offset..offset + 2].copy_from_slice(&[value.re, value.im]);
            }
        }
    }
    llrs
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lte::pbch::bch::{seen_bits, tests::REFERENCE};

    const PCI: u16 = 235;
    pub(in crate::lte::pbch) const PSS_START: usize = 200;
    const FIRST_PBCH: usize = 338;

    fn encoded_bits(quarter: usize) -> [u8; 480] {
        std::array::from_fn(|index| (REFERENCE[quarter * 60 + index / 8] >> (7 - index % 8)) & 1)
    }

    fn pilot_chips(symbol: usize) -> [Complex32; 12] {
        let mut x1 = [0_u8; 1863];
        let mut x2 = x1;
        let seed = 1024 * (15 + symbol) * 471 + 471;
        x1[0] = 1;
        for (index, bit) in x2[..31].iter_mut().enumerate() {
            *bit = ((seed >> index) & 1) as u8;
        }
        for index in 0..1832 {
            x1[index + 31] = (x1[index + 3] + x1[index]) % 2;
            x2[index + 31] = (x2[index + 3] + x2[index + 2] + x2[index + 1] + x2[index]) % 2;
        }
        std::array::from_fn(|index| {
            let first = 1600 + 2 * (104 + index);
            let re = (x1[first] + x2[first]) % 2;
            let im = (x1[first + 1] + x2[first + 1]) % 2;
            Complex32::new(0.5 - f32::from(re), 0.5 - f32::from(im)) * 2.0_f32.sqrt()
        })
    }

    fn impulse_response(port: usize, bin: i32) -> Complex32 {
        let center = [
            Complex32::new(0.52, 0.21),
            Complex32::new(-0.28, 0.83),
            Complex32::new(0.15, -0.74),
            Complex32::new(0.81, 0.52),
        ];
        let phase = -TAU * bin as f32 * (port + 1) as f32 / 128.0;
        center[port] + Complex32::from_polar(0.13, phase)
    }

    pub(in crate::lte::pbch) fn fixture(
        ports: usize,
        carrier_hz: f32,
        quarter: usize,
    ) -> Vec<Complex32> {
        let bits = encoded_bits(quarter);
        let qpsk: [Complex32; 240] = std::array::from_fn(|index| {
            Complex32::new(
                0.5 - f32::from(bits[2 * index]),
                0.5 - f32::from(bits[2 * index + 1]),
            ) * 2.0_f32.sqrt()
        });
        // PCI 235 gives pilot offsets of 1 and 4 across the center six RBs.
        let mut resources = Vec::new();
        for symbol in 0..4 {
            for tone in 0..72 {
                if symbol >= 2 || !matches!(tone % 6, 1 | 4) {
                    resources.push((symbol, tone));
                }
            }
        }
        let mut grid = [[[Complex32::default(); 72]; 4]; 5];
        if ports == 1 {
            for ((symbol, tone), value) in resources.iter().copied().zip(qpsk) {
                grid[symbol][0][tone] = value;
            }
        } else {
            for (pair_index, (&[(symbol, k0), (_, k1)], &[x0, x1])) in resources
                .as_chunks::<2>()
                .0
                .iter()
                .zip(qpsk.as_chunks::<2>().0)
                .enumerate()
            {
                let (first, second) = match ports {
                    2 => (0, 1),
                    4 if pair_index % 2 == 0 => (0, 2),
                    4 => (1, 3),
                    _ => unreachable!("the fixture accepts 1, 2 or 4 ports"),
                };
                let gain = (ports as f32).sqrt();
                grid[symbol][first][k0] = x0 / gain;
                grid[symbol][first][k1] = x1 / gain;
                grid[symbol][second][k0] = -x1.conj() / gain;
                grid[symbol][second][k1] = x0.conj() / gain;
            }
        }
        for (port, symbol, offset) in [
            (0, 0, 1),
            (1, 0, 4),
            (2, 1, 4),
            (3, 1, 1),
            (0, 4, 4),
            (1, 4, 1),
        ] {
            if port < ports {
                for (index, value) in pilot_chips(symbol).into_iter().enumerate() {
                    grid[symbol][port][6 * index + offset] = value;
                }
            }
        }

        let mut iq = vec![Complex32::default(); 1014];
        for (symbol, port_tones) in grid.iter().enumerate() {
            let start = FIRST_PBCH + 137 * symbol;
            let prefix = if symbol == 0 { 10 } else { 9 };
            let tones: Vec<_> = (-36..=-1)
                .chain(1..=36)
                .enumerate()
                .map(|(index, bin)| {
                    let value = (0..ports)
                        .map(|port| port_tones[port][index] * impulse_response(port, bin))
                        .sum::<Complex32>();
                    (bin, value)
                })
                .collect();
            for (index, sample) in iq
                .iter_mut()
                .enumerate()
                .take(start + 128)
                .skip(start - prefix)
            {
                let time = (index as f32 - start as f32) / 128.0;
                let baseband = tones
                    .iter()
                    .map(|&(bin, tone)| tone * Complex32::from_polar(1.0, TAU * bin as f32 * time))
                    .sum::<Complex32>();
                let phase = TAU * carrier_hz * index as f32 / 1_920_000.0 + 0.19;
                *sample = baseband * Complex32::from_polar(1.0, phase);
            }
        }
        iq
    }

    #[test]
    fn recovers_soft_bits_for_all_port_counts() {
        let mut demodulator = Demodulator::new();
        for (index, (ports, cfo, timing)) in [(1, 2080.0, 0), (2, -4870.0, 1), (4, 2510.0, -1)]
            .into_iter()
            .enumerate()
        {
            let iq = fixture(ports, cfo, 0);
            let block = demodulator
                .extract(&iq, PSS_START.checked_add_signed(timing).unwrap(), PCI)
                .unwrap();
            assert!((block.cfo_hz - cfo).abs() < 10.0, "{}", block.cfo_hz);
            let llrs = &block.llrs[index];
            for (bit_index, (&llr, bit)) in llrs.iter().zip(encoded_bits(0)).enumerate() {
                assert_eq!(llr < 0.0, bit == 1, "port count {ports}, bit {bit_index}");
            }
            if ports == 2 {
                let decoded = seen_bits(std::slice::from_ref(llrs), 0, PCI, 2).unwrap();
                let expected = std::array::from_fn(|index| ((0x6a_7000 >> (23 - index)) & 1) as u8);
                assert_eq!(decoded, expected);
            }
        }
    }

    #[test]
    fn accepts_the_last_complete_block_and_rejects_invalid_extents() {
        let mut demodulator = Demodulator::new();
        let iq = fixture(1, 1200.0, 0);
        let end_of_pbch = FIRST_PBCH + 3 * 137 + 128;
        assert!(
            demodulator
                .extract(&iq[..end_of_pbch], PSS_START, PCI)
                .is_some()
        );
        assert!(demodulator.extract(&iq[..600], PSS_START, PCI).is_none());
        assert!(demodulator.extract(&iq, usize::MAX, PCI).is_none());
        assert!(demodulator.extract(&iq, PSS_START, 504).is_none());
        assert!(
            demodulator
                .extract(&[Complex32::default(); 1000], PSS_START, PCI)
                .is_none()
        );
    }
}
