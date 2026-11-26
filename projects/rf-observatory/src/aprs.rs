//! Decode 1200-baud APRS-format AX.25 frames from one saved RF channel.

mod afsk;
mod frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::Args;
use serde::Serialize;

use crate::dsp::{self, CIC_DECIMATION, FixedBank, Iq16, NarrowFm};
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

const TARGETED_RATE_HZ: u32 = 2_000_000;
/// Required passband margin on either side of an APRS channel center.
pub(crate) const CHANNEL_MARGIN_HZ: u64 = 12_000;
const IQ_SAMPLE_BYTES: u64 = 4;
/// Original-rate samples for the shortest possible AX.25 packet and its flags.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    (frame::MIN_WIRE_BITS * SAMPLE_RATE as u64).div_ceil(afsk::SYMBOL_RATE as u64);

#[derive(Debug, Args)]
pub(crate) struct IdentifyAprs {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// APRS channel center in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// Actual acquisition rate in samples per second (2 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum number of original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    audio_sample_rate_hz: u32,
    timing_lanes: usize,
    pub(crate) validated_frames: usize,
    frames: Vec<Observation>,
}

#[derive(Serialize)]
struct Observation {
    decision_input_sample: u64,
    #[serde(flatten)]
    frame: frame::Frame,
}

/// RF labels and sample rate for one APRS channel in saved IQ.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Whether the RF channel is inside the supported sampled bandwidth.
    pub(crate) fn is_covered(self) -> bool {
        let offset_limit = u64::from(self.sample_rate_hz) * 2 / 5;
        self.center_hz.abs_diff(self.channel_hz) <= offset_limit.saturating_sub(CHANNEL_MARGIN_HZ)
    }

    /// Decode an admitted IQ prefix starting at sample zero.
    ///
    /// # Errors
    /// Returns an error for invalid RF labels or samples, input I/O or interruption.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut decoder = ChannelDecoder::new(self)?;
        let analyzed_samples = source_samples.min(sample_limit);
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |sample, index| {
                decoder.push(sample, index);
                Ok(())
            })?;
        Ok(Report {
            schema_version: 1,
            operation: "identify_aprs_in_saved_iq",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.center_hz,
            channel_frequency_hz: self.channel_hz,
            sample_rate_hz: self.sample_rate_hz,
            source_samples,
            analyzed_samples,
            analyzed_iq_sha256,
            audio_sample_rate_hz: afsk::AUDIO_SAMPLE_RATE_HZ,
            timing_lanes: afsk::LANES,
            validated_frames: decoder.frames.len(),
            frames: decoder.frames,
        })
    }
}

struct ChannelDecoder {
    bank: FixedBank,
    fm: NarrowFm,
    afsk: afsk::Demodulator,
    framing: [frame::Decoder; afsk::LANES],
    duplicate_samples: u64,
    frames: Vec<Observation>,
}

impl ChannelDecoder {
    fn new(channel: Channel) -> Result<Self> {
        let rate = channel.sample_rate_hz;
        ensure!(
            [TARGETED_RATE_HZ, SAMPLE_RATE].contains(&rate),
            "APRS replay requires 2 or 20 MSps IQ input"
        );
        let tolerance = crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
        ensure!(
            (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&channel.center_hz)
                && channel.is_covered(),
            "APRS channel requires {CHANNEL_MARGIN_HZ} Hz of margin inside the central 80% of the sampled band"
        );
        let offset_hz = channel.channel_hz as f64 - channel.center_hz as f64;
        Ok(Self {
            bank: FixedBank::new(&[dsp::tuning_word(offset_hz, rate)?])?,
            fm: NarrowFm::new(rate / CIC_DECIMATION as u32)?,
            afsk: afsk::Demodulator::default(),
            framing: std::array::from_fn(|_| frame::Decoder::default()),
            duplicate_samples: 2 * u64::from(rate).div_ceil(afsk::SYMBOL_RATE as u64),
            frames: Vec::new(),
        })
    }

    fn push(&mut self, input: Iq16, index: u64) {
        let Some(cic) = self.bank.push(input) else {
            return;
        };
        let Some(audio) = self.fm.push(cic.samples[0]) else {
            return;
        };
        for (framing, bit) in self.framing.iter_mut().zip(self.afsk.push(audio)) {
            let Some(frame) = bit.and_then(|bit| framing.push(bit)) else {
                continue;
            };
            // Timing lanes can finish within a few audio samples of each other.
            let duplicate = self
                .frames
                .iter()
                .rev()
                .take_while(|earlier| {
                    index - earlier.decision_input_sample <= self.duplicate_samples
                })
                .any(|earlier| earlier.frame.frame_hex == frame.frame_hex);
            if !duplicate {
                self.frames.push(Observation {
                    decision_input_sample: index,
                    frame,
                });
            }
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "APRS replay interrupted");
    Ok(())
}

/// Decode one labeled RF channel and publish its source-bound observations.
///
/// # Errors
/// Returns an error on invalid RF geometry, unsupported samples, changed IQ,
/// interruption, or failure to publish a fresh output file.
pub(crate) fn identify(root: &Path, args: IdentifyAprs, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let channel = Channel {
        center_hz: args.center_hz,
        channel_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
    };
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length == before.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
        "APRS input needs a stable extent of complete I/Q pairs"
    );
    let report = channel.replay(&mut input, length / IQ_SAMPLE_BYTES, args.max_samples, stop)?;
    private_output::publish_json_file(&output, &report, || {
        let path_stamp = source.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&before, &path_stamp)
                && file_pin::same_file(&before, &input.metadata()?),
            "APRS IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} AX.25 frames. Output: {}",
        report.validated_frames,
        output.display()
    );
    Ok(())
}
