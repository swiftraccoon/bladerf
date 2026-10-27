//! Bounded, receive-only device operations using the safe SoapySDR wrapper.
//!
//! Native calls are synchronous. Every activated stream is explicitly deactivated
//! on success and recoverable errors; the dependency closes stream/device handles
//! on drop. The wrapper exposes no fallible explicit close operation, so close
//! errors are not reported to the caller and native teardown may block.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use chrono::{SecondsFormat, Utc};
use clap::Args;
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use soapysdr::{Device, Direction::Rx, Format, RxStream, StreamSample};

const MAX_CAPTURE_SAMPLES: usize = 250_000_000;
const CHUNK_SAMPLES: usize = 65_536;
const READ_TIMEOUT_US: i64 = 2_000_000;

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct FrequencyHz(f64);

impl TryFrom<f64> for FrequencyHz {
    type Error = anyhow::Error;

    fn try_from(hz: f64) -> Result<Self> {
        ensure!(
            hz.is_finite() && (70e6..=6e9).contains(&hz),
            "frequency must be finite and in 70 MHz through 6 GHz"
        );
        Ok(Self(hz))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct SampleRateHz(f64);

impl TryFrom<f64> for SampleRateHz {
    type Error = anyhow::Error;

    fn try_from(hz: f64) -> Result<Self> {
        ensure!(
            hz.is_finite() && (520_834.0..=61.44e6).contains(&hz),
            "sample rate must be finite and in 520834 through 61440000 S/s"
        );
        Ok(Self(hz))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
struct DurationSeconds(f64);

impl TryFrom<f64> for DurationSeconds {
    type Error = anyhow::Error;

    fn try_from(seconds: f64) -> Result<Self> {
        ensure!(
            seconds.is_finite() && seconds > 0.0 && seconds <= 60.0,
            "duration must be finite, greater than zero, and at most 60 seconds"
        );
        Ok(Self(seconds))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SampleCount(usize);

impl SampleCount {
    fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for SampleCount {
    type Error = anyhow::Error;

    fn try_from(count: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_CAPTURE_SAMPLES).contains(&count),
            "capture must contain 1 through {MAX_CAPTURE_SAMPLES} samples (at most 1 GB)"
        );
        Ok(Self(count))
    }
}

#[derive(Args, Debug)]
pub(crate) struct RadioArgs {
    /// Receive center frequency in Hz, 70e6 through 6e9.
    #[arg(long, default_value_t = 100e6)]
    frequency: f64,
    /// Complex samples per second, 520834 through 61.44e6.
    #[arg(long, default_value_t = 2e6)]
    sample_rate: f64,
    /// Manual receive gain in dB; checked against the device's reported range.
    #[arg(long, default_value_t = 20.0, allow_negative_numbers = true)]
    gain: f64,
    /// Physical receive port, RX1 or RX2.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=2))]
    channel: u8,
    /// Optional bladeRF hexadecimal serial; required if multiple devices match.
    #[arg(long)]
    serial: Option<String>,
}

impl RadioArgs {
    fn validate(&self) -> Result<()> {
        FrequencyHz::try_from(self.frequency)?;
        SampleRateHz::try_from(self.sample_rate)?;
        ensure!(self.gain.is_finite(), "gain must be finite");
        ensure!((1..=2).contains(&self.channel), "channel must be 1 or 2");
        if let Some(serial) = &self.serial {
            ensure!(
                !serial.is_empty()
                    && serial.len() <= 32
                    && serial.bytes().all(|b| b.is_ascii_hexdigit()),
                "serial must contain 1 through 32 hexadecimal digits"
            );
        }
        Ok(())
    }

    fn index(&self) -> usize {
        usize::from(self.channel - 1)
    }

    fn open(&self) -> Result<Device> {
        self.validate()?;
        let mut selector = soapysdr::Args::new();
        selector.set("driver", "bladerf");
        if let Some(serial) = &self.serial {
            selector.set("serial", serial.as_str());
        }
        let mut devices = soapysdr::enumerate(selector).context("enumerate bladeRF devices")?;
        ensure!(
            devices.len() == 1,
            "expected one bladeRF, found {}; select --serial when multiple devices are attached",
            devices.len()
        );
        let args = devices.pop().context("no bladeRF device found")?;
        let device = Device::new(args).context("open bladeRF")?;
        ensure!(
            device.hardware_key()? == "bladerf2",
            "this tool requires a bladeRF 2.0 micro"
        );
        ensure!(
            device.num_channels(Rx)? > self.index(),
            "requested RX port is unavailable"
        );
        let (format, scale) = device.native_stream_format(Rx, self.index())?;
        ensure!(
            format == Format::CS16 && scale == 2048.0,
            "expected native CS16 with full scale 2048; got {format} with scale {scale}"
        );
        device
            .write_setting("biastee_rx", "false")
            .context("disable RX bias tee")?;
        device
            .write_setting("loopback", "none")
            .context("disable internal loopback")?;
        let sample_rate = SampleRateHz::try_from(self.sample_rate)?;
        let frequency = FrequencyHz::try_from(self.frequency)?;
        device.set_sample_rate(Rx, self.index(), sample_rate.0)?;
        device.set_frequency(Rx, self.index(), frequency.0, ())?;
        device.set_bandwidth(Rx, self.index(), sample_rate.0.min(56e6))?;
        device.set_gain_mode(Rx, self.index(), false)?;
        let range = device.gain_range(Rx, self.index())?;
        ensure!(
            self.gain >= range.minimum && self.gain <= range.maximum,
            "gain must be in {} through {} dB for this device",
            range.minimum,
            range.maximum
        );
        device.set_gain(Rx, self.index(), self.gain)?;
        Ok(device)
    }
}

#[derive(Args, Debug)]
pub(crate) struct CaptureArgs {
    #[command(flatten)]
    radio: RadioArgs,
    /// Duration in seconds, greater than zero and at most 60; capture capped at 1 GB.
    #[arg(long, default_value_t = 1.0)]
    duration: f64,
    /// IQ output; OUTPUT.json is the metadata sidecar. Existing paths are refused.
    #[arg(long, default_value = "captures/rx1.sc16q11")]
    output: PathBuf,
}

#[derive(Args, Debug)]
pub(crate) struct SoapyRxArgs {
    #[command(flatten)]
    radio: RadioArgs,
    /// Number of complex samples; maximum 10 million and 60 seconds.
    #[arg(long, default_value_t = 200_000)]
    samples: usize,
}

fn sample_count(rate: SampleRateHz, duration: DurationSeconds) -> Result<SampleCount> {
    let samples = (rate.0 * duration.0).round();
    ensure!(
        samples >= 1.0 && samples <= MAX_CAPTURE_SAMPLES as f64,
        "capture must contain 1 through {MAX_CAPTURE_SAMPLES} samples (at most 1 GB)"
    );
    SampleCount::try_from(samples as usize)
}

fn interrupt_flag() -> Result<Arc<AtomicBool>> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .context("install interruption handler")?;
    Ok(interrupted)
}

/// Explicitly stop even when receive or file output fails; retain both failures.
fn with_active_stream<E: StreamSample, T>(
    stream: &mut RxStream<E>,
    operation: impl FnOnce(&mut RxStream<E>) -> Result<T>,
) -> Result<T> {
    stream.activate(None).context("activate receive stream")?;
    let result = operation(stream);
    let shutdown = stream.deactivate(None).context("deactivate receive stream");
    match (result, shutdown) {
        (Err(primary), Err(shutdown)) => {
            Err(primary.context(format!("stream shutdown also failed: {shutdown:#}")))
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(shutdown)) => Err(shutdown),
        (Ok(value), Ok(())) => Ok(value),
    }
}

/// Keep incomplete output in private temporary files until streaming and sync pass.
/// Final paths are never overwritten or unlinked during failure cleanup.
#[derive(Debug)]
struct CaptureFiles {
    raw: BufWriter<tempfile::NamedTempFile>,
    metadata: BufWriter<tempfile::NamedTempFile>,
    raw_path: PathBuf,
    metadata_path: PathBuf,
}

fn require_absent(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(_) => anyhow::bail!("output already exists: {}", path.display()),
    }
}

fn publish_capture(
    raw: tempfile::NamedTempFile,
    metadata: tempfile::NamedTempFile,
    raw_path: &Path,
    metadata_path: &Path,
) -> Result<()> {
    raw.persist_noclobber(raw_path)
        .map_err(|error| error.error)
        .with_context(|| format!("publish new IQ file {}", raw_path.display()))?;
    // Two filenames cannot be committed in one rename. On a concurrent sidecar
    // collision, retain the complete IQ file for recovery rather than unlinking
    // a path another process might now own. Report failure, never success.
    metadata
        .persist_noclobber(metadata_path)
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "publish new sidecar {}; complete IQ retained at {}",
                metadata_path.display(),
                raw_path.display()
            )
        })?;
    Ok(())
}

impl CaptureFiles {
    fn create(output: &Path) -> Result<Self> {
        let mut metadata_path = output.as_os_str().to_os_string();
        metadata_path.push(".json");
        let metadata_path = PathBuf::from(metadata_path);
        require_absent(output)?;
        require_absent(&metadata_path)?;
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).context("create capture directory")?;
        let raw = tempfile::Builder::new()
            .prefix(".bladerf-iq-")
            .tempfile_in(parent)?;
        let metadata = tempfile::Builder::new()
            .prefix(".bladerf-meta-")
            .tempfile_in(parent)?;
        Ok(Self {
            raw: BufWriter::new(raw),
            metadata: BufWriter::new(metadata),
            raw_path: output.to_path_buf(),
            metadata_path,
        })
    }

    fn complete(mut self, settings: &CaptureMetadata) -> Result<()> {
        self.raw.flush().context("flush IQ output")?;
        self.raw
            .get_ref()
            .as_file()
            .sync_all()
            .context("sync IQ output")?;
        serde_json::to_writer_pretty(&mut self.metadata, settings)?;
        self.metadata.write_all(b"\n")?;
        self.metadata.flush().context("flush metadata")?;
        self.metadata
            .get_ref()
            .as_file()
            .sync_all()
            .context("sync metadata")?;
        let raw = self.raw.into_inner().map_err(|error| error.into_error())?;
        let metadata = self
            .metadata
            .into_inner()
            .map_err(|error| error.into_error())?;
        publish_capture(raw, metadata, &self.raw_path, &self.metadata_path)
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CaptureMetadata {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: u16,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    gain_db: f64,
    gain_mode: String,
    bias_tee_enabled: bool,
    channel: String,
    device_serial: Option<String>,
    firmware_version: Option<String>,
    fpga_version: Option<String>,
    num_samples: usize,
    duration_seconds: f64,
    started_utc: String,
    timestamp_note: String,
    continuity_note: String,
    receive_api: String,
}

fn encode_iq(samples: &[Complex<i16>], bytes: &mut Vec<u8>) -> Result<()> {
    bytes.clear();
    for sample in samples {
        ensure!(
            (-2048..=2047).contains(&sample.re) && (-2048..=2047).contains(&sample.im),
            "received a component outside the SC16_Q11 range"
        );
        bytes.extend_from_slice(&sample.re.to_le_bytes());
        bytes.extend_from_slice(&sample.im.to_le_bytes());
    }
    Ok(())
}

pub(crate) fn capture(args: CaptureArgs) -> Result<()> {
    args.radio.validate()?;
    let duration = DurationSeconds::try_from(args.duration)?;
    sample_count(SampleRateHz::try_from(args.radio.sample_rate)?, duration)?;
    let interrupted = interrupt_flag()?;
    let mut files = CaptureFiles::create(&args.output)?;
    let device = args.radio.open()?;
    let rate = SampleRateHz::try_from(device.sample_rate(Rx, args.radio.index())?)?;
    let count = sample_count(rate, duration)?.get();
    let info = device.hardware_info()?;
    let mut settings = CaptureMetadata {
        format: "SC16_Q11".into(),
        dtype: "<i2".into(),
        interleave: "I,Q".into(),
        complex_scale: 2048,
        frequency_hz: device.frequency(Rx, args.radio.index())?,
        sample_rate_hz: rate.0,
        bandwidth_hz: device.bandwidth(Rx, args.radio.index())?,
        gain_db: 0.0,
        gain_mode: "manual".into(),
        bias_tee_enabled: false,
        channel: format!("RX{}", args.radio.channel),
        device_serial: info.get("serial").map(str::to_owned),
        firmware_version: info.get("fw_version").map(str::to_owned),
        fpga_version: info.get("fpga_version").map(str::to_owned),
        num_samples: count,
        duration_seconds: count as f64 / rate.0,
        started_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true),
        timestamp_note: "Host wall-clock before RX enable, not a hardware timestamp".into(),
        continuity_note:
            "Untimestamped SC16_Q11 output does not certify absence of dropped samples".into(),
        receive_api: "Rust soapysdr -> SoapyBladeRF -> libbladeRF".into(),
    };
    let mut stream = device.rx_stream::<Complex<i16>>(&[args.radio.index()])?;
    let mut buffer = vec![Complex::new(0i16, 0); CHUNK_SAMPLES];
    let mut encoded = Vec::with_capacity(CHUNK_SAMPLES * 4);
    let deadline = Instant::now() + Duration::from_secs_f64(duration.0 + 15.0);
    with_active_stream(&mut stream, |stream| {
        settings.gain_db = device.gain(Rx, args.radio.index())?;
        let mut remaining = count;
        while remaining > 0 {
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "capture interrupted; incomplete output removed"
            );
            ensure!(
                Instant::now() < deadline,
                "capture exceeded its wall-clock deadline"
            );
            let wanted = remaining.min(buffer.len());
            let received = stream
                .read(&mut [&mut buffer[..wanted]], READ_TIMEOUT_US)
                .context("receive IQ")?;
            ensure!(
                received > 0 && received <= wanted,
                "receive returned invalid count {received}"
            );
            encode_iq(&buffer[..received], &mut encoded)?;
            files.raw.write_all(&encoded).context("write IQ samples")?;
            remaining -= received;
        }
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "capture interrupted; incomplete output removed"
        );
        Ok(())
    })?;
    files.complete(&settings)?;
    println!(
        "Captured {count} complex samples from RX{} at {} S/s\nIQ: {}\nMetadata: {}.json",
        args.radio.channel,
        rate.0,
        args.output.display(),
        args.output.display()
    );
    Ok(())
}

pub(crate) fn soapy_rx(args: SoapyRxArgs) -> Result<()> {
    args.radio.validate()?;
    ensure!(
        (1..=10_000_000).contains(&args.samples),
        "samples must be 1 through 10000000"
    );
    ensure!(
        args.samples as f64 / args.radio.sample_rate <= 60.0,
        "receive duration exceeds 60 seconds"
    );
    let interrupted = interrupt_flag()?;
    let device = args.radio.open()?;
    let rate = SampleRateHz::try_from(device.sample_rate(Rx, args.radio.index())?)?;
    let duration = DurationSeconds::try_from(args.samples as f64 / rate.0)?;
    let mut stream = device.rx_stream::<Complex<f32>>(&[args.radio.index()])?;
    let mut buffer = vec![Complex::new(0.0f32, 0.0); CHUNK_SAMPLES];
    let mut energy = 0.0;
    let deadline = Instant::now() + Duration::from_secs_f64(duration.0 + 15.0);
    with_active_stream(&mut stream, |stream| {
        let mut remaining = args.samples;
        while remaining > 0 {
            ensure!(!interrupted.load(Ordering::Relaxed), "receive interrupted");
            ensure!(
                Instant::now() < deadline,
                "receive exceeded its wall-clock deadline"
            );
            let wanted = remaining.min(buffer.len());
            let received = stream
                .read(&mut [&mut buffer[..wanted]], READ_TIMEOUT_US)
                .context("receive CF32")?;
            ensure!(
                received > 0 && received <= wanted,
                "receive returned invalid count {received}"
            );
            for sample in &buffer[..received] {
                ensure!(
                    sample.re.is_finite() && sample.im.is_finite(),
                    "received non-finite IQ"
                );
                energy += f64::from(sample.re).powi(2) + f64::from(sample.im).powi(2);
            }
            remaining -= received;
        }
        Ok(())
    })?;
    ensure!(energy > 0.0, "received only zero samples");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "PASS", "api": "Rust SoapySDR", "channel": format!("RX{}", args.radio.channel),
            "complex_samples": args.samples, "sample_rate_hz": rate.0,
            "frequency_hz": device.frequency(Rx, args.radio.index())?, "bias_tee_enabled": false,
            "rms_dbfs": 10.0 * (energy / args.samples as f64).log10(),
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbounded_or_empty_capture_before_hardware() {
        let rate = SampleRateHz::try_from(2e6).unwrap();
        for duration in [0.0, -1.0, f64::NAN, f64::INFINITY, 60.1] {
            assert!(DurationSeconds::try_from(duration).is_err());
        }
        assert!(
            sample_count(
                SampleRateHz::try_from(61.44e6).unwrap(),
                DurationSeconds::try_from(60.0).unwrap()
            )
            .is_err()
        );
        assert!(sample_count(rate, DurationSeconds::try_from(1e-9).unwrap()).is_err());
        assert_eq!(
            sample_count(rate, DurationSeconds::try_from(1.0).unwrap())
                .unwrap()
                .get(),
            2_000_000
        );
    }

    #[test]
    fn encodes_known_signed_iq_little_endian_and_checks_q11_range() {
        let mut bytes = Vec::new();
        encode_iq(
            &[Complex::new(-2048, 2047), Complex::new(1, -1)],
            &mut bytes,
        )
        .unwrap();
        assert_eq!(bytes, [0, 248, 255, 7, 1, 0, 255, 255]);
        assert!(encode_iq(&[Complex::new(2048, 0)], &mut bytes).is_err());
    }

    #[test]
    fn failure_cleanup_removes_only_own_reserved_capture() {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path().join("capture.sc16q11");
        let sidecar = directory.path().join("capture.sc16q11.json");
        {
            let mut files = CaptureFiles::create(&raw).unwrap();
            files.raw.write_all(b"partial").unwrap();
        }
        assert!(!raw.exists());
        assert!(!sidecar.exists());
        std::fs::write(&sidecar, b"existing metadata").unwrap();
        assert!(CaptureFiles::create(&raw).is_err());
        assert!(!raw.exists());
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"existing metadata");
        std::fs::write(&raw, b"existing samples").unwrap();
        assert!(CaptureFiles::create(&raw).is_err());
        assert_eq!(std::fs::read(&raw).unwrap(), b"existing samples");
    }

    #[test]
    fn concurrent_output_creation_is_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let raw_path = directory.path().join("capture.sc16q11");
        let meta_path = directory.path().join("capture.sc16q11.json");
        let mut files = CaptureFiles::create(&raw_path).unwrap();
        files.raw.write_all(b"our samples").unwrap();
        files.raw.flush().unwrap();
        files.metadata.write_all(b"our metadata").unwrap();
        files.metadata.flush().unwrap();
        std::fs::write(&raw_path, b"concurrent capture").unwrap();
        let raw = files.raw.into_inner().unwrap();
        let metadata = files.metadata.into_inner().unwrap();
        assert!(publish_capture(raw, metadata, &raw_path, &meta_path).is_err());
        assert_eq!(std::fs::read(&raw_path).unwrap(), b"concurrent capture");
        assert!(!meta_path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn sidecar_publication_collision_retains_complete_iq_and_existing_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let raw_path = directory.path().join("capture.sc16q11");
        let meta_path = directory.path().join("capture.sc16q11.json");
        let mut raw = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let mut metadata = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        raw.write_all(b"complete IQ").unwrap();
        metadata.write_all(b"our metadata").unwrap();
        std::fs::write(&meta_path, b"concurrent metadata").unwrap();
        let error = publish_capture(raw, metadata, &raw_path, &meta_path).unwrap_err();
        assert!(error.to_string().contains("complete IQ retained"));
        assert_eq!(std::fs::read(&raw_path).unwrap(), b"complete IQ");
        assert_eq!(std::fs::read(&meta_path).unwrap(), b"concurrent metadata");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
