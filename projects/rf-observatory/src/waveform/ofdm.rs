//! Cyclic-prefix measurements on original-rate IQ from an offline clip.

use std::f64::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};
use clap::{Args, ValueEnum};
use rustfft::num_complex::Complex;
use serde::Serialize;

use super::{check_stop, iq, publish_report};

const RUN_SYMBOLS: usize = 8;
const MIN_CORRELATION: f64 = 0.56;
const MIN_SYMBOL_CORRELATION: f64 = 0.42;
const MIN_CONTROL_MARGIN: f64 = 0.16;

#[derive(Debug, Args)]
pub(crate) struct ProbeOfdm {
    /// Saved 20 MSps SC16 Q11 clip, up to 245,760 complex samples.
    input: PathBuf,
    /// First input sample to inspect, local to the clip.
    #[arg(long)]
    start_sample: usize,
    /// Exclusive end of inspected samples, local to the clip.
    #[arg(long)]
    end_sample: usize,
    /// Useful sample count per symbol. Selects the cyclic-prefix lengths to test.
    #[arg(long, value_enum, default_value = "64")]
    useful_samples: UsefulSamples,
    /// Optional SHA-256 identity from the clip receipt.
    #[arg(long)]
    expect_sha256: Option<String>,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum UsefulSamples {
    #[value(name = "64")]
    Samples64,
    #[value(name = "256")]
    Samples256,
}

struct Search {
    useful_samples: usize,
    prefixes: &'static [usize],
    lag_control_delta: usize,
}

impl UsefulSamples {
    fn search(self) -> Search {
        match self {
            Self::Samples64 => Search {
                useful_samples: 64,
                prefixes: &[16, 8],
                lag_control_delta: 4,
            },
            Self::Samples256 => Search {
                useful_samples: 256,
                prefixes: &[64, 32, 16],
                lag_control_delta: 8,
            },
        }
    }
}

impl Search {
    fn phase_controls(&self, prefix: usize) -> [usize; 2] {
        let midpoint = (self.useful_samples + prefix) / 2;
        let second = if self.useful_samples == 256 {
            // Keep both off-prefix windows disjoint for the 64-sample guard.
            midpoint.max(3 * prefix)
        } else {
            midpoint
        };
        [2 * prefix, second]
    }

    fn required_samples(&self, prefix: usize) -> usize {
        RUN_SYMBOLS * (self.useful_samples + prefix)
            + self.phase_controls(prefix)[1].max(self.lag_control_delta)
    }

    fn minimum_interval(&self) -> usize {
        self.prefixes
            .iter()
            .map(|&prefix| self.required_samples(prefix))
            .max()
            .expect("search defines at least one prefix length")
    }
}

#[derive(Debug, Serialize)]
struct Detection {
    useful_samples: usize,
    prefix_samples: usize,
    first_prefix_sample: usize,
    end_sample_exclusive: usize,
    symbol_count: usize,
    supported_symbols: usize,
    coherent_correlation: f64,
    control_correlation: f64,
    offset_modulo_subcarrier_hz: f64,
}

impl Detection {
    fn margin(&self) -> f64 {
        self.coherent_correlation - self.control_correlation
    }
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    source_sha256: String,
    source_complex_samples: usize,
    source_sample_rate_hz: u32,
    start_sample: usize,
    end_sample_exclusive: usize,
    tested_useful_samples: usize,
    tested_prefix_samples: &'static [usize],
    detection: Option<Detection>,
}

/// Each value is the coherent lag product of a sliding guard-length window,
/// normalized by both windows' combined energy.
fn correlations(
    samples: &[Complex<f64>],
    lag: usize,
    width: usize,
    stop: &AtomicBool,
) -> Result<Vec<Complex<f64>>> {
    let pair = |index: usize| {
        let left = samples[index];
        let right = samples[index + lag];
        (left.conj() * right, left.norm_sqr() + right.norm_sqr())
    };
    let mut product = Complex::default();
    let mut energy = 0.0;
    for index in 0..width {
        let (value, power) = pair(index);
        product += value;
        energy += power;
    }
    let windows = samples.len() - (lag + width) + 1;
    let mut result = Vec::with_capacity(windows);
    for start in 0..windows {
        if start.is_multiple_of(4096) {
            check_stop(stop)?;
        }
        result.push(if energy > 0.0 {
            product * (2.0 / energy)
        } else {
            Complex::default()
        });
        if start + 1 < windows {
            let (old_product, old_energy) = pair(start);
            let (new_product, new_energy) = pair(start + width);
            product += new_product - old_product;
            energy += new_energy - old_energy;
        }
    }
    Ok(result)
}

fn distributed_pair_energy(
    samples: &[Complex<f64>],
    start: usize,
    useful_samples: usize,
    prefix: usize,
) -> bool {
    let delayed = start + useful_samples;
    let pairs = samples[start..start + prefix]
        .iter()
        .zip(&samples[delayed..delayed + prefix]);
    let mut total = 0.0;
    let mut squared_total = 0.0;
    for (left, right) in pairs {
        let energy = left.norm_sqr() + right.norm_sqr();
        total += energy;
        squared_total += energy * energy;
    }
    // Reject a correlation carried by just one or two repeated impulses.
    total * total >= (prefix / 4).max(3) as f64 * squared_total && squared_total > 0.0
}

fn measure(
    samples: &[Complex<f64>],
    source_start: usize,
    search: &Search,
    stop: &AtomicBool,
) -> Result<Option<Detection>> {
    let mut best: Option<Detection> = None;
    let useful = search.useful_samples;
    for &prefix in search.prefixes {
        let period = useful + prefix;
        let run_samples = RUN_SYMBOLS * period;
        let phase_controls = search.phase_controls(prefix);
        let on = correlations(samples, useful, prefix, stop)?;
        let early_lag = correlations(samples, useful - search.lag_control_delta, prefix, stop)?;
        let late_lag = correlations(samples, useful + search.lag_control_delta, prefix, stop)?;
        for first in 0..=samples.len() - search.required_samples(prefix) {
            if first.is_multiple_of(4096) {
                check_stop(stop)?;
            }
            let observed: [Complex<f64>; RUN_SYMBOLS] =
                std::array::from_fn(|symbol| on[first + symbol * period]);
            let mean = observed.iter().sum::<Complex<f64>>() / RUN_SYMBOLS as f64;
            let strength = mean.norm().min(1.0);
            if strength < MIN_CORRELATION {
                continue;
            }
            // Demand repeated correlation with the same phase in both halves.
            let reference = mean.conj() / mean.norm();
            let support = |half: &[Complex<f64>], first_symbol: usize| {
                half.iter()
                    .enumerate()
                    .filter(|&(index, &sample)| {
                        (sample * reference).re >= MIN_SYMBOL_CORRELATION
                            && distributed_pair_energy(
                                samples,
                                first + (first_symbol + index) * period,
                                useful,
                                prefix,
                            )
                    })
                    .count()
            };
            let (first_half, second_half) = observed.split_at(RUN_SYMBOLS / 2);
            let supported = [
                support(first_half, 0),
                support(second_half, RUN_SYMBOLS / 2),
            ];
            if supported.iter().any(|&count| count < 3) {
                continue;
            }

            // A stationary carrier can correlate at every phase and lag.
            let average_magnitude = |values: &[Complex<f64>], offset| {
                values[first + offset..]
                    .iter()
                    .step_by(period)
                    .take(RUN_SYMBOLS)
                    .map(|value| value.norm())
                    .sum::<f64>()
                    / RUN_SYMBOLS as f64
            };
            let control = [
                average_magnitude(&on, phase_controls[0]),
                average_magnitude(&on, phase_controls[1]),
                average_magnitude(&early_lag, 0),
                average_magnitude(&late_lag, 0),
            ]
            .into_iter()
            .fold(0.0, f64::max)
            .min(1.0);
            let margin = strength - control;
            if margin < MIN_CONTROL_MARGIN
                || best.as_ref().is_some_and(|old| old.margin() >= margin)
            {
                continue;
            }
            best = Some(Detection {
                useful_samples: useful,
                prefix_samples: prefix,
                first_prefix_sample: source_start + first,
                end_sample_exclusive: source_start + first + run_samples,
                symbol_count: RUN_SYMBOLS,
                supported_symbols: supported.iter().sum(),
                coherent_correlation: strength,
                control_correlation: control,
                offset_modulo_subcarrier_hz: mean.arg() * f64::from(crate::SAMPLE_RATE)
                    / (TAU * useful as f64),
            });
        }
    }
    Ok(best)
}

pub(crate) fn probe(root: &Path, args: &ProbeOfdm, stop: &AtomicBool) -> Result<()> {
    let extent = args.start_sample..args.end_sample;
    let search = args.useful_samples.search();
    let minimum_interval = search.minimum_interval();
    ensure!(
        extent.len() >= minimum_interval,
        "OFDM interval requires at least {minimum_interval} complex samples"
    );
    let clip = iq::read_clip(&args.input, stop)?;
    if let Some(expected) = &args.expect_sha256 {
        ensure!(
            *expected == clip.source_sha256,
            "saved IQ does not match the expected SHA-256"
        );
    }
    let samples = clip.samples(extent.clone(), stop)?;
    let report = Report {
        schema_version: 1,
        operation: "probe_saved_iq_cyclic_prefix",
        source_complex_samples: clip.source_samples(),
        source_sha256: clip.source_sha256,
        source_sample_rate_hz: crate::SAMPLE_RATE,
        start_sample: extent.start,
        end_sample_exclusive: extent.end,
        tested_useful_samples: search.useful_samples,
        tested_prefix_samples: search.prefixes,
        detection: measure(&samples, extent.start, &search, stop)?,
    };
    let output = publish_report(root, &args.output, &report, stop)?;
    println!("Saved cyclic-prefix measurements to {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(useful_samples: usize, prefix: usize, copy_prefix: bool) -> Vec<Complex<f64>> {
        let mut state = 0x1983_45be_80cb_7821_u64;
        let mut next = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            f64::from((state >> 48) as i16 % 128)
        };
        let mut samples = vec![Complex::default(); 21];
        for _ in 0..12 {
            let useful: Vec<_> = (0..useful_samples)
                .map(|_| Complex::new(next(), next()))
                .collect();
            if copy_prefix {
                samples.extend_from_slice(&useful[useful_samples - prefix..]);
            } else {
                samples.extend((0..prefix).map(|_| Complex::new(next(), next())));
            }
            samples.extend(useful);
        }
        samples.resize(samples.len().max(1200), Complex::default());
        samples
    }

    #[test]
    fn detects_each_prefix_length_and_reports_frequency_modulo_subcarrier_spacing() {
        for family in [UsefulSamples::Samples64, UsefulSamples::Samples256] {
            let search = family.search();
            let useful = search.useful_samples;
            let spacing = f64::from(crate::SAMPLE_RATE) / useful as f64;
            for &prefix in search.prefixes {
                let mut samples = symbols(useful, prefix, true);
                let offset_hz = -47_000.0;
                for (index, sample) in samples.iter_mut().enumerate() {
                    *sample *= Complex::from_polar(
                        1.0,
                        TAU * offset_hz * index as f64 / f64::from(crate::SAMPLE_RATE),
                    );
                }
                let detection = measure(&samples, 500, &search, &AtomicBool::new(false))
                    .unwrap()
                    .unwrap();
                assert_eq!(detection.useful_samples, useful);
                assert_eq!(detection.prefix_samples, prefix);
                let period = useful + prefix;
                let phase = (detection.first_prefix_sample + period - 521) % period;
                assert!(phase.min(period - phase) <= 1);
                let expected = (offset_hz + spacing / 2.0).rem_euclid(spacing) - spacing / 2.0;
                assert!(
                    (detection.offset_modulo_subcarrier_hz - expected).abs() < spacing / 200.0,
                    "{detection:?}"
                );
            }
        }
    }

    #[test]
    fn abstains_on_independent_guard_samples_and_a_stationary_carrier() {
        let stop = AtomicBool::new(false);
        for family in [UsefulSamples::Samples64, UsefulSamples::Samples256] {
            let search = family.search();
            for &prefix in search.prefixes {
                let samples = symbols(search.useful_samples, prefix, false);
                assert!(measure(&samples, 0, &search, &stop).unwrap().is_none());
            }
            let tone: Vec<_> = (0..search.minimum_interval())
                .map(|index| Complex::from_polar(100.0, 0.28 * index as f64))
                .collect();
            assert!(measure(&tone, 0, &search, &stop).unwrap().is_none());
        }
    }

    #[test]
    fn abstains_on_periodic_pairs_of_sparse_impulses() {
        let stop = AtomicBool::new(false);
        for family in [UsefulSamples::Samples64, UsefulSamples::Samples256] {
            let search = family.search();
            let useful = search.useful_samples;
            let mut samples = vec![Complex::default(); search.minimum_interval() + 100];
            for &prefix in search.prefixes {
                samples.fill(Complex::default());
                for start in (3..samples.len() - useful).step_by(useful + prefix) {
                    samples[start] = Complex::new(100.0, 20.0);
                    samples[start + useful] = samples[start];
                }
                assert!(measure(&samples, 0, &search, &stop).unwrap().is_none());
            }
        }
    }
}
