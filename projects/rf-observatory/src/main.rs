//! Wideband capture, bounded multichannel receive references and FPGA verification.
//!
//! Host processing is a reference path, not evidence that custom gateware was
//! fitted or loaded. Captures, audio, waveforms and local tool reports stay in
//! ignored output directories. See the project guide for hardware build limits.

#![forbid(unsafe_code)]

mod acquisition;
mod adsb;
mod ais;
mod aprs;
mod dect;
mod dmr;
mod dsp;
mod ecowitt;
mod efergy;
mod event_slices;
mod file_pin;
mod fpga_analysis;
mod fpga_capture;
mod gateware;
mod halow;
mod lora;
mod lte;
mod morse;
mod nr;
mod ook;
mod private_output;
mod profile2_adapter;
mod profile2_analysis;
mod profile2_control;
mod profile2_image;
mod profile2_session;
mod rds;
mod receiver;
mod rs41;
mod scm;
mod signal_profile;
mod spectrum;
mod sweep;
mod transport;
// Profile 2 is decoded independently until its capture path is integrated.
mod tpms;
mod transport_v2;
mod triage;
mod uat;
mod waveform;
mod wh51;
mod ws90;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

pub(crate) const MIN_CENTER_HZ: u64 = 70_000_000;
pub(crate) const MAX_CENTER_HZ: u64 = 6_000_000_000;
const SAMPLE_RATE: u32 = 20_000_000;
const MAX_SAMPLES: u64 = 200_000_000;

#[derive(Debug, Parser)]
#[command(about = "Wideband receiver references and verified FPGA channel-bank experiments")]
struct Cli {
    /// Workspace root used to resolve relative command paths.
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
    /// RAM-load profile 1, capture framed RX, validate it, and restore stock.
    CaptureFpga(fpga_capture::CaptureArgs),
    /// RAM-load profile 2, capture bounded RX, restore stock, and publish evidence.
    ///
    /// This claims the selected bladeRF USB interface, changes its RX
    /// configuration, RAM-loads profile 2 and restores the frozen stock image.
    /// RX1 or RX2 must be selected explicitly; that selection does not identify
    /// which connector is physically antenna-fed. This receive-only action
    /// never flashes, transmits, or enables a bias tee.
    CaptureProfile2(profile2_session::CaptureArgs),
    /// Produce a bounded DC-aware spectrum and unclassified activity regions.
    Survey(Survey),
    /// Plan or record a bounded receive sweep across multiple tunings.
    Sweep(sweep::Args),
    /// Measure saved listening time for selected bands in a completed sweep or series.
    SweepCoverage(triage::coverage::Args),
    /// Run selected protocol analyzers on one completed sweep or series.
    ReplaySweep(triage::replay::Args),
    /// Find transient RF activity and export evidence clips from a saved sweep.
    TriageSweep(triage::Args),
    /// Find frequency bands with repeated detector evidence across a saved sweep series.
    TriageSeries(triage::series::Args),
    /// Check saved follow-up bands and controls against original IQ.
    AnalyzeFollowUp(triage::follow_up::Args),
    /// Decode APRS-format AX.25 frames on one channel in saved IQ.
    IdentifyAprs(aprs::IdentifyAprs),
    /// Decode APRS channels in eligible dwells of a completed sweep or series.
    IdentifyAprsSweep(triage::aprs_sweep::Args),
    /// Identify full DMR Voice Link Control in headers and terminators on one saved channel.
    IdentifyDmr(dmr::Args),
    /// Identify full DMR Voice Link Control across a completed sweep or series.
    IdentifyDmrSweep(triage::channel_sweep::Args),
    /// Search a guarded 12.5 kHz DMR channel grid in a completed sweep or series.
    IdentifyDmrRasterSweep(triage::dmr_raster::Args),
    /// Identify DECT fixed-part broadcasts on one saved channel.
    IdentifyDect(dect::Args),
    /// Identify DECT fixed-part broadcasts across a completed sweep or series.
    IdentifyDectSweep(triage::channel_sweep::Args),
    /// Identify Wi-Fi HaLow 2 MHz S1G beacons on one saved channel.
    IdentifyHalow(halow::Args),
    /// Identify Wi-Fi HaLow 2 MHz S1G beacons across a completed sweep or series.
    IdentifyHalowSweep(triage::channel_sweep::Args),
    /// Identify legacy Itron SCM endpoints on one saved channel.
    IdentifyScm(scm::Args),
    /// Identify legacy Itron SCM endpoints across a completed sweep or series.
    IdentifyScmSweep(triage::channel_sweep::Args),
    /// Identify Itron SCM+ endpoints on one saved channel.
    IdentifyScmPlus(scm::Args),
    /// Identify Itron SCM+ endpoints across a completed sweep or series.
    IdentifyScmPlusSweep(triage::channel_sweep::Args),
    /// Identify VHF AIS stations on one saved channel.
    IdentifyAis(ais::Args),
    /// Identify VHF AIS stations across a completed sweep or series.
    IdentifyAisSweep(triage::channel_sweep::Args),
    /// Identify supported 433 MHz OOK sensors in one saved IQ channel.
    IdentifyOok(ook::Args),
    /// Identify supported OOK sensors across a completed sweep or series.
    IdentifyOokSweep(triage::ook_sweep::Args),
    /// Identify Efergy e2-compatible transmitters on one saved IQ channel.
    IdentifyEfergy(efergy::Args),
    /// Identify Efergy e2-compatible transmitters across a completed sweep or series.
    IdentifyEfergySweep(triage::sensor_sweep::Args),
    /// Decode WH51-format FSK packets on one saved IQ channel.
    IdentifyWh51(wh51::Args),
    /// Decode WH51-format packets across a completed sweep or series.
    IdentifyWh51Sweep(triage::sensor_sweep::Args),
    /// Decode WS90-format FSK packets on one saved IQ channel.
    IdentifyWs90(ws90::Args),
    /// Decode WS90-format packets across a completed sweep or series.
    IdentifyWs90Sweep(triage::sensor_sweep::Args),
    /// Identify supported Pacific TPMS frames on one saved IQ channel.
    IdentifyTpms(tpms::Args),
    /// Identify supported Pacific TPMS frames across a completed sweep or series.
    IdentifyTpmsSweep(triage::sensor_sweep::Args),
    /// Identify Vaisala RS41 radiosondes on one saved IQ channel.
    IdentifyRs41(rs41::Args),
    /// Identify Vaisala RS41 radiosondes across a completed sweep or series.
    IdentifyRs41Sweep(triage::sensor_sweep::Args),
    /// Search a Vaisala RS41 channel raster in a completed sweep or series.
    IdentifyRs41RasterSweep(triage::rs41_raster::Args),
    /// Decode validated broadcast FM RDS groups and text from one saved channel.
    IdentifyRds(rds::Args),
    /// Decode broadcast FM RDS stations in a completed sweep or series.
    IdentifyRdsSweep(triage::rds_sweep::Args),
    /// Identify LTE FDD cells from broadcast synchronization in saved IQ.
    IdentifyLte(lte::Args),
    /// Identify LTE cells across a carrier grid in one pass over saved IQ.
    IdentifyLteRaster(lte::raster::Args),
    /// Identify LTE cells and carrier coverage in a completed sweep or series.
    IdentifyLteSweep(triage::lte_sweep::Args),
    /// Identify repeated 5G NR broadcast synchronization in one saved IQ channel.
    IdentifyNr(nr::Args),
    /// Identify 5G NR synchronization profiles across a completed sweep or series.
    IdentifyNrSweep(triage::nr_sweep::Args),
    /// Decode Morse from keyed RF or a selected narrowband FM tone in saved IQ.
    IdentifyMorse(morse::Args),
    /// Decode configured Morse profiles in a completed sweep or series.
    IdentifyMorseSweep(triage::morse_sweep::Args),
    /// Recover validated 1090 MHz extended squitter frames from saved IQ.
    IdentifyAdsb(adsb::Args),
    /// Recover validated 1090 MHz frames from a completed sweep or series.
    IdentifyAdsbSweep(triage::adsb_sweep::Args),
    /// Recover verified 978 MHz UAT airborne frames from saved IQ.
    IdentifyUat(uat::Args),
    /// Recover verified 978 MHz UAT airborne frames from a completed sweep or series.
    IdentifyUatSweep(triage::channel_sweep::CaptureArgs),
    /// Export bounded full-band IQ ranges from a verified dense event report.
    SliceEvents(event_slices::SliceEvents),
    /// Measure candidate modulation features in a saved IQ clip.
    ProbeWaveform(waveform::ProbeWaveform),
    /// Locate power bursts in a selected band of a saved IQ clip.
    ProbeBursts(waveform::burst::ProbeBursts),
    /// Search a saved IQ interval for repeated cyclic-prefix structure.
    ProbeOfdm(waveform::ofdm::ProbeOfdm),
    /// Search a saved IQ channel for repeated LoRa chirps and an opposite-slope delimiter.
    #[command(name = "probe-lora")]
    ProbeLoRa(lora::ProbeLoRa),
    /// Search multiple LoRa profiles across a completed sweep or series.
    #[command(name = "probe-lora-sweep")]
    ProbeLoRaSweep(triage::lora_sweep::Args),
    /// Decode CRC-valid LoRa frames from one channel in a saved IQ interval.
    #[command(name = "decode-lora")]
    DecodeLoRa(lora::decode::DecodeLoRa),
    /// Decode CRC-valid LoRa frames in a completed sweep or series.
    #[command(name = "decode-lora-sweep")]
    DecodeLoRaSweep(triage::lora_decode_sweep::Args),
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
        /// Validate the complete stream without producing channel IQ or audio.
        #[arg(long)]
        validate_only: bool,
        /// Bound derived output; defaults to one million records.
        #[arg(long)]
        max_records: Option<u64>,
    },
    /// Validate a complete saved profile-2 CONFIG/DATA stream offline on Linux.
    ValidateProfile2 { input: PathBuf },
    /// Analyze saved profile-2 payloads offline with bounded memory.
    ///
    /// The default validates and analyzes the complete file. A DATA-record
    /// limit below the available extent selects and labels a CONFIG-plus-DATA
    /// prefix instead. This action opens no device and performs no USB, FPGA,
    /// RF, bias-tee or TX work.
    AnalyzeProfile2(AnalyzeProfile2),
    /// Validate and relatively analyze saved profile-1 channel samples offline.
    AnalyzeFpga(AnalyzeFpga),
    /// Verify the locally approved Profile-2 FPGA image bundle offline on Linux.
    VerifyProfile2Image {
        /// Strict bundle manifest, relative to the workspace by default.
        #[arg(long)]
        manifest: PathBuf,
        /// Separately recorded 64-character lowercase manifest SHA-256.
        #[arg(long)]
        manifest_sha256: String,
    },
    /// Exercise profile-2 control and guarded session policy with CPU fixtures.
    VerifyProfile2Adapter {
        /// Fixture adapter executable, relative to the workspace by default.
        #[arg(long, default_value = "target/debug/bladerf-profile2-adapter")]
        adapter: PathBuf,
        /// Seconds allowed for each framed fixture transaction (maximum 60).
        #[arg(long, default_value_t = 2.0)]
        timeout: f64,
    },
    /// Open one already loaded profile-2 image and verify typed register control.
    ///
    /// This claims the selected bladeRF USB interface and changes its transport
    /// alternate settings. It does not load an FPGA image, configure RF, capture
    /// samples, enable a bias tee or transmit.
    VerifyProfile2NativeAdapter {
        /// Exact adapter executable to launch; no PATH lookup is used.
        #[arg(long)]
        adapter: PathBuf,
        /// Exact specialized libbladeRF file selected by its trusted digest.
        #[arg(long)]
        library: PathBuf,
        /// Expected 64-character lowercase hexadecimal library SHA-256.
        #[arg(long)]
        sha256: String,
        /// Complete 32-character lowercase hexadecimal bladeRF serial.
        #[arg(long)]
        serial: String,
        /// Seconds allowed for each framed/native transaction (maximum 60).
        #[arg(long, default_value_t = 2.0)]
        timeout: f64,
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
    /// Number of FFT windows distributed across the selected prefix.
    #[arg(long, default_value_t = 128)]
    windows: usize,
    /// Analyze every complete consecutive FFT window in the selected file prefix.
    #[arg(long, conflicts_with = "windows")]
    dense: bool,
    /// Also report time-frequency events from the dense survey in events.json.
    #[arg(long, requires = "dense")]
    events: bool,
    /// Extend event detection to the nominal 20 MHz sampled band.
    #[arg(long, requires = "events")]
    include_band_edges: bool,
    /// Use half-overlapping FFT windows for event detection.
    #[arg(long, requires = "events")]
    overlap_events: bool,
    /// Record all runs of active FFT windows, including activity outside retained components.
    #[arg(long, requires = "events")]
    active_window_ranges: bool,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AnalyzeFpga {
    input: PathBuf,
    /// Optional unverified RF center used only to label spectrum frequencies.
    #[arg(long)]
    center_mhz: Option<f64>,
    /// Analyze an explicit prefix instead of the complete stream.
    #[arg(long)]
    max_records: Option<u64>,
    #[arg(long, default_value_t = 128)]
    windows: usize,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AnalyzeProfile2 {
    /// Saved profile-2 CONFIG/DATA stream to validate and analyze.
    input: PathBuf,
    /// Analyze at most this many DATA records after CONFIG; a shorter selection is a prefix.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    max_data_records: Option<u64>,
    /// Fresh private directory for the terminal analysis.json report.
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
    let mut file = BufWriter::new(new_file(path)?);
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.get_ref().sync_all()?;
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
        mhz.is_finite()
            && (MIN_CENTER_HZ as f64 / 1_000_000.0..=MAX_CENTER_HZ as f64 / 1_000_000.0)
                .contains(&mhz),
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
        Action::Capture(_)
            | Action::Sweep(_)
            | Action::CaptureFpga(_)
            | Action::CaptureProfile2(_)
            | Action::Doctor
            | Action::VerifyHdl { .. }
            | Action::VerifyProfile2NativeAdapter { .. }
    );
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))?;
    if let Action::Sweep(args) = &cli.command
        && args.plan_only
    {
        return sweep::print_plan(args, &stop);
    }
    let root = if offline && cli.workspace.is_none() {
        workspace(None).or_else(|_| std::env::current_dir())?
    } else {
        workspace(cli.workspace)?
    };
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
        Action::Sweep(args) => sweep::run(&root, args, &stop)?,
        Action::SweepCoverage(args) => triage::coverage::run(&root, args, &stop)?,
        Action::ReplaySweep(args) => triage::replay::run(&root, args, &stop)?,
        Action::TriageSweep(args) => triage::run(&root, args, &stop)?,
        Action::TriageSeries(args) => triage::series::run(&root, args, &stop)?,
        Action::AnalyzeFollowUp(args) => triage::follow_up::run(&root, args, &stop)?,
        Action::IdentifyAprs(args) => aprs::identify(&root, args, &stop)?,
        Action::IdentifyAprsSweep(args) => triage::aprs_sweep::identify(&root, args, &stop)?,
        Action::IdentifyDmr(args) => dmr::identify(&root, args, &stop)?,
        Action::IdentifyDmrSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::Dmr,
                &stop,
            )?;
        }
        Action::IdentifyDmrRasterSweep(args) => triage::dmr_raster::identify(&root, args, &stop)?,
        Action::IdentifyDect(args) => dect::identify(&root, args, &stop)?,
        Action::IdentifyDectSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::Dect,
                &stop,
            )?;
        }
        Action::IdentifyHalow(args) => halow::identify(&root, args, &stop)?,
        Action::IdentifyHalowSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::Halow,
                &stop,
            )?;
        }
        Action::IdentifyScm(args) => scm::identify(&root, args, scm::Format::Legacy, &stop)?,
        Action::IdentifyScmSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::Scm,
                &stop,
            )?;
        }
        Action::IdentifyScmPlus(args) => {
            scm::identify(&root, args, scm::Format::Plus, &stop)?;
        }
        Action::IdentifyScmPlusSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::ScmPlus,
                &stop,
            )?;
        }
        Action::IdentifyAis(args) => ais::identify(&root, args, &stop)?,
        Action::IdentifyAisSweep(args) => {
            triage::channel_sweep::identify(
                &root,
                args,
                triage::channel_sweep::Protocol::Ais,
                &stop,
            )?;
        }
        Action::IdentifyOok(args) => ook::identify(&root, args, &stop)?,
        Action::IdentifyOokSweep(args) => triage::ook_sweep::run(&root, args, &stop)?,
        Action::IdentifyEfergy(args) => efergy::identify(&root, args, &stop)?,
        Action::IdentifyEfergySweep(args) => {
            triage::sensor_sweep::run(&root, args, triage::sensor_sweep::Sensor::Efergy, &stop)?;
        }
        Action::IdentifyWh51(args) => wh51::identify(&root, args, &stop)?,
        Action::IdentifyWh51Sweep(args) => {
            triage::sensor_sweep::run(&root, args, triage::sensor_sweep::Sensor::Wh51, &stop)?;
        }
        Action::IdentifyWs90(args) => ws90::identify(&root, args, &stop)?,
        Action::IdentifyWs90Sweep(args) => {
            triage::sensor_sweep::run(&root, args, triage::sensor_sweep::Sensor::Ws90, &stop)?;
        }
        Action::IdentifyTpms(args) => tpms::identify(&root, args, &stop)?,
        Action::IdentifyTpmsSweep(args) => {
            triage::sensor_sweep::run(&root, args, triage::sensor_sweep::Sensor::Tpms, &stop)?;
        }
        Action::IdentifyRs41(args) => rs41::identify(&root, args, &stop)?,
        Action::IdentifyRs41Sweep(args) => {
            triage::sensor_sweep::run(&root, args, triage::sensor_sweep::Sensor::Rs41, &stop)?;
        }
        Action::IdentifyRs41RasterSweep(args) => triage::rs41_raster::identify(&root, args, &stop)?,
        Action::IdentifyRds(args) => rds::identify(&root, args, &stop)?,
        Action::IdentifyRdsSweep(args) => triage::rds_sweep::identify(&root, args, &stop)?,
        Action::IdentifyLte(args) => lte::identify(&root, args, &stop)?,
        Action::IdentifyLteRaster(args) => lte::raster::identify(&root, args, &stop)?,
        Action::IdentifyLteSweep(args) => triage::lte_sweep::identify(&root, args, &stop)?,
        Action::IdentifyNr(args) => nr::identify(&root, args, &stop)?,
        Action::IdentifyNrSweep(args) => triage::nr_sweep::identify(&root, args, &stop)?,
        Action::IdentifyMorse(args) => morse::identify(&root, args, &stop)?,
        Action::IdentifyMorseSweep(args) => triage::morse_sweep::identify(&root, args, &stop)?,
        Action::IdentifyAdsb(args) => adsb::identify(&root, args, &stop)?,
        Action::IdentifyAdsbSweep(args) => triage::adsb_sweep::identify(&root, args, &stop)?,
        Action::IdentifyUat(args) => uat::identify(&root, args, &stop)?,
        Action::IdentifyUatSweep(args) => triage::channel_sweep::identify_uat(&root, args, &stop)?,
        Action::CaptureFpga(args) => fpga_capture::capture(&root, args, &stop)?,
        Action::CaptureProfile2(args) => {
            let receipt = profile2_session::capture(&root, args, Arc::clone(&stop))?;
            println!(
                "Profile-2 receive session completed on {}; durable capture and stock-restoration evidence: {}. No FPGA flash, transmit stream, or bias tee was enabled.",
                receipt.receiver(),
                receipt.output().display()
            );
        }
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
            let (report, events) = if args.events {
                let band = if args.include_band_edges {
                    spectrum::EventBand::Full
                } else {
                    spectrum::EventBand::Inner
                };
                let stride = if args.overlap_events {
                    spectrum::EventStride::HalfWindow
                } else {
                    spectrum::EventStride::Contiguous
                };
                let (report, events) = spectrum::survey_with_events(
                    &input,
                    center,
                    args.max_samples,
                    band,
                    stride,
                    args.active_window_ranges,
                    &stop,
                )?;
                (report, Some(events))
            } else if args.dense {
                (
                    spectrum::survey_dense(&input, center, args.max_samples, &stop)?,
                    None,
                )
            } else {
                (
                    spectrum::survey(&input, center, args.max_samples, args.windows, &stop)?,
                    None,
                )
            };
            json_file(&output.join("spectrum.json"), &report)?;
            let mut csv = BufWriter::new(new_file(&output.join("spectrum.csv"))?);
            writeln!(csv, "frequency_hz,relative_power_db")?;
            for bin in &report.bins {
                writeln!(csv, "{:.3},{:.6}", bin.frequency_hz, bin.relative_power_db)?;
            }
            csv.flush()?;
            csv.get_ref().sync_all()?;
            if let Some(events) = events {
                json_file(&output.join("events.json"), &events)?;
            }
            println!(
                "Averaged {} FFT windows; energy-ranked centers outside the center guard (not verified stations):",
                report.windows
            );
            for candidate in &report.candidates {
                println!(
                    "{:.3} MHz ({:.1} dB relative)",
                    candidate.frequency_hz / 1_000_000.0,
                    candidate.relative_power_db
                );
            }
            println!("Unclassified activity regions (relative heuristic, not protocol identity):");
            if report.activity_detection.regions.is_empty() {
                println!("none");
            }
            for region in &report.activity_detection.regions {
                println!(
                    "{:.3}-{:.3} MHz, strongest trigger cell {:.3} MHz, {} cells, mean excess {:.1} dB, transient excess {:.1} dB, maximum sampled-window hit fraction {:.3} (not occupancy)",
                    region.lower_frequency_hz / 1_000_000.0,
                    region.upper_frequency_hz / 1_000_000.0,
                    region.strongest_trigger_cell_frequency_hz / 1_000_000.0,
                    region.cell_count,
                    region.maximum_mean_excess_db,
                    region.maximum_transient_excess_db,
                    region.maximum_sampled_window_hit_fraction,
                );
            }
            println!("Output: {}", output.display());
        }
        Action::SliceEvents(args) => event_slices::export(&root, &args, &stop)?,
        Action::ProbeWaveform(args) => waveform::probe(&root, &args, &stop)?,
        Action::ProbeLoRa(args) => lora::probe(&root, args, &stop)?,
        Action::ProbeLoRaSweep(args) => triage::lora_sweep::probe(&root, args, &stop)?,
        Action::DecodeLoRa(args) => lora::decode::run(&root, args, &stop)?,
        Action::DecodeLoRaSweep(args) => triage::lora_decode_sweep::decode(&root, args, &stop)?,
        Action::ProbeBursts(args) => waveform::burst::probe(&root, &args, &stop)?,
        Action::ProbeOfdm(args) => waveform::ofdm::probe(&root, &args, &stop)?,
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
            validate_only,
            max_records,
        } => {
            ensure!(
                !validate_only || max_records.is_none(),
                "--max-records is only valid when producing derived outputs"
            );
            let derived_limit = if validate_only {
                None
            } else {
                let limit = max_records.unwrap_or(1_000_000);
                ensure!(
                    (1..=1_000_000).contains(&limit),
                    "max records must be 1..=1000000"
                );
                Some(limit)
            };
            let input = input.canonicalize()?;
            let output = directory(
                &root,
                output.as_deref(),
                "artifacts/rf-observatory/fpga-stream",
            )?;
            let summary = if validate_only {
                transport::validate(&input, &stop)?
            } else {
                transport::decode(
                    &input,
                    &output,
                    derived_limit.context("derived record limit")?,
                    &stop,
                )?
            };
            json_file(&output.join("stream.json"), &summary)?;
            println!(
                "{} {} custom-format records. Output: {}",
                if validate_only {
                    "Validated"
                } else {
                    "Decoded"
                },
                summary.records,
                output.display()
            );
        }
        Action::ValidateProfile2 { input } => {
            ensure!(
                !stop.load(Ordering::Relaxed),
                "profile-2 file validation was interrupted before opening the input"
            );
            let validated = transport_v2::validate_file(&input, &stop)?;
            println!(
                "Validated one CONFIG and {} profile-2 DATA records: epoch {}, {}, source indices {}..={}, {} bytes, SHA-256 {}. This was offline file validation only.",
                validated.data_records,
                validated.config.epoch,
                if validated.config.status.rx2 {
                    "RX2"
                } else {
                    "RX1"
                },
                validated.first_source_index,
                validated.last_source_index,
                validated.processed_bytes,
                validated.processed_sha256
            );
        }
        Action::AnalyzeProfile2(args) => {
            if let Some(limit) = args.max_data_records {
                ensure!(limit > 0, "max data records must be positive");
            }
            let analysis = profile2_analysis::analyze(&args.input, args.max_data_records, &stop)?;
            let output = directory(
                &root,
                args.output.as_deref(),
                "artifacts/rf-observatory/profile2-analysis",
            )?;
            profile2_analysis::publish(&output, &analysis, &stop)?;
            println!(
                "Analyzed {} validated profile-2 DATA records from {}. Offline digital-count report: {}",
                analysis.data_records(),
                analysis.receiver(),
                output.display()
            );
        }
        Action::AnalyzeFpga(args) => {
            if let Some(limit) = args.max_records {
                ensure!(limit > 0, "max records must be positive");
            }
            ensure!((1..=512).contains(&args.windows), "windows must be 1..=512");
            let center = args.center_mhz.map(center_hz).transpose()?;
            let input = args.input.canonicalize()?;
            let output = directory(
                &root,
                args.output.as_deref(),
                "artifacts/rf-observatory/fpga-analysis",
            )?;
            let analysis =
                fpga_analysis::analyze(&input, args.max_records, args.windows, center, &stop)?;
            fpga_analysis::publish(&output, &analysis, &stop)?;
            println!(
                "Analyzed {} validated profile-1 records from {}. Relative digital results: {}",
                analysis.records(),
                analysis.receiver().unwrap_or("unknown receiver"),
                output.display()
            );
        }
        Action::VerifyProfile2Image {
            manifest,
            manifest_sha256,
        } => {
            let manifest = if manifest.is_absolute() {
                manifest
            } else {
                root.join(manifest)
            };
            let report = profile2_image::verify(&root, &manifest, &manifest_sha256, &stop)?;
            println!(
                "Qualified profile-2 image verified offline: {} bytes, SHA-256 {}. Manifest: {} (SHA-256 {}). No device was opened and no FPGA, USB, RF, capture, bias-tee or TX operation was performed.",
                report.image_bytes(),
                report.image_sha256(),
                report.manifest().display(),
                report.manifest_sha256()
            );
            println!("Image at verification time: {}", report.image().display());
        }
        Action::VerifyProfile2Adapter { adapter, timeout } => {
            let timeout = budget(
                timeout,
                bladerf_profile2_ipc::MAX_TRANSACTION_TIMEOUT.as_secs_f64(),
            )?;
            let adapter = if adapter.is_absolute() {
                adapter
            } else {
                root.join(adapter)
            };
            let applied = profile2_adapter::verify_fixture(&adapter, timeout)?;
            profile2_session::verify_policy()?;
            println!(
                "Profile-2 fixture verified through Controller: epoch {}, words {:08x?}. The guarded session policy also passed same-handle stock restoration and ambiguous-load fresh-worker recovery models. This action used only CPU fixtures and performed no device, USB, FPGA-load, RF, capture, bias-tee, flash or TX operation.",
                applied.epoch(),
                applied.phase_words()
            );
        }
        Action::VerifyProfile2NativeAdapter {
            adapter,
            library,
            sha256,
            serial,
            timeout,
        } => {
            let timeout = budget(
                timeout,
                bladerf_profile2_ipc::MAX_TRANSACTION_TIMEOUT.as_secs_f64(),
            )?;
            ensure!(
                !stop.load(Ordering::Relaxed),
                "native profile-2 verification was interrupted before startup"
            );
            let adapter = if adapter.is_absolute() {
                adapter
            } else {
                root.join(adapter)
            };
            let library = if library.is_absolute() {
                library
            } else {
                root.join(library)
            };
            let applied = profile2_adapter::verify_native(
                &adapter, &library, &sha256, &serial, timeout, &stop,
            )?;
            println!(
                "Profile-2 native register control verified through Controller: epoch {}, words {:08x?}. The child claimed USB, changed its transport alternate settings and changed only allowlisted profile-2 registers; this action did not load an FPGA image, configure RF, capture, enable a bias tee or transmit.",
                applied.epoch(),
                applied.phase_words()
            );
        }
    }
    Ok(())
}

fn capture(root: &Path, args: Capture, stop: &AtomicBool) -> Result<()> {
    let center = center_hz(args.center_mhz)?;
    let settings = acquisition::Settings::new(center, args.duration, args.antenna, args.gain)?;
    if !args.stations.is_empty() {
        station_words(center, &args.stations)?;
    }
    let program = acquisition::resolve_program(root, args.receiver.as_deref())?;
    let output = directory(root, args.output.as_deref(), "captures/rf-observatory")?;
    let window = acquisition::record(root, &program, &output, settings, stop)?;
    if !args.stations.is_empty() {
        let words = station_words(window.center_hz, &args.stations)?;
        let report = receiver::receive(
            &window.iq,
            &output,
            &words,
            settings.samples,
            Duration::from_secs(120),
            stop,
        )?;
        json_file(&output.join("receive.json"), &report)?;
    }
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "Recorded {} complex samples. Output: {}",
        settings.samples,
        output.display()
    )?;
    stdout.flush().context("flush capture result")
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
    use clap::CommandFactory;

    #[test]
    fn dense_survey_is_opt_in_and_conflicts_with_an_explicit_window_count() {
        let args = |extra: &[&str]| {
            Cli::try_parse_from(
                ["rf-observatory", "survey", "input.sc16q11"]
                    .into_iter()
                    .chain(extra.iter().copied()),
            )
            .map(|cli| cli.command)
        };
        assert!(matches!(
            args(&[]).unwrap(),
            Action::Survey(Survey {
                dense: false,
                windows: 128,
                ..
            })
        ));
        assert!(matches!(
            args(&["--dense"]).unwrap(),
            Action::Survey(Survey { dense: true, .. })
        ));
        assert!(args(&["--dense", "--windows", "128"]).is_err());
        assert!(args(&["--events"]).is_err());
        assert!(matches!(
            args(&["--dense", "--events"]).unwrap(),
            Action::Survey(Survey {
                events: true,
                include_band_edges: false,
                active_window_ranges: false,
                ..
            })
        ));
        assert!(args(&["--dense", "--include-band-edges"]).is_err());
        assert!(args(&["--dense", "--overlap-events"]).is_err());
        assert!(args(&["--dense", "--active-window-ranges"]).is_err());
        assert!(args(&["--events", "--active-window-ranges"]).is_err());
        assert!(matches!(
            args(&["--dense", "--events", "--include-band-edges"]).unwrap(),
            Action::Survey(Survey {
                events: true,
                include_band_edges: true,
                ..
            })
        ));
        assert!(matches!(
            args(&["--dense", "--events", "--overlap-events"]).unwrap(),
            Action::Survey(Survey {
                overlap_events: true,
                ..
            })
        ));
        assert!(matches!(
            args(&["--dense", "--events", "--active-window-ranges"]).unwrap(),
            Action::Survey(Survey {
                active_window_ranges: true,
                ..
            })
        ));
    }

    fn profile2_capture_arguments(receiver: &'static str) -> Vec<&'static str> {
        vec![
            "rf-observatory",
            "capture-profile2",
            "--receiver",
            receiver,
            "--center-hz",
            "98100000",
            "--gain-db",
            "40",
            "--phase-0",
            "0x01234567",
            "--phase-1",
            "2309737967",
            "--phase-2",
            "0x80000001",
            "--phase-3",
            "3735928559",
            "--manifest",
            "manifest.json",
            "--manifest-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--adapter",
            "bladerf-profile2-adapter",
            "--adapter-sha256",
            "123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "--library",
            "libbladeRF.so.2",
            "--library-sha256",
            "23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01",
            "--stock-image",
            "stock.rbf",
            "--serial",
            "0123456789abcdef0123456789abcdef",
            "--output",
            "private/session",
        ]
    }

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
    fn native_profile2_action_requires_every_identity_argument() {
        assert!(Cli::try_parse_from(["rf-observatory", "verify-profile2-native-adapter"]).is_err());
        let cli = Cli::try_parse_from([
            "rf-observatory",
            "verify-profile2-native-adapter",
            "--adapter",
            "/opt/bladerf-profile2-adapter",
            "--library",
            "/opt/libbladeRF.so.2",
            "--sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--serial",
            "0123456789abcdef0123456789abcdef",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Action::VerifyProfile2NativeAdapter { timeout, .. } if timeout == 2.0
        ));

        assert!(matches!(
            Cli::try_parse_from(["rf-observatory", "verify-profile2-adapter"])
                .unwrap()
                .command,
            Action::VerifyProfile2Adapter { timeout, .. } if timeout == 2.0
        ));
    }

    #[test]
    fn offline_profile2_image_action_requires_manifest_and_external_digest() {
        assert!(Cli::try_parse_from(["rf-observatory", "verify-profile2-image"]).is_err());
        assert!(
            Cli::try_parse_from([
                "rf-observatory",
                "verify-profile2-image",
                "--manifest",
                "manifest.json",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "rf-observatory",
            "verify-profile2-image",
            "--manifest",
            "manifest.json",
            "--manifest-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .unwrap();
        assert!(matches!(cli.command, Action::VerifyProfile2Image { .. }));
    }

    #[test]
    fn profile2_file_validation_has_a_separate_offline_action() {
        assert!(Cli::try_parse_from(["rf-observatory", "validate-profile2"]).is_err());
        assert!(matches!(
            Cli::try_parse_from([
                "rf-observatory",
                "validate-profile2",
                "saved-profile2.frames",
            ])
            .unwrap()
            .command,
            Action::ValidateProfile2 { .. }
        ));
    }

    #[test]
    fn profile2_analysis_is_an_explicit_offline_data_prefix_action() {
        assert!(Cli::try_parse_from(["rf-observatory", "analyze-profile2"]).is_err());
        let default = Cli::try_parse_from([
            "rf-observatory",
            "analyze-profile2",
            "saved-profile2.frames",
        ])
        .unwrap();
        assert!(matches!(
            default.command,
            Action::AnalyzeProfile2(AnalyzeProfile2 {
                max_data_records: None,
                output: None,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from([
                "rf-observatory",
                "analyze-profile2",
                "saved-profile2.frames",
                "--max-data-records",
                "0",
            ])
            .is_err()
        );

        let cli = Cli::try_parse_from([
            "rf-observatory",
            "analyze-profile2",
            "saved-profile2.frames",
            "--max-data-records",
            "100000",
            "--output",
            "private/profile2-analysis",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Action::AnalyzeProfile2(AnalyzeProfile2 {
                max_data_records: Some(100_000),
                ..
            })
        ));

        let mut command = Cli::command();
        let action = command
            .find_subcommand_mut("analyze-profile2")
            .expect("profile-2 analysis action");
        let help = action.render_long_help().to_string().to_lowercase();
        for required in [
            "offline",
            "bounded memory",
            "complete file",
            "prefix",
            "data records",
            "opens no device",
            "--max-data-records",
            "--output",
        ] {
            assert!(help.contains(required), "missing help text: {required}");
        }
    }

    #[test]
    fn profile2_capture_requires_an_explicit_receiver_and_complete_identity() {
        assert!(Cli::try_parse_from(["rf-observatory", "capture-profile2"]).is_err());

        let cli = Cli::try_parse_from(profile2_capture_arguments("rx1")).unwrap();
        assert!(matches!(cli.command, Action::CaptureProfile2(_)));
        let cli = Cli::try_parse_from(profile2_capture_arguments("rx2")).unwrap();
        assert!(matches!(cli.command, Action::CaptureProfile2(_)));

        for required in [
            "--receiver",
            "--center-hz",
            "--gain-db",
            "--phase-0",
            "--phase-1",
            "--phase-2",
            "--phase-3",
            "--manifest",
            "--manifest-sha256",
            "--adapter",
            "--adapter-sha256",
            "--library",
            "--library-sha256",
            "--stock-image",
            "--serial",
            "--output",
        ] {
            let mut incomplete = profile2_capture_arguments("rx1");
            let index = incomplete
                .iter()
                .position(|argument| *argument == required)
                .unwrap();
            incomplete.drain(index..=index + 1);
            assert!(
                Cli::try_parse_from(incomplete).is_err(),
                "accepted command without {required}"
            );
        }

        for receiver in ["both", "rx0", "rx3"] {
            assert!(Cli::try_parse_from(profile2_capture_arguments(receiver)).is_err());
        }
    }

    #[test]
    fn profile2_capture_help_exposes_only_the_receive_only_ram_surface() {
        let mut command = Cli::command();
        let action = command
            .find_subcommand_mut("capture-profile2")
            .expect("profile-2 capture action");
        let help = action.render_long_help().to_string().to_lowercase();
        for required in [
            "ram-load",
            "rx1",
            "rx2",
            "does not identify",
            "receive-only",
            "never flashes",
            "transmit",
            "bias tee",
            "claims the selected bladerf usb interface",
            "changes its rx configuration",
            "restores the frozen stock image",
        ] {
            assert!(help.contains(required), "missing help text: {required}");
        }

        let argument_ids = action
            .get_arguments()
            .map(|argument| argument.get_id().as_str())
            .collect::<Vec<_>>();
        for forbidden in [
            "antenna",
            "device",
            "tx",
            "transmit",
            "flash",
            "bias_tee",
            "loopback",
            "custom_image",
            "image_role",
            "sample_rate",
            "bandwidth",
            "stock_sha256",
        ] {
            assert!(!argument_ids.contains(&forbidden));
        }
        for forbidden_option in [
            "--antenna",
            "--device",
            "--tx",
            "--transmit",
            "--flash",
            "--bias-tee",
            "--loopback",
            "--custom-image",
            "--image-role",
            "--sample-rate",
            "--bandwidth",
            "--stock-sha256",
        ] {
            assert!(!help.contains(forbidden_option));
        }
        let receiver = action
            .get_arguments()
            .find(|argument| argument.get_id() == "receiver")
            .expect("required receiver argument");
        assert!(receiver.is_required_set());
        assert!(receiver.get_default_values().is_empty());

        let root_help = Cli::command().render_long_help().to_string();
        assert!(root_help.contains("Workspace root used to resolve relative command paths"));
    }
}
