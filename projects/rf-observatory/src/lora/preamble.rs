//! Measure repeated chirp structure at a selected LoRa symbol geometry.

use std::f64::consts::TAU;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};
use serde::Serialize;

pub(super) const MIN_UP_WINDOWS: usize = 5;
pub(super) const MIN_PEAK_TO_MEAN: f32 = 24.0;
pub(super) const DELIMITER_SEARCH_WINDOWS: usize = 5;
const BIN_TOLERANCE: usize = 1;
const SLOPE_POWER_MARGIN: f32 = 2.0;

#[derive(Serialize)]
pub(super) struct Detection {
    first_up_window: usize,
    up_windows: usize,
    up_peak_bin: usize,
    minimum_up_peak_fraction: f32,
    delimiter_window: usize,
    down_peak_bin: usize,
    down_peak_fraction: f32,
}

#[derive(Clone, Copy, Default)]
struct Peak {
    bin: usize,
    fraction: f32,
}

#[derive(Clone, Copy)]
struct Run {
    first_window: usize,
    windows: usize,
    first_peak_bin: usize,
    minimum_fraction: f32,
}

impl Run {
    fn new(window: usize, peak: Peak) -> Self {
        Self {
            first_window: window,
            windows: 1,
            first_peak_bin: peak.bin,
            minimum_fraction: peak.fraction,
        }
    }

    fn accepts(self, peak: Peak, bins: usize) -> bool {
        let distance = peak.bin.abs_diff(self.first_peak_bin);
        distance.min(bins - distance) <= BIN_TOLERANCE
    }

    fn extend(&mut self, peak: Peak) {
        self.windows += 1;
        self.minimum_fraction = self.minimum_fraction.min(peak.fraction);
    }

    fn delimiter_window_is_close(self, window: usize) -> bool {
        window < self.first_window + self.windows + DELIMITER_SEARCH_WINDOWS
    }
}

/// Nonoverlapping chip-rate windows and a bounded preamble search.
/// Each unmodulated upchirp is periodic, allowing run detection at an
/// arbitrary window phase. A subsequent opposite slope supplies a control.
pub(super) struct Detector {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    spectrum: Vec<Complex32>,
    upchirp: Vec<Complex32>,
    input: Vec<Complex32>,
    processed_windows: usize,
    up_run: Option<Run>,
    pending_delimiter: Option<Run>,
}

impl Detector {
    pub(super) fn new(chips_per_symbol: usize) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(chips_per_symbol);
        let upchirp = (0..chips_per_symbol)
            .map(|index| {
                let chip = index as f64;
                let phase = (TAU / 2.0) * chip * (chip / chips_per_symbol as f64 - 1.0);
                let (sin, cos) = phase.sin_cos();
                Complex32::new(cos as f32, sin as f32)
            })
            .collect();
        Self {
            scratch: vec![Complex32::default(); fft.get_inplace_scratch_len()],
            spectrum: vec![Complex32::default(); chips_per_symbol],
            fft,
            upchirp,
            input: Vec::with_capacity(chips_per_symbol),
            processed_windows: 0,
            up_run: None,
            pending_delimiter: None,
        }
    }

    pub(super) fn processed_windows(&self) -> usize {
        self.processed_windows
    }

    pub(super) fn minimum_peak_fraction(&self) -> f32 {
        MIN_PEAK_TO_MEAN / self.upchirp.len() as f32
    }

    pub(super) fn push(&mut self, sample: Complex32) -> Option<Detection> {
        self.input.push(sample);
        if self.input.len() != self.upchirp.len() {
            return None;
        }
        let detection = self.inspect_window();
        self.input.clear();
        self.processed_windows += 1;
        detection
    }

    fn inspect_window(&mut self) -> Option<Detection> {
        let window = self.processed_windows;
        let normalization =
            self.input.iter().map(Complex32::norm_sqr).sum::<f32>() * self.input.len() as f32;
        let up = self.peak(normalization, true);
        let minimum_fraction = self.minimum_peak_fraction();
        let qualified_up = (up.fraction >= minimum_fraction).then_some(up);
        if let (Some(run), Some(peak)) = (&mut self.up_run, qualified_up)
            && run.accepts(peak, self.upchirp.len())
        {
            run.extend(peak);
        } else {
            if let Some(run) = self
                .up_run
                .take()
                .filter(|run| run.windows >= MIN_UP_WINDOWS)
            {
                self.pending_delimiter = Some(run);
            }
            self.up_run = qualified_up.map(|peak| Run::new(window, peak));
        }
        let run = self
            .pending_delimiter
            .filter(|run| run.delimiter_window_is_close(window));
        self.pending_delimiter = run;
        let run = run?;
        let down = self.peak(normalization, false);
        if down.fraction < minimum_fraction || down.fraction < SLOPE_POWER_MARGIN * up.fraction {
            return None;
        }
        self.up_run = None;
        self.pending_delimiter = None;
        Some(Detection {
            first_up_window: run.first_window,
            up_windows: run.windows,
            up_peak_bin: run.first_peak_bin,
            minimum_up_peak_fraction: run.minimum_fraction,
            delimiter_window: window,
            down_peak_bin: down.bin,
            down_peak_fraction: down.fraction,
        })
    }

    fn peak(&mut self, normalization: f32, rising: bool) -> Peak {
        if normalization == 0.0 {
            return Peak::default();
        }
        for ((bin, sample), chirp) in self.spectrum.iter_mut().zip(&self.input).zip(&self.upchirp) {
            let reference = if rising { chirp.conj() } else { *chirp };
            *bin = sample * reference;
        }
        self.fft
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        let mut peak_bin = 0;
        let mut peak_power = 0.0;
        for (bin, value) in self.spectrum.iter().enumerate() {
            let power = value.norm_sqr();
            if power > peak_power {
                peak_bin = bin;
                peak_power = power;
            }
        }
        Peak {
            bin: peak_bin,
            fraction: (peak_power / normalization).min(1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHIPS: usize = 128;

    fn chirp(chips: usize) -> Vec<Complex32> {
        let mut phase = 0.0;
        (0..chips)
            .map(|index| {
                let sample = Complex32::from_polar(1.0, phase as f32);
                let frequency = (index as f64 + 0.5) / chips as f64 - 0.5;
                phase += TAU * frequency;
                sample
            })
            .collect()
    }

    fn burst(chips: usize, up_count: usize, with_delimiter: bool) -> Vec<Complex32> {
        let up = chirp(chips);
        let mut samples = vec![Complex32::default(); chips / 3];
        for _ in 0..up_count {
            samples.extend_from_slice(&up);
        }
        if with_delimiter {
            for shift in [17, 36] {
                samples.extend((0..chips).map(|sample| up[(sample + shift) % chips]));
            }
            samples.extend(up.iter().cycle().take(chips * 9 / 4).map(Complex32::conj));
        }
        samples.resize(samples.len() + 2 * chips, Complex32::default());
        samples
    }

    #[test]
    fn repeated_chirps_and_delimiter_survive_timing_and_carrier_offset() {
        let mut detector = Detector::new(CHIPS);
        let found = burst(CHIPS, 8, true)
            .into_iter()
            .enumerate()
            .filter_map(|(index, sample)| {
                let phase = TAU * 2.4 * index as f64 / CHIPS as f64;
                detector.push(sample * Complex32::from_polar(1.0, phase as f32))
            })
            .collect::<Vec<_>>();
        let [detection] = found.as_slice() else {
            panic!("expected one preamble, detected {}", found.len());
        };
        assert!(detection.up_windows >= MIN_UP_WINDOWS);
        assert!(detection.delimiter_window > detection.first_up_window);
        assert!(detection.minimum_up_peak_fraction > 0.45);
        assert!(detection.down_peak_fraction >= detector.minimum_peak_fraction());
    }

    #[test]
    fn rejects_missing_delimiter_and_stationary_tone() {
        let tone = (0..12 * CHIPS).map(|index| {
            Complex32::from_polar(1.0, (TAU * 3.1 * index as f64 / CHIPS as f64) as f32)
        });
        let mut detector = Detector::new(CHIPS);
        assert!(
            burst(CHIPS, 10, false)
                .into_iter()
                .chain(tone)
                .all(|sample| detector.push(sample).is_none())
        );
    }

    #[test]
    fn long_symbols_retain_chirp_evidence_below_the_noise_power() {
        let chips = 2_048;
        let mut detector = Detector::new(chips);
        let mut noise_detector = Detector::new(chips);
        let mut seed = 4_138_963_u32;
        let standard_deviation = (10.0f32.powf(1.5) / 2.0).sqrt();
        let mut component = || {
            // Twelve centered uniforms sum to approximately unit variance.
            let sum = (0..12)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 16) as f32 / 65_535.0 - 0.5
                })
                .sum::<f32>();
            sum * standard_deviation
        };
        let mut detections = Vec::new();
        for sample in burst(chips, 16, true) {
            let noise = Complex32::new(component(), component());
            assert!(noise_detector.push(noise).is_none());
            detections.extend(detector.push(sample + noise));
        }
        assert_eq!(detections.len(), 1);
        assert!(detections[0].minimum_up_peak_fraction < 0.05);
    }
}
