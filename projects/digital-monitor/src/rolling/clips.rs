//! Retain bounded full-band IQ around verified activity and decode evidence.

use std::fs::{self, Metadata, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use nix::fcntl::{RenameFlags, renameat2};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::event_replay::replay::EvidenceFile;
use crate::process::ProgramProvenance;
use crate::report;

use super::burst::{Band, BurstDetection, BurstReport, WINDOW_SAMPLES};
use super::clip_probe::{self, ClipProbeReport, ProbeRequest};
use super::detection::{Detection, Event, NARROW_CONTRAST_THRESHOLD_DB};
use super::direct::DirectReport;
use super::intake::ValidatedSegment;
use super::overlap::open_admitted_iq;

const CLIP_DIR: &str = "clips";
const RECEIPT_NAME: &str = "clips.json";
const RECEIPT_BYTES: u64 = 64 * 1024;
const MAX_CLIPS: u8 = 4;
const REPLAY_ALIGNMENT: u64 = 20;
const PRE_SAMPLES: u64 = 40_960;
const POST_SAMPLES: u64 = 204_800;
const PROXIMITY_SAMPLES: u64 = 204_800;
const GROUP_HZ: u64 = 2_000_000;
const SAMPLE_BYTES: u64 = 4;
const SAMPLE_PERIOD_NS: i64 = 50;
const PROBE_CONTEXT_SAMPLES: u64 = 400;
const PROBE_MAX_OFFSET_HZ: u64 = 9_500_000;
const MIN_BAND_BURST_SAMPLES: u64 = 4 * WINDOW_SAMPLES;
const HT_PREAMBLE_GUARD_SAMPLES: u64 = 320;
// L-STF through HE-SIG-A takes 32 us on the 20 MSps source grid.
const HE_SU_SIGNAL_PREAMBLE_SAMPLES: u64 = 640;

/// Policy for selecting clips from one segment's verified survey components.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum ClipSelection {
    Activity,
    Mixed,
    Contrast,
    NarrowSearch,
}

impl ClipSelection {
    fn uses_contrast(self) -> bool {
        matches!(self, Self::Contrast | Self::NarrowSearch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SelectionRole {
    Activity,
    NarrowTransient,
    BandBurst,
    ShortBandBurst,
    WifiHtSignal,
    WifiHeSuSignal,
}

#[derive(Clone, Copy)]
struct Selection<'a> {
    event_index: usize,
    independent: bool,
    event: &'a Event,
    start: u64,
    end: u64,
    group: u64,
    truncated: bool,
    nearby_packet_start: Option<bool>,
    wifi_overlap_samples: Option<u64>,
    role: SelectionRole,
}

impl Selection<'_> {
    fn overlaps_wifi(&self) -> bool {
        self.wifi_overlap_samples
            .is_some_and(|samples| samples != 0)
    }

    fn probe_interval(&self, center_hz: u64) -> Option<(Range<u64>, u64)> {
        if self.truncated || !self.event.is_narrow_transient() {
            return None;
        }
        let interval =
            (self.event.start_sample - self.start)..(self.event.end_sample_exclusive - self.start);
        let midpoint = self.event.lower_frequency_hz
            + (self.event.upper_frequency_hz_exclusive - self.event.lower_frequency_hz) / 2;
        (interval.start >= PROBE_CONTEXT_SAMPLES
            && interval.end + PROBE_CONTEXT_SAMPLES <= self.end - self.start
            && center_hz.abs_diff(midpoint) <= PROBE_MAX_OFFSET_HZ)
            .then_some((interval, midpoint))
    }
}

struct EventLists<'a> {
    components: &'a [Event],
    narrow_candidates: &'a [Event],
    band_clips: [Option<BandClip>; 2],
    signal_clips: [Option<SignalClip>; 2],
}

#[derive(Clone, Copy, Default)]
struct FrameContext<'a> {
    packet_starts: Option<&'a [u64]>,
    wifi_spans: Option<&'a [Range<u64>]>,
}

impl FrameContext<'_> {
    fn packet_start_in(self, interval: Range<u64>) -> Option<bool> {
        self.packet_starts.map(|starts| {
            let next = starts.partition_point(|sample| *sample < interval.start);
            starts
                .get(next)
                .is_some_and(|sample| *sample < interval.end)
        })
    }

    fn nearby_packet_start(self, stream_start: u64, clip: &Range<u64>) -> Option<bool> {
        let guard_start = stream_start
            .saturating_add(clip.start)
            .saturating_sub(PROXIMITY_SAMPLES);
        let guard_end = stream_start
            .saturating_add(clip.end)
            .saturating_add(PROXIMITY_SAMPLES);
        self.packet_start_in(guard_start..guard_end)
    }

    fn wifi_overlap_samples(self, interval: Range<u64>) -> Option<u64> {
        self.wifi_spans
            .map(|spans| overlap_samples(spans, interval))
    }
}

fn overlap_samples(spans: &[Range<u64>], event: Range<u64>) -> u64 {
    let first = spans.partition_point(|span| span.end <= event.start);
    spans[first..]
        .iter()
        .take_while(|span| span.start < event.end)
        .map(|span| span.end.min(event.end) - span.start.max(event.start))
        .sum()
}

fn clip_extent(onset: u64, samples: u64) -> Range<u64> {
    let aligned_onset = onset - onset % REPLAY_ALIGNMENT;
    let aligned_end = samples - samples % REPLAY_ALIGNMENT;
    aligned_onset.saturating_sub(PRE_SAMPLES)
        ..aligned_onset.saturating_add(POST_SAMPLES).min(aligned_end)
}

#[derive(Clone, Copy, Serialize)]
struct BandBurst {
    #[serde(flatten)]
    band: Band,
    #[serde(flatten)]
    detection: BurstDetection,
}

#[derive(Clone, Copy)]
struct BandClip {
    index: usize,
    burst: BandBurst,
    start: u64,
    end: u64,
    group: u64,
    nearby_packet_start: Option<bool>,
    wifi_overlap_samples: Option<u64>,
    role: SelectionRole,
}

enum BandBurstSelection {
    Long,
    Short { reserved: [Option<Range<u64>>; 3] },
}

fn select_band_burst(
    band: Band,
    bursts: &[BurstDetection],
    samples: u64,
    stream_start: u64,
    band_lower_hz: u64,
    decoded: FrameContext<'_>,
    selection: BandBurstSelection,
) -> Option<BandClip> {
    let midpoint =
        band.lower_frequency_hz + (band.upper_frequency_hz_exclusive - band.lower_frequency_hz) / 2;
    let group = (midpoint - band_lower_hz) / GROUP_HZ;
    bursts
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(index, detection)| {
            let extent = clip_extent(detection.start_sample, samples);
            let duration = detection.end_sample_exclusive - detection.start_sample;
            let role = match &selection {
                BandBurstSelection::Long if duration >= MIN_BAND_BURST_SAMPLES => {
                    SelectionRole::BandBurst
                }
                BandBurstSelection::Short { reserved }
                    if detection.is_strong_short()
                        && reserved
                            .iter()
                            .flatten()
                            .all(|clip| clip.end <= extent.start || extent.end <= clip.start) =>
                {
                    SelectionRole::ShortBandBurst
                }
                _ => return None,
            };
            if detection.end_sample_exclusive > extent.end {
                return None;
            }
            let anchor = (stream_start + detection.start_sample)
                ..(stream_start + detection.end_sample_exclusive);
            let wifi_overlap_samples = decoded.wifi_overlap_samples(anchor.clone());
            if wifi_overlap_samples.is_some_and(|overlap| overlap != 0)
                || decoded.packet_start_in(anchor) == Some(true)
            {
                return None;
            }
            Some(BandClip {
                index,
                burst: BandBurst { band, detection },
                start: extent.start,
                end: extent.end,
                group,
                nearby_packet_start: decoded.nearby_packet_start(stream_start, &extent),
                wifi_overlap_samples,
                role,
            })
        })
        .min_by(|left, right| {
            right
                .burst
                .detection
                .peak_excess_db
                .total_cmp(&left.burst.detection.peak_excess_db)
                .then_with(|| {
                    left.burst
                        .detection
                        .start_sample
                        .cmp(&right.burst.detection.start_sample)
                })
        })
}

#[derive(Clone, Copy)]
struct SignalClip {
    preamble_stream_sample: u64,
    start: u64,
    end: u64,
    nearby_packet_start: Option<bool>,
}

impl SignalClip {
    fn extent(self) -> Range<u64> {
        self.start..self.end
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WifiSignalFormat {
    Ht,
    HeSu,
}

fn select_signal_clip(
    preamble_starts: &[u64],
    samples: u64,
    stream_start: u64,
    decoded: FrameContext<'_>,
    format: WifiSignalFormat,
    reserved: &[Option<Range<u64>>],
) -> Option<SignalClip> {
    let guard_samples = match format {
        WifiSignalFormat::Ht => HT_PREAMBLE_GUARD_SAMPLES,
        WifiSignalFormat::HeSu => HE_SU_SIGNAL_PREAMBLE_SAMPLES,
    };
    preamble_starts
        .iter()
        .copied()
        .filter_map(|preamble_stream_sample| {
            let preamble = preamble_stream_sample.checked_sub(stream_start)?;
            let extent = clip_extent(preamble, samples);
            let aligned = preamble - preamble % REPLAY_ALIGNMENT;
            let guard = preamble_stream_sample.saturating_sub(guard_samples)
                ..preamble_stream_sample.saturating_add(guard_samples);
            // HE replay needs the full aligned window in the original segment.
            if (format == WifiSignalFormat::HeSu
                && (extent.start != aligned.checked_sub(PRE_SAMPLES)?
                    || preamble.checked_add(HE_SU_SIGNAL_PREAMBLE_SAMPLES)? > extent.end))
                || extent.end.checked_sub(aligned) != Some(POST_SAMPLES)
                || decoded.wifi_overlap_samples(guard) != Some(0)
                || reserved
                    .iter()
                    .flatten()
                    .any(|clip| clip.start < extent.end && extent.start < clip.end)
            {
                return None;
            }
            Some(SignalClip {
                preamble_stream_sample,
                start: extent.start,
                end: extent.end,
                nearby_packet_start: decoded.nearby_packet_start(stream_start, &extent),
            })
        })
        .min_by_key(|clip| clip.preamble_stream_sample)
}

fn select_signal_clips(
    he_su_starts: Option<&[u64]>,
    ht_starts: Option<&[u64]>,
    samples: u64,
    stream_start: u64,
    decoded: FrameContext<'_>,
    long_clip: Option<BandClip>,
    limit: u8,
) -> (Option<SignalClip>, Option<SignalClip>) {
    let available = limit.saturating_sub(u8::from(long_clip.is_some()));
    let mut reserved = [long_clip.map(|clip| clip.start..clip.end), None];
    let he_su = he_su_starts.filter(|_| available > 0).and_then(|starts| {
        select_signal_clip(
            starts,
            samples,
            stream_start,
            decoded,
            WifiSignalFormat::HeSu,
            &reserved,
        )
    });
    reserved[1] = he_su.map(SignalClip::extent);
    let ht = ht_starts
        .filter(|_| available > u8::from(he_su.is_some()))
        .and_then(|starts| {
            select_signal_clip(
                starts,
                samples,
                stream_start,
                decoded,
                WifiSignalFormat::Ht,
                &reserved,
            )
        });
    (he_su, ht)
}

fn select<'a>(
    events: EventLists<'a>,
    samples: u64,
    stream_start: u64,
    band_lower_hz: u64,
    decoded: FrameContext<'_>,
    limit: u8,
    selection: ClipSelection,
) -> Result<Vec<Selection<'a>>> {
    ensure!(
        (1..=MAX_CLIPS).contains(&limit),
        "survey IQ clip limit must be 1 through {MAX_CLIPS}"
    );
    let remaining = usize::from(limit)
        .checked_sub(
            events.band_clips.iter().flatten().count()
                + events.signal_clips.iter().flatten().count(),
        )
        .context("reserved IQ clips exceed the clip limit")?;
    if remaining == 0 {
        return Ok(Vec::new());
    }
    let use_contrast = selection.uses_contrast();
    let independent = if selection == ClipSelection::NarrowSearch {
        events.narrow_candidates
    } else {
        &[]
    };
    let indexed_events = [(false, events.components), (true, independent)]
        .into_iter()
        .flat_map(|(independent, list)| {
            list.iter()
                .enumerate()
                .map(move |(index, event)| (independent, index, event))
        });
    let mut candidates = Vec::with_capacity(events.components.len() + independent.len());
    for (independent, event_index, event) in indexed_events {
        ensure!(
            event.start_sample < event.end_sample_exclusive
                && event.end_sample_exclusive <= samples
                && event.lower_frequency_hz >= band_lower_hz
                && event.lower_frequency_hz < event.upper_frequency_hz_exclusive
                && event.peak_excess_db.is_finite(),
            "invalid validated survey component bounds"
        );
        let extent = clip_extent(event.start_sample, samples);
        let (start, end) = (extent.start, extent.end);
        if start == end
            || events
                .band_clips
                .iter()
                .flatten()
                .any(|clip| clip.start < end && start < clip.end)
            || events
                .signal_clips
                .iter()
                .flatten()
                .any(|clip| clip.start < end && start < clip.end)
        {
            continue;
        }
        let midpoint = event.lower_frequency_hz
            + (event.upper_frequency_hz_exclusive - event.lower_frequency_hz) / 2;
        let group = (midpoint - band_lower_hz) / GROUP_HZ;
        let nearby_packet_start = decoded.nearby_packet_start(stream_start, &extent);
        let wifi_overlap_samples = decoded.wifi_overlap_samples(
            (stream_start + event.start_sample)..(stream_start + event.end_sample_exclusive),
        );
        candidates.push(Selection {
            event_index,
            independent,
            event,
            start,
            end,
            group,
            truncated: event.end_sample_exclusive > end,
            nearby_packet_start,
            wifi_overlap_samples,
            role: SelectionRole::Activity,
        });
    }

    let contrast_rank = |candidate: &Selection<'_>| {
        if use_contrast {
            candidate
                .event
                .narrow_contrast_db
                .unwrap_or(f64::NEG_INFINITY)
        } else {
            0.0
        }
    };
    let mut selected: Vec<Selection<'_>> = Vec::with_capacity(remaining);
    if selection != ClipSelection::Activity
        && let Some(mut narrow) = candidates
            .iter()
            .filter(|candidate| {
                !candidate.truncated
                    && candidate.event.is_narrow_transient()
                    && (!use_contrast || contrast_rank(candidate) >= NARROW_CONTRAST_THRESHOLD_DB)
            })
            .min_by(|a, b| {
                a.overlaps_wifi()
                    .cmp(&b.overlaps_wifi())
                    .then_with(|| a.nearby_packet_start.cmp(&b.nearby_packet_start))
                    .then_with(|| contrast_rank(b).total_cmp(&contrast_rank(a)))
                    .then_with(|| b.event.peak_excess_db.total_cmp(&a.event.peak_excess_db))
                    .then_with(|| a.independent.cmp(&b.independent))
                    .then_with(|| a.event_index.cmp(&b.event_index))
            })
            .copied()
    {
        narrow.role = SelectionRole::NarrowTransient;
        selected.push(narrow);
    }

    candidates.retain(|candidate| !candidate.independent);
    candidates.sort_unstable_by(|a, b| {
        a.overlaps_wifi()
            .cmp(&b.overlaps_wifi())
            .then_with(|| a.nearby_packet_start.cmp(&b.nearby_packet_start))
            .then_with(|| a.truncated.cmp(&b.truncated))
            .then_with(|| b.event.hit_tile_count.cmp(&a.event.hit_tile_count))
            .then_with(|| b.event.peak_excess_db.total_cmp(&a.event.peak_excess_db))
            .then_with(|| a.event.start_sample.cmp(&b.event.start_sample))
            .then_with(|| a.event.lower_frequency_hz.cmp(&b.event.lower_frequency_hz))
            .then_with(|| a.event_index.cmp(&b.event_index))
    });

    for prefer_unused_group in [true, false] {
        for candidate in &candidates {
            if selected.len() == remaining {
                break;
            }
            if prefer_unused_group
                && events
                    .band_clips
                    .iter()
                    .flatten()
                    .any(|clip| clip.group == candidate.group)
            {
                continue;
            }
            if selected.iter().any(|prior| {
                (prior.start < candidate.end && candidate.start < prior.end)
                    || (prefer_unused_group && prior.group == candidate.group)
            }) {
                continue;
            }
            selected.push(*candidate);
        }
    }
    selected.sort_unstable_by_key(|clip| clip.start);
    Ok(selected)
}

#[derive(Serialize)]
#[serde(untagged)]
enum ClipAnchor<'a> {
    Survey {
        #[serde(skip_serializing_if = "Option::is_none")]
        event_index: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        narrow_candidate_index: Option<usize>,
        event: &'a Event,
    },
    BandBurst {
        band_burst_index: usize,
        band_burst: BandBurst,
    },
    WifiHtSignal {
        ht_preamble_stream_sample: u64,
    },
    WifiHeSuSignal {
        he_su_preamble_stream_sample: u64,
    },
}

enum PlannedClip<'a> {
    Survey(Selection<'a>),
    BandBurst(BandClip),
    WifiHtSignal(SignalClip),
    WifiHeSuSignal(SignalClip),
}

impl PlannedClip<'_> {
    fn extent(&self) -> (u64, u64) {
        match self {
            Self::Survey(clip) => (clip.start, clip.end),
            Self::BandBurst(clip) => (clip.start, clip.end),
            Self::WifiHtSignal(clip) | Self::WifiHeSuSignal(clip) => (clip.start, clip.end),
        }
    }

    fn probe_interval(&self, center_hz: u64) -> Option<(Range<u64>, u64)> {
        match self {
            Self::Survey(clip) => clip.probe_interval(center_hz),
            Self::BandBurst(_) | Self::WifiHtSignal(_) | Self::WifiHeSuSignal(_) => None,
        }
    }
}

#[derive(Serialize)]
struct Clip<'a> {
    iq_file: String,
    sidecar_file: String,
    #[serde(flatten)]
    anchor: ClipAnchor<'a>,
    start_sample: u64,
    end_sample_exclusive: u64,
    stream_start_sample: u64,
    first_sample_ns: i64,
    end_sample_exclusive_ns: i64,
    event_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    nearby_validated_packet_start: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_overlap_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_role: Option<SelectionRole>,
    iq_sha256: String,
    sidecar_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_probe: Option<ClipProbeReport>,
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    operation: &'static str,
    source_index: usize,
    source_iq_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    source_receipt_sha256: &'a str,
    source_start_sample: u64,
    source_samples: u64,
    source_first_sample_ns: i64,
    source_end_sample_exclusive_ns: i64,
    source_witness_start_ns: i64,
    event_report_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_receipt_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_burst_report_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_ht_signal_report_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_he_su_signal_report_sha256: Option<&'a str>,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    clip_limit: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    selection: Option<ClipSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_contrast_threshold_db: Option<f64>,
    pre_samples: u64,
    post_samples_from_event_start: u64,
    packet_proximity_samples: u64,
    frequency_group_hz: u64,
    iq_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_event_clips: Option<bool>,
    clips: &'a [Clip<'a>],
}

struct ClipFile {
    path: PathBuf,
    sha256: String,
    stamp: Metadata,
}

impl ClipFile {
    fn verify(&self) -> Result<()> {
        let (digest, stamp) =
            report::hash_direct_regular_file_with_snapshot(&self.path, self.stamp.len())?;
        ensure!(
            digest.sha256 == self.sha256 && report::same_file_identity(&stamp, &self.stamp),
            "retained survey clip evidence changed: {}",
            self.path.display()
        );
        Ok(())
    }
}

#[derive(Serialize)]
pub(super) struct ClipReport {
    #[serde(rename = "receipt_sha256")]
    receipt: EvidenceFile,
    clip_count: usize,
    iq_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_ht_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_he_su_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform_probes: Option<ProbeCounts>,
    #[serde(skip)]
    files: Vec<ClipFile>,
    #[serde(skip)]
    he_su_file_pair: Option<usize>,
}

#[derive(Clone, Copy, Default, Serialize)]
pub(super) struct ProbeCounts {
    measured_clips: usize,
    recovered_clips: usize,
    recovered_symbols: usize,
}

impl ProbeCounts {
    pub(super) fn total<'a>(reports: impl Iterator<Item = &'a ClipReport>) -> Self {
        let mut total = Self::default();
        for counts in reports.filter_map(|report| report.waveform_probes) {
            total.measured_clips += counts.measured_clips;
            total.recovered_clips += counts.recovered_clips;
            total.recovered_symbols += counts.recovered_symbols;
        }
        total
    }
}

pub(super) struct RetainOptions<'a> {
    pub(super) limit: u8,
    pub(super) selection: ClipSelection,
    pub(super) probe_program: Option<&'a ProgramProvenance>,
    pub(super) wifi_spans: Option<&'a [Range<u64>]>,
    pub(super) band_bursts: Option<&'a BurstReport>,
    pub(super) retain_long_band_burst: bool,
    pub(super) retain_short_band_burst: bool,
    pub(super) ht_signal: Option<HtSignalContext<'a>>,
    pub(super) he_su_signal: Option<HeSuSignalContext<'a>>,
}

/// Verified HT-SIG preamble positions on the stream sample grid.
pub(super) struct HtSignalContext<'a> {
    pub(super) preamble_starts: &'a [u64],
    pub(super) report_sha256: &'a str,
}

/// Verified HE20 SU preamble positions on the stream sample grid.
pub(super) struct HeSuSignalContext<'a> {
    pub(super) preamble_starts: &'a [u64],
    pub(super) report_sha256: &'a str,
}

impl ClipReport {
    pub(super) fn receipt_sha256(&self) -> &str {
        &self.receipt.sha256
    }

    pub(super) fn count(&self) -> usize {
        self.clip_count
    }

    pub(super) fn bytes(&self) -> u64 {
        self.iq_bytes
    }

    pub(super) fn band_burst_clip_count(&self) -> Option<u64> {
        self.band_burst_clips
    }

    pub(super) fn short_band_burst_clip_count(&self) -> Option<u64> {
        self.short_band_burst_clips
    }

    pub(super) fn wifi_ht_signal_clip_count(&self) -> Option<u64> {
        self.wifi_ht_signal_clips
    }

    pub(super) fn wifi_he_su_signal_clip_count(&self) -> Option<u64> {
        self.wifi_he_su_signal_clips
    }

    /// Return the published HE SU IQ and sidecar paths in receipt order.
    pub(super) fn he_su_clip_files(&self) -> Option<(&Path, &Path)> {
        self.he_su_file_pair.map(|index| {
            (
                self.files[index].path.as_path(),
                self.files[index + 1].path.as_path(),
            )
        })
    }

    pub(super) fn verify(&self) -> Result<()> {
        self.receipt.verify()?;
        self.files.iter().try_for_each(ClipFile::verify)
    }
}

fn write_file(stage: &Path, destination: PathBuf, bytes: &[u8]) -> Result<ClipFile> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(stage)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(ClipFile {
        path: destination,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        stamp: file.metadata()?,
    })
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "survey IQ clip retention interrupted"
    );
    Ok(())
}

/// Preserve selected clips before any raw IQ is released.
pub(super) fn retain(
    segment: &ValidatedSegment,
    detection: &Detection,
    direct: Option<&DirectReport>,
    options: RetainOptions<'_>,
    report_dir: &Path,
    stop: &AtomicBool,
) -> Result<ClipReport> {
    check_stop(stop)?;
    let RetainOptions {
        limit,
        selection,
        probe_program,
        wifi_spans,
        band_bursts,
        retain_long_band_burst,
        retain_short_band_burst,
        ht_signal,
        he_su_signal,
    } = options;
    ensure!(
        !selection.uses_contrast() || detection.narrow_contrast_method.is_some(),
        "contrast clip selection requires rf-observatory with narrow event contrast support"
    );
    ensure!(
        selection != ClipSelection::NarrowSearch || detection.narrow_search.is_some(),
        "narrow-search clip selection requires rf-observatory with independent narrow search support"
    );
    ensure!(
        ht_signal.is_none() || wifi_spans.is_some(),
        "HT-SIG clip selection requires verified Wi-Fi frame spans"
    );
    ensure!(
        he_su_signal.is_none() || wifi_spans.is_some(),
        "HE SU clip selection requires verified Wi-Fi frame spans"
    );
    ensure!(
        band_bursts.is_some() || (!retain_long_band_burst && !retain_short_band_burst),
        "band burst clip selection requires a verified burst report"
    );
    let original = &segment.receipt;
    let source = &segment.source;
    let packet_starts = direct.map(|report| report.packet_starts(original.start_sample));
    let band_lower_hz = source
        .center_frequency_hz
        .saturating_sub(source.bandwidth_hz / 2);
    let decoded = FrameContext {
        packet_starts: packet_starts.as_deref(),
        wifi_spans,
    };
    let long_clip = band_bursts
        .filter(|_| retain_long_band_burst)
        .and_then(|report| {
            select_band_burst(
                report.band(),
                report.detections(),
                original.num_samples,
                original.start_sample,
                band_lower_hz,
                decoded,
                BandBurstSelection::Long,
            )
        });
    let (he_su_clip, ht_clip) = select_signal_clips(
        he_su_signal.as_ref().map(|context| context.preamble_starts),
        ht_signal.as_ref().map(|context| context.preamble_starts),
        original.num_samples,
        original.start_sample,
        decoded,
        long_clip,
        limit,
    );
    let reserved_count = u8::from(long_clip.is_some())
        + u8::from(he_su_clip.is_some())
        + u8::from(ht_clip.is_some());
    let short_clip = band_bursts
        .filter(|_| retain_short_band_burst && limit > reserved_count)
        .and_then(|report| {
            select_band_burst(
                report.band(),
                report.detections(),
                original.num_samples,
                original.start_sample,
                band_lower_hz,
                decoded,
                BandBurstSelection::Short {
                    reserved: [
                        long_clip.map(|clip| clip.start..clip.end),
                        he_su_clip.map(SignalClip::extent),
                        ht_clip.map(SignalClip::extent),
                    ],
                },
            )
        });
    let band_clips = [long_clip, short_clip];
    let selected = select(
        EventLists {
            components: &detection.events,
            narrow_candidates: detection
                .narrow_search
                .as_ref()
                .map_or(&[], |search| search.candidates.as_slice()),
            band_clips,
            signal_clips: [he_su_clip, ht_clip],
        },
        original.num_samples,
        original.start_sample,
        band_lower_hz,
        decoded,
        limit,
        selection,
    )?;
    let mut selected: Vec<_> = selected
        .into_iter()
        .map(PlannedClip::Survey)
        .chain(band_clips.into_iter().flatten().map(PlannedClip::BandBurst))
        .chain(he_su_clip.map(PlannedClip::WifiHeSuSignal))
        .chain(ht_clip.map(PlannedClip::WifiHtSignal))
        .collect();
    selected.sort_unstable_by_key(|clip| clip.extent().0);
    let recorded_selection = (selection != ClipSelection::Activity).then_some(selection);
    let input = open_admitted_iq(segment)?;
    let destination = report_dir.join(CLIP_DIR);
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(report_dir)?;
    let stage = tempfile::Builder::new()
        .prefix(".clips-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(report_dir)?;
    let mut records = Vec::with_capacity(selected.len());
    let mut files = Vec::with_capacity(2 * selected.len());
    let mut he_su_file_pair = None;
    let mut iq_bytes = 0;
    let mut waveform_probes = probe_program.map(|_| ProbeCounts::default());
    let mut bytes = Vec::new();
    for (index, clip) in selected.into_iter().enumerate() {
        check_stop(stop)?;
        let (start, end) = clip.extent();
        let count = end - start;
        let extent = count * SAMPLE_BYTES;
        bytes.resize(usize::try_from(extent)?, 0);
        input.read_exact_at(&mut bytes, start * SAMPLE_BYTES)?;
        let iq_file = format!("clip-{:02}.sc16q11", index + 1);
        let sidecar_file = format!("{iq_file}.json");
        let iq = write_file(
            &stage.path().join(&iq_file),
            destination.join(&iq_file),
            &bytes,
        )?;
        let stream_start_sample = original
            .start_sample
            .checked_add(start)
            .context("survey IQ clip sample offset overflow")?;
        let first_sample_ns = original
            .first_sample_ns
            .checked_add(i64::try_from(start)? * SAMPLE_PERIOD_NS)
            .context("survey IQ clip start clock overflow")?;
        let end_sample_exclusive_ns = original
            .first_sample_ns
            .checked_add(i64::try_from(end)? * SAMPLE_PERIOD_NS)
            .context("survey IQ clip end clock overflow")?;
        let sidecar = serde_json::to_vec_pretty(&serde_json::json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": source.center_frequency_hz,
            "sample_rate_hz": source.sample_rate_hz,
            "bandwidth_hz": source.bandwidth_hz,
            "num_samples": count,
            "source_kind": "derived_rolling_survey_clip",
            "source_start_sample": start,
            "stream_start_sample": stream_start_sample,
            "continuity_guaranteed": false,
        }))?;
        let metadata = write_file(
            &stage.path().join(&sidecar_file),
            destination.join(&sidecar_file),
            &sidecar,
        )?;
        let waveform_probe = if let (Some(program), Some(counts)) =
            (probe_program, &mut waveform_probes)
            && let Some((interval, target_hz)) = clip.probe_interval(source.center_frequency_hz)
        {
            let report_file = format!("clip-{:02}.waveform.json", index + 1);
            let sampled = clip_probe::probe(
                program,
                ProbeRequest {
                    iq: &stage.path().join(&iq_file),
                    expected_sha256: &iq.sha256,
                    source_samples: count,
                    center_frequency_hz: source.center_frequency_hz,
                    target_frequency_hz: target_hz,
                    interval,
                    report_path: &stage.path().join(&report_file),
                    report_dir: stage.path(),
                },
                stop,
            )
            .with_context(|| format!("measure waveform in {iq_file}"))?;
            files.push(ClipFile {
                path: destination.join(report_file),
                sha256: sampled.report.report_sha256.clone(),
                stamp: sampled.stamp,
            });
            counts.measured_clips += 1;
            if let Some(symbols) = sampled.report.recovered_symbol_count {
                counts.recovered_clips += 1;
                counts.recovered_symbols += symbols;
            }
            Some(sampled.report)
        } else {
            None
        };
        let (anchor, truncated, nearby_packet_start, wifi_overlap_samples, selection_role) =
            match clip {
                PlannedClip::Survey(clip) => (
                    ClipAnchor::Survey {
                        event_index: (!clip.independent).then_some(clip.event_index),
                        narrow_candidate_index: clip.independent.then_some(clip.event_index),
                        event: clip.event,
                    },
                    clip.truncated,
                    clip.nearby_packet_start,
                    clip.wifi_overlap_samples,
                    recorded_selection.map(|_| clip.role),
                ),
                PlannedClip::BandBurst(clip) => (
                    ClipAnchor::BandBurst {
                        band_burst_index: clip.index,
                        band_burst: clip.burst,
                    },
                    false,
                    clip.nearby_packet_start,
                    clip.wifi_overlap_samples,
                    Some(clip.role),
                ),
                PlannedClip::WifiHtSignal(clip) => (
                    ClipAnchor::WifiHtSignal {
                        ht_preamble_stream_sample: clip.preamble_stream_sample,
                    },
                    false,
                    clip.nearby_packet_start,
                    Some(0),
                    Some(SelectionRole::WifiHtSignal),
                ),
                PlannedClip::WifiHeSuSignal(clip) => {
                    he_su_file_pair = Some(files.len());
                    (
                        ClipAnchor::WifiHeSuSignal {
                            he_su_preamble_stream_sample: clip.preamble_stream_sample,
                        },
                        false,
                        clip.nearby_packet_start,
                        Some(0),
                        Some(SelectionRole::WifiHeSuSignal),
                    )
                }
            };
        iq_bytes += extent;
        records.push(Clip {
            iq_file,
            sidecar_file,
            anchor,
            start_sample: start,
            end_sample_exclusive: end,
            stream_start_sample,
            first_sample_ns,
            end_sample_exclusive_ns,
            event_truncated: truncated,
            nearby_validated_packet_start: nearby_packet_start,
            wifi_overlap_samples,
            selection_role,
            iq_sha256: iq.sha256.clone(),
            sidecar_sha256: metadata.sha256.clone(),
            waveform_probe,
        });
        files.extend([iq, metadata]);
    }

    segment.verify_unchanged()?;
    check_stop(stop)?;
    let receipt = Receipt {
        schema_version: if he_su_signal.is_some() {
            7
        } else if retain_short_band_burst {
            6
        } else if ht_signal.is_some() {
            5
        } else if band_bursts.is_some() {
            4
        } else if probe_program.is_some() {
            3
        } else if selection == ClipSelection::NarrowSearch {
            2
        } else {
            1
        },
        operation: "rolling_survey_iq_clips",
        source_index: original.index,
        source_iq_sha256: &source.sha256,
        source_sidecar_sha256: &source.sidecar_sha256,
        source_receipt_sha256: &segment.receipt_sha256,
        source_start_sample: original.start_sample,
        source_samples: original.num_samples,
        source_first_sample_ns: original.first_sample_ns,
        source_end_sample_exclusive_ns: original.end_sample_exclusive_ns,
        source_witness_start_ns: original.witness_start_ns,
        event_report_sha256: &detection.event_report_sha256,
        direct_receipt_sha256: direct.map(DirectReport::receipt_sha256),
        band_burst_report_sha256: band_bursts.map(BurstReport::sha256),
        wifi_ht_signal_report_sha256: ht_signal.as_ref().map(|context| context.report_sha256),
        wifi_he_su_signal_report_sha256: he_su_signal.as_ref().map(|context| context.report_sha256),
        center_frequency_hz: source.center_frequency_hz,
        sample_rate_hz: source.sample_rate_hz,
        clip_limit: limit,
        selection: recorded_selection,
        narrow_contrast_threshold_db: selection
            .uses_contrast()
            .then_some(NARROW_CONTRAST_THRESHOLD_DB),
        pre_samples: PRE_SAMPLES,
        post_samples_from_event_start: POST_SAMPLES,
        packet_proximity_samples: PROXIMITY_SAMPLES,
        frequency_group_hz: GROUP_HZ,
        iq_bytes,
        probe_event_clips: probe_program.is_some().then_some(true),
        clips: &records,
    };
    report::publish_json(stage.path(), RECEIPT_NAME, &receipt)?;
    check_stop(stop)?;
    let name = stage
        .path()
        .file_name()
        .context("survey clip stage lacks a name")?;
    renameat2(
        &parent,
        Path::new(name),
        &parent,
        Path::new(CLIP_DIR),
        RenameFlags::RENAME_NOREPLACE,
    )
    .context("publish survey IQ clips without replacing output")?;
    let _ = stage.keep();
    parent.sync_all()?;
    let segments_dir = report_dir
        .parent()
        .context("survey clip report has no parent")?;
    fs::File::open(segments_dir)?.sync_all()?;
    let (published, proof) =
        EvidenceFile::read_json(destination.join(RECEIPT_NAME), RECEIPT_BYTES)?;
    ensure!(
        published == serde_json::to_value(&receipt)?,
        "published survey IQ clip receipt differs from admitted evidence"
    );
    Ok(ClipReport {
        receipt: proof,
        clip_count: records.len(),
        iq_bytes,
        band_burst_clips: retain_long_band_burst.then_some(u64::from(long_clip.is_some())),
        short_band_burst_clips: retain_short_band_burst.then_some(u64::from(short_clip.is_some())),
        wifi_ht_signal_clips: ht_signal.map(|_| u64::from(ht_clip.is_some())),
        wifi_he_su_signal_clips: he_su_signal.map(|_| u64::from(he_su_clip.is_some())),
        waveform_probes,
        files,
        he_su_file_pair,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rolling::detection::WINDOW_SAMPLES as SURVEY_WINDOW_SAMPLES;

    fn select<'a>(
        events: &'a [Event],
        samples: u64,
        stream_start: u64,
        band_lower_hz: u64,
        packet_starts: Option<&[u64]>,
        limit: u8,
        selection: ClipSelection,
    ) -> Result<Vec<Selection<'a>>> {
        super::select(
            EventLists {
                components: events,
                narrow_candidates: &[],
                band_clips: [None, None],
                signal_clips: [None, None],
            },
            samples,
            stream_start,
            band_lower_hz,
            FrameContext {
                packet_starts,
                ..FrameContext::default()
            },
            limit,
            selection,
        )
    }

    fn event(start: u64, end: u64, group: u64, hits: u64) -> Event {
        let lower = 2_427_000_000 + group * GROUP_HZ;
        Event {
            start_sample: start,
            end_sample_exclusive: end,
            lower_frequency_hz: lower,
            upper_frequency_hz_exclusive: lower + 100_000,
            hit_tile_count: hits,
            peak_excess_db: 12.0,
            narrow_contrast_db: None,
        }
    }

    fn band_burst(start: u64, end: u64, peak: f64) -> BurstDetection {
        BurstDetection {
            start_sample: start,
            end_sample_exclusive: end,
            peak_excess_db: peak,
        }
    }

    #[test]
    fn selects_the_strongest_complete_band_burst_without_known_packet_activity() {
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let bursts = [
            band_burst(20_480, 22_528, 60.0),
            band_burst(40_960, 49_152, 50.0),
            band_burst(81_920, 86_016, 40.0),
            band_burst(128_000, 133_120, 20.0),
            band_burst(512_000, 520_192, 20.0),
            band_burst(716_800, 997_376, 30.0),
        ];
        let start = 30_000_000;
        let wifi = [
            (start + 44_000)..(start + 45_000),
            (start + 127_000)..(start + 128_000),
        ];
        let packet_starts = [start + 81_920, start + 133_120];
        let clip = select_band_burst(
            band,
            &bursts,
            1_000_000,
            start,
            2_427_000_000,
            FrameContext {
                packet_starts: Some(&packet_starts),
                wifi_spans: Some(&wifi),
            },
            BandBurstSelection::Long,
        )
        .unwrap();
        assert_eq!(clip.index, 3);
        assert_eq!(clip.wifi_overlap_samples, Some(0));
        assert_eq!((clip.start, clip.end), (87_040, 332_800));
        let anchor = serde_json::to_value(ClipAnchor::BandBurst {
            band_burst_index: clip.index,
            band_burst: clip.burst,
        })
        .unwrap();
        assert_eq!(
            anchor,
            serde_json::json!({
                "band_burst_index": 3,
                "band_burst": {
                    "lower_frequency_hz": 2444000000u64,
                    "upper_frequency_hz_exclusive": 2446000000u64,
                    "start_sample": 128000,
                    "end_sample_exclusive": 133120,
                    "peak_excess_db": 20.0
                }
            })
        );
        assert!(
            PlannedClip::BandBurst(clip)
                .probe_interval(2_437_000_000)
                .is_none()
        );
        assert!(
            select_band_burst(
                band,
                &bursts[..3],
                1_000_000,
                start,
                2_427_000_000,
                FrameContext {
                    packet_starts: Some(&packet_starts),
                    wifi_spans: Some(&wifi),
                },
                BandBurstSelection::Long,
            )
            .is_none()
        );
    }

    #[test]
    fn reserved_band_crop_counts_against_the_limit_and_prevents_overlapping_survey_clips() {
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let reserved = select_band_burst(
            band,
            &[band_burst(102_400, 106_496, 12.0)],
            1_000_000,
            0,
            2_427_000_000,
            FrameContext::default(),
            BandBurstSelection::Long,
        )
        .unwrap();
        let mut overlapping = event(200_000, 200_000 + 2 * SURVEY_WINDOW_SAMPLES, 1, 100);
        overlapping.narrow_contrast_db = Some(5.0);
        let disjoint = event(600_000, 640_000, 2, 10);
        let ordinary = [event(220_000, 230_000, 1, 150), disjoint];
        let independent = [overlapping];
        let clips = |limit, reserved| {
            super::select(
                EventLists {
                    components: &ordinary,
                    narrow_candidates: &independent,
                    band_clips: [reserved, None],
                    signal_clips: [None, None],
                },
                1_000_000,
                0,
                2_427_000_000,
                FrameContext::default(),
                limit,
                ClipSelection::NarrowSearch,
            )
            .unwrap()
        };
        assert!(clips(1, Some(reserved)).is_empty());
        let selected = clips(2, Some(reserved));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].start, 559_040);
        let fallback = clips(1, None);
        assert_eq!(fallback.len(), 1);
        assert!(fallback[0].independent);
    }

    #[test]
    fn ht_clip_uses_the_earliest_eligible_preamble_in_the_original_segment() {
        let stream_start = 30_000_000;
        let samples = 1_500_000;
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let band_clip = select_band_burst(
            band,
            &[band_burst(300_000, 305_000, 12.0)],
            samples,
            stream_start,
            2_427_000_000,
            FrameContext::default(),
            BandBurstSelection::Long,
        );
        let valid_start = stream_start + 90_200;
        let frame = valid_start..valid_start + 1200;
        let preambles = [
            stream_start + 900_000,
            stream_start - 5,
            stream_start + 301_003,
            stream_start + 90_000,
            stream_start + 600_003,
            stream_start + 1_450_000,
        ];
        let decoded = FrameContext {
            wifi_spans: Some(std::slice::from_ref(&frame)),
            ..FrameContext::default()
        };
        let select = |starts| {
            select_signal_clip(
                starts,
                samples,
                stream_start,
                decoded,
                WifiSignalFormat::Ht,
                &[band_clip.map(|clip| clip.start..clip.end)],
            )
        };
        let clip = select(&preambles).unwrap();
        assert_eq!(clip.preamble_stream_sample, stream_start + 600_003);
        assert_eq!((clip.start, clip.end), (559_040, 804_800));
        assert!(select(&preambles[1..4]).is_none());
        assert!(select(&preambles[5..]).is_none());
        let anchor = serde_json::to_value(ClipAnchor::WifiHtSignal {
            ht_preamble_stream_sample: clip.preamble_stream_sample,
        })
        .unwrap();
        assert_eq!(
            anchor,
            serde_json::json!({"ht_preamble_stream_sample": stream_start + 600_003})
        );
    }

    #[test]
    fn reserved_ht_clip_counts_against_the_limit_and_displaces_overlapping_survey_clips() {
        let samples = 900_000;
        let clip = select_signal_clip(
            &[100_000],
            samples,
            0,
            FrameContext {
                wifi_spans: Some(&[]),
                ..FrameContext::default()
            },
            WifiSignalFormat::Ht,
            &[],
        )
        .unwrap();
        let events = [
            event(110_000, 120_000, 1, 900),
            event(500_000, 510_000, 2, 9),
        ];
        let clips = |limit| {
            super::select(
                EventLists {
                    components: &events,
                    narrow_candidates: &[],
                    band_clips: [None, None],
                    signal_clips: [None, Some(clip)],
                },
                samples,
                0,
                2_427_000_000,
                FrameContext::default(),
                limit,
                ClipSelection::Activity,
            )
            .unwrap()
        };
        assert!(clips(1).is_empty());
        let selected = clips(2);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].event_index, 1);
    }

    #[test]
    fn he_su_clip_requires_complete_aligned_context_and_clear_signal_fields() {
        let stream_start = 30_000_000;
        let samples = 800_000;
        let frame = (stream_start + 100_500)..(stream_start + 101_000);
        let decoded = FrameContext {
            wifi_spans: Some(std::slice::from_ref(&frame)),
            ..FrameContext::default()
        };
        let eligible = stream_start + 300_003;
        let starts = [
            stream_start - 1,
            stream_start + PRE_SAMPLES - 1,
            stream_start + 100_003,
            eligible,
            eligible,
            stream_start + samples - HE_SU_SIGNAL_PREAMBLE_SAMPLES,
        ];
        let select = |reserved: &[Option<Range<u64>>]| {
            select_signal_clip(
                &starts,
                samples,
                stream_start,
                decoded,
                WifiSignalFormat::HeSu,
                reserved,
            )
        };
        let clip = select(&[]).unwrap();
        assert_eq!(clip.preamble_stream_sample, eligible);
        assert_eq!(clip.extent(), 259_040..504_800);
        assert!(select(&[Some(clip.extent())]).is_none());
        assert_eq!(
            serde_json::to_value(ClipAnchor::WifiHeSuSignal {
                he_su_preamble_stream_sample: eligible,
            })
            .unwrap(),
            serde_json::json!({"he_su_preamble_stream_sample": eligible})
        );
    }

    #[test]
    fn he_su_and_ht_share_remaining_slots_after_long_band_clip() {
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let samples = 1_500_000;
        let decoded = FrameContext {
            wifi_spans: Some(&[]),
            ..FrameContext::default()
        };
        let long = select_band_burst(
            band,
            &[band_burst(102_400, 106_496, 12.0)],
            samples,
            0,
            2_427_000_000,
            decoded,
            BandBurstSelection::Long,
        )
        .unwrap();
        let select = |limit| {
            let (he_su, ht) = select_signal_clips(
                Some(&[200_000, 580_003]),
                Some(&[580_003, 850_003]),
                samples,
                0,
                decoded,
                Some(long),
                limit,
            );
            (
                he_su.map(|clip| clip.preamble_stream_sample),
                ht.map(|clip| clip.preamble_stream_sample),
            )
        };
        assert_eq!(select(1), (None, None));
        assert_eq!(select(2), (Some(580_003), None));
        assert_eq!(select(3), (Some(580_003), Some(850_003)));
    }

    #[test]
    fn short_band_clip_requires_the_complete_burst_without_validated_overlap() {
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let stream_start = 30_000_000;
        let bursts = [
            band_burst(20_480, 24_576, 90.0),
            band_burst(40_960, 43_008, 9.99),
            band_burst(61_440, 64_512, 80.0),
            band_burst(81_920, 83_968, 70.0),
            band_burst(98_304, 100_352, 10.0),
            band_burst(102_400, 105_472, 12.0),
        ];
        let wifi = (stream_start + 64_511)..(stream_start + 65_536);
        let packet_starts = [stream_start + 81_920, stream_start + 105_472];
        let select = |samples| {
            select_band_burst(
                band,
                &bursts,
                samples,
                stream_start,
                2_427_000_000,
                FrameContext {
                    packet_starts: Some(&packet_starts),
                    wifi_spans: Some(std::slice::from_ref(&wifi)),
                },
                BandBurstSelection::Short {
                    reserved: [None, None, None],
                },
            )
            .unwrap()
        };
        let clip = select(105_480);
        assert_eq!(clip.index, 5);
        assert_eq!(clip.end, 105_480);
        assert_eq!(clip.wifi_overlap_samples, Some(0));
        assert_eq!(serde_json::to_value(clip.role).unwrap(), "short_band_burst");
        assert_eq!(select(105_471).index, 4);
    }

    #[test]
    fn short_band_clip_uses_an_extra_slot_and_displaces_overlapping_survey_clips() {
        let band: Band = "2444000000:2446000000".parse().unwrap();
        let short = select_band_burst(
            band,
            &[
                band_burst(102_400, 104_448, 60.0),
                band_burst(358_400, 360_448, 50.0),
                band_burst(614_400, 617_472, 20.0),
            ],
            1_000_000,
            0,
            2_427_000_000,
            FrameContext::default(),
            BandBurstSelection::Short {
                reserved: [Some(0..240_000), Some(300_000..573_440), None],
            },
        )
        .unwrap();
        assert_eq!(short.index, 2);
        assert_eq!(short.start, 573_440);
        let events = [
            event(650_000, 652_000, 2, 900),
            event(920_000, 922_000, 3, 9),
        ];
        let clips = |limit| {
            super::select(
                EventLists {
                    components: &events,
                    narrow_candidates: &[],
                    band_clips: [None, Some(short)],
                    signal_clips: [None, None],
                },
                1_000_000,
                0,
                2_427_000_000,
                FrameContext::default(),
                limit,
                ClipSelection::Activity,
            )
            .unwrap()
        };
        assert!(clips(1).is_empty());
        let selected = clips(2);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].event_index, 1);
    }

    #[test]
    fn waveform_probe_requires_saved_context_and_frequency_margin() {
        let duration = 2 * SURVEY_WINDOW_SAMPLES;
        for (start, group, source_samples, eligible) in [
            (2_000, 1, 250_000, true),
            (399, 1, 250_000, false),
            (2_000, 1, 2_000 + duration + 399, false),
            (2_000, 0, 250_000, false),
        ] {
            let events = [event(start, start + duration, group, 2)];
            let clips = select(
                &events,
                source_samples,
                0,
                2_427_000_000,
                None,
                1,
                ClipSelection::Mixed,
            )
            .unwrap();
            let interval = clips[0].probe_interval(2_437_000_000);
            assert_eq!(interval.is_some(), eligible);
            if let Some((range, midpoint)) = interval {
                assert_eq!(range, start..start + duration);
                assert_eq!(midpoint, 2_429_050_000);
            }
        }
    }

    #[test]
    fn selects_quiet_proximity_and_distinct_frequencies_before_filling() {
        let events = [
            event(100_000, 110_000, 1, 100),
            event(700_000, 710_000, 1, 50),
            event(1_100_000, 1_110_000, 1, 40),
            event(1_800_000, 1_810_000, 2, 30),
        ];
        let selected = select(
            &events,
            2_100_000,
            0,
            2_427_000_000,
            Some(&[120_000]),
            2,
            ClipSelection::Activity,
        )
        .unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|clip| clip.event_index)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        let selected = select(
            &events,
            2_100_000,
            0,
            2_427_000_000,
            Some(&[120_000]),
            4,
            ClipSelection::Activity,
        )
        .unwrap();
        assert_eq!(selected.len(), 4);
        assert_eq!(selected[0].nearby_packet_start, Some(true));
    }

    #[test]
    fn clips_clamp_to_grid_and_prefer_complete_nonoverlapping_components() {
        let events = [
            event(3, 400_000, 1, 1000),
            event(30_003, 40_000, 1, 50),
            event(440_003, 500_000, 2, 5),
        ];
        let selected = select(
            &events,
            500_001,
            0,
            2_427_000_000,
            None,
            4,
            ClipSelection::Activity,
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].event_index, 1);
        assert_eq!(selected[0].start, 0);
        assert_eq!(selected[1].end, 500_000);
        assert!(selected.iter().all(|clip| {
            clip.nearby_packet_start.is_none()
                && clip.start.is_multiple_of(REPLAY_ALIGNMENT)
                && clip.end.is_multiple_of(REPLAY_ALIGNMENT)
                && clip.end - clip.start <= PRE_SAMPLES + POST_SAMPLES
        }));
    }

    fn window_event(start: u64, windows: u64, group: u64) -> Event {
        event(
            start * SURVEY_WINDOW_SAMPLES,
            (start + windows) * SURVEY_WINDOW_SAMPLES,
            group,
            windows,
        )
    }

    #[test]
    fn mixed_reserves_one_transient_then_fills_with_nonoverlapping_frequency_diversity() {
        let events = [
            window_event(20, 40, 1),
            window_event(35, 2, 2),
            window_event(120, 30, 2),
            window_event(200, 20, 3),
        ];
        let selected = select(
            &events,
            300 * SURVEY_WINDOW_SAMPLES,
            0,
            2_427_000_000,
            None,
            3,
            ClipSelection::Mixed,
        )
        .unwrap();
        let roles: Vec<_> = selected
            .iter()
            .map(|clip| (clip.event_index, clip.role))
            .collect();
        assert_eq!(
            roles,
            [
                (1, SelectionRole::NarrowTransient),
                (2, SelectionRole::Activity),
                (3, SelectionRole::Activity),
            ]
        );
        let selected = select(
            &events,
            300 * SURVEY_WINDOW_SAMPLES,
            0,
            2_427_000_000,
            None,
            2,
            ClipSelection::Mixed,
        )
        .unwrap();
        assert_eq!(selected[1].event_index, 3);
    }

    #[test]
    fn mixed_ranks_transients_by_proximity_then_peak_then_event_index() {
        let mut events = [
            window_event(20, 2, 1),
            window_event(360, 5, 2),
            window_event(240, 10, 3),
            window_event(120, 2, 4),
        ];
        events[0].peak_excess_db = 40.0;
        events[2].peak_excess_db = 20.0;
        events[2].upper_frequency_hz_exclusive = events[2].lower_frequency_hz + 400_000;
        events[3].peak_excess_db = 20.0;
        let stream_start = 100_000;
        let packet_starts = [stream_start + events[0].start_sample];
        let select_one = |events: &[Event]| {
            let selected = select(
                events,
                500 * SURVEY_WINDOW_SAMPLES,
                stream_start,
                2_427_000_000,
                Some(&packet_starts),
                1,
                ClipSelection::Mixed,
            )
            .unwrap()[0];
            (
                selected.event_index,
                selected.role,
                selected.nearby_packet_start,
            )
        };
        assert_eq!(
            select_one(&events),
            (2, SelectionRole::NarrowTransient, Some(false))
        );
        assert_eq!(
            select_one(&events[..1]),
            (0, SelectionRole::NarrowTransient, Some(true))
        );
    }

    #[test]
    fn mixed_falls_back_to_activity_when_no_complete_transient_qualifies() {
        let mut events = [
            window_event(20, 2, 1),
            window_event(120, 1, 2),
            window_event(240, 11, 3),
            window_event(399, 2, 4),
        ];
        events[0].upper_frequency_hz_exclusive = events[0].lower_frequency_hz + 500_000;
        let select_events = |policy| {
            select(
                &events,
                401 * SURVEY_WINDOW_SAMPLES,
                0,
                2_427_000_000,
                None,
                4,
                policy,
            )
            .unwrap()
            .iter()
            .map(|clip| (clip.event_index, clip.role, clip.truncated))
            .collect::<Vec<_>>()
        };
        let mixed = select_events(ClipSelection::Mixed);
        assert_eq!(mixed.len(), 4);
        assert_eq!(mixed[3], (3, SelectionRole::Activity, true));
        assert_eq!(mixed, select_events(ClipSelection::Activity));
    }

    #[test]
    fn contrast_reserves_a_measured_local_rise_before_activity_fill() {
        let mut events = [
            window_event(20, 2, 1),
            window_event(90, 2, 2),
            window_event(160, 2, 3),
            window_event(230, 3, 4),
            window_event(300, 2, 5),
        ];
        events[0].narrow_contrast_db = Some(2.99);
        events[0].peak_excess_db = 30.0;
        events[1].narrow_contrast_db = Some(3.0);
        events[2].narrow_contrast_db = Some(4.0);
        events[4].narrow_contrast_db = Some(8.0);
        let packet_starts = [events[4].start_sample];
        let select_events = |mode| {
            select(
                &events,
                400 * SURVEY_WINDOW_SAMPLES,
                0,
                2_427_000_000,
                Some(&packet_starts),
                2,
                mode,
            )
            .unwrap()
            .iter()
            .map(|clip| (clip.event_index, clip.role))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            select_events(ClipSelection::Contrast),
            [
                (2, SelectionRole::NarrowTransient),
                (3, SelectionRole::Activity),
            ]
        );
        assert_eq!(
            select_events(ClipSelection::Mixed)[0],
            (0, SelectionRole::NarrowTransient)
        );
    }

    #[test]
    fn contrast_fills_with_activity_when_qualification_is_missing_or_below_threshold() {
        let mut events = [window_event(20, 2, 1), window_event(90, 2, 2)];
        events[0].narrow_contrast_db = Some(2.99);
        let selected = select(
            &events,
            150 * SURVEY_WINDOW_SAMPLES,
            0,
            2_427_000_000,
            None,
            2,
            ClipSelection::Contrast,
        )
        .unwrap();
        assert_eq!(selected.len(), 2);
        assert!(
            selected
                .iter()
                .all(|clip| clip.role == SelectionRole::Activity)
        );
    }

    #[test]
    fn narrow_search_anchors_to_an_independent_event_inside_broad_activity() {
        let mut broad = window_event(20, 20, 1);
        broad.upper_frequency_hz_exclusive += GROUP_HZ;
        let components = [broad, window_event(120, 12, 2)];
        let mut narrow = window_event(25, 3, 1);
        narrow.narrow_contrast_db = Some(5.0);
        let independent = [narrow];
        let select_events = |selection| {
            super::select(
                EventLists {
                    components: &components,
                    narrow_candidates: &independent,
                    band_clips: [None, None],
                    signal_clips: [None, None],
                },
                200 * SURVEY_WINDOW_SAMPLES,
                0,
                2_427_000_000,
                FrameContext::default(),
                2,
                selection,
            )
            .unwrap()
        };
        let selected = select_events(ClipSelection::NarrowSearch);
        assert_eq!(selected.len(), 2);
        assert!(selected[0].independent);
        assert_eq!(selected[0].role, SelectionRole::NarrowTransient);
        assert_eq!(selected[0].event_index, 0);
        assert!(!selected[1].independent);
        assert_eq!(selected[1].event_index, 1);
        assert!(
            select_events(ClipSelection::Contrast)
                .iter()
                .all(|clip| !clip.independent)
        );
    }

    #[test]
    fn frame_context_prefers_a_disjoint_anchor_and_counts_half_open_overlap() {
        let mut events = [window_event(20, 2, 1), window_event(90, 2, 2)];
        events[0].peak_excess_db = 25.0;
        let offset = 123_400;
        let first = offset + events[0].start_sample;
        let second = offset + events[1].start_sample;
        let decoded = [first - 100..first + 500, second - 200..second];
        let clips = super::select(
            EventLists {
                components: &events,
                narrow_candidates: &[],
                band_clips: [None, None],
                signal_clips: [None, None],
            },
            140 * SURVEY_WINDOW_SAMPLES,
            offset,
            2_427_000_000,
            FrameContext {
                packet_starts: None,
                wifi_spans: Some(&decoded),
            },
            1,
            ClipSelection::Mixed,
        )
        .unwrap();
        assert_eq!(overlap_samples(&decoded, first..first + 600), 500);
        assert_eq!(clips[0].event_index, 1);
        assert_eq!(clips[0].wifi_overlap_samples, Some(0));
        let legacy = select(
            &events,
            140 * SURVEY_WINDOW_SAMPLES,
            offset,
            2_427_000_000,
            None,
            1,
            ClipSelection::Mixed,
        )
        .unwrap();
        assert_eq!(legacy[0].event_index, 0);
        assert_eq!(legacy[0].wifi_overlap_samples, None);
    }
}
