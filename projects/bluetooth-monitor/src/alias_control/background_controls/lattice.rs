//! Fixed sample-index lattice and matched-rate analysis.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::super::matched_rate::{FILTER_HALF_LEN, filter_settings, matched_views};
use super::super::report::{MatchedRateAccumulator, MatchedRateReport};
use super::super::{CONTROL_SAMPLE_RATE_HZ, cf32_slice};
use crate::burst_families::complex_direct_feature;

// This fixed margin excludes the authenticated channelizer FIR transient; it is
// deliberately not represented as receiver or RF settling evidence.
pub(super) const FIRST_OUTPUT_INDEX: u64 = 400_000;
pub(super) const WINDOW_OUTPUT_SAMPLES: u64 = 512;
// 65,537 alternates start parity and advances one sample modulo 65,536. It does
// not establish a relationship to any host or channelizer buffer boundary.
pub(super) const WINDOW_STRIDE_OUTPUT_SAMPLES: u64 = 65_537;
// A fixed, bounded population selected before observing window signal metrics.
pub(super) const WINDOW_COUNT: u64 = 48;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextWindow {
    StartUnavailable,
    TailUnavailable,
    Available { start: u64, count: u64 },
}

pub(super) fn context_window(
    output_start: u64,
    first_valid_output: u64,
    output_extent: u64,
) -> Result<ContextWindow> {
    let half = FILTER_HALF_LEN as u64;
    let earliest_context_backed_start = first_valid_output
        .checked_add(half)
        .context("background valid start overflow")?;
    if output_start < earliest_context_backed_start {
        return Ok(ContextWindow::StartUnavailable);
    }
    let output_end = output_start
        .checked_add(WINDOW_OUTPUT_SAMPLES)
        .context("background window end overflow")?;
    let context_end = output_end
        .checked_add(half)
        .context("background context end overflow")?;
    if output_end > output_extent || context_end > output_extent {
        return Ok(ContextWindow::TailUnavailable);
    }
    let context_start = output_start
        .checked_sub(half)
        .context("background context start underflow")?;
    Ok(ContextWindow::Available {
        start: context_start,
        count: WINDOW_OUTPUT_SAMPLES + 2 * half,
    })
}

pub(super) fn window_start(index: u64) -> Result<u64> {
    ensure!(
        index < WINDOW_COUNT,
        "background window index is out of range"
    );
    FIRST_OUTPUT_INDEX
        .checked_add(
            index
                .checked_mul(WINDOW_STRIDE_OUTPUT_SAMPLES)
                .context("background lattice offset overflow")?,
        )
        .context("background lattice start overflow")
}

#[derive(Debug, Serialize)]
pub(super) struct LatticeSettings {
    pub(super) first_output_index: u64,
    window_output_samples: u64,
    stride_output_samples: u64,
    window_count: u64,
    pub(super) last_output_start_index: u64,
    pub(super) last_output_end_exclusive: u64,
    pub(super) context_samples_per_window: u64,
    pub(super) first_context_start_index: u64,
    pub(super) last_context_end_exclusive: u64,
    pub(super) inter_context_gap_samples: u64,
    pub(super) absolute_even_start_windows: u64,
    pub(super) absolute_odd_start_windows: u64,
    contexts_disjoint: bool,
    fixed_sample_index_lattice: bool,
    duration_matched_to_candidate_windows: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct LatticeAccounting {
    planned_windows: u64,
    scheduled_windows: u64,
    context_available_windows: u64,
    unavailable_windows: u64,
    start_context_unavailable_windows: u64,
    tail_context_unavailable_windows: u64,
    pub(super) analyzed_windows: u64,
    accounted_windows: u64,
    accounting_closed: bool,
    pub(super) analyzed_output_samples: u64,
    authenticated_output_samples: u64,
    analyzed_fraction: [u64; 2],
    analyzed_percentage: f64,
}

pub(super) fn lattice_settings() -> Result<LatticeSettings> {
    let half = FILTER_HALF_LEN as u64;
    let context_samples = WINDOW_OUTPUT_SAMPLES + 2 * half;
    let last_start = window_start(WINDOW_COUNT - 1)?;
    let last_output_end = last_start
        .checked_add(WINDOW_OUTPUT_SAMPLES)
        .context("last background output end overflow")?;
    let last_context_end = last_output_end
        .checked_add(half)
        .context("last background context end overflow")?;
    let first_context_start = FIRST_OUTPUT_INDEX
        .checked_sub(half)
        .context("first background context start underflow")?;
    let inter_context_gap = WINDOW_STRIDE_OUTPUT_SAMPLES
        .checked_sub(context_samples)
        .context("background contexts overlap")?;
    let even_starts = (0..WINDOW_COUNT)
        .filter(|index| window_start(*index).is_ok_and(|start| start.is_multiple_of(2)))
        .count() as u64;
    ensure!(
        even_starts * 2 == WINDOW_COUNT,
        "background lattice start parity is imbalanced"
    );
    Ok(LatticeSettings {
        first_output_index: FIRST_OUTPUT_INDEX,
        window_output_samples: WINDOW_OUTPUT_SAMPLES,
        stride_output_samples: WINDOW_STRIDE_OUTPUT_SAMPLES,
        window_count: WINDOW_COUNT,
        last_output_start_index: last_start,
        last_output_end_exclusive: last_output_end,
        context_samples_per_window: context_samples,
        first_context_start_index: first_context_start,
        last_context_end_exclusive: last_context_end,
        inter_context_gap_samples: inter_context_gap,
        absolute_even_start_windows: even_starts,
        absolute_odd_start_windows: WINDOW_COUNT - even_starts,
        contexts_disjoint: true,
        fixed_sample_index_lattice: true,
        duration_matched_to_candidate_windows: false,
    })
}

pub(super) fn analyze_lattice(
    cf32: &[u8],
    first_valid_output: u64,
    output_extent: u64,
    interrupted: &AtomicBool,
) -> Result<(MatchedRateReport, LatticeAccounting)> {
    let mut matched_rate = MatchedRateAccumulator::default();
    let mut start_unavailable = 0u64;
    let mut tail_unavailable = 0u64;
    let mut analyzed = 0u64;
    for index in 0..WINDOW_COUNT {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "background controls interrupted"
        );
        let output_start = window_start(index)?;
        let (context_start, context_count) =
            match context_window(output_start, first_valid_output, output_extent)? {
                ContextWindow::StartUnavailable => {
                    start_unavailable += 1;
                    matched_rate.observe_start_context_unavailable();
                    continue;
                }
                ContextWindow::TailUnavailable => {
                    tail_unavailable += 1;
                    matched_rate.observe_tail_context_unavailable();
                    continue;
                }
                ContextWindow::Available { start, count } => (start, count),
            };
        let context = cf32_slice(cf32, context_start, context_count)?;
        let views = matched_views(
            &context,
            output_start,
            WINDOW_OUTPUT_SAMPLES as usize,
            interrupted,
        )?;
        matched_rate.observe(
            complex_direct_feature(&views.four_msps, CONTROL_SAMPLE_RATE_HZ, interrupted)?,
            complex_direct_feature(
                &views.two_msps_global_even,
                CONTROL_SAMPLE_RATE_HZ / 2,
                interrupted,
            )?,
            complex_direct_feature(
                &views.two_msps_global_odd,
                CONTROL_SAMPLE_RATE_HZ / 2,
                interrupted,
            )?,
        );
        analyzed += 1;
    }
    let accounted = start_unavailable
        .checked_add(tail_unavailable)
        .and_then(|count| count.checked_add(analyzed))
        .context("background window accounting overflow")?;
    ensure!(
        accounted == WINDOW_COUNT,
        "background window accounting differs"
    );
    let analyzed_samples = analyzed
        .checked_mul(WINDOW_OUTPUT_SAMPLES)
        .context("background analyzed sample count overflow")?;
    let unavailable = start_unavailable
        .checked_add(tail_unavailable)
        .context("background unavailable window count overflow")?;
    Ok((
        matched_rate.finish(WINDOW_COUNT, WINDOW_COUNT, filter_settings())?,
        LatticeAccounting {
            planned_windows: WINDOW_COUNT,
            scheduled_windows: WINDOW_COUNT,
            context_available_windows: analyzed,
            unavailable_windows: unavailable,
            start_context_unavailable_windows: start_unavailable,
            tail_context_unavailable_windows: tail_unavailable,
            analyzed_windows: analyzed,
            accounted_windows: accounted,
            accounting_closed: true,
            analyzed_output_samples: analyzed_samples,
            authenticated_output_samples: output_extent,
            analyzed_fraction: [analyzed_samples, output_extent],
            analyzed_percentage: (analyzed_samples as f64 * 1_000_000.0 / output_extent as f64)
                .round()
                / 10_000.0,
        },
    ))
}
