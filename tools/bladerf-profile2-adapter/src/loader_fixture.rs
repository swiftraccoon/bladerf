//! Lazy construction of the role-fixed, device-free loader fixture backends.
//!
//! Construction stages the exact image roles and retains the exact B2L1
//! library table, but the guarded worker receives only [`ModelCalls`]. The
//! retained native table is never passed to a worker or used for a device call.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use bladerf_profile2_loader_ipc::{BundleFacts, ImageFacts, LibraryFacts};

use crate::loader_server::LoaderBackend;
use crate::loader_worker::{MODEL_SERIAL, ModelCalls, PrimaryLoaderWorker, RecoveryLoaderWorker};
use crate::native_abi::{
    LoaderDynamicCalls, NativeLibraryReport, prepare_bladerf2_loader_neutral_library,
};
use crate::sealed_image::{CustomImage, ExpectedImage, SealedImage, StagedImages, StockImage};

const PRIMARY_FIXTURE_HANDLE: u32 = 0x1001;
const RECOVERY_FIXTURE_HANDLE: u32 = 0x2001;

pub(crate) struct PrimaryFixtureConfig {
    library: PathBuf,
    library_sha256: String,
    custom_image: PathBuf,
    custom_sha256: String,
    custom_bytes: u64,
    stock_image: PathBuf,
}

impl PrimaryFixtureConfig {
    pub(crate) fn new(
        library: PathBuf,
        library_sha256: String,
        custom_image: PathBuf,
        custom_sha256: String,
        custom_bytes: u64,
        stock_image: PathBuf,
    ) -> Self {
        Self {
            library,
            library_sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        }
    }

    /// Qualify all files and create the fake primary only after loader hello.
    pub(crate) fn open(self) -> Result<PrimaryFixture> {
        validate_library_expectation(&self.library, &self.library_sha256)?;
        let custom = ExpectedImage::<CustomImage>::custom(
            &self.custom_image,
            &self.custom_sha256,
            self.custom_bytes,
        )?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;
        StagedImages::validate_roles(&custom, &stock)?;

        // Stage before loading a shared object so malformed or drifting image
        // inputs cannot run its initializer. All work remains inside the child
        // and therefore within the parent's hello transaction deadline.
        let images = StagedImages::stage(&custom, &stock)?;
        let image_report = images.report();
        let (retained_library, library_report) =
            prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
        let bundle = BundleFacts::new(
            library_facts(&library_report)?,
            Some(ImageFacts::new(
                image_report.custom_sha256(),
                image_report.custom_bytes(),
            )),
            ImageFacts::new(image_report.stock_sha256(), image_report.stock_bytes()),
        );
        let worker = PrimaryLoaderWorker::open(
            ModelCalls::fixture(PRIMARY_FIXTURE_HANDLE),
            images,
            MODEL_SERIAL,
        )?;
        Ok(PrimaryFixture {
            worker,
            _retained_library: retained_library,
            bundle,
        })
    }
}

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

    /// Independently restage stock and create the fake recovery after hello.
    pub(crate) fn open(self) -> Result<RecoveryFixture> {
        validate_library_expectation(&self.library, &self.library_sha256)?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;
        let staged_stock = stage_recovery_stock(&stock)?;
        let stock_facts = ImageFacts::new(staged_stock.sha256(), staged_stock.bytes());
        let (retained_library, library_report) =
            prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
        let bundle = BundleFacts::new(library_facts(&library_report)?, None, stock_facts);
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

pub(crate) fn validate_library_expectation(path: &Path, sha256: &str) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "loader library path is empty");
    ensure!(
        path.is_absolute(),
        "loader library path must be absolute: {}",
        path.display()
    );
    ensure!(
        sha256.len() == 64
            && sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "loader library SHA-256 must be exactly 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn library_facts(report: &NativeLibraryReport) -> Result<LibraryFacts> {
    let marker = report
        .bladerf2_loader_neutral_open_marker()
        .context("qualified loader library lacks the required B2L1 marker")?;
    Ok(LibraryFacts::new(
        report.version().to_string(),
        report.sha256(),
        marker,
    ))
}

pub(crate) struct PrimaryFixture {
    // Drop the guarded fake worker before releasing the retained B2L1 table.
    worker: PrimaryLoaderWorker<ModelCalls>,
    _retained_library: LoaderDynamicCalls,
    bundle: BundleFacts,
}

impl LoaderBackend for PrimaryFixture {
    fn bundle(&self) -> &BundleFacts {
        &self.bundle
    }

    fn load_profile2(&mut self) -> Result<()> {
        self.worker.load_profile2()
    }

    fn verify_profile2(&mut self) -> Result<()> {
        self.worker.verify_profile2()
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

pub(crate) struct RecoveryFixture {
    // The recovery worker owns a stock snapshot staged in this process only.
    worker: RecoveryLoaderWorker<ModelCalls>,
    _retained_library: LoaderDynamicCalls,
    bundle: BundleFacts,
}

impl LoaderBackend for RecoveryFixture {
    fn bundle(&self) -> &BundleFacts {
        &self.bundle
    }

    fn load_profile2(&mut self) -> Result<()> {
        bail!("the recovery fixture cannot load the custom profile-2 image")
    }

    fn verify_profile2(&mut self) -> Result<()> {
        bail!("the recovery fixture cannot verify the custom profile-2 image")
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

    #[test]
    fn malformed_library_expectations_are_rejected_without_path_access() {
        let missing = Path::new("/this/path/must/not/be-opened/libbladeRF.so.2");
        assert!(validate_library_expectation(missing, "not-a-digest").is_err());
        assert!(
            validate_library_expectation(
                Path::new("relative.so"),
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_recovery_start_stages_a_distinct_stock_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stock.rbf");
        let bytes = b"synthetic independently staged stock image";
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
