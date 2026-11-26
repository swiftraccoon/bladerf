//! Replay integrity-checked Itron SCM and SCM+ identities from saved IQ.

mod demod;
mod frame;
mod legacy_frame;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq16, ScmChannel};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// One-sided receive margin around a selected Itron carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = ScmChannel::HALF_WIDTH_HZ;
/// Minimum 20 MSps replay window for one complete SCM+ frame.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = 160_000;
/// Minimum planned visit with room for a complete frame.
pub(crate) const FRAME_DWELL_MS: u64 = 16;
/// Minimum 20 MSps replay window for one complete legacy SCM frame.
pub(crate) const LEGACY_MIN_SWEEP_SAMPLES: u64 = 120_000;
/// Minimum planned visit with room for a complete legacy SCM frame.
pub(crate) const LEGACY_FRAME_DWELL_MS: u64 = 12;
/// Default number of retained frame observations.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of retained frame observations.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Select an Itron endpoint carrier in a saved SC16 Q11 file.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal endpoint carrier in hertz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    channel_hz: u64,
    /// Acquisition sample rate in samples per second.
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
    endpoint_id: u32,
    endpoint_type: u8,
    #[serde(flatten)]
    integrity: Integrity,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Integrity {
    Scm {
        bch16: u16,
        physical_tamper: u8,
        encoder_tamper: u8,
    },
    ScmPlus {
        crc16: u16,
    },
}

/// The selected Itron wire format and endpoint identifier namespace.
#[derive(Clone, Copy)]
pub(crate) enum Format {
    /// Legacy SCM with a 26-bit endpoint identifier and BCH check.
    Legacy,
    /// SCM+ with a 32-bit endpoint identifier and CRC check.
    Plus,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Legacy => "SCM",
            Self::Plus => "SCM+",
        }
    }

    fn observe(
        self,
        candidate: &demod::Candidate,
        first_input_sample: u64,
        end_input_sample_exclusive: u64,
    ) -> Option<Observation> {
        let (endpoint_id, endpoint_type, integrity) = match self {
            Self::Legacy => {
                let endpoint = legacy_frame::decode(candidate.bytes.first_chunk()?)?;
                (
                    endpoint.id,
                    endpoint.endpoint_type,
                    Integrity::Scm {
                        bch16: endpoint.crc,
                        physical_tamper: endpoint.physical_tamper,
                        encoder_tamper: endpoint.encoder_tamper,
                    },
                )
            }
            Self::Plus => {
                let endpoint = frame::decode(&candidate.bytes)?;
                (
                    endpoint.id,
                    endpoint.endpoint_type,
                    Integrity::ScmPlus {
                        crc16: endpoint.crc,
                    },
                )
            }
        };
        Some(Observation {
            first_input_sample,
            end_input_sample_exclusive,
            endpoint_id,
            endpoint_type,
            integrity,
        })
    }
}

/// Integrity-checked observations bound to a single channel and IQ prefix.
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
    /// Number of integrity-verified frame occurrences, including omitted details.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed frame observations retained.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry and original sample rate for one endpoint channel.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Check whether this channel fits the available acquisition passband.
    pub(crate) fn is_covered(self) -> bool {
        let readback_range = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        if !readback_range.contains(&self.center_hz)
            || !(MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&self.channel_hz)
        {
            return false;
        }
        let offset = self.center_hz.abs_diff(self.channel_hz);
        let rate = u64::from(self.sample_rate_hz);
        match self.sample_rate_hz {
            250_000 | 500_000 | 1_000_000 | 2_000_000 | 2_359_296 | 2_400_000 => {
                offset < rate / 2 - CHANNEL_HALF_WIDTH_HZ
            }
            SAMPLE_RATE => (CENTER_GUARD_HZ as u64 + CHANNEL_HALF_WIDTH_HZ
                ..=rate * 2 / 5 - CHANNEL_HALF_WIDTH_HZ)
                .contains(&offset),
            _ => false,
        }
    }

    /// Decode one bounded prefix. A zero retention limit keeps counts only.
    ///
    /// # Errors
    /// Rejects invalid RF geometry, malformed or incomplete IQ, or interruption.
    fn replay(
        self,
        format: Format,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut stream = self.start(format, source_samples, sample_limit, max_frames)?;
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, stream.analyzed_samples, stop, |sample, index| {
                stream.push(sample, index);
                Ok(())
            })?;
        check_stop(stop, format)?;
        Ok(stream.finish(analyzed_iq_sha256, max_frames))
    }

    /// Initialize the selected format before consuming its admitted IQ prefix.
    /// # Errors
    /// Rejects invalid coverage or retention limits.
    pub(crate) fn start(
        self,
        format: Format,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
    ) -> Result<Stream> {
        let name = format.name();
        ensure!(self.is_covered(), "{name} channel lacks usable IQ coverage");
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid {name} frame cap"
        );
        let channel = ScmChannel::new(
            self.sample_rate_hz,
            self.channel_hz as f64 - self.center_hz as f64,
        )?;
        Ok(Stream {
            geometry: self,
            format,
            detector: demod::Detector::new(channel.rate_hz(), format),
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

/// Incremental Itron decoding state for one admitted source prefix.
pub(crate) struct Stream {
    geometry: Channel,
    format: Format,
    detector: demod::Detector,
    stride: u64,
    delay: u64,
    channel: ScmChannel,
    source_samples: u64,
    analyzed_samples: u64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Stream {
    /// Visit the next validated sample in source order.
    pub(crate) fn push(&mut self, sample: Iq16, _index: u64) {
        let Some(iq) = self.channel.push(sample) else {
            return;
        };
        let input_sample = |index: u64| ((index + 1) * self.stride - 1).checked_sub(self.delay);
        self.detector.push(iq.power(), |candidate| {
            if let Some(first) = input_sample(candidate.first_sample)
                && let Some(end) = input_sample(candidate.end_sample_exclusive)
                && first < end
                && end <= self.analyzed_samples
                && let Some(observation) = self.format.observe(&candidate, first, end)
            {
                self.validated_frames += 1;
                if self.frames.len() < self.max_frames {
                    self.frames.push(observation);
                }
            }
        });
    }

    /// Bind the finished prefix and retain details allowed by the sweep cap.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String, retained_limit: usize) -> Report {
        self.frames.truncate(retained_limit);
        let (operation, identifier_scope) = match self.format {
            Format::Legacy => ("identify_scm_in_saved_iq", "scm_endpoint_id"),
            Format::Plus => ("identify_scm_plus_in_saved_iq", "scm_plus_endpoint_id"),
        };
        Report {
            schema_version: 1,
            operation,
            identifier_scope,
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

fn check_stop(stop: &AtomicBool, format: Format) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "{} replay interrupted",
        format.name()
    );
    Ok(())
}

/// Decode the selected Itron format and publish a fresh private report.
///
/// # Errors
/// Rejects invalid or changed IQ, interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, format: Format, stop: &AtomicBool) -> Result<()> {
    check_stop(stop, format)?;
    let name = format.name();
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length > 0 && length == before.len() && length.is_multiple_of(IQ_BYTES),
        "{name} input needs complete SC16 Q11 I/Q pairs"
    );
    let report = Channel {
        center_hz: args.center_hz,
        channel_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
    }
    .replay(
        format,
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
            "{name} IQ input changed after admission"
        );
        check_stop(stop, format)
    })?;
    println!(
        "Validated {} {name} frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
