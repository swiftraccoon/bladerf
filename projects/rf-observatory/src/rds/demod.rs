//! Recover RDS data-stream 0 from the real FM multiplex.
//!
//! The 57 kHz subcarrier, biphase symbols, and differential coding follow
//! IEC 62106-1:2018, clauses 4.3 through 4.9. See also the corresponding
//! [NRSC RBDS physical-layer description](https://www.nrscstandards.org/standards-and-guidelines/documents/archive/nrsc-4-2004.pdf),
//! sections 1.2 through 1.7.

use std::f64::consts::TAU;

use rustfft::num_complex::Complex64;

use crate::dsp::FM_MULTIPLEX_RATE_HZ;

/// Evenly spaced candidate symbol phases for independent framing.
pub(super) const LANES: usize = 8;

const SUBCARRIER_HZ: u32 = 57_000;
const OSCILLATOR_SAMPLES: usize = (FM_MULTIPLEX_RATE_HZ / 1_000) as usize;
const DECIMATION: usize = 10;
const DECIMATION_TAPS: usize = 3 * (DECIMATION - 1) + 1;
const BASEBAND_SAMPLE_RATE_HZ: u32 = FM_MULTIPLEX_RATE_HZ / DECIMATION as u32;
const BASEBAND_TAPS: usize = 63;
// The clock is exactly 19/400 bits per input sample at 25 kHz.
const TICKS_PER_SAMPLE: u16 = 19;
const TICKS_PER_BIT: u16 = 400;
const _: () = assert!(FM_MULTIPLEX_RATE_HZ == 250_000);

struct Fir<const N: usize> {
    taps: [f64; N],
    history: [Complex64; N],
    oldest: usize,
}

impl<const N: usize> Fir<N> {
    fn new(taps: [f64; N]) -> Self {
        Self {
            taps,
            history: [Complex64::default(); N],
            oldest: 0,
        }
    }

    fn push(&mut self, sample: Complex64) {
        self.history[self.oldest] = sample;
        self.oldest = (self.oldest + 1) % N;
    }

    fn output(&self) -> Complex64 {
        // Both kernels are symmetric. Apply them oldest to newest in two slices.
        self.history[self.oldest..]
            .iter()
            .chain(&self.history[..self.oldest])
            .zip(&self.taps)
            .map(|(sample, tap)| sample * tap)
            .sum()
    }
}

fn decimation_taps() -> [f64; DECIMATION_TAPS] {
    // Convolve three ten-sample averages for the response of a third-order CIC
    // decimator, using finite history to bound floating-point state.
    let mut taps = [0.0; DECIMATION_TAPS];
    for first in 0..DECIMATION {
        for second in 0..DECIMATION {
            for third in 0..DECIMATION {
                taps[first + second + third] += 1.0;
            }
        }
    }
    taps.map(|tap| tap / DECIMATION.pow(3) as f64)
}

fn baseband_taps() -> [f64; BASEBAND_TAPS] {
    // Pass the ±2.375 kHz data spectrum and suppress adjacent multiplex content.
    let cutoff = 2_600.0 / f64::from(BASEBAND_SAMPLE_RATE_HZ);
    let taps = std::array::from_fn(|index| {
        let offset = index as f64 - (BASEBAND_TAPS - 1) as f64 * 0.5;
        let sinc = if index == BASEBAND_TAPS / 2 {
            2.0 * cutoff
        } else {
            (TAU * cutoff * offset).sin() / (TAU * offset * 0.5)
        };
        sinc * (0.54 - 0.46 * (TAU * index as f64 / (BASEBAND_TAPS - 1) as f64).cos())
    });
    let gain: f64 = taps.iter().sum();
    taps.map(|tap| tap / gain)
}

struct TimingLane {
    phase: u16,
    integral: Complex64,
    previous: Option<Complex64>,
}

impl TimingLane {
    fn push(&mut self, sample: Complex64) -> Option<bool> {
        // Integrate with opposite polarities in the two halves of the symbol.
        // Split samples at fractional boundaries so both halves have equal weight.
        let current_ticks = (TICKS_PER_BIT - self.phase).min(TICKS_PER_SAMPLE);
        let positive_ticks = (TICKS_PER_BIT / 2)
            .saturating_sub(self.phase)
            .min(current_ticks);
        let weight = f64::from(2 * positive_ticks) - f64::from(current_ticks);
        self.integral += sample * weight;
        self.phase += TICKS_PER_SAMPLE;
        if self.phase < TICKS_PER_BIT {
            return None;
        }
        self.phase -= TICKS_PER_BIT;
        let symbol = std::mem::replace(&mut self.integral, sample * f64::from(self.phase));
        // A differential one flips polarity. The complex dot product cancels an
        // arbitrary carrier phase and tolerates rotation below π/2 per symbol.
        self.previous
            .replace(symbol)
            .map(|previous| previous.re * symbol.re + previous.im * symbol.im < 0.0)
    }
}

/// Generate candidate differential bits from successive 250 kHz FM discriminator samples.
///
/// The lanes share the mixer and filters and cover fixed phases at the nominal
/// 1,187.5 bit/s clock. A framing decoder must validate each lane independently.
pub(super) struct Demodulator {
    oscillator: [Complex64; OSCILLATOR_SAMPLES],
    oscillator_index: usize,
    decimator: Fir<DECIMATION_TAPS>,
    baseband: Fir<BASEBAND_TAPS>,
    lanes: [TimingLane; LANES],
}

impl Default for Demodulator {
    fn default() -> Self {
        // 57 kHz is an integer multiple of 1 kHz, so the NCO table repeats exactly.
        let oscillator = std::array::from_fn(|index| {
            let phase =
                -TAU * index as f64 * f64::from(SUBCARRIER_HZ) / f64::from(FM_MULTIPLEX_RATE_HZ);
            Complex64::from_polar(1.0, phase)
        });
        Self {
            oscillator,
            oscillator_index: 0,
            decimator: Fir::new(decimation_taps()),
            baseband: Fir::new(baseband_taps()),
            lanes: std::array::from_fn(|index| TimingLane {
                phase: index as u16 * TICKS_PER_BIT / LANES as u16,
                integral: Complex64::default(),
                previous: None,
            }),
        }
    }
}

impl Demodulator {
    /// Advance by one finite multiplex sample and return each lane's next bit, if due.
    pub(super) fn push(&mut self, mpx: f64) -> [Option<bool>; LANES] {
        self.decimator
            .push(self.oscillator[self.oscillator_index] * mpx);
        self.oscillator_index = (self.oscillator_index + 1) % OSCILLATOR_SAMPLES;
        // The oscillator period contains exactly 25 decimation intervals.
        if !self.oscillator_index.is_multiple_of(DECIMATION) {
            return [None; LANES];
        }
        self.baseband.push(self.decimator.output());
        let sample = self.baseband.output();
        self.lanes.each_mut().map(|lane| lane.push(sample))
    }
}

#[cfg(test)]
mod tests {
    use std::f64::consts::PI;

    use super::*;

    #[test]
    fn emits_exact_nominal_bit_cadence_after_the_first_polarity() {
        let mut demodulator = Demodulator::default();
        let mut count = [0; LANES];
        for _ in 0..4_000 {
            for (count, bit) in count.iter_mut().zip(demodulator.push(0.0)) {
                if let Some(bit) = bit {
                    assert!(!bit);
                    *count += 1;
                }
            }
        }
        assert_eq!(count, [18; LANES]);
    }

    // The transmit shaping response is cos(πf/4) for |f| <= 2, with
    // frequency in bit-clock units. Its impulse is a pair of shifted sinc terms.
    fn shaped_pulse(t: f64) -> f64 {
        [-0.125, 0.125]
            .into_iter()
            .map(|shift| {
                let x = 4.0 * PI * (t - shift);
                if x.abs() < 1e-8 { 1.0 } else { x.sin() / x }
            })
            .sum()
    }

    #[test]
    fn recovers_shaped_multiplex_at_unknown_carrier_and_symbol_phases() {
        let mut state = 0x6329_4a0du32;
        let bits: [bool; 160] = std::array::from_fn(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state >> 31 != 0
        });
        let mut polarity = 1.0;
        let symbols = bits.map(|bit| {
            if bit {
                polarity = -polarity;
            }
            polarity
        });
        for (carrier_hz, carrier_phase, bit_shift) in
            [(57_006.0, 0.91, 0.37), (56_994.0, -2.11, 0.61)]
        {
            let mut demodulator = Demodulator::default();
            let mut lanes = std::array::from_fn::<_, LANES, _>(|_| Vec::new());
            let bit_rate = carrier_hz / 48.0;
            let sample_count = ((bits.len() as f64 + 2.0) * 250_000.0 / bit_rate) as usize;
            for index in 0..sample_count {
                let t = index as f64 / 250_000.0;
                let bit_time = t * bit_rate - bit_shift;
                let nearby = bit_time as usize;
                let baseband: f64 = (nearby.saturating_sub(4)..(nearby + 5).min(symbols.len()))
                    .map(|index| {
                        let offset = bit_time - index as f64;
                        symbols[index] * (shaped_pulse(offset - 0.25) - shaped_pulse(offset - 0.75))
                    })
                    .sum();
                let mpx = 0.035 * baseband * (TAU * carrier_hz * t + carrier_phase).cos()
                    + 0.100 * (TAU * 19_000.0 * t).cos()
                    + 0.220 * (TAU * 1_700.0 * t).cos()
                    + 0.300 * (TAU * 53_000.0 * t).cos()
                    + 0.200 * (TAU * 67_000.0 * t).cos();
                for (lane, bit) in lanes.iter_mut().zip(demodulator.push(mpx)) {
                    lane.extend(bit);
                }
            }
            let expected = &bits[16..144];
            assert!(
                lanes
                    .iter()
                    .any(|lane| lane.windows(expected.len()).any(|bits| bits == expected)),
                "no lane recovered the test bits at a {carrier_hz} Hz subcarrier"
            );
        }
    }
}
