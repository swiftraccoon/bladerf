//! Shared Ecowitt FSK channel geometry, packet replay, and source receipts.

mod demod;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::SensorChannel;
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// Nominal center in hertz for the supported North American sensors.
pub(crate) const DEFAULT_CHANNEL_HZ: u64 = 915_000_000;
/// Default cap on detailed records per sensor replay report.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode one selected sensor format in saved SC16 Q11 IQ.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal sensor channel in hertz.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_HZ)]
    channel_hz: u64,
    /// Actual acquisition rate in samples per second (1, 2 or 20 MSps).
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

/// Parse a nonzero cap on detailed sensor frame records.
///
/// # Errors
/// Rejects malformed, zero and excessive counts.
fn parse_retention(value: &str) -> Result<usize> {
    let count = value.parse()?;
    ensure!(
        (1..=MAX_RETAINED_FRAMES).contains(&count),
        "retain 1..={MAX_RETAINED_FRAMES} sensor frames"
    );
    Ok(count)
}

/// Framing parameters and validation for one supported sensor payload.
pub(crate) trait FrameFormat<const N: usize>: Copy {
    const LABEL: &'static str;
    const MODEL: &'static str;
    const OPERATION: &'static str;
    const SYNC_WORD: u32;
    const SYNC_BITS: u32;
    const FAMILY: u8;

    /// Validate a complete body and decode it when its integrity checks pass.
    fn decode(payload: &[u8; N]) -> Option<Self>;
}

/// Original-rate floor for sync and payload at the shortest accepted bit period.
pub(crate) const fn min_sweep_samples<F: FrameFormat<N>, const N: usize>() -> u64 {
    (F::SYNC_BITS as u64 + N as u64 * 8)
        * demod::MIN_BIT_SAMPLES as u64
        * (SAMPLE_RATE / SensorChannel::OUTPUT_RATE_HZ) as u64
}

#[derive(Serialize)]
struct Observation<F> {
    first_input_sample: u64,
    end_input_sample_exclusive: u64,
    #[serde(flatten)]
    frame: F,
}

/// Source-bound sensor frame observations from a single channel and IQ prefix.
#[derive(Serialize)]
pub(crate) struct Report<F> {
    schema_version: u32,
    operation: &'static str,
    model: &'static str,
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
    frames: Vec<Observation<F>>,
}

impl<F> Report<F> {
    /// Number of integrity-checked frame occurrences, including omitted details.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed frame observations retained in this report.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry for one saved complex sensor channel.
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
            1_000_000 | 2_000_000 => offset <= outer - width,
            SAMPLE_RATE => (CENTER_GUARD_HZ as u64 + width..=outer - width).contains(&offset),
            _ => false,
        }
    }

    /// Decode one admitted prefix. Every verified physical frame counts,
    /// including separate retransmissions of identical payloads.
    ///
    /// # Errors
    /// Returns an error for interrupted reads, invalid IQ or RF geometry.
    pub(crate) fn replay<F: FrameFormat<N>, const N: usize>(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report<F>> {
        ensure!(
            self.is_covered(),
            "{} needs a usable channel in 1 or 2 MSps IQ or a guarded 20 MSps dwell",
            F::LABEL
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid {} frame cap",
            F::LABEL
        );
        let mut channel = SensorChannel::new(
            self.sample_rate_hz,
            self.channel_hz as f64 - self.center_hz as f64,
        )?;
        let rate = SensorChannel::OUTPUT_RATE_HZ;
        let stride = u64::from(self.sample_rate_hz / rate);
        let delay = channel.delay_input_samples();
        let analyzed_samples = source_samples.min(sample_limit);
        let input_sample = |index: u64| {
            ((index + 1) * stride - 1)
                .saturating_sub(delay)
                .min(analyzed_samples)
        };
        let mut decoder = demod::Decoder::<F, N>::new();
        let mut validated_frames = 0;
        let mut frames = Vec::new();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |x, n| {
                if let Some(iq) = channel.push(x)
                    && let Some(packet) = decoder.push(iq, n / stride)
                {
                    validated_frames += 1;
                    if frames.len() < max_frames {
                        frames.push(Observation {
                            first_input_sample: input_sample(packet.first_sample),
                            end_input_sample_exclusive: input_sample(packet.end_sample_exclusive),
                            frame: packet.frame,
                        });
                    }
                }
                Ok(())
            })?;
        check_stop(stop, F::LABEL)?;
        Ok(Report {
            schema_version: 1,
            operation: F::OPERATION,
            model: F::MODEL,
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

fn check_stop(stop: &AtomicBool, label: &str) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "{label} replay interrupted");
    Ok(())
}

/// Identify frames and publish a fresh source-bound report.
///
/// # Errors
/// Returns an error for invalid or changed IQ, interruption or output failure.
pub(crate) fn identify<F: FrameFormat<N> + Serialize, const N: usize>(
    root: &Path,
    args: Args,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop, F::LABEL)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length == before.len() && length.is_multiple_of(IQ_BYTES),
        "{} input needs complete SC16 Q11 I/Q pairs",
        F::LABEL
    );
    let report = Channel {
        center_hz: args.center_hz,
        channel_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
    }
    .replay::<F, N>(
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
            "{} IQ input changed after admission",
            F::LABEL
        );
        check_stop(stop, F::LABEL)
    })?;
    println!(
        "Validated {} {} frames. Output: {}",
        report.validated_frames(),
        F::LABEL,
        output.display()
    );
    Ok(())
}

/// Compute the MSB-first Ecowitt CRC-8 with polynomial 0x31 and initial zero.
pub(crate) fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            let feedback = if crc & 0x80 != 0 { 0x31 } else { 0 };
            crc = (crc << 1) ^ feedback;
        }
    }
    crc
}
