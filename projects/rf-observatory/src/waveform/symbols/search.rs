//! Propose bounded symbol rates from recurring transition times.

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use rustfft::{FftPlanner, num_complex::Complex};

use super::{BASEBAND_RATE, MAX_RATE_HZ, MIN_RATE_HZ, check_stop, timing};

const REFERENCE_PERIODS: [f64; 3] = [16.0, 40.0, 100.0];
const FFT_OVERSAMPLE: usize = 8;
const MAX_PEAKS: usize = 48;
const MIN_SEED_ALIGNMENT: f64 = 0.45;
const ALIGNMENT_MARGIN: f64 = 0.10;

pub(super) enum RateSearch {
    NoClock,
    Ambiguous([f64; 2]),
    Found(f64),
}

#[derive(Clone, Copy)]
struct Candidate {
    rate_hz: f64,
    alignment: f64,
}

fn transition_spectrum(
    samples: &[Complex<f64>],
    order: u32,
    size: usize,
    first_bin: usize,
    last_bin: usize,
    stop: &AtomicBool,
) -> Result<Vec<f64>> {
    let mut powers = vec![0.0f64; last_bin - first_bin + 1];
    let transform = FftPlanner::new().plan_fft_forward(size);
    let mut fft = vec![Complex::<f64>::default(); size];
    let mut scratch = vec![Complex::default(); transform.get_inplace_scratch_len()];
    for period in REFERENCE_PERIODS {
        check_stop(stop)?;
        let edges = timing::transitions(samples, period, order);
        if edges.len() < timing::MIN_TRANSITIONS {
            continue;
        }
        fft.fill(Complex::default());
        for &edge in &edges {
            let index = edge.floor() as usize;
            let fraction = edge - index as f64;
            fft[index].re += 1.0 - fraction;
            fft[index + 1].re += fraction;
        }
        transform.process_with_scratch(&mut fft, &mut scratch);
        for (power, bin) in powers.iter_mut().zip(&fft[first_bin..=last_bin]) {
            *power = power.max(bin.norm() / edges.len() as f64);
        }
    }
    Ok(powers)
}

fn refine(samples: &[Complex<f64>], order: u32, seed_hz: f64, bin_hz: f64) -> Option<Candidate> {
    let rate = f64::from(BASEBAND_RATE);
    let midpoint = samples.len() as f64 / 2.0;
    let edges = timing::transitions(samples, rate / seed_hz, order);
    (-8..=8)
        .map(|step| seed_hz + f64::from(step) * bin_hz / 8.0)
        .filter(|trial| (f64::from(MIN_RATE_HZ)..=f64::from(MAX_RATE_HZ)).contains(trial))
        .filter_map(|rate_hz| {
            let clock = timing::fold(&edges, midpoint, rate / rate_hz);
            clock.qualified.then_some(Candidate {
                rate_hz,
                alignment: clock.evidence.alignment(),
            })
        })
        .max_by(|a, b| a.alignment.total_cmp(&b.alignment))
}

fn harmonic(a: f64, b: f64, tolerance_hz: f64) -> bool {
    let lower = a.min(b);
    let upper = a.max(b);
    (upper - (upper / lower).round() * lower).abs() <= tolerance_hz
}

pub(super) fn find_rate(
    samples: &[Complex<f64>],
    order: u32,
    stop: &AtomicBool,
) -> Result<RateSearch> {
    let size = samples.len().next_power_of_two() * FFT_OVERSAMPLE;
    let bin_hz = f64::from(BASEBAND_RATE) / size as f64;
    let first_bin = (f64::from(MIN_RATE_HZ) / bin_hz).floor() as usize;
    let last_bin = (f64::from(MAX_RATE_HZ) / bin_hz).ceil() as usize;
    let spectrum = transition_spectrum(samples, order, size, first_bin, last_bin, stop)?;
    let mut peaks: Vec<_> = spectrum
        .iter()
        .copied()
        .enumerate()
        .filter(|&(index, score)| {
            score >= MIN_SEED_ALIGNMENT
                && (index == 0 || score > spectrum[index - 1])
                && spectrum.get(index + 1).is_none_or(|&next| score >= next)
        })
        .collect();
    peaks.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut candidates = Vec::new();
    for (index, _) in peaks.into_iter().take(MAX_PEAKS) {
        check_stop(stop)?;
        let seed = (index + first_bin) as f64 * bin_hz;
        if let Some(candidate) = refine(samples, order, seed, bin_hz) {
            candidates.push(candidate);
        }
    }
    let Some(best) = candidates
        .iter()
        .copied()
        .max_by(|a, b| a.alignment.total_cmp(&b.alignment))
    else {
        return Ok(RateSearch::NoClock);
    };
    let close = |candidate: &&Candidate| candidate.alignment >= best.alignment - ALIGNMENT_MARGIN;
    // Related peaks can describe integer multiples of one clock. Prefer the
    // lowest supported rate and abstain if an unrelated candidate is close.
    let tolerance = 2.0 * bin_hz;
    let fundamental = candidates
        .iter()
        .filter(close)
        .filter(|candidate| harmonic(candidate.rate_hz, best.rate_hz, tolerance))
        .min_by(|a, b| a.rate_hz.total_cmp(&b.rate_hz))
        .unwrap_or(&best);
    if let Some(rival) = candidates
        .iter()
        .filter(close)
        .find(|candidate| !harmonic(candidate.rate_hz, fundamental.rate_hz, tolerance))
    {
        return Ok(RateSearch::Ambiguous([fundamental.rate_hz, rival.rate_hz]));
    }
    Ok(RateSearch::Found(fundamental.rate_hz))
}
