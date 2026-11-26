//! Identify conventional DECT radio fixed parts in saved IQ.

mod demod;
mod frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{DectChannel, Iq16};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// One-sided margin around a selected DECT carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = DectChannel::HALF_WIDTH_HZ;
/// Smallest 20 MSps interval that fits a conventional S and A field.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    (SAMPLE_RATE as u64 * frame::PREFIX_BITS as u64).div_ceil(demod::BAUD);
/// Nominal dwell with room for one conventional identity multiframe.
pub(crate) const FRAME_DWELL_MS: u64 = 200;
/// Default cap on detailed frame observations.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of detailed observations retained in one report.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode DECT frames on a selected channel in saved little-endian Q11 IQ.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved SC16 Q11 IQ file.
    input: PathBuf,
    /// Observed acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal DECT channel center in hertz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Original sample rate, either 4608000 or 20000000 samples per second.
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Number of verified frame observations to retain.
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
    estimated_carrier_offset_hz: i32,
    spectrum_inverted: bool,
    rfpi: String,
    a_field_header: u8,
}

/// Source-bound observations from one DECT channel and IQ prefix.
#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    operation: &'static str,
    identifier_scope: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    demodulated_sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    sync_candidates: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Observation>,
}

impl Report {
    /// Number of integrity-checked physical bursts.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed observations retained.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry for one DECT channel in a saved capture.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Check sample rate, usable band edges and the wideband center guard.
    pub(crate) fn is_covered(self) -> bool {
        let readback_range = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        readback_range.contains(&self.center_hz)
            && self.channel_hz != 0
            && DectChannel::is_covered(
                self.sample_rate_hz,
                self.center_hz.abs_diff(self.channel_hz),
            )
    }

    /// Read one bounded prefix and count each verified identity prefix.
    /// A zero retention limit keeps aggregate evidence only.
    ///
    /// # Errors
    /// Rejects invalid geometry or IQ, incomplete reads, or interruption.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut stream = self.start(source_samples, sample_limit, max_frames)?;
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, stream.analyzed_samples, stop, |sample, index| {
                stream.push(sample, index);
                Ok(())
            })?;
        check_stop(stop)?;
        Ok(stream.finish(analyzed_iq_sha256, max_frames))
    }

    /// Start decoding one channel from a bounded prefix at sample zero.
    ///
    /// # Errors
    /// Rejects invalid channel geometry or frame retention limits.
    pub(crate) fn start(
        self,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
    ) -> Result<Stream> {
        ensure!(
            self.is_covered(),
            "DECT channel lies outside the supported receive geometry"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid DECT frame retention cap"
        );
        let offset_hz = self.channel_hz as i64 - self.center_hz as i64;
        let channel = DectChannel::new(self.sample_rate_hz, offset_hz)?;
        let stride = channel.stride();
        let decoder = demod::Decoder::new(channel.rate_hz());
        let delay = channel.delay_input_samples() as f64
            + demod::DISCRIMINATOR_DELAY_SAMPLES * stride as f64;
        Ok(Stream {
            geometry: self,
            source_samples,
            analyzed_samples: source_samples.min(sample_limit),
            channel,
            decoder,
            stride,
            delay,
            max_frames,
            validated_frames: 0,
            frames: Vec::new(),
        })
    }
}

/// DECT demodulator and retained observations for one replay prefix.
pub(crate) struct Stream {
    geometry: Channel,
    source_samples: u64,
    analyzed_samples: u64,
    channel: DectChannel,
    decoder: demod::Decoder,
    stride: u64,
    delay: f64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Stream {
    /// Feed the next validated original-rate sample and its index in the prefix.
    pub(crate) fn push(&mut self, sample: Iq16, index: u64) {
        if let Some(iq) = self.channel.push(sample)
            && let Some(frame) = self.decoder.push(iq, index / self.stride)
        {
            let input_sample =
                |index: f64| ((index + 1.0) * self.stride as f64 - 1.0 - self.delay).round();
            let first = input_sample(frame.first_sample);
            let end = input_sample(frame.end_sample_exclusive);
            if first >= 0.0 && end <= self.analyzed_samples as f64 {
                self.validated_frames += 1;
                if self.frames.len() < self.max_frames {
                    self.frames.push(Observation {
                        first_input_sample: first as u64,
                        end_input_sample_exclusive: end as u64,
                        estimated_carrier_offset_hz: frame.carrier_offset_hz.round() as i32,
                        spectrum_inverted: frame.spectrum_inverted,
                        rfpi: format!("{:010x}", frame.identity.rfpi),
                        a_field_header: frame.identity.a_field_header,
                    });
                }
            }
        }
    }

    /// Bind the consumed IQ prefix and apply the remaining report retention limit.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String, retained_limit: usize) -> Report {
        self.frames.truncate(retained_limit);
        Report {
            schema_version: 1,
            operation: "identify_dect_in_saved_iq",
            identifier_scope: "dect_radio_fixed_part",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.geometry.center_hz,
            channel_frequency_hz: self.geometry.channel_hz,
            sample_rate_hz: self.geometry.sample_rate_hz,
            demodulated_sample_rate_hz: self.channel.rate_hz(),
            source_samples: self.source_samples,
            analyzed_samples: self.analyzed_samples,
            analyzed_iq_sha256,
            sync_candidates: self.decoder.synchronizations(),
            validated_frames: self.validated_frames,
            omitted_valid_frames: self.validated_frames - self.frames.len(),
            frames: self.frames,
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "DECT replay interrupted");
    Ok(())
}

/// Decode saved IQ and privately publish its source-bound DECT report.
///
/// # Errors
/// Rejects invalid or changed input, interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length > 0 && length == before.len() && length.is_multiple_of(IQ_BYTES),
        "DECT input needs complete SC16 Q11 I/Q pairs"
    );
    let report = Channel {
        center_hz: args.center_hz,
        channel_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
    }
    .replay(
        &mut input,
        length / IQ_BYTES,
        args.max_samples,
        args.max_frames,
        stop,
    )?;
    private_output::publish_json_file(&output, &report, || {
        let path_stamp = source.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&before, &path_stamp)
                && file_pin::same_file(&before, &input.metadata()?),
            "DECT IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} DECT frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
