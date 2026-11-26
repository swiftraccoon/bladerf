//! Offline LoRa preamble evidence from one labeled channel in saved IQ.

pub(crate) mod bank;
pub(crate) mod decode;
mod preamble;

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::Args;
use rustfft::num_complex::Complex32;
use serde::Serialize;

use crate::dsp::ChirpChannel;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

const IQ_SAMPLE_BYTES: u64 = 4;
/// Maximum number of preamble candidate records retained in a probe report.
pub(crate) const MAX_DETECTIONS: usize = 1_024;

#[derive(Debug, Args)]
pub(crate) struct ProbeLoRa {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    #[command(flatten)]
    channel: ChannelArgs,
    /// Maximum number of original-rate samples to inspect from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct ChannelArgs {
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Candidate LoRa channel center in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// Actual acquisition rate in complex samples per second (2, 4 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// LoRa bandwidth in hertz (125, 250 or 500 kHz).
    #[arg(long)]
    bandwidth_hz: u32,
    /// Spreading factor from 7 through 12.
    #[arg(long, value_parser = clap::value_parser!(u8).range(7..=12))]
    spreading_factor: u8,
}

impl ChannelArgs {
    fn offset_hz(&self) -> Result<f64> {
        let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
        ensure!(
            (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&self.center_hz)
                && self.center_hz.abs_diff(self.channel_hz)
                    <= (u64::from(self.sample_rate_hz) * 2 / 5)
                        .saturating_sub(u64::from(self.bandwidth_hz) / 2),
            "LoRa channel and its bandwidth must fit inside the central 80% of the sampled band"
        );
        Ok(self.channel_hz as f64 - self.center_hz as f64)
    }
}

struct Capture {
    path: PathBuf,
    input: File,
    stamp: Metadata,
    samples: u64,
}

impl Capture {
    fn open(path: PathBuf) -> Result<Self> {
        let (input, length) = file_pin::open_regular(&path, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
        let stamp = input.metadata()?;
        ensure!(
            length == stamp.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
            "LoRa input needs a stable extent of complete I/Q pairs"
        );
        Ok(Self {
            path,
            input,
            stamp,
            samples: length / IQ_SAMPLE_BYTES,
        })
    }

    fn verify(&self) -> Result<()> {
        let path_stamp = self.path.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&self.stamp, &path_stamp)
                && file_pin::same_file(&self.stamp, &self.input.metadata()?),
            "LoRa IQ input changed after admission"
        );
        Ok(())
    }
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    bandwidth_hz: u32,
    spreading_factor: u8,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    #[serde(flatten)]
    evidence: bank::ProbeEvidence,
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "LoRa replay interrupted");
    Ok(())
}

/// Search a labeled channel for repeated upchirps followed by a downchirp.
///
/// # Errors
/// Returns an error for unsupported RF geometry, invalid or changed IQ,
/// interruption, or failure to publish a fresh report.
pub(crate) fn probe(root: &Path, args: ProbeLoRa, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let settings = args.channel;
    let mut channel = ChirpChannel::new(
        settings.sample_rate_hz,
        settings.offset_hz()?,
        settings.bandwidth_hz,
    )?;
    let mut detector = bank::ProfileProbe::new(settings.spreading_factor);
    let output = root.join(args.output);
    let mut capture = Capture::open(root.join(args.input))?;
    let source_samples = capture.samples;
    let analyzed_samples = source_samples.min(args.max_samples);
    let mut remaining_candidates = MAX_DETECTIONS;
    let analyzed_iq_sha256 =
        receiver::read_q11_prefix(&mut capture.input, analyzed_samples, stop, |sample, _| {
            if let Some(chip) = channel.push(sample) {
                detector.push(
                    Complex32::new(chip.i as f32, chip.q as f32),
                    &mut remaining_candidates,
                );
            }
            Ok(())
        })?;
    let evidence = detector.finish(&channel);
    let candidate_count = evidence.candidate_count();
    let report = Report {
        schema_version: 1,
        operation: "probe_lora_preamble",
        sample_format: "SC16_Q11_LE",
        center_frequency_hz: settings.center_hz,
        channel_frequency_hz: settings.channel_hz,
        sample_rate_hz: settings.sample_rate_hz,
        bandwidth_hz: settings.bandwidth_hz,
        spreading_factor: settings.spreading_factor,
        source_samples,
        analyzed_samples,
        analyzed_iq_sha256,
        evidence,
    };
    private_output::publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Found {candidate_count} LoRa preamble candidates. Output: {}",
        output.display()
    );
    Ok(())
}
