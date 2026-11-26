//! Place paired receive tunings around a target band with readback margins.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::{CENTER_GUARD_HZ, USABLE_HALF_SPAN_HZ};
use crate::triage::MIN_NARROW_CENTER_OFFSET_HZ;
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ};

const PREFERRED_OFFSET_HZ: u64 = 2_000_000;
const MIN_LO_CHANGE_HZ: u64 = 500_000;
const READBACK_MARGIN_HZ: u64 = ACQUISITION_CENTER_TOLERANCE_HZ;

/// Target RF bounds, target frequency, and contributing receive tunings.
pub(super) struct Band {
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    pub(super) target_hz: u64,
    pub(super) source_low_hz: u64,
    pub(super) source_high_hz: u64,
}

/// Find tuning ranges that admit the full target band and keep it clear of DC.
fn center_ranges(band: &Band) -> [RangeInclusive<u64>; 2] {
    let half_span = USABLE_HALF_SPAN_HZ as u64 - READBACK_MARGIN_HZ;
    let guard = CENTER_GUARD_HZ as u64 + READBACK_MARGIN_HZ;
    let probe_offset = MIN_NARROW_CENTER_OFFSET_HZ + READBACK_MARGIN_HZ;
    let left = band.upper_hz.saturating_sub(half_span).max(MIN_CENTER_HZ)
        ..=band
            .lower_hz
            .saturating_sub(guard)
            .min(band.target_hz.saturating_sub(probe_offset))
            .min(MAX_CENTER_HZ);
    let right = (band.lower_hz + half_span).min(MAX_CENTER_HZ);
    let right_start = (band.upper_hz + guard)
        .max(band.target_hz + probe_offset)
        .max(MIN_CENTER_HZ);
    [left, right_start..=right]
}

/// Place two exact requested centers away from the source tuning interval.
/// Prefer opposite offsets, then reuse existing centers where possible.
pub(super) fn place_pair(band: &Band, assigned: &BTreeSet<u64>) -> Option<[u64; 2]> {
    if band.source_low_hz > band.source_high_hz {
        return None;
    }
    let source_separation = MIN_LO_CHANGE_HZ + READBACK_MARGIN_HZ;
    let allowed_below = band.source_low_hz.saturating_sub(source_separation);
    let allowed_above = band.source_high_hz.checked_add(source_separation)?;
    let pair_separation = MIN_LO_CHANGE_HZ + 2 * READBACK_MARGIN_HZ;
    let mut candidates = BTreeSet::new();
    for (range, preferred) in center_ranges(band).into_iter().zip([
        band.target_hz.saturating_sub(PREFERRED_OFFSET_HZ),
        band.target_hz + PREFERRED_OFFSET_HZ,
    ]) {
        if range.is_empty() {
            continue;
        }
        // Keep feasible endpoints when the preferred offset lies outside the
        // usable range or inside the contributing tuning interval.
        let seeds = [
            *range.start(),
            *range.end(),
            preferred.clamp(*range.start(), *range.end()),
            preferred.saturating_sub(pair_separation),
            preferred + pair_separation,
            allowed_below,
            allowed_above,
        ];
        candidates.extend(
            seeds
                .into_iter()
                .chain(assigned.iter().copied())
                .filter(|center| {
                    range.contains(center) && (*center <= allowed_below || *center >= allowed_above)
                }),
        );
    }
    let placement_key = |a: u64, b: u64| {
        let same_side = a >= band.target_hz || b <= band.target_hz;
        let added = usize::from(!assigned.contains(&a)) + usize::from(!assigned.contains(&b));
        let offset_error = |center: u64| {
            center
                .abs_diff(band.target_hz)
                .abs_diff(PREFERRED_OFFSET_HZ)
        };
        (same_side, added, offset_error(a) + offset_error(b), a, b)
    };
    candidates
        .iter()
        .flat_map(|&a| {
            candidates
                .range((a + pair_separation)..)
                .map(move |&b| [a, b])
        })
        .min_by_key(|&[a, b]| placement_key(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::plan;

    fn band(target_hz: u64, source_hz: u64) -> Band {
        Band {
            lower_hz: target_hz - 50_000,
            upper_hz: target_hz + 50_000,
            target_hz,
            source_low_hz: source_hz,
            source_high_hz: source_hz,
        }
    }

    fn verify_geometry(band: &Band, pair: [u64; 2]) {
        for requested in pair {
            assert!((MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&requested));
            for observed in [
                requested - READBACK_MARGIN_HZ,
                requested + READBACK_MARGIN_HZ,
            ] {
                assert!(
                    observed + MIN_LO_CHANGE_HZ <= band.source_low_hz
                        || observed >= band.source_high_hz + MIN_LO_CHANGE_HZ
                );
                assert!(observed.abs_diff(band.target_hz) >= MIN_NARROW_CENTER_OFFSET_HZ);
                assert!(plan::covers(
                    band.lower_hz,
                    band.upper_hz,
                    plan::observed_bands(observed, 0)
                ));
            }
        }
        assert!(pair[1] - pair[0] >= MIN_LO_CHANGE_HZ + 2 * READBACK_MARGIN_HZ);
    }

    #[test]
    fn prefers_opposite_offsets_and_reuses_a_pair_for_nearby_bands() {
        let first = band(915_000_000, 910_000_000);
        let pair = place_pair(&first, &BTreeSet::new()).unwrap();
        assert_eq!(pair, [913_000_000, 917_000_000]);
        verify_geometry(&first, pair);

        let nearby = band(915_150_000, 910_000_000);
        let retained = place_pair(&nearby, &BTreeSet::from(pair)).unwrap();
        assert_eq!(retained, pair);
        verify_geometry(&nearby, retained);
    }

    #[test]
    fn uses_same_side_or_no_pair_at_tuning_limits() {
        for edge in [
            band(MIN_CENTER_HZ, MIN_CENTER_HZ + 2_000_000),
            band(MAX_CENTER_HZ, MAX_CENTER_HZ - 2_000_000),
        ] {
            let pair = place_pair(&edge, &BTreeSet::new()).unwrap();
            verify_geometry(&edge, pair);
            assert_eq!(pair[0] < edge.target_hz, pair[1] < edge.target_hz);
        }
        let too_close_to_edge = band(MIN_CENTER_HZ - 7_500_000, MIN_CENTER_HZ);
        assert!(place_pair(&too_close_to_edge, &BTreeSet::new()).is_none());
    }

    #[test]
    fn excludes_the_whole_source_tuning_interval() {
        let target = Band {
            source_high_hz: 913_500_000,
            ..band(915_000_000, 911_000_000)
        };
        let assigned = BTreeSet::from([912_000_000, 917_000_000]);
        let pair = place_pair(&target, &assigned).unwrap();
        assert_eq!(pair, [914_000_010, 917_000_000]);
        verify_geometry(&target, pair);
    }
}
