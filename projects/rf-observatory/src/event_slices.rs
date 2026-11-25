//! Export full-rate IQ slices from a provenance-bound dense event report.

mod input;
mod output;
mod plan;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use clap::Args;

use plan::Limits;

const MAX_SLICE_FILES: i64 = 4_096;
const MAX_CONTEXT_SAMPLES: u64 = crate::SAMPLE_RATE as u64;

#[derive(Debug, Args)]
pub(crate) struct SliceEvents {
    /// Saved full-band SC16 Q11 IQ with its adjacent acquisition sidecar.
    input: PathBuf,
    /// Dense spectrum event report for this IQ. Schema 3 selects active windows.
    #[arg(long)]
    events: PathBuf,
    /// New directory for the IQ slices, sidecars and terminal manifest.
    #[arg(long)]
    output: PathBuf,
    /// Saved samples of context to add before each selected range.
    #[arg(long, default_value_t = 8192, value_parser = clap::value_parser!(u64).range(..=MAX_CONTEXT_SAMPLES))]
    pre_roll_samples: u64,
    /// Saved samples of context to add after each selected range.
    #[arg(long, default_value_t = 8192, value_parser = clap::value_parser!(u64).range(..=MAX_CONTEXT_SAMPLES))]
    post_roll_samples: u64,
    /// Include gaps up to this many saved samples between padded, aligned spans.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(..=MAX_CONTEXT_SAMPLES))]
    merge_gap_samples: u64,
    /// Maximum number of exported slices.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..=MAX_SLICE_FILES))]
    max_slices: u32,
    /// Bridge the shortest remaining gaps as needed to meet the slice limit.
    #[arg(long)]
    coalesce_to_limit: bool,
    /// Refuse a plan with more than this many total exported complex samples.
    #[arg(long, default_value_t = 20_000_000, value_parser = clap::value_parser!(u64).range(1..=crate::MAX_SAMPLES))]
    max_total_samples: u64,
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "IQ slice export interrupted");
    Ok(())
}

pub(crate) fn export(root: &Path, args: &SliceEvents, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let limits = Limits {
        pre_roll_samples: args.pre_roll_samples,
        post_roll_samples: args.post_roll_samples,
        merge_gap_samples: args.merge_gap_samples,
        max_slices: args.max_slices as usize,
        coalesce_to_limit: args.coalesce_to_limit,
        max_total_samples: args.max_total_samples,
    };
    let source = input::open(&args.input, &args.events)?;
    let spans = plan::plan(&source.replay_spans, source.analyzed_samples, limits)?;
    let destination = root.join(&args.output);
    output::export(source, &spans, limits, &destination, stop)?;
    println!(
        "Exported {} IQ slices to {}",
        spans.len(),
        destination.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
