//! Decode an admitted channel-centered CF32 recording in a file-only process.

mod archive;
mod input;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use clap::{Args, Parser, Subcommand};
use futuresdr::prelude::{Flowgraph, Runtime};
use futuresdr::runtime::{config, scheduler::SmolScheduler};
use lora::utils::{Bandwidth, Channel, HeaderMode, LdroMode, SpreadingFactor, SynchWord};
use serde::Serialize;

use crate::archive::{Archive, Summary};
use crate::input::{Cf32LeSource, CompletedInput, Input};

const OVERSAMPLING: u32 = 4;
const MAX_FRAMES: usize = 4096;
const RUNTIME_THREADS: usize = 2;
const RUNTIME_BUFFER_BYTES: u64 = 512 * 1024;
const RUNTIME_QUEUE_CAPACITY: u64 = 64;
const RUNTIME_STACK_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Replay a channel-centered little-endian CF32 recording.
    Decode(Decode),
}

#[derive(Args)]
struct Decode {
    input: PathBuf,
    /// RF center of the channel in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// LoRa bandwidth in hertz.
    #[arg(long)]
    bandwidth_hz: u32,
    /// LoRa spreading factor (7 through 12).
    #[arg(long, value_parser = clap::value_parser!(u8).range(7..=12))]
    spreading_factor: u8,
    /// Compact LoRa sync word as a decimal byte.
    #[arg(long)]
    sync_word: u8,
    /// Maximum valid frame records to retain (1 through 4096).
    #[arg(long)]
    max_frames: usize,
    /// New JSON receipt file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    source_format: &'static str,
    source_sample_count: u64,
    source_sha256: &'a str,
    channel_hz: u64,
    bandwidth_hz: u32,
    sample_rate_hz: u32,
    spreading_factor: u8,
    sync_word: u8,
    expected_sync_symbols: [u16; 2],
    max_frames: usize,
    #[serde(flatten)]
    summary: Summary,
}

fn publish(output: &Path, report: &Report<'_>, input: &CompletedInput<'_>) -> Result<()> {
    let parent = output
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("create private LoRa receipt staging file")?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, report)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    input.verify()?;
    temporary
        .persist_noclobber(output)
        .context("publish LoRa receipt without replacing output")?;
    File::open(parent)?
        .sync_all()
        .context("LoRa receipt was published, but parent sync failed")?;
    Ok(())
}

fn decode(args: Decode) -> Result<()> {
    ensure!(
        (1..=MAX_FRAMES).contains(&args.max_frames),
        "max frames must be 1..={MAX_FRAMES}"
    );
    let channel =
        u32::try_from(args.channel_hz).context("channel frequency exceeds worker range")?;
    ensure!(channel > 0, "channel frequency must be positive");
    let bandwidth = Bandwidth::try_from(args.bandwidth_hz)
        .map_err(|_| anyhow!("unsupported LoRa bandwidth"))?;
    let spreading_factor = SpreadingFactor::try_from(args.spreading_factor)
        .map_err(|_| anyhow!("unsupported LoRa spreading factor"))?;
    let sync = SynchWord::from(args.sync_word);
    let sequence = sync.verify_and_expand(spreading_factor)?;
    let expected_sync_symbols = [u16::try_from(sequence[0])?, u16::try_from(sequence[1])?];

    let input = Input::open(&args.input)?;
    let source_sample_count = input.sample_count;
    config::set("buffer_size", RUNTIME_BUFFER_BYTES);
    config::set("queue_size", RUNTIME_QUEUE_CAPACITY);
    config::set("stack_size", RUNTIME_STACK_BYTES);
    config::set("ctrlport_enable", false);
    config::set("log_level", "error");
    let mut graph = Flowgraph::new();
    let source = graph.add(Cf32LeSource::new(input))?;
    let (synchronizer, decoder) = lora::build_lora_rx_soft_decoding(
        &mut graph,
        Channel::Custom(channel),
        bandwidth,
        spreading_factor,
        HeaderMode::Explicit,
        LdroMode::AUTO,
        Some(&[sync]),
        OVERSAMPLING as usize,
        None,
        Some("none"),
        false,
        None,
    )?;
    let archive = graph.add(Archive::new(
        expected_sync_symbols,
        source_sample_count,
        args.max_frames,
    ))?;
    graph.stream_dyn(source, "output", synchronizer, "input")?;
    graph.message(decoder, "out_annotated", archive, "in")?;
    let runtime = Runtime::with_scheduler(SmolScheduler::with_config(RUNTIME_THREADS, false));
    let terminated = runtime.run(graph).context("run LoRa receive flowgraph")?;
    let summary = terminated.with(&archive, Archive::summary)??;
    let source = terminated.block(&source)?;
    let input = source.completed()?;
    let report = Report {
        schema_version: 1,
        operation: "decode_lora_cf32",
        source_format: "CF32_LE",
        source_sample_count: input.sample_count,
        source_sha256: input.sha256,
        channel_hz: args.channel_hz,
        bandwidth_hz: args.bandwidth_hz,
        sample_rate_hz: args.bandwidth_hz * OVERSAMPLING,
        spreading_factor: args.spreading_factor,
        sync_word: args.sync_word,
        expected_sync_symbols,
        max_frames: args.max_frames,
        summary,
    };
    publish(&args.output, &report, &input)?;
    println!("Validated {} LoRa frames.", report.summary.validated_frames);
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().action {
        Action::Decode(args) => decode(args),
    }
}
