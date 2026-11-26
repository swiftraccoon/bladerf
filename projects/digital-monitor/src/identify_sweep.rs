//! Route completed sweep IQ to file-only decoders and retain verified evidence.

mod admission;
mod bluetooth;
mod ieee;
mod routes;
mod wifi;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::DirBuilder;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use self::routes::{Channel, ChannelScope, Protocol, Route, Visit, count_attempts, routes};
use crate::event_replay::replay::LaneEvidence;
use crate::input::SourceReport;
use crate::process::{self, ProgramProvenance};
use crate::report::{self, OverallState};

const DEFAULT_PROTOCOLS: [&str; 2] = ["wifi", "ieee802154"];

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory with a completed sweep or, with --series, a sweep series.
    input: PathBuf,
    /// Admit all passes of a completed sweep series.
    #[arg(long)]
    series: bool,
    /// Replay every eligible visit per channel.
    #[arg(long)]
    all_visits: bool,
    /// Print the validated replay plan without reading IQ or opening a decoder.
    #[arg(long)]
    plan_only: bool,
    /// Fresh directory for identification.json and decoder evidence.
    #[arg(
        long,
        required_unless_present = "plan_only",
        conflicts_with = "plan_only"
    )]
    output: Option<PathBuf>,
    /// Protocol to try. Repeat to select multiple protocols.
    #[arg(long = "protocol", value_enum, default_values = DEFAULT_PROTOCOLS)]
    protocols: Vec<Protocol>,
    /// Use the requested RF scope or all channels eligible in the saved tunes.
    #[arg(long, value_enum, default_value = "requested")]
    channel_scope: ChannelScope,
    /// Prebuilt Wi-Fi monitor with its offline decoder enabled.
    #[arg(long, default_value = "target/release/wifi-monitor")]
    wifi_monitor: PathBuf,
    /// Prebuilt IEEE 802.15.4 monitor.
    #[arg(long, default_value = "target/release/ieee802154-monitor")]
    ieee802154_monitor: PathBuf,
    /// Prebuilt standalone FutureSDR IEEE 802.15.4 worker.
    #[arg(
        long,
        default_value = "projects/ieee802154-monitor/futuresdr-worker/target/release/ieee802154-futuresdr-worker"
    )]
    ieee_worker: PathBuf,
    /// Prebuilt Bluetooth monitor with offline replay support.
    #[arg(long, default_value = "target/release/bluetooth-monitor")]
    bluetooth_monitor: PathBuf,
    /// Patched Blue Dragon executable for the Bluetooth LE lane.
    #[arg(long, default_value = "build/blue-dragon/release/blue-dragon")]
    blue_dragon: PathBuf,
    /// Workspace with the Bluetooth decoder source patches applied.
    #[arg(long)]
    bluetooth_workspace: Option<PathBuf>,
    /// Complete IQ sample ceiling per selected tune.
    #[arg(long, default_value_t = 20_000_000, value_parser = clap::value_parser!(u64).range(8_192..=200_000_000))]
    max_samples_per_tune: u64,
    /// Maximum complete IQ samples across distinct selected visits.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    max_total_samples: Option<u64>,
    /// Maximum number of decoder replays. Bluetooth LE reuses one per tune.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..))]
    max_attempts: u16,
}

#[derive(Debug, clap::Args)]
pub(crate) struct FollowUpArgs {
    /// Directory with a complete sweep.json to check against decoder geometry.
    input: PathBuf,
    /// Protocol to plan for. Repeat to select multiple protocols.
    #[arg(long = "protocol", value_enum, default_values = DEFAULT_PROTOCOLS)]
    protocols: Vec<Protocol>,
}

#[derive(Serialize)]
struct FollowUpPlan<'a> {
    schema_version: u32,
    operation: &'static str,
    source_sweep_sha256: &'a str,
    centers_hz: Vec<u64>,
    targets: Vec<Channel>,
}

/// Print exact centers for missing decoder routes without opening IQ or a decoder.
pub(crate) fn print_follow_up(args: FollowUpArgs, stop: &AtomicBool) -> Result<()> {
    let batch = admission::Batch::open(&args.input, false)?;
    let targets: Vec<_> = routes(&batch, &args.protocols, ChannelScope::Requested, false)
        .into_iter()
        .filter_map(|route| route.tune.is_none().then_some(route.channel))
        .collect();
    let mut centers_hz: Vec<_> = targets.iter().map(|target| target.frequency_hz).collect();
    centers_hz.sort_unstable();
    centers_hz.dedup();
    batch.verify()?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep follow-up planning interrupted"
    );
    let plan = FollowUpPlan {
        schema_version: 1,
        operation: "plan_sweep_follow_up",
        source_sweep_sha256: &batch.sha256,
        centers_hz,
        targets,
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &plan).context("write sweep follow-up plan")?;
    writeln!(stdout)?;
    stdout.flush().context("flush sweep follow-up plan")
}

struct Programs {
    wifi: Option<ProgramProvenance>,
    ieee: Option<ieee::Programs>,
    bluetooth: Option<bluetooth::Programs>,
}

fn bluetooth_workspace(explicit: Option<&Path>) -> Result<PathBuf> {
    let is_workspace =
        |path: &Path| path.join("Cargo.toml").is_file() && path.join("repos").is_dir();
    if let Some(path) = explicit {
        let directory = path
            .canonicalize()
            .context("resolve Bluetooth decoder workspace")?;
        ensure!(
            is_workspace(&directory),
            "Bluetooth decoder needs a prepared workspace directory"
        );
        return Ok(directory);
    }
    std::env::current_dir()?
        .ancestors()
        .find(|path| is_workspace(path))
        .map(Path::to_path_buf)
        .context("find the Bluetooth decoder workspace or pass --bluetooth-workspace")
}

impl Programs {
    fn resolve(routes: &[Route], args: &Args) -> Result<Self> {
        let has_lane = |protocol| {
            routes
                .iter()
                .any(|route| route.channel.protocol == protocol && route.tune.is_some())
        };
        let wifi = has_lane(Protocol::Wifi)
            .then(|| process::resolve_program("wifi-monitor", &args.wifi_monitor))
            .transpose()?;
        let ieee = has_lane(Protocol::Ieee802154)
            .then(|| -> Result<_> {
                Ok(ieee::Programs {
                    monitor: process::resolve_program(
                        "ieee802154-monitor",
                        &args.ieee802154_monitor,
                    )?,
                    worker: process::resolve_program(
                        "ieee802154-futuresdr-worker",
                        &args.ieee_worker,
                    )?,
                })
            })
            .transpose()?;
        let bluetooth = has_lane(Protocol::BluetoothLe)
            .then(|| -> Result<_> {
                Ok(bluetooth::Programs {
                    monitor: process::resolve_program(
                        "bluetooth-monitor",
                        &args.bluetooth_monitor,
                    )?,
                    decoder: process::resolve_program("blue-dragon", &args.blue_dragon)?,
                    workspace: bluetooth_workspace(args.bluetooth_workspace.as_deref())?,
                })
            })
            .transpose()?;
        Ok(Self {
            wifi,
            ieee,
            bluetooth,
        })
    }

    fn identities(&self) -> Vec<&ProgramProvenance> {
        self.wifi
            .iter()
            .chain(
                self.ieee
                    .iter()
                    .flat_map(|programs| [&programs.monitor, &programs.worker]),
            )
            .chain(
                self.bluetooth
                    .iter()
                    .flat_map(|programs| [&programs.monitor, &programs.decoder]),
            )
            .collect()
    }

    fn decode(
        &self,
        route: &Route,
        capture: &admission::Capture,
        directory: &Path,
        bluetooth_replays: &mut BTreeMap<Visit, Result<bluetooth::TuneEvidence>>,
        stop: &AtomicBool,
    ) -> Result<Evidence> {
        capture.verify()?;
        let channel = route.channel;
        let source = &capture.source;
        let evidence = match channel.protocol {
            Protocol::Wifi => Evidence::Wifi(Box::new(wifi::run(
                self.wifi.as_ref().context("missing Wi-Fi decoder")?,
                source,
                u32::try_from(channel.frequency_hz / 1_000_000)?,
                directory,
                stop,
            )?)),
            Protocol::Ieee802154 => Evidence::Ieee802154(Box::new(ieee::run(
                self.ieee
                    .as_ref()
                    .context("missing IEEE 802.15.4 decoder")?,
                source,
                u8::try_from(channel.channel_number)?,
                directory,
                stop,
            )?)),
            Protocol::BluetoothLe => {
                let programs = self
                    .bluetooth
                    .as_ref()
                    .context("missing Bluetooth decoder")?;
                let tune = route.tune.context("missing Bluetooth tune")?;
                let evidence = bluetooth_replays
                    .entry(tune)
                    .or_insert_with(|| bluetooth::run(programs, source, directory, stop))
                    .as_ref()
                    .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                Evidence::BluetoothLe {
                    decoder_center_hz: evidence.decoder_center_hz,
                    tune_evidence_sha256: evidence.receipt_sha256.clone(),
                    validated_frames: evidence
                        .validated_frames(u16::try_from(channel.frequency_hz / 1_000_000)?),
                }
            }
        };
        capture.verify()?;
        Ok(evidence)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "protocol", content = "artifacts")]
enum Evidence {
    Wifi(Box<LaneEvidence>),
    Ieee802154(Box<ieee::Evidence>),
    BluetoothLe {
        decoder_center_hz: u64,
        tune_evidence_sha256: String,
        validated_frames: u64,
    },
}

impl Evidence {
    fn validated_frames(&self) -> u64 {
        match self {
            Self::Wifi(evidence) => evidence.validated_packets() as u64,
            Self::Ieee802154(evidence) => evidence.validated_frames,
            Self::BluetoothLe {
                validated_frames, ..
            } => *validated_frames,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
enum LaneResult {
    NoEligibleTune,
    Confirmed {
        validated_frames: u64,
        evidence: Evidence,
    },
    NoValidatedFrames {
        evidence: Evidence,
    },
    Failed {
        detail: String,
    },
    Interrupted,
}

#[derive(Serialize)]
struct ResultRow<'a> {
    channel: Channel,
    #[serde(flatten)]
    location: Option<Location<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_directory: Option<String>,
    #[serde(flatten)]
    result: LaneResult,
}

#[derive(Serialize)]
struct CaptureRow<'a> {
    #[serde(flatten)]
    location: Location<'a>,
    #[serde(flatten)]
    source: &'a SourceReport,
}

#[derive(Serialize)]
struct ScopeReport {
    channel_scope: ChannelScope,
    eligible_channel_count: usize,
    planned_decoder_attempt_count: usize,
}

#[derive(Clone, Copy, Serialize)]
struct Location<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune: &'a str,
}

impl<'a> Location<'a> {
    fn new(batch: &'a admission::Batch, visit: Visit) -> Self {
        let pass = &batch.passes[visit.pass];
        Self {
            pass_directory: pass.directory.as_deref(),
            tune: &pass.sweep.receipt.tunes[visit.tune].directory,
        }
    }

    fn parent(self, nested: bool) -> Option<String> {
        if let Some(pass) = self.pass_directory {
            Some(format!("{pass}/{}", self.tune))
        } else {
            nested.then(|| self.tune.to_owned())
        }
    }
}

#[derive(Clone, Copy, Serialize)]
struct SourceReceipt<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep_series_sha256: Option<&'a str>,
}

#[derive(Serialize)]
struct Identification<'a> {
    schema_version: u32,
    operation: &'static str,
    overall_state: OverallState,
    #[serde(flatten)]
    source_receipt: SourceReceipt<'a>,
    lower_hz: u64,
    upper_hz: u64,
    requested_protocols: &'a [Protocol],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    all_visits: bool,
    #[serde(flatten)]
    eligible_scope: Option<ScopeReport>,
    programs: Vec<&'a ProgramProvenance>,
    captures: Vec<CaptureRow<'a>>,
    results: Vec<ResultRow<'a>>,
}

#[derive(Serialize)]
struct PlanRow<'a> {
    channel: Channel,
    #[serde(flatten)]
    location: Option<Location<'a>>,
}

#[derive(Serialize)]
struct Preview<'a> {
    schema_version: u32,
    operation: &'static str,
    #[serde(flatten)]
    source_receipt: SourceReceipt<'a>,
    lower_hz: u64,
    upper_hz: u64,
    requested_protocols: &'a [Protocol],
    all_visits: bool,
    #[serde(flatten)]
    scope: ScopeReport,
    distinct_tune_count: usize,
    admitted_iq_samples: u64,
    admitted_iq_bytes: u64,
    max_attempts: u16,
    max_samples_per_tune: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_total_samples: Option<u64>,
    routes: Vec<PlanRow<'a>>,
}

/// Plan or identify supported channels from bounded saved files.
///
/// Preview reads receipts only and prints to stdout. Execution returns the
/// terminal report directory and state after validating the selected IQ.
pub(crate) fn run(args: Args, stop: &AtomicBool) -> Result<Option<(PathBuf, OverallState)>> {
    let batch = admission::Batch::open(&args.input, args.series)?;
    let receipt = &batch.passes[0].sweep.receipt;
    let routes = routes(&batch, &args.protocols, args.channel_scope, args.all_visits);
    let attempts = count_attempts(&routes);
    ensure!(
        attempts <= usize::from(args.max_attempts),
        "sweep needs {attempts} replays, exceeding --max-attempts"
    );
    let visits: BTreeSet<_> = routes.iter().filter_map(|route| route.tune).collect();
    let mut admitted_iq_samples = 0;
    for &visit in &visits {
        let location = Location::new(&batch, visit);
        let samples = batch.passes[visit.pass]
            .sweep
            .receipt
            .samples_at(visit.tune)?;
        ensure!(
            samples <= args.max_samples_per_tune,
            "visit {}/{} needs {samples} samples, exceeding --max-samples-per-tune ({})",
            location.pass_directory.unwrap_or("sweep"),
            location.tune,
            args.max_samples_per_tune
        );
        admitted_iq_samples += samples;
    }
    if let Some(max_total) = args.max_total_samples {
        ensure!(
            admitted_iq_samples <= max_total,
            "sweep needs {admitted_iq_samples} distinct IQ samples, exceeding --max-total-samples ({max_total})"
        );
    }
    let scope = ScopeReport {
        channel_scope: args.channel_scope,
        eligible_channel_count: routes
            .iter()
            .filter_map(|route| route.tune.map(|_| route.channel))
            .collect::<BTreeSet<_>>()
            .len(),
        planned_decoder_attempt_count: attempts,
    };
    let source_receipt = SourceReceipt {
        sweep_sha256: (!args.series).then_some(batch.sha256.as_str()),
        sweep_series_sha256: args.series.then_some(batch.sha256.as_str()),
    };
    if args.plan_only {
        batch.verify()?;
        ensure!(
            !stop.load(Ordering::Relaxed),
            "sweep identification planning interrupted"
        );
        let preview = Preview {
            schema_version: 1,
            operation: "plan_saved_sweep_identification",
            source_receipt,
            lower_hz: receipt.lower_hz,
            upper_hz: receipt.upper_hz,
            requested_protocols: &args.protocols,
            all_visits: args.all_visits,
            scope,
            distinct_tune_count: visits.len(),
            admitted_iq_samples,
            admitted_iq_bytes: admitted_iq_samples * 4,
            max_attempts: args.max_attempts,
            max_samples_per_tune: args.max_samples_per_tune,
            max_total_samples: args.max_total_samples,
            routes: routes
                .iter()
                .map(|route| PlanRow {
                    channel: route.channel,
                    location: route.tune.map(|visit| Location::new(&batch, visit)),
                })
                .collect(),
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &preview)
            .context("write sweep identification plan")?;
        writeln!(stdout)?;
        stdout.flush().context("flush sweep identification plan")?;
        return Ok(None);
    }

    let eligible_scope =
        (args.channel_scope == ChannelScope::Eligible || args.series || args.all_visits)
            .then_some(scope);
    let programs = Programs::resolve(&routes, &args)?;
    let mut captures = BTreeMap::new();
    for visit in visits {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "identification interrupted during source validation"
        );
        let capture = batch.passes[visit.pass]
            .sweep
            .admit(visit.tune, args.max_samples_per_tune)?;
        captures.insert(visit, capture);
    }
    batch.verify()?;
    let output = report::fresh_output(
        args.output
            .as_deref()
            .context("supply --output for identification")?,
    )?;
    let nested = args.series || args.all_visits;
    if nested {
        for &visit in captures.keys() {
            if let Some(parent) = Location::new(&batch, visit).parent(nested) {
                let path = output.join(parent);
                DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&path)
                    .with_context(|| {
                        format!("create private visit directory {}", path.display())
                    })?;
            }
        }
    }
    let mut bluetooth_replays = BTreeMap::new();
    let results = routes
        .into_iter()
        .map(|route| {
            let Some(visit) = route.tune else {
                return ResultRow {
                    channel: route.channel,
                    location: None,
                    evidence_directory: None,
                    result: LaneResult::NoEligibleTune,
                };
            };
            let location = Location::new(&batch, visit);
            let lane = route.channel.directory_name(location.tune);
            let name = match location.parent(nested) {
                Some(parent) => format!("{parent}/{lane}"),
                None => lane,
            };
            let mut evidence_directory = None;
            let result = if stop.load(Ordering::Relaxed) {
                LaneResult::Interrupted
            } else {
                let capture = captures
                    .get(&visit)
                    .context("missing admitted sweep capture");
                let directory = output.join(&name);
                let evidence = capture.and_then(|capture| {
                    programs.decode(&route, capture, &directory, &mut bluetooth_replays, stop)
                });
                if directory.is_dir() {
                    evidence_directory = Some(name);
                }
                match evidence {
                    Ok(evidence) => match evidence.validated_frames() {
                        0 => LaneResult::NoValidatedFrames { evidence },
                        validated_frames => LaneResult::Confirmed {
                            validated_frames,
                            evidence,
                        },
                    },
                    Err(_) if stop.load(Ordering::Relaxed) => LaneResult::Interrupted,
                    Err(error) => LaneResult::Failed {
                        detail: format!("{error:#}"),
                    },
                }
            };
            ResultRow {
                channel: route.channel,
                location: Some(location),
                evidence_directory,
                result,
            }
        })
        .collect::<Vec<_>>();

    batch.verify()?;
    for capture in captures.values() {
        capture.verify()?;
    }
    let identities = programs.identities();
    for program in &identities {
        let observed = report::hash_direct_regular_file(&program.path, program.bytes)?;
        ensure!(
            observed.bytes == program.bytes && observed.sha256 == program.sha256,
            "decoder executable changed during identification: {}",
            program.name
        );
    }
    let overall_state = if stop.load(Ordering::Relaxed) {
        OverallState::Interrupted
    } else if results
        .iter()
        .any(|row| matches!(row.result, LaneResult::Failed { .. }))
    {
        OverallState::Failed
    } else {
        OverallState::Complete
    };
    report::publish_json(
        &output,
        "identification.json",
        &Identification {
            schema_version: 1,
            operation: match (args.series, receipt.is_targeted()) {
                (false, false) => "identify_saved_sweep",
                (false, true) => "identify_targeted_rf_sweep",
                (true, false) => "identify_saved_sweep_series",
                (true, true) => "identify_targeted_rf_sweep_series",
            },
            overall_state,
            source_receipt,
            lower_hz: receipt.lower_hz,
            upper_hz: receipt.upper_hz,
            requested_protocols: &args.protocols,
            all_visits: args.all_visits,
            eligible_scope,
            programs: identities,
            captures: captures
                .iter()
                .map(|(&visit, capture)| CaptureRow {
                    location: Location::new(&batch, visit),
                    source: &capture.source,
                })
                .collect(),
            results,
        },
    )?;
    Ok(Some((output, overall_state)))
}
