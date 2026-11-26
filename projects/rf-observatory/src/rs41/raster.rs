//! Search a guard-safe 20 MSps dwell using an overlapping RS41 channel bank.

use std::io::Read;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq, RasterBank};
use crate::receiver;
use crate::{MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE};

use super::{Channel, MAX_RETAINED_FRAMES, Observation, check_stop, demod};

/// Lower carrier limit from the RS41-SG technical data.
pub(crate) const BAND_LOWER_HZ: u64 = 400_150_000;
/// Upper carrier limit from the RS41-SG technical data.
pub(crate) const BAND_UPPER_HZ: u64 = 406_000_000;
/// Spacing between overlapping search channels, in hertz.
pub(crate) const GRID_SPACING_HZ: u64 = RasterBank::GRID_SPACING_HZ;
const MERGE_START_SAMPLES: u64 = (2 * SAMPLE_RATE as u64).div_ceil(demod::BAUD);
const MERGE_CARRIER_HZ: u64 = GRID_SPACING_HZ / 4;

/// Inclusive endpoints of the 12.5 kHz RS41 search raster.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    pub(crate) lower_hz: u64,
    pub(crate) upper_hz: u64,
}

impl Bounds {
    /// Validate endpoints on the absolute RS41 grid.
    ///
    /// # Errors
    /// Rejects reversed, unaligned or out-of-band endpoints.
    pub(crate) fn new(lower_hz: u64, upper_hz: u64) -> Result<Self> {
        validate_range(lower_hz, upper_hz)?;
        Ok(Self { lower_hz, upper_hz })
    }

    /// Parse inclusive `LOWER:UPPER` limits in hertz.
    ///
    /// # Errors
    /// Rejects malformed values or invalid raster endpoints.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER:UPPER in hertz")?;
        Self::new(
            lower
                .parse()
                .context("invalid RS41 lower carrier in hertz")?,
            upper
                .parse()
                .context("invalid RS41 upper carrier in hertz")?,
        )
    }
}

/// Check inclusive limits on the absolute RS41 search raster.
///
/// # Errors
/// Rejects reversed, unaligned, or out-of-band limits.
pub(crate) fn validate_range(lower_hz: u64, upper_hz: u64) -> Result<()> {
    ensure!(
        BAND_LOWER_HZ <= lower_hz
            && lower_hz <= upper_hz
            && upper_hz <= BAND_UPPER_HZ
            && lower_hz.is_multiple_of(GRID_SPACING_HZ)
            && upper_hz.is_multiple_of(GRID_SPACING_HZ),
        "RS41 raster limits must be ordered on the 12.5 kHz grid within 400.15–406 MHz"
    );
    Ok(())
}

/// A dwell's admitted raster centers and corresponding DFT bins.
pub(crate) struct Grid {
    center_hz: u64,
    grid_offset_hz: i64,
    channels_hz: Vec<u64>,
    bins: Vec<usize>,
}

impl Grid {
    /// Select every requested center inside the guarded passband.
    /// A dwell may have zero eligible centers.
    ///
    /// # Errors
    /// Rejects invalid raster limits or capture center.
    pub(crate) fn new(center_hz: u64, lower_hz: u64, upper_hz: u64) -> Result<Self> {
        validate_range(lower_hz, upper_hz)?;
        let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
        ensure!(
            (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&center_hz),
            "RS41 raster capture center lies outside the supported range"
        );
        let grid_center = ((center_hz + GRID_SPACING_HZ / 2) / GRID_SPACING_HZ) * GRID_SPACING_HZ;
        let channels_hz: Vec<_> = (lower_hz..=upper_hz)
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
        let bins = channels_hz
            .iter()
            .map(|&channel| {
                let index = (channel as i64 - grid_center as i64) / GRID_SPACING_HZ as i64;
                index.rem_euclid(RasterBank::BIN_COUNT as i64) as usize
            })
            .collect();
        Ok(Self {
            center_hz,
            grid_offset_hz: grid_center as i64 - center_hz as i64,
            channels_hz,
            bins,
        })
    }

    /// Absolute centers admitted for replay, in ascending order.
    pub(crate) fn channels_hz(&self) -> &[u64] {
        &self.channels_hz
    }

    /// Count completed output samples charged across all admitted channels.
    pub(crate) fn channelized_samples(&self, original_samples: u64) -> u64 {
        original_samples / RasterBank::RS41_STRIDE as u64 * self.channels_hz.len() as u64
    }

    /// Decode every channel of a bounded dwell prefix in one source read.
    /// The caller must admit the reader, budget the channelized sample work,
    /// and verify the source after replay.
    ///
    /// # Errors
    /// Rejects impossible geometry or extent, incomplete or invalid input,
    /// and interruption.
    pub(crate) fn replay(
        &self,
        input: &mut dyn Read,
        source_samples: u64,
        selected_samples: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        ensure!(
            (1..=MAX_SAMPLES).contains(&selected_samples) && selected_samples <= source_samples,
            "invalid RS41 raster IQ prefix extent"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid RS41 frame retention cap"
        );
        check_stop(stop)?;
        let mut bank = RasterBank::rs41(self.grid_offset_hz, &self.bins)?;
        let mut channels: Vec<_> = self.channels_hz.iter().copied().map(Search::new).collect();
        let stride = RasterBank::RS41_STRIDE as u64;
        let mut frames = Vec::new();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, selected_samples, stop, |x, n| {
                if let Some(iqs) = bank.push(x) {
                    for (state, iq) in channels.iter_mut().zip(iqs) {
                        let iq = Iq {
                            i: f64::from(iq.re),
                            q: f64::from(iq.im),
                        };
                        if let Some(decoded) = state.decoder.push(iq, n / stride) {
                            state.valid_candidates += 1;
                            let delay = RasterBank::RS41_DELAY_INPUT_SAMPLES
                                + state.decoder.delay_samples() * stride;
                            let input_sample = |sample: u64| {
                                ((sample + 1) * stride - 1)
                                    .saturating_sub(delay)
                                    .min(selected_samples)
                            };
                            frames.push(Frame {
                                channel_hz: state.channel_hz,
                                estimated_carrier_hz: (state.channel_hz as f64
                                    + decoded.carrier_offset_hz)
                                    .round()
                                    as u64,
                                observation: Observation {
                                    first_input_sample: input_sample(decoded.first_sample),
                                    end_input_sample_exclusive: input_sample(
                                        decoded.end_sample_exclusive,
                                    ),
                                    frame_bytes: decoded.frame_bytes,
                                    corrected_bytes: decoded.corrected_bytes,
                                    status: decoded.status,
                                },
                            });
                        }
                    }
                }
                Ok(())
            })?;
        check_stop(stop)?;
        let valid_candidates = frames.len();
        let mut frames = merge(frames);
        let validated_frames = frames.len();
        frames.truncate(max_frames);
        let channels = channels.into_iter().map(Search::report).collect();
        Ok(Report {
            center_frequency_hz: self.center_hz,
            input_sample_rate_hz: SAMPLE_RATE,
            channel_sample_rate_hz: RasterBank::RS41_OUTPUT_RATE_HZ,
            grid_spacing_hz: GRID_SPACING_HZ,
            filter_delay_input_samples: RasterBank::RS41_DELAY_INPUT_SAMPLES,
            source_samples,
            analyzed_samples: selected_samples,
            analyzed_iq_sha256,
            channelized_samples: self.channelized_samples(selected_samples),
            valid_candidates,
            validated_frames,
            omitted_valid_frames: validated_frames - frames.len(),
            channels,
            frames,
        })
    }
}

struct Search {
    channel_hz: u64,
    decoder: demod::Decoder,
    valid_candidates: usize,
}

impl Search {
    fn new(channel_hz: u64) -> Self {
        Self {
            channel_hz,
            decoder: demod::Decoder::new(RasterBank::RS41_OUTPUT_RATE_HZ),
            valid_candidates: 0,
        }
    }

    fn report(self) -> ChannelReport {
        ChannelReport {
            channel_hz: self.channel_hz,
            sync_candidates: self.decoder.synchronizations(),
            valid_candidates: self.valid_candidates,
        }
    }
}

#[derive(Serialize)]
struct ChannelReport {
    channel_hz: u64,
    sync_candidates: usize,
    valid_candidates: usize,
}

#[derive(Serialize)]
struct Frame {
    channel_hz: u64,
    estimated_carrier_hz: u64,
    #[serde(flatten)]
    observation: Observation,
}

/// Source-bound RS41 evidence from one dwell and its complete selected raster.
#[derive(Serialize)]
pub(crate) struct Report {
    center_frequency_hz: u64,
    input_sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    grid_spacing_hz: u64,
    filter_delay_input_samples: u64,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    channelized_samples: u64,
    valid_candidates: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    channels: Vec<ChannelReport>,
    frames: Vec<Frame>,
}

impl Report {
    /// All candidate frame decodes across the channel bank before merging.
    pub(crate) fn valid_candidates(&self) -> usize {
        self.valid_candidates
    }

    /// Verified frame occurrences after merging overlapping channel results.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of retained matched frame occurrences.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

fn preference(frame: &Frame) -> (usize, u64, u64) {
    (
        frame.observation.corrected_bytes,
        frame.channel_hz.abs_diff(frame.estimated_carrier_hz),
        frame.channel_hz,
    )
}

fn merge(mut candidates: Vec<Frame>) -> Vec<Frame> {
    // Keep the earliest anchor when a later duplicate has fewer corrections.
    // This preserves the time window and avoids transitive merges.
    candidates.sort_by_key(|frame| (frame.observation.first_input_sample, frame.channel_hz));
    let mut groups: Vec<(u64, u64, Frame)> = Vec::new();
    for frame in candidates {
        let sample = frame.observation.first_input_sample;
        let carrier = frame.estimated_carrier_hz;
        let matching = groups
            .iter_mut()
            .rev()
            .take_while(|(start, _, _)| sample - *start <= MERGE_START_SAMPLES)
            .find(|(_, estimate, best)| {
                best.observation.status == frame.observation.status
                    && best.observation.frame_bytes == frame.observation.frame_bytes
                    && estimate.abs_diff(carrier) <= MERGE_CARRIER_HZ
            });
        if let Some((_, _, best)) = matching {
            if preference(&frame) < preference(best) {
                *best = frame;
            }
        } else {
            groups.push((sample, carrier, frame));
        }
    }
    let mut frames: Vec<_> = groups.into_iter().map(|(_, _, frame)| frame).collect();
    frames.sort_by_key(|frame| (frame.observation.first_input_sample, frame.channel_hz));
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rs41::frame::Status;

    #[test]
    fn selects_complete_grid_and_leaves_the_center_guard_clear() {
        let full = Grid::new(399_025_007, BAND_LOWER_HZ, BAND_UPPER_HZ).unwrap();
        assert_eq!(full.channels_hz().len(), 469);
        assert_eq!(full.channels_hz().first(), Some(&BAND_LOWER_HZ));
        assert_eq!(full.channels_hz().last(), Some(&BAND_UPPER_HZ));
        assert_eq!(full.grid_offset_hz, -7);
        assert_eq!(full.channelized_samples(40_000_399), 46_900_000);
        let center = 402_500_000;
        let guarded = Grid::new(center, BAND_LOWER_HZ, BAND_UPPER_HZ).unwrap();
        assert_eq!(guarded.channels_hz().len(), 452);
        assert!(!guarded.channels_hz().contains(&center));
        assert!(guarded.channels_hz().contains(&(center + 112_500)));
        assert!(
            Grid::new(900_000_000, BAND_LOWER_HZ, BAND_UPPER_HZ)
                .unwrap()
                .channels_hz()
                .is_empty()
        );
        assert!(validate_range(BAND_LOWER_HZ + 1, BAND_UPPER_HZ).is_err());
    }

    #[test]
    fn merges_adjacent_views_and_retains_distinct_occurrences() {
        let observation = |start, bin, carrier, errors| Frame {
            channel_hz: bin,
            estimated_carrier_hz: carrier,
            observation: Observation {
                first_input_sample: start,
                end_input_sample_exclusive: start + super::super::MIN_SWEEP_SAMPLES,
                frame_bytes: 320,
                corrected_bytes: errors,
                status: Status {
                    serial: "RS41T001".into(),
                    frame_number: 42,
                },
            },
        };
        let frames = merge(vec![
            observation(10_000, 402_500_000, 402_506_240, 1),
            observation(11_200, 402_512_500, 402_506_400, 0),
            observation(12_000, 404_500_000, 404_506_300, 0),
            observation(20_000_000, 402_500_000, 402_506_240, 0),
        ]);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].channel_hz, 402_512_500);
        assert_eq!(frames[1].channel_hz, 404_500_000);
        assert_eq!(frames[2].observation.first_input_sample, 20_000_000);
    }
}
