//! Finite receive-only RX1 capture and closed-file analysis cycles.
//!
//! Each acquisition child is fully shut down and reaped before the accepted IQ
//! file enters the offline analyzer. Retention applies only to sessions whose
//! compact outcome and terminal manifest digest have already been synced into
//! the run journal and atomic snapshot.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::input::{LiveExpectation, SourceReport};
use crate::orchestrate::{self, ProgramPaths, ProgramSet};
use crate::process::{self, CommandReport, LogReport, ProgramProvenance};
use crate::report::{self, OverallState};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_CAPTURE_SECONDS: u64 = 2;
const MAX_CAPTURE_SAMPLES: u64 = SAMPLE_RATE_HZ * MAX_CAPTURE_SECONDS;
const DEFAULT_ANALYSIS_SAMPLE_LIMIT: u64 = 20_000_000;
pub(crate) const MAX_DISCARD_INITIAL_SAMPLES: u64 = SAMPLE_RATE_HZ;
const RX_CHANNEL: &str = "RX1";
const STOCK_FPGA_VERSION: &str = "0.16.0";
const MAX_TREE_ENTRIES: u64 = 100_000;
const MAX_TREE_DEPTH: usize = 32;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CAPTURE_LOG_SCAN_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NESTED_DECODER_BYTES: u64 = 512 * 1024 * 1024;
const WIFI_PENDING_FRAME_LIMIT: u64 = 64;
const WIFI_BUFFER_SAMPLE_LIMIT: u64 = 1_048_576;
const BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY: u64 = 4096;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

#[cfg(test)]
const DEFAULT_TEST_CENTER_HZ: u64 = 2_437_000_000;

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) output: PathBuf,
    pub(crate) cycles: u16,
    pub(crate) center_mhz: u32,
    pub(crate) alternate_center_mhz: Option<u32>,
    pub(crate) duration_seconds: f64,
    pub(crate) discard_initial_samples: u64,
    pub(crate) gain_db: f64,
    pub(crate) max_consecutive_failures: u16,
    pub(crate) keep_sessions: u16,
    pub(crate) retained_limit_mib: u32,
    pub(crate) pending_captures: u8,
    pub(crate) bluetooth_coded_shadow: bool,
    pub(crate) capture_timeout_seconds: u64,
    pub(crate) spectrum_windows: usize,
    pub(crate) decoder_timeout_seconds: u64,
    pub(crate) child_timeout_seconds: u64,
    pub(crate) log_limit_mib: u64,
    pub(crate) receiver: PathBuf,
    pub(crate) programs: ProgramPaths,
}

mod acquisition;
mod config;
mod metrics;
mod model;
mod pipeline;
mod retention;
mod sequential;
mod state;
pub(crate) mod storage;
mod worker;

pub(crate) use acquisition::read_complete_capture_log;
#[cfg(test)]
use acquisition::{capture_arguments, capture_diagnostics, live_expectation};
use acquisition::{capture_once, enforce_capture_center};
use config::{analysis_request, prepare, verify_programs};
use metrics::analyze_once;
#[cfg(test)]
use metrics::{
    attach_protocol_metrics, bluetooth_duplicate_accounting, bluetooth_metrics, compact_analysis,
    ieee_membership_metrics, protocol_metrics, wifi_metrics,
};
use model::{
    AnalysisStep, Attempt, AttemptPhase, BluetoothCodedShadow, BluetoothCodedShadowMetrics,
    BluetoothDuplicateAccounting, BluetoothMetrics, BurstFamilyAccounting, BurstFamilyCount,
    BurstFamilyMetrics, BurstFamilySettings, BurstFeatureSummaries, BurstFeatureSummary,
    BurstFrequencyBin, CaptureDiagnostics, CaptureStep, CompactAnalysis, CompactProgram,
    CompactSource, CompactStage, IeeeChannelMetrics, IeeeHypothesisMetrics,
    IeeeMembershipPartitionMetrics, IeeeMetrics, NestedDecoderProgram, Plan, ProtocolMetrics,
    RetentionPriority, RetentionPruneReason, RetentionReason, RfMetrics, RfRegionAggregate,
    StepState, Termination, WifiDecoderHealth, WifiMetrics, WifiOfdmDecoderHealth,
    WifiPhyDecoderHealth,
};
#[cfg(test)]
use pipeline::overlap_capture_admissible;
use pipeline::run_pipelined_plan;
use retention::{
    prune_after_pipeline_attempt, prune_one, reserve_capture_capacity, retention_reasons,
};
use sequential::run_plan;
use state::{RunState, journal, save_run, unix_ns};
use storage::{
    TreeBudget, inspect_tree, private_file, remove_quarantined_tree, sync_directory, tree_bytes,
};
use worker::{
    AnalysisCompletion, AnalysisJob, AnalysisWorkerEvent, analysis_worker, failed_analysis,
};

#[derive(Debug)]
pub(crate) struct RunResult {
    pub(crate) output: PathBuf,
    pub(crate) successful: bool,
}
pub(crate) fn watch(request: Request, stop: &AtomicBool) -> Result<RunResult> {
    let prepared = prepare(&request)?;
    let output = report::fresh_output(&request.output)?;
    let successful = if prepared.plan.pending_captures == 0 {
        let receiver_for_verify = prepared.receiver.clone();
        let analysis_for_verify = prepared.analysis_programs.clone();
        let request_for_verify = request.clone();
        let receiver_for_capture = prepared.receiver.clone();
        let plan_for_capture = prepared.plan.clone();
        let request_for_analysis = request.clone();
        let analysis_for_run = prepared.analysis_programs.clone();
        run_plan(
            &output,
            &prepared.plan,
            stop,
            move || {
                verify_programs(
                    &request_for_verify,
                    &receiver_for_verify,
                    &analysis_for_verify,
                )
            },
            move |center_frequency_hz, session, flag| {
                capture_once(
                    &receiver_for_capture,
                    &plan_for_capture,
                    center_frequency_hz,
                    session,
                    flag,
                )
            },
            move |source, session, flag| {
                analyze_once(
                    &request_for_analysis,
                    &analysis_for_run,
                    source,
                    session,
                    flag,
                )
            },
        )?
    } else {
        let coordinator_receiver = prepared.receiver.clone();
        let coordinator_programs = prepared.analysis_programs.clone();
        let coordinator_request = request.clone();
        let worker_receiver = prepared.receiver.clone();
        let worker_programs = prepared.analysis_programs.clone();
        let worker_request = request.clone();
        let receiver_for_capture = prepared.receiver.clone();
        let plan_for_capture = prepared.plan.clone();
        let request_for_analysis = request.clone();
        let analysis_for_run = prepared.analysis_programs.clone();
        run_pipelined_plan(
            &output,
            &prepared.plan,
            stop,
            move || {
                verify_programs(
                    &coordinator_request,
                    &coordinator_receiver,
                    &coordinator_programs,
                )
            },
            move || verify_programs(&worker_request, &worker_receiver, &worker_programs),
            move |center_frequency_hz, session, flag| {
                capture_once(
                    &receiver_for_capture,
                    &plan_for_capture,
                    center_frequency_hz,
                    session,
                    flag,
                )
            },
            move |source, session, flag| {
                analyze_once(
                    &request_for_analysis,
                    &analysis_for_run,
                    source,
                    session,
                    flag,
                )
            },
        )?
    };
    Ok(RunResult { output, successful })
}

#[cfg(test)]
mod tests;
