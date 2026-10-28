//! Bounded averaged spectra from ordinary hosted-image SC16 Q11 recordings.
//!
//! Windows are distributed over a bounded prefix. Candidate centers rank local
//! spectral energy on a 100 kHz grid; they are not confirmed stations, calibrated
//! power, occupancy percentages or continuously monitored RF observations.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::Serialize;

const FFT: usize = 8192;

#[derive(Debug, Serialize)]
pub(crate) struct Bin {
    pub(crate) frequency_hz: f64,
    pub(crate) relative_power_db: f64,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    sample_rate_hz: u32,
    center_frequency_hz: u64,
    input_samples: u64,
    inspected_prefix_samples: u64,
    fft_size: usize,
    pub(crate) windows: usize,
    window: &'static str,
    power_note: &'static str,
    pub(crate) bins: Vec<Bin>,
    pub(crate) candidates: Vec<Bin>,
}

pub(crate) fn survey(
    path: &Path,
    center: u64,
    max_samples: u64,
    requested_windows: usize,
    stop: &AtomicBool,
) -> Result<Report> {
    ensure!(
        (1..=crate::MAX_SAMPLES).contains(&max_samples),
        "invalid spectrum sample limit"
    );
    ensure!(
        (1..=512).contains(&requested_windows),
        "invalid spectrum window count"
    );
    let (mut file, available) = crate::receiver::input(path)?;
    let samples = available.min(max_samples);
    ensure!(
        samples >= FFT as u64,
        "spectrum requires at least {FFT} samples"
    );
    let windows = requested_windows.min(samples as usize / FFT);
    let hann: Vec<_> = (0..FFT)
        .map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (FFT - 1) as f64).cos())
        .collect();
    let norm = (hann.iter().sum::<f64>() * 2048.0).powi(2);
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(FFT);
    let mut buffer = vec![Complex::new(0.0, 0.0); FFT];
    let mut raw = vec![0u8; FFT * 4];
    let mut powers = vec![0.0; FFT];
    for window in 0..windows {
        ensure!(!stop.load(Ordering::Relaxed), "spectrum interrupted");
        let offset = if windows == 1 {
            0
        } else {
            window as u64 * (samples - FFT as u64) / (windows - 1) as u64
        };
        file.seek(SeekFrom::Start(offset * 4))?;
        file.read_exact(&mut raw)?;
        for (i, bytes) in raw.as_chunks::<4>().0.iter().enumerate() {
            let sample = crate::receiver::parse_sample(bytes)?;
            buffer[i] = Complex::new(f64::from(sample.i) * hann[i], f64::from(sample.q) * hann[i]);
        }
        fft.process(&mut buffer);
        for (power, value) in powers.iter_mut().zip(&buffer) {
            *power += value.norm_sqr() / norm / windows as f64;
        }
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
            relative_power_db: 10.0 * powers[source].max(1e-24).log10(),
        });
    }
    let mut ranked = Vec::<(f64, f64)>::new();
    let low = ((center as f64 - 8_000_000.0) / 100_000.0).ceil() as i64;
    let high = ((center as f64 + 8_000_000.0) / 100_000.0).floor() as i64;
    for grid in low..=high {
        let frequency = grid as f64 * 100_000.0;
        let power = linear
            .iter()
            .filter(|(f, _)| (*f - frequency).abs() <= 80_000.0)
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
            relative_power_db: 10.0 * power.max(1e-24).log10(),
        });
        if candidates.len() == 12 {
            break;
        }
    }
    Ok(Report {
        sample_rate_hz: crate::SAMPLE_RATE,
        center_frequency_hz: center,
        input_samples: available,
        inspected_prefix_samples: samples,
        fft_size: FFT,
        windows,
        window: "symmetric_Hann",
        power_note: "Relative complex FFT-bin power with nominal Q11 scale and coherent-window normalization; not calibrated dBm. Candidate values integrate +/-80 kHz and are not directly comparable with one bin.",
        bins,
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn independent_analytic_tone_is_in_the_expected_shifted_bin() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tone.iq");
        let mut file = std::fs::File::create(&path).unwrap();
        for n in 0..FFT {
            let phase = std::f64::consts::TAU * 100.0 * n as f64 / FFT as f64;
            file.write_all(&((1000.0 * phase.cos()).round() as i16).to_le_bytes())
                .unwrap();
            file.write_all(&((1000.0 * phase.sin()).round() as i16).to_le_bytes())
                .unwrap();
        }
        drop(file);
        let report = survey(&path, 98_000_000, FFT as u64, 1, &AtomicBool::new(false)).unwrap();
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
}
