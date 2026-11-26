//! Measure saved IQ extent and host read gaps by band or frequency region.

mod frequency_map;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::SAMPLE_RATE;
use crate::private_output::publish_json_file;
use crate::spectrum::{CENTER_GUARD_HZ, USABLE_HALF_SPAN_HZ};
use crate::sweep::plan::{self, Band};
use crate::sweep::timing::{BoundTiming, HostRead};

use super::admission::{CompletedCapture, Series, Sweep, Tune, descriptor_path, open_directory};
use super::check_stop;
use frequency_map::FrequencyMap;

const SAMPLES_PER_MILLISECOND: u64 = SAMPLE_RATE as u64 / 1_000;
const NANOSECONDS_PER_SAMPLE: u64 = 1_000_000_000 / SAMPLE_RATE as u64;
const _: () =
    assert!(SAMPLE_RATE.is_multiple_of(1_000) && 1_000_000_000u32.is_multiple_of(SAMPLE_RATE));

/// Select frequencies and a minimum dwell for an offline listening-time report.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Band to measure. Repeat for up to 32 bands, or use the bands recorded by the sweep.
    #[arg(long = "band", conflicts_with = "frequency_map", value_name = "LOWER_HZ:UPPER_HZ", value_parser = Band::parse)]
    bands: Vec<Band>,
    /// Map listening time across the recorded contiguous sweep range.
    #[arg(long)]
    frequency_map: bool,
    /// Override the mapped range. Required for captures without a contiguous range.
    #[arg(long, requires = "frequency_map", value_name = "LOWER_HZ:UPPER_HZ", value_parser = Band::parse)]
    range_hz: Option<Band>,
    /// Minimum saved duration of a qualifying dwell in milliseconds.
    #[arg(long = "min-dwell-ms", default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=plan::MAX_DWELL_MS))]
    minimum_dwell_ms: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

struct Dwell<'a> {
    pass_index: usize,
    tune: &'a Tune,
    read: Option<&'a HostRead>,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_continuity_checked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_timing_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_read_window: Option<HostReadWindow>,
    pass_count: usize,
    dwell_count: usize,
    sample_rate_hz: u32,
    usable_half_span_hz: u64,
    center_guard_hz: u64,
    minimum_dwell_ms: u64,
    bands: Vec<BandReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_map: Option<FrequencyMap>,
}

#[derive(Default, Serialize)]
struct ListeningTime {
    covered_dwells: usize,
    qualifying_dwells: usize,
    covered_samples: u64,
    qualifying_samples: u64,
    covered_time_ns: u64,
    qualifying_time_ns: u64,
    longest_dwell_ns: u64,
    passes_with_coverage: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    longest_host_read_gap_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    covered_host_read_gaps: Option<HostReadGaps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualifying_host_read_gaps: Option<HostReadGaps>,
}

/// Offsets from the native session start to the first and last read boundaries.
#[derive(Serialize)]
struct HostReadWindow {
    started_ns: u64,
    finished_ns: u64,
}

#[derive(Serialize)]
struct HostReadGaps {
    #[serde(skip_serializing_if = "Option::is_none")]
    leading_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trailing_ns: Option<u64>,
    longest_ns: u64,
}

#[derive(Default)]
struct ReadSelection {
    first_started_ns: Option<u64>,
    last_finished_ns: Option<u64>,
    longest_between_ns: Option<u64>,
}

impl ReadSelection {
    fn include(&mut self, read: &HostRead) {
        self.first_started_ns.get_or_insert(read.started_ns);
        if let Some(previous_end) = self.last_finished_ns.replace(read.finished_ns) {
            let gap = read.started_ns - previous_end;
            self.longest_between_ns = Some(self.longest_between_ns.unwrap_or(0).max(gap));
        }
    }

    fn gaps(&self, window: &HostReadWindow) -> HostReadGaps {
        if let Some((first_start, last_end)) = self.first_started_ns.zip(self.last_finished_ns) {
            let leading_ns = first_start - window.started_ns;
            let trailing_ns = window.finished_ns - last_end;
            HostReadGaps {
                leading_ns: Some(leading_ns),
                trailing_ns: Some(trailing_ns),
                longest_ns: leading_ns
                    .max(trailing_ns)
                    .max(self.longest_between_ns.unwrap_or(0)),
            }
        } else {
            HostReadGaps {
                leading_ns: None,
                trailing_ns: None,
                longest_ns: window.finished_ns - window.started_ns,
            }
        }
    }
}

#[derive(Serialize)]
struct BandReport {
    #[serde(flatten)]
    band: Band,
    #[serde(flatten)]
    listening: ListeningTime,
}

impl ListeningTime {
    fn measure<'a>(
        dwells: impl IntoIterator<Item = &'a Dwell<'a>>,
        minimum_samples: u64,
        host_window: Option<&HostReadWindow>,
    ) -> Self {
        let mut time = Self::default();
        let mut last_pass = None;
        let mut covered_reads = ReadSelection::default();
        let mut qualifying_reads = ReadSelection::default();
        for dwell in dwells {
            let samples = dwell.tune.samples;
            time.covered_dwells += 1;
            time.covered_samples += samples;
            if samples >= minimum_samples {
                time.qualifying_dwells += 1;
                time.qualifying_samples += samples;
                if let Some(read) = dwell.read {
                    qualifying_reads.include(read);
                }
            }
            time.longest_dwell_ns = time.longest_dwell_ns.max(samples * NANOSECONDS_PER_SAMPLE);
            if last_pass != Some(dwell.pass_index) {
                time.passes_with_coverage += 1;
                last_pass = Some(dwell.pass_index);
            }
            if let Some(read) = dwell.read {
                covered_reads.include(read);
            }
        }
        time.covered_time_ns = time.covered_samples * NANOSECONDS_PER_SAMPLE;
        time.qualifying_time_ns = time.qualifying_samples * NANOSECONDS_PER_SAMPLE;
        time.longest_host_read_gap_ns = covered_reads.longest_between_ns;
        time.covered_host_read_gaps = host_window.map(|window| covered_reads.gaps(window));
        time.qualifying_host_read_gaps = host_window.map(|window| qualifying_reads.gaps(window));
        time
    }
}

/// Publish listening times for each complete band or mapped frequency region.
///
/// Durations come from validated IQ extents. Bound monotonic timing includes
/// gaps inside the first-to-last host read window when available. Raw IQ and
/// hardware are outside the measurement.
///
/// # Errors
/// Returns an error for inconsistent or changed source evidence, interruption,
/// invalid frequency selections, or publication failure.
pub(crate) fn run(root: &Path, mut args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    if !args.bands.is_empty() {
        plan::normalize_bands(&mut args.bands)?;
    }
    let directory = root.join(args.capture_directory).canonicalize()?;
    let capture = if args.series {
        CompletedCapture::Repeated(Series::open(&directory)?)
    } else {
        CompletedCapture::Single(Sweep::open(&directory)?)
    };
    let passes = capture.passes();
    let map_range = if args.frequency_map {
        let range = match args.range_hz {
            Some(range) => range,
            None => {
                let range = passes
                    .first()
                    .and_then(|pass| pass.sweep.requested_range.as_ref())
                    .context("supply --range-hz for a sweep without a contiguous range")?;
                Band::new(range.start, range.end)?
            }
        };
        Some(range)
    } else {
        None
    };
    let requested = if map_range.is_some() {
        &[]
    } else if args.bands.is_empty() {
        capture
            .requested_bands()
            .context("supply --band for a sweep without recorded band requests")?
    } else {
        &args.bands
    };
    let pinned_root = open_directory(&directory)?;
    capture.verify_pinned(&pinned_root)?;
    let tunes: Vec<_> = passes
        .iter()
        .enumerate()
        .flat_map(|(pass_index, pass)| pass.sweep.tunes.iter().map(move |tune| (pass_index, tune)))
        .collect();
    let timing = capture
        .timing_sha256()
        .map(|sha256| {
            let centers: Vec<_> = tunes
                .iter()
                .map(|(_, tune)| tune.requested_center_hz)
                .collect();
            BoundTiming::open(
                &descriptor_path(&pinned_root),
                sha256,
                &centers,
                passes.len(),
            )
        })
        .transpose()?;
    let host_read_window = timing.as_ref().and_then(|timing| {
        Some(HostReadWindow {
            started_ns: timing.reads().first()?.started_ns,
            finished_ns: timing.reads().last()?.finished_ns,
        })
    });
    let dwells: Vec<_> = tunes
        .into_iter()
        .enumerate()
        .map(|(index, (pass_index, tune))| Dwell {
            pass_index,
            tune,
            read: timing.as_ref().map(|timing| &timing.reads()[index]),
        })
        .collect();
    let minimum_samples = args.minimum_dwell_ms * SAMPLES_PER_MILLISECOND;
    let bands = requested
        .iter()
        .copied()
        .map(|band| BandReport {
            band,
            listening: ListeningTime::measure(
                dwells.iter().filter(|dwell| {
                    band.is_covered_by(plan::observed_bands(dwell.tune.observed_center_hz, 0))
                }),
                minimum_samples,
                host_read_window.as_ref(),
            ),
        })
        .collect();
    let frequency_map = map_range
        .map(|range| {
            FrequencyMap::measure(
                range,
                &dwells,
                minimum_samples,
                host_read_window.as_ref(),
                stop,
            )
        })
        .transpose()?;
    let report = Report {
        schema_version: 1,
        operation: "sweep_listening_coverage",
        capture_kind: if args.series { "series" } else { "sweep" },
        source_capture_sha256: capture.source_receipt_sha256(),
        sample_continuity_checked: capture.continuity_checked().then_some(true),
        source_timing_sha256: capture.timing_sha256(),
        host_read_window,
        pass_count: passes.len(),
        dwell_count: dwells.len(),
        sample_rate_hz: SAMPLE_RATE,
        usable_half_span_hz: USABLE_HALF_SPAN_HZ as u64,
        center_guard_hz: CENTER_GUARD_HZ as u64,
        minimum_dwell_ms: args.minimum_dwell_ms,
        bands,
        frequency_map,
    };
    let output = root.join(args.output);
    publish_json_file(&output, &report, || {
        capture.verify()?;
        if let Some(timing) = &timing {
            timing.verify()?;
        }
        check_stop(stop)
    })?;
    if let Some(map) = &report.frequency_map {
        println!(
            "Mapped {} frequency regions. Output: {}",
            map.region_count(),
            output.display()
        );
    } else {
        println!(
            "Measured {} bands. Output: {}",
            report.bands.len(),
            output.display()
        );
    }
    Ok(())
}
