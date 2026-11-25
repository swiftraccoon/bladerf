//! Bounded, host-only candidate-family analysis for validated Blue Dragon bursts.
//!
//! The classifications describe waveform shape and simultaneous channelized
//! records. They do not identify a protocol, transmitter, packet or occupancy.

mod breakdown;
mod features;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::bursts::{
    self, CHANNEL_SAMPLE_RATE_HZ, HIGHEST_FREQUENCY_MHZ, LOWEST_FREQUENCY_MHZ, ParseLimits,
    RecordMetadata,
};
#[cfg(test)]
pub(crate) use features::TwoLevelFeature;
#[cfg(test)]
use features::fit_two_levels;
use features::record_feature;
pub(crate) use features::{
    ComplexSample, DirectFeature, FeatureSettings, RecordFeature, ShapeFirstFailure,
    ShapeGateEvaluation, circular_shape_gate_evaluation, complex_control_features,
    complex_direct_feature, complex_record_feature, direct_shape_gate_evaluation, feature_settings,
    two_level_shape_qualified,
};

const DEFAULT_MAX_INPUT_MIB: u32 = 128;
const HARD_MAX_INPUT_MIB: u32 = 512;
const DEFAULT_MAX_SAMPLES: u64 = 50_000_000;
const HARD_MAX_SAMPLES: u64 = 200_000_000;
const DEFAULT_MAX_RECORDS: u64 = 100_000;
const HARD_MAX_RECORDS: u64 = 100_000;
const DEFAULT_MAX_GROUPS: u64 = 100_000;
const HARD_MAX_GROUPS: u64 = 100_000;
const DEFAULT_MAX_OUTPUT_MIB: u32 = 16;
const HARD_MAX_OUTPUT_MIB: u32 = 256;
const SAMPLE_PERIOD_NS: u128 = 500;
const GROUP_GAP_NS: u128 = 50_000;
const GROUP_FREQUENCY_STEP_MHZ: usize = 1;
const ACTIVE_POWER_QUANTILE_NUMERATOR: usize = 4;
const ACTIVE_POWER_QUANTILE_DENOMINATOR: usize = 5;
const ACTIVE_POWER_DIVISOR: u64 = 16;
const MIN_USABLE_PHASE_DIFFERENCES: usize = 128;
const MAX_ENVELOPE_CV: f64 = 0.25;
const MIN_CLUSTER_BALANCE: f64 = 0.15;
const MIN_VARIANCE_REDUCTION: f64 = 0.60;
const MIN_MODE_CONTRAST: f64 = 4.0;
const MODE_CONTRAST_STD_FLOOR_HZ: f64 = 1.0;
const MIN_PHASE_CONCENTRATION: f64 = 0.05;
const MIN_SEPARATION_HZ: f64 = 40_000.0;
const MAX_SEPARATION_HZ: f64 = 900_000.0;
const MIN_TRANSITIONS: u64 = 6;
const KMEANS_ITERATIONS: usize = 8;
const ADJACENT_MULTIBIN_MIN_WIDTH: usize = 2;
const WIDEBAND_MULTIBIN_MIN_WIDTH: usize = 4;
const FREQUENCY_COUNT: usize = (HIGHEST_FREQUENCY_MHZ - LOWEST_FREQUENCY_MHZ + 1) as usize;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Args)]
pub(crate) struct ClassifyBursts {
    /// Existing Blue Dragon BDB v1 archive; direct regular files only.
    input: PathBuf,
    /// New output directory; defaults under artifacts/bluetooth-burst-families/.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Optional closed family row for a separate aggregate feature breakdown.
    #[arg(long, value_enum, requires = "feature_breakdown_frequency_mhz")]
    feature_breakdown_family: Option<Family>,
    /// Optional 1 MHz row for the paired aggregate feature breakdown.
    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(2402..=2480),
        requires = "feature_breakdown_family"
    )]
    feature_breakdown_frequency_mhz: Option<u32>,
    /// Maximum accepted archive size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_INPUT_MIB)]
    max_input_mib: u32,
    /// Maximum total complex samples across squelch and scan records.
    #[arg(long, default_value_t = DEFAULT_MAX_SAMPLES)]
    max_samples: u64,
    /// Maximum archive records.
    #[arg(long, default_value_t = DEFAULT_MAX_RECORDS)]
    max_records: u64,
    /// Maximum temporal groups formed from squelch records.
    #[arg(long, default_value_t = DEFAULT_MAX_GROUPS)]
    max_groups: u64,
    /// Maximum terminal JSON size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_MIB)]
    max_output_mib: u32,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    max_input_bytes: u64,
    max_samples: u64,
    max_records: u64,
    max_groups: u64,
    max_output_bytes: u64,
}

impl ClassifyBursts {
    fn limits(&self) -> Result<Limits> {
        ensure!(
            (1..=HARD_MAX_INPUT_MIB).contains(&self.max_input_mib),
            "classification input limit must be 1..={HARD_MAX_INPUT_MIB} MiB"
        );
        ensure!(
            (1..=HARD_MAX_SAMPLES).contains(&self.max_samples),
            "classification sample limit must be 1..={HARD_MAX_SAMPLES}"
        );
        ensure!(
            (1..=HARD_MAX_RECORDS).contains(&self.max_records),
            "classification record limit must be 1..={HARD_MAX_RECORDS}"
        );
        ensure!(
            (1..=HARD_MAX_GROUPS).contains(&self.max_groups),
            "classification group limit must be 1..={HARD_MAX_GROUPS}"
        );
        ensure!(
            (1..=HARD_MAX_OUTPUT_MIB).contains(&self.max_output_mib),
            "classification output limit must be 1..={HARD_MAX_OUTPUT_MIB} MiB"
        );
        Ok(Limits {
            max_input_bytes: u64::from(self.max_input_mib) * 1024 * 1024,
            max_samples: self.max_samples,
            max_records: self.max_records,
            max_groups: self.max_groups,
            max_output_bytes: u64::from(self.max_output_mib) * 1024 * 1024,
        })
    }

    fn feature_breakdown_selection(&self) -> Result<Option<breakdown::Selection>> {
        match (
            self.feature_breakdown_family,
            self.feature_breakdown_frequency_mhz,
        ) {
            (None, None) => Ok(None),
            (Some(family), Some(frequency_mhz)) => {
                Ok(Some(breakdown::Selection::new(family, frequency_mhz)?))
            }
            _ => bail!(
                "feature-breakdown-family and feature-breakdown-frequency-mhz must be provided together"
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct SourceIdentity {
    format: &'static str,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Serialize)]
struct Accounting {
    archive_record_count: u64,
    squelch_record_count: u64,
    scan_window_record_count: u64,
    archive_complex_sample_count: u64,
    squelch_complex_sample_count: u64,
    scan_window_complex_sample_count: u64,
    feature_eligible_record_count: u64,
    feature_insufficient_record_count: u64,
    group_count: u64,
    grouped_squelch_record_count: u64,
}

#[derive(Clone, Debug, Serialize)]
struct Settings {
    channel_sample_rate_hz: u32,
    sample_period_ns: u64,
    group_connectivity_gap_ns: u64,
    group_frequency_step_mhz: u64,
    simultaneous_interval: &'static str,
    active_power_quantile: [u64; 2],
    active_power_divisor: u64,
    minimum_usable_phase_differences: u64,
    maximum_envelope_cv: f64,
    minimum_cluster_balance: f64,
    minimum_variance_reduction: f64,
    minimum_mode_contrast: f64,
    mode_contrast_std_floor_hz: f64,
    minimum_phase_concentration: f64,
    separation_hz: [f64; 2],
    minimum_transitions: u64,
    kmeans_iterations: u64,
    adjacent_multibin_minimum_width: u64,
    wideband_multibin_minimum_width: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
struct NumericSummary {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
}

#[derive(Clone, Debug, Default)]
struct NumericAccumulator {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
}

impl NumericAccumulator {
    fn observe(&mut self, value: f64) {
        debug_assert!(value.is_finite());
        self.observations += 1;
        self.min = Some(self.min.map_or(value, |current| current.min(value)));
        self.max = Some(self.max.map_or(value, |current| current.max(value)));
        self.sum += value;
    }

    fn finish(self) -> NumericSummary {
        NumericSummary {
            observations: self.observations,
            min: self.min,
            max: self.max,
            mean: (self.observations > 0).then_some(self.sum / self.observations as f64),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct FeatureSummaries {
    envelope_cv: NumericSummary,
    usable_phase_differences: NumericSummary,
    phase_concentration: NumericSummary,
    two_level_separation_hz: NumericSummary,
    two_level_variance_reduction: NumericSummary,
    two_level_mode_contrast: NumericSummary,
    two_level_cluster_balance: NumericSummary,
    two_level_transitions: NumericSummary,
}

#[derive(Clone, Copy, Debug)]
struct Event {
    archive_index: u64,
    start_ns: u128,
    end_ns: u128,
    frequency_mhz: u32,
    sample_count: u64,
    feature: Option<RecordFeature>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Family {
    #[value(name = "wideband_multibin_candidate")]
    WidebandMultibin,
    #[value(name = "adjacent_multibin_candidate")]
    AdjacentMultibin,
    #[value(name = "narrowband_binary_fsk_like_candidate")]
    NarrowbandBinaryFskLike,
    #[value(name = "narrowband_constant_envelope_unresolved_candidate")]
    NarrowbandConstantEnvelopeUnresolved,
    #[value(name = "unclassified_burst_group")]
    Unclassified,
}

#[derive(Debug)]
pub(crate) struct SelectedRecord {
    pub(crate) metadata: RecordMetadata,
    pub(crate) packed_iq: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct SelectedArchive {
    pub(crate) source_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) archive_record_count: u64,
    pub(crate) selected_group_count: u64,
    pub(crate) records: Vec<SelectedRecord>,
}

#[derive(Clone, Copy, Debug)]
struct RecordSelection {
    family: Family,
    frequency_mhz: u32,
}

#[derive(Debug, Default)]
struct RecordSelectionResult {
    group_count: u64,
    archive_indices: Vec<u64>,
}

impl Family {
    const ALL: [Self; 5] = [
        Self::WidebandMultibin,
        Self::AdjacentMultibin,
        Self::NarrowbandBinaryFskLike,
        Self::NarrowbandConstantEnvelopeUnresolved,
        Self::Unclassified,
    ];

    fn index(self) -> usize {
        match self {
            Self::WidebandMultibin => 0,
            Self::AdjacentMultibin => 1,
            Self::NarrowbandBinaryFskLike => 2,
            Self::NarrowbandConstantEnvelopeUnresolved => 3,
            Self::Unclassified => 4,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WidebandMultibin => "wideband_multibin_candidate",
            Self::AdjacentMultibin => "adjacent_multibin_candidate",
            Self::NarrowbandBinaryFskLike => "narrowband_binary_fsk_like_candidate",
            Self::NarrowbandConstantEnvelopeUnresolved => {
                "narrowband_constant_envelope_unresolved_candidate"
            }
            Self::Unclassified => "unclassified_burst_group",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct FamilyCount {
    candidate_family: &'static str,
    group_count: u64,
    record_count: u64,
    complex_sample_count: u64,
}

#[derive(Clone, Debug, Serialize)]
struct FrequencyFamilyCount {
    candidate_family: &'static str,
    frequency_mhz: u32,
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    operation: &'static str,
    source: SourceIdentity,
    accounting: Accounting,
    settings: Settings,
    families: Vec<FamilyCount>,
    frequency_family_counts: Vec<FrequencyFamilyCount>,
    eligible_record_features: FeatureSummaries,
    protocol_identity_established: bool,
    interpretation: &'static str,
}

#[derive(Debug, Default)]
struct FeatureAccumulators {
    envelope_cv: NumericAccumulator,
    usable: NumericAccumulator,
    concentration: NumericAccumulator,
    separation: NumericAccumulator,
    reduction: NumericAccumulator,
    contrast: NumericAccumulator,
    balance: NumericAccumulator,
    transitions: NumericAccumulator,
}

impl FeatureAccumulators {
    fn observe(&mut self, feature: RecordFeature) {
        self.envelope_cv.observe(feature.envelope_cv);
        self.usable.observe(feature.usable_phase_differences as f64);
        self.concentration.observe(feature.phase_concentration);
        if let Some(two_level) = feature.two_level {
            self.separation.observe(two_level.separation_hz);
            self.reduction.observe(two_level.variance_reduction);
            self.contrast.observe(two_level.mode_contrast);
            self.balance.observe(two_level.cluster_balance);
            self.transitions.observe(two_level.transitions as f64);
        }
    }

    fn finish(self) -> FeatureSummaries {
        FeatureSummaries {
            envelope_cv: self.envelope_cv.finish(),
            usable_phase_differences: self.usable.finish(),
            phase_concentration: self.concentration.finish(),
            two_level_separation_hz: self.separation.finish(),
            two_level_variance_reduction: self.reduction.finish(),
            two_level_mode_contrast: self.contrast.finish(),
            two_level_cluster_balance: self.balance.finish(),
            two_level_transitions: self.transitions.finish(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SweepPoint {
    time_ns: u128,
    kind: u8,
    frequency_index: usize,
}

fn simultaneous_width(events: &[Event]) -> usize {
    let mut points = Vec::with_capacity(events.len().saturating_mul(2));
    for event in events {
        if event.start_ns < event.end_ns {
            let frequency_index = (event.frequency_mhz - LOWEST_FREQUENCY_MHZ) as usize;
            points.push(SweepPoint {
                time_ns: event.end_ns,
                kind: 0,
                frequency_index,
            });
            points.push(SweepPoint {
                time_ns: event.start_ns,
                kind: 1,
                frequency_index,
            });
        }
    }
    points.sort_by(|left, right| {
        (left.time_ns, left.kind, left.frequency_index).cmp(&(
            right.time_ns,
            right.kind,
            right.frequency_index,
        ))
    });
    let mut active = [0u32; FREQUENCY_COUNT];
    let mut maximum = 0usize;
    let mut index = 0usize;
    while index < points.len() {
        let time = points[index].time_ns;
        while index < points.len() && points[index].time_ns == time && points[index].kind == 0 {
            let frequency = points[index].frequency_index;
            debug_assert!(active[frequency] > 0);
            active[frequency] -= 1;
            index += 1;
        }
        while index < points.len() && points[index].time_ns == time {
            active[points[index].frequency_index] += 1;
            index += 1;
        }
        let mut run = 0usize;
        for count in active {
            if count > 0 {
                run += 1;
                maximum = maximum.max(run);
            } else {
                run = 0;
            }
        }
    }
    maximum
}

fn classify_group(events: &[Event]) -> Family {
    let width = simultaneous_width(events);
    if width >= WIDEBAND_MULTIBIN_MIN_WIDTH {
        return Family::WidebandMultibin;
    }
    let eligible = events.iter().filter_map(|event| event.feature);
    let (eligible_count, fsk_like_count, constant_count) = eligible.fold(
        (0u64, 0u64, 0u64),
        |(eligible_count, fsk_like_count, constant_count), feature| {
            (
                eligible_count + 1,
                fsk_like_count + u64::from(feature.fsk_like),
                constant_count + u64::from(feature.constant_envelope),
            )
        },
    );
    let mut occupied = [false; FREQUENCY_COUNT];
    for event in events {
        occupied[(event.frequency_mhz - LOWEST_FREQUENCY_MHZ) as usize] = true;
    }
    let first_frequency = occupied.iter().position(|present| *present);
    let last_frequency = occupied.iter().rposition(|present| *present);
    let distinct_frequencies = occupied.iter().filter(|present| **present).count();
    let narrow_footprint = distinct_frequencies <= 2
        && first_frequency
            .zip(last_frequency)
            .is_none_or(|(first, last)| last - first <= GROUP_FREQUENCY_STEP_MHZ);
    let record_count = events.len() as u64;
    if narrow_footprint
        && width <= ADJACENT_MULTIBIN_MIN_WIDTH
        && eligible_count > 0
        && fsk_like_count * 2 > record_count
    {
        Family::NarrowbandBinaryFskLike
    } else if width >= ADJACENT_MULTIBIN_MIN_WIDTH {
        Family::AdjacentMultibin
    } else if narrow_footprint && eligible_count > 0 && constant_count * 2 > record_count {
        Family::NarrowbandConstantEnvelopeUnresolved
    } else {
        Family::Unclassified
    }
}

#[derive(Debug)]
struct DisjointSets {
    parents: Vec<usize>,
    sizes: Vec<usize>,
}

impl DisjointSets {
    fn new(length: usize) -> Self {
        Self {
            parents: (0..length).collect(),
            sizes: vec![1; length],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let parent = self.parents[index];
        if parent == index {
            index
        } else {
            let root = self.find(parent);
            self.parents[index] = root;
            root
        }
    }

    fn union(&mut self, left: usize, right: usize) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root != right_root {
            let (root, child) = if self.sizes[left_root] > self.sizes[right_root]
                || (self.sizes[left_root] == self.sizes[right_root] && left_root < right_root)
            {
                (left_root, right_root)
            } else {
                (right_root, left_root)
            };
            self.parents[child] = root;
            self.sizes[root] += self.sizes[child];
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FrequencyAggregate {
    group_incidence_count: u64,
    record_count: u64,
    complex_sample_count: u64,
}

#[derive(Debug)]
struct GroupAggregates {
    group_count: u64,
    family_groups: [u64; 5],
    family_records: [u64; 5],
    family_samples: [u64; 5],
    frequency_families: [[FrequencyAggregate; FREQUENCY_COUNT]; 5],
}

impl GroupAggregates {
    fn empty() -> Self {
        Self {
            group_count: 0,
            family_groups: [0; 5],
            family_records: [0; 5],
            family_samples: [0; 5],
            frequency_families: [[FrequencyAggregate::default(); FREQUENCY_COUNT]; 5],
        }
    }
}

#[cfg(test)]
fn aggregate_groups(
    events: &mut [Event],
    max_groups: u64,
    interrupted: &AtomicBool,
) -> Result<GroupAggregates> {
    aggregate_groups_with_breakdown(events, max_groups, interrupted, None, None)
        .map(|(aggregates, _, _)| aggregates)
}

fn aggregate_groups_with_breakdown(
    events: &mut [Event],
    max_groups: u64,
    interrupted: &AtomicBool,
    selection: Option<breakdown::Selection>,
    record_selection: Option<RecordSelection>,
) -> Result<(
    GroupAggregates,
    Option<breakdown::Accumulator>,
    RecordSelectionResult,
)> {
    let mut feature_breakdown = selection.map(breakdown::Accumulator::new);
    let mut selected = RecordSelectionResult::default();
    events.sort_by(|left, right| {
        (
            left.start_ns,
            left.end_ns,
            left.frequency_mhz,
            left.archive_index,
        )
            .cmp(&(
                right.start_ns,
                right.end_ns,
                right.frequency_mhz,
                right.archive_index,
            ))
    });
    if events.is_empty() {
        return Ok((GroupAggregates::empty(), feature_breakdown, selected));
    }
    let mut sets = DisjointSets::new(events.len());
    let mut active_by_frequency: [Option<(usize, u128)>; FREQUENCY_COUNT] = [None; FREQUENCY_COUNT];
    for (index, event) in events.iter().copied().enumerate() {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "burst processing interrupted"
        );
        let frequency = (event.frequency_mhz - LOWEST_FREQUENCY_MHZ) as usize;
        let first = frequency.saturating_sub(GROUP_FREQUENCY_STEP_MHZ);
        let last = (frequency + GROUP_FREQUENCY_STEP_MHZ).min(FREQUENCY_COUNT - 1);
        for active in active_by_frequency[first..=last].iter().flatten() {
            if active.1 >= event.start_ns {
                sets.union(index, active.0);
            }
        }
        let reach = event.end_ns.saturating_add(GROUP_GAP_NS);
        match active_by_frequency[frequency] {
            Some((_, previous_reach)) if previous_reach >= reach => {}
            _ => active_by_frequency[frequency] = Some((index, reach)),
        }
    }

    let mut membership = Vec::with_capacity(events.len());
    for index in 0..events.len() {
        membership.push((sets.find(index), index));
    }
    membership.sort_unstable();
    let mut aggregates = GroupAggregates::empty();
    let mut begin = 0usize;
    while begin < membership.len() {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "burst processing interrupted"
        );
        ensure!(
            aggregates.group_count < max_groups,
            "burst group limit exceeded"
        );
        let root = membership[begin].0;
        let mut end = begin + 1;
        while end < membership.len() && membership[end].0 == root {
            end += 1;
        }
        let group: Vec<Event> = membership[begin..end]
            .iter()
            .map(|(_, index)| events[*index])
            .collect();
        let family = classify_group(&group);
        if record_selection.is_some_and(|selection| selection.family == family) {
            let mut group_selected = false;
            for event in &group {
                if record_selection
                    .is_some_and(|selection| selection.frequency_mhz == event.frequency_mhz)
                {
                    selected.archive_indices.push(event.archive_index);
                    group_selected = true;
                }
            }
            selected.group_count += u64::from(group_selected);
        }
        if let Some(feature_breakdown) = &mut feature_breakdown {
            feature_breakdown.observe_group(family, &group)?;
        }
        let family_index = family.index();
        aggregates.family_groups[family_index] = aggregates.family_groups[family_index]
            .checked_add(1)
            .context("family group count overflow")?;
        let mut group_frequencies = [false; FREQUENCY_COUNT];
        for event in &group {
            aggregates.family_records[family_index] = aggregates.family_records[family_index]
                .checked_add(1)
                .context("family record count overflow")?;
            aggregates.family_samples[family_index] = aggregates.family_samples[family_index]
                .checked_add(event.sample_count)
                .context("family sample count overflow")?;
            let frequency_index = (event.frequency_mhz - LOWEST_FREQUENCY_MHZ) as usize;
            let frequency = &mut aggregates.frequency_families[family_index][frequency_index];
            frequency.record_count = frequency
                .record_count
                .checked_add(1)
                .context("frequency-family record count overflow")?;
            frequency.complex_sample_count = frequency
                .complex_sample_count
                .checked_add(event.sample_count)
                .context("frequency-family sample count overflow")?;
            group_frequencies[frequency_index] = true;
        }
        for (frequency_index, present) in group_frequencies.into_iter().enumerate() {
            if present {
                let frequency = &mut aggregates.frequency_families[family_index][frequency_index];
                frequency.group_incidence_count = frequency
                    .group_incidence_count
                    .checked_add(1)
                    .context("frequency-family group incidence overflow")?;
            }
        }
        aggregates.group_count += 1;
        begin = end;
    }
    selected.archive_indices.sort_unstable();
    Ok((aggregates, feature_breakdown, selected))
}

#[derive(Debug)]
struct Classification {
    summary: Summary,
    feature_breakdown: Option<breakdown::Report>,
}

fn classifier_settings() -> Settings {
    Settings {
        channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
        sample_period_ns: SAMPLE_PERIOD_NS as u64,
        group_connectivity_gap_ns: GROUP_GAP_NS as u64,
        group_frequency_step_mhz: GROUP_FREQUENCY_STEP_MHZ as u64,
        simultaneous_interval: "half_open_unexpanded_record_interval",
        active_power_quantile: [
            ACTIVE_POWER_QUANTILE_NUMERATOR as u64,
            ACTIVE_POWER_QUANTILE_DENOMINATOR as u64,
        ],
        active_power_divisor: ACTIVE_POWER_DIVISOR,
        minimum_usable_phase_differences: MIN_USABLE_PHASE_DIFFERENCES as u64,
        maximum_envelope_cv: MAX_ENVELOPE_CV,
        minimum_cluster_balance: MIN_CLUSTER_BALANCE,
        minimum_variance_reduction: MIN_VARIANCE_REDUCTION,
        minimum_mode_contrast: MIN_MODE_CONTRAST,
        mode_contrast_std_floor_hz: MODE_CONTRAST_STD_FLOOR_HZ,
        minimum_phase_concentration: MIN_PHASE_CONCENTRATION,
        separation_hz: [MIN_SEPARATION_HZ, MAX_SEPARATION_HZ],
        minimum_transitions: MIN_TRANSITIONS,
        kmeans_iterations: KMEANS_ITERATIONS as u64,
        adjacent_multibin_minimum_width: ADJACENT_MULTIBIN_MIN_WIDTH as u64,
        wideband_multibin_minimum_width: WIDEBAND_MULTIBIN_MIN_WIDTH as u64,
    }
}

#[cfg(test)]
fn classify(path: &Path, limits: Limits, interrupted: &AtomicBool) -> Result<Summary> {
    classify_with_breakdown(path, limits, interrupted, None)
        .map(|classification| classification.summary)
}

fn classify_with_breakdown(
    path: &Path,
    limits: Limits,
    interrupted: &AtomicBool,
    selection: Option<breakdown::Selection>,
) -> Result<Classification> {
    let mut events = Vec::new();
    let mut eligible_records = 0u64;
    let mut insufficient_records = 0u64;
    let mut squelch_samples = 0u64;
    let mut scan_samples = 0u64;
    let mut features = FeatureAccumulators::default();
    let facts = bursts::stream_archive(
        path,
        ParseLimits {
            max_input_bytes: limits.max_input_bytes,
            max_records: limits.max_records,
            max_total_samples: limits.max_samples,
        },
        interrupted,
        true,
        |metadata, packed_iq| {
            if metadata.scan {
                scan_samples = scan_samples
                    .checked_add(u64::from(metadata.sample_count))
                    .context("scan sample total overflow")?;
                return Ok(());
            }
            squelch_samples = squelch_samples
                .checked_add(u64::from(metadata.sample_count))
                .context("squelch sample total overflow")?;
            let feature = record_feature(metadata, packed_iq, interrupted)?;
            if let Some(feature) = feature {
                eligible_records += 1;
                features.observe(feature);
            } else {
                insufficient_records += 1;
            }
            let start_ns = u128::from(metadata.timestamp_seconds)
                .checked_mul(1_000_000_000)
                .and_then(|seconds| seconds.checked_add(u128::from(metadata.timestamp_nanoseconds)))
                .context("burst timestamp overflow")?;
            let duration_ns = u128::from(metadata.sample_count)
                .checked_mul(SAMPLE_PERIOD_NS)
                .context("burst duration overflow")?;
            let end_ns = start_ns
                .checked_add(duration_ns)
                .context("burst end timestamp overflow")?;
            events.push(Event {
                archive_index: metadata.archive_index,
                start_ns,
                end_ns,
                frequency_mhz: metadata.frequency_mhz,
                sample_count: u64::from(metadata.sample_count),
                feature,
            });
            Ok(())
        },
    )?;
    ensure!(
        facts.archive_record_count == facts.squelch_record_count + facts.scan_window_record_count,
        "internal archive record accounting mismatch"
    );
    ensure!(
        facts.complex_sample_count == squelch_samples + scan_samples,
        "internal archive sample accounting mismatch"
    );
    ensure!(
        facts.squelch_record_count == eligible_records + insufficient_records,
        "internal feature accounting mismatch"
    );
    ensure!(
        events.len() as u64 == facts.squelch_record_count,
        "internal squelch event accounting mismatch"
    );

    let (aggregates, feature_breakdown, selected) = aggregate_groups_with_breakdown(
        &mut events,
        limits.max_groups,
        interrupted,
        selection,
        None,
    )?;
    debug_assert!(selected.archive_indices.is_empty());
    ensure!(
        aggregates.family_groups.iter().sum::<u64>() == aggregates.group_count,
        "internal family group accounting mismatch"
    );
    ensure!(
        aggregates.family_records.iter().sum::<u64>() == facts.squelch_record_count,
        "internal grouped record accounting mismatch"
    );
    ensure!(
        aggregates.family_samples.iter().sum::<u64>() == squelch_samples,
        "internal grouped sample accounting mismatch"
    );
    let feature_breakdown = match (selection, feature_breakdown) {
        (Some(selection), Some(feature_breakdown)) => {
            let frequency_index = (selection.frequency_mhz() - LOWEST_FREQUENCY_MHZ) as usize;
            let expected =
                aggregates.frequency_families[selection.family().index()][frequency_index];
            Some(feature_breakdown.finish(expected)?)
        }
        (None, None) => None,
        _ => bail!("internal feature-breakdown selection mismatch"),
    };
    let families = Family::ALL
        .into_iter()
        .map(|family| FamilyCount {
            candidate_family: family.label(),
            group_count: aggregates.family_groups[family.index()],
            record_count: aggregates.family_records[family.index()],
            complex_sample_count: aggregates.family_samples[family.index()],
        })
        .collect::<Vec<_>>();
    let mut frequency_family_counts = Vec::new();
    for family in Family::ALL {
        let family_index = family.index();
        let mut family_records = 0u64;
        let mut family_samples = 0u64;
        for (frequency_index, frequency) in aggregates.frequency_families[family_index]
            .iter()
            .copied()
            .enumerate()
        {
            if frequency.record_count == 0 {
                ensure!(
                    frequency.group_incidence_count == 0 && frequency.complex_sample_count == 0,
                    "empty frequency-family row has counts"
                );
                continue;
            }
            ensure!(
                frequency.group_incidence_count > 0
                    && frequency.group_incidence_count <= aggregates.family_groups[family_index]
                    && frequency.group_incidence_count <= frequency.record_count,
                "frequency-family group incidence differs"
            );
            family_records = family_records
                .checked_add(frequency.record_count)
                .context("frequency-family record partition overflow")?;
            family_samples = family_samples
                .checked_add(frequency.complex_sample_count)
                .context("frequency-family sample partition overflow")?;
            frequency_family_counts.push(FrequencyFamilyCount {
                candidate_family: family.label(),
                frequency_mhz: LOWEST_FREQUENCY_MHZ + frequency_index as u32,
                group_incidence_count: frequency.group_incidence_count,
                record_count: frequency.record_count,
                complex_sample_count: frequency.complex_sample_count,
            });
        }
        ensure!(
            family_records == aggregates.family_records[family_index]
                && family_samples == aggregates.family_samples[family_index],
            "frequency-family partition differs from family totals"
        );
    }

    let source = SourceIdentity {
        format: "blue_dragon_burst_v1",
        bytes: facts.source_bytes,
        sha256: facts.sha256,
    };
    let settings = classifier_settings();
    let feature_breakdown =
        feature_breakdown.map(|data| breakdown::report(source.clone(), settings.clone(), data));
    let summary = Summary {
        schema_version: 2,
        operation: "classify_bursts",
        source,
        accounting: Accounting {
            archive_record_count: facts.archive_record_count,
            squelch_record_count: facts.squelch_record_count,
            scan_window_record_count: facts.scan_window_record_count,
            archive_complex_sample_count: facts.complex_sample_count,
            squelch_complex_sample_count: squelch_samples,
            scan_window_complex_sample_count: scan_samples,
            feature_eligible_record_count: eligible_records,
            feature_insufficient_record_count: insufficient_records,
            group_count: aggregates.group_count,
            grouped_squelch_record_count: events.len() as u64,
        },
        settings,
        families,
        frequency_family_counts,
        eligible_record_features: features.finish(),
        protocol_identity_established: false,
        interpretation: "Provisional waveform-family candidates from channelized squelch records. Scan windows are validated and counted but excluded. Multibin width is the maximum simultaneously active contiguous 1 MHz bins, not a chained union. Frequency-family group incidence counts each group once per distinct channelized bin; overlapping bins mean it is neither a unique-signal count nor occupancy. FSK-like means bounded constant-envelope/two-level discriminator evidence only; it does not identify Bluetooth or any other protocol. AGC, squelch tails, optional pre-roll, 2 Msps phase aliasing and detector-aligned host timestamps limit inference. No packet bits, device identifiers, absolute timestamps or source paths are emitted.",
    };
    Ok(Classification {
        summary,
        feature_breakdown,
    })
}

fn encode_summary(summary: &Summary, max_output_bytes: u64) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec_pretty(summary)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() as u64 <= max_output_bytes,
        "classification report exceeds the {max_output_bytes}-byte output limit"
    );
    Ok(encoded)
}

fn publish_summary(path: &Path, encoded: &[u8], interrupted: &AtomicBool) -> Result<()> {
    ensure!(!path.exists(), "output already exists: {}", path.display());
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = None;
    for _ in 0..128 {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".burst-families-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create private output in {}", parent.display()));
            }
        }
    }
    let Some((temporary_path, mut file)) = temporary else {
        bail!("could not reserve a temporary classification output")
    };
    let mut published = false;
    let result = (|| -> Result<()> {
        file.write_all(encoded)
            .context("write classification report")?;
        file.sync_all().context("sync classification report")?;
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "burst processing interrupted before report publication"
        );
        fs::hard_link(&temporary_path, path)
            .with_context(|| format!("publish {} without overwriting", path.display()))?;
        published = true;
        File::open(parent)?.sync_all()?;
        fs::remove_file(&temporary_path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        if published {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_file(&temporary_path);
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
    }
    result
}

fn publish_classification(
    output: &Path,
    summary: &[u8],
    feature_breakdown: Option<&[u8]>,
    interrupted: &AtomicBool,
) -> Result<()> {
    let summary_path = output.join("burst-families.json");
    let feature_breakdown_path = output.join("feature-breakdown.json");
    if let Some(feature_breakdown) = feature_breakdown {
        publish_summary(&feature_breakdown_path, feature_breakdown, interrupted)?;
        if let Err(error) = publish_summary(&summary_path, summary, interrupted) {
            if let Err(cleanup_error) = fs::remove_file(&feature_breakdown_path) {
                return Err(error).context(format!(
                    "also could not remove partial feature breakdown: {cleanup_error}"
                ));
            }
            File::open(output)?.sync_all()?;
            return Err(error);
        }
        return Ok(());
    }
    publish_summary(&summary_path, summary, interrupted)
}

/// Selects the exact records contributing to one classified family/frequency row.
///
/// The archive is validated twice: the first pass classifies all squelch records
/// and fixes group membership, while the second retains only the selected IQ.
/// Both complete-file identities must agree before the result is returned.
pub(crate) fn select_family_frequency(
    path: &Path,
    limits: ParseLimits,
    max_groups: u64,
    family: Family,
    frequency_mhz: u32,
    interrupted: &AtomicBool,
) -> Result<SelectedArchive> {
    ensure!(
        (LOWEST_FREQUENCY_MHZ..=HIGHEST_FREQUENCY_MHZ).contains(&frequency_mhz),
        "selection frequency must be within {LOWEST_FREQUENCY_MHZ}..={HIGHEST_FREQUENCY_MHZ} MHz"
    );
    ensure!(
        (1..=HARD_MAX_GROUPS).contains(&max_groups),
        "burst group limit must be 1..={HARD_MAX_GROUPS}"
    );
    let mut events = Vec::new();
    let first = bursts::stream_archive(path, limits, interrupted, true, |metadata, packed_iq| {
        if metadata.scan {
            return Ok(());
        }
        let feature = record_feature(metadata, packed_iq, interrupted)?;
        let start_ns = u128::from(metadata.timestamp_seconds)
            .checked_mul(1_000_000_000)
            .and_then(|seconds| seconds.checked_add(u128::from(metadata.timestamp_nanoseconds)))
            .context("burst timestamp overflow")?;
        let duration_ns = u128::from(metadata.sample_count)
            .checked_mul(SAMPLE_PERIOD_NS)
            .context("burst duration overflow")?;
        events.push(Event {
            archive_index: metadata.archive_index,
            start_ns,
            end_ns: start_ns
                .checked_add(duration_ns)
                .context("burst end timestamp overflow")?,
            frequency_mhz: metadata.frequency_mhz,
            sample_count: u64::from(metadata.sample_count),
            feature,
        });
        Ok(())
    })?;
    let (_, _, selected) = aggregate_groups_with_breakdown(
        &mut events,
        max_groups,
        interrupted,
        None,
        Some(RecordSelection {
            family,
            frequency_mhz,
        }),
    )?;
    ensure!(
        !selected.archive_indices.is_empty(),
        "requested family/frequency row is absent"
    );

    let mut records = Vec::with_capacity(selected.archive_indices.len());
    let second = bursts::stream_archive(path, limits, interrupted, true, |metadata, packed_iq| {
        if selected
            .archive_indices
            .binary_search(&metadata.archive_index)
            .is_ok()
        {
            ensure!(
                !metadata.scan,
                "selected row unexpectedly contains a scan window"
            );
            records.push(SelectedRecord {
                metadata,
                packed_iq: packed_iq.to_vec(),
            });
        }
        Ok(())
    })?;
    ensure!(
        first.source_bytes == second.source_bytes
            && first.sha256 == second.sha256
            && first.archive_record_count == second.archive_record_count
            && first.squelch_record_count == second.squelch_record_count
            && first.scan_window_record_count == second.scan_window_record_count
            && first.complex_sample_count == second.complex_sample_count,
        "burst archive identity changed between selection passes"
    );
    ensure!(
        records.len() == selected.archive_indices.len(),
        "selected record accounting changed between passes"
    );
    Ok(SelectedArchive {
        source_bytes: first.source_bytes,
        sha256: first.sha256,
        archive_record_count: first.archive_record_count,
        selected_group_count: selected.group_count,
        records,
    })
}

pub(crate) fn run(root: &Path, args: &ClassifyBursts, interrupted: &AtomicBool) -> Result<()> {
    let limits = args.limits()?;
    let selection = args.feature_breakdown_selection()?;
    let classification = classify_with_breakdown(&args.input, limits, interrupted, selection)?;
    let encoded = encode_summary(&classification.summary, limits.max_output_bytes)?;
    let feature_breakdown = classification
        .feature_breakdown
        .as_ref()
        .map(|report| breakdown::encode(report, limits.max_output_bytes))
        .transpose()?;
    let combined_output_bytes =
        feature_breakdown
            .as_ref()
            .map_or(Ok(encoded.len()), |feature_breakdown| {
                encoded
                    .len()
                    .checked_add(feature_breakdown.len())
                    .context("combined classification output size overflow")
            })?;
    ensure!(
        combined_output_bytes as u64 <= limits.max_output_bytes,
        "combined classification output exceeds the {}-byte output limit",
        limits.max_output_bytes
    );
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "burst processing interrupted before output creation"
    );
    let output = root.join(crate::output_path(
        args.output.as_deref(),
        "artifacts/bluetooth-burst-families",
    ));
    crate::new_directory(&output)?;
    publish_classification(&output, &encoded, feature_breakdown.as_deref(), interrupted)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "{}",
        serde_json::to_string_pretty(&classification.summary)?
    )?;
    if classification.feature_breakdown.is_some() {
        writeln!(
            stdout,
            "Feature breakdown: {}",
            output.join("feature-breakdown.json").display()
        )?;
    }
    writeln!(
        stdout,
        "Burst-family analysis directory: {}",
        output.display()
    )?;
    stdout.flush().context("flush burst-family output")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::{PI, TAU};
    use std::os::unix::fs::{MetadataExt, symlink};

    const FILE_HEADER_LEN: usize = 16;
    const RECORD_HEADER_LEN: usize = 48;

    fn packed(samples: &[(i16, i16)]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 4);
        for &(i, q) in samples {
            bytes.extend_from_slice(&i.to_le_bytes());
            bytes.extend_from_slice(&q.to_le_bytes());
        }
        bytes
    }

    fn oscillator(
        frequencies_hz: impl IntoIterator<Item = f64>,
        amplitude: f64,
    ) -> Vec<(i16, i16)> {
        let mut phase: f64 = 0.0;
        frequencies_hz
            .into_iter()
            .map(|frequency| {
                phase += TAU * frequency / f64::from(CHANNEL_SAMPLE_RATE_HZ);
                (
                    (amplitude * phase.cos()).round() as i16,
                    (amplitude * phase.sin()).round() as i16,
                )
            })
            .collect()
    }

    fn binary_fsk(sample_count: usize, smoothing: f64, carrier_hz: f64) -> Vec<(i16, i16)> {
        let mut frequency = carrier_hz - 250_000.0;
        let targets = (0..sample_count).map(|index| {
            let symbol = (index / 16).is_multiple_of(2);
            let target = carrier_hz + if symbol { -250_000.0 } else { 250_000.0 };
            frequency += smoothing * (target - frequency);
            frequency
        });
        oscillator(targets, 12_000.0)
    }

    fn metadata(sample_count: usize) -> RecordMetadata {
        RecordMetadata {
            archive_index: 0,
            timestamp_seconds: 1,
            timestamp_nanoseconds: 0,
            frequency_mhz: 2437,
            relative_rssi_db: -40.0,
            relative_noise_db: -80.0,
            sample_count: sample_count as u32,
            scan: false,
        }
    }

    fn feature(samples: &[(i16, i16)]) -> Option<RecordFeature> {
        record_feature(
            metadata(samples.len()),
            &packed(samples),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    fn file_header() -> Vec<u8> {
        let mut bytes = Vec::from(*b"BDBURST\0");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&(FILE_HEADER_LEN as u16).to_le_bytes());
        bytes.extend_from_slice(&CHANNEL_SAMPLE_RATE_HZ.to_le_bytes());
        bytes
    }

    fn append_record(
        bytes: &mut Vec<u8>,
        seconds: u64,
        nanoseconds: u32,
        frequency_mhz: u32,
        scan: bool,
        sequence: u32,
        samples: &[(i16, i16)],
    ) {
        let length = RECORD_HEADER_LEN as u32 + samples.len() as u32 * 4;
        bytes.extend_from_slice(b"BRST");
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&seconds.to_le_bytes());
        bytes.extend_from_slice(&nanoseconds.to_le_bytes());
        bytes.extend_from_slice(&frequency_mhz.to_le_bytes());
        bytes.extend_from_slice(&(-40.0f32).to_le_bytes());
        bytes.extend_from_slice(&(-80.0f32).to_le_bytes());
        bytes.extend_from_slice(&sequence.to_le_bytes());
        bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&u32::from(scan).to_le_bytes());
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&packed(samples));
    }

    fn write_archive(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn test_limits(bytes: usize) -> Limits {
        Limits {
            max_input_bytes: bytes as u64,
            max_samples: 1_000_000,
            max_records: 100,
            max_groups: 100,
            max_output_bytes: 1024 * 1024,
        }
    }

    fn family_count<'a>(summary: &'a Summary, label: &str) -> &'a FamilyCount {
        summary
            .families
            .iter()
            .find(|family| family.candidate_family == label)
            .unwrap()
    }

    fn frequency_family_count<'a>(
        summary: &'a Summary,
        label: &str,
        frequency_mhz: u32,
    ) -> &'a FrequencyFamilyCount {
        summary
            .frequency_family_counts
            .iter()
            .find(|count| count.candidate_family == label && count.frequency_mhz == frequency_mhz)
            .unwrap()
    }

    fn assert_frequency_partitions(summary: &Summary) {
        let mut previous = None;
        for row in &summary.frequency_family_counts {
            let family_index = Family::ALL
                .iter()
                .position(|family| family.label() == row.candidate_family)
                .unwrap();
            let key = (family_index, row.frequency_mhz);
            assert!(previous.is_none_or(|previous| previous < key));
            previous = Some(key);
            let family = family_count(summary, row.candidate_family);
            assert!(row.group_incidence_count > 0);
            assert!(row.group_incidence_count <= family.group_count);
            assert!(row.group_incidence_count <= row.record_count);
        }
        for family in &summary.families {
            let rows = summary
                .frequency_family_counts
                .iter()
                .filter(|row| row.candidate_family == family.candidate_family);
            let (records, samples) = rows.fold((0u64, 0u64), |(records, samples), row| {
                (
                    records + row.record_count,
                    samples + row.complex_sample_count,
                )
            });
            assert_eq!(records, family.record_count);
            assert_eq!(samples, family.complex_sample_count);
        }
    }

    fn event(start_ns: u128, end_ns: u128, frequency_mhz: u32) -> Event {
        Event {
            archive_index: start_ns as u64,
            start_ns,
            end_ns,
            frequency_mhz,
            sample_count: 1,
            feature: None,
        }
    }

    #[test]
    fn recognizes_independent_binary_and_smoothed_fsk_fixtures() {
        for samples in [
            binary_fsk(2048, 1.0, 0.0),
            binary_fsk(2048, 0.25, 170_000.0),
            binary_fsk(2048, 0.25, 850_000.0),
        ] {
            let feature = feature(&samples).unwrap();
            assert!(feature.fsk_like, "{feature:?}");
            let fit = feature.two_level.unwrap();
            assert!(
                (300_000.0..=600_000.0).contains(&fit.separation_hz),
                "{fit:?}"
            );
            assert!(fit.mode_contrast >= MIN_MODE_CONTRAST);
        }
        let conjugated: Vec<_> = binary_fsk(2048, 1.0, -120_000.0)
            .into_iter()
            .map(|(i, q)| (i, -q))
            .collect();
        assert!(feature(&conjugated).unwrap().fsk_like);
    }

    #[test]
    fn rejects_tone_am_qpsk_random_phase_and_multicarrier_controls() {
        let tone = oscillator(std::iter::repeat_n(170_000.0, 2048), 12_000.0);
        assert!(!feature(&tone).unwrap().fsk_like);

        let mut phase: f64 = 0.0;
        let am: Vec<_> = (0..2048)
            .map(|index| {
                phase += TAU * 170_000.0 / f64::from(CHANNEL_SAMPLE_RATE_HZ);
                let amplitude = 7_500.0 + 6_500.0 * (TAU * index as f64 / 97.0).sin();
                (
                    (amplitude * phase.cos()).round() as i16,
                    (amplitude * phase.sin()).round() as i16,
                )
            })
            .collect();
        assert!(!feature(&am).unwrap().fsk_like);

        let increments = [0.0, PI / 2.0, PI, -PI / 2.0];
        let mut qpsk_phase: f64 = 0.0;
        let qpsk: Vec<_> = (0..2048)
            .map(|index| {
                qpsk_phase += increments[index % increments.len()];
                (
                    (12_000.0 * qpsk_phase.cos()).round() as i16,
                    (12_000.0 * qpsk_phase.sin()).round() as i16,
                )
            })
            .collect();
        assert!(!feature(&qpsk).unwrap().fsk_like);

        let mut state = 0x9e37_79b9u32;
        let random_frequencies = (0..4096).map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (f64::from(state) / f64::from(u32::MAX) - 0.5) * 1_800_000.0
        });
        let random_phase = oscillator(random_frequencies, 12_000.0);
        assert!(!feature(&random_phase).unwrap().fsk_like);

        let carriers = [
            -700_000.0, -430_000.0, -170_000.0, 110_000.0, 390_000.0, 690_000.0,
        ];
        let multicarrier: Vec<_> = (0..4096)
            .map(|index| {
                let time = index as f64 / f64::from(CHANNEL_SAMPLE_RATE_HZ);
                let i = carriers
                    .iter()
                    .enumerate()
                    .map(|(carrier, frequency)| {
                        (TAU * frequency * time + carrier as f64 * 0.37).cos()
                    })
                    .sum::<f64>()
                    * 3_000.0;
                let q = carriers
                    .iter()
                    .enumerate()
                    .map(|(carrier, frequency)| {
                        (TAU * frequency * time + carrier as f64 * 0.37).sin()
                    })
                    .sum::<f64>()
                    * 3_000.0;
                (i.round() as i16, q.round() as i16)
            })
            .collect();
        assert!(!feature(&multicarrier).unwrap().fsk_like);
    }

    #[test]
    fn usable_phase_boundary_is_exact() {
        for (samples, eligible) in [(128usize, false), (129, true), (130, true)] {
            let waveform = binary_fsk(samples, 1.0, 0.0);
            assert_eq!(feature(&waveform).is_some(), eligible);
        }
    }

    #[test]
    fn inactive_gaps_do_not_create_discriminator_transitions() {
        let mut values = Vec::new();
        let mut continuity = Vec::new();
        for run in 0usize..8 {
            for index in 0..16 {
                values.push(if run.is_multiple_of(2) {
                    -250_000.0
                } else {
                    250_000.0
                });
                continuity.push(index > 0);
            }
        }
        let fit = fit_two_levels(&values, &continuity, 1.0).unwrap();
        assert_eq!(fit.transitions, 0);
    }

    #[test]
    fn simultaneous_width_does_not_inflate_chains_or_duplicate_bins() {
        let simultaneous = [
            event(0, 100, 2420),
            event(0, 100, 2421),
            event(0, 100, 2422),
            event(0, 100, 2423),
        ];
        assert_eq!(simultaneous_width(&simultaneous), 4);
        assert!(matches!(
            classify_group(&simultaneous),
            Family::WidebandMultibin
        ));

        let chain = [
            event(0, 100, 2420),
            event(80, 180, 2421),
            event(160, 260, 2422),
            event(240, 340, 2423),
        ];
        assert_eq!(simultaneous_width(&chain), 2);
        assert!(matches!(classify_group(&chain), Family::AdjacentMultibin));

        let noncontiguous = [
            event(0, 100, 2420),
            event(0, 100, 2422),
            event(0, 100, 2424),
            event(0, 100, 2426),
        ];
        assert_eq!(simultaneous_width(&noncontiguous), 1);
        assert!(!matches!(
            classify_group(&noncontiguous),
            Family::WidebandMultibin | Family::AdjacentMultibin
        ));

        let boundary = [event(0, 100, 2420), event(100, 200, 2421)];
        assert_eq!(simultaneous_width(&boundary), 1);
        let duplicates = [event(0, 100, 2420), event(0, 100, 2420)];
        assert_eq!(simultaneous_width(&duplicates), 1);
    }

    #[test]
    fn classifier_exactly_accounts_scan_eligible_and_insufficient_records() {
        let directory = tempfile::tempdir().unwrap();
        let fsk = binary_fsk(1024, 1.0, 0.0);
        let scan = vec![(0, 0); 32];
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2437, false, 11, &fsk);
        append_record(&mut bytes, 1, 0, 2438, true, 12, &scan);
        append_record(&mut bytes, 1, 10_000, 2437, false, 13, &[]);
        let path = write_archive(directory.path(), "mixed.bdb", &bytes);
        let summary = classify(&path, test_limits(bytes.len()), &AtomicBool::new(false)).unwrap();
        assert_eq!(summary.accounting.archive_record_count, 3);
        assert_eq!(summary.accounting.squelch_record_count, 2);
        assert_eq!(summary.accounting.scan_window_record_count, 1);
        assert_eq!(summary.accounting.feature_eligible_record_count, 1);
        assert_eq!(summary.accounting.feature_insufficient_record_count, 1);
        assert_eq!(summary.accounting.grouped_squelch_record_count, 2);
        assert_eq!(summary.accounting.group_count, 1);
        assert_eq!(
            family_count(&summary, "narrowband_binary_fsk_like_candidate").group_count,
            0
        );
        assert_eq!(
            family_count(&summary, "unclassified_burst_group").group_count,
            1
        );
        assert_eq!(
            summary
                .families
                .iter()
                .map(|item| item.group_count)
                .sum::<u64>(),
            1
        );
        assert_eq!(
            summary
                .families
                .iter()
                .map(|item| item.record_count)
                .sum::<u64>(),
            2
        );
        assert_eq!(
            summary
                .families
                .iter()
                .map(|item| item.complex_sample_count)
                .sum::<u64>(),
            fsk.len() as u64
        );
        assert_eq!(
            summary
                .frequency_family_counts
                .iter()
                .map(|item| item.record_count)
                .sum::<u64>(),
            2
        );
        assert_eq!(
            summary
                .frequency_family_counts
                .iter()
                .map(|item| item.complex_sample_count)
                .sum::<u64>(),
            fsk.len() as u64
        );
        assert_frequency_partitions(&summary);
    }

    #[test]
    fn classifier_emits_wideband_only_for_simultaneous_contiguous_bins() {
        let directory = tempfile::tempdir().unwrap();
        let tone = oscillator(std::iter::repeat_n(100_000.0, 256), 12_000.0);
        let mut bytes = file_header();
        for frequency in 2420..=2423 {
            append_record(&mut bytes, 1, 0, frequency, false, frequency, &tone);
        }
        append_record(&mut bytes, 2, 0, 2437, false, 2500, &[]);
        let path = write_archive(directory.path(), "simultaneous.bdb", &bytes);
        let summary = classify(&path, test_limits(bytes.len()), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            family_count(&summary, "wideband_multibin_candidate").group_count,
            1
        );
        let family = family_count(&summary, "wideband_multibin_candidate");
        assert_eq!(family.record_count, 4);
        assert_eq!(family.complex_sample_count, 4 * tone.len() as u64);
        let rows: Vec<_> = summary
            .frequency_family_counts
            .iter()
            .filter(|row| row.candidate_family == "wideband_multibin_candidate")
            .collect();
        assert_eq!(rows.len(), 4);
        for (row, frequency) in rows.into_iter().zip(2420..=2423) {
            assert_eq!(row.frequency_mhz, frequency);
            assert_eq!(row.group_incidence_count, 1);
            assert_eq!(row.record_count, 1);
            assert_eq!(row.complex_sample_count, tone.len() as u64);
        }
        assert_eq!(summary.frequency_family_counts.len(), 5);
        assert_eq!(
            summary
                .frequency_family_counts
                .last()
                .unwrap()
                .candidate_family,
            "unclassified_burst_group"
        );
        assert_eq!(
            summary
                .frequency_family_counts
                .last()
                .unwrap()
                .frequency_mhz,
            2437
        );
        assert_frequency_partitions(&summary);

        let mut chained = file_header();
        for (index, frequency) in (2420..=2423).enumerate() {
            append_record(
                &mut chained,
                1,
                index as u32 * 100_000,
                frequency,
                false,
                frequency,
                &tone,
            );
        }
        let path = write_archive(directory.path(), "chained.bdb", &chained);
        let summary = classify(&path, test_limits(chained.len()), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            family_count(&summary, "wideband_multibin_candidate").group_count,
            0
        );
        assert_eq!(
            family_count(&summary, "adjacent_multibin_candidate").group_count,
            1
        );
    }

    #[test]
    fn frequency_rows_count_groups_once_per_bin_and_preserve_zero_samples() {
        let directory = tempfile::tempdir().unwrap();
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2437, false, 1, &[]);
        append_record(&mut bytes, 1, 0, 2437, false, 2, &[]);
        append_record(&mut bytes, 2, 0, 2437, false, 3, &[]);
        let path = write_archive(directory.path(), "zero-sample-groups.bdb", &bytes);
        let summary = classify(&path, test_limits(bytes.len()), &AtomicBool::new(false)).unwrap();

        let family = family_count(&summary, "unclassified_burst_group");
        assert_eq!(family.group_count, 2);
        assert_eq!(family.record_count, 3);
        assert_eq!(family.complex_sample_count, 0);
        let frequency = frequency_family_count(&summary, "unclassified_burst_group", 2437);
        assert_eq!(frequency.group_incidence_count, 2);
        assert_eq!(frequency.record_count, 3);
        assert_eq!(frequency.complex_sample_count, 0);
        assert_eq!(summary.frequency_family_counts.len(), 1);
        assert_frequency_partitions(&summary);
    }

    #[test]
    fn grouping_separates_simultaneous_nonadjacent_frequency_components() {
        let mut events = vec![
            event(0, 100, 2420),
            event(0, 100, 2422),
            event(0, 100, 2424),
            event(0, 100, 2426),
        ];
        let aggregates = aggregate_groups(&mut events, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(aggregates.group_count, 4);
        assert_eq!(aggregates.family_groups[Family::Unclassified.index()], 4);
        assert_eq!(aggregates.family_records.iter().sum::<u64>(), 4);

        let fsk_feature = feature(&binary_fsk(1024, 1.0, 0.0)).unwrap();
        let mut fsk_events = vec![event(0, 100, 2420), event(0, 100, 2422)];
        for event in &mut fsk_events {
            event.feature = Some(fsk_feature);
        }
        let aggregates = aggregate_groups(&mut fsk_events, 10, &AtomicBool::new(false)).unwrap();
        assert_eq!(aggregates.group_count, 2);
        assert_eq!(
            aggregates.family_groups[Family::NarrowbandBinaryFskLike.index()],
            2
        );
    }

    #[test]
    fn union_by_size_bounds_adversarial_component_depth() {
        let mut sets = DisjointSets::new(100_000);
        for index in (1..100_000).rev() {
            sets.union(index, index - 1);
        }
        let root = sets.find(0);
        for index in 0..100_000 {
            assert_eq!(sets.find(index), root);
        }
    }

    #[test]
    fn feature_breakdown_closes_selected_row_without_changing_schema_two() {
        let directory = tempfile::tempdir().unwrap();
        let tone = oscillator(std::iter::repeat_n(100_000.0, 256), 12_000.0);
        let short = vec![(12_000, 0); 2];
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2438, false, 1, &tone);
        append_record(&mut bytes, 1, 0, 2438, false, 2, &tone);
        append_record(&mut bytes, 1, 0, 2438, false, 3, &short);
        let path = write_archive(
            directory.path(),
            "PRIVATE_FEATURE_BREAKDOWN_SENTINEL.bdb",
            &bytes,
        );
        let limits = test_limits(bytes.len());
        let ordinary = classify(&path, limits, &AtomicBool::new(false)).unwrap();
        let selected = classify_with_breakdown(
            &path,
            limits,
            &AtomicBool::new(false),
            Some(
                breakdown::Selection::new(Family::NarrowbandConstantEnvelopeUnresolved, 2438)
                    .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            encode_summary(&ordinary, limits.max_output_bytes).unwrap(),
            encode_summary(&selected.summary, limits.max_output_bytes).unwrap()
        );
        let report = selected.feature_breakdown.as_ref().unwrap();
        let encoded = breakdown::encode(report, limits.max_output_bytes).unwrap();
        assert_eq!(
            encoded,
            breakdown::encode(report, limits.max_output_bytes).unwrap()
        );
        let text = String::from_utf8(encoded).unwrap();
        assert!(!text.contains("PRIVATE_FEATURE_BREAKDOWN_SENTINEL"));
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["operation"], "diagnose_burst_family_feature_gates");
        assert_eq!(value["selection"]["frequency_mhz"], 2438);
        assert_eq!(value["accounting"]["group_incidence_count"], 1);
        assert_eq!(value["accounting"]["record_count"], 3);
        assert_eq!(value["accounting"]["complex_sample_count"], 514);
        assert_eq!(value["accounting"]["feature_eligible_record_count"], 2);
        assert_eq!(value["accounting"]["feature_insufficient_record_count"], 1);
        let partition_total = value["first_failure_partition"]
            .as_object()
            .unwrap()
            .values()
            .map(|count| count.as_u64().unwrap())
            .sum::<u64>();
        assert_eq!(partition_total, 3);
        assert_eq!(value["first_failure_partition"]["all_gates_passed"], 0);
        assert_eq!(value["protocol_identity_established"], false);

        assert!(
            classify_with_breakdown(
                &path,
                limits,
                &AtomicBool::new(false),
                Some(
                    breakdown::Selection::new(Family::NarrowbandConstantEnvelopeUnresolved, 2439,)
                        .unwrap(),
                ),
            )
            .is_err()
        );
    }

    #[test]
    fn feature_breakdown_records_all_passing_fsk_gates() {
        let directory = tempfile::tempdir().unwrap();
        let samples = binary_fsk(1024, 1.0, 0.0);
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2438, false, 1, &samples);
        let path = write_archive(directory.path(), "fsk.bdb", &bytes);
        let classification = classify_with_breakdown(
            &path,
            test_limits(bytes.len()),
            &AtomicBool::new(false),
            Some(breakdown::Selection::new(Family::NarrowbandBinaryFskLike, 2438).unwrap()),
        )
        .unwrap();
        let value = serde_json::to_value(classification.feature_breakdown.unwrap()).unwrap();
        assert_eq!(value["accounting"]["record_count"], 1);
        assert_eq!(value["accounting"]["two_level_fit_record_count"], 1);
        assert_eq!(value["first_failure_partition"]["all_gates_passed"], 1);
        assert_eq!(
            value["selected_record_features"]["two_level_separation_hz"]["observations"],
            1
        );
    }

    #[test]
    fn optional_feature_breakdown_is_private_and_leaves_ordinary_file_identical() {
        let directory = tempfile::tempdir().unwrap();
        let tone = oscillator(std::iter::repeat_n(100_000.0, 256), 12_000.0);
        let short = vec![(12_000, 0); 2];
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2438, false, 1, &tone);
        append_record(&mut bytes, 1, 0, 2438, false, 2, &tone);
        append_record(&mut bytes, 1, 0, 2438, false, 3, &short);
        let input = write_archive(directory.path(), "input.bdb", &bytes);
        let args = |output: &str, family| ClassifyBursts {
            input: input.clone(),
            output: Some(PathBuf::from(output)),
            feature_breakdown_family: family,
            feature_breakdown_frequency_mhz: family.map(|_| 2438),
            max_input_mib: 1,
            max_samples: 1_000_000,
            max_records: 100,
            max_groups: 100,
            max_output_mib: 1,
        };
        run(
            directory.path(),
            &args("ordinary", None),
            &AtomicBool::new(false),
        )
        .unwrap();
        run(
            directory.path(),
            &args(
                "diagnostic",
                Some(Family::NarrowbandConstantEnvelopeUnresolved),
            ),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            fs::read(directory.path().join("ordinary/burst-families.json")).unwrap(),
            fs::read(directory.path().join("diagnostic/burst-families.json")).unwrap()
        );
        assert!(
            !directory
                .path()
                .join("ordinary/feature-breakdown.json")
                .exists()
        );
        let feature_breakdown = directory.path().join("diagnostic/feature-breakdown.json");
        assert_eq!(
            fs::symlink_metadata(&feature_breakdown).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::symlink_metadata(directory.path().join("diagnostic"))
                .unwrap()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn empty_and_scan_only_archives_have_no_candidate_groups() {
        let directory = tempfile::tempdir().unwrap();
        let empty = file_header();
        let path = write_archive(directory.path(), "empty.bdb", &empty);
        let summary = classify(&path, test_limits(empty.len()), &AtomicBool::new(false)).unwrap();
        assert_eq!(summary.accounting.archive_record_count, 0);
        assert_eq!(summary.accounting.group_count, 0);

        let mut scan_only = file_header();
        append_record(&mut scan_only, 1, 0, 2437, true, 1, &vec![(0, 0); 256]);
        let path = write_archive(directory.path(), "scan-only.bdb", &scan_only);
        let summary =
            classify(&path, test_limits(scan_only.len()), &AtomicBool::new(false)).unwrap();
        assert_eq!(summary.accounting.archive_record_count, 1);
        assert_eq!(summary.accounting.scan_window_record_count, 1);
        assert_eq!(summary.accounting.squelch_record_count, 0);
        assert_eq!(summary.accounting.group_count, 0);
    }

    #[test]
    fn rejects_malformed_symlink_cancelled_and_bounded_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let tone = oscillator(std::iter::repeat_n(100_000.0, 256), 12_000.0);
        let mut bytes = file_header();
        append_record(&mut bytes, 1, 0, 2437, false, 1, &tone);
        append_record(&mut bytes, 2, 0, 2437, false, 2, &tone);
        let path = write_archive(directory.path(), "bounded.bdb", &bytes);

        let mut malformed = bytes.clone();
        malformed[0] ^= 0xff;
        let malformed_path = write_archive(directory.path(), "malformed.bdb", &malformed);
        assert!(
            classify(
                &malformed_path,
                test_limits(malformed.len()),
                &AtomicBool::new(false)
            )
            .is_err()
        );

        let link = directory.path().join("linked.bdb");
        symlink(&path, &link).unwrap();
        assert!(classify(&link, test_limits(bytes.len()), &AtomicBool::new(false)).is_err());
        assert!(classify(&path, test_limits(bytes.len()), &AtomicBool::new(true)).is_err());

        let mut limits = test_limits(bytes.len());
        limits.max_input_bytes -= 1;
        assert!(classify(&path, limits, &AtomicBool::new(false)).is_err());
        let mut limits = test_limits(bytes.len());
        limits.max_records = 1;
        assert!(classify(&path, limits, &AtomicBool::new(false)).is_err());
        let mut limits = test_limits(bytes.len());
        limits.max_samples = 511;
        assert!(classify(&path, limits, &AtomicBool::new(false)).is_err());
        let mut limits = test_limits(bytes.len());
        limits.max_groups = 1;
        assert!(classify(&path, limits, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn output_is_deterministic_bounded_private_and_path_free() {
        let directory = tempfile::tempdir().unwrap();
        let samples = binary_fsk(1024, 1.0, 0.0);
        let mut bytes = file_header();
        append_record(
            &mut bytes,
            8_765_432_109,
            987_654_321,
            2437,
            false,
            3_987_654_321,
            &samples,
        );
        let path = write_archive(directory.path(), "PRIVATE_PATH_SENTINEL.bdb", &bytes);
        let first = classify(&path, test_limits(bytes.len()), &AtomicBool::new(false)).unwrap();
        let second = classify(&path, test_limits(bytes.len()), &AtomicBool::new(false)).unwrap();
        let encoded = encode_summary(&first, 1024 * 1024).unwrap();
        assert_eq!(encoded, encode_summary(&second, 1024 * 1024).unwrap());
        let text = String::from_utf8(encoded.clone()).unwrap();
        assert!(!text.contains("PRIVATE_PATH_SENTINEL"));
        assert!(!text.contains("8765432109"));
        assert!(!text.contains("987654321"));
        assert!(!text.contains("3987654321"));
        let value = serde_json::to_value(&first).unwrap();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(
            value["settings"]["active_power_quantile"],
            serde_json::json!([4, 5])
        );
        assert_eq!(value["settings"]["active_power_divisor"], 16);
        assert_eq!(value["settings"]["mode_contrast_std_floor_hz"], 1.0);
        assert_eq!(value["settings"]["group_frequency_step_mhz"], 1);
        assert_eq!(value["settings"]["adjacent_multibin_minimum_width"], 2);
        assert_eq!(value["settings"]["wideband_multibin_minimum_width"], 4);
        assert_eq!(value["protocol_identity_established"], false);
        assert!(encode_summary(&first, encoded.len() as u64 - 1).is_err());

        let output = directory.path().join("burst-families.json");
        publish_summary(&output, &encoded, &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::symlink_metadata(&output).unwrap().mode() & 0o777, 0o600);
        assert!(publish_summary(&output, &encoded, &AtomicBool::new(false)).is_err());
        let cancelled = directory.path().join("cancelled.json");
        assert!(publish_summary(&cancelled, &encoded, &AtomicBool::new(true)).is_err());
        assert!(!cancelled.exists());
    }
}
