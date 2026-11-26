//! Route guarded NR synchronization profiles to their original dwell evidence.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::nr::raster::{self, Bounds};
use crate::nr::{self, Profile};
use crate::private_output::publish_json_file;
use crate::spectrum::USABLE_HALF_SPAN_HZ;
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE};

use super::admission::{CompletedCapture, Tune};
use super::check_stop;
use super::lte_sweep::{DEFAULT_TOTAL_CHANNELIZED_SAMPLES, MAX_TOTAL_CHANNELIZED_SAMPLES};
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

/// Replay NR synchronization profiles across a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Synchronization profile as SS_REF_HZ:SCS_KHZ. Repeat for up to 16 profiles.
    #[arg(long, required_unless_present = "nr_raster", conflicts_with = "nr_raster", value_parser = Profile::parse)]
    profile: Vec<Profile>,
    #[command(flatten)]
    raster: RasterArgs,
    /// Verify public broadcast blocks and report CRC-valid master information.
    #[arg(long)]
    nr_pbch: bool,
    /// Maximum original-rate samples selected from an eligible dwell.
    #[arg(long, default_value_t = nr::MAX_PREFIX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=nr::MAX_PREFIX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Maximum aggregate channelized samples across profiles and dwells.
    #[arg(long, default_value_t = DEFAULT_TOTAL_CHANNELIZED_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_CHANNELIZED_SAMPLES))]
    max_total_channelized_samples: u64,
    /// Print NR profile coverage and replay work without reading IQ payloads.
    #[arg(long)]
    plan_only: bool,
    /// New private JSON report file in an existing directory.
    #[arg(
        long,
        required_unless_present = "plan_only",
        conflicts_with = "plan_only"
    )]
    output: Option<PathBuf>,
}

/// Select the standard global NR synchronization raster for offline replay.
#[derive(Debug, clap::Args)]
pub(super) struct RasterArgs {
    /// Generate profiles from the global NR synchronization raster.
    #[arg(long)]
    pub(super) nr_raster: bool,
    /// Half-open SS reference range in hertz. Defaults to the saved capture span.
    #[arg(long, requires = "nr_raster", value_name = "LOWER:UPPER", value_parser = Bounds::parse)]
    nr_raster_hz: Option<Bounds>,
    /// Raster subcarrier spacing in kHz. Repeat to select both (the default).
    #[arg(long = "nr-scs-khz", requires = "nr_raster", value_name = "KHZ", value_parser = parse_scs_khz)]
    nr_scs_hz: Vec<u32>,
}

impl RasterArgs {
    /// Use a finite raster selected by a signal profile.
    pub(super) fn from_profile(bounds: Option<Bounds>, spacings_hz: Vec<u32>) -> Self {
        Self {
            nr_raster: bounds.is_some(),
            nr_raster_hz: bounds,
            nr_scs_hz: spacings_hz,
        }
    }
}

fn parse_scs_khz(value: &str) -> Result<u32> {
    match value {
        "15" => Ok(15_000),
        "30" => Ok(30_000),
        _ => anyhow::bail!("NR raster spacing must be 15 or 30 kHz"),
    }
}

#[derive(Serialize)]
struct RasterScope {
    #[serde(flatten)]
    bounds: Bounds,
    subcarrier_spacings_hz: Vec<u32>,
}

#[derive(Serialize)]
struct Coverage {
    #[serde(skip_serializing_if = "Option::is_none")]
    gscn: Option<u32>,
    profile: Profile,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_samples: u64,
}

impl Coverage {
    fn new(profile: Profile, gscn: Option<u32>) -> Self {
        Self {
            gscn,
            profile,
            covered_dwells: 0,
            short_dwells: 0,
            replayed_samples: 0,
        }
    }
}

struct Dwell<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
    profiles: Vec<Profile>,
}

impl<'a> Dwell<'a> {
    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.selected_samples < nr::MIN_DWELL_SAMPLES {
                0
            } else {
                self.selected_samples
            },
        }
    }
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    tune_directory: &'a str,
    source_sweep_sha256: &'a str,
    source_sidecar_sha256: &'a str,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    ShortDwell { profiles: Vec<Profile> },
    Replayed { replay: nr::Evidence },
}

#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    sample_rate_hz: u32,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    raster: Option<RasterScope>,
    work: Work,
    pub(super) cell_reports: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pbch_reports: Option<usize>,
    profiles: Vec<Coverage>,
    dwells: Vec<Attempt<'a>>,
}

/// Validated coverage and bounded replay work prepared before opening IQ.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    raster: Option<RasterScope>,
    decode_pbch: bool,
    work: Work,
    profiles: Vec<Coverage>,
    dwells: Vec<Dwell<'a>>,
    attempts: Vec<Attempt<'a>>,
    replayer: Option<nr::Replayer>,
}

pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    profiles: &[Profile],
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    ensure!(
        (1..=nr::MAX_PROFILE_COUNT).contains(&profiles.len()),
        "supply 1 through {} NR profiles",
        nr::MAX_PROFILE_COUNT
    );
    let mut profiles = profiles.to_vec();
    profiles.sort_unstable();
    profiles.dedup();
    let profiles = profiles
        .into_iter()
        .map(|profile| Coverage::new(profile, None))
        .collect();
    prepare_coverage(
        capture,
        profiles,
        None,
        max_samples_per_dwell,
        max_total_channelized_samples,
        stop,
    )
}

pub(super) fn prepare_raster<'a>(
    capture: &'a CompletedCapture,
    args: &RasterArgs,
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    let bounds = args
        .nr_raster_hz
        .map_or_else(|| captured_bounds(capture), Ok)?;
    let mut subcarrier_spacings_hz = args.nr_scs_hz.clone();
    if subcarrier_spacings_hz.is_empty() {
        subcarrier_spacings_hz = raster::SPACINGS_HZ.to_vec();
    }
    subcarrier_spacings_hz.sort_unstable();
    subcarrier_spacings_hz.dedup();
    let profiles = raster::entries(bounds, &subcarrier_spacings_hz)?
        .into_iter()
        .map(|entry| Coverage::new(entry.profile, Some(entry.gscn)))
        .collect();
    let raster = RasterScope {
        bounds,
        subcarrier_spacings_hz,
    };
    prepare_coverage(
        capture,
        profiles,
        Some(raster),
        max_samples_per_dwell,
        max_total_channelized_samples,
        stop,
    )
}

fn captured_bounds(capture: &CompletedCapture) -> Result<Bounds> {
    let mut centers = capture
        .passes()
        .into_iter()
        .flat_map(|pass| pass.sweep.tunes.iter().map(|tune| tune.observed_center_hz));
    let first = centers
        .next()
        .context("NR raster needs a saved dwell span")?;
    let (lower, upper) = centers.fold((first, first), |(lower, upper), center| {
        (lower.min(center), upper.max(center))
    });
    let half_span = USABLE_HALF_SPAN_HZ as u64;
    Ok(Bounds {
        lower_hz: (lower - half_span).max(MIN_CENTER_HZ),
        upper_hz: (upper + half_span).min(MAX_CENTER_HZ + 1),
    })
}

fn prepare_coverage<'a>(
    capture: &'a CompletedCapture,
    mut profiles: Vec<Coverage>,
    raster: Option<RasterScope>,
    max_samples_per_dwell: u64,
    max_total_channelized_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    let mut dwells = Vec::new();
    let mut work = Work {
        replayed_iq_samples: 0,
        eligible_attempts: 0,
        short_attempts: 0,
        budget_samples: 0,
        max_budget_samples: max_total_channelized_samples,
        budget_unit: BudgetUnit::Channelized,
    };
    for (index, pass) in capture.passes().into_iter().enumerate() {
        for tune in &pass.sweep.tunes {
            check_stop(stop)?;
            let selected_samples = tune.samples.min(max_samples_per_dwell);
            let mut selected_profiles = Vec::new();
            let half_span = USABLE_HALF_SPAN_HZ as u64;
            let start = profiles.partition_point(|entry| {
                entry.profile.ss_ref_hz() < tune.observed_center_hz - half_span
            });
            let end = profiles.partition_point(|entry| {
                entry.profile.ss_ref_hz() <= tune.observed_center_hz + half_span
            });
            for coverage in profiles[start..end]
                .iter_mut()
                .filter(|entry| entry.profile.covered_by(tune.observed_center_hz))
            {
                coverage.covered_dwells += 1;
                if selected_samples < nr::MIN_DWELL_SAMPLES {
                    coverage.short_dwells += 1;
                } else {
                    coverage.replayed_samples += selected_samples;
                }
                selected_profiles.push(coverage.profile);
            }
            if selected_profiles.is_empty() {
                continue;
            }
            if selected_samples < nr::MIN_DWELL_SAMPLES {
                work.short_attempts += selected_profiles.len();
            } else {
                let channelized_samples: u64 = selected_profiles
                    .iter()
                    .map(|profile| profile.channelized_count(selected_samples) as u64)
                    .sum();
                ensure!(
                    raster.is_some() || channelized_samples <= nr::MAX_DWELL_CHANNEL_SAMPLES,
                    "NR dwell needs {channelized_samples} channel samples, exceeding the {}-sample dwell limit",
                    nr::MAX_DWELL_CHANNEL_SAMPLES
                );
                work.replayed_iq_samples += selected_samples;
                work.eligible_attempts += selected_profiles.len();
                work.budget_samples += channelized_samples;
            }
            dwells.push(Dwell {
                pass_index: index + 1,
                pass_directory: pass.directory,
                source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                tune,
                selected_samples,
                profiles: selected_profiles,
            });
        }
    }
    ensure!(
        work.budget_samples <= max_total_channelized_samples,
        "NR sweep replay needs {} channelized samples, exceeding --max-total-channelized-samples ({max_total_channelized_samples})",
        work.budget_samples
    );
    Ok(Plan {
        capture,
        max_samples_per_dwell,
        raster,
        decode_pbch: false,
        work,
        profiles,
        dwells,
        attempts: Vec::new(),
        replayer: None,
    })
}

#[derive(Serialize)]
struct Preview<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    raster: Option<&'a RasterScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch_max_observations_per_profile: Option<usize>,
    work: Work,
    profiles: &'a [Coverage],
}

impl<'a> Plan<'a> {
    pub(super) fn with_pbch(mut self, enabled: bool) -> Self {
        self.decode_pbch = enabled;
        self
    }

    pub(super) fn work(&self) -> Work {
        self.work
    }

    fn print(&self, stop: &AtomicBool) -> Result<()> {
        self.capture.verify()?;
        check_stop(stop)?;
        let preview = Preview {
            schema_version: 1,
            operation: "plan_nr_sweep_replay",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            minimum_dwell_samples: nr::MIN_DWELL_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            raster: self.raster.as_ref(),
            pbch_max_observations_per_profile: self
                .decode_pbch
                .then_some(nr::PBCH_MAX_OBSERVATIONS),
            work: self.work,
            profiles: &self.profiles,
        };
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, &preview).context("write NR sweep preview")?;
        writeln!(stdout)?;
        stdout.flush().context("flush NR sweep preview")
    }

    /// Return the remaining planned requests in pass and tune order.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.dwells[self.attempts.len()..]
            .iter()
            .map(Dwell::request)
    }

    /// Replay the admitted prefixes. Reverify the capture before publication.
    ///
    /// # Errors
    /// Returns an error for invalid or changed IQ, or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Assemble the standalone receipt after all replay requests complete.
    fn finish(self) -> Report<'a> {
        debug_assert!(self.next_request().is_none());
        let (cell_reports, pbch_reports) = self
            .attempts
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                Outcome::ShortDwell { .. } => None,
                Outcome::Replayed { replay } => Some(replay),
            })
            .fold((0, 0), |(cells, blocks), replay| {
                (cells + replay.cell_count(), blocks + replay.pbch_count())
            });
        Report {
            schema_version: 1,
            operation: "identify_nr_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            minimum_dwell_samples: nr::MIN_DWELL_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            raster: self.raster,
            work: self.work,
            cell_reports,
            pbch_reports: self.decode_pbch.then_some(pbch_reports),
            profiles: self.profiles,
            dwells: self.attempts,
        }
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.requests().next()
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        check_stop(stop)?;
        let dwell = self
            .dwells
            .get_mut(self.attempts.len())
            .context("NR replay has no pending dwell")?;
        let tune = dwell.tune;
        let outcome = if dwell.selected_samples < nr::MIN_DWELL_SAMPLES {
            Outcome::ShortDwell {
                profiles: std::mem::take(&mut dwell.profiles),
            }
        } else {
            let replayer = self
                .replayer
                .get_or_insert_with(|| nr::Replayer::new().with_pbch(self.decode_pbch));
            let replay = if self.raster.is_some() {
                replayer.replay_buffered(
                    input,
                    tune.observed_center_hz,
                    dwell.selected_samples,
                    &dwell.profiles,
                    stop,
                )
            } else {
                replayer.replay(
                    input,
                    tune.observed_center_hz,
                    dwell.selected_samples,
                    &dwell.profiles,
                    stop,
                )
            }?;
            Outcome::Replayed { replay }
        };
        self.attempts.push(Attempt {
            pass_index: dwell.pass_index,
            pass_directory: dwell.pass_directory,
            tune_directory: &tune.directory,
            source_sweep_sha256: dwell.source_sweep_sha256,
            source_sidecar_sha256: &tune.sidecar_sha256,
            observed_center_hz: tune.observed_center_hz,
            saved_samples: tune.samples,
            selected_samples: dwell.selected_samples,
            outcome,
        });
        Ok(())
    }
}

/// Verify NR synchronization in each eligible prefix before reporting results.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let plan = if args.raster.nr_raster {
        prepare_raster(
            &capture,
            &args.raster,
            args.max_samples_per_dwell,
            args.max_total_channelized_samples,
            stop,
        )
    } else {
        prepare(
            &capture,
            &args.profile,
            args.max_samples_per_dwell,
            args.max_total_channelized_samples,
            stop,
        )
    }?
    .with_pbch(args.nr_pbch);
    if args.plan_only {
        return plan.print(stop);
    }
    let report = plan.replay(stop)?;
    let output = root.join(args.output.context("supply --output for NR sweep replay")?);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Found {} repeated NR cell reports. Output: {}",
        report.cell_reports,
        output.display()
    );
    if let Some(count) = report.pbch_reports {
        println!("Validated {count} NR PBCH observations.");
    }
    Ok(())
}
