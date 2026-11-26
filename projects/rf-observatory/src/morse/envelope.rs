//! Select a keyed RF carrier or a keyed tone in narrowband FM.
//!
//! A translated CIC stream supplies complex RF samples or an FM discriminator.
//! Three cascaded 2 ms moving averages select the complex envelope before
//! decimation and power measurement. They have approximately 3 ms of delay
//! and 6 ms of finite history. The power scale depends on the receive mode.
//! The zero-state FM path can show transients during the first 8 ms.

use std::f64::consts::TAU;

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex64;

use crate::dsp::{AUDIO_OUTPUT_RATE_HZ, CIC_DECIMATION, FixedBank, Iq16, NarrowFm, tuning_word};

/// Output samples of uncalibrated linear envelope power per second.
pub(super) const RATE_HZ: u32 = 1_000;

const AVERAGE_STAGES: usize = 3;

struct Lowpass {
    history: Vec<[Complex64; AVERAGE_STAGES]>,
    sums: [Complex64; AVERAGE_STAGES],
    position: usize,
    decimation: usize,
    scale: f64,
}

impl Lowpass {
    fn new(rate_hz: u32) -> Self {
        let decimation = (rate_hz / RATE_HZ) as usize;
        let window = 2 * decimation;
        Self {
            history: vec![[Complex64::default(); AVERAGE_STAGES]; window],
            sums: [Complex64::default(); AVERAGE_STAGES],
            position: 0,
            decimation,
            scale: (window as f64).powi(AVERAGE_STAGES as i32).recip(),
        }
    }

    fn push(&mut self, mut sample: Complex64) -> Option<f64> {
        for (sum, previous) in self.sums.iter_mut().zip(&mut self.history[self.position]) {
            *sum += sample - *previous;
            *previous = sample;
            sample = *sum;
        }
        self.position += 1;
        if self.position == self.history.len() {
            self.position = 0;
        }
        // The 2 ms window wraps after every other 1 ms output.
        (self.position == 0 || self.position == self.decimation)
            .then(|| (sample * self.scale).norm_sqr())
    }
}

struct AudioTone {
    fm: NarrowFm,
    phase: Complex64,
    step: Complex64,
    index: u32,
}

impl AudioTone {
    fn new(input_rate_hz: u32, tone_hz: u32) -> Result<Self> {
        ensure!(
            super::TONE_RANGE_HZ.contains(&tone_hz),
            "Morse audio tone must be 300..=2000 Hz"
        );
        let angle = -TAU * f64::from(tone_hz) / f64::from(AUDIO_OUTPUT_RATE_HZ);
        Ok(Self {
            fm: NarrowFm::new(input_rate_hz)?,
            phase: Complex64::new(1.0, 0.0),
            step: Complex64::from_polar(1.0, angle),
            index: 0,
        })
    }

    fn push(&mut self, sample: Iq16) -> Option<Complex64> {
        let audio = self.fm.push(sample)?;
        let baseband = self.phase * audio;
        self.phase *= self.step;
        self.index += 1;
        if self.index == AUDIO_OUTPUT_RATE_HZ {
            // Every allowed tone completes an integer number of cycles in 1 s.
            self.phase = Complex64::new(1.0, 0.0);
            self.index = 0;
        }
        Some(baseband)
    }
}

/// Downconvert one Morse channel from a stream of Q11 IQ samples.
pub(super) struct Detector {
    receiver: FixedBank,
    tone: Option<AudioTone>,
    lowpass: Lowpass,
}

impl Detector {
    /// Select a signed RF offset in hertz and optionally a tone in the FM audio.
    ///
    /// # Errors
    /// Rejects rates other than 2 or 20 MSps, offsets outside the sampled
    /// Nyquist interval, and audio tones outside 300 through 2000 Hz.
    pub(super) fn new(input_rate_hz: u32, offset_hz: f64, tone_hz: Option<u32>) -> Result<Self> {
        ensure!(
            [2_000_000, 20_000_000].contains(&input_rate_hz),
            "Morse envelope requires 2 or 20 MSps IQ input"
        );
        let receiver = FixedBank::new(&[tuning_word(offset_hz, input_rate_hz)?])?;
        let intermediate_rate_hz = input_rate_hz / CIC_DECIMATION as u32;
        let tone = tone_hz
            .map(|hz| AudioTone::new(intermediate_rate_hz, hz))
            .transpose()?;
        let rate_hz = if tone.is_some() {
            AUDIO_OUTPUT_RATE_HZ
        } else {
            intermediate_rate_hz
        };
        Ok(Self {
            receiver,
            tone,
            lowpass: Lowpass::new(rate_hz),
        })
    }

    /// Advance by one sample. The first power output follows a full millisecond.
    pub(super) fn push(&mut self, sample: Iq16) -> Option<f64> {
        let iq = self.receiver.push(sample)?.samples[0];
        let baseband = match &mut self.tone {
            Some(tone) => tone.push(iq)?,
            None => Complex64::new(f64::from(iq.i), f64::from(iq.q)),
        };
        self.lowpass.push(baseband)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT_RATE_HZ: u32 = 2_000_000;

    fn quantize(sample: Complex64) -> Iq16 {
        Iq16 {
            i: sample.re.round() as i16,
            q: sample.im.round() as i16,
        }
    }

    #[test]
    fn paces_both_modes_at_exactly_one_kilohertz() {
        for rate_hz in [INPUT_RATE_HZ, 20_000_000] {
            let inputs_per_output = rate_hz / RATE_HZ;
            for tone_hz in [None, Some(300), Some(2_000)] {
                let mut detector = Detector::new(rate_hz, 0.0, tone_hz).unwrap();
                for index in 1..=3 * inputs_per_output + 7 {
                    let output = detector.push(Iq16::default());
                    assert_eq!(
                        output,
                        index.is_multiple_of(inputs_per_output).then_some(0.0)
                    );
                }
            }
        }
        assert!(Detector::new(4_000_000, 0.0, None).is_err());
        assert!(Detector::new(INPUT_RATE_HZ, f64::NAN, None).is_err());
        assert!(Detector::new(INPUT_RATE_HZ, 0.0, Some(299)).is_err());
        assert!(Detector::new(INPUT_RATE_HZ, 0.0, Some(2_001)).is_err());
    }

    fn keyed(time: f64) -> bool {
        (0.012..0.034).contains(&time)
    }

    fn assert_keyed_envelope(power: &[f64], minimum_mark_power: f64) {
        let mark = power[20..31].iter().sum::<f64>() / 11.0;
        let space = power[7..11]
            .iter()
            .chain(&power[42..])
            .copied()
            .fold(0.0, f64::max);
        assert!(mark > minimum_mark_power, "mark power {mark}");
        assert!(
            mark > 50.0 * space,
            "mark power {mark}, space power {space}"
        );
        let first = 8 + power[8..]
            .iter()
            .position(|&value| value > mark * 0.4)
            .unwrap();
        let last = power.iter().rposition(|&value| value > mark * 0.4).unwrap();
        assert!((13..=16).contains(&first), "first mark bin {first}");
        assert!((34..=38).contains(&last), "last mark bin {last}");
    }

    #[test]
    fn preserves_cw_edges_with_residual_offset_and_a_stronger_neighbor() {
        let offset_hz = -145_000.0;
        let mut detector = Detector::new(INPUT_RATE_HZ, offset_hz, None).unwrap();
        let power: Vec<_> = (0..INPUT_RATE_HZ / 20)
            .filter_map(|index| {
                let t = f64::from(index) / f64::from(INPUT_RATE_HZ);
                let mut iq = Complex64::from_polar(550.0, TAU * (offset_hz + 700.0) * t + 0.27);
                if keyed(t) {
                    iq += Complex64::from_polar(120.0, TAU * (offset_hz - 100.0) * t - 1.5);
                }
                detector.push(quantize(iq))
            })
            .collect();
        assert_keyed_envelope(&power, 7_000.0);
    }

    #[test]
    fn selects_a_keyed_tone_in_uninterrupted_fm() {
        let offset_hz = 210_000.0;
        let mut detector = Detector::new(INPUT_RATE_HZ, offset_hz, Some(700)).unwrap();
        let mut phase = 0.42;
        let power: Vec<_> = (0..INPUT_RATE_HZ / 20)
            .filter_map(|index| {
                let t = f64::from(index) / f64::from(INPUT_RATE_HZ);
                let mut hz = offset_hz + 840.0 + 1_700.0 * (TAU * 1_540.0 * t).cos();
                if keyed(t) {
                    hz += 700.0 * (TAU * 740.0 * t).cos();
                }
                phase = (phase + TAU * hz / f64::from(INPUT_RATE_HZ)).rem_euclid(TAU);
                detector.push(quantize(Complex64::from_polar(800.0, phase)))
            })
            .collect();
        assert_keyed_envelope(&power, 0.001);
    }
}
