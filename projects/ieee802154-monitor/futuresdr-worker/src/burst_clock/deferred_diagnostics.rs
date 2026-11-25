use anyhow::{anyhow, ensure};
use futuresdr::runtime::dev::prelude::{Complex32, Result};

use super::{
    DEFERRED_SELECTION_METRICS, DEFERRED_STRUCTURAL_CHIPS, DeferredStructureOutcome, DetectedBurst,
    IndexedSample, PRETRIGGER_SAMPLES, PhaseDiscriminatorState, PositionedMetric,
    SHR_PHASE_HYPOTHESES, SHR_RATE_HYPOTHESES_PPM, SHR_RATE_HYPOTHESIS_COUNT, SHR_START_HYPOTHESES,
    SHR_SYMBOLS, ScoredShrRateTimingSelection, ShrRateTimingSelection, ShrTimingSelection,
    deferred_structure, deferred_structure_after_score, interpolated_shr_rate_chip,
    select_deferred_shr_rate_timing, shr_rate_candidate_is_better,
};

#[cfg(test)]
mod tests;

const DEFERRED_LATE_START_BEGIN: usize = SHR_START_HYPOTHESES;
const DEFERRED_LATE_START_COUNT: usize = 16;
const DEFERRED_LATE_START_END: usize = DEFERRED_LATE_START_BEGIN + DEFERRED_LATE_START_COUNT;
const DEFERRED_LATE_SELECTION_METRICS: usize =
    DEFERRED_SELECTION_METRICS + DEFERRED_LATE_START_COUNT;
const DEFERRED_LATE_HYPOTHESIS_COUNT: u32 =
    (DEFERRED_LATE_START_COUNT * SHR_PHASE_HYPOTHESES * SHR_RATE_HYPOTHESES_PPM.len()) as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateCandidateSummary {
    pub(crate) selection: ShrRateTimingSelection,
    pub(crate) full_score_bits: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeferredShrRateFirstFailureHistogram {
    pub(crate) interpolation_nonfinite: u32,
    pub(crate) correlation_undefined: u32,
    pub(crate) score_at_or_below_cap: u32,
    pub(crate) shr_unclassified: [u32; SHR_SYMBOLS.len()],
    pub(crate) shr_wrong: [u32; SHR_SYMBOLS.len()],
    pub(crate) phr_low_unclassified: u32,
    pub(crate) phr_high_unclassified: u32,
    pub(crate) length_zero: u32,
    pub(crate) length_oversize: u32,
    pub(crate) eligible: u32,
}

impl DeferredShrRateFirstFailureHistogram {
    pub(crate) fn checked_total(&self) -> Option<u32> {
        self.shr_unclassified
            .iter()
            .chain(&self.shr_wrong)
            .copied()
            .chain([
                self.interpolation_nonfinite,
                self.correlation_undefined,
                self.score_at_or_below_cap,
                self.phr_low_unclassified,
                self.phr_high_unclassified,
                self.length_zero,
                self.length_oversize,
                self.eligible,
            ])
            .try_fold(0u32, u32::checked_add)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateSelectorAudit {
    pub(crate) evaluated_hypotheses: u32,
    pub(crate) first_failures: DeferredShrRateFirstFailureHistogram,
    pub(crate) best_defined: Option<DeferredShrRateCandidateSummary>,
    pub(crate) accepted_winner: Option<DeferredShrRateCandidateSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateLateStartAudit {
    pub(crate) evaluated_hypotheses: u32,
    pub(crate) first_failures: DeferredShrRateFirstFailureHistogram,
    pub(crate) best_eligible: Option<DeferredShrRateCandidateSummary>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeferredShrRatePostScoreFirstFailureHistogram {
    pub(crate) shr_unclassified: [u32; SHR_SYMBOLS.len()],
    pub(crate) shr_wrong: [u32; SHR_SYMBOLS.len()],
    pub(crate) phr_low_unclassified: u32,
    pub(crate) phr_high_unclassified: u32,
    pub(crate) length_zero: u32,
    pub(crate) length_oversize: u32,
    pub(crate) eligible: u32,
}

impl DeferredShrRatePostScoreFirstFailureHistogram {
    pub(crate) fn checked_total(&self) -> Option<u32> {
        self.shr_unclassified
            .iter()
            .chain(&self.shr_wrong)
            .copied()
            .chain([
                self.phr_low_unclassified,
                self.phr_high_unclassified,
                self.length_zero,
                self.length_oversize,
                self.eligible,
            ])
            .try_fold(0u32, u32::checked_add)
    }
}

/// Diagnostic-only later-predicate partition for the authoritative score-rejected cohort.
///
/// This record deliberately omits decoded PHR values and payload state. It
/// cannot make a hypothesis eligible for the authoritative selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateScoreCounterfactualAudit {
    pub(crate) evaluated_hypotheses: u32,
    pub(crate) score_rejected_hypotheses: u32,
    pub(crate) first_failures: DeferredShrRatePostScoreFirstFailureHistogram,
    pub(crate) best_structurally_eligible: Option<DeferredShrRateCandidateSummary>,
}

fn increment_audit_counter(counter: &mut u32) -> Result<()> {
    *counter = counter
        .checked_add(1)
        .ok_or_else(|| anyhow!("deferred selector audit counter overflow"))?;
    Ok(())
}

fn candidate_summary(
    selection: ShrRateTimingSelection,
    score: f64,
) -> DeferredShrRateCandidateSummary {
    DeferredShrRateCandidateSummary {
        selection,
        full_score_bits: score.to_bits(),
    }
}

fn deferred_late_start_outcome(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
) -> DeferredStructureOutcome {
    let chips: [f32; DEFERRED_STRUCTURAL_CHIPS] = std::array::from_fn(|chip_index| {
        interpolated_shr_rate_chip(
            metrics,
            start_offset,
            mu_sixteenths,
            rate_error_ppm,
            chip_index,
        )
    });
    deferred_structure(&chips)
}

fn audit_deferred_shr_rate_late_starts(
    metrics: &[PositionedMetric],
) -> Result<DeferredShrRateLateStartAudit> {
    ensure!(
        metrics.len() == DEFERRED_LATE_SELECTION_METRICS,
        "deferred late-start audit requires exactly {DEFERRED_LATE_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "deferred late-start audit metrics must be finite"
    );
    ensure!(
        metrics.windows(2).all(|pair| {
            pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
        }),
        "deferred late-start audit metrics must have contiguous source coordinates"
    );

    let mut evaluated_hypotheses = 0u32;
    let mut first_failures = DeferredShrRateFirstFailureHistogram::default();
    let mut best_eligible: Option<ScoredShrRateTimingSelection> = None;
    for start_offset in DEFERRED_LATE_START_BEGIN..DEFERRED_LATE_START_END {
        let start_offset_u8 = u8::try_from(start_offset)
            .map_err(|_| anyhow!("deferred late-start offset does not fit u8"))?;
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            let mu_sixteenths_u8 = u8::try_from(mu_sixteenths)
                .map_err(|_| anyhow!("deferred late-start phase does not fit u8"))?;
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                increment_audit_counter(&mut evaluated_hypotheses)?;
                let outcome = deferred_late_start_outcome(
                    metrics,
                    start_offset,
                    mu_sixteenths,
                    rate_error_ppm,
                );
                // Every hypothesis contributes to its first authoritative
                // predicate failure, or to the structurally eligible cohort.
                match outcome {
                    DeferredStructureOutcome::InterpolationNonfinite => {
                        increment_audit_counter(&mut first_failures.interpolation_nonfinite)?;
                    }
                    DeferredStructureOutcome::CorrelationUndefined => {
                        increment_audit_counter(&mut first_failures.correlation_undefined)?;
                    }
                    DeferredStructureOutcome::ScoreAtOrBelowCap { .. } => {
                        increment_audit_counter(&mut first_failures.score_at_or_below_cap)?;
                    }
                    DeferredStructureOutcome::ShrUnclassified { symbol_index, .. } => {
                        increment_audit_counter(
                            &mut first_failures.shr_unclassified[symbol_index],
                        )?;
                    }
                    DeferredStructureOutcome::ShrWrong { symbol_index, .. } => {
                        increment_audit_counter(&mut first_failures.shr_wrong[symbol_index])?;
                    }
                    DeferredStructureOutcome::PhrLowUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_low_unclassified)?;
                    }
                    DeferredStructureOutcome::PhrHighUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_high_unclassified)?;
                    }
                    DeferredStructureOutcome::LengthZero { .. } => {
                        increment_audit_counter(&mut first_failures.length_zero)?;
                    }
                    DeferredStructureOutcome::LengthOversize { .. } => {
                        increment_audit_counter(&mut first_failures.length_oversize)?;
                    }
                    DeferredStructureOutcome::Eligible {
                        phr_length: _,
                        score,
                    } => {
                        increment_audit_counter(&mut first_failures.eligible)?;
                        let candidate = ScoredShrRateTimingSelection {
                            selection: ShrRateTimingSelection {
                                timing: ShrTimingSelection {
                                    start_offset: start_offset_u8,
                                    mu_sixteenths: mu_sixteenths_u8,
                                    source_sample_index: metrics[start_offset].source_sample_index,
                                },
                                rate_error_ppm,
                            },
                            score,
                        };
                        if best_eligible
                            .is_none_or(|prior| shr_rate_candidate_is_better(candidate, prior))
                        {
                            best_eligible = Some(candidate);
                        }
                    }
                }
            }
        }
    }
    ensure!(
        evaluated_hypotheses == DEFERRED_LATE_HYPOTHESIS_COUNT,
        "deferred late-start audit did not evaluate the frozen hypothesis grid"
    );
    ensure!(
        first_failures.checked_total() == Some(evaluated_hypotheses),
        "deferred late-start first-failure histogram did not partition the grid"
    );
    ensure!(
        best_eligible.is_some() == (first_failures.eligible != 0),
        "deferred late-start eligibility count diverged from its best candidate"
    );
    Ok(DeferredShrRateLateStartAudit {
        evaluated_hypotheses,
        first_failures,
        best_eligible: best_eligible
            .map(|candidate| candidate_summary(candidate.selection, candidate.score)),
    })
}

pub(super) fn audit_deferred_shr_rate_timing(
    metrics: &[PositionedMetric],
) -> Result<DeferredShrRateSelectorAudit> {
    ensure!(
        metrics.len() == DEFERRED_SELECTION_METRICS,
        "deferred selector audit requires exactly {DEFERRED_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "deferred selector audit metrics must be finite"
    );
    ensure!(
        metrics.windows(2).all(|pair| {
            pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
        }),
        "deferred selector audit metrics must have contiguous source coordinates"
    );

    let mut evaluated_hypotheses = 0u32;
    let mut first_failures = DeferredShrRateFirstFailureHistogram::default();
    let mut best_defined: Option<ScoredShrRateTimingSelection> = None;

    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                increment_audit_counter(&mut evaluated_hypotheses)?;
                let chips: [f32; DEFERRED_STRUCTURAL_CHIPS] = std::array::from_fn(|chip_index| {
                    interpolated_shr_rate_chip(
                        metrics,
                        start_offset,
                        mu_sixteenths,
                        rate_error_ppm,
                        chip_index,
                    )
                });
                let outcome = deferred_structure(&chips);
                if let Some(score) = outcome.defined_score() {
                    let scored = ScoredShrRateTimingSelection {
                        selection: ShrRateTimingSelection {
                            timing: ShrTimingSelection {
                                start_offset: start_offset as u8,
                                mu_sixteenths: mu_sixteenths as u8,
                                source_sample_index: metrics[start_offset].source_sample_index,
                            },
                            rate_error_ppm,
                        },
                        score,
                    };
                    if best_defined.is_none_or(|prior| shr_rate_candidate_is_better(scored, prior))
                    {
                        best_defined = Some(scored);
                    }
                }
                // Each hypothesis contributes to its first failure in the
                // authoritative deferred predicate order, or to `eligible`.
                match outcome {
                    DeferredStructureOutcome::InterpolationNonfinite => {
                        increment_audit_counter(&mut first_failures.interpolation_nonfinite)?;
                    }
                    DeferredStructureOutcome::CorrelationUndefined => {
                        increment_audit_counter(&mut first_failures.correlation_undefined)?;
                    }
                    DeferredStructureOutcome::ScoreAtOrBelowCap { .. } => {
                        increment_audit_counter(&mut first_failures.score_at_or_below_cap)?;
                    }
                    DeferredStructureOutcome::ShrUnclassified { symbol_index, .. } => {
                        increment_audit_counter(
                            &mut first_failures.shr_unclassified[symbol_index],
                        )?;
                    }
                    DeferredStructureOutcome::ShrWrong { symbol_index, .. } => {
                        increment_audit_counter(&mut first_failures.shr_wrong[symbol_index])?;
                    }
                    DeferredStructureOutcome::PhrLowUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_low_unclassified)?;
                    }
                    DeferredStructureOutcome::PhrHighUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_high_unclassified)?;
                    }
                    DeferredStructureOutcome::LengthZero { .. } => {
                        increment_audit_counter(&mut first_failures.length_zero)?;
                    }
                    DeferredStructureOutcome::LengthOversize { .. } => {
                        increment_audit_counter(&mut first_failures.length_oversize)?;
                    }
                    DeferredStructureOutcome::Eligible { .. } => {
                        increment_audit_counter(&mut first_failures.eligible)?;
                    }
                }
            }
        }
    }
    ensure!(
        evaluated_hypotheses == SHR_RATE_HYPOTHESIS_COUNT,
        "deferred selector audit did not evaluate the frozen hypothesis grid"
    );
    ensure!(
        first_failures.checked_total() == Some(evaluated_hypotheses),
        "deferred selector first-failure histogram did not partition the grid"
    );
    let authoritative = select_deferred_shr_rate_timing(metrics)?;
    ensure!(
        authoritative.is_some() == (first_failures.eligible != 0),
        "deferred selector eligibility count diverged from the authoritative selector"
    );
    Ok(DeferredShrRateSelectorAudit {
        evaluated_hypotheses,
        first_failures,
        best_defined: best_defined
            .map(|candidate| candidate_summary(candidate.selection, candidate.score)),
        accepted_winner: authoritative
            .map(|candidate| candidate_summary(candidate.handoff.selection, candidate.score)),
    })
}

pub(super) fn audit_deferred_shr_rate_score_counterfactual(
    metrics: &[PositionedMetric],
) -> Result<DeferredShrRateScoreCounterfactualAudit> {
    ensure!(
        metrics.len() == DEFERRED_SELECTION_METRICS,
        "deferred score counterfactual requires exactly {DEFERRED_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "deferred score counterfactual metrics must be finite"
    );
    ensure!(
        metrics.windows(2).all(|pair| {
            pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
        }),
        "deferred score counterfactual metrics must have contiguous source coordinates"
    );

    let mut evaluated_hypotheses = 0u32;
    let mut score_rejected_hypotheses = 0u32;
    let mut first_failures = DeferredShrRatePostScoreFirstFailureHistogram::default();
    let mut best_structurally_eligible: Option<ScoredShrRateTimingSelection> = None;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                increment_audit_counter(&mut evaluated_hypotheses)?;
                let chips: [f32; DEFERRED_STRUCTURAL_CHIPS] = std::array::from_fn(|chip_index| {
                    interpolated_shr_rate_chip(
                        metrics,
                        start_offset,
                        mu_sixteenths,
                        rate_error_ppm,
                        chip_index,
                    )
                });
                let DeferredStructureOutcome::ScoreAtOrBelowCap { score } =
                    deferred_structure(&chips)
                else {
                    continue;
                };
                increment_audit_counter(&mut score_rejected_hypotheses)?;
                let selection = ShrRateTimingSelection {
                    timing: ShrTimingSelection {
                        start_offset: start_offset as u8,
                        mu_sixteenths: mu_sixteenths as u8,
                        source_sample_index: metrics[start_offset].source_sample_index,
                    },
                    rate_error_ppm,
                };
                match deferred_structure_after_score(&chips, score) {
                    DeferredStructureOutcome::ShrUnclassified { symbol_index, .. } => {
                        increment_audit_counter(&mut first_failures.shr_unclassified[symbol_index])?
                    }
                    DeferredStructureOutcome::ShrWrong { symbol_index, .. } => {
                        increment_audit_counter(&mut first_failures.shr_wrong[symbol_index])?
                    }
                    DeferredStructureOutcome::PhrLowUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_low_unclassified)?
                    }
                    DeferredStructureOutcome::PhrHighUnclassified { .. } => {
                        increment_audit_counter(&mut first_failures.phr_high_unclassified)?
                    }
                    DeferredStructureOutcome::LengthZero { .. } => {
                        increment_audit_counter(&mut first_failures.length_zero)?
                    }
                    DeferredStructureOutcome::LengthOversize { .. } => {
                        increment_audit_counter(&mut first_failures.length_oversize)?
                    }
                    DeferredStructureOutcome::Eligible { .. } => {
                        increment_audit_counter(&mut first_failures.eligible)?;
                        let scored = ScoredShrRateTimingSelection { selection, score };
                        if best_structurally_eligible
                            .is_none_or(|prior| shr_rate_candidate_is_better(scored, prior))
                        {
                            best_structurally_eligible = Some(scored);
                        }
                    }
                    DeferredStructureOutcome::InterpolationNonfinite
                    | DeferredStructureOutcome::CorrelationUndefined
                    | DeferredStructureOutcome::ScoreAtOrBelowCap { .. } => {
                        return Err(anyhow!(
                            "deferred post-score classifier returned a pre-score outcome"
                        ));
                    }
                }
            }
        }
    }
    ensure!(
        evaluated_hypotheses == SHR_RATE_HYPOTHESIS_COUNT,
        "deferred score counterfactual did not evaluate the frozen hypothesis grid"
    );
    ensure!(
        first_failures.checked_total() == Some(score_rejected_hypotheses),
        "deferred score counterfactual did not partition the score-rejected cohort"
    );
    ensure!(
        best_structurally_eligible.is_some() == (first_failures.eligible != 0),
        "deferred score counterfactual eligibility count diverged from its best candidate"
    );
    Ok(DeferredShrRateScoreCounterfactualAudit {
        evaluated_hypotheses,
        score_rejected_hypotheses,
        first_failures,
        best_structurally_eligible: best_structurally_eligible
            .map(|candidate| candidate_summary(candidate.selection, candidate.score)),
    })
}

fn deferred_shr_rate_diagnostic_metrics(
    samples: &[Complex32],
    event: DetectedBurst,
) -> Result<Vec<PositionedMetric>> {
    ensure!(
        !event.forced_end
            && event
                .detected_at
                .checked_add(1)
                .and_then(|end| end.checked_sub(event.replay_start))
                == Some(PRETRIGGER_SAMPLES),
        "deferred selector diagnostic requires one natural fixed-pretrigger event"
    );
    let start = usize::try_from(event.replay_start)
        .map_err(|_| anyhow!("deferred selector replay start does not fit usize"))?;
    let end = start
        .checked_add(DEFERRED_SELECTION_METRICS)
        .ok_or_else(|| anyhow!("deferred selector diagnostic span overflow"))?;
    ensure!(
        end <= samples.len()
            && u64::try_from(end)
                .map_err(|_| anyhow!("deferred selector diagnostic end does not fit u64"))?
                <= event.end_exclusive,
        "deferred selector diagnostic span exceeded its event or input"
    );
    let selector_samples = &samples[start..end];
    let last_metric_source_sample_index = event
        .replay_start
        .checked_add((DEFERRED_SELECTION_METRICS - 1) as u64)
        .ok_or_else(|| anyhow!("deferred selector diagnostic coordinate overflow"))?;
    // The core resets phase state at detector start, replays from replay_start,
    // and invokes this selector after exactly 832 metrics. A fresh state over
    // this exact span therefore reproduces that selector input without a run.
    let mut phase = PhaseDiscriminatorState::new();
    let mut metrics = Vec::with_capacity(DEFERRED_SELECTION_METRICS);
    for (offset, sample) in selector_samples.iter().copied().enumerate() {
        let source_sample_index = event
            .replay_start
            .checked_add(offset as u64)
            .ok_or_else(|| anyhow!("deferred selector diagnostic coordinate overflow"))?;
        metrics.push(phase.push(IndexedSample {
            value: sample,
            source_sample_index,
        })?);
    }
    ensure!(
        metrics.first().map(|metric| metric.source_sample_index) == Some(event.replay_start)
            && metrics.last().map(|metric| metric.source_sample_index)
                == Some(last_metric_source_sample_index)
            && metrics.windows(2).all(|pair| {
                pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
            }),
        "deferred selector diagnostic metric coordinates were not contiguous"
    );
    Ok(metrics)
}

fn deferred_shr_rate_late_start_diagnostic_metrics(
    samples: &[Complex32],
    event: DetectedBurst,
) -> Result<Vec<PositionedMetric>> {
    ensure!(
        !event.forced_end
            && event
                .detected_at
                .checked_add(1)
                .and_then(|end| end.checked_sub(event.replay_start))
                == Some(PRETRIGGER_SAMPLES),
        "deferred late-start diagnostic requires one natural fixed-pretrigger event"
    );
    let start = usize::try_from(event.replay_start)
        .map_err(|_| anyhow!("deferred late-start replay start does not fit usize"))?;
    let end = start
        .checked_add(DEFERRED_LATE_SELECTION_METRICS)
        .ok_or_else(|| anyhow!("deferred late-start diagnostic span overflow"))?;
    ensure!(
        end <= samples.len()
            && u64::try_from(end)
                .map_err(|_| anyhow!("deferred late-start diagnostic end does not fit u64"))?
                <= event.end_exclusive,
        "deferred late-start diagnostic span exceeded its event or input"
    );
    let selector_samples = &samples[start..end];
    let last_metric_source_sample_index = event
        .replay_start
        .checked_add((DEFERRED_LATE_SELECTION_METRICS - 1) as u64)
        .ok_or_else(|| anyhow!("deferred late-start diagnostic coordinate overflow"))?;
    let mut phase = PhaseDiscriminatorState::new();
    let mut metrics = Vec::with_capacity(DEFERRED_LATE_SELECTION_METRICS);
    for (offset, sample) in selector_samples.iter().copied().enumerate() {
        let source_sample_index = event
            .replay_start
            .checked_add(offset as u64)
            .ok_or_else(|| anyhow!("deferred late-start diagnostic coordinate overflow"))?;
        metrics.push(phase.push(IndexedSample {
            value: sample,
            source_sample_index,
        })?);
    }
    ensure!(
        metrics.first().map(|metric| metric.source_sample_index) == Some(event.replay_start)
            && metrics.last().map(|metric| metric.source_sample_index)
                == Some(last_metric_source_sample_index)
            && metrics.windows(2).all(|pair| {
                pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
            }),
        "deferred late-start diagnostic metric coordinates were not contiguous"
    );
    Ok(metrics)
}

pub(crate) fn diagnose_deferred_shr_rate_selection(
    samples: &[Complex32],
    event: DetectedBurst,
) -> Result<DeferredShrRateSelectorAudit> {
    audit_deferred_shr_rate_timing(&deferred_shr_rate_diagnostic_metrics(samples, event)?)
}

pub(crate) fn diagnose_deferred_shr_rate_score_counterfactual(
    samples: &[Complex32],
    event: DetectedBurst,
) -> Result<DeferredShrRateScoreCounterfactualAudit> {
    audit_deferred_shr_rate_score_counterfactual(&deferred_shr_rate_diagnostic_metrics(
        samples, event,
    )?)
}

pub(crate) fn diagnose_rejected_deferred_shr_rate_late_starts(
    samples: &[Complex32],
    event: DetectedBurst,
) -> Result<DeferredShrRateLateStartAudit> {
    let authoritative_metrics = deferred_shr_rate_diagnostic_metrics(samples, event)?;
    let authoritative = audit_deferred_shr_rate_timing(&authoritative_metrics)?;
    ensure!(
        authoritative.accepted_winner.is_none(),
        "late-start diagnostic requires authoritative 832-metric rejection"
    );

    let late_metrics = deferred_shr_rate_late_start_diagnostic_metrics(samples, event)?;
    ensure!(
        late_metrics[..DEFERRED_SELECTION_METRICS] == authoritative_metrics,
        "deferred late-start diagnostic prefix diverged from the authoritative selector input"
    );
    audit_deferred_shr_rate_late_starts(&late_metrics)
}
