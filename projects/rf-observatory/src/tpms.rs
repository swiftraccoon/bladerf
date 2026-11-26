//! Identify supported Pacific TPMS frames in saved RF samples.

mod demod;
mod frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{SensorChannel, fsk::Discriminator};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// Default receive channel for the PMV-107J format.
pub(crate) const DEFAULT_CHANNEL_HZ: u64 = 315_000_000;
/// Conservative sweep floor below the shortest admitted C210 frame.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = SAMPLE_RATE as u64 * 6 / 1_000;
/// Planned dwell in milliseconds for a complete nominal preamble and frame.
pub(crate) const FRAME_DWELL_MS: u64 = 16;
/// Default cap on detailed records in a TPMS replay report.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode supported TPMS formats on a labeled channel in saved SC16 Q11 IQ.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal TPMS channel in hertz.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_HZ, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Actual acquisition rate in samples per second (0.25, 0.5, 1, 2 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Maximum number of verified frames to retain.
    #[arg(long, default_value_t = DEFAULT_RETAINED_FRAMES, value_parser = parse_retention)]
    max_frames: usize,
    /// New private JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// Parse a nonzero cap on detailed TPMS frame records.
///
/// # Errors
/// Rejects malformed, zero and excessive counts.
pub(crate) fn parse_retention(value: &str) -> Result<usize> {
    let count = value.parse()?;
    ensure!(
        (1..=MAX_RETAINED_FRAMES).contains(&count),
        "retain 1..={MAX_RETAINED_FRAMES} TPMS frames"
    );
    Ok(count)
}

#[derive(Serialize)]
struct Observation {
    first_input_sample: u64,
    end_input_sample_exclusive: u64,
    #[serde(flatten)]
    frame: frame::Frame,
}

/// Source-bound TPMS frame observations from a single channel and IQ prefix.
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
    /// Number of integrity-checked frame occurrences, including omitted details.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed frame observations retained in this report.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry for one saved complex TPMS channel.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Whether the filtered channel fits the actual acquisition readback.
    /// Full-width sweeps leave the DC guard clear. Narrow recordings may
    /// already be centered on the sensor channel.
    pub(crate) fn is_covered(self) -> bool {
        let readback_range = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        if !readback_range.contains(&self.center_hz) || self.channel_hz == 0 {
            return false;
        }
        let width = SensorChannel::HALF_WIDTH_HZ;
        let offset = self.center_hz.abs_diff(self.channel_hz);
        let outer = u64::from(self.sample_rate_hz) * 2 / 5;
        match self.sample_rate_hz {
            250_000 | 500_000 | 1_000_000 | 2_000_000 => offset <= outer - width,
            SAMPLE_RATE => (CENTER_GUARD_HZ as u64 + width..=outer - width).contains(&offset),
            _ => false,
        }
    }

    /// Decode one admitted prefix. Every verified physical frame counts,
    /// including separate retransmissions of identical payloads.
    ///
    /// # Errors
    /// Returns an error for interrupted reads, invalid IQ or RF geometry.
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
            "TPMS needs a usable channel in 0.25, 0.5, 1 or 2 MSps IQ or a guarded 20 MSps dwell"
        );
        ensure!(max_frames <= MAX_RETAINED_FRAMES, "invalid TPMS frame cap");
        let mut channel = SensorChannel::new(
            self.sample_rate_hz,
            self.channel_hz as f64 - self.center_hz as f64,
        )?;
        let rate = self.sample_rate_hz.min(SensorChannel::OUTPUT_RATE_HZ);
        let stride = u64::from(self.sample_rate_hz / rate);
        let mut discriminator = Discriminator::new(rate);
        let delay = channel.delay_input_samples() + discriminator.delay_samples() * stride;
        let analyzed_samples = source_samples.min(sample_limit);
        let input_sample = |index: u64| {
            ((index + 1) * stride - 1)
                .saturating_sub(delay)
                .min(analyzed_samples)
        };
        let mut decoder = demod::Decoder::new(rate);
        let mut validated_frames = 0;
        let mut frames = Vec::new();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |x, n| {
                if let Some(iq) = channel.push(x) {
                    let Some(angle) = discriminator.push(iq) else {
                        decoder.reset();
                        return Ok(());
                    };
                    if let Some(packet) = decoder.push(angle, n / stride) {
                        validated_frames += 1;
                        if frames.len() < max_frames {
                            frames.push(Observation {
                                first_input_sample: input_sample(packet.first_sample),
                                end_input_sample_exclusive: input_sample(
                                    packet.end_sample_exclusive,
                                ),
                                frame: packet.frame,
                            });
                        }
                    }
                }
                Ok(())
            })?;
        check_stop(stop)?;
        Ok(Report {
            schema_version: 1,
            operation: "identify_tpms_in_saved_iq",
            identifier_scope: "protocol_sensor_id",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.center_hz,
            channel_frequency_hz: self.channel_hz,
            sample_rate_hz: self.sample_rate_hz,
            demodulated_sample_rate_hz: rate,
            source_samples,
            analyzed_samples,
            analyzed_iq_sha256,
            validated_frames,
            omitted_valid_frames: validated_frames - frames.len(),
            frames,
        })
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "TPMS replay interrupted");
    Ok(())
}

/// Identify TPMS-format frames and publish a fresh source-bound report.
///
/// # Errors
/// Returns an error for invalid or changed IQ, interruption or output failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length == before.len() && length.is_multiple_of(IQ_BYTES),
        "TPMS input needs complete SC16 Q11 I/Q pairs"
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
            "TPMS IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} TPMS frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
