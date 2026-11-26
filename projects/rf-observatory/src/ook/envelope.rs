//! Slice smoothed channel power into bounded OOK pulse bursts.

use super::Pulse;

const SETTLE_US: u64 = 80;
// Debounce brief threshold crossings while retaining the shortest accepted lows.
const STABLE_EDGE_US: u64 = 24;
const BURST_GAP_US: u64 = 2_000;
const MAX_HIGH_US: u64 = 2_000;
const MAX_PULSES: usize = 1_024;
const PEAK_DECAY_US: f64 = 2_000.0;
const RISING_PEAK_FRACTION: f64 = 0.35;
const FALLING_PEAK_FRACTION: f64 = 0.30;

/// Threshold state on one uniformly sampled channel and its current burst.
pub(super) struct Detector {
    tick_us: u64,
    time_us: u64,
    mean_power: f64,
    noise_power: f64,
    peak_power: Option<f64>,
    high_since: Option<u64>,
    candidate_since: Option<u64>,
    pending: Option<(u64, u64)>,
    discarded: bool,
    oversize_bursts: usize,
    pulses: Vec<Pulse>,
    starts_us: Vec<u64>,
}

impl Detector {
    pub(super) fn new(sample_rate_hz: u32) -> Self {
        Self {
            tick_us: 1_000_000 / u64::from(sample_rate_hz),
            time_us: 0,
            mean_power: 0.0,
            noise_power: 1.0,
            peak_power: None,
            high_since: None,
            candidate_since: None,
            pending: None,
            discarded: false,
            oversize_bursts: 0,
            pulses: Vec::new(),
            starts_us: Vec::new(),
        }
    }

    /// Track a threshold floor against recent signal peaks.
    pub(super) fn with_peak_tracking(sample_rate_hz: u32) -> Self {
        Self {
            peak_power: Some(0.0),
            ..Self::new(sample_rate_hz)
        }
    }

    /// Advance one power sample. Confirmed edge times use the start of the run.
    pub(super) fn push(&mut self, power: f64, emit: &mut impl FnMut(&[Pulse], &[u64])) {
        let now = self.time_us;
        self.time_us += self.tick_us;
        self.mean_power += (power - self.mean_power) * self.tick_us as f64 / 12.0;
        if now < SETTLE_US {
            self.noise_power = self.mean_power.max(1.0);
            return;
        }

        let high = self.high_since.is_some();
        let mut threshold = self.noise_power * if high { 2.0 } else { 4.0 };
        if let Some(peak) = &mut self.peak_power {
            *peak = self
                .mean_power
                .max(*peak * (1.0 - self.tick_us as f64 / PEAK_DECAY_US));
            // The noise estimate freezes while high. A separate peak floor
            // recovers valleys when their noise rises during a long burst.
            let fraction = if high {
                FALLING_PEAK_FRACTION
            } else {
                RISING_PEAK_FRACTION
            };
            threshold = threshold.max(*peak * fraction);
        }
        let above = self.mean_power > threshold;
        if high == above {
            self.candidate_since = None;
            if !high {
                self.noise_power +=
                    (self.mean_power.max(1.0) - self.noise_power) * self.tick_us as f64 / 400.0;
            }
        } else {
            let edge = *self.candidate_since.get_or_insert(now);
            if now + self.tick_us - edge >= STABLE_EDGE_US {
                self.candidate_since = None;
                if high {
                    self.pending = self.high_since.take().map(|start| (start, edge));
                } else {
                    self.close_pulse(edge);
                    self.high_since = Some(edge);
                }
            }
        }

        if self
            .high_since
            .is_some_and(|start| now - start >= MAX_HIGH_US)
        {
            // A stationary carrier cannot be one of these symbols. Preserve
            // the preceding completed highs and rebase the noise estimate.
            self.noise_power = self.mean_power.max(1.0);
            self.high_since = None;
            self.candidate_since = None;
            self.finish_burst(now, emit);
        }
        if self
            .pending
            .is_some_and(|(_, end)| self.candidate_since.unwrap_or(now) - end >= BURST_GAP_US)
        {
            self.finish_burst(now, emit);
        }
    }

    /// Flush measured completed highs at EOF. A high still in progress has
    /// no measured width and is excluded.
    pub(super) fn finish(&mut self, emit: &mut impl FnMut(&[Pulse], &[u64])) {
        self.finish_burst(self.time_us, emit);
    }

    /// Count bursts skipped because their pulse train exceeded the retention bound.
    pub(super) fn oversize_bursts(&self) -> usize {
        self.oversize_bursts
    }

    fn close_pulse(&mut self, next: u64) {
        if let Some((start, end)) = self.pending.take() {
            if self.pulses.len() == MAX_PULSES {
                self.discarded = true;
            }
            if !self.discarded {
                self.starts_us.push(start);
                self.pulses.push(Pulse {
                    high_us: (end - start) as u32,
                    low_us: (next - end) as u32,
                });
            }
        }
    }

    fn finish_burst(&mut self, next: u64, emit: &mut impl FnMut(&[Pulse], &[u64])) {
        self.close_pulse(next);
        if self.discarded {
            self.oversize_bursts += 1;
        } else if !self.pulses.is_empty() {
            emit(&self.pulses, &self.starts_us);
        }
        self.pulses.clear();
        self.starts_us.clear();
        self.discarded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oregon_peak_tracking_recovers_valleys_above_the_frozen_noise_threshold() {
        let mut detector = Detector::with_peak_tracking(500_000);
        let mut widths = Vec::new();
        let mut emit = |pulses: &[Pulse], _: &[u64]| {
            widths.extend(pulses.iter().map(|pulse| pulse.high_us));
        };
        for _ in 0..500 {
            detector.push(100.0, &mut emit);
        }
        for _ in 0..6 {
            for (power, ticks) in [(2_500.0, 244), (300.0, 244)] {
                for _ in 0..ticks {
                    detector.push(power, &mut emit);
                }
            }
        }
        detector.finish(&mut emit);
        assert_eq!(widths.len(), 6);
        assert!(widths.into_iter().all(|width| (450..=550).contains(&width)));
    }

    #[test]
    fn ignores_a_short_power_spike_before_a_pulse() {
        let mut detector = Detector::new(500_000);
        let mut emitted = Vec::new();
        let mut emit = |pulses: &[Pulse], _: &[u64]| emitted.push(pulses.len());
        for (power, ticks) in [
            (64.0, 500),
            (450.0, 10),
            (64.0, 100),
            (4_096.0, 100),
            (64.0, 1_100),
        ] {
            for _ in 0..ticks {
                detector.push(power, &mut emit);
            }
        }
        detector.finish(&mut emit);
        assert_eq!(emitted, [1]);
    }

    #[test]
    fn counts_an_oversize_burst_and_recovers_after_silence() {
        let mut detector = Detector::new(500_000);
        let mut emitted = Vec::new();
        let mut emit = |pulses: &[Pulse], _: &[u64]| emitted.push(pulses.len());
        let mut level = |detector: &mut Detector, power, ticks| {
            for _ in 0..ticks {
                detector.push(power, &mut emit);
            }
        };
        level(&mut detector, 64.0, 100);
        for _ in 0..=MAX_PULSES {
            level(&mut detector, 4_096.0, 100);
            level(&mut detector, 64.0, 100);
        }
        level(&mut detector, 64.0, 1_100);
        level(&mut detector, 4_096.0, 100);
        level(&mut detector, 64.0, 100);
        detector.finish(&mut emit);
        assert_eq!(emitted, [1]);
        assert_eq!(detector.oversize_bursts(), 1);
    }
}
