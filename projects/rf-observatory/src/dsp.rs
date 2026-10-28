//! Streaming reference DSP and bit-exact models for the first FPGA channel bank.
//!
//! The hardware stage uses a 256-entry Q1.15 oscillator, three 48-bit wrapping
//! CIC integrators/combs, and decimation by 20. Each call represents one accepted
//! input sample; hardware clock latency and ready/valid stalls are separate
//! transport properties. The first output follows the twentieth input sample.
//! Integer shifts are arithmetic; CIC gain is 8000/8192 after the 13-bit shift.
//! The CIC is not an ideal channel filter: aliasing and passband droop remain.
//!
//! Host postprocessing performs a separate FIR decimation, mono FM discrimination,
//! audio low-pass filtering and deemphasis. It does not decode stereo or RDS.
//! These synchronous bounded-state objects perform no radio access or file I/O.

use std::f64::consts::TAU;

use anyhow::{Result, ensure};

pub(crate) const CIC_DECIMATION: usize = 20;
pub(crate) const MAX_CHANNELS: usize = 4;
const CIC_MASK: i64 = (1_i64 << 48) - 1;

/// round(32767 * sin(2*pi*k/256)); fixed literals define the RTL/model contract.
pub(crate) const SIN_Q15: [i16; 256] = [
    0, 804, 1608, 2410, 3212, 4011, 4808, 5602, 6393, 7179, 7962, 8739, 9512, 10278, 11039, 11793,
    12539, 13279, 14010, 14732, 15446, 16151, 16846, 17530, 18204, 18868, 19519, 20159, 20787,
    21403, 22005, 22594, 23170, 23731, 24279, 24811, 25329, 25832, 26319, 26790, 27245, 27683,
    28105, 28510, 28898, 29268, 29621, 29956, 30273, 30571, 30852, 31113, 31356, 31580, 31785,
    31971, 32137, 32285, 32412, 32521, 32609, 32678, 32728, 32757, 32767, 32757, 32728, 32678,
    32609, 32521, 32412, 32285, 32137, 31971, 31785, 31580, 31356, 31113, 30852, 30571, 30273,
    29956, 29621, 29268, 28898, 28510, 28105, 27683, 27245, 26790, 26319, 25832, 25329, 24811,
    24279, 23731, 23170, 22594, 22005, 21403, 20787, 20159, 19519, 18868, 18204, 17530, 16846,
    16151, 15446, 14732, 14010, 13279, 12539, 11793, 11039, 10278, 9512, 8739, 7962, 7179, 6393,
    5602, 4808, 4011, 3212, 2410, 1608, 804, 0, -804, -1608, -2410, -3212, -4011, -4808, -5602,
    -6393, -7179, -7962, -8739, -9512, -10278, -11039, -11793, -12539, -13279, -14010, -14732,
    -15446, -16151, -16846, -17530, -18204, -18868, -19519, -20159, -20787, -21403, -22005, -22594,
    -23170, -23731, -24279, -24811, -25329, -25832, -26319, -26790, -27245, -27683, -28105, -28510,
    -28898, -29268, -29621, -29956, -30273, -30571, -30852, -31113, -31356, -31580, -31785, -31971,
    -32137, -32285, -32412, -32521, -32609, -32678, -32728, -32757, -32767, -32757, -32728, -32678,
    -32609, -32521, -32412, -32285, -32137, -31971, -31785, -31580, -31356, -31113, -30852, -30571,
    -30273, -29956, -29621, -29268, -28898, -28510, -28105, -27683, -27245, -26790, -26319, -25832,
    -25329, -24811, -24279, -23731, -23170, -22594, -22005, -21403, -20787, -20159, -19519, -18868,
    -18204, -17530, -16846, -16151, -15446, -14732, -14010, -13279, -12539, -11793, -11039, -10278,
    -9512, -8739, -7962, -7179, -6393, -5602, -4808, -4011, -3212, -2410, -1608, -804,
];

/// One complex sample. Radio acquisition must validate its native
/// SC16 Q11 range; the DSP model also defines behavior for every signed i16.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Iq16 {
    pub(crate) i: i16,
    pub(crate) q: i16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Iq {
    pub(crate) i: f64,
    pub(crate) q: f64,
}

impl From<Iq16> for Iq {
    fn from(value: Iq16) -> Self {
        Self {
            i: f64::from(value.i),
            q: f64::from(value.q),
        }
    }
}

impl Iq {
    pub(crate) fn power(self) -> f64 {
        self.i * self.i + self.q * self.q
    }
}

/// Convert a signed frequency offset to the nearest modulo-2^32 oscillator word.
/// Positive words select positive-offset input signals by negative rotation.
/// The Nyquist endpoints are excluded because their sign is ambiguous.
pub(crate) fn tuning_word(offset_hz: f64, sample_rate_hz: u32) -> Result<u32> {
    ensure!(sample_rate_hz > 0, "sample rate must be positive");
    ensure!(
        offset_hz.is_finite() && offset_hz.abs() < f64::from(sample_rate_hz) / 2.0,
        "offset must be finite and strictly within the sampled Nyquist window"
    );
    let signed = (offset_hz / f64::from(sample_rate_hz) * 4_294_967_296.0).round() as i64;
    Ok(signed.rem_euclid(1_i64 << 32) as u32)
}

pub(crate) fn tuning_frequency_hz(word: u32, sample_rate_hz: u32) -> f64 {
    f64::from(word as i32) * f64::from(sample_rate_hz) / 4_294_967_296.0
}

fn wrap48(value: i64) -> i64 {
    let raw = value & CIC_MASK;
    if raw & (1_i64 << 47) != 0 {
        raw - (1_i64 << 48)
    } else {
        raw
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FixedCic {
    integrators: [i64; 3],
    comb_delays: [i64; 3],
}

impl FixedCic {
    fn integrate(&mut self, sample: i64) {
        let mut current = sample;
        for stage in &mut self.integrators {
            *stage = wrap48(*stage + current);
            current = *stage;
        }
    }

    fn decimate(&mut self) -> (i16, bool) {
        let mut current = self.integrators[2];
        for delay in &mut self.comb_delays {
            let difference = wrap48(current - *delay);
            *delay = current;
            current = difference;
        }
        let shifted = current >> 13;
        let clipped = !(i64::from(i16::MIN)..=i64::from(i16::MAX)).contains(&shifted);
        (
            shifted.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16,
            clipped,
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FixedChannel {
    word: u32,
    phase: u32,
    i: FixedCic,
    q: FixedCic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FixedOutput {
    pub(crate) samples: [Iq16; MAX_CHANNELS],
    pub(crate) clipped: [bool; MAX_CHANNELS],
}

/// Four-lane bit-exact NCO/CIC reference, with one to four configured channels.
/// Construction is the configuration boundary; changing frequency requires a
/// new bank or resetting the FPGA with its corresponding new words.
#[derive(Clone, Debug)]
pub(crate) struct FixedBank {
    channels: [FixedChannel; MAX_CHANNELS],
    count: usize,
    accepted_modulo: usize,
}

impl FixedBank {
    pub(crate) fn new(words: &[u32]) -> Result<Self> {
        ensure!(
            !words.is_empty() && words.len() <= MAX_CHANNELS,
            "select one through four channels"
        );
        let mut channels = [FixedChannel::default(); MAX_CHANNELS];
        for (channel, word) in channels.iter_mut().zip(words) {
            channel.word = *word;
        }
        Ok(Self {
            channels,
            count: words.len(),
            accepted_modulo: 0,
        })
    }

    pub(crate) fn channels(&self) -> usize {
        self.count
    }

    #[cfg(test)]
    fn reset(&mut self) {
        for channel in &mut self.channels {
            let word = channel.word;
            *channel = FixedChannel {
                word,
                ..FixedChannel::default()
            };
        }
        self.accepted_modulo = 0;
    }

    /// Advance on exactly one accepted sample. A caller models a ready/valid
    /// stall by omitting this call, which freezes every oscillator and CIC.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<FixedOutput> {
        for channel in &mut self.channels[..self.count] {
            let index = (channel.phase >> 24) as usize;
            let sin = i64::from(SIN_Q15[index]);
            let cos = i64::from(SIN_Q15[(index + 64) & 255]);
            let i = i64::from(sample.i);
            let q = i64::from(sample.q);
            channel.i.integrate((i * cos + q * sin) >> 15);
            channel.q.integrate((q * cos - i * sin) >> 15);
            channel.phase = channel.phase.wrapping_add(channel.word);
        }
        self.accepted_modulo += 1;
        if self.accepted_modulo < CIC_DECIMATION {
            return None;
        }
        self.accepted_modulo = 0;
        let mut output = FixedOutput {
            samples: [Iq16::default(); MAX_CHANNELS],
            clipped: [false; MAX_CHANNELS],
        };
        for (index, channel) in self.channels[..self.count].iter_mut().enumerate() {
            let (i, clip_i) = channel.i.decimate();
            let (q, clip_q) = channel.q.decimate();
            output.samples[index] = Iq16 { i, q };
            output.clipped[index] = clip_i || clip_q;
        }
        Some(output)
    }
}

const CIC_TAPS: usize = 3 * (CIC_DECIMATION - 1) + 1;

const fn cic_coefficients() -> [u16; CIC_TAPS] {
    let mut coefficients = [0; CIC_TAPS];
    let mut a = 0;
    while a < CIC_DECIMATION {
        let mut b = 0;
        while b < CIC_DECIMATION {
            let mut c = 0;
            while c < CIC_DECIMATION {
                coefficients[a + b + c] += 1;
                c += 1;
            }
            b += 1;
        }
        a += 1;
    }
    coefficients
}

const CIC_COEFFICIENTS: [u16; CIC_TAPS] = cic_coefficients();

#[derive(Clone, Copy, Debug)]
struct FloatCic {
    history: [f64; CIC_TAPS],
    position: usize,
}

impl Default for FloatCic {
    fn default() -> Self {
        Self {
            history: [0.0; CIC_TAPS],
            position: 0,
        }
    }
}

impl FloatCic {
    fn integrate(&mut self, sample: f64) {
        self.history[self.position] = sample;
        self.position = (self.position + 1) % CIC_TAPS;
    }

    fn decimate(&mut self) -> f64 {
        let mut output = 0.0;
        let mut index = (self.position + CIC_TAPS - 1) % CIC_TAPS;
        for coefficient in CIC_COEFFICIENTS {
            output += self.history[index] * f64::from(coefficient);
            index = if index == 0 { CIC_TAPS - 1 } else { index - 1 };
        }
        output / 8192.0
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FloatChannel {
    word: u32,
    phase: u32,
    i: FloatCic,
    q: FloatCic,
}

/// Floating oscillator/CIC reference for bounded comparisons. Its oscillator
/// has the same u32 phase increment but uses full sine/cosine precision. A
/// bounded 58-tap convolution implements the equivalent CIC response without
/// indefinitely growing floating integrators and their cancellation error.
#[derive(Clone, Debug)]
pub(crate) struct FloatBank {
    channels: [FloatChannel; MAX_CHANNELS],
    count: usize,
    accepted_modulo: usize,
}

impl FloatBank {
    pub(crate) fn new(words: &[u32]) -> Result<Self> {
        ensure!(
            !words.is_empty() && words.len() <= MAX_CHANNELS,
            "select one through four channels"
        );
        let mut channels = [FloatChannel::default(); MAX_CHANNELS];
        for (channel, word) in channels.iter_mut().zip(words) {
            channel.word = *word;
        }
        Ok(Self {
            channels,
            count: words.len(),
            accepted_modulo: 0,
        })
    }

    pub(crate) fn push(&mut self, sample: Iq16) -> Option<[Iq; MAX_CHANNELS]> {
        for channel in &mut self.channels[..self.count] {
            let angle = f64::from(channel.phase) * TAU / 4_294_967_296.0;
            let (sin, cos) = angle.sin_cos();
            let i = f64::from(sample.i);
            let q = f64::from(sample.q);
            channel.i.integrate(i * cos + q * sin);
            channel.q.integrate(q * cos - i * sin);
            channel.phase = channel.phase.wrapping_add(channel.word);
        }
        self.accepted_modulo += 1;
        if self.accepted_modulo < CIC_DECIMATION {
            return None;
        }
        self.accepted_modulo = 0;
        let mut output = [Iq::default(); MAX_CHANNELS];
        for (sample, channel) in output.iter_mut().zip(&mut self.channels[..self.count]) {
            *sample = Iq {
                i: channel.i.decimate(),
                q: channel.q.decimate(),
            };
        }
        Some(output)
    }
}

/// Ideal normalized amplitude of the three-stage CIC before output quantization.
/// This includes the hardware gain 8000/8192. Frequency is relative to the
/// selected channel; the response is periodic and does not imply ideal isolation.
pub(crate) fn cic_amplitude(frequency_hz: f64, sample_rate_hz: u32) -> Result<f64> {
    ensure!(
        sample_rate_hz > 0
            && frequency_hz.is_finite()
            && frequency_hz.abs() <= f64::from(sample_rate_hz) / 2.0,
        "invalid response frequency/rate"
    );
    let x = std::f64::consts::PI * frequency_hz / f64::from(sample_rate_hz);
    let ratio = if x.sin().abs() < 1e-12 {
        20.0
    } else {
        (20.0 * x).sin() / x.sin()
    };
    Ok(ratio.abs().powi(3) / 8192.0)
}

const CHANNEL_FIR_TAPS: usize = 257;
const AUDIO_FIR_TAPS: usize = 257;
pub(crate) const CHANNEL_OUTPUT_RATE_HZ: u32 = 250_000;
pub(crate) const AUDIO_OUTPUT_RATE_HZ: u32 = 50_000;

fn lowpass(taps: usize, cutoff_hz: f64, sample_rate_hz: f64) -> Vec<f64> {
    let midpoint = (taps - 1) as f64 / 2.0;
    let mut coefficients = Vec::with_capacity(taps);
    for index in 0..taps {
        let distance = index as f64 - midpoint;
        let sinc = if distance == 0.0 {
            2.0 * cutoff_hz / sample_rate_hz
        } else {
            (TAU * cutoff_hz / sample_rate_hz * distance).sin() / (std::f64::consts::PI * distance)
        };
        let fraction = index as f64 / (taps - 1) as f64;
        let window = 0.42 - 0.5 * (TAU * fraction).cos() + 0.08 * (2.0 * TAU * fraction).cos();
        coefficients.push(sinc * window);
    }
    let sum: f64 = coefficients.iter().sum();
    for coefficient in &mut coefficients {
        *coefficient /= sum;
    }
    coefficients
}

#[derive(Clone, Debug)]
struct FirDecimator {
    coefficients: Vec<f64>,
    history: Vec<Iq>,
    position: usize,
    modulo: usize,
    decimation: usize,
}

impl FirDecimator {
    fn new(taps: usize, cutoff_hz: f64, sample_rate_hz: f64, decimation: usize) -> Self {
        Self {
            coefficients: lowpass(taps, cutoff_hz, sample_rate_hz),
            history: vec![Iq::default(); taps],
            position: 0,
            modulo: 0,
            decimation,
        }
    }

    fn push(&mut self, sample: Iq) -> Option<Iq> {
        self.history[self.position] = sample;
        self.position = (self.position + 1) % self.history.len();
        self.modulo += 1;
        if self.modulo < self.decimation {
            return None;
        }
        self.modulo = 0;
        let mut output = Iq::default();
        let mut index = (self.position + self.history.len() - 1) % self.history.len();
        for coefficient in &self.coefficients {
            output.i += coefficient * self.history[index].i;
            output.q += coefficient * self.history[index].q;
            index = if index == 0 {
                self.history.len() - 1
            } else {
                index - 1
            };
        }
        Some(output)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PostSample {
    /// FIR-selected complex samples at 250 ksample/s, in input Q11 amplitude units.
    pub(crate) iq: Iq,
    /// Mono audio at 50 ksample/s, with a fixed nominal +/-75 kHz deviation scale.
    pub(crate) audio: Option<i16>,
}

/// Host complement of the 20 MSps FPGA channel bank.
///
/// Input is 1 MSps CIC output. A 257-tap Blackman-windowed 100 kHz low-pass
/// precedes complex decimation by four. The FM discriminator is arg(x*conj(prev))
/// scaled for +/-75 kHz deviation. A 30 Hz DC blocker and 257-tap 15 kHz
/// low-pass precede audio decimation by five; 75 us deemphasis runs at 50 kHz.
/// Output is mono, fixed gain 0.7 full scale, without stereo/RDS/AGC/squelch.
///
/// FIRs start at zero; no leading transient is silently removed. Combined FIR
/// group delay is 128/1e6 + 128/250e3 seconds, in addition to the CIC delay.
/// Phase is undefined below one squared input amplitude unit; such samples
/// contribute zero discriminator input and reset its previous-sample state.
#[derive(Clone, Debug)]
pub(crate) struct HostPost {
    mixer_word: u32,
    mixer_phase: u32,
    channel: FirDecimator,
    audio: FirDecimator,
    previous: Option<Iq>,
    dc_input: f64,
    dc_output: f64,
    dc_alpha: f64,
    deemphasis: f64,
    deemphasis_alpha: f64,
    low_power_samples: u64,
    audio_clipped_samples: u64,
}

impl HostPost {
    /// # Errors
    /// Rejects rates other than the first gateware revision's fixed 1 MSps.
    pub(crate) fn new(input_rate_hz: u32) -> Result<Self> {
        Self::with_offset(input_rate_hz, 0.0)
    }

    /// Select a station at a residual offset within a fixed coarse channel.
    /// This rotation precedes the host FIR. It cannot undo aliasing or restore
    /// signals already removed by the FPGA CIC; its passband droop still applies.
    /// # Errors
    /// Requires 1 MSps input and a finite residual offset in -250..=250 kHz.
    pub(crate) fn with_offset(input_rate_hz: u32, residual_hz: f64) -> Result<Self> {
        ensure!(
            input_rate_hz == 1_000_000,
            "host FM postprocessing requires 1 MSps CIC input"
        );
        ensure!(
            residual_hz.is_finite() && residual_hz.abs() <= 250_000.0,
            "residual station offset must be finite and within +/-250 kHz"
        );
        Ok(Self {
            mixer_word: tuning_word(residual_hz, input_rate_hz)?,
            mixer_phase: 0,
            channel: FirDecimator::new(CHANNEL_FIR_TAPS, 100_000.0, 1_000_000.0, 4),
            audio: FirDecimator::new(AUDIO_FIR_TAPS, 15_000.0, 250_000.0, 5),
            previous: None,
            dc_input: 0.0,
            dc_output: 0.0,
            dc_alpha: (-TAU * 30.0 / 250_000.0).exp(),
            deemphasis: 0.0,
            deemphasis_alpha: 1.0 - (-1.0 / (50_000.0 * 75e-6_f64)).exp(),
            low_power_samples: 0,
            audio_clipped_samples: 0,
        })
    }

    pub(crate) fn low_power_samples(&self) -> u64 {
        self.low_power_samples
    }
    pub(crate) fn audio_clipped_samples(&self) -> u64 {
        self.audio_clipped_samples
    }

    pub(crate) fn push(&mut self, sample: Iq16) -> Option<PostSample> {
        self.advance(sample.into())
    }

    /// Also accepts floating-reference CIC output for comparison experiments.
    /// # Errors
    /// Non-finite input is rejected without modifying filter/discriminator state.
    pub(crate) fn push_float(&mut self, sample: Iq) -> Result<Option<PostSample>> {
        ensure!(
            sample.i.is_finite() && sample.q.is_finite(),
            "postprocessor samples must be finite"
        );
        Ok(self.advance(sample))
    }

    fn advance(&mut self, sample: Iq) -> Option<PostSample> {
        let sample = if self.mixer_word != 0 {
            let angle = f64::from(self.mixer_phase) * TAU / 4_294_967_296.0;
            let (sin, cos) = angle.sin_cos();
            self.mixer_phase = self.mixer_phase.wrapping_add(self.mixer_word);
            Iq {
                i: sample.i * cos + sample.q * sin,
                q: sample.q * cos - sample.i * sin,
            }
        } else {
            sample
        };
        let iq = self.channel.push(sample)?;
        let fm = if iq.power() >= 1.0 {
            let angle = self.previous.map_or(0.0, |previous| {
                let real = iq.i * previous.i + iq.q * previous.q;
                let imag = iq.q * previous.i - iq.i * previous.q;
                imag.atan2(real)
            });
            self.previous = Some(iq);
            angle * 250_000.0 / (TAU * 75_000.0)
        } else {
            self.previous = None;
            self.low_power_samples += 1;
            0.0
        };
        let dc = fm - self.dc_input + self.dc_alpha * self.dc_output;
        self.dc_input = fm;
        self.dc_output = dc;
        let audio = self.audio.push(Iq { i: dc, q: 0.0 }).map(|sample| {
            self.deemphasis += self.deemphasis_alpha * (sample.i - self.deemphasis);
            let scaled = (self.deemphasis * 0.7 * 32767.0).round();
            if scaled < f64::from(i16::MIN) || scaled > f64::from(i16::MAX) {
                self.audio_clipped_samples += 1;
            }
            scaled.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
        });
        Some(PostSample { iq, audio })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(value: f64) -> i16 {
        value.round().clamp(-32768.0, 32767.0) as i16
    }

    fn response(coefficients: &[f64], frequency: f64, rate: f64) -> f64 {
        let (mut real, mut imag) = (0.0, 0.0);
        for (index, coefficient) in coefficients.iter().enumerate() {
            let phase = -TAU * frequency / rate * index as f64;
            real += coefficient * phase.cos();
            imag += coefficient * phase.sin();
        }
        real.hypot(imag)
    }

    #[test]
    fn impulse_matches_independent_boxcar_polynomial_and_signed_rounding() {
        // Three length-20 boxes give coefficients C(n+2,2) until n=19;
        // at n=39 subtract 3*C(n-18,2). These yield 210 and 190.
        let mut positive = FixedBank::new(&[0]).unwrap();
        let mut negative = FixedBank::new(&[0]).unwrap();
        let mut p = Vec::new();
        let mut n = Vec::new();
        for index in 0..80 {
            let a = positive.push(Iq16 {
                i: if index == 0 { 32767 } else { 0 },
                q: 0,
            });
            let b = negative.push(Iq16 {
                i: if index == 0 { -32768 } else { 0 },
                q: 0,
            });
            assert_eq!(a.is_some(), index % 20 == 19);
            if let Some(output) = a {
                p.push(output.samples[0].i);
            }
            if let Some(output) = b {
                n.push(output.samples[0].i);
            }
        }
        assert_eq!(
            p,
            vec![
                ((32766_i64 * 210) >> 13) as i16,
                ((32766_i64 * 190) >> 13) as i16,
                0,
                0
            ]
        );
        assert_eq!(
            n,
            vec![
                ((-32767_i64 * 210) >> 13) as i16,
                ((-32767_i64 * 190) >> 13) as i16,
                0,
                0
            ]
        );
    }

    #[test]
    fn modulo_overflow_reset_and_output_clipping_are_explicit() {
        let mut bank = FixedBank::new(&[0]).unwrap();
        let mut wrapped = false;
        for _ in 0..100_000 {
            if let Some(output) = bank.push(Iq16 { i: 32767, q: 0 }) {
                if wrapped {
                    assert_eq!(output.samples[0].i, ((32766_i64 * 8000) >> 13) as i16);
                }
                assert!(!output.clipped[0]);
            }
            wrapped |= bank.channels[0].i.integrators[2] < 0;
        }
        assert!(wrapped, "stimulus must exercise actual 48-bit wrap");
        bank.reset();
        for index in 0..20 {
            let output = bank.push(Iq16::default());
            assert_eq!(output.is_some(), index == 19);
            if let Some(output) = output {
                assert_eq!(output.samples, [Iq16::default(); 4]);
            }
        }
        let mut clipping = FixedBank::new(&[0x2000_0000]).unwrap();
        let mut saw_clip = false;
        for index in 0..1000 {
            let phase = (index * 32) & 255;
            let sin = SIN_Q15[phase];
            let cos = SIN_Q15[(phase + 64) & 255];
            let sign = |x: i16| {
                if x > 0 {
                    32767
                } else if x < 0 {
                    -32768
                } else {
                    0
                }
            };
            if let Some(output) = clipping.push(Iq16 {
                i: sign(cos),
                q: sign(sin),
            }) {
                saw_clip |= output.clipped[0];
                if output.clipped[0] {
                    assert_eq!(output.samples[0].i, i16::MAX);
                }
            }
        }
        assert!(saw_clip);
    }

    #[test]
    fn selected_positive_and_negative_carriers_follow_tuning_sign() {
        let rate = 20_000_000;
        let words = [
            tuning_word(2_500_000.0, rate).unwrap(),
            tuning_word(-2_500_000.0, rate).unwrap(),
        ];
        assert_eq!(words, [0x2000_0000, 0xe000_0000]);
        for (frequency, wanted) in [(2_500_000.0, 0), (-2_500_000.0, 1)] {
            let mut fixed = FixedBank::new(&words).unwrap();
            let mut floating = FloatBank::new(&words).unwrap();
            let mut energies = [0.0; 2];
            let mut differences = Vec::new();
            for index in 0..4000 {
                let phase = TAU * frequency * index as f64 / f64::from(rate);
                let sample = Iq16 {
                    i: component(1000.0 * phase.cos()),
                    q: component(1000.0 * phase.sin()),
                };
                let a = fixed.push(sample);
                let b = floating.push(sample);
                assert_eq!(a.is_some(), b.is_some());
                if let (Some(a), Some(b)) = (a, b)
                    && index > 100
                {
                    for (channel, energy) in energies.iter_mut().enumerate() {
                        *energy += Iq::from(a.samples[channel]).power();
                    }
                    differences.push((f64::from(a.samples[wanted].i) - b[wanted].i).abs());
                }
            }
            assert!(energies[wanted] > energies[1 - wanted] * 10_000.0);
            assert!(differences.iter().copied().fold(0.0, f64::max) < 3.0);
        }
        assert!(tuning_word(f64::NAN, rate).is_err());
        assert!(tuning_word(10_000_000.0, rate).is_err());
        assert!(FixedBank::new(&[]).is_err());
    }

    #[test]
    fn filtering_contract_has_measured_passband_and_alias_rejection() {
        let channel = lowpass(257, 100_000.0, 1_000_000.0);
        assert!(response(&channel, 85_000.0, 1_000_000.0) > 0.99);
        assert!(response(&channel, 125_000.0, 1_000_000.0) < 0.001);
        let audio = lowpass(257, 15_000.0, 250_000.0);
        assert!(response(&audio, 12_000.0, 250_000.0) > 0.99);
        assert!(response(&audio, 20_000.0, 250_000.0) < 0.001);
        let dc = cic_amplitude(0.0, 20_000_000).unwrap();
        assert_eq!(dc, 8000.0 / 8192.0);
        assert!(cic_amplitude(100_000.0, 20_000_000).unwrap() / dc > 0.95);
        assert!(cic_amplitude(900_000.0, 20_000_000).unwrap() / dc < 0.002);
    }

    #[test]
    fn fm_chain_recovers_modulation_and_preserves_output_counts() {
        for offset in [0.0, 150_000.0, -150_000.0] {
            let mut post = HostPost::with_offset(1_000_000, offset).unwrap();
            let mut phase = 0.0;
            let mut audio = Vec::new();
            let mut iq_count = 0;
            for index in 0..60_000 {
                let deviation = 40_000.0 * (TAU * 1000.0 * index as f64 / 1_000_000.0).sin();
                phase += TAU * (offset + deviation) / 1_000_000.0;
                let input = Iq16 {
                    i: component(1000.0 * phase.cos()),
                    q: component(1000.0 * phase.sin()),
                };
                if let Some(sample) = post.push(input) {
                    iq_count += 1;
                    if let Some(sample) = sample.audio {
                        audio.push(sample);
                    }
                }
            }
            assert_eq!(iq_count, 15_000);
            assert_eq!(audio.len(), 3000);
            let magnitude = |frequency: f64| {
                let (mut real, mut imag) = (0.0, 0.0);
                for (index, sample) in audio.iter().enumerate().skip(500) {
                    let phase = TAU * frequency * index as f64 / 50_000.0;
                    real += f64::from(*sample) * phase.cos();
                    imag += f64::from(*sample) * phase.sin();
                }
                real.hypot(imag)
            };
            assert!(magnitude(1000.0) > magnitude(2000.0) * 100.0);
            assert_eq!(post.audio_clipped_samples(), 0);
        }
        assert!(HostPost::new(800_000).is_err());
        assert!(HostPost::with_offset(1_000_000, f64::NAN).is_err());
        assert!(HostPost::with_offset(1_000_000, 250_001.0).is_err());
        let mut silence = HostPost::new(1_000_000).unwrap();
        assert!(
            silence
                .push_float(Iq {
                    i: f64::NAN,
                    q: 0.0
                })
                .is_err()
        );
        for _ in 0..10_000 {
            if let Some(sample) = silence.push(Iq16::default())
                && let Some(audio) = sample.audio
            {
                assert_eq!(audio, 0);
            }
        }
        assert_eq!(silence.low_power_samples(), 2500);
    }
}
