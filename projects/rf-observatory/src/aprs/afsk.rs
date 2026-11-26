//! Bell 202 audio discrimination, symbol clock recovery, and NRZI decoding.
//!
//! Tone frequencies follow IARU Region 1 Technical Recommendation D.1.2 in
//! the [VHF Handbook v9.01](https://www.iaru-r1.org/wp-content/uploads/2021/03/VHF_Handbook_V9.01.pdf).

use std::f64::consts::TAU;

use rustfft::num_complex::Complex64;

/// Sample rate of the discriminator audio consumed by [`Demodulator`].
pub(super) const AUDIO_SAMPLE_RATE_HZ: u32 = crate::dsp::AUDIO_OUTPUT_RATE_HZ;
/// Parallel correlation and timing hypotheses carried through frame validation.
pub(super) const LANES: usize = WINDOWS.len() * LANES_PER_WINDOW;
/// Bell 202 symbols per second.
pub(super) const SYMBOL_RATE: i32 = 1_200;

const MARK_HZ: u32 = 1_200;
const SPACE_HZ: u32 = 2_200;
// The tones share a 200 Hz fundamental, so the local oscillators repeat exactly.
const OSCILLATOR_SAMPLES: usize = (AUDIO_SAMPLE_RATE_HZ / 200) as usize;
// About one symbol and 1.18 symbols at 50 kHz. The longer interval trades
// responsiveness at tone changes for finer discrimination under interference.
const LONG_WINDOW_SAMPLES: usize = 49;
const WINDOWS: [usize; 2] = [
    (AUDIO_SAMPLE_RATE_HZ / SYMBOL_RATE as u32) as usize,
    LONG_WINDOW_SAMPLES,
];
const LANES_PER_WINDOW: usize = 3;
const PHASE_WRAP: i32 = AUDIO_SAMPLE_RATE_HZ as i32;
const EDGE_PHASES: [i32; LANES_PER_WINDOW] =
    [PHASE_WRAP / 2, 3 * PHASE_WRAP / 8, 5 * PHASE_WRAP / 8];

struct TimingLane {
    edge_phase: i32,
    phase: i32,
    previous_symbol: Option<bool>,
}

impl TimingLane {
    fn push(&mut self, mark: bool, transition: bool) -> Option<bool> {
        if transition {
            // Pull the symbol clock toward the observed edge gradually to tolerate
            // the correlation window's phase-dependent transition jitter.
            self.phase += (self.edge_phase - self.phase) / 4;
        }
        self.phase += SYMBOL_RATE;
        if self.phase < PHASE_WRAP {
            return None;
        }
        self.phase -= PHASE_WRAP;
        // An NRZI zero changes tone. The preceding symbol is needed for a decision.
        self.previous_symbol
            .replace(mark)
            .map(|previous| previous == mark)
    }
}

struct CorrelationWindow {
    length: usize,
    history: [[Complex64; 2]; LONG_WINDOW_SAMPLES],
    history_index: usize,
    sums: [Complex64; 2],
    previous_mark: Option<bool>,
    lanes: [TimingLane; LANES_PER_WINDOW],
}

impl CorrelationWindow {
    fn new(length: usize) -> Self {
        Self {
            length,
            history: [[Complex64::default(); 2]; LONG_WINDOW_SAMPLES],
            history_index: 0,
            sums: [Complex64::default(); 2],
            previous_mark: None,
            lanes: EDGE_PHASES.map(|edge_phase| TimingLane {
                edge_phase,
                phase: 0,
                previous_symbol: None,
            }),
        }
    }

    fn push(&mut self, mixed: [Complex64; 2]) -> [Option<bool>; LANES_PER_WINDOW] {
        let history = &mut self.history[self.history_index];
        for ((sum, previous), mixed) in self.sums.iter_mut().zip(history).zip(mixed) {
            *sum += mixed - *previous;
            *previous = mixed;
        }
        self.history_index += 1;
        if self.history_index == self.length {
            self.history_index = 0;
        }

        let [mark_power, space_power] = self.sums.map(|tone| tone.norm_sqr());
        let mark = match self.previous_mark {
            Some(true) => 1.1 * mark_power >= space_power,
            Some(false) => mark_power > 1.1 * space_power,
            None => mark_power >= space_power,
        };
        let transition = self
            .previous_mark
            .replace(mark)
            .is_some_and(|previous| previous != mark);
        self.lanes
            .each_mut()
            .map(|lane| lane.push(mark, transition))
    }
}

/// Recover 1,200 bit/s Bell 202 from successive 50 kHz FM discriminator samples.
///
/// Two rectangular correlation windows compare quadrature tone energies. Each
/// window uses three transition-tracked clocks to sample at nearby symbol
/// phases. A framing decoder must independently validate each candidate stream.
pub(super) struct Demodulator {
    oscillators: [[Complex64; 2]; OSCILLATOR_SAMPLES],
    oscillator_index: usize,
    dc: f64,
    windows: [CorrelationWindow; WINDOWS.len()],
}

impl Default for Demodulator {
    fn default() -> Self {
        let oscillators = std::array::from_fn(|index| {
            [MARK_HZ, SPACE_HZ].map(|frequency| {
                let phase =
                    TAU * (index as f64 * f64::from(frequency)) / f64::from(AUDIO_SAMPLE_RATE_HZ);
                Complex64::from_polar(1.0, phase)
            })
        });
        Self {
            oscillators,
            oscillator_index: 0,
            dc: 0.0,
            windows: WINDOWS.map(CorrelationWindow::new),
        }
    }
}

impl Demodulator {
    /// Advance by one finite discriminator sample and emit any ready NRZI bits.
    pub(super) fn push(&mut self, sample: f64) -> [Option<bool>; LANES] {
        // Slowly reject residual FM carrier offset before tone correlation.
        self.dc += (sample - self.dc) / 32.0;
        let centered = sample - self.dc;
        let mixed = self.oscillators[self.oscillator_index].map(|oscillator| oscillator * centered);
        self.oscillator_index = (self.oscillator_index + 1) % OSCILLATOR_SAMPLES;

        let decisions = self.windows.each_mut().map(|window| window.push(mixed));
        std::array::from_fn(|lane| decisions[lane / LANES_PER_WINDOW][lane % LANES_PER_WINDOW])
    }
}

#[cfg(test)]
mod tests {
    use data_encoding::HEXLOWER;

    use super::super::frame;
    use super::*;

    fn line_bits() -> (Vec<bool>, Vec<bool>) {
        let flag = [false, true, true, true, true, true, true, false];
        let mut expected = flag.to_vec();
        let mut random = 0xa83a_d879_u32;
        let mut ones = 0;
        for _ in 0..1_024 {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let bit = (random >> 31) != 0 && ones < 5;
            expected.push(bit);
            ones = if bit { ones + 1 } else { 0 };
        }
        expected.extend(flag);
        let mut stream = flag.repeat(15);
        stream.extend_from_slice(&expected);
        stream.extend(flag.repeat(2));
        (stream, expected)
    }

    #[test]
    fn recovers_long_packet_with_fractional_timing_and_tone_imbalance() {
        let (bits, expected) = line_bits();
        for (delay, starting_phase, sender_clock, space_amplitude) in [
            (0, 0.0, 1.0, 1.0),
            (13, 1.5, 0.995, 0.5),
            (39, 4.2, 1.005, 1.8),
        ] {
            let mut mark = false;
            let tones: Vec<_> = bits
                .iter()
                .map(|&bit| {
                    mark ^= !bit;
                    if mark {
                        (f64::from(MARK_HZ), 1.0)
                    } else {
                        (f64::from(SPACE_HZ), space_amplitude)
                    }
                })
                .collect();
            let mut decoder = Demodulator::default();
            let mut decoded = std::array::from_fn::<_, LANES, _>(|_| Vec::new());
            let mut phase: f64 = starting_phase;
            let mut noise = 42_u32;
            let mut push = |sample| {
                for (lane, bit) in decoded.iter_mut().zip(decoder.push(sample)) {
                    lane.extend(bit);
                }
            };
            for _ in 0..delay {
                push(0.25);
            }
            for index in 0.. {
                let elapsed_seconds = index as f64 / f64::from(AUDIO_SAMPLE_RATE_HZ);
                let symbol = (elapsed_seconds * f64::from(SYMBOL_RATE) * sender_clock) as usize;
                let Some(&(frequency, amplitude)) = tones.get(symbol) else {
                    break;
                };
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise_sample = (f64::from(noise >> 16) / 65_535.0 - 0.5) * 0.4;
                let sample = amplitude * phase.sin() + noise_sample + 0.25;
                push(sample);
                phase += TAU * frequency / f64::from(AUDIO_SAMPLE_RATE_HZ);
            }
            assert!(
                decoded.iter().any(|lane| lane
                    .windows(expected.len())
                    .any(|window| window == expected)),
                "delay={delay}, clock={sender_clock}, space amplitude={space_amplitude}"
            );
        }
    }

    #[test]
    fn longer_window_recovers_a_frame_with_audio_tone_interference() {
        const FRAME_HEX: &str = "82a0a4a64040609c60868298986303f03e746573742036ccf8";
        let frame = HEXLOWER.decode(FRAME_HEX.as_bytes()).unwrap();
        let flag = [false, true, true, true, true, true, true, false];
        let mut wire = flag.repeat(14);
        let mut ones = 0;
        for byte in frame {
            for position in 0..8 {
                let bit = byte & (1 << position) != 0;
                wire.push(bit);
                ones = if bit { ones + 1 } else { 0 };
                if ones == 5 {
                    wire.push(false);
                    ones = 0;
                }
            }
        }
        wire.extend(flag.repeat(4));
        let mut mark = false;
        let tones: Vec<_> = wire
            .iter()
            .map(|&bit| {
                mark ^= !bit;
                if mark { 1_200.0 } else { 2_200.0 }
            })
            .collect();
        let mut demodulator = Demodulator::default();
        let mut framing: [frame::Decoder; LANES] =
            std::array::from_fn(|_| frame::Decoder::default());
        let mut validated = [0; LANES];
        let mut phase: f64 = 1.2;
        let rate = AUDIO_SAMPLE_RATE_HZ as usize;
        let baud = SYMBOL_RATE as usize;
        for sample in 0..tones.len() * rate / baud {
            phase += TAU * tones[sample * baud / rate] / rate as f64;
            let interference = 1.25 * (0.4 + TAU * 3_000.0 * sample as f64 / rate as f64).sin();
            for (index, bit) in demodulator
                .push(phase.sin() + interference)
                .into_iter()
                .enumerate()
            {
                if let Some(frame) = bit.and_then(|bit| framing[index].push(bit)) {
                    assert_eq!(frame.frame_hex, FRAME_HEX);
                    validated[index] += 1;
                }
            }
        }
        assert_eq!(validated[..LANES_PER_WINDOW], [0; LANES_PER_WINDOW]);
        assert!(validated[LANES_PER_WINDOW..].contains(&1));
    }
}
