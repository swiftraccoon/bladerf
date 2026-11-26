//! Locate transient activity and preserve bounded evidence from saved sweeps.

mod admission;
pub(crate) mod adsb_sweep;
pub(crate) mod aprs_sweep;
pub(crate) mod channel_sweep;
pub(crate) mod coverage;
pub(crate) mod dmr_raster;
pub(crate) mod follow_up;
pub(crate) mod lora_decode_sweep;
pub(crate) mod lora_sweep;
pub(crate) mod lte_sweep;
pub(crate) mod morse_sweep;
pub(crate) mod nr_sweep;
pub(crate) mod ook_sweep;
pub(crate) mod rds_sweep;
pub(crate) mod replay;
pub(crate) mod rs41_raster;
pub(crate) mod sampling;
mod selection;
pub(crate) mod sensor_sweep;
pub(crate) mod series;

use std::fs::File;
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use clap::Args as ClapArgs;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::private_output::StagedDirectory;
use crate::spectrum::{Event, EventStride, EventWindows, FFT};
use crate::sweep::{CompletedOperation, plan};
use crate::waveform::{self, CLIP_CONTEXT_SAMPLES, MAX_CLIP_SAMPLES, MIN_INPUT_INTERVAL};

use self::admission::{Sweep, Tune};
use self::sampling::{IntervalEvidence, MAX_TIME_SLICES};
use self::selection::{Clips, Detector};

/// Maximum number of source samples read for analysis per tune.
pub(crate) const MAX_SAMPLES_PER_TUNE: u64 = 4_000_000;
/// Maximum number of source samples read for analysis across the capture.
pub(crate) const MAX_TOTAL_SAMPLES: u64 = 200_000_000;
pub(crate) const MAX_NARROW_WIDTH_HZ: u64 = 400_000;
/// Keep a narrow waveform measurement clear of the acquisition center.
pub(crate) const MIN_NARROW_CENTER_OFFSET_HZ: u64 = 500_000;
const COMPLEX_BYTES: u64 = 4;

#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Directory containing a completed sweep.json and its tune directories.
    #[arg(long)]
    sweep_directory: PathBuf,
    /// Fresh directory for the report and its original-rate IQ clips.
    #[arg(long)]
    output_directory: PathBuf,
    /// Maximum number of evidence clips to retain across all tunes.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=32))]
    max_clips: u32,
    /// Slots reserved within the total for narrow contrast candidates, leaving one slot for general activity.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u32).range(0..=31))]
    narrow_clips: u32,
    /// Maximum samples per tune, rounded down to complete FFT windows.
    #[arg(long, default_value_t = MAX_SAMPLES_PER_TUNE, value_parser = clap::value_parser!(u64).range(FFT as u64..=MAX_SAMPLES_PER_TUNE))]
    max_samples_per_tune: u64,
    /// Shared analysis budget, divided equally among tunes.
    #[arg(long, default_value_t = MAX_TOTAL_SAMPLES, value_parser = clap::value_parser!(u64).range(FFT as u64..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Maximum number of intervals spaced across each saved dwell.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_TIME_SLICES)))]
    time_slices: u32,
    /// Use 50% overlapping FFT windows for event detection within each analyzed interval.
    #[arg(long)]
    overlap_events: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    source_operation: CompletedOperation,
    source_receipt_sha256: &'a str,
    requested_range: &'a Option<Range<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visit_centers_hz: Option<&'a [u64]>,
    sample_format: &'static str,
    sample_rate_hz: u32,
    fft_samples: usize,
    max_total_samples: u64,
    max_samples_per_tune: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_slices: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_window_stride_samples: Option<usize>,
    max_clips: u32,
    max_narrow_clips: usize,
    tunes: Vec<TuneReport<'a>>,
    clips: Vec<ClipReport<'a>>,
}

#[derive(Serialize)]
struct TuneReport<'a> {
    directory: &'a str,
    requested_center_hz: u64,
    observed_center_hz: u64,
    observed_bands: [plan::Band; 2],
    source_sidecar_sha256: &'a str,
    saved_samples: u64,
    analyzed_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    analyzed_iq_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    analyzed_intervals: Option<Vec<IntervalEvidence>>,
    event_count: usize,
    detector_omitted_event_count: usize,
    eligible_retained_event_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_search: Option<NarrowCounts>,
}

#[derive(Default, Serialize)]
struct NarrowCounts {
    candidate_count: usize,
    detector_omitted_count: usize,
    eligible_retained_count: usize,
}

struct PendingClip<'a> {
    tune: &'a Tune,
    event: Event,
    detector: Detector,
    bounds: ClipBounds,
    bytes: Vec<u8>,
}

struct ClipBounds {
    interval: Range<u64>,
    waveform_interval: Range<u64>,
}

#[derive(Serialize)]
struct ClipReport<'a> {
    tune_directory: &'a str,
    file: String,
    start_sample: u64,
    end_sample_exclusive: u64,
    lower_hz: u64,
    upper_hz: u64,
    peak_window_start_sample: u64,
    event_start_sample: u64,
    event_end_sample_exclusive: u64,
    peak_excess_db: f64,
    detector: Detector,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrow_contrast_db: Option<f64>,
    sha256: String,
    waveform: Waveform,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Waveform {
    Measured {
        target_hz: u64,
        clip_start_sample: usize,
        clip_end_sample_exclusive: usize,
        evidence: Box<waveform::NarrowEvidence>,
    },
    Skipped {
        reason: &'static str,
    },
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "sweep triage interrupted");
    Ok(())
}

impl ClipBounds {
    fn new(event: &Event, analyzed: &Range<u64>) -> Self {
        let padding = CLIP_CONTEXT_SAMPLES as u64;
        let max_interval = MAX_CLIP_SAMPLES as u64 - 2 * padding;
        let mut waveform_interval = event.start_sample..event.end_sample_exclusive;
        if waveform_interval.end - waveform_interval.start > max_interval {
            let peak_center = event.peak_window_start_sample + FFT as u64 / 2;
            let start = peak_center.saturating_sub(max_interval / 2).clamp(
                waveform_interval.start,
                waveform_interval.end - max_interval,
            );
            waveform_interval = start..start + max_interval;
        }
        let interval = waveform_interval
            .start
            .saturating_sub(padding)
            .max(analyzed.start)
            ..(waveform_interval.end + padding).min(analyzed.end);
        Self {
            interval,
            waveform_interval,
        }
    }

    fn probe_parameters(
        &self,
        event: &Event,
        center_hz: u64,
    ) -> std::result::Result<(u64, Range<usize>), &'static str> {
        if event.upper_frequency_hz_exclusive - event.lower_frequency_hz > MAX_NARROW_WIDTH_HZ {
            return Err("event_wider_than_narrow_probe");
        }
        let start = (self.waveform_interval.start - self.interval.start) as usize;
        let end = (self.waveform_interval.end - self.interval.start) as usize;
        let samples = (self.interval.end - self.interval.start) as usize;
        if end - start < MIN_INPUT_INTERVAL
            || start < CLIP_CONTEXT_SAMPLES
            || samples - end < CLIP_CONTEXT_SAMPLES
        {
            return Err("insufficient_saved_filter_context_or_interval");
        }
        let target_hz = event
            .lower_frequency_hz
            .midpoint(event.upper_frequency_hz_exclusive);
        if target_hz.abs_diff(center_hz) < MIN_NARROW_CENTER_OFFSET_HZ {
            return Err("probe_passband_near_tune_center");
        }
        Ok((target_hz, start..end))
    }
}

impl<'a> PendingClip<'a> {
    fn read(
        tune: &'a Tune,
        event: Event,
        detector: Detector,
        bounds: ClipBounds,
        file: &File,
    ) -> Result<Self> {
        let span = &bounds.interval;
        let mut bytes = vec![0; ((span.end - span.start) * COMPLEX_BYTES) as usize];
        file.read_exact_at(&mut bytes, span.start * COMPLEX_BYTES)
            .context("read selected IQ interval from the analyzed recording")?;
        Ok(Self {
            tune,
            event,
            detector,
            bounds,
            bytes,
        })
    }

    fn waveform(&self, path: &Path, sha256: &str, stop: &AtomicBool) -> Result<Waveform> {
        let center_hz = self.tune.observed_center_hz;
        let (target_hz, interval) = match self.bounds.probe_parameters(&self.event, center_hz) {
            Ok(parameters) => parameters,
            Err(reason) => return Ok(Waveform::Skipped { reason }),
        };
        let evidence = waveform::measure_narrow(
            path,
            center_hz,
            target_hz,
            interval.clone(),
            Some(sha256),
            stop,
        )?;
        Ok(Waveform::Measured {
            target_hz,
            clip_start_sample: interval.start,
            clip_end_sample_exclusive: interval.end,
            evidence: Box::new(evidence),
        })
    }

    fn write(self, directory: &Path, index: usize, stop: &AtomicBool) -> Result<ClipReport<'a>> {
        check_stop(stop)?;
        let file = format!("clip-{:04}.sc16q11", index + 1);
        let path = directory.join(&file);
        let mut output = crate::new_file(&path)?;
        output.write_all(&self.bytes).context("write IQ clip")?;
        output.sync_all().context("sync IQ clip")?;
        let span = &self.bounds.interval;
        crate::receiver::write_derived_iq_sidecar(
            &path,
            self.tune.observed_center_hz,
            "derived_sweep_triage_clip",
            span.start,
            span.end - span.start,
        )?;
        let sha256 = format!("{:x}", Sha256::digest(&self.bytes));
        let waveform = self.waveform(&path, &sha256, stop)?;
        Ok(ClipReport {
            tune_directory: &self.tune.directory,
            file,
            start_sample: span.start,
            end_sample_exclusive: span.end,
            lower_hz: self.event.lower_frequency_hz,
            upper_hz: self.event.upper_frequency_hz_exclusive,
            peak_window_start_sample: self.event.peak_window_start_sample,
            event_start_sample: self.event.start_sample,
            event_end_sample_exclusive: self.event.end_sample_exclusive,
            peak_excess_db: self.event.peak_excess_db,
            detector: self.detector,
            narrow_contrast_db: self.event.narrow_contrast_db,
            sha256,
            waveform,
        })
    }
}

fn analyze_tune<'a>(
    tune: &'a Tune,
    requested: Option<&Range<u64>>,
    sample_limit: u64,
    time_slices: u32,
    stride: EventStride,
    clips: &mut Clips<'a>,
    stop: &AtomicBool,
) -> Result<TuneReport<'a>> {
    let mut input = tune.open_iq()?;
    let center = tune.observed_center_hz;
    let intervals = sampling::intervals(tune.samples, sample_limit, time_slices);
    ensure!(
        !intervals.is_empty(),
        "spectrum requires at least {FFT} samples"
    );
    let intersects_request = |event: &Event| {
        requested.is_none_or(|range| {
            event.lower_frequency_hz < range.end && range.start < event.upper_frequency_hz_exclusive
        })
    };
    let mut event_count = 0;
    let mut detector_omitted_event_count = 0;
    let mut eligible_retained_event_count = 0;
    let mut narrow_search = (clips.narrow_limit() > 0).then(NarrowCounts::default);
    let mut analyzed_samples = 0;
    let mut evidence = Vec::with_capacity(intervals.len());
    for interval in intervals {
        let (_, inspected, events) = sampling::survey(
            &mut input,
            center,
            interval.clone(),
            EventWindows::Stride(stride),
            narrow_search.is_some(),
            stop,
        )?;
        let mut candidates = events.primary;
        analyzed_samples += interval.end - interval.start;
        evidence.push(inspected);
        event_count += candidates.components.count;
        detector_omitted_event_count +=
            candidates.components.count - candidates.components.retained.len();
        candidates.components.retained.retain(intersects_request);
        eligible_retained_event_count += candidates.components.retained.len();
        clips.consider_components(
            tune,
            &input,
            &interval,
            candidates.components.retained,
            stop,
        )?;
        if let Some(counts) = &mut narrow_search {
            let mut narrow = candidates
                .narrow
                .context("missing narrow detector results")?;
            counts.candidate_count += narrow.count;
            counts.detector_omitted_count += narrow.count - narrow.retained.len();
            narrow.retained.retain(intersects_request);
            counts.eligible_retained_count +=
                clips.consider_narrow(tune, &input, &interval, narrow.retained, stop)?;
        }
    }
    tune.verify_iq(&input)?;
    let analyzed_iq_sha256 = if time_slices == 1 {
        evidence.pop().map(|interval| interval.analyzed_iq_sha256)
    } else {
        None
    };
    Ok(TuneReport {
        directory: &tune.directory,
        requested_center_hz: tune.requested_center_hz,
        observed_center_hz: center,
        observed_bands: plan::observed_bands(center, 0),
        source_sidecar_sha256: &tune.sidecar_sha256,
        saved_samples: tune.samples,
        analyzed_samples,
        analyzed_iq_sha256,
        analyzed_intervals: (time_slices > 1).then_some(evidence),
        event_count,
        detector_omitted_event_count,
        eligible_retained_event_count,
        narrow_search,
    })
}

/// Read completed sweeps offline and atomically publish an evidence directory.
pub(crate) fn run(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let sweep = Sweep::open(&root.join(args.sweep_directory))?;
    let sample_limit =
        (args.max_total_samples / sweep.tunes.len() as u64).min(args.max_samples_per_tune);
    ensure!(
        sample_limit >= FFT as u64,
        "analysis budget needs one complete FFT window per tune"
    );
    let destination = root.join(args.output_directory);
    let stage = StagedDirectory::new(&destination, ".sweep-triage-")?;
    let mut retained = Clips::new(args.max_clips as usize, args.narrow_clips as usize);
    let stride = if args.overlap_events {
        EventStride::HalfWindow
    } else {
        EventStride::Contiguous
    };
    let mut tunes = Vec::with_capacity(sweep.tunes.len());
    for tune in &sweep.tunes {
        check_stop(stop)?;
        tunes.push(
            analyze_tune(
                tune,
                sweep.analysis_range(),
                sample_limit,
                args.time_slices,
                stride,
                &mut retained,
                stop,
            )
            .with_context(|| format!("analyze {}", tune.directory))?,
        );
    }
    let max_narrow_clips = retained.narrow_limit();
    let clips = retained
        .finish()
        .into_iter()
        .enumerate()
        .map(|(index, clip)| clip.write(stage.path(), index, stop))
        .collect::<Result<Vec<_>>>()?;
    let report = Report {
        schema_version: match (args.overlap_events, args.time_slices) {
            (true, _) => 4,
            (false, 1) => 2,
            (false, _) => 3,
        },
        operation: "offline_rf_sweep_triage",
        source_operation: sweep.operation,
        source_receipt_sha256: &sweep.source_receipt_sha256,
        requested_range: &sweep.requested_range,
        visit_centers_hz: sweep.visit_centers_hz.as_deref(),
        sample_format: "SC16_Q11_LE",
        sample_rate_hz: crate::SAMPLE_RATE,
        fft_samples: FFT,
        max_total_samples: args.max_total_samples,
        max_samples_per_tune: args.max_samples_per_tune,
        time_slices: (args.time_slices > 1).then_some(args.time_slices),
        event_window_stride_samples: args.overlap_events.then_some(stride.samples()),
        max_clips: args.max_clips,
        max_narrow_clips,
        tunes,
        clips,
    };
    crate::json_file(&stage.path().join("triage.json"), &report)?;
    stage.publish(|| {
        sweep.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Analyzed {} saved tunes and exported {} evidence clips to {}",
        report.tunes.len(),
        report.clips.len(),
        destination.display()
    );
    Ok(())
}
