//! Private, synchronous host application for the bladeRF micro xA9 lab.
//!
//! Run `cargo run --locked -- --help` for commands. Native dependencies are
//! optional so `--no-default-features -- setup-fedora` can bootstrap a new host.
//! All radio operations receive only. See `docs/SETUP.md` for native prerequisites
//! and `docs/RUST-TOOLCHAIN.md` for build features and dependencies.

#![forbid(unsafe_code)]

#[cfg(feature = "analysis")]
mod analysis;
mod host;
#[cfg(feature = "radio")]
mod radio;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

/// Receive-only SDR experiments and reproducible host setup.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Workspace directory; defaults to discovery from the current directory.
    #[arg(long, global = true)]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Capture RX1 or RX2 IQ with actual settings in a JSON sidecar.
    #[cfg(feature = "radio")]
    Capture(radio::CaptureArgs),
    /// Produce IQ statistics and a PNG power spectral density plot.
    #[cfg(feature = "analysis")]
    Analyze(analysis::AnalyzeArgs),
    /// Validate a finite RX1 stream through Rust's safe SoapySDR bindings.
    #[cfg(feature = "radio")]
    SoapyRx(radio::SoapyRxArgs),
    /// Inspect the device and installed native tools without starting a stream.
    Doctor,
    /// Download release images and verify SHA256; never program the device.
    FetchImages,
    /// Rebuild native host dependencies on Fedora; never flash the board.
    SetupFedora(host::SetupArgs),
    /// Validate finite receive through an upstream-generated GNU Radio graph.
    GnuradioRx(host::GnuradioArgs),
}

fn workspace(path: Option<PathBuf>) -> Result<PathBuf> {
    let start = path.unwrap_or(std::env::current_dir().context("read current directory")?);
    let start = start
        .canonicalize()
        .context("resolve workspace directory")?;
    for candidate in start.ancestors() {
        if candidate.join("config/images.sha256").is_file()
            && candidate.join(".gitmodules").is_file()
        {
            return Ok(candidate.to_path_buf());
        }
    }
    bail!(
        "no bladeRF workspace found from {}; use --workspace",
        start.display()
    )
}

fn run(cli: Cli) -> Result<()> {
    let root = workspace(cli.workspace)?;
    // Set the directory before creating streams, children, or worker threads.
    // This private CLI executes one command per process.
    std::env::set_current_dir(&root).context("enter workspace directory")?;
    dispatch(cli.command)
}

fn dispatch(command: Command) -> Result<()> {
    match command {
        #[cfg(feature = "radio")]
        Command::Capture(args) => radio::capture(args),
        #[cfg(feature = "analysis")]
        Command::Analyze(args) => analysis::run(args),
        #[cfg(feature = "radio")]
        Command::SoapyRx(args) => radio::soapy_rx(args),
        Command::Doctor => host::doctor(),
        Command::FetchImages => host::fetch_images(),
        Command::SetupFedora(args) => host::setup_fedora(args),
        Command::GnuradioRx(args) => host::gnuradio_rx(args),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
