//! File-only modulation measurements for a selected interval of a saved IQ clip.

pub(crate) mod burst;
mod fsk;
mod iq;
pub(crate) mod ofdm;
mod symbols;

use std::f64::consts::TAU;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::Args;
use rustfft::{FftPlanner, num_complex::Complex};
use serde::Serialize;

use self::iq::{BASEBAND_RATE, DECIMATION, FILTER_CUTOFF_HZ, FILTER_HALF_TAPS};

/// Minimum number of original-rate complex samples to measure.
pub(crate) const MIN_INPUT_INTERVAL: usize = 8_192;
/// Maximum number of original-rate complex samples in a saved clip.
pub(crate) const MAX_CLIP_SAMPLES: usize = iq::MAX_INPUT_SAMPLES;
/// Number of saved samples needed before and after the measurement interval.
pub(crate) const CLIP_CONTEXT_SAMPLES: usize = FILTER_HALF_TAPS;

const MAX_CARRIER_OFFSET_HZ: f64 = 200_000.0;
const MIN_POWER_LINE_FRACTION: f64 = 0.20;
const MIN_POWER_LINE_GAIN: f64 = 4.0;

#[derive(Debug, Args)]
pub(crate) struct ProbeWaveform {
    /// Saved little-endian SC16 Q11 clip, up to 245,760 complex samples.
    input: PathBuf,
    /// RF tuning of the saved clip in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Center of the candidate band in hertz.
    #[arg(long)]
    target_hz: u64,
    /// First input sample in the candidate interval, local to the clip.
    #[arg(long)]
    start_sample: usize,
    /// Exclusive end of the candidate interval, local to the clip.
    #[arg(long)]
    end_sample: usize,
    /// Optional SHA-256 identity from the clip receipt.
    #[arg(long)]
    expect_sha256: Option<String>,
    /// Select the modulation used for unframed symbol recovery.
    #[arg(long, value_enum)]
    modulation: Option<symbols::Modulation>,
    /// Expected symbol rate in symbols per second (8,000 through 200,000).
    #[arg(long, requires = "modulation", value_parser = clap::value_parser!(u32).range(symbols::MIN_RATE_HZ as i64..=symbols::MAX_RATE_HZ as i64))]
    symbol_rate_hz: Option<u32>,
    /// Search for a symbol rate from 8,000 through 200,000 symbols per second.
    #[arg(long, conflicts_with = "symbol_rate_hz")]
    search_symbol_rate: bool,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    source_sha256: String,
    source_complex_samples: usize,
    source_sample_rate_hz: u32,
    center_frequency_hz: u64,
    target_frequency_hz: u64,
    start_sample: usize,
    end_sample_exclusive: usize,
    baseband_sample_rate_hz: u32,
    baseband_complex_samples: usize,
    decimation: usize,
    filter_taps: usize,
    filter_cutoff_hz: u64,
    filter_window: &'static str,
    line_window: &'static str,
    #[serde(flatten)]
    evidence: NarrowEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_recovery: Option<symbols::SymbolRecovery>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blind_symbol_recovery: Option<symbols::BlindSymbolRecovery>,
}

/// Phase symmetry and frequency plateau measurements for a narrow IQ channel.
///
/// Qualified hints describe possible modulation and carry no protocol identity.
#[derive(Serialize)]
pub(crate) struct NarrowEvidence {
    phase_lines: [PhaseLine; 3],
    phase_order_hint: Option<u32>,
    frequency_plateaus: Option<fsk::FrequencyPlateaus>,
}

#[derive(Serialize)]
struct PhaseLine {
    exponent: u32,
    peak_offset_hz: Option<f64>,
    peak_bin_power_fraction: Option<f64>,
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "waveform analysis interrupted"
    );
    Ok(())
}

fn phase_lines(samples: &[Complex<f64>], stop: &AtomicBool) -> Result<[PhaseLine; 3]> {
    let size = samples.len();
    let transform = FftPlanner::new().plan_fft_forward(size);
    let weights: Vec<_> = (0..size)
        .map(|index| 0.5 - 0.5 * (TAU * index as f64 / (size - 1) as f64).cos())
        .collect();
    let mut spectrum = vec![Complex::default(); size];
    let mut measure = |exponent| -> Result<PhaseLine> {
        check_stop(stop)?;
        for ((bin, &sample), &weight) in spectrum.iter_mut().zip(samples).zip(&weights) {
            *bin = sample.powu(exponent) * weight;
        }
        transform.process(&mut spectrum);
        let mut total = 0.0;
        let mut peak = 0.0;
        let mut peak_index = 0;
        for (index, sample) in spectrum.iter().enumerate() {
            let power = sample.norm_sqr();
            total += power;
            if power > peak {
                peak = power;
                peak_index = index;
            }
        }
        let bin = if peak_index < size.div_ceil(2) {
            peak_index as f64
        } else {
            peak_index as f64 - size as f64
        };
        Ok(PhaseLine {
            exponent,
            peak_offset_hz: (total > 0.0).then_some(bin * f64::from(BASEBAND_RATE) / size as f64),
            peak_bin_power_fraction: (total > 0.0).then(|| peak / total),
        })
    };
    Ok([measure(1)?, measure(2)?, measure(4)?])
}

fn phase_order_hint(lines: &[PhaseLine; 3]) -> Option<u32> {
    let ordinary = lines[0].peak_bin_power_fraction?;
    lines[1..].iter().find_map(|line| {
        let fraction = line.peak_bin_power_fraction?;
        let carrier_offset = line.peak_offset_hz? / f64::from(line.exponent);
        // Ordinary tones retain their concentration, while phase-state symmetry
        // can create a stronger line after raising the signal to that order.
        (carrier_offset.abs() <= MAX_CARRIER_OFFSET_HZ
            && fraction >= MIN_POWER_LINE_FRACTION
            && fraction >= MIN_POWER_LINE_GAIN * ordinary)
            .then_some(line.exponent)
    })
}

fn read_narrow_measurement(
    input: iq::ClipInput<'_>,
    center_hz: u64,
    target_hz: u64,
    interval: Range<usize>,
    expected_sha256: Option<&str>,
    stop: &AtomicBool,
) -> Result<(iq::Baseband, NarrowEvidence)> {
    ensure!(
        interval.len() >= MIN_INPUT_INTERVAL,
        "waveform interval requires at least {MIN_INPUT_INTERVAL} complex samples"
    );
    let baseband = iq::channel(input, center_hz, target_hz, interval, stop)?;
    if let Some(expected) = expected_sha256 {
        ensure!(
            expected == baseband.source_sha256,
            "saved IQ does not match the expected SHA-256"
        );
    }

    let phase_lines = phase_lines(&baseband.samples, stop)?;
    let phase_order_hint = phase_order_hint(&phase_lines);
    check_stop(stop)?;
    let frequency_plateaus = fsk::measure(&baseband.samples);
    check_stop(stop)?;
    let evidence = NarrowEvidence {
        phase_lines,
        phase_order_hint,
        frequency_plateaus,
    };
    Ok((baseband, evidence))
}

/// Measure a half-open, clip-local sample interval at a selected RF frequency.
///
/// The input is a 20 MSps little-endian SC16 Q11 clip with at most
/// [`MAX_CLIP_SAMPLES`] complex samples. The interval needs at least
/// [`MIN_INPUT_INTERVAL`] samples with [`CLIP_CONTEXT_SAMPLES`] saved on each
/// side for the filter. The optional SHA-256 covers the whole clip.
///
/// # Errors
///
/// Returns an error on interruption, file or sample validation failure, or
/// digest mismatch. Both frequencies must be positive, and the target must
/// have at least 500 kHz of margin inside the sampled band.
pub(crate) fn measure_narrow(
    path: &Path,
    center_hz: u64,
    target_hz: u64,
    interval: Range<usize>,
    expected_sha256: Option<&str>,
    stop: &AtomicBool,
) -> Result<NarrowEvidence> {
    read_narrow_measurement(
        iq::ClipInput::Path(path),
        center_hz,
        target_hz,
        interval,
        expected_sha256,
        stop,
    )
    .map(|(_, evidence)| evidence)
}

/// Measure a clip already read into memory, with the same interval as [`measure_narrow`].
///
/// Returns the lowercase SHA-256 of the complete input buffer and its narrow
/// waveform evidence. The caller owns verification of the clip's source identity.
///
/// # Errors
///
/// Returns an error on interruption, unsupported frequencies, an invalid
/// SC16 Q11 clip, or an interval without sufficient samples and filter context.
pub(crate) fn measure_narrow_bytes(
    bytes: Vec<u8>,
    center_hz: u64,
    target_hz: u64,
    interval: Range<usize>,
    stop: &AtomicBool,
) -> Result<(String, NarrowEvidence)> {
    let (baseband, evidence) = read_narrow_measurement(
        iq::ClipInput::Bytes(bytes),
        center_hz,
        target_hz,
        interval,
        None,
        stop,
    )?;
    Ok((baseband.source_sha256, evidence))
}

pub(crate) fn probe(root: &Path, args: &ProbeWaveform, stop: &AtomicBool) -> Result<()> {
    ensure!(
        args.modulation.is_none() || args.symbol_rate_hz.is_some() || args.search_symbol_rate,
        "modulation requires a supplied symbol rate or a symbol rate search"
    );
    let (baseband, evidence) = read_narrow_measurement(
        iq::ClipInput::Path(&args.input),
        args.center_hz,
        args.target_hz,
        args.start_sample..args.end_sample,
        args.expect_sha256.as_deref(),
        stop,
    )?;
    let symbol_recovery = args
        .modulation
        .zip(args.symbol_rate_hz)
        .map(|(modulation, rate)| {
            symbols::measure(
                &baseband.samples,
                modulation,
                rate,
                &evidence.phase_lines,
                evidence.phase_order_hint,
                evidence.frequency_plateaus.as_ref(),
                stop,
            )
        })
        .transpose()?;
    let blind_symbol_recovery = args
        .search_symbol_rate
        .then(|| {
            symbols::estimate(
                &baseband.samples,
                args.modulation,
                &evidence.phase_lines,
                evidence.phase_order_hint,
                evidence.frequency_plateaus.as_ref(),
                stop,
            )
        })
        .transpose()?;
    let report = Report {
        schema_version: match (symbol_recovery.is_some(), blind_symbol_recovery.is_some()) {
            (_, true) => 3,
            (true, false) => 2,
            (false, false) => 1,
        },
        operation: "probe_saved_iq_waveform",
        source_sha256: baseband.source_sha256,
        source_complex_samples: baseband.source_samples,
        source_sample_rate_hz: crate::SAMPLE_RATE,
        center_frequency_hz: args.center_hz,
        target_frequency_hz: args.target_hz,
        start_sample: args.start_sample,
        end_sample_exclusive: args.end_sample,
        baseband_sample_rate_hz: BASEBAND_RATE,
        baseband_complex_samples: baseband.samples.len(),
        decimation: DECIMATION,
        filter_taps: 2 * FILTER_HALF_TAPS + 1,
        filter_cutoff_hz: FILTER_CUTOFF_HZ,
        filter_window: "hamming",
        line_window: "hann",
        evidence,
        symbol_recovery,
        blind_symbol_recovery,
    };
    let output = publish_report(root, &args.output, &report, stop)?;
    println!("Saved waveform measurements to {}", output.display());
    Ok(())
}

fn publish_report(
    root: &Path,
    output: &Path,
    report: &impl Serialize,
    stop: &AtomicBool,
) -> Result<PathBuf> {
    check_stop(stop)?;
    let output = root.join(output);
    crate::private_output::publish_json_file(&output, report, || check_stop(stop))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn narrow_measurement_matches_the_waveform_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("clip.sc16q11");
        let output = directory.path().join("waveform.json");
        let center_hz = 100_000_000;
        let target_hz = center_hz + 250_000;
        let start_sample = CLIP_CONTEXT_SAMPLES;
        let end_sample = start_sample + MIN_INPUT_INTERVAL;
        let mut raw = Vec::new();
        let mut state = 17_u32;
        let mut sign = 1.0;
        for index in 0..end_sample + CLIP_CONTEXT_SAMPLES {
            if index.is_multiple_of(256) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                sign = 1.0 - 2.0 * f64::from(state & 1);
            }
            let phase =
                TAU * index as f64 * (target_hz - center_hz) as f64 / f64::from(crate::SAMPLE_RATE);
            for component in [phase.cos(), phase.sin()] {
                raw.extend_from_slice(&((600.0 * sign * component).round() as i16).to_le_bytes());
            }
        }
        let expected_sha256 = format!("{:x}", Sha256::digest(&raw));
        std::fs::write(&input, &raw).unwrap();
        let stop = AtomicBool::new(false);
        let evidence = measure_narrow(
            &input,
            center_hz,
            target_hz,
            start_sample..end_sample,
            Some(&expected_sha256),
            &stop,
        )
        .unwrap();
        let (digest, bytes_evidence) =
            measure_narrow_bytes(raw, center_hz, target_hz, start_sample..end_sample, &stop)
                .unwrap();
        assert_eq!(digest, expected_sha256);
        let evidence = serde_json::to_vec(&evidence).unwrap();
        assert_eq!(serde_json::to_vec(&bytes_evidence).unwrap(), evidence);
        let evidence: serde_json::Value = serde_json::from_slice(&evidence).unwrap();
        assert_eq!(evidence["phase_order_hint"], 2);
        let args = ProbeWaveform {
            input,
            center_hz,
            target_hz,
            start_sample,
            end_sample,
            expect_sha256: Some(expected_sha256),
            modulation: None,
            symbol_rate_hz: None,
            search_symbol_rate: false,
            output,
        };
        probe(directory.path(), &args, &stop).unwrap();
        let receipt: serde_json::Value =
            serde_json::from_reader(File::open(&args.output).unwrap()).unwrap();
        assert_eq!(receipt["schema_version"], 1);
        for (key, field) in evidence.as_object().unwrap() {
            assert_eq!(&receipt[key], field);
        }
    }

    #[test]
    fn phase_order_distinguishes_two_and_four_states_from_a_carrier() {
        let stop = AtomicBool::new(false);
        for (order, expected) in [(1, None), (2, Some(2)), (4, Some(4))] {
            let mut state = 17u32;
            let mut phase = 0.0;
            let samples: Vec<_> = (0..4096u32)
                .map(|index| {
                    if index.is_multiple_of(50) {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        phase = TAU * f64::from(state % order) / f64::from(order);
                    }
                    let carrier = TAU * f64::from(index) * 37_000.0 / f64::from(BASEBAND_RATE);
                    Complex::from_polar(0.6, phase + carrier)
                })
                .collect();
            let lines = phase_lines(&samples, &stop).unwrap();
            assert_eq!(phase_order_hint(&lines), expected);
        }
    }
}
