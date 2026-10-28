//! Wideband capture, bounded multichannel receive references and FPGA verification.
//!
//! Host processing is a reference path, not evidence that custom gateware was
//! fitted or loaded. Captures, audio, waveforms and local tool reports stay in
//! ignored output directories. See the project guide for hardware build limits.

#![forbid(unsafe_code)]

mod dsp;
mod gateware;
mod receiver;
mod spectrum;
mod transport;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

const SAMPLE_RATE: u32 = 20_000_000;
const MAX_SAMPLES: u64 = 200_000_000;

#[derive(Debug, Parser)]
#[command(about = "Wideband receiver references and verified FPGA channel-bank experiments")]
struct Cli {
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Check local tools without opening the radio or loading gateware.
    Doctor,
    /// Record ordinary hosted-image IQ, optionally extract selected FM stations.
    Capture(Capture),
    /// Produce a bounded averaged spectrum and suggested FM tuning candidates.
    Survey(Survey),
    /// Run the bit-exact and floating reference receivers on saved 20 MSps IQ.
    Receive(Receive),
    /// Generate deterministic multi-carrier FM input for host/HDL experiments.
    Fixture(Fixture),
    /// Compile, simulate and generically synthesize HDL using GHDL.
    VerifyHdl {
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 120.0)]
        timeout: f64,
    },
    /// Decode the explicit custom-FPGA stream format from a saved file.
    DecodeFpga {
        input: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 1_000_000)]
        max_records: u64,
    },
}

#[derive(Debug, Args)]
struct Capture {
    #[arg(long, default_value_t = 98.0)]
    center_mhz: f64,
    #[arg(long, default_value_t = 0.5)]
    duration: f64,
    #[arg(long, default_value_t = 40.0)]
    gain: f64,
    #[arg(long,default_value_t=1,value_parser=clap::value_parser!(u8).range(1..=2))]
    antenna: u8,
    #[arg(long, value_delimiter = ',')]
    stations: Vec<f64>,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Existing native acquisition tool, built from this workspace.
    #[arg(long)]
    receiver: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
struct Receive {
    input: PathBuf,
    #[arg(long, default_value_t = 98.0)]
    center_mhz: f64,
    #[arg(long, value_delimiter = ',', required = true)]
    stations: Vec<f64>,
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
    #[arg(long, default_value_t = 120.0)]
    timeout: f64,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Survey {
    input: PathBuf,
    #[arg(long, default_value_t = 98.0)]
    center_mhz: f64,
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
    #[arg(long, default_value_t = 128)]
    windows: usize,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct Fixture {
    #[arg(long, default_value_t = 0.1)]
    duration: f64,
    #[arg(long)]
    output: Option<PathBuf>,
}

fn workspace(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = path.canonicalize()?;
        ensure!(
            path.join("Cargo.toml").is_file() && path.join("repos/bladeRF").is_dir(),
            "expected this workspace root"
        );
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|p| p.join("Cargo.toml").is_file() && p.join("repos/bladeRF").is_dir())
        .map(Path::to_path_buf)
        .context("workspace not found; pass --workspace")
}

fn directory(root: &Path, chosen: Option<&Path>, category: &str) -> Result<PathBuf> {
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
                "create new output {}; existing output is preserved",
                path.display()
            )
        })?;
    path.canonicalize().context("resolve output directory")
}

fn new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

fn json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = new_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn budget(seconds: f64, maximum: f64) -> Result<Duration> {
    ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= maximum,
        "timeout/duration must be finite, positive and at most {maximum} seconds"
    );
    Ok(Duration::from_secs_f64(seconds))
}

fn center_hz(mhz: f64) -> Result<u64> {
    ensure!(
        mhz.is_finite() && (70.0..=6000.0).contains(&mhz),
        "center must be 70..=6000 MHz"
    );
    Ok((mhz * 1_000_000.0).round() as u64)
}

fn station_words(center: u64, stations: &[f64]) -> Result<Vec<u32>> {
    ensure!(
        !stations.is_empty() && stations.len() <= 4,
        "select 1..=4 station frequencies"
    );
    let mut words = Vec::new();
    for station in stations {
        center_hz(*station)?;
        let offset = station * 1_000_000.0 - center as f64;
        ensure!(
            offset.abs() <= 8_000_000.0,
            "station must be within the central 16 MHz input window"
        );
        let word = dsp::tuning_word(offset, SAMPLE_RATE)?;
        ensure!(!words.contains(&word), "duplicate station frequencies");
        words.push(word);
    }
    Ok(words)
}

fn run(cli: Cli) -> Result<()> {
    let offline = !matches!(
        cli.command,
        Action::Capture(_) | Action::Doctor | Action::VerifyHdl { .. }
    );
    let root = if offline && cli.workspace.is_none() {
        workspace(None).or_else(|_| std::env::current_dir())?
    } else {
        workspace(cli.workspace)?
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))?;
    match cli.command {
        Action::Doctor => {
            for name in ["ghdl", "quartus_sh", "nios2-bsp"] {
                let present = std::env::var_os("PATH").is_some_and(|paths| {
                    std::env::split_paths(&paths).any(|directory| {
                        fs::metadata(directory.join(name))
                            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    })
                });
                println!("{name}: {}", if present { "available" } else { "missing" });
            }
            println!(
                "Native acquisition binary: {}",
                root.join("target/release/bladerf-lab").is_file()
            );
            println!(
                "No radio opened. Tool availability does not establish a fitted or loaded FPGA design."
            );
        }
        Action::Capture(args) => capture(&root, args, &stop)?,
        Action::Receive(args) => {
            let center = center_hz(args.center_mhz)?;
            let words = station_words(center, &args.stations)?;
            let timeout = budget(args.timeout, 3600.0)?;
            ensure!(
                (1..=MAX_SAMPLES).contains(&args.max_samples),
                "max samples must be 1..={MAX_SAMPLES}"
            );
            let input = args.input.canonicalize()?;
            let output = directory(
                &root,
                args.output.as_deref(),
                "artifacts/rf-observatory/receive",
            )?;
            json_file(
                &output.join("request.json"),
                &json!({"input":input,"center_frequency_hz":center,"sample_rate_hz":SAMPLE_RATE,"stations_mhz":args.stations,"tuning_words":words,"mode":"host_reference_not_fpga_execution"}),
            )?;
            let report =
                receiver::receive(&input, &output, &words, args.max_samples, timeout, &stop)?;
            json_file(&output.join("receive.json"), &report)?;
            println!(
                "Decoded {} samples across {} reference channels. Output: {}",
                report.input_samples,
                words.len(),
                output.display()
            );
        }
        Action::Survey(args) => {
            let center = center_hz(args.center_mhz)?;
            ensure!(
                (1..=MAX_SAMPLES).contains(&args.max_samples),
                "invalid sample limit"
            );
            ensure!((1..=512).contains(&args.windows), "windows must be 1..=512");
            let input = args.input.canonicalize()?;
            let output = directory(
                &root,
                args.output.as_deref(),
                "artifacts/rf-observatory/spectrum",
            )?;
            let report = spectrum::survey(&input, center, args.max_samples, args.windows, &stop)?;
            json_file(&output.join("spectrum.json"), &report)?;
            let mut csv = new_file(&output.join("spectrum.csv"))?;
            writeln!(csv, "frequency_hz,relative_power_db")?;
            for bin in &report.bins {
                writeln!(csv, "{:.3},{:.6}", bin.frequency_hz, bin.relative_power_db)?;
            }
            csv.sync_all()?;
            println!(
                "Averaged {} FFT windows; suggested carrier centers (energy evidence, not verified stations):",
                report.windows
            );
            for candidate in &report.candidates {
                println!(
                    "{:.3} MHz ({:.1} dB relative)",
                    candidate.frequency_hz / 1_000_000.0,
                    candidate.relative_power_db
                );
            }
            println!("Output: {}", output.display());
        }
        Action::Fixture(args) => {
            budget(args.duration, 1.0)?;
            let samples = (args.duration * f64::from(SAMPLE_RATE)).round() as u64;
            ensure!(samples >= 400, "fixture needs at least 400 samples");
            let output = directory(
                &root,
                args.output.as_deref(),
                "artifacts/rf-observatory/fixture",
            )?;
            receiver::fixture(&output, samples, &stop)?;
            println!("Synthetic IQ fixture: {}", output.display());
        }
        Action::VerifyHdl { output, timeout } => {
            let timeout = budget(timeout, 600.0)?;
            let output = directory(&root, output.as_deref(), "artifacts/rf-observatory/hdl")?;
            json_file(
                &output.join("request.json"),
                &json!({"tool":"GHDL","purpose":"simulation_and_generic_synthesis_not_quartus_fit_or_loaded_gateware"}),
            )?;
            let summary = gateware::verify_hdl_with_cancel(&root, &output, timeout, &stop)?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            println!("Output: {}", output.display());
        }
        Action::DecodeFpga {
            input,
            output,
            max_records,
        } => {
            ensure!(
                (1..=1_000_000).contains(&max_records),
                "max records must be 1..=1000000"
            );
            let input = input.canonicalize()?;
            let output = directory(
                &root,
                output.as_deref(),
                "artifacts/rf-observatory/fpga-stream",
            )?;
            let summary = transport::decode(&input, &output, max_records, &stop)?;
            json_file(&output.join("stream.json"), &summary)?;
            println!(
                "Decoded {} custom-format frames. Output: {}",
                summary.records,
                output.display()
            );
        }
    }
    Ok(())
}

fn capture(root: &Path, args: Capture, stop: &AtomicBool) -> Result<()> {
    let center = center_hz(args.center_mhz)?;
    budget(args.duration, 10.0)?;
    ensure!(
        args.gain.is_finite() && (0.0..=71.0).contains(&args.gain),
        "gain must be 0..=71 dB"
    );
    let samples = (args.duration * f64::from(SAMPLE_RATE)).round() as u64;
    ensure!(
        samples >= 8192,
        "capture requires at least 8192 samples for its spectrum report"
    );
    let words = if args.stations.is_empty() {
        vec![]
    } else {
        station_words(center, &args.stations)?
    };
    let program = args
        .receiver
        .unwrap_or_else(|| root.join("target/release/bladerf-lab"))
        .canonicalize()?;
    ensure!(
        program.is_file() && fs::metadata(&program)?.permissions().mode() & 0o111 != 0,
        "native acquisition binary must be an executable file"
    );
    let output = directory(root, args.output.as_deref(), "captures/rf-observatory")?;
    let iq = output.join("wideband.sc16q11");
    json_file(
        &output.join("request.json"),
        &json!({"center_frequency_hz":center,"sample_rate_hz":SAMPLE_RATE,"samples_requested":samples,"antenna_requested":args.antenna,"gain_db_requested":args.gain,"mode":"hosted_iq_capture_not_custom_fpga_channelizer"}),
    )?;
    let log = new_file(&output.join("acquisition.log"))?;
    let mut command = Command::new(program);
    command
        .arg("--workspace")
        .arg(root)
        .args([
            "capture",
            "--frequency",
            &center.to_string(),
            "--sample-rate",
            "20000000",
            "--channel",
            &args.antenna.to_string(),
            "--gain",
            &args.gain.to_string(),
            "--duration",
            &args.duration.to_string(),
            "--output",
        ])
        .arg(&iq)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = AcquisitionChild {
        child: command.spawn().context("start native acquisition")?,
        reaped: false,
    };
    let start = Instant::now();
    let result = loop {
        if let Some(status) = child.child.try_wait()? {
            child.reaped = true;
            break if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "acquisition failed ({status}); see {}",
                    output.join("acquisition.log").display()
                ))
            };
        }
        if stop.load(Ordering::Relaxed) || start.elapsed().as_secs_f64() > args.duration + 20.0 {
            let forced = child.stop()?;
            json_file(
                &output.join("acquisition-failure.json"),
                &json!({"interrupted":stop.load(Ordering::Relaxed),"forced_kill":forced}),
            )?;
            break Err(anyhow::anyhow!(
                "acquisition interrupted or timed out; evidence in {}",
                output.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    result?;
    let sidecar = receiver::validate_acquisition(&iq, center, samples, args.antenna, args.gain)
        .with_context(|| {
            format!(
                "validate acquisition; source and logs retained in {}",
                output.display()
            )
        })?;
    let actual_center = sidecar.frequency_hz();
    let survey = spectrum::survey(&iq, actual_center, samples, 128, stop)?;
    json_file(&output.join("spectrum.json"), &survey)?;
    json_file(&output.join("capture.json"), &sidecar)?;
    if !words.is_empty() {
        let words = station_words(actual_center, &args.stations)?;
        let report = receiver::receive(
            &iq,
            &output,
            &words,
            samples,
            Duration::from_secs(120),
            stop,
        )?;
        json_file(&output.join("receive.json"), &report)?;
    }
    println!(
        "Recorded {samples} complex samples. Output: {}",
        output.display()
    );
    Ok(())
}

/// Own and reap a synchronous native acquisition process. Explicit stop first
/// allows native SIGINT cleanup, then kills a stalled worker. Fallback Drop
/// cleanup kills and waits and may block in operating-system teardown.
struct AcquisitionChild {
    child: std::process::Child,
    reaped: bool,
}
impl AcquisitionChild {
    fn stop(&mut self) -> Result<bool> {
        if self.child.try_wait()?.is_some() {
            self.reaped = true;
            return Ok(false);
        }
        let _ = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                self.reaped = true;
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.child.kill()?;
        self.child.wait()?;
        self.reaped = true;
        Ok(true)
    }
}
impl Drop for AcquisitionChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn channel_selection_bounds_and_quantized_duplicates_are_rejected() {
        assert!(station_words(98_000_000, &[92.0, 96.0, 100.0, 104.0]).is_ok());
        for stations in [
            vec![],
            vec![f64::NAN],
            vec![80.0],
            vec![98.0, 98.0 + 0.00000000001],
            vec![92.0; 5],
        ] {
            assert!(station_words(98_000_000, &stations).is_err());
        }
        assert!(center_hz(f64::INFINITY).is_err());
    }
    #[test]
    fn tiny_capture_fails_before_files_or_native_binary_access() {
        let temp = tempfile::tempdir().unwrap();
        let Action::Capture(args) = Cli::try_parse_from([
            "rf-observatory",
            "capture",
            "--duration",
            "0.000001",
            "--output",
            "new-output",
        ])
        .unwrap()
        .command
        else {
            panic!("capture")
        };
        assert!(capture(temp.path(), args, &AtomicBool::new(false)).is_err());
        assert!(!temp.path().join("new-output").exists());
    }
    #[test]
    fn native_worker_interrupt_is_reaped() {
        let mut child = AcquisitionChild {
            child: Command::new("sleep").arg("5").spawn().unwrap(),
            reaped: false,
        };
        assert!(!child.stop().unwrap());
        assert!(child.reaped);
    }
}
