use std::fs;
use std::io::Seek;

use serde_json::{Value, json};

use super::*;

const CENTER_HZ: u64 = 2_437_000_000;
const INPUT_SAMPLES: u64 = WINDOW_SAMPLES + 7;
const HALF_WINDOW_STRIDE: u64 = WINDOW_SAMPLES / 2;
const OVERLAP_ANALYZED_SAMPLES: u64 = WINDOW_SAMPLES + 3 * HALF_WINDOW_STRIDE;
const OVERLAP_INPUT_SAMPLES: u64 = OVERLAP_ANALYZED_SAMPLES + 7;
const CORE_HALF_SPAN_HZ: u64 = 8_000_000;
const FULL_HALF_SPAN_HZ: u64 = 10_000_000;

fn event_report() -> Value {
    json!({
        "schema_version": 1,
        "operation": "offline_rf_spectrum_events",
        "source_format": "SC16_Q11_LE",
        "center_frequency_hz": CENTER_HZ,
        "sample_rate_hz": SAMPLE_RATE,
        "input_samples": INPUT_SAMPLES,
        "analyzed_samples": WINDOW_SAMPLES,
        "analyzed_prefix_sha256": "f".repeat(64),
        "window_samples": WINDOW_SAMPLES,
        "cell_width_hz": CELL_WIDTH_HZ,
        "usable_half_span_hz": CORE_HALF_SPAN_HZ,
        "center_guard_hz": CENTER_GUARD_HZ,
        "minimum_excess_over_temporal_median_db": 6.0,
        "event_limit": EVENT_LIMIT,
        "connectivity": "orthogonal_neighbors_in_sample_window_and_frequency_cell",
        "windows": 1,
        "eligible_cells": 158,
        "evaluated_tile_count": 158,
        "hit_tile_count": 1,
        "active_window_count": 1,
        "component_count": 1,
        "omitted_component_count": 0,
        "omitted_hit_tile_count": 0,
        "events": [{
            "start_sample": 0,
            "end_sample_exclusive": WINDOW_SAMPLES,
            "lower_frequency_hz": CENTER_HZ + CELL_WIDTH_HZ,
            "upper_frequency_hz_exclusive": CENTER_HZ + 2 * CELL_WIDTH_HZ,
            "peak_excess_db": 8.0,
            "hit_tile_count": 1,
        }],
    })
}

fn acquisition(center_hz: u64) -> Value {
    json!({
        "format": "SC16_Q11",
        "dtype": "<i2",
        "interleave": "I,Q",
        "complex_scale": 2048,
        "frequency_hz": center_hz,
        "sample_rate_hz": SAMPLE_RATE,
        "bandwidth_hz": SAMPLE_RATE,
        "num_samples": INPUT_SAMPLES,
    })
}

#[test]
fn open_pins_complete_iq_matches_tuning_and_hashes_json_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let iq = directory.path().join("capture.iq");
    fs::write(&iq, vec![0; INPUT_SAMPLES as usize * 4]).unwrap();
    let sidecar = directory.path().join("capture.iq.json");
    let source_bytes = serde_json::to_vec_pretty(&acquisition(CENTER_HZ)).unwrap();
    fs::write(&sidecar, &source_bytes).unwrap();
    let events = directory.path().join("events.json");
    let event_bytes = serde_json::to_vec_pretty(&event_report()).unwrap();
    fs::write(&events, &event_bytes).unwrap();

    let mut input = open(&iq, &events).unwrap();
    assert_eq!(input.input_samples, INPUT_SAMPLES);
    assert_eq!(input.analyzed_samples, WINDOW_SAMPLES);
    assert!(matches!(input.event_stride, EventStride::Contiguous));
    assert_eq!(input.center_frequency_hz, CENTER_HZ);
    assert_eq!(input.iq.stream_position().unwrap(), 0);
    assert_eq!(input.event_count, 1);
    assert_eq!(input.active_window_count, None);
    assert_eq!(input.replay_spans.len(), 1);
    assert_eq!(input.replay_spans[0].samples(), WINDOW_SAMPLES);
    assert_eq!(
        input.event_report_sha256,
        format!("{:x}", Sha256::digest(event_bytes))
    );
    assert_eq!(
        input.acquisition_sha256,
        format!("{:x}", Sha256::digest(source_bytes))
    );

    fs::write(
        &sidecar,
        serde_json::to_vec(&acquisition(CENTER_HZ + 1)).unwrap(),
    )
    .unwrap();
    assert!(
        open(&iq, &events)
            .unwrap_err()
            .to_string()
            .contains("tuning")
    );
}

#[test]
fn reject_wrong_schema_incomplete_grid_and_inconsistent_omissions() {
    let cases = [
        ("/schema_version", json!(4), "version 1"),
        (
            "/analyzed_samples",
            json!(WINDOW_SAMPLES + 1),
            "window grid",
        ),
        ("/omitted_component_count", json!(1), "accounting"),
        (
            "/events/0/lower_frequency_hz",
            json!(CENTER_HZ),
            "frequency bounds",
        ),
        ("/events/0/hit_tile_count", json!(2), "hit count"),
    ];
    for (field, value, expected) in cases {
        let mut report = event_report();
        *report.pointer_mut(field).unwrap() = value;
        let report: EventReport = serde_json::from_value(report).unwrap();
        assert!(
            report
                .validate(INPUT_SAMPLES)
                .unwrap_err()
                .to_string()
                .contains(expected),
            "{field}"
        );
    }
}

fn overlapping_report() -> Value {
    let mut report = event_report();
    report["schema_version"] = json!(2);
    report["window_stride_samples"] = json!(HALF_WINDOW_STRIDE);
    report["input_samples"] = json!(OVERLAP_INPUT_SAMPLES);
    report["analyzed_samples"] = json!(OVERLAP_ANALYZED_SAMPLES);
    report["windows"] = json!(4);
    report["evaluated_tile_count"] = json!(4 * 158);
    report["hit_tile_count"] = json!(3);
    report["active_window_count"] = json!(3);
    report["events"][0]["start_sample"] = json!(HALF_WINDOW_STRIDE);
    report["events"][0]["end_sample_exclusive"] = json!(OVERLAP_ANALYZED_SAMPLES);
    report["events"][0]["hit_tile_count"] = json!(3);
    report
}

#[test]
fn accept_half_window_grid_and_count_overlapping_event_tiles() {
    let report: EventReport = serde_json::from_value(overlapping_report()).unwrap();
    let (stride, spans) = report.validate(OVERLAP_INPUT_SAMPLES).unwrap();
    assert!(matches!(stride, EventStride::HalfWindow));
    assert_eq!(
        spans,
        [Span {
            start_sample: HALF_WINDOW_STRIDE,
            end_sample_exclusive: OVERLAP_ANALYZED_SAMPLES,
        }]
    );
}

#[test]
fn reject_stride_declarations_incompatible_with_event_report_version() {
    let mut version_one = event_report();
    version_one["window_stride_samples"] = json!(WINDOW_SAMPLES);
    let mut version_two_missing_stride = event_report();
    version_two_missing_stride["schema_version"] = json!(2);
    let mut version_two_wrong_stride = overlapping_report();
    version_two_wrong_stride["window_stride_samples"] = json!(WINDOW_SAMPLES);
    for (report, input_samples) in [
        (version_one, INPUT_SAMPLES),
        (version_two_missing_stride, INPUT_SAMPLES),
        (version_two_wrong_stride, OVERLAP_INPUT_SAMPLES),
    ] {
        let report: EventReport = serde_json::from_value(report).unwrap();
        let error = report.validate(input_samples).unwrap_err().to_string();
        assert!(error.contains("window_stride_samples"), "{error}");
    }

    let mut report = event_report();
    report["window_stride_samples"] = Value::Null;
    assert!(serde_json::from_value::<EventReport>(report).is_err());
}

#[test]
fn reject_inconsistent_half_window_extents_and_event_tile_counts() {
    for (field, value, expected) in [
        ("/windows", json!(u64::MAX), "window grid"),
        (
            "/analyzed_samples",
            json!(OVERLAP_ANALYZED_SAMPLES - 1),
            "window grid",
        ),
        (
            "/events/0/start_sample",
            json!(OVERLAP_ANALYZED_SAMPLES - HALF_WINDOW_STRIDE),
            "sample bounds",
        ),
        (
            "/events/0/end_sample_exclusive",
            json!(OVERLAP_ANALYZED_SAMPLES - 1),
            "sample bounds",
        ),
        ("/events/0/hit_tile_count", json!(4), "hit count"),
    ] {
        let mut report = overlapping_report();
        *report.pointer_mut(field).unwrap() = value;
        let report: EventReport = serde_json::from_value(report).unwrap();
        let error = report
            .validate(OVERLAP_INPUT_SAMPLES)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{field}: {error}");
    }
}

fn overlapping_survey_report() -> Value {
    let mut report = overlapping_report();
    report["schema_version"] = json!(3);
    report["active_window_ranges"] = json!([[1, 4]]);
    report
}

fn validate_survey(report: Value) -> Result<Vec<Span>> {
    let report: EventReport = serde_json::from_value(report).unwrap();
    let (_, spans) = report.validate(OVERLAP_INPUT_SAMPLES)?;
    Ok(spans)
}

#[test]
fn survey_replay_uses_union_of_retained_rows_and_rejects_missing_coverage() {
    let mut report = overlapping_survey_report();
    let mut later = report["events"][0].clone();
    later["start_sample"] = json!(2 * HALF_WINDOW_STRIDE);
    later["hit_tile_count"] = json!(2);
    later["lower_frequency_hz"] = json!(CENTER_HZ + 3 * CELL_WIDTH_HZ);
    later["upper_frequency_hz_exclusive"] = json!(CENTER_HZ + 4 * CELL_WIDTH_HZ);
    let mut earlier = report["events"][0].clone();
    earlier["end_sample_exclusive"] = json!(WINDOW_SAMPLES + 2 * HALF_WINDOW_STRIDE);
    earlier["hit_tile_count"] = json!(2);
    report["events"] = json!([later, earlier]);
    report["component_count"] = json!(2);
    report["hit_tile_count"] = json!(4);
    assert_eq!(
        validate_survey(report.clone()).unwrap(),
        [Span {
            start_sample: HALF_WINDOW_STRIDE,
            end_sample_exclusive: OVERLAP_ANALYZED_SAMPLES,
        }]
    );

    report["events"][0]["start_sample"] = json!(HALF_WINDOW_STRIDE);
    report["events"][0]["end_sample_exclusive"] = json!(WINDOW_SAMPLES + 2 * HALF_WINDOW_STRIDE);
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(
        error.contains("exceed the omitted hit tile count"),
        "{error}"
    );
}

fn survey_with_omission() -> Value {
    let mut report = overlapping_survey_report();
    report["event_limit"] = json!(1);
    report["component_count"] = json!(2);
    report["omitted_component_count"] = json!(1);
    report["hit_tile_count"] = json!(3);
    report["omitted_hit_tile_count"] = json!(1);
    report["active_window_count"] = json!(2);
    report["active_window_ranges"] = json!([[0, 1], [2, 3]]);
    report["events"][0]["start_sample"] = json!(0);
    report["events"][0]["end_sample_exclusive"] = json!(WINDOW_SAMPLES);
    report["events"][0]["upper_frequency_hz_exclusive"] = json!(CENTER_HZ + 3 * CELL_WIDTH_HZ);
    report["events"][0]["hit_tile_count"] = json!(2);
    report
}

#[test]
fn survey_retains_active_runs_from_omitted_components_within_hit_budget() {
    let mut report = survey_with_omission();
    assert_eq!(
        validate_survey(report.clone()).unwrap(),
        [
            Span {
                start_sample: 0,
                end_sample_exclusive: WINDOW_SAMPLES,
            },
            Span {
                start_sample: 2 * HALF_WINDOW_STRIDE,
                end_sample_exclusive: 2 * HALF_WINDOW_STRIDE + WINDOW_SAMPLES,
            },
        ]
    );
    report["active_window_ranges"] = json!([[0, 1], [2, 4]]);
    report["active_window_count"] = json!(3);
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(
        error.contains("exceed the omitted hit tile count"),
        "{error}"
    );
}

#[test]
fn survey_rejects_invalid_runs_and_retained_events_crossing_inactive_rows() {
    for (ranges, expected) in [
        (json!([[0, 1], [2, 3], [3, 4]]), "range count"),
        (json!([[0, 1], [1, 2]]), "ordered maximal runs"),
        (json!([[2, 3], [0, 1]]), "ordered maximal runs"),
        (json!([[0, 1], [4, 5]]), "inside the window grid"),
        (json!([[0, 1], [2, 2]]), "ordered maximal runs"),
        (json!([[0, 1]]), "lengths differ"),
    ] {
        let mut report = survey_with_omission();
        report["active_window_ranges"] = ranges;
        let error = validate_survey(report).unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }

    let mut report = survey_with_omission();
    report["events"][0]["end_sample_exclusive"] = json!(WINDOW_SAMPLES + 2 * HALF_WINDOW_STRIDE);
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(
        error.contains("outside the active window ranges"),
        "{error}"
    );
}

#[test]
fn survey_requires_versioned_fields_with_explicit_numeric_stride_and_range_pairs() {
    let mut report = overlapping_survey_report();
    report
        .as_object_mut()
        .unwrap()
        .remove("active_window_ranges");
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(error.contains("active_window_ranges"), "{error}");

    let mut report = overlapping_survey_report();
    report
        .as_object_mut()
        .unwrap()
        .remove("window_stride_samples");
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(error.contains("window_stride_samples"), "{error}");

    let mut report = overlapping_survey_report();
    report["schema_version"] = json!(2);
    let error = validate_survey(report).unwrap_err().to_string();
    assert!(error.contains("active_window_ranges"), "{error}");

    for invalid in [Value::Null, json!([[1, 2, 3]])] {
        let mut report = overlapping_survey_report();
        report["active_window_ranges"] = invalid;
        assert!(serde_json::from_value::<EventReport>(report).is_err());
    }
}

#[test]
fn survey_reader_accepts_contiguous_stride_and_preserves_versioned_size_limits() {
    let directory = tempfile::tempdir().unwrap();
    let iq = directory.path().join("capture.iq");
    fs::write(&iq, vec![0; INPUT_SAMPLES as usize * 4]).unwrap();
    fs::write(
        directory.path().join("capture.iq.json"),
        serde_json::to_vec(&acquisition(CENTER_HZ)).unwrap(),
    )
    .unwrap();
    let events = directory.path().join("events.json");
    let mut report = event_report();
    let mut bytes = serde_json::to_vec(&report).unwrap();
    bytes.resize(LEGACY_EVENT_REPORT_LIMIT as usize + 1, b' ');
    fs::write(&events, bytes).unwrap();
    let error = open(&iq, &events).unwrap_err().to_string();
    assert!(error.contains("legacy event report"), "{error}");

    report["schema_version"] = json!(3);
    report["window_stride_samples"] = json!(WINDOW_SAMPLES);
    report["active_window_ranges"] = json!([[0, 1]]);
    let mut bytes = serde_json::to_vec(&report).unwrap();
    bytes.resize(SURVEY_EVENT_REPORT_LIMIT as usize, b' ');
    fs::write(&events, &bytes).unwrap();
    let input = open(&iq, &events).unwrap();
    assert!(matches!(input.event_stride, EventStride::Contiguous));
    assert_eq!(input.event_count, 1);
    assert_eq!(input.active_window_count, Some(1));
    assert_eq!(input.replay_spans[0].samples(), WINDOW_SAMPLES);
    bytes.push(b' ');
    fs::write(&events, bytes).unwrap();
    let error = open(&iq, &events).unwrap_err().to_string();
    assert!(error.contains("no larger than"), "{error}");
}

fn full_band_report() -> Value {
    let mut report = event_report();
    report["usable_half_span_hz"] = json!(FULL_HALF_SPAN_HZ);
    report["eligible_cells"] = json!(198);
    report["evaluated_tile_count"] = json!(198);
    report["events"][0]["lower_frequency_hz"] = json!(CENTER_HZ - FULL_HALF_SPAN_HZ);
    report["events"][0]["upper_frequency_hz_exclusive"] =
        json!(CENTER_HZ - FULL_HALF_SPAN_HZ + CELL_WIDTH_HZ);
    report
}

#[test]
fn accept_full_band_with_events_at_both_edges() {
    let mut report = full_band_report();
    let mut upper = report["events"][0].clone();
    upper["lower_frequency_hz"] = json!(CENTER_HZ + FULL_HALF_SPAN_HZ - CELL_WIDTH_HZ);
    upper["upper_frequency_hz_exclusive"] = json!(CENTER_HZ + FULL_HALF_SPAN_HZ);
    report["events"].as_array_mut().unwrap().push(upper);
    report["component_count"] = json!(2);
    report["hit_tile_count"] = json!(2);

    let report: EventReport = serde_json::from_value(report).unwrap();
    let (_, spans) = report.validate(INPUT_SAMPLES).unwrap();
    assert_eq!(
        spans,
        [Span {
            start_sample: 0,
            end_sample_exclusive: WINDOW_SAMPLES
        }; 2]
    );
}

#[test]
fn reject_full_band_grid_mismatch_and_out_of_band_or_guard_events() {
    for (field, value, error) in [
        ("/eligible_cells", json!(158), "event report grid"),
        ("/usable_half_span_hz", json!(9_000_000), "half-span"),
        (
            "/events/0/upper_frequency_hz_exclusive",
            json!(CENTER_HZ),
            "frequency bounds",
        ),
        (
            "/events/0/lower_frequency_hz",
            json!(CENTER_HZ - FULL_HALF_SPAN_HZ - CELL_WIDTH_HZ),
            "frequency bounds",
        ),
        (
            "/events/0/upper_frequency_hz_exclusive",
            json!(CENTER_HZ + FULL_HALF_SPAN_HZ + CELL_WIDTH_HZ),
            "frequency bounds",
        ),
    ] {
        let mut report = full_band_report();
        *report.pointer_mut(field).unwrap() = value;
        let report: EventReport = serde_json::from_value(report).unwrap();
        assert!(
            report
                .validate(INPUT_SAMPLES)
                .unwrap_err()
                .to_string()
                .contains(error),
            "{field}"
        );
    }
}
