//! Bounded complete-session IPC for the bladeRF profile-2 observatory.
//!
//! [`SessionClient`] owns one child from custom-image load through receive,
//! stock restoration, and shutdown. The adapter owns the device handle across
//! these operations. Build a [`ReceivePlan`] for receive configuration.
//! [`SessionClient::capture`] checks the expected raw extent and independently
//! computed digest before returning the caller's sink.
//! This crate defines its own wire schema for complete-session operations.
//!
//! ```
//! use bladerf_profile2_session_ipc::{
//!     read_frame, write_frame, ReceiveChannel, ReceivePlan, Request,
//!     RequestOperation,
//! };
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let plan = ReceivePlan::new(ReceiveChannel::Rx1, 915_000_000, 20, 100_000)?;
//! let request = Request::new(1, RequestOperation::ConfigureReceive { plan });
//! let mut wire = Vec::new();
//! write_frame(&mut wire, &request)?;
//! let decoded: Request = read_frame(wire.as_slice())?.expect("one frame");
//! assert_eq!(decoded, request);
//! # Ok(())
//! # }
//! # example().unwrap();
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(any(
    target_os = "android",
    target_os = "freebsd",
    target_os = "haiku",
    all(target_os = "linux", not(target_env = "uclibc")),
))]
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
#[cfg(unix)]
use nix::unistd::Pid;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// Current complete-session request and response schema version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Fixed discriminator for the complete-session protocol family.
pub const PROTOCOL_FAMILY: ProtocolFamily = ProtocolFamily::Session;
/// Maximum JSON payload bytes in one request or response frame.
pub const MAX_FRAME_BYTES: usize = 4096;
/// Maximum UTF-8 bytes retained in one server diagnostic.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 512;
/// Maximum requests in one primary complete session.
pub const MAX_PRIMARY_SESSION_REQUESTS: usize = 4_126;
/// Largest accepted deadline for one client transaction.
pub const MAX_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);
/// Exact retained libbladeRF semantic version.
pub const REQUIRED_LIBRARY_VERSION: &str = "2.6.0";
/// Exact B2L1 loader-neutral open-contract marker.
pub const REQUIRED_LIBRARY_MARKER: u32 = 0x4232_4c01;
/// Largest accepted custom or stock image attestation.
pub const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
/// Frozen stock xA9 v0.16.0 image digest.
pub const REQUIRED_STOCK_SHA256: &str =
    "3004467110e2b2790c8f6128f998c2149905a88eed91a87f11af3db524d60c65";
/// Frozen stock xA9 v0.16.0 image byte count.
pub const REQUIRED_STOCK_BYTES: u64 = 12_858_972;
/// Fixed profile-2 commit-register byte offset.
pub const COMMIT_BYTE_OFFSET: u32 = 0x20;
/// Fixed profile-2 commit value (`CMIT`).
pub const COMMIT_MAGIC: u32 = 0x434d_4954;
/// Fixed source sample rate required by this capture profile.
pub const SOURCE_SAMPLE_RATE_HZ: u32 = 20_000_000;
/// Fixed source bandwidth required by this capture profile.
pub const SOURCE_BANDWIDTH_HZ: u32 = 20_000_000;
/// Largest accepted configured-frequency readback error.
pub const MAX_FREQUENCY_ERROR_HZ: u64 = 10;
/// Minimum accepted manual receive gain.
pub const MIN_GAIN_DB: i32 = -16;
/// Maximum accepted manual receive gain.
pub const MAX_GAIN_DB: i32 = 60;
/// Minimum bounded DATA-record count.
pub const MIN_DATA_RECORDS: u64 = 100_000;
/// Maximum bounded DATA-record count.
pub const MAX_DATA_RECORDS: u64 = 10_000_000;
/// Complex transport samples in one CONFIG or DATA record.
pub const TRANSPORT_SAMPLES_PER_RECORD: u64 = 8;
/// Bytes in one SC16 Q11 complex transport sample.
pub const BYTES_PER_TRANSPORT_SAMPLE: u64 = 4;

const FRAME_HEADER_BYTES: usize = 4;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);
const RAW_CHUNK_BYTES: usize = 64 * 1024;

/// Errors while reading or writing a bounded session frame.
#[derive(Debug)]
pub enum FrameError {
    /// The reader or writer failed.
    Io(io::Error),
    /// JSON serialization failed.
    Serialize(serde_json::Error),
    /// JSON decoding failed.
    Deserialize(serde_json::Error),
    /// A frame declared no payload.
    EmptyFrame,
    /// A frame exceeded the protocol bound.
    FrameTooLarge {
        /// Declared or attempted payload size.
        length: usize,
        /// Maximum accepted payload size.
        maximum: usize,
    },
    /// EOF occurred inside the length prefix.
    TruncatedHeader {
        /// Prefix bytes received.
        received: usize,
    },
    /// EOF occurred inside the declared payload.
    TruncatedPayload {
        /// Declared payload bytes.
        expected: usize,
        /// Payload bytes received.
        received: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "session protocol I/O failed: {error}"),
            Self::Serialize(error) => write!(f, "serialize session frame: {error}"),
            Self::Deserialize(error) => write!(f, "decode session frame: {error}"),
            Self::EmptyFrame => f.write_str("session frame payload is empty"),
            Self::FrameTooLarge { length, maximum } => {
                write!(
                    f,
                    "session frame has {length} payload bytes; maximum is {maximum}"
                )
            }
            Self::TruncatedHeader { received } => write!(
                f,
                "session frame ended after {received} of {FRAME_HEADER_BYTES} header bytes"
            ),
            Self::TruncatedPayload { expected, received } => write!(
                f,
                "session frame ended after {received} of {expected} payload bytes"
            ),
        }
    }
}

impl Error for FrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) | Self::Deserialize(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct BoundedPayload {
    bytes: Vec<u8>,
    overflow_length: Option<usize>,
}

impl Write for BoundedPayload {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let attempted = self.bytes.len().saturating_add(input.len());
        if attempted > MAX_FRAME_BYTES {
            self.overflow_length = Some(attempted);
            return Err(io::Error::other("session frame exceeds payload bound"));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Serialize and flush one bounded length-prefixed JSON frame.
///
/// Pass a mutable reference to reuse the writer for additional frames.
///
/// # Errors
/// Returns a framing, serialization, or I/O error. Discard the transport after
/// any error because a partial frame may have been written.
pub fn write_frame<W: Write, T: Serialize + ?Sized>(
    mut writer: W,
    value: &T,
) -> Result<(), FrameError> {
    let mut payload = BoundedPayload {
        bytes: Vec::with_capacity(MAX_FRAME_BYTES),
        overflow_length: None,
    };
    if let Err(error) = serde_json::to_writer(&mut payload, value) {
        if let Some(length) = payload.overflow_length {
            return Err(FrameError::FrameTooLarge {
                length,
                maximum: MAX_FRAME_BYTES,
            });
        }
        return Err(FrameError::Serialize(error));
    }
    let length = u32::try_from(payload.bytes.len()).expect("frame bound fits u32");
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload.bytes)?;
    writer.flush()?;
    Ok(())
}

/// Read one bounded length-prefixed JSON frame, or `None` at a clean EOF.
///
/// Pass a mutable reference to reuse the reader for additional frames.
///
/// # Errors
/// Returns a framing, decoding, or I/O error. No allocation is made until the
/// declared length has passed the fixed bound.
pub fn read_frame<R: Read, T: DeserializeOwned>(mut reader: R) -> Result<Option<T>, FrameError> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    let header_bytes = read_partial(&mut reader, &mut header)?;
    if header_bytes == 0 {
        return Ok(None);
    }
    if header_bytes != FRAME_HEADER_BYTES {
        return Err(FrameError::TruncatedHeader {
            received: header_bytes,
        });
    }
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 {
        return Err(FrameError::EmptyFrame);
    }
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge {
            length,
            maximum: MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0_u8; length];
    let received = read_partial(&mut reader, &mut payload)?;
    if received != length {
        return Err(FrameError::TruncatedPayload {
            expected: length,
            received,
        });
    }
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(FrameError::Deserialize)
}

fn read_partial(reader: &mut impl Read, output: &mut [u8]) -> Result<usize, FrameError> {
    let mut received = 0;
    while received < output.len() {
        match reader.read(&mut output[received..]) {
            Ok(0) => break,
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(received)
}

/// Fixed family encoded in every complete-session envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolFamily {
    /// One-handle profile-2 complete-session protocol.
    #[serde(rename = "bladerf_profile2_session")]
    Session,
}

/// Register access class used in invalid-offset diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterAccess {
    /// Read allowlist.
    Read,
    /// Shadow-write allowlist.
    ShadowWrite,
}

impl fmt::Display for RegisterAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::ShadowWrite => "shadow write",
        })
    }
}

/// Error returned when a raw byte offset is outside a typed allowlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRegisterOffset {
    byte_offset: u32,
    access: RegisterAccess,
}

impl InvalidRegisterOffset {
    /// Rejected byte offset.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        self.byte_offset
    }
    /// Access class which rejected the offset.
    #[must_use]
    pub const fn access(self) -> RegisterAccess {
        self.access
    }
}

impl fmt::Display for InvalidRegisterOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "profile-2 {} byte offset 0x{:02x} is not allowlisted",
            self.access, self.byte_offset
        )
    }
}

impl Error for InvalidRegisterOffset {}

/// Profile-2 registers that may be read by this protocol.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRegister {
    /// Profile identifier and layout version.
    Id,
    /// Frozen capability word.
    Capabilities,
    /// Shadow phase word 0.
    Shadow0,
    /// Shadow phase word 1.
    Shadow1,
    /// Shadow phase word 2.
    Shadow2,
    /// Shadow phase word 3.
    Shadow3,
    /// Busy, outcome, RX-active, reason, and freshness state.
    Status,
    /// Last successfully applied epoch.
    AppliedEpoch,
    /// Epoch associated with the last terminal outcome.
    OutcomeEpoch,
    /// Applied phase word 0.
    Applied0,
    /// Applied phase word 1.
    Applied1,
    /// Applied phase word 2.
    Applied2,
    /// Applied phase word 3.
    Applied3,
}

impl ReadRegister {
    /// Host API byte offset for this register.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        match self {
            Self::Id => 0x00,
            Self::Capabilities => 0x04,
            Self::Shadow0 => 0x10,
            Self::Shadow1 => 0x14,
            Self::Shadow2 => 0x18,
            Self::Shadow3 => 0x1c,
            Self::Status => 0x24,
            Self::AppliedEpoch => 0x28,
            Self::OutcomeEpoch => 0x2c,
            Self::Applied0 => 0x30,
            Self::Applied1 => 0x34,
            Self::Applied2 => 0x38,
            Self::Applied3 => 0x3c,
        }
    }
}

impl TryFrom<u32> for ReadRegister {
    type Error = InvalidRegisterOffset;
    fn try_from(offset: u32) -> Result<Self, Self::Error> {
        match offset {
            0x00 => Ok(Self::Id),
            0x04 => Ok(Self::Capabilities),
            0x10 => Ok(Self::Shadow0),
            0x14 => Ok(Self::Shadow1),
            0x18 => Ok(Self::Shadow2),
            0x1c => Ok(Self::Shadow3),
            0x24 => Ok(Self::Status),
            0x28 => Ok(Self::AppliedEpoch),
            0x2c => Ok(Self::OutcomeEpoch),
            0x30 => Ok(Self::Applied0),
            0x34 => Ok(Self::Applied1),
            0x38 => Ok(Self::Applied2),
            0x3c => Ok(Self::Applied3),
            _ => Err(InvalidRegisterOffset {
                byte_offset: offset,
                access: RegisterAccess::Read,
            }),
        }
    }
}

/// One of the four writable profile-2 shadow phase-word channels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowChannel {
    /// Channel 0.
    Channel0,
    /// Channel 1.
    Channel1,
    /// Channel 2.
    Channel2,
    /// Channel 3.
    Channel3,
}

impl ShadowChannel {
    /// Zero-based channel index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Channel0 => 0,
            Self::Channel1 => 1,
            Self::Channel2 => 2,
            Self::Channel3 => 3,
        }
    }
    /// Host API byte offset for this shadow register.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        0x10 + self.index() as u32 * 4
    }
}

impl TryFrom<u32> for ShadowChannel {
    type Error = InvalidRegisterOffset;
    fn try_from(offset: u32) -> Result<Self, Self::Error> {
        match offset {
            0x10 => Ok(Self::Channel0),
            0x14 => Ok(Self::Channel1),
            0x18 => Ok(Self::Channel2),
            0x1c => Ok(Self::Channel3),
            _ => Err(InvalidRegisterOffset {
                byte_offset: offset,
                access: RegisterAccess::ShadowWrite,
            }),
        }
    }
}

/// One explicitly selected bladeRF receive connector.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiveChannel {
    /// Receive connector RX1.
    Rx1,
    /// Receive connector RX2.
    Rx2,
}

/// Reason a receive plan is outside the fixed complete-session bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanError {
    /// Center frequency was zero.
    ZeroCenterFrequency,
    /// Manual gain was outside the supported range.
    GainOutOfRange {
        /// Rejected gain in dB.
        gain_db: i32,
    },
    /// DATA-record count was outside the bounded range.
    DataRecordsOutOfRange {
        /// Rejected record count.
        data_records: u64,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCenterFrequency => f.write_str("center frequency must be positive"),
            Self::GainOutOfRange { gain_db } => write!(
                f,
                "gain {gain_db} dB is outside {MIN_GAIN_DB}..={MAX_GAIN_DB} dB"
            ),
            Self::DataRecordsOutOfRange { data_records } => write!(
                f,
                "DATA-record count {data_records} is outside {MIN_DATA_RECORDS}..={MAX_DATA_RECORDS}"
            ),
        }
    }
}

impl Error for PlanError {}

/// Fully bounded receive request retained by one complete session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivePlan {
    channel: ReceiveChannel,
    center_frequency_hz: u64,
    gain_db: i32,
    data_records: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceivePlanWire {
    channel: ReceiveChannel,
    center_frequency_hz: u64,
    gain_db: i32,
    data_records: u64,
}

impl<'de> Deserialize<'de> for ReceivePlan {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ReceivePlanWire::deserialize(deserializer)?;
        Self::new(
            wire.channel,
            wire.center_frequency_hz,
            wire.gain_db,
            wire.data_records,
        )
        .map_err(serde::de::Error::custom)
    }
}

impl ReceivePlan {
    /// Validate and construct a fixed-profile receive plan.
    ///
    /// # Errors
    /// Returns [`PlanError`] for a zero center frequency, gain outside
    /// -16 through 60 dB, or DATA count outside 100,000 through 10,000,000.
    pub const fn new(
        channel: ReceiveChannel,
        center_frequency_hz: u64,
        gain_db: i32,
        data_records: u64,
    ) -> Result<Self, PlanError> {
        if center_frequency_hz == 0 {
            return Err(PlanError::ZeroCenterFrequency);
        }
        if gain_db < MIN_GAIN_DB || gain_db > MAX_GAIN_DB {
            return Err(PlanError::GainOutOfRange { gain_db });
        }
        if data_records < MIN_DATA_RECORDS || data_records > MAX_DATA_RECORDS {
            return Err(PlanError::DataRecordsOutOfRange { data_records });
        }
        Ok(Self {
            channel,
            center_frequency_hz,
            gain_db,
            data_records,
        })
    }

    /// Selected receive connector.
    #[must_use]
    pub const fn channel(self) -> ReceiveChannel {
        self.channel
    }
    /// Requested center frequency in hertz.
    #[must_use]
    pub const fn center_frequency_hz(self) -> u64 {
        self.center_frequency_hz
    }
    /// Requested manual gain in dB.
    #[must_use]
    pub const fn gain_db(self) -> i32 {
        self.gain_db
    }
    /// Requested number of DATA records, excluding the one CONFIG record.
    #[must_use]
    pub const fn data_records(self) -> u64 {
        self.data_records
    }
    /// Exact CONFIG-plus-DATA transport sample count.
    #[must_use]
    pub const fn expected_transport_samples(self) -> u32 {
        ((self.data_records + 1) * TRANSPORT_SAMPLES_PER_RECORD) as u32
    }
    /// Exact CONFIG-plus-DATA raw byte count.
    #[must_use]
    pub const fn expected_raw_bytes(self) -> u64 {
        self.expected_transport_samples() as u64 * BYTES_PER_TRANSPORT_SAMPLE
    }
}

/// RF settings independently read back while RX remains disabled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredReceiveReadback {
    channel: ReceiveChannel,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    bandwidth_hz: u32,
}

impl ConfiguredReceiveReadback {
    /// Construct a configured receive readback.
    #[must_use]
    pub const fn new(
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
    /// Read-back receive connector.
    #[must_use]
    pub const fn channel(self) -> ReceiveChannel {
        self.channel
    }
    /// Read-back center frequency in hertz.
    #[must_use]
    pub const fn center_frequency_hz(self) -> u64 {
        self.center_frequency_hz
    }
    /// Read-back sample rate in hertz.
    #[must_use]
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }
    /// Read-back analog bandwidth in hertz.
    #[must_use]
    pub const fn bandwidth_hz(self) -> u32 {
        self.bandwidth_hz
    }
}

/// Active RX readback, including gain observed after enable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveReceiveReadback {
    configured: ConfiguredReceiveReadback,
    gain_db: i32,
}

impl ActiveReceiveReadback {
    /// Construct an active receive readback.
    #[must_use]
    pub const fn new(configured: ConfiguredReceiveReadback, gain_db: i32) -> Self {
        Self {
            configured,
            gain_db,
        }
    }
    /// Configured values retained after RX enable.
    #[must_use]
    pub const fn configured(self) -> ConfiguredReceiveReadback {
        self.configured
    }
    /// Manual gain read back after RX enable.
    #[must_use]
    pub const fn gain_db(self) -> i32 {
        self.gain_db
    }
}

/// Exact extent and digest of one completed raw capture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureReport {
    transport_samples: u32,
    raw_bytes: u64,
    sha256: String,
}

impl CaptureReport {
    /// Construct a capture report; clients validate its digest and extent.
    #[must_use]
    pub fn new(transport_samples: u32, raw_bytes: u64, sha256: impl Into<String>) -> Self {
        Self {
            transport_samples,
            raw_bytes,
            sha256: sha256.into(),
        }
    }
    /// Complex transport samples captured.
    #[must_use]
    pub const fn transport_samples(&self) -> u32 {
        self.transport_samples
    }
    /// Raw SC16 Q11 bytes captured.
    #[must_use]
    pub const fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }
    /// Lowercase SHA-256 of the exact raw bytes.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Value-only attestation for the retained loader-neutral library.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryFacts {
    version: String,
    sha256: String,
    marker: u32,
}

impl LibraryFacts {
    /// Construct an unchecked value-only claim; client spawn validates it.
    #[must_use]
    pub fn new(version: impl Into<String>, sha256: impl Into<String>, marker: u32) -> Self {
        Self {
            version: version.into(),
            sha256: sha256.into(),
            marker,
        }
    }
    /// Claimed semantic version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }
    /// Claimed library SHA-256.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    /// Claimed B2L1 marker.
    #[must_use]
    pub const fn marker(&self) -> u32 {
        self.marker
    }
}

/// Value-only attestation for one sealed FPGA image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageFacts {
    sha256: String,
    bytes: u64,
}

impl ImageFacts {
    /// Construct an unchecked value-only image claim; spawn validates it.
    #[must_use]
    pub fn new(sha256: impl Into<String>, bytes: u64) -> Self {
        Self {
            sha256: sha256.into(),
            bytes,
        }
    }
    /// Claimed image SHA-256.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    /// Claimed image byte count.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Exact retained library, custom image, and stock image facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFacts {
    library: LibraryFacts,
    custom: ImageFacts,
    stock: ImageFacts,
}

impl BundleFacts {
    /// Validate and construct exact value-only bundle facts.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless the library version, marker, digest,
    /// custom-image bounds, and frozen stock identity are exact.
    pub fn new(
        library: LibraryFacts,
        custom: ImageFacts,
        stock: ImageFacts,
    ) -> Result<Self, ClientError> {
        let bundle = Self {
            library,
            custom,
            stock,
        };
        validate_bundle(&bundle)?;
        Ok(bundle)
    }
    /// Retained library facts.
    #[must_use]
    pub const fn library(&self) -> &LibraryFacts {
        &self.library
    }
    /// Retained custom-image facts.
    #[must_use]
    pub const fn custom(&self) -> &ImageFacts {
        &self.custom
    }
    /// Retained stock-image facts.
    #[must_use]
    pub const fn stock(&self) -> &ImageFacts {
        &self.stock
    }
    /// Split the value into its three owned claims.
    #[must_use]
    pub fn into_parts(self) -> (LibraryFacts, ImageFacts, ImageFacts) {
        (self.library, self.custom, self.stock)
    }
}

/// Frozen backend mode for the complete primary session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// Deterministic CPU-only primary fixture.
    FixturePrimary,
    /// Native one-handle primary worker.
    NativePrimary,
}

impl ServerMode {
    /// Fixed complete request allowance.
    #[must_use]
    pub const fn max_session_requests(self) -> usize {
        MAX_PRIMARY_SESSION_REQUESTS
    }
}

/// Last exact state confirmed by a successful response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Worker opened and has not dispatched an image.
    Ready,
    /// Custom load succeeded but identity is unverified.
    CustomUnverified,
    /// Exact custom profile identity was verified.
    CustomVerified,
    /// Receive settings were configured and read back with RX disabled.
    ReceiveConfigured,
    /// Selected RX was enabled and active settings were read back.
    ReceiveActive,
    /// Exact raw capture and completion evidence were produced.
    CaptureComplete,
    /// Selected RX was explicitly stopped.
    ReceiveStopped,
    /// Stock load succeeded but identity is unverified.
    StockUnverified,
    /// Exact stock identity was verified.
    StockVerified,
    /// Orderly worker shutdown completed.
    Closed,
}

/// Operation carried by one session [`Request`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum RequestOperation {
    /// Negotiate version and require one exact primary mode.
    Hello {
        /// Exact mode the client intends to use.
        mode: ServerMode,
    },
    /// Dispatch the sealed custom profile-2 image.
    LoadProfile2 {},
    /// Verify exact profile-2 identity.
    VerifyProfile2 {},
    /// Read one allowlisted register.
    ReadRegister {
        /// Register to read.
        register: ReadRegister,
    },
    /// Write one complete phase word to a typed shadow channel.
    WriteShadow {
        /// Shadow channel to write.
        channel: ShadowChannel,
        /// Complete phase word.
        value: u32,
    },
    /// Write the fixed `CMIT` value to the commit register.
    Commit {},
    /// Configure and independently read back receive settings.
    ConfigureReceive {
        /// Fully bounded receive plan.
        plan: ReceivePlan,
    },
    /// Enable selected RX and read back active gain.
    StartReceive {},
    /// Start the exact bounded raw capture extent.
    Capture {},
    /// Disable selected RX after start or capture.
    StopReceive {},
    /// Dispatch the sealed stock image on the same handle.
    RestoreStock {},
    /// Verify exact stock identity.
    VerifyStock {},
    /// Orderly close from Ready or StockVerified.
    Shutdown {},
}

/// One versioned complete-session request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    operation: RequestOperation,
}

impl Request {
    /// Construct a request with the frozen family and version.
    #[must_use]
    pub const fn new(id: u64, operation: RequestOperation) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            operation,
        }
    }
    /// Decoded protocol family.
    #[must_use]
    pub const fn family(&self) -> ProtocolFamily {
        self.family
    }
    /// Decoded protocol version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }
    /// Caller-assigned request identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }
    /// Borrow the typed operation.
    #[must_use]
    pub const fn operation(&self) -> &RequestOperation {
        &self.operation
    }
    /// Split into family, version, identifier, and operation.
    #[must_use]
    pub fn into_parts(self) -> (ProtocolFamily, u16, u64, RequestOperation) {
        (self.family, self.version, self.id, self.operation)
    }
}

/// Stable class for a terminal session-server error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed frame, version, ID, mode, or operation order.
    Protocol,
    /// Operation is not legal in the current lifecycle state.
    InvalidState,
    /// Fixed request allowance was reached.
    RequestLimit,
    /// One guarded backend operation failed.
    Backend,
}

/// Result carried by one session [`Response`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum ResponseKind {
    /// Negotiation and exact bundle attestation succeeded.
    Hello {
        /// Exact server mode.
        mode: ServerMode,
        /// Required initial state.
        state: SessionState,
        /// Exact retained bundle facts.
        bundle: BundleFacts,
    },
    /// A simple lifecycle transition completed.
    State {
        /// Exact resulting state.
        state: SessionState,
    },
    /// An allowlisted register read completed.
    RegisterValue {
        /// Exact state, required to remain CustomVerified.
        state: SessionState,
        /// Register that was read.
        register: ReadRegister,
        /// Complete 32-bit value.
        value: u32,
    },
    /// Receive configuration and disabled-RX readback completed.
    Configured {
        /// Required ReceiveConfigured state.
        state: SessionState,
        /// Independently observed configuration.
        readback: ConfiguredReceiveReadback,
    },
    /// RX enable and active-gain readback completed.
    Active {
        /// Required ReceiveActive state.
        state: SessionState,
        /// Independently observed active configuration.
        readback: ActiveReceiveReadback,
    },
    /// Raw capture mode begins immediately after this frame.
    CaptureStarted {
        /// Required ReceiveActive state.
        state: SessionState,
        /// Exact raw transport sample extent which follows.
        transport_samples: u32,
        /// Exact raw byte extent which follows.
        raw_bytes: u64,
    },
    /// Completion frame following exactly the declared raw bytes.
    CaptureComplete {
        /// Required CaptureComplete state.
        state: SessionState,
        /// Backend capture evidence for independent comparison.
        report: CaptureReport,
    },
    /// A request failed and the server session is terminal.
    Error {
        /// Stable machine-readable error class.
        code: ErrorCode,
        /// Bounded human-readable diagnostic.
        message: String,
    },
}

/// One versioned response correlated to a request identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    result: ResponseKind,
}

impl Response {
    /// Construct a response with the frozen family and version.
    #[must_use]
    pub const fn new(id: u64, result: ResponseKind) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            result,
        }
    }
    /// Construct a terminal error response with a bounded diagnostic.
    #[must_use]
    pub fn error(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(
            id,
            ResponseKind::Error {
                code,
                message: bounded_error_message(message.into()),
            },
        )
    }
    /// Decoded protocol family.
    #[must_use]
    pub const fn family(&self) -> ProtocolFamily {
        self.family
    }
    /// Decoded protocol version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }
    /// Correlated request identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }
    /// Borrow the typed result.
    #[must_use]
    pub const fn result(&self) -> &ResponseKind {
        &self.result
    }
    /// Split into family, version, ID, and result.
    #[must_use]
    pub fn into_parts(self) -> (ProtocolFamily, u16, u64, ResponseKind) {
        (self.family, self.version, self.id, self.result)
    }
}

fn bounded_error_message(mut message: String) -> String {
    if message.len() <= MAX_ERROR_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES - 3;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("...");
    message
}

/// Errors produced by a synchronous [`SessionClient`].
#[derive(Debug)]
pub enum ClientError {
    /// Transaction timeout was zero or exceeded the fixed maximum.
    InvalidTimeout {
        /// Rejected timeout.
        timeout: Duration,
        /// Largest accepted timeout.
        maximum: Duration,
    },
    /// Library version did not equal [`REQUIRED_LIBRARY_VERSION`].
    InvalidLibraryVersion,
    /// Library digest was not exactly 64 lowercase hexadecimal digits.
    InvalidLibrarySha256,
    /// Library marker did not equal [`REQUIRED_LIBRARY_MARKER`].
    InvalidLibraryMarker,
    /// Custom-image digest was invalid.
    InvalidCustomSha256,
    /// Custom-image byte count was outside the fixed nonzero bound.
    InvalidCustomBytes {
        /// Rejected byte count.
        bytes: u64,
    },
    /// Custom and stock image digests were identical.
    AliasedImageSha256,
    /// Stock digest did not equal [`REQUIRED_STOCK_SHA256`].
    InvalidStockSha256,
    /// Stock byte count did not equal [`REQUIRED_STOCK_BYTES`].
    InvalidStockBytes {
        /// Rejected byte count.
        bytes: u64,
    },
    /// The child could not be started, or safe process cleanup is unavailable before spawn.
    Spawn(io::Error),
    /// A requested standard-I/O pipe was unavailable.
    MissingPipe {
        /// Missing standard-I/O pipe name.
        name: &'static str,
    },
    /// The dedicated I/O worker could not be started.
    WorkerSpawn(io::Error),
    /// The only outstanding task could not be published.
    Publish {
        /// Unpublished request identifier.
        request_id: u64,
    },
    /// The complete transaction exceeded its deadline.
    Timeout {
        /// Timed-out request identifier.
        request_id: u64,
        /// Configured deadline duration.
        timeout: Duration,
    },
    /// JSON framing or transport failed.
    Transport {
        /// Affected request identifier.
        request_id: u64,
        /// Underlying frame error.
        source: FrameError,
    },
    /// EOF occurred before a framed response or complete raw extent.
    EarlyExit {
        /// Affected request identifier.
        request_id: u64,
        /// Transaction phase which observed EOF.
        phase: &'static str,
    },
    /// Reading raw capture bytes failed.
    RawRead {
        /// Capture request identifier.
        request_id: u64,
        /// Underlying read error.
        source: io::Error,
    },
    /// The caller-owned capture sink rejected output.
    Sink {
        /// Capture request identifier.
        request_id: u64,
        /// Underlying sink error.
        source: io::Error,
    },
    /// The I/O worker stopped without a result.
    WorkerStopped {
        /// Affected request identifier.
        request_id: u64,
    },
    /// The fixed complete-session request bound was reached.
    RequestLimit,
    /// An operation was attempted from the wrong confirmed state.
    InvalidState {
        /// Operation description.
        operation: &'static str,
        /// Required state or state set.
        expected: &'static str,
        /// Last exact confirmed state.
        actual: SessionState,
    },
    /// A correlated response violated an exact expectation.
    Protocol {
        /// Affected request identifier.
        request_id: u64,
        /// Local mismatch diagnostic.
        message: String,
    },
    /// The child returned a typed terminal error.
    Server {
        /// Correlated request identifier.
        request_id: u64,
        /// Stable server error class.
        code: ErrorCode,
        /// Bounded server diagnostic.
        message: String,
    },
    /// Explicit shutdown produced a failing status.
    ChildExit {
        /// Observed process exit status.
        status: ExitStatus,
    },
    /// Waiting for the child failed.
    Wait(io::Error),
    /// The I/O worker panicked.
    WorkerPanicked,
    /// The I/O worker did not complete within its cleanup bound.
    WorkerCompletionTimeout {
        /// Cleanup wait bound.
        timeout: Duration,
    },
    /// Cleanup also failed after the primary failure.
    Cleanup {
        /// Original operation error.
        primary: Box<ClientError>,
        /// Additional cleanup diagnostic.
        message: String,
    },
    /// Explicit termination encountered cleanup errors.
    Termination {
        /// Cleanup diagnostic.
        message: String,
    },
    /// The session is terminal and cannot publish another request.
    Terminal,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeout { timeout, maximum } => write!(
                f,
                "timeout {timeout:?} must be positive and at most {maximum:?}"
            ),
            Self::InvalidLibraryVersion => {
                write!(f, "library version must equal {REQUIRED_LIBRARY_VERSION}")
            }
            Self::InvalidLibrarySha256 => {
                f.write_str("library SHA-256 must be 64 lowercase hexadecimal digits")
            }
            Self::InvalidLibraryMarker => write!(
                f,
                "library marker must equal 0x{REQUIRED_LIBRARY_MARKER:08x}"
            ),
            Self::InvalidCustomSha256 => {
                f.write_str("custom SHA-256 must be 64 lowercase hexadecimal digits")
            }
            Self::InvalidCustomBytes { bytes } => write!(
                f,
                "custom image has {bytes} bytes; expected 1..={MAX_IMAGE_BYTES}"
            ),
            Self::AliasedImageSha256 => f.write_str("custom and stock image digests must differ"),
            Self::InvalidStockSha256 => {
                f.write_str("stock image digest does not match frozen xA9 v0.16.0")
            }
            Self::InvalidStockBytes { bytes } => write!(
                f,
                "stock image has {bytes} bytes; expected {REQUIRED_STOCK_BYTES}"
            ),
            Self::Spawn(error) => write!(f, "spawn session child: {error}"),
            Self::MissingPipe { name } => write!(f, "session child has no piped {name}"),
            Self::WorkerSpawn(error) => write!(f, "spawn session I/O worker: {error}"),
            Self::Publish { request_id } => write!(f, "publish request {request_id}"),
            Self::Timeout {
                request_id,
                timeout,
            } => write!(f, "request {request_id} exceeded {timeout:?}"),
            Self::Transport { request_id, source } => {
                write!(f, "request {request_id} transport failed: {source}")
            }
            Self::EarlyExit { request_id, phase } => {
                write!(f, "child exited during {phase} for request {request_id}")
            }
            Self::RawRead { request_id, source } => {
                write!(f, "read raw bytes for request {request_id}: {source}")
            }
            Self::Sink { request_id, source } => {
                write!(f, "capture sink failed for request {request_id}: {source}")
            }
            Self::WorkerStopped { request_id } => {
                write!(f, "I/O worker stopped during request {request_id}")
            }
            Self::RequestLimit => write!(
                f,
                "session reached its {MAX_PRIMARY_SESSION_REQUESTS}-request bound"
            ),
            Self::InvalidState {
                operation,
                expected,
                actual,
            } => write!(f, "cannot {operation} from {actual:?}; expected {expected}"),
            Self::Protocol {
                request_id,
                message,
            } => write!(f, "request {request_id} protocol mismatch: {message}"),
            Self::Server {
                request_id,
                code,
                message,
            } => write!(f, "request {request_id} failed ({code:?}): {message}"),
            Self::ChildExit { status } => write!(f, "session child exited with {status}"),
            Self::Wait(error) => write!(f, "wait for session child: {error}"),
            Self::WorkerPanicked => f.write_str("session I/O worker panicked"),
            Self::WorkerCompletionTimeout { timeout } => {
                write!(f, "I/O worker did not stop within {timeout:?}")
            }
            Self::Cleanup { primary, message } => {
                write!(f, "{primary}; cleanup also failed: {message}")
            }
            Self::Termination { message } => write!(f, "terminate session: {message}"),
            Self::Terminal => f.write_str("session is terminal"),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error)
            | Self::WorkerSpawn(error)
            | Self::Wait(error)
            | Self::RawRead { source: error, .. }
            | Self::Sink { source: error, .. } => Some(error),
            Self::Transport { source, .. } => Some(source),
            Self::Cleanup { primary, .. } => Some(primary),
            _ => None,
        }
    }
}

fn exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_bundle(bundle: &BundleFacts) -> Result<(), ClientError> {
    if bundle.library.version != REQUIRED_LIBRARY_VERSION {
        return Err(ClientError::InvalidLibraryVersion);
    }
    if !exact_lower_hex(&bundle.library.sha256, 64) {
        return Err(ClientError::InvalidLibrarySha256);
    }
    if bundle.library.marker != REQUIRED_LIBRARY_MARKER {
        return Err(ClientError::InvalidLibraryMarker);
    }
    if !exact_lower_hex(&bundle.custom.sha256, 64) {
        return Err(ClientError::InvalidCustomSha256);
    }
    if !(1..=MAX_IMAGE_BYTES).contains(&bundle.custom.bytes) {
        return Err(ClientError::InvalidCustomBytes {
            bytes: bundle.custom.bytes,
        });
    }
    if bundle.stock.sha256 != REQUIRED_STOCK_SHA256 {
        return Err(ClientError::InvalidStockSha256);
    }
    if bundle.stock.bytes != REQUIRED_STOCK_BYTES {
        return Err(ClientError::InvalidStockBytes {
            bytes: bundle.stock.bytes,
        });
    }
    if bundle.custom.sha256 == bundle.stock.sha256 {
        return Err(ClientError::AliasedImageSha256);
    }
    Ok(())
}

fn validate_timeout(timeout: Duration) -> Result<(), ClientError> {
    if timeout.is_zero() || timeout > MAX_TRANSACTION_TIMEOUT {
        return Err(ClientError::InvalidTimeout {
            timeout,
            maximum: MAX_TRANSACTION_TIMEOUT,
        });
    }
    Ok(())
}

type WorkerTask = Box<dyn FnOnce(&mut ChildStdin, &mut ChildStdout) + Send + 'static>;

#[derive(Clone, Copy)]
enum WorkerCompletion {
    Completed,
    Panicked,
}

enum WorkerFailure {
    Frame(FrameError),
    EndOfStream(&'static str),
    RawRead(io::Error),
    Sink(io::Error),
    StartResponse(Box<Response>),
    Protocol(String),
}

fn io_worker(mut input: ChildStdin, mut output: ChildStdout, tasks: Receiver<WorkerTask>) {
    while let Ok(task) = tasks.recv() {
        task(&mut input, &mut output);
    }
}

fn configure_private_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn missing_process(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(Errno::ESRCH as i32)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn signal_private_process_group(process_group: u32, diagnostics: &mut Vec<String>) -> bool {
    #[cfg(unix)]
    {
        let Ok(process_group) = i32::try_from(process_group) else {
            diagnostics.push("process-group identifier exceeds platform range".to_owned());
            return false;
        };
        match killpg(Pid::from_raw(process_group), Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => true,
            Err(error) => {
                diagnostics.push(format!("signal child process group: {error}"));
                false
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (process_group, diagnostics);
        true
    }
}

fn kill_and_reap(child: &mut Child, diagnostics: &mut Vec<String>) -> bool {
    if let Err(error) = child.kill()
        && !missing_process(&error)
    {
        diagnostics.push(format!("kill child directly: {error}"));
    }
    match child.wait() {
        Ok(_) => true,
        Err(error) => {
            diagnostics.push(format!("reap child: {error}"));
            false
        }
    }
}

fn force_reap(child: &mut Child, process_group: u32) -> Option<String> {
    let mut diagnostics = Vec::new();
    let _ = signal_private_process_group(process_group, &mut diagnostics);
    let _ = kill_and_reap(child, &mut diagnostics);
    (!diagnostics.is_empty()).then(|| diagnostics.join("; "))
}

fn with_cleanup(primary: ClientError, cleanup: Option<String>) -> ClientError {
    match cleanup {
        Some(message) => ClientError::Cleanup {
            primary: Box::new(primary),
            message,
        },
        None => primary,
    }
}

fn build_command<P, I, S>(program: P, arguments: I) -> Command
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_remove("LD_PRELOAD")
        .env_remove("LD_AUDIT")
        .env_remove("LD_LIBRARY_PATH");
    command
}

const fn supports_nonreaping_child_wait() -> bool {
    cfg!(not(unix))
        || cfg!(any(
            target_os = "android",
            target_os = "freebsd",
            target_os = "haiku",
            all(target_os = "linux", not(target_env = "uclibc")),
        ))
}

fn require_nonreaping_child_wait(supported: bool) -> Result<(), ClientError> {
    if supported {
        Ok(())
    } else {
        Err(ClientError::Spawn(io::Error::new(
            io::ErrorKind::Unsupported,
            "complete-session process cleanup requires non-reaping child-status observation",
        )))
    }
}

fn round_trip_task(
    request: Request,
    sender: SyncSender<Result<Response, WorkerFailure>>,
) -> WorkerTask {
    Box::new(move |input, output| {
        let result = match write_frame(input, &request) {
            Ok(()) => match read_frame(output) {
                Ok(Some(response)) => Ok(response),
                Ok(None) => Err(WorkerFailure::EndOfStream("response")),
                Err(error) => Err(WorkerFailure::Frame(error)),
            },
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                Err(WorkerFailure::EndOfStream("request publication"))
            }
            Err(error) => Err(WorkerFailure::Frame(error)),
        };
        let _ = sender.try_send(result);
    })
}

struct CaptureSuccess<W> {
    started: Response,
    complete: Response,
    observed_bytes: u64,
    observed_sha256: String,
    sink: W,
}

fn capture_task<W: Write + Send + 'static>(
    request: Request,
    expected_samples: u32,
    expected_bytes: u64,
    mut sink: W,
    sender: SyncSender<Result<CaptureSuccess<W>, WorkerFailure>>,
) -> WorkerTask {
    Box::new(move |input, output| {
        let result = (|| {
            match write_frame(input, &request) {
                Ok(()) => {}
                Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                    return Err(WorkerFailure::EndOfStream("capture request publication"));
                }
                Err(error) => return Err(WorkerFailure::Frame(error)),
            }
            let started: Response = match read_frame::<_, Response>(&mut *output) {
                Ok(Some(response)) => response,
                Ok(None) => return Err(WorkerFailure::EndOfStream("CaptureStarted")),
                Err(error) => return Err(WorkerFailure::Frame(error)),
            };

            if matches!(started.result(), ResponseKind::Error { .. }) {
                return Err(WorkerFailure::StartResponse(Box::new(started)));
            }

            // Only enter unframed mode after an exact CaptureStarted envelope.
            validate_capture_started(&started, request.id(), expected_samples, expected_bytes)
                .map_err(WorkerFailure::Protocol)?;

            let mut remaining = expected_bytes;
            let mut observed_bytes = 0_u64;
            let mut digest = Sha256::new();
            let mut buffer = vec![0_u8; RAW_CHUNK_BYTES];
            while remaining != 0 {
                let wanted = usize::try_from(remaining.min(RAW_CHUNK_BYTES as u64))
                    .expect("bounded chunk fits usize");
                let count = loop {
                    match output.read(&mut buffer[..wanted]) {
                        Ok(0) => return Err(WorkerFailure::EndOfStream("raw capture")),
                        Ok(count) => break count,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(error) => return Err(WorkerFailure::RawRead(error)),
                    }
                };
                digest.update(&buffer[..count]);
                observed_bytes += count as u64;
                remaining -= count as u64;
                sink.write_all(&buffer[..count])
                    .map_err(WorkerFailure::Sink)?;
            }
            sink.flush().map_err(WorkerFailure::Sink)?;
            let complete: Response = match read_frame::<_, Response>(output) {
                Ok(Some(response)) => response,
                Ok(None) => return Err(WorkerFailure::EndOfStream("CaptureComplete")),
                Err(error) => return Err(WorkerFailure::Frame(error)),
            };
            Ok(CaptureSuccess {
                started,
                complete,
                observed_bytes,
                observed_sha256: format!("{:x}", digest.finalize()),
                sink,
            })
        })();
        let _ = sender.try_send(result);
    })
}

fn validate_capture_started(
    response: &Response,
    request_id: u64,
    expected_samples: u32,
    expected_bytes: u64,
) -> Result<(), String> {
    let valid_envelope = response.family() == PROTOCOL_FAMILY
        && response.version() == PROTOCOL_VERSION
        && response.id() == request_id;
    let valid_result = matches!(
        response.result(),
        ResponseKind::CaptureStarted {
            state: SessionState::ReceiveActive,
            transport_samples,
            raw_bytes,
        } if *transport_samples == expected_samples && *raw_bytes == expected_bytes
    );
    if valid_envelope && valid_result {
        Ok(())
    } else {
        Err("response is not the exact CaptureStarted envelope and extent".to_owned())
    }
}

/// Hardened synchronous client for one complete primary child session.
///
/// [`Self::spawn`] validates bundle facts, removes dynamic-loader injection
/// variables, creates a private process group, starts a dedicated I/O worker,
/// and completes the exact Hello exchange before returning in Ready. Every
/// request marks the client terminal before publication and only an exact
/// response clears that uncertainty. Any error starts process-group, leader and
/// worker cleanup; ambiguous resources remain retained and retryable until
/// cleanup is confirmed while the client remains owned.
/// Direct operating-system kill/wait cleanup can outlast the transaction
/// deadline; I/O-worker completion uses a separate 100 ms cleanup bound.
/// On Unix targets without a safe non-reaping child-status observation,
/// [`Self::spawn`] returns an unsupported [`ClientError::Spawn`] before process
/// creation rather than risk signaling a reused process-group identifier.
pub struct SessionClient {
    child: Option<Child>,
    tasks: Option<SyncSender<WorkerTask>>,
    worker: Option<JoinHandle<()>>,
    worker_completion: Option<Receiver<WorkerCompletion>>,
    process_group: u32,
    process_group_active: bool,
    timeout: Duration,
    next_id: u64,
    state: SessionState,
    terminal: bool,
    plan: Option<ReceivePlan>,
    cleanup_confirmed: bool,
    cleanup_diagnostic: Option<String>,
}

impl fmt::Debug for SessionClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionClient")
            .field("timeout", &self.timeout)
            .field("next_id", &self.next_id)
            .field("state", &self.state)
            .field("terminal", &self.terminal)
            .field("plan", &self.plan)
            .field("cleanup_confirmed", &self.cleanup_confirmed)
            .finish_non_exhaustive()
    }
}

impl SessionClient {
    /// Spawn, harden, and negotiate one exact primary session child.
    ///
    /// Mode and bundle attestation validate wire claims; they cannot prove that
    /// an arbitrary caller-selected executable is the intended implementation.
    /// Callers must supply a trusted, independently qualified command.
    ///
    /// # Errors
    /// Returns [`ClientError`] for invalid facts or timeout, process/worker
    /// startup failure, or any non-exact Hello response. Failed startup always
    /// attempts private-process-group cleanup and leader reaping.
    pub fn spawn<P, I, S>(
        program: P,
        arguments: I,
        mode: ServerMode,
        expected_bundle: &BundleFacts,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        validate_timeout(transaction_timeout)?;
        validate_bundle(expected_bundle)?;
        require_nonreaping_child_wait(supports_nonreaping_child_wait())?;
        let mut command = build_command(program, arguments);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        configure_private_process_group(&mut command);
        let mut child = command.spawn().map_err(ClientError::Spawn)?;
        let process_group = child.id();
        let Some(input) = child.stdin.take() else {
            let cleanup = force_reap(&mut child, process_group);
            return Err(with_cleanup(
                ClientError::MissingPipe { name: "stdin" },
                cleanup,
            ));
        };
        let Some(output) = child.stdout.take() else {
            drop(input);
            let cleanup = force_reap(&mut child, process_group);
            return Err(with_cleanup(
                ClientError::MissingPipe { name: "stdout" },
                cleanup,
            ));
        };
        let (task_sender, task_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("profile2-session-io".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    io_worker(input, output, task_receiver);
                }));
                let completion = if outcome.is_ok() {
                    WorkerCompletion::Completed
                } else {
                    WorkerCompletion::Panicked
                };
                let _ = completion_sender.try_send(completion);
            }) {
            Ok(worker) => worker,
            Err(error) => {
                let cleanup = force_reap(&mut child, process_group);
                return Err(with_cleanup(ClientError::WorkerSpawn(error), cleanup));
            }
        };
        let mut client = Self {
            child: Some(child),
            tasks: Some(task_sender),
            worker: Some(worker),
            worker_completion: Some(completion_receiver),
            process_group,
            process_group_active: true,
            timeout: transaction_timeout,
            next_id: 1,
            state: SessionState::Ready,
            terminal: false,
            plan: None,
            cleanup_confirmed: false,
            cleanup_diagnostic: None,
        };
        let result = client.exchange(RequestOperation::Hello { mode })?;
        match result {
            ResponseKind::Hello {
                mode: actual_mode,
                state: SessionState::Ready,
                bundle,
            } if actual_mode == mode && &bundle == expected_bundle => Ok(client),
            actual => client.fail(ClientError::Protocol {
                request_id: 1,
                message: format!("hello result {actual:?} is not exact Ready mode/bundle"),
            }),
        }
    }

    /// Last state confirmed by an exact successful response.
    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }
    /// Whether this client can never publish another request.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
    /// Whether child, process-group and I/O-worker teardown is confirmed.
    ///
    /// This accessor performs no signal, wait, join or other side effect. A
    /// terminal session can remain cleanup-unconfirmed after an ambiguous
    /// operating-system or worker cleanup result.
    #[must_use]
    pub const fn is_cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed
    }
    /// Number of requests published, including Hello.
    #[must_use]
    pub const fn request_count(&self) -> u64 {
        self.next_id - 1
    }
    /// Load the sealed custom image and enter CustomUnverified.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from Ready with an exact response.
    pub fn load_profile2(&mut self) -> Result<(), ClientError> {
        self.state_transition(
            "load profile-2",
            RequestOperation::LoadProfile2 {},
            SessionState::Ready,
            SessionState::CustomUnverified,
        )
    }

    /// Verify exact custom identity and enter CustomVerified.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from CustomUnverified with an exact response.
    pub fn verify_profile2(&mut self) -> Result<(), ClientError> {
        self.state_transition(
            "verify profile-2",
            RequestOperation::VerifyProfile2 {},
            SessionState::CustomUnverified,
            SessionState::CustomVerified,
        )
    }

    /// Read one typed profile-2 register while CustomVerified.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state or any non-exact response.
    pub fn read_register(&mut self, register: ReadRegister) -> Result<u32, ClientError> {
        self.require_state("read a register", "CustomVerified", |state| {
            state == SessionState::CustomVerified
        })?;
        let request_id = self.next_id;
        match self.exchange(RequestOperation::ReadRegister { register })? {
            ResponseKind::RegisterValue {
                state: SessionState::CustomVerified,
                register: actual,
                value,
            } if actual == register => Ok(value),
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("register result {actual:?} is not exact"),
            }),
        }
    }

    /// Write one typed shadow phase word while CustomVerified.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state or any non-exact response.
    pub fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<(), ClientError> {
        self.self_loop(
            "write a shadow register",
            RequestOperation::WriteShadow { channel, value },
            SessionState::CustomVerified,
        )
    }

    /// Write the fixed commit magic while CustomVerified.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state or any non-exact response.
    pub fn commit(&mut self) -> Result<(), ClientError> {
        self.self_loop(
            "commit shadow registers",
            RequestOperation::Commit {},
            SessionState::CustomVerified,
        )
    }

    /// Configure receive settings and validate the independent disabled-RX readback.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state, readback mismatch, or transport failure.
    pub fn configure_receive(
        &mut self,
        plan: ReceivePlan,
    ) -> Result<ConfiguredReceiveReadback, ClientError> {
        self.require_state("configure receive", "CustomVerified", |state| {
            state == SessionState::CustomVerified
        })?;
        let request_id = self.next_id;
        match self.exchange(RequestOperation::ConfigureReceive { plan })? {
            ResponseKind::Configured {
                state: SessionState::ReceiveConfigured,
                readback,
            } if configured_matches(plan, readback) => {
                self.state = SessionState::ReceiveConfigured;
                self.plan = Some(plan);
                Ok(readback)
            }
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("configured result {actual:?} is not exact"),
            }),
        }
    }

    /// Enable selected RX and validate active gain and configuration readback.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state, readback mismatch, or transport failure.
    pub fn start_receive(&mut self) -> Result<ActiveReceiveReadback, ClientError> {
        self.require_state("start receive", "ReceiveConfigured", |state| {
            state == SessionState::ReceiveConfigured
        })?;
        let plan = self.plan.expect("ReceiveConfigured retains a plan");
        let request_id = self.next_id;
        match self.exchange(RequestOperation::StartReceive {})? {
            ResponseKind::Active {
                state: SessionState::ReceiveActive,
                readback,
            } if configured_matches(plan, readback.configured())
                && readback.gain_db() == plan.gain_db() =>
            {
                self.state = SessionState::ReceiveActive;
                Ok(readback)
            }
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("active result {actual:?} is not exact"),
            }),
        }
    }

    /// Capture the exact derived raw extent into an owned caller sink.
    ///
    /// The sink is returned only after exact CaptureStarted, raw extent,
    /// independently computed digest, and same-ID CaptureComplete validation.
    /// On EOF, timeout, sink failure, mismatch, or any other error the partial
    /// sink is consumed and dropped as invalid evidence.
    /// A caller sink whose `write` implementation itself blocks cannot be
    /// forcibly interrupted; timeout still kills the child, waits 100 ms for
    /// the worker, and never returns that partial sink.
    ///
    /// # Errors
    /// Returns [`ClientError`] on wrong state, framing/raw/sink failure, extent
    /// or digest mismatch, timeout, or any non-exact completion response.
    pub fn capture<W: Write + Send + 'static>(
        &mut self,
        sink: W,
    ) -> Result<(CaptureReport, W), ClientError> {
        self.require_state("capture receive data", "ReceiveActive", |state| {
            state == SessionState::ReceiveActive
        })?;
        self.check_request_allowance()?;
        let plan = self.plan.expect("ReceiveActive retains a plan");
        let expected_samples = plan.expected_transport_samples();
        let expected_bytes = plan.expected_raw_bytes();
        let request_id = self.next_id;
        let deadline = Instant::now() + self.timeout;
        self.terminal = true;
        let (sender, receiver) = mpsc::sync_channel(1);
        let task = capture_task(
            Request::new(request_id, RequestOperation::Capture {}),
            expected_samples,
            expected_bytes,
            sink,
            sender,
        );
        if let Err(task) = self.publish_task(task) {
            drop(task);
            return self.fail(ClientError::Publish { request_id });
        }
        let result = match recv_until(&receiver, deadline, request_id, self.timeout) {
            Ok(result) => result,
            Err(error) => return self.fail(error),
        };
        let success = match result {
            Ok(success) => success,
            Err(WorkerFailure::StartResponse(response)) => {
                let error = response_error(*response, request_id).unwrap_or_else(|| {
                    ClientError::Protocol {
                        request_id,
                        message: "capture start response was neither exact start nor typed error"
                            .to_owned(),
                    }
                });
                return self.fail(error);
            }
            Err(failure) => return self.fail(worker_error(failure, request_id)),
        };
        if Instant::now() >= deadline {
            return self.fail(ClientError::Timeout {
                request_id,
                timeout: self.timeout,
            });
        }
        if validate_capture_started(
            &success.started,
            request_id,
            expected_samples,
            expected_bytes,
        )
        .is_err()
        {
            return self.fail(ClientError::Protocol {
                request_id,
                message: "CaptureStarted changed after pre-raw validation".to_owned(),
            });
        }
        let report = match validate_capture_complete(
            success.complete,
            request_id,
            expected_samples,
            expected_bytes,
            success.observed_bytes,
            &success.observed_sha256,
        ) {
            Ok(report) => report,
            Err(error) => return self.fail(error),
        };
        self.state = SessionState::CaptureComplete;
        self.next_id += 1;
        self.terminal = false;
        Ok((report, success.sink))
    }

    /// Disable selected RX after start or capture and enter ReceiveStopped.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from ReceiveActive or
    /// CaptureComplete with an exact response.
    pub fn stop_receive(&mut self) -> Result<(), ClientError> {
        self.require_state(
            "stop receive",
            "ReceiveActive or CaptureComplete",
            |state| {
                matches!(
                    state,
                    SessionState::ReceiveActive | SessionState::CaptureComplete
                )
            },
        )?;
        let request_id = self.next_id;
        match self.exchange(RequestOperation::StopReceive {})? {
            ResponseKind::State {
                state: SessionState::ReceiveStopped,
            } => {
                self.state = SessionState::ReceiveStopped;
                Ok(())
            }
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("stop result {actual:?} is not ReceiveStopped"),
            }),
        }
    }

    /// Restore the frozen stock image on the same handle.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from CustomVerified,
    /// ReceiveConfigured, or ReceiveStopped with an exact response.
    pub fn restore_stock(&mut self) -> Result<(), ClientError> {
        self.require_state(
            "restore stock",
            "CustomVerified, ReceiveConfigured, or ReceiveStopped",
            |state| {
                matches!(
                    state,
                    SessionState::CustomVerified
                        | SessionState::ReceiveConfigured
                        | SessionState::ReceiveStopped
                )
            },
        )?;
        let request_id = self.next_id;
        match self.exchange(RequestOperation::RestoreStock {})? {
            ResponseKind::State {
                state: SessionState::StockUnverified,
            } => {
                self.state = SessionState::StockUnverified;
                self.plan = None;
                Ok(())
            }
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("restore result {actual:?} is not StockUnverified"),
            }),
        }
    }

    /// Verify exact frozen stock identity.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from StockUnverified with an exact response.
    pub fn verify_stock(&mut self) -> Result<(), ClientError> {
        self.state_transition(
            "verify stock",
            RequestOperation::VerifyStock {},
            SessionState::StockUnverified,
            SessionState::StockVerified,
        )
    }

    /// Request orderly shutdown, then observe and reap the child within the same deadline.
    ///
    /// # Errors
    /// Returns [`ClientError`] unless called from Ready or StockVerified with
    /// an exact Closed response, successful exit, and completed I/O worker.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.require_state("shut down", "Ready or StockVerified", |state| {
            matches!(state, SessionState::Ready | SessionState::StockVerified)
        })?;
        let request_id = self.next_id;
        let deadline = Instant::now() + self.timeout;
        match self.exchange_until(RequestOperation::Shutdown {}, deadline)? {
            ResponseKind::State {
                state: SessionState::Closed,
            } => {}
            actual => {
                return self.fail(ClientError::Protocol {
                    request_id,
                    message: format!("shutdown result {actual:?} is not Closed"),
                });
            }
        }
        self.state = SessionState::Closed;
        self.terminal = true;
        self.tasks.take();
        let status = match self.wait_for_exit(request_id, deadline) {
            Ok(status) => status,
            Err(error) => return self.fail(error),
        };
        self.child.take();
        if !status.success() {
            return self.fail(ClientError::ChildExit { status });
        }
        if let Err(error) = self.finish_worker(request_id) {
            return self.fail(error);
        }
        self.cleanup_confirmed = true;
        self.cleanup_diagnostic = None;
        Ok(())
    }

    /// Force termination and reap the private child process group.
    ///
    /// This operation is idempotent after confirmed cleanup. An ambiguous
    /// process-group, reap or worker result remains cleanup-unconfirmed and a
    /// later call retries retained resources instead of reporting false
    /// success.
    ///
    /// # Errors
    /// Returns [`ClientError::Termination`] if any cleanup step reports failure.
    pub fn terminate(&mut self) -> Result<(), ClientError> {
        match self.abort_and_reap() {
            Some(message) => Err(ClientError::Termination { message }),
            None => Ok(()),
        }
    }

    fn require_state(
        &mut self,
        operation: &'static str,
        expected: &'static str,
        predicate: impl FnOnce(SessionState) -> bool,
    ) -> Result<(), ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        if predicate(self.state) {
            Ok(())
        } else {
            self.fail(ClientError::InvalidState {
                operation,
                expected,
                actual: self.state,
            })
        }
    }

    fn state_transition(
        &mut self,
        operation_name: &'static str,
        operation: RequestOperation,
        required: SessionState,
        next: SessionState,
    ) -> Result<(), ClientError> {
        self.require_state(operation_name, state_name(required), |state| {
            state == required
        })?;
        let request_id = self.next_id;
        match self.exchange(operation)? {
            ResponseKind::State { state } if state == next => {
                self.state = next;
                Ok(())
            }
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("transition result {actual:?} is not {next:?}"),
            }),
        }
    }

    fn self_loop(
        &mut self,
        operation_name: &'static str,
        operation: RequestOperation,
        required: SessionState,
    ) -> Result<(), ClientError> {
        self.require_state(operation_name, state_name(required), |state| {
            state == required
        })?;
        let request_id = self.next_id;
        match self.exchange(operation)? {
            ResponseKind::State { state } if state == required => Ok(()),
            actual => self.fail(ClientError::Protocol {
                request_id,
                message: format!("self-loop result {actual:?} is not {required:?}"),
            }),
        }
    }

    fn check_request_allowance(&mut self) -> Result<(), ClientError> {
        if self.request_count() >= MAX_PRIMARY_SESSION_REQUESTS as u64 {
            return self.fail(ClientError::RequestLimit);
        }
        Ok(())
    }

    fn exchange(&mut self, operation: RequestOperation) -> Result<ResponseKind, ClientError> {
        self.exchange_until(operation, Instant::now() + self.timeout)
    }

    fn exchange_until(
        &mut self,
        operation: RequestOperation,
        deadline: Instant,
    ) -> Result<ResponseKind, ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        self.check_request_allowance()?;
        let request_id = self.next_id;
        self.terminal = true;
        let (sender, receiver) = mpsc::sync_channel(1);
        let task = round_trip_task(Request::new(request_id, operation), sender);
        if let Err(task) = self.publish_task(task) {
            drop(task);
            return self.fail(ClientError::Publish { request_id });
        }
        let result = match recv_until(&receiver, deadline, request_id, self.timeout) {
            Ok(result) => result,
            Err(error) => return self.fail(error),
        };
        let response = match result {
            Ok(response) => response,
            Err(failure) => return self.fail(worker_error(failure, request_id)),
        };
        if Instant::now() >= deadline {
            return self.fail(ClientError::Timeout {
                request_id,
                timeout: self.timeout,
            });
        }
        let result = match validate_response_envelope(response, request_id) {
            Ok(result) => result,
            Err(error) => return self.fail(error),
        };
        self.next_id += 1;
        self.terminal = false;
        Ok(result)
    }

    fn publish_task(&self, task: WorkerTask) -> Result<(), WorkerTask> {
        let Some(tasks) = self.tasks.as_ref() else {
            return Err(task);
        };
        match tasks.try_send(task) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(task) | TrySendError::Disconnected(task)) => Err(task),
        }
    }

    fn finish_worker(&mut self, request_id: u64) -> Result<(), ClientError> {
        match (self.worker.is_some(), self.worker_completion.is_some()) {
            (false, false) => Ok(()),
            (true, true) => {
                let completion = self
                    .worker_completion
                    .as_ref()
                    .expect("worker completion presence checked")
                    .recv_timeout(WORKER_COMPLETION_TIMEOUT);
                match completion {
                    Ok(WorkerCompletion::Completed) => {
                        self.worker_completion.take();
                        self.worker
                            .take()
                            .expect("worker presence checked")
                            .join()
                            .map_err(|_| ClientError::WorkerPanicked)
                    }
                    Ok(WorkerCompletion::Panicked) => {
                        self.worker_completion.take();
                        let _ = self.worker.take().expect("worker presence checked").join();
                        Err(ClientError::WorkerPanicked)
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        Err(ClientError::WorkerCompletionTimeout {
                            timeout: WORKER_COMPLETION_TIMEOUT,
                        })
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.worker_completion.take();
                        let _ = self.worker.take().expect("worker presence checked").join();
                        Err(ClientError::WorkerStopped { request_id })
                    }
                }
            }
            _ => Err(ClientError::WorkerStopped { request_id }),
        }
    }

    #[cfg(any(
        target_os = "android",
        target_os = "freebsd",
        target_os = "haiku",
        all(target_os = "linux", not(target_env = "uclibc")),
    ))]
    fn wait_for_exit(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<ExitStatus, ClientError> {
        let process = i32::try_from(self.process_group).map_err(|_| ClientError::Protocol {
            request_id,
            message: "child process identifier exceeds platform range".to_owned(),
        })?;
        loop {
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            let observation = waitid(
                Id::Pid(Pid::from_raw(process)),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            )
            .map_err(|error| ClientError::Wait(io::Error::from_raw_os_error(error as i32)))?;
            match observation {
                WaitStatus::StillAlive => {
                    let observed = Instant::now();
                    if observed >= deadline {
                        return Err(ClientError::Timeout {
                            request_id,
                            timeout: self.timeout,
                        });
                    }
                    thread::sleep((deadline - observed).min(Duration::from_millis(2)));
                }
                WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _) => {
                    let mut diagnostics = Vec::new();
                    if self.process_group_active
                        && signal_private_process_group(self.process_group, &mut diagnostics)
                    {
                        self.process_group_active = false;
                    }
                    if !diagnostics.is_empty() {
                        return Err(ClientError::Termination {
                            message: diagnostics.join("; "),
                        });
                    }
                    return self
                        .child
                        .as_mut()
                        .ok_or(ClientError::Terminal)?
                        .wait()
                        .map_err(ClientError::Wait);
                }
                other => {
                    return Err(ClientError::Protocol {
                        request_id,
                        message: format!("unexpected wait status {other:?}"),
                    });
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn wait_for_exit(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<ExitStatus, ClientError> {
        loop {
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or(ClientError::Terminal)?
                .try_wait()
                .map_err(ClientError::Wait)?
            {
                let mut diagnostics = Vec::new();
                if self.process_group_active
                    && signal_private_process_group(self.process_group, &mut diagnostics)
                {
                    self.process_group_active = false;
                }
                if !diagnostics.is_empty() {
                    return Err(ClientError::Termination {
                        message: diagnostics.join("; "),
                    });
                }
                return Ok(status);
            }
            let observed = Instant::now();
            if observed >= deadline {
                return Err(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            thread::sleep((deadline - observed).min(Duration::from_millis(2)));
        }
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "android",
            target_os = "freebsd",
            target_os = "haiku",
            all(target_os = "linux", not(target_env = "uclibc")),
        )),
    ))]
    fn wait_for_exit(
        &mut self,
        _request_id: u64,
        _deadline: Instant,
    ) -> Result<ExitStatus, ClientError> {
        Err(ClientError::Wait(io::Error::new(
            io::ErrorKind::Unsupported,
            "safe non-reaping child-status observation is unavailable",
        )))
    }

    fn fail<T>(&mut self, primary: ClientError) -> Result<T, ClientError> {
        Err(with_cleanup(primary, self.abort_and_reap()))
    }

    fn abort_and_reap(&mut self) -> Option<String> {
        if self.cleanup_confirmed {
            return None;
        }
        self.terminal = true;
        self.tasks.take();
        let mut diagnostics = Vec::new();
        if self.process_group_active
            && signal_private_process_group(self.process_group, &mut diagnostics)
        {
            self.process_group_active = false;
        }
        // Retain the leader as an unreaped PID anchor if group signaling was
        // ambiguous. Reaping it first could allow that numeric group ID to be
        // reused before a later retry.
        if !self.process_group_active
            && let Some(child) = self.child.as_mut()
            && kill_and_reap(child, &mut diagnostics)
        {
            self.child.take();
        }
        if let Err(error) = self.finish_worker(self.next_id) {
            diagnostics.push(error.to_string());
        }
        let confirmed = !self.process_group_active
            && self.child.is_none()
            && self.worker.is_none()
            && self.worker_completion.is_none();
        if confirmed {
            self.cleanup_confirmed = true;
            self.cleanup_diagnostic = None;
            return (!diagnostics.is_empty()).then(|| diagnostics.join("; "));
        }

        let message = if diagnostics.is_empty() {
            self.cleanup_diagnostic
                .clone()
                .unwrap_or_else(|| "session cleanup remains unconfirmed".to_owned())
        } else {
            diagnostics.join("; ")
        };
        self.cleanup_diagnostic = Some(message.clone());
        Some(message)
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        if !self.cleanup_confirmed {
            let _ = self.abort_and_reap();
        }
    }
}

fn recv_until<T>(
    receiver: &Receiver<T>,
    deadline: Instant,
    request_id: u64,
    timeout: Duration,
) -> Result<T, ClientError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ClientError::Timeout {
            request_id,
            timeout,
        });
    }
    match receiver.recv_timeout(remaining) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(ClientError::Timeout {
            request_id,
            timeout,
        }),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ClientError::WorkerStopped { request_id }),
    }
}

fn worker_error(failure: WorkerFailure, request_id: u64) -> ClientError {
    match failure {
        WorkerFailure::Frame(source) => ClientError::Transport { request_id, source },
        WorkerFailure::EndOfStream(phase) => ClientError::EarlyExit { request_id, phase },
        WorkerFailure::RawRead(source) => ClientError::RawRead { request_id, source },
        WorkerFailure::Sink(source) => ClientError::Sink { request_id, source },
        WorkerFailure::StartResponse(response) => response_error(*response, request_id)
            .unwrap_or_else(|| ClientError::Protocol {
                request_id,
                message: "unexpected response before raw capture".to_owned(),
            }),
        WorkerFailure::Protocol(message) => ClientError::Protocol {
            request_id,
            message,
        },
    }
}

fn validate_response_envelope(
    response: Response,
    request_id: u64,
) -> Result<ResponseKind, ClientError> {
    if response.family() != PROTOCOL_FAMILY {
        return Err(ClientError::Protocol {
            request_id,
            message: "response protocol family differs".to_owned(),
        });
    }
    if response.version() != PROTOCOL_VERSION {
        return Err(ClientError::Protocol {
            request_id,
            message: format!("response version {} differs", response.version()),
        });
    }
    if response.id() != request_id {
        return Err(ClientError::Protocol {
            request_id,
            message: format!("response ID {} differs", response.id()),
        });
    }
    match response.into_parts().3 {
        ResponseKind::Error { code, message } => {
            if message.len() > MAX_ERROR_MESSAGE_BYTES {
                Err(ClientError::Protocol {
                    request_id,
                    message: "server diagnostic exceeds its UTF-8 byte bound".to_owned(),
                })
            } else {
                Err(ClientError::Server {
                    request_id,
                    code,
                    message,
                })
            }
        }
        result => Ok(result),
    }
}

fn response_error(response: Response, request_id: u64) -> Option<ClientError> {
    validate_response_envelope(response, request_id).err()
}

fn configured_matches(plan: ReceivePlan, readback: ConfiguredReceiveReadback) -> bool {
    readback.channel() == plan.channel()
        && readback
            .center_frequency_hz()
            .abs_diff(plan.center_frequency_hz())
            <= MAX_FREQUENCY_ERROR_HZ
        && readback.sample_rate_hz() == SOURCE_SAMPLE_RATE_HZ
        && readback.bandwidth_hz() == SOURCE_BANDWIDTH_HZ
}

fn validate_capture_complete(
    response: Response,
    request_id: u64,
    expected_samples: u32,
    expected_bytes: u64,
    observed_bytes: u64,
    observed_sha256: &str,
) -> Result<CaptureReport, ClientError> {
    if response.family() != PROTOCOL_FAMILY
        || response.version() != PROTOCOL_VERSION
        || response.id() != request_id
    {
        return Err(ClientError::Protocol {
            request_id,
            message: "CaptureComplete envelope differs".to_owned(),
        });
    }
    match response.into_parts().3 {
        ResponseKind::CaptureComplete {
            state: SessionState::CaptureComplete,
            report,
        } if observed_bytes == expected_bytes
            && report.transport_samples() == expected_samples
            && report.raw_bytes() == expected_bytes
            && exact_lower_hex(report.sha256(), 64)
            && report.sha256() == observed_sha256 => Ok(report),
        ResponseKind::Error { .. } => Err(ClientError::Protocol {
            request_id,
            message: "error frame after CaptureStarted is forbidden; backend failure must close the stream".to_owned(),
        }),
        actual => Err(ClientError::Protocol {
            request_id,
            message: format!("completion result {actual:?} or independent raw evidence differs"),
        }),
    }
}

const fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Ready => "Ready",
        SessionState::CustomUnverified => "CustomUnverified",
        SessionState::CustomVerified => "CustomVerified",
        SessionState::ReceiveConfigured => "ReceiveConfigured",
        SessionState::ReceiveActive => "ReceiveActive",
        SessionState::CaptureComplete => "CaptureComplete",
        SessionState::ReceiveStopped => "ReceiveStopped",
        SessionState::StockUnverified => "StockUnverified",
        SessionState::StockVerified => "StockVerified",
        SessionState::Closed => "Closed",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    fn bundle() -> BundleFacts {
        BundleFacts::new(
            LibraryFacts::new(
                REQUIRED_LIBRARY_VERSION,
                "1".repeat(64),
                REQUIRED_LIBRARY_MARKER,
            ),
            ImageFacts::new("2".repeat(64), REQUIRED_STOCK_BYTES),
            ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
        )
        .unwrap()
    }

    fn plan() -> ReceivePlan {
        ReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MIN_DATA_RECORDS).unwrap()
    }

    fn framed<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, value).unwrap();
        bytes
    }

    fn primary_error(mut error: &ClientError) -> &ClientError {
        while let ClientError::Cleanup { primary, .. } = error {
            error = primary;
        }
        error
    }

    #[test]
    fn framing_round_trip_and_truncation_are_exact() {
        let request = Request::new(
            7,
            RequestOperation::WriteShadow {
                channel: ShadowChannel::Channel3,
                value: 0x1122_3344,
            },
        );
        let bytes = framed(&request);
        let mut input = bytes.as_slice();
        assert_eq!(read_frame::<_, Request>(&mut input).unwrap(), Some(request));
        assert_eq!(read_frame::<_, Request>(&mut input).unwrap(), None);

        for length in 1..FRAME_HEADER_BYTES {
            assert!(matches!(
                read_frame::<_, Request>(&mut &bytes[..length]),
                Err(FrameError::TruncatedHeader { received }) if received == length
            ));
        }
        let declared = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert!(matches!(
            read_frame::<_, Request>(&mut &bytes[..bytes.len() - 1]),
            Err(FrameError::TruncatedPayload { expected, received })
                if expected == declared && received == declared - 1
        ));
        assert!(matches!(
            read_frame::<_, Request>(&mut [0_u8; 4].as_slice()),
            Err(FrameError::EmptyFrame)
        ));
        let oversized = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert!(matches!(
            read_frame::<_, Request>(&mut oversized.as_slice()),
            Err(FrameError::FrameTooLarge { length, maximum })
                if length == MAX_FRAME_BYTES + 1 && maximum == MAX_FRAME_BYTES
        ));
    }

    #[test]
    fn serialization_bound_stops_before_transport_publication() {
        let response = Response::error(1, ErrorCode::Backend, "x".repeat(10_000));
        let mut output = Vec::new();
        write_frame(&mut output, &response).unwrap();
        assert!(output.len() <= MAX_FRAME_BYTES + FRAME_HEADER_BYTES);
        let decoded: Response = read_frame(&mut output.as_slice()).unwrap().unwrap();
        match decoded.result() {
            ResponseKind::Error { message, .. } => {
                assert_eq!(message.len(), MAX_ERROR_MESSAGE_BYTES);
                assert!(message.ends_with("..."));
            }
            other => panic!("unexpected result {other:?}"),
        }

        let value = "x".repeat(MAX_FRAME_BYTES + 1);
        let mut untouched = Vec::new();
        assert!(matches!(
            write_frame(&mut untouched, &value),
            Err(FrameError::FrameTooLarge { .. })
        ));
        assert!(untouched.is_empty());
    }

    #[test]
    fn schema_json_goldens_are_frozen() {
        let request = Request::new(1, RequestOperation::ConfigureReceive { plan: plan() });
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"family":"bladerf_profile2_session","version":1,"id":1,"operation":{"kind":"configure_receive","plan":{"channel":"rx2","center_frequency_hz":98100000,"gain_db":40,"data_records":100000}}}"#
        );
        let response = Response::new(
            9,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: plan().expected_transport_samples(),
                raw_bytes: plan().expected_raw_bytes(),
            },
        );
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"family":"bladerf_profile2_session","version":1,"id":9,"result":{"kind":"capture_started","state":"receive_active","transport_samples":800008,"raw_bytes":3200032}}"#
        );
        assert_eq!(MAX_PRIMARY_SESSION_REQUESTS, 4_126);
        assert_eq!(ServerMode::FixturePrimary.max_session_requests(), 4_126);
        assert_eq!(ServerMode::NativePrimary.max_session_requests(), 4_126);
    }

    #[test]
    fn every_mode_state_operation_result_and_error_spelling_is_frozen() {
        for (mode, spelling) in [
            (ServerMode::FixturePrimary, r#""fixture_primary""#),
            (ServerMode::NativePrimary, r#""native_primary""#),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), spelling);
        }
        for (state, spelling) in [
            (SessionState::Ready, "ready"),
            (SessionState::CustomUnverified, "custom_unverified"),
            (SessionState::CustomVerified, "custom_verified"),
            (SessionState::ReceiveConfigured, "receive_configured"),
            (SessionState::ReceiveActive, "receive_active"),
            (SessionState::CaptureComplete, "capture_complete"),
            (SessionState::ReceiveStopped, "receive_stopped"),
            (SessionState::StockUnverified, "stock_unverified"),
            (SessionState::StockVerified, "stock_verified"),
            (SessionState::Closed, "closed"),
        ] {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{spelling}\"")
            );
        }
        for (code, spelling) in [
            (ErrorCode::Protocol, "protocol"),
            (ErrorCode::InvalidState, "invalid_state"),
            (ErrorCode::RequestLimit, "request_limit"),
            (ErrorCode::Backend, "backend"),
        ] {
            assert_eq!(
                serde_json::to_string(&code).unwrap(),
                format!("\"{spelling}\"")
            );
        }

        let operations = [
            (
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
                "hello",
            ),
            (RequestOperation::LoadProfile2 {}, "load_profile2"),
            (RequestOperation::VerifyProfile2 {}, "verify_profile2"),
            (
                RequestOperation::ReadRegister {
                    register: ReadRegister::Id,
                },
                "read_register",
            ),
            (
                RequestOperation::WriteShadow {
                    channel: ShadowChannel::Channel0,
                    value: 0,
                },
                "write_shadow",
            ),
            (RequestOperation::Commit {}, "commit"),
            (
                RequestOperation::ConfigureReceive { plan: plan() },
                "configure_receive",
            ),
            (RequestOperation::StartReceive {}, "start_receive"),
            (RequestOperation::Capture {}, "capture"),
            (RequestOperation::StopReceive {}, "stop_receive"),
            (RequestOperation::RestoreStock {}, "restore_stock"),
            (RequestOperation::VerifyStock {}, "verify_stock"),
            (RequestOperation::Shutdown {}, "shutdown"),
        ];
        for (operation, spelling) in operations {
            assert_eq!(serde_json::to_value(operation).unwrap()["kind"], spelling);
        }

        let configured = ConfiguredReceiveReadback::new(
            ReceiveChannel::Rx1,
            1,
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        let results = [
            (
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: SessionState::Ready,
                    bundle: bundle(),
                },
                "hello",
            ),
            (
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
                "state",
            ),
            (
                ResponseKind::RegisterValue {
                    state: SessionState::CustomVerified,
                    register: ReadRegister::Id,
                    value: 0,
                },
                "register_value",
            ),
            (
                ResponseKind::Configured {
                    state: SessionState::ReceiveConfigured,
                    readback: configured,
                },
                "configured",
            ),
            (
                ResponseKind::Active {
                    state: SessionState::ReceiveActive,
                    readback: ActiveReceiveReadback::new(configured, 0),
                },
                "active",
            ),
            (
                ResponseKind::CaptureStarted {
                    state: SessionState::ReceiveActive,
                    transport_samples: 1,
                    raw_bytes: 4,
                },
                "capture_started",
            ),
            (
                ResponseKind::CaptureComplete {
                    state: SessionState::CaptureComplete,
                    report: CaptureReport::new(1, 4, "0".repeat(64)),
                },
                "capture_complete",
            ),
            (
                ResponseKind::Error {
                    code: ErrorCode::Backend,
                    message: "x".to_owned(),
                },
                "error",
            ),
        ];
        for (result, spelling) in results {
            assert_eq!(serde_json::to_value(result).unwrap()["kind"], spelling);
        }
    }

    #[test]
    fn register_abi_and_plan_bounds_are_exact() {
        let reads = [
            (ReadRegister::Id, 0x00),
            (ReadRegister::Capabilities, 0x04),
            (ReadRegister::Shadow0, 0x10),
            (ReadRegister::Shadow1, 0x14),
            (ReadRegister::Shadow2, 0x18),
            (ReadRegister::Shadow3, 0x1c),
            (ReadRegister::Status, 0x24),
            (ReadRegister::AppliedEpoch, 0x28),
            (ReadRegister::OutcomeEpoch, 0x2c),
            (ReadRegister::Applied0, 0x30),
            (ReadRegister::Applied1, 0x34),
            (ReadRegister::Applied2, 0x38),
            (ReadRegister::Applied3, 0x3c),
        ];
        for (register, offset) in reads {
            assert_eq!(register.byte_offset(), offset);
            assert_eq!(ReadRegister::try_from(offset), Ok(register));
        }
        assert!(ReadRegister::try_from(COMMIT_BYTE_OFFSET).is_err());
        for (channel, offset) in [
            (ShadowChannel::Channel0, 0x10),
            (ShadowChannel::Channel1, 0x14),
            (ShadowChannel::Channel2, 0x18),
            (ShadowChannel::Channel3, 0x1c),
        ] {
            assert_eq!(channel.byte_offset(), offset);
            assert_eq!(ShadowChannel::try_from(offset), Ok(channel));
        }
        assert_eq!(COMMIT_MAGIC, 0x434d_4954);

        let minimum =
            ReceivePlan::new(ReceiveChannel::Rx1, 1, MIN_GAIN_DB, MIN_DATA_RECORDS).unwrap();
        assert_eq!(minimum.expected_transport_samples(), 800_008);
        assert_eq!(minimum.expected_raw_bytes(), 3_200_032);
        let maximum =
            ReceivePlan::new(ReceiveChannel::Rx2, u64::MAX, MAX_GAIN_DB, MAX_DATA_RECORDS).unwrap();
        assert_eq!(maximum.expected_transport_samples(), 80_000_008);
        assert_eq!(maximum.expected_raw_bytes(), 320_000_032);
        assert!(ReceivePlan::new(ReceiveChannel::Rx1, 0, 0, MIN_DATA_RECORDS).is_err());
        assert!(
            ReceivePlan::new(ReceiveChannel::Rx1, 1, MIN_GAIN_DB - 1, MIN_DATA_RECORDS).is_err()
        );
        assert!(ReceivePlan::new(ReceiveChannel::Rx1, 1, 0, MIN_DATA_RECORDS - 1).is_err());
        assert!(
            serde_json::from_str::<ReceivePlan>(
                r#"{"channel":"rx1","center_frequency_hz":1,"gain_db":0,"data_records":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn bundle_constructor_enforces_every_exact_fact() {
        let library = || {
            LibraryFacts::new(
                REQUIRED_LIBRARY_VERSION,
                "1".repeat(64),
                REQUIRED_LIBRARY_MARKER,
            )
        };
        let custom = || ImageFacts::new("2".repeat(64), 1);
        let stock = || ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES);
        assert!(BundleFacts::new(library(), custom(), stock()).is_ok());
        assert!(matches!(
            BundleFacts::new(
                LibraryFacts::new("2.6.1", "1".repeat(64), REQUIRED_LIBRARY_MARKER),
                custom(),
                stock()
            ),
            Err(ClientError::InvalidLibraryVersion)
        ));
        assert!(matches!(
            BundleFacts::new(
                LibraryFacts::new(
                    REQUIRED_LIBRARY_VERSION,
                    "A".repeat(64),
                    REQUIRED_LIBRARY_MARKER
                ),
                custom(),
                stock()
            ),
            Err(ClientError::InvalidLibrarySha256)
        ));
        assert!(matches!(
            BundleFacts::new(
                LibraryFacts::new(REQUIRED_LIBRARY_VERSION, "1".repeat(64), 0),
                custom(),
                stock()
            ),
            Err(ClientError::InvalidLibraryMarker)
        ));
        assert!(matches!(
            BundleFacts::new(library(), ImageFacts::new("bad", 1), stock()),
            Err(ClientError::InvalidCustomSha256)
        ));
        assert!(matches!(
            BundleFacts::new(library(), ImageFacts::new("2".repeat(64), 0), stock()),
            Err(ClientError::InvalidCustomBytes { bytes: 0 })
        ));
        assert!(matches!(
            BundleFacts::new(
                library(),
                ImageFacts::new("2".repeat(64), MAX_IMAGE_BYTES + 1),
                stock()
            ),
            Err(ClientError::InvalidCustomBytes { bytes }) if bytes == MAX_IMAGE_BYTES + 1
        ));
        assert!(matches!(
            BundleFacts::new(
                library(),
                custom(),
                ImageFacts::new("3".repeat(64), REQUIRED_STOCK_BYTES)
            ),
            Err(ClientError::InvalidStockSha256)
        ));
        assert!(matches!(
            BundleFacts::new(
                library(),
                custom(),
                ImageFacts::new(REQUIRED_STOCK_SHA256, 1)
            ),
            Err(ClientError::InvalidStockBytes { bytes: 1 })
        ));
        assert!(matches!(
            BundleFacts::new(
                library(),
                ImageFacts::new(REQUIRED_STOCK_SHA256, 1),
                stock()
            ),
            Err(ClientError::AliasedImageSha256)
        ));
    }

    #[test]
    fn command_builder_preserves_arguments_and_sanitizes_loader_environment() {
        let command = build_command("/sentinel/session", ["fixture-primary", "two words"]);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["fixture-primary", "two words"]
                .iter()
                .map(OsStr::new)
                .collect::<Vec<_>>()
        );
        let removals: Vec<_> = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect();
        for name in ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            assert!(removals.contains(&OsStr::new(name)));
        }
    }

    #[test]
    fn unsupported_nonreaping_wait_has_explicit_spawn_error() {
        require_nonreaping_child_wait(true).unwrap();
        let error = require_nonreaping_child_wait(false).unwrap_err();
        assert!(matches!(
            error,
            ClientError::Spawn(source) if source.kind() == io::ErrorKind::Unsupported
        ));
    }

    #[cfg(unix)]
    struct ScriptedChild {
        _directory: tempfile::TempDir,
        program: PathBuf,
        arguments: Vec<PathBuf>,
        captures: Vec<PathBuf>,
    }

    #[cfg(unix)]
    impl ScriptedChild {
        fn new(requests: &[Request], outputs: &[Vec<u8>], suffix: &str) -> Self {
            assert_eq!(requests.len(), outputs.len());
            let directory = tempfile::tempdir().unwrap();
            let script_path = directory.path().join("session-child.sh");
            let program = PathBuf::from("/bin/sh");
            let mut script = String::from("#!/bin/sh\nset -eu\n");
            let mut arguments = vec![script_path.clone()];
            let mut captures = Vec::new();
            for (index, (request, output)) in requests.iter().zip(outputs).enumerate() {
                let output_path = directory.path().join(format!("output-{index}.bin"));
                let capture_path = directory.path().join(format!("request-{index}.bin"));
                fs::write(&output_path, output).unwrap();
                let request_bytes = framed(request).len();
                script.push_str(&format!(
                    "dd bs=1 count={request_bytes} of=\"$2\" 2>/dev/null\ndd if=\"$1\" bs=65536 2>/dev/null\nshift 2\n"
                ));
                arguments.push(output_path);
                arguments.push(capture_path.clone());
                captures.push(capture_path);
            }
            script.push_str(suffix);
            script.push('\n');
            fs::write(script_path, script).unwrap();
            Self {
                _directory: directory,
                program,
                arguments,
                captures,
            }
        }

        fn assert_requests(&self, expected: &[Request]) {
            for (path, expected) in self.captures.iter().zip(expected) {
                let bytes = fs::read(path).unwrap();
                assert_eq!(
                    read_frame::<_, Request>(&mut bytes.as_slice()).unwrap(),
                    Some(expected.clone())
                );
            }
        }
    }

    #[cfg(unix)]
    fn path_arguments(paths: &[PathBuf]) -> impl Iterator<Item = &Path> {
        paths.iter().map(PathBuf::as_path)
    }

    #[test]
    fn unknown_fields_and_cross_family_values_are_rejected() {
        assert!(serde_json::from_str::<Request>(
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"shutdown"}}"#
        ).is_err());
        assert!(serde_json::from_str::<Request>(
            r#"{"family":"bladerf_profile2_session","version":1,"id":1,"operation":{"kind":"shutdown"},"extra":true}"#
        ).is_err());
        assert!(
            serde_json::from_str::<RequestOperation>(r#"{"kind":"commit","value":1}"#).is_err()
        );
        assert!(serde_json::from_str::<ReceivePlan>(
            r#"{"channel":"rx1","center_frequency_hz":1,"gain_db":0,"data_records":100000,"extra":0}"#
        ).is_err());
    }

    #[cfg(unix)]
    fn capture_prefix(capture_output: Vec<u8>, suffix: &str) -> ScriptedChild {
        let receive = plan();
        let requests = vec![
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(4, RequestOperation::ConfigureReceive { plan: receive }),
            Request::new(5, RequestOperation::StartReceive {}),
            Request::new(6, RequestOperation::Capture {}),
        ];
        let configured = ConfiguredReceiveReadback::new(
            receive.channel(),
            receive.center_frequency_hz(),
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        let outputs = vec![
            framed(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: SessionState::Ready,
                    bundle: bundle(),
                },
            )),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
            framed(&Response::new(
                3,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                4,
                ResponseKind::Configured {
                    state: SessionState::ReceiveConfigured,
                    readback: configured,
                },
            )),
            framed(&Response::new(
                5,
                ResponseKind::Active {
                    state: SessionState::ReceiveActive,
                    readback: ActiveReceiveReadback::new(configured, receive.gain_db()),
                },
            )),
            capture_output,
        ];
        ScriptedChild::new(&requests, &outputs, suffix)
    }

    #[cfg(unix)]
    fn advance_to_active(child: &ScriptedChild, timeout: Duration) -> SessionClient {
        let expected = bundle();
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            timeout,
        )
        .unwrap();
        client.load_profile2().unwrap();
        client.verify_profile2().unwrap();
        client.configure_receive(plan()).unwrap();
        client.start_receive().unwrap();
        client
    }

    fn successful_capture_output(request_id: u64, raw: &[u8]) -> Vec<u8> {
        let receive = plan();
        assert_eq!(raw.len() as u64, receive.expected_raw_bytes());
        let digest = format!("{:x}", Sha256::digest(raw));
        let mut output = framed(&Response::new(
            request_id,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: receive.expected_transport_samples(),
                raw_bytes: receive.expected_raw_bytes(),
            },
        ));
        output.extend_from_slice(raw);
        output.extend_from_slice(&framed(&Response::new(
            request_id,
            ResponseKind::CaptureComplete {
                state: SessionState::CaptureComplete,
                report: CaptureReport::new(
                    receive.expected_transport_samples(),
                    receive.expected_raw_bytes(),
                    digest,
                ),
            },
        )));
        output
    }

    #[cfg(unix)]
    #[test]
    fn client_runs_exact_complete_lifecycle_and_returns_validated_sink() {
        let receive = plan();
        let raw: Vec<u8> = (0..receive.expected_raw_bytes())
            .map(|index| (index % 251) as u8)
            .collect();
        let digest = format!("{:x}", Sha256::digest(&raw));
        let configured = ConfiguredReceiveReadback::new(
            receive.channel(),
            receive.center_frequency_hz(),
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        let requests = vec![
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(
                4,
                RequestOperation::ReadRegister {
                    register: ReadRegister::Status,
                },
            ),
            Request::new(
                5,
                RequestOperation::WriteShadow {
                    channel: ShadowChannel::Channel2,
                    value: 0x1234_5678,
                },
            ),
            Request::new(6, RequestOperation::Commit {}),
            Request::new(7, RequestOperation::ConfigureReceive { plan: receive }),
            Request::new(8, RequestOperation::StartReceive {}),
            Request::new(9, RequestOperation::Capture {}),
            Request::new(10, RequestOperation::StopReceive {}),
            Request::new(11, RequestOperation::RestoreStock {}),
            Request::new(12, RequestOperation::VerifyStock {}),
            Request::new(13, RequestOperation::Shutdown {}),
        ];
        let mut capture = framed(&Response::new(
            9,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: receive.expected_transport_samples(),
                raw_bytes: receive.expected_raw_bytes(),
            },
        ));
        capture.extend_from_slice(&raw);
        capture.extend_from_slice(&framed(&Response::new(
            9,
            ResponseKind::CaptureComplete {
                state: SessionState::CaptureComplete,
                report: CaptureReport::new(
                    receive.expected_transport_samples(),
                    receive.expected_raw_bytes(),
                    &digest,
                ),
            },
        )));
        let outputs = vec![
            framed(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: SessionState::Ready,
                    bundle: bundle(),
                },
            )),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
            framed(&Response::new(
                3,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                4,
                ResponseKind::RegisterValue {
                    state: SessionState::CustomVerified,
                    register: ReadRegister::Status,
                    value: 0xa5a5_5a5a,
                },
            )),
            framed(&Response::new(
                5,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                6,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                7,
                ResponseKind::Configured {
                    state: SessionState::ReceiveConfigured,
                    readback: configured,
                },
            )),
            framed(&Response::new(
                8,
                ResponseKind::Active {
                    state: SessionState::ReceiveActive,
                    readback: ActiveReceiveReadback::new(configured, receive.gain_db()),
                },
            )),
            capture,
            framed(&Response::new(
                10,
                ResponseKind::State {
                    state: SessionState::ReceiveStopped,
                },
            )),
            framed(&Response::new(
                11,
                ResponseKind::State {
                    state: SessionState::StockUnverified,
                },
            )),
            framed(&Response::new(
                12,
                ResponseKind::State {
                    state: SessionState::StockVerified,
                },
            )),
            framed(&Response::new(
                13,
                ResponseKind::State {
                    state: SessionState::Closed,
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "exit 0");
        let expected = bundle();
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(3),
        )
        .unwrap();
        client.load_profile2().unwrap();
        client.verify_profile2().unwrap();
        assert_eq!(
            client.read_register(ReadRegister::Status).unwrap(),
            0xa5a5_5a5a
        );
        client
            .write_shadow(ShadowChannel::Channel2, 0x1234_5678)
            .unwrap();
        client.commit().unwrap();
        client.configure_receive(receive).unwrap();
        client.start_receive().unwrap();
        let (report, sink) = client.capture(Vec::new()).unwrap();
        assert_eq!(sink, raw);
        assert_eq!(report.sha256(), digest);
        assert_eq!(client.state(), SessionState::CaptureComplete);
        client.stop_receive().unwrap();
        client.restore_stock().unwrap();
        client.verify_stock().unwrap();
        client.shutdown().unwrap();
        assert_eq!(client.state(), SessionState::Closed);
        assert!(client.is_terminal());
        assert!(client.is_cleanup_confirmed());
        assert_eq!(client.request_count(), 13);
        child.assert_requests(&requests);
    }

    #[derive(Debug)]
    struct ProbeSink {
        writes: Arc<AtomicUsize>,
        dropped: Arc<AtomicBool>,
        fail: bool,
    }

    impl Write for ProbeSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.fail {
                return Err(io::Error::other("injected sink failure"));
            }
            self.writes.fetch_add(bytes.len(), Ordering::SeqCst);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for ProbeSink {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[cfg(unix)]
    #[test]
    fn wrong_capture_started_extents_consume_no_raw_and_drop_sink() {
        let receive = plan();
        for (samples, bytes) in [
            (
                receive.expected_transport_samples() - 1,
                receive.expected_raw_bytes(),
            ),
            (
                receive.expected_transport_samples(),
                receive.expected_raw_bytes() - 4,
            ),
        ] {
            let output = framed(&Response::new(
                6,
                ResponseKind::CaptureStarted {
                    state: SessionState::ReceiveActive,
                    transport_samples: samples,
                    raw_bytes: bytes,
                },
            ));
            let child = capture_prefix(output, "sleep 10");
            let mut client = advance_to_active(&child, Duration::from_secs(1));
            let writes = Arc::new(AtomicUsize::new(0));
            let dropped = Arc::new(AtomicBool::new(false));
            let error = client
                .capture(ProbeSink {
                    writes: Arc::clone(&writes),
                    dropped: Arc::clone(&dropped),
                    fail: false,
                })
                .unwrap_err();
            assert!(
                matches!(
                    primary_error(&error),
                    ClientError::Protocol { request_id: 6, .. }
                ),
                "{error:?}"
            );
            assert_eq!(writes.load(Ordering::SeqCst), 0);
            assert!(dropped.load(Ordering::SeqCst));
            assert!(client.is_terminal());
        }
    }

    #[cfg(unix)]
    #[test]
    fn typed_error_before_capture_started_is_preserved_and_sink_is_dropped() {
        let child = capture_prefix(
            framed(&Response::error(6, ErrorCode::Backend, "capture rejected")),
            "sleep 10",
        );
        let mut client = advance_to_active(&child, Duration::from_secs(1));
        let writes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let error = client
            .capture(ProbeSink {
                writes: Arc::clone(&writes),
                dropped: Arc::clone(&dropped),
                fail: false,
            })
            .unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Server { request_id: 6, code: ErrorCode::Backend, message }
                if message == "capture rejected"
        ));
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn raw_truncation_is_terminal_and_partial_sink_is_invalid() {
        let receive = plan();
        let mut output = framed(&Response::new(
            6,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: receive.expected_transport_samples(),
                raw_bytes: receive.expected_raw_bytes(),
            },
        ));
        output.extend_from_slice(&[0x5a; 31]);
        let child = capture_prefix(output, "exit 0");
        let mut client = advance_to_active(&child, Duration::from_secs(1));
        let writes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let error = client
            .capture(ProbeSink {
                writes: Arc::clone(&writes),
                dropped: Arc::clone(&dropped),
                fail: false,
            })
            .unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::EarlyExit {
                    request_id: 6,
                    phase: "raw capture"
                }
            ),
            "{error:?}"
        );
        assert_eq!(writes.load(Ordering::SeqCst), 31);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(client.is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn sink_failure_consumes_and_drops_partial_sink() {
        let receive = plan();
        let raw = vec![0x33; receive.expected_raw_bytes() as usize];
        let child = capture_prefix(successful_capture_output(6, &raw), "sleep 10");
        let mut client = advance_to_active(&child, Duration::from_secs(2));
        let writes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let error = client
            .capture(ProbeSink {
                writes: Arc::clone(&writes),
                dropped: Arc::clone(&dropped),
                fail: true,
            })
            .unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Sink { request_id: 6, .. }
            ),
            "{error:?}"
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(client.is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn completion_requires_same_id_state_extent_and_digest() {
        let receive = plan();
        let raw = vec![0x44; receive.expected_raw_bytes() as usize];
        let digest = format!("{:x}", Sha256::digest(&raw));
        let cases = [
            Response::new(
                7,
                ResponseKind::CaptureComplete {
                    state: SessionState::CaptureComplete,
                    report: CaptureReport::new(
                        receive.expected_transport_samples(),
                        receive.expected_raw_bytes(),
                        &digest,
                    ),
                },
            ),
            Response::new(
                6,
                ResponseKind::CaptureComplete {
                    state: SessionState::ReceiveActive,
                    report: CaptureReport::new(
                        receive.expected_transport_samples(),
                        receive.expected_raw_bytes(),
                        &digest,
                    ),
                },
            ),
            Response::new(
                6,
                ResponseKind::CaptureComplete {
                    state: SessionState::CaptureComplete,
                    report: CaptureReport::new(
                        receive.expected_transport_samples() - 1,
                        receive.expected_raw_bytes(),
                        &digest,
                    ),
                },
            ),
            Response::new(
                6,
                ResponseKind::CaptureComplete {
                    state: SessionState::CaptureComplete,
                    report: CaptureReport::new(
                        receive.expected_transport_samples(),
                        receive.expected_raw_bytes() - 4,
                        &digest,
                    ),
                },
            ),
            Response::new(
                6,
                ResponseKind::CaptureComplete {
                    state: SessionState::CaptureComplete,
                    report: CaptureReport::new(
                        receive.expected_transport_samples(),
                        receive.expected_raw_bytes(),
                        "0".repeat(64),
                    ),
                },
            ),
        ];
        for completion in cases {
            let mut output = framed(&Response::new(
                6,
                ResponseKind::CaptureStarted {
                    state: SessionState::ReceiveActive,
                    transport_samples: receive.expected_transport_samples(),
                    raw_bytes: receive.expected_raw_bytes(),
                },
            ));
            output.extend_from_slice(&raw);
            output.extend_from_slice(&framed(&completion));
            let child = capture_prefix(output, "sleep 10");
            let mut client = advance_to_active(&child, Duration::from_secs(2));
            let error = client.capture(Vec::new()).unwrap_err();
            assert!(
                matches!(
                    primary_error(&error),
                    ClientError::Protocol { request_id: 6, .. }
                ),
                "{error:?}"
            );
            assert!(client.is_terminal());
        }
    }

    #[cfg(unix)]
    #[test]
    fn error_frame_after_capture_started_is_forbidden() {
        let receive = plan();
        let raw = vec![0x55; receive.expected_raw_bytes() as usize];
        let mut output = framed(&Response::new(
            6,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: receive.expected_transport_samples(),
                raw_bytes: receive.expected_raw_bytes(),
            },
        ));
        output.extend_from_slice(&raw);
        output.extend_from_slice(&framed(&Response::error(6, ErrorCode::Backend, "late")));
        let child = capture_prefix(output, "sleep 10");
        let mut client = advance_to_active(&child, Duration::from_secs(2));
        let error = client.capture(Vec::new()).unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Protocol { request_id: 6, message }
                    if message.contains("forbidden")
            ),
            "{error:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_capture_complete_times_out_and_drops_sink() {
        let receive = plan();
        let raw = vec![0x66; receive.expected_raw_bytes() as usize];
        let mut output = framed(&Response::new(
            6,
            ResponseKind::CaptureStarted {
                state: SessionState::ReceiveActive,
                transport_samples: receive.expected_transport_samples(),
                raw_bytes: receive.expected_raw_bytes(),
            },
        ));
        output.extend_from_slice(&raw);
        let child = capture_prefix(output, "sleep 10");
        let mut client = advance_to_active(&child, Duration::from_millis(250));
        let writes = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let error = client
            .capture(ProbeSink {
                writes: Arc::clone(&writes),
                dropped: Arc::clone(&dropped),
                fail: false,
            })
            .unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Timeout { request_id: 6, .. }
            ),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            writes.load(Ordering::SeqCst),
            receive.expected_raw_bytes() as usize
        );
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn eof_malformed_response_and_timeout_are_terminal_and_bounded() {
        let expected = bundle();
        assert!(matches!(
            SessionClient::spawn(
                "true",
                [] as [&str; 0],
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_millis(250)
            ),
            Err(ClientError::EarlyExit { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));
        let request = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        );
        let malformed = ScriptedChild::new(&[request], &[vec![1, 0]], "exit 0");
        assert!(matches!(
            SessionClient::spawn(
                &malformed.program,
                path_arguments(&malformed.arguments),
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_millis(250)
            ),
            Err(ClientError::Transport { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));
        let started = Instant::now();
        assert!(matches!(
            SessionClient::spawn(
                "sleep",
                ["10"],
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_millis(100)
            ),
            Err(ClientError::Timeout { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_local_state_and_request_cap_terminalize_before_publication() {
        let hello = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        );
        let output = framed(&Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixturePrimary,
                state: SessionState::Ready,
                bundle: bundle(),
            },
        ));
        let child = ScriptedChild::new(
            std::slice::from_ref(&hello),
            std::slice::from_ref(&output),
            "sleep 10",
        );
        let expected = bundle();
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        let error = client.verify_profile2().unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::InvalidState {
                actual: SessionState::Ready,
                ..
            }
        ));
        assert!(client.is_terminal());

        let child = ScriptedChild::new(&[hello], &[output], "sleep 10");
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        client.next_id = MAX_PRIMARY_SESSION_REQUESTS as u64 + 1;
        let error = client.load_profile2().unwrap_err();
        assert!(matches!(primary_error(&error), ClientError::RequestLimit));
        assert!(client.is_terminal());
    }

    #[cfg(unix)]
    #[test]
    fn configured_active_register_and_simple_transition_mismatches_are_terminal() {
        let receive = plan();
        let exact = ConfiguredReceiveReadback::new(
            receive.channel(),
            receive.center_frequency_hz(),
            SOURCE_SAMPLE_RATE_HZ,
            SOURCE_BANDWIDTH_HZ,
        );
        for mismatch in [
            ConfiguredReceiveReadback::new(
                ReceiveChannel::Rx1,
                receive.center_frequency_hz(),
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                receive.channel(),
                receive.center_frequency_hz() + MAX_FREQUENCY_ERROR_HZ + 1,
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                receive.channel(),
                receive.center_frequency_hz(),
                SOURCE_SAMPLE_RATE_HZ - 1,
                SOURCE_BANDWIDTH_HZ,
            ),
            ConfiguredReceiveReadback::new(
                receive.channel(),
                receive.center_frequency_hz(),
                SOURCE_SAMPLE_RATE_HZ,
                SOURCE_BANDWIDTH_HZ - 1,
            ),
        ] {
            assert!(!configured_matches(receive, mismatch));
        }

        let hello = || {
            Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: SessionState::Ready,
                    bundle: bundle(),
                },
            )
        };
        let expected = bundle();

        let requests = [
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
        ];
        let outputs = [
            framed(&hello()),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::Ready,
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "sleep 10");
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            primary_error(&client.load_profile2().unwrap_err()),
            ClientError::Protocol { request_id: 2, .. }
        ));

        let requests = [
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(
                4,
                RequestOperation::ReadRegister {
                    register: ReadRegister::Status,
                },
            ),
        ];
        let outputs = [
            framed(&hello()),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
            framed(&Response::new(
                3,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                4,
                ResponseKind::RegisterValue {
                    state: SessionState::CustomVerified,
                    register: ReadRegister::Id,
                    value: 0,
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "sleep 10");
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        client.load_profile2().unwrap();
        client.verify_profile2().unwrap();
        assert!(matches!(
            primary_error(&client.read_register(ReadRegister::Status).unwrap_err()),
            ClientError::Protocol { request_id: 4, .. }
        ));

        let requests = [
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(4, RequestOperation::ConfigureReceive { plan: receive }),
        ];
        let outputs = [
            framed(&hello()),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
            framed(&Response::new(
                3,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                4,
                ResponseKind::Configured {
                    state: SessionState::ReceiveConfigured,
                    readback: ConfiguredReceiveReadback::new(
                        receive.channel(),
                        receive.center_frequency_hz(),
                        SOURCE_SAMPLE_RATE_HZ - 1,
                        SOURCE_BANDWIDTH_HZ,
                    ),
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "sleep 10");
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        client.load_profile2().unwrap();
        client.verify_profile2().unwrap();
        assert!(matches!(
            primary_error(&client.configure_receive(receive).unwrap_err()),
            ClientError::Protocol { request_id: 4, .. }
        ));

        let requests = [
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(4, RequestOperation::ConfigureReceive { plan: receive }),
            Request::new(5, RequestOperation::StartReceive {}),
        ];
        let outputs = [
            framed(&hello()),
            framed(&Response::new(
                2,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
            framed(&Response::new(
                3,
                ResponseKind::State {
                    state: SessionState::CustomVerified,
                },
            )),
            framed(&Response::new(
                4,
                ResponseKind::Configured {
                    state: SessionState::ReceiveConfigured,
                    readback: exact,
                },
            )),
            framed(&Response::new(
                5,
                ResponseKind::Active {
                    state: SessionState::ReceiveActive,
                    readback: ActiveReceiveReadback::new(exact, receive.gain_db() - 1),
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "sleep 10");
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        client.load_profile2().unwrap();
        client.verify_profile2().unwrap();
        client.configure_receive(receive).unwrap();
        assert!(matches!(
            primary_error(&client.start_receive().unwrap_err()),
            ClientError::Protocol { request_id: 5, .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn request_allowance_accepts_last_request_then_rejects_next() {
        let last_id = MAX_PRIMARY_SESSION_REQUESTS as u64;
        let requests = [
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(last_id, RequestOperation::LoadProfile2 {}),
        ];
        let outputs = [
            framed(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: SessionState::Ready,
                    bundle: bundle(),
                },
            )),
            framed(&Response::new(
                last_id,
                ResponseKind::State {
                    state: SessionState::CustomUnverified,
                },
            )),
        ];
        let child = ScriptedChild::new(&requests, &outputs, "sleep 10");
        let expected = bundle();
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        client.next_id = last_id;
        client.load_profile2().unwrap();
        assert_eq!(client.request_count(), last_id);
        let error = client.verify_profile2().unwrap_err();
        assert!(matches!(primary_error(&error), ClientError::RequestLimit));
        assert!(client.is_terminal());
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_terminate_drop_and_descendant_cleanup_are_bounded() {
        let request = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        );
        let output = framed(&Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixturePrimary,
                state: SessionState::Ready,
                bundle: bundle(),
            },
        ));
        let expected = bundle();
        let child = ScriptedChild::new(
            std::slice::from_ref(&request),
            std::slice::from_ref(&output),
            "sleep 10",
        );
        let mut client = SessionClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            ServerMode::FixturePrimary,
            &expected,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!client.is_cleanup_confirmed());
        let started = Instant::now();
        client.terminate().unwrap();
        assert!(client.is_cleanup_confirmed());
        client.terminate().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));

        let child = ScriptedChild::new(
            std::slice::from_ref(&request),
            std::slice::from_ref(&output),
            "sleep 10",
        );
        let started = Instant::now();
        {
            let _client = SessionClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_secs(1),
            )
            .unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(2));

        let started = Instant::now();
        assert!(
            SessionClient::spawn(
                "sh",
                ["-c", "sleep 30 & wait"],
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_millis(100),
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn worker_only_client(
        worker: Option<JoinHandle<()>>,
        completion: Option<Receiver<WorkerCompletion>>,
    ) -> SessionClient {
        SessionClient {
            child: None,
            tasks: None,
            worker,
            worker_completion: completion,
            process_group: 0,
            process_group_active: false,
            timeout: Duration::from_secs(1),
            next_id: 1,
            state: SessionState::Ready,
            terminal: true,
            plan: None,
            cleanup_confirmed: false,
            cleanup_diagnostic: None,
        }
    }

    #[test]
    fn worker_cleanup_ambiguity_is_sticky_until_a_retry_confirms_join() {
        let (sender, completion) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            release_receiver.recv().unwrap();
            sender.send(WorkerCompletion::Completed).unwrap();
        });
        let mut client = worker_only_client(Some(worker), Some(completion));

        for _ in 0..2 {
            let error = client.terminate().unwrap_err();
            assert!(matches!(error, ClientError::Termination { .. }));
            assert!(!client.is_cleanup_confirmed());
            assert!(client.worker.is_some());
            assert!(client.worker_completion.is_some());
        }

        release_sender.send(()).unwrap();
        client.terminate().unwrap();
        assert!(client.is_cleanup_confirmed());
        assert!(client.worker.is_none());
        assert!(client.worker_completion.is_none());
        client.terminate().unwrap();
    }

    #[test]
    fn unconfirmable_worker_ownership_never_becomes_false_success() {
        let (_sender, completion) = mpsc::sync_channel(1);
        let mut client = worker_only_client(None, Some(completion));
        for _ in 0..2 {
            let error = client.terminate().unwrap_err();
            assert!(matches!(error, ClientError::Termination { .. }));
            assert!(!client.is_cleanup_confirmed());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unconfirmable_process_group_never_becomes_false_success() {
        let mut client = worker_only_client(None, None);
        client.process_group = u32::MAX;
        client.process_group_active = true;
        for _ in 0..2 {
            let error = client.terminate().unwrap_err();
            assert!(matches!(error, ClientError::Termination { .. }));
            assert!(!client.is_cleanup_confirmed());
            assert!(client.process_group_active);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unconfirmable_process_group_retains_leader_pid_anchor() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let child_id = child.id();
        let mut client = worker_only_client(None, None);
        client.child = Some(child);
        client.process_group = u32::MAX;
        client.process_group_active = true;

        let result = client.terminate();
        let retained_child_id = client.child.as_ref().map(Child::id);
        let process_group_active = client.process_group_active;

        // Restore a confirmable state before asserting so a failed assertion
        // cannot leave the test child running.
        client.process_group_active = false;
        let cleanup = client.terminate();

        assert!(matches!(result, Err(ClientError::Termination { .. })));
        assert_eq!(retained_child_id, Some(child_id));
        assert!(process_group_active);
        cleanup.unwrap();
        assert!(client.is_cleanup_confirmed());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn timeout_kills_private_process_group_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("descendant.pid");
        let pid_argument = pid_path.to_str().unwrap();
        let expected = bundle();
        assert!(
            SessionClient::spawn(
                "sh",
                [
                    "-c",
                    "sleep 30 & echo $! > \"$1\"; wait",
                    "session-fixture",
                    pid_argument,
                ],
                ServerMode::FixturePrimary,
                &expected,
                Duration::from_millis(150),
            )
            .is_err()
        );
        let pid = fs::read_to_string(&pid_path).unwrap();
        let process_path = PathBuf::from(format!("/proc/{}", pid.trim()));
        let deadline = Instant::now() + Duration::from_secs(1);
        while process_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !process_path.exists(),
            "descendant {} survived cleanup",
            pid.trim()
        );
    }
}
