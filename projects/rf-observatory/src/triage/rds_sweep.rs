//! Replay broadcast FM stations from individual dwells in an admitted capture.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::private_output::publish_json_file;
use crate::rds::{self, BankReport, MIN_SWEEP_SAMPLES};
use crate::{MAX_SAMPLES, SAMPLE_RATE};

use super::admission::{CompletedCapture, PassRef, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, Task};
use super::replay::{BudgetUnit, Work};

const MAX_STATIONS: usize = 32;
const MAX_RASTER_STATIONS: u64 = 512;
const MAX_GROUP_RECORDS: usize = 4_096;
/// Maximum original-rate samples charged across all RDS stations and dwells.
pub(super) const MAX_TOTAL_STATION_SAMPLES: u64 = 2_000_000_000;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const _: () = assert!(1_000_000_000u32.is_multiple_of(SAMPLE_RATE));

/// Select broadcast stations or a station raster and replay budgets for a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// FM station center in hertz. Repeat for up to 32 distinct stations.
    #[arg(long, required_unless_present = "raster", conflicts_with = "raster")]
    channel_hz: Vec<u64>,
    /// Inclusive FM station raster in hertz with up to 512 candidates.
    #[arg(long = "raster-hz", value_name = "LOWER:UPPER:STEP", value_parser = StationRaster::parse)]
    raster: Option<StationRaster>,
    /// Maximum original-rate samples to replay from each eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a plan above this sample count, charging the prefix once per eligible station.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_STATION_SAMPLES))]
    max_total_station_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// A finite grid of broadcast FM station centers.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StationRaster {
    lower_hz: u64,
    step_hz: u64,
    count: u64,
}

impl StationRaster {
    /// Parse inclusive endpoints and a positive step as `LOWER:UPPER:STEP` in hertz.
    ///
    /// # Errors
    /// Rejects malformed values, unordered or out-of-band endpoints, a zero
    /// step, an upper endpoint off the raster, or more than 512 candidates.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, rest) = value
            .split_once(':')
            .context("expected RDS raster LOWER:UPPER:STEP in hertz")?;
        let (upper, step) = rest
            .split_once(':')
            .context("expected RDS raster LOWER:UPPER:STEP in hertz")?;
        let lower_hz = lower
            .parse()
            .context("invalid RDS raster lower frequency")?;
        let upper_hz = upper
            .parse()
            .context("invalid RDS raster upper frequency")?;
        let step_hz = step.parse().context("invalid RDS raster step in hertz")?;
        ensure!(
            rds::is_fm_channel_hz(lower_hz) && rds::is_fm_channel_hz(upper_hz),
            "RDS raster endpoints must be in 64..=108 MHz"
        );
        ensure!(lower_hz <= upper_hz, "RDS raster endpoints must be ordered");
        ensure!(step_hz > 0, "RDS raster step must be positive");
        let span = upper_hz - lower_hz;
        ensure!(
            span.is_multiple_of(step_hz),
            "RDS raster upper endpoint must align with the step from the lower endpoint"
        );
        let count = span / step_hz + 1;
        ensure!(
            count <= MAX_RASTER_STATIONS,
            "RDS raster has {count} candidates, exceeding {MAX_RASTER_STATIONS}"
        );
        Ok(Self {
            lower_hz,
            step_hz,
            count,
        })
    }
}

/// Resolve either explicit FM stations or a finite raster in ascending order.
/// Returns an empty list when neither selector is present.
///
/// # Errors
/// Rejects conflicting selectors, out-of-band explicit stations, or more than
/// 32 distinct explicit stations.
pub(crate) fn select_stations(
    mut explicit: Vec<u64>,
    raster: Option<StationRaster>,
) -> Result<Vec<u64>> {
    if let Some(raster) = raster {
        ensure!(
            explicit.is_empty(),
            "explicit RDS stations and an RDS station raster are mutually exclusive"
        );
        return Ok((0..raster.count)
            .map(|index| raster.lower_hz + index * raster.step_hz)
            .collect());
    }
    explicit.sort_unstable();
    explicit.dedup();
    ensure!(
        explicit.len() <= MAX_STATIONS,
        "select 1 through {MAX_STATIONS} distinct RDS stations"
    );
    for &frequency in &explicit {
        ensure!(
            rds::is_fm_channel_hz(frequency),
            "RDS station center {frequency} Hz must be in 64..=108 MHz"
        );
    }
    Ok(explicit)
}

#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    sample_rate_hz: u32,
    minimum_group_samples: u64,
    max_samples_per_dwell: u64,
    max_total_station_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    station_samples: u64,
    validated_groups: usize,
    omitted_valid_groups: usize,
    stations: Vec<Coverage>,
    dwells: Vec<Attempt<'a>>,
}

impl Report<'_> {
    /// Number of validated groups, including those beyond report capacity.
    pub(super) fn validated_groups(&self) -> usize {
        self.validated_groups
    }
}

#[derive(Serialize)]
struct Coverage {
    channel_frequency_hz: u64,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    covered_samples: u64,
    replayed_samples: u64,
    covered_time_ns: u64,
    replayed_time_ns: u64,
}

impl Coverage {
    fn new(channel_frequency_hz: u64) -> Self {
        Self {
            channel_frequency_hz,
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
    channel_frequencies_hz: Vec<u64>,
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
    channel_frequencies_hz: Vec<u64>,
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
        let mut channel_frequencies_hz = Vec::new();
        for station in coverage {
            if rds::is_covered(tune.observed_center_hz, station.channel_frequency_hz) {
                station.record(tune.samples, selected_samples);
                channel_frequencies_hz.push(station.channel_frequency_hz);
            }
        }
        if channel_frequencies_hz.is_empty() {
            return None;
        }
        Some(Self {
            pass_index,
            pass_directory: pass.directory,
            source_sweep_sha256: &pass.sweep.source_receipt_sha256,
            tune,
            selected_samples,
            channel_frequencies_hz,
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

    fn attempt(
        self,
        input: &mut dyn Read,
        remaining_groups: usize,
        stop: &AtomicBool,
    ) -> Result<Attempt<'a>> {
        check_stop(stop)?;
        let outcome = if self.is_short() {
            Outcome::ShortDwell
        } else {
            let replay = rds::replay(
                input,
                self.tune.observed_center_hz,
                self.selected_samples,
                &self.channel_frequencies_hz,
                remaining_groups,
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
            channel_frequencies_hz: self.channel_frequencies_hz,
            outcome,
        })
    }
}

/// Publish provenance-bound RDS evidence from a completed capture.
///
/// # Errors
/// Returns an error for invalid stations, an invalid or changed capture,
/// an excessive replay plan, invalid IQ, interruption, or publication failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let frequencies = select_stations(args.channel_hz, args.raster)?;
    let capture = CompletedCapture::open(&root.join(args.capture_directory), args.series)?;
    let report = prepare(
        &capture,
        frequencies,
        args.max_samples_per_dwell,
        args.max_total_station_samples,
        stop,
    )?
    .replay(stop)?;
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} RDS groups across {} replayed dwells. Output: {}",
        report.validated_groups(),
        report.replayed_dwells,
        output.display()
    );
    Ok(())
}

/// Prepare RDS batches for the resolved stations without reading capture IQ.
/// Supply frequencies from [select_stations].
///
/// # Errors
/// Returns an error for work beyond the replay budget or interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    frequencies: Vec<u64>,
    max_samples_per_dwell: u64,
    max_total_station_samples: u64,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let passes = capture.passes();
    let dwell_count = passes.iter().map(|pass| pass.sweep.tunes.len()).sum();
    let mut stations: Vec<_> = frequencies.into_iter().map(Coverage::new).collect();
    let mut routes = Vec::new();
    let mut replayed_dwells = 0;
    let mut replayed_iq_samples = 0;
    for (index, pass) in passes.iter().enumerate() {
        for tune in &pass.sweep.tunes {
            if let Some(route) =
                Route::plan(index + 1, pass, tune, max_samples_per_dwell, &mut stations)
            {
                if !route.is_short() {
                    replayed_dwells += 1;
                    replayed_iq_samples += route.selected_samples;
                }
                routes.push(route);
            }
        }
    }
    let station_samples = stations
        .iter()
        .map(|station| station.replayed_samples)
        .sum();
    ensure!(
        station_samples <= max_total_station_samples,
        "RDS sweep replay needs {station_samples} station samples, exceeding --max-total-station-samples ({max_total_station_samples})"
    );

    Ok(Plan {
        capture,
        max_samples_per_dwell,
        max_total_station_samples,
        pass_count: passes.len(),
        dwell_count,
        replayed_dwells,
        replayed_iq_samples,
        station_samples,
        stations,
        dwells: Vec::with_capacity(routes.len()),
        routes: routes.into_iter(),
        remaining_groups: MAX_GROUP_RECORDS,
    })
}

/// RDS replay work and the admitted capture it references.
pub(super) struct Plan<'a> {
    capture: &'a CompletedCapture,
    max_samples_per_dwell: u64,
    max_total_station_samples: u64,
    pass_count: usize,
    dwell_count: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    station_samples: u64,
    stations: Vec<Coverage>,
    routes: std::vec::IntoIter<Route<'a>>,
    dwells: Vec<Attempt<'a>>,
    remaining_groups: usize,
}

impl<'a> Plan<'a> {
    /// Return the source-prefix work and station attempts selected by this plan.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.replayed_iq_samples,
            eligible_attempts: self
                .stations
                .iter()
                .map(|station| station.replayed_dwells)
                .sum(),
            short_attempts: self
                .stations
                .iter()
                .map(|station| station.short_dwells)
                .sum(),
            budget_samples: self.station_samples,
            max_budget_samples: self.max_total_station_samples,
            budget_unit: BudgetUnit::Station,
        }
    }

    /// Iterate the remaining dwell requests in pass order.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.as_slice().iter().map(Route::request)
    }

    /// Read and hash each eligible prefix once. Group and text assembly start
    /// fresh for every dwell. Recheck the capture before publishing the report.
    /// # Errors
    /// Returns an error for invalid or changed IQ, or interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        check_stop(stop)?;
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.finish())
    }

    /// Return the report from completed dwell requests.
    fn finish(self) -> Report<'a> {
        let covered_dwells = self.dwells.len();
        let validated_groups = self
            .dwells
            .iter()
            .map(|attempt| match &attempt.outcome {
                Outcome::ShortDwell => 0,
                Outcome::Replayed { replay } => replay.validated_groups(),
            })
            .sum::<usize>();
        Report {
            schema_version: 1,
            operation: "identify_rds_in_saved_sweeps",
            capture_kind: self.capture.kind(),
            source_capture_sha256: self.capture.source_receipt_sha256(),
            sample_rate_hz: SAMPLE_RATE,
            minimum_group_samples: MIN_SWEEP_SAMPLES,
            max_samples_per_dwell: self.max_samples_per_dwell,
            max_total_station_samples: self.max_total_station_samples,
            pass_count: self.pass_count,
            dwell_count: self.dwell_count,
            covered_dwells,
            short_dwells: covered_dwells - self.replayed_dwells,
            replayed_dwells: self.replayed_dwells,
            replayed_iq_samples: self.replayed_iq_samples,
            station_samples: self.station_samples,
            validated_groups,
            omitted_valid_groups: validated_groups - (MAX_GROUP_RECORDS - self.remaining_groups),
            stations: self.stations,
            dwells: self.dwells,
        }
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.requests().next()
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        let route = self.routes.next().context("no pending RDS dwell")?;
        let pass_index = route.pass_index;
        let tune_directory = &route.tune.directory;
        let attempt = route
            .attempt(input, self.remaining_groups, stop)
            .with_context(|| format!("replay RDS for pass {pass_index}/{tune_directory}"))?;
        if let Outcome::Replayed { replay } = &attempt.outcome {
            self.remaining_groups -= replay.retained_groups();
        }
        self.dwells.push(attempt);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_raster_accepts_the_largest_positive_step() {
        let raster = StationRaster::parse("108000000:108000000:18446744073709551615").unwrap();
        assert_eq!(
            select_stations(Vec::new(), Some(raster)).unwrap(),
            vec![108_000_000]
        );
    }
}
