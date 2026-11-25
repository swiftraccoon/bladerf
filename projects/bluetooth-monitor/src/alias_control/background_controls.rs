//! Fixed CF32 windows selected without burst triggers or signal metrics.

mod contract;
mod lattice;
mod report;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::Args;

use self::contract::{FrequencyManifest, OUTPUT_BYTES, validate_manifest};
use self::lattice::{WINDOW_COUNT, analyze_lattice, lattice_settings};
use self::report::{ReportInputs, build as build_report};
use super::provenance::{ContentIdentity, read_cf32, read_stable_json};
use super::publish_report;

const DEFAULT_MAX_CF32_INPUT_MIB: u32 = 64;
const DEFAULT_MAX_OUTPUT_MIB: u32 = 16;

#[derive(Debug, Args)]
pub(crate) struct BackgroundControls {
    /// Existing 4 MSps CF32 stream from `channelize-frequency`.
    cf32_input: PathBuf,
    /// Matching `channelize-frequency.json` provenance manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Expected 1 MHz target center; must match the manifest.
    #[arg(long, value_parser = clap::value_parser!(u32).range(2402..=2480))]
    frequency_mhz: u32,
    /// New output directory; defaults under artifacts/bluetooth-background-controls/.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Maximum accepted CF32 size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_CF32_INPUT_MIB)]
    max_cf32_input_mib: u32,
    /// Maximum JSON output size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_MIB)]
    max_output_mib: u32,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    cf32_input_bytes: u64,
    output_bytes: u64,
}

impl BackgroundControls {
    fn limits(&self) -> Result<Limits> {
        ensure!(
            (32..=64).contains(&self.max_cf32_input_mib),
            "CF32 limit must be 32..=64 MiB"
        );
        ensure!(
            (1..=256).contains(&self.max_output_mib),
            "output limit must be 1..=256 MiB"
        );
        Ok(Limits {
            cf32_input_bytes: u64::from(self.max_cf32_input_mib) * 1024 * 1024,
            output_bytes: u64::from(self.max_output_mib) * 1024 * 1024,
        })
    }
}

pub(super) fn run(root: &Path, args: &BackgroundControls, interrupted: &AtomicBool) -> Result<()> {
    let limits = args.limits()?;
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "background controls interrupted before manifest access"
    );
    let (manifest, manifest_identity) = read_stable_json::<FrequencyManifest>(
        &args.manifest,
        "frequency channelizer manifest",
        "frequency_channelizer_manifest_v1",
    )?;
    validate_manifest(args, limits, &manifest)?;
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "background controls interrupted before CF32 access"
    );
    let cf32 = read_cf32(
        &args.cf32_input,
        limits.cf32_input_bytes,
        &manifest.target.sha256,
    )?;
    ensure!(
        cf32.len() as u64 == OUTPUT_BYTES,
        "CF32 length differs from the exact 4,000,000-sample contract"
    );
    let (matched_rate, accounting) = analyze_lattice(
        &cf32,
        manifest.filter.startup_transient_output_samples as u64,
        manifest.target.output_complex_samples,
        interrupted,
    )?;
    ensure!(
        accounting.analyzed_windows == WINDOW_COUNT,
        "authenticated background lattice was not fully analyzable"
    );
    let report = build_report(ReportInputs {
        args,
        manifest: &manifest,
        manifest_identity,
        cf32_identity: ContentIdentity {
            format: "CF32_LE",
            bytes: cf32.len() as u64,
            sha256: manifest.target.sha256.clone(),
        },
        lattice: lattice_settings()?,
        accounting,
        matched_rate,
    });
    let mut encoded = serde_json::to_vec_pretty(&report)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() as u64 <= limits.output_bytes,
        "background-controls report exceeds its output limit"
    );
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "background controls interrupted before output creation"
    );
    let output = root.join(crate::output_path(
        args.output.as_deref(),
        "artifacts/bluetooth-background-controls",
    ));
    crate::new_directory(&output)?;
    publish_report(&output.join("background-controls.json"), &encoded)?;
    crate::print_summary(&report, "Background-controls directory", &output)
}

#[cfg(test)]
mod tests;
