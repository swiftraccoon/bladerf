//! Validate the documentation catalog and reproduce its handbook reference.
//!
//! Run from the workspace root: `cargo run --locked -p bladerf-docs -- check`.
//! This host-only tool performs no network requests or radio operations.

#![forbid(unsafe_code)]

mod catalog;
mod json;
mod render;

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};

use crate::catalog::Catalog;

#[derive(Debug, Parser)]
#[command(about = "Validate source-qualified bladeRF documentation without hardware or networking")]
struct Cli {
    #[arg(long, global = true, default_value = "docs/knowledge/catalog.json")]
    catalog: PathBuf,
    #[arg(
        long,
        global = true,
        default_value = "docs/knowledge/catalog.schema.json"
    )]
    schema: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check schema, declared source identity, evidence locators, and record consistency.
    Check,
    /// Reproduce the reference page; use --check to detect drift without writing.
    Render {
        #[arg(long, default_value = "docs/handbook/fact-catalog.md")]
        output: PathBuf,
        #[arg(long)]
        check: bool,
    },
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary output in {}", parent.display()))?;
    output
        .write_all(contents)
        .context("write generated reference")?;
    output.flush().context("flush generated reference")?;
    output
        .as_file()
        .sync_all()
        .context("sync generated reference")?;
    output
        .persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let catalog = Catalog::load(&cli.schema, &cli.catalog)?;
    match cli.command {
        Command::Check => {
            println!(
                "Catalog valid: {} claims, {} sources, {} issues",
                catalog.claims()?.len(),
                catalog.sources()?.len(),
                catalog.issues()?.len()
            );
        }
        Command::Render { output, check } => {
            let rendered = render::handbook(&catalog)?;
            if check {
                let input =
                    File::open(&output).with_context(|| format!("open {}", output.display()))?;
                let limit = u64::try_from(rendered.len())?.saturating_add(1);
                let mut actual = Vec::new();
                input
                    .take(limit)
                    .read_to_end(&mut actual)
                    .with_context(|| format!("read {}", output.display()))?;
                ensure!(
                    actual == rendered.as_bytes(),
                    "{} is stale; run bladerf-docs render to regenerate it",
                    output.display()
                );
                println!("Generated reference is current: {}", output.display());
            } else {
                if let Ok(destination) = output.canonicalize() {
                    ensure!(
                        destination != cli.catalog.canonicalize()?
                            && destination != cli.schema.canonicalize()?,
                        "output must not replace catalog or schema input"
                    );
                }
                write_atomic(&output, rendered.as_bytes())?;
                println!("Generated {}", output.display());
            }
        }
    }
    Ok(())
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
