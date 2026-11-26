//! Recover GMSK phase at symbol boundaries after a discriminator opening.

mod sync;

use std::f64::consts::TAU;

use super::{FLAG, FLAG_BITS, MAX_TIMING_PHASES, SYMBOL_RATE, Status, WireFrame};
use crate::dsp::Iq;
use sync::{Match, Synchronizer};

const TRAINING_SYMBOLS: usize = 24;
const OPENING_SYMBOLS: usize = TRAINING_SYMBOLS + FLAG_BITS;
const TRAINING_SKIP: usize = 8;
const TRAINING_WINDOW: usize = TRAINING_SYMBOLS - TRAINING_SKIP;
const BOUNDARY_RADIUS_SYMBOLS: f64 = 0.8;
const TRACKING_GAIN: f64 = 0.16;
const TRACKING_CLIP: f64 = 0.6;

/// A framed and FCS-checked observation on the filtered IQ grid.
pub(super) struct Recovered {
    frame: WireFrame,
    /// Center of the first opening-flag symbol.
    pub(super) first_sample: u64,
    /// One sample past the center of the last closing-flag symbol.
    pub(super) end_sample_exclusive: u64,
}

impl Recovered {
    /// De-stuffed wire octets including the little-endian FCS.
    pub(super) fn bytes(&self) -> &[u8] {
        &self.frame.bytes[..self.frame.length]
    }
}

/// Bounded secondary decoder seeded by the discriminator's openings.
pub(super) struct Decoder {
    rate_hz: u32,
    history: Vec<Iq>,
    history_count: usize,
    last_opening: Option<u64>,
    synchronizer: Synchronizer,
    tracks: Vec<Track>,
}

impl Decoder {
    /// Initialize at the channelizer's 50 or 96 kSps sample rate.
    pub(super) fn new(rate_hz: u32) -> Self {
        let retained = ((OPENING_SYMBOLS + 2) * rate_hz as usize).div_ceil(SYMBOL_RATE as usize);
        Self {
            rate_hz,
            history: vec![Iq::default(); retained],
            history_count: 0,
            last_opening: None,
            synchronizer: Synchronizer::new(rate_hz),
            tracks: Vec::with_capacity(MAX_TIMING_PHASES),
        }
    }

    /// Advance one contiguous IQ sample and optionally refine a new opening.
    ///
    /// Candidate positions within one symbol of each other share a preamble
    /// fit. A recovered extent can end before this input sample because a
    /// centered boundary observation uses IQ following the symbol.
    pub(super) fn push(&mut self, sample: Iq, index: u64, opening: bool) -> Option<Recovered> {
        let history_len = self.history.len() as u64;
        self.history[(index % history_len) as usize] = sample;
        self.history_count = (self.history_count + 1).min(self.history.len());
        let stored = |at: u64| {
            (at <= index && index - at < self.history_count as u64)
                .then(|| self.history[(at % history_len) as usize])
        };

        if opening {
            let cluster_width = self.rate_hz.div_ceil(SYMBOL_RATE);
            let fresh = self
                .last_opening
                .replace(index)
                .is_none_or(|last| index - last > u64::from(cluster_width));
            if fresh
                && self.tracks.len() < MAX_TIMING_PHASES
                && let Some(found) = self.synchronizer.fit(index, stored)
                && let Some(track) = Track::new(found, self.rate_hz, stored)
            {
                self.tracks.push(track);
            }
        }

        let mut slot = 0;
        let mut recovered = None;
        while let Some(track) = self.tracks.get_mut(slot) {
            match track.advance(stored) {
                Status::Pending => slot += 1,
                Status::Rejected => {
                    self.tracks.swap_remove(slot);
                }
                Status::Closed => {
                    let track = self.tracks.swap_remove(slot);
                    if recovered.is_none() && track.frame.validated_bytes().is_some() {
                        recovered = Some(Recovered {
                            first_sample: track.clock.symbol_center(TRAINING_SYMBOLS),
                            end_sample_exclusive: track.clock.symbol_center(track.next_symbol - 2)
                                + 1,
                            frame: track.frame,
                        });
                    }
                }
            }
        }
        recovered
    }
}

struct Clock {
    start: u64,
    rate_hz: u32,
    carrier_per_sample: f64,
    radius: f64,
    first_lag: i32,
    local_rotation: Vec<Iq>,
}

impl Clock {
    fn new(found: Match, rate_hz: u32) -> Self {
        let carrier_per_sample = -TAU * found.offset_hz / f64::from(rate_hz);
        let radius = BOUNDARY_RADIUS_SYMBOLS * f64::from(rate_hz) / f64::from(SYMBOL_RATE);
        let max_lag = radius.ceil() as i32;
        let first_lag = 1 - max_lag;
        let local_rotation = (first_lag..=max_lag)
            .map(|lag| phasor(carrier_per_sample * f64::from(lag)))
            .collect();
        Self {
            start: found.training_start,
            rate_hz,
            carrier_per_sample,
            radius,
            first_lag,
            local_rotation,
        }
    }

    fn symbol_center(&self, symbol: usize) -> u64 {
        let twice_rate = 2 * u64::from(SYMBOL_RATE);
        let numerator = (2 * symbol as u64 + 1) * u64::from(self.rate_hz);
        self.start + (numerator + twice_rate / 2) / twice_rate
    }

    fn boundary(&self, symbol: usize, sample: impl Fn(u64) -> Option<Iq>) -> Option<Iq> {
        let numerator = symbol as u64 * u64::from(self.rate_hz);
        let offset = numerator / u64::from(SYMBOL_RATE);
        let fraction = (numerator % u64::from(SYMBOL_RATE)) as f64 / f64::from(SYMBOL_RATE);
        let center = self.start + offset;
        let last_positive_lag = (self.radius + fraction).ceil() as u64 - 1;
        sample(center + last_positive_lag)?;
        // A triangular window suppresses noise while favoring samples near the
        // boundary, where the phase largely follows the ideal quarter-turn.
        let mut sum = Iq::default();
        let mut total = 0.0;
        for (rotation, lag) in self.local_rotation.iter().zip(self.first_lag..) {
            let weight = 1.0 - (f64::from(lag) - fraction).abs() / self.radius;
            if weight > 0.0 {
                let rotated = multiply(
                    sample(center.checked_add_signed(i64::from(lag))?)?,
                    *rotation,
                );
                sum.i += weight * rotated.i;
                sum.q += weight * rotated.q;
                total += weight;
            }
        }
        let demodulation =
            self.carrier_per_sample * offset as f64 - (TAU / 4.0) * (symbol % 4) as f64;
        Some(multiply(
            Iq {
                i: sum.i / total,
                q: sum.q / total,
            },
            phasor(demodulation),
        ))
    }
}

struct Track {
    clock: Clock,
    next_symbol: usize,
    squared_carrier: Iq,
    direction: Iq,
    clip: f64,
    signs: u8,
    opening: u8,
    frame: WireFrame,
}

impl Track {
    fn new(found: Match, rate_hz: u32, sample: impl Fn(u64) -> Option<Iq>) -> Option<Self> {
        let direction = phasor(found.phase);
        let clock = Clock::new(found, rate_hz);
        let mut magnitudes = [0.0; TRAINING_WINDOW];
        let mut squared_sum = Iq::default();
        let mut training = [Iq::default(); TRAINING_WINDOW];
        for (offset, value) in training.iter_mut().enumerate() {
            *value = clock.boundary(TRAINING_SKIP + offset, &sample)?;
            magnitudes[offset] = value.power().sqrt();
        }
        magnitudes.sort_unstable_by(f64::total_cmp);
        let middle = TRAINING_WINDOW / 2;
        let clip = TRACKING_CLIP * (magnitudes[middle - 1] + magnitudes[middle]) / 2.0;
        if clip == 0.0 {
            return None;
        }
        for value in training {
            let estimate = squared_estimate(value, clip);
            squared_sum.i += estimate.i;
            squared_sum.q += estimate.q;
        }
        let scale = squared_sum.power().sqrt() / TRAINING_WINDOW as f64;
        let initial = multiply(direction, direction);
        Some(Self {
            clock,
            next_symbol: TRAINING_SKIP,
            squared_carrier: Iq {
                i: initial.i * scale,
                q: initial.q * scale,
            },
            direction,
            clip,
            signs: 0,
            opening: 0,
            frame: WireFrame::default(),
        })
    }

    fn advance(&mut self, sample: impl Fn(u64) -> Option<Iq>) -> Status {
        while let Some(boundary) = self.clock.boundary(self.next_symbol, &sample) {
            let at = self.next_symbol;
            self.next_symbol += 1;
            let sign = self.update_carrier(boundary);
            // The two successive tone signs share their intervening boundary.
            // NRZI therefore compares the phase signs two boundaries apart.
            let bit = sign == (self.signs & 2 != 0);
            self.signs = (self.signs << 1) | u8::from(sign);
            if (TRAINING_SYMBOLS + 1..=OPENING_SYMBOLS).contains(&at) {
                self.opening = (self.opening << 1) | u8::from(bit);
                if at == OPENING_SYMBOLS && self.opening != FLAG {
                    return Status::Rejected;
                }
            } else if at > OPENING_SYMBOLS {
                let status = self.frame.push(bit);
                if !matches!(status, Status::Pending) {
                    return status;
                }
            }
        }
        Status::Pending
    }

    fn update_carrier(&mut self, boundary: Iq) -> bool {
        // After removing the ideal quarter-turn per symbol, the data is a
        // phase sign. Squaring removes that sign so both symbols update the
        // same carrier estimate without feeding back a hard decision.
        let estimate = squared_estimate(boundary, self.clip);
        self.squared_carrier.i += TRACKING_GAIN * (estimate.i - self.squared_carrier.i);
        self.squared_carrier.q += TRACKING_GAIN * (estimate.q - self.squared_carrier.q);
        let angle = self.squared_carrier.q.atan2(self.squared_carrier.i);
        let mut half = phasor(angle * 0.5);
        // Resolve the square-root sign continuously. The unknown initial
        // sign cancels when any two symbol boundaries are compared.
        if dot(self.direction, half) < 0.0 {
            half = Iq {
                i: -half.i,
                q: -half.q,
            };
        }
        self.direction = half;
        dot(boundary, half) > 0.0
    }
}

fn squared_estimate(value: Iq, clip: f64) -> Iq {
    let power = value.power();
    let weight = power.sqrt().min(clip) / power.max(f64::MIN_POSITIVE);
    let squared = multiply(value, value);
    Iq {
        i: squared.i * weight,
        q: squared.q * weight,
    }
}

fn phasor(angle: f64) -> Iq {
    let (q, i) = angle.sin_cos();
    Iq { i, q }
}

fn dot(left: Iq, right: Iq) -> f64 {
    left.i * right.i + left.q * right.q
}

fn multiply(left: Iq, right: Iq) -> Iq {
    Iq {
        i: left.i * right.i - left.q * right.q,
        q: left.i * right.q + left.q * right.i,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ais::demod::tests::{BURST, iq, tones};

    #[test]
    fn tracks_signed_offsets_and_requires_a_complete_checked_frame() {
        let mut corrupt = BURST;
        corrupt[14] ^= 1;
        for rate_hz in [50_000, 96_000] {
            let samples_per_symbol = f64::from(rate_hz) / f64::from(SYMBOL_RATE);
            const PREFIX: usize = 24;
            let opening = PREFIX
                + (31.5 * samples_per_symbol).round() as usize
                + (0.9 * samples_per_symbol).round() as usize / 2;
            for (bytes, closing_flag, offset_hz, valid) in [
                (&BURST, true, -730.0, true),
                (&corrupt, true, 680.0, false),
                (&BURST, true, 580.0, true),
                (&BURST, false, -120.0, false),
            ] {
                let mut decoder = Decoder::new(rate_hz);
                let mut waveform = tones(bytes, closing_flag);
                let tone = waveform[waveform.len() - 1];
                waveform.extend([tone; 2]);
                let samples = [Iq::default(); PREFIX]
                    .into_iter()
                    .chain(iq(&waveform, rate_hz, offset_hz, false));
                let mut frames = 0;
                for (index, sample) in samples.enumerate() {
                    if let Some(frame) = decoder.push(sample, index as u64, index == opening) {
                        assert_eq!(frame.bytes(), BURST);
                        assert!(frame.first_sample < frame.end_sample_exclusive);
                        assert!(frame.end_sample_exclusive <= index as u64 + 1);
                        frames += 1;
                    }
                }
                assert_eq!(frames, usize::from(valid), "{rate_hz} Sps, {offset_hz} Hz");
            }
        }
    }
}
