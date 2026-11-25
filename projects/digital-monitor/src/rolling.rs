//! Supervise one RX1 stream and survey only verified, closed segments.

mod burst;
mod cadence;
mod clip_probe;
mod clips;
mod coded_sentinel;
mod decode;
mod detection;
mod direct;
mod he_clip;
mod he_payload;
mod intake;
mod overlap;
mod pruning;
mod recovery;
mod retention;
mod wifi_ht;

use std::cmp::Reverse;
use std::collections::{BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::AnalyzerPrograms;
use crate::event_replay::replay::{WifiReplayOptions, WifiTuning};
use crate::orchestrate::ieee_nominal::{self, NominalOwnership, NominalPlan, NominalReport};
use crate::process::{self, CommandReport, ProgramProvenance};
use crate::report::{self, OverallState};
use crate::watch::{MAX_DISCARD_INITIAL_SAMPLES, read_complete_capture_log};

use self::burst::{Band, BurstCounts, BurstReport};
use self::clips::{
    ClipReport, ClipSelection, HeSuSignalContext, HtSignalContext, ProbeCounts, RetainOptions,
};
use self::decode::DecodeReport;
use self::detection::{Detection, WifiActivityWindows};
use self::direct::{BoundaryDecode, DirectPlan, DirectReport};
use self::intake::{
    DEFAULT_KEEP_LIMIT, MAX_SEGMENTS, SEGMENT_SAMPLES, SegmentSettings, ValidatedSegment,
    read_segment,
};
use self::overlap::{CONTEXT_SAMPLES, PREFIX_SAMPLES, Tail, TailProof};
use self::pruning::RetiredPrefix;
use self::retention::{ReleaseEvidence, ReleaseProof};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const BASE_CAPTURE_TIMEOUT_SECONDS: u64 = 45;
const CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const PUBLICATION_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOG_LIMIT_BYTES: u64 = 1024 * 1024;
const MAX_SEGMENT_WORKERS: i64 = 6;
const MAX_SPOOL_SEGMENTS: i64 = 120;
const MAX_USB_BUFFER_SAMPLES: u32 = 65_536;
const USB_BUFFER_SAMPLE_ALIGNMENT: u32 = 2048;
const BACKGROUND_REPLAY_NICE: i32 = 15;
const _: () = assert!(PREFIX_SAMPLES <= ieee_nominal::MAX_BOUNDARY_PREFIX_SAMPLES);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum DecodeSelection {
    Earliest,
    Activity,
}

#[derive(Debug, Args)]
pub(crate) struct Arguments {
    /// New private directory for captures, detection results and the terminal report.
    #[arg(long)]
    output: PathBuf,
    /// Number of consecutive segments requested from a single stream.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..=MAX_SEGMENTS as i64))]
    segments: u16,
    /// Keep receiving until an empty finish file is created in the output directory.
    #[arg(long, requires = "keep_evidence_segments", conflicts_with = "segments")]
    until_finished: bool,
    /// Maximum new capture epochs after recoverable acquisition failures.
    #[arg(long, requires = "until_finished", value_parser = clap::value_parser!(u8).range(1..))]
    recoverable_restarts: Option<u8>,
    /// Maximum consecutive restarts after failed epochs that released fewer than 30 segments.
    #[arg(long, requires = "recoverable_restarts")]
    short_epoch_restarts: Option<u8>,
    /// Bound retained IQ and release surveyed IQ outside the full-decode selection.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=MAX_SPOOL_SEGMENTS))]
    spool_segments: Option<u8>,
    /// Number of completed IQ segments the receiver may queue for its writer.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=8))]
    writer_queue_depth: Option<u8>,
    /// Complex samples per receiver USB transfer, in multiples of 2,048.
    #[arg(long, value_parser = usb_buffer_samples)]
    usb_buffer_samples: Option<u32>,
    /// Retain this many completed segments of evidence. Requires a zero decode limit.
    #[arg(long, requires = "spool_segments", value_parser = clap::value_parser!(u8).range(1..=MAX_SPOOL_SEGMENTS))]
    keep_evidence_segments: Option<u8>,
    /// Tuning in integer MHz. Full decoding requires a compatible IEEE channel layout.
    #[arg(long, default_value_t = 2437, value_parser = clap::value_parser!(u32).range(70..=6000))]
    center_mhz: u32,
    /// Manual RX1 gain in dB.
    #[arg(long, default_value_t = 40.0, allow_negative_numbers = true)]
    gain: f64,
    /// Samples discarded once at stream start.
    #[arg(long, default_value_t = 2_000_000, value_parser = clap::value_parser!(u64).range(..=MAX_DISCARD_INITIAL_SAMPLES))]
    discard_initial_samples: u64,
    /// Number of segments to fully decode after acquisition. Zero skips full analysis.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(..=DEFAULT_KEEP_LIMIT as i64))]
    decode_limit: u8,
    /// Choose earliest segments or those with the most active survey windows.
    #[arg(long, value_enum, default_value = "earliest")]
    decode_selection: DecodeSelection,
    /// Decode Wi-Fi and BLE from every surveyed segment during acquisition.
    #[arg(long)]
    decode_wifi_ble: bool,
    /// Wi-Fi channel center for direct decoding, within 2 MHz of the capture center.
    #[arg(long, requires = "decode_wifi_ble", value_parser = clap::value_parser!(u32).range(2400..=2480))]
    wifi_channel_center_mhz: Option<u32>,
    /// Double Q11 components before direct Wi-Fi decoding, with a clipping count.
    #[arg(long, requires = "decode_wifi_ble")]
    wifi_double_q11: bool,
    /// Prebuilt offline Blue Dragon executable for rolling BLE replay.
    #[arg(long, requires = "decode_wifi_ble")]
    blue_dragon: Option<PathBuf>,
    /// Offer one low-priority selected-bin Coded replay per N IQ release opportunities.
    #[arg(long, requires_all = ["spool_segments", "coded_sentinel_decoder"], value_parser = clap::value_parser!(u32).range(1..))]
    coded_sentinel_every: Option<u32>,
    /// Prebuilt Blue Dragon with selected-frequency scan coverage reporting.
    #[arg(long, requires = "coded_sentinel_every")]
    coded_sentinel_decoder: Option<PathBuf>,
    /// Prebuilt Bluetooth replay wrapper for the optional Coded sentinel.
    #[arg(long, requires = "coded_sentinel_every")]
    coded_sentinel_wrapper: Option<PathBuf>,
    /// Keep decoded packets while omitting ordinary burst IQ from selected Coded replay.
    #[arg(long, requires = "coded_sentinel_every")]
    coded_sentinel_omit_squelch_iq: bool,
    /// Decode nominal hard IEEE 802.15.4 on channels 16–18 from every surveyed segment.
    #[arg(long)]
    decode_ieee802154: bool,
    /// Also decode IEEE 802.15.4 channel 19 at a 2437 MHz center.
    #[arg(long, requires = "decode_ieee802154")]
    decode_ieee802154_edge: bool,
    /// Use the contiguous predecessor tail for IEEE 802.15.4 decoding.
    #[arg(long, requires = "decode_ieee802154")]
    decode_ieee802154_boundaries: bool,
    /// Executor threads per nominal IEEE 802.15.4 channel worker.
    #[arg(long, requires = "decode_ieee802154", value_parser = clap::value_parser!(u8).range(1..=8))]
    ieee_executor_threads: Option<u8>,
    /// Retain up to this many full-band IQ clips per segment from survey activity.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=4))]
    retain_event_clips: Option<u8>,
    /// Choose activity, shape-based or contrast-qualified survey clips.
    #[arg(long, value_enum, requires = "retain_event_clips")]
    event_clip_selection: Option<ClipSelection>,
    /// Search eligible retained narrow clips for an unframed symbol clock.
    #[arg(long, requires = "retain_event_clips")]
    probe_event_clips: bool,
    /// Probe a band across each full segment, using integer frequency edges in hertz.
    #[arg(long, value_name = "LOWER:UPPER")]
    burst_band_hz: Option<Band>,
    /// Record simultaneous above-threshold windows in up to seven disjoint context bands.
    #[arg(long, value_name = "LOWER:UPPER", requires = "burst_band_hz")]
    burst_context_band_hz: Vec<Band>,
    /// Reserve one clip slot for a complete longer burst in the selected band.
    #[arg(long, requires_all = ["burst_band_hz", "retain_event_clips"])]
    retain_band_burst_clip: bool,
    /// Reserve a clip for a strong, complete two- or three-window band burst.
    #[arg(long, requires_all = ["burst_band_hz", "retain_event_clips"])]
    retain_short_band_burst_clip: bool,
    /// Measure short band-burst recurrence near 1 ms using only earlier segments.
    #[arg(long, requires_all = ["burst_band_hz", "decode_wifi_ble"])]
    track_short_burst_cadence: bool,
    /// Prefer survey events outside observed FCS-valid Wi-Fi frame spans.
    #[arg(long, requires_all = ["decode_wifi_ble", "retain_event_clips"])]
    wifi_frame_context: bool,
    /// Reserve a clip for a Wi-Fi HT header outside validated frame spans.
    #[arg(long, requires = "wifi_frame_context")]
    retain_ht_signal_clip: bool,
    /// Track per-mode HT headers and exact FCS-validated frame matches.
    #[arg(long, requires = "decode_wifi_ble")]
    track_wifi_ht_modes: bool,
    /// Count validated HE20 single-user headers and reserve an IQ clip when it fits.
    #[arg(long, requires = "wifi_frame_context")]
    retain_he_su_signal_clip: bool,
    /// Pinned HE receiver for background payload replay of the first archived clip.
    #[arg(long, requires = "retain_he_su_signal_clip")]
    wifi_he_decoder: Option<PathBuf>,
    /// Retain adjacent IQ context for Wi-Fi/BLE decoding across segment boundaries.
    #[arg(long, requires = "decode_wifi_ble")]
    decode_boundaries: bool,
    /// Maximum simultaneous segment workers for surveys and enabled decoders.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=MAX_SEGMENT_WORKERS))]
    survey_workers: u8,
    /// Prebuilt receiver with the capture-segments action.
    #[arg(long, default_value = "target/release/bladerf-lab")]
    receiver: PathBuf,
    #[command(flatten)]
    programs: AnalyzerPrograms,
}

fn usb_buffer_samples(value: &str) -> Result<u32, String> {
    let samples: u32 = value
        .parse()
        .map_err(|_| format!("invalid USB buffer size {value}"))?;
    if (USB_BUFFER_SAMPLE_ALIGNMENT..=MAX_USB_BUFFER_SAMPLES).contains(&samples)
        && samples.is_multiple_of(USB_BUFFER_SAMPLE_ALIGNMENT)
    {
        Ok(samples)
    } else {
        Err(format!(
            "USB buffer size must be a multiple of {USB_BUFFER_SAMPLE_ALIGNMENT} from {USB_BUFFER_SAMPLE_ALIGNMENT} through {MAX_USB_BUFFER_SAMPLES} samples"
        ))
    }
}

struct Record {
    segment: Arc<ValidatedSegment>,
    intake_wall_seconds: f64,
    survey_wall_seconds: Option<f64>,
    detection: Option<Detection>,
    decode: Option<DecodeReport>,
    direct_decode: Option<DirectReport>,
    ieee_nominal: Option<NominalReport>,
    event_clips: Option<ClipReport>,
    band_bursts: Option<BurstReport>,
    cadence_input: Option<cadence::Observation>,
    short_burst_cadence: Option<cadence::Score>,
    overlap_tail: Option<TailProof>,
    iq_release: Option<ReleaseProof>,
}

impl Record {
    fn activity_score(&self) -> Option<(usize, u64)> {
        self.detection
            .as_ref()
            .map(|detection| (self.segment.receipt.index, detection.active_window_count))
    }

    fn wifi_activity_windows(&self) -> Option<WifiActivityWindows> {
        self.detection.as_ref()?.wifi_activity_windows
    }

    fn wifi_gain_clipped_components(&self) -> Option<u64> {
        self.direct_decode.as_ref()?.wifi_gain_clipped_components()
    }

    fn band_burst_counts(&self) -> Option<BurstCounts> {
        self.band_bursts.as_ref().map(BurstReport::counts)
    }

    fn band_burst_clip_count(&self) -> Option<u64> {
        self.event_clips.as_ref()?.band_burst_clip_count()
    }

    fn short_band_burst_clip_count(&self) -> Option<u64> {
        self.event_clips.as_ref()?.short_band_burst_clip_count()
    }

    fn wifi_ht_signal_clip_count(&self) -> Option<u64> {
        self.event_clips.as_ref()?.wifi_ht_signal_clip_count()
    }

    fn wifi_ht_modes(&self) -> Option<&wifi_ht::WifiHtModes> {
        self.direct_decode.as_ref()?.wifi_ht_modes()
    }

    fn wifi_he_su_header_count(&self) -> Option<u64> {
        self.direct_decode.as_ref()?.wifi_he_su_header_count()
    }

    fn wifi_he_su_signal_clip_count(&self) -> Option<u64> {
        self.event_clips.as_ref()?.wifi_he_su_signal_clip_count()
    }
}

fn rank_activity(scores: impl Iterator<Item = (usize, u64)>) -> Vec<usize> {
    let mut ranked: Vec<_> = scores
        .map(|(index, count)| (Reverse(count), index))
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, index)| index).collect()
}

fn lower_replay_priority() -> Result<()> {
    // The worker's Linux scheduling priority is inherited by its child process.
    if rustix::process::getpriority_process(None)? < BACKGROUND_REPLAY_NICE {
        rustix::process::setpriority_process(None, BACKGROUND_REPLAY_NICE)
            .context("lower rolling replay scheduling priority")?;
    }
    Ok(())
}

#[derive(Default)]
struct Progress {
    records: VecDeque<Record>,
    retired: RetiredPrefix,
    cadence: Option<cadence::Tracker>,
    coded_sentinel: Option<coded_sentinel::Sentinel>,
    first_he_su_clip_sha256: Option<String>,
    he_payload: Option<he_payload::Worker>,
    peak_ready_segments_observed: usize,
    peak_in_flight_workers: usize,
    surveys_started_before_capture_supervisor_finished: usize,
    direct_decodes_completed_before_capture_finished: usize,
}

impl Progress {
    fn score_ready_cadence(&mut self) -> Result<()> {
        let Some(tracker) = &mut self.cadence else {
            return Ok(());
        };
        // Workers may finish out of capture order. Stop at the first gap.
        loop {
            let index = tracker
                .next_index()
                .checked_sub(self.retired.retired_segments)
                .context("short burst cadence cursor precedes the retired prefix")?;
            let Some(record) = self.records.get_mut(index) else {
                break;
            };
            let Some(input) = record.cadence_input.take() else {
                break;
            };
            record.short_burst_cadence = Some(tracker.score(input));
        }
        Ok(())
    }

    fn admitted(&self) -> usize {
        self.retired.retired_segments + self.records.len()
    }

    fn record_mut(&mut self, index: usize) -> &mut Record {
        &mut self.records[index - self.retired.retired_segments]
    }
}

#[derive(Serialize)]
struct SegmentReport<'a> {
    index: usize,
    first_sample_ns: i64,
    end_sample_exclusive_ns: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_on_request: Option<bool>,
    complex_samples: u64,
    iq_sha256: &'a str,
    sidecar_sha256: &'a str,
    receipt_sha256: &'a str,
    intake_wall_seconds: f64,
    survey_wall_seconds: Option<f64>,
    detection: Option<&'a Detection>,
    decode: Option<&'a DecodeReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_decode: Option<&'a DirectReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_nominal: Option<&'a NominalReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_clips: Option<&'a ClipReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_bursts: Option<&'a BurstReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_burst_cadence: Option<&'a cadence::Score>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iq_release: Option<&'a ReleaseProof>,
}

#[derive(Serialize)]
struct RunReport<'a> {
    schema_version: u8,
    operation: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    gain_db: f64,
    discarded_initial_samples: u64,
    segments_requested: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_on_request: Option<bool>,
    samples_per_segment: u64,
    decode_limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    decode_selection: Option<DecodeSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decode_selected_segment_indices: Option<&'a [usize]>,
    survey_workers: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    spool_segments: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_queue_depth: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usb_buffer_samples: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_evidence_segments: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retired_prefix: Option<&'a RetiredPrefix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    released_iq_segments: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    coded_sentinel: Option<&'a coded_sentinel::Summary>,
    published_segments: Option<usize>,
    verified_segments: usize,
    surveyed_segments: usize,
    decoded_segments: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_decoded_segments: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_channel_center_frequency_hz: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_double_q11: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_gain_clipped_components: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_nominal_decoded_segments: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_nominal_nyquist_edge: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_boundary_prefix_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_executor_threads: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_event_clips: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_event_clip_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_probes: Option<ProbeCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_clip_selection: Option<ClipSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_frame_context: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_ht_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_ht_modes: Option<wifi_ht::WifiHtModes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_he_su_headers: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_he_su_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_he_su_clip_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    he_su_payload: Option<&'a he_payload::Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_activity_windows: Option<WifiActivityWindows>,
    #[serde(skip_serializing_if = "Option::is_none")]
    burst_band_hz: Option<Band>,
    #[serde(skip_serializing_if = "Option::is_none")]
    burst_context_band_hz: Option<&'a [Band]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_bursts: Option<BurstCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retain_band_burst_clip: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retain_short_band_burst_clip: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track_short_burst_cadence: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_burst_cadence: Option<cadence::Counts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_decodes_completed_before_capture_finished: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decode_boundary_context_samples: Option<u64>,
    peak_ready_segments_observed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_in_flight_surveys: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_in_flight_segment_workers: Option<usize>,
    surveys_started_before_capture_supervisor_finished: usize,
    elapsed_wall_seconds: f64,
    overall_state: OverallState,
    error: Option<String>,
    receiver: &'a ProgramProvenance,
    detector: &'a ProgramProvenance,
    capture: Option<&'a CommandReport>,
    capture_fault: Option<&'static str>,
    segments: Vec<SegmentReport<'a>>,
}

fn capture_arguments(args: &Arguments, directory: &Path) -> Vec<OsString> {
    let mut arguments = vec![
        "capture-segments".into(),
        "--frequency".into(),
        (u64::from(args.center_mhz) * 1_000_000).to_string().into(),
        "--gain".into(),
        args.gain.to_string().into(),
    ];
    if args.until_finished {
        arguments.push("--until-finished".into());
    } else {
        arguments.extend(["--segments".into(), args.segments.to_string().into()]);
    }
    arguments.extend([
        "--discard-initial-samples".into(),
        args.discard_initial_samples.to_string().into(),
        "--output".into(),
        directory.as_os_str().to_owned(),
    ]);
    if let Some(slots) = args.spool_segments {
        arguments.extend(["--spool-segments".into(), slots.to_string().into()]);
    }
    if let Some(depth) = args.writer_queue_depth {
        arguments.extend(["--writer-queue-depth".into(), depth.to_string().into()]);
    }
    if let Some(samples) = args.usb_buffer_samples {
        arguments.extend(["--usb-buffer-samples".into(), samples.to_string().into()]);
    }
    if args.keep_evidence_segments.is_some() {
        arguments.push("--retire-prefix".into());
    }
    arguments
}

fn stream_error_is_receive_timeout(stderr: &str) -> bool {
    const LIBUSB_SOURCE: &str = "[ERROR @ host/libraries/libbladeRF/src/backend/usb/libusb.c:";
    const LIBUSB_TIMEOUT: &str = "] Transfer timed out for RX buffer 0x";
    const SOAPY_RESULT_PREFIX: &str = "[ERROR] bladerf_sync_rx() returned ";
    const SOAPY_IO_ERROR: &str = "[ERROR] bladerf_sync_rx() returned -5 - File/Device I/O error";
    const TERMINAL_ERROR: &str = "Error: read timestamped RX1 IQ: StreamError:";

    if !stderr.ends_with('\n') {
        return false;
    }
    let lines = stderr
        .lines()
        .map(|line| line.trim_start_matches('\r').trim_end());
    let mut terminal_errors = lines.clone().enumerate().filter(|(_, line)| {
        let line = line.trim_start();
        line.starts_with("Error:") || line.starts_with("Caused by:")
    });
    let Some((terminal_index, TERMINAL_ERROR)) = terminal_errors.next() else {
        return false;
    };
    if terminal_errors.next().is_some() {
        return false;
    }

    // The RX timeout can be followed by transfer errors during shutdown.
    // Require the observed Soapy I/O result between it and the terminal error.
    let mut sync_results = lines
        .clone()
        .enumerate()
        .filter(|(_, line)| line.contains(SOAPY_RESULT_PREFIX));
    let Some((sync_index, sync_result)) = sync_results.next() else {
        return false;
    };
    let sync_result = sync_result
        .strip_prefix("\x1b[1m\x1b[31m")
        .and_then(|line| line.strip_suffix("\x1b[0m"))
        .unwrap_or(sync_result);
    sync_result == SOAPY_IO_ERROR
        && sync_index < terminal_index
        && sync_results.next().is_none()
        && lines
            .take(sync_index)
            .filter_map(|line| line.strip_prefix(LIBUSB_SOURCE)?.split_once(LIBUSB_TIMEOUT))
            .any(|(location, address)| {
                !location.is_empty()
                    && location.bytes().all(|byte| byte.is_ascii_digit())
                    && address.len() <= usize::BITS as usize / 4
                    && address.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && usize::from_str_radix(address, 16).is_ok_and(|pointer| pointer != 0)
            })
}

fn capture_fault(output: &Path, capture: &CommandReport, continuous: bool) -> Option<&'static str> {
    if capture.success {
        return None;
    }
    if capture.timed_out {
        return Some(if continuous {
            "capture_idle"
        } else {
            "deadline"
        });
    }
    if capture.interrupted {
        return Some("interrupted");
    }
    let stderr = read_complete_capture_log(output, &capture.stderr, "acquisition.stderr.log")
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&stderr);
    Some(if text.contains("read timestamped RX1 IQ: Overflow:") {
        "receive_overflow"
    } else if text.contains("RX timestamp discontinuity") {
        "timestamp_discontinuity"
    } else if text.contains("segment writer backlog exhausted") {
        "writer_backlog"
    } else if text.contains("segment IQ spool is full") {
        "raw_spool_limit"
    } else if text
        .lines()
        .filter_map(|line| line.trim_end().strip_prefix("Error: "))
        .eq(["read timestamped RX1 IQ: Timeout:"])
        || stream_error_is_receive_timeout(&text)
    {
        "receive_timeout"
    } else {
        "unsuccessful_exit"
    })
}

/// Check the reaped producer's published directories for a complete prefix.
fn published_segments(
    root: &Path,
    requested: Option<usize>,
    retired: usize,
    entry_limit: usize,
) -> Result<usize> {
    let ordinals = publication_ordinals(root, entry_limit)?;
    let published = retired
        .checked_add(ordinals.len())
        .context("rolling published segment count overflow")?;
    ensure!(
        requested.is_none_or(|limit| published <= limit)
            && ordinals.iter().copied().eq((retired + 1)..=published),
        "published rolling segments do not continue the verified prefix"
    );
    Ok(published)
}

fn published_ordinal(name: &OsStr) -> Result<usize> {
    let name = name
        .to_str()
        .context("rolling capture entry name must be UTF-8")?;
    let ordinal = name
        .parse::<usize>()
        .context("invalid rolling segment name")?;
    ensure!(
        ordinal != 0 && name == format!("{ordinal:06}"),
        "invalid rolling segment name"
    );
    Ok(ordinal)
}

/// List atomically published segments even while the retired prefix advances.
fn publication_ordinals(root: &Path, entry_limit: usize) -> Result<BTreeSet<usize>> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => ensure!(
            metadata.is_dir(),
            "rolling capture directory must be direct"
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).context("inspect rolling capture directory"),
    }
    let mut ordinals = BTreeSet::new();
    for (index, entry) in fs::read_dir(root)?.enumerate() {
        ensure!(
            index <= entry_limit,
            "rolling capture tree exceeds its entry bound"
        );
        let entry = entry?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b".segment-") {
            continue;
        }
        let ordinal = published_ordinal(&name)?;
        match entry.file_type() {
            Ok(kind) => {
                ensure!(kind.is_dir(), "rolling segment must be a direct directory");
                ordinals.insert(ordinal);
            }
            // Retirement can remove this entry while reading the directory.
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error).context("inspect rolling segment directory"),
        }
    }
    Ok(ordinals)
}

struct Observer<'a> {
    capture_dir: &'a Path,
    reports_dir: &'a Path,
    detector: &'a ProgramProvenance,
    settings: SegmentSettings,
    requested: Option<usize>,
    spool: bool,
    keep_evidence: Option<usize>,
    record_limit: Option<usize>,
    entry_limit: usize,
    decode_selection: DecodeSelection,
    decode_limit: usize,
    direct: Option<&'a DirectPlan>,
    ieee_nominal: Option<&'a NominalPlan>,
    event_clip_limit: Option<u8>,
    event_clip_selection: ClipSelection,
    probe_event_clips: bool,
    burst_band_hz: Option<Band>,
    burst_context_band_hz: &'a [Band],
    retain_band_burst_clip: bool,
    retain_short_band_burst_clip: bool,
    track_short_burst_cadence: bool,
    wifi_frame_context: bool,
    decode_boundaries: bool,
    decode_ieee802154_boundaries: bool,
}

/// Signal scoped workers even if their coordinator unwinds.
struct CancelWorkers<'a>(&'a AtomicBool);

impl Drop for CancelWorkers<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn overlap_next<T: Send>(
    intake: impl FnOnce(&AtomicBool) -> Result<T> + Send,
    survey: impl FnOnce() -> Result<()>,
) -> Result<T> {
    let cancelled = AtomicBool::new(false);
    thread::scope(|scope| {
        let worker = thread::Builder::new()
            .name("rolling-rx1-intake".into())
            .spawn_scoped(scope, || intake(&cancelled));
        let _cancel_before_scope_cleanup = CancelWorkers(&cancelled);
        let surveyed = survey();
        if surveyed.is_err() {
            cancelled.store(true, Ordering::Relaxed);
        }
        let next = worker
            .context("start rolling segment intake")
            .and_then(|worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("rolling segment intake panicked"))?
            });
        surveyed?;
        next
    })
}

struct SegmentOutcome {
    survey: (Detection, f64),
    direct: Result<Option<DirectReport>>,
    ieee_nominal: Result<Option<NominalReport>>,
    event_clips: Result<Option<ClipReport>>,
    band_bursts: Result<Option<BurstReport>>,
    cadence_input: Option<cadence::Observation>,
}

struct WorkerOutcome {
    started_during_capture: bool,
    direct_completed_during_capture: bool,
    result: Result<SegmentOutcome>,
}

type SegmentWorker<'a> = (usize, thread::ScopedJoinHandle<'a, WorkerOutcome>);

fn finish_segment(
    observer: &Observer<'_>,
    progress: &mut Progress,
    (index, worker): SegmentWorker<'_>,
    release_stop: Option<&AtomicBool>,
) -> Result<()> {
    let outcome = worker
        .join()
        .map_err(|_| anyhow::anyhow!("rolling worker for segment {} panicked", index + 1))?;
    progress.surveys_started_before_capture_supervisor_finished +=
        usize::from(outcome.started_during_capture);
    progress.direct_decodes_completed_before_capture_finished +=
        usize::from(outcome.direct_completed_during_capture);
    observer.complete_segment(progress, index, outcome.result?, release_stop)
}

impl Observer<'_> {
    fn intake(&self, index: usize, previous_end: Option<i64>) -> Result<Option<Record>> {
        let started = Instant::now();
        let segment = read_segment(self.capture_dir, self.settings, index, previous_end)
            .with_context(|| format!("validate segment {}", index + 1))?;
        ensure!(
            self.requested.is_none()
                || segment
                    .as_ref()
                    .is_none_or(|segment| !segment.receipt.finished_on_request),
            "bounded capture returned an unrequested finish receipt"
        );
        Ok(segment.map(|segment| Record {
            segment: Arc::new(segment),
            intake_wall_seconds: started.elapsed().as_secs_f64(),
            survey_wall_seconds: None,
            detection: None,
            decode: None,
            direct_decode: None,
            ieee_nominal: None,
            event_clips: None,
            band_bursts: None,
            cadence_input: None,
            short_burst_cadence: None,
            overlap_tail: None,
            iq_release: None,
        }))
    }

    fn wait_segment(
        &self,
        index: usize,
        previous_end: Option<i64>,
        finished: &impl Fn() -> bool,
        stop: &AtomicBool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Option<Record>> {
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "rolling watch interrupted");
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
                return Ok(None);
            }
            let capture_finished = finished();
            // Check the directory after observing termination so the last
            // atomic publication cannot race past the final poll.
            if let Some(record) = self.intake(index, previous_end)? {
                return Ok(Some(record));
            }
            if capture_finished {
                return Ok(None);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn sample_ready_backlog(&self, index: usize, progress: &mut Progress) {
        // This is a sampled backlog statistic. Intake and terminal
        // reconciliation perform the authoritative directory checks.
        let ready = (index..)
            .take_while(|index| {
                self.requested.is_none_or(|limit| *index < limit)
                    && self.capture_dir.join(format!("{:06}", index + 1)).is_dir()
            })
            .count();
        progress.peak_ready_segments_observed = progress.peak_ready_segments_observed.max(ready);
    }

    fn is_final(&self, segment: &ValidatedSegment) -> bool {
        segment.receipt.finished_on_request || self.requested == Some(segment.receipt.index + 1)
    }

    fn survey_segment(
        &self,
        segment: &ValidatedSegment,
        stop: &AtomicBool,
        on_start: impl FnOnce(),
    ) -> Result<(Detection, f64)> {
        let index = segment.receipt.index;
        let output = self.reports_dir.join(format!("{:06}", index + 1));
        segment.verify_unchanged()?;
        on_start();
        let started = Instant::now();
        let detection = detection::detect(
            self.detector,
            &segment.source.input,
            &output,
            self.settings.center_frequency_hz,
            segment.source.complex_samples,
            &segment.source.sha256,
            stop,
        )
        .with_context(|| format!("detect activity in segment {}", index + 1))?;
        segment.verify_unchanged()?;
        Ok((detection, started.elapsed().as_secs_f64()))
    }

    fn process_segment(
        &self,
        segment: &ValidatedSegment,
        prefix: Option<&Tail>,
        stop: &AtomicBool,
        on_survey_start: impl FnOnce(),
    ) -> Result<SegmentOutcome> {
        let mut survey = self.survey_segment(segment, stop, on_survey_start)?;
        let ordinal = segment.receipt.index + 1;
        let report_dir = self.reports_dir.join(format!("{ordinal:06}"));
        let staged = OnceLock::new();
        let staged_input = || {
            staged
                .get_or_init(|| {
                    prefix
                        .map(|tail| overlap::stage(tail, segment, &report_dir, stop))
                        .transpose()
                })
                .as_ref()
                .map(Option::as_ref)
                .map_err(|error| anyhow::anyhow!("{error:#}"))
        };
        let verify_decode_input = |staged: Option<&overlap::StagedInput>| {
            if let Some(staged) = staged {
                staged.verify()?;
            }
            segment.verify_unchanged()
        };
        let run_direct = || {
            self.direct
                .map(|plan| {
                    let staged = if self.decode_boundaries {
                        staged_input()?
                    } else {
                        None
                    };
                    let start = segment.receipt.start_sample;
                    let end = start + segment.source.complex_samples;
                    let boundary = self.decode_boundaries.then(|| BoundaryDecode {
                        staged_source: staged.map(|staged| &staged.source),
                        segment_start_sample: start,
                        owned_start_sample: if ordinal == 1 {
                            start
                        } else {
                            start - CONTEXT_SAMPLES
                        },
                        owned_end_sample_exclusive: if self.is_final(segment) {
                            end
                        } else {
                            end - CONTEXT_SAMPLES
                        },
                        prefix: prefix.map(Tail::proof),
                    });
                    plan.run(
                        &segment.source,
                        boundary,
                        &report_dir.join("direct"),
                        stop,
                        || verify_decode_input(staged),
                    )
                    .with_context(|| format!("direct Wi-Fi/BLE decode of segment {ordinal}"))
                })
                .transpose()
        };
        let run_ieee = || {
            self.ieee_nominal
                .map(|plan| {
                    let output = report_dir.join("ieee802154");
                    let result = if self.decode_ieee802154_boundaries {
                        let staged = staged_input()?;
                        let prefix = prefix.map(Tail::proof);
                        let segment_start_sample = segment.receipt.start_sample;
                        let ownership = NominalOwnership {
                            input_start_sample: prefix
                                .map_or(segment_start_sample, |proof| proof.start_sample),
                            segment_start_sample,
                            segment_source: &segment.source,
                            prefix_receipt_sha256: prefix.map(TailProof::receipt_sha256),
                        };
                        plan.run_with_ownership(
                            staged.map_or(&segment.source, |staged| &staged.source),
                            ownership,
                            &output,
                            stop,
                            || verify_decode_input(staged),
                        )
                    } else {
                        plan.run(&segment.source, &output, stop, || {
                            segment.verify_unchanged()
                        })
                    };
                    result.with_context(|| {
                        format!("nominal IEEE 802.15.4 decode of segment {ordinal}")
                    })
                })
                .transpose()
        };
        let run_decoders = || {
            if self.direct.is_some() && self.ieee_nominal.is_some() {
                thread::scope(|scope| {
                    let ieee = thread::Builder::new()
                        .name("rolling-ieee-nominal".into())
                        .spawn_scoped(scope, run_ieee)
                        .context("start rolling IEEE 802.15.4 supervisor");
                    let direct = run_direct();
                    let ieee = ieee.and_then(|worker| {
                        worker.join().map_err(|_| {
                            anyhow::anyhow!("rolling IEEE 802.15.4 supervisor panicked")
                        })?
                    });
                    (direct, ieee)
                })
            } else {
                (run_direct(), run_ieee())
            }
        };
        let run_band_probe = |band| {
            burst::probe(
                self.detector,
                band,
                self.burst_context_band_hz,
                segment,
                &report_dir,
                stop,
            )
            .with_context(|| format!("probe full-segment band bursts in segment {ordinal}"))
        };
        let ((mut direct, mut ieee_nominal), mut band_bursts) = match self.burst_band_hz {
            Some(band) if self.direct.is_some() || self.ieee_nominal.is_some() => {
                thread::scope(|scope| {
                    // The probe reads the admitted IQ independently. Packet
                    // context is needed only after both workloads have joined.
                    let worker = thread::Builder::new()
                        .name("rolling-band-probe".into())
                        .spawn_scoped(scope, || run_band_probe(band))
                        .context("start rolling band-burst probe supervisor");
                    let decodes = run_decoders();
                    let bursts = worker.and_then(|worker| {
                        worker.join().map_err(|_| {
                            anyhow::anyhow!("rolling band-burst probe supervisor panicked")
                        })?
                    });
                    (decodes, bursts.map(Some))
                })
            }
            band => (run_decoders(), band.map(run_band_probe).transpose()),
        };
        if let Some(Ok(Some(staged))) = staged.into_inner()
            && let Err(error) = staged.close()
        {
            if self.decode_boundaries && direct.is_ok() {
                direct = Err(error);
            } else if self.decode_ieee802154_boundaries && ieee_nominal.is_ok() {
                ieee_nominal = Err(error);
            }
        }
        let direct_report = direct.as_ref().ok().and_then(Option::as_ref);
        let wifi_spans =
            direct_report.and_then(|report| report.wifi_frame_spans(segment.receipt.start_sample));
        let clip_wifi_spans = wifi_spans.as_deref().filter(|_| self.wifi_frame_context);
        survey
            .0
            .detect_wifi_activity(segment.receipt.start_sample, clip_wifi_spans);
        if let Ok(Some(report)) = &mut band_bursts {
            report.record_wifi_spans(clip_wifi_spans);
        }
        let cadence_input = band_bursts
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .zip(wifi_spans.as_deref())
            .filter(|_| self.track_short_burst_cadence)
            .map(|(report, spans)| {
                cadence::Observation::new(
                    segment.receipt.start_sample,
                    segment.source.complex_samples,
                    report.detections(),
                    spans,
                )
            });
        let event_clips = self
            .event_clip_limit
            .filter(|_| direct.is_ok() && ieee_nominal.is_ok() && band_bursts.is_ok())
            .map(|limit| {
                let ht_signal = direct_report
                    .and_then(|report| report.wifi_ht_preambles(segment.receipt.start_sample));
                let he_su_signal = direct_report
                    .and_then(|report| report.wifi_he_su_preambles(segment.receipt.start_sample));
                clips::retain(
                    segment,
                    &survey.0,
                    direct_report,
                    RetainOptions {
                        limit,
                        selection: self.event_clip_selection,
                        probe_program: self.probe_event_clips.then_some(self.detector),
                        wifi_spans: clip_wifi_spans,
                        ht_signal: ht_signal.as_ref().map(|(starts, sha256)| HtSignalContext {
                            preamble_starts: starts,
                            report_sha256: sha256,
                        }),
                        he_su_signal: he_su_signal.as_ref().map(|(starts, sha256)| {
                            HeSuSignalContext {
                                preamble_starts: starts,
                                report_sha256: sha256,
                            }
                        }),
                        band_bursts: band_bursts.as_ref().ok().and_then(Option::as_ref).filter(
                            |_| self.retain_band_burst_clip || self.retain_short_band_burst_clip,
                        ),
                        retain_long_band_burst: self.retain_band_burst_clip,
                        retain_short_band_burst: self.retain_short_band_burst_clip,
                    },
                    &report_dir,
                    stop,
                )
                .with_context(|| format!("retain survey IQ clips from segment {ordinal}"))
            })
            .transpose();
        if let Ok(Some(report)) = &mut band_bursts {
            report.discard_detections();
        }
        survey.0.events = Vec::new();
        if let Some(search) = &mut survey.0.narrow_search {
            search.candidates = Vec::new();
        }
        Ok(SegmentOutcome {
            survey,
            direct,
            ieee_nominal,
            event_clips,
            band_bursts,
            cadence_input,
        })
    }

    fn prepare_record(
        &self,
        record: &mut Record,
        previous_tail: &mut Option<Tail>,
    ) -> Result<Option<Tail>> {
        let index = record.segment.receipt.index;
        let output = self.reports_dir.join(format!("{:06}", index + 1));
        report::private_directory(&output)?;
        if !self.decode_boundaries && !self.decode_ieee802154_boundaries {
            return Ok(None);
        }
        let prefix = previous_tail.take();
        ensure!(
            prefix.is_some() == (index != 0),
            "boundary decode is missing the preceding segment tail"
        );
        if !self.is_final(&record.segment) {
            let tail = overlap::read_tail(&record.segment, &output)
                .with_context(|| format!("save overlap tail for segment {}", index + 1))?;
            record.overlap_tail = Some(tail.proof().clone());
            *previous_tail = Some(tail);
        }
        Ok(prefix)
    }

    fn complete_segment(
        &self,
        progress: &mut Progress,
        index: usize,
        outcome: SegmentOutcome,
        release_stop: Option<&AtomicBool>,
    ) -> Result<()> {
        let base = progress.retired.retired_segments;
        let records = &mut progress.records;
        let record = &mut records[index - base];
        let (detection, seconds) = outcome.survey;
        record.survey_wall_seconds = Some(seconds);
        record.detection = Some(detection);
        outcome
            .direct
            .map(|report| record.direct_decode = report)
            .and(
                outcome
                    .ieee_nominal
                    .map(|report| record.ieee_nominal = report),
            )
            .and(
                outcome
                    .event_clips
                    .map(|report| record.event_clips = report),
            )
            .and(
                outcome
                    .band_bursts
                    .map(|report| record.band_bursts = report),
            )?;
        ensure!(
            !self.track_short_burst_cadence || outcome.cadence_input.is_some(),
            "short burst cadence requires verified burst and Wi-Fi span evidence"
        );
        record.cadence_input = outcome.cadence_input;
        if let Some(worker) = &mut progress.he_payload {
            worker.poll();
        }
        if progress.first_he_su_clip_sha256.is_none()
            && let Some(stop) = release_stop
            && let Some(clips) = &record.event_clips
            && let Some(files) = clips.he_su_clip_files()
        {
            let output = self
                .reports_dir
                .parent()
                .context("rolling output has no epoch root")?;
            let direct = record
                .direct_decode
                .as_ref()
                .context("HE clip has no verified direct replay")?;
            let digest = he_clip::publish(output, &record.segment, clips, direct, files, stop)?;
            if let Some(worker) = &mut progress.he_payload {
                worker.start(&digest);
            }
            progress.first_he_su_clip_sha256 = Some(digest);
        }
        if let Some(stop) = release_stop
            && self.spool
            && let Some(discard) = self.discard_after_survey(records, index)
        {
            if let Some(sentinel) = &mut progress.coded_sentinel {
                let segment = &records[discard - base].segment;
                sentinel.offer(segment, stop, || {
                    let admitted = base + records.len();
                    let newest = publication_ordinals(self.capture_dir, self.entry_limit)?
                        .last()
                        .copied()
                        .unwrap_or(0);
                    let unadmitted = newest.saturating_sub(admitted);
                    let unreleased = records
                        .iter()
                        .filter(|record| record.iq_release.is_none())
                        .count();
                    Ok(unadmitted + unreleased)
                });
            }
            let record = &mut records[discard - base];
            let detection = record
                .detection
                .as_ref()
                .context("release requires a survey")?;
            let ordinal = discard + 1;
            let output = self.reports_dir.join(format!("{ordinal:06}"));
            let direct = record.direct_decode.as_ref();
            ensure!(
                self.direct.is_none() || direct.is_some(),
                "release requires completed direct decoding"
            );
            let ieee_nominal = record.ieee_nominal.as_ref();
            ensure!(
                self.ieee_nominal.is_none() || ieee_nominal.is_some(),
                "release requires completed nominal IEEE 802.15.4 decoding"
            );
            ensure!(
                self.event_clip_limit.is_none() || record.event_clips.is_some(),
                "release requires a verified survey clip report"
            );
            ensure!(
                self.burst_band_hz.is_none() || record.band_bursts.is_some(),
                "release requires a verified band-burst report"
            );
            if let Some(released) = retention::release_iq(
                &record.segment,
                detection,
                ReleaseEvidence {
                    direct,
                    ieee_nominal,
                    overlap_tail: record.overlap_tail.as_ref(),
                    event_clips: record.event_clips.as_ref(),
                    band_bursts: record.band_bursts.as_ref(),
                },
                &output,
                stop,
            )
            .with_context(|| format!("release IQ for segment {ordinal}"))?
            {
                record.iq_release = Some(released.proof);
                released
                    .confirmation
                    .with_context(|| format!("confirm IQ release for segment {ordinal}"))?;
            }
            if let Some(sentinel) = &mut progress.coded_sentinel {
                sentinel.publish_pending();
            }
        }
        if let Some(stop) = release_stop {
            if !stop.load(Ordering::Relaxed) {
                progress.score_ready_cadence()?;
            }
            self.retire_evidence(progress, stop)?;
        }
        Ok(())
    }

    fn verify_record(&self, record: &Record) -> Result<()> {
        if self.burst_band_hz.is_some() {
            let burst = record
                .band_bursts
                .as_ref()
                .context("rolling segment is missing its band-burst report")?;
            if record.iq_release.is_none() {
                burst.verify()?;
            }
        }
        if self.event_clip_limit.is_some() {
            record
                .event_clips
                .as_ref()
                .context("rolling segment is missing its survey clip report")?
                .verify()?;
        }
        if self.ieee_nominal.is_some() {
            record
                .ieee_nominal
                .as_ref()
                .context("rolling segment is missing its nominal IEEE 802.15.4 decode")?
                .verify()?;
        }
        if let Some(tail) = &record.overlap_tail {
            let ordinal = record.segment.receipt.index + 1;
            tail.verify(&self.reports_dir.join(format!("{ordinal:06}")))?;
        }
        if self.direct.is_some() {
            record
                .direct_decode
                .as_ref()
                .context("rolling segment is missing its direct decode")?
                .verify()?;
        }
        match &record.iq_release {
            Some(proof) => proof.verify_after_release(&record.segment),
            None => record.segment.verify_unchanged(),
        }
    }

    fn retire_evidence(&self, progress: &mut Progress, stop: &AtomicBool) -> Result<()> {
        let Some(keep) = self.keep_evidence else {
            return Ok(());
        };
        // A completed successor no longer reads the oldest record's overlap
        // tail. Keep at least one settled record even when acquisition stops.
        while !stop.load(Ordering::Relaxed)
            && progress.records.len() > keep
            && progress
                .records
                .iter()
                .take(keep + 1)
                .all(|record| record.iq_release.is_some())
        {
            let record = &progress.records[0];
            self.verify_record(record)?;
            let previous = progress.retired.retired_segments;
            let pruned = progress
                .retired
                .retire(record, self.capture_dir, self.reports_dir);
            if progress.retired.retired_segments > previous {
                progress.records.pop_front();
            }
            pruned?;
        }
        Ok(())
    }

    fn discard_after_survey(&self, records: &VecDeque<Record>, index: usize) -> Option<usize> {
        match self.decode_selection {
            DecodeSelection::Earliest => (index >= self.decode_limit).then_some(index),
            DecodeSelection::Activity => {
                // A completed survey adds one contender to the retained winners.
                let retained = rank_activity(
                    records
                        .iter()
                        .filter(|record| record.iq_release.is_none())
                        .filter_map(Record::activity_score),
                );
                retained.get(self.decode_limit).copied()
            }
        }
    }

    fn collect(
        &self,
        finished: impl Fn() -> bool + Sync,
        progress: &mut Progress,
        stop: &AtomicBool,
    ) -> Result<()> {
        let mut current = self.wait_segment(0, None, &finished, stop, None)?;
        let mut previous_tail = None;
        while let Some(record) = current {
            let index = record.segment.receipt.index;
            let next_index = index + 1;
            let segment = Arc::clone(&record.segment);
            let end = segment.receipt.end_sample_exclusive_ns;
            progress.records.push_back(record);
            let prefix = self.prepare_record(progress.record_mut(index), &mut previous_tail)?;
            let mut process = || {
                self.sample_ready_backlog(index, progress);
                let outcome = self.process_segment(&segment, prefix.as_ref(), stop, || {
                    progress.peak_in_flight_workers = 1;
                    progress.surveys_started_before_capture_supervisor_finished +=
                        usize::from(!finished());
                })?;
                if outcome.direct.as_ref().is_ok_and(Option::is_some) && !finished() {
                    progress.direct_decodes_completed_before_capture_finished += 1;
                }
                self.complete_segment(progress, index, outcome, Some(stop))
            };
            let next = if !self.is_final(&segment) {
                overlap_next(
                    |cancelled| {
                        self.wait_segment(next_index, Some(end), &finished, stop, Some(cancelled))
                    },
                    process,
                )
            } else {
                process().map(|()| None)
            };
            current = next?;
        }
        Ok(())
    }

    fn collect_parallel(
        &self,
        worker_limit: usize,
        finished: impl Fn() -> bool + Sync,
        progress: &mut Progress,
        stop: &AtomicBool,
        capture_cancelled: &mut bool,
    ) -> Result<()> {
        let cancelled = AtomicBool::new(false);
        thread::scope(|scope| {
            let _cancel_before_scope_cleanup = CancelWorkers(&cancelled);
            let mut workers: Vec<SegmentWorker<'_>> = Vec::with_capacity(worker_limit);
            let mut intake_worker: Option<thread::ScopedJoinHandle<'_, Result<Option<Record>>>> =
                None;
            let scheduled = (|| -> Result<()> {
                let mut pending: Option<Record> = None;
                let mut previous_tail = None;
                let mut previous_end = None;
                let mut no_more_segments = false;
                loop {
                    ensure!(!stop.load(Ordering::Relaxed), "rolling watch interrupted");
                    while let Some(slot) =
                        workers.iter().position(|(_, worker)| worker.is_finished())
                    {
                        finish_segment(self, progress, workers.remove(slot), Some(stop))?;
                    }
                    if let Some(worker) = intake_worker.take_if(|worker| worker.is_finished()) {
                        pending = worker
                            .join()
                            .map_err(|_| anyhow::anyhow!("rolling segment intake panicked"))??;
                        no_more_segments = pending.is_none();
                    }
                    let index = progress.admitted();
                    let window_available = self
                        .record_limit
                        .is_none_or(|limit| progress.records.len() < limit);
                    if window_available
                        && workers.len() < worker_limit
                        && let Some(record) = pending.take()
                    {
                        no_more_segments = record.segment.receipt.finished_on_request;
                        previous_end = Some(record.segment.receipt.end_sample_exclusive_ns);
                        self.sample_ready_backlog(index, progress);
                        let segment = Arc::clone(&record.segment);
                        progress.records.push_back(record);
                        let prefix =
                            self.prepare_record(progress.record_mut(index), &mut previous_tail)?;
                        let finished = &finished;
                        let cancelled = &cancelled;
                        let worker = thread::Builder::new()
                            .name("rolling-rx1-segment".into())
                            .spawn_scoped(scope, move || {
                                let mut started_during_capture = false;
                                let result = self.process_segment(
                                    &segment,
                                    prefix.as_ref(),
                                    cancelled,
                                    || {
                                        started_during_capture = !finished();
                                    },
                                );
                                let direct_completed_during_capture =
                                    result.as_ref().is_ok_and(|outcome| {
                                        outcome.direct.as_ref().is_ok_and(Option::is_some)
                                    }) && !finished();
                                WorkerOutcome {
                                    started_during_capture,
                                    direct_completed_during_capture,
                                    result,
                                }
                            })
                            .with_context(|| format!("start worker for segment {}", index + 1))?;
                        workers.push((index, worker));
                        progress.peak_in_flight_workers =
                            progress.peak_in_flight_workers.max(workers.len());
                        continue;
                    }
                    if window_available
                        && self.requested.is_none_or(|limit| index < limit)
                        && !no_more_segments
                        && pending.is_none()
                        && intake_worker.is_none()
                    {
                        // A full IQ read must not delay reaping completed segment workers.
                        let finished = &finished;
                        let cancelled = &cancelled;
                        intake_worker = Some(
                            thread::Builder::new()
                                .name("rolling-rx1-intake".into())
                                .spawn_scoped(scope, move || {
                                    self.wait_segment(
                                        index,
                                        previous_end,
                                        finished,
                                        stop,
                                        Some(cancelled),
                                    )
                                })
                                .with_context(|| {
                                    format!("start intake for segment {}", index + 1)
                                })?,
                        );
                    }
                    if (Some(index) == self.requested || no_more_segments) && workers.is_empty() {
                        return Ok(());
                    }
                    ensure!(
                        window_available || !workers.is_empty(),
                        "rolling evidence window cannot advance without a segment worker"
                    );
                    thread::sleep(POLL_INTERVAL);
                }
            })();
            if scheduled.is_err() {
                // Stop the radio before waiting out worker shutdown.
                // Preserve an already-present user interruption separately.
                *capture_cancelled = !stop.swap(true, Ordering::Relaxed);
                cancelled.store(true, Ordering::Relaxed);
            }
            // Join every started worker and retain its checked result before returning any error.
            let joined = workers
                .into_iter()
                .map(|worker| finish_segment(self, progress, worker, None))
                .fold(Ok(()), Result::and);
            let intake_joined = intake_worker.map_or(Ok(()), |worker| {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("rolling segment intake panicked"))?
                    .map(|_| ())
            });
            scheduled.and(joined).and(intake_joined)
        })
    }
}

impl<'a> From<&'a Record> for SegmentReport<'a> {
    fn from(record: &'a Record) -> Self {
        SegmentReport {
            index: record.segment.receipt.index,
            first_sample_ns: record.segment.receipt.first_sample_ns,
            end_sample_exclusive_ns: record.segment.receipt.end_sample_exclusive_ns,
            finished_on_request: record.segment.receipt.finished_on_request.then_some(true),
            complex_samples: record.segment.source.complex_samples,
            iq_sha256: &record.segment.source.sha256,
            sidecar_sha256: &record.segment.source.sidecar_sha256,
            receipt_sha256: &record.segment.receipt_sha256,
            intake_wall_seconds: record.intake_wall_seconds,
            survey_wall_seconds: record.survey_wall_seconds,
            detection: record.detection.as_ref(),
            decode: record.decode.as_ref(),
            direct_decode: record.direct_decode.as_ref(),
            ieee_nominal: record.ieee_nominal.as_ref(),
            event_clips: record.event_clips.as_ref(),
            band_bursts: record.band_bursts.as_ref(),
            short_burst_cadence: record.short_burst_cadence.as_ref(),
            iq_release: record.iq_release.as_ref(),
        }
    }
}

pub(crate) fn run(args: Arguments, stop: &AtomicBool) -> Result<ExitCode> {
    if let Some(restarts) = args.recoverable_restarts {
        return recovery::run(&args, restarts, stop);
    }
    let mut coded_release_offset = 0;
    let end = run_epoch(&args, &args.output, None, stop, &mut coded_release_offset)?;
    Ok(if end.overall_state == OverallState::Complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run_epoch(
    args: &Arguments,
    output_path: &Path,
    session_finish: Option<&Path>,
    stop: &AtomicBool,
    coded_release_offset: &mut usize,
) -> Result<recovery::EpochEnd> {
    let requested = (!args.until_finished).then_some(usize::from(args.segments));
    let decode_limit = usize::from(args.decode_limit);
    ensure!(
        requested.is_none_or(|count| decode_limit <= count),
        "decode-limit exceeds the segment count"
    );
    ensure!(args.gain.is_finite(), "gain must be finite");
    ensure!(
        requested.is_some_and(|count| count <= DEFAULT_KEEP_LIMIT) || args.spool_segments.is_some(),
        "long rolling runs require --spool-segments"
    );
    ensure!(
        requested.is_some_and(|count| count <= MAX_SPOOL_SEGMENTS as usize)
            || args.keep_evidence_segments.is_some(),
        "continuous or extended rolling runs require --keep-evidence-segments"
    );
    ensure!(
        args.keep_evidence_segments.is_none()
            || (args.spool_segments.is_some() && decode_limit == 0),
        "evidence retention requires --spool-segments and --decode-limit 0"
    );
    if let Some(slots) = args.spool_segments {
        let slots = usize::from(slots);
        ensure!(
            requested.is_some_and(|count| count <= slots) || decode_limit < slots,
            "IQ spool must leave room for a segment beyond the decode budget"
        );
    }
    // A continuous tree can contain the recent window, admitted work and raw
    // segments waiting for intake. One private staging name is allowed separately.
    let entry_limit = requested.unwrap_or_else(|| {
        usize::from(args.keep_evidence_segments.unwrap_or(0))
            + 2 * usize::from(args.spool_segments.unwrap_or(0))
    });
    let settings = SegmentSettings {
        center_frequency_hz: u64::from(args.center_mhz) * 1_000_000,
        exact_samples: SEGMENT_SAMPLES,
        discarded_initial_samples: args.discard_initial_samples,
        gain_db: args.gain,
    };
    if let Some(band) = args.burst_band_hz {
        band.verify_context(&args.burst_context_band_hz, settings.center_frequency_hz)?;
    }
    let wifi_tuning = args
        .wifi_channel_center_mhz
        .map(|center| WifiTuning::new(settings.center_frequency_hz, center))
        .transpose()?
        .flatten();
    let receiver = process::resolve_program("bladerf-lab", &args.receiver)?;
    let detector = process::resolve_program("rf-observatory", &args.programs.rf_observatory)?;
    let decode = (decode_limit != 0)
        .then(|| decode::prepare(args.programs.paths(), args.center_mhz))
        .transpose()?;
    let direct = args
        .decode_wifi_ble
        .then(|| {
            ensure!(
                (2400..=2480).contains(&args.center_mhz),
                "direct Wi-Fi/BLE decode requires a center between 2400 and 2480 MHz"
            );
            direct::prepare(
                &args.programs.wifi_monitor,
                &args.programs.bluetooth_monitor,
                args.blue_dragon.as_deref(),
                WifiReplayOptions {
                    frame_spans: args.wifi_frame_context
                        || args.track_short_burst_cadence
                        || args.track_wifi_ht_modes,
                    ht_signal_events: args.retain_ht_signal_clip || args.track_wifi_ht_modes,
                    he_su_signal_events: args.retain_he_su_signal_clip,
                    tuning: wifi_tuning,
                    double_q11: args.wifi_double_q11,
                },
                args.track_wifi_ht_modes,
            )
        })
        .transpose()?;
    let ieee_nominal = args
        .decode_ieee802154
        .then(|| {
            ieee_nominal::prepare(
                &args.programs.ieee802154_monitor,
                &args.programs.ieee_worker,
                args.center_mhz,
                args.decode_ieee802154_edge,
                args.ieee_executor_threads,
            )
        })
        .transpose()?;
    let output = report::fresh_output(output_path)?;
    let coded_sentinel = args
        .coded_sentinel_every
        .map(|every| {
            let decoder = args
                .coded_sentinel_decoder
                .as_deref()
                .context("Coded sentinel requires its decoder executable")?;
            coded_sentinel::Sentinel::prepare(
                &output,
                coded_sentinel::Options {
                    every: every as usize,
                    spool_slots: usize::from(
                        args.spool_segments
                            .context("Coded sentinel requires a spool")?,
                    ),
                    omit_squelch_iq: args.coded_sentinel_omit_squelch_iq,
                },
                args.center_mhz,
                args.coded_sentinel_wrapper
                    .as_deref()
                    .unwrap_or(&args.programs.bluetooth_monitor),
                decoder,
                *coded_release_offset,
            )
        })
        .transpose()?;
    let capture_dir = output.join("capture");
    let reports_dir = output.join("segments");
    let log_dir = output.join("logs");
    report::private_directory(&reports_dir)?;
    report::private_directory(&log_dir)?;
    let arguments = capture_arguments(args, &capture_dir);
    let observer = Observer {
        capture_dir: &capture_dir,
        reports_dir: &reports_dir,
        detector: &detector,
        settings,
        requested,
        spool: args.spool_segments.is_some(),
        keep_evidence: args.keep_evidence_segments.map(usize::from),
        record_limit: args
            .keep_evidence_segments
            .zip(args.spool_segments)
            .map(|(keep, spool)| usize::from(keep) + usize::from(spool)),
        entry_limit,
        decode_selection: args.decode_selection,
        decode_limit,
        direct: direct.as_ref(),
        ieee_nominal: ieee_nominal.as_ref(),
        event_clip_limit: args.retain_event_clips,
        event_clip_selection: args.event_clip_selection.unwrap_or(ClipSelection::Activity),
        probe_event_clips: args.probe_event_clips,
        burst_band_hz: args.burst_band_hz,
        burst_context_band_hz: &args.burst_context_band_hz,
        retain_band_burst_clip: args.retain_band_burst_clip,
        retain_short_band_burst_clip: args.retain_short_band_burst_clip,
        track_short_burst_cadence: args.track_short_burst_cadence,
        wifi_frame_context: args.wifi_frame_context,
        decode_boundaries: args.decode_boundaries,
        decode_ieee802154_boundaries: args.decode_ieee802154_boundaries,
    };
    let started = Instant::now();
    let mut progress = Progress {
        cadence: args
            .track_short_burst_cadence
            .then(cadence::Tracker::default),
        coded_sentinel,
        he_payload: args
            .wifi_he_decoder
            .as_ref()
            .map(|decoder| {
                he_payload::Worker::prepare(&output, &args.programs.wifi_monitor, decoder)
            })
            .transpose()?,
        ..Progress::default()
    };
    let (observation, capture_result, observed_interrupt) = thread::scope(|scope| {
        let capture = thread::Builder::new()
            .name("rolling-rx1-capture".into())
            .spawn_scoped(scope, || {
                ensure!(
                    !stop.load(Ordering::Relaxed),
                    "interrupted before rolling capture start"
                );
                if let Some(count) = requested {
                    let timeout = Duration::from_secs(
                        BASE_CAPTURE_TIMEOUT_SECONDS
                            + count.saturating_sub(DEFAULT_KEEP_LIMIT) as u64,
                    );
                    return process::run(
                        &receiver,
                        &arguments,
                        &output,
                        "acquisition",
                        LOG_LIMIT_BYTES,
                        timeout,
                        stop,
                    );
                }
                if let Some(marker) = session_finish {
                    recovery::relay_finish(marker, &output)?;
                }
                let mut newest = 0;
                let mut next_poll = Instant::now();
                process::run_with_idle_timeout(
                    &receiver,
                    &arguments,
                    &output,
                    "acquisition",
                    LOG_LIMIT_BYTES,
                    CAPTURE_IDLE_TIMEOUT,
                    stop,
                    || {
                        let now = Instant::now();
                        if now >= next_poll {
                            if let Some(marker) = session_finish {
                                recovery::relay_finish(marker, &output)?;
                            }
                            newest = newest.max(
                                publication_ordinals(&capture_dir, entry_limit)?
                                    .last()
                                    .copied()
                                    .unwrap_or(0),
                            );
                            next_poll = now + PUBLICATION_POLL_INTERVAL;
                        }
                        Ok(newest)
                    },
                )
            });
        let Ok(capture) = capture else {
            return (
                Ok(()),
                Err(capture.unwrap_err().into()),
                stop.load(Ordering::Relaxed),
            );
        };
        let finished = || capture.is_finished();
        let mut capture_cancelled = false;
        let observation = if args.survey_workers > 1 {
            observer.collect_parallel(
                usize::from(args.survey_workers),
                finished,
                &mut progress,
                stop,
                &mut capture_cancelled,
            )
        } else {
            observer.collect(finished, &mut progress, stop)
        };
        let interrupted = stop.load(Ordering::Relaxed) && !capture_cancelled;
        if observation.is_err() {
            // A detector or intake failure stops acquisition. Save the
            // primary failure and interruption status before signalling.
            stop.store(true, Ordering::Relaxed);
        }
        let acquisition = capture
            .join()
            .map_err(|_| anyhow::anyhow!("rolling capture supervisor panicked"))
            .and_then(|result| result);
        (observation, acquisition, interrupted)
    });
    if let Some(sentinel) = &mut progress.coded_sentinel {
        sentinel.finish(
            args.until_finished
                || observed_interrupt
                || observation.is_err()
                || !capture_result.as_ref().is_ok_and(|report| report.success),
        );
        sentinel.publish_pending();
        *coded_release_offset = sentinel.next_release_offset();
    }
    if let Some(worker) = &mut progress.he_payload {
        worker.finish(
            observed_interrupt
                || observation.is_err()
                || !capture_result.as_ref().is_ok_and(|report| report.success),
        );
    }
    let mut capture = None;
    let mut fault = None;
    let mut restartable = false;
    let mut published = None;
    let mut decode_indices = None;
    let observation_cancelled = observation.is_err();
    let finished_on_request = progress
        .records
        .back()
        .is_some_and(|record| record.segment.receipt.finished_on_request);

    let mut completion = (|| -> Result<()> {
        let tree = published_segments(
            &capture_dir,
            requested,
            progress.retired.retired_segments,
            entry_limit,
        );
        published = tree.as_ref().ok().copied();
        let capture_status = match capture_result {
            Ok(report) => {
                let status = (|| {
                    ensure!(
                        !args.until_finished || !report.timed_out,
                        "continuous capture exceeded its publication idle deadline"
                    );
                    process::require_success(&report)?;
                    ensure!(
                        !report.stdout.truncated && !report.stderr.truncated,
                        "rolling capture exceeded its log bound"
                    );
                    Ok(())
                })();
                capture = Some(report);
                status
            }
            Err(error) => Err(error).context("supervise rolling capture"),
        };
        fault = capture
            .as_ref()
            .and_then(|report| capture_fault(&output, report, args.until_finished));
        observation?;
        let capture_error = capture_status.err();
        let check_recovery = session_finish.is_some()
            && capture_error.is_some()
            && !finished_on_request
            && matches!(
                fault,
                Some(
                    "receive_overflow"
                        | "receive_timeout"
                        | "timestamp_discontinuity"
                        | "writer_backlog"
                        | "raw_spool_limit"
                )
            )
            && capture.as_ref().is_some_and(|child| {
                child.exit_code.is_some_and(|code| code != 0)
                    && child.exit_signal.is_none()
                    && !child.timed_out
                    && !child.interrupted
                    && !child.stdout.truncated
                    && !child.stderr.truncated
            });
        if !check_recovery && let Some(error) = capture_error {
            return Err(error);
        }
        let published = tree?;
        let surveyed = published == progress.admitted()
            && progress
                .records
                .iter()
                .all(|record| record.detection.is_some());
        if let Some(count) = requested {
            ensure!(
                published == count && surveyed,
                "rolling capture ended before all requested segments were surveyed"
            );
        } else if check_recovery {
            ensure!(
                published > 0
                    && surveyed
                    && progress
                        .records
                        .iter()
                        .all(|record| record.iq_release.is_some()),
                "recoverable capture exit needs a nonempty surveyed and released prefix"
            );
            for entry in fs::read_dir(&capture_dir)? {
                ensure!(
                    !entry?
                        .file_name()
                        .as_encoded_bytes()
                        .starts_with(b".segment-"),
                    "recoverable capture exit left a staged segment"
                );
            }
        } else {
            ensure!(
                finished_on_request && surveyed,
                "continuous capture needs a witnessed finish on its last published segment"
            );
        }
        ensure!(!stop.load(Ordering::Relaxed), "rolling watch interrupted");
        if let Some(keep) = args.keep_evidence_segments {
            ensure!(
                progress.records.len() == published.min(usize::from(keep)),
                "rolling evidence suffix differs from the requested retention"
            );
            progress.retired.verify(&output)?;
            ensure!(
                published_segments(
                    &reports_dir,
                    requested,
                    progress.retired.retired_segments,
                    entry_limit,
                )? == published,
                "rolling report directories differ from the verified suffix"
            );
        }
        for record in &progress.records {
            observer.verify_record(record)?;
        }
        ensure!(
            process::resolve_program(receiver.name, &receiver.path)? == receiver
                && process::resolve_program(detector.name, &detector.path)? == detector,
            "rolling receiver or detector executable changed"
        );
        if let Some(error) = capture_error {
            // Allow a fresh epoch only after the failed epoch's complete
            // published prefix and released evidence have been verified.
            restartable = true;
            return Err(error);
        }
        let selected: Vec<_> = match args.decode_selection {
            DecodeSelection::Earliest => (0..decode_limit).collect(),
            DecodeSelection::Activity => {
                rank_activity(progress.records.iter().filter_map(Record::activity_score))
                    .into_iter()
                    .take(decode_limit)
                    .collect()
            }
        };
        if args.spool_segments.is_some() {
            ensure!(
                progress
                    .records
                    .iter()
                    .all(|record| record.iq_release.is_none()
                        == selected.contains(&record.segment.receipt.index)),
                "retained IQ does not match the decode selection"
            );
        }
        decode_indices = Some(selected);
        Ok(())
    })();

    if let (Some(indices), Some(plan)) = (&decode_indices, decode) {
        completion = (|| {
            for &index in indices {
                ensure!(!stop.load(Ordering::Relaxed), "rolling watch interrupted");
                let record = progress.record_mut(index);
                record.segment.verify_unchanged()?;
                let ordinal = record.segment.receipt.index + 1;
                let output = reports_dir.join(format!("{ordinal:06}")).join("decode");
                let decoded = plan
                    .run(&record.segment.source, &output, stop)
                    .with_context(|| format!("decode segment {ordinal}"))?;
                let complete = decoded.state == OverallState::Complete;
                record.segment.verify_unchanged()?;
                record.decode = Some(decoded);
                ensure!(
                    complete,
                    "full decode of segment {ordinal} did not complete"
                );
            }
            Ok(())
        })();
    }

    let interrupted =
        observed_interrupt || (!observation_cancelled && stop.load(Ordering::Relaxed));
    let overall_state = if interrupted {
        OverallState::Interrupted
    } else if completion.is_ok() {
        OverallState::Complete
    } else {
        OverallState::Failed
    };
    let activity = args.decode_selection == DecodeSelection::Activity;
    let processing = args.decode_wifi_ble
        || args.decode_ieee802154
        || args.retain_event_clips.is_some()
        || args.burst_band_hz.is_some();
    let clip_reports = || {
        progress
            .records
            .iter()
            .filter_map(|record| record.event_clips.as_ref())
    };
    let result = RunReport {
        schema_version: if args.until_finished {
            3
        } else if args.keep_evidence_segments.is_some() {
            2
        } else {
            1
        },
        operation: "rolling_rx1_detector",
        center_frequency_hz: settings.center_frequency_hz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        gain_db: settings.gain_db,
        discarded_initial_samples: settings.discarded_initial_samples,
        segments_requested: requested,
        finished_on_request: args.until_finished.then_some(finished_on_request),
        samples_per_segment: SEGMENT_SAMPLES,
        decode_limit,
        decode_selection: activity.then_some(args.decode_selection),
        decode_selected_segment_indices: decode_indices.as_deref().filter(|_| activity),
        survey_workers: args.survey_workers,
        spool_segments: args.spool_segments,
        writer_queue_depth: args.writer_queue_depth,
        usb_buffer_samples: args.usb_buffer_samples,
        keep_evidence_segments: args.keep_evidence_segments,
        retired_prefix: args.keep_evidence_segments.map(|_| &progress.retired),
        coded_sentinel: progress
            .coded_sentinel
            .as_ref()
            .map(coded_sentinel::Sentinel::summary),
        released_iq_segments: args.spool_segments.map(|_| {
            progress.retired.retired_segments
                + progress
                    .records
                    .iter()
                    .filter(|record| record.iq_release.is_some())
                    .count()
        }),
        published_segments: published,
        verified_segments: progress.admitted(),
        surveyed_segments: progress.retired.retired_segments
            + progress
                .records
                .iter()
                .filter(|record| record.detection.is_some())
                .count(),
        decoded_segments: progress
            .records
            .iter()
            .filter(|record| {
                record
                    .decode
                    .as_ref()
                    .is_some_and(|decode| decode.state == OverallState::Complete)
            })
            .count(),
        direct_decoded_segments: args.decode_wifi_ble.then(|| {
            progress.retired.direct_decoded_segments
                + progress
                    .records
                    .iter()
                    .filter(|record| record.direct_decode.is_some())
                    .count()
        }),
        wifi_channel_center_frequency_hz: wifi_tuning.map(WifiTuning::center_frequency_hz),
        wifi_double_q11: args.wifi_double_q11.then_some(true),
        wifi_gain_clipped_components: progress
            .retired
            .wifi_gain_clipped_components
            .into_iter()
            .chain(
                progress
                    .records
                    .iter()
                    .filter_map(Record::wifi_gain_clipped_components),
            )
            .reduce(|total, count| total + count),
        ieee_nominal_decoded_segments: args.decode_ieee802154.then(|| {
            progress.retired.ieee_nominal_decoded_segments
                + progress
                    .records
                    .iter()
                    .filter(|record| record.ieee_nominal.is_some())
                    .count()
        }),
        ieee_nominal_nyquist_edge: args.decode_ieee802154_edge.then_some(true),
        ieee_boundary_prefix_samples: args.decode_ieee802154_boundaries.then_some(PREFIX_SAMPLES),
        ieee_executor_threads: args.ieee_executor_threads,
        retained_event_clips: args
            .retain_event_clips
            .map(|_| clip_reports().map(ClipReport::count).sum()),
        retained_event_clip_bytes: args
            .retain_event_clips
            .map(|_| clip_reports().map(ClipReport::bytes).sum()),
        waveform_probes: args
            .probe_event_clips
            .then(|| ProbeCounts::total(clip_reports())),
        event_clip_selection: args.event_clip_selection,
        wifi_frame_context: args.wifi_frame_context.then_some(true),
        wifi_ht_signal_clips: args.retain_ht_signal_clip.then(|| {
            progress.retired.wifi_ht_signal_clips.unwrap_or(0)
                + progress
                    .records
                    .iter()
                    .filter_map(Record::wifi_ht_signal_clip_count)
                    .sum::<u64>()
        }),
        wifi_ht_modes: wifi_ht::WifiHtModes::total(
            progress
                .retired
                .wifi_ht_modes
                .iter()
                .chain(progress.records.iter().filter_map(Record::wifi_ht_modes)),
        )?
        .or_else(|| args.track_wifi_ht_modes.then(wifi_ht::WifiHtModes::default)),
        wifi_he_su_headers: args.retain_he_su_signal_clip.then(|| {
            progress.retired.wifi_he_su_headers.unwrap_or(0)
                + progress
                    .records
                    .iter()
                    .filter_map(Record::wifi_he_su_header_count)
                    .sum::<u64>()
        }),
        wifi_he_su_signal_clips: args.retain_he_su_signal_clip.then(|| {
            progress.retired.wifi_he_su_signal_clips.unwrap_or(0)
                + progress
                    .records
                    .iter()
                    .filter_map(Record::wifi_he_su_signal_clip_count)
                    .sum::<u64>()
        }),
        first_he_su_clip_sha256: progress.first_he_su_clip_sha256.as_deref(),
        he_su_payload: progress.he_payload.as_ref().map(he_payload::Worker::status),
        wifi_activity_windows: WifiActivityWindows::total(
            progress.retired.wifi_activity_windows.into_iter().chain(
                progress
                    .records
                    .iter()
                    .filter_map(Record::wifi_activity_windows),
            ),
        ),
        burst_band_hz: args.burst_band_hz,
        burst_context_band_hz: (!args.burst_context_band_hz.is_empty())
            .then_some(args.burst_context_band_hz.as_slice()),
        band_bursts: BurstCounts::total(
            progress.retired.band_bursts.into_iter().chain(
                progress
                    .records
                    .iter()
                    .filter_map(Record::band_burst_counts),
            ),
        ),
        retain_band_burst_clip: args.retain_band_burst_clip.then_some(true),
        band_burst_clips: progress
            .retired
            .band_burst_clips
            .into_iter()
            .chain(
                progress
                    .records
                    .iter()
                    .filter_map(Record::band_burst_clip_count),
            )
            .reduce(|total, count| total + count),
        retain_short_band_burst_clip: args.retain_short_band_burst_clip.then_some(true),
        short_band_burst_clips: args.retain_short_band_burst_clip.then(|| {
            progress.retired.short_band_burst_clips.unwrap_or(0)
                + progress
                    .records
                    .iter()
                    .filter_map(Record::short_band_burst_clip_count)
                    .sum::<u64>()
        }),
        track_short_burst_cadence: args.track_short_burst_cadence.then_some(true),
        short_burst_cadence: cadence::Counts::total(
            progress.retired.short_burst_cadence.into_iter().chain(
                progress.records.iter().filter_map(|record| {
                    record
                        .short_burst_cadence
                        .as_ref()
                        .map(cadence::Score::counts)
                }),
            ),
        ),
        direct_decodes_completed_before_capture_finished: args
            .decode_wifi_ble
            .then_some(progress.direct_decodes_completed_before_capture_finished),
        decode_boundary_context_samples: args.decode_boundaries.then_some(CONTEXT_SAMPLES),
        peak_ready_segments_observed: progress.peak_ready_segments_observed,
        peak_in_flight_surveys: (!processing).then_some(progress.peak_in_flight_workers),
        peak_in_flight_segment_workers: processing.then_some(progress.peak_in_flight_workers),
        surveys_started_before_capture_supervisor_finished: progress
            .surveys_started_before_capture_supervisor_finished,
        elapsed_wall_seconds: started.elapsed().as_secs_f64(),
        overall_state,
        error: completion.err().map(|error| format!("{error:#}")),
        receiver: &receiver,
        detector: &detector,
        capture: capture.as_ref(),
        capture_fault: fault,
        segments: progress.records.iter().map(SegmentReport::from).collect(),
    };
    report::publish_json(&output, "run.json", &result)?;
    let status_output = crate::print_status(format_args!(
        "Terminal rolling watch report: {}",
        output.join("run.json").display()
    ));
    if let Some(error) = &result.error {
        crate::print_error(format_args!("Rolling watch failed: {error}"));
    }
    // Recoverable sessions record the epoch before writing their own terminal status.
    if session_finish.is_none() {
        status_output?;
    }
    Ok(recovery::EpochEnd {
        overall_state,
        capture_fault: result.capture_fault,
        published_segments: result.published_segments,
        verified_segments: result.verified_segments,
        released_iq_segments: result.released_iq_segments,
        restartable: restartable && overall_state == OverallState::Failed,
        extra_run_report_bytes: result
            .wifi_ht_modes
            .iter()
            .chain(
                result
                    .retired_prefix
                    .and_then(|prefix| prefix.wifi_ht_modes.as_ref()),
            )
            .map(wifi_ht::WifiHtModes::json_budget)
            .sum(),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    use clap::{Parser, error::ErrorKind as CliErrorKind};

    use super::*;
    use crate::{Action, Cli};

    #[test]
    fn usb_buffer_size_requires_an_exact_aligned_sample_count() {
        let base = ["digital-monitor", "watch-segments", "--output", "capture"];
        for (arguments, expected) in [
            (&[][..], None),
            (&["--usb-buffer-samples", "2048"][..], Some(2048)),
            (&["--usb-buffer-samples", "65536"][..], Some(65_536)),
        ] {
            let cli =
                Cli::try_parse_from(base.into_iter().chain(arguments.iter().copied())).unwrap();
            let Action::WatchSegments(args) = cli.command else {
                panic!("watch-segments command")
            };
            assert_eq!(args.usb_buffer_samples, expected);
        }
        for invalid in ["0", "1024", "3072", "32769", "67584"] {
            let error =
                Cli::try_parse_from(base.into_iter().chain(["--usb-buffer-samples", invalid]))
                    .err()
                    .unwrap();
            assert_eq!(error.kind(), CliErrorKind::ValueValidation);
        }
    }

    #[test]
    fn short_epoch_restart_limit_requires_recovery() {
        let base = ["digital-monitor", "watch-segments", "--output", "capture"];
        let continuous = [
            "--until-finished",
            "--spool-segments",
            "8",
            "--keep-evidence-segments",
            "2",
            "--short-epoch-restarts",
            "0",
        ];
        let error = Cli::try_parse_from(base.into_iter().chain(continuous))
            .err()
            .unwrap();
        assert_eq!(error.kind(), CliErrorKind::MissingRequiredArgument);
        let cli = Cli::try_parse_from(
            base.into_iter()
                .chain(continuous)
                .chain(["--recoverable-restarts", "12"]),
        )
        .unwrap();
        let Action::WatchSegments(args) = cli.command else {
            panic!("watch-segments command");
        };
        assert_eq!(args.short_epoch_restarts, Some(0));
    }

    #[test]
    fn he_su_clip_requires_verified_wifi_context() {
        let base = [
            "digital-monitor",
            "watch-segments",
            "--output",
            "capture",
            "--retain-he-su-signal-clip",
        ];
        let error = Cli::try_parse_from(base).err().unwrap();
        assert_eq!(error.kind(), CliErrorKind::MissingRequiredArgument);
        let cli = Cli::try_parse_from(base.into_iter().chain([
            "--decode-wifi-ble",
            "--retain-event-clips",
            "2",
            "--wifi-frame-context",
            "--retain-ht-signal-clip",
        ]))
        .unwrap();
        let Action::WatchSegments(args) = cli.command else {
            panic!("watch-segments command");
        };
        assert!(args.retain_he_su_signal_clip && args.retain_ht_signal_clip);
    }

    #[test]
    fn he_payload_decoder_requires_clip_retention() {
        let base = [
            "digital-monitor",
            "watch-segments",
            "--output",
            "capture",
            "--decode-wifi-ble",
            "--retain-event-clips",
            "2",
            "--wifi-frame-context",
            "--wifi-he-decoder",
            "radio_receive",
        ];
        let error = Cli::try_parse_from(base).err().unwrap();
        assert_eq!(error.kind(), CliErrorKind::MissingRequiredArgument);
        let cli =
            Cli::try_parse_from(base.into_iter().chain(["--retain-he-su-signal-clip"])).unwrap();
        let Action::WatchSegments(args) = cli.command else {
            panic!("watch-segments command");
        };
        assert_eq!(
            args.wifi_he_decoder.as_deref(),
            Some(Path::new("radio_receive"))
        );
    }

    #[test]
    fn ordinary_iq_omission_requires_a_coded_sentinel() {
        let base = [
            "digital-monitor",
            "watch-segments",
            "--output",
            "capture",
            "--coded-sentinel-omit-squelch-iq",
        ];
        let error = Cli::try_parse_from(base).err().unwrap();
        assert_eq!(error.kind(), CliErrorKind::MissingRequiredArgument);
        let cli = Cli::try_parse_from(base.into_iter().chain([
            "--spool-segments",
            "2",
            "--coded-sentinel-every",
            "1",
            "--coded-sentinel-decoder",
            "decoder",
        ]))
        .unwrap();
        let Action::WatchSegments(args) = cli.command else {
            panic!("watch-segments command");
        };
        assert!(args.coded_sentinel_omit_squelch_iq);
    }

    #[test]
    fn receive_timeout_requires_the_complete_native_terminal_error() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("logs"))?;
        let error = "Error: read timestamped RX1 IQ: Timeout: \n";
        let native =
            format!("[ERROR] Transfer timed out for RX buffer\n{error}[DEBUG] deinitializing\n");
        let stream_error = "Error: read timestamped RX1 IQ: StreamError: \n";
        let transfer_timeout = concat!(
            "[ERROR @ host/libraries/libbladeRF/src/backend/usb/libusb.c:1089] ",
            "Transfer timed out for RX buffer 0x7f00abcd\n"
        );
        let io_error = "[ERROR] bladerf_sync_rx() returned -5 - File/Device I/O error";
        let plain = format!("{transfer_timeout}{io_error}\n{stream_error}");
        let colored = format!(
            "{transfer_timeout}\r[ERROR @ host/libraries/libbladeRF/src/backend/usb/libusb.c:1076] Got transfer error for buffer 0x7f00dcba\n\r\x1b[1m\x1b[31m{io_error}\x1b[0m\n{stream_error}[DEBUG] deinitializing\n"
        );
        let mut capture = CommandReport {
            program: "bladerf-lab",
            arguments: Vec::new(),
            exit_code: Some(1),
            exit_signal: None,
            success: false,
            timed_out: false,
            interrupted: false,
            sigkill_cleanup_sent: true,
            stdout: process::LogReport {
                path: "logs/acquisition.stdout.log".into(),
                observed_bytes: 0,
                retained_bytes: 0,
                truncated: false,
            },
            stderr: process::LogReport {
                path: "logs/acquisition.stderr.log".into(),
                observed_bytes: 0,
                retained_bytes: 0,
                truncated: false,
            },
        };
        for (stderr, truncated, expected) in [
            (
                "[ERROR] Transfer timed out for RX buffer\n",
                false,
                "unsuccessful_exit",
            ),
            (
                "Error: open RX device: Timeout: \n",
                false,
                "unsuccessful_exit",
            ),
            (
                "[DEBUG] Error: read timestamped RX1 IQ: Timeout: \n",
                false,
                "unsuccessful_exit",
            ),
            (
                "Error: read timestamped RX1 IQ: Timeout: unrelated context\n",
                false,
                "unsuccessful_exit",
            ),
            (
                "Error: read timestamped RX1 IQ: Timeout:\nError: finalize segment\n",
                false,
                "unsuccessful_exit",
            ),
            (&native, true, "unsuccessful_exit"),
            (&native, false, "receive_timeout"),
            (&plain, false, "receive_timeout"),
            (
                &plain.replace("libusb.c:1089", "libusb.c:1097"),
                false,
                "receive_timeout",
            ),
            (&colored, false, "receive_timeout"),
            (&colored, true, "unsuccessful_exit"),
            (stream_error, false, "unsuccessful_exit"),
            (
                &format!("{transfer_timeout}{stream_error}"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{io_error}\n{stream_error}"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{io_error}\n{transfer_timeout}{stream_error}"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{transfer_timeout}{stream_error}{io_error}\n"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{transfer_timeout}{io_error}\n{io_error}\n{stream_error}"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{colored} Error: flush capture\n"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{plain}Caused by:\n    device was removed\n"),
                false,
                "unsuccessful_exit",
            ),
            (
                &format!("{plain}{stream_error}"),
                false,
                "unsuccessful_exit",
            ),
            (plain.trim_end(), false, "unsuccessful_exit"),
            (&colored.replace("\x1b[0m", ""), false, "unsuccessful_exit"),
            (
                &plain.replace("libusb.c:1089", "sync.c:1089"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("libusb.c:1089", "libusb.c:1089x"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("libusb.c:1089", "libusb.c:"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("for RX buffer", "for TX buffer"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("0x7f00abcd", "0x+7f00abcd"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("0x7f00abcd", "0x0"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("0x7f00abcd", "0x123456789abcdef012"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("returned -5", "returned -7"),
                false,
                "unsuccessful_exit",
            ),
            (
                &plain.replace("StreamError: ", "StreamError: error while closing"),
                false,
                "unsuccessful_exit",
            ),
        ] {
            fs::write(root.path().join(&capture.stderr.path), stderr)?;
            capture.stderr.retained_bytes = stderr.len() as u64;
            capture.stderr.observed_bytes = stderr.len() as u64 + u64::from(truncated);
            capture.stderr.truncated = truncated;
            assert_eq!(
                capture_fault(root.path(), &capture, true),
                Some(expected),
                "stderr {stderr:?}, truncated {truncated}"
            );
        }
        fs::write(root.path().join(&capture.stderr.path), colored.as_bytes())?;
        capture.stderr.retained_bytes = colored.len() as u64 + 1;
        capture.stderr.observed_bytes = capture.stderr.retained_bytes;
        assert_eq!(
            capture_fault(root.path(), &capture, true),
            Some("unsuccessful_exit")
        );
        capture.timed_out = true;
        assert_eq!(
            capture_fault(root.path(), &capture, true),
            Some("capture_idle")
        );
        assert_eq!(
            capture_fault(root.path(), &capture, false),
            Some("deadline")
        );
        Ok(())
    }

    #[test]
    fn publication_progress_ignores_staging_and_checks_the_ordinal_suffix() {
        let root = tempfile::tempdir().unwrap();
        for name in [".segment-staging", "999999", "1000000"] {
            fs::create_dir(root.path().join(name)).unwrap();
        }
        assert_eq!(
            publication_ordinals(root.path(), 3).unwrap(),
            BTreeSet::from([999_999, 1_000_000])
        );
        assert_eq!(
            published_segments(root.path(), None, 999_998, 3).unwrap(),
            1_000_000
        );
        assert!(published_segments(root.path(), None, 999_997, 3).is_err());
    }

    #[test]
    fn activity_order_prefers_more_active_windows_then_earlier_capture() {
        let completed = [(3, 9), (2, 12), (0, 0), (1, 12)];
        assert_eq!(rank_activity(completed.into_iter()), [1, 2, 3, 0]);
        assert_eq!(rank_activity(completed.into_iter().rev()), [1, 2, 3, 0]);
    }

    #[test]
    fn next_intake_and_current_survey_overlap_before_ordered_error() {
        let (intake_started, seen_intake) = mpsc::channel();
        let (survey_finished, seen_survey) = mpsc::channel();
        let timeout = Duration::from_secs(1);
        let result = overlap_next(
            move |_| -> Result<()> {
                intake_started.send(())?;
                seen_survey.recv_timeout(timeout)?;
                anyhow::bail!("bad next segment")
            },
            || {
                seen_intake.recv_timeout(timeout)?;
                survey_finished.send(())?;
                Ok(())
            },
        );
        assert_eq!(result.unwrap_err().to_string(), "bad next segment");
    }

    #[test]
    fn survey_failure_cancels_and_joins_prefetch_without_overwriting_its_error() {
        let joined = AtomicBool::new(false);
        let result = overlap_next(
            |cancelled| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !cancelled.load(Ordering::Relaxed) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(1));
                }
                ensure!(
                    cancelled.load(Ordering::Relaxed),
                    "intake was not cancelled"
                );
                joined.store(true, Ordering::Relaxed);
                anyhow::bail!("intake cancellation")
            },
            || anyhow::bail!("survey failed"),
        );
        assert_eq!(result.unwrap_err().to_string(), "survey failed");
        assert!(joined.load(Ordering::Relaxed));
    }

    #[test]
    fn successful_child_with_no_published_segments_is_a_terminal_failure() {
        for survey_workers in [1, 2] {
            check_no_published_segments(survey_workers);
        }
    }

    fn check_no_published_segments(survey_workers: u8) {
        let directory = tempfile::tempdir().unwrap();
        let receiver = directory.path().join("test-receiver");
        let detector = directory.path().join("test-detector");
        fs::write(&receiver, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&detector, b"#!/bin/sh\nexit 7\n").unwrap();
        for program in [&receiver, &detector] {
            fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let output = directory.path().join("run");
        let writer_queue_depth = (survey_workers == 2).then_some(4);
        let usb_buffer_samples = (survey_workers == 2).then_some(65_536);
        let args = Arguments {
            output: output.clone(),
            segments: 1,
            until_finished: false,
            recoverable_restarts: None,
            short_epoch_restarts: None,
            spool_segments: None,
            writer_queue_depth,
            usb_buffer_samples,
            keep_evidence_segments: None,
            center_mhz: 2437,
            gain: 40.0,
            discard_initial_samples: 0,
            decode_limit: 0,
            decode_selection: DecodeSelection::Earliest,
            decode_wifi_ble: false,
            wifi_channel_center_mhz: None,
            wifi_double_q11: false,
            blue_dragon: None,
            coded_sentinel_every: None,
            coded_sentinel_decoder: None,
            coded_sentinel_wrapper: None,
            coded_sentinel_omit_squelch_iq: false,
            decode_ieee802154: false,
            decode_ieee802154_edge: false,
            decode_ieee802154_boundaries: false,
            ieee_executor_threads: None,
            retain_event_clips: None,
            event_clip_selection: None,
            probe_event_clips: false,
            burst_band_hz: None,
            burst_context_band_hz: Vec::new(),
            retain_band_burst_clip: false,
            retain_short_band_burst_clip: false,
            track_short_burst_cadence: false,
            wifi_frame_context: false,
            retain_ht_signal_clip: false,
            track_wifi_ht_modes: false,
            retain_he_su_signal_clip: false,
            wifi_he_decoder: None,
            decode_boundaries: false,
            survey_workers,
            receiver,
            programs: AnalyzerPrograms {
                wifi_monitor: "unused".into(),
                rf_observatory: detector,
                ieee802154_monitor: "unused".into(),
                ieee_worker: "unused".into(),
                bluetooth_monitor: "unused".into(),
            },
        };
        let receiver_args = capture_arguments(&args, &output);
        let position = receiver_args
            .iter()
            .position(|arg| arg == "--writer-queue-depth");
        assert_eq!(
            position.map(|index| receiver_args[index + 1].to_str().unwrap()),
            writer_queue_depth.map(|_| "4"),
        );
        let position = receiver_args
            .iter()
            .position(|arg| arg == "--usb-buffer-samples");
        assert_eq!(
            position.map(|index| receiver_args[index + 1].to_str().unwrap()),
            usb_buffer_samples.map(|_| "65536"),
        );
        assert_eq!(
            run(args, &AtomicBool::new(false)).unwrap(),
            ExitCode::FAILURE
        );
        let result = report::read_json(&output.join("run.json")).unwrap();
        assert_eq!(result["overall_state"], "failed");
        assert_eq!(result["capture"]["success"], true);
        assert_eq!(result["published_segments"], 0);
        assert_eq!(result["surveyed_segments"], 0);
        assert_eq!(
            result
                .get("writer_queue_depth")
                .and_then(serde_json::Value::as_u64),
            writer_queue_depth.map(u64::from),
        );
        assert_eq!(
            result
                .get("usb_buffer_samples")
                .and_then(serde_json::Value::as_u64),
            usb_buffer_samples.map(u64::from),
        );
        assert!(result.get("decode_selection").is_none());
        assert!(result.get("decode_selected_segment_indices").is_none());
        assert!(result.get("direct_decoded_segments").is_none());
        assert!(result.get("wifi_channel_center_frequency_hz").is_none());
        assert!(result.get("ieee_nominal_decoded_segments").is_none());
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("before all requested segments")
        );
    }
}
