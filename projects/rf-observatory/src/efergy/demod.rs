//! Recover Efergy e2 Classic pulse-width FSK frames on a channel grid.
//!
//! The [pinned format] describes the sync, two pulse widths and byte-sum check.
//! The [public IQ] also shows that the runs immediately after sync form pairs
//! of complementary widths. Verify both widths to reject accidental framing.
//!
//! [pinned format]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/efergy_e2_classic.c#L10-L94
//! [public IQ]: https://github.com/merbanan/rtl_433_tests/tree/1bd5af52f389f0150bda566437e7d5e17e11fc7f/tests/efergy_e2_classic/01

use std::f64::consts::TAU;
use std::ops::RangeInclusive;

use serde::Serialize;

use crate::dsp::Iq;
use crate::dsp::fsk::Discriminator;

const BODY_BYTES: usize = 8;
const BODY_RUNS: usize = BODY_BYTES * 8 * 2;
const MAX_BURST_US: u32 = 32_000;
const MAX_RUNS: usize = 512;
const QUIET_US: u32 = 160;
const STABLE_US: u32 = 12;
const SYNC_US: RangeInclusive<u32> = 380..=630;
const SHORT_US: RangeInclusive<u32> = 36..=91;
const LONG_US: RangeInclusive<u32> = 105..=178;
const CELL_US: RangeInclusive<u32> = 165..=245;
const TONE_SEPARATION_HZ: RangeInclusive<f64> = 65_000.0..=125_000.0;

/// Checksum-valid transmitter data. Current is the raw mantissa and exponent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Frame {
    id: u16,
    interval_seconds: u8,
    battery_ok: bool,
    learning: bool,
    current_mantissa: u16,
    current_exponent: i8,
}

impl Frame {
    fn parse(mut bytes: [u8; BODY_BYTES]) -> Option<Self> {
        if bytes[0] >> 4 == 0xf {
            bytes = bytes.map(|byte| !byte);
        }
        let [
            header,
            id_low,
            id_high,
            flags,
            current_high,
            current_low,
            exponent,
            check,
        ] = bytes;
        let sum: u16 = bytes[..BODY_BYTES - 1].iter().copied().map(u16::from).sum();
        let current_exponent = i8::from_ne_bytes([exponent]);
        if header >> 4 != 0
            || sum == 0
            || bytes.iter().filter(|&&byte| byte == 0).count() > 5
            || sum as u8 != check
            || !(-8..=8).contains(&current_exponent)
        {
            return None;
        }
        Some(Self {
            id: u16::from_le_bytes([id_low, id_high]),
            interval_seconds: ((flags >> 4 & 3) + 1) * 6,
            battery_ok: flags & 0x40 != 0,
            learning: flags & 0x80 != 0,
            current_mantissa: u16::from_be_bytes([current_high, current_low]),
            current_exponent,
        })
    }
}

/// Bounded burst discriminator and pulse-width frame recovery.
pub(super) struct Decoder {
    rate_hz: u32,
    discriminator: Discriminator,
    phase: Vec<f64>,
    ordered: Vec<f64>,
    edges: Vec<usize>,
    start_sample: u64,
    mean_power: f64,
    fast_power: f64,
    carrier_power: f64,
    noise_power: f64,
    quiet_samples: usize,
    quiet_limit: usize,
    burst_limit: usize,
}

impl Decoder {
    /// Start a decoder for consecutive 250 or 500 kSps channel samples.
    pub(super) fn new(rate_hz: u32) -> Self {
        let burst_limit = samples(rate_hz, MAX_BURST_US);
        Self {
            rate_hz,
            discriminator: Discriminator::new(rate_hz),
            phase: Vec::with_capacity(burst_limit),
            ordered: Vec::with_capacity(burst_limit),
            edges: Vec::with_capacity(MAX_RUNS + 1),
            start_sample: 0,
            mean_power: 0.0,
            fast_power: 0.0,
            carrier_power: 0.0,
            noise_power: 1.0,
            quiet_samples: 0,
            quiet_limit: samples(rate_hz, QUIET_US),
            burst_limit,
        }
    }

    /// Advance a channel sample, emitting each complete body and its extent.
    ///
    /// Extents are half-open on the incoming sample grid and compensate for
    /// discriminator smoothing. The caller handles channelizer latency.
    pub(super) fn push(&mut self, sample: Iq, index: u64, mut emit: impl FnMut(Frame, u64, u64)) {
        let tick = 1_000_000.0 / f64::from(self.rate_hz);
        let power = sample.power();
        self.mean_power += (power - self.mean_power) * (tick / 24.0);
        self.fast_power += (power - self.fast_power) * (tick / 8.0);
        if self.phase.is_empty() {
            if self.mean_power <= self.noise_power * 3.5 {
                self.noise_power +=
                    (self.mean_power.max(1.0) - self.noise_power) * (tick / 1_000.0);
                return;
            }
            self.start_sample = index;
        }
        // A strong carrier can take several symbol widths to decay all
        // the way to the noise floor in the attack envelope. Timestamp its
        // loss with a faster envelope relative to the recent carrier peak.
        self.carrier_power = self
            .mean_power
            .max(self.carrier_power * (1.0 - tick / 1_000.0));
        let quiet_power = (self.noise_power * 2.0).max(self.carrier_power * 0.2);
        self.quiet_samples = if self.fast_power <= quiet_power {
            self.quiet_samples + 1
        } else {
            0
        };
        self.phase
            .push(self.discriminator.push(sample).unwrap_or(0.0));
        if self.quiet_samples == self.quiet_limit {
            self.finish(&mut emit);
        } else if self.phase.len() == self.burst_limit {
            self.finish(&mut emit);
            self.noise_power = self.mean_power.max(1.0);
        }
    }

    /// Check the final measured samples. Incomplete symbol pairs are excluded.
    pub(super) fn finish(&mut self, mut emit: impl FnMut(Frame, u64, u64)) {
        self.phase.truncate(self.phase.len() - self.quiet_samples);
        let delay = self.discriminator.delay_samples();
        if let Some(midpoint) = self.midpoint()
            && self.find_edges(midpoint)
        {
            let mut sync = 0;
            while let Some(width) = self.width_us(sync) {
                if SYNC_US.contains(&width)
                    && let Some((frame, end_edge)) = self.body_after(sync)
                {
                    let first = self.start_sample + self.edges[sync + 1] as u64;
                    let end = self.start_sample + self.edges[end_edge] as u64;
                    emit(
                        frame,
                        first.saturating_sub(delay),
                        end.saturating_sub(delay),
                    );
                    sync = end_edge;
                } else {
                    sync += 1;
                }
            }
        }
        self.phase.clear();
        self.carrier_power = 0.0;
        self.quiet_samples = 0;
        self.discriminator = Discriminator::new(self.rate_hz);
    }

    fn midpoint(&mut self) -> Option<f64> {
        if self.phase.len() < samples(self.rate_hz, 10_000) {
            return None;
        }
        self.ordered.clear();
        self.ordered.extend_from_slice(&self.phase);
        let quarter = self.ordered.len() / 4;
        let (_, low, upper) = self.ordered.select_nth_unstable_by(quarter, f64::total_cmp);
        let (_, high, _) = upper.select_nth_unstable_by(2 * quarter - 1, f64::total_cmp);
        let separation_hz = (*high - *low) * f64::from(self.rate_hz) / TAU;
        TONE_SEPARATION_HZ
            .contains(&separation_hz)
            .then_some((*low + *high) * 0.5)
    }

    fn find_edges(&mut self, midpoint: f64) -> bool {
        self.edges.clear();
        self.edges.push(0);
        let stable = samples(self.rate_hz, STABLE_US);
        let mut above = self.phase[0] > midpoint;
        let mut changed = 0;
        for (index, &angle) in self.phase.iter().enumerate().skip(1) {
            if (angle > midpoint) == above {
                changed = 0;
            } else {
                changed += 1;
                if changed == stable {
                    if self.edges.len() == MAX_RUNS {
                        return false;
                    }
                    self.edges.push(index + 1 - stable);
                    above = !above;
                    changed = 0;
                }
            }
        }
        self.edges.push(self.phase.len());
        true
    }

    fn width_us(&self, run: usize) -> Option<u32> {
        let end = *self.edges.get(run + 1)?;
        let width = (end - self.edges[run]) as u32;
        Some(width * (1_000_000 / self.rate_hz))
    }

    fn body_after(&self, sync: usize) -> Option<(Frame, usize)> {
        let first = sync + 1;
        let end_edge = first + BODY_RUNS;
        let mut bytes = [0u8; BODY_BYTES];
        for (bit, run) in (first..end_edge).step_by(2).enumerate() {
            let value = self.symbol(run)?;
            let byte = &mut bytes[bit / 8];
            *byte = (*byte << 1) | u8::from(value);
        }
        if self.symbol(end_edge).is_some() && self.symbol(end_edge + 2).is_some() {
            return None;
        }
        Some((Frame::parse(bytes)?, end_edge))
    }

    fn symbol(&self, run: usize) -> Option<bool> {
        let first = self.width_us(run)?;
        let second = self.width_us(run + 1)?;
        if !CELL_US.contains(&(first + second)) {
            return None;
        }
        if SHORT_US.contains(&first) && LONG_US.contains(&second) {
            Some(true)
        } else if LONG_US.contains(&first) && SHORT_US.contains(&second) {
            Some(false)
        } else {
            None
        }
    }
}

fn samples(rate_hz: u32, duration_us: u32) -> usize {
    (u64::from(rate_hz) * u64::from(duration_us) / 1_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: [u8; 8] = [0x0b, 0x34, 0x12, 0xd0, 0x51, 0x23, 0xfd, 0x92];

    fn emit_frame(
        body: [u8; BODY_BYTES],
        trailing_symbols: usize,
        spur: bool,
        offset_hz: f64,
        mut level: impl FnMut(f64, u32),
    ) {
        let low = -58_000.0 + offset_hz;
        let high = 38_000.0 + offset_hz;
        level(0.0, 800);
        level(high, 500);
        let bits = body
            .into_iter()
            .flat_map(|byte| (0..8).rev().map(move |bit| byte & (1 << bit) != 0))
            .chain(std::iter::repeat_n(true, trailing_symbols));
        for (index, one) in bits.enumerate() {
            if index == 0 && spur {
                // A short excursion inside the first long run mimics the
                // isolated discriminator crossing in the public gfile010 IQ.
                level(low, 48);
                level(high, 8);
                level(low, 80);
            } else {
                level(low, if one { 64 } else { 136 });
            }
            level(high, if one { 136 } else { 64 });
        }
        // Carrier-off bounds the final symbol, even when a zero phase step
        // remains on the same side of the discriminator as the final tone.
        level(0.0, 800);
    }

    #[test]
    fn reports_only_complete_checked_width_frames_in_both_channel_rates() {
        let mut corrupt = BODY;
        corrupt[7] ^= 1;
        let geometries = [250_000, 500_000]
            .into_iter()
            .flat_map(|rate| [-45_000.0, 0.0, 60_000.0].map(|offset_hz| (rate, offset_hz)));
        for (rate, offset_hz) in geometries {
            let mut decoder = Decoder::new(rate);
            let mut frames = Vec::new();
            let mut emit = |frame, start, end| frames.push((frame, start, end));
            let mut phase: f64 = 0.0;
            let mut index = 0u64;
            let mut level = |hz: f64, duration_us| {
                for _ in 0..samples(rate, duration_us) {
                    phase += hz * TAU / f64::from(rate);
                    let (q, i) = phase.sin_cos();
                    let amplitude = if hz == 0.0 { 0.0 } else { 400.0 };
                    let iq = Iq {
                        i: i * amplitude,
                        q: q * amplitude,
                    };
                    decoder.push(iq, index, &mut emit);
                    index += 1;
                }
            };
            emit_frame(corrupt, 0, false, offset_hz, &mut level);
            emit_frame(BODY, 2, false, offset_hz, &mut level);
            emit_frame(BODY, 0, true, offset_hz, &mut level);
            decoder.finish(emit);
            assert_eq!(frames.len(), 1, "{rate} Sps at {offset_hz} Hz: {frames:?}");
            let (frame, first, end) = frames[0];
            assert_eq!(frame.id, 0x1234);
            assert_eq!(frame.interval_seconds, 12);
            assert!(frame.battery_ok && frame.learning);
            assert_eq!(frame.current_mantissa, 0x5123);
            assert_eq!(frame.current_exponent, -3);
            assert!(first > u64::from(rate) / 50);
            assert!(
                (samples(rate, 12_700)..=samples(rate, 12_900)).contains(&((end - first) as usize))
            );
        }
    }
}
