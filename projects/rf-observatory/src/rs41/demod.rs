//! Recover the 4,800-baud RS41 symbol clock and collect dewhitened frames.

use std::f64::consts::TAU;

use crate::dsp::{Iq, phase_step};

use super::{fec, frame};

pub(super) const BAUD: u64 = 4_800;
const PHASES: u64 = 8;
const SYNC_BITS: usize = frame::AIR_SYNC.len() * 8;
const SYNC_SIGNS: [f64; SYNC_BITS] = sync_signs();
const SYNC_MEAN: f64 = {
    let ones = u64::from_le_bytes(frame::AIR_SYNC).count_ones() as f64;
    2.0 * ones / SYNC_BITS as f64 - 1.0
};
const SYNC_VARIANCE: f64 = SYNC_BITS as f64 * (1.0 - SYNC_MEAN * SYNC_MEAN);
const MIN_CORRELATION_SQUARED: f64 = 0.84;
// The link specifies ±2,400 Hz. Leave room for Gaussian pulse shaping,
// imperfect symbol phase, quantization, and noise in the sync estimate.
const SYNC_DEVIATION_HZ: std::ops::RangeInclusive<f64> = 900.0..=4_000.0;

const fn sync_signs() -> [f64; SYNC_BITS] {
    let mut signs = [-1.0; SYNC_BITS];
    let mut index = 0;
    while index < SYNC_BITS {
        if frame::AIR_SYNC[index / 8] & (1 << (index % 8)) != 0 {
            signs[index] = 1.0;
        }
        index += 1;
    }
    signs
}

struct Lane {
    history: [f64; SYNC_BITS],
    next: usize,
    filled: usize,
    sum: f64,
    squares: f64,
}

impl Lane {
    fn new() -> Self {
        Self {
            history: [0.0; SYNC_BITS],
            next: 0,
            filled: 0,
            sum: 0.0,
            squares: 0.0,
        }
    }

    fn push(&mut self, hz: f64, sample: u64, phase: usize) -> Option<Fit> {
        let previous = std::mem::replace(&mut self.history[self.next], hz);
        self.next = (self.next + 1) % SYNC_BITS;
        self.filled = (self.filled + 1).min(SYNC_BITS);
        self.sum += hz - previous;
        self.squares += hz * hz - previous * previous;
        if self.filled != SYNC_BITS {
            return None;
        }
        let variance = self.squares - self.sum * self.sum / SYNC_BITS as f64;
        // Cauchy–Schwarz bounds the fitted deviation and correlation before
        // evaluating the projection. The margin covers floating-point roundoff.
        let mean_variance = variance / SYNC_VARIANCE;
        if mean_variance < SYNC_DEVIATION_HZ.start().powi(2) * 0.99999
            || mean_variance > SYNC_DEVIATION_HZ.end().powi(2) / MIN_CORRELATION_SQUARED * 1.00001
        {
            return None;
        }
        // Fit the known sync to both frequency polarities. Centering makes the
        // correlation insensitive to a constant carrier error.
        let (early, late) = self.history.split_at(self.next);
        let projection = late
            .iter()
            .chain(early)
            .zip(SYNC_SIGNS.iter())
            .map(|(value, sign)| value * sign)
            .sum::<f64>();
        let covariance = projection - self.sum * SYNC_MEAN;
        let quality = covariance * covariance / (variance * SYNC_VARIANCE);
        let deviation = covariance / SYNC_VARIANCE;
        if !quality.is_finite()
            || quality < MIN_CORRELATION_SQUARED
            || !SYNC_DEVIATION_HZ.contains(&deviation.abs())
        {
            return None;
        }
        Some(Fit {
            phase,
            sample,
            center_hz: self.sum / SYNC_BITS as f64 - deviation * SYNC_MEAN,
            inverted: deviation < 0.0,
            quality,
        })
    }
}

#[derive(Clone, Copy)]
struct Fit {
    phase: usize,
    sample: u64,
    center_hz: f64,
    inverted: bool,
    quality: f64,
}

struct Pending {
    until_tick: u64,
    best: Fit,
}

struct Collecting {
    fit: Fit,
    bytes: [u8; frame::EXTENDED_FRAME_BYTES],
    bit_count: usize,
}

impl Collecting {
    fn new(fit: Fit) -> Self {
        let mut bytes = [0; frame::EXTENDED_FRAME_BYTES];
        bytes[..frame::SYNC.len()].copy_from_slice(&frame::SYNC);
        Self {
            fit,
            bytes,
            bit_count: SYNC_BITS,
        }
    }

    fn push(&mut self, hz: f64, sample: u64, rate: u64) -> Option<Decoded> {
        let index = self.bit_count / 8;
        let bit = (hz > self.fit.center_hz) != self.fit.inverted;
        self.bytes[index] |= u8::from(bit) << (self.bit_count % 8);
        self.bit_count += 1;
        if !self.bit_count.is_multiple_of(8) {
            return None;
        }
        self.bytes[index] = frame::dewhiten_byte(index, self.bytes[index]);
        let length = index + 1;
        if !matches!(
            length,
            frame::STANDARD_FRAME_BYTES | frame::EXTENDED_FRAME_BYTES
        ) {
            return None;
        }

        // Test shortening before using the length marker. A valid extended
        // frame can have a damaged marker, so preserve the full raw prefix.
        let mut corrected = self.bytes;
        let corrected = &mut corrected[..length];
        let corrected_bytes = fec::correct(corrected)?;
        let status = frame::status(corrected)?;
        Some(Decoded {
            first_sample: self
                .fit
                .sample
                .saturating_sub((2 * SYNC_BITS as u64 - 1) * rate / (2 * BAUD)),
            end_sample_exclusive: sample + rate.div_ceil(2 * BAUD),
            status,
            frame_bytes: length,
            corrected_bytes,
            carrier_offset_hz: self.fit.center_hz,
        })
    }
}

/// CRC-checked STATUS and nominal sample bounds on the demodulator grid.
pub(super) struct Decoded {
    pub(super) first_sample: u64,
    pub(super) end_sample_exclusive: u64,
    pub(super) status: frame::Status,
    pub(super) frame_bytes: usize,
    pub(super) corrected_bytes: usize,
    pub(super) carrier_offset_hz: f64,
}

/// One independent channel's discriminator, clock search, and framing state.
pub(super) struct Decoder {
    rate: u64,
    previous: Option<Iq>,
    smoothing: Vec<f64>,
    smoothing_position: usize,
    smooth_sum: f64,
    phase_accumulator: u64,
    tick: u64,
    lanes: [Lane; PHASES as usize],
    pending: Option<Pending>,
    receiving: Option<Collecting>,
    synchronizations: usize,
}

impl Decoder {
    pub(super) fn new(rate_hz: u32) -> Self {
        let rate = u64::from(rate_hz);
        Self {
            rate,
            previous: None,
            smoothing: vec![0.0; (rate / (2 * BAUD)).max(1) as usize],
            smoothing_position: 0,
            smooth_sum: 0.0,
            phase_accumulator: 0,
            tick: 0,
            lanes: std::array::from_fn(|_| Lane::new()),
            pending: None,
            receiving: None,
            synchronizations: 0,
        }
    }

    /// Rounded-up delay of the adjacent-phase moving average in output samples.
    pub(super) fn delay_samples(&self) -> u64 {
        (self.smoothing.len() as u64).div_ceil(2)
    }

    pub(super) fn synchronizations(&self) -> usize {
        self.synchronizations
    }

    pub(super) fn push(&mut self, iq: Iq, sample: u64) -> Option<Decoded> {
        let previous = self.previous.replace(iq)?;
        let hz = phase_step(previous, iq) * (self.rate as f64 / TAU);
        let prior = std::mem::replace(&mut self.smoothing[self.smoothing_position], hz);
        self.smoothing_position = (self.smoothing_position + 1) % self.smoothing.len();
        self.smooth_sum += hz - prior;

        // The rational clock keeps eight candidate phases at both sample rates.
        self.phase_accumulator += PHASES * BAUD;
        if self.phase_accumulator < self.rate {
            return None;
        }
        self.phase_accumulator -= self.rate;
        self.tick += 1;
        let phase = (self.tick % PHASES) as usize;
        let hz = self.smooth_sum / self.smoothing.len() as f64;

        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.until_tick == self.tick)
            && let Some(pending) = self.pending.take()
        {
            self.receiving = Some(Collecting::new(pending.best));
            self.synchronizations += 1;
        }
        if let Some(collector) = &mut self.receiving {
            if phase != collector.fit.phase {
                return None;
            }
            let decoded = collector.push(hz, sample, self.rate);
            if decoded.is_some() || collector.bit_count == frame::EXTENDED_FRAME_BYTES * 8 {
                self.receiving = None;
                self.lanes = std::array::from_fn(|_| Lane::new());
            }
            return decoded;
        }

        if let Some(fit) = self.lanes[phase].push(hz, sample, phase) {
            if let Some(pending) = &mut self.pending {
                if fit.quality > pending.best.quality {
                    pending.best = fit;
                }
            } else {
                // Let all phases measure the same sync before the earliest
                // possible first data bit. Choose the best-correlated phase.
                self.pending = Some(Pending {
                    until_tick: self.tick + PHASES,
                    best: fit,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_shifted_inverted_frame_on_both_sample_grids() {
        // Independent polynomial division and CRC-16/CCITT-FALSE for the
        // synthetic STATUS body (frame number 0x1234 and serial RS41T001).
        const PARITY: [u8; 48] = [
            0x85, 0xcf, 0x5f, 0xff, 0x9c, 0x5f, 0xc1, 0x33, 0x94, 0x5e, 0xb1, 0x0a, 0x13, 0x48,
            0x07, 0x16, 0xa0, 0x8a, 0xcc, 0xc6, 0xcf, 0x5b, 0x1f, 0x43, 0x8e, 0x69, 0x6d, 0x54,
            0x37, 0x0c, 0xe1, 0x71, 0xdc, 0x0a, 0xd2, 0x9e, 0xf9, 0x6c, 0x44, 0xc5, 0x46, 0xac,
            0x26, 0xb6, 0x5e, 0x62, 0x32, 0x01,
        ];
        let mut packet = [0; frame::STANDARD_FRAME_BYTES];
        packet[..8].copy_from_slice(&frame::SYNC);
        packet[8..56].copy_from_slice(&PARITY);
        packet[56..69].copy_from_slice(b"\x0f\x79\x28\x34\x12RS41T001");
        packet[99..101].copy_from_slice(&[0xc1, 0x5e]);
        packet[34] ^= 0x40;
        for (index, byte) in packet.iter_mut().enumerate() {
            *byte = frame::dewhiten_byte(index, *byte);
        }
        for rate in [50_000, 96_000] {
            let mut decoder = Decoder::new(rate);
            let rate = u64::from(rate);
            let prefix = 777;
            let duration = (packet.len() as u64 * 8 * rate).div_ceil(BAUD);
            let mut angle: f64 = 0.0;
            let mut decoded = Vec::new();
            for sample in 0..prefix + duration + 100 {
                let symbol = sample.saturating_sub(prefix) * BAUD / rate;
                let hz = if sample < prefix || symbol >= packet.len() as u64 * 8 {
                    3_000.0
                } else {
                    let bit = (packet[symbol as usize / 8] >> (symbol % 8)) & 1;
                    3_000.0 + if bit == 1 { -2_400.0 } else { 2_400.0 }
                };
                angle += TAU * hz / rate as f64;
                let (q, i) = angle.sin_cos();
                if let Some(frame) = decoder.push(Iq { i, q }, sample) {
                    decoded.push(frame);
                }
            }
            let [frame] = decoded.as_slice() else {
                panic!("expected one RS41 frame at {rate} samples per second");
            };
            assert_eq!(frame.status.serial, "RS41T001");
            assert_eq!(frame.status.frame_number, 0x1234);
            assert_eq!(frame.corrected_bytes, 1);
            assert!(frame.first_sample.abs_diff(prefix) <= rate.div_ceil(BAUD));
        }
    }
}
