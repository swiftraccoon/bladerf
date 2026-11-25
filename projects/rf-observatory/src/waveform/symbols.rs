//! Recover unframed symbol decisions at a supplied or measured rate.

mod search;
mod timing;

use std::f64::consts::TAU;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use clap::ValueEnum;
use rustfft::num_complex::Complex;
use serde::Serialize;

use super::{BASEBAND_RATE, PhaseLine, check_stop, fsk::FrequencyPlateaus};

pub(super) const MIN_RATE_HZ: u32 = 8_000;
pub(super) const MAX_RATE_HZ: u32 = 200_000;
const MIN_SYMBOLS: usize = 12;
const MIN_CONSTELLATION_COHERENCE: f64 = 0.65;
const MIN_BLIND_AMPLITUDE_RATIO: f64 = 0.65;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum Modulation {
    Bpsk,
    Qpsk,
    BinaryFsk,
}

impl Modulation {
    fn order(self) -> u32 {
        match self {
            Self::Bpsk | Self::BinaryFsk => 2,
            Self::Qpsk => 4,
        }
    }
}

#[derive(Serialize)]
pub(super) struct SymbolRecovery {
    modulation: Modulation,
    symbol_rate_hz: u32,
    #[serde(flatten)]
    result: Recovery,
}

#[derive(Serialize)]
pub(super) struct BlindSymbolRecovery {
    modulation: Option<Modulation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_rate_hz: Option<f64>,
    #[serde(flatten)]
    result: Recovery,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Recovery {
    InsufficientModulationEvidence,
    AmbiguousModulationEvidence,
    InsufficientSymbols,
    NoQualifiedClock,
    AmbiguousClockCandidates {
        candidate_rates_hz: [f64; 2],
    },
    InsufficientTimingEvidence {
        clock: timing::ClockEvidence,
    },
    AmbiguousSymbolRate {
        clock: timing::ClockEvidence,
        compatible_lower_rate_hz: f64,
    },
    AmbiguousConstellation {
        clock: timing::ClockEvidence,
        constellation_coherence: f64,
    },
    InsufficientAmplitudeConsistency {
        clock: timing::ClockEvidence,
        amplitude_p20_p80_ratio: f64,
    },
    Recovered {
        carrier_offset_hz: f64,
        first_center_baseband_sample: f64,
        clock: timing::ClockEvidence,
        constellation_coherence: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        amplitude_p20_p80_ratio: Option<f64>,
        symbols: Vec<u8>,
    },
}

struct Decisions {
    samples: Vec<Complex<f64>>,
    carrier_offset_hz: f64,
    first_sample_position: f64,
}

fn refine_carrier(
    samples: &[Complex<f64>],
    order: u32,
    coarse_line_hz: f64,
    stop: &AtomicBool,
) -> Result<f64> {
    let len = samples.len() as f64;
    let rate = f64::from(BASEBAND_RATE);
    let powered: Vec<_> = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let weight = 0.5 - 0.5 * (TAU * index as f64 / (len - 1.0)).cos();
            sample.powu(order) * weight
        })
        .collect();
    let mut best_line_hz = coarse_line_hz;
    let mut best_power = 0.0;
    // Refine the FFT-bin estimate over one bin with symmetric local offsets.
    for offset in -8..=8 {
        check_stop(stop)?;
        let line_hz = coarse_line_hz + f64::from(offset) * rate / (16.0 * len);
        let step = Complex::from_polar(1.0, -TAU * line_hz / rate);
        let mut phase = Complex::new(1.0, 0.0);
        let mut sum = Complex::default();
        for &sample in &powered {
            sum += sample * phase;
            phase *= step;
        }
        let power = sum.norm_sqr();
        if power > best_power {
            best_line_hz = line_hz;
            best_power = power;
        }
    }
    Ok(best_line_hz / f64::from(order))
}

fn decision_samples(
    samples: &[Complex<f64>],
    modulation: Modulation,
    lines: &[PhaseLine; 3],
    phase_order_hint: Option<u32>,
    plateaus: Option<&FrequencyPlateaus>,
    stop: &AtomicBool,
) -> Result<Option<Decisions>> {
    let rate = f64::from(BASEBAND_RATE);
    if matches!(modulation, Modulation::BinaryFsk) {
        let Some([low, high]) = plateaus.and_then(FrequencyPlateaus::qualified_levels) else {
            return Ok(None);
        };
        let midpoint = (high + low) / 2.0;
        let deviation = (high - low) / 2.0;
        let samples = samples
            .windows(2)
            .map(|pair| {
                let offset_hz = (pair[0].conj() * pair[1]).arg() * rate / TAU;
                let level = ((offset_hz - midpoint) / deviation).clamp(-2.0, 2.0);
                Complex::new(level, 0.0)
            })
            .collect();
        return Ok(Some(Decisions {
            samples,
            carrier_offset_hz: midpoint,
            first_sample_position: 0.5,
        }));
    }

    let order = modulation.order();
    if phase_order_hint != Some(order) {
        return Ok(None);
    }
    let Some(coarse_line_hz) = lines
        .iter()
        .find(|line| line.exponent == order)
        .and_then(|line| line.peak_offset_hz)
    else {
        return Ok(None);
    };
    let carrier_offset_hz = refine_carrier(samples, order, coarse_line_hz, stop)?;
    let samples = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            sample * Complex::from_polar(1.0, -TAU * carrier_offset_hz * index as f64 / rate)
        })
        .collect();
    Ok(Some(Decisions {
        samples,
        carrier_offset_hz,
        first_sample_position: 0.0,
    }))
}

fn integrate_symbols(
    prefix: &[Complex<f64>],
    first_center: f64,
    period: f64,
    count: usize,
) -> Vec<Complex<f64>> {
    // The central half symbol avoids transition samples and filter ringing.
    (0..count)
        .map(|ordinal| {
            let center = first_center + ordinal as f64 * period;
            let start = (center - period / 4.0).round() as usize;
            let end = (center + period / 4.0).round() as usize;
            (prefix[end] - prefix[start]) / (end - start) as f64
        })
        .collect()
}

fn recover(
    decisions: &Decisions,
    rate: f64,
    order: u32,
    blind: bool,
    stop: &AtomicBool,
) -> Result<Recovery> {
    let period = f64::from(BASEBAND_RATE) / rate;
    let len = decisions.samples.len() as f64;
    if len / period < MIN_SYMBOLS as f64 {
        return Ok(Recovery::InsufficientSymbols);
    }
    check_stop(stop)?;
    let edges = timing::transitions(&decisions.samples, period, order);
    let clock = timing::fold(&edges, len / 2.0, period);
    if !clock.qualified {
        return Ok(Recovery::InsufficientTimingEvidence {
            clock: clock.evidence,
        });
    }
    for divisor in 2..=(rate / f64::from(MIN_RATE_HZ)) as u32 {
        check_stop(stop)?;
        if timing::fold(&edges, len / 2.0, period * f64::from(divisor)).qualified {
            return Ok(Recovery::AmbiguousSymbolRate {
                clock: clock.evidence,
                compatible_lower_rate_hz: rate / f64::from(divisor),
            });
        }
    }
    let mut boundary = clock.boundary;
    if boundary >= period - 2.0 {
        // Keep the first symbol when timing jitter wraps its boundary at epoch zero.
        boundary = 0.0;
    }
    let count = ((len - boundary) / period).floor() as usize;
    if count < MIN_SYMBOLS {
        return Ok(Recovery::InsufficientSymbols);
    }
    let mut prefix = Vec::with_capacity(decisions.samples.len() + 1);
    let mut sum = Complex::default();
    prefix.push(sum);
    for &sample in &decisions.samples {
        sum += sample;
        prefix.push(sum);
    }

    let first_center = boundary + period / 2.0;
    let integrated = integrate_symbols(&prefix, first_center, period, count);
    let moment: Complex<f64> = integrated.iter().map(|sample| sample.powu(order)).sum();
    let magnitude_sum = integrated
        .iter()
        .map(|sample| sample.norm().powi(order as i32))
        .sum::<f64>();
    let coherence = (moment.norm() / magnitude_sum.max(f64::MIN_POSITIVE)).min(1.0);
    let orientation = moment.arg() / f64::from(order);
    let symbols: Vec<_> = integrated
        .iter()
        .map(|&sample| timing::sector_index(sample, orientation, order))
        .collect();
    let changes = symbols.windows(2).filter(|pair| pair[0] != pair[1]).count();
    if coherence < MIN_CONSTELLATION_COHERENCE || changes < 3 {
        return Ok(Recovery::AmbiguousConstellation {
            clock: clock.evidence,
            constellation_coherence: coherence,
        });
    }
    let amplitude_p20_p80_ratio = blind.then(|| {
        let mut amplitudes: Vec<_> = integrated.iter().map(|sample| sample.norm()).collect();
        amplitudes.sort_unstable_by(f64::total_cmp);
        let last = amplitudes.len() - 1;
        amplitudes[last / 5] / amplitudes[last * 4 / 5].max(f64::MIN_POSITIVE)
    });
    if let Some(ratio) = amplitude_p20_p80_ratio
        && ratio < MIN_BLIND_AMPLITUDE_RATIO
    {
        return Ok(Recovery::InsufficientAmplitudeConsistency {
            clock: clock.evidence,
            amplitude_p20_p80_ratio: ratio,
        });
    }
    Ok(Recovery::Recovered {
        carrier_offset_hz: decisions.carrier_offset_hz,
        first_center_baseband_sample: first_center + decisions.first_sample_position - 0.5,
        clock: clock.evidence,
        constellation_coherence: coherence,
        amplitude_p20_p80_ratio,
        symbols,
    })
}

pub(super) fn measure(
    samples: &[Complex<f64>],
    modulation: Modulation,
    symbol_rate_hz: u32,
    lines: &[PhaseLine; 3],
    phase_order_hint: Option<u32>,
    plateaus: Option<&FrequencyPlateaus>,
    stop: &AtomicBool,
) -> Result<SymbolRecovery> {
    let result =
        match decision_samples(samples, modulation, lines, phase_order_hint, plateaus, stop)? {
            Some(decisions) => recover(
                &decisions,
                f64::from(symbol_rate_hz),
                modulation.order(),
                false,
                stop,
            )?,
            None => Recovery::InsufficientModulationEvidence,
        };
    Ok(SymbolRecovery {
        modulation,
        symbol_rate_hz,
        result,
    })
}

pub(super) fn estimate(
    samples: &[Complex<f64>],
    requested: Option<Modulation>,
    lines: &[PhaseLine; 3],
    phase_order_hint: Option<u32>,
    plateaus: Option<&FrequencyPlateaus>,
    stop: &AtomicBool,
) -> Result<BlindSymbolRecovery> {
    let modulation = if let Some(mode) = requested {
        mode
    } else {
        let fsk = plateaus
            .and_then(FrequencyPlateaus::qualified_levels)
            .is_some();
        match (phase_order_hint, fsk) {
            (Some(2), false) => Modulation::Bpsk,
            (Some(4), false) => Modulation::Qpsk,
            (None, true) => Modulation::BinaryFsk,
            (phase, fsk) => {
                return Ok(BlindSymbolRecovery {
                    modulation: None,
                    symbol_rate_hz: None,
                    result: if phase.is_some() && fsk {
                        Recovery::AmbiguousModulationEvidence
                    } else {
                        Recovery::InsufficientModulationEvidence
                    },
                });
            }
        }
    };
    let mut report = BlindSymbolRecovery {
        modulation: Some(modulation),
        symbol_rate_hz: None,
        result: Recovery::InsufficientModulationEvidence,
    };
    let Some(decisions) =
        decision_samples(samples, modulation, lines, phase_order_hint, plateaus, stop)?
    else {
        return Ok(report);
    };
    let order = modulation.order();
    report.result = match search::find_rate(&decisions.samples, order, stop)? {
        search::RateSearch::NoClock => Recovery::NoQualifiedClock,
        search::RateSearch::Ambiguous(candidate_rates_hz) => {
            Recovery::AmbiguousClockCandidates { candidate_rates_hz }
        }
        search::RateSearch::Found(rate_hz) => {
            let result = recover(&decisions, rate_hz, order, true, stop)?;
            if matches!(result, Recovery::Recovered { .. }) {
                report.symbol_rate_hz = Some(rate_hz);
            }
            result
        }
    };
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_fractional_symbol_centers_and_rejects_a_clock_multiple() {
        let rate = 51_000;
        let period = f64::from(BASEBAND_RATE) / f64::from(rate);
        let start = 11.0;
        let mut state = 17u32;
        let truth: Vec<_> = (0..128)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state % 4) as u8
            })
            .collect();
        let decisions = Decisions {
            samples: (0..4800)
                .map(|sample| {
                    let symbol = ((f64::from(sample) + start) / period) as usize;
                    Complex::from_polar(0.6, 0.3 + TAU * f64::from(truth[symbol]) / 4.0)
                })
                .collect(),
            carrier_offset_hz: 0.0,
            first_sample_position: 0.0,
        };
        let stop = AtomicBool::new(false);
        let Recovery::Recovered {
            first_center_baseband_sample,
            symbols,
            ..
        } = recover(&decisions, f64::from(rate), 4, false, &stop).unwrap()
        else {
            panic!("fixture clock must qualify");
        };
        assert!(symbols.len() > 100);
        let first = ((first_center_baseband_sample + start) / period) as usize;
        let rotation = (truth[first] + 4 - symbols[0]) % 4;
        for (symbol, expected) in symbols.into_iter().zip(&truth[first..]) {
            assert_eq!((symbol + rotation) % 4, *expected);
        }
        assert!(matches!(
            recover(&decisions, 2.0 * f64::from(rate), 4, false, &stop).unwrap(),
            Recovery::AmbiguousSymbolRate { .. }
        ));
        assert!(matches!(
            recover(&decisions, 58_000.0, 4, false, &stop).unwrap(),
            Recovery::InsufficientTimingEvidence { .. }
        ));
        let search::RateSearch::Found(measured_hz) =
            search::find_rate(&decisions.samples, 4, &stop).unwrap()
        else {
            panic!("the fractional fixture must support a unique fundamental");
        };
        assert!((measured_hz - f64::from(rate)).abs() < 50.0);
    }
}
