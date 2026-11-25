//! Bounded host-only comparison of mixed-center recurrence inventories.
//!
//! Comparisons preserve each input's support and opportunity counts. They do
//! not establish temporal, environmental, cross-center, or protocol identity.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use self::input::{ParsedStratum, parse_inventory};
use crate::{orchestrate, report};

mod input;

const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const OUTPUT_NAME: &str = "comparison.json";
const OPERATION: &str = "compare_burst_family_recurrence";
const CENTER_SELECTION: &str = "round_robin_by_attempt_index";

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) baseline: PathBuf,
    pub(crate) current: PathBuf,
    pub(crate) output: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ComparisonResult {
    pub(crate) output: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RowState {
    support_captures: u64,
    eligible_captures: u64,
    recurring: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Classification {
    RecurringInBoth,
    CurrentRecurringOnly,
    BaselineRecurringOnly,
}

#[derive(Debug, Serialize)]
struct ComparisonRow {
    candidate_family: &'static str,
    frequency_mhz: u64,
    classification: Classification,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline: Option<RowState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<RowState>,
}

#[derive(Debug, Default, Serialize)]
struct TransitionAccounting {
    recurring_in_both: u64,
    current_recurring_only: u64,
    baseline_recurring_only: u64,
}

impl TransitionAccounting {
    fn record(&mut self, classification: Classification) -> Result<()> {
        let count = match classification {
            Classification::RecurringInBoth => &mut self.recurring_in_both,
            Classification::CurrentRecurringOnly => &mut self.current_recurring_only,
            Classification::BaselineRecurringOnly => &mut self.baseline_recurring_only,
        };
        *count = count
            .checked_add(1)
            .context("comparison row count overflow")?;
        Ok(())
    }

    fn add(&mut self, other: &Self) -> Result<()> {
        self.recurring_in_both = self
            .recurring_in_both
            .checked_add(other.recurring_in_both)
            .context("comparison shared-row total overflow")?;
        self.current_recurring_only = self
            .current_recurring_only
            .checked_add(other.current_recurring_only)
            .context("comparison current-only total overflow")?;
        self.baseline_recurring_only = self
            .baseline_recurring_only
            .checked_add(other.baseline_recurring_only)
            .context("comparison baseline-only total overflow")?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct CenterComparison {
    center_frequency_hz: u64,
    nominal_half_open_frequency_band_hz: [u64; 2],
    baseline_eligible_captures: u64,
    current_eligible_captures: u64,
    baseline_minimum_support_captures: u64,
    current_minimum_support_captures: u64,
    baseline_observed_rows: u64,
    current_observed_rows: u64,
    baseline_recurring_rows: u64,
    current_recurring_rows: u64,
    accounting: TransitionAccounting,
    rows: Vec<ComparisonRow>,
}

#[derive(Debug, Serialize)]
struct InputIdentity {
    recurrence_schema_version: u32,
    recurrence_sha256: String,
    source_run_sha256: String,
    watch_schema_version: u64,
}

#[derive(Debug, Serialize)]
struct ComparisonReport {
    schema_version: u32,
    operation: &'static str,
    baseline: InputIdentity,
    current: InputIdentity,
    center_frequency_schedule_hz: [u64; 2],
    center_selection: &'static str,
    family_order: [&'static str; 5],
    center_strata: Vec<CenterComparison>,
    accounting: TransitionAccounting,
    support_counts_combined: bool,
    cross_run_support_aggregation_performed: bool,
    cross_center_comparison_performed: bool,
    cross_run_support_comparability_established: bool,
    temporal_comparability_established: bool,
    environmental_comparability_established: bool,
    occupancy_comparability_established: bool,
    protocol_identity_established: bool,
    host_only: bool,
    radio_opened: bool,
    transmit_requested: bool,
    interpretation: &'static str,
}

fn compare_stratum(baseline: &ParsedStratum, current: &ParsedStratum) -> Result<CenterComparison> {
    ensure!(
        baseline.center_frequency_hz == current.center_frequency_hz
            && baseline.nominal_band_hz == current.nominal_band_hz,
        "recurrence center strata are not compatible"
    );
    let keys: BTreeSet<_> = baseline
        .rows
        .keys()
        .chain(current.rows.keys())
        .copied()
        .collect();
    let mut accounting = TransitionAccounting::default();
    let mut rows = Vec::new();
    for (family_index, frequency_mhz) in keys {
        let baseline_state = baseline.rows.get(&(family_index, frequency_mhz));
        let current_state = current.rows.get(&(family_index, frequency_mhz));
        let classification = match (
            baseline_state.is_some_and(|state| state.recurring),
            current_state.is_some_and(|state| state.recurring),
        ) {
            (true, true) => Classification::RecurringInBoth,
            (false, true) => Classification::CurrentRecurringOnly,
            (true, false) => Classification::BaselineRecurringOnly,
            (false, false) => continue,
        };
        accounting.record(classification)?;
        rows.push(ComparisonRow {
            candidate_family: orchestrate::BURST_FAMILY_LABELS[family_index],
            frequency_mhz,
            classification,
            baseline: baseline_state.cloned(),
            current: current_state.cloned(),
        });
    }
    Ok(CenterComparison {
        center_frequency_hz: baseline.center_frequency_hz,
        nominal_half_open_frequency_band_hz: baseline.nominal_band_hz,
        baseline_eligible_captures: baseline.eligible_captures,
        current_eligible_captures: current.eligible_captures,
        baseline_minimum_support_captures: baseline.minimum_support_captures,
        current_minimum_support_captures: current.minimum_support_captures,
        baseline_observed_rows: baseline.observed_rows,
        current_observed_rows: current.observed_rows,
        baseline_recurring_rows: baseline.recurring_rows,
        current_recurring_rows: current.recurring_rows,
        accounting,
        rows,
    })
}

fn publish(output: &Path, comparison: &ComparisonReport, stop: &AtomicBool) -> Result<()> {
    let mut encoded = serde_json::to_vec_pretty(comparison)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() as u64 <= MAX_REPORT_BYTES,
        "recurrence comparison exceeds {MAX_REPORT_BYTES} bytes"
    );
    let partial = output.join(".comparison.json.partial");
    let final_path = output.join(OUTPUT_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("create {}", partial.display()))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    if stop.load(Ordering::Relaxed) {
        drop(file);
        fs::remove_file(&partial)?;
        bail!("recurrence comparison interrupted before publication");
    }
    if let Err(error) = fs::hard_link(&partial, &final_path) {
        let _ = fs::remove_file(&partial);
        return Err(error).with_context(|| {
            format!(
                "publish {} without replacing an existing file",
                final_path.display()
            )
        });
    }
    fs::remove_file(&partial)?;
    File::open(output)?.sync_all()?;
    Ok(())
}

pub(crate) fn compare(request: Request, stop: &AtomicBool) -> Result<ComparisonResult> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence comparison interrupted before input validation"
    );
    let baseline = parse_inventory(&request.baseline).context("validate baseline recurrence")?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence comparison interrupted between inputs"
    );
    let current = parse_inventory(&request.current).context("validate current recurrence")?;
    ensure!(
        baseline.schedule == current.schedule
            && baseline.center_selection == current.center_selection,
        "recurrence inventories use incompatible center schedules"
    );
    let mut center_strata = Vec::with_capacity(2);
    let mut accounting = TransitionAccounting::default();
    for slot in 0..2 {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "recurrence comparison interrupted while comparing strata"
        );
        let stratum = compare_stratum(&baseline.strata[slot], &current.strata[slot])?;
        accounting.add(&stratum.accounting)?;
        center_strata.push(stratum);
    }
    let comparison = ComparisonReport {
        schema_version: 1,
        operation: OPERATION,
        baseline: InputIdentity {
            recurrence_schema_version: 3,
            recurrence_sha256: baseline.recurrence_sha256,
            source_run_sha256: baseline.source_run_sha256,
            watch_schema_version: baseline.watch_schema_version,
        },
        current: InputIdentity {
            recurrence_schema_version: 3,
            recurrence_sha256: current.recurrence_sha256,
            source_run_sha256: current.source_run_sha256,
            watch_schema_version: current.watch_schema_version,
        },
        center_frequency_schedule_hz: baseline.schedule,
        center_selection: CENTER_SELECTION,
        family_order: orchestrate::BURST_FAMILY_LABELS,
        center_strata,
        accounting,
        support_counts_combined: false,
        cross_run_support_aggregation_performed: false,
        cross_center_comparison_performed: false,
        cross_run_support_comparability_established: false,
        temporal_comparability_established: false,
        environmental_comparability_established: false,
        occupancy_comparability_established: false,
        protocol_identity_established: false,
        host_only: true,
        radio_opened: false,
        transmit_requested: false,
        interpretation: "Rows compare exact family/frequency recurrence classifications within the same configured center. Baseline and current support remain separate, and role-only recurrence does not prove signal appearance, disappearance, occupancy, transmitter identity, protocol identity, temporal or environmental comparability, or calibrated response.",
    };
    ensure!(
        !stop.load(Ordering::Relaxed),
        "recurrence comparison interrupted before output creation"
    );
    let output = report::fresh_output(&request.output)?;
    publish(&output, &comparison, stop)?;
    Ok(ComparisonResult { output })
}

#[cfg(test)]
#[path = "recurrence_compare/tests.rs"]
mod tests;
