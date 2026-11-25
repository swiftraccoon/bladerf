use super::*;
use crate::spectrum::{CELL_COUNT, WindowLayout, activity_detection};

const CENTER_HZ: u64 = 2_437_000_000;
const WINDOWS: usize = 8;

fn detect(points: &[(usize, usize, f64)], limit: usize) -> Detection {
    let powers = fixture(points, EventBand::Inner);
    let stop = AtomicBool::new(false);
    let activity = activity_detection(
        CENTER_HZ,
        (WINDOWS * FFT) as u64,
        WINDOWS,
        &powers,
        WindowLayout::Contiguous,
        &stop,
    )
    .unwrap();
    let detection = locate_limited(
        &powers,
        EventBand::Inner,
        CENTER_HZ,
        EventStride::Contiguous,
        false,
        limit,
        &stop,
    )
    .unwrap();
    assert_eq!(
        detection.hit_tile_count,
        activity
            .cells
            .iter()
            .filter(|cell| cell.excluded_reason.is_none())
            .map(|cell| cell.windows_above_threshold)
            .sum::<usize>()
    );
    detection
}

fn fixture(points: &[(usize, usize, f64)], band: EventBand) -> Vec<Vec<f64>> {
    let mut powers = vec![vec![1.0; WINDOWS]; band.cell_count()];
    for &(window, cell, power) in points {
        powers[cell][window] = power;
    }
    powers
}

#[test]
fn isolated_and_adjacent_hits_are_preserved_without_diagonal_bridging() {
    let detection = detect(
        &[
            (0, 12, 10.0),
            (1, 5, 10.0),
            (1, 6, 10.0),
            (2, 6, 100.0),
            (3, 7, 10.0),
        ],
        MAX_EVENTS,
    );

    assert_eq!(detection.eligible_cells, 158);
    assert_eq!(detection.evaluated_tile_count, WINDOWS * 158);
    assert_eq!(detection.hit_tile_count, 5);
    assert_eq!(detection.active_window_count, 4);
    assert_eq!(detection.component_count, 3);
    assert_eq!(detection.omitted_component_count, 0);
    let events = detection.events;
    assert_eq!(events[0].start_sample, 0);
    assert_eq!(events[0].end_sample_exclusive, FFT as u64);
    assert_eq!(events[0].hit_tile_count, 1);
    assert_eq!(events[1].start_sample, FFT as u64);
    assert_eq!(events[1].end_sample_exclusive, (3 * FFT) as u64);
    assert_eq!(events[1].lower_frequency_hz, CENTER_HZ - 7_500_000);
    assert_eq!(
        events[1].upper_frequency_hz_exclusive,
        CENTER_HZ - 7_300_000
    );
    assert_eq!(events[1].hit_tile_count, 3);
    assert!((events[1].peak_excess_db - 20.0).abs() < 1e-12);
    assert_eq!(events[2].start_sample, (3 * FFT) as u64);
    assert_eq!(events[2].hit_tile_count, 1);
}

#[test]
fn simultaneous_hits_on_opposite_sides_of_dc_stay_separate() {
    let detection = detect(
        &[
            (1, 78, 10.0),
            (1, 79, 10_000.0),
            (1, 80, 10_000.0),
            (1, 81, 10.0),
        ],
        MAX_EVENTS,
    );
    assert_eq!(detection.hit_tile_count, 2);
    assert_eq!(detection.active_window_count, 1);
    assert_eq!(detection.component_count, 2);
    assert_eq!(
        detection.events[0].upper_frequency_hz_exclusive,
        CENTER_HZ - 100_000
    );
    assert_eq!(detection.events[1].lower_frequency_hz, CENTER_HZ + 100_000);
    for event in detection.events {
        assert_eq!(event.start_sample, FFT as u64);
        assert!((event.peak_excess_db - 10.0).abs() < 1e-12);
    }
}

#[test]
fn constant_background_and_dc_only_transients_have_no_events() {
    let detection = detect(&[(1, 79, 100.0), (1, 80, 100.0)], MAX_EVENTS);
    assert_eq!(detection.hit_tile_count, 0);
    assert_eq!(detection.active_window_count, 0);
    assert_eq!(detection.component_count, 0);
    assert!(detection.events.is_empty());
}

#[test]
fn retains_strongest_then_earliest_under_cap_and_orders_by_time() {
    let detection = detect(
        &[
            (0, 0, 100.0),
            (1, 6, 1000.0),
            (2, 12, 100.0),
            (WINDOWS - 1, CELL_COUNT - 1, 10.0),
        ],
        2,
    );
    assert_eq!(detection.hit_tile_count, 4);
    assert_eq!(detection.active_window_count, 4);
    assert_eq!(detection.component_count, 4);
    assert_eq!(detection.omitted_component_count, 2);
    assert_eq!(detection.omitted_hit_tile_count, 2);
    assert_eq!(detection.events.len(), 2);
    assert_eq!(detection.events[0].start_sample, 0);
    assert_eq!(
        detection.events[0].lower_frequency_hz,
        CENTER_HZ - 8_000_000
    );
    assert_eq!(detection.events[1].start_sample, FFT as u64);
}

#[test]
fn active_ranges_include_omitted_components_and_exclude_guard_only_windows() {
    let band = EventBand::Inner;
    let powers = fixture(
        &[
            (0, 5, 100.0),
            (1, 5, 100.0),
            (2, 79, 10_000.0),
            (2, 80, 10_000.0),
            (WINDOWS - 1, 35, 10.0),
        ],
        band,
    );
    let stop = AtomicBool::new(false);
    let detection = locate_limited(
        &powers,
        band,
        CENTER_HZ,
        EventStride::HalfWindow,
        true,
        1,
        &stop,
    )
    .unwrap();

    assert_eq!(detection.active_window_count, 3);
    assert_eq!(
        detection.active_window_ranges,
        Some(vec![[0, 2], [WINDOWS - 1, WINDOWS]])
    );
    assert_eq!(detection.omitted_component_count, 1);
    assert_eq!(detection.omitted_hit_tile_count, 1);
    assert_eq!(detection.events.len(), 1);
    assert_eq!(detection.events[0].start_sample, 0);
    assert_eq!(
        detection.events[0].end_sample_exclusive,
        (FFT + FFT / 2) as u64
    );
    assert_eq!(detection.events[0].narrow_contrast_db, Some(20.0));
}

#[test]
fn narrow_contrast_compares_temporal_excess_with_the_stronger_flank_in_each_window() {
    let band = EventBand::Inner;
    let mut powers = fixture(&[], band);
    for (index, cell) in powers.iter_mut().enumerate() {
        cell.fill(2.0_f64.powi((index % 9) as i32 - 4));
    }
    for window in [1, 2] {
        for cell in [20, 21, 36, 37] {
            powers[cell][window] *= 10.0;
        }
        for cell in &mut powers[45..51] {
            cell[window] *= 10.0;
        }
    }
    for cell in [34, 35] {
        powers[cell][1] *= 3.0;
    }
    for cell in [38, 39] {
        powers[cell][2] *= 2.0;
    }

    let stop = AtomicBool::new(false);
    let detect = |measure_contrast| {
        locate(
            &powers,
            band,
            CENTER_HZ,
            EventStride::Contiguous,
            measure_contrast,
            &stop,
        )
        .unwrap()
    };
    let measured = detect(true);
    assert_eq!(measured.events.len(), 3);
    let [isolated, with_flanks, broad] = measured.events.as_slice() else {
        unreachable!()
    };
    assert_close_to(isolated.narrow_contrast_db.unwrap(), 10.0);
    assert_close_to(
        with_flanks.narrow_contrast_db.unwrap(),
        10.0 - 5.0 * (3.0_f64.log10() + 2.0_f64.log10()),
    );
    assert_eq!(broad.narrow_contrast_db, None);

    let mut measured = serde_json::to_value(measured).unwrap();
    measured
        .as_object_mut()
        .unwrap()
        .remove("active_window_ranges");
    measured.as_object_mut().unwrap().remove("narrow_search");
    for event in measured["events"].as_array_mut().unwrap() {
        event.as_object_mut().unwrap().remove("narrow_contrast_db");
    }
    assert_eq!(measured, serde_json::to_value(detect(false)).unwrap());
}

#[test]
fn narrow_contrast_requires_complete_flanks_and_at_least_two_windows() {
    let band = EventBand::Inner;
    let mut powers = fixture(&[(1, 30, 10.0)], band);
    for cell in [0, 1, 20, 78, 81, CELL_COUNT - 1] {
        for power in &mut powers[cell][1..3] {
            *power *= 10.0;
        }
    }
    let detection = locate(
        &powers,
        band,
        CENTER_HZ,
        EventStride::Contiguous,
        true,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        detection
            .events
            .iter()
            .map(|event| event.narrow_contrast_db)
            .collect::<Vec<_>>(),
        [None, Some(10.0), None, None, None, None]
    );
}

#[test]
fn fixed_band_search_finds_a_narrow_burst_embedded_in_broad_activity() {
    let band = EventBand::Inner;
    let mut powers = fixture(&[], band);
    for cell in &mut powers {
        cell[1..3].fill(10.0);
    }
    let stop = AtomicBool::new(false);
    let survey = |powers: &[Vec<f64>]| {
        locate_limited(
            powers,
            band,
            CENTER_HZ,
            EventStride::Contiguous,
            true,
            1,
            &stop,
        )
        .unwrap()
    };
    let baseline = serde_json::to_value(survey(&powers)).unwrap();
    assert_eq!(baseline["narrow_search"]["candidate_count"], 0);
    for cell in &mut powers[20..22] {
        cell[1..3].fill(30.0);
    }
    let detected = survey(&powers);
    assert_eq!(detected.omitted_component_count, 1);
    assert_eq!(detected.events.len(), 1);
    assert_eq!(detected.events[0].narrow_contrast_db, None);
    let lower_frequency_hz = CENTER_HZ - 6_000_000;
    assert!(detected.events[0].lower_frequency_hz < lower_frequency_hz);
    assert!(detected.events[0].upper_frequency_hz_exclusive > lower_frequency_hz + 200_000);
    let report = serde_json::to_value(detected).unwrap();
    let target = report["narrow_search"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["lower_frequency_hz"] == lower_frequency_hz
                && event["upper_frequency_hz_exclusive"] == lower_frequency_hz + 200_000
        })
        .unwrap();
    assert_eq!(target["start_sample"], FFT);
    assert_eq!(target["end_sample_exclusive"], 3 * FFT);
    assert_eq!(target["hit_tile_count"], 4);
    assert_close_to(
        target["narrow_contrast_db"].as_f64().unwrap(),
        10.0 * 3.0_f64.log10(),
    );
    assert_close_to(
        target["peak_excess_db"].as_f64().unwrap(),
        10.0 * 30.0_f64.log10(),
    );
}

#[test]
fn fixed_band_search_caps_candidates_separately_with_deterministic_order() {
    let band = EventBand::Inner;
    let mut powers = vec![vec![1.0; 14]; band.cell_count()];
    let burst_cells: Vec<_> = (2..78).step_by(3).chain((82..158).step_by(3)).collect();
    for &cell in &burst_cells {
        for start in [0, 3, 6] {
            powers[cell][start..start + 2].fill(4.0);
        }
    }
    powers[157][6..8].fill(10.0);
    let report = locate_limited(
        &powers,
        band,
        CENTER_HZ,
        EventStride::Contiguous,
        true,
        1,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(report.events.len(), 1);
    let report = serde_json::to_value(report).unwrap();
    let search = &report["narrow_search"];
    assert_eq!(search["candidate_limit"], 128);
    assert_eq!(search["candidate_count"], 3 * burst_cells.len());
    let candidates = search["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 128);
    let last = candidates.last().unwrap();
    assert_eq!(last["start_sample"], 6 * FFT);
    assert_eq!(last["lower_frequency_hz"], CENTER_HZ + 7_700_000);
    let penultimate = &candidates[126];
    assert_eq!(penultimate["start_sample"], 6 * FFT);
    let expected =
        CENTER_HZ - 8_000_000 + burst_cells[126 - 2 * burst_cells.len()] as u64 * 100_000;
    assert_eq!(penultimate["lower_frequency_hz"], expected);
}

#[test]
fn fixed_band_search_counts_only_target_cells_above_the_broad_threshold() {
    let band = EventBand::Inner;
    let mut powers = fixture(&[], band);
    powers[20][1..3].fill(100.0);
    powers[21][1..3].fill(2.0);
    let report = locate(
        &powers,
        band,
        CENTER_HZ,
        EventStride::Contiguous,
        true,
        &AtomicBool::new(false),
    )
    .unwrap();
    let report = serde_json::to_value(report).unwrap();
    let target = report["narrow_search"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| {
            event["lower_frequency_hz"] == CENTER_HZ - 6_000_000
                && event["upper_frequency_hz_exclusive"] == CENTER_HZ - 5_800_000
        })
        .unwrap();
    assert_eq!(target["hit_tile_count"], 2);
}

#[test]
fn fixed_band_search_rejects_overlong_runs_and_flushes_half_stride_runs_at_end() {
    let band = EventBand::Inner;
    let mut powers = vec![vec![1.0; 24]; band.cell_count()];
    powers[20][..11].fill(10.0);
    powers[40][22..].fill(10.0);
    powers[42][21] = 10.0;
    let report = locate(
        &powers,
        band,
        CENTER_HZ,
        EventStride::HalfWindow,
        true,
        &AtomicBool::new(false),
    )
    .unwrap();
    let report = serde_json::to_value(report).unwrap();
    let search = &report["narrow_search"];
    assert_eq!(search["candidate_count"], 1);
    let event = &search["candidates"][0];
    assert_eq!(event["lower_frequency_hz"], CENTER_HZ - 4_000_000);
    assert_eq!(event["start_sample"], 22 * (FFT / 2));
    assert_eq!(event["end_sample_exclusive"], 23 * (FFT / 2) + FFT);
}

fn assert_close_to(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

#[test]
fn last_window_and_last_in_band_cell_end_at_half_open_boundaries() {
    let detection = detect(&[(WINDOWS - 1, CELL_COUNT - 1, 10.0)], MAX_EVENTS);
    assert_eq!(detection.component_count, 1);
    let event = &detection.events[0];
    assert_eq!(event.start_sample, ((WINDOWS - 1) * FFT) as u64);
    assert_eq!(event.end_sample_exclusive, (WINDOWS * FFT) as u64);
    assert_eq!(event.lower_frequency_hz, CENTER_HZ + 7_900_000);
    assert_eq!(event.upper_frequency_hz_exclusive, CENTER_HZ + 8_000_000);
}

#[test]
fn full_band_includes_both_edges_without_wrap_adjacency() {
    let band = EventBand::Full;
    let powers = fixture(
        &[
            (0, 0, 10.0),
            (0, 1, 10.0),
            (0, 99, 10_000.0),
            (0, 100, 10_000.0),
            (0, 198, 10.0),
            (0, 199, 10.0),
            (1, 0, 10.0),
        ],
        band,
    );
    let stop = AtomicBool::new(false);
    let detection = locate(
        &powers,
        band,
        CENTER_HZ,
        EventStride::Contiguous,
        false,
        &stop,
    )
    .unwrap();
    assert_eq!(detection.eligible_cells, 198);
    assert_eq!(detection.evaluated_tile_count, WINDOWS * 198);
    assert_eq!(detection.hit_tile_count, 5);
    assert_eq!(detection.component_count, 2);
    let left = &detection.events[0];
    let right = &detection.events[1];
    assert_eq!(left.lower_frequency_hz, CENTER_HZ - 10_000_000);
    assert_eq!(left.upper_frequency_hz_exclusive, CENTER_HZ - 9_800_000);
    assert_eq!(left.end_sample_exclusive, (2 * FFT) as u64);
    assert_eq!(left.hit_tile_count, 3);
    assert_eq!(right.lower_frequency_hz, CENTER_HZ + 9_800_000);
    assert_eq!(right.upper_frequency_hz_exclusive, CENTER_HZ + 10_000_000);
    assert_eq!(right.end_sample_exclusive, FFT as u64);
    assert_eq!(right.hit_tile_count, 2);
}

#[test]
fn overlapping_components_enclose_the_full_fft_windows() {
    let band = EventBand::Inner;
    let powers = fixture(&[(1, 5, 10.0), (2, 5, 12.0), (WINDOWS - 1, 5, 10.0)], band);
    let stop = AtomicBool::new(false);
    let detection = locate(
        &powers,
        band,
        CENTER_HZ,
        EventStride::HalfWindow,
        false,
        &stop,
    )
    .unwrap();
    let half_window = FFT as u64 / 2;

    assert_eq!(detection.component_count, 2);
    let first = &detection.events[0];
    assert_eq!(first.start_sample, half_window);
    assert_eq!(first.end_sample_exclusive, 2 * FFT as u64);
    let last = &detection.events[1];
    assert_eq!(last.start_sample, (WINDOWS as u64 - 1) * half_window);
    assert_eq!(last.end_sample_exclusive, last.start_sample + FFT as u64);
}

#[test]
fn rejects_wrong_band_shape_and_honors_cancellation() {
    let powers = fixture(&[], EventBand::Inner);
    let stop = AtomicBool::new(false);
    let stride = EventStride::Contiguous;
    let error = locate(&powers, EventBand::Full, CENTER_HZ, stride, false, &stop).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("invalid dense spectrum event grid")
    );
    stop.store(true, Relaxed);
    let error = locate(&powers, EventBand::Inner, CENTER_HZ, stride, false, &stop).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("spectrum event detection interrupted")
    );
}
