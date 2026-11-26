//! Identify Efergy e2-compatible transmitters in saved RF samples.

mod demod;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq16, SensorChannel};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// Initial receive channel for the e2 Classic FSK format.
pub(crate) const DEFAULT_CHANNEL_HZ: u64 = 433_550_000;
/// One-sided receive margin around the selected carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = SensorChannel::HALF_WIDTH_HZ;
/// Conservative 20 MSps prefix floor below the shortest admitted sync and body.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = SAMPLE_RATE as u64 / 100;
/// Planned capture duration for one frame, including burst boundaries.
pub(crate) const FRAME_DWELL_MS: u64 = 25;
/// Default cap on retained frame details per report.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode one channel in a saved SC16 Q11 file.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal Efergy channel in hertz.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_HZ, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Actual acquisition rate in samples per second (0.25, 0.5, 1, 2 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Maximum number of verified frames to retain.
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
    frame: demod::Frame,
}

/// Checked e2 transmitter frames and the IQ prefix that supplied them.
#[derive(Serialize)]
pub(crate) struct Report {
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
    frames: Vec<Observation>,
}

impl Report {
    /// Count checked occurrences including those omitted by the detail cap.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Count the retained frame details.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF and sample-rate labels for an Efergy replay channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Whether the channel fits the labeled passband and clears the sweep DC guard.
    pub(crate) fn is_covered(self) -> bool {
        let readbacks = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        if !readbacks.contains(&self.center_hz) || self.channel_hz == 0 {
            return false;
        }
        let offset = self.center_hz.abs_diff(self.channel_hz);
        let outer = u64::from(self.sample_rate_hz) * 2 / 5;
        match self.sample_rate_hz {
            250_000 | 500_000 | 1_000_000 | 2_000_000 => offset <= outer - CHANNEL_HALF_WIDTH_HZ,
            SAMPLE_RATE => (CENTER_GUARD_HZ as u64 + CHANNEL_HALF_WIDTH_HZ
                ..=outer - CHANNEL_HALF_WIDTH_HZ)
                .contains(&offset),
            _ => false,
        }
    }

    /// Decode one bounded prefix. A zero detail limit still counts checked frames.
    ///
    /// # Errors
    /// Rejects invalid geometry, failed or interrupted IQ reads, and excessive retention.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut stream = self.start(source_samples, sample_limit, max_frames)?;
        let digest = receiver::read_q11_prefix(
            input,
            stream.observations.analyzed_samples,
            stop,
            |iq, index| {
                stream.push(iq, index);
                Ok(())
            },
        )?;
        check_stop(stop)?;
        Ok(stream.finish(digest))
    }

    /// Initialize decoding for the admitted prefix at sample zero.
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
            "Efergy needs a usable channel in 0.25, 0.5, 1 or 2 MSps IQ or a guarded 20 MSps dwell"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid Efergy frame cap"
        );
        let channel = SensorChannel::new(
            self.sample_rate_hz,
            self.channel_hz as f64 - self.center_hz as f64,
        )?;
        let rate_hz = self.sample_rate_hz.min(SensorChannel::OUTPUT_RATE_HZ);
        Ok(Stream {
            geometry: self,
            source_samples,
            rate_hz,
            observations: Observations {
                stride: u64::from(self.sample_rate_hz / rate_hz),
                delay: channel.delay_input_samples(),
                analyzed_samples: source_samples.min(sample_limit),
                max_frames,
                validated_frames: 0,
                frames: Vec::new(),
            },
            channel,
            decoder: demod::Decoder::new(rate_hz),
        })
    }
}

struct Observations {
    stride: u64,
    delay: u64,
    analyzed_samples: u64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Observations {
    fn record(&mut self, frame: demod::Frame, first: u64, end: u64) {
        let input_sample = |index: u64| {
            ((index + 1) * self.stride - 1)
                .saturating_sub(self.delay)
                .min(self.analyzed_samples)
        };
        let first_input_sample = input_sample(first);
        let end_input_sample_exclusive = input_sample(end);
        if first_input_sample < end_input_sample_exclusive {
            self.validated_frames += 1;
            if self.frames.len() < self.max_frames {
                self.frames.push(Observation {
                    first_input_sample,
                    end_input_sample_exclusive,
                    frame,
                });
            }
        }
    }
}

/// Incremental Efergy decoding state for one bounded source prefix.
pub(crate) struct Stream {
    geometry: Channel,
    source_samples: u64,
    rate_hz: u32,
    channel: SensorChannel,
    decoder: demod::Decoder,
    observations: Observations,
}

impl Stream {
    /// Advance the next validated input sample in source order.
    pub(crate) fn push(&mut self, sample: Iq16, index: u64) {
        if let Some(iq) = self.channel.push(sample) {
            let observations = &mut self.observations;
            self.decoder
                .push(iq, index / observations.stride, |frame, first, end| {
                    observations.record(frame, first, end);
                });
        }
    }

    /// Flush the last measured burst under the existing cap and bind the IQ digest.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String) -> Report {
        let observations = &mut self.observations;
        self.decoder.finish(|frame, first, end| {
            observations.record(frame, first, end);
        });
        let Observations {
            analyzed_samples,
            validated_frames,
            frames,
            ..
        } = self.observations;
        Report {
            schema_version: 1,
            operation: "identify_efergy_in_saved_iq",
            model: "efergy_e2_classic",
            identifier_scope: "protocol_transmitter_id",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.geometry.center_hz,
            channel_frequency_hz: self.geometry.channel_hz,
            sample_rate_hz: self.geometry.sample_rate_hz,
            demodulated_sample_rate_hz: self.rate_hz,
            source_samples: self.source_samples,
            analyzed_samples,
            analyzed_iq_sha256,
            validated_frames,
            omitted_valid_frames: validated_frames - frames.len(),
            frames,
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "Efergy replay interrupted");
    Ok(())
}

/// Identify transmitters and publish a fresh private report.
///
/// # Errors
/// Rejects invalid or changed source IQ, interruption and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length > 0 && length == before.len() && length.is_multiple_of(IQ_BYTES),
        "Efergy input needs complete SC16 Q11 I/Q pairs"
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
            "Efergy IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} Efergy frames. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
