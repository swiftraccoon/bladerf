//! Recover PMV-107J and PMV-C210 FSK frames from channel-grid phase steps.

use std::collections::VecDeque;
use std::f64::consts::TAU;

use super::frame::{self, C210_PAYLOAD_BYTES, Frame};

const LOOKBACK_MS: u32 = 3;
const MAX_CANDIDATES: usize = 6;
const MIN_FIT_EDGES: usize = 6;
const MIN_TONE_SEPARATION_HZ: f64 = 25_000.0;
const MAX_TONE_SEPARATION_HZ: f64 = 130_000.0;
const MAX_ABSOLUTE_TONE_HZ: f64 = 115_000.0;
const EDGE_TOLERANCE_CHIPS: f64 = 0.25;
const EDGE_PHASE_GAIN: f64 = 0.375;
const WIRE_CAPACITY_BITS: usize = C210_PAYLOAD_BYTES * 8;

/// A checked frame and its approximate sync-to-payload extent on the channel grid.
#[derive(Clone, Copy, Debug)]
pub(super) struct Packet {
    pub(super) frame: Frame,
    pub(super) first_sample: u64,
    pub(super) end_sample_exclusive: u64,
}

#[derive(Clone, Copy)]
enum WireFormat {
    Pmv107j,
    PmvC210,
}

impl WireFormat {
    const ALL: [Self; 2] = [Self::Pmv107j, Self::PmvC210];

    fn chip_us(self) -> f64 {
        match self {
            Self::Pmv107j => 100.0,
            Self::PmvC210 => 52.0,
        }
    }

    fn sync(self) -> (u16, usize) {
        // Use on-air suffixes through the chip immediately before the data.
        // Leaving out the first chips tolerates receiver startup clipping.
        match self {
            Self::Pmv107j => (0b111_1100, 7),
            Self::PmvC210 => (0b0101_0100_1111, 12),
        }
    }

    fn leading_zero_bits(self) -> usize {
        match self {
            Self::Pmv107j => 6,
            Self::PmvC210 => 0,
        }
    }

    fn decode(self, bytes: &[u8; C210_PAYLOAD_BYTES]) -> Option<Frame> {
        match self {
            Self::Pmv107j => frame::decode_107j(bytes),
            Self::PmvC210 => frame::decode_c210(bytes),
        }
    }
}

/// Bounded clock acquisition and differential Manchester packet framing.
pub(super) struct Decoder {
    rate_hz: f64,
    lookback: usize,
    history: VecDeque<f64>,
    ordered: Vec<f64>,
    until_search: usize,
    candidates: Vec<Candidate>,
    reported_end: u64,
}

impl Decoder {
    /// Create an empty decoder for a 250 or 500 kSps channel.
    pub(super) fn new(rate_hz: u32) -> Self {
        let lookback = (rate_hz * LOOKBACK_MS / 1_000) as usize;
        Self {
            rate_hz: f64::from(rate_hz),
            lookback,
            history: VecDeque::with_capacity(lookback),
            ordered: Vec::with_capacity(lookback),
            until_search: lookback,
            candidates: Vec::with_capacity(MAX_CANDIDATES),
            reported_end: 0,
        }
    }

    /// Drop incomplete frames and clock history when the discriminator loses signal.
    pub(super) fn reset(&mut self) {
        self.history.clear();
        self.until_search = self.lookback;
        self.candidates.clear();
    }

    /// Accept a smoothed phase step in radians on a consecutive channel sample index.
    pub(super) fn push(&mut self, angle: f64, index: u64) -> Option<Packet> {
        if self.history.len() == self.lookback {
            self.history.pop_front();
        }
        self.history.push_back(angle);

        let mut packet = None;
        self.candidates.retain_mut(|candidate| {
            if let Some(decoded) = candidate.push(angle, index)
                && packet.is_none()
                && decoded.first_sample >= self.reported_end
            {
                self.reported_end = decoded.end_sample_exclusive;
                packet = Some(decoded);
            }
            matches!(candidate.stage, Stage::Payload(_) | Stage::AwaitEnd(_))
        });

        self.until_search -= 1;
        if self.until_search == 0 {
            self.until_search = self.lookback / LOOKBACK_MS as usize;
            self.acquire(index);
        }
        packet
    }

    fn acquire(&mut self, index: u64) {
        if self.candidates.len() == MAX_CANDIDATES {
            return;
        }
        let origin = index + 1 - self.history.len() as u64;
        self.ordered.clear();
        self.ordered.extend(&self.history);
        let low_index = self.ordered.len() / 5;
        let high_index = 4 * low_index;
        let (_, low, upper) = self
            .ordered
            .select_nth_unstable_by(low_index, f64::total_cmp);
        let (_, high, _) = upper.select_nth_unstable_by(high_index - low_index - 1, f64::total_cmp);
        let hz_per_radian = self.rate_hz / TAU;
        let separation_hz = (*high - *low) * hz_per_radian;
        if !(MIN_TONE_SEPARATION_HZ..=MAX_TONE_SEPARATION_HZ).contains(&separation_hz)
            || low.abs().max(high.abs()) * hz_per_radian > MAX_ABSOLUTE_TONE_HZ
        {
            return;
        }
        let midpoint = (*low + *high) * 0.5;

        for format in WireFormat::ALL {
            let nominal = self.rate_hz * format.chip_us() * 1e-6;
            let Some((first_edge, period)) = fit_clock(&self.history, midpoint, nominal) else {
                continue;
            };
            let mut candidate = Candidate::new(format, origin, first_edge, period, midpoint);
            // Replay the overlapped window to recover sync before its clock was fitted.
            for (offset, &angle) in self.history.iter().enumerate() {
                let _ = candidate.push(angle, origin + offset as u64);
            }
            if matches!(candidate.stage, Stage::Payload(_)) {
                self.candidates.push(candidate);
                if self.candidates.len() == MAX_CANDIDATES {
                    break;
                }
            }
        }
    }
}

fn fit_clock(history: &VecDeque<f64>, midpoint: f64, nominal: f64) -> Option<(f64, f64)> {
    let mut previous = *history.front()? > midpoint;
    let mut run = EdgeFit::default();
    let mut best = EdgeFit::default();
    for (index, &angle) in history.iter().enumerate().skip(1) {
        let high = angle > midpoint;
        if std::mem::replace(&mut previous, high) == high {
            continue;
        }
        let position = index as f64 - 0.5;
        let spacing_chips = (position - run.last) / nominal;
        let chips = spacing_chips.round();
        if !(1.0..=2.0).contains(&chips) || (spacing_chips - chips).abs() > EDGE_TOLERANCE_CHIPS {
            run = EdgeFit::default();
        } else if run.count > 0 {
            run.ordinal += chips;
        }
        run.include(position);
        if run.count > best.count {
            best = run;
        }
    }
    (best.count >= MIN_FIT_EDGES).then(|| best.clock())
}

#[derive(Clone, Copy, Default)]
struct EdgeFit {
    count: usize,
    last: f64,
    ordinal: f64,
    sum_ordinal: f64,
    sum_ordinal_squared: f64,
    sum_position: f64,
    sum_product: f64,
}

impl EdgeFit {
    fn include(&mut self, position: f64) {
        self.count += 1;
        self.last = position;
        self.sum_ordinal += self.ordinal;
        self.sum_ordinal_squared += self.ordinal * self.ordinal;
        self.sum_position += position;
        self.sum_product += self.ordinal * position;
    }

    /// Fit each edge position against its accumulated integer chip count.
    fn clock(self) -> (f64, f64) {
        let count = self.count as f64;
        let ordinal_mean = self.sum_ordinal / count;
        let position_mean = self.sum_position / count;
        let variance = self.sum_ordinal_squared - self.sum_ordinal * ordinal_mean;
        let period = (self.sum_product - self.sum_ordinal * position_mean) / variance;
        (position_mean - ordinal_mean * period, period)
    }
}

struct Candidate {
    format: WireFormat,
    origin: u64,
    next_center: f64,
    period: f64,
    midpoint: f64,
    previous_level: Option<bool>,
    shift: u16,
    chips: usize,
    stage: Stage,
}

enum Stage {
    Sync,
    Payload(Payload),
    AwaitEnd(Packet),
    Done,
}

impl Candidate {
    fn new(format: WireFormat, origin: u64, first_edge: f64, period: f64, midpoint: f64) -> Self {
        Self {
            format,
            origin,
            next_center: (first_edge + 0.5 * period).rem_euclid(period),
            period,
            midpoint,
            previous_level: None,
            shift: 0,
            chips: 0,
            stage: Stage::Sync,
        }
    }

    fn push(&mut self, angle: f64, index: u64) -> Option<Packet> {
        if let Stage::AwaitEnd(packet) = self.stage {
            if index + 1 < packet.end_sample_exclusive {
                return None;
            }
            self.stage = Stage::Done;
            return Some(packet);
        }

        let offset = (index - self.origin) as f64;
        let high = angle > self.midpoint;
        if let Some(previous) = self.previous_level.replace(high)
            && previous != high
        {
            // Manchester transitions correct phase before crystal drift can
            // move the sample point out of the chip.
            let edge = offset - 0.5;
            let expected_edge = self.next_center - self.period * 0.5;
            let delta = edge - expected_edge;
            let error = delta - (delta / self.period).round() * self.period;
            if error.abs() < self.period * EDGE_TOLERANCE_CHIPS {
                self.next_center += error * EDGE_PHASE_GAIN;
            }
        }

        let center = self.next_center;
        if offset < center.round() {
            return None;
        }
        self.next_center += self.period;
        let (sync, width) = self.format.sync();
        let mask = (1 << width) - 1;
        self.shift = ((self.shift << 1) | u16::from(high)) & mask;
        self.chips = (self.chips + 1).min(width);
        // The long constant-tone run in each sync is impossible in data.
        // Retain sync history when a candidate is rejected during window replay.
        if self.chips == width && (self.shift == sync || self.shift == (sync ^ mask)) {
            let first_edge = center - (width as f64 - 0.5) * self.period;
            self.stage = Stage::Payload(Payload {
                first_sample: self.origin + first_edge.round() as u64,
                previous: high,
                first_chip: None,
                bytes: [0; C210_PAYLOAD_BYTES],
                bits: self.format.leading_zero_bits(),
            });
        } else if let Stage::Payload(payload) = &mut self.stage {
            if let Some(first_chip) = payload.first_chip.take() {
                let byte = &mut payload.bytes[payload.bits / 8];
                *byte = (*byte << 1) | u8::from(first_chip == high);
                payload.previous = high;
                payload.bits += 1;
                if payload.bits == WIRE_CAPACITY_BITS {
                    self.stage = self
                        .format
                        .decode(&payload.bytes)
                        .map_or(Stage::Sync, |frame| {
                            Stage::AwaitEnd(Packet {
                                frame,
                                first_sample: payload.first_sample,
                                end_sample_exclusive: self.origin
                                    + (center + 0.5 * self.period).round() as u64,
                            })
                        });
                }
            } else if high == payload.previous {
                self.stage = Stage::Sync;
            } else {
                payload.first_chip = Some(high);
            }
        }
        None
    }
}

struct Payload {
    first_sample: u64,
    previous: bool,
    first_chip: Option<bool>,
    bytes: [u8; C210_PAYLOAD_BYTES],
    bits: usize,
}
