//! Bounded SC16_Q11 validation, frequency translation and FIR decimation.

use std::collections::BTreeSet;
use std::f32::consts::TAU;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use anyhow::{Context, Result, anyhow, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use serde::{Deserialize, Serialize};

const SOURCE_SAMPLE_RATE_HZ: u64 = 20_000_000;
const OUTPUT_SAMPLE_RATE_HZ: u64 = 4_000_000;
const DECIMATION: usize = 5;
const FILTER_TAPS: usize = 225;
const _: () = assert!(FILTER_TAPS.is_multiple_of(DECIMATION));
const FIR_BATCH_SIZE: usize = 4;
const TAPS_PER_PHASE: usize = FILTER_TAPS / DECIMATION;
const HISTORY_BLOCKS: usize = TAPS_PER_PHASE + FIR_BATCH_SIZE - 1;
const BATCHED_INPUT_SAMPLES: usize = FIR_BATCH_SIZE * DECIMATION;
const PASSBAND_EDGE_HZ: u64 = 1_500_000;
const STOPBAND_EDGE_HZ: u64 = 2_000_000;
const FILTER_CUTOFF_HZ: f64 = 1_750_000.0;
const KAISER_BETA: f64 = 7.857_26;
const Q11_SCALE: f32 = 2048.0;
const MAX_METADATA_BYTES: u64 = 65_536;
const IO_CHUNK_BYTES: usize = 64 * 1024;
const MIN_SOURCE_SAMPLES: u64 = DECIMATION as u64;
const MAX_SOURCE_SAMPLES: u64 = 200_000_000;

/// Inputs, output location, and resource bound for one offline channelization run.
#[derive(Debug)]
pub struct Request {
    /// SC16_Q11 input path; metadata is read from this path with `.json` appended.
    pub input: PathBuf,
    /// New output directory to create without replacing an existing path.
    pub output: PathBuf,
    /// Unique IEEE 802.15.4 channel numbers in the inclusive range 11 through 26.
    pub channels: Vec<u8>,
    /// Maximum input prefix to process, in complex samples (5..=200,000,000).
    pub max_samples: u64,
}

/// Inputs and bounds for one arbitrary-frequency offline channelization run.
#[derive(Debug)]
pub struct FrequencyRequest {
    /// SC16_Q11 input path; metadata is read from this path with `.json` appended.
    pub input: PathBuf,
    /// New output directory to create without replacing an existing path.
    pub output: PathBuf,
    /// Target center frequency in hertz.
    pub frequency_hz: u64,
    /// Maximum input prefix to process, in complex samples (5..=200,000,000).
    pub max_samples: u64,
}

#[derive(Debug, Deserialize)]
struct Acquisition {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: f64,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    num_samples: u64,
    channel: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct SidecarIdentity {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Complex {
    re: f32,
    im: f32,
}

impl Complex {
    fn multiply(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }

    fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

#[derive(Clone, Debug)]
struct FirDecimator {
    coefficients: [f32; FILTER_TAPS],
    history: [[Complex; 2 * HISTORY_BLOCKS]; DECIMATION],
    position: usize,
    modulo: usize,
}

impl FirDecimator {
    fn new() -> Self {
        Self {
            coefficients: filter_coefficients(),
            history: [[Complex::default(); 2 * HISTORY_BLOCKS]; DECIMATION],
            position: 0,
            modulo: 0,
        }
    }

    fn append(&mut self, sample: Complex) {
        let phase = &mut self.history[self.modulo];
        phase[self.position] = sample;
        phase[self.position + HISTORY_BLOCKS] = sample;
        self.modulo += 1;
        if self.modulo == DECIMATION {
            self.modulo = 0;
            self.position = (self.position + 1) % HISTORY_BLOCKS;
        }
    }

    fn push(&mut self, sample: Complex) -> Option<Complex> {
        self.append(sample);
        (self.modulo == 0).then(|| self.accumulate::<1>()[0])
    }

    fn batch(&self) -> [Complex; FIR_BATCH_SIZE] {
        self.accumulate()
    }

    fn accumulate<const OUTPUTS: usize>(&self) -> [Complex; OUTPUTS] {
        debug_assert_eq!(self.modulo, 0);
        let mut outputs = [Complex::default(); OUTPUTS];
        let end = HISTORY_BLOCKS + self.position;
        let phase_history = self
            .history
            .each_ref()
            .map(|phase| &phase[end - HISTORY_BLOCKS..end]);
        let (coefficient_blocks, _) = self.coefficients.as_chunks::<DECIMATION>();
        // The mirror makes adjacent output blocks contiguous for each tap.
        // Both loops visit coefficients in ascending tap order.
        for (lag, coefficients) in coefficient_blocks.iter().enumerate() {
            let first = HISTORY_BLOCKS - OUTPUTS - lag;
            for (&coefficient, samples) in coefficients.iter().zip(phase_history.iter().rev()) {
                for (output, sample) in outputs.iter_mut().zip(&samples[first..first + OUTPUTS]) {
                    output.re += coefficient * sample.re;
                    output.im += coefficient * sample.im;
                }
            }
        }
        outputs
    }
}

#[derive(Debug)]
struct ChannelProcessor {
    ieee_channel: Option<u8>,
    frequency_hz: u64,
    offset_hz: i64,
    edge_margin_hz: u64,
    oscillator: Complex,
    oscillator_step: Complex,
    oscillator_samples: u64,
    fir: FirDecimator,
}

impl ChannelProcessor {
    fn new(channel: u8, source_center_hz: u64, allow_nyquist_edge: bool) -> Result<Self> {
        let frequency_hz = channel_frequency_hz(channel)?;
        let mut processor =
            Self::new_with_coverage(frequency_hz, source_center_hz, allow_nyquist_edge)?;
        processor.ieee_channel = Some(channel);
        Ok(processor)
    }

    fn new_frequency(frequency_hz: u64, source_center_hz: u64) -> Result<Self> {
        Self::new_with_coverage(frequency_hz, source_center_hz, false)
    }

    fn new_with_coverage(
        frequency_hz: u64,
        source_center_hz: u64,
        allow_nyquist_edge: bool,
    ) -> Result<Self> {
        let offset_hz = i64::try_from(frequency_hz)? - i64::try_from(source_center_hz)?;
        let half_rate = SOURCE_SAMPLE_RATE_HZ / 2;
        let occupied_edge = offset_hz
            .unsigned_abs()
            .checked_add(STOPBAND_EDGE_HZ)
            .context("channel coverage edge overflow")?;
        ensure!(
            occupied_edge < half_rate || (allow_nyquist_edge && occupied_edge == half_rate),
            "target {frequency_hz} Hz lacks {} margin for the required {STOPBAND_EDGE_HZ} Hz stopband inside the source Nyquist range",
            if allow_nyquist_edge {
                "nonnegative"
            } else {
                "positive"
            }
        );
        let phase_step = -TAU * offset_hz as f32 / SOURCE_SAMPLE_RATE_HZ as f32;
        Ok(Self {
            ieee_channel: None,
            frequency_hz,
            offset_hz,
            edge_margin_hz: half_rate - occupied_edge,
            oscillator: Complex { re: 1.0, im: 0.0 },
            oscillator_step: Complex {
                re: phase_step.cos(),
                im: phase_step.sin(),
            },
            oscillator_samples: 0,
            fir: FirDecimator::new(),
        })
    }

    fn mix(&mut self, sample: Complex) -> Complex {
        let mixed = sample.multiply(self.oscillator);
        self.oscillator = self.oscillator.multiply(self.oscillator_step);
        self.oscillator_samples += 1;
        if self.oscillator_samples.is_multiple_of(4096) {
            let magnitude = self.oscillator.norm_sqr().sqrt();
            self.oscillator.re /= magnitude;
            self.oscillator.im /= magnitude;
        }
        mixed
    }

    fn push(&mut self, sample: Complex) -> Option<Complex> {
        let mixed = self.mix(sample);
        self.fir.push(mixed)
    }

    fn push_batch(
        &mut self,
        samples: &[Complex; BATCHED_INPUT_SAMPLES],
    ) -> [Complex; FIR_BATCH_SIZE] {
        for &sample in samples {
            let mixed = self.mix(sample);
            self.fir.append(mixed);
        }
        self.fir.batch()
    }
}

struct ChannelFile {
    processor: ChannelProcessor,
    partial_path: PathBuf,
    final_path: PathBuf,
    writer: BufWriter<File>,
    encoded_samples: Vec<u8>,
    digest: digest::Context,
    output_samples: u64,
    energy: f64,
    peak: f32,
}

impl ChannelFile {
    fn new(output: &Path, channel: u8, center_hz: u64, allow_nyquist_edge: bool) -> Result<Self> {
        Self::new_with_processor(
            output,
            &format!("channel-{channel}.cf32"),
            ChannelProcessor::new(channel, center_hz, allow_nyquist_edge)?,
        )
    }

    fn new_frequency(
        output: &Path,
        file_name: &str,
        frequency_hz: u64,
        center_hz: u64,
    ) -> Result<Self> {
        Self::new_with_processor(
            output,
            file_name,
            ChannelProcessor::new_frequency(frequency_hz, center_hz)?,
        )
    }

    fn new_with_processor(
        output: &Path,
        file_name: &str,
        processor: ChannelProcessor,
    ) -> Result<Self> {
        let final_path = output.join(file_name);
        let partial_path = output.join(format!("{file_name}.partial"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)
            .with_context(|| format!("create {}", partial_path.display()))?;
        Ok(Self {
            processor,
            partial_path,
            final_path,
            writer: BufWriter::with_capacity(IO_CHUNK_BYTES, file),
            encoded_samples: Vec::with_capacity((IO_CHUNK_BYTES / 4).div_ceil(DECIMATION) * 8),
            digest: digest::Context::new(&SHA256),
            output_samples: 0,
            energy: 0.0,
            peak: 0.0,
        })
    }

    fn push(&mut self, sample: Complex) -> Result<()> {
        if let Some(output) = self.processor.push(sample) {
            self.encode_sample(output)?;
        }
        Ok(())
    }

    fn push_samples(&mut self, samples: &[Complex]) -> Result<()> {
        let until_output = (DECIMATION - self.processor.fir.modulo) % DECIMATION;
        let (leading, samples) = samples.split_at(until_output.min(samples.len()));
        for &sample in leading {
            self.push(sample)?;
        }
        let (batches, trailing) = samples.as_chunks::<BATCHED_INPUT_SAMPLES>();
        for batch in batches {
            for output in self.processor.push_batch(batch) {
                self.encode_sample(output)?;
            }
        }
        for &sample in trailing {
            self.push(sample)?;
        }
        Ok(())
    }

    fn encode_sample(&mut self, output: Complex) -> Result<()> {
        ensure!(
            output.re.is_finite() && output.im.is_finite(),
            "channelizer produced a non-finite sample"
        );
        let encoded = [output.re.to_le_bytes(), output.im.to_le_bytes()];
        self.encoded_samples
            .extend_from_slice(encoded.as_flattened());
        self.output_samples += 1;
        let magnitude_squared = output.norm_sqr();
        self.energy += f64::from(magnitude_squared);
        self.peak = self.peak.max(magnitude_squared.sqrt());
        Ok(())
    }

    /// Write each encoded input chunk, including any valid prefix before an error.
    fn write_encoded_samples(&mut self) -> Result<()> {
        self.writer.write_all(&self.encoded_samples)?;
        self.digest.update(&self.encoded_samples);
        self.encoded_samples.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<ChannelReport> {
        debug_assert!(self.encoded_samples.is_empty());
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        fs::hard_link(&self.partial_path, &self.final_path).with_context(|| {
            format!(
                "publish {} without replacing an existing output",
                self.final_path.display()
            )
        })?;
        fs::remove_file(&self.partial_path)?;
        let file_name = self
            .final_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("channel output name is not UTF-8")?
            .to_owned();
        let relative_rms = if self.output_samples == 0 {
            None
        } else {
            Some((self.energy / self.output_samples as f64).sqrt())
        };
        Ok(ChannelReport {
            channel: self.processor.ieee_channel,
            center_frequency_hz: self.processor.frequency_hz,
            source_offset_hz: self.processor.offset_hz,
            source_edge_margin_hz: self.processor.edge_margin_hz,
            output_file: file_name,
            output_format: "CF32_LE",
            output_sample_rate_hz: OUTPUT_SAMPLE_RATE_HZ,
            output_complex_samples: self.output_samples,
            output_duration_seconds: self.output_samples as f64 / OUTPUT_SAMPLE_RATE_HZ as f64,
            sha256: HEXLOWER.encode(self.digest.finish().as_ref()),
            relative_rms,
            relative_peak: (self.output_samples > 0).then_some(f64::from(self.peak)),
        })
    }
}

#[derive(Debug, Serialize)]
struct SourceReport {
    input: PathBuf,
    sidecar: PathBuf,
    format: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    bandwidth_hz: u64,
    capture_channel_label: Option<String>,
    available_complex_samples: u64,
    processed_complex_samples: u64,
    processed_full_input: bool,
    source_bytes: u64,
    processed_prefix_bytes: u64,
    processed_prefix_sha256: String,
    continuity_guaranteed: bool,
    timestamp_kind: &'static str,
}

#[derive(Debug, Serialize)]
struct FilterReport {
    design: &'static str,
    taps: usize,
    kaiser_beta: f64,
    passband_edge_hz: u64,
    stopband_edge_hz: u64,
    design_cutoff_hz: u64,
    decimation: usize,
    group_delay_source_samples: usize,
    group_delay_seconds: f64,
    first_output_source_sample_index: usize,
    startup_transient_output_samples: usize,
    tail_flushed: bool,
}

#[derive(Debug, Serialize)]
struct ChannelReport {
    channel: Option<u8>,
    center_frequency_hz: u64,
    source_offset_hz: i64,
    source_edge_margin_hz: u64,
    output_file: String,
    output_format: &'static str,
    output_sample_rate_hz: u64,
    output_complex_samples: u64,
    output_duration_seconds: f64,
    sha256: String,
    relative_rms: Option<f64>,
    relative_peak: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Manifest {
    schema_version: u32,
    operation: &'static str,
    source: SourceReport,
    filter: FilterReport,
    channels: Vec<ChannelReport>,
    interpretation: &'static str,
}

#[derive(Debug, Serialize)]
struct FrequencyChannelReport {
    center_frequency_hz: u64,
    source_offset_hz: i64,
    source_edge_margin_hz: u64,
    output_file: String,
    output_format: &'static str,
    output_sample_rate_hz: u64,
    output_complex_samples: u64,
    output_duration_seconds: f64,
    sha256: String,
    relative_rms: Option<f64>,
    relative_peak: Option<f64>,
}

impl From<ChannelReport> for FrequencyChannelReport {
    fn from(report: ChannelReport) -> Self {
        Self {
            center_frequency_hz: report.center_frequency_hz,
            source_offset_hz: report.source_offset_hz,
            source_edge_margin_hz: report.source_edge_margin_hz,
            output_file: report.output_file,
            output_format: report.output_format,
            output_sample_rate_hz: report.output_sample_rate_hz,
            output_complex_samples: report.output_complex_samples,
            output_duration_seconds: report.output_duration_seconds,
            sha256: report.sha256,
            relative_rms: report.relative_rms,
            relative_peak: report.relative_peak,
        }
    }
}

#[derive(Debug, Serialize)]
struct FrequencyManifest {
    schema_version: u32,
    operation: &'static str,
    acquisition_sidecar: SidecarIdentity,
    source: SourceReport,
    filter: FilterReport,
    sample_index_mapping: FrequencySampleIndexMapping,
    target: FrequencyChannelReport,
    continuity_guaranteed: bool,
    timestamp_kind: &'static str,
    interpretation: &'static str,
}

#[derive(Debug, Serialize)]
struct FrequencySampleIndexMapping {
    output_stride_source_samples: usize,
    latest_source_index_for_output_zero: usize,
    group_delay_corrected_source_index_for_output_zero: i64,
}

/// Parses a comma-separated IEEE 802.15.4 channel list into ascending order.
///
/// Whitespace surrounding each item is ignored.
///
/// # Errors
///
/// Returns an error when the list is empty, contains an empty or non-numeric
/// item, names a channel outside 11 through 26, or repeats a channel.
pub fn parse_channels(input: &str) -> Result<Vec<u8>> {
    let mut channels = BTreeSet::new();
    for item in input.split(',') {
        let item = item.trim();
        ensure!(!item.is_empty(), "channel list contains an empty item");
        let channel: u8 = item
            .parse()
            .with_context(|| format!("invalid IEEE 802.15.4 channel {item:?}"))?;
        ensure!(
            (11..=26).contains(&channel),
            "IEEE 802.15.4 channel must be 11..=26"
        );
        ensure!(channels.insert(channel), "duplicate channel {channel}");
    }
    ensure!(!channels.is_empty(), "at least one channel is required");
    Ok(channels.into_iter().collect())
}

fn channel_frequency_hz(channel: u8) -> Result<u64> {
    ensure!(
        (11..=26).contains(&channel),
        "IEEE 802.15.4 channel must be 11..=26"
    );
    Ok(2_405_000_000 + u64::from(channel - 11) * 5_000_000)
}

fn append_json(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    PathBuf::from(name)
}

fn finite_integer(value: f64, label: &str) -> Result<u64> {
    ensure!(
        value.is_finite() && value > 0.0,
        "{label} must be finite and positive"
    );
    let rounded = value.round();
    ensure!(
        (value - rounded).abs() <= 0.001 && rounded <= u64::MAX as f64,
        "{label} must be an exactly representable integer number of hertz"
    );
    Ok(rounded as u64)
}

fn read_acquisition(input: &Path) -> Result<(Acquisition, PathBuf, SidecarIdentity)> {
    let sidecar = append_json(input);
    let file = File::open(&sidecar)
        .with_context(|| format!("open acquisition sidecar {}", sidecar.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "acquisition sidecar must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read acquisition sidecar {}", sidecar.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA_BYTES,
        "acquisition sidecar exceeds 64 KiB"
    );
    let acquisition: Acquisition = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse acquisition sidecar {}", sidecar.display()))?;
    ensure!(
        acquisition.format == "SC16_Q11"
            && acquisition.dtype == "<i2"
            && acquisition.interleave == "I,Q"
            && acquisition.complex_scale == f64::from(Q11_SCALE),
        "requires SC16_Q11 little-endian int16 I,Q metadata with scale 2048"
    );
    if let Some(channel) = &acquisition.channel {
        ensure!(
            channel.len() <= 32 && channel.bytes().all(|byte| byte.is_ascii_graphic()),
            "capture channel label must contain at most 32 printable ASCII bytes"
        );
    }
    Ok((
        acquisition,
        sidecar,
        SidecarIdentity {
            bytes: bytes.len() as u64,
            sha256: HEXLOWER.encode(digest::digest(&SHA256, &bytes).as_ref()),
        },
    ))
}

fn bessel_i0(value: f64) -> f64 {
    let y = value * value / 4.0;
    let mut sum = 1.0;
    let mut term = 1.0;
    for index in 1..=64 {
        term *= y / (index * index) as f64;
        sum += term;
        if term.abs() <= sum.abs() * 1e-16 {
            break;
        }
    }
    sum
}

fn filter_coefficients() -> [f32; FILTER_TAPS] {
    let midpoint = (FILTER_TAPS - 1) as f64 / 2.0;
    let denominator = bessel_i0(KAISER_BETA);
    let mut coefficients = [0.0; FILTER_TAPS];
    for (index, coefficient) in coefficients.iter_mut().enumerate() {
        let distance = index as f64 - midpoint;
        let sinc = if distance == 0.0 {
            2.0 * FILTER_CUTOFF_HZ / SOURCE_SAMPLE_RATE_HZ as f64
        } else {
            (std::f64::consts::TAU * FILTER_CUTOFF_HZ / SOURCE_SAMPLE_RATE_HZ as f64 * distance)
                .sin()
                / (std::f64::consts::PI * distance)
        };
        let ratio = distance / midpoint;
        let window = bessel_i0(KAISER_BETA * (1.0 - ratio * ratio).max(0.0).sqrt()) / denominator;
        *coefficient = (sinc * window) as f32;
    }
    let sum: f32 = coefficients.iter().sum();
    for coefficient in &mut coefficients {
        *coefficient /= sum;
    }
    coefficients
}

fn parse_q11(raw: &[u8]) -> Result<Complex> {
    ensure!(raw.len() == 4, "one SC16_Q11 sample requires four bytes");
    let i = i16::from_le_bytes([raw[0], raw[1]]);
    let q = i16::from_le_bytes([raw[2], raw[3]]);
    ensure!(
        (-2048..=2047).contains(&i) && (-2048..=2047).contains(&q),
        "IQ contains a component outside the SC16_Q11 range"
    );
    Ok(Complex {
        re: f32::from(i) / Q11_SCALE,
        im: f32::from(q) / Q11_SCALE,
    })
}

fn read_iq_prefix(
    input: &mut File,
    mut remaining: u64,
    mut consume: impl FnMut(&[[u8; 4]]) -> Result<()>,
) -> Result<digest::Digest> {
    let mut digest = digest::Context::new(&SHA256);
    let mut buffer = vec![0u8; IO_CHUNK_BYTES];
    while remaining > 0 {
        let count = usize::try_from(remaining.min(IO_CHUNK_BYTES as u64))?;
        ensure!(
            count.is_multiple_of(4),
            "internal IQ chunk lost sample alignment"
        );
        input
            .read_exact(&mut buffer[..count])
            .context("read complete IQ input prefix")?;
        digest.update(&buffer[..count]);
        consume(buffer[..count].as_chunks::<4>().0)?;
        remaining -= count as u64;
    }
    Ok(digest.finish())
}

fn channelize_one(
    input: &mut File,
    bytes: u64,
    channel: &mut ChannelFile,
) -> Result<digest::Digest> {
    read_iq_prefix(input, bytes, |chunk| {
        let processed = chunk
            .iter()
            .try_for_each(|raw| channel.push(parse_q11(raw)?));
        processed.and(channel.write_encoded_samples())
    })
}

fn channelize_channels(
    input: &mut File,
    bytes: u64,
    channels: &mut [ChannelFile],
) -> Result<digest::Digest> {
    if let [channel] = channels {
        return channelize_one(input, bytes, channel);
    }
    // The validated 5 MHz channel spacing and stopband margin allow at most
    // four channels. Each worker has room for one queued shared chunk.
    thread::scope(|scope| {
        let mut senders = Vec::with_capacity(channels.len());
        let mut workers = Vec::with_capacity(channels.len());
        let spawned = channels
            .iter_mut()
            .enumerate()
            .try_for_each(|(index, channel)| {
                let (sender, receiver) = mpsc::sync_channel::<Arc<Vec<Complex>>>(1);
                let worker = thread::Builder::new()
                    .spawn_scoped(scope, move || -> Result<()> {
                        for chunk in receiver {
                            let processed = channel.push_samples(&chunk);
                            processed
                                .and(channel.write_encoded_samples())
                                .with_context(|| {
                                    format!(
                                        "write channel output {}",
                                        channel.partial_path.display()
                                    )
                                })?;
                        }
                        Ok(())
                    })
                    .with_context(|| format!("start channelizer worker {}", index + 1))?;
                senders.push(sender);
                workers.push(worker);
                Ok(())
            });
        let mut disconnected = false;
        let reader_result = spawned.and_then(|()| {
            read_iq_prefix(input, bytes, |chunk| {
                let mut samples = Vec::with_capacity(chunk.len());
                let parsed = chunk.iter().try_for_each(|raw| {
                    samples.push(parse_q11(raw)?);
                    Ok::<_, anyhow::Error>(())
                });
                // Preserve valid samples preceding a malformed sample.
                let delivered = if samples.is_empty() {
                    true
                } else {
                    let samples = Arc::new(samples);
                    senders
                        .iter()
                        .all(|sender| sender.send(Arc::clone(&samples)).is_ok())
                };
                parsed?;
                disconnected = !delivered;
                ensure!(!disconnected, "channelizer worker disconnected");
                Ok(())
            })
        });
        drop(senders);
        // All receivers can now terminate, including after a read or spawn error.
        let worker_result = workers
            .into_iter()
            .enumerate()
            .map(|(index, worker)| {
                worker
                    .join()
                    .map_err(|_| anyhow!("channelizer worker {} panicked", index + 1))?
            })
            .fold(Ok(()), Result::and);
        if disconnected {
            worker_result?;
            return reader_result;
        }
        let digest = reader_result?;
        worker_result?;
        Ok(digest)
    })
}

fn write_summary(manifest: &impl Serialize, label: &str, output: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(manifest)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{json}\n{label}: {}", output.display())
        .context("write channelization summary")?;
    stdout.flush().context("flush channelization summary")
}

fn write_manifest(path: &Path, manifest: &impl Serialize) -> Result<()> {
    let mut partial_name = path.as_os_str().to_os_string();
    partial_name.push(".partial");
    let partial_path = PathBuf::from(partial_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial_path)
        .with_context(|| format!("create {}", partial_path.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::hard_link(&partial_path, path).with_context(|| {
        format!(
            "publish {} without replacing an existing output",
            path.display()
        )
    })?;
    fs::remove_file(&partial_path)?;
    Ok(())
}

/// Channelizes a bounded prefix of one 20 MSps SC16_Q11 capture.
///
/// This is a blocking, file-only operation. For each requested channel it
/// publishes a 4 MSps CF32 stream under the new output directory, then writes a
/// `channelize.json` provenance manifest. The same manifest and a short output
/// summary are printed to standard output for compatibility with the CLI.
/// Multi-channel calls share one input read and join all workers before
/// publishing channel files in request order.
/// The 2 MHz stopband edge must have positive source Nyquist margin.
///
/// # Errors
///
/// Returns an error if the request is out of range, channels are invalid or
/// duplicated, input metadata or samples violate the documented capture
/// contract, the checked input extent or modification time changes during
/// processing, an output path already exists, or any I/O, serialization or
/// channelization invariant fails. A failed run can leave its newly created
/// output directory and partial or completed channel files for inspection;
/// existing paths are never replaced.
pub fn run(request: Request) -> Result<()> {
    run_with_coverage(request, false)
}

/// Channelizes nominal IEEE channels, allowing zero source Nyquist margin.
///
/// At equality the 2 MHz stopband edge reaches source Nyquist while the
/// 1.5 MHz passband stays 500 kHz inside. The manifest records zero margin.
/// This qualifies the digital filter geometry. Analog band-edge reception
/// depends on the acquisition path.
///
/// # Errors
///
/// Returns the same input, publication and processing errors as [`run`].
pub fn run_allowing_nyquist_edge(request: Request) -> Result<()> {
    run_with_coverage(request, true)
}

fn run_with_coverage(request: Request, allow_nyquist_edge: bool) -> Result<()> {
    ensure!(
        (MIN_SOURCE_SAMPLES..=MAX_SOURCE_SAMPLES).contains(&request.max_samples),
        "max-samples must be {MIN_SOURCE_SAMPLES}..={MAX_SOURCE_SAMPLES}"
    );
    ensure!(
        !request.channels.is_empty(),
        "at least one channel is required"
    );
    let unique: BTreeSet<_> = request.channels.iter().copied().collect();
    ensure!(
        unique.len() == request.channels.len(),
        "channel list contains duplicates"
    );

    let (acquisition, sidecar, _) = read_acquisition(&request.input)?;
    let source_center_hz = finite_integer(acquisition.frequency_hz, "center frequency")?;
    let source_rate_hz = finite_integer(acquisition.sample_rate_hz, "sample rate")?;
    let source_bandwidth_hz = finite_integer(acquisition.bandwidth_hz, "bandwidth")?;
    ensure!(
        source_rate_hz == SOURCE_SAMPLE_RATE_HZ && source_bandwidth_hz == SOURCE_SAMPLE_RATE_HZ,
        "channelizer requires 20 MHz sample rate and bandwidth"
    );
    ensure!(
        (MIN_SOURCE_SAMPLES..=MAX_SOURCE_SAMPLES).contains(&acquisition.num_samples),
        "acquisition sample count must be {MIN_SOURCE_SAMPLES}..={MAX_SOURCE_SAMPLES}"
    );

    let mut input = File::open(&request.input)
        .with_context(|| format!("open IQ input {}", request.input.display()))?;
    let before = input.metadata()?;
    ensure!(before.is_file(), "IQ input must be a regular file");
    let expected_bytes = acquisition
        .num_samples
        .checked_mul(4)
        .context("IQ byte count overflow")?;
    ensure!(
        before.len() == expected_bytes,
        "IQ byte length does not match acquisition sample count"
    );
    let before_modified = before.modified().ok();
    let processed_samples = acquisition.num_samples.min(request.max_samples);
    let processed_bytes = processed_samples
        .checked_mul(4)
        .context("processed IQ byte count overflow")?;

    for channel in &request.channels {
        ChannelProcessor::new(*channel, source_center_hz, allow_nyquist_edge)?;
    }

    if let Some(parent) = request
        .output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output parent {}", parent.display()))?;
    }
    DirBuilder::new()
        .mode(0o700)
        .create(&request.output)
        .with_context(|| {
            format!(
                "create new output directory {} (existing outputs are preserved)",
                request.output.display()
            )
        })?;
    let mut channels = request
        .channels
        .iter()
        .map(|channel| {
            ChannelFile::new(
                &request.output,
                *channel,
                source_center_hz,
                allow_nyquist_edge,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let input_digest = channelize_channels(&mut input, processed_bytes, &mut channels)?;
    let after = input.metadata()?;
    ensure!(
        after.len() == before.len()
            && before_modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "IQ input changed while channelizing"
    );

    let reports = channels
        .into_iter()
        .map(ChannelFile::finish)
        .collect::<Result<Vec<_>>>()?;
    let expected_output_samples = processed_samples / DECIMATION as u64;
    ensure!(
        reports
            .iter()
            .all(|report| report.output_complex_samples == expected_output_samples),
        "channelizer output count invariant failed"
    );

    let manifest = Manifest {
        schema_version: 1,
        operation: "offline_ieee802154_channelization",
        source: SourceReport {
            input: request.input,
            sidecar,
            format: "SC16_Q11_LE",
            center_frequency_hz: source_center_hz,
            sample_rate_hz: source_rate_hz,
            bandwidth_hz: source_bandwidth_hz,
            capture_channel_label: acquisition.channel,
            available_complex_samples: acquisition.num_samples,
            processed_complex_samples: processed_samples,
            processed_full_input: processed_samples == acquisition.num_samples,
            source_bytes: before.len(),
            processed_prefix_bytes: processed_bytes,
            processed_prefix_sha256: HEXLOWER.encode(input_digest.as_ref()),
            continuity_guaranteed: false,
            timestamp_kind: "none; sample zero follows the source prefix and source sidecar timing is not promoted",
        },
        filter: FilterReport {
            design: "normalized 225-tap Kaiser-windowed sinc low-pass",
            taps: FILTER_TAPS,
            kaiser_beta: KAISER_BETA,
            passband_edge_hz: PASSBAND_EDGE_HZ,
            stopband_edge_hz: STOPBAND_EDGE_HZ,
            design_cutoff_hz: FILTER_CUTOFF_HZ as u64,
            decimation: DECIMATION,
            group_delay_source_samples: (FILTER_TAPS - 1) / 2,
            group_delay_seconds: ((FILTER_TAPS - 1) / 2) as f64 / SOURCE_SAMPLE_RATE_HZ as f64,
            first_output_source_sample_index: DECIMATION - 1,
            startup_transient_output_samples: (FILTER_TAPS - 1) / DECIMATION,
            tail_flushed: false,
        },
        channels: reports,
        interpretation: "Channel-centered CF32 samples are bounded derived decoder inputs, not decoded packets or proof of IEEE 802.15.4 activity. Relative RMS/peak values are normalized sample amplitudes, not calibrated dBm. FIR startup transients are retained; source continuity and RF timestamps are not established.",
    };
    write_manifest(&request.output.join("channelize.json"), &manifest)?;
    File::open(&request.output)?.sync_all()?;
    write_summary(&manifest, "Channelized output", &request.output)
}

/// Channelizes one bounded 20 MSps SC16_Q11 prefix around an arbitrary frequency.
///
/// This additive file-only path uses the same fixed filter and decimator as
/// [`run`], but publishes one `frequency-HZ.cf32` stream and a separate
/// `channelize-frequency.json` manifest. Existing paths are never replaced. A
/// failed run can leave its new output directory and partial files for inspection.
///
/// # Errors
///
/// Returns an error for invalid bounds, unsupported input metadata or encoding,
/// insufficient source coverage, changed inputs, existing outputs, or I/O and
/// serialization failures. Published files remain available if reporting to
/// standard output fails.
pub fn run_frequency(request: FrequencyRequest) -> Result<()> {
    ensure!(
        (MIN_SOURCE_SAMPLES..=MAX_SOURCE_SAMPLES).contains(&request.max_samples),
        "max-samples must be {MIN_SOURCE_SAMPLES}..={MAX_SOURCE_SAMPLES}"
    );
    ensure!(
        request.frequency_hz > 0,
        "target frequency must be positive"
    );

    let (acquisition, sidecar, acquisition_sidecar) = read_acquisition(&request.input)?;
    let source_center_hz = finite_integer(acquisition.frequency_hz, "center frequency")?;
    let source_rate_hz = finite_integer(acquisition.sample_rate_hz, "sample rate")?;
    let source_bandwidth_hz = finite_integer(acquisition.bandwidth_hz, "bandwidth")?;
    ensure!(
        source_rate_hz == SOURCE_SAMPLE_RATE_HZ && source_bandwidth_hz == SOURCE_SAMPLE_RATE_HZ,
        "channelizer requires 20 MHz sample rate and bandwidth"
    );
    ensure!(
        (MIN_SOURCE_SAMPLES..=MAX_SOURCE_SAMPLES).contains(&acquisition.num_samples),
        "acquisition sample count must be {MIN_SOURCE_SAMPLES}..={MAX_SOURCE_SAMPLES}"
    );
    ChannelProcessor::new_frequency(request.frequency_hz, source_center_hz)?;

    let mut input = File::open(&request.input)
        .with_context(|| format!("open IQ input {}", request.input.display()))?;
    let before = input.metadata()?;
    ensure!(before.is_file(), "IQ input must be a regular file");
    let expected_bytes = acquisition
        .num_samples
        .checked_mul(4)
        .context("IQ byte count overflow")?;
    ensure!(
        before.len() == expected_bytes,
        "IQ byte length does not match acquisition sample count"
    );
    let before_modified = before.modified().ok();
    let processed_samples = acquisition.num_samples.min(request.max_samples);
    let processed_bytes = processed_samples
        .checked_mul(4)
        .context("processed IQ byte count overflow")?;

    if let Some(parent) = request
        .output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create output parent {}", parent.display()))?;
    }
    DirBuilder::new()
        .mode(0o700)
        .create(&request.output)
        .with_context(|| {
            format!(
                "create new output directory {} (existing outputs are preserved)",
                request.output.display()
            )
        })?;
    let output_name = format!("frequency-{}.cf32", request.frequency_hz);
    let mut channel = ChannelFile::new_frequency(
        &request.output,
        &output_name,
        request.frequency_hz,
        source_center_hz,
    )?;

    let input_digest = channelize_one(&mut input, processed_bytes, &mut channel)?;
    let after = input.metadata()?;
    ensure!(
        after.len() == before.len()
            && before_modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "IQ input changed while channelizing"
    );

    let report = channel.finish()?;
    ensure!(
        report.output_complex_samples == processed_samples / DECIMATION as u64,
        "channelizer output count invariant failed"
    );
    let manifest = FrequencyManifest {
        schema_version: 1,
        operation: "offline_frequency_channelization",
        acquisition_sidecar,
        source: SourceReport {
            input: request.input,
            sidecar,
            format: "SC16_Q11_LE",
            center_frequency_hz: source_center_hz,
            sample_rate_hz: source_rate_hz,
            bandwidth_hz: source_bandwidth_hz,
            capture_channel_label: acquisition.channel,
            available_complex_samples: acquisition.num_samples,
            processed_complex_samples: processed_samples,
            processed_full_input: processed_samples == acquisition.num_samples,
            source_bytes: before.len(),
            processed_prefix_bytes: processed_bytes,
            processed_prefix_sha256: HEXLOWER.encode(input_digest.as_ref()),
            continuity_guaranteed: false,
            timestamp_kind: "none; sample zero follows the source prefix and source sidecar timing is not promoted",
        },
        filter: FilterReport {
            design: "normalized 225-tap Kaiser-windowed sinc low-pass",
            taps: FILTER_TAPS,
            kaiser_beta: KAISER_BETA,
            passband_edge_hz: PASSBAND_EDGE_HZ,
            stopband_edge_hz: STOPBAND_EDGE_HZ,
            design_cutoff_hz: FILTER_CUTOFF_HZ as u64,
            decimation: DECIMATION,
            group_delay_source_samples: (FILTER_TAPS - 1) / 2,
            group_delay_seconds: ((FILTER_TAPS - 1) / 2) as f64 / SOURCE_SAMPLE_RATE_HZ as f64,
            first_output_source_sample_index: DECIMATION - 1,
            startup_transient_output_samples: (FILTER_TAPS - 1) / DECIMATION,
            tail_flushed: false,
        },
        sample_index_mapping: FrequencySampleIndexMapping {
            output_stride_source_samples: DECIMATION,
            latest_source_index_for_output_zero: DECIMATION - 1,
            group_delay_corrected_source_index_for_output_zero: DECIMATION as i64
                - 1
                - ((FILTER_TAPS - 1) / 2) as i64,
        },
        target: report.into(),
        continuity_guaranteed: false,
        timestamp_kind: "derived sample indices only; absolute RF time and source continuity are not established",
        interpretation: "Frequency-centered CF32 samples are bounded derived analysis inputs, not decoded packets, occupancy, protocol identity or proof of a transmitter. Relative RMS/peak values are normalized sample amplitudes, not calibrated dBm. FIR startup transients are retained and the tail is not flushed.",
    };
    write_manifest(&request.output.join("channelize-frequency.json"), &manifest)?;
    File::open(&request.output)?.sync_all()?;
    write_summary(&manifest, "Frequency-channelized output", &request.output)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use sha2::{Digest, Sha256};

    use super::*;

    fn tone(frequency_hz: f64, samples: usize) -> Vec<Complex> {
        (0..samples)
            .map(|index| {
                let phase = std::f64::consts::TAU * frequency_hz * index as f64
                    / SOURCE_SAMPLE_RATE_HZ as f64;
                Complex {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect()
    }

    fn process(processor: &mut ChannelProcessor, input: &[Complex]) -> Vec<Complex> {
        input
            .iter()
            .filter_map(|sample| processor.push(*sample))
            .collect()
    }

    struct ReferenceFir {
        coefficients: [f32; FILTER_TAPS],
        history: [Complex; FILTER_TAPS],
        position: usize,
        modulo: usize,
    }

    impl ReferenceFir {
        fn new() -> Self {
            Self {
                coefficients: filter_coefficients(),
                history: [Complex::default(); FILTER_TAPS],
                position: 0,
                modulo: 0,
            }
        }
    }

    fn reference_push(decimator: &mut ReferenceFir, sample: Complex) -> Option<Complex> {
        decimator.history[decimator.position] = sample;
        decimator.position = (decimator.position + 1) % FILTER_TAPS;
        decimator.modulo += 1;
        if decimator.modulo < DECIMATION {
            return None;
        }
        decimator.modulo = 0;

        let mut output = Complex::default();
        let mut index = (decimator.position + FILTER_TAPS - 1) % FILTER_TAPS;
        for coefficient in &decimator.coefficients {
            output.re += coefficient * decimator.history[index].re;
            output.im += coefficient * decimator.history[index].im;
            index = if index == 0 {
                FILTER_TAPS - 1
            } else {
                index - 1
            };
        }
        Some(output)
    }

    fn rms(samples: &[Complex]) -> f64 {
        (samples
            .iter()
            .map(|sample| f64::from(sample.norm_sqr()))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    }

    fn response_db(coefficients: &[f32], frequency_hz: f64) -> f64 {
        let (re, im) =
            coefficients
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(re, im), (index, coefficient)| {
                    let phase = -std::f64::consts::TAU * frequency_hz * index as f64
                        / SOURCE_SAMPLE_RATE_HZ as f64;
                    (
                        re + f64::from(*coefficient) * phase.cos(),
                        im + f64::from(*coefficient) * phase.sin(),
                    )
                });
        20.0 * re.hypot(im).log10()
    }

    #[test]
    fn single_output_accumulation_matches_wrapping_reference() {
        let mut candidate = FirDecimator::new();
        let mut reference = ReferenceFir::new();
        for index in 0..FILTER_TAPS * 4 {
            let sample = Complex {
                re: (index.wrapping_mul(17) % 101) as f32 / 64.0 - 0.75,
                im: (index.wrapping_mul(29) % 97) as f32 / 64.0 - 0.75,
            };
            let actual = candidate.push(sample);
            let expected = reference_push(&mut reference, sample);
            assert_eq!(
                actual.map(|value| (value.re.to_bits(), value.im.to_bits())),
                expected.map(|value| (value.re.to_bits(), value.im.to_bits()))
            );
        }
    }

    #[test]
    fn four_output_accumulation_matches_wrapping_reference() {
        let input = tone(1_437_500.0, BATCHED_INPUT_SAMPLES * 64);
        let mut candidate = FirDecimator::new();
        let mut reference = ReferenceFir::new();
        for samples in input.as_chunks::<BATCHED_INPUT_SAMPLES>().0 {
            let mut expected = Vec::new();
            for &sample in samples {
                candidate.append(sample);
                expected.extend(reference_push(&mut reference, sample));
            }
            let actual = candidate.batch();
            assert_eq!(expected.len(), actual.len());
            for (output, expected) in actual.into_iter().zip(expected) {
                assert_eq!(output.re.to_bits(), expected.re.to_bits());
                assert_eq!(output.im.to_bits(), expected.im.to_bits());
            }
        }
    }

    fn write_capture(directory: &Path, samples: &[(i16, i16)]) -> PathBuf {
        let input = directory.join("input.sc16q11");
        let mut iq = Vec::with_capacity(samples.len() * 4);
        for (i, q) in samples {
            iq.extend_from_slice(&i.to_le_bytes());
            iq.extend_from_slice(&q.to_le_bytes());
        }
        fs::write(&input, iq).unwrap();
        let metadata = serde_json::json!({
            "format":"SC16_Q11", "dtype":"<i2", "interleave":"I,Q",
            "complex_scale":2048, "frequency_hz":2_437_000_000.0,
            "sample_rate_hz":20_000_000.0, "bandwidth_hz":20_000_000.0,
            "num_samples":samples.len(), "channel":"RX1"
        });
        fs::write(append_json(&input), serde_json::to_vec(&metadata).unwrap()).unwrap();
        input
    }

    #[test]
    fn maps_channels_and_rejects_insufficient_source_coverage() {
        assert_eq!(channel_frequency_hz(11).unwrap(), 2_405_000_000);
        assert_eq!(channel_frequency_hz(17).unwrap(), 2_435_000_000);
        assert_eq!(channel_frequency_hz(18).unwrap(), 2_440_000_000);
        assert_eq!(channel_frequency_hz(26).unwrap(), 2_480_000_000);
        assert!(channel_frequency_hz(10).is_err());
        assert_eq!(
            ChannelProcessor::new(17, 2_437_000_000, false)
                .unwrap()
                .offset_hz,
            -2_000_000
        );
        assert_eq!(
            ChannelProcessor::new(18, 2_437_000_000, false)
                .unwrap()
                .offset_hz,
            3_000_000
        );
        assert!(ChannelProcessor::new(16, 2_437_000_000, false).is_ok());
        assert!(ChannelProcessor::new(19, 2_437_000_000, false).is_err());
        assert!(ChannelProcessor::new(15, 2_437_000_000, false).is_err());
        assert!(ChannelProcessor::new(20, 2_437_000_000, false).is_err());
        assert_eq!(parse_channels("18, 17").unwrap(), vec![17, 18]);
        assert!(parse_channels("17,17").is_err());
        let arbitrary = ChannelProcessor::new_frequency(2_438_000_000, 2_437_000_000).unwrap();
        assert_eq!(arbitrary.ieee_channel, None);
        assert_eq!(arbitrary.offset_hz, 1_000_000);
        assert!(ChannelProcessor::new_frequency(2_445_000_000, 2_437_000_000).is_err());
    }

    #[test]
    fn edge_allowance_admits_equality_on_either_side_only() {
        for (channel, center, offset) in [
            (19, 2_437_000_000_u64, 8_000_000_i64),
            (15, 2_433_000_000_u64, -8_000_000_i64),
        ] {
            let processor = ChannelProcessor::new(channel, center, true).unwrap();
            assert_eq!(processor.offset_hz, offset);
            assert_eq!(processor.edge_margin_hz, 0);
            assert!(ChannelProcessor::new(channel, center, false).is_err());
            let outside = center.checked_add_signed(-offset.signum()).unwrap();
            assert!(ChannelProcessor::new(channel, outside, true).is_err());
        }
    }

    #[test]
    fn edge_channels_preserve_the_outer_passband_and_reject_opposite_edge_tones() {
        let count = 32_768;
        let skip = 200;
        for (channel, center, direction) in [(19, 2_437_000_000, 1.0), (15, 2_433_000_000, -1.0)] {
            let gain_db = |frequency_hz| {
                let mut processor = ChannelProcessor::new(channel, center, true).unwrap();
                let output = process(&mut processor, &tone(frequency_hz, count));
                20.0 * rms(&output[skip..]).log10()
            };
            let wanted = gain_db(direction * 9_500_000.0);
            assert!(wanted.abs() < 0.05, "edge passband gain {wanted} dB");
            for opposite_tone in [8_000_000.0, 9_975_000.0] {
                let rejected = gain_db(-direction * opposite_tone);
                assert!(rejected < -60.0, "opposite-edge gain {rejected} dB");
            }
        }
    }

    #[test]
    fn nominal_edge_manifest_records_zero_margin() {
        let directory = tempfile::tempdir().unwrap();
        let input = write_capture(directory.path(), &[(0, 0); 10]);
        let output = directory.path().join("channelized");
        let request = || Request {
            input: input.clone(),
            output: output.clone(),
            channels: vec![19],
            max_samples: 10,
        };
        assert!(run(request()).is_err());
        assert!(!output.exists());
        run_allowing_nyquist_edge(request()).unwrap();
        let manifest = fs::read(output.join("channelize.json")).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        assert_eq!(report["channels"][0]["source_edge_margin_hz"], 0);
        assert_eq!(
            fs::metadata(output.join("channel-19.cf32")).unwrap().len(),
            16
        );
    }

    #[test]
    fn frozen_filter_meets_passband_and_stopband_contract() {
        let coefficients = filter_coefficients();
        for bin in 0..=300 {
            let frequency = PASSBAND_EDGE_HZ as f64 * bin as f64 / 300.0;
            let response = response_db(&coefficients, frequency);
            assert!(
                (-0.05..=0.05).contains(&response),
                "passband response at {frequency} Hz was {response} dB"
            );
        }
        let maximum_stopband = (0..=1600)
            .map(|bin| {
                let frequency = STOPBAND_EDGE_HZ as f64
                    + (SOURCE_SAMPLE_RATE_HZ as f64 / 2.0 - STOPBAND_EDGE_HZ as f64) * bin as f64
                        / 1600.0;
                response_db(&coefficients, frequency)
            })
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            maximum_stopband <= -70.0,
            "maximum stopband was {maximum_stopband} dB"
        );
    }

    #[test]
    fn mixer_centers_the_selected_negative_offset() {
        let input = tone(-2_000_000.0, 20_000);
        let mut processor = ChannelProcessor::new(17, 2_437_000_000, false).unwrap();
        let output = process(&mut processor, &input);
        let settled = &output[100..];
        let mean = settled.iter().fold(Complex::default(), |mut sum, value| {
            sum.re += value.re;
            sum.im += value.im;
            sum
        });
        let mean_magnitude = f64::from(mean.norm_sqr()).sqrt() / settled.len() as f64;
        assert!(
            mean_magnitude > 0.999,
            "centered magnitude {mean_magnitude}"
        );
    }

    #[test]
    fn processing_is_chunk_boundary_invariant_and_count_is_exact() {
        let input = tone(3_431_250.0, 12_347);
        let mut whole = ChannelProcessor::new(18, 2_437_000_000, false).unwrap();
        let expected = process(&mut whole, &input);
        let mut chunked = ChannelProcessor::new(18, 2_437_000_000, false).unwrap();
        let mut actual = Vec::new();
        for chunk in input.chunks(113) {
            actual.extend(process(&mut chunked, chunk));
        }
        assert_eq!(expected, actual);
        assert_eq!(actual.len(), input.len() / DECIMATION);
    }

    #[test]
    fn filter_preserves_desired_tone_and_rejects_decimation_alias() {
        let count = 120_000;
        let mut desired = ChannelProcessor::new(18, 2_437_000_000, false).unwrap();
        let desired_output = process(&mut desired, &tone(3_000_000.0 + 1_000_000.0, count));
        let mut alias = ChannelProcessor::new(18, 2_437_000_000, false).unwrap();
        let alias_output = process(&mut alias, &tone(3_000_000.0 + 4_000_000.0, count));
        let mut adjacent = ChannelProcessor::new(18, 2_437_000_000, false).unwrap();
        let adjacent_output = process(&mut adjacent, &tone(3_000_000.0 + 5_000_000.0, count));
        let skip = (FILTER_TAPS - 1).div_ceil(DECIMATION) + 8;
        let desired_rms = rms(&desired_output[skip..]);
        let alias_rms = rms(&alias_output[skip..]);
        let adjacent_rms = rms(&adjacent_output[skip..]);
        let desired_db = 20.0 * desired_rms.log10();
        let rejection_db = 20.0 * (alias_rms / desired_rms).log10();
        let adjacent_rejection_db = 20.0 * (adjacent_rms / desired_rms).log10();
        assert!(desired_db.abs() < 0.05, "desired response {desired_db} dB");
        assert!(
            rejection_db < -60.0,
            "alias rejection was only {rejection_db} dB"
        );
        assert!(
            adjacent_rejection_db < -60.0,
            "adjacent-channel rejection was only {adjacent_rejection_db} dB"
        );
    }

    #[test]
    fn validates_sidecars_framing_q11_and_sample_limits() {
        let directory = tempfile::tempdir().unwrap();
        let input = write_capture(directory.path(), &[(0, 0); 10]);
        let (metadata, _, _) = read_acquisition(&input).unwrap();
        assert_eq!(metadata.num_samples, 10);

        fs::write(
            append_json(&input),
            vec![b' '; MAX_METADATA_BYTES as usize + 1],
        )
        .unwrap();
        assert!(read_acquisition(&input).is_err());
        fs::write(append_json(&input), b"{").unwrap();
        assert!(read_acquisition(&input).is_err());

        let partial_dir = tempfile::tempdir().unwrap();
        let partial = write_capture(partial_dir.path(), &[(0, 0); 10]);
        OpenOptions::new()
            .append(true)
            .open(&partial)
            .unwrap()
            .write_all(&[0])
            .unwrap();
        assert!(
            run(Request {
                input: partial,
                output: partial_dir.path().join("out"),
                channels: vec![17],
                max_samples: 10,
            })
            .is_err()
        );

        let bad_dir = tempfile::tempdir().unwrap();
        let bad = write_capture(bad_dir.path(), &[(2048, 0); 10]);
        assert!(
            run(Request {
                input: bad,
                output: bad_dir.path().join("out"),
                channels: vec![17],
                max_samples: 10,
            })
            .is_err()
        );
        let limit_dir = tempfile::tempdir().unwrap();
        let limit_input = write_capture(limit_dir.path(), &[(0, 0); 10]);
        assert!(
            run(Request {
                input: limit_input,
                output: limit_dir.path().join("too-small"),
                channels: vec![17],
                max_samples: 4,
            })
            .is_err()
        );
    }

    #[test]
    fn processes_only_the_bounded_prefix_and_reports_its_digest() {
        let directory = tempfile::tempdir().unwrap();
        let samples = (0..11)
            .map(|index| (index * 10, -(index * 10)))
            .collect::<Vec<_>>();
        let input = write_capture(directory.path(), &samples);
        let raw = fs::read(&input).unwrap();
        let output = directory.path().join("prefix");
        run(Request {
            input,
            output: output.clone(),
            channels: vec![17],
            max_samples: 7,
        })
        .unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("channelize.json")).unwrap()).unwrap();
        assert_eq!(manifest["source"]["available_complex_samples"], 11);
        assert_eq!(manifest["source"]["processed_complex_samples"], 7);
        assert_eq!(manifest["source"]["processed_full_input"], false);
        assert_eq!(manifest["source"]["processed_prefix_bytes"], 28);
        assert_eq!(
            manifest["source"]["processed_prefix_sha256"],
            format!("{:x}", Sha256::digest(&raw[..28]))
        );
        assert_eq!(
            fs::metadata(output.join("channel-17.cf32")).unwrap().len(),
            8
        );
        assert!(!output.join("channelize.json.partial").exists());
    }

    #[test]
    fn publishes_private_outputs_without_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let samples = (0..1000)
            .map(|index| {
                if index % 2 == 0 {
                    (100, -100)
                } else {
                    (-100, 100)
                }
            })
            .collect::<Vec<_>>();
        let input = write_capture(directory.path(), &samples);
        let output = directory.path().join("out");
        run(Request {
            input: input.clone(),
            output: output.clone(),
            channels: vec![17, 18],
            max_samples: 1000,
        })
        .unwrap();
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["channel-17.cf32", "channel-18.cf32", "channelize.json"] {
            assert_eq!(
                fs::metadata(output.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let second = directory.path().join("second");
        run(Request {
            input: input.clone(),
            output: second.clone(),
            channels: vec![17, 18],
            max_samples: 1000,
        })
        .unwrap();
        for name in ["channel-17.cf32", "channel-18.cf32"] {
            assert_eq!(
                fs::read(output.join(name)).unwrap(),
                fs::read(second.join(name)).unwrap()
            );
        }
        let first_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("channelize.json")).unwrap()).unwrap();
        let second_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(second.join("channelize.json")).unwrap()).unwrap();
        assert_eq!(
            first_manifest["channels"][0]["sha256"],
            second_manifest["channels"][0]["sha256"]
        );
        assert_eq!(first_manifest["channels"][0]["channel"], 17);
        let before = fs::read(output.join("channelize.json")).unwrap();
        assert!(
            run(Request {
                input,
                output: output.clone(),
                channels: vec![17, 18],
                max_samples: 1000,
            })
            .is_err()
        );
        assert_eq!(fs::read(output.join("channelize.json")).unwrap(), before);
    }

    #[test]
    fn multiple_channels_match_direct_processing_and_keep_request_order() {
        let directory = tempfile::tempdir().unwrap();
        let samples: Vec<_> = (0..IO_CHUNK_BYTES / 4 + 113)
            .map(|index| {
                let ramp = (index % 4096) as i16;
                (ramp - 2048, 2047 - ramp)
            })
            .collect();
        let input = write_capture(directory.path(), &samples);
        let output = directory.path().join("parallel");
        let channels = [18, 16, 17];
        let reads_manifest = |root: &Path| -> serde_json::Value {
            serde_json::from_slice(&fs::read(root.join("channelize.json")).unwrap()).unwrap()
        };
        run(Request {
            input: input.clone(),
            output: output.clone(),
            channels: channels.to_vec(),
            max_samples: samples.len() as u64,
        })
        .unwrap();
        let manifest = reads_manifest(&output);
        for (index, channel) in channels.into_iter().enumerate() {
            let direct = directory.path().join(format!("direct-{channel}"));
            run(Request {
                input: input.clone(),
                output: direct.clone(),
                channels: vec![channel],
                max_samples: samples.len() as u64,
            })
            .unwrap();
            let expected = reads_manifest(&direct);
            assert_eq!(manifest["source"], expected["source"]);
            assert_eq!(manifest["channels"][index], expected["channels"][0]);
            let name = format!("channel-{channel}.cf32");
            assert_eq!(
                fs::read(output.join(&name)).unwrap(),
                fs::read(direct.join(name)).unwrap()
            );
        }
    }

    #[test]
    fn chunked_edge_output_matches_scalar_bytes_digest_and_statistics() {
        let directory = tempfile::tempdir().unwrap();
        let samples: Vec<_> = (0..2 * (IO_CHUNK_BYTES / 4) + 37)
            .map(|index| {
                let ramp = (index % 4096) as i16;
                (ramp - 2048, 2047 - ramp)
            })
            .collect();
        let input = write_capture(directory.path(), &samples);
        let output = directory.path().join("parallel");
        run_allowing_nyquist_edge(Request {
            input,
            output: output.clone(),
            channels: vec![19, 17],
            max_samples: samples.len() as u64,
        })
        .unwrap();
        let manifest = fs::read(output.join("channelize.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
        let samples: Vec<_> = samples
            .into_iter()
            .map(|(re, im)| Complex {
                re: f32::from(re) / Q11_SCALE,
                im: f32::from(im) / Q11_SCALE,
            })
            .collect();
        for (index, channel) in [19, 17].into_iter().enumerate() {
            let mut processor = ChannelProcessor::new(channel, 2_437_000_000, true).unwrap();
            let reference = process(&mut processor, &samples);
            let mut encoded = Vec::new();
            for sample in &reference {
                encoded.extend_from_slice(&sample.re.to_le_bytes());
                encoded.extend_from_slice(&sample.im.to_le_bytes());
            }
            let stream = fs::read(output.join(format!("channel-{channel}.cf32"))).unwrap();
            assert_eq!(stream, encoded);
            let report = &manifest["channels"][index];
            assert_eq!(report["channel"], channel);
            assert_eq!(report["output_complex_samples"], reference.len());
            assert_eq!(report["sha256"], format!("{:x}", Sha256::digest(&encoded)));
            let peak = reference
                .iter()
                .map(|sample| sample.norm_sqr().sqrt())
                .fold(0.0, f32::max);
            assert!(
                (report["relative_peak"].as_f64().unwrap() - f64::from(peak)).abs() <= f64::EPSILON
            );
            assert!(
                (report["relative_rms"].as_f64().unwrap() - rms(&reference)).abs() <= f64::EPSILON
            );
        }
    }

    #[test]
    fn invalid_later_chunk_leaves_only_partial_channel_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let invalid_index = IO_CHUNK_BYTES / 4 + 16;
        let mut samples = vec![(500, -200); invalid_index + 21];
        samples[invalid_index] = (2048, 0);
        let input = write_capture(directory.path(), &samples);
        for channels in [vec![16], vec![16, 18]] {
            let output = directory.path().join(format!("invalid-{}", channels.len()));
            let error = run(Request {
                input: input.clone(),
                output: output.clone(),
                channels: channels.clone(),
                max_samples: samples.len() as u64,
            })
            .unwrap_err();
            assert!(error.to_string().contains("outside the SC16_Q11 range"));
            assert!(!output.join("channelize.json").exists());
            assert!(!output.join("channelize.json.partial").exists());
            for channel in channels {
                assert!(!output.join(format!("channel-{channel}.cf32")).exists());
                let partial = output.join(format!("channel-{channel}.cf32.partial"));
                assert_eq!(
                    fs::metadata(partial).unwrap().len(),
                    (invalid_index / DECIMATION * 8) as u64
                );
            }
        }
    }

    #[test]
    fn arbitrary_frequency_run_attests_inputs_mapping_and_private_output() {
        let directory = tempfile::tempdir().unwrap();
        let samples = (0..1003)
            .map(|index| {
                let phase = std::f64::consts::TAU * 1_000_000.0 * index as f64
                    / SOURCE_SAMPLE_RATE_HZ as f64;
                (
                    (phase.cos() * 1024.0).round() as i16,
                    (phase.sin() * 1024.0).round() as i16,
                )
            })
            .collect::<Vec<_>>();
        let input = write_capture(directory.path(), &samples);
        let sidecar = append_json(&input);
        let output = directory.path().join("frequency");
        run_frequency(FrequencyRequest {
            input: input.clone(),
            output: output.clone(),
            frequency_hz: 2_438_000_000,
            max_samples: 1003,
        })
        .unwrap();

        let stream = output.join("frequency-2438000000.cf32");
        let manifest_path = output.join("channelize-frequency.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 1);
        assert_eq!(manifest["operation"], "offline_frequency_channelization");
        assert_eq!(manifest["target"]["center_frequency_hz"], 2_438_000_000u64);
        assert_eq!(manifest["target"]["source_offset_hz"], 1_000_000);
        assert_eq!(manifest["target"]["output_complex_samples"], 200);
        assert_eq!(manifest["filter"]["decimation"], 5);
        assert_eq!(manifest["filter"]["group_delay_source_samples"], 112);
        assert_eq!(manifest["filter"]["startup_transient_output_samples"], 44);
        assert_eq!(manifest["filter"]["tail_flushed"], false);
        assert_eq!(
            manifest["sample_index_mapping"]["latest_source_index_for_output_zero"],
            4
        );
        assert_eq!(
            manifest["sample_index_mapping"]["group_delay_corrected_source_index_for_output_zero"],
            -108
        );
        assert_eq!(
            manifest["acquisition_sidecar"]["sha256"],
            format!("{:x}", Sha256::digest(fs::read(&sidecar).unwrap()))
        );
        assert_eq!(
            manifest["target"]["sha256"],
            format!("{:x}", Sha256::digest(fs::read(&stream).unwrap()))
        );
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for path in [&stream, &manifest_path] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let before = fs::read(&manifest_path).unwrap();
        assert!(
            run_frequency(FrequencyRequest {
                input,
                output: output.clone(),
                frequency_hz: 2_438_000_000,
                max_samples: 1003,
            })
            .is_err()
        );
        assert_eq!(fs::read(manifest_path).unwrap(), before);
    }
}
