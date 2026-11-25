//! Private process binding for the guarded profile-2 session policy.
//!
//! This module fixes the native primary and fresh-recovery child roles. The
//! primary child owns one device handle across custom load, typed control,
//! receive, capture, stop and same-handle stock restoration. A distinct
//! recovery child can load only the frozen stock image. Capture bytes flow into
//! a parent-owned sink; no writable path or descriptor crosses either protocol.
//!
//! The parent module exposes the only launcher surface. This private module
//! fixes preflight before publication setup, consumes one guarded factory run
//! and attempts terminalization exactly once.

use std::any::Any;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, anyhow, ensure};
use bladerf_profile2_recovery_ipc::NativeRecoveryClient;
use bladerf_profile2_session_ipc::{
    ActiveReceiveReadback, BundleFacts, CaptureReport, ConfiguredReceiveReadback, ImageFacts,
    LibraryFacts, MAX_FREQUENCY_ERROR_HZ, REQUIRED_LIBRARY_MARKER, REQUIRED_LIBRARY_VERSION,
    REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256, ReadRegister, ReceiveChannel, ReceivePlan,
    SOURCE_BANDWIDTH_HZ, SOURCE_SAMPLE_RATE_HZ, ServerMode, SessionClient, ShadowChannel,
};
use chrono::{SecondsFormat, Utc};
use nix::fcntl::{AT_FDCWD, AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::{UnlinkatFlags, linkat, unlinkat};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use super::{RecoveryDevice, SessionDevice, SessionFactory, SessionPlan};
use crate::profile2_control::{
    AppliedConfiguration, ReceiveReadback, Receiver, RegisterAddress, RegisterIo, reconcile_capture,
};
use crate::transport_v2::{ValidatedStream, validate_file_exact};

const MAX_ADAPTER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;
const MIN_USBFS_MEMORY_MB: u64 = 64;
const USBFS_MEMORY_PATH: &str = "/sys/module/usbcore/parameters/usbfs_memory_mb";
const ADAPTER_MODE: u32 = 0o500;
const INPUT_MODE: u32 = 0o400;
const OUTPUT_DIRECTORY_MODE: u32 = 0o700;
const PUBLICATION_FILE_MODE: u32 = 0o600;
const CAPTURE_FINAL_NAME: &str = "profile2.frames";
const TERMINAL_FINAL_NAME: &str = "session.json";
const CAPTURE_STAGE_NAME: &str = "profile2.frames.partial";
const SUCCESS_TERMINAL_STAGE_NAME: &str = "session.success.json.partial";
const FAILURE_TERMINAL_STAGE_NAME: &str = "session.failure.json.partial";
const MAX_TERMINAL_BYTES: usize = 64 * 1024;
// Five independent session diagnostics can appear in one terminal record.
// JSON may expand each input byte to a six-byte `\u00xx` escape, so keep the
// pre-serialization field bound small enough for the aggregate 64-KiB limit.
const MAX_FAILURE_TEXT_BYTES: usize = 1024;

struct ProcessPreflight<'a> {
    workspace_root: &'a Path,
    manifest: &'a Path,
    manifest_sha256: &'a str,
    adapter: &'a Path,
    adapter_sha256: &'a str,
    library: &'a Path,
    library_sha256: &'a str,
    stock_image: &'a Path,
    serial: &'a str,
    transaction_timeout: Duration,
    stage_parent: &'a Path,
}

/// Complete private request for one guarded, published process session.
///
/// The output is always a direct child of the exact trusted staging parent.
/// It has no public or direct command-line constructor; the parent launch
/// binding alone translates validated arguments into this value.
struct ProcessSessionRequest<'a> {
    preflight: ProcessPreflight<'a>,
    plan: SessionPlan,
    output_basename: &'a OsStr,
}

#[derive(Clone)]
struct VerifiedCustomImage {
    path: PathBuf,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy)]
struct StockExpectation<'a> {
    sha256: &'a str,
    bytes: u64,
}

trait PreflightChecks {
    fn check_usbfs(&mut self) -> Result<u64>;

    fn verify_custom(
        &mut self,
        workspace_root: &Path,
        manifest: &Path,
        manifest_sha256: &str,
        stop: &AtomicBool,
    ) -> Result<VerifiedCustomImage>;

    fn check_library(
        &mut self,
        adapter: &Path,
        library: &Path,
        library_sha256: &str,
        working_directory: &Path,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<()>;
}

struct HostPreflightChecks;

impl PreflightChecks for HostPreflightChecks {
    fn check_usbfs(&mut self) -> Result<u64> {
        crate::fpga_capture::check_usbfs(Path::new(USBFS_MEMORY_PATH))
    }

    fn verify_custom(
        &mut self,
        workspace_root: &Path,
        manifest: &Path,
        manifest_sha256: &str,
        stop: &AtomicBool,
    ) -> Result<VerifiedCustomImage> {
        let report =
            crate::profile2_image::verify(workspace_root, manifest, manifest_sha256, stop)?;
        Ok(VerifiedCustomImage {
            path: report.image().to_path_buf(),
            sha256: report.image_sha256().to_owned(),
            bytes: report.image_bytes(),
        })
    }

    fn check_library(
        &mut self,
        adapter: &Path,
        library: &Path,
        library_sha256: &str,
        working_directory: &Path,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> Result<()> {
        run_device_free_library_check(
            adapter,
            library,
            library_sha256,
            working_directory,
            timeout,
            stop,
        )
    }
}

struct StagedInput {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
    sha256: String,
    mode: u32,
}

impl StagedInput {
    fn path(&self) -> &Path {
        &self.path
    }

    fn revalidate(&self, stop: &AtomicBool) -> Result<()> {
        ensure_preflight_running(stop, "while revalidating staged inputs")?;
        let descriptor = self
            .file
            .metadata()
            .with_context(|| format!("inspect retained staged input {}", self.path.display()))?;
        let path = fs::symlink_metadata(&self.path)
            .with_context(|| format!("inspect staged input path {}", self.path.display()))?;
        ensure!(
            descriptor.file_type().is_file()
                && path.file_type().is_file()
                && descriptor.dev() == self.device
                && descriptor.ino() == self.inode
                && path.dev() == self.device
                && path.ino() == self.inode
                && descriptor.len() == self.bytes
                && path.len() == self.bytes
                && descriptor.nlink() == 1
                && path.nlink() == 1
                && descriptor.permissions().mode() & 0o7777 == self.mode
                && path.permissions().mode() & 0o7777 == self.mode,
            "staged input identity or permissions changed: {}",
            self.path.display()
        );

        let mut file = self
            .file
            .try_clone()
            .with_context(|| format!("clone retained staged input {}", self.path.display()))?;
        file.seek(SeekFrom::Start(0))?;
        let (sha256, bytes) = hash_reader(&mut file, self.bytes, stop)
            .with_context(|| format!("rehash staged input {}", self.path.display()))?;
        let final_descriptor = file.metadata()?;
        let final_path = fs::symlink_metadata(&self.path)?;
        ensure!(
            sha256 == self.sha256
                && bytes == self.bytes
                && final_descriptor.file_type().is_file()
                && final_path.file_type().is_file()
                && final_descriptor.dev() == self.device
                && final_descriptor.ino() == self.inode
                && final_descriptor.len() == self.bytes
                && final_descriptor.nlink() == 1
                && final_descriptor.permissions().mode() & 0o7777 == self.mode
                && final_path.dev() == self.device
                && final_path.ino() == self.inode
                && final_path.len() == self.bytes
                && final_path.nlink() == 1
                && final_path.permissions().mode() & 0o7777 == self.mode,
            "staged input bytes or path identity changed: {}",
            self.path.display()
        );
        Ok(())
    }
}

struct InputStage {
    directory: TempDir,
    adapter: StagedInput,
    library: StagedInput,
    custom: StagedInput,
    stock: StagedInput,
}

impl InputStage {
    fn revalidate_all(&self, stop: &AtomicBool) -> Result<()> {
        self.adapter.revalidate(stop)?;
        self.library.revalidate(stop)?;
        self.custom.revalidate(stop)?;
        self.stock.revalidate(stop)
    }

    fn revalidate_primary(&self, stop: &AtomicBool) -> Result<()> {
        self.revalidate_all(stop)
    }

    fn revalidate_recovery(&self, stop: &AtomicBool) -> Result<()> {
        self.adapter.revalidate(stop)?;
        self.library.revalidate(stop)?;
        self.stock.revalidate(stop)
    }
}

/// All preflighted values needed by the fixed native child interfaces.
///
/// The retained input stage keeps every child path private and inode-pinned.
/// Only the private process preflight constructs this value.
struct ProcessConfig {
    inputs: InputStage,
    adapter_sha256: String,
    library_sha256: String,
    custom_sha256: String,
    custom_bytes: u64,
    manifest_sha256: String,
    usbfs_memory_mb: u64,
    serial: String,
    transaction_timeout: Duration,
    stop: Arc<AtomicBool>,
}

impl ProcessConfig {
    fn preflight(request: ProcessPreflight<'_>, stop: &Arc<AtomicBool>) -> Result<Self> {
        let mut checks = HostPreflightChecks;
        Self::preflight_with(
            request,
            stop,
            &mut checks,
            StockExpectation {
                sha256: REQUIRED_STOCK_SHA256,
                bytes: REQUIRED_STOCK_BYTES,
            },
        )
    }

    fn preflight_with<C: PreflightChecks>(
        request: ProcessPreflight<'_>,
        stop: &Arc<AtomicBool>,
        checks: &mut C,
        stock: StockExpectation<'_>,
    ) -> Result<Self> {
        require_unwind_profile()?;
        ensure!(
            cfg!(all(
                target_os = "linux",
                target_arch = "x86_64",
                target_endian = "little"
            )),
            "profile-2 process preflight requires little-endian x86-64 Linux"
        );
        ensure!(
            !request.transaction_timeout.is_zero()
                && request.transaction_timeout
                    <= bladerf_profile2_session_ipc::MAX_TRANSACTION_TIMEOUT,
            "profile-2 transaction timeout must be positive and no greater than {:?}",
            bladerf_profile2_session_ipc::MAX_TRANSACTION_TIMEOUT
        );
        ensure_preflight_running(stop, "before path access")?;
        ensure!(
            exact_lower_hex(request.serial, 32),
            "bladeRF serial must be exactly 32 lowercase hexadecimal characters"
        );
        for (label, digest) in [
            ("adapter", request.adapter_sha256),
            ("library", request.library_sha256),
            ("manifest", request.manifest_sha256),
            ("stock", stock.sha256),
        ] {
            ensure!(
                exact_lower_hex(digest, 64),
                "{label} SHA-256 must be exactly 64 lowercase hexadecimal characters"
            );
        }
        ensure!(stock.bytes > 0, "stock image byte count must be positive");
        for (label, path) in [
            ("workspace root", request.workspace_root),
            ("manifest", request.manifest),
            ("adapter", request.adapter),
            ("library", request.library),
            ("stock image", request.stock_image),
            ("staging parent", request.stage_parent),
        ] {
            ensure!(
                path.is_absolute() && !path.as_os_str().is_empty(),
                "{label} path must be absolute and nonempty"
            );
        }

        let usbfs_memory_mb = checks
            .check_usbfs()
            .context("qualify the current boot usbfs transfer quota")?;
        ensure!(
            usbfs_memory_mb >= MIN_USBFS_MEMORY_MB,
            "usbfs memory quota is below the {MIN_USBFS_MEMORY_MB}-MiB process minimum"
        );
        ensure_preflight_running(stop, "before custom-image verification")?;
        let custom = checks.verify_custom(
            request.workspace_root,
            request.manifest,
            request.manifest_sha256,
            stop,
        )?;
        ensure!(
            custom.path.is_absolute() && exact_lower_hex(&custom.sha256, 64) && custom.bytes > 0,
            "verified custom image facts are malformed"
        );

        let directory = create_input_directory(request.stage_parent)?;
        let adapter = stage_pinned_exact(
            request.adapter,
            &directory.path().join("profile2-adapter"),
            request.adapter_sha256,
            None,
            MAX_ADAPTER_BYTES,
            ADAPTER_MODE,
            stop,
        )?;
        let library = stage_pinned_exact(
            request.library,
            &directory.path().join("libbladeRF.so.2"),
            request.library_sha256,
            None,
            MAX_LIBRARY_BYTES,
            INPUT_MODE,
            stop,
        )?;
        let custom_input = stage_pinned_exact(
            &custom.path,
            &directory.path().join("profile2.rbf"),
            &custom.sha256,
            Some(custom.bytes),
            custom.bytes,
            INPUT_MODE,
            stop,
        )?;
        let stock_input = stage_pinned_exact(
            request.stock_image,
            &directory.path().join("stock.rbf"),
            stock.sha256,
            Some(stock.bytes),
            stock.bytes,
            INPUT_MODE,
            stop,
        )?;
        let inputs = InputStage {
            directory,
            adapter,
            library,
            custom: custom_input,
            stock: stock_input,
        };
        File::open(inputs.directory.path())?.sync_all()?;
        inputs.revalidate_all(stop)?;
        checks.check_library(
            inputs.adapter.path(),
            inputs.library.path(),
            request.library_sha256,
            inputs.directory.path(),
            request.transaction_timeout,
            stop,
        )?;
        ensure_preflight_running(stop, "after loader-neutral library check")?;
        inputs.revalidate_all(stop)?;

        let config = Self {
            inputs,
            adapter_sha256: request.adapter_sha256.to_owned(),
            library_sha256: request.library_sha256.to_owned(),
            custom_sha256: custom.sha256,
            custom_bytes: custom.bytes,
            manifest_sha256: request.manifest_sha256.to_owned(),
            usbfs_memory_mb,
            serial: request.serial.to_owned(),
            transaction_timeout: request.transaction_timeout,
            stop: Arc::clone(stop),
        };
        config.session_bundle()?;
        Ok(config)
    }

    fn session_bundle(&self) -> Result<BundleFacts> {
        BundleFacts::new(
            LibraryFacts::new(
                REQUIRED_LIBRARY_VERSION,
                &self.library_sha256,
                REQUIRED_LIBRARY_MARKER,
            ),
            ImageFacts::new(&self.custom_sha256, self.custom_bytes),
            ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
        )
        .map_err(Error::new)
        .context("construct exact native primary bundle")
    }

    fn primary_arguments(&self) -> Vec<OsString> {
        native_primary_arguments(
            self.inputs.library.path(),
            &self.library_sha256,
            self.inputs.custom.path(),
            &self.custom_sha256,
            self.custom_bytes,
            self.inputs.stock.path(),
            &self.serial,
        )
    }

    fn revalidate_primary(&self, stop: &AtomicBool) -> Result<()> {
        self.inputs.revalidate_primary(stop)
    }

    fn revalidate_recovery(&self, stop: &AtomicBool) -> Result<()> {
        self.inputs.revalidate_recovery(stop)
    }
}

fn require_unwind_profile() -> Result<()> {
    ensure!(
        cfg!(panic = "unwind"),
        "profile-2 process sessions require panic=unwind for guarded recovery"
    );
    Ok(())
}

fn exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_preflight_running(stop: &AtomicBool, stage: &str) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 process preflight was interrupted {stage}"
    );
    Ok(())
}

fn create_input_directory(parent: &Path) -> Result<TempDir> {
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect profile-2 staging parent {}", parent.display()))?;
    ensure!(
        metadata.file_type().is_dir() && metadata.permissions().mode() & 0o7777 == 0o700,
        "profile-2 staging parent must be a real mode-0700 directory"
    );
    ensure!(
        parent.canonicalize()? == parent,
        "profile-2 staging parent must be an absolute canonical path without symlinks"
    );
    let directory = tempfile::Builder::new()
        .prefix("profile2-inputs-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .with_context(|| format!("create private input stage in {}", parent.display()))?;
    let stage_metadata = fs::symlink_metadata(directory.path())?;
    ensure!(
        directory.path().is_absolute()
            && stage_metadata.file_type().is_dir()
            && stage_metadata.permissions().mode() & 0o7777 == 0o700,
        "profile-2 input stage must be a real absolute mode-0700 directory"
    );
    Ok(directory)
}

fn hash_reader<R: Read>(reader: &mut R, maximum: u64, stop: &AtomicBool) -> Result<(String, u64)> {
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_preflight_running(stop, "while hashing an input")?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .context("profile-2 staged input byte count overflow")?;
        ensure!(
            bytes <= maximum,
            "profile-2 staged input exceeded its {maximum}-byte bound"
        );
        hash.update(&buffer[..count]);
    }
    ensure_preflight_running(stop, "after hashing an input")?;
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn stage_pinned_exact(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    expected_bytes: Option<u64>,
    maximum: u64,
    mode: u32,
    stop: &AtomicBool,
) -> Result<StagedInput> {
    ensure!(
        exact_lower_hex(expected_sha256, 64),
        "staged input SHA-256 must be exactly 64 lowercase hexadecimal characters"
    );
    ensure!(
        maximum > 0 && matches!(mode, ADAPTER_MODE | INPUT_MODE),
        "staged input bound and mode must be fixed and nonzero"
    );
    if let Some(expected) = expected_bytes {
        ensure!(
            expected > 0 && expected <= maximum,
            "staged input exact byte count exceeds its bound"
        );
    }
    ensure_preflight_running(stop, "before opening an input")?;
    let (mut input, pinned_bytes) = crate::file_pin::open_regular(source, maximum)?;
    if let Some(expected) = expected_bytes {
        ensure!(
            pinned_bytes == expected,
            "{} has {pinned_bytes} bytes; expected {expected}",
            source.display()
        );
    }

    let mut output = crate::new_file(destination)?;
    let mut hash = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_preflight_running(stop, "while staging an input")?;
        let count = input
            .read(&mut buffer)
            .with_context(|| format!("read pinned input {}", source.display()))?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(u64::try_from(count)?)
            .context("profile-2 staged copy byte count overflow")?;
        ensure!(
            copied <= maximum,
            "{} grew beyond its {maximum}-byte staging bound",
            source.display()
        );
        hash.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .with_context(|| format!("write staged input {}", destination.display()))?;
    }
    ensure_preflight_running(stop, "after staging an input")?;
    let sha256 = format!("{:x}", hash.finalize());
    ensure!(
        copied == pinned_bytes
            && input.metadata()?.len() == pinned_bytes
            && sha256 == expected_sha256,
        "{} changed or differs from its separately trusted identity",
        source.display()
    );
    output.flush()?;
    output.sync_all()?;
    output.set_permissions(fs::Permissions::from_mode(mode))?;
    output.sync_all()?;
    drop(output);

    let (file, staged_bytes) = crate::file_pin::open_regular(destination, maximum)?;
    ensure!(
        staged_bytes == copied,
        "staged input extent changed before it was retained"
    );
    let descriptor = file.metadata()?;
    let path = fs::symlink_metadata(destination)?;
    ensure!(
        descriptor.file_type().is_file()
            && path.file_type().is_file()
            && descriptor.dev() == path.dev()
            && descriptor.ino() == path.ino()
            && descriptor.len() == copied
            && path.len() == copied
            && descriptor.nlink() == 1
            && path.nlink() == 1
            && descriptor.permissions().mode() & 0o7777 == mode
            && path.permissions().mode() & 0o7777 == mode,
        "staged input descriptor/path identity is not private and exact"
    );
    let staged = StagedInput {
        file,
        path: destination.to_path_buf(),
        device: descriptor.dev(),
        inode: descriptor.ino(),
        bytes: copied,
        sha256,
        mode,
    };
    staged.revalidate(stop)?;
    Ok(staged)
}

/// Owns the authenticated, childless ABI checker process.
///
/// Drop kills and waits for the leader and can therefore block in operating-
/// system process teardown. The qualified `check-native-library` action does
/// not create descendants; a future broader checker must use process-group
/// cleanup before it can replace this exact action.
struct PreflightChild {
    child: Child,
    reaped: bool,
}

impl PreflightChild {
    fn kill_and_reap(&mut self) -> Result<()> {
        if self.reaped {
            return Ok(());
        }
        let _ = self.child.kill();
        self.child.wait()?;
        self.reaped = true;
        Ok(())
    }
}

impl Drop for PreflightChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

fn run_device_free_library_check(
    adapter: &Path,
    library: &Path,
    library_sha256: &str,
    working_directory: &Path,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<()> {
    ensure_preflight_running(stop, "before the loader-neutral library check")?;
    let child = Command::new(adapter)
        .arg("check-native-library")
        .arg("--library")
        .arg(library)
        .arg("--sha256")
        .arg(library_sha256)
        .arg("--require-bladerf2-loader-neutral-open")
        .current_dir(working_directory)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start staged device-free loader-neutral library check")?;
    let mut child = PreflightChild {
        child,
        reaped: false,
    };
    let started = Instant::now();
    loop {
        if let Some(status) = child.child.try_wait()? {
            child.reaped = true;
            ensure_preflight_running(stop, "after the loader-neutral library check")?;
            ensure!(
                status.success(),
                "staged device-free loader-neutral library check exited with {status}"
            );
            return Ok(());
        }
        if stop.load(Ordering::Relaxed) || started.elapsed() >= timeout {
            child
                .kill_and_reap()
                .context("kill and reap staged loader-neutral library check")?;
            anyhow::bail!(
                "staged loader-neutral library check was cancelled or exceeded {:?}",
                timeout
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// File sink retained by the parent process and never named on the wire.
///
/// The I/O worker checks cancellation at each write boundary. An interrupted
/// write is a capture failure, which terminalizes the primary client and makes
/// the coordinator choose the fresh-recovery path.
struct CaptureSink {
    file: File,
    stage_path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl CaptureSink {
    fn new(file: File, stage_path: PathBuf, stop: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            stage_path.is_absolute(),
            "profile-2 capture stage path must be absolute"
        );
        ensure!(
            stage_path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".partial")),
            "profile-2 capture stage must use a .partial name"
        );
        let file_metadata = file
            .metadata()
            .context("inspect parent-owned profile-2 capture file")?;
        let path_metadata = fs::symlink_metadata(&stage_path).with_context(|| {
            format!(
                "inspect parent-owned profile-2 capture path {}",
                stage_path.display()
            )
        })?;
        ensure!(
            path_metadata.file_type().is_file()
                && file_metadata.dev() == path_metadata.dev()
                && file_metadata.ino() == path_metadata.ino()
                && file_metadata.len() == 0
                && path_metadata.len() == 0,
            "profile-2 capture descriptor and empty regular stage path must identify the same inode"
        );
        ensure!(
            file_metadata.permissions().mode() & 0o777 == 0o600,
            "profile-2 capture stage must have mode 0600"
        );
        let parent = stage_path
            .parent()
            .context("profile-2 capture stage has no parent directory")?;
        let parent_metadata = fs::symlink_metadata(parent)
            .with_context(|| format!("inspect profile-2 capture directory {}", parent.display()))?;
        ensure!(
            parent_metadata.file_type().is_dir()
                && parent_metadata.permissions().mode() & 0o777 == 0o700,
            "profile-2 capture directory must be a real mode-0700 directory"
        );
        Ok(Self {
            file,
            stage_path,
            stop,
        })
    }

    fn check_interrupted(&self) -> io::Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            // Write::write_all retries Interrupted forever. Use a terminal sink
            // error so cancellation reaches SessionClient and kills the child.
            Err(io::Error::other("profile-2 capture sink was cancelled"))
        } else {
            Ok(())
        }
    }
}

impl Write for CaptureSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check_interrupted()?;
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_interrupted()?;
        self.file.flush()?;
        self.file.sync_all()
    }
}

/// Capture evidence returned by the primary process before strict parsing.
struct ProcessCapture {
    sink: CaptureSink,
    report: CaptureReport,
    configured: ConfiguredReceiveReadback,
    active: ActiveReceiveReadback,
}

/// Strictly validated process evidence retained for descriptor publication.
///
/// This is not published evidence until `SessionPublication` links the exact
/// retained inode no-clobber and writes the sole terminal metadata last.
struct ProcessSummary {
    plan: SessionPlan,
    applied: AppliedConfiguration,
    readback: ReceiveReadback,
    configured: ConfiguredReceiveReadback,
    active: ActiveReceiveReadback,
    report: CaptureReport,
    stage: ValidatedStage,
    stream: ValidatedStream,
}

/// Descriptor-pinned validated stage retained until publication.
///
/// Publication links from this descriptor and treats `path` only as a mutable
/// namespace fact that must still match before the commit boundary.
struct ValidatedStage {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            mode: metadata.permissions().mode() & 0o7777,
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_file()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.len() == self.bytes
            && metadata.permissions().mode() & 0o7777 == self.mode
    }
}

/// Descriptor-pinned private directory.
///
/// Path revalidation detects same-UID namespace replacement before a result is
/// accepted. Entry operations still use `file` as their dirfd. A hostile
/// same-UID process that can mutate the trusted parent, fatal signals, power
/// loss, and a failed directory fsync remain outside this guarantee.
struct PinnedDirectory {
    file: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl PinnedDirectory {
    fn open(path: &Path, require_canonical: bool) -> Result<Self> {
        ensure!(
            path.is_absolute() && !path.as_os_str().is_empty(),
            "pinned directory path must be absolute and nonempty"
        );
        if require_canonical {
            ensure!(
                path.canonicalize()? == path,
                "pinned directory must be an absolute canonical path without symlinks: {}",
                path.display()
            );
        }
        let path_metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect pinned directory {}", path.display()))?;
        ensure!(
            path_metadata.file_type().is_dir()
                && path_metadata.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE,
            "pinned directory must be a real mode-0700 directory: {}",
            path.display()
        );
        let descriptor = openat(
            AT_FDCWD,
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("open pinned directory {}", path.display()))?;
        let file = File::from(descriptor);
        let descriptor_metadata = file.metadata()?;
        ensure!(
            descriptor_metadata.file_type().is_dir()
                && descriptor_metadata.dev() == path_metadata.dev()
                && descriptor_metadata.ino() == path_metadata.ino()
                && descriptor_metadata.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE,
            "pinned directory descriptor/path identity changed: {}",
            path.display()
        );
        Ok(Self {
            file,
            path: path.to_path_buf(),
            device: descriptor_metadata.dev(),
            inode: descriptor_metadata.ino(),
        })
    }

    fn create_child(parent: &Self, basename: &OsStr) -> Result<Self> {
        validate_basename(basename)?;
        parent.revalidate_path()?;
        mkdirat(
            &parent.file,
            basename,
            Mode::from_bits_truncate(OUTPUT_DIRECTORY_MODE),
        )
        .with_context(|| {
            format!(
                "create private publication directory {}",
                parent.path.join(basename).display()
            )
        })?;
        parent.file.sync_all()?;
        let path = parent.path.join(basename);
        ensure!(
            path.canonicalize()? == path,
            "new publication directory path was not canonical"
        );
        let descriptor = openat(
            &parent.file,
            basename,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .context("pin new publication directory relative to its trusted parent")?;
        let file = File::from(descriptor);
        let descriptor_metadata = file.metadata()?;
        let path_metadata = fs::symlink_metadata(&path)?;
        ensure!(
            descriptor_metadata.file_type().is_dir()
                && path_metadata.file_type().is_dir()
                && descriptor_metadata.dev() == path_metadata.dev()
                && descriptor_metadata.ino() == path_metadata.ino()
                && descriptor_metadata.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE
                && path_metadata.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE,
            "new publication directory descriptor/path identity or mode changed"
        );
        let directory = Self {
            file,
            path,
            device: descriptor_metadata.dev(),
            inode: descriptor_metadata.ino(),
        };
        ensure!(
            directory.is_empty()?,
            "new publication directory was not empty"
        );
        parent.revalidate_path()?;
        Ok(directory)
    }

    fn revalidate_path(&self) -> Result<()> {
        let descriptor = self.file.metadata()?;
        let path = fs::symlink_metadata(&self.path)
            .with_context(|| format!("reinspect pinned directory {}", self.path.display()))?;
        ensure!(
            descriptor.file_type().is_dir()
                && path.file_type().is_dir()
                && descriptor.dev() == self.device
                && descriptor.ino() == self.inode
                && path.dev() == self.device
                && path.ino() == self.inode
                && descriptor.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE
                && path.permissions().mode() & 0o7777 == OUTPUT_DIRECTORY_MODE,
            "pinned directory path identity or mode changed: {}",
            self.path.display()
        );
        Ok(())
    }

    fn is_empty(&self) -> Result<bool> {
        let proc_path = proc_fd_path(&self.file);
        Ok(fs::read_dir(&proc_path)?.next().transpose()?.is_none())
    }

    fn open_entry(&self, basename: &OsStr) -> Result<File> {
        validate_basename(basename)?;
        let descriptor = openat(
            &self.file,
            basename,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?;
        Ok(File::from(descriptor))
    }
}

fn validate_basename(basename: &OsStr) -> Result<()> {
    let mut components = Path::new(basename).components();
    ensure!(
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none(),
        "publication entry name must be one normal relative component"
    );
    Ok(())
}

fn proc_fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn reopen_read_only_from_fd(file: &File) -> Result<File> {
    let source_metadata = file.metadata()?;
    ensure!(
        source_metadata.file_type().is_file(),
        "descriptor to reopen must identify a regular file"
    );
    let path = proc_fd_path(file);
    let reopened = File::open(&path)
        .with_context(|| format!("reopen retained inode read-only through {}", path.display()))?;
    let reopened_metadata = reopened.metadata()?;
    ensure!(
        reopened_metadata.dev() == source_metadata.dev()
            && reopened_metadata.ino() == source_metadata.ino()
            && reopened_metadata.len() == source_metadata.len(),
        "read-only descriptor reopen changed inode identity or extent"
    );
    Ok(reopened)
}

fn link_fd_no_clobber(
    source: &File,
    destination: &PinnedDirectory,
    basename: &OsStr,
) -> Result<FileIdentity> {
    validate_basename(basename)?;
    let metadata = source.metadata()?;
    ensure!(
        metadata.file_type().is_file(),
        "publication source descriptor must be a regular file"
    );
    ensure!(
        metadata.dev() == destination.device,
        "publication source and destination must be on the same filesystem"
    );
    let identity = FileIdentity::from_metadata(&metadata);
    let source_path = proc_fd_path(source);
    linkat(
        AT_FDCWD,
        source_path.as_path(),
        &destination.file,
        basename,
        AtFlags::AT_SYMLINK_FOLLOW,
    )
    .with_context(|| {
        format!(
            "descriptor-link retained inode as {} without replacement",
            basename.to_string_lossy()
        )
    })?;
    Ok(identity)
}

fn verify_link_at(
    directory: &PinnedDirectory,
    basename: &OsStr,
    expected: FileIdentity,
    expected_links: u64,
) -> Result<File> {
    let file = directory.open_entry(basename).with_context(|| {
        format!(
            "open published entry {} relative to pinned directory",
            basename.to_string_lossy()
        )
    })?;
    let metadata = file.metadata()?;
    ensure!(
        expected.matches(&metadata) && metadata.nlink() == expected_links,
        "published entry identity, extent, mode or link count differs from its staged descriptor"
    );
    Ok(file)
}

fn guarded_unlink_at(
    directory: &PinnedDirectory,
    basename: &OsStr,
    expected: FileIdentity,
    expected_links: u64,
) -> Result<()> {
    let file = directory
        .open_entry(basename)
        .with_context(|| format!("open guarded-unlink entry {}", basename.to_string_lossy()))?;
    let metadata = file.metadata()?;
    ensure!(
        expected.matches(&metadata) && metadata.nlink() == expected_links,
        "refusing to unlink a replaced or changed staged entry"
    );
    unlinkat(&directory.file, basename, UnlinkatFlags::NoRemoveDir)
        .context("remove exactly validated staged entry")?;
    directory.file.sync_all()?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureDisposition {
    None,
    PartialRetained,
    AcceptedUnpublished,
    Published,
    PublishedDurabilityUnconfirmed,
}

impl CaptureDisposition {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PartialRetained => "partial_retained",
            Self::AcceptedUnpublished => "accepted_unpublished",
            Self::Published => "published",
            Self::PublishedDurabilityUnconfirmed => "published_durability_unconfirmed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PublishedFile {
    identity: FileIdentity,
    basename: &'static str,
}

#[derive(Debug)]
struct PublishedSession {
    output: PathBuf,
    capture: PublishedFile,
    terminal: PublishedFile,
}

#[derive(Clone)]
struct PublicationPreflightFacts {
    adapter_sha256: String,
    adapter_bytes: u64,
    library_sha256: String,
    library_bytes: u64,
    manifest_sha256: String,
    custom_sha256: String,
    custom_bytes: u64,
    stock_sha256: &'static str,
    stock_bytes: u64,
    usbfs_memory_mb: u64,
    serial_selected_exactly: bool,
}

impl PublicationPreflightFacts {
    fn from_config(config: &ProcessConfig) -> Self {
        Self {
            adapter_sha256: config.adapter_sha256.clone(),
            adapter_bytes: config.inputs.adapter.bytes,
            library_sha256: config.library_sha256.clone(),
            library_bytes: config.inputs.library.bytes,
            manifest_sha256: config.manifest_sha256.clone(),
            custom_sha256: config.custom_sha256.clone(),
            custom_bytes: config.custom_bytes,
            stock_sha256: REQUIRED_STOCK_SHA256,
            stock_bytes: REQUIRED_STOCK_BYTES,
            usbfs_memory_mb: config.usbfs_memory_mb,
            serial_selected_exactly: exact_lower_hex(&config.serial, 32),
        }
    }
}

struct PublicationFailure {
    source: Error,
    terminal_error: Option<Error>,
    capture: CaptureDisposition,
    capture_facts: Value,
    final_capture_basename: Option<&'static str>,
    terminal_linked: bool,
    terminal_durable: bool,
}

impl fmt::Debug for PublicationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicationFailure")
            .field("source", &format_args!("{:#}", self.source))
            .field(
                "terminal_error",
                &self
                    .terminal_error
                    .as_ref()
                    .map(|error| format!("{error:#}")),
            )
            .field("capture", &self.capture)
            .field("capture_facts", &self.capture_facts)
            .field("final_capture_basename", &self.final_capture_basename)
            .field("terminal_linked", &self.terminal_linked)
            .field("terminal_durable", &self.terminal_durable)
            .finish()
    }
}

impl fmt::Display for PublicationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "profile-2 publication failed; capture={}; terminal_linked={}; terminal_durable={}: {:#}",
            self.capture.as_str(),
            self.terminal_linked,
            self.terminal_durable,
            self.source
        )?;
        if let Some(retained_stage_directory) = self
            .capture_facts
            .get("retained_stage_directory")
            .and_then(Value::as_str)
        {
            write!(
                formatter,
                "; retained_stage_directory={retained_stage_directory}"
            )?;
        }
        if let Some(error) = &self.terminal_error {
            write!(formatter, "; terminalization ambiguity: {error:#}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Failure from the complete private preflight/run/publication composition.
///
/// `Preflight` happens before the output namespace exists. `PublicationSetup`
/// means no usable terminalizer was returned, so no terminal record is
/// promised and setup may have created an empty output before failing. Once a
/// publisher exists, every later returned error is retained as `Publication`
/// with its capture and terminal durability facts.
enum ProcessSessionFailure {
    Preflight(Error),
    PublicationSetup(Error),
    Publication(PublicationFailure),
}

impl fmt::Debug for ProcessSessionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight(error) => formatter
                .debug_tuple("Preflight")
                .field(&format_args!("{error:#}"))
                .finish(),
            Self::PublicationSetup(error) => formatter
                .debug_tuple("PublicationSetup")
                .field(&format_args!("{error:#}"))
                .finish(),
            Self::Publication(error) => formatter.debug_tuple("Publication").field(error).finish(),
        }
    }
}

impl fmt::Display for ProcessSessionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight(error) => write!(
                formatter,
                "profile-2 session preflight failed before publication: {error:#}"
            ),
            Self::PublicationSetup(error) => write!(
                formatter,
                "profile-2 publication setup failed before terminal authority; an empty requested output directory may remain and no terminal record is guaranteed: {error:#}"
            ),
            Self::Publication(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for ProcessSessionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Preflight(error) | Self::PublicationSetup(error) => Some(error.as_ref()),
            Self::Publication(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationStep {
    BeforeCaptureCommit,
    CaptureCommitted,
    BeforeTerminalCommit,
    TerminalCommitted,
}

trait PublicationHooks {
    fn reached(&mut self, _step: PublicationStep, _stop: &Arc<AtomicBool>) -> Result<()> {
        Ok(())
    }
}

struct NoPublicationHooks;

impl PublicationHooks for NoPublicationHooks {}

/// Owns the independent read-only capture descriptor while the writer is lent
/// to the consuming process factory.
struct CaptureStageGuard {
    owner: Option<TempDir>,
    directory: PinnedDirectory,
    writer: Option<File>,
    reader: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl CaptureStageGuard {
    fn create(parent: &PinnedDirectory, output: &PinnedDirectory) -> Result<Self> {
        parent.revalidate_path()?;
        let owner = tempfile::Builder::new()
            .prefix("profile2-capture-")
            .permissions(fs::Permissions::from_mode(OUTPUT_DIRECTORY_MODE))
            .tempdir_in(&parent.path)
            .with_context(|| {
                format!(
                    "create private capture stage beside {}",
                    output.path.display()
                )
            })?;
        fs::set_permissions(
            owner.path(),
            fs::Permissions::from_mode(OUTPUT_DIRECTORY_MODE),
        )?;
        parent.file.sync_all()?;
        parent.revalidate_path()?;
        let directory = PinnedDirectory::open(owner.path(), true)?;
        ensure!(
            directory.device == output.device,
            "capture stage and publication output must be on the same filesystem"
        );
        let descriptor = openat(
            &directory.file,
            CAPTURE_STAGE_NAME,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(PUBLICATION_FILE_MODE),
        )
        .context("create private capture stage file")?;
        let writer = File::from(descriptor);
        writer.set_permissions(fs::Permissions::from_mode(PUBLICATION_FILE_MODE))?;
        let reader = reopen_read_only_from_fd(&writer)?;
        let metadata = reader.metadata()?;
        let path = directory.path.join(CAPTURE_STAGE_NAME);
        let path_metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.file_type().is_file()
                && metadata.dev() == path_metadata.dev()
                && metadata.ino() == path_metadata.ino()
                && metadata.len() == 0
                && path_metadata.len() == 0
                && metadata.nlink() == 1
                && path_metadata.nlink() == 1
                && metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE
                && path_metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE,
            "new capture stage is not one private empty mode-0600 inode"
        );
        directory.file.sync_all()?;
        Ok(Self {
            owner: Some(owner),
            directory,
            writer: Some(writer),
            reader,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn take_sink(&mut self, stop: Arc<AtomicBool>) -> Result<CaptureSink> {
        let writer = self
            .writer
            .take()
            .context("profile-2 capture writer was already transferred")?;
        CaptureSink::new(writer, self.path.clone(), stop)
    }

    fn validate_accepted(
        &self,
        summary: &ProcessSummary,
        stop: &AtomicBool,
    ) -> Result<FileIdentity> {
        ensure_preflight_running(stop, "before accepted-capture publication")?;
        self.directory.revalidate_path()?;
        ensure!(
            self.writer.is_none(),
            "capture writer remained owned after the process session"
        );
        let descriptor_before = self.reader.metadata()?;
        let summary_descriptor = summary.stage.file.metadata()?;
        let path = fs::symlink_metadata(&self.path)
            .with_context(|| format!("revalidate capture stage path {}", self.path.display()))?;
        let expected = FileIdentity {
            device: self.device,
            inode: self.inode,
            bytes: summary.report.raw_bytes(),
            mode: PUBLICATION_FILE_MODE,
        };
        ensure!(
            summary.stage.path == self.path
                && summary.stage.device == self.device
                && summary.stage.inode == self.inode
                && summary.stage.bytes == expected.bytes
                && expected.matches(&descriptor_before)
                && expected.matches(&summary_descriptor)
                && expected.matches(&path)
                && descriptor_before.nlink() == 1
                && summary_descriptor.nlink() == 1
                && path.nlink() == 1,
            "accepted capture descriptor/path identity, extent, mode or link count changed"
        );

        let mut reader = self.reader.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let (sha256, bytes) = hash_reader(&mut reader, expected.bytes, stop)
            .context("rehash accepted capture through its retained descriptor")?;
        let descriptor_after = self.reader.metadata()?;
        let path_after = fs::symlink_metadata(&self.path)?;
        ensure!(
            sha256 == summary.report.sha256()
                && sha256 == summary.stream.processed_sha256
                && bytes == expected.bytes
                && expected.matches(&descriptor_after)
                && expected.matches(&path_after)
                && descriptor_after.nlink() == 1
                && path_after.nlink() == 1
                && descriptor_after.mtime() == descriptor_before.mtime()
                && descriptor_after.mtime_nsec() == descriptor_before.mtime_nsec()
                && descriptor_after.ctime() == descriptor_before.ctime()
                && descriptor_after.ctime_nsec() == descriptor_before.ctime_nsec(),
            "accepted capture changed while it was rehashed"
        );
        ensure_preflight_running(stop, "immediately before capture commit")?;
        self.reader.sync_all()?;
        Ok(expected)
    }

    fn partial_facts(&self) -> Value {
        match self.reader.metadata() {
            Ok(metadata) => {
                let path_matches = fs::symlink_metadata(&self.path).is_ok_and(|path| {
                    path.file_type().is_file()
                        && path.dev() == metadata.dev()
                        && path.ino() == metadata.ino()
                        && path.len() == metadata.len()
                        && path.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE
                });
                json!({
                    "device": metadata.dev(),
                    "inode": metadata.ino(),
                    "bytes": metadata.len(),
                    "mode": format!("{:04o}", metadata.permissions().mode() & 0o7777),
                    "links": metadata.nlink(),
                    "stage_basename": path_matches.then_some(CAPTURE_STAGE_NAME),
                    "path_identity_verified": path_matches,
                })
            }
            Err(error) => json!({"inspection_error": bounded_text(&format!("{error:#}"))}),
        }
    }

    fn preserve(&mut self) -> Option<String> {
        self.owner.take().and_then(|owner| {
            let path = owner.keep();
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
    }
}

fn bounded_text(text: &str) -> String {
    if text.len() <= MAX_FAILURE_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_FAILURE_TEXT_BYTES - 3;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

fn restoration_label(restoration: super::RestorationOutcome) -> &'static str {
    match restoration {
        super::RestorationOutcome::NotRequired => "not_required",
        super::RestorationOutcome::SameHandleVerified => "same_handle_verified",
        super::RestorationOutcome::FreshWorkerVerified => "fresh_worker_verified",
        super::RestorationOutcome::Unknown => "unknown",
    }
}

fn session_failure_diagnostics(failure: &super::SessionFailure) -> Value {
    fn diagnostic(error: Option<&Error>) -> Value {
        error.map_or(Value::Null, |error| {
            json!(bounded_text(&format!("{error:#}")))
        })
    }
    json!({
        "primary": diagnostic(failure.primary.as_ref()),
        "same_handle_restoration": diagnostic(failure.same_handle_restore.as_ref()),
        "fresh_recovery": diagnostic(failure.fresh_recovery.as_ref()),
        "primary_cleanup": diagnostic(failure.primary_cleanup.as_ref()),
        "recovery_cleanup": diagnostic(failure.recovery_cleanup.as_ref()),
        "primary_cleanup_uncertain": failure.primary_cleanup.is_some(),
        "recovery_cleanup_uncertain": failure.recovery_cleanup.is_some(),
    })
}

fn plan_terminal_facts(plan: SessionPlan) -> Value {
    json!({
        "receiver": plan.request.receiver().to_string(),
        "center_frequency_hz": plan.center_hz,
        "gain_db": plan.gain_db,
        "data_records": plan.records,
        "max_status_polls": plan.max_status_polls,
        "phase_words": plan.request.phase_words(),
    })
}

fn preflight_terminal_facts(preflight: &PublicationPreflightFacts) -> Value {
    json!({
        "adapter": {
            "sha256": preflight.adapter_sha256,
            "bytes": preflight.adapter_bytes,
        },
        "library": {
            "sha256": preflight.library_sha256,
            "bytes": preflight.library_bytes,
        },
        "manifest_sha256": preflight.manifest_sha256,
        "custom_image": {
            "sha256": preflight.custom_sha256,
            "bytes": preflight.custom_bytes,
        },
        "stock_image": {
            "sha256": preflight.stock_sha256,
            "bytes": preflight.stock_bytes,
        },
        "usbfs_memory_mb": preflight.usbfs_memory_mb,
        "serial_selected_exactly": preflight.serial_selected_exactly,
    })
}

fn success_terminal_report(
    success: &super::SessionSuccess<ProcessSummary>,
    capture: PublishedFile,
    started_utc: &str,
    preflight: &PublicationPreflightFacts,
) -> Value {
    let summary = &success.summary;
    let status = summary.stream.config.status;
    json!({
        "schema_version": 1,
        "outcome": "success",
        "started_utc": started_utc,
        "finished_utc": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "restoration": restoration_label(success.restoration),
        "board_state_known_stock": matches!(
            success.restoration,
            super::RestorationOutcome::SameHandleVerified
                | super::RestorationOutcome::FreshWorkerVerified
        ),
        "safety": {
            "tx_stream_enabled": false,
            "flash_written": false,
        },
        "preflight": preflight_terminal_facts(preflight),
        "plan": plan_terminal_facts(summary.plan),
        "applied": {
            "epoch": success.applied.epoch(),
            "phase_words": success.applied.phase_words(),
        },
        "coordinator_readback": {
            "verified": success.readback == summary.readback,
            "receiver": summary.plan.request.receiver().to_string(),
            "sample_rate_hz": summary.configured.sample_rate_hz(),
        },
        "configured_receive": summary.configured,
        "active_receive": summary.active,
        "capture_report": summary.report,
        "validated_stream": {
            "config": {
                "epoch": summary.stream.config.epoch,
                "required_source_sample_rate_hz": summary.stream.config.required_source_sample_rate_hz,
                "phase_words": summary.stream.config.phase_words,
                "status": {
                    "clipped_channels": status.clipped_channels,
                    "loss": status.loss,
                    "configuration_error": status.configuration_error,
                    "rx2": status.rx2,
                    "epoch_tag": status.epoch_tag,
                },
            },
            "data_records": summary.stream.data_records,
            "first_source_index": summary.stream.first_source_index,
            "last_source_index": summary.stream.last_source_index,
            "processed_bytes": summary.stream.processed_bytes,
            "processed_sha256": summary.stream.processed_sha256,
        },
        "capture_file": {
            "basename": capture.basename,
            "device": capture.identity.device,
            "inode": capture.identity.inode,
            "bytes": capture.identity.bytes,
            "mode": format!("{:04o}", capture.identity.mode),
        },
    })
}

struct SessionPublication {
    output: PinnedDirectory,
    stage: CaptureStageGuard,
    stop: Arc<AtomicBool>,
    requested_plan: SessionPlan,
    started_utc: String,
    preflight: Option<PublicationPreflightFacts>,
    capture_disposition: CaptureDisposition,
    capture: Option<PublishedFile>,
    capture_durable: bool,
    terminal_linked: bool,
    terminal_durable: bool,
}

impl SessionPublication {
    fn create(
        trusted_parent: &Path,
        output_basename: &OsStr,
        requested_plan: SessionPlan,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        require_unwind_profile()?;
        ensure_preflight_running(&stop, "before publication setup")?;
        let parent = PinnedDirectory::open(trusted_parent, true)
            .context("pin trusted private publication parent")?;
        let output = PinnedDirectory::create_child(&parent, output_basename)?;
        let stage = CaptureStageGuard::create(&parent, &output)?;
        ensure!(
            stage.directory.device == output.device,
            "capture stage and output are not on the same filesystem"
        );
        output.file.sync_all()?;
        Ok(Self {
            output,
            stage,
            stop,
            requested_plan,
            started_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            preflight: None,
            capture_disposition: CaptureDisposition::None,
            capture: None,
            capture_durable: false,
            terminal_linked: false,
            terminal_durable: false,
        })
    }

    fn bind_preflight(&mut self, config: &ProcessConfig) -> Result<()> {
        ensure!(
            self.preflight.is_none(),
            "publication preflight facts were already bound"
        );
        ensure!(
            Arc::ptr_eq(&self.stop, &config.stop),
            "publication facts and process config must share the exact cancellation token"
        );
        self.preflight = Some(PublicationPreflightFacts::from_config(config));
        Ok(())
    }

    fn capture_facts(&self, retained_stage_directory: Option<&str>) -> Value {
        let mut facts = self.stage.partial_facts();
        let object = facts
            .as_object_mut()
            .expect("partial capture facts are always a JSON object");
        object.insert(
            "disposition".to_owned(),
            json!(self.capture_disposition.as_str()),
        );
        object.insert(
            "retained_stage_directory".to_owned(),
            json!(retained_stage_directory),
        );
        object.insert(
            "durability_confirmed".to_owned(),
            json!(self.capture_durable),
        );
        object.insert(
            "final_basename".to_owned(),
            json!(self.capture.map(|capture| capture.basename)),
        );
        facts
    }

    fn stage_terminal(
        &self,
        report: &Value,
        basename: &'static str,
    ) -> Result<(File, FileIdentity)> {
        validate_basename(OsStr::new(basename))?;
        self.stage.directory.revalidate_path()?;
        let mut bytes =
            serde_json::to_vec(report).context("serialize profile-2 terminal report")?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_TERMINAL_BYTES,
            "profile-2 terminal report exceeded its {MAX_TERMINAL_BYTES}-byte bound"
        );
        let descriptor = openat(
            &self.stage.directory.file,
            basename,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(PUBLICATION_FILE_MODE),
        )
        .with_context(|| format!("create terminal report stage {basename}"))?;
        let mut writer = File::from(descriptor);
        writer.set_permissions(fs::Permissions::from_mode(PUBLICATION_FILE_MODE))?;
        writer.write_all(&bytes)?;
        writer.flush()?;
        writer.sync_all()?;
        let reader = reopen_read_only_from_fd(&writer)?;
        drop(writer);
        let metadata = reader.metadata()?;
        let identity = FileIdentity::from_metadata(&metadata);
        ensure!(
            identity.bytes == u64::try_from(bytes.len())?
                && identity.mode == PUBLICATION_FILE_MODE
                && metadata.nlink() == 1,
            "staged terminal report identity, extent, mode or link count is invalid"
        );
        let mut verification_reader = reader.try_clone()?;
        verification_reader.seek(SeekFrom::Start(0))?;
        let mut verified_bytes = Vec::with_capacity(bytes.len());
        verification_reader
            .take(u64::try_from(MAX_TERMINAL_BYTES + 1)?)
            .read_to_end(&mut verified_bytes)?;
        ensure!(
            verified_bytes == bytes,
            "staged terminal report bytes changed after sync"
        );
        let staged_path = fs::symlink_metadata(self.stage.directory.path.join(basename))?;
        let final_metadata = reader.metadata()?;
        ensure!(
            identity.matches(&final_metadata)
                && identity.matches(&staged_path)
                && final_metadata.nlink() == 1
                && staged_path.nlink() == 1,
            "terminal stage descriptor/path identity differs"
        );
        self.stage.directory.file.sync_all()?;
        Ok((reader, identity))
    }
}

impl SessionPublication {
    fn refresh_capture_durability(&mut self) {
        let Some(capture) = self.capture else {
            return;
        };
        if self.capture_durable {
            return;
        }
        let refreshed = (|| -> Result<()> {
            self.output.revalidate_path()?;
            let file = self.output.open_entry(OsStr::new(CAPTURE_FINAL_NAME))?;
            let metadata = file.metadata()?;
            ensure!(
                capture.identity.matches(&metadata) && matches!(metadata.nlink(), 1 | 2),
                "committed capture link changed before failure terminalization"
            );
            file.sync_all()?;
            self.output.file.sync_all()?;
            self.output.revalidate_path()?;
            Ok(())
        })();
        if refreshed.is_ok() {
            self.capture_durable = true;
            self.capture_disposition = CaptureDisposition::Published;
        }
    }

    fn link_terminal<H: PublicationHooks>(
        &mut self,
        staged: &File,
        staged_identity: FileIdentity,
        staged_basename: &'static str,
        hooks: &mut H,
    ) -> Result<PublishedFile> {
        hooks.reached(PublicationStep::BeforeTerminalCommit, &self.stop)?;
        self.output.revalidate_path()?;
        let linked_identity =
            link_fd_no_clobber(staged, &self.output, OsStr::new(TERMINAL_FINAL_NAME))?;
        // This flag is set at the exact successful linkat boundary. No caller
        // may attempt a fallback terminal after this point, even if validation
        // or directory fsync below fails.
        self.terminal_linked = true;
        ensure!(
            linked_identity == staged_identity,
            "terminal link identity differs from staged descriptor"
        );
        hooks.reached(PublicationStep::TerminalCommitted, &self.stop)?;
        let final_file = verify_link_at(
            &self.output,
            OsStr::new(TERMINAL_FINAL_NAME),
            staged_identity,
            2,
        )?;
        final_file.sync_all()?;
        self.output.file.sync_all()?;
        self.terminal_durable = true;

        // Output namespace mutation ended at linkat. This cleanup is relative
        // to the separate private stage directory.
        guarded_unlink_at(
            &self.stage.directory,
            OsStr::new(staged_basename),
            staged_identity,
            2,
        )?;
        let final_file = verify_link_at(
            &self.output,
            OsStr::new(TERMINAL_FINAL_NAME),
            staged_identity,
            1,
        )?;
        final_file.sync_all()?;
        self.output.revalidate_path()?;
        Ok(PublishedFile {
            identity: staged_identity,
            basename: TERMINAL_FINAL_NAME,
        })
    }

    fn failure_report(
        &self,
        restoration: super::RestorationOutcome,
        diagnostics: Value,
        retained_stage_directory: Option<&str>,
    ) -> Value {
        json!({
            "schema_version": 1,
            "outcome": "failure",
            "started_utc": self.started_utc,
            "finished_utc": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "restoration": restoration_label(restoration),
            "board_state_known_stock": matches!(
                restoration,
                super::RestorationOutcome::SameHandleVerified
                    | super::RestorationOutcome::FreshWorkerVerified
            ),
            "safety": {
                "tx_stream_enabled": false,
                "flash_written": false,
            },
            "preflight": self
                .preflight
                .as_ref()
                .map_or(Value::Null, preflight_terminal_facts),
            "plan": plan_terminal_facts(self.requested_plan),
            "diagnostics": diagnostics,
            "capture": self.capture_facts(retained_stage_directory),
        })
    }

    fn fail_with_terminal<H: PublicationHooks>(
        mut self,
        source: Error,
        restoration: super::RestorationOutcome,
        diagnostics: Value,
        hooks: &mut H,
    ) -> PublicationFailure {
        self.refresh_capture_durability();
        if self.capture_disposition == CaptureDisposition::None
            && self
                .stage
                .reader
                .metadata()
                .is_ok_and(|metadata| metadata.len() > 0)
        {
            self.capture_disposition = CaptureDisposition::PartialRetained;
        }
        let retained_stage = self.stage.preserve();
        let report = self.failure_report(restoration, diagnostics, retained_stage.as_deref());
        let terminal_result = self
            .stage_terminal(&report, FAILURE_TERMINAL_STAGE_NAME)
            .and_then(|(staged, identity)| {
                self.link_terminal(&staged, identity, FAILURE_TERMINAL_STAGE_NAME, hooks)
            });
        let terminal_error = terminal_result.err();
        PublicationFailure {
            source,
            terminal_error,
            capture: self.capture_disposition,
            capture_facts: self.capture_facts(retained_stage.as_deref()),
            final_capture_basename: self.capture.map(|capture| capture.basename),
            terminal_linked: self.terminal_linked,
            terminal_durable: self.terminal_durable,
        }
    }

    fn fail_after_terminal_attempt(
        mut self,
        source: Error,
        terminal_error: Error,
    ) -> PublicationFailure {
        let retained_stage = self.stage.preserve();
        PublicationFailure {
            source,
            terminal_error: Some(terminal_error),
            capture: self.capture_disposition,
            capture_facts: self.capture_facts(retained_stage.as_deref()),
            final_capture_basename: self.capture.map(|capture| capture.basename),
            terminal_linked: self.terminal_linked,
            terminal_durable: self.terminal_durable,
        }
    }
}

impl SessionPublication {
    fn terminalize_factory_failure(self, error: Error) -> PublicationFailure {
        let diagnostics = json!({
            "factory": bounded_text(&format!("{error:#}")),
        });
        let mut hooks = NoPublicationHooks;
        self.fail_with_terminal(
            error,
            super::RestorationOutcome::NotRequired,
            diagnostics,
            &mut hooks,
        )
    }

    fn publish_capture<H: PublicationHooks>(
        &mut self,
        summary: &ProcessSummary,
        hooks: &mut H,
    ) -> Result<PublishedFile> {
        self.output.revalidate_path()?;
        ensure!(
            self.output.is_empty()?,
            "publication output changed before capture commit"
        );
        let identity = self.stage.validate_accepted(summary, &self.stop)?;
        ensure!(
            identity.device == self.output.device,
            "accepted capture and output are not on the same filesystem"
        );
        self.capture_disposition = CaptureDisposition::AcceptedUnpublished;
        hooks.reached(PublicationStep::BeforeCaptureCommit, &self.stop)?;
        ensure_preflight_running(&self.stop, "at the capture publication boundary")?;
        self.output.revalidate_path()?;
        self.stage.directory.revalidate_path()?;
        ensure_preflight_running(&self.stop, "immediately before the capture link")?;
        let linked_identity = link_fd_no_clobber(
            &self.stage.reader,
            &self.output,
            OsStr::new(CAPTURE_FINAL_NAME),
        )?;
        let published = PublishedFile {
            identity: linked_identity,
            basename: CAPTURE_FINAL_NAME,
        };
        self.capture = Some(published);
        self.capture_disposition = CaptureDisposition::PublishedDurabilityUnconfirmed;
        ensure!(
            linked_identity == identity,
            "capture link identity differs from the accepted descriptor"
        );

        // Cancellation is deliberately ignored after the capture link commit.
        hooks.reached(PublicationStep::CaptureCommitted, &self.stop)?;
        let final_file = verify_link_at(&self.output, OsStr::new(CAPTURE_FINAL_NAME), identity, 2)?;
        final_file.sync_all()?;
        self.output.file.sync_all()?;
        self.capture_durable = true;
        self.capture_disposition = CaptureDisposition::Published;

        guarded_unlink_at(
            &self.stage.directory,
            OsStr::new(CAPTURE_STAGE_NAME),
            identity,
            2,
        )?;
        let final_file = verify_link_at(&self.output, OsStr::new(CAPTURE_FINAL_NAME), identity, 1)?;
        let retained = self.stage.reader.metadata()?;
        ensure!(
            identity.matches(&retained) && retained.nlink() == 1,
            "capture inode changed after guarded stage removal"
        );
        final_file.sync_all()?;
        self.output.revalidate_path()?;
        Ok(published)
    }

    fn terminalize(
        self,
        result: std::result::Result<super::SessionSuccess<ProcessSummary>, super::SessionFailure>,
    ) -> std::result::Result<PublishedSession, PublicationFailure> {
        let mut hooks = NoPublicationHooks;
        self.terminalize_with_hooks(result, &mut hooks)
    }

    fn terminalize_with_hooks<H: PublicationHooks>(
        mut self,
        result: std::result::Result<super::SessionSuccess<ProcessSummary>, super::SessionFailure>,
        hooks: &mut H,
    ) -> std::result::Result<PublishedSession, PublicationFailure> {
        let success = match result {
            Ok(success) => success,
            Err(failure) => {
                let restoration = failure.restoration;
                let diagnostics = session_failure_diagnostics(&failure);
                return Err(self.fail_with_terminal(
                    Error::new(failure),
                    restoration,
                    diagnostics,
                    hooks,
                ));
            }
        };

        if success.restoration != super::RestorationOutcome::SameHandleVerified
            || success.applied != success.summary.applied
            || success.readback != success.summary.readback
            || success.summary.plan != self.requested_plan
        {
            let error = anyhow!(
                "successful session fields do not agree with the requested plan, validated summary and same-handle restoration"
            );
            let diagnostics = json!({
                "publication": bounded_text(&format!("{error:#}")),
            });
            return Err(self.fail_with_terminal(error, success.restoration, diagnostics, hooks));
        }

        let capture = match self.publish_capture(&success.summary, hooks) {
            Ok(capture) => capture,
            Err(error) => {
                let diagnostics = json!({
                    "publication": bounded_text(&format!("{error:#}")),
                });
                return Err(self.fail_with_terminal(
                    error,
                    success.restoration,
                    diagnostics,
                    hooks,
                ));
            }
        };
        let preflight = match self.preflight.clone() {
            Some(preflight) => preflight,
            None => {
                let error = anyhow!("success publication lacks bound process preflight facts");
                let diagnostics = json!({
                    "publication": bounded_text(&format!("{error:#}")),
                });
                return Err(self.fail_with_terminal(
                    error,
                    success.restoration,
                    diagnostics,
                    hooks,
                ));
            }
        };
        let report = success_terminal_report(&success, capture, &self.started_utc, &preflight);
        let (staged, identity) = match self.stage_terminal(&report, SUCCESS_TERMINAL_STAGE_NAME) {
            Ok(staged) => staged,
            Err(error) => {
                let diagnostics = json!({
                    "publication": bounded_text(&format!("{error:#}")),
                });
                return Err(self.fail_with_terminal(
                    error,
                    success.restoration,
                    diagnostics,
                    hooks,
                ));
            }
        };
        let terminal =
            match self.link_terminal(&staged, identity, SUCCESS_TERMINAL_STAGE_NAME, hooks) {
                Ok(terminal) => terminal,
                Err(error) => {
                    return Err(self.fail_after_terminal_attempt(
                        anyhow!("success terminal publication is incomplete or ambiguous"),
                        error,
                    ));
                }
            };
        Ok(PublishedSession {
            output: self.output.path.clone(),
            capture,
            terminal,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum RestorationState {
    Pristine = 0,
    StockRequired = 1,
    PrimaryStockVerified = 2,
    RecoveryStockVerified = 3,
}

struct RestorationTracker {
    state: AtomicU8,
}

impl RestorationTracker {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(RestorationState::Pristine as u8),
        }
    }

    fn state(&self) -> RestorationState {
        match self.state.load(Ordering::Acquire) {
            value if value == RestorationState::Pristine as u8 => RestorationState::Pristine,
            value if value == RestorationState::StockRequired as u8 => {
                RestorationState::StockRequired
            }
            value if value == RestorationState::PrimaryStockVerified as u8 => {
                RestorationState::PrimaryStockVerified
            }
            value if value == RestorationState::RecoveryStockVerified as u8 => {
                RestorationState::RecoveryStockVerified
            }
            _ => unreachable!("restoration tracker stores only closed internal states"),
        }
    }

    fn arm(&self) -> Result<()> {
        self.transition(
            RestorationState::Pristine,
            RestorationState::StockRequired,
            "arm stock-restoration obligation",
        )
    }

    fn require_stock(&self) -> Result<()> {
        ensure!(
            self.state() == RestorationState::StockRequired,
            "fresh recovery requires one outstanding stock-restoration obligation"
        );
        Ok(())
    }

    fn primary_verified(&self) -> Result<()> {
        self.transition(
            RestorationState::StockRequired,
            RestorationState::PrimaryStockVerified,
            "record primary stock verification",
        )
    }

    fn recovery_verified(&self) -> Result<()> {
        self.transition(
            RestorationState::StockRequired,
            RestorationState::RecoveryStockVerified,
            "record fresh-recovery stock verification",
        )
    }

    fn transition(
        &self,
        expected: RestorationState,
        next: RestorationState,
        operation: &str,
    ) -> Result<()> {
        self.state
            .compare_exchange(
                expected as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|actual| {
                anyhow!(
                    "cannot {operation}: restoration state byte {actual} differs from {:?}",
                    expected
                )
            })
    }
}

#[derive(Clone)]
struct ProcessLifecycle {
    restoration: Arc<RestorationTracker>,
    primary_spawn_attempted: Arc<AtomicBool>,
    primary_opened: Arc<AtomicBool>,
    primary_cleanup_confirmed: Arc<AtomicBool>,
    recovery_started: Arc<AtomicBool>,
    recovery_cleanup_confirmed: Arc<AtomicBool>,
}

impl ProcessLifecycle {
    fn new() -> Self {
        Self {
            restoration: Arc::new(RestorationTracker::new()),
            primary_spawn_attempted: Arc::new(AtomicBool::new(false)),
            primary_opened: Arc::new(AtomicBool::new(false)),
            primary_cleanup_confirmed: Arc::new(AtomicBool::new(false)),
            recovery_started: Arc::new(AtomicBool::new(false)),
            recovery_cleanup_confirmed: Arc::new(AtomicBool::new(false)),
        }
    }
}

trait PrimaryClient {
    fn load_profile2(&mut self) -> Result<()>;
    fn verify_profile2(&mut self) -> Result<()>;
    fn read_register(&mut self, register: ReadRegister) -> Result<u32>;
    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback>;
    fn start_receive(&mut self) -> Result<ActiveReceiveReadback>;
    fn capture(&mut self, sink: CaptureSink) -> Result<(CaptureReport, CaptureSink)>;
    fn stop_receive(&mut self) -> Result<()>;
    fn restore_stock(&mut self) -> Result<()>;
    fn verify_stock(&mut self) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
    fn is_terminal(&self) -> bool;
    fn is_cleanup_confirmed(&self) -> bool;
}

impl PrimaryClient for SessionClient {
    fn load_profile2(&mut self) -> Result<()> {
        SessionClient::load_profile2(self).map_err(Error::new)
    }

    fn verify_profile2(&mut self) -> Result<()> {
        SessionClient::verify_profile2(self).map_err(Error::new)
    }

    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        SessionClient::read_register(self, register).map_err(Error::new)
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        SessionClient::write_shadow(self, channel, value).map_err(Error::new)
    }

    fn commit(&mut self) -> Result<()> {
        SessionClient::commit(self).map_err(Error::new)
    }

    fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
        SessionClient::configure_receive(self, plan).map_err(Error::new)
    }

    fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
        SessionClient::start_receive(self).map_err(Error::new)
    }

    fn capture(&mut self, sink: CaptureSink) -> Result<(CaptureReport, CaptureSink)> {
        SessionClient::capture(self, sink).map_err(Error::new)
    }

    fn stop_receive(&mut self) -> Result<()> {
        SessionClient::stop_receive(self).map_err(Error::new)
    }

    fn restore_stock(&mut self) -> Result<()> {
        SessionClient::restore_stock(self).map_err(Error::new)
    }

    fn verify_stock(&mut self) -> Result<()> {
        SessionClient::verify_stock(self).map_err(Error::new)
    }

    fn shutdown(&mut self) -> Result<()> {
        SessionClient::shutdown(self).map_err(Error::new)
    }

    fn terminate(&mut self) -> Result<()> {
        SessionClient::terminate(self).map_err(Error::new)
    }

    fn is_terminal(&self) -> bool {
        SessionClient::is_terminal(self)
    }

    fn is_cleanup_confirmed(&self) -> bool {
        SessionClient::is_cleanup_confirmed(self)
    }
}

trait RecoveryClient {
    fn restore_stock(&mut self) -> Result<()>;
    fn verify_stock(&mut self) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

impl RecoveryClient for NativeRecoveryClient {
    fn restore_stock(&mut self) -> Result<()> {
        NativeRecoveryClient::restore_stock(self).map_err(Error::new)
    }

    fn verify_stock(&mut self) -> Result<()> {
        NativeRecoveryClient::verify_stock(self).map_err(Error::new)
    }

    fn shutdown(&mut self) -> Result<()> {
        NativeRecoveryClient::shutdown(self).map_err(Error::new)
    }

    fn terminate(&mut self) -> Result<()> {
        NativeRecoveryClient::terminate(self).map_err(Error::new)
    }
}

struct ProcessPrimaryImpl<C: PrimaryClient> {
    client: Option<C>,
    sink: Option<CaptureSink>,
    wire_plan: Option<ReceivePlan>,
    configured: Option<ConfiguredReceiveReadback>,
    active: Option<ActiveReceiveReadback>,
    stop: Arc<AtomicBool>,
    lifecycle: ProcessLifecycle,
}

type ProcessPrimary = ProcessPrimaryImpl<SessionClient>;

impl ProcessPrimary {
    fn spawn(
        config: &ProcessConfig,
        sink: CaptureSink,
        stop: Arc<AtomicBool>,
        lifecycle: ProcessLifecycle,
    ) -> Result<Self> {
        config
            .revalidate_primary(&stop)
            .context("revalidate pinned primary-process inputs")?;
        let bundle = config.session_bundle()?;
        let arguments = config.primary_arguments();
        // Publish the attempt at the exact boundary after all local preparation
        // and before process creation/Hello negotiation. A panic inside spawn
        // must retain an unconfirmed-cleanup fact even if no client returns.
        lifecycle
            .primary_spawn_attempted
            .store(true, Ordering::Release);
        let client = match SessionClient::spawn(
            config.inputs.adapter.path(),
            arguments,
            ServerMode::NativePrimary,
            &bundle,
            config.transaction_timeout,
        ) {
            Ok(client) => {
                lifecycle.primary_opened.store(true, Ordering::Release);
                client
            }
            Err(error) => {
                return Err(Error::new(error))
                    .context("start exact native profile-2 primary process");
            }
        };
        Ok(Self::from_client(client, sink, stop, lifecycle))
    }
}

impl<C: PrimaryClient> ProcessPrimaryImpl<C> {
    fn from_client(
        client: C,
        sink: CaptureSink,
        stop: Arc<AtomicBool>,
        lifecycle: ProcessLifecycle,
    ) -> Self {
        Self {
            client: Some(client),
            sink: Some(sink),
            wire_plan: None,
            configured: None,
            active: None,
            stop,
            lifecycle,
        }
    }

    fn client(&self) -> &C {
        self.client
            .as_ref()
            .expect("primary client remains owned outside destructor cleanup")
    }

    fn client_mut(&mut self) -> &mut C {
        self.client
            .as_mut()
            .expect("primary client remains owned outside destructor cleanup")
    }

    fn record_cleanup_confirmation(&self) {
        if self.client().is_cleanup_confirmed() {
            self.lifecycle
                .primary_cleanup_confirmed
                .store(true, Ordering::Release);
        }
    }

    fn check_interrupted(&self) -> Result<()> {
        ensure!(
            !self.stop.load(Ordering::Relaxed),
            "profile-2 process control was cancelled"
        );
        Ok(())
    }

    fn require_plan(&self, plan: SessionPlan) -> Result<ReceivePlan> {
        let wire_plan = wire_plan(plan)?;
        ensure!(
            self.wire_plan == Some(wire_plan),
            "profile-2 process plan changed after receive configuration"
        );
        Ok(wire_plan)
    }
}

impl<C: PrimaryClient> Drop for ProcessPrimaryImpl<C> {
    fn drop(&mut self) {
        let already_confirmed = contain_panic(|| {
            self.client
                .as_ref()
                .expect("primary client is present at destructor entry")
                .is_cleanup_confirmed()
        })
        .unwrap_or(false);
        if !already_confirmed {
            let _ = contain_panic(|| {
                self.client
                    .as_mut()
                    .expect("primary client is present during destructor cleanup")
                    .terminate()
            });
        }
        // Inspect independently even if termination panicked: it can have
        // completed teardown before unwinding, and only this side-effect-free
        // ownership fact may permit fresh recovery.
        let confirmed = contain_panic(|| {
            self.client
                .as_ref()
                .expect("primary client is present after destructor cleanup")
                .is_cleanup_confirmed()
        })
        .unwrap_or(false);
        if confirmed {
            self.lifecycle
                .primary_cleanup_confirmed
                .store(true, Ordering::Release);
        }
        if let Some(client) = self.client.take() {
            let _ = contain_panic(|| drop(client));
        }
    }
}

impl<C: PrimaryClient> RegisterIo for ProcessPrimaryImpl<C> {
    fn read(&mut self, address: RegisterAddress) -> Result<u32> {
        self.check_interrupted()?;
        let register = map_read(address.byte_offset())?;
        self.client_mut()
            .read_register(register)
            .context("read typed register through native primary process")
    }

    fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()> {
        self.check_interrupted()?;
        match map_write(address.byte_offset(), value)? {
            RegisterWrite::Shadow(channel, value) => self
                .client_mut()
                .write_shadow(channel, value)
                .context("write typed shadow register through native primary process"),
            RegisterWrite::Commit => self
                .client_mut()
                .commit()
                .context("commit profile-2 shadow bank through native primary process"),
        }
    }
}

impl<C: PrimaryClient> RecoveryDevice for ProcessPrimaryImpl<C> {
    fn restore_stock(&mut self) -> Result<()> {
        self.client_mut()
            .restore_stock()
            .context("restore frozen stock image on native primary handle")
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.client_mut()
            .verify_stock()
            .context("verify frozen stock identity on native primary handle")?;
        self.lifecycle.restoration.primary_verified()?;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        let result = self
            .client_mut()
            .shutdown()
            .context("shut down native primary process");
        self.record_cleanup_confirmation();
        result
    }

    fn terminate(&mut self) -> Result<()> {
        let result = self
            .client_mut()
            .terminate()
            .context("terminate native primary process");
        self.record_cleanup_confirmation();
        result
    }
}

impl<C: PrimaryClient> SessionDevice for ProcessPrimaryImpl<C> {
    type Capture = ProcessCapture;

    fn load_profile2(&mut self) -> Result<()> {
        // Arm before publication: even an error return can mean that custom
        // configuration reached the device.
        self.lifecycle.restoration.arm()?;
        self.client_mut()
            .load_profile2()
            .context("dispatch sealed profile-2 image in native primary process")
    }

    fn verify_profile2(&mut self) -> Result<()> {
        self.client_mut()
            .verify_profile2()
            .context("verify profile-2 identity in native primary process")
    }

    fn configure_receive_and_verify(&mut self, plan: SessionPlan) -> Result<ReceiveReadback> {
        let wire_plan = wire_plan(plan)?;
        let configured = self
            .client_mut()
            .configure_receive(wire_plan)
            .context("configure disabled receive path in native primary process")?;
        validate_configured(wire_plan, configured)?;
        self.wire_plan = Some(wire_plan);
        self.configured = Some(configured);
        Ok(ReceiveReadback::new(
            configured.sample_rate_hz(),
            plan.request.receiver(),
        ))
    }

    fn start_receive(&mut self, plan: SessionPlan) -> Result<()> {
        let wire_plan = self.require_plan(plan)?;
        let configured = self
            .configured
            .context("profile-2 process lacks configured receive readback")?;
        let active = self
            .client_mut()
            .start_receive()
            .context("start receive in native primary process")?;
        validate_active(wire_plan, configured, active)?;
        self.active = Some(active);
        Ok(())
    }

    fn capture(&mut self, plan: SessionPlan) -> Result<Self::Capture> {
        let wire_plan = self.require_plan(plan)?;
        let configured = self
            .configured
            .context("profile-2 process lacks configured receive readback")?;
        let active = self
            .active
            .context("profile-2 process lacks active receive readback")?;
        let sink = self
            .sink
            .take()
            .context("profile-2 process capture sink was already consumed")?;
        let (report, sink) = self
            .client_mut()
            .capture(sink)
            .context("capture exact raw extent through native primary process")?;
        validate_capture_report(wire_plan, &report)?;
        Ok(ProcessCapture {
            sink,
            report,
            configured,
            active,
        })
    }

    fn stop_receive(&mut self) -> Result<()> {
        self.client_mut()
            .stop_receive()
            .context("stop receive in native primary process")
    }

    fn is_terminal(&self) -> bool {
        self.client().is_terminal()
    }

    fn is_cleanup_confirmed(&self) -> bool {
        self.client().is_cleanup_confirmed()
    }
}

struct ProcessRecoveryImpl<C: RecoveryClient> {
    client: Option<C>,
    lifecycle: ProcessLifecycle,
}

type ProcessRecovery = ProcessRecoveryImpl<NativeRecoveryClient>;

impl ProcessRecovery {
    fn spawn(
        config: &ProcessConfig,
        lifecycle: ProcessLifecycle,
        stop: &AtomicBool,
    ) -> Result<Self> {
        config
            .revalidate_recovery(stop)
            .context("revalidate pinned fresh-recovery inputs")?;
        let client = NativeRecoveryClient::spawn(
            config.inputs.adapter.path(),
            config.inputs.library.path(),
            &config.library_sha256,
            &config.serial,
            config.inputs.stock.path(),
            config.transaction_timeout,
        )
        .map_err(Error::new)
        .context("start exact native profile-2 fresh-recovery process")?;
        Ok(Self::from_client(client, lifecycle))
    }
}

impl<C: RecoveryClient> ProcessRecoveryImpl<C> {
    fn from_client(client: C, lifecycle: ProcessLifecycle) -> Self {
        Self {
            client: Some(client),
            lifecycle,
        }
    }

    #[cfg(test)]
    fn client(&self) -> &C {
        self.client
            .as_ref()
            .expect("recovery client remains owned outside destructor cleanup")
    }

    fn client_mut(&mut self) -> &mut C {
        self.client
            .as_mut()
            .expect("recovery client remains owned outside destructor cleanup")
    }
}

impl<C: RecoveryClient> Drop for ProcessRecoveryImpl<C> {
    fn drop(&mut self) {
        if !self
            .lifecycle
            .recovery_cleanup_confirmed
            .load(Ordering::Acquire)
        {
            let _ = contain_panic(|| {
                self.client
                    .as_mut()
                    .expect("recovery client is present at destructor entry")
                    .terminate()
            });
        }
        if let Some(client) = self.client.take() {
            let _ = contain_panic(|| drop(client));
        }
    }
}

impl<C: RecoveryClient> RecoveryDevice for ProcessRecoveryImpl<C> {
    fn restore_stock(&mut self) -> Result<()> {
        self.lifecycle.restoration.require_stock()?;
        self.client_mut()
            .restore_stock()
            .context("restore frozen stock image in fresh native process")
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.client_mut()
            .verify_stock()
            .context("verify frozen stock identity in fresh native process")?;
        self.lifecycle.restoration.recovery_verified()?;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        let result = self
            .client_mut()
            .shutdown()
            .context("shut down fresh native recovery process");
        if result.is_ok() {
            self.lifecycle
                .recovery_cleanup_confirmed
                .store(true, Ordering::Release);
        }
        result
    }

    fn terminate(&mut self) -> Result<()> {
        self.client_mut()
            .terminate()
            .context("terminate fresh native recovery process")
    }
}

const MAX_PANIC_DIAGNOSTIC_BYTES: usize = 512;

fn discard_panic_payload(payload: Box<dyn Any + Send>) {
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        // The original payload had a panicking destructor. Forget only the
        // secondary payload so its destructor cannot start a third unwind.
        std::mem::forget(secondary);
    }
}

fn contain_panic<T>(operation: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(value) => Some(value),
        Err(payload) => {
            // A panic payload may itself have a panicking destructor. Cleanup
            // and destructor containment must not re-panic while another panic
            // is already unwinding.
            discard_panic_payload(payload);
            None
        }
    }
}

fn bounded_panic_text(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("non-string panic payload");
    if message.len() <= MAX_PANIC_DIAGNOSTIC_BYTES {
        return message.to_owned();
    }

    let mut end = MAX_PANIC_DIAGNOSTIC_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = message[..end].to_owned();
    bounded.push_str("...");
    bounded
}

fn consume_bounded_panic_text(payload: Box<dyn Any + Send>) -> String {
    let text = bounded_panic_text(payload.as_ref());
    discard_panic_payload(payload);
    text
}

fn claim_recovery(lifecycle: &ProcessLifecycle) -> Result<()> {
    lifecycle.restoration.require_stock()?;
    ensure!(
        lifecycle.primary_opened.load(Ordering::Acquire),
        "fresh recovery requires a primary process ownership record"
    );
    ensure!(
        lifecycle.primary_cleanup_confirmed.load(Ordering::Acquire),
        "fresh recovery requires confirmed primary process cleanup"
    );
    lifecycle
        .recovery_started
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| anyhow!("fresh recovery is one-shot and was already attempted"))
}

fn restoration_outcome(state: RestorationState) -> super::RestorationOutcome {
    match state {
        RestorationState::Pristine => super::RestorationOutcome::NotRequired,
        RestorationState::StockRequired => super::RestorationOutcome::Unknown,
        RestorationState::PrimaryStockVerified => super::RestorationOutcome::SameHandleVerified,
        RestorationState::RecoveryStockVerified => super::RestorationOutcome::FreshWorkerVerified,
    }
}

fn retain_unconfirmed_cleanup(lifecycle: &ProcessLifecycle, failure: &mut super::SessionFailure) {
    if lifecycle.primary_spawn_attempted.load(Ordering::Acquire)
        && !lifecycle.primary_cleanup_confirmed.load(Ordering::Acquire)
    {
        failure.primary_cleanup(anyhow!(
            "primary process cleanup remains unconfirmed after panic"
        ));
    }
    if lifecycle.recovery_started.load(Ordering::Acquire)
        && !lifecycle.recovery_cleanup_confirmed.load(Ordering::Acquire)
    {
        failure.recovery_cleanup(anyhow!(
            "fresh recovery process cleanup remains unconfirmed after panic"
        ));
    }
}

fn run_with_panic_recovery<F: SessionFactory>(
    plan: SessionPlan,
    factory: &mut F,
    stop: &AtomicBool,
    lifecycle: &ProcessLifecycle,
) -> std::result::Result<super::SessionSuccess<F::Summary>, super::SessionFailure> {
    let panic_text = match catch_unwind(AssertUnwindSafe(|| super::execute(plan, factory, stop))) {
        Ok(result) => return result,
        Err(payload) => consume_bounded_panic_text(payload),
    };

    let mut failure = super::SessionFailure::new();
    failure.primary(anyhow!(
        "profile-2 process session panicked: {}",
        panic_text
    ));
    failure.restoration = restoration_outcome(lifecycle.restoration.state());

    if lifecycle.restoration.state() == RestorationState::StockRequired
        && lifecycle.primary_opened.load(Ordering::Acquire)
        && lifecycle.primary_cleanup_confirmed.load(Ordering::Acquire)
        && !lifecycle.recovery_started.load(Ordering::Acquire)
    {
        let recovery = catch_unwind(AssertUnwindSafe(|| {
            super::recover_with_fresh_worker(factory, &mut failure);
        }));
        if let Err(recovery_payload) = recovery {
            failure.fresh_recovery = Some(anyhow!(
                "panic while recovering stock after session panic: {}",
                consume_bounded_panic_text(recovery_payload)
            ));
            failure.restoration = restoration_outcome(lifecycle.restoration.state());
        }
    } else if lifecycle.restoration.state() == RestorationState::StockRequired
        && lifecycle.recovery_started.load(Ordering::Acquire)
    {
        failure.fresh_recovery = Some(anyhow!(
            "session panic escaped an existing recovery attempt; no second recovery was opened"
        ));
    }

    retain_unconfirmed_cleanup(lifecycle, &mut failure);
    Err(failure)
}

/// Fixed factory consumed only by the guarded coordinator.
struct ProcessFactory {
    config: ProcessConfig,
    sink: Option<CaptureSink>,
    stop: Arc<AtomicBool>,
    lifecycle: ProcessLifecycle,
}

impl ProcessFactory {
    fn new(config: ProcessConfig, publication: &mut SessionPublication) -> Result<Self> {
        require_unwind_profile()?;
        let stop = Arc::clone(&config.stop);
        ensure!(
            Arc::ptr_eq(&stop, &publication.stop),
            "process factory and publisher must share the exact cancellation token"
        );
        publication.bind_preflight(&config)?;
        let sink = publication.stage.take_sink(Arc::clone(&stop))?;
        Ok(Self {
            config,
            sink: Some(sink),
            stop,
            lifecycle: ProcessLifecycle::new(),
        })
    }

    fn run(
        mut self,
        plan: SessionPlan,
    ) -> std::result::Result<super::SessionSuccess<ProcessSummary>, super::SessionFailure> {
        // Clone the factory-owned token so the coordinator, register bridge,
        // capture sink and validator necessarily observe the same source while
        // `self` remains exclusively borrowed for the session.
        let stop = Arc::clone(&self.stop);
        let lifecycle = self.lifecycle.clone();
        run_with_panic_recovery(plan, &mut self, &stop, &lifecycle)
    }
}

/// Compose one complete private process session behind the narrow launcher.
///
/// Preflight intentionally precedes every output mutation. Once publication
/// setup succeeds, factory construction and the single consuming run are routed
/// through exactly one terminalization attempt. The production constructor
/// cannot open a child; its unwind is therefore safe to report as restoration
/// not required. Runtime panics remain the responsibility of
/// `ProcessFactory::run` and its recovery-aware fence.
///
/// `construct_factory` must remain process- and device-free. `run_factory` must
/// contain every recoverable runtime unwind that could require stock recovery.
/// Production fixes those obligations to `ProcessFactory::new` and
/// `ProcessFactory::run`; the generic parameters exist only for CPU tests.
fn compose_process_session_with<C, F, P, N, R>(
    trusted_parent: &Path,
    output_basename: &OsStr,
    requested_plan: SessionPlan,
    stop: Arc<AtomicBool>,
    preflight: P,
    construct_factory: N,
    run_factory: R,
) -> std::result::Result<PublishedSession, ProcessSessionFailure>
where
    P: FnOnce() -> Result<C>,
    N: FnOnce(C, &mut SessionPublication) -> Result<F>,
    R: FnOnce(
        F,
    )
        -> std::result::Result<super::SessionSuccess<ProcessSummary>, super::SessionFailure>,
{
    require_unwind_profile().map_err(ProcessSessionFailure::Preflight)?;
    let config = preflight()
        .context("complete profile-2 process preflight")
        .map_err(ProcessSessionFailure::Preflight)?;
    let mut publication = SessionPublication::create(
        trusted_parent,
        output_basename,
        requested_plan,
        Arc::clone(&stop),
    )
    .context("create profile-2 session publication authority")
    .map_err(ProcessSessionFailure::PublicationSetup)?;

    let factory = match catch_unwind(AssertUnwindSafe(|| {
        construct_factory(config, &mut publication)
    })) {
        Ok(Ok(factory)) => factory,
        Ok(Err(error)) => {
            let error = error.context("construct profile-2 process factory");
            return Err(ProcessSessionFailure::Publication(
                publication.terminalize_factory_failure(error),
            ));
        }
        Err(payload) => {
            let error = anyhow!(
                "profile-2 process factory construction panicked: {}",
                consume_bounded_panic_text(payload)
            );
            return Err(ProcessSessionFailure::Publication(
                publication.terminalize_factory_failure(error),
            ));
        }
    };

    // No cancellation check or fallible composition step may separate the one
    // guarded run from terminalization. Publication owns the commit boundary.
    let result = run_factory(factory);
    publication
        .terminalize(result)
        .map_err(ProcessSessionFailure::Publication)
}

fn run_process_session(
    request: ProcessSessionRequest<'_>,
    stop: Arc<AtomicBool>,
) -> std::result::Result<PublishedSession, ProcessSessionFailure> {
    let ProcessSessionRequest {
        preflight,
        plan,
        output_basename,
    } = request;
    let trusted_parent = preflight.stage_parent.to_path_buf();
    let preflight_stop = Arc::clone(&stop);
    compose_process_session_with(
        &trusted_parent,
        output_basename,
        plan,
        stop,
        move || ProcessConfig::preflight(preflight, &preflight_stop),
        ProcessFactory::new,
        move |factory| factory.run(plan),
    )
}

pub(super) fn run_launch(request: super::LaunchRequest, stop: Arc<AtomicBool>) -> Result<PathBuf> {
    let super::LaunchRequest {
        workspace_root,
        manifest,
        manifest_sha256,
        adapter,
        adapter_sha256,
        library,
        library_sha256,
        stock_image,
        serial,
        transaction_timeout,
        stage_parent,
        output_basename,
        plan,
    } = request;
    let intended_output = stage_parent.join(Path::new(&output_basename));
    let published = run_process_session(
        ProcessSessionRequest {
            preflight: ProcessPreflight {
                workspace_root: &workspace_root,
                manifest: &manifest,
                manifest_sha256: &manifest_sha256,
                adapter: &adapter,
                adapter_sha256: &adapter_sha256,
                library: &library,
                library_sha256: &library_sha256,
                stock_image: &stock_image,
                serial: &serial,
                transaction_timeout,
                stage_parent: &stage_parent,
            },
            plan,
            output_basename: &output_basename,
        },
        stop,
    )
    .map_err(Error::new)
    .with_context(|| {
        format!(
            "run guarded profile-2 session for requested output {}",
            intended_output.display()
        )
    })?;
    let PublishedSession {
        output,
        capture: _capture,
        terminal: _terminal,
    } = published;
    Ok(output)
}

impl SessionFactory for ProcessFactory {
    type Capture = ProcessCapture;
    type Primary = ProcessPrimary;
    type Recovery = ProcessRecovery;
    type Summary = ProcessSummary;

    fn open_primary(&mut self, _plan: SessionPlan) -> Result<Self::Primary> {
        ensure!(
            self.sink.is_some(),
            "profile-2 process factory has no unused parent capture sink"
        );
        let client = ProcessPrimary::spawn(
            &self.config,
            self.sink
                .take()
                .expect("capture sink presence checked before child spawn"),
            Arc::clone(&self.stop),
            self.lifecycle.clone(),
        );
        if client.is_err() {
            // The sink moved only after every local check. A failed spawn drops
            // it as non-evidence; opening another primary is forbidden.
            self.sink = None;
        }
        client
    }

    fn open_recovery(&mut self) -> Result<Self::Recovery> {
        claim_recovery(&self.lifecycle)?;
        ProcessRecovery::spawn(&self.config, self.lifecycle.clone(), &self.stop)
    }

    fn validate_capture(
        &mut self,
        plan: SessionPlan,
        applied: AppliedConfiguration,
        readback: ReceiveReadback,
        capture: Self::Capture,
    ) -> Result<Self::Summary> {
        validate_process_capture(plan, applied, readback, capture)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegisterWrite {
    Shadow(ShadowChannel, u32),
    Commit,
}

fn map_read(byte_offset: u32) -> Result<ReadRegister> {
    ReadRegister::try_from(byte_offset)
        .map_err(Error::new)
        .context("map controller read to the complete-session allowlist")
}

fn map_write(byte_offset: u32, value: u32) -> Result<RegisterWrite> {
    if byte_offset == bladerf_profile2_session_ipc::COMMIT_BYTE_OFFSET {
        ensure!(
            value == bladerf_profile2_session_ipc::COMMIT_MAGIC,
            "controller supplied an incompatible profile-2 commit value"
        );
        return Ok(RegisterWrite::Commit);
    }
    let channel = ShadowChannel::try_from(byte_offset)
        .map_err(Error::new)
        .context("map controller write to the complete-session allowlist")?;
    Ok(RegisterWrite::Shadow(channel, value))
}

fn receive_channel(receiver: Receiver) -> ReceiveChannel {
    match receiver {
        Receiver::Rx1 => ReceiveChannel::Rx1,
        Receiver::Rx2 => ReceiveChannel::Rx2,
    }
}

fn wire_plan(plan: SessionPlan) -> Result<ReceivePlan> {
    ReceivePlan::new(
        receive_channel(plan.request.receiver()),
        plan.center_hz,
        plan.gain_db,
        plan.records,
    )
    .map_err(Error::new)
    .context("construct exact complete-session receive plan")
}

fn validate_configured(plan: ReceivePlan, readback: ConfiguredReceiveReadback) -> Result<()> {
    ensure!(
        readback.channel() == plan.channel()
            && readback
                .center_frequency_hz()
                .abs_diff(plan.center_frequency_hz())
                <= MAX_FREQUENCY_ERROR_HZ
            && readback.sample_rate_hz() == SOURCE_SAMPLE_RATE_HZ
            && readback.bandwidth_hz() == SOURCE_BANDWIDTH_HZ,
        "configured receive readback differs from the exact profile-2 plan"
    );
    Ok(())
}

fn validate_active(
    plan: ReceivePlan,
    configured: ConfiguredReceiveReadback,
    active: ActiveReceiveReadback,
) -> Result<()> {
    validate_configured(plan, active.configured())?;
    ensure!(
        active.configured() == configured && active.gain_db() == plan.gain_db(),
        "active receive readback differs from configured profile-2 state"
    );
    Ok(())
}

fn validate_capture_report(plan: ReceivePlan, report: &CaptureReport) -> Result<()> {
    ensure!(
        report.transport_samples() == plan.expected_transport_samples()
            && report.raw_bytes() == plan.expected_raw_bytes()
            && report.sha256().len() == 64
            && report
                .sha256()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "native capture report differs from the plan-derived extent or digest contract"
    );
    Ok(())
}

fn validate_process_capture(
    plan: SessionPlan,
    applied: AppliedConfiguration,
    readback: ReceiveReadback,
    capture: ProcessCapture,
) -> Result<ProcessSummary> {
    let wire_plan = wire_plan(plan)?;
    validate_configured(wire_plan, capture.configured)?;
    validate_active(wire_plan, capture.configured, capture.active)?;
    validate_capture_report(wire_plan, &capture.report)?;
    ensure!(
        readback
            == ReceiveReadback::new(capture.configured.sample_rate_hz(), plan.request.receiver()),
        "profile-2 coordinator readback differs from process capture evidence"
    );
    ensure!(
        applied.phase_words() == plan.request.phase_words(),
        "profile-2 applied words differ from the process request"
    );

    // SessionClient returned the sink only after flush and sync_all succeeded.
    // Keep that descriptor as the publication authority while the strict
    // scanner independently reopens the private path.
    let CaptureSink {
        file,
        stage_path,
        stop,
    } = capture.sink;
    let captured_metadata = file
        .metadata()
        .context("inspect completed profile-2 capture descriptor")?;
    let path_metadata = fs::symlink_metadata(&stage_path).with_context(|| {
        format!(
            "inspect completed profile-2 capture path {}",
            stage_path.display()
        )
    })?;
    ensure!(
        captured_metadata.dev() == path_metadata.dev()
            && captured_metadata.ino() == path_metadata.ino()
            && captured_metadata.len() == capture.report.raw_bytes()
            && path_metadata.len() == capture.report.raw_bytes()
            && captured_metadata.nlink() == 1
            && path_metadata.nlink() == 1
            && captured_metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE
            && path_metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE,
        "completed profile-2 capture descriptor/path identity or extent changed before validation"
    );
    let stream = validate_file_exact(&stage_path, capture.report.raw_bytes(), &stop)
        .with_context(|| format!("strictly validate process capture {}", stage_path.display()))?;
    let final_metadata = file
        .metadata()
        .context("reinspect validated profile-2 capture descriptor")?;
    let final_path_metadata = fs::symlink_metadata(&stage_path).with_context(|| {
        format!(
            "reinspect validated profile-2 capture path {}",
            stage_path.display()
        )
    })?;
    ensure!(
        stream.data_records == plan.records
            && stream.processed_bytes == capture.report.raw_bytes()
            && stream.processed_sha256 == capture.report.sha256()
            && final_metadata.dev() == captured_metadata.dev()
            && final_metadata.ino() == captured_metadata.ino()
            && final_metadata.len() == captured_metadata.len()
            && final_metadata.nlink() == 1
            && final_metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE
            && final_path_metadata.dev() == captured_metadata.dev()
            && final_path_metadata.ino() == captured_metadata.ino()
            && final_path_metadata.len() == captured_metadata.len()
            && final_path_metadata.nlink() == 1
            && final_path_metadata.permissions().mode() & 0o7777 == PUBLICATION_FILE_MODE,
        "strict profile-2 stream differs from the process capture report"
    );
    reconcile_capture(plan.request, applied, readback, &stream)
        .context("reconcile process capture with requested, applied and RF readback state")?;
    let read_only = reopen_read_only_from_fd(&file)
        .context("retain accepted capture through an independent read-only descriptor")?;
    drop(file);

    Ok(ProcessSummary {
        plan,
        applied,
        readback,
        configured: capture.configured,
        active: capture.active,
        report: capture.report,
        stage: ValidatedStage {
            file: read_only,
            path: stage_path,
            device: captured_metadata.dev(),
            inode: captured_metadata.ino(),
            bytes: captured_metadata.len(),
        },
        stream,
    })
}

fn native_primary_arguments(
    library: &Path,
    library_sha256: &str,
    custom_image: &Path,
    custom_sha256: &str,
    custom_bytes: u64,
    stock_image: &Path,
    serial: &str,
) -> Vec<OsString> {
    vec![
        "session-native-primary".into(),
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
        "--serial".into(),
        serial.into(),
    ]
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    use std::rc::Rc;

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::profile2_control::{CaptureRequest, exercise_reference_contract};

    const WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
    const CUSTOM_SHA256: &str = "a4ad63aa0f5668de2d628ceb724048ab8e49991383e1be9ec519e7e26dcc5e0c";

    fn plan(receiver: Receiver) -> SessionPlan {
        SessionPlan::new(
            CaptureRequest::new(WORDS, receiver),
            98_100_000,
            40,
            100_000,
            4,
        )
        .unwrap()
    }

    fn sink(stop: Arc<AtomicBool>) -> (tempfile::TempDir, CaptureSink) {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("capture.sc16q11.partial");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        (directory, CaptureSink::new(file, path, stop).unwrap())
    }

    fn retained_test_input(directory: &Path, name: &str, mode: u32) -> StagedInput {
        let path = directory.join(name);
        let bytes = format!("synthetic {name}\n");
        fs::write(&path, bytes.as_bytes()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let file = File::open(&path).unwrap();
        let metadata = file.metadata().unwrap();
        StagedInput {
            file,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            sha256: format!("{:x}", Sha256::digest(bytes.as_bytes())),
            mode,
        }
    }

    fn synthetic_process_config() -> ProcessConfig {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = retained_test_input(directory.path(), "adapter", ADAPTER_MODE);
        let library = retained_test_input(directory.path(), "library", INPUT_MODE);
        let custom = retained_test_input(directory.path(), "custom", INPUT_MODE);
        let stock = retained_test_input(directory.path(), "stock", INPUT_MODE);
        ProcessConfig {
            inputs: InputStage {
                directory,
                adapter,
                library,
                custom,
                stock,
            },
            adapter_sha256: "0".repeat(64),
            library_sha256: "1".repeat(64),
            custom_sha256: CUSTOM_SHA256.to_owned(),
            custom_bytes: 12_858_972,
            manifest_sha256: "2".repeat(64),
            usbfs_memory_mb: 64,
            serial: "0123456789abcdef0123456789abcdef".to_owned(),
            transaction_timeout: Duration::from_secs(1),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    struct PreflightFixture {
        _directory: tempfile::TempDir,
        workspace: PathBuf,
        manifest: PathBuf,
        manifest_sha256: String,
        adapter: PathBuf,
        adapter_sha256: String,
        library: PathBuf,
        library_sha256: String,
        custom: PathBuf,
        custom_sha256: String,
        stock: PathBuf,
        stock_sha256: String,
        stage_parent: PathBuf,
    }

    impl PreflightFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let workspace = directory.path().join("workspace");
            let manifest = directory.path().join("manifest.json");
            let adapter = directory.path().join("adapter-source");
            let library = directory.path().join("library-source");
            let custom = directory.path().join("custom-source");
            let stock = directory.path().join("stock-source");
            let stage_parent = directory.path().join("stage-parent");
            fs::create_dir(&workspace).unwrap();
            fs::create_dir(&stage_parent).unwrap();
            fs::set_permissions(&stage_parent, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&manifest, b"synthetic manifest\n").unwrap();
            let adapter_bytes = b"synthetic adapter\n";
            let library_bytes = b"synthetic library\n";
            let custom_bytes = b"synthetic custom image\n";
            let stock_bytes = b"synthetic stock image\n";
            fs::write(&adapter, adapter_bytes).unwrap();
            fs::write(&library, library_bytes).unwrap();
            fs::write(&custom, custom_bytes).unwrap();
            fs::write(&stock, stock_bytes).unwrap();
            Self {
                _directory: directory,
                workspace,
                manifest,
                manifest_sha256: "a".repeat(64),
                adapter,
                adapter_sha256: format!("{:x}", Sha256::digest(adapter_bytes)),
                library,
                library_sha256: format!("{:x}", Sha256::digest(library_bytes)),
                custom,
                custom_sha256: format!("{:x}", Sha256::digest(custom_bytes)),
                stock,
                stock_sha256: format!("{:x}", Sha256::digest(stock_bytes)),
                stage_parent,
            }
        }

        fn request(&self) -> ProcessPreflight<'_> {
            ProcessPreflight {
                workspace_root: &self.workspace,
                manifest: &self.manifest,
                manifest_sha256: &self.manifest_sha256,
                adapter: &self.adapter,
                adapter_sha256: &self.adapter_sha256,
                library: &self.library,
                library_sha256: &self.library_sha256,
                stock_image: &self.stock,
                serial: "0123456789abcdef0123456789abcdef",
                transaction_timeout: Duration::from_secs(1),
                stage_parent: &self.stage_parent,
            }
        }

        fn custom(&self) -> VerifiedCustomImage {
            VerifiedCustomImage {
                path: self.custom.clone(),
                sha256: self.custom_sha256.clone(),
                bytes: fs::metadata(&self.custom).unwrap().len(),
            }
        }

        fn stock(&self) -> StockExpectation<'_> {
            StockExpectation {
                sha256: &self.stock_sha256,
                bytes: fs::metadata(&self.stock).unwrap().len(),
            }
        }
    }

    struct MockPreflightChecks {
        custom: VerifiedCustomImage,
        events: Rc<RefCell<Vec<&'static str>>>,
        usbfs_error: bool,
        usbfs_memory_mb: u64,
        library_error: bool,
    }

    impl MockPreflightChecks {
        fn new(custom: VerifiedCustomImage) -> Self {
            Self {
                custom,
                events: Rc::new(RefCell::new(Vec::new())),
                usbfs_error: false,
                usbfs_memory_mb: 64,
                library_error: false,
            }
        }
    }

    impl PreflightChecks for MockPreflightChecks {
        fn check_usbfs(&mut self) -> Result<u64> {
            self.events.borrow_mut().push("usbfs");
            ensure!(!self.usbfs_error, "synthetic usbfs rejection");
            Ok(self.usbfs_memory_mb)
        }

        fn verify_custom(
            &mut self,
            _workspace_root: &Path,
            _manifest: &Path,
            _manifest_sha256: &str,
            _stop: &AtomicBool,
        ) -> Result<VerifiedCustomImage> {
            self.events.borrow_mut().push("custom");
            Ok(self.custom.clone())
        }

        fn check_library(
            &mut self,
            adapter: &Path,
            library: &Path,
            _library_sha256: &str,
            working_directory: &Path,
            _timeout: Duration,
            _stop: &AtomicBool,
        ) -> Result<()> {
            self.events.borrow_mut().push("library");
            ensure!(
                adapter.parent() == Some(working_directory)
                    && library.parent() == Some(working_directory),
                "checker did not receive only staged inputs"
            );
            ensure!(!self.library_error, "synthetic library rejection");
            Ok(())
        }
    }

    fn valid_stream(plan: SessionPlan, applied: AppliedConfiguration) -> Vec<u8> {
        const MAGIC: [u8; 4] = *b"FRBO";
        const FORMAT_V2: u16 = 0x2000;
        const CONFIG: u16 = 0x0010;
        const RX2: u16 = 0x0020;
        const PROFILE2: u16 = 2;

        let receiver = if plan.request.receiver() == Receiver::Rx2 {
            RX2
        } else {
            0
        };
        let epoch_tag = (applied.epoch() & 0x0f) as u16;
        let mut bytes = Vec::with_capacity(((plan.records + 1) * 32) as usize);

        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&(FORMAT_V2 | CONFIG | receiver | epoch_tag).to_le_bytes());
        bytes.extend_from_slice(&PROFILE2.to_le_bytes());
        bytes.extend_from_slice(&applied.epoch().to_le_bytes());
        bytes.extend_from_slice(&SOURCE_SAMPLE_RATE_HZ.to_le_bytes());
        for word in applied.phase_words() {
            bytes.extend_from_slice(&word.to_le_bytes());
        }

        for record in 0..plan.records {
            bytes.extend_from_slice(&MAGIC);
            bytes.extend_from_slice(&(FORMAT_V2 | receiver | epoch_tag).to_le_bytes());
            bytes.extend_from_slice(&PROFILE2.to_le_bytes());
            bytes.extend_from_slice(&(19 + record * 20).to_le_bytes());
            bytes.extend_from_slice(&[0; 16]);
        }
        bytes
    }

    #[test]
    fn register_mapping_is_exact_and_rejects_every_other_write() {
        for (offset, expected) in [
            (0x00, ReadRegister::Id),
            (0x04, ReadRegister::Capabilities),
            (0x10, ReadRegister::Shadow0),
            (0x14, ReadRegister::Shadow1),
            (0x18, ReadRegister::Shadow2),
            (0x1c, ReadRegister::Shadow3),
            (0x24, ReadRegister::Status),
            (0x28, ReadRegister::AppliedEpoch),
            (0x2c, ReadRegister::OutcomeEpoch),
            (0x30, ReadRegister::Applied0),
            (0x34, ReadRegister::Applied1),
            (0x38, ReadRegister::Applied2),
            (0x3c, ReadRegister::Applied3),
        ] {
            assert_eq!(map_read(offset).unwrap(), expected);
        }
        assert!(map_read(0x08).is_err());

        for (offset, channel) in [
            (0x10, ShadowChannel::Channel0),
            (0x14, ShadowChannel::Channel1),
            (0x18, ShadowChannel::Channel2),
            (0x1c, ShadowChannel::Channel3),
        ] {
            assert_eq!(
                map_write(offset, 0x1122_3344).unwrap(),
                RegisterWrite::Shadow(channel, 0x1122_3344)
            );
        }
        assert_eq!(
            map_write(
                bladerf_profile2_session_ipc::COMMIT_BYTE_OFFSET,
                bladerf_profile2_session_ipc::COMMIT_MAGIC,
            )
            .unwrap(),
            RegisterWrite::Commit
        );
        assert!(map_write(0x00, 1).is_err());
        assert!(map_write(0x20, 0).is_err());
    }

    #[test]
    fn receiver_and_extent_mapping_are_explicit() {
        let rx1 = wire_plan(plan(Receiver::Rx1)).unwrap();
        let rx2 = wire_plan(plan(Receiver::Rx2)).unwrap();
        assert_eq!(rx1.channel(), ReceiveChannel::Rx1);
        assert_eq!(rx2.channel(), ReceiveChannel::Rx2);
        assert_eq!(rx2.expected_transport_samples(), 800_008);
        assert_eq!(rx2.expected_raw_bytes(), 3_200_032);
    }

    #[test]
    fn native_primary_arguments_are_exact_and_role_fixed() {
        let arguments = native_primary_arguments(
            Path::new("/opt/profile 2/libbladeRF.so.2"),
            &"1".repeat(64),
            Path::new("/opt/profile 2/custom.rbf"),
            CUSTOM_SHA256,
            12_858_972,
            Path::new("/opt/profile 2/stock.rbf"),
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            arguments,
            [
                "session-native-primary",
                "--library",
                "/opt/profile 2/libbladeRF.so.2",
                "--sha256",
                &"1".repeat(64),
                "--custom-image",
                "/opt/profile 2/custom.rbf",
                "--custom-sha256",
                CUSTOM_SHA256,
                "--custom-bytes",
                "12858972",
                "--stock-image",
                "/opt/profile 2/stock.rbf",
                "--serial",
                "0123456789abcdef0123456789abcdef",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn complete_request_budget_has_zero_slack_at_the_poll_limit() {
        let maximum = SessionPlan::new(
            CaptureRequest::new(WORDS, Receiver::Rx1),
            98_100_000,
            40,
            100_000,
            4_096,
        )
        .unwrap();
        assert_eq!(
            30 + maximum.max_status_polls,
            bladerf_profile2_session_ipc::MAX_PRIMARY_SESSION_REQUESTS
        );
    }

    #[test]
    fn capture_sink_checks_cancellation_before_each_write() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, mut sink) = sink(Arc::clone(&stop));
        sink.write_all(b"accepted").unwrap();
        stop.store(true, Ordering::Relaxed);
        let error = sink.write_all(b"rejected").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        drop(sink.file);
        assert_eq!(fs::read(&sink.stage_path).unwrap(), b"accepted");
    }

    #[test]
    fn capture_sink_rejects_mismatched_path_and_insecure_modes() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let first_path = directory.path().join("first.partial");
        let second_path = directory.path().join("second.partial");
        let first = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&first_path)
            .unwrap();
        fs::write(&second_path, []).unwrap();
        assert!(CaptureSink::new(first, second_path, Arc::new(AtomicBool::new(false)),).is_err());

        let insecure_path = directory.path().join("insecure.partial");
        let insecure = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o644)
            .open(&insecure_path)
            .unwrap();
        fs::set_permissions(&insecure_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            CaptureSink::new(insecure, insecure_path, Arc::new(AtomicBool::new(false)),).is_err()
        );
    }

    #[derive(Default)]
    struct MockPrimary {
        terminal: bool,
        cleanup_confirmed: bool,
        calls: Vec<&'static str>,
    }

    impl PrimaryClient for MockPrimary {
        fn load_profile2(&mut self) -> Result<()> {
            self.calls.push("load");
            Ok(())
        }

        fn verify_profile2(&mut self) -> Result<()> {
            self.calls.push("verify-custom");
            Ok(())
        }

        fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
            self.calls.push("read");
            Ok(register.byte_offset())
        }

        fn write_shadow(&mut self, _channel: ShadowChannel, _value: u32) -> Result<()> {
            self.calls.push("write");
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            self.calls.push("commit");
            Ok(())
        }

        fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
            self.calls.push("configure");
            Ok(ConfiguredReceiveReadback::new(
                plan.channel(),
                plan.center_frequency_hz(),
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ,
            ))
        }

        fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
            self.calls.push("start");
            Ok(ActiveReceiveReadback::new(
                ConfiguredReceiveReadback::new(
                    ReceiveChannel::Rx2,
                    98_100_000,
                    SOURCE_SAMPLE_RATE_HZ,
                    SOURCE_BANDWIDTH_HZ,
                ),
                40,
            ))
        }

        fn capture(&mut self, mut sink: CaptureSink) -> Result<(CaptureReport, CaptureSink)> {
            self.calls.push("capture");
            sink.write_all(b"model")?;
            Ok((CaptureReport::new(800_008, 3_200_032, "a".repeat(64)), sink))
        }

        fn stop_receive(&mut self) -> Result<()> {
            self.calls.push("stop");
            Ok(())
        }

        fn restore_stock(&mut self) -> Result<()> {
            self.calls.push("restore");
            Ok(())
        }

        fn verify_stock(&mut self) -> Result<()> {
            self.calls.push("verify-stock");
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.calls.push("shutdown");
            self.cleanup_confirmed = true;
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            self.calls.push("terminate");
            self.cleanup_confirmed = true;
            Ok(())
        }

        fn is_terminal(&self) -> bool {
            self.terminal
        }

        fn is_cleanup_confirmed(&self) -> bool {
            self.cleanup_confirmed
        }
    }

    #[test]
    fn primary_retains_readbacks_sink_and_restoration_obligation() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let lifecycle = ProcessLifecycle::new();
        let mut primary =
            ProcessPrimaryImpl::from_client(MockPrimary::default(), sink, stop, lifecycle.clone());
        let plan = plan(Receiver::Rx2);

        primary.load_profile2().unwrap();
        assert_eq!(
            lifecycle.restoration.state(),
            RestorationState::StockRequired
        );
        primary.verify_profile2().unwrap();
        let readback = primary.configure_receive_and_verify(plan).unwrap();
        assert_eq!(
            readback,
            ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx2)
        );
        primary.start_receive(plan).unwrap();
        let capture = primary.capture(plan).unwrap();
        assert_eq!(capture.configured.channel(), ReceiveChannel::Rx2);
        assert_eq!(capture.active.gain_db(), 40);
        assert_eq!(capture.report.raw_bytes(), 3_200_032);
        drop(capture.sink.file);
        assert_eq!(fs::read(&capture.sink.stage_path).unwrap(), b"model");
        primary.stop_receive().unwrap();
        primary.restore_stock().unwrap();
        primary.verify_stock().unwrap();
        assert_eq!(
            lifecycle.restoration.state(),
            RestorationState::PrimaryStockVerified
        );
        assert!(!primary.is_cleanup_confirmed());
        primary.shutdown().unwrap();
        assert!(primary.is_cleanup_confirmed());
        assert_eq!(
            primary.client().calls,
            [
                "load",
                "verify-custom",
                "configure",
                "start",
                "capture",
                "stop",
                "restore",
                "verify-stock",
                "shutdown",
            ]
        );
    }

    #[test]
    fn control_cancellation_precedes_every_client_operation() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut primary = ProcessPrimaryImpl::from_client(
            MockPrimary::default(),
            sink,
            Arc::clone(&stop),
            ProcessLifecycle::new(),
        );
        stop.store(true, Ordering::Relaxed);
        let error = crate::profile2_control::Controller::new(&mut primary, 4)
            .and_then(|mut controller| {
                controller.apply_stopped(CaptureRequest::new(WORDS, Receiver::Rx1))
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"));
        assert!(primary.client().calls.is_empty());
    }

    struct PollCancellingPrimary {
        stop: Arc<AtomicBool>,
        shadow: [u32; 4],
        written_mask: u8,
        committed: bool,
        status_polls: usize,
    }

    impl PollCancellingPrimary {
        fn new(stop: Arc<AtomicBool>) -> Self {
            Self {
                stop,
                shadow: [0; 4],
                written_mask: 0,
                committed: false,
                status_polls: 0,
            }
        }
    }

    impl PrimaryClient for PollCancellingPrimary {
        fn load_profile2(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not load an image")
        }

        fn verify_profile2(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not verify an image")
        }

        fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
            match register {
                ReadRegister::Id => Ok(0x5246_0201),
                ReadRegister::Capabilities => Ok(0x0000_1f04),
                ReadRegister::Shadow0
                | ReadRegister::Shadow1
                | ReadRegister::Shadow2
                | ReadRegister::Shadow3 => {
                    let index = usize::try_from((register.byte_offset() - 0x10) / 4).unwrap();
                    Ok(self.shadow[index])
                }
                ReadRegister::Status if !self.committed => Ok(u32::from(self.written_mask) << 20),
                ReadRegister::Status => {
                    self.status_polls += 1;
                    self.stop.store(true, Ordering::Relaxed);
                    Ok(1 << 31)
                }
                ReadRegister::AppliedEpoch => Ok(0),
                _ => unreachable!("cancellation must precede post-poll register reads"),
            }
        }

        fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
            let index = usize::try_from((channel.byte_offset() - 0x10) / 4).unwrap();
            self.shadow[index] = value;
            self.written_mask |= 1 << index;
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            self.committed = true;
            self.written_mask = 0;
            Ok(())
        }

        fn configure_receive(&mut self, _plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
            unreachable!("mid-poll cancellation model does not configure receive")
        }

        fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
            unreachable!("mid-poll cancellation model does not start receive")
        }

        fn capture(&mut self, _sink: CaptureSink) -> Result<(CaptureReport, CaptureSink)> {
            unreachable!("mid-poll cancellation model does not capture")
        }

        fn stop_receive(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not stop receive")
        }

        fn restore_stock(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not restore an image")
        }

        fn verify_stock(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not verify stock")
        }

        fn shutdown(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not own a process")
        }

        fn terminate(&mut self) -> Result<()> {
            unreachable!("mid-poll cancellation model does not own a process")
        }

        fn is_terminal(&self) -> bool {
            false
        }

        fn is_cleanup_confirmed(&self) -> bool {
            false
        }
    }

    #[test]
    fn control_cancellation_is_sampled_between_status_polls() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut primary = ProcessPrimaryImpl::from_client(
            PollCancellingPrimary::new(Arc::clone(&stop)),
            sink,
            Arc::clone(&stop),
            ProcessLifecycle::new(),
        );

        let error = crate::profile2_control::Controller::new(&mut primary, 4)
            .and_then(|mut controller| {
                controller.apply_stopped(CaptureRequest::new(WORDS, Receiver::Rx1))
            })
            .unwrap_err();

        assert!(format!("{error:#}").contains("cancelled"));
        assert_eq!(primary.client().status_polls, 1);
        assert!(stop.load(Ordering::Relaxed));
    }

    #[derive(Default)]
    struct MockRecovery {
        calls: Vec<&'static str>,
    }

    impl RecoveryClient for MockRecovery {
        fn restore_stock(&mut self) -> Result<()> {
            self.calls.push("restore");
            Ok(())
        }

        fn verify_stock(&mut self) -> Result<()> {
            self.calls.push("verify");
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.calls.push("shutdown");
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            self.calls.push("terminate");
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PanicScenario {
        BeforeLoad,
        SpawnPanic,
        LoadPanicConfirmed,
        LoadPanicClientDropPanic,
        LoadPanickingPayload,
        LoadPanicUnconfirmed,
        LoadPanicTerminatePanic,
        LoadPanicTerminateAfterConfirmPanic,
        HandlerRecoveryPanic,
        ExistingRecoveryPanic,
        ExistingRecoverySpawnPanic,
        RecoveryClientDropPanic,
    }

    struct PanickingPayload;

    impl Drop for PanickingPayload {
        fn drop(&mut self) {
            panic!("synthetic panic-payload destructor panic");
        }
    }

    struct PanicPrimary {
        scenario: PanicScenario,
        cleanup_confirmed: bool,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl PrimaryClient for PanicPrimary {
        fn load_profile2(&mut self) -> Result<()> {
            self.events.borrow_mut().push("load");
            if matches!(
                self.scenario,
                PanicScenario::ExistingRecoveryPanic | PanicScenario::ExistingRecoverySpawnPanic
            ) {
                anyhow::bail!("synthetic load failure before recovery panic");
            }
            if self.scenario == PanicScenario::LoadPanickingPayload {
                std::panic::panic_any(PanickingPayload);
            }
            panic!("synthetic primary load panic");
        }

        fn verify_profile2(&mut self) -> Result<()> {
            unreachable!("panic model stops at load")
        }

        fn read_register(&mut self, _register: ReadRegister) -> Result<u32> {
            unreachable!("panic model never reaches control")
        }

        fn write_shadow(&mut self, _channel: ShadowChannel, _value: u32) -> Result<()> {
            unreachable!("panic model never reaches control")
        }

        fn commit(&mut self) -> Result<()> {
            unreachable!("panic model never reaches control")
        }

        fn configure_receive(&mut self, _plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
            unreachable!("panic model never reaches receive configuration")
        }

        fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
            unreachable!("panic model never reaches receive start")
        }

        fn capture(&mut self, _sink: CaptureSink) -> Result<(CaptureReport, CaptureSink)> {
            unreachable!("panic model never reaches capture")
        }

        fn stop_receive(&mut self) -> Result<()> {
            unreachable!("panic model never reaches receive stop")
        }

        fn restore_stock(&mut self) -> Result<()> {
            unreachable!("panic model uses only fresh recovery")
        }

        fn verify_stock(&mut self) -> Result<()> {
            unreachable!("panic model uses only fresh recovery")
        }

        fn shutdown(&mut self) -> Result<()> {
            unreachable!("panic model never reaches orderly primary shutdown")
        }

        fn terminate(&mut self) -> Result<()> {
            self.events.borrow_mut().push("terminate-primary");
            match self.scenario {
                PanicScenario::LoadPanicUnconfirmed => {
                    anyhow::bail!("synthetic unconfirmed primary cleanup")
                }
                PanicScenario::LoadPanicTerminatePanic => {
                    panic!("synthetic panic during primary termination")
                }
                PanicScenario::LoadPanicTerminateAfterConfirmPanic => {
                    self.cleanup_confirmed = true;
                    panic!("synthetic panic after confirmed primary termination")
                }
                _ => {
                    self.cleanup_confirmed = true;
                    Ok(())
                }
            }
        }

        fn is_terminal(&self) -> bool {
            false
        }

        fn is_cleanup_confirmed(&self) -> bool {
            self.cleanup_confirmed
        }
    }

    impl Drop for PanicPrimary {
        fn drop(&mut self) {
            if self.scenario == PanicScenario::LoadPanicClientDropPanic {
                self.events.borrow_mut().push("drop-primary");
                panic!("synthetic primary-client destructor panic");
            }
        }
    }

    struct PanicRecovery {
        panic_restore: bool,
        panic_drop: bool,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl RecoveryClient for PanicRecovery {
        fn restore_stock(&mut self) -> Result<()> {
            self.events.borrow_mut().push("restore-recovery");
            if self.panic_restore {
                panic!("synthetic fresh-recovery panic");
            }
            Ok(())
        }

        fn verify_stock(&mut self) -> Result<()> {
            self.events.borrow_mut().push("verify-recovery");
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.events.borrow_mut().push("shutdown-recovery");
            Ok(())
        }

        fn terminate(&mut self) -> Result<()> {
            self.events.borrow_mut().push("terminate-recovery");
            Ok(())
        }
    }

    impl Drop for PanicRecovery {
        fn drop(&mut self) {
            if self.panic_drop {
                self.events.borrow_mut().push("drop-recovery");
                panic!("synthetic recovery-client destructor panic");
            }
        }
    }

    struct PanicFactory {
        scenario: PanicScenario,
        sink: Option<CaptureSink>,
        lifecycle: ProcessLifecycle,
        events: Rc<RefCell<Vec<&'static str>>>,
    }

    impl PanicFactory {
        fn new(scenario: PanicScenario, sink: CaptureSink) -> Self {
            Self {
                scenario,
                sink: Some(sink),
                lifecycle: ProcessLifecycle::new(),
                events: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn run(
            &mut self,
            plan: SessionPlan,
            stop: &AtomicBool,
        ) -> std::result::Result<super::super::SessionSuccess<()>, super::super::SessionFailure>
        {
            let lifecycle = self.lifecycle.clone();
            run_with_panic_recovery(plan, self, stop, &lifecycle)
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.borrow().clone()
        }
    }

    impl SessionFactory for PanicFactory {
        type Capture = ProcessCapture;
        type Primary = ProcessPrimaryImpl<PanicPrimary>;
        type Recovery = ProcessRecoveryImpl<PanicRecovery>;
        type Summary = ();

        fn open_primary(&mut self, _plan: SessionPlan) -> Result<Self::Primary> {
            self.events.borrow_mut().push("open-primary");
            if self.scenario == PanicScenario::BeforeLoad {
                panic!("synthetic panic before custom-image load");
            }
            self.lifecycle
                .primary_spawn_attempted
                .store(true, Ordering::Release);
            if self.scenario == PanicScenario::SpawnPanic {
                panic!("synthetic panic during primary spawn");
            }
            let primary = ProcessPrimaryImpl::from_client(
                PanicPrimary {
                    scenario: self.scenario,
                    cleanup_confirmed: false,
                    events: Rc::clone(&self.events),
                },
                self.sink.take().expect("panic factory owns one sink"),
                Arc::new(AtomicBool::new(false)),
                self.lifecycle.clone(),
            );
            self.lifecycle.primary_opened.store(true, Ordering::Release);
            Ok(primary)
        }

        fn open_recovery(&mut self) -> Result<Self::Recovery> {
            claim_recovery(&self.lifecycle)?;
            self.events.borrow_mut().push("open-recovery");
            if self.scenario == PanicScenario::ExistingRecoverySpawnPanic {
                panic!("synthetic panic during recovery spawn");
            }
            Ok(ProcessRecoveryImpl::from_client(
                PanicRecovery {
                    panic_restore: matches!(
                        self.scenario,
                        PanicScenario::HandlerRecoveryPanic | PanicScenario::ExistingRecoveryPanic
                    ),
                    panic_drop: self.scenario == PanicScenario::RecoveryClientDropPanic,
                    events: Rc::clone(&self.events),
                },
                self.lifecycle.clone(),
            ))
        }

        fn validate_capture(
            &mut self,
            _plan: SessionPlan,
            _applied: AppliedConfiguration,
            _readback: ReceiveReadback,
            _capture: Self::Capture,
        ) -> Result<Self::Summary> {
            unreachable!("panic model never validates a capture")
        }
    }

    #[test]
    fn fresh_recovery_clears_obligation_only_after_verification() {
        let lifecycle = ProcessLifecycle::new();
        lifecycle.restoration.arm().unwrap();
        let mut recovery =
            ProcessRecoveryImpl::from_client(MockRecovery::default(), lifecycle.clone());
        recovery.restore_stock().unwrap();
        assert_eq!(
            lifecycle.restoration.state(),
            RestorationState::StockRequired
        );
        recovery.verify_stock().unwrap();
        assert_eq!(
            lifecycle.restoration.state(),
            RestorationState::RecoveryStockVerified
        );
        recovery.shutdown().unwrap();
        assert_eq!(recovery.client().calls, ["restore", "verify", "shutdown"]);
    }

    #[test]
    fn panic_before_load_needs_no_restoration_or_process_cleanup() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::BeforeLoad, sink);
        let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::NotRequired
        );
        assert!(failure.primary.is_some());
        assert!(failure.primary_cleanup.is_none());
        assert!(failure.fresh_recovery.is_none());
        assert_eq!(factory.events(), ["open-primary"]);
    }

    #[test]
    fn panic_during_primary_spawn_retains_unconfirmed_cleanup() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::SpawnPanic, sink);
        let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::NotRequired
        );
        assert!(failure.primary.is_some());
        assert!(failure.primary_cleanup.is_some());
        assert!(failure.fresh_recovery.is_none());
        assert!(!factory.lifecycle.primary_opened.load(Ordering::Acquire));
        assert_eq!(factory.events(), ["open-primary"]);
    }

    #[test]
    fn panic_after_load_recovers_only_after_confirmed_primary_drop_cleanup() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::LoadPanicConfirmed, sink);
        let failure = factory.run(plan(Receiver::Rx2), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::FreshWorkerVerified
        );
        assert_eq!(
            factory.lifecycle.restoration.state(),
            RestorationState::RecoveryStockVerified
        );
        assert_eq!(
            factory.events(),
            [
                "open-primary",
                "load",
                "terminate-primary",
                "open-recovery",
                "restore-recovery",
                "verify-recovery",
                "shutdown-recovery",
            ]
        );
    }

    #[test]
    fn restoration_outcomes_preserve_all_four_internal_states() {
        assert_eq!(
            restoration_outcome(RestorationState::Pristine),
            super::super::RestorationOutcome::NotRequired
        );
        assert_eq!(
            restoration_outcome(RestorationState::StockRequired),
            super::super::RestorationOutcome::Unknown
        );
        assert_eq!(
            restoration_outcome(RestorationState::PrimaryStockVerified),
            super::super::RestorationOutcome::SameHandleVerified
        );
        assert_eq!(
            restoration_outcome(RestorationState::RecoveryStockVerified),
            super::super::RestorationOutcome::FreshWorkerVerified
        );
    }

    #[test]
    fn primary_client_destructor_panic_is_contained_before_recovery() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::LoadPanicClientDropPanic, sink);
        let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::FreshWorkerVerified
        );
        assert!(factory.events().contains(&"drop-primary"));
        assert_eq!(
            factory
                .events()
                .iter()
                .filter(|event| **event == "open-recovery")
                .count(),
            1
        );
    }

    #[test]
    fn confirmed_cleanup_survives_a_late_termination_panic() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory =
            PanicFactory::new(PanicScenario::LoadPanicTerminateAfterConfirmPanic, sink);
        let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::FreshWorkerVerified
        );
        assert_eq!(
            factory
                .events()
                .iter()
                .filter(|event| **event == "open-recovery")
                .count(),
            1
        );
    }

    #[test]
    fn panicking_payload_destructor_cannot_escape_session_guard() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::LoadPanickingPayload, sink);
        let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::FreshWorkerVerified
        );
        assert!(
            format!("{:#}", failure.primary.as_ref().unwrap()).contains("non-string panic payload")
        );
    }

    #[test]
    fn unconfirmed_or_panicking_primary_drop_cleanup_blocks_recovery() {
        for scenario in [
            PanicScenario::LoadPanicUnconfirmed,
            PanicScenario::LoadPanicTerminatePanic,
        ] {
            let stop = Arc::new(AtomicBool::new(false));
            let (_directory, sink) = sink(Arc::clone(&stop));
            let mut factory = PanicFactory::new(scenario, sink);
            let failure = factory.run(plan(Receiver::Rx1), &stop).unwrap_err();

            assert_eq!(
                failure.restoration,
                super::super::RestorationOutcome::Unknown
            );
            assert!(failure.primary_cleanup.is_some());
            assert!(!factory.events().contains(&"open-recovery"));
        }
    }

    #[test]
    fn recovery_panic_is_caught_once_without_overlapping_worker() {
        for scenario in [
            PanicScenario::HandlerRecoveryPanic,
            PanicScenario::ExistingRecoveryPanic,
            PanicScenario::ExistingRecoverySpawnPanic,
        ] {
            let stop = Arc::new(AtomicBool::new(false));
            let (_directory, sink) = sink(Arc::clone(&stop));
            let mut factory = PanicFactory::new(scenario, sink);
            let failure = factory.run(plan(Receiver::Rx2), &stop).unwrap_err();

            assert_eq!(
                failure.restoration,
                super::super::RestorationOutcome::Unknown
            );
            assert!(failure.fresh_recovery.is_some());
            assert!(failure.recovery_cleanup.is_some());
            assert_eq!(
                factory
                    .events()
                    .iter()
                    .filter(|event| **event == "open-recovery")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn recovery_client_destructor_panic_is_contained_after_orderly_shutdown() {
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, sink) = sink(Arc::clone(&stop));
        let mut factory = PanicFactory::new(PanicScenario::RecoveryClientDropPanic, sink);
        let failure = factory.run(plan(Receiver::Rx2), &stop).unwrap_err();

        assert_eq!(
            failure.restoration,
            super::super::RestorationOutcome::FreshWorkerVerified
        );
        assert!(failure.recovery_cleanup.is_none());
        assert!(factory.events().contains(&"drop-recovery"));
        assert_eq!(
            factory
                .events()
                .iter()
                .filter(|event| **event == "open-recovery")
                .count(),
            1
        );
    }

    #[test]
    fn panic_diagnostics_are_utf8_bounded() {
        let payload = "é".repeat(MAX_PANIC_DIAGNOSTIC_BYTES);
        let bounded = bounded_panic_text(&payload);
        assert!(bounded.len() <= MAX_PANIC_DIAGNOSTIC_BYTES);
        assert!(bounded.ends_with("..."));
    }

    #[test]
    fn preflight_stages_private_pinned_inputs_and_retains_their_owner() {
        let fixture = PreflightFixture::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut checks = MockPreflightChecks::new(fixture.custom());
        let events = Rc::clone(&checks.events);
        let config =
            ProcessConfig::preflight_with(fixture.request(), &stop, &mut checks, fixture.stock())
                .unwrap();

        assert_eq!(*events.borrow(), ["usbfs", "custom", "library"]);
        assert_eq!(config.adapter_sha256, fixture.adapter_sha256);
        assert_eq!(config.library_sha256, fixture.library_sha256);
        assert_eq!(config.manifest_sha256, fixture.manifest_sha256);
        assert_eq!(config.usbfs_memory_mb, 64);
        assert!(Arc::ptr_eq(&config.stop, &stop));
        assert_eq!(
            config
                .inputs
                .adapter
                .file
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            ADAPTER_MODE
        );
        for input in [
            &config.inputs.library,
            &config.inputs.custom,
            &config.inputs.stock,
        ] {
            let metadata = input.file.metadata().unwrap();
            assert_eq!(metadata.permissions().mode() & 0o7777, INPUT_MODE);
            assert_eq!(metadata.nlink(), 1);
        }
        let stage = config.inputs.directory.path().to_path_buf();
        fs::remove_file(&fixture.adapter).unwrap();
        fs::remove_file(&fixture.library).unwrap();
        fs::remove_file(&fixture.custom).unwrap();
        fs::remove_file(&fixture.stock).unwrap();
        config.inputs.revalidate_all(&stop).unwrap();
        assert!(stage.exists());
        let adapter_path = config.inputs.adapter.path().to_path_buf();
        let displaced = stage.join("displaced-adapter");
        fs::rename(&adapter_path, &displaced).unwrap();
        fs::write(&adapter_path, b"replacement adapter\n").unwrap();
        fs::set_permissions(&adapter_path, fs::Permissions::from_mode(ADAPTER_MODE)).unwrap();
        assert!(config.inputs.adapter.revalidate(&stop).is_err());
        drop(config);
        assert!(!stage.exists());
    }

    #[test]
    fn preflight_rejects_scalars_and_usbfs_before_artifact_access() {
        let fixture = PreflightFixture::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut invalid_serial = fixture.request();
        invalid_serial.serial = "ABCDEF0123456789abcdef0123456789";
        let mut checks = MockPreflightChecks::new(fixture.custom());
        assert!(
            ProcessConfig::preflight_with(invalid_serial, &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert!(checks.events.borrow().is_empty());

        let mut invalid_timeout = fixture.request();
        invalid_timeout.transaction_timeout = Duration::ZERO;
        let mut checks = MockPreflightChecks::new(fixture.custom());
        assert!(
            ProcessConfig::preflight_with(invalid_timeout, &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert!(checks.events.borrow().is_empty());

        let uppercase_digest = "A".repeat(64);
        let mut invalid_digest = fixture.request();
        invalid_digest.adapter_sha256 = &uppercase_digest;
        let mut checks = MockPreflightChecks::new(fixture.custom());
        assert!(
            ProcessConfig::preflight_with(invalid_digest, &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert!(checks.events.borrow().is_empty());

        let cancelled = Arc::new(AtomicBool::new(true));
        let mut checks = MockPreflightChecks::new(fixture.custom());
        assert!(
            ProcessConfig::preflight_with(
                fixture.request(),
                &cancelled,
                &mut checks,
                fixture.stock()
            )
            .is_err()
        );
        assert!(checks.events.borrow().is_empty());

        let mut checks = MockPreflightChecks::new(fixture.custom());
        checks.usbfs_error = true;
        assert!(
            ProcessConfig::preflight_with(fixture.request(), &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert_eq!(*checks.events.borrow(), ["usbfs"]);

        let mut checks = MockPreflightChecks::new(fixture.custom());
        checks.usbfs_memory_mb = 63;
        assert!(
            ProcessConfig::preflight_with(fixture.request(), &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert_eq!(*checks.events.borrow(), ["usbfs"]);

        let relative_parent = Path::new("relative-stage-parent");
        let mut relative_path = fixture.request();
        relative_path.stage_parent = relative_parent;
        let mut checks = MockPreflightChecks::new(fixture.custom());
        assert!(
            ProcessConfig::preflight_with(relative_path, &stop, &mut checks, fixture.stock())
                .is_err()
        );
        assert!(checks.events.borrow().is_empty());
    }

    #[test]
    fn pinned_staging_rejects_symlinks_digest_drift_and_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"trusted bytes\n").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"trusted bytes\n"));
        let stop = AtomicBool::new(false);
        assert!(
            stage_pinned_exact(
                &source,
                &directory.path().join("bad-digest"),
                &"0".repeat(64),
                None,
                1024,
                INPUT_MODE,
                &stop,
            )
            .is_err()
        );

        let link = directory.path().join("source-link");
        symlink(&source, &link).unwrap();
        assert!(
            stage_pinned_exact(
                &link,
                &directory.path().join("from-link"),
                &digest,
                None,
                1024,
                INPUT_MODE,
                &stop,
            )
            .is_err()
        );

        let cancelled = AtomicBool::new(true);
        let cancelled_destination = directory.path().join("cancelled");
        assert!(
            stage_pinned_exact(
                &source,
                &cancelled_destination,
                &digest,
                None,
                1024,
                INPUT_MODE,
                &cancelled,
            )
            .is_err()
        );
        assert!(!cancelled_destination.exists());
    }

    #[test]
    fn recovery_claim_remains_consumed_when_staged_revalidation_fails() {
        let config = synthetic_process_config();
        let lifecycle = ProcessLifecycle::new();
        lifecycle.restoration.arm().unwrap();
        lifecycle.primary_opened.store(true, Ordering::Release);
        lifecycle
            .primary_cleanup_confirmed
            .store(true, Ordering::Release);

        claim_recovery(&lifecycle).unwrap();
        let adapter = config.inputs.adapter.path().to_path_buf();
        let displaced = config.inputs.directory.path().join("displaced-adapter");
        fs::rename(&adapter, displaced).unwrap();
        fs::write(&adapter, b"replacement adapter\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(ADAPTER_MODE)).unwrap();

        assert!(config.revalidate_recovery(&config.stop).is_err());
        assert!(lifecycle.recovery_started.load(Ordering::Acquire));
        assert!(claim_recovery(&lifecycle).is_err());
    }

    #[test]
    fn device_free_checker_uses_exact_arguments_empty_environment_and_reaps() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("adapter");
        let library = directory.path().join("library");
        let marker = directory.path().join("checked");
        fs::write(&library, b"synthetic library\n").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"synthetic library\n"));
        let script = format!(
            "#!/bin/sh\n\
             test \"$#\" -eq 6 || exit 21\n\
             test \"$1\" = check-native-library || exit 22\n\
             test \"$2\" = --library || exit 23\n\
             test \"$3\" = '{}' || exit 24\n\
             test \"$4\" = --sha256 || exit 25\n\
             test \"$5\" = '{}' || exit 26\n\
             test \"$6\" = --require-bladerf2-loader-neutral-open || exit 27\n\
             test -z \"${{LD_PRELOAD+x}}\" || exit 28\n\
             test -z \"${{LD_AUDIT+x}}\" || exit 29\n\
             test -z \"${{LD_LIBRARY_PATH+x}}\" || exit 30\n\
             : > '{}'\n",
            library.display(),
            digest,
            marker.display()
        );
        fs::write(&adapter, script).unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(ADAPTER_MODE)).unwrap();
        let stop = AtomicBool::new(false);
        run_device_free_library_check(
            &adapter,
            &library,
            &digest,
            directory.path(),
            Duration::from_secs(1),
            &stop,
        )
        .unwrap();
        assert!(marker.exists());

        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&adapter, "#!/bin/sh\nexit 7\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(ADAPTER_MODE)).unwrap();
        let error = run_device_free_library_check(
            &adapter,
            &library,
            &digest,
            directory.path(),
            Duration::from_secs(1),
            &stop,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("exited"));

        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&adapter, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        fs::set_permissions(&adapter, fs::Permissions::from_mode(ADAPTER_MODE)).unwrap();
        let error = run_device_free_library_check(
            &adapter,
            &library,
            &digest,
            directory.path(),
            Duration::from_millis(25),
            &stop,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("exceeded"));

        let missing = directory.path().join("missing-adapter");
        let error = run_device_free_library_check(
            &missing,
            &library,
            &digest,
            directory.path(),
            Duration::from_secs(1),
            &stop,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("start staged"));

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_source = Arc::clone(&cancelled);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            cancel_source.store(true, Ordering::Relaxed);
        });
        let error = run_device_free_library_check(
            &adapter,
            &library,
            &digest,
            directory.path(),
            Duration::from_secs(1),
            &cancelled,
        )
        .unwrap_err();
        canceller.join().unwrap();
        assert!(format!("{error:#}").contains("cancelled"));
    }

    #[test]
    fn bundle_claim_and_capture_report_are_validated_without_spawning() {
        let config = synthetic_process_config();
        let bundle = config.session_bundle().unwrap();
        assert_eq!(bundle.custom().sha256(), CUSTOM_SHA256);
        assert_eq!(bundle.stock().sha256(), REQUIRED_STOCK_SHA256);

        let plan = wire_plan(plan(Receiver::Rx1)).unwrap();
        assert!(
            validate_capture_report(
                plan,
                &CaptureReport::new(
                    plan.expected_transport_samples(),
                    plan.expected_raw_bytes(),
                    "f".repeat(64),
                ),
            )
            .is_ok()
        );
        assert!(
            validate_capture_report(
                plan,
                &CaptureReport::new(1, plan.expected_raw_bytes(), "f".repeat(64)),
            )
            .is_err()
        );
    }

    #[test]
    fn closed_process_sink_is_strictly_validated_and_reconciled() {
        let plan = plan(Receiver::Rx2);
        let applied = exercise_reference_contract(plan.request, 0).unwrap();
        let raw = valid_stream(plan, applied);
        let digest = format!("{:x}", Sha256::digest(&raw));
        let stop = Arc::new(AtomicBool::new(false));
        let (_directory, mut sink) = sink(stop);
        sink.write_all(&raw).unwrap();
        sink.flush().unwrap();
        let configured = ConfiguredReceiveReadback::new(
            ReceiveChannel::Rx2,
            plan.center_hz,
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        let active = ActiveReceiveReadback::new(configured, plan.gain_db);
        let report = CaptureReport::new(
            ((plan.records + 1) * 8) as u32,
            (plan.records + 1) * 32,
            digest,
        );
        let summary = validate_process_capture(
            plan,
            applied,
            ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx2),
            ProcessCapture {
                sink,
                report,
                configured,
                active,
            },
        )
        .unwrap();
        assert_eq!(summary.stream.data_records, plan.records);
        assert_eq!(summary.stream.config.epoch, applied.epoch());
        assert_eq!(summary.report.sha256(), summary.stream.processed_sha256);
        assert_eq!(
            summary.stage.path.file_name().unwrap(),
            "capture.sc16q11.partial"
        );
        let metadata = summary.stage.file.metadata().unwrap();
        assert_eq!(metadata.dev(), summary.stage.device);
        assert_eq!(metadata.ino(), summary.stage.inode);
        assert_eq!(metadata.len(), summary.stage.bytes);
    }

    fn successful_process_result(
        plan: SessionPlan,
        mut sink: CaptureSink,
    ) -> super::super::SessionSuccess<ProcessSummary> {
        let applied = exercise_reference_contract(plan.request, 0).unwrap();
        let raw = valid_stream(plan, applied);
        let digest = format!("{:x}", Sha256::digest(&raw));
        sink.write_all(&raw).unwrap();
        sink.flush().unwrap();
        let configured = ConfiguredReceiveReadback::new(
            ReceiveChannel::Rx2,
            plan.center_hz,
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        let active = ActiveReceiveReadback::new(configured, plan.gain_db);
        let report = CaptureReport::new(
            ((plan.records + 1) * 8) as u32,
            (plan.records + 1) * 32,
            digest,
        );
        let readback = ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx2);
        let summary = validate_process_capture(
            plan,
            applied,
            readback,
            ProcessCapture {
                sink,
                report,
                configured,
                active,
            },
        )
        .unwrap();
        super::super::SessionSuccess {
            applied,
            readback,
            summary,
            restoration: super::super::RestorationOutcome::SameHandleVerified,
        }
    }

    fn prepared_publication(
        output_name: &str,
    ) -> (
        tempfile::TempDir,
        SessionPublication,
        super::super::SessionSuccess<ProcessSummary>,
    ) {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requested_plan = plan(Receiver::Rx2);
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new(output_name),
            requested_plan,
            Arc::clone(&stop),
        )
        .unwrap();
        let mut config = synthetic_process_config();
        config.stop = Arc::clone(&stop);
        publication.bind_preflight(&config).unwrap();
        let sink = publication.stage.take_sink(Arc::clone(&stop)).unwrap();
        let success = successful_process_result(requested_plan, sink);
        (parent, publication, success)
    }

    #[test]
    fn composition_preflight_cancellation_creates_no_namespace_or_factory() {
        let fixture = PreflightFixture::new();
        let stop = Arc::new(AtomicBool::new(true));
        let error = run_process_session(
            ProcessSessionRequest {
                preflight: fixture.request(),
                plan: plan(Receiver::Rx2),
                output_basename: OsStr::new("cancelled-before-publication"),
            },
            stop,
        )
        .unwrap_err();

        assert!(matches!(error, ProcessSessionFailure::Preflight(_)));
        assert!(
            fs::read_dir(&fixture.stage_parent)
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn composition_publication_collision_preserves_destination_and_skips_factory() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let output = parent.path().join("composition-collision");
        fs::create_dir(&output).unwrap();
        let sentinel = output.join("sentinel");
        fs::write(&sentinel, b"preserve me\n").unwrap();
        let factory_called = Rc::new(Cell::new(false));
        let factory_called_by_constructor = Rc::clone(&factory_called);

        let error = compose_process_session_with(
            parent.path(),
            OsStr::new("composition-collision"),
            plan(Receiver::Rx2),
            Arc::new(AtomicBool::new(false)),
            || Ok(()),
            move |(), _publication| {
                factory_called_by_constructor.set(true);
                Ok(())
            },
            |()| unreachable!("publication collision cannot run a factory"),
        )
        .unwrap_err();

        let setup_text = error.to_string();
        assert!(matches!(error, ProcessSessionFailure::PublicationSetup(_)));
        assert!(setup_text.contains("empty requested output directory may remain"));
        assert!(setup_text.contains("no terminal record is guaranteed"));
        assert!(!factory_called.get());
        assert_eq!(fs::read(sentinel).unwrap(), b"preserve me\n");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    }

    #[test]
    fn composition_factory_failure_terminalizes_without_running_a_child() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let preflight_stop = Arc::clone(&stop);
        let run_called = Rc::new(Cell::new(false));
        let run_called_by_runner = Rc::clone(&run_called);
        let session_plan = plan(Receiver::Rx2);

        let error = compose_process_session_with(
            parent.path(),
            OsStr::new("composition-factory-failure"),
            session_plan,
            stop,
            move || {
                let mut config = synthetic_process_config();
                config.stop = preflight_stop;
                Ok(config)
            },
            |config, publication| {
                let moved = publication
                    .stage
                    .directory
                    .path
                    .join("capture-stage-substitution");
                fs::rename(&publication.stage.path, moved)?;
                ProcessFactory::new(config, publication)
            },
            move |factory| {
                run_called_by_runner.set(true);
                factory.run(session_plan)
            },
        )
        .unwrap_err();

        let ProcessSessionFailure::Publication(failure) = error else {
            panic!("factory failure must retain publication evidence")
        };
        assert!(!run_called.get());
        assert_eq!(failure.capture, CaptureDisposition::None);
        assert_eq!(failure.capture_facts["path_identity_verified"], false);
        assert!(failure.terminal_linked);
        assert!(failure.terminal_durable);
        let output = parent.path().join("composition-factory-failure");
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["restoration"], "not_required");
        assert!(
            report["diagnostics"]["factory"]
                .as_str()
                .unwrap()
                .contains("construct profile-2 process factory")
        );
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
    }

    #[test]
    fn composition_factory_panic_is_bounded_and_terminalized_once() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let preflight_stop = Arc::clone(&stop);
        let run_called = Rc::new(Cell::new(false));
        let run_called_by_runner = Rc::clone(&run_called);
        let session_plan = plan(Receiver::Rx2);

        let error = compose_process_session_with(
            parent.path(),
            OsStr::new("composition-factory-panic"),
            session_plan,
            stop,
            move || {
                let mut config = synthetic_process_config();
                config.stop = preflight_stop;
                Ok(config)
            },
            |_config, _publication| -> Result<()> {
                panic!("synthetic composition constructor panic")
            },
            move |()| {
                run_called_by_runner.set(true);
                unreachable!("panicking constructor cannot return a factory")
            },
        )
        .unwrap_err();

        let ProcessSessionFailure::Publication(failure) = error else {
            panic!("constructor panic must retain publication evidence")
        };
        assert!(!run_called.get());
        assert!(failure.terminal_linked);
        assert!(failure.terminal_durable);
        let output = parent.path().join("composition-factory-panic");
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["restoration"], "not_required");
        assert_eq!(report["plan"], plan_terminal_facts(session_plan));
        assert!(report["preflight"].is_null());
        assert!(
            report["diagnostics"]["factory"]
                .as_str()
                .unwrap()
                .contains("synthetic composition constructor panic")
        );
        assert_eq!(fs::read_dir(output).unwrap().count(), 1);
    }

    #[test]
    fn composition_run_failure_retains_partial_without_promoting_it() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let preflight_stop = Arc::clone(&stop);
        let sink_stop = Arc::clone(&stop);
        let input_stage = Rc::new(RefCell::new(None));
        let input_stage_from_preflight = Rc::clone(&input_stage);
        let input_stage_in_runner = Rc::clone(&input_stage);
        let session_plan = plan(Receiver::Rx2);
        let expected_preflight = {
            let config = synthetic_process_config();
            preflight_terminal_facts(&PublicationPreflightFacts::from_config(&config))
        };

        let error = compose_process_session_with(
            parent.path(),
            OsStr::new("composition-run-failure"),
            session_plan,
            stop,
            move || {
                let mut config = synthetic_process_config();
                config.stop = preflight_stop;
                input_stage_from_preflight
                    .borrow_mut()
                    .replace(config.inputs.directory.path().to_path_buf());
                Ok(config)
            },
            move |config, publication| {
                ensure!(
                    Arc::ptr_eq(&config.stop, &publication.stop),
                    "composition changed the cancellation token"
                );
                publication.bind_preflight(&config)?;
                let sink = publication.stage.take_sink(Arc::clone(&sink_stop))?;
                ensure!(
                    Arc::ptr_eq(&config.stop, &sink.stop),
                    "capture sink did not retain the shared cancellation token"
                );
                Ok((config, sink))
            },
            move |(_config, mut sink)| {
                assert!(input_stage_in_runner.borrow().as_ref().unwrap().exists());
                sink.write_all(b"partial").unwrap();
                sink.flush().unwrap();
                drop(sink);
                let mut failure = super::super::SessionFailure::new();
                failure.primary(anyhow!("synthetic composed run failure"));
                Err(failure)
            },
        )
        .unwrap_err();

        let ProcessSessionFailure::Publication(failure) = error else {
            panic!("run failure must retain publication evidence")
        };
        assert_eq!(failure.capture, CaptureDisposition::PartialRetained);
        assert!(failure.terminal_linked);
        assert!(failure.terminal_durable);
        assert_eq!(failure.capture_facts["bytes"], 7);
        assert!(failure.final_capture_basename.is_none());
        assert!(failure.to_string().contains("retained_stage_directory="));
        let retained = failure.capture_facts["retained_stage_directory"]
            .as_str()
            .unwrap();
        assert_eq!(
            fs::read(parent.path().join(retained).join(CAPTURE_STAGE_NAME)).unwrap(),
            b"partial"
        );
        let output = parent.path().join("composition-run-failure");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["restoration"], "not_required");
        assert_eq!(report["plan"], plan_terminal_facts(session_plan));
        assert_eq!(report["preflight"], expected_preflight);
        assert_eq!(report["capture"]["disposition"], "partial_retained");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        assert!(!input_stage.borrow().as_ref().unwrap().exists());
    }

    #[test]
    fn composition_success_owns_inputs_through_run_and_publishes_terminal_last() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let preflight_stop = Arc::clone(&stop);
        let sink_stop = Arc::clone(&stop);
        let input_stage = Rc::new(RefCell::new(None));
        let input_stage_from_preflight = Rc::clone(&input_stage);
        let input_stage_in_runner = Rc::clone(&input_stage);
        let session_plan = plan(Receiver::Rx2);

        let receipt = compose_process_session_with(
            parent.path(),
            OsStr::new("composition-success"),
            session_plan,
            stop,
            move || {
                let mut config = synthetic_process_config();
                config.stop = preflight_stop;
                input_stage_from_preflight
                    .borrow_mut()
                    .replace(config.inputs.directory.path().to_path_buf());
                Ok(config)
            },
            move |config, publication| {
                ensure!(
                    Arc::ptr_eq(&config.stop, &publication.stop),
                    "composition changed the cancellation token"
                );
                publication.bind_preflight(&config)?;
                let sink = publication.stage.take_sink(Arc::clone(&sink_stop))?;
                ensure!(
                    Arc::ptr_eq(&config.stop, &sink.stop),
                    "capture sink did not retain the shared cancellation token"
                );
                Ok((config, sink))
            },
            move |(_config, sink)| {
                assert!(input_stage_in_runner.borrow().as_ref().unwrap().exists());
                Ok(successful_process_result(session_plan, sink))
            },
        )
        .unwrap();

        assert!(!input_stage.borrow().as_ref().unwrap().exists());
        let mut entries = fs::read_dir(&receipt.output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                OsString::from(CAPTURE_FINAL_NAME),
                OsString::from(TERMINAL_FINAL_NAME)
            ]
        );
        let report: Value =
            serde_json::from_slice(&fs::read(receipt.output.join(TERMINAL_FINAL_NAME)).unwrap())
                .unwrap();
        assert_eq!(report["outcome"], "success");
        assert_eq!(report["preflight"]["usbfs_memory_mb"], 64);
        assert_eq!(report["capture_file"]["basename"], CAPTURE_FINAL_NAME);
        assert_eq!(report["safety"]["tx_stream_enabled"], false);
        assert_eq!(report["safety"]["flash_written"], false);
    }

    struct TestHook {
        step: PublicationStep,
        fail: bool,
        cancel: bool,
        create_terminal: Option<PathBuf>,
    }

    impl PublicationHooks for TestHook {
        fn reached(&mut self, step: PublicationStep, stop: &Arc<AtomicBool>) -> Result<()> {
            if step != self.step {
                return Ok(());
            }
            if self.cancel {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(path) = self.create_terminal.take() {
                fs::write(path, b"sentinel terminal\n")?;
            }
            ensure!(!self.fail, "injected publication failure at {step:?}");
            Ok(())
        }
    }

    #[test]
    fn publication_success_links_exact_capture_and_writes_one_terminal_last() {
        let (_parent, publication, success) = prepared_publication("published-success");
        let expected_digest = success.summary.report.sha256().to_owned();
        let receipt = publication.terminalize(Ok(success)).unwrap();
        let mut entries = fs::read_dir(&receipt.output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            [
                OsString::from(CAPTURE_FINAL_NAME),
                OsString::from(TERMINAL_FINAL_NAME)
            ]
        );

        let capture_path = receipt.output.join(CAPTURE_FINAL_NAME);
        let capture = fs::metadata(&capture_path).unwrap();
        assert_eq!(capture.dev(), receipt.capture.identity.device);
        assert_eq!(capture.ino(), receipt.capture.identity.inode);
        assert_eq!(capture.nlink(), 1);
        assert_eq!(capture.permissions().mode() & 0o7777, 0o600);
        let digest = format!("{:x}", Sha256::digest(fs::read(&capture_path).unwrap()));
        assert_eq!(digest, expected_digest);

        let terminal_path = receipt.output.join(TERMINAL_FINAL_NAME);
        let terminal_metadata = fs::metadata(&terminal_path).unwrap();
        assert_eq!(terminal_metadata.ino(), receipt.terminal.identity.inode);
        assert_eq!(terminal_metadata.nlink(), 1);
        assert_eq!(terminal_metadata.permissions().mode() & 0o7777, 0o600);
        let report: Value = serde_json::from_slice(&fs::read(&terminal_path).unwrap()).unwrap();
        assert_eq!(report["outcome"], "success");
        assert_eq!(report["capture_file"]["basename"], CAPTURE_FINAL_NAME);
        assert_eq!(report["safety"]["tx_stream_enabled"], false);
        assert_eq!(report["safety"]["flash_written"], false);
        assert_eq!(report["plan"]["receiver"], "RX2");
        assert_eq!(report["coordinator_readback"]["verified"], true);

        let snapshot = fs::read(&terminal_path).unwrap();
        drop(receipt);
        assert_eq!(fs::read(&terminal_path).unwrap(), snapshot);
    }

    #[test]
    fn session_failure_retains_partial_and_never_promotes_it() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new("published-failure"),
            plan(Receiver::Rx2),
            Arc::clone(&stop),
        )
        .unwrap();
        let mut config = synthetic_process_config();
        config.stop = Arc::clone(&stop);
        publication.bind_preflight(&config).unwrap();
        let mut sink = publication.stage.take_sink(Arc::clone(&stop)).unwrap();
        sink.write_all(b"partial capture evidence").unwrap();
        sink.flush().unwrap();
        drop(sink);

        let mut failure = super::super::SessionFailure::new();
        failure.primary(anyhow!("synthetic session failure"));
        failure.restoration = super::super::RestorationOutcome::FreshWorkerVerified;
        let error = publication.terminalize(Err(failure)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::PartialRetained);
        assert!(error.terminal_linked);
        assert!(error.terminal_durable);
        let retained_stage = error.capture_facts["retained_stage_directory"]
            .as_str()
            .unwrap()
            .to_owned();
        drop(error);
        let output = parent.path().join("published-failure");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["capture"]["disposition"], "partial_retained");
        assert_eq!(report["capture"]["bytes"], 24);
        assert_eq!(report["capture"]["path_identity_verified"], true);
        assert_eq!(report["restoration"], "fresh_worker_verified");
        assert_eq!(
            fs::read(parent.path().join(retained_stage).join(CAPTURE_STAGE_NAME)).unwrap(),
            b"partial capture evidence"
        );
    }

    #[test]
    fn escaped_failure_diagnostics_fit_the_terminal_record_bound() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new("escaped-failure-diagnostics"),
            plan(Receiver::Rx2),
            Arc::clone(&stop),
        )
        .unwrap();
        let mut config = synthetic_process_config();
        config.stop = stop;
        publication.bind_preflight(&config).unwrap();

        let control_bytes = "\0".repeat(MAX_FAILURE_TEXT_BYTES * 2);
        let mut failure = super::super::SessionFailure::new();
        failure.primary(anyhow!(control_bytes.clone()));
        failure.same_handle_restore = Some(anyhow!(control_bytes.clone()));
        failure.fresh_recovery = Some(anyhow!(control_bytes.clone()));
        failure.primary_cleanup = Some(anyhow!(control_bytes.clone()));
        failure.recovery_cleanup = Some(anyhow!(control_bytes));
        failure.restoration = super::super::RestorationOutcome::Unknown;

        let error = publication.terminalize(Err(failure)).unwrap_err();
        assert!(error.terminal_linked);
        assert!(error.terminal_durable);
        let terminal = fs::read(
            parent
                .path()
                .join("escaped-failure-diagnostics")
                .join(TERMINAL_FINAL_NAME),
        )
        .unwrap();
        assert!(terminal.len() <= MAX_TERMINAL_BYTES);
        let report: Value = serde_json::from_slice(&terminal).unwrap();
        assert_eq!(report["outcome"], "failure");
    }

    #[test]
    fn cancellation_before_commit_publishes_failure_without_capture() {
        let (parent, publication, success) = prepared_publication("cancelled-before-commit");
        publication.stop.store(true, Ordering::Relaxed);
        let error = publication.terminalize(Ok(success)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::PartialRetained);
        assert!(error.terminal_linked);
        let output = parent.path().join("cancelled-before-commit");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert!(
            report["diagnostics"]["publication"]
                .as_str()
                .unwrap()
                .contains("interrupted")
        );
    }

    #[test]
    fn inconsistent_success_fields_fail_before_capture_commit() {
        let (parent, publication, mut success) = prepared_publication("inconsistent-success");
        success.readback = ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx1);

        let error = publication.terminalize(Ok(success)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::PartialRetained);
        assert!(error.terminal_linked);
        let output = parent.path().join("inconsistent-success");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert!(
            report["diagnostics"]["publication"]
                .as_str()
                .unwrap()
                .contains("do not agree")
        );
    }

    #[test]
    fn cancellation_after_capture_commit_cannot_interrupt_terminalization() {
        let (_parent, publication, success) = prepared_publication("cancelled-after-commit");
        let mut hook = TestHook {
            step: PublicationStep::CaptureCommitted,
            fail: false,
            cancel: true,
            create_terminal: None,
        };
        let receipt = publication
            .terminalize_with_hooks(Ok(success), &mut hook)
            .unwrap();
        assert!(receipt.output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(receipt.output.join(TERMINAL_FINAL_NAME)).unwrap())
                .unwrap();
        assert_eq!(report["outcome"], "success");
    }

    #[test]
    fn failure_after_capture_link_rechecks_and_records_durable_capture() {
        let (parent, publication, success) = prepared_publication("post-link-failure");
        let mut hook = TestHook {
            step: PublicationStep::CaptureCommitted,
            fail: true,
            cancel: false,
            create_terminal: None,
        };
        let error = publication
            .terminalize_with_hooks(Ok(success), &mut hook)
            .unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::Published);
        assert_eq!(error.final_capture_basename, Some(CAPTURE_FINAL_NAME));
        assert!(error.terminal_linked);
        let output = parent.path().join("post-link-failure");
        assert!(output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["capture"]["disposition"], "published");
        assert_eq!(report["capture"]["durability_confirmed"], true);
        assert_eq!(report["capture"]["final_basename"], CAPTURE_FINAL_NAME);
    }

    #[test]
    fn descriptor_link_uses_retained_inode_and_eexist_preserves_destination() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new("descriptor-link"),
            plan(Receiver::Rx2),
            Arc::clone(&stop),
        )
        .unwrap();
        let mut sink = publication.stage.take_sink(stop).unwrap();
        sink.write_all(b"retained inode").unwrap();
        sink.flush().unwrap();
        drop(sink);
        let retained = publication.stage.reader.metadata().unwrap();
        let moved = publication.stage.directory.path.join("moved.partial");
        fs::rename(&publication.stage.path, &moved).unwrap();
        fs::write(&publication.stage.path, b"replacement!!").unwrap();
        fs::set_permissions(
            &publication.stage.path,
            fs::Permissions::from_mode(PUBLICATION_FILE_MODE),
        )
        .unwrap();

        let linked = link_fd_no_clobber(
            &publication.stage.reader,
            &publication.output,
            OsStr::new(CAPTURE_FINAL_NAME),
        )
        .unwrap();
        assert_eq!(linked.inode, retained.ino());
        let final_bytes = fs::read(publication.output.path.join(CAPTURE_FINAL_NAME)).unwrap();
        assert_eq!(final_bytes, b"retained inode");

        let error = link_fd_no_clobber(
            &publication.stage.reader,
            &publication.output,
            OsStr::new(CAPTURE_FINAL_NAME),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("EEXIST"));
        assert_eq!(
            fs::read(publication.output.path.join(CAPTURE_FINAL_NAME)).unwrap(),
            b"retained inode"
        );
    }

    #[test]
    fn high_level_stage_path_substitution_fails_closed() {
        let (parent, publication, success) = prepared_publication("stage-substitution");
        let moved = publication.stage.directory.path.join("moved.partial");
        fs::rename(&publication.stage.path, &moved).unwrap();
        fs::write(
            &publication.stage.path,
            vec![0u8; success.summary.stage.bytes as usize],
        )
        .unwrap();
        fs::set_permissions(
            &publication.stage.path,
            fs::Permissions::from_mode(PUBLICATION_FILE_MODE),
        )
        .unwrap();
        let error = publication.terminalize(Ok(success)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::PartialRetained);
        let output = parent.path().join("stage-substitution");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["capture"]["path_identity_verified"], false);
    }

    #[test]
    fn accepted_capture_digest_mutation_is_rejected_before_commit() {
        let (parent, publication, success) = prepared_publication("capture-mutation");
        let mut changed = OpenOptions::new()
            .write(true)
            .open(&publication.stage.path)
            .unwrap();
        changed.write_all(&[0xff]).unwrap();
        changed.sync_all().unwrap();
        drop(changed);
        let error = publication.terminalize(Ok(success)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::PartialRetained);
        let output = parent.path().join("capture-mutation");
        assert!(!output.join(CAPTURE_FINAL_NAME).exists());
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert!(
            report["diagnostics"]["publication"]
                .as_str()
                .unwrap()
                .contains("changed")
        );
    }

    #[test]
    fn output_path_replacement_does_not_redirect_dirfd_link_and_is_detected() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new("replace-output"),
            plan(Receiver::Rx2),
            Arc::clone(&stop),
        )
        .unwrap();
        let mut sink = publication.stage.take_sink(stop).unwrap();
        sink.write_all(b"pinned output").unwrap();
        sink.flush().unwrap();
        drop(sink);

        let moved_output = parent.path().join("original-output-moved");
        fs::rename(&publication.output.path, &moved_output).unwrap();
        fs::create_dir(&publication.output.path).unwrap();
        fs::set_permissions(
            &publication.output.path,
            fs::Permissions::from_mode(OUTPUT_DIRECTORY_MODE),
        )
        .unwrap();
        link_fd_no_clobber(
            &publication.stage.reader,
            &publication.output,
            OsStr::new(CAPTURE_FINAL_NAME),
        )
        .unwrap();
        assert!(moved_output.join(CAPTURE_FINAL_NAME).exists());
        assert!(!publication.output.path.join(CAPTURE_FINAL_NAME).exists());
        assert!(publication.output.revalidate_path().is_err());
    }

    #[test]
    fn same_filesystem_is_required_before_descriptor_link() {
        let parent = tempfile::tempdir().unwrap();
        fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut publication = SessionPublication::create(
            parent.path(),
            OsStr::new("same-filesystem"),
            plan(Receiver::Rx2),
            Arc::clone(&stop),
        )
        .unwrap();
        assert_eq!(
            publication.stage.directory.device,
            publication.output.device
        );
        publication.output.device = publication.output.device.wrapping_add(1);
        let error = link_fd_no_clobber(
            &publication.stage.reader,
            &publication.output,
            OsStr::new(CAPTURE_FINAL_NAME),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("same filesystem"));
        assert!(!publication.output.path.join(CAPTURE_FINAL_NAME).exists());
    }

    #[test]
    fn terminal_eexist_is_ambiguous_and_never_creates_an_alternate_terminal() {
        let (_parent, publication, success) = prepared_publication("terminal-eexist");
        let terminal_path = publication.output.path.join(TERMINAL_FINAL_NAME);
        let mut hook = TestHook {
            step: PublicationStep::BeforeTerminalCommit,
            fail: false,
            cancel: false,
            create_terminal: Some(terminal_path.clone()),
        };
        let error = publication
            .terminalize_with_hooks(Ok(success), &mut hook)
            .unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::Published);
        assert!(!error.terminal_linked);
        let retained_stage = error.capture_facts["retained_stage_directory"]
            .as_str()
            .expect("ambiguous terminal failure retains its private stage");
        let display = error.to_string();
        assert!(display.contains(&format!("retained_stage_directory={retained_stage}")));
        assert!(display.contains("terminalization ambiguity"));
        assert_eq!(fs::read(&terminal_path).unwrap(), b"sentinel terminal\n");
        assert!(
            !terminal_path
                .parent()
                .unwrap()
                .join("session-failure.json")
                .exists()
        );
    }

    #[test]
    fn success_terminal_staging_failure_uses_one_failure_terminal() {
        let (_parent, publication, success) = prepared_publication("terminal-stage-failure");
        let output = publication.output.path.clone();
        let success_stage = publication
            .stage
            .directory
            .path
            .join(SUCCESS_TERMINAL_STAGE_NAME);
        fs::write(&success_stage, b"occupied success stage\n").unwrap();
        fs::set_permissions(
            &success_stage,
            fs::Permissions::from_mode(PUBLICATION_FILE_MODE),
        )
        .unwrap();
        let error = publication.terminalize(Ok(success)).unwrap_err();
        assert_eq!(error.capture, CaptureDisposition::Published);
        assert!(error.terminal_linked);
        let report: Value =
            serde_json::from_slice(&fs::read(output.join(TERMINAL_FINAL_NAME)).unwrap()).unwrap();
        assert_eq!(report["outcome"], "failure");
        assert_eq!(report["capture"]["disposition"], "published");
        let terminal_count = fs::read_dir(&output)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() == OsStr::new(TERMINAL_FINAL_NAME))
            .count();
        assert_eq!(terminal_count, 1);
    }

    #[test]
    fn error_after_terminal_link_never_attempts_a_fallback_terminal() {
        let (_parent, publication, success) = prepared_publication("terminal-post-link-error");
        let terminal_path = publication.output.path.join(TERMINAL_FINAL_NAME);
        let mut hook = TestHook {
            step: PublicationStep::TerminalCommitted,
            fail: true,
            cancel: false,
            create_terminal: None,
        };
        let error = publication
            .terminalize_with_hooks(Ok(success), &mut hook)
            .unwrap_err();
        assert!(error.terminal_linked);
        assert!(!error.terminal_durable);
        let report: Value = serde_json::from_slice(&fs::read(&terminal_path).unwrap()).unwrap();
        assert_eq!(report["outcome"], "success");
        assert!(
            !terminal_path
                .parent()
                .unwrap()
                .join("session-failure.json")
                .exists()
        );
    }
}
