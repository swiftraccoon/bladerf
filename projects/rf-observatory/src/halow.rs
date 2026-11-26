//! Identify 2 MHz Wi-Fi HaLow beacon addresses in saved IQ.

mod bits;
mod ofdm;
mod receive;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{HalowChannel, Iq16};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// One-sided margin around a selected HaLow carrier.
pub(crate) const CHANNEL_HALF_WIDTH_HZ: u64 = HalowChannel::HALF_WIDTH_HZ;
/// Smallest 20 MSps interval that fits a short preamble and one DATA symbol.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = 5_600;
/// Nominal dwell with room for a typical configured beacon interval.
pub(crate) const FRAME_DWELL_MS: u64 = 200;
/// Default cap on detailed frame observations.
pub(crate) const DEFAULT_RETAINED_FRAMES: usize = 256;
/// Maximum number of detailed observations retained in one report.
pub(crate) const MAX_RETAINED_FRAMES: usize = 4_096;
const IQ_BYTES: u64 = 4;

/// Decode HaLow frames on a selected channel in saved little-endian Q11 IQ.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved SC16 Q11 IQ file.
    input: PathBuf,
    /// Observed acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal HaLow channel center in hertz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: u64,
    /// Original sample rate of 2, 3.84, 4 or 20 MSps.
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
    ppdu_format: &'static str,
    mcs: u8,
    short_gi: bool,
    bssid: String,
    mpdu_bytes: usize,
    fcs: String,
}

/// Source-bound observations from one HaLow channel and IQ prefix.
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
    stf_candidates: usize,
    validated_sig_headers: usize,
    validated_mpdus: usize,
    skipped_candidates: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Observation>,
}

impl Report {
    /// Number of FCS-verified beacon occurrences.
    pub(crate) fn validated_frames(&self) -> usize {
        self.validated_frames
    }

    /// Number of detailed observations retained.
    pub(crate) fn retained_frames(&self) -> usize {
        self.frames.len()
    }
}

/// RF geometry for one HaLow channel in a saved capture.
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
            && HalowChannel::is_covered(
                self.sample_rate_hz,
                self.center_hz.abs_diff(self.channel_hz),
            )
    }

    /// Read one bounded prefix and count each verified beacon occurrence.
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
        let analyzed_iq_sha256 = receiver::read_q11_prefix(
            input,
            stream.observations.analyzed_samples,
            stop,
            |sample, index| {
                stream.push(sample, index);
                Ok(())
            },
        )?;
        check_stop(stop)?;
        let report = stream.finish(analyzed_iq_sha256, max_frames);
        check_stop(stop)?;
        Ok(report)
    }

    /// Initialize one channel before consuming its admitted IQ prefix.
    /// # Errors
    /// Rejects invalid coverage or retention limits.
    pub(crate) fn start(
        self,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
    ) -> Result<Stream> {
        ensure!(
            self.is_covered(),
            "HaLow channel lies outside the supported receive geometry"
        );
        ensure!(
            max_frames <= MAX_RETAINED_FRAMES,
            "invalid HaLow frame retention cap"
        );
        let offset_hz = self.channel_hz as i64 - self.center_hz as i64;
        let channel = HalowChannel::new(self.sample_rate_hz, offset_hz)?;
        let origin = channel.input_sample(0);
        let ratio = channel.input_sample(1) - origin;
        Ok(Stream {
            geometry: self,
            channel,
            decoder: receive::Receiver::new(),
            source_samples,
            observations: Observations {
                origin,
                ratio,
                analyzed_samples: source_samples.min(sample_limit),
                max_frames,
                validated_frames: 0,
                frames: Vec::new(),
            },
        })
    }
}

/// Incremental HaLow decoding state for one admitted source prefix.
pub(crate) struct Stream {
    geometry: Channel,
    channel: HalowChannel,
    decoder: receive::Receiver,
    source_samples: u64,
    observations: Observations,
}

impl Stream {
    /// Visit the next validated sample in source order.
    pub(crate) fn push(&mut self, sample: Iq16, _index: u64) {
        if let Some(iq) = self.channel.push(sample) {
            self.decoder
                .push(iq, &mut |frame| self.observations.record(frame));
        }
    }

    /// Flush ready candidates and bind the report to the validated prefix.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String, retained_limit: usize) -> Report {
        self.observations.max_frames = retained_limit.min(self.observations.max_frames);
        self.observations.frames.truncate(retained_limit);
        self.decoder
            .finish(&mut |frame| self.observations.record(frame));
        let observations = self.observations;
        let counts = self.decoder.counts;
        Report {
            schema_version: 1,
            operation: "identify_halow_in_saved_iq",
            identifier_scope: "s1g_basic_service_set",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.geometry.center_hz,
            channel_frequency_hz: self.geometry.channel_hz,
            sample_rate_hz: self.geometry.sample_rate_hz,
            demodulated_sample_rate_hz: HalowChannel::OUTPUT_RATE_HZ,
            source_samples: self.source_samples,
            analyzed_samples: observations.analyzed_samples,
            analyzed_iq_sha256,
            stf_candidates: counts.stf_candidates,
            validated_sig_headers: counts.validated_sig_headers,
            validated_mpdus: counts.validated_mpdus,
            skipped_candidates: counts.skipped_candidates,
            validated_frames: observations.validated_frames,
            omitted_valid_frames: observations.validated_frames - observations.frames.len(),
            frames: observations.frames,
        }
    }
}

struct Observations {
    origin: f64,
    ratio: f64,
    analyzed_samples: u64,
    max_frames: usize,
    validated_frames: usize,
    frames: Vec<Observation>,
}

impl Observations {
    fn record(&mut self, frame: receive::Beacon) {
        let input_sample = |index: u64| (self.origin + index as f64 * self.ratio).round();
        let first = input_sample(frame.first_sample);
        let end = input_sample(frame.end_sample_exclusive);
        if first >= 0.0 && end <= self.analyzed_samples as f64 {
            self.validated_frames += 1;
            if self.frames.len() < self.max_frames {
                let [a, b, c, d, e, f] = frame.bssid;
                self.frames.push(Observation {
                    first_input_sample: first as u64,
                    end_input_sample_exclusive: end as u64,
                    estimated_carrier_offset_hz: frame.carrier_offset_hz.round() as i32,
                    ppdu_format: match frame.format {
                        ofdm::Format::Short => "s1g_short",
                        ofdm::Format::Long => "s1g_long",
                    },
                    mcs: frame.mcs,
                    short_gi: frame.short_gi,
                    bssid: format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}"),
                    mpdu_bytes: frame.mpdu_bytes,
                    fcs: format!("{:08x}", frame.fcs),
                });
            }
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "HaLow replay interrupted");
    Ok(())
}

/// Decode saved IQ and privately publish its source-bound HaLow report.
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
        "HaLow input needs complete SC16 Q11 I/Q pairs"
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
            "HaLow IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} HaLow beacons. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
