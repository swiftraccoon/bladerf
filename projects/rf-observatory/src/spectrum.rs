//! Bounded averaged spectra from ordinary hosted-image SC16 Q11 recordings.
//!
//! Windows may be distributed across a bounded prefix or tiled across its
//! complete FFT windows. Candidate centers rank local spectral energy on a
//! 100 kHz grid. DC-aware regions identify relative spectral activity. Neither
//! establishes protocol identity, calibrated power or RF occupancy.

mod events;

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::Serialize;

pub(crate) const FFT: usize = 8192;
const POWER_FLOOR: f64 = 1e-24;
const CELL_WIDTH_HZ: f64 = 100_000.0;
const USABLE_HALF_SPAN_HZ: f64 = 8_000_000.0;
const CENTER_GUARD_HZ: f64 = 100_000.0;
const CANDIDATE_HALF_WIDTH_HZ: f64 = 80_000.0;
const ACTIVITY_THRESHOLD_DB: f64 = 6.0;
const CELL_COUNT: usize = EventBand::Inner.cell_count();
const NOMINAL_HALF_SPAN_HZ: u64 = crate::SAMPLE_RATE as u64 / 2;

#[derive(Clone, Copy, Debug)]
pub(crate) enum EventBand {
    Inner,
    Full,
}

impl EventBand {
    pub(crate) const fn half_span_hz(self) -> u64 {
        match self {
            Self::Inner => USABLE_HALF_SPAN_HZ as u64,
            Self::Full => NOMINAL_HALF_SPAN_HZ,
        }
    }

    pub(crate) const fn cell_count(self) -> usize {
        (2 * self.half_span_hz() / CELL_WIDTH_HZ as u64) as usize
    }

    pub(crate) fn from_half_span_hz(half_span_hz: u64) -> Option<Self> {
        [Self::Inner, Self::Full]
            .into_iter()
            .find(|band| band.half_span_hz() == half_span_hz)
    }

    fn cell_for_shifted_bin(self, index: usize) -> Option<usize> {
        let offset = (index as f64 - FFT as f64 / 2.0) * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        let half_span_hz = self.half_span_hz() as f64;
        (-half_span_hz..half_span_hz)
            .contains(&offset)
            .then(|| ((offset + half_span_hz) / CELL_WIDTH_HZ).floor() as usize)
    }
}

#[derive(Clone, Copy)]
enum WindowLayout {
    Distributed(usize),
    Contiguous,
    ContiguousWithEvents {
        band: EventBand,
        stride: EventStride,
        include_active_window_ranges: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum EventStride {
    Contiguous,
    HalfWindow,
}

impl EventStride {
    pub(crate) const fn samples(self) -> usize {
        match self {
            Self::Contiguous => FFT,
            Self::HalfWindow => FFT / 2,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Bin {
    pub(crate) frequency_hz: f64,
    pub(crate) relative_power_db: f64,
}

#[derive(Debug, Serialize)]
pub(crate) struct ActivityCell {
    start_frequency_hz: f64,
    end_frequency_hz: f64,
    mean_relative_power_db: f64,
    mean_excess_over_frequency_baseline_db: f64,
    median_window_power_db: f64,
    peak_window_power_db: f64,
    peak_excess_over_temporal_median_db: f64,
    windows_above_threshold: usize,
    sampled_window_hit_fraction: f64,
    excluded_reason: Option<&'static str>,
    activity_triggered: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ActivityRegion {
    pub(crate) lower_frequency_hz: f64,
    pub(crate) upper_frequency_hz: f64,
    pub(crate) cell_count: usize,
    pub(crate) strongest_trigger_cell_frequency_hz: f64,
    pub(crate) maximum_mean_excess_db: f64,
    pub(crate) maximum_transient_excess_db: f64,
    pub(crate) maximum_sampled_window_hit_fraction: f64,
    pub(crate) classification: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct ActivityDetection {
    method: &'static str,
    cell_width_hz: f64,
    usable_half_span_hz: f64,
    center_guard_hz: f64,
    threshold_db: f64,
    temporal_baseline: &'static str,
    frequency_baseline: &'static str,
    frequency_baseline_relative_power_db: f64,
    region_peak_rule: &'static str,
    sampled_window_seconds: f64,
    sampled_seconds: f64,
    sampled_fraction_of_inspected_prefix: f64,
    cells: Vec<ActivityCell>,
    pub(crate) regions: Vec<ActivityRegion>,
    interpretation: &'static str,
}

#[derive(Debug, Serialize)]
struct ContiguousWindowExtent {
    stride_samples: usize,
    analyzed_samples: u64,
    unprocessed_tail_samples: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) schema_version: u32,
    pub(crate) operation: &'static str,
    sample_rate_hz: u32,
    center_frequency_hz: u64,
    input_samples: u64,
    inspected_prefix_samples: u64,
    fft_size: usize,
    pub(crate) windows: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    contiguous_window_extent: Option<ContiguousWindowExtent>,
    window: &'static str,
    power_note: &'static str,
    pub(crate) bins: Vec<Bin>,
    pub(crate) candidates: Vec<Bin>,
    pub(crate) activity_detection: ActivityDetection,
}

#[derive(Debug, Serialize)]
pub(crate) struct EventReport {
    schema_version: u32,
    operation: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    source_format: &'static str,
    input_samples: u64,
    analyzed_samples: u64,
    analyzed_prefix_sha256: String,
    window_samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_stride_samples: Option<usize>,
    cell_width_hz: u64,
    usable_half_span_hz: u64,
    center_guard_hz: u64,
    minimum_excess_over_temporal_median_db: f64,
    event_limit: usize,
    connectivity: &'static str,
    interpretation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_contrast_method: Option<&'static str>,
    #[serde(flatten)]
    detection: events::Detection,
}

fn power_db(power: f64) -> f64 {
    10.0 * power.max(POWER_FLOOR).log10()
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn quantile(values: &[f64], fraction: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = fraction * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

fn overlaps(low: f64, high: f64, other_low: f64, other_high: f64) -> bool {
    low < other_high && high > other_low
}

fn activity_regions(cells: &[ActivityCell]) -> Vec<ActivityRegion> {
    let mut regions = Vec::new();
    let mut index = 0;
    while index < cells.len() {
        if !cells[index].activity_triggered {
            index += 1;
            continue;
        }
        let start = index;
        while index + 1 < cells.len() && cells[index + 1].activity_triggered {
            index += 1;
        }
        let end = index;
        let selected = &cells[start..=end];
        let peak = selected
            .iter()
            .max_by(|a, b| {
                a.mean_excess_over_frequency_baseline_db
                    .max(a.peak_excess_over_temporal_median_db)
                    .total_cmp(
                        &b.mean_excess_over_frequency_baseline_db
                            .max(b.peak_excess_over_temporal_median_db),
                    )
            })
            .expect("active region is nonempty");
        regions.push(ActivityRegion {
            lower_frequency_hz: selected[0].start_frequency_hz,
            upper_frequency_hz: selected[selected.len() - 1].end_frequency_hz,
            cell_count: selected.len(),
            strongest_trigger_cell_frequency_hz: (peak.start_frequency_hz + peak.end_frequency_hz)
                / 2.0,
            maximum_mean_excess_db: selected
                .iter()
                .map(|cell| cell.mean_excess_over_frequency_baseline_db)
                .max_by(f64::total_cmp)
                .expect("active region is nonempty"),
            maximum_transient_excess_db: selected
                .iter()
                .map(|cell| cell.peak_excess_over_temporal_median_db)
                .max_by(f64::total_cmp)
                .expect("active region is nonempty"),
            maximum_sampled_window_hit_fraction: selected
                .iter()
                .map(|cell| cell.sampled_window_hit_fraction)
                .max_by(f64::total_cmp)
                .expect("active region is nonempty"),
            classification: "unclassified_spectral_activity",
        });
        index += 1;
    }
    regions
}

fn activity_detection(
    center: u64,
    inspected_samples: u64,
    windows: usize,
    cell_window_powers: &[Vec<f64>],
    layout: WindowLayout,
    stop: &AtomicBool,
) -> Result<ActivityDetection> {
    let center = center as f64;
    let guard_low = center - CENTER_GUARD_HZ;
    let guard_high = center + CENTER_GUARD_HZ;
    let mut means = Vec::with_capacity(CELL_COUNT);
    let mut excluded = Vec::with_capacity(CELL_COUNT);
    for (index, powers) in cell_window_powers.iter().enumerate() {
        let start = center - USABLE_HALF_SPAN_HZ + index as f64 * CELL_WIDTH_HZ;
        let end = start + CELL_WIDTH_HZ;
        means.push(powers.iter().sum::<f64>() / windows as f64);
        excluded.push(overlaps(start, end, guard_low, guard_high));
    }
    let usable_means: Vec<_> = means
        .iter()
        .zip(&excluded)
        .filter_map(|(mean, excluded)| (!excluded).then_some(*mean))
        .collect();
    let frequency_baseline = quantile(&usable_means, 0.25);
    let frequency_baseline_db = power_db(frequency_baseline);
    let cells: Vec<_> = cell_window_powers
        .iter()
        .enumerate()
        .map(|(index, powers)| {
            ensure!(!stop.load(Ordering::Relaxed), "spectrum interrupted");
            let start = center - USABLE_HALF_SPAN_HZ + index as f64 * CELL_WIDTH_HZ;
            let end = start + CELL_WIDTH_HZ;
            let mean_db = power_db(means[index]);
            let median_power = median(powers);
            let median_db = power_db(median_power);
            let peak_db = power_db(
                powers
                    .iter()
                    .copied()
                    .max_by(f64::total_cmp)
                    .expect("each cell has one value per FFT window"),
            );
            let mean_excess = mean_db - frequency_baseline_db;
            let peak_excess = peak_db - median_db;
            let hits = powers
                .iter()
                .filter(|power| power_db(**power) - median_db >= ACTIVITY_THRESHOLD_DB)
                .count();
            let excluded_reason = excluded[index].then_some("center_guard");
            Ok(ActivityCell {
                start_frequency_hz: start,
                end_frequency_hz: end,
                mean_relative_power_db: mean_db,
                mean_excess_over_frequency_baseline_db: mean_excess,
                median_window_power_db: median_db,
                peak_window_power_db: peak_db,
                peak_excess_over_temporal_median_db: peak_excess,
                windows_above_threshold: hits,
                sampled_window_hit_fraction: hits as f64 / windows as f64,
                excluded_reason,
                activity_triggered: excluded_reason.is_none()
                    && (mean_excess >= ACTIVITY_THRESHOLD_DB
                        || peak_excess >= ACTIVITY_THRESHOLD_DB),
            })
        })
        .collect::<Result<_>>()?;
    let regions = activity_regions(&cells);
    let sampled_window_seconds = FFT as f64 / f64::from(crate::SAMPLE_RATE);
    let sampled_seconds = sampled_window_seconds * windows as f64;
    let (method, interpretation) = match layout {
        WindowLayout::Distributed(_) => (
            "distributed_Hann_FFT_with_per_window_weighted_DC_removal_and_100_kHz_cells",
            "Relative heuristic triggers from sparsely sampled FFT windows. A hit fraction is not channel occupancy. Regions may be analog signals, digital signals, noise, interference or receiver artifacts; protocol identity requires independent decoder evidence. Center-guard activity is excluded and needs an offset retune for inference.",
        ),
        WindowLayout::Contiguous | WindowLayout::ContiguousWithEvents { .. } => (
            "contiguous_Hann_FFT_with_per_window_weighted_DC_removal_and_100_kHz_cells",
            "Relative heuristic triggers from consecutive complete FFT windows in the selected file prefix. The inspection establishes neither RF continuity nor channel occupancy. Regions may be analog signals, digital signals, noise, interference or receiver artifacts. Protocol identity requires independent decoder evidence, and the center guard needs an offset retune.",
        ),
    };
    Ok(ActivityDetection {
        method,
        cell_width_hz: CELL_WIDTH_HZ,
        usable_half_span_hz: USABLE_HALF_SPAN_HZ,
        center_guard_hz: CENTER_GUARD_HZ,
        threshold_db: ACTIVITY_THRESHOLD_DB,
        temporal_baseline: "per_cell_median",
        frequency_baseline: "lower_quartile_of_usable_cell_means",
        frequency_baseline_relative_power_db: frequency_baseline_db,
        region_peak_rule: "cell_center_maximizing_the_larger_of_mean_frequency_excess_and_peak_temporal_excess",
        sampled_window_seconds,
        sampled_seconds,
        sampled_fraction_of_inspected_prefix: (windows * FFT) as f64 / inspected_samples as f64,
        cells,
        regions,
        interpretation,
    })
}

pub(crate) fn survey(
    path: &Path,
    center: u64,
    max_samples: u64,
    requested_windows: usize,
    stop: &AtomicBool,
) -> Result<Report> {
    survey_with_layout(
        path,
        center,
        max_samples,
        WindowLayout::Distributed(requested_windows),
        stop,
    )
    .map(|(report, _)| report)
}

pub(crate) fn survey_dense(
    path: &Path,
    center: u64,
    max_samples: u64,
    stop: &AtomicBool,
) -> Result<Report> {
    survey_with_layout(path, center, max_samples, WindowLayout::Contiguous, stop)
        .map(|(report, _)| report)
}

pub(crate) fn survey_with_events(
    path: &Path,
    center: u64,
    max_samples: u64,
    band: EventBand,
    stride: EventStride,
    include_active_window_ranges: bool,
    stop: &AtomicBool,
) -> Result<(Report, EventReport)> {
    let (report, events) = survey_with_layout(
        path,
        center,
        max_samples,
        WindowLayout::ContiguousWithEvents {
            band,
            stride,
            include_active_window_ranges,
        },
        stop,
    )?;
    Ok((
        report,
        events.context("dense event report was not produced")?,
    ))
}

fn survey_with_layout(
    path: &Path,
    center: u64,
    max_samples: u64,
    layout: WindowLayout,
    stop: &AtomicBool,
) -> Result<(Report, Option<EventReport>)> {
    ensure!(
        (1..=crate::MAX_SAMPLES).contains(&max_samples),
        "invalid spectrum sample limit"
    );
    if let WindowLayout::Distributed(requested) = layout {
        ensure!(
            (1..=512).contains(&requested),
            "invalid spectrum window count"
        );
    }
    let (mut file, available) = crate::receiver::input(path)?;
    let locate_events = matches!(layout, WindowLayout::ContiguousWithEvents { .. });
    let (band, event_stride, include_active_window_ranges) = match layout {
        WindowLayout::ContiguousWithEvents {
            band,
            stride,
            include_active_window_ranges,
        } => (band, stride, include_active_window_ranges),
        _ => (EventBand::Inner, EventStride::Contiguous, false),
    };
    let event_input_metadata = locate_events.then(|| file.metadata()).transpose()?;
    ensure!(
        event_input_metadata.as_ref().is_none_or(
            |metadata| metadata.len() / 4 == available && metadata.len().is_multiple_of(4)
        ),
        "event survey IQ extent changed before analysis"
    );
    let mut event_input_digest = locate_events.then(|| digest::Context::new(&SHA256));
    let samples = available.min(max_samples);
    ensure!(
        samples >= FFT as u64,
        "spectrum requires at least {FFT} samples"
    );
    let complete_windows = samples as usize / FFT;
    let windows = match layout {
        WindowLayout::Distributed(requested) => requested.min(complete_windows),
        WindowLayout::Contiguous | WindowLayout::ContiguousWithEvents { .. } => complete_windows,
    };
    let stride_samples = event_stride.samples();
    let overlap = stride_samples < FFT;
    let processed_windows = if overlap {
        (samples as usize - FFT) / stride_samples + 1
    } else {
        windows
    };
    let hann: Vec<_> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (FFT - 1) as f64).cos())
        .collect();
    let hann_sum = hann.iter().sum::<f64>();
    let norm = (hann_sum * 2048.0).powi(2);
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(FFT);
    let mut buffer = vec![Complex::new(0.0, 0.0); FFT];
    let mut scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut raw = vec![0u8; FFT * 4];
    let mut powers = vec![0.0; FFT];
    let shifted_cells: Vec<_> = (0..FFT).map(|bin| band.cell_for_shifted_bin(bin)).collect();
    let cell_count = band.cell_count();
    let mut cell_bin_counts = vec![0usize; cell_count];
    for cell in shifted_cells.iter().flatten() {
        cell_bin_counts[*cell] += 1;
    }
    ensure!(
        cell_bin_counts.iter().all(|count| *count > 0),
        "activity cell has no FFT bins"
    );
    let mut cell_window_powers: Vec<Vec<f64>> = (0..cell_count)
        .map(|_| Vec::with_capacity(windows))
        .collect();
    let mut overlapping_event_powers = overlap.then(|| {
        (0..cell_count)
            .map(|_| Vec::with_capacity(processed_windows))
            .collect::<Vec<_>>()
    });
    for window in 0..processed_windows {
        ensure!(!stop.load(Ordering::Relaxed), "spectrum interrupted");
        if matches!(layout, WindowLayout::Distributed(_)) && windows > 1 {
            let offset = window as u64 * (samples - FFT as u64) / (windows - 1) as u64;
            file.seek(SeekFrom::Start(offset * 4))?;
        }
        // Hash only the fresh suffix after reusing the shared half of a window.
        let retained_bytes = if overlap && window > 0 {
            raw.copy_within(stride_samples * 4.., 0);
            (FFT - stride_samples) * 4
        } else {
            0
        };
        let fresh = &mut raw[retained_bytes..];
        file.read_exact(fresh)?;
        // Reused bytes were validated in the previous window. Reduce the range
        // check over the fresh chunk so the compiler can vectorize it.
        let in_range = fresh.as_chunks::<2>().0.iter().fold(true, |valid, raw| {
            valid & (-2048..=2047).contains(&i16::from_le_bytes(*raw))
        });
        ensure!(in_range, "input contains a component outside SC16 Q11");
        if let Some(digest) = &mut event_input_digest {
            digest.update(fresh);
        }
        let mut weighted_sum = Complex::new(0.0, 0.0);
        for (i, &[i_low, i_high, q_low, q_high]) in raw.as_chunks::<4>().0.iter().enumerate() {
            let real = f64::from(i16::from_le_bytes([i_low, i_high]));
            let imaginary = f64::from(i16::from_le_bytes([q_low, q_high]));
            buffer[i] = Complex::new(real, imaginary);
            weighted_sum += buffer[i] * hann[i];
        }
        let weighted_mean = weighted_sum / hann_sum;
        for (value, weight) in buffer.iter_mut().zip(&hann) {
            *value = (*value - weighted_mean) * *weight;
        }
        fft.process_with_scratch(&mut buffer, &mut scratch);
        let summary_window = window.is_multiple_of(FFT / stride_samples);
        let mut window_cell_sums = [0.0f64; EventBand::Full.cell_count()];
        for (shifted, cell) in shifted_cells.iter().enumerate() {
            let source = (shifted + FFT / 2) % FFT;
            let bin_power = buffer[source].norm_sqr() / norm;
            if summary_window {
                powers[source] += bin_power / windows as f64;
            }
            if let Some(cell) = cell {
                window_cell_sums[*cell] += bin_power;
            }
        }
        for (index, sum) in window_cell_sums.into_iter().take(cell_count).enumerate() {
            let mean = sum / cell_bin_counts[index] as f64;
            if summary_window {
                cell_window_powers[index].push(mean);
            }
            if let Some(powers) = &mut overlapping_event_powers {
                powers[index].push(mean);
            }
        }
    }
    if let Some(before) = event_input_metadata {
        let after = file.metadata()?;
        ensure!(
            after.len() == before.len()
                && after.mtime() == before.mtime()
                && after.mtime_nsec() == before.mtime_nsec()
                && after.ctime() == before.ctime()
                && after.ctime_nsec() == before.ctime_nsec(),
            "event survey IQ input changed during analysis"
        );
    }
    let mut linear = Vec::with_capacity(FFT);
    let mut bins = Vec::with_capacity(FFT);
    for i in 0..FFT {
        let source = (i + FFT / 2) % FFT;
        let frequency = center as f64
            + (i as f64 - FFT as f64 / 2.0) * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        linear.push((frequency, powers[source]));
        bins.push(Bin {
            frequency_hz: frequency,
            relative_power_db: power_db(powers[source]),
        });
    }
    let mut ranked = Vec::<(f64, f64)>::new();
    let low = ((center as f64 - 8_000_000.0) / 100_000.0).ceil() as i64;
    let high = ((center as f64 + 8_000_000.0) / 100_000.0).floor() as i64;
    for grid in low..=high {
        let frequency = grid as f64 * 100_000.0;
        if overlaps(
            frequency - CANDIDATE_HALF_WIDTH_HZ,
            frequency + CANDIDATE_HALF_WIDTH_HZ,
            center as f64 - CENTER_GUARD_HZ,
            center as f64 + CENTER_GUARD_HZ,
        ) {
            continue;
        }
        let power = linear
            .iter()
            .filter(|(f, _)| (*f - frequency).abs() <= CANDIDATE_HALF_WIDTH_HZ)
            .map(|(_, p)| p)
            .sum::<f64>();
        ranked.push((frequency, power));
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut candidates: Vec<Bin> = Vec::new();
    for (frequency, power) in ranked {
        if candidates
            .iter()
            .any(|other| (other.frequency_hz - frequency).abs() < 200_000.0)
        {
            continue;
        }
        candidates.push(Bin {
            frequency_hz: frequency,
            relative_power_db: power_db(power),
        });
        if candidates.len() == 12 {
            break;
        }
    }
    // Both grids use the same 100 kHz boundaries in the inner band.
    let inner_start = (cell_count - CELL_COUNT) / 2;
    let inner_powers = &cell_window_powers[inner_start..inner_start + CELL_COUNT];
    let activity_detection =
        activity_detection(center, samples, windows, inner_powers, layout, stop)?;
    let event_report = if let Some(digest) = event_input_digest {
        let event_powers = overlapping_event_powers
            .as_deref()
            .unwrap_or(&cell_window_powers);
        let detection = events::locate(
            event_powers,
            band,
            center,
            event_stride,
            include_active_window_ranges,
            stop,
        )?;
        Some(EventReport {
            schema_version: match (include_active_window_ranges, overlap) {
                (true, _) => 3,
                (false, true) => 2,
                (false, false) => 1,
            },
            operation: "offline_rf_spectrum_events",
            center_frequency_hz: center,
            sample_rate_hz: crate::SAMPLE_RATE,
            source_format: "SC16_Q11_LE",
            input_samples: available,
            analyzed_samples: ((processed_windows - 1) * stride_samples + FFT) as u64,
            analyzed_prefix_sha256: HEXLOWER.encode(digest.finish().as_ref()),
            window_samples: FFT,
            window_stride_samples: (overlap || include_active_window_ranges)
                .then_some(stride_samples),
            cell_width_hz: CELL_WIDTH_HZ as u64,
            usable_half_span_hz: band.half_span_hz(),
            center_guard_hz: CENTER_GUARD_HZ as u64,
            minimum_excess_over_temporal_median_db: ACTIVITY_THRESHOLD_DB,
            event_limit: events::MAX_EVENTS,
            connectivity: "orthogonal_neighbors_in_sample_window_and_frequency_cell",
            interpretation: "Bounds enclose connected relative-power hits in saved IQ coordinates. Individual tiles inside a bounding box can be below threshold. The frequency axis uses the supplied center label.",
            narrow_contrast_method: include_active_window_ranges
                .then_some("mean_target_excess_minus_stronger_adjacent_band"),
            detection,
        })
    } else {
        None
    };
    let (operation, contiguous_window_extent) = match layout {
        WindowLayout::Distributed(_) => ("offline_rf_spectrum_survey", None),
        WindowLayout::Contiguous | WindowLayout::ContiguousWithEvents { .. } => {
            let analyzed_samples = (windows * FFT) as u64;
            (
                "offline_rf_dense_spectrum_survey",
                Some(ContiguousWindowExtent {
                    stride_samples: FFT,
                    analyzed_samples,
                    unprocessed_tail_samples: samples - analyzed_samples,
                }),
            )
        }
    };
    let report = Report {
        schema_version: 1,
        operation,
        sample_rate_hz: crate::SAMPLE_RATE,
        center_frequency_hz: center,
        input_samples: available,
        inspected_prefix_samples: samples,
        fft_size: FFT,
        windows,
        contiguous_window_extent,
        window: "symmetric_Hann_after_per_window_weighted_DC_removal",
        power_note: "Relative complex FFT-bin power after per-window Hann-weighted DC removal, with nominal Q11 scale and coherent-window normalization; not calibrated dBm. Candidate values integrate +/-80 kHz, exclude integrations overlapping the +/-100 kHz center guard, and are not directly comparable with one bin.",
        bins,
        candidates,
        activity_detection,
    };
    Ok((report, event_report))
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use std::io::Write;

    fn analytic(bin: f64, sample: usize, amplitude: f64) -> Complex<f64> {
        let phase = std::f64::consts::TAU * bin * sample as f64 / FFT as f64;
        Complex::new(amplitude * phase.cos(), amplitude * phase.sin())
    }

    fn noise(window: usize, sample: usize, amplitude: f64) -> Complex<f64> {
        let mut value = ((window * FFT + sample) as u64 + 1) * 0x9e37_79b9;
        value ^= value >> 13;
        value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value ^= value >> 29;
        let i = ((value & 0xffff) as f64 / 32767.5) - 1.0;
        let q = (((value >> 16) & 0xffff) as f64 / 32767.5) - 1.0;
        Complex::new(i * amplitude, q * amplitude)
    }

    fn write_iq(path: &Path, windows: usize, mut sample: impl FnMut(usize, usize) -> Complex<f64>) {
        let mut file = std::fs::File::create(path).unwrap();
        for window in 0..windows {
            for index in 0..FFT {
                let value = sample(window, index);
                let i = value.re.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                let q = value.im.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                file.write_all(&i.to_le_bytes()).unwrap();
                file.write_all(&q.to_le_bytes()).unwrap();
            }
        }
    }

    fn cell_at(report: &Report, frequency: f64) -> &ActivityCell {
        report
            .activity_detection
            .cells
            .iter()
            .find(|cell| cell.start_frequency_hz <= frequency && frequency < cell.end_frequency_hz)
            .unwrap()
    }

    fn example_cell(index: usize, triggered: bool, mean: f64, transient: f64) -> ActivityCell {
        ActivityCell {
            start_frequency_hz: index as f64 * CELL_WIDTH_HZ,
            end_frequency_hz: (index + 1) as f64 * CELL_WIDTH_HZ,
            mean_relative_power_db: mean,
            mean_excess_over_frequency_baseline_db: mean,
            median_window_power_db: -80.0,
            peak_window_power_db: -80.0 + transient,
            peak_excess_over_temporal_median_db: transient,
            windows_above_threshold: usize::from(triggered),
            sampled_window_hit_fraction: if triggered { 0.25 } else { 0.0 },
            excluded_reason: None,
            activity_triggered: triggered,
        }
    }

    #[test]
    fn independent_analytic_tone_is_in_the_expected_shifted_bin() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tone.iq");
        write_iq(&path, 1, |_window, n| analytic(100.0, n, 1000.0));
        let report = survey(&path, 98_000_000, FFT as u64, 1, &AtomicBool::new(false)).unwrap();
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.operation, "offline_rf_spectrum_survey");
        let serialized = serde_json::to_value(&report).unwrap();
        assert_eq!(serialized["schema_version"], 1);
        assert_eq!(serialized["operation"], "offline_rf_spectrum_survey");
        let peak = report
            .bins
            .iter()
            .max_by(|a, b| a.relative_power_db.total_cmp(&b.relative_power_db))
            .unwrap();
        assert!(
            (peak.frequency_hz - (98_000_000.0 + 100.0 * 20_000_000.0 / FFT as f64)).abs() < 0.01
        );
        assert!((peak.relative_power_db - 20.0 * (1000.0f64 / 2048.0).log10()).abs() < 0.02);
    }

    #[test]
    fn weighted_dc_removal_suppresses_center_and_preserves_weaker_tone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dc-tone.iq");
        let center = 2_437_000_000u64;
        let tone_bin = 640.0;
        let tone_frequency = center as f64 + tone_bin * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        write_iq(&path, 4, |window, n| {
            Complex::new(900.0, -700.0) + analytic(tone_bin, n, 300.0) + noise(window, n, 3.0)
        });
        let report = survey(&path, center, (4 * FFT) as u64, 4, &AtomicBool::new(false)).unwrap();
        let peak = report
            .bins
            .iter()
            .max_by(|a, b| a.relative_power_db.total_cmp(&b.relative_power_db))
            .unwrap();
        assert!((peak.frequency_hz - tone_frequency).abs() < 0.01);
        let center_bin = &report.bins[FFT / 2];
        assert!(peak.relative_power_db - center_bin.relative_power_db > 60.0);
        assert!(report.candidates.iter().all(|candidate| {
            !overlaps(
                candidate.frequency_hz - CANDIDATE_HALF_WIDTH_HZ,
                candidate.frequency_hz + CANDIDATE_HALF_WIDTH_HZ,
                center as f64 - CENTER_GUARD_HZ,
                center as f64 + CENTER_GUARD_HZ,
            )
        }));
        assert!(report.activity_detection.regions.iter().any(|region| {
            region.lower_frequency_hz <= tone_frequency
                && tone_frequency < region.upper_frequency_hz
        }));
    }

    #[test]
    fn center_only_dc_produces_no_activity_and_finite_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dc.iq");
        write_iq(&path, 4, |_window, _n| Complex::new(1000.0, -800.0));
        let report = survey(
            &path,
            100_000_000,
            (4 * FFT) as u64,
            4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.activity_detection.regions.is_empty());
        assert_eq!(
            report
                .activity_detection
                .cells
                .iter()
                .filter(|cell| cell.excluded_reason == Some("center_guard"))
                .count(),
            2
        );
        assert!(
            report
                .activity_detection
                .cells
                .iter()
                .all(|cell| !cell.activity_triggered)
        );
        serde_json::to_value(&report).unwrap();
    }

    #[test]
    fn persistent_tone_uses_frequency_baseline_and_sampling_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("persistent.iq");
        let center = 100_000_000u64;
        let tone_bin = -700.0;
        let tone_frequency = center as f64 + tone_bin * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        write_iq(&path, 8, |window, n| {
            analytic(tone_bin, n, 500.0) + noise(window, n, 8.0)
        });
        let report = survey(&path, center, (8 * FFT) as u64, 4, &AtomicBool::new(false)).unwrap();
        let cell = cell_at(&report, tone_frequency);
        assert!(cell.activity_triggered);
        assert!(cell.mean_excess_over_frequency_baseline_db > ACTIVITY_THRESHOLD_DB);
        assert!(cell.peak_excess_over_temporal_median_db < ACTIVITY_THRESHOLD_DB);
        assert_eq!(report.activity_detection.sampled_window_seconds, 0.0004096);
        assert_eq!(report.activity_detection.sampled_seconds, 0.0016384);
        assert_eq!(
            report
                .activity_detection
                .sampled_fraction_of_inspected_prefix,
            0.5
        );
    }

    #[test]
    fn transient_tone_reports_exact_sparse_window_hits() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("burst.iq");
        let center = 100_000_000u64;
        let tone_bin = 900.0;
        let tone_frequency = center as f64 + tone_bin * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        write_iq(&path, 8, |window, n| {
            let burst = if matches!(window, 1 | 6) {
                analytic(tone_bin, n, 700.0)
            } else {
                Complex::new(0.0, 0.0)
            };
            burst + noise(window, n, 8.0)
        });
        let report = survey(&path, center, (8 * FFT) as u64, 8, &AtomicBool::new(false)).unwrap();
        let cell = cell_at(&report, tone_frequency);
        assert!(cell.activity_triggered);
        assert_eq!(cell.windows_above_threshold, 2);
        assert_eq!(cell.sampled_window_hit_fraction, 0.25);
        assert!(cell.peak_excess_over_temporal_median_db > 20.0);
    }

    #[test]
    fn dense_survey_finds_transient_between_sparse_windows_and_accounts_for_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("burst.iq");
        let center = 100_000_000;
        let tone_bin = 900.0;
        let tone_frequency = center as f64 + tone_bin * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        write_iq(&path, 9, |window, n| {
            let background = noise(window, n, 8.0);
            if window == 3 {
                background + analytic(tone_bin, n, 700.0)
            } else {
                background
            }
        });
        let stop = AtomicBool::new(false);
        let prefix_samples = (8 * FFT + 11) as u64;
        // The two distributed endpoint windows both miss the burst in window 3.
        let sparse = survey(&path, center, prefix_samples, 2, &stop).unwrap();
        let sparse_cell = cell_at(&sparse, tone_frequency);
        assert!(!sparse_cell.activity_triggered);
        assert_eq!(sparse_cell.windows_above_threshold, 0);
        assert!(
            serde_json::to_value(&sparse)
                .unwrap()
                .get("contiguous_window_extent")
                .is_none()
        );

        let dense = survey_dense(&path, center, prefix_samples, &stop).unwrap();
        let dense_cell = cell_at(&dense, tone_frequency);
        assert!(dense_cell.activity_triggered);
        assert_eq!(dense_cell.windows_above_threshold, 1);
        assert_eq!(dense_cell.sampled_window_hit_fraction, 0.125);
        let report = serde_json::to_value(&dense).unwrap();
        assert_eq!(report["operation"], "offline_rf_dense_spectrum_survey");
        assert_eq!(report["input_samples"], 9 * FFT);
        assert_eq!(report["inspected_prefix_samples"], prefix_samples);
        assert_eq!(report["windows"], 8);
        let extent = &report["contiguous_window_extent"];
        assert_eq!(extent["stride_samples"], FFT);
        assert_eq!(extent["analyzed_samples"], 8 * FFT);
        assert_eq!(extent["unprocessed_tail_samples"], 11);

        let (with_events, events) = survey_with_events(
            &path,
            center,
            prefix_samples,
            EventBand::Inner,
            EventStride::Contiguous,
            false,
            &stop,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(with_events).unwrap(), report);
        let event_report = serde_json::to_value(events).unwrap();
        assert_eq!(event_report["analyzed_samples"], 8 * FFT);
        let file_bytes = std::fs::read(path).unwrap();
        assert_eq!(
            event_report["analyzed_prefix_sha256"],
            format!("{:x}", Sha256::digest(&file_bytes[..8 * FFT * 4]))
        );
        assert!(event_report["events"].as_array().unwrap().iter().any(
            |event| event["start_sample"] == 3 * FFT
                && event["end_sample_exclusive"] == 4 * FFT
                && event["lower_frequency_hz"].as_f64().unwrap() <= tone_frequency
                && tone_frequency < event["upper_frequency_hz_exclusive"].as_f64().unwrap()
        ));
    }

    #[test]
    fn overlapping_survey_checks_the_new_half_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.iq");
        let samples = FFT + FFT / 2;
        let mut iq = vec![0; samples * 4];
        iq[samples * 4 - 2..].copy_from_slice(&2048_i16.to_le_bytes());
        std::fs::write(&path, iq).unwrap();

        let error = survey_with_events(
            &path,
            100_000_000,
            samples as u64,
            EventBand::Full,
            EventStride::HalfWindow,
            true,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "input contains a component outside SC16 Q11"
        );
    }

    #[test]
    fn overlapping_events_recover_a_short_burst_at_a_window_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("boundary.iq");
        let center = 100_000_000;
        let tone_bin = 900.0;
        let frequency = center as f64 + tone_bin * f64::from(crate::SAMPLE_RATE) / FFT as f64;
        let boundary = 4 * FFT;
        write_iq(&path, 9, |window, n| {
            let background = noise(window, n, 8.0);
            if (window * FFT + n).abs_diff(boundary) < 64 {
                background + analytic(tone_bin, n, 700.0)
            } else {
                background
            }
        });
        let stride = FFT / 2;
        let analyzed = 8 * FFT + stride;
        let prefix = (analyzed + 11) as u64;
        let stop = AtomicBool::new(false);
        let run = |stride| {
            survey_with_events(&path, center, prefix, EventBand::Full, stride, false, &stop)
        };
        let (contiguous, standard) = run(EventStride::Contiguous).unwrap();
        let (overlapped, overlapping) = run(EventStride::HalfWindow).unwrap();
        assert_eq!(
            serde_json::to_vec(&contiguous).unwrap(),
            serde_json::to_vec(&overlapped).unwrap()
        );
        let standard = serde_json::to_value(standard).unwrap();
        let overlapping = serde_json::to_value(overlapping).unwrap();
        let covers_burst = |event: &serde_json::Value| {
            event["start_sample"].as_u64().unwrap() <= boundary as u64
                && event["end_sample_exclusive"].as_u64().unwrap() > boundary as u64
                && event["lower_frequency_hz"].as_f64().unwrap() <= frequency
                && event["upper_frequency_hz_exclusive"].as_f64().unwrap() > frequency
        };
        assert!(
            !standard["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(covers_burst)
        );
        assert!(
            overlapping["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(covers_burst)
        );
        assert!(standard.get("window_stride_samples").is_none());
        assert!(standard.get("active_window_ranges").is_none());
        assert!(standard.get("narrow_contrast_method").is_none());
        assert!(standard.get("narrow_search").is_none());
        assert_eq!(overlapping["schema_version"], 2);
        assert_eq!(overlapping["window_stride_samples"], stride);
        assert!(overlapping.get("active_window_ranges").is_none());
        assert!(overlapping.get("narrow_contrast_method").is_none());
        assert!(overlapping.get("narrow_search").is_none());
        assert_eq!(overlapping["analyzed_samples"], analyzed);
        assert_eq!(overlapping["windows"], 16);
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(
            overlapping["analyzed_prefix_sha256"],
            format!("{:x}", Sha256::digest(&bytes[..analyzed * 4]))
        );
    }

    #[test]
    fn active_window_reports_declare_both_strides_even_without_hits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quiet.iq");
        let stop = AtomicBool::new(false);
        write_iq(&path, 2, |_, _| Complex::new(0.0, 0.0));

        for stride in [EventStride::Contiguous, EventStride::HalfWindow] {
            let (_, report) = survey_with_events(
                &path,
                100_000_000,
                (2 * FFT) as u64,
                EventBand::Inner,
                stride,
                true,
                &stop,
            )
            .unwrap();
            let report = serde_json::to_value(report).unwrap();
            assert_eq!(report["schema_version"], 3);
            assert_eq!(report["window_stride_samples"], stride.samples());
            assert_eq!(report["active_window_count"], 0);
            assert_eq!(report["active_window_ranges"], serde_json::json!([]));
            assert_eq!(
                report["narrow_contrast_method"],
                "mean_target_excess_minus_stronger_adjacent_band"
            );
            assert_eq!(report["narrow_search"]["method"], "fixed_band_runs");
            assert_eq!(report["narrow_search"]["candidate_count"], 0);
            assert_eq!(report["narrow_search"]["candidates"], serde_json::json!([]));
        }
    }

    #[test]
    fn full_event_grid_finds_both_nyquist_edge_tones_in_one_window() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("band-edges.iq");
        let center = 100_000_000u64;
        let stop = AtomicBool::new(false);
        let count = (8 * FFT) as u64;
        let band = EventBand::Full;
        assert_eq!(band.cell_for_shifted_bin(0), Some(0));
        assert_eq!(band.cell_for_shifted_bin(FFT - 1), Some(199));
        assert_eq!(band.cell_for_shifted_bin(FFT), None);
        write_iq(&path, 8, |window, n| {
            let background = noise(window, n, 8.0);
            if window == 3 {
                background + analytic(-4070.0, n, 450.0) + analytic(4070.0, n, 450.0)
            } else {
                background
            }
        });
        let (inner_spectrum, inner_events) = survey_with_events(
            &path,
            center,
            count,
            EventBand::Inner,
            EventStride::Contiguous,
            false,
            &stop,
        )
        .unwrap();
        let (full_spectrum, full_events) = survey_with_events(
            &path,
            center,
            count,
            band,
            EventStride::Contiguous,
            false,
            &stop,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&inner_spectrum).unwrap(),
            serde_json::to_vec(&full_spectrum).unwrap()
        );
        let inner = serde_json::to_value(inner_events).unwrap();
        let full = serde_json::to_value(full_events).unwrap();
        assert_eq!(full["evaluated_tile_count"], 8 * 198);
        assert_eq!(inner["evaluated_tile_count"], 8 * 158);
        assert_eq!(
            full["analyzed_prefix_sha256"],
            inner["analyzed_prefix_sha256"]
        );
        let in_window = |event: &serde_json::Value| {
            event["start_sample"] == 3 * FFT && event["end_sample_exclusive"] == 4 * FFT
        };
        let events = full["events"].as_array().unwrap();
        assert!(events.iter().any(|event| {
            in_window(event)
                && event["lower_frequency_hz"] == center - 10_000_000
                && event["upper_frequency_hz_exclusive"].as_u64().unwrap() <= center - 100_000
        }));
        assert!(events.iter().any(|event| {
            in_window(event)
                && event["upper_frequency_hz_exclusive"] == center + 10_000_000
                && event["lower_frequency_hz"].as_u64().unwrap() >= center + 100_000
        }));
    }

    #[test]
    fn adjacent_activity_merges_and_a_gap_splits_regions() {
        let cells = vec![
            example_cell(0, true, 8.0, 7.0),
            example_cell(1, true, 9.0, 12.0),
            example_cell(2, false, 0.0, 0.0),
            example_cell(3, true, 15.0, 8.0),
        ];
        let regions = activity_regions(&cells);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].lower_frequency_hz, 0.0);
        assert_eq!(regions[0].upper_frequency_hz, 200_000.0);
        assert_eq!(regions[0].cell_count, 2);
        assert_eq!(regions[0].strongest_trigger_cell_frequency_hz, 150_000.0);
        assert_eq!(regions[1].lower_frequency_hz, 300_000.0);
        assert_eq!(regions[1].upper_frequency_hz, 400_000.0);
        assert_eq!(regions[1].classification, "unclassified_spectral_activity");
    }

    #[test]
    fn bin_count_normalization_removes_forty_vs_forty_one_bin_bias() {
        let mut counts = [0usize; CELL_COUNT];
        for cell in (0..FFT).filter_map(|bin| EventBand::Inner.cell_for_shifted_bin(bin)) {
            counts[cell] += 1;
        }
        assert!(counts.contains(&40));
        assert!(counts.contains(&41));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("flat-spectrum.iq");
        write_iq(&path, 1, |_window, n| {
            if n == FFT / 2 {
                Complex::new(1000.0, 0.0)
            } else {
                Complex::new(0.0, 0.0)
            }
        });
        let report = survey(&path, 100_000_000, FFT as u64, 1, &AtomicBool::new(false)).unwrap();
        let usable: Vec<_> = report
            .activity_detection
            .cells
            .iter()
            .filter(|cell| cell.excluded_reason.is_none())
            .map(|cell| cell.mean_relative_power_db)
            .collect();
        let low = usable.iter().copied().min_by(f64::total_cmp).unwrap();
        let high = usable.iter().copied().max_by(f64::total_cmp).unwrap();
        assert!(
            high - low < 0.01,
            "normalized cell spread was {} dB",
            high - low
        );
    }
}
