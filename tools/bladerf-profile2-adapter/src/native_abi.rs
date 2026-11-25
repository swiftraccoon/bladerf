//! Hash-qualified libbladeRF ABI and one-handle profile-2 device boundary.
//!
//! This module is the only authored unsafe boundary in the adapter. It mirrors
//! the small ABI subset selected from Nuand's pinned `libbladeRF.h` and
//! `bladeRF2.h`, whose audited declaration reports API version `0x02060000`.
//! The preflight resolves required symbols and calls `bladerf_version`. When the
//! caller requires a specialized bladeRF 2 open contract, the preflight also
//! calls that patch's device-independent build-marker function.
//!
//! The register-only native adapter uses the configuration-neutral open contract
//! to select one bladeRF 2 by its full serial, validate the loaded profile and
//! expose the reviewed Wishbone operations. The guarded native session uses the
//! loader-neutral contract for image transitions, register operations, fixed RF
//! readback and synchronous RX. The device-free fixture worker models those
//! operations with deterministic calls.

#![allow(unsafe_code)]

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::marker::PhantomData;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ptr::NonNull;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::rc::Rc;

use anyhow::{Context, Result, ensure};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use bladerf_profile2_ipc::{
    COMMIT_BYTE_OFFSET, COMMIT_MAGIC, PROFILE2_CAPABILITIES, PROFILE2_ID, ReadRegister,
    ShadowChannel,
};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use libloading::Library;
#[cfg(target_os = "linux")]
use nix::fcntl::{FcntlArg, OFlag, SealFlag, fcntl, open as nix_open};
#[cfg(target_os = "linux")]
use nix::sys::memfd::{MFdFlags, memfd_create};
#[cfg(target_os = "linux")]
use nix::sys::stat::{Mode, SFlag, fstat};
use sha2::{Digest, Sha256};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::loader_worker::{
    ActiveReceiveReadback, CaptureReport, ConfiguredReceiveReadback, LOADER_SERIAL_BYTES,
    LoaderCalls, LoaderFpgaVersion, MAX_FREQUENCY_ERROR_HZ, ReceiveChannel, ReceivePlan,
    SOURCE_BANDWIDTH_HZ, SOURCE_SAMPLE_RATE_HZ, SessionCalls,
};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::sealed_image::{CustomImage, SealedImage, StockImage};

const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;
const EXPECTED_MAJOR: u16 = 2;
const EXPECTED_MINOR: u16 = 6;
const EXPECTED_PATCH: u16 = 0;
const BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1: u32 = 0x4232_4e01;
const BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1: u32 = 0x4232_4c01;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeOpenContract {
    ConfigNeutral,
    LoaderNeutral,
}

impl NativeOpenContract {
    const fn marker(self) -> u32 {
        match self {
            Self::ConfigNeutral => BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1,
            Self::LoaderNeutral => BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1,
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::ConfigNeutral => "configuration-neutral",
            Self::LoaderNeutral => "loader-neutral",
        }
    }

    const fn symbol(self) -> &'static [u8] {
        match self {
            Self::ConfigNeutral => b"bladerf_get_bladerf2_config_neutral_open_marker\0",
            Self::LoaderNeutral => b"bladerf_get_bladerf2_loader_neutral_open_marker\0",
        }
    }
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_FPGA_A9: i32 = 301;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_FPGA_SOURCE_HOST: i32 = 2;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SERIAL_BYTES: usize = 33;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_CHANNEL_RX1: c_int = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_CHANNEL_RX2: c_int = 2;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_CHANNEL_TX1: c_int = 1;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_DEVICE_SPEED_SUPER: c_int = 2;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_RX_X1: c_int = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_FORMAT_SC16_Q11: c_int = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_GAIN_MGC: c_int = 1;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_LB_NONE: c_int = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLADERF_RX_MUX_BASEBAND: c_int = 0;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SESSION_NUM_BUFFERS: u32 = 128;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SESSION_BUFFER_SAMPLES: u32 = 16_384;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SESSION_NUM_TRANSFERS: u32 = 32;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SESSION_STREAM_TIMEOUT_MS: u32 = 5_000;
const REQUIRED_SYMBOLS: [&[u8]; 12] = [
    b"bladerf_version\0",
    b"bladerf_open\0",
    b"bladerf_close\0",
    b"bladerf_strerror\0",
    b"bladerf_get_serial_struct\0",
    b"bladerf_get_board_name\0",
    b"bladerf_get_fpga_size\0",
    b"bladerf_is_fpga_configured\0",
    b"bladerf_get_fpga_source\0",
    b"bladerf_fpga_version\0",
    b"bladerf_wishbone_master_read\0",
    b"bladerf_wishbone_master_write\0",
];
const LOADER_REQUIRED_SYMBOLS: [&[u8]; 19] = [
    b"bladerf_load_fpga\0",
    b"bladerf_device_speed\0",
    b"bladerf_get_bias_tee\0",
    b"bladerf_set_frequency\0",
    b"bladerf_get_frequency\0",
    b"bladerf_set_sample_rate\0",
    b"bladerf_get_sample_rate\0",
    b"bladerf_set_bandwidth\0",
    b"bladerf_get_bandwidth\0",
    b"bladerf_set_gain_mode\0",
    b"bladerf_get_gain_mode\0",
    b"bladerf_set_gain\0",
    b"bladerf_get_gain\0",
    b"bladerf_get_loopback\0",
    b"bladerf_set_rx_mux\0",
    b"bladerf_get_rx_mux\0",
    b"bladerf_sync_config\0",
    b"bladerf_enable_module\0",
    b"bladerf_sync_rx\0",
];

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(C)]
struct BladeRf {
    _opaque: [u8; 0],
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(C)]
struct BladeRfSerial {
    serial: [c_char; SERIAL_BYTES],
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct BladeRfVersion {
    major: u16,
    minor: u16,
    patch: u16,
    describe: *const c_char,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[repr(C)]
struct BladeRfMetadata {
    // Opaque marker only: this adapter always passes a null metadata pointer
    // and never allocates, dereferences or makes a layout claim for this type.
    _opaque: [u8; 0],
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type VersionFn = unsafe extern "C" fn(*mut BladeRfVersion);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type OpenFn = unsafe extern "C" fn(*mut *mut BladeRf, *const c_char) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type CloseFn = unsafe extern "C" fn(*mut BladeRf);
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type StrerrorFn = unsafe extern "C" fn(c_int) -> *const c_char;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetSerialStructFn = unsafe extern "C" fn(*mut BladeRf, *mut BladeRfSerial) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetBoardNameFn = unsafe extern "C" fn(*mut BladeRf) -> *const c_char;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetFpgaSizeFn = unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type IsFpgaConfiguredFn = unsafe extern "C" fn(*mut BladeRf) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetFpgaSourceFn = unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type FpgaVersionFn = unsafe extern "C" fn(*mut BladeRf, *mut BladeRfVersion) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type WishboneReadFn = unsafe extern "C" fn(*mut BladeRf, u32, *mut u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type WishboneWriteFn = unsafe extern "C" fn(*mut BladeRf, u32, u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type OpenMarkerFn = unsafe extern "C" fn() -> u32;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type LoadFpgaFn = unsafe extern "C" fn(*mut BladeRf, *const c_char) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type DeviceSpeedFn = unsafe extern "C" fn(*mut BladeRf) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetBiasTeeFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut bool) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetFrequencyFn = unsafe extern "C" fn(*mut BladeRf, c_int, u64) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetFrequencyFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut u64) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetSampleRateFn = unsafe extern "C" fn(*mut BladeRf, c_int, u32, *mut u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetSampleRateFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetBandwidthFn = unsafe extern "C" fn(*mut BladeRf, c_int, u32, *mut u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetBandwidthFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetGainModeFn = unsafe extern "C" fn(*mut BladeRf, c_int, c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetGainModeFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetGainFn = unsafe extern "C" fn(*mut BladeRf, c_int, c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetGainFn = unsafe extern "C" fn(*mut BladeRf, c_int, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetLoopbackFn = unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SetRxMuxFn = unsafe extern "C" fn(*mut BladeRf, c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type GetRxMuxFn = unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SyncConfigFn = unsafe extern "C" fn(*mut BladeRf, c_int, c_int, u32, u32, u32, u32) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type EnableModuleFn = unsafe extern "C" fn(*mut BladeRf, c_int, bool) -> c_int;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
type SyncRxFn =
    unsafe extern "C" fn(*mut BladeRf, *mut c_void, u32, *mut BladeRfMetadata, u32) -> c_int;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct NativeFunctions {
    version: VersionFn,
    open: OpenFn,
    close: CloseFn,
    strerror: StrerrorFn,
    get_serial: GetSerialStructFn,
    get_board_name: GetBoardNameFn,
    get_fpga_size: GetFpgaSizeFn,
    is_fpga_configured: IsFpgaConfiguredFn,
    get_fpga_source: GetFpgaSourceFn,
    fpga_version: FpgaVersionFn,
    wishbone_read: WishboneReadFn,
    wishbone_write: WishboneWriteFn,
}

/// Function table retained only by the loader-neutral receive-session worker.
///
/// Keeping this distinct from `NativeFunctions` prevents the existing
/// configuration-neutral register server from acquiring FPGA-load, RF or
/// streaming surfaces.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct LoaderFunctions {
    open: OpenFn,
    close: CloseFn,
    strerror: StrerrorFn,
    get_serial: GetSerialStructFn,
    get_board_name: GetBoardNameFn,
    get_fpga_size: GetFpgaSizeFn,
    load_fpga: LoadFpgaFn,
    device_speed: DeviceSpeedFn,
    get_bias_tee: GetBiasTeeFn,
    is_fpga_configured: IsFpgaConfiguredFn,
    get_fpga_source: GetFpgaSourceFn,
    fpga_version: FpgaVersionFn,
    wishbone_read: WishboneReadFn,
    wishbone_write: WishboneWriteFn,
    set_frequency: SetFrequencyFn,
    get_frequency: GetFrequencyFn,
    set_sample_rate: SetSampleRateFn,
    get_sample_rate: GetSampleRateFn,
    set_bandwidth: SetBandwidthFn,
    get_bandwidth: GetBandwidthFn,
    set_gain_mode: SetGainModeFn,
    get_gain_mode: GetGainModeFn,
    set_gain: SetGainFn,
    get_gain: GetGainFn,
    get_loopback: GetLoopbackFn,
    set_rx_mux: SetRxMuxFn,
    get_rx_mux: GetRxMuxFn,
    sync_config: SyncConfigFn,
    enable_module: EnableModuleFn,
    sync_rx: SyncRxFn,
}

/// Opaque non-null handle used only by the private loader worker.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
pub(crate) struct LoaderHandle(NonNull<BladeRf>);

/// Exact numeric version required from the already loaded profile-2 FPGA.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeFpgaVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl NativeFpgaVersion {
    pub(crate) const PROFILE2_REQUIRED: Self = Self {
        major: 0,
        minor: 16,
        patch: 0,
    };

    fn from_abi(version: BladeRfVersion) -> Self {
        Self {
            major: version.major,
            minor: version.minor,
            patch: version.patch,
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl fmt::Display for NativeFpgaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Exact numeric version returned by the qualified native library.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeLibraryVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

impl NativeLibraryVersion {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn from_abi(version: BladeRfVersion) -> Self {
        Self {
            major: version.major,
            minor: version.minor,
            patch: version.patch,
        }
    }
}

impl fmt::Display for NativeLibraryVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Result of inspecting one exact libbladeRF file without invoking a device API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeLibraryReport {
    path: PathBuf,
    sha256: String,
    version: NativeLibraryVersion,
    open_contract: Option<NativeOpenContract>,
}

impl NativeLibraryReport {
    /// Canonical source path whose bytes were copied into the sealed snapshot.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Lowercase SHA-256 digest verified before loading.
    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Exact numeric libbladeRF version reported by the library.
    pub(crate) const fn version(&self) -> &NativeLibraryVersion {
        &self.version
    }

    /// Configuration-neutral marker for the register-only open contract.
    pub(crate) const fn bladerf2_config_neutral_open_marker(&self) -> Option<u32> {
        match self.open_contract {
            Some(NativeOpenContract::ConfigNeutral) => Some(BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1),
            Some(NativeOpenContract::LoaderNeutral) | None => None,
        }
    }

    /// Loader-neutral marker for the required image-transition contract.
    pub(crate) const fn bladerf2_loader_neutral_open_marker(&self) -> Option<u32> {
        match self.open_contract {
            Some(NativeOpenContract::LoaderNeutral) => Some(BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1),
            Some(NativeOpenContract::ConfigNeutral) | None => None,
        }
    }
}

#[derive(Debug)]
struct QualifiedFile {
    canonical_path: PathBuf,
    staged_file: File,
    sha256: String,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
trait NativeCalls {
    type Handle: Copy;

    fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>);
    fn close(&mut self, handle: Self::Handle);
    fn error_text(&self, status: c_int) -> Option<String>;
    fn get_serial(&mut self, handle: Self::Handle, serial: &mut [u8; SERIAL_BYTES]) -> c_int;
    fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>>;
    fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int;
    fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int;
    fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int;
    fn fpga_version(&mut self, handle: Self::Handle, version: &mut NativeFpgaVersion) -> c_int;
    fn wishbone_read(&mut self, handle: Self::Handle, address: u32, value: &mut u32) -> c_int;
    fn wishbone_write(&mut self, handle: Self::Handle, address: u32, value: u32) -> c_int;
}

/// Private ABI-orchestration test seam for the native receive-session sequence.
///
/// This raw-value trait deliberately remains internal to this unsafe module;
/// only the typed `SessionCalls` implementation is the closed receive-only
/// surface. The production implementation retains the qualified dynamic
/// library. Tests use an ordinary Rust handle token and never construct raw
/// device pointers or C function tables.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
trait SessionNativeApi {
    type HandleToken: Copy;

    fn status_text(&self, status: c_int) -> Option<String>;
    fn device_speed(&mut self, handle: Self::HandleToken) -> c_int;
    fn get_bias_tee(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        enabled: &mut bool,
    ) -> c_int;
    fn get_loopback(&mut self, handle: Self::HandleToken, loopback: &mut c_int) -> c_int;
    fn set_rx_mux(&mut self, handle: Self::HandleToken, mux: c_int) -> c_int;
    fn get_rx_mux(&mut self, handle: Self::HandleToken, mux: &mut c_int) -> c_int;
    fn set_gain_mode(&mut self, handle: Self::HandleToken, channel: c_int, mode: c_int) -> c_int;
    fn get_gain_mode(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        mode: &mut c_int,
    ) -> c_int;
    fn set_frequency(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        frequency_hz: u64,
    ) -> c_int;
    fn get_frequency(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        frequency_hz: &mut u64,
    ) -> c_int;
    fn set_sample_rate(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        sample_rate_hz: u32,
        actual_sample_rate_hz: &mut u32,
    ) -> c_int;
    fn get_sample_rate(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        sample_rate_hz: &mut u32,
    ) -> c_int;
    fn set_bandwidth(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        bandwidth_hz: u32,
        actual_bandwidth_hz: &mut u32,
    ) -> c_int;
    fn get_bandwidth(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        bandwidth_hz: &mut u32,
    ) -> c_int;
    fn set_gain(&mut self, handle: Self::HandleToken, channel: c_int, gain_db: c_int) -> c_int;
    fn get_gain(&mut self, handle: Self::HandleToken, channel: c_int, gain_db: &mut c_int)
    -> c_int;
    #[allow(
        clippy::too_many_arguments,
        reason = "mirror the argument order of the C bladerf_sync_config ABI"
    )]
    fn sync_config(
        &mut self,
        handle: Self::HandleToken,
        layout: c_int,
        format: c_int,
        num_buffers: u32,
        buffer_samples: u32,
        num_transfers: u32,
        timeout_ms: u32,
    ) -> c_int;
    fn enable_module(&mut self, handle: Self::HandleToken, channel: c_int, enable: bool) -> c_int;
    fn sync_rx(&mut self, handle: Self::HandleToken, samples: &mut [u32], timeout_ms: u32)
    -> c_int;
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct DynamicCalls {
    functions: NativeFunctions,
    // Keep the loaded snapshot and its sealed backing file alive until after
    // the sole device handle has closed.
    _library: Library,
    _qualified: QualifiedFile,
}

/// Retained loader-neutral function table from one sealed, hash-qualified
/// library snapshot.
///
/// Construction performs only library qualification, symbol resolution,
/// `bladerf_version` and the device-independent B2L1 marker call. Device-facing
/// methods remain available solely through the private `LoaderCalls` and
/// `SessionCalls` traits. The device-free session-worker command retains this
/// table but gives its worker only deterministic model calls.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) struct LoaderDynamicCalls {
    functions: LoaderFunctions,
    // Drop the dynamic library before releasing the sealed file which supplied
    // its mapped bytes. The marker also makes the one-handle table !Send/!Sync.
    _library: Library,
    _qualified: QualifiedFile,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) struct LoaderDynamicCalls;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl DynamicCalls {
    fn load(path: &Path, expected_sha256: &str) -> Result<Self> {
        let qualified = qualify_file(path, expected_sha256)?;
        let library = load_qualified_library(&qualified)?;
        // SAFETY: This is the sealed, hash-qualified snapshot. The declarations
        // mirror the pinned x86-64 libbladeRF 2.6.0 header. Inspection invokes
        // only device-independent version and build-marker functions.
        let (functions, _version, open_contract) =
            unsafe { inspect_symbols(&library, Some(NativeOpenContract::ConfigNeutral)) }?;
        ensure!(
            open_contract == Some(NativeOpenContract::ConfigNeutral),
            "configuration-neutral marker was not retained after qualification"
        );
        Ok(Self {
            functions,
            _library: library,
            _qualified: qualified,
        })
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl LoaderCalls for LoaderDynamicCalls {
    type Handle = LoaderHandle;

    fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>) {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `raw` is an initialized exclusive output pointer and the
        // identifier is a live C string. This owner retains the library.
        let status = unsafe { (self.functions.open)(&mut raw, identifier.as_ptr()) };
        (status, NonNull::new(raw).map(LoaderHandle))
    }

    fn close(&mut self, handle: Self::Handle) {
        // SAFETY: The safe worker permits this call exactly once and only in a
        // pre-dispatch or verified-stock state. The non-null handle belongs to
        // this retained library table.
        unsafe { (self.functions.close)(handle.0.as_ptr()) };
    }

    fn error_text(&self, status: c_int) -> Option<String> {
        // SAFETY: The pinned function returns a borrowed static C string or
        // null. This handle-free diagnostic is copied immediately.
        let message = unsafe { (self.functions.strerror)(status) };
        (!message.is_null()).then(|| {
            // SAFETY: A non-null libbladeRF error string follows its C-string
            // contract and remains owned by the retained library.
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        })
    }

    fn get_serial(
        &mut self,
        handle: Self::Handle,
        serial: &mut [u8; LOADER_SERIAL_BYTES],
    ) -> c_int {
        let mut raw = BladeRfSerial {
            serial: [0; SERIAL_BYTES],
        };
        // SAFETY: The handle is live in the safe worker's pre-dispatch state;
        // `raw` is an initialized exclusive output matching the pinned ABI.
        let status = unsafe { (self.functions.get_serial)(handle.0.as_ptr(), &mut raw) };
        if status == 0 {
            for (destination, source) in serial.iter_mut().zip(raw.serial) {
                *destination = source as u8;
            }
        }
        status
    }

    fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>> {
        // SAFETY: The handle is live in the safe worker's pre-dispatch state.
        // Copy the borrowed string before another library call.
        let name = unsafe { (self.functions.get_board_name)(handle.0.as_ptr()) };
        (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_bytes().to_vec())
    }

    fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int {
        // SAFETY: The handle is live and `size` is an initialized exclusive
        // output for the C enum representation.
        unsafe { (self.functions.get_fpga_size)(handle.0.as_ptr(), size) }
    }

    fn load_custom(&mut self, handle: Self::Handle, image: &SealedImage<CustomImage>) -> c_int {
        // SAFETY: The safe worker marks the handle fail-closed before entering
        // this synchronous call. The typed owner keeps its sealed descriptor
        // and C path live until the call returns.
        unsafe { (self.functions.load_fpga)(handle.0.as_ptr(), image.custom_load_path().as_ptr()) }
    }

    fn load_stock(&mut self, handle: Self::Handle, image: &SealedImage<StockImage>) -> c_int {
        // SAFETY: The safe worker marks the handle fail-closed before entering
        // this synchronous call. The typed owner keeps its sealed descriptor
        // and C path live until the call returns.
        unsafe { (self.functions.load_fpga)(handle.0.as_ptr(), image.stock_load_path().as_ptr()) }
    }

    fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int {
        // SAFETY: The safe worker exposes this only while verifying a completed
        // explicit load; the function has no output pointer.
        unsafe { (self.functions.is_fpga_configured)(handle.0.as_ptr()) }
    }

    fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int {
        // SAFETY: The handle is in a post-load verification state and `source`
        // is an initialized exclusive C-enum output.
        unsafe { (self.functions.get_fpga_source)(handle.0.as_ptr(), source) }
    }

    fn fpga_version(&mut self, handle: Self::Handle, version: &mut LoaderFpgaVersion) -> c_int {
        let mut raw = BladeRfVersion {
            major: 0,
            minor: 0,
            patch: 0,
            describe: std::ptr::null(),
        };
        // SAFETY: The handle is in a post-load verification state and `raw` is
        // an initialized exclusive output matching the pinned structure.
        let status = unsafe { (self.functions.fpga_version)(handle.0.as_ptr(), &mut raw) };
        if status == 0 {
            *version = LoaderFpgaVersion {
                major: raw.major,
                minor: raw.minor,
                patch: raw.patch,
            };
        }
        status
    }

    fn read_profile_id(&mut self, handle: Self::Handle, value: &mut u32) -> c_int {
        // SAFETY: The safe trait fixes this call to the profile-2 ID offset and
        // exposes it only after a successful custom-image load.
        unsafe { (self.functions.wishbone_read)(handle.0.as_ptr(), 0x00, value) }
    }

    fn read_profile_capabilities(&mut self, handle: Self::Handle, value: &mut u32) -> c_int {
        // SAFETY: The safe trait fixes this call to the profile-2 capabilities
        // offset and exposes it only after a successful custom-image load.
        unsafe { (self.functions.wishbone_read)(handle.0.as_ptr(), 0x04, value) }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl SessionNativeApi for LoaderDynamicCalls {
    type HandleToken = LoaderHandle;

    fn status_text(&self, status: c_int) -> Option<String> {
        <Self as LoaderCalls>::error_text(self, status)
    }

    fn device_speed(&mut self, handle: Self::HandleToken) -> c_int {
        // SAFETY: The worker supplies its sole live non-null handle and this
        // owner retains the pinned function table and dynamic library.
        unsafe { (self.functions.device_speed)(handle.0.as_ptr()) }
    }

    fn get_bias_tee(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        enabled: &mut bool,
    ) -> c_int {
        // SAFETY: `enabled` is initialized and exclusively borrowed. The
        // channel is one of the fixed direction representatives.
        unsafe { (self.functions.get_bias_tee)(handle.0.as_ptr(), channel, enabled) }
    }

    fn get_loopback(&mut self, handle: Self::HandleToken, loopback: &mut c_int) -> c_int {
        // SAFETY: `loopback` is an initialized exclusive C-enum output and the
        // worker supplies its sole live handle.
        unsafe { (self.functions.get_loopback)(handle.0.as_ptr(), loopback) }
    }

    fn set_rx_mux(&mut self, handle: Self::HandleToken, mux: c_int) -> c_int {
        // SAFETY: The helper fixes `mux` to the pinned baseband RX value.
        unsafe { (self.functions.set_rx_mux)(handle.0.as_ptr(), mux) }
    }

    fn get_rx_mux(&mut self, handle: Self::HandleToken, mux: &mut c_int) -> c_int {
        // SAFETY: `mux` is an initialized exclusive C-enum output.
        unsafe { (self.functions.get_rx_mux)(handle.0.as_ptr(), mux) }
    }

    fn set_gain_mode(&mut self, handle: Self::HandleToken, channel: c_int, mode: c_int) -> c_int {
        // SAFETY: The helper maps only RX1/RX2 and fixes manual-gain mode.
        unsafe { (self.functions.set_gain_mode)(handle.0.as_ptr(), channel, mode) }
    }

    fn get_gain_mode(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        mode: &mut c_int,
    ) -> c_int {
        // SAFETY: `mode` is initialized and exclusively borrowed for the
        // helper's selected RX channel.
        unsafe { (self.functions.get_gain_mode)(handle.0.as_ptr(), channel, mode) }
    }

    fn set_frequency(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        frequency_hz: u64,
    ) -> c_int {
        // SAFETY: The helper maps only the selected receive channel.
        unsafe { (self.functions.set_frequency)(handle.0.as_ptr(), channel, frequency_hz) }
    }

    fn get_frequency(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        frequency_hz: &mut u64,
    ) -> c_int {
        // SAFETY: `frequency_hz` is initialized and exclusively borrowed.
        unsafe { (self.functions.get_frequency)(handle.0.as_ptr(), channel, frequency_hz) }
    }

    fn set_sample_rate(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        sample_rate_hz: u32,
        actual_sample_rate_hz: &mut u32,
    ) -> c_int {
        // SAFETY: `actual_sample_rate_hz` is initialized and exclusively
        // borrowed for the selected receive channel.
        unsafe {
            (self.functions.set_sample_rate)(
                handle.0.as_ptr(),
                channel,
                sample_rate_hz,
                actual_sample_rate_hz,
            )
        }
    }

    fn get_sample_rate(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        sample_rate_hz: &mut u32,
    ) -> c_int {
        // SAFETY: `sample_rate_hz` is initialized and exclusively borrowed.
        unsafe { (self.functions.get_sample_rate)(handle.0.as_ptr(), channel, sample_rate_hz) }
    }

    fn set_bandwidth(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        bandwidth_hz: u32,
        actual_bandwidth_hz: &mut u32,
    ) -> c_int {
        // SAFETY: `actual_bandwidth_hz` is initialized and exclusively
        // borrowed for the selected receive channel.
        unsafe {
            (self.functions.set_bandwidth)(
                handle.0.as_ptr(),
                channel,
                bandwidth_hz,
                actual_bandwidth_hz,
            )
        }
    }

    fn get_bandwidth(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        bandwidth_hz: &mut u32,
    ) -> c_int {
        // SAFETY: `bandwidth_hz` is initialized and exclusively borrowed.
        unsafe { (self.functions.get_bandwidth)(handle.0.as_ptr(), channel, bandwidth_hz) }
    }

    fn set_gain(&mut self, handle: Self::HandleToken, channel: c_int, gain_db: c_int) -> c_int {
        // SAFETY: The helper maps only the selected receive channel.
        unsafe { (self.functions.set_gain)(handle.0.as_ptr(), channel, gain_db) }
    }

    fn get_gain(
        &mut self,
        handle: Self::HandleToken,
        channel: c_int,
        gain_db: &mut c_int,
    ) -> c_int {
        // SAFETY: `gain_db` is initialized and exclusively borrowed for the
        // active selected receive channel.
        unsafe { (self.functions.get_gain)(handle.0.as_ptr(), channel, gain_db) }
    }

    fn sync_config(
        &mut self,
        handle: Self::HandleToken,
        layout: c_int,
        sample_format: c_int,
        num_buffers: u32,
        buffer_samples: u32,
        num_transfers: u32,
        timeout_ms: u32,
    ) -> c_int {
        // SAFETY: The helper supplies the pinned synchronous receive literals
        // and the worker serializes access to the sole live handle.
        unsafe {
            (self.functions.sync_config)(
                handle.0.as_ptr(),
                layout,
                sample_format,
                num_buffers,
                buffer_samples,
                num_transfers,
                timeout_ms,
            )
        }
    }

    fn enable_module(&mut self, handle: Self::HandleToken, channel: c_int, enable: bool) -> c_int {
        // SAFETY: The helper maps only RX1/RX2 and owns the serialized session
        // transition around this call.
        unsafe { (self.functions.enable_module)(handle.0.as_ptr(), channel, enable) }
    }

    fn sync_rx(
        &mut self,
        handle: Self::HandleToken,
        samples: &mut [u32],
        timeout_ms: u32,
    ) -> c_int {
        let sample_count =
            u32::try_from(samples.len()).expect("bounded synchronous RX chunk length must fit u32");
        // SAFETY: The initialized mutable u32 slice is aligned and has exactly
        // four writable bytes per SC16 Q11 sample. Its derived count cannot
        // exceed its extent. Metadata remains null for this selected format.
        unsafe {
            (self.functions.sync_rx)(
                handle.0.as_ptr(),
                samples.as_mut_ptr().cast(),
                sample_count,
                std::ptr::null_mut(),
                timeout_ms,
            )
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn receive_channel_value(channel: ReceiveChannel) -> c_int {
    match channel {
        ReceiveChannel::Rx1 => BLADERF_CHANNEL_RX1,
        ReceiveChannel::Rx2 => BLADERF_CHANNEL_RX2,
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn session_status_error<C: SessionNativeApi>(
    calls: &C,
    status: c_int,
    operation: &str,
) -> anyhow::Error {
    let detail = calls
        .status_text(status)
        .unwrap_or_else(|| "no diagnostic text".to_owned());
    anyhow::anyhow!("{operation} failed with libbladeRF status {status}: {detail}")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ensure_session_status<C: SessionNativeApi>(
    calls: &C,
    status: c_int,
    operation: &str,
) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(session_status_error(calls, status, operation))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn disable_after_receive_error<C: SessionNativeApi>(
    calls: &mut C,
    handle: C::HandleToken,
    channel: c_int,
    primary: anyhow::Error,
) -> anyhow::Error {
    let cleanup_status = calls.enable_module(handle, channel, false);
    if cleanup_status == 0 {
        primary
    } else {
        let cleanup = session_status_error(
            calls,
            cleanup_status,
            "disable selected RX channel after receive failure",
        );
        primary.context(format!("cleanup also failed: {cleanup:#}"))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn configure_receive_native<C: SessionNativeApi>(
    calls: &mut C,
    handle: C::HandleToken,
    plan: ReceivePlan,
    readback: &mut ConfiguredReceiveReadback,
) -> Result<()> {
    let channel = receive_channel_value(plan.channel());
    let device_speed = calls.device_speed(handle);
    ensure!(
        device_speed == BLADERF_DEVICE_SPEED_SUPER,
        "bladeRF device speed {device_speed} differs from required SuperSpeed value {BLADERF_DEVICE_SPEED_SUPER}"
    );

    for (label, bias_channel) in [("RX", BLADERF_CHANNEL_RX1), ("TX", BLADERF_CHANNEL_TX1)] {
        let mut enabled = true;
        let status = calls.get_bias_tee(handle, bias_channel, &mut enabled);
        ensure_session_status(calls, status, &format!("read {label} bias-tee state"))?;
        ensure!(!enabled, "{label} bias tee is enabled");
    }

    let mut loopback = -1;
    let status = calls.get_loopback(handle, &mut loopback);
    ensure_session_status(calls, status, "read bladeRF loopback")?;
    ensure!(
        loopback == BLADERF_LB_NONE,
        "bladeRF loopback {loopback} differs from required disabled value {BLADERF_LB_NONE}"
    );

    let status = calls.set_rx_mux(handle, BLADERF_RX_MUX_BASEBAND);
    ensure_session_status(calls, status, "select bladeRF baseband RX mux")?;
    let mut rx_mux = -1;
    let status = calls.get_rx_mux(handle, &mut rx_mux);
    ensure_session_status(calls, status, "read bladeRF RX mux")?;
    ensure!(
        rx_mux == BLADERF_RX_MUX_BASEBAND,
        "bladeRF RX mux {rx_mux} differs from required baseband value {BLADERF_RX_MUX_BASEBAND}"
    );

    let status = calls.set_gain_mode(handle, channel, BLADERF_GAIN_MGC);
    ensure_session_status(calls, status, "select manual RX gain")?;
    let mut gain_mode = -1;
    let status = calls.get_gain_mode(handle, channel, &mut gain_mode);
    ensure_session_status(calls, status, "read RX gain mode")?;
    ensure!(
        gain_mode == BLADERF_GAIN_MGC,
        "bladeRF RX gain mode {gain_mode} differs from required manual value {BLADERF_GAIN_MGC}"
    );

    let status = calls.set_frequency(handle, channel, plan.center_hz());
    ensure_session_status(calls, status, "set RX center frequency")?;
    let mut center_frequency_hz = 0;
    let status = calls.get_frequency(handle, channel, &mut center_frequency_hz);
    ensure_session_status(calls, status, "read RX center frequency")?;
    ensure!(
        center_frequency_hz.abs_diff(plan.center_hz()) <= MAX_FREQUENCY_ERROR_HZ,
        "RX center-frequency readback {center_frequency_hz} Hz differs from requested {} Hz by more than {MAX_FREQUENCY_ERROR_HZ} Hz",
        plan.center_hz(),
    );

    let mut actual_sample_rate_hz = 0;
    let status = calls.set_sample_rate(
        handle,
        channel,
        SOURCE_SAMPLE_RATE_HZ,
        &mut actual_sample_rate_hz,
    );
    ensure_session_status(calls, status, "set RX sample rate")?;
    ensure!(
        actual_sample_rate_hz == SOURCE_SAMPLE_RATE_HZ,
        "RX actual sample rate {actual_sample_rate_hz} Hz differs from required {SOURCE_SAMPLE_RATE_HZ} Hz"
    );
    let mut sample_rate_hz = 0;
    let status = calls.get_sample_rate(handle, channel, &mut sample_rate_hz);
    ensure_session_status(calls, status, "read RX sample rate")?;
    ensure!(
        sample_rate_hz == SOURCE_SAMPLE_RATE_HZ,
        "RX sample-rate readback {sample_rate_hz} Hz differs from required {SOURCE_SAMPLE_RATE_HZ} Hz"
    );

    let mut actual_bandwidth_hz = 0;
    let status = calls.set_bandwidth(
        handle,
        channel,
        SOURCE_BANDWIDTH_HZ,
        &mut actual_bandwidth_hz,
    );
    ensure_session_status(calls, status, "set RX bandwidth")?;
    ensure!(
        actual_bandwidth_hz == SOURCE_BANDWIDTH_HZ,
        "RX actual bandwidth {actual_bandwidth_hz} Hz differs from required {SOURCE_BANDWIDTH_HZ} Hz"
    );
    let mut bandwidth_hz = 0;
    let status = calls.get_bandwidth(handle, channel, &mut bandwidth_hz);
    ensure_session_status(calls, status, "read RX bandwidth")?;
    ensure!(
        bandwidth_hz == SOURCE_BANDWIDTH_HZ,
        "RX bandwidth readback {bandwidth_hz} Hz differs from required {SOURCE_BANDWIDTH_HZ} Hz"
    );

    let status = calls.set_gain(handle, channel, plan.gain_db());
    ensure_session_status(calls, status, "set manual RX gain")?;
    let status = calls.sync_config(
        handle,
        BLADERF_RX_X1,
        BLADERF_FORMAT_SC16_Q11,
        SESSION_NUM_BUFFERS,
        SESSION_BUFFER_SAMPLES,
        SESSION_NUM_TRANSFERS,
        SESSION_STREAM_TIMEOUT_MS,
    );
    ensure_session_status(calls, status, "configure synchronous RX stream")?;

    *readback = ConfiguredReceiveReadback::new(
        plan.channel(),
        center_frequency_hz,
        sample_rate_hz,
        bandwidth_hz,
    );
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn start_receive_native<C: SessionNativeApi>(
    calls: &mut C,
    handle: C::HandleToken,
    plan: ReceivePlan,
    readback: &mut ActiveReceiveReadback,
) -> Result<()> {
    let channel = receive_channel_value(plan.channel());
    let status = calls.enable_module(handle, channel, true);
    ensure_session_status(calls, status, "enable selected RX channel")?;

    let active = (|| -> Result<ActiveReceiveReadback> {
        let mut gain_db = 0;
        let status = calls.get_gain(handle, channel, &mut gain_db);
        ensure_session_status(calls, status, "read active RX gain")?;
        ensure!(
            gain_db == plan.gain_db(),
            "active RX gain readback {gain_db} dB differs from requested {} dB",
            plan.gain_db()
        );

        let mut center_frequency_hz = 0;
        let status = calls.get_frequency(handle, channel, &mut center_frequency_hz);
        ensure_session_status(calls, status, "read active RX center frequency")?;
        ensure!(
            center_frequency_hz.abs_diff(plan.center_hz()) <= MAX_FREQUENCY_ERROR_HZ,
            "active RX center-frequency readback {center_frequency_hz} Hz differs from requested {} Hz by more than {MAX_FREQUENCY_ERROR_HZ} Hz",
            plan.center_hz(),
        );

        let mut sample_rate_hz = 0;
        let status = calls.get_sample_rate(handle, channel, &mut sample_rate_hz);
        ensure_session_status(calls, status, "read active RX sample rate")?;
        ensure!(
            sample_rate_hz == SOURCE_SAMPLE_RATE_HZ,
            "active RX sample-rate readback {sample_rate_hz} Hz differs from required {SOURCE_SAMPLE_RATE_HZ} Hz"
        );

        let mut bandwidth_hz = 0;
        let status = calls.get_bandwidth(handle, channel, &mut bandwidth_hz);
        ensure_session_status(calls, status, "read active RX bandwidth")?;
        ensure!(
            bandwidth_hz == SOURCE_BANDWIDTH_HZ,
            "active RX bandwidth readback {bandwidth_hz} Hz differs from required {SOURCE_BANDWIDTH_HZ} Hz"
        );

        Ok(ActiveReceiveReadback::new(
            ConfiguredReceiveReadback::new(
                plan.channel(),
                center_frequency_hz,
                sample_rate_hz,
                bandwidth_hz,
            ),
            gain_db,
        ))
    })();

    match active {
        Ok(active) => {
            *readback = active;
            Ok(())
        }
        Err(primary) => Err(disable_after_receive_error(calls, handle, channel, primary)),
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn capture_receive_native<C: SessionNativeApi>(
    calls: &mut C,
    handle: C::HandleToken,
    plan: ReceivePlan,
    output: &mut dyn Write,
) -> Result<CaptureReport> {
    let channel = receive_channel_value(plan.channel());
    let capture = (|| -> Result<CaptureReport> {
        let mut buffer = vec![0u32; SESSION_BUFFER_SAMPLES as usize];
        let mut remaining = plan.expected_samples();
        let mut bytes = 0u64;
        let mut hasher = Sha256::new();
        while remaining != 0 {
            let sample_count = remaining.min(SESSION_BUFFER_SAMPLES);
            let samples = &mut buffer[..sample_count as usize];
            let status = calls.sync_rx(handle, samples, SESSION_STREAM_TIMEOUT_MS);
            ensure_session_status(calls, status, "receive synchronous profile-2 samples")?;
            let byte_count = std::mem::size_of_val(samples);
            // SAFETY: The successful native call populated the initialized u32
            // slice. This byte view has the same lifetime and exact extent and
            // is consumed before the buffer is reused.
            let chunk =
                unsafe { std::slice::from_raw_parts(samples.as_ptr().cast::<u8>(), byte_count) };
            output
                .write_all(chunk)
                .context("write completed profile-2 capture chunk")?;
            hasher.update(chunk);
            bytes = bytes
                .checked_add(byte_count as u64)
                .context("profile-2 capture byte count overflow")?;
            remaining -= sample_count;
        }
        output
            .flush()
            .context("flush completed profile-2 capture")?;
        ensure!(
            bytes == plan.expected_bytes(),
            "profile-2 capture wrote {bytes} bytes; expected {}",
            plan.expected_bytes()
        );
        Ok(CaptureReport::new(
            plan.expected_samples(),
            bytes,
            format!("{:x}", hasher.finalize()),
        ))
    })();

    capture.map_err(|primary| disable_after_receive_error(calls, handle, channel, primary))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn stop_receive_native<C: SessionNativeApi>(
    calls: &mut C,
    handle: C::HandleToken,
    channel: ReceiveChannel,
) -> Result<()> {
    let status = calls.enable_module(handle, receive_channel_value(channel), false);
    ensure_session_status(calls, status, "disable selected RX channel")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl SessionCalls for LoaderDynamicCalls {
    fn read_register(
        &mut self,
        handle: Self::Handle,
        register: ReadRegister,
        value: &mut u32,
    ) -> c_int {
        // SAFETY: The safe session worker exposes only the closed readable
        // register enum after exact profile identity, and `value` is an
        // initialized exclusive output.
        unsafe { (self.functions.wishbone_read)(handle.0.as_ptr(), register.byte_offset(), value) }
    }

    fn write_shadow(&mut self, handle: Self::Handle, channel: ShadowChannel, value: u32) -> c_int {
        // SAFETY: The safe session worker exposes only the four typed shadow
        // offsets while RX is stopped after exact profile identity.
        unsafe { (self.functions.wishbone_write)(handle.0.as_ptr(), channel.byte_offset(), value) }
    }

    fn commit(&mut self, handle: Self::Handle) -> c_int {
        // SAFETY: The safe session worker fixes both the commit address and
        // complete value and exposes this only while RX is stopped.
        unsafe {
            (self.functions.wishbone_write)(handle.0.as_ptr(), COMMIT_BYTE_OFFSET, COMMIT_MAGIC)
        }
    }

    fn configure_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ConfiguredReceiveReadback,
    ) -> Result<()> {
        configure_receive_native(self, handle, plan, readback)
    }

    fn start_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        readback: &mut ActiveReceiveReadback,
    ) -> Result<()> {
        start_receive_native(self, handle, plan, readback)
    }

    fn capture_receive(
        &mut self,
        handle: Self::Handle,
        plan: ReceivePlan,
        output: &mut dyn Write,
    ) -> Result<CaptureReport> {
        capture_receive_native(self, handle, plan, output)
    }

    fn stop_receive(&mut self, handle: Self::Handle, channel: ReceiveChannel) -> Result<()> {
        stop_receive_native(self, handle, channel)
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl NativeCalls for DynamicCalls {
    type Handle = NonNull<BladeRf>;

    fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>) {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `raw` is an initialized exclusive output pointer and the
        // identifier is a live C string. `self` retains the library.
        let status = unsafe { (self.functions.open)(&mut raw, identifier.as_ptr()) };
        (status, NonNull::new(raw))
    }

    fn close(&mut self, handle: Self::Handle) {
        // SAFETY: A `NativeSession` owns this non-null handle and takes its
        // Option before this call, preventing a second close.
        unsafe { (self.functions.close)(handle.as_ptr()) };
    }

    fn error_text(&self, status: c_int) -> Option<String> {
        // SAFETY: The pinned function returns a borrowed static C string or,
        // defensively, null. We copy it immediately.
        let message = unsafe { (self.functions.strerror)(status) };
        (!message.is_null()).then(|| {
            // SAFETY: A non-null libbladeRF error string follows its C-string
            // contract and remains owned by the loaded library.
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        })
    }

    fn get_serial(&mut self, handle: Self::Handle, serial: &mut [u8; SERIAL_BYTES]) -> c_int {
        let mut raw = BladeRfSerial {
            serial: [0; SERIAL_BYTES],
        };
        // SAFETY: The handle is live and `raw` is an initialized exclusive
        // output matching the pinned structure.
        let status = unsafe { (self.functions.get_serial)(handle.as_ptr(), &mut raw) };
        if status == 0 {
            for (destination, source) in serial.iter_mut().zip(raw.serial) {
                *destination = source as u8;
            }
        }
        status
    }

    fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>> {
        // SAFETY: The live device returns a borrowed C string or null. Copy it
        // before making another library call.
        let name = unsafe { (self.functions.get_board_name)(handle.as_ptr()) };
        (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_bytes().to_vec())
    }

    fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int {
        // SAFETY: The handle is live and `size` is an initialized, aligned,
        // exclusive output for the C enum representation.
        unsafe { (self.functions.get_fpga_size)(handle.as_ptr(), size) }
    }

    fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int {
        // SAFETY: The handle is live and the function has no output pointers.
        unsafe { (self.functions.is_fpga_configured)(handle.as_ptr()) }
    }

    fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int {
        // SAFETY: The handle is live and `source` is an initialized, aligned,
        // exclusive output for the C enum representation.
        unsafe { (self.functions.get_fpga_source)(handle.as_ptr(), source) }
    }

    fn fpga_version(&mut self, handle: Self::Handle, version: &mut NativeFpgaVersion) -> c_int {
        let mut raw = BladeRfVersion {
            major: 0,
            minor: 0,
            patch: 0,
            describe: std::ptr::null(),
        };
        // SAFETY: The handle is live and `raw` is an initialized exclusive
        // output matching the pinned structure.
        let status = unsafe { (self.functions.fpga_version)(handle.as_ptr(), &mut raw) };
        if status == 0 {
            *version = NativeFpgaVersion::from_abi(raw);
        }
        status
    }

    fn wishbone_read(&mut self, handle: Self::Handle, address: u32, value: &mut u32) -> c_int {
        // SAFETY: The handle is live and `value` is an initialized exclusive
        // output. Callers expose only reviewed profile-2 byte offsets.
        unsafe { (self.functions.wishbone_read)(handle.as_ptr(), address, value) }
    }

    fn wishbone_write(&mut self, handle: Self::Handle, address: u32, value: u32) -> c_int {
        // SAFETY: The handle is live. Callers expose only reviewed profile-2
        // byte offsets and values.
        unsafe { (self.functions.wishbone_write)(handle.as_ptr(), address, value) }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
struct NativeSession<C: NativeCalls> {
    calls: C,
    handle: Option<C::Handle>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl<C: NativeCalls> NativeSession<C> {
    fn handle(&self) -> Result<C::Handle> {
        self.handle.context("native bladeRF session is shut down")
    }

    fn read_register(&mut self, address: u32) -> Result<u32> {
        let handle = self.handle()?;
        let mut value = 0u32;
        let status = self.calls.wishbone_read(handle, address, &mut value);
        ensure_native_status(
            &self.calls,
            status,
            &format!("read Wishbone byte offset 0x{address:02x}"),
        )?;
        Ok(value)
    }

    fn write_register(&mut self, address: u32, value: u32) -> Result<()> {
        let handle = self.handle()?;
        let status = self.calls.wishbone_write(handle, address, value);
        ensure_native_status(
            &self.calls,
            status,
            &format!("write 0x{value:08x} to Wishbone byte offset 0x{address:02x}"),
        )
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.calls.close(handle);
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl<C: NativeCalls> Drop for NativeSession<C> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn validate_requested_serial(serial: &str) -> Result<()> {
    ensure!(
        serial.len() == 32
            && serial
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "bladeRF serial must be exactly 32 lowercase hexadecimal characters"
    );
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ensure_native_status<C: NativeCalls>(calls: &C, status: c_int, operation: &str) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    let detail = calls
        .error_text(status)
        .unwrap_or_else(|| "no diagnostic text".to_owned());
    anyhow::bail!("{operation} failed with libbladeRF status {status}: {detail}")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn validate_returned_serial(raw: &[u8; SERIAL_BYTES], expected: &str) -> Result<()> {
    ensure!(
        raw[SERIAL_BYTES - 1] == 0,
        "returned bladeRF serial lacks its required terminator"
    );
    let serial = &raw[..SERIAL_BYTES - 1];
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

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn open_validated_session<C: NativeCalls>(
    mut calls: C,
    serial: &str,
    expected_fpga_version: NativeFpgaVersion,
) -> Result<NativeSession<C>> {
    validate_requested_serial(serial)?;
    let identifier = CString::new(format!("libusb:serial={serial}"))
        .context("construct exact bladeRF device selector")?;
    let (open_status, handle) = calls.open(&identifier);
    if open_status != 0 {
        let error = ensure_native_status(&calls, open_status, "open exact serial-selected bladeRF")
            .expect_err("nonzero open status must fail");
        if let Some(handle) = handle {
            calls.close(handle);
        }
        return Err(error);
    }
    let handle = handle.context("libbladeRF reported successful open with a null device handle")?;
    let mut session = NativeSession {
        calls,
        handle: Some(handle),
        _not_send_or_sync: PhantomData,
    };

    let mut returned_serial = [0u8; SERIAL_BYTES];
    let status = session.calls.get_serial(handle, &mut returned_serial);
    ensure_native_status(&session.calls, status, "read opened bladeRF serial")?;
    validate_returned_serial(&returned_serial, serial)?;

    let board_name = session
        .calls
        .get_board_name(handle)
        .context("opened bladeRF returned a null board name")?;
    ensure!(
        board_name == b"bladerf2",
        "opened board name {:?} differs from required bladerf2",
        String::from_utf8_lossy(&board_name)
    );

    let mut fpga_size = 0;
    let status = session.calls.get_fpga_size(handle, &mut fpga_size);
    ensure_native_status(&session.calls, status, "read bladeRF FPGA size")?;
    ensure!(
        fpga_size == BLADERF_FPGA_A9,
        "bladeRF FPGA size {fpga_size} differs from required xA9 value {BLADERF_FPGA_A9}"
    );

    let configured = session.calls.is_fpga_configured(handle);
    if configured < 0 {
        ensure_native_status(
            &session.calls,
            configured,
            "read bladeRF FPGA configured state",
        )?;
    }
    ensure!(
        configured == 1,
        "bladeRF FPGA configured state {configured} differs from required 1"
    );

    let mut fpga_source = 0;
    let status = session.calls.get_fpga_source(handle, &mut fpga_source);
    ensure_native_status(&session.calls, status, "read bladeRF FPGA source")?;
    ensure!(
        fpga_source == BLADERF_FPGA_SOURCE_HOST,
        "bladeRF FPGA source {fpga_source} differs from required host-loaded value {BLADERF_FPGA_SOURCE_HOST}"
    );

    let mut fpga_version = NativeFpgaVersion {
        major: 0,
        minor: 0,
        patch: 0,
    };
    let status = session.calls.fpga_version(handle, &mut fpga_version);
    ensure_native_status(&session.calls, status, "read bladeRF FPGA version")?;
    ensure!(
        fpga_version == expected_fpga_version,
        "bladeRF FPGA version {fpga_version} differs from required {expected_fpga_version}"
    );

    let profile_id = session.read_register(0x00)?;
    ensure!(
        profile_id == PROFILE2_ID,
        "profile-2 ID 0x{profile_id:08x} differs from required 0x{PROFILE2_ID:08x}"
    );
    let capabilities = session.read_register(0x04)?;
    ensure!(
        capabilities == PROFILE2_CAPABILITIES,
        "profile-2 capabilities 0x{capabilities:08x} differ from required 0x{PROFILE2_CAPABILITIES:08x}"
    );

    Ok(session)
}

/// One exact serial-selected bladeRF2 handle backed by the qualified library.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) struct NativeDevice {
    session: NativeSession<DynamicCalls>,
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(crate) struct NativeDevice;

impl NativeDevice {
    /// Qualify the exact library and open one already configured profile-2 xA9.
    pub(crate) fn open(path: &Path, expected_sha256: &str, serial: &str) -> Result<Self> {
        // Validate before any filesystem access, dynamic loading or USB call.
        validate_requested_serial(serial)?;
        ensure_supported_target()?;
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let calls = DynamicCalls::load(path, expected_sha256)?;
            let session =
                open_validated_session(calls, serial, NativeFpgaVersion::PROFILE2_REQUIRED)?;
            Ok(Self { session })
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (path, expected_sha256);
            anyhow::bail!("native bladeRF sessions are supported only on x86-64 Linux")
        }
    }

    pub(crate) fn read_register(&mut self, address: u32) -> Result<u32> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            self.session.read_register(address)
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = address;
            anyhow::bail!("native bladeRF sessions are supported only on x86-64 Linux")
        }
    }

    pub(crate) fn write_register(&mut self, address: u32, value: u32) -> Result<()> {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            self.session.write_register(address, value)
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            let _ = (address, value);
            anyhow::bail!("native bladeRF sessions are supported only on x86-64 Linux")
        }
    }

    pub(crate) fn shutdown(&mut self) {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        self.session.shutdown();
    }
}

/// Inspect a hash-qualified libbladeRF without invoking a device API.
///
/// The expected digest must be exactly 64 lowercase hexadecimal characters.
/// On x86-64 Linux, the function copies and hashes one open source descriptor
/// into an anonymous memfd, seals the snapshot against writes and size changes,
/// and passes only that sealed descriptor through `/proc/self/fd` to `dlopen`.
/// The inspected source must be a nonempty regular file no larger than 64 MiB.
///
/// # Errors
///
/// Returns an error for invalid input, file metadata or I/O failure, a digest
/// mismatch, staging/sealing/loading/symbol failure, or any runtime semantic
/// version other than libbladeRF 2.6.0. The resolved declarations are audited
/// separately against the pinned header's API `0x02060000` contract.
pub(crate) fn check_library(path: &Path, expected_sha256: &str) -> Result<NativeLibraryReport> {
    check_library_with_contract(path, expected_sha256, None)
}

/// Inspect a hash-qualified library and require the patched bladeRF 2
/// configuration-neutral open contract without invoking a device API.
pub(crate) fn check_bladerf2_config_neutral_library(
    path: &Path,
    expected_sha256: &str,
) -> Result<NativeLibraryReport> {
    check_library_with_contract(
        path,
        expected_sha256,
        Some(NativeOpenContract::ConfigNeutral),
    )
}

/// Inspect a hash-qualified library and require the patched bladeRF 2
/// loader-neutral open contract without invoking a device API.
pub(crate) fn check_bladerf2_loader_neutral_library(
    path: &Path,
    expected_sha256: &str,
) -> Result<NativeLibraryReport> {
    check_library_with_contract(
        path,
        expected_sha256,
        Some(NativeOpenContract::LoaderNeutral),
    )
}

/// Retain the exact loader-neutral image/control/RX table without invoking any
/// device API.
///
/// This differs from the report-only preflight: the typed function pointers
/// needed by the receive-only safe surface, dynamic library mapping and sealed
/// backing file remain owned together. The raw table includes generic C
/// functions such as module enable; `SessionCalls` fixes their use to RX.
/// Construction still calls only
/// `bladerf_version` and the device-independent B2L1 marker.
pub(crate) fn prepare_bladerf2_loader_neutral_library(
    path: &Path,
    expected_sha256: &str,
) -> Result<(LoaderDynamicCalls, NativeLibraryReport)> {
    ensure_supported_target()?;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let qualified = qualify_file(path, expected_sha256)?;
        let library = load_qualified_library(&qualified)?;
        // SAFETY: The library is the sealed, hash-qualified snapshot. Typed
        // declarations mirror the pinned x86-64 libbladeRF 2.6.0 header. This
        // inspection invokes only version and the device-independent marker.
        let (functions, version) = unsafe { inspect_loader_symbols(&library) }?;
        let report = NativeLibraryReport {
            path: qualified.canonical_path.clone(),
            sha256: qualified.sha256.clone(),
            version,
            open_contract: Some(NativeOpenContract::LoaderNeutral),
        };
        let calls = LoaderDynamicCalls {
            functions,
            _library: library,
            _qualified: qualified,
            _not_send_or_sync: PhantomData,
        };
        Ok((calls, report))
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (path, expected_sha256);
        anyhow::bail!("loader-neutral library preparation is supported only on x86-64 Linux")
    }
}

fn check_library_with_contract(
    path: &Path,
    expected_sha256: &str,
    required_open_contract: Option<NativeOpenContract>,
) -> Result<NativeLibraryReport> {
    ensure_supported_target()?;
    let qualified = qualify_file(path, expected_sha256)?;
    let (version, open_contract) = inspect_qualified_library(&qualified, required_open_contract)?;
    Ok(NativeLibraryReport {
        path: qualified.canonical_path,
        sha256: qualified.sha256,
        version,
        open_contract,
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn ensure_supported_target() -> Result<()> {
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn ensure_supported_target() -> Result<()> {
    anyhow::bail!("native library inspection is supported only on x86_64 Linux")
}

fn validate_expected_sha256(expected: &str) -> Result<()> {
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "expected library SHA-256 must be exactly 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn qualify_file(path: &Path, expected_sha256: &str) -> Result<QualifiedFile> {
    validate_expected_sha256(expected_sha256)?;
    ensure!(!path.as_os_str().is_empty(), "library path is empty");
    let canonical_path = fs::canonicalize(path)
        .with_context(|| format!("resolve native library {}", path.display()))?;
    let (mut source, source_size) = open_validated_source(&canonical_path)?;
    let mut staged_file = create_staged_file()?;

    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut hashed_bytes = 0u64;
    loop {
        let count = source
            .read(&mut buffer)
            .with_context(|| format!("read native library {}", canonical_path.display()))?;
        if count == 0 {
            break;
        }
        hashed_bytes = hashed_bytes
            .checked_add(count as u64)
            .context("native library byte count overflow")?;
        ensure!(
            hashed_bytes <= MAX_LIBRARY_BYTES,
            "native library grew beyond the {}-byte maximum while hashing {}",
            MAX_LIBRARY_BYTES,
            canonical_path.display()
        );
        hasher.update(&buffer[..count]);
        staged_file
            .write_all(&buffer[..count])
            .context("stage native library in immutable snapshot")?;
    }
    ensure!(
        hashed_bytes == source_size,
        "native library size changed while hashing {}",
        canonical_path.display()
    );
    let actual_sha256 = format!("{:x}", hasher.finalize());
    ensure!(
        actual_sha256 == expected_sha256,
        "native library SHA-256 mismatch for {}: expected {}, found {}",
        canonical_path.display(),
        expected_sha256,
        actual_sha256
    );
    staged_file
        .flush()
        .context("flush staged native-library snapshot")?;
    staged_file
        .seek(SeekFrom::Start(0))
        .context("rewind staged native-library snapshot")?;
    seal_staged_file(&staged_file)?;

    Ok(QualifiedFile {
        canonical_path,
        staged_file,
        sha256: actual_sha256,
    })
}

#[cfg(target_os = "linux")]
fn open_validated_source(canonical_path: &Path) -> Result<(File, u64)> {
    let pinned = nix_open(
        canonical_path,
        OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("pin native library {}", canonical_path.display()))?;
    let pinned_stat = fstat(&pinned)
        .with_context(|| format!("inspect native library {}", canonical_path.display()))?;
    ensure!(
        is_regular_file_type(SFlag::from_bits_truncate(pinned_stat.st_mode)),
        "native library is not a regular file: {}",
        canonical_path.display()
    );
    ensure!(
        pinned_stat.st_size > 0,
        "native library is empty: {}",
        canonical_path.display()
    );
    ensure!(
        pinned_stat.st_size as u64 <= MAX_LIBRARY_BYTES,
        "native library is {} bytes; maximum is {}",
        pinned_stat.st_size,
        MAX_LIBRARY_BYTES
    );

    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
    let source = File::open(&descriptor_path).with_context(|| {
        format!(
            "open validated native-library inode {}",
            canonical_path.display()
        )
    })?;
    let readable_stat = fstat(&source)
        .with_context(|| format!("reinspect native library {}", canonical_path.display()))?;
    ensure!(
        readable_stat.st_dev == pinned_stat.st_dev && readable_stat.st_ino == pinned_stat.st_ino,
        "native library inode changed before reading {}",
        canonical_path.display()
    );
    Ok((source, pinned_stat.st_size as u64))
}

#[cfg(target_os = "linux")]
fn is_regular_file_type(file_type: SFlag) -> bool {
    file_type & SFlag::S_IFMT == SFlag::S_IFREG
}

#[cfg(not(target_os = "linux"))]
fn open_validated_source(_canonical_path: &Path) -> Result<(File, u64)> {
    anyhow::bail!("native library inspection is supported only on Linux")
}

#[cfg(target_os = "linux")]
fn create_staged_file() -> Result<File> {
    let descriptor = memfd_create(
        "bladerf-profile2-native",
        MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
    )
    .context("create sealed native-library snapshot")?;
    Ok(File::from(descriptor))
}

#[cfg(not(target_os = "linux"))]
fn create_staged_file() -> Result<File> {
    anyhow::bail!("native library inspection is supported only on Linux")
}

#[cfg(target_os = "linux")]
fn seal_staged_file(staged_file: &File) -> Result<()> {
    let required = SealFlag::F_SEAL_WRITE
        | SealFlag::F_SEAL_GROW
        | SealFlag::F_SEAL_SHRINK
        | SealFlag::F_SEAL_SEAL;
    fcntl(staged_file, FcntlArg::F_ADD_SEALS(required)).context("seal native-library snapshot")?;
    let actual =
        fcntl(staged_file, FcntlArg::F_GET_SEALS).context("read native-library snapshot seals")?;
    ensure!(
        actual & required.bits() == required.bits(),
        "native-library snapshot is missing required seals"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn seal_staged_file(_staged_file: &File) -> Result<()> {
    anyhow::bail!("native library inspection is supported only on Linux")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn load_qualified_library(qualified: &QualifiedFile) -> Result<Library> {
    let descriptor_path = PathBuf::from(format!(
        "/proc/self/fd/{}",
        qualified.staged_file.as_raw_fd()
    ));
    // SAFETY: The path names the still-open sealed memfd whose complete bytes
    // were bounded and hash-checked above. Loading a library can run its own
    // initializers, which is why this occurs only after the exact digest match
    // and WRITE/GROW/SHRINK/SEAL seals have been verified.
    unsafe { Library::new(&descriptor_path) }.with_context(|| {
        format!(
            "load hash-qualified native library {}",
            qualified.canonical_path.display()
        )
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn inspect_qualified_library(
    qualified: &QualifiedFile,
    required_open_contract: Option<NativeOpenContract>,
) -> Result<(NativeLibraryVersion, Option<NativeOpenContract>)> {
    let library = load_qualified_library(qualified)?;
    // SAFETY: Every declaration below mirrors the selected signatures in the
    // pinned libbladeRF 2.6.0 header for x86_64-unknown-linux-gnu. Merely
    // resolving a symbol does not call it. `inspect_symbols` invokes only the
    // device-independent version function and, when required, the specialized
    // build-marker function.
    let (_functions, version, open_contract) =
        unsafe { inspect_symbols(&library, required_open_contract) }?;
    Ok((version, open_contract))
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn inspect_qualified_library(
    _qualified: &QualifiedFile,
    _required_open_contract: Option<NativeOpenContract>,
) -> Result<(NativeLibraryVersion, Option<NativeOpenContract>)> {
    anyhow::bail!("native library inspection is supported only on x86_64 Linux")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn require_symbol<T>(library: &Library, name: &'static [u8]) -> Result<T>
where
    T: Copy,
{
    // SAFETY: The caller supplies the exact ABI type associated with `name` in
    // the pinned header and keeps `library` alive while the pointer is used.
    let symbol = unsafe { library.get::<T>(name) }.with_context(|| {
        let printable = String::from_utf8_lossy(name.strip_suffix(b"\0").unwrap_or(name));
        format!("resolve required libbladeRF symbol {printable}")
    })?;
    Ok(*symbol)
}

fn printable_symbol(name: &[u8]) -> String {
    String::from_utf8_lossy(name.strip_suffix(b"\0").unwrap_or(name)).into_owned()
}

fn check_required_symbol_names(mut resolve: impl FnMut(&'static [u8]) -> Result<()>) -> Result<()> {
    for name in REQUIRED_SYMBOLS {
        resolve(name).with_context(|| {
            format!(
                "resolve required libbladeRF symbol {}",
                printable_symbol(name)
            )
        })?;
    }
    Ok(())
}

fn check_loader_required_symbol_names(
    mut resolve: impl FnMut(&'static [u8]) -> Result<()>,
) -> Result<()> {
    for name in LOADER_REQUIRED_SYMBOLS {
        resolve(name).with_context(|| {
            format!(
                "resolve required loader libbladeRF symbol {}",
                printable_symbol(name)
            )
        })?;
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn ensure_required_symbols(library: &Library) -> Result<()> {
    check_required_symbol_names(|name| {
        // SAFETY: This lookup checks only that a symbol address exists. It does
        // not dereference or call the address, and `library` outlives the
        // returned temporary symbol wrapper.
        let _symbol = unsafe { library.get::<*const c_void>(name) }?;
        Ok(())
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn ensure_loader_required_symbols(library: &Library) -> Result<()> {
    check_loader_required_symbol_names(|name| {
        // SAFETY: This lookup checks only that a symbol address exists. It does
        // not dereference or call the address, and `library` outlives the
        // returned temporary symbol wrapper.
        let _symbol = unsafe { library.get::<*const c_void>(name) }?;
        Ok(())
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn inspect_symbols(
    library: &Library,
    required_open_contract: Option<NativeOpenContract>,
) -> Result<(
    NativeFunctions,
    NativeLibraryVersion,
    Option<NativeOpenContract>,
)> {
    // SAFETY: Presence checks do not call any symbol. Typed resolution below
    // then associates each already-present name with its pinned declaration.
    unsafe { ensure_required_symbols(library) }?;
    // SAFETY: Each type is the pinned C declaration for its named symbol. The
    // function pointers remain valid because `library` is borrowed for this
    // entire inspection.
    let functions = NativeFunctions {
        version: unsafe { require_symbol(library, b"bladerf_version\0") }?,
        open: unsafe { require_symbol(library, b"bladerf_open\0") }?,
        close: unsafe { require_symbol(library, b"bladerf_close\0") }?,
        strerror: unsafe { require_symbol(library, b"bladerf_strerror\0") }?,
        get_serial: unsafe { require_symbol(library, b"bladerf_get_serial_struct\0") }?,
        get_board_name: unsafe { require_symbol(library, b"bladerf_get_board_name\0") }?,
        get_fpga_size: unsafe { require_symbol(library, b"bladerf_get_fpga_size\0") }?,
        is_fpga_configured: unsafe { require_symbol(library, b"bladerf_is_fpga_configured\0") }?,
        get_fpga_source: unsafe { require_symbol(library, b"bladerf_get_fpga_source\0") }?,
        fpga_version: unsafe { require_symbol(library, b"bladerf_fpga_version\0") }?,
        wishbone_read: unsafe { require_symbol(library, b"bladerf_wishbone_master_read\0") }?,
        wishbone_write: unsafe { require_symbol(library, b"bladerf_wishbone_master_write\0") }?,
    };

    let mut raw_version = BladeRfVersion {
        major: 0,
        minor: 0,
        patch: 0,
        describe: std::ptr::null(),
    };
    // SAFETY: `raw_version` is aligned, initialized and exclusively borrowed;
    // the pinned function writes this fixed-layout structure and retains no
    // pointer to it.
    unsafe { (functions.version)(&mut raw_version) };
    let version = validate_version(NativeLibraryVersion::from_abi(raw_version))?;

    if required_open_contract == Some(NativeOpenContract::LoaderNeutral) {
        // SAFETY: Presence checks do not call a symbol. The typed resolution
        // below freezes the pinned loader declaration. The receive-only names
        // are also required, but preflight deliberately invokes none of these
        // device-facing functions.
        unsafe { ensure_loader_required_symbols(library) }?;
        let _load_fpga: LoadFpgaFn = unsafe { require_symbol(library, b"bladerf_load_fpga\0") }?;
    }

    if let Some(required) = required_open_contract {
        // SAFETY: This type is the patched C declaration for either named
        // marker. Both functions take no arguments, return a fixed-width value
        // and are explicitly device-independent.
        let marker_fn: OpenMarkerFn = unsafe { require_symbol(library, required.symbol()) }?;
        // SAFETY: The hash-qualified patched library owns the function pointer;
        // its no-argument ABI and u32 return type are declared above.
        validate_open_contract_marker(required, unsafe { marker_fn() })?;
    }

    Ok((functions, version, required_open_contract))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
unsafe fn inspect_loader_symbols(
    library: &Library,
) -> Result<(LoaderFunctions, NativeLibraryVersion)> {
    // SAFETY: `inspect_symbols` performs the complete common typed resolution,
    // invokes only version/B2L1, and requires the complete typed loader/session
    // receive-only symbol set to be present.
    let (common, version, open_contract) =
        unsafe { inspect_symbols(library, Some(NativeOpenContract::LoaderNeutral)) }?;
    ensure!(
        open_contract == Some(NativeOpenContract::LoaderNeutral),
        "loader-neutral marker was not retained after qualification"
    );
    // SAFETY: These are the pinned declarations already required above. The
    // returned pointers remain valid because `LoaderDynamicCalls` retains this
    // exact `Library` for longer than the function table and device handle.
    let load_fpga = unsafe { require_symbol(library, b"bladerf_load_fpga\0") }?;
    Ok((
        LoaderFunctions {
            open: common.open,
            close: common.close,
            strerror: common.strerror,
            get_serial: common.get_serial,
            get_board_name: common.get_board_name,
            get_fpga_size: common.get_fpga_size,
            load_fpga,
            device_speed: unsafe { require_symbol(library, b"bladerf_device_speed\0") }?,
            get_bias_tee: unsafe { require_symbol(library, b"bladerf_get_bias_tee\0") }?,
            is_fpga_configured: common.is_fpga_configured,
            get_fpga_source: common.get_fpga_source,
            fpga_version: common.fpga_version,
            wishbone_read: common.wishbone_read,
            wishbone_write: common.wishbone_write,
            set_frequency: unsafe { require_symbol(library, b"bladerf_set_frequency\0") }?,
            get_frequency: unsafe { require_symbol(library, b"bladerf_get_frequency\0") }?,
            set_sample_rate: unsafe { require_symbol(library, b"bladerf_set_sample_rate\0") }?,
            get_sample_rate: unsafe { require_symbol(library, b"bladerf_get_sample_rate\0") }?,
            set_bandwidth: unsafe { require_symbol(library, b"bladerf_set_bandwidth\0") }?,
            get_bandwidth: unsafe { require_symbol(library, b"bladerf_get_bandwidth\0") }?,
            set_gain_mode: unsafe { require_symbol(library, b"bladerf_set_gain_mode\0") }?,
            get_gain_mode: unsafe { require_symbol(library, b"bladerf_get_gain_mode\0") }?,
            set_gain: unsafe { require_symbol(library, b"bladerf_set_gain\0") }?,
            get_gain: unsafe { require_symbol(library, b"bladerf_get_gain\0") }?,
            get_loopback: unsafe { require_symbol(library, b"bladerf_get_loopback\0") }?,
            set_rx_mux: unsafe { require_symbol(library, b"bladerf_set_rx_mux\0") }?,
            get_rx_mux: unsafe { require_symbol(library, b"bladerf_get_rx_mux\0") }?,
            sync_config: unsafe { require_symbol(library, b"bladerf_sync_config\0") }?,
            enable_module: unsafe { require_symbol(library, b"bladerf_enable_module\0") }?,
            sync_rx: unsafe { require_symbol(library, b"bladerf_sync_rx\0") }?,
        },
        version,
    ))
}

fn validate_version(version: NativeLibraryVersion) -> Result<NativeLibraryVersion> {
    ensure!(
        version
            == (NativeLibraryVersion {
                major: EXPECTED_MAJOR,
                minor: EXPECTED_MINOR,
                patch: EXPECTED_PATCH,
            }),
        "libbladeRF version {version} differs from required {EXPECTED_MAJOR}.{EXPECTED_MINOR}.{EXPECTED_PATCH}"
    );
    Ok(version)
}

fn validate_open_contract_marker(contract: NativeOpenContract, marker: u32) -> Result<u32> {
    let expected = contract.marker();
    ensure!(
        marker == expected,
        "bladeRF 2 {} open marker {marker:#010x} differs from required {expected:#010x}",
        contract.description()
    );
    Ok(marker)
}

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    use std::cell::RefCell;
    use std::fs;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    use std::rc::Rc;

    use super::*;

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const TEST_SERIAL: &str = "0123456789abcdef0123456789abcdef";

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    struct FakeCalls {
        events: Rc<RefCell<Vec<String>>>,
        open_status: c_int,
        open_handle: Option<u8>,
        error_text: Option<String>,
        serial_status: c_int,
        serial: [u8; SERIAL_BYTES],
        board_name: Option<Vec<u8>>,
        fpga_size_status: c_int,
        fpga_size: c_int,
        configured: c_int,
        fpga_source_status: c_int,
        fpga_source: c_int,
        fpga_version_status: c_int,
        fpga_version: NativeFpgaVersion,
        wishbone_read_status: c_int,
        profile_id_status: c_int,
        capabilities_status: c_int,
        profile_id: u32,
        capabilities: u32,
        wishbone_write_status: c_int,
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl FakeCalls {
        fn valid(events: Rc<RefCell<Vec<String>>>) -> Self {
            let mut serial = [0u8; SERIAL_BYTES];
            serial[..SERIAL_BYTES - 1].copy_from_slice(TEST_SERIAL.as_bytes());
            Self {
                events,
                open_status: 0,
                open_handle: Some(7),
                error_text: Some("injected native error".to_owned()),
                serial_status: 0,
                serial,
                board_name: Some(b"bladerf2".to_vec()),
                fpga_size_status: 0,
                fpga_size: BLADERF_FPGA_A9,
                configured: 1,
                fpga_source_status: 0,
                fpga_source: BLADERF_FPGA_SOURCE_HOST,
                fpga_version_status: 0,
                fpga_version: NativeFpgaVersion::PROFILE2_REQUIRED,
                wishbone_read_status: 0,
                profile_id_status: 0,
                capabilities_status: 0,
                profile_id: PROFILE2_ID,
                capabilities: PROFILE2_CAPABILITIES,
                wishbone_write_status: 0,
            }
        }

        fn event(&self, event: impl Into<String>) {
            self.events.borrow_mut().push(event.into());
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl NativeCalls for FakeCalls {
        type Handle = u8;

        fn open(&mut self, identifier: &CStr) -> (c_int, Option<Self::Handle>) {
            self.event(format!("open:{}", identifier.to_string_lossy()));
            (self.open_status, self.open_handle)
        }

        fn close(&mut self, handle: Self::Handle) {
            self.event(format!("close:{handle}"));
        }

        fn error_text(&self, status: c_int) -> Option<String> {
            self.event(format!("error:{status}"));
            self.error_text.clone()
        }

        fn get_serial(&mut self, handle: Self::Handle, serial: &mut [u8; SERIAL_BYTES]) -> c_int {
            self.event(format!("serial:{handle}"));
            *serial = self.serial;
            self.serial_status
        }

        fn get_board_name(&mut self, handle: Self::Handle) -> Option<Vec<u8>> {
            self.event(format!("board:{handle}"));
            self.board_name.clone()
        }

        fn get_fpga_size(&mut self, handle: Self::Handle, size: &mut c_int) -> c_int {
            self.event(format!("size:{handle}"));
            *size = self.fpga_size;
            self.fpga_size_status
        }

        fn is_fpga_configured(&mut self, handle: Self::Handle) -> c_int {
            self.event(format!("configured:{handle}"));
            self.configured
        }

        fn get_fpga_source(&mut self, handle: Self::Handle, source: &mut c_int) -> c_int {
            self.event(format!("source:{handle}"));
            *source = self.fpga_source;
            self.fpga_source_status
        }

        fn fpga_version(&mut self, handle: Self::Handle, version: &mut NativeFpgaVersion) -> c_int {
            self.event(format!("fpga-version:{handle}"));
            *version = self.fpga_version;
            self.fpga_version_status
        }

        fn wishbone_read(&mut self, handle: Self::Handle, address: u32, value: &mut u32) -> c_int {
            self.event(format!("read:{handle}:{address:#04x}"));
            *value = match address {
                0x00 => self.profile_id,
                0x04 => self.capabilities,
                _ => 0xfeed_beef,
            };
            match address {
                0x00 => self.profile_id_status,
                0x04 => self.capabilities_status,
                _ => self.wishbone_read_status,
            }
        }

        fn wishbone_write(&mut self, handle: Self::Handle, address: u32, value: u32) -> c_int {
            self.event(format!("write:{handle}:{address:#04x}:{value:#010x}"));
            self.wishbone_write_status
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeSessionOperation {
        GetBias(c_int),
        GetLoopback,
        SetRxMux,
        GetRxMux,
        SetGainMode,
        GetGainMode,
        SetFrequency,
        GetFrequency,
        SetSampleRate,
        GetSampleRate,
        SetBandwidth,
        GetBandwidth,
        SetGain,
        GetGain,
        SyncConfig,
        Enable,
        Disable,
        SyncRx(usize),
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    struct FakeSessionApi {
        events: RefCell<Vec<String>>,
        failures: Vec<(FakeSessionOperation, c_int)>,
        device_speed: c_int,
        rx_bias_enabled: bool,
        tx_bias_enabled: bool,
        loopback: c_int,
        rx_mux: c_int,
        gain_mode: c_int,
        frequency_hz: u64,
        actual_sample_rate_hz: u32,
        sample_rate_hz: u32,
        actual_bandwidth_hz: u32,
        bandwidth_hz: u32,
        gain_db: c_int,
        sync_rx_calls: usize,
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl FakeSessionApi {
        fn valid() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                failures: Vec::new(),
                device_speed: BLADERF_DEVICE_SPEED_SUPER,
                rx_bias_enabled: false,
                tx_bias_enabled: false,
                loopback: BLADERF_LB_NONE,
                rx_mux: BLADERF_RX_MUX_BASEBAND,
                gain_mode: BLADERF_GAIN_MGC,
                frequency_hz: 98_100_000,
                actual_sample_rate_hz: SOURCE_SAMPLE_RATE_HZ,
                sample_rate_hz: SOURCE_SAMPLE_RATE_HZ,
                actual_bandwidth_hz: SOURCE_BANDWIDTH_HZ,
                bandwidth_hz: SOURCE_BANDWIDTH_HZ,
                gain_db: 40,
                sync_rx_calls: 0,
            }
        }

        fn event(&self, event: impl Into<String>) {
            self.events.borrow_mut().push(event.into());
        }

        fn inject_failure(&mut self, operation: FakeSessionOperation, status: c_int) {
            assert_ne!(status, 0);
            assert!(
                self.failures
                    .iter()
                    .all(|(existing, _)| *existing != operation),
                "duplicate fake failure injection for {operation:?}"
            );
            self.failures.push((operation, status));
        }

        fn status_for(&self, operation: FakeSessionOperation) -> c_int {
            self.failures
                .iter()
                .find_map(|(candidate, status)| (*candidate == operation).then_some(*status))
                .unwrap_or(0)
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl SessionNativeApi for FakeSessionApi {
        type HandleToken = u8;

        fn status_text(&self, status: c_int) -> Option<String> {
            self.event(format!("status-text:{status}"));
            Some(format!("injected session error {status}"))
        }

        fn device_speed(&mut self, handle: Self::HandleToken) -> c_int {
            self.event(format!("device-speed:{handle}"));
            self.device_speed
        }

        fn get_bias_tee(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            enabled: &mut bool,
        ) -> c_int {
            self.event(format!("get-bias:{handle}:{channel}"));
            *enabled = match channel {
                BLADERF_CHANNEL_RX1 => self.rx_bias_enabled,
                BLADERF_CHANNEL_TX1 => self.tx_bias_enabled,
                _ => panic!("unexpected fake bias-tee channel {channel}"),
            };
            self.status_for(FakeSessionOperation::GetBias(channel))
        }

        fn get_loopback(&mut self, handle: Self::HandleToken, loopback: &mut c_int) -> c_int {
            self.event(format!("get-loopback:{handle}"));
            *loopback = self.loopback;
            self.status_for(FakeSessionOperation::GetLoopback)
        }

        fn set_rx_mux(&mut self, handle: Self::HandleToken, mux: c_int) -> c_int {
            self.event(format!("set-rx-mux:{handle}:{mux}"));
            self.status_for(FakeSessionOperation::SetRxMux)
        }

        fn get_rx_mux(&mut self, handle: Self::HandleToken, mux: &mut c_int) -> c_int {
            self.event(format!("get-rx-mux:{handle}"));
            *mux = self.rx_mux;
            self.status_for(FakeSessionOperation::GetRxMux)
        }

        fn set_gain_mode(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            mode: c_int,
        ) -> c_int {
            self.event(format!("set-gain-mode:{handle}:{channel}:{mode}"));
            self.status_for(FakeSessionOperation::SetGainMode)
        }

        fn get_gain_mode(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            mode: &mut c_int,
        ) -> c_int {
            self.event(format!("get-gain-mode:{handle}:{channel}"));
            *mode = self.gain_mode;
            self.status_for(FakeSessionOperation::GetGainMode)
        }

        fn set_frequency(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            frequency_hz: u64,
        ) -> c_int {
            self.event(format!("set-frequency:{handle}:{channel}:{frequency_hz}"));
            self.status_for(FakeSessionOperation::SetFrequency)
        }

        fn get_frequency(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            frequency_hz: &mut u64,
        ) -> c_int {
            self.event(format!("get-frequency:{handle}:{channel}"));
            *frequency_hz = self.frequency_hz;
            self.status_for(FakeSessionOperation::GetFrequency)
        }

        fn set_sample_rate(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            sample_rate_hz: u32,
            actual_sample_rate_hz: &mut u32,
        ) -> c_int {
            self.event(format!(
                "set-sample-rate:{handle}:{channel}:{sample_rate_hz}"
            ));
            *actual_sample_rate_hz = self.actual_sample_rate_hz;
            self.status_for(FakeSessionOperation::SetSampleRate)
        }

        fn get_sample_rate(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            sample_rate_hz: &mut u32,
        ) -> c_int {
            self.event(format!("get-sample-rate:{handle}:{channel}"));
            *sample_rate_hz = self.sample_rate_hz;
            self.status_for(FakeSessionOperation::GetSampleRate)
        }

        fn set_bandwidth(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            bandwidth_hz: u32,
            actual_bandwidth_hz: &mut u32,
        ) -> c_int {
            self.event(format!("set-bandwidth:{handle}:{channel}:{bandwidth_hz}"));
            *actual_bandwidth_hz = self.actual_bandwidth_hz;
            self.status_for(FakeSessionOperation::SetBandwidth)
        }

        fn get_bandwidth(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            bandwidth_hz: &mut u32,
        ) -> c_int {
            self.event(format!("get-bandwidth:{handle}:{channel}"));
            *bandwidth_hz = self.bandwidth_hz;
            self.status_for(FakeSessionOperation::GetBandwidth)
        }

        fn set_gain(&mut self, handle: Self::HandleToken, channel: c_int, gain_db: c_int) -> c_int {
            self.event(format!("set-gain:{handle}:{channel}:{gain_db}"));
            self.status_for(FakeSessionOperation::SetGain)
        }

        fn get_gain(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            gain_db: &mut c_int,
        ) -> c_int {
            self.event(format!("get-gain:{handle}:{channel}"));
            *gain_db = self.gain_db;
            self.status_for(FakeSessionOperation::GetGain)
        }

        fn sync_config(
            &mut self,
            handle: Self::HandleToken,
            layout: c_int,
            sample_format: c_int,
            num_buffers: u32,
            buffer_samples: u32,
            num_transfers: u32,
            timeout_ms: u32,
        ) -> c_int {
            self.event(format!(
                "sync-config:{handle}:{layout}:{sample_format}:{num_buffers}:{buffer_samples}:{num_transfers}:{timeout_ms}"
            ));
            self.status_for(FakeSessionOperation::SyncConfig)
        }

        fn enable_module(
            &mut self,
            handle: Self::HandleToken,
            channel: c_int,
            enable: bool,
        ) -> c_int {
            self.event(format!("enable:{handle}:{channel}:{enable}"));
            if enable {
                self.status_for(FakeSessionOperation::Enable)
            } else {
                self.status_for(FakeSessionOperation::Disable)
            }
        }

        fn sync_rx(
            &mut self,
            handle: Self::HandleToken,
            samples: &mut [u32],
            timeout_ms: u32,
        ) -> c_int {
            self.sync_rx_calls += 1;
            self.event(format!("sync-rx:{handle}:{}:{timeout_ms}", samples.len()));
            let status = self.status_for(FakeSessionOperation::SyncRx(self.sync_rx_calls));
            if status != 0 {
                return status;
            }
            samples.fill(u32::from_ne_bytes([0, 1, 2, 3]));
            0
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    struct RecordingWriter {
        bytes: Vec<u8>,
        fail_write: bool,
        fail_flush: bool,
        flushes: usize,
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl RecordingWriter {
        fn successful() -> Self {
            Self {
                bytes: Vec::new(),
                fail_write: false,
                fail_flush: false,
                flushes: 0,
            }
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::other("injected capture write failure"));
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.fail_flush {
                Err(std::io::Error::other("injected capture flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn fake_receive_plan(channel: ReceiveChannel) -> ReceivePlan {
        ReceivePlan::new(channel, 98_100_000, 40, 100_000).unwrap()
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const FAKE_CONFIGURE_EVENTS: [&str; 16] = [
        "device-speed:7",
        "get-bias:7:0",
        "get-bias:7:1",
        "get-loopback:7",
        "set-rx-mux:7:0",
        "get-rx-mux:7",
        "set-gain-mode:7:2:1",
        "get-gain-mode:7:2",
        "set-frequency:7:2:98100000",
        "get-frequency:7:2",
        "set-sample-rate:7:2:20000000",
        "get-sample-rate:7:2",
        "set-bandwidth:7:2:20000000",
        "get-bandwidth:7:2",
        "set-gain:7:2:40",
        "sync-config:7:0:0:128:16384:32:5000",
    ];

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_configuration_uses_the_fixed_guarded_call_order() {
        let mut calls = FakeSessionApi::valid();
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let mut readback =
            ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, u64::MAX, u32::MAX, u32::MAX);

        configure_receive_native(&mut calls, 7, plan, &mut readback).unwrap();

        assert_eq!(readback.channel(), ReceiveChannel::Rx2);
        assert_eq!(readback.center_frequency_hz(), 98_100_000);
        assert_eq!(readback.sample_rate_hz(), SOURCE_SAMPLE_RATE_HZ);
        assert_eq!(readback.bandwidth_hz(), SOURCE_BANDWIDTH_HZ);
        let events = calls.events.borrow();
        assert_eq!(
            events.iter().map(String::as_str).collect::<Vec<_>>(),
            FAKE_CONFIGURE_EVENTS
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn every_native_receive_configuration_status_failure_stops_at_the_exact_call() {
        let cases = [
            (FakeSessionOperation::GetBias(BLADERF_CHANNEL_RX1), 1),
            (FakeSessionOperation::GetBias(BLADERF_CHANNEL_TX1), 2),
            (FakeSessionOperation::GetLoopback, 3),
            (FakeSessionOperation::SetRxMux, 4),
            (FakeSessionOperation::GetRxMux, 5),
            (FakeSessionOperation::SetGainMode, 6),
            (FakeSessionOperation::GetGainMode, 7),
            (FakeSessionOperation::SetFrequency, 8),
            (FakeSessionOperation::GetFrequency, 9),
            (FakeSessionOperation::SetSampleRate, 10),
            (FakeSessionOperation::GetSampleRate, 11),
            (FakeSessionOperation::SetBandwidth, 12),
            (FakeSessionOperation::GetBandwidth, 13),
            (FakeSessionOperation::SetGain, 14),
            (FakeSessionOperation::SyncConfig, 15),
        ];

        for (operation, event_index) in cases {
            let mut calls = FakeSessionApi::valid();
            calls.inject_failure(operation, -31);
            let plan = fake_receive_plan(ReceiveChannel::Rx2);
            let sentinel =
                ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, u64::MAX, u32::MAX, u32::MAX);
            let mut readback = sentinel;

            let error = configure_receive_native(&mut calls, 7, plan, &mut readback).unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains("status -31"),
                "{operation:?}: {diagnostic}"
            );
            assert_eq!(readback, sentinel, "{operation:?}");

            let mut expected = FAKE_CONFIGURE_EVENTS[..=event_index].to_vec();
            expected.push("status-text:-31");
            assert_eq!(
                calls
                    .events
                    .borrow()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected,
                "{operation:?}"
            );
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn every_native_receive_configuration_value_mismatch_stops_at_the_exact_call() {
        #[derive(Clone, Copy, Debug)]
        enum Mismatch {
            RxMux,
            GainMode,
            Frequency,
            ActualSampleRate,
            SampleRateReadback,
            ActualBandwidth,
            BandwidthReadback,
        }

        let cases = [
            (Mismatch::RxMux, 5, "RX mux"),
            (Mismatch::GainMode, 7, "gain mode"),
            (Mismatch::Frequency, 9, "center-frequency"),
            (Mismatch::ActualSampleRate, 10, "actual sample rate"),
            (Mismatch::SampleRateReadback, 11, "sample-rate readback"),
            (Mismatch::ActualBandwidth, 12, "actual bandwidth"),
            (Mismatch::BandwidthReadback, 13, "bandwidth readback"),
        ];

        for (mismatch, event_index, diagnostic_fragment) in cases {
            let mut calls = FakeSessionApi::valid();
            match mismatch {
                Mismatch::RxMux => calls.rx_mux = BLADERF_RX_MUX_BASEBAND + 1,
                Mismatch::GainMode => calls.gain_mode = BLADERF_GAIN_MGC + 1,
                Mismatch::Frequency => {
                    calls.frequency_hz = 98_100_000 + MAX_FREQUENCY_ERROR_HZ + 1;
                }
                Mismatch::ActualSampleRate => {
                    calls.actual_sample_rate_hz = SOURCE_SAMPLE_RATE_HZ + 1;
                }
                Mismatch::SampleRateReadback => {
                    calls.sample_rate_hz = SOURCE_SAMPLE_RATE_HZ + 1;
                }
                Mismatch::ActualBandwidth => {
                    calls.actual_bandwidth_hz = SOURCE_BANDWIDTH_HZ + 1;
                }
                Mismatch::BandwidthReadback => {
                    calls.bandwidth_hz = SOURCE_BANDWIDTH_HZ + 1;
                }
            }
            let plan = fake_receive_plan(ReceiveChannel::Rx2);
            let sentinel =
                ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, u64::MAX, u32::MAX, u32::MAX);
            let mut readback = sentinel;

            let error = configure_receive_native(&mut calls, 7, plan, &mut readback).unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains(diagnostic_fragment),
                "{mismatch:?}: {diagnostic}"
            );
            assert_eq!(readback, sentinel, "{mismatch:?}");
            assert_eq!(
                calls
                    .events
                    .borrow()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                FAKE_CONFIGURE_EVENTS[..=event_index],
                "{mismatch:?}"
            );
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_safety_guards_fail_before_the_first_mutation() {
        let cases: &[(c_int, bool, bool, c_int, &[&str])] = &[
            (1, false, false, BLADERF_LB_NONE, &["device-speed:7"]),
            (
                BLADERF_DEVICE_SPEED_SUPER,
                true,
                false,
                BLADERF_LB_NONE,
                &["device-speed:7", "get-bias:7:0"],
            ),
            (
                BLADERF_DEVICE_SPEED_SUPER,
                false,
                true,
                BLADERF_LB_NONE,
                &["device-speed:7", "get-bias:7:0", "get-bias:7:1"],
            ),
            (
                BLADERF_DEVICE_SPEED_SUPER,
                false,
                false,
                2,
                &[
                    "device-speed:7",
                    "get-bias:7:0",
                    "get-bias:7:1",
                    "get-loopback:7",
                ],
            ),
        ];

        for &(speed, rx_bias, tx_bias, loopback, expected) in cases {
            let mut calls = FakeSessionApi::valid();
            calls.device_speed = speed;
            calls.rx_bias_enabled = rx_bias;
            calls.tx_bias_enabled = tx_bias;
            calls.loopback = loopback;
            let plan = fake_receive_plan(ReceiveChannel::Rx2);
            let sentinel =
                ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, u64::MAX, u32::MAX, u32::MAX);
            let mut readback = sentinel;

            assert!(
                configure_receive_native(&mut calls, 7, plan, &mut readback).is_err(),
                "speed={speed} rx_bias={rx_bias} tx_bias={tx_bias} loopback={loopback}"
            );
            assert_eq!(readback, sentinel);
            let events = calls.events.borrow();
            assert_eq!(
                events.iter().map(String::as_str).collect::<Vec<_>>(),
                expected
            );
            assert!(events.iter().all(|event| !event.starts_with("set-")));
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_start_enables_before_active_readback() {
        let mut calls = FakeSessionApi::valid();
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let sentinel = ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, 1, 2, 3);
        let mut readback = ActiveReceiveReadback::new(sentinel, i32::MIN);

        start_receive_native(&mut calls, 7, plan, &mut readback).unwrap();

        assert_eq!(readback.configured().channel(), ReceiveChannel::Rx2);
        assert_eq!(readback.configured().center_frequency_hz(), 98_100_000);
        assert_eq!(
            readback.configured().sample_rate_hz(),
            SOURCE_SAMPLE_RATE_HZ
        );
        assert_eq!(readback.configured().bandwidth_hz(), SOURCE_BANDWIDTH_HZ);
        assert_eq!(readback.gain_db(), 40);
        let expected = [
            "enable:7:2:true",
            "get-gain:7:2",
            "get-frequency:7:2",
            "get-sample-rate:7:2",
            "get-bandwidth:7:2",
        ];
        assert_eq!(
            calls
                .events
                .borrow()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_start_cleans_up_only_after_a_successful_enable() {
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let sentinel = ActiveReceiveReadback::new(
            ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, 1, 2, 3),
            i32::MIN,
        );

        let mut enable_failure = FakeSessionApi::valid();
        enable_failure.inject_failure(FakeSessionOperation::Enable, -8);
        enable_failure.inject_failure(FakeSessionOperation::Disable, -9);
        let mut readback = sentinel;
        let error = start_receive_native(&mut enable_failure, 7, plan, &mut readback).unwrap_err();
        assert!(format!("{error:#}").contains("status -8"));
        assert_eq!(readback, sentinel);
        assert_eq!(
            enable_failure.events.into_inner(),
            ["enable:7:2:true", "status-text:-8"].map(str::to_owned)
        );

        let mut readback_failure = FakeSessionApi::valid();
        readback_failure.inject_failure(FakeSessionOperation::GetGain, -7);
        readback_failure.inject_failure(FakeSessionOperation::Disable, -9);
        let mut readback = sentinel;
        let error =
            start_receive_native(&mut readback_failure, 7, plan, &mut readback).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("read active RX gain"), "{diagnostic}");
        assert!(diagnostic.contains("status -7"), "{diagnostic}");
        assert!(diagnostic.contains("cleanup also failed"), "{diagnostic}");
        assert!(diagnostic.contains("status -9"), "{diagnostic}");
        assert!(
            error
                .chain()
                .skip(1)
                .any(|source| source.to_string().contains("read active RX gain"))
        );
        assert_eq!(readback, sentinel);
        assert_eq!(
            readback_failure.events.into_inner(),
            [
                "enable:7:2:true",
                "get-gain:7:2",
                "status-text:-7",
                "enable:7:2:false",
                "status-text:-9",
            ]
            .map(str::to_owned)
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn active_rf_readback_failures_and_mismatches_disable_the_selected_rx() {
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let sentinel = ActiveReceiveReadback::new(
            ConfiguredReceiveReadback::new(ReceiveChannel::Rx1, 1, 2, 3),
            i32::MIN,
        );
        let status_cases: &[(FakeSessionOperation, &[&str])] = &[
            (
                FakeSessionOperation::GetFrequency,
                &["enable:7:2:true", "get-gain:7:2", "get-frequency:7:2"],
            ),
            (
                FakeSessionOperation::GetSampleRate,
                &[
                    "enable:7:2:true",
                    "get-gain:7:2",
                    "get-frequency:7:2",
                    "get-sample-rate:7:2",
                ],
            ),
            (
                FakeSessionOperation::GetBandwidth,
                &[
                    "enable:7:2:true",
                    "get-gain:7:2",
                    "get-frequency:7:2",
                    "get-sample-rate:7:2",
                    "get-bandwidth:7:2",
                ],
            ),
        ];

        for &(operation, prefix) in status_cases {
            let mut calls = FakeSessionApi::valid();
            calls.inject_failure(operation, -42);
            let mut readback = sentinel;
            let error = start_receive_native(&mut calls, 7, plan, &mut readback).unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains("status -42"),
                "{operation:?}: {diagnostic}"
            );
            assert_eq!(readback, sentinel, "{operation:?}");

            let mut expected = prefix.to_vec();
            expected.extend(["status-text:-42", "enable:7:2:false"]);
            assert_eq!(
                calls
                    .events
                    .borrow()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected,
                "{operation:?}"
            );
        }

        #[derive(Clone, Copy, Debug)]
        enum Mismatch {
            Gain,
            Frequency,
            SampleRate,
            Bandwidth,
        }
        let mismatch_cases: &[(Mismatch, &str, &[&str])] = &[
            (
                Mismatch::Gain,
                "active RX gain",
                &["enable:7:2:true", "get-gain:7:2", "enable:7:2:false"],
            ),
            (
                Mismatch::Frequency,
                "center-frequency",
                &[
                    "enable:7:2:true",
                    "get-gain:7:2",
                    "get-frequency:7:2",
                    "enable:7:2:false",
                ],
            ),
            (
                Mismatch::SampleRate,
                "sample-rate",
                &[
                    "enable:7:2:true",
                    "get-gain:7:2",
                    "get-frequency:7:2",
                    "get-sample-rate:7:2",
                    "enable:7:2:false",
                ],
            ),
            (
                Mismatch::Bandwidth,
                "bandwidth",
                &[
                    "enable:7:2:true",
                    "get-gain:7:2",
                    "get-frequency:7:2",
                    "get-sample-rate:7:2",
                    "get-bandwidth:7:2",
                    "enable:7:2:false",
                ],
            ),
        ];

        for &(mismatch, diagnostic_fragment, expected) in mismatch_cases {
            let mut calls = FakeSessionApi::valid();
            match mismatch {
                Mismatch::Gain => calls.gain_db = plan.gain_db() + 1,
                Mismatch::Frequency => {
                    calls.frequency_hz = plan.center_hz() + MAX_FREQUENCY_ERROR_HZ + 1;
                }
                Mismatch::SampleRate => calls.sample_rate_hz = SOURCE_SAMPLE_RATE_HZ + 1,
                Mismatch::Bandwidth => calls.bandwidth_hz = SOURCE_BANDWIDTH_HZ + 1,
            }
            let mut readback = sentinel;
            let error = start_receive_native(&mut calls, 7, plan, &mut readback).unwrap_err();
            let diagnostic = format!("{error:#}");
            assert!(
                diagnostic.contains(diagnostic_fragment),
                "{mismatch:?}: {diagnostic}"
            );
            assert_eq!(readback, sentinel, "{mismatch:?}");
            assert_eq!(
                calls
                    .events
                    .borrow()
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                expected,
                "{mismatch:?}"
            );
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_capture_chunks_and_hashes_the_exact_extent() {
        let mut calls = FakeSessionApi::valid();
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let mut output = RecordingWriter::successful();

        let report = capture_receive_native(&mut calls, 7, plan, &mut output).unwrap();

        assert_eq!(report.samples(), 800_008);
        assert_eq!(report.bytes(), 3_200_032);
        assert_eq!(output.bytes.len(), 3_200_032);
        assert_eq!(output.flushes, 1);
        let (samples, remainder) = output.bytes.as_chunks::<4>();
        assert!(remainder.is_empty());
        assert!(samples.iter().all(|sample| sample == &[0, 1, 2, 3]));
        assert_eq!(
            report.sha256(),
            format!("{:x}", Sha256::digest(&output.bytes))
        );
        let events = calls.events.borrow();
        assert_eq!(events.len(), 49);
        assert!(
            events[..48]
                .iter()
                .all(|event| event == "sync-rx:7:16384:5000")
        );
        assert_eq!(events[48], "sync-rx:7:13576:5000");
        assert!(events.iter().all(|event| !event.starts_with("enable:")));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_capture_errors_disable_the_selected_rx() {
        let plan = fake_receive_plan(ReceiveChannel::Rx2);

        let mut native_failure = FakeSessionApi::valid();
        native_failure.inject_failure(FakeSessionOperation::SyncRx(2), -12);
        let mut output = RecordingWriter::successful();
        let error = capture_receive_native(&mut native_failure, 7, plan, &mut output).unwrap_err();
        assert!(format!("{error:#}").contains("status -12"));
        assert_eq!(output.bytes.len(), SESSION_BUFFER_SAMPLES as usize * 4);
        assert_eq!(output.flushes, 0);
        assert_eq!(
            native_failure.events.into_inner(),
            [
                "sync-rx:7:16384:5000",
                "sync-rx:7:16384:5000",
                "status-text:-12",
                "enable:7:2:false",
            ]
            .map(str::to_owned)
        );

        let mut write_failure = FakeSessionApi::valid();
        let mut output = RecordingWriter::successful();
        output.fail_write = true;
        let error = capture_receive_native(&mut write_failure, 7, plan, &mut output).unwrap_err();
        assert!(format!("{error:#}").contains("write completed profile-2 capture chunk"));
        assert_eq!(output.flushes, 0);
        assert_eq!(
            write_failure.events.into_inner(),
            ["sync-rx:7:16384:5000", "enable:7:2:false"].map(str::to_owned)
        );

        let mut flush_failure = FakeSessionApi::valid();
        let mut output = RecordingWriter::successful();
        output.fail_flush = true;
        let error = capture_receive_native(&mut flush_failure, 7, plan, &mut output).unwrap_err();
        assert!(format!("{error:#}").contains("flush completed profile-2 capture"));
        assert_eq!(output.flushes, 1);
        let events = flush_failure.events.into_inner();
        assert_eq!(events.len(), 50);
        assert_eq!(events.last().unwrap(), "enable:7:2:false");
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn capture_cleanup_failure_preserves_primary_and_cleanup_diagnostics() {
        let mut calls = FakeSessionApi::valid();
        calls.inject_failure(FakeSessionOperation::SyncRx(1), -12);
        calls.inject_failure(FakeSessionOperation::Disable, -9);
        let plan = fake_receive_plan(ReceiveChannel::Rx2);
        let mut output = RecordingWriter::successful();

        let error = capture_receive_native(&mut calls, 7, plan, &mut output).unwrap_err();

        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("cleanup also failed"), "{diagnostic}");
        assert!(diagnostic.contains("status -9"), "{diagnostic}");
        assert!(diagnostic.contains("status -12"), "{diagnostic}");
        assert!(error.chain().skip(1).any(|source| {
            source
                .to_string()
                .contains("receive synchronous profile-2 samples")
        }));
        assert_eq!(output.bytes.len(), 0);
        assert_eq!(output.flushes, 0);
        assert_eq!(
            calls.events.into_inner(),
            [
                "sync-rx:7:16384:5000",
                "status-text:-12",
                "enable:7:2:false",
                "status-text:-9",
            ]
            .map(str::to_owned)
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_receive_stop_disables_only_the_selected_rx() {
        for (channel, raw_channel) in [
            (ReceiveChannel::Rx1, BLADERF_CHANNEL_RX1),
            (ReceiveChannel::Rx2, BLADERF_CHANNEL_RX2),
        ] {
            let mut calls = FakeSessionApi::valid();
            stop_receive_native(&mut calls, 7, channel).unwrap();
            assert_eq!(
                calls.events.into_inner(),
                [format!("enable:7:{raw_channel}:false")]
            );
        }

        let mut failure = FakeSessionApi::valid();
        failure.inject_failure(FakeSessionOperation::Disable, -9);
        let error = stop_receive_native(&mut failure, 7, ReceiveChannel::Rx2).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("disable selected RX channel"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("status -9"), "{diagnostic}");
        assert_eq!(
            failure.events.into_inner(),
            ["enable:7:2:false", "status-text:-9"].map(str::to_owned)
        );
    }

    #[test]
    fn expected_digest_requires_exact_lowercase_hex() {
        assert!(validate_expected_sha256(ABC_SHA256).is_ok());
        for invalid in [
            "",
            "abc",
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "ga7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(validate_expected_sha256(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn device_serial_requires_the_complete_canonical_value() {
        assert!(validate_requested_serial("0123456789abcdef0123456789abcdef").is_ok());
        for invalid in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
            "ANY",
        ] {
            assert!(validate_requested_serial(invalid).is_err(), "{invalid}");
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_open_rejects_identity_inputs_before_file_or_device_access() {
        let missing = Path::new("/this/path/must/not/be-opened/libbladeRF.so.2");
        let serial_error = match NativeDevice::open(missing, "not-a-digest", "bad-serial") {
            Ok(_) => panic!("invalid serial unexpectedly opened a native session"),
            Err(error) => error,
        };
        let serial_diagnostic = format!("{serial_error:#}");
        assert!(serial_diagnostic.contains("serial"), "{serial_diagnostic}");
        assert!(
            !serial_diagnostic.contains("No such file"),
            "{serial_diagnostic}"
        );

        let digest_error = match NativeDevice::open(missing, "not-a-digest", TEST_SERIAL) {
            Ok(_) => panic!("invalid digest unexpectedly opened a native session"),
            Err(error) => error,
        };
        let digest_diagnostic = format!("{digest_error:#}");
        assert!(digest_diagnostic.contains("SHA-256"), "{digest_diagnostic}");
        assert!(
            !digest_diagnostic.contains("No such file"),
            "{digest_diagnostic}"
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn returned_serial_requires_exact_bytes_and_terminator() {
        let mut valid = [0u8; SERIAL_BYTES];
        valid[..SERIAL_BYTES - 1].copy_from_slice(TEST_SERIAL.as_bytes());
        assert!(validate_returned_serial(&valid, TEST_SERIAL).is_ok());

        let mut missing_terminator = valid;
        missing_terminator[SERIAL_BYTES - 1] = b'0';
        assert!(validate_returned_serial(&missing_terminator, TEST_SERIAL).is_err());

        let mut interior_nul = valid;
        interior_nul[4] = 0;
        assert!(validate_returned_serial(&interior_nul, TEST_SERIAL).is_err());

        let mut uppercase = valid;
        uppercase[10] = b'A';
        assert!(validate_returned_serial(&uppercase, TEST_SERIAL).is_err());

        let mut invalid_hex = valid;
        invalid_hex[10] = b'g';
        assert!(validate_returned_serial(&invalid_hex, TEST_SERIAL).is_err());

        let mut mismatch = valid;
        mismatch[0] = b'f';
        assert!(validate_returned_serial(&mismatch, TEST_SERIAL).is_err());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_session_validates_identity_before_exposing_register_writes() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let calls = FakeCalls::valid(Rc::clone(&events));
        let mut session =
            open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED)
                .unwrap();

        assert_eq!(
            events.borrow().as_slice(),
            [
                format!("open:libusb:serial={TEST_SERIAL}"),
                "serial:7".to_owned(),
                "board:7".to_owned(),
                "size:7".to_owned(),
                "configured:7".to_owned(),
                "source:7".to_owned(),
                "fpga-version:7".to_owned(),
                "read:7:0x00".to_owned(),
                "read:7:0x04".to_owned(),
            ]
        );
        assert!(
            events
                .borrow()
                .iter()
                .all(|event| !event.starts_with("write:"))
        );

        session.write_register(0x10, 0x1234_5678).unwrap();
        assert_eq!(session.read_register(0x24).unwrap(), 0xfeed_beef);
        session.shutdown();
        session.shutdown();
        assert!(session.read_register(0x00).is_err());

        let events = events.borrow();
        assert!(events.contains(&"write:7:0x10:0x12345678".to_owned()));
        assert_eq!(events.iter().filter(|event| *event == "close:7").count(), 1);
        assert_eq!(events.last().unwrap(), "close:7");
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn runtime_status_failures_never_publish_poisoned_outputs() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let calls = FakeCalls::valid(Rc::clone(&events));
        let mut session =
            open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED)
                .unwrap();

        session.calls.wishbone_read_status = 3;
        session.calls.error_text = None;
        let read_error = session.read_register(0x24).unwrap_err();
        let read_diagnostic = format!("{read_error:#}");
        assert!(read_diagnostic.contains("status 3"), "{read_diagnostic}");
        assert!(
            read_diagnostic.contains("no diagnostic text"),
            "{read_diagnostic}"
        );

        session.calls.wishbone_read_status = 0;
        session.calls.wishbone_write_status = 2;
        let write_error = session.write_register(0x10, 7).unwrap_err();
        assert!(format!("{write_error:#}").contains("status 2"));
        session.shutdown();
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn every_post_open_identity_failure_closes_once_without_writing() {
        let mut cases = Vec::new();

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.serial[0] = b'f';
        cases.push(("serial", calls, events, "serial:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.board_name = Some(b"bladerf1".to_vec());
        cases.push(("board", calls, events, "board:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_size = 77;
        cases.push(("size", calls, events, "size:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.configured = 0;
        cases.push(("configured", calls, events, "configured:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_source = 1;
        cases.push(("source", calls, events, "source:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_version = NativeFpgaVersion {
            major: 0,
            minor: 16,
            patch: 1,
        };
        cases.push(("version", calls, events, "fpga-version:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.profile_id ^= 1;
        cases.push(("profile ID", calls, events, "read:7:0x00"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.capabilities ^= 1;
        cases.push(("capabilities", calls, events, "read:7:0x04"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_size_status = -8;
        calls.fpga_size = BLADERF_FPGA_A9;
        cases.push(("getter status", calls, events, "error:-8"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.serial_status = -2;
        cases.push(("serial status", calls, events, "error:-2"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.board_name = None;
        cases.push(("null board", calls, events, "board:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.configured = -3;
        cases.push(("configured status", calls, events, "error:-3"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.configured = 2;
        cases.push(("unexpected configured value", calls, events, "configured:7"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_source_status = -4;
        cases.push(("source status", calls, events, "error:-4"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.fpga_version_status = -5;
        cases.push(("version status", calls, events, "error:-5"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.profile_id_status = -6;
        cases.push(("profile ID status", calls, events, "error:-6"));

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.capabilities_status = -7;
        cases.push(("capabilities status", calls, events, "error:-7"));

        for (name, calls, events, terminal_event) in cases {
            let result =
                open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED);
            assert!(result.is_err(), "{name}");
            let events = events.borrow();
            assert_eq!(
                events.iter().filter(|event| *event == "close:7").count(),
                1,
                "{name}: {events:?}"
            );
            assert_eq!(events.last().map(String::as_str), Some("close:7"), "{name}");
            assert_eq!(
                events.get(events.len() - 2).map(String::as_str),
                Some(terminal_event),
                "{name}: {events:?}"
            );
            assert!(
                events.iter().all(|event| !event.starts_with("write:")),
                "{name}: {events:?}"
            );
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn abnormal_open_results_preserve_status_and_handle_ownership() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.open_status = -7;
        let error = match open_validated_session(
            calls,
            TEST_SERIAL,
            NativeFpgaVersion::PROFILE2_REQUIRED,
        ) {
            Ok(_) => panic!("open failure unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("status -7"), "{error:#}");
        assert_eq!(
            events.borrow().as_slice(),
            [
                format!("open:libusb:serial={TEST_SERIAL}"),
                "error:-7".to_owned(),
                "close:7".to_owned(),
            ]
        );

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.open_status = -9;
        calls.open_handle = None;
        let result =
            open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED);
        assert!(result.is_err());
        assert_eq!(
            events.borrow().as_slice(),
            [
                format!("open:libusb:serial={TEST_SERIAL}"),
                "error:-9".to_owned(),
            ]
        );

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut calls = FakeCalls::valid(Rc::clone(&events));
        calls.open_handle = None;
        let result =
            open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED);
        assert!(result.is_err());
        assert_eq!(
            events.borrow().as_slice(),
            [format!("open:libusb:serial={TEST_SERIAL}")]
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_session_drop_closes_the_handle_once() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let calls = FakeCalls::valid(Rc::clone(&events));
        let session =
            open_validated_session(calls, TEST_SERIAL, NativeFpgaVersion::PROFILE2_REQUIRED)
                .unwrap();
        drop(session);
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|event| *event == "close:7")
                .count(),
            1
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn file_hashing_is_exact_and_rejects_empty_oversized_or_mismatched_files() {
        let directory = tempfile::tempdir().unwrap();
        let valid_path = directory.path().join("libvalid.so");
        fs::write(&valid_path, b"abc").unwrap();
        let qualified = qualify_file(&valid_path, ABC_SHA256).unwrap();
        assert_eq!(qualified.sha256, ABC_SHA256);
        assert_eq!(qualified.canonical_path, valid_path.canonicalize().unwrap());

        let mismatch = qualify_file(&valid_path, EMPTY_SHA256).unwrap_err();
        assert!(mismatch.to_string().contains("SHA-256 mismatch"));

        let empty_path = directory.path().join("empty.so");
        File::create(&empty_path).unwrap();
        assert!(qualify_file(&empty_path, EMPTY_SHA256).is_err());

        let oversized_path = directory.path().join("oversized.so");
        let oversized = File::create(&oversized_path).unwrap();
        oversized.set_len(MAX_LIBRARY_BYTES + 1).unwrap();
        assert!(qualify_file(&oversized_path, EMPTY_SHA256).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_is_rejected_promptly_without_opening_a_reader() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let fifo_path = directory.path().join("library.fifo");
        nix::unistd::mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let worker_path = fifo_path.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _ = sender.send(qualify_file(&worker_path, EMPTY_SHA256));
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("FIFO qualification blocked waiting for a writer");
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );
        worker.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_exact_regular_file_type_is_accepted() {
        assert!(is_regular_file_type(SFlag::S_IFREG));
        for file_type in [
            SFlag::S_IFLNK,
            SFlag::S_IFSOCK,
            SFlag::S_IFIFO,
            SFlag::S_IFCHR,
            SFlag::S_IFDIR,
            SFlag::S_IFBLK,
        ] {
            assert!(!is_regular_file_type(file_type), "{file_type:?}");
        }
    }

    #[test]
    fn only_exact_numeric_library_version_is_accepted() {
        let expected = NativeLibraryVersion {
            major: 2,
            minor: 6,
            patch: 0,
        };
        assert_eq!(validate_version(expected).unwrap(), expected);
        for version in [
            NativeLibraryVersion {
                major: 2,
                minor: 5,
                patch: 0,
            },
            NativeLibraryVersion {
                major: 2,
                minor: 6,
                patch: 1,
            },
            NativeLibraryVersion {
                major: 3,
                minor: 0,
                patch: 0,
            },
        ] {
            assert!(validate_version(version).is_err(), "{version}");
        }
    }

    #[test]
    fn only_exact_specialized_open_markers_are_accepted() {
        for contract in [
            NativeOpenContract::ConfigNeutral,
            NativeOpenContract::LoaderNeutral,
        ] {
            let expected = contract.marker();
            assert_eq!(
                validate_open_contract_marker(contract, expected).unwrap(),
                expected
            );
            let other = match contract {
                NativeOpenContract::ConfigNeutral => BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1,
                NativeOpenContract::LoaderNeutral => BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1,
            };
            for invalid in [0, 1, expected - 1, expected + 1, other, u32::MAX] {
                assert!(
                    validate_open_contract_marker(contract, invalid).is_err(),
                    "{} accepted {invalid:#010x}",
                    contract.description()
                );
            }
        }
    }

    #[test]
    fn report_exposes_at_most_one_specialized_open_contract() {
        let version = NativeLibraryVersion {
            major: EXPECTED_MAJOR,
            minor: EXPECTED_MINOR,
            patch: EXPECTED_PATCH,
        };
        for (open_contract, config_marker, loader_marker) in [
            (None, None, None),
            (
                Some(NativeOpenContract::ConfigNeutral),
                Some(BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1),
                None,
            ),
            (
                Some(NativeOpenContract::LoaderNeutral),
                None,
                Some(BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1),
            ),
        ] {
            let report = NativeLibraryReport {
                path: PathBuf::from("qualified-library"),
                sha256: String::new(),
                version,
                open_contract,
            };
            assert_eq!(report.bladerf2_config_neutral_open_marker(), config_marker);
            assert_eq!(report.bladerf2_loader_neutral_open_marker(), loader_marker);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn staged_bytes_are_immutable_after_source_mutation_and_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("libsource.so");
        fs::write(&source_path, b"abc").unwrap();
        let qualified = qualify_file(&source_path, ABC_SHA256).unwrap();

        fs::write(&source_path, b"xyz").unwrap();
        fs::remove_file(&source_path).unwrap();
        fs::write(&source_path, b"replacement").unwrap();

        let mut staged_reader = qualified.staged_file.try_clone().unwrap();
        staged_reader.seek(SeekFrom::Start(0)).unwrap();
        let mut staged_bytes = Vec::new();
        staged_reader.read_to_end(&mut staged_bytes).unwrap();
        assert_eq!(staged_bytes, b"abc");

        let required = SealFlag::F_SEAL_WRITE
            | SealFlag::F_SEAL_GROW
            | SealFlag::F_SEAL_SHRINK
            | SealFlag::F_SEAL_SEAL;
        let actual = fcntl(&qualified.staged_file, FcntlArg::F_GET_SEALS).unwrap();
        assert_eq!(actual & required.bits(), required.bits());

        let mut staged_writer = qualified.staged_file.try_clone().unwrap();
        staged_writer.seek(SeekFrom::End(0)).unwrap();
        assert!(staged_writer.write_all(b"changed").is_err());
        assert!(staged_writer.set_len(0).is_err());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn pinned_abi_layouts_match_x86_64_linux_header_contract() {
        use std::mem::{align_of, offset_of, size_of};

        assert_eq!(size_of::<BladeRfSerial>(), 33);
        assert_eq!(align_of::<BladeRfSerial>(), 1);
        assert_eq!(offset_of!(BladeRfSerial, serial), 0);

        assert_eq!(size_of::<BladeRfVersion>(), 16);
        assert_eq!(align_of::<BladeRfVersion>(), 8);
        assert_eq!(offset_of!(BladeRfVersion, major), 0);
        assert_eq!(offset_of!(BladeRfVersion, minor), 2);
        assert_eq!(offset_of!(BladeRfVersion, patch), 4);
        assert_eq!(offset_of!(BladeRfVersion, describe), 8);

        assert_eq!(size_of::<c_int>(), 4);
        assert_eq!(align_of::<c_int>(), 4);
        assert_eq!(size_of::<bool>(), 1);
        assert_eq!(size_of::<*mut BladeRf>(), 8);
        assert_eq!(align_of::<*mut BladeRf>(), 8);
        for function_pointer_size in [
            size_of::<DeviceSpeedFn>(),
            size_of::<GetBiasTeeFn>(),
            size_of::<SetFrequencyFn>(),
            size_of::<GetFrequencyFn>(),
            size_of::<SetSampleRateFn>(),
            size_of::<GetSampleRateFn>(),
            size_of::<SetBandwidthFn>(),
            size_of::<GetBandwidthFn>(),
            size_of::<SetGainModeFn>(),
            size_of::<GetGainModeFn>(),
            size_of::<SetGainFn>(),
            size_of::<GetGainFn>(),
            size_of::<GetLoopbackFn>(),
            size_of::<SetRxMuxFn>(),
            size_of::<GetRxMuxFn>(),
            size_of::<SyncConfigFn>(),
            size_of::<EnableModuleFn>(),
            size_of::<SyncRxFn>(),
        ] {
            assert_eq!(function_pointer_size, 8);
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn pinned_receive_literals_match_the_header_and_fixed_stream_contract() {
        assert_eq!(BLADERF_CHANNEL_RX1, 0);
        assert_eq!(BLADERF_CHANNEL_RX2, 2);
        assert_eq!(BLADERF_CHANNEL_TX1, 1);
        assert_eq!(BLADERF_DEVICE_SPEED_SUPER, 2);
        assert_eq!(BLADERF_RX_X1, 0);
        assert_eq!(BLADERF_FORMAT_SC16_Q11, 0);
        assert_eq!(BLADERF_GAIN_MGC, 1);
        assert_eq!(BLADERF_LB_NONE, 0);
        assert_eq!(BLADERF_RX_MUX_BASEBAND, 0);
        assert_eq!(SESSION_NUM_BUFFERS, 128);
        assert_eq!(SESSION_BUFFER_SAMPLES, 16_384);
        assert_eq!(SESSION_NUM_TRANSFERS, 32);
        const { assert!(SESSION_NUM_BUFFERS > SESSION_NUM_TRANSFERS) };
        assert_eq!(SESSION_STREAM_TIMEOUT_MS, 5_000);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn pinned_receive_function_signatures_match_the_header_contract() {
        let _: Option<DeviceSpeedFn> = None::<unsafe extern "C" fn(*mut BladeRf) -> c_int>;
        let _: Option<GetBiasTeeFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut bool) -> c_int>;
        let _: Option<SetFrequencyFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, u64) -> c_int>;
        let _: Option<GetFrequencyFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut u64) -> c_int>;
        let _: Option<SetSampleRateFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, u32, *mut u32) -> c_int>;
        let _: Option<GetSampleRateFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut u32) -> c_int>;
        let _: Option<SetBandwidthFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, u32, *mut u32) -> c_int>;
        let _: Option<GetBandwidthFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut u32) -> c_int>;
        let _: Option<SetGainModeFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, c_int) -> c_int>;
        let _: Option<GetGainModeFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut c_int) -> c_int>;
        let _: Option<SetGainFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, c_int) -> c_int>;
        let _: Option<GetGainFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, *mut c_int) -> c_int>;
        let _: Option<GetLoopbackFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int>;
        let _: Option<SetRxMuxFn> = None::<unsafe extern "C" fn(*mut BladeRf, c_int) -> c_int>;
        let _: Option<GetRxMuxFn> = None::<unsafe extern "C" fn(*mut BladeRf, *mut c_int) -> c_int>;
        let _: Option<SyncConfigFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, c_int, u32, u32, u32, u32) -> c_int>;
        let _: Option<EnableModuleFn> =
            None::<unsafe extern "C" fn(*mut BladeRf, c_int, bool) -> c_int>;
        let _: Option<SyncRxFn> = None::<
            unsafe extern "C" fn(
                *mut BladeRf,
                *mut c_void,
                u32,
                *mut BladeRfMetadata,
                u32,
            ) -> c_int,
        >;
    }

    #[test]
    fn every_required_symbol_is_individually_diagnosed() {
        for missing in REQUIRED_SYMBOLS {
            let error = check_required_symbol_names(|name| {
                ensure!(name != missing, "injected missing symbol");
                Ok(())
            })
            .unwrap_err();
            let expected = printable_symbol(missing);
            assert!(format!("{error:#}").contains(&expected), "{error:#}");
        }
    }

    #[test]
    fn every_loader_required_symbol_is_individually_diagnosed() {
        for missing in LOADER_REQUIRED_SYMBOLS {
            let error = check_loader_required_symbol_names(|name| {
                ensure!(name != missing, "injected missing symbol");
                Ok(())
            })
            .unwrap_err();
            let expected = printable_symbol(missing);
            assert!(format!("{error:#}").contains(&expected), "{error:#}");
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn unrelated_library_is_rejected_for_missing_required_symbols() {
        // SAFETY: This test loads the platform C runtime only to exercise symbol
        // lookup. It invokes no symbol and opens no device.
        let library = unsafe { Library::new("libc.so.6") }.unwrap();
        // SAFETY: `inspect_symbols` validates names before its sole call; libc
        // lacks the first required symbol, so no function is invoked here.
        let error = unsafe { inspect_symbols(&library, None) }
            .err()
            .expect("libc unexpectedly exposed the complete bladeRF ABI");
        assert!(
            format!("{error:#}").contains("bladerf_version"),
            "{error:#}"
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    #[ignore = "requires explicit BLADERF_PROFILE2_LIBRARY and BLADERF_PROFILE2_LIBRARY_SHA256"]
    fn explicitly_selected_real_library_passes_device_free_preflight() {
        let path = std::env::var_os("BLADERF_PROFILE2_LIBRARY")
            .expect("set BLADERF_PROFILE2_LIBRARY to the exact local libbladeRF path");
        let expected = std::env::var("BLADERF_PROFILE2_LIBRARY_SHA256")
            .expect("set BLADERF_PROFILE2_LIBRARY_SHA256 to its lowercase digest");
        let report = check_library(Path::new(&path), &expected).unwrap();
        assert_eq!(report.sha256(), expected);
        assert_eq!(report.version().to_string(), "2.6.0");
        assert!(report.path().is_absolute());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    #[ignore = "requires explicit BLADERF_PROFILE2_NEUTRAL_LIBRARY and BLADERF_PROFILE2_NEUTRAL_LIBRARY_SHA256"]
    fn explicitly_selected_config_neutral_library_passes_device_free_preflight() {
        let path = std::env::var_os("BLADERF_PROFILE2_NEUTRAL_LIBRARY")
            .expect("set BLADERF_PROFILE2_NEUTRAL_LIBRARY to the exact patched library path");
        let expected = std::env::var("BLADERF_PROFILE2_NEUTRAL_LIBRARY_SHA256")
            .expect("set BLADERF_PROFILE2_NEUTRAL_LIBRARY_SHA256 to its lowercase digest");
        let report = check_bladerf2_config_neutral_library(Path::new(&path), &expected).unwrap();
        assert_eq!(report.sha256(), expected);
        assert_eq!(report.version().to_string(), "2.6.0");
        assert_eq!(
            report.bladerf2_config_neutral_open_marker(),
            Some(BLADERF2_CONFIG_NEUTRAL_OPEN_MARKER_V1)
        );
        assert!(report.path().is_absolute());

        // Build and drop the exact retained function table used by native
        // sessions. This still invokes no device function.
        let calls = DynamicCalls::load(Path::new(&path), &expected).unwrap();
        drop(calls);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    #[ignore = "requires explicit BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY and BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY_SHA256"]
    fn explicitly_selected_loader_neutral_library_passes_device_free_preflight() {
        let path = std::env::var_os("BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY").expect(
            "set BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY to the exact patched library path",
        );
        let expected = std::env::var("BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY_SHA256")
            .expect("set BLADERF_PROFILE2_LOADER_NEUTRAL_LIBRARY_SHA256 to its lowercase digest");
        let report = check_bladerf2_loader_neutral_library(Path::new(&path), &expected).unwrap();
        assert_eq!(report.sha256(), expected);
        assert_eq!(report.version().to_string(), "2.6.0");
        assert_eq!(report.bladerf2_config_neutral_open_marker(), None);
        assert_eq!(
            report.bladerf2_loader_neutral_open_marker(),
            Some(BLADERF2_LOADER_NEUTRAL_OPEN_MARKER_V1)
        );
        assert!(report.path().is_absolute());

        // Retain and drop the exact loader table without invoking open, load,
        // or any other device-facing function.
        let (calls, retained_report) =
            prepare_bladerf2_loader_neutral_library(Path::new(&path), &expected).unwrap();
        assert_eq!(retained_report, report);
        drop(calls);
    }
}
