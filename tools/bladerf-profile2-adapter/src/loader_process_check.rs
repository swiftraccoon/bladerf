//! Parent-side device-free qualification of both loader fixture processes.
//!
//! The parent performs only lexical expectation validation and process
//! supervision. Each child independently stages its role-specific images and
//! qualifies the exact loader-neutral library after protocol Hello. Both child
//! workers receive deterministic fake calls; no serial or device selector is
//! accepted anywhere in this flow.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use bladerf_profile2_loader_ipc::{
    BundleFacts, ImageFacts, LibraryFacts, MAX_TRANSACTION_TIMEOUT, PrimaryFixtureClient,
    REQUIRED_LIBRARY_MARKER, REQUIRED_LIBRARY_VERSION, REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256,
    RecoveryFixtureClient,
};

use crate::loader_fixture::validate_library_expectation;
use crate::sealed_image::{CustomImage, ExpectedImage, StagedImages, StockImage};

/// Value-only report from two complete fake subprocess lifecycles.
#[derive(Debug)]
pub(crate) struct LoaderProcessReport {
    library_sha256: String,
    custom_sha256: String,
    custom_bytes: u64,
}

impl LoaderProcessReport {
    pub(crate) fn library_sha256(&self) -> &str {
        &self.library_sha256
    }

    pub(crate) fn custom_sha256(&self) -> &str {
        &self.custom_sha256
    }

    pub(crate) const fn custom_bytes(&self) -> u64 {
        self.custom_bytes
    }
}

/// Run complete primary and fresh-recovery fixture clients against this binary.
///
/// Validation before spawn is lexical only: it does not open, canonicalize or
/// hash the library or image paths. Each child repeats validation and performs
/// all file access inside its bounded Hello transaction.
pub(crate) fn check_profile2_loader_process(
    program: &Path,
    library: &Path,
    library_sha256: &str,
    custom_image: &Path,
    custom_sha256: &str,
    custom_bytes: u64,
    stock_image: &Path,
) -> Result<LoaderProcessReport> {
    ensure!(
        !program.as_os_str().is_empty() && program.is_absolute(),
        "loader fixture executable must be an absolute path: {}",
        program.display()
    );
    validate_library_expectation(library, library_sha256)?;
    let custom = ExpectedImage::<CustomImage>::custom(custom_image, custom_sha256, custom_bytes)?;
    let stock = ExpectedImage::<StockImage>::stock(stock_image)?;
    StagedImages::validate_roles(&custom, &stock)?;

    let library_facts = LibraryFacts::new(
        REQUIRED_LIBRARY_VERSION,
        library_sha256,
        REQUIRED_LIBRARY_MARKER,
    );
    let stock_facts = ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES);
    let primary_bundle = BundleFacts::new(
        library_facts.clone(),
        Some(ImageFacts::new(custom_sha256, custom_bytes)),
        stock_facts.clone(),
    );
    let recovery_bundle = BundleFacts::new(library_facts, None, stock_facts);

    let mut primary = PrimaryFixtureClient::spawn(
        program,
        primary_arguments(
            library,
            library_sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        ),
        primary_bundle,
        MAX_TRANSACTION_TIMEOUT,
    )
    .context("start exact primary loader fixture process")?;
    primary
        .load_profile2()
        .context("dispatch custom image through fake primary loader")?;
    primary
        .verify_profile2()
        .context("verify fake primary custom identity")?;
    primary
        .restore_stock()
        .context("dispatch stock through fake primary loader")?;
    primary
        .verify_stock()
        .context("verify fake primary stock identity")?;
    primary
        .shutdown()
        .context("shut down primary loader fixture process")?;

    let mut recovery = RecoveryFixtureClient::spawn(
        program,
        recovery_arguments(library, library_sha256, stock_image),
        recovery_bundle,
        MAX_TRANSACTION_TIMEOUT,
    )
    .context("start independent recovery loader fixture process")?;
    recovery
        .restore_stock()
        .context("dispatch independently staged stock through fake recovery loader")?;
    recovery
        .verify_stock()
        .context("verify fake recovery stock identity")?;
    recovery
        .shutdown()
        .context("shut down recovery loader fixture process")?;

    Ok(LoaderProcessReport {
        library_sha256: library_sha256.to_owned(),
        custom_sha256: custom_sha256.to_owned(),
        custom_bytes,
    })
}

fn primary_arguments(
    library: &Path,
    library_sha256: &str,
    custom_image: &Path,
    custom_sha256: &str,
    custom_bytes: u64,
    stock_image: &Path,
) -> Vec<OsString> {
    vec![
        "loader-fixture-primary".into(),
        "--library".into(),
        library.as_os_str().to_owned(),
        "--sha256".into(),
        library_sha256.into(),
        "--custom-image".into(),
        custom_image.as_os_str().to_owned(),
        "--custom-sha256".into(),
        custom_sha256.into(),
        "--custom-bytes".into(),
        custom_bytes.to_string().into(),
        "--stock-image".into(),
        stock_image.as_os_str().to_owned(),
    ]
}

fn recovery_arguments(library: &Path, library_sha256: &str, stock_image: &Path) -> Vec<OsString> {
    vec![
        "loader-fixture-recovery".into(),
        "--library".into(),
        library.as_os_str().to_owned(),
        "--sha256".into(),
        library_sha256.into(),
        "--stock-image".into(),
        stock_image.as_os_str().to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn role_arguments_are_fixed_and_contain_no_device_selector() {
        let primary = primary_arguments(
            Path::new("/opt/profile 2/libbladeRF.so.2"),
            SHA256,
            Path::new("/opt/profile 2/custom.rbf"),
            &"a".repeat(64),
            12_858_972,
            Path::new("/opt/profile 2/stock.rbf"),
        );
        assert_eq!(primary[0], OsStr::new("loader-fixture-primary"));
        assert!(primary.contains(&OsString::from("/opt/profile 2/custom.rbf")));
        assert!(!primary.iter().any(|value| value == "--serial"));

        let recovery = recovery_arguments(
            Path::new("/opt/profile 2/libbladeRF.so.2"),
            SHA256,
            Path::new("/opt/profile 2/stock.rbf"),
        );
        assert_eq!(recovery[0], OsStr::new("loader-fixture-recovery"));
        assert!(!recovery.iter().any(|value| value == "--custom-image"));
        assert!(!recovery.iter().any(|value| value == "--serial"));
    }

    #[test]
    fn malformed_expectations_fail_before_spawning_the_program() {
        let error = check_profile2_loader_process(
            Path::new("/this/program/must/not/be/spawned"),
            Path::new("/this/library/must/not/be/opened.so"),
            "not-a-digest",
            Path::new("/this/custom/must/not/be/opened.rbf"),
            &"a".repeat(64),
            12_858_972,
            Path::new("/this/stock/must/not/be/opened.rbf"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256"));
    }
}
