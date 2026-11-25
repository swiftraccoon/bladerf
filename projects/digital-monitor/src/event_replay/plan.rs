//! Verify an event slice export against its original closed IQ capture.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

use crate::input::{self, SourceReport};
use crate::report;

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_SAMPLES: u64 = 200_000_000;
pub(super) const MAX_SLICES: usize = 4096;
const MAX_EVENTS: u64 = 4096;
const MAX_EVENT_CELLS_PER_WINDOW: u64 = 198;
pub(super) const WINDOW_SAMPLES: u64 = 8192;
pub(super) const OVERLAP_STRIDE_SAMPLES: u64 = WINDOW_SAMPLES / 2;
const ALIGNMENT_SAMPLES: u64 = 10;
const SAMPLE_BYTES: u64 = 4;
const SIDECAR_BYTES: u64 = 64 * 1024;
pub(super) const MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
pub(super) const MANIFEST_NAME: &str = "slices.json";

#[derive(Debug)]
pub(super) struct Plan {
    pub(super) source: SourceReport,
    pub(super) manifest_sha256: String,
    pub(super) analyzed_samples: u64,
    pub(super) exported_samples: u64,
    pub(super) omitted_component_count: u64,
    pub(super) slices: Vec<Slice>,
    snapshots: Vec<PathSnapshot>,
}

#[derive(Debug)]
struct PathSnapshot {
    path: PathBuf,
    metadata: Metadata,
}

impl Plan {
    /// Recheck the verified artifact identities without rereading their contents.
    pub(super) fn verify_snapshot(&self, stop: &AtomicBool) -> Result<()> {
        for snapshot in &self.snapshots {
            check_stop(stop)?;
            let observed = fs::symlink_metadata(&snapshot.path).with_context(|| {
                format!("inspect event replay input {}", snapshot.path.display())
            })?;
            ensure!(
                observed.file_type() == snapshot.metadata.file_type()
                    && report::same_file_identity(&snapshot.metadata, &observed),
                "event replay input changed since verification: {}",
                snapshot.path.display()
            );
        }
        check_stop(stop)
    }
}

#[derive(Debug)]
pub(super) struct Slice {
    pub(super) iq_file: String,
    pub(super) start_sample: u64,
    pub(super) end_sample_exclusive: u64,
}

impl Slice {
    pub(super) fn samples(&self) -> u64 {
        self.end_sample_exclusive - self.start_sample
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    operation: String,
    source_format: String,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    input_samples: u64,
    analyzed_samples: u64,
    #[serde(default, deserialize_with = "deserialize_present_u64")]
    event_window_stride_samples: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present_u64")]
    active_window_count: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present_u64")]
    active_window_range_count: Option<u64>,
    analyzed_prefix_sha256: String,
    event_report_sha256: String,
    acquisition_sha256: String,
    event_count: u64,
    omitted_component_count: u64,
    alignment_samples: u64,
    alignment_omitted_tail_samples: u64,
    limits: Limits,
    exported_samples: u64,
    slices: Vec<ManifestSlice>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    pre_roll_samples: u64,
    post_roll_samples: u64,
    #[serde(default)]
    merge_gap_samples: u64,
    max_slices: usize,
    max_total_samples: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSlice {
    iq_file: String,
    start_sample: u64,
    end_sample_exclusive: u64,
    iq_sha256: String,
}

impl From<ManifestSlice> for Slice {
    fn from(slice: ManifestSlice) -> Self {
        Self {
            iq_file: slice.iq_file,
            start_sample: slice.start_sample,
            end_sample_exclusive: slice.end_sample_exclusive,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SliceSidecar {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: u16,
    frequency_hz: u64,
    sample_rate_hz: u64,
    bandwidth_hz: u64,
    num_samples: u64,
    source_kind: String,
    source_start_sample: u64,
    continuity_guaranteed: bool,
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sidecar_name(iq_file: &str) -> String {
    format!("{iq_file}.json")
}

fn deserialize_present_u64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}

impl Manifest {
    fn validate(&self) -> Result<HashSet<OsString>> {
        let stride = match (self.schema_version, self.event_window_stride_samples) {
            (1, None) => WINDOW_SAMPLES,
            (2, Some(OVERLAP_STRIDE_SAMPLES)) => OVERLAP_STRIDE_SAMPLES,
            (3, Some(stride @ (WINDOW_SAMPLES | OVERLAP_STRIDE_SAMPLES))) => stride,
            _ => anyhow::bail!("invalid event slice manifest schema or window stride"),
        };
        ensure!(
            self.operation == "offline_rf_event_slice_export"
                && self.source_format == input::NORMALIZED_SC16_Q11_FORMAT
                && self.sample_rate_hz == SAMPLE_RATE_HZ
                && (70_000_000..=6_000_000_000).contains(&self.center_frequency_hz),
            "expected SC16_Q11_LE event slices at 20 MSps"
        );
        ensure!(
            self.input_samples <= MAX_SAMPLES
                && (WINDOW_SAMPLES..=self.input_samples).contains(&self.analyzed_samples)
                && (self.analyzed_samples - WINDOW_SAMPLES).is_multiple_of(stride)
                && self.alignment_samples == ALIGNMENT_SAMPLES
                && self.alignment_omitted_tail_samples == self.analyzed_samples % ALIGNMENT_SAMPLES,
            "invalid event slice sample extent or alignment"
        );
        ensure!(
            is_sha256(&self.analyzed_prefix_sha256)
                && is_sha256(&self.event_report_sha256)
                && is_sha256(&self.acquisition_sha256),
            "invalid event slice provenance SHA-256"
        );
        ensure!(
            self.limits.pre_roll_samples <= SAMPLE_RATE_HZ
                && self.limits.post_roll_samples <= SAMPLE_RATE_HZ
                && self.limits.merge_gap_samples <= SAMPLE_RATE_HZ
                && (1..=MAX_SLICES).contains(&self.limits.max_slices)
                && (1..=MAX_SAMPLES).contains(&self.limits.max_total_samples)
                && self.slices.len() <= self.limits.max_slices
                && self.exported_samples <= self.limits.max_total_samples,
            "invalid event slice export limits"
        );
        let event_windows = (self.analyzed_samples - WINDOW_SAMPLES) / stride + 1;
        let (component_windows, ranges) = match (
            self.schema_version,
            self.active_window_count,
            self.active_window_range_count,
        ) {
            (1 | 2, None, None) => (event_windows, self.event_count),
            (3, Some(active), Some(ranges)) => {
                ensure!(
                    active <= event_windows
                        && ranges <= active
                        && ranges <= event_windows - active + 1
                        && (active == 0) == (ranges == 0)
                        && (active == 0) == (self.event_count == 0),
                    "invalid event slice active window accounting"
                );
                (active, ranges)
            }
            _ => anyhow::bail!("invalid event slice manifest schema or active window counts"),
        };
        // An inactive window separates components, so each active range contains at least one.
        ensure!(
            self.event_count <= MAX_EVENTS
                && (self.omitted_component_count == 0 || self.event_count > 0)
                && self
                    .event_count
                    .checked_add(self.omitted_component_count)
                    .is_some_and(|total| {
                        (ranges..=component_windows * MAX_EVENT_CELLS_PER_WINDOW).contains(&total)
                    }),
            "invalid event slice component accounting"
        );
        ensure!(
            self.slices.len() as u64 <= ranges && (ranges == 0) == self.slices.is_empty(),
            "invalid event slice count for the declared ranges"
        );

        let aligned_prefix = self.analyzed_samples - self.alignment_omitted_tail_samples;
        let mut expected_paths = HashSet::with_capacity(self.slices.len() * 2 + 1);
        expected_paths.insert(OsString::from(MANIFEST_NAME));
        let mut previous_end = None;
        let mut exported_samples = 0;
        for (index, slice) in self.slices.iter().enumerate() {
            ensure!(
                slice.iq_file == format!("slice-{:04}.sc16q11", index + 1),
                "unexpected event slice filename at position {}",
                index + 1
            );
            ensure!(
                slice.start_sample < slice.end_sample_exclusive
                    && slice.end_sample_exclusive <= aligned_prefix
                    && slice.start_sample.is_multiple_of(ALIGNMENT_SAMPLES)
                    && slice.end_sample_exclusive.is_multiple_of(ALIGNMENT_SAMPLES),
                "event slice ranges must be nonempty and aligned within the prefix"
            );
            ensure!(
                previous_end.is_none_or(|end| {
                    slice
                        .start_sample
                        .checked_sub(end)
                        .is_some_and(|gap| gap > self.limits.merge_gap_samples)
                }),
                "event slice ranges must be ordered with gaps larger than merge_gap_samples"
            );
            ensure!(is_sha256(&slice.iq_sha256), "invalid event slice SHA-256");
            previous_end = Some(slice.end_sample_exclusive);
            exported_samples += slice.end_sample_exclusive - slice.start_sample;
            expected_paths.insert(OsString::from(&slice.iq_file));
            expected_paths.insert(OsString::from(sidecar_name(&slice.iq_file)));
        }
        ensure!(
            exported_samples == self.exported_samples,
            "event slice sample sum differs from exported_samples"
        );
        Ok(expected_paths)
    }

    fn verify_slice(&self, root: &Path, slice: &ManifestSlice) -> Result<[PathSnapshot; 2]> {
        let samples = slice.end_sample_exclusive - slice.start_sample;
        let bytes = samples * SAMPLE_BYTES;
        let iq_path = root.join(&slice.iq_file);
        let (raw, iq_metadata) = report::hash_direct_regular_file_with_snapshot(&iq_path, bytes)?;
        ensure!(
            raw.bytes == bytes && raw.sha256 == slice.iq_sha256,
            "event slice byte length or SHA-256 differs from the manifest"
        );

        let sidecar_path = root.join(sidecar_name(&slice.iq_file));
        let (sidecar, _, sidecar_metadata) =
            report::read_json_with_snapshot(&sidecar_path, SIDECAR_BYTES)?;
        let sidecar: SliceSidecar =
            serde_json::from_value(sidecar).context("parse event slice sidecar")?;
        ensure!(
            sidecar.format == "SC16_Q11"
                && sidecar.dtype == "<i2"
                && sidecar.interleave == "I,Q"
                && sidecar.complex_scale == 2048
                && sidecar.frequency_hz == self.center_frequency_hz
                && sidecar.sample_rate_hz == self.sample_rate_hz
                && sidecar.bandwidth_hz == self.sample_rate_hz
                && sidecar.num_samples == samples
                && sidecar.source_kind == "derived_event_slice"
                && sidecar.source_start_sample == slice.start_sample
                && !sidecar.continuity_guaranteed,
            "event slice sidecar differs from the export geometry"
        );
        Ok([
            PathSnapshot {
                path: iq_path,
                metadata: iq_metadata,
            },
            PathSnapshot {
                path: sidecar_path,
                metadata: sidecar_metadata,
            },
        ])
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "event replay verification interrupted"
    );
    Ok(())
}

struct SourceReader {
    file: File,
    buffer: Vec<u8>,
    samples_read: u64,
    digest: Sha256,
}

impl SourceReader {
    fn consume(
        &mut self,
        end: u64,
        stop: &AtomicBool,
        mut inspect: impl FnMut(&[u8]),
    ) -> Result<()> {
        while self.samples_read < end {
            check_stop(stop)?;
            let count = (end - self.samples_read).min(self.buffer.len() as u64 / SAMPLE_BYTES);
            let bytes = &mut self.buffer[..(count * SAMPLE_BYTES) as usize];
            self.file
                .read_exact(bytes)
                .context("read event replay source IQ")?;
            input::validate_q11(bytes)?;
            self.digest.update(&*bytes);
            inspect(bytes);
            self.samples_read += count;
        }
        Ok(())
    }
}

fn verify_source(
    input_path: &Path,
    expected_bytes: u64,
    manifest: &Manifest,
    root: &Path,
    stop: &AtomicBool,
    snapshots: &mut Vec<PathSnapshot>,
) -> Result<String> {
    let before = fs::symlink_metadata(input_path)?;
    ensure!(
        before.is_file() && before.len() == expected_bytes,
        "event replay source must be a direct regular file with the validated byte length"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(input_path)
        .context("open event replay source IQ")?;
    ensure!(
        report::same_file_identity(&before, &file.metadata()?),
        "event replay source changed while opening"
    );
    let mut reader = SourceReader {
        file,
        buffer: vec![0; 64 * 1024],
        samples_read: 0,
        digest: Sha256::new(),
    };
    for slice in &manifest.slices {
        reader.consume(slice.start_sample, stop, |_| {})?;
        let mut subset = Sha256::new();
        reader.consume(slice.end_sample_exclusive, stop, |bytes| {
            subset.update(bytes)
        })?;
        ensure!(
            format!("{:x}", subset.finalize()) == slice.iq_sha256,
            "{} SHA-256 differs from its exact source IQ range",
            slice.iq_file
        );
        snapshots.extend(
            manifest
                .verify_slice(root, slice)
                .with_context(|| format!("verify event export {}", slice.iq_file))?,
        );
    }
    reader.consume(manifest.analyzed_samples, stop, |_| {})?;
    // The same digest state can continue through the unanalyzed tail.
    ensure!(
        format!("{:x}", reader.digest.clone().finalize()) == manifest.analyzed_prefix_sha256,
        "analyzed prefix SHA-256 differs from the event slice manifest"
    );
    reader.consume(manifest.input_samples, stop, |_| {})?;
    ensure!(
        report::same_file_identity(&before, &reader.file.metadata()?)
            && report::same_file_identity(&before, &fs::symlink_metadata(input_path)?),
        "source IQ changed during event replay verification"
    );
    snapshots.push(PathSnapshot {
        path: input_path.to_path_buf(),
        metadata: before,
    });
    Ok(format!("{:x}", reader.digest.finalize()))
}

pub(super) fn verify(input: &Path, slice_root: &Path, stop: &AtomicBool) -> Result<Plan> {
    check_stop(stop)?;
    let root_before = fs::symlink_metadata(slice_root).context("inspect event slice directory")?;
    ensure!(
        root_before.is_dir(),
        "event slice path must be a direct directory"
    );
    let manifest_path = slice_root.join(MANIFEST_NAME);
    let (value, manifest_sha256, manifest_metadata) =
        report::read_json_with_snapshot(&manifest_path, MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_value(value).context("parse event slice manifest")?;
    let mut expected_paths = manifest.validate()?;
    for entry in fs::read_dir(slice_root)? {
        ensure!(
            expected_paths.remove(&entry?.file_name()),
            "unexpected event export directory entry"
        );
    }
    ensure!(
        expected_paths.is_empty(),
        "event export directory is incomplete"
    );

    let mut snapshots = Vec::with_capacity(4 + 2 * manifest.slices.len());
    snapshots.push(PathSnapshot {
        path: slice_root.to_path_buf(),
        metadata: root_before,
    });
    let source = input::validate_with_iq(
        input,
        manifest.center_frequency_hz,
        SAMPLE_RATE_HZ,
        MAX_SAMPLES,
        |input_path, expected_bytes, acquisition_sha256| {
            ensure!(
                expected_bytes == manifest.input_samples * SAMPLE_BYTES
                    && acquisition_sha256 == manifest.acquisition_sha256,
                "source sample count or acquisition SHA-256 differs from the event slice manifest"
            );
            verify_source(
                input_path,
                expected_bytes,
                &manifest,
                slice_root,
                stop,
                &mut snapshots,
            )
        },
    )?;
    let (acquisition, acquisition_metadata) =
        report::hash_direct_regular_file_with_snapshot(&source.sidecar, SIDECAR_BYTES)?;
    ensure!(
        acquisition.sha256 == source.sidecar_sha256,
        "source acquisition sidecar changed during event replay verification"
    );
    snapshots.push(PathSnapshot {
        path: source.sidecar.clone(),
        metadata: acquisition_metadata,
    });
    ensure!(
        report::hash_direct_regular_file(&manifest_path, MANIFEST_BYTES)?.sha256 == manifest_sha256,
        "event export changed during verification"
    );
    snapshots.push(PathSnapshot {
        path: manifest_path,
        metadata: manifest_metadata,
    });
    let plan = Plan {
        source,
        manifest_sha256,
        analyzed_samples: manifest.analyzed_samples,
        exported_samples: manifest.exported_samples,
        omitted_component_count: manifest.omitted_component_count,
        slices: manifest.slices.into_iter().map(Slice::from).collect(),
        snapshots,
    };
    plan.verify_snapshot(stop)?;
    Ok(plan)
}

#[cfg(test)]
mod tests;
