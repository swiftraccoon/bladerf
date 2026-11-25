//! Bounded concurrent file-only orchestration and evidence classification.

use std::ffi::OsString;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::input::SourceReport;
use crate::process::{self, CommandReport, ProgramProvenance};
use crate::report::{
    self, AnalysisReport, Outcome, OverallState, Settings, StageReport, StageState,
};

mod bluetooth_shadow;
pub(crate) mod ieee_nominal;

pub(crate) use bluetooth_shadow::run_bluetooth_coded_shadow;

const IEEE_CHANNELS: [u8; 3] = [16, 17, 18];
const IEEE_DECODER_LANES: [&str; 2] = ["hard", "soft"];
pub(crate) const IEEE_HYPOTHESIS_COUNT: usize = 6;
const IEEE_MAX_FRAMES: u64 = 1_000_000;
const IEEE_ARCHIVE_MIB: u64 = 64;
const INSPECTION_OUTPUT_MIB: u64 = 512;
pub(crate) const IEEE_ARCHIVE_MAX_BYTES: u64 = IEEE_ARCHIVE_MIB * 1024 * 1024;
const BURST_FAMILY_MAX_INPUT_MIB: u64 = 128;
const BURST_FAMILY_MAX_SAMPLES: u64 = 50_000_000;
const BURST_FAMILY_MAX_RECORDS: u64 = 100_000;
const BURST_FAMILY_MAX_GROUPS: u64 = 100_000;
const BURST_FAMILY_MAX_OUTPUT_MIB: u64 = 16;
pub(crate) const BURST_FAMILY_MAX_INPUT_BYTES: u64 = BURST_FAMILY_MAX_INPUT_MIB * 1024 * 1024;
pub(crate) const BURST_FAMILY_LABELS: [&str; 5] = [
    "wideband_multibin_candidate",
    "adjacent_multibin_candidate",
    "narrowband_binary_fsk_like_candidate",
    "narrowband_constant_envelope_unresolved_candidate",
    "unclassified_burst_group",
];

type StageRunner = fn(&Programs, &SourceReport, &Path, &Settings, &AtomicBool) -> StageReport;

const STAGE_NAMES: [&str; 5] = [
    "wifi",
    "rf_survey",
    "ieee802154",
    "bluetooth",
    "burst_families",
];

enum BranchResult<T> {
    Complete(T),
    SpawnFailed,
    Panicked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IeeeIntegrityPhase {
    InputHash,
    Terminal,
}

impl IeeeIntegrityPhase {
    fn thread_name(self, channel: u8) -> String {
        match self {
            Self::InputHash => format!("ieee802154-hash-{channel}"),
            Self::Terminal => format!("ieee802154-terminal-{channel}"),
        }
    }

    fn failure_label(self) -> &'static str {
        match self {
            Self::InputHash => "input-hash",
            Self::Terminal => "terminal-integrity",
        }
    }
}

#[derive(Debug)]
pub(crate) struct IeeeChannelInput {
    pub(crate) channel: u8,
    pub(crate) digest: report::FileDigest,
    path: PathBuf,
    snapshot: fs::Metadata,
}

impl IeeeChannelInput {
    fn verify_unchanged(&self) -> Result<()> {
        let current = fs::symlink_metadata(&self.path)
            .with_context(|| format!("inspect IEEE 802.15.4 channel {} input", self.channel))?;
        ensure!(
            current.is_file() && report::same_file_identity(&self.snapshot, &current),
            "IEEE 802.15.4 channel {} input changed after hashing",
            self.channel
        );
        Ok(())
    }
}

#[derive(Debug)]
struct PendingIeeeChannelInput {
    channel: u8,
    path: PathBuf,
    bytes: u64,
    expected_sha256: Option<String>,
}

#[derive(Debug)]
pub(crate) struct IeeeChannelization {
    pub(crate) worker_input_mib: u64,
    pub(crate) channels: Vec<IeeeChannelInput>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IeeeHypothesisSpec {
    pub(crate) source_index: usize,
    pub(crate) decoder_lane: &'static str,
    pub(crate) carrier_offset_hz: i32,
    pub(crate) source_role: &'static str,
}

impl IeeeHypothesisSpec {
    pub(crate) fn archive_name(self, channel: u8) -> String {
        format!("channel-{channel}-{}.i154raw", self.source_role)
    }

    pub(crate) fn inspection_name(self, channel: u8) -> String {
        format!("channel-{channel}-{}-inspection", self.source_role)
    }
}

pub(crate) const IEEE_HYPOTHESES: [IeeeHypothesisSpec; IEEE_HYPOTHESIS_COUNT] = [
    IeeeHypothesisSpec {
        source_index: 0,
        decoder_lane: "hard",
        carrier_offset_hz: 0,
        source_role: "hard",
    },
    IeeeHypothesisSpec {
        source_index: 1,
        decoder_lane: "soft",
        carrier_offset_hz: 0,
        source_role: "soft",
    },
    IeeeHypothesisSpec {
        source_index: 2,
        decoder_lane: "hard",
        carrier_offset_hz: -100_000,
        source_role: "hard-cfo-neg100000",
    },
    IeeeHypothesisSpec {
        source_index: 3,
        decoder_lane: "soft",
        carrier_offset_hz: -100_000,
        source_role: "soft-cfo-neg100000",
    },
    IeeeHypothesisSpec {
        source_index: 4,
        decoder_lane: "hard",
        carrier_offset_hz: 100_000,
        source_role: "hard-cfo-pos100000",
    },
    IeeeHypothesisSpec {
        source_index: 5,
        decoder_lane: "soft",
        carrier_offset_hz: 100_000,
        source_role: "soft-cfo-pos100000",
    },
];

#[derive(Clone, Debug)]
struct IeeeHypothesisEvidence {
    spec: IeeeHypothesisSpec,
    archive: report::FileDigest,
    inspection: Value,
    inspection_sha256: String,
    source_index: Option<ieee_nominal::SourceIndexEvidence>,
}

#[derive(Clone, Debug)]
struct IeeeChannelEvidence {
    channel: u8,
    sources: Vec<IeeeHypothesisEvidence>,
    comparison: Value,
    comparison_sha256: String,
}

#[derive(Debug)]
struct IeeeChannelRun {
    channel: u8,
    commands: Vec<CommandReport>,
    evidence: Option<IeeeChannelEvidence>,
    errors: Vec<String>,
    interrupted: bool,
}

impl IeeeChannelRun {
    fn failed(channel: u8, detail: impl Into<String>) -> Self {
        Self {
            channel,
            commands: Vec::new(),
            evidence: None,
            errors: vec![detail.into()],
            interrupted: false,
        }
    }

    fn interrupted(channel: u8) -> Self {
        Self {
            channel,
            commands: Vec::new(),
            evidence: None,
            errors: vec![format!(
                "interrupted before IEEE 802.15.4 channel {channel}"
            )],
            interrupted: true,
        }
    }
}

impl IeeeChannelization {
    pub(crate) fn channel(&self, channel: u8) -> Result<&IeeeChannelInput> {
        self.channels
            .iter()
            .find(|input| input.channel == channel)
            .with_context(|| format!("missing validated IEEE 802.15.4 channel {channel} input"))
    }

    fn verify_unchanged(&self) -> Result<()> {
        self.channels
            .iter()
            .try_for_each(IeeeChannelInput::verify_unchanged)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ProgramPaths {
    pub(crate) wifi: PathBuf,
    pub(crate) rf: PathBuf,
    pub(crate) ieee: PathBuf,
    pub(crate) ieee_worker: PathBuf,
    pub(crate) bluetooth: PathBuf,
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) input: PathBuf,
    pub(crate) output: PathBuf,
    pub(crate) center_mhz: u32,
    pub(crate) sample_rate_hz: u64,
    pub(crate) max_samples: u64,
    pub(crate) spectrum_windows: usize,
    pub(crate) decoder_timeout_seconds: u64,
    pub(crate) child_timeout_seconds: u64,
    pub(crate) log_limit_mib: u64,
    pub(crate) programs: ProgramPaths,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Programs {
    orchestrator: ProgramProvenance,
    wifi: ProgramProvenance,
    rf: ProgramProvenance,
    ieee: ProgramProvenance,
    ieee_worker: ProgramProvenance,
    bluetooth: ProgramProvenance,
}

impl Programs {
    fn resolve(paths: &ProgramPaths) -> Result<Self> {
        Ok(Self {
            orchestrator: process::resolve_program(
                "digital-monitor",
                &std::env::current_exe().context("resolve running executable path")?,
            )?,
            wifi: process::resolve_program("wifi-monitor", &paths.wifi)?,
            rf: process::resolve_program("rf-observatory", &paths.rf)?,
            ieee: process::resolve_program("ieee802154-monitor", &paths.ieee)?,
            ieee_worker: process::resolve_program(
                "ieee802154-futuresdr-worker",
                &paths.ieee_worker,
            )?,
            bluetooth: process::resolve_program("bluetooth-monitor", &paths.bluetooth)?,
        })
    }

    fn reports(&self) -> Vec<ProgramProvenance> {
        vec![
            self.orchestrator.clone(),
            self.wifi.clone(),
            self.rf.clone(),
            self.ieee.clone(),
            self.ieee_worker.clone(),
            self.bluetooth.clone(),
        ]
    }
}

fn text(value: impl ToString) -> OsString {
    OsString::from(value.to_string())
}

fn path(value: &Path) -> OsString {
    value.as_os_str().to_owned()
}

fn failed(
    name: &'static str,
    state: StageState,
    commands: Vec<CommandReport>,
    error: impl std::fmt::Display,
) -> StageReport {
    StageReport {
        name,
        state,
        outcome: None,
        commands,
        detail: Some(error.to_string()),
    }
}

fn command_state(command: &CommandReport) -> StageState {
    if command.interrupted {
        StageState::Interrupted
    } else {
        StageState::Failed
    }
}

fn completed(name: &'static str, outcome: Outcome, commands: Vec<CommandReport>) -> StageReport {
    StageReport {
        name,
        state: StageState::Complete,
        outcome: Some(outcome),
        commands,
        detail: None,
    }
}

fn run_one(
    program: &ProgramProvenance,
    arguments: Vec<OsString>,
    output: &Path,
    log_stem: &str,
    settings: &Settings,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    process::run(
        program,
        &arguments,
        output,
        log_stem,
        settings.retained_log_limit_bytes_per_stream,
        Duration::from_secs(settings.child_timeout_seconds),
        stop,
    )
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiManagementFrameTypes {
    pub(crate) association_request: u64,
    pub(crate) association_response: u64,
    pub(crate) reassociation_request: u64,
    pub(crate) reassociation_response: u64,
    pub(crate) probe_request: u64,
    pub(crate) probe_response: u64,
    pub(crate) beacon: u64,
    pub(crate) atim: u64,
    pub(crate) disassociation: u64,
    pub(crate) authentication: u64,
    pub(crate) deauthentication: u64,
    pub(crate) action: u64,
    pub(crate) action_no_ack: u64,
    pub(crate) reserved_management: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiOfdmRejectionMetrics {
    pub(crate) signal_or_probe_rejected: u64,
    pub(crate) greenfield_header_unsupported: u64,
    pub(crate) greenfield_probe_unrecognized: u64,
    pub(crate) mixed_header_unsupported: u64,
    pub(crate) legacy_fallback_unreservable: u64,
    pub(crate) data_decode_failed: u64,
    pub(crate) pending_capacity_rejected: u64,
    pub(crate) strong_long_training_not_found: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiOfdmDataFailureMetrics {
    pub(crate) legacy: u64,
    pub(crate) ht_bcc: u64,
    pub(crate) ht_ldpc: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiOfdmStrongLongTrainingMetrics {
    pub(crate) strong_short_periodicity_timeout: u64,
    pub(crate) strong_long_repeat_not_found: u64,
    pub(crate) strong_long_reference_not_found: u64,
    pub(crate) strong_long_coordinates_unavailable: u64,
}

pub(crate) fn wifi_ofdm_rejection_metrics(decode: &Value) -> Result<WifiOfdmRejectionMetrics> {
    const FIELDS: [&str; 8] = [
        "signal_or_probe_rejected",
        "greenfield_header_unsupported",
        "greenfield_probe_unrecognized",
        "mixed_header_unsupported",
        "legacy_fallback_unreservable",
        "data_decode_failed",
        "pending_capacity_rejected",
        "strong_long_training_not_found",
    ];
    let ofdm = decode
        .get("ofdm")
        .context("Wi-Fi IQ-decode summary lacks OFDM counters")?;
    let rejections = ofdm
        .get("rejections")
        .context("Wi-Fi IQ-decode summary lacks OFDM rejection accounting")?;
    ensure!(
        rejections.as_object().is_some_and(|object| {
            object.len() == FIELDS.len() && FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi OFDM rejection accounting fields differ"
    );
    let metrics = WifiOfdmRejectionMetrics {
        signal_or_probe_rejected: report::required_u64(rejections, "signal_or_probe_rejected")?,
        greenfield_header_unsupported: report::required_u64(
            rejections,
            "greenfield_header_unsupported",
        )?,
        greenfield_probe_unrecognized: report::required_u64(
            rejections,
            "greenfield_probe_unrecognized",
        )?,
        mixed_header_unsupported: report::required_u64(rejections, "mixed_header_unsupported")?,
        legacy_fallback_unreservable: report::required_u64(
            rejections,
            "legacy_fallback_unreservable",
        )?,
        data_decode_failed: report::required_u64(rejections, "data_decode_failed")?,
        pending_capacity_rejected: report::required_u64(rejections, "pending_capacity_rejected")?,
        strong_long_training_not_found: report::required_u64(
            rejections,
            "strong_long_training_not_found",
        )?,
    };
    let partition_count = [
        metrics.signal_or_probe_rejected,
        metrics.greenfield_header_unsupported,
        metrics.greenfield_probe_unrecognized,
        metrics.mixed_header_unsupported,
        metrics.legacy_fallback_unreservable,
        metrics.data_decode_failed,
        metrics.pending_capacity_rejected,
        metrics.strong_long_training_not_found,
    ]
    .into_iter()
    .try_fold(0u64, u64::checked_add)
    .context("Wi-Fi OFDM rejection partition overflow")?;
    ensure!(
        partition_count == report::required_u64(ofdm, "rejected_frames")?,
        "Wi-Fi OFDM rejection partition differs from rejected frames"
    );
    Ok(metrics)
}

pub(crate) fn wifi_ofdm_data_failure_metrics(
    decode: &Value,
    rejections: &WifiOfdmRejectionMetrics,
) -> Result<WifiOfdmDataFailureMetrics> {
    const FIELDS: [&str; 3] = ["legacy", "ht_bcc", "ht_ldpc"];
    let details = decode
        .get("ofdm")
        .context("Wi-Fi IQ-decode summary lacks OFDM counters")?
        .get("data_failures")
        .context("Wi-Fi IQ-decode summary lacks OFDM data-failure detail")?;
    ensure!(
        details.as_object().is_some_and(|object| {
            object.len() == FIELDS.len() && FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi OFDM data-failure detail fields differ"
    );
    let metrics = WifiOfdmDataFailureMetrics {
        legacy: report::required_u64(details, "legacy")?,
        ht_bcc: report::required_u64(details, "ht_bcc")?,
        ht_ldpc: report::required_u64(details, "ht_ldpc")?,
    };
    let detail_count = [metrics.legacy, metrics.ht_bcc, metrics.ht_ldpc]
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .context("Wi-Fi OFDM data-failure detail overflow")?;
    ensure!(
        detail_count == rejections.data_decode_failed,
        "Wi-Fi OFDM data-failure detail differs from data-decode failures"
    );
    Ok(metrics)
}

pub(crate) fn wifi_ofdm_strong_long_training_metrics(
    decode: &Value,
    rejections: &WifiOfdmRejectionMetrics,
) -> Result<WifiOfdmStrongLongTrainingMetrics> {
    const FIELDS: [&str; 4] = [
        "strong_short_periodicity_timeout",
        "strong_long_repeat_not_found",
        "strong_long_reference_not_found",
        "strong_long_coordinates_unavailable",
    ];
    let details = decode
        .get("ofdm")
        .context("Wi-Fi IQ-decode summary lacks OFDM counters")?
        .get("strong_long_training")
        .context("Wi-Fi IQ-decode summary lacks strong long-training detail")?;
    ensure!(
        details.as_object().is_some_and(|object| {
            object.len() == FIELDS.len() && FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi strong long-training detail fields differ"
    );
    let metrics = WifiOfdmStrongLongTrainingMetrics {
        strong_short_periodicity_timeout: report::required_u64(
            details,
            "strong_short_periodicity_timeout",
        )?,
        strong_long_repeat_not_found: report::required_u64(
            details,
            "strong_long_repeat_not_found",
        )?,
        strong_long_reference_not_found: report::required_u64(
            details,
            "strong_long_reference_not_found",
        )?,
        strong_long_coordinates_unavailable: report::required_u64(
            details,
            "strong_long_coordinates_unavailable",
        )?,
    };
    let detail_count = [
        metrics.strong_short_periodicity_timeout,
        metrics.strong_long_repeat_not_found,
        metrics.strong_long_reference_not_found,
        metrics.strong_long_coordinates_unavailable,
    ]
    .into_iter()
    .try_fold(0u64, u64::checked_add)
    .context("Wi-Fi strong long-training detail partition overflow")?;
    ensure!(
        detail_count == rejections.strong_long_training_not_found,
        "Wi-Fi strong long-training detail differs from its rejection subtotal"
    );
    Ok(metrics)
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiControlFrameTypes {
    pub(crate) control_wrapper: u64,
    pub(crate) block_ack_request: u64,
    pub(crate) block_ack: u64,
    pub(crate) ps_poll: u64,
    pub(crate) rts: u64,
    pub(crate) cts: u64,
    pub(crate) ack: u64,
    pub(crate) cf_end: u64,
    pub(crate) cf_end_ack: u64,
    pub(crate) other_control: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiDataFrameTypes {
    pub(crate) data: u64,
    pub(crate) null_data: u64,
    pub(crate) qos_data: u64,
    pub(crate) qos_null: u64,
    pub(crate) other_data: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiFrameTypeMetrics {
    pub(crate) management: WifiManagementFrameTypes,
    pub(crate) control: WifiControlFrameTypes,
    pub(crate) data: WifiDataFrameTypes,
}

fn optional_frame_type_count(frame_types: &Value, source_name: &str) -> Result<u64> {
    match frame_types.get(source_name) {
        Some(value) => value
            .as_u64()
            .with_context(|| format!("Wi-Fi frame-type count {source_name} is not u64")),
        None => Ok(0),
    }
}

fn checked_frame_type_sum<const N: usize>(counts: [u64; N], group: &str) -> Result<u64> {
    counts
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .with_context(|| format!("Wi-Fi {group} frame-type partition overflow"))
}

pub(crate) fn wifi_frame_type_metrics(summary: &Value) -> Result<WifiFrameTypeMetrics> {
    const SOURCE_NAMES: [&str; 29] = [
        "association_request",
        "association_response",
        "reassociation_request",
        "reassociation_response",
        "probe_request",
        "probe_response",
        "beacon",
        "ATIM",
        "disassociation",
        "authentication",
        "deauthentication",
        "action",
        "action_no_ack",
        "reserved_management",
        "control_wrapper",
        "block_ack_request",
        "block_ack",
        "PS_POLL",
        "RTS",
        "CTS",
        "ACK",
        "CF_END",
        "CF_END_ACK",
        "other_control",
        "data",
        "null_data",
        "qos_data",
        "qos_null",
        "other_data",
    ];
    let frame_types = summary
        .get("frames_by_type")
        .and_then(Value::as_object)
        .context("Wi-Fi packet summary lacks frame-type accounting")?;
    ensure!(
        frame_types
            .keys()
            .all(|source_name| SOURCE_NAMES.contains(&source_name.as_str())),
        "Wi-Fi frame-type accounting contains an unknown producer key"
    );
    let frame_types = Value::Object(frame_types.clone());
    let management = WifiManagementFrameTypes {
        association_request: optional_frame_type_count(&frame_types, "association_request")?,
        association_response: optional_frame_type_count(&frame_types, "association_response")?,
        reassociation_request: optional_frame_type_count(&frame_types, "reassociation_request")?,
        reassociation_response: optional_frame_type_count(&frame_types, "reassociation_response")?,
        probe_request: optional_frame_type_count(&frame_types, "probe_request")?,
        probe_response: optional_frame_type_count(&frame_types, "probe_response")?,
        beacon: optional_frame_type_count(&frame_types, "beacon")?,
        atim: optional_frame_type_count(&frame_types, "ATIM")?,
        disassociation: optional_frame_type_count(&frame_types, "disassociation")?,
        authentication: optional_frame_type_count(&frame_types, "authentication")?,
        deauthentication: optional_frame_type_count(&frame_types, "deauthentication")?,
        action: optional_frame_type_count(&frame_types, "action")?,
        action_no_ack: optional_frame_type_count(&frame_types, "action_no_ack")?,
        reserved_management: optional_frame_type_count(&frame_types, "reserved_management")?,
    };
    let control = WifiControlFrameTypes {
        control_wrapper: optional_frame_type_count(&frame_types, "control_wrapper")?,
        block_ack_request: optional_frame_type_count(&frame_types, "block_ack_request")?,
        block_ack: optional_frame_type_count(&frame_types, "block_ack")?,
        ps_poll: optional_frame_type_count(&frame_types, "PS_POLL")?,
        rts: optional_frame_type_count(&frame_types, "RTS")?,
        cts: optional_frame_type_count(&frame_types, "CTS")?,
        ack: optional_frame_type_count(&frame_types, "ACK")?,
        cf_end: optional_frame_type_count(&frame_types, "CF_END")?,
        cf_end_ack: optional_frame_type_count(&frame_types, "CF_END_ACK")?,
        other_control: optional_frame_type_count(&frame_types, "other_control")?,
    };
    let data = WifiDataFrameTypes {
        data: optional_frame_type_count(&frame_types, "data")?,
        null_data: optional_frame_type_count(&frame_types, "null_data")?,
        qos_data: optional_frame_type_count(&frame_types, "qos_data")?,
        qos_null: optional_frame_type_count(&frame_types, "qos_null")?,
        other_data: optional_frame_type_count(&frame_types, "other_data")?,
    };
    let management_total = checked_frame_type_sum(
        [
            management.association_request,
            management.association_response,
            management.reassociation_request,
            management.reassociation_response,
            management.probe_request,
            management.probe_response,
            management.beacon,
            management.atim,
            management.disassociation,
            management.authentication,
            management.deauthentication,
            management.action,
            management.action_no_ack,
            management.reserved_management,
        ],
        "management",
    )?;
    let control_total = checked_frame_type_sum(
        [
            control.control_wrapper,
            control.block_ack_request,
            control.block_ack,
            control.ps_poll,
            control.rts,
            control.cts,
            control.ack,
            control.cf_end,
            control.cf_end_ack,
            control.other_control,
        ],
        "control",
    )?;
    let data_total = checked_frame_type_sum(
        [
            data.data,
            data.null_data,
            data.qos_data,
            data.qos_null,
            data.other_data,
        ],
        "data",
    )?;
    ensure!(
        management_total == report::required_u64(summary, "management_frames")?
            && control_total == report::required_u64(summary, "control_frames")?
            && data_total == report::required_u64(summary, "data_frames")?,
        "Wi-Fi frame-type group partition differs from frame-class accounting"
    );
    ensure!(
        management_total
            .checked_add(control_total)
            .and_then(|total| total.checked_add(data_total))
            == Some(report::required_u64(summary, "validated_frames")?),
        "Wi-Fi frame-type partition differs from validated frames"
    );
    Ok(WifiFrameTypeMetrics {
        management,
        control,
        data,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiBlockAckTidCounts {
    pub(crate) tid_0: u64,
    pub(crate) tid_1: u64,
    pub(crate) tid_2: u64,
    pub(crate) tid_3: u64,
    pub(crate) tid_4: u64,
    pub(crate) tid_5: u64,
    pub(crate) tid_6: u64,
    pub(crate) tid_7: u64,
    pub(crate) tid_8: u64,
    pub(crate) tid_9: u64,
    pub(crate) tid_10: u64,
    pub(crate) tid_11: u64,
    pub(crate) tid_12: u64,
    pub(crate) tid_13: u64,
    pub(crate) tid_14: u64,
    pub(crate) tid_15: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct WifiBlockAckMetrics {
    pub(crate) frame_count: u64,
    pub(crate) truncated_control_count: u64,
    pub(crate) complete_compressed_single_tid_count: u64,
    pub(crate) truncated_compressed_single_tid_count: u64,
    pub(crate) trailing_compressed_single_tid_count: u64,
    pub(crate) other_variant_count: u64,
    pub(crate) tids: WifiBlockAckTidCounts,
    pub(crate) set_bitmap_bits_total: u64,
    pub(crate) zero_bitmap_frames: u64,
    pub(crate) nonzero_bitmap_frames: u64,
    pub(crate) full_bitmap_frames: u64,
}

pub(crate) fn wifi_block_ack_metrics(
    summary: &Value,
    frame_types: &WifiFrameTypeMetrics,
) -> Result<WifiBlockAckMetrics> {
    const BLOCK_ACK_FIELDS: [&str; 11] = [
        "frame_count",
        "truncated_control_count",
        "complete_compressed_single_tid_count",
        "truncated_compressed_single_tid_count",
        "trailing_compressed_single_tid_count",
        "other_variant_count",
        "tids",
        "set_bitmap_bits_total",
        "zero_bitmap_frames",
        "nonzero_bitmap_frames",
        "full_bitmap_frames",
    ];
    const TID_FIELDS: [&str; 16] = [
        "tid_0", "tid_1", "tid_2", "tid_3", "tid_4", "tid_5", "tid_6", "tid_7", "tid_8", "tid_9",
        "tid_10", "tid_11", "tid_12", "tid_13", "tid_14", "tid_15",
    ];
    let block_ack = summary
        .get("block_ack")
        .context("Wi-Fi packet summary lacks Block Ack accounting")?;
    ensure!(
        block_ack.as_object().is_some_and(|object| {
            object.len() == BLOCK_ACK_FIELDS.len()
                && BLOCK_ACK_FIELDS
                    .iter()
                    .all(|field| object.contains_key(*field))
        }),
        "Wi-Fi Block Ack accounting fields differ"
    );
    let tids = block_ack
        .get("tids")
        .context("Wi-Fi Block Ack accounting lacks TID counts")?;
    ensure!(
        tids.as_object().is_some_and(|object| {
            object.len() == TID_FIELDS.len()
                && TID_FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi Block Ack TID fields differ"
    );
    let metrics = WifiBlockAckMetrics {
        frame_count: report::required_u64(block_ack, "frame_count")?,
        truncated_control_count: report::required_u64(block_ack, "truncated_control_count")?,
        complete_compressed_single_tid_count: report::required_u64(
            block_ack,
            "complete_compressed_single_tid_count",
        )?,
        truncated_compressed_single_tid_count: report::required_u64(
            block_ack,
            "truncated_compressed_single_tid_count",
        )?,
        trailing_compressed_single_tid_count: report::required_u64(
            block_ack,
            "trailing_compressed_single_tid_count",
        )?,
        other_variant_count: report::required_u64(block_ack, "other_variant_count")?,
        tids: WifiBlockAckTidCounts {
            tid_0: report::required_u64(tids, "tid_0")?,
            tid_1: report::required_u64(tids, "tid_1")?,
            tid_2: report::required_u64(tids, "tid_2")?,
            tid_3: report::required_u64(tids, "tid_3")?,
            tid_4: report::required_u64(tids, "tid_4")?,
            tid_5: report::required_u64(tids, "tid_5")?,
            tid_6: report::required_u64(tids, "tid_6")?,
            tid_7: report::required_u64(tids, "tid_7")?,
            tid_8: report::required_u64(tids, "tid_8")?,
            tid_9: report::required_u64(tids, "tid_9")?,
            tid_10: report::required_u64(tids, "tid_10")?,
            tid_11: report::required_u64(tids, "tid_11")?,
            tid_12: report::required_u64(tids, "tid_12")?,
            tid_13: report::required_u64(tids, "tid_13")?,
            tid_14: report::required_u64(tids, "tid_14")?,
            tid_15: report::required_u64(tids, "tid_15")?,
        },
        set_bitmap_bits_total: report::required_u64(block_ack, "set_bitmap_bits_total")?,
        zero_bitmap_frames: report::required_u64(block_ack, "zero_bitmap_frames")?,
        nonzero_bitmap_frames: report::required_u64(block_ack, "nonzero_bitmap_frames")?,
        full_bitmap_frames: report::required_u64(block_ack, "full_bitmap_frames")?,
    };
    let partition_count = [
        metrics.truncated_control_count,
        metrics.complete_compressed_single_tid_count,
        metrics.truncated_compressed_single_tid_count,
        metrics.trailing_compressed_single_tid_count,
        metrics.other_variant_count,
    ]
    .into_iter()
    .try_fold(0u64, u64::checked_add)
    .context("Wi-Fi Block Ack frame partition overflow")?;
    ensure!(
        partition_count == metrics.frame_count,
        "Wi-Fi Block Ack frame partition differs from frame count"
    );
    ensure!(
        metrics.frame_count == frame_types.control.block_ack,
        "Wi-Fi Block Ack aggregate differs from frame-type accounting"
    );
    ensure!(
        metrics.frame_count <= report::required_u64(summary, "control_frames")?
            && metrics.frame_count <= report::required_u64(summary, "validated_frames")?,
        "Wi-Fi Block Ack frame count exceeds validated control frames"
    );
    let tid_count = [
        metrics.tids.tid_0,
        metrics.tids.tid_1,
        metrics.tids.tid_2,
        metrics.tids.tid_3,
        metrics.tids.tid_4,
        metrics.tids.tid_5,
        metrics.tids.tid_6,
        metrics.tids.tid_7,
        metrics.tids.tid_8,
        metrics.tids.tid_9,
        metrics.tids.tid_10,
        metrics.tids.tid_11,
        metrics.tids.tid_12,
        metrics.tids.tid_13,
        metrics.tids.tid_14,
        metrics.tids.tid_15,
    ]
    .into_iter()
    .try_fold(0u64, u64::checked_add)
    .context("Wi-Fi Block Ack TID partition overflow")?;
    ensure!(
        tid_count == metrics.complete_compressed_single_tid_count,
        "Wi-Fi Block Ack TID partition differs from complete compressed single-TID frames"
    );
    ensure!(
        metrics
            .zero_bitmap_frames
            .checked_add(metrics.nonzero_bitmap_frames)
            == Some(metrics.complete_compressed_single_tid_count),
        "Wi-Fi Block Ack bitmap partition differs from complete compressed single-TID frames"
    );
    ensure!(
        metrics.full_bitmap_frames <= metrics.nonzero_bitmap_frames,
        "Wi-Fi Block Ack full-bitmap count exceeds nonzero bitmaps"
    );
    let minimum_set_bitmap_bits = metrics
        .full_bitmap_frames
        .checked_mul(63)
        .and_then(|full_remainder| metrics.nonzero_bitmap_frames.checked_add(full_remainder))
        .context("Wi-Fi Block Ack minimum bitmap population overflow")?;
    let maximum_set_bitmap_bits = metrics
        .nonzero_bitmap_frames
        .checked_mul(64)
        .context("Wi-Fi Block Ack bitmap capacity overflow")?;
    ensure!(
        (minimum_set_bitmap_bits..=maximum_set_bitmap_bits)
            .contains(&metrics.set_bitmap_bits_total),
        "Wi-Fi Block Ack set-bit count is inconsistent with zero, nonzero and full bitmaps"
    );
    Ok(metrics)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WifiEapolPacketTypes {
    pub(crate) eap_packet: u64,
    pub(crate) start: u64,
    pub(crate) logoff: u64,
    pub(crate) key: u64,
    pub(crate) asf_alert: u64,
    pub(crate) other: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WifiEapolMetrics {
    pub(crate) frame_count: u64,
    pub(crate) complete_header_count: u64,
    pub(crate) truncated_header_count: u64,
    pub(crate) complete_body_count: u64,
    pub(crate) truncated_body_count: u64,
    pub(crate) packet_types: WifiEapolPacketTypes,
    pub(crate) parsed_key_header_count: u64,
    pub(crate) pairwise_key_count: u64,
    pub(crate) install_key_count: u64,
    pub(crate) ack_key_count: u64,
    pub(crate) mic_key_count: u64,
    pub(crate) secure_key_count: u64,
}

pub(crate) fn wifi_eapol_metrics(summary: &Value) -> Result<WifiEapolMetrics> {
    const EAPOL_FIELDS: [&str; 12] = [
        "frame_count",
        "complete_header_count",
        "truncated_header_count",
        "complete_body_count",
        "truncated_body_count",
        "packet_types",
        "parsed_key_header_count",
        "pairwise_key_count",
        "install_key_count",
        "ack_key_count",
        "mic_key_count",
        "secure_key_count",
    ];
    const PACKET_TYPE_FIELDS: [&str; 6] =
        ["eap_packet", "start", "logoff", "key", "asf_alert", "other"];
    let eapol = summary
        .get("eapol")
        .context("Wi-Fi packet summary lacks EAPOL accounting")?;
    ensure!(
        eapol.as_object().is_some_and(|object| {
            object.len() == EAPOL_FIELDS.len()
                && EAPOL_FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi EAPOL accounting fields differ"
    );
    let packet_types = eapol
        .get("packet_types")
        .context("Wi-Fi EAPOL accounting lacks packet types")?;
    ensure!(
        packet_types.as_object().is_some_and(|object| {
            object.len() == PACKET_TYPE_FIELDS.len()
                && PACKET_TYPE_FIELDS
                    .iter()
                    .all(|field| object.contains_key(*field))
        }),
        "Wi-Fi EAPOL packet-type fields differ"
    );
    let packet_types = WifiEapolPacketTypes {
        eap_packet: report::required_u64(packet_types, "eap_packet")?,
        start: report::required_u64(packet_types, "start")?,
        logoff: report::required_u64(packet_types, "logoff")?,
        key: report::required_u64(packet_types, "key")?,
        asf_alert: report::required_u64(packet_types, "asf_alert")?,
        other: report::required_u64(packet_types, "other")?,
    };
    let metrics = WifiEapolMetrics {
        frame_count: report::required_u64(eapol, "frame_count")?,
        complete_header_count: report::required_u64(eapol, "complete_header_count")?,
        truncated_header_count: report::required_u64(eapol, "truncated_header_count")?,
        complete_body_count: report::required_u64(eapol, "complete_body_count")?,
        truncated_body_count: report::required_u64(eapol, "truncated_body_count")?,
        packet_types,
        parsed_key_header_count: report::required_u64(eapol, "parsed_key_header_count")?,
        pairwise_key_count: report::required_u64(eapol, "pairwise_key_count")?,
        install_key_count: report::required_u64(eapol, "install_key_count")?,
        ack_key_count: report::required_u64(eapol, "ack_key_count")?,
        mic_key_count: report::required_u64(eapol, "mic_key_count")?,
        secure_key_count: report::required_u64(eapol, "secure_key_count")?,
    };
    ensure!(
        metrics
            .complete_header_count
            .checked_add(metrics.truncated_header_count)
            == Some(metrics.frame_count),
        "Wi-Fi EAPOL header partition differs from frame count"
    );
    ensure!(
        metrics
            .complete_body_count
            .checked_add(metrics.truncated_body_count)
            == Some(metrics.complete_header_count),
        "Wi-Fi EAPOL body partition differs from complete headers"
    );
    let packet_type_count = [
        metrics.packet_types.eap_packet,
        metrics.packet_types.start,
        metrics.packet_types.logoff,
        metrics.packet_types.key,
        metrics.packet_types.asf_alert,
        metrics.packet_types.other,
    ]
    .into_iter()
    .try_fold(0u64, |sum, value| sum.checked_add(value))
    .context("Wi-Fi EAPOL packet-type partition overflow")?;
    ensure!(
        packet_type_count == metrics.complete_header_count,
        "Wi-Fi EAPOL packet-type partition differs from complete headers"
    );
    ensure!(
        metrics.parsed_key_header_count <= metrics.packet_types.key,
        "Wi-Fi EAPOL parsed-key count exceeds Key packet count"
    );
    ensure!(
        metrics.parsed_key_header_count <= metrics.complete_body_count,
        "Wi-Fi EAPOL parsed-key count exceeds complete bodies"
    );
    ensure!(
        [
            metrics.pairwise_key_count,
            metrics.install_key_count,
            metrics.ack_key_count,
            metrics.mic_key_count,
            metrics.secure_key_count,
        ]
        .into_iter()
        .all(|count| count <= metrics.parsed_key_header_count),
        "Wi-Fi EAPOL key-flag count exceeds parsed key headers"
    );
    ensure!(
        metrics.frame_count <= report::required_u64(summary, "data_frames")?,
        "Wi-Fi EAPOL frame count exceeds data frames"
    );
    Ok(metrics)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WifiAdvertisementPhyMetrics {
    pub(crate) ht_capabilities_element_frames: u64,
    pub(crate) ht_operation_element_frames: u64,
    pub(crate) vht_capabilities_element_frames: u64,
    pub(crate) vht_operation_element_frames: u64,
    pub(crate) he_capabilities_element_frames: u64,
    pub(crate) he_operation_element_frames: u64,
    pub(crate) eht_capabilities_element_frames: u64,
    pub(crate) eht_operation_element_frames: u64,
    pub(crate) unrecognized_extension_element_frames: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WifiAdvertisementSecurityMetrics {
    pub(crate) privacy_capability_frames: u64,
    pub(crate) rsn_present_frames: u64,
    pub(crate) rsn_parsed_frames: u64,
    pub(crate) rsn_malformed_frames: u64,
    pub(crate) pmf_capable_frames: u64,
    pub(crate) pmf_required_frames: u64,
    pub(crate) wpa_present_frames: u64,
    pub(crate) wpa_parsed_frames: u64,
    pub(crate) wpa_malformed_frames: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WifiAdvertisementMetrics {
    pub(crate) eligible_frame_count: u64,
    pub(crate) beacon_frames: u64,
    pub(crate) probe_response_frames: u64,
    pub(crate) complete_ie_layout_frames: u64,
    pub(crate) truncated_ie_layout_frames: u64,
    pub(crate) phy: WifiAdvertisementPhyMetrics,
    pub(crate) security: WifiAdvertisementSecurityMetrics,
}

pub(crate) fn wifi_advertisement_metrics(summary: &Value) -> Result<WifiAdvertisementMetrics> {
    const ADVERTISEMENT_FIELDS: [&str; 7] = [
        "eligible_frame_count",
        "beacon_frames",
        "probe_response_frames",
        "complete_ie_layout_frames",
        "truncated_ie_layout_frames",
        "phy",
        "security",
    ];
    const PHY_FIELDS: [&str; 9] = [
        "ht_capabilities_element_frames",
        "ht_operation_element_frames",
        "vht_capabilities_element_frames",
        "vht_operation_element_frames",
        "he_capabilities_element_frames",
        "he_operation_element_frames",
        "eht_capabilities_element_frames",
        "eht_operation_element_frames",
        "unrecognized_extension_element_frames",
    ];
    const SECURITY_FIELDS: [&str; 9] = [
        "privacy_capability_frames",
        "rsn_present_frames",
        "rsn_parsed_frames",
        "rsn_malformed_frames",
        "pmf_capable_frames",
        "pmf_required_frames",
        "wpa_present_frames",
        "wpa_parsed_frames",
        "wpa_malformed_frames",
    ];
    let advertisements = summary
        .get("advertisements")
        .context("Wi-Fi packet summary lacks advertisement accounting")?;
    ensure!(
        advertisements.as_object().is_some_and(|object| {
            object.len() == ADVERTISEMENT_FIELDS.len()
                && ADVERTISEMENT_FIELDS
                    .iter()
                    .all(|field| object.contains_key(*field))
        }),
        "Wi-Fi advertisement accounting fields differ"
    );
    let phy = advertisements
        .get("phy")
        .context("Wi-Fi advertisement accounting lacks PHY fields")?;
    ensure!(
        phy.as_object().is_some_and(|object| {
            object.len() == PHY_FIELDS.len()
                && PHY_FIELDS.iter().all(|field| object.contains_key(*field))
        }),
        "Wi-Fi advertisement PHY fields differ"
    );
    let security = advertisements
        .get("security")
        .context("Wi-Fi advertisement accounting lacks security fields")?;
    ensure!(
        security.as_object().is_some_and(|object| {
            object.len() == SECURITY_FIELDS.len()
                && SECURITY_FIELDS
                    .iter()
                    .all(|field| object.contains_key(*field))
        }),
        "Wi-Fi advertisement security fields differ"
    );
    let metrics = WifiAdvertisementMetrics {
        eligible_frame_count: report::required_u64(advertisements, "eligible_frame_count")?,
        beacon_frames: report::required_u64(advertisements, "beacon_frames")?,
        probe_response_frames: report::required_u64(advertisements, "probe_response_frames")?,
        complete_ie_layout_frames: report::required_u64(
            advertisements,
            "complete_ie_layout_frames",
        )?,
        truncated_ie_layout_frames: report::required_u64(
            advertisements,
            "truncated_ie_layout_frames",
        )?,
        phy: WifiAdvertisementPhyMetrics {
            ht_capabilities_element_frames: report::required_u64(
                phy,
                "ht_capabilities_element_frames",
            )?,
            ht_operation_element_frames: report::required_u64(phy, "ht_operation_element_frames")?,
            vht_capabilities_element_frames: report::required_u64(
                phy,
                "vht_capabilities_element_frames",
            )?,
            vht_operation_element_frames: report::required_u64(
                phy,
                "vht_operation_element_frames",
            )?,
            he_capabilities_element_frames: report::required_u64(
                phy,
                "he_capabilities_element_frames",
            )?,
            he_operation_element_frames: report::required_u64(phy, "he_operation_element_frames")?,
            eht_capabilities_element_frames: report::required_u64(
                phy,
                "eht_capabilities_element_frames",
            )?,
            eht_operation_element_frames: report::required_u64(
                phy,
                "eht_operation_element_frames",
            )?,
            unrecognized_extension_element_frames: report::required_u64(
                phy,
                "unrecognized_extension_element_frames",
            )?,
        },
        security: WifiAdvertisementSecurityMetrics {
            privacy_capability_frames: report::required_u64(security, "privacy_capability_frames")?,
            rsn_present_frames: report::required_u64(security, "rsn_present_frames")?,
            rsn_parsed_frames: report::required_u64(security, "rsn_parsed_frames")?,
            rsn_malformed_frames: report::required_u64(security, "rsn_malformed_frames")?,
            pmf_capable_frames: report::required_u64(security, "pmf_capable_frames")?,
            pmf_required_frames: report::required_u64(security, "pmf_required_frames")?,
            wpa_present_frames: report::required_u64(security, "wpa_present_frames")?,
            wpa_parsed_frames: report::required_u64(security, "wpa_parsed_frames")?,
            wpa_malformed_frames: report::required_u64(security, "wpa_malformed_frames")?,
        },
    };
    ensure!(
        metrics
            .beacon_frames
            .checked_add(metrics.probe_response_frames)
            == Some(metrics.eligible_frame_count),
        "Wi-Fi advertisement subtype partition differs from eligible frames"
    );
    ensure!(
        metrics
            .complete_ie_layout_frames
            .checked_add(metrics.truncated_ie_layout_frames)
            == Some(metrics.eligible_frame_count),
        "Wi-Fi advertisement IE-layout partition differs from eligible frames"
    );
    ensure!(
        [
            metrics.phy.ht_capabilities_element_frames,
            metrics.phy.ht_operation_element_frames,
            metrics.phy.vht_capabilities_element_frames,
            metrics.phy.vht_operation_element_frames,
            metrics.phy.he_capabilities_element_frames,
            metrics.phy.he_operation_element_frames,
            metrics.phy.eht_capabilities_element_frames,
            metrics.phy.eht_operation_element_frames,
            metrics.phy.unrecognized_extension_element_frames,
            metrics.security.privacy_capability_frames,
            metrics.security.rsn_present_frames,
            metrics.security.rsn_parsed_frames,
            metrics.security.rsn_malformed_frames,
            metrics.security.pmf_capable_frames,
            metrics.security.pmf_required_frames,
            metrics.security.wpa_present_frames,
            metrics.security.wpa_parsed_frames,
            metrics.security.wpa_malformed_frames,
        ]
        .into_iter()
        .all(|count| count <= metrics.eligible_frame_count),
        "Wi-Fi advertisement overlapping count exceeds eligible frames"
    );
    ensure!(
        metrics.eligible_frame_count <= report::required_u64(summary, "management_frames")?
            && metrics.eligible_frame_count <= report::required_u64(summary, "validated_frames")?,
        "Wi-Fi advertisement count exceeds validated management frames"
    );
    ensure!(
        metrics.security.rsn_parsed_frames <= metrics.security.rsn_present_frames
            && metrics.security.rsn_malformed_frames <= metrics.security.rsn_present_frames,
        "Wi-Fi RSN parse count exceeds RSN presence"
    );
    ensure!(
        metrics.security.wpa_parsed_frames <= metrics.security.wpa_present_frames
            && metrics.security.wpa_malformed_frames <= metrics.security.wpa_present_frames,
        "Wi-Fi WPA parse count exceeds WPA presence"
    );
    ensure!(
        metrics.security.pmf_capable_frames <= metrics.security.rsn_parsed_frames
            && metrics.security.pmf_required_frames <= metrics.security.rsn_parsed_frames,
        "Wi-Fi PMF count exceeds parsed RSN frames"
    );
    Ok(metrics)
}

pub(crate) fn wifi_outcome(
    summary: &Value,
    decode: &Value,
    source: &SourceReport,
) -> Result<Outcome> {
    ensure!(
        [
            "capture_frequency_hz",
            "frequency_shift_hz",
            "translation_clipped_samples",
            "translation_filter",
        ]
        .iter()
        .all(|field| decode.get(field).is_none()),
        "Wi-Fi replay reported an unrequested frequency translation"
    );
    wifi_outcome_at(summary, decode, source, source.center_frequency_hz)
}

pub(crate) fn wifi_outcome_at(
    summary: &Value,
    decode: &Value,
    source: &SourceReport,
    decoder_frequency_hz: u64,
) -> Result<Outcome> {
    ensure!(
        report::required_u64(summary, "schema_version")? == 4
            && summary.get("operation").and_then(Value::as_str) == Some("inspect_wifi_packets"),
        "unsupported Wi-Fi packet-summary contract"
    );
    ensure!(
        report::required_u64(decode, "schema_version")? == 4
            && decode.get("operation").and_then(Value::as_str) == Some("offline_wifi_iq_decode"),
        "unsupported Wi-Fi IQ-decode contract"
    );
    let total = report::required_u64(summary, "total_records")?;
    let valid = report::required_u64(summary, "validated_frames")?;
    let frame_types = wifi_frame_type_metrics(summary)?;
    let _block_ack = wifi_block_ack_metrics(summary, &frame_types)?;
    let _eapol = wifi_eapol_metrics(summary)?;
    let _advertisements = wifi_advertisement_metrics(summary)?;
    let ofdm_rejections = wifi_ofdm_rejection_metrics(decode)?;
    let _data_failures = wifi_ofdm_data_failure_metrics(decode, &ofdm_rejections)?;
    let _strong_long_training = wifi_ofdm_strong_long_training_metrics(decode, &ofdm_rejections)?;
    let ofdm_invalid_fcs = report::required_u64(
        decode
            .get("ofdm")
            .context("Wi-Fi IQ-decode summary lacks OFDM counters")?,
        "invalid_fcs",
    )?;
    let dsss_invalid_fcs = report::required_u64(
        decode
            .get("dsss")
            .context("Wi-Fi IQ-decode summary lacks DSSS counters")?,
        "invalid_fcs",
    )?;
    ensure!(
        valid <= total,
        "Wi-Fi validated frame count exceeds records"
    );
    ensure!(
        report::required_u64(decode, "input_samples")? == source.complex_samples
            && report::required_u64(decode, "processed_samples")? == source.complex_samples,
        "Wi-Fi replay did not report the complete validated input extent"
    );
    ensure!(
        report::required_u64(decode, "center_frequency_hz")? == decoder_frequency_hz
            && report::required_u64(decode, "sample_rate_hz")? == source.sample_rate_hz,
        "Wi-Fi replay frequency contract differs from the validated input"
    );
    ensure!(
        report::required_u64(decode, "frames_written")? == total,
        "Wi-Fi decoder and inspector record counts differ"
    );
    Ok(if valid > 0 {
        Outcome::EvidenceFound
    } else if total > 0 || ofdm_invalid_fcs > 0 || dsss_invalid_fcs > 0 {
        Outcome::CandidatesWithoutValidFrames
    } else {
        Outcome::NoValidCandidates
    })
}

fn wifi(
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    const NAME: &str = "wifi";
    let destination = output.join("wifi");
    let arguments = vec![
        text("decode-iq"),
        path(&source.input),
        text("--format"),
        text("sc16q11"),
        text("--frequency-mhz"),
        text(settings.center_frequency_hz / 1_000_000),
        text("--max-samples"),
        text(settings.max_samples),
        text("--decode-timeout"),
        text(settings.decoder_timeout_seconds),
        text("--output"),
        path(&destination),
    ];
    let command = match run_one(&programs.wifi, arguments, output, "wifi", settings, stop) {
        Ok(command) => command,
        Err(error) => return failed(NAME, StageState::Failed, Vec::new(), error),
    };
    let state = command_state(&command);
    let commands = vec![command];
    if let Err(error) = process::require_success(&commands[0]) {
        return failed(NAME, state, commands, error);
    }
    let outcome = report::read_json(&destination.join("summary.json")).and_then(|summary| {
        report::read_json(&destination.join("candidates.decode.json"))
            .and_then(|decode| wifi_outcome(&summary, &decode, source))
    });
    match outcome {
        Ok(outcome) => completed(NAME, outcome, commands),
        Err(error) => failed(NAME, StageState::Failed, commands, error),
    }
}

pub(crate) fn rf_outcome(summary: &Value, source: &SourceReport) -> Result<Outcome> {
    ensure!(
        report::required_u64(summary, "schema_version")? == 1
            && summary.get("operation").and_then(Value::as_str)
                == Some("offline_rf_spectrum_survey"),
        "unsupported RF spectrum-summary contract"
    );
    ensure!(
        report::required_u64(summary, "input_samples")? == source.complex_samples
            && report::required_u64(summary, "inspected_prefix_samples")? == source.complex_samples,
        "RF survey did not report the complete validated input extent"
    );
    ensure!(
        report::required_u64(summary, "center_frequency_hz")? == source.center_frequency_hz
            && report::required_u64(summary, "sample_rate_hz")? == source.sample_rate_hz,
        "RF survey frequency contract differs from the validated input"
    );
    let regions = report::required_array_len(summary, "/activity_detection/regions")?;
    Ok(if regions > 0 {
        Outcome::UnclassifiedActivity
    } else {
        Outcome::NoUnclassifiedActivityDetected
    })
}

fn rf_survey(
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    const NAME: &str = "rf_survey";
    let destination = output.join("rf-survey");
    let arguments = vec![
        text("survey"),
        path(&source.input),
        text("--center-mhz"),
        text(settings.center_frequency_hz / 1_000_000),
        text("--max-samples"),
        text(settings.max_samples),
        text("--windows"),
        text(settings.spectrum_windows),
        text("--output"),
        path(&destination),
    ];
    let command = match run_one(&programs.rf, arguments, output, "rf-survey", settings, stop) {
        Ok(command) => command,
        Err(error) => return failed(NAME, StageState::Failed, Vec::new(), error),
    };
    let state = command_state(&command);
    let commands = vec![command];
    if let Err(error) = process::require_success(&commands[0]) {
        return failed(NAME, state, commands, error);
    }
    match report::read_json(&destination.join("spectrum.json"))
        .and_then(|value| rf_outcome(&value, source))
    {
        Ok(outcome) => completed(NAME, outcome, commands),
        Err(error) => failed(NAME, StageState::Failed, commands, error),
    }
}

pub(crate) fn ieee_comparison_outcome(comparisons: &[Value]) -> Result<Outcome> {
    let mut identities = 0u64;
    let mut valid = 0u64;
    for comparison in comparisons {
        identities = identities
            .checked_add(report::required_u64(
                comparison,
                "identity_union_record_count",
            )?)
            .context("IEEE 802.15.4 identity-union total overflow")?;
        valid = valid
            .checked_add(report::required_u64(
                comparison,
                "identity_union_valid_fcs_record_count",
            )?)
            .context("IEEE 802.15.4 valid identity-union total overflow")?;
    }
    ensure!(
        valid <= identities,
        "IEEE 802.15.4 valid identity count exceeds the identity union"
    );
    Ok(if valid > 0 {
        Outcome::EvidenceFound
    } else if identities > 0 {
        Outcome::CandidatesWithoutValidFrames
    } else {
        Outcome::NoValidCandidates
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct IeeeCounts {
    records: u64,
    valid: u64,
    invalid: u64,
    invalid_length: u64,
}

impl IeeeCounts {
    fn checked_add(self, other: Self, context: &'static str) -> Result<Self> {
        Ok(Self {
            records: self.records.checked_add(other.records).context(context)?,
            valid: self.valid.checked_add(other.valid).context(context)?,
            invalid: self.invalid.checked_add(other.invalid).context(context)?,
            invalid_length: self
                .invalid_length
                .checked_add(other.invalid_length)
                .context(context)?,
        })
    }

    fn checked_sub(self, other: Self, context: &'static str) -> Result<Self> {
        Ok(Self {
            records: self.records.checked_sub(other.records).context(context)?,
            valid: self.valid.checked_sub(other.valid).context(context)?,
            invalid: self.invalid.checked_sub(other.invalid).context(context)?,
            invalid_length: self
                .invalid_length
                .checked_sub(other.invalid_length)
                .context(context)?,
        })
    }

    fn checked_mul(self, factor: u64, context: &'static str) -> Result<Self> {
        Ok(Self {
            records: self.records.checked_mul(factor).context(context)?,
            valid: self.valid.checked_mul(factor).context(context)?,
            invalid: self.invalid.checked_mul(factor).context(context)?,
            invalid_length: self.invalid_length.checked_mul(factor).context(context)?,
        })
    }

    fn require_closed(self, context: &'static str) -> Result<()> {
        ensure!(
            self.valid
                .checked_add(self.invalid)
                .and_then(|count| count.checked_add(self.invalid_length))
                == Some(self.records),
            "{context} FCS partition does not close"
        );
        Ok(())
    }
}

fn require_exact_fields(value: &Value, expected: &[&str], context: &'static str) -> Result<()> {
    let fields = value
        .as_object()
        .with_context(|| format!("{context} must be an object"))?;
    ensure!(
        fields.len() == expected.len() && expected.iter().all(|field| fields.contains_key(*field)),
        "{context} has an unexpected field set"
    );
    Ok(())
}

fn ieee_inspection_counts(summary: &Value) -> Result<IeeeCounts> {
    let counts = IeeeCounts {
        records: report::required_u64(summary, "candidate_record_count")?,
        valid: report::required_u64(summary, "valid_fcs_record_count")?,
        invalid: report::required_u64(summary, "invalid_fcs_record_count")?,
        invalid_length: report::required_u64(summary, "invalid_length_record_count")?,
    };
    counts.require_closed("IEEE 802.15.4 inspection")?;
    Ok(counts)
}

fn ieee_aggregate_counts(summary: &Value, prefix: &str) -> Result<IeeeCounts> {
    Ok(IeeeCounts {
        records: report::required_u64(summary, &format!("{prefix}_record_count"))?,
        valid: report::required_u64(summary, &format!("{prefix}_valid_fcs_record_count"))?,
        invalid: report::required_u64(summary, &format!("{prefix}_invalid_fcs_record_count"))?,
        invalid_length: report::required_u64(
            summary,
            &format!("{prefix}_invalid_length_record_count"),
        )?,
    })
}

pub(crate) fn validate_ieee_comparison(
    summary: &Value,
    expected_channel: u8,
    archives: &[report::FileDigest],
    inspections: &[Value],
) -> Result<()> {
    const SUMMARY_FIELDS: [&str; 24] = [
        "schema_version",
        "operation",
        "source_format",
        "source_count",
        "sources",
        "ieee802154_channel",
        "channel_sample_rate_hz",
        "recovered_chip_rate_hz",
        "source_candidate_record_count_sum",
        "source_valid_fcs_record_count_sum",
        "source_invalid_fcs_record_count_sum",
        "source_invalid_length_record_count_sum",
        "identity_union_record_count",
        "identity_union_valid_fcs_record_count",
        "identity_union_invalid_fcs_record_count",
        "identity_union_invalid_length_record_count",
        "duplicate_record_count",
        "duplicate_valid_fcs_record_count",
        "duplicate_invalid_fcs_record_count",
        "duplicate_invalid_length_record_count",
        "membership_partitions",
        "configured_max_input_bytes_per_source",
        "configured_max_records_per_source",
        "interpretation",
    ];
    const SOURCE_FIELDS: [&str; 10] = [
        "source_index",
        "decoder_lane",
        "carrier_offset_hz",
        "source_format",
        "source_bytes",
        "source_sha256",
        "candidate_record_count",
        "valid_fcs_record_count",
        "invalid_fcs_record_count",
        "invalid_length_record_count",
    ];
    const MEMBERSHIP_FIELDS: [&str; 6] = [
        "source_membership_mask",
        "source_membership_count",
        "identity_record_count",
        "valid_fcs_record_count",
        "invalid_fcs_record_count",
        "invalid_length_record_count",
    ];

    ensure!(
        archives.len() == IEEE_HYPOTHESIS_COUNT && inspections.len() == IEEE_HYPOTHESIS_COUNT,
        "IEEE 802.15.4 comparison validation requires the fixed six sources"
    );
    require_exact_fields(summary, &SUMMARY_FIELDS, "IEEE 802.15.4 comparison summary")?;
    ensure!(
        report::required_u64(summary, "schema_version")? == 1
            && summary.get("operation").and_then(Value::as_str)
                == Some("compare_ieee802154_candidate_hypotheses")
            && summary.get("source_format").and_then(Value::as_str)
                == Some("ieee802154_raw_candidates_v1")
            && report::required_u64(summary, "source_count")? == IEEE_HYPOTHESIS_COUNT as u64,
        "unsupported IEEE 802.15.4 hypothesis-comparison contract"
    );
    ensure!(
        report::required_u64(summary, "ieee802154_channel")? == u64::from(expected_channel)
            && report::required_u64(summary, "channel_sample_rate_hz")? == 4_000_000
            && report::required_u64(summary, "recovered_chip_rate_hz")? == 2_000_000,
        "IEEE 802.15.4 comparison channel or sample-rate contract differs"
    );
    ensure!(
        summary
            .get("interpretation")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        "IEEE 802.15.4 comparison interpretation is missing"
    );

    let sources = summary
        .get("sources")
        .and_then(Value::as_array)
        .context("IEEE 802.15.4 comparison summary lacks sources")?;
    ensure!(
        sources.len() == IEEE_HYPOTHESIS_COUNT,
        "IEEE 802.15.4 comparison source count differs from the fixed bank"
    );
    let mut source_counts = [IeeeCounts::default(); IEEE_HYPOTHESIS_COUNT];
    let mut raw = IeeeCounts::default();
    for (index, (((source, spec), archive), inspection)) in sources
        .iter()
        .zip(IEEE_HYPOTHESES)
        .zip(archives)
        .zip(inspections)
        .enumerate()
    {
        require_exact_fields(source, &SOURCE_FIELDS, "IEEE 802.15.4 comparison source")?;
        ensure!(
            spec.source_index == index
                && report::required_u64(source, "source_index")? == index as u64
                && source.get("decoder_lane").and_then(Value::as_str) == Some(spec.decoder_lane)
                && source.get("carrier_offset_hz").and_then(Value::as_i64)
                    == Some(i64::from(spec.carrier_offset_hz)),
            "IEEE 802.15.4 comparison source descriptors differ from the fixed bank"
        );
        ensure!(
            source.get("source_format").and_then(Value::as_str)
                == Some("ieee802154_raw_candidates_v1")
                && report::required_u64(source, "source_bytes")? == archive.bytes
                && source.get("source_sha256").and_then(Value::as_str)
                    == Some(archive.sha256.as_str()),
            "IEEE 802.15.4 comparison source identity differs from its archive"
        );
        let counts = IeeeCounts {
            records: report::required_u64(source, "candidate_record_count")?,
            valid: report::required_u64(source, "valid_fcs_record_count")?,
            invalid: report::required_u64(source, "invalid_fcs_record_count")?,
            invalid_length: report::required_u64(source, "invalid_length_record_count")?,
        };
        counts.require_closed("IEEE 802.15.4 comparison source")?;
        ensure!(
            counts == ieee_inspection_counts(inspection)? && counts.records <= IEEE_MAX_FRAMES,
            "IEEE 802.15.4 comparison source totals differ from independent inspection"
        );
        source_counts[index] = counts;
        raw = raw.checked_add(counts, "IEEE 802.15.4 source total overflow")?;
    }

    let reported_raw = IeeeCounts {
        records: report::required_u64(summary, "source_candidate_record_count_sum")?,
        valid: report::required_u64(summary, "source_valid_fcs_record_count_sum")?,
        invalid: report::required_u64(summary, "source_invalid_fcs_record_count_sum")?,
        invalid_length: report::required_u64(summary, "source_invalid_length_record_count_sum")?,
    };
    let union = ieee_aggregate_counts(summary, "identity_union")?;
    let duplicate = ieee_aggregate_counts(summary, "duplicate")?;
    reported_raw.require_closed("IEEE 802.15.4 source aggregate")?;
    union.require_closed("IEEE 802.15.4 identity union")?;
    duplicate.require_closed("IEEE 802.15.4 duplicate aggregate")?;
    ensure!(
        reported_raw == raw
            && raw.checked_sub(union, "IEEE 802.15.4 duplicate underflow")? == duplicate,
        "IEEE 802.15.4 raw, union and duplicate aggregates are inconsistent"
    );

    let memberships = summary
        .get("membership_partitions")
        .and_then(Value::as_array)
        .context("IEEE 802.15.4 comparison lacks membership partitions")?;
    ensure!(
        memberships.len() < (1usize << IEEE_HYPOTHESIS_COUNT),
        "IEEE 802.15.4 comparison has too many membership partitions"
    );
    let mut prior_mask = 0u64;
    let mut partition_union = IeeeCounts::default();
    let mut weighted = IeeeCounts::default();
    let mut per_source = [IeeeCounts::default(); IEEE_HYPOTHESIS_COUNT];
    for membership in memberships {
        require_exact_fields(
            membership,
            &MEMBERSHIP_FIELDS,
            "IEEE 802.15.4 membership partition",
        )?;
        let mask = report::required_u64(membership, "source_membership_mask")?;
        ensure!(
            mask > prior_mask && mask < (1u64 << IEEE_HYPOTHESIS_COUNT),
            "IEEE 802.15.4 membership masks are not unique, sorted and in range"
        );
        prior_mask = mask;
        let popcount = u64::from(mask.count_ones());
        ensure!(
            report::required_u64(membership, "source_membership_count")? == popcount,
            "IEEE 802.15.4 membership popcount differs from its mask"
        );
        let counts = IeeeCounts {
            records: report::required_u64(membership, "identity_record_count")?,
            valid: report::required_u64(membership, "valid_fcs_record_count")?,
            invalid: report::required_u64(membership, "invalid_fcs_record_count")?,
            invalid_length: report::required_u64(membership, "invalid_length_record_count")?,
        };
        ensure!(
            counts.records > 0,
            "IEEE 802.15.4 membership partition must be nonempty"
        );
        counts.require_closed("IEEE 802.15.4 membership partition")?;
        partition_union =
            partition_union.checked_add(counts, "IEEE 802.15.4 membership union overflow")?;
        weighted = weighted.checked_add(
            counts.checked_mul(popcount, "IEEE 802.15.4 weighted membership overflow")?,
            "IEEE 802.15.4 weighted membership overflow",
        )?;
        for (source_index, source_total) in per_source.iter_mut().enumerate() {
            if mask & (1u64 << source_index) != 0 {
                *source_total =
                    source_total.checked_add(counts, "IEEE 802.15.4 source membership overflow")?;
            }
        }
    }
    ensure!(
        partition_union == union && weighted == raw && per_source == source_counts,
        "IEEE 802.15.4 membership partitions do not close over union and source totals"
    );
    ensure!(
        report::required_u64(summary, "configured_max_input_bytes_per_source")?
            == IEEE_ARCHIVE_MAX_BYTES
            && report::required_u64(summary, "configured_max_records_per_source")?
                == IEEE_MAX_FRAMES,
        "IEEE 802.15.4 comparison limits differ from the orchestrated contract"
    );
    Ok(())
}

pub(crate) fn validate_ieee_inspection(
    summary: &Value,
    expected_channel: u8,
    archive: &report::FileDigest,
) -> Result<()> {
    validate_ieee_inspection_for_archive(summary, expected_channel, archive, None)
}

fn validate_ieee_inspection_for_archive(
    summary: &Value,
    expected_channel: u8,
    archive: &report::FileDigest,
    indexed_records: Option<u64>,
) -> Result<()> {
    let schema_version = if indexed_records.is_some() { 2 } else { 1 };
    ensure!(
        report::required_u64(summary, "schema_version")? == schema_version,
        "unsupported IEEE 802.15.4 inspector schema"
    );
    ensure!(
        summary.get("operation").and_then(Value::as_str) == Some("inspect_ieee802154_candidates"),
        "unexpected IEEE 802.15.4 inspector operation"
    );
    ensure!(
        summary.get("source_format").and_then(Value::as_str)
            == Some("ieee802154_raw_candidates_v1")
            && report::required_u64(summary, "source_bytes")? == archive.bytes
            && summary.get("source_sha256").and_then(Value::as_str)
                == Some(archive.sha256.as_str()),
        "IEEE 802.15.4 inspector source identity differs from the candidate archive"
    );
    ensure!(
        (24..=IEEE_ARCHIVE_MAX_BYTES).contains(&archive.bytes),
        "IEEE 802.15.4 candidate archive extent is outside its configured bound"
    );
    ensure!(
        report::required_u64(summary, "ieee802154_channel")? == u64::from(expected_channel)
            && report::required_u64(summary, "channel_sample_rate_hz")? == 4_000_000
            && report::required_u64(summary, "recovered_chip_rate_hz")? == 2_000_000,
        "IEEE 802.15.4 inspector channel or sample-rate contract differs"
    );
    let candidates = report::required_u64(summary, "candidate_record_count")?;
    let valid = report::required_u64(summary, "valid_fcs_record_count")?;
    let invalid = report::required_u64(summary, "invalid_fcs_record_count")?;
    let invalid_length = report::required_u64(summary, "invalid_length_record_count")?;
    let classified = valid
        .checked_add(invalid)
        .and_then(|count| count.checked_add(invalid_length))
        .context("IEEE 802.15.4 inspector record partition overflow")?;
    ensure!(
        classified == candidates && candidates <= IEEE_MAX_FRAMES,
        "IEEE 802.15.4 inspector record partition differs from candidate count"
    );
    // Indexed inspection reports only owned candidates; the archive is whole.
    let archive_candidates = indexed_records.unwrap_or(candidates);
    let minimum_archive_bytes = archive_candidates
        .checked_mul(21)
        .and_then(|bytes| bytes.checked_add(24))
        .context("IEEE 802.15.4 minimum candidate archive extent overflow")?;
    let maximum_archive_bytes = archive_candidates
        .checked_mul(147)
        .and_then(|bytes| bytes.checked_add(24))
        .context("IEEE 802.15.4 maximum candidate archive extent overflow")?;
    ensure!(
        (minimum_archive_bytes..=maximum_archive_bytes).contains(&archive.bytes),
        "IEEE 802.15.4 candidate archive extent differs from its record count"
    );
    let validated_bytes = report::required_u64(summary, "validated_psdu_bytes")?;
    let minimum_validated_bytes = valid
        .checked_mul(2)
        .context("IEEE 802.15.4 minimum validated PSDU extent overflow")?;
    let maximum_validated_bytes = valid
        .checked_mul(127)
        .context("IEEE 802.15.4 maximum validated PSDU extent overflow")?;
    ensure!(
        (valid == 0) == (validated_bytes == 0)
            && (minimum_validated_bytes..=maximum_validated_bytes).contains(&validated_bytes),
        "IEEE 802.15.4 validated PSDU extent differs from valid frame count"
    );
    ensure!(
        report::required_u64(summary, "configured_max_input_bytes")? == IEEE_ARCHIVE_MAX_BYTES
            && report::required_u64(summary, "configured_max_output_bytes")?
                == INSPECTION_OUTPUT_MIB * 1024 * 1024
            && report::required_u64(summary, "configured_max_records")? == IEEE_MAX_FRAMES,
        "IEEE 802.15.4 inspector limits differ from the orchestrated contract"
    );
    let first = summary
        .get("first_recovered_chip_count_at_end")
        .context("IEEE 802.15.4 inspector lacks its first recovered-chip position")?;
    let last = summary
        .get("last_recovered_chip_count_at_end")
        .context("IEEE 802.15.4 inspector lacks its last recovered-chip position")?;
    ensure!(
        if candidates == 0 {
            first.is_null() && last.is_null()
        } else {
            first.as_u64().is_some_and(|value| value > 0)
                && match (first.as_u64(), last.as_u64()) {
                    (Some(first), Some(last)) if candidates == 1 => last == first,
                    (Some(first), Some(last)) => last > first,
                    _ => false,
                }
        },
        "IEEE 802.15.4 recovered-chip position bounds differ from candidate count"
    );
    Ok(())
}

fn require_ieee_worker_eof(
    output: &Path,
    command: &CommandReport,
    expected_lane: &str,
) -> Result<()> {
    ensure!(
        IEEE_DECODER_LANES.contains(&expected_lane),
        "unsupported expected IEEE 802.15.4 decoder lane"
    );
    ensure!(
        !command.stdout.truncated && command.stdout.observed_bytes == command.stdout.retained_bytes,
        "IEEE 802.15.4 worker stdout was not retained completely"
    );
    ensure!(
        command.stdout.retained_bytes <= 64 * 1024,
        "IEEE 802.15.4 worker completion log exceeds 64 KiB"
    );
    let bytes = fs::read(output.join(&command.stdout.path)).context("read IEEE worker stdout")?;
    ensure!(
        bytes.len() as u64 == command.stdout.retained_bytes,
        "IEEE 802.15.4 worker stdout extent changed"
    );
    let text = std::str::from_utf8(&bytes).context("IEEE worker stdout is not UTF-8")?;
    let expected_prefix = format!("Candidate archive: decoder lane {expected_lane},");
    let other_lane = if expected_lane == "hard" {
        "soft"
    } else {
        "hard"
    };
    ensure!(
        text.matches("Candidate archive:").count() == 1
            && text.matches("decoder lane").count() == 1
            && text.matches(&expected_prefix).count() == 1
            && !text.contains(&format!("decoder lane {other_lane}"))
            && text.matches("stop reason InputEof").count() == 1
            && !text.contains("stop reason FrameLimit")
            && !text.contains("stop reason OutputLimit"),
        "IEEE 802.15.4 worker did not bind the expected lane and input-EOF completion"
    );
    Ok(())
}

pub(crate) fn validate_channelize(
    summary: &Value,
    source: &SourceReport,
    output: &Path,
) -> Result<IeeeChannelization> {
    validate_channelize_for(summary, source, output, IEEE_CHANNELS)
}

fn validate_channelize_for<const N: usize>(
    summary: &Value,
    source: &SourceReport,
    output: &Path,
    requested_channels: [u8; N],
) -> Result<IeeeChannelization> {
    ensure!(
        report::required_u64(summary, "schema_version")? == 1,
        "unsupported IEEE 802.15.4 channelizer schema"
    );
    ensure!(
        summary.get("operation").and_then(Value::as_str)
            == Some("offline_ieee802154_channelization"),
        "unexpected IEEE 802.15.4 channelizer operation"
    );
    let source_summary = summary
        .get("source")
        .context("IEEE 802.15.4 channelizer summary lacks source")?;
    ensure!(
        report::required_u64(source_summary, "available_complex_samples")?
            == source.complex_samples
            && report::required_u64(source_summary, "processed_complex_samples")?
                == source.complex_samples
            && source_summary
                .get("processed_full_input")
                .and_then(Value::as_bool)
                == Some(true),
        "IEEE 802.15.4 channelizer did not report the complete validated input extent"
    );
    ensure!(
        report::required_u64(source_summary, "center_frequency_hz")? == source.center_frequency_hz
            && report::required_u64(source_summary, "sample_rate_hz")? == source.sample_rate_hz,
        "IEEE 802.15.4 channelizer frequency contract differs from the validated input"
    );
    ensure!(
        report::required_u64(source_summary, "source_bytes")? == source.bytes
            && report::required_u64(source_summary, "processed_prefix_bytes")? == source.bytes
            && source_summary
                .get("processed_prefix_sha256")
                .and_then(Value::as_str)
                == Some(source.sha256.as_str()),
        "IEEE 802.15.4 channelizer source extent or digest differs from the validated input"
    );
    ensure!(
        summary
            .pointer("/filter/decimation")
            .and_then(Value::as_u64)
            == Some(5),
        "IEEE 802.15.4 channelizer did not report the reviewed decimation-by-five filter"
    );
    let channels = summary
        .get("channels")
        .and_then(Value::as_array)
        .context("IEEE 802.15.4 channelizer summary lacks channels")?;
    ensure!(
        channels.len() == requested_channels.len(),
        "IEEE 802.15.4 channelizer did not report all requested channels"
    );
    let includes_channel_19 = requested_channels.contains(&19);
    const STOPBAND_EDGE_HZ: u64 = 2_000_000;
    if includes_channel_19 {
        ensure!(
            source.center_frequency_hz == 2_437_000_000
                && source.sample_rate_hz == 20_000_000
                && summary
                    .pointer("/filter/passband_edge_hz")
                    .and_then(Value::as_u64)
                    == Some(1_500_000)
                && summary
                    .pointer("/filter/stopband_edge_hz")
                    .and_then(Value::as_u64)
                    == Some(STOPBAND_EDGE_HZ),
            "IEEE 802.15.4 Nyquist-edge plan differs from the reviewed source or filter"
        );
    }
    let mut maximum_bytes = 0u64;
    let mut pending_inputs = Vec::with_capacity(requested_channels.len());
    for expected_channel in requested_channels {
        let matches = channels
            .iter()
            .filter(|value| {
                value.get("channel").and_then(Value::as_u64) == Some(u64::from(expected_channel))
            })
            .collect::<Vec<_>>();
        ensure!(
            matches.len() == 1,
            "IEEE 802.15.4 channelizer did not report channel {expected_channel} exactly once"
        );
        let channel = matches[0];
        if includes_channel_19 {
            let raster_index = expected_channel
                .checked_sub(11)
                .context("IEEE 802.15.4 channel precedes the 2.4 GHz raster")?;
            let center_hz = 2_405_000_000 + u64::from(raster_index) * 5_000_000;
            let offset_hz = i64::try_from(center_hz)? - i64::try_from(source.center_frequency_hz)?;
            let margin = (source.sample_rate_hz / 2)
                .checked_sub(offset_hz.unsigned_abs() + STOPBAND_EDGE_HZ)
                .context("IEEE 802.15.4 channel falls outside the Nyquist-edge plan")?;
            ensure!(
                channel["center_frequency_hz"].as_u64() == Some(center_hz)
                    && channel["source_offset_hz"].as_i64() == Some(offset_hz)
                    && channel["source_edge_margin_hz"].as_u64() == Some(margin),
                "IEEE 802.15.4 channel {expected_channel} has inconsistent Nyquist-edge geometry"
            );
        }
        ensure!(
            channel.get("output_format").and_then(Value::as_str) == Some("CF32_LE")
                && report::required_u64(channel, "output_sample_rate_hz")? == 4_000_000,
            "IEEE 802.15.4 channel {expected_channel} has the wrong derived format"
        );
        let samples = report::required_u64(channel, "output_complex_samples")?;
        ensure!(
            samples == source.complex_samples / 5,
            "IEEE 802.15.4 channel {expected_channel} output sample count is not exactly source/5"
        );
        let bytes = samples
            .checked_mul(8)
            .context("IEEE 802.15.4 derived input byte count overflow")?;
        let expected_name = format!("channel-{expected_channel}.cf32");
        ensure!(
            channel.get("output_file").and_then(Value::as_str) == Some(expected_name.as_str()),
            "IEEE 802.15.4 channel {expected_channel} has an unexpected output name"
        );
        maximum_bytes = maximum_bytes.max(bytes);
        pending_inputs.push(PendingIeeeChannelInput {
            channel: expected_channel,
            path: output.join(expected_name),
            bytes,
            expected_sha256: channel
                .get("sha256")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    let hash_results = parallel_ieee_integrity_tasks(
        IeeeIntegrityPhase::InputHash,
        requested_channels,
        |channel| {
            let pending = pending_inputs
                .iter()
                .find(|input| input.channel == channel)
                .with_context(|| {
                    format!("missing pending IEEE 802.15.4 channel {channel} input identity")
                })?;
            let (digest, snapshot) =
                report::hash_direct_regular_file_with_snapshot(&pending.path, pending.bytes)?;
            ensure!(
                digest.bytes == pending.bytes
                    && pending.expected_sha256.as_deref() == Some(digest.sha256.as_str()),
                "IEEE 802.15.4 channel {channel} file identity differs from its terminal metadata"
            );
            Ok(IeeeChannelInput {
                channel,
                digest,
                path: pending.path.clone(),
                snapshot,
            })
        },
    );
    // Every hash worker has already been joined. Interpret failures only now,
    // in fixed channel order, so an early error cannot bypass a later join.
    let mut inputs = Vec::with_capacity(requested_channels.len());
    for result in hash_results {
        inputs.push(result?);
    }
    Ok(IeeeChannelization {
        worker_input_mib: maximum_bytes.div_ceil(1024 * 1024).max(1),
        channels: inputs,
    })
}

fn require_file_digest(
    path: &Path,
    maximum_bytes: u64,
    expected: &report::FileDigest,
    description: &str,
) -> Result<report::FileDigest> {
    let observed = report::hash_direct_regular_file(path, maximum_bytes)
        .with_context(|| format!("hash {description}"))?;
    ensure!(
        &observed == expected,
        "{description} content identity changed"
    );
    Ok(observed)
}

fn ieee_worker_arguments(
    channel_input: &Path,
    archive: &Path,
    channel: u8,
    worker_input_mib: u64,
    spec: IeeeHypothesisSpec,
) -> Vec<OsString> {
    vec![
        path(channel_input),
        text("--output"),
        path(archive),
        text("--channel"),
        text(channel),
        text("--max-input-mib"),
        text(worker_input_mib),
        text("--max-frames"),
        text(IEEE_MAX_FRAMES),
        text("--max-output-mib"),
        text(IEEE_ARCHIVE_MIB),
        text("--decoder-lane"),
        text(spec.decoder_lane),
        text("--carrier-offset-hz"),
        text(spec.carrier_offset_hz),
    ]
}

fn ieee_comparison_arguments(
    channel: u8,
    protocol_root: &Path,
    comparison_dir: &Path,
) -> Vec<OsString> {
    let mut arguments = vec![text("compare-hypotheses")];
    for spec in IEEE_HYPOTHESES {
        arguments.push(text("--source"));
        arguments.push(text(spec.decoder_lane));
        arguments.push(text(spec.carrier_offset_hz));
        arguments.push(path(&protocol_root.join(spec.archive_name(channel))));
    }
    arguments.extend([
        text("--output"),
        path(comparison_dir),
        text("--max-input-mib"),
        text(IEEE_ARCHIVE_MIB),
        text("--max-records"),
        text(IEEE_MAX_FRAMES),
    ]);
    arguments
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep child budgets, source ownership and evidence sinks explicit per hypothesis"
)]
fn run_ieee_hypothesis(
    monitor: &ProgramProvenance,
    worker_program: &ProgramProvenance,
    spec: IeeeHypothesisSpec,
    channel_input: &IeeeChannelInput,
    worker_input_mib: u64,
    executor_threads: Option<u8>,
    ownership: Option<ieee_nominal::ChannelOwnership>,
    protocol_root: &Path,
    output: &Path,
    log_limit: u64,
    child_timeout: Duration,
    stop: &AtomicBool,
    commands: &mut Vec<CommandReport>,
    interrupted: &mut bool,
) -> Result<IeeeHypothesisEvidence> {
    let channel = channel_input.channel;
    ensure!(
        IEEE_HYPOTHESES.get(spec.source_index) == Some(&spec)
            && IEEE_DECODER_LANES.contains(&spec.decoder_lane),
        "unsupported IEEE 802.15.4 hypothesis descriptor"
    );
    channel_input.verify_unchanged()?;

    let archive = protocol_root.join(spec.archive_name(channel));
    let source_index = ownership
        .map(|ownership| ownership.index_for(&archive, spec, channel_input))
        .transpose()?;
    let mut worker_arguments = ieee_worker_arguments(
        &channel_input.path,
        &archive,
        channel,
        worker_input_mib,
        spec,
    );
    if let Some(count) = executor_threads {
        worker_arguments.extend([text("--executor-threads"), text(count)]);
    }
    if let Some(index) = &source_index {
        index.add_worker_arguments(&mut worker_arguments);
    }
    let worker = process::run_with_temp_directory(
        worker_program,
        &worker_arguments,
        output,
        &format!("ieee-worker-{channel}-source-{}", spec.source_index),
        log_limit,
        child_timeout,
        protocol_root,
        stop,
    )?;
    *interrupted |= worker.interrupted;
    let worker_result = (|| -> Result<()> {
        channel_input.verify_unchanged()?;
        process::require_success(&worker)?;
        require_ieee_worker_eof(output, &worker, spec.decoder_lane)
    })();
    commands.push(worker);
    worker_result.with_context(|| {
        format!(
            "channel {channel} source {} ({}/{:+} Hz) worker",
            spec.source_index, spec.decoder_lane, spec.carrier_offset_hz
        )
    })?;

    let archive_before = report::hash_direct_regular_file(&archive, IEEE_ARCHIVE_MAX_BYTES)
        .with_context(|| {
            format!(
                "hash channel {channel} source {} archive before inspection",
                spec.source_index
            )
        })?;
    let inspection = protocol_root.join(spec.inspection_name(channel));
    let mut inspect_arguments = vec![
        text("inspect-candidates"),
        path(&archive),
        text("--output"),
        path(&inspection),
        text("--max-input-mib"),
        text(IEEE_ARCHIVE_MIB),
        text("--max-output-mib"),
        text(INSPECTION_OUTPUT_MIB),
        text("--max-records"),
        text(IEEE_MAX_FRAMES),
    ];
    let source_index_before = source_index
        .map(|index| {
            let digest = index.digest()?;
            index.add_inspection_arguments(&mut inspect_arguments);
            Ok::<_, anyhow::Error>((index, digest))
        })
        .transpose()?;
    let inspect = process::run(
        monitor,
        &inspect_arguments,
        output,
        &format!("ieee-inspect-{channel}-source-{}", spec.source_index),
        log_limit,
        child_timeout,
        stop,
    )?;
    *interrupted |= inspect.interrupted;
    let inspect_result = (|| -> Result<report::FileDigest> {
        let archive_after = require_file_digest(
            &archive,
            IEEE_ARCHIVE_MAX_BYTES,
            &archive_before,
            &format!(
                "IEEE 802.15.4 channel {channel} source {} candidate archive",
                spec.source_index
            ),
        )?;
        process::require_success(&inspect)?;
        Ok(archive_after)
    })();
    commands.push(inspect);
    let archive_after = inspect_result
        .with_context(|| format!("channel {channel} source {} inspector", spec.source_index))?;
    let (inspection_summary, inspection_sha256) =
        report::read_json_with_sha256(&inspection.join("summary.json"))?;
    let source_index = source_index_before
        .map(|(index, digest)| index.verify_inspection(&inspection_summary, &digest))
        .transpose()?;
    validate_ieee_inspection_for_archive(
        &inspection_summary,
        channel,
        &archive_after,
        source_index
            .as_ref()
            .map(|index| index.indexed_record_count),
    )?;
    Ok(IeeeHypothesisEvidence {
        spec,
        archive: archive_after,
        inspection: inspection_summary,
        inspection_sha256,
        source_index,
    })
}

fn run_ieee_channel(
    programs: &Programs,
    channel_input: &IeeeChannelInput,
    worker_input_mib: u64,
    protocol_root: &Path,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> IeeeChannelRun {
    let channel = channel_input.channel;
    if stop.load(Ordering::Relaxed) {
        return IeeeChannelRun::interrupted(channel);
    }

    let mut commands = Vec::new();
    let mut errors = Vec::new();
    let mut interrupted = false;
    let mut sources = Vec::with_capacity(IEEE_HYPOTHESIS_COUNT);
    for spec in IEEE_HYPOTHESES {
        if stop.load(Ordering::Relaxed) {
            interrupted = true;
            errors.push(format!(
                "interrupted before IEEE 802.15.4 channel {channel} source {} worker",
                spec.source_index
            ));
            break;
        }
        match run_ieee_hypothesis(
            &programs.ieee,
            &programs.ieee_worker,
            spec,
            channel_input,
            worker_input_mib,
            None,
            None,
            protocol_root,
            output,
            settings.retained_log_limit_bytes_per_stream,
            Duration::from_secs(settings.child_timeout_seconds),
            stop,
            &mut commands,
            &mut interrupted,
        ) {
            Ok(evidence) => sources.push(evidence),
            Err(error) => {
                errors.push(format!(
                    "channel {channel} source {} hypothesis analysis: {error:#}",
                    spec.source_index
                ));
                break;
            }
        }
    }
    if interrupted || sources.len() != IEEE_HYPOTHESIS_COUNT {
        return IeeeChannelRun {
            channel,
            commands,
            evidence: None,
            errors,
            interrupted,
        };
    }

    if !sources
        .iter()
        .zip(IEEE_HYPOTHESES)
        .all(|(evidence, expected)| evidence.spec == expected)
    {
        errors.push(format!(
            "channel {channel} hypothesis roles were not retained in fixed order"
        ));
        return IeeeChannelRun {
            channel,
            commands,
            evidence: None,
            errors,
            interrupted,
        };
    }
    for source in &sources {
        let archive_path = protocol_root.join(source.spec.archive_name(channel));
        if let Err(error) = require_file_digest(
            &archive_path,
            IEEE_ARCHIVE_MAX_BYTES,
            &source.archive,
            &format!(
                "IEEE 802.15.4 channel {channel} source {} archive before comparison",
                source.spec.source_index
            ),
        ) {
            errors.push(format!(
                "channel {channel} source {} archive before comparison: {error:#}",
                source.spec.source_index
            ));
            return IeeeChannelRun {
                channel,
                commands,
                evidence: None,
                errors,
                interrupted,
            };
        }
    }

    let comparison_dir = protocol_root.join(format!("channel-{channel}-comparison"));
    let comparison_arguments = ieee_comparison_arguments(channel, protocol_root, &comparison_dir);
    let comparison_command = match run_one(
        &programs.ieee,
        comparison_arguments,
        output,
        &format!("ieee-compare-{channel}"),
        settings,
        stop,
    ) {
        Ok(command) => command,
        Err(error) => {
            errors.push(format!(
                "channel {channel} hypothesis comparison: {error:#}"
            ));
            return IeeeChannelRun {
                channel,
                commands,
                evidence: None,
                errors,
                interrupted,
            };
        }
    };
    interrupted |= comparison_command.interrupted;
    let comparison_result = (|| -> Result<(Value, String)> {
        for source in &sources {
            require_file_digest(
                &protocol_root.join(source.spec.archive_name(channel)),
                IEEE_ARCHIVE_MAX_BYTES,
                &source.archive,
                &format!(
                    "IEEE 802.15.4 channel {channel} source {} archive after comparison",
                    source.spec.source_index
                ),
            )?;
        }
        process::require_success(&comparison_command)?;
        let result = report::read_json_with_sha256(&comparison_dir.join("summary.json"))?;
        let archives = sources
            .iter()
            .map(|source| source.archive.clone())
            .collect::<Vec<_>>();
        let inspections = sources
            .iter()
            .map(|source| source.inspection.clone())
            .collect::<Vec<_>>();
        validate_ieee_comparison(&result.0, channel, &archives, &inspections)?;
        Ok(result)
    })();
    commands.push(comparison_command);
    let evidence = match comparison_result {
        Ok((comparison, comparison_sha256)) => Some(IeeeChannelEvidence {
            channel,
            sources,
            comparison,
            comparison_sha256,
        }),
        Err(error) => {
            errors.push(format!(
                "channel {channel} hypothesis comparison: {error:#}"
            ));
            None
        }
    };
    IeeeChannelRun {
        channel,
        commands,
        evidence,
        errors,
        interrupted,
    }
}

fn validate_terminal_ieee_channel(
    evidence: &IeeeChannelEvidence,
    channelization: &IeeeChannelization,
    protocol_root: &Path,
) -> Result<()> {
    channelization
        .channel(evidence.channel)?
        .verify_unchanged()?;
    ensure!(
        evidence.sources.len() == IEEE_HYPOTHESIS_COUNT
            && evidence
                .sources
                .iter()
                .zip(IEEE_HYPOTHESES)
                .all(|(source, expected)| source.spec == expected),
        "IEEE 802.15.4 terminal evidence does not retain the fixed source order"
    );
    let mut archives = Vec::with_capacity(IEEE_HYPOTHESIS_COUNT);
    let mut inspections = Vec::with_capacity(IEEE_HYPOTHESIS_COUNT);
    for source in &evidence.sources {
        let archive_path = protocol_root.join(source.spec.archive_name(evidence.channel));
        let archive = require_file_digest(
            &archive_path,
            IEEE_ARCHIVE_MAX_BYTES,
            &source.archive,
            &format!(
                "IEEE 802.15.4 channel {} terminal source {} archive",
                evidence.channel, source.spec.source_index
            ),
        )?;
        let inspection_path = protocol_root
            .join(source.spec.inspection_name(evidence.channel))
            .join("summary.json");
        let (inspection, inspection_sha256) = report::read_json_with_sha256(&inspection_path)?;
        ensure!(
            inspection_sha256 == source.inspection_sha256,
            "IEEE 802.15.4 channel {} source {} inspection summary changed",
            evidence.channel,
            source.spec.source_index
        );
        validate_ieee_inspection(&inspection, evidence.channel, &archive)?;
        ensure!(
            inspection == source.inspection,
            "IEEE 802.15.4 channel {} source {} inspection value changed",
            evidence.channel,
            source.spec.source_index
        );
        archives.push(archive);
        inspections.push(inspection);
    }
    let comparison_path = protocol_root.join(format!(
        "channel-{}-comparison/summary.json",
        evidence.channel
    ));
    let (comparison, comparison_sha256) = report::read_json_with_sha256(&comparison_path)?;
    ensure!(
        comparison_sha256 == evidence.comparison_sha256 && comparison == evidence.comparison,
        "IEEE 802.15.4 channel {} comparison summary changed",
        evidence.channel
    );
    validate_ieee_comparison(&comparison, evidence.channel, &archives, &inspections)
}

fn reduce_ieee_integrity_result<T>(
    phase: IeeeIntegrityPhase,
    expected_channel: u8,
    result: BranchResult<(u8, Result<T>)>,
) -> Result<T> {
    let label = phase.failure_label();
    match result {
        BranchResult::Complete((channel, result)) if channel == expected_channel => result,
        BranchResult::Complete(_) => Err(anyhow::anyhow!(
            "IEEE 802.15.4 channel {expected_channel} {label} worker returned wrong channel"
        )),
        BranchResult::SpawnFailed => Err(anyhow::anyhow!(
            "could not start IEEE 802.15.4 channel {expected_channel} {label} worker"
        )),
        BranchResult::Panicked => Err(anyhow::anyhow!(
            "IEEE 802.15.4 channel {expected_channel} {label} worker panicked"
        )),
    }
}

fn parallel_ieee_integrity_tasks<T, F, const N: usize>(
    phase: IeeeIntegrityPhase,
    channels: [u8; N],
    run: F,
) -> [Result<T>; N]
where
    T: Send,
    F: Fn(u8) -> Result<T> + Sync,
{
    thread::scope(|scope| {
        let run = &run;
        let branches = channels.map(|channel| {
            let worker = thread::Builder::new()
                .name(phase.thread_name(channel))
                .spawn_scoped(scope, move || (channel, run(channel)));
            (channel, worker)
        });
        // Map every branch before returning so errors cannot skip a join.
        branches.map(|(channel, worker)| {
            reduce_ieee_integrity_result(phase, channel, finish_branch(worker))
        })
    })
}

fn reduce_ieee_channel_result(
    expected_channel: u8,
    result: BranchResult<IeeeChannelRun>,
) -> IeeeChannelRun {
    match result {
        BranchResult::Complete(run) if run.channel == expected_channel => run,
        BranchResult::Complete(_) => IeeeChannelRun::failed(
            expected_channel,
            format!(
                "IEEE 802.15.4 channel {expected_channel} worker returned an unexpected channel"
            ),
        ),
        BranchResult::SpawnFailed => IeeeChannelRun::failed(
            expected_channel,
            format!("could not start IEEE 802.15.4 channel {expected_channel} worker"),
        ),
        BranchResult::Panicked => IeeeChannelRun::failed(
            expected_channel,
            format!("IEEE 802.15.4 channel {expected_channel} worker panicked"),
        ),
    }
}

fn reduce_ieee_channel_results(results: [BranchResult<IeeeChannelRun>; 3]) -> [IeeeChannelRun; 3] {
    let [channel_16, channel_17, channel_18] = results;
    [
        reduce_ieee_channel_result(IEEE_CHANNELS[0], channel_16),
        reduce_ieee_channel_result(IEEE_CHANNELS[1], channel_17),
        reduce_ieee_channel_result(IEEE_CHANNELS[2], channel_18),
    ]
}

fn parallel_ieee_channels(
    stop: &AtomicBool,
    run: impl Fn(u8) -> IeeeChannelRun + Sync,
) -> [IeeeChannelRun; 3] {
    if stop.load(Ordering::Relaxed) {
        return IEEE_CHANNELS.map(IeeeChannelRun::interrupted);
    }
    thread::scope(|scope| {
        let run = &run;
        let branches = IEEE_CHANNELS.map(|channel| {
            let name = format!("ieee802154-channel-{channel}");
            thread::Builder::new()
                .name(name)
                .spawn_scoped(scope, move || run(channel))
        });
        let [channel_16, channel_17, channel_18] = branches;
        // Finish every successfully spawned worker before reducing any result,
        // so a failure cannot detach a sibling or bypass its process cleanup.
        let results = [
            finish_branch(channel_16),
            finish_branch(channel_17),
            finish_branch(channel_18),
        ];
        reduce_ieee_channel_results(results)
    })
}

fn ieee802154(
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    const NAME: &str = "ieee802154";
    let protocol_root = output.join("ieee802154");
    if let Err(error) = report::private_directory(&protocol_root) {
        return failed(NAME, StageState::Failed, Vec::new(), error);
    }
    let channelized = protocol_root.join("channelized");
    let channelize_arguments =
        ieee_channelize_arguments(source, &channelized, settings.max_samples, &IEEE_CHANNELS);
    let channelize = match run_one(
        &programs.ieee,
        channelize_arguments,
        output,
        "ieee-channelize",
        settings,
        stop,
    ) {
        Ok(command) => command,
        Err(error) => return failed(NAME, StageState::Failed, Vec::new(), error),
    };
    let state = command_state(&channelize);
    let mut commands = vec![channelize];
    if let Err(error) = process::require_success(&commands[0]) {
        return failed(NAME, state, commands, error);
    }
    let channelize_path = channelized.join("channelize.json");
    let (channelize_summary, channelize_summary_sha256) =
        match report::read_json_with_sha256(&channelize_path) {
            Ok(value) => value,
            Err(error) => return failed(NAME, StageState::Failed, commands, error),
        };
    let channelization = match validate_channelize(&channelize_summary, source, &channelized) {
        Ok(value) => value,
        Err(error) => return failed(NAME, StageState::Failed, commands, error),
    };
    let worker_input_mib = channelization.worker_input_mib;
    if worker_input_mib > 8192 {
        return failed(
            NAME,
            StageState::Failed,
            commands,
            "IEEE 802.15.4 derived input exceeds the worker's 8192 MiB limit",
        );
    }
    let mut channel_evidence = Vec::new();
    let mut errors = Vec::new();
    let mut interrupted = false;
    let channel_runs = parallel_ieee_channels(stop, |channel| {
        let channel_input = match channelization.channel(channel) {
            Ok(input) => input,
            Err(error) => {
                return IeeeChannelRun::failed(
                    channel,
                    format!("channel {channel} input identity: {error:#}"),
                );
            }
        };
        run_ieee_channel(
            programs,
            channel_input,
            worker_input_mib,
            &protocol_root,
            output,
            settings,
            stop,
        )
    });
    for mut run in channel_runs {
        commands.append(&mut run.commands);
        interrupted |= run.interrupted;
        errors.append(&mut run.errors);
        if let Some(evidence) = run.evidence {
            channel_evidence.push(evidence);
        }
    }
    if interrupted {
        return failed(NAME, StageState::Interrupted, commands, errors.join("; "));
    }
    if !errors.is_empty() || channel_evidence.len() != IEEE_CHANNELS.len() {
        return failed(
            NAME,
            StageState::Failed,
            commands,
            if errors.is_empty() {
                "not every IEEE 802.15.4 channel produced terminal six-hypothesis evidence"
                    .to_owned()
            } else {
                errors.join("; ")
            },
        );
    }
    let terminal_manifest = (|| -> Result<()> {
        let (_, terminal_sha256) = report::read_json_with_sha256(&channelize_path)?;
        ensure!(
            terminal_sha256 == channelize_summary_sha256,
            "IEEE 802.15.4 channelizer summary changed during downstream analysis"
        );
        channelization.verify_unchanged()
    })();
    if let Err(error) = terminal_manifest {
        return failed(NAME, StageState::Failed, commands, error);
    }
    let terminal_results =
        parallel_ieee_integrity_tasks(IeeeIntegrityPhase::Terminal, IEEE_CHANNELS, |channel| {
            let evidence = channel_evidence
                .iter()
                .find(|evidence| evidence.channel == channel)
                .with_context(|| {
                    format!("missing IEEE 802.15.4 channel {channel} terminal evidence")
                })?;
            validate_terminal_ieee_channel(evidence, &channelization, &protocol_root)
        });
    // All terminal workers have already joined. Keep failure selection in fixed
    // channel order, matching the prior sequential validation behavior.
    let terminal_evidence = (|| -> Result<()> {
        for result in terminal_results {
            result?;
        }
        Ok(())
    })();
    if let Err(error) = terminal_evidence {
        return failed(NAME, StageState::Failed, commands, error);
    }
    let comparisons = channel_evidence
        .iter()
        .map(|evidence| evidence.comparison.clone())
        .collect::<Vec<_>>();
    match ieee_comparison_outcome(&comparisons) {
        Ok(outcome) => completed(NAME, outcome, commands),
        Err(error) => failed(NAME, StageState::Failed, commands, error),
    }
}

fn ieee_channelize_arguments(
    source: &SourceReport,
    output: &Path,
    max_samples: u64,
    channels: &[u8],
) -> Vec<OsString> {
    vec![
        text("channelize"),
        path(&source.input),
        text("--output"),
        path(output),
        text("--channels"),
        text(
            channels
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ),
        text("--max-samples"),
        text(max_samples),
    ]
}

pub(crate) fn bluetooth_outcome(summary: &Value, source: &SourceReport) -> Result<Outcome> {
    let schema_version = report::required_u64(summary, "schema_version")?;
    ensure!(
        matches!(schema_version, 1 | 2),
        "unsupported Bluetooth replay schema"
    );
    if schema_version == 2 {
        ensure!(
            summary.get("coded_scan_requested").and_then(Value::as_bool) == Some(false),
            "routine Bluetooth replay must not request coded scanning"
        );
        let primary_frequencies = summary
            .get("coded_scan_primary_frequencies_mhz")
            .and_then(Value::as_array)
            .context("Bluetooth replay lacks a coded-scan frequency list")?;
        ensure!(
            primary_frequencies.is_empty(),
            "routine Bluetooth replay must not report coded-scan frequencies"
        );
    }
    ensure!(
        summary.get("processed_full_input").and_then(Value::as_bool) == Some(true),
        "Bluetooth replay did not process the full input"
    );
    ensure!(
        report::required_u64(summary, "input_complex_samples")? == source.complex_samples,
        "Bluetooth replay extent differs from the validated input"
    );
    ensure!(
        report::required_u64(summary, "decoder_processed_complex_samples")?
            == source.complex_samples,
        "Bluetooth decoder did not report consuming the complete validated input extent"
    );
    let channelizer_step = report::required_u64(summary, "channelizer_step_complex_samples")?;
    let decoder_block = report::required_u64(summary, "decoder_file_block_complex_samples")?;
    let bluetooth_channels = source.sample_rate_hz / 1_000_000;
    ensure!(
        bluetooth_channels >= 2 && bluetooth_channels.is_multiple_of(2),
        "validated sample rate does not produce a supported even Bluetooth channel count"
    );
    let expected_step = bluetooth_channels / 2;
    ensure!(
        channelizer_step == expected_step
            && source.complex_samples.is_multiple_of(channelizer_step)
            && decoder_block == 65_530
            && decoder_block.is_multiple_of(channelizer_step),
        "Bluetooth decoder block contract is not aligned to complete channelizer steps"
    );
    ensure!(
        report::required_u64(summary, "channelizer_steps_processed")?
            == source.complex_samples / channelizer_step,
        "Bluetooth decoder channelizer-step count differs from the full input extent"
    );
    for field in ["radio_opened", "network_requested", "transmit_requested"] {
        ensure!(
            summary.get(field).and_then(Value::as_bool) == Some(false),
            "Bluetooth replay terminal summary has unsafe {field} state"
        );
    }
    let candidates = report::required_u64(summary, "candidate_records")?;
    let valid = report::required_u64(summary, "validated_ble_packets")?;
    ensure!(
        valid <= candidates,
        "Bluetooth validated BLE packet count exceeds candidates"
    );
    let outcome = summary
        .get("outcome")
        .and_then(Value::as_str)
        .context("Bluetooth replay terminal summary lacks outcome")?;
    match outcome {
        "completed_with_validated_ble" => {
            ensure!(
                candidates > 0 && valid > 0,
                "validated BLE outcome has inconsistent candidate counts"
            );
            Ok(Outcome::EvidenceFound)
        }
        "completed_candidates_without_validated_ble" => {
            ensure!(
                candidates > 0 && valid == 0,
                "Bluetooth candidate-only outcome has inconsistent counts"
            );
            Ok(Outcome::CandidatesWithoutValidFrames)
        }
        "completed_zero_candidates" => {
            ensure!(
                candidates == 0 && valid == 0,
                "Bluetooth zero-candidate outcome has inconsistent counts"
            );
            Ok(Outcome::CompletedZeroCandidates)
        }
        _ => anyhow::bail!("unknown Bluetooth replay outcome {outcome}"),
    }
}

fn exact_f64(value: &Value, field: &str, expected: f64) -> Result<()> {
    let observed = value
        .get(field)
        .and_then(Value::as_f64)
        .with_context(|| format!("burst-family settings lack number {field}"))?;
    ensure!(observed == expected, "burst-family setting {field} differs");
    Ok(())
}

fn validate_numeric_summary(
    value: &Value,
    name: &str,
    range: Option<(f64, f64)>,
    integral: bool,
) -> Result<u64> {
    let object = value
        .as_object()
        .with_context(|| format!("burst-family feature {name} is not an object"))?;
    ensure!(
        object.len() == 4
            && ["observations", "min", "max", "mean"]
                .into_iter()
                .all(|field| object.contains_key(field)),
        "burst-family feature {name} has a different schema"
    );
    let observations = report::required_u64(value, "observations")?;
    if observations == 0 {
        ensure!(
            ["min", "max", "mean"]
                .into_iter()
                .all(|field| value.get(field).is_some_and(Value::is_null)),
            "empty burst-family feature {name} has numeric extrema"
        );
        return Ok(0);
    }
    let read = |field: &str| -> Result<f64> {
        let number = value
            .get(field)
            .and_then(Value::as_f64)
            .with_context(|| format!("burst-family feature {name} lacks number {field}"))?;
        ensure!(
            number.is_finite(),
            "burst-family feature {name} is not finite"
        );
        if let Some((minimum, maximum)) = range {
            ensure!(
                (minimum..=maximum).contains(&number),
                "burst-family feature {name} is outside its natural range"
            );
        }
        if integral && field != "mean" {
            ensure!(
                number.fract() == 0.0,
                "burst-family feature {name} must be integral"
            );
        }
        Ok(number)
    };
    let minimum = read("min")?;
    let maximum = read("max")?;
    let mean = read("mean")?;
    ensure!(
        minimum <= mean && mean <= maximum,
        "burst-family feature {name} has inconsistent extrema"
    );
    ensure!(
        observations != 1 || (minimum == maximum && maximum == mean),
        "single-observation burst-family feature {name} has differing extrema"
    );
    Ok(observations)
}

pub(crate) fn burst_family_outcome(
    summary: &Value,
    bursts: &Value,
    archive: &report::FileDigest,
) -> Result<Outcome> {
    let top = summary
        .as_object()
        .context("burst-family summary is not an object")?;
    ensure!(
        top.len() == 10
            && [
                "schema_version",
                "operation",
                "source",
                "accounting",
                "settings",
                "families",
                "frequency_family_counts",
                "eligible_record_features",
                "protocol_identity_established",
                "interpretation",
            ]
            .into_iter()
            .all(|field| top.contains_key(field))
            && summary
                .get("interpretation")
                .and_then(Value::as_str)
                .is_some(),
        "burst-family top-level schema differs"
    );
    ensure!(
        report::required_u64(summary, "schema_version")? == 2
            && summary.get("operation").and_then(Value::as_str) == Some("classify_bursts"),
        "unsupported burst-family classifier contract"
    );
    ensure!(
        summary
            .get("protocol_identity_established")
            .and_then(Value::as_bool)
            == Some(false),
        "burst-family output claims protocol identity"
    );
    let source = summary
        .get("source")
        .context("burst-family summary lacks source identity")?;
    ensure!(
        source.as_object().is_some_and(|object| {
            object.len() == 3
                && ["format", "bytes", "sha256"]
                    .into_iter()
                    .all(|field| object.contains_key(field))
        }) && source.get("format").and_then(Value::as_str) == Some("blue_dragon_burst_v1")
            && report::required_u64(source, "bytes")? == archive.bytes
            && source.get("sha256").and_then(Value::as_str) == Some(archive.sha256.as_str()),
        "burst-family source identity differs from the retained archive"
    );

    let settings = summary
        .get("settings")
        .and_then(Value::as_object)
        .context("burst-family summary lacks settings")?;
    ensure!(settings.len() == 19, "burst-family settings schema differs");
    for (field, expected) in [
        ("channel_sample_rate_hz", 2_000_000),
        ("sample_period_ns", 500),
        ("group_connectivity_gap_ns", 50_000),
        ("group_frequency_step_mhz", 1),
        ("active_power_divisor", 16),
        ("minimum_usable_phase_differences", 128),
        ("minimum_transitions", 6),
        ("kmeans_iterations", 8),
        ("adjacent_multibin_minimum_width", 2),
        ("wideband_multibin_minimum_width", 4),
    ] {
        ensure!(
            report::required_u64(summary.get("settings").expect("checked settings"), field)?
                == expected,
            "burst-family setting {field} differs"
        );
    }
    ensure!(
        settings
            .get("simultaneous_interval")
            .and_then(Value::as_str)
            == Some("half_open_unexpanded_record_interval")
            && settings
                .get("active_power_quantile")
                .and_then(Value::as_array)
                == Some(&vec![Value::from(4), Value::from(5)])
            && settings.get("separation_hz").and_then(Value::as_array)
                == Some(&vec![Value::from(40_000.0), Value::from(900_000.0)]),
        "burst-family interval or array settings differ"
    );
    let settings_value = summary.get("settings").expect("checked settings");
    for (field, expected) in [
        ("maximum_envelope_cv", 0.25),
        ("minimum_cluster_balance", 0.15),
        ("minimum_variance_reduction", 0.60),
        ("minimum_mode_contrast", 4.0),
        ("mode_contrast_std_floor_hz", 1.0),
        ("minimum_phase_concentration", 0.05),
    ] {
        exact_f64(settings_value, field, expected)?;
    }

    let accounting = summary
        .get("accounting")
        .and_then(Value::as_object)
        .context("burst-family summary lacks accounting")?;
    ensure!(
        accounting.len() == 10,
        "burst-family accounting schema differs"
    );
    let accounting_value = summary.get("accounting").expect("checked accounting");
    let records = report::required_u64(accounting_value, "archive_record_count")?;
    let squelch = report::required_u64(accounting_value, "squelch_record_count")?;
    let scans = report::required_u64(accounting_value, "scan_window_record_count")?;
    let samples = report::required_u64(accounting_value, "archive_complex_sample_count")?;
    let squelch_samples = report::required_u64(accounting_value, "squelch_complex_sample_count")?;
    let scan_samples = report::required_u64(accounting_value, "scan_window_complex_sample_count")?;
    let eligible = report::required_u64(accounting_value, "feature_eligible_record_count")?;
    let insufficient = report::required_u64(accounting_value, "feature_insufficient_record_count")?;
    let groups = report::required_u64(accounting_value, "group_count")?;
    let grouped = report::required_u64(accounting_value, "grouped_squelch_record_count")?;
    ensure!(
        records <= BURST_FAMILY_MAX_RECORDS
            && samples <= BURST_FAMILY_MAX_SAMPLES
            && groups <= BURST_FAMILY_MAX_GROUPS
            && squelch.checked_add(scans) == Some(records)
            && squelch_samples.checked_add(scan_samples) == Some(samples)
            && eligible.checked_add(insufficient) == Some(squelch)
            && grouped == squelch
            && (groups == 0) == (squelch == 0),
        "burst-family accounting partitions differ"
    );
    let expected_bytes = 16u64
        .checked_add(
            records
                .checked_mul(48)
                .context("burst header extent overflow")?,
        )
        .and_then(|bytes| {
            samples
                .checked_mul(4)
                .and_then(|body| bytes.checked_add(body))
        })
        .context("burst archive extent overflow")?;
    ensure!(
        expected_bytes == archive.bytes,
        "burst-family archive byte equation differs"
    );
    ensure!(
        report::required_u64(bursts, "schema_version")? == 1
            && bursts.get("source_format").and_then(Value::as_str) == Some("blue_dragon_burst_v1")
            && report::required_u64(bursts, "channel_sample_rate_hz")? == 2_000_000
            && report::required_u64(bursts, "source_bytes")? == archive.bytes
            && report::required_u64(bursts, "archive_record_count")? == records
            && report::required_u64(bursts, "squelch_record_count")? == squelch
            && report::required_u64(bursts, "scan_window_record_count")? == scans
            && report::required_u64(bursts, "complex_sample_count")? == samples,
        "burst-family accounting differs from Bluetooth burst inspection"
    );

    let families = summary
        .get("families")
        .and_then(Value::as_array)
        .context("burst-family summary lacks families")?;
    ensure!(
        families.len() == BURST_FAMILY_LABELS.len(),
        "burst-family label set differs"
    );
    let mut family_groups = 0u64;
    let mut family_records = 0u64;
    let mut family_samples = 0u64;
    let mut groups_by_family = [0u64; 5];
    let mut records_by_family = [0u64; 5];
    let mut samples_by_family = [0u64; 5];
    for (index, (family, expected_label)) in families.iter().zip(BURST_FAMILY_LABELS).enumerate() {
        ensure!(
            family.as_object().is_some_and(|object| {
                object.len() == 4
                    && [
                        "candidate_family",
                        "group_count",
                        "record_count",
                        "complex_sample_count",
                    ]
                    .into_iter()
                    .all(|field| object.contains_key(field))
            }) && family.get("candidate_family").and_then(Value::as_str) == Some(expected_label),
            "burst-family label or order differs"
        );
        let label_groups = report::required_u64(family, "group_count")?;
        let label_records = report::required_u64(family, "record_count")?;
        let label_samples = report::required_u64(family, "complex_sample_count")?;
        let minimum_records_per_group = match index {
            0 => 4,
            1 => 2,
            _ => 1,
        };
        ensure!(
            label_groups
                .checked_mul(minimum_records_per_group)
                .is_some_and(|minimum| minimum <= label_records)
                && (label_groups == 0) == (label_records == 0),
            "burst-family label count is impossible"
        );
        family_groups = family_groups
            .checked_add(label_groups)
            .context("burst-family group total overflow")?;
        family_records = family_records
            .checked_add(label_records)
            .context("burst-family record total overflow")?;
        family_samples = family_samples
            .checked_add(label_samples)
            .context("burst-family sample total overflow")?;
        groups_by_family[index] = label_groups;
        records_by_family[index] = label_records;
        samples_by_family[index] = label_samples;
    }
    ensure!(
        family_groups == groups && family_records == grouped && family_samples == squelch_samples,
        "burst-family partitions differ from accounting"
    );

    let rows = summary
        .get("frequency_family_counts")
        .and_then(Value::as_array)
        .context("burst-family summary lacks frequency-family counts")?;
    ensure!(rows.len() <= 79 * 5, "too many burst frequency-family rows");
    let mut row_incidence = [0u64; 5];
    let mut row_records = [0u64; 5];
    let mut row_samples = [0u64; 5];
    let mut previous = None;
    for row in rows {
        ensure!(
            row.as_object().is_some_and(|object| {
                object.len() == 5
                    && [
                        "candidate_family",
                        "frequency_mhz",
                        "group_incidence_count",
                        "record_count",
                        "complex_sample_count",
                    ]
                    .into_iter()
                    .all(|field| object.contains_key(field))
            }),
            "burst frequency-family row schema differs"
        );
        let label = row
            .get("candidate_family")
            .and_then(Value::as_str)
            .context("burst frequency-family row lacks label")?;
        let family_index = BURST_FAMILY_LABELS
            .iter()
            .position(|expected| *expected == label)
            .context("unknown burst frequency-family label")?;
        let frequency = report::required_u64(row, "frequency_mhz")?;
        ensure!(
            (2402..=2480).contains(&frequency)
                && previous.is_none_or(|(previous_family, previous_frequency)| {
                    family_index > previous_family
                        || (family_index == previous_family && frequency > previous_frequency)
                }),
            "burst frequency-family rows are not in strict family/frequency order"
        );
        previous = Some((family_index, frequency));
        let incidence = report::required_u64(row, "group_incidence_count")?;
        let records = report::required_u64(row, "record_count")?;
        let samples = report::required_u64(row, "complex_sample_count")?;
        ensure!(
            incidence > 0 && incidence <= records && incidence <= groups_by_family[family_index],
            "burst frequency-family incidence is impossible"
        );
        row_incidence[family_index] = row_incidence[family_index]
            .checked_add(incidence)
            .context("burst frequency-family incidence overflow")?;
        row_records[family_index] = row_records[family_index]
            .checked_add(records)
            .context("burst frequency-family record overflow")?;
        row_samples[family_index] = row_samples[family_index]
            .checked_add(samples)
            .context("burst frequency-family sample overflow")?;
    }
    for index in 0..BURST_FAMILY_LABELS.len() {
        ensure!(
            row_records[index] == records_by_family[index]
                && row_samples[index] == samples_by_family[index]
                && row_incidence[index] >= groups_by_family[index]
                && row_incidence[index] <= records_by_family[index],
            "burst frequency-family partitions differ from family totals"
        );
    }

    let features = summary
        .get("eligible_record_features")
        .and_then(Value::as_object)
        .context("burst-family summary lacks feature summaries")?;
    ensure!(features.len() == 8, "burst-family feature schema differs");
    let features_value = summary
        .get("eligible_record_features")
        .expect("checked feature summaries");
    for name in [
        "envelope_cv",
        "usable_phase_differences",
        "phase_concentration",
    ] {
        let (range, integral) = match name {
            "envelope_cv" => (Some((0.0, f64::MAX)), false),
            "usable_phase_differences" => (Some((128.0, 1_048_575.0)), true),
            _ => (Some((0.0, 1.0)), false),
        };
        ensure!(
            validate_numeric_summary(
                features_value
                    .get(name)
                    .with_context(|| format!("missing {name}"))?,
                name,
                range,
                integral,
            )? == eligible,
            "burst-family feature {name} observation count differs"
        );
    }
    let two_level = [
        ("two_level_separation_hz", Some((0.0, 2_000_000.0)), false),
        ("two_level_variance_reduction", Some((0.0, 1.0)), false),
        ("two_level_mode_contrast", Some((0.0, f64::MAX)), false),
        ("two_level_cluster_balance", Some((0.0, 0.5)), false),
        ("two_level_transitions", Some((0.0, 1_048_574.0)), true),
    ];
    let mut two_level_observations = None;
    for (name, range, integral) in two_level {
        let observations = validate_numeric_summary(
            features_value
                .get(name)
                .with_context(|| format!("missing {name}"))?,
            name,
            range,
            integral,
        )?;
        ensure!(
            observations <= eligible
                && two_level_observations.is_none_or(|expected| expected == observations),
            "burst-family two-level feature observation counts differ"
        );
        two_level_observations = Some(observations);
    }

    Ok(if groups > 0 {
        Outcome::UnclassifiedActivity
    } else {
        Outcome::NoUnclassifiedActivityDetected
    })
}

fn bluetooth(
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    const NAME: &str = "bluetooth";
    let destination = output.join("bluetooth");
    let arguments = vec![
        text("replay-iq"),
        path(&source.input),
        text("--center-mhz"),
        text(settings.center_frequency_hz / 1_000_000),
        text("--sample-rate"),
        text(settings.sample_rate_hz),
        text("--max-samples"),
        text(settings.max_samples),
        text("--squelch"),
        text(-45),
        text("--decoder-timeout"),
        text(settings.decoder_timeout_seconds),
        text("--burst-limit-mib"),
        text(128),
        text("--output-limit-mib"),
        text(512),
        text("--output"),
        path(&destination),
    ];
    let command = match run_one(
        &programs.bluetooth,
        arguments,
        output,
        "bluetooth",
        settings,
        stop,
    ) {
        Ok(command) => command,
        Err(error) => return failed(NAME, StageState::Failed, Vec::new(), error),
    };
    let state = command_state(&command);
    let commands = vec![command];
    if let Err(error) = process::require_success(&commands[0]) {
        return failed(NAME, state, commands, error);
    }
    match report::read_json(&destination.join("replay.json"))
        .and_then(|value| bluetooth_outcome(&value, source))
    {
        Ok(outcome) => completed(NAME, outcome, commands),
        Err(error) => failed(NAME, StageState::Failed, commands, error),
    }
}

fn burst_families(
    programs: &Programs,
    _source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    const NAME: &str = "burst_families";
    let archive = output.join("bluetooth/bursts.bdb");
    let destination = output.join("burst-families");
    let arguments = vec![
        text("classify-bursts"),
        path(&archive),
        text("--max-input-mib"),
        text(BURST_FAMILY_MAX_INPUT_MIB),
        text("--max-samples"),
        text(BURST_FAMILY_MAX_SAMPLES),
        text("--max-records"),
        text(BURST_FAMILY_MAX_RECORDS),
        text("--max-groups"),
        text(BURST_FAMILY_MAX_GROUPS),
        text("--max-output-mib"),
        text(BURST_FAMILY_MAX_OUTPUT_MIB),
        text("--output"),
        path(&destination),
    ];
    let command = match run_one(
        &programs.bluetooth,
        arguments,
        output,
        "burst-families",
        settings,
        stop,
    ) {
        Ok(command) => command,
        Err(error) => return failed(NAME, StageState::Failed, Vec::new(), error),
    };
    let state = command_state(&command);
    let commands = vec![command];
    if let Err(error) = process::require_success(&commands[0]) {
        return failed(NAME, state, commands, error);
    }
    let result = (|| -> Result<Outcome> {
        let archive = report::hash_direct_regular_file(&archive, BURST_FAMILY_MAX_INPUT_BYTES)?;
        let bursts = report::read_json(&output.join("bluetooth/bursts.json"))?;
        let summary = report::read_json(&destination.join("burst-families.json"))?;
        burst_family_outcome(&summary, &bursts, &archive)
    })();
    match result {
        Ok(outcome) => completed(NAME, outcome, commands),
        Err(error) => failed(NAME, StageState::Failed, commands, error),
    }
}

fn terminal_integrity(
    request: &Request,
    source: &SourceReport,
    programs: &Programs,
    center_hz: u64,
) -> StageReport {
    const NAME: &str = "terminal_integrity";
    let result = (|| -> Result<()> {
        let observed_source = crate::input::validate(
            &request.input,
            center_hz,
            request.sample_rate_hz,
            request.max_samples,
        )?;
        ensure!(
            &observed_source == source,
            "source capture or sidecar identity changed during analysis"
        );
        let observed_programs = Programs::resolve(&request.programs)?;
        ensure!(
            &observed_programs == programs,
            "one or more analysis executables changed during analysis"
        );
        Ok(())
    })();
    match result {
        Ok(()) => StageReport {
            name: NAME,
            state: StageState::Complete,
            outcome: None,
            commands: Vec::new(),
            detail: Some(
                "source, sidecar and executable sizes and SHA-256 identities revalidated"
                    .to_owned(),
            ),
        },
        Err(error) => failed(NAME, StageState::Failed, Vec::new(), error),
    }
}

fn validate_request(request: &Request) -> Result<(u64, u64)> {
    ensure!(
        request.sample_rate_hz == 20_000_000,
        "multi-protocol analysis currently requires --sample-rate 20000000"
    );
    ensure!(
        (1..=200_000_000).contains(&request.max_samples),
        "max-samples must be 1..=200000000"
    );
    ensure!(
        (1..=512).contains(&request.spectrum_windows),
        "windows must be 1..=512"
    );
    ensure!(
        (1..=3600).contains(&request.decoder_timeout_seconds),
        "decoder-timeout must be 1..=3600 seconds"
    );
    let minimum_child_timeout = request
        .decoder_timeout_seconds
        .checked_add(5)
        .context("decoder timeout overflow")?;
    ensure!(
        (minimum_child_timeout..=7200).contains(&request.child_timeout_seconds),
        "child-timeout must be at least decoder-timeout + 5 and at most 7200 seconds"
    );
    ensure!(
        (1..=64).contains(&request.log_limit_mib),
        "log-limit-mib must be 1..=64"
    );
    let center_hz = u64::from(request.center_mhz)
        .checked_mul(1_000_000)
        .context("center frequency overflow")?;
    ensure!(
        (70_000_000..=6_000_000_000).contains(&center_hz),
        "center-mhz must be 70..=6000"
    );
    for channel in IEEE_CHANNELS {
        let channel_hz = 2_405_000_000 + u64::from(channel - 11) * 5_000_000;
        let edge = center_hz.abs_diff(channel_hz) + 2_000_000;
        ensure!(
            edge < request.sample_rate_hz / 2,
            "center frequency does not fully cover IEEE 802.15.4 channel {channel}"
        );
    }
    let log_limit = request
        .log_limit_mib
        .checked_mul(1024 * 1024)
        .context("log byte limit overflow")?;
    Ok((center_hz, log_limit))
}

/// Opaque identities for the orchestrator and every analysis executable,
/// frozen before a finite live watch is allowed to open the radio.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProgramSet(Programs);

impl ProgramSet {
    pub(crate) fn reports(&self) -> Vec<ProgramProvenance> {
        self.0.reports()
    }
}

/// Validate the fixed analysis contract and freeze every executable identity.
pub(crate) fn preflight(request: &Request) -> Result<ProgramSet> {
    validate_request(request)?;
    Ok(ProgramSet(Programs::resolve(&request.programs)?))
}

/// Refuse a finite watch if any pinned analysis executable changed.
pub(crate) fn revalidate_programs(request: &Request, expected: &ProgramSet) -> Result<()> {
    ensure!(
        Programs::resolve(&request.programs)? == expected.0,
        "one or more pinned analysis executables changed during the watch"
    );
    Ok(())
}

/// Analyze a complete capture and publish a terminal manifest.
pub(crate) fn analyze(request: Request, stop: &AtomicBool) -> Result<(PathBuf, OverallState)> {
    validate_request(&request)?;
    let programs = Programs::resolve(&request.programs)?;
    analyze_with_programs(request, programs, stop)
}

/// Analyze using identities frozen by a live-watch preflight. Terminal
/// integrity still rehashes each program after the decoder stages finish.
pub(crate) fn analyze_pinned(
    request: Request,
    expected: &ProgramSet,
    stop: &AtomicBool,
) -> Result<(PathBuf, OverallState)> {
    validate_request(&request)?;
    revalidate_programs(&request, expected)?;
    analyze_with_programs(request, expected.0.clone(), stop)
}

fn run_stage(
    runner: StageRunner,
    name: &'static str,
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> StageReport {
    if stop.load(Ordering::Relaxed) {
        return StageReport::skipped(name, "skipped after interruption");
    }
    catch_unwind(AssertUnwindSafe(|| {
        runner(programs, source, output, settings, stop)
    }))
    .unwrap_or_else(|_| {
        failed(
            name,
            StageState::Failed,
            Vec::new(),
            "analysis stage worker panicked",
        )
    })
}

#[cfg(test)]
fn sequential_stages(
    runners: [StageRunner; 5],
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> Vec<StageReport> {
    runners
        .into_iter()
        .zip(STAGE_NAMES)
        .map(|(runner, name)| run_stage(runner, name, programs, source, output, settings, stop))
        .collect()
}

fn finish_branch<T>(branch: std::io::Result<thread::ScopedJoinHandle<'_, T>>) -> BranchResult<T> {
    match branch {
        Ok(handle) => match handle.join() {
            Ok(value) => BranchResult::Complete(value),
            Err(_) => BranchResult::Panicked,
        },
        Err(_) => BranchResult::SpawnFailed,
    }
}

fn reduce_stage_branch(
    result: BranchResult<StageReport>,
    name: &'static str,
    spawn_detail: &'static str,
    panic_detail: &'static str,
) -> StageReport {
    match result {
        BranchResult::Complete(report) => report,
        BranchResult::SpawnFailed => failed(name, StageState::Failed, Vec::new(), spawn_detail),
        BranchResult::Panicked => failed(name, StageState::Failed, Vec::new(), panic_detail),
    }
}

fn reduce_branch_results(
    wifi: BranchResult<StageReport>,
    rf: BranchResult<StageReport>,
    ieee: BranchResult<StageReport>,
    bluetooth: BranchResult<[StageReport; 2]>,
) -> [StageReport; 5] {
    let wifi = reduce_stage_branch(
        wifi,
        STAGE_NAMES[0],
        "could not start Wi-Fi analysis branch",
        "Wi-Fi analysis branch panicked",
    );
    let rf = reduce_stage_branch(
        rf,
        STAGE_NAMES[1],
        "could not start RF survey analysis branch",
        "RF survey analysis branch panicked",
    );
    let ieee = reduce_stage_branch(
        ieee,
        STAGE_NAMES[2],
        "could not start IEEE 802.15.4 analysis branch",
        "IEEE 802.15.4 analysis branch panicked",
    );
    let [bluetooth, burst] = match bluetooth {
        BranchResult::Complete(reports) => reports,
        BranchResult::SpawnFailed => [
            failed(
                STAGE_NAMES[3],
                StageState::Failed,
                Vec::new(),
                "could not start Bluetooth analysis branch",
            ),
            failed(
                STAGE_NAMES[4],
                StageState::Failed,
                Vec::new(),
                "Bluetooth analysis branch could not start before burst-family analysis",
            ),
        ],
        BranchResult::Panicked => [
            failed(
                STAGE_NAMES[3],
                StageState::Failed,
                Vec::new(),
                "Bluetooth analysis branch panicked",
            ),
            failed(
                STAGE_NAMES[4],
                StageState::Failed,
                Vec::new(),
                "Bluetooth analysis branch panicked before completing burst-family analysis",
            ),
        ],
    };
    [wifi, rf, ieee, bluetooth, burst]
}

fn parallel_branches<W, R, I, B>(wifi: W, rf: R, ieee: I, bluetooth: B) -> [StageReport; 5]
where
    W: FnOnce() -> StageReport + Send,
    R: FnOnce() -> StageReport + Send,
    I: FnOnce() -> StageReport + Send,
    B: FnOnce() -> [StageReport; 2] + Send,
{
    thread::scope(|scope| {
        let wifi = thread::Builder::new()
            .name("digital-wifi-analysis".into())
            .spawn_scoped(scope, wifi);
        let rf = thread::Builder::new()
            .name("digital-rf-analysis".into())
            .spawn_scoped(scope, rf);
        let ieee = thread::Builder::new()
            .name("digital-ieee802154-analysis".into())
            .spawn_scoped(scope, ieee);
        let bluetooth = thread::Builder::new()
            .name("digital-bluetooth-analysis".into())
            .spawn_scoped(scope, bluetooth);

        // Join every successfully spawned branch before interpreting any
        // failure so no worker can detach or skip another worker's cleanup.
        reduce_branch_results(
            finish_branch(wifi),
            finish_branch(rf),
            finish_branch(ieee),
            finish_branch(bluetooth),
        )
    })
}

fn parallel_stages(
    runners: [StageRunner; 5],
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> Vec<StageReport> {
    // The four branches write disjoint output subtrees. Burst-family analysis
    // shares the Bluetooth branch because it consumes that branch's archive.
    let [
        wifi_runner,
        rf_runner,
        ieee_runner,
        bluetooth_runner,
        burst_runner,
    ] = runners;
    Vec::from(parallel_branches(
        || {
            run_stage(
                wifi_runner,
                STAGE_NAMES[0],
                programs,
                source,
                output,
                settings,
                stop,
            )
        },
        || {
            run_stage(
                rf_runner,
                STAGE_NAMES[1],
                programs,
                source,
                output,
                settings,
                stop,
            )
        },
        || {
            run_stage(
                ieee_runner,
                STAGE_NAMES[2],
                programs,
                source,
                output,
                settings,
                stop,
            )
        },
        || {
            let bluetooth = run_stage(
                bluetooth_runner,
                STAGE_NAMES[3],
                programs,
                source,
                output,
                settings,
                stop,
            );
            let burst = run_stage(
                burst_runner,
                STAGE_NAMES[4],
                programs,
                source,
                output,
                settings,
                stop,
            );
            [bluetooth, burst]
        },
    ))
}

fn analysis_stages(
    runners: [StageRunner; 5],
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    settings: &Settings,
    stop: &AtomicBool,
) -> Vec<StageReport> {
    let stages = if stop.load(Ordering::Relaxed) {
        STAGE_NAMES
            .map(|name| StageReport::skipped(name, "skipped after interruption"))
            .into()
    } else {
        parallel_stages(runners, programs, source, output, settings, stop)
    };
    // Progress must not prevent publishing the terminal analysis report.
    for stage in &stages {
        if stage.state != StageState::Skipped {
            let _ = crate::print_status(format_args!(
                "{}: {:?} {:?}",
                stage.name, stage.state, stage.outcome
            ));
        }
    }
    stages
}

fn analyze_with_programs(
    request: Request,
    programs: Programs,
    stop: &AtomicBool,
) -> Result<(PathBuf, OverallState)> {
    let (center_hz, log_limit) = validate_request(&request)?;
    let source = crate::input::validate(
        &request.input,
        center_hz,
        request.sample_rate_hz,
        request.max_samples,
    )?;
    let bluetooth_channels = source.sample_rate_hz / 1_000_000;
    ensure!(
        bluetooth_channels >= 2
            && bluetooth_channels.is_multiple_of(2)
            && source
                .complex_samples
                .is_multiple_of(bluetooth_channels / 2),
        "full input sample count must align to a complete Bluetooth channelizer step"
    );
    let output = report::fresh_output(&request.output)?;
    report::private_directory(&output.join("logs"))?;
    let settings = Settings {
        center_frequency_hz: center_hz,
        sample_rate_hz: request.sample_rate_hz,
        max_samples: request.max_samples,
        spectrum_windows: request.spectrum_windows,
        decoder_timeout_seconds: request.decoder_timeout_seconds,
        child_timeout_seconds: request.child_timeout_seconds,
        retained_log_limit_bytes_per_stream: log_limit,
        ieee802154_channels: IEEE_CHANNELS,
    };
    let runners: [StageRunner; 5] = [wifi, rf_survey, ieee802154, bluetooth, burst_families];
    let mut stages = analysis_stages(runners, &programs, &source, &output, &settings, stop);
    let integrity = terminal_integrity(&request, &source, &programs, center_hz);
    let _ = crate::print_status(format_args!(
        "terminal_integrity: {:?} {:?}",
        integrity.state, integrity.outcome
    ));
    stages.push(integrity);
    let overall_state = if stop.load(Ordering::Relaxed)
        || stages
            .iter()
            .any(|stage| stage.state == StageState::Interrupted)
    {
        OverallState::Interrupted
    } else if stages
        .iter()
        .any(|stage| stage.state != StageState::Complete)
    {
        OverallState::Failed
    } else {
        OverallState::Complete
    };
    let report = AnalysisReport {
        schema_version: 2,
        operation: "offline_multi_protocol_analysis",
        overall_state,
        source,
        settings,
        programs: programs.reports(),
        stages,
        interpretation: "Each outcome is a bounded offline decoder or relative-activity observation over one saved capture. Valid FCS/CRC establishes consistency of recovered bytes, not device identity, authorization, decryption, continuous reception or complete protocol coverage. Zero candidates or retained burst groups do not establish signal absence, and unclassified spectral or waveform-family activity does not establish a digital protocol.",
    };
    report::publish(&output, &report)?;
    Ok((output, overall_state))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::Instant;

    use serde_json::json;

    use super::*;

    const TEST_SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    fn source() -> SourceReport {
        SourceReport {
            input: "input".into(),
            sidecar: "input.json".into(),
            format: "SC16_Q11_LE",
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: 20_000_000,
            bandwidth_hz: 20_000_000,
            complex_samples: 8200,
            bytes: 32_800,
            sha256: "0".repeat(64),
            sidecar_sha256: "1".repeat(64),
            continuity_guaranteed: false,
        }
    }

    fn test_programs() -> Programs {
        fn program(name: &'static str) -> ProgramProvenance {
            ProgramProvenance {
                name,
                path: PathBuf::from(name),
                bytes: 0,
                sha256: "0".repeat(64),
            }
        }
        Programs {
            orchestrator: program("digital-monitor"),
            wifi: program("wifi-monitor"),
            rf: program("rf-observatory"),
            ieee: program("ieee802154-monitor"),
            ieee_worker: program("ieee802154-futuresdr-worker"),
            bluetooth: program("bluetooth-monitor"),
        }
    }

    fn test_settings() -> Settings {
        Settings {
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: 20_000_000,
            max_samples: 8200,
            spectrum_windows: 1,
            decoder_timeout_seconds: 1,
            child_timeout_seconds: 6,
            retained_log_limit_bytes_per_stream: 1024,
            ieee802154_channels: IEEE_CHANNELS,
        }
    }

    fn complete_stage(name: &'static str) -> StageReport {
        completed(name, Outcome::NoValidCandidates, Vec::new())
    }

    fn test_ieee_channel_run(channel: u8, detail: impl Into<String>) -> IeeeChannelRun {
        IeeeChannelRun {
            channel,
            commands: Vec::new(),
            evidence: None,
            errors: vec![detail.into()],
            interrupted: false,
        }
    }

    fn complete_wifi(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        complete_stage("wifi")
    }

    fn complete_rf(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        complete_stage("rf_survey")
    }

    fn complete_ieee(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        complete_stage("ieee802154")
    }

    fn complete_bluetooth(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        complete_stage("bluetooth")
    }

    fn complete_burst(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        complete_stage("burst_families")
    }

    fn panic_rf(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        panic!("synthetic RF stage panic")
    }

    fn panic_bluetooth(
        _: &Programs,
        _: &SourceReport,
        _: &Path,
        _: &Settings,
        _: &AtomicBool,
    ) -> StageReport {
        panic!("synthetic Bluetooth stage panic")
    }

    fn empty_wifi_eapol() -> Value {
        json!({
            "frame_count":0,
            "complete_header_count":0,
            "truncated_header_count":0,
            "complete_body_count":0,
            "truncated_body_count":0,
            "packet_types":{
                "eap_packet":0,
                "start":0,
                "logoff":0,
                "key":0,
                "asf_alert":0,
                "other":0
            },
            "parsed_key_header_count":0,
            "pairwise_key_count":0,
            "install_key_count":0,
            "ack_key_count":0,
            "mic_key_count":0,
            "secure_key_count":0
        })
    }

    fn empty_wifi_block_ack() -> Value {
        json!({
            "frame_count":0,
            "truncated_control_count":0,
            "complete_compressed_single_tid_count":0,
            "truncated_compressed_single_tid_count":0,
            "trailing_compressed_single_tid_count":0,
            "other_variant_count":0,
            "tids":{
                "tid_0":0,"tid_1":0,"tid_2":0,"tid_3":0,
                "tid_4":0,"tid_5":0,"tid_6":0,"tid_7":0,
                "tid_8":0,"tid_9":0,"tid_10":0,"tid_11":0,
                "tid_12":0,"tid_13":0,"tid_14":0,"tid_15":0
            },
            "set_bitmap_bits_total":0,
            "zero_bitmap_frames":0,
            "nonzero_bitmap_frames":0,
            "full_bitmap_frames":0
        })
    }

    fn empty_wifi_ofdm_rejections() -> Value {
        json!({
            "signal_or_probe_rejected":0,
            "greenfield_header_unsupported":0,
            "greenfield_probe_unrecognized":0,
            "mixed_header_unsupported":0,
            "legacy_fallback_unreservable":0,
            "data_decode_failed":0,
            "pending_capacity_rejected":0,
            "strong_long_training_not_found":0
        })
    }

    fn saved_wifi_ofdm_rejections() -> Value {
        json!({
            "signal_or_probe_rejected":11,
            "greenfield_header_unsupported":13,
            "greenfield_probe_unrecognized":17,
            "mixed_header_unsupported":19,
            "legacy_fallback_unreservable":23,
            "data_decode_failed":29,
            "pending_capacity_rejected":31,
            "strong_long_training_not_found":20
        })
    }

    fn empty_wifi_ofdm_data_failures() -> Value {
        json!({"legacy":0,"ht_bcc":0,"ht_ldpc":0})
    }

    fn saved_wifi_ofdm_data_failures() -> Value {
        json!({"legacy":7,"ht_bcc":11,"ht_ldpc":11})
    }

    fn empty_wifi_ofdm_strong_long_training() -> Value {
        json!({
            "strong_short_periodicity_timeout":0,
            "strong_long_repeat_not_found":0,
            "strong_long_reference_not_found":0,
            "strong_long_coordinates_unavailable":0
        })
    }

    fn saved_wifi_ofdm_strong_long_training() -> Value {
        json!({
            "strong_short_periodicity_timeout":2,
            "strong_long_repeat_not_found":3,
            "strong_long_reference_not_found":5,
            "strong_long_coordinates_unavailable":10
        })
    }

    fn mixed_wifi_block_ack() -> Value {
        json!({
            "frame_count":6,
            "truncated_control_count":1,
            "complete_compressed_single_tid_count":2,
            "truncated_compressed_single_tid_count":1,
            "trailing_compressed_single_tid_count":1,
            "other_variant_count":1,
            "tids":{
                "tid_0":1,"tid_1":0,"tid_2":0,"tid_3":0,
                "tid_4":1,"tid_5":0,"tid_6":0,"tid_7":0,
                "tid_8":0,"tid_9":0,"tid_10":0,"tid_11":0,
                "tid_12":0,"tid_13":0,"tid_14":0,"tid_15":0
            },
            "set_bitmap_bits_total":65,
            "zero_bitmap_frames":0,
            "nonzero_bitmap_frames":2,
            "full_bitmap_frames":1
        })
    }

    fn one_key_wifi_eapol() -> Value {
        json!({
            "frame_count":1,
            "complete_header_count":1,
            "truncated_header_count":0,
            "complete_body_count":1,
            "truncated_body_count":0,
            "packet_types":{
                "eap_packet":0,
                "start":0,
                "logoff":0,
                "key":1,
                "asf_alert":0,
                "other":0
            },
            "parsed_key_header_count":1,
            "pairwise_key_count":1,
            "install_key_count":0,
            "ack_key_count":1,
            "mic_key_count":0,
            "secure_key_count":0
        })
    }

    fn empty_wifi_advertisements() -> Value {
        json!({
            "eligible_frame_count":0,
            "beacon_frames":0,
            "probe_response_frames":0,
            "complete_ie_layout_frames":0,
            "truncated_ie_layout_frames":0,
            "phy":{
                "ht_capabilities_element_frames":0,
                "ht_operation_element_frames":0,
                "vht_capabilities_element_frames":0,
                "vht_operation_element_frames":0,
                "he_capabilities_element_frames":0,
                "he_operation_element_frames":0,
                "eht_capabilities_element_frames":0,
                "eht_operation_element_frames":0,
                "unrecognized_extension_element_frames":0
            },
            "security":{
                "privacy_capability_frames":0,
                "rsn_present_frames":0,
                "rsn_parsed_frames":0,
                "rsn_malformed_frames":0,
                "pmf_capable_frames":0,
                "pmf_required_frames":0,
                "wpa_present_frames":0,
                "wpa_parsed_frames":0,
                "wpa_malformed_frames":0
            }
        })
    }

    fn ten_beacon_wifi_advertisements() -> Value {
        json!({
            "eligible_frame_count":10,
            "beacon_frames":10,
            "probe_response_frames":0,
            "complete_ie_layout_frames":10,
            "truncated_ie_layout_frames":0,
            "phy":{
                "ht_capabilities_element_frames":10,
                "ht_operation_element_frames":10,
                "vht_capabilities_element_frames":0,
                "vht_operation_element_frames":0,
                "he_capabilities_element_frames":10,
                "he_operation_element_frames":10,
                "eht_capabilities_element_frames":10,
                "eht_operation_element_frames":10,
                "unrecognized_extension_element_frames":10
            },
            "security":{
                "privacy_capability_frames":10,
                "rsn_present_frames":10,
                "rsn_parsed_frames":10,
                "rsn_malformed_frames":0,
                "pmf_capable_frames":10,
                "pmf_required_frames":0,
                "wpa_present_frames":0,
                "wpa_parsed_frames":0,
                "wpa_malformed_frames":0
            }
        })
    }

    fn burst_family_fixture(active: bool) -> (Value, Value, report::FileDigest) {
        let (records, samples, eligible, insufficient, groups, bytes) = if active {
            (2, 4, 2, 0, 1, 128)
        } else {
            (0, 0, 0, 0, 0, 16)
        };
        let numeric = |value: f64| {
            if active {
                json!({"observations":2,"min":value,"max":value,"mean":value})
            } else {
                json!({"observations":0,"min":null,"max":null,"mean":null})
            }
        };
        let families: Vec<_> = BURST_FAMILY_LABELS
            .into_iter()
            .enumerate()
            .map(|(index, label)| {
                json!({
                    "candidate_family":label,
                    "group_count":u64::from(active && index == 4),
                    "record_count":if active && index == 4 { records } else { 0 },
                    "complex_sample_count":if active && index == 4 { samples } else { 0 }
                })
            })
            .collect();
        let digest = report::FileDigest {
            bytes,
            sha256: "a".repeat(64),
        };
        let summary = json!({
            "schema_version":2,
            "operation":"classify_bursts",
            "source":{"format":"blue_dragon_burst_v1","bytes":bytes,"sha256":digest.sha256},
            "accounting":{
                "archive_record_count":records,
                "squelch_record_count":records,
                "scan_window_record_count":0,
                "archive_complex_sample_count":samples,
                "squelch_complex_sample_count":samples,
                "scan_window_complex_sample_count":0,
                "feature_eligible_record_count":eligible,
                "feature_insufficient_record_count":insufficient,
                "group_count":groups,
                "grouped_squelch_record_count":records
            },
            "settings":{
                "channel_sample_rate_hz":2000000,
                "sample_period_ns":500,
                "group_connectivity_gap_ns":50000,
                "group_frequency_step_mhz":1,
                "simultaneous_interval":"half_open_unexpanded_record_interval",
                "active_power_quantile":[4,5],
                "active_power_divisor":16,
                "minimum_usable_phase_differences":128,
                "maximum_envelope_cv":0.25,
                "minimum_cluster_balance":0.15,
                "minimum_variance_reduction":0.6,
                "minimum_mode_contrast":4.0,
                "mode_contrast_std_floor_hz":1.0,
                "minimum_phase_concentration":0.05,
                "separation_hz":[40000.0,900000.0],
                "minimum_transitions":6,
                "kmeans_iterations":8,
                "adjacent_multibin_minimum_width":2,
                "wideband_multibin_minimum_width":4
            },
            "families":families,
            "frequency_family_counts":if active { vec![json!({
                "candidate_family":"unclassified_burst_group",
                "frequency_mhz":2437,"group_incidence_count":1,
                "record_count":records,"complex_sample_count":samples
            })] } else { Vec::<Value>::new() },
            "eligible_record_features":{
                "envelope_cv":numeric(0.1),
                "usable_phase_differences":if active {
                    json!({"observations":2,"min":128.0,"max":129.0,"mean":128.5})
                } else { numeric(0.0) },
                "phase_concentration":numeric(0.5),
                "two_level_separation_hz":numeric(500000.0),
                "two_level_variance_reduction":numeric(0.8),
                "two_level_mode_contrast":numeric(5.0),
                "two_level_cluster_balance":numeric(0.5),
                "two_level_transitions":if active {
                    json!({"observations":2,"min":10.0,"max":11.0,"mean":10.5})
                } else { numeric(0.0) }
            },
            "protocol_identity_established":false,
            "interpretation":"test"
        });
        let bursts = json!({
            "schema_version":1,
            "source_format":"blue_dragon_burst_v1",
            "source_bytes":bytes,
            "channel_sample_rate_hz":2000000,
            "archive_record_count":records,
            "squelch_record_count":records,
            "scan_window_record_count":0,
            "complex_sample_count":samples
        });
        (summary, bursts, digest)
    }

    #[test]
    fn wifi_ofdm_rejection_accounting_is_closed_and_exact() {
        let valid = json!({
            "ofdm":{
                "rejected_frames":36,
                "rejections":{
                    "signal_or_probe_rejected":1,
                    "greenfield_header_unsupported":2,
                    "greenfield_probe_unrecognized":3,
                    "mixed_header_unsupported":4,
                    "legacy_fallback_unreservable":5,
                    "data_decode_failed":6,
                    "pending_capacity_rejected":7,
                    "strong_long_training_not_found":8
                }
            }
        });
        assert_eq!(
            serde_json::to_value(wifi_ofdm_rejection_metrics(&valid).unwrap()).unwrap(),
            valid["ofdm"]["rejections"]
        );

        let mut missing = valid.clone();
        missing["ofdm"]["rejections"]
            .as_object_mut()
            .unwrap()
            .remove("data_decode_failed");
        assert!(wifi_ofdm_rejection_metrics(&missing).is_err());

        let mut extra = valid.clone();
        extra["ofdm"]["rejections"]["unexpected"] = json!(0);
        assert!(wifi_ofdm_rejection_metrics(&extra).is_err());

        let mut wrong_type = valid.clone();
        wrong_type["ofdm"]["rejections"]["strong_long_training_not_found"] = json!("8");
        assert!(wifi_ofdm_rejection_metrics(&wrong_type).is_err());

        let mut wrong_sum = valid.clone();
        wrong_sum["ofdm"]["rejected_frames"] = json!(35);
        assert!(wifi_ofdm_rejection_metrics(&wrong_sum).is_err());

        let mut overflow = valid;
        overflow["ofdm"]["rejected_frames"] = json!(u64::MAX);
        overflow["ofdm"]["rejections"]["signal_or_probe_rejected"] = json!(u64::MAX);
        assert!(wifi_ofdm_rejection_metrics(&overflow).is_err());
    }

    #[test]
    fn wifi_ofdm_data_failure_detail_is_closed_and_exact() {
        let rejections = WifiOfdmRejectionMetrics {
            data_decode_failed: 6,
            ..Default::default()
        };
        let valid = json!({"ofdm":{"data_failures":{
            "legacy":1,"ht_bcc":2,"ht_ldpc":3
        }}});
        assert_eq!(
            serde_json::to_value(wifi_ofdm_data_failure_metrics(&valid, &rejections).unwrap())
                .unwrap(),
            valid["ofdm"]["data_failures"]
        );

        let mut missing = valid.clone();
        missing["ofdm"]["data_failures"]
            .as_object_mut()
            .unwrap()
            .remove("ht_bcc");
        assert!(wifi_ofdm_data_failure_metrics(&missing, &rejections).is_err());

        let mut extra = valid.clone();
        extra["ofdm"]["data_failures"]["unexpected"] = json!(0);
        assert!(wifi_ofdm_data_failure_metrics(&extra, &rejections).is_err());

        let mut wrong_type = valid.clone();
        wrong_type["ofdm"]["data_failures"]["ht_ldpc"] = json!("3");
        assert!(wifi_ofdm_data_failure_metrics(&wrong_type, &rejections).is_err());

        let mut wrong_sum = valid;
        wrong_sum["ofdm"]["data_failures"]["legacy"] = json!(0);
        assert!(wifi_ofdm_data_failure_metrics(&wrong_sum, &rejections).is_err());

        let saturated_rejections = WifiOfdmRejectionMetrics {
            data_decode_failed: u64::MAX,
            ..Default::default()
        };
        let overflow = json!({"ofdm":{"data_failures":{
            "legacy":u64::MAX,"ht_bcc":1,"ht_ldpc":0
        }}});
        assert!(wifi_ofdm_data_failure_metrics(&overflow, &saturated_rejections).is_err());
    }

    #[test]
    fn wifi_strong_long_training_detail_is_closed_and_exact() {
        let valid = json!({
            "ofdm":{
                "rejected_frames":8,
                "rejections":{
                    "signal_or_probe_rejected":0,
                    "greenfield_header_unsupported":0,
                    "greenfield_probe_unrecognized":0,
                    "mixed_header_unsupported":0,
                    "legacy_fallback_unreservable":0,
                    "data_decode_failed":0,
                    "pending_capacity_rejected":0,
                    "strong_long_training_not_found":8
                },
                "strong_long_training":{
                    "strong_short_periodicity_timeout":1,
                    "strong_long_repeat_not_found":2,
                    "strong_long_reference_not_found":3,
                    "strong_long_coordinates_unavailable":2
                }
            }
        });
        let rejections = wifi_ofdm_rejection_metrics(&valid).unwrap();
        assert_eq!(
            serde_json::to_value(
                wifi_ofdm_strong_long_training_metrics(&valid, &rejections).unwrap()
            )
            .unwrap(),
            valid["ofdm"]["strong_long_training"]
        );

        let mut missing = valid.clone();
        missing["ofdm"]["strong_long_training"]
            .as_object_mut()
            .unwrap()
            .remove("strong_long_repeat_not_found");
        assert!(wifi_ofdm_strong_long_training_metrics(&missing, &rejections).is_err());

        let mut extra = valid.clone();
        extra["ofdm"]["strong_long_training"]["unexpected"] = json!(0);
        assert!(wifi_ofdm_strong_long_training_metrics(&extra, &rejections).is_err());

        let mut wrong_type = valid.clone();
        wrong_type["ofdm"]["strong_long_training"]["strong_long_reference_not_found"] = json!("3");
        assert!(wifi_ofdm_strong_long_training_metrics(&wrong_type, &rejections).is_err());

        let mut wrong_sum = valid.clone();
        wrong_sum["ofdm"]["strong_long_training"]["strong_long_coordinates_unavailable"] = json!(1);
        assert!(wifi_ofdm_strong_long_training_metrics(&wrong_sum, &rejections).is_err());

        let saturated_rejections = WifiOfdmRejectionMetrics {
            strong_long_training_not_found: u64::MAX,
            ..Default::default()
        };
        let saturated = json!({
            "ofdm":{"strong_long_training":{
                "strong_short_periodicity_timeout":0,
                "strong_long_repeat_not_found":0,
                "strong_long_reference_not_found":u64::MAX,
                "strong_long_coordinates_unavailable":0
            }}
        });
        assert_eq!(
            wifi_ofdm_strong_long_training_metrics(&saturated, &saturated_rejections)
                .unwrap()
                .strong_long_reference_not_found,
            u64::MAX
        );

        let overflow = json!({
            "ofdm":{"strong_long_training":{
                "strong_short_periodicity_timeout":u64::MAX,
                "strong_long_repeat_not_found":1,
                "strong_long_reference_not_found":0,
                "strong_long_coordinates_unavailable":0
            }}
        });
        assert!(wifi_ofdm_strong_long_training_metrics(&overflow, &saturated_rejections).is_err());
    }

    #[test]
    fn classifies_protocol_evidence_without_absence_claims() {
        assert_eq!(
            wifi_outcome(
                &json!({
                    "schema_version":4,"operation":"inspect_wifi_packets",
                    "total_records":2,"validated_frames":1,"management_frames":0,
                    "control_frames":0,"data_frames":1,
                    "frames_by_type":{"data":1},"eapol":empty_wifi_eapol(),
                    "block_ack":empty_wifi_block_ack(),
                    "advertisements":empty_wifi_advertisements()
                }),
                &json!({
                    "schema_version":4,"operation":"offline_wifi_iq_decode",
                    "input_samples":8200,"processed_samples":8200,
                    "center_frequency_hz":2_437_000_000u64,
                    "sample_rate_hz":20_000_000,"frames_written":2,
                    "ofdm":{"invalid_fcs":0,"rejected_frames":0,
                        "rejections":empty_wifi_ofdm_rejections(),
                        "data_failures":empty_wifi_ofdm_data_failures(),
                        "strong_long_training":empty_wifi_ofdm_strong_long_training()},
                    "dsss":{"invalid_fcs":0}
                }),
                &source(),
            )
            .unwrap(),
            Outcome::EvidenceFound
        );
        assert_eq!(
            wifi_outcome(
                &json!({
                    "schema_version":4,"operation":"inspect_wifi_packets",
                    "total_records":0,"validated_frames":0,"management_frames":0,
                    "control_frames":0,"data_frames":0,"frames_by_type":{},
                    "eapol":empty_wifi_eapol(),
                    "block_ack":empty_wifi_block_ack(),
                    "advertisements":empty_wifi_advertisements()
                }),
                &json!({
                    "schema_version":4,"operation":"offline_wifi_iq_decode",
                    "input_samples":8200,"processed_samples":8200,
                    "center_frequency_hz":2_437_000_000u64,
                    "sample_rate_hz":20_000_000,"frames_written":0,
                    "ofdm":{"invalid_fcs":0,"rejected_frames":163,
                        "rejections":saved_wifi_ofdm_rejections(),
                        "data_failures":saved_wifi_ofdm_data_failures(),
                        "strong_long_training":saved_wifi_ofdm_strong_long_training()},
                    "dsss":{"invalid_fcs":0,"rejected_frames":7}
                }),
                &source(),
            )
            .unwrap(),
            Outcome::NoValidCandidates
        );
        assert_eq!(
            rf_outcome(
                &json!({
                    "schema_version":1,"operation":"offline_rf_spectrum_survey",
                    "input_samples":8200,"inspected_prefix_samples":8200,
                    "center_frequency_hz":2_437_000_000u64,
                    "sample_rate_hz":20_000_000,
                    "activity_detection":{"regions":[{}]}
                }),
                &source(),
            )
            .unwrap(),
            Outcome::UnclassifiedActivity
        );

        let wifi_summary = json!({
            "schema_version":4,"operation":"inspect_wifi_packets",
            "total_records":0,"validated_frames":0,"management_frames":0,
            "control_frames":0,"data_frames":0,"frames_by_type":{},
            "eapol":empty_wifi_eapol(),
            "block_ack":empty_wifi_block_ack(),
            "advertisements":empty_wifi_advertisements()
        });
        let wifi_decode = json!({
            "schema_version":4,"operation":"offline_wifi_iq_decode",
            "input_samples":8200,"processed_samples":8200,
            "center_frequency_hz":2_437_000_000u64,
            "sample_rate_hz":20_000_000,"frames_written":0,
            "ofdm":{"invalid_fcs":0,"rejected_frames":0,
                "rejections":empty_wifi_ofdm_rejections(),
                "data_failures":empty_wifi_ofdm_data_failures(),
                "strong_long_training":empty_wifi_ofdm_strong_long_training()},
            "dsss":{"invalid_fcs":0}
        });
        let mut wrong_wifi_schema = wifi_summary.clone();
        wrong_wifi_schema["schema_version"] = json!(3);
        assert!(wifi_outcome(&wrong_wifi_schema, &wifi_decode, &source()).is_err());
        let mut old_decode_schema = wifi_decode.clone();
        old_decode_schema["schema_version"] = json!(3);
        assert!(wifi_outcome(&wifi_summary, &old_decode_schema, &source()).is_err());
        let mut translated_decode = wifi_decode.clone();
        translated_decode["center_frequency_hz"] = json!(2_436_000_000u64);
        assert!(wifi_outcome(&wifi_summary, &translated_decode, &source()).is_err());
        assert!(
            wifi_outcome_at(&wifi_summary, &translated_decode, &source(), 2_436_000_000).is_ok()
        );
        let mut unexpected_translation = wifi_decode.clone();
        unexpected_translation["frequency_shift_hz"] = json!(1_000_000i64);
        assert!(wifi_outcome(&wifi_summary, &unexpected_translation, &source()).is_err());
        let mut unexpected_filter = wifi_decode.clone();
        unexpected_filter["translation_filter"] = json!("anti-wrap-kaiser-257");
        assert!(wifi_outcome(&wifi_summary, &unexpected_filter, &source()).is_err());
        let mut wrong_decode_operation = wifi_decode;
        wrong_decode_operation["operation"] = json!("decode");
        assert!(wifi_outcome(&wifi_summary, &wrong_decode_operation, &source()).is_err());

        for phy in ["ofdm", "dsss"] {
            let mut bad_fcs_decode = wrong_decode_operation.clone();
            bad_fcs_decode["operation"] = json!("offline_wifi_iq_decode");
            bad_fcs_decode[phy]["invalid_fcs"] = json!(1);
            assert_eq!(
                wifi_outcome(&wifi_summary, &bad_fcs_decode, &source()).unwrap(),
                Outcome::CandidatesWithoutValidFrames
            );
        }

        let mut missing_bad_fcs = wrong_decode_operation.clone();
        missing_bad_fcs["operation"] = json!("offline_wifi_iq_decode");
        missing_bad_fcs["ofdm"]
            .as_object_mut()
            .unwrap()
            .remove("invalid_fcs");
        assert!(wifi_outcome(&wifi_summary, &missing_bad_fcs, &source()).is_err());

        let mut wrong_rf_operation = json!({
            "schema_version":1,"operation":"survey",
            "input_samples":8200,"inspected_prefix_samples":8200,
            "center_frequency_hz":2_437_000_000u64,
            "sample_rate_hz":20_000_000,
            "activity_detection":{"regions":[]}
        });
        assert!(rf_outcome(&wrong_rf_operation, &source()).is_err());
        wrong_rf_operation["operation"] = json!("offline_rf_spectrum_survey");
        wrong_rf_operation["schema_version"] = json!(2);
        assert!(rf_outcome(&wrong_rf_operation, &source()).is_err());
        assert_eq!(
            ieee_comparison_outcome(&[json!({
                "identity_union_record_count":1,
                "identity_union_valid_fcs_record_count":0
            })])
            .unwrap(),
            Outcome::CandidatesWithoutValidFrames
        );
    }

    #[test]
    fn validates_closed_wifi_frame_types_and_rejects_partition_mutations() {
        let valid = json!({
            "validated_frames":29,
            "management_frames":14,
            "control_frames":10,
            "data_frames":5,
            "frames_by_type":{
                "association_request":1,
                "association_response":1,
                "reassociation_request":1,
                "reassociation_response":1,
                "probe_request":1,
                "probe_response":1,
                "beacon":1,
                "ATIM":1,
                "disassociation":1,
                "authentication":1,
                "deauthentication":1,
                "action":1,
                "action_no_ack":1,
                "reserved_management":1,
                "control_wrapper":1,
                "block_ack_request":1,
                "block_ack":1,
                "PS_POLL":1,
                "RTS":1,
                "CTS":1,
                "ACK":1,
                "CF_END":1,
                "CF_END_ACK":1,
                "other_control":1,
                "data":1,
                "null_data":1,
                "qos_data":1,
                "qos_null":1,
                "other_data":1
            }
        });
        let metrics = wifi_frame_type_metrics(&valid).unwrap();
        assert_eq!(metrics.management.atim, 1);
        assert_eq!(metrics.control.ps_poll, 1);
        assert_eq!(metrics.control.rts, 1);
        assert_eq!(metrics.control.cts, 1);
        assert_eq!(metrics.control.ack, 1);
        assert_eq!(metrics.control.cf_end_ack, 1);
        assert_eq!(metrics.data.qos_null, 1);

        let empty = json!({
            "validated_frames":0,"management_frames":0,
            "control_frames":0,"data_frames":0,"frames_by_type":{}
        });
        assert_eq!(wifi_frame_type_metrics(&empty).unwrap().control.ack, 0);

        let mut unknown = valid.clone();
        unknown["frames_by_type"]["identifier_like_key"] = json!(1);
        assert!(wifi_frame_type_metrics(&unknown).is_err());

        let mut wrong_group = valid.clone();
        wrong_group["management_frames"] = json!(13);
        assert!(wifi_frame_type_metrics(&wrong_group).is_err());

        let mut wrong_total = valid.clone();
        wrong_total["validated_frames"] = json!(28);
        assert!(wifi_frame_type_metrics(&wrong_total).is_err());

        let mut overflow = valid.clone();
        overflow["frames_by_type"]["association_request"] = json!(u64::MAX);
        assert!(wifi_frame_type_metrics(&overflow).is_err());

        let mut wrong_type = valid;
        wrong_type["frames_by_type"]["beacon"] = json!("1");
        assert!(wifi_frame_type_metrics(&wrong_type).is_err());
    }

    #[test]
    fn validates_closed_block_ack_aggregate_and_rejects_partition_mutations() {
        let valid = json!({
            "validated_frames":6,
            "management_frames":0,
            "control_frames":6,
            "data_frames":0,
            "frames_by_type":{"block_ack":6},
            "block_ack":mixed_wifi_block_ack()
        });
        let frame_types = wifi_frame_type_metrics(&valid).unwrap();
        let metrics = wifi_block_ack_metrics(&valid, &frame_types).unwrap();
        assert_eq!(metrics.frame_count, 6);
        assert_eq!(metrics.complete_compressed_single_tid_count, 2);
        assert_eq!(metrics.tids.tid_0, 1);
        assert_eq!(metrics.tids.tid_4, 1);
        assert_eq!(metrics.set_bitmap_bits_total, 65);
        assert_eq!(metrics.nonzero_bitmap_frames, 2);
        assert_eq!(metrics.full_bitmap_frames, 1);

        let serialized = serde_json::to_value(&metrics).unwrap();
        let mut fields = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "complete_compressed_single_tid_count",
                "frame_count",
                "full_bitmap_frames",
                "nonzero_bitmap_frames",
                "other_variant_count",
                "set_bitmap_bits_total",
                "tids",
                "trailing_compressed_single_tid_count",
                "truncated_compressed_single_tid_count",
                "truncated_control_count",
                "zero_bitmap_frames",
            ]
        );
        let mut tid_fields = serialized["tids"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        tid_fields.sort_unstable();
        assert_eq!(
            tid_fields,
            [
                "tid_0", "tid_1", "tid_10", "tid_11", "tid_12", "tid_13", "tid_14", "tid_15",
                "tid_2", "tid_3", "tid_4", "tid_5", "tid_6", "tid_7", "tid_8", "tid_9",
            ]
        );

        let mut wrong_partition = valid.clone();
        wrong_partition["block_ack"]["truncated_control_count"] = json!(0);
        assert!(wifi_block_ack_metrics(&wrong_partition, &frame_types).is_err());

        let mut wrong_frame_type = frame_types.clone();
        wrong_frame_type.control.block_ack = 5;
        assert!(wifi_block_ack_metrics(&valid, &wrong_frame_type).is_err());

        for bound in ["control_frames", "validated_frames"] {
            let mut exceeds_bound = valid.clone();
            exceeds_bound[bound] = json!(5);
            assert!(wifi_block_ack_metrics(&exceeds_bound, &frame_types).is_err());
        }

        let mut wrong_tid_partition = valid.clone();
        wrong_tid_partition["block_ack"]["tids"]["tid_1"] = json!(1);
        assert!(wifi_block_ack_metrics(&wrong_tid_partition, &frame_types).is_err());

        let mut wrong_bitmap_partition = valid.clone();
        wrong_bitmap_partition["block_ack"]["zero_bitmap_frames"] = json!(1);
        assert!(wifi_block_ack_metrics(&wrong_bitmap_partition, &frame_types).is_err());

        let mut full_exceeds_nonzero = valid.clone();
        full_exceeds_nonzero["block_ack"]["full_bitmap_frames"] = json!(3);
        assert!(wifi_block_ack_metrics(&full_exceeds_nonzero, &frame_types).is_err());

        let mut set_bits_below_minimum = valid.clone();
        set_bits_below_minimum["block_ack"]["set_bitmap_bits_total"] = json!(64);
        assert!(wifi_block_ack_metrics(&set_bits_below_minimum, &frame_types).is_err());

        let mut set_bits_exceed_nonzero_capacity = valid.clone();
        set_bits_exceed_nonzero_capacity["block_ack"]["set_bitmap_bits_total"] = json!(129);
        assert!(wifi_block_ack_metrics(&set_bits_exceed_nonzero_capacity, &frame_types).is_err());

        let mut nonzero_bitmap_with_zero_set_bits = valid.clone();
        nonzero_bitmap_with_zero_set_bits["block_ack"]["zero_bitmap_frames"] = json!(1);
        nonzero_bitmap_with_zero_set_bits["block_ack"]["nonzero_bitmap_frames"] = json!(1);
        nonzero_bitmap_with_zero_set_bits["block_ack"]["full_bitmap_frames"] = json!(0);
        nonzero_bitmap_with_zero_set_bits["block_ack"]["set_bitmap_bits_total"] = json!(0);
        assert!(wifi_block_ack_metrics(&nonzero_bitmap_with_zero_set_bits, &frame_types).is_err());

        let mut incomplete_with_tid_metrics = valid.clone();
        incomplete_with_tid_metrics["block_ack"]["complete_compressed_single_tid_count"] = json!(0);
        incomplete_with_tid_metrics["block_ack"]["other_variant_count"] = json!(3);
        assert!(wifi_block_ack_metrics(&incomplete_with_tid_metrics, &frame_types).is_err());

        let mut partition_overflow = valid.clone();
        partition_overflow["block_ack"]["truncated_control_count"] = json!(u64::MAX);
        assert!(wifi_block_ack_metrics(&partition_overflow, &frame_types).is_err());

        let mut tid_overflow = valid.clone();
        tid_overflow["block_ack"]["tids"]["tid_0"] = json!(u64::MAX);
        assert!(wifi_block_ack_metrics(&tid_overflow, &frame_types).is_err());

        let mut capacity_overflow = valid.clone();
        capacity_overflow["validated_frames"] = json!(u64::MAX);
        capacity_overflow["control_frames"] = json!(u64::MAX);
        capacity_overflow["block_ack"] = json!({
            "frame_count":u64::MAX,
            "truncated_control_count":0,
            "complete_compressed_single_tid_count":u64::MAX,
            "truncated_compressed_single_tid_count":0,
            "trailing_compressed_single_tid_count":0,
            "other_variant_count":0,
            "tids":{
                "tid_0":u64::MAX,"tid_1":0,"tid_2":0,"tid_3":0,
                "tid_4":0,"tid_5":0,"tid_6":0,"tid_7":0,
                "tid_8":0,"tid_9":0,"tid_10":0,"tid_11":0,
                "tid_12":0,"tid_13":0,"tid_14":0,"tid_15":0
            },
            "set_bitmap_bits_total":u64::MAX,
            "zero_bitmap_frames":0,
            "nonzero_bitmap_frames":u64::MAX,
            "full_bitmap_frames":0
        });
        let mut maximum_frame_types = frame_types.clone();
        maximum_frame_types.control.block_ack = u64::MAX;
        assert!(wifi_block_ack_metrics(&capacity_overflow, &maximum_frame_types).is_err());

        let mut extra_field = valid.clone();
        extra_field["block_ack"]["receiver_address"] = json!("not-retained");
        assert!(wifi_block_ack_metrics(&extra_field, &frame_types).is_err());

        let mut extra_tid = valid.clone();
        extra_tid["block_ack"]["tids"]["tid_16"] = json!(1);
        assert!(wifi_block_ack_metrics(&extra_tid, &frame_types).is_err());

        let mut missing_field = valid.clone();
        missing_field["block_ack"]
            .as_object_mut()
            .unwrap()
            .remove("set_bitmap_bits_total");
        assert!(wifi_block_ack_metrics(&missing_field, &frame_types).is_err());

        for invalid in [json!(-1), json!(0.5), json!("6")] {
            let mut wrong_type = valid.clone();
            wrong_type["block_ack"]["frame_count"] = invalid;
            assert!(wifi_block_ack_metrics(&wrong_type, &frame_types).is_err());
        }
    }

    #[test]
    fn validates_one_key_ack_aggregate_and_rejects_eapol_mutations() {
        let valid = json!({
            "data_frames":1,
            "eapol":one_key_wifi_eapol()
        });
        let metrics = wifi_eapol_metrics(&valid).unwrap();
        assert_eq!(metrics.frame_count, 1);
        assert_eq!(metrics.packet_types.key, 1);
        assert_eq!(metrics.parsed_key_header_count, 1);
        assert_eq!(metrics.ack_key_count, 1);
        assert_eq!(metrics.mic_key_count, 0);

        let mut wrong_header_partition = valid.clone();
        wrong_header_partition["eapol"]["complete_header_count"] = json!(0);
        assert!(wifi_eapol_metrics(&wrong_header_partition).is_err());

        let mut wrong_body_partition = valid.clone();
        wrong_body_partition["eapol"]["truncated_body_count"] = json!(1);
        assert!(wifi_eapol_metrics(&wrong_body_partition).is_err());

        let mut wrong_packet_type_partition = valid.clone();
        wrong_packet_type_partition["eapol"]["packet_types"]["other"] = json!(1);
        assert!(wifi_eapol_metrics(&wrong_packet_type_partition).is_err());

        let mut parsed_key_exceeds_key_packets = valid.clone();
        parsed_key_exceeds_key_packets["eapol"]["packet_types"]["key"] = json!(0);
        parsed_key_exceeds_key_packets["eapol"]["packet_types"]["other"] = json!(1);
        assert!(wifi_eapol_metrics(&parsed_key_exceeds_key_packets).is_err());

        let mut parsed_key_exceeds_complete_bodies = valid.clone();
        parsed_key_exceeds_complete_bodies["eapol"]["complete_body_count"] = json!(0);
        parsed_key_exceeds_complete_bodies["eapol"]["truncated_body_count"] = json!(1);
        assert!(wifi_eapol_metrics(&parsed_key_exceeds_complete_bodies).is_err());

        let mut key_flag_exceeds_parsed_headers = valid.clone();
        key_flag_exceeds_parsed_headers["eapol"]["parsed_key_header_count"] = json!(0);
        assert!(wifi_eapol_metrics(&key_flag_exceeds_parsed_headers).is_err());

        let mut frame_count_exceeds_data_frames = valid.clone();
        frame_count_exceeds_data_frames["data_frames"] = json!(0);
        assert!(wifi_eapol_metrics(&frame_count_exceeds_data_frames).is_err());

        let mut arbitrary_eapol_field = valid.clone();
        arbitrary_eapol_field["eapol"]["body_hex"] = json!("00");
        assert!(wifi_eapol_metrics(&arbitrary_eapol_field).is_err());

        let mut arbitrary_packet_type = valid;
        arbitrary_packet_type["eapol"]["packet_types"]["vendor"] = json!(1);
        assert!(wifi_eapol_metrics(&arbitrary_packet_type).is_err());
    }

    #[test]
    fn validates_advertisement_aggregate_and_rejects_contract_mutations() {
        let valid = json!({
            "validated_frames":10,
            "management_frames":10,
            "advertisements":ten_beacon_wifi_advertisements()
        });
        let metrics = wifi_advertisement_metrics(&valid).unwrap();
        assert_eq!(metrics.eligible_frame_count, 10);
        assert_eq!(metrics.beacon_frames, 10);
        assert_eq!(metrics.probe_response_frames, 0);
        assert_eq!(metrics.phy.ht_capabilities_element_frames, 10);
        assert_eq!(metrics.phy.vht_capabilities_element_frames, 0);
        assert_eq!(metrics.phy.eht_operation_element_frames, 10);
        assert_eq!(metrics.security.rsn_parsed_frames, 10);
        assert_eq!(metrics.security.pmf_capable_frames, 10);

        let serialized = serde_json::to_value(&metrics).unwrap();
        let mut fields = serialized
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "beacon_frames",
                "complete_ie_layout_frames",
                "eligible_frame_count",
                "phy",
                "probe_response_frames",
                "security",
                "truncated_ie_layout_frames",
            ]
        );

        let mut wrong_subtype_partition = valid.clone();
        wrong_subtype_partition["advertisements"]["probe_response_frames"] = json!(1);
        assert!(wifi_advertisement_metrics(&wrong_subtype_partition).is_err());

        let mut subtype_overflow = valid.clone();
        subtype_overflow["advertisements"]["eligible_frame_count"] = json!(0);
        subtype_overflow["advertisements"]["beacon_frames"] = json!(u64::MAX);
        subtype_overflow["advertisements"]["probe_response_frames"] = json!(1);
        subtype_overflow["advertisements"]["complete_ie_layout_frames"] = json!(0);
        assert!(wifi_advertisement_metrics(&subtype_overflow).is_err());

        let mut wrong_layout_partition = valid.clone();
        wrong_layout_partition["advertisements"]["truncated_ie_layout_frames"] = json!(1);
        assert!(wifi_advertisement_metrics(&wrong_layout_partition).is_err());

        let mut phy_exceeds_eligible = valid.clone();
        phy_exceeds_eligible["advertisements"]["phy"]["vht_capabilities_element_frames"] =
            json!(11);
        assert!(wifi_advertisement_metrics(&phy_exceeds_eligible).is_err());

        let mut privacy_exceeds_eligible = valid.clone();
        privacy_exceeds_eligible["advertisements"]["security"]["privacy_capability_frames"] =
            json!(11);
        assert!(wifi_advertisement_metrics(&privacy_exceeds_eligible).is_err());

        let mut exceeds_management = valid.clone();
        exceeds_management["management_frames"] = json!(9);
        assert!(wifi_advertisement_metrics(&exceeds_management).is_err());

        let mut exceeds_validated = valid.clone();
        exceeds_validated["validated_frames"] = json!(9);
        assert!(wifi_advertisement_metrics(&exceeds_validated).is_err());

        for field in ["rsn_parsed_frames", "rsn_malformed_frames"] {
            let mut exceeds_presence = valid.clone();
            exceeds_presence["advertisements"]["security"]["rsn_present_frames"] = json!(0);
            exceeds_presence["advertisements"]["security"][field] = json!(1);
            assert!(wifi_advertisement_metrics(&exceeds_presence).is_err());
        }
        for field in ["wpa_parsed_frames", "wpa_malformed_frames"] {
            let mut exceeds_presence = valid.clone();
            exceeds_presence["advertisements"]["security"][field] = json!(1);
            assert!(wifi_advertisement_metrics(&exceeds_presence).is_err());
        }
        for field in ["pmf_capable_frames", "pmf_required_frames"] {
            let mut exceeds_parsed_rsn = valid.clone();
            exceeds_parsed_rsn["advertisements"]["security"]["rsn_parsed_frames"] = json!(0);
            exceeds_parsed_rsn["advertisements"]["security"][field] = json!(1);
            assert!(wifi_advertisement_metrics(&exceeds_parsed_rsn).is_err());
        }

        let mut contradictory_but_observable = valid.clone();
        contradictory_but_observable["advertisements"]["security"]["pmf_capable_frames"] = json!(0);
        contradictory_but_observable["advertisements"]["security"]["pmf_required_frames"] =
            json!(10);
        contradictory_but_observable["advertisements"]["security"]["rsn_malformed_frames"] =
            json!(10);
        assert!(wifi_advertisement_metrics(&contradictory_but_observable).is_ok());

        let mut extra_top_field = valid.clone();
        extra_top_field["advertisements"]["ssid"] = json!("not-retained");
        assert!(wifi_advertisement_metrics(&extra_top_field).is_err());

        let mut extra_phy_field = valid.clone();
        extra_phy_field["advertisements"]["phy"]["extension_ids"] = json!([35]);
        assert!(wifi_advertisement_metrics(&extra_phy_field).is_err());

        let mut extra_security_field = valid.clone();
        extra_security_field["advertisements"]["security"]["akm_suites"] = json!([]);
        assert!(wifi_advertisement_metrics(&extra_security_field).is_err());

        let mut missing_field = valid.clone();
        missing_field["advertisements"]["phy"]
            .as_object_mut()
            .unwrap()
            .remove("he_operation_element_frames");
        assert!(wifi_advertisement_metrics(&missing_field).is_err());

        for invalid in [json!(-1), json!(0.5), json!("10")] {
            let mut wrong_type = valid.clone();
            wrong_type["advertisements"]["security"]["rsn_present_frames"] = invalid;
            assert!(wifi_advertisement_metrics(&wrong_type).is_err());
        }
    }

    #[test]
    fn ieee_worker_requires_explicit_input_eof_not_a_successful_limit_stop() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        let path = PathBuf::from("logs/worker.stdout.log");
        let mut command = CommandReport {
            program: "ieee802154-futuresdr-worker",
            arguments: Vec::new(),
            exit_code: Some(0),
            exit_signal: None,
            success: true,
            timed_out: false,
            interrupted: false,
            sigkill_cleanup_sent: true,
            stdout: crate::process::LogReport {
                path: path.clone(),
                observed_bytes: 0,
                retained_bytes: 0,
                truncated: false,
            },
            stderr: crate::process::LogReport {
                path: PathBuf::from("logs/worker.stderr.log"),
                observed_bytes: 0,
                retained_bytes: 0,
                truncated: false,
            },
        };
        for (text, lane, accepted) in [
            (
                "Candidate archive: decoder lane hard, 0 records, 24 bytes, stop reason InputEof\n",
                "hard",
                true,
            ),
            (
                "Candidate archive: decoder lane soft, 0 records, 24 bytes, stop reason InputEof\n",
                "soft",
                true,
            ),
            (
                "Candidate archive: decoder lane soft, 0 records, 24 bytes, stop reason InputEof\n",
                "hard",
                false,
            ),
            (
                "Candidate archive: decoder lane hard, 0 records, 24 bytes, stop reason InputEof\nCandidate archive: decoder lane hard, 0 records, 24 bytes, stop reason InputEof\n",
                "hard",
                false,
            ),
            (
                "Candidate archive: decoder lane hard, 1 records, 64 bytes, stop reason FrameLimit\n",
                "hard",
                false,
            ),
            (
                "Candidate archive: decoder lane hard, 1 records, 64 bytes, stop reason OutputLimit\n",
                "hard",
                false,
            ),
        ] {
            fs::write(directory.path().join(&path), text).unwrap();
            command.stdout.observed_bytes = text.len() as u64;
            command.stdout.retained_bytes = text.len() as u64;
            assert_eq!(
                require_ieee_worker_eof(directory.path(), &command, lane).is_ok(),
                accepted
            );
        }
        command.stdout.truncated = true;
        assert!(require_ieee_worker_eof(directory.path(), &command, "hard").is_err());
    }

    #[test]
    fn ieee_hypothesis_bank_freezes_worker_and_comparison_argument_order() {
        assert_eq!(
            IEEE_HYPOTHESES.map(|spec| (
                spec.source_index,
                spec.decoder_lane,
                spec.carrier_offset_hz,
                spec.source_role,
            )),
            [
                (0, "hard", 0, "hard"),
                (1, "soft", 0, "soft"),
                (2, "hard", -100_000, "hard-cfo-neg100000"),
                (3, "soft", -100_000, "soft-cfo-neg100000"),
                (4, "hard", 100_000, "hard-cfo-pos100000"),
                (5, "soft", 100_000, "soft-cfo-pos100000"),
            ]
        );

        let input = Path::new("channel-17.cf32");
        let archive = Path::new("channel-17-hard-cfo-neg100000.i154raw");
        let worker = ieee_worker_arguments(input, archive, 17, 8, IEEE_HYPOTHESES[2]);
        assert_eq!(
            worker,
            [
                path(input),
                text("--output"),
                path(archive),
                text("--channel"),
                text(17),
                text("--max-input-mib"),
                text(8),
                text("--max-frames"),
                text(IEEE_MAX_FRAMES),
                text("--max-output-mib"),
                text(IEEE_ARCHIVE_MIB),
                text("--decoder-lane"),
                text("hard"),
                text("--carrier-offset-hz"),
                text(-100_000),
            ]
        );
        for spec in IEEE_HYPOTHESES {
            let arguments = ieee_worker_arguments(input, archive, 17, 8, spec);
            assert_eq!(
                &arguments[11..],
                &[
                    text("--decoder-lane"),
                    text(spec.decoder_lane),
                    text("--carrier-offset-hz"),
                    text(spec.carrier_offset_hz),
                ]
            );
        }

        let root = Path::new("ieee802154");
        let comparison_dir = root.join("channel-17-comparison");
        let comparison = ieee_comparison_arguments(17, root, &comparison_dir);
        assert_eq!(comparison[0], text("compare-hypotheses"));
        for (index, spec) in IEEE_HYPOTHESES.iter().enumerate() {
            let offset = 1 + index * 4;
            assert_eq!(comparison[offset], text("--source"));
            assert_eq!(comparison[offset + 1], text(spec.decoder_lane));
            assert_eq!(comparison[offset + 2], text(spec.carrier_offset_hz));
            assert_eq!(
                comparison[offset + 3],
                path(&root.join(spec.archive_name(17)))
            );
        }
        assert_eq!(
            &comparison[25..],
            &[
                text("--output"),
                path(&comparison_dir),
                text("--max-input-mib"),
                text(IEEE_ARCHIVE_MIB),
                text("--max-records"),
                text(IEEE_MAX_FRAMES),
            ]
        );
    }

    #[test]
    fn ieee_inspector_requires_the_exact_terminal_contract() {
        let archive = report::FileDigest {
            bytes: 49,
            sha256: "a".repeat(64),
        };
        let base = json!({
            "schema_version":1,
            "operation":"inspect_ieee802154_candidates",
            "source_format":"ieee802154_raw_candidates_v1",
            "source_bytes":49,
            "source_sha256":"a".repeat(64),
            "ieee802154_channel":16,
            "channel_sample_rate_hz":4_000_000,
            "recovered_chip_rate_hz":2_000_000,
            "candidate_record_count":1,
            "valid_fcs_record_count":1,
            "invalid_fcs_record_count":0,
            "invalid_length_record_count":0,
            "validated_psdu_bytes":5,
            "first_recovered_chip_count_at_end":704,
            "last_recovered_chip_count_at_end":704,
            "configured_max_input_bytes":IEEE_ARCHIVE_MAX_BYTES,
            "configured_max_output_bytes":INSPECTION_OUTPUT_MIB * 1024 * 1024,
            "configured_max_records":IEEE_MAX_FRAMES
        });
        assert!(validate_ieee_inspection(&base, 16, &archive).is_ok());

        let zero_archive = report::FileDigest {
            bytes: 24,
            sha256: "b".repeat(64),
        };
        let mut zero = base.clone();
        zero["source_bytes"] = json!(24);
        zero["source_sha256"] = json!("b".repeat(64));
        zero["candidate_record_count"] = json!(0);
        zero["valid_fcs_record_count"] = json!(0);
        zero["validated_psdu_bytes"] = json!(0);
        zero["first_recovered_chip_count_at_end"] = Value::Null;
        zero["last_recovered_chip_count_at_end"] = Value::Null;
        assert!(validate_ieee_inspection(&zero, 16, &zero_archive).is_ok());

        let two_record_archive = report::FileDigest {
            bytes: 70,
            sha256: "c".repeat(64),
        };
        let mut two_records = base.clone();
        two_records["source_bytes"] = json!(70);
        two_records["source_sha256"] = json!("c".repeat(64));
        two_records["candidate_record_count"] = json!(2);
        two_records["invalid_fcs_record_count"] = json!(1);
        two_records["last_recovered_chip_count_at_end"] = json!(705);
        assert!(validate_ieee_inspection(&two_records, 16, &two_record_archive).is_ok());
        two_records["last_recovered_chip_count_at_end"] = json!(704);
        assert!(validate_ieee_inspection(&two_records, 16, &two_record_archive).is_err());

        let mut wrong_schema = base.clone();
        wrong_schema["schema_version"] = json!(2);
        assert!(validate_ieee_inspection(&wrong_schema, 16, &archive).is_err());

        let mut wrong_operation = base.clone();
        wrong_operation["operation"] = json!("channelize");
        assert!(validate_ieee_inspection(&wrong_operation, 16, &archive).is_err());

        let mut wrong_channel = base.clone();
        wrong_channel["ieee802154_channel"] = json!(17);
        assert!(validate_ieee_inspection(&wrong_channel, 16, &archive).is_err());

        for (field, value) in [
            ("source_format", json!("other")),
            ("source_bytes", json!(48)),
            ("source_sha256", json!("c".repeat(64))),
            ("channel_sample_rate_hz", json!(2_000_000)),
            ("recovered_chip_rate_hz", json!(4_000_000)),
            ("candidate_record_count", json!(2)),
            ("validated_psdu_bytes", json!(1)),
            ("configured_max_records", json!(1)),
        ] {
            let mut wrong = base.clone();
            wrong[field] = value;
            assert!(validate_ieee_inspection(&wrong, 16, &archive).is_err());
        }
        let mut overflow = base;
        overflow["candidate_record_count"] = json!(u64::MAX);
        overflow["valid_fcs_record_count"] = json!(u64::MAX);
        assert!(validate_ieee_inspection(&overflow, 16, &archive).is_err());
    }

    #[test]
    fn ieee_hypothesis_comparison_requires_role_bound_identities_and_exact_closure() {
        let archives =
            std::array::from_fn::<_, IEEE_HYPOTHESIS_COUNT, _>(|index| report::FileDigest {
                bytes: match index {
                    0 => 70,
                    1 => 49,
                    _ => 24,
                },
                sha256: char::from(b'a' + index as u8).to_string().repeat(64),
            });
        let counts = [
            IeeeCounts {
                records: 2,
                valid: 1,
                invalid: 1,
                invalid_length: 0,
            },
            IeeeCounts {
                records: 1,
                valid: 1,
                invalid: 0,
                invalid_length: 0,
            },
            IeeeCounts::default(),
            IeeeCounts::default(),
            IeeeCounts::default(),
            IeeeCounts::default(),
        ];
        let inspections = counts
            .iter()
            .map(|counts| {
                json!({
                    "candidate_record_count":counts.records,
                    "valid_fcs_record_count":counts.valid,
                    "invalid_fcs_record_count":counts.invalid,
                    "invalid_length_record_count":counts.invalid_length
                })
            })
            .collect::<Vec<_>>();
        let sources = IEEE_HYPOTHESES
            .iter()
            .zip(&archives)
            .zip(counts)
            .map(|((spec, archive), counts)| {
                json!({
                    "source_index":spec.source_index,
                    "decoder_lane":spec.decoder_lane,
                    "carrier_offset_hz":spec.carrier_offset_hz,
                    "source_format":"ieee802154_raw_candidates_v1",
                    "source_bytes":archive.bytes,
                    "source_sha256":archive.sha256,
                    "candidate_record_count":counts.records,
                    "valid_fcs_record_count":counts.valid,
                    "invalid_fcs_record_count":counts.invalid,
                    "invalid_length_record_count":counts.invalid_length
                })
            })
            .collect::<Vec<_>>();
        let base = json!({
            "schema_version":1,
            "operation":"compare_ieee802154_candidate_hypotheses",
            "source_format":"ieee802154_raw_candidates_v1",
            "source_count":6,
            "sources":sources,
            "ieee802154_channel":16,
            "channel_sample_rate_hz":4_000_000,
            "recovered_chip_rate_hz":2_000_000,
            "source_candidate_record_count_sum":3,
            "source_valid_fcs_record_count_sum":2,
            "source_invalid_fcs_record_count_sum":1,
            "source_invalid_length_record_count_sum":0,
            "identity_union_record_count":2,
            "identity_union_valid_fcs_record_count":1,
            "identity_union_invalid_fcs_record_count":1,
            "identity_union_invalid_length_record_count":0,
            "duplicate_record_count":1,
            "duplicate_valid_fcs_record_count":1,
            "duplicate_invalid_fcs_record_count":0,
            "duplicate_invalid_length_record_count":0,
            "membership_partitions":[
                {
                    "source_membership_mask":1,
                    "source_membership_count":1,
                    "identity_record_count":1,
                    "valid_fcs_record_count":0,
                    "invalid_fcs_record_count":1,
                    "invalid_length_record_count":0
                },
                {
                    "source_membership_mask":3,
                    "source_membership_count":2,
                    "identity_record_count":1,
                    "valid_fcs_record_count":1,
                    "invalid_fcs_record_count":0,
                    "invalid_length_record_count":0
                }
            ],
            "configured_max_input_bytes_per_source":IEEE_ARCHIVE_MAX_BYTES,
            "configured_max_records_per_source":IEEE_MAX_FRAMES,
            "interpretation":"exact decoder-output identity union"
        });
        assert!(validate_ieee_comparison(&base, 16, &archives, &inspections).is_ok());
        assert_eq!(
            ieee_comparison_outcome(std::slice::from_ref(&base)).unwrap(),
            Outcome::EvidenceFound
        );

        for (field, value) in [
            ("schema_version", json!(2)),
            ("operation", json!("inspect_ieee802154_candidates")),
            ("source_format", json!("other")),
            ("source_count", json!(5)),
            ("ieee802154_channel", json!(17)),
            ("channel_sample_rate_hz", json!(2_000_000)),
            ("recovered_chip_rate_hz", json!(4_000_000)),
            ("source_candidate_record_count_sum", json!(2)),
            ("source_valid_fcs_record_count_sum", json!(1)),
            ("source_invalid_fcs_record_count_sum", json!(0)),
            ("identity_union_record_count", json!(1)),
            ("identity_union_valid_fcs_record_count", json!(2)),
            ("duplicate_record_count", json!(0)),
            ("configured_max_input_bytes_per_source", json!(1)),
            ("configured_max_records_per_source", json!(1)),
            ("interpretation", json!("")),
        ] {
            let mut wrong = base.clone();
            wrong[field] = value;
            assert!(
                validate_ieee_comparison(&wrong, 16, &archives, &inspections).is_err(),
                "comparison mutation in {field} was accepted"
            );
        }

        let mut extra_field = base.clone();
        extra_field["source_file"] = json!("not-retained");
        assert!(validate_ieee_comparison(&extra_field, 16, &archives, &inspections).is_err());

        for pointer in [
            "/sources/0/source_index",
            "/sources/0/decoder_lane",
            "/sources/0/carrier_offset_hz",
            "/sources/0/source_sha256",
            "/sources/0/invalid_fcs_record_count",
            "/membership_partitions/0/source_membership_mask",
            "/membership_partitions/0/source_membership_count",
            "/membership_partitions/0/valid_fcs_record_count",
        ] {
            let mut wrong = base.clone();
            *wrong.pointer_mut(pointer).unwrap() = json!(99);
            assert!(
                validate_ieee_comparison(&wrong, 16, &archives, &inspections).is_err(),
                "comparison mutation at {pointer} was accepted"
            );
        }
        let mut source_extra = base.clone();
        source_extra["sources"][0]["path"] = json!("not-retained");
        assert!(validate_ieee_comparison(&source_extra, 16, &archives, &inspections).is_err());
        let mut membership_extra = base.clone();
        membership_extra["membership_partitions"][0]["psdu"] = json!("not-retained");
        assert!(validate_ieee_comparison(&membership_extra, 16, &archives, &inspections).is_err());
        assert!(
            ieee_comparison_outcome(&[
                json!({
                    "identity_union_record_count":u64::MAX,
                    "identity_union_valid_fcs_record_count":0
                }),
                json!({
                    "identity_union_record_count":1,
                    "identity_union_valid_fcs_record_count":0
                })
            ])
            .is_err()
        );
    }

    #[test]
    fn channelizer_contract_requires_exact_decimation_and_output_extent() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("channelized");
        fs::create_dir(&output).unwrap();
        for channel in IEEE_CHANNELS {
            fs::write(
                output.join(format!("channel-{channel}.cf32")),
                vec![0u8; 1640 * 8],
            )
            .unwrap();
        }
        let channels: Vec<_> = IEEE_CHANNELS
            .into_iter()
            .map(|channel| {
                let path = output.join(format!("channel-{channel}.cf32"));
                let digest = report::hash_direct_regular_file(&path, 1640 * 8).unwrap();
                json!({
                    "channel":channel,
                    "output_file":format!("channel-{channel}.cf32"),
                    "output_format":"CF32_LE",
                    "output_sample_rate_hz":4_000_000,
                    "output_complex_samples":1640,
                    "sha256":digest.sha256
                })
            })
            .collect();
        let mut summary = json!({
            "schema_version":1,
            "operation":"offline_ieee802154_channelization",
            "source":{
                "available_complex_samples":8200,
                "processed_complex_samples":8200,
                "processed_full_input":true,
                "center_frequency_hz":2_437_000_000u64,
                "sample_rate_hz":20_000_000,
                "source_bytes":32_800,
                "processed_prefix_bytes":32_800,
                "processed_prefix_sha256":"0000000000000000000000000000000000000000000000000000000000000000"
            },
            "filter":{"decimation":5},
            "channels":channels
        });
        let validated = validate_channelize(&summary, &source(), &output).unwrap();
        assert_eq!(validated.worker_input_mib, 1);
        assert_eq!(validated.channels.len(), 3);
        assert_eq!(validated.channels[0].channel, 16);
        assert_eq!(validated.channels[0].digest.bytes, 1640 * 8);

        let mut edged = summary.clone();
        edged["filter"]["passband_edge_hz"] = json!(1_500_000);
        edged["filter"]["stopband_edge_hz"] = json!(2_000_000);
        let mut edge_channel = edged["channels"][2].clone();
        edge_channel["channel"] = json!(19);
        edge_channel["output_file"] = json!("channel-19.cf32");
        edged["channels"].as_array_mut().unwrap().push(edge_channel);
        let requested = [16, 17, 18, 19];
        for (channel, (center, offset, margin)) in
            edged["channels"].as_array_mut().unwrap().iter_mut().zip([
                (2_430_000_000u64, -7_000_000, 1_000_000),
                (2_435_000_000, -2_000_000, 6_000_000),
                (2_440_000_000, 3_000_000, 5_000_000),
                (2_445_000_000, 8_000_000, 0),
            ])
        {
            channel["center_frequency_hz"] = json!(center);
            channel["source_offset_hz"] = json!(offset);
            channel["source_edge_margin_hz"] = json!(margin);
        }
        assert!(validate_channelize(&edged, &source(), &output).is_err());
        assert!(validate_channelize_for(&edged, &source(), &output, requested).is_err());
        let edge_file = output.join("channel-19.cf32");
        fs::copy(output.join("channel-18.cf32"), &edge_file).unwrap();
        let validated_edge =
            validate_channelize_for(&edged, &source(), &output, requested).unwrap();
        assert_eq!(validated_edge.channel(19).unwrap().digest.bytes, 1640 * 8);
        edged["channels"][3]["source_edge_margin_hz"] = json!(1);
        assert!(validate_channelize_for(&edged, &source(), &output, requested).is_err());

        let channel_16 = output.join("channel-16.cf32");
        let replacement = output.join("replacement.cf32");
        validated.channel(16).unwrap().verify_unchanged().unwrap();
        fs::copy(&channel_16, &replacement).unwrap();
        fs::rename(&replacement, &channel_16).unwrap();
        assert!(validated.channel(16).unwrap().verify_unchanged().is_err());
        let validated = validate_channelize(&summary, &source(), &output).unwrap();

        let mut mutated = fs::read(&channel_16).unwrap();
        mutated[0] = 1;
        fs::write(&channel_16, &mutated).unwrap();
        assert!(validated.channel(16).unwrap().verify_unchanged().is_err());
        assert!(validate_channelize(&summary, &source(), &output).is_err());
        mutated[0] = 0;
        fs::write(&channel_16, &mutated).unwrap();

        let digest = summary["channels"][0]["sha256"].clone();
        summary["channels"][0]["sha256"] = json!("f".repeat(64));
        assert!(validate_channelize(&summary, &source(), &output).is_err());
        summary["channels"][0]["sha256"] = digest;

        let channel_18 = summary["channels"][2].clone();
        summary["channels"][2] = summary["channels"][0].clone();
        assert!(validate_channelize(&summary, &source(), &output).is_err());
        summary["channels"][2] = channel_18;

        summary["operation"] = json!("inspect_ieee802154_candidates");
        assert!(validate_channelize(&summary, &source(), &output).is_err());
        summary["operation"] = json!("offline_ieee802154_channelization");
        summary["filter"]["decimation"] = json!(4);
        assert!(validate_channelize(&summary, &source(), &output).is_err());
        summary["filter"]["decimation"] = json!(5);
        summary["channels"][0]["output_complex_samples"] = json!(1639);
        assert!(validate_channelize(&summary, &source(), &output).is_err());
    }

    #[test]
    fn bluetooth_requires_terminal_safety_and_consistent_counts() {
        let base = json!({
            "schema_version":1,
            "processed_full_input":true,
            "radio_opened":false,
            "network_requested":false,
            "transmit_requested":false,
            "input_complex_samples":8200,
            "decoder_processed_complex_samples":8200,
            "channelizer_step_complex_samples":10,
            "decoder_file_block_complex_samples":65530,
            "channelizer_steps_processed":820,
            "candidate_records":0,
            "validated_ble_packets":0,
            "outcome":"completed_zero_candidates"
        });
        assert_eq!(
            bluetooth_outcome(&base, &source()).unwrap(),
            Outcome::CompletedZeroCandidates
        );
        let mut schema_two = base.clone();
        schema_two["schema_version"] = json!(2);
        schema_two["coded_scan_requested"] = json!(false);
        schema_two["coded_scan_primary_frequencies_mhz"] = json!([]);
        assert_eq!(
            bluetooth_outcome(&schema_two, &source()).unwrap(),
            Outcome::CompletedZeroCandidates
        );

        let mut active_coded_scan = schema_two.clone();
        active_coded_scan["coded_scan_requested"] = json!(true);
        assert!(bluetooth_outcome(&active_coded_scan, &source()).is_err());

        let mut coded_frequency = schema_two.clone();
        coded_frequency["coded_scan_primary_frequencies_mhz"] = json!([2426]);
        assert!(bluetooth_outcome(&coded_frequency, &source()).is_err());

        for field in ["coded_scan_requested", "coded_scan_primary_frequencies_mhz"] {
            let mut missing = schema_two.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(bluetooth_outcome(&missing, &source()).is_err());
        }
        let mut wrong_requested_type = schema_two.clone();
        wrong_requested_type["coded_scan_requested"] = json!(0);
        assert!(bluetooth_outcome(&wrong_requested_type, &source()).is_err());
        let mut wrong_frequency_type = schema_two;
        wrong_frequency_type["coded_scan_primary_frequencies_mhz"] = json!(false);
        assert!(bluetooth_outcome(&wrong_frequency_type, &source()).is_err());

        let mut validated = base.clone();
        validated["candidate_records"] = json!(1);
        validated["validated_ble_packets"] = json!(1);
        validated["outcome"] = json!("completed_with_validated_ble");
        assert_eq!(
            bluetooth_outcome(&validated, &source()).unwrap(),
            Outcome::EvidenceFound
        );

        let mut impossible_valid_count = validated;
        impossible_valid_count["validated_ble_packets"] = json!(2);
        assert!(bluetooth_outcome(&impossible_valid_count, &source()).is_err());

        let mut unsafe_report = base;
        unsafe_report["radio_opened"] = Value::Bool(true);
        assert!(bluetooth_outcome(&unsafe_report, &source()).is_err());

        let mut unaligned = unsafe_report;
        unaligned["radio_opened"] = Value::Bool(false);
        unaligned["decoder_file_block_complex_samples"] = json!(65_536);
        assert!(bluetooth_outcome(&unaligned, &source()).is_err());

        let mut missing_consumed_extent = unaligned;
        missing_consumed_extent["decoder_file_block_complex_samples"] = json!(65_530);
        missing_consumed_extent
            .as_object_mut()
            .unwrap()
            .remove("decoder_processed_complex_samples");
        assert!(bluetooth_outcome(&missing_consumed_extent, &source()).is_err());
    }

    #[test]
    fn burst_family_contract_binds_source_settings_partitions_and_features() {
        let (summary, bursts, digest) = burst_family_fixture(true);
        assert_eq!(
            burst_family_outcome(&summary, &bursts, &digest).unwrap(),
            Outcome::UnclassifiedActivity
        );
        let (empty, empty_bursts, empty_digest) = burst_family_fixture(false);
        assert_eq!(
            burst_family_outcome(&empty, &empty_bursts, &empty_digest).unwrap(),
            Outcome::NoUnclassifiedActivityDetected
        );

        let mut changed = summary.clone();
        changed["protocol_identity_established"] = json!(true);
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["settings"]["active_power_divisor"] = json!(8);
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["source"]["sha256"] = json!("b".repeat(64));
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["accounting"]["feature_insufficient_record_count"] = json!(1);
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["families"][4]["candidate_family"] = json!("identifier_like_label");
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["frequency_family_counts"][0]["complex_sample_count"] = json!(3);
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary.clone();
        changed["eligible_record_features"]["usable_phase_differences"]["mean"] = json!(127.0);
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
        changed = summary;
        changed["private_path"] = json!("/tmp/private");
        assert!(burst_family_outcome(&changed, &bursts, &digest).is_err());
    }

    #[test]
    fn request_validation_enforces_time_and_channel_coverage_bounds() {
        let paths = ProgramPaths {
            wifi: "wifi".into(),
            rf: "rf".into(),
            ieee: "ieee".into(),
            ieee_worker: "worker".into(),
            bluetooth: "bluetooth".into(),
        };
        let mut request = Request {
            input: "input".into(),
            output: "output".into(),
            center_mhz: 2437,
            sample_rate_hz: 20_000_000,
            max_samples: 20_000_000,
            spectrum_windows: 128,
            decoder_timeout_seconds: 120,
            child_timeout_seconds: 180,
            log_limit_mib: 1,
            programs: paths,
        };
        assert_eq!(
            validate_request(&request).unwrap(),
            (2_437_000_000, 1_048_576)
        );
        request.center_mhz = 2442;
        assert!(validate_request(&request).is_err());
        request.center_mhz = 2437;
        request.child_timeout_seconds = 120;
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn ieee_integrity_failure_reduction_is_fixed_bounded_and_ordered() {
        let results: [Result<u8>; 3] = [
            (16, BranchResult::SpawnFailed),
            (17, BranchResult::Panicked),
            (18, BranchResult::Complete((99, Ok(18)))),
        ]
        .map(|(channel, result)| {
            reduce_ieee_integrity_result(IeeeIntegrityPhase::Terminal, channel, result)
        });
        let details = results.map(|result| result.unwrap_err().to_string());
        assert_eq!(
            details,
            [
                "could not start IEEE 802.15.4 channel 16 terminal-integrity worker",
                "IEEE 802.15.4 channel 17 terminal-integrity worker panicked",
                "IEEE 802.15.4 channel 18 terminal-integrity worker returned wrong channel",
            ]
        );
        for detail in details {
            assert!(detail.len() <= 80);
            assert!(!detail.contains('/'));
            assert!(!detail.contains('\\'));
        }

        let retained: Result<u8> = reduce_ieee_integrity_result(
            IeeeIntegrityPhase::InputHash,
            16,
            BranchResult::Complete((16, Err(anyhow::anyhow!("retained validation error")))),
        );
        assert_eq!(
            retained.unwrap_err().to_string(),
            "retained validation error"
        );
    }

    #[test]
    fn ieee_integrity_thread_names_are_phase_bound() {
        assert_eq!(
            IEEE_CHANNELS.map(|channel| IeeeIntegrityPhase::InputHash.thread_name(channel)),
            [
                "ieee802154-hash-16",
                "ieee802154-hash-17",
                "ieee802154-hash-18",
            ]
        );
        assert_eq!(
            IEEE_CHANNELS.map(|channel| IeeeIntegrityPhase::Terminal.thread_name(channel)),
            [
                "ieee802154-terminal-16",
                "ieee802154-terminal-17",
                "ieee802154-terminal-18",
            ]
        );
    }

    #[test]
    fn ieee_integrity_workers_overlap_join_and_restore_fixed_order() {
        let releases = Arc::new([
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ]);
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let scheduler_releases = Arc::clone(&releases);
        let scheduler = thread::Builder::new()
            .name("ieee802154-integrity-scheduler-test".into())
            .spawn(move || {
                let results = parallel_ieee_integrity_tasks(
                    IeeeIntegrityPhase::InputHash,
                    IEEE_CHANNELS,
                    |channel| {
                        started_tx
                            .send((
                                channel,
                                thread::current().name().unwrap_or("unnamed").to_owned(),
                            ))
                            .unwrap();
                        let index = usize::from(channel - IEEE_CHANNELS[0]);
                        let deadline = Instant::now() + TEST_SYNC_TIMEOUT + TEST_SYNC_TIMEOUT;
                        while !scheduler_releases[index].load(Ordering::Acquire) {
                            assert!(
                                Instant::now() < deadline,
                                "timed out waiting to release IEEE integrity channel {channel}"
                            );
                            thread::yield_now();
                        }
                        if channel == 17 {
                            panic!("synthetic IEEE integrity panic");
                        }
                        completed_tx.send(channel).unwrap();
                        Ok(channel)
                    },
                );
                result_tx.send(results).unwrap();
            })
            .unwrap();

        let mut started = Vec::new();
        for _ in 0..IEEE_CHANNELS.len() {
            match started_rx.recv_timeout(TEST_SYNC_TIMEOUT) {
                Ok(start) => started.push(start),
                Err(_) => break,
            }
        }
        releases[0].store(true, Ordering::Release);
        releases[1].store(true, Ordering::Release);
        let first_completed = completed_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok();
        let (early_results, result_disconnected) = match result_rx.try_recv() {
            Ok(results) => (Some(results), false),
            Err(TryRecvError::Empty) => (None, false),
            Err(TryRecvError::Disconnected) => (None, true),
        };
        let returned_while_channel_18_was_held = early_results.is_some();
        releases[2].store(true, Ordering::Release);
        let second_completed = completed_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok();
        let results = early_results.or_else(|| result_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok());
        let scheduler_joined = scheduler.join().is_ok();

        started.sort_unstable();
        assert_eq!(
            started,
            [
                (16, "ieee802154-hash-16".to_owned()),
                (17, "ieee802154-hash-17".to_owned()),
                (18, "ieee802154-hash-18".to_owned()),
            ]
        );
        assert_eq!(first_completed, Some(16));
        assert!(!result_disconnected);
        assert!(!returned_while_channel_18_was_held);
        assert_eq!(second_completed, Some(18));
        assert!(scheduler_joined);
        let [channel_16, channel_17, channel_18] = results.unwrap();
        assert_eq!(channel_16.unwrap(), 16);
        assert_eq!(
            channel_17.unwrap_err().to_string(),
            "IEEE 802.15.4 channel 17 input-hash worker panicked"
        );
        assert_eq!(channel_18.unwrap(), 18);
    }

    #[test]
    fn ieee_channel_failure_reduction_is_fixed_bounded_and_ordered() {
        let runs = reduce_ieee_channel_results([
            BranchResult::SpawnFailed,
            BranchResult::Panicked,
            BranchResult::Complete(test_ieee_channel_run(99, "discarded mismatched result")),
        ]);
        assert_eq!(
            runs.iter().map(|run| run.channel).collect::<Vec<_>>(),
            IEEE_CHANNELS
        );
        assert_eq!(
            runs[0].errors,
            ["could not start IEEE 802.15.4 channel 16 worker"]
        );
        assert_eq!(runs[1].errors, ["IEEE 802.15.4 channel 17 worker panicked"]);
        assert_eq!(
            runs[2].errors,
            ["IEEE 802.15.4 channel 18 worker returned an unexpected channel"]
        );
        for run in runs {
            assert!(run.commands.is_empty());
            assert!(run.evidence.is_none());
            assert!(!run.interrupted);
            for detail in run.errors {
                assert!(detail.len() <= 80);
                assert!(!detail.contains('/'));
                assert!(!detail.contains('\\'));
            }
        }
    }

    #[test]
    fn ieee_channel_initial_stop_starts_no_worker_and_retains_order() {
        let calls = AtomicUsize::new(0);
        let runs = parallel_ieee_channels(&AtomicBool::new(true), |channel| {
            calls.fetch_add(1, Ordering::Relaxed);
            test_ieee_channel_run(channel, "unexpected worker invocation")
        });
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            runs.iter().map(|run| run.channel).collect::<Vec<_>>(),
            IEEE_CHANNELS
        );
        for (run, channel) in runs.iter().zip(IEEE_CHANNELS) {
            assert!(run.interrupted);
            assert_eq!(
                run.errors,
                [format!(
                    "interrupted before IEEE 802.15.4 channel {channel}"
                )]
            );
        }
    }

    #[test]
    fn ieee_channel_workers_overlap_join_and_restore_fixed_order() {
        let releases = Arc::new([
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ]);
        let (started_tx, started_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let scheduler_releases = Arc::clone(&releases);
        let scheduler = thread::Builder::new()
            .name("ieee802154-scheduler-test".into())
            .spawn(move || {
                let stop = AtomicBool::new(false);
                let runs = parallel_ieee_channels(&stop, |channel| {
                    started_tx
                        .send((
                            channel,
                            thread::current().name().unwrap_or("unnamed").to_owned(),
                        ))
                        .unwrap();
                    let index = usize::from(channel - IEEE_CHANNELS[0]);
                    let deadline = Instant::now() + TEST_SYNC_TIMEOUT + TEST_SYNC_TIMEOUT;
                    while !scheduler_releases[index].load(Ordering::Acquire) {
                        assert!(
                            Instant::now() < deadline,
                            "timed out waiting to release IEEE channel {channel}"
                        );
                        thread::yield_now();
                    }
                    if channel == 17 {
                        panic!("synthetic IEEE channel panic");
                    }
                    completed_tx.send(channel).unwrap();
                    test_ieee_channel_run(channel, format!("completed channel {channel}"))
                });
                result_tx.send(runs).unwrap();
            })
            .unwrap();

        let mut started = Vec::new();
        for _ in 0..IEEE_CHANNELS.len() {
            match started_rx.recv_timeout(TEST_SYNC_TIMEOUT) {
                Ok(start) => started.push(start),
                Err(_) => break,
            }
        }
        releases[0].store(true, Ordering::Release);
        releases[1].store(true, Ordering::Release);
        let first_completed = completed_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok();
        let (early_runs, result_disconnected) = match result_rx.try_recv() {
            Ok(runs) => (Some(runs), false),
            Err(TryRecvError::Empty) => (None, false),
            Err(TryRecvError::Disconnected) => (None, true),
        };
        let returned_while_channel_18_was_held = early_runs.is_some();
        releases[2].store(true, Ordering::Release);
        let second_completed = completed_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok();
        let runs = early_runs.or_else(|| result_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok());
        let scheduler_joined = scheduler.join().is_ok();

        started.sort_unstable();
        assert_eq!(
            started,
            [
                (16, "ieee802154-channel-16".to_owned()),
                (17, "ieee802154-channel-17".to_owned()),
                (18, "ieee802154-channel-18".to_owned()),
            ]
        );
        assert_eq!(first_completed, Some(16));
        assert!(!result_disconnected);
        assert!(!returned_while_channel_18_was_held);
        assert_eq!(second_completed, Some(18));
        assert!(scheduler_joined);
        let runs = runs.unwrap();
        assert_eq!(
            runs.iter().map(|run| run.channel).collect::<Vec<_>>(),
            IEEE_CHANNELS
        );
        assert_eq!(runs[0].errors, ["completed channel 16"]);
        assert_eq!(runs[1].errors, ["IEEE 802.15.4 channel 17 worker panicked"]);
        assert_eq!(runs[2].errors, ["completed channel 18"]);
    }

    #[test]
    fn branch_failure_reduction_is_fixed_bounded_and_ordered() {
        let spawn_failures = reduce_branch_results(
            BranchResult::SpawnFailed,
            BranchResult::Panicked,
            BranchResult::Complete(complete_stage("ieee802154")),
            BranchResult::SpawnFailed,
        );
        assert_eq!(
            spawn_failures
                .iter()
                .map(|stage| stage.name)
                .collect::<Vec<_>>(),
            STAGE_NAMES
        );
        assert_eq!(
            spawn_failures[0].detail.as_deref(),
            Some("could not start Wi-Fi analysis branch")
        );
        assert_eq!(
            spawn_failures[1].detail.as_deref(),
            Some("RF survey analysis branch panicked")
        );
        assert_eq!(spawn_failures[2].state, StageState::Complete);
        assert_eq!(
            spawn_failures[3].detail.as_deref(),
            Some("could not start Bluetooth analysis branch")
        );
        assert_eq!(
            spawn_failures[4].detail.as_deref(),
            Some("Bluetooth analysis branch could not start before burst-family analysis")
        );

        let bluetooth_panic = reduce_branch_results(
            BranchResult::Complete(complete_stage("wifi")),
            BranchResult::Complete(complete_stage("rf_survey")),
            BranchResult::Complete(complete_stage("ieee802154")),
            BranchResult::Panicked,
        );
        assert_eq!(
            bluetooth_panic[3].detail.as_deref(),
            Some("Bluetooth analysis branch panicked")
        );
        assert_eq!(
            bluetooth_panic[4].detail.as_deref(),
            Some("Bluetooth analysis branch panicked before completing burst-family analysis")
        );
        for detail in spawn_failures
            .iter()
            .chain(&bluetooth_panic)
            .filter_map(|stage| stage.detail.as_deref())
        {
            assert!(detail.len() <= 80);
            assert!(!detail.contains('/'));
            assert!(!detail.contains('\\'));
        }
    }

    #[test]
    fn parallel_stage_panics_keep_fixed_reports_and_join_every_branch() {
        let stages = analysis_stages(
            [
                complete_wifi,
                panic_rf,
                complete_ieee,
                panic_bluetooth,
                complete_burst,
            ],
            &test_programs(),
            &source(),
            Path::new("output"),
            &test_settings(),
            &AtomicBool::new(false),
        );
        assert_eq!(
            stages.iter().map(|stage| stage.name).collect::<Vec<_>>(),
            STAGE_NAMES
        );
        assert_eq!(stages[0].state, StageState::Complete);
        assert_eq!(stages[1].state, StageState::Failed);
        assert_eq!(
            stages[1].detail.as_deref(),
            Some("analysis stage worker panicked")
        );
        assert_eq!(stages[2].state, StageState::Complete);
        assert_eq!(stages[3].state, StageState::Failed);
        assert_eq!(
            stages[3].detail.as_deref(),
            Some("analysis stage worker panicked")
        );
        assert_eq!(stages[4].state, StageState::Complete);
    }

    #[test]
    fn initial_stop_skips_every_stage_in_fixed_order() {
        let stop = AtomicBool::new(true);
        let stages = analysis_stages(
            [panic_rf; 5],
            &test_programs(),
            &source(),
            Path::new("output"),
            &test_settings(),
            &stop,
        );
        assert_eq!(
            stages.iter().map(|stage| stage.name).collect::<Vec<_>>(),
            STAGE_NAMES
        );
        assert!(
            stages
                .iter()
                .all(|stage| stage.state == StageState::Skipped)
        );
        assert!(
            stages
                .iter()
                .all(|stage| stage.detail.as_deref() == Some("skipped after interruption"))
        );
    }

    #[test]
    fn parallel_branches_overlap_order_dependencies_and_wait_for_every_join() {
        fn started_name() -> String {
            thread::current().name().unwrap_or("unnamed").to_owned()
        }

        fn ordinary_branch(
            started: mpsc::Sender<String>,
            release: mpsc::Receiver<()>,
            stage_name: &'static str,
        ) -> StageReport {
            started.send(started_name()).unwrap();
            release.recv_timeout(TEST_SYNC_TIMEOUT).unwrap();
            complete_stage(stage_name)
        }

        let (started_tx, started_rx) = mpsc::channel();
        let (wifi_release_tx, wifi_release_rx) = mpsc::channel();
        let (rf_release_tx, rf_release_rx) = mpsc::channel();
        let (ieee_release_tx, ieee_release_rx) = mpsc::channel();
        let (bluetooth_release_tx, bluetooth_release_rx) = mpsc::channel();
        let (ieee_held_tx, ieee_held_rx) = mpsc::channel();
        let (ieee_finish_tx, ieee_finish_rx) = mpsc::channel();
        let (bluetooth_order_tx, bluetooth_order_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();

        let wifi_started = started_tx.clone();
        let rf_started = started_tx.clone();
        let ieee_started = started_tx.clone();
        let bluetooth_started = started_tx;
        let scheduler = thread::Builder::new()
            .name("digital-analysis-scheduler-test".into())
            .spawn(move || {
                let reports = parallel_branches(
                    move || ordinary_branch(wifi_started, wifi_release_rx, "wifi"),
                    move || ordinary_branch(rf_started, rf_release_rx, "rf_survey"),
                    move || {
                        ieee_started.send(started_name()).unwrap();
                        ieee_release_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap();
                        ieee_held_tx.send(()).unwrap();
                        ieee_finish_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap();
                        complete_stage("ieee802154")
                    },
                    move || {
                        bluetooth_started.send(started_name()).unwrap();
                        bluetooth_release_rx
                            .recv_timeout(TEST_SYNC_TIMEOUT)
                            .unwrap();
                        let bluetooth = complete_stage("bluetooth");
                        bluetooth_order_tx.send("bluetooth").unwrap();
                        let burst = complete_stage("burst_families");
                        bluetooth_order_tx.send("burst_families").unwrap();
                        [bluetooth, burst]
                    },
                );
                result_tx.send(reports).unwrap();
            })
            .unwrap();

        let mut started = Vec::new();
        for _ in 0..4 {
            match started_rx.recv_timeout(TEST_SYNC_TIMEOUT) {
                Ok(name) => started.push(name),
                Err(_) => break,
            }
        }
        for release in [
            wifi_release_tx,
            rf_release_tx,
            ieee_release_tx,
            bluetooth_release_tx,
        ] {
            let _ = release.send(());
        }

        let ieee_was_held = ieee_held_rx.recv_timeout(TEST_SYNC_TIMEOUT).is_ok();
        let (early_reports, result_disconnected) = match result_rx.try_recv() {
            Ok(reports) => (Some(reports), false),
            Err(TryRecvError::Empty) => (None, false),
            Err(TryRecvError::Disconnected) => (None, true),
        };
        let returned_while_ieee_was_held = early_reports.is_some();
        let _ = ieee_finish_tx.send(());
        let reports = early_reports.or_else(|| result_rx.recv_timeout(TEST_SYNC_TIMEOUT).ok());
        let scheduler_joined = scheduler.join().is_ok();

        started.sort_unstable();
        assert_eq!(
            started,
            [
                "digital-bluetooth-analysis",
                "digital-ieee802154-analysis",
                "digital-rf-analysis",
                "digital-wifi-analysis",
            ]
        );
        assert!(ieee_was_held);
        assert!(!result_disconnected);
        assert!(!returned_while_ieee_was_held);
        assert!(scheduler_joined);
        let reports = reports.unwrap();
        assert_eq!(
            reports.iter().map(|stage| stage.name).collect::<Vec<_>>(),
            STAGE_NAMES
        );
        assert!(
            reports
                .iter()
                .all(|stage| stage.state == StageState::Complete)
        );
        assert_eq!(
            [
                bluetooth_order_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap(),
                bluetooth_order_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap(),
            ],
            ["bluetooth", "burst_families"]
        );
    }

    #[test]
    fn sequential_reference_and_parallel_scheduler_restore_the_same_order() {
        let programs = test_programs();
        let source = source();
        let settings = test_settings();
        let runners = [
            complete_wifi,
            complete_rf,
            complete_ieee,
            complete_bluetooth,
            complete_burst,
        ];
        let stop = AtomicBool::new(false);
        let sequential = sequential_stages(
            runners,
            &programs,
            &source,
            Path::new("output"),
            &settings,
            &stop,
        );
        let parallel = analysis_stages(
            runners,
            &programs,
            &source,
            Path::new("output"),
            &settings,
            &stop,
        );
        for stages in [sequential, parallel] {
            assert_eq!(
                stages.iter().map(|stage| stage.name).collect::<Vec<_>>(),
                STAGE_NAMES
            );
            assert!(
                stages
                    .iter()
                    .all(|stage| stage.state == StageState::Complete)
            );
        }
    }

    #[test]
    fn parallel_stage_failures_still_publish_a_terminal_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("iq.sc16q11");
        fs::write(&input, vec![0u8; 8200 * 4]).unwrap();
        let mut sidecar_name = input.as_os_str().to_os_string();
        sidecar_name.push(".json");
        fs::write(
            PathBuf::from(sidecar_name),
            serde_json::to_vec(&json!({
                "format":"SC16_Q11", "dtype":"<i2", "interleave":"I,Q",
                "complex_scale":2048, "frequency_hz":2_437_000_000.0,
                "sample_rate_hz":20_000_000.0, "bandwidth_hz":20_000_000.0,
                "num_samples":8200
            }))
            .unwrap(),
        )
        .unwrap();
        let executable = PathBuf::from("/bin/true");
        let output = directory.path().join("analysis");
        let (published, state) = analyze(
            Request {
                input,
                output: output.clone(),
                center_mhz: 2437,
                sample_rate_hz: 20_000_000,
                max_samples: 8200,
                spectrum_windows: 1,
                decoder_timeout_seconds: 1,
                child_timeout_seconds: 6,
                log_limit_mib: 1,
                programs: ProgramPaths {
                    wifi: executable.clone(),
                    rf: executable.clone(),
                    ieee: executable.clone(),
                    ieee_worker: executable.clone(),
                    bluetooth: executable,
                },
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(published, output.canonicalize().unwrap());
        assert_eq!(state, OverallState::Failed);
        let manifest: Value =
            serde_json::from_slice(&fs::read(output.join("analysis.json")).unwrap()).unwrap();
        assert_eq!(manifest["overall_state"], "failed");
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["stages"].as_array().unwrap().len(), 6);
        assert_eq!(
            manifest["stages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|stage| stage["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "wifi",
                "rf_survey",
                "ieee802154",
                "bluetooth",
                "burst_families",
                "terminal_integrity",
            ]
        );
        assert_eq!(manifest["stages"][4]["name"], "burst_families");
        assert_eq!(manifest["stages"][5]["name"], "terminal_integrity");
        assert_eq!(manifest["stages"][5]["state"], "complete");
    }
}
