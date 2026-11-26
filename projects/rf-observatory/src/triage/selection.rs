//! Bound clip retention and rank connected events and narrow search separately.

use std::cmp::Ordering;
use std::fs::File;
use std::ops::Range;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::Serialize;

use super::{ClipBounds, Event, MAX_NARROW_WIDTH_HZ, PendingClip, Tune, check_stop};

const MAX_DUPLICATE_CENTER_DISTANCE_HZ: u64 = 100_000;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Detector {
    ConnectedComponent,
    AdjacentContrast,
}

impl Detector {
    /// Compare strength within one detector. Greater ranks first.
    fn compare(self, a: &Event, b: &Event) -> Ordering {
        let peak = || a.peak_excess_db.total_cmp(&b.peak_excess_db);
        match self {
            Self::ConnectedComponent => peak(),
            Self::AdjacentContrast => {
                let contrast =
                    |event: &Event| event.narrow_contrast_db.unwrap_or(f64::NEG_INFINITY);
                contrast(a).total_cmp(&contrast(b)).then_with(peak)
            }
        }
    }
}

/// Retain bounded original-rate clip buffers while each admitted tune is open.
pub(super) struct Clips<'a> {
    components: Vec<PendingClip<'a>>,
    narrow: Vec<PendingClip<'a>>,
    total_limit: usize,
    narrow_limit: usize,
}

impl<'a> Clips<'a> {
    pub(super) fn new(total_limit: usize, narrow_slots: usize) -> Self {
        let narrow_limit = narrow_slots.min(total_limit.saturating_sub(1));
        Self {
            components: Vec::with_capacity(total_limit),
            narrow: Vec::with_capacity(narrow_limit),
            total_limit,
            narrow_limit,
        }
    }

    pub(super) fn narrow_limit(&self) -> usize {
        self.narrow_limit
    }

    pub(super) fn consider_components(
        &mut self,
        tune: &'a Tune,
        file: &File,
        interval: &Range<u64>,
        mut events: Vec<Event>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let detector = Detector::ConnectedComponent;
        events.sort_by(|a, b| detector.compare(b, a));
        for event in events {
            check_stop(stop)?;
            let index = insertion_index(detector, &self.components, &event);
            if index == self.total_limit {
                break;
            }
            let bounds = ClipBounds::new(&event, interval);
            let clip = PendingClip::read(tune, event, detector, bounds, file)?;
            retain(&mut self.components, self.total_limit, index, clip);
        }
        Ok(())
    }

    /// Keep distinct probeable runs and count all eligible retained runs.
    pub(super) fn consider_narrow(
        &mut self,
        tune: &'a Tune,
        file: &File,
        interval: &Range<u64>,
        mut events: Vec<Event>,
        stop: &AtomicBool,
    ) -> Result<usize> {
        let detector = Detector::AdjacentContrast;
        events.sort_by(|a, b| detector.compare(b, a));
        let mut eligible = 0;
        for event in events {
            check_stop(stop)?;
            let bounds = ClipBounds::new(&event, interval);
            if bounds
                .probe_parameters(&event, tune.observed_center_hz)
                .is_err()
            {
                continue;
            }
            eligible += 1;
            let index = insertion_index(detector, &self.narrow, &event);
            if index == self.narrow_limit
                || self
                    .narrow
                    .iter()
                    .any(|clip| duplicates(clip, tune, &event))
            {
                continue;
            }
            let clip = PendingClip::read(tune, event, detector, bounds, file)?;
            retain(&mut self.narrow, self.narrow_limit, index, clip);
        }
        Ok(eligible)
    }

    pub(super) fn finish(self) -> Vec<PendingClip<'a>> {
        let mut selected: Vec<_> = self
            .components
            .into_iter()
            .filter(|clip| {
                !self
                    .narrow
                    .iter()
                    .any(|narrow| duplicates(narrow, clip.tune, &clip.event))
            })
            .take(self.total_limit - self.narrow.len())
            .collect();
        selected.extend(self.narrow);
        selected
    }
}

fn insertion_index(detector: Detector, retained: &[PendingClip<'_>], event: &Event) -> usize {
    retained.partition_point(|clip| detector.compare(&clip.event, event).is_ge())
}

fn retain<'a>(clips: &mut Vec<PendingClip<'a>>, limit: usize, index: usize, clip: PendingClip<'a>) {
    if clips.len() == limit {
        clips.pop();
    }
    clips.insert(index, clip);
}

/// Check overlap against the shorter of two nonempty spans.
fn shares_quarters(a: Range<u64>, b: Range<u64>, minimum_quarters: u64) -> bool {
    let overlap = a.end.min(b.end).saturating_sub(a.start.max(b.start));
    let shorter = (a.end - a.start).min(b.end - b.start);
    overlap * 4 >= shorter * minimum_quarters
}

fn duplicates(clip: &PendingClip<'_>, tune: &Tune, event: &Event) -> bool {
    let a = &clip.event;
    let a_frequency = a.lower_frequency_hz..a.upper_frequency_hz_exclusive;
    let b_frequency = event.lower_frequency_hz..event.upper_frequency_hz_exclusive;
    clip.tune.directory == tune.directory
        && b_frequency.end - b_frequency.start <= MAX_NARROW_WIDTH_HZ
        && a_frequency
            .start
            .midpoint(a_frequency.end)
            .abs_diff(b_frequency.start.midpoint(b_frequency.end))
            <= MAX_DUPLICATE_CENTER_DISTANCE_HZ
        && shares_quarters(a_frequency, b_frequency, 2)
        && shares_quarters(
            a.start_sample..a.end_sample_exclusive,
            event.start_sample..event.end_sample_exclusive,
            3,
        )
}
