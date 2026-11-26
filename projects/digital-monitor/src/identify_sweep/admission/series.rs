//! Bind an ordered sweep series to its independently readable pass receipts.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::Deserialize;

use super::{MAX_TUNES_PER_PASS, Pass, Sweep, direct_directory, verify_directory, verify_file};
use crate::report::read_typed_json_with_snapshot;

const MAX_SERIES_PASSES: usize = 16;
const MAX_SERIES_DWELLS: usize = 2_000;
const MAX_SERIES_RECEIPT_BYTES: u64 = 64 * 1024;
const SERIES_RECEIPT_NAME: &str = "sweep-series.json";

#[derive(Deserialize)]
struct SeriesReceipt {
    schema_version: u32,
    operation: String,
    iq_bytes: u64,
    dwells: usize,
    passes: Vec<PassRecord>,
    visit_centers_hz: Option<Vec<u64>>,
    #[serde(default)]
    check_continuity: bool,
}

#[derive(Deserialize)]
struct PassRecord {
    directory: String,
    sweep_sha256: String,
}

impl SeriesReceipt {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.operation == "rf_sweep_series",
            "unsupported completed sweep series receipt"
        );
        let pass_count = self.passes.len();
        ensure!(
            (2..=MAX_SERIES_PASSES).contains(&pass_count),
            "invalid sweep series pass count"
        );
        ensure!(
            (pass_count..=MAX_SERIES_DWELLS).contains(&self.dwells)
                && self.dwells.is_multiple_of(pass_count)
                && self.dwells / pass_count <= MAX_TUNES_PER_PASS,
            "invalid sweep series dwell count"
        );
        ensure!(
            self.iq_bytes > 0 && self.iq_bytes.is_multiple_of(pass_count as u64),
            "invalid sweep series IQ extent"
        );
        ensure!(
            self.visit_centers_hz
                .as_ref()
                .is_none_or(|visits| visits.len() == self.dwells / pass_count),
            "invalid sweep series visit count"
        );
        for (index, pass) in self.passes.iter().enumerate() {
            ensure!(
                pass.directory == format!("pass-{:04}", index + 1),
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

struct SeriesSource {
    root: PathBuf,
    root_stamp: Metadata,
    receipt_stamp: Metadata,
}

/// A single admitted sweep or the ordered passes of an admitted series.
pub(in crate::identify_sweep) struct Batch {
    pub(in crate::identify_sweep) passes: Vec<Pass>,
    pub(in crate::identify_sweep) sha256: String,
    series: Option<SeriesSource>,
}

impl Batch {
    /// Open the selected sweep or series and verify the manifest's pass bindings.
    ///
    /// # Errors
    /// Returns an error for missing, changed, indirect, or inconsistent receipts.
    pub(in crate::identify_sweep) fn open(directory: &Path, series: bool) -> Result<Self> {
        if !series {
            let sweep = Sweep::open(directory)?;
            return Ok(Self {
                sha256: sweep.sha256.clone(),
                passes: vec![Pass {
                    directory: None,
                    sweep,
                }],
                series: None,
            });
        }

        let root = directory
            .canonicalize()
            .context("resolve saved sweep series directory")?;
        let root_stamp = direct_directory(&root)?;
        let (receipt, sha256, receipt_stamp): (SeriesReceipt, _, _) =
            read_typed_json_with_snapshot(
                &root.join(SERIES_RECEIPT_NAME),
                MAX_SERIES_RECEIPT_BYTES,
            )?;
        receipt.validate()?;
        let mut passes: Vec<Pass> = Vec::with_capacity(receipt.passes.len());
        let mut dwells = 0;
        let mut iq_bytes = 0;
        for pass in receipt.passes {
            let sweep = Sweep::open_direct(root.join(&pass.directory))
                .with_context(|| format!("admit sweep series {}", pass.directory))?;
            ensure!(
                sweep.sha256 == pass.sweep_sha256,
                "sweep receipt SHA-256 differs from the series manifest for {}",
                pass.directory
            );
            ensure!(
                sweep.receipt.visit_centers_hz == receipt.visit_centers_hz
                    && sweep.receipt.check_continuity == receipt.check_continuity,
                "sweep visit order or continuity differs from the series manifest for {}",
                pass.directory
            );
            if let Some(first) = passes.first() {
                ensure!(
                    sweep.receipt.same_acquisition(&first.sweep.receipt),
                    "{} acquisition differs from the first sweep pass",
                    pass.directory
                );
            }
            dwells += sweep.receipt.tunes.len();
            iq_bytes += sweep.receipt.iq_bytes;
            passes.push(Pass {
                directory: Some(pass.directory),
                sweep,
            });
        }
        ensure!(
            dwells == receipt.dwells && iq_bytes == receipt.iq_bytes,
            "sweep series dwell count or IQ extent differs from its pass totals"
        );
        let batch = Self {
            passes,
            sha256,
            series: Some(SeriesSource {
                root,
                root_stamp,
                receipt_stamp,
            }),
        };
        batch.verify()?;
        Ok(batch)
    }

    /// Recheck the admitted directory and receipt identities.
    ///
    /// # Errors
    /// Returns an error if an admitted path disappeared or changed.
    pub(in crate::identify_sweep) fn verify(&self) -> Result<()> {
        if let Some(series) = &self.series {
            verify_directory(&series.root, &series.root_stamp)?;
            verify_file(
                &series.root.join(SERIES_RECEIPT_NAME),
                &series.receipt_stamp,
            )?;
        }
        for pass in &self.passes {
            pass.sweep.verify()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::identify_sweep::admission::{SAMPLE_RATE_HZ, SWEEP_RECEIPT_NAME};

    fn write_series(root: &Path, durations_ms: [[u64; 2]; 2]) -> Result<()> {
        let center = 2_412_000_000;
        let mut passes = Vec::new();
        let mut series_bytes = 0;
        for (pass, durations_ms) in durations_ms.into_iter().enumerate() {
            let name = format!("pass-{:04}", pass + 1);
            let directory = root.join(&name);
            fs::create_dir(&directory)?;
            let observed = center + pass as u64;
            let tunes: Vec<_> = durations_ms
                .into_iter()
                .enumerate()
                .map(|(tune, dwell_ms)| {
                    json!({
                        "directory": format!("tune-{:04}", tune + 1),
                        "requested_center_hz": center,
                        "observed_center_hz": observed,
                        "dwell_ms": dwell_ms,
                        "observed_bands": [
                            {"lower_hz": observed - 8_000_000, "upper_hz": observed - 100_000},
                            {"lower_hz": observed + 100_000, "upper_hz": observed + 8_000_000}
                        ]
                    })
                })
                .collect();
            let pass_bytes = durations_ms.iter().sum::<u64>() * (SAMPLE_RATE_HZ / 1_000) * 4;
            series_bytes += pass_bytes;
            let bytes = serde_json::to_vec(&json!({
                "schema_version": 1,
                "operation": "targeted_rf_sweep",
                "lower_hz": center,
                "upper_hz": center + 1,
                "dwell_ms": 1,
                "iq_bytes": pass_bytes,
                "visit_centers_hz": [center, center],
                "tunes": tunes,
            }))?;
            fs::write(directory.join(SWEEP_RECEIPT_NAME), &bytes)?;
            passes.push(json!({
                "directory": name,
                "sweep_sha256": format!("{:x}", Sha256::digest(bytes)),
            }));
        }
        let bytes = serde_json::to_vec(&json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "iq_bytes": series_bytes,
            "dwells": 4,
            "visit_centers_hz": [center, center],
            "passes": passes,
        }))?;
        fs::write(root.join(SERIES_RECEIPT_NAME), bytes)?;
        Ok(())
    }

    #[test]
    fn series_preserves_ordered_repeat_visits_and_rechecks_its_passes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        write_series(root, [[1, 2], [1, 2]])?;

        let batch = Batch::open(root, true)?;
        assert_eq!(batch.passes.len(), 2);
        assert_eq!(batch.passes[1].directory.as_deref(), Some("pass-0002"));
        assert_eq!(batch.passes[1].sweep.receipt.samples_at(0)?, 20_000);
        assert_eq!(batch.passes[1].sweep.receipt.samples_at(1)?, 40_000);
        assert_eq!(
            batch.sha256,
            format!(
                "{:x}",
                Sha256::digest(fs::read(root.join(SERIES_RECEIPT_NAME))?)
            )
        );
        batch.verify()?;

        let path = root.join("pass-0002").join(SWEEP_RECEIPT_NAME);
        let mut bytes = fs::read(&path)?;
        bytes.push(b'\n');
        fs::write(path, bytes)?;
        assert!(batch.verify().is_err());
        let error = Batch::open(root, true).err().unwrap();
        assert!(error.to_string().contains("SHA-256 differs"), "{error:#}");
        Ok(())
    }

    #[test]
    fn series_rejects_swapped_durations_with_unchanged_byte_totals() -> Result<()> {
        let directory = tempfile::tempdir()?;
        write_series(directory.path(), [[1, 2], [2, 1]])?;
        let error = Batch::open(directory.path(), true).err().unwrap();
        assert!(
            error.to_string().contains("acquisition differs"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn series_rejects_a_pass_directory_redirect() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        write_series(root, [[1, 2], [1, 2]])?;
        let batch = Batch::open(root, true)?;
        let path = root.join("pass-0002");
        let relocated = root.join("relocated");
        fs::rename(&path, &relocated)?;
        std::os::unix::fs::symlink(&relocated, path)?;
        assert!(batch.verify().is_err());
        let error = Batch::open(root, true).err().unwrap();
        assert!(format!("{error:#}").contains("must be a direct directory"));
        Ok(())
    }
}
