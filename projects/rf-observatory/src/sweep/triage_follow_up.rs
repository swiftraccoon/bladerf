//! Plan two offset receive dwells for each probeable narrow triage clip.

mod input;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Result, ensure};
use serde::Serialize;

use super::offset::{Band, place_pair};
use super::plan::{self, Plan};

#[derive(Debug, Serialize)]
pub(super) struct FollowUp {
    source_clip_count: usize,
    eligible_clip_count: usize,
    targets: Vec<Target>,
}

#[derive(Debug, Serialize)]
struct Target {
    file: String,
    sha256: String,
    source_tune_directory: String,
    source_center_hz: u64,
    lower_hz: u64,
    upper_hz: u64,
    target_hz: u64,
    centers_hz: [u64; 2],
}

/// Read a triage receipt and plan bounded sparse follow-up without opening a receiver.
pub(super) fn read_plan(path: &Path, dwell_ms: u64, max_iq_bytes: u64) -> Result<Plan> {
    let source = input::read(path)?;
    let eligible_clip_count = source.clips.len();
    ensure!(
        eligible_clip_count > 0,
        "triage report has no measured narrow clips"
    );
    let mut centers = BTreeSet::new();
    let mut targets = Vec::new();
    for clip in source.clips {
        let band = Band {
            lower_hz: clip.lower_hz,
            upper_hz: clip.upper_hz,
            target_hz: clip.target_hz,
            source_low_hz: clip.observed_center_hz,
            source_high_hz: clip.observed_center_hz,
        };
        if let Some(pair) = place_pair(&band, &centers) {
            centers.extend(pair);
            targets.push(Target {
                file: clip.file,
                sha256: clip.sha256,
                source_tune_directory: clip.tune_directory,
                source_center_hz: clip.observed_center_hz,
                lower_hz: clip.lower_hz,
                upper_hz: clip.upper_hz,
                target_hz: clip.target_hz,
                centers_hz: pair,
            });
        }
    }
    ensure!(
        !targets.is_empty(),
        "no narrow clip fits two offset tuning centers"
    );
    let mut plan = plan::targeted(
        &centers.into_iter().collect::<Vec<_>>(),
        dwell_ms,
        max_iq_bytes,
    )?;
    plan.source_triage_sha256 = Some(source.report_sha256);
    plan.source_sweep_sha256 = Some(source.source_sweep_sha256);
    plan.triage_follow_up = Some(FollowUp {
        source_clip_count: source.input_clips,
        eligible_clip_count,
        targets,
    });
    Ok(plan)
}
