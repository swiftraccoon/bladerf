//! Safe state machine for one guarded loader-neutral bladeRF worker.
//!
//! Image dispatch is role-typed and synchronous. Every FPGA load and identity
//! check is marked terminal before entering its callback, then advances only
//! after exact success. A failure or unwind permits no later device call,
//! including close.
//! If any live handle remains when the worker drops, it intentionally retains
//! the call-table owner and mapped native library until process teardown so a
//! foreign worker cannot execute code from an unloaded mapping.
//! Recovery therefore belongs to a fresh process with an independently staged
//! stock image.

#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString, c_int};
use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};
use bladerf_profile2_ipc::{COMMIT_BYTE_OFFSET, COMMIT_MAGIC, ReadRegister, ShadowChannel};
use sha2::{Digest, Sha256};

use crate::sealed_image::{
    CustomImage, ExpectedImage, ImageStagingReport, SealedImage, StagedImages, StockImage,
};

pub(crate) const LOADER_SERIAL_BYTES: usize = 33;
pub(crate) const LOADER_FPGA_A9: c_int = 301;
const LOADER_FPGA_SOURCE_HOST: c_int = 2;
const PROFILE2_ID: u32 = 0x5246_0201;
const PROFILE2_CAPABILITIES: u32 = 0x0000_1f04;
pub(crate) const SOURCE_SAMPLE_RATE_HZ: u32 = 20_000_000;
pub(crate) const SOURCE_BANDWIDTH_HZ: u32 = 20_000_000;
const MIN_GAIN_DB: i32 = -16;
const MAX_GAIN_DB: i32 = 60;
const MIN_DATA_RECORDS: u64 = 100_000;
const MAX_DATA_RECORDS: u64 = 10_000_000;
const TRANSPORT_SLOTS_PER_RECORD: u64 = 8;
const BYTES_PER_TRANSPORT_SLOT: u64 = 4;
const PROFILE2_RECORD_BYTES: usize = 32;
const PROFILE2_WIRE_MAGIC_LOW: u16 = 0x5246;
const PROFILE2_WIRE_MAGIC_HIGH: u16 = 0x4f42;
const PROFILE2_WIRE_VERSION_FLAGS: u16 = 0x2000;
const PROFILE2_WIRE_RX2_FLAG: u16 = 0x0020;
const PROFILE2_WIRE_CONFIG_FLAG: u16 = 0x0010;
const PROFILE2_WIRE_PROFILE_ID: u16 = 2;
const PROFILE2_FIRST_SOURCE_INDEX: u64 = 19;
const PROFILE2_SOURCE_INDEX_STEP: u64 = 20;
const MODEL_CAPTURE_CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const MAX_FREQUENCY_ERROR_HZ: u64 = 10;

/// One explicitly selected receive connector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReceiveChannel {
    Rx1,
    Rx2,
}

/// Fully bounded receive request retained by one session worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReceivePlan {
    channel: ReceiveChannel,
    center_hz: u64,
    gain_db: i32,
    data_records: u64,
    expected_samples: u32,
    expected_bytes: u64,
}

impl ReceivePlan {
    pub(crate) fn new(
        channel: ReceiveChannel,
        center_hz: u64,
        gain_db: i32,
        data_records: u64,
    ) -> Result<Self> {
        ensure!(center_hz > 0, "profile-2 center frequency must be positive");
        ensure!(
            (MIN_GAIN_DB..=MAX_GAIN_DB).contains(&gain_db),
            "profile-2 gain must be {MIN_GAIN_DB} through {MAX_GAIN_DB} dB"
        );
        ensure!(
            (MIN_DATA_RECORDS..=MAX_DATA_RECORDS).contains(&data_records),
            "profile-2 DATA record count must be {MIN_DATA_RECORDS} through {MAX_DATA_RECORDS}"
        );
        let framed_records = data_records
            .checked_add(1)
            .context("profile-2 CONFIG plus DATA record count overflow")?;
        let samples = framed_records
            .checked_mul(TRANSPORT_SLOTS_PER_RECORD)
            .context("profile-2 capture sample count overflow")?;
        let expected_samples = u32::try_from(samples)
            .context("profile-2 capture sample count exceeds the native API bound")?;
        let expected_bytes = samples
            .checked_mul(BYTES_PER_TRANSPORT_SLOT)
            .context("profile-2 capture byte count overflow")?;
        Ok(Self {
            channel,
            center_hz,
            gain_db,
            data_records,
            expected_samples,
            expected_bytes,
        })
    }

    pub(crate) const fn channel(self) -> ReceiveChannel {
        self.channel
    }

    pub(crate) const fn center_hz(self) -> u64 {
        self.center_hz
    }

    pub(crate) const fn gain_db(self) -> i32 {
        self.gain_db
    }

    pub(crate) const fn data_records(self) -> u64 {
        self.data_records
    }

    pub(crate) const fn expected_samples(self) -> u32 {
        self.expected_samples
    }

    pub(crate) const fn expected_bytes(self) -> u64 {
        self.expected_bytes
    }
}

/// RF settings independently read back while RX remains disabled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConfiguredReceiveReadback {
    channel: ReceiveChannel,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    bandwidth_hz: u32,
}

impl ConfiguredReceiveReadback {
    pub(crate) const fn new(
        channel: ReceiveChannel,
        center_frequency_hz: u64,
        sample_rate_hz: u32,
        bandwidth_hz: u32,
    ) -> Self {
        Self {
            channel,
            center_frequency_hz,
            sample_rate_hz,
            bandwidth_hz,
        }
    }

    pub(crate) const fn channel(self) -> ReceiveChannel {
        self.channel
    }

    pub(crate) const fn center_frequency_hz(self) -> u64 {
        self.center_frequency_hz
    }

    pub(crate) const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    pub(crate) const fn bandwidth_hz(self) -> u32 {
        self.bandwidth_hz
    }
}

/// Active RX readback, including gain observed only after enable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ActiveReceiveReadback {
    configured: ConfiguredReceiveReadback,
    gain_db: i32,
}

impl ActiveReceiveReadback {
    pub(crate) const fn new(configured: ConfiguredReceiveReadback, gain_db: i32) -> Self {
        Self {
            configured,
            gain_db,
        }
    }

    pub(crate) const fn configured(self) -> ConfiguredReceiveReadback {
        self.configured
    }

    pub(crate) const fn gain_db(self) -> i32 {
        self.gain_db
    }
}

/// Exact extent and digest of one completed raw capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CaptureReport {
    samples: u32,
    bytes: u64,
    sha256: String,
}

impl CaptureReport {
    pub(crate) fn new(samples: u32, bytes: u64, sha256: impl Into<String>) -> Self {
        Self {
            samples,
            bytes,
            sha256: sha256.into(),
        }
    }

    pub(crate) const fn samples(&self) -> u32 {
        self.samples
    }

    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Numeric FPGA version used by the narrow loader ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LoaderFpgaVersion {
    pub(crate) major: u16,
    pub(crate) minor: u16,
    pub(crate) patch: u16,
}

impl LoaderFpgaVersion {
    pub(crate) const PROFILE2_REQUIRED: Self = Self {
        major: 0,
        minor: 16,
        patch: 0,
    };
}

impl fmt::Display for LoaderFpgaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Reviewed native calls available to one loader-neutral worker.
///
/// Only the isolated native implementation may convert a sealed image owner to
/// its borrowed C path. Callers cannot supply an arbitrary load path.
pub(crate) trait LoaderCalls {
    type Handle: Copy;

    fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>);
    fn close(&mut self, handle: Self::Handle);
    fn error_text(&self, status: c_int) -> Option<String>;
    fn get_serial(&mut self, handle: Self::Handle, serial: &mut [u8; LOADER_SERIAL_BYTES])
    -> c_int;
    fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>>;
    fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int;
    fn load_custom(&mut self, handle: Self::Handle, image: &SealedImage<CustomImage>) -> c_int;
    fn load_stock(&mut self, handle: Self::Handle, image: &SealedImage<StockImage>) -> c_int;
    fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int;
    fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int;
    fn fpga_version(&mut self, handle: Self::Handle, version: &mut LoaderFpgaVersion) -> c_int;
    fn read_profile_id(&mut self, handle: Self::Handle, value: &mut u32) -> c_int;
    fn read_profile_capabilities(&mut self, handle: Self::Handle, value: &mut u32) -> c_int;
}

/// Reviewed receive-only calls available after exact custom-image identity.
///
/// The native implementation fixes every RF and stream parameter internally;
/// this trait contains no transmit, flash, bias-tee or arbitrary-address API.
pub(crate) trait SessionCalls: LoaderCalls {
    fn read_register(
        &mut self,
        handle: Self::Handle,
        register: ReadRegister,
        value: &mut u32,
    ) -> c_int;
    fn write_shadow(&mut self, handle: Self::Handle, channel: ShadowChannel, value: u32) -> c_int;
    fn commit(&mut self, handle: Self::Handle) -> c_int;
    fn configure_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ConfiguredReceiveReadback,
    ) -> Result<()>;
    fn start_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ActiveReceiveReadback,
    ) -> Result<()>;
    fn capture_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        output: &mut dyn Write,
    ) -> Result<CaptureReport>;
    fn stop_receive(&mut self, handle: Self::Handle, channel: ReceiveChannel) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoaderState {
    Ready,
    CustomUnverified,
    CustomVerified,
    ReceiveConfigured,
    ReceiveActive,
    CaptureComplete,
    ReceiveStopped,
    StockUnverified,
    StockVerified,
    Terminal,
    Closed,
}

struct LoaderCore<C: LoaderCalls> {
    calls: Option<C>,
    handle: Option<C::Handle>,
    state: LoaderState,
}

impl<C: LoaderCalls> LoaderCore<C> {
    fn open(mut calls: C, serial: &str) -> Result<Self> {
        validate_requested_serial(serial)?;
        let identifier = CString::new(format!("libusb:serial={serial}"))
            .context("construct exact bladeRF loader selector")?;
        let (status, handle) = calls.open(&identifier);
        if status != 0 {
            let error = native_error(&calls, status, "open exact serial-selected bladeRF");
            if let Some(handle) = handle {
                calls.close(handle);
            }
            return Err(error);
        }
        let handle =
            handle.context("libbladeRF reported successful loader open with a null handle")?;
        let mut core = Self {
            calls: Some(calls),
            handle: Some(handle),
            state: LoaderState::Ready,
        };
        core.verify_open_identity(serial)?;
        Ok(core)
    }

    fn handle(&self) -> Result<C::Handle> {
        self.handle
            .context("loader worker has no live device handle")
    }

    fn calls(&self) -> &C {
        self.calls
            .as_ref()
            .expect("loader calls owner is retained until LoaderCore drop")
    }

    fn calls_mut(&mut self) -> &mut C {
        self.calls
            .as_mut()
            .expect("loader calls owner is retained until LoaderCore drop")
    }

    fn require_state(&self, expected: LoaderState, operation: &str) -> Result<()> {
        ensure!(
            self.state == expected,
            "cannot {operation} while loader worker state is {:?}",
            self.state
        );
        Ok(())
    }

    fn verify_open_identity(&mut self, expected_serial: &str) -> Result<()> {
        let handle = self.handle()?;
        let mut serial = [0u8; LOADER_SERIAL_BYTES];
        let status = self.calls_mut().get_serial(handle, &mut serial);
        ensure_status(self.calls(), status, "read opened bladeRF serial")?;
        validate_returned_serial(&serial, expected_serial)?;

        let board_name = self
            .calls_mut()
            .get_board_name(handle)
            .context("opened bladeRF returned a null board name")?;
        ensure!(
            board_name == b"bladerf2",
            "opened board name {:?} differs from required bladerf2",
            String::from_utf8_lossy(&board_name)
        );

        let mut fpga_size = 0;
        let status = self.calls_mut().get_fpga_size(handle, &mut fpga_size);
        ensure_status(self.calls(), status, "read bladeRF FPGA size")?;
        ensure!(
            fpga_size == LOADER_FPGA_A9,
            "bladeRF FPGA size {fpga_size} differs from required xA9 value {LOADER_FPGA_A9}"
        );
        Ok(())
    }

    fn load_custom(&mut self, image: &SealedImage<CustomImage>) -> Result<()> {
        self.require_state(LoaderState::Ready, "load the custom profile-2 image")?;
        let handle = self.handle()?;
        // Fail closed before entering a safe trait callback. An unexpected
        // unwind therefore cannot leave this handle eligible for verification
        // or orderly close.
        self.state = LoaderState::Terminal;
        let status = self.calls_mut().load_custom(handle, image);
        if status != 0 {
            return Err(native_error(
                self.calls(),
                status,
                "load sealed custom profile-2 image",
            ));
        }
        self.state = LoaderState::CustomUnverified;
        Ok(())
    }

    fn verify_custom(&mut self) -> Result<()> {
        self.require_state(
            LoaderState::CustomUnverified,
            "verify the custom profile-2 image",
        )?;
        self.state = LoaderState::Terminal;
        match self.verify_loaded_identity(true) {
            Ok(()) => {
                self.state = LoaderState::CustomVerified;
                Ok(())
            }
            Err(error) => {
                self.state = LoaderState::Terminal;
                Err(error)
            }
        }
    }

    fn load_stock_after_custom(&mut self, image: &SealedImage<StockImage>) -> Result<()> {
        ensure!(
            matches!(
                self.state,
                LoaderState::CustomVerified
                    | LoaderState::ReceiveConfigured
                    | LoaderState::ReceiveStopped
            ),
            "cannot restore the stock image while loader worker state is {:?}",
            self.state
        );
        self.dispatch_stock(image)
    }

    fn load_stock_for_recovery(&mut self, image: &SealedImage<StockImage>) -> Result<()> {
        self.require_state(LoaderState::Ready, "load the stock recovery image")?;
        self.dispatch_stock(image)
    }

    fn dispatch_stock(&mut self, image: &SealedImage<StockImage>) -> Result<()> {
        let handle = self.handle()?;
        // As with custom dispatch, an unwind is an ambiguous terminal result.
        self.state = LoaderState::Terminal;
        let status = self.calls_mut().load_stock(handle, image);
        if status != 0 {
            return Err(native_error(
                self.calls(),
                status,
                "load sealed stock recovery image",
            ));
        }
        self.state = LoaderState::StockUnverified;
        Ok(())
    }

    fn verify_stock(&mut self) -> Result<()> {
        self.require_state(
            LoaderState::StockUnverified,
            "verify the stock recovery image",
        )?;
        self.state = LoaderState::Terminal;
        match self.verify_loaded_identity(false) {
            Ok(()) => {
                self.state = LoaderState::StockVerified;
                Ok(())
            }
            Err(error) => {
                self.state = LoaderState::Terminal;
                Err(error)
            }
        }
    }

    fn verify_loaded_identity(&mut self, require_profile2: bool) -> Result<()> {
        let handle = self.handle()?;
        let configured = self.calls_mut().is_fpga_configured(handle);
        if configured < 0 {
            return Err(native_error(
                self.calls(),
                configured,
                "read bladeRF FPGA configured state",
            ));
        }
        ensure!(
            configured == 1,
            "bladeRF FPGA configured state {configured} differs from required 1"
        );

        let mut source = 0;
        let status = self.calls_mut().get_fpga_source(handle, &mut source);
        ensure_status(self.calls(), status, "read bladeRF FPGA source")?;
        ensure!(
            source == LOADER_FPGA_SOURCE_HOST,
            "bladeRF FPGA source {source} differs from required host-loaded value {LOADER_FPGA_SOURCE_HOST}"
        );

        let mut version = LoaderFpgaVersion {
            major: 0,
            minor: 0,
            patch: 0,
        };
        let status = self.calls_mut().fpga_version(handle, &mut version);
        ensure_status(self.calls(), status, "read bladeRF FPGA version")?;
        ensure!(
            version == LoaderFpgaVersion::PROFILE2_REQUIRED,
            "bladeRF FPGA version {version} differs from required {}",
            LoaderFpgaVersion::PROFILE2_REQUIRED
        );

        if require_profile2 {
            let mut id = 0;
            let status = self.calls_mut().read_profile_id(handle, &mut id);
            ensure_status(self.calls(), status, "read profile-2 ID")?;
            ensure!(
                id == PROFILE2_ID,
                "profile-2 ID 0x{id:08x} differs from required 0x{PROFILE2_ID:08x}"
            );

            let mut capabilities = 0;
            let status = self
                .calls_mut()
                .read_profile_capabilities(handle, &mut capabilities);
            ensure_status(self.calls(), status, "read profile-2 capabilities")?;
            ensure!(
                capabilities == PROFILE2_CAPABILITIES,
                "profile-2 capabilities 0x{capabilities:08x} differs from required 0x{PROFILE2_CAPABILITIES:08x}"
            );
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        ensure!(
            matches!(self.state, LoaderState::Ready | LoaderState::StockVerified),
            "cannot orderly close loader worker while state is {:?}",
            self.state
        );
        if let Some(handle) = self.handle.take() {
            self.calls_mut().close(handle);
        }
        self.state = LoaderState::Closed;
        Ok(())
    }

    fn is_terminal(&self) -> bool {
        self.state == LoaderState::Terminal
    }

    fn close_on_safe_drop(&mut self) {
        if matches!(self.state, LoaderState::Ready | LoaderState::StockVerified) {
            if let Some(handle) = self.handle.take() {
                self.calls_mut().close(handle);
            }
            self.state = LoaderState::Closed;
        }
    }
}

impl<C: LoaderCalls> Drop for LoaderCore<C> {
    fn drop(&mut self) {
        self.close_on_safe_drop();
        if self.handle.is_some() {
            // An ambiguous or protocol-aborted worker deliberately makes no
            // later device call, including close. Retain the call table, its
            // mapped library, and its sealed backing file until kernel process
            // teardown as well: unloading libbladeRF while a live handle or
            // synchronous-stream worker may still execute its code is unsafe.
            if let Some(calls) = self.calls.take() {
                std::mem::forget(calls);
            }
        }
    }
}

impl<C: SessionCalls> LoaderCore<C> {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        self.require_state(LoaderState::CustomVerified, "read a profile-2 register")?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let mut value = 0;
        let status = self.calls_mut().read_register(handle, register, &mut value);
        ensure_status(
            self.calls(),
            status,
            &format!(
                "read profile-2 byte offset 0x{:02x}",
                register.byte_offset()
            ),
        )?;
        self.state = LoaderState::CustomVerified;
        Ok(value)
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        self.require_state(
            LoaderState::CustomVerified,
            "write a profile-2 shadow register",
        )?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let status = self.calls_mut().write_shadow(handle, channel, value);
        ensure_status(
            self.calls(),
            status,
            &format!(
                "write profile-2 shadow byte offset 0x{:02x}",
                channel.byte_offset()
            ),
        )?;
        self.state = LoaderState::CustomVerified;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.require_state(LoaderState::CustomVerified, "commit profile-2 control")?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let status = self.calls_mut().commit(handle);
        ensure_status(self.calls(), status, "commit profile-2 control")?;
        self.state = LoaderState::CustomVerified;
        Ok(())
    }

    fn configure_receive(&mut self, plan: ReceivePlan) -> Result<ConfiguredReceiveReadback> {
        self.require_state(LoaderState::CustomVerified, "configure profile-2 receive")?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let mut readback = ConfiguredReceiveReadback::new(plan.channel(), 0, 0, 0);
        self.calls_mut()
            .configure_receive(handle, plan, &mut readback)?;
        validate_configured_readback(plan, readback)?;
        self.state = LoaderState::ReceiveConfigured;
        Ok(readback)
    }

    fn start_receive(&mut self, plan: ReceivePlan) -> Result<ActiveReceiveReadback> {
        self.require_state(LoaderState::ReceiveConfigured, "start profile-2 receive")?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let configured = ConfiguredReceiveReadback::new(plan.channel(), 0, 0, 0);
        let mut readback = ActiveReceiveReadback::new(configured, i32::MIN);
        self.calls_mut()
            .start_receive(handle, plan, &mut readback)?;
        validate_active_readback(plan, readback)?;
        self.state = LoaderState::ReceiveActive;
        Ok(readback)
    }

    fn capture_receive(
        &mut self,
        plan: ReceivePlan,
        output: &mut dyn Write,
    ) -> Result<CaptureReport> {
        self.require_state(LoaderState::ReceiveActive, "capture profile-2 receive data")?;
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        let report = self.calls_mut().capture_receive(handle, plan, output)?;
        validate_capture_report(plan, &report)?;
        self.state = LoaderState::CaptureComplete;
        Ok(report)
    }

    fn stop_receive(&mut self, channel: ReceiveChannel) -> Result<()> {
        ensure!(
            matches!(
                self.state,
                LoaderState::ReceiveActive | LoaderState::CaptureComplete
            ),
            "cannot stop profile-2 receive while loader worker state is {:?}",
            self.state
        );
        let handle = self.handle()?;
        self.state = LoaderState::Terminal;
        self.calls_mut().stop_receive(handle, channel)?;
        self.state = LoaderState::ReceiveStopped;
        Ok(())
    }
}

fn validate_configured_readback(
    plan: ReceivePlan,
    readback: ConfiguredReceiveReadback,
) -> Result<()> {
    ensure!(
        readback.channel() == plan.channel(),
        "configured receive channel differs from the request"
    );
    ensure!(
        readback.center_frequency_hz().abs_diff(plan.center_hz()) <= MAX_FREQUENCY_ERROR_HZ,
        "configured center-frequency readback differs from the request by more than {MAX_FREQUENCY_ERROR_HZ} Hz"
    );
    ensure!(
        readback.sample_rate_hz() == SOURCE_SAMPLE_RATE_HZ,
        "configured sample-rate readback differs from required {SOURCE_SAMPLE_RATE_HZ} Hz"
    );
    ensure!(
        readback.bandwidth_hz() == SOURCE_BANDWIDTH_HZ,
        "configured bandwidth readback differs from required {SOURCE_BANDWIDTH_HZ} Hz"
    );
    Ok(())
}

fn validate_active_readback(plan: ReceivePlan, readback: ActiveReceiveReadback) -> Result<()> {
    validate_configured_readback(plan, readback.configured())?;
    ensure!(
        readback.gain_db() == plan.gain_db(),
        "active gain readback differs from requested {} dB",
        plan.gain_db()
    );
    Ok(())
}

fn validate_capture_report(plan: ReceivePlan, report: &CaptureReport) -> Result<()> {
    ensure!(
        report.samples() == plan.expected_samples(),
        "capture sample count differs from the bounded plan"
    );
    ensure!(
        report.bytes() == plan.expected_bytes(),
        "capture byte count differs from the bounded plan"
    );
    ensure!(
        report.sha256().len() == 64
            && report
                .sha256()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "capture SHA-256 is not exactly 64 lowercase hexadecimal characters"
    );
    Ok(())
}

/// Primary worker retaining both sealed image roles for one guarded epoch.
pub(crate) struct PrimaryLoaderWorker<C: LoaderCalls> {
    core: LoaderCore<C>,
    images: StagedImages,
    receive_plan: Option<ReceivePlan>,
}

impl<C: LoaderCalls> PrimaryLoaderWorker<C> {
    pub(crate) fn open(calls: C, images: StagedImages, serial: &str) -> Result<Self> {
        let core = LoaderCore::open(calls, serial)?;
        Ok(Self {
            core,
            images,
            receive_plan: None,
        })
    }

    pub(crate) fn load_profile2(&mut self) -> Result<()> {
        self.core.load_custom(self.images.custom())
    }

    pub(crate) fn verify_profile2(&mut self) -> Result<()> {
        self.core.verify_custom()
    }

    pub(crate) fn restore_stock(&mut self) -> Result<()> {
        self.core.load_stock_after_custom(self.images.stock())?;
        self.receive_plan = None;
        Ok(())
    }

    pub(crate) fn verify_stock(&mut self) -> Result<()> {
        self.core.verify_stock()
    }

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.core.shutdown()
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.core.is_terminal()
    }
}

impl<C: SessionCalls> PrimaryLoaderWorker<C> {
    pub(crate) fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        self.core.read_register(register)
    }

    pub(crate) fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        self.core.write_shadow(channel, value)
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        self.core.commit()
    }

    pub(crate) fn configure_receive(
        &mut self,
        plan: ReceivePlan,
    ) -> Result<ConfiguredReceiveReadback> {
        ensure!(
            self.receive_plan.is_none(),
            "profile-2 worker already retains a receive plan"
        );
        let readback = self.core.configure_receive(plan)?;
        self.receive_plan = Some(plan);
        Ok(readback)
    }

    pub(crate) fn start_receive(&mut self) -> Result<ActiveReceiveReadback> {
        let plan = self
            .receive_plan
            .context("profile-2 worker lacks a configured receive plan")?;
        self.core.start_receive(plan)
    }

    pub(crate) fn capture_receive(&mut self, output: &mut dyn Write) -> Result<CaptureReport> {
        let plan = self
            .receive_plan
            .context("profile-2 worker lacks a configured receive plan")?;
        self.core.capture_receive(plan, output)
    }

    pub(crate) fn stop_receive(&mut self) -> Result<()> {
        let plan = self
            .receive_plan
            .context("profile-2 worker lacks a configured receive plan")?;
        self.core.stop_receive(plan.channel())
    }
}

/// Fresh recovery worker retaining only its independently staged stock image.
pub(crate) struct RecoveryLoaderWorker<C: LoaderCalls> {
    core: LoaderCore<C>,
    stock: SealedImage<StockImage>,
}

impl<C: LoaderCalls> RecoveryLoaderWorker<C> {
    pub(crate) fn open(calls: C, stock: SealedImage<StockImage>, serial: &str) -> Result<Self> {
        let core = LoaderCore::open(calls, serial)?;
        Ok(Self { core, stock })
    }

    pub(crate) fn restore_stock(&mut self) -> Result<()> {
        self.core.load_stock_for_recovery(&self.stock)
    }

    pub(crate) fn verify_stock(&mut self) -> Result<()> {
        self.core.verify_stock()
    }

    pub(crate) fn shutdown(&mut self) -> Result<()> {
        self.core.shutdown()
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.core.is_terminal()
    }
}

pub(crate) fn validate_requested_serial(serial: &str) -> Result<()> {
    ensure!(
        serial.len() == 32
            && serial
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "bladeRF serial must be exactly 32 lowercase hexadecimal characters"
    );
    Ok(())
}

fn validate_returned_serial(raw: &[u8; LOADER_SERIAL_BYTES], expected: &str) -> Result<()> {
    ensure!(
        raw[LOADER_SERIAL_BYTES - 1] == 0,
        "returned bladeRF serial lacks its required terminator"
    );
    let serial = &raw[..LOADER_SERIAL_BYTES - 1];
    ensure!(
        serial
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
        "returned bladeRF serial is not exactly 32 lowercase hexadecimal characters"
    );
    ensure!(
        serial == expected.as_bytes(),
        "opened bladeRF serial does not exactly match the requested serial"
    );
    Ok(())
}

fn ensure_status<C: LoaderCalls>(calls: &C, status: c_int, operation: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(native_error(calls, status, operation))
    }
}

fn native_error<C: LoaderCalls>(calls: &C, status: c_int, operation: &str) -> anyhow::Error {
    let detail = calls
        .error_text(status)
        .unwrap_or_else(|| "no diagnostic text".to_owned());
    anyhow::anyhow!("{operation} failed with libbladeRF status {status}: {detail}")
}

pub(crate) const MODEL_SERIAL: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelEvent {
    Open {
        handle: u32,
        identifier: Vec<u8>,
    },
    Serial(u32),
    Board(u32),
    Size(u32),
    LoadCustom {
        handle: u32,
        path: Vec<u8>,
        bytes: u64,
        sha256: String,
    },
    LoadStock {
        handle: u32,
        path: Vec<u8>,
        bytes: u64,
        sha256: String,
    },
    Configured(u32),
    Source(u32),
    Version(u32),
    Read {
        handle: u32,
        address: u32,
    },
    RegisterRead {
        handle: u32,
        register: ReadRegister,
    },
    ShadowWrite {
        handle: u32,
        channel: ShadowChannel,
        value: u32,
    },
    Commit {
        handle: u32,
        address: u32,
        value: u32,
    },
    Configure {
        handle: u32,
        plan: ReceivePlan,
    },
    Start {
        handle: u32,
        plan: ReceivePlan,
    },
    Capture {
        handle: u32,
        plan: ReceivePlan,
    },
    Stop {
        handle: u32,
        channel: ReceiveChannel,
    },
    Close(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelSessionOperation {
    RegisterRead,
    ShadowWrite,
    Commit,
    Configure,
    Start,
    Capture,
    Stop,
}

#[derive(Clone, Debug, Default)]
struct ModelRegisters {
    shadow: [u32; 4],
    written_mask: u8,
    committed: bool,
    applied_epoch: u32,
    outcome_epoch: u32,
    applied: [u32; 4],
}

impl ModelRegisters {
    fn read(&self, register: ReadRegister) -> u32 {
        match register {
            ReadRegister::Id => PROFILE2_ID,
            ReadRegister::Capabilities => PROFILE2_CAPABILITIES,
            ReadRegister::Shadow0 => self.shadow[0],
            ReadRegister::Shadow1 => self.shadow[1],
            ReadRegister::Shadow2 => self.shadow[2],
            ReadRegister::Shadow3 => self.shadow[3],
            ReadRegister::Status if self.committed => 0x6000_0000,
            ReadRegister::Status => u32::from(self.written_mask) << 20,
            ReadRegister::AppliedEpoch => self.applied_epoch,
            ReadRegister::OutcomeEpoch => self.outcome_epoch,
            ReadRegister::Applied0 => self.applied[0],
            ReadRegister::Applied1 => self.applied[1],
            ReadRegister::Applied2 => self.applied[2],
            ReadRegister::Applied3 => self.applied[3],
        }
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) {
        self.shadow[channel.index()] = value;
        self.written_mask |= 1 << channel.index();
    }

    fn commit(&mut self) -> Result<()> {
        ensure!(
            self.written_mask == 0x0f,
            "model commit requires all four fresh shadow words"
        );
        self.committed = true;
        self.applied_epoch = self
            .applied_epoch
            .checked_add(1)
            .context("model applied epoch overflow")?;
        self.outcome_epoch = self.applied_epoch;
        self.applied = self.shadow;
        self.written_mask = 0;
        Ok(())
    }
}

fn model_wire_flags(channel: ReceiveChannel, epoch: u32, config: bool) -> u16 {
    PROFILE2_WIRE_VERSION_FLAGS
        | if channel == ReceiveChannel::Rx2 {
            PROFILE2_WIRE_RX2_FLAG
        } else {
            0
        }
        | if config { PROFILE2_WIRE_CONFIG_FLAG } else { 0 }
        | u16::try_from(epoch & 0x0f).expect("four-bit epoch tag fits u16")
}

fn model_wire_header(record: &mut [u8; PROFILE2_RECORD_BYTES], flags: u16) {
    record[0..2].copy_from_slice(&PROFILE2_WIRE_MAGIC_LOW.to_le_bytes());
    record[2..4].copy_from_slice(&PROFILE2_WIRE_MAGIC_HIGH.to_le_bytes());
    record[4..6].copy_from_slice(&flags.to_le_bytes());
    record[6..8].copy_from_slice(&PROFILE2_WIRE_PROFILE_ID.to_le_bytes());
}

fn model_config_record(
    plan: ReceivePlan,
    registers: &ModelRegisters,
) -> Result<[u8; PROFILE2_RECORD_BYTES]> {
    ensure!(
        registers.applied_epoch != 0,
        "model capture requires one applied profile-2 configuration epoch"
    );
    let mut record = [0u8; PROFILE2_RECORD_BYTES];
    model_wire_header(
        &mut record,
        model_wire_flags(plan.channel(), registers.applied_epoch, true),
    );
    record[8..12].copy_from_slice(&registers.applied_epoch.to_le_bytes());
    record[12..16].copy_from_slice(&SOURCE_SAMPLE_RATE_HZ.to_le_bytes());
    for (channel, word) in registers.applied.iter().enumerate() {
        let offset = 16 + channel * 4;
        record[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    Ok(record)
}

fn model_data_record(
    plan: ReceivePlan,
    epoch: u32,
    data_record: u64,
) -> Result<[u8; PROFILE2_RECORD_BYTES]> {
    let source_index = data_record
        .checked_mul(PROFILE2_SOURCE_INDEX_STEP)
        .and_then(|offset| offset.checked_add(PROFILE2_FIRST_SOURCE_INDEX))
        .context("model profile-2 source index overflow")?;
    let mut record = [0u8; PROFILE2_RECORD_BYTES];
    model_wire_header(&mut record, model_wire_flags(plan.channel(), epoch, false));
    record[8..16].copy_from_slice(&source_index.to_le_bytes());
    // The four complex signed-i16 payload samples remain deterministic zeros.
    Ok(record)
}

fn write_model_capture(
    output: &mut dyn Write,
    plan: ReceivePlan,
    registers: &ModelRegisters,
) -> Result<String> {
    const RECORDS_PER_CHUNK: usize = MODEL_CAPTURE_CHUNK_BYTES / PROFILE2_RECORD_BYTES;
    const _: () = assert!(MODEL_CAPTURE_CHUNK_BYTES.is_multiple_of(PROFILE2_RECORD_BYTES));

    let config = model_config_record(plan, registers)?;
    let total_records = plan
        .data_records()
        .checked_add(1)
        .context("model profile-2 CONFIG plus DATA record count overflow")?;
    let mut buffer = [0u8; MODEL_CAPTURE_CHUNK_BYTES];
    let mut next_record = 0u64;
    let mut observed_bytes = 0u64;
    let mut hasher = Sha256::new();

    while next_record < total_records {
        let remaining_records = total_records - next_record;
        let chunk_records = usize::try_from(remaining_records.min(RECORDS_PER_CHUNK as u64))
            .context("model profile-2 chunk record count exceeds usize")?;
        for chunk_record in 0..chunk_records {
            let record_number = next_record + chunk_record as u64;
            let record = if record_number == 0 {
                config
            } else {
                model_data_record(plan, registers.applied_epoch, record_number - 1)?
            };
            let offset = chunk_record * PROFILE2_RECORD_BYTES;
            buffer[offset..offset + PROFILE2_RECORD_BYTES].copy_from_slice(&record);
        }

        let chunk_bytes = chunk_records * PROFILE2_RECORD_BYTES;
        output
            .write_all(&buffer[..chunk_bytes])
            .context("write model profile-2 capture")?;
        hasher.update(&buffer[..chunk_bytes]);
        observed_bytes = observed_bytes
            .checked_add(chunk_bytes as u64)
            .context("model profile-2 byte count overflow")?;
        next_record += chunk_records as u64;
    }

    ensure!(
        observed_bytes == plan.expected_bytes(),
        "model profile-2 output extent differs from the bounded plan"
    );
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Clone)]
struct ModelBehavior {
    open_status: c_int,
    return_handle: bool,
    serial_status: c_int,
    serial: String,
    board_present: bool,
    board_name: Vec<u8>,
    size_status: c_int,
    fpga_size: c_int,
    custom_status: c_int,
    panic_custom_load: bool,
    stock_status: c_int,
    panic_stock_load: bool,
    panic_identity: bool,
    configured: c_int,
    source_status: c_int,
    source: c_int,
    version_status: c_int,
    version: LoaderFpgaVersion,
    id_status: c_int,
    profile_id: u32,
    capabilities_status: c_int,
    capabilities: u32,
    session_failure: Option<ModelSessionOperation>,
    panic_session: Option<ModelSessionOperation>,
    configured_readback_override: Option<ConfiguredReceiveReadback>,
    active_readback_override: Option<ActiveReceiveReadback>,
    capture_report_override: Option<CaptureReport>,
}

impl Default for ModelBehavior {
    fn default() -> Self {
        Self {
            open_status: 0,
            return_handle: true,
            serial_status: 0,
            serial: MODEL_SERIAL.to_owned(),
            board_present: true,
            board_name: b"bladerf2".to_vec(),
            size_status: 0,
            fpga_size: LOADER_FPGA_A9,
            custom_status: 0,
            panic_custom_load: false,
            stock_status: 0,
            panic_stock_load: false,
            panic_identity: false,
            configured: 1,
            source_status: 0,
            source: LOADER_FPGA_SOURCE_HOST,
            version_status: 0,
            version: LoaderFpgaVersion::PROFILE2_REQUIRED,
            id_status: 0,
            profile_id: PROFILE2_ID,
            capabilities_status: 0,
            capabilities: PROFILE2_CAPABILITIES,
            session_failure: None,
            panic_session: None,
            configured_readback_override: None,
            active_readback_override: None,
            capture_report_override: None,
        }
    }
}

pub(crate) struct ModelCalls {
    handle: u32,
    trace: Rc<RefCell<Vec<ModelEvent>>>,
    behavior: ModelBehavior,
    registers: ModelRegisters,
    snapshot_error: Option<String>,
    drop_count: Option<Rc<Cell<u32>>>,
}

impl ModelCalls {
    /// Construct the deterministic, successful call table used by a fixture
    /// process. Its trace remains private to this module so callers cannot
    /// alter the fixed device-free behavior.
    pub(crate) fn fixture(handle: u32) -> Self {
        Self::new(handle, Rc::new(RefCell::new(Vec::new())))
    }

    fn new(handle: u32, trace: Rc<RefCell<Vec<ModelEvent>>>) -> Self {
        Self {
            handle,
            trace,
            behavior: ModelBehavior::default(),
            registers: ModelRegisters::default(),
            snapshot_error: None,
            drop_count: None,
        }
    }

    fn snapshot(path: &CStr) -> Result<(u64, String)> {
        let path = std::str::from_utf8(path.to_bytes()).context("decode model image path")?;
        let mut image = File::open(path).context("open sealed model image")?;
        let mut buffer = [0u8; 64 * 1024];
        let mut bytes = 0u64;
        let mut hasher = Sha256::new();
        loop {
            let count = image.read(&mut buffer).context("read sealed model image")?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(count as u64)
                .context("model image byte count overflow")?;
            hasher.update(&buffer[..count]);
        }
        Ok((bytes, format!("{:x}", hasher.finalize())))
    }

    fn enter_session_operation(&self, operation: ModelSessionOperation) -> Result<()> {
        assert!(
            self.behavior.panic_session != Some(operation),
            "injected {operation:?} callback panic"
        );
        if self.behavior.session_failure == Some(operation) {
            bail!("injected {operation:?} callback failure");
        }
        Ok(())
    }
}

impl Drop for ModelCalls {
    fn drop(&mut self) {
        if let Some(count) = &self.drop_count {
            count.set(count.get() + 1);
        }
    }
}

impl LoaderCalls for ModelCalls {
    type Handle = u32;

    fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>) {
        self.trace.borrow_mut().push(ModelEvent::Open {
            handle: self.handle,
            identifier: identifier.to_bytes().to_vec(),
        });
        (
            self.behavior.open_status,
            self.behavior.return_handle.then_some(self.handle),
        )
    }

    fn close(&mut self, handle: Self::Handle) {
        self.trace.borrow_mut().push(ModelEvent::Close(handle));
    }

    fn error_text(&self, status: c_int) -> Option<String> {
        self.snapshot_error
            .clone()
            .or_else(|| Some(format!("model status {status}")))
    }

    fn get_serial(
        &mut self,
        handle: Self::Handle,
        serial: &mut [u8; LOADER_SERIAL_BYTES],
    ) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Serial(handle));
        if self.behavior.serial_status == 0 {
            serial[..32].copy_from_slice(self.behavior.serial.as_bytes());
            serial[32] = 0;
        }
        self.behavior.serial_status
    }

    fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>> {
        self.trace.borrow_mut().push(ModelEvent::Board(handle));
        self.behavior
            .board_present
            .then(|| self.behavior.board_name.clone())
    }

    fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Size(handle));
        if self.behavior.size_status == 0 {
            *size = self.behavior.fpga_size;
        }
        self.behavior.size_status
    }

    fn load_custom(&mut self, handle: Self::Handle, image: &SealedImage<CustomImage>) -> c_int {
        assert!(
            !self.behavior.panic_custom_load,
            "injected custom-load callback panic"
        );
        let path = image.custom_load_path();
        let (bytes, sha256) = match Self::snapshot(path) {
            Ok(facts) => facts,
            Err(error) => {
                self.snapshot_error = Some(format!("read staged custom image: {error:#}"));
                return -1;
            }
        };
        self.trace.borrow_mut().push(ModelEvent::LoadCustom {
            handle,
            path: path.to_bytes().to_vec(),
            bytes,
            sha256,
        });
        self.behavior.custom_status
    }

    fn load_stock(&mut self, handle: Self::Handle, image: &SealedImage<StockImage>) -> c_int {
        assert!(
            !self.behavior.panic_stock_load,
            "injected stock-load callback panic"
        );
        let path = image.stock_load_path();
        let (bytes, sha256) = match Self::snapshot(path) {
            Ok(facts) => facts,
            Err(error) => {
                self.snapshot_error = Some(format!("read staged stock image: {error:#}"));
                return -1;
            }
        };
        self.trace.borrow_mut().push(ModelEvent::LoadStock {
            handle,
            path: path.to_bytes().to_vec(),
            bytes,
            sha256,
        });
        self.behavior.stock_status
    }

    fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int {
        assert!(
            !self.behavior.panic_identity,
            "injected identity callback panic"
        );
        self.trace.borrow_mut().push(ModelEvent::Configured(handle));
        self.behavior.configured
    }

    fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Source(handle));
        if self.behavior.source_status == 0 {
            *source = self.behavior.source;
        }
        self.behavior.source_status
    }

    fn fpga_version(&mut self, handle: Self::Handle, version: &mut LoaderFpgaVersion) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Version(handle));
        if self.behavior.version_status == 0 {
            *version = self.behavior.version;
        }
        self.behavior.version_status
    }

    fn read_profile_id(&mut self, handle: Self::Handle, value: &mut u32) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Read {
            handle,
            address: 0x00,
        });
        if self.behavior.id_status == 0 {
            *value = self.behavior.profile_id;
        }
        self.behavior.id_status
    }

    fn read_profile_capabilities(&mut self, handle: Self::Handle, value: &mut u32) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Read {
            handle,
            address: 0x04,
        });
        if self.behavior.capabilities_status == 0 {
            *value = self.behavior.capabilities;
        }
        self.behavior.capabilities_status
    }
}

impl SessionCalls for ModelCalls {
    fn read_register(
        &mut self,
        handle: Self::Handle,
        register: ReadRegister,
        value: &mut u32,
    ) -> c_int {
        self.trace
            .borrow_mut()
            .push(ModelEvent::RegisterRead { handle, register });
        if let Err(error) = self.enter_session_operation(ModelSessionOperation::RegisterRead) {
            self.snapshot_error = Some(error.to_string());
            return -1;
        }
        *value = self.registers.read(register);
        0
    }

    fn write_shadow(&mut self, handle: Self::Handle, channel: ShadowChannel, value: u32) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::ShadowWrite {
            handle,
            channel,
            value,
        });
        if let Err(error) = self.enter_session_operation(ModelSessionOperation::ShadowWrite) {
            self.snapshot_error = Some(error.to_string());
            return -1;
        }
        self.registers.write_shadow(channel, value);
        0
    }

    fn commit(&mut self, handle: Self::Handle) -> c_int {
        self.trace.borrow_mut().push(ModelEvent::Commit {
            handle,
            address: COMMIT_BYTE_OFFSET,
            value: COMMIT_MAGIC,
        });
        if let Err(error) = self.enter_session_operation(ModelSessionOperation::Commit) {
            self.snapshot_error = Some(error.to_string());
            return -1;
        }
        if let Err(error) = self.registers.commit() {
            self.snapshot_error = Some(error.to_string());
            return -1;
        }
        0
    }

    fn configure_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ConfiguredReceiveReadback,
    ) -> Result<()> {
        self.trace
            .borrow_mut()
            .push(ModelEvent::Configure { handle, plan });
        self.enter_session_operation(ModelSessionOperation::Configure)?;
        *readback = self
            .behavior
            .configured_readback_override
            .unwrap_or_else(|| {
                ConfiguredReceiveReadback::new(
                    plan.channel(),
                    plan.center_hz(),
                    SOURCE_SAMPLE_RATE_HZ,
                    SOURCE_BANDWIDTH_HZ,
                )
            });
        Ok(())
    }

    fn start_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ActiveReceiveReadback,
    ) -> Result<()> {
        self.trace
            .borrow_mut()
            .push(ModelEvent::Start { handle, plan });
        self.enter_session_operation(ModelSessionOperation::Start)?;
        *readback = self.behavior.active_readback_override.unwrap_or_else(|| {
            ActiveReceiveReadback::new(
                ConfiguredReceiveReadback::new(
                    plan.channel(),
                    plan.center_hz(),
                    SOURCE_SAMPLE_RATE_HZ,
                    SOURCE_BANDWIDTH_HZ,
                ),
                plan.gain_db(),
            )
        });
        Ok(())
    }

    fn capture_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        output: &mut dyn Write,
    ) -> Result<CaptureReport> {
        self.trace
            .borrow_mut()
            .push(ModelEvent::Capture { handle, plan });
        self.enter_session_operation(ModelSessionOperation::Capture)?;
        let observed_sha256 = write_model_capture(output, plan, &self.registers)?;
        Ok(self
            .behavior
            .capture_report_override
            .clone()
            .unwrap_or_else(|| {
                CaptureReport::new(
                    plan.expected_samples(),
                    plan.expected_bytes(),
                    observed_sha256,
                )
            }))
    }

    fn stop_receive(&mut self, handle: Self::Handle, channel: ReceiveChannel) -> Result<()> {
        self.trace
            .borrow_mut()
            .push(ModelEvent::Stop { handle, channel });
        self.enter_session_operation(ModelSessionOperation::Stop)
    }
}

/// Exercise exact image roles and fresh recovery without a native device call.
pub(crate) fn verify_loader_worker(
    custom: &ExpectedImage<CustomImage>,
    stock: &ExpectedImage<StockImage>,
) -> Result<ImageStagingReport> {
    let happy_images = StagedImages::stage(custom, stock)?;
    let report = happy_images.report();
    let happy_trace = Rc::new(RefCell::new(Vec::new()));
    let mut happy = PrimaryLoaderWorker::open(
        ModelCalls::new(11, Rc::clone(&happy_trace)),
        happy_images,
        MODEL_SERIAL,
    )?;
    happy.load_profile2()?;
    happy.verify_profile2()?;
    happy.restore_stock()?;
    happy.verify_stock()?;
    happy.shutdown()?;
    let (
        observed_custom_sha256,
        observed_custom_bytes,
        observed_stock_sha256,
        observed_stock_bytes,
    ) = image_facts(&happy_trace.borrow())?;
    ensure!(
        observed_custom_sha256 == report.custom_sha256()
            && observed_custom_bytes == report.custom_bytes(),
        "loader model did not observe the exact sealed custom image"
    );
    ensure!(
        observed_stock_sha256 == report.stock_sha256()
            && observed_stock_bytes == report.stock_bytes(),
        "loader model did not observe the exact sealed stock image"
    );
    ensure_happy_order(&happy_trace.borrow(), 11)?;

    let failed_images = StagedImages::stage(custom, stock)?;
    let failed_trace = Rc::new(RefCell::new(Vec::new()));
    let mut failed_calls = ModelCalls::new(22, Rc::clone(&failed_trace));
    failed_calls.behavior.custom_status = -7;
    let mut failed = PrimaryLoaderWorker::open(failed_calls, failed_images, MODEL_SERIAL)?;
    let failed_stock_path = failed.images.stock().stock_load_path().to_bytes().to_vec();
    ensure!(
        failed.load_profile2().is_err(),
        "release model custom load unexpectedly succeeded"
    );
    ensure!(
        failed.is_terminal(),
        "ambiguous custom load was not terminal"
    );
    let terminal_events = failed_trace.borrow().len();
    ensure!(
        failed.restore_stock().is_err(),
        "terminal worker accepted stock restoration"
    );
    ensure!(
        failed_trace.borrow().len() == terminal_events,
        "terminal worker issued a later native call"
    );

    let recovery_stock = stock.stage()?;
    let recovery_stock_path = recovery_stock.stock_load_path().to_bytes().to_vec();
    ensure!(
        recovery_stock_path != failed_stock_path,
        "fresh recovery reused the failed primary stock descriptor"
    );
    let recovery_trace = Rc::new(RefCell::new(Vec::new()));
    let mut recovery = RecoveryLoaderWorker::open(
        ModelCalls::new(33, Rc::clone(&recovery_trace)),
        recovery_stock,
        MODEL_SERIAL,
    )?;
    recovery.restore_stock()?;
    recovery.verify_stock()?;
    ensure!(
        !recovery.is_terminal(),
        "verified recovery worker unexpectedly became terminal"
    );
    recovery.shutdown()?;
    drop(recovery);
    drop(failed);
    ensure_ambiguous_order(&failed_trace.borrow(), 22)?;
    ensure_recovery_order(&recovery_trace.borrow(), 33)?;
    let (_, _, recovery_stock_sha256, recovery_stock_bytes) =
        image_facts(&recovery_trace.borrow())?;
    ensure!(
        recovery_stock_sha256 == observed_stock_sha256
            && recovery_stock_bytes == observed_stock_bytes,
        "independently staged recovery stock differs from primary stock"
    );

    Ok(report)
}

/// Exercise one complete image/control/receive/restore epoch with model calls.
pub(crate) fn verify_session_worker(
    custom: &ExpectedImage<CustomImage>,
    stock: &ExpectedImage<StockImage>,
) -> Result<(ImageStagingReport, CaptureReport)> {
    const WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
    const CHANNELS: [ShadowChannel; 4] = [
        ShadowChannel::Channel0,
        ShadowChannel::Channel1,
        ShadowChannel::Channel2,
        ShadowChannel::Channel3,
    ];
    const SHADOW_READS: [ReadRegister; 4] = [
        ReadRegister::Shadow0,
        ReadRegister::Shadow1,
        ReadRegister::Shadow2,
        ReadRegister::Shadow3,
    ];
    const APPLIED_READS: [ReadRegister; 4] = [
        ReadRegister::Applied0,
        ReadRegister::Applied1,
        ReadRegister::Applied2,
        ReadRegister::Applied3,
    ];

    let images = StagedImages::stage(custom, stock)?;
    let image_report = images.report();
    let trace = Rc::new(RefCell::new(Vec::new()));
    let mut worker =
        PrimaryLoaderWorker::open(ModelCalls::new(44, Rc::clone(&trace)), images, MODEL_SERIAL)?;
    worker.load_profile2()?;
    worker.verify_profile2()?;

    ensure!(
        worker.read_register(ReadRegister::Id)? == PROFILE2_ID,
        "session model profile ID differs"
    );
    ensure!(
        worker.read_register(ReadRegister::Capabilities)? == PROFILE2_CAPABILITIES,
        "session model capabilities differ"
    );
    ensure!(
        worker.read_register(ReadRegister::Status)? == 0,
        "session model initial status differs"
    );
    ensure!(
        worker.read_register(ReadRegister::AppliedEpoch)? == 0,
        "session model initial epoch differs"
    );
    for ((channel, register), word) in CHANNELS.into_iter().zip(SHADOW_READS).zip(WORDS) {
        worker.write_shadow(channel, word)?;
        ensure!(
            worker.read_register(register)? == word,
            "session model shadow readback differs"
        );
    }
    ensure!(
        worker.read_register(ReadRegister::Status)? == 0x00f0_0000,
        "session model staged status differs"
    );
    worker.commit()?;
    ensure!(
        worker.read_register(ReadRegister::Status)? == 0x6000_0000,
        "session model terminal outcome differs"
    );
    ensure!(
        worker.read_register(ReadRegister::OutcomeEpoch)? == 1
            && worker.read_register(ReadRegister::AppliedEpoch)? == 1,
        "session model applied epoch differs"
    );
    for (register, word) in APPLIED_READS.into_iter().zip(WORDS) {
        ensure!(
            worker.read_register(register)? == word,
            "session model applied readback differs"
        );
    }

    let alternate_plan = ReceivePlan::new(ReceiveChannel::Rx1, 98_100_000, 40, MIN_DATA_RECORDS)?;
    ensure!(
        alternate_plan.data_records() == MIN_DATA_RECORDS
            && alternate_plan.expected_samples() == 800_008
            && alternate_plan.expected_bytes() == 3_200_032,
        "RX1 bounded capture plan drifted"
    );
    let plan = ReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MIN_DATA_RECORDS)?;
    let configured = worker.configure_receive(plan)?;
    validate_configured_readback(plan, configured)?;
    let active = worker.start_receive()?;
    validate_active_readback(plan, active)?;
    struct CaptureEvidenceWriter {
        bytes: u64,
        hasher: Sha256,
    }

    impl Write for CaptureEvidenceWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes += buffer.len() as u64;
            self.hasher.update(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut evidence = CaptureEvidenceWriter {
        bytes: 0,
        hasher: Sha256::new(),
    };
    let capture = worker.capture_receive(&mut evidence)?;
    validate_capture_report(plan, &capture)?;
    let evidence_sha256 = format!("{:x}", evidence.hasher.finalize());
    ensure!(
        evidence.bytes == capture.bytes() && evidence_sha256 == capture.sha256(),
        "session model capture report differs from independently observed output"
    );
    worker.stop_receive()?;
    worker.restore_stock()?;
    worker.verify_stock()?;
    worker.shutdown()?;

    ensure_session_order(&trace.borrow(), 44)?;
    let (custom_sha256, custom_bytes, stock_sha256, stock_bytes) = image_facts(&trace.borrow())?;
    ensure!(
        custom_sha256 == image_report.custom_sha256()
            && custom_bytes == image_report.custom_bytes()
            && stock_sha256 == image_report.stock_sha256()
            && stock_bytes == image_report.stock_bytes(),
        "session model did not observe the exact sealed image bundle"
    );
    Ok((image_report, capture))
}

fn image_facts(trace: &[ModelEvent]) -> Result<(String, u64, String, u64)> {
    let custom = trace.iter().find_map(|event| match event {
        ModelEvent::LoadCustom { sha256, bytes, .. } => Some((sha256.clone(), *bytes)),
        _ => None,
    });
    let stock = trace.iter().find_map(|event| match event {
        ModelEvent::LoadStock { sha256, bytes, .. } => Some((sha256.clone(), *bytes)),
        _ => None,
    });
    let (custom_sha256, custom_bytes) = custom.unwrap_or_default();
    let (stock_sha256, stock_bytes) = stock.context("loader model did not dispatch stock")?;
    Ok((custom_sha256, custom_bytes, stock_sha256, stock_bytes))
}

fn event_kind(event: &ModelEvent) -> &'static str {
    match event {
        ModelEvent::Open { .. } => "open",
        ModelEvent::Serial(_) => "serial",
        ModelEvent::Board(_) => "board",
        ModelEvent::Size(_) => "size",
        ModelEvent::LoadCustom { .. } => "load-custom",
        ModelEvent::LoadStock { .. } => "load-stock",
        ModelEvent::Configured(_) => "configured",
        ModelEvent::Source(_) => "source",
        ModelEvent::Version(_) => "version",
        ModelEvent::Read { address: 0x00, .. } => "id",
        ModelEvent::Read { address: 0x04, .. } => "capabilities",
        ModelEvent::Read { .. } => "unexpected-read",
        ModelEvent::RegisterRead { .. } => "register-read",
        ModelEvent::ShadowWrite { .. } => "shadow-write",
        ModelEvent::Commit { .. } => "commit",
        ModelEvent::Configure { .. } => "configure-receive",
        ModelEvent::Start { .. } => "start-receive",
        ModelEvent::Capture { .. } => "capture-receive",
        ModelEvent::Stop { .. } => "stop-receive",
        ModelEvent::Close(_) => "close",
    }
}

fn ensure_order(trace: &[ModelEvent], expected: &[&str], label: &str) -> Result<()> {
    let actual: Vec<_> = trace.iter().map(event_kind).collect();
    ensure!(actual == expected, "{label} trace differs: {actual:?}");
    Ok(())
}

fn ensure_happy_order(trace: &[ModelEvent], handle: u32) -> Result<()> {
    ensure_trace_handle(trace, handle)?;
    ensure_order(
        trace,
        &[
            "open",
            "serial",
            "board",
            "size",
            "load-custom",
            "configured",
            "source",
            "version",
            "id",
            "capabilities",
            "load-stock",
            "configured",
            "source",
            "version",
            "close",
        ],
        "happy loader",
    )
}

fn ensure_ambiguous_order(trace: &[ModelEvent], handle: u32) -> Result<()> {
    ensure_trace_handle(trace, handle)?;
    ensure_order(
        trace,
        &["open", "serial", "board", "size", "load-custom"],
        "ambiguous loader",
    )
}

fn ensure_recovery_order(trace: &[ModelEvent], handle: u32) -> Result<()> {
    ensure_trace_handle(trace, handle)?;
    ensure_order(
        trace,
        &[
            "open",
            "serial",
            "board",
            "size",
            "load-stock",
            "configured",
            "source",
            "version",
            "close",
        ],
        "recovery loader",
    )
}

fn ensure_session_order(trace: &[ModelEvent], handle: u32) -> Result<()> {
    ensure_trace_handle(trace, handle)?;
    ensure_order(
        trace,
        &[
            "open",
            "serial",
            "board",
            "size",
            "load-custom",
            "configured",
            "source",
            "version",
            "id",
            "capabilities",
            "register-read",
            "register-read",
            "register-read",
            "register-read",
            "shadow-write",
            "register-read",
            "shadow-write",
            "register-read",
            "shadow-write",
            "register-read",
            "shadow-write",
            "register-read",
            "register-read",
            "commit",
            "register-read",
            "register-read",
            "register-read",
            "register-read",
            "register-read",
            "register-read",
            "register-read",
            "configure-receive",
            "start-receive",
            "capture-receive",
            "stop-receive",
            "load-stock",
            "configured",
            "source",
            "version",
            "close",
        ],
        "complete session",
    )
}

fn ensure_trace_handle(trace: &[ModelEvent], expected: u32) -> Result<()> {
    for event in trace {
        if let ModelEvent::Open { identifier, .. } = event {
            ensure!(
                identifier == format!("libusb:serial={MODEL_SERIAL}").as_bytes(),
                "loader model used an unexpected device selector"
            );
        }
        let handle = match event {
            ModelEvent::Open { handle, .. }
            | ModelEvent::LoadCustom { handle, .. }
            | ModelEvent::LoadStock { handle, .. }
            | ModelEvent::Read { handle, .. }
            | ModelEvent::RegisterRead { handle, .. }
            | ModelEvent::ShadowWrite { handle, .. }
            | ModelEvent::Configure { handle, .. }
            | ModelEvent::Start { handle, .. }
            | ModelEvent::Capture { handle, .. }
            | ModelEvent::Stop { handle, .. }
            | ModelEvent::Serial(handle)
            | ModelEvent::Board(handle)
            | ModelEvent::Size(handle)
            | ModelEvent::Configured(handle)
            | ModelEvent::Source(handle)
            | ModelEvent::Version(handle)
            | ModelEvent::Commit { handle, .. }
            | ModelEvent::Close(handle) => *handle,
        };
        ensure!(handle == expected, "loader model used an unexpected handle");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;

    use super::*;

    const REFERENCE_WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
    const REFERENCE_CHANNELS: [ShadowChannel; 4] = [
        ShadowChannel::Channel0,
        ShadowChannel::Channel1,
        ShadowChannel::Channel2,
        ShadowChannel::Channel3,
    ];

    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn expected_image<R: crate::sealed_image::ImageRole>(
        path: &Path,
        bytes: &[u8],
    ) -> ExpectedImage<R> {
        fs::write(path, bytes).unwrap();
        ExpectedImage::<R>::test_image(path, &digest(bytes), bytes.len() as u64).unwrap()
    }

    fn staged_images() -> (tempfile::TempDir, StagedImages) {
        let directory = tempfile::tempdir().unwrap();
        let custom =
            expected_image::<CustomImage>(&directory.path().join("custom.rbf"), b"custom image");
        let stock =
            expected_image::<StockImage>(&directory.path().join("stock.rbf"), b"stock image");
        let images = StagedImages::stage(&custom, &stock).unwrap();
        (directory, images)
    }

    fn staged_stock(directory: &Path, name: &str) -> SealedImage<StockImage> {
        expected_image::<StockImage>(&directory.join(name), b"stock image")
            .stage()
            .unwrap()
    }

    fn verified_session_worker(
        handle: u32,
    ) -> (
        tempfile::TempDir,
        Rc<RefCell<Vec<ModelEvent>>>,
        PrimaryLoaderWorker<ModelCalls>,
    ) {
        let (directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut worker = PrimaryLoaderWorker::open(
            ModelCalls::new(handle, Rc::clone(&trace)),
            images,
            MODEL_SERIAL,
        )
        .unwrap();
        worker.load_profile2().unwrap();
        worker.verify_profile2().unwrap();
        for (channel, word) in REFERENCE_CHANNELS.into_iter().zip(REFERENCE_WORDS) {
            worker.write_shadow(channel, word).unwrap();
        }
        worker.commit().unwrap();
        (directory, trace, worker)
    }

    fn reference_plan() -> ReceivePlan {
        ReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MIN_DATA_RECORDS).unwrap()
    }

    #[test]
    fn happy_primary_uses_exact_roles_and_closes_only_after_verified_stock() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut worker =
            PrimaryLoaderWorker::open(ModelCalls::new(1, Rc::clone(&trace)), images, MODEL_SERIAL)
                .unwrap();

        worker.load_profile2().unwrap();
        worker.verify_profile2().unwrap();
        worker.restore_stock().unwrap();
        worker.verify_stock().unwrap();
        worker.shutdown().unwrap();
        ensure_happy_order(&trace.borrow(), 1).unwrap();

        let paths: Vec<_> = trace
            .borrow()
            .iter()
            .filter_map(|event| match event {
                ModelEvent::LoadCustom { path, .. } | ModelEvent::LoadStock { path, .. } => {
                    Some(path.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
    }

    #[test]
    fn open_checks_only_serial_board_and_size_before_first_load() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut worker =
            PrimaryLoaderWorker::open(ModelCalls::new(2, Rc::clone(&trace)), images, MODEL_SERIAL)
                .unwrap();
        let before_load: Vec<_> = trace.borrow().iter().map(event_kind).collect();
        assert_eq!(before_load, ["open", "serial", "board", "size"]);
        worker.load_profile2().unwrap();
        assert_eq!(event_kind(trace.borrow().last().unwrap()), "load-custom");
        drop(worker);
        assert!(
            !trace
                .borrow()
                .iter()
                .any(|event| matches!(event, ModelEvent::Close(_)))
        );
    }

    #[test]
    fn live_handle_retains_calls_owner_until_process_teardown() {
        let (_safe_directory, safe_images) = staged_images();
        let safe_trace = Rc::new(RefCell::new(Vec::new()));
        let safe_drop_count = Rc::new(Cell::new(0));
        let mut safe_calls = ModelCalls::new(20, Rc::clone(&safe_trace));
        safe_calls.drop_count = Some(Rc::clone(&safe_drop_count));
        let safe_worker = PrimaryLoaderWorker::open(safe_calls, safe_images, MODEL_SERIAL).unwrap();
        drop(safe_worker);
        assert_eq!(safe_drop_count.get(), 1);
        assert!(matches!(
            safe_trace.borrow().last(),
            Some(ModelEvent::Close(20))
        ));

        let (_live_directory, live_images) = staged_images();
        let live_trace = Rc::new(RefCell::new(Vec::new()));
        let live_drop_count = Rc::new(Cell::new(0));
        let mut live_calls = ModelCalls::new(21, Rc::clone(&live_trace));
        live_calls.drop_count = Some(Rc::clone(&live_drop_count));
        let mut live_worker =
            PrimaryLoaderWorker::open(live_calls, live_images, MODEL_SERIAL).unwrap();
        live_worker.load_profile2().unwrap();
        live_worker.verify_profile2().unwrap();

        // A protocol abort in CustomVerified cannot call close and must not
        // unload the retained library while its live handle may still own
        // foreign state. The process kernel, rather than Rust Drop, releases
        // both at child exit.
        drop(live_worker);
        assert_eq!(live_drop_count.get(), 0);
        assert!(
            !live_trace
                .borrow()
                .iter()
                .any(|event| matches!(event, ModelEvent::Close(_)))
        );
    }

    #[test]
    fn ambiguous_custom_status_is_terminal_and_issues_no_later_call_or_close() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut calls = ModelCalls::new(3, Rc::clone(&trace));
        calls.behavior.custom_status = 9;
        let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();

        let error = worker.load_profile2().unwrap_err();
        assert!(format!("{error:#}").contains("status 9"));
        assert!(worker.is_terminal());
        let count = trace.borrow().len();
        assert!(worker.verify_profile2().is_err());
        assert!(worker.restore_stock().is_err());
        assert!(worker.shutdown().is_err());
        assert_eq!(trace.borrow().len(), count);
        drop(worker);
        ensure_ambiguous_order(&trace.borrow(), 3).unwrap();
    }

    #[test]
    fn panicking_dispatch_and_identity_callbacks_fail_closed_without_close() {
        #[derive(Clone, Copy)]
        enum PanicStage {
            CustomLoad,
            CustomIdentity,
            StockLoad,
            StockIdentity,
        }

        for (index, stage) in [
            PanicStage::CustomLoad,
            PanicStage::CustomIdentity,
            PanicStage::StockLoad,
            PanicStage::StockIdentity,
        ]
        .into_iter()
        .enumerate()
        {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let calls = ModelCalls::new(30 + index as u32, Rc::clone(&trace));
            let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();

            if matches!(stage, PanicStage::CustomLoad) {
                worker.core.calls_mut().behavior.panic_custom_load = true;
            }
            let custom_load = catch_unwind(AssertUnwindSafe(|| worker.load_profile2()));
            if matches!(stage, PanicStage::CustomLoad) {
                assert!(custom_load.is_err());
            } else {
                custom_load.unwrap().unwrap();
                if matches!(stage, PanicStage::CustomIdentity) {
                    worker.core.calls_mut().behavior.panic_identity = true;
                }
                let custom_identity = catch_unwind(AssertUnwindSafe(|| worker.verify_profile2()));
                if matches!(stage, PanicStage::CustomIdentity) {
                    assert!(custom_identity.is_err());
                } else {
                    custom_identity.unwrap().unwrap();
                    if matches!(stage, PanicStage::StockLoad) {
                        worker.core.calls_mut().behavior.panic_stock_load = true;
                    }
                    let stock_load = catch_unwind(AssertUnwindSafe(|| worker.restore_stock()));
                    if matches!(stage, PanicStage::StockLoad) {
                        assert!(stock_load.is_err());
                    } else {
                        stock_load.unwrap().unwrap();
                        worker.core.calls_mut().behavior.panic_identity = true;
                        assert!(catch_unwind(AssertUnwindSafe(|| worker.verify_stock())).is_err());
                    }
                }
            }

            assert!(worker.is_terminal());
            let count = trace.borrow().len();
            assert!(worker.shutdown().is_err());
            drop(worker);
            assert_eq!(trace.borrow().len(), count);
            assert!(
                !trace
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Close(_)))
            );
        }
    }

    #[test]
    fn custom_identity_mismatch_is_terminal_without_close() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut calls = ModelCalls::new(4, Rc::clone(&trace));
        calls.behavior.profile_id = 0;
        let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();
        worker.load_profile2().unwrap();
        assert!(worker.verify_profile2().is_err());
        assert!(worker.is_terminal());
        let count = trace.borrow().len();
        assert!(worker.restore_stock().is_err());
        drop(worker);
        assert_eq!(trace.borrow().len(), count);
        assert!(
            !trace
                .borrow()
                .iter()
                .any(|event| matches!(event, ModelEvent::Close(_)))
        );
    }

    #[test]
    fn every_custom_identity_failure_is_terminal() {
        type Mutator = fn(&mut ModelBehavior);
        let cases: [(&str, Mutator); 10] = [
            ("configured error", |value| value.configured = -1),
            ("not configured", |value| value.configured = 0),
            ("source error", |value| value.source_status = -2),
            ("wrong source", |value| value.source = 1),
            ("version error", |value| value.version_status = -3),
            ("wrong version", |value| {
                value.version = LoaderFpgaVersion {
                    major: 0,
                    minor: 15,
                    patch: 0,
                };
            }),
            ("ID error", |value| value.id_status = -4),
            ("wrong ID", |value| value.profile_id = 0),
            ("capabilities error", |value| {
                value.capabilities_status = -5;
            }),
            ("wrong capabilities", |value| value.capabilities = 0),
        ];

        for (label, mutate) in cases {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let mut calls = ModelCalls::new(40, Rc::clone(&trace));
            mutate(&mut calls.behavior);
            let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();
            worker.load_profile2().unwrap();
            assert!(worker.verify_profile2().is_err(), "{label}");
            assert!(worker.is_terminal(), "{label}");
            let count = trace.borrow().len();
            drop(worker);
            assert_eq!(trace.borrow().len(), count, "{label}");
            assert!(
                !trace
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Close(_))),
                "{label}"
            );
        }
    }

    #[test]
    fn stock_load_and_identity_failures_are_terminal() {
        for identity_failure in [false, true] {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let calls = ModelCalls::new(5, Rc::clone(&trace));
            let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();
            worker.load_profile2().unwrap();
            worker.verify_profile2().unwrap();
            if identity_failure {
                worker.core.calls_mut().behavior.configured = 0;
            } else {
                worker.core.calls_mut().behavior.stock_status = -8;
            }
            let result = worker.restore_stock().and_then(|()| worker.verify_stock());
            assert!(result.is_err());
            assert!(worker.is_terminal());
            drop(worker);
            assert!(
                !trace
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Close(_)))
            );
        }
    }

    #[test]
    fn every_stock_identity_failure_is_terminal() {
        type Mutator = fn(&mut ModelBehavior);
        let cases: [(&str, Mutator); 6] = [
            ("configured error", |value| value.configured = -1),
            ("not configured", |value| value.configured = 0),
            ("source error", |value| value.source_status = -2),
            ("wrong source", |value| value.source = 1),
            ("version error", |value| value.version_status = -3),
            ("wrong version", |value| {
                value.version = LoaderFpgaVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                };
            }),
        ];

        for (label, mutate) in cases {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let calls = ModelCalls::new(41, Rc::clone(&trace));
            let mut worker = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();
            worker.load_profile2().unwrap();
            worker.verify_profile2().unwrap();
            mutate(&mut worker.core.calls_mut().behavior);
            worker.restore_stock().unwrap();
            assert!(worker.verify_stock().is_err(), "{label}");
            assert!(worker.is_terminal(), "{label}");
            let count = trace.borrow().len();
            drop(worker);
            assert_eq!(trace.borrow().len(), count, "{label}");
            assert!(
                !trace
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Close(_))),
                "{label}"
            );
        }
    }

    #[test]
    fn fresh_recovery_uses_an_independent_stock_descriptor_and_handle() {
        let (directory, images) = staged_images();
        let failed_trace = Rc::new(RefCell::new(Vec::new()));
        let mut calls = ModelCalls::new(6, Rc::clone(&failed_trace));
        calls.behavior.custom_status = -1;
        let mut failed = PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).unwrap();
        let primary_stock_path = failed.images.stock().stock_load_path().to_bytes().to_vec();
        assert!(failed.load_profile2().is_err());

        let stock = staged_stock(directory.path(), "fresh-stock.rbf");
        let recovery_stock_path = stock.stock_load_path().to_bytes().to_vec();
        assert_ne!(primary_stock_path, recovery_stock_path);
        let recovery_trace = Rc::new(RefCell::new(Vec::new()));
        let mut recovery = RecoveryLoaderWorker::open(
            ModelCalls::new(7, Rc::clone(&recovery_trace)),
            stock,
            MODEL_SERIAL,
        )
        .unwrap();
        recovery.restore_stock().unwrap();
        recovery.verify_stock().unwrap();
        assert!(!recovery.is_terminal());
        recovery.shutdown().unwrap();
        ensure_recovery_order(&recovery_trace.borrow(), 7).unwrap();
        drop(failed);
        ensure_ambiguous_order(&failed_trace.borrow(), 6).unwrap();
    }

    #[test]
    fn invalid_transitions_make_no_native_call() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut worker =
            PrimaryLoaderWorker::open(ModelCalls::new(8, Rc::clone(&trace)), images, MODEL_SERIAL)
                .unwrap();
        let count = trace.borrow().len();
        assert!(worker.verify_profile2().is_err());
        assert!(worker.restore_stock().is_err());
        assert_eq!(trace.borrow().len(), count);
        worker.shutdown().unwrap();
    }

    #[test]
    fn every_open_status_handle_combination_has_defined_cleanup() {
        for (status, return_handle, close_count) in [(0, false, 0), (-2, false, 0), (-2, true, 1)] {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let mut calls = ModelCalls::new(9, Rc::clone(&trace));
            calls.behavior.open_status = status;
            calls.behavior.return_handle = return_handle;
            assert!(PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).is_err());
            assert_eq!(
                trace
                    .borrow()
                    .iter()
                    .filter(|event| matches!(event, ModelEvent::Close(9)))
                    .count(),
                close_count
            );
        }
    }

    #[test]
    fn predispatch_identity_failure_closes_once_and_bad_request_never_opens() {
        type Mutator = fn(&mut ModelBehavior);
        let cases: [(&str, Mutator); 6] = [
            ("serial status", |value| value.serial_status = -1),
            ("serial mismatch", |value| value.serial = "f".repeat(32)),
            ("null board", |value| value.board_present = false),
            ("wrong board", |value| value.board_name = b"wrong".to_vec()),
            ("size status", |value| value.size_status = -2),
            ("wrong size", |value| value.fpga_size = 115),
        ];

        for (label, mutate) in cases {
            let (_directory, images) = staged_images();
            let trace = Rc::new(RefCell::new(Vec::new()));
            let mut calls = ModelCalls::new(10, Rc::clone(&trace));
            mutate(&mut calls.behavior);
            assert!(
                PrimaryLoaderWorker::open(calls, images, MODEL_SERIAL).is_err(),
                "{label}"
            );
            assert_eq!(
                trace
                    .borrow()
                    .iter()
                    .filter(|event| matches!(event, ModelEvent::Close(10)))
                    .count(),
                1,
                "{label}"
            );
            assert!(
                !trace.borrow().iter().any(|event| matches!(
                    event,
                    ModelEvent::LoadCustom { .. } | ModelEvent::LoadStock { .. }
                )),
                "{label}"
            );
        }

        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        assert!(
            PrimaryLoaderWorker::open(ModelCalls::new(11, Rc::clone(&trace)), images, "BAD")
                .is_err()
        );
        assert!(trace.borrow().is_empty());
    }

    #[test]
    fn receive_plan_freezes_bounds_and_transport_extent() {
        for channel in [ReceiveChannel::Rx1, ReceiveChannel::Rx2] {
            let minimum = ReceivePlan::new(channel, 1, MIN_GAIN_DB, MIN_DATA_RECORDS).unwrap();
            assert_eq!(minimum.data_records(), 100_000);
            assert_eq!(minimum.expected_samples(), 800_008);
            assert_eq!(minimum.expected_bytes(), 3_200_032);

            let maximum =
                ReceivePlan::new(channel, u64::MAX, MAX_GAIN_DB, MAX_DATA_RECORDS).unwrap();
            assert_eq!(maximum.expected_samples(), 80_000_008);
            assert_eq!(maximum.expected_bytes(), 320_000_032);
        }
        for (center_hz, gain_db, records) in [
            (0, 0, MIN_DATA_RECORDS),
            (1, MIN_GAIN_DB - 1, MIN_DATA_RECORDS),
            (1, MAX_GAIN_DB + 1, MIN_DATA_RECORDS),
            (1, 0, MIN_DATA_RECORDS - 1),
            (1, 0, MAX_DATA_RECORDS + 1),
        ] {
            assert!(ReceivePlan::new(ReceiveChannel::Rx1, center_hz, gain_db, records).is_err());
        }
    }

    fn wire_u16(record: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([record[offset], record[offset + 1]])
    }

    fn wire_u32(record: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(
            record[offset..offset + 4]
                .try_into()
                .expect("four-byte model field"),
        )
    }

    fn wire_u64(record: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(
            record[offset..offset + 8]
                .try_into()
                .expect("eight-byte model field"),
        )
    }

    fn assert_valid_model_stream(
        raw: &[u8],
        plan: ReceivePlan,
        expected_epoch: u32,
        expected_words: [u32; 4],
    ) {
        assert_eq!(raw.len() as u64, plan.expected_bytes());
        let (records, remainder) = raw.as_chunks::<PROFILE2_RECORD_BYTES>();
        assert!(remainder.is_empty());
        assert_eq!(records.len() as u64, plan.data_records() + 1);
        let (config_flags, data_flags) = match plan.channel() {
            ReceiveChannel::Rx1 => (0x2011, 0x2001),
            ReceiveChannel::Rx2 => (0x2031, 0x2021),
        };

        let config = &records[0];
        assert_eq!(&config[..4], &[0x46, 0x52, 0x42, 0x4f]);
        assert_eq!(wire_u16(config, 4), config_flags);
        assert_eq!(wire_u16(config, 6), 2);
        assert_eq!(wire_u32(config, 8), expected_epoch);
        assert_eq!(wire_u32(config, 12), 20_000_000);
        for (channel, expected) in expected_words.into_iter().enumerate() {
            assert_eq!(wire_u32(config, 16 + channel * 4), expected);
        }

        for (data_record, record) in records[1..].iter().enumerate() {
            assert_eq!(&record[..4], &[0x46, 0x52, 0x42, 0x4f]);
            assert_eq!(wire_u16(record, 4), data_flags);
            assert_eq!(wire_u16(record, 6), 2);
            assert_eq!(
                wire_u64(record, 8),
                data_record as u64 * PROFILE2_SOURCE_INDEX_STEP + PROFILE2_FIRST_SOURCE_INDEX
            );
            assert!(record[16..].iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn model_capture_is_valid_profile2_for_both_receive_channels() {
        for (index, (channel, expected_sha256)) in [
            (
                ReceiveChannel::Rx1,
                "58d6be06e28d7049f28cd15c60ebe2525ed49cbea97771c5247841b93176bfd0",
            ),
            (
                ReceiveChannel::Rx2,
                "284863be84327eead3b5f9c4f2873f55f93664861b3c70693df62415002fa12e",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let (_directory, _trace, mut worker) = verified_session_worker(200 + index as u32);
            let plan = ReceivePlan::new(channel, 98_100_000, 40, MIN_DATA_RECORDS).unwrap();
            worker.configure_receive(plan).unwrap();
            worker.start_receive().unwrap();
            let mut raw = Vec::with_capacity(plan.expected_bytes() as usize);
            let report = worker.capture_receive(&mut raw).unwrap();

            assert_valid_model_stream(&raw, plan, 1, REFERENCE_WORDS);
            assert_eq!(report.samples(), plan.expected_samples());
            assert_eq!(report.bytes(), raw.len() as u64);
            assert_eq!(report.sha256(), digest(&raw));
            assert_eq!(report.sha256(), expected_sha256);

            worker.stop_receive().unwrap();
            worker.restore_stock().unwrap();
            worker.verify_stock().unwrap();
            worker.shutdown().unwrap();
        }
    }

    #[test]
    fn model_wire_boundary_preserves_epoch_nibble_and_maximum_source_index() {
        let plan = ReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MAX_DATA_RECORDS).unwrap();
        let record = model_data_record(plan, 0x1234_567e, MAX_DATA_RECORDS - 1).unwrap();
        assert_eq!(wire_u16(&record, 4), 0x202e);
        assert_eq!(wire_u64(&record, 8), 199_999_999);
        assert!(model_data_record(plan, 1, u64::MAX).is_err());
    }

    #[test]
    fn complete_session_model_covers_control_capture_and_same_handle_restore() {
        let directory = tempfile::tempdir().unwrap();
        let custom = expected_image::<CustomImage>(
            &directory.path().join("custom.rbf"),
            b"custom session image",
        );
        let stock = expected_image::<StockImage>(
            &directory.path().join("stock.rbf"),
            b"stock session image",
        );
        let (images, capture) = verify_session_worker(&custom, &stock).unwrap();
        assert_eq!(images.custom_sha256(), digest(b"custom session image"));
        assert_eq!(images.stock_sha256(), digest(b"stock session image"));
        assert_eq!(capture.samples(), 800_008);
        assert_eq!(capture.bytes(), 3_200_032);
        assert_eq!(capture.sha256().len(), 64);
    }

    #[test]
    fn invalid_session_transitions_issue_no_callback() {
        let (_directory, images) = staged_images();
        let trace = Rc::new(RefCell::new(Vec::new()));
        let mut worker =
            PrimaryLoaderWorker::open(ModelCalls::new(50, Rc::clone(&trace)), images, MODEL_SERIAL)
                .unwrap();
        let count = trace.borrow().len();
        assert!(worker.read_register(ReadRegister::Id).is_err());
        assert!(worker.write_shadow(ShadowChannel::Channel0, 1).is_err());
        assert!(worker.commit().is_err());
        assert!(worker.configure_receive(reference_plan()).is_err());
        assert!(worker.start_receive().is_err());
        assert!(worker.capture_receive(&mut io::sink()).is_err());
        assert!(worker.stop_receive().is_err());
        assert_eq!(trace.borrow().len(), count);
        worker.shutdown().unwrap();
    }

    #[test]
    fn every_session_callback_error_is_terminal_and_suppresses_close() {
        for (index, operation) in [
            ModelSessionOperation::RegisterRead,
            ModelSessionOperation::ShadowWrite,
            ModelSessionOperation::Commit,
            ModelSessionOperation::Configure,
            ModelSessionOperation::Start,
            ModelSessionOperation::Capture,
            ModelSessionOperation::Stop,
        ]
        .into_iter()
        .enumerate()
        {
            let (_directory, trace, mut worker) = verified_session_worker(60 + index as u32);
            if matches!(
                operation,
                ModelSessionOperation::Start
                    | ModelSessionOperation::Capture
                    | ModelSessionOperation::Stop
            ) {
                worker.configure_receive(reference_plan()).unwrap();
            }
            if matches!(
                operation,
                ModelSessionOperation::Capture | ModelSessionOperation::Stop
            ) {
                worker.start_receive().unwrap();
            }
            if operation == ModelSessionOperation::Stop {
                worker.capture_receive(&mut io::sink()).unwrap();
            }
            worker.core.calls_mut().behavior.session_failure = Some(operation);
            let result = match operation {
                ModelSessionOperation::RegisterRead => {
                    worker.read_register(ReadRegister::Status).map(|_| ())
                }
                ModelSessionOperation::ShadowWrite => {
                    worker.write_shadow(ShadowChannel::Channel0, 1)
                }
                ModelSessionOperation::Commit => worker.commit(),
                ModelSessionOperation::Configure => {
                    worker.configure_receive(reference_plan()).map(|_| ())
                }
                ModelSessionOperation::Start => worker.start_receive().map(|_| ()),
                ModelSessionOperation::Capture => {
                    worker.capture_receive(&mut io::sink()).map(|_| ())
                }
                ModelSessionOperation::Stop => worker.stop_receive(),
            };
            assert!(result.is_err(), "{operation:?}");
            assert!(worker.is_terminal(), "{operation:?}");
            let count = trace.borrow().len();
            assert!(worker.restore_stock().is_err(), "{operation:?}");
            assert!(worker.shutdown().is_err(), "{operation:?}");
            drop(worker);
            assert_eq!(trace.borrow().len(), count, "{operation:?}");
            assert!(
                !trace
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Close(_))),
                "{operation:?}"
            );
        }
    }

    #[test]
    fn successful_but_invalid_session_outputs_are_terminal() {
        let plan = reference_plan();
        let wrong_configured = [
            ConfiguredReceiveReadback::new(
                ReceiveChannel::Rx1,
                plan.center_hz(),
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                plan.channel(),
                plan.center_hz() + MAX_FREQUENCY_ERROR_HZ + 1,
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                plan.channel(),
                plan.center_hz(),
                SOURCE_SAMPLE_RATE_HZ - 1,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                plan.channel(),
                plan.center_hz(),
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ - 1,
            ),
        ];
        for (index, readback) in wrong_configured.into_iter().enumerate() {
            let (_directory, _trace, mut worker) = verified_session_worker(90 + index as u32);
            worker
                .core
                .calls_mut()
                .behavior
                .configured_readback_override = Some(readback);
            assert!(worker.configure_receive(plan).is_err());
            assert!(worker.is_terminal());
        }

        let wrong_active = [
            ActiveReceiveReadback::new(
                ConfiguredReceiveReadback::new(
                    ReceiveChannel::Rx1,
                    plan.center_hz(),
                    SOURCE_SAMPLE_RATE_HZ,
                    SOURCE_BANDWIDTH_HZ,
                ),
                plan.gain_db(),
            ),
            ActiveReceiveReadback::new(
                ConfiguredReceiveReadback::new(
                    plan.channel(),
                    plan.center_hz(),
                    SOURCE_SAMPLE_RATE_HZ,
                    SOURCE_BANDWIDTH_HZ,
                ),
                plan.gain_db() - 1,
            ),
        ];
        for (index, readback) in wrong_active.into_iter().enumerate() {
            let (_directory, _trace, mut worker) = verified_session_worker(100 + index as u32);
            worker.configure_receive(plan).unwrap();
            worker.core.calls_mut().behavior.active_readback_override = Some(readback);
            assert!(worker.start_receive().is_err());
            assert!(worker.is_terminal());
        }

        let wrong_reports = [
            CaptureReport::new(
                plan.expected_samples() - 1,
                plan.expected_bytes(),
                "0".repeat(64),
            ),
            CaptureReport::new(
                plan.expected_samples(),
                plan.expected_bytes() - 1,
                "0".repeat(64),
            ),
            CaptureReport::new(
                plan.expected_samples(),
                plan.expected_bytes(),
                "not-a-digest",
            ),
        ];
        for (index, report) in wrong_reports.into_iter().enumerate() {
            let (_directory, _trace, mut worker) = verified_session_worker(110 + index as u32);
            worker.configure_receive(plan).unwrap();
            worker.start_receive().unwrap();
            worker.core.calls_mut().behavior.capture_report_override = Some(report);
            assert!(worker.capture_receive(&mut io::sink()).is_err());
            assert!(worker.is_terminal());
        }
    }

    #[test]
    fn panicking_session_callbacks_remain_terminal() {
        for (index, operation) in [
            ModelSessionOperation::RegisterRead,
            ModelSessionOperation::ShadowWrite,
            ModelSessionOperation::Commit,
            ModelSessionOperation::Configure,
            ModelSessionOperation::Start,
            ModelSessionOperation::Capture,
            ModelSessionOperation::Stop,
        ]
        .into_iter()
        .enumerate()
        {
            let (_directory, trace, mut worker) = verified_session_worker(70 + index as u32);
            if matches!(
                operation,
                ModelSessionOperation::Start
                    | ModelSessionOperation::Capture
                    | ModelSessionOperation::Stop
            ) {
                worker.configure_receive(reference_plan()).unwrap();
            }
            if matches!(
                operation,
                ModelSessionOperation::Capture | ModelSessionOperation::Stop
            ) {
                worker.start_receive().unwrap();
            }
            if operation == ModelSessionOperation::Stop {
                worker.capture_receive(&mut io::sink()).unwrap();
            }
            worker.core.calls_mut().behavior.panic_session = Some(operation);
            let unwind = catch_unwind(AssertUnwindSafe(|| match operation {
                ModelSessionOperation::RegisterRead => {
                    let _ = worker.read_register(ReadRegister::Status);
                }
                ModelSessionOperation::ShadowWrite => {
                    let _ = worker.write_shadow(ShadowChannel::Channel0, 1);
                }
                ModelSessionOperation::Commit => {
                    let _ = worker.commit();
                }
                ModelSessionOperation::Configure => {
                    let _ = worker.configure_receive(reference_plan());
                }
                ModelSessionOperation::Start => {
                    let _ = worker.start_receive();
                }
                ModelSessionOperation::Capture => {
                    let _ = worker.capture_receive(&mut io::sink());
                }
                ModelSessionOperation::Stop => {
                    let _ = worker.stop_receive();
                }
            }));
            assert!(unwind.is_err(), "{operation:?}");
            assert!(worker.is_terminal(), "{operation:?}");
            let count = trace.borrow().len();
            drop(worker);
            assert_eq!(trace.borrow().len(), count, "{operation:?}");
        }
    }

    #[test]
    fn capture_sink_failure_is_terminal() {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected capture sink failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (_directory, trace, mut worker) = verified_session_worker(80);
        worker.configure_receive(reference_plan()).unwrap();
        worker.start_receive().unwrap();
        assert!(worker.capture_receive(&mut FailingWriter).is_err());
        assert!(worker.is_terminal());
        let count = trace.borrow().len();
        drop(worker);
        assert_eq!(trace.borrow().len(), count);
        assert!(
            !trace
                .borrow()
                .iter()
                .any(|event| matches!(event, ModelEvent::Close(_)))
        );
    }

    #[test]
    fn same_handle_restore_accepts_only_known_stopped_session_states() {
        let (_directory, _trace, mut configured) = verified_session_worker(81);
        configured.configure_receive(reference_plan()).unwrap();
        configured.restore_stock().unwrap();
        configured.verify_stock().unwrap();
        configured.shutdown().unwrap();

        let (_directory, _trace, mut cancelled) = verified_session_worker(82);
        cancelled.configure_receive(reference_plan()).unwrap();
        cancelled.start_receive().unwrap();
        assert!(cancelled.restore_stock().is_err());
        cancelled.stop_receive().unwrap();
        cancelled.restore_stock().unwrap();
        cancelled.verify_stock().unwrap();
        cancelled.shutdown().unwrap();

        let (_directory, _trace, mut stopped) = verified_session_worker(83);
        stopped.configure_receive(reference_plan()).unwrap();
        stopped.start_receive().unwrap();
        stopped.capture_receive(&mut io::sink()).unwrap();
        assert!(stopped.restore_stock().is_err());
        stopped.stop_receive().unwrap();
        stopped.restore_stock().unwrap();
        stopped.verify_stock().unwrap();
        stopped.shutdown().unwrap();
    }
}
