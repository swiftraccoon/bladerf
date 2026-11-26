//! Bind a completed follow-up capture to its saved plan and source reports.

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::file_pin;
use crate::sweep::{CompletedOperation, rebuild_series_follow_up_plan};

use super::super::admission::{
    CompletedCapture, PassRef, Series, Sweep, descriptor_path, open_directory, read_bounded,
    reopen_directory, verify_path,
};

const PLAN_FILE: &str = "plan.json";
const MAX_PLAN_BYTES: u64 = 512 * 1024;
const MAX_SOURCE_REPORT_BYTES: u64 = 4 * 1024 * 1024;
const COMPLEX_BYTES: u64 = 4;

#[derive(Deserialize)]
struct Plan {
    schema_version: u32,
    operation: String,
    dwell_ms: u64,
    samples_per_tune: u64,
    max_iq_bytes: u64,
    planned_iq_bytes: u64,
    passes: Option<usize>,
    #[serde(default)]
    check_continuity: bool,
    centers_hz: Vec<u64>,
    source_series_sha256: String,
    source_series_triage_sha256: String,
    source_series_triage_overlap_sha256: Option<String>,
    series_follow_up: FollowUp,
}

#[derive(Deserialize)]
struct FollowUp {
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Target {
    pub(super) source_row: usize,
    pub(super) overlap_source_row: Option<usize>,
    pub(super) requested_center_hz: u64,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) target_hz: u64,
    pub(super) centers_hz: [u64; 2],
}

struct PinnedReport {
    path: PathBuf,
    stamp: Metadata,
    // Keep the admitted inode alive through publication.
    _file: File,
}

impl PinnedReport {
    fn open(path: &Path) -> Result<Self> {
        let (file, _) = file_pin::open_regular(path, MAX_SOURCE_REPORT_BYTES)?;
        Ok(Self {
            path: path.to_path_buf(),
            stamp: file.metadata()?,
            _file: file,
        })
    }

    fn verify(&self) -> Result<()> {
        verify_path(&self.path, &self.stamp)
            .with_context(|| format!("verify source series triage report {}", self.path.display()))
    }
}

/// A validated follow-up root and the identities of its saved sources.
pub(super) struct Input {
    pub(super) source_capture_sha256: String,
    pub(super) source_plan_sha256: String,
    pub(super) source_series_sha256: String,
    pub(super) source_series_triage_sha256: String,
    pub(super) source_series_triage_overlap_sha256: Option<String>,
    pub(super) targets: Vec<Target>,
    capture: CompletedCapture,
    root: PathBuf,
    root_stamp: Metadata,
    plan_stamp: Metadata,
    report: PinnedReport,
    overlap_report: Option<PinnedReport>,
}

impl Input {
    /// Admit the saved plan and all completed passes for this follow-up capture.
    ///
    /// # Errors
    /// Returns an error if a source is unreadable, changed, or inconsistent with
    /// the targeted plan rebuilt from the original and optional overlap reports.
    pub(super) fn open(
        capture_directory: &Path,
        report: &Path,
        overlap_report: Option<&Path>,
    ) -> Result<Self> {
        let root = capture_directory.canonicalize().with_context(|| {
            format!(
                "resolve follow-up capture directory {}",
                capture_directory.display()
            )
        })?;
        let pinned_root = open_directory(&root)?;
        let root_stamp = pinned_root.metadata()?;
        let (plan_bytes, plan_stamp) = read_bounded(
            &descriptor_path(&pinned_root).join(PLAN_FILE),
            MAX_PLAN_BYTES,
        )
        .context("read saved follow-up plan")?;
        let plan: Plan =
            serde_json::from_slice(&plan_bytes).context("parse saved follow-up plan")?;
        ensure!(
            plan.schema_version == 1 && plan.operation == "targeted_rf_sweep_plan",
            "unsupported follow-up sweep plan"
        );
        let saved: Value =
            serde_json::from_slice(&plan_bytes).context("read follow-up plan geometry")?;
        let source_plan_sha256 = format!("{:x}", Sha256::digest(plan_bytes));
        let pass_count = plan.passes.unwrap_or(1);

        match (
            plan.source_series_triage_overlap_sha256.is_some(),
            overlap_report.is_some(),
        ) {
            (true, false) => bail!("saved follow-up plan requires --overlap-series-triage-report"),
            (false, true) => {
                bail!("saved follow-up plan did not use an overlap series triage report")
            }
            _ => {}
        }
        let pinned_report = PinnedReport::open(report)?;
        let pinned_overlap_report = overlap_report.map(PinnedReport::open).transpose()?;
        let rebuilt = rebuild_series_follow_up_plan(
            report,
            overlap_report,
            plan.dwell_ms,
            plan.max_iq_bytes,
            pass_count,
            plan.check_continuity,
        )
        .context("rebuild follow-up plan from source series triage report")?;
        ensure!(
            saved == rebuilt,
            "saved follow-up plan differs from the rebuilt plan"
        );

        let capture = if pass_count == 1 {
            CompletedCapture::Single(Sweep::open(&root)?)
        } else {
            let series = Series::open(&root)?;
            ensure!(
                series.series_source.matches(
                    &plan.source_series_sha256,
                    &plan.source_series_triage_sha256,
                    plan.source_series_triage_overlap_sha256.as_deref(),
                ),
                "completed follow-up series source digests differ from the plan"
            );
            CompletedCapture::Repeated(series)
        };
        ensure!(
            capture.continuity_checked() == plan.check_continuity,
            "completed follow-up continuity policy differs from the plan"
        );
        let input = Self {
            source_capture_sha256: capture.source_receipt_sha256().to_owned(),
            source_plan_sha256,
            source_series_sha256: plan.source_series_sha256,
            source_series_triage_sha256: plan.source_series_triage_sha256,
            source_series_triage_overlap_sha256: plan.source_series_triage_overlap_sha256,
            targets: plan.series_follow_up.targets,
            capture,
            root,
            root_stamp,
            plan_stamp,
            report: pinned_report,
            overlap_report: pinned_overlap_report,
        };
        input.verify_capture(
            pass_count,
            &plan.centers_hz,
            plan.samples_per_tune,
            plan.planned_iq_bytes,
        )?;
        input.verify()?;
        Ok(input)
    }

    pub(super) fn passes(&self) -> Vec<PassRef<'_>> {
        self.capture.passes()
    }

    fn verify_capture(
        &self,
        count: usize,
        centers: &[u64],
        samples: u64,
        bytes: u64,
    ) -> Result<()> {
        let passes = self.passes();
        ensure!(
            passes.len() == count,
            "completed follow-up pass count differs from the plan"
        );
        let mut saved_samples = 0;
        for pass in passes {
            let sweep = pass.sweep;
            ensure!(
                sweep.operation == CompletedOperation::Targeted
                    && sweep
                        .tunes
                        .iter()
                        .map(|tune| (tune.requested_center_hz, tune.samples))
                        .eq(centers.iter().map(|&center| (center, samples))),
                "completed follow-up sweep targets or dwell extent differ from the plan"
            );
            ensure!(
                sweep.series_source.matches(
                    &self.source_series_sha256,
                    &self.source_series_triage_sha256,
                    self.source_series_triage_overlap_sha256.as_deref(),
                ),
                "completed follow-up sweep source digests differ from the plan"
            );
            saved_samples += sweep.tunes.iter().map(|tune| tune.samples).sum::<u64>();
        }
        ensure!(
            saved_samples * COMPLEX_BYTES == bytes,
            "completed follow-up IQ bytes differ from the plan"
        );
        Ok(())
    }

    /// Recheck the admitted files before publishing derived evidence.
    ///
    /// # Errors
    /// Returns an error if a directory or source file no longer has its admitted identity.
    pub(super) fn verify(&self) -> Result<()> {
        let root = reopen_directory(&self.root, &self.root_stamp)?;
        verify_path(&descriptor_path(&root).join(PLAN_FILE), &self.plan_stamp)
            .context("verify saved follow-up plan")?;
        self.report.verify()?;
        if let Some(overlap_report) = &self.overlap_report {
            overlap_report.verify()?;
        }
        self.capture.verify_pinned(&root)
    }
}
