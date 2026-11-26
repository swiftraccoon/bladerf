//! Place paired tunings for explicit activity rows from a saved survey.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::sweep::{offset, plan};
use crate::triage::sampling::IntervalEvidence;
use crate::triage::series::{MAX_ACTIVITY_PER_DWELL, activity_source};

use super::input;

const MAX_SELECTIONS: usize = 32;

/// Dwell, repeat count and output extent for the new capture.
pub(in crate::sweep) struct Limits {
    pub(in crate::sweep) dwell_ms: u64,
    pub(in crate::sweep) passes: usize,
    pub(in crate::sweep) max_iq_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    Transient,
    MeanPower,
}

/// One-based indices into a saved series activity report.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(in crate::sweep) struct Selection {
    pub(super) pass: usize,
    pub(super) tune: usize,
    pub(super) kind: Kind,
    pub(super) row: usize,
}

impl Selection {
    /// Parse a bounded `PASS:TUNE:KIND:ROW` index tuple.
    ///
    /// # Errors
    /// Rejects missing values, unknown detector kinds and out-of-range indices.
    pub(in crate::sweep) fn parse(value: &str) -> Result<Self> {
        let fields = value.splitn(4, ':').collect::<Vec<_>>();
        let [pass, tune, kind, row] = fields.as_slice() else {
            bail!("expected PASS:TUNE:KIND:ROW");
        };
        let kind = match *kind {
            "transient" => Kind::Transient,
            "mean-power" => Kind::MeanPower,
            _ => bail!("activity kind must be transient or mean-power"),
        };
        Ok(Self {
            pass: index(pass, "pass", plan::MAX_PASSES)?,
            tune: index(tune, "tune", plan::MAX_TUNES)?,
            kind,
            row: index(row, "row", MAX_ACTIVITY_PER_DWELL)?,
        })
    }
}

fn index(value: &str, name: &str, maximum: usize) -> Result<usize> {
    let index = value
        .parse()
        .with_context(|| format!("invalid activity {name} index"))?;
    ensure!(
        (1..=maximum).contains(&index),
        "activity {name} index must be 1..={maximum}"
    );
    Ok(index)
}

#[derive(Debug, Serialize)]
pub(in crate::sweep) struct FollowUp {
    verified_source_interval_count: usize,
    verified_source_iq_bytes: u64,
    targets: Vec<Target>,
}

#[derive(Debug, Serialize)]
struct Target {
    selection: Selection,
    source_center_hz: u64,
    lower_hz: u64,
    upper_hz: u64,
    start_sample: u64,
    end_sample_exclusive: u64,
    analyzed_interval: IntervalEvidence,
    centers_hz: [u64; 2],
}

impl FollowUp {
    /// Check both actual readbacks cover each complete selected RF band.
    ///
    /// # Errors
    /// Returns an error if either requested visit is missing or cannot admit its band.
    pub(in crate::sweep) fn verify(
        &self,
        observed: impl IntoIterator<Item = (u64, [plan::Band; 2])>,
    ) -> Result<()> {
        let observed: BTreeMap<_, _> = observed.into_iter().collect();
        for target in &self.targets {
            for center in target.centers_hz {
                ensure!(
                    observed.get(&center).is_some_and(|spans| spans
                        .iter()
                        .any(|span| { span.contains_interval(target.lower_hz, target.upper_hz) })),
                    "activity target {:?} lacks actual guarded coverage at {center} Hz",
                    target.selection
                );
            }
        }
        Ok(())
    }
}

/// Verify selected activity evidence and build a sweep covering every selected row.
///
/// # Errors
/// Rejects inconsistent or changed inputs, unsupported RF geometry, duplicate
/// selections, exceeded acquisition limits, and interruption.
pub(in crate::sweep) fn read_plan(
    activity: &Path,
    triage: &Path,
    capture: &Path,
    selections: &[Selection],
    limits: Limits,
    stop: &AtomicBool,
) -> Result<plan::Plan> {
    ensure!(
        (1..=MAX_SELECTIONS).contains(&selections.len()),
        "select 1..={MAX_SELECTIONS} activity targets"
    );
    let mut selections = selections.to_vec();
    selections.sort_unstable();
    ensure!(
        selections.windows(2).all(|pair| pair[0] != pair[1]),
        "duplicate activity target selection"
    );
    let triage = input::read(triage).context("read companion series triage report")?;
    let input::activity::ActivitySource {
        report_sha256,
        targets: evidence,
    } = input::activity::read(activity, &triage, &selections)?;
    let mut centers = BTreeSet::new();
    let mut targets = Vec::with_capacity(selections.len());
    for witness in &evidence {
        let selection = witness.selection;
        let geometry = offset::Band {
            lower_hz: witness.rf.start,
            upper_hz: witness.rf.end,
            target_hz: witness.rf.start.midpoint(witness.rf.end),
            source_low_hz: witness.observed_center_hz,
            source_high_hz: witness.observed_center_hz,
        };
        let pair = offset::place_pair(&geometry, &centers).with_context(|| {
            format!(
                "activity target {selection:?} at {}..{} Hz cannot fit two offset tunings",
                witness.rf.start, witness.rf.end
            )
        })?;
        centers.extend(pair);
        targets.push(Target {
            selection,
            source_center_hz: witness.observed_center_hz,
            lower_hz: witness.rf.start,
            upper_hz: witness.rf.end,
            start_sample: witness.samples.start,
            end_sample_exclusive: witness.samples.end,
            analyzed_interval: IntervalEvidence {
                start_sample: witness.analyzed_samples.start,
                end_sample_exclusive: witness.analyzed_samples.end,
                analyzed_iq_sha256: witness.analyzed_iq_sha256.to_owned(),
            },
            centers_hz: pair,
        });
    }
    let mut plan = plan::targeted(
        &centers.into_iter().collect::<Vec<_>>(),
        limits.dwell_ms,
        limits.max_iq_bytes,
    )?
    .repeat(limits.passes)?;
    let witnesses = evidence
        .into_iter()
        .map(|witness| activity_source::Witness {
            pass: witness.selection.pass,
            tune: witness.selection.tune,
            source_sweep_sha256: witness.source_sweep_sha256,
            source_sidecar_sha256: witness.source_sidecar_sha256,
            requested_center_hz: witness.requested_center_hz,
            observed_center_hz: witness.observed_center_hz,
            saved_samples: witness.saved_samples,
            analyzed_samples: witness.analyzed_samples,
            analyzed_iq_sha256: witness.analyzed_iq_sha256,
        });
    let expected = activity_source::ExpectedSeries {
        sha256: &triage.source_series_sha256,
        operation: triage.source_operation,
        requested_range: triage.requested_range.as_ref(),
        visit_centers_hz: triage.visit_centers_hz.as_deref(),
    };
    let verified = activity_source::verify(capture, expected, witnesses, stop)?;
    plan.source_series_sha256 = Some(triage.source_series_sha256);
    plan.source_series_triage_sha256 = Some(triage.report_sha256);
    plan.source_series_activity_sha256 = Some(report_sha256);
    plan.activity_follow_up = Some(FollowUp {
        verified_source_interval_count: verified.interval_count,
        verified_source_iq_bytes: verified.bytes,
        targets,
    });
    Ok(plan)
}
