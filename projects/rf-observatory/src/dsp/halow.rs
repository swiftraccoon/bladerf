//! Stream a guarded HaLow channel onto its exact 2 MHz OFDM sample grid.

use std::collections::VecDeque;
use std::f64::consts::{PI, TAU};

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex32;

use super::{FirDecimator, FixedChannel, Iq, Iq16, tuning_word};

const OUTPUT_HZ: u32 = 2_000_000;
const WORK_TAPS: usize = 97;
const CUTOFF_HZ: f64 = 920_000.0;
const WIDE_DECIMATION: u32 = 4;

/// A 2 MHz HaLow channel with bounded translation and resampling state.
pub(crate) struct HalowChannel {
    mixer: FixedChannel,
    antialias: Option<FirDecimator>,
    resampler: Option<Resampler>,
    input_ratio: f64,
    input_origin: f64,
}

impl HalowChannel {
    /// One-sided usable margin credited by the sweep planner.
    pub(crate) const HALF_WIDTH_HZ: u64 = 1_000_000;
    /// Sample rate on the OFDM decoder's grid.
    pub(crate) const OUTPUT_RATE_HZ: u32 = OUTPUT_HZ;

    /// Whether the selected 2 MHz channel fits the captured usable passband.
    pub(crate) fn is_covered(input_rate_hz: u32, offset_hz: u64) -> bool {
        match input_rate_hz {
            OUTPUT_HZ => offset_hz == 0,
            3_840_000 | 4_000_000 => {
                offset_hz <= u64::from(input_rate_hz) * 2 / 5 - Self::HALF_WIDTH_HZ
            }
            crate::SAMPLE_RATE => {
                let guard = crate::spectrum::CENTER_GUARD_HZ as u64 + Self::HALF_WIDTH_HZ;
                let outer = u64::from(input_rate_hz) * 2 / 5 - Self::HALF_WIDTH_HZ;
                (guard..=outer).contains(&offset_hz)
            }
            _ => false,
        }
    }

    /// Translate supported narrow or guarded 20 MSps IQ to the OFDM grid.
    ///
    /// # Errors
    /// Rejects unsupported rates or geometry outside the usable passband.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: i64) -> Result<Self> {
        ensure!(
            Self::is_covered(input_rate_hz, offset_hz.unsigned_abs()),
            "HaLow needs centered 2 MSps IQ, usable 3.84 or 4 MSps IQ, or a guarded 20 MSps channel"
        );
        let wide = input_rate_hz == crate::SAMPLE_RATE;
        let intermediate_hz = if wide {
            input_rate_hz / WIDE_DECIMATION
        } else {
            input_rate_hz
        };
        let antialias = wide.then(|| {
            FirDecimator::new(
                41,
                2_750_000.0,
                f64::from(input_rate_hz),
                WIDE_DECIMATION as usize,
            )
        });
        let input_origin = antialias.as_ref().map_or(0.0, |filter| {
            f64::from(WIDE_DECIMATION - 1) - (filter.coefficients.len() / 2) as f64
        });
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz as f64, input_rate_hz)?,
                ..FixedChannel::default()
            },
            antialias,
            resampler: (intermediate_hz != OUTPUT_HZ).then(|| Resampler::new(intermediate_hz)),
            input_ratio: f64::from(input_rate_hz) / f64::from(OUTPUT_HZ),
            input_origin,
        })
    }

    /// Advance the stream, returning the next scheduled channel sample.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Complex32> {
        let mixed = self.mixer.mix_interpolated(sample);
        let filtered = match &mut self.antialias {
            Some(filter) => filter.push(mixed)?,
            None => mixed,
        };
        let output = match &mut self.resampler {
            Some(resampler) => resampler.push(filtered)?,
            None => filtered,
        };
        Some(Complex32::new(output.i as f32, output.q as f32))
    }

    /// Map a channel-grid position to its centered original-sample estimate.
    pub(crate) fn input_sample(&self, index: u64) -> f64 {
        self.input_origin + index as f64 * self.input_ratio
    }
}

struct Resampler {
    phases: Vec<Vec<f64>>,
    down: u64,
    history: VecDeque<Iq>,
    incoming_index: u64,
    next_tick: u64,
}

impl Resampler {
    fn new(input_hz: u32) -> Self {
        // Supported 3.84, 4 and 5 MHz intermediate rates reduce to these
        // exact rational steps on the 2 MHz output lattice.
        let (up, down) = match input_hz {
            3_840_000 => (25, 48),
            4_000_000 => (1, 2),
            _ => (2, 5),
        };
        let phases = (0..up)
            .map(|phase| fractional_taps(f64::from(input_hz), f64::from(phase) / f64::from(up)))
            .collect();
        Self {
            phases,
            down,
            history: VecDeque::from(vec![Iq::default(); WORK_TAPS]),
            incoming_index: 0,
            next_tick: 0,
        }
    }

    fn push(&mut self, value: Iq) -> Option<Iq> {
        self.history.pop_front();
        self.history.push_back(value);
        let index = self.incoming_index;
        self.incoming_index += 1;
        let up = self.phases.len() as u64;
        let center = self.next_tick / up;
        if index < center + (WORK_TAPS / 2) as u64 {
            return None;
        }
        let phase = (self.next_tick % up) as usize;
        self.next_tick += self.down;
        let mut output = Iq::default();
        for (sample, &tap) in self.history.iter().zip(&self.phases[phase]) {
            output.i += sample.i * tap;
            output.q += sample.q * tap;
        }
        Some(output)
    }
}

fn fractional_taps(input_hz: f64, fraction: f64) -> Vec<f64> {
    let mut taps: Vec<_> = (0..WORK_TAPS)
        .map(|index| {
            let relative = index as f64 - (WORK_TAPS / 2) as f64 - fraction;
            let sinc_angle = TAU * CUTOFF_HZ * relative / input_hz;
            let sinc = if sinc_angle.abs() < f64::EPSILON {
                1.0
            } else {
                sinc_angle.sin() / sinc_angle
            };
            let theta = 2.0 * PI * index as f64 / (WORK_TAPS - 1) as f64;
            let blackman = 0.42 - 0.5 * theta.cos() + 0.08 * (2.0 * theta).cos();
            sinc * blackman
        })
        .collect();
    let sum = taps.iter().sum::<f64>();
    for tap in &mut taps {
        *tap /= sum;
    }
    taps
}
