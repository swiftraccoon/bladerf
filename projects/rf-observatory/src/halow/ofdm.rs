//! Timing, channel estimation, and symbol recovery for 2 MHz S1G OFDM.

use std::{f32::consts::TAU, sync::Arc};

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

const RATE: f32 = 2_000_000.0;
const FFT_LEN: usize = 64;
const LONG_GI: usize = 16;
const SHORT_GI: usize = 8;
const SYMBOL_LEN: usize = FFT_LEN + LONG_GI;
const LTF_START: usize = 192;
const SIG_START: usize = 320;
const SHORT_DATA_START: usize = 480;
const LONG_DATA_START: usize = 720;
const LTF_SEARCH_RADIUS: usize = 48;
const MAX_DATA_SYMBOLS: usize = 4095;
const FIXED_PILOTS: [i32; 4] = [-21, -7, 7, 21];
const PILOT_SIGNS: [f32; 4] = [1.0, 1.0, 1.0, -1.0];

// IEEE 802.11 S1G 2 MHz long training tones, from subcarrier -28 through +28
// with the DC carrier omitted.
#[rustfmt::skip]
const TRAINING_SIGNS: [i8; 56] = [
     1,  1,  1,  1, -1, -1,  1,  1, -1,  1, -1,  1,  1,  1,
     1,  1,  1, -1, -1,  1,  1, -1,  1, -1,  1,  1,  1,  1,
     1, -1, -1,  1,  1, -1,  1, -1,  1, -1, -1, -1, -1, -1,
     1,  1, -1, -1,  1, -1,  1, -1,  1,  1,  1,  1, -1, -1,
];

const TRAVELING_PILOTS: [[i32; 4]; 14] = [
    [-28, -12, 4, 20],
    [-24, -8, 8, 24],
    [-20, -4, 12, 28],
    [-16, -2, 16, 26],
    [-26, -14, 2, 14],
    [-22, -10, 6, 18],
    [-18, -6, 10, 22],
    [-27, -11, 5, 21],
    [-23, -7, 9, 25],
    [-19, -3, 13, 23],
    [-15, 1, 17, 27],
    [-25, -13, -1, 11],
    [-21, -9, 3, 15],
    [-17, -5, 7, 19],
];

const PILOT_POLARITY: [f32; 127] = {
    let mut polarity = [0.0; 127];
    let mut state = 0x7f_u8;
    let mut index = 0;
    while index < polarity.len() {
        let feedback = (state >> 6 ^ state >> 3) & 1;
        polarity[index] = if feedback == 0 { 1.0 } else { -1.0 };
        state = (state << 1 | feedback) & 0x7f;
        index += 1;
    }
    polarity
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Format {
    Short,
    Long,
}

impl Format {
    fn data_start(self) -> usize {
        match self {
            Self::Short => SHORT_DATA_START,
            Self::Long => LONG_DATA_START,
        }
    }
}

pub(super) struct Preamble {
    /// Index of the first STF sample in the 2 MSps input.
    pub(super) start: usize,
    /// Observed carrier frequency offset in hertz.
    pub(super) cfo_hz: f32,
    channel: Channel,
}

pub(super) struct SigTones {
    pub(super) format: Format,
    /// BPSK soft values by symbol and ascending carrier. Positive favors one.
    pub(super) soft: [[f32; 48]; 2],
}

#[derive(Clone, Copy)]
pub(super) struct DataParameters {
    pub(super) format: Format,
    pub(super) symbols: usize,
    pub(super) short_gi: bool,
    pub(super) traveling_pilots: bool,
}

pub(super) struct DataTones {
    /// Equalized data carriers in ascending order with pilots removed.
    pub(super) tones: Vec<[Complex32; 52]>,
    /// Exclusive end index in the 2 MSps input.
    pub(super) end: usize,
}

struct Channel {
    inverse: [Complex32; FFT_LEN],
}

impl Channel {
    fn estimate(first: [Complex32; FFT_LEN], second: [Complex32; FFT_LEN]) -> Option<Self> {
        let mut inverse = [Complex32::default(); FFT_LEN];
        let mut power = 0.0;
        for (carrier, sign) in active_carriers(28).zip(TRAINING_SIGNS) {
            let bin = fft_bin(carrier);
            let estimate = (first[bin] + second[bin]) * (f32::from(sign) * 0.5);
            let tone_power = estimate.norm_sqr();
            power += tone_power;
            inverse[bin] = estimate.conj();
        }
        if !power.is_finite() || power <= f32::MIN_POSITIVE {
            return None;
        }
        let power_floor = power / TRAINING_SIGNS.len() as f32 * 1e-4;
        for tone in &mut inverse {
            *tone /= tone.norm_sqr().max(power_floor);
        }
        Some(Self { inverse })
    }

    fn equalize(&self, tones: &mut [Complex32; FFT_LEN]) {
        for (tone, inverse) in tones.iter_mut().zip(self.inverse) {
            *tone *= inverse;
        }
    }
}

#[derive(Default)]
struct PilotPhase {
    offset: f32,
    slope: f32,
}

impl PilotPhase {
    fn correct(&mut self, tones: &mut [Complex32; FFT_LEN], pilots: [i32; 4], signs: [f32; 4]) {
        // Fit the phase drift relative to the preceding symbol. Keeping each
        // residual near zero avoids discontinuities when phase wraps at pi.
        let mut carrier_sum = 0.0;
        let mut carrier_squares = 0.0;
        let mut phase_sum = 0.0;
        let mut carrier_phases = 0.0;
        let mut count = 0.0;
        for (carrier, sign) in pilots.into_iter().zip(signs) {
            let observed = tones[fft_bin(carrier)];
            let power = observed.norm_sqr();
            if !power.is_finite() || power < 0.04 {
                continue;
            }
            let carrier = carrier as f32;
            let residual = observed * sign * Complex32::from_polar(1.0, -self.at(carrier));
            let phase = residual.arg();
            carrier_sum += carrier;
            carrier_squares += carrier * carrier;
            phase_sum += phase;
            carrier_phases += carrier * phase;
            count += 1.0;
        }
        let determinant = count * carrier_squares - carrier_sum * carrier_sum;
        if determinant > 0.0 {
            let slope = (count * carrier_phases - carrier_sum * phase_sum) / determinant;
            self.slope += slope;
            self.offset += (phase_sum - slope * carrier_sum) / count;
        } else if count > 0.0 {
            self.offset += phase_sum / count;
        }
        let step = Complex32::from_polar(1.0, -self.slope);
        let mut rotation = Complex32::from_polar(1.0, -self.at(-28.0));
        for carrier in -28..=28 {
            tones[fft_bin(carrier)] *= rotation;
            rotation *= step;
        }
    }

    fn at(&self, carrier: f32) -> f32 {
        self.offset + self.slope * carrier
    }
}

pub(super) struct Ofdm {
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    training: [Complex32; FFT_LEN],
}

impl Ofdm {
    pub(super) fn new() -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        let mut training = [Complex32::default(); FFT_LEN];
        for (carrier, sign) in active_carriers(28).zip(TRAINING_SIGNS) {
            training[fft_bin(carrier)].re = f32::from(sign);
        }
        planner.plan_fft_inverse(FFT_LEN).process(&mut training);
        let norm = (FFT_LEN * TRAINING_SIGNS.len()) as f32;
        for sample in &mut training {
            *sample /= norm.sqrt();
        }
        Self {
            fft,
            scratch,
            training,
        }
    }

    /// Refine a coarse STF timing and frequency estimate using both LTFs.
    pub(super) fn lock(
        &mut self,
        iq: &[Complex32],
        coarse_start: usize,
        coarse_cfo_hz: f32,
    ) -> Option<Preamble> {
        if !coarse_cfo_hz.is_finite() {
            return None;
        }
        let estimated = coarse_start.checked_add(LTF_START)?;
        let search_start = estimated.saturating_sub(LTF_SEARCH_RADIUS).max(LTF_START);
        let search_end = estimated
            .saturating_add(LTF_SEARCH_RADIUS)
            .min(iq.len().checked_sub(2 * FFT_LEN)?);
        if search_start > search_end {
            return None;
        }

        let mut reference = self.training;
        rotate_frequency(&mut reference, coarse_cfo_hz, 0);
        let mut best = None;
        let mut best_score = 0.0;
        for first in search_start..=search_end {
            let ltf = &iq[first..first + 2 * FFT_LEN];
            let (early, late) = ltf.split_at(FFT_LEN);
            let score = correlation(early, &reference) * correlation(late, &reference);
            if score > best_score {
                best = Some(first);
                best_score = score;
            }
        }
        // Both long training repetitions must correlate with the waveform.
        let first = best.filter(|_| best_score >= 0.10)?;
        let second = first + FFT_LEN;
        let reference_cfo = iq[first..second]
            .iter()
            .zip(&iq[second..second + FFT_LEN])
            .map(|(early, late)| early.conj() * late)
            .sum::<Complex32>()
            .arg()
            * RATE
            / (TAU * FFT_LEN as f32);
        let cfo_period = RATE / FFT_LEN as f32;
        let cfo_hz =
            reference_cfo + ((coarse_cfo_hz - reference_cfo) / cfo_period).round() * cfo_period;
        let start = first - LTF_START;
        let channel = self.channel(iq, start, first, second, cfo_hz)?;
        Some(Preamble {
            start,
            cfo_hz,
            channel,
        })
    }

    /// Recover both signaling symbols and identify the second symbol's axis.
    pub(super) fn sig(&mut self, iq: &[Complex32], preamble: &Preamble) -> Option<SigTones> {
        let mut spectra = [[Complex32::default(); FFT_LEN]; 2];
        let mut phase = PilotPhase::default();
        for (index, tones) in spectra.iter_mut().enumerate() {
            let useful = preamble
                .start
                .checked_add(SIG_START + index * SYMBOL_LEN + LONG_GI)?;
            *tones = self.spectrum(iq, preamble.start, useful, LONG_GI, preamble.cfo_hz)?;
            preamble.channel.equalize(tones);
            phase.correct(tones, FIXED_PILOTS, PILOT_SIGNS);
        }

        let (real, imaginary) = active_carriers(26)
            .filter(|carrier| !FIXED_PILOTS.contains(carrier))
            .map(|carrier| spectra[1][fft_bin(carrier)])
            .fold((0.0, 0.0), |(real, imaginary), tone| {
                (real + tone.re * tone.re, imaginary + tone.im * tone.im)
            });
        let format = if real > imaginary {
            Format::Long
        } else {
            Format::Short
        };
        let mut soft = [[0.0; 48]; 2];
        for (index, carrier) in active_carriers(26)
            .filter(|carrier| !FIXED_PILOTS.contains(carrier))
            .enumerate()
        {
            let bin = fft_bin(carrier);
            soft[0][index] = spectra[0][bin].im;
            soft[1][index] = match format {
                Format::Short => spectra[1][bin].im,
                Format::Long => spectra[1][bin].re,
            };
        }
        Some(SigTones { format, soft })
    }

    /// Calculate the exclusive end without requiring the remaining samples.
    pub(super) fn data_end(preamble: &Preamble, parameters: DataParameters) -> Option<usize> {
        if !(1..=MAX_DATA_SYMBOLS).contains(&parameters.symbols) {
            return None;
        }
        let remaining_gi = if parameters.short_gi {
            SHORT_GI
        } else {
            LONG_GI
        };
        let length = SYMBOL_LEN + (parameters.symbols - 1) * (FFT_LEN + remaining_gi);
        preamble
            .start
            .checked_add(parameters.format.data_start() + length)
    }

    /// Recover every announced data symbol once the full sample interval is available.
    pub(super) fn data(
        &mut self,
        iq: &[Complex32],
        preamble: &Preamble,
        parameters: DataParameters,
    ) -> Option<DataTones> {
        let end = Self::data_end(preamble, parameters)?;
        if end > iq.len() {
            return None;
        }
        let channel = match parameters.format {
            Format::Short => &preamble.channel,
            Format::Long => &self.channel(
                iq,
                preamble.start,
                preamble.start + SHORT_DATA_START + SYMBOL_LEN + LONG_GI,
                preamble.start + SHORT_DATA_START + 2 * SYMBOL_LEN + LONG_GI,
                preamble.cfo_hz,
            )?,
        };
        let mut phase = PilotPhase::default();
        let mut tones = Vec::with_capacity(parameters.symbols);
        let mut position = preamble.start + parameters.format.data_start();
        for index in 0..parameters.symbols {
            let guard = if index == 0 || !parameters.short_gi {
                LONG_GI
            } else {
                SHORT_GI
            };
            let mut spectrum =
                self.spectrum(iq, preamble.start, position + guard, guard, preamble.cfo_hz)?;
            channel.equalize(&mut spectrum);
            let pilots = if parameters.traveling_pilots {
                TRAVELING_PILOTS[index % TRAVELING_PILOTS.len()]
            } else {
                FIXED_PILOTS
            };
            let polarity = PILOT_POLARITY[(index + 2) % PILOT_POLARITY.len()];
            let signs = std::array::from_fn(|pilot| PILOT_SIGNS[(pilot + index) % 4] * polarity);
            phase.correct(&mut spectrum, pilots, signs);
            let mut symbol = [Complex32::default(); 52];
            for (tone, carrier) in symbol
                .iter_mut()
                .zip(active_carriers(28).filter(|carrier| !pilots.contains(carrier)))
            {
                *tone = spectrum[fft_bin(carrier)];
            }
            tones.push(symbol);
            position += guard + FFT_LEN;
        }
        Some(DataTones { tones, end })
    }

    fn channel(
        &mut self,
        iq: &[Complex32],
        start: usize,
        first: usize,
        second: usize,
        cfo_hz: f32,
    ) -> Option<Channel> {
        let first = self.spectrum(iq, start, first, LONG_GI, cfo_hz)?;
        let second = self.spectrum(iq, start, second, LONG_GI, cfo_hz)?;
        Channel::estimate(first, second)
    }

    fn spectrum(
        &mut self,
        iq: &[Complex32],
        start: usize,
        useful: usize,
        guard: usize,
        cfo_hz: f32,
    ) -> Option<[Complex32; FFT_LEN]> {
        // Sampling halfway into the guard leaves room for timing error and
        // multipath. Undo the cyclic shift before transforming the symbol.
        let lead = guard / 2;
        let first = useful.checked_sub(lead)?;
        let mut symbol: [Complex32; FFT_LEN] = iq
            .get(first..first.checked_add(FFT_LEN)?)?
            .try_into()
            .ok()?;
        rotate_frequency(&mut symbol, -cfo_hz, first.checked_sub(start)?);
        symbol.rotate_left(lead);
        self.fft
            .process_with_scratch(&mut symbol, &mut self.scratch);
        Some(symbol)
    }
}

fn active_carriers(outer: i32) -> impl Iterator<Item = i32> {
    (-outer..0).chain(1..=outer)
}

fn fft_bin(carrier: i32) -> usize {
    carrier.rem_euclid(FFT_LEN as i32) as usize
}

fn rotate_frequency(samples: &mut [Complex32], frequency: f32, start: usize) {
    let rotation_per_sample = f64::from(frequency) / f64::from(RATE);
    let step = Complex32::from_polar(1.0, TAU * rotation_per_sample as f32);
    let mut rotation = Complex32::from_polar(
        1.0,
        TAU * (rotation_per_sample * start as f64).fract() as f32,
    );
    for sample in samples {
        *sample *= rotation;
        rotation *= step;
    }
}

fn correlation(samples: &[Complex32], reference: &[Complex32; FFT_LEN]) -> f32 {
    let (product, power) = samples.iter().zip(reference).fold(
        (Complex32::default(), 0.0),
        |(product, power), (sample, reference)| {
            (
                product + sample * reference.conj(),
                power + sample.norm_sqr(),
            )
        },
    );
    product.norm_sqr() / power.max(f32::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::{
        Complex32, DataParameters, FftPlanner, Format, LTF_START, Ofdm, active_carriers, fft_bin,
        rotate_frequency,
    };

    #[test]
    fn training_lock_resolves_frequency_alias_and_coarse_timing() {
        let mut ofdm = Ofdm::new();
        let start = 76;
        let mut iq = vec![Complex32::default(); start + LTF_START - 32];
        iq.extend(&ofdm.training[32..]);
        iq.extend(ofdm.training);
        iq.extend(ofdm.training);
        rotate_frequency(&mut iq, -44_000.0, 0);

        let preamble = ofdm.lock(&iq, start + 25, -42_500.0).unwrap();

        assert_eq!(preamble.start, start);
        assert!((preamble.cfo_hz + 44_000.0).abs() < 1.0);
    }

    #[test]
    fn data_tracks_pilots_across_a_short_guard_interval() {
        let mut ofdm = Ofdm::new();
        let mut iq = vec![Complex32::default(); LTF_START - 32];
        iq.extend(&ofdm.training[32..]);
        iq.extend(ofdm.training);
        iq.extend(ofdm.training);
        iq.resize(480, Complex32::default());

        let expected = Complex32::new(-0.6, 0.8);
        let inverse = FftPlanner::new().plan_fft_inverse(64);
        for (guard, offset, slope, pilots) in [
            (16, 0.4, 0.027, [1.0, 1.0, 1.0, -1.0]),
            (8, 0.6, 0.017, [1.0, 1.0, -1.0, 1.0]),
        ] {
            let mut bins = [Complex32::default(); 64];
            for carrier in active_carriers(28) {
                let value = match [-21, -7, 7, 21].iter().position(|&pilot| pilot == carrier) {
                    Some(pilot) => Complex32::new(pilots[pilot], 0.0),
                    None => expected,
                };
                bins[fft_bin(carrier)] =
                    value * Complex32::from_polar(1.0, offset + slope * carrier as f32);
            }
            inverse.process(&mut bins);
            let samples = bins.map(|sample| sample / (64.0_f32 * 56.0).sqrt());
            iq.extend(&samples[samples.len() - guard..]);
            iq.extend(samples);
        }

        let preamble = ofdm.lock(&iq, 0, 0.0).unwrap();
        let parameters = DataParameters {
            format: Format::Short,
            symbols: 2,
            short_gi: true,
            traveling_pilots: false,
        };
        assert_eq!(Ofdm::data_end(&preamble, parameters), Some(632));
        assert!(ofdm.data(&iq[..631], &preamble, parameters).is_none());

        let data = ofdm.data(&iq, &preamble, parameters).unwrap();
        assert_eq!(data.end, 632);
        assert_eq!(data.tones.len(), 2);
        assert!(
            data.tones
                .iter()
                .flatten()
                .all(|tone| (tone - expected).norm() < 1e-4)
        );
    }
}
