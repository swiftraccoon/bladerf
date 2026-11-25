//! Validate the saved source, capture metadata and dense event-report contract.

use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

use super::plan::Span;
use crate::spectrum::{EventBand, EventStride, FFT};
use crate::{MAX_SAMPLES, SAMPLE_RATE, file_pin};

const LEGACY_EVENT_REPORT_LIMIT: u64 = 2 * 1024 * 1024;
const SURVEY_EVENT_REPORT_LIMIT: u64 = 4 * 1024 * 1024;
const ACQUISITION_LIMIT: u64 = 64 * 1024;
const EVENT_LIMIT: u64 = 4096;
const WINDOW_SAMPLES: u64 = FFT as u64;
const CELL_WIDTH_HZ: u64 = 100_000;
const CENTER_GUARD_HZ: u64 = 100_000;

#[derive(Debug)]
pub(super) struct Input {
    pub(super) iq: File,
    pub(super) source_metadata: Metadata,
    pub(super) center_frequency_hz: u64,
    pub(super) input_samples: u64,
    pub(super) analyzed_samples: u64,
    pub(super) event_stride: EventStride,
    pub(super) analyzed_prefix_sha256: String,
    pub(super) event_report_sha256: String,
    pub(super) acquisition_sha256: String,
    pub(super) replay_spans: Vec<Span>,
    pub(super) event_count: usize,
    pub(super) active_window_count: Option<u64>,
    pub(super) omitted_component_count: u64,
}

#[derive(Deserialize)]
struct Acquisition {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: u16,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    num_samples: u64,
}

#[derive(Deserialize)]
struct EventReport {
    schema_version: u32,
    operation: String,
    source_format: String,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    input_samples: u64,
    analyzed_samples: u64,
    analyzed_prefix_sha256: String,
    window_samples: u64,
    #[serde(default, deserialize_with = "deserialize_present")]
    window_stride_samples: Option<u64>,
    cell_width_hz: u64,
    usable_half_span_hz: u64,
    center_guard_hz: u64,
    minimum_excess_over_temporal_median_db: f64,
    event_limit: u64,
    connectivity: String,
    windows: u64,
    eligible_cells: u64,
    evaluated_tile_count: u64,
    hit_tile_count: u64,
    active_window_count: u64,
    #[serde(default, deserialize_with = "deserialize_present")]
    active_window_ranges: Option<Vec<[u64; 2]>>,
    component_count: u64,
    omitted_component_count: u64,
    omitted_hit_tile_count: u64,
    events: Vec<Event>,
}

#[derive(Deserialize)]
struct Event {
    start_sample: u64,
    end_sample_exclusive: u64,
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    peak_excess_db: f64,
    hit_tile_count: u64,
}

fn deserialize_present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn windows_in_span(samples: u64, stride_samples: u64) -> Option<u64> {
    let last_start = samples.checked_sub(WINDOW_SAMPLES)?;
    last_start
        .is_multiple_of(stride_samples)
        .then_some(1 + last_start / stride_samples)
}

pub(super) fn same_extent_and_times(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn read_json<T: DeserializeOwned>(path: &Path, limit: u64) -> Result<(T, String, u64)> {
    let (mut file, length) = file_pin::open_regular(path, limit)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == length && length <= limit && same_extent_and_times(&before, &after),
        "JSON input changed while it was read: {}",
        path.display()
    );
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse JSON input {}", path.display()))?;
    Ok((value, format!("{:x}", Sha256::digest(&bytes)), length))
}

impl EventReport {
    fn validate(&self, input_samples: u64) -> Result<(EventStride, Vec<Span>)> {
        ensure!(
            matches!(self.schema_version, 1..=3)
                && self.operation == "offline_rf_spectrum_events"
                && self.source_format == "SC16_Q11_LE"
                && self.sample_rate_hz == SAMPLE_RATE,
            "expected a version 1, 2 or 3 dense SC16_Q11_LE event report at 20 MSps"
        );
        let contiguous_stride = EventStride::Contiguous.samples() as u64;
        let half_window_stride = EventStride::HalfWindow.samples() as u64;
        let event_stride = match (self.schema_version, self.window_stride_samples) {
            (1, None) => EventStride::Contiguous,
            (3, Some(stride)) if stride == contiguous_stride => EventStride::Contiguous,
            (2 | 3, Some(stride)) if stride == half_window_stride => EventStride::HalfWindow,
            _ => bail!(
                "expected window_stride_samples omitted in version 1, {half_window_stride} in version 2, and {contiguous_stride} or {half_window_stride} in version 3"
            ),
        };
        let active_ranges = match (self.schema_version, self.active_window_ranges.as_deref()) {
            (1 | 2, None) => None,
            (3, Some(ranges)) => Some(ranges),
            _ => bail!(
                "active_window_ranges is required in version 3 and must be omitted in versions 1 and 2"
            ),
        };
        let stride_samples = event_stride.samples() as u64;
        ensure!(
            self.input_samples == input_samples
                && input_samples <= MAX_SAMPLES
                && self.analyzed_samples <= input_samples
                && windows_in_span(self.analyzed_samples, stride_samples) == Some(self.windows),
            "event report sample extent differs from the source or complete window grid"
        );
        let band = EventBand::from_half_span_hz(self.usable_half_span_hz)
            .context("unsupported event report frequency half-span")?;
        let guard_cells = 2 * CENTER_GUARD_HZ / CELL_WIDTH_HZ;
        ensure!(
            (70_000_000..=6_000_000_000).contains(&self.center_frequency_hz)
                && self.window_samples == WINDOW_SAMPLES
                && self.cell_width_hz == CELL_WIDTH_HZ
                && self.center_guard_hz == CENTER_GUARD_HZ
                && self.eligible_cells == band.cell_count() as u64 - guard_cells
                && self.connectivity == "orthogonal_neighbors_in_sample_window_and_frequency_cell"
                && self.minimum_excess_over_temporal_median_db.is_finite()
                && self.minimum_excess_over_temporal_median_db >= 0.0,
            "invalid event report grid or center frequency"
        );
        ensure!(
            self.analyzed_prefix_sha256.len() == 64
                && self
                    .analyzed_prefix_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid analyzed-prefix SHA-256"
        );
        ensure!(
            (1..=EVENT_LIMIT).contains(&self.event_limit)
                && self.events.len() as u64 == self.component_count.min(self.event_limit)
                && self.component_count.checked_sub(self.events.len() as u64)
                    == Some(self.omitted_component_count)
                && self.component_count <= self.hit_tile_count
                && self.omitted_component_count <= self.omitted_hit_tile_count
                && (self.omitted_component_count == 0) == (self.omitted_hit_tile_count == 0)
                && self.evaluated_tile_count == self.windows * self.eligible_cells
                && self.active_window_count <= self.windows.min(self.hit_tile_count)
                && self.hit_tile_count <= self.active_window_count * self.eligible_cells,
            "inconsistent event report accounting"
        );

        let mut spans = Vec::with_capacity(self.events.len());
        let mut retained_hits = 0;
        for event in &self.events {
            spans.push(self.validate_event(event, band, stride_samples)?);
            retained_hits += event.hit_tile_count;
        }
        ensure!(
            self.hit_tile_count.checked_sub(retained_hits) == Some(self.omitted_hit_tile_count),
            "retained and omitted event hits differ from the hit tile count"
        );
        let replay_spans = match active_ranges {
            Some(ranges) => self.validate_active_ranges(ranges, &mut spans, stride_samples)?,
            None => spans,
        };
        Ok((event_stride, replay_spans))
    }

    fn validate_event(&self, event: &Event, band: EventBand, stride_samples: u64) -> Result<Span> {
        let time_windows = event
            .end_sample_exclusive
            .checked_sub(event.start_sample)
            .and_then(|span_samples| windows_in_span(span_samples, stride_samples))
            .filter(|_| {
                event.end_sample_exclusive <= self.analyzed_samples
                    && event.start_sample.is_multiple_of(stride_samples)
            })
            .context("event sample bounds are outside the analyzed window grid")?;
        let band_start = self.center_frequency_hz - band.half_span_hz();
        ensure!(
            band_start <= event.lower_frequency_hz
                && event.lower_frequency_hz < event.upper_frequency_hz_exclusive
                && event.upper_frequency_hz_exclusive
                    <= self.center_frequency_hz + band.half_span_hz()
                && (event.lower_frequency_hz - band_start).is_multiple_of(CELL_WIDTH_HZ)
                && (event.upper_frequency_hz_exclusive - band_start).is_multiple_of(CELL_WIDTH_HZ)
                && (event.upper_frequency_hz_exclusive
                    <= self.center_frequency_hz - CENTER_GUARD_HZ
                    || event.lower_frequency_hz >= self.center_frequency_hz + CENTER_GUARD_HZ),
            "event frequency bounds are outside the eligible frequency grid"
        );
        let span = Span {
            start_sample: event.start_sample,
            end_sample_exclusive: event.end_sample_exclusive,
        };
        let frequency_cells =
            (event.upper_frequency_hz_exclusive - event.lower_frequency_hz) / CELL_WIDTH_HZ;
        ensure!(
            (1..=time_windows * frequency_cells).contains(&event.hit_tile_count)
                && event.peak_excess_db.is_finite()
                && event.peak_excess_db >= self.minimum_excess_over_temporal_median_db,
            "event hit count or peak power is inconsistent with its bounds"
        );
        Ok(span)
    }

    fn validate_active_ranges(
        &self,
        ranges: &[[u64; 2]],
        retained_spans: &mut [Span],
        stride_samples: u64,
    ) -> Result<Vec<Span>> {
        ensure!(
            ranges.len() as u64 <= self.windows.div_ceil(2).min(self.component_count),
            "active window range count exceeds the window grid or component count"
        );
        let mut previous_end = None;
        let mut active_rows = 0;
        for &[start, end] in ranges {
            ensure!(
                start < end && end <= self.windows && previous_end.is_none_or(|last| last < start),
                "active window ranges must be ordered maximal runs inside the window grid"
            );
            active_rows += end - start;
            previous_end = Some(end);
        }
        ensure!(
            active_rows == self.active_window_count,
            "active window range lengths differ from the active window count"
        );

        retained_spans.sort_unstable_by_key(|span| span.start_sample);
        let mut retained_rows = 0;
        let mut retained_end = 0;
        for span in retained_spans {
            let start = span.start_sample / stride_samples;
            let end = 1 + (span.end_sample_exclusive - WINDOW_SAMPLES) / stride_samples;
            let run = ranges.partition_point(|range| range[1] <= start);
            ensure!(
                ranges
                    .get(run)
                    .is_some_and(|&[run_start, run_end]| run_start <= start && end <= run_end),
                "retained event sample bounds extend outside the active window ranges"
            );
            // Frequency components can share rows, so count only the newly covered suffix.
            retained_rows += end.saturating_sub(start.max(retained_end));
            retained_end = retained_end.max(end);
        }
        ensure!(
            active_rows - retained_rows <= self.omitted_hit_tile_count,
            "active windows outside retained event bounds exceed the omitted hit tile count"
        );

        Ok(ranges
            .iter()
            .map(|&[start, end]| Span {
                start_sample: start * stride_samples,
                end_sample_exclusive: (end - 1) * stride_samples + WINDOW_SAMPLES,
            })
            .collect())
    }
}

pub(super) fn open(iq_path: &Path, events_path: &Path) -> Result<Input> {
    let (iq, iq_bytes) = file_pin::open_regular(iq_path, MAX_SAMPLES * 4)?;
    let source_metadata = iq.metadata()?;
    ensure!(
        source_metadata.len() == iq_bytes && iq_bytes.is_multiple_of(4),
        "expected a stable extent of complete SC16_Q11 samples"
    );
    let input_samples = iq_bytes / 4;
    let (events, event_report_sha256, report_bytes) =
        read_json::<EventReport>(events_path, SURVEY_EVENT_REPORT_LIMIT)?;
    ensure!(
        events.schema_version == 3 || report_bytes <= LEGACY_EVENT_REPORT_LIMIT,
        "legacy event report exceeds the {LEGACY_EVENT_REPORT_LIMIT}-byte limit"
    );
    let (event_stride, replay_spans) = events.validate(input_samples)?;
    let mut sidecar = iq_path.as_os_str().to_owned();
    sidecar.push(".json");
    let (acquisition, acquisition_sha256, _) =
        read_json::<Acquisition>(Path::new(&sidecar), ACQUISITION_LIMIT)?;
    ensure!(
        acquisition.format == "SC16_Q11"
            && acquisition.dtype == "<i2"
            && acquisition.interleave == "I,Q"
            && acquisition.complex_scale == 2048
            && acquisition.num_samples == input_samples
            && acquisition.frequency_hz == events.center_frequency_hz as f64
            && acquisition.sample_rate_hz == f64::from(SAMPLE_RATE)
            && acquisition.bandwidth_hz == f64::from(SAMPLE_RATE),
        "acquisition encoding, extent or tuning differs from the dense event source"
    );
    Ok(Input {
        iq,
        source_metadata,
        center_frequency_hz: events.center_frequency_hz,
        input_samples,
        analyzed_samples: events.analyzed_samples,
        event_stride,
        analyzed_prefix_sha256: events.analyzed_prefix_sha256,
        event_report_sha256,
        acquisition_sha256,
        replay_spans,
        event_count: events.events.len(),
        active_window_count: (events.schema_version == 3).then_some(events.active_window_count),
        omitted_component_count: events.omitted_component_count,
    })
}

#[cfg(test)]
mod tests;
