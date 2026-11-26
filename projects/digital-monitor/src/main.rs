//! Bounded multi-protocol analysis for completed bladeRF IQ captures.
//!
//! `analyze` owns no radio, network or transmit interface. `watch` starts only
//! the explicit receive-only acquisition child, waits for its complete process-
//! group shutdown, and then invokes the same closed-file analysis pipeline.

#![forbid(unsafe_code)]

mod event_replay;
mod identify_sweep;
mod input;
mod orchestrate;
mod process;
mod recurrence;
mod recurrence_compare;
mod report;
mod rolling;
mod watch;

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::report::OverallState;

#[derive(Debug, Parser)]
#[command(about = "Bounded multi-protocol capture and analysis of completed IQ files")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Analyze one complete 20 MSps SC16_Q11 capture without opening a radio.
    Analyze(Analyze),
    /// Identify supported digital protocols in a saved RF observatory sweep.
    IdentifySweep(identify_sweep::Args),
    /// Print exact tuning centers for decoder channels the saved sweep missed.
    PlanSweepFollowUp(identify_sweep::FollowUpArgs),
    /// Inventory recurring waveform-family bins in one terminal watch report.
    InventoryRecurrence(InventoryRecurrence),
    /// Compare two compatible mixed-center recurrence inventories without opening a radio.
    CompareRecurrence(CompareRecurrence),
    /// Replay verified event slices and compare validated frame occurrences.
    CompareEventReplay(event_replay::CompareEventReplay),
    /// Export active-window IQ from a closed capture and compare both decoder lanes.
    QualifyEventReplay(event_replay::QualifyEventReplay),
    /// Repeat finite RX1 snapshots at a bounded 2.4 GHz center.
    Watch(Watch),
    /// Detect activity in closed segments from one finite, timestamp-checked RX1 stream.
    WatchSegments(rolling::Arguments),
}

#[derive(Debug, Args)]
struct InventoryRecurrence {
    /// Terminal schema-2, schema-3, schema-4 or schema-5 run.json; no sessions are opened.
    run_json: PathBuf,
    /// New private output directory containing recurrence.json.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Args)]
struct CompareRecurrence {
    /// Reference mixed-center recurrence schema-3 report.
    baseline: PathBuf,
    /// Subject mixed-center recurrence schema-3 report.
    current: PathBuf,
    /// New private output directory containing comparison.json.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Clone, Debug, Args)]
struct AnalyzerPrograms {
    /// Prebuilt Wi-Fi monitor executable; no PATH lookup or build occurs.
    #[arg(long, default_value = "target/release/wifi-monitor")]
    wifi_monitor: PathBuf,
    /// Prebuilt RF observatory executable; no PATH lookup or build occurs.
    #[arg(long, default_value = "target/release/rf-observatory")]
    rf_observatory: PathBuf,
    /// Prebuilt IEEE 802.15.4 monitor executable.
    #[arg(long, default_value = "target/release/ieee802154-monitor")]
    ieee802154_monitor: PathBuf,
    /// Prebuilt isolated FutureSDR IEEE 802.15.4 file worker.
    #[arg(
        long,
        default_value = "projects/ieee802154-monitor/futuresdr-worker/target/release/ieee802154-futuresdr-worker"
    )]
    ieee_worker: PathBuf,
    /// Prebuilt Bluetooth monitor executable; no PATH lookup or build occurs.
    #[arg(long, default_value = "target/release/bluetooth-monitor")]
    bluetooth_monitor: PathBuf,
}

impl AnalyzerPrograms {
    fn paths(&self) -> orchestrate::ProgramPaths {
        orchestrate::ProgramPaths {
            wifi: self.wifi_monitor.clone(),
            rf: self.rf_observatory.clone(),
            ieee: self.ieee802154_monitor.clone(),
            ieee_worker: self.ieee_worker.clone(),
            bluetooth: self.bluetooth_monitor.clone(),
        }
    }
}

#[derive(Debug, Args)]
struct Analyze {
    /// Complete SC16_Q11 input; acquisition metadata must be in INPUT.json.
    input: PathBuf,
    /// New private output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Integer center frequency in MHz; it must match the sidecar.
    #[arg(long, default_value_t = 2437)]
    center_mhz: u32,
    /// Complex sample rate in hertz; the current pipeline requires 20 MSps.
    #[arg(long, default_value_t = 20_000_000)]
    sample_rate: u64,
    /// Full-input sample-count acceptance ceiling.
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
    /// Distributed FFT windows for relative activity detection.
    #[arg(long, default_value_t = 128)]
    windows: usize,
    /// Inner Wi-Fi/Bluetooth decoder deadline in seconds.
    #[arg(long, default_value_t = 120)]
    decoder_timeout: u64,
    /// Independent deadline for each direct child process in seconds.
    #[arg(long, default_value_t = 180)]
    child_timeout: u64,
    /// Retained prefix per child stdout/stderr stream in MiB.
    #[arg(long, default_value_t = 1)]
    log_limit_mib: u64,
    #[command(flatten)]
    programs: AnalyzerPrograms,
}

#[derive(Debug, Args)]
struct Watch {
    /// New private run directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Finite snapshot count; a failed snapshot still consumes its cycle.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..=1000))]
    cycles: u16,
    /// Integer MHz center; this range preserves IEEE 802.15.4 channels 16--18.
    #[arg(long, default_value_t = 2437, value_parser = clap::value_parser!(u32).range(2433..=2437))]
    center_mhz: u32,
    /// Optional second center; attempts alternate primary, secondary, primary.
    #[arg(long, value_parser = clap::value_parser!(u32).range(2433..=2437))]
    alternate_center_mhz: Option<u32>,
    /// Seconds per RX1 snapshot, up to two. Coded shadow requires at most one.
    #[arg(long, default_value_t = 1.0)]
    duration: f64,
    /// Initial complex samples to discard on each active RX1 stream.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(..=watch::MAX_DISCARD_INITIAL_SAMPLES))]
    discard_initial_samples: u64,
    /// Manual RX1 gain in dB.
    #[arg(long, default_value_t = 40.0)]
    gain: f64,
    /// Stop after this many consecutive incomplete capture/analysis cycles.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=100))]
    max_consecutive_failures: u16,
    /// Maximum successful sessions; rare candidate evidence is retained first.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u16).range(1..=100))]
    keep_sessions: u16,
    /// Stop threshold checked between cycles; this is not a filesystem quota.
    #[arg(long, default_value_t = 1024)]
    retained_limit_mib: u32,
    /// Later captures allowed while the preceding closed file is analyzed.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=1))]
    pending_captures: u8,
    /// Run a separate file-only LE Coded shadow on eligible 2435 MHz captures.
    #[arg(long)]
    bluetooth_coded_shadow: bool,
    /// Wall-clock deadline for each acquisition child.
    #[arg(long, default_value_t = 30)]
    capture_timeout: u64,
    /// Distributed FFT windows for relative activity detection.
    #[arg(long, default_value_t = 128)]
    windows: usize,
    /// Inner Wi-Fi/Bluetooth decoder deadline in seconds.
    #[arg(long, default_value_t = 120)]
    decoder_timeout: u64,
    /// Independent deadline for each analysis child in seconds.
    #[arg(long, default_value_t = 180)]
    child_timeout: u64,
    /// Retained prefix per child stdout/stderr stream in MiB.
    #[arg(long, default_value_t = 1)]
    log_limit_mib: u64,
    /// Prebuilt receive-only bladeRF acquisition executable.
    #[arg(long, default_value = "target/release/bladerf-lab")]
    receiver: PathBuf,
    #[command(flatten)]
    programs: AnalyzerPrograms,
}

fn print_status(message: fmt::Arguments<'_>) -> Result<()> {
    writeln!(std::io::stdout().lock(), "{message}").context("write run status")
}

fn print_error(message: fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr().lock(), "{message}");
}

fn run(cli: Cli, stop: &AtomicBool) -> Result<ExitCode> {
    match cli.command {
        Action::PlanSweepFollowUp(args) => {
            identify_sweep::print_follow_up(args, stop)?;
            Ok(ExitCode::SUCCESS)
        }
        Action::IdentifySweep(args) => {
            let Some((output, state)) = identify_sweep::run(args, stop)? else {
                return Ok(ExitCode::SUCCESS);
            };
            print_status(format_args!(
                "Terminal sweep identification: {}\nOverall state: {state:?}",
                output.join("identification.json").display()
            ))?;
            Ok(if state == OverallState::Complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Action::Analyze(args) => {
            let (output, state) = orchestrate::analyze(
                orchestrate::Request {
                    input: args.input,
                    output: args.output,
                    center_mhz: args.center_mhz,
                    sample_rate_hz: args.sample_rate,
                    max_samples: args.max_samples,
                    spectrum_windows: args.windows,
                    decoder_timeout_seconds: args.decoder_timeout,
                    child_timeout_seconds: args.child_timeout,
                    log_limit_mib: args.log_limit_mib,
                    programs: args.programs.paths(),
                },
                stop,
            )?;
            print_status(format_args!(
                "Terminal analysis: {}\nOverall state: {state:?}",
                output.join("analysis.json").display()
            ))?;
            Ok(if state == OverallState::Complete {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Action::InventoryRecurrence(args) => {
            let result = recurrence::inventory(
                recurrence::Request {
                    run_json: args.run_json,
                    output: args.output,
                },
                stop,
            )?;
            print_status(format_args!(
                "Terminal recurrence inventory: {}",
                result.output.join("recurrence.json").display()
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::CompareRecurrence(args) => {
            let result = recurrence_compare::compare(
                recurrence_compare::Request {
                    baseline: args.baseline,
                    current: args.current,
                    output: args.output,
                },
                stop,
            )?;
            print_status(format_args!(
                "Terminal recurrence comparison: {}",
                result.output.join("comparison.json").display()
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::CompareEventReplay(args) => {
            let output = event_replay::compare(&args, stop)?;
            print_status(format_args!(
                "Terminal event replay comparison: {}",
                output.join("comparison.json").display()
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::QualifyEventReplay(args) => {
            let output = event_replay::qualify(&args, stop)?;
            print_status(format_args!(
                "Completed event replay qualification in {}",
                output.display()
            ))?;
            Ok(ExitCode::SUCCESS)
        }
        Action::Watch(args) => {
            let result = watch::watch(
                watch::Request {
                    output: args.output,
                    cycles: args.cycles,
                    center_mhz: args.center_mhz,
                    alternate_center_mhz: args.alternate_center_mhz,
                    duration_seconds: args.duration,
                    discard_initial_samples: args.discard_initial_samples,
                    gain_db: args.gain,
                    max_consecutive_failures: args.max_consecutive_failures,
                    keep_sessions: args.keep_sessions,
                    retained_limit_mib: args.retained_limit_mib,
                    pending_captures: args.pending_captures,
                    bluetooth_coded_shadow: args.bluetooth_coded_shadow,
                    capture_timeout_seconds: args.capture_timeout,
                    spectrum_windows: args.windows,
                    decoder_timeout_seconds: args.decoder_timeout,
                    child_timeout_seconds: args.child_timeout,
                    log_limit_mib: args.log_limit_mib,
                    receiver: args.receiver,
                    programs: args.programs.paths(),
                },
                stop,
            )?;
            print_status(format_args!(
                "Terminal watch report: {}",
                result.output.join("run.json").display()
            ))?;
            Ok(if result.successful {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Action::WatchSegments(args) => rolling::run(args, stop),
    }
}

fn main() -> ExitCode {
    let stop = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&stop);
    if let Err(error) = ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .context("install shutdown handler")
    {
        print_error(format_args!("{error:#}"));
        return ExitCode::FAILURE;
    }
    match run(Cli::parse(), &stop) {
        Ok(code) => code,
        Err(error) => {
            print_error(format_args!("{error:#}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_are_the_current_observation_contract() {
        let cli = Cli::try_parse_from([
            "digital-monitor",
            "analyze",
            "iq.sc16q11",
            "--output",
            "analysis",
        ])
        .unwrap();
        let Action::Analyze(args) = cli.command else {
            panic!("analyze")
        };
        assert_eq!(args.center_mhz, 2437);
        assert_eq!(args.sample_rate, 20_000_000);
        assert_eq!(args.max_samples, 20_000_000);
        assert_eq!(args.windows, 128);
        assert_eq!(args.decoder_timeout, 120);
        assert_eq!(args.child_timeout, 180);
    }

    #[test]
    fn watch_defaults_pin_the_current_receive_contract() {
        let cli = Cli::try_parse_from(["digital-monitor", "watch", "--output", "run"]).unwrap();
        let Action::Watch(args) = cli.command else {
            panic!("watch")
        };
        assert_eq!(args.cycles, 10);
        assert_eq!(args.center_mhz, 2437);
        assert_eq!(args.alternate_center_mhz, None);
        assert_eq!(args.duration, 1.0);
        assert_eq!(args.discard_initial_samples, 0);
        assert_eq!(args.gain, 40.0);
        assert_eq!(args.max_consecutive_failures, 3);
        assert_eq!(args.keep_sessions, 3);
        assert_eq!(args.retained_limit_mib, 1024);
        assert_eq!(args.pending_captures, 0);
        assert_eq!(args.capture_timeout, 30);
    }

    #[test]
    fn watch_center_preserves_all_fixed_ieee_channels() {
        for value in ["2433", "2437"] {
            Cli::try_parse_from([
                "digital-monitor",
                "watch",
                "--output",
                "run",
                "--center-mhz",
                value,
            ])
            .unwrap();
        }
        for value in ["2432", "2438"] {
            assert!(
                Cli::try_parse_from([
                    "digital-monitor",
                    "watch",
                    "--output",
                    "run",
                    "--center-mhz",
                    value,
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn watch_accepts_one_bounded_alternate_center() {
        let cli = Cli::try_parse_from([
            "digital-monitor",
            "watch",
            "--output",
            "run",
            "--center-mhz",
            "2435",
            "--alternate-center-mhz",
            "2437",
        ])
        .unwrap();
        let Action::Watch(args) = cli.command else {
            panic!("watch")
        };
        assert_eq!(args.center_mhz, 2435);
        assert_eq!(args.alternate_center_mhz, Some(2437));

        for value in ["2432", "2438"] {
            assert!(
                Cli::try_parse_from([
                    "digital-monitor",
                    "watch",
                    "--output",
                    "run",
                    "--alternate-center-mhz",
                    value,
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn watch_accepts_only_the_bounded_pipeline_depth() {
        for value in ["0", "1"] {
            Cli::try_parse_from([
                "digital-monitor",
                "watch",
                "--output",
                "run",
                "--pending-captures",
                value,
            ])
            .unwrap();
        }
        assert!(
            Cli::try_parse_from([
                "digital-monitor",
                "watch",
                "--output",
                "run",
                "--pending-captures",
                "2",
            ])
            .is_err()
        );
    }

    #[test]
    fn recurrence_inventory_requires_input_and_output_paths() {
        let cli = Cli::try_parse_from([
            "digital-monitor",
            "inventory-recurrence",
            "run.json",
            "--output",
            "inventory",
        ])
        .unwrap();
        let Action::InventoryRecurrence(args) = cli.command else {
            panic!("inventory-recurrence")
        };
        assert_eq!(args.run_json, PathBuf::from("run.json"));
        assert_eq!(args.output, PathBuf::from("inventory"));
        assert!(
            Cli::try_parse_from(["digital-monitor", "inventory-recurrence", "run.json"]).is_err()
        );
    }

    #[test]
    fn recurrence_comparison_requires_ordered_inputs_and_output() {
        let cli = Cli::try_parse_from([
            "digital-monitor",
            "compare-recurrence",
            "baseline.json",
            "current.json",
            "--output",
            "comparison",
        ])
        .unwrap();
        let Action::CompareRecurrence(args) = cli.command else {
            panic!("compare-recurrence")
        };
        assert_eq!(args.baseline, PathBuf::from("baseline.json"));
        assert_eq!(args.current, PathBuf::from("current.json"));
        assert_eq!(args.output, PathBuf::from("comparison"));
        assert!(
            Cli::try_parse_from([
                "digital-monitor",
                "compare-recurrence",
                "baseline.json",
                "current.json",
            ])
            .is_err()
        );
    }
}
