//! Decode one admitted segment and record its verified Wi-Fi and BLE evidence.

use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::event_replay::{
    Protocol,
    replay::{
        self, EvidenceFile, LaneEvidence, ReplayLane, SampleOwnership, WifiReplayOptions,
        WifiTuning,
    },
};
use crate::input::{NORMALIZED_SC16_Q11_FORMAT, SourceReport};
use crate::{process, report};

use super::overlap::{CONTEXT_SAMPLES, PREFIX_SAMPLES, TailProof};
use super::wifi_ht::WifiHtModes;

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const RECEIPT_NAME: &str = "direct.json";
const RECEIPT_BYTES: u64 = 8192;
const WIFI_HT_MODES_NAME: &str = "wifi-ht-modes.json";

pub(super) struct BoundaryDecode<'a> {
    pub(super) staged_source: Option<&'a SourceReport>,
    pub(super) segment_start_sample: u64,
    pub(super) owned_start_sample: u64,
    pub(super) owned_end_sample_exclusive: u64,
    pub(super) prefix: Option<&'a TailProof>,
}

#[derive(Serialize)]
struct Coverage<'a> {
    segment_start_sample: u64,
    decoded_end_sample_exclusive: u64,
    #[serde(flatten)]
    ownership: SampleOwnership,
    decoded_iq_sha256: &'a str,
    decoded_sidecar_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<&'a TailProof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix_receipt_sha256: Option<&'a str>,
}

impl<'a> BoundaryDecode<'a> {
    fn coverage(&self, source: &'a SourceReport) -> Result<Coverage<'a>> {
        let segment_end = self
            .segment_start_sample
            .checked_add(source.complex_samples)
            .context("rolling segment sample extent overflow")?;
        let (decoded_start, decoded) = match (self.staged_source, self.prefix) {
            (None, None) if self.segment_start_sample == 0 => (0, source),
            (Some(staged), Some(prefix)) => {
                let decoded_start = self
                    .segment_start_sample
                    .checked_sub(PREFIX_SAMPLES)
                    .context("segment begins before its decoded prefix")?;
                ensure!(
                    prefix.start_sample == decoded_start
                        && prefix.samples == PREFIX_SAMPLES
                        && decoded_start.checked_add(staged.complex_samples) == Some(segment_end)
                        && staged.complex_samples.checked_mul(4) == Some(staged.bytes)
                        && staged.format == source.format
                        && staged.center_frequency_hz == source.center_frequency_hz
                        && staged.sample_rate_hz == source.sample_rate_hz
                        && staged.bandwidth_hz == source.bandwidth_hz,
                    "staged IQ extent or tuning differs from the admitted segment and prefix"
                );
                (decoded_start, staged)
            }
            _ => anyhow::bail!("boundary decode needs matching staged IQ and predecessor context"),
        };
        ensure!(
            self.owned_start_sample == self.segment_start_sample.saturating_sub(CONTEXT_SAMPLES)
                && self.owned_start_sample < self.owned_end_sample_exclusive
                && (self.owned_end_sample_exclusive == segment_end
                    || segment_end.checked_sub(CONTEXT_SAMPLES)
                        == Some(self.owned_end_sample_exclusive)),
            "packet ownership differs from the rolling boundary intervals"
        );
        Ok(Coverage {
            segment_start_sample: self.segment_start_sample,
            decoded_end_sample_exclusive: segment_end,
            ownership: SampleOwnership {
                decoded_start_sample: decoded_start,
                owned_start_sample: self.owned_start_sample,
                owned_end_sample_exclusive: self.owned_end_sample_exclusive,
            },
            decoded_iq_sha256: &decoded.sha256,
            decoded_sidecar_sha256: &decoded.sidecar_sha256,
            prefix: self.prefix,
            prefix_receipt_sha256: self.prefix.map(TailProof::receipt_sha256),
        })
    }
}

pub(super) struct DirectPlan {
    wifi: process::ProgramProvenance,
    bluetooth: process::ProgramProvenance,
    blue_dragon: process::ProgramProvenance,
    wifi_options: WifiReplayOptions,
    track_wifi_ht_modes: bool,
}

#[derive(Serialize)]
pub(super) struct DirectReport {
    #[serde(rename = "receipt_sha256")]
    receipt: EvidenceFile,
    wifi: LaneEvidence,
    bluetooth: LaneEvidence,
    #[serde(skip)]
    packet_starts_are_global: bool,
    #[serde(skip)]
    wifi_ht_modes: Option<(WifiHtModes, EvidenceFile)>,
}

impl DirectReport {
    pub(super) fn wifi_ht_modes(&self) -> Option<&WifiHtModes> {
        self.wifi_ht_modes.as_ref().map(|(modes, _)| modes)
    }

    pub(super) fn wifi_gain_clipped_components(&self) -> Option<u64> {
        self.wifi.wifi_gain_clipped_components()
    }

    /// Return observed HT preamble starts on the stream grid and their source hash.
    pub(super) fn wifi_ht_preambles(&self, physical_start: u64) -> Option<(Vec<u64>, &str)> {
        let offset = self.sample_offset(physical_start);
        let starts = self
            .wifi
            .wifi_ht_preambles()?
            .iter()
            .map(|sample| offset + sample)
            .collect();
        Some((starts, self.wifi.wifi_ht_signal_sha256()?))
    }

    /// Return observed HE20 SU preamble starts on the stream grid and their source hash.
    pub(super) fn wifi_he_su_preambles(&self, physical_start: u64) -> Option<(Vec<u64>, &str)> {
        let offset = self.sample_offset(physical_start);
        let starts = self
            .wifi
            .wifi_he_su_preambles()?
            .iter()
            .map(|sample| offset + sample)
            .collect();
        Some((starts, self.wifi_he_su_signal_sha256()?))
    }

    pub(super) fn wifi_he_su_signal_sha256(&self) -> Option<&str> {
        self.wifi.wifi_he_su_signal_sha256()
    }

    pub(super) fn wifi_he_su_header_count(&self) -> Option<u64> {
        self.wifi
            .wifi_he_su_owned_events()
            .map(|count| count as u64)
    }

    pub(super) fn validated_packets(&self) -> (usize, usize) {
        (
            self.wifi.validated_packets(),
            self.bluetooth.validated_packets(),
        )
    }

    fn sample_offset(&self, physical_start: u64) -> u64 {
        if self.packet_starts_are_global {
            0
        } else {
            physical_start
        }
    }

    /// Return all observed validated starts, including those outside boundary ownership.
    pub(super) fn packet_starts(&self, physical_start: u64) -> Vec<u64> {
        let offset = self.sample_offset(physical_start);
        let mut starts: Vec<_> = self
            .wifi
            .packet_starts()
            .iter()
            .chain(self.bluetooth.packet_starts())
            .map(|sample| offset + sample)
            .collect();
        starts.sort_unstable();
        starts
    }

    /// Return the union of observed Wi-Fi frame spans on the stream sample grid.
    pub(super) fn wifi_frame_spans(&self, physical_start: u64) -> Option<Vec<Range<u64>>> {
        let offset = self.sample_offset(physical_start);
        let spans = self.wifi.wifi_frame_spans()?;
        let mut spans: Vec<_> = spans
            .iter()
            .map(|span| (offset + span.start)..(offset + span.end))
            .collect();
        spans.sort_unstable_by_key(|span| span.start);
        let mut union: Vec<Range<u64>> = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(last) = union.last_mut()
                && span.start <= last.end
            {
                last.end = last.end.max(span.end);
            } else {
                union.push(span);
            }
        }
        Some(union)
    }

    pub(super) fn receipt_sha256(&self) -> &str {
        &self.receipt.sha256
    }

    /// Recheck decoder evidence after the original IQ can have been removed.
    pub(super) fn verify(&self) -> Result<()> {
        self.receipt.verify()?;
        self.wifi.verify()?;
        if let Some((_, file)) = &self.wifi_ht_modes {
            file.verify()?;
        }
        self.bluetooth.verify()
    }
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    operation: &'static str,
    source_format: &'a str,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    bandwidth_hz: u64,
    samples: u64,
    iq_sha256: &'a str,
    sidecar_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage: Option<Coverage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_tuning: Option<WifiTuning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_ht_modes_sha256: Option<&'a str>,
    wifi: &'a LaneEvidence,
    bluetooth: &'a LaneEvidence,
}

pub(super) fn prepare(
    wifi_monitor: &Path,
    bluetooth_monitor: &Path,
    blue_dragon: Option<&Path>,
    wifi_options: WifiReplayOptions,
    track_wifi_ht_modes: bool,
) -> Result<DirectPlan> {
    let default_decoder;
    let decoder_path = if let Some(path) = blue_dragon {
        path
    } else {
        let directory = std::env::current_dir()?;
        let workspace = directory
            .ancestors()
            .find(|path| path.join("Cargo.toml").is_file() && path.join("repos").is_dir())
            .context("workspace for the Blue Dragon build was not found")?;
        default_decoder = workspace.join("build/blue-dragon/release/blue-dragon");
        &default_decoder
    };
    Ok(DirectPlan {
        wifi: process::resolve_program(Protocol::Wifi.program(), wifi_monitor)?,
        bluetooth: process::resolve_program(Protocol::Bluetooth.program(), bluetooth_monitor)?,
        blue_dragon: process::resolve_program("blue-dragon", decoder_path)?,
        wifi_options,
        track_wifi_ht_modes,
    })
}

impl DirectPlan {
    fn check_programs(&self) -> Result<()> {
        for program in [&self.wifi, &self.bluetooth, &self.blue_dragon] {
            ensure!(
                process::resolve_program(program.name, &program.path)? == *program,
                "{} executable changed during rolling decode",
                program.name
            );
        }
        Ok(())
    }

    /// Join both offline lanes before publishing a no-replace evidence receipt.
    /// A failed lane leaves partial artifacts and cannot authorize IQ release.
    pub(super) fn run<'a>(
        &self,
        source: &'a SourceReport,
        boundary: Option<BoundaryDecode<'a>>,
        output: &Path,
        stop: &AtomicBool,
        verify_input: impl FnOnce() -> Result<()>,
    ) -> Result<DirectReport> {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "rolling direct decode interrupted"
        );
        ensure!(
            source.format == NORMALIZED_SC16_Q11_FORMAT
                && source.sample_rate_hz == SAMPLE_RATE_HZ
                && source.bandwidth_hz == SAMPLE_RATE_HZ
                && source.center_frequency_hz.is_multiple_of(1_000_000)
                && (2_400_000_000..=2_480_000_000).contains(&source.center_frequency_hz)
                && (1..=SAMPLE_RATE_HZ).contains(&source.complex_samples)
                && source.complex_samples.is_multiple_of(10),
            "direct decode requires a full-band 2.4 GHz segment aligned to the replay grid"
        );
        let coverage = boundary
            .as_ref()
            .map(|boundary| boundary.coverage(source))
            .transpose()?;
        let ownership = coverage.as_ref().map(|coverage| coverage.ownership);
        let decoded = boundary
            .and_then(|boundary| boundary.staged_source)
            .unwrap_or(source);
        self.check_programs()?;
        report::private_directory(output)?;
        let (wifi, bluetooth) = thread::scope(|scope| {
            let wifi = thread::Builder::new()
                .name("rolling-wifi-decode".into())
                .spawn_scoped(scope, || {
                    replay::run_full(
                        &self.wifi,
                        ReplayLane::Wifi(self.wifi_options),
                        decoded,
                        ownership,
                        &output.join("wifi"),
                        stop,
                    )
                })
                .context("start rolling Wi-Fi decoder supervisor")?;
            let bluetooth = replay::run_full(
                &self.bluetooth,
                ReplayLane::Bluetooth {
                    decoder: &self.blue_dragon,
                },
                decoded,
                ownership,
                &output.join("bluetooth"),
                stop,
            );
            let wifi = wifi
                .join()
                .map_err(|_| anyhow::anyhow!("rolling Wi-Fi decoder supervisor panicked"))?;
            Ok::<_, anyhow::Error>((
                wifi.context("rolling Wi-Fi decode")?,
                bluetooth.context("rolling Bluetooth decode")?,
            ))
        })?;
        self.check_programs()?;
        verify_input()?;
        ensure!(
            !stop.load(Ordering::Relaxed),
            "rolling direct decode interrupted"
        );
        let wifi_ht_modes = if self.track_wifi_ht_modes {
            let observations = wifi
                .wifi_ht_observations(ownership)?
                .context("HT mode counting requires verified headers and frame spans")?;
            let limited = wifi
                .wifi_ht_at_capacity()
                .context("HT mode counting requires verified header capacity status")?;
            let modes = WifiHtModes::from_segment(&observations, limited);
            report::publish_json(output, WIFI_HT_MODES_NAME, &modes)?;
            let (published, file) =
                EvidenceFile::read_json(output.join(WIFI_HT_MODES_NAME), modes.json_budget())?;
            ensure!(
                published == serde_json::to_value(&modes)?,
                "published Wi-Fi HT modes differ from verified decoder evidence"
            );
            Some((modes, file))
        } else {
            None
        };
        let manifest = Receipt {
            schema_version: if coverage.is_some() { 2 } else { 1 },
            operation: "rolling_direct_decode",
            source_format: source.format,
            center_frequency_hz: source.center_frequency_hz,
            sample_rate_hz: source.sample_rate_hz,
            bandwidth_hz: source.bandwidth_hz,
            samples: source.complex_samples,
            iq_sha256: &source.sha256,
            sidecar_sha256: &source.sidecar_sha256,
            coverage,
            wifi_tuning: self.wifi_options.tuning,
            wifi_ht_modes_sha256: wifi_ht_modes.as_ref().map(|(_, file)| file.sha256.as_str()),
            wifi: &wifi,
            bluetooth: &bluetooth,
        };
        report::publish_json(output, RECEIPT_NAME, &manifest)?;
        let parent = output
            .parent()
            .context("direct decode output has no parent")?;
        File::open(parent)?.sync_all()?;
        let (published, receipt) =
            EvidenceFile::read_json(output.join(RECEIPT_NAME), RECEIPT_BYTES)?;
        ensure!(
            published == serde_json::to_value(&manifest)?,
            "published rolling direct decode receipt differs from verified evidence"
        );
        Ok(DirectReport {
            receipt,
            wifi,
            bluetooth,
            packet_starts_are_global: ownership.is_some(),
            wifi_ht_modes,
        })
    }
}

#[cfg(test)]
#[test]
fn explicit_blue_dragon_is_pinned_and_rechecked() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let monitor = root.path().join("monitor");
    let decoder = root.path().join("decoder");
    for path in [&monitor, &decoder] {
        fs::write(path, b"prebuilt").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let plan = prepare(
        &monitor,
        &monitor,
        Some(&decoder),
        WifiReplayOptions::default(),
        false,
    )
    .unwrap();
    assert_eq!(plan.blue_dragon.path, decoder.canonicalize().unwrap());
    plan.check_programs().unwrap();
    fs::write(decoder, b"modified").unwrap();
    assert!(plan.check_programs().is_err());
}

#[cfg(test)]
#[test]
fn first_boundary_receipt_keeps_full_decode_extent_and_checked_ownership() {
    let samples = SAMPLE_RATE_HZ;
    let source = SourceReport {
        input: "iq.sc16q11".into(),
        sidecar: "iq.sc16q11.json".into(),
        format: NORMALIZED_SC16_Q11_FORMAT,
        center_frequency_hz: 2_437_000_000,
        sample_rate_hz: SAMPLE_RATE_HZ,
        bandwidth_hz: SAMPLE_RATE_HZ,
        complex_samples: samples,
        bytes: samples * 4,
        sha256: "source-iq".into(),
        sidecar_sha256: "source-sidecar".into(),
        continuity_guaranteed: true,
    };
    let boundary = BoundaryDecode {
        staged_source: None,
        segment_start_sample: 0,
        owned_start_sample: 0,
        owned_end_sample_exclusive: samples - CONTEXT_SAMPLES,
        prefix: None,
    };
    let coverage = serde_json::to_value(boundary.coverage(&source).unwrap()).unwrap();
    assert_eq!(coverage["decoded_start_sample"], 0);
    assert_eq!(coverage["decoded_end_sample_exclusive"], samples);
    assert_eq!(coverage["owned_start_sample"], 0);
    assert_eq!(
        coverage["owned_end_sample_exclusive"],
        samples - CONTEXT_SAMPLES
    );
    assert_eq!(coverage["decoded_iq_sha256"], source.sha256);
    assert!(coverage.get("prefix").is_none());

    let boundary = BoundaryDecode {
        owned_end_sample_exclusive: samples - 1,
        ..boundary
    };
    assert!(boundary.coverage(&source).is_err());
}
