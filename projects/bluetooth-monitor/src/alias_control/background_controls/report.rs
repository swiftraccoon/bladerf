//! Path-free schema for fixed background-control results.

use serde::Serialize;

use super::super::matched_rate::{FilterSettings, filter_settings};
use super::super::provenance::ContentIdentity;
use super::super::report::MatchedRateReport;
use super::BackgroundControls;
use super::contract::FrequencyManifest;
use super::lattice::{LatticeAccounting, LatticeSettings};
use crate::burst_families::{FeatureSettings, feature_settings};

#[derive(Debug, Serialize)]
struct ManifestAttestedSidecar {
    bytes: u64,
    sha256: String,
    independently_verified: bool,
}

#[derive(Debug, Serialize)]
struct ManifestAttestedSource {
    format: String,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    bandwidth_hz: u64,
    available_complex_samples: u64,
    processed_complex_samples: u64,
    processed_full_input: bool,
    source_bytes: u64,
    processed_prefix_bytes: u64,
    processed_prefix_sha256: String,
    continuity_guaranteed: bool,
    independently_verified: bool,
}

#[derive(Debug, Serialize)]
struct FrequencyContract {
    expected_frequency_mhz: u32,
    target_center_frequency_hz: u64,
    source_offset_hz: i64,
    source_edge_margin_hz: u64,
    target_output_format: String,
    target_output_sample_rate_hz: u64,
    target_output_complex_samples: u64,
    channelizer_filter_taps: usize,
    channelizer_decimation: usize,
    channelizer_group_delay_source_samples: usize,
    channelizer_first_output_source_sample_index: usize,
    channelizer_startup_transient_output_samples: usize,
    channelizer_tail_flushed: bool,
    output_stride_source_samples: usize,
    latest_source_index_for_output_zero: usize,
    group_delay_corrected_source_index_for_output_zero: i64,
    continuity_guaranteed: bool,
}

#[derive(Debug, Serialize)]
struct Settings {
    feature_algorithm_version: u32,
    features: FeatureSettings,
    feature_activity_gate_enabled: bool,
    feature_activity_gate: &'static str,
    direct_frequency_reference: &'static str,
    matched_filter: FilterSettings,
}

#[derive(Debug, Serialize)]
struct MatchedBackgroundAnalysis {
    record_unit: &'static str,
    analysis: MatchedRateReport,
}

#[derive(Debug, Serialize)]
pub(super) struct Report {
    schema_version: u32,
    operation: &'static str,
    independently_verified_frequency_manifest: ContentIdentity,
    independently_verified_cf32_source: ContentIdentity,
    manifest_attested_acquisition_sidecar: ManifestAttestedSidecar,
    manifest_attested_source: ManifestAttestedSource,
    frequency_contract: FrequencyContract,
    lattice: LatticeSettings,
    lattice_accounting: LatticeAccounting,
    settings: Settings,
    bandwidth_matched_same_stream: MatchedBackgroundAnalysis,
    burst_trigger_used: bool,
    window_selection_uses_signal_metrics: bool,
    both_absolute_decimation_phases_evaluated: bool,
    candidate_promotion_permitted: bool,
    thresholds_tuned_on_background_windows: bool,
    aggregate_only: bool,
    channelizer_fir_startup_transient_excluded: bool,
    receiver_settling_validated: bool,
    continuous_wall_time_coverage_established: bool,
    statistical_independence_established: bool,
    signal_absence_established: bool,
    protocol_identity_established: bool,
    interpretation: &'static str,
}

pub(super) struct ReportInputs<'a> {
    pub(super) args: &'a BackgroundControls,
    pub(super) manifest: &'a FrequencyManifest,
    pub(super) manifest_identity: ContentIdentity,
    pub(super) cf32_identity: ContentIdentity,
    pub(super) lattice: LatticeSettings,
    pub(super) accounting: LatticeAccounting,
    pub(super) matched_rate: MatchedRateReport,
}

pub(super) fn build(inputs: ReportInputs<'_>) -> Report {
    let manifest = inputs.manifest;
    Report {
        schema_version: 1,
        operation: "bluetooth_background_controls",
        independently_verified_frequency_manifest: inputs.manifest_identity,
        independently_verified_cf32_source: inputs.cf32_identity,
        manifest_attested_acquisition_sidecar: ManifestAttestedSidecar {
            bytes: manifest.acquisition_sidecar.bytes,
            sha256: manifest.acquisition_sidecar.sha256.clone(),
            independently_verified: false,
        },
        manifest_attested_source: ManifestAttestedSource {
            format: manifest.source.format.clone(),
            center_frequency_hz: manifest.source.center_frequency_hz,
            sample_rate_hz: manifest.source.sample_rate_hz,
            bandwidth_hz: manifest.source.bandwidth_hz,
            available_complex_samples: manifest.source.available_complex_samples,
            processed_complex_samples: manifest.source.processed_complex_samples,
            processed_full_input: manifest.source.processed_full_input,
            source_bytes: manifest.source.source_bytes,
            processed_prefix_bytes: manifest.source.processed_prefix_bytes,
            processed_prefix_sha256: manifest.source.processed_prefix_sha256.clone(),
            continuity_guaranteed: manifest.source.continuity_guaranteed,
            independently_verified: false,
        },
        frequency_contract: FrequencyContract {
            expected_frequency_mhz: inputs.args.frequency_mhz,
            target_center_frequency_hz: manifest.target.center_frequency_hz,
            source_offset_hz: manifest.target.source_offset_hz,
            source_edge_margin_hz: manifest.target.source_edge_margin_hz,
            target_output_format: manifest.target.output_format.clone(),
            target_output_sample_rate_hz: manifest.target.output_sample_rate_hz,
            target_output_complex_samples: manifest.target.output_complex_samples,
            channelizer_filter_taps: manifest.filter.taps,
            channelizer_decimation: manifest.filter.decimation,
            channelizer_group_delay_source_samples: manifest.filter.group_delay_source_samples,
            channelizer_first_output_source_sample_index: manifest
                .filter
                .first_output_source_sample_index,
            channelizer_startup_transient_output_samples: manifest
                .filter
                .startup_transient_output_samples,
            channelizer_tail_flushed: manifest.filter.tail_flushed,
            output_stride_source_samples: manifest
                .sample_index_mapping
                .output_stride_source_samples,
            latest_source_index_for_output_zero: manifest
                .sample_index_mapping
                .latest_source_index_for_output_zero,
            group_delay_corrected_source_index_for_output_zero: manifest
                .sample_index_mapping
                .group_delay_corrected_source_index_for_output_zero,
            continuity_guaranteed: manifest.continuity_guaranteed,
        },
        lattice: inputs.lattice,
        lattice_accounting: inputs.accounting,
        settings: Settings {
            feature_algorithm_version: 1,
            features: feature_settings(),
            feature_activity_gate_enabled: true,
            feature_activity_gate: "squared_complex_power_at_or_above_p80_divided_by_16",
            direct_frequency_reference: "signed_baseband_hz_relative_to_target_frequency",
            matched_filter: filter_settings(),
        },
        bandwidth_matched_same_stream: MatchedBackgroundAnalysis {
            record_unit: "fixed_background_window",
            analysis: inputs.matched_rate,
        },
        burst_trigger_used: false,
        window_selection_uses_signal_metrics: false,
        both_absolute_decimation_phases_evaluated: true,
        candidate_promotion_permitted: false,
        thresholds_tuned_on_background_windows: false,
        aggregate_only: true,
        channelizer_fir_startup_transient_excluded: true,
        receiver_settling_validated: false,
        continuous_wall_time_coverage_established: false,
        statistical_independence_established: false,
        signal_absence_established: false,
        protocol_identity_established: false,
        interpretation: "The fixed sample-index lattice is independent of burst triggers and window signal metrics. It excludes the channelizer FIR startup transient, but does not validate receiver or RF settling. The 48 disjoint 512-sample windows come from one capture, are not duration-matched to candidate windows and analyze 24,576 of 4,000,000 outputs (0.6144%); statistical independence, sample continuity and continuous wall-time coverage are not established. One declared matched filter precedes the 4 MSps view and both absolute 2 MSps phases, with no favorable-phase selection. The p80/16 activity gate and all other waveform thresholds remain diagnostic only and were not tuned on these windows. Results cannot establish signal absence, promote candidates or identify a protocol, packet, transmitter or occupancy.",
    }
}
