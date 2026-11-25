//! Parent-side device-free qualification of a fresh recovery process.
//!
//! The parent validates value syntax only. The child independently stages the
//! stock image, qualifies B2L1, and drives one fake recovery handle.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use bladerf_profile2_recovery_ipc::{
    BundleFacts, FixtureRecoveryClient, ImageFacts, LibraryFacts, MAX_TRANSACTION_TIMEOUT,
    REQUIRED_LIBRARY_MARKER, REQUIRED_LIBRARY_VERSION, REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256,
};

use crate::loader_fixture::validate_library_expectation;
use crate::sealed_image::{ExpectedImage, StockImage};

/// Value-only result of one complete fake fresh-recovery lifecycle.
#[derive(Debug)]
pub(crate) struct RecoveryProcessReport {
    library_sha256: String,
}

impl RecoveryProcessReport {
    pub(crate) fn library_sha256(&self) -> &str {
        &self.library_sha256
    }
}

/// Run restore, verification and shutdown against a role-fixed fake child.
pub(crate) fn check_profile2_recovery_process(
    program: &Path,
    library: &Path,
    library_sha256: &str,
    stock_image: &Path,
) -> Result<RecoveryProcessReport> {
    ensure!(
        !program.as_os_str().is_empty() && program.is_absolute(),
        "recovery fixture executable must be an absolute path: {}",
        program.display()
    );
    validate_library_expectation(library, library_sha256)?;
    ExpectedImage::<StockImage>::stock(stock_image)?;

    let bundle = BundleFacts::new(
        LibraryFacts::new(
            REQUIRED_LIBRARY_VERSION,
            library_sha256,
            REQUIRED_LIBRARY_MARKER,
        ),
        ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
    );
    let mut client = FixtureRecoveryClient::spawn(
        program,
        recovery_arguments(library, library_sha256, stock_image),
        bundle,
        MAX_TRANSACTION_TIMEOUT,
    )
    .context("start exact fresh-recovery fixture process")?;
    client
        .restore_stock()
        .context("dispatch frozen stock through fake fresh-recovery worker")?;
    client
        .verify_stock()
        .context("verify fake fresh-recovery stock identity")?;
    client
        .shutdown()
        .context("shut down and reap fresh-recovery fixture process")?;

    Ok(RecoveryProcessReport {
        library_sha256: library_sha256.to_owned(),
    })
}

fn recovery_arguments(library: &Path, library_sha256: &str, stock_image: &Path) -> Vec<OsString> {
    vec![
        OsString::from("recovery-fixture"),
        OsString::from("--library"),
        library.as_os_str().to_owned(),
        OsString::from("--sha256"),
        OsString::from(library_sha256),
        OsString::from("--stock-image"),
        stock_image.as_os_str().to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_arguments_are_role_fixed_and_have_no_device_selector() {
        let arguments = recovery_arguments(
            Path::new("/trusted/libbladeRF.so.2"),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            Path::new("/trusted/stock.rbf"),
        );
        let rendered: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            rendered,
            [
                "recovery-fixture",
                "--library",
                "/trusted/libbladeRF.so.2",
                "--sha256",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "--stock-image",
                "/trusted/stock.rbf",
            ]
        );
        assert!(!rendered.iter().any(|argument| argument == "--serial"));
        assert!(!rendered.iter().any(|argument| argument == "--device"));
        assert!(!rendered.iter().any(|argument| argument.contains("custom")));
    }
}
