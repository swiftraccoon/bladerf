//! Recover 978 MHz UAT airborne messages from saved IQ.

mod demod;
mod frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{AdsbChannel, Iq16};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// Nominal UAT carrier in hertz.
pub(crate) const CHANNEL_HZ: u64 = 978_000_000;
/// One-sided receiver margin around the UAT carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = AdsbChannel::HALF_WIDTH_HZ;
/// Shortest admitted airborne replay prefix on the 20 MSps grid.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = sweep_samples(demod::MIN_FRAME_SAMPLES);
/// Planned 20 MSps prefix with room for a nominal long frame.
pub(crate) const PLANNED_FRAME_SAMPLES: u64 = sweep_samples(demod::MAX_FRAME_SAMPLES);
/// Default maximum retained frame records.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of retained frame records.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;
const DUPLICATE_WINDOW_SAMPLES: u64 = 4;
const _: () = assert!(AdsbChannel::OUTPUT_RATE_HZ == demod::SAMPLE_RATE_HZ);

const fn sweep_samples(channel_samples: u64) -> u64 {
    (channel_samples + AdsbChannel::FILTER_DELAY_OUTPUT_SAMPLES as u64)
        * (SAMPLE_RATE / AdsbChannel::OUTPUT_RATE_HZ) as u64
}

/// Replay the fixed UAT channel from one saved IQ file.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved SC16 Q11 little-endian IQ.
    input: PathBuf,
    /// Actual acquisition center frequency in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Original sample rate, either 4 or 20 MSps.
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to replay from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Maximum number of detailed frame observations to retain.
    #[arg(long, default_value_t = DEFAULT_RETAINED_FRAMES, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=MAX_RETAINED_FRAMES as u64))]
    max_frames: usize,
    /// New private JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Observation {
    first_input_sample: u64,
    end_input_sample_exclusive: u64,
    #[serde(flatten)]
    frame: frame::Frame,
}

/// Verified airborne frame observations bound to one IQ prefix.
#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    demodulated_sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Observation>,
}

impl Report {
    /// Count verified airborne frames, including omitted detail.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Count frame records retained in the report.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// Original sample rate and readback center for the fixed UAT channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Check receiver bandwidth and, for a sweep, the DC guard.
    pub(crate) fn is_covered(self) -> bool {
        let readbacks = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        let offset = self.center_hz.abs_diff(CHANNEL_HZ);
        let half_span = u64::from(self.sample_rate_hz) * 2 / 5;
        readbacks.contains(&self.center_hz)
            && [AdsbChannel::OUTPUT_RATE_HZ, SAMPLE_RATE].contains(&self.sample_rate_hz)
            && offset + CHANNEL_HALF_WIDTH_HZ <= half_span
            && (self.sample_rate_hz != SAMPLE_RATE
                || offset >= CHANNEL_HALF_WIDTH_HZ + CENTER_GUARD_HZ as u64)
    }

    fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        check_stop(stop)?;
        let mut stream = self.start(source_samples, sample_limit, max_frames)?;
        let digest = receiver::read_q11_prefix(
            input,
            stream.evidence.analyzed_samples,
            stop,
            |sample, index| {
                stream.push(sample, index);
                Ok(())
            },
        )?;
        check_stop(stop)?;
        Ok(stream.finish(digest, max_frames))
    }

    /// Prepare a bounded channel decoder before reading IQ.
    ///
    /// # Errors
    /// Rejects unsupported geometry, source extent or frame cap.
    pub(crate) fn start(
        self,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
    ) -> Result<Stream> {
        ensure!(
            self.is_covered(),
            "UAT replay needs a usable 4 MSps channel or guarded 20 MSps coverage of 978 MHz"
        );
        ensure!(
            source_samples <= MAX_SAMPLES,
            "UAT input exceeds the supported extent"
        );
        ensure!(max_frames <= MAX_RETAINED_FRAMES, "invalid UAT frame cap");
        let channel = AdsbChannel::new(
            self.sample_rate_hz,
            CHANNEL_HZ as f64 - self.center_hz as f64,
        )?;
        Ok(Stream {
            geometry: self,
            evidence: Evidence {
                stride: channel.stride() as u64,
                delay: channel.delay_input_samples() as u64,
                pending: None,
                analyzed_samples: source_samples.min(sample_limit),
                max_frames,
                validated_frames: 0,
                frames: Vec::new(),
            },
            channel,
            decoder: demod::Demodulator::new(),
            source_samples,
        })
    }
}

/// Incremental UAT validation state for one admitted prefix.
pub(crate) struct Stream {
    geometry: Channel,
    channel: AdsbChannel,
    decoder: demod::Demodulator,
    source_samples: u64,
    evidence: Evidence,
}

struct Evidence {
    pending: Option<Observation>,
    stride: u64,
    delay: u64,
    analyzed_samples: u64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Stream {
    /// Visit one validated source sample in sequence.
    pub(crate) fn push(&mut self, sample: Iq16, index: u64) {
        let Some(sample) = self.channel.push(sample) else {
            return;
        };
        let channel_index = index / self.evidence.stride;
        self.decoder.push(sample, channel_index, |candidate| {
            self.evidence.observe(candidate);
        });
    }

    /// Flush complete basic frames, bind the digest and apply the detail cap.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String, remaining: usize) -> Report {
        self.decoder
            .finish(|candidate| self.evidence.observe(candidate));
        if let Some(observation) = self.evidence.pending.take() {
            self.evidence.record(observation);
        }
        self.evidence.frames.truncate(remaining);
        Report {
            schema_version: 1,
            operation: "identify_uat_in_saved_iq",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.geometry.center_hz,
            channel_frequency_hz: CHANNEL_HZ,
            sample_rate_hz: self.geometry.sample_rate_hz,
            demodulated_sample_rate_hz: demod::SAMPLE_RATE_HZ,
            source_samples: self.source_samples,
            analyzed_samples: self.evidence.analyzed_samples,
            analyzed_iq_sha256,
            validated_frames: self.evidence.validated_frames,
            omitted_valid_frames: self.evidence.validated_frames - self.evidence.frames.len(),
            frames: self.evidence.frames,
        }
    }
}

impl Evidence {
    fn observe(&mut self, candidate: demod::Candidate) {
        let bytes = &candidate.codeword[..candidate.length];
        let frame = if bytes.len() == frame::LONG_BYTES {
            frame::Frame::parse(bytes).or_else(|| frame::Frame::parse(&bytes[..frame::BASIC_BYTES]))
        } else {
            frame::Frame::parse(bytes)
        };
        let Some(frame) = frame else {
            return;
        };
        let end_sample = if frame.bytes().len() == frame::BASIC_BYTES {
            candidate.basic_end_sample
        } else {
            candidate.end_sample
        };
        let source_sample = |sample: u64| ((sample + 1) * self.stride - 1).checked_sub(self.delay);
        let (Some(first_input_sample), Some(end_input_sample_exclusive)) = (
            source_sample(candidate.first_sample),
            source_sample(end_sample),
        ) else {
            return;
        };
        if first_input_sample >= end_input_sample_exclusive
            || end_input_sample_exclusive > self.analyzed_samples
        {
            return;
        }
        let observation = Observation {
            first_input_sample,
            end_input_sample_exclusive,
            frame,
        };
        if let Some(pending) = &mut self.pending
            && first_input_sample.abs_diff(pending.first_input_sample)
                <= DUPLICATE_WINDOW_SAMPLES * self.stride
            && observation.frame.bytes() == pending.frame.bytes()
        {
            // One packet can validate at several adjacent timing phases.
            if observation.frame.corrected_symbols < pending.frame.corrected_symbols {
                *pending = observation;
            }
        } else if let Some(pending) = self.pending.replace(observation) {
            self.record(pending);
        }
    }

    fn record(&mut self, observation: Observation) {
        self.validated_frames += 1;
        if self.frames.len() < self.max_frames {
            self.frames.push(observation);
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "UAT replay interrupted");
    Ok(())
}

/// Replay an admitted IQ file and publish a private UAT report.
///
/// # Errors
/// Rejects invalid or changed IQ, interruption and publication failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let channel = Channel {
        center_hz: args.center_hz,
        sample_rate_hz: args.sample_rate_hz,
    };
    let source = root.join(args.input);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let stamp = input.metadata()?;
    ensure!(
        length == stamp.len() && length.is_multiple_of(IQ_BYTES),
        "UAT input needs a stable extent of complete I/Q pairs"
    );
    let report = channel.replay(
        &mut input,
        length / IQ_BYTES,
        args.max_samples,
        args.max_frames,
        stop,
    )?;
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        let current = source.symlink_metadata()?;
        ensure!(
            current.is_file()
                && file_pin::same_file(&stamp, &current)
                && file_pin::same_file(&stamp, &input.metadata()?),
            "UAT IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} UAT airborne frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_alignments_keep_the_fewest_corrections_and_repeats_stay_distinct() {
        let bytes = data_encoding::HEXLOWER
            .decode(b"00faa123555555c000000354064432c02800fe97c434e1ff5365cf8fafe4")
            .unwrap();
        let mut evidence = Evidence {
            pending: None,
            stride: 1,
            delay: 0,
            analyzed_samples: 5_000,
            max_frames: 2,
            validated_frames: 0,
            frames: Vec::new(),
        };
        for (first_sample, error) in [(100, 1), (101, 0), (2_000, 0)] {
            let mut codeword = [0; frame::LONG_BYTES];
            codeword[..frame::BASIC_BYTES].copy_from_slice(&bytes);
            codeword[8] ^= error;
            let end_sample = first_sample + demod::MIN_FRAME_SAMPLES;
            evidence.observe(demod::Candidate {
                first_sample,
                end_sample,
                basic_end_sample: end_sample,
                codeword,
                length: frame::BASIC_BYTES,
            });
        }
        assert_eq!(evidence.validated_frames, 1);
        assert_eq!(evidence.frames[0].first_input_sample, 101);
        assert_eq!(evidence.frames[0].frame.corrected_symbols, 0);
        assert_eq!(evidence.pending.unwrap().first_input_sample, 2_000);
    }
}
