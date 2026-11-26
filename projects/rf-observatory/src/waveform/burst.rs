//! Localize relative band-power rises on the original-rate IQ sample grid.

use std::f64::consts::TAU;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use clap::Args;
use data_encoding::HEXLOWER;
use rustfft::{FftPlanner, num_complex::Complex};
use serde::Serialize;

use crate::SAMPLE_RATE;

use super::{check_stop, iq, publish_report};

const MAX_INPUT_SAMPLES: usize = SAMPLE_RATE as usize;
pub(crate) const WINDOW_SAMPLES: usize = 1024;
const MIN_BASELINE_SAMPLES: usize = 8 * WINDOW_SAMPLES;
pub(crate) const THRESHOLD_DB: f64 = 6.0;
const MINIMUM_WINDOWS: usize = 2;
const MAX_BANDS: usize = 8;
const MAX_PINNED_BANDS: usize = 1_280;
const MAX_BAND_WINDOWS: usize = 8_000_000;

#[derive(Debug, Args)]
pub(crate) struct ProbeBursts {
    /// Saved 20 MSps SC16 Q11 input, up to 20,000,000 complex samples.
    input: PathBuf,
    /// RF tuning of the saved input in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Inclusive band edge in hertz.
    #[arg(long)]
    lower_hz: u64,
    /// Exclusive band edge in hertz.
    #[arg(long)]
    upper_hz: u64,
    /// Additional half-open bands measured on the same FFT grid.
    #[arg(long, value_name = "LOWER:UPPER")]
    also_band_hz: Vec<BandEdges>,
    /// Quiet prefix samples, in multiples of 1,024 and at least 8,192.
    /// Omit to use the median of all complete windows and search from sample zero.
    #[arg(long)]
    baseline_samples: Option<usize>,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
    /// Optional SHA-256 identity of the complete saved input.
    #[arg(long)]
    expect_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BandEdges {
    lower: u64,
    upper: u64,
}

impl FromStr for BandEdges {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER:UPPER in hertz")?;
        Ok(Self {
            lower: lower.parse().context("invalid lower band edge in hertz")?,
            upper: upper.parse().context("invalid upper band edge in hertz")?,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Detection {
    pub(crate) start_sample: usize,
    pub(crate) end_sample_exclusive: usize,
    pub(crate) peak_excess_db: f64,
}

/// Evidence from the selected prefix of a pinned IQ file.
pub(crate) struct Measurement {
    /// Number of samples included in the digest, including a partial final window.
    pub(crate) analyzed_samples: usize,
    pub(crate) iq_sha256: String,
    /// Results in the order the caller supplied the bands.
    pub(crate) bands: Vec<MeasuredBand>,
}

/// Results for a half-open band integrated on the shared FFT grid.
pub(crate) struct MeasuredBand {
    pub(crate) baseline_band_power: f64,
    pub(crate) integrated_fft_bins: usize,
    pub(crate) detections: Vec<Detection>,
    /// One threshold result per complete window. A zero baseline gives no hits.
    pub(crate) above_threshold: Vec<bool>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    source_sha256: String,
    source_complex_samples: usize,
    source_sample_rate_hz: u32,
    center_frequency_hz: u64,
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    window_samples: usize,
    integrated_fft_bins: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_scope: Option<&'static str>,
    baseline_end_sample_exclusive: usize,
    baseline_band_power: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_start_sample: Option<usize>,
    analyzed_end_sample_exclusive: usize,
    omitted_trailing_samples: usize,
    threshold_db: f64,
    minimum_windows: usize,
    detections: Vec<Detection>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    additional_bands: Vec<BandActivity>,
}

#[derive(Serialize)]
struct BandActivity {
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    integrated_fft_bins: usize,
    baseline_band_power: f64,
    above_threshold_windows_hex: String,
}

fn median_power(windows: &[f64]) -> f64 {
    let mut sorted = windows.to_vec();
    sorted.sort_unstable_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn above_threshold_windows(search: &[f64], baseline: f64) -> String {
    let mut bytes = vec![0; search.len().div_ceil(8)];
    // The first search window is the low bit of the first byte.
    for (index, hit) in threshold_windows(search, baseline).enumerate() {
        if hit {
            bytes[index / 8] |= 1 << (index % 8);
        }
    }
    HEXLOWER.encode(&bytes)
}

fn threshold_windows(search: &[f64], baseline: f64) -> impl Iterator<Item = bool> + '_ {
    let threshold = burst_threshold(baseline);
    search
        .iter()
        .map(move |&power| baseline > 0.0 && power >= threshold)
}

fn band_bins(center_hz: u64, lower_hz: u64, upper_hz: u64) -> Result<Range<usize>> {
    let half_rate = u64::from(SAMPLE_RATE) / 2;
    let sampled_lower = center_hz
        .checked_sub(half_rate)
        .context("saved clip tuning is below half the sample rate")?;
    let sampled_upper = center_hz
        .checked_add(half_rate)
        .context("saved clip tuning overflows the sampled band")?;
    ensure!(
        sampled_lower <= lower_hz && lower_hz < upper_hz && upper_hz <= sampled_upper,
        "burst band must lie inside the saved clip's sampled band"
    );
    // Scale the edges by the transform size to compare fractional bin centers
    // exactly. Both half-open edges use the first bin at or above the edge.
    let edge_bin = |hz: u64| {
        ((hz - sampled_lower) * WINDOW_SAMPLES as u64).div_ceil(u64::from(SAMPLE_RATE)) as usize
    };
    let bins = edge_bin(lower_hz)..edge_bin(upper_hz);
    ensure!(!bins.is_empty(), "burst band contains no FFT bin centers");
    Ok(bins)
}

fn band_powers(
    input: iq::SavedIq,
    samples: usize,
    bands: &[Range<usize>],
    stop: &AtomicBool,
) -> Result<(Vec<Vec<f64>>, String)> {
    let transform = FftPlanner::new().plan_fft_forward(WINDOW_SAMPLES);
    let mut buffer = [Complex::default(); WINDOW_SAMPLES];
    let mut scratch = vec![Complex::default(); transform.get_inplace_scratch_len()];
    let hann: [f64; WINDOW_SAMPLES] = std::array::from_fn(|index| {
        0.5 - 0.5 * (TAU * index as f64 / (WINDOW_SAMPLES - 1) as f64).cos()
    });
    let hann_sum = hann.iter().sum::<f64>();
    let norm = (hann_sum * 2048.0).powi(2);
    let mut powers: Vec<_> = bands
        .iter()
        .map(|_| Vec::with_capacity(samples / WINDOW_SAMPLES))
        .collect();
    // Cache squared magnitudes when overlapping bands would compute more than
    // a full spectrum's worth per window. Each band still sums its own bins.
    let cache_magnitudes = bands.iter().map(Range::len).sum::<usize>() > WINDOW_SAMPLES;
    let source_sha256 = input.read_windows::<WINDOW_SAMPLES>(samples, stop, |window| {
        let mean: Complex<f64> = window
            .iter()
            .zip(hann.iter())
            .map(|(sample, weight)| sample * weight)
            .sum();
        let mean = mean / hann_sum;
        for ((value, sample), weight) in buffer.iter_mut().zip(window).zip(hann.iter()) {
            *value = (sample - mean) * weight;
        }
        transform.process_with_scratch(&mut buffer, &mut scratch);
        let bin_power = |bin: usize| buffer[(bin + WINDOW_SAMPLES / 2) % WINDOW_SAMPLES].norm_sqr();
        let magnitudes: Option<[f64; WINDOW_SAMPLES]> =
            cache_magnitudes.then(|| std::array::from_fn(bin_power));
        for (bins, powers) in bands.iter().zip(&mut powers) {
            let power: f64 = match &magnitudes {
                Some(values) => values[bins.clone()].iter().sum(),
                None => bins.clone().map(bin_power).sum(),
            };
            powers.push(power / norm);
        }
    })?;
    Ok((powers, source_sha256))
}

fn burst_threshold(baseline: f64) -> f64 {
    baseline * 10.0_f64.powf(THRESHOLD_DB / 10.0)
}

fn detections(search: &[f64], first_sample: usize, baseline: f64) -> Vec<Detection> {
    // A zero-power baseline provides no finite excess ratio.
    if baseline == 0.0 {
        return Vec::new();
    }
    let threshold = burst_threshold(baseline);
    let mut runs = Vec::new();
    let mut consecutive = 0;
    let mut peak = 0.0_f64;
    for (window, power) in search
        .iter()
        .copied()
        .chain(std::iter::once(0.0))
        .enumerate()
    {
        if power >= threshold {
            consecutive += 1;
            peak = peak.max(power);
            continue;
        }
        if consecutive >= MINIMUM_WINDOWS {
            runs.push(Detection {
                start_sample: first_sample + (window - consecutive) * WINDOW_SAMPLES,
                end_sample_exclusive: first_sample + window * WINDOW_SAMPLES,
                peak_excess_db: 10.0 * (peak.log10() - baseline.log10()),
            });
        }
        consecutive = 0;
        peak = 0.0;
    }
    runs
}

/// Measure overlapping half-open RF bands in one pass over a pinned IQ prefix.
///
/// The median of each band's complete windows supplies its baseline. The digest
/// covers the selected prefix, including any partial final window. The shared
/// file cursor is rewound before reading. Measurements use at most 1,280 bands,
/// 20 million selected samples and eight million band-window combinations.
///
/// # Errors
///
/// Returns an error for interrupted, unreadable, changed or invalid IQ, an
/// invalid band, a budget overrun or fewer than two complete windows.
pub(crate) fn measure_pinned(
    file: &File,
    center_hz: u64,
    bands: &[Range<u64>],
    sample_limit: usize,
    stop: &AtomicBool,
) -> Result<Measurement> {
    ensure!(
        (1..=MAX_PINNED_BANDS).contains(&bands.len()),
        "pinned burst probe requires 1 through {MAX_PINNED_BANDS} bands"
    );
    ensure!(
        (1..=MAX_INPUT_SAMPLES).contains(&sample_limit),
        "pinned burst prefix must select 1 through {MAX_INPUT_SAMPLES} samples"
    );
    let bins = bands
        .iter()
        .map(|band| band_bins(center_hz, band.start, band.end))
        .collect::<Result<Vec<_>>>()?;
    let input = iq::SavedIq::from_pinned(file, crate::MAX_SAMPLES as usize, stop)?;
    let analyzed_samples = input.source_samples().min(sample_limit);
    let windows = analyzed_samples / WINDOW_SAMPLES;
    ensure!(
        windows >= MINIMUM_WINDOWS,
        "burst search requires at least {MINIMUM_WINDOWS} complete windows"
    );
    ensure!(
        windows * bands.len() <= MAX_BAND_WINDOWS,
        "pinned burst probe exceeds the {MAX_BAND_WINDOWS}-band-window budget"
    );
    let (powers, iq_sha256) = band_powers(input, analyzed_samples, &bins, stop)?;
    let bands = bins
        .into_iter()
        .zip(powers)
        .map(|(bins, powers)| {
            check_stop(stop)?;
            let baseline = median_power(&powers);
            Ok(MeasuredBand {
                baseline_band_power: baseline,
                integrated_fft_bins: bins.len(),
                detections: detections(&powers, 0, baseline),
                above_threshold: threshold_windows(&powers, baseline).collect(),
            })
        })
        .collect::<Result<_>>()?;
    Ok(Measurement {
        analyzed_samples,
        iq_sha256,
        bands,
    })
}

pub(crate) fn probe(root: &Path, args: &ProbeBursts, stop: &AtomicBool) -> Result<()> {
    ensure!(
        args.also_band_hz.len() < MAX_BANDS,
        "burst probe accepts at most {MAX_BANDS} bands including the primary band"
    );
    let mut bands = vec![BandEdges {
        lower: args.lower_hz,
        upper: args.upper_hz,
    }];
    for &band in &args.also_band_hz {
        ensure!(
            !bands
                .iter()
                .any(|prior| prior.lower < band.upper && band.lower < prior.upper),
            "burst bands must not overlap"
        );
        bands.push(band);
    }
    let bins = bands
        .iter()
        .map(|band| band_bins(args.center_hz, band.lower, band.upper))
        .collect::<Result<Vec<_>>>()?;
    if let Some(baseline_samples) = args.baseline_samples {
        ensure!(
            baseline_samples >= MIN_BASELINE_SAMPLES
                && baseline_samples.is_multiple_of(WINDOW_SAMPLES),
            "burst baseline requires at least {MIN_BASELINE_SAMPLES} samples in complete windows"
        );
    }
    let input = iq::SavedIq::open(&args.input, MAX_INPUT_SAMPLES, stop)?;
    let source_samples = input.source_samples();
    let analyzed_samples = source_samples - source_samples % WINDOW_SAMPLES;
    let search_start = args.baseline_samples.unwrap_or(0);
    ensure!(
        analyzed_samples.saturating_sub(search_start) >= MINIMUM_WINDOWS * WINDOW_SAMPLES,
        "burst search requires at least {MINIMUM_WINDOWS} complete windows"
    );
    let (powers, source_sha256) = band_powers(input, source_samples, &bins, stop)?;
    if let Some(expected) = &args.expect_sha256 {
        ensure!(
            *expected == source_sha256,
            "saved IQ does not match the expected SHA-256"
        );
    }
    let baseline_end = args.baseline_samples.unwrap_or(analyzed_samples);
    let baseline_windows = baseline_end / WINDOW_SAMPLES;
    let first = search_start / WINDOW_SAMPLES;
    let primary_baseline = median_power(&powers[0][..baseline_windows]);
    let additional_bands: Vec<_> = bands
        .iter()
        .zip(&bins)
        .zip(&powers)
        .skip(1)
        .map(|((band, bins), powers)| {
            let baseline = median_power(&powers[..baseline_windows]);
            BandActivity {
                lower_frequency_hz: band.lower,
                upper_frequency_hz_exclusive: band.upper,
                integrated_fft_bins: bins.len(),
                baseline_band_power: baseline,
                above_threshold_windows_hex: above_threshold_windows(&powers[first..], baseline),
            }
        })
        .collect();
    let report = Report {
        schema_version: if additional_bands.is_empty() { 1 } else { 2 },
        operation: "probe_saved_iq_band_bursts",
        source_sha256,
        source_complex_samples: source_samples,
        source_sample_rate_hz: SAMPLE_RATE,
        center_frequency_hz: args.center_hz,
        lower_frequency_hz: args.lower_hz,
        upper_frequency_hz_exclusive: args.upper_hz,
        window_samples: WINDOW_SAMPLES,
        integrated_fft_bins: bins[0].len(),
        baseline_scope: args.baseline_samples.is_none().then_some("whole_extent"),
        baseline_end_sample_exclusive: baseline_end,
        baseline_band_power: primary_baseline,
        search_start_sample: args.baseline_samples.is_none().then_some(search_start),
        analyzed_end_sample_exclusive: analyzed_samples,
        omitted_trailing_samples: source_samples - analyzed_samples,
        threshold_db: THRESHOLD_DB,
        minimum_windows: MINIMUM_WINDOWS,
        detections: detections(&powers[0][first..], search_start, primary_baseline),
        additional_bands,
    };
    let output = publish_report(root, &args.output, &report, stop)?;
    println!("Saved band-burst measurements to {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Seek, SeekFrom};

    use sha2::{Digest, Sha256};

    use super::*;

    const CENTER_HZ: u64 = 2_437_000_000;

    #[test]
    fn includes_only_fft_bin_centers_inside_half_open_edges() {
        assert_eq!(
            band_bins(CENTER_HZ, CENTER_HZ + 1_250_000, CENTER_HZ + 1_269_532).unwrap(),
            576..578
        );
        assert_eq!(
            band_bins(CENTER_HZ, CENTER_HZ + 1_250_000, CENTER_HZ + 1_269_531).unwrap(),
            576..577
        );
        assert!(band_bins(CENTER_HZ, CENTER_HZ + 1_250_001, CENTER_HZ + 1_269_531).is_err());
        assert!(band_bins(CENTER_HZ, CENTER_HZ - 10_000_001, CENTER_HZ).is_err());
    }

    #[test]
    fn pinned_bands_share_windows_and_hash_only_the_selected_prefix() {
        let input = tempfile::NamedTempFile::new().unwrap();
        let mut raw = Vec::new();
        let selected = 5 * WINDOW_SAMPLES + 7;
        for index in 0..6 * WINDOW_SAMPLES {
            let amp = if matches!(index / WINDOW_SAMPLES, 2 | 3) {
                64.0
            } else {
                8.0
            };
            let sample = Complex::from_polar(amp, TAU * (index % 16) as f64 / 16.0);
            raw.extend((sample.re.round() as i16).to_le_bytes());
            raw.extend((sample.im.round() as i16).to_le_bytes());
        }
        raw[4 * selected..4 * selected + 2].copy_from_slice(&2048_i16.to_le_bytes());
        fs::write(input.path(), &raw).unwrap();
        let mut file = File::open(input.path()).unwrap();
        file.seek(SeekFrom::End(0)).unwrap();

        let result = measure_pinned(
            &file,
            CENTER_HZ,
            &[
                CENTER_HZ + 1_250_000..CENTER_HZ + 1_260_000,
                CENTER_HZ + 1_240_000..CENTER_HZ + 1_270_000,
            ],
            selected,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.analyzed_samples, selected);
        assert_eq!(file.stream_position().unwrap(), (4 * selected) as u64);
        assert_eq!(
            result.iq_sha256,
            format!("{:x}", Sha256::digest(&raw[..4 * selected]))
        );
        for (band, bins) in result.bands.iter().zip([1, 2]) {
            assert!(band.baseline_band_power > 0.0);
            assert_eq!(band.integrated_fft_bins, bins);
            assert_eq!(band.above_threshold, [false, false, true, true, false]);
            assert_eq!(band.detections.len(), 1);
            assert_eq!(band.detections[0].start_sample, 2 * WINDOW_SAMPLES);
            assert_eq!(band.detections[0].end_sample_exclusive, 4 * WINDOW_SAMPLES);
        }
    }

    #[test]
    fn locates_contiguous_in_band_rises_including_the_final_complete_window() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("clip.sc16q11");
        let output = directory.path().join("bursts.json");
        let complete = 18 * WINDOW_SAMPLES;
        let mut raw = Vec::new();
        for index in 0..complete + 513 {
            let window = index / WINDOW_SAMPLES;
            let phase = TAU * (index % WINDOW_SAMPLES) as f64 / WINDOW_SAMPLES as f64;
            let amplitude = match window {
                10 | 12..=13 | 15.. => 64.0,
                _ => 8.0,
            };
            let other = if matches!(window, 8..=9 | 12 | 14) {
                192.0
            } else {
                8.0
            };
            let sample = Complex::from_polar(amplitude, 64.0 * phase)
                + Complex::from_polar(other, -96.0 * phase);
            for component in [sample.re, sample.im] {
                raw.extend((component.round() as i16).to_le_bytes());
            }
        }
        fs::write(&input, raw).unwrap();
        let mut args = ProbeBursts {
            input,
            output: output.clone(),
            center_hz: CENTER_HZ,
            lower_hz: CENTER_HZ + 1_250_000,
            upper_hz: CENTER_HZ + 1_300_000,
            also_band_hz: Vec::new(),
            baseline_samples: Some(MIN_BASELINE_SAMPLES),
            expect_sha256: None,
        };
        let stop = AtomicBool::new(false);
        probe(directory.path(), &args, &stop).unwrap();
        let bytes = fs::read(&output).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["analyzed_end_sample_exclusive"], complete);
        assert_eq!(report["omitted_trailing_samples"], 513);
        assert!(report.get("baseline_scope").is_none());
        assert!(report.get("search_start_sample").is_none());
        let runs = report["detections"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        for (run, (start, end)) in runs.iter().zip([(12, 14), (15, 18)]) {
            assert_eq!(run["start_sample"], start * WINDOW_SAMPLES);
            assert_eq!(run["end_sample_exclusive"], end * WINDOW_SAMPLES);
            assert!((run["peak_excess_db"].as_f64().unwrap() - 18.06).abs() < 0.25);
        }
        assert!(probe(directory.path(), &args, &stop).is_err());
        assert_eq!(fs::read(output).unwrap(), bytes);

        args.also_band_hz = vec![BandEdges {
            lower: CENTER_HZ - 1_875_000,
            upper: CENTER_HZ - 1_825_000,
        }];
        args.output = directory.path().join("with-additional.json");
        probe(directory.path(), &args, &stop).unwrap();
        let mut expanded: serde_json::Value =
            serde_json::from_slice(&fs::read(&args.output).unwrap()).unwrap();
        assert_eq!(expanded["schema_version"], 2);
        let bands = expanded
            .as_object_mut()
            .unwrap()
            .remove("additional_bands")
            .unwrap();
        assert_eq!(bands[0]["integrated_fft_bins"], 3);
        assert_eq!(bands[0]["above_threshold_windows_hex"], "5300");
        expanded["schema_version"] = serde_json::json!(1);
        assert_eq!(expanded, report);

        args.also_band_hz.push(BandEdges {
            lower: args.upper_hz - 1,
            upper: args.upper_hz + 1,
        });
        args.output = directory.path().join("overlapping.json");
        assert!(probe(directory.path(), &args, &stop).is_err());
        assert!(!args.output.exists());
    }

    #[test]
    fn searches_beyond_the_clip_limit_with_a_whole_extent_median() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("segment.sc16q11");
        let whole = directory.path().join("whole.json");
        let prefix = directory.path().join("prefix.json");
        let window_count = iq::MAX_INPUT_SAMPLES / WINDOW_SAMPLES + 4;
        let complete = window_count * WINDOW_SAMPLES;
        let tone = |amplitude: f64| {
            let mut bytes = Vec::with_capacity(4 * WINDOW_SAMPLES);
            for index in 0..WINDOW_SAMPLES {
                let value = Complex::from_polar(amplitude, TAU * (index % 16) as f64 / 16.0);
                bytes.extend((value.re.round() as i16).to_le_bytes());
                bytes.extend((value.im.round() as i16).to_le_bytes());
            }
            bytes
        };
        let loud = tone(64.0);
        let quiet = tone(8.0);
        let mut raw = Vec::with_capacity(4 * (complete + 1));
        for window in 0..window_count {
            let block = if window < 2 || window >= window_count - 2 {
                &loud
            } else {
                &quiet
            };
            raw.extend_from_slice(block);
        }
        raw.extend_from_slice(&quiet[..4]);
        fs::write(&input, &raw).unwrap();
        let args = ProbeBursts {
            input,
            output: whole.clone(),
            center_hz: CENTER_HZ,
            lower_hz: CENTER_HZ + 1_250_000,
            upper_hz: CENTER_HZ + 1_300_000,
            also_band_hz: Vec::new(),
            baseline_samples: None,
            expect_sha256: Some(format!("{:x}", Sha256::digest(&raw))),
        };
        let stop = AtomicBool::new(false);
        probe(directory.path(), &args, &stop).unwrap();
        let measured: serde_json::Value =
            serde_json::from_slice(&fs::read(whole).unwrap()).unwrap();
        assert_eq!(measured["baseline_scope"], "whole_extent");
        assert_eq!(measured["baseline_end_sample_exclusive"], complete);
        assert_eq!(measured["search_start_sample"], 0);
        assert_eq!(measured["source_complex_samples"], complete + 1);
        assert_eq!(measured["omitted_trailing_samples"], 1);
        let full_runs = measured["detections"].as_array().unwrap();
        assert_eq!(full_runs.len(), 2);
        assert_eq!(full_runs[0]["start_sample"], 0);
        assert_eq!(full_runs[1]["end_sample_exclusive"], complete);

        let mut args = ProbeBursts {
            output: prefix.clone(),
            baseline_samples: Some(MIN_BASELINE_SAMPLES),
            ..args
        };
        probe(directory.path(), &args, &stop).unwrap();
        let measured: serde_json::Value =
            serde_json::from_slice(&fs::read(prefix).unwrap()).unwrap();
        assert_eq!(
            measured["baseline_end_sample_exclusive"],
            MIN_BASELINE_SAMPLES
        );
        assert_eq!(measured["detections"], serde_json::json!(full_runs[1..]));

        raw[4 * complete..4 * complete + 2].copy_from_slice(&2048_i16.to_le_bytes());
        fs::write(&args.input, raw).unwrap();
        args.output = directory.path().join("invalid.json");
        args.expect_sha256 = None;
        assert!(probe(directory.path(), &args, &stop).is_err());
        assert!(!args.output.exists());
    }

    #[test]
    fn zero_baseline_provides_no_finite_excess() {
        assert!(detections(&[1.0, 1.0], MIN_BASELINE_SAMPLES, 0.0).is_empty());
    }
}
