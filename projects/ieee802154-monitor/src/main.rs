//! Bounded offline preparation and inspection for IEEE 802.15.4 decoders.
//!
//! This binary does not open a radio, network socket or transmit path. Channelized
//! samples and demodulator candidates are evidence for later analysis, not proof of
//! transmitter identity, complete reception or authenticated protocol content.

#![forbid(unsafe_code)]

mod candidates;
mod compare;
mod hypotheses;
mod source_index;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ieee802154_monitor::channelize;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Offline IEEE 802.15.4 receive preparation")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Extract channel-centered 4 MSps CF32 streams from a 20 MSps SC16_Q11 capture.
    Channelize(ChannelizeArgs),
    /// Extract one arbitrary-frequency 4 MSps CF32 stream from 20 MSps SC16_Q11.
    ChannelizeFrequency(ChannelizeFrequencyArgs),
    /// Validate raw demodulator candidates and independently check IEEE 802.15.4 FCS.
    InspectCandidates(InspectCandidatesArgs),
    /// Compare hard- and soft-lane candidates by exact decoder-output identity.
    CompareCandidates(CompareCandidatesArgs),
    /// Compare six decoder hypotheses by exact decoder-output identity.
    CompareHypotheses(CompareHypothesesArgs),
}

#[derive(Debug, Args)]
struct ChannelizeFrequencyArgs {
    /// SC16_Q11 input; its metadata must be in INPUT.json.
    input: PathBuf,
    /// New output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Target center frequency in hertz.
    #[arg(long)]
    frequency_hz: u64,
    /// Maximum source complex samples processed from the input prefix.
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
}

#[derive(Debug, Args)]
struct ChannelizeArgs {
    /// SC16_Q11 input; its metadata must be in INPUT.json.
    input: PathBuf,
    /// New output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Comma-separated IEEE 802.15.4 channel numbers (11..=26).
    #[arg(long, default_value = "16,17,18")]
    channels: String,
    /// Allow a requested channel's stopband edge to reach source Nyquist.
    #[arg(long)]
    allow_nyquist_edge: bool,
    /// Maximum source complex samples processed from the input prefix.
    #[arg(long, default_value_t = 20_000_000)]
    max_samples: u64,
}

#[derive(Debug, Args)]
struct InspectCandidatesArgs {
    /// Versioned raw candidate archive produced by the file-only decoder worker.
    input: PathBuf,
    /// New output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Maximum accepted candidate archive size in MiB.
    #[arg(long, default_value_t = 64)]
    max_input_mib: u32,
    /// Maximum combined analysis output size in MiB, including the summary.
    #[arg(long, default_value_t = 512)]
    max_output_mib: u32,
    /// Maximum candidate records accepted from one archive.
    #[arg(long, default_value_t = 1_000_000)]
    max_records: u64,
    /// I154IDX v1 sidecar from the same successful hard-decoder run.
    #[arg(long, requires_all = [
        "source_complex_samples",
        "owned_start_channel_sample",
        "owned_end_channel_sample",
    ])]
    source_index: Option<PathBuf>,
    /// Number of complex samples in the complete 4 MSps CF32 worker input.
    #[arg(long, requires = "source_index")]
    source_complex_samples: Option<u64>,
    /// First owned CF32 sample, inclusive.
    #[arg(long, requires = "source_index")]
    owned_start_channel_sample: Option<u64>,
    /// End of the owned CF32 sample interval, exclusive.
    #[arg(long, requires = "source_index")]
    owned_end_channel_sample: Option<u64>,
}

#[derive(Debug, Args)]
struct CompareCandidatesArgs {
    /// Hard-lane versioned raw candidate archive.
    hard: PathBuf,
    /// Soft-lane versioned raw candidate archive.
    soft: PathBuf,
    /// New output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Maximum accepted size of each candidate archive in MiB.
    #[arg(long, default_value_t = 64)]
    max_input_mib: u32,
    /// Maximum candidate records accepted from each archive.
    #[arg(long, default_value_t = 1_000_000)]
    max_records: u64,
}

#[derive(Debug, Args)]
struct CompareHypothesesArgs {
    /// Decoder hypothesis as separate LANE OFFSET_HZ PATH values; repeat exactly six times.
    #[arg(
        long = "source",
        value_names = ["LANE", "OFFSET_HZ", "PATH"],
        num_args = 3,
        action = clap::ArgAction::Append,
        allow_hyphen_values = true
    )]
    sources: Vec<OsString>,
    /// New output directory. Existing paths are preserved.
    #[arg(long)]
    output: PathBuf,
    /// Maximum accepted size of each candidate archive in MiB.
    #[arg(long, default_value_t = 64)]
    max_input_mib: u32,
    /// Maximum candidate records accepted from each archive.
    #[arg(long, default_value_t = 1_000_000)]
    max_records: u64,
}

fn print_summary(summary: &impl Serialize, label: &str, output: &Path) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", serde_json::to_string_pretty(summary)?)?;
    writeln!(stdout, "{label}: {}", output.display())?;
    stdout.flush().context("flush candidate summary")
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Action::Channelize(args) => {
            let request = channelize::Request {
                input: args.input,
                output: args.output,
                channels: channelize::parse_channels(&args.channels)?,
                max_samples: args.max_samples,
            };
            if args.allow_nyquist_edge {
                channelize::run_allowing_nyquist_edge(request)
            } else {
                channelize::run(request)
            }
        }
        Action::ChannelizeFrequency(args) => {
            channelize::run_frequency(channelize::FrequencyRequest {
                input: args.input,
                output: args.output,
                frequency_hz: args.frequency_hz,
                max_samples: args.max_samples,
            })
        }
        Action::InspectCandidates(args) => {
            let limits = candidates::Limits::from_cli(
                args.max_input_mib,
                args.max_output_mib,
                args.max_records,
            )?;
            let source_index = match (
                args.source_index,
                args.source_complex_samples,
                args.owned_start_channel_sample,
                args.owned_end_channel_sample,
            ) {
                (Some(file), Some(source_complex_samples), Some(start), Some(end)) => {
                    Some(source_index::Request {
                        file,
                        source_complex_samples,
                        owned_samples: start..end,
                    })
                }
                (None, None, None, None) => None,
                _ => anyhow::bail!("source index requires all four indexed inspection options"),
            };
            candidates::run(candidates::Request {
                input: args.input,
                output: args.output,
                limits,
                source_index,
            })?;
            Ok(())
        }
        Action::CompareCandidates(args) => {
            let limits = compare::Limits::from_cli(args.max_input_mib, args.max_records)?;
            compare::run(compare::Request {
                hard_input: args.hard,
                soft_input: args.soft,
                output: args.output,
                limits,
            })?;
            Ok(())
        }
        Action::CompareHypotheses(args) => {
            let sources = hypotheses::sources_from_cli(&args.sources)?;
            let limits = hypotheses::Limits::from_cli(args.max_input_mib, args.max_records)?;
            hypotheses::run(hypotheses::Request {
                sources,
                output: args.output,
                limits,
            })?;
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr().lock(), "{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channelize_defaults_to_every_fully_covered_channel_at_2437_mhz() {
        let cli = Cli::try_parse_from([
            "ieee802154-monitor",
            "channelize",
            "input.sc16q11",
            "--output",
            "out",
        ])
        .unwrap();
        let Action::Channelize(args) = cli.command else {
            panic!("expected channelize action");
        };
        assert_eq!(
            channelize::parse_channels(&args.channels).unwrap(),
            [16, 17, 18]
        );
        assert!(!args.allow_nyquist_edge);
    }

    #[test]
    fn nyquist_edge_allowance_is_explicit_and_nominal_only() {
        let cli = Cli::try_parse_from([
            "ieee802154-monitor",
            "channelize",
            "input.sc16q11",
            "--output",
            "out",
            "--channels",
            "19",
            "--allow-nyquist-edge",
        ])
        .unwrap();
        let Action::Channelize(args) = cli.command else {
            panic!("expected channelize action");
        };
        assert!(args.allow_nyquist_edge);
        assert_eq!(channelize::parse_channels(&args.channels).unwrap(), [19]);
        assert!(
            Cli::try_parse_from([
                "ieee802154-monitor",
                "channelize-frequency",
                "input.sc16q11",
                "--output",
                "out",
                "--frequency-hz",
                "2445000000",
                "--allow-nyquist-edge",
            ])
            .is_err()
        );
    }

    #[test]
    fn arbitrary_frequency_channelization_has_a_separate_cli_contract() {
        let cli = Cli::try_parse_from([
            "ieee802154-monitor",
            "channelize-frequency",
            "input.sc16q11",
            "--output",
            "out",
            "--frequency-hz",
            "2438000000",
        ])
        .unwrap();
        let Action::ChannelizeFrequency(args) = cli.command else {
            panic!("expected channelize-frequency action");
        };
        assert_eq!(args.input, PathBuf::from("input.sc16q11"));
        assert_eq!(args.output, PathBuf::from("out"));
        assert_eq!(args.frequency_hz, 2_438_000_000);
        assert_eq!(args.max_samples, 20_000_000);
    }

    #[test]
    fn indexed_inspection_requires_the_sidecar_and_full_sample_window_together() {
        let base = [
            "ieee802154-monitor",
            "inspect-candidates",
            "in.i154raw",
            "--output",
            "out",
        ];
        let indexed = [
            "--source-index",
            "in.i154idx",
            "--source-complex-samples",
            "12",
            "--owned-start-channel-sample",
            "5",
            "--owned-end-channel-sample",
            "8",
        ];
        assert!(Cli::try_parse_from(base).is_ok());
        assert!(Cli::try_parse_from(base.into_iter().chain(indexed)).is_ok());
        for args in indexed.as_chunks::<2>().0 {
            assert!(Cli::try_parse_from(base.into_iter().chain(args.iter().copied())).is_err());
        }
    }

    #[test]
    fn compare_candidates_roles_are_explicit_and_bounded() {
        let cli = Cli::try_parse_from([
            "ieee802154-monitor",
            "compare-candidates",
            "hard.i154raw",
            "soft.i154raw",
            "--output",
            "out",
            "--max-input-mib",
            "64",
            "--max-records",
            "1000000",
        ])
        .unwrap();
        let Action::CompareCandidates(args) = cli.command else {
            panic!("expected compare-candidates action");
        };
        assert_eq!(args.hard, PathBuf::from("hard.i154raw"));
        assert_eq!(args.soft, PathBuf::from("soft.i154raw"));
        assert_eq!(args.output, PathBuf::from("out"));
        assert_eq!(args.max_input_mib, 64);
        assert_eq!(args.max_records, 1_000_000);
    }

    #[test]
    fn compare_hypotheses_preserves_six_source_groups_in_caller_order() {
        let cli = Cli::try_parse_from([
            "ieee802154-monitor",
            "compare-hypotheses",
            "--source",
            "hard",
            "0",
            "zero-hard.i154raw",
            "--source",
            "soft",
            "0",
            "zero-soft.i154raw",
            "--source",
            "hard",
            "-100000",
            "negative-hard.i154raw",
            "--source",
            "soft",
            "-100000",
            "negative-soft.i154raw",
            "--source",
            "hard",
            "100000",
            "positive-hard.i154raw",
            "--source",
            "soft",
            "100000",
            "positive-soft.i154raw",
            "--output",
            "out",
        ])
        .unwrap();
        let Action::CompareHypotheses(args) = cli.command else {
            panic!("expected compare-hypotheses action");
        };
        let sources = hypotheses::sources_from_cli(&args.sources).unwrap();
        assert_eq!(sources.len(), 6);
        assert_eq!(sources[0].decoder_lane, hypotheses::DecoderLane::Hard);
        assert_eq!(sources[0].carrier_offset_hz, 0);
        assert_eq!(sources[0].input, PathBuf::from("zero-hard.i154raw"));
        assert_eq!(sources[2].decoder_lane, hypotheses::DecoderLane::Hard);
        assert_eq!(sources[2].carrier_offset_hz, -100_000);
        assert_eq!(sources[5].decoder_lane, hypotheses::DecoderLane::Soft);
        assert_eq!(sources[5].carrier_offset_hz, 100_000);
        assert_eq!(args.output, PathBuf::from("out"));
    }
}
