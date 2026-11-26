//! Streaming Mode S preamble and pulse-position detection at 4 MSps.
//!
//! Pulse timing and bit order follow [ICAO Annex 10, Volume IV,
//! §§3.1.2.2.5 and 3.1.2.3.1.2](https://www.bazl.admin.ch/dam/en/sd-web/SLpAhvMKSY6k/icao_annex_10_aeronauticaltelecommunicationsvolumeiv-surveillanc.pdf).

use crate::dsp::AdsbChannel;

use super::frame::Frame;

const SAMPLE_RATE_HZ: u64 = AdsbChannel::OUTPUT_RATE_HZ as u64;
/// Samples per one-microsecond bit at the channelized rate.
pub(super) const SAMPLES_PER_BIT: u64 = SAMPLE_RATE_HZ / 1_000_000;
const SAMPLES_PER_PULSE: u64 = SAMPLES_PER_BIT / 2;
/// Samples from the preamble start to the first data bit.
pub(super) const PREAMBLE_SAMPLES: u64 = 8 * SAMPLES_PER_BIT;
const FRAME_BYTES: usize = 14;
const FORMAT_BITS: u64 = 5;
const FORMAT_READY_SAMPLES: u64 = PREAMBLE_SAMPLES + FORMAT_BITS * SAMPLES_PER_BIT;
/// Samples occupied by the complete nominal extended squitter.
pub(super) const FRAME_SAMPLES: u64 = PREAMBLE_SAMPLES + (FRAME_BYTES as u64 * 8) * SAMPLES_PER_BIT;
const HISTORY_SAMPLES: usize = 512;
const _: () = assert!(SAMPLE_RATE_HZ == 4_000_000);
const _: () = assert!(HISTORY_SAMPLES.is_power_of_two() && HISTORY_SAMPLES as u64 >= FRAME_SAMPLES);

/// One CRC-valid sample alignment for an extended squitter.
pub(super) struct Detection {
    /// Zero-based start in channel samples.
    pub(super) start_sample: u64,
    pub(super) frame: Frame,
}

/// Check every sample phase, with fixed memory and no holdoff on candidates.
pub(super) struct Detector {
    history: [f64; HISTORY_SAMPLES],
    due: [bool; HISTORY_SAMPLES],
    samples_seen: u64,
}

impl Default for Detector {
    fn default() -> Self {
        Self {
            history: [0.0; HISTORY_SAMPLES],
            due: [false; HISTORY_SAMPLES],
            samples_seen: 0,
        }
    }
}

fn slot(sample: u64) -> usize {
    sample as usize & (HISTORY_SAMPLES - 1)
}

impl Detector {
    /// Accept one nonnegative power sample at 4 MSps.
    ///
    /// A result, if any, follows the last of the 480 samples in its frame.
    /// Neighboring CRC-valid alignments are available for caller-side RF
    /// checks and deduplication. Repeated frames at distinct times remain eligible.
    pub(super) fn push(&mut self, power: f64) -> Option<Detection> {
        let index = self.samples_seen;
        let position = slot(index);
        self.history[position] = power;
        self.samples_seen += 1;
        let ready = std::mem::take(&mut self.due[position]);

        if self.samples_seen >= FORMAT_READY_SAMPLES {
            let start = self.samples_seen - FORMAT_READY_SAMPLES;
            if self.has_preamble(start) && matches!(self.downlink_format(start), 17 | 18) {
                self.due[slot(start + FRAME_SAMPLES - 1)] = true;
            }
        }

        if !ready {
            return None;
        }
        let start_sample = self.samples_seen - FRAME_SAMPLES;
        let frame = Frame::parse(self.message(start_sample))?;
        Some(Detection {
            start_sample,
            frame,
        })
    }

    fn pulse_energy(&self, first: u64) -> f64 {
        self.history[slot(first)] + self.history[slot(first + 1)]
    }

    fn has_preamble(&self, start: u64) -> bool {
        // Each pulse pair should rise above the intervening half-bit. The two
        // samples per half-bit and adjacent timing hypotheses tolerate phase error.
        let early_gap = self.pulse_energy(start + 2);
        let first = self.pulse_energy(start);
        let second = self.pulse_energy(start + 4);
        if first <= early_gap || second <= early_gap {
            return false;
        }
        let late_gap = self.pulse_energy(start + 16);
        let third = self.pulse_energy(start + 14);
        let fourth = self.pulse_energy(start + 18);
        if third <= late_gap || fourth <= late_gap {
            return false;
        }
        let minimum = first.min(second).min(third).min(fourth);
        let maximum = first.max(second).max(third).max(fourth);
        if minimum < 0.20 * maximum {
            return false;
        }

        let quiet = [6, 8, 10, 12, 20, 22, 26, 30]
            .into_iter()
            .map(|offset| self.pulse_energy(start + offset))
            .sum::<f64>()
            + early_gap
            + late_gap;
        // Compare the mean of four pulse energies with ten background slots.
        // Use a permissive 1.6:1 ratio and leave admission to the complete CRC.
        first + second + third + fourth > (1.6 * 4.0 / 10.0) * quiet
    }

    fn bit(&self, sample: u64) -> u8 {
        let early = self.pulse_energy(sample);
        let late = self.pulse_energy(sample + SAMPLES_PER_PULSE);
        u8::from(early > late)
    }

    fn downlink_format(&self, start: u64) -> u8 {
        let first = start + PREAMBLE_SAMPLES;
        (0..FORMAT_BITS).fold(0, |value, offset| {
            (value << 1) | self.bit(first + offset * SAMPLES_PER_BIT)
        })
    }

    fn message(&self, start: u64) -> [u8; FRAME_BYTES] {
        let mut sample = start + PREAMBLE_SAMPLES;
        std::array::from_fn(|_| {
            let mut value = 0;
            for _ in 0..8 {
                value = (value << 1) | self.bit(sample);
                sample += SAMPLES_PER_BIT;
            }
            value
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Published in The 1090 Megahertz Riddle, chapter 2, "Raw Mode S messages".
    const DF17: [u8; 14] = [
        0x8d, 0x40, 0x74, 0x35, 0x99, 0x0d, 0xbd, 0x01, 0x90, 0x04, 0x84, 0xf6, 0x6c, 0x3c,
    ];
    // Synthetic DF18 bits with parity computed by separate polynomial division.
    const DF18: [u8; 14] = [
        0x90, 0x12, 0x34, 0x56, 0x20, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0xcc, 0xc0, 0xfd,
    ];

    fn pulse(power: &mut [f64], start: f64, strength: f64) {
        let end = start + 2.0;
        let first = start.floor() as usize;
        for (offset, power) in power[first..end.ceil() as usize].iter_mut().enumerate() {
            let sample = (first + offset) as f64;
            let overlap = (sample + 1.0).min(end) - sample.max(start);
            *power += strength * overlap;
        }
    }

    fn insert(power: &mut [f64], start: f64, frame: [u8; 14], bits: usize) {
        // Integrate continuous-time pulses over receiver bins, including a
        // fractional start, varying pulse amplitude, and bounded data jitter.
        for (position, strength) in [(0.0, 9.0), (4.0, 8.0), (14.0, 10.0), (18.0, 9.0)] {
            pulse(power, start + position, strength);
        }
        for bit in 0..bits {
            let late = frame[bit / 8] & (0x80 >> (bit % 8)) == 0;
            let offset = 32 + 4 * bit + 2 * usize::from(late);
            let jitter = ((bit * 7 % 5) as f64 - 2.0) * 0.052;
            let strength = 8.0 + (bit % 4) as f64 * 0.7;
            pulse(power, start + offset as f64 + jitter, strength);
        }
    }

    #[test]
    fn keeps_fractional_alignments_and_repeats_despite_a_pending_false_preamble() {
        let mut state = 0x93ea_f4c9_u32;
        let mut power: Vec<_> = (0..2_600)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.4 + f64::from(state >> 24) / 128.0
            })
            .collect();
        // The incomplete candidate would still be pending when the first real
        // preamble arrives. Both repeated DF17 packets must be observable.
        insert(&mut power, 40.49, DF17, 5);
        insert(&mut power, 171.42, DF17, 112);
        let mut corrupt = DF17;
        corrupt[7] ^= 0x02;
        insert(&mut power, 749.33, corrupt, 112);
        insert(&mut power, 1_388.58, DF17, 112);
        insert(&mut power, 2_010.49, DF18, 112);

        let mut detector = Detector::default();
        let found: Vec<_> = power
            .into_iter()
            .enumerate()
            .filter_map(|(index, power)| {
                detector.push(power).map(|detection| {
                    assert_eq!(index as u64 - detection.start_sample, 479);
                    (detection.start_sample, *detection.frame.bytes())
                })
            })
            .collect();
        let expected = [(171, DF17), (1_389, DF17), (2_010, DF18)];
        let same_signal =
            |&(left_start, left_bytes): &(u64, [u8; 14]),
             &(right_start, right_bytes): &(u64, [u8; 14])| {
                left_start.abs_diff(right_start) <= 1 && left_bytes == right_bytes
            };
        assert!(
            expected
                .iter()
                .all(|wanted| found.iter().any(|observed| same_signal(wanted, observed))),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|observed| expected.iter().any(|wanted| same_signal(wanted, observed))),
            "{found:?}"
        );
    }

    #[test]
    fn waits_for_the_complete_codeword() {
        let mut power = [0.0; 481];
        insert(&mut power, 0.0, DF17, 112);
        let mut detector = Detector::default();
        assert!(
            power[..479]
                .iter()
                .all(|&sample| detector.push(sample).is_none())
        );
        assert_eq!(detector.push(power[479]).unwrap().start_sample, 0);
    }
}
