//! Qualify LTE FDD broadcast information using repeated CRC and radio-frame timing.

mod bch;
mod demod;
mod payload;

use std::sync::atomic::AtomicBool;

use anyhow::Result;
use rustfft::num_complex::Complex32;
use serde::Serialize;

use super::{Observation, REPEAT_CHANNEL_SAMPLES, check_stop};

const FRAME_CHANNEL_SAMPLES: usize = 2 * REPEAT_CHANNEL_SAMPLES;
const FRAMES_PER_CODEWORD: usize = 4;
const SYSTEM_FRAME_MODULUS: usize = 1024;
const PORT_COUNTS: [u8; 3] = [1, 2, 4];

#[derive(Serialize)]
pub(super) struct Evidence {
    subframe_zero_observations: usize,
    scheduled_radio_frames: usize,
    complete_blocks: usize,
    confirmations: Vec<Confirmation>,
}

impl Evidence {
    pub(super) fn confirmed(&self) -> bool {
        !self.confirmations.is_empty()
    }
}

#[derive(Serialize)]
struct Confirmation {
    antenna_ports: u8,
    checks: Vec<Check>,
}

#[derive(Serialize)]
struct Check {
    pss_start_channel_sample: usize,
    pss_observed: bool,
    carrier_offset_hz: f32,
    radio_frames: usize,
    mib: payload::Mib,
    #[serde(skip)]
    frame_offset: usize,
}

impl Check {
    fn agrees_with(&self, other: &Self) -> bool {
        let epoch = |check: &Self| {
            (usize::from(check.mib.system_frame_number()) + SYSTEM_FRAME_MODULUS
                - check.frame_offset % SYSTEM_FRAME_MODULUS)
                % SYSTEM_FRAME_MODULUS
        };
        epoch(self) == epoch(other) && self.mib.same_configuration(other.mib)
    }
}

struct RadioFrame {
    pss_start_channel_sample: usize,
    pss_observed: bool,
    frame_offset: usize,
    block: demod::Block,
}

pub(super) struct Decoder {
    demodulator: demod::Demodulator,
}

impl Decoder {
    pub(super) fn new() -> Self {
        Self {
            demodulator: demod::Demodulator::new(),
        }
    }

    /// Check complete subframe-zero blocks on the admitted cell's frame clock.
    ///
    /// # Errors
    /// Returns an error if replay is interrupted.
    pub(super) fn examine(
        &mut self,
        iq: &[Complex32],
        pci: u16,
        observations: &[Observation],
        stop: &AtomicBool,
    ) -> Result<Evidence> {
        let mut radio_frames = Vec::new();
        let observed = || observations.iter().filter(|hit| hit.subframe == 0);
        let mut scheduled_radio_frames = 0;
        if let Some(anchor) = observations.get(observations.len() / 2) {
            let half_frame = usize::from(anchor.subframe == 5) * REPEAT_CHANNEL_SAMPLES;
            let first = (anchor.pss_start_channel_sample + FRAME_CHANNEL_SAMPLES - half_frame)
                % FRAME_CHANNEL_SAMPLES;
            let mut observed = observed().peekable();
            // The established cell can continue transmitting when individual
            // PSS or SSS matches fall below the synchronization thresholds.
            for (frame_offset, predicted) in
                (first..iq.len()).step_by(FRAME_CHANNEL_SAMPLES).enumerate()
            {
                check_stop(stop)?;
                scheduled_radio_frames += 1;
                let observed = observed.next_if(|hit| {
                    hit.pss_start_channel_sample < predicted + FRAME_CHANNEL_SAMPLES / 2
                });
                let start = observed.map_or(predicted, |hit| hit.pss_start_channel_sample);
                if let Some(block) = self.demodulator.extract(iq, start, pci) {
                    radio_frames.push(RadioFrame {
                        pss_start_channel_sample: start,
                        pss_observed: observed.is_some(),
                        frame_offset,
                        block,
                    });
                }
            }
        }

        let mut confirmations = Vec::new();
        if radio_frames.len() >= 2 {
            for (port_index, antenna_ports) in PORT_COUNTS.into_iter().enumerate() {
                for initial_phase in 0..FRAMES_PER_CODEWORD {
                    let checks = check_phase(
                        &radio_frames,
                        pci,
                        port_index,
                        antenna_ports,
                        initial_phase,
                        stop,
                    )?;
                    if checks.len() >= 2 {
                        confirmations.push(Confirmation {
                            antenna_ports,
                            checks,
                        });
                    }
                }
            }
        }
        Ok(Evidence {
            subframe_zero_observations: observed().count(),
            scheduled_radio_frames,
            complete_blocks: radio_frames.len(),
            confirmations,
        })
    }
}

fn check_phase(
    radio_frames: &[RadioFrame],
    pci: u16,
    port_index: usize,
    antenna_ports: u8,
    initial_phase: usize,
    stop: &AtomicBool,
) -> Result<Vec<Check>> {
    let quarter_index = |frame: &RadioFrame| frame.frame_offset + initial_phase;
    let mut checks = Vec::new();
    for group in radio_frames.chunk_by(|earlier, later| {
        let earlier = quarter_index(earlier);
        let later = quarter_index(later);
        earlier + 1 == later && earlier / FRAMES_PER_CODEWORD == later / FRAMES_PER_CODEWORD
    }) {
        let before = checks.len();
        for frame in group {
            check_stop(stop)?;
            let phase = quarter_index(frame) % FRAMES_PER_CODEWORD;
            let soft_bits = std::slice::from_ref(&frame.block.llrs[port_index]);
            if let Some(check) = decode_check(frame, soft_bits, phase, pci, antenna_ports) {
                checks.push(check);
            }
        }
        // A coherent set of quarters can recover a block that was too weak
        // in isolation. Its samples contribute to at most one CRC check.
        if before == checks.len() && group.len() > 1 {
            check_stop(stop)?;
            let combined: Vec<_> = group
                .iter()
                .map(|frame| frame.block.llrs[port_index])
                .collect();
            let phase = quarter_index(&group[0]) % FRAMES_PER_CODEWORD;
            if let Some(check) = decode_check(&group[0], &combined, phase, pci, antenna_ports) {
                checks.push(check);
            }
        }
    }
    // Keep the largest set of independent checks with one SFN origin and
    // matching broadcast configuration. An isolated CRC is insufficient.
    let Some(anchor) = checks.iter().max_by_key(|item| {
        checks
            .iter()
            .filter(|other| item.agrees_with(other))
            .count()
    }) else {
        return Ok(checks);
    };
    let accepted: Vec<_> = checks.iter().map(|item| item.agrees_with(anchor)).collect();
    Ok(checks
        .into_iter()
        .zip(accepted)
        .filter_map(|(check, keep)| keep.then_some(check))
        .collect())
}

fn decode_check(
    first: &RadioFrame,
    soft_bits: &[[f32; 480]],
    phase: usize,
    pci: u16,
    antenna_ports: u8,
) -> Option<Check> {
    let bits = bch::seen_bits(soft_bits, phase, pci, antenna_ports)?;
    let mib = payload::decode(bits, phase)?;
    Some(Check {
        pss_start_channel_sample: first.pss_start_channel_sample,
        pss_observed: first.pss_observed,
        carrier_offset_hz: first.block.cfo_hz,
        radio_frames: soft_bits.len(),
        mib,
        frame_offset: first.frame_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bch::tests::REFERENCE;
    use demod::tests::{PSS_START, fixture};

    fn frame(quarter: usize, observed_offset: usize) -> RadioFrame {
        let soft_bits = std::array::from_fn(|index| {
            let bit = quarter * 480 + index;
            let encoded = (REFERENCE[bit / 8] >> (7 - bit % 8)) & 1;
            1.0 - 2.0 * f32::from(encoded)
        });
        let mut llrs = [[0.0; 480]; 3];
        llrs[1] = soft_bits;
        RadioFrame {
            pss_start_channel_sample: observed_offset * FRAME_CHANNEL_SAMPLES,
            pss_observed: true,
            frame_offset: observed_offset,
            block: demod::Block { llrs, cfo_hz: 0.0 },
        }
    }

    #[test]
    fn requires_independent_crc_checks_with_consistent_frame_timing() -> Result<()> {
        let stop = AtomicBool::new(false);
        let ordered = [frame(1, 0), frame(2, 1), frame(3, 2)];
        let checks = check_phase(&ordered, 235, 1, 2, 1, &stop)?;
        let frame_numbers: Vec<_> = checks
            .iter()
            .map(|check| check.mib.system_frame_number())
            .collect();
        assert_eq!(frame_numbers, [625, 626, 627]);
        assert!(checks.iter().all(|check| check.radio_frames == 1));

        // Repeating a valid first quarter in an adjacent radio frame is not
        // a second timing-consistent check.
        let repeated = [frame(1, 0), frame(1, 1)];
        assert_eq!(check_phase(&repeated, 235, 1, 2, 1, &stop)?.len(), 1);
        Ok(())
    }

    #[test]
    fn recovers_broadcast_when_a_sync_observation_is_missing() -> Result<()> {
        let mut iq = vec![Complex32::default(); 2 * FRAME_CHANNEL_SAMPLES];
        for (index, quarter) in [1, 2].into_iter().enumerate() {
            let block = fixture(2, 1800.0, quarter);
            let offset = index * FRAME_CHANNEL_SAMPLES;
            iq[offset..offset + block.len()].copy_from_slice(&block);
        }
        let observations = [(1, 5), (2, 0), (3, 5)].map(|(half_frame, subframe)| Observation {
            pss_start_channel_sample: PSS_START + half_frame * REPEAT_CHANNEL_SAMPLES,
            subframe,
            pss_score: 1.0,
            sss_score: 1.0,
        });
        let stop = AtomicBool::new(false);
        let evidence = Decoder::new().examine(&iq, 235, &observations, &stop)?;
        assert_eq!(evidence.subframe_zero_observations, 1);
        assert_eq!(evidence.complete_blocks, 2);
        let [confirmed] = evidence.confirmations.as_slice() else {
            panic!("expected one confirmed antenna and phase hypothesis");
        };
        assert_eq!(confirmed.antenna_ports, 2);
        let [first, second] = confirmed.checks.as_slice() else {
            panic!("expected two independent frame checks");
        };
        assert!(!first.pss_observed);
        assert!(second.pss_observed);
        assert_eq!(first.mib.system_frame_number(), 625);
        assert_eq!(second.mib.system_frame_number(), 626);
        Ok(())
    }
}
