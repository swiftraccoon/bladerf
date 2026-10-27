//! Sequential receive surveys, retained-session monitoring and saved-PCAP inventory.
//!
//! Runs own fresh private directories. A synced append-only journal records
//! attempts; inventory snapshots are individually replaced atomically. Snapshot
//! files are not a multi-file transaction. Retention deletes only this run's
//! completed, ingested sessions, after publishing their aggregate evidence.
//! Byte thresholds are checked between bounded captures, not filesystem quotas.
//! Wall-clock accounting and untimestamped recorded sample time do not establish
//! exact RF coverage, packet loss, channel occupancy or physical device identity.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use chrono::Utc;
use clap::{Args, ValueEnum};
use serde::Serialize;
use serde_json::json;

use super::inventory::{Inventory, Limits, SourceContext, TimestampBasis};
use super::{Band, Capture, Engine, Receive};

#[derive(Debug, Args)]
pub(crate) struct Survey {
    #[command(flatten)]
    radio: Receive,
    /// Ordered channel list; defaults to 1,6,11 (2g) or 36,40,44,48 (5g).
    #[arg(long, value_delimiter = ',')]
    channels: Vec<u16>,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Stop threshold for retained run bytes, checked between captures.
    #[arg(long, default_value_t = 512)]
    retained_limit_mib: u32,
}

#[derive(Debug, Args)]
pub(crate) struct Watch {
    #[command(flatten)]
    radio: Receive,
    #[arg(long)]
    channel: Option<u16>,
    /// Finite capture count; use a fresh output directory for another run.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..=1000))]
    cycles: u16,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=100))]
    keep_sessions: u16,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Retained-byte threshold, checked between captures; not a disk quota.
    #[arg(long, default_value_t = 512)]
    retained_limit_mib: u32,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Clock {
    Unknown,
    SampleRelative,
    HostBeforeRxEnablePlusSampleOffset,
    HostUdpReceive,
}

impl From<Clock> for TimestampBasis {
    fn from(value: Clock) -> Self {
        match value {
            Clock::Unknown => Self::Unknown,
            Clock::SampleRelative => Self::SampleRelative,
            Clock::HostBeforeRxEnablePlusSampleOffset => Self::HostBeforeRxEnablePlusSampleOffset,
            Clock::HostUdpReceive => Self::HostUdpReceive,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct InventoryArgs {
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Known tuning of all inputs; advertised channels remain separate.
    #[arg(long)]
    frequency_mhz: Option<u32>,
    /// Explicit clock provenance for all inputs. Unknown is conservative.
    #[arg(long, value_enum, default_value = "unknown")]
    clock: Clock,
    #[arg(long, default_value_t = 1_000_000)]
    max_records: u64,
    #[arg(long, default_value_t = 256)]
    max_input_mib: u32,
}

#[derive(Debug)]
struct Plan {
    mode: &'static str,
    radio: Receive,
    channels: Vec<u16>,
    cycles: u16,
    keep_sessions: Option<usize>,
    retained_limit: u64,
    output: Option<PathBuf>,
}

fn byte_limit(mib: u32) -> Result<u64> {
    ensure!(
        (1..=16384).contains(&mib),
        "retained limit must be 1..=16384 MiB"
    );
    Ok(u64::from(mib) * 1024 * 1024)
}

impl Plan {
    fn validate(&self) -> Result<()> {
        self.radio.validate()?;
        ensure!(
            !self.channels.is_empty() && self.channels.len() <= 64,
            "select 1..=64 channels"
        );
        let unique: BTreeSet<_> = self.channels.iter().copied().collect();
        ensure!(
            unique.len() == self.channels.len(),
            "duplicate survey channels are not allowed"
        );
        for channel in &self.channels {
            super::frequency(self.radio.band, Some(*channel))?;
        }
        ensure!((1..=1000).contains(&self.cycles), "cycles must be 1..=1000");
        ensure!(
            self.retained_limit > 0 && self.keep_sessions.is_none_or(|keep| keep > 0),
            "retention bounds must be positive"
        );
        if matches!(self.radio.engine, Engine::Crafter) {
            ensure!(
                cfg!(feature = "decode"),
                "IQ survey/watch requires the decode feature"
            );
            ensure!(
                self.estimated_iq_bytes() <= self.retained_limit,
                "one requested IQ recording exceeds the retained-byte threshold"
            );
        }
        Ok(())
    }

    fn estimated_iq_bytes(&self) -> u64 {
        match self.radio.engine {
            Engine::Crafter => (self.radio.duration() * 20_000_000.0).round() as u64 * 4,
            Engine::Futuresdr => 0,
        }
    }
}

pub(crate) fn survey(
    root: &Path,
    receiver: Option<&Path>,
    args: Survey,
    stop: &AtomicBool,
) -> Result<()> {
    let channels = if args.channels.is_empty() {
        match args.radio.band {
            Band::TwoGhz => vec![1, 6, 11],
            Band::FiveGhz => vec![36, 40, 44, 48],
        }
    } else {
        args.channels
    };
    execute(
        root,
        receiver,
        Plan {
            mode: "survey",
            radio: args.radio,
            channels,
            cycles: 1,
            keep_sessions: None,
            retained_limit: byte_limit(args.retained_limit_mib)?,
            output: args.output,
        },
        stop,
    )
}

pub(crate) fn watch(
    root: &Path,
    receiver: Option<&Path>,
    args: Watch,
    stop: &AtomicBool,
) -> Result<()> {
    let (channel, _) = super::frequency(args.radio.band, args.channel)?;
    execute(
        root,
        receiver,
        Plan {
            mode: "watch",
            radio: args.radio,
            channels: vec![channel],
            cycles: args.cycles,
            keep_sessions: Some(usize::from(args.keep_sessions)),
            retained_limit: byte_limit(args.retained_limit_mib)?,
            output: args.output,
        },
        stop,
    )
}

fn execute(root: &Path, receiver: Option<&Path>, plan: Plan, stop: &AtomicBool) -> Result<()> {
    plan.validate()?;
    let program = super::receiver(root, receiver, plan.radio.engine)?;
    let directory = super::output(root, plan.output.as_deref(), "captures/wifi-monitor")?;
    run_plan(&directory, &plan, stop, |capture, session, flag| {
        super::capture_into(root, &program, capture, session, flag)
    })
    .with_context(|| {
        format!(
            "monitoring run failed; available evidence in {}",
            directory.display()
        )
    })
}

/// Write a new snapshot then atomically replace this run's own snapshot name.
/// Pre-rename failure preserves the previous snapshot and may leave staging.
/// A directory-sync failure after rename leaves a complete replacement whose
/// crash durability is uncertain.
fn snapshot(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let stage = directory.join(format!(".{name}.next"));
    let mut file = super::new_file(&stage)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&stage, directory.join(name))?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn save_inventory(directory: &Path, inventory: &Inventory) -> Result<()> {
    snapshot(
        directory,
        "inventory.json",
        &serde_json::to_vec_pretty(inventory.report())?,
    )?;
    snapshot(
        directory,
        "inventory.md",
        inventory.report().markdown().as_bytes(),
    )
}

fn journal(file: &mut File, record: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *file, record)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Do not follow links while accounting or pruning managed evidence.
fn tree_bytes(path: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "unexpected symlink in managed run"
    );
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    ensure!(metadata.is_dir(), "unexpected non-file in managed run");
    let mut total = 0u64;
    for child in fs::read_dir(path)? {
        total = total
            .checked_add(tree_bytes(&child?.path())?)
            .context("run byte count overflow")?;
    }
    Ok(total)
}

#[derive(Debug, Serialize)]
struct Attempt {
    index: usize,
    channel: u16,
    tuned_frequency_hz: u64,
    session: String,
    started_utc: String,
    ended_utc: String,
    start_offset_seconds: f64,
    end_offset_seconds: f64,
    recorded_sample_seconds: Option<f64>,
    host_before_rx_enable_ns: Option<u128>,
    capture_succeeded: bool,
    inventory_ingested: bool,
    interrupted: bool,
    retained: bool,
    capture_error: Option<String>,
    inventory_error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct ChannelAccounting {
    attempted_visits: usize,
    successful_visits: usize,
    recorded_sample_seconds: Option<f64>,
    known_recorded_sample_seconds: f64,
    attempts_without_sample_time: usize,
    wall_seconds_without_recorded_samples_estimate: Option<f64>,
    between_visit_wall_gaps_seconds: Vec<f64>,
}

fn channel_accounting(attempts: &[Attempt], elapsed: f64) -> BTreeMap<u16, ChannelAccounting> {
    let mut accounting = BTreeMap::<u16, ChannelAccounting>::new();
    let mut previous = BTreeMap::<u16, f64>::new();
    for attempt in attempts {
        let entry = accounting.entry(attempt.channel).or_default();
        entry.attempted_visits += 1;
        entry.successful_visits += usize::from(
            attempt.capture_succeeded && attempt.inventory_ingested && !attempt.interrupted,
        );
        match attempt.recorded_sample_seconds {
            Some(seconds) => entry.known_recorded_sample_seconds += seconds,
            None => entry.attempts_without_sample_time += 1,
        }
        if let Some(end) = previous.insert(attempt.channel, attempt.end_offset_seconds) {
            entry
                .between_visit_wall_gaps_seconds
                .push((attempt.start_offset_seconds - end).max(0.0));
        }
    }
    for entry in accounting.values_mut() {
        entry.recorded_sample_seconds = (entry.attempts_without_sample_time == 0)
            .then_some(entry.known_recorded_sample_seconds);
        entry.wall_seconds_without_recorded_samples_estimate = entry
            .recorded_sample_seconds
            .map(|seconds| (elapsed - seconds).max(0.0));
    }
    accounting
}

struct RunState {
    attempts: Vec<Attempt>,
    retained: VecDeque<(usize, PathBuf)>,
    inventory: Inventory,
}

fn save_run(
    directory: &Path,
    plan: &Plan,
    state: &RunState,
    elapsed: f64,
    finished: bool,
    interrupted: bool,
    stop_reason: Option<&str>,
) -> Result<()> {
    let channels = channel_accounting(&state.attempts, elapsed);
    let sample_seconds: f64 = state
        .attempts
        .iter()
        .filter_map(|a| a.recorded_sample_seconds)
        .fold(0.0, |sum, seconds| sum + seconds);
    let unknown_sample_times = state
        .attempts
        .iter()
        .filter(|a| a.recorded_sample_seconds.is_none())
        .count();
    let complete_sample_seconds =
        (!state.attempts.is_empty() && unknown_sample_times == 0).then_some(sample_seconds);
    snapshot(
        directory,
        "run.json",
        &serde_json::to_vec_pretty(&json!({
            "mode":plan.mode,"engine":plan.radio.engine,"channels_requested":plan.channels,
            "cycles_requested":plan.cycles,"attempts_requested":plan.channels.len()*usize::from(plan.cycles),
            "finished":finished,"plan_completed":finished && !interrupted && stop_reason.is_none()
                && state.attempts.len()==plan.channels.len()*usize::from(plan.cycles),
            "successful":finished && !interrupted && stop_reason.is_none()
                && state.attempts.iter().all(|a|a.capture_succeeded && a.inventory_ingested),
            "interrupted":interrupted,"stop_reason":stop_reason,
            "elapsed_wall_seconds":elapsed,"recorded_sample_seconds":complete_sample_seconds,
            "known_recorded_sample_seconds":sample_seconds,"attempts_without_sample_time":unknown_sample_times,
            "unaccounted_wall_seconds_estimate":complete_sample_seconds.map(|seconds|(elapsed-seconds).max(0.0)),
            "timing_note":"Sample counts and host timing do not establish exact RF windows or continuity; between-visit gaps exclude blind periods within attempts. Live active sample time is unknown.",
            "continuity_guaranteed":false,"retained_limit_bytes":plan.retained_limit,
            "retention_note":"Threshold checked between captures, not a filesystem quota. Failed/interrupted evidence is retained; aggregate observations survive successful-session pruning.",
            "channel_accounting":channels,"attempts":state.attempts
        }))?,
    )
}

fn prune(directory: &Path, state: &mut RunState, log: &mut File) -> Result<bool> {
    let Some((index, path)) = state.retained.pop_front() else {
        return Ok(false);
    };
    ensure!(
        path.parent() == Some(directory.join("sessions").as_path()),
        "session is outside managed directory"
    );
    tree_bytes(&path)?;
    fs::remove_dir_all(&path)?;
    if let Some(attempt) = state.attempts.iter_mut().find(|a| a.index == index) {
        attempt.retained = false;
    }
    journal(log, &json!({"event":"session_pruned","index":index}))?;
    Ok(true)
}

/// Execute a finite plan. Capture errors preserve evidence and allow a survey to
/// visit remaining channels. Watch stops on its first failed or interrupted
/// attempt. Successful aggregate publication precedes any retention deletion.
fn run_plan(
    directory: &Path,
    plan: &Plan,
    stop: &AtomicBool,
    mut capture: impl FnMut(&Capture, &Path, &AtomicBool) -> Result<()>,
) -> Result<()> {
    let start = Instant::now();
    let mut log = super::new_file(&directory.join("attempts.jsonl"))?;
    let sessions = super::output(directory, Some(Path::new("sessions")), "unused")?;
    let mut state = RunState {
        attempts: Vec::new(),
        retained: VecDeque::new(),
        inventory: Inventory::new(Limits::default())?,
    };
    save_inventory(directory, &state.inventory)?;
    save_run(directory, plan, &state, 0.0, false, false, None)?;
    let mut stop_reason = None;
    'cycles: for _ in 0..plan.cycles {
        for channel in &plan.channels {
            if stop.load(Ordering::Relaxed) {
                stop_reason = Some("interrupted");
                break 'cycles;
            }
            if let Some(keep) = plan.keep_sessions {
                while state.retained.len() >= keep
                    || tree_bytes(directory)?.saturating_add(plan.estimated_iq_bytes())
                        > plan.retained_limit
                {
                    if !prune(directory, &mut state, &mut log)? {
                        break;
                    }
                }
            }
            if tree_bytes(directory)?.saturating_add(plan.estimated_iq_bytes())
                > plan.retained_limit
            {
                stop_reason = Some("retained_byte_threshold_before_capture");
                break 'cycles;
            }
            let index = state.attempts.len() + 1;
            let name = format!("{index:06}-ch{channel:03}");
            let session = super::output(&sessions, Some(Path::new(&name)), "unused")?;
            let capture_args = Capture {
                radio: plan.radio.clone(),
                channel: Some(*channel),
                output: None,
            };
            let (_, frequency_hz) = capture_args.validate()?;
            let started_utc = Utc::now().to_rfc3339();
            let offset = start.elapsed().as_secs_f64();
            journal(
                &mut log,
                &json!({"event":"attempt_started","index":index,"channel":channel,
                "tuned_frequency_hz":frequency_hz,"session":format!("sessions/{name}"),
                "started_utc":started_utc,"start_offset_seconds":offset}),
            )?;
            let result = capture(&capture_args, &session, stop);
            let interrupted = stop.load(Ordering::Relaxed);
            let sample_metadata = if matches!(plan.radio.engine, Engine::Crafter) {
                super::acquisition(&session.join("iq.sc16q11"), &capture_args, frequency_hz).ok()
            } else {
                None
            };
            let mut attempt = Attempt {
                index,
                channel: *channel,
                tuned_frequency_hz: frequency_hz,
                session: format!("sessions/{name}"),
                started_utc,
                ended_utc: Utc::now().to_rfc3339(),
                start_offset_seconds: offset,
                end_offset_seconds: start.elapsed().as_secs_f64(),
                recorded_sample_seconds: sample_metadata.map(|(n, _)| n as f64 / 20_000_000.0),
                host_before_rx_enable_ns: sample_metadata.map(|(_, t)| t),
                capture_succeeded: result.is_ok(),
                inventory_ingested: false,
                interrupted,
                retained: true,
                capture_error: result.err().map(|e| format!("{e:#}")),
                inventory_error: None,
            };
            let pcap = session.join("candidates.pcap");
            if pcap.is_file() {
                let ingest = state.inventory.add_pcap(
                    &pcap,
                    SourceContext {
                        label: name.clone(),
                        tuned_frequency_hz: Some(frequency_hz),
                        timestamp_basis: match plan.radio.engine {
                            Engine::Crafter => TimestampBasis::HostBeforeRxEnablePlusSampleOffset,
                            Engine::Futuresdr => TimestampBasis::HostUdpReceive,
                        },
                    },
                );
                attempt.inventory_ingested = ingest.is_ok();
                attempt.inventory_error = ingest.err().map(|e| format!("{e:#}"));
            } else {
                attempt.inventory_error = Some("no completed candidate PCAP".into());
            }
            let succeeded = attempt.capture_succeeded && attempt.inventory_ingested && !interrupted;
            save_inventory(directory, &state.inventory)?;
            journal(
                &mut log,
                &json!({"event":"attempt_finished","attempt":attempt}),
            )?;
            state.attempts.push(attempt);
            if succeeded {
                state.retained.push_back((index, session));
            }
            if plan.keep_sessions.is_some() {
                while tree_bytes(directory)? > plan.retained_limit && state.retained.len() > 1 {
                    prune(directory, &mut state, &mut log)?;
                }
            }
            save_run(
                directory,
                plan,
                &state,
                start.elapsed().as_secs_f64(),
                false,
                interrupted,
                None,
            )?;
            println!(
                "Channel {channel}: {} (attempt {index})",
                if succeeded {
                    "captured and inventoried"
                } else {
                    "incomplete; evidence retained"
                }
            );
            if tree_bytes(directory)? > plan.retained_limit {
                stop_reason = Some("retained_byte_threshold_after_capture");
                break 'cycles;
            }
            if interrupted {
                stop_reason = Some("interrupted");
                break 'cycles;
            }
            if !succeeded && plan.keep_sessions.is_some() {
                stop_reason = Some("failed_watch_attempt");
                break 'cycles;
            }
        }
    }
    let interrupted = stop.load(Ordering::Relaxed);
    save_run(
        directory,
        plan,
        &state,
        start.elapsed().as_secs_f64(),
        true,
        interrupted,
        stop_reason,
    )?;
    println!(
        "Inventory: {} networks, {} observed addresses",
        state.inventory.report().networks.len(),
        state.inventory.report().observed_addresses.len()
    );
    println!("Output directory: {}", directory.display());
    ensure!(
        stop_reason.is_none()
            && !interrupted
            && state
                .attempts
                .iter()
                .all(|a| a.capture_succeeded && a.inventory_ingested),
        "monitoring plan incomplete or had failed attempts; see {}",
        directory.join("run.json").display()
    );
    Ok(())
}

pub(crate) fn inventory(root: &Path, args: InventoryArgs, stop: &AtomicBool) -> Result<()> {
    if let Some(frequency) = args.frequency_mhz {
        ensure!(
            (70..=6000).contains(&frequency),
            "frequency must be 70..=6000 MHz"
        );
    }
    ensure!(
        (1..=4096).contains(&args.max_input_mib),
        "max input must be 1..=4096 MiB"
    );
    ensure!(
        (1..=10_000_000).contains(&args.max_records),
        "max records must be 1..=10000000"
    );
    let limits = Limits {
        max_records_per_input: args.max_records,
        max_input_bytes: u64::from(args.max_input_mib) * 1024 * 1024,
        ..Limits::default()
    };
    let mut inventory = Inventory::new(limits)?;
    ensure!(
        args.inputs.len() <= 1024,
        "at most 1024 PCAP inputs per inventory"
    );
    let inputs: Vec<_> = args
        .inputs
        .iter()
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<_>>()?;
    ensure!(
        inputs.iter().collect::<BTreeSet<_>>().len() == inputs.len(),
        "duplicate PCAP inputs are not allowed"
    );
    let directory = super::output(root, args.output.as_deref(), "artifacts/wifi-inventory")?;
    save_inventory(&directory, &inventory)?;
    for (index, input) in inputs.iter().enumerate() {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "inventory interrupted; completed inputs preserved"
        );
        inventory
            .add_pcap(
                input,
                SourceContext {
                    label: format!("input-{:06}", index + 1),
                    tuned_frequency_hz: args.frequency_mhz.map(|n| u64::from(n) * 1_000_000),
                    timestamp_basis: args.clock.into(),
                },
            )
            .with_context(|| {
                format!(
                    "inventory input failed; completed inputs preserved in {}",
                    directory.display()
                )
            })?;
        save_inventory(&directory, &inventory)?;
    }
    ensure!(
        !stop.load(Ordering::Relaxed),
        "inventory interrupted; completed inputs preserved in {}",
        directory.display()
    );
    println!(
        "Inventory: {} networks, {} observed addresses",
        inventory.report().networks.len(),
        inventory.report().observed_addresses.len()
    );
    println!("Output directory: {}", directory.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, Cli, PcapWriter};
    use clap::Parser;

    fn plan(mode: &'static str, channels: Vec<u16>, cycles: u16, keep: Option<usize>) -> Plan {
        let Action::Capture(args) = Cli::try_parse_from([
            "wifi-monitor",
            "capture",
            "--engine",
            "futuresdr",
            "--duration",
            "0.1",
        ])
        .unwrap()
        .command
        else {
            panic!("capture")
        };
        Plan {
            mode,
            radio: args.radio,
            channels,
            cycles,
            keep_sessions: keep,
            retained_limit: 1024 * 1024,
            output: None,
        }
    }

    fn empty_capture(_args: &Capture, session: &Path, _stop: &AtomicBool) -> Result<()> {
        PcapWriter::new(&session.join("candidates.pcap"))?.finish()?;
        fs::write(session.join("source-evidence"), [0u8; 1024])?;
        Ok(())
    }

    fn read_run(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path.join("run.json")).unwrap()).unwrap()
    }

    #[test]
    fn retention_keeps_aggregates_after_pruning_only_owned_successes() {
        let directory = tempfile::tempdir().unwrap();
        let plan = plan("watch", vec![6], 3, Some(1));
        run_plan(
            directory.path(),
            &plan,
            &AtomicBool::new(false),
            empty_capture,
        )
        .unwrap();
        let run = read_run(directory.path());
        assert_eq!(run["successful"], true);
        assert_eq!(run["plan_completed"], true);
        assert_eq!(run["attempts"][0]["retained"], false);
        assert_eq!(run["attempts"][2]["retained"], true);
        assert_eq!(
            fs::read_dir(directory.path().join("sessions"))
                .unwrap()
                .count(),
            1
        );
        let inventory: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("inventory.json")).unwrap())
                .unwrap();
        assert_eq!(inventory["sources"].as_array().unwrap().len(), 3);
        assert_eq!(
            run["channel_accounting"]["6"]["attempts_without_sample_time"],
            3
        );
        assert_eq!(
            run["channel_accounting"]["6"]["between_visit_wall_gaps_seconds"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn survey_journals_failure_continues_and_returns_failure() {
        let directory = tempfile::tempdir().unwrap();
        let plan = plan("survey", vec![1, 6, 11], 1, None);
        let result = run_plan(
            directory.path(),
            &plan,
            &AtomicBool::new(false),
            |args, path, stop| {
                if args.channel == Some(6) {
                    anyhow::bail!("synthetic receive failure");
                }
                empty_capture(args, path, stop)
            },
        );
        assert!(result.is_err());
        let run = read_run(directory.path());
        assert_eq!(run["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(run["successful"], false);
        assert_eq!(run["attempts"][1]["capture_succeeded"], false);
        assert!(directory.path().join("sessions/000002-ch006").is_dir());
        let journal = fs::read_to_string(directory.path().join("attempts.jsonl")).unwrap();
        assert_eq!(journal.lines().count(), 6);
    }

    #[test]
    fn cancellation_keeps_partial_report_and_stops_next_capture() {
        let directory = tempfile::tempdir().unwrap();
        let stop = AtomicBool::new(false);
        assert!(
            run_plan(
                directory.path(),
                &plan("watch", vec![6], 3, Some(1)),
                &stop,
                |args, path, flag| {
                    empty_capture(args, path, flag)?;
                    flag.store(true, Ordering::Relaxed);
                    Ok(())
                }
            )
            .is_err()
        );
        let run = read_run(directory.path());
        assert_eq!(run["interrupted"], true);
        assert_eq!(run["plan_completed"], false);
        assert_eq!(run["attempts"].as_array().unwrap().len(), 1);
        assert!(
            directory
                .path()
                .join("sessions/000001-ch006/candidates.pcap")
                .is_file()
        );
    }

    #[test]
    fn oversized_session_is_retained_and_prevents_further_capture() {
        let directory = tempfile::tempdir().unwrap();
        assert!(
            run_plan(
                directory.path(),
                &plan("watch", vec![6], 3, Some(1)),
                &AtomicBool::new(false),
                |args, path, flag| {
                    empty_capture(args, path, flag)?;
                    File::create(path.join("oversized-evidence"))?.set_len(2 * 1024 * 1024)?;
                    Ok(())
                }
            )
            .is_err()
        );
        let run = read_run(directory.path());
        assert_eq!(run["stop_reason"], "retained_byte_threshold_after_capture");
        assert_eq!(run["attempts"].as_array().unwrap().len(), 1);
        assert!(
            directory
                .path()
                .join("sessions/000001-ch006/oversized-evidence")
                .is_file()
        );
    }

    #[test]
    fn pruning_rejects_symlinks_and_preserves_external_files() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), "preserved").unwrap();
        fs::create_dir(directory.path().join("sessions")).unwrap();
        let path = directory.path().join("sessions/000001-ch006");
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        let mut state = RunState {
            attempts: vec![],
            retained: VecDeque::from([(1, path)]),
            inventory: Inventory::new(Limits::default()).unwrap(),
        };
        let mut log = super::super::new_file(&directory.path().join("journal")).unwrap();
        assert!(prune(directory.path(), &mut state, &mut log).is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("keep")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn duplicate_or_invalid_channels_fail_preflight() {
        assert!(plan("survey", vec![1, 1], 1, None).validate().is_err());
        assert!(plan("survey", vec![1, 36], 1, None).validate().is_err());
    }
    #[test]
    fn duplicate_inventory_inputs_fail_before_creating_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.pcap");
        PcapWriter::new(&input).unwrap().finish().unwrap();
        let alias = directory.path().join("alias.pcap");
        std::os::unix::fs::symlink(&input, &alias).unwrap();
        let args = InventoryArgs {
            inputs: vec![input, alias],
            output: Some(PathBuf::from("output")),
            frequency_mhz: None,
            clock: Clock::Unknown,
            max_records: 1_000_000,
            max_input_mib: 256,
        };
        assert!(inventory(directory.path(), args, &AtomicBool::new(false)).is_err());
        assert!(!directory.path().join("output").exists());
    }
}
