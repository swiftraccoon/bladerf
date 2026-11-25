//! Preserve survey evidence before releasing the raw IQ from an admitted segment.

use std::fs::{self, File, Metadata, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use nix::unistd::{UnlinkatFlags, unlinkat};
use serde::Serialize;

use crate::orchestrate::ieee_nominal::NominalReport;
use crate::report;

use super::burst::{self, BurstReport};
use super::clips::ClipReport;
use super::detection::Detection;
use super::direct::DirectReport;
use super::intake::{IQ_NAME, ValidatedSegment};
use super::overlap::TailProof;

const RECEIPT_NAME: &str = "release.json";
const RECEIPT_BYTES: u64 = 4096;
const EVENTS_NAME: &str = "events.json";
const EVENT_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    operation: &'static str,
    index: usize,
    start_sample: u64,
    num_samples: u64,
    first_sample_ns: i64,
    end_sample_exclusive_ns: i64,
    witness_start_ns: i64,
    iq_sha256: &'a str,
    sidecar_sha256: &'a str,
    segment_receipt_sha256: &'a str,
    event_report_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_receipt_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ieee_nominal_receipt_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overlap_tail_receipt_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_clips_receipt_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    band_burst_report_sha256: Option<&'a str>,
}

#[derive(Default)]
pub(super) struct ReleaseEvidence<'a> {
    pub(super) direct: Option<&'a DirectReport>,
    pub(super) ieee_nominal: Option<&'a NominalReport>,
    pub(super) overlap_tail: Option<&'a TailProof>,
    pub(super) event_clips: Option<&'a ClipReport>,
    pub(super) band_bursts: Option<&'a BurstReport>,
}

#[derive(Serialize)]
pub(super) struct ReleaseProof {
    receipt_sha256: String,
    durability_confirmed: bool,
    #[serde(skip)]
    event_report_sha256: String,
    #[serde(skip)]
    report_dir: PathBuf,
    #[serde(skip)]
    report_files: [Metadata; 2],
    #[serde(skip)]
    band_burst_report: Option<(String, Metadata)>,
    #[serde(skip)]
    released_directory: Option<Metadata>,
}

/// The unlink succeeded. Its confirmation may still fail and must be reported separately.
pub(super) struct ReleasedIq {
    pub(super) proof: ReleaseProof,
    pub(super) confirmation: Result<()>,
}

impl ReleaseProof {
    /// Verify the retained acquisition and analysis files and durable release receipt.
    pub(super) fn verify_after_release(&self, segment: &ValidatedSegment) -> Result<()> {
        let directory = self
            .released_directory
            .as_ref()
            .context("rolling IQ release has no post-unlink directory snapshot")?;
        segment.verify_after_release(directory)?;
        let report_dir = open_directory(&self.report_dir)?;
        let survey_dir = open_survey_directory(&report_dir)?;
        let (receipt, receipt_stamp) = report::hash_direct_regular_file_with_snapshot(
            &fd_path(&report_dir).join(RECEIPT_NAME),
            RECEIPT_BYTES,
        )?;
        let (events, event_stamp) = report::hash_direct_regular_file_with_snapshot(
            &fd_path(&survey_dir).join(EVENTS_NAME),
            EVENT_BYTES,
        )?;
        ensure!(
            receipt.sha256 == self.receipt_sha256
                && events.sha256 == self.event_report_sha256
                && report::same_file_identity(&receipt_stamp, &self.report_files[0])
                && report::same_file_identity(&event_stamp, &self.report_files[1]),
            "rolling release receipt or survey report changed after IQ release"
        );
        if let Some((expected_sha256, before)) = &self.band_burst_report {
            let (band_burst, stamp) = report::hash_direct_regular_file_with_snapshot(
                &fd_path(&report_dir).join(burst::REPORT_NAME),
                burst::REPORT_BYTES,
            )?;
            ensure!(
                band_burst.sha256 == *expected_sha256 && report::same_file_identity(before, &stamp),
                "rolling band-burst report changed after IQ release"
            );
        }
        verify_directory_path(&self.report_dir, &report_dir)?;
        verify_directory_path(&self.report_dir.join("survey"), &survey_dir)
    }
}

/// Sync the evidence and remove this segment's IQ. Callers reserve decoder inputs.
/// A stop observed before unlink leaves IQ intact and may leave the intent receipt.
pub(super) fn release_iq(
    segment: &ValidatedSegment,
    detection: &Detection,
    evidence: ReleaseEvidence<'_>,
    report_dir: &Path,
    stop: &AtomicBool,
) -> Result<Option<ReleasedIq>> {
    if stop.load(Ordering::Relaxed) {
        return Ok(None);
    }
    if let Some(direct) = evidence.direct {
        direct
            .verify()
            .context("verify retained direct decode before IQ release")?;
    }
    if let Some(ieee_nominal) = evidence.ieee_nominal {
        ieee_nominal
            .verify()
            .context("verify retained IEEE 802.15.4 decode before IQ release")?;
    }
    if let Some(tail) = evidence.overlap_tail {
        tail.verify(report_dir)
            .context("verify retained overlap tail before IQ release")?;
    }
    if let Some(clips) = evidence.event_clips {
        clips
            .verify()
            .context("verify retained survey clips before IQ release")?;
    }
    let original = &segment.receipt;
    let ordinal = format!("{:06}", original.index + 1);
    ensure!(
        report_dir
            .file_name()
            .is_some_and(|name| name == ordinal.as_str()),
        "rolling report directory does not match the admitted segment ordinal"
    );
    let (segment_directory, iq_stamp) = segment.open_for_release()?;
    let parent = report_dir
        .parent()
        .context("rolling report directory has no parent")?;
    let segments_dir = open_directory(parent)?;
    let reports = open_directory(report_dir)?;
    let pinned_reports = fd_path(&reports);
    let survey = open_survey_directory(&reports)?;
    let event_path = fd_path(&survey).join(EVENTS_NAME);
    let event_file = open_file_at(&survey, EVENTS_NAME, OFlag::O_RDONLY | OFlag::O_NONBLOCK)?;
    let (events, event_stamp) =
        report::hash_direct_regular_file_with_snapshot(&event_path, EVENT_BYTES)?;
    ensure!(
        events.sha256 == detection.event_report_sha256
            && report::same_file_identity(&event_file.metadata()?, &event_stamp),
        "rolling survey report changed before IQ release"
    );
    event_file
        .sync_all()
        .context("sync rolling survey event report")?;
    survey.sync_all().context("sync rolling survey directory")?;

    let band_burst_path = pinned_reports.join(burst::REPORT_NAME);
    let band_burst_report = if let Some(band_bursts) = evidence.band_bursts {
        let file = open_file_at(
            &reports,
            burst::REPORT_NAME,
            OFlag::O_RDONLY | OFlag::O_NONBLOCK,
        )?;
        let (digest, stamp) =
            report::hash_direct_regular_file_with_snapshot(&band_burst_path, burst::REPORT_BYTES)?;
        band_bursts.verify_stamp(&stamp)?;
        ensure!(
            digest.sha256 == band_bursts.sha256()
                && report::same_file_identity(&file.metadata()?, &stamp),
            "rolling band-burst report changed before IQ release"
        );
        file.sync_all().context("sync rolling band-burst report")?;
        Some((digest.sha256, stamp))
    } else {
        None
    };

    if stop.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let receipt = Receipt {
        schema_version: 1,
        operation: "rolling_survey_iq_release_intent",
        index: original.index,
        start_sample: original.start_sample,
        num_samples: original.num_samples,
        first_sample_ns: original.first_sample_ns,
        end_sample_exclusive_ns: original.end_sample_exclusive_ns,
        witness_start_ns: original.witness_start_ns,
        iq_sha256: &original.iq_sha256,
        sidecar_sha256: &original.sidecar_sha256,
        segment_receipt_sha256: &segment.receipt_sha256,
        event_report_sha256: &detection.event_report_sha256,
        direct_receipt_sha256: evidence.direct.map(DirectReport::receipt_sha256),
        ieee_nominal_receipt_sha256: evidence.ieee_nominal.map(NominalReport::receipt_sha256),
        overlap_tail_receipt_sha256: evidence.overlap_tail.map(TailProof::receipt_sha256),
        event_clips_receipt_sha256: evidence.event_clips.map(ClipReport::receipt_sha256),
        band_burst_report_sha256: band_burst_report
            .as_ref()
            .map(|(sha256, _)| sha256.as_str()),
    };
    report::publish_json(&pinned_reports, RECEIPT_NAME, &receipt)
        .context("publish rolling IQ release receipt")?;
    segments_dir
        .sync_all()
        .context("sync rolling segment reports parent")?;
    let (published, receipt_sha256, receipt_stamp) =
        report::read_json_with_snapshot(&pinned_reports.join(RECEIPT_NAME), RECEIPT_BYTES)?;
    ensure!(
        published == serde_json::to_value(&receipt)?
            && report::same_file_identity(&event_stamp, &fs::symlink_metadata(event_path)?),
        "rolling release receipt or survey report changed before IQ release"
    );
    verify_directory_path(parent, &segments_dir)?;
    verify_directory_path(report_dir, &reports)?;
    verify_directory_path(&report_dir.join("survey"), &survey)?;

    segment.verify_unchanged()?;
    if let Some((_, stamp)) = &band_burst_report {
        ensure!(
            report::same_file_identity(stamp, &fs::symlink_metadata(band_burst_path)?),
            "rolling band-burst report changed before IQ release"
        );
        verify_directory_path(report_dir, &reports)?;
    }
    if !remove_verified_iq(&segment_directory, iq_stamp, stop)? {
        return Ok(None);
    }
    let mut proof = ReleaseProof {
        receipt_sha256,
        durability_confirmed: false,
        event_report_sha256: detection.event_report_sha256.clone(),
        report_dir: report_dir.to_path_buf(),
        report_files: [receipt_stamp, event_stamp],
        band_burst_report,
        released_directory: None,
    };
    let synced = segment_directory
        .sync_all()
        .context("sync released rolling IQ directory");
    let verified = segment_directory
        .metadata()
        .context("snapshot released rolling IQ directory")
        .and_then(|stamp| {
            let result = segment.verify_after_release(&stamp);
            proof.released_directory = Some(stamp);
            result
        });
    let confirmation = synced.and(verified);
    proof.durability_confirmed = confirmation.is_ok();
    Ok(Some(ReleasedIq {
        proof,
        confirmation,
    }))
}

fn open_directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("pin rolling report directory {}", path.display()))
}

fn open_file_at(directory: &File, name: &str, flags: OFlag) -> Result<File> {
    openat(
        directory,
        name,
        flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .with_context(|| format!("open rolling entry {name} without following links"))
}

fn open_survey_directory(report_dir: &File) -> Result<File> {
    open_file_at(report_dir, "survey", OFlag::O_RDONLY | OFlag::O_DIRECTORY)
}

fn fd_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn verify_directory_path(path: &Path, directory: &File) -> Result<()> {
    let opened = directory.metadata()?;
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.is_dir() && current.dev() == opened.dev() && current.ino() == opened.ino(),
        "rolling report directory was replaced: {}",
        path.display()
    );
    Ok(())
}

fn remove_verified_iq(directory: &File, before: &Metadata, stop: &AtomicBool) -> Result<bool> {
    let raw = open_file_at(directory, IQ_NAME, OFlag::O_PATH)?;
    let current = raw.metadata()?;
    ensure!(
        current.is_file() && current.nlink() == 1 && report::same_file_identity(before, &current),
        "rolling IQ changed or acquired a hard link before release"
    );
    drop(raw);
    if stop.load(Ordering::Relaxed) {
        return Ok(false);
    }
    // The producer never rewrites a published ordinal in the private run directory.
    unlinkat(directory, IQ_NAME, UnlinkatFlags::NoRemoveDir)
        .context("unlink verified rolling IQ")?;
    Ok(true)
}
