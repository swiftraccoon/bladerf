//! Test-only causal burst replay, phase discrimination and clock recovery.
//!
//! The production worker graph remains unchanged while this bounded candidate
//! is qualified. Each output keeps its original 4 MSps source coordinate and
//! marks the first recovered chip after an atomic burst-state reset.

mod deferred_diagnostics;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, ensure};
use futuresdr::runtime::dev::prelude::*;

use super::burst_detector::{
    BurstStart, CausalBurstDetector, DetectedBurst, DetectorUpdate, MAX_BURST_SAMPLES,
    PRETRIGGER_SAMPLES,
};
use super::soft_decoder::{
    CHIP_SEQUENCES, UNIQUE_ANGULAR_CAP, classify_payload_symbol, template_correlation,
};
pub(crate) use deferred_diagnostics::{
    DeferredShrRateCandidateSummary, DeferredShrRateFirstFailureHistogram,
    DeferredShrRateLateStartAudit, DeferredShrRatePostScoreFirstFailureHistogram,
    DeferredShrRateScoreCounterfactualAudit, DeferredShrRateSelectorAudit,
    diagnose_deferred_shr_rate_score_counterfactual, diagnose_deferred_shr_rate_selection,
    diagnose_rejected_deferred_shr_rate_late_starts,
};

const PRETRIGGER_CAPACITY: usize = PRETRIGGER_SAMPLES as usize;
const PHASE_IIR_ALPHA: f32 = 0.00016;
const CLOCK_OMEGA: f32 = 2.0;
const CLOCK_GAIN_OMEGA: f32 = 0.000225;
const CLOCK_INITIAL_MU: f32 = 0.5;
const CLOCK_GAIN_MU: f32 = 0.03;
const CLOCK_RELATIVE_LIMIT: f32 = 0.0002;
const CLOCK_LOOK_AHEAD: usize = 3;
const MAX_METRIC_QUEUE: usize = CLOCK_LOOK_AHEAD + 1;
const SHR_SYMBOLS: [u8; 10] = [0, 0, 0, 0, 0, 0, 0, 0, 7, 10];
const SHR_SUFFIX_START_SYMBOL: usize = 4;
const SYMBOL_CHIPS: usize = 32;
const SCORED_CHIPS: usize = 30;
const SHR_CHIPS: usize = SHR_SYMBOLS.len() * SYMBOL_CHIPS;
const SHR_START_HYPOTHESES: usize = PRETRIGGER_CAPACITY;
const SHR_PHASE_HYPOTHESES: usize = 16;
const SHR_SELECTION_METRICS: usize = SHR_START_HYPOTHESES + 2 * (SHR_CHIPS - 1) + 1;
const SHR_RATE_HYPOTHESES_PPM: [i32; 3] = [-100, 0, 100];
const SHR_RATE_PPM_DENOMINATOR: i64 = 1_000_000;
const SHR_RATE_POSITION_DENOMINATOR: i64 = SHR_PHASE_HYPOTHESES as i64 * SHR_RATE_PPM_DENOMINATOR;
const SHR_RATE_SELECTION_METRICS: usize = SHR_SELECTION_METRICS + 1;
const DEFERRED_STRUCTURAL_SYMBOLS: usize = SHR_SYMBOLS.len() + 2;
const DEFERRED_STRUCTURAL_CHIPS: usize = DEFERRED_STRUCTURAL_SYMBOLS * SYMBOL_CHIPS;
const DEFERRED_SELECTION_METRICS: usize =
    SHR_START_HYPOTHESES + 2 * (DEFERRED_STRUCTURAL_CHIPS - 1) + 2;
const DEFERRED_PAYLOAD_CHIP_INDEX: usize = DEFERRED_STRUCTURAL_CHIPS;
const MIN_DEFERRED_PAYLOAD_LEFT_OFFSET: usize = 767;
const MAX_DEFERRED_HANDOFF_TAIL_METRICS: usize =
    DEFERRED_SELECTION_METRICS - MIN_DEFERRED_PAYLOAD_LEFT_OFFSET;
const MAX_SELECTION_RECORDS: usize = 2;
const SHR_RATE_HYPOTHESIS_COUNT: u32 =
    (SHR_START_HYPOTHESES * SHR_PHASE_HYPOTHESES * SHR_RATE_HYPOTHESES_PPM.len()) as u32;
const CLOCK_BOUNDARY_INTERVAL: u64 = 32;
const MAX_CLOCK_BOUNDARY_POINTS: usize =
    MAX_BURST_SAMPLES.div_ceil(CLOCK_BOUNDARY_INTERVAL) as usize;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PositionedChip {
    pub(crate) metric: f32,
    pub(crate) source_sample_index: u64,
    pub(crate) burst_start: bool,
    /// Present only on the sole marked first-payload chip of a deferred handoff.
    pub(crate) payload_length: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BurstClockStats {
    pub(crate) source_samples: u64,
    pub(crate) starts: u64,
    pub(crate) ends: u64,
    pub(crate) markers: u64,
    pub(crate) max_pretrigger: usize,
    pub(crate) max_replay: usize,
    pub(crate) max_metrics: usize,
    pub(crate) shr_locks: u64,
    pub(crate) shr_rejections: u64,
    pub(crate) shr_selections: [Option<ShrTimingSelection>; MAX_SELECTION_RECORDS],
    pub(crate) shr_selection_overflow: u64,
    pub(crate) max_shr_selection_metrics: usize,
    pub(crate) max_shr_metric_replay: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShrTimingSelection {
    pub(crate) start_offset: u8,
    pub(crate) mu_sixteenths: u8,
    pub(crate) source_sample_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateTimingSelection {
    pub(crate) timing: ShrTimingSelection,
    pub(crate) rate_error_ppm: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateClockStats {
    pub(crate) burst: BurstClockStats,
    pub(crate) shr_rate_selections: [Option<ShrRateTimingSelection>; MAX_SELECTION_RECORDS],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateClockStats {
    pub(crate) burst: BurstClockStats,
    pub(crate) deferred_selections: [Option<DeferredShrRateHandoff>; MAX_SELECTION_RECORDS],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateScoredCandidateTrace {
    pub(crate) selection: ShrRateTimingSelection,
    pub(crate) full_score_bits: u64,
    pub(crate) suffix_score_bits: [Option<u64>; SHR_SYMBOLS.len() - SHR_SUFFIX_START_SYMBOL],
    pub(crate) limiting_suffix_symbol: Option<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateSelectorAudit {
    pub(crate) evaluated_hypotheses: u32,
    pub(crate) eligible_hypotheses: u32,
    pub(crate) best_unconstrained: Option<ShrRateScoredCandidateTrace>,
    pub(crate) best_eligible: Option<ShrRateScoredCandidateTrace>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClockBoundaryPoint {
    pub(crate) chip_ordinal: u64,
    pub(crate) source_sample_index: u64,
    pub(crate) omega_bits: u32,
    pub(crate) mu_bits: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClockTrajectoryTrace {
    pub(crate) initial_omega_bits: u32,
    pub(crate) initial_mu_bits: u32,
    pub(crate) final_omega_bits: u32,
    pub(crate) final_mu_bits: u32,
    pub(crate) min_omega_bits: u32,
    pub(crate) max_omega_bits: u32,
    pub(crate) min_mu_bits: u32,
    pub(crate) max_mu_bits: u32,
    pub(crate) emitted_chips: u64,
    pub(crate) markers: u64,
    pub(crate) first_source_sample_index: Option<u64>,
    pub(crate) last_source_sample_index: Option<u64>,
    pub(crate) advance_histogram: [u64; MAX_METRIC_QUEUE],
    pub(crate) advance_overflow: u64,
    pub(crate) boundaries: Vec<ClockBoundaryPoint>,
    pub(crate) boundary_overflow: u64,
}

impl ClockTrajectoryTrace {
    fn new(omega: f32, mu: f32) -> Self {
        Self {
            initial_omega_bits: omega.to_bits(),
            initial_mu_bits: mu.to_bits(),
            final_omega_bits: omega.to_bits(),
            final_mu_bits: mu.to_bits(),
            min_omega_bits: omega.to_bits(),
            max_omega_bits: omega.to_bits(),
            min_mu_bits: mu.to_bits(),
            max_mu_bits: mu.to_bits(),
            emitted_chips: 0,
            markers: 0,
            first_source_sample_index: None,
            last_source_sample_index: None,
            advance_histogram: [0; MAX_METRIC_QUEUE],
            advance_overflow: 0,
            boundaries: Vec::with_capacity(MAX_CLOCK_BOUNDARY_POINTS),
            boundary_overflow: 0,
        }
    }

    fn observe(&mut self, chip: PositionedChip, advance: usize, omega: f32, mu: f32) -> Result<()> {
        ensure!(
            omega.is_finite() && mu.is_finite(),
            "clock trace state must be finite"
        );
        self.emitted_chips = self
            .emitted_chips
            .checked_add(1)
            .ok_or_else(|| anyhow!("clock trace chip counter overflow"))?;
        self.markers = self
            .markers
            .checked_add(u64::from(chip.burst_start))
            .ok_or_else(|| anyhow!("clock trace marker counter overflow"))?;
        self.first_source_sample_index
            .get_or_insert(chip.source_sample_index);
        self.last_source_sample_index = Some(chip.source_sample_index);
        self.final_omega_bits = omega.to_bits();
        self.final_mu_bits = mu.to_bits();
        self.min_omega_bits = f32::from_bits(self.min_omega_bits).min(omega).to_bits();
        self.max_omega_bits = f32::from_bits(self.max_omega_bits).max(omega).to_bits();
        self.min_mu_bits = f32::from_bits(self.min_mu_bits).min(mu).to_bits();
        self.max_mu_bits = f32::from_bits(self.max_mu_bits).max(mu).to_bits();
        if let Some(slot) = advance
            .checked_sub(1)
            .and_then(|index| self.advance_histogram.get_mut(index))
        {
            *slot = slot
                .checked_add(1)
                .ok_or_else(|| anyhow!("clock trace advance counter overflow"))?;
        } else {
            self.advance_overflow = self
                .advance_overflow
                .checked_add(1)
                .ok_or_else(|| anyhow!("clock trace advance overflow counter overflow"))?;
        }
        if self.emitted_chips.is_multiple_of(CLOCK_BOUNDARY_INTERVAL) {
            let point = ClockBoundaryPoint {
                chip_ordinal: self.emitted_chips,
                source_sample_index: chip.source_sample_index,
                omega_bits: omega.to_bits(),
                mu_bits: mu.to_bits(),
            };
            if self.boundaries.len() < MAX_CLOCK_BOUNDARY_POINTS {
                self.boundaries.push(point);
            } else {
                self.boundary_overflow = self
                    .boundary_overflow
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("clock trace boundary overflow counter overflow"))?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShrRateTraceOutcome {
    Locked(ShrRateTimingSelection),
    RejectedNoEligible,
    RejectedBeforeSelection,
    RejectedBeforeLock(ShrRateTimingSelection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateEventTrace {
    pub(crate) event: DetectedBurst,
    pub(crate) selector: Option<ShrRateSelectorAudit>,
    pub(crate) outcome: ShrRateTraceOutcome,
    pub(crate) clock: Option<ClockTrajectoryTrace>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShrRateDiagnosticTrace {
    pub(crate) events: [Option<ShrRateEventTrace>; MAX_SELECTION_RECORDS],
    pub(crate) overflow: u64,
}

impl Default for ShrRateDiagnosticTrace {
    fn default() -> Self {
        Self {
            events: std::array::from_fn(|_| None),
            overflow: 0,
        }
    }
}

#[derive(Debug)]
struct ActiveShrRateDiagnosticTrace {
    start: BurstStart,
    selector: Option<ShrRateSelectorAudit>,
    outcome: Option<ShrRateTraceOutcome>,
    clock: Option<ClockTrajectoryTrace>,
}

#[derive(Clone, Copy, Debug)]
struct IndexedSample {
    value: Complex32,
    source_sample_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PositionedMetric {
    value: f32,
    source_sample_index: u64,
}

#[derive(Debug)]
struct PhaseDiscriminatorState {
    last_complex: Complex32,
    phase_iir: f32,
}

impl PhaseDiscriminatorState {
    fn new() -> Self {
        Self {
            last_complex: Complex32::new(0.0, 0.0),
            phase_iir: 0.0,
        }
    }

    fn reset(&mut self) {
        self.last_complex = Complex32::new(0.0, 0.0);
        self.phase_iir = 0.0;
    }

    fn push(&mut self, sample: IndexedSample) -> Result<PositionedMetric> {
        let phase = (self.last_complex.conj() * sample.value).arg();
        ensure!(phase.is_finite(), "burst phase difference must be finite");
        self.last_complex = sample.value;
        self.phase_iir = (1.0 - PHASE_IIR_ALPHA) * self.phase_iir + PHASE_IIR_ALPHA * phase;
        let metric = phase - self.phase_iir;
        ensure!(metric.is_finite(), "burst phase metric must be finite");
        Ok(PositionedMetric {
            value: metric,
            source_sample_index: sample.source_sample_index,
        })
    }
}

#[derive(Debug)]
struct ClockRecoveryState {
    omega: f32,
    mu: f32,
    last_mm: f32,
    metrics: VecDeque<PositionedMetric>,
    mark_next_output: bool,
    next_payload_length: Option<u8>,
    max_metric_queue_observed: usize,
    last_advance: Option<usize>,
}

impl ClockRecoveryState {
    fn new() -> Self {
        Self {
            omega: CLOCK_OMEGA,
            mu: CLOCK_INITIAL_MU,
            last_mm: 0.0,
            metrics: VecDeque::with_capacity(MAX_METRIC_QUEUE),
            mark_next_output: true,
            next_payload_length: None,
            max_metric_queue_observed: 0,
            last_advance: None,
        }
    }

    fn reset(&mut self, initial_mu: f32) -> Result<()> {
        self.reset_with_timing(CLOCK_OMEGA, initial_mu)
    }

    fn reset_with_timing(&mut self, initial_omega: f32, initial_mu: f32) -> Result<()> {
        self.reset_with_handoff(initial_omega, initial_mu, 0.0, None)
    }

    fn reset_with_handoff(
        &mut self,
        initial_omega: f32,
        initial_mu: f32,
        prior_metric: f32,
        payload_length: Option<u8>,
    ) -> Result<()> {
        let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
        ensure!(
            initial_omega.is_finite()
                && (CLOCK_OMEGA - omega_limit..=CLOCK_OMEGA + omega_limit).contains(&initial_omega),
            "burst clock initial omega must be finite and within the nominal relative limit"
        );
        ensure!(
            initial_mu.is_finite() && (0.0..1.0).contains(&initial_mu),
            "burst clock initial mu must be finite and satisfy 0 <= mu < 1"
        );
        ensure!(
            prior_metric.is_finite(),
            "burst clock prior metric must be finite"
        );
        ensure!(
            payload_length.is_none_or(|length| (1..=127).contains(&length)),
            "burst clock payload length must be absent or satisfy 1 <= length <= 127"
        );
        self.omega = initial_omega;
        self.mu = initial_mu;
        // A deferred reset starts at payload chip 384, so the interpolated
        // chip-383 metric is the prior M&M decision rather than an idle zero.
        self.last_mm = prior_metric;
        self.metrics.clear();
        self.mark_next_output = true;
        self.next_payload_length = payload_length;
        self.last_advance = None;
        Ok(())
    }

    fn discard_tail(&mut self) {
        self.metrics.clear();
        self.mark_next_output = false;
        self.next_payload_length = None;
        self.last_advance = None;
    }

    fn push(&mut self, metric: PositionedMetric) -> Result<Option<PositionedChip>> {
        self.last_advance = None;
        self.metrics.push_back(metric);
        self.max_metric_queue_observed = self.max_metric_queue_observed.max(self.metrics.len());
        ensure!(
            self.metrics.len() <= MAX_METRIC_QUEUE,
            "burst clock metric queue exceeded its bound"
        );
        if self.metrics.len() <= CLOCK_LOOK_AHEAD {
            return Ok(None);
        }

        let left = self.metrics[0];
        let right = self.metrics[1];
        let output = left.value + self.mu * (right.value - left.value);
        ensure!(output.is_finite(), "burst clock output must be finite");
        let mm = slice(self.last_mm) * output - slice(output) * self.last_mm;
        ensure!(mm.is_finite(), "burst clock error must be finite");
        self.last_mm = output;

        self.omega += CLOCK_GAIN_OMEGA * mm;
        let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
        self.omega = CLOCK_OMEGA + (self.omega - CLOCK_OMEGA).clamp(-omega_limit, omega_limit);
        self.mu += self.omega + CLOCK_GAIN_MU * mm;
        let advance = self.mu.floor();
        ensure!(
            advance.is_finite() && advance >= 1.0,
            "burst clock advance must be finite and positive"
        );
        self.mu -= advance;
        let advance = advance as usize;
        ensure!(
            advance <= self.metrics.len(),
            "burst clock advance exceeded available metrics"
        );
        for _ in 0..advance {
            self.metrics.pop_front();
        }
        self.last_advance = Some(advance);

        let burst_start = std::mem::replace(&mut self.mark_next_output, false);
        let payload_length = self.next_payload_length.take();
        ensure!(
            payload_length.is_none() || burst_start,
            "burst clock payload length must accompany the first marked output"
        );
        Ok(Some(PositionedChip {
            metric: output,
            source_sample_index: left.source_sample_index,
            burst_start,
            payload_length,
        }))
    }

    fn take_last_advance(&mut self) -> Option<usize> {
        self.last_advance.take()
    }
}

fn slice(value: f32) -> f32 {
    if value > 0.0 { 1.0 } else { -1.0 }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimingMode {
    Immediate,
    ShrInitializer,
    ShrRateInitializer,
    DeferredShrRateInitializer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BurstTimingState {
    Idle,
    Selecting,
    Selected,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScoredShrTimingSelection {
    selection: ShrTimingSelection,
    score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScoredShrRateTimingSelection {
    selection: ShrRateTimingSelection,
    score: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeferredShrRateHandoff {
    pub(crate) selection: ShrRateTimingSelection,
    pub(crate) phr_length: u8,
    pub(crate) payload_left_offset: u16,
    pub(crate) payload_source_sample_index: u64,
    pub(crate) payload_mu_bits: u32,
    pub(crate) prior_metric_bits: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScoredDeferredShrRateTimingSelection {
    handoff: DeferredShrRateHandoff,
    score: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingShrTimingSelection {
    Phase(ShrTimingSelection),
    PhaseRate(ShrRateTimingSelection),
    Deferred(DeferredShrRateHandoff),
}

fn interpolated_shr_chip(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
    chip_index: usize,
) -> f32 {
    let left = metrics[start_offset + 2 * chip_index].value;
    let right = metrics[start_offset + 2 * chip_index + 1].value;
    let mu = mu_sixteenths as f32 / SHR_PHASE_HYPOTHESES as f32;
    left + mu * (right - left)
}

fn shr_rate_position(
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    chip_index: usize,
) -> (usize, f32) {
    let numerator = start_offset as i64 * SHR_RATE_POSITION_DENOMINATOR
        + mu_sixteenths as i64 * SHR_RATE_PPM_DENOMINATOR
        + 2 * SHR_PHASE_HYPOTHESES as i64
            * chip_index as i64
            * (SHR_RATE_PPM_DENOMINATOR + i64::from(rate_error_ppm));
    let left_index = usize::try_from(numerator.div_euclid(SHR_RATE_POSITION_DENOMINATOR))
        .expect("bounded SHR rate position must be nonnegative");
    let fraction_numerator = numerator.rem_euclid(SHR_RATE_POSITION_DENOMINATOR);
    let fraction = fraction_numerator as f32 / SHR_RATE_POSITION_DENOMINATOR as f32;
    (left_index, fraction)
}

fn interpolated_shr_rate_chip(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    chip_index: usize,
) -> f32 {
    let (left_index, fraction) =
        shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
    let left = metrics[left_index].value;
    let right = metrics[left_index + 1].value;
    left + fraction * (right - left)
}

fn full_shr_correlation(chips: &[f32; SHR_CHIPS]) -> Option<f64> {
    let mut dot = 0.0f64;
    let mut energy = 0.0f64;
    for (symbol_index, symbol) in SHR_SYMBOLS.into_iter().enumerate() {
        let sequence = CHIP_SEQUENCES[usize::from(symbol)];
        for chip_index in 1..=SCORED_CHIPS {
            let chip = f64::from(chips[symbol_index * SYMBOL_CHIPS + chip_index]);
            if !chip.is_finite() {
                return None;
            }
            let bit = 31 - chip_index;
            let expected = if sequence & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
            dot += chip * expected;
            energy += chip * chip;
        }
    }
    if !dot.is_finite() || !energy.is_finite() || energy <= 0.0 {
        return None;
    }
    let denominator = ((SHR_SYMBOLS.len() * SCORED_CHIPS) as f64 * energy).sqrt();
    (denominator.is_finite() && denominator > 0.0).then_some(dot / denominator)
}

fn shr_suffix_matches(chips: &[f32; SHR_CHIPS]) -> bool {
    SHR_SYMBOLS
        .iter()
        .copied()
        .enumerate()
        .skip(SHR_SUFFIX_START_SYMBOL)
        .all(|(symbol_index, expected_symbol)| {
            let start = symbol_index * SYMBOL_CHIPS;
            let window: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
                .try_into()
                .expect("SHR symbol window has fixed length");
            template_correlation(window, expected_symbol)
                .is_some_and(|score| score > UNIQUE_ANGULAR_CAP)
        })
}

fn shr_suffix_score_bits(
    chips: &[f32; SHR_CHIPS],
) -> [Option<u64>; SHR_SYMBOLS.len() - SHR_SUFFIX_START_SYMBOL] {
    std::array::from_fn(|suffix_index| {
        let symbol_index = SHR_SUFFIX_START_SYMBOL + suffix_index;
        let start = symbol_index * SYMBOL_CHIPS;
        let window: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
            .try_into()
            .expect("SHR symbol window has fixed length");
        template_correlation(window, SHR_SYMBOLS[symbol_index]).map(f64::to_bits)
    })
}

fn limiting_suffix_symbol(
    scores: &[Option<u64>; SHR_SYMBOLS.len() - SHR_SUFFIX_START_SYMBOL],
) -> Option<u8> {
    scores
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| match (left, right) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => f64::from_bits(*left)
                .partial_cmp(&f64::from_bits(*right))
                .unwrap_or(std::cmp::Ordering::Equal),
        })
        .map(|(index, _)| (SHR_SUFFIX_START_SYMBOL + index) as u8)
}

fn traced_candidate_is_better(
    candidate: ShrRateScoredCandidateTrace,
    prior: ShrRateScoredCandidateTrace,
) -> bool {
    let candidate_score = f64::from_bits(candidate.full_score_bits);
    let prior_score = f64::from_bits(prior.full_score_bits);
    candidate_score > prior_score
        || (candidate_score == prior_score
            && shr_rate_tie_key(candidate.selection) < shr_rate_tie_key(prior.selection))
}

fn audit_shr_rate_timing(metrics: &[PositionedMetric]) -> Result<ShrRateSelectorAudit> {
    ensure!(
        metrics.len() == SHR_RATE_SELECTION_METRICS,
        "SHR rate timing audit requires exactly {SHR_RATE_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "SHR rate timing audit metrics must be finite"
    );
    ensure!(
        metrics
            .windows(2)
            .all(|pair| pair[1].source_sample_index == pair[0].source_sample_index + 1),
        "SHR rate timing audit metrics must have contiguous source coordinates"
    );

    let mut evaluated_hypotheses = 0u32;
    let mut eligible_hypotheses = 0u32;
    let mut best_unconstrained: Option<ShrRateScoredCandidateTrace> = None;
    let mut best_eligible: Option<ShrRateScoredCandidateTrace> = None;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                evaluated_hypotheses = evaluated_hypotheses
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("SHR rate audit hypothesis counter overflow"))?;
                let chips = std::array::from_fn(|chip_index| {
                    interpolated_shr_rate_chip(
                        metrics,
                        start_offset,
                        mu_sixteenths,
                        rate_error_ppm,
                        chip_index,
                    )
                });
                let suffix_score_bits = shr_suffix_score_bits(&chips);
                let Some(full_score) = full_shr_correlation(&chips) else {
                    continue;
                };
                let candidate = ShrRateScoredCandidateTrace {
                    selection: ShrRateTimingSelection {
                        timing: ShrTimingSelection {
                            start_offset: start_offset as u8,
                            mu_sixteenths: mu_sixteenths as u8,
                            source_sample_index: metrics[start_offset].source_sample_index,
                        },
                        rate_error_ppm,
                    },
                    full_score_bits: full_score.to_bits(),
                    suffix_score_bits,
                    limiting_suffix_symbol: limiting_suffix_symbol(&suffix_score_bits),
                };
                if best_unconstrained
                    .is_none_or(|prior| traced_candidate_is_better(candidate, prior))
                {
                    best_unconstrained = Some(candidate);
                }
                let eligible = suffix_score_bits.iter().all(|score| {
                    score.is_some_and(|score| f64::from_bits(score) > UNIQUE_ANGULAR_CAP)
                });
                if !eligible {
                    continue;
                }
                eligible_hypotheses = eligible_hypotheses
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("SHR rate audit eligible counter overflow"))?;
                if best_eligible.is_none_or(|prior| traced_candidate_is_better(candidate, prior)) {
                    best_eligible = Some(candidate);
                }
            }
        }
    }
    ensure!(
        evaluated_hypotheses == SHR_RATE_HYPOTHESIS_COUNT,
        "SHR rate audit did not evaluate the frozen hypothesis grid"
    );
    Ok(ShrRateSelectorAudit {
        evaluated_hypotheses,
        eligible_hypotheses,
        best_unconstrained,
        best_eligible,
    })
}

fn select_shr_timing(metrics: &[PositionedMetric]) -> Result<Option<ScoredShrTimingSelection>> {
    ensure!(
        metrics.len() == SHR_SELECTION_METRICS,
        "SHR timing selection requires exactly {SHR_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "SHR timing selection metrics must be finite"
    );
    ensure!(
        metrics
            .windows(2)
            .all(|pair| pair[1].source_sample_index == pair[0].source_sample_index + 1),
        "SHR timing selection metrics must have contiguous source coordinates"
    );

    let mut best: Option<ScoredShrTimingSelection> = None;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            let chips = std::array::from_fn(|chip_index| {
                interpolated_shr_chip(metrics, start_offset, mu_sixteenths, chip_index)
            });
            if !shr_suffix_matches(&chips) {
                continue;
            }
            let Some(score) = full_shr_correlation(&chips) else {
                continue;
            };
            let candidate = ScoredShrTimingSelection {
                selection: ShrTimingSelection {
                    start_offset: start_offset as u8,
                    mu_sixteenths: mu_sixteenths as u8,
                    source_sample_index: metrics[start_offset].source_sample_index,
                },
                score,
            };
            if best.is_none_or(|prior| candidate.score > prior.score) {
                best = Some(candidate);
            }
        }
    }
    Ok(best)
}

fn shr_rate_tie_key(selection: ShrRateTimingSelection) -> (u8, u8, u32, i32) {
    (
        selection.timing.start_offset,
        selection.timing.mu_sixteenths,
        selection.rate_error_ppm.unsigned_abs(),
        selection.rate_error_ppm,
    )
}

fn shr_rate_candidate_is_better(
    candidate: ScoredShrRateTimingSelection,
    prior: ScoredShrRateTimingSelection,
) -> bool {
    candidate.score > prior.score
        || (candidate.score == prior.score
            && shr_rate_tie_key(candidate.selection) < shr_rate_tie_key(prior.selection))
}

fn select_shr_rate_timing(
    metrics: &[PositionedMetric],
) -> Result<Option<ScoredShrRateTimingSelection>> {
    ensure!(
        metrics.len() == SHR_RATE_SELECTION_METRICS,
        "SHR rate timing selection requires exactly {SHR_RATE_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "SHR rate timing selection metrics must be finite"
    );
    ensure!(
        metrics
            .windows(2)
            .all(|pair| pair[1].source_sample_index == pair[0].source_sample_index + 1),
        "SHR rate timing selection metrics must have contiguous source coordinates"
    );

    let mut best: Option<ScoredShrRateTimingSelection> = None;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                let chips = std::array::from_fn(|chip_index| {
                    interpolated_shr_rate_chip(
                        metrics,
                        start_offset,
                        mu_sixteenths,
                        rate_error_ppm,
                        chip_index,
                    )
                });
                if !shr_suffix_matches(&chips) {
                    continue;
                }
                let Some(score) = full_shr_correlation(&chips) else {
                    continue;
                };
                let candidate = ScoredShrRateTimingSelection {
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
                if best.is_none_or(|prior| shr_rate_candidate_is_better(candidate, prior)) {
                    best = Some(candidate);
                }
            }
        }
    }
    Ok(best)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DeferredStructureOutcome {
    InterpolationNonfinite,
    CorrelationUndefined,
    ScoreAtOrBelowCap { score: f64 },
    ShrUnclassified { symbol_index: usize, score: f64 },
    ShrWrong { symbol_index: usize, score: f64 },
    PhrLowUnclassified { score: f64 },
    PhrHighUnclassified { score: f64 },
    LengthZero { score: f64 },
    LengthOversize { score: f64 },
    Eligible { phr_length: u8, score: f64 },
}

impl DeferredStructureOutcome {
    fn defined_score(self) -> Option<f64> {
        match self {
            Self::InterpolationNonfinite | Self::CorrelationUndefined => None,
            Self::ScoreAtOrBelowCap { score }
            | Self::ShrUnclassified { score, .. }
            | Self::ShrWrong { score, .. }
            | Self::PhrLowUnclassified { score }
            | Self::PhrHighUnclassified { score }
            | Self::LengthZero { score }
            | Self::LengthOversize { score }
            | Self::Eligible { score, .. } => Some(score),
        }
    }
}

// Keep the authoritative and counterfactual paths on one exact post-score
// predicate order. Only the authoritative caller decides whether the aggregate
// score cap admits a hypothesis.
fn deferred_structure_after_score(
    chips: &[f32; DEFERRED_STRUCTURAL_CHIPS],
    score: f64,
) -> DeferredStructureOutcome {
    for (symbol_index, expected_symbol) in SHR_SYMBOLS.into_iter().enumerate() {
        let start = symbol_index * SYMBOL_CHIPS;
        let window: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
            .try_into()
            .expect("deferred SHR symbol has fixed length");
        match classify_payload_symbol(window) {
            None => {
                return DeferredStructureOutcome::ShrUnclassified {
                    symbol_index,
                    score,
                };
            }
            Some(actual) if actual != expected_symbol => {
                return DeferredStructureOutcome::ShrWrong {
                    symbol_index,
                    score,
                };
            }
            Some(_) => {}
        }
    }
    let phr_symbol = |symbol_index: usize| {
        let start = symbol_index * SYMBOL_CHIPS;
        let window: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
            .try_into()
            .expect("deferred PHR symbol has fixed length");
        classify_payload_symbol(window)
    };
    let Some(low) = phr_symbol(SHR_SYMBOLS.len()) else {
        return DeferredStructureOutcome::PhrLowUnclassified { score };
    };
    let Some(high) = phr_symbol(SHR_SYMBOLS.len() + 1) else {
        return DeferredStructureOutcome::PhrHighUnclassified { score };
    };
    let phr_length = (high << 4) | low;
    if phr_length == 0 {
        DeferredStructureOutcome::LengthZero { score }
    } else if phr_length > 127 {
        DeferredStructureOutcome::LengthOversize { score }
    } else {
        DeferredStructureOutcome::Eligible { phr_length, score }
    }
}

fn deferred_structure(chips: &[f32; DEFERRED_STRUCTURAL_CHIPS]) -> DeferredStructureOutcome {
    if !chips.iter().all(|chip| chip.is_finite()) {
        return DeferredStructureOutcome::InterpolationNonfinite;
    }
    let shr: &[f32; SHR_CHIPS] = chips[..SHR_CHIPS]
        .try_into()
        .expect("deferred SHR prefix has fixed length");
    let Some(score) = full_shr_correlation(shr) else {
        return DeferredStructureOutcome::CorrelationUndefined;
    };
    if score <= UNIQUE_ANGULAR_CAP {
        return DeferredStructureOutcome::ScoreAtOrBelowCap { score };
    }
    deferred_structure_after_score(chips, score)
}

fn deferred_candidate_is_better(
    candidate: ScoredDeferredShrRateTimingSelection,
    prior: ScoredDeferredShrRateTimingSelection,
) -> bool {
    candidate.score > prior.score
        || (candidate.score == prior.score
            && shr_rate_tie_key(candidate.handoff.selection)
                < shr_rate_tie_key(prior.handoff.selection))
}

fn deferred_candidate(
    metrics: &[PositionedMetric],
    chips: &[f32; DEFERRED_STRUCTURAL_CHIPS],
    selection: ShrRateTimingSelection,
    phr_length: u8,
    score: f64,
) -> Result<ScoredDeferredShrRateTimingSelection> {
    let (payload_left_offset, payload_mu) = shr_rate_position(
        usize::from(selection.timing.start_offset),
        usize::from(selection.timing.mu_sixteenths),
        selection.rate_error_ppm,
        DEFERRED_PAYLOAD_CHIP_INDEX,
    );
    ensure!(
        payload_left_offset <= DEFERRED_SELECTION_METRICS,
        "deferred payload handoff exceeded the selector buffer boundary"
    );
    let buffered_handoff_tail = DEFERRED_SELECTION_METRICS
        .checked_sub(payload_left_offset)
        .expect("deferred payload boundary was checked");
    ensure!(
        buffered_handoff_tail <= MAX_DEFERRED_HANDOFF_TAIL_METRICS,
        "deferred payload handoff tail exceeded its bound"
    );
    let payload_source_sample_index = metrics[0]
        .source_sample_index
        .checked_add(payload_left_offset as u64)
        .ok_or_else(|| anyhow!("deferred payload source coordinate overflow"))?;
    Ok(ScoredDeferredShrRateTimingSelection {
        handoff: DeferredShrRateHandoff {
            selection,
            phr_length,
            payload_left_offset: u16::try_from(payload_left_offset)
                .expect("deferred payload offset is bounded below u16::MAX"),
            payload_source_sample_index,
            payload_mu_bits: payload_mu.to_bits(),
            prior_metric_bits: chips[DEFERRED_STRUCTURAL_CHIPS - 1].to_bits(),
        },
        score,
    })
}

fn select_deferred_shr_rate_timing(
    metrics: &[PositionedMetric],
) -> Result<Option<ScoredDeferredShrRateTimingSelection>> {
    ensure!(
        metrics.len() == DEFERRED_SELECTION_METRICS,
        "deferred SHR/PHR selection requires exactly {DEFERRED_SELECTION_METRICS} metrics"
    );
    ensure!(
        metrics.iter().all(|metric| metric.value.is_finite()),
        "deferred SHR/PHR selection metrics must be finite"
    );
    ensure!(
        metrics.windows(2).all(|pair| {
            pair[0].source_sample_index.checked_add(1) == Some(pair[1].source_sample_index)
        }),
        "deferred SHR/PHR selection metrics must have contiguous source coordinates"
    );

    let mut best: Option<ScoredDeferredShrRateTimingSelection> = None;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                let chips: [f32; DEFERRED_STRUCTURAL_CHIPS] = std::array::from_fn(|chip_index| {
                    interpolated_shr_rate_chip(
                        metrics,
                        start_offset,
                        mu_sixteenths,
                        rate_error_ppm,
                        chip_index,
                    )
                });
                let DeferredStructureOutcome::Eligible { phr_length, score } =
                    deferred_structure(&chips)
                else {
                    continue;
                };
                let selection = ShrRateTimingSelection {
                    timing: ShrTimingSelection {
                        start_offset: start_offset as u8,
                        mu_sixteenths: mu_sixteenths as u8,
                        source_sample_index: metrics[start_offset].source_sample_index,
                    },
                    rate_error_ppm,
                };
                let candidate = deferred_candidate(metrics, &chips, selection, phr_length, score)?;
                if best.is_none_or(|prior| deferred_candidate_is_better(candidate, prior)) {
                    best = Some(candidate);
                }
            }
        }
    }
    Ok(best)
}

fn shr_rate_clock_omega(rate_error_ppm: i32) -> f32 {
    CLOCK_OMEGA * (1.0 + rate_error_ppm as f32 / SHR_RATE_PPM_DENOMINATOR as f32)
}

#[derive(Debug)]
struct BurstClockCore {
    detector: CausalBurstDetector,
    pretrigger: VecDeque<IndexedSample>,
    replay: VecDeque<IndexedSample>,
    shr_metrics: VecDeque<PositionedMetric>,
    phase: PhaseDiscriminatorState,
    clock: ClockRecoveryState,
    timing_mode: TimingMode,
    timing_state: BurstTimingState,
    pending_selection: Option<PendingShrTimingSelection>,
    deferred_output_produced: bool,
    ready: Option<PositionedChip>,
    active: bool,
    finished: bool,
    starts: u64,
    ends: u64,
    markers: u64,
    max_pretrigger_observed: usize,
    max_replay_observed: usize,
    shr_locks: u64,
    shr_rejections: u64,
    shr_selections: [Option<ShrTimingSelection>; MAX_SELECTION_RECORDS],
    shr_rate_selections: [Option<ShrRateTimingSelection>; MAX_SELECTION_RECORDS],
    deferred_selections: [Option<DeferredShrRateHandoff>; MAX_SELECTION_RECORDS],
    shr_selection_overflow: u64,
    max_shr_selection_metrics: usize,
    max_shr_metric_replay: usize,
    diagnostic_trace: Option<ShrRateDiagnosticTrace>,
    active_diagnostic_trace: Option<ActiveShrRateDiagnosticTrace>,
}

impl BurstClockCore {
    fn new() -> Self {
        Self::with_timing_mode(TimingMode::Immediate)
    }

    fn with_shr_initializer() -> Self {
        Self::with_timing_mode(TimingMode::ShrInitializer)
    }

    fn with_shr_rate_initializer() -> Self {
        Self::with_timing_mode(TimingMode::ShrRateInitializer)
    }

    fn with_deferred_shr_rate_initializer() -> Self {
        Self::with_timing_mode(TimingMode::DeferredShrRateInitializer)
    }

    fn with_shr_rate_diagnostic() -> Self {
        let mut core = Self::with_timing_mode(TimingMode::ShrRateInitializer);
        core.diagnostic_trace = Some(ShrRateDiagnosticTrace::default());
        core
    }

    fn with_timing_mode(timing_mode: TimingMode) -> Self {
        let shr_metric_capacity = match timing_mode {
            TimingMode::DeferredShrRateInitializer => DEFERRED_SELECTION_METRICS,
            TimingMode::Immediate | TimingMode::ShrInitializer | TimingMode::ShrRateInitializer => {
                SHR_RATE_SELECTION_METRICS
            }
        };
        Self {
            detector: CausalBurstDetector::new(),
            pretrigger: VecDeque::with_capacity(PRETRIGGER_CAPACITY),
            replay: VecDeque::with_capacity(PRETRIGGER_CAPACITY),
            shr_metrics: VecDeque::with_capacity(shr_metric_capacity),
            phase: PhaseDiscriminatorState::new(),
            clock: ClockRecoveryState::new(),
            timing_mode,
            timing_state: BurstTimingState::Idle,
            pending_selection: None,
            deferred_output_produced: false,
            ready: None,
            active: false,
            finished: false,
            starts: 0,
            ends: 0,
            markers: 0,
            max_pretrigger_observed: 0,
            max_replay_observed: 0,
            shr_locks: 0,
            shr_rejections: 0,
            shr_selections: [None; MAX_SELECTION_RECORDS],
            shr_rate_selections: [None; MAX_SELECTION_RECORDS],
            deferred_selections: [None; MAX_SELECTION_RECORDS],
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 0,
            max_shr_metric_replay: 0,
            diagnostic_trace: None,
            active_diagnostic_trace: None,
        }
    }

    fn accept_source(&mut self, value: Complex32) -> Result<()> {
        ensure!(!self.finished, "burst clock core is finished");
        ensure!(self.ready.is_none(), "burst clock output is pending");
        ensure!(
            self.replay.is_empty(),
            "burst replay must drain before source input"
        );
        ensure!(
            self.timing_state != BurstTimingState::Selected || self.shr_metrics.is_empty(),
            "SHR metric replay must drain before source input"
        );

        let source_sample_index = self.detector.samples_seen();
        let was_active = self.active;
        let update = self.detector.push_with_transition(value)?;
        if self.pretrigger.len() == PRETRIGGER_CAPACITY {
            self.pretrigger.pop_front();
        }
        self.pretrigger.push_back(IndexedSample {
            value,
            source_sample_index,
        });
        self.max_pretrigger_observed = self.max_pretrigger_observed.max(self.pretrigger.len());
        ensure!(
            self.pretrigger.len() <= PRETRIGGER_CAPACITY,
            "burst pretrigger queue exceeded its bound"
        );
        if was_active {
            self.process_active_sample(IndexedSample {
                value,
                source_sample_index,
            })?;
        }
        self.apply_update(update, was_active)?;
        Ok(())
    }

    fn apply_update(&mut self, update: DetectorUpdate, was_active: bool) -> Result<()> {
        ensure!(
            !(update.started.is_some() && update.ended.is_some()),
            "detector cannot start and end on one sample"
        );
        if let Some(start) = update.started {
            ensure!(
                !was_active && !self.active,
                "detector restarted an active burst"
            );
            ensure!(
                self.pretrigger.len() == PRETRIGGER_CAPACITY,
                "detector trigger lacks the fixed pretrigger window"
            );
            ensure!(
                self.pretrigger
                    .front()
                    .map(|sample| sample.source_sample_index)
                    == Some(start.replay_start),
                "detector replay start does not match retained pretrigger"
            );
            ensure!(
                self.pretrigger
                    .back()
                    .map(|sample| sample.source_sample_index)
                    == Some(start.detected_at),
                "detector trigger coordinate does not match retained input"
            );
            ensure!(
                self.timing_state == BurstTimingState::Idle,
                "burst timing state was not idle at detector start"
            );
            ensure!(
                self.shr_metrics.is_empty()
                    && self.pending_selection.is_none()
                    && !self.deferred_output_produced,
                "burst timing buffers were not empty at detector start"
            );
            if self.diagnostic_trace.is_some() {
                ensure!(
                    self.active_diagnostic_trace.is_none(),
                    "burst diagnostic trace was already active at detector start"
                );
                self.active_diagnostic_trace = Some(ActiveShrRateDiagnosticTrace {
                    start,
                    selector: None,
                    outcome: None,
                    clock: None,
                });
            }
            self.phase.reset();
            match self.timing_mode {
                TimingMode::Immediate => {
                    self.clock.reset(CLOCK_INITIAL_MU)?;
                    self.timing_state = BurstTimingState::Selected;
                }
                TimingMode::ShrInitializer
                | TimingMode::ShrRateInitializer
                | TimingMode::DeferredShrRateInitializer => {
                    self.clock.discard_tail();
                    self.timing_state = BurstTimingState::Selecting;
                }
            }
            self.replay.extend(self.pretrigger.iter().copied());
            self.max_replay_observed = self.max_replay_observed.max(self.replay.len());
            ensure!(
                self.replay.len() <= PRETRIGGER_CAPACITY,
                "burst replay queue exceeded its bound"
            );
            self.active = true;
            self.starts = self
                .starts
                .checked_add(1)
                .ok_or_else(|| anyhow!("burst start counter overflow"))?;
        }
        if let Some(event) = update.ended {
            ensure!(
                was_active && self.active,
                "detector ended an inactive burst"
            );
            self.active = false;
            self.end_active_timing()?;
            self.finalize_diagnostic_event(event)?;
            self.ends = self
                .ends
                .checked_add(1)
                .ok_or_else(|| anyhow!("burst end counter overflow"))?;
        }
        Ok(())
    }

    fn advance_replay(&mut self) -> Result<()> {
        ensure!(self.ready.is_none(), "burst clock output is pending");
        let sample = self
            .replay
            .pop_front()
            .ok_or_else(|| anyhow!("burst replay queue is empty"))?;
        self.process_active_sample(sample)?;
        Ok(())
    }

    fn advance_metric_replay(&mut self) -> Result<()> {
        ensure!(self.ready.is_none(), "burst clock output is pending");
        if self.pending_selection.is_some()
            && !matches!(
                self.pending_selection,
                Some(PendingShrTimingSelection::Deferred(_))
            )
        {
            self.commit_pending_selection()?;
        }
        let metric = self
            .shr_metrics
            .pop_front()
            .ok_or_else(|| anyhow!("SHR metric replay queue is empty"))?;
        self.ready = self.push_selected_clock_metric(metric)?;
        Ok(())
    }

    fn push_selected_clock_metric(
        &mut self,
        metric: PositionedMetric,
    ) -> Result<Option<PositionedChip>> {
        let output = self.push_clock_metric(metric)?;
        if let Some(chip) = output
            && let Some(PendingShrTimingSelection::Deferred(handoff)) = self.pending_selection
        {
            ensure!(
                chip.burst_start
                    && chip.payload_length == Some(handoff.phr_length)
                    && chip.source_sample_index == handoff.payload_source_sample_index,
                "first deferred output lost its payload marker, length, or coordinate"
            );
            // Retaining a selection is not a lock until the four-metric clock
            // queue actually yields its marked first-payload output. This lets
            // detector end or EOF reject a zero-tail handoff without a marker.
            ensure!(
                !self.deferred_output_produced,
                "deferred handoff produced its first payload output more than once"
            );
            self.deferred_output_produced = true;
            self.commit_pending_selection()?;
        }
        Ok(output)
    }

    fn push_clock_metric(&mut self, metric: PositionedMetric) -> Result<Option<PositionedChip>> {
        let output = self.clock.push(metric)?;
        let advance = self.clock.take_last_advance();
        ensure!(
            output.is_some() == advance.is_some(),
            "burst clock output and advance trace diverged"
        );
        if let (Some(chip), Some(advance), Some(active)) =
            (output, advance, self.active_diagnostic_trace.as_mut())
        {
            let trace = active
                .clock
                .as_mut()
                .ok_or_else(|| anyhow!("clock diagnostic output preceded timing selection"))?;
            trace.observe(chip, advance, self.clock.omega, self.clock.mu)?;
        }
        Ok(output)
    }

    fn process_active_sample(&mut self, sample: IndexedSample) -> Result<()> {
        let metric = self.phase.push(sample)?;
        match self.timing_state {
            BurstTimingState::Selected => {
                self.ready = self.push_selected_clock_metric(metric)?;
            }
            BurstTimingState::Selecting => {
                self.shr_metrics.push_back(metric);
                self.max_shr_selection_metrics =
                    self.max_shr_selection_metrics.max(self.shr_metrics.len());
                let required_metrics = match self.timing_mode {
                    TimingMode::ShrInitializer => SHR_SELECTION_METRICS,
                    TimingMode::ShrRateInitializer => SHR_RATE_SELECTION_METRICS,
                    TimingMode::DeferredShrRateInitializer => DEFERRED_SELECTION_METRICS,
                    TimingMode::Immediate => {
                        return Err(anyhow!("immediate timing mode entered SHR selection"));
                    }
                };
                ensure!(
                    self.shr_metrics.len() <= required_metrics,
                    "SHR timing selection buffer exceeded its bound"
                );
                if self.shr_metrics.len() == required_metrics {
                    self.finish_shr_selection()?;
                }
            }
            BurstTimingState::Rejected => {}
            BurstTimingState::Idle => {
                return Err(anyhow!("active burst has idle timing state"));
            }
        }
        Ok(())
    }

    fn finish_shr_selection(&mut self) -> Result<()> {
        ensure!(
            self.timing_state == BurstTimingState::Selecting,
            "SHR timing selection finished outside selection state"
        );
        match self.timing_mode {
            TimingMode::ShrInitializer => self.finish_shr_phase_selection(),
            TimingMode::ShrRateInitializer => self.finish_shr_rate_selection(),
            TimingMode::DeferredShrRateInitializer => self.finish_deferred_shr_rate_selection(),
            TimingMode::Immediate => {
                Err(anyhow!("immediate timing mode cannot finish SHR selection"))
            }
        }
    }

    fn finish_shr_phase_selection(&mut self) -> Result<()> {
        let Some(scored) = select_shr_timing(self.shr_metrics.make_contiguous())? else {
            self.reject_shr_selection()?;
            return Ok(());
        };
        let selection = scored.selection;
        self.clock
            .reset(f32::from(selection.mu_sixteenths) / SHR_PHASE_HYPOTHESES as f32)?;
        for _ in 0..usize::from(selection.start_offset) {
            self.shr_metrics.pop_front();
        }
        self.max_shr_metric_replay = self.max_shr_metric_replay.max(self.shr_metrics.len());
        ensure!(
            self.shr_metrics.len() <= SHR_SELECTION_METRICS,
            "SHR metric replay queue exceeded its bound"
        );
        self.pending_selection = Some(PendingShrTimingSelection::Phase(selection));
        self.timing_state = BurstTimingState::Selected;
        Ok(())
    }

    fn finish_shr_rate_selection(&mut self) -> Result<()> {
        let scored = select_shr_rate_timing(self.shr_metrics.make_contiguous())?;
        let audit = if self.active_diagnostic_trace.is_some() {
            Some(audit_shr_rate_timing(self.shr_metrics.make_contiguous())?)
        } else {
            None
        };
        if let Some(audit) = audit {
            let audited = audit
                .best_eligible
                .map(|candidate| (candidate.selection, candidate.full_score_bits));
            let authoritative =
                scored.map(|candidate| (candidate.selection, candidate.score.to_bits()));
            ensure!(
                audited == authoritative,
                "independent SHR rate audit diverged from authoritative selector"
            );
            self.active_diagnostic_trace
                .as_mut()
                .expect("diagnostic trace presence was checked")
                .selector = Some(audit);
        }
        let Some(scored) = scored else {
            self.reject_shr_selection()?;
            return Ok(());
        };
        let selection = scored.selection;
        self.clock.reset_with_timing(
            shr_rate_clock_omega(selection.rate_error_ppm),
            f32::from(selection.timing.mu_sixteenths) / SHR_PHASE_HYPOTHESES as f32,
        )?;
        if let Some(active) = self.active_diagnostic_trace.as_mut() {
            ensure!(
                active.clock.is_none(),
                "clock diagnostic trace was initialized more than once"
            );
            active.clock = Some(ClockTrajectoryTrace::new(self.clock.omega, self.clock.mu));
        }
        for _ in 0..usize::from(selection.timing.start_offset) {
            self.shr_metrics.pop_front();
        }
        self.max_shr_metric_replay = self.max_shr_metric_replay.max(self.shr_metrics.len());
        ensure!(
            self.shr_metrics.len() <= SHR_RATE_SELECTION_METRICS,
            "SHR rate metric replay queue exceeded its bound"
        );
        self.pending_selection = Some(PendingShrTimingSelection::PhaseRate(selection));
        self.timing_state = BurstTimingState::Selected;
        Ok(())
    }

    fn finish_deferred_shr_rate_selection(&mut self) -> Result<()> {
        let Some(scored) = select_deferred_shr_rate_timing(self.shr_metrics.make_contiguous())?
        else {
            self.reject_shr_selection()?;
            return Ok(());
        };
        let handoff = scored.handoff;
        self.clock.reset_with_handoff(
            shr_rate_clock_omega(handoff.selection.rate_error_ppm),
            f32::from_bits(handoff.payload_mu_bits),
            f32::from_bits(handoff.prior_metric_bits),
            Some(handoff.phr_length),
        )?;
        // Selection consumed exactly 832 metrics. Depending on the selected
        // timing, payload chip 384 starts at offset 767..=832, so replay only
        // the retained 0..=65 tail and then stream new metrics into the same
        // bounded four-metric clock queue.
        for _ in 0..usize::from(handoff.payload_left_offset) {
            self.shr_metrics.pop_front();
        }
        self.max_shr_metric_replay = self.max_shr_metric_replay.max(self.shr_metrics.len());
        ensure!(
            self.shr_metrics.len() <= MAX_DEFERRED_HANDOFF_TAIL_METRICS,
            "deferred handoff replay queue exceeded its bound"
        );
        if let Some(first) = self.shr_metrics.front() {
            ensure!(
                first.source_sample_index == handoff.payload_source_sample_index,
                "deferred handoff replay lost its payload coordinate"
            );
        }
        ensure!(
            !self.deferred_output_produced,
            "deferred output proof remained set before a new handoff"
        );
        self.pending_selection = Some(PendingShrTimingSelection::Deferred(handoff));
        self.timing_state = BurstTimingState::Selected;
        Ok(())
    }

    fn commit_pending_selection(&mut self) -> Result<()> {
        let selection = self
            .pending_selection
            .as_ref()
            .copied()
            .ok_or_else(|| anyhow!("SHR timing selection was not pending"))?;
        if matches!(selection, PendingShrTimingSelection::Deferred(_)) {
            ensure!(
                self.deferred_output_produced,
                "deferred selection cannot commit before its first payload output"
            );
        }
        let selection = self
            .pending_selection
            .take()
            .expect("pending selection was validated before commit");
        let lock_index = usize::try_from(self.shr_locks)
            .map_err(|_| anyhow!("SHR lock count does not fit usize"))?;
        let stored = match selection {
            PendingShrTimingSelection::Phase(selection) => self
                .shr_selections
                .get_mut(lock_index)
                .map(|slot| *slot = Some(selection))
                .is_some(),
            PendingShrTimingSelection::PhaseRate(selection) => self
                .shr_rate_selections
                .get_mut(lock_index)
                .map(|slot| *slot = Some(selection))
                .is_some(),
            PendingShrTimingSelection::Deferred(selection) => self
                .deferred_selections
                .get_mut(lock_index)
                .map(|slot| *slot = Some(selection))
                .is_some(),
        };
        if let PendingShrTimingSelection::PhaseRate(selection) = selection
            && let Some(active) = self.active_diagnostic_trace.as_mut()
        {
            ensure!(
                active.outcome.is_none(),
                "joint SHR diagnostic outcome was already set before lock"
            );
            active.outcome = Some(ShrRateTraceOutcome::Locked(selection));
        }
        if !stored {
            self.shr_selection_overflow = self
                .shr_selection_overflow
                .checked_add(1)
                .ok_or_else(|| anyhow!("SHR selection overflow counter overflow"))?;
        }
        self.shr_locks = self
            .shr_locks
            .checked_add(1)
            .ok_or_else(|| anyhow!("SHR lock counter overflow"))?;
        if matches!(selection, PendingShrTimingSelection::Deferred(_)) {
            self.deferred_output_produced = false;
        }
        Ok(())
    }

    fn reject_shr_selection(&mut self) -> Result<()> {
        ensure!(
            matches!(
                self.timing_mode,
                TimingMode::ShrInitializer
                    | TimingMode::ShrRateInitializer
                    | TimingMode::DeferredShrRateInitializer
            ),
            "immediate timing mode cannot reject SHR selection"
        );
        ensure!(
            matches!(
                self.timing_state,
                BurstTimingState::Selecting | BurstTimingState::Selected
            ) && self.pending_selection.is_none(),
            "SHR rejection requires an uncommitted selection state"
        );
        if let Some(active) = self.active_diagnostic_trace.as_mut()
            && active.outcome.is_none()
        {
            active.outcome = Some(
                if active
                    .selector
                    .is_some_and(|selector| selector.best_eligible.is_none())
                {
                    ShrRateTraceOutcome::RejectedNoEligible
                } else {
                    ShrRateTraceOutcome::RejectedBeforeSelection
                },
            );
        }
        self.shr_metrics.clear();
        self.clock.discard_tail();
        self.deferred_output_produced = false;
        self.timing_state = BurstTimingState::Rejected;
        self.shr_rejections = self
            .shr_rejections
            .checked_add(1)
            .ok_or_else(|| anyhow!("SHR rejection counter overflow"))?;
        Ok(())
    }

    fn end_active_timing(&mut self) -> Result<()> {
        match self.timing_state {
            BurstTimingState::Selected if self.pending_selection.is_some() => {
                let selection = self
                    .pending_selection
                    .take()
                    .expect("pending selection presence was checked");
                if let PendingShrTimingSelection::PhaseRate(selection) = selection
                    && let Some(active) = self.active_diagnostic_trace.as_mut()
                {
                    ensure!(
                        active.outcome.is_none(),
                        "joint SHR diagnostic outcome was already set before pending rejection"
                    );
                    active.outcome = Some(ShrRateTraceOutcome::RejectedBeforeLock(selection));
                }
                self.reject_shr_selection()?;
            }
            BurstTimingState::Selected => self.clock.discard_tail(),
            BurstTimingState::Selecting => self.reject_shr_selection()?,
            BurstTimingState::Rejected => {}
            BurstTimingState::Idle => {
                return Err(anyhow!("detector ended a burst with idle timing state"));
            }
        }
        ensure!(
            self.shr_metrics.is_empty(),
            "detector ended while SHR metric replay was pending"
        );
        self.shr_metrics.clear();
        self.pending_selection = None;
        self.deferred_output_produced = false;
        self.timing_state = BurstTimingState::Idle;
        Ok(())
    }

    fn finalize_diagnostic_event(&mut self, event: DetectedBurst) -> Result<()> {
        let Some(trace) = self.diagnostic_trace.as_mut() else {
            ensure!(
                self.active_diagnostic_trace.is_none(),
                "inactive diagnostic retained an event trace"
            );
            return Ok(());
        };
        let active = self
            .active_diagnostic_trace
            .take()
            .ok_or_else(|| anyhow!("diagnostic detector end lacked an active trace"))?;
        ensure!(
            active.start.replay_start == event.replay_start
                && active.start.detected_at == event.detected_at,
            "diagnostic detector coordinates changed before event finalization"
        );
        let event_trace = ShrRateEventTrace {
            event,
            selector: active.selector,
            outcome: active
                .outcome
                .ok_or_else(|| anyhow!("diagnostic event ended without a selector outcome"))?,
            clock: active.clock,
        };
        if let Some(slot) = trace.events.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(event_trace);
        } else {
            trace.overflow = trace
                .overflow
                .checked_add(1)
                .ok_or_else(|| anyhow!("diagnostic event overflow counter overflow"))?;
        }
        Ok(())
    }

    fn take_ready(&mut self) -> Option<PositionedChip> {
        let chip = self.ready.take();
        if chip.is_some_and(|chip| chip.burst_start) {
            self.markers += 1;
        }
        chip
    }

    fn has_pending(&self) -> bool {
        self.ready.is_some()
            || !self.replay.is_empty()
            || (self.timing_state == BurstTimingState::Selected && !self.shr_metrics.is_empty())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        ensure!(
            !self.has_pending(),
            "burst clock pending work must drain before EOF"
        );
        if let Some(event) = self.detector.finish() {
            ensure!(self.active, "detector forced EOF for an inactive burst");
            self.active = false;
            self.end_active_timing()?;
            self.finalize_diagnostic_event(event)?;
            self.ends = self
                .ends
                .checked_add(1)
                .ok_or_else(|| anyhow!("burst end counter overflow"))?;
        }
        self.finished = true;
        ensure!(
            self.active_diagnostic_trace.is_none(),
            "diagnostic event trace remained active at EOF"
        );
        Ok(())
    }

    fn stats(&self) -> BurstClockStats {
        BurstClockStats {
            source_samples: self.detector.samples_seen(),
            starts: self.starts,
            ends: self.ends,
            markers: self.markers,
            max_pretrigger: self.max_pretrigger_observed,
            max_replay: self.max_replay_observed,
            max_metrics: self.clock.max_metric_queue_observed,
            shr_locks: self.shr_locks,
            shr_rejections: self.shr_rejections,
            shr_selections: self.shr_selections,
            shr_selection_overflow: self.shr_selection_overflow,
            max_shr_selection_metrics: self.max_shr_selection_metrics,
            max_shr_metric_replay: self.max_shr_metric_replay,
        }
    }

    fn rate_stats(&self) -> ShrRateClockStats {
        ShrRateClockStats {
            burst: self.stats(),
            shr_rate_selections: self.shr_rate_selections,
        }
    }

    fn deferred_stats(&self) -> DeferredShrRateClockStats {
        DeferredShrRateClockStats {
            burst: self.stats(),
            deferred_selections: self.deferred_selections,
        }
    }

    fn diagnostic_trace(&self) -> Option<ShrRateDiagnosticTrace> {
        self.diagnostic_trace.clone()
    }
}

#[derive(Debug)]
enum BurstClockStatsObserver {
    None,
    Burst(Arc<Mutex<Option<BurstClockStats>>>),
    ShrRate(Arc<Mutex<Option<ShrRateClockStats>>>),
    Deferred(Arc<Mutex<Option<DeferredShrRateClockStats>>>),
}

#[derive(Block)]
pub(crate) struct BurstClockRecovery<
    I = DefaultCpuReader<Complex32>,
    O = DefaultCpuWriter<PositionedChip>,
> where
    I: CpuBufferReader<Item = Complex32>,
    O: CpuBufferWriter<Item = PositionedChip>,
{
    #[input]
    input: I,
    #[output]
    output: O,
    core: BurstClockCore,
    stats_observer: BurstClockStatsObserver,
    diagnostic_observer: Option<Arc<Mutex<Option<ShrRateDiagnosticTrace>>>>,
    stats_published: bool,
}

impl<I, O> BurstClockRecovery<I, O>
where
    I: CpuBufferReader<Item = Complex32>,
    O: CpuBufferWriter<Item = PositionedChip>,
{
    pub(crate) fn new() -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::new(),
            stats_observer: BurstClockStatsObserver::None,
            diagnostic_observer: None,
            stats_published: false,
        }
    }

    pub(crate) fn with_stats_observer(observer: Arc<Mutex<Option<BurstClockStats>>>) -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::new(),
            stats_observer: BurstClockStatsObserver::Burst(observer),
            diagnostic_observer: None,
            stats_published: false,
        }
    }

    pub(crate) fn with_shr_stats_observer(observer: Arc<Mutex<Option<BurstClockStats>>>) -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::with_shr_initializer(),
            stats_observer: BurstClockStatsObserver::Burst(observer),
            diagnostic_observer: None,
            stats_published: false,
        }
    }

    pub(crate) fn with_shr_rate_stats_observer(
        observer: Arc<Mutex<Option<ShrRateClockStats>>>,
    ) -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::with_shr_rate_initializer(),
            stats_observer: BurstClockStatsObserver::ShrRate(observer),
            diagnostic_observer: None,
            stats_published: false,
        }
    }

    pub(crate) fn with_shr_rate_diagnostic_observers(
        stats_observer: Arc<Mutex<Option<ShrRateClockStats>>>,
        diagnostic_observer: Arc<Mutex<Option<ShrRateDiagnosticTrace>>>,
    ) -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::with_shr_rate_diagnostic(),
            stats_observer: BurstClockStatsObserver::ShrRate(stats_observer),
            diagnostic_observer: Some(diagnostic_observer),
            stats_published: false,
        }
    }

    pub(crate) fn with_deferred_shr_rate_stats_observer(
        observer: Arc<Mutex<Option<DeferredShrRateClockStats>>>,
    ) -> Self {
        Self {
            input: I::default(),
            output: O::default(),
            core: BurstClockCore::with_deferred_shr_rate_initializer(),
            stats_observer: BurstClockStatsObserver::Deferred(observer),
            diagnostic_observer: None,
            stats_published: false,
        }
    }

    fn publish_stats(&mut self) -> Result<()> {
        if self.stats_published {
            return Ok(());
        }
        match &self.stats_observer {
            BurstClockStatsObserver::None => {}
            BurstClockStatsObserver::Burst(observer) => {
                *observer
                    .lock()
                    .map_err(|_| anyhow!("burst clock statistics lock was poisoned"))? =
                    Some(self.core.stats());
            }
            BurstClockStatsObserver::ShrRate(observer) => {
                *observer
                    .lock()
                    .map_err(|_| anyhow!("burst clock rate statistics lock was poisoned"))? =
                    Some(self.core.rate_stats());
            }
            BurstClockStatsObserver::Deferred(observer) => {
                *observer
                    .lock()
                    .map_err(|_| anyhow!("deferred SHR statistics lock was poisoned"))? =
                    Some(self.core.deferred_stats());
            }
        }
        if let Some(observer) = &self.diagnostic_observer {
            *observer
                .lock()
                .map_err(|_| anyhow!("burst clock diagnostic lock was poisoned"))? = Some(
                self.core
                    .diagnostic_trace()
                    .ok_or_else(|| anyhow!("diagnostic observer used without diagnostic core"))?,
            );
        }
        self.stats_published = true;
        Ok(())
    }
}

impl<I, O> Kernel for BurstClockRecovery<I, O>
where
    I: CpuBufferReader<Item = Complex32>,
    O: CpuBufferWriter<Item = PositionedChip>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        let input = self.input.slice();
        let input_len = input.len();
        let output = self.output.slice();
        let mut consumed = 0;
        let mut produced = 0;

        while produced < output.len() {
            if let Some(chip) = self.core.take_ready() {
                output[produced] = chip;
                produced += 1;
            } else if self.core.timing_state == BurstTimingState::Selected
                && !self.core.shr_metrics.is_empty()
            {
                self.core.advance_metric_replay()?;
            } else if !self.core.replay.is_empty() {
                self.core.advance_replay()?;
            } else if consumed < input_len {
                self.core.accept_source(input[consumed])?;
                consumed += 1;
            } else {
                break;
            }
        }

        self.input.consume(consumed);
        self.output.produce(produced);
        if self.input.finished() && consumed == input_len && !self.core.has_pending() {
            self.core.finish()?;
            self.publish_stats()?;
            io.finished = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
