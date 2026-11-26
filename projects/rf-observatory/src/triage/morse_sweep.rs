//! Replay labeled Morse profiles from individual dwells in an admitted capture.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::morse::{self, BankReport, MIN_SWEEP_SAMPLES, Profile};
use crate::private_output::publish_json_file;
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, PassRef, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const MAX_PROFILES: usize = 32;
/// Maximum original-rate samples charged across all Morse profiles and dwells.
pub(super) const MAX_TOTAL_PROFILE_SAMPLES: u64 = 2_000_000_000;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const _: () = assert!(1_000_000_000u32.is_multiple_of(SAMPLE_RATE));

/// Select Morse profiles and replay budgets for a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// CW carrier or NFM tone profile. Repeat for up to 32 distinct profiles.
    #[arg(
        long,
        required = true,
        value_name = "cw:CHANNEL_HZ[:WPM] or nfm:CHANNEL_HZ:TONE_HZ[:WPM]",
        value_parser = Profile::parse
    )]
    profile: Vec<Profile>,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this sample count, charging the prefix once per eligible profile.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_PROFILE_SAMPLES))]
    max_total_profile_samples: u64,
    /// New private JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
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
    max_total_profile_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    profile_samples: u64,
    resolved_characters: usize,
    profiles: Vec<Coverage>,
    dwells: Vec<Attempt<'a>>,
}

impl Report<'_> {
    /// Number of resolved character occurrences across replayed profiles.
    pub(super) fn resolved_characters(&self) -> usize {
        self.resolved_characters
    }
}

#[derive(Serialize)]
struct Coverage {
    #[serde(flatten)]
    profile: Profile,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    covered_samples: u64,
    replayed_samples: u64,
    covered_time_ns: u64,
    replayed_time_ns: u64,
}

impl Coverage {
    fn new(profile: Profile) -> Self {
        Self {
            profile,
            covered_dwells: 0,
            short_dwells: 0,
            replayed_dwells: 0,
            covered_samples: 0,
            replayed_samples: 0,
            covered_time_ns: 0,
            replayed_time_ns: 0,
        }
    }

    fn record(&mut self, saved_samples: u64, selected_samples: u64) {
        self.covered_dwells += 1;
        self.covered_samples += saved_samples;
        self.covered_time_ns += saved_samples * NANOSECONDS_PER_SAMPLE;
        if selected_samples < MIN_SWEEP_SAMPLES {
            self.short_dwells += 1;
        } else {
            self.replayed_dwells += 1;
            self.replayed_samples += selected_samples;
            self.replayed_time_ns += selected_samples * NANOSECONDS_PER_SAMPLE;
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
    profiles: Vec<Profile>,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    ShortDwell,
    Replayed { replay: BankReport },
}

struct Route<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
    profiles: Vec<Profile>,
}

impl<'a> Route<'a> {
    fn plan(
        pass_index: usize,
        pass: &PassRef<'a>,
        tune: &'a Tune,
        max_samples_per_dwell: u64,
        coverage: &mut [Coverage],
    ) -> Option<Self> {
        let selected_samples = tune.samples.min(max_samples_per_dwell);
        let mut profiles = Vec::new();
        for summary in coverage {
            if summary.profile.is_covered(tune.observed_center_hz) {
                summary.record(tune.samples, selected_samples);
                profiles.push(summary.profile);
            }
        }
        if profiles.is_empty() {
            return None;
        }
        Some(Self {
            pass_index,
            pass_directory: pass.directory,
            source_sweep_sha256: &pass.sweep.source_receipt_sha256,
            tune,
            selected_samples,
            profiles,
        })
    }

    fn is_short(&self) -> bool {
        self.selected_samples < MIN_SWEEP_SAMPLES
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.is_short() {
                0
            } else {
                self.selected_samples
            },
        }
    }

    fn attempt(self, input: &mut dyn Read, stop: &AtomicBool) -> Result<Attempt<'a>> {
        check_stop(stop)?;
        let outcome = if self.is_short() {
            Outcome::ShortDwell
        } else {
            let replay = morse::replay(
                input,
                self.tune.observed_center_hz,
                self.selected_samples,
                &self.profiles,
                stop,
            )?;
            Outcome::Replayed { replay }
        };
        Ok(Attempt {
            pass_index: self.pass_index,
            pass_directory: self.pass_directory,
            tune_directory: &self.tune.directory,
            source_sweep_sha256: self.source_sweep_sha256,
            source_sidecar_sha256: &self.tune.sidecar_sha256,
            observed_center_hz: self.tune.observed_center_hz,
            saved_samples: self.tune.samples,
            selected_samples: self.selected_samples,
            profiles: self.profiles,
            outcome,
        })
    }
}

/// Publish source-bound Morse evidence from a completed capture.
/// # Errors
/// Returns an error for invalid profiles, an invalid or changed source,
/// an excessive replay plan, invalid IQ, interruption, or publication failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let profiles = distinct_profiles(args.profile)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let report = prepare_profiles(
        &capture,
        profiles,
        args.max_samples_per_dwell,
        args.max_total_profile_samples,
        stop,
    )?
    .replay(stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Resolved {} Morse characters across {} replayed dwells. Output: {}",
        report.resolved_characters(),
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}

/// Route selected Morse profiles and check their budget before reading IQ.
///
/// # Errors
/// Returns an error for invalid profile counts, an over-budget plan, or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    profiles: &[Profile],
    max_samples_per_dwell: u64,
    max_total_profile_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    prepare_profiles(
        capture,
        distinct_profiles(profiles.to_vec())?,
        max_samples_per_dwell,
        max_total_profile_samples,
        stop,
    )
}

pub(crate) fn distinct_profiles(mut profiles: Vec<Profile>) -> Result<Vec<Profile>> {
    profiles.sort_unstable();
    profiles.dedup();
    ensure!(
        (1..=MAX_PROFILES).contains(&profiles.len()),
        "select 1 through {MAX_PROFILES} distinct Morse profiles"
    );
    Ok(profiles)
}

/// Eligible Morse profiles, including covered prefixes that are too short to replay.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    pass_count: usize,
    dwell_count: usize,
    work: Work,
    profiles: Vec<Coverage>,
    routes: std::vec::IntoIter<Route<'a>>,
    dwells: Vec<Attempt<'a>>,
}

fn prepare_profiles<'a>(
    capture: &'a CompletedCapture,
    profiles: Vec<Profile>,
    max_samples_per_dwell: u64,
    max_total_profile_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    let passes = capture.passes();
    let dwell_count = passes.iter().map(|pass| pass.sweep.tunes.len()).sum();
    let mut profiles: Vec<_> = profiles.into_iter().map(Coverage::new).collect();
    let mut routes = Vec::new();
    for (index, pass) in passes.iter().enumerate() {
        for tune in &pass.sweep.tunes {
            check_stop(stop)?;
            if let Some(route) =
                Route::plan(index + 1, pass, tune, max_samples_per_dwell, &mut profiles)
            {
                routes.push(route);
            }
        }
    }
    let profile_samples = profiles
        .iter()
        .map(|profile| profile.replayed_samples)
        .sum();
    ensure!(
        profile_samples <= max_total_profile_samples,
        "Morse sweep replay needs {profile_samples} profile samples, exceeding --max-total-profile-samples ({max_total_profile_samples})"
    );

    let work = Work {
        replayed_iq_samples: routes
            .iter()
            .filter(|route| !route.is_short())
            .map(|route| route.selected_samples)
            .sum(),
        eligible_attempts: profiles.iter().map(|profile| profile.replayed_dwells).sum(),
        short_attempts: profiles.iter().map(|profile| profile.short_dwells).sum(),
        budget_samples: profile_samples,
        max_budget_samples: max_total_profile_samples,
        budget_unit: BudgetUnit::Profile,
    };
    Ok(Plan {
        capture,
        max_samples_per_dwell,
        pass_count: passes.len(),
        dwell_count,
        work,
        profiles,
        dwells: Vec::with_capacity(routes.len()),
        routes: routes.into_iter(),
    })
}

impl<'a> Plan<'a> {
    /// Return the accepted IQ extent and profile attempt counts.
    pub(super) fn work(&self) -> Work {
        self.work
    }

    /// Iterate the remaining dwell requests in pass order.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.as_slice().iter().map(Route::request)
    }

    /// Read each eligible prefix once, with fresh envelope and timing state.
    /// Recheck the capture when publishing the report.
    ///
    /// # Errors
    /// Returns an error for invalid or changed IQ or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Return the report from completed dwell requests.
    fn finish(self) -> Report<'a> {
        let covered_dwells = self.dwells.len();
        let mut replayed_dwells = 0;
        let mut resolved_characters = 0;
        for attempt in &self.dwells {
            if let Outcome::Replayed { replay } = &attempt.outcome {
                replayed_dwells += 1;
                resolved_characters += replay.resolved_characters();
            }
        }
        Report {
            schema_version: 1,
            operation: "identify_morse_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            minimum_dwell_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_profile_samples: self.work.max_budget_samples,
            pass_count: self.pass_count,
            dwell_count: self.dwell_count,
            covered_dwells,
            short_dwells: covered_dwells - replayed_dwells,
            replayed_dwells,
            replayed_iq_samples: self.work.replayed_iq_samples,
            profile_samples: self.work.budget_samples,
            resolved_characters,
            profiles: self.profiles,
            dwells: self.dwells,
        }
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.requests().next()
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        let route = self.routes.next().context("no pending Morse dwell")?;
        let pass_index = route.pass_index;
        let tune_directory = &route.tune.directory;
        let attempt = route
            .attempt(input, stop)
            .with_context(|| format!("replay Morse for pass {pass_index}/{tune_directory}"))?;
        self.dwells.push(attempt);
        Ok(())
    }
}
