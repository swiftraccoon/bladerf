//! Stream DMR 4FSK with matched filtering and data-sync clock acquisition.
//!
//! Frame geometry, symbol mapping and the square-root raised-cosine filter
//! follow [ETSI TS 102 361-1 §§ 9 and 10.2][air-interface].
//!
//! [air-interface]: https://www.etsi.org/deliver/etsi_ts/102300_102399/10236101/02.07.01_60/ts_10236101v020701p.pdf#page=112

use std::collections::VecDeque;
use std::f64::consts::{PI, TAU};

use crate::dsp::fsk::SampleDelay;
use crate::dsp::{Iq, phase_step};

use super::frame::{self, BURST_BITS, DATA_SYNC_WORDS, Sync, VoiceLinkControl};

pub(super) const BAUD: u64 = 4_800;
const BURST_SYMBOLS: usize = BURST_BITS / 2;
const SYNC_SYMBOLS: usize = 24;
const LAST_SYNC_SYMBOL: f64 = 77.0;
const MAX_OFFSET_HZ: f64 = 5_000.0;
const MIN_OUTER_LEVEL_HZ: f64 = 1_050.0;
const OUTER_LEVEL_HZ: std::ops::RangeInclusive<f64> = MIN_OUTER_LEVEL_HZ..=2_900.0;
// Leave one hertz for rounding in interpolation and the regression sum.
const MIN_SYNC_SAMPLE_MAGNITUDE_HZ: f64 = MIN_OUTER_LEVEL_HZ - 1.0;
const MIN_SYNC_QUALITY: f64 = 0.88;
const MAX_SYNC_SIGN_ERRORS: u32 = ((1.0 - MIN_SYNC_QUALITY) * SYNC_SYMBOLS as f64) as u32;
const ROLLOFF: f64 = 0.2;

/// A verified burst on the matched-filter grid.
pub(super) struct Packet {
    pub(super) first_sample: f64,
    pub(super) end_sample_exclusive: f64,
    pub(super) carrier_offset_hz: f64,
    pub(super) spectrum_inverted: bool,
    pub(super) link_control: VoiceLinkControl,
}

/// Bounded discriminator, fractional symbol sampler, and burst framer.
pub(super) struct Decoder {
    previous: Option<Iq>,
    filter: MatchedFilter,
    radians_to_hz: f64,
    samples_per_symbol: f64,
    history: VecDeque<f64>,
    history_limit: usize,
    sync_delays: [SampleDelay; SYNC_SYMBOLS],
    sync_lookback_samples: u64,
    last_sync_level_sample: Option<u64>,
    patterns: [Pattern; DATA_SYNC_WORDS.len()],
    settling_samples: u64,
    peak: Option<Fit>,
    pending: VecDeque<Fit>,
    synchronizations: usize,
    last_valid_sync: Option<u64>,
}

impl Decoder {
    /// Start a fresh discriminator on the channelizer's contiguous output grid.
    pub(super) fn new(rate_hz: u32) -> Self {
        let samples_per_symbol = f64::from(rate_hz) / BAUD as f64;
        let history_limit = ((BURST_SYMBOLS + 2) as f64 * samples_per_symbol).ceil() as usize;
        Self {
            previous: None,
            filter: MatchedFilter::new(samples_per_symbol),
            radians_to_hz: f64::from(rate_hz) / TAU,
            samples_per_symbol,
            history: VecDeque::with_capacity(history_limit),
            history_limit,
            sync_delays: std::array::from_fn(|index| {
                SampleDelay::new((SYNC_SYMBOLS - index - 1) as f64 * samples_per_symbol)
            }),
            sync_lookback_samples: ((SYNC_SYMBOLS - 1) as f64 * samples_per_symbol).ceil() as u64,
            last_sync_level_sample: None,
            patterns: DATA_SYNC_WORDS.map(|(sync, word)| Pattern::new(sync, word)),
            settling_samples: (0.75 * samples_per_symbol).ceil() as u64,
            peak: None,
            pending: VecDeque::new(),
            synchronizations: 0,
            last_valid_sync: None,
        }
    }

    /// Nominal matched-filter latency and the phase difference's half sample.
    pub(super) fn delay_samples(&self) -> f64 {
        (self.filter.taps.len() / 2) as f64 + 0.5
    }

    /// Number of distinct data-sync fits before body validation.
    pub(super) fn synchronizations(&self) -> usize {
        self.synchronizations
    }

    /// Advance one channel sample with its consecutive zero-based index.
    pub(super) fn push(&mut self, iq: Iq, index: u64) -> Option<Packet> {
        let previous = self.previous.take();
        let deviation = if iq.power() >= 1.0 {
            self.previous = Some(iq);
            previous.map_or(0.0, |previous| {
                phase_step(previous, iq) * self.radians_to_hz
            })
        } else {
            0.0
        };
        let filtered = self.filter.push(deviation);
        if self.history.len() == self.history_limit {
            self.history.pop_front();
        }
        self.history.push_back(filtered);
        if filtered.abs() >= MIN_SYNC_SAMPLE_MAGNITUDE_HZ {
            self.last_sync_level_sample = Some(index);
        }

        if let Some(peak) = self.peak
            && index - peak.index > self.settling_samples
        {
            self.peak = None;
            self.synchronizations += 1;
            self.pending.push_back(peak);
        }
        if let Some(fit) = self.acquire(index)
            && self.peak.is_none_or(|old| fit.quality > old.quality)
        {
            self.peak = Some(fit);
        }

        while let Some(fit) = self.pending.front().copied() {
            let first = fit.index as f64 - (LAST_SYNC_SYMBOL + 0.5) * self.samples_per_symbol;
            let end = first + BURST_SYMBOLS as f64 * self.samples_per_symbol;
            if (index as f64) < end {
                break;
            }
            self.pending.pop_front();
            if self
                .last_valid_sync
                .is_some_and(|last| fit.index - last <= self.settling_samples)
            {
                continue;
            }
            if let Some(packet) = self.decode(fit, index, first, end) {
                self.last_valid_sync = Some(fit.index);
                return Some(packet);
            }
        }
        None
    }

    fn acquire(&self, index: u64) -> Option<Fit> {
        // The ±1 regression gain is half the difference of its two sign
        // means. Linear interpolation stays within the endpoints, so a
        // quiet lookback cannot meet the minimum fitted gain.
        if (index as f64) < (LAST_SYNC_SYMBOL + 0.5) * self.samples_per_symbol
            || self
                .last_sync_level_sample
                .is_none_or(|last| index - last > self.sync_lookback_samples)
        {
            return None;
        }
        let values = self
            .sync_delays
            .map(|delay| delay.read(&self.history).unwrap_or(0.0));
        let sum = values.iter().sum::<f64>();
        let mean = sum / SYNC_SYMBOLS as f64;
        let observed_signs = values
            .iter()
            .fold(0, |mask, &value| (mask << 1) | u32::from(value < mean));
        let eligible = self
            .patterns
            .each_ref()
            .map(|pattern| pattern.compatible_signs(observed_signs));
        if !eligible.contains(&true) {
            return None;
        }
        let variance = values.iter().map(|value| value * value).sum::<f64>() - sum * mean;
        if variance <= 0.0 {
            return None;
        }
        self.patterns
            .iter()
            .zip(eligible)
            .filter_map(|(pattern, eligible)| {
                if !eligible {
                    return None;
                }
                let covariance = values
                    .iter()
                    .zip(pattern.signs)
                    .map(|(value, sign)| value * sign)
                    .sum::<f64>()
                    - sum * pattern.mean;
                let gain = covariance / pattern.variance;
                let offset_hz = mean - gain * pattern.mean;
                let quality = gain * covariance / variance;
                (quality >= MIN_SYNC_QUALITY
                    && OUTER_LEVEL_HZ.contains(&gain.abs())
                    && offset_hz.abs() <= MAX_OFFSET_HZ)
                    .then_some(Fit {
                        index,
                        gain,
                        offset_hz,
                        quality,
                        sync: pattern.sync,
                    })
            })
            .max_by(|left, right| left.quality.total_cmp(&right.quality))
    }

    fn decode(&self, fit: Fit, index: u64, first: f64, end: f64) -> Option<Packet> {
        let mut bits = [0; BURST_BITS];
        for (symbol, pair) in bits.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let center = first + (symbol as f64 + 0.5) * self.samples_per_symbol;
            let deviation = SampleDelay::new(index as f64 - center).read(&self.history)?;
            let level = 3.0 * (deviation - fit.offset_hz) / fit.gain;
            *pair = [u8::from(level < 0.0), u8::from(level.abs() > 2.0)];
        }
        let link_control = frame::decode(&bits, fit.sync)?;
        Some(Packet {
            first_sample: first,
            end_sample_exclusive: end,
            carrier_offset_hz: fit.offset_hz,
            spectrum_inverted: fit.gain.is_sign_negative(),
            link_control,
        })
    }
}

#[derive(Clone, Copy)]
struct Fit {
    index: u64,
    gain: f64,
    offset_hz: f64,
    quality: f64,
    sync: Sync,
}

struct Pattern {
    sync: Sync,
    signs: [f64; SYNC_SYMBOLS],
    negative_signs: u32,
    mean: f64,
    variance: f64,
}

impl Pattern {
    fn new(sync: Sync, word: u64) -> Self {
        let signs = std::array::from_fn(|index| {
            let shift = 2 * (SYNC_SYMBOLS - 1 - index);
            if word & (2 << shift) == 0 { 1.0 } else { -1.0 }
        });
        let sum = signs.iter().sum::<f64>();
        let mean = sum / SYNC_SYMBOLS as f64;
        let variance = SYNC_SYMBOLS as f64 - sum * mean;
        Self {
            sync,
            signs,
            negative_signs: signs
                .iter()
                .fold(0, |mask, &sign| (mask << 1) | u32::from(sign < 0.0)),
            mean,
            variance,
        }
    }

    fn compatible_signs(&self, observed: u32) -> bool {
        if self.mean != 0.0 {
            return true;
        }
        // For a balanced template, center the observed values at their
        // mean. Only terms matching the fitted sign can increase the
        // signed correlation. Cauchy–Schwarz bounds fit quality by the
        // fraction of matching signs. Test both gain polarities.
        let errors = (self.negative_signs ^ observed).count_ones();
        errors.min(SYNC_SYMBOLS as u32 - errors) <= MAX_SYNC_SIGN_ERRORS
    }
}

struct MatchedFilter {
    taps: Vec<f64>,
    history: VecDeque<f64>,
    nonzero_tail: usize,
}

impl MatchedFilter {
    fn new(samples_per_symbol: f64) -> Self {
        let half = (4.0 * samples_per_symbol).ceil() as usize;
        let mut taps: Vec<_> = (0..=2 * half)
            .map(|index| root_raised_cosine((index as f64 - half as f64) / samples_per_symbol))
            .collect();
        let sum = taps.iter().sum::<f64>();
        for tap in &mut taps {
            *tap /= sum;
        }
        Self {
            history: VecDeque::with_capacity(taps.len()),
            taps,
            nonzero_tail: 0,
        }
    }

    fn push(&mut self, deviation: f64) -> f64 {
        if self.history.len() == self.taps.len() {
            self.history.pop_front();
        }
        self.history.push_back(deviation);
        self.nonzero_tail = if deviation == 0.0 {
            self.nonzero_tail.saturating_sub(1)
        } else {
            self.taps.len()
        };
        if self.nonzero_tail == 0 {
            return 0.0;
        }
        let (older, newer) = self.history.as_slices();
        let (newer_taps, older_taps) = self.taps.split_at(newer.len());
        // Walk the contiguous slices in the original newest-to-oldest
        // order so the floating-point sum stays identical across wraps.
        let mut filtered = -0.0;
        for (samples, taps) in [(newer, newer_taps), (older, older_taps)] {
            for (&sample, &tap) in samples.iter().rev().zip(taps) {
                filtered += tap * sample;
            }
        }
        filtered
    }
}

fn root_raised_cosine(time: f64) -> f64 {
    let reduced = 4.0 * ROLLOFF * time;
    if time.abs() < 1e-12 {
        1.0 + ROLLOFF * (4.0 / PI - 1.0)
    } else if (reduced.abs() - 1.0).abs() < 1e-12 {
        let (sin, cos) = (PI / (4.0 * ROLLOFF)).sin_cos();
        ROLLOFF / 2_f64.sqrt() * ((1.0 + 2.0 / PI) * sin + (1.0 - 2.0 / PI) * cos)
    } else {
        ((PI * time * (1.0 - ROLLOFF)).sin() + reduced * (PI * time * (1.0 + ROLLOFF)).cos())
            / (PI * time * (1.0 - reduced * reduced))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_sync_sign_gate_keeps_two_errors_in_both_polarities() {
        let mut decoder = Decoder::new(25_000);
        let mask = (1 << SYNC_SYMBOLS) - 1;
        for pattern in &decoder.patterns {
            assert_eq!(pattern.mean, 0.0);
            let expected = pattern.negative_signs;
            for observed in [expected, expected ^ mask] {
                assert!(pattern.compatible_signs(observed ^ 0b11));
                assert!(!pattern.compatible_signs(observed ^ 0b111));
            }
        }
        decoder.patterns[0].mean = 0.25;
        assert!(decoder.patterns[0].compatible_signs(0));
    }

    #[test]
    fn sync_gate_keeps_the_older_fractional_endpoint() {
        let mut decoder = Decoder::new(25_000);
        let index = 1_000;
        let oldest = decoder.sync_lookback_samples as usize;
        let len = decoder.history_limit;
        decoder.history.resize(len, 0.0);
        let signs = decoder.patterns[0].signs;
        for (symbol, sign) in signs.into_iter().enumerate() {
            let delay = (SYNC_SYMBOLS - symbol - 1) as f64 * decoder.samples_per_symbol;
            let newer = len - 1 - delay.floor() as usize;
            let older = len - 1 - delay.ceil() as usize;
            decoder.history[newer] = 1_048.0 * sign;
            decoder.history[older] = 1_048.0 * sign;
        }
        decoder.history[len - 1 - oldest] = 1_160.0 * signs[0];
        decoder.last_sync_level_sample = Some(index - oldest as u64);
        assert!(decoder.acquire(index).is_some());

        decoder.history[len - 1 - oldest] = 1_048.0 * signs[0];
        decoder.history[len - 2 - oldest] = 1_160.0 * signs[0];
        decoder.last_sync_level_sample = Some(index - oldest as u64 - 1);
        assert!(decoder.acquire(index).is_none());
    }

    #[test]
    fn filter_keeps_the_last_nonzero_tap_before_its_quiet_tail() {
        let mut filter = MatchedFilter::new(25_000.0 / BAUD as f64);
        let width = filter.taps.len();
        let mut inputs = Vec::new();
        for index in 0..3 * width {
            let sample = match index {
                0 => -1_000.0,
                index if index == width + 2 => 250.0,
                _ => 0.0,
            };
            inputs.push(sample);
            let expected = filter
                .taps
                .iter()
                .zip(inputs.iter().rev())
                .map(|(tap, input)| tap * input)
                .sum::<f64>();
            assert_eq!(filter.push(sample), expected);
        }
    }
}
