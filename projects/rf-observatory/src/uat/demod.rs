//! Streaming UAT downlink reception at four million complex samples per second.
//!
//! ICAO Annex 10 Volume III Chapter 12 specifies the continuous-phase binary
//! modulation, symbol rate, and distinct 36-bit airborne and ground sync words.
//! The [pinned dump978 constants] provide a second reference for bit order.
//!
//! [pinned dump978 constants]: https://github.com/flightaware/dump978/blob/f40aa9a0d2d0a0067c0c628b44ba316f7049b8ce/uat_protocol.h

use std::f64::consts::TAU;

use crate::dsp::{Iq, phase_step};

use super::frame::{BASIC_BYTES, LONG_BYTES};

/// Fixed complex sample rate after channelization.
pub(super) const SAMPLE_RATE_HZ: u32 = 4_000_000;
const SYNC_BITS: u64 = 36;
const DOWNLINK_SYNC: u64 = 0x000e_acdd_a4e2;
const SYNC_MASK: u64 = (1 << SYNC_BITS) - 1;
const MAX_SYNC_ERRORS: u32 = 4;
const BASIC_BITS: u64 = (BASIC_BYTES * 8) as u64;
const LONG_BITS: u64 = (LONG_BYTES * 8) as u64;

// At 1,041,666⅔ symbols/s, each symbol occupies exactly 96/25 samples.
const SAMPLE_TICKS: u64 = 25;
const SYMBOL_TICKS: u64 = 96;
const PHASES: usize = 4;
const PHASE_TICKS: u64 = SYMBOL_TICKS / PHASES as u64;
const SYNC_LAST_CENTER_TICKS: u64 = (SYNC_BITS - 1) * SYMBOL_TICKS + SYMBOL_TICKS / 2;
const HISTORY: usize = 512;
const _: () = assert!(
    HISTORY.is_power_of_two() && HISTORY as u64 * SYMBOL_TICKS >= MAX_FRAME_SAMPLES * SAMPLE_TICKS
);

/// Rounded-up sample extent of a nominal basic message including sync.
pub(super) const MIN_FRAME_SAMPLES: u64 =
    ((SYNC_BITS + BASIC_BITS) * SYMBOL_TICKS).div_ceil(SAMPLE_TICKS);
/// Rounded-up sample extent of a nominal long message including sync.
pub(super) const MAX_FRAME_SAMPLES: u64 =
    ((SYNC_BITS + LONG_BITS) * SYMBOL_TICKS).div_ceil(SAMPLE_TICKS);

// Leave room for filtered transitions and receiver tuning error in the sync
// gate. The ideal deviation is ±312.5 kHz. Reed–Solomon validation is separate.
const MIN_SYNC_TONE_GAP: f64 = 180_000.0 * TAU / SAMPLE_RATE_HZ as f64;
const MAX_SYNC_CENTER_OFFSET: f64 = 200_000.0 * TAU / SAMPLE_RATE_HZ as f64;

/// A demodulated codeword awaiting forward error correction.
pub(super) struct Candidate {
    /// Nominal sync start, rounded to the channel sample grid.
    pub(super) first_sample: u64,
    /// Nominal end on the channel sample grid, exclusive.
    pub(super) end_sample: u64,
    /// Nominal end if only the basic codeword validates, exclusive.
    pub(super) basic_end_sample: u64,
    pub(super) codeword: [u8; LONG_BYTES],
    pub(super) length: usize,
}

/// Check interleaved symbol timing phases and retain bounded soft evidence.
pub(super) struct Demodulator {
    phases: [TimingPhase; PHASES],
    next_phase: usize,
    next_center_ticks: u64,
    previous: Option<Iq>,
    previous_step: f64,
    older_step: f64,
}

impl Demodulator {
    pub(super) fn new() -> Self {
        Self {
            phases: std::array::from_fn(|_| TimingPhase::new()),
            // The first center with an available central phase window.
            next_phase: 1,
            next_center_ticks: PHASE_TICKS,
            previous: None,
            previous_step: 0.0,
            older_step: 0.0,
        }
    }

    /// Consume contiguous channelized samples starting at index zero.
    ///
    /// A single sample can complete several alignments. Each provides the long
    /// window first, including the basic prefix for the shorter format.
    pub(super) fn push(&mut self, sample: Iq, index: u64, mut emit: impl FnMut(Candidate)) {
        let Some(previous) = self.previous.replace(sample) else {
            return;
        };
        let step = phase_step(previous, sample);
        if index >= 2 {
            // Average phase over two sample intervals centered on the symbol.
            // For a fractional center, share the two edge differences while
            // giving full weight to the middle one. Their sum cancels the
            // common phase errors of adjacent samples.
            let latest_center = (index - 1) * SAMPLE_TICKS;
            while self.next_center_ticks < latest_center {
                let from_latest =
                    (latest_center - self.next_center_ticks) as f64 / SAMPLE_TICKS as f64;
                let observed =
                    (self.previous_step + step + from_latest * (self.older_step - step)) * 0.5;
                self.phases[self.next_phase].push(observed, self.next_center_ticks, &mut emit);
                self.next_phase = (self.next_phase + 1) % PHASES;
                self.next_center_ticks += PHASE_TICKS;
            }
        }
        self.older_step = self.previous_step;
        self.previous_step = step;
    }

    /// Emit complete basic prefixes whose long windows extend past the input.
    pub(super) fn finish(&self, mut emit: impl FnMut(Candidate)) {
        let lookback_ticks = (LONG_BITS - BASIC_BITS) * SYMBOL_TICKS;
        let earliest = self.next_center_ticks.saturating_sub(lookback_ticks);
        for center in (earliest..self.next_center_ticks).step_by(PHASE_TICKS as usize) {
            let phase = (center / PHASE_TICKS) as usize % PHASES;
            if let Some(candidate) = self.phases[phase].basic_candidate(center) {
                emit(candidate);
            }
        }
    }
}

struct TimingPhase {
    observations: [f64; HISTORY],
    sync_centers: [Option<f64>; HISTORY],
    history_bits: u64,
    next_symbol: u64,
    last_center_ticks: u64,
}

impl TimingPhase {
    fn new() -> Self {
        Self {
            observations: [0.0; HISTORY],
            sync_centers: [None; HISTORY],
            history_bits: 0,
            next_symbol: 0,
            last_center_ticks: 0,
        }
    }

    fn push(&mut self, phase_step: f64, center_ticks: u64, emit: &mut impl FnMut(Candidate)) {
        let symbol = self.next_symbol;
        self.next_symbol += 1;
        self.last_center_ticks = center_ticks;
        let position = slot(symbol);
        self.observations[position] = phase_step;
        self.history_bits = (self.history_bits << 1) | u64::from(phase_step > 0.0);

        // A cheap sign match prevents a full sync fit at every candidate start.
        // The ground sync is the complement and does not qualify here.
        self.sync_centers[position] = if self.next_symbol >= SYNC_BITS
            && ((self.history_bits ^ DOWNLINK_SYNC) & SYNC_MASK).count_ones() <= MAX_SYNC_ERRORS
        {
            self.fit_sync(symbol + 1 - SYNC_BITS)
        } else {
            None
        };

        if let Some(sync_end) = symbol.checked_sub(LONG_BITS)
            && let Some(threshold) = self.sync_centers[slot(sync_end)]
        {
            emit(self.candidate(sync_end, LONG_BYTES, threshold));
        }
    }

    fn basic_candidate(&self, center_ticks: u64) -> Option<Candidate> {
        let back = self.last_center_ticks.checked_sub(center_ticks)? / SYMBOL_TICKS;
        let sync_end = self.next_symbol.checked_sub(back + 1 + BASIC_BITS)?;
        let threshold = self.sync_centers[slot(sync_end)]?;
        Some(self.candidate(sync_end, BASIC_BYTES, threshold))
    }

    fn candidate(&self, sync_end: u64, length: usize, threshold: f64) -> Candidate {
        let symbols_after_sync = self.next_symbol - 1 - sync_end;
        let sync_center_ticks = self.last_center_ticks - symbols_after_sync * SYMBOL_TICKS;
        Candidate {
            first_sample: ticks_to_samples(
                sync_center_ticks.saturating_sub(SYNC_LAST_CENTER_TICKS),
            ),
            end_sample: ticks_to_samples(
                sync_center_ticks + (length * 8) as u64 * SYMBOL_TICKS + SYMBOL_TICKS / 2,
            ),
            basic_end_sample: ticks_to_samples(
                sync_center_ticks + BASIC_BITS * SYMBOL_TICKS + SYMBOL_TICKS / 2,
            ),
            codeword: self.codeword(sync_end + 1, length, threshold),
            length,
        }
    }

    fn fit_sync(&self, first: u64) -> Option<f64> {
        let mut sums = [0.0; 2];
        for offset in 0..SYNC_BITS {
            let bit = sync_bit(offset);
            sums[usize::from(bit)] += self.observations[slot(first + offset)];
        }
        let ones = f64::from(DOWNLINK_SYNC.count_ones());
        let zero = sums[0] / (SYNC_BITS as f64 - ones);
        let one = sums[1] / ones;
        let center = (zero + one) * 0.5;
        if one - zero < MIN_SYNC_TONE_GAP || center.abs() > MAX_SYNC_CENTER_OFFSET {
            return None;
        }
        let errors = (0..SYNC_BITS)
            .filter(|&offset| {
                (self.observations[slot(first + offset)] > center) != sync_bit(offset)
            })
            .count();
        (errors <= MAX_SYNC_ERRORS as usize).then_some(center)
    }

    fn codeword(&self, mut symbol: u64, length: usize, center: f64) -> [u8; LONG_BYTES] {
        let mut word = [0; LONG_BYTES];
        for byte in &mut word[..length] {
            for _ in 0..8 {
                *byte = (*byte << 1) | u8::from(self.observations[slot(symbol)] > center);
                symbol += 1;
            }
        }
        word
    }
}

fn ticks_to_samples(ticks: u64) -> u64 {
    (ticks + SAMPLE_TICKS / 2) / SAMPLE_TICKS
}

fn slot(symbol: u64) -> usize {
    symbol as usize & (HISTORY - 1)
}

fn sync_bit(offset: u64) -> bool {
    DOWNLINK_SYNC & (1 << (SYNC_BITS - 1 - offset)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushes_an_airborne_basic_word_at_its_nominal_end() {
        let word: [u8; BASIC_BYTES] = std::array::from_fn(|i| (19 * i + 1) as u8);
        let bits = (0..36)
            .rev()
            .map(|i| 0xeacdda4e2_u64 & (1 << i) != 0)
            .chain(
                word.iter()
                    .flat_map(|&byte| (0..8).rev().map(move |i| byte & (1 << i) != 0)),
            );
        let levels: Vec<_> = bits.map(|bit| if bit { 1.0 } else { -1.0 }).collect();
        let symbol_rate_hz = 3_125_000.0 / 3.0;
        let samples_per_symbol = f64::from(SAMPLE_RATE_HZ) / symbol_rate_hz;
        let start = 23.18;
        let end = (start + levels.len() as f64 * samples_per_symbol).ceil() as u64;
        let mut detector = Demodulator::new();
        for index in 0..end {
            let sample = if (index as f64) < start {
                Iq { i: 0.0, q: 0.0 }
            } else {
                let elapsed = (index as f64 - start) / samples_per_symbol;
                let bit = elapsed as usize;
                let integral = levels[..bit].iter().sum::<f64>() + elapsed.fract() * levels[bit];
                let cycles = (312_500.0 * integral + 91_000.0 * elapsed) / symbol_rate_hz;
                let (q, i) = (TAU * cycles).sin_cos();
                Iq { i, q }
            };
            detector.push(sample, index, |_| panic!("no long window has completed"));
        }

        let mut recovered = false;
        detector.finish(|candidate| {
            assert_eq!(candidate.length, BASIC_BYTES);
            assert_eq!(candidate.basic_end_sample, candidate.end_sample);
            assert!(candidate.end_sample <= end);
            if candidate.codeword[..BASIC_BYTES] == word {
                assert!(candidate.first_sample.abs_diff(start.round() as u64) <= 2);
                assert!(candidate.end_sample.abs_diff(end) <= 2);
                recovered = true;
            }
        });
        assert!(recovered);
    }
}
