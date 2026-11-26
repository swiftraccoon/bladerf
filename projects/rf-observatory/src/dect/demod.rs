//! Recover conventional DECT S and A fields from a guarded carrier.
//!
//! The sync and binary deviation follow [ETSI EN 300 175-2 §§ 4.6 and 5.4][phy].
//!
//! [phy]: https://www.etsi.org/deliver/etsi_en/300100_300199/30017502/02.09.01_60/en_30017502v020901p.pdf#page=23

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::ops::RangeInclusive;

use crate::dsp::fsk::SampleDelay;
use crate::dsp::{Iq, phase_step};

use super::frame::{self, A_BYTES, Identity, PREFIX_BITS, RFP_SYNC, S_BITS};

pub(super) const BAUD: u64 = 1_152_000;
/// Differential phase describes the midpoint of adjacent channel samples.
pub(super) const DISCRIMINATOR_DELAY_SAMPLES: f64 = 0.5;
const MAX_OFFSET_HZ: f64 = 160_000.0;
const SYNC_DEVIATION_HZ: RangeInclusive<f64> = 140_000.0..=430_000.0;
const MIN_SYNC_QUALITY: f64 = 0.76;
const MAX_PREAMBLE_ERRORS: u32 = 2;
const SUFFIX_MASK: u32 = 0xffff;
// Each pair compares a mark to a space in the exact synchronization suffix.
const SUFFIX_PAIRS: [(usize, usize); 6] =
    [(18, 19), (20, 21), (23, 22), (24, 25), (28, 29), (30, 31)];

/// A checked identity and the extent of its S and A fields on the channel grid.
pub(super) struct Packet {
    pub(super) first_sample: f64,
    pub(super) end_sample_exclusive: f64,
    pub(super) carrier_offset_hz: f64,
    pub(super) spectrum_inverted: bool,
    pub(super) identity: Identity,
}

/// Bounded phase discriminator, clock search and A-field framer.
pub(super) struct Decoder {
    previous: Option<Iq>,
    radians_to_hz: f64,
    samples_per_symbol: f64,
    history: VecDeque<f64>,
    history_limit: usize,
    sync_delays: [SampleDelay; S_BITS],
    settle: u64,
    peak: Option<Fit>,
    pending: VecDeque<Fit>,
    sync_candidates: usize,
    last_valid_sync: Option<u64>,
}

impl Decoder {
    /// Start a fresh decoder on a contiguous channelizer output grid.
    pub(super) fn new(rate_hz: u32) -> Self {
        let samples_per_symbol = f64::from(rate_hz) / BAUD as f64;
        let history_limit = ((PREFIX_BITS + 2) as f64 * samples_per_symbol).ceil() as usize;
        Self {
            previous: None,
            radians_to_hz: f64::from(rate_hz) / TAU,
            samples_per_symbol,
            history: VecDeque::with_capacity(history_limit),
            history_limit,
            sync_delays: std::array::from_fn(|index| {
                SampleDelay::new((S_BITS - index - 1) as f64 * samples_per_symbol)
            }),
            settle: samples_per_symbol.ceil() as u64,
            peak: None,
            pending: VecDeque::new(),
            sync_candidates: 0,
            last_valid_sync: None,
        }
    }

    /// Count distinct synchronization fits before A-field validation.
    pub(super) fn synchronizations(&self) -> usize {
        self.sync_candidates
    }

    /// Advance one channel sample at its consecutive zero-based index.
    pub(super) fn push(&mut self, iq: Iq, index: u64) -> Option<Packet> {
        let previous = self.previous.take();
        let deviation = if iq.power() >= 1.0 {
            self.previous = Some(iq);
            previous.map_or(0.0, |sample| phase_step(sample, iq) * self.radians_to_hz)
        } else {
            0.0
        };
        if self.history.len() == self.history_limit {
            self.history.pop_front();
        }
        self.history.push_back(deviation);

        if let Some(peak) = self.peak
            && index - peak.index > self.settle
        {
            self.peak = None;
            self.sync_candidates += 1;
            self.pending.push_back(peak);
        }
        if let Some(fit) = self.acquire(index)
            && self.peak.is_none_or(|old| fit.quality > old.quality)
        {
            self.peak = Some(fit);
        }

        while let Some(fit) = self.pending.front().copied() {
            let first = fit.index as f64 - (S_BITS as f64 - 0.5) * self.samples_per_symbol;
            let end = first + PREFIX_BITS as f64 * self.samples_per_symbol;
            if (index as f64) < end {
                break;
            }
            self.pending.pop_front();
            if self
                .last_valid_sync
                .is_some_and(|last| fit.index - last <= self.settle)
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
        if (index as f64) < (S_BITS as f64 - 0.5) * self.samples_per_symbol {
            return None;
        }
        let read = |position: usize| {
            self.sync_delays[position]
                .read(&self.history)
                .unwrap_or(0.0)
        };
        // A correct suffix needs the same mark-space ordering in every pair
        // under either polarity. This rejects most noise without a full fit.
        let (mark, space) = SUFFIX_PAIRS[0];
        let difference = read(mark) - read(space);
        if difference == 0.0 {
            return None;
        }
        for &(mark, space) in &SUFFIX_PAIRS[1..] {
            if (read(mark) - read(space)) * difference <= 0.0 {
                return None;
            }
        }

        let values = self
            .sync_delays
            .map(|delay| delay.read(&self.history).unwrap_or(0.0));
        // The last 28 bits contain equal numbers of marks and spaces.
        // Omitting the start of the ramp stabilizes the offset and gain fit.
        let mut sum = 0.0;
        let mut squares = 0.0;
        let mut projection = 0.0;
        for (index, &value) in values.iter().enumerate().skip(4) {
            let sign = if RFP_SYNC & (1 << (S_BITS - 1 - index)) == 0 {
                -1.0
            } else {
                1.0
            };
            sum += value;
            squares += value * value;
            projection += sign * value;
        }
        let count = (S_BITS - 4) as f64;
        let offset_hz = sum / count;
        let gain = projection / count;
        let variance = squares - sum * offset_hz;
        let quality = if variance > 0.0 {
            gain * projection / variance
        } else {
            0.0
        };
        if !SYNC_DEVIATION_HZ.contains(&gain.abs())
            || offset_hz.abs() > MAX_OFFSET_HZ
            || quality < MIN_SYNC_QUALITY
        {
            return None;
        }
        let inverted = gain.is_sign_negative();
        let word = values.iter().fold(0u32, |bits, &value| {
            (bits << 1) | u32::from((value > offset_hz) ^ inverted)
        });
        let mismatch = word ^ RFP_SYNC;
        if mismatch & SUFFIX_MASK != 0 || mismatch.count_ones() > MAX_PREAMBLE_ERRORS {
            return None;
        }
        Some(Fit {
            index,
            inverted,
            offset_hz,
            quality,
        })
    }

    fn decode(&self, fit: Fit, index: u64, first: f64, end: f64) -> Option<Packet> {
        let mut a_field = [0u8; A_BYTES];
        for bit in 0..A_BYTES * 8 {
            let center = first + ((S_BITS + bit) as f64 + 0.5) * self.samples_per_symbol;
            let deviation = SampleDelay::new(index as f64 - center).read(&self.history)?;
            let bit_value = (deviation > fit.offset_hz) ^ fit.inverted;
            a_field[bit / 8] = (a_field[bit / 8] << 1) | u8::from(bit_value);
        }
        let identity = frame::decode(a_field)?;
        Some(Packet {
            first_sample: first,
            end_sample_exclusive: end,
            carrier_offset_hz: fit.offset_hz,
            spectrum_inverted: fit.inverted,
            identity,
        })
    }
}

#[derive(Clone, Copy)]
struct Fit {
    index: u64,
    inverted: bool,
    offset_hz: f64,
    quality: f64,
}
