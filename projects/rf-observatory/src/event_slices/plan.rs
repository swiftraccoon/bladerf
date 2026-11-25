use anyhow::{Result, ensure};
use serde::Serialize;

/// Align the source sample grid to Blue Dragon's full-band replay step.
pub(super) const ALIGNMENT_SAMPLES: u64 = 10;

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct Limits {
    pub(super) pre_roll_samples: u64,
    pub(super) post_roll_samples: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub(super) merge_gap_samples: u64,
    pub(super) max_slices: usize,
    #[serde(skip)]
    pub(super) coalesce_to_limit: bool,
    pub(super) max_total_samples: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Span {
    pub(super) start_sample: u64,
    pub(super) end_sample_exclusive: u64,
}

impl Span {
    pub(super) fn samples(self) -> u64 {
        self.end_sample_exclusive - self.start_sample
    }
}

fn align_down(samples: u64) -> u64 {
    samples - samples % ALIGNMENT_SAMPLES
}

/// Pad and align projected events before merging spans within the allowed gap.
/// When coalescing is enabled, bridge the shortest remaining gaps as needed,
/// favoring the earlier gap in ties. Bridged gaps count toward total samples.
/// The final partial replay step is clipped. An event wholly inside that step
/// or a merged plan that exceeds either limit is an error.
pub(super) fn plan(events: &[Span], analyzed_samples: u64, limits: Limits) -> Result<Vec<Span>> {
    let aligned_prefix = align_down(analyzed_samples);
    let mut spans = Vec::with_capacity(events.len());
    for event in events {
        ensure!(
            event.start_sample < event.end_sample_exclusive
                && event.end_sample_exclusive <= analyzed_samples,
            "event sample bounds are empty, reversed or outside the analyzed prefix"
        );
        ensure!(
            event.start_sample < aligned_prefix,
            "event is entirely inside the unalignable analyzed tail"
        );
        let padded_end = event
            .end_sample_exclusive
            .saturating_add(limits.post_roll_samples)
            .min(aligned_prefix);
        spans.push(Span {
            start_sample: align_down(event.start_sample.saturating_sub(limits.pre_roll_samples)),
            end_sample_exclusive: padded_end.div_ceil(ALIGNMENT_SAMPLES) * ALIGNMENT_SAMPLES,
        });
    }
    spans.sort_unstable_by_key(|span| span.start_sample);

    let mut merged: Vec<Span> = Vec::new();
    for span in spans {
        if let Some(previous) = merged.last_mut()
            && span
                .start_sample
                .saturating_sub(previous.end_sample_exclusive)
                <= limits.merge_gap_samples
        {
            previous.end_sample_exclusive =
                previous.end_sample_exclusive.max(span.end_sample_exclusive);
        } else {
            merged.push(span);
        }
    }

    if limits.coalesce_to_limit && merged.len() > limits.max_slices {
        merged = coalesce(merged, limits.max_slices);
    }
    ensure!(
        merged.len() <= limits.max_slices,
        "slice count exceeds limit after padding and merging"
    );
    // The merged spans are disjoint and contained in the aligned prefix.
    let total_samples: u64 = merged.iter().map(|span| span.samples()).sum();
    ensure!(
        total_samples <= limits.max_total_samples,
        "total slice samples exceed limit after padding and merging"
    );
    Ok(merged)
}

fn coalesce(spans: Vec<Span>, max_slices: usize) -> Vec<Span> {
    // Each bridge removes one boundary and adds that gap's samples.
    let mut gaps: Vec<_> = spans
        .windows(2)
        .enumerate()
        .map(|(index, pair)| {
            (
                pair[1].start_sample - pair[0].end_sample_exclusive,
                index + 1,
            )
        })
        .collect();
    gaps.sort_unstable();
    let mut bridge = vec![false; spans.len()];
    for (_, index) in gaps.into_iter().take(spans.len() - max_slices) {
        bridge[index] = true;
    }

    let mut merged: Vec<Span> = Vec::with_capacity(max_slices);
    for (span, bridge_previous) in spans.into_iter().zip(bridge) {
        if let Some(previous) = merged.last_mut()
            && bridge_previous
        {
            previous.end_sample_exclusive = span.end_sample_exclusive;
        } else {
            merged.push(span);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start_sample: u64, end_sample_exclusive: u64) -> Span {
        Span {
            start_sample,
            end_sample_exclusive,
        }
    }

    const LIMITS: Limits = Limits {
        pre_roll_samples: 0,
        post_roll_samples: 0,
        merge_gap_samples: 0,
        max_slices: 2,
        coalesce_to_limit: false,
        max_total_samples: 60,
    };

    #[test]
    fn sorts_and_merges_touching_and_overlapping_spans() {
        let events = [span(81, 85), span(44, 59), span(30, 31), span(41, 48)];
        assert_eq!(
            plan(&events, 100, LIMITS).unwrap(),
            [span(30, 60), span(80, 90)]
        );
    }

    #[test]
    fn pads_and_aligns_outward_at_both_prefix_boundaries() {
        let limits = Limits {
            pre_roll_samples: 5,
            post_roll_samples: 13,
            ..LIMITS
        };
        assert_eq!(
            plan(&[span(2, 4), span(94, 97)], 103, limits).unwrap(),
            [span(0, 20), span(80, 100)]
        );
    }

    #[test]
    fn merges_at_the_padded_and_aligned_gap_threshold() {
        let events = [span(21, 24), span(47, 49)];
        let limits = Limits {
            pre_roll_samples: 2,
            post_roll_samples: 4,
            merge_gap_samples: 9,
            ..LIMITS
        };
        assert_eq!(
            plan(&events, 100, limits).unwrap(),
            [span(10, 30), span(40, 60)]
        );
        let limits = Limits {
            merge_gap_samples: 10,
            ..limits
        };
        assert_eq!(plan(&events, 100, limits).unwrap(), [span(10, 60)]);
    }

    #[test]
    fn counts_gap_samples_and_applies_slice_count_after_merging() {
        let events = [span(1, 21), span(52, 59)];
        let limits = Limits {
            merge_gap_samples: 20,
            max_slices: 1,
            ..LIMITS
        };
        assert_eq!(plan(&events, 100, limits).unwrap(), [span(0, 60)]);

        let limits = Limits {
            max_total_samples: 59,
            ..limits
        };
        let error = plan(&events, 100, limits).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("total slice samples exceed limit")
        );
    }

    #[test]
    fn coalesces_the_shortest_gaps_in_capture_order_and_counts_their_samples() {
        let events = [
            span(70, 80),
            span(30, 40),
            span(90, 100),
            span(0, 10),
            span(50, 60),
        ];
        let limits = Limits {
            max_slices: 3,
            coalesce_to_limit: true,
            max_total_samples: 70,
            ..LIMITS
        };
        assert_eq!(
            plan(&events, 100, limits).unwrap(),
            [span(0, 10), span(30, 80), span(90, 100)]
        );
        let error = plan(
            &events,
            100,
            Limits {
                max_total_samples: 69,
                ..limits
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("total slice samples exceed limit")
        );
    }

    #[test]
    fn clips_the_last_fft_window_but_rejects_a_wholly_lost_event() {
        let limits = Limits {
            max_total_samples: 8192,
            ..LIMITS
        };
        assert_eq!(
            plan(&[span(8192, 16384)], 16384, limits).unwrap(),
            [span(8190, 16380)]
        );
        assert!(plan(&[span(16380, 16384)], 16384, limits).is_err());
    }

    #[test]
    fn validates_input_and_enforces_budgets_on_aligned_union() {
        let events = [span(1, 21), span(53, 76)];
        let limits = Limits {
            max_total_samples: 59,
            ..LIMITS
        };
        assert!(plan(&events, 100, limits).is_err());
        let limits = Limits {
            max_slices: 1,
            ..LIMITS
        };
        assert!(plan(&events, 100, limits).is_err());
        assert_eq!(
            plan(&events, 100, LIMITS).unwrap(),
            [span(0, 30), span(50, 80)]
        );
        for invalid in [span(20, 20), span(30, 20), span(99, 101)] {
            assert!(plan(&[invalid], 100, LIMITS).is_err());
        }
        assert!(plan(&[], 100, LIMITS).unwrap().is_empty());
    }
}
