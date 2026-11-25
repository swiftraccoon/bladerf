//! Immutable, role-typed staging for the fixed profile-2 FPGA images.
//!
//! This module performs no device or native-library operation. It pins each
//! source inode without following a final symlink, copies and hashes it once
//! into a sealed memfd, and constructs the only path a future loader call may
//! receive. Custom and stock images have distinct Rust types so future loader
//! APIs can require the correct role at each transition.

#![forbid(unsafe_code)]

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use bladerf_profile2_loader_ipc::{MAX_IMAGE_BYTES, REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256};
#[cfg(target_os = "linux")]
use nix::fcntl::{FcntlArg, FdFlag, OFlag, SealFlag, fcntl, open as nix_open};
#[cfg(target_os = "linux")]
use nix::sys::memfd::{MFdFlags, memfd_create};
#[cfg(target_os = "linux")]
use nix::sys::stat::{Mode, SFlag, fstat};
use sha2::{Digest, Sha256};

pub(crate) trait ImageRole {
    const LABEL: &'static str;
    const MEMFD_NAME: &'static str;
}

pub(crate) enum CustomImage {}

impl ImageRole for CustomImage {
    const LABEL: &'static str = "custom profile-2 image";
    const MEMFD_NAME: &'static str = "bladerf-profile2-custom";
}

pub(crate) enum StockImage {}

impl ImageRole for StockImage {
    const LABEL: &'static str = "stock recovery image";
    const MEMFD_NAME: &'static str = "bladerf-profile2-stock";
}

pub(crate) struct ExpectedImage<R: ImageRole> {
    source: PathBuf,
    sha256: String,
    bytes: u64,
    _role: PhantomData<fn() -> R>,
}

impl<R: ImageRole> ExpectedImage<R> {
    fn new(source: &Path, sha256: &str, bytes: u64) -> Result<Self> {
        ensure!(!source.as_os_str().is_empty(), "{} path is empty", R::LABEL);
        ensure!(
            source.is_absolute(),
            "{} path must be absolute: {}",
            R::LABEL,
            source.display()
        );
        validate_sha256(sha256, R::LABEL)?;
        ensure!(
            (1..=MAX_IMAGE_BYTES).contains(&bytes),
            "{} size must be 1 through {MAX_IMAGE_BYTES} bytes",
            R::LABEL
        );
        Ok(Self {
            source: source.to_path_buf(),
            sha256: sha256.to_owned(),
            bytes,
            _role: PhantomData,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_image(source: &Path, sha256: &str, bytes: u64) -> Result<Self> {
        Self::new(source, sha256, bytes)
    }

    pub(crate) fn stage(&self) -> Result<SealedImage<R>> {
        let staged = self.stage_with_after_pin(|| Ok(()))?;
        staged.verify_ready()?;
        Ok(staged)
    }

    #[cfg(target_os = "linux")]
    fn stage_with_after_pin<F>(&self, after_pin: F) -> Result<SealedImage<R>>
    where
        F: FnOnce() -> Result<()>,
    {
        let (mut source, _pinned) = self.open_source_with_after_pin(after_pin)?;

        let mut staged_file = create_staged_file::<R>()?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut copied = 0u64;
        loop {
            let count = source
                .read(&mut buffer)
                .with_context(|| format!("read {} {}", R::LABEL, self.source.display()))?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .with_context(|| format!("{} byte count overflow", R::LABEL))?;
            ensure!(
                copied <= self.bytes,
                "{} grew beyond its expected {} bytes while staging {}",
                R::LABEL,
                self.bytes,
                self.source.display()
            );
            hasher.update(&buffer[..count]);
            staged_file
                .write_all(&buffer[..count])
                .with_context(|| format!("copy {} into sealed snapshot", R::LABEL))?;
        }
        ensure!(
            copied == self.bytes,
            "{} size changed while staging {}: expected {}, copied {}",
            R::LABEL,
            self.source.display(),
            self.bytes,
            copied
        );
        let actual_sha256 = format!("{:x}", hasher.finalize());
        ensure!(
            actual_sha256 == self.sha256,
            "{} SHA-256 mismatch for {}: expected {}, found {}",
            R::LABEL,
            self.source.display(),
            self.sha256,
            actual_sha256
        );

        staged_file
            .flush()
            .with_context(|| format!("flush sealed {} snapshot", R::LABEL))?;
        staged_file
            .seek(SeekFrom::Start(0))
            .with_context(|| format!("rewind sealed {} snapshot", R::LABEL))?;
        seal_staged_file::<R>(&staged_file)?;
        let load_path = CString::new(format!("/proc/self/fd/{}", staged_file.as_raw_fd()))
            .with_context(|| format!("construct sealed {} load path", R::LABEL))?;

        Ok(SealedImage {
            file: staged_file,
            load_path,
            sha256: actual_sha256,
            bytes: copied,
            _role: PhantomData,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn stage_with_after_pin<F>(&self, _after_pin: F) -> Result<SealedImage<R>>
    where
        F: FnOnce() -> Result<()>,
    {
        anyhow::bail!("profile-2 image staging is supported only on Linux")
    }

    #[cfg(target_os = "linux")]
    fn open_source_with_after_pin<F>(&self, after_pin: F) -> Result<(File, std::os::fd::OwnedFd)>
    where
        F: FnOnce() -> Result<()>,
    {
        let pinned = nix_open(
            self.source.as_path(),
            OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .with_context(|| format!("pin {} {}", R::LABEL, self.source.display()))?;
        let pinned_stat = fstat(&pinned)
            .with_context(|| format!("inspect {} {}", R::LABEL, self.source.display()))?;
        ensure!(
            is_regular_file_type(SFlag::from_bits_truncate(pinned_stat.st_mode)),
            "{} is not a regular file: {}",
            R::LABEL,
            self.source.display()
        );
        ensure!(
            pinned_stat.st_size == self.bytes as i64,
            "{} size differs for {}: expected {}, found {}",
            R::LABEL,
            self.source.display(),
            self.bytes,
            pinned_stat.st_size
        );
        after_pin().with_context(|| format!("after pinning {}", R::LABEL))?;

        let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
        let source = File::open(&descriptor_path)
            .with_context(|| format!("open pinned {} inode {}", R::LABEL, self.source.display()))?;
        let readable_stat = fstat(&source)
            .with_context(|| format!("reinspect {} {}", R::LABEL, self.source.display()))?;
        ensure!(
            readable_stat.st_dev == pinned_stat.st_dev
                && readable_stat.st_ino == pinned_stat.st_ino
                && readable_stat.st_size == pinned_stat.st_size,
            "{} inode changed before reading {}",
            R::LABEL,
            self.source.display()
        );
        Ok((source, pinned))
    }
}

impl ExpectedImage<CustomImage> {
    pub(crate) fn custom(source: &Path, sha256: &str, bytes: u64) -> Result<Self> {
        Self::new(source, sha256, bytes)
    }
}

impl ExpectedImage<StockImage> {
    pub(crate) fn stock(source: &Path) -> Result<Self> {
        Self::new(source, REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES)
    }
}

/// A sealed image snapshot whose role and descriptor lifetime are owned together.
///
/// This type is deliberately not `Clone`. The future loader worker must retain
/// it until the synchronous FPGA-load call using its borrowed role-specific path
/// has returned. A numeric proc-fd path must not be copied and retained after
/// this owner is dropped because the process can reuse that descriptor number.
pub(crate) struct SealedImage<R: ImageRole> {
    file: File,
    load_path: CString,
    sha256: String,
    bytes: u64,
    _role: PhantomData<fn() -> R>,
}

impl<R: ImageRole> SealedImage<R> {
    fn load_path(&self) -> &CStr {
        &self.load_path
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(target_os = "linux")]
    fn verify_ready(&self) -> Result<()> {
        let descriptor_path = Path::new(std::ffi::OsStr::from_bytes(self.load_path().to_bytes()));
        let reopened = File::open(descriptor_path)
            .with_context(|| format!("reopen sealed {} load path", R::LABEL))?;
        let owner_stat =
            fstat(&self.file).with_context(|| format!("inspect sealed {} owner", R::LABEL))?;
        let reopened_stat =
            fstat(&reopened).with_context(|| format!("inspect reopened sealed {}", R::LABEL))?;
        ensure!(
            owner_stat.st_dev == reopened_stat.st_dev
                && owner_stat.st_ino == reopened_stat.st_ino
                && reopened_stat.st_size == self.bytes as i64,
            "sealed {} load path does not name its owned snapshot",
            R::LABEL
        );
        verify_descriptor_flags::<R>(&self.file)?;
        verify_required_seals::<R>(&self.file)
    }

    #[cfg(not(target_os = "linux"))]
    fn verify_ready(&self) -> Result<()> {
        anyhow::bail!("profile-2 image staging is supported only on Linux")
    }
}

impl SealedImage<CustomImage> {
    pub(crate) fn custom_load_path(&self) -> &CStr {
        self.load_path()
    }
}

impl SealedImage<StockImage> {
    pub(crate) fn stock_load_path(&self) -> &CStr {
        self.load_path()
    }
}

/// Both independently sealed image roles retained for one primary worker.
///
/// A fresh recovery worker must create a new stock expectation and snapshot in
/// its own process; this owner is never transferred out of the primary worker.
pub(crate) struct StagedImages {
    custom: SealedImage<CustomImage>,
    stock: SealedImage<StockImage>,
}

impl StagedImages {
    pub(crate) fn validate_roles(
        custom: &ExpectedImage<CustomImage>,
        stock: &ExpectedImage<StockImage>,
    ) -> Result<()> {
        ensure!(
            custom.sha256 != stock.sha256,
            "custom profile-2 and stock recovery image SHA-256 values must differ"
        );
        Ok(())
    }

    pub(crate) fn stage(
        custom: &ExpectedImage<CustomImage>,
        stock: &ExpectedImage<StockImage>,
    ) -> Result<Self> {
        Self::validate_roles(custom, stock)?;
        let custom = custom.stage()?;
        let stock = stock.stage()?;
        ensure!(
            custom.custom_load_path() != stock.stock_load_path(),
            "custom and stock images unexpectedly share one staged descriptor"
        );
        Ok(Self { custom, stock })
    }

    pub(crate) const fn custom(&self) -> &SealedImage<CustomImage> {
        &self.custom
    }

    pub(crate) const fn stock(&self) -> &SealedImage<StockImage> {
        &self.stock
    }

    pub(crate) fn report(&self) -> ImageStagingReport {
        ImageStagingReport {
            custom_sha256: self.custom().sha256.clone(),
            custom_bytes: self.custom().bytes,
            stock_sha256: self.stock().sha256.clone(),
            stock_bytes: self.stock().bytes,
        }
    }
}

/// Device-free result of staging both exact FPGA image roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ImageStagingReport {
    custom_sha256: String,
    custom_bytes: u64,
    stock_sha256: String,
    stock_bytes: u64,
}

impl ImageStagingReport {
    pub(crate) fn custom_sha256(&self) -> &str {
        &self.custom_sha256
    }

    pub(crate) const fn custom_bytes(&self) -> u64 {
        self.custom_bytes
    }

    pub(crate) fn stock_sha256(&self) -> &str {
        &self.stock_sha256
    }

    pub(crate) const fn stock_bytes(&self) -> u64 {
        self.stock_bytes
    }
}

/// Stage and verify the exact custom profile-2 and stock xA9 image roles.
///
/// This function performs filesystem and memory-file operations only. It does
/// not load libbladeRF, open a device, transfer USB data, or load an FPGA.
pub(crate) fn check_profile2_images(
    custom_path: &Path,
    custom_sha256: &str,
    custom_bytes: u64,
    stock_path: &Path,
) -> Result<ImageStagingReport> {
    let custom = ExpectedImage::<CustomImage>::custom(custom_path, custom_sha256, custom_bytes)?;
    let stock = ExpectedImage::<StockImage>::stock(stock_path)?;
    let staged = StagedImages::stage(&custom, &stock)?;
    Ok(staged.report())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} SHA-256 must be exactly 64 lowercase hexadecimal characters"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_regular_file_type(file_type: SFlag) -> bool {
    file_type & SFlag::S_IFMT == SFlag::S_IFREG
}

#[cfg(target_os = "linux")]
fn create_staged_file<R: ImageRole>() -> Result<File> {
    let descriptor = memfd_create(
        R::MEMFD_NAME,
        MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
    )
    .with_context(|| format!("create sealed {} snapshot", R::LABEL))?;
    Ok(File::from(descriptor))
}

#[cfg(target_os = "linux")]
fn required_seals() -> SealFlag {
    SealFlag::F_SEAL_WRITE | SealFlag::F_SEAL_GROW | SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_SEAL
}

#[cfg(target_os = "linux")]
fn seal_staged_file<R: ImageRole>(file: &File) -> Result<()> {
    fcntl(file, FcntlArg::F_ADD_SEALS(required_seals()))
        .with_context(|| format!("seal {} snapshot", R::LABEL))?;
    verify_required_seals::<R>(file)
}

#[cfg(target_os = "linux")]
fn verify_required_seals<R: ImageRole>(file: &File) -> Result<()> {
    let actual = fcntl(file, FcntlArg::F_GET_SEALS)
        .with_context(|| format!("read sealed {} snapshot seals", R::LABEL))?;
    let required = required_seals().bits();
    ensure!(
        actual & required == required,
        "sealed {} snapshot is missing required seals",
        R::LABEL
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_descriptor_flags<R: ImageRole>(file: &File) -> Result<()> {
    let actual = fcntl(file, FcntlArg::F_GETFD)
        .with_context(|| format!("read sealed {} descriptor flags", R::LABEL))?;
    ensure!(
        FdFlag::from_bits_truncate(actual).contains(FdFlag::FD_CLOEXEC),
        "sealed {} descriptor is missing close-on-exec",
        R::LABEL
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};

    #[cfg(target_os = "linux")]
    use nix::sys::stat::Mode;
    #[cfg(target_os = "linux")]
    use nix::unistd::mkfifo;

    use super::*;

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[cfg(target_os = "linux")]
    fn load_path<R: ImageRole>(image: &SealedImage<R>) -> PathBuf {
        PathBuf::from(std::ffi::OsStr::from_bytes(image.load_path().to_bytes()))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_custom_and_stock_roles_are_sealed_and_ready() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.rbf");
        let bytes = b"typed image bytes";
        fs::write(&path, bytes).unwrap();

        let custom_expected =
            ExpectedImage::<CustomImage>::new(&path, &digest(bytes), bytes.len() as u64).unwrap();
        let stock_expected =
            ExpectedImage::<StockImage>::new(&path, &digest(bytes), bytes.len() as u64).unwrap();
        let custom = custom_expected.stage().unwrap();
        let stock = stock_expected.stage().unwrap();

        custom.verify_ready().unwrap();
        stock.verify_ready().unwrap();
        assert_ne!(custom.load_path(), stock.load_path());
        assert_eq!(custom.sha256, digest(bytes));
        assert_eq!(custom.bytes, bytes.len() as u64);

        let mut reopened = File::open(load_path(&custom)).unwrap();
        let mut staged = Vec::new();
        reopened.read_to_end(&mut staged).unwrap();
        assert_eq!(staged, bytes);

        let mut writer = custom.file.try_clone().unwrap();
        writer.seek(SeekFrom::End(0)).unwrap();
        assert!(writer.write_all(b"changed").is_err());
        assert!(writer.set_len(0).is_err());

        assert!(fs::metadata(load_path(&stock)).unwrap().is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staged_bytes_survive_source_mutation_and_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom.rbf");
        let original = b"original custom bytes";
        fs::write(&path, original).unwrap();
        let expected =
            ExpectedImage::<CustomImage>::new(&path, &digest(original), original.len() as u64)
                .unwrap();
        let sealed = expected.stage().unwrap();

        fs::write(&path, b"same-length mutation!").unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement pathname").unwrap();

        let mut staged = Vec::new();
        File::open(load_path(&sealed))
            .unwrap()
            .read_to_end(&mut staged)
            .unwrap();
        assert_eq!(staged, original);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn final_symlinks_fifos_directories_and_devices_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular.rbf");
        fs::write(&regular, b"abc").unwrap();
        let expected_hash = digest(b"abc");

        let symlink_path = directory.path().join("link.rbf");
        symlink(&regular, &symlink_path).unwrap();
        let symlink_expected =
            ExpectedImage::<CustomImage>::new(&symlink_path, &expected_hash, 3).unwrap();
        assert!(symlink_expected.stage().is_err());

        let fifo = directory.path().join("image.fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let fifo_expected = ExpectedImage::<CustomImage>::new(&fifo, &expected_hash, 3).unwrap();
        assert!(fifo_expected.stage().is_err());

        let directory_expected =
            ExpectedImage::<CustomImage>::new(directory.path(), &expected_hash, 3).unwrap();
        assert!(directory_expected.stage().is_err());

        let device_expected =
            ExpectedImage::<CustomImage>::new(Path::new("/dev/null"), &expected_hash, 3).unwrap();
        assert!(device_expected.stage().is_err());
    }

    #[test]
    fn expectation_validation_precedes_file_access() {
        let missing = Path::new("/this/path/must/not/be-opened/image.rbf");
        for (path, hash, bytes) in [
            (Path::new("relative.rbf"), "0".repeat(64), 1),
            (missing, "bad-digest".to_owned(), 1),
            (missing, "0".repeat(64), 0),
            (missing, "0".repeat(64), MAX_IMAGE_BYTES + 1),
        ] {
            let error = match ExpectedImage::<CustomImage>::new(path, &hash, bytes) {
                Ok(_) => panic!("invalid expectation unexpectedly succeeded"),
                Err(error) => error,
            };
            assert!(!format!("{error:#}").contains("No such file"));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exact_size_and_hash_are_both_required() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.rbf");
        fs::write(&path, b"abc").unwrap();

        let wrong_size = ExpectedImage::<CustomImage>::new(&path, &digest(b"abc"), 2).unwrap();
        assert!(wrong_size.stage().is_err());
        let wrong_hash = ExpectedImage::<CustomImage>::new(&path, &digest(b"xyz"), 3).unwrap();
        assert!(wrong_hash.stage().is_err());

        let empty = directory.path().join("empty.rbf");
        fs::write(&empty, []).unwrap();
        assert!(ExpectedImage::<CustomImage>::new(&empty, &digest(b""), 0).is_err());

        let oversized = directory.path().join("oversized.rbf");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert!(
            ExpectedImage::<CustomImage>::new(&oversized, &"0".repeat(64), MAX_IMAGE_BYTES + 1)
                .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_inode_survives_replacement_but_growth_and_truncation_fail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.rbf");
        fs::write(&path, b"abc").unwrap();
        let expected = ExpectedImage::<CustomImage>::new(&path, &digest(b"abc"), 3).unwrap();
        let sealed = expected
            .stage_with_after_pin(|| {
                fs::remove_file(&path)?;
                fs::write(&path, b"replacement")?;
                Ok(())
            })
            .unwrap();
        let mut staged = Vec::new();
        File::open(load_path(&sealed))
            .unwrap()
            .read_to_end(&mut staged)
            .unwrap();
        assert_eq!(staged, b"abc");

        fs::write(&path, b"abc").unwrap();
        let expected = ExpectedImage::<CustomImage>::new(&path, &digest(b"abc"), 3).unwrap();
        let truncated = expected.stage_with_after_pin(|| {
            OpenOptions::new().write(true).open(&path)?.set_len(1)?;
            Ok(())
        });
        assert!(truncated.is_err());

        fs::write(&path, b"abc").unwrap();
        let expected = ExpectedImage::<CustomImage>::new(&path, &digest(b"abc"), 3).unwrap();
        let grown = expected.stage_with_after_pin(|| {
            OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(b"d")?;
            Ok(())
        });
        assert!(grown.is_err());
    }

    #[test]
    fn custom_cannot_alias_the_frozen_stock_digest() {
        let missing_custom = Path::new("/this/path/must/not/be-opened/custom.rbf");
        let missing_stock = Path::new("/this/path/must/not/be-opened/stock.rbf");
        let error = check_profile2_images(
            missing_custom,
            REQUIRED_STOCK_SHA256,
            REQUIRED_STOCK_BYTES,
            missing_stock,
        )
        .unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("must differ"), "{diagnostic}");
        assert!(!diagnostic.contains("No such file"), "{diagnostic}");
    }

    #[test]
    fn stock_expectation_freezes_the_qualified_digest_and_size() {
        let expected = ExpectedImage::<StockImage>::stock(Path::new(
            "/this/path/does/not/need/to/exist/stock.rbf",
        ))
        .unwrap();
        assert_eq!(expected.sha256, REQUIRED_STOCK_SHA256);
        assert_eq!(expected.bytes, REQUIRED_STOCK_BYTES);
    }
}
