//! Recover evidence of 1090 MHz extended squitters from saved SC16 Q11 IQ.

mod demod;
mod frame;

use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{AdsbChannel, Iq};
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

use self::demod::{PREAMBLE_SAMPLES, SAMPLES_PER_BIT};

/// RF center in hertz of the extended squitter channel.
pub(crate) const CHANNEL_HZ: u64 = 1_090_000_000;
/// Native sweep samples needed for one nominal frame and filter delay.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = (demod::FRAME_SAMPLES
    + AdsbChannel::FILTER_DELAY_OUTPUT_SAMPLES as u64)
    * (SAMPLE_RATE / AdsbChannel::OUTPUT_RATE_HZ) as u64;
/// Maximum number of frame records retained in a report.
pub(crate) const MAX_FRAME_RECORDS: usize = 4_096;
const IQ_SAMPLE_BYTES: u64 = 4;
const IQ_HISTORY_SAMPLES: usize = 512;
const DUPLICATE_WINDOW_SAMPLES: u64 = SAMPLES_PER_BIT;
const CARRIER_LIMIT_HZ: f64 = 1_100_000.0;
const MIN_CARRIER_COHERENCE: f64 = 0.55;

/// Select the source geometry and prefix for 1090 MHz replay.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Actual acquisition rate in samples per second (4 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Observation {
    preamble_channel_sample: u64,
    estimated_carrier_offset_hz: f64,
    #[serde(flatten)]
    frame: frame::Frame,
}

#[derive(Default, Serialize)]
struct Evidence {
    crc_valid_candidates: usize,
    rejected_carrier_candidates: usize,
    validated_frames: usize,
    omitted_valid_frames: usize,
    frames: Vec<Observation>,
}

/// Evidence and source geometry for replay of an IQ prefix.
#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    input_samples_per_channel_sample: usize,
    channel_filter_delay_input_samples: usize,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    channelized_samples: u64,
    #[serde(flatten)]
    evidence: Evidence,
}

impl Report {
    /// Number of confirmed frame occurrences, including occurrences omitted from storage.
    pub(crate) fn validated_frames(&self) -> usize {
        self.evidence.validated_frames
    }

    /// Number of frame records retained in this replay.
    pub(crate) fn retained_frames(&self) -> usize {
        self.evidence.frames.len()
    }
}

/// Source geometry for replay of the fixed extended squitter channel.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Check the replay margin against the central 80% of a supported IQ stream.
    pub(crate) fn is_covered(self) -> bool {
        let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
        let half_span_hz = u64::from(self.sample_rate_hz) * 2 / 5;
        [AdsbChannel::OUTPUT_RATE_HZ, SAMPLE_RATE].contains(&self.sample_rate_hz)
            && (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&self.center_hz)
            && self.center_hz.abs_diff(CHANNEL_HZ) + AdsbChannel::HALF_WIDTH_HZ <= half_span_hz
    }

    /// Replay an admitted IQ prefix starting at sample zero.
    ///
    /// The caller supplies the source extents and rechecks the source identity.
    /// Validation continues after the requested record limit is reached.
    ///
    /// # Errors
    /// Rejects invalid geometry, out-of-bounds or invalid IQ, and interruption.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        analyzed_samples: u64,
        max_frame_records: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        check_stop(stop)?;
        ensure!(
            [AdsbChannel::OUTPUT_RATE_HZ, SAMPLE_RATE].contains(&self.sample_rate_hz),
            "ADS-B replay requires 4 or 20 MSps IQ input"
        );
        ensure!(
            self.is_covered(),
            "1090 MHz requires a {} Hz margin inside the central 80% of the capture",
            AdsbChannel::HALF_WIDTH_HZ
        );
        ensure!(
            analyzed_samples <= source_samples && source_samples <= MAX_SAMPLES,
            "ADS-B replay prefix exceeds the source or the supported IQ extent"
        );
        let mut channel = AdsbChannel::new(
            self.sample_rate_hz,
            CHANNEL_HZ as f64 - self.center_hz as f64,
        )?;
        let mut detector = Receiver::new(max_frame_records.min(MAX_FRAME_RECORDS));
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |sample, _| {
                if let Some(sample) = channel.push(sample) {
                    detector.push(sample);
                }
                Ok(())
            })?;
        Ok(Report {
            schema_version: 1,
            operation: "identify_adsb_in_saved_iq",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.center_hz,
            channel_frequency_hz: CHANNEL_HZ,
            sample_rate_hz: self.sample_rate_hz,
            channel_sample_rate_hz: AdsbChannel::OUTPUT_RATE_HZ,
            input_samples_per_channel_sample: channel.stride(),
            channel_filter_delay_input_samples: channel.delay_input_samples(),
            source_samples,
            analyzed_samples,
            analyzed_iq_sha256,
            channelized_samples: detector.samples,
            evidence: detector.evidence,
        })
    }
}

struct Receiver {
    detector: demod::Detector,
    history: [Iq; IQ_HISTORY_SAMPLES],
    samples: u64,
    recent: VecDeque<(u64, [u8; 14])>,
    max_frame_records: usize,
    evidence: Evidence,
}

impl Receiver {
    fn new(max_frame_records: usize) -> Self {
        Self {
            detector: demod::Detector::default(),
            history: [Iq::default(); IQ_HISTORY_SAMPLES],
            samples: 0,
            recent: VecDeque::new(),
            max_frame_records,
            evidence: Evidence::default(),
        }
    }

    fn push(&mut self, sample: Iq) {
        self.history[ring_index(self.samples)] = sample;
        self.samples += 1;
        let Some(decoded) = self.detector.push(sample.power()) else {
            return;
        };
        self.evidence.crc_valid_candidates += 1;
        let message = decoded.frame.bytes();
        let Some(estimated_carrier_offset_hz) = self.carrier_offset(decoded.start_sample, message)
        else {
            self.evidence.rejected_carrier_candidates += 1;
            return;
        };
        self.recent
            .retain(|(earlier, _)| decoded.start_sample - earlier <= DUPLICATE_WINDOW_SAMPLES);
        if self.recent.iter().any(|(_, earlier)| earlier == message) {
            return;
        }
        self.recent.push_back((decoded.start_sample, *message));
        self.evidence.validated_frames += 1;
        if self.evidence.frames.len() == self.max_frame_records {
            self.evidence.omitted_valid_frames += 1;
            return;
        }
        self.evidence.frames.push(Observation {
            preamble_channel_sample: decoded.start_sample,
            estimated_carrier_offset_hz,
            frame: decoded.frame,
        });
    }

    fn carrier_offset(&self, start: u64, message: &[u8; 14]) -> Option<f64> {
        let data_start = start + PREAMBLE_SAMPLES;
        let data_end = data_start + message.len() as u64 * 8 * SAMPLES_PER_BIT;
        if data_start < self.samples.saturating_sub(IQ_HISTORY_SAMPLES as u64)
            || data_end > self.samples
        {
            return None;
        }
        let mut phase = Iq::default();
        let mut weight = 0.0;
        for (byte_index, &byte) in message.iter().enumerate() {
            for bit_index in 0..8 {
                // The verified bits select the two adjacent samples in each pulse.
                // Products within a pulse remain useful when its starting phase changes.
                let one = byte & (0x80 >> bit_index) != 0;
                let pulse = data_start
                    + (byte_index as u64 * 8 + bit_index) * SAMPLES_PER_BIT
                    + if one { 0 } else { SAMPLES_PER_BIT / 2 };
                let first = self.history[ring_index(pulse)];
                let second = self.history[ring_index(pulse + 1)];
                phase.i += first.i * second.i + first.q * second.q;
                phase.q += first.i * second.q - first.q * second.i;
                weight += (first.power() * second.power()).sqrt();
            }
        }
        if weight <= 0.0 || phase.power() < (MIN_CARRIER_COHERENCE * weight).powi(2) {
            return None;
        }
        let offset_hz = phase.q.atan2(phase.i) * f64::from(AdsbChannel::OUTPUT_RATE_HZ) / TAU;
        (offset_hz.abs() <= CARRIER_LIMIT_HZ).then_some(offset_hz)
    }
}

fn ring_index(sample: u64) -> usize {
    (sample % IQ_HISTORY_SAMPLES as u64) as usize
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "ADS-B IQ replay interrupted");
    Ok(())
}

/// Replay one labeled capture and publish checksum-validated squitters privately.
///
/// # Errors
/// Rejects invalid geometry or IQ, changed inputs, interruption and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let channel = Channel {
        center_hz: args.center_hz,
        sample_rate_hz: args.sample_rate_hz,
    };
    let source = root.join(args.input);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
    let stamp = input.metadata()?;
    ensure!(
        length == stamp.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
        "ADS-B input needs a stable extent of complete I/Q pairs"
    );
    let source_samples = length / IQ_SAMPLE_BYTES;
    let analyzed_samples = source_samples.min(args.max_samples);
    let report = channel.replay(
        &mut input,
        source_samples,
        analyzed_samples,
        MAX_FRAME_RECORDS,
        stop,
    )?;
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        let current = source.symlink_metadata()?;
        ensure!(
            current.is_file()
                && file_pin::same_file(&stamp, &current)
                && file_pin::same_file(&stamp, &input.metadata()?),
            "ADS-B IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} extended squitters. Output: {}",
        report.validated_frames(),
        output.display()
    );
    Ok(())
}
