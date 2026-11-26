//! Recover AIS GMSK bursts from a single channel's complex samples.
//!
//! [Recommendation ITU-R M.1371-6] Annex 2 §§ A2-2.3–2.5 and A2-3.2.2
//! specifies 9,600 bit/s GMSK, NRZI, training, flags, stuffing and FCS.
//!
//! [Recommendation ITU-R M.1371-6]: https://www.itu.int/epublications/fr/publication/recommendation-itu-r-m-1371-6-02-2026-technical-characteristics-for-vhf-automatic-identification-system-using-time-division-multiple-access-in-the-maritime-mobile-se/en

mod coherent;

use std::f64::consts::TAU;

use crate::dsp::{Iq, hdlc, phase_step};

const SYMBOL_RATE: u32 = 9_600;
const FLAG: u8 = 0x7e;
const FLAG_BITS: usize = u8::BITS as usize;
// The suffix admits a burst whose power or frequency settled late in training.
const TRAINING_BITS: usize = 16;
const SYNC_BITS: usize = TRAINING_BITS + FLAG_BITS;
const MAX_TIMING_PHASES: usize = 12;
const MIN_FRAME_BYTES: usize = 7;
// Annex 7 Table 81 bounds the data field at 1,064 bits before the 16-bit FCS.
const MAX_FRAME_BYTES: usize = 135;

struct WireFrame {
    bytes: [u8; MAX_FRAME_BYTES],
    length: usize,
    partial: u8,
    bits: u8,
    ones: u8,
    recent: u8,
    recent_bits: usize,
}

impl Default for WireFrame {
    fn default() -> Self {
        Self {
            bytes: [0; MAX_FRAME_BYTES],
            length: 0,
            partial: 0,
            bits: 0,
            ones: 0,
            recent: 0,
            recent_bits: 0,
        }
    }
}

enum Status {
    Pending,
    Closed,
    Rejected,
}

impl WireFrame {
    fn push_payload_bit(&mut self, bit: bool) -> bool {
        if self.ones == 5 {
            self.ones = 0;
            return !bit;
        }
        if bit {
            self.ones += 1;
            self.partial |= 1 << self.bits;
        } else {
            self.ones = 0;
        }
        self.bits += 1;
        if self.bits == u8::BITS as u8 {
            let Some(byte) = self.bytes.get_mut(self.length) else {
                return false;
            };
            *byte = self.partial;
            self.length += 1;
            self.partial = 0;
            self.bits = 0;
        }
        true
    }

    fn push(&mut self, bit: bool) -> Status {
        // Until an octet leaves the flag window, none of its bits is known
        // to be payload. The closing flag is never passed to the un-stuffer.
        if self.recent_bits == FLAG_BITS {
            if !self.push_payload_bit(self.recent & 0x80 != 0) {
                return Status::Rejected;
            }
        } else {
            self.recent_bits += 1;
        }
        self.recent = (self.recent << 1) | u8::from(bit);
        if self.recent_bits == FLAG_BITS && self.recent == FLAG {
            Status::Closed
        } else {
            Status::Pending
        }
    }

    fn validated_bytes(&self) -> Option<&[u8]> {
        if self.bits != 0 || self.ones == 5 || self.length < MIN_FRAME_BYTES {
            return None;
        }
        let bytes = &self.bytes[..self.length];
        let (payload, &wire_fcs) = bytes.split_last_chunk::<2>()?;
        (hdlc::fcs(payload) == u16::from_le_bytes(wire_fcs)).then_some(bytes)
    }
}

struct TimingPhase {
    threshold: f64,
    previous_high: bool,
    phase: u32,
    first_sample: u64,
    frame: WireFrame,
}

impl TimingPhase {
    fn push(&mut self, sample: f64, sample_rate_hz: u32) -> Status {
        self.phase += SYMBOL_RATE;
        if self.phase < sample_rate_hz {
            return Status::Pending;
        }
        self.phase -= sample_rate_hz;
        let high = sample > self.threshold;
        // A tone change represents zero in NRZI, independent of IQ polarity.
        let bit = std::mem::replace(&mut self.previous_high, high) == high;
        self.frame.push(bit)
    }
}

struct SeenFrame {
    bytes: [u8; MAX_FRAME_BYTES],
    length: usize,
    first_sample: u64,
}

/// One channel of AIS symbol recovery and bounded HDLC framing.
pub(super) struct Decoder {
    sample_rate_hz: u32,
    previous: Option<Iq>,
    average: Vec<f64>,
    average_head: usize,
    average_total: f64,
    history: Vec<f64>,
    history_head: usize,
    history_count: usize,
    sync_ages: [usize; SYNC_BITS],
    nominal_tone_phase: f64,
    phases: Vec<TimingPhase>,
    coherent: coherent::Decoder,
    seen: Option<SeenFrame>,
}

impl Decoder {
    /// Initialize at a channelizer's 50 or 96 kSps output rate.
    pub(super) fn new(sample_rate_hz: u32) -> Self {
        debug_assert!(matches!(sample_rate_hz, 50_000 | 96_000));
        let samples_for_symbols = |symbols: usize| {
            (symbols * sample_rate_hz as usize + SYMBOL_RATE as usize / 2) / SYMBOL_RATE as usize
        };
        let sync_ages = std::array::from_fn(|bit| samples_for_symbols(SYNC_BITS - bit - 1));
        // Integrate nearly one symbol to suppress phase noise. A smaller
        // window gives each transition little protection from the noise floor.
        let average_samples = (samples_for_symbols(9) + 5) / 10;
        Self {
            sample_rate_hz,
            previous: None,
            average: vec![0.0; average_samples],
            average_head: 0,
            average_total: 0.0,
            history: vec![0.0; sync_ages[0] + 1],
            history_head: 0,
            history_count: 0,
            sync_ages,
            nominal_tone_phase: TAU * f64::from(SYMBOL_RATE) / (4.0 * f64::from(sample_rate_hz)),
            phases: Vec::with_capacity(MAX_TIMING_PHASES),
            coherent: coherent::Decoder::new(sample_rate_hz),
            seen: None,
        }
    }

    /// Consume the next sample and emit complete FCS-checked frames.
    ///
    /// Callback extents bracket the observed opening and closing flag
    /// decisions on the channel sample grid, with discriminator delay
    /// compensated. The captured octets include the two FCS bytes.
    pub(super) fn push(&mut self, sample: Iq, index: u64, mut emit: impl FnMut(&[u8], u64, u64)) {
        let step = self
            .previous
            .replace(sample)
            .map_or(0.0, |previous| phase_step(previous, sample));
        self.average_total += step - self.average[self.average_head];
        self.average[self.average_head] = step;
        self.average_head = (self.average_head + 1) % self.average.len();
        let angle = self.average_total / self.average.len() as f64;
        self.history[self.history_head] = angle;
        self.history_head = (self.history_head + 1) % self.history.len();
        self.history_count = (self.history_count + 1).min(self.history.len());

        let delay = (self.average.len() / 2) as u64;
        let mut slot = 0;
        while let Some(phase) = self.phases.get_mut(slot) {
            match phase.push(angle, self.sample_rate_hz) {
                Status::Pending => slot += 1,
                Status::Rejected => {
                    self.phases.swap_remove(slot);
                }
                Status::Closed => {
                    let phase = self.phases.swap_remove(slot);
                    if let Some(bytes) = phase.frame.validated_bytes()
                        && self.is_new(bytes, phase.first_sample)
                    {
                        emit(bytes, phase.first_sample, (index + 1).saturating_sub(delay));
                    }
                }
            }
        }

        let opening = if self.phases.len() < MAX_TIMING_PHASES
            && self.history_count == self.history.len()
            && let Some(threshold) = self.opening_threshold()
        {
            self.phases.push(TimingPhase {
                threshold,
                previous_high: angle > threshold,
                phase: SYMBOL_RATE / 2,
                first_sample: index.saturating_sub(self.sync_ages[TRAINING_BITS] as u64 + delay),
                frame: WireFrame::default(),
            });
            true
        } else {
            false
        };
        if let Some(frame) = self.coherent.push(sample, index, opening)
            && self.is_new(frame.bytes(), frame.first_sample)
        {
            emit(
                frame.bytes(),
                frame.first_sample,
                frame.end_sample_exclusive,
            );
        }
    }

    fn stored_angle(&self, position: usize) -> f64 {
        let index = self.history_head + self.history.len() - self.sync_ages[position] - 1;
        self.history[index % self.history.len()]
    }

    fn opening_threshold(&self) -> Option<f64> {
        // After NRZI the alternating training bits visit two symbols per
        // tone. The flag then holds one tone for seven symbols before its
        // final transition. This cheap contrast test rejects most offsets.
        let flag_tone = self.stored_angle(TRAINING_BITS + 3);
        let last_tone = self.stored_angle(SYNC_BITS - 1);
        if (flag_tone - last_tone).abs() < self.nominal_tone_phase * 0.5 {
            return None;
        }

        let mut tone_totals = [0.0; 2];
        for symbol in 0..TRAINING_BITS {
            tone_totals[symbol / 2 % 2] += self.stored_angle(symbol);
        }
        let [first, second] = tone_totals.map(|sum| sum / (TRAINING_BITS / 2) as f64);
        let separation = (first - second).abs() / self.nominal_tone_phase;
        if !(0.55..=3.0).contains(&separation) {
            return None;
        }

        let threshold = (first + second) * 0.5;
        let first_high = first > second;
        let flag_matches = (TRAINING_BITS..SYNC_BITS).all(|symbol| {
            (self.stored_angle(symbol) > threshold) == (first_high == (symbol + 1 != SYNC_BITS))
        });
        if !flag_matches {
            return None;
        }
        let training_errors = (0..TRAINING_BITS)
            .filter(|&symbol| {
                (self.stored_angle(symbol) > threshold) != (first_high == (symbol / 2 % 2 == 0))
            })
            .count();
        (training_errors <= 4).then_some(threshold)
    }

    fn is_new(&mut self, bytes: &[u8], first_sample: u64) -> bool {
        // Sampling phases can recover the same packet a few samples apart.
        // A one-millisecond window is shorter than a complete AIS frame.
        if self.seen.as_ref().is_some_and(|seen| {
            seen.first_sample.abs_diff(first_sample) <= u64::from(self.sample_rate_hz / 1_000)
                && &seen.bytes[..seen.length] == bytes
        }) {
            return false;
        }
        let seen = self.seen.get_or_insert(SeenFrame {
            bytes: [0; MAX_FRAME_BYTES],
            length: 0,
            first_sample,
        });
        seen.length = bytes.len();
        seen.first_sample = first_sample;
        seen.bytes[..bytes.len()].copy_from_slice(bytes);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic type 4 body with an independent CRC-16/X.25 calculation.
    pub(super) const BURST: [u8; 23] = [
        0x12, 0x02, 0xf1, 0x85, 0x38, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x85, 0xd3,
    ];

    fn bits(byte: u8) -> impl Iterator<Item = bool> {
        (0..8).map(move |bit| byte & (1 << bit) != 0)
    }

    pub(super) fn tones(body: &[u8], closing_flag: bool) -> Vec<f64> {
        let mut bits_on_air = Vec::new();
        for _ in 0..12 {
            bits_on_air.extend([false, true]);
        }
        bits_on_air.extend(bits(FLAG));
        let mut ones = 0;
        for bit in body.iter().copied().flat_map(bits) {
            bits_on_air.push(bit);
            ones = if bit { ones + 1 } else { 0 };
            if ones == 5 {
                bits_on_air.push(false);
                ones = 0;
            }
        }
        if closing_flag {
            bits_on_air.extend(bits(FLAG));
        }
        let mut high = false;
        bits_on_air
            .into_iter()
            .map(|bit| {
                high ^= !bit;
                if high { 1.0 } else { -1.0 }
            })
            .collect()
    }

    pub(super) fn iq(
        tones: &[f64],
        sample_rate_hz: u32,
        offset_hz: f64,
        conjugate: bool,
    ) -> Vec<Iq> {
        let samples_per_bit = f64::from(sample_rate_hz) / f64::from(SYMBOL_RATE);
        // Gaussian frequency shaping with 3 dB bandwidth 0.4 times baud.
        let sigma = samples_per_bit * (2.0_f64.ln()).sqrt() / (TAU * 0.4);
        let span = (sigma * 4.0).ceil() as i32;
        let weights: Vec<_> = (-span..=span)
            .map(|lag| (-0.5 * (f64::from(lag) / sigma).powi(2)).exp())
            .collect();
        let gain = weights.iter().sum::<f64>();
        let count = (tones.len() as f64 * samples_per_bit + 1.0).ceil() as usize;
        let mut phase = 0.2;
        let mut noise = 0x1234_5678u32;
        let mut next_noise = || {
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(noise >> 16) / 65_535.0 - 0.5) * 0.25
        };
        (0..count)
            .map(|index| {
                let nominal = index as f64 - 0.37;
                let shaped = (-span..=span)
                    .zip(&weights)
                    .map(|(lag, weight)| {
                        let symbol = ((nominal - f64::from(lag)) / samples_per_bit) as usize;
                        tones[symbol.min(tones.len() - 1)] * weight
                    })
                    .sum::<f64>()
                    / gain;
                phase += TAU * (offset_hz + 2_400.0 * shaped) / f64::from(sample_rate_hz);
                let (q, i) = phase.sin_cos();
                Iq {
                    i: i + next_noise(),
                    q: (if conjugate { -q } else { q }) + next_noise(),
                }
            })
            .collect()
    }

    #[test]
    fn recovers_one_timing_copy_at_both_channel_rates_and_rejects_corruption() {
        let mut corrupt = BURST;
        corrupt[14] ^= 1;
        for sample_rate_hz in [50_000, 96_000] {
            let mut decoder = Decoder::new(sample_rate_hz);
            let mut index = 0;
            let mut observations = Vec::new();
            for (body, close, offset, conjugate) in [
                (&BURST, true, -3_100.0, false),
                (&corrupt, true, 1_500.0, false),
                (&BURST, true, 2_700.0, true),
                (&BURST, false, -1_100.0, false),
            ] {
                for sample in iq(&tones(body, close), sample_rate_hz, offset, conjugate) {
                    decoder.push(sample, index, |bytes, first, end| {
                        assert_eq!(bytes, BURST);
                        assert!(first < end && end <= index + 1);
                        observations.push((first, end));
                    });
                    index += 1;
                }
            }
            assert_eq!(observations.len(), 2, "{sample_rate_hz} Sps");
            assert!(observations[0].1 < observations[1].0);
        }
    }
}
