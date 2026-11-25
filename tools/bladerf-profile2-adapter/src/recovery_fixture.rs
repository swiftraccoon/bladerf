//! Lazy construction of one device-free fresh-recovery fixture.
//!
//! The child stages a new frozen-stock snapshot and retains the exact B2L1
//! table, but the guarded worker receives only deterministic [`ModelCalls`].

#![forbid(unsafe_code)]

use std::error::Error;
use std::path::PathBuf;

use anyhow::{Context, Result};
use bladerf_profile2_recovery_ipc::{
    BackendInputError, BundleFacts, ImageFacts, LibraryFacts, RecoveryBackend,
};

use crate::loader_fixture::validate_library_expectation;
use crate::loader_worker::{MODEL_SERIAL, ModelCalls, RecoveryLoaderWorker};
use crate::native_abi::{LoaderDynamicCalls, prepare_bladerf2_loader_neutral_library};
use crate::sealed_image::{ExpectedImage, SealedImage, StockImage};

const RECOVERY_FIXTURE_HANDLE: u32 = 0x4001;

/// Exact files needed to construct one recovery fixture after Hello.
pub(crate) struct RecoveryFixtureConfig {
    library: PathBuf,
    library_sha256: String,
    stock_image: PathBuf,
}

impl RecoveryFixtureConfig {
    pub(crate) fn new(library: PathBuf, library_sha256: String, stock_image: PathBuf) -> Self {
        Self {
            library,
            library_sha256,
            stock_image,
        }
    }

    /// Independently stage stock and construct the fake worker after Hello.
    pub(crate) fn open(self) -> Result<RecoveryFixture, Box<dyn Error + Send + Sync>> {
        validate_library_expectation(&self.library, &self.library_sha256)
            .map_err(BackendInputError::invalid_library_expectation)?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;

        // Stage before loading a shared object so malformed or drifting stock
        // cannot run its initializer. This descriptor is unique to this child.
        let staged_stock = stage_recovery_stock(&stock)?;
        let stock_facts = ImageFacts::new(staged_stock.sha256(), staged_stock.bytes());
        let (retained_library, library_report) =
            prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
        let marker = library_report
            .bladerf2_loader_neutral_open_marker()
            .context("qualified recovery fixture library lacks the required B2L1 marker")?;
        let bundle = BundleFacts::new(
            LibraryFacts::new(
                library_report.version().to_string(),
                library_report.sha256(),
                marker,
            ),
            stock_facts,
        );
        let worker = RecoveryLoaderWorker::open(
            ModelCalls::fixture(RECOVERY_FIXTURE_HANDLE),
            staged_stock,
            MODEL_SERIAL,
        )?;
        Ok(RecoveryFixture {
            worker,
            _retained_library: retained_library,
            bundle,
        })
    }
}

fn stage_recovery_stock(stock: &ExpectedImage<StockImage>) -> Result<SealedImage<StockImage>> {
    stock.stage()
}

/// One fake recovery backend with its own stock snapshot and worker.
pub(crate) struct RecoveryFixture {
    // Drop the fake worker before releasing the retained, unused B2L1 table.
    worker: RecoveryLoaderWorker<ModelCalls>,
    _retained_library: LoaderDynamicCalls,
    bundle: BundleFacts,
}

impl RecoveryBackend for RecoveryFixture {
    type Error = anyhow::Error;

    fn bundle(&self) -> &BundleFacts {
        &self.bundle
    }

    fn restore_stock(&mut self) -> Result<()> {
        self.worker.restore_stock()
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.worker.verify_stock()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.worker.shutdown()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};

    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn every_dedicated_recovery_start_stages_a_distinct_stock_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stock.rbf");
        let bytes = b"dedicated recovery stock staging fixture";
        fs::write(&path, bytes).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let expected =
            ExpectedImage::<StockImage>::test_image(&path, &sha256, bytes.len() as u64).unwrap();

        let first = stage_recovery_stock(&expected).unwrap();
        let second = stage_recovery_stock(&expected).unwrap();
        assert_ne!(
            first.stock_load_path().to_bytes(),
            second.stock_load_path().to_bytes()
        );
        assert_eq!(first.sha256(), second.sha256());
        assert_eq!(first.bytes(), second.bytes());
    }
}
