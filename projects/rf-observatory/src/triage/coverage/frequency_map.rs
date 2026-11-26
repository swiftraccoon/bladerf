//! Partition a requested RF range wherever the set of covering dwells changes.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde::Serialize;

use crate::sweep::plan::{Band, observed_bands};

use super::{BandReport, Dwell, HostReadWindow, ListeningTime, check_stop};

struct Edge {
    hz: u64,
    dwell: usize,
    starts: bool,
}

#[derive(Serialize)]
pub(super) struct FrequencyMap {
    #[serde(flatten)]
    range: Band,
    covered_bandwidth_hz: u64,
    qualifying_bandwidth_hz: u64,
    regions: Vec<BandReport>,
}

impl FrequencyMap {
    /// Measure each half-open region using its chronologically ordered dwells.
    /// Report uncovered regions even when no observed span intersects the range.
    ///
    /// # Errors
    /// Returns an error if interrupted or if a region has invalid bounds.
    pub(super) fn measure(
        range: Band,
        dwells: &[Dwell<'_>],
        minimum_samples: u64,
        host_window: Option<&HostReadWindow>,
        stop: &AtomicBool,
    ) -> Result<Self> {
        let scope = range.bounds();
        let mut edges = Vec::new();
        for (index, dwell) in dwells.iter().enumerate() {
            for span in observed_bands(dwell.tune.observed_center_hz, 0) {
                let span = span.bounds();
                let lower_hz = scope.start.max(span.start);
                let upper_hz = scope.end.min(span.end);
                if lower_hz < upper_hz {
                    edges.extend([
                        Edge {
                            hz: lower_hz,
                            dwell: index,
                            starts: true,
                        },
                        Edge {
                            hz: upper_hz,
                            dwell: index,
                            starts: false,
                        },
                    ]);
                }
            }
        }
        edges.sort_unstable_by_key(|edge| edge.hz);
        let mut map = Self {
            range,
            covered_bandwidth_hz: 0,
            qualifying_bandwidth_hz: 0,
            regions: Vec::new(),
        };
        let mut active = BTreeSet::<usize>::new();
        let mut lower_hz = scope.start;
        for group in edges.chunk_by(|left, right| left.hz == right.hz) {
            let upper_hz = group[0].hz;
            check_stop(stop)?;
            if lower_hz < upper_hz {
                let listening = ListeningTime::measure(
                    active.iter().map(|&index| &dwells[index]),
                    minimum_samples,
                    host_window,
                );
                map.add_region(lower_hz, upper_hz, listening)?;
            }
            // Equal-frequency edges affect only the next region. The two
            // usable spans of a dwell are disjoint and never touch.
            for edge in group {
                if edge.starts {
                    active.insert(edge.dwell);
                } else {
                    active.remove(&edge.dwell);
                }
            }
            lower_hz = upper_hz;
        }
        if lower_hz < scope.end {
            check_stop(stop)?;
            map.add_region(
                lower_hz,
                scope.end,
                ListeningTime::measure([], minimum_samples, host_window),
            )?;
        }
        Ok(map)
    }

    fn add_region(&mut self, lower_hz: u64, upper_hz: u64, listening: ListeningTime) -> Result<()> {
        let band = Band::new(lower_hz, upper_hz)?;
        let width_hz = upper_hz - lower_hz;
        self.covered_bandwidth_hz += width_hz * u64::from(listening.covered_dwells > 0);
        self.qualifying_bandwidth_hz += width_hz * u64::from(listening.qualifying_dwells > 0);
        self.regions.push(BandReport { band, listening });
        Ok(())
    }

    pub(super) fn region_count(&self) -> usize {
        self.regions.len()
    }
}
