//! Score short band bursts using phase estimated from earlier segments in one epoch.

use std::f64::consts::TAU;
use std::ops::Range;

use serde::Serialize;

use super::burst::{BurstDetection, WINDOW_SAMPLES};

pub(super) const PERIOD_SAMPLES: u16 = 20_000;
pub(super) const TRAINING_SEGMENTS: usize = 32;
pub(super) const GATE_HALF_WIDTH_SAMPLES: u64 = WINDOW_SAMPLES;
pub(super) const MINIMUM_TRAINING_SHORTS: u64 = 16;
pub(super) const MINIMUM_CONCENTRATION: f64 = 0.70;

pub(super) struct Observation {
    origin_phase: u16,
    grid_windows: u64,
    strong_short_phases: Vec<u16>,
}

impl Observation {
    /// Collect verified burst starts outside the segment's sorted Wi-Fi span union.
    pub(super) fn new(
        stream_start: u64,
        source_samples: u64,
        detections: &[BurstDetection],
        wifi_spans: &[Range<u64>],
    ) -> Self {
        let strong_short_phases = detections
            .iter()
            .filter(|detection| detection.is_strong_short())
            .filter_map(|detection| {
                // The band report has already verified the complete stream extent.
                let start = stream_start + detection.start_sample;
                let end = stream_start + detection.end_sample_exclusive;
                let next_wifi = wifi_spans.partition_point(|span| span.end <= start);
                wifi_spans
                    .get(next_wifi)
                    .is_none_or(|span| end <= span.start)
                    .then(|| phase_residue(start))
            })
            .collect();
        Self {
            origin_phase: phase_residue(stream_start),
            grid_windows: source_samples / WINDOW_SAMPLES,
            strong_short_phases,
        }
    }
}

fn phase_residue(sample: u64) -> u16 {
    (sample % u64::from(PERIOD_SAMPLES)) as u16
}

#[derive(Clone, Copy, Default)]
struct PhaseSum {
    count: u64,
    cosine: f64,
    sine: f64,
}

impl PhaseSum {
    fn from_phases(phases: &[u16]) -> Self {
        let mut sum = Self {
            count: phases.len() as u64,
            ..Self::default()
        };
        for &phase in phases {
            let (sine, cosine) = (TAU * f64::from(phase) / f64::from(PERIOD_SAMPLES)).sin_cos();
            sum.sine += sine;
            sum.cosine += cosine;
        }
        sum
    }

    fn concentration(self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        (self.cosine.hypot(self.sine) / self.count as f64).min(1.0)
    }

    fn center(self) -> f64 {
        let period = f64::from(PERIOD_SAMPLES);
        (self.sine.atan2(self.cosine) / TAU * period).rem_euclid(period)
    }
}

#[derive(Default)]
pub(super) struct Tracker {
    history: [PhaseSum; TRAINING_SEGMENTS],
    next_index: usize,
}

impl Tracker {
    pub(super) fn next_index(&self) -> usize {
        self.next_index
    }

    /// Score the next contiguous segment before adding its phases to training.
    pub(super) fn score(&mut self, observation: Observation) -> Score {
        let outcome = if self.next_index < TRAINING_SEGMENTS {
            Outcome::Warmup
        } else {
            let mut training = PhaseSum::default();
            for phase_sum in self.history {
                training.count += phase_sum.count;
                training.cosine += phase_sum.cosine;
                training.sine += phase_sum.sine;
            }
            let concentration = training.concentration();
            if training.count < MINIMUM_TRAINING_SHORTS || concentration < MINIMUM_CONCENTRATION {
                Outcome::Abstained {
                    training_shorts: training.count,
                    concentration,
                }
            } else {
                let center = training.center();
                let period = f64::from(PERIOD_SAMPLES);
                let within_gate = |phase: u16| {
                    let distance = (f64::from(phase) - center).abs();
                    distance.min(period - distance) <= GATE_HALF_WIDTH_SAMPLES as f64
                };
                let in_gate_strong_shorts = observation
                    .strong_short_phases
                    .iter()
                    .filter(|&&phase| within_gate(phase))
                    .count() as u64;
                let in_gate_windows = (0..observation.grid_windows)
                    .filter(|&window| {
                        within_gate(phase_residue(
                            u64::from(observation.origin_phase) + window * WINDOW_SAMPLES,
                        ))
                    })
                    .count() as u64;
                Outcome::Scored {
                    training_shorts: training.count,
                    concentration,
                    gate_center_samples: center,
                    in_gate_strong_shorts,
                    grid_windows: observation.grid_windows,
                    in_gate_windows,
                }
            }
        };
        let phase_sum = PhaseSum::from_phases(&observation.strong_short_phases);
        let training_segments = self.next_index.min(TRAINING_SEGMENTS);
        self.history[self.next_index % TRAINING_SEGMENTS] = phase_sum;
        self.next_index += 1;
        Score {
            eligible_strong_shorts: phase_sum.count,
            training_segments,
            outcome,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Score {
    eligible_strong_shorts: u64,
    training_segments: usize,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    Warmup,
    Abstained {
        training_shorts: u64,
        concentration: f64,
    },
    Scored {
        training_shorts: u64,
        concentration: f64,
        gate_center_samples: f64,
        in_gate_strong_shorts: u64,
        grid_windows: u64,
        in_gate_windows: u64,
    },
}

impl Score {
    pub(super) fn counts(&self) -> Counts {
        let eligible = self.eligible_strong_shorts;
        match self.outcome {
            Outcome::Warmup => Counts {
                warmup_segments: 1,
                warmup_strong_shorts: eligible,
                ..Counts::default()
            },
            Outcome::Abstained { .. } => Counts {
                abstained_segments: 1,
                abstained_strong_shorts: eligible,
                ..Counts::default()
            },
            Outcome::Scored {
                in_gate_strong_shorts,
                grid_windows,
                in_gate_windows,
                ..
            } => Counts {
                scored_segments: 1,
                scored_strong_shorts: eligible,
                in_gate_strong_shorts,
                scored_grid_windows: grid_windows,
                in_gate_windows,
                ..Counts::default()
            },
        }
    }
}

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct Counts {
    warmup_segments: u64,
    warmup_strong_shorts: u64,
    abstained_segments: u64,
    abstained_strong_shorts: u64,
    scored_segments: u64,
    scored_strong_shorts: u64,
    in_gate_strong_shorts: u64,
    scored_grid_windows: u64,
    in_gate_windows: u64,
}

impl Counts {
    pub(super) fn total(counts: impl Iterator<Item = Self>) -> Option<Self> {
        counts.reduce(|first, second| Self {
            warmup_segments: first.warmup_segments + second.warmup_segments,
            warmup_strong_shorts: first.warmup_strong_shorts + second.warmup_strong_shorts,
            abstained_segments: first.abstained_segments + second.abstained_segments,
            abstained_strong_shorts: first.abstained_strong_shorts + second.abstained_strong_shorts,
            scored_segments: first.scored_segments + second.scored_segments,
            scored_strong_shorts: first.scored_strong_shorts + second.scored_strong_shorts,
            in_gate_strong_shorts: first.in_gate_strong_shorts + second.in_gate_strong_shorts,
            scored_grid_windows: first.scored_grid_windows + second.scored_grid_windows,
            in_gate_windows: first.in_gate_windows + second.in_gate_windows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(phases: &[u16]) -> Observation {
        Observation {
            origin_phase: 0,
            grid_windows: 625,
            strong_short_phases: phases.to_vec(),
        }
    }

    #[test]
    fn collects_strong_shorts_with_half_open_wifi_exclusion() {
        let burst = |start, windows, peak_excess_db| BurstDetection {
            start_sample: start * WINDOW_SAMPLES,
            end_sample_exclusive: (start + windows) * WINDOW_SAMPLES,
            peak_excess_db,
        };
        let detections = [
            burst(0, 3, 15.0),
            burst(4, 2, 10.0),
            burst(8, 2, 9.99),
            burst(12, 4, 20.0),
        ];
        let stream_start = 3 * u64::from(PERIOD_SAMPLES) + 16;
        let observed = Observation::new(
            stream_start,
            16 * WINDOW_SAMPLES + 1,
            &detections,
            &[
                (stream_start + 100)..(stream_start + 200),
                (stream_start + 6 * WINDOW_SAMPLES)..(stream_start + 7 * WINDOW_SAMPLES),
            ],
        );
        assert_eq!(observed.origin_phase, 16);
        assert_eq!(observed.grid_windows, 16);
        assert_eq!(observed.strong_short_phases, [4112]);
    }

    #[test]
    fn scores_against_prior_segments_and_counts_wrapping_grid_exposure() {
        let mut tracker = Tracker::default();
        for index in 0..TRAINING_SEGMENTS {
            assert_eq!(tracker.next_index(), index);
            let score = tracker.score(observation(&[0]));
            assert!(matches!(score.outcome, Outcome::Warmup));
        }
        let score = tracker.score(observation(&[0, 1024, 18_976, 1025]));
        assert_eq!(score.eligible_strong_shorts, 4);
        let Outcome::Scored {
            training_shorts,
            gate_center_samples,
            ..
        } = score.outcome
        else {
            panic!("expected prior observations to qualify");
        };
        assert_eq!(training_shorts, 32);
        assert_eq!(gate_center_samples, 0.0);
        let counts = score.counts();
        assert_eq!(counts.in_gate_strong_shorts, 3);
        assert_eq!(counts.scored_grid_windows, 625);
        assert_eq!(counts.in_gate_windows, 65);
    }

    #[test]
    fn abstains_until_history_qualifies_and_starts_each_epoch_fresh() {
        let mut tracker = Tracker::default();
        for _ in 0..TRAINING_SEGMENTS {
            tracker.score(observation(&[0, 10_000]));
        }
        let abstention = tracker.score(observation(&[0]));
        assert!(matches!(
            abstention.outcome,
            Outcome::Abstained {
                training_shorts: 64,
                ..
            }
        ));
        for _ in 1..TRAINING_SEGMENTS {
            tracker.score(observation(&[0]));
        }
        let ready = tracker.score(observation(&[0]));
        assert!(matches!(ready.outcome, Outcome::Scored { .. }));
        let mut new_epoch = Tracker::default();
        assert_eq!(new_epoch.next_index(), 0);
        let warmup = new_epoch.score(observation(&[0]));
        let counts =
            Counts::total([abstention.counts(), ready.counts(), warmup.counts()].into_iter())
                .unwrap();
        assert_eq!(counts.abstained_strong_shorts, 1);
        assert_eq!(counts.warmup_strong_shorts, 1);
        assert_eq!(counts.scored_strong_shorts, 1);
        assert_eq!(counts.in_gate_strong_shorts, 1);
    }

    #[test]
    fn too_few_prior_shorts_abstain_even_at_perfect_concentration() {
        let mut tracker = Tracker::default();
        for index in 0..TRAINING_SEGMENTS {
            let phases: &[u16] = if index < 15 { &[0] } else { &[] };
            tracker.score(observation(phases));
        }
        let abstention = tracker.score(observation(&[0, 0]));
        assert!(matches!(
            abstention.outcome,
            Outcome::Abstained {
                training_shorts: 15,
                concentration: 1.0,
            }
        ));
        assert_eq!(abstention.counts().scored_grid_windows, 0);
        assert_eq!(abstention.counts().abstained_strong_shorts, 2);
    }
}
