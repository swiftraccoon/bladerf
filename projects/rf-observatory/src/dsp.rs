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
//! audio low-pass filtering and deemphasis. The full multiplex path supplies
//! separate RDS analysis before audio filtering.
//! These synchronous bounded-state objects perform no radio access or file I/O.

pub(crate) mod fsk;
mod halow;
pub(crate) mod hdlc;
mod lte_raster;
mod raster;
pub(crate) mod reed_solomon;

use std::f64::consts::TAU;

use anyhow::{Result, ensure};

pub(crate) use halow::HalowChannel;
pub(crate) use lte_raster::LteRasterBank;
pub(crate) use raster::RasterBank;

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

/// Iterate the LTE and NR length-31 Gold sequence.
pub(crate) struct GoldSequence {
    x1: u32,
    x2: u32,
}

impl GoldSequence {
    /// Advance one bit in this infinite sequence.
    pub(crate) fn bit(&mut self) -> u8 {
        let output = ((self.x1 ^ self.x2) & 1) as u8;
        let feedback_x1 = (self.x1 ^ (self.x1 >> 3)) & 1;
        let feedback_x2 = (self.x2 ^ (self.x2 >> 1) ^ (self.x2 >> 2) ^ (self.x2 >> 3)) & 1;
        self.x1 = (self.x1 >> 1) | (feedback_x1 << 30);
        self.x2 = (self.x2 >> 1) | (feedback_x2 << 30);
        output
    }
}

impl Iterator for GoldSequence {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.bit())
    }
}

/// Start a Gold sequence at c(0) using the low 31 bits of its seed.
pub(crate) fn gold(seed: u32) -> GoldSequence {
    let mut sequence = GoldSequence {
        x1: 1,
        x2: seed & 0x7fff_ffff,
    };
    for _ in 0..1600 {
        sequence.bit();
    }
    sequence
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

    fn comb(&mut self) -> i64 {
        let mut current = self.integrators[2];
        for delay in &mut self.comb_delays {
            let difference = wrap48(current - *delay);
            *delay = current;
            current = difference;
        }
        current
    }

    fn decimate(&mut self) -> (i16, bool) {
        let shifted = self.comb() >> 13;
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

impl FixedChannel {
    fn mix(&mut self, sample: Iq16) -> [i64; 2] {
        let index = (self.phase >> 24) as usize;
        let sin = i64::from(SIN_Q15[index]);
        let cos = i64::from(SIN_Q15[(index + 64) & 255]);
        let i = i64::from(sample.i);
        let q = i64::from(sample.q);
        self.phase = self.phase.wrapping_add(self.word);
        [i * cos + q * sin, q * cos - i * sin]
    }

    fn mix_interpolated(&mut self, sample: Iq16) -> Iq {
        let mixed = interpolated_mix(sample, self.phase);
        self.phase = self.phase.wrapping_add(self.word);
        mixed
    }
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
            let [i, q] = channel.mix(sample);
            channel.i.integrate(i >> 15);
            channel.q.integrate(q >> 15);
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
    fractional_lowpass(taps, cutoff_hz, sample_rate_hz, 0.0)
}

fn fractional_lowpass(
    taps: usize,
    cutoff_hz: f64,
    sample_rate_hz: f64,
    fractional_delay: f64,
) -> Vec<f64> {
    let midpoint = (taps - 1) as f64 / 2.0;
    let mut coefficients = Vec::with_capacity(taps);
    for index in 0..taps {
        let distance = index as f64 - midpoint - fractional_delay;
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

const LTE_FIR_TAPS: usize = 257;
const LTE_RATE_NUMERATOR: usize = 12;
const LTE_RATE_DENOMINATOR: usize = 125;

fn lte_fractional_filter(phase: usize) -> Vec<f64> {
    fractional_lowpass(
        LTE_FIR_TAPS,
        620_000.0,
        f64::from(crate::SAMPLE_RATE),
        phase as f64 / LTE_RATE_NUMERATOR as f64,
    )
}

fn advance_lte_phase(phase: &mut usize) -> Option<usize> {
    *phase += LTE_RATE_NUMERATOR;
    if *phase < LTE_RATE_DENOMINATOR {
        return None;
    }
    *phase -= LTE_RATE_DENOMINATOR;
    Some(*phase)
}

/// Translate the central LTE synchronization subcarriers onto a 1.92 MSps grid.
/// A 620 kHz low-pass runs on each polyphase output. Input history starts at zero.
pub(crate) struct LteSyncChannel {
    mixer: FixedChannel,
    filters: [Vec<f64>; LTE_RATE_NUMERATOR],
    history: [Iq; LTE_FIR_TAPS],
    oldest: usize,
    phase: usize,
}

impl LteSyncChannel {
    /// Output rate in complex samples per second.
    pub(crate) const OUTPUT_RATE_HZ: u32 =
        crate::SAMPLE_RATE * LTE_RATE_NUMERATOR as u32 / LTE_RATE_DENOMINATOR as u32;
    /// Nominal FIR delay in original-rate input samples.
    pub(crate) const DELAY_INPUT_SAMPLES: usize = (LTE_FIR_TAPS - 1) / 2;

    /// Allocate a translated channel for 20 MSps input.
    ///
    /// # Errors
    /// Rejects offsets without a full 1 MHz margin inside the sampled Nyquist band.
    pub(crate) fn new(offset_hz: f64) -> Result<Self> {
        let sample_rate_hz = f64::from(crate::SAMPLE_RATE);
        ensure!(
            offset_hz.abs() + 1_000_000.0 < sample_rate_hz / 2.0,
            "LTE synchronization channel must fit inside the sampled band"
        );
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz, crate::SAMPLE_RATE)?,
                ..FixedChannel::default()
            },
            filters: std::array::from_fn(lte_fractional_filter),
            history: [Iq::default(); LTE_FIR_TAPS],
            oldest: 0,
            phase: 0,
        })
    }

    /// Advance by one Q11 sample. Output index `n` refers to input position
    /// `(n + 1) * 125 / 12 - 1` before subtracting the nominal FIR delay.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let [i, q] = self.mixer.mix(sample);
        self.history[self.oldest] = Iq {
            i: i as f64 / 32_768.0,
            q: q as f64 / 32_768.0,
        };
        self.oldest = (self.oldest + 1) % self.history.len();
        let phase = advance_lte_phase(&mut self.phase)?;
        let (before, after) = self.history.split_at(self.oldest);
        let mut output = Iq::default();
        for (sample, coefficient) in after.iter().chain(before).rev().zip(&self.filters[phase]) {
            output.i += sample.i * coefficient;
            output.q += sample.q * coefficient;
        }
        Some(output)
    }
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
        self.position += 1;
        if self.position == self.history.len() {
            self.position = 0;
        }
        self.modulo += 1;
        if self.modulo < self.decimation {
            return None;
        }
        self.modulo = 0;
        let mut output = Iq::default();
        let (before, after) = self.history.split_at(self.position);
        let (first, remaining) = self.coefficients.split_at(before.len());
        // Keep tap accumulation newest to oldest across the ring boundary.
        for (coefficients, samples) in [(first, before), (remaining, after)] {
            for (coefficient, sample) in coefficients.iter().zip(samples.iter().rev()) {
                output.i += coefficient * sample.i;
                output.q += coefficient * sample.q;
            }
        }
        Some(output)
    }
}

/// Translate one ADS-B receive channel onto a 4 MSps demodulator grid.
pub(crate) struct AdsbChannel {
    mixer: FixedChannel,
    filter: FirDecimator,
}

impl AdsbChannel {
    /// Complex samples per second on the demodulator's grid.
    pub(crate) const OUTPUT_RATE_HZ: u32 = 4_000_000;
    /// One-sided channel filter cutoff and minimum required RF margin in hertz.
    pub(crate) const HALF_WIDTH_HZ: u64 = 1_300_000;
    /// Nominal filter delay in samples on the output grid.
    pub(crate) const FILTER_DELAY_OUTPUT_SAMPLES: usize = 8;

    /// Select a signed frequency offset and initialize zero-valued filter history.
    ///
    /// # Errors
    /// Rejects rates other than 4 or 20 MSps and channels crossing a Nyquist edge.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: f64) -> Result<Self> {
        ensure!(
            [Self::OUTPUT_RATE_HZ, crate::SAMPLE_RATE].contains(&input_rate_hz),
            "ADS-B replay requires 4 or 20 MSps IQ input"
        );
        ensure!(
            offset_hz.abs() + (Self::HALF_WIDTH_HZ as f64) < f64::from(input_rate_hz) / 2.0,
            "ADS-B channel must fit inside the sampled band"
        );
        let stride = (input_rate_hz / Self::OUTPUT_RATE_HZ) as usize;
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz, input_rate_hz)?,
                ..FixedChannel::default()
            },
            filter: FirDecimator::new(
                2 * Self::FILTER_DELAY_OUTPUT_SAMPLES * stride + 1,
                Self::HALF_WIDTH_HZ as f64,
                f64::from(input_rate_hz),
                stride,
            ),
        })
    }

    /// Number of raw input samples per output interval.
    pub(crate) fn stride(&self) -> usize {
        self.filter.decimation
    }

    /// Nominal FIR delay on the original-rate input grid.
    pub(crate) fn delay_input_samples(&self) -> usize {
        (self.filter.coefficients.len() - 1) / 2
    }

    /// Advance one original IQ sample. Complete intervals emit the last sample's output.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let [i, q] = self.mixer.mix(sample);
        self.filter.push(Iq {
            i: i as f64 / 32_768.0,
            q: q as f64 / 32_768.0,
        })
    }
}

/// Translate and filter a narrow sensor channel before demodulation.
pub(crate) struct SensorChannel {
    mixer: FixedChannel,
    filter: FirDecimator,
}

impl SensorChannel {
    /// One-sided low-pass cutoff in hertz.
    pub(crate) const HALF_WIDTH_HZ: u64 = 90_000;
    /// Maximum output rate in complex samples per second for translated IQ.
    pub(crate) const OUTPUT_RATE_HZ: u32 = 500_000;

    /// Create a channelizer with anti-alias filtering before decimation.
    ///
    /// # Errors
    /// Rejects unsupported input rates or a channel crossing Nyquist.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: f64) -> Result<Self> {
        let taps = match input_rate_hz {
            250_000 | 500_000 | 1_000_000 => 33,
            2_000_000 | 2_359_296 => 49,
            crate::SAMPLE_RATE => 257,
            _ => {
                anyhow::bail!("sensor channelizer requires 0.25, 0.5, 1, 2, 2.359296 or 20 MSps IQ")
            }
        };
        Self::with_taps(input_rate_hz, offset_hz, taps)
    }

    fn with_taps(input_rate_hz: u32, offset_hz: f64, taps: usize) -> Result<Self> {
        ensure!(
            offset_hz.abs() + (Self::HALF_WIDTH_HZ as f64) < f64::from(input_rate_hz) / 2.0,
            "sensor channel must fit inside the sampled band"
        );
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz, input_rate_hz)?,
                ..FixedChannel::default()
            },
            filter: FirDecimator::new(
                taps,
                Self::HALF_WIDTH_HZ as f64,
                f64::from(input_rate_hz),
                (input_rate_hz / Self::OUTPUT_RATE_HZ).max(1) as usize,
            ),
        })
    }

    /// Nominal FIR group delay in original-rate samples.
    pub(crate) fn delay_input_samples(&self) -> u64 {
        (self.filter.coefficients.len() as u64 - 1) / 2
    }

    /// Advance one input sample with oscillator interpolation to suppress images.
    fn push_interpolated(&mut self, sample: Iq16) -> Option<Iq> {
        self.filter.push(self.mixer.mix_interpolated(sample))
    }

    /// Advance one original input sample, emitting on the last sample of a group.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let [i, q] = self.mixer.mix(sample);
        self.filter.push(Iq {
            i: i as f64 / 32_768.0,
            q: q as f64 / 32_768.0,
        })
    }
}

/// Translate and isolate Itron SCM and SCM+ channels for envelope detection.
pub(crate) struct ScmChannel {
    translator: SensorChannel,
    channel_filter: FirDecimator,
    rate_hz: u32,
}

impl ScmChannel {
    /// One-sided passband margin in hertz.
    pub(crate) const HALF_WIDTH_HZ: u64 = SensorChannel::HALF_WIDTH_HZ;

    /// Set up the cascade for a supported original rate and carrier offset.
    ///
    /// # Errors
    /// Rejects an unsupported rate or a channel crossing Nyquist.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: f64) -> Result<Self> {
        let translator = match input_rate_hz {
            // Suppress images near integer multiples of the 500 kHz output
            // rate before they can fold into the selected carrier's passband.
            crate::SAMPLE_RATE => SensorChannel::with_taps(input_rate_hz, offset_hz, 393)?,
            2_400_000 => SensorChannel::with_taps(input_rate_hz, offset_hz, 49)?,
            _ => SensorChannel::new(input_rate_hz, offset_hz)?,
        };
        let rate_hz = input_rate_hz / translator.filter.decimation as u32;
        // Complete adjacent-carrier rejection on the cheaper decimated grid.
        let channel_filter =
            FirDecimator::new(49, Self::HALF_WIDTH_HZ as f64, f64::from(rate_hz), 1);
        Ok(Self {
            translator,
            channel_filter,
            rate_hz,
        })
    }

    /// Complex samples per second on the envelope grid.
    pub(crate) fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Original samples per output interval.
    pub(crate) fn stride(&self) -> u64 {
        self.translator.filter.decimation as u64
    }

    /// Nominal group delay of both filters in original input samples.
    pub(crate) fn delay_input_samples(&self) -> u64 {
        self.translator.delay_input_samples()
            + (self.channel_filter.coefficients.len() / 2) as u64 * self.stride()
    }

    /// Advance one input sample and emit on the last input of an interval.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        self.channel_filter
            .push(self.translator.push_interpolated(sample)?)
    }
}

/// Produce standard and interpolated sensor IQ on the same translation phase.
pub(crate) struct SensorChannelPair {
    channel: SensorChannel,
    interpolated_filter: FirDecimator,
}

impl SensorChannelPair {
    /// Create two same-delay filters for one guarded RF center.
    ///
    /// # Errors
    /// Rejects invalid rates and a second cutoff outside the input or output
    /// sampled band.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: f64, second_cutoff_hz: u64) -> Result<Self> {
        let channel = SensorChannel::new(input_rate_hz, offset_hz)?;
        ensure!(
            second_cutoff_hz >= SensorChannel::HALF_WIDTH_HZ
                && second_cutoff_hz
                    < u64::from(input_rate_hz.min(SensorChannel::OUTPUT_RATE_HZ)) / 2
                && offset_hz.abs() + (second_cutoff_hz as f64) < f64::from(input_rate_hz) / 2.0,
            "second sensor channel must fit inside the sampled band"
        );
        let interpolated_filter = FirDecimator::new(
            channel.filter.coefficients.len(),
            second_cutoff_hz as f64,
            f64::from(input_rate_hz),
            channel.filter.decimation,
        );
        Ok(Self {
            channel,
            interpolated_filter,
        })
    }

    /// Nominal group delay of both filters in original-rate samples.
    pub(crate) fn delay_input_samples(&self) -> u64 {
        self.channel.delay_input_samples()
    }

    /// Advance one original sample and emit both outputs at the same position.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<(Iq, Iq)> {
        // Interpolation suppresses oscillator images that could move a strong
        // off-channel carrier into the second path. Keep the original mixer
        // and coefficient order for the standard detector.
        let interpolated = self
            .interpolated_filter
            .push(interpolated_mix(sample, self.channel.mixer.phase));
        self.channel.push(sample).zip(interpolated)
    }
}

fn interpolated_mix(sample: Iq16, phase: u32) -> Iq {
    let index = (phase >> 24) as usize;
    let fraction = f64::from(phase & 0x00ff_ffff) / 16_777_216.0;
    let interpolate = |index: usize| {
        let first = f64::from(SIN_Q15[index]);
        let next = f64::from(SIN_Q15[(index + 1) & 255]);
        (first + fraction * (next - first)) / 32_768.0
    };
    let sin = interpolate(index);
    let cos = interpolate((index + 64) & 255);
    let i = f64::from(sample.i);
    let q = f64::from(sample.q);
    Iq {
        i: i * cos + q * sin,
        q: q * cos - i * sin,
    }
}

/// Guarded narrowband IQ for passive FSK demodulators.
pub(crate) struct NarrowFskChannel {
    source: NarrowFskSource,
    filter: FirDecimator,
    rate_hz: u32,
    source_stride: u64,
    source_delay: u64,
    interpolate_oscillator: bool,
}

enum NarrowFskSource {
    Narrow(FixedChannel),
    Wide(SensorChannel),
}

impl NarrowFskChannel {
    /// Nominal one-sided channel filter cutoff and required passband margin.
    pub(crate) const HALF_WIDTH_HZ: u64 = 12_000;
    const NARROW_RATE_HZ: u32 = 96_000;
    const WIDE_OUTPUT_RATE_HZ: u32 = 50_000;

    /// Whether the selected channel fits the supported input geometry.
    pub(crate) fn is_covered(input_rate_hz: u32, offset_hz: u64) -> bool {
        match input_rate_hz {
            Self::NARROW_RATE_HZ => {
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

    /// Translate a usable channel from 96 kSps or guarded 20 MSps input.
    ///
    /// # Errors
    /// Rejects unsupported input rates or a channel outside the usable passband.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: i64) -> Result<Self> {
        Self::configure(input_rate_hz, offset_hz, false)
    }

    /// Translate a channel with the interpolated oscillator to suppress images.
    ///
    /// # Errors
    /// Rejects unsupported input rates or a channel outside the usable passband.
    pub(crate) fn interpolated(input_rate_hz: u32, offset_hz: i64) -> Result<Self> {
        Self::configure(input_rate_hz, offset_hz, true)
    }

    fn configure(input_rate_hz: u32, offset_hz: i64, interpolate_oscillator: bool) -> Result<Self> {
        ensure!(
            Self::is_covered(input_rate_hz, offset_hz.unsigned_abs()),
            "narrow FSK needs a usable 96 kSps channel or a guarded 20 MSps channel"
        );
        let (source, intermediate_rate_hz, rate_hz, source_delay) =
            if input_rate_hz == Self::NARROW_RATE_HZ {
                let mixer = FixedChannel {
                    word: tuning_word(offset_hz as f64, input_rate_hz)?,
                    ..FixedChannel::default()
                };
                (
                    NarrowFskSource::Narrow(mixer),
                    input_rate_hz,
                    input_rate_hz,
                    0,
                )
            } else {
                let wide = SensorChannel::new(input_rate_hz, offset_hz as f64)?;
                let delay = wide.delay_input_samples();
                (
                    NarrowFskSource::Wide(wide),
                    SensorChannel::OUTPUT_RATE_HZ,
                    Self::WIDE_OUTPUT_RATE_HZ,
                    delay,
                )
            };
        let decimation = (intermediate_rate_hz / rate_hz) as usize;
        let taps = (24 * decimation + 1).max(81);
        Ok(Self {
            source,
            filter: FirDecimator::new(
                taps,
                Self::HALF_WIDTH_HZ as f64,
                f64::from(intermediate_rate_hz),
                decimation,
            ),
            rate_hz,
            source_stride: u64::from(input_rate_hz / intermediate_rate_hz),
            source_delay,
            interpolate_oscillator,
        })
    }

    /// Complex samples per second on the discriminator grid.
    pub(crate) fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Original samples per completed output interval.
    pub(crate) fn stride(&self) -> u64 {
        self.source_stride * self.filter.decimation as u64
    }

    /// Nominal cascade group delay in original samples.
    pub(crate) fn delay_input_samples(&self) -> u64 {
        self.source_delay + (self.filter.coefficients.len() as u64 - 1) / 2 * self.source_stride
    }

    /// Advance one original sample and emit after each completed interval.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let iq = match &mut self.source {
            NarrowFskSource::Narrow(mixer) if self.interpolate_oscillator => {
                mixer.mix_interpolated(sample)
            }
            NarrowFskSource::Wide(wide) if self.interpolate_oscillator => {
                wide.push_interpolated(sample)?
            }
            NarrowFskSource::Narrow(mixer) => {
                let [i, q] = mixer.mix(sample);
                Iq {
                    i: i as f64 / 32_768.0,
                    q: q as f64 / 32_768.0,
                }
            }
            NarrowFskSource::Wide(wide) => wide.push(sample)?,
        };
        self.filter.push(iq)
    }
}

/// Guarded DECT IQ with anti-alias and adjacent-channel filtering.
pub(crate) struct DectChannel {
    mixer: FixedChannel,
    antialias: Option<FirDecimator>,
    filter: FirDecimator,
    rate_hz: u32,
    stride: u64,
}

impl DectChannel {
    /// One-sided usable span credited by the capture planner.
    pub(crate) const HALF_WIDTH_HZ: u64 = 750_000;
    const NARROW_RATE_HZ: u32 = 4_608_000;
    const WIDE_OUTPUT_RATE_HZ: u32 = 5_000_000;

    /// Whether a carrier's receive span clears the recorded usable-band edges.
    pub(crate) fn is_covered(input_rate_hz: u32, offset_hz: u64) -> bool {
        match input_rate_hz {
            Self::NARROW_RATE_HZ => {
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

    /// Translate a DECT carrier from 4.608 MSps or guarded 20 MSps input.
    ///
    /// # Errors
    /// Rejects unsupported rates and channels outside the usable passband.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: i64) -> Result<Self> {
        ensure!(
            Self::is_covered(input_rate_hz, offset_hz.unsigned_abs()),
            "DECT needs a usable 4.608 MSps channel or a guarded 20 MSps channel"
        );
        let rate_hz = if input_rate_hz == crate::SAMPLE_RATE {
            Self::WIDE_OUTPUT_RATE_HZ
        } else {
            input_rate_hz
        };
        let stride = u64::from(input_rate_hz / rate_hz);
        // Separate anti-alias filtering from adjacent-channel selectivity so
        // the sharper filter runs on the lower-rate grid.
        let antialias = (stride > 1)
            .then(|| FirDecimator::new(41, 2_750_000.0, f64::from(input_rate_hz), stride as usize));
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz as f64, input_rate_hz)?,
                ..FixedChannel::default()
            },
            antialias,
            filter: FirDecimator::new(69, 860_000.0, f64::from(rate_hz), 1),
            rate_hz,
            stride,
        })
    }

    /// Complex samples per second on the discriminator grid.
    pub(crate) fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Original samples per completed output interval.
    pub(crate) fn stride(&self) -> u64 {
        self.stride
    }

    /// Nominal cascade group delay in original samples.
    pub(crate) fn delay_input_samples(&self) -> u64 {
        let first = self
            .antialias
            .as_ref()
            .map_or(0, |filter| (filter.coefficients.len() as u64 - 1) / 2);
        first + (self.filter.coefficients.len() as u64 - 1) / 2 * self.stride
    }

    /// Advance one original sample and emit at the end of an output interval.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let translated = self.mixer.mix_interpolated(sample);
        let intermediate = match &mut self.antialias {
            Some(filter) => filter.push(translated)?,
            None => translated,
        };
        self.filter.push(intermediate)
    }
}

const MIN_CHIRP_INTERMEDIATE_RATE_HZ: u32 = 1_000_000;
const PROBE_FIR_SPAN_CHIPS: usize = 32;
const OVERSAMPLED_FIR_SPAN_CHIPS: usize = 16;
const PACKET_PREFILTER_SPAN_INTERMEDIATE_SAMPLES: usize = 8;

/// Translate a chirp channel with a selectable output sample rate.
///
/// The first stage reduces translated IQ to the higher of 1 MSps and the
/// output rate. Packet replay uses a Blackman FIR to suppress
/// intermediate-rate aliases. Other paths use a three-stage CIC that retains
/// the mixer's fractional precision before normalization. The final Blackman
/// FIR uses a cutoff at half the channel bandwidth.
pub(crate) struct ChirpChannel {
    mixer: FixedChannel,
    input_decimation: usize,
    cic_modulo: usize,
    cic_scale: f64,
    packet_prefilter: Option<FirDecimator>,
    channel: FirDecimator,
}

impl ChirpChannel {
    /// Allocate a downconverter whose output sample rate equals `bandwidth_hz`.
    ///
    /// # Errors
    /// Rejects rates other than 2, 4 or 20 MSps, bandwidths other than
    /// 125, 250 or 500 kHz, and channels that touch or cross a Nyquist edge.
    pub(crate) fn new(input_rate_hz: u32, offset_hz: f64, bandwidth_hz: u32) -> Result<Self> {
        Self::with_rate(
            input_rate_hz,
            offset_hz,
            bandwidth_hz,
            1,
            PROBE_FIR_SPAN_CHIPS,
        )
    }

    /// Allocate a downconverter whose output rate is four times `bandwidth_hz`.
    ///
    /// # Errors
    /// Rejects the same input geometry as [`ChirpChannel::new`].
    pub(crate) fn oversampled(
        input_rate_hz: u32,
        offset_hz: f64,
        bandwidth_hz: u32,
    ) -> Result<Self> {
        Self::with_rate(
            input_rate_hz,
            offset_hz,
            bandwidth_hz,
            4,
            OVERSAMPLED_FIR_SPAN_CHIPS,
        )
    }

    fn with_rate(
        input_rate_hz: u32,
        offset_hz: f64,
        bandwidth_hz: u32,
        samples_per_chip: u32,
        fir_span_chips: usize,
    ) -> Result<Self> {
        ensure!(
            [2_000_000, 4_000_000, 20_000_000].contains(&input_rate_hz),
            "chirp channel input requires 2, 4 or 20 MSps"
        );
        ensure!(
            [125_000, 250_000, 500_000].contains(&bandwidth_hz),
            "chirp channel bandwidth requires 125, 250 or 500 kHz"
        );
        let cutoff_hz = f64::from(bandwidth_hz) / 2.0;
        ensure!(
            offset_hz.abs() + cutoff_hz < f64::from(input_rate_hz) / 2.0,
            "chirp channel must fit strictly inside the sampled band"
        );
        let output_rate_hz = bandwidth_hz * samples_per_chip;
        let intermediate_rate_hz = MIN_CHIRP_INTERMEDIATE_RATE_HZ.max(output_rate_hz);
        let input_decimation = (input_rate_hz / intermediate_rate_hz) as usize;
        let fir_decimation = (intermediate_rate_hz / output_rate_hz) as usize;
        let fir_intervals = fir_span_chips * (intermediate_rate_hz / bandwidth_hz) as usize;
        let packet_prefilter = (samples_per_chip == 4 && input_decimation > 1).then(|| {
            // At 2:1, an even tap count preserves the CIC's fractional output
            // phase. LoRa timing recovery can lose frames when that phase shifts.
            let taps = PACKET_PREFILTER_SPAN_INTERMEDIATE_SAMPLES * input_decimation
                + usize::from(input_decimation != 2);
            FirDecimator::new(
                taps,
                f64::from(intermediate_rate_hz) / 2.0,
                f64::from(input_rate_hz),
                input_decimation,
            )
        });
        Ok(Self {
            mixer: FixedChannel {
                word: tuning_word(offset_hz, input_rate_hz)?,
                ..FixedChannel::default()
            },
            input_decimation,
            cic_modulo: 0,
            cic_scale: 1.0 / (32_768.0 * (input_decimation as f64).powi(3)),
            packet_prefilter,
            channel: FirDecimator::new(
                fir_intervals + 1,
                cutoff_hz,
                f64::from(intermediate_rate_hz),
                fir_decimation,
            ),
        })
    }

    /// Number of input samples per output sample.
    pub(crate) fn decimation(&self) -> usize {
        self.input_decimation * self.channel.decimation
    }

    /// Nominal combined filter group delay in original input samples.
    pub(crate) fn group_delay_input_samples(&self) -> f64 {
        let first_interval_count = self
            .packet_prefilter
            .as_ref()
            .map_or(3 * (self.input_decimation - 1), |filter| {
                filter.coefficients.len() - 1
            });
        let final_interval_count = (self.channel.coefficients.len() - 1) * self.input_decimation;
        (first_interval_count + final_interval_count) as f64 / 2.0
    }

    /// Accept one sample and emit IQ on the last sample of each decimation group.
    /// Startup uses zero-valued filter history.
    pub(crate) fn push(&mut self, sample: Iq16) -> Option<Iq> {
        let [mut i, mut q] = self.mixer.mix(sample);
        if let Some(prefilter) = &mut self.packet_prefilter {
            let filtered = prefilter.push(Iq {
                i: i as f64 / 32_768.0,
                q: q as f64 / 32_768.0,
            })?;
            return self.channel.push(filtered);
        }
        if self.input_decimation > 1 {
            self.mixer.i.integrate(i);
            self.mixer.q.integrate(q);
            self.cic_modulo += 1;
            if self.cic_modulo != self.input_decimation {
                return None;
            }
            self.cic_modulo = 0;
            i = self.mixer.i.comb();
            q = self.mixer.q.comb();
        }
        self.channel.push(Iq {
            i: i as f64 * self.cic_scale,
            q: q as f64 * self.cic_scale,
        })
    }
}

/// Narrowband FM channel for packet radio from a translated CIC output.
/// A 10 kHz complex low-pass precedes decimation to 50 kSps. The output is
/// the differential phase in radians, with no deemphasis or audio squelch.
pub(crate) struct NarrowFm {
    channel: FirDecimator,
    previous: Option<Iq>,
}

impl NarrowFm {
    /// # Errors
    /// The CIC output rate must be 100 kSps or 1 MSps.
    pub(crate) fn new(input_rate_hz: u32) -> Result<Self> {
        let taps = match input_rate_hz {
            100_000 => 129,
            1_000_000 => 513,
            _ => anyhow::bail!("narrow FM requires 100 kSps or 1 MSps CIC input"),
        };
        Ok(Self {
            channel: FirDecimator::new(
                taps,
                10_000.0,
                f64::from(input_rate_hz),
                (input_rate_hz / AUDIO_OUTPUT_RATE_HZ) as usize,
            ),
            previous: None,
        })
    }

    pub(crate) fn push(&mut self, sample: Iq16) -> Option<f64> {
        let iq = self.channel.push(sample.into())?;
        if iq.power() < 1.0 {
            self.previous = None;
            return Some(0.0);
        }
        Some(
            self.previous
                .replace(iq)
                .map_or(0.0, |previous| phase_step(previous, iq)),
        )
    }
}

/// Signed phase change in radians between adjacent complex samples.
pub(crate) fn phase_step(previous: Iq, current: Iq) -> f64 {
    let real = current.i * previous.i + current.q * previous.q;
    let imag = current.q * previous.i - current.i * previous.q;
    imag.atan2(real)
}

/// Sample rate of the channel-centered broadcast FM multiplex.
pub(crate) const FM_MULTIPLEX_RATE_HZ: u32 = 250_000;

fn fm_channel_filter() -> FirDecimator {
    FirDecimator::new(
        CHANNEL_FIR_TAPS,
        100_000.0,
        1_000_000.0,
        (1_000_000 / FM_MULTIPLEX_RATE_HZ) as usize,
    )
}

/// Broadcast FM discriminator for the translated 1 MSps CIC output.
///
/// Decimates the complex channel by four and emits phase steps in radians.
/// The multiplex retains subcarriers and starts with the zero-state FIR transient.
pub(crate) struct FmMultiplex {
    channel: FirDecimator,
    previous: Option<Iq>,
}

impl FmMultiplex {
    /// Construct a discriminator with cleared phase and filter history.
    pub(crate) fn new() -> Self {
        Self {
            channel: fm_channel_filter(),
            previous: None,
        }
    }

    /// Advance by one CIC sample and emit a multiplex sample every fourth input.
    pub(crate) fn push(&mut self, input: Iq16) -> Option<f64> {
        let iq = self.channel.push(input.into())?;
        if iq.power() < 1.0 {
            self.previous = None;
            return Some(0.0);
        }
        Some(
            self.previous
                .replace(iq)
                .map_or(0.0, |previous| phase_step(previous, iq)),
        )
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
            channel: fm_channel_filter(),
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
            let angle = self
                .previous
                .map_or(0.0, |previous| phase_step(previous, iq));
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

    #[test]
    fn lte_channel_preserves_fractional_cadence_and_rejects_aliases() {
        let measure = |tone_hz: f64| {
            let mut channel = LteSyncChannel::new(2_300_000.0).unwrap();
            let mut outputs = 0;
            let mut power = 0.0;
            for index in 0..12_500 {
                let seconds = f64::from(index) / f64::from(crate::SAMPLE_RATE);
                let (sine, cosine) = (TAU * tone_hz * seconds).sin_cos();
                if let Some(iq) = channel.push(Iq16 {
                    i: (1000.0 * cosine).round() as i16,
                    q: (1000.0 * sine).round() as i16,
                }) {
                    outputs += 1;
                    if outputs > 100 {
                        power += iq.power();
                    }
                }
            }
            assert_eq!(outputs, 1_200);
            (power / f64::from(outputs - 100)).sqrt()
        };
        assert!(measure(2_765_000.0) > 975.0);
        assert!(measure(3_755_000.0) < 1.0);
    }

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
    fn chirp_channel_preserves_chip_cadence_and_rejects_the_final_alias() {
        assert_chirp_channel_response(1, ChirpChannel::new);
    }

    #[test]
    fn oversampled_chirp_channel_preserves_cadence_and_rejects_the_adjacent_tone() {
        assert_chirp_channel_response(4, ChirpChannel::oversampled);
    }

    #[test]
    fn oversampled_two_to_one_prefilter_preserves_fractional_output_phase() {
        for (rate, bandwidth, delay) in [
            (2_000_000, 125_000, 135.5),
            (2_000_000, 250_000, 71.5),
            (4_000_000, 500_000, 71.5),
        ] {
            let channel = ChirpChannel::oversampled(rate, 0.0, bandwidth).unwrap();
            assert_eq!(channel.group_delay_input_samples(), delay);
        }
    }

    #[test]
    fn oversampled_wideband_chirp_channel_rejects_the_first_decimation_image() {
        const RATE_HZ: u32 = 20_000_000;
        const OFFSET_HZ: f64 = 2_500_000.0;
        const DESIRED_AMPLITUDE: f64 = 600.0;
        const INTERFERENCE_AMPLITUDE: f64 = 1_800.0;
        const WARMUP_OUTPUTS: usize = 32 * 4;
        const MEASURED_OUTPUTS: usize = 64 * 4;

        for bandwidth_hz in [125_000, 250_000, 500_000] {
            let intermediate_hz = f64::from(1_000_000_u32.max(4 * bandwidth_hz));
            let decimation = (RATE_HZ / (4 * bandwidth_hz)) as usize;
            let band_frequency_hz = -7.0 * f64::from(bandwidth_hz) / 16.0;
            let radians_per_hz = TAU / f64::from(RATE_HZ);
            let measure = |translated_hz: f64, amplitude: f64| {
                let mut channel =
                    ChirpChannel::oversampled(RATE_HZ, OFFSET_HZ, bandwidth_hz).unwrap();
                assert_eq!(channel.decimation(), decimation);
                let delay = channel.group_delay_input_samples();
                let signal_hz = OFFSET_HZ + translated_hz;
                let mut coherent = Iq::default();
                for index in 0..(WARMUP_OUTPUTS + MEASURED_OUTPUTS) * decimation {
                    let (sin, cos) = (signal_hz * radians_per_hz * index as f64).sin_cos();
                    let output = channel.push(Iq16 {
                        i: component(amplitude * cos),
                        q: component(amplitude * sin),
                    });
                    assert_eq!(output.is_some(), (index + 1).is_multiple_of(decimation));
                    if let Some(sample) = output.filter(|_| index >= WARMUP_OUTPUTS * decimation) {
                        let phase = band_frequency_hz * radians_per_hz * (index as f64 - delay);
                        let (sin, cos) = phase.sin_cos();
                        coherent.i += sample.i * cos + sample.q * sin;
                        coherent.q += sample.q * cos - sample.i * sin;
                    }
                }
                let measured = MEASURED_OUTPUTS as f64;
                Iq {
                    i: coherent.i / measured,
                    q: coherent.q / measured,
                }
            };

            let wanted = measure(band_frequency_hz, DESIRED_AMPLITUDE);
            assert!(
                wanted.i > 0.82 * DESIRED_AMPLITUDE,
                "{bandwidth_hz} Hz: {wanted:?}"
            );
            assert!(wanted.q.abs() < 1.0, "{bandwidth_hz} Hz: {wanted:?}");

            // Decimating this tone without sufficient filtering would move it
            // to the same frequency as the desired in-band signal.
            let alias = measure(intermediate_hz + band_frequency_hz, INTERFERENCE_AMPLITUDE);
            assert!(alias.power().sqrt() < 0.20, "{bandwidth_hz} Hz: {alias:?}");
        }
    }

    fn assert_chirp_channel_response(
        samples_per_chip: u32,
        create: fn(u32, f64, u32) -> Result<ChirpChannel>,
    ) {
        const CHIPS: usize = 128;
        const WARMUP_CHIPS: usize = 40;
        const DESIRED: f64 = 600.0;
        const INTERFERER: f64 = 1_200.0;

        for rate in [2_000_000_u32, 4_000_000, 20_000_000] {
            let offset = f64::from(rate) / if rate == 4_000_000 { 8.0 } else { -8.0 };
            let radians_per_sample = TAU / f64::from(rate);
            for bandwidth in [125_000_u32, 250_000, 500_000] {
                let mut channel = create(rate, offset, bandwidth).unwrap();
                let decimation = (rate / (bandwidth * samples_per_chip)) as usize;
                assert_eq!(channel.decimation(), decimation);
                let delay = channel.group_delay_input_samples();
                let quarter_bandwidth = f64::from(bandwidth) / 4.0;
                let mut desired = Iq::default();
                let mut interferer = Iq::default();
                let output_samples = CHIPS * samples_per_chip as usize;
                let warmup_samples = WARMUP_CHIPS * samples_per_chip as usize;

                for index in 0..output_samples * decimation {
                    let time = index as f64 * radians_per_sample;
                    let (desired_q, desired_i) = ((offset + quarter_bandwidth) * time).sin_cos();
                    let (interferer_q, interferer_i) =
                        ((offset + 3.0 * quarter_bandwidth) * time).sin_cos();
                    let output = channel.push(Iq16 {
                        i: component(DESIRED * desired_i + INTERFERER * interferer_i),
                        q: component(DESIRED * desired_q + INTERFERER * interferer_q),
                    });
                    assert_eq!(output.is_some(), (index + 1).is_multiple_of(decimation));
                    if let Some(value) = output.filter(|_| index >= warmup_samples * decimation) {
                        let phase = quarter_bandwidth * (index as f64 - delay) * radians_per_sample;
                        let (sin, cos) = phase.sin_cos();
                        desired.i += value.i * cos + value.q * sin;
                        desired.q += value.q * cos - value.i * sin;
                        let (sin, cos) = (3.0 * phase).sin_cos();
                        interferer.i += value.i * cos + value.q * sin;
                        interferer.q += value.q * cos - value.i * sin;
                    }
                }

                let measured_samples = (output_samples - warmup_samples) as f64;
                assert!(desired.i / measured_samples > 0.9 * DESIRED);
                assert!(desired.q.abs() / measured_samples < 1.0);
                assert!(interferer.i.hypot(interferer.q) / measured_samples < 2.0);
            }
        }
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
    fn paired_sensor_channel_preserves_standard_output_and_admits_an_offset_tone() {
        let input_rate = 20_000_000;
        let offset = 4_000_000.0;
        let phase_step = TAU * (offset + 120_000.0) / f64::from(input_rate);
        let mut standard = SensorChannel::new(input_rate, offset).unwrap();
        let mut paired = SensorChannelPair::new(input_rate, offset, 220_000).unwrap();
        assert_eq!(standard.delay_input_samples(), paired.delay_input_samples());
        let mut standard_power = 0.0;
        let mut wider_power = 0.0;
        for index in 0..2_000 {
            let (sin, cos) = (phase_step * f64::from(index)).sin_cos();
            let input = Iq16 {
                i: (900.0 * cos).round() as i16,
                q: (900.0 * sin).round() as i16,
            };
            let reference = standard.push(input);
            let pair = paired.push(input);
            assert_eq!(reference.is_some(), pair.is_some());
            if let (Some(reference), Some((narrow, wider))) = (reference, pair) {
                assert_eq!(reference.i.to_bits(), narrow.i.to_bits());
                assert_eq!(reference.q.to_bits(), narrow.q.to_bits());
                if index >= 300 {
                    standard_power += narrow.power();
                    wider_power += wider.power();
                }
            }
        }
        assert!(wider_power > 2.0 * standard_power);
    }

    #[test]
    fn wider_sensor_channel_rejects_a_phase_table_image() {
        let input_rate = 20_000_000;
        let phase_step = TAU * 3_998_000.0 / f64::from(input_rate);
        let mut paired = SensorChannelPair::new(input_rate, 3_200_000.0, 220_000).unwrap();
        let mut powers = [0.0; 2];
        for index in 0..4_000 {
            let (sin, cos) = (phase_step * f64::from(index)).sin_cos();
            let input = Iq16 {
                i: (1_850.0 * cos).round() as i16,
                q: (1_850.0 * sin).round() as i16,
            };
            if let Some((standard, wider)) = paired.push(input)
                && index >= 300
            {
                powers[0] += standard.power();
                powers[1] += wider.power();
            }
        }
        assert!(
            powers[0] > 100.0 && powers[1] < powers[0] * 0.01,
            "{powers:?}"
        );
    }

    #[test]
    fn dect_channel_rejects_a_phase_table_image() {
        let input_rate = 20_000_000;
        let carrier_offset = 6_600_000;
        let mut wanted = DectChannel::new(input_rate, carrier_offset).unwrap();
        let mut image = DectChannel::new(input_rate, 915_294).unwrap();
        let step = TAU * carrier_offset as f64 / f64::from(input_rate);
        let mut received_power: f64 = 0.0;
        let mut image_power: f64 = 0.0;
        for index in 0..5_000 {
            let (sin, cos) = (step * f64::from(index)).sin_cos();
            let input = Iq16 {
                i: (1_750.0 * cos).round() as i16,
                q: (1_750.0 * sin).round() as i16,
            };
            let desired = wanted.push(input);
            let suppressed = image.push(input);
            if let (Some(desired), Some(suppressed)) = (desired, suppressed)
                && index >= 500
            {
                received_power = received_power.max(desired.power());
                image_power = image_power.max(suppressed.power());
            }
        }
        assert!(
            received_power > 1_000_000.0 && image_power < 1.0,
            "{received_power} {image_power}"
        );
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
