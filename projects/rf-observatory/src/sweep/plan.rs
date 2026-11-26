//! Frequency coverage and exact-center plans for stock-image acquisition.

mod bands;

use std::collections::BTreeMap;
use std::ops::Range;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::{CENTER_GUARD_HZ, USABLE_HALF_SPAN_HZ};
use crate::triage::replay::preflight::Preflight;
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE};

use super::packet_preflight::PacketPreflight;
use super::series_follow_up::SeriesFollowUp;
use super::series_follow_up::activity::FollowUp as ActivityFollowUp;
use super::triage_follow_up::FollowUp;

const PAIR_SPACING_HZ: u64 = 7_800_000;
pub(crate) const MAX_DWELL_MS: u64 = 10_000;
pub(crate) const MAX_TUNES: usize = 500;
pub(crate) const MAX_PASSES: usize = 16;
pub(crate) const MAX_TOTAL_DWELLS: usize = 2_000;
/// Maximum number of band arguments accepted for a sparse sweep or coverage report.
pub(crate) const MAX_REQUESTED_BANDS: usize = 32;
const BYTES_PER_SAMPLE: u64 = 4;
const SAMPLES_PER_MS: u64 = SAMPLE_RATE as u64 / 1_000;

/// An exact-center override of the sweep's default dwell duration.
#[derive(Clone, Copy, Debug)]
pub(super) struct DwellOverride {
    center_hz: u64,
    dwell_ms: u64,
}

impl DwellOverride {
    pub(super) fn new(center_hz: u64, dwell_ms: u64) -> Result<Self> {
        ensure!(
            (1..=MAX_DWELL_MS).contains(&dwell_ms),
            "sweep dwell must be 1..={MAX_DWELL_MS} milliseconds"
        );
        Ok(Self {
            center_hz,
            dwell_ms,
        })
    }

    /// Parse a center in hertz and an integer duration in milliseconds.
    pub(super) fn parse(value: &str) -> Result<Self> {
        let (center, dwell) = value.split_once(':').context("expected CENTER_HZ:MS")?;
        let center_hz = center.parse().context("invalid override center in hertz")?;
        let dwell_ms = dwell
            .parse()
            .context("invalid sweep dwell in milliseconds")?;
        Self::new(center_hz, dwell_ms)
    }
}

fn parse_dwell_ms(value: &str) -> Result<u64> {
    let dwell_ms = value
        .parse()
        .context("invalid sweep dwell in milliseconds")?;
    ensure!(
        (1..=MAX_DWELL_MS).contains(&dwell_ms),
        "sweep dwell must be 1..={MAX_DWELL_MS} milliseconds"
    );
    Ok(dwell_ms)
}

/// Half-open frequency bounds in integer hertz.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub(crate) struct Band {
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
}

impl Band {
    /// Create a nonempty half-open band in hertz.
    ///
    /// # Errors
    /// Rejects a zero lower bound or non-increasing bounds.
    pub(crate) fn new(lower_hz: u64, upper_hz: u64) -> Result<Self> {
        let band = Self { lower_hz, upper_hz };
        ensure!(
            band.is_valid_request(),
            "band must satisfy 0 < LOWER_HZ < UPPER_HZ"
        );
        Ok(band)
    }

    /// Parse inclusive lower and exclusive upper hertz bounds separated by a colon.
    ///
    /// # Errors
    /// Rejects a missing, non-integer, zero lower, or non-increasing bound.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER_HZ:UPPER_HZ")?;
        let lower_hz = lower.parse().context("invalid lower frequency in hertz")?;
        let upper_hz = upper.parse().context("invalid upper frequency in hertz")?;
        Self::new(lower_hz, upper_hz)
    }

    /// Return the inclusive lower and exclusive upper bounds in hertz.
    pub(crate) fn bounds(self) -> Range<u64> {
        self.lower_hz..self.upper_hz
    }

    fn is_valid_request(self) -> bool {
        self.lower_hz > 0 && self.lower_hz < self.upper_hz
    }

    /// Whether this span contains the entire nonempty half-open interval.
    pub(crate) fn contains_interval(self, lower_hz: u64, upper_hz: u64) -> bool {
        self.lower_hz <= lower_hz && lower_hz < upper_hz && upper_hz <= self.upper_hz
    }

    /// Whether one observed span contains the complete requested band.
    pub(crate) fn is_covered_by(self, spans: impl IntoIterator<Item = Self>) -> bool {
        spans
            .into_iter()
            .any(|span| span.contains_interval(self.lower_hz, self.upper_hz))
    }
}

/// Requested band and optional minimum saved time in each pass.
#[derive(Clone, Copy, Debug)]
pub(super) struct BandRequest {
    band: Band,
    minimum_dwell_ms: Option<u64>,
}

impl BandRequest {
    /// Parse `LOWER_HZ:UPPER_HZ` with an optional trailing millisecond minimum.
    pub(super) fn parse(value: &str) -> Result<Self> {
        let (bounds, minimum_dwell_ms) = value
            .rsplit_once(':')
            .filter(|(bounds, _)| bounds.contains(':'))
            .map_or(Ok((value, None)), |(bounds, dwell)| {
                parse_dwell_ms(dwell).map(|dwell| (bounds, Some(dwell)))
            })?;
        Ok(Self {
            band: Band::parse(bounds)?,
            minimum_dwell_ms,
        })
    }
}

/// Minimum dwell duration for a complete requested band.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct BandDwell {
    #[serde(flatten)]
    band: Band,
    minimum_dwell_ms: u64,
}

/// Guarded dwells inserted before each group of background visits.
#[derive(Debug, Serialize)]
pub(super) struct Priority {
    bands: Vec<BandDwell>,
    pub(super) centers_hz: Vec<u64>,
    pub(super) revisit_every: usize,
}

impl Priority {
    pub(super) fn verify(
        &self,
        dwells: impl Iterator<Item = ([Band; 2], u64)> + Clone,
    ) -> Result<()> {
        for request in &self.bands {
            verify_band_dwell(request.band, request.minimum_dwell_ms, dwells.clone())?;
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
enum Operation {
    #[serde(rename = "sequential_rf_sweep_plan")]
    Contiguous,
    #[serde(rename = "targeted_rf_sweep_plan")]
    Targeted,
}

#[derive(Debug, Serialize)]
pub(super) struct Plan {
    schema_version: u32,
    operation: Operation,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) dwell_ms: u64,
    pub(super) samples_per_tune: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) dwell_ms_by_center: BTreeMap<u64, u64>,
    pub(super) planned_iq_bytes: u64,
    pub(super) centers_hz: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) visit_centers_hz: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) priority: Option<Priority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) requested_bands: Option<Vec<Band>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) requested_band_dwells: Option<Vec<BandDwell>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    passes: Option<usize>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) check_continuity: bool,
    sample_rate_hz: u32,
    max_iq_bytes: u64,
    activity_half_span_hz: u64,
    center_guard_hz: u64,
    center_readback_tolerance_hz: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signal_profile_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) replay_preflight: Option<Preflight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) packet_decode_preflight: Option<PacketPreflight>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) follow_up_plan_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_sweep_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_triage_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) triage_follow_up: Option<FollowUp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_series_triage_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_series_triage_overlap_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_series_activity_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source_series_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) series_follow_up: Option<SeriesFollowUp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) activity_follow_up: Option<ActivityFollowUp>,
}

impl Plan {
    fn new(
        operation: Operation,
        lower_hz: u64,
        upper_hz: u64,
        dwell_ms: u64,
        max_iq_bytes: u64,
        centers_hz: Vec<u64>,
        overrides: &[DwellOverride],
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_DWELL_MS).contains(&dwell_ms),
            "sweep dwell must be 1..={MAX_DWELL_MS} milliseconds"
        );
        ensure!(
            (1..=MAX_TUNES).contains(&centers_hz.len()),
            "sweep requires 1 through {MAX_TUNES} unique centers"
        );
        let mut dwell_ms_by_center = BTreeMap::new();
        for override_dwell in overrides {
            let center = override_dwell.center_hz;
            ensure!(
                centers_hz.binary_search(&center).is_ok(),
                "dwell override center {center} Hz is absent from the sweep"
            );
            ensure!(
                dwell_ms_by_center
                    .insert(center, override_dwell.dwell_ms)
                    .is_none(),
                "duplicate dwell override at {center} Hz"
            );
        }
        dwell_ms_by_center.retain(|_, override_ms| *override_ms != dwell_ms);
        let total_ms: u64 = centers_hz
            .iter()
            .map(|center| dwell_ms_by_center.get(center).copied().unwrap_or(dwell_ms))
            .sum();
        let planned_iq_bytes = total_ms * SAMPLES_PER_MS * BYTES_PER_SAMPLE;
        ensure!(
            planned_iq_bytes <= max_iq_bytes,
            "sweep plan needs {planned_iq_bytes} IQ bytes, exceeding the {max_iq_bytes}-byte budget"
        );
        Ok(Self {
            schema_version: 1,
            operation,
            lower_hz,
            upper_hz,
            dwell_ms,
            samples_per_tune: dwell_ms * SAMPLES_PER_MS,
            dwell_ms_by_center,
            planned_iq_bytes,
            centers_hz,
            visit_centers_hz: None,
            priority: None,
            requested_bands: None,
            requested_band_dwells: None,
            passes: None,
            check_continuity: false,
            sample_rate_hz: SAMPLE_RATE,
            max_iq_bytes,
            activity_half_span_hz: USABLE_HALF_SPAN_HZ as u64,
            center_guard_hz: CENTER_GUARD_HZ as u64,
            center_readback_tolerance_hz: ACQUISITION_CENTER_TOLERANCE_HZ,
            signal_profile_sha256: None,
            replay_preflight: None,
            packet_decode_preflight: None,
            follow_up_plan_sha256: None,
            source_sweep_sha256: None,
            source_triage_sha256: None,
            triage_follow_up: None,
            source_series_triage_sha256: None,
            source_series_triage_overlap_sha256: None,
            source_series_activity_sha256: None,
            source_series_sha256: None,
            series_follow_up: None,
            activity_follow_up: None,
        })
    }

    /// A targeted plan records each tune without claiming contiguous coverage.
    pub(super) fn is_targeted(&self) -> bool {
        matches!(self.operation, Operation::Targeted)
    }

    pub(super) fn passes(&self) -> usize {
        self.passes.unwrap_or(1)
    }

    /// Ordered centers for one pass, including every priority revisit.
    pub(super) fn visits(&self) -> &[u64] {
        self.visit_centers_hz.as_deref().unwrap_or(&self.centers_hz)
    }

    /// Insert packed priority dwells while retaining the original guarded union.
    pub(super) fn with_priority(
        mut self,
        requested: &[BandRequest],
        revisit_every: usize,
    ) -> Result<Self> {
        ensure!(
            (1..=MAX_TUNES).contains(&revisit_every),
            "--revisit-every must be 1 through {MAX_TUNES} background visits"
        );
        let bands = band_requirements(requested, self.dwell_ms)?;
        let schedule = bands::schedule(&bands)?;
        let centers_hz: Vec<_> = schedule.iter().map(|&(center, _)| center).collect();
        let background: Vec<_> = self
            .centers_hz
            .iter()
            .copied()
            .filter(|center| centers_hz.binary_search(center).is_err())
            .collect();
        let groups = background.len().div_ceil(revisit_every).max(1);
        let count = background.len() + centers_hz.len() * groups;
        ensure!(
            count <= MAX_TUNES,
            "priority schedule requires {count} visits per pass, exceeding {MAX_TUNES}"
        );
        let mut visits = Vec::with_capacity(count);
        for chunk in background.chunks(revisit_every) {
            visits.extend_from_slice(&centers_hz);
            visits.extend_from_slice(chunk);
        }
        if background.is_empty() {
            visits.extend_from_slice(&centers_hz);
        }
        for (center, minimum) in schedule {
            let dwell = if self.centers_hz.binary_search(&center).is_ok() {
                minimum.max(self.dwell_ms)
            } else {
                minimum
            };
            if dwell != self.dwell_ms {
                self.dwell_ms_by_center.insert(center, dwell);
            }
        }
        self.centers_hz.extend_from_slice(&centers_hz);
        self.centers_hz.sort_unstable();
        self.centers_hz.dedup();
        self.planned_iq_bytes = visits
            .iter()
            .map(|&center| self.samples_at(center) * BYTES_PER_SAMPLE)
            .sum();
        self.visit_centers_hz = Some(visits);
        self.priority = Some(Priority {
            bands,
            centers_hz,
            revisit_every,
        });
        Ok(self)
    }

    /// Effective dwell duration at one of the plan's requested centers.
    pub(super) fn dwell_at(&self, center_hz: u64) -> u64 {
        self.dwell_ms_by_center
            .get(&center_hz)
            .copied()
            .unwrap_or(self.dwell_ms)
    }

    /// Exact original-rate sample extent for a planned dwell.
    pub(super) fn samples_at(&self, center_hz: u64) -> u64 {
        self.dwell_at(center_hz) * SAMPLES_PER_MS
    }

    /// Apply the total dwell and IQ limits before capture or directory creation.
    pub(super) fn repeat(mut self, passes: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_PASSES).contains(&passes),
            "sweep requires 1 through {MAX_PASSES} passes"
        );
        ensure!(
            self.visits().len() * passes <= MAX_TOTAL_DWELLS,
            "sweep series requires at most {MAX_TOTAL_DWELLS} dwells"
        );
        let total_bytes = self.planned_iq_bytes * passes as u64;
        ensure!(
            total_bytes <= self.max_iq_bytes,
            "sweep plan needs {total_bytes} IQ bytes, exceeding the {}-byte budget",
            self.max_iq_bytes
        );
        self.passes = (passes > 1).then_some(passes);
        self.planned_iq_bytes = total_bytes;
        Ok(self)
    }
}

/// Pack generated protocol bands with the same dwell and clearance rules as
/// explicit bandwidth requests.
pub(super) fn schedule_profile_bands(bands: &[(Band, u64)]) -> Result<Vec<(u64, u64)>> {
    let mut required = BTreeMap::<Band, u64>::new();
    for &(band, dwell_ms) in bands {
        ensure!(band.is_valid_request(), "invalid signal profile band");
        required
            .entry(band)
            .and_modify(|minimum| *minimum = (*minimum).max(dwell_ms))
            .or_insert(dwell_ms);
    }
    let required: Vec<_> = required
        .into_iter()
        .map(|(band, minimum_dwell_ms)| BandDwell {
            band,
            minimum_dwell_ms,
        })
        .collect();
    bands::schedule(&required)
}

/// Choose the latest legal tuning center for a guarded band.
pub(super) fn preferred_profile_center(band: Band) -> Result<u64> {
    bands::center_ranges(band)
        .into_iter()
        .filter(|range| !range.is_empty())
        .map(|range| *range.end())
        .max()
        .with_context(|| {
            format!(
                "signal profile band {}..{} Hz has no supported guarded tuning",
                band.lower_hz, band.upper_hz
            )
        })
}

/// Half-open spans of complete activity cells at zero margin. A nonzero
/// margin retains their intersection across all accepted center displacements.
pub(crate) fn observed_bands(center_hz: u64, margin_hz: u64) -> [Band; 2] {
    let outer_offset = (USABLE_HALF_SPAN_HZ as u64).saturating_sub(margin_hz);
    let inner_offset = (CENTER_GUARD_HZ as u64).saturating_add(margin_hz);
    [
        Band {
            lower_hz: center_hz.saturating_sub(outer_offset),
            upper_hz: center_hz.saturating_sub(inner_offset),
        },
        Band {
            lower_hz: center_hz.saturating_add(inner_offset),
            upper_hz: center_hz.saturating_add(outer_offset),
        },
    ]
}

/// Whether the union of the spans covers the nonempty half-open request.
pub(crate) fn covers(lower_hz: u64, upper_hz: u64, bands: impl IntoIterator<Item = Band>) -> bool {
    let mut bands: Vec<_> = bands.into_iter().collect();
    bands.sort_unstable_by_key(|band| band.lower_hz);
    let mut covered_to = lower_hz;
    for band in bands {
        if band.lower_hz > covered_to {
            break;
        }
        covered_to = covered_to.max(band.upper_hz);
        if covered_to >= upper_hz {
            return true;
        }
    }
    false
}

pub(super) fn plan(lower_hz: u64, upper_hz: u64, dwell_ms: u64, max_iq_bytes: u64) -> Result<Plan> {
    ensure!(
        lower_hz >= MIN_CENTER_HZ && upper_hz <= MAX_CENTER_HZ && lower_hz < upper_hz,
        "sweep requires an increasing half-open range within {MIN_CENTER_HZ}..={MAX_CENTER_HZ} Hz"
    );
    let margin = ACQUISITION_CENTER_TOLERANCE_HZ;
    let span = USABLE_HALF_SPAN_HZ as u64 - margin;
    let mut centers_hz = Vec::new();
    let mut covered_to = lower_hz;
    while covered_to < upper_hz {
        // Adjacent pairs tile the range. Each tune sees the other tune's
        // guarded center even at opposite readback extremes.
        let first = (covered_to + span).min(MAX_CENTER_HZ - PAIR_SPACING_HZ);
        centers_hz.push(first);
        if covers(covered_to, upper_hz, observed_bands(first, margin)) {
            break;
        }
        let second = first + PAIR_SPACING_HZ;
        centers_hz.push(second);
        covered_to = second + span;
    }
    // Clamping the final pair to the tuning limit can bring its first center
    // back into the preceding pair.
    centers_hz.sort_unstable();
    centers_hz.dedup();
    ensure!(
        covers(
            lower_hz,
            upper_hz,
            centers_hz
                .iter()
                .flat_map(|&center| observed_bands(center, margin))
        ),
        "sweep plan leaves a gap in the requested range"
    );
    Plan::new(
        Operation::Contiguous,
        lower_hz,
        upper_hz,
        dwell_ms,
        max_iq_bytes,
        centers_hz,
        &[],
    )
}

/// Plan one dwell per distinct requested center in ascending tuning order.
/// The half-open bounds enclose the requested centers.
pub(super) fn targeted(centers: &[u64], dwell_ms: u64, max_iq_bytes: u64) -> Result<Plan> {
    targeted_with_dwells(centers, dwell_ms, max_iq_bytes, &[])
}

/// Plan sparse dwells and optional millisecond overrides with exact byte accounting.
pub(super) fn targeted_with_dwells(
    centers: &[u64],
    dwell_ms: u64,
    max_iq_bytes: u64,
    overrides: &[DwellOverride],
) -> Result<Plan> {
    let mut centers_hz = centers.to_vec();
    centers_hz.sort_unstable();
    centers_hz.dedup();
    let (&lower_hz, &highest_center_hz) = centers_hz
        .first()
        .zip(centers_hz.last())
        .context("targeted sweep requires at least one center")?;
    ensure!(
        lower_hz >= MIN_CENTER_HZ && highest_center_hz <= MAX_CENTER_HZ,
        "targeted sweep centers must be within {MIN_CENTER_HZ}..={MAX_CENTER_HZ} Hz"
    );
    Plan::new(
        Operation::Targeted,
        lower_hz,
        highest_center_hz + 1,
        dwell_ms,
        max_iq_bytes,
        centers_hz,
        overrides,
    )
}

/// Choose tunings that cover each requested band in a single dwell.
pub(super) fn targeted_bands(
    requested: &[BandRequest],
    dwell_ms: u64,
    max_iq_bytes: u64,
) -> Result<Plan> {
    let requested = band_requirements(requested, dwell_ms)?;
    let schedule = bands::schedule(&requested)?;
    let requested_bands: Vec<_> = requested.iter().map(|request| request.band).collect();
    let requested_band_dwells: Vec<_> = requested
        .into_iter()
        .filter(|request| request.minimum_dwell_ms != dwell_ms)
        .collect();
    verify_band_dwells(
        &requested_bands,
        &requested_band_dwells,
        dwell_ms,
        schedule.iter().map(|&(center, dwell)| {
            (
                observed_bands(center, ACQUISITION_CENTER_TOLERANCE_HZ),
                dwell,
            )
        }),
    )?;
    let centers: Vec<_> = schedule.iter().map(|&(center, _)| center).collect();
    let overrides: Vec<_> = schedule
        .into_iter()
        .filter(|&(_, dwell)| dwell != dwell_ms)
        .map(|(center_hz, dwell_ms)| DwellOverride {
            center_hz,
            dwell_ms,
        })
        .collect();
    let mut plan = targeted_with_dwells(&centers, dwell_ms, max_iq_bytes, &overrides)?;
    plan.requested_bands = Some(requested_bands);
    plan.requested_band_dwells =
        (!requested_band_dwells.is_empty()).then_some(requested_band_dwells);
    Ok(plan)
}

fn band_requirements(requested: &[BandRequest], dwell_ms: u64) -> Result<Vec<BandDwell>> {
    ensure!(
        (1..=MAX_REQUESTED_BANDS).contains(&requested.len()),
        "sweep requires 1 through {MAX_REQUESTED_BANDS} band arguments"
    );
    let mut minimums: BTreeMap<Band, u64> = BTreeMap::new();
    for request in requested {
        let minimum = request.minimum_dwell_ms.unwrap_or(dwell_ms);
        minimums
            .entry(request.band)
            .and_modify(|previous| *previous = (*previous).max(minimum))
            .or_insert(minimum);
    }
    Ok(minimums
        .into_iter()
        .map(|(band, minimum_dwell_ms)| BandDwell {
            band,
            minimum_dwell_ms,
        })
        .collect())
}

/// Sort and deduplicate band arguments after applying their input count limit.
///
/// # Errors
/// Returns an error when the input is empty, exceeds the limit, or contains
/// an invalid band.
pub(crate) fn normalize_bands(requested: &mut Vec<Band>) -> Result<()> {
    ensure!(
        requested.len() <= MAX_REQUESTED_BANDS,
        "at most {MAX_REQUESTED_BANDS} bands may be requested"
    );
    requested.sort_unstable();
    requested.dedup();
    validate_requested_bands(requested)
}

/// Validate nonempty, strictly sorted, unique requested bands from a receipt.
pub(crate) fn validate_requested_bands(requested: &[Band]) -> Result<()> {
    ensure!(
        (1..=MAX_REQUESTED_BANDS).contains(&requested.len())
            && requested.iter().all(|band| band.is_valid_request())
            && requested.is_sorted_by(|left, right| left < right),
        "requested bands must contain 1 through {MAX_REQUESTED_BANDS} strictly ordered valid intervals"
    );
    Ok(())
}

/// Require a single complete span and minimum saved duration for each band.
///
/// The requested bands must be sorted and unique.
///
/// # Errors
/// Rejects invalid duration overrides or a band missing a qualifying dwell.
pub(crate) fn verify_band_dwells(
    requested: &[Band],
    requested_dwells: &[BandDwell],
    default_ms: u64,
    dwells: impl Iterator<Item = ([Band; 2], u64)> + Clone,
) -> Result<()> {
    ensure!(
        (1..=MAX_DWELL_MS).contains(&default_ms),
        "sweep dwell must be 1..={MAX_DWELL_MS} milliseconds"
    );
    ensure!(
        requested_dwells.is_sorted_by(|left, right| left.band < right.band)
            && requested_dwells.iter().all(|request| {
                (1..=MAX_DWELL_MS).contains(&request.minimum_dwell_ms)
                    && request.minimum_dwell_ms != default_ms
                    && requested.binary_search(&request.band).is_ok()
            }),
        "sweep band dwell overrides must name distinct requested bands in order with valid nondefault durations"
    );
    for &band in requested {
        let minimum = requested_dwells
            .binary_search_by_key(&band, |request| request.band)
            .map_or(default_ms, |index| requested_dwells[index].minimum_dwell_ms);
        verify_band_dwell(band, minimum, dwells.clone())?;
    }
    Ok(())
}

fn verify_band_dwell(
    band: Band,
    minimum: u64,
    mut dwells: impl Iterator<Item = ([Band; 2], u64)>,
) -> Result<()> {
    ensure!(
        dwells.any(|(spans, dwell)| dwell >= minimum && band.is_covered_by(spans)),
        "no single dwell covers the complete requested band {}..{} Hz for {minimum} ms",
        band.lower_hz,
        band.upper_hz
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_uses_half_open_bounds_and_accepts_unordered_spans() {
        let left = Band {
            lower_hz: 0,
            upper_hz: 20,
        };
        let right = Band {
            lower_hz: 20,
            upper_hz: 40,
        };
        assert!(covers(10, 40, [right, left]));
        assert!(!covers(10, 41, [right, left]));
        assert!(!covers(
            10,
            40,
            [
                Band {
                    upper_hz: 19,
                    ..left
                },
                right
            ]
        ));
    }

    #[test]
    fn offsets_cover_both_centers_after_opposite_readback_shifts() {
        let center = 100_000_000;
        let offset = center + PAIR_SPACING_HZ;
        let margin = ACQUISITION_CENTER_TOLERANCE_HZ;
        let outer = USABLE_HALF_SPAN_HZ as u64 - margin;
        assert!(!covers(center - 1, center + 1, observed_bands(center, 0)));
        for (first, second) in [
            (center + margin, offset - margin),
            (center - margin, offset + margin),
        ] {
            assert!(covers(
                center - outer,
                offset + outer,
                [first, second]
                    .into_iter()
                    .flat_map(|center| observed_bands(center, 0))
            ));
        }
    }

    #[test]
    fn band_edges_and_single_hertz_requests_have_full_coverage() {
        for (lower, upper) in [
            (MIN_CENTER_HZ, MIN_CENTER_HZ + 1),
            (MIN_CENTER_HZ, MIN_CENTER_HZ + 16_000_000),
            (MAX_CENTER_HZ - 1, MAX_CENTER_HZ),
            (MAX_CENTER_HZ - 16_000_000, MAX_CENTER_HZ),
        ] {
            let result = plan(lower, upper, 1, 320_000).unwrap();
            assert!(
                result
                    .centers_hz
                    .iter()
                    .all(|center| (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(center))
            );
            assert!(covers(
                lower,
                upper,
                result.centers_hz.iter().flat_map(|&center| {
                    observed_bands(center, ACQUISITION_CENTER_TOLERANCE_HZ)
                })
            ));
            if upper - lower == 1 {
                assert_eq!(result.centers_hz.len(), 1);
            }
        }
    }

    #[test]
    fn full_range_plan_is_bounded_ordered_and_within_budget() {
        let result = plan(MIN_CENTER_HZ, MAX_CENTER_HZ, 1, 40_000_000).unwrap();
        assert_eq!(result.samples_per_tune, 20_000);
        assert!(result.centers_hz.len() <= MAX_TUNES);
        assert!(result.centers_hz.is_sorted_by(|left, right| left < right));
        assert!(!result.is_targeted());
        assert_eq!(
            serde_json::to_value(&result).unwrap()["operation"],
            "sequential_rf_sweep_plan"
        );
        assert_eq!(
            result.planned_iq_bytes,
            result.centers_hz.len() as u64 * 20_000 * 4
        );
    }

    #[test]
    fn rejects_invalid_requests_and_budget_shortfall() {
        assert!(plan(MIN_CENTER_HZ, MIN_CENTER_HZ, 1, u64::MAX).is_err());
        assert!(plan(MIN_CENTER_HZ - 1, MIN_CENTER_HZ, 1, u64::MAX).is_err());
        assert!(plan(MAX_CENTER_HZ, MAX_CENTER_HZ + 1, 1, u64::MAX).is_err());
        assert!(plan(MIN_CENTER_HZ, MIN_CENTER_HZ + 1, 0, u64::MAX).is_err());
        assert!(plan(MIN_CENTER_HZ, MIN_CENTER_HZ + 1, MAX_DWELL_MS + 1, u64::MAX).is_err());
        let error = plan(MIN_CENTER_HZ, MIN_CENTER_HZ + 1, 1, 79_999).unwrap_err();
        assert!(error.to_string().contains("needs 80000 IQ bytes"));
    }

    #[test]
    fn targeted_plan_sorts_and_deduplicates_sparse_dwell_centers() {
        let requested = [5_180_000_000, 2_412_000_000, 5_180_000_000, 2_462_000_000];
        let result = targeted(&requested, 40, 9_600_000).unwrap();
        assert!(result.is_targeted());
        assert_eq!(
            result.centers_hz,
            [2_412_000_000, 2_462_000_000, 5_180_000_000]
        );
        assert_eq!(result.planned_iq_bytes, 9_600_000);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["operation"], "targeted_rf_sweep_plan");
        assert!(targeted(&requested, 40, 9_599_999).is_err());
    }

    #[test]
    fn targeted_plan_includes_supported_endpoints_and_rejects_invalid_centers() {
        let result = targeted(&[MAX_CENTER_HZ, MIN_CENTER_HZ], 1, 160_000).unwrap();
        assert_eq!(result.lower_hz, MIN_CENTER_HZ);
        assert_eq!(result.upper_hz, MAX_CENTER_HZ + 1);
        let single = targeted(&[MAX_CENTER_HZ], 1, 80_000).unwrap();
        assert_eq!(single.lower_hz, MAX_CENTER_HZ);
        assert_eq!(single.upper_hz, MAX_CENTER_HZ + 1);
        assert!(targeted(&[], 1, u64::MAX).is_err());
        assert!(targeted(&[MIN_CENTER_HZ - 1], 1, u64::MAX).is_err());
        assert!(targeted(&[u64::MAX], 1, u64::MAX).is_err());
        assert!(targeted(&[MIN_CENTER_HZ], 0, u64::MAX).is_err());
        assert!(targeted(&[MIN_CENTER_HZ], MAX_DWELL_MS + 1, u64::MAX).is_err());
    }

    #[test]
    fn targeted_tune_limit_counts_distinct_centers() {
        let mut centers = (MIN_CENTER_HZ..MIN_CENTER_HZ + MAX_TUNES as u64).collect::<Vec<_>>();
        centers.push(MIN_CENTER_HZ);
        assert_eq!(
            targeted(&centers, 1, 40_000_000).unwrap().centers_hz.len(),
            MAX_TUNES
        );
        centers.push(MIN_CENTER_HZ + MAX_TUNES as u64);
        assert!(targeted(&centers, 1, u64::MAX).is_err());
    }
}
