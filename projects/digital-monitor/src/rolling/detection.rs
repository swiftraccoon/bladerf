//! Supervise the dense survey and bind its summary to one complete RX segment.

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{process, report};

const SEGMENT_SAMPLES: u64 = 19_988_480;
const SAMPLE_RATE_HZ: u64 = 20_000_000;
pub(super) const WINDOW_SAMPLES: u64 = 8192;
const CELL_WIDTH_HZ: u64 = 100_000;
const CENTER_GUARD_HZ: u64 = 100_000;
const NARROW_MAX_WIDTH_HZ: u64 = 400_000;
const NARROW_WINDOWS: RangeInclusive<u64> = 2..=10;
const NARROW_CANDIDATE_LIMIT: u64 = 128;
const ACTIVITY_THRESHOLD_DB: f64 = 6.0;
pub(super) const NARROW_CONTRAST_THRESHOLD_DB: f64 = 3.0;
const ELIGIBLE_CELLS: u64 = 198;
const EVENT_LIMIT: u64 = 4096;
const EVENT_REPORT_BYTES: u64 = 4 * 1024 * 1024;
const LOG_BYTES: u64 = 1024 * 1024;
const SURVEY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum NarrowContrastMethod {
    MeanTargetExcessMinusStrongerAdjacentBand,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NarrowSearchMethod {
    FixedBandRuns,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct NarrowSearch {
    method: NarrowSearchMethod,
    candidate_limit: u64,
    candidate_count: u64,
    #[serde(skip_serializing)]
    pub(super) candidates: Vec<Event>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct WifiActivityWindows {
    pub(super) active_windows: u64,
    pub(super) overlapping_validated_wifi: u64,
}

impl WifiActivityWindows {
    pub(super) fn total(counts: impl Iterator<Item = Self>) -> Option<Self> {
        counts.reduce(|total, next| Self {
            active_windows: total.active_windows + next.active_windows,
            overlapping_validated_wifi: total.overlapping_validated_wifi
                + next.overlapping_validated_wifi,
        })
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Detection {
    pub(super) event_report_sha256: String,
    pub(super) windows: u64,
    pub(super) active_window_count: u64,
    pub(super) active_window_range_count: u64,
    pub(super) hit_tile_count: u64,
    pub(super) component_count: u64,
    pub(super) retained_component_count: u64,
    pub(super) omitted_component_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) narrow_contrast_method: Option<NarrowContrastMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) narrow_search: Option<NarrowSearch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_activity_windows: Option<WifiActivityWindows>,
    #[serde(skip)]
    pub(super) active_window_ranges: Vec<[u64; 2]>,
    #[serde(skip)]
    pub(super) events: Vec<Event>,
}

impl Detection {
    /// Consume the survey ranges using the optional sorted union of validated
    /// Wi-Fi frame spans on the stream sample grid.
    pub(super) fn detect_wifi_activity(
        &mut self,
        physical_start_sample: u64,
        spans: Option<&[Range<u64>]>,
    ) {
        let ranges = std::mem::take(&mut self.active_window_ranges);
        self.wifi_activity_windows = spans.map(|mut spans| {
            let overlapping_validated_wifi = ranges
                .into_iter()
                .flat_map(|[first, last]| first..last)
                .filter(|&window| {
                    let start = window * WINDOW_SAMPLES;
                    while spans
                        .first()
                        .is_some_and(|span| span.end.saturating_sub(physical_start_sample) <= start)
                    {
                        spans = &spans[1..];
                    }
                    spans.first().is_some_and(|span| {
                        span.start.saturating_sub(physical_start_sample) < start + WINDOW_SAMPLES
                    })
                })
                .count() as u64;
            WifiActivityWindows {
                active_windows: self.active_window_count,
                overlapping_validated_wifi,
            }
        });
    }
}

#[derive(Deserialize)]
struct EventReport {
    schema_version: u32,
    operation: String,
    source_format: String,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    input_samples: u64,
    analyzed_samples: u64,
    analyzed_prefix_sha256: String,
    window_samples: u64,
    window_stride_samples: u64,
    windows: u64,
    cell_width_hz: u64,
    usable_half_span_hz: u64,
    center_guard_hz: u64,
    eligible_cells: u64,
    evaluated_tile_count: u64,
    hit_tile_count: u64,
    active_window_count: u64,
    active_window_ranges: Vec<[u64; 2]>,
    event_limit: u64,
    component_count: u64,
    omitted_component_count: u64,
    omitted_hit_tile_count: u64,
    narrow_contrast_method: Option<NarrowContrastMethod>,
    narrow_search: Option<NarrowSearch>,
    events: Vec<Event>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Event {
    pub(super) start_sample: u64,
    pub(super) end_sample_exclusive: u64,
    pub(super) lower_frequency_hz: u64,
    pub(super) upper_frequency_hz_exclusive: u64,
    pub(super) peak_excess_db: f64,
    pub(super) hit_tile_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) narrow_contrast_db: Option<f64>,
}

impl Event {
    pub(super) fn is_narrow_transient(&self) -> bool {
        let width = self.upper_frequency_hz_exclusive - self.lower_frequency_hz;
        let duration = self.end_sample_exclusive - self.start_sample;
        width <= NARROW_MAX_WIDTH_HZ
            && duration.is_multiple_of(WINDOW_SAMPLES)
            && NARROW_WINDOWS.contains(&(duration / WINDOW_SAMPLES))
    }
}

impl EventReport {
    fn verify_event(&self, event: &Event, lower_edge: u64, upper_edge: u64) -> Result<(u64, u64)> {
        ensure!(
            event.start_sample < event.end_sample_exclusive
                && event.end_sample_exclusive <= SEGMENT_SAMPLES
                && event.start_sample.is_multiple_of(WINDOW_SAMPLES)
                && event.end_sample_exclusive.is_multiple_of(WINDOW_SAMPLES),
            "event interval lies outside the contiguous window grid"
        );
        let guard_lower = self.center_frequency_hz - CENTER_GUARD_HZ;
        let guard_upper = self.center_frequency_hz + CENTER_GUARD_HZ;
        ensure!(
            lower_edge <= event.lower_frequency_hz
                && event.lower_frequency_hz < event.upper_frequency_hz_exclusive
                && event.upper_frequency_hz_exclusive <= upper_edge
                && (event.lower_frequency_hz - lower_edge).is_multiple_of(CELL_WIDTH_HZ)
                && (event.upper_frequency_hz_exclusive - lower_edge).is_multiple_of(CELL_WIDTH_HZ)
                && (event.upper_frequency_hz_exclusive <= guard_lower
                    || event.lower_frequency_hz >= guard_upper)
                && event.peak_excess_db.is_finite(),
            "event frequency bounds or peak differ from the eligible survey grid"
        );
        let width = event.upper_frequency_hz_exclusive - event.lower_frequency_hz;
        let expect_contrast = self.narrow_contrast_method.is_some()
            && event.is_narrow_transient()
            && event.lower_frequency_hz - lower_edge >= width
            && upper_edge - event.upper_frequency_hz_exclusive >= width
            && (guard_lower
                .checked_sub(event.upper_frequency_hz_exclusive)
                .is_some_and(|margin| margin >= width)
                || event
                    .lower_frequency_hz
                    .checked_sub(guard_upper)
                    .is_some_and(|margin| margin >= width));
        ensure!(
            event.narrow_contrast_db.is_some() == expect_contrast
                && event.narrow_contrast_db.is_none_or(f64::is_finite),
            "event contrast score differs from the declared method or eligible survey grid"
        );
        let cells = width / CELL_WIDTH_HZ;
        let start = event.start_sample / WINDOW_SAMPLES;
        let end = event.end_sample_exclusive / WINDOW_SAMPLES;
        let range = self
            .active_window_ranges
            .partition_point(|range| range[1] <= start);
        ensure!(
            self.active_window_ranges
                .get(range)
                .is_some_and(|&[first, last]| first <= start && end <= last)
                && (end - start..=(end - start) * cells.min(ELIGIBLE_CELLS))
                    .contains(&event.hit_tile_count),
            "event hit count or interval differs from the active window and frequency bounds"
        );
        Ok((start, end))
    }

    fn verify_narrow_search(
        &self,
        search: &NarrowSearch,
        lower_edge: u64,
        upper_edge: u64,
    ) -> Result<()> {
        // A target band and both flanks must fit on one side of the center guard.
        let possible_bands = (1..=NARROW_MAX_WIDTH_HZ / CELL_WIDTH_HZ)
            .map(|width| 2 * (ELIGIBLE_CELLS / 2 + 1 - 3 * width))
            .sum::<u64>();
        let runs_per_band = (self.active_window_count / 2).min((self.windows + 1) / 3);
        ensure!(
            self.narrow_contrast_method.is_some()
                && search.candidate_limit == NARROW_CANDIDATE_LIMIT
                && search.candidate_count <= possible_bands * runs_per_band
                && search.candidates.len() as u64
                    == search.candidate_count.min(search.candidate_limit),
            "independent narrow search has inconsistent method or candidate counts"
        );
        let mut previous_position = None;
        let mut last_end_by_band = BTreeMap::new();
        for event in &search.candidates {
            let (start, end) = self.verify_event(event, lower_edge, upper_edge)?;
            let band = (event.lower_frequency_hz, event.upper_frequency_hz_exclusive);
            let position = (start, band);
            ensure!(
                event.peak_excess_db >= ACTIVITY_THRESHOLD_DB
                    && event
                        .narrow_contrast_db
                        .is_some_and(|score| score >= NARROW_CONTRAST_THRESHOLD_DB)
                    && event.hit_tile_count <= self.hit_tile_count
                    && previous_position.is_none_or(|previous| previous < position)
                    && last_end_by_band
                        .insert(band, end)
                        .is_none_or(|previous| previous < start),
                "invalid independent narrow candidate threshold, order or fixed-band run"
            );
            previous_position = Some(position);
        }
        Ok(())
    }

    fn verify(
        self,
        center_frequency_hz: u64,
        iq_sha256: &str,
        event_report_sha256: String,
    ) -> Result<Detection> {
        ensure!(
            self.schema_version == 3
                && self.operation == "offline_rf_spectrum_events"
                && self.source_format == "SC16_Q11_LE"
                && self.center_frequency_hz == center_frequency_hz
                && self.sample_rate_hz == SAMPLE_RATE_HZ
                && self.input_samples == SEGMENT_SAMPLES
                && self.analyzed_samples == SEGMENT_SAMPLES
                && self.analyzed_prefix_sha256 == iq_sha256,
            "dense event report does not identify the complete rolling RX segment"
        );
        ensure!(
            self.window_samples == WINDOW_SAMPLES
                && self.window_stride_samples == WINDOW_SAMPLES
                && self.windows == SEGMENT_SAMPLES / WINDOW_SAMPLES
                && self.cell_width_hz == CELL_WIDTH_HZ
                && self.usable_half_span_hz == SAMPLE_RATE_HZ / 2
                && self.center_guard_hz == CENTER_GUARD_HZ
                && self.eligible_cells == ELIGIBLE_CELLS
                && self.evaluated_tile_count == self.windows * ELIGIBLE_CELLS,
            "dense event report differs from the full-band contiguous window grid"
        );
        let lower_edge = center_frequency_hz
            .checked_sub(self.usable_half_span_hz)
            .context("dense event band lower edge underflow")?;
        let upper_edge = center_frequency_hz
            .checked_add(self.usable_half_span_hz)
            .context("dense event band upper edge overflow")?;

        let range_count = self.active_window_ranges.len() as u64;
        let retained_count = self.events.len() as u64;
        ensure!(
            self.active_window_count <= self.windows.min(self.hit_tile_count)
                && self.hit_tile_count <= self.active_window_count * ELIGIBLE_CELLS
                && range_count <= self.component_count
                && self.component_count <= self.hit_tile_count
                && self.event_limit == EVENT_LIMIT
                && retained_count == self.component_count.min(EVENT_LIMIT)
                && self.component_count.checked_sub(retained_count)
                    == Some(self.omitted_component_count)
                && self.omitted_component_count <= self.omitted_hit_tile_count
                && (self.omitted_component_count == 0) == (self.omitted_hit_tile_count == 0),
            "dense event report has inconsistent component or tile counts"
        );

        let mut active_windows = 0;
        let mut previous_end = None;
        for &[start, end] in &self.active_window_ranges {
            ensure!(
                start < end && end <= self.windows && previous_end.is_none_or(|last| last < start),
                "active window intervals must be ordered, maximal runs inside the window grid"
            );
            active_windows += end - start;
            previous_end = Some(end);
        }
        ensure!(
            active_windows == self.active_window_count,
            "active window intervals differ from the reported active window count"
        );

        let mut retained_hits = 0;
        let mut retained_intervals = Vec::with_capacity(self.events.len());
        for event in &self.events {
            retained_intervals.push(self.verify_event(event, lower_edge, upper_edge)?);
            retained_hits += event.hit_tile_count;
        }
        ensure!(
            self.hit_tile_count.checked_sub(retained_hits) == Some(self.omitted_hit_tile_count),
            "retained and omitted event hits differ from the reported tile count"
        );
        retained_intervals.sort_unstable();
        let mut retained_windows = 0;
        let mut retained_end = 0;
        for (start, end) in retained_intervals {
            retained_windows += end.saturating_sub(start.max(retained_end));
            retained_end = retained_end.max(end);
        }
        ensure!(
            active_windows - retained_windows <= self.omitted_hit_tile_count,
            "active windows outside retained event intervals exceed omitted hit tiles"
        );
        if let Some(search) = &self.narrow_search {
            self.verify_narrow_search(search, lower_edge, upper_edge)?;
        }

        Ok(Detection {
            event_report_sha256,
            windows: self.windows,
            active_window_count: self.active_window_count,
            active_window_range_count: range_count,
            hit_tile_count: self.hit_tile_count,
            component_count: self.component_count,
            retained_component_count: retained_count,
            omitted_component_count: self.omitted_component_count,
            narrow_contrast_method: self.narrow_contrast_method,
            narrow_search: self.narrow_search,
            wifi_activity_windows: None,
            active_window_ranges: self.active_window_ranges,
            events: self.events,
        })
    }
}

/// Analyze one complete witnessed segment into an existing private result directory.
pub(super) fn detect(
    program: &process::ProgramProvenance,
    input: &Path,
    output: &Path,
    center_frequency_hz: u64,
    num_samples: u64,
    iq_sha256: &str,
    stop: &AtomicBool,
) -> Result<Detection> {
    ensure!(
        num_samples == SEGMENT_SAMPLES && center_frequency_hz.is_multiple_of(1_000_000),
        "rolling detection needs a complete segment and a whole-MHz center"
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "rolling detection interrupted"
    );
    report::private_directory(&output.join("logs"))?;
    let survey = output.join("survey");
    let arguments = [
        "survey".into(),
        input.as_os_str().to_owned(),
        "--center-mhz".into(),
        (center_frequency_hz / 1_000_000).to_string().into(),
        "--max-samples".into(),
        num_samples.to_string().into(),
        "--dense".into(),
        "--events".into(),
        "--include-band-edges".into(),
        "--active-window-ranges".into(),
        "--output".into(),
        survey.as_os_str().to_owned(),
    ];
    let command = process::run(
        program,
        &arguments,
        output,
        "survey",
        LOG_BYTES,
        SURVEY_TIMEOUT,
        stop,
    )?;
    process::require_success(&command).context("rolling dense survey")?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "rolling dense survey exceeded its log bound"
    );
    ensure!(
        process::resolve_program(program.name, &program.path)? == *program,
        "RF observatory executable changed during rolling detection"
    );
    let (value, event_report_sha256) =
        report::read_json_with_sha256_limit(&survey.join("events.json"), EVENT_REPORT_BYTES)?;
    let events: EventReport = serde_json::from_value(value).context("parse dense event report")?;
    events.verify(center_frequency_hz, iq_sha256, event_report_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CENTER: u64 = 2_437_000_000;

    fn report_fixture(digest: &str) -> serde_json::Value {
        let center = CENTER;
        json!({
            "schema_version": 3,
            "operation": "offline_rf_spectrum_events",
            "source_format": "SC16_Q11_LE",
            "center_frequency_hz": center,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "input_samples": SEGMENT_SAMPLES,
            "analyzed_samples": SEGMENT_SAMPLES,
            "analyzed_prefix_sha256": digest,
            "window_samples": WINDOW_SAMPLES,
            "window_stride_samples": WINDOW_SAMPLES,
            "windows": 2440,
            "cell_width_hz": 100_000,
            "usable_half_span_hz": 10_000_000,
            "center_guard_hz": 100_000,
            "eligible_cells": ELIGIBLE_CELLS,
            "evaluated_tile_count": 2440 * ELIGIBLE_CELLS,
            "hit_tile_count": 2,
            "active_window_count": 2,
            "active_window_ranges": [[2, 4]],
            "event_limit": EVENT_LIMIT,
            "component_count": 1,
            "omitted_component_count": 0,
            "omitted_hit_tile_count": 0,
            "events": [{
                "start_sample": 16384,
                "end_sample_exclusive": 32768,
                "lower_frequency_hz": center + CELL_WIDTH_HZ,
                "upper_frequency_hz_exclusive": center + 2 * CELL_WIDTH_HZ,
                "peak_excess_db": 7.0,
                "hit_tile_count": 2
            }],
        })
    }

    #[test]
    fn validates_segment_identity_and_active_window_accounting() {
        let center = CENTER;
        let digest = "ba".repeat(32);
        let report = report_fixture(&digest);
        let verify = |value| {
            serde_json::from_value::<EventReport>(value)
                .unwrap()
                .verify(center, &digest, "report".to_owned())
        };
        let detection = verify(report.clone()).unwrap();
        assert!(detection.narrow_contrast_method.is_none());
        assert_eq!(detection.active_window_range_count, 1);
        assert_eq!(detection.active_window_count, 2);
        assert_eq!(
            detection.events[0].lower_frequency_hz,
            center + CELL_WIDTH_HZ
        );
        assert!(
            serde_json::to_value(&detection)
                .unwrap()
                .get("events")
                .is_none()
        );
        for (field, value) in [
            ("analyzed_prefix_sha256", json!("changed")),
            ("analyzed_samples", json!(SEGMENT_SAMPLES - WINDOW_SAMPLES)),
            ("active_window_ranges", json!([[0, 2]])),
            ("active_window_count", json!(1)),
            ("omitted_component_count", json!(1)),
        ] {
            let mut invalid = report.clone();
            invalid[field] = value;
            assert!(verify(invalid).is_err(), "accepted invalid {field}");
        }
        for (field, value) in [
            (
                "lower_frequency_hz",
                json!(center - 10_000_000 - CELL_WIDTH_HZ),
            ),
            (
                "upper_frequency_hz_exclusive",
                json!(center + 10_000_000 + CELL_WIDTH_HZ),
            ),
            (
                "upper_frequency_hz_exclusive",
                json!(center + 2 * CELL_WIDTH_HZ + 1),
            ),
            ("lower_frequency_hz", json!(center + 2 * CELL_WIDTH_HZ)),
        ] {
            let mut invalid = report.clone();
            invalid["events"][0][field] = value;
            assert!(verify(invalid).is_err(), "accepted invalid {field}");
        }
        let mut invalid = report.clone();
        invalid["events"][0]["narrow_contrast_db"] = json!(3.5);
        assert!(verify(invalid.clone()).is_err());
        invalid["narrow_contrast_method"] =
            json!("mean_target_excess_minus_stronger_adjacent_band");
        assert!(
            verify(invalid.clone()).is_err(),
            "left flank is in the center guard"
        );
        invalid["events"][0]["lower_frequency_hz"] = json!(center + 2 * CELL_WIDTH_HZ);
        invalid["events"][0]["upper_frequency_hz_exclusive"] = json!(center + 3 * CELL_WIDTH_HZ);
        let detection = verify(invalid.clone()).unwrap();
        assert!(detection.narrow_contrast_method.is_some());
        assert_eq!(detection.events[0].narrow_contrast_db, Some(3.5));
        invalid["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("narrow_contrast_db");
        assert!(
            verify(invalid).is_err(),
            "eligible event is missing its contrast score"
        );

        let mut invalid = report.clone();
        invalid["hit_tile_count"] = json!(3);
        invalid["events"][0]["hit_tile_count"] = json!(3);
        assert!(
            verify(invalid).is_err(),
            "accepted more hits than bounding tiles"
        );

        let mut invalid: EventReport = serde_json::from_value(report).unwrap();
        invalid.events[0].peak_excess_db = f64::INFINITY;
        assert!(
            invalid
                .verify(center, &digest, "report".to_owned())
                .is_err()
        );
    }

    #[test]
    fn retained_event_bounds_exclude_the_center_guard() {
        let digest = "ba".repeat(32);
        let guard_lower = CENTER - CENTER_GUARD_HZ;
        let guard_upper = CENTER + CENTER_GUARD_HZ;
        for (lower, upper, eligible) in [
            (guard_lower - CELL_WIDTH_HZ, guard_lower, true),
            (guard_upper, guard_upper + CELL_WIDTH_HZ, true),
            (guard_lower, CENTER, false),
            (CENTER, guard_upper, false),
            (
                guard_lower - CELL_WIDTH_HZ,
                guard_upper + CELL_WIDTH_HZ,
                false,
            ),
        ] {
            let mut report = report_fixture(&digest);
            report["events"][0]["lower_frequency_hz"] = json!(lower);
            report["events"][0]["upper_frequency_hz_exclusive"] = json!(upper);
            let result = serde_json::from_value::<EventReport>(report)
                .unwrap()
                .verify(CENTER, &digest, "report".to_owned());
            assert_eq!(result.is_ok(), eligible, "{lower}..{upper}");
        }
    }

    #[test]
    fn counts_each_active_window_touching_validated_wifi_once() {
        let digest = "ba".repeat(32);
        let mut report = report_fixture(&digest);
        report["active_window_ranges"] = json!([[0, 1], [2, 4]]);
        report["active_window_count"] = json!(3);
        report["hit_tile_count"] = json!(3);
        report["component_count"] = json!(2);
        let mut first_window = report["events"][0].clone();
        first_window["start_sample"] = json!(0);
        first_window["end_sample_exclusive"] = json!(WINDOW_SAMPLES);
        first_window["hit_tile_count"] = json!(1);
        report["events"].as_array_mut().unwrap().push(first_window);

        let start = 10_000;
        let spans = [
            (start - 10)..start,
            (start + 100)..(start + 101),
            (start + 200)..(start + 201),
            (start + WINDOW_SAMPLES)..(start + 2 * WINDOW_SAMPLES),
            (start + 3 * WINDOW_SAMPLES + 1)..(start + 4 * WINDOW_SAMPLES),
        ];
        for (spans, overlapping) in [
            (Some(spans.as_slice()), Some(2)),
            (Some([].as_slice()), Some(0)),
            (None, None),
        ] {
            let mut detection = serde_json::from_value::<EventReport>(report.clone())
                .unwrap()
                .verify(CENTER, &digest, "survey".to_owned())
                .unwrap();
            detection.detect_wifi_activity(start, spans);
            assert!(detection.active_window_ranges.is_empty());
            assert_eq!(
                detection.wifi_activity_windows,
                overlapping.map(|count| WifiActivityWindows {
                    active_windows: 3,
                    overlapping_validated_wifi: count,
                }),
            );
            let saved = serde_json::to_value(detection).unwrap();
            assert!(saved.get("active_window_ranges").is_none());
            assert_eq!(
                saved.get("wifi_activity_windows").is_some(),
                spans.is_some()
            );
        }
    }

    #[test]
    fn validates_independent_candidates_without_counting_their_hits_twice() {
        let digest = "ba".repeat(32);
        let mut report = report_fixture(&digest);
        report["narrow_contrast_method"] = json!("mean_target_excess_minus_stronger_adjacent_band");
        let mut candidate = report["events"][0].clone();
        candidate["lower_frequency_hz"] = json!(CENTER + 2 * CELL_WIDTH_HZ);
        candidate["upper_frequency_hz_exclusive"] = json!(CENTER + 3 * CELL_WIDTH_HZ);
        candidate["narrow_contrast_db"] = json!(3.0);
        report["events"][0] = candidate.clone();
        report["narrow_search"] = json!({
            "method": "fixed_band_runs",
            "candidate_limit": NARROW_CANDIDATE_LIMIT,
            "candidate_count": 1,
            "candidates": [candidate.clone()]
        });
        let verify = |report| {
            serde_json::from_value::<EventReport>(report)
                .unwrap()
                .verify(CENTER, &digest, "survey".to_owned())
        };
        let detection = verify(report.clone()).unwrap();
        assert_eq!(detection.hit_tile_count, 2);
        assert_eq!(detection.narrow_search.unwrap().candidates.len(), 1);

        let mut duplicate = report.clone();
        duplicate["narrow_search"]["candidate_count"] = json!(2);
        duplicate["narrow_search"]["candidates"] = json!([candidate.clone(), candidate]);
        assert!(verify(duplicate).is_err());
        for (field, value) in [
            ("narrow_contrast_db", json!(2.99)),
            ("peak_excess_db", json!(5.99)),
            ("hit_tile_count", json!(3)),
            ("lower_frequency_hz", json!(CENTER + CELL_WIDTH_HZ)),
        ] {
            let mut invalid = report.clone();
            invalid["narrow_search"]["candidates"][0][field] = value;
            assert!(verify(invalid).is_err(), "accepted invalid {field}");
        }
        report["narrow_search"]["candidate_count"] = json!(NARROW_CANDIDATE_LIMIT + 1);
        assert!(verify(report).is_err());
    }
}
