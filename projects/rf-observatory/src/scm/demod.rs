//! Recover Itron SCM and SCM+ Manchester pairs from channel power.
//!
//! The [reference receiver's sample grid] uses 32,768 chips per second.
//! Each data bit has two complementary chips, with a falling data edge
//! representing one. Both possible pair alignments are searched.
//!
//! [reference receiver's sample grid]: https://github.com/bemasher/rtlamr/wiki/Symbol-Length

use super::{Format, frame, legacy_frame};

const CHIP_RATE_HZ: f64 = 32_768.0;
const SETTLE_US: u64 = 100;
const STABLE_EDGE_US: u64 = 3;
const MAX_HIGH_US: u64 = 500;
const MAX_SYNC_BITS: usize = frame::SYNC.len() * 8;

/// Wire sync and frame lengths, in Manchester-decoded bits.
#[derive(Clone, Copy)]
struct Layout {
    sync_bits: usize,
    sync_word: u32,
    frame_bits: usize,
}

impl Layout {
    fn new(format: Format) -> Self {
        match format {
            Format::Legacy => Self {
                sync_bits: legacy_frame::SYNC_BITS,
                sync_word: legacy_frame::SYNC_WORD,
                frame_bits: legacy_frame::FRAME_BITS,
            },
            Format::Plus => {
                let [a, b, c] = frame::SYNC;
                Self {
                    sync_bits: MAX_SYNC_BITS,
                    sync_word: u32::from_be_bytes([0, a, b, c]),
                    frame_bits: frame::FRAME_BYTES * 8,
                }
            }
        }
    }
}

/// One sync-aligned packet candidate on the detector's input sample grid.
pub(super) struct Candidate {
    /// MSB-first frame octets, with four zero suffix bytes for legacy SCM.
    pub(super) bytes: [u8; frame::FRAME_BYTES],
    /// Estimated start of the first sync chip, in envelope samples.
    pub(super) first_sample: u64,
    /// Estimated boundary after the final Manchester chip, in envelope samples.
    pub(super) end_sample_exclusive: u64,
}

/// Bounded envelope slicer and parallel Manchester pair alignment search.
pub(super) struct Detector {
    layout: Layout,
    index: u64,
    samples_per_chip: f64,
    terminal_chip_samples: u64,
    settle_samples: u64,
    stable_edge_samples: u64,
    max_high_samples: u64,
    mean_step: f64,
    noise_step: f64,
    peak_decay: f64,
    mean: f64,
    noise: f64,
    peak: f64,
    high: bool,
    run_start: u64,
    edge: Option<u64>,
    previous_chip: Option<Chip>,
    phase: usize,
    rows: [Row; 2],
}

impl Detector {
    /// Start on a supported, nonzero channel envelope sample rate.
    pub(super) fn new(sample_rate_hz: u32, format: Format) -> Self {
        let samples = |us: u64| (u64::from(sample_rate_hz) * us).div_ceil(1_000_000);
        let step = |us: f64| (1_000_000.0 / (f64::from(sample_rate_hz) * us)).min(1.0);
        let samples_per_chip = f64::from(sample_rate_hz) / CHIP_RATE_HZ;
        Self {
            layout: Layout::new(format),
            index: 0,
            samples_per_chip,
            terminal_chip_samples: samples_per_chip.round() as u64,
            settle_samples: samples(SETTLE_US),
            stable_edge_samples: samples(STABLE_EDGE_US),
            max_high_samples: samples(MAX_HIGH_US),
            mean_step: step(2.0),
            noise_step: step(300.0),
            peak_decay: 1.0 - step(250.0),
            mean: 0.0,
            noise: 1.0,
            peak: 0.0,
            high: false,
            run_start: 0,
            edge: None,
            previous_chip: None,
            phase: 0,
            rows: Default::default(),
        }
    }

    /// Consume one consecutive power sample and emit any complete candidates.
    /// The caller validates packet integrity before admitting an identity.
    pub(super) fn push(&mut self, power: f64, mut emit: impl FnMut(Candidate)) {
        let now = self.index;
        self.index += 1;
        self.mean += (power - self.mean) * self.mean_step;
        if now < self.settle_samples {
            self.noise = self.mean.max(1.0);
            return;
        }

        self.peak = self.mean.max(self.peak * self.peak_decay);
        let (noise_factor, peak_fraction) = if self.high { (2.0, 0.25) } else { (4.0, 0.30) };
        let above = self.mean > (self.noise * noise_factor).max(self.peak * peak_fraction);
        if self.high == above {
            self.edge = None;
            if !self.high {
                self.noise += (self.mean.max(1.0) - self.noise) * self.noise_step;
            }
        } else {
            let edge = *self.edge.get_or_insert(now);
            if self.index - edge >= self.stable_edge_samples {
                self.close_run(edge, &mut emit);
                self.high = above;
                self.run_start = edge;
                self.edge = None;
            }
        }

        let observed_end = self.edge.unwrap_or(self.index);
        let terminal_end = self.run_start + self.terminal_chip_samples;
        if self.rows[self.phase].received == self.layout.frame_bits - 1
            && observed_end >= terminal_end
            && let Some(previous) = self.previous_chip
            && previous.high != self.high
            && let Some(candidate) =
                self.rows[self.phase].push_bit(self.layout, previous, terminal_end)
        {
            // The final chip can merge into silence or a longer carrier tail.
            // Admit it only after the nominal chip interval has been observed.
            emit(candidate);
        }

        if self.high && observed_end - self.run_start >= self.max_high_samples {
            // Rebase the envelope slicer after an unmodulated carrier.
            self.noise = self.mean.max(1.0);
            self.high = false;
            self.run_start = now;
            self.edge = None;
            self.reset_pairs();
        }
    }

    fn close_run(&mut self, end_sample: u64, emit: &mut impl FnMut(Candidate)) {
        let width = end_sample - self.run_start;
        let chips = width as f64 / self.samples_per_chip;
        let rounded = chips.round();
        // The transmitter shape and envelope thresholds can move opposing
        // edges in different directions while preserving the chip clock.
        if !(1.0..=2.0).contains(&rounded) || (chips - rounded).abs() > 0.40 {
            self.reset_pairs();
            return;
        }

        let chip_count = rounded as u64;
        for position in 0..chip_count {
            let chip = Chip {
                high: self.high,
                first_sample: self.run_start + position * width / chip_count,
            };
            let chip_end = self.run_start + (position + 1) * width / chip_count;
            if let Some(previous) = self.previous_chip.replace(chip) {
                let row = &mut self.rows[self.phase];
                self.phase ^= 1;
                if previous.high == chip.high {
                    row.reset();
                } else if let Some(candidate) = row.push_bit(self.layout, previous, chip_end) {
                    emit(candidate);
                }
            }
        }
    }

    fn reset_pairs(&mut self) {
        self.previous_chip = None;
        for row in &mut self.rows {
            row.reset();
        }
    }
}

#[derive(Clone, Copy)]
struct Chip {
    high: bool,
    first_sample: u64,
}

#[derive(Default)]
struct Row {
    sync_word: u32,
    sync_starts: [u64; MAX_SYNC_BITS],
    sync_position: usize,
    sync_length: usize,
    received: usize,
    bytes: [u8; frame::FRAME_BYTES],
    first_sample: u64,
}

impl Row {
    fn reset(&mut self) {
        self.sync_word = 0;
        self.sync_length = 0;
        self.received = 0;
    }

    fn push_bit(
        &mut self,
        layout: Layout,
        first: Chip,
        end_sample_exclusive: u64,
    ) -> Option<Candidate> {
        let bit = u8::from(first.high);
        if self.received == 0 {
            let sync_mask = (1 << layout.sync_bits) - 1;
            self.sync_word = ((self.sync_word << 1) | u32::from(bit)) & sync_mask;
            self.sync_starts[self.sync_position] = first.first_sample;
            self.sync_position += 1;
            if self.sync_position == layout.sync_bits {
                self.sync_position = 0;
            }
            self.sync_length = (self.sync_length + 1).min(layout.sync_bits);
            if self.sync_length == layout.sync_bits && self.sync_word == layout.sync_word {
                self.bytes = [0; frame::FRAME_BYTES];
                let sync_bytes = (layout.sync_word << (32 - layout.sync_bits)).to_be_bytes();
                self.bytes[..sync_bytes.len()].copy_from_slice(&sync_bytes);
                self.first_sample = self.sync_starts[self.sync_position];
                self.received = layout.sync_bits;
            }
            return None;
        }

        let byte = &mut self.bytes[self.received / 8];
        *byte |= bit << (7 - self.received % 8);
        self.received += 1;
        if self.received < layout.frame_bits {
            return None;
        }
        self.reset();
        Some(Candidate {
            bytes: self.bytes,
            first_sample: self.first_sample,
            end_sample_exclusive,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::frame::{FRAME_BYTES, SYNC};
    use super::*;

    #[test]
    fn requires_a_complete_terminal_chip_after_stationary_carrier() {
        const RATE_HZ: u32 = 524_288;
        const CHIP_SAMPLES: usize = 16;
        const LOW: f64 = 4.0;
        const HIGH: f64 = 16_384.0;
        let mut packet = [0x7e; FRAME_BYTES];
        packet[..SYNC.len()].copy_from_slice(&SYNC);
        for prefix_chips in [1, 2] {
            let mut power = vec![LOW; 512];
            power.extend(std::iter::repeat_n(HIGH, 1_024));
            power.extend(std::iter::repeat_n(LOW, 4_096));
            power.extend(std::iter::repeat_n(HIGH, prefix_chips * CHIP_SAMPLES));
            let first = power.len();
            for bit in packet
                .iter()
                .flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1))
            {
                let pair = if bit == 0 { [LOW, HIGH] } else { [HIGH, LOW] };
                for level in pair {
                    power.extend(std::iter::repeat_n(level, CHIP_SAMPLES));
                }
            }
            let end = power.len();
            power.extend(std::iter::repeat_n(HIGH, 4 * CHIP_SAMPLES));
            let decode = |samples: &[f64]| {
                let mut detector = Detector::new(RATE_HZ, Format::Plus);
                let mut packets = Vec::new();
                for &power in samples {
                    detector.push(power, |candidate| packets.push(candidate));
                }
                packets
            };
            assert!(decode(&power[..end - CHIP_SAMPLES / 2]).is_empty());
            let packets = decode(&power);
            assert_eq!(packets.len(), 1);
            assert_eq!(packets[0].bytes, packet);
            assert!(packets[0].first_sample.abs_diff(first as u64) <= 2);
            assert!(packets[0].end_sample_exclusive.abs_diff(end as u64) <= 2);
        }
    }
}
