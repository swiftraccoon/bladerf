//! Passive Wi-Fi capture, bounded IQ replay, and independent frame checks.
//!
//! Run `cargo run --locked -p wifi-monitor -- --help`. Packet identities and
//! lab observations belong in local captures, not shared documentation.

#![forbid(unsafe_code)]

#[cfg(feature = "decode")]
mod decode;
mod inventory;
mod monitor;
mod packets;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::net::UdpSocket;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(about = "Passive Wi-Fi monitoring with independent 802.11 FCS checks")]
struct Cli {
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    /// Existing bladeRF lab acquisition binary, or FutureSDR rx binary for its engine.
    #[arg(long, global = true)]
    receiver: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Check software and source-patch availability without opening the radio.
    Doctor {
        #[arg(long, value_enum, default_value = "crafter")]
        engine: Engine,
    },
    /// Capture a Wi-Fi channel; the default records IQ then decodes offline.
    Capture(Capture),
    /// Survey selected Wi-Fi channels and build an observed-address inventory.
    Survey(monitor::Survey),
    /// Repeat fixed-channel captures with bounded session retention.
    Watch(monitor::Watch),
    /// Build an inventory from saved PCAP files without opening a radio.
    Inventory(monitor::InventoryArgs),
    /// Inspect classic PCAP IEEE802.11/radiotap without an SDR or PHY decoder.
    Inspect {
        input: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Decode an existing 20 MSps IQ file in a bounded worker, without a radio.
    DecodeIq(Replay),
    #[command(hide = true)]
    DecodeWorker(Worker),
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
enum Engine {
    Crafter,
    Futuresdr,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Band {
    #[value(name = "2g")]
    TwoGhz,
    #[value(name = "5g")]
    FiveGhz,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Antenna {
    Rx1,
    Rx2,
}

impl Antenna {
    fn channel(self) -> u8 {
        match self {
            Self::Rx1 => 1,
            Self::Rx2 => 2,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Rx1 => "RX1",
            Self::Rx2 => "RX2",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum InputFormat {
    Sc16q11,
    Cs8,
}

#[derive(Clone, Debug, Args)]
struct Capture {
    #[command(flatten)]
    radio: Receive,
    /// 20 MHz channel number. 2.4 GHz defaults to 1; 5 GHz defaults to 36.
    #[arg(long)]
    channel: Option<u16>,
    /// New session directory, relative to the workspace unless absolute.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
struct Receive {
    #[arg(long, value_enum, default_value = "crafter")]
    engine: Engine,
    #[arg(long, value_enum, default_value = "2g")]
    band: Band,
    #[arg(long, value_enum, default_value = "rx1")]
    antenna: Antenna,
    #[arg(long, default_value_t = 40.0)]
    gain: f64,
    /// Sample duration (default 1s), or live wall-time window (default 15s).
    #[arg(long)]
    duration: Option<f64>,
    #[arg(long, default_value_t = 120.0)]
    decode_timeout: f64,
    #[arg(long, default_value_t = 256)]
    iq_limit_mib: u32,
    /// Optional hexadecimal device serial for bounded-IQ acquisition.
    #[arg(long)]
    serial: Option<String>,
}

#[derive(Debug, Args)]
struct Replay {
    input: PathBuf,
    #[arg(long, value_enum, default_value = "sc16q11")]
    format: InputFormat,
    #[arg(long, default_value_t = 2412)]
    frequency_mhz: u32,
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
    #[arg(long, default_value_t = 120.0)]
    decode_timeout: f64,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Worker {
    input: PathBuf,
    #[arg(long)]
    pcap: PathBuf,
    #[arg(long, value_enum)]
    format: InputFormat,
    #[arg(long)]
    frequency_hz: u64,
    #[arg(long)]
    max_samples: u64,
    #[arg(long)]
    time_origin_ns: Option<u128>,
}

fn frequency(band: Band, channel: Option<u16>) -> Result<(u16, u64)> {
    let channel = channel.unwrap_or(match band {
        Band::TwoGhz => 1,
        Band::FiveGhz => 36,
    });
    match band {
        Band::TwoGhz => {
            ensure!((1..=13).contains(&channel), "2g channel must be 1..=13");
            Ok((channel, (2407 + u64::from(channel) * 5) * 1_000_000))
        }
        Band::FiveGhz => {
            ensure!(
                [
                    36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132,
                    136, 140, 144, 149, 153, 157, 161, 165
                ]
                .contains(&channel),
                "unsupported 20 MHz 5g channel"
            );
            Ok((channel, (5000 + u64::from(channel) * 5) * 1_000_000))
        }
    }
}

fn positive_seconds(value: f64, maximum: f64, label: &str) -> Result<Duration> {
    ensure!(
        value.is_finite() && value > 0.0 && value <= maximum,
        "{label} must be finite, positive and at most {maximum} seconds"
    );
    Ok(Duration::from_secs_f64(value))
}

impl Receive {
    fn duration(&self) -> f64 {
        self.duration.unwrap_or(match self.engine {
            Engine::Crafter => 1.0,
            Engine::Futuresdr => 15.0,
        })
    }

    fn validate(&self) -> Result<()> {
        positive_seconds(
            self.duration(),
            match self.engine {
                Engine::Crafter => 10.0,
                Engine::Futuresdr => 300.0,
            },
            "duration",
        )?;
        positive_seconds(self.decode_timeout, 3600.0, "decode timeout")?;
        ensure!(
            self.gain.is_finite() && (0.0..=71.0).contains(&self.gain),
            "manual gain must be finite and between 0 and 71 dB"
        );
        ensure!(
            (1..=1024).contains(&self.iq_limit_mib),
            "IQ limit must be 1..=1024 MiB"
        );
        if let Some(serial) = &self.serial {
            ensure!(
                !serial.is_empty()
                    && serial.len() <= 32
                    && serial.bytes().all(|c| c.is_ascii_hexdigit()),
                "serial must contain 1..=32 hexadecimal digits"
            );
            ensure!(
                matches!(self.engine, Engine::Crafter),
                "serial selection is available with the bounded-IQ engine"
            );
        }
        if matches!(self.engine, Engine::Crafter) {
            let samples = (self.duration() * 20_000_000.0).round();
            ensure!(samples >= 1.0, "duration must request at least one sample");
            ensure!(
                samples * 4.0 <= f64::from(self.iq_limit_mib) * 1024.0 * 1024.0,
                "requested IQ capture exceeds --iq-limit-mib"
            );
        }
        Ok(())
    }
}

impl Capture {
    fn duration(&self) -> f64 {
        self.radio.duration()
    }
    fn validate(&self) -> Result<(u16, u64)> {
        self.radio.validate()?;
        frequency(self.radio.band, self.channel)
    }
}

fn workspace(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = path.canonicalize()?;
        ensure!(
            path.join("Cargo.toml").is_file() && path.join("repos").is_dir(),
            "expected this workspace root"
        );
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|p| p.join("Cargo.toml").is_file() && p.join("repos").is_dir())
        .map(Path::to_path_buf)
        .context("workspace not found; pass --workspace /path/to/bladerf")
}

fn output(root: &Path, chosen: Option<&Path>, category: &str) -> Result<PathBuf> {
    let path = root.join(chosen.map(Path::to_path_buf).unwrap_or_else(|| {
        Path::new(category).join(Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string())
    }));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .with_context(|| {
            format!(
                "create new output directory {}; existing outputs are preserved",
                path.display()
            )
        })?;
    path.canonicalize().context("resolve new output directory")
}

fn new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

fn json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut f = new_file(path)?;
    serde_json::to_writer_pretty(&mut f, value)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}

fn inspect(input: &Path, directory: &Path) -> Result<()> {
    let summary = packets::inspect(
        input,
        Some(&directory.join("validated.pcap")),
        Some(&directory.join("packets.jsonl")),
    )
    .with_context(|| {
        format!(
            "inspect {}; source and logs are preserved in {}",
            input.display(),
            directory.display()
        )
    })?;
    json_file(&directory.join("summary.json"), &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    println!("Output directory: {}", directory.display());
    Ok(())
}

/// Owns and reaps one subprocess. Explicit shutdown waits up to five seconds
/// after SIGINT, then kills and waits. Drop also kills and waits as best-effort
/// cleanup and can block in the operating system's wait operation.
struct OwnedChild {
    child: Child,
    reaped: bool,
}
impl OwnedChild {
    fn spawn(command: &mut Command, log: &Path) -> Result<Self> {
        let f = new_file(log)?;
        command
            .stdout(f.try_clone()?)
            .stderr(f)
            .stdin(Stdio::null());
        Ok(Self {
            child: command.spawn().context("start worker")?,
            reaped: false,
        })
    }
    fn poll(&mut self) -> Result<Option<ExitStatus>> {
        let result = self.child.try_wait()?;
        if result.is_some() {
            self.reaped = true;
        }
        Ok(result)
    }
    fn stop(&mut self) -> Result<(ExitStatus, bool)> {
        if let Some(status) = self.poll()? {
            return Ok((status, false));
        }
        let signal = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .context("interrupt worker")?;
        if !signal.success() {
            if let Some(status) = self.poll()? {
                return Ok((status, false));
            }
            bail!("could not interrupt worker");
        }
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.poll()? {
                return Ok((status, false));
            }
            if Instant::now() >= end {
                self.child.kill()?;
                let status = self.child.wait()?;
                self.reaped = true;
                return Ok((status, true));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn run_completed(
    command: &mut Command,
    log: &Path,
    budget: Duration,
    interrupted: &AtomicBool,
) -> Result<()> {
    let start = Instant::now();
    let mut process = OwnedChild::spawn(command, log)?;
    loop {
        if let Some(status) = process.poll()? {
            ensure!(
                status.success(),
                "worker failed ({status}); see {}",
                log.display()
            );
            return Ok(());
        }
        if interrupted.load(Ordering::Relaxed) || start.elapsed() >= budget {
            let (_, forced) = process.stop()?;
            bail!(
                "worker interrupted or exceeded time budget (forced kill: {forced}); inputs/logs preserved at {}",
                log.parent().unwrap_or(Path::new(".")).display()
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn receiver(root: &Path, override_path: Option<&Path>, engine: Engine) -> Result<PathBuf> {
    let default = match engine {
        Engine::Crafter => "target/release/bladerf-lab",
        Engine::Futuresdr => "build/futuresdr/release/rx",
    };
    let path = override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join(default));
    let path = path.canonicalize().with_context(|| {
        format!(
            "receiver missing at {}; see project build instructions",
            path.display()
        )
    })?;
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "receiver must be an executable regular file"
    );
    if matches!(engine, Engine::Futuresdr) {
        let checked = Command::new("git")
            .arg("-C")
            .arg(root.join("repos/FutureSDR"))
            .args(["apply", "--reverse", "--check"])
            .arg(root.join("patches/futuresdr-wlan-passive-rx.patch"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        ensure!(
            checked.success(),
            "FutureSDR passive/FCS source patch is missing"
        );
    }
    Ok(path)
}

fn decode_worker(
    worker: &Worker,
    directory: &Path,
    budget: Duration,
    stop: &AtomicBool,
) -> Result<()> {
    ensure!(
        cfg!(feature = "decode"),
        "IQ decoding requires the decode Cargo feature"
    );
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("decode-worker")
        .arg(&worker.input)
        .arg("--pcap")
        .arg(&worker.pcap)
        .args([
            "--frequency-hz",
            &worker.frequency_hz.to_string(),
            "--max-samples",
            &worker.max_samples.to_string(),
            "--format",
            match worker.format {
                InputFormat::Sc16q11 => "sc16q11",
                InputFormat::Cs8 => "cs8",
            },
        ]);
    if let Some(origin) = worker.time_origin_ns {
        command.arg("--time-origin-ns").arg(origin.to_string());
    }
    run_completed(&mut command, &directory.join("decoder.log"), budget, stop)?;
    inspect(&worker.pcap, directory)
}

#[derive(Debug, Deserialize)]
struct Acquisition {
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
    num_samples: u64,
    started_utc: String,
}

fn acquisition(iq: &Path, args: &Capture, hz: u64) -> Result<(u64, u128)> {
    let sidecar = iq.with_extension("sc16q11.json");
    let mut bytes = Vec::new();
    File::open(sidecar)?.take(65_537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65_536, "acquisition sidecar exceeds 64 KiB");
    let settings: Acquisition = serde_json::from_slice(&bytes)?;
    ensure!(
        settings.format == "SC16_Q11"
            && settings.dtype == "<i2"
            && settings.interleave == "I,Q"
            && settings.complex_scale == 2048,
        "acquisition sample encoding differs from SC16_Q11 little-endian I,Q"
    );
    ensure!(
        (settings.sample_rate_hz - 20_000_000.0).abs() <= 1.0
            && (settings.frequency_hz - hz as f64).abs() <= 1.0
            && (settings.bandwidth_hz - 20_000_000.0).abs() <= 1.0,
        "acquisition rate, frequency or bandwidth readback differs from request"
    );
    ensure!(
        !settings.bias_tee_enabled
            && settings.channel == args.radio.antenna.name()
            && settings.gain_mode == "manual"
            && (settings.gain_db - args.radio.gain).abs() <= 0.01,
        "acquisition bias, channel or gain readback differs from request"
    );
    ensure!(
        settings.num_samples == (settings.sample_rate_hz * args.duration()).round() as u64
            && (1..=200_000_000).contains(&settings.num_samples)
            && fs::metadata(iq)?.len() == settings.num_samples * 4,
        "acquisition sample count or IQ byte length differs from request"
    );
    let origin = chrono::DateTime::parse_from_rfc3339(&settings.started_utc)?
        .timestamp_nanos_opt()
        .and_then(|n| u128::try_from(n).ok())
        .context("acquisition host start time is out of range")?;
    Ok((settings.num_samples, origin))
}

/// A bounded log scan catches known native I/O failures, but cannot certify
/// continuity. Packet CRC failures are ordinary RF decoder outcomes, not I/O.
fn io_diagnostics(log: &Path) -> Result<serde_json::Value> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(log)?.take(LIMIT).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    let errors = [
        "transfer error",
        "transfer timed out",
        "stream error",
        "seify source error",
        "panicked at",
    ]
    .iter()
    .map(|needle| text.matches(needle).count())
    .sum::<usize>();
    let overruns = text.matches("seify source overrun").count();
    Ok(
        json!({"known_io_error_markers":errors,"overrun_markers":overruns,"log_scan_bytes":bytes.len(),
        "log_scan_truncated":fs::metadata(log)?.len() > LIMIT,"continuity_guaranteed":false}),
    )
}

fn capture_iq(
    root: &Path,
    program: &Path,
    args: &Capture,
    hz: u64,
    directory: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    ensure!(
        cfg!(feature = "decode"),
        "bounded IQ capture/replay requires the decode feature"
    );
    let iq = directory.join("iq.sc16q11");
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .args(["--workspace"])
        .arg(root)
        .arg("capture")
        .args([
            "--frequency",
            &hz.to_string(),
            "--sample-rate",
            "20000000",
            "--gain",
            &args.radio.gain.to_string(),
            "--channel",
            &args.radio.antenna.channel().to_string(),
            "--duration",
            &args.duration().to_string(),
            "--output",
        ])
        .arg(&iq);
    if let Some(serial) = &args.radio.serial {
        command.arg("--serial").arg(serial);
    }
    run_completed(
        &mut command,
        &directory.join("acquisition.log"),
        Duration::from_secs_f64(args.duration() + 20.0),
        stop,
    )?;
    let (samples, origin) = acquisition(&iq, args, hz)?;
    let diagnostics = io_diagnostics(&directory.join("acquisition.log"))?;
    json_file(
        &directory.join("capture.json"),
        &json!({"engine":"crafter","samples":samples,"frequency_hz_requested":hz,"sample_rate_hz_requested":20_000_000,"sample_format":"SC16_Q11_LE","host_rx_origin_ns":origin,"timestamp_kind":"host_before_rx_enable_plus_sample_offset_not_hardware_time","continuity_guaranteed":false,"io_diagnostics":diagnostics}),
    )?;
    decode_worker(
        &Worker {
            input: iq,
            pcap: directory.join("candidates.pcap"),
            format: InputFormat::Sc16q11,
            frequency_hz: hz,
            max_samples: samples,
            time_origin_ns: Some(origin),
        },
        directory,
        Duration::from_secs_f64(args.radio.decode_timeout),
        stop,
    )?;
    ensure!(
        diagnostics["known_io_error_markers"] == 0 && diagnostics["log_scan_truncated"] == false,
        "native I/O errors recorded; inspect acquisition.log and preserved outputs"
    );
    Ok(())
}

struct PcapWriter(BufWriter<File>);
impl PcapWriter {
    fn new(path: &Path) -> Result<Self> {
        let mut f = BufWriter::new(new_file(path)?);
        f.write_all(&0xa1b2c3d4u32.to_le_bytes())?;
        f.write_all(&2u16.to_le_bytes())?;
        f.write_all(&4u16.to_le_bytes())?;
        f.write_all(&[0; 8])?;
        f.write_all(&65536u32.to_le_bytes())?;
        f.write_all(&127u32.to_le_bytes())?;
        f.flush()?;
        Ok(Self(f))
    }
    fn packet(&mut self, mpdu: &[u8]) -> Result<()> {
        ensure!(mpdu.len() <= 65507, "oversized UDP MPDU");
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let seconds = u32::try_from(now.as_secs())?;
        let length = u32::try_from(mpdu.len() + 9)?;
        self.0.write_all(&seconds.to_le_bytes())?;
        self.0.write_all(&now.subsec_micros().to_le_bytes())?;
        self.0.write_all(&length.to_le_bytes())?;
        self.0.write_all(&length.to_le_bytes())?;
        self.0.write_all(&[0, 0, 9, 0, 2, 0, 0, 0, 0x10])?;
        self.0.write_all(mpdu)?;
        self.0.flush()?;
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        self.0.flush()?;
        self.0.get_ref().sync_all()?;
        Ok(())
    }
}

fn capture_live(
    root: &Path,
    program: &Path,
    args: &Capture,
    channel: u16,
    directory: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let mut command = Command::new(program);
    command.current_dir(root).args([
        "--args",
        "driver=soapy,soapy_driver=bladerf,instance=0",
        "--channel",
        &channel.to_string(),
        "--gain",
        &args.radio.gain.to_string(),
        "--rx-channel",
        &(args.radio.antenna.channel() - 1).to_string(),
        "--antenna",
        args.radio.antenna.name(),
        "--sample-rate",
        "20000000",
        "--udp-port",
        &socket.local_addr()?.port().to_string(),
    ]);
    let mut writer = PcapWriter::new(&directory.join("candidates.pcap"))?;
    let start = Instant::now();
    let mut process = OwnedChild::spawn(&mut command, &directory.join("decoder.log"))?;
    let mut buffer = [0; 65536];
    let mut received = 0u64;
    let mut early = None;
    while start.elapsed().as_secs_f64() < args.duration() && !stop.load(Ordering::Relaxed) {
        if let Some(status) = process.poll()? {
            early = Some(status);
            break;
        }
        match socket.recv_from(&mut buffer) {
            Ok((len, peer)) => {
                ensure!(peer.ip().is_loopback(), "unexpected UDP sender");
                writer.packet(&buffer[..len])?;
                received += 1;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let was_early = early.is_some();
    let interrupted = stop.load(Ordering::Relaxed);
    let (status, forced) = match early {
        Some(status) => (status, false),
        None => process.stop()?,
    };
    writer.finish()?;
    let diagnostics = io_diagnostics(&directory.join("decoder.log"))?;
    json_file(
        &directory.join("capture.json"),
        &json!({"engine":"futuresdr","host_elapsed_seconds":start.elapsed().as_secs_f64(),"udp_records":received,"interrupted":interrupted,"unexpected_early_exit":was_early,"native_exit_code":status.code(),"native_exit_signal":status.signal(),"forced_kill":forced,"timestamp_kind":"host_udp_receive","continuity_guaranteed":false,"udp_loss_known":false,"io_diagnostics":diagnostics}),
    )?;
    inspect(&directory.join("candidates.pcap"), directory)?;
    ensure!(
        !interrupted && !was_early && !forced && (status.success() || status.signal() == Some(2)),
        "live receiver did not finish the requested window cleanly; files are preserved in {}",
        directory.display()
    );
    ensure!(
        diagnostics["known_io_error_markers"] == 0
            && diagnostics["overrun_markers"] == 0
            && diagnostics["log_scan_truncated"] == false,
        "native I/O errors or overruns recorded; inspect decoder.log and preserved outputs"
    );
    Ok(())
}

fn capture_into(
    root: &Path,
    program: &Path,
    args: &Capture,
    directory: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    let (channel, hz) = args.validate()?;
    json_file(
        &directory.join("request.json"),
        &json!({"engine":args.radio.engine,"channel":channel,"frequency_hz_requested":hz,"sample_rate_hz_requested":20_000_000,"antenna_requested":args.radio.antenna.name(),"gain_db_requested":args.radio.gain,"duration_requested":args.duration(),"decode_timeout_seconds":args.radio.decode_timeout,"rx_bias_tee_requested":false,"receive_only_requested":true}),
    )?;
    match args.radio.engine {
        Engine::Crafter => capture_iq(root, program, args, hz, directory, stop),
        Engine::Futuresdr => capture_live(root, program, args, channel, directory, stop),
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Action::DecodeWorker(worker) = cli.command {
        #[cfg(feature = "decode")]
        {
            let summary = decode::decode(
                &worker.input,
                &worker.pcap,
                worker.frequency_hz,
                match worker.format {
                    InputFormat::Sc16q11 => decode::IqFormat::Sc16Q11,
                    InputFormat::Cs8 => decode::IqFormat::Cs8,
                },
                worker.max_samples,
                worker.time_origin_ns,
            )?;
            json_file(&worker.pcap.with_extension("decode.json"), &summary)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        #[cfg(not(feature = "decode"))]
        {
            let _ = worker;
            bail!("IQ decoding was disabled at build time");
        }
    }
    let root = if cli.workspace.is_none()
        && matches!(
            cli.command,
            Action::Inspect { .. } | Action::DecodeIq(_) | Action::Inventory(_)
        ) {
        workspace(None).or_else(|_| std::env::current_dir())?
    } else {
        workspace(cli.workspace)?
    };
    if let Action::Inspect {
        input,
        output: chosen,
    } = &cli.command
    {
        let directory = output(&root, chosen.as_deref(), "artifacts/wifi-inspect")?;
        return inspect(input, &directory);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .context("install shutdown handler")?;
    match cli.command {
        Action::Doctor { engine } => {
            receiver(&root, cli.receiver.as_deref(), engine)?;
            if matches!(engine, Engine::Crafter) {
                ensure!(cfg!(feature = "decode"), "rebuild with decode feature");
            }
            println!(
                "Software available; source patch checked where required. Executable provenance is not attested. No radio opened."
            );
        }
        Action::Capture(args) => {
            args.validate()?;
            let program = receiver(&root, cli.receiver.as_deref(), args.radio.engine)?;
            let directory = output(&root, args.output.as_deref(), "captures/wifi")?;
            capture_into(&root, &program, &args, &directory, &stop)?;
        }
        Action::Survey(args) => monitor::survey(&root, cli.receiver.as_deref(), args, &stop)?,
        Action::Watch(args) => monitor::watch(&root, cli.receiver.as_deref(), args, &stop)?,
        Action::Inventory(args) => monitor::inventory(&root, args, &stop)?,
        Action::DecodeIq(args) => {
            let budget = positive_seconds(args.decode_timeout, 3600.0, "decode timeout")?;
            ensure!(
                (1..=200_000_000).contains(&args.max_samples),
                "max samples must be 1..=200000000"
            );
            ensure!(
                (70..=6000).contains(&args.frequency_mhz),
                "frequency MHz must be 70..=6000"
            );
            let input = args.input.canonicalize()?;
            let directory = output(&root, args.output.as_deref(), "artifacts/wifi-decoded")?;
            decode_worker(
                &Worker {
                    input,
                    pcap: directory.join("candidates.pcap"),
                    format: args.format,
                    frequency_hz: u64::from(args.frequency_mhz) * 1_000_000,
                    max_samples: args.max_samples,
                    time_origin_ns: None,
                },
                &directory,
                budget,
                &stop,
            )?;
        }
        Action::Inspect { .. } | Action::DecodeWorker(_) => unreachable!(),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_channel_math_and_iq_bounds_before_receiver() {
        assert_eq!(
            frequency(Band::TwoGhz, Some(6)).unwrap(),
            (6, 2_437_000_000)
        );
        assert_eq!(
            frequency(Band::FiveGhz, Some(36)).unwrap(),
            (36, 5_180_000_000)
        );
        assert!(frequency(Band::TwoGhz, Some(14)).is_err());
        for args in [
            ["--duration", "NaN"],
            ["--duration", "0.00000000001"],
            ["--duration", "11"],
            ["--gain", "99"],
            ["--iq-limit-mib", "1"],
        ] {
            let cli = Cli::try_parse_from(["wifi-monitor", "capture", args[0], args[1]]).unwrap();
            let Action::Capture(c) = cli.command else {
                panic!("capture")
            };
            assert!(c.validate().is_err());
        }
    }
    #[test]
    fn receiver_requires_an_executable_file() {
        let root = tempfile::tempdir().unwrap();
        assert!(receiver(root.path(), Some(root.path()), Engine::Crafter).is_err());
        let file = root.path().join("worker");
        fs::write(&file, "not executable").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(receiver(root.path(), Some(&file), Engine::Crafter).is_err());
    }
    #[test]
    fn capture_sidecar_must_match_format_readbacks_and_completed_iq() {
        let root = tempfile::tempdir().unwrap();
        let iq = root.path().join("iq.sc16q11");
        fs::write(&iq, [0; 16]).unwrap();
        let Action::Capture(args) =
            Cli::try_parse_from(["wifi-monitor", "capture", "--duration", "0.0000002"])
                .unwrap()
                .command
        else {
            panic!("capture")
        };
        let baseline = json!({"format":"SC16_Q11","dtype":"<i2","interleave":"I,Q",
            "complex_scale":2048,"frequency_hz":2412000000u64,"sample_rate_hz":20000000,
            "bandwidth_hz":20000000,"gain_db":40,"gain_mode":"manual",
            "bias_tee_enabled":false,"channel":"RX1","num_samples":4,
            "started_utc":"2024-01-01T00:00:00Z"});
        let sidecar = iq.with_extension("sc16q11.json");
        fs::write(&sidecar, serde_json::to_vec(&baseline).unwrap()).unwrap();
        assert_eq!(acquisition(&iq, &args, 2_412_000_000).unwrap().0, 4);
        for (key, value) in [
            ("dtype", json!(">i2")),
            ("sample_rate_hz", json!(10000000)),
            ("bias_tee_enabled", json!(true)),
            ("channel", json!("RX2")),
            ("gain_db", json!(50)),
            ("num_samples", json!(5)),
        ] {
            let mut wrong = baseline.clone();
            wrong[key] = value;
            fs::write(&sidecar, serde_json::to_vec(&wrong).unwrap()).unwrap();
            assert!(acquisition(&iq, &args, 2_412_000_000).is_err(), "{key}");
        }
        fs::write(&sidecar, [b' '; 65_537]).unwrap();
        assert!(acquisition(&iq, &args, 2_412_000_000).is_err());
    }
    #[test]
    fn timed_out_worker_is_reaped_and_keeps_log() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("worker.log");
        let mut command = Command::new("sleep");
        command.arg("5");
        assert!(
            run_completed(
                &mut command,
                &log,
                Duration::from_millis(10),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(log.is_file());
    }
    #[test]
    fn diagnostics_distinguish_io_errors_overruns_and_rf_rejections() {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("worker.log");
        fs::write(
            &log,
            "Seify Source Error: timeout\nSeify Source Overrun\ninvalid frame CRC\n",
        )
        .unwrap();
        let report = io_diagnostics(&log).unwrap();
        assert_eq!(report["known_io_error_markers"], 1);
        assert_eq!(report["overrun_markers"], 1);
        assert_eq!(report["log_scan_truncated"], false);
        fs::write(&log, "invalid frame CRC\n").unwrap();
        assert_eq!(io_diagnostics(&log).unwrap()["known_io_error_markers"], 0);
    }
    #[test]
    fn outputs_are_workspace_relative_and_never_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let path = output(root.path(), Some(Path::new("captures/example")), "unused").unwrap();
        assert!(path.starts_with(root.path()));
        fs::write(path.join("keep"), "preserved").unwrap();
        assert!(output(root.path(), Some(Path::new("captures/example")), "unused").is_err());
    }
    #[test]
    fn pcap_writer_declares_actual_fcs_and_inspector_can_read_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("candidate.pcap");
        let mut writer = PcapWriter::new(&path).unwrap();
        writer.packet(&[0; 14]).unwrap();
        writer.finish().unwrap();
        let summary = packets::inspect(&path, None, None).unwrap();
        let value = serde_json::to_value(summary).unwrap();
        assert_eq!(value["invalid_fcs_frames"], 1);
        assert!(PcapWriter::new(&path).is_err());
    }
}
