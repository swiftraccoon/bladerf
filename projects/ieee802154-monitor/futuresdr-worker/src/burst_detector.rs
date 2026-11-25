//! Fixed, causal burst detector used to qualify burst-aware acquisition.

use anyhow::{Result, ensure};
use futuresdr::num_complex::Complex32;

pub(crate) const POWER_WINDOW_SAMPLES: usize = 32;
pub(crate) const NOISE_WARMUP_SAMPLES: u64 = 4_096;
pub(crate) const NOISE_EMA_ALPHA: f64 = 1.0 / 4_096.0;
pub(crate) const TRIGGER_POWER_RATIO: f64 = 3.0;
pub(crate) const TRIGGER_PERSISTENCE_SAMPLES: usize = 4;
pub(crate) const RELEASE_POWER_RATIO: f64 = 2.0;
pub(crate) const RELEASE_PERSISTENCE_SAMPLES: usize = 64;
pub(crate) const PRETRIGGER_SAMPLES: u64 = 64;
pub(crate) const MAX_BURST_SAMPLES: u64 = 20_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DetectedBurst {
    pub(crate) replay_start: u64,
    pub(crate) detected_at: u64,
    pub(crate) end_exclusive: u64,
    pub(crate) forced_end: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BurstStart {
    pub(crate) replay_start: u64,
    pub(crate) detected_at: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DetectorUpdate {
    pub(crate) started: Option<BurstStart>,
    pub(crate) ended: Option<DetectedBurst>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Idle,
    Active { replay_start: u64, detected_at: u64 },
    Inhibited,
    Finished,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetectorDiagnosticMode {
    Idle,
    Active { replay_start: u64, detected_at: u64 },
    Inhibited,
    Finished,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DetectorDiagnosticSnapshot {
    pub(crate) samples_seen: u64,
    pub(crate) mode: DetectorDiagnosticMode,
    pub(crate) release_run: usize,
    pub(crate) window_len: usize,
    pub(crate) window_nonzero_count: usize,
    pub(crate) stored_window_sum_bits: u64,
    pub(crate) mean_power_bits: u64,
    pub(crate) recomputed_window_sum_bits: u64,
    pub(crate) noise_floor_bits: u64,
    pub(crate) release_threshold_bits: u64,
}

#[derive(Debug)]
pub(crate) struct CausalBurstDetector {
    power_window: [f64; POWER_WINDOW_SAMPLES],
    power_window_index: usize,
    power_window_len: usize,
    power_window_sum: f64,
    warmup_power_sum: f64,
    noise_floor: f64,
    samples_seen: u64,
    trigger_run: usize,
    release_run: usize,
    mode: Mode,
}

impl CausalBurstDetector {
    pub(crate) fn new() -> Self {
        Self {
            power_window: [0.0; POWER_WINDOW_SAMPLES],
            power_window_index: 0,
            power_window_len: 0,
            power_window_sum: 0.0,
            warmup_power_sum: 0.0,
            noise_floor: 0.0,
            samples_seen: 0,
            trigger_run: 0,
            release_run: 0,
            mode: Mode::Idle,
        }
    }

    pub(crate) fn push(&mut self, sample: Complex32) -> Result<Option<DetectedBurst>> {
        Ok(self.push_with_transition(sample)?.ended)
    }

    pub(crate) fn push_with_transition(&mut self, sample: Complex32) -> Result<DetectorUpdate> {
        ensure!(self.mode != Mode::Finished, "burst detector is finished");
        ensure!(
            sample.re.is_finite() && sample.im.is_finite(),
            "burst detector input must be finite"
        );
        let re = f64::from(sample.re);
        let im = f64::from(sample.im);
        let power = re * re + im * im;
        ensure!(power.is_finite(), "burst detector power must be finite");
        let sample_index = self.samples_seen;
        let next_index = sample_index
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("burst detector sample counter overflow"))?;
        self.samples_seen = next_index;
        self.push_power(power);

        if next_index <= NOISE_WARMUP_SAMPLES {
            self.warmup_power_sum += power;
            if next_index == NOISE_WARMUP_SAMPLES {
                self.noise_floor = self.warmup_power_sum / NOISE_WARMUP_SAMPLES as f64;
            }
            return Ok(DetectorUpdate::default());
        }

        let mean_power = self.power_window_sum / self.power_window_len as f64;
        let release_threshold = RELEASE_POWER_RATIO * self.noise_floor;
        match self.mode {
            Mode::Idle => {
                let trigger_threshold = TRIGGER_POWER_RATIO * self.noise_floor;
                if mean_power > trigger_threshold {
                    self.trigger_run += 1;
                } else {
                    self.trigger_run = 0;
                    if mean_power <= release_threshold {
                        self.noise_floor += NOISE_EMA_ALPHA * (mean_power - self.noise_floor);
                    }
                }
                if self.trigger_run == TRIGGER_PERSISTENCE_SAMPLES {
                    let replay_start = next_index.saturating_sub(PRETRIGGER_SAMPLES);
                    self.mode = Mode::Active {
                        replay_start,
                        detected_at: sample_index,
                    };
                    self.trigger_run = 0;
                    self.release_run = 0;
                    return Ok(DetectorUpdate {
                        started: Some(BurstStart {
                            replay_start,
                            detected_at: sample_index,
                        }),
                        ended: None,
                    });
                }
                Ok(DetectorUpdate::default())
            }
            Mode::Active {
                replay_start,
                detected_at,
            } => {
                if next_index - replay_start >= MAX_BURST_SAMPLES {
                    self.mode = Mode::Inhibited;
                    self.release_run = usize::from(mean_power <= release_threshold);
                    return Ok(DetectorUpdate {
                        started: None,
                        ended: Some(DetectedBurst {
                            replay_start,
                            detected_at,
                            end_exclusive: next_index,
                            forced_end: true,
                        }),
                    });
                }
                if mean_power <= release_threshold {
                    self.release_run += 1;
                } else {
                    self.release_run = 0;
                }
                if self.release_run == RELEASE_PERSISTENCE_SAMPLES {
                    self.mode = Mode::Idle;
                    self.release_run = 0;
                    return Ok(DetectorUpdate {
                        started: None,
                        ended: Some(DetectedBurst {
                            replay_start,
                            detected_at,
                            end_exclusive: next_index,
                            forced_end: false,
                        }),
                    });
                }
                Ok(DetectorUpdate::default())
            }
            Mode::Inhibited => {
                if mean_power <= release_threshold {
                    self.release_run += 1;
                } else {
                    self.release_run = 0;
                }
                if self.release_run == RELEASE_PERSISTENCE_SAMPLES {
                    self.mode = Mode::Idle;
                    self.release_run = 0;
                }
                Ok(DetectorUpdate::default())
            }
            Mode::Finished => unreachable!("finished mode rejected above"),
        }
    }

    pub(crate) fn finish(&mut self) -> Option<DetectedBurst> {
        let event = match self.mode {
            Mode::Active {
                replay_start,
                detected_at,
            } => Some(DetectedBurst {
                replay_start,
                detected_at,
                end_exclusive: self.samples_seen,
                forced_end: true,
            }),
            Mode::Idle | Mode::Inhibited | Mode::Finished => None,
        };
        self.mode = Mode::Finished;
        event
    }

    #[cfg(test)]
    pub(crate) fn diagnostic_snapshot(&self) -> DetectorDiagnosticSnapshot {
        let window = &self.power_window[..self.power_window_len];
        // Recompute independently so tests can distinguish live window energy
        // from residue in the incrementally maintained accumulator.
        let recomputed_window_sum = window.iter().sum::<f64>();
        let mean_power = if self.power_window_len == 0 {
            0.0
        } else {
            self.power_window_sum / self.power_window_len as f64
        };
        let mode = match self.mode {
            Mode::Idle => DetectorDiagnosticMode::Idle,
            Mode::Active {
                replay_start,
                detected_at,
            } => DetectorDiagnosticMode::Active {
                replay_start,
                detected_at,
            },
            Mode::Inhibited => DetectorDiagnosticMode::Inhibited,
            Mode::Finished => DetectorDiagnosticMode::Finished,
        };
        DetectorDiagnosticSnapshot {
            samples_seen: self.samples_seen,
            mode,
            release_run: self.release_run,
            window_len: self.power_window_len,
            window_nonzero_count: window.iter().filter(|power| **power != 0.0).count(),
            stored_window_sum_bits: self.power_window_sum.to_bits(),
            mean_power_bits: mean_power.to_bits(),
            recomputed_window_sum_bits: recomputed_window_sum.to_bits(),
            noise_floor_bits: self.noise_floor.to_bits(),
            release_threshold_bits: (RELEASE_POWER_RATIO * self.noise_floor).to_bits(),
        }
    }

    pub(crate) const fn samples_seen(&self) -> u64 {
        self.samples_seen
    }

    fn push_power(&mut self, power: f64) {
        if self.power_window_len == POWER_WINDOW_SAMPLES {
            self.power_window_sum -= self.power_window[self.power_window_index];
        } else {
            self.power_window_len += 1;
        }
        self.power_window[self.power_window_index] = power;
        self.power_window_sum += power;
        self.power_window_index = (self.power_window_index + 1) % POWER_WINDOW_SAMPLES;
        let full_window_is_zero = self.power_window_len == POWER_WINDOW_SAMPLES
            && self.power_window.iter().all(|power| *power == 0.0);
        // Rolling arithmetic can retain positive residue after all energy has
        // left; canonicalize only an exact zero window, never a live one.
        if self.power_window_sum < 0.0 || full_window_is_zero {
            self.power_window_sum = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(amplitude: f32) -> Complex32 {
        Complex32::new(amplitude, 0.0)
    }

    fn synthetic_input(scale: f32, active_samples: usize, trailing_idle: usize) -> Vec<Complex32> {
        let mut input = vec![sample(scale); NOISE_WARMUP_SAMPLES as usize];
        input.extend(vec![sample(3.0 * scale); active_samples]);
        input.extend(vec![sample(scale); trailing_idle]);
        input
    }

    fn detect(input: &[Complex32]) -> Result<Vec<DetectedBurst>> {
        let mut detector = CausalBurstDetector::new();
        let mut events = Vec::new();
        for value in input {
            if let Some(event) = detector.push(*value)? {
                events.push(event);
            }
        }
        if let Some(event) = detector.finish() {
            events.push(event);
        }
        Ok(events)
    }

    #[test]
    fn candidate_constants_are_frozen() {
        assert_eq!(POWER_WINDOW_SAMPLES, 32);
        assert_eq!(NOISE_WARMUP_SAMPLES, 4_096);
        assert!((NOISE_EMA_ALPHA - 1.0 / 4_096.0).abs() < f64::EPSILON);
        assert_eq!(TRIGGER_POWER_RATIO, 3.0);
        assert_eq!(TRIGGER_PERSISTENCE_SAMPLES, 4);
        assert_eq!(RELEASE_POWER_RATIO, 2.0);
        assert_eq!(RELEASE_PERSISTENCE_SAMPLES, 64);
        assert_eq!(PRETRIGGER_SAMPLES, 64);
        assert_eq!(MAX_BURST_SAMPLES, 20_000);
    }

    #[test]
    fn exact_zero_full_window_canonicalizes_rolling_sum() {
        let mut detector = CausalBurstDetector::new();
        // At 2^54, the 31 accumulated unit powers round into positive residue.
        for _ in 0..POWER_WINDOW_SAMPLES - 1 {
            detector.push_power(1.0);
        }
        let large_power = (1u64 << 54) as f64;
        detector.push_power(large_power);
        for _ in 0..POWER_WINDOW_SAMPLES - 1 {
            detector.push_power(0.0);
        }

        let nonzero = detector.diagnostic_snapshot();
        assert_eq!(nonzero.window_len, POWER_WINDOW_SAMPLES);
        assert_eq!(nonzero.window_nonzero_count, 1);
        assert_eq!(
            f64::from_bits(nonzero.recomputed_window_sum_bits),
            large_power
        );
        assert!(f64::from_bits(nonzero.stored_window_sum_bits) > large_power);

        detector.push_power(0.0);
        let zero = detector.diagnostic_snapshot();
        assert_eq!(zero.window_nonzero_count, 0);
        assert_eq!(zero.recomputed_window_sum_bits, 0.0f64.to_bits());
        assert_eq!(zero.stored_window_sum_bits, 0.0f64.to_bits());
        assert_eq!(zero.mean_power_bits, 0.0f64.to_bits());
    }

    #[test]
    fn silence_never_triggers_and_nonfinite_input_is_rejected() {
        assert!(
            detect(&vec![Complex32::new(0.0, 0.0); 20_000])
                .unwrap()
                .is_empty()
        );
        let mut detector = CausalBurstDetector::new();
        assert!(detector.push(Complex32::new(f32::NAN, 0.0)).is_err());
        assert!(detector.push(Complex32::new(0.0, f32::INFINITY)).is_err());
        assert_eq!(detector.samples_seen, 0);
        let input = synthetic_input(1.0, 1_000, 256);
        let mut observed = Vec::new();
        for value in &input {
            if let Some(event) = detector.push(*value).unwrap() {
                observed.push(event);
            }
        }
        observed.extend(detector.finish());
        assert_eq!(observed, detect(&input).unwrap());

        let mut maximum = CausalBurstDetector::new();
        assert!(maximum.push(Complex32::new(f32::MAX, f32::MAX)).is_ok());
        assert_eq!(maximum.samples_seen, 1);
    }

    #[test]
    fn detects_once_with_pretrigger_and_hysteretic_release() {
        let active_start = NOISE_WARMUP_SAMPLES;
        let active_end = active_start + 1_000;
        let events = detect(&synthetic_input(1.0, 1_000, 256)).unwrap();
        assert_eq!(events.len(), 1);
        let event = events[0];
        assert!(event.replay_start <= active_start);
        assert!(event.detected_at >= active_start);
        assert!(event.end_exclusive > active_end);
        assert!(!event.forced_end);
        assert_eq!(
            event.detected_at + 1 - event.replay_start,
            PRETRIGGER_SAMPLES
        );
    }

    #[test]
    fn power_of_two_scaling_preserves_the_exact_event() {
        let expected = detect(&synthetic_input(1.0, 1_000, 256)).unwrap();
        for scale in [0.25, 4.0] {
            assert_eq!(
                detect(&synthetic_input(scale, 1_000, 256)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn transition_api_preserves_completed_events_and_exposes_start() {
        let input = synthetic_input(1.0, 1_000, 256);
        let expected = detect(&input).unwrap();
        let mut detector = CausalBurstDetector::new();
        let mut starts = Vec::new();
        let mut ended = Vec::new();
        for sample in input {
            let update = detector.push_with_transition(sample).unwrap();
            starts.extend(update.started);
            ended.extend(update.ended);
        }
        ended.extend(detector.finish());

        assert_eq!(ended, expected);
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].replay_start, expected[0].replay_start);
        assert_eq!(starts[0].detected_at, expected[0].detected_at);
        assert!(starts[0].replay_start < starts[0].detected_at);
    }

    #[test]
    fn maximum_length_forces_one_event_and_inhibits_retriggering() {
        let input = synthetic_input(1.0, 30_000, 256);
        let events = detect(&input).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].forced_end);
        assert_eq!(
            events[0].end_exclusive - events[0].replay_start,
            MAX_BURST_SAMPLES
        );
    }

    #[test]
    fn eof_forces_one_open_event_and_finish_is_idempotent() {
        let input = synthetic_input(1.0, 256, 0);
        let mut detector = CausalBurstDetector::new();
        let mut events = Vec::new();
        for value in input {
            if let Some(event) = detector.push(value).unwrap() {
                events.push(event);
            }
        }
        events.extend(detector.finish());
        assert_eq!(events.len(), 1);
        assert!(events[0].forced_end);
        assert_eq!(detector.finish(), None);
        assert!(detector.push(sample(1.0)).is_err());
    }
}
