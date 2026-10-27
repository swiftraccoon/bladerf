//! Passive Bluetooth capture with bounded decoder lifetime and independent BLE CRC checks.
//!
//! Run `cargo run --locked -p bluetooth-monitor -- --help` from the workspace.
//! Captures and per-packet reports contain radio observations; keep them local.

#![forbid(unsafe_code)]

mod packets;

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(about = "Passive Bluetooth monitoring and independent BLE packet validation")]
struct Cli {
    /// Workspace root; otherwise discover it from the current directory.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    /// Decoder executable; defaults to the selected engine's workspace build.
    #[arg(long, global = true)]
    decoder: Option<PathBuf>,
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Check decoder files and the required receive-setup patch without opening a radio.
    Doctor {
        #[arg(long, value_enum, default_value = "blue-dragon")]
        engine: Engine,
    },
    /// Receive packets, preserve candidates, and write independently verified BLE output.
    Capture(Capture),
    /// Inspect a PCAP without a radio or external decoder.
    Inspect {
        input: PathBuf,
        /// New output directory; defaults under artifacts/bluetooth-inspect/.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum Engine {
    Ice9,
    BlueDragon,
}

impl Engine {
    fn source(self) -> &'static str {
        match self {
            Self::Ice9 => "repos/ice9-bluetooth-sniffer",
            Self::BlueDragon => "repos/blue-dragon",
        }
    }

    fn binary(self) -> &'static str {
        match self {
            Self::Ice9 => "build/ice9-bluetooth/ice9-bluetooth",
            Self::BlueDragon => "build/blue-dragon/release/blue-dragon",
        }
    }

    fn patch(self) -> &'static str {
        match self {
            Self::Ice9 => "patches/ice9-bluetooth-bladerf-rx-bias.patch",
            Self::BlueDragon => "patches/blue-dragon-bladerf-rx-setup.patch",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
enum Antenna {
    Rx1,
    Rx2,
}

impl Antenna {
    fn native(self) -> &'static str {
        match self {
            Self::Rx1 => "RX1",
            Self::Rx2 => "RX2",
        }
    }
}

#[derive(Debug, Args)]
struct Capture {
    #[arg(long, value_enum, default_value = "blue-dragon")]
    engine: Engine,
    /// Decoder wall-time budget, including initialization (0.1..=3600 seconds).
    #[arg(long, alias = "seconds", default_value_t = 15.0)]
    duration: f64,
    /// New output directory; defaults under captures/bluetooth/.
    #[arg(long)]
    output: Option<PathBuf>,
    /// bladeRF device index, for example bladerf0.
    #[arg(long, default_value = "bladerf0")]
    interface: String,
    /// Receive connector. ICE9 supports RX1 only; Blue Dragon supports either RX port.
    #[arg(long, value_enum, default_value = "rx1")]
    antenna: Antenna,
    /// Sample rate in MSps and number of 1 MHz channelizer bins (8..=96, multiple of 4).
    #[arg(long, default_value_t = 32)]
    channels: u32,
    /// Center frequency in MHz; default covers primary advertising channels 37 and 38.
    #[arg(long, default_value_t = 2414)]
    center_mhz: u32,
    /// Select 96 MSps centered on 2441 MHz; RF sensitivity and throughput remain conditional.
    #[arg(long)]
    all_channels: bool,
    /// Manual RX gain. ICE9 supports only its fixed 30 dB setting.
    #[arg(long, default_value_t = 30, allow_hyphen_values = true)]
    gain: i32,
    /// Preserve IQ: ICE9 SC8_Q7 stream, or Blue Dragon channelized burst records.
    #[arg(long)]
    save_iq: bool,
    /// Soft IQ recording limit in MiB; polling and native buffering may overshoot it.
    #[arg(long, default_value_t = 512)]
    iq_limit_mib: u32,
    /// Soft combined limit for candidates, decoder log and IQ, in MiB.
    #[arg(long, default_value_t = 2048)]
    output_limit_mib: u32,
    /// Enable Blue Dragon's continuous passive coded-PHY advertising scan.
    #[arg(long)]
    coded_scan: bool,
}

impl Capture {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.duration.is_finite() && (0.1..=3600.0).contains(&self.duration),
            "duration must be finite and between 0.1 and 3600 seconds"
        );
        ensure!(
            (8..=96).contains(&self.channels) && self.channels.is_multiple_of(4),
            "channels must be a multiple of 4 between 8 and 96"
        );
        ensure!(
            (2400..=2480).contains(&self.center_mhz),
            "center MHz must be between 2400 and 2480"
        );
        ensure!(
            (0..=71).contains(&self.gain),
            "manual gain must be between 0 and 71 dB; the native driver also checks applicability"
        );
        ensure!(
            (1..=8192).contains(&self.iq_limit_mib),
            "IQ limit must be between 1 and 8192 MiB"
        );
        ensure!(
            (1..=8192).contains(&self.output_limit_mib),
            "output limit must be between 1 and 8192 MiB"
        );
        ensure!(
            self.interface
                .strip_prefix("bladerf")
                .is_some_and(|n| !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_digit())
                    && n.parse::<u32>().is_ok()),
            "interface must be a bladeRF device index, for example bladerf0"
        );
        if matches!(self.engine, Engine::Ice9) {
            ensure!(
                matches!(self.antenna, Antenna::Rx1),
                "ICE9 supports RX1 only; select Blue Dragon for RX2"
            );
            ensure!(
                self.gain == 30,
                "ICE9 uses fixed 30 dB gain; use Blue Dragon for configurable gain"
            );
            ensure!(!self.coded_scan, "coded scanning requires Blue Dragon");
        }
        Ok(())
    }
}

fn workspace(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let path = path.canonicalize().context("resolve workspace")?;
        ensure!(
            path.join("Cargo.toml").is_file() && path.join("repos").is_dir(),
            "expected this repository's workspace root"
        );
        return Ok(path);
    }
    let cwd = std::env::current_dir()?;
    cwd.ancestors()
        .find(|p| p.join("Cargo.toml").is_file() && p.join("repos").is_dir())
        .map(Path::to_path_buf)
        .context("workspace not found; pass --workspace /path/to/bladerf")
}

fn decoder(root: &Path, selected: Option<&Path>, engine: Engine) -> Result<PathBuf> {
    let binary = selected
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join(engine.binary()));
    let binary = binary.canonicalize().with_context(|| {
        format!(
            "decoder not found at {}; build the patched decoder described in the project README",
            binary.display()
        )
    })?;
    let status = Command::new("git")
        .arg("-C")
        .arg(root.join(engine.source()))
        .args(["apply", "--reverse", "--check"])
        .arg(root.join(engine.patch()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("check required decoder patch")?;
    ensure!(
        status.success(),
        "required decoder receive-setup patch is missing; follow the project build instructions"
    );
    Ok(binary)
}

fn new_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    DirBuilder::new().mode(0o700).create(path).with_context(|| {
        format!(
            "create new output directory {} (existing outputs are preserved)",
            path.display()
        )
    })
}

fn json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn output_path(explicit: Option<&Path>, base: &str) -> PathBuf {
    explicit.map(Path::to_path_buf).unwrap_or_else(|| {
        Path::new(base).join(Utc::now().format("%Y%m%dT%H%M%S%.6fZ").to_string())
    })
}

#[derive(Debug, Serialize)]
struct Diagnostics {
    scanned_bytes: usize,
    prefix_only: bool,
    stream_error_mentions: usize,
    processing_warning_mentions: usize,
}

fn diagnostics(path: &Path) -> Result<Diagnostics> {
    let input = std::fs::File::open(path)?;
    let length = input.metadata()?.len();
    let mut bytes = Vec::new();
    input.take(16 * 1024 * 1024).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
    Ok(Diagnostics {
        scanned_bytes: bytes.len(),
        prefix_only: length > bytes.len() as u64,
        stream_error_mentions: [
            "transfer error",
            "transfer timed out",
            "hit stall",
            "got transfer over",
            "rx error",
        ]
        .iter()
        .map(|message| text.matches(message).count())
        .sum(),
        processing_warning_mentions: ["channelizer too slow", "dropped samples", "overrun"]
            .iter()
            .map(|message| text.matches(message).count())
            .sum::<usize>()
            + ["overflow:", "shed:"]
                .iter()
                .map(|key| {
                    text.split(key)
                        .skip(1)
                        .filter(|suffix| {
                            suffix
                                .trim_start()
                                .split(|c: char| !c.is_ascii_digit())
                                .next()
                                .and_then(|number| number.parse::<u64>().ok())
                                .is_some_and(|n| n > 0)
                        })
                        .count()
                })
                .sum::<usize>(),
    })
}

/// The owned child is always reaped; fallback cleanup cannot report errors.
struct DecoderChild {
    child: Child,
    reaped: bool,
}

impl DecoderChild {
    fn poll(&mut self) -> Result<Option<ExitStatus>> {
        let result = self.child.try_wait().context("poll decoder")?;
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
            .context("interrupt decoder")?;
        if !signal.success() {
            if let Some(status) = self.poll()? {
                return Ok((status, false));
            }
            bail!("could not interrupt decoder");
        }
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.poll()? {
                return Ok((status, false));
            }
            if Instant::now() >= until {
                self.child.kill().context("kill stalled decoder")?;
                let status = self.child.wait().context("reap stalled decoder")?;
                self.reaped = true;
                return Ok((status, true));
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for DecoderChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn capture(root: &Path, executable: &Path, args: &Capture, interrupted: &AtomicBool) -> Result<()> {
    let output = root.join(output_path(args.output.as_deref(), "captures/bluetooth"));
    new_directory(&output)?;
    let output = output.canonicalize()?;
    let candidates = output.join("candidates.pcap");
    let iq = output.join(match args.engine {
        Engine::Ice9 => "iq.sc8q7",
        Engine::BlueDragon => "bursts.bdb",
    });
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .args(["-l", "-i", &args.interface]);
    if args.all_channels {
        command.arg("-a");
    } else {
        command.args([
            "-C",
            &args.channels.to_string(),
            "-c",
            &args.center_mhz.to_string(),
        ]);
    }
    command.arg("-w").arg(&candidates);
    match args.engine {
        Engine::Ice9 => {
            command.args(["-s", "-v"]);
            if args.save_iq {
                command.arg("-d").arg(&iq);
            }
        }
        Engine::BlueDragon => {
            command.args([
                "-g",
                &args.gain.to_string(),
                "--antenna",
                args.antenna.native(),
                "--check-crc",
                "--stats",
                "--no-gpu",
            ]);
            if args.coded_scan {
                command.arg("--coded-scan");
            }
            if args.save_iq {
                command
                    .arg("--write-bursts")
                    .arg(&iq)
                    .arg("--burst-limit-mb")
                    .arg(args.iq_limit_mib.to_string());
            }
        }
    }
    let decoder_args: Vec<_> = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    json_file(
        &output.join("request.json"),
        &json!({"engine":args.engine,"decoder_arguments":decoder_args,"duration_seconds":args.duration,"iq_soft_limit_mib":args.iq_limit_mib,"native_output_soft_limit_mib":args.output_limit_mib,"receive_only_requested":true,"rx_bias_tee_requested":false,"source_patch_check":true,"continuity_guaranteed":false,"timestamp_kind":"host_observation"}),
    )?;
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(output.join("decoder.log"))?;
    command
        .stdout(log.try_clone()?)
        .stderr(log)
        .stdin(Stdio::null());
    let start = Instant::now();
    let mut child = DecoderChild {
        child: command.spawn().context("start Bluetooth decoder")?,
        reaped: false,
    };
    let mut reason = "duration";
    let early = loop {
        if let Some(status) = child.poll()? {
            reason = "decoder_exit";
            break Some(status);
        }
        if interrupted.load(Ordering::Relaxed) {
            reason = "interrupted";
            break None;
        }
        if args.save_iq
            && fs::metadata(&iq)
                .is_ok_and(|m| m.len() >= u64::from(args.iq_limit_mib) * 1024 * 1024)
        {
            reason = "iq_limit";
            break None;
        }
        let bytes = [&candidates, &iq, &output.join("decoder.log")]
            .iter()
            .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
            .sum::<u64>();
        if bytes >= u64::from(args.output_limit_mib) * 1024 * 1024 {
            reason = "output_limit";
            break None;
        }
        if start.elapsed().as_secs_f64() >= args.duration {
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let (status, forced) = match early {
        Some(status) => (status, false),
        None => child.stop()?,
    };
    let diagnostics = diagnostics(&output.join("decoder.log"))?;
    json_file(
        &output.join("capture.json"),
        &json!({"engine":args.engine,"stop_reason":reason,"exit_code":status.code(),"exit_success":status.success(),"forced_kill":forced,"elapsed_wall_seconds":start.elapsed().as_secs_f64(),"iq_bytes":fs::metadata(&iq).ok().map(|m|m.len()),"native_diagnostics":diagnostics,"continuity_guaranteed":false}),
    )?;
    if candidates.is_file() {
        let summary = packets::inspect(
            &candidates,
            Some(&output.join("validated.pcap")),
            Some(&output.join("packets.jsonl")),
        )
        .with_context(|| {
            format!(
                "inspect capture; candidates and logs are preserved in {}",
                output.display()
            )
        })?;
        json_file(&output.join("summary.json"), &summary)?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
    }
    println!("Capture directory: {}", output.display());
    if diagnostics.processing_warning_mentions > 0 {
        eprintln!(
            "Decoder reported processing warnings; inspect decoder.log for possible capture gaps."
        );
    }
    ensure!(
        status.success() && !forced && reason != "decoder_exit",
        "decoder did not shut down successfully; candidates and logs are preserved in {}",
        output.display()
    );
    ensure!(
        candidates.is_file(),
        "decoder produced no PCAP; inspect decoder.log"
    );
    ensure!(
        diagnostics.stream_error_mentions == 0,
        "decoder reported receive/USB errors; validated packets are preserved, but the capture may have gaps (see decoder.log)"
    );
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    if let Action::Inspect { input, output } = &cli.command {
        let base = match &cli.workspace {
            Some(path) => workspace(Some(path.clone()))?,
            None => workspace(None).unwrap_or(std::env::current_dir()?),
        };
        let output = base.join(output_path(
            output.as_deref(),
            "artifacts/bluetooth-inspect",
        ));
        new_directory(&output)?;
        let summary = packets::inspect(
            input,
            Some(&output.join("validated.pcap")),
            Some(&output.join("packets.jsonl")),
        )?;
        json_file(&output.join("summary.json"), &summary)?;
        println!("{}", serde_json::to_string_pretty(&summary)?);
        println!("Inspection directory: {}", output.display());
        return Ok(());
    }
    if let Action::Capture(args) = &cli.command {
        args.validate()?;
    }
    let root = workspace(cli.workspace)?;
    let engine = match &cli.command {
        Action::Doctor { engine } => *engine,
        Action::Capture(args) => args.engine,
        Action::Inspect { .. } => unreachable!(),
    };
    let executable = decoder(&root, cli.decoder.as_deref(), engine)?;
    match cli.command {
        Action::Doctor { .. } => {
            println!(
                "Decoder available; required source patch present. Binary provenance is not attested. No radio opened."
            )
        }
        Action::Capture(args) => {
            let interrupted = Arc::new(AtomicBool::new(false));
            let signal = Arc::clone(&interrupted);
            ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
                .context("install shutdown handler")?;
            capture(&root, &executable, &args, &interrupted)?;
        }
        Action::Inspect { .. } => unreachable!(),
    }
    Ok(())
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
    fn rejects_invalid_capture_before_radio_access() {
        for args in [
            ["--duration", "NaN"],
            ["--duration", "0"],
            ["--channels", "4"],
            ["--interface", "other0"],
            ["--gain", "80"],
        ] {
            let parsed =
                Cli::try_parse_from(["bluetooth-monitor", "capture", args[0], args[1]]).unwrap();
            let Action::Capture(capture) = parsed.command else {
                panic!("capture command");
            };
            assert!(capture.validate().is_err());
        }
    }

    #[test]
    fn preserves_existing_output_directory() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("capture");
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("keep"), "preserved").unwrap();
        assert!(new_directory(&existing).is_err());
        assert_eq!(
            fs::read_to_string(existing.join("keep")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn early_successful_decoder_exit_is_failure_and_defaults_stay_in_workspace() {
        let root = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from(["bluetooth-monitor", "capture", "--duration", "1"]).unwrap();
        let Action::Capture(args) = cli.command else {
            panic!("capture command");
        };
        let error = capture(
            root.path(),
            Path::new("/bin/true"),
            &args,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not shut down successfully"));
        let sessions: Vec<_> = fs::read_dir(root.path().join("captures/bluetooth"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(sessions.len(), 1);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(sessions[0].join("capture.json")).unwrap()).unwrap();
        assert_eq!(manifest["stop_reason"], "decoder_exit");
        assert_eq!(manifest["exit_success"], true);
        assert!(sessions[0].join("request.json").is_file());
    }

    #[test]
    fn interrupts_and_reaps_owned_child() {
        let process = Command::new("sleep").arg("60").spawn().unwrap();
        let mut child = DecoderChild {
            child: process,
            reaped: false,
        };
        let (_, forced) = child.stop().unwrap();
        assert!(!forced);
        assert!(child.reaped);
    }

    #[test]
    fn distinguishes_zero_counters_from_reported_stream_and_processing_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("decoder.log");
        fs::write(&path, "overflow: 0 shed:0\n").unwrap();
        let clean = diagnostics(&path).unwrap();
        assert_eq!(clean.processing_warning_mentions, 0);
        fs::write(&path, "overflow: 3 shed:2\nGot transfer error for buffer\n").unwrap();
        let degraded = diagnostics(&path).unwrap();
        assert_eq!(degraded.processing_warning_mentions, 2);
        assert_eq!(degraded.stream_error_mentions, 1);
    }
}
