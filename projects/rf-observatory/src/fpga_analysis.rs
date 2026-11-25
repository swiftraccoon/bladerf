//! Offline relative analysis of validated profile-1 framed channel samples.
//!
//! Payloads are signed-i16 CIC outputs, not ordinary SC16 Q11 input. Statistics
//! use digital counts and spectra use deterministic distributed Hann windows.
//! Nothing in this module opens a radio or infers antenna wiring, calibrated RF
//! power, signal identity, a noise floor or channel occupancy.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use serde::Serialize;

use crate::dsp::{self, Iq16};
use crate::transport::{self, Frame, Summary};

const CHANNEL_SAMPLE_RATE_HZ: u32 = 1_000_000;
const FFT_SIZE: usize = 8192;
const MAX_WINDOWS: usize = 512;
const I16_COMPONENT_FULL_SCALE_POWER: f64 = 32768.0 * 32768.0;

#[derive(Debug, Serialize)]
struct ChannelReport {
    channel: usize,
    phase_word: u32,
    phase_word_hex: String,
    coarse_offset_hz: f64,
    caller_labeled_channel_center_hz: Option<f64>,
    samples: u64,
    minimum_i_counts: i16,
    maximum_i_counts: i16,
    minimum_q_counts: i16,
    maximum_q_counts: i16,
    mean_i_counts: f64,
    mean_q_counts: f64,
    dc_magnitude_counts: f64,
    mean_power_counts_squared: f64,
    rms_magnitude_counts: f64,
    ac_power_counts_squared: f64,
    ac_rms_magnitude_counts: f64,
    mean_power_db_relative_to_i16_component_full_scale: Option<f64>,
    ac_power_db_relative_to_i16_component_full_scale: Option<f64>,
    mean_power_db_relative_to_strongest_channel: Option<f64>,
    dc_power_fraction_db: Option<f64>,
    peak_magnitude_counts: f64,
    zero_complex_samples: u64,
    numeric_limit_component_hits: u64,
    spectrum_csv: String,
    spectrum_peak_residual_hz: Option<f64>,
    spectrum_peak_caller_labeled_rf_hz: Option<f64>,
    spectrum_peak_coherent_bin_power_counts_squared: f64,
    spectrum_peak_coherent_bin_power_db_relative_to_i16_component_full_scale: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    mode: &'static str,
    stream: Summary,
    channel_sample_rate_hz: u32,
    selected_duration_seconds: f64,
    caller_supplied_center_frequency_hz: Option<u64>,
    fft_size: usize,
    fft_windows: usize,
    fft_window_start_records: Vec<u64>,
    fft_bin_width_hz: f64,
    fft_window: &'static str,
    fft_normalization: &'static str,
    channels: Vec<ChannelReport>,
    power_note: &'static str,
    spectrum_note: &'static str,
    receiver_note: &'static str,
}

#[derive(Debug)]
struct SpectrumBin {
    residual_frequency_hz: f64,
    caller_labeled_rf_frequency_hz: Option<f64>,
    coherent_bin_power_counts_squared: f64,
    coherent_bin_power_db_relative_to_i16_component_full_scale: Option<f64>,
}

#[derive(Debug)]
pub(crate) struct Analysis {
    report: Report,
    spectra: [Vec<SpectrumBin>; 4],
}

impl Analysis {
    pub(crate) fn records(&self) -> u64 {
        self.report.stream.records
    }

    pub(crate) fn receiver(&self) -> Option<&'static str> {
        self.report.stream.receiver()
    }
}

#[derive(Clone, Copy, Debug)]
struct Moments {
    samples: u64,
    sum_i: i128,
    sum_q: i128,
    sum_power: u128,
    peak_power: u64,
    minimum_i: i16,
    maximum_i: i16,
    minimum_q: i16,
    maximum_q: i16,
    zero_complex_samples: u64,
    numeric_limit_component_hits: u64,
}

impl Default for Moments {
    fn default() -> Self {
        Self {
            samples: 0,
            sum_i: 0,
            sum_q: 0,
            sum_power: 0,
            peak_power: 0,
            minimum_i: i16::MAX,
            maximum_i: i16::MIN,
            minimum_q: i16::MAX,
            maximum_q: i16::MIN,
            zero_complex_samples: 0,
            numeric_limit_component_hits: 0,
        }
    }
}

impl Moments {
    fn push(&mut self, sample: Iq16) {
        let i = i64::from(sample.i);
        let q = i64::from(sample.q);
        let power = (i * i + q * q) as u64;
        self.samples += 1;
        self.sum_i += i128::from(sample.i);
        self.sum_q += i128::from(sample.q);
        self.sum_power += u128::from(power);
        self.peak_power = self.peak_power.max(power);
        self.minimum_i = self.minimum_i.min(sample.i);
        self.maximum_i = self.maximum_i.max(sample.i);
        self.minimum_q = self.minimum_q.min(sample.q);
        self.maximum_q = self.maximum_q.max(sample.q);
        self.zero_complex_samples += u64::from(sample.i == 0 && sample.q == 0);
        self.numeric_limit_component_hits += u64::from(matches!(sample.i, i16::MIN | i16::MAX));
        self.numeric_limit_component_hits += u64::from(matches!(sample.q, i16::MIN | i16::MAX));
    }

    fn values(self) -> MomentValues {
        let samples = self.samples as f64;
        let mean_i = self.sum_i as f64 / samples;
        let mean_q = self.sum_q as f64 / samples;
        let mean_power = self.sum_power as f64 / samples;
        let dc_power = mean_i * mean_i + mean_q * mean_q;
        let ac_power = (mean_power - dc_power).max(0.0);
        MomentValues {
            moments: self,
            mean_i,
            mean_q,
            mean_power,
            dc_power,
            ac_power,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MomentValues {
    moments: Moments,
    mean_i: f64,
    mean_q: f64,
    mean_power: f64,
    dc_power: f64,
    ac_power: f64,
}

struct Accumulator {
    moments: [Moments; 4],
    window_starts: Vec<u64>,
    current_window: usize,
    hann: Vec<f64>,
    fft: Arc<dyn Fft<f64>>,
    buffers: [Vec<Complex<f64>>; 4],
    accumulated_fft_power: [Vec<f64>; 4],
}

impl Accumulator {
    fn new(records: u64, requested_windows: usize) -> Result<Self> {
        ensure!(
            records >= FFT_SIZE as u64,
            "FPGA analysis requires at least {FFT_SIZE} records"
        );
        let available_windows = (records / FFT_SIZE as u64).min(MAX_WINDOWS as u64) as usize;
        let windows = requested_windows.min(available_windows);
        let window_starts = (0..windows)
            .map(|window| {
                if windows == 1 {
                    0
                } else {
                    window as u64 * (records - FFT_SIZE as u64) / (windows - 1) as u64
                }
            })
            .collect();
        let hann = (0..FFT_SIZE)
            .map(|index| {
                0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (FFT_SIZE - 1) as f64).cos()
            })
            .collect();
        let mut planner = FftPlanner::new();
        Ok(Self {
            moments: [Moments::default(); 4],
            window_starts,
            current_window: 0,
            hann,
            fft: planner.plan_fft_forward(FFT_SIZE),
            buffers: std::array::from_fn(|_| vec![Complex::new(0.0, 0.0); FFT_SIZE]),
            accumulated_fft_power: std::array::from_fn(|_| vec![0.0; FFT_SIZE]),
        })
    }

    fn push(&mut self, record: u64, frame: Frame) {
        for (moments, sample) in self.moments.iter_mut().zip(frame.samples) {
            moments.push(sample);
        }
        let Some(&start) = self.window_starts.get(self.current_window) else {
            return;
        };
        if !(start..start + FFT_SIZE as u64).contains(&record) {
            return;
        }
        let position = (record - start) as usize;
        for (channel, sample) in frame.samples.into_iter().enumerate() {
            self.buffers[channel][position] = Complex::new(
                f64::from(sample.i) * self.hann[position],
                f64::from(sample.q) * self.hann[position],
            );
        }
        if position + 1 != FFT_SIZE {
            return;
        }
        for channel in 0..4 {
            self.fft.process(&mut self.buffers[channel]);
            for (power, value) in self.accumulated_fft_power[channel]
                .iter_mut()
                .zip(&self.buffers[channel])
            {
                *power += value.norm_sqr();
            }
        }
        self.current_window += 1;
    }

    fn finish(self, stream: Summary, center_hz: Option<u64>) -> Result<Analysis> {
        ensure!(
            self.current_window == self.window_starts.len(),
            "selected FFT windows were not completely processed"
        );
        ensure!(
            self.moments
                .iter()
                .all(|moments| moments.samples == stream.records),
            "channel statistics do not cover the selected stream"
        );
        let values: [MomentValues; 4] = self.moments.map(Moments::values);
        let strongest_channel_power = values
            .iter()
            .map(|value| value.mean_power)
            .fold(0.0_f64, f64::max);
        let coherent_sum = self.hann.iter().sum::<f64>();
        let fft_normalization = coherent_sum * coherent_sum * self.window_starts.len() as f64;
        let mut spectra: [Vec<SpectrumBin>; 4] =
            std::array::from_fn(|_| Vec::with_capacity(FFT_SIZE));
        let mut channels = Vec::with_capacity(4);
        for channel in 0..4 {
            let phase_word = transport::PROFILE_WORDS[channel];
            let coarse_offset_hz = dsp::tuning_frequency_hz(phase_word, crate::SAMPLE_RATE);
            let channel_center_hz = center_hz.map(|center| center as f64 + coarse_offset_hz);
            let mut peak_coherent_power = 0.0_f64;
            let mut peak_residual_hz = None;
            for display_bin in 0..FFT_SIZE {
                let source_bin = (display_bin + FFT_SIZE / 2) % FFT_SIZE;
                let residual_frequency_hz = (display_bin as f64 - FFT_SIZE as f64 / 2.0)
                    * f64::from(CHANNEL_SAMPLE_RATE_HZ)
                    / FFT_SIZE as f64;
                let coherent_bin_power_counts_squared =
                    self.accumulated_fft_power[channel][source_bin] / fft_normalization;
                if coherent_bin_power_counts_squared > peak_coherent_power {
                    peak_coherent_power = coherent_bin_power_counts_squared;
                    peak_residual_hz = Some(residual_frequency_hz);
                }
                spectra[channel].push(SpectrumBin {
                    residual_frequency_hz,
                    caller_labeled_rf_frequency_hz: channel_center_hz
                        .map(|frequency| frequency + residual_frequency_hz),
                    coherent_bin_power_counts_squared,
                    coherent_bin_power_db_relative_to_i16_component_full_scale: power_db(
                        coherent_bin_power_counts_squared,
                        I16_COMPONENT_FULL_SCALE_POWER,
                    ),
                });
            }
            let value = values[channel];
            channels.push(ChannelReport {
                channel,
                phase_word,
                phase_word_hex: format!("0x{phase_word:08x}"),
                coarse_offset_hz,
                caller_labeled_channel_center_hz: channel_center_hz,
                samples: value.moments.samples,
                minimum_i_counts: value.moments.minimum_i,
                maximum_i_counts: value.moments.maximum_i,
                minimum_q_counts: value.moments.minimum_q,
                maximum_q_counts: value.moments.maximum_q,
                mean_i_counts: value.mean_i,
                mean_q_counts: value.mean_q,
                dc_magnitude_counts: value.dc_power.sqrt(),
                mean_power_counts_squared: value.mean_power,
                rms_magnitude_counts: value.mean_power.sqrt(),
                ac_power_counts_squared: value.ac_power,
                ac_rms_magnitude_counts: value.ac_power.sqrt(),
                mean_power_db_relative_to_i16_component_full_scale: power_db(
                    value.mean_power,
                    I16_COMPONENT_FULL_SCALE_POWER,
                ),
                ac_power_db_relative_to_i16_component_full_scale: power_db(
                    value.ac_power,
                    I16_COMPONENT_FULL_SCALE_POWER,
                ),
                mean_power_db_relative_to_strongest_channel: power_db(
                    value.mean_power,
                    strongest_channel_power,
                ),
                dc_power_fraction_db: power_db(value.dc_power, value.mean_power),
                peak_magnitude_counts: (value.moments.peak_power as f64).sqrt(),
                zero_complex_samples: value.moments.zero_complex_samples,
                numeric_limit_component_hits: value.moments.numeric_limit_component_hits,
                spectrum_csv: format!("channel-{channel}-spectrum.csv"),
                spectrum_peak_residual_hz: peak_residual_hz,
                spectrum_peak_caller_labeled_rf_hz: peak_residual_hz
                    .zip(channel_center_hz)
                    .map(|(residual, center)| center + residual),
                spectrum_peak_coherent_bin_power_counts_squared: peak_coherent_power,
                spectrum_peak_coherent_bin_power_db_relative_to_i16_component_full_scale: power_db(
                    peak_coherent_power,
                    I16_COMPONENT_FULL_SCALE_POWER,
                ),
            });
        }
        Ok(Analysis {
            report: Report {
                schema_version: 1,
                mode: "offline_relative_profile1_analysis",
                selected_duration_seconds: stream.records as f64
                    / f64::from(CHANNEL_SAMPLE_RATE_HZ),
                stream,
                channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
                caller_supplied_center_frequency_hz: center_hz,
                fft_size: FFT_SIZE,
                fft_windows: self.window_starts.len(),
                fft_window_start_records: self.window_starts,
                fft_bin_width_hz: f64::from(CHANNEL_SAMPLE_RATE_HZ) / FFT_SIZE as f64,
                fft_window: "symmetric_Hann",
                fft_normalization: "mean squared complex DFT magnitude divided by the squared coherent Hann sum",
                channels,
                power_note: "Payload statistics are digital FPGA NCO/CIC output counts. Log powers use 32768^2 counts^2 as the explicit 0 dB reference; simultaneous near-full-scale I and Q can approach +3.01 dB. Values are not dBm, RSSI, receiver calibration, antenna response, or analog-ADC clipping tests.",
                spectrum_note: "Spectra use deterministic windows distributed over the selected records. The first window begins at record zero and retains the CIC startup transient. Squared coherent-Hann-gain normalization gives coherent bin-centered tone amplitude squared; it is not Parseval broadband-noise bin power. CIC droop and aliasing remain; a bin is not a station, signal identity, noise floor, occupancy percentage, or continuous-monitoring result. RF labels, when present, add an unverified caller-supplied center to exact profile-word and residual offsets.",
                receiver_note: "The validated RX1/RX2 transport flag identifies the selected bladeRF input, not which connector is attached to an antenna or distribution path.",
            },
            spectra,
        })
    }
}

fn power_db(power: f64, reference: f64) -> Option<f64> {
    (power > 0.0 && reference > 0.0).then(|| 10.0 * (power / reference).log10())
}

/// Analyze a complete framed stream, or an explicit prefix, with bounded memory.
pub(crate) fn analyze(
    path: &Path,
    max_records: Option<u64>,
    requested_windows: usize,
    center_hz: Option<u64>,
    stop: &AtomicBool,
) -> Result<Analysis> {
    ensure!(
        (1..=MAX_WINDOWS).contains(&requested_windows),
        "windows must be 1..={MAX_WINDOWS}"
    );
    let mut accumulator = None;
    let stream = transport::inspect(path, max_records, stop, |record, records, frame| {
        if accumulator.is_none() {
            accumulator = Some(Accumulator::new(records, requested_windows)?);
        }
        accumulator
            .as_mut()
            .context("initialize FPGA analysis")?
            .push(record, frame);
        Ok(())
    })?;
    accumulator
        .context("empty framed stream")?
        .finish(stream, center_hz)
}

/// Publish spectra first and a terminal JSON report last. Staging files remain
/// useful evidence if a write, sync or no-replace publication fails.
pub(crate) fn publish(directory: &Path, analysis: &Analysis, stop: &AtomicBool) -> Result<()> {
    let destinations: Vec<_> = (0..4)
        .map(|channel| directory.join(format!("channel-{channel}-spectrum.csv")))
        .chain(std::iter::once(directory.join("analysis.json")))
        .collect();
    for destination in &destinations {
        let stage = partial_path(destination)?;
        ensure!(
            !destination.exists() && !stage.exists(),
            "analysis output already exists: {}",
            destination.display()
        );
    }
    for (channel, destination) in destinations.iter().take(4).enumerate() {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "analysis publication interrupted"
        );
        let stage = partial_path(destination)?;
        let mut output = BufWriter::new(crate::new_file(&stage)?);
        writeln!(
            output,
            "residual_frequency_hz,caller_labeled_rf_frequency_hz,coherent_bin_power_counts_squared,coherent_bin_power_db_relative_to_i16_component_full_scale"
        )?;
        for bin in &analysis.spectra[channel] {
            write!(output, "{:.9},", bin.residual_frequency_hz)?;
            if let Some(frequency) = bin.caller_labeled_rf_frequency_hz {
                write!(output, "{frequency:.9}")?;
            }
            write!(output, ",{:.17e},", bin.coherent_bin_power_counts_squared)?;
            if let Some(power) = bin.coherent_bin_power_db_relative_to_i16_component_full_scale {
                write!(output, "{power:.9}")?;
            }
            writeln!(output)?;
        }
        output.flush()?;
        output.get_ref().sync_all()?;
        drop(output);
        ensure!(
            !stop.load(Ordering::Relaxed),
            "analysis publication interrupted"
        );
        publish_stage(&stage, destination)?;
    }
    ensure!(
        !stop.load(Ordering::Relaxed),
        "analysis publication interrupted"
    );
    let report_destination = destinations.last().context("analysis report path")?;
    let report_stage = partial_path(report_destination)?;
    let mut report = BufWriter::new(crate::new_file(&report_stage)?);
    serde_json::to_writer_pretty(&mut report, &analysis.report)?;
    report.write_all(b"\n")?;
    report.flush()?;
    report.get_ref().sync_all()?;
    drop(report);
    ensure!(
        !stop.load(Ordering::Relaxed),
        "analysis publication interrupted"
    );
    publish_stage(&report_stage, report_destination)
}

fn partial_path(destination: &Path) -> Result<std::path::PathBuf> {
    let name = destination
        .file_name()
        .context("analysis destination has no file name")?
        .to_string_lossy();
    Ok(destination.with_file_name(format!("{name}.partial")))
}

fn publish_stage(stage: &Path, destination: &Path) -> Result<()> {
    fs::hard_link(stage, destination)
        .with_context(|| format!("publish complete analysis output {}", destination.display()))?;
    let _ = fs::remove_file(stage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed_record(index: u64, samples: [Iq16; 4], rx2: bool) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..2].copy_from_slice(&0x5246u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&0x4f42u16.to_le_bytes());
        let flags: u16 = 0x1000 | if rx2 { 0x20 } else { 0 };
        bytes[4..6].copy_from_slice(&flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&transport::PROFILE_ID.to_le_bytes());
        bytes[8..16].copy_from_slice(&index.to_le_bytes());
        for (channel, sample) in samples.into_iter().enumerate() {
            bytes[16 + channel * 4..18 + channel * 4].copy_from_slice(&sample.i.to_le_bytes());
            bytes[18 + channel * 4..20 + channel * 4].copy_from_slice(&sample.q.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn known_tones_and_zero_channel_have_stable_statistics_and_bins() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("tones.frames");
        let mut file = BufWriter::new(std::fs::File::create(&input).unwrap());
        for record in 0..FFT_SIZE {
            let positive = std::f64::consts::TAU * 100.0 * record as f64 / FFT_SIZE as f64;
            let negative = -std::f64::consts::TAU * 40.0 * record as f64 / FFT_SIZE as f64;
            let samples = [
                Iq16 {
                    i: (1000.0 * positive.cos()).round() as i16,
                    q: (1000.0 * positive.sin()).round() as i16,
                },
                Iq16 { i: 300, q: -400 },
                Iq16::default(),
                Iq16 {
                    i: (2000.0 * negative.cos()).round() as i16,
                    q: (2000.0 * negative.sin()).round() as i16,
                },
            ];
            file.write_all(&framed_record(record as u64 * 20 + 19, samples, true))
                .unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let analysis = analyze(&input, None, 1, Some(98_100_000), &AtomicBool::new(false)).unwrap();
        assert_eq!(analysis.records(), FFT_SIZE as u64);
        assert_eq!(analysis.receiver(), Some("RX2"));
        let json = serde_json::to_value(&analysis.report).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["fft_window_start_records"], serde_json::json!([0]));
        assert!(
            (json["channels"][0]["rms_magnitude_counts"]
                .as_f64()
                .unwrap()
                - 1000.0)
                .abs()
                < 0.1
        );
        assert_eq!(json["channels"][1]["mean_i_counts"], 300.0);
        assert_eq!(json["channels"][1]["mean_q_counts"], -400.0);
        assert_eq!(json["channels"][1]["mean_power_counts_squared"], 250_000.0);
        assert_eq!(json["channels"][1]["rms_magnitude_counts"], 500.0);
        assert_eq!(json["channels"][0]["minimum_i_counts"], -1000);
        assert_eq!(json["channels"][0]["maximum_i_counts"], 1000);
        assert_eq!(json["channels"][0]["minimum_q_counts"], -1000);
        assert_eq!(json["channels"][0]["maximum_q_counts"], 1000);
        assert_eq!(json["channels"][2]["mean_power_counts_squared"], 0.0);
        assert!(
            json["channels"][2]["mean_power_db_relative_to_i16_component_full_scale"].is_null()
        );
        let bin_width = f64::from(CHANNEL_SAMPLE_RATE_HZ) / FFT_SIZE as f64;
        assert!(
            (json["channels"][0]["spectrum_peak_residual_hz"]
                .as_f64()
                .unwrap()
                - 100.0 * bin_width)
                .abs()
                < 1e-9
        );
        assert!(
            (json["channels"][3]["spectrum_peak_residual_hz"]
                .as_f64()
                .unwrap()
                + 40.0 * bin_width)
                .abs()
                < 1e-9
        );
        assert!(
            (json["channels"][0]["spectrum_peak_coherent_bin_power_counts_squared"]
                .as_f64()
                .unwrap()
                - 1_000_000.0)
                .abs()
                < 100.0
        );
        assert!(json["channels"][0]["spectrum_peak_caller_labeled_rf_hz"].is_number());
    }

    #[test]
    fn explicit_prefix_is_labeled_and_publication_is_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("prefix.frames");
        let mut bytes = Vec::new();
        for record in 0..FFT_SIZE + 1 {
            let sample = if record == 0 {
                Iq16 {
                    i: i16::MIN,
                    q: i16::MAX,
                }
            } else {
                Iq16::default()
            };
            bytes.extend_from_slice(&framed_record(record as u64 * 20 + 19, [sample; 4], false));
        }
        std::fs::write(&input, bytes).unwrap();
        let analysis = analyze(
            &input,
            Some(FFT_SIZE as u64),
            512,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        let json = serde_json::to_value(&analysis.report).unwrap();
        assert_eq!(json["stream"]["available_records"], (FFT_SIZE + 1) as u64);
        assert_eq!(json["stream"]["records"], FFT_SIZE as u64);
        assert_eq!(json["stream"]["input_truncated_to_limit"], true);
        assert_eq!(json["channels"][0]["numeric_limit_component_hits"], 2);
        assert!(json["channels"][0]["caller_labeled_channel_center_hz"].is_null());
        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let stop = AtomicBool::new(false);
        publish(&output, &analysis, &stop).unwrap();
        assert!(output.join("analysis.json").is_file());
        assert!(!output.join("analysis.json.partial").exists());
        for channel in 0..4 {
            assert!(
                output
                    .join(format!("channel-{channel}-spectrum.csv"))
                    .is_file()
            );
        }
        assert!(publish(&output, &analysis, &stop).is_err());

        let interrupted = temp.path().join("interrupted");
        std::fs::create_dir(&interrupted).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(publish(&interrupted, &analysis, &stop).is_err());
        assert_eq!(std::fs::read_dir(interrupted).unwrap().count(), 0);
    }
}
