//! Schedule and bind bounded, nonoverlapping intervals of saved IQ.

use std::fs::File;
use std::ops::Range;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

use crate::spectrum::{self, EventCandidateSets, EventWindows, FFT};

/// Maximum number of time slices in each dwell.
pub(crate) const MAX_TIME_SLICES: u32 = 8;
const WINDOW_SAMPLES: u64 = FFT as u64;
const MIN_WINDOWS_PER_SLICE: u64 = 8;

/// Plan complete FFT windows using at most the supplied sample budget.
///
/// A single slice starts at sample zero. Multiple slices use both ends of
/// the complete-window extent, with nearly equal lengths and gaps.
pub(crate) fn intervals(
    saved_samples: u64,
    sample_limit: u64,
    time_slices: u32,
) -> Vec<Range<u64>> {
    let available = saved_samples / WINDOW_SAMPLES;
    let selected = available.min(sample_limit / WINDOW_SAMPLES);
    if selected == 0 {
        return Vec::new();
    }
    let skipped = available - selected;
    let count = u64::from(time_slices)
        .min(selected / MIN_WINDOWS_PER_SLICE)
        .min(skipped + 1)
        .max(1);
    let mut result = Vec::with_capacity(count as usize);
    let mut next = 0;
    for index in 0..count {
        let end = next + selected / count + u64::from(index < selected % count);
        result.push(next * WINDOW_SAMPLES..end * WINDOW_SAMPLES);
        if index + 1 < count {
            let gaps = count - 1;
            next = end + skipped / gaps + u64::from(index < skipped % gaps);
        }
    }
    result
}

/// Source coordinates and digest of one analyzed SC16 Q11 interval.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct IntervalEvidence {
    pub(crate) start_sample: u64,
    pub(crate) end_sample_exclusive: u64,
    pub(crate) analyzed_iq_sha256: String,
}

/// Compare the reported extents and digest syntax with the expected schedule.
pub(crate) fn validate_intervals(
    evidence: &[IntervalEvidence],
    expected: &[Range<u64>],
    analyzed_samples: u64,
) -> bool {
    !expected.is_empty()
        && evidence.len() == expected.len()
        && analyzed_samples
            == expected
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u64>()
        && evidence.iter().zip(expected).all(|(item, expected)| {
            item.start_sample == expected.start
                && item.end_sample_exclusive == expected.end
                && item.analyzed_iq_sha256.len() == 64
                && item
                    .analyzed_iq_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// Require all samples in a nonempty span to lie inside one analyzed interval.
pub(crate) fn contains_interval(evidence: &[IntervalEvidence], start: u64, end: u64) -> bool {
    start < end
        && evidence
            .iter()
            .any(|item| item.start_sample <= start && end <= item.end_sample_exclusive)
}

/// Deserialize an optional receipt field and reject explicit null values.
pub(crate) fn deserialize_present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// Analyze one interval and express its candidates on the original sample axis.
/// The interval always spans complete FFT windows. Stride affects only event
/// detection within that interval.
///
/// # Errors
/// Propagates interruption, invalid extent or samples, and source read errors.
pub(super) fn survey(
    input: &mut File,
    center_hz: u64,
    interval: Range<u64>,
    windows: EventWindows,
    narrow: bool,
    stop: &AtomicBool,
) -> Result<(spectrum::Report, IntervalEvidence, EventCandidateSets)> {
    let (power, mut events) = spectrum::survey_file_interval_with_events(
        input,
        center_hz,
        interval.clone(),
        windows,
        narrow,
        stop,
    )?;
    let evidence = IntervalEvidence {
        start_sample: interval.start,
        end_sample_exclusive: interval.end,
        analyzed_iq_sha256: std::mem::take(&mut events.analyzed_prefix_sha256),
    };
    let mut candidates = events.into_candidates();
    for set in std::iter::once(&mut candidates.primary).chain(&mut candidates.overlap) {
        for event in set
            .components
            .retained
            .iter_mut()
            .chain(set.narrow.iter_mut().flat_map(|set| &mut set.retained))
        {
            event.start_sample += interval.start;
            event.end_sample_exclusive += interval.start;
            event.peak_window_start_sample += interval.start;
        }
    }
    Ok((power, evidence, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divides_complete_windows_and_gaps_with_remainders() {
        let samples = |windows: u64| windows * WINDOW_SAMPLES;
        assert_eq!(
            intervals(samples(53) + 3, samples(31) + 7, 8),
            [
                0..samples(11),
                samples(22)..samples(32),
                samples(43)..samples(53)
            ]
        );
        let fully_covered = intervals(samples(17) + 3, samples(17) + 7, 8);
        assert!(matches!(fully_covered.as_slice(), [range] if *range == (0..samples(17))));
        let short_budget = intervals(samples(100), samples(15), 8);
        assert!(matches!(short_budget.as_slice(), [range] if *range == (0..samples(15))));
        assert!(intervals(WINDOW_SAMPLES - 1, WINDOW_SAMPLES, 2).is_empty());
    }
}
