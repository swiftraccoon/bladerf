//! Verify the terminal series manifest and its independently replayable passes.

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::sweep::{pass_directory_name, plan};

use super::{
    CaptureCadence, ExpectedPass, SeriesSource, Sweep, descriptor_path, open_directory,
    read_bounded, same_inode, verify_path,
};

const SERIES_RECEIPT: &str = "sweep-series.json";
const MAX_SERIES_RECEIPT_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
struct Receipt {
    schema_version: u32,
    operation: String,
    dwells: usize,
    iq_bytes: u64,
    #[serde(default)]
    check_continuity: bool,
    capture_cadence: Option<CaptureCadence>,
    passes: Vec<PassRecord>,
    source_series_sha256: Option<String>,
    source_series_triage_sha256: Option<String>,
    source_series_triage_overlap_sha256: Option<String>,
    source_series_activity_sha256: Option<String>,
    visit_centers_hz: Option<Vec<u64>>,
}

#[derive(Deserialize)]
struct PassRecord {
    directory: String,
    sweep_sha256: String,
}

impl Receipt {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.operation == "rf_sweep_series",
            "unsupported sweep series receipt contract"
        );
        let passes = self.passes.len();
        ensure!(
            (2..=plan::MAX_PASSES).contains(&passes),
            "invalid sweep series pass count"
        );
        ensure!(
            (passes..=plan::MAX_TOTAL_DWELLS).contains(&self.dwells)
                && self.dwells.is_multiple_of(passes)
                && self.dwells / passes <= plan::MAX_TUNES,
            "invalid sweep series dwell count"
        );
        ensure!(
            self.iq_bytes > 0 && self.iq_bytes.is_multiple_of(passes as u64),
            "invalid sweep series IQ extent"
        );
        ensure!(
            self.visit_centers_hz
                .as_ref()
                .is_none_or(|visits| visits.len() == self.dwells / passes),
            "invalid sweep series visit count"
        );
        for (index, pass) in self.passes.iter().enumerate() {
            ensure!(
                pass.directory == pass_directory_name(index),
                "unexpected sweep pass directory at index {index}"
            );
            ensure!(
                pass.sweep_sha256.len() == 64
                    && HEXLOWER.decode(pass.sweep_sha256.as_bytes()).is_ok(),
                "invalid sweep receipt SHA-256 for {}",
                pass.directory
            );
        }
        Ok(())
    }
}

/// A verified manifest and its ordered, independently admitted sweep passes.
#[derive(Debug)]
pub(in crate::triage) struct Series {
    pub(in crate::triage) source_series_sha256: String,
    pub(in crate::triage) passes: Vec<Pass>,
    pub(in crate::triage) series_source: SeriesSource,
    pub(in crate::triage) timing_sha256: Option<String>,
    pub(super) check_continuity: bool,
    root: PathBuf,
    root_stamp: Metadata,
    receipt_stamp: Metadata,
}

#[derive(Debug)]
pub(in crate::triage) struct Pass {
    pub(in crate::triage) directory: String,
    pub(in crate::triage) sweep: Sweep,
}

impl Series {
    /// Admit a complete series and check all passes use the same acquisition plan.
    ///
    /// # Errors
    /// Returns an error for missing, indirect, changed, or inconsistent inputs.
    pub(in crate::triage) fn open(directory: &Path) -> Result<Self> {
        let root = directory.canonicalize().with_context(|| {
            format!(
                "resolve saved sweep series directory {}",
                directory.display()
            )
        })?;
        let pinned_root = open_directory(&root)?;
        let root_stamp = pinned_root.metadata()?;
        let root_fd = descriptor_path(&pinned_root);
        let (bytes, receipt_stamp) =
            read_bounded(&root_fd.join(SERIES_RECEIPT), MAX_SERIES_RECEIPT_BYTES)
                .context("read completed sweep series receipt")?;
        let receipt: Receipt =
            serde_json::from_slice(&bytes).context("parse completed sweep series receipt")?;
        receipt.validate()?;

        let source_series_sha256 = format!("{:x}", Sha256::digest(bytes));
        let pass_count = receipt.passes.len();
        let tunes_per_pass = receipt.dwells / pass_count;
        let bytes_per_pass = receipt.iq_bytes / pass_count as u64;
        let mut passes: Vec<Pass> = Vec::with_capacity(pass_count);
        for pass in receipt.passes {
            let sweep = open_directory(&root_fd.join(&pass.directory))
                .and_then(|directory| {
                    Sweep::open_pinned(
                        root.join(&pass.directory),
                        &directory,
                        Some(ExpectedPass {
                            sha256: &pass.sweep_sha256,
                            tunes: tunes_per_pass,
                            iq_bytes: bytes_per_pass,
                            check_continuity: receipt.check_continuity,
                            visit_centers_hz: receipt.visit_centers_hz.as_deref(),
                        }),
                    )
                })
                .with_context(|| format!("admit sweep series {}", pass.directory))?;
            if let Some(first) = passes.first() {
                ensure!(
                    same_acquisition(&first.sweep, &sweep),
                    "{} acquisition differs from the first sweep pass",
                    pass.directory
                );
            }
            passes.push(Pass {
                directory: pass.directory,
                sweep,
            });
        }
        let series = Self {
            source_series_sha256,
            passes,
            series_source: SeriesSource {
                source_series_sha256: receipt.source_series_sha256,
                source_series_triage_sha256: receipt.source_series_triage_sha256,
                source_series_triage_overlap_sha256: receipt.source_series_triage_overlap_sha256,
                source_series_activity_sha256: receipt.source_series_activity_sha256,
            },
            timing_sha256: receipt
                .capture_cadence
                .and_then(CaptureCadence::into_timing_sha256),
            check_continuity: receipt.check_continuity,
            root,
            root_stamp,
            receipt_stamp,
        };
        series.verify()?;
        Ok(series)
    }

    /// Recheck the manifest and the pinned source identities of every pass.
    ///
    /// # Errors
    /// Returns an error if an admitted path can no longer be verified.
    pub(in crate::triage) fn verify(&self) -> Result<()> {
        let root = open_directory(&self.root)?;
        self.verify_pinned(&root)
    }

    pub(in crate::triage) fn verify_pinned(&self, root: &File) -> Result<()> {
        ensure!(
            same_inode(&self.root_stamp, &root.metadata()?),
            "sweep series directory changed after admission: {}",
            self.root.display()
        );
        let root_fd = descriptor_path(root);
        verify_path(&root_fd.join(SERIES_RECEIPT), &self.receipt_stamp)
            .context("verify completed sweep series receipt")?;
        for pass in &self.passes {
            open_directory(&root_fd.join(&pass.directory))
                .and_then(|directory| pass.sweep.verify_pinned(&directory))
                .with_context(|| format!("verify sweep series {}", pass.directory))?;
        }
        Ok(())
    }
}

fn same_acquisition(a: &Sweep, b: &Sweep) -> bool {
    a.operation == b.operation
        && a.requested_range == b.requested_range
        && a.requested_bands == b.requested_bands
        && a.requested_band_dwells == b.requested_band_dwells
        && (a.requested_bands.is_none() || a.dwell_ms == b.dwell_ms)
        && a.antenna == b.antenna
        && a.gain_db == b.gain_db
        && a.tunes
            .iter()
            .map(|tune| (tune.requested_center_hz, tune.samples))
            .eq(b
                .tunes
                .iter()
                .map(|tune| (tune.requested_center_hz, tune.samples)))
}
