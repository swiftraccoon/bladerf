//! Verify atomically published capture directories before rolling analysis.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::{input, report};

pub(super) const SEGMENT_SAMPLES: u64 = 19_988_480;
pub(super) const MAX_SEGMENTS: usize = 3600;
pub(super) const DEFAULT_KEEP_LIMIT: usize = 12;
const SAMPLE_RATE_HZ: u64 = 20_000_000;
const SAMPLE_PERIOD_NS: i64 = 50;
const RECEIPT_BYTES: u64 = 4096;
pub(super) const IQ_NAME: &str = "iq.sc16q11";
const SIDECAR_NAME: &str = "iq.sc16q11.json";
const RECEIPT_NAME: &str = "segment.json";
const FILE_NAMES: [&str; 3] = [IQ_NAME, SIDECAR_NAME, RECEIPT_NAME];

#[derive(Clone, Copy, Debug)]
pub(super) struct SegmentSettings {
    pub(super) center_frequency_hz: u64,
    pub(super) exact_samples: u64,
    pub(super) discarded_initial_samples: u64,
    pub(super) gain_db: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SegmentReceipt {
    schema_version: u32,
    pub(super) index: usize,
    pub(super) start_sample: u64,
    pub(super) num_samples: u64,
    pub(super) first_sample_ns: i64,
    pub(super) end_sample_exclusive_ns: i64,
    pub(super) witness_start_ns: i64,
    pub(super) iq_sha256: String,
    pub(super) sidecar_sha256: String,
    #[serde(default)]
    pub(super) finished_on_request: bool,
}

impl SegmentReceipt {
    fn validate(&self, index: usize, samples: u64, previous_end_ns: Option<i64>) -> Result<()> {
        let offset = u64::try_from(index)?
            .checked_mul(samples)
            .context("rolling segment sample offset overflow")?;
        ensure!(
            self.schema_version == 1
                && self.index == index
                && self.start_sample == offset
                && self.num_samples == samples,
            "rolling segment receipt differs from the requested index or sample extent"
        );
        let duration_ns = i64::try_from(samples)?
            .checked_mul(SAMPLE_PERIOD_NS)
            .context("rolling segment duration overflow")?;
        ensure!(
            self.first_sample_ns.checked_add(duration_ns) == Some(self.end_sample_exclusive_ns)
                && self.witness_start_ns == self.end_sample_exclusive_ns,
            "rolling segment device clock or end witness is inconsistent"
        );
        ensure!(
            previous_end_ns.is_none_or(|previous| self.first_sample_ns == previous),
            "rolling segment starts after a device clock discontinuity"
        );
        for digest in [&self.iq_sha256, &self.sidecar_sha256] {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "rolling segment receipt contains an invalid SHA-256"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct ValidatedSegment {
    pub(super) source: input::SourceReport,
    pub(super) receipt: SegmentReceipt,
    pub(super) receipt_sha256: String,
    root: (PathBuf, Metadata),
    paths: [(PathBuf, Metadata); 4],
}

impl ValidatedSegment {
    /// Recheck direct paths without hashing the IQ again.
    pub(super) fn verify_unchanged(&self) -> Result<()> {
        verify_root(&self.root.0, &self.root.1)?;
        verify_paths(&self.paths)
    }

    /// Pin the admitted directory for descriptor-relative removal of the admitted IQ.
    pub(super) fn open_for_release(&self) -> Result<(File, &Metadata)> {
        self.verify_unchanged()?;
        let (directory, before) = &self.paths[0];
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(directory)
            .context("pin rolling segment directory for IQ release")?;
        ensure!(
            report::same_file_identity(&opened.metadata()?, before),
            "rolling segment directory changed before IQ release"
        );
        Ok((opened, &self.paths[1].1))
    }

    /// Recheck the numbered directory and original sidecars against the release proof.
    pub(super) fn verify_after_release(&self, released_directory: &Metadata) -> Result<()> {
        verify_root(&self.root.0, &self.root.1)?;
        let (directory, before) = &self.paths[0];
        ensure!(
            released_directory.dev() == before.dev() && released_directory.ino() == before.ino(),
            "released rolling directory differs from the admitted directory"
        );
        verify_entries(directory, &[SIDECAR_NAME, RECEIPT_NAME])?;
        verify_paths(&self.paths[2..])?;
        let current =
            direct_directory(directory)?.context("released rolling directory disappeared")?;
        ensure!(
            report::same_file_identity(&current, released_directory),
            "rolling segment directory changed after IQ release"
        );
        Ok(())
    }
}

fn verify_paths(paths: &[(PathBuf, Metadata)]) -> Result<()> {
    for (path, before) in paths {
        let current = fs::symlink_metadata(path)
            .with_context(|| format!("recheck rolling segment path {}", path.display()))?;
        ensure!(
            current.file_type() == before.file_type()
                && report::same_file_identity(&current, before),
            "rolling segment path changed after validation: {}",
            path.display()
        );
    }
    Ok(())
}

fn direct_directory(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir(),
                "rolling path must be a direct directory: {}",
                path.display()
            );
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspect rolling path {}", path.display()))
        }
    }
}

fn verify_root(path: &Path, before: &Metadata) -> Result<()> {
    let current = direct_directory(path)?.context("rolling capture root disappeared")?;
    // Publishing sibling segments updates the capture root's directory timestamps.
    ensure!(
        current.dev() == before.dev() && current.ino() == before.ino(),
        "rolling capture root was replaced"
    );
    Ok(())
}

fn direct_file(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect rolling segment file {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "rolling segment entry must be a direct regular file: {}",
        path.display()
    );
    Ok(metadata)
}

fn verify_entries(directory: &Path, expected: &[&str]) -> Result<()> {
    let mut entries = 0;
    for entry in fs::read_dir(directory)? {
        let name = entry?.file_name();
        ensure!(
            expected.iter().any(|expected| name == *expected),
            "unexpected entry in rolling segment directory"
        );
        entries += 1;
    }
    ensure!(
        entries == expected.len(),
        "rolling segment directory is incomplete"
    );
    Ok(())
}

/// Poll the next numbered directory. Only an unpublished segment returns `None`.
pub(super) fn read_segment(
    root: &Path,
    settings: SegmentSettings,
    index: usize,
    previous_end_ns: Option<i64>,
) -> Result<Option<ValidatedSegment>> {
    ensure!(
        (8192..=SEGMENT_SAMPLES).contains(&settings.exact_samples)
            && (index == 0) == previous_end_ns.is_none(),
        "invalid rolling segment index, sample limit, or preceding clock bound"
    );
    let Some(root_before) = direct_directory(root)? else {
        return Ok(None);
    };
    let ordinal = index
        .checked_add(1)
        .context("rolling segment ordinal overflow")?;
    let directory = root.join(format!("{ordinal:06}"));
    let Some(directory_before) = direct_directory(&directory)? else {
        verify_root(root, &root_before)?;
        return Ok(None);
    };

    verify_entries(&directory, &FILE_NAMES)?;
    let iq = directory.join(IQ_NAME);
    let sidecar = directory.join(SIDECAR_NAME);
    let receipt_path = directory.join(RECEIPT_NAME);
    let iq_before = direct_file(&iq)?;
    let sidecar_before = direct_file(&sidecar)?;
    let (receipt, receipt_sha256, receipt_before) =
        report::read_json_with_snapshot(&receipt_path, RECEIPT_BYTES)?;
    let receipt: SegmentReceipt =
        serde_json::from_value(receipt).context("parse rolling segment receipt")?;
    receipt.validate(index, settings.exact_samples, previous_end_ns)?;
    let source = input::validate_live(
        &iq,
        input::LiveExpectation {
            center_frequency_hz: settings.center_frequency_hz,
            sample_rate_hz: SAMPLE_RATE_HZ,
            exact_samples: settings.exact_samples,
            discarded_initial_samples: if index == 0 {
                settings.discarded_initial_samples
            } else {
                0
            },
            gain_db: settings.gain_db,
            channel: "RX1",
            fpga_version: "0.16.0",
        },
    )?;
    ensure!(
        source.sha256 == receipt.iq_sha256 && source.sidecar_sha256 == receipt.sidecar_sha256,
        "rolling segment IQ or sidecar digest differs from its receipt"
    );
    let segment = ValidatedSegment {
        source,
        receipt,
        receipt_sha256,
        root: (root.to_path_buf(), root_before),
        paths: [
            (directory, directory_before),
            (iq, iq_before),
            (sidecar, sidecar_before),
            (receipt_path, receipt_before),
        ],
    };
    segment.verify_unchanged()?;
    Ok(Some(segment))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use crate::rolling::{
        detection::Detection,
        retention::{ReleaseEvidence, release_iq},
    };

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::*;

    const SETTINGS: SegmentSettings = SegmentSettings {
        center_frequency_hz: 2_437_000_000,
        exact_samples: 8192,
        discarded_initial_samples: 128,
        gain_db: 40.0,
    };

    fn fixture(root: &Path, index: usize, first_sample_ns: i64) -> (PathBuf, Value) {
        let directory = root.join(format!("{:06}", index + 1));
        fs::create_dir(&directory).unwrap();
        let iq = vec![0; SETTINGS.exact_samples as usize * 4];
        fs::write(directory.join(IQ_NAME), &iq).unwrap();
        let mut sidecar = json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": SETTINGS.center_frequency_hz,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "num_samples": SETTINGS.exact_samples,
            "gain_db": SETTINGS.gain_db,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "fpga_version": "0.16.0",
            "required_fpga_version": "0.16.0"
        });
        if index == 0 {
            sidecar["discarded_initial_samples"] = json!(SETTINGS.discarded_initial_samples);
        }
        let sidecar = serde_json::to_vec(&sidecar).unwrap();
        fs::write(directory.join(SIDECAR_NAME), &sidecar).unwrap();
        let end = first_sample_ns + SETTINGS.exact_samples as i64 * SAMPLE_PERIOD_NS;
        let receipt = json!({
            "schema_version": 1,
            "index": index,
            "start_sample": index as u64 * SETTINGS.exact_samples,
            "num_samples": SETTINGS.exact_samples,
            "first_sample_ns": first_sample_ns,
            "end_sample_exclusive_ns": end,
            "witness_start_ns": end,
            "iq_sha256": format!("{:x}", Sha256::digest(iq)),
            "sidecar_sha256": format!("{:x}", Sha256::digest(sidecar))
        });
        fs::write(
            directory.join(RECEIPT_NAME),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        (directory, receipt)
    }

    #[test]
    fn accepts_ordered_witnesses_and_rejects_clock_gap() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            read_segment(root.path(), SETTINGS, 0, None)
                .unwrap()
                .is_none()
        );
        fixture(root.path(), 0, -101);
        let first = read_segment(root.path(), SETTINGS, 0, None)
            .unwrap()
            .unwrap();
        assert_eq!(first.receipt.first_sample_ns, -101);
        let end = first.receipt.end_sample_exclusive_ns;
        let (directory, _) = fixture(root.path(), 1, end);
        let second = read_segment(root.path(), SETTINGS, 1, Some(end))
            .unwrap()
            .unwrap();
        assert_eq!(second.source.complex_samples, SETTINGS.exact_samples);
        first.verify_unchanged().unwrap();
        let bad = read_segment(root.path(), SETTINGS, 1, Some(end - 50)).unwrap_err();
        assert!(bad.to_string().contains("clock discontinuity"));
        assert!(directory.exists());
    }

    #[test]
    fn admits_continuous_ordinals_with_the_previous_clock() {
        let root = tempfile::tempdir().unwrap();
        for index in [120, 3600, 999_999] {
            fixture(root.path(), index, 64_000);
            let segment = read_segment(root.path(), SETTINGS, index, Some(64_000))
                .unwrap()
                .unwrap();
            assert_eq!(segment.receipt.index, index);
            assert_eq!(
                segment.receipt.start_sample,
                index as u64 * SETTINGS.exact_samples
            );
            assert!(read_segment(root.path(), SETTINGS, index, Some(63_950)).is_err());
        }
    }

    #[test]
    fn rejects_invalid_receipt_and_same_byte_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let (directory, mut receipt) = fixture(root.path(), 0, 6400);
        let segment = read_segment(root.path(), SETTINGS, 0, None)
            .unwrap()
            .unwrap();
        let replacement = root.path().join("replacement");
        fs::copy(directory.join(IQ_NAME), &replacement).unwrap();
        fs::rename(replacement, directory.join(IQ_NAME)).unwrap();
        assert!(segment.verify_unchanged().is_err());

        receipt["witness_start_ns"] = json!(0);
        fs::write(
            directory.join(RECEIPT_NAME),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let bad = read_segment(root.path(), SETTINGS, 0, None).unwrap_err();
        assert!(bad.to_string().contains("end witness"));
    }

    #[test]
    fn rejects_indirect_numbered_directories_and_wrong_discard() {
        let root = tempfile::tempdir().unwrap();
        let (directory, _) = fixture(root.path(), 0, 6400);
        let settings = SegmentSettings {
            discarded_initial_samples: 0,
            ..SETTINGS
        };
        let bad = read_segment(root.path(), settings, 0, None).unwrap_err();
        assert!(bad.to_string().contains("initial discard"));

        let original = root.path().join("original");
        fs::rename(&directory, &original).unwrap();
        std::os::unix::fs::symlink(original, directory).unwrap();
        let bad = read_segment(root.path(), SETTINGS, 0, None).unwrap_err();
        assert!(bad.to_string().contains("direct directory"));
    }

    fn released_input(root: &Path) -> (ValidatedSegment, Detection, PathBuf, PathBuf) {
        let captures = root.join("captures");
        fs::create_dir(&captures).unwrap();
        let (directory, _) = fixture(&captures, 0, -101);
        let segment = read_segment(&captures, SETTINGS, 0, None).unwrap().unwrap();
        let report_dir = root.join("segments/000001");
        fs::create_dir_all(report_dir.join("survey")).unwrap();
        let events = br#"{"events":[]}"#;
        fs::write(report_dir.join("survey/events.json"), events).unwrap();
        let detection = Detection {
            event_report_sha256: format!("{:x}", Sha256::digest(events)),
            windows: 1,
            active_window_count: 0,
            active_window_range_count: 0,
            hit_tile_count: 0,
            component_count: 0,
            retained_component_count: 0,
            omitted_component_count: 0,
            narrow_contrast_method: None,
            narrow_search: None,
            events: Vec::new(),
            active_window_ranges: Vec::new(),
            wifi_activity_windows: None,
        };
        (segment, detection, directory, report_dir)
    }

    #[test]
    fn iq_release_preserves_clock_and_survey_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (segment, detection, directory, report_dir) = released_input(root.path());
        let sidecar = fs::read(directory.join(SIDECAR_NAME)).unwrap();
        let receipt = fs::read(directory.join(RECEIPT_NAME)).unwrap();
        let release = release_iq(
            &segment,
            &detection,
            ReleaseEvidence::default(),
            &report_dir,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        release.confirmation.unwrap();
        let proof = release.proof;
        proof.verify_after_release(&segment).unwrap();
        assert!(!directory.join(IQ_NAME).exists());
        assert_eq!(fs::read(directory.join(SIDECAR_NAME)).unwrap(), sidecar);
        assert_eq!(fs::read(directory.join(RECEIPT_NAME)).unwrap(), receipt);
        let (value, digest) =
            report::read_json_with_sha256(&report_dir.join("release.json")).unwrap();
        assert_eq!(value["index"], 0);
        assert_eq!(value["first_sample_ns"], -101);
        assert_eq!(value["witness_start_ns"], segment.receipt.witness_start_ns);
        assert_eq!(value["iq_sha256"], segment.receipt.iq_sha256);
        assert_eq!(value["sidecar_sha256"], segment.receipt.sidecar_sha256);
        assert_eq!(value["segment_receipt_sha256"], segment.receipt_sha256);
        assert_eq!(value["event_report_sha256"], detection.event_report_sha256);
        let terminal_proof = serde_json::to_value(&proof).unwrap();
        assert_eq!(terminal_proof["receipt_sha256"], digest);
        assert_eq!(terminal_proof["durability_confirmed"], true);

        fs::write(report_dir.join("survey/events.json"), b"{}").unwrap();
        assert!(proof.verify_after_release(&segment).is_err());
    }

    #[test]
    fn iq_release_keeps_raw_file_without_a_new_durable_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (segment, detection, directory, report_dir) = released_input(root.path());
        let stop = AtomicBool::new(false);
        let events_path = report_dir.join("survey/events.json");
        let events = fs::read(&events_path).unwrap();
        fs::write(&events_path, b"{}").unwrap();
        assert!(
            release_iq(
                &segment,
                &detection,
                ReleaseEvidence::default(),
                &report_dir,
                &stop
            )
            .is_err()
        );
        assert!(!report_dir.join("release.json").exists());
        assert!(directory.join(IQ_NAME).is_file());

        fs::write(&events_path, events).unwrap();
        fs::write(report_dir.join("release.json"), b"original").unwrap();
        assert!(
            release_iq(
                &segment,
                &detection,
                ReleaseEvidence::default(),
                &report_dir,
                &stop
            )
            .is_err()
        );
        assert_eq!(
            fs::read(report_dir.join("release.json")).unwrap(),
            b"original"
        );
        assert!(directory.join(IQ_NAME).is_file());
    }

    #[test]
    fn iq_release_rejects_a_replaced_raw_entry() {
        let root = tempfile::tempdir().unwrap();
        let (segment, detection, directory, report_dir) = released_input(root.path());
        let iq = directory.join(IQ_NAME);
        let exchanged = root.path().join("replacement");
        fs::copy(&iq, &exchanged).unwrap();
        fs::rename(exchanged, &iq).unwrap();
        assert!(
            release_iq(
                &segment,
                &detection,
                ReleaseEvidence::default(),
                &report_dir,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(iq.is_file());
        assert!(!report_dir.join("release.json").exists());
    }

    #[test]
    fn stopped_iq_release_preserves_raw_without_preparing_a_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (segment, detection, directory, report_dir) = released_input(root.path());
        assert!(
            release_iq(
                &segment,
                &detection,
                ReleaseEvidence::default(),
                &report_dir,
                &AtomicBool::new(true)
            )
            .unwrap()
            .is_none()
        );
        assert!(directory.join(IQ_NAME).is_file());
        assert!(!report_dir.join("release.json").exists());
        segment.verify_unchanged().unwrap();
    }
}
