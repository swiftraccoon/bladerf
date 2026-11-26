//! Verify public FR1 broadcast blocks associated with admitted synchronization.

mod demod;
mod payload;
mod polar;

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::Serialize;

use crate::SAMPLE_RATE;
use crate::dsp::Iq16;

use super::{Cell, Profile, channel::Channelizer, check_stop};

/// Upper bound on public broadcast observations examined per replayed profile.
pub(crate) const MAX_OBSERVATIONS: usize = 64;

#[derive(Serialize)]
struct Interpretation {
    dmrs_index: u8,
    dmrs_score: f32,
    l_max: u8,
    mib: payload::Mib,
}

#[derive(Serialize)]
pub(super) struct Frame {
    physical_cell_id: u16,
    pss_start_channel_sample: usize,
    interpretations: Vec<Interpretation>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Evidence {
    OutsideGuardedSideband,
    Checked {
        examined_observations: usize,
        omitted_observations: usize,
        frames: Vec<Frame>,
    },
}

impl Evidence {
    pub(super) fn frame_count(&self) -> usize {
        match self {
            Self::OutsideGuardedSideband => 0,
            Self::Checked { frames, .. } => frames.len(),
        }
    }
}

pub(super) fn examine(
    iq: &[Iq16],
    center_hz: u64,
    profile: Profile,
    cells: &[Cell],
    stop: &AtomicBool,
) -> Result<Evidence> {
    if !profile.sideband_covers(center_hz, true) {
        return Ok(Evidence::OutsideGuardedSideband);
    }
    let mut candidates: Vec<_> = cells
        .iter()
        .flat_map(|cell| {
            cell.observations
                .iter()
                .map(move |item| (cell.physical_cell_id, item))
        })
        .collect();
    let omitted_observations = candidates.len().saturating_sub(MAX_OBSERVATIONS);
    if omitted_observations > 0 {
        candidates.sort_by(|(_, a), (_, b)| {
            (b.pss_score * b.sss_score).total_cmp(&(a.pss_score * a.sss_score))
        });
        candidates.truncate(MAX_OBSERVATIONS);
    }
    candidates.sort_unstable_by_key(|(pci, item)| (item.pss_start_channel_sample, *pci));

    let samples_per_channel_sample = f64::from(SAMPLE_RATE) / f64::from(profile.sample_rate());
    let offset_hz = profile.ss_ref_hz() as f64 - center_hz as f64;
    let mut frames = Vec::new();
    for &(physical_cell_id, observation) in &candidates {
        check_stop(stop)?;
        // Translate the resampler's output interval end back to its original
        // sample clock and remove the causal FIR delay.
        let start = (observation.pss_start_channel_sample + 1) as f64 * samples_per_channel_sample
            - 1.0
            - Channelizer::DELAY_INPUT_SAMPLES as f64;
        let mut interpretations = Vec::new();
        for hypothesis in demod::hypotheses(
            iq,
            offset_hz,
            profile.subcarrier_spacing_hz,
            start,
            observation.carrier_offset_hz,
            physical_cell_id,
        ) {
            check_stop(stop)?;
            if let Some(bits) = polar::decode(&hypothesis.llrs)
                && let Some(mib) = payload::decode(
                    bits,
                    physical_cell_id,
                    hypothesis.l_max,
                    hypothesis.dmrs_index,
                )
            {
                interpretations.push(Interpretation {
                    dmrs_index: hypothesis.dmrs_index,
                    dmrs_score: hypothesis.score,
                    l_max: hypothesis.l_max,
                    mib,
                });
            }
        }
        if !interpretations.is_empty() {
            frames.push(Frame {
                physical_cell_id,
                pss_start_channel_sample: observation.pss_start_channel_sample,
                interpretations,
            });
        }
    }
    Ok(Evidence::Checked {
        examined_observations: candidates.len(),
        omitted_observations,
        frames,
    })
}
