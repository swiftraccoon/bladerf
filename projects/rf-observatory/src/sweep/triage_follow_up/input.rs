//! Admit candidate metadata from a bounded saved sweep triage receipt.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::{EventStride, FFT};
use crate::sweep::{plan, tune_directory_name};
use crate::triage::sampling::{
    IntervalEvidence, MAX_TIME_SLICES, contains_interval, deserialize_present, intervals,
    validate_intervals,
};
use crate::triage::{
    MAX_NARROW_WIDTH_HZ, MAX_SAMPLES_PER_TUNE, MAX_TOTAL_SAMPLES, MIN_NARROW_CENTER_OFFSET_HZ,
};
use crate::waveform::{CLIP_CONTEXT_SAMPLES, MAX_CLIP_SAMPLES, MIN_INPUT_INTERVAL};
use crate::{MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin};

const MAX_REPORT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CLIPS: usize = 32;

pub(super) struct Source {
    pub(super) report_sha256: String,
    pub(super) source_sweep_sha256: String,
    pub(super) input_clips: usize,
    pub(super) clips: Vec<Clip>,
}

pub(super) struct Clip {
    pub(super) file: String,
    pub(super) sha256: String,
    pub(super) tune_directory: String,
    pub(super) observed_center_hz: u64,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) target_hz: u64,
}

#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    operation: String,
    source_receipt_sha256: String,
    sample_format: String,
    sample_rate_hz: u32,
    fft_samples: usize,
    #[serde(default, deserialize_with = "deserialize_present")]
    time_slices: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_present")]
    event_window_stride_samples: Option<usize>,
    max_samples_per_tune: Option<u64>,
    max_total_samples: Option<u64>,
    tunes: Vec<Tune>,
    clips: Vec<ClipRecord>,
}

impl Report {
    fn event_stride(&self) -> Result<EventStride> {
        match (self.schema_version, self.event_window_stride_samples) {
            (1..=3, None) => Ok(EventStride::Contiguous),
            (4, Some(samples)) if samples == EventStride::HalfWindow.samples() => {
                Ok(EventStride::HalfWindow)
            }
            _ => anyhow::bail!(
                "invalid event window stride for schema {}",
                self.schema_version
            ),
        }
    }

    fn sample_schedule(&self) -> Result<Option<(u64, u32)>> {
        if self.schema_version < 3 {
            ensure!(
                self.time_slices.is_none(),
                "legacy triage receipt has time slices"
            );
            return Ok(None);
        }
        let time_slices = match (self.schema_version, self.time_slices) {
            (4, None) => 1,
            (_, Some(count)) if (2..=MAX_TIME_SLICES).contains(&count) => count,
            _ => anyhow::bail!("invalid distributed triage slice count"),
        };
        let per_tune = self
            .max_samples_per_tune
            .filter(|cap| (FFT as u64..=MAX_SAMPLES_PER_TUNE).contains(cap))
            .context("invalid triage per-tune sample budget")?;
        let total = self
            .max_total_samples
            .filter(|cap| (FFT as u64..=MAX_TOTAL_SAMPLES).contains(cap))
            .context("invalid triage total sample budget")?;
        let sample_limit = per_tune.min(total / self.tunes.len() as u64);
        ensure!(
            sample_limit >= FFT as u64,
            "triage budget omits a complete tune window"
        );
        Ok(Some((sample_limit, time_slices)))
    }
}

#[derive(Deserialize)]
struct Tune {
    directory: String,
    requested_center_hz: u64,
    observed_center_hz: u64,
    observed_bands: [plan::Band; 2],
    source_sidecar_sha256: String,
    saved_samples: u64,
    analyzed_samples: u64,
    #[serde(default, deserialize_with = "deserialize_present")]
    analyzed_iq_sha256: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    analyzed_intervals: Option<Vec<IntervalEvidence>>,
}

impl Tune {
    fn validate(&self, index: usize, sample_schedule: Option<(u64, u32)>) -> Result<()> {
        ensure!(
            self.directory == tune_directory_name(index),
            "unexpected triage tune directory at index {index}"
        );
        ensure!(
            (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&self.requested_center_hz)
                && self.observed_center_hz.abs_diff(self.requested_center_hz)
                    <= ACQUISITION_CENTER_TOLERANCE_HZ
                && self.observed_bands == plan::observed_bands(self.observed_center_hz, 0),
            "invalid tuning or observed bands for {}",
            self.directory
        );
        ensure!(
            (FFT as u64..=self.saved_samples).contains(&self.analyzed_samples)
                && self.analyzed_samples.is_multiple_of(FFT as u64)
                && self.saved_samples <= MAX_SAMPLES,
            "invalid saved or analyzed sample extent for {}",
            self.directory
        );
        ensure!(
            is_sha256(&self.source_sidecar_sha256),
            "invalid input SHA-256 for {}",
            self.directory
        );
        match (
            sample_schedule,
            self.analyzed_iq_sha256.as_deref(),
            self.analyzed_intervals.as_deref(),
        ) {
            (None, Some(prefix), None) if is_sha256(prefix) => {}
            (Some((sample_limit, 1)), Some(prefix), None) if is_sha256(prefix) => {
                let windows = self.saved_samples.min(sample_limit) / FFT as u64;
                ensure!(
                    self.analyzed_samples == windows * FFT as u64,
                    "invalid analyzed IQ prefix for {}",
                    self.directory
                );
            }
            (Some((sample_limit, time_slices)), None, Some(analyzed)) if time_slices > 1 => {
                let expected = intervals(self.saved_samples, sample_limit, time_slices);
                ensure!(
                    validate_intervals(analyzed, &expected, self.analyzed_samples),
                    "invalid distributed analysis intervals for {}",
                    self.directory
                );
            }
            _ => anyhow::bail!(
                "invalid analyzed IQ extent or SHA-256 for {}",
                self.directory
            ),
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct ClipRecord {
    tune_directory: String,
    file: String,
    sha256: String,
    start_sample: u64,
    end_sample_exclusive: u64,
    event_start_sample: u64,
    event_end_sample_exclusive: u64,
    peak_window_start_sample: u64,
    lower_hz: u64,
    upper_hz: u64,
    detector: Option<Detector>,
    waveform: Waveform,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Detector {
    AdjacentContrast,
    ConnectedComponent,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Waveform {
    Measured {
        target_hz: u64,
        clip_start_sample: u64,
        clip_end_sample_exclusive: u64,
    },
    Skipped,
}

impl ClipRecord {
    fn into_candidate(
        self,
        index: usize,
        schema_version: u32,
        stride: EventStride,
        tunes: &[Tune],
    ) -> Result<Option<(Detector, Clip)>> {
        ensure!(
            self.file == format!("clip-{:04}.sc16q11", index + 1),
            "unexpected clip file name"
        );
        ensure!(is_sha256(&self.sha256), "invalid clip SHA-256");
        let tune = tunes
            .iter()
            .find(|tune| tune.directory == self.tune_directory)
            .context("clip refers to an unknown tune directory")?;
        let start = self.start_sample.min(self.event_start_sample);
        let end = self
            .end_sample_exclusive
            .max(self.event_end_sample_exclusive);
        let covered = tune
            .analyzed_intervals
            .as_deref()
            .map_or(end <= tune.analyzed_samples, |analyzed| {
                contains_interval(analyzed, start, end)
            });
        let stride_samples = stride.samples() as u64;
        let event_aligned = schema_version < 4
            || (self.event_start_sample.is_multiple_of(stride_samples)
                && self
                    .event_end_sample_exclusive
                    .is_multiple_of(stride_samples));
        ensure!(
            self.start_sample < self.end_sample_exclusive
                && self.end_sample_exclusive - self.start_sample <= MAX_CLIP_SAMPLES as u64
                && self.event_start_sample < self.event_end_sample_exclusive
                && covered
                && event_aligned
                && self.peak_window_start_sample.is_multiple_of(stride_samples)
                && self.peak_window_start_sample >= self.event_start_sample.max(self.start_sample)
                && self
                    .peak_window_start_sample
                    .checked_add(FFT as u64)
                    .is_some_and(|end| {
                        end <= self
                            .event_end_sample_exclusive
                            .min(self.end_sample_exclusive)
                    }),
            "invalid clip, event or peak-window sample extent"
        );
        ensure!(
            self.lower_hz < self.upper_hz
                && tune.observed_bands.iter().any(|band| {
                    band.lower_hz <= self.lower_hz && self.upper_hz <= band.upper_hz
                }),
            "clip frequency bounds are outside the observed sidebands"
        );
        let detector = if schema_version >= 2 {
            self.detector
                .with_context(|| format!("schema {schema_version} clip needs a detector"))?
        } else {
            Detector::ConnectedComponent
        };
        let Waveform::Measured {
            target_hz,
            clip_start_sample,
            clip_end_sample_exclusive,
        } = self.waveform
        else {
            return Ok(None);
        };
        ensure!(
            target_hz == self.lower_hz.midpoint(self.upper_hz)
                && self.upper_hz - self.lower_hz <= MAX_NARROW_WIDTH_HZ
                && target_hz.abs_diff(tune.observed_center_hz) >= MIN_NARROW_CENTER_OFFSET_HZ,
            "measured waveform target is outside the usable probe geometry"
        );
        let clip_samples = self.end_sample_exclusive - self.start_sample;
        ensure!(
            clip_start_sample >= CLIP_CONTEXT_SAMPLES as u64
                && clip_start_sample < clip_end_sample_exclusive
                && clip_end_sample_exclusive <= clip_samples
                && clip_samples - clip_end_sample_exclusive >= CLIP_CONTEXT_SAMPLES as u64
                && clip_end_sample_exclusive - clip_start_sample >= MIN_INPUT_INTERVAL as u64
                && self.start_sample + clip_start_sample >= self.event_start_sample
                && self.start_sample + clip_end_sample_exclusive <= self.event_end_sample_exclusive,
            "measured waveform interval is outside the event or lacks filter context"
        );
        Ok(Some((
            detector,
            Clip {
                file: self.file,
                sha256: self.sha256,
                tune_directory: self.tune_directory,
                observed_center_hz: tune.observed_center_hz,
                lower_hz: self.lower_hz,
                upper_hz: self.upper_hz,
                target_hz,
            },
        )))
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && HEXLOWER.decode(value.as_bytes()).is_ok()
}

/// Read the saved receipt and return measured clips in detector priority order.
///
/// Candidate provenance binds the receipt bytes and its recorded digests.
/// The receipt is the sole input.
pub(super) fn read(path: &Path) -> Result<Source> {
    let (mut file, length) = file_pin::open_regular(path, MAX_REPORT_BYTES)?;
    let before = file.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&mut file)
        .take(length + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read sweep triage receipt {}", path.display()))?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == length
            && before.len() == length
            && after.len() == length
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "triage receipt changed while reading"
    );
    let report: Report = serde_json::from_slice(&bytes).context("parse sweep triage receipt")?;
    ensure!(
        (1..=4).contains(&report.schema_version) && report.operation == "offline_rf_sweep_triage",
        "unsupported sweep triage receipt contract"
    );
    ensure!(
        report.sample_format == "SC16_Q11_LE"
            && report.sample_rate_hz == SAMPLE_RATE
            && report.fft_samples == FFT,
        "unsupported sample format, rate or FFT size in sweep triage receipt"
    );
    ensure!(
        is_sha256(&report.source_receipt_sha256),
        "invalid source sweep receipt SHA-256"
    );
    ensure!(
        (1..=plan::MAX_TUNES).contains(&report.tunes.len()) && report.clips.len() <= MAX_CLIPS,
        "invalid triage tune or clip count"
    );
    let stride = report.event_stride()?;
    let sample_schedule = report.sample_schedule()?;
    for (index, tune) in report.tunes.iter().enumerate() {
        tune.validate(index, sample_schedule)?;
    }
    let input_clips = report.clips.len();
    let mut candidates = Vec::with_capacity(input_clips);
    for (index, clip) in report.clips.into_iter().enumerate() {
        if let Some(candidate) = clip
            .into_candidate(index, report.schema_version, stride, &report.tunes)
            .with_context(|| format!("admit triage clip {}", index + 1))?
        {
            candidates.push(candidate);
        }
    }
    candidates.sort_by_key(|(detector, _)| *detector);
    Ok(Source {
        report_sha256: HEXLOWER.encode(&Sha256::digest(&bytes)),
        source_sweep_sha256: report.source_receipt_sha256,
        input_clips,
        clips: candidates.into_iter().map(|(_, clip)| clip).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_a_late_clip_and_rejects_an_event_spanning_unread_samples() {
        let center = 915_000_000;
        let saved = 400_000;
        let budget = 16 * FFT as u64;
        let digest = "a".repeat(64);
        let schedule = intervals(saved, budget, 2);
        let late_start = schedule[1].start;
        let late_end = late_start + FFT as u64;
        let tune = Tune {
            directory: tune_directory_name(0),
            requested_center_hz: center,
            observed_center_hz: center,
            observed_bands: plan::observed_bands(center, 0),
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
        };
        tune.validate(0, Some((budget, 2))).unwrap();
        let clip = |event_start_sample| ClipRecord {
            tune_directory: tune.directory.clone(),
            file: "clip-0001.sc16q11".to_owned(),
            sha256: digest.clone(),
            start_sample: late_start,
            end_sample_exclusive: late_end,
            event_start_sample,
            event_end_sample_exclusive: late_end,
            peak_window_start_sample: late_start,
            lower_hz: center + 1_000_000,
            upper_hz: center + 1_100_000,
            detector: Some(Detector::ConnectedComponent),
            waveform: Waveform::Skipped,
        };
        assert!(late_end > tune.analyzed_samples);
        assert!(
            clip(late_start)
                .into_candidate(0, 3, EventStride::Contiguous, std::slice::from_ref(&tune))
                .is_ok()
        );
        assert!(
            clip(schedule[0].end - FFT as u64)
                .into_candidate(0, 3, EventStride::Contiguous, std::slice::from_ref(&tune))
                .is_err()
        );
    }
}
