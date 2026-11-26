//! Validate completed sweep receipts and the direct files selected for replay.

mod series;

use std::fs::{self, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::input::{self, SourceReport};
use crate::report::{read_typed_json_with_snapshot, same_file_identity};

pub(super) use self::series::Batch;

pub(super) const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MIN_CENTER_HZ: u64 = 70_000_000;
const MAX_CENTER_HZ: u64 = 6_000_000_000;
const MAX_DWELL_MS: u64 = 10_000;
const MAX_TUNES_PER_PASS: usize = 500;
const MAX_SWEEP_RECEIPT_BYTES: u64 = 64 * 1024 * 1024;
const SWEEP_RECEIPT_NAME: &str = "sweep.json";
const IQ_NAME: &str = "wideband.sc16q11";
const SIDECAR_NAME: &str = "wideband.sc16q11.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
struct Band {
    lower_hz: u64,
    upper_hz: u64,
}

#[derive(Deserialize)]
pub(super) struct Tune {
    pub(super) directory: String,
    pub(super) requested_center_hz: u64,
    pub(super) observed_center_hz: u64,
    observed_bands: [Band; 2],
    dwell_ms: Option<u64>,
}

impl Tune {
    fn validate(&self, index: usize) -> Result<()> {
        ensure!(
            self.directory == format!("tune-{:04}", index + 1),
            "unexpected sweep tune directory at index {index}"
        );
        ensure!(
            (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&self.requested_center_hz)
                && self.observed_center_hz.abs_diff(self.requested_center_hz) <= 10,
            "invalid requested or observed center at {}",
            self.directory
        );
        let center = self.observed_center_hz;
        let expected = [
            Band {
                lower_hz: center - 8_000_000,
                upper_hz: center - 100_000,
            },
            Band {
                lower_hz: center + 100_000,
                upper_hz: center + 8_000_000,
            },
        ];
        ensure!(
            self.observed_bands == expected,
            "invalid observed band geometry at {}",
            self.directory
        );
        Ok(())
    }

    fn samples(&self, default_dwell_ms: u64) -> Result<u64> {
        let dwell_ms = self.dwell_ms.unwrap_or(default_dwell_ms);
        ensure!(
            (1..=MAX_DWELL_MS).contains(&dwell_ms),
            "invalid sweep dwell duration at {}",
            self.directory
        );
        Ok(dwell_ms * (SAMPLE_RATE_HZ / 1_000))
    }
}

#[derive(Deserialize)]
pub(super) struct Receipt {
    schema_version: u32,
    operation: String,
    pub(super) lower_hz: u64,
    pub(super) upper_hz: u64,
    dwell_ms: u64,
    iq_bytes: u64,
    visit_centers_hz: Option<Vec<u64>>,
    #[serde(default)]
    check_continuity: bool,
    pub(super) tunes: Vec<Tune>,
}

impl Receipt {
    fn validate(&self) -> Result<()> {
        let targeted = self.is_targeted();
        ensure!(
            self.schema_version == 1 && (targeted || self.operation == "sequential_rf_sweep"),
            "unsupported completed sweep receipt"
        );
        let maximum_upper_hz = MAX_CENTER_HZ + u64::from(targeted);
        ensure!(
            (MIN_CENTER_HZ..self.upper_hz).contains(&self.lower_hz)
                && self.upper_hz <= maximum_upper_hz,
            "invalid requested sweep bounds"
        );
        ensure!(
            (1..=MAX_TUNES_PER_PASS).contains(&self.tunes.len())
                && (1..=MAX_DWELL_MS).contains(&self.dwell_ms),
            "invalid sweep tune count or dwell duration"
        );
        if let Some(visits) = &self.visit_centers_hz {
            ensure!(
                self.tunes
                    .iter()
                    .map(|tune| tune.requested_center_hz)
                    .eq(visits.iter().copied()),
                "sweep visit order differs from the requested tune centers"
            );
        } else {
            ensure!(
                self.tunes.is_sorted_by(
                    |left, right| left.requested_center_hz < right.requested_center_hz
                ),
                "sweep requested centers must be strictly increasing"
            );
        }
        let mut iq_bytes = 0;
        for (index, tune) in self.tunes.iter().enumerate() {
            tune.validate(index)?;
            iq_bytes += tune.samples(self.dwell_ms)? * 4;
        }
        ensure!(
            self.iq_bytes == iq_bytes,
            "sweep IQ bytes differ from the sum of its tune extents"
        );
        if targeted {
            let centers = self.tunes.iter().map(|tune| tune.requested_center_hz);
            ensure!(
                centers.clone().min() == Some(self.lower_hz)
                    && centers.max().map(|center| center + 1) == Some(self.upper_hz),
                "targeted sweep bounds differ from the requested centers"
            );
            return Ok(());
        }
        let mut bands: Vec<_> = self
            .tunes
            .iter()
            .flat_map(|tune| tune.observed_bands)
            .collect();
        bands.sort_unstable_by_key(|band| band.lower_hz);
        let mut covered = self.lower_hz;
        for band in bands {
            if band.lower_hz > covered {
                break;
            }
            covered = covered.max(band.upper_hz);
        }
        ensure!(
            covered >= self.upper_hz,
            "completed sweep receipt leaves a coverage gap"
        );
        Ok(())
    }

    pub(super) fn is_targeted(&self) -> bool {
        self.operation == "targeted_rf_sweep"
    }

    /// Return the complete sample extent declared for one validated visit.
    ///
    /// # Errors
    /// Returns an error for an unknown tune index or an invalid dwell duration.
    pub(super) fn samples_at(&self, index: usize) -> Result<u64> {
        self.tunes
            .get(index)
            .context("unknown sweep tune index")?
            .samples(self.dwell_ms)
    }

    /// Check the requested scope of a validated receipt.
    pub(super) fn includes_channel(&self, frequency_hz: u64) -> bool {
        if self.is_targeted() {
            self.tunes
                .iter()
                .any(|tune| tune.requested_center_hz == frequency_hz)
        } else {
            (self.lower_hz..self.upper_hz).contains(&frequency_hz)
        }
    }

    fn same_acquisition(&self, other: &Self) -> bool {
        self.operation == other.operation
            && self.lower_hz == other.lower_hz
            && self.upper_hz == other.upper_hz
            && self
                .tunes
                .iter()
                .map(|tune| {
                    (
                        tune.requested_center_hz,
                        tune.dwell_ms.unwrap_or(self.dwell_ms),
                    )
                })
                .eq(other.tunes.iter().map(|tune| {
                    (
                        tune.requested_center_hz,
                        tune.dwell_ms.unwrap_or(other.dwell_ms),
                    )
                }))
    }
}

pub(super) struct Sweep {
    root: PathBuf,
    pub(super) receipt: Receipt,
    pub(super) sha256: String,
    root_stamp: Metadata,
    stamp: Metadata,
}

impl Sweep {
    pub(super) fn open(directory: &Path) -> Result<Self> {
        let root = directory
            .canonicalize()
            .context("resolve saved sweep directory")?;
        Self::open_direct(root)
    }

    fn open_direct(root: PathBuf) -> Result<Self> {
        let root_stamp = direct_directory(&root)?;
        let (receipt, sha256, stamp): (Receipt, _, _) =
            read_typed_json_with_snapshot(&root.join(SWEEP_RECEIPT_NAME), MAX_SWEEP_RECEIPT_BYTES)?;
        receipt.validate()?;
        let sweep = Self {
            root,
            receipt,
            sha256,
            root_stamp,
            stamp,
        };
        sweep.verify()?;
        Ok(sweep)
    }

    pub(super) fn verify(&self) -> Result<()> {
        verify_directory(&self.root, &self.root_stamp)?;
        verify_file(&self.root.join(SWEEP_RECEIPT_NAME), &self.stamp)
    }

    pub(super) fn admit(&self, index: usize, sample_limit: u64) -> Result<Capture> {
        let expected_samples = self.receipt.samples_at(index)?;
        let tune = &self.receipt.tunes[index];
        let directory = self.root.join(&tune.directory);
        let directory_stamp = direct_directory(&directory)?;
        let iq = directory.join(IQ_NAME);
        let sidecar = directory.join(SIDECAR_NAME);
        let iq_stamp = direct_file(&iq)?;
        let sidecar_stamp = direct_file(&sidecar)?;
        let source = input::validate(&iq, tune.observed_center_hz, SAMPLE_RATE_HZ, sample_limit)
            .with_context(|| format!("validate sweep {}", tune.directory))?;
        ensure!(
            source.complex_samples == expected_samples,
            "sweep tune sample extent differs from its receipt"
        );
        ensure!(
            source.input == iq && source.sidecar == sidecar,
            "sweep tune redirected during validation"
        );
        let capture = Capture {
            source,
            directory,
            directory_stamp,
            iq_stamp,
            sidecar_stamp,
        };
        capture.verify()?;
        Ok(capture)
    }
}

pub(super) struct Pass {
    pub(super) directory: Option<String>,
    pub(super) sweep: Sweep,
}

pub(super) struct Capture {
    pub(super) source: SourceReport,
    directory: PathBuf,
    directory_stamp: Metadata,
    iq_stamp: Metadata,
    sidecar_stamp: Metadata,
}

impl Capture {
    pub(super) fn verify(&self) -> Result<()> {
        let directory = fs::symlink_metadata(&self.directory)?;
        ensure!(
            directory.is_dir() && same_file_identity(&self.directory_stamp, &directory),
            "sweep tune directory changed after admission"
        );
        verify_file(&self.source.input, &self.iq_stamp)?;
        verify_file(&self.source.sidecar, &self.sidecar_stamp)
    }
}

fn direct_directory(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect sweep directory {}", path.display()))?;
    ensure!(
        metadata.is_dir(),
        "sweep path must be a direct directory: {}",
        path.display()
    );
    Ok(metadata)
}

fn verify_directory(path: &Path, expected: &Metadata) -> Result<()> {
    let actual = direct_directory(path)?;
    ensure!(
        actual.dev() == expected.dev() && actual.ino() == expected.ino(),
        "sweep directory changed: {}",
        path.display()
    );
    Ok(())
}

fn direct_file(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect sweep file {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "sweep file must be a direct regular file: {}",
        path.display()
    );
    Ok(metadata)
}

fn verify_file(path: &Path, expected: &Metadata) -> Result<()> {
    ensure!(
        same_file_identity(&direct_file(path)?, expected),
        "sweep file changed: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(operation: &str, lower_hz: u64, upper_hz: u64, centers: &[(u64, u64)]) -> Receipt {
        let tunes = centers
            .iter()
            .enumerate()
            .map(|(index, &(requested_center_hz, observed_center_hz))| Tune {
                directory: format!("tune-{:04}", index + 1),
                requested_center_hz,
                observed_center_hz,
                dwell_ms: None,
                observed_bands: [
                    Band {
                        lower_hz: observed_center_hz - 8_000_000,
                        upper_hz: observed_center_hz - 100_000,
                    },
                    Band {
                        lower_hz: observed_center_hz + 100_000,
                        upper_hz: observed_center_hz + 8_000_000,
                    },
                ],
            })
            .collect();
        Receipt {
            schema_version: 1,
            operation: operation.to_owned(),
            lower_hz,
            upper_hz,
            dwell_ms: 10,
            iq_bytes: 10 * (SAMPLE_RATE_HZ / 1_000) * 4 * centers.len() as u64,
            visit_centers_hz: None,
            check_continuity: false,
            tunes,
        }
    }

    #[test]
    fn scheduled_receipt_binds_repeated_centers_in_tune_order() {
        let high = 927_000_000;
        let low = 902_000_000;
        let mut sweep = receipt(
            "targeted_rf_sweep",
            low,
            high + 1,
            &[(high, high), (low, low), (high, high)],
        );
        assert!(sweep.validate().is_err());
        sweep.visit_centers_hz = Some(vec![high, low, high]);
        sweep.validate().unwrap();
        assert!(sweep.includes_channel(low));
        assert!(sweep.includes_channel(high));
        assert!(!sweep.includes_channel(low + 1));
        for visits in [vec![low, high, high], vec![high, low]] {
            sweep.visit_centers_hz = Some(visits);
            assert!(sweep.validate().is_err());
        }
    }

    #[test]
    fn targeted_receipt_admits_only_requested_centers_across_gaps() {
        let centers = [(902_000_000, 902_000_010), (927_000_000, 926_999_990)];
        let mut sparse = receipt("targeted_rf_sweep", 902_000_000, 927_000_001, &centers);
        sparse.validate().unwrap();
        assert!(sparse.is_targeted());
        assert!(sparse.includes_channel(902_000_000));
        assert!(sparse.includes_channel(927_000_000));
        assert!(!sparse.includes_channel(902_000_010));
        assert!(!sparse.includes_channel(914_000_000));
        assert!(!sparse.includes_channel(927_000_001));

        sparse.operation = "sequential_rf_sweep".to_owned();
        assert!(
            sparse
                .validate()
                .unwrap_err()
                .to_string()
                .contains("coverage gap")
        );
    }

    #[test]
    fn sequential_receipt_uses_a_half_open_requested_range() {
        let mut continuous = receipt(
            "sequential_rf_sweep",
            900_000_000,
            906_000_000,
            &[(907_000_000, 907_000_000)],
        );
        continuous.validate().unwrap();
        assert!(!continuous.is_targeted());
        assert!(continuous.includes_channel(900_000_000));
        assert!(continuous.includes_channel(905_999_999));
        assert!(!continuous.includes_channel(906_000_000));
        continuous.operation = "targeted_rf_sweep".to_owned();
        assert!(continuous.validate().is_err());
    }

    #[test]
    fn targeted_boundary_has_an_exclusive_upper_hertz_beyond_the_rf_limit() {
        let mut edge = receipt(
            "targeted_rf_sweep",
            MAX_CENTER_HZ,
            MAX_CENTER_HZ + 1,
            &[(MAX_CENTER_HZ, MAX_CENTER_HZ)],
        );
        edge.validate().unwrap();
        assert!(edge.includes_channel(MAX_CENTER_HZ));
        assert!(!edge.includes_channel(MAX_CENTER_HZ + 1));
        edge.upper_hz += 1;
        assert!(edge.validate().is_err());
        edge.upper_hz -= 1;
        edge.operation = "sequential_rf_sweep".to_owned();
        assert!(edge.validate().is_err());
    }

    #[test]
    fn receipt_sums_default_and_overridden_tune_extents() {
        let centers = [(902_000_000, 902_000_000), (927_000_000, 927_000_000)];
        let mut sweep = receipt("targeted_rf_sweep", 902_000_000, 927_000_001, &centers);
        sweep.tunes[1].dwell_ms = Some(1);
        assert!(sweep.validate().is_err());
        sweep.iq_bytes = 11 * (SAMPLE_RATE_HZ / 1_000) * 4;
        sweep.validate().unwrap();
        assert_eq!(sweep.tunes[0].samples(sweep.dwell_ms).unwrap(), 200_000);
        assert_eq!(sweep.tunes[1].samples(sweep.dwell_ms).unwrap(), 20_000);
        for invalid in [0, MAX_DWELL_MS + 1, u64::MAX] {
            sweep.tunes[1].dwell_ms = Some(invalid);
            assert!(
                sweep
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("invalid sweep dwell duration at tune-0002")
            );
        }
    }

    #[test]
    fn targeted_receipt_still_checks_byte_extent_and_each_observed_band() {
        let mut targeted = receipt(
            "targeted_rf_sweep",
            902_000_000,
            902_000_001,
            &[(902_000_000, 902_000_000)],
        );
        targeted.iq_bytes -= 4;
        assert!(targeted.validate().is_err());
        targeted.iq_bytes += 4;
        targeted.tunes[0].observed_bands[0].lower_hz += 1;
        assert!(targeted.validate().is_err());
        targeted.tunes[0].observed_center_hz += 11;
        assert!(
            targeted
                .validate()
                .unwrap_err()
                .to_string()
                .contains("observed center")
        );
    }
}
