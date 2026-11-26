//! Plan bounded LTE raster replay from admitted sweep geometry.

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::lte::raster::{
    GRID_SPACING_HZ, MAX_CHANNELIZED_SAMPLES, MIN_SWEEP_SAMPLES, channelized_count,
    covered_centers, validate_endpoints,
};

use super::super::admission::{PassRef, Tune};

/// A consecutive range of carrier centers on the inclusive raster.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct FrequencyRange {
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Coverage {
    Uncovered,
    Short,
    Eligible,
}

/// The requested carrier centers and IQ prefix intersecting one dwell.
pub(super) struct Dwell<'a> {
    pub(super) pass_index: usize,
    pub(super) pass_directory: Option<&'a str>,
    pub(super) source_sweep_sha256: &'a str,
    pub(super) tune: &'a Tune,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) selected_samples: u64,
}

/// All intersecting dwells, coverage gaps, and replay work before IQ reads.
pub(super) struct Plan<'a> {
    pub(super) dwells: Vec<Dwell<'a>>,
    pub(super) uncovered_ranges: Vec<FrequencyRange>,
    pub(super) short_only_ranges: Vec<FrequencyRange>,
    pub(super) replayed_iq_samples: u64,
    pub(super) planned_channelized_samples: u64,
    pub(super) carrier_attempts: usize,
}

impl<'a> Plan<'a> {
    /// Compute coverage from observed tuning centers and charge only eligible IQ.
    ///
    /// # Errors
    /// Rejects invalid raster endpoints or work beyond the dwell or total caps.
    pub(super) fn build(
        passes: &[PassRef<'a>],
        lower_hz: u64,
        upper_hz: u64,
        max_samples_per_dwell: u64,
        max_total_channelized_samples: u64,
    ) -> Result<Self> {
        validate_endpoints(lower_hz, upper_hz)?;
        let mut coverage = vec![Coverage::Uncovered; raster_index(lower_hz, upper_hz) + 1];
        let mut dwells = Vec::new();
        let mut replayed_iq_samples = 0;
        let mut planned_channelized_samples = 0;
        let mut carrier_attempts = 0;
        for (index, pass) in passes.iter().enumerate() {
            let pass_index = index + 1;
            for tune in &pass.sweep.tunes {
                let Some((first_hz, last_hz)) =
                    covered_centers(tune.observed_center_hz, lower_hz, upper_hz)
                else {
                    continue;
                };
                let selected_samples = tune.samples.min(max_samples_per_dwell);
                let span = &mut coverage
                    [raster_index(lower_hz, first_hz)..=raster_index(lower_hz, last_hz)];
                let status = if selected_samples < MIN_SWEEP_SAMPLES {
                    Coverage::Short
                } else {
                    let samples = channelized_count(selected_samples) as u64 * span.len() as u64;
                    ensure!(
                        samples <= MAX_CHANNELIZED_SAMPLES as u64,
                        "LTE sweep pass {pass_index} tune {} needs {samples} channelized samples, exceeding the {MAX_CHANNELIZED_SAMPLES}-sample per-dwell limit. Reduce --max-samples-per-dwell or narrow the range",
                        tune.directory
                    );
                    planned_channelized_samples += samples;
                    replayed_iq_samples += selected_samples;
                    carrier_attempts += span.len();
                    Coverage::Eligible
                };
                for carrier in span {
                    *carrier = (*carrier).max(status);
                }
                dwells.push(Dwell {
                    pass_index,
                    pass_directory: pass.directory,
                    source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                    tune,
                    lower_hz: first_hz,
                    upper_hz: last_hz,
                    selected_samples,
                });
            }
        }
        ensure!(
            planned_channelized_samples <= max_total_channelized_samples,
            "LTE sweep replay needs {planned_channelized_samples} channelized samples, exceeding --max-total-channelized-samples ({max_total_channelized_samples})"
        );
        let (uncovered_ranges, short_only_ranges) = coverage_gaps(lower_hz, &coverage);
        Ok(Self {
            dwells,
            uncovered_ranges,
            short_only_ranges,
            replayed_iq_samples,
            planned_channelized_samples,
            carrier_attempts,
        })
    }
}

fn raster_index(origin_hz: u64, frequency_hz: u64) -> usize {
    ((frequency_hz - origin_hz) / GRID_SPACING_HZ) as usize
}

fn coverage_gaps(
    mut lower_hz: u64,
    coverage: &[Coverage],
) -> (Vec<FrequencyRange>, Vec<FrequencyRange>) {
    let mut uncovered = Vec::new();
    let mut short_only = Vec::new();
    for group in coverage.chunk_by(|left, right| left == right) {
        let upper_hz = lower_hz + (group.len() - 1) as u64 * GRID_SPACING_HZ;
        let range = FrequencyRange { lower_hz, upper_hz };
        match group[0] {
            Coverage::Uncovered => uncovered.push(range),
            Coverage::Short => short_only.push(range),
            Coverage::Eligible => {}
        }
        lower_hz = upper_hz + GRID_SPACING_HZ;
    }
    (uncovered, short_only)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs::{self, File};
    use std::path::Path;

    use serde_json::json;

    use super::super::super::admission::Sweep;
    use super::*;
    use crate::sweep::plan::observed_bands;

    fn admitted_sweep(root: &Path, requested_hz: u64, observed_hz: u64, dwell_ms: u64) -> Sweep {
        let tune = root.join("tune-0001");
        fs::create_dir_all(&tune).unwrap();
        let samples = dwell_ms * u64::from(crate::SAMPLE_RATE) / 1_000;
        File::create(tune.join("wideband.sc16q11"))
            .unwrap()
            .set_len(4 * samples)
            .unwrap();
        fs::write(
            tune.join("wideband.sc16q11.json"),
            json!({
                "format": "SC16_Q11",
                "dtype": "<i2",
                "interleave": "I,Q",
                "complex_scale": 2048,
                "frequency_hz": observed_hz,
                "sample_rate_hz": crate::SAMPLE_RATE,
                "bandwidth_hz": crate::SAMPLE_RATE,
                "gain_db": 40,
                "gain_mode": "manual",
                "bias_tee_enabled": false,
                "channel": "RX1",
                "num_samples": samples,
                "started_utc": "2000-01-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            root.join("sweep.json"),
            json!({
                "schema_version": 1,
                "operation": "targeted_rf_sweep",
                "lower_hz": requested_hz,
                "upper_hz": requested_hz + 1,
                "antenna": 1,
                "gain_db": 40,
                "dwell_ms": dwell_ms,
                "iq_bytes": 4 * samples,
                "tunes": [{
                    "directory": "tune-0001",
                    "requested_center_hz": requested_hz,
                    "observed_center_hz": observed_hz,
                    "observed_bands": observed_bands(observed_hz, 0),
                }],
            })
            .to_string(),
        )
        .unwrap();
        Sweep::open(root).unwrap()
    }

    #[test]
    fn distinguishes_uncovered_short_only_and_eligible_grid_centers() {
        let root = tempfile::tempdir().unwrap();
        let ready = admitted_sweep(&root.path().join("ready"), 900_000_000, 900_000_007, 200);
        let short = admitted_sweep(&root.path().join("short"), 908_000_000, 908_000_000, 1);
        let passes = [
            PassRef {
                directory: Some("pass-0001"),
                sweep: &ready,
            },
            PassRef {
                directory: Some("pass-0002"),
                sweep: &short,
            },
        ];
        let selected = MIN_SWEEP_SAMPLES + 23;
        let work = 140 * channelized_count(selected) as u64;
        let plan = Plan::build(&passes, 892_900_000, 915_100_000, selected, work).unwrap();
        assert_eq!(
            plan.uncovered_ranges,
            [
                FrequencyRange {
                    lower_hz: 892_900_000,
                    upper_hz: 893_000_000
                },
                FrequencyRange {
                    lower_hz: 915_100_000,
                    upper_hz: 915_100_000
                },
            ]
        );
        assert_eq!(
            plan.short_only_ranges,
            [FrequencyRange {
                lower_hz: 907_100_000,
                upper_hz: 915_000_000
            }]
        );
        assert_eq!(plan.dwells.len(), 2);
        let [first, second] = plan.dwells.as_slice() else {
            unreachable!()
        };
        assert_eq!(first.pass_index, 1);
        assert_eq!(first.pass_directory, Some("pass-0001"));
        assert_eq!(first.source_sweep_sha256, ready.source_receipt_sha256);
        assert_eq!(first.tune.observed_center_hz, 900_000_007);
        assert_eq!(
            (first.lower_hz, first.upper_hz, first.selected_samples),
            (893_100_000, 907_000_000, selected)
        );
        assert_eq!(second.pass_index, 2);
        assert_eq!(second.selected_samples, short.tunes[0].samples);
        assert_eq!(plan.replayed_iq_samples, selected);
        assert_eq!(plan.carrier_attempts, 140);
        assert_eq!(plan.planned_channelized_samples, work);

        let total = Plan::build(&passes, 892_900_000, 915_100_000, selected, work - 1)
            .err()
            .unwrap();
        assert!(
            total
                .to_string()
                .contains("--max-total-channelized-samples")
        );
        let dwell = Plan::build(
            &passes,
            892_900_000,
            915_100_000,
            ready.tunes[0].samples,
            u64::MAX,
        )
        .err()
        .unwrap();
        assert!(dwell.to_string().contains("per-dwell"));
    }

    #[test]
    fn preserves_singleton_uncovered_range_and_rejects_off_grid_endpoints() {
        let plan = Plan::build(&[], 915_000_000, 915_000_000, 1, 1).unwrap();
        assert_eq!(
            plan.uncovered_ranges,
            [FrequencyRange {
                lower_hz: 915_000_000,
                upper_hz: 915_000_000
            }]
        );
        assert!(plan.short_only_ranges.is_empty());
        assert!(Plan::build(&[], 915_000_001, 915_100_000, 1, 1).is_err());
    }
}
