//! Recover Ecowitt FSK timing and integrity-checked packets at 500 kSps.

use std::collections::VecDeque;
use std::f64::consts::TAU;

use crate::dsp::Iq;
use crate::dsp::fsk::{Discriminator, FULL_RATE_DELAY_SAMPLES};

use super::FrameFormat;

const SAMPLE_RATE_HZ: f64 = 500_000.0;
const SMOOTH_DELAY: f64 = FULL_RATE_DELAY_SAMPLES as f64;
// Overlapping windows fit the alternating preamble with room for its edges.
const LOOKBACK: usize = 768;
const HOP: usize = LOOKBACK / 2;
const ALTERNATING_INTERVALS: usize = 10;
const NOMINAL_BIT_SAMPLES: usize = 29;
const BIT_TOLERANCE_SAMPLES: usize = 4;
pub(super) const MIN_BIT_SAMPLES: usize = NOMINAL_BIT_SAMPLES - BIT_TOLERANCE_SAMPLES;
const MAX_SYNC_SEARCH_BITS: usize = 64;
const MAX_CANDIDATES: usize = 6;
const RADIANS_PER_HZ: f64 = TAU / SAMPLE_RATE_HZ;
const TONE_SEPARATION: std::ops::RangeInclusive<f64> =
    (35_000.0 * RADIANS_PER_HZ)..=(115_000.0 * RADIANS_PER_HZ);
const MAX_ABSOLUTE_TONE: f64 = 110_000.0 * RADIANS_PER_HZ;

/// A checked frame and its approximate sync-to-payload extent on the channel grid.
#[derive(Clone, Copy, Debug)]
pub(super) struct Packet<F> {
    pub(super) frame: F,
    pub(super) first_sample: u64,
    pub(super) end_sample_exclusive: u64,
}

/// Bounded channel-grid discriminator, clock acquisition and packet framing.
pub(super) struct Decoder<F, const N: usize> {
    discriminator: Discriminator,
    history: VecDeque<f64>,
    ordered: Vec<f64>,
    until_search: usize,
    candidates: Vec<Candidate<F, N>>,
    reported_end: u64,
}

impl<F: FrameFormat<N>, const N: usize> Decoder<F, N> {
    /// Create a decoder with empty timing and discriminator history.
    pub(super) fn new() -> Self {
        Self {
            discriminator: Discriminator::new(SAMPLE_RATE_HZ as u32),
            history: VecDeque::with_capacity(LOOKBACK),
            ordered: Vec::with_capacity(LOOKBACK),
            until_search: LOOKBACK,
            candidates: Vec::with_capacity(MAX_CANDIDATES),
            reported_end: 0,
        }
    }

    /// Advance one channel sample with a consecutive zero-based index.
    ///
    /// Phase steps are smoothed to suppress isolated crossings. Preamble
    /// transitions establish both the tone midpoint and the bit clock, so a
    /// carrier offset can move both tones to the same side of zero.
    pub(super) fn push(&mut self, iq: Iq, index: u64) -> Option<Packet<F>> {
        let Some(angle) = self.discriminator.push(iq) else {
            self.history.clear();
            self.candidates.clear();
            self.until_search = LOOKBACK;
            return None;
        };
        if self.history.len() == LOOKBACK {
            self.history.pop_front();
        }
        self.history.push_back(angle);

        let mut packet = None;
        self.candidates.retain_mut(|candidate| {
            if let Some(result) = candidate.push(angle, index)
                && result.first_sample >= self.reported_end
                && packet.is_none()
            {
                self.reported_end = result.end_sample_exclusive;
                packet = Some(result);
            }
            !matches!(candidate.stage, Stage::Done)
        });

        self.until_search -= 1;
        if self.until_search == 0 {
            self.until_search = HOP;
            if self.candidates.len() < MAX_CANDIDATES {
                self.acquire(index);
            }
        }
        packet
    }

    fn acquire(&mut self, index: u64) {
        let origin = index + 1 - self.history.len() as u64;
        self.ordered.clear();
        self.ordered.extend(&self.history);
        let low_index = self.ordered.len() / 5;
        let high_index = 4 * low_index;
        let (_, low, upper) = self
            .ordered
            .select_nth_unstable_by(low_index, f64::total_cmp);
        let (_, high, _) = upper.select_nth_unstable_by(high_index - low_index - 1, f64::total_cmp);
        let (low, high) = (*low, *high);
        if !TONE_SEPARATION.contains(&(high - low)) || low.abs().max(high.abs()) > MAX_ABSOLUTE_TONE
        {
            return;
        }
        let midpoint = (low + high) * 0.5;
        let Some((first_edge, period)) = alternating_clock(&self.history, midpoint) else {
            return;
        };
        let mut candidate = Candidate::new(origin, first_edge, period, midpoint);
        // The acquisition window is shorter than one full frame. Replay its
        // samples into the candidate to retain a sync already in the window.
        for (offset, &angle) in self.history.iter().enumerate() {
            let _ = candidate.push(angle, origin + offset as u64);
        }
        if !matches!(candidate.stage, Stage::Done) {
            self.candidates.push(candidate);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wh51::TestFrame;

    // Public payload from the pinned rtl_433 frame description.
    const PAYLOAD: [u8; 14] = [
        0x51, 0x00, 0x6b, 0x58, 0x6e, 0x7f, 0x24, 0xf8, 0xd2, 0xff, 0xff, 0xff, 0x3c, 0x28,
    ];

    #[test]
    fn recovers_same_side_of_center_and_keeps_separate_occurrences() {
        let mut invalid = PAYLOAD;
        // Keep the additive sum valid while invalidating the CRC byte.
        invalid[12] += 1;
        invalid[13] += 1;

        let mut decoder = Decoder::<TestFrame, { PAYLOAD.len() }>::new();
        let mut index = 0;
        let mut packets = Vec::new();
        let mut advance = |iq| {
            packets.extend(decoder.push(iq, index));
            index += 1;
        };
        for payload in [PAYLOAD, invalid, PAYLOAD] {
            let mut phase = 0.0;
            let bytes = [0xaa; 4]
                .into_iter()
                .chain([0xaa, 0x2d, 0xd4])
                .chain(payload);
            for byte in bytes {
                for bit in (0..8).rev() {
                    let tone_hz = if byte & (1 << bit) == 0 {
                        10_000.0
                    } else {
                        80_000.0
                    };
                    for _ in 0..NOMINAL_BIT_SAMPLES {
                        phase += tone_hz * RADIANS_PER_HZ;
                        let (sin, cos) = phase.sin_cos();
                        advance(Iq {
                            i: 100.0 * cos,
                            q: 100.0 * sin,
                        });
                    }
                }
            }
            for _ in 0..96 {
                advance(Iq::default());
            }
        }
        assert_eq!(packets.len(), 2, "{packets:?}");
        assert_eq!(Some(packets[0].frame), TestFrame::decode(&PAYLOAD));
        assert_eq!(packets[0].frame, packets[1].frame);
        assert!(packets[0].end_sample_exclusive < packets[1].first_sample);
    }
}

fn alternating_clock(history: &VecDeque<f64>, midpoint: f64) -> Option<(f64, f64)> {
    let mut run = EdgeFit::default();
    let mut best = EdgeFit::default();
    let mut previous = history.front().map(|&angle| angle > midpoint)?;
    for (index, &angle) in history.iter().enumerate().skip(1) {
        let high = angle > midpoint;
        if std::mem::replace(&mut previous, high) == high {
            continue;
        }
        let separation = index - run.last;
        let allowed = MIN_BIT_SAMPLES..=(NOMINAL_BIT_SAMPLES + BIT_TOLERANCE_SAMPLES);
        if run.count > 0 && !allowed.contains(&separation) {
            run = EdgeFit::default();
        }
        run.include(index);
        if run.count > best.count {
            best = run;
        }
    }
    (best.count > ALTERNATING_INTERVALS).then(|| best.clock())
}

#[derive(Clone, Copy, Default)]
struct EdgeFit {
    count: usize,
    last: usize,
    sum_position: f64,
    sum_cross: f64,
}

impl EdgeFit {
    fn include(&mut self, index: usize) {
        self.sum_position += index as f64;
        self.sum_cross += self.count as f64 * index as f64;
        self.last = index;
        self.count += 1;
    }

    /// Fit edge index = first edge + ordinal × bit period.
    fn clock(self) -> (f64, f64) {
        let count = self.count as f64;
        let ordinal_mean = (count - 1.0) * 0.5;
        let ordinal_square_sum = count * (count * count - 1.0) / 12.0;
        let period = (self.sum_cross - ordinal_mean * self.sum_position) / ordinal_square_sum;
        (self.sum_position / count - ordinal_mean * period, period)
    }
}

struct Candidate<F, const N: usize> {
    origin: u64,
    next_center: f64,
    period: f64,
    midpoint: f64,
    stage: Stage<F, N>,
}

enum Stage<F, const N: usize> {
    Seek {
        shift: u32,
        bits: usize,
    },
    Payload {
        first_sample: u64,
        bytes: [u8; N],
        bits: usize,
    },
    AwaitEnd(Packet<F>),
    Done,
}

impl<F: FrameFormat<N>, const N: usize> Candidate<F, N> {
    fn new(origin: u64, first_edge: f64, period: f64, midpoint: f64) -> Self {
        Self {
            origin,
            next_center: first_edge + 0.5 * period,
            period,
            midpoint,
            stage: Stage::Seek { shift: 0, bits: 0 },
        }
    }

    fn push(&mut self, angle: f64, index: u64) -> Option<Packet<F>> {
        if let Stage::AwaitEnd(packet) = self.stage {
            if index + 1 < packet.end_sample_exclusive {
                return None;
            }
            self.stage = Stage::Done;
            return Some(packet);
        }
        let center = self.next_center;
        if index < self.origin + center.round() as u64 {
            return None;
        }
        self.next_center += self.period;
        let high = u8::from(angle > self.midpoint);
        match &mut self.stage {
            Stage::Seek { shift, bits } => {
                *bits += 1;
                *shift = ((*shift << 1) | u32::from(high)) & (u32::MAX >> (32 - F::SYNC_BITS));
                if *bits >= F::SYNC_BITS as usize && *shift == F::SYNC_WORD {
                    let first_edge = center - (f64::from(F::SYNC_BITS) - 0.5) * self.period;
                    self.stage = Stage::Payload {
                        first_sample: self.grid_sample(first_edge),
                        bytes: [0; N],
                        bits: 0,
                    };
                } else if *bits == MAX_SYNC_SEARCH_BITS {
                    self.stage = Stage::Done;
                }
            }
            Stage::Payload {
                first_sample,
                bytes,
                bits,
            } => {
                let byte = &mut bytes[*bits / 8];
                *byte = (*byte << 1) | high;
                *bits += 1;
                if *bits == 8 && bytes[0] != F::FAMILY {
                    self.stage = Stage::Done;
                } else if *bits == N * 8 {
                    let first_sample = *first_sample;
                    let frame = F::decode(bytes);
                    let end_sample_exclusive = self.grid_sample(center + 0.5 * self.period);
                    self.stage = frame.map_or(Stage::Done, |frame| {
                        Stage::AwaitEnd(Packet {
                            frame,
                            first_sample,
                            end_sample_exclusive,
                        })
                    });
                }
            }
            Stage::AwaitEnd(_) | Stage::Done => {}
        }
        None
    }

    fn grid_sample(&self, edge: f64) -> u64 {
        (self.origin + edge.round() as u64).saturating_sub(SMOOTH_DELAY as u64)
    }
}
