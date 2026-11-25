//! Bounded comparison of classified 2 MSps bursts with a retained 4 MSps stream.

mod background_controls;
mod matched_rate;
mod provenance;
mod report;

pub(crate) use self::background_controls::BackgroundControls;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use clap::Args;

use crate::burst_families::{
    ComplexSample, Family, complex_control_features, complex_direct_feature,
    complex_record_feature, feature_settings, select_family_frequency,
};
use crate::bursts::{CHANNEL_SAMPLE_RATE_HZ, ParseLimits};

use self::provenance::{
    ContentIdentity, FrequencyManifest, ReplayManifest, read_cf32, read_stable_json, valid_sha256,
};
use self::report::{
    BranchControlAccumulator, FourRateAccumulator, HYPOTHESIS_RELATIVE_TOLERANCE,
    ManifestAttestedSidecarReport, ManifestAttestedSourcePrefixReport, MappingReport,
    MatchedRateAccumulator, ReplayAttestedDecoderProgramReport, Report, SelectionReport,
    SettingsReport,
};

const SOURCE_SAMPLE_RATE_HZ: u64 = 20_000_000;
const CONTROL_SAMPLE_RATE_HZ: u32 = 4_000_000;
const DEFAULT_MAX_BURST_INPUT_MIB: u32 = 128;
const DEFAULT_MAX_CF32_INPUT_MIB: u32 = 64;
const DEFAULT_MAX_BURST_SAMPLES: u64 = 10_000_000;
const DEFAULT_MAX_RECORDS: u64 = 20_000;
const DEFAULT_MAX_GROUPS: u64 = 20_000;
const DEFAULT_MAX_OUTPUT_MIB: u32 = 16;
const ROTATION_FREQUENCIES_HZ: [f64; 8] = [
    0.0,
    250_000.0,
    500_000.0,
    750_000.0,
    1_000_000.0,
    1_250_000.0,
    1_500_000.0,
    1_750_000.0,
];
const BLUE_DRAGON_ALIGNMENT_REVISION: &str = "7bc9ec6b9228852b752f7309e46ecb186b87b2eb";
// Provisional pinned-source contract: Blue Dragon's first stored IQ center is modeled as
// R - 61 after the Rise sample, while DDC output k has delay-corrected center 5k - 108.
// Equating the centers gives k = ceil((R + 47) / 5) = R / 5 + 10 on the required grid.
// The report keeps this mapping explicitly unvalidated pending a transition/correlation sweep.
const BDB_TO_CF32_OUTPUT_OFFSET: u64 = 10;

pub(crate) fn run_background_controls(
    root: &Path,
    args: &BackgroundControls,
    interrupted: &AtomicBool,
) -> Result<()> {
    background_controls::run(root, args, interrupted)
}

#[derive(Debug, Args)]
pub(crate) struct AliasControl {
    /// Existing Blue Dragon BDB v1 archive.
    burst_input: PathBuf,
    /// Existing 4 MSps CF32 stream from `channelize-frequency`.
    cf32_input: PathBuf,
    /// Matching `channelize-frequency.json` provenance manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Matching schema-2 replay terminal manifest.
    #[arg(long)]
    replay_manifest: PathBuf,
    /// Closed candidate family whose exact row supplies records.
    #[arg(long, value_enum)]
    family: Family,
    /// Exact 1 MHz BDB row and 4 MSps stream center.
    #[arg(long, value_parser = clap::value_parser!(u32).range(2402..=2480))]
    frequency_mhz: u32,
    /// New output directory; defaults under artifacts/bluetooth-alias-control/.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Maximum accepted BDB size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_BURST_INPUT_MIB)]
    max_burst_input_mib: u32,
    /// Maximum accepted CF32 size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_CF32_INPUT_MIB)]
    max_cf32_input_mib: u32,
    /// Maximum total BDB complex samples.
    #[arg(long, default_value_t = DEFAULT_MAX_BURST_SAMPLES)]
    max_burst_samples: u64,
    /// Maximum BDB records.
    #[arg(long, default_value_t = DEFAULT_MAX_RECORDS)]
    max_records: u64,
    /// Maximum temporal BDB groups.
    #[arg(long, default_value_t = DEFAULT_MAX_GROUPS)]
    max_groups: u64,
    /// Maximum JSON output size in MiB.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_MIB)]
    max_output_mib: u32,
}

#[derive(Clone, Copy, Debug)]
struct Limits {
    burst_input_bytes: u64,
    cf32_input_bytes: u64,
    burst_samples: u64,
    records: u64,
    groups: u64,
    output_bytes: u64,
}

impl AliasControl {
    fn limits(&self) -> Result<Limits> {
        ensure!(
            (1..=128).contains(&self.max_burst_input_mib),
            "BDB limit must be 1..=128 MiB"
        );
        ensure!(
            (1..=64).contains(&self.max_cf32_input_mib),
            "CF32 limit must be 1..=64 MiB"
        );
        ensure!(
            (1..=10_000_000).contains(&self.max_burst_samples),
            "BDB sample limit must be 1..=10000000"
        );
        ensure!(
            (1..=20_000).contains(&self.max_records),
            "record limit must be 1..=20000"
        );
        ensure!(
            (1..=20_000).contains(&self.max_groups),
            "group limit must be 1..=20000"
        );
        ensure!(
            (1..=256).contains(&self.max_output_mib),
            "output limit must be 1..=256 MiB"
        );
        Ok(Limits {
            burst_input_bytes: u64::from(self.max_burst_input_mib) * 1024 * 1024,
            cf32_input_bytes: u64::from(self.max_cf32_input_mib) * 1024 * 1024,
            burst_samples: self.max_burst_samples,
            records: self.max_records,
            groups: self.max_groups,
            output_bytes: u64::from(self.max_output_mib) * 1024 * 1024,
        })
    }
}

fn packed_complex(bytes: &[u8]) -> Vec<ComplexSample> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| ComplexSample {
            re: f32::from(i16::from_le_bytes([sample[0], sample[1]])),
            im: f32::from(i16::from_le_bytes([sample[2], sample[3]])),
        })
        .collect()
}

fn rotate(samples: &[ComplexSample], frequency_hz: f64, conjugate: bool) -> Vec<ComplexSample> {
    samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let phase = std::f64::consts::TAU * frequency_hz * index as f64
                / f64::from(CHANNEL_SAMPLE_RATE_HZ);
            let (sin, cos) = phase.sin_cos();
            let im = if conjugate { -sample.im } else { sample.im };
            ComplexSample {
                re: (f64::from(sample.re) * cos - f64::from(im) * sin) as f32,
                im: (f64::from(sample.re) * sin + f64::from(im) * cos) as f32,
            }
        })
        .collect()
}

fn cf32_slice(bytes: &[u8], start: u64, count: u64) -> Result<Vec<ComplexSample>> {
    let byte_start = usize::try_from(start.checked_mul(8).context("CF32 slice start overflow")?)?;
    let byte_end = usize::try_from(
        start
            .checked_add(count)
            .and_then(|end| end.checked_mul(8))
            .context("CF32 slice end overflow")?,
    )?;
    ensure!(
        byte_end <= bytes.len(),
        "selected CF32 window exceeds the retained stream"
    );
    Ok(bytes[byte_start..byte_end]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|sample| ComplexSample {
            re: f32::from_le_bytes(sample[..4].try_into().expect("fixed CF32 sample")),
            im: f32::from_le_bytes(sample[4..].try_into().expect("fixed CF32 sample")),
        })
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchedContextWindow {
    StartUnavailable,
    TailUnavailable,
    Available { start: u64, count: u64 },
}

/// Classifies the context extent for a base-valid 4 MSps control window.
fn matched_context_window(
    control_start: u64,
    control_count: u64,
    first_valid_output: u64,
    output_extent: u64,
) -> Result<MatchedContextWindow> {
    ensure!(control_count > 0, "matched-rate control window is empty");
    let filter_half_len = matched_rate::FILTER_HALF_LEN as u64;
    let earliest_context_backed_start = first_valid_output
        .checked_add(filter_half_len)
        .context("matched-rate valid start overflow")?;
    if control_start < earliest_context_backed_start {
        return Ok(MatchedContextWindow::StartUnavailable);
    }
    let control_end = control_start
        .checked_add(control_count)
        .context("matched-rate control end overflow")?;
    ensure!(
        control_end <= output_extent,
        "matched-rate control window exceeds the base-valid extent"
    );
    let context_end = control_end
        .checked_add(filter_half_len)
        .context("matched-rate context end overflow")?;
    if context_end > output_extent {
        return Ok(MatchedContextWindow::TailUnavailable);
    }
    let context_start = control_start
        .checked_sub(filter_half_len)
        .context("matched-rate context start underflow")?;
    let context_padding = filter_half_len
        .checked_mul(2)
        .context("matched-rate context padding overflow")?;
    let context_count = control_count
        .checked_add(context_padding)
        .context("matched-rate context length overflow")?;
    Ok(MatchedContextWindow::Available {
        start: context_start,
        count: context_count,
    })
}

fn raw_timestamp_index(seconds: u64, nanoseconds: u32) -> Result<u64> {
    ensure!(
        nanoseconds.is_multiple_of(50),
        "BDB timestamp is not on the 20 MSps source grid"
    );
    let within_second = u64::from(nanoseconds) / 50;
    let index = seconds
        .checked_mul(SOURCE_SAMPLE_RATE_HZ)
        .and_then(|base| base.checked_add(within_second))
        .context("BDB source index overflow")?;
    ensure!(
        index.is_multiple_of(10),
        "BDB timestamp is not on the 2 MSps channelizer grid"
    );
    Ok(index)
}

fn publish_report(path: &Path, encoded: &[u8]) -> Result<()> {
    let partial = path.with_extension("json.partial");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    let mut published = false;
    let result = (|| -> Result<()> {
        file.write_all(encoded)?;
        file.sync_all()?;
        fs::hard_link(&partial, path)
            .with_context(|| format!("publish {} without overwriting", path.display()))?;
        published = true;
        fs::remove_file(&partial)?;
        File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        if published {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_file(&partial);
    }
    result
}

pub(crate) fn run(root: &Path, args: &AliasControl, interrupted: &AtomicBool) -> Result<()> {
    let limits = args.limits()?;
    let (manifest, frequency_manifest_identity) = read_stable_json::<FrequencyManifest>(
        &args.manifest,
        "frequency channelizer manifest",
        "frequency_channelizer_manifest_v1",
    )?;
    let (replay, replay_manifest_identity) = read_stable_json::<ReplayManifest>(
        &args.replay_manifest,
        "Bluetooth replay manifest",
        "bluetooth_replay_manifest_v2",
    )?;
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
        manifest.source.processed_prefix_bytes
            == manifest
                .source
                .processed_complex_samples
                .checked_mul(4)
                .context("source prefix byte count overflow")?,
        "source prefix byte count is inconsistent"
    );
    ensure!(
        valid_sha256(&manifest.source.processed_prefix_sha256)
            && valid_sha256(&manifest.acquisition_sidecar.sha256)
            && valid_sha256(&manifest.target.sha256),
        "source prefix digest is malformed"
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
        !manifest.continuity_guaranteed,
        "unexpected continuity claim in frequency manifest"
    );
    ensure!(
        manifest.target.center_frequency_hz == u64::from(args.frequency_mhz) * 1_000_000
            && manifest.target.output_format == "CF32_LE"
            && manifest.target.output_sample_rate_hz == u64::from(CONTROL_SAMPLE_RATE_HZ),
        "frequency target differs from the selected row"
    );
    ensure!(
        manifest.target.output_complex_samples == manifest.source.processed_complex_samples / 5,
        "frequency output count differs from its source extent"
    );
    ensure!(
        manifest
            .target
            .output_complex_samples
            .checked_mul(8)
            .context("CF32 byte count overflow")?
            <= limits.cf32_input_bytes,
        "manifest CF32 extent exceeds the configured limit"
    );
    ensure!(
        args.cf32_input.file_name().and_then(|name| name.to_str())
            == Some(manifest.target.output_file.as_str()),
        "CF32 filename differs from the manifest"
    );
    let cf32 = read_cf32(
        &args.cf32_input,
        limits.cf32_input_bytes,
        &manifest.target.sha256,
    )?;
    ensure!(
        cf32.len() as u64 == manifest.target.output_complex_samples * 8,
        "CF32 length differs from the manifest"
    );

    let selected = select_family_frequency(
        &args.burst_input,
        ParseLimits {
            max_input_bytes: limits.burst_input_bytes,
            max_records: limits.records,
            max_total_samples: limits.burst_samples,
        },
        limits.groups,
        args.family,
        args.frequency_mhz,
        interrupted,
    )?;
    ensure!(
        replay.schema_version == 2
            && replay.processed_full_input
            && replay.decoder_processed_complex_samples
                == Some(manifest.source.processed_complex_samples)
            && replay.input_complex_samples == manifest.source.processed_complex_samples
            && replay.channelizer_step_complex_samples == 10
            && replay
                .channelizer_steps_processed
                .checked_mul(replay.channelizer_step_complex_samples)
                == Some(replay.input_complex_samples)
            && replay.burst_archive_records == selected.archive_record_count
            && !replay.burst_archive_truncated_by_limit
            && !replay.radio_opened
            && !replay.network_requested
            && !replay.transmit_requested,
        "Bluetooth replay manifest is incompatible with a complete host-only derivation"
    );
    ensure!(
        replay.decoder_program.schema_version == 1
            && replay.decoder_program.name == "blue-dragon"
            && replay.decoder_program.bytes > 0
            && valid_sha256(&replay.decoder_program.sha256),
        "Bluetooth replay decoder identity is malformed"
    );
    let mut branch = BranchControlAccumulator::default();
    let mut four_rate = FourRateAccumulator::default();
    let mut matched_rate = MatchedRateAccumulator::default();
    let mut selected_samples = 0u64;
    let mut valid_control_windows = 0u64;
    let mut startup_transient_windows = 0u64;
    let mut nonflushed_tail_windows = 0u64;
    for record in &selected.records {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "alias control interrupted"
        );
        selected_samples = selected_samples
            .checked_add(u64::from(record.metadata.sample_count))
            .context("selected sample count overflow")?;
        let bdb_samples = packed_complex(&record.packed_iq);
        let baseline = complex_record_feature(&bdb_samples, CHANNEL_SAMPLE_RATE_HZ, interrupted)?;
        for frequency_hz in ROTATION_FREQUENCIES_HZ {
            let rotated = rotate(&bdb_samples, frequency_hz, false);
            branch.compare(
                baseline,
                complex_record_feature(&rotated, CHANNEL_SAMPLE_RATE_HZ, interrupted)?,
            );
        }
        let conjugated = rotate(&bdb_samples, 0.0, true);
        branch.compare(
            baseline,
            complex_record_feature(&conjugated, CHANNEL_SAMPLE_RATE_HZ, interrupted)?,
        );

        let raw_start = raw_timestamp_index(
            record.metadata.timestamp_seconds,
            record.metadata.timestamp_nanoseconds,
        )?;
        let control_start = raw_start / 5 + BDB_TO_CF32_OUTPUT_OFFSET;
        let control_count = u64::from(record.metadata.sample_count)
            .checked_mul(2)
            .context("4 MSps window length overflow")?;
        if control_start < manifest.filter.startup_transient_output_samples as u64 {
            startup_transient_windows += 1;
            continue;
        }
        let control_end = control_start
            .checked_add(control_count)
            .context("4 MSps window end overflow")?;
        if control_end > manifest.target.output_complex_samples {
            nonflushed_tail_windows += 1;
            continue;
        }
        valid_control_windows += 1;
        let control = cf32_slice(&cf32, control_start, control_count)?;
        let controls = complex_control_features(&control, CONTROL_SAMPLE_RATE_HZ, interrupted)?;
        four_rate.observe(baseline, controls.circular, controls.direct);

        let (context_start, context_count) = match matched_context_window(
            control_start,
            control_count,
            manifest.filter.startup_transient_output_samples as u64,
            manifest.target.output_complex_samples,
        )? {
            MatchedContextWindow::StartUnavailable => {
                matched_rate.observe_start_context_unavailable();
                continue;
            }
            MatchedContextWindow::TailUnavailable => {
                matched_rate.observe_tail_context_unavailable();
                continue;
            }
            MatchedContextWindow::Available { start, count } => (start, count),
        };
        let context = cf32_slice(&cf32, context_start, context_count)?;
        let views = matched_rate::matched_views(
            &context,
            control_start,
            usize::try_from(control_count).context("matched-rate output length exceeds usize")?,
            interrupted,
        )?;
        matched_rate.observe(
            complex_direct_feature(&views.four_msps, CONTROL_SAMPLE_RATE_HZ, interrupted)?,
            complex_direct_feature(
                &views.two_msps_global_even,
                CONTROL_SAMPLE_RATE_HZ / 2,
                interrupted,
            )?,
            complex_direct_feature(
                &views.two_msps_global_odd,
                CONTROL_SAMPLE_RATE_HZ / 2,
                interrupted,
            )?,
        );
    }
    ensure!(
        valid_control_windows
            .checked_add(startup_transient_windows)
            .and_then(|count| count.checked_add(nonflushed_tail_windows))
            == Some(selected.records.len() as u64),
        "internal 4 MSps control-window accounting mismatch"
    );

    let branch = branch.finish();
    let report = Report {
        schema_version: 3,
        operation: "bluetooth_burst_alias_control",
        burst_source: ContentIdentity {
            format: "blue_dragon_burst_v1",
            bytes: selected.source_bytes,
            sha256: selected.sha256,
        },
        cf32_source: ContentIdentity {
            format: "CF32_LE",
            bytes: cf32.len() as u64,
            sha256: manifest.target.sha256,
        },
        frequency_manifest: frequency_manifest_identity,
        replay_manifest: replay_manifest_identity,
        replay_attested_decoder_program: ReplayAttestedDecoderProgramReport::from(
            replay.decoder_program,
        ),
        manifest_attested_acquisition_sidecar: ManifestAttestedSidecarReport {
            bytes: manifest.acquisition_sidecar.bytes,
            sha256: manifest.acquisition_sidecar.sha256,
            independently_verified: false,
        },
        manifest_attested_source_prefix: ManifestAttestedSourcePrefixReport {
            format: manifest.source.format,
            bytes: manifest.source.processed_prefix_bytes,
            sha256: manifest.source.processed_prefix_sha256,
            processed_complex_samples: manifest.source.processed_complex_samples,
            independently_verified: false,
        },
        selection: SelectionReport {
            family: args.family.label(),
            frequency_mhz: args.frequency_mhz,
            group_count: selected.selected_group_count,
            record_count: selected.records.len() as u64,
            complex_sample_count_2msps: selected_samples,
        },
        mapping: MappingReport {
            blue_dragon_revision: BLUE_DRAGON_ALIGNMENT_REVISION,
            blue_dragon_revision_verified_from_replay: false,
            alignment_validation_kind: "source_revision_contract_without_pfb_burst_catcher_integration_fixture",
            exact_timestamp_grid_validated: true,
            bdb_to_cf32_output_offset: BDB_TO_CF32_OUTPUT_OFFSET,
            cf32_samples_per_bdb_sample: 2,
            frequency_channelizer_stride_source_samples: manifest
                .sample_index_mapping
                .output_stride_source_samples,
            frequency_channelizer_corrected_origin_source_samples: manifest
                .sample_index_mapping
                .group_delay_corrected_source_index_for_output_zero,
            selected_windows: selected.records.len() as u64,
            valid_control_windows,
            startup_transient_windows,
            nonflushed_tail_windows,
            all_selected_windows_within_valid_extent: startup_transient_windows == 0
                && nonflushed_tail_windows == 0,
            edr_preroll_exclusion_validated: false,
            pfb_burst_catcher_integration_fixture_executed: false,
        },
        settings: SettingsReport {
            source_sample_rate_hz: SOURCE_SAMPLE_RATE_HZ,
            bdb_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
            control_sample_rate_hz: CONTROL_SAMPLE_RATE_HZ,
            rotation_frequencies_hz: ROTATION_FREQUENCIES_HZ,
            conjugation_control: true,
            direct_frequency_reference: "signed_baseband_hz_relative_to_selection_frequency",
            hypothesis_relative_tolerance: HYPOTHESIS_RELATIVE_TOLERANCE,
            feature_algorithm_version: 1,
            features: feature_settings(),
        },
        branch_control_2msps: branch,
        alias_control_4msps: four_rate
            .finish(selected.records.len() as u64, valid_control_windows)?,
        bandwidth_matched_same_stream: matched_rate.finish(
            selected.records.len() as u64,
            valid_control_windows,
            matched_rate::filter_settings(),
        )?,
        evidence_limited: true,
        protocol_identity_established: false,
        interpretation: "The 2 MSps transformations test circular branch stability only. Selected windows intersecting the independent FIR startup transient or non-flushed tail are explicitly unavailable, not analyzed as valid controls. The declared 4 MSps first-failure partitions are closed diagnostic orderings; overlapping gate-failure counts are non-exclusive and neither is a causal model. Direct centers are unrecentered signed baseband frequencies relative to the selected row, and direct phase concentration is reported only as a diagnostic. A branch-spanning alias candidate requires a qualified direct separation above 2 MHz plus a qualified circular fold matching 4 MHz minus that separation; it is still not established evidence while BDB-to-CF32 alignment remains unvalidated. Other raw or qualified fits remain diagnostic. Paired 2-versus-4 MSps hypothesis matches are non-exclusive, filter-sensitive and inconclusive because the channelizers have different passbands; a mismatch is not classified as aliasing or artifact. The same-stream branch applies one declared filter before comparing 4 MSps output with both absolute 2 MSps decimation phases; it reduces that filter confound but is not a bit-exact Blue Dragon PFB comparison, mapping proof, decoder result or protocol identity. The action independently verifies BDB and CF32 bytes. Decoder identity is replay-attested, while acquisition-sidecar and raw-prefix identities are channelizer-manifest-attested; this action does not rehash those three sources. The replay artifacts do not cryptographically bind BDB bytes to the raw source. The provisional BDB-to-CF32 offset has no authenticated PFB/BurstCatcher fixture and cannot exclude unencoded EDR pre-roll. Results are evidence-limited waveform diagnostics.",
    };
    let mut encoded = serde_json::to_vec_pretty(&report)?;
    encoded.push(b'\n');
    ensure!(
        encoded.len() as u64 <= limits.output_bytes,
        "alias-control report exceeds its output limit"
    );
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "alias control interrupted before output creation"
    );
    let output = root.join(crate::output_path(
        args.output.as_deref(),
        "artifacts/bluetooth-alias-control",
    ));
    crate::new_directory(&output)?;
    publish_report(&output.join("alias-control.json"), &encoded)?;
    crate::print_summary(&report, "Alias-control directory", &output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    fn bdb_record(
        frequency_mhz: u32,
        timestamp_nanoseconds: u32,
        samples: &[(i16, i16)],
    ) -> Vec<u8> {
        let mut bytes = Vec::from(*b"BDBURST\0");
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(&CHANNEL_SAMPLE_RATE_HZ.to_le_bytes());
        bytes.extend_from_slice(b"BRST");
        bytes.extend_from_slice(&(48u32 + samples.len() as u32 * 4).to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&timestamp_nanoseconds.to_le_bytes());
        bytes.extend_from_slice(&frequency_mhz.to_le_bytes());
        bytes.extend_from_slice(&(-20.0f32).to_le_bytes());
        bytes.extend_from_slice(&(-60.0f32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&2048.0f32.to_le_bytes());
        for &(i, q) in samples {
            bytes.extend_from_slice(&i.to_le_bytes());
            bytes.extend_from_slice(&q.to_le_bytes());
        }
        bytes
    }

    fn oscillator(
        sample_rate_hz: u32,
        frequencies_hz: impl IntoIterator<Item = f64>,
    ) -> Vec<ComplexSample> {
        let mut phase = 0.0f64;
        frequencies_hz
            .into_iter()
            .map(|frequency| {
                phase += std::f64::consts::TAU * frequency / f64::from(sample_rate_hz);
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect()
    }

    fn q11_tone(sample_count: usize, frequency_hz: f64) -> Vec<(i16, i16)> {
        oscillator(
            CHANNEL_SAMPLE_RATE_HZ,
            std::iter::repeat_n(frequency_hz, sample_count),
        )
        .into_iter()
        .map(|sample| {
            (
                (sample.re * 1500.0).round() as i16,
                (sample.im * 1500.0).round() as i16,
            )
        })
        .collect()
    }

    fn cf32_bytes(samples: &[ComplexSample]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * 8);
        for sample in samples {
            bytes.extend_from_slice(&sample.re.to_le_bytes());
            bytes.extend_from_slice(&sample.im.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn timestamp_mapping_is_exact_and_bounded() {
        assert_eq!(raw_timestamp_index(0, 500).unwrap(), 10);
        assert_eq!(raw_timestamp_index(1, 0).unwrap(), SOURCE_SAMPLE_RATE_HZ);
        assert!(raw_timestamp_index(0, 51).is_err());
    }

    #[test]
    fn matched_context_boundaries_are_exact_and_base_bounded() {
        assert_eq!(
            matched_context_window(59, 1_024, 44, 2_000).unwrap(),
            MatchedContextWindow::StartUnavailable
        );
        assert_eq!(
            matched_context_window(60, 1_024, 44, 2_000).unwrap(),
            MatchedContextWindow::Available {
                start: 44,
                count: 1_056
            }
        );
        assert_eq!(
            matched_context_window(960, 1_024, 44, 2_000).unwrap(),
            MatchedContextWindow::Available {
                start: 944,
                count: 1_056
            }
        );
        assert_eq!(
            matched_context_window(961, 1_024, 44, 2_000).unwrap(),
            MatchedContextWindow::TailUnavailable
        );
        assert!(matched_context_window(1_000, 1_024, 44, 2_000).is_err());
    }

    #[test]
    fn rotations_and_conjugation_preserve_a_synthetic_two_level_fit() {
        let interrupted = AtomicBool::new(false);
        let mut phase = 0.0f64;
        let samples = (0..2048)
            .map(|index| {
                let frequency = if (index / 32) % 2 == 0 {
                    -220_000.0
                } else {
                    220_000.0
                };
                phase += std::f64::consts::TAU * frequency / f64::from(CHANNEL_SAMPLE_RATE_HZ);
                ComplexSample {
                    re: phase.cos() as f32,
                    im: phase.sin() as f32,
                }
            })
            .collect::<Vec<_>>();
        let baseline =
            complex_record_feature(&samples, CHANNEL_SAMPLE_RATE_HZ, &interrupted).unwrap();
        for frequency in ROTATION_FREQUENCIES_HZ {
            let candidate = complex_record_feature(
                &rotate(&samples, frequency, false),
                CHANNEL_SAMPLE_RATE_HZ,
                &interrupted,
            )
            .unwrap();
            assert_eq!(baseline.is_some(), candidate.is_some());
            let change = (baseline.unwrap().two_level.unwrap().separation_hz
                - candidate.unwrap().two_level.unwrap().separation_hz)
                .abs();
            assert!(change < 2_000.0, "rotation changed separation by {change}");
        }
        let conjugated = complex_record_feature(
            &rotate(&samples, 0.0, true),
            CHANNEL_SAMPLE_RATE_HZ,
            &interrupted,
        )
        .unwrap();
        assert!(
            (baseline.unwrap().two_level.unwrap().separation_hz
                - conjugated.unwrap().two_level.unwrap().separation_hz)
                .abs()
                < 2_000.0
        );
    }

    #[test]
    fn action_selects_a_row_and_publishes_path_free_alias_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let bdb = directory.path().join("bursts.bdb");
        fs::write(&bdb, bdb_record(2438, 50_000, &q11_tone(512, 30_000.0))).unwrap();

        let mut stream = vec![ComplexSample { re: 0.01, im: 0.0 }; 2000];
        let target = oscillator(
            CONTROL_SAMPLE_RATE_HZ,
            (0..1024).map(|index| {
                if (index / 32) % 2 == 0 {
                    -1_530_000.0
                } else {
                    1_530_000.0
                }
            }),
        );
        stream[210..1234].copy_from_slice(&target);
        let cf32 = directory.path().join("frequency-2438000000.cf32");
        let cf32_data = cf32_bytes(&stream);
        fs::write(&cf32, &cf32_data).unwrap();
        let cf32_sha = format!("{:x}", Sha256::digest(&cf32_data));

        let manifest_path = directory.path().join("channelize-frequency.json");
        let manifest = serde_json::json!({
            "schema_version":1,
            "operation":"offline_frequency_channelization",
            "acquisition_sidecar":{"bytes":128,"sha256":"1".repeat(64)},
            "source":{
                "format":"SC16_Q11_LE", "sample_rate_hz":20000000,
                "bandwidth_hz":20000000, "processed_complex_samples":10000,
                "processed_prefix_bytes":40000, "processed_prefix_sha256":"2".repeat(64)
            },
            "filter":{
                "taps":225, "decimation":5, "group_delay_source_samples":112,
                "first_output_source_sample_index":4,
                "startup_transient_output_samples":44, "tail_flushed":false
            },
            "sample_index_mapping":{
                "output_stride_source_samples":5,
                "latest_source_index_for_output_zero":4,
                "group_delay_corrected_source_index_for_output_zero":-108
            },
            "target":{
                "center_frequency_hz":2438000000u64,
                "output_file":"frequency-2438000000.cf32", "output_format":"CF32_LE",
                "output_sample_rate_hz":4000000, "output_complex_samples":2000,
                "sha256":cf32_sha
            },
            "continuity_guaranteed":false
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let replay_path = directory.path().join("replay.json");
        let replay = serde_json::json!({
            "schema_version":2, "processed_full_input":true,
            "decoder_processed_complex_samples":10000,
            "channelizer_step_complex_samples":10, "channelizer_steps_processed":1000,
            "decoder_program":{
                "schema_version":1, "name":"blue-dragon", "bytes":1024,
                "sha256":"3".repeat(64)
            },
            "input_complex_samples":10000, "burst_archive_records":1,
            "burst_archive_truncated_by_limit":false,
            "radio_opened":false, "network_requested":false, "transmit_requested":false
        });
        fs::write(&replay_path, serde_json::to_vec(&replay).unwrap()).unwrap();

        let mut args = AliasControl {
            burst_input: bdb,
            cf32_input: cf32,
            manifest: manifest_path.clone(),
            replay_manifest: replay_path.clone(),
            family: Family::NarrowbandConstantEnvelopeUnresolved,
            frequency_mhz: 2438,
            output: Some(PathBuf::from("out")),
            max_burst_input_mib: 1,
            max_cf32_input_mib: 1,
            max_burst_samples: 10_000,
            max_records: 100,
            max_groups: 100,
            max_output_mib: 1,
        };
        run(directory.path(), &args, &AtomicBool::new(false)).unwrap();
        let report_bytes = fs::read(directory.path().join("out/alias-control.json")).unwrap();
        assert_eq!(
            fs::metadata(directory.path().join("out"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(directory.path().join("out/alias-control.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            !directory
                .path()
                .join("out/alias-control.json.partial")
                .exists()
        );
        let report: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();
        assert_eq!(report["schema_version"], 3);
        assert_eq!(report["selection"]["record_count"], 1);
        assert_eq!(report["selection"]["group_count"], 1);
        assert_eq!(
            report["alias_control_4msps"]["direct_unrecentered_diagnostic_fit_records"],
            1
        );
        assert_eq!(
            report["alias_control_4msps"]["qualified_direct_two_level_shape_records"],
            1
        );
        assert_eq!(
            report["alias_control_4msps"]["qualified_direct_separation_above_2mhz_records"],
            1
        );
        assert_eq!(
            report["alias_control_4msps"]["qualified_branch_spanning_alias_candidate_records"],
            1
        );
        assert_eq!(
            report["alias_control_4msps"]["qualified_branch_spanning_alias_candidate_observed"],
            true
        );
        assert_eq!(
            report["alias_control_4msps"]["paired_comparison_filter_sensitive_inconclusive"],
            true
        );
        let matched = &report["bandwidth_matched_same_stream"];
        assert_eq!(matched["accounting"]["base_control_available_records"], 1);
        assert_eq!(
            matched["accounting"]["matched_context_available_records"],
            1
        );
        assert_eq!(matched["same_source_samples"], true);
        assert_eq!(matched["identical_prefilter"], true);
        assert_eq!(matched["blue_dragon_bit_exact_comparison"], false);
        assert_eq!(
            matched["comparison"]["comparison_population"],
            "records_with_all_three_branches_qualified"
        );
        assert_eq!(
            matched["partition_uses_first_outcome_in_declared_order"],
            true
        );
        assert_eq!(matched["outcome_partition_is_closed"], true);
        assert_eq!(
            matched["filter"]["coefficient_sha256"],
            "a7499acefb635acb0272c674b11d714d3c9ef16e4401da90322223ae09867cd3"
        );
        assert_eq!(
            matched["filter"]["derivation_source_revision"],
            BLUE_DRAGON_ALIGNMENT_REVISION
        );
        for name in [
            "filtered_4msps_direct_gate_breakdown",
            "global_even_2msps_direct_gate_breakdown",
            "global_odd_2msps_direct_gate_breakdown",
        ] {
            assert_eq!(
                matched[name]["accounting"]["control_window_available_records"],
                1
            );
            assert_eq!(matched[name]["first_failure_partition_is_closed"], true);
        }
        for name in [
            "circular_recentered_gate_breakdown",
            "direct_unrecentered_gate_breakdown",
        ] {
            assert_eq!(
                report["alias_control_4msps"][name]["accounting"]["control_window_available_records"],
                1
            );
            assert_eq!(
                report["alias_control_4msps"][name]["first_failure_partition"]["all_gates_passed"],
                1
            );
            assert_eq!(
                report["alias_control_4msps"][name]["first_failure_partition_is_closed"],
                true
            );
        }
        let separation = report["alias_control_4msps"]["qualified_direct_separation_hz"]["mean"]
            .as_f64()
            .unwrap();
        assert!((separation - 3_060_000.0).abs() < 4_000.0);
        assert_eq!(
            report["manifest_attested_acquisition_sidecar"]["independently_verified"],
            false
        );
        assert_eq!(
            report["manifest_attested_source_prefix"]["independently_verified"],
            false
        );
        assert_eq!(
            report["replay_attested_decoder_program"]["independently_verified"],
            false
        );
        assert_eq!(report["evidence_limited"], true);
        assert_eq!(report["protocol_identity_established"], false);
        assert_eq!(
            report["mapping"]["pfb_burst_catcher_integration_fixture_executed"],
            false
        );
        let rendered = String::from_utf8(report_bytes).unwrap();
        assert!(!rendered.contains(directory.path().to_string_lossy().as_ref()));
        assert!(run(directory.path(), &args, &AtomicBool::new(false)).is_err());

        let mut incompatible_replay = replay.clone();
        incompatible_replay["network_requested"] = true.into();
        fs::write(
            &replay_path,
            serde_json::to_vec(&incompatible_replay).unwrap(),
        )
        .unwrap();
        args.output = Some(PathBuf::from("replay-mismatch"));
        assert!(run(directory.path(), &args, &AtomicBool::new(false)).is_err());
        assert!(!directory.path().join("replay-mismatch").exists());
        fs::write(&replay_path, serde_json::to_vec(&replay).unwrap()).unwrap();

        let mut incompatible_manifest = manifest.clone();
        incompatible_manifest["target"]["center_frequency_hz"] = 2_437_000_000u64.into();
        fs::write(
            &manifest_path,
            serde_json::to_vec(&incompatible_manifest).unwrap(),
        )
        .unwrap();
        args.output = Some(PathBuf::from("manifest-mismatch"));
        assert!(run(directory.path(), &args, &AtomicBool::new(false)).is_err());
        assert!(!directory.path().join("manifest-mismatch").exists());

        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let early_bdb = directory.path().join("early-bursts.bdb");
        fs::write(&early_bdb, bdb_record(2438, 0, &q11_tone(512, 30_000.0))).unwrap();
        args.burst_input = early_bdb;
        args.output = Some(PathBuf::from("startup-window"));
        run(directory.path(), &args, &AtomicBool::new(false)).unwrap();
        let startup_report: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.path().join("startup-window/alias-control.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(startup_report["mapping"]["selected_windows"], 1);
        assert_eq!(startup_report["mapping"]["valid_control_windows"], 0);
        assert_eq!(startup_report["mapping"]["startup_transient_windows"], 1);
        assert_eq!(
            startup_report["alias_control_4msps"]["control_window_available_records"],
            0
        );
        assert_eq!(
            startup_report["bandwidth_matched_same_stream"]["accounting"]["base_control_unavailable_records"],
            1
        );
        assert_eq!(
            startup_report["bandwidth_matched_same_stream"]["accounting"]["matched_context_available_records"],
            0
        );
        for name in [
            "circular_recentered_gate_breakdown",
            "direct_unrecentered_gate_breakdown",
        ] {
            assert_eq!(
                startup_report["alias_control_4msps"][name]["accounting"]["control_window_available_records"],
                0
            );
        }

        let start_context_bdb = directory.path().join("start-context-bursts.bdb");
        fs::write(
            &start_context_bdb,
            bdb_record(2438, 10_000, &q11_tone(512, 30_000.0)),
        )
        .unwrap();
        args.burst_input = start_context_bdb;
        args.output = Some(PathBuf::from("start-context-window"));
        run(directory.path(), &args, &AtomicBool::new(false)).unwrap();
        let start_context_report: serde_json::Value = serde_json::from_slice(
            &fs::read(
                directory
                    .path()
                    .join("start-context-window/alias-control.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(start_context_report["mapping"]["valid_control_windows"], 1);
        assert_eq!(
            start_context_report["bandwidth_matched_same_stream"]["accounting"]["start_context_unavailable_records"],
            1
        );
        assert_eq!(
            start_context_report["bandwidth_matched_same_stream"]["accounting"]["matched_context_available_records"],
            0
        );

        let tail_context_bdb = directory.path().join("tail-context-bursts.bdb");
        fs::write(
            &tail_context_bdb,
            bdb_record(2438, 240_000, &q11_tone(512, 30_000.0)),
        )
        .unwrap();
        args.burst_input = tail_context_bdb;
        args.output = Some(PathBuf::from("tail-context-window"));
        run(directory.path(), &args, &AtomicBool::new(false)).unwrap();
        let tail_context_report: serde_json::Value = serde_json::from_slice(
            &fs::read(
                directory
                    .path()
                    .join("tail-context-window/alias-control.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(tail_context_report["mapping"]["valid_control_windows"], 1);
        assert_eq!(
            tail_context_report["bandwidth_matched_same_stream"]["accounting"]["tail_context_unavailable_records"],
            1
        );
        assert_eq!(
            tail_context_report["bandwidth_matched_same_stream"]["accounting"]["matched_context_available_records"],
            0
        );
    }

    #[test]
    fn limits_reject_zero_and_above_hard_maxima() {
        let mut args = AliasControl {
            burst_input: PathBuf::from("bursts.bdb"),
            cf32_input: PathBuf::from("frequency.cf32"),
            manifest: PathBuf::from("channelize-frequency.json"),
            replay_manifest: PathBuf::from("replay.json"),
            family: Family::NarrowbandConstantEnvelopeUnresolved,
            frequency_mhz: 2438,
            output: None,
            max_burst_input_mib: 1,
            max_cf32_input_mib: 1,
            max_burst_samples: 1,
            max_records: 1,
            max_groups: 1,
            max_output_mib: 1,
        };
        assert!(args.limits().is_ok());
        args.max_cf32_input_mib = 65;
        assert!(args.limits().is_err());
        args.max_cf32_input_mib = 1;
        args.max_burst_samples = 0;
        assert!(args.limits().is_err());
        args.max_burst_samples = 1;
        args.max_records = 20_001;
        assert!(args.limits().is_err());
    }
}
