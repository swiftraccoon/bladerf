//! Select receive centers that retain each requested band across readback shifts.

use std::cmp::Reverse;
use std::ops::RangeInclusive;

use anyhow::{Context, Result, ensure};

use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::{CENTER_GUARD_HZ, USABLE_HALF_SPAN_HZ};
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ};

use super::{Band, BandDwell, observed_bands};

const MARGIN_HZ: u64 = ACQUISITION_CENTER_TOLERANCE_HZ;
const OUTER_HZ: u64 = USABLE_HALF_SPAN_HZ as u64 - MARGIN_HZ;
const INNER_HZ: u64 = CENTER_GUARD_HZ as u64 + MARGIN_HZ;

/// Invert the guarded receive spans into inclusive ranges of legal centers.
pub(super) fn center_ranges(band: Band) -> [RangeInclusive<u64>; 2] {
    let above_band = band.upper_hz.saturating_add(INNER_HZ).max(MIN_CENTER_HZ)
        ..=band.lower_hz.saturating_add(OUTER_HZ).min(MAX_CENTER_HZ);
    let below_band = band.upper_hz.saturating_sub(OUTER_HZ).max(MIN_CENTER_HZ)
        ..=band.lower_hz.saturating_sub(INNER_HZ).min(MAX_CENTER_HZ);
    [above_band, below_band]
}

fn clearance(spans: &[Band; 2], band: Band) -> Option<u64> {
    let span = spans
        .iter()
        .find(|span| span.contains_interval(band.lower_hz, band.upper_hz))?;
    Some((band.lower_hz - span.lower_hz).min(span.upper_hz - band.upper_hz))
}

fn score(
    center_hz: u64,
    uncovered: &[&BandDwell],
    longest_dwell_ms: u64,
) -> (usize, usize, u64, Reverse<u64>) {
    let spans = observed_bands(center_hz, MARGIN_HZ);
    let mut longest_count = 0;
    let mut count = 0;
    let mut minimum_clearance_hz = u64::MAX;
    for request in uncovered {
        let Some(edge_distance_hz) = clearance(&spans, request.band) else {
            continue;
        };
        longest_count += usize::from(request.minimum_dwell_ms == longest_dwell_ms);
        count += 1;
        minimum_clearance_hz = minimum_clearance_hz.min(edge_distance_hz);
    }
    (
        longest_count,
        count,
        minimum_clearance_hz,
        Reverse(center_hz),
    )
}

/// Greedily schedule the longest outstanding dwell requirements first.
/// Within each step, prefer covering more bands at that duration, then more
/// remaining bands overall, greater guarded clearance and a lower frequency.
/// Return `(center_hz, dwell_ms)` pairs in ascending center order.
///
/// # Errors
/// Returns an error if a band cannot fit in a single guarded receive span
/// from a supported tuning center.
pub(super) fn schedule(requests: &[BandDwell]) -> Result<Vec<(u64, u64)>> {
    let mut endpoints = Vec::new();
    for request in requests {
        let band = request.band;
        let ranges = center_ranges(band);
        ensure!(
            ranges.iter().any(|range| !range.is_empty()),
            "band {}..{} Hz cannot fit in one guarded receive span at a supported center",
            band.lower_hz,
            band.upper_hz
        );
        for range in ranges.into_iter().filter(|range| !range.is_empty()) {
            endpoints.extend([*range.start(), *range.end()]);
        }
    }
    endpoints.sort_unstable();
    endpoints.dedup();
    // A shared interval contains at least one endpoint. Adjacent midpoints
    // offer interior choices with more clearance when they cover as many bands.
    let midpoints = endpoints.windows(2).map(|pair| pair[0].midpoint(pair[1]));
    let candidates: Vec<_> = endpoints.iter().copied().chain(midpoints).collect();

    let mut selected = Vec::new();
    let mut uncovered: Vec<_> = requests.iter().collect();
    while let Some(longest_dwell_ms) = uncovered
        .iter()
        .map(|request| request.minimum_dwell_ms)
        .max()
    {
        let (_, _, _, Reverse(center_hz)) = candidates
            .iter()
            .map(|&center| score(center, &uncovered, longest_dwell_ms))
            .filter(|&(longest_count, _, _, _)| longest_count > 0)
            .max()
            .context("no candidate retains a remaining requested band")?;
        let spans = observed_bands(center_hz, MARGIN_HZ);
        uncovered.retain(|request| clearance(&spans, request.band).is_none());
        selected.push((center_hz, longest_dwell_ms));
    }
    selected.sort_unstable();
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(lower_hz: u64, upper_hz: u64) -> Band {
        Band { lower_hz, upper_hz }
    }

    fn uniform_centers(bands: &[Band]) -> Result<Vec<u64>> {
        let requests: Vec<_> = bands
            .iter()
            .map(|&band| BandDwell {
                band,
                minimum_dwell_ms: 40,
            })
            .collect();
        Ok(schedule(&requests)?
            .into_iter()
            .map(|(center, _)| center)
            .collect())
    }

    fn assert_covered(band: Band, center_hz: u64) {
        for observed_hz in [center_hz - MARGIN_HZ, center_hz, center_hz + MARGIN_HZ] {
            assert!(
                observed_bands(observed_hz, 0)
                    .iter()
                    .any(|span| span.contains_interval(band.lower_hz, band.upper_hz))
            );
        }
    }

    #[test]
    fn groups_opposite_sides_with_clearance() {
        let bands = [
            band(915_000_000, 915_400_000),
            band(928_000_000, 928_400_000),
        ];
        let selected = uniform_centers(&bands).unwrap();
        assert_eq!(selected, [921_700_000]);
        for band in bands {
            assert_covered(band, selected[0]);
        }
    }

    #[test]
    fn keeps_the_single_common_endpoint() {
        let center_hz = 915_000_000;
        let low = band(center_hz - OUTER_HZ, center_hz - OUTER_HZ + 1);
        let high = band(center_hz + OUTER_HZ - 1, center_hz + OUTER_HZ);
        assert_eq!(uniform_centers(&[low, high]).unwrap(), [center_hz]);
        assert_covered(low, center_hz);
        assert_covered(high, center_hz);

        let farther = band(high.lower_hz + 1, high.upper_hz + 1);
        let selected = uniform_centers(&[low, farther]).unwrap();
        assert_eq!(selected.len(), 2);
        assert!(selected[0] < selected[1]);
        assert_covered(low, selected[0]);
        assert_covered(farther, selected[1]);
    }

    #[test]
    fn retains_edge_bands_after_both_extreme_readback_shifts() {
        let bands = [
            band(MIN_CENTER_HZ - OUTER_HZ, MIN_CENTER_HZ - INNER_HZ),
            band(MAX_CENTER_HZ + INNER_HZ, MAX_CENTER_HZ + OUTER_HZ),
        ];
        let selected = uniform_centers(&bands).unwrap();
        assert_eq!(selected, [MIN_CENTER_HZ, MAX_CENTER_HZ]);
        for (band, center_hz) in bands.into_iter().zip(selected) {
            assert_covered(band, center_hz);
        }
    }

    #[test]
    fn rejects_impossible_width_and_tuning_range() {
        let lower_hz = 915_000_000;
        for request in [
            band(lower_hz, lower_hz + OUTER_HZ - INNER_HZ + 1),
            band(MIN_CENTER_HZ - OUTER_HZ - 1, MIN_CENTER_HZ - OUTER_HZ),
            band(MAX_CENTER_HZ + OUTER_HZ, MAX_CENTER_HZ + OUTER_HZ + 1),
            band(u64::MAX - 1, u64::MAX),
        ] {
            assert!(uniform_centers(&[request]).is_err());
        }
    }

    #[test]
    fn groups_long_dwells_before_a_larger_cluster_of_short_requests() {
        let center_hz = 915_000_000;
        let requests = [
            BandDwell {
                band: band(905_000_000, 905_200_000),
                minimum_dwell_ms: 20,
            },
            BandDwell {
                band: band(905_300_000, 905_500_000),
                minimum_dwell_ms: 40,
            },
            BandDwell {
                band: band(905_700_000, 905_900_000),
                minimum_dwell_ms: 60,
            },
            BandDwell {
                band: band(center_hz - OUTER_HZ, center_hz - OUTER_HZ + 1),
                minimum_dwell_ms: 1_000,
            },
            BandDwell {
                band: band(center_hz + OUTER_HZ - 1, center_hz + OUTER_HZ),
                minimum_dwell_ms: 1_000,
            },
        ];
        let selected = schedule(&requests).unwrap();
        let [(short_center, 60), (long_center, 1_000)] = selected.as_slice() else {
            panic!("expected one short and one long dwell, received {selected:?}");
        };
        assert!(*short_center < *long_center);
        assert_eq!(*long_center, center_hz);
        for request in &requests[..3] {
            assert_covered(request.band, *short_center);
        }
        for request in &requests[3..] {
            assert_covered(request.band, *long_center);
        }
    }
}
