//! Exact provenance contract for fixed background controls.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::super::CONTROL_SAMPLE_RATE_HZ;
use super::super::provenance::valid_sha256;
use super::lattice::{ContextWindow, WINDOW_COUNT, context_window, window_start};
use super::{BackgroundControls, Limits};

pub(super) const SOURCE_SAMPLE_RATE_HZ: u64 = 20_000_000;
pub(super) const SOURCE_COMPLEX_SAMPLES: u64 = 20_000_000;
pub(super) const SOURCE_BYTES: u64 = SOURCE_COMPLEX_SAMPLES * 4;
pub(super) const OUTPUT_COMPLEX_SAMPLES: u64 = 4_000_000;
pub(super) const OUTPUT_BYTES: u64 = OUTPUT_COMPLEX_SAMPLES * 8;
const FREQUENCY_FILTER_STOPBAND_HZ: u64 = 2_000_000;

#[derive(Debug, Deserialize)]
pub(super) struct ManifestSidecar {
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestSource {
    pub(super) format: String,
    pub(super) center_frequency_hz: u64,
    pub(super) sample_rate_hz: u64,
    pub(super) bandwidth_hz: u64,
    pub(super) available_complex_samples: u64,
    pub(super) processed_complex_samples: u64,
    pub(super) processed_full_input: bool,
    pub(super) source_bytes: u64,
    pub(super) processed_prefix_bytes: u64,
    pub(super) processed_prefix_sha256: String,
    pub(super) continuity_guaranteed: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestFilter {
    pub(super) taps: usize,
    pub(super) decimation: usize,
    pub(super) group_delay_source_samples: usize,
    pub(super) first_output_source_sample_index: usize,
    pub(super) startup_transient_output_samples: usize,
    pub(super) tail_flushed: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestMapping {
    pub(super) output_stride_source_samples: usize,
    pub(super) latest_source_index_for_output_zero: usize,
    pub(super) group_delay_corrected_source_index_for_output_zero: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct ManifestTarget {
    pub(super) center_frequency_hz: u64,
    pub(super) source_offset_hz: i64,
    pub(super) source_edge_margin_hz: u64,
    pub(super) output_file: String,
    pub(super) output_format: String,
    pub(super) output_sample_rate_hz: u64,
    pub(super) output_complex_samples: u64,
    pub(super) sha256: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct FrequencyManifest {
    pub(super) schema_version: u32,
    pub(super) operation: String,
    pub(super) acquisition_sidecar: ManifestSidecar,
    pub(super) source: ManifestSource,
    pub(super) filter: ManifestFilter,
    pub(super) sample_index_mapping: ManifestMapping,
    pub(super) target: ManifestTarget,
    pub(super) continuity_guaranteed: bool,
}

fn source_offset_hz(manifest: &FrequencyManifest) -> Result<i64> {
    i64::try_from(manifest.target.center_frequency_hz)?
        .checked_sub(i64::try_from(manifest.source.center_frequency_hz)?)
        .context("frequency source offset overflow")
}

pub(super) fn validate_manifest(
    args: &BackgroundControls,
    limits: Limits,
    manifest: &FrequencyManifest,
) -> Result<()> {
    ensure!(
        manifest.schema_version == 1 && manifest.operation == "offline_frequency_channelization",
        "unsupported frequency channelizer manifest"
    );
    ensure!(
        manifest.source.format == "SC16_Q11_LE"
            && manifest.source.sample_rate_hz == SOURCE_SAMPLE_RATE_HZ
            && manifest.source.bandwidth_hz == SOURCE_SAMPLE_RATE_HZ,
        "frequency source contract differs from 20 MSps SC16_Q11"
    );
    ensure!(
        manifest.source.available_complex_samples == SOURCE_COMPLEX_SAMPLES
            && manifest.source.processed_complex_samples == SOURCE_COMPLEX_SAMPLES
            && manifest.source.processed_full_input
            && manifest.source.source_bytes == SOURCE_BYTES
            && manifest.source.processed_prefix_bytes == SOURCE_BYTES,
        "background controls require the complete 20,000,000-sample source extent"
    );
    ensure!(
        !manifest.source.continuity_guaranteed && !manifest.continuity_guaranteed,
        "frequency manifest must not claim source continuity"
    );
    ensure!(
        manifest.acquisition_sidecar.bytes > 0
            && valid_sha256(&manifest.acquisition_sidecar.sha256)
            && valid_sha256(&manifest.source.processed_prefix_sha256)
            && valid_sha256(&manifest.target.sha256),
        "frequency manifest identity is malformed"
    );
    ensure!(
        manifest.filter.taps == 225
            && manifest.filter.decimation == 5
            && manifest.filter.group_delay_source_samples == 112
            && manifest.filter.first_output_source_sample_index == 4
            && manifest.filter.startup_transient_output_samples == 44
            && !manifest.filter.tail_flushed,
        "frequency filter contract differs from the qualified path"
    );
    ensure!(
        manifest.sample_index_mapping.output_stride_source_samples == 5
            && manifest
                .sample_index_mapping
                .latest_source_index_for_output_zero
                == 4
            && manifest
                .sample_index_mapping
                .group_delay_corrected_source_index_for_output_zero
                == -108,
        "frequency sample-index mapping differs from the qualified path"
    );
    ensure!(
        manifest.target.center_frequency_hz == u64::from(args.frequency_mhz) * 1_000_000,
        "frequency target differs from --frequency-mhz"
    );
    let offset_hz = source_offset_hz(manifest)?;
    ensure!(
        manifest.target.source_offset_hz == offset_hz,
        "frequency target offset differs from the source centers"
    );
    let occupied_edge_hz = offset_hz
        .unsigned_abs()
        .checked_add(FREQUENCY_FILTER_STOPBAND_HZ)
        .context("frequency occupied edge overflow")?;
    let half_source_rate_hz = SOURCE_SAMPLE_RATE_HZ / 2;
    ensure!(
        occupied_edge_hz < half_source_rate_hz
            && manifest.target.source_edge_margin_hz == half_source_rate_hz - occupied_edge_hz,
        "frequency target edge margin differs from the qualified path"
    );
    ensure!(
        manifest.target.output_format == "CF32_LE"
            && manifest.target.output_sample_rate_hz == u64::from(CONTROL_SAMPLE_RATE_HZ)
            && manifest.target.output_complex_samples == OUTPUT_COMPLEX_SAMPLES,
        "frequency target differs from the exact 4 MSps contract"
    );
    ensure!(
        args.cf32_input.file_name().and_then(|name| name.to_str())
            == Some(manifest.target.output_file.as_str()),
        "CF32 filename differs from the manifest"
    );
    ensure!(
        OUTPUT_BYTES <= limits.cf32_input_bytes,
        "manifest CF32 extent exceeds the configured limit"
    );
    let last_start = window_start(WINDOW_COUNT - 1)?;
    ensure!(
        matches!(
            context_window(
                last_start,
                manifest.filter.startup_transient_output_samples as u64,
                manifest.target.output_complex_samples,
            )?,
            ContextWindow::Available { .. }
        ),
        "background lattice exceeds the authenticated CF32 extent"
    );
    Ok(())
}
