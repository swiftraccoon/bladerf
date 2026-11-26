//! Measure bounded narrowband evidence in isolated follow-up intervals.

use std::cmp::Reverse;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::waveform::{self, CLIP_CONTEXT_SAMPLES, MAX_CLIP_SAMPLES, MIN_INPUT_INTERVAL};

use super::super::admission::{PassRef, Tune};
use super::{Detection, Observation, PassReport, TargetReport, check_stop};

const BYTES_PER_SAMPLE: usize = 4;

#[derive(Default, Serialize)]
pub(super) struct Analysis {
    max_probes: usize,
    eligible_runs: usize,
    omitted_eligible_runs: usize,
    too_short_runs: usize,
    missing_filter_context_runs: usize,
    probes: Vec<Probe>,
}

#[derive(Serialize)]
struct Probe {
    source_row: usize,
    pass_index: usize,
    requested_center_hz: u64,
    run_start_sample: usize,
    run_end_sample_exclusive: usize,
    measured_start_sample: usize,
    measured_end_sample_exclusive: usize,
    clip_start_sample: usize,
    clip_end_sample_exclusive: usize,
    clip_sha256: String,
    evidence: waveform::NarrowEvidence,
}

struct Candidate<'a> {
    target: &'a TargetReport,
    observation: &'a Observation,
    tune: &'a Tune,
    run: &'a Detection,
}

impl Candidate<'_> {
    fn measure(&self, stop: &AtomicBool) -> Result<Probe> {
        check_stop(stop)?;
        let run = self.run;
        let measured = measured_extent(run);
        let clip = measured.start - CLIP_CONTEXT_SAMPLES..measured.end + CLIP_CONTEXT_SAMPLES;
        let mut bytes = vec![0; clip.len() * BYTES_PER_SAMPLE];
        let iq = self.tune.open_iq()?;
        iq.read_exact_at(&mut bytes, (clip.start * BYTES_PER_SAMPLE) as u64)
            .context("read isolated waveform and its filter context")?;
        self.tune.verify_iq(&iq)?;
        let interval = CLIP_CONTEXT_SAMPLES..CLIP_CONTEXT_SAMPLES + measured.len();
        let (clip_sha256, evidence) = waveform::measure_narrow_bytes(
            bytes,
            self.tune.observed_center_hz,
            self.target.target_hz,
            interval,
            stop,
        )?;
        Ok(Probe {
            source_row: self.target.source_row,
            pass_index: self.observation.pass_index,
            requested_center_hz: self.tune.requested_center_hz,
            run_start_sample: run.start_sample,
            run_end_sample_exclusive: run.end_sample_exclusive,
            measured_start_sample: measured.start,
            measured_end_sample_exclusive: measured.end,
            clip_start_sample: clip.start,
            clip_end_sample_exclusive: clip.end,
            clip_sha256,
            evidence,
        })
    }
}

fn run_len(run: &Detection) -> usize {
    run.end_sample_exclusive - run.start_sample
}

/// Rank eligible runs within each observation, then distribute probes across targets.
///
/// # Errors
/// Returns an error on interruption, source changes, or clip read or validation failure.
pub(super) fn measure(
    passes: &[PassRef<'_>],
    pass_reports: &[PassReport<'_>],
    targets: &[TargetReport],
    limit: usize,
    stop: &AtomicBool,
) -> Result<Analysis> {
    let mut analysis = Analysis {
        max_probes: limit,
        ..Analysis::default()
    };
    let mut candidates = Vec::new();
    for target in targets {
        let mut observation_rank = 0;
        for observation in &target.observations {
            check_stop(stop)?;
            let Some(runs) = &observation.isolated_runs else {
                continue;
            };
            let index = observation.pass_index - 1;
            let (tune, tune_report) = passes[index]
                .sweep
                .tunes
                .iter()
                .zip(&pass_reports[index].tunes)
                .find(|(tune, _)| tune.requested_center_hz == observation.requested_center_hz)
                .context("isolated waveform has no matching dwell")?;
            let mut eligible = Vec::new();
            for run in runs {
                if run_len(run) < MIN_INPUT_INTERVAL {
                    analysis.too_short_runs += 1;
                } else if !has_context(run, tune_report.analyzed_samples) {
                    analysis.missing_filter_context_runs += 1;
                } else {
                    eligible.push(run);
                }
            }
            if eligible.is_empty() {
                continue;
            }
            eligible.sort_unstable_by(|a, b| {
                Reverse(run_len(a))
                    .cmp(&Reverse(run_len(b)))
                    .then_with(|| b.peak_excess_db.total_cmp(&a.peak_excess_db))
                    .then_with(|| a.start_sample.cmp(&b.start_sample))
            });
            analysis.eligible_runs += eligible.len();
            candidates.extend(
                eligible
                    .into_iter()
                    .take(limit)
                    .enumerate()
                    .map(|(rank, run)| {
                        (
                            (rank, observation_rank),
                            Candidate {
                                target,
                                observation,
                                tune,
                                run,
                            },
                        )
                    }),
            );
            observation_rank += 1;
        }
    }
    candidates.sort_by_key(|(rank, _)| *rank);
    analysis.omitted_eligible_runs = analysis.eligible_runs.saturating_sub(limit);
    for (_, candidate) in candidates.into_iter().take(limit) {
        let probe = candidate.measure(stop).with_context(|| {
            format!(
                "probe row {} pass {}/{} at sample {}",
                candidate.target.source_row,
                candidate.observation.pass_index,
                candidate.tune.directory,
                candidate.run.start_sample
            )
        })?;
        analysis.probes.push(probe);
    }
    Ok(analysis)
}

fn has_context(run: &Detection, analyzed_samples: usize) -> bool {
    let extent = measured_extent(run);
    extent.start >= CLIP_CONTEXT_SAMPLES && extent.end + CLIP_CONTEXT_SAMPLES <= analyzed_samples
}

fn measured_extent(run: &Detection) -> Range<usize> {
    let interval_len = run_len(run);
    let measured_len = interval_len.min(MAX_CLIP_SAMPLES - 2 * CLIP_CONTEXT_SAMPLES);
    let start = run.start_sample + (interval_len - measured_len) / 2;
    start..start + measured_len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_context_is_checked_against_the_measured_span() {
        let mut run = Detection {
            start_sample: CLIP_CONTEXT_SAMPLES,
            end_sample_exclusive: CLIP_CONTEXT_SAMPLES + MIN_INPUT_INTERVAL,
            peak_excess_db: 10.0,
        };
        let prefix = run.end_sample_exclusive + CLIP_CONTEXT_SAMPLES;
        assert!(has_context(&run, prefix));
        assert!(!has_context(&run, prefix - 1));
        run.start_sample -= 1;
        assert!(!has_context(&run, prefix));

        run.start_sample = 0;
        run.end_sample_exclusive = MAX_CLIP_SAMPLES;
        assert!(has_context(&run, MAX_CLIP_SAMPLES));
        assert_eq!(
            measured_extent(&run),
            CLIP_CONTEXT_SAMPLES..MAX_CLIP_SAMPLES - CLIP_CONTEXT_SAMPLES
        );
    }
}
