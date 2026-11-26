//! Smoothed phase discrimination and silence detection for narrow FSK.

use std::collections::VecDeque;

use super::{Iq, phase_step};

const FULL_RATE_WINDOW: usize = 7;
const QUIET_US: usize = 96;

/// Delay of the 500 kSps discriminator's smoothing window.
pub(crate) const FULL_RATE_DELAY_SAMPLES: u64 = (FULL_RATE_WINDOW / 2) as u64;

/// Discriminate adjacent channel samples and smooth isolated phase steps.
pub(crate) struct Discriminator {
    previous: Option<Iq>,
    window: [f64; FULL_RATE_WINDOW],
    width: usize,
    index: usize,
    total: f64,
    quiet: usize,
    quiet_limit: usize,
}

impl Discriminator {
    /// Choose a short symmetric smoothing window at 250 or 500 kSps.
    pub(crate) fn new(sample_rate_hz: u32) -> Self {
        let width = if sample_rate_hz == 250_000 {
            3
        } else {
            FULL_RATE_WINDOW
        };
        Self {
            previous: None,
            window: [0.0; FULL_RATE_WINDOW],
            width,
            index: 0,
            total: 0.0,
            quiet: 0,
            quiet_limit: (sample_rate_hz as usize * QUIET_US / 1_000_000).max(1),
        }
    }

    /// Smoothing latency in channel samples.
    pub(crate) fn delay_samples(&self) -> u64 {
        (self.width / 2) as u64
    }

    /// Return the smoothed phase change in radians. Sustained sub-Q11
    /// silence returns `None` so a caller can discard partial framing state.
    pub(crate) fn push(&mut self, iq: Iq) -> Option<f64> {
        let step = if iq.power() < 1.0 {
            self.previous = None;
            self.quiet = (self.quiet + 1).min(self.quiet_limit);
            if self.quiet == self.quiet_limit {
                return None;
            }
            0.0
        } else {
            self.quiet = 0;
            self.previous
                .replace(iq)
                .map_or(0.0, |previous| phase_step(previous, iq))
        };
        self.total += step - self.window[self.index];
        self.window[self.index] = step;
        self.index = (self.index + 1) % self.width;
        Some(self.total / self.width as f64)
    }
}

/// Fractional lookback on a stream of discriminator samples.
#[derive(Clone, Copy)]
pub(crate) struct SampleDelay {
    whole: usize,
    fraction: f64,
}

impl SampleDelay {
    /// Split a nonnegative lookback on the sampled grid.
    pub(crate) fn new(samples: f64) -> Self {
        let whole = samples.floor() as usize;
        Self {
            whole,
            fraction: samples - whole as f64,
        }
    }

    /// Interpolate between recorded samples when both endpoints exist.
    pub(crate) fn read(self, history: &VecDeque<f64>) -> Option<f64> {
        let index = history.len().checked_sub(self.whole + 1)?;
        let later = history[index];
        if self.fraction == 0.0 {
            Some(later)
        } else {
            let earlier = history[index.checked_sub(1)?];
            Some(later + self.fraction * (earlier - later))
        }
    }
}
