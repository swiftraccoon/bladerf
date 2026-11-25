//! Validate the whole-segment band probe before the rolling watch releases IQ.

use std::fs::Metadata;
use std::ops::Range;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::{Deserialize, Deserializer, Serialize};

use crate::event_replay::replay::EvidenceFile;
use crate::input::{NORMALIZED_SC16_Q11_FORMAT, SourceReport};
use crate::process::{self, ProgramProvenance};

use super::intake::ValidatedSegment;

pub(super) const REPORT_NAME: &str = "band-bursts.json";
pub(super) const REPORT_BYTES: u64 = 2 * 1024 * 1024;
const LOG_BYTES: u64 = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const SAMPLE_RATE_HZ: u64 = 20_000_000;
pub(super) const WINDOW_SAMPLES: u64 = 1024;
const MINIMUM_WINDOWS: u64 = 2;
const THRESHOLD_DB: f64 = 6.0;
const MAX_CONTEXT_BANDS: usize = 7;

/// Half-open absolute frequency band in hertz.
#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct Band {
    pub(super) lower_frequency_hz: u64,
    pub(super) upper_frequency_hz_exclusive: u64,
}

impl FromStr for Band {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOW:HIGH in hertz")?;
        let lower_frequency_hz = lower.parse().context("invalid lower band edge in hertz")?;
        let upper_frequency_hz_exclusive =
            upper.parse().context("invalid upper band edge in hertz")?;
        ensure!(
            lower_frequency_hz < upper_frequency_hz_exclusive,
            "burst band lower edge must precede its upper edge"
        );
        Ok(Self {
            lower_frequency_hz,
            upper_frequency_hz_exclusive,
        })
    }
}

impl Band {
    pub(super) fn verify(self, center_hz: u64) -> Result<()> {
        self.integrated_bins(center_hz).map(|_| ())
    }

    pub(super) fn verify_context(self, context: &[Self], center_hz: u64) -> Result<()> {
        ensure!(
            context.len() <= MAX_CONTEXT_BANDS,
            "burst probe supports up to {MAX_CONTEXT_BANDS} context bands"
        );
        self.verify(center_hz)?;
        for (index, &band) in context.iter().enumerate() {
            band.verify(center_hz)?;
            ensure!(
                std::iter::once(&self)
                    .chain(&context[..index])
                    .all(|previous| {
                        band.upper_frequency_hz_exclusive <= previous.lower_frequency_hz
                            || previous.upper_frequency_hz_exclusive <= band.lower_frequency_hz
                    }),
                "burst probe bands must be disjoint"
            );
        }
        Ok(())
    }

    fn integrated_bins(self, center_hz: u64) -> Result<u64> {
        let half_rate = SAMPLE_RATE_HZ / 2;
        let lower = center_hz
            .checked_sub(half_rate)
            .context("burst tuning lies below half the sample rate")?;
        let upper = center_hz
            .checked_add(half_rate)
            .context("burst sampled band overflows hertz")?;
        ensure!(
            lower <= self.lower_frequency_hz
                && self.lower_frequency_hz < self.upper_frequency_hz_exclusive
                && self.upper_frequency_hz_exclusive <= upper,
            "burst band must lie inside the sampled band"
        );
        // Each half-open edge selects the first FFT bin at or above the edge.
        let edge_bin = |hz: u64| ((hz - lower) * WINDOW_SAMPLES).div_ceil(SAMPLE_RATE_HZ);
        let bins = edge_bin(self.upper_frequency_hz_exclusive) - edge_bin(self.lower_frequency_hz);
        ensure!(bins > 0, "burst band contains no FFT bin centers");
        Ok(bins)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct BurstCounts {
    pub(super) segments: u64,
    pub(super) bursts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) overlapping_validated_wifi: Option<u64>,
}

impl BurstCounts {
    pub(super) fn total(counts: impl Iterator<Item = Self>) -> Option<Self> {
        counts.reduce(|total, next| Self {
            segments: total.segments + next.segments,
            bursts: total.bursts + next.bursts,
            overlapping_validated_wifi: total
                .overlapping_validated_wifi
                .zip(next.overlapping_validated_wifi)
                .map(|(left, right)| left + right),
        })
    }
}

#[derive(Serialize)]
pub(super) struct BurstReport {
    #[serde(rename = "report_sha256")]
    evidence: EvidenceFile,
    #[serde(flatten)]
    counts: BurstCounts,
    #[serde(skip)]
    band: Band,
    #[serde(skip)]
    physical_start: u64,
    #[serde(skip)]
    detections: Vec<BurstDetection>,
}

impl BurstReport {
    pub(super) fn verify(&self) -> Result<()> {
        self.evidence.verify()
    }

    pub(super) fn sha256(&self) -> &str {
        &self.evidence.sha256
    }

    pub(super) fn verify_stamp(&self, observed: &Metadata) -> Result<()> {
        self.evidence.verify_stamp(observed)
    }

    pub(super) fn counts(&self) -> BurstCounts {
        self.counts
    }

    pub(super) fn band(&self) -> Band {
        self.band
    }

    pub(super) fn detections(&self) -> &[BurstDetection] {
        &self.detections
    }

    /// Count overlaps after the decoder returns its sorted union of frame spans.
    pub(super) fn record_wifi_spans(&mut self, wifi_spans: Option<&[Range<u64>]>) {
        self.counts.overlapping_validated_wifi = wifi_spans.map(|spans| {
            let mut wifi = spans.iter().peekable();
            self.detections
                .iter()
                .filter(|detection| {
                    let start = self.physical_start + detection.start_sample;
                    let end = self.physical_start + detection.end_sample_exclusive;
                    while wifi.peek().is_some_and(|span| span.end <= start) {
                        wifi.next();
                    }
                    wifi.peek().is_some_and(|span| span.start < end)
                })
                .count() as u64
        });
    }

    pub(super) fn discard_detections(&mut self) {
        self.detections = Vec::new();
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BurstDetection {
    pub(super) start_sample: u64,
    pub(super) end_sample_exclusive: u64,
    pub(super) peak_excess_db: f64,
}

impl BurstDetection {
    pub(super) fn is_strong_short(self) -> bool {
        let duration = self.end_sample_exclusive - self.start_sample;
        (duration == 2 * WINDOW_SAMPLES || duration == 3 * WINDOW_SAMPLES)
            && self.peak_excess_db >= 10.0
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeOutput {
    schema_version: u32,
    operation: String,
    source_sha256: String,
    source_complex_samples: u64,
    source_sample_rate_hz: u64,
    center_frequency_hz: u64,
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    window_samples: u64,
    integrated_fft_bins: u64,
    baseline_scope: String,
    baseline_end_sample_exclusive: u64,
    baseline_band_power: f64,
    search_start_sample: u64,
    analyzed_end_sample_exclusive: u64,
    omitted_trailing_samples: u64,
    threshold_db: f64,
    minimum_windows: u64,
    detections: Vec<BurstDetection>,
    #[serde(default, deserialize_with = "present_context")]
    additional_bands: Option<Vec<ContextBand>>,
}

// A present field must be an array. This preserves schema 1's exact field set.
fn present_context<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<ContextBand>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextBand {
    lower_frequency_hz: u64,
    upper_frequency_hz_exclusive: u64,
    integrated_fft_bins: u64,
    baseline_band_power: f64,
    above_threshold_windows_hex: String,
}

impl ContextBand {
    fn verify(&self, expected: Band, center_hz: u64, windows: u64) -> Result<()> {
        ensure!(
            self.lower_frequency_hz == expected.lower_frequency_hz
                && self.upper_frequency_hz_exclusive == expected.upper_frequency_hz_exclusive
                && self.integrated_fft_bins == expected.integrated_bins(center_hz)?
                && self.baseline_band_power.is_finite()
                && self.baseline_band_power >= 0.0,
            "burst probe context differs from the requested band"
        );
        let encoded = &self.above_threshold_windows_hex;
        ensure!(
            encoded.len() as u64 == 2 * windows.div_ceil(8)
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "burst probe context has an invalid hexadecimal window bitmap"
        );
        let decoded = HEXLOWER
            .decode(encoded.as_bytes())
            .context("decode burst probe context window bitmap")?;
        let tail_bits = windows % 8;
        ensure!(
            tail_bits == 0 || decoded.last().is_some_and(|last| *last >> tail_bits == 0),
            "burst probe context window bitmap has nonzero spare bits"
        );
        ensure!(
            self.baseline_band_power > 0.0 || decoded.iter().all(|byte| *byte == 0),
            "burst probe context window bitmap is nonempty with a zero baseline"
        );
        Ok(())
    }
}

impl ProbeOutput {
    fn counts(
        &self,
        band: Band,
        context_bands: &[Band],
        source: &SourceReport,
        physical_start: u64,
    ) -> Result<BurstCounts> {
        let schema_version = if context_bands.is_empty() { 1 } else { 2 };
        ensure!(
            self.schema_version == schema_version
                && self.operation == "probe_saved_iq_band_bursts"
                && self.source_sha256 == source.sha256
                && source.format == NORMALIZED_SC16_Q11_FORMAT
                && source.sample_rate_hz == SAMPLE_RATE_HZ
                && self.source_sample_rate_hz == source.sample_rate_hz
                && self.source_complex_samples == source.complex_samples
                && self.center_frequency_hz == source.center_frequency_hz
                && self.lower_frequency_hz == band.lower_frequency_hz
                && self.upper_frequency_hz_exclusive == band.upper_frequency_hz_exclusive
                && self.integrated_fft_bins == band.integrated_bins(source.center_frequency_hz)?,
            "burst probe report differs from the admitted IQ or requested band"
        );
        let windows = source.complex_samples / WINDOW_SAMPLES;
        let analyzed_end = windows * WINDOW_SAMPLES;
        let bursts = u64::try_from(self.detections.len())?;
        ensure!(
            source.complex_samples <= SAMPLE_RATE_HZ
                && windows >= MINIMUM_WINDOWS
                && self.window_samples == WINDOW_SAMPLES
                && self.minimum_windows == MINIMUM_WINDOWS
                && self.threshold_db == THRESHOLD_DB
                && self.baseline_scope == "whole_extent"
                && self.search_start_sample == 0
                && self.analyzed_end_sample_exclusive == analyzed_end
                && self.baseline_end_sample_exclusive == analyzed_end
                && self.omitted_trailing_samples == source.complex_samples - analyzed_end
                && self.baseline_band_power.is_finite()
                && self.baseline_band_power >= 0.0
                && (self.baseline_band_power > 0.0 || bursts == 0)
                && bursts <= (windows + 1) / (MINIMUM_WINDOWS + 1),
            "burst probe does not cover the expected whole-segment window grid"
        );
        physical_start
            .checked_add(analyzed_end)
            .context("burst probe stream sample extent overflow")?;
        ensure!(
            self.additional_bands.as_ref().map(Vec::len)
                == (!context_bands.is_empty()).then_some(context_bands.len()),
            "burst probe report differs from the requested context band count"
        );
        if let Some(measured) = &self.additional_bands {
            for (measurement, &expected) in measured.iter().zip(context_bands) {
                measurement.verify(expected, source.center_frequency_hz, windows)?;
            }
        }

        let mut previous_end = None;
        for detection in &self.detections {
            let start = detection.start_sample;
            let end = detection.end_sample_exclusive;
            ensure!(
                start.is_multiple_of(WINDOW_SAMPLES)
                    && end.is_multiple_of(WINDOW_SAMPLES)
                    && end <= analyzed_end
                    && end.saturating_sub(start) >= MINIMUM_WINDOWS * WINDOW_SAMPLES
                    && previous_end.is_none_or(|prior| prior < start)
                    && detection.peak_excess_db.is_finite()
                    // The probe reports excess from a difference of logarithms.
                    && detection.peak_excess_db >= THRESHOLD_DB - 1e-9,
                "burst probe reported an invalid detection interval or peak"
            );
            previous_end = Some(end);
        }
        Ok(BurstCounts {
            segments: 1,
            bursts,
            overlapping_validated_wifi: None,
        })
    }
}

/// Run the whole-extent probe on the admitted physical IQ and pin its report.
pub(super) fn probe(
    program: &ProgramProvenance,
    band: Band,
    context_bands: &[Band],
    segment: &ValidatedSegment,
    output: &Path,
    stop: &AtomicBool,
) -> Result<BurstReport> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "rolling burst probe interrupted"
    );
    let source = &segment.source;
    band.verify_context(context_bands, source.center_frequency_hz)?;
    segment.verify_unchanged()?;
    let path = output.join(REPORT_NAME);
    let mut arguments = vec![
        "probe-bursts".into(),
        source.input.as_os_str().to_owned(),
        "--center-hz".into(),
        source.center_frequency_hz.to_string().into(),
        "--lower-hz".into(),
        band.lower_frequency_hz.to_string().into(),
        "--upper-hz".into(),
        band.upper_frequency_hz_exclusive.to_string().into(),
        "--expect-sha256".into(),
        source.sha256.as_str().into(),
        "--output".into(),
        path.as_os_str().to_owned(),
    ];
    for context in context_bands {
        arguments.extend([
            "--also-band-hz".into(),
            format!(
                "{}:{}",
                context.lower_frequency_hz, context.upper_frequency_hz_exclusive
            )
            .into(),
        ]);
    }
    let command = process::run(
        program,
        &arguments,
        output,
        "band-burst",
        LOG_BYTES,
        PROBE_TIMEOUT,
        stop,
    )?;
    process::require_success(&command).context("rolling band-burst probe")?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "rolling band-burst probe exceeded its log bound"
    );
    ensure!(
        process::resolve_program(program.name, &program.path)? == *program,
        "RF observatory executable changed during rolling band-burst probing"
    );
    segment.verify_unchanged()?;

    // rf-observatory synchronizes the report and its directory before exit.
    let (value, evidence) = EvidenceFile::read_json(path, REPORT_BYTES)?;
    let parsed: ProbeOutput = serde_json::from_value(value).context("parse band-burst report")?;
    let physical_start = segment.receipt.start_sample;
    let counts = parsed.counts(band, context_bands, source, physical_start)?;
    Ok(BurstReport {
        evidence,
        counts,
        band,
        physical_start,
        detections: parsed.detections,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const CENTER: u64 = 2_437_000_000;
    const BAND: Band = Band {
        lower_frequency_hz: CENTER,
        upper_frequency_hz_exclusive: CENTER + 1,
    };

    fn fixture() -> (SourceReport, Value) {
        let source = SourceReport {
            input: "iq.sc16q11".into(),
            sidecar: "iq.sc16q11.json".into(),
            format: NORMALIZED_SC16_Q11_FORMAT,
            center_frequency_hz: CENTER,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bandwidth_hz: SAMPLE_RATE_HZ,
            complex_samples: 8223,
            bytes: 8223 * 4,
            sha256: "a".repeat(64),
            sidecar_sha256: "b".repeat(64),
            continuity_guaranteed: true,
        };
        let value = json!({
            "schema_version": 1,
            "operation": "probe_saved_iq_band_bursts",
            "source_sha256": source.sha256,
            "source_complex_samples": 8223,
            "source_sample_rate_hz": SAMPLE_RATE_HZ,
            "center_frequency_hz": CENTER,
            "lower_frequency_hz": CENTER,
            "upper_frequency_hz_exclusive": CENTER + 1,
            "window_samples": 1024,
            "integrated_fft_bins": 1,
            "baseline_scope": "whole_extent",
            "baseline_end_sample_exclusive": 8192,
            "baseline_band_power": 0.15,
            "search_start_sample": 0,
            "analyzed_end_sample_exclusive": 8192,
            "omitted_trailing_samples": 31,
            "threshold_db": 6.0,
            "minimum_windows": 2,
            "detections": [
                {"start_sample": 1024, "end_sample_exclusive": 3072, "peak_excess_db": 6.5},
                {"start_sample": 4096, "end_sample_exclusive": 6144, "peak_excess_db": 10.0}
            ],
        });
        (source, value)
    }

    #[test]
    fn accepts_only_ordered_in_band_edges_with_a_bin() -> Result<()> {
        let parsed: Band = "2437000000:2437000001".parse()?;
        parsed.verify(CENTER)?;
        assert!("4:4".parse::<Band>().is_err());
        assert!("5:4".parse::<Band>().is_err());
        assert!("4:5:6".parse::<Band>().is_err());
        assert!(
            "2437000001:2437000002"
                .parse::<Band>()?
                .verify(CENTER)
                .is_err()
        );
        assert!(
            "2426000000:2437000001"
                .parse::<Band>()?
                .verify(CENTER)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn counts_half_open_overlap_on_the_stream_grid() -> Result<()> {
        let (source, value) = fixture();
        let temp = tempfile::tempdir()?;
        let path = temp.path().join(REPORT_NAME);
        std::fs::write(&path, serde_json::to_vec(&value)?)?;
        let (value, evidence) = EvidenceFile::read_json(path, REPORT_BYTES)?;
        let parsed: ProbeOutput = serde_json::from_value(value)?;
        let physical_start = 10_000;
        let mut report = BurstReport {
            evidence,
            counts: parsed.counts(BAND, &[], &source, physical_start)?,
            band: BAND,
            physical_start,
            detections: parsed.detections,
        };
        let unobserved = report.counts();
        assert_eq!(unobserved.overlapping_validated_wifi, None);
        let frames = [10_000..11_024, 13_072..15_120];
        report.record_wifi_spans(Some(&frames));
        let observed = report.counts();
        assert_eq!(
            observed,
            BurstCounts {
                segments: 1,
                bursts: 2,
                overlapping_validated_wifi: Some(1),
            }
        );
        report.record_wifi_spans(Some(&[]));
        assert_eq!(report.counts().overlapping_validated_wifi, Some(0));
        let mixed = BurstCounts::total([observed, unobserved].into_iter()).unwrap();
        assert_eq!(mixed.bursts, 4);
        assert_eq!(mixed.overlapping_validated_wifi, None);
        Ok(())
    }

    #[test]
    fn rejects_wrong_identity_partial_search_and_touching_detections() -> Result<()> {
        let (source, valid) = fixture();
        for (name, incorrect) in [
            ("source_sha256", json!("b".repeat(64))),
            ("baseline_scope", json!("prefix")),
            ("search_start_sample", json!(1024)),
            ("integrated_fft_bins", json!(2)),
            ("baseline_band_power", json!(0.0)),
        ] {
            let mut value = valid.clone();
            value[name] = incorrect;
            let report: ProbeOutput = serde_json::from_value(value)?;
            assert!(report.counts(BAND, &[], &source, 0).is_err(), "{name}");
        }
        let mut touching = valid;
        touching["detections"][1]["start_sample"] = json!(3072);
        let report: ProbeOutput = serde_json::from_value(touching)?;
        assert!(report.counts(BAND, &[], &source, 0).is_err());
        Ok(())
    }

    #[test]
    fn preflight_requires_disjoint_context_bands_within_the_sampled_band() -> Result<()> {
        let adjacent = Band {
            lower_frequency_hz: CENTER + 1,
            upper_frequency_hz_exclusive: CENTER + 40_000,
        };
        BAND.verify_context(&[adjacent], CENTER)?;
        for invalid in [
            vec![BAND],
            vec![adjacent, adjacent],
            vec![adjacent; MAX_CONTEXT_BANDS + 1],
            vec![Band {
                lower_frequency_hz: CENTER + 10_000_000,
                upper_frequency_hz_exclusive: CENTER + 10_000_001,
            }],
            vec![Band {
                lower_frequency_hz: CENTER + 1,
                upper_frequency_hz_exclusive: CENTER + 2,
            }],
        ] {
            assert!(BAND.verify_context(&invalid, CENTER).is_err());
        }
        Ok(())
    }

    #[test]
    fn context_reports_bind_ordered_bands_and_canonical_window_bitmaps() -> Result<()> {
        let (mut source, mut value) = fixture();
        source.complex_samples += WINDOW_SAMPLES;
        source.bytes += 4 * WINDOW_SAMPLES;
        value["source_complex_samples"] = json!(source.complex_samples);
        value["analyzed_end_sample_exclusive"] = json!(9 * WINDOW_SAMPLES);
        value["baseline_end_sample_exclusive"] = json!(9 * WINDOW_SAMPLES);
        let legacy: ProbeOutput = serde_json::from_value(value.clone())?;
        let context = [
            Band {
                lower_frequency_hz: CENTER + 1,
                upper_frequency_hz_exclusive: CENTER + 20_000,
            },
            Band {
                lower_frequency_hz: CENTER - 20_000,
                upper_frequency_hz_exclusive: CENTER,
            },
        ];
        let old_counts = legacy.counts(BAND, &[], &source, 0)?;
        assert!(legacy.counts(BAND, &context, &source, 0).is_err());
        for additional in [json!([]), Value::Null] {
            let mut unexpected = value.clone();
            unexpected["additional_bands"] = additional;
            assert!(
                serde_json::from_value::<ProbeOutput>(unexpected)
                    .and_then(|report| {
                        report
                            .counts(BAND, &[], &source, 0)
                            .map_err(serde::de::Error::custom)
                    })
                    .is_err()
            );
        }

        value["schema_version"] = json!(2);
        value["additional_bands"] = json!([
            {
                "lower_frequency_hz": context[0].lower_frequency_hz,
                "upper_frequency_hz_exclusive": context[0].upper_frequency_hz_exclusive,
                "integrated_fft_bins": 1,
                "baseline_band_power": 0.2,
                "above_threshold_windows_hex": "a501"
            },
            {
                "lower_frequency_hz": context[1].lower_frequency_hz,
                "upper_frequency_hz_exclusive": context[1].upper_frequency_hz_exclusive,
                "integrated_fft_bins": 1,
                "baseline_band_power": 0.0,
                "above_threshold_windows_hex": "0000"
            }
        ]);
        let report: ProbeOutput = serde_json::from_value(value.clone())?;
        assert_eq!(report.counts(BAND, &context, &source, 0)?, old_counts);
        assert!(report.counts(BAND, &context[..1], &source, 0).is_err());
        assert!(
            report
                .counts(BAND, &[context[1], context[0]], &source, 0)
                .is_err()
        );
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove("additional_bands");
        let missing: ProbeOutput = serde_json::from_value(missing)?;
        assert!(missing.counts(BAND, &context, &source, 0).is_err());

        for (field, wrong) in [
            ("lower_frequency_hz", json!(CENTER + 2)),
            ("integrated_fft_bins", json!(2)),
            ("baseline_band_power", json!(0.0)),
            ("baseline_band_power", json!(-1.0)),
            ("above_threshold_windows_hex", json!("A501")),
            ("above_threshold_windows_hex", json!("a5")),
            ("above_threshold_windows_hex", json!("a502")),
        ] {
            let mut invalid = value.clone();
            invalid["additional_bands"][0][field] = wrong;
            let report: ProbeOutput = serde_json::from_value(invalid)?;
            assert!(
                report.counts(BAND, &context, &source, 0).is_err(),
                "{field}"
            );
        }
        Ok(())
    }
}
