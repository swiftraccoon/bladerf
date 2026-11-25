//! Lazy construction of one serial-selected native fresh-recovery backend.
//!
//! A new stock snapshot is sealed before B2L1 is mapped. The retained native
//! table then moves into the guarded recovery worker with its sole handle.

#![forbid(unsafe_code)]

use std::error::Error;
use std::path::PathBuf;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
use anyhow::bail;
use bladerf_profile2_recovery_ipc::{BackendInputError, BundleFacts, RecoveryBackend};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bladerf_profile2_recovery_ipc::{ImageFacts, LibraryFacts};

use crate::loader_fixture::validate_library_expectation;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::loader_worker::RecoveryLoaderWorker;
use crate::loader_worker::validate_requested_serial;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::native_abi::{LoaderDynamicCalls, prepare_bladerf2_loader_neutral_library};
use crate::sealed_image::{ExpectedImage, StockImage};

/// Direct arguments needed to open one native recovery worker after Hello.
pub(crate) struct RecoveryNativeConfig {
    library: PathBuf,
    library_sha256: String,
    stock_image: PathBuf,
    serial: String,
}

impl RecoveryNativeConfig {
    pub(crate) fn new(
        library: PathBuf,
        library_sha256: String,
        stock_image: PathBuf,
        serial: String,
    ) -> Self {
        Self {
            library,
            library_sha256,
            stock_image,
            serial,
        }
    }

    /// Stage and qualify stock, then open the exact selected device.
    ///
    /// The recovery server calls this only after an exact native Hello.
    pub(crate) fn open(self) -> Result<RecoveryNative, Box<dyn Error + Send + Sync>> {
        // Reject the selector before reading any supplied path.
        validate_requested_serial(&self.serial).map_err(BackendInputError::invalid_serial)?;
        validate_library_expectation(&self.library, &self.library_sha256)
            .map_err(BackendInputError::invalid_library_expectation)?;
        let stock = ExpectedImage::<StockImage>::stock(&self.stock_image)?;

        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            // A malformed or drifting stock image cannot run a shared-object
            // initializer. Every fresh child owns a distinct sealed snapshot.
            let staged_stock = stock.stage()?;
            let stock_facts = ImageFacts::new(staged_stock.sha256(), staged_stock.bytes());
            let (calls, library_report) =
                prepare_bladerf2_loader_neutral_library(&self.library, &self.library_sha256)?;
            let marker = library_report
                .bladerf2_loader_neutral_open_marker()
                .context("qualified native recovery library lacks the required B2L1 marker")?;
            let bundle = BundleFacts::new(
                LibraryFacts::new(
                    library_report.version().to_string(),
                    library_report.sha256(),
                    marker,
                ),
                stock_facts,
            );

            // The worker owns the table so a live handle keeps the mapping and
            // sealed backing file until orderly close or process teardown.
            let worker = RecoveryLoaderWorker::open(calls, staged_stock, &self.serial)?;
            Ok(RecoveryNative { worker, bundle })
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = stock;
            unsupported().map_err(Into::into)
        }
    }
}

/// One native stock-only recovery backend.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) struct RecoveryNative {
    worker: RecoveryLoaderWorker<LoaderDynamicCalls>,
    bundle: BundleFacts,
}

/// Native backend placeholder for unsupported build targets.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) struct RecoveryNative;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl RecoveryBackend for RecoveryNative {
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

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn unsupported<T>() -> Result<T> {
    bail!("native bladeRF recovery is supported only on x86-64 Linux")
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
impl RecoveryBackend for RecoveryNative {
    type Error = anyhow::Error;

    fn bundle(&self) -> &BundleFacts {
        unreachable!("unsupported targets cannot construct native recovery")
    }

    fn restore_stock(&mut self) -> Result<()> {
        unsupported()
    }

    fn verify_stock(&mut self) -> Result<()> {
        unsupported()
    }

    fn shutdown(&mut self) -> Result<()> {
        unsupported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_serial_precedes_every_path_access() {
        let error = RecoveryNativeConfig::new(
            PathBuf::from("/this/path/must/not-be-opened/libbladeRF.so.2"),
            "not-a-library-digest".to_owned(),
            PathBuf::from("/this/path/must/not-be-opened/stock.rbf"),
            "BAD-SERIAL".to_owned(),
        )
        .open()
        .err()
        .expect("malformed serial must fail");
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("serial"), "{diagnostic}");
        assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    }
}
