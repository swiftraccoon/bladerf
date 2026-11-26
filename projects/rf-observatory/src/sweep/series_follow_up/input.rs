//! Validate recorded narrow recurrence geometry using only the bound report bytes.

pub(super) mod activity;

use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::{Deserialize, de::IgnoredAny};
use sha2::{Digest, Sha256};

use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::{CELL_WIDTH_HZ, CENTER_GUARD_HZ, EventBand, EventStride, FFT};
use crate::sweep::{CompletedOperation, pass_directory_name, plan, tune_directory_name};
use crate::triage::sampling::{
    IntervalEvidence, MAX_TIME_SLICES, contains_interval, deserialize_present, intervals,
    validate_intervals,
};
use crate::triage::{MAX_NARROW_WIDTH_HZ, MAX_SAMPLES_PER_TUNE, MAX_TOTAL_SAMPLES};
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE, file_pin};

// The total FFT window budget also bounds the number of interval rows.
const MAX_REPORT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BANDS: usize = 256;
const RETAINED_NARROW_LIMIT: usize = 128;
const HALF_SPAN_HZ: u64 = EventBand::Inner.half_span_hz();
const CELL_HZ: u64 = CELL_WIDTH_HZ as u64;
const GUARD_HZ: u64 = CENTER_GUARD_HZ as u64;
const WINDOW_SAMPLES: u64 = FFT as u64;
const SAMPLES_PER_MILLISECOND: u64 = SAMPLE_RATE as u64 / 1_000;
const _: () = assert!(plan::MAX_PASSES <= u16::BITS as usize);

pub(super) struct Source {
    pub(super) report_sha256: String,
    pub(super) source_series_sha256: String,
    pub(super) narrow_band_count: usize,
    pub(super) bands: Vec<Band>,
    stride: EventStride,
    pub(super) source_operation: CompletedOperation,
    pub(super) requested_range: Option<Range<u64>>,
    pub(super) visit_centers_hz: Option<Vec<u64>>,
    passes: Vec<Pass>,
}

impl Source {
    /// Require the two independent analyses to describe the same saved acquisition.
    pub(super) fn validate_overlap(&self, overlap: &Self) -> Result<()> {
        ensure!(
            matches!(self.stride, EventStride::Contiguous)
                && matches!(overlap.stride, EventStride::HalfWindow),
            "paired series triage requires a contiguous primary report and a half-window overlap report"
        );
        ensure!(
            self.source_series_sha256 == overlap.source_series_sha256,
            "series triage reports refer to different source series digests"
        );
        ensure!(
            self.source_operation == overlap.source_operation
                && self.requested_range == overlap.requested_range
                && self.visit_centers_hz == overlap.visit_centers_hz
                && self.passes.len() == overlap.passes.len(),
            "series triage reports disagree about the acquisition scope or pass count"
        );
        for (pass, paired) in self.passes.iter().zip(&overlap.passes) {
            ensure!(
                pass.directory == paired.directory
                    && pass.source_sweep_sha256 == paired.source_sweep_sha256
                    && pass.tunes.len() == paired.tunes.len(),
                "series triage reports disagree about pass lineage at {}",
                pass.directory
            );
            for (tune, paired) in pass.tunes.iter().zip(&paired.tunes) {
                ensure!(
                    tune.directory == paired.directory
                        && tune.requested_center_hz == paired.requested_center_hz
                        && tune.observed_center_hz == paired.observed_center_hz
                        && tune.saved_samples == paired.saved_samples
                        && tune.source_sidecar_sha256 == paired.source_sidecar_sha256,
                    "series triage reports disagree about tune lineage at {}/{}",
                    pass.directory,
                    tune.directory
                );
                for (samples, digest) in tune.analyzed_extents() {
                    ensure!(
                        paired
                            .analyzed_extents()
                            .all(|(extent, hash)| extent != samples || hash == digest),
                        "series triage reports disagree about analyzed IQ SHA-256 at {}/{} for samples {}..{}",
                        pass.directory,
                        tune.directory,
                        samples.start,
                        samples.end
                    );
                }
            }
        }
        Ok(())
    }
}

pub(super) struct Band {
    pub(super) row_index: usize,
    pub(super) overlap_row_index: Option<usize>,
    pub(super) tune_index: usize,
    pub(super) requested_center_hz: u64,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) source_low_hz: u64,
    pub(super) source_high_hz: u64,
    pub(super) support_mask: u16,
}

#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    operation: String,
    source_series_sha256: String,
    source_operation: CompletedOperation,
    requested_range: Option<Range<u64>>,
    visit_centers_hz: Option<Vec<u64>>,
    sample_format: String,
    sample_rate_hz: u32,
    fft_samples: usize,
    cell_width_hz: u64,
    min_passes: usize,
    max_bands: usize,
    max_samples_per_tune: u64,
    max_total_samples: u64,
    #[serde(default, deserialize_with = "deserialize_present")]
    time_slices: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_present")]
    event_window_stride_samples: Option<usize>,
    passes: Vec<Pass>,
    mean_power_band_count: usize,
    narrow_band_count: usize,
    mean_power_bands: Vec<IgnoredAny>,
    narrow_bands: Vec<ReportedBand>,
}

#[derive(Deserialize)]
struct Pass {
    directory: String,
    source_sweep_sha256: String,
    tunes: Vec<Tune>,
}

#[derive(Deserialize)]
struct Tune {
    directory: String,
    requested_center_hz: u64,
    observed_center_hz: u64,
    source_sidecar_sha256: String,
    saved_samples: u64,
    analyzed_samples: u64,
    #[serde(default, deserialize_with = "deserialize_present")]
    analyzed_iq_sha256: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    analyzed_intervals: Option<Vec<IntervalEvidence>>,
    narrow_candidate_count: usize,
    omitted_narrow_candidate_count: usize,
}

#[derive(Deserialize)]
struct ReportedBand {
    requested_center_hz: u64,
    tune_directory: Option<String>,
    lower_hz: u64,
    upper_hz: u64,
    passes: Vec<Witness>,
}

#[derive(Deserialize)]
struct Witness {
    pass_index: usize,
    score_db: f64,
    narrow_window: NarrowWindow,
}

#[derive(Deserialize)]
struct NarrowWindow {
    lower_hz: u64,
    upper_hz: u64,
    start_sample: u64,
    end_sample_exclusive: u64,
}

impl Report {
    fn validate(&self) -> Result<EventStride> {
        ensure!(
            self.operation == "offline_rf_sweep_series_triage",
            "unsupported sweep series triage report"
        );
        let event_stride = match (
            self.schema_version,
            self.time_slices,
            self.event_window_stride_samples,
        ) {
            (1, None, None) | (2, Some(2..=MAX_TIME_SLICES), None) => EventStride::Contiguous,
            (3, None | Some(2..=MAX_TIME_SLICES), Some(stride))
                if stride == EventStride::HalfWindow.samples() =>
            {
                EventStride::HalfWindow
            }
            _ => anyhow::bail!(
                "unsupported schema, time slices or event stride in sweep series triage report"
            ),
        };
        ensure!(
            is_sha256(&self.source_series_sha256)
                && self.sample_format == "SC16_Q11_LE"
                && self.sample_rate_hz == SAMPLE_RATE
                && self.fft_samples == FFT
                && self.cell_width_hz == CELL_HZ,
            "invalid source digest or unsupported sample geometry"
        );
        ensure!(
            (2..=plan::MAX_PASSES).contains(&self.passes.len())
                && (2..=self.passes.len()).contains(&self.min_passes),
            "invalid series triage pass count or minimum support"
        );
        let reference = &self.passes[0].tunes;
        let dwells = reference.len() * self.passes.len();
        ensure!(
            (1..=plan::MAX_TUNES).contains(&reference.len()) && dwells <= plan::MAX_TOTAL_DWELLS,
            "invalid series triage tune count"
        );
        ensure!(
            (1..=MAX_BANDS).contains(&self.max_bands)
                && self.narrow_bands.len() == self.narrow_band_count.min(self.max_bands)
                && self.mean_power_bands.len() == self.mean_power_band_count.min(self.max_bands)
                && self.narrow_band_count <= reference.len() * EventBand::Inner.cell_count()
                && self.mean_power_band_count <= reference.len() * EventBand::Inner.cell_count(),
            "invalid series triage band counts"
        );
        ensure!(
            (WINDOW_SAMPLES..=MAX_SAMPLES_PER_TUNE).contains(&self.max_samples_per_tune)
                && (WINDOW_SAMPLES..=MAX_TOTAL_SAMPLES).contains(&self.max_total_samples),
            "invalid series triage analysis budget"
        );
        let sample_limit = (self.max_total_samples / dwells as u64).min(self.max_samples_per_tune);
        ensure!(
            sample_limit >= WINDOW_SAMPLES,
            "series triage budget omits a complete dwell window"
        );
        match (self.source_operation, &self.requested_range) {
            (CompletedOperation::Sequential, Some(range))
                if range.start >= MIN_CENTER_HZ
                    && range.start < range.end
                    && range.end <= MAX_CENTER_HZ => {}
            (CompletedOperation::Targeted, None) => {}
            _ => anyhow::bail!("invalid requested scope for source sweep series"),
        }
        if let Some(visits) = &self.visit_centers_hz {
            ensure!(
                reference
                    .iter()
                    .map(|tune| tune.requested_center_hz)
                    .eq(visits.iter().copied()),
                "series triage visit order differs from the requested tune centers"
            );
        } else {
            ensure!(
                reference.is_sorted_by(|a, b| a.requested_center_hz < b.requested_center_hz),
                "series triage requested centers must be strictly increasing"
            );
        }
        for (index, pass) in self.passes.iter().enumerate() {
            ensure!(
                pass.directory == pass_directory_name(index)
                    && is_sha256(&pass.source_sweep_sha256)
                    && pass.tunes.len() == reference.len(),
                "inconsistent series triage pass at index {index}"
            );
            for (index, (tune, nominal)) in pass.tunes.iter().zip(reference).enumerate() {
                tune.validate(
                    index,
                    nominal.requested_center_hz,
                    nominal.saved_samples,
                    sample_limit,
                    self.time_slices,
                )
                .with_context(|| format!("validate {}/{}", pass.directory, tune.directory))?;
            }
            if let Some(range) = &self.requested_range {
                ensure!(
                    plan::covers(
                        range.start,
                        range.end,
                        pass.tunes
                            .iter()
                            .flat_map(|tune| { plan::observed_bands(tune.observed_center_hz, 0) })
                    ),
                    "series triage pass leaves a requested coverage gap"
                );
            }
        }
        Ok(event_stride)
    }

    fn band(&self, row_index: usize, band: &ReportedBand, stride: EventStride) -> Result<Band> {
        let center = band.requested_center_hz;
        let reference = &self.passes[0].tunes;
        let scheduled = self.visit_centers_hz.is_some();
        ensure!(
            band.tune_directory.is_some() == scheduled,
            "narrow band tune identity differs from the series visit policy"
        );
        let tune_index = reference
            .iter()
            .position(|tune| {
                tune.requested_center_hz == center
                    && band
                        .tune_directory
                        .as_ref()
                        .is_none_or(|name| name == &tune.directory)
            })
            .context("narrow band refers to an unknown requested tune")?;
        let requested = self.requested_range.as_ref().filter(|_| !scheduled);
        let nominal = cell_offsets(center, band.lower_hz, band.upper_hz)?;
        ensure!(
            within_sideband(&nominal) && nominal.end - nominal.start <= MAX_NARROW_WIDTH_HZ,
            "nominal narrow band lies outside its sideband or exceeds its width limit"
        );
        ensure!(
            (self.min_passes..=self.passes.len()).contains(&band.passes.len())
                && band.passes.is_sorted_by(|a, b| a.pass_index < b.pass_index),
            "narrow band requires ordered distinct pass support"
        );
        let mut source_low_hz = u64::MAX;
        let mut source_high_hz = 0;
        let mut support_mask = 0;
        for witness in &band.passes {
            let (index, pass) = witness
                .pass_index
                .checked_sub(1)
                .and_then(|index| self.passes.get(index).map(|pass| (index, pass)))
                .context("narrow witness refers to an unknown pass")?;
            let tune = &pass.tunes[tune_index];
            witness.validate(tune, &nominal, requested, stride)?;
            source_low_hz = source_low_hz.min(tune.observed_center_hz);
            source_high_hz = source_high_hz.max(tune.observed_center_hz);
            support_mask |= 1 << index;
        }
        Ok(Band {
            row_index,
            overlap_row_index: None,
            tune_index,
            requested_center_hz: center,
            lower_hz: band.lower_hz,
            upper_hz: band.upper_hz,
            source_low_hz,
            source_high_hz,
            support_mask,
        })
    }
}

impl Tune {
    fn validate(
        &self,
        index: usize,
        center: u64,
        saved: u64,
        sample_limit: u64,
        time_slices: Option<u32>,
    ) -> Result<()> {
        ensure!(
            self.directory == tune_directory_name(index)
                && self.requested_center_hz == center
                && (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&center)
                && self.observed_center_hz.abs_diff(center) <= ACQUISITION_CENTER_TOLERANCE_HZ,
            "invalid series triage tune identity or center"
        );
        let analyzed = saved.min(sample_limit) / WINDOW_SAMPLES * WINDOW_SAMPLES;
        ensure!(
            saved.is_multiple_of(SAMPLES_PER_MILLISECOND)
                && (1..=plan::MAX_DWELL_MS).contains(&(saved / SAMPLES_PER_MILLISECOND))
                && self.saved_samples == saved
                && self.analyzed_samples == analyzed
                && is_sha256(&self.source_sidecar_sha256),
            "invalid series triage tune extent or digest"
        );
        let interval_count = match (
            time_slices,
            self.analyzed_iq_sha256.as_deref(),
            self.analyzed_intervals.as_deref(),
        ) {
            (None, Some(prefix), None) if is_sha256(prefix) => 1,
            (Some(time_slices), None, Some(evidence)) => {
                let expected = intervals(saved, sample_limit, time_slices);
                ensure!(
                    validate_intervals(evidence, &expected, self.analyzed_samples),
                    "invalid distributed series analysis intervals"
                );
                evidence.len()
            }
            _ => anyhow::bail!("invalid series analyzed IQ extent or SHA-256"),
        };
        ensure!(
            self.retained_candidates()
                .is_some_and(|count| count <= interval_count * RETAINED_NARROW_LIMIT),
            "invalid series triage narrow candidate count"
        );
        Ok(())
    }

    fn retained_candidates(&self) -> Option<usize> {
        self.narrow_candidate_count
            .checked_sub(self.omitted_narrow_candidate_count)
    }

    fn analyzed_extents(&self) -> impl Iterator<Item = (Range<u64>, &str)> {
        self.analyzed_iq_sha256
            .as_deref()
            .map(|digest| (0..self.analyzed_samples, digest))
            .into_iter()
            .chain(self.analyzed_intervals.iter().flatten().map(|interval| {
                (
                    interval.start_sample..interval.end_sample_exclusive,
                    interval.analyzed_iq_sha256.as_str(),
                )
            }))
    }
}

impl Witness {
    fn validate(
        &self,
        tune: &Tune,
        nominal: &Range<u64>,
        requested: Option<&Range<u64>>,
        stride: EventStride,
    ) -> Result<()> {
        let window = &self.narrow_window;
        let observed = tune.observed_center_hz;
        let covered = tune.analyzed_intervals.as_deref().map_or(
            window.end_sample_exclusive <= tune.analyzed_samples,
            |analyzed| {
                contains_interval(analyzed, window.start_sample, window.end_sample_exclusive)
            },
        );
        let stride_samples = stride.samples() as u64;
        // The first window spans FFT samples, then each additional window extends
        // the witness by one stride.
        let extension = window
            .end_sample_exclusive
            .saturating_sub(window.start_sample)
            .saturating_sub(WINDOW_SAMPLES);
        ensure!(
            self.score_db.is_finite()
                && self.score_db >= 3.0
                && tune.retained_candidates().is_some_and(|count| count > 0)
                && window.start_sample < window.end_sample_exclusive
                && covered
                && window.start_sample.is_multiple_of(stride_samples)
                && window.end_sample_exclusive.is_multiple_of(stride_samples)
                && (1..=9).contains(&(extension / stride_samples)),
            "invalid narrow witness contrast or analyzed sample interval"
        );
        let raw = cell_offsets(observed, window.lower_hz, window.upper_hz)?;
        let width = raw.end - raw.start;
        let left = raw
            .start
            .checked_sub(width)
            .context("narrow witness lacks its left flank")?;
        ensure!(
            width <= MAX_NARROW_WIDTH_HZ
                && within_sideband(&(left..raw.end + width))
                && raw.start <= nominal.start
                && nominal.end <= raw.end,
            "narrow witness or its flanks do not cover the nominal band within a sideband"
        );
        if let Some(request) = requested {
            ensure!(
                (nominal.start..nominal.end)
                    .step_by(CELL_HZ as usize)
                    .all(|offset| {
                        [tune.requested_center_hz, observed]
                            .into_iter()
                            .all(|center| {
                                let start = center - HALF_SPAN_HZ + offset;
                                start < request.end && request.start < start + CELL_HZ
                            })
                    }),
                "narrow band contains a cell outside the requested scope"
            );
        }
        Ok(())
    }
}

fn cell_offsets(center: u64, lower: u64, upper: u64) -> Result<Range<u64>> {
    let origin = center - HALF_SPAN_HZ;
    ensure!(
        origin <= lower
            && lower < upper
            && upper <= center + HALF_SPAN_HZ
            && (lower - origin).is_multiple_of(CELL_HZ)
            && (upper - origin).is_multiple_of(CELL_HZ),
        "narrow frequency bounds are outside the activity grid or misaligned"
    );
    Ok(lower - origin..upper - origin)
}

fn within_sideband(offsets: &Range<u64>) -> bool {
    offsets.end <= HALF_SPAN_HZ - GUARD_HZ
        || (offsets.start >= HALF_SPAN_HZ + GUARD_HZ && offsets.end <= 2 * HALF_SPAN_HZ)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && HEXLOWER.decode(value.as_bytes()).is_ok()
}

/// Read a stable saved series triage report and extract its narrow rows.
/// Referenced pass receipts, sidecars and IQ are outside this admission path.
pub(super) fn read(path: &Path) -> Result<Source> {
    let (mut file, length) = file_pin::open_regular(path, MAX_REPORT_BYTES)?;
    let before = file.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&mut file).take(length + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == length
            && before.len() == length
            && after.len() == length
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "series triage report changed while reading"
    );
    let report: Report = serde_json::from_slice(&bytes).context("parse series triage report")?;
    let stride = report.validate()?;
    let bands = report
        .narrow_bands
        .iter()
        .enumerate()
        .map(|(index, band)| {
            report
                .band(index + 1, band, stride)
                .with_context(|| format!("validate narrow band row {}", index + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Source {
        report_sha256: HEXLOWER.encode(&Sha256::digest(bytes)),
        source_series_sha256: report.source_series_sha256,
        narrow_band_count: report.narrow_band_count,
        bands,
        stride,
        source_operation: report.source_operation,
        requested_range: report.requested_range,
        visit_centers_hz: report.visit_centers_hz,
        passes: report.passes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confines_late_narrow_witnesses_to_one_analyzed_interval() {
        let center = 915_000_000;
        let saved = 200_000;
        let budget = 16 * WINDOW_SAMPLES;
        let digest = "a".repeat(64);
        let schedule = intervals(saved, budget, 2);
        let tune = Tune {
            directory: tune_directory_name(0),
            requested_center_hz: center,
            observed_center_hz: center,
            source_sidecar_sha256: digest.clone(),
            saved_samples: saved,
            analyzed_samples: budget,
            analyzed_iq_sha256: None,
            analyzed_intervals: Some(
                schedule
                    .iter()
                    .map(|span| IntervalEvidence {
                        start_sample: span.start,
                        end_sample_exclusive: span.end,
                        analyzed_iq_sha256: digest.clone(),
                    })
                    .collect(),
            ),
            narrow_candidate_count: 2 * RETAINED_NARROW_LIMIT,
            omitted_narrow_candidate_count: 0,
        };
        tune.validate(0, center, saved, budget, Some(2)).unwrap();
        let lower = center + 8 * CELL_HZ;
        let upper = lower + CELL_HZ;
        let nominal = cell_offsets(center, lower, upper).unwrap();
        let late_start = schedule[1].start;
        let mut witness = Witness {
            pass_index: 1,
            score_db: 6.0,
            narrow_window: NarrowWindow {
                lower_hz: lower,
                upper_hz: upper,
                start_sample: late_start,
                end_sample_exclusive: late_start + 2 * WINDOW_SAMPLES,
            },
        };
        assert!(witness.narrow_window.end_sample_exclusive > tune.analyzed_samples);
        witness
            .validate(&tune, &nominal, None, EventStride::Contiguous)
            .unwrap();

        let stride = EventStride::HalfWindow;
        let step = stride.samples() as u64;
        witness.narrow_window.start_sample = late_start + step;
        for (windows, valid) in [(1, false), (2, true), (10, true), (11, false)] {
            witness.narrow_window.end_sample_exclusive =
                witness.narrow_window.start_sample + WINDOW_SAMPLES + (windows - 1) * step;
            assert_eq!(
                witness.validate(&tune, &nominal, None, stride).is_ok(),
                valid
            );
        }
        witness.narrow_window.end_sample_exclusive =
            witness.narrow_window.start_sample + WINDOW_SAMPLES + step + 1;
        assert!(witness.validate(&tune, &nominal, None, stride).is_err());

        witness.narrow_window.start_sample = schedule[0].end - WINDOW_SAMPLES;
        witness.narrow_window.end_sample_exclusive = late_start + WINDOW_SAMPLES;
        assert_eq!(
            witness.narrow_window.end_sample_exclusive - witness.narrow_window.start_sample,
            10 * WINDOW_SAMPLES
        );
        assert!(
            witness
                .validate(&tune, &nominal, None, EventStride::Contiguous)
                .is_err()
        );
    }

    #[test]
    fn scheduled_recurrence_binds_the_witness_to_its_visit() {
        let background = 907_000_000;
        let priority = 1_090_000_000;
        let visits = [priority, background, priority];
        let digest = "a".repeat(64);
        let sample_limit = MAX_SAMPLES_PER_TUNE;
        let cell_lower = priority + 8 * CELL_HZ;
        let mut report = Report {
            schema_version: 1,
            operation: "offline_rf_sweep_series_triage".to_owned(),
            source_series_sha256: digest.clone(),
            source_operation: CompletedOperation::Sequential,
            requested_range: Some(902_000_000..906_000_000),
            visit_centers_hz: Some(visits.to_vec()),
            sample_format: "SC16_Q11_LE".to_owned(),
            sample_rate_hz: SAMPLE_RATE,
            fft_samples: FFT,
            cell_width_hz: CELL_HZ,
            min_passes: 2,
            max_bands: 1,
            max_samples_per_tune: sample_limit,
            max_total_samples: sample_limit * 6,
            time_slices: None,
            event_window_stride_samples: None,
            passes: (0..2)
                .map(|pass| Pass {
                    directory: pass_directory_name(pass),
                    source_sweep_sha256: digest.clone(),
                    tunes: visits
                        .into_iter()
                        .enumerate()
                        .map(|(index, center)| {
                            let saved_samples = if center == priority { 40_000 } else { 20_000 };
                            Tune {
                                directory: tune_directory_name(index),
                                requested_center_hz: center,
                                observed_center_hz: center,
                                source_sidecar_sha256: digest.clone(),
                                saved_samples,
                                analyzed_samples: saved_samples / WINDOW_SAMPLES * WINDOW_SAMPLES,
                                analyzed_iq_sha256: Some(digest.clone()),
                                analyzed_intervals: None,
                                narrow_candidate_count: usize::from(index == 2),
                                omitted_narrow_candidate_count: 0,
                            }
                        })
                        .collect(),
                })
                .collect(),
            mean_power_band_count: 0,
            narrow_band_count: 1,
            mean_power_bands: Vec::new(),
            narrow_bands: vec![ReportedBand {
                requested_center_hz: priority,
                tune_directory: Some(tune_directory_name(2)),
                lower_hz: cell_lower,
                upper_hz: cell_lower + CELL_HZ,
                passes: (1..=2)
                    .map(|pass_index| Witness {
                        pass_index,
                        score_db: 6.0,
                        narrow_window: NarrowWindow {
                            lower_hz: cell_lower,
                            upper_hz: cell_lower + CELL_HZ,
                            start_sample: 0,
                            end_sample_exclusive: 2 * WINDOW_SAMPLES,
                        },
                    })
                    .collect(),
            }],
        };
        let stride = report.validate().unwrap();
        let band = report.band(1, &report.narrow_bands[0], stride).unwrap();
        assert_eq!(band.support_mask.count_ones(), 2);
        assert_eq!(band.source_low_hz, priority);

        for directory in [Some(tune_directory_name(0)), None] {
            report.narrow_bands[0].tune_directory = directory;
            assert!(report.band(1, &report.narrow_bands[0], stride).is_err());
        }
    }
}
