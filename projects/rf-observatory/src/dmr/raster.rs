//! Search a configured 12.5 kHz DMR lattice through one guarded IQ stream.

use std::io::Read;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq, RasterBank};
use crate::receiver::{self, ACQUISITION_CENTER_TOLERANCE_HZ};
use crate::spectrum::{CENTER_GUARD_HZ, USABLE_HALF_SPAN_HZ};
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE};

use super::{CHANNEL_HALF_WIDTH_HZ, MAX_RETAINED_FRAMES, Observation, check_stop, demod};

/// Spacing between consecutive configured DMR channels.
pub(crate) const GRID_SPACING_HZ: u64 = RasterBank::GRID_SPACING_HZ;

/// Inclusive endpoints of a configured 12.5 kHz channel lattice.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    pub(crate) lower_hz: u64,
    pub(crate) upper_hz: u64,
}

impl Bounds {
    /// Validate inclusive limits anchored at the lower channel.
    ///
    /// # Errors
    /// Rejects reversed, out-of-band, or misaligned endpoints.
    pub(crate) fn new(lower_hz: u64, upper_hz: u64) -> Result<Self> {
        validate_range(lower_hz, upper_hz)?;
        Ok(Self { lower_hz, upper_hz })
    }

    /// Read the inclusive limits from a `LOWER:UPPER` selector in hertz.
    ///
    /// # Errors
    /// Rejects malformed values and invalid lattice limits.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER:UPPER in hertz")?;
        Self::new(
            lower
                .parse()
                .context("invalid DMR lower carrier in hertz")?,
            upper
                .parse()
                .context("invalid DMR upper carrier in hertz")?,
        )
    }
}

/// Verify that inclusive endpoints describe one supported channel lattice.
///
/// # Errors
/// Rejects endpoints outside the receiver range, reversed endpoints, or an
/// upper endpoint that is not aligned relative to the lower endpoint.
fn validate_range(lower_hz: u64, upper_hz: u64) -> Result<()> {
    ensure!(
        MIN_CENTER_HZ <= lower_hz
            && lower_hz <= upper_hz
            && upper_hz <= MAX_CENTER_HZ
            && (upper_hz - lower_hz).is_multiple_of(GRID_SPACING_HZ),
        "DMR raster bounds must describe a 12.5 kHz grid within {MIN_CENTER_HZ}..={MAX_CENTER_HZ} Hz"
    );
    Ok(())
}

/// Inclusive frequency limits of one contiguous guarded run of channels.
#[derive(Clone, Copy, Serialize)]
pub(crate) struct Span {
    pub(crate) first_hz: u64,
    pub(crate) last_hz: u64,
}

impl Span {
    fn len(self) -> usize {
        ((self.last_hz - self.first_hz) / GRID_SPACING_HZ + 1) as usize
    }
}

/// Channels of the configured lattice inside the usable passband of a dwell.
pub(crate) struct Grid {
    center_hz: u64,
    grid_offset_hz: i64,
    spans: [Option<Span>; 2],
}

impl Grid {
    /// Intersect the configured lattice with both sides of the center guard.
    ///
    /// # Errors
    /// Rejects invalid lattice bounds or an unsupported acquisition center.
    pub(crate) fn new(center_hz: u64, lower_hz: u64, upper_hz: u64) -> Result<Self> {
        validate_range(lower_hz, upper_hz)?;
        let tolerance = ACQUISITION_CENTER_TOLERANCE_HZ;
        ensure!(
            (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&center_hz),
            "DMR raster capture center lies outside the supported range"
        );

        let step = GRID_SPACING_HZ as i64;
        let grid_offset_hz =
            (lower_hz as i64 - center_hz as i64 + step / 2).rem_euclid(step) - step / 2;
        let inner = CENTER_GUARD_HZ as u64 + CHANNEL_HALF_WIDTH_HZ;
        let outer = USABLE_HALF_SPAN_HZ as u64 - CHANNEL_HALF_WIDTH_HZ;
        let spans = [
            (center_hz - outer, center_hz - inner),
            (center_hz + inner, center_hz + outer),
        ]
        .map(|(start, end)| {
            let start = start.max(lower_hz);
            let end = end.min(upper_hz);
            if start > end {
                return None;
            }
            let first_hz =
                lower_hz + (start - lower_hz).div_ceil(GRID_SPACING_HZ) * GRID_SPACING_HZ;
            let last_hz = lower_hz + (end - lower_hz) / GRID_SPACING_HZ * GRID_SPACING_HZ;
            (first_hz <= last_hz).then_some(Span { first_hz, last_hz })
        });
        Ok(Self {
            center_hz,
            grid_offset_hz,
            spans,
        })
    }

    /// Contiguous guarded channel runs, ordered by frequency.
    pub(crate) fn spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.spans.iter().flatten().copied()
    }

    /// Number of guarded channel centers in the saved dwell.
    pub(crate) fn channel_count(&self) -> usize {
        self.spans().map(Span::len).sum()
    }

    /// Count decoder inputs across all selected channels and complete blocks.
    pub(crate) fn channelized_samples(&self, original_samples: u64) -> u64 {
        original_samples / RasterBank::DMR_STRIDE as u64 * self.channel_count() as u64
    }

    /// Read, hash, and decode an admitted source prefix through one filter bank.
    /// The caller bounds work and verifies source identity after processing.
    ///
    /// # Errors
    /// Rejects an empty grid, invalid prefix or retention cap, interrupted
    /// replay, and incomplete or invalid IQ.
    pub(crate) fn replay(
        &self,
        input: &mut dyn Read,
        source_samples: u64,
        selected_samples: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        ensure!(
            (super::MIN_SWEEP_SAMPLES..=source_samples).contains(&selected_samples)
                && selected_samples <= crate::MAX_SAMPLES,
            "invalid DMR raster IQ prefix extent"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid DMR frame retention cap"
        );
        check_stop(stop)?;

        let lattice_center = self.center_hz as i64 + self.grid_offset_hz;
        let channels: Vec<_> = self
            .spans()
            .flat_map(|span| (span.first_hz..=span.last_hz).step_by(GRID_SPACING_HZ as usize))
            .collect();
        let bins: Vec<_> = channels
            .iter()
            .map(|&channel_hz| {
                let bin = (channel_hz as i64 - lattice_center) / GRID_SPACING_HZ as i64;
                bin.rem_euclid(RasterBank::BIN_COUNT as i64) as usize
            })
            .collect();
        let mut bank = RasterBank::dmr(self.grid_offset_hz, &bins)?;
        let rate = RasterBank::DMR_OUTPUT_RATE_HZ;
        let stride = RasterBank::DMR_STRIDE as u64;
        let mut decoders: Vec<_> = channels.iter().map(|_| demod::Decoder::new(rate)).collect();
        let mut validated_frames = 0;
        let mut frames = Vec::new();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, selected_samples, stop, |sample, index| {
                if let Some(iqs) = bank.push(sample) {
                    for ((decoder, iq), &channel_hz) in decoders.iter_mut().zip(iqs).zip(&channels)
                    {
                        let iq = Iq {
                            i: f64::from(iq.re),
                            q: f64::from(iq.im),
                        };
                        if let Some(packet) = decoder.push(iq, index / stride)
                            && let Some(observation) = Observation::from_packet(
                                packet,
                                stride,
                                RasterBank::DMR_DELAY_INPUT_SAMPLES as f64
                                    + decoder.delay_samples() * stride as f64,
                                selected_samples,
                            )
                        {
                            validated_frames += 1;
                            if frames.len() < max_frames {
                                frames.push(Frame {
                                    channel_hz,
                                    observation,
                                });
                            }
                        }
                    }
                }
                Ok(())
            })?;
        check_stop(stop)?;
        Ok(Report {
            center_frequency_hz: self.center_hz,
            input_sample_rate_hz: SAMPLE_RATE,
            channel_sample_rate_hz: rate,
            filter_delay_input_samples: RasterBank::DMR_DELAY_INPUT_SAMPLES,
            source_samples,
            analyzed_samples: selected_samples,
            analyzed_iq_sha256,
            sync_candidates: decoders.iter().map(demod::Decoder::synchronizations).sum(),
            validated_frames,
            omitted_valid_frames: validated_frames - frames.len(),
            frames,
        })
    }
}

#[derive(Serialize)]
struct Frame {
    channel_hz: u64,
    #[serde(flatten)]
    observation: Observation,
}

/// Source-bound DMR Voice Link Control evidence from one guarded raster.
#[derive(Serialize)]
pub(crate) struct Report {
    center_frequency_hz: u64,
    input_sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    filter_delay_input_samples: u64,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    sync_candidates: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Frame>,
}

impl Report {
    /// Number of verified physical bursts, including omitted details.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed burst observations retained.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dmr::Channel;

    #[test]
    fn offset_lattice_matches_scalar_channel_admission_at_the_guards() {
        let lower_hz = 432_006_250;
        let upper_hz = 448_081_250;
        let center_hz = 440_009_375;
        let grid = Grid::new(center_hz, lower_hz, upper_hz).unwrap();
        assert_eq!(grid.grid_offset_hz, -3_125);
        let channels: Vec<_> = grid
            .spans()
            .flat_map(|span| (span.first_hz..=span.last_hz).step_by(GRID_SPACING_HZ as usize))
            .collect();
        let scalar: Vec<_> = (lower_hz..=upper_hz)
            .step_by(GRID_SPACING_HZ as usize)
            .filter(|&channel_hz| {
                Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered()
            })
            .collect();
        assert_eq!(channels, scalar);
        assert_eq!(grid.channel_count(), scalar.len());
        assert!(validate_range(lower_hz, upper_hz + 1).is_err());
    }
}
