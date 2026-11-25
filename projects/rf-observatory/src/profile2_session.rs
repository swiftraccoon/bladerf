//! Device-independent policy for one guarded profile-2 receive session.
//!
//! This module owns lifecycle ordering. Its narrow traits contain no image path,
//! generic FPGA-load, flash, bias-tee or transmit operation. Deterministic CPU
//! models exercise the policy, while the private process binding fixes sealed
//! custom/stock roles around the reviewed one-handle native worker and distinct
//! fresh recovery process. Its reviewed private composition fixes preflight,
//! publication, factory construction, one guarded run and one terminalization
//! attempt. The command-line boundary below accepts only an explicit bounded
//! receive plan and exact pinned inputs; it exposes no flash, transmit, bias-tee
//! or generic image-load operation.

// Keep the process implementation private behind the narrow launcher boundary.
mod process;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Error, Result, anyhow, bail, ensure};
use clap::{Args, ValueEnum};

use crate::profile2_control::{
    AppliedConfiguration, CaptureRequest, Controller, ReceiveReadback, Receiver, RegisterAddress,
    RegisterIo, validate_status_poll_bound,
};

const REFERENCE_CENTER_HZ: u64 = 98_100_000;
const REFERENCE_GAIN_DB: i32 = 40;
const REFERENCE_RECORDS: u64 = 100_000;
const MIN_CENTER_HZ: u64 = 70_000_000;
const MAX_CENTER_HZ: u64 = 6_000_000_000;
const MIN_GAIN_DB: i32 = -16;
const MAX_GAIN_DB: i32 = 60;
const MIN_RECORDS: u64 = 100_000;
const MAX_RECORDS: u64 = 10_000_000;
const REFERENCE_WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
const PROFILE2_ID: u32 = 0x5246_0201;
const PROFILE2_CAPABILITIES: u32 = 0x0000_1f04;
const SOURCE_SAMPLE_RATE_HZ: u32 = 20_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CaptureReceiver {
    Rx1,
    Rx2,
}

impl From<CaptureReceiver> for Receiver {
    fn from(receiver: CaptureReceiver) -> Self {
        match receiver {
            CaptureReceiver::Rx1 => Self::Rx1,
            CaptureReceiver::Rx2 => Self::Rx2,
        }
    }
}

/// RAM-load profile 2, capture bounded receive data, restore stock and publish evidence.
///
/// This command is receive-only and RAM-only. It requires an explicit RX1/RX2
/// connector because connector selection does not establish which input has the
/// antenna feed. It never flashes, transmits or enables a bias tee.
#[derive(Debug, Args)]
pub(crate) struct CaptureArgs {
    /// Physical receive connector to use; this does not identify the antenna feed.
    #[arg(long, value_enum)]
    receiver: CaptureReceiver,
    /// Integer RF center frequency in hertz, from 70 MHz through 6 GHz.
    #[arg(long, value_parser = clap::value_parser!(u64).range(MIN_CENTER_HZ..=MAX_CENTER_HZ))]
    center_hz: u64,
    /// Integer manual receive gain in dB, from -16 through 60.
    #[arg(long, allow_negative_numbers = true, value_parser = clap::value_parser!(i32).range(i64::from(MIN_GAIN_DB)..=i64::from(MAX_GAIN_DB)))]
    gain_db: i32,
    /// Profile-2 DATA records, 100,000 through 10,000,000.
    #[arg(long, default_value_t = REFERENCE_RECORDS, value_parser = clap::value_parser!(u64).range(MIN_RECORDS..=MAX_RECORDS))]
    records: u64,
    /// First exact unsigned 32-bit phase word, decimal or 0x hexadecimal.
    #[arg(long, value_parser = parse_phase_word)]
    phase_0: u32,
    /// Second exact unsigned 32-bit phase word, decimal or 0x hexadecimal.
    #[arg(long, value_parser = parse_phase_word)]
    phase_1: u32,
    /// Third exact unsigned 32-bit phase word, decimal or 0x hexadecimal.
    #[arg(long, value_parser = parse_phase_word)]
    phase_2: u32,
    /// Fourth exact unsigned 32-bit phase word, decimal or 0x hexadecimal.
    #[arg(long, value_parser = parse_phase_word)]
    phase_3: u32,
    /// Maximum status-register polls for the stopped atomic commit.
    #[arg(long, default_value_t = 4, value_parser = parse_status_polls)]
    max_status_polls: usize,
    /// Seconds allowed for each framed native transaction (positive, at most 60).
    #[arg(
        long,
        default_value_t = 60.0,
        value_parser = parse_transaction_timeout_seconds
    )]
    transaction_timeout_seconds: f64,
    /// Qualified profile-2 bundle manifest, relative to the workspace by default.
    #[arg(long)]
    manifest: PathBuf,
    /// Separately trusted lowercase manifest SHA-256.
    #[arg(long, value_parser = parse_sha256)]
    manifest_sha256: String,
    /// Exact native adapter executable; no PATH lookup is used.
    #[arg(long)]
    adapter: PathBuf,
    /// Separately trusted lowercase adapter SHA-256.
    #[arg(long, value_parser = parse_sha256)]
    adapter_sha256: String,
    /// Exact specialized libbladeRF file.
    #[arg(long)]
    library: PathBuf,
    /// Separately trusted lowercase library SHA-256.
    #[arg(long, value_parser = parse_sha256)]
    library_sha256: String,
    /// Frozen stock xA9 image for verified RAM restoration.
    #[arg(long)]
    stock_image: PathBuf,
    /// Complete 32-character lowercase hexadecimal bladeRF serial.
    #[arg(long, value_parser = parse_serial)]
    serial: String,
    /// Fresh session directory under an existing trusted canonical mode-0700 parent.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug)]
pub(crate) struct CaptureReceipt {
    output: PathBuf,
    receiver: Receiver,
}

impl CaptureReceipt {
    pub(crate) fn output(&self) -> &Path {
        &self.output
    }

    pub(crate) fn receiver(&self) -> Receiver {
        self.receiver
    }
}

struct LaunchRequest {
    workspace_root: PathBuf,
    manifest: PathBuf,
    manifest_sha256: String,
    adapter: PathBuf,
    adapter_sha256: String,
    library: PathBuf,
    library_sha256: String,
    stock_image: PathBuf,
    serial: String,
    transaction_timeout: Duration,
    stage_parent: PathBuf,
    output_basename: OsString,
    plan: SessionPlan,
}

fn exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn parse_sha256(value: &str) -> std::result::Result<String, String> {
    if exact_lower_hex(value, 64) {
        Ok(value.to_owned())
    } else {
        Err("SHA-256 must be exactly 64 lowercase hexadecimal characters".to_owned())
    }
}

fn parse_serial(value: &str) -> std::result::Result<String, String> {
    if exact_lower_hex(value, 32) {
        Ok(value.to_owned())
    } else {
        Err("bladeRF serial must be exactly 32 lowercase hexadecimal characters".to_owned())
    }
}

fn parse_phase_word(value: &str) -> std::result::Result<u32, String> {
    let (digits, radix) = value
        .strip_prefix("0x")
        .map_or((value, 10), |digits| (digits, 16));
    let valid = !digits.is_empty()
        && match radix {
            10 => digits.bytes().all(|byte| byte.is_ascii_digit()),
            16 => digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
            _ => unreachable!("phase-word parser uses only decimal and hexadecimal"),
        };
    if !valid {
        return Err("phase word must be a decimal u32 or 0x-prefixed hexadecimal u32".to_owned());
    }
    u32::from_str_radix(digits, radix)
        .map_err(|_| "phase word is outside the unsigned 32-bit range".to_owned())
}

fn transaction_timeout(seconds: f64) -> Result<Duration> {
    ensure!(
        seconds.is_finite()
            && seconds > 0.0
            && seconds <= bladerf_profile2_session_ipc::MAX_TRANSACTION_TIMEOUT.as_secs_f64(),
        "profile-2 transaction timeout must be finite, positive and no greater than {:?}",
        bladerf_profile2_session_ipc::MAX_TRANSACTION_TIMEOUT
    );
    let timeout = Duration::try_from_secs_f64(seconds)
        .map_err(|_| anyhow!("profile-2 transaction timeout cannot be represented"))?;
    ensure!(
        !timeout.is_zero(),
        "profile-2 transaction timeout is below the representable positive duration"
    );
    Ok(timeout)
}

fn parse_transaction_timeout_seconds(value: &str) -> std::result::Result<f64, String> {
    let seconds = value
        .parse::<f64>()
        .map_err(|_| "transaction timeout must be a number of seconds".to_owned())?;
    transaction_timeout(seconds)
        .map(|_| seconds)
        .map_err(|error| error.to_string())
}

fn parse_status_polls(value: &str) -> std::result::Result<usize, String> {
    let polls = value
        .parse::<usize>()
        .map_err(|_| "status poll bound must be an integer".to_owned())?;
    validate_status_poll_bound(polls)
        .map(|_| polls)
        .map_err(|error| error.to_string())
}

fn absolute_lexical_path(path: &Path, label: &str) -> Result<()> {
    ensure!(
        path.is_absolute() && !path.as_os_str().is_empty(),
        "{label} must be an absolute nonempty path"
    );
    let mut components = path.components();
    ensure!(
        matches!(components.next(), Some(Component::RootDir))
            && components.all(|component| matches!(component, Component::Normal(_)))
            && !has_dot_segment(path),
        "{label} must be lexically normalized without '.' or '..' components"
    );
    Ok(())
}

fn has_dot_segment(path: &Path) -> bool {
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .any(|segment| matches!(segment, b"." | b".."))
}

fn resolve_input(workspace_root: &Path, path: PathBuf, label: &str) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "{label} path must be nonempty"
    );
    let resolved = if path.is_absolute() {
        path
    } else {
        ensure!(
            path.components()
                .all(|component| matches!(component, Component::Normal(_))),
            "relative {label} path must not contain '.' or '..' components"
        );
        ensure!(
            !has_dot_segment(&path),
            "relative {label} path must not contain '.' or '..' components"
        );
        workspace_root.join(path)
    };
    absolute_lexical_path(&resolved, &format!("{label} path"))?;
    Ok(resolved)
}

fn split_output(workspace_root: &Path, output: PathBuf) -> Result<(PathBuf, OsString)> {
    let output = resolve_input(workspace_root, output, "output")?;
    let basename = output
        .file_name()
        .context("output must name one fresh session directory")?
        .to_os_string();
    ensure!(
        matches!(
            Path::new(&basename).components().next(),
            Some(Component::Normal(_))
        ) && Path::new(&basename).components().count() == 1,
        "output basename must be one normal path component"
    );
    let parent = output
        .parent()
        .context("output must have an existing private parent")?
        .to_path_buf();
    absolute_lexical_path(&parent, "output parent")?;
    Ok((parent, basename))
}

fn prepare_launch(workspace_root: &Path, args: CaptureArgs) -> Result<(LaunchRequest, Receiver)> {
    absolute_lexical_path(workspace_root, "workspace root")?;

    ensure!(
        (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&args.center_hz),
        "profile-2 center frequency must be {MIN_CENTER_HZ} through {MAX_CENTER_HZ} Hz"
    );
    ensure!(
        exact_lower_hex(&args.manifest_sha256, 64)
            && exact_lower_hex(&args.adapter_sha256, 64)
            && exact_lower_hex(&args.library_sha256, 64),
        "profile-2 input SHA-256 values must be exactly 64 lowercase hexadecimal characters"
    );
    ensure!(
        exact_lower_hex(&args.serial, 32),
        "bladeRF serial must be exactly 32 lowercase hexadecimal characters"
    );
    let transaction_timeout = transaction_timeout(args.transaction_timeout_seconds)?;
    let receiver = Receiver::from(args.receiver);
    let plan = SessionPlan::new(
        CaptureRequest::new(
            [args.phase_0, args.phase_1, args.phase_2, args.phase_3],
            receiver,
        ),
        args.center_hz,
        args.gain_db,
        args.records,
        args.max_status_polls,
    )?;

    let manifest = resolve_input(workspace_root, args.manifest, "manifest")?;
    let adapter = resolve_input(workspace_root, args.adapter, "adapter")?;
    let library = resolve_input(workspace_root, args.library, "library")?;
    let stock_image = resolve_input(workspace_root, args.stock_image, "stock image")?;
    let (stage_parent, output_basename) = split_output(workspace_root, args.output)?;
    Ok((
        LaunchRequest {
            workspace_root: workspace_root.to_path_buf(),
            manifest,
            manifest_sha256: args.manifest_sha256,
            adapter,
            adapter_sha256: args.adapter_sha256,
            library,
            library_sha256: args.library_sha256,
            stock_image,
            serial: args.serial,
            transaction_timeout,
            stage_parent,
            output_basename,
            plan,
        },
        receiver,
    ))
}

pub(crate) fn capture(
    workspace_root: &Path,
    args: CaptureArgs,
    stop: Arc<AtomicBool>,
) -> Result<CaptureReceipt> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 capture was interrupted before path access"
    );
    let (request, receiver) = prepare_launch(workspace_root, args)?;
    let output = process::run_launch(request, stop)?;
    Ok(CaptureReceipt { output, receiver })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SessionPlan {
    request: CaptureRequest,
    center_hz: u64,
    gain_db: i32,
    records: u64,
    max_status_polls: usize,
}

impl SessionPlan {
    fn new(
        request: CaptureRequest,
        center_hz: u64,
        gain_db: i32,
        records: u64,
        max_status_polls: usize,
    ) -> Result<Self> {
        ensure!(center_hz > 0, "profile-2 center frequency must be positive");
        ensure!(
            (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&gain_db),
            "profile-2 gain must be {MIN_GAIN_DB} through {MAX_GAIN_DB} dB"
        );
        ensure!(
            (MIN_RECORDS..=MAX_RECORDS).contains(&records),
            "profile-2 record count must be {MIN_RECORDS} through {MAX_RECORDS}"
        );
        validate_status_poll_bound(max_status_polls)?;
        Ok(Self {
            request,
            center_hz,
            gain_db,
            records,
            max_status_polls,
        })
    }

    fn reference(receiver: Receiver, words: [u32; 4]) -> Result<Self> {
        Self::new(
            CaptureRequest::new(words, receiver),
            REFERENCE_CENTER_HZ,
            REFERENCE_GAIN_DB,
            REFERENCE_RECORDS,
            4,
        )
    }
}

/// Fixed stock-image recovery operations owned by one worker and bladeRF handle.
///
/// `shutdown` is the orderly path, but an error does not prove that the worker
/// exited. `terminate` must therefore be idempotent process cleanup that performs
/// no device, FPGA or RF call and treats an already exited worker as success.
trait RecoveryDevice {
    /// Dispatch the one sealed stock-image role; no caller-selected path exists.
    fn restore_stock(&mut self) -> Result<()>;
    /// Establish the exact expected stock identity after a successful dispatch.
    fn verify_stock(&mut self) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}

/// Operations on the primary worker for one custom receive epoch.
///
/// The worker owns one handle and serializes all calls. Configuration must keep
/// RX disabled even when it returns an error; if that cannot be established,
/// `is_terminal` must return true. A start or capture error can leave RX state
/// unknown. The coordinator stops only a nonterminal worker and otherwise
/// terminates it before opening a fresh recovery worker.
trait SessionDevice: RecoveryDevice + RegisterIo {
    type Capture;

    /// Dispatch the one sealed profile-2 image role.
    ///
    /// Any return means configuration might have reached the FPGA. Until exact
    /// profile identity succeeds, the current handle is not trusted for recovery.
    fn load_profile2(&mut self) -> Result<()>;
    fn verify_profile2(&mut self) -> Result<()>;
    fn configure_receive_and_verify(&mut self, plan: SessionPlan) -> Result<ReceiveReadback>;
    fn start_receive(&mut self, plan: SessionPlan) -> Result<()>;
    fn capture(&mut self, plan: SessionPlan) -> Result<Self::Capture>;
    fn stop_receive(&mut self) -> Result<()>;
    /// Return local worker state without issuing an IPC, native or device call.
    fn is_terminal(&self) -> bool;
    /// Return whether process/group/worker teardown is confirmed, without I/O.
    fn is_cleanup_confirmed(&self) -> bool;
}

/// Creates independent primary and recovery workers and validates closed output.
///
/// `open_recovery` must create a fresh process, handle and independently staged
/// stock image; it cannot inherit trusted state from a failed primary worker.
trait SessionFactory {
    type Capture;
    type Primary: SessionDevice<Capture = Self::Capture>;
    type Recovery: RecoveryDevice;
    type Summary;

    fn open_primary(&mut self, plan: SessionPlan) -> Result<Self::Primary>;
    fn open_recovery(&mut self) -> Result<Self::Recovery>;
    fn validate_capture(
        &mut self,
        plan: SessionPlan,
        applied: AppliedConfiguration,
        readback: ReceiveReadback,
        capture: Self::Capture,
    ) -> Result<Self::Summary>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestorationOutcome {
    NotRequired,
    SameHandleVerified,
    FreshWorkerVerified,
    Unknown,
}

#[derive(Debug)]
struct SessionSuccess<S> {
    applied: AppliedConfiguration,
    readback: ReceiveReadback,
    summary: S,
    restoration: RestorationOutcome,
}

#[derive(Debug)]
struct SessionFailure {
    primary: Option<Error>,
    same_handle_restore: Option<Error>,
    fresh_recovery: Option<Error>,
    primary_cleanup: Option<Error>,
    recovery_cleanup: Option<Error>,
    restoration: RestorationOutcome,
}

impl SessionFailure {
    fn new() -> Self {
        Self {
            primary: None,
            same_handle_restore: None,
            fresh_recovery: None,
            primary_cleanup: None,
            recovery_cleanup: None,
            restoration: RestorationOutcome::NotRequired,
        }
    }

    fn has_error(&self) -> bool {
        self.primary.is_some()
            || self.same_handle_restore.is_some()
            || self.fresh_recovery.is_some()
            || self.primary_cleanup.is_some()
            || self.recovery_cleanup.is_some()
    }

    fn primary(&mut self, error: Error) {
        if let Some(previous) = self.primary.take() {
            self.primary = Some(previous.context(format!("additional primary failure: {error:#}")));
        } else {
            self.primary = Some(error);
        }
    }

    fn primary_cleanup(&mut self, error: Error) {
        if let Some(previous) = self.primary_cleanup.take() {
            self.primary_cleanup =
                Some(previous.context(format!("additional primary cleanup failure: {error:#}")));
        } else {
            self.primary_cleanup = Some(error);
        }
    }

    fn recovery_cleanup(&mut self, error: Error) {
        if let Some(previous) = self.recovery_cleanup.take() {
            self.recovery_cleanup =
                Some(previous.context(format!("additional recovery cleanup failure: {error:#}")));
        } else {
            self.recovery_cleanup = Some(error);
        }
    }
}

impl fmt::Display for SessionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "profile-2 session failed; stock restoration: {:?}",
            self.restoration
        )?;
        for (label, error) in [
            ("primary", self.primary.as_ref()),
            ("same-handle restoration", self.same_handle_restore.as_ref()),
            ("fresh recovery", self.fresh_recovery.as_ref()),
            ("primary cleanup", self.primary_cleanup.as_ref()),
            ("recovery cleanup", self.recovery_cleanup.as_ref()),
        ] {
            if let Some(error) = error {
                write!(formatter, "; {label}: {error:#}")?;
            }
        }
        Ok(())
    }
}

impl StdError for SessionFailure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.primary
            .as_ref()
            .or(self.same_handle_restore.as_ref())
            .or(self.fresh_recovery.as_ref())
            .or(self.primary_cleanup.as_ref())
            .or(self.recovery_cleanup.as_ref())
            .map(Error::as_ref)
    }
}

fn interrupted(stage: &str, stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 session was interrupted {stage}"
    );
    Ok(())
}

fn terminate_primary<D: SessionDevice>(device: &mut D, failure: &mut SessionFailure) -> bool {
    if let Err(error) = device.terminate() {
        failure.primary_cleanup(error.context("terminate primary profile-2 worker"));
    }
    let confirmed = device.is_cleanup_confirmed();
    if !confirmed {
        failure.primary_cleanup(anyhow!(
            "primary profile-2 worker teardown remains unconfirmed"
        ));
    }
    confirmed
}

fn shutdown_primary<D: SessionDevice>(device: &mut D, failure: &mut SessionFailure) {
    if let Err(error) = device.shutdown() {
        failure.primary_cleanup(error.context("shut down primary profile-2 worker"));
        let _ = terminate_primary(device, failure);
    } else if !device.is_cleanup_confirmed() {
        failure.primary_cleanup(anyhow!(
            "orderly primary profile-2 shutdown did not confirm teardown"
        ));
        let _ = terminate_primary(device, failure);
    }
}

fn terminate_recovery<D: RecoveryDevice>(device: &mut D, failure: &mut SessionFailure) {
    if let Err(error) = device.terminate() {
        failure.recovery_cleanup(error.context("terminate fresh recovery worker"));
    }
}

fn shutdown_recovery<D: RecoveryDevice>(device: &mut D, failure: &mut SessionFailure) {
    if let Err(error) = device.shutdown() {
        failure.recovery_cleanup(error.context("shut down fresh recovery worker"));
        terminate_recovery(device, failure);
    }
}

fn recover_with_fresh_worker<F: SessionFactory>(factory: &mut F, failure: &mut SessionFailure) {
    let mut recovery = match factory.open_recovery() {
        Ok(recovery) => recovery,
        Err(error) => {
            failure.fresh_recovery = Some(error.context("open fresh stock-recovery worker"));
            failure.restoration = RestorationOutcome::Unknown;
            return;
        }
    };

    let restored = recovery
        .restore_stock()
        .context("load stock image in fresh recovery worker")
        .and_then(|()| {
            recovery
                .verify_stock()
                .context("verify stock image in fresh recovery worker")
        });
    match restored {
        Ok(()) => {
            failure.restoration = RestorationOutcome::FreshWorkerVerified;
            shutdown_recovery(&mut recovery, failure);
        }
        Err(error) => {
            failure.fresh_recovery = Some(error);
            failure.restoration = RestorationOutcome::Unknown;
            terminate_recovery(&mut recovery, failure);
        }
    }
}

fn recover_after_primary_termination<F: SessionFactory>(
    factory: &mut F,
    device: &mut F::Primary,
    failure: &mut SessionFailure,
) {
    if terminate_primary(device, failure) {
        recover_with_fresh_worker(factory, failure);
    } else {
        failure.restoration = RestorationOutcome::Unknown;
    }
}

fn restore_after_verified_custom<F: SessionFactory>(
    factory: &mut F,
    device: &mut F::Primary,
    rx_known_stopped: bool,
    failure: &mut SessionFailure,
) {
    if device.is_terminal() || !rx_known_stopped {
        recover_after_primary_termination(factory, device, failure);
        return;
    }

    let restored = device
        .restore_stock()
        .context("load stock image on the primary handle")
        .and_then(|()| {
            device
                .verify_stock()
                .context("verify stock image on the primary handle")
        });
    match restored {
        Ok(()) => {
            failure.restoration = RestorationOutcome::SameHandleVerified;
            shutdown_primary(device, failure);
        }
        Err(error) => {
            failure.same_handle_restore = Some(error);
            recover_after_primary_termination(factory, device, failure);
        }
    }
}

fn execute<F: SessionFactory>(
    plan: SessionPlan,
    factory: &mut F,
    stop: &AtomicBool,
) -> std::result::Result<SessionSuccess<F::Summary>, SessionFailure> {
    let mut failure = SessionFailure::new();
    if let Err(error) = interrupted("before opening a worker", stop) {
        failure.primary(error);
        return Err(failure);
    }

    let mut device = match factory.open_primary(plan) {
        Ok(device) => device,
        Err(error) => {
            failure.primary(error.context("open primary profile-2 worker"));
            return Err(failure);
        }
    };
    if let Err(error) = interrupted("before custom-image dispatch", stop) {
        failure.primary(error);
        shutdown_primary(&mut device, &mut failure);
        return Err(failure);
    }

    // From this point onward, even an error return can mean that custom
    // configuration reached the FPGA. Set the obligation before the call.
    failure.restoration = RestorationOutcome::Unknown;
    if let Err(error) = device
        .load_profile2()
        .context("dispatch the fixed profile-2 RAM image")
    {
        failure.primary(error);
        recover_after_primary_termination(factory, &mut device, &mut failure);
        return Err(failure);
    }
    if let Err(error) = interrupted("before custom-image verification", stop) {
        failure.primary(error);
        recover_after_primary_termination(factory, &mut device, &mut failure);
        return Err(failure);
    }
    if let Err(error) = device
        .verify_profile2()
        .context("verify the loaded profile-2 image identity")
    {
        failure.primary(error);
        recover_after_primary_termination(factory, &mut device, &mut failure);
        return Err(failure);
    }

    let mut applied = None;
    let mut readback = None;
    let mut capture = None;
    let mut receive_dispatched = false;
    let mut rx_known_stopped = true;

    if let Err(error) = interrupted("after custom-image verification", stop) {
        failure.primary(error);
    }
    if !failure.has_error() {
        match Controller::new(&mut device, plan.max_status_polls)
            .and_then(|mut controller| controller.apply_stopped(plan.request))
            .context("apply stopped profile-2 control")
        {
            Ok(value) => applied = Some(value),
            Err(error) => failure.primary(error),
        }
    }
    if !failure.has_error()
        && let Err(error) = interrupted("after profile-2 control apply", stop)
    {
        failure.primary(error);
    }
    if !failure.has_error() {
        match device
            .configure_receive_and_verify(plan)
            .context("configure and verify fixed receive settings")
        {
            Ok(value) => readback = Some(value),
            Err(error) => failure.primary(error),
        }
    }
    if !failure.has_error()
        && let Err(error) = interrupted("before receive start", stop)
    {
        failure.primary(error);
    }
    if !failure.has_error() {
        receive_dispatched = true;
        rx_known_stopped = false;
        if let Err(error) = device
            .start_receive(plan)
            .context("start fixed receive stream")
        {
            failure.primary(error);
        }
    }
    if receive_dispatched
        && !failure.has_error()
        && let Err(error) = interrupted("after receive start", stop)
    {
        failure.primary(error);
    }
    if receive_dispatched && !failure.has_error() {
        match device.capture(plan).context("capture profile-2 stream") {
            Ok(value) => capture = Some(value),
            Err(error) => failure.primary(error),
        }
    }
    if receive_dispatched && !device.is_terminal() {
        match device
            .stop_receive()
            .context("stop profile-2 receive stream")
        {
            Ok(()) => rx_known_stopped = true,
            Err(error) => {
                failure.primary(error);
                rx_known_stopped = false;
            }
        }
    }
    if let Err(error) = interrupted("after receive completion", stop) {
        failure.primary(error);
    }

    restore_after_verified_custom(factory, &mut device, rx_known_stopped, &mut failure);

    if failure.has_error() {
        return Err(failure);
    }
    if failure.restoration != RestorationOutcome::SameHandleVerified {
        failure.primary(anyhow!(
            "successful profile-2 session did not verify same-handle stock restoration"
        ));
        return Err(failure);
    }
    let applied = match applied.context("profile-2 session lacks applied control") {
        Ok(applied) => applied,
        Err(error) => {
            failure.primary(error);
            return Err(failure);
        }
    };
    let readback = match readback.context("profile-2 session lacks RF readback") {
        Ok(readback) => readback,
        Err(error) => {
            failure.primary(error);
            return Err(failure);
        }
    };
    let capture = match capture.context("profile-2 session lacks capture evidence") {
        Ok(capture) => capture,
        Err(error) => {
            failure.primary(error);
            return Err(failure);
        }
    };
    let summary = match factory
        .validate_capture(plan, applied, readback, capture)
        .context("validate captured profile-2 stream after stock restoration")
    {
        Ok(summary) => summary,
        Err(error) => {
            failure.primary(error);
            return Err(failure);
        }
    };
    Ok(SessionSuccess {
        applied,
        readback,
        summary,
        restoration: failure.restoration,
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Fault {
    OpenPrimary,
    LoadProfile2,
    VerifyProfile2,
    Configure,
    Start,
    Capture,
    Stop,
    SameRestore,
    SameVerifyStock,
    PrimaryShutdown,
    PrimaryTerminate,
    OpenRecovery,
    RecoveryRestore,
    RecoveryVerifyStock,
    RecoveryShutdown,
    RecoveryTerminate,
    Validate,
    TerminalControl,
    TerminalStart,
    TerminalCapture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopPoint {
    AfterOpen,
    AfterLoad,
    AfterControl,
    AfterStart,
    DuringCapture,
}

#[derive(Clone, Debug, Default)]
struct ModelScript {
    faults: BTreeSet<Fault>,
    stop_point: Option<StopPoint>,
    control_fail_after: Option<usize>,
}

#[derive(Debug)]
struct ModelFactory<'a> {
    script: ModelScript,
    events: Rc<RefCell<Vec<&'static str>>>,
    stop: &'a AtomicBool,
}

impl<'a> ModelFactory<'a> {
    fn new(stop: &'a AtomicBool) -> Self {
        Self {
            script: ModelScript::default(),
            events: Rc::new(RefCell::new(Vec::new())),
            stop,
        }
    }

    fn with_faults(stop: &'a AtomicBool, faults: &[Fault]) -> Self {
        let mut factory = Self::new(stop);
        factory.script.faults.extend(faults.iter().copied());
        factory
    }

    fn events(&self) -> Vec<&'static str> {
        self.events.borrow().clone()
    }
}

#[derive(Debug, Default)]
struct RegisterModel {
    shadow: [u32; 4],
    written_mask: u8,
    applied: [u32; 4],
    applied_epoch: u32,
    outcome_epoch: u32,
    committed: bool,
    busy_reads: u8,
    operations: usize,
    control_event: bool,
}

#[derive(Debug)]
struct ModelPrimary<'a> {
    script: ModelScript,
    events: Rc<RefCell<Vec<&'static str>>>,
    stop: &'a AtomicBool,
    registers: RegisterModel,
    terminal: bool,
    cleanup_confirmed: bool,
}

impl ModelPrimary<'_> {
    fn event(&self, event: &'static str) {
        self.events.borrow_mut().push(event);
    }

    fn fail(&self, fault: Fault, label: &str) -> Result<()> {
        if self.script.faults.contains(&fault) {
            bail!("synthetic {label} failure");
        }
        Ok(())
    }

    fn control_operation(&mut self) -> Result<()> {
        if !self.registers.control_event {
            self.event("control");
            self.registers.control_event = true;
        }
        self.registers.operations += 1;
        if self.script.stop_point == Some(StopPoint::AfterControl)
            && self.registers.operations == 22
        {
            self.stop.store(true, Ordering::Relaxed);
        }
        if self.script.control_fail_after == Some(self.registers.operations) {
            self.terminal = self.script.faults.contains(&Fault::TerminalControl);
            bail!("synthetic control failure");
        }
        Ok(())
    }

    fn status(&mut self) -> u32 {
        if self.registers.committed && self.registers.busy_reads > 0 {
            self.registers.busy_reads -= 1;
            return (1 << 31) | (u32::from(self.registers.written_mask) << 20);
        }
        if self.registers.committed {
            self.registers.committed = false;
            self.registers.applied = self.registers.shadow;
            self.registers.written_mask = 0;
            self.registers.applied_epoch += 1;
            self.registers.outcome_epoch = self.registers.applied_epoch;
            return (1 << 30) | (1 << 29);
        }
        u32::from(self.registers.written_mask) << 20
    }
}

impl RegisterIo for ModelPrimary<'_> {
    fn read(&mut self, address: RegisterAddress) -> Result<u32> {
        self.control_operation()?;
        match address.byte_offset() {
            0x00 => Ok(PROFILE2_ID),
            0x04 => Ok(PROFILE2_CAPABILITIES),
            0x10 | 0x14 | 0x18 | 0x1c => {
                Ok(self.registers.shadow[((address.byte_offset() - 0x10) / 4) as usize])
            }
            0x24 => Ok(self.status()),
            0x28 => Ok(self.registers.applied_epoch),
            0x2c => Ok(self.registers.outcome_epoch),
            0x30 | 0x34 | 0x38 | 0x3c => {
                Ok(self.registers.applied[((address.byte_offset() - 0x30) / 4) as usize])
            }
            offset => bail!("unexpected model read at 0x{offset:02x}"),
        }
    }

    fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()> {
        self.control_operation()?;
        match address.byte_offset() {
            0x10 | 0x14 | 0x18 | 0x1c => {
                let index = ((address.byte_offset() - 0x10) / 4) as usize;
                self.registers.shadow[index] = value;
                self.registers.written_mask |= 1 << index;
                Ok(())
            }
            0x20 => {
                ensure!(value == 0x434d_4954, "unexpected model commit value");
                self.registers.committed = true;
                self.registers.busy_reads = 1;
                Ok(())
            }
            offset => bail!("unexpected model write at 0x{offset:02x}"),
        }
    }
}

impl RecoveryDevice for ModelPrimary<'_> {
    fn restore_stock(&mut self) -> Result<()> {
        self.event("restore_stock_primary");
        self.fail(Fault::SameRestore, "same-handle restore")
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.event("verify_stock_primary");
        self.fail(Fault::SameVerifyStock, "same-handle stock verification")
    }

    fn shutdown(&mut self) -> Result<()> {
        self.event("shutdown_primary");
        self.fail(Fault::PrimaryShutdown, "primary shutdown")?;
        self.cleanup_confirmed = true;
        Ok(())
    }

    fn terminate(&mut self) -> Result<()> {
        self.event("terminate_primary");
        self.fail(Fault::PrimaryTerminate, "primary termination")?;
        self.cleanup_confirmed = true;
        Ok(())
    }
}

impl SessionDevice for ModelPrimary<'_> {
    type Capture = u64;

    fn load_profile2(&mut self) -> Result<()> {
        self.event("load_profile2");
        self.fail(Fault::LoadProfile2, "profile-2 load")?;
        if self.script.stop_point == Some(StopPoint::AfterLoad) {
            self.stop.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn verify_profile2(&mut self) -> Result<()> {
        self.event("verify_profile2");
        self.fail(Fault::VerifyProfile2, "profile-2 verification")
    }

    fn configure_receive_and_verify(&mut self, plan: SessionPlan) -> Result<ReceiveReadback> {
        self.event("configure_receive");
        self.fail(Fault::Configure, "receive configuration")?;
        ensure!(plan.center_hz > 0, "model center frequency is zero");
        let _ = plan.gain_db;
        Ok(ReceiveReadback::new(
            SOURCE_SAMPLE_RATE_HZ,
            plan.request.receiver(),
        ))
    }

    fn start_receive(&mut self, _plan: SessionPlan) -> Result<()> {
        self.event("start_receive");
        if self.script.faults.contains(&Fault::Start) {
            self.terminal = self.script.faults.contains(&Fault::TerminalStart);
            bail!("synthetic receive start failure");
        }
        if self.script.stop_point == Some(StopPoint::AfterStart) {
            self.stop.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn capture(&mut self, plan: SessionPlan) -> Result<Self::Capture> {
        self.event("capture");
        if self.script.stop_point == Some(StopPoint::DuringCapture) {
            self.stop.store(true, Ordering::Relaxed);
        }
        if self.script.faults.contains(&Fault::Capture) {
            self.terminal = self.script.faults.contains(&Fault::TerminalCapture);
            bail!("synthetic capture failure");
        }
        Ok(plan.records)
    }

    fn stop_receive(&mut self) -> Result<()> {
        self.event("stop_receive");
        self.fail(Fault::Stop, "receive stop")
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn is_cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed
    }
}

#[derive(Debug)]
struct ModelRecovery {
    script: ModelScript,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl ModelRecovery {
    fn event(&self, event: &'static str) {
        self.events.borrow_mut().push(event);
    }

    fn fail(&self, fault: Fault, label: &str) -> Result<()> {
        if self.script.faults.contains(&fault) {
            bail!("synthetic {label} failure");
        }
        Ok(())
    }
}

impl RecoveryDevice for ModelRecovery {
    fn restore_stock(&mut self) -> Result<()> {
        self.event("restore_stock_recovery");
        self.fail(Fault::RecoveryRestore, "fresh recovery restore")
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.event("verify_stock_recovery");
        self.fail(Fault::RecoveryVerifyStock, "fresh stock verification")
    }

    fn shutdown(&mut self) -> Result<()> {
        self.event("shutdown_recovery");
        self.fail(Fault::RecoveryShutdown, "recovery shutdown")
    }

    fn terminate(&mut self) -> Result<()> {
        self.event("terminate_recovery");
        self.fail(Fault::RecoveryTerminate, "recovery termination")
    }
}

impl<'a> SessionFactory for ModelFactory<'a> {
    type Capture = u64;
    type Primary = ModelPrimary<'a>;
    type Recovery = ModelRecovery;
    type Summary = u64;

    fn open_primary(&mut self, _plan: SessionPlan) -> Result<Self::Primary> {
        self.events.borrow_mut().push("open_primary");
        if self.script.faults.contains(&Fault::OpenPrimary) {
            bail!("synthetic primary open failure");
        }
        if self.script.stop_point == Some(StopPoint::AfterOpen) {
            self.stop.store(true, Ordering::Relaxed);
        }
        Ok(ModelPrimary {
            script: self.script.clone(),
            events: Rc::clone(&self.events),
            stop: self.stop,
            registers: RegisterModel::default(),
            terminal: false,
            cleanup_confirmed: false,
        })
    }

    fn open_recovery(&mut self) -> Result<Self::Recovery> {
        self.events.borrow_mut().push("open_recovery");
        if self.script.faults.contains(&Fault::OpenRecovery) {
            bail!("synthetic recovery open failure");
        }
        Ok(ModelRecovery {
            script: self.script.clone(),
            events: Rc::clone(&self.events),
        })
    }

    fn validate_capture(
        &mut self,
        plan: SessionPlan,
        applied: AppliedConfiguration,
        readback: ReceiveReadback,
        capture: Self::Capture,
    ) -> Result<Self::Summary> {
        self.events.borrow_mut().push("validate_capture");
        if self.script.faults.contains(&Fault::Validate) {
            bail!("synthetic stream validation failure");
        }
        ensure!(
            applied.phase_words() == plan.request.phase_words(),
            "model applied words differ from request"
        );
        ensure!(
            readback == ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, plan.request.receiver()),
            "model RFIC readback differs from request"
        );
        ensure!(capture == plan.records, "model capture extent differs");
        Ok(capture)
    }
}

/// Exercise the guarded lifecycle policy using only deterministic CPU models.
///
/// This verifies both the ordinary same-handle restoration path and the rule
/// that an ambiguous custom-image load abandons the primary worker before a
/// fresh worker restores stock. It performs no native, USB, FPGA or RF action.
pub(crate) fn verify_policy() -> Result<()> {
    let stop = AtomicBool::new(false);
    let plan = SessionPlan::reference(Receiver::Rx2, REFERENCE_WORDS)?;
    let mut happy = ModelFactory::new(&stop);
    let success = execute(plan, &mut happy, &stop).map_err(Error::new)?;
    ensure!(
        success.restoration == RestorationOutcome::SameHandleVerified
            && success.applied.phase_words() == REFERENCE_WORDS
            && success.readback == ReceiveReadback::new(SOURCE_SAMPLE_RATE_HZ, Receiver::Rx2)
            && success.summary == REFERENCE_RECORDS,
        "CPU lifecycle happy-path result drifted"
    );
    ensure!(
        happy.events()
            == [
                "open_primary",
                "load_profile2",
                "verify_profile2",
                "control",
                "configure_receive",
                "start_receive",
                "capture",
                "stop_receive",
                "restore_stock_primary",
                "verify_stock_primary",
                "shutdown_primary",
                "validate_capture",
            ],
        "CPU lifecycle happy-path ordering drifted"
    );

    let mut ambiguous = ModelFactory::with_faults(&stop, &[Fault::LoadProfile2]);
    let failure = execute(plan, &mut ambiguous, &stop)
        .err()
        .context("ambiguous-load policy unexpectedly succeeded")?;
    ensure!(
        failure.restoration == RestorationOutcome::FreshWorkerVerified,
        "ambiguous-load policy did not verify fresh-worker restoration"
    );
    ensure!(
        ambiguous.events()
            == [
                "open_primary",
                "load_profile2",
                "terminate_primary",
                "open_recovery",
                "restore_stock_recovery",
                "verify_stock_recovery",
                "shutdown_recovery",
            ],
        "ambiguous-load recovery ordering drifted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use clap::Parser;

    #[derive(Debug, Parser)]
    struct LauncherParser {
        #[command(flatten)]
        args: CaptureArgs,
    }

    fn plan(receiver: Receiver, words: [u32; 4]) -> SessionPlan {
        SessionPlan::reference(receiver, words).unwrap()
    }

    fn launch_args(receiver: CaptureReceiver, output: PathBuf) -> CaptureArgs {
        CaptureArgs {
            receiver,
            center_hz: 98_100_000,
            gain_db: 40,
            records: 100_000,
            phase_0: 0,
            phase_1: 1,
            phase_2: 0x8000_0001,
            phase_3: u32::MAX,
            max_status_polls: 4,
            transaction_timeout_seconds: 60.0,
            manifest: PathBuf::from("bundle/manifest.json"),
            manifest_sha256: "0".repeat(64),
            adapter: PathBuf::from("target/release/bladerf-profile2-adapter"),
            adapter_sha256: "1".repeat(64),
            library: PathBuf::from("build/libbladeRF.so.2"),
            library_sha256: "2".repeat(64),
            stock_image: PathBuf::from("downloads/stock.rbf"),
            serial: "0123456789abcdef0123456789abcdef".to_owned(),
            output,
        }
    }

    #[test]
    fn launcher_parsers_are_exact_and_bounded() {
        assert_eq!(parse_phase_word("0").unwrap(), 0);
        assert_eq!(parse_phase_word("4294967295").unwrap(), u32::MAX);
        assert_eq!(parse_phase_word("0xdeadBEEF").unwrap(), 0xdead_beef);
        for invalid in ["", "0x", "0X1", "+1", "1_000", "4294967296"] {
            assert!(parse_phase_word(invalid).is_err(), "accepted {invalid}");
        }

        assert!(parse_sha256(&"a".repeat(64)).is_ok());
        assert!(parse_sha256(&"A".repeat(64)).is_err());
        assert!(parse_sha256(&"0".repeat(63)).is_err());
        assert!(parse_serial("0123456789abcdef0123456789abcdef").is_ok());
        assert!(parse_serial("0123456789ABCDEF0123456789ABCDEF").is_err());

        assert_eq!(parse_status_polls("1").unwrap(), 1);
        assert_eq!(parse_status_polls("4096").unwrap(), 4096);
        assert!(parse_status_polls("0").is_err());
        assert!(parse_status_polls("4097").is_err());
        assert_eq!(
            transaction_timeout(60.0).unwrap(),
            bladerf_profile2_session_ipc::MAX_TRANSACTION_TIMEOUT
        );
        for invalid in [
            0.0,
            f64::from_bits(1),
            -1.0,
            f64::INFINITY,
            f64::NAN,
            f64::from_bits(60.0f64.to_bits() + 1),
            60.000_001,
        ] {
            assert!(transaction_timeout(invalid).is_err());
        }
    }

    #[test]
    fn launcher_cli_freezes_only_the_bounded_operational_defaults() {
        let parsed = LauncherParser::try_parse_from([
            "capture-profile2",
            "--receiver",
            "rx2",
            "--center-hz",
            "98100000",
            "--gain-db",
            "40",
            "--phase-0",
            "0",
            "--phase-1",
            "1",
            "--phase-2",
            "2",
            "--phase-3",
            "3",
            "--manifest",
            "manifest.json",
            "--manifest-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "--adapter",
            "adapter",
            "--adapter-sha256",
            "123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "--library",
            "library",
            "--library-sha256",
            "23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01",
            "--stock-image",
            "stock.rbf",
            "--serial",
            "0123456789abcdef0123456789abcdef",
            "--output",
            "private/session",
        ])
        .unwrap()
        .args;

        assert_eq!(parsed.receiver, CaptureReceiver::Rx2);
        assert_eq!(parsed.records, REFERENCE_RECORDS);
        assert_eq!(parsed.max_status_polls, 4);
        assert_eq!(parsed.transaction_timeout_seconds, 60.0);
    }

    #[test]
    fn launcher_preparation_maps_receiver_plan_and_workspace_paths_without_io() {
        for (selected, expected) in [
            (CaptureReceiver::Rx1, Receiver::Rx1),
            (CaptureReceiver::Rx2, Receiver::Rx2),
        ] {
            let (request, receiver) = prepare_launch(
                Path::new("/workspace"),
                launch_args(selected, PathBuf::from("private/session")),
            )
            .unwrap();
            assert_eq!(receiver, expected);
            assert_eq!(request.plan.request.receiver(), expected);
            assert_eq!(
                request.plan.request.phase_words(),
                [0, 1, 0x8000_0001, u32::MAX]
            );
            assert_eq!(request.plan.center_hz, 98_100_000);
            assert_eq!(request.plan.gain_db, 40);
            assert_eq!(request.plan.records, 100_000);
            assert_eq!(request.plan.max_status_polls, 4);
            assert_eq!(request.transaction_timeout, Duration::from_secs(60));
            assert_eq!(
                request.manifest,
                Path::new("/workspace/bundle/manifest.json")
            );
            assert_eq!(
                request.adapter,
                Path::new("/workspace/target/release/bladerf-profile2-adapter")
            );
            assert_eq!(
                request.library,
                Path::new("/workspace/build/libbladeRF.so.2")
            );
            assert_eq!(
                request.stock_image,
                Path::new("/workspace/downloads/stock.rbf")
            );
            assert_eq!(request.stage_parent, Path::new("/workspace/private"));
            assert_eq!(request.output_basename, OsString::from("session"));
        }

        assert!(
            prepare_launch(
                Path::new("/workspace"),
                launch_args(CaptureReceiver::Rx1, PathBuf::from("../outside")),
            )
            .is_err()
        );
        assert!(
            prepare_launch(
                Path::new("/workspace"),
                launch_args(CaptureReceiver::Rx1, PathBuf::from("private/./session"),),
            )
            .is_err()
        );
    }

    #[test]
    fn launcher_pre_set_cancellation_creates_no_output_or_stage() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("never-created");
        let error = capture(
            root.path(),
            launch_args(CaptureReceiver::Rx2, output.clone()),
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("before path access"));
        assert!(!output.exists());
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn release_policy_reference_passes() {
        verify_policy().unwrap();
    }

    #[test]
    fn invalid_plan_fails_before_a_worker_can_exist() {
        let request = CaptureRequest::new(REFERENCE_WORDS, Receiver::Rx1);
        assert!(SessionPlan::new(request, 0, 0, MIN_RECORDS, 1).is_err());
        assert!(SessionPlan::new(request, 1, MIN_GAIN_DB - 1, MIN_RECORDS, 1).is_err());
        assert!(SessionPlan::new(request, 1, MAX_GAIN_DB + 1, MIN_RECORDS, 1).is_err());
        assert!(SessionPlan::new(request, 1, 0, MIN_RECORDS - 1, 1).is_err());
        assert!(SessionPlan::new(request, 1, 0, MAX_RECORDS + 1, 1).is_err());
        assert!(SessionPlan::new(request, 1, 0, MIN_RECORDS, 0).is_err());
    }

    #[test]
    fn interruption_before_open_or_load_needs_no_restoration() {
        let stopped = AtomicBool::new(true);
        let mut factory = ModelFactory::new(&stopped);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stopped).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::NotRequired);
        assert!(factory.events().is_empty());

        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.stop_point = Some(StopPoint::AfterOpen);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::NotRequired);
        assert_eq!(factory.events(), ["open_primary", "shutdown_primary"]);
    }

    #[test]
    fn ambiguous_load_or_identity_failure_uses_only_fresh_recovery() {
        for fault in [Fault::LoadProfile2, Fault::VerifyProfile2] {
            let stop = AtomicBool::new(false);
            let mut factory = ModelFactory::with_faults(&stop, &[fault]);
            let failure =
                execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
            assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
            let events = factory.events();
            assert!(events.contains(&"terminate_primary"));
            assert!(events.contains(&"open_recovery"));
            assert!(!events.contains(&"restore_stock_primary"));
            assert!(!events.contains(&"control"));
        }
    }

    #[test]
    fn unconfirmed_primary_teardown_blocks_fresh_recovery() {
        let stop = AtomicBool::new(false);
        let mut factory =
            ModelFactory::with_faults(&stop, &[Fault::LoadProfile2, Fault::PrimaryTerminate]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::Unknown);
        assert!(failure.primary_cleanup.is_some());
        assert_eq!(
            factory.events(),
            ["open_primary", "load_profile2", "terminate_primary"]
        );
        assert!(!factory.events().contains(&"open_recovery"));
    }

    #[test]
    fn partial_control_failure_restores_on_same_handle_unless_terminal() {
        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.control_fail_after = Some(8);
        let failure = execute(plan(Receiver::Rx1, [1, 2, 3, 4]), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(factory.events().contains(&"restore_stock_primary"));
        assert!(!factory.events().contains(&"open_recovery"));

        let mut terminal = ModelFactory::with_faults(&stop, &[Fault::TerminalControl]);
        terminal.script.control_fail_after = Some(8);
        let failure = execute(plan(Receiver::Rx2, [5, 6, 7, 8]), &mut terminal, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
        assert!(!terminal.events().contains(&"restore_stock_primary"));
    }

    #[test]
    fn configuration_start_and_capture_failures_preserve_rx_state_rules() {
        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::with_faults(&stop, &[Fault::Configure]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(!factory.events().contains(&"start_receive"));

        let mut factory = ModelFactory::with_faults(&stop, &[Fault::Start]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(factory.events().contains(&"stop_receive"));

        let mut factory = ModelFactory::with_faults(&stop, &[Fault::Capture]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(factory.events().contains(&"stop_receive"));

        let mut factory = ModelFactory::with_faults(&stop, &[Fault::Capture, Fault::Stop]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
        assert!(!factory.events().contains(&"restore_stock_primary"));

        for faults in [
            &[Fault::Start, Fault::TerminalStart][..],
            &[Fault::Capture, Fault::TerminalCapture][..],
        ] {
            let mut factory = ModelFactory::with_faults(&stop, faults);
            let failure =
                execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
            assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
            assert!(!factory.events().contains(&"stop_receive"));
            assert!(!factory.events().contains(&"restore_stock_primary"));
        }
    }

    #[test]
    fn failed_same_handle_restore_falls_back_once_and_preserves_diagnostics() {
        let stop = AtomicBool::new(false);
        for fault in [Fault::SameRestore, Fault::SameVerifyStock] {
            let mut factory = ModelFactory::with_faults(&stop, &[fault]);
            let failure =
                execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
            assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
            assert!(failure.same_handle_restore.is_some());
            assert_eq!(
                factory
                    .events()
                    .iter()
                    .filter(|e| **e == "open_recovery")
                    .count(),
                1
            );
        }

        let mut factory =
            ModelFactory::with_faults(&stop, &[Fault::SameRestore, Fault::RecoveryRestore]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::Unknown);
        assert!(failure.same_handle_restore.is_some());
        assert!(failure.fresh_recovery.is_some());
        let text = failure.to_string();
        assert!(text.contains("same-handle restoration"));
        assert!(text.contains("fresh recovery"));
    }

    #[test]
    fn cancellation_uses_recovery_appropriate_to_the_known_state() {
        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.stop_point = Some(StopPoint::AfterLoad);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
        assert!(!factory.events().contains(&"verify_profile2"));
        assert!(!factory.events().contains(&"restore_stock_primary"));

        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.stop_point = Some(StopPoint::AfterControl);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(!factory.events().contains(&"configure_receive"));

        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.stop_point = Some(StopPoint::AfterStart);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(!factory.events().contains(&"capture"));
        assert!(factory.events().contains(&"stop_receive"));

        let stop = AtomicBool::new(false);
        let mut factory = ModelFactory::new(&stop);
        factory.script.stop_point = Some(StopPoint::DuringCapture);
        let failure =
            execute(plan(Receiver::Rx1, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        assert!(factory.events().contains(&"restore_stock_primary"));
    }

    #[test]
    fn stream_validation_and_shutdown_failures_keep_known_stock() {
        let stop = AtomicBool::new(false);
        for fault in [Fault::Validate, Fault::PrimaryShutdown] {
            let mut factory = ModelFactory::with_faults(&stop, &[fault]);
            let failure =
                execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
            assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
            assert!(factory.events().contains(&"verify_stock_primary"));
            if fault == Fault::Validate {
                assert!(factory.events().contains(&"validate_capture"));
            } else {
                assert!(!factory.events().contains(&"validate_capture"));
                assert!(factory.events().contains(&"terminate_primary"));
            }
        }

        let mut factory =
            ModelFactory::with_faults(&stop, &[Fault::PrimaryShutdown, Fault::PrimaryTerminate]);
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::SameHandleVerified);
        let cleanup = format!("{:#}", failure.primary_cleanup.unwrap());
        assert!(cleanup.contains("shut down primary profile-2 worker"));
        assert!(cleanup.contains("terminate primary profile-2 worker"));
    }

    #[test]
    fn fresh_recovery_failures_and_cleanup_are_retained() {
        let stop = AtomicBool::new(false);
        for fault in [
            Fault::OpenRecovery,
            Fault::RecoveryRestore,
            Fault::RecoveryVerifyStock,
        ] {
            let mut factory = ModelFactory::with_faults(&stop, &[Fault::LoadProfile2, fault]);
            let failure =
                execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
            assert_eq!(failure.restoration, RestorationOutcome::Unknown);
            assert!(failure.fresh_recovery.is_some());
        }

        let mut factory = ModelFactory::with_faults(
            &stop,
            &[
                Fault::LoadProfile2,
                Fault::RecoveryShutdown,
                Fault::RecoveryTerminate,
            ],
        );
        let failure =
            execute(plan(Receiver::Rx2, REFERENCE_WORDS), &mut factory, &stop).unwrap_err();
        assert_eq!(failure.restoration, RestorationOutcome::FreshWorkerVerified);
        let cleanup = format!("{:#}", failure.recovery_cleanup.unwrap());
        assert!(cleanup.contains("shut down fresh recovery worker"));
        assert!(cleanup.contains("terminate fresh recovery worker"));
        assert!(factory.events().contains(&"terminate_recovery"));
    }

    #[test]
    fn rx1_rx2_and_arbitrary_words_reach_the_controller() {
        for (receiver, words) in [
            (Receiver::Rx1, [0, 1, u32::MAX, 0x8000_0000]),
            (Receiver::Rx2, [9, 8, 7, 6]),
        ] {
            let stop = AtomicBool::new(false);
            let mut factory = ModelFactory::new(&stop);
            let success = execute(plan(receiver, words), &mut factory, &stop).unwrap();
            assert_eq!(success.applied.phase_words(), words);
            assert_eq!(success.restoration, RestorationOutcome::SameHandleVerified);
        }
    }
}
