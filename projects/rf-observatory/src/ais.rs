//! Identify stations in integrity-checked VHF AIS broadcasts from saved IQ.

mod demod;
mod frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq16, NarrowFskChannel};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// One-sided receive margin for a configured AIS channel.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = NarrowFskChannel::HALF_WIDTH_HZ;
/// Conservative 20 MSps prefix floor below a complete training and data burst.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = SAMPLE_RATE as u64 / 100;
/// Planned dwell with room for two nominal AIS slots and filter margins.
pub(crate) const FRAME_DWELL_MS: u64 = 60;
/// Default cap on detailed frame observations.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of detailed observations retained in one report.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode one AIS channel in a saved SC16 Q11 file.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal AIS channel in hertz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    channel_hz: u64,
    /// Original sample rate, either 96000 or 20000000 samples per second.
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Maximum number of verified observations to retain.
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

/// Checked AIS station observations bound to one channel and IQ prefix.
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
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Observation>,
}

impl Report {
    /// Count every validated reception including omitted details.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Count detailed observations retained in this report.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry and original sample rate for a single AIS channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Check sample rate, usable band edges, and the wideband center guard.
    pub(crate) fn is_covered(self) -> bool {
        let readbacks = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        readbacks.contains(&self.center_hz)
            && (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&self.channel_hz)
            && NarrowFskChannel::is_covered(
                self.sample_rate_hz,
                self.center_hz.abs_diff(self.channel_hz),
            )
    }

    /// Read a bounded prefix and count each verified frame.
    ///
    /// # Errors
    /// Rejects invalid geometry, incomplete or invalid IQ, and interruption.
    fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut stream = self.start(source_samples, sample_limit, max_frames)?;
        let digest =
            receiver::read_q11_prefix(input, stream.analyzed_samples, stop, |iq, index| {
                stream.push(iq, index);
                Ok(())
            })?;
        check_stop(stop)?;
        Ok(stream.finish(digest, max_frames))
    }

    /// Initialize one channel before consuming its admitted prefix.
    ///
    /// # Errors
    /// Rejects unsupported channel geometry or an excessive frame cap.
    pub(crate) fn start(
        self,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
    ) -> Result<Stream> {
        ensure!(
            self.is_covered(),
            "AIS needs a usable 96 kSps channel or a guarded 20 MSps channel"
        );
        ensure!(max_frames <= MAX_RETAINED_FRAMES, "invalid AIS frame cap");
        let channel = NarrowFskChannel::interpolated(
            self.sample_rate_hz,
            self.channel_hz as i64 - self.center_hz as i64,
        )?;
        Ok(Stream {
            geometry: self,
            decoder: demod::Decoder::new(channel.rate_hz()),
            stride: channel.stride(),
            delay: channel.delay_input_samples(),
            channel,
            source_samples,
            analyzed_samples: source_samples.min(sample_limit),
            max_frames,
            validated_frames: 0,
            frames: Vec::new(),
        })
    }
}

/// Incremental AIS decoding state for one bounded source prefix.
pub(crate) struct Stream {
    geometry: Channel,
    decoder: demod::Decoder,
    channel: NarrowFskChannel,
    stride: u64,
    delay: u64,
    source_samples: u64,
    analyzed_samples: u64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Stream {
    /// Visit the next validated IQ sample in source order.
    pub(crate) fn push(&mut self, sample: Iq16, index: u64) {
        let Some(sample) = self.channel.push(sample) else {
            return;
        };
        let input_sample = |index: u64| ((index + 1) * self.stride - 1).checked_sub(self.delay);
        self.decoder
            .push(sample, index / self.stride, |bytes, first, end| {
                if let Some(frame) = frame::decode(bytes)
                    && let Some(first_input_sample) = input_sample(first)
                    && let Some(end_input_sample_exclusive) = input_sample(end)
                    && first_input_sample < end_input_sample_exclusive
                    && end_input_sample_exclusive <= self.analyzed_samples
                {
                    self.validated_frames += 1;
                    if self.frames.len() < self.max_frames {
                        self.frames.push(Observation {
                            first_input_sample,
                            end_input_sample_exclusive,
                            frame,
                        });
                    }
                }
            });
    }

    /// Bind the IQ digest and apply the capture-wide detail limit.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String, remaining: usize) -> Report {
        self.frames.truncate(remaining);
        Report {
            schema_version: 1,
            operation: "identify_ais_in_saved_iq",
            identifier_scope: "ais_station_mmsi",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.geometry.center_hz,
            channel_frequency_hz: self.geometry.channel_hz,
            sample_rate_hz: self.geometry.sample_rate_hz,
            demodulated_sample_rate_hz: self.channel.rate_hz(),
            source_samples: self.source_samples,
            analyzed_samples: self.analyzed_samples,
            analyzed_iq_sha256,
            validated_frames: self.validated_frames,
            omitted_valid_frames: self.validated_frames - self.frames.len(),
            frames: self.frames,
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "AIS replay interrupted");
    Ok(())
}

/// Identify AIS stations and publish a fresh private report.
///
/// # Errors
/// Rejects invalid or changed source IQ, interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length > 0 && length == before.len() && length.is_multiple_of(IQ_BYTES),
        "AIS input needs complete SC16 Q11 I/Q pairs"
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
            "AIS IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} AIS frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
