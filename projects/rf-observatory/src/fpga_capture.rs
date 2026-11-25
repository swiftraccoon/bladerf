//! Guarded, receive-only capture for the qualified profile-1 FPGA image.
//!
//! The pinned native CLI owns FPGA loading and the raw receive stream. This
//! module verifies every local input before device access, restores a known
//! stock image immediately after the native process exits, and only then
//! validates the complete framed recording. It never writes FPGA flash or
//! starts a transmit stream.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::transport;

const PROFILE_ID: u16 = 1;
const PROFILE_RECORD_RATE: u64 = 1_000_000;
const TRANSPORT_SLOTS_PER_RECORD: u64 = 8;
const BYTES_PER_RECORD: u64 = 32;
const MIN_USBFS_MEMORY_MB: u64 = 64;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
const STOCK_CATALOG_PATH: &str = "downloads/hostedxA9-v0.16.0.rbf";
const PINNED_CLI_VERSION: &str = "1.10.0-git-fcf94233";
const PINNED_LIB_VERSION: &str = "2.6.0-git-fcf94233";
const QUALIFIED_QUARTUS_VERSION: &str = "23.1std.1 Build 993, Lite Edition";
const QUALIFIED_NIOS_CONFIGURATION: &str = "tiny";
const QUALIFIED_FITTER_SEED: u32 = 1;
const QUALIFIED_TIMING_CHECKS: u32 = 184;
const CLI_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_FILE: &str = "channel-bank.frames";
const STAGED_CLI_FILE: &str = "bladeRF-cli";
const STAGED_LIB_FILE: &str = "libbladeRF.so.2";
const CUSTOM_IMAGE_FILE: &str = "custom-profile1.rbf";
const RECOVERY_IMAGE_FILE: &str = "stock-recovery.rbf";
const CAPTURE_SCRIPT: &str = "capture.cli";
const RESTORE_SCRIPT: &str = "restore.cli";
const ACTIVE_GAIN_BEGIN: &str = "RF_OBSERVATORY_ACTIVE_GAIN_BEGIN";
const ACTIVE_GAIN_END: &str = "RF_OBSERVATORY_ACTIVE_GAIN_END";

/// Capture a bounded custom-FPGA receive session and restore stock gateware.
#[derive(Debug, Args)]
pub(crate) struct CaptureArgs {
    /// RF center frequency in MHz.
    #[arg(long, default_value_t = 98.1)]
    center_mhz: f64,
    /// Profile-1 records, 100,000 through 10,000,000 (0.1 through 10 seconds).
    #[arg(long, default_value_t = 100_000, value_parser = clap::value_parser!(u64).range(100_000..=10_000_000))]
    records: u64,
    /// Integer manual receive gain in dB, from -16 through 60.
    #[arg(long, default_value_t = 40, allow_negative_numbers = true, value_parser = clap::value_parser!(i32).range(-16..=60))]
    gain: i32,
    /// Exactly one physical receive connector, RX1 or RX2.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=2))]
    antenna: u8,
    /// Fresh private session directory without ':', ';', or '$'.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Qualified custom-image manifest; its sibling image is verified and loaded.
    #[arg(long)]
    manifest: PathBuf,
    /// Known stock xA9 recovery image, verified against config/images.sha256.
    #[arg(long)]
    recovery_image: PathBuf,
    /// bladeRF-cli built from the pinned source, beside its libbladeRF.so.2.
    #[arg(long)]
    blade_rf_cli: PathBuf,
    /// Complete 32-digit hexadecimal bladeRF serial selecting one device.
    #[arg(long)]
    serial: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ImageManifest {
    schema_version: u32,
    image_file: String,
    image_sha256: String,
    image_bytes: u64,
    device: String,
    profile_id: u16,
    workspace_source_commit: String,
    nuand_source_commit: String,
    integration_patch_sha256: String,
    hdl_source_sha256: BTreeMap<String, String>,
    quartus: String,
    nios_configuration: String,
    fitter_seed: u32,
    timing: TimingQualification,
}

#[derive(Clone, Debug, Deserialize)]
struct TimingQualification {
    checks: u32,
    negative_checks: u32,
    minimum_setup_ns: f64,
    minimum_hold_ns: f64,
    minimum_recovery_ns: f64,
    minimum_removal_ns: f64,
    minimum_pulse_width_ns: f64,
    constraints_relaxed: bool,
}

impl TimingQualification {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.checks == QUALIFIED_TIMING_CHECKS,
            "manifest timing check count differs from the qualified profile"
        );
        ensure!(
            self.negative_checks == 0,
            "manifest records failing timing checks"
        );
        ensure!(
            !self.constraints_relaxed,
            "manifest records relaxed timing constraints"
        );
        for (name, slack) in [
            ("setup", self.minimum_setup_ns),
            ("hold", self.minimum_hold_ns),
            ("recovery", self.minimum_recovery_ns),
            ("removal", self.minimum_removal_ns),
            ("pulse width", self.minimum_pulse_width_ns),
        ] {
            ensure!(
                slack.is_finite() && slack >= 0.0,
                "manifest minimum {name} slack is not timing-clean"
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Prepared {
    cli: PathBuf,
    cli_sha256: String,
    libblade_rf: PathBuf,
    libblade_rf_sha256: String,
    custom_image: PathBuf,
    recovery_image: PathBuf,
    manifest_path: PathBuf,
    manifest_sha256: String,
    custom_sha256: String,
    recovery_sha256: String,
    workspace_source_commit: String,
    device_selector: String,
    center_hz: u64,
    gain_db: i32,
    antenna: u8,
    records: u64,
    transport_slots: u64,
    raw_bytes: u64,
    duration_seconds: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Capture,
    Restore,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Capture => "custom capture",
            Self::Restore => "stock restoration",
        }
    }
}

#[derive(Clone, Debug)]
struct Invocation {
    stage: Stage,
    program: PathBuf,
    library_directory: PathBuf,
    arguments: Vec<OsString>,
    current_dir: PathBuf,
    log: PathBuf,
    timeout: Duration,
}

trait Runner {
    fn run(&mut self, invocation: &Invocation, stop: &AtomicBool) -> Result<()>;
}

struct ProcessRunner;

impl Runner for ProcessRunner {
    fn run(&mut self, invocation: &Invocation, stop: &AtomicBool) -> Result<()> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&invocation.log);
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.arguments)
            .current_dir(&invocation.current_dir)
            .stdin(Stdio::null())
            .env_clear()
            .env("LD_LIBRARY_PATH", &invocation.library_directory);
        let logging_failure = match log {
            Ok(log) => match log.try_clone() {
                Ok(stdout) => {
                    command.stdout(stdout).stderr(log);
                    None
                }
                Err(error) if invocation.stage == Stage::Restore => {
                    command.stdout(Stdio::null()).stderr(Stdio::null());
                    Some(anyhow::Error::new(error).context("clone stock restoration log"))
                }
                Err(error) => return Err(error).context("clone custom capture log"),
            },
            Err(error) if invocation.stage == Stage::Restore => {
                command.stdout(Stdio::null()).stderr(Stdio::null());
                Some(anyhow::Error::new(error).context("open stock restoration log"))
            }
            Err(error) => return Err(error).context("open custom capture log"),
        };
        let mut child = crate::AcquisitionChild {
            child: command
                .spawn()
                .with_context(|| format!("start {} CLI", invocation.stage.label()))?,
            reaped: false,
        };
        let started = Instant::now();
        loop {
            if let Some(status) = child.child.try_wait()? {
                child.reaped = true;
                ensure!(
                    status.success(),
                    "{} CLI exited with {status}",
                    invocation.stage.label()
                );
                if let Some(error) = logging_failure {
                    return Err(error).context(
                        "stock restoration ran without a verifiable native log; board state is uncertain",
                    );
                }
                return Ok(());
            }
            if stop.load(Ordering::Relaxed) || started.elapsed() > invocation.timeout {
                let forced = child.stop()?;
                bail!(
                    "{} interrupted or timed out (forced kill: {forced})",
                    invocation.stage.label()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[derive(Debug, Serialize)]
struct CaptureReadbacks {
    receiver: String,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    bandwidth_hz: u32,
    active_gain_readback_db: i32,
    firmware_version: String,
    usb_speed: String,
    bias_tees_off: bool,
    native_state: &'static str,
    native_last_error: &'static str,
}

fn checked_file(path: &Path, executable: bool) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file(),
        "expected a regular file: {}",
        path.display()
    );
    if executable {
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "expected an executable file: {}",
            path.display()
        );
    }
    Ok(path)
}

fn workspace_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= maximum,
        "{} must be a regular file no larger than {maximum} bytes",
        path.display()
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    file.take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= maximum,
        "{} grew while reading",
        path.display()
    );
    Ok(bytes)
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let before = file.metadata()?;
    ensure!(
        before.is_file(),
        "expected a regular file: {}",
        path.display()
    );
    let mut input = BufReader::new(file);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .context("file byte count overflow")?;
    }
    ensure!(
        bytes == before.len() && input.get_ref().metadata()?.len() == before.len(),
        "{} changed while hashing",
        path.display()
    );
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{label} must be a 64-digit SHA-256"
    );
    Ok(())
}

fn validate_git_commit(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{label} must be a complete 40-digit hexadecimal commit"
    );
    Ok(())
}

fn catalog_stock_hash(root: &Path) -> Result<String> {
    let catalog = read_bounded(&root.join("config/images.sha256"), 64 * 1024)?;
    let text = std::str::from_utf8(&catalog).context("image checksum catalog is not UTF-8")?;
    let mut found = None;
    for line in text.lines() {
        let Some((hash, path)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if path.trim() == STOCK_CATALOG_PATH {
            ensure!(found.is_none(), "duplicate stock image catalog entry");
            validate_sha256(hash, "stock catalog hash")?;
            found = Some(hash.to_ascii_lowercase());
        }
    }
    found.context("stock xA9 image is absent from config/images.sha256")
}

fn verify_cli_version(
    cli: &Path,
    library_directory: Option<&Path>,
    option: &str,
    expected: &str,
) -> Result<()> {
    let mut command = Command::new(cli);
    command
        .arg(option)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    if let Some(directory) = library_directory {
        command.env("LD_LIBRARY_PATH", directory);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("start bladeRF-cli {option} query"))?;
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() > CLI_VERSION_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("bladeRF-cli {option} query timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("collect bladeRF-cli {option} query"))?;
    ensure!(
        output.status.success(),
        "bladeRF-cli {option} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let actual = std::str::from_utf8(&output.stdout)
        .context("bladeRF-cli version output is not UTF-8")?
        .trim();
    ensure!(
        actual == expected,
        "bladeRF-cli {option} returned {actual:?}; expected {expected:?}"
    );
    Ok(())
}

fn verify_manifest_qualification(manifest: &ImageManifest) -> Result<()> {
    ensure!(
        manifest.quartus == QUALIFIED_QUARTUS_VERSION,
        "manifest uses an unqualified Quartus version"
    );
    ensure!(
        manifest.nios_configuration == QUALIFIED_NIOS_CONFIGURATION,
        "manifest uses an unqualified Nios configuration"
    );
    ensure!(
        manifest.fitter_seed == QUALIFIED_FITTER_SEED,
        "manifest uses an unqualified fitter seed"
    );
    manifest.timing.validate()
}

fn verify_maintained_sources(root: &Path, manifest: &ImageManifest) -> Result<()> {
    let (patch_hash, _) =
        sha256_file(&root.join("patches/bladerf-rf-observatory-channel-bank.patch"))?;
    ensure!(
        patch_hash == manifest.integration_patch_sha256.to_ascii_lowercase(),
        "integration patch differs from the image manifest"
    );
    for name in [
        "rf_observatory_p.vhd",
        "rf_channel_bank.vhd",
        "rf_channel_framer.vhd",
    ] {
        let expected = manifest
            .hdl_source_sha256
            .get(name)
            .with_context(|| format!("image manifest lacks {name} hash"))?;
        validate_sha256(expected, &format!("{name} hash"))?;
        let (actual, _) = sha256_file(&root.join("projects/rf-observatory/hdl").join(name))?;
        ensure!(
            actual == expected.to_ascii_lowercase(),
            "{name} differs from image manifest"
        );
    }
    Ok(())
}

pub(crate) fn check_usbfs(path: &Path) -> Result<u64> {
    let value = fs::read_to_string(path)
        .with_context(|| format!("read usbfs memory limit from {}", path.display()))?;
    let memory_mb: u64 = value.trim().parse().context("parse usbfs memory limit")?;
    ensure!(
        memory_mb >= MIN_USBFS_MEMORY_MB,
        "usbfs_memory_mb is {memory_mb}; set it to at least {MIN_USBFS_MEMORY_MB} for this boot before loading the FPGA"
    );
    Ok(memory_mb)
}

fn device_selector(serial: &str) -> Result<String> {
    ensure!(
        serial.len() == 32 && serial.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "serial must contain exactly 32 hexadecimal digits"
    );
    Ok(format!("*:serial={}", serial.to_ascii_lowercase()))
}

fn validate_loader_directory(path: &Path) -> Result<()> {
    ensure!(
        !path
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b':' | b';' | b'$')),
        "session path may not contain ':', ';', or '$' because it is used as a dynamic-library search directory"
    );
    Ok(())
}

fn prepare(root: &Path, args: &CaptureArgs) -> Result<Prepared> {
    ensure!(
        cfg!(target_endian = "little"),
        "custom transport capture requires a little-endian host"
    );
    let device_selector = device_selector(&args.serial)?;
    ensure!(
        (100_000..=10_000_000).contains(&args.records),
        "record count must be from 100000 through 10000000"
    );
    ensure!(
        (-16..=60).contains(&args.gain),
        "gain must be from -16 through 60 dB"
    );
    ensure!((1..=2).contains(&args.antenna), "antenna must be 1 or 2");
    let center_hz = crate::center_hz(args.center_mhz)?;
    let records = args.records;
    let transport_slots = records
        .checked_mul(TRANSPORT_SLOTS_PER_RECORD)
        .context("transport slot count overflow")?;
    let raw_bytes = records
        .checked_mul(BYTES_PER_RECORD)
        .context("capture byte count overflow")?;

    let cli = checked_file(&workspace_path(root, &args.blade_rf_cli), true)?;
    let (cli_sha256, _) = sha256_file(&cli)?;
    let libblade_rf = checked_file(
        &cli.parent()
            .context("bladeRF-cli has no parent directory")?
            .join(STAGED_LIB_FILE),
        false,
    )?;
    let (libblade_rf_sha256, _) = sha256_file(&libblade_rf)?;
    verify_cli_version(&cli, None, "--version", PINNED_CLI_VERSION)?;
    verify_cli_version(&cli, None, "--lib-version", PINNED_LIB_VERSION)?;

    let manifest_path = checked_file(&workspace_path(root, &args.manifest), false)?;
    let manifest_bytes = read_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: ImageManifest =
        serde_json::from_slice(&manifest_bytes).context("parse profile-1 image manifest")?;
    ensure!(
        manifest.schema_version == 1,
        "unsupported image manifest schema"
    );
    ensure!(
        manifest.profile_id == PROFILE_ID,
        "manifest is not profile 1"
    );
    ensure!(
        manifest.device == "5CEBA9F23C8",
        "manifest targets the wrong FPGA device"
    );
    ensure!(
        manifest.nuand_source_commit == "fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47",
        "manifest uses an unqualified Nuand source revision"
    );
    validate_git_commit(&manifest.workspace_source_commit, "workspace source commit")?;
    validate_git_commit(&manifest.nuand_source_commit, "Nuand source commit")?;
    verify_manifest_qualification(&manifest)?;
    validate_sha256(&manifest.image_sha256, "custom image hash")?;
    validate_sha256(&manifest.integration_patch_sha256, "integration patch hash")?;
    let image_name = Path::new(&manifest.image_file);
    ensure!(
        image_name.components().count() == 1 && image_name.file_name().is_some(),
        "manifest image_file must be one file name"
    );
    let custom_image = checked_file(
        &manifest_path
            .parent()
            .context("manifest has no parent directory")?
            .join(image_name),
        false,
    )?;
    let (custom_sha256, custom_bytes) = sha256_file(&custom_image)?;
    ensure!(
        custom_sha256 == manifest.image_sha256.to_ascii_lowercase()
            && custom_bytes == manifest.image_bytes,
        "custom image size or SHA-256 differs from its manifest"
    );
    verify_maintained_sources(root, &manifest)?;

    let recovery_image = checked_file(&workspace_path(root, &args.recovery_image), false)?;
    let expected_recovery_hash = catalog_stock_hash(root)?;
    let (recovery_sha256, _) = sha256_file(&recovery_image)?;
    ensure!(
        recovery_sha256 == expected_recovery_hash,
        "recovery image SHA-256 differs from the stock image catalog"
    );
    ensure!(
        recovery_sha256 != custom_sha256,
        "custom and recovery images must be different"
    );
    let manifest_sha256 = format!("{:x}", Sha256::digest(&manifest_bytes));
    check_usbfs(Path::new("/sys/module/usbcore/parameters/usbfs_memory_mb"))?;

    Ok(Prepared {
        cli,
        cli_sha256,
        libblade_rf,
        libblade_rf_sha256,
        custom_image,
        recovery_image,
        manifest_path,
        manifest_sha256,
        custom_sha256,
        recovery_sha256,
        workspace_source_commit: manifest.workspace_source_commit,
        device_selector,
        center_hz,
        gain_db: args.gain,
        antenna: args.antenna,
        records,
        transport_slots,
        raw_bytes,
        duration_seconds: records as f64 / PROFILE_RECORD_RATE as f64,
    })
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = crate::new_file(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn publish_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let file_name = path
        .file_name()
        .context("JSON publication path has no file name")?
        .to_string_lossy();
    let stage = path.with_file_name(format!("{file_name}.partial"));
    let mut file = crate::new_file(&stage)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::hard_link(&stage, path)
        .with_context(|| format!("publish complete JSON report {}", path.display()))?;
    let _ = fs::remove_file(stage);
    Ok(())
}

fn stage_file(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    mode: u32,
) -> Result<PathBuf> {
    let mut input = File::open(source).with_context(|| format!("open {}", source.display()))?;
    let mut output = crate::new_file(destination)?;
    std::io::copy(&mut input, &mut output)
        .with_context(|| format!("stage {}", destination.display()))?;
    output.set_permissions(fs::Permissions::from_mode(mode))?;
    output.sync_all()?;
    drop(output);
    let (actual, _) = sha256_file(destination)?;
    ensure!(
        actual == expected_sha256,
        "staged file {} differs from its verified source",
        destination.display()
    );
    destination
        .canonicalize()
        .with_context(|| format!("resolve staged image {}", destination.display()))
}

fn capture_script(prepared: &Prepared) -> String {
    let rx = format!("rx{}", prepared.antenna);
    let wait_ms = (prepared.duration_seconds * 1000.0).ceil() as u64 + 20_000;
    format!(
        "set bitmode 16\n\
set rx_mux BASEBAND\n\
set loopback none\n\
set biastee rx1 off\n\
set biastee rx2 off\n\
set biastee tx1 off\n\
set biastee tx2 off\n\
trigger J51-1 rx off\n\
set samplerate {rx} 20000000\n\
set bandwidth {rx} 20000000\n\
set frequency {rx} {}\n\
set agc {rx} off\n\
set gain {rx} {}\n\
rx config channel={} format=bin file={FRAME_FILE} n={} samples=16384 buffers=128 xfers=32 timeout=5s\n\
rx start\n\
rx wait 10ms\n\
echo {ACTIVE_GAIN_BEGIN}\n\
print gain {rx}\n\
echo {ACTIVE_GAIN_END}\n\
rx wait {wait_ms}ms\n\
rx\n\
print samplerate {rx}\n\
print bandwidth {rx}\n\
print frequency {rx}\n\
print gain {rx}\n\
print biastee\n\
version\n\
info\n",
        prepared.center_hz, prepared.gain_db, prepared.antenna, prepared.transport_slots,
    )
}

fn restore_script() -> &'static str {
    "set loopback none\n\
set biastee rx1 off\n\
set biastee rx2 off\n\
set biastee tx1 off\n\
set biastee tx2 off\n\
print biastee\n\
version\n\
info\n"
}

fn invocation(
    prepared: &Prepared,
    output: &Path,
    program: &Path,
    library_directory: &Path,
    stage: Stage,
    image: &Path,
    timeout: Duration,
) -> Invocation {
    let (script, log) = match stage {
        Stage::Capture => (CAPTURE_SCRIPT, "capture.log"),
        Stage::Restore => (RESTORE_SCRIPT, "restore.log"),
    };
    Invocation {
        stage,
        program: program.to_path_buf(),
        library_directory: library_directory.to_path_buf(),
        arguments: vec![
            OsString::from("--device"),
            OsString::from(&prepared.device_selector),
            OsString::from("--load-fpga"),
            image.as_os_str().to_owned(),
            OsString::from("--script"),
            OsString::from(script),
        ],
        current_dir: output.to_path_buf(),
        log: output.join(log),
        timeout,
    }
}

fn normalized_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

fn require_line(lines: &[String], expected: &str) -> Result<()> {
    ensure!(
        lines.iter().any(|line| line.contains(expected)),
        "native CLI log lacks {expected:?}"
    );
    Ok(())
}

fn reject_cli_errors(lines: &[String]) -> Result<()> {
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if (lower.starts_with("error") || lower.contains("[error"))
            && !lower.contains("last error: none")
        {
            bail!("native CLI reported an error: {line}");
        }
    }
    Ok(())
}

fn active_gain_readback(lines: &[String], prefix: &str) -> Result<i32> {
    let begin = lines
        .iter()
        .position(|line| line == ACTIVE_GAIN_BEGIN)
        .context("native CLI log lacks active-gain begin marker")?;
    let end = lines
        .iter()
        .skip(begin + 1)
        .position(|line| line == ACTIVE_GAIN_END)
        .map(|offset| begin + 1 + offset)
        .context("native CLI log lacks active-gain end marker")?;
    ensure!(
        lines
            .iter()
            .filter(|line| *line == ACTIVE_GAIN_BEGIN)
            .count()
            == 1
            && lines.iter().filter(|line| *line == ACTIVE_GAIN_END).count() == 1,
        "native CLI log contains duplicate active-gain markers"
    );
    lines[begin + 1..end]
        .iter()
        .filter_map(|line| line.strip_prefix(prefix))
        .find_map(|suffix| suffix.split_whitespace().next()?.parse().ok())
        .with_context(|| format!("native CLI log lacks active numeric {prefix:?} readback"))
}

fn last_u64_after(lines: &[String], prefix: &str) -> Result<u64> {
    lines
        .iter()
        .rev()
        .filter_map(|line| line.strip_prefix(prefix))
        .find_map(|suffix| suffix.split_whitespace().next()?.parse().ok())
        .with_context(|| format!("native CLI log lacks numeric {prefix:?} readback"))
}

fn last_text_after(lines: &[String], prefix: &str) -> Result<String> {
    lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("native CLI log lacks nonempty {prefix:?} readback"))
}

fn parse_capture_log(text: &str, prepared: &Prepared) -> Result<CaptureReadbacks> {
    let lines = normalized_lines(text);
    reject_cli_errors(&lines)?;
    let rx = format!("RX{}", prepared.antenna);
    for expected in [
        "Successfully loaded FPGA bitstream!".to_owned(),
        "Bit Mode: 16 bit samples".to_owned(),
        "RX mux: BASEBAND - Baseband samples".to_owned(),
        "Loopback mode: none".to_owned(),
        "J51-1 RX trigger disabled".to_owned(),
        format!("{rx} AGC: Disabled"),
        "State: Idle".to_owned(),
        format!("Channels: {rx}"),
        "Last error: None".to_owned(),
        format!("File: {FRAME_FILE}"),
        "File format: SC16 Q11, Binary".to_owned(),
        format!("# Samples: {}", prepared.transport_slots),
        "Bias Tee (RX1): off".to_owned(),
        "Bias Tee (RX2): off".to_owned(),
        "Bias Tee (TX1): off".to_owned(),
        "Bias Tee (TX2): off".to_owned(),
        format!(
            "Serial #: {}",
            prepared.device_selector.trim_start_matches("*:serial=")
        ),
    ] {
        require_line(&lines, &expected)?;
    }
    let sample_rate = last_u64_after(&lines, &format!("{rx} sample rate:"))?;
    let bandwidth = last_u64_after(&lines, &format!("{rx} Bandwidth:"))?;
    let center_frequency = last_u64_after(&lines, &format!("{rx} Frequency:"))?;
    let active_gain = active_gain_readback(&lines, &format!("Gain {rx} overall:"))?;
    let firmware_version = last_text_after(&lines, "Firmware version:")?;
    let usb_speed = last_text_after(&lines, "USB speed:")?;
    ensure!(
        sample_rate == u64::from(crate::SAMPLE_RATE),
        "native sample-rate readback differs from profile 1"
    );
    ensure!(
        bandwidth == u64::from(crate::SAMPLE_RATE),
        "native bandwidth readback differs from profile 1"
    );
    ensure!(
        center_frequency.abs_diff(prepared.center_hz) <= 10,
        "native center-frequency readback differs from the request by more than 10 Hz"
    );
    ensure!(
        active_gain == prepared.gain_db,
        "active gain readback differs from the request"
    );
    ensure!(
        usb_speed == "SuperSpeed",
        "capture device is not using SuperSpeed USB"
    );
    Ok(CaptureReadbacks {
        receiver: rx,
        center_frequency_hz: center_frequency,
        sample_rate_hz: crate::SAMPLE_RATE,
        bandwidth_hz: crate::SAMPLE_RATE,
        active_gain_readback_db: active_gain,
        firmware_version,
        usb_speed,
        bias_tees_off: true,
        native_state: "Idle",
        native_last_error: "None",
    })
}

fn parse_restore_log(text: &str, prepared: &Prepared) -> Result<()> {
    let lines = normalized_lines(text);
    reject_cli_errors(&lines)?;
    for expected in [
        "Successfully loaded FPGA bitstream!",
        "FPGA version: 0.16.0",
        "Bias Tee (RX1): off",
        "Bias Tee (RX2): off",
        "Bias Tee (TX1): off",
        "Bias Tee (TX2): off",
    ] {
        require_line(&lines, expected)?;
    }
    require_line(
        &lines,
        &format!(
            "Serial #: {}",
            prepared.device_selector.trim_start_matches("*:serial=")
        ),
    )?;
    last_text_after(&lines, "Firmware version:")?;
    ensure!(
        last_text_after(&lines, "USB speed:")? == "SuperSpeed",
        "restored device is not using SuperSpeed USB"
    );
    Ok(())
}

fn read_log(path: &Path) -> Result<String> {
    let bytes = read_bounded(path, MAX_LOG_BYTES)?;
    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

fn validate_recording(
    output: &Path,
    prepared: &Prepared,
    stop: &AtomicBool,
) -> Result<(CaptureReadbacks, transport::Summary)> {
    let readbacks = parse_capture_log(&read_log(&output.join("capture.log"))?, prepared)?;
    let frame_path = output.join(FRAME_FILE);
    let frame = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&frame_path)
        .context("open completed framed capture")?;
    let metadata = frame.metadata().context("inspect framed capture")?;
    ensure!(
        metadata.is_file() && metadata.len() == prepared.raw_bytes,
        "framed capture has {} bytes; expected {}",
        metadata.len(),
        prepared.raw_bytes
    );
    fs::set_permissions(&frame_path, fs::Permissions::from_mode(0o600))?;
    frame.sync_all().context("sync completed framed capture")?;
    drop(frame);
    let summary = transport::validate(&frame_path, stop)?;
    ensure!(
        summary.records == prepared.records,
        "validated record count differs from request"
    );
    ensure!(
        summary.receiver() == Some(readbacks.receiver.as_str()),
        "framed source does not match the selected receive connector"
    );
    let expected_last = prepared
        .records
        .checked_mul(20)
        .and_then(|samples| samples.checked_sub(1))
        .context("expected source index overflow")?;
    ensure!(
        summary.first_source_index() == Some(19)
            && summary.last_source_index() == Some(expected_last),
        "framed capture does not span the complete requested source epoch"
    );
    Ok((readbacks, summary))
}

fn reconcile<T>(primary: Result<T>, restore: Result<()>) -> Result<T> {
    match (primary, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(restore)) => Err(restore.context("stock restoration failed after capture")),
        (Err(primary), Err(restore)) => {
            Err(primary.context(format!("stock restoration also failed: {restore:#}")))
        }
    }
}

struct Execution<T> {
    primary: Result<T>,
    restore: Result<()>,
}

impl<T> Execution<T> {
    fn restore_verified(&self) -> bool {
        self.restore.is_ok()
    }

    fn finish(self) -> Result<T> {
        reconcile(self.primary, self.restore)
    }
}

fn execute_with_runner<R: Runner>(
    runner: &mut R,
    capture: &Invocation,
    restore: &Invocation,
    output: &Path,
    prepared: &Prepared,
    stop: &AtomicBool,
) -> Execution<(CaptureReadbacks, transport::Summary)> {
    let capture_result = runner.run(capture, stop);
    let never_cancel = AtomicBool::new(false);
    let restore_result = runner
        .run(restore, &never_cancel)
        .and_then(|()| parse_restore_log(&read_log(&restore.log)?, prepared));
    let primary = capture_result.and_then(|()| validate_recording(output, prepared, stop));
    Execution {
        primary,
        restore: restore_result,
    }
}

/// Panic-unwind fallback. Normal errors use the explicit restoration path above,
/// which reports failure. This best-effort fallback may block in native teardown
/// and cannot protect against process kill, host loss, power loss, or device loss.
struct RestoreFallback {
    invocation: Invocation,
    armed: bool,
}

impl RestoreFallback {
    fn new(invocation: Invocation) -> Self {
        Self {
            invocation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RestoreFallback {
    fn drop(&mut self) {
        if self.armed {
            let mut runner = ProcessRunner;
            let _ = runner.run(&self.invocation, &AtomicBool::new(false));
        }
    }
}

fn write_failure_report(
    output: &Path,
    started: &str,
    error: &anyhow::Error,
    restore_verified: bool,
    stream_report_published: bool,
    success_report_published: bool,
) -> Result<()> {
    publish_private_json(
        &output.join("session-failure.json"),
        &serde_json::json!({
            "schema_version": 1,
            "started_utc": started,
            "finished_utc": Utc::now().to_rfc3339(),
            "error": format!("{error:#}"),
            "stock_restore_attempted": true,
            "stock_restore_verified": restore_verified,
            "board_state_known_stock": restore_verified,
            "stream_report_published": stream_report_published,
            "success_report_published": success_report_published,
            "evidence_directory": output,
        }),
    )
}

fn publication_failure(
    output: &Path,
    started: &str,
    error: anyhow::Error,
    stream_report_published: bool,
) -> Result<()> {
    if let Err(report_error) = write_failure_report(
        output,
        started,
        &error,
        true,
        stream_report_published,
        false,
    ) {
        return Err(error.context(format!(
            "terminal failure report also failed: {report_error:#}"
        )));
    }
    Err(error)
}

fn publish_terminal(
    output: &Path,
    started: &str,
    prepared: &Prepared,
    readbacks: &CaptureReadbacks,
    summary: &transport::Summary,
) -> Result<()> {
    if let Err(error) = publish_private_json(&output.join("stream.json"), summary) {
        return publication_failure(output, started, error, false);
    }
    let session = serde_json::json!({
        "schema_version": 1,
        "started_utc": started,
        "finished_utc": Utc::now().to_rfc3339(),
        "profile_id": PROFILE_ID,
        "manifest_sha256": prepared.manifest_sha256,
        "blade_rf_cli_sha256": prepared.cli_sha256,
        "libblade_rf_sha256": prepared.libblade_rf_sha256,
        "custom_image_sha256": prepared.custom_sha256,
        "recovery_image_sha256": prepared.recovery_sha256,
        "workspace_source_commit": prepared.workspace_source_commit,
        "readbacks": readbacks,
        "records": prepared.records,
        "transport_slots": prepared.transport_slots,
        "raw_bytes": prepared.raw_bytes,
        "stream": summary,
        "stock_restore_completed": true,
        "flash_written": false,
        "tx_stream_enabled": false,
        "limitations": "Exact framed transport and settings readback do not establish calibrated RF power, absence of analog clipping, or independent RF continuity. Restoration cannot be guaranteed after SIGKILL, host/power loss, or device removal."
    });
    publish_private_json(&output.join("session.json"), &session)
        .or_else(|error| publication_failure(output, started, error, true))
}

pub(crate) fn capture(root: &Path, args: CaptureArgs, stop: &AtomicBool) -> Result<()> {
    let prepared = prepare(root, &args)?;
    let output = crate::directory(
        root,
        args.output.as_deref(),
        "captures/rf-observatory/fpga-profile1",
    )?;
    validate_loader_directory(&output)?;
    write_private(&output.join(CAPTURE_SCRIPT), &capture_script(&prepared))?;
    write_private(&output.join(RESTORE_SCRIPT), restore_script())?;
    write_private(&output.join("capture.log"), "")?;
    write_private(&output.join("restore.log"), "")?;
    let staged_cli = stage_file(
        &prepared.cli,
        &output.join(STAGED_CLI_FILE),
        &prepared.cli_sha256,
        0o700,
    )?;
    let _staged_lib = stage_file(
        &prepared.libblade_rf,
        &output.join(STAGED_LIB_FILE),
        &prepared.libblade_rf_sha256,
        0o600,
    )?;
    verify_cli_version(&staged_cli, Some(&output), "--version", PINNED_CLI_VERSION)?;
    verify_cli_version(
        &staged_cli,
        Some(&output),
        "--lib-version",
        PINNED_LIB_VERSION,
    )?;
    let custom_image = stage_file(
        &prepared.custom_image,
        &output.join(CUSTOM_IMAGE_FILE),
        &prepared.custom_sha256,
        0o600,
    )?;
    let recovery_image = stage_file(
        &prepared.recovery_image,
        &output.join(RECOVERY_IMAGE_FILE),
        &prepared.recovery_sha256,
        0o600,
    )?;
    crate::json_file(
        &output.join("request.json"),
        &serde_json::json!({
            "schema_version": 1,
            "mode": "profile1_custom_fpga_ram_capture_with_stock_restore",
            "manifest": prepared.manifest_path,
            "manifest_sha256": prepared.manifest_sha256,
            "blade_rf_cli_sha256": prepared.cli_sha256,
            "libblade_rf_sha256": prepared.libblade_rf_sha256,
            "blade_rf_cli_version": PINNED_CLI_VERSION,
            "libblade_rf_version": PINNED_LIB_VERSION,
            "custom_image_sha256": prepared.custom_sha256,
            "recovery_image_sha256": prepared.recovery_sha256,
            "workspace_source_commit": prepared.workspace_source_commit,
            "qualified_quartus": QUALIFIED_QUARTUS_VERSION,
            "qualified_nios_configuration": QUALIFIED_NIOS_CONFIGURATION,
            "qualified_fitter_seed": QUALIFIED_FITTER_SEED,
            "qualified_timing_checks": QUALIFIED_TIMING_CHECKS,
            "profile_id": PROFILE_ID,
            "center_frequency_hz": prepared.center_hz,
            "sample_rate_hz": crate::SAMPLE_RATE,
            "bandwidth_hz": crate::SAMPLE_RATE,
            "gain_db_requested": prepared.gain_db,
            "physical_receiver": format!("RX{}", prepared.antenna),
            "records_requested": prepared.records,
            "transport_slots_requested": prepared.transport_slots,
            "raw_bytes_expected": prepared.raw_bytes,
            "duration_seconds": prepared.duration_seconds,
            "device_selection": "explicit_hexadecimal_serial_not_repeated_in_shared_documentation",
            "flash_write": false,
            "tx_stream": false,
        }),
    )?;

    let capture_invocation = invocation(
        &prepared,
        &output,
        &staged_cli,
        &output,
        Stage::Capture,
        &custom_image,
        Duration::from_secs_f64(prepared.duration_seconds + 30.0),
    );
    let restore_invocation = invocation(
        &prepared,
        &output,
        &staged_cli,
        &output,
        Stage::Restore,
        &recovery_image,
        Duration::from_secs(30),
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "custom FPGA session interrupted before device access"
    );
    let mut fallback = RestoreFallback::new(restore_invocation.clone());
    let started = Utc::now().to_rfc3339();
    let mut runner = ProcessRunner;
    let execution = execute_with_runner(
        &mut runner,
        &capture_invocation,
        &restore_invocation,
        &output,
        &prepared,
        stop,
    );
    let restore_verified = execution.restore_verified();
    let result = execution.finish();
    if restore_verified {
        fallback.disarm();
    }
    let (readbacks, summary) = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = write_failure_report(&output, &started, &error, restore_verified, false, false);
            return Err(error).with_context(|| {
                format!(
                    "custom FPGA session failed; evidence retained in {}",
                    output.display()
                )
            });
        }
    };
    if let Err(error) = publish_terminal(&output, &started, &prepared, &readbacks, &summary) {
        return Err(error).with_context(|| {
            format!(
                "stock FPGA was restored, but success reporting failed; evidence retained in {}",
                output.display()
            )
        });
    }
    println!(
        "Captured and validated {} profile-1 records; stock FPGA restored. Output: {}",
        prepared.records,
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn prepared(temp: &Path) -> Prepared {
        Prepared {
            cli: temp.join("bladeRF-cli"),
            cli_sha256: "0".repeat(64),
            libblade_rf: temp.join(STAGED_LIB_FILE),
            libblade_rf_sha256: "f".repeat(64),
            custom_image: temp.join("custom.rbf"),
            recovery_image: temp.join("stock.rbf"),
            manifest_path: temp.join("manifest.json"),
            manifest_sha256: "1".repeat(64),
            custom_sha256: "2".repeat(64),
            recovery_sha256: "3".repeat(64),
            workspace_source_commit: "commit".into(),
            device_selector: "*:serial=0123456789abcdef0123456789abcdef".into(),
            center_hz: 98_100_000,
            gain_db: 40,
            antenna: 2,
            records: 2,
            transport_slots: 16,
            raw_bytes: 64,
            duration_seconds: 0.000002,
        }
    }

    fn qualified_manifest() -> ImageManifest {
        ImageManifest {
            schema_version: 1,
            image_file: "profile1.rbf".into(),
            image_sha256: "1".repeat(64),
            image_bytes: 1,
            device: "5CEBA9F23C8".into(),
            profile_id: PROFILE_ID,
            workspace_source_commit: "2".repeat(40),
            nuand_source_commit: "fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47".into(),
            integration_patch_sha256: "3".repeat(64),
            hdl_source_sha256: BTreeMap::new(),
            quartus: QUALIFIED_QUARTUS_VERSION.into(),
            nios_configuration: QUALIFIED_NIOS_CONFIGURATION.into(),
            fitter_seed: QUALIFIED_FITTER_SEED,
            timing: TimingQualification {
                checks: QUALIFIED_TIMING_CHECKS,
                negative_checks: 0,
                minimum_setup_ns: 0.411,
                minimum_hold_ns: 0.011,
                minimum_recovery_ns: 3.625,
                minimum_removal_ns: 0.163,
                minimum_pulse_width_ns: 0.981,
                constraints_relaxed: false,
            },
        }
    }

    fn valid_frame(index: u64, rx2: bool) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..2].copy_from_slice(&0x5246u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&0x4f42u16.to_le_bytes());
        let flags = 0x1000u16 | if rx2 { 0x20 } else { 0 };
        bytes[4..6].copy_from_slice(&flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&PROFILE_ID.to_le_bytes());
        bytes[8..16].copy_from_slice(&index.to_le_bytes());
        bytes
    }

    fn capture_log(prepared: &Prepared) -> String {
        let rx = format!("RX{}", prepared.antenna);
        format!(
            "Successfully loaded FPGA bitstream!\n\
Bit Mode: 16 bit samples\n\
RX mux: BASEBAND - Baseband samples\n\
Loopback mode: none\n\
J51-1 RX trigger disabled\n\
{rx} AGC: Disabled\n\
State: Idle\n\
Channels: {rx}\n\
Last error: None\n\
File: {FRAME_FILE}\n\
File format: SC16 Q11, Binary\n\
# Samples: {}\n\
{rx} sample rate: 20000000 0/1\n\
{rx} Bandwidth: 20000000 Hz\n\
{rx} Frequency: {} Hz\n\
Gain {rx} overall: 60 dB\n\
{ACTIVE_GAIN_BEGIN}\n\
Gain {rx} overall: {} dB\n\
{ACTIVE_GAIN_END}\n\
Bias Tee (RX1): off\n\
Bias Tee (RX2): off\n\
Bias Tee (TX1): off\n\
Bias Tee (TX2): off\n\
Firmware version: 2.6.0-test\n\
USB speed: SuperSpeed\n\
Serial #: {}\n",
            prepared.transport_slots,
            prepared.center_hz,
            prepared.gain_db,
            prepared.device_selector.trim_start_matches("*:serial=")
        )
    }

    fn restore_log() -> &'static str {
        "Successfully loaded FPGA bitstream!\n\
FPGA version: 0.16.0 (configured by USB host)\n\
Bias Tee (RX1): off\n\
Bias Tee (RX2): off\n\
Bias Tee (TX1): off\n\
Bias Tee (TX2): off\n\
Firmware version: 2.6.0-test\n\
USB speed: SuperSpeed\n\
Serial #: 0123456789abcdef0123456789abcdef\n"
    }

    struct FakeRunner {
        stages: Rc<RefCell<Vec<(Stage, bool)>>>,
        capture_error: bool,
        capture_log_error: bool,
        truncate_frame: bool,
        restore_error: bool,
    }

    impl Runner for FakeRunner {
        fn run(&mut self, invocation: &Invocation, stop: &AtomicBool) -> Result<()> {
            let cancelled = stop.load(Ordering::Relaxed);
            self.stages.borrow_mut().push((invocation.stage, cancelled));
            if cancelled {
                bail!("synthetic cancellation");
            }
            match invocation.stage {
                Stage::Capture if self.capture_error => bail!("synthetic capture failure"),
                Stage::Restore if self.restore_error => bail!("synthetic restore failure"),
                Stage::Capture => {
                    let mut frames = [valid_frame(19, true), valid_frame(39, true)].concat();
                    if self.truncate_frame {
                        frames.pop();
                    }
                    fs::write(invocation.current_dir.join(FRAME_FILE), frames)?;
                    let mut log = capture_log(&prepared(&invocation.current_dir));
                    if self.capture_log_error {
                        log.push_str("Error: synthetic script failure\n");
                    }
                    fs::write(&invocation.log, log)?;
                    Ok(())
                }
                Stage::Restore => {
                    fs::write(&invocation.log, restore_log())?;
                    Ok(())
                }
            }
        }
    }

    fn test_invocations(temp: &Path, prepared: &Prepared) -> (Invocation, Invocation) {
        fs::write(temp.join(CAPTURE_SCRIPT), capture_script(prepared)).unwrap();
        fs::write(temp.join(RESTORE_SCRIPT), restore_script()).unwrap();
        (
            invocation(
                prepared,
                temp,
                &prepared.cli,
                temp,
                Stage::Capture,
                &prepared.custom_image,
                Duration::from_secs(1),
            ),
            invocation(
                prepared,
                temp,
                &prepared.cli,
                temp,
                Stage::Restore,
                &prepared.recovery_image,
                Duration::from_secs(1),
            ),
        )
    }

    #[test]
    fn generated_capture_is_single_rx_ram_only_and_receive_only() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let script = capture_script(&prepared);
        assert!(script.contains("rx config channel=2"));
        assert!(script.contains("set biastee tx1 off"));
        assert!(!script.contains("channel=1,2"));
        assert!(!script.contains("tx start"));
        for invocation in [&capture, &restore] {
            let arguments = invocation
                .arguments
                .iter()
                .map(|value| value.to_string_lossy())
                .collect::<Vec<_>>();
            assert!(arguments.iter().any(|value| value == "--load-fpga"));
            assert!(
                !arguments
                    .iter()
                    .any(|value| value == "--flash-fpga" || value == "-L")
            );
        }
    }

    #[test]
    fn staged_recovery_tools_are_hashed_private_and_executable() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source-cli");
        let destination = temp.path().join("staged-cli");
        fs::write(&source, b"synthetic executable").unwrap();
        let (expected, _) = sha256_file(&source).unwrap();

        let staged = stage_file(&source, &destination, &expected, 0o700).unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"synthetic executable");
        assert_eq!(
            fs::metadata(staged).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn restoration_still_runs_when_its_log_cannot_be_opened() {
        let temp = tempfile::tempdir().unwrap();
        let invocation = Invocation {
            stage: Stage::Restore,
            program: PathBuf::from("/bin/sh"),
            library_directory: temp.path().to_path_buf(),
            arguments: vec![
                OsString::from("-c"),
                OsString::from("printf ran > restore-ran"),
            ],
            current_dir: temp.path().to_path_buf(),
            log: temp.path().join("missing/restore.log"),
            timeout: Duration::from_secs(1),
        };

        let error = ProcessRunner
            .run(&invocation, &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(
            fs::read_to_string(temp.path().join("restore-ran")).unwrap(),
            "ran"
        );
        assert!(format!("{error:#}").contains("without a verifiable native log"));
    }

    #[test]
    fn manifest_requires_the_qualified_fit_and_clean_timing() {
        let manifest = qualified_manifest();
        verify_manifest_qualification(&manifest).unwrap();

        let mut wrong = manifest.clone();
        wrong.quartus = "different Quartus".into();
        assert!(verify_manifest_qualification(&wrong).is_err());
        let mut wrong = manifest.clone();
        wrong.nios_configuration = "fast".into();
        assert!(verify_manifest_qualification(&wrong).is_err());
        let mut wrong = manifest.clone();
        wrong.fitter_seed = 2;
        assert!(verify_manifest_qualification(&wrong).is_err());
        let mut wrong = manifest.clone();
        wrong.timing.checks -= 1;
        assert!(verify_manifest_qualification(&wrong).is_err());
        let mut wrong = manifest.clone();
        wrong.timing.negative_checks = 1;
        assert!(verify_manifest_qualification(&wrong).is_err());
        let mut wrong = manifest.clone();
        wrong.timing.constraints_relaxed = true;
        assert!(verify_manifest_qualification(&wrong).is_err());

        for invalid in [-0.001, f64::NAN, f64::INFINITY] {
            let mut wrong = manifest.clone();
            wrong.timing.minimum_setup_ns = invalid;
            assert!(verify_manifest_qualification(&wrong).is_err());
            let mut wrong = manifest.clone();
            wrong.timing.minimum_hold_ns = invalid;
            assert!(verify_manifest_qualification(&wrong).is_err());
            let mut wrong = manifest.clone();
            wrong.timing.minimum_recovery_ns = invalid;
            assert!(verify_manifest_qualification(&wrong).is_err());
            let mut wrong = manifest.clone();
            wrong.timing.minimum_removal_ns = invalid;
            assert!(verify_manifest_qualification(&wrong).is_err());
            let mut wrong = manifest.clone();
            wrong.timing.minimum_pulse_width_ns = invalid;
            assert!(verify_manifest_qualification(&wrong).is_err());
        }
    }

    #[test]
    fn restoration_runs_after_capture_failure_and_combines_failures() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: true,
            capture_log_error: false,
            truncate_frame: false,
            restore_error: true,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(false),
        );
        assert!(!execution.restore_verified());
        let error = execution.finish().unwrap_err();
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, false), (Stage::Restore, false)]
        );
        let text = format!("{error:#}");
        assert!(text.contains("synthetic capture failure"));
        assert!(text.contains("synthetic restore failure"));
    }

    #[test]
    fn successful_fake_session_validates_after_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: false,
            capture_log_error: false,
            truncate_frame: false,
            restore_error: false,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(false),
        );
        assert!(execution.restore_verified());
        let (readbacks, summary) = execution.finish().unwrap();
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, false), (Stage::Restore, false)]
        );
        assert_eq!(readbacks.receiver, "RX2");
        assert_eq!(summary.records, 2);
    }

    #[test]
    fn cli_errors_and_low_usbfs_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let bad_log = format!("{}Error: invalid command\n", capture_log(&prepared));
        assert!(parse_capture_log(&bad_log, &prepared).is_err());
        let quota = temp.path().join("usbfs_memory_mb");
        fs::write(&quota, "16\n").unwrap();
        assert!(check_usbfs(&quota).is_err());
        fs::write(&quota, "64\n").unwrap();
        assert_eq!(check_usbfs(&quota).unwrap(), 64);
        assert_eq!(
            device_selector("ABCDEF0123456789ABCDEF0123456789").unwrap(),
            "*:serial=abcdef0123456789abcdef0123456789"
        );
        validate_loader_directory(Path::new("/safe/session-path")).unwrap();
        for path in ["/bad:session", "/bad;session", "/bad/$ORIGIN"] {
            assert!(validate_loader_directory(Path::new(path)).is_err());
        }
    }

    #[test]
    fn misleading_zero_exit_log_failure_still_restores_stock() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: false,
            capture_log_error: true,
            truncate_frame: false,
            restore_error: false,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(false),
        );
        assert!(execution.restore_verified());
        let error = execution.finish().unwrap_err();
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, false), (Stage::Restore, false)]
        );
        assert!(format!("{error:#}").contains("synthetic script failure"));
    }

    #[test]
    fn cancellation_does_not_cancel_stock_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: false,
            capture_log_error: false,
            truncate_frame: false,
            restore_error: false,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(true),
        );
        assert!(execution.restore_verified());
        assert!(format!("{:#}", execution.finish().unwrap_err()).contains("cancellation"));
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, true), (Stage::Restore, false)]
        );
    }

    #[test]
    fn restoration_failure_after_capture_is_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: false,
            capture_log_error: false,
            truncate_frame: false,
            restore_error: true,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(false),
        );
        assert!(!execution.restore_verified());
        assert!(
            format!("{:#}", execution.finish().unwrap_err())
                .contains("stock restoration failed after capture")
        );
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, false), (Stage::Restore, false)]
        );
    }

    #[test]
    fn malformed_capture_is_rejected_after_stock_restoration() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let (capture, restore) = test_invocations(temp.path(), &prepared);
        let stages = Rc::new(RefCell::new(Vec::new()));
        let mut runner = FakeRunner {
            stages: Rc::clone(&stages),
            capture_error: false,
            capture_log_error: false,
            truncate_frame: true,
            restore_error: false,
        };
        let execution = execute_with_runner(
            &mut runner,
            &capture,
            &restore,
            temp.path(),
            &prepared,
            &AtomicBool::new(false),
        );
        assert!(execution.restore_verified());
        assert!(format!("{:#}", execution.finish().unwrap_err()).contains("expected 64"));
        assert_eq!(
            *stages.borrow(),
            [(Stage::Capture, false), (Stage::Restore, false)]
        );
    }

    #[test]
    fn capture_epoch_must_begin_with_source_index_nineteen() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        fs::write(
            temp.path().join(FRAME_FILE),
            [valid_frame(39, true), valid_frame(59, true)].concat(),
        )
        .unwrap();
        fs::write(temp.path().join("capture.log"), capture_log(&prepared)).unwrap();
        let error =
            validate_recording(temp.path(), &prepared, &AtomicBool::new(false)).unwrap_err();
        assert!(format!("{error:#}").contains("complete requested source epoch"));
    }

    #[test]
    fn publication_failure_writes_a_terminal_failure_report() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepared(temp.path());
        let frames = [valid_frame(19, true), valid_frame(39, true)].concat();
        fs::write(temp.path().join(FRAME_FILE), frames).unwrap();
        let summary =
            transport::validate(&temp.path().join(FRAME_FILE), &AtomicBool::new(false)).unwrap();
        let readbacks = parse_capture_log(&capture_log(&prepared), &prepared).unwrap();
        fs::write(temp.path().join("session.json"), "already exists").unwrap();

        let error = publish_terminal(
            temp.path(),
            "2026-01-01T00:00:00Z",
            &prepared,
            &readbacks,
            &summary,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("session.json"));
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("session-failure.json")).unwrap())
                .unwrap();
        assert_eq!(report["stock_restore_verified"], true);
        assert_eq!(report["stream_report_published"], true);
        assert_eq!(report["success_report_published"], false);
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(temp.path().join("stream.json")).unwrap(),
        )
        .unwrap();
    }
}
