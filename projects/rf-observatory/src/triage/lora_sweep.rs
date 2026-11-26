//! Measure LoRa preamble evidence across eligible profiles in saved sweeps.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::lora::MAX_DETECTIONS;
use crate::lora::bank::{self, BankReport, Profile};
use crate::private_output::publish_json_file;
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, PassRef, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const MAX_PROFILES: usize = 32;
/// Maximum original-rate samples charged across all LoRa profiles and dwells.
pub(super) const MAX_TOTAL_PROFILE_SAMPLES: u64 = 2_000_000_000;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const _: () = assert!(1_000_000_000u32.is_multiple_of(SAMPLE_RATE));

/// Select chirp hypotheses and replay budgets for a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Profile as CENTER_HZ:BANDWIDTH_HZ:SF. Repeat for up to 32 distinct profiles.
    #[arg(long, required = true, value_name = "CENTER_HZ:BANDWIDTH_HZ:SF", value_parser = Profile::parse)]
    profile: Vec<Profile>,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this sample count, charging the prefix once per eligible profile.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_PROFILE_SAMPLES))]
    max_total_profile_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Coverage {
    #[serde(flatten)]
    profile: Profile,
    minimum_samples: u64,
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
            minimum_samples: profile.minimum_samples(),
            covered_dwells: 0,
            short_dwells: 0,
            replayed_dwells: 0,
            covered_samples: 0,
            replayed_samples: 0,
            covered_time_ns: 0,
            replayed_time_ns: 0,
        }
    }

    fn record(&mut self, saved_samples: u64, selected_samples: u64) -> bool {
        self.covered_dwells += 1;
        self.covered_samples += saved_samples;
        self.covered_time_ns += saved_samples * NANOSECONDS_PER_SAMPLE;
        if selected_samples < self.minimum_samples {
            self.short_dwells += 1;
            return false;
        }
        self.replayed_dwells += 1;
        self.replayed_samples += selected_samples;
        self.replayed_time_ns += selected_samples * NANOSECONDS_PER_SAMPLE;
        true
    }
}

#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    sample_rate_hz: u32,
    max_samples_per_dwell: u64,
    max_total_profile_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    profile_samples: u64,
    /// Preamble candidate occurrences, including those beyond report capacity.
    pub(super) candidate_count: usize,
    omitted_candidates: usize,
    profiles: Vec<Coverage>,
    dwells: Vec<Attempt<'a>>,
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
    short_profiles: Vec<Profile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay: Option<BankReport>,
}

struct Route<'a> {
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
    profiles: Vec<Profile>,
    short_profiles: Vec<Profile>,
}

impl<'a> Route<'a> {
    fn plan(
        pass_index: usize,
        pass: &PassRef<'a>,
        tune: &'a Tune,
        max_samples: u64,
        coverage: &mut [Coverage],
    ) -> Option<Self> {
        let selected_samples = tune.samples.min(max_samples);
        let mut profiles = Vec::new();
        let mut short_profiles = Vec::new();
        for summary in coverage {
            if summary.profile.is_covered(tune.observed_center_hz) {
                if summary.record(tune.samples, selected_samples) {
                    profiles.push(summary.profile);
                } else {
                    short_profiles.push(summary.profile);
                }
            }
        }
        if profiles.is_empty() && short_profiles.is_empty() {
            return None;
        }
        Some(Self {
            pass_index,
            pass_directory: pass.directory,
            source_sweep_sha256: &pass.sweep.source_receipt_sha256,
            tune,
            selected_samples,
            profiles,
            short_profiles,
        })
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.profiles.is_empty() {
                0
            } else {
                self.selected_samples
            },
        }
    }

    fn attempt(
        self,
        input: &mut dyn Read,
        remaining_candidates: usize,
        stop: &AtomicBool,
    ) -> Result<Attempt<'a>> {
        check_stop(stop)?;
        let replay = if self.profiles.is_empty() {
            None
        } else {
            Some(bank::replay(
                input,
                self.tune.observed_center_hz,
                self.selected_samples,
                &self.profiles,
                remaining_candidates,
                stop,
            )?)
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
            short_profiles: self.short_profiles,
            replay,
        })
    }
}

/// Replay eligible profiles and publish source-bound LoRa preamble evidence.
///
/// # Errors
/// Returns an error for invalid or changed inputs, an over-budget plan,
/// invalid IQ, interruption, or publication failure.
pub(crate) fn probe(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let profiles = distinct_profiles(args.profile)?;
    let directory = root.join(args.capture_directory);
    let capture = CompletedCapture::open(&directory, args.series)?;
    let report = prepare_profiles(
        &capture,
        profiles,
        args.max_samples_per_dwell,
        args.max_total_profile_samples,
    )?
    .replay(stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Found {} LoRa preamble candidates across {} replayed dwells. Output: {}",
        report.candidate_count,
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}

/// Route selected LoRa preamble profiles and check their budget before reading IQ.
///
/// # Errors
/// Returns an error for too many profiles, an over-budget plan, or interruption.
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
        distinct_profiles(profiles.iter().copied())?,
        max_samples_per_dwell,
        max_total_profile_samples,
    )
}

pub(crate) fn distinct_profiles(
    profiles: impl IntoIterator<Item = Profile>,
) -> Result<BTreeSet<Profile>> {
    let profiles: BTreeSet<_> = profiles.into_iter().collect();
    ensure!(
        profiles.len() <= MAX_PROFILES,
        "select up to {MAX_PROFILES} distinct LoRa profiles"
    );
    Ok(profiles)
}

/// Eligible LoRa profiles, including covered prefixes that are too short to replay.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    pass_count: usize,
    dwell_count: usize,
    work: Work,
    profiles: Vec<Coverage>,
    routes: std::vec::IntoIter<Route<'a>>,
    dwells: Vec<Attempt<'a>>,
    remaining_candidates: usize,
}

fn prepare_profiles<'a>(
    capture: &'a CompletedCapture,
    profiles: BTreeSet<Profile>,
    max_samples_per_dwell: u64,
    max_total_profile_samples: u64,
) -> Result<Plan<'a>> {
    let passes = capture.passes();
    let dwell_count = passes.iter().map(|pass| pass.sweep.tunes.len()).sum();
    let mut profiles: Vec<_> = profiles.into_iter().map(Coverage::new).collect();
    let mut routes = Vec::new();
    for (index, pass) in passes.iter().enumerate() {
        for tune in &pass.sweep.tunes {
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
        .sum::<u64>();
    ensure!(
        profile_samples <= max_total_profile_samples,
        "LoRa sweep probe needs {profile_samples} profile samples, exceeding --max-total-profile-samples ({max_total_profile_samples})"
    );

    let work = Work {
        replayed_iq_samples: routes
            .iter()
            .filter(|route| !route.profiles.is_empty())
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
        remaining_candidates: MAX_DETECTIONS,
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

    /// Read only the planned prefixes and return evidence for publication.
    /// Reverify the capture receipts when publishing the report.
    ///
    /// # Errors
    /// Returns an error for invalid or changed IQ or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Return the report from completed dwell requests.
    fn finish(self) -> Report<'a> {
        let mut replayed_dwells = 0;
        let mut candidate_count = 0;
        for replay in self
            .dwells
            .iter()
            .filter_map(|attempt| attempt.replay.as_ref())
        {
            replayed_dwells += 1;
            candidate_count += replay.candidate_count();
        }
        Report {
            schema_version: 1,
            operation: "probe_lora_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_profile_samples: self.work.max_budget_samples,
            pass_count: self.pass_count,
            dwell_count: self.dwell_count,
            replayed_dwells,
            replayed_iq_samples: self.work.replayed_iq_samples,
            profile_samples: self.work.budget_samples,
            candidate_count,
            omitted_candidates: candidate_count - (MAX_DETECTIONS - self.remaining_candidates),
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
        let route = self.routes.next().context("no pending LoRa dwell")?;
        let pass_index = route.pass_index;
        let tune_directory = &route.tune.directory;
        let attempt = route
            .attempt(input, self.remaining_candidates, stop)
            .with_context(|| format!("probe LoRa for pass {pass_index}/{tune_directory}"))?;
        if let Some(replay) = &attempt.replay {
            self.remaining_candidates -= replay.retained_candidates();
        }
        self.dwells.push(attempt);
        Ok(())
    }
}
