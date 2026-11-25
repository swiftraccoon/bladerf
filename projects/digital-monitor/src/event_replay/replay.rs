//! Direct bounded decoder invocations and terminal packet contracts.

use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::packets::{Packet, read_packets};
use super::plan::Slice;
use super::{Protocol, remaining};
use crate::{input::SourceReport, orchestrate, process, report};

mod wifi_signal;

use wifi_signal::{HeSuEvent, HtEvent, WifiSignalEvidence};

const CHILD_TIMEOUT: Duration = Duration::from_secs(30);
const SLOT_WAIT_INTERVAL: Duration = Duration::from_millis(20);
const LOG_BYTES: u64 = 1024 * 1024;
const JSON_BYTES: u64 = 16 * 1024 * 1024;
const WIFI_FRAME_SPAN_BYTES: u64 = 2 * 1024 * 1024;
const WIFI_TRANSLATION_FILTER: &str = "anti-wrap-kaiser-257";

pub(super) struct DecodeSlots {
    available: Mutex<u8>,
    released: Condvar,
}

impl DecodeSlots {
    pub(super) fn new(jobs: u8) -> Self {
        Self {
            available: Mutex::new(jobs),
            released: Condvar::new(),
        }
    }

    fn acquire(&self, deadline: Instant, stop: &AtomicBool) -> Result<DecodePermit<'_>> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            let budget = remaining(deadline, stop)?;
            if *available > 0 {
                *available -= 1;
                return Ok(DecodePermit(self));
            }
            (available, _) = self
                .released
                .wait_timeout(available, budget.min(SLOT_WAIT_INTERVAL))
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct DecodePermit<'a>(&'a DecodeSlots);

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        *self
            .0
            .available
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        self.0.released.notify_one();
    }
}

#[cfg(test)]
#[test]
fn decoder_slots_wait_within_the_deadline_and_reuse_released_capacity() {
    let stop = AtomicBool::new(false);
    let slots = DecodeSlots::new(2);
    let deadline = Instant::now() + Duration::from_secs(5);
    let first = slots.acquire(deadline, &stop).unwrap();
    let _second = slots.acquire(deadline, &stop).unwrap();
    let waiting = slots.acquire(Instant::now() + Duration::from_millis(5), &stop);
    assert!(
        waiting
            .err()
            .is_some_and(|error| error.to_string().contains("time budget"))
    );
    drop(first);
    let _replacement = slots.acquire(deadline, &stop).unwrap();
}

pub(super) struct Replay {
    pub(super) packets: Vec<Packet>,
    pub(super) decoder_sha256: Option<String>,
    files: ReplayFiles,
    wifi_frame_spans: Option<Vec<Range<u64>>>,
    wifi_ht_signal: Option<WifiSignalEvidence>,
    wifi_he_su_signal: Option<WifiSignalEvidence>,
    q11_gain: Option<WifiGainEvidence>,
}

#[derive(Serialize)]
#[serde(transparent)]
pub(crate) struct EvidenceFile {
    pub(crate) sha256: String,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    stamp: Metadata,
}

impl EvidenceFile {
    pub(crate) fn read_json(path: PathBuf, maximum_bytes: u64) -> Result<(Value, Self)> {
        let (value, sha256, stamp) = report::read_json_with_snapshot(&path, maximum_bytes)?;
        Ok((
            value,
            Self {
                path,
                sha256,
                stamp,
            },
        ))
    }

    pub(crate) fn verify_stamp(&self, observed: &Metadata) -> Result<()> {
        ensure!(
            observed.is_file() && report::same_file_identity(observed, &self.stamp),
            "replay evidence changed: {}",
            self.path.display()
        );
        Ok(())
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.verify_stamp(&fs::symlink_metadata(&self.path)?)?;
        if self.stamp.len() > 0 {
            let (digest, stamp) =
                report::hash_direct_regular_file_with_snapshot(&self.path, self.stamp.len())?;
            self.verify_stamp(&stamp)?;
            ensure!(
                digest.sha256 == self.sha256,
                "replay evidence digest changed: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
            .open(&self.path)?;
        self.verify_stamp(&file.metadata()?)?;
        file.sync_all()?;
        self.verify_stamp(&fs::symlink_metadata(&self.path)?)
    }
}

#[derive(Serialize)]
struct ReplayFiles {
    #[serde(rename = "summary_sha256")]
    summary: EvidenceFile,
    #[serde(rename = "terminal_sha256")]
    terminal: EvidenceFile,
    #[serde(rename = "request_sha256", skip_serializing_if = "Option::is_none")]
    request: Option<EvidenceFile>,
    #[serde(rename = "packets_sha256")]
    packets: EvidenceFile,
    #[serde(rename = "frame_spans_sha256", skip_serializing_if = "Option::is_none")]
    frame_spans: Option<EvidenceFile>,
    #[serde(
        rename = "ht_signal_events_sha256",
        skip_serializing_if = "Option::is_none"
    )]
    ht_signal_events: Option<EvidenceFile>,
    #[serde(
        rename = "he_su_signal_events_sha256",
        skip_serializing_if = "Option::is_none"
    )]
    he_su_signal_events: Option<EvidenceFile>,
}

impl ReplayFiles {
    fn iter(&self) -> impl Iterator<Item = &EvidenceFile> {
        [
            Some(&self.summary),
            Some(&self.terminal),
            self.request.as_ref(),
            Some(&self.packets),
            self.frame_spans.as_ref(),
            self.ht_signal_events.as_ref(),
            self.he_su_signal_events.as_ref(),
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Serialize)]
pub(crate) struct LaneEvidence {
    validated_packets: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_validated_packets: Option<usize>,
    monitor_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoder_sha256: Option<String>,
    #[serde(flatten)]
    files: ReplayFiles,
    #[serde(skip)]
    packet_starts: Vec<u64>,
    #[serde(skip)]
    wifi_frame_spans: Option<Vec<Range<u64>>>,
    #[serde(rename = "ht_signal", skip_serializing_if = "Option::is_none")]
    wifi_ht_signal: Option<WifiSignalEvidence>,
    #[serde(rename = "he_su_signal", skip_serializing_if = "Option::is_none")]
    wifi_he_su_signal: Option<WifiSignalEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    q11_gain: Option<WifiGainEvidence>,
}

/// Mode fields from an integrity-checked HT header.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct WifiHtMode {
    pub(crate) mcs: u8,
    pub(crate) channel_width_40_mhz: bool,
    pub(crate) ldpc: bool,
    pub(crate) short_guard_interval: bool,
    pub(crate) stbc: u8,
    pub(crate) aggregation: bool,
    pub(crate) extension_spatial_streams: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WifiHtObservation {
    pub(crate) preamble_sample_index: u64,
    pub(crate) mode: WifiHtMode,
    /// An independently validated frame starts at this exact preamble sample.
    pub(crate) validated_frame: bool,
}

/// Half-open ownership on the capture sample grid.
#[derive(Clone, Copy, Serialize)]
pub(crate) struct SampleOwnership {
    pub(crate) decoded_start_sample: u64,
    pub(crate) owned_start_sample: u64,
    pub(crate) owned_end_sample_exclusive: u64,
}

impl SampleOwnership {
    fn owns(self, sample: u64) -> bool {
        (self.owned_start_sample..self.owned_end_sample_exclusive).contains(&sample)
    }
}

impl LaneEvidence {
    pub(crate) fn validated_packets(&self) -> usize {
        self.validated_packets
    }

    pub(crate) fn packet_starts(&self) -> &[u64] {
        &self.packet_starts
    }

    pub(crate) fn wifi_frame_spans(&self) -> Option<&[Range<u64>]> {
        self.wifi_frame_spans.as_deref()
    }

    /// All observed integrity-checked header starts on the replay sample grid.
    pub(crate) fn wifi_ht_preambles(&self) -> Option<&[u64]> {
        self.wifi_ht_signal
            .as_ref()
            .map(WifiSignalEvidence::preambles)
    }

    pub(crate) fn wifi_ht_signal_sha256(&self) -> Option<&str> {
        self.files
            .ht_signal_events
            .as_ref()
            .map(|file| file.sha256.as_str())
    }

    /// Unique owned HT preambles on the replay sample grid, with verified frame-start matches.
    pub(crate) fn wifi_ht_observations(
        &self,
        ownership: Option<SampleOwnership>,
    ) -> Result<Option<Vec<WifiHtObservation>>> {
        self.wifi_ht_signal
            .as_ref()
            .zip(self.wifi_frame_spans.as_deref())
            .map(|(headers, spans)| headers.ht_observations(spans, ownership))
            .transpose()
    }

    /// Whether the HT sidecar or native diagnostic output reached capacity.
    pub(crate) fn wifi_ht_at_capacity(&self) -> Option<bool> {
        self.wifi_ht_signal
            .as_ref()
            .map(WifiSignalEvidence::at_capacity)
    }

    /// All observed HE SU preamble starts on the replay sample grid.
    pub(crate) fn wifi_he_su_preambles(&self) -> Option<&[u64]> {
        self.wifi_he_su_signal
            .as_ref()
            .map(WifiSignalEvidence::preambles)
    }

    pub(crate) fn wifi_he_su_signal_sha256(&self) -> Option<&str> {
        self.files
            .he_su_signal_events
            .as_ref()
            .map(|file| file.sha256.as_str())
    }

    pub(crate) fn wifi_he_su_owned_events(&self) -> Option<usize> {
        self.wifi_he_su_signal
            .as_ref()
            .map(WifiSignalEvidence::owned_events)
    }

    pub(crate) fn wifi_gain_clipped_components(&self) -> Option<u64> {
        self.q11_gain.as_ref().map(|gain| gain.clipped_components)
    }

    pub(crate) fn verify(&self) -> Result<()> {
        self.files.iter().try_for_each(EvidenceFile::verify)
    }
}

pub(super) struct SliceResult {
    pub(super) replay: Replay,
    pub(super) supervised: Duration,
    pub(super) retained_bytes: u64,
}

pub(super) fn read(
    directory: &Path,
    protocol: Protocol,
    source: &SourceReport,
    start_sample: u64,
    wifi: WifiReplayOptions,
    stop: &AtomicBool,
) -> Result<Replay> {
    let (summary, summary_file) =
        EvidenceFile::read_json(directory.join("summary.json"), JSON_BYTES)?;
    let (expected, decoder_sha256, terminal, request, q11_gain, signals) = match protocol {
        Protocol::Wifi => {
            let (decode, terminal) =
                EvidenceFile::read_json(directory.join("candidates.decode.json"), JSON_BYTES)?;
            if let Some(tuning) = wifi.tuning {
                tuning.verify(&decode, source)?;
                orchestrate::wifi_outcome_at(
                    &summary,
                    &decode,
                    source,
                    tuning.center_frequency_hz,
                )?;
            } else {
                orchestrate::wifi_outcome(&summary, &decode, source)?;
            }
            ensure!(
                decode["timestamp_basis"].as_str() == Some("sample-relative")
                    && decode["iq_format"].as_str() == Some("sc16-q11"),
                "Wi-Fi event replay requires sample-relative SC16 Q11 decoding"
            );
            (
                report::required_u64(&summary, "validated_frames")?,
                None,
                terminal,
                None,
                WifiGainEvidence::from_report(&decode, wifi.double_q11, source.complex_samples)?,
                (
                    wifi.ht_signal_events
                        .then(|| wifi_signal::read::<HtEvent>(directory, &decode, start_sample))
                        .transpose()?,
                    wifi.he_su_signal_events
                        .then(|| wifi_signal::read::<HeSuEvent>(directory, &decode, start_sample))
                        .transpose()?,
                ),
            )
        }
        Protocol::Bluetooth => {
            let (terminal, terminal_file) =
                EvidenceFile::read_json(directory.join("replay.json"), JSON_BYTES)?;
            orchestrate::bluetooth_outcome(&terminal, source)?;
            let expected = report::required_u64(&terminal, "validated_ble_packets")?;
            let ads = report::required_u64(&summary, "validated_advertising_packets")?;
            let data = report::required_u64(&summary, "validated_data_packets")?;
            ensure!(
                ads.checked_add(data) == Some(expected),
                "BLE replay and packet counts differ"
            );
            let program = terminal
                .get("decoder_program")
                .context("Bluetooth replay lacks decoder identity")?;
            let (request, request_file) =
                EvidenceFile::read_json(directory.join("request.json"), JSON_BYTES)?;
            let acquisition = &request["acquisition"];
            ensure!(
                request["schema_version"].as_u64() == Some(1)
                    && request["operation"].as_str() == Some("offline_sc16q11_replay")
                    && request["decoder_program"] == *program
                    && acquisition["frequency_hz"].as_f64()
                        == Some(source.center_frequency_hz as f64)
                    && acquisition["sample_rate_hz"].as_f64() == Some(source.sample_rate_hz as f64)
                    && acquisition["bandwidth_hz"].as_f64() == Some(source.bandwidth_hz as f64)
                    && acquisition["num_samples"].as_u64() == Some(source.complex_samples),
                "Bluetooth replay request differs from its source or terminal decoder identity"
            );
            let digest = program
                .get("sha256")
                .and_then(Value::as_str)
                .context("Bluetooth replay lacks decoder SHA-256")?;
            ensure!(
                program["schema_version"].as_u64() == Some(1)
                    && program["name"].as_str() == Some("blue-dragon")
                    && program["bytes"].as_u64().is_some_and(|bytes| bytes > 0)
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid Blue Dragon decoder identity"
            );
            (
                expected,
                Some(digest.to_owned()),
                terminal_file,
                Some(request_file),
                None,
                (None, None),
            )
        }
    };
    let packet_path = directory.join("packets.jsonl");
    let packets = read_packets(
        &packet_path,
        protocol,
        start_sample,
        source.complex_samples,
        expected,
        stop,
    )?;
    let (wifi_frame_spans, frame_spans) = if wifi.frame_spans {
        ensure!(
            packets.record_count == report::required_u64(&summary, "total_records")?,
            "Wi-Fi packet record count differs from its inspector summary"
        );
        let (value, file) = EvidenceFile::read_json(
            directory.join("candidates.frame-spans.json"),
            WIFI_FRAME_SPAN_BYTES,
        )?;
        let spans: WifiFrameSpans =
            serde_json::from_value(value).context("parse Wi-Fi frame spans")?;
        let spans = spans.verify(
            source,
            start_sample,
            packets.record_count,
            packets
                .packets
                .iter()
                .map(|packet| (packet.record_index, packet.sample)),
        )?;
        (Some(spans), Some(file))
    } else {
        (None, None)
    };
    let (wifi_ht_signal, ht_signal_events) = signals.0.unzip();
    let (wifi_he_su_signal, he_su_signal_events) = signals.1.unzip();
    Ok(Replay {
        packets: packets.packets,
        decoder_sha256,
        wifi_frame_spans,
        wifi_ht_signal,
        wifi_he_su_signal,
        q11_gain,
        files: ReplayFiles {
            summary: summary_file,
            terminal,
            request,
            frame_spans,
            ht_signal_events,
            he_su_signal_events,
            packets: EvidenceFile {
                path: packet_path,
                sha256: packets.sha256,
                stamp: packets.stamp,
            },
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WifiFrameSpans {
    schema_version: u32,
    sample_rate_hz: u64,
    input_samples: u64,
    processed_samples: u64,
    spans: Vec<[u64; 2]>,
}

impl WifiFrameSpans {
    fn verify(
        self,
        source: &SourceReport,
        decoded_start: u64,
        record_count: u64,
        packet_starts: impl Iterator<Item = (u64, u64)>,
    ) -> Result<Vec<Range<u64>>> {
        ensure!(
            self.schema_version == 1
                && self.sample_rate_hz == source.sample_rate_hz
                && self.input_samples == source.complex_samples
                && self.processed_samples == source.complex_samples
                && self.spans.len() as u64 == record_count
                && self
                    .spans
                    .iter()
                    .all(|&[start, end]| start < end && end <= self.processed_samples),
            "Wi-Fi frame span report differs from the complete decode extent or packet count"
        );
        let limit = decoded_start
            .checked_add(source.complex_samples)
            .context("Wi-Fi frame span source extent overflow")?;
        packet_starts
            .map(|(record, packet_start)| {
                let index = record
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .context("invalid Wi-Fi packet record index")?;
                let &[start, end] = self.spans.get(index).context("missing Wi-Fi frame span")?;
                ensure!(
                    decoded_start.checked_add(start) == Some(packet_start),
                    "Wi-Fi frame span does not match a validated packet start or saved IQ"
                );
                let end = decoded_start
                    .checked_add(end)
                    .filter(|end| *end <= limit)
                    .context("Wi-Fi frame span end overflow")?;
                Ok(packet_start..end)
            })
            .collect()
    }
}

#[cfg(test)]
#[test]
fn frame_spans_bind_valid_packet_ordinals_and_skip_unvalidated_records() {
    let source = test_source();
    let verify = |spans, starts: [(u64, u64); 3]| {
        WifiFrameSpans {
            schema_version: 1,
            sample_rate_hz: 20_000_000,
            input_samples: 200,
            processed_samples: 200,
            spans,
        }
        .verify(&source, 1_000, 4, starts.into_iter())
    };
    let spans = vec![[5, 40], [75, 90], [5, 40], [130, 200]];
    let valid_starts = [(1, 1_005), (3, 1_005), (4, 1_130)];
    assert_eq!(
        verify(spans.clone(), valid_starts).unwrap(),
        [1_005..1_040, 1_005..1_040, 1_130..1_200]
    );
    assert!(verify(spans, [(1, 1_005), (2, 1_005), (4, 1_130)]).is_err());
    assert!(verify(vec![[5, 40]], valid_starts).is_err());
    assert!(verify(vec![[5, 40], [75, 90], [5, 40], [130, 201]], valid_starts).is_err());
}

#[cfg(test)]
fn test_source() -> SourceReport {
    SourceReport {
        input: "iq.sc16q11".into(),
        sidecar: "iq.sc16q11.json".into(),
        format: "sc16-q11",
        center_frequency_hz: 2_437_000_000,
        sample_rate_hz: 20_000_000,
        bandwidth_hz: 20_000_000,
        complex_samples: 200,
        bytes: 800,
        sha256: String::new(),
        sidecar_sha256: String::new(),
        continuity_guaranteed: true,
    }
}

struct ReplayInput<'a> {
    source: &'a SourceReport,
    iq: &'a Path,
    start_sample: u64,
    decoder: Option<&'a Path>,
    wifi: WifiReplayOptions,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct WifiReplayOptions {
    pub(crate) frame_spans: bool,
    pub(crate) ht_signal_events: bool,
    pub(crate) he_su_signal_events: bool,
    pub(crate) tuning: Option<WifiTuning>,
    pub(crate) double_q11: bool,
}

#[cfg(test)]
#[test]
fn wifi_signal_events_are_opt_in_for_wifi_only() {
    let source = test_source();
    let ht = WifiReplayOptions {
        ht_signal_events: true,
        ..WifiReplayOptions::default()
    };
    let he_su = WifiReplayOptions {
        he_su_signal_events: true,
        ..WifiReplayOptions::default()
    };
    let flags = |protocol, options| {
        let args = arguments(protocol, &source, &source.input, Path::new("out"), options);
        [
            args.iter().any(|arg| arg == "--ht-signal-events"),
            args.iter().any(|arg| arg == "--he-su-signal-events"),
        ]
    };
    assert_eq!(
        flags(Protocol::Wifi, WifiReplayOptions::default()),
        [false, false]
    );
    assert_eq!(flags(Protocol::Wifi, ht), [true, false]);
    assert_eq!(flags(Protocol::Wifi, he_su), [false, true]);
    assert_eq!(flags(Protocol::Bluetooth, ht), [false, false]);
    assert_eq!(flags(Protocol::Bluetooth, he_su), [false, false]);
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WifiGainEvidence {
    factor: u64,
    clipped_components: u64,
}

impl WifiGainEvidence {
    fn from_report(decode: &Value, requested: bool, samples: u64) -> Result<Option<Self>> {
        let evidence = decode.get("q11_gain");
        ensure!(
            evidence.is_some() == requested,
            "Wi-Fi terminal Q11 gain evidence differs from the replay request"
        );
        let Some(evidence) = evidence else {
            return Ok(None);
        };
        let gain = Self::deserialize(evidence).context("invalid Wi-Fi terminal Q11 gain shape")?;
        ensure!(
            gain.factor == 2
                && samples
                    .checked_mul(2)
                    .is_some_and(|limit| gain.clipped_components <= limit),
            "invalid Wi-Fi terminal Q11 gain factor or clipped component count"
        );
        Ok(Some(gain))
    }
}

#[cfg(test)]
#[test]
fn wifi_q11_gain_is_bound_to_request_and_decoded_extent() {
    use serde_json::json;

    let source = test_source();
    let child_arguments = |double_q11| {
        arguments(
            Protocol::Wifi,
            &source,
            &source.input,
            Path::new("out"),
            WifiReplayOptions {
                double_q11,
                ..WifiReplayOptions::default()
            },
        )
    };
    assert!(
        !child_arguments(false)
            .iter()
            .any(|arg| arg == "--double-q11")
    );
    assert!(
        child_arguments(true)
            .iter()
            .any(|arg| arg == "--double-q11")
    );

    let verify = |terminal, requested| {
        WifiGainEvidence::from_report(&terminal, requested, source.complex_samples)
    };
    assert!(verify(json!({}), false).unwrap().is_none());
    assert!(verify(json!({}), true).is_err());

    let evidence = json!({"factor": 2, "clipped_components": 400});
    let terminal = json!({"q11_gain": evidence});
    assert!(verify(terminal.clone(), false).is_err());
    assert_eq!(
        serde_json::to_value(verify(terminal, true).unwrap().unwrap()).unwrap(),
        evidence
    );
    for invalid in [
        Value::Null,
        json!({"factor": 1, "clipped_components": 0}),
        json!({"factor": 2, "clipped_components": 401}),
        json!({"factor": 2, "clipped_components": -1}),
        json!({"factor": 2}),
        json!({"factor": 2, "clipped_components": 0, "unknown": 1}),
    ] {
        assert!(verify(json!({"q11_gain": invalid}), true).is_err());
    }
}

/// Absolute capture and decoder centers with the signed IQ mixer shift and filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WifiTuning {
    capture_frequency_hz: u64,
    center_frequency_hz: u64,
    frequency_shift_hz: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    translation_filter: Option<&'static str>,
}

impl WifiTuning {
    /// An equal decoder center uses the original samples and decoder contract.
    pub(crate) fn new(capture_frequency_hz: u64, center_mhz: u32) -> Result<Option<Self>> {
        let center_frequency_hz = u64::from(center_mhz) * 1_000_000;
        let distance_hz = capture_frequency_hz.abs_diff(center_frequency_hz);
        ensure!(
            distance_hz <= 2_000_000,
            "rolling Wi-Fi decoder center must be within 2 MHz of the capture center"
        );
        let frequency_shift_hz =
            i64::try_from(capture_frequency_hz)? - i64::try_from(center_frequency_hz)?;
        Ok((frequency_shift_hz != 0).then_some(Self {
            capture_frequency_hz,
            center_frequency_hz,
            frequency_shift_hz,
            translation_filter: (distance_hz > 1_000_000).then_some(WIFI_TRANSLATION_FILTER),
        }))
    }

    pub(crate) fn center_frequency_hz(self) -> u64 {
        self.center_frequency_hz
    }

    fn verify(self, decode: &Value, source: &SourceReport) -> Result<()> {
        ensure!(
            self.capture_frequency_hz == source.center_frequency_hz
                && report::required_u64(decode, "capture_frequency_hz")?
                    == self.capture_frequency_hz
                && decode["frequency_shift_hz"].as_i64() == Some(self.frequency_shift_hz)
                && report::required_u64(decode, "translation_clipped_samples")?
                    <= source.complex_samples,
            "Wi-Fi translation differs from the validated capture or requested shift"
        );
        let reported_filter = decode.get("translation_filter");
        ensure!(
            match self.translation_filter {
                Some(filter) => reported_filter.and_then(Value::as_str) == Some(filter),
                None => reported_filter.is_none(),
            },
            "Wi-Fi translation filter evidence differs from the requested shift"
        );
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn wifi_translation_binds_tuning_and_clipping_evidence() {
    use serde_json::json;

    let source = test_source();
    let tuning = WifiTuning::new(source.center_frequency_hz, 2436)
        .unwrap()
        .unwrap();
    let options = arguments(
        Protocol::Wifi,
        &source,
        &source.input,
        Path::new("out"),
        WifiReplayOptions {
            tuning: Some(tuning),
            ..WifiReplayOptions::default()
        },
    );
    assert!(
        options
            .windows(2)
            .any(|pair| pair == ["--frequency-mhz", "2436"])
    );
    assert!(
        options
            .windows(2)
            .any(|pair| pair == ["--capture-frequency-hz", "2437000000"])
    );
    assert!(
        WifiTuning::new(source.center_frequency_hz, 2437)
            .unwrap()
            .is_none()
    );
    assert!(WifiTuning::new(source.center_frequency_hz, 2434).is_err());
    assert!(WifiTuning::new(2_437_000_001, 2435).is_err());
    assert_eq!(
        serde_json::to_value(tuning).unwrap(),
        json!({
            "capture_frequency_hz": 2_437_000_000u64,
            "center_frequency_hz": 2_436_000_000u64,
            "frequency_shift_hz": 1_000_000,
        })
    );
    let report = json!({
        "capture_frequency_hz": 2_437_000_000u64,
        "frequency_shift_hz": 1_000_000,
        "translation_clipped_samples": 13
    });
    tuning.verify(&report, &source).unwrap();
    for (field, invalid) in [
        ("capture_frequency_hz", json!(2_436_000_000u64)),
        ("frequency_shift_hz", json!(-1_000_000)),
        ("translation_clipped_samples", json!(201)),
        ("translation_clipped_samples", Value::Null),
        ("translation_filter", json!("anti-wrap-kaiser-257")),
        ("translation_filter", Value::Null),
    ] {
        let mut wrong = report.clone();
        wrong[field] = invalid;
        assert!(tuning.verify(&wrong, &source).is_err(), "{field}");
    }
}

#[cfg(test)]
#[test]
fn wide_wifi_translation_requires_and_records_anti_wrap_filter() {
    use serde_json::json;

    let source = test_source();
    for (center_mhz, shift_hz) in [(2435, 2_000_000), (2439, -2_000_000)] {
        let tuning = WifiTuning::new(source.center_frequency_hz, center_mhz)
            .unwrap()
            .unwrap();
        let center = center_mhz.to_string();
        let options = arguments(
            Protocol::Wifi,
            &source,
            &source.input,
            Path::new("out"),
            WifiReplayOptions {
                tuning: Some(tuning),
                ..WifiReplayOptions::default()
            },
        );
        assert!(
            options
                .windows(2)
                .any(|pair| pair == ["--frequency-mhz", center.as_str()])
        );
        assert!(
            options
                .windows(2)
                .any(|pair| pair == ["--capture-frequency-hz", "2437000000"])
        );
        let mut report = json!({
            "capture_frequency_hz": 2_437_000_000u64,
            "frequency_shift_hz": shift_hz,
            "translation_clipped_samples": 0,
        });
        assert!(tuning.verify(&report, &source).is_err());
        for invalid in [json!("kaiser-257"), Value::Null, json!(257)] {
            report["translation_filter"] = invalid;
            assert!(tuning.verify(&report, &source).is_err());
        }
        report["translation_filter"] = json!("anti-wrap-kaiser-257");
        tuning.verify(&report, &source).unwrap();
        assert_eq!(
            serde_json::to_value(tuning).unwrap(),
            json!({
                "capture_frequency_hz": 2_437_000_000u64,
                "center_frequency_hz": u64::from(center_mhz) * 1_000_000,
                "frequency_shift_hz": shift_hz,
                "translation_filter": "anti-wrap-kaiser-257",
            })
        );
    }
}

fn run_input(
    program: &process::ProgramProvenance,
    protocol: Protocol,
    input: ReplayInput<'_>,
    directory: &Path,
    budget: Duration,
    stop: &AtomicBool,
) -> Result<(Replay, Duration)> {
    report::private_directory(directory)?;
    report::private_directory(&directory.join("logs"))?;
    let destination = directory.join("decoded");
    let mut args = arguments(protocol, input.source, input.iq, &destination, input.wifi);
    if let Some(decoder) = input.decoder {
        args.extend(["--decoder".into(), decoder.as_os_str().to_owned()]);
    }
    let supervision_started = Instant::now();
    let command = process::run(program, &args, directory, "decode", LOG_BYTES, budget, stop)?;
    let supervised = supervision_started.elapsed();
    process::require_success(&command).with_context(|| format!("decode {}", input.iq.display()))?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "replay decoder logs exceeded their evidence bound"
    );
    let replay = read(
        &destination,
        protocol,
        input.source,
        input.start_sample,
        input.wifi,
        stop,
    )?;
    Ok((replay, supervised))
}

pub(crate) enum ReplayLane<'a> {
    Wifi(WifiReplayOptions),
    Bluetooth {
        decoder: &'a process::ProgramProvenance,
    },
}

/// Replay the complete admitted IQ with one pinned decoder and sync its evidence.
pub(crate) fn run_full(
    program: &process::ProgramProvenance,
    lane: ReplayLane<'_>,
    source: &SourceReport,
    ownership: Option<SampleOwnership>,
    directory: &Path,
    stop: &AtomicBool,
) -> Result<LaneEvidence> {
    let (protocol, blue_dragon, wifi) = match lane {
        ReplayLane::Wifi(wifi) => (Protocol::Wifi, None, wifi),
        ReplayLane::Bluetooth { decoder } => (
            Protocol::Bluetooth,
            Some(decoder),
            WifiReplayOptions::default(),
        ),
    };
    let start_sample = ownership.map_or(0, |window| window.decoded_start_sample);
    if let Some(window) = ownership {
        ensure!(
            start_sample <= window.owned_start_sample
                && window.owned_start_sample < window.owned_end_sample_exclusive
                && start_sample
                    .checked_add(source.complex_samples)
                    .is_some_and(|end| window.owned_end_sample_exclusive <= end),
            "packet ownership must be inside the decoded IQ extent"
        );
    }
    let (mut replay, _) = run_input(
        program,
        protocol,
        ReplayInput {
            source,
            iq: &source.input,
            start_sample,
            decoder: blue_dragon.map(|decoder| decoder.path.as_path()),
            wifi,
        },
        directory,
        CHILD_TIMEOUT,
        stop,
    )?;
    ensure!(
        replay.decoder_sha256.as_deref() == blue_dragon.map(|decoder| decoder.sha256.as_str()),
        "nested decoder differs from the pinned replay program"
    );
    ensure!(
        output_bytes(directory)? <= 128 * 1024 * 1024,
        "whole-segment decoder output exceeds 128 MiB"
    );
    replay.files.iter().try_for_each(EvidenceFile::sync)?;
    File::open(directory.join("decoded"))?.sync_all()?;
    File::open(directory)?.sync_all()?;
    let observed = replay.packets.len();
    if let Some(window) = ownership {
        [
            replay.wifi_ht_signal.as_mut(),
            replay.wifi_he_su_signal.as_mut(),
        ]
        .into_iter()
        .flatten()
        .for_each(|headers| headers.count_owned(window));
    }
    Ok(LaneEvidence {
        validated_packets: ownership.map_or(observed, |window| {
            replay
                .packets
                .iter()
                .filter(|packet| window.owns(packet.sample))
                .count()
        }),
        observed_validated_packets: ownership.map(|_| observed),
        monitor_sha256: program.sha256.clone(),
        decoder_sha256: replay.decoder_sha256,
        files: replay.files,
        wifi_frame_spans: replay.wifi_frame_spans,
        wifi_ht_signal: replay.wifi_ht_signal,
        wifi_he_su_signal: replay.wifi_he_su_signal,
        q11_gain: replay.q11_gain,
        packet_starts: replay
            .packets
            .into_iter()
            .map(|packet| packet.sample)
            .collect(),
    })
}

#[cfg(test)]
#[test]
fn adjacent_packet_owners_partition_their_shared_decode_context() {
    let windows = [
        SampleOwnership {
            decoded_start_sample: 0,
            owned_start_sample: 0,
            owned_end_sample_exclusive: 100,
        },
        SampleOwnership {
            decoded_start_sample: 0,
            owned_start_sample: 100,
            owned_end_sample_exclusive: 200,
        },
    ];
    for sample in 0..200 {
        assert_eq!(
            windows.map(|window| window.owns(sample)),
            [sample < 100, sample >= 100]
        );
    }
    assert!(windows.iter().all(|window| !window.owns(200)));
}

pub(super) struct Runner<'a> {
    pub(super) program: &'a process::ProgramProvenance,
    pub(super) protocol: Protocol,
    pub(super) source: &'a SourceReport,
    pub(super) slice_root: &'a Path,
    pub(super) stop: &'a AtomicBool,
    pub(super) slots: Option<&'a DecodeSlots>,
}

impl Runner<'_> {
    /// Replay one slice and measure time inside the process supervisor.
    pub(super) fn run(
        &self,
        slice: &Slice,
        directory: &Path,
        budget: Duration,
    ) -> Result<(Replay, Duration)> {
        let iq = self.slice_root.join(&slice.iq_file);
        let mut source = self.source.clone();
        source.complex_samples = slice.samples();
        run_input(
            self.program,
            self.protocol,
            ReplayInput {
                source: &source,
                iq: &iq,
                start_sample: slice.start_sample,
                decoder: None,
                wifi: WifiReplayOptions::default(),
            },
            directory,
            budget.min(CHILD_TIMEOUT),
            self.stop,
        )
    }

    /// Replay each slice in its own directory and return results in plan order.
    pub(super) fn run_group(
        &self,
        group: &[Slice],
        output: &Path,
        deadline: Instant,
    ) -> Result<Vec<SliceResult>> {
        let run = |slice: &Slice| -> Result<SliceResult> {
            let directory = output.join(&slice.iq_file);
            let _permit = self
                .slots
                .map(|slots| slots.acquire(deadline, self.stop))
                .transpose()?;
            let budget = remaining(deadline, self.stop)?;
            let (replay, supervised) = self.run(slice, &directory, budget)?;
            Ok(SliceResult {
                replay,
                supervised,
                retained_bytes: output_bytes(&directory)?,
            })
        };
        if let [slice] = group {
            return Ok(vec![run(slice)?]);
        }
        thread::scope(|scope| {
            let run = &run;
            let mut workers = Vec::with_capacity(group.len());
            for slice in group {
                let worker = thread::Builder::new()
                    .name("event-replay-slice".into())
                    .spawn_scoped(scope, move || run(slice));
                let launch_failed = worker.is_err();
                workers.push(worker);
                if launch_failed {
                    break;
                }
            }
            // Join every started worker before returning any error.
            let ordered: Vec<_> = workers
                .into_iter()
                .zip(group)
                .map(|(worker, slice)| {
                    worker
                        .with_context(|| format!("start replay worker for {}", slice.iq_file))?
                        .join()
                        .map_err(|_| {
                            anyhow::anyhow!("replay worker panicked for {}", slice.iq_file)
                        })?
                })
                .collect();
            ordered.into_iter().collect()
        })
    }
}

fn arguments(
    protocol: Protocol,
    source: &SourceReport,
    iq: &Path,
    output: &Path,
    wifi: WifiReplayOptions,
) -> Vec<OsString> {
    let center_hz = wifi.tuning.map_or(source.center_frequency_hz, |tuning| {
        tuning.center_frequency_hz
    });
    let center = (center_hz / 1_000_000).to_string();
    let mut args: Vec<OsString> = match protocol {
        Protocol::Wifi => [
            "decode-iq".into(),
            iq.as_os_str().to_owned(),
            "--format".into(),
            "sc16q11".into(),
            "--frequency-mhz".into(),
            center.into(),
            "--decode-timeout".into(),
            "20".into(),
        ]
        .into(),
        Protocol::Bluetooth => [
            "replay-iq".into(),
            iq.as_os_str().to_owned(),
            "--center-mhz".into(),
            center.into(),
            "--sample-rate".into(),
            "20000000".into(),
            "--squelch".into(),
            "-45".into(),
            "--decoder-timeout".into(),
            "20".into(),
            "--burst-limit-mib".into(),
            "16".into(),
            "--output-limit-mib".into(),
            "32".into(),
        ]
        .into(),
    };
    if let Some(tuning) = wifi.tuning {
        args.extend([
            "--capture-frequency-hz".into(),
            tuning.capture_frequency_hz.to_string().into(),
        ]);
    }
    args.extend([
        "--max-samples".into(),
        source.complex_samples.to_string().into(),
        "--output".into(),
        output.as_os_str().to_owned(),
    ]);
    if wifi.frame_spans {
        args.push("--frame-spans".into());
    }
    if matches!(protocol, Protocol::Wifi) {
        if wifi.ht_signal_events {
            args.push("--ht-signal-events".into());
        }
        if wifi.he_su_signal_events {
            args.push("--he-su-signal-events".into());
        }
    }
    if wifi.double_q11 {
        args.push("--double-q11".into());
    }
    args
}

pub(super) fn output_bytes(directory: &Path) -> Result<u64> {
    fn walk(directory: &Path, depth: usize, entries: &mut usize) -> Result<u64> {
        ensure!(
            depth <= 3,
            "slice decoder output contains excessive directory nesting"
        );
        let mut bytes = 0u64;
        for entry in fs::read_dir(directory)? {
            *entries += 1;
            ensure!(
                *entries <= 64,
                "slice decoder output contains too many entries"
            );
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            let added = if metadata.is_dir() {
                walk(&entry.path(), depth + 1, entries)?
            } else {
                ensure!(
                    metadata.is_file(),
                    "slice decoder output contains a non-regular leaf"
                );
                metadata.len()
            };
            bytes = bytes
                .checked_add(added)
                .context("slice decoder output size overflow")?;
        }
        Ok(bytes)
    }
    walk(directory, 0, &mut 0)
}
