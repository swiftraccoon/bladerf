//! Identify the serial and frame number in Vaisala RS41 weather telemetry.

mod demod;
mod fec;
mod frame;
pub(crate) mod raster;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::NarrowFskChannel;
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// One-sided margin around a selected RS41 carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = NarrowFskChannel::HALF_WIDTH_HZ;
/// Smallest saved 20 MSps interval that fits a standard frame.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    (SAMPLE_RATE as u64 * frame::STANDARD_FRAME_BYTES as u64 * 8).div_ceil(demod::BAUD);
/// Nominal dwell with room for one extended frame.
pub(crate) const FRAME_DWELL_MS: u64 = 900;
/// Default cap on detailed frame observations.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of detailed observations retained in one report.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode RS41 frames on a selected channel in saved little-endian Q11 IQ.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved SC16 Q11 IQ file.
    input: PathBuf,
    /// Observed acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal radiosonde carrier in hertz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Original sample rate, either 96000 or 20000000 samples per second.
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
    frame_bytes: usize,
    corrected_bytes: usize,
    #[serde(flatten)]
    status: frame::Status,
}

/// Source-bound observations from one RS41 channel and IQ prefix.
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
    /// Number of parity- and STATUS-checked physical frames.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed observations retained.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry for one RS41 channel in a saved capture.
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
            && NarrowFskChannel::is_covered(
                self.sample_rate_hz,
                self.center_hz.abs_diff(self.channel_hz),
            )
    }

    /// Read one bounded prefix and count each verified STATUS occurrence.
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
        ensure!(
            self.is_covered(),
            "RS41 channel lies outside the supported receive geometry"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid RS41 frame retention cap"
        );
        let offset_hz = self.channel_hz as i64 - self.center_hz as i64;
        let mut channel = NarrowFskChannel::new(self.sample_rate_hz, offset_hz)?;
        let rate = channel.rate_hz();
        let stride = channel.stride();
        let mut decoder = demod::Decoder::new(rate);
        let delay = channel.delay_input_samples() + decoder.delay_samples() * stride;
        let analyzed_samples = source_samples.min(sample_limit);
        let input_sample = |index: u64| {
            ((index + 1) * stride - 1)
                .saturating_sub(delay)
                .min(analyzed_samples)
        };
        let mut validated_frames = 0;
        let mut frames = Vec::new();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |x, n| {
                if let Some(iq) = channel.push(x)
                    && let Some(frame) = decoder.push(iq, n / stride)
                {
                    validated_frames += 1;
                    if frames.len() < max_frames {
                        frames.push(Observation {
                            first_input_sample: input_sample(frame.first_sample),
                            end_input_sample_exclusive: input_sample(frame.end_sample_exclusive),
                            frame_bytes: frame.frame_bytes,
                            corrected_bytes: frame.corrected_bytes,
                            status: frame.status,
                        });
                    }
                }
                Ok(())
            })?;
        check_stop(stop)?;
        Ok(Report {
            schema_version: 1,
            operation: "identify_rs41_in_saved_iq",
            identifier_scope: "rs41_status_serial",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.center_hz,
            channel_frequency_hz: self.channel_hz,
            sample_rate_hz: self.sample_rate_hz,
            demodulated_sample_rate_hz: rate,
            source_samples,
            analyzed_samples,
            analyzed_iq_sha256,
            sync_candidates: decoder.synchronizations(),
            validated_frames,
            omitted_valid_frames: validated_frames - frames.len(),
            frames,
        })
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "RS41 replay interrupted");
    Ok(())
}

/// Decode saved IQ and privately publish its source-bound RS41 report.
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
        "RS41 input needs complete SC16 Q11 I/Q pairs"
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
            "RS41 IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} RS41 frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
