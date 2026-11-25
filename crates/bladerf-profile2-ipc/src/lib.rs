//! Bounded, device-independent IPC for the RF-observatory profile-2 adapter.
//!
//! Frames contain a four-byte little-endian payload length followed by one JSON
//! request or response. Readers allocate only after checking the declared bound;
//! writers serialize into a fixed-capacity buffer and reject an oversized value
//! before writing any frame bytes. The operation enums do
//! not expose generic memory access: they name only the frozen profile-2 reads,
//! four shadow writes, commit and session lifecycle.
//! Any [`FrameError`] is terminal for the pipe session. In particular, an I/O
//! failure can leave a partial outgoing frame, while an oversized incoming
//! prefix is rejected without draining the claimed payload.
//! Any incompatible change to a schema field, enum tag or enum spelling must
//! increment [`PROTOCOL_VERSION`].
//!
//! Use [`FixtureClient`] for the CPU register model or [`NativeClient`] for the
//! device-backed adapter. [`read_frame`] and [`write_frame`] operate on the same
//! typed [`Request`] and [`Response`] messages for both clients.
//!
//! ```
//! use bladerf_profile2_ipc::{
//!     Request, RequestOperation, ShadowChannel, read_frame, write_frame,
//! };
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let request = Request::new(
//!     1,
//!     RequestOperation::WriteShadow {
//!         channel: ShadowChannel::Channel0,
//!         value: 0x0123_4567,
//!     },
//! );
//! let mut bytes = Vec::new();
//! write_frame(&mut bytes, &request)?;
//! let decoded: Request = read_frame(bytes.as_slice())?.expect("one frame");
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
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
#[cfg(unix)]
use nix::unistd::Pid;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Current request and response schema version.
///
/// Increment this value for every incompatible wire change, including changes
/// to a JSON field name, enum tag, enum spelling, required field or field type.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum JSON payload bytes in one framed request or response.
pub const MAX_FRAME_BYTES: usize = 4096;

/// Maximum requests handled in one adapter session, including hello and
/// explicit shutdown.
pub const MAX_SESSION_REQUESTS: usize = 256;

/// Largest accepted deadline for one adapter-client transaction.
pub const MAX_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Exact profile identifier and register-layout version.
pub const PROFILE2_ID: u32 = 0x5246_0201;

/// Exact capability word for the frozen profile-2 register contract.
pub const PROFILE2_CAPABILITIES: u32 = 0x0000_1f04;

/// Host API byte offset of the write-only profile-2 commit register.
pub const COMMIT_BYTE_OFFSET: u32 = 0x20;

/// Frozen profile-2 commit value (`CMIT` in little-endian byte order).
pub const COMMIT_MAGIC: u32 = 0x434d_4954;

const FRAME_HEADER_BYTES: usize = 4;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

/// Errors produced while reading or writing a bounded protocol frame.
#[derive(Debug)]
pub enum FrameError {
    /// The underlying reader or writer failed.
    Io(io::Error),
    /// A value could not be serialized as JSON.
    Serialize(serde_json::Error),
    /// A bounded payload was not a valid value of the requested type.
    Deserialize(serde_json::Error),
    /// A frame declared a zero-byte payload.
    EmptyFrame,
    /// A frame exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge {
        /// Declared length, or the encoded lower bound reached before stopping.
        length: usize,
        /// Maximum accepted payload bytes.
        maximum: usize,
    },
    /// End of input occurred within the four-byte length prefix.
    TruncatedHeader {
        /// Prefix bytes received before end of input.
        received: usize,
    },
    /// End of input occurred within a declared payload.
    TruncatedPayload {
        /// Payload bytes declared by the prefix.
        expected: usize,
        /// Payload bytes received before end of input.
        received: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "protocol I/O failed: {error}"),
            Self::Serialize(error) => write!(formatter, "serialize protocol frame: {error}"),
            Self::Deserialize(error) => write!(formatter, "decode protocol frame: {error}"),
            Self::EmptyFrame => formatter.write_str("protocol frame payload is empty"),
            Self::FrameTooLarge { length, maximum } => write!(
                formatter,
                "protocol frame reached {length} payload bytes; maximum is {maximum}"
            ),
            Self::TruncatedHeader { received } => write!(
                formatter,
                "protocol frame ended after {received} of {FRAME_HEADER_BYTES} header bytes"
            ),
            Self::TruncatedPayload { expected, received } => write!(
                formatter,
                "protocol frame ended after {received} of {expected} payload bytes"
            ),
        }
    }
}

impl Error for FrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialize(error) | Self::Deserialize(error) => Some(error),
            Self::EmptyFrame
            | Self::FrameTooLarge { .. }
            | Self::TruncatedHeader { .. }
            | Self::TruncatedPayload { .. } => None,
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

impl BoundedPayload {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(MAX_FRAME_BYTES),
            overflow_length: None,
        }
    }
}

impl Write for BoundedPayload {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let attempted = self.bytes.len().saturating_add(input.len());
        if attempted > MAX_FRAME_BYTES {
            self.overflow_length = Some(attempted);
            return Err(io::Error::other("protocol frame exceeds its payload bound"));
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
///
/// Returns [`FrameError::Serialize`] if `value` cannot be represented as JSON,
/// [`FrameError::FrameTooLarge`] if the encoded payload exceeds the protocol
/// bound, or [`FrameError::Io`] if the prefix, payload or flush fails.
/// An I/O error may leave a partial frame in `writer`; discard the transport
/// rather than attempting another write.
pub fn write_frame<W, T>(mut writer: W, value: &T) -> Result<(), FrameError>
where
    W: Write,
    T: Serialize + ?Sized,
{
    let mut payload = BoundedPayload::new();
    if let Err(error) = serde_json::to_writer(&mut payload, value) {
        if let Some(length) = payload.overflow_length {
            return Err(FrameError::FrameTooLarge {
                length,
                maximum: MAX_FRAME_BYTES,
            });
        }
        return Err(FrameError::Serialize(error));
    }
    if payload.bytes.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    let length = payload.bytes.len() as u32;
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload.bytes)?;
    writer.flush()?;
    Ok(())
}

/// Read and decode one bounded length-prefixed JSON frame.
///
/// Clean end of input before a new prefix returns `Ok(None)`. End of input after
/// any prefix or payload byte is a truncation error rather than a clean close.
///
/// Pass a mutable reference to reuse the reader for additional frames.
///
/// # Errors
///
/// Returns a [`FrameError`] for I/O failure, truncation, zero/oversized payloads
/// or JSON that does not decode as `T`.
pub fn read_frame<R, T>(mut reader: R) -> Result<Option<T>, FrameError>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut header = [0u8; FRAME_HEADER_BYTES];
    let header_bytes = read_partial(&mut reader, &mut header)?;
    if header_bytes == 0 {
        return Ok(None);
    }
    if header_bytes != FRAME_HEADER_BYTES {
        return Err(FrameError::TruncatedHeader {
            received: header_bytes,
        });
    }

    let declared_length = u32::from_le_bytes(header);
    if declared_length == 0 {
        return Err(FrameError::EmptyFrame);
    }
    if declared_length > MAX_FRAME_BYTES as u32 {
        return Err(FrameError::FrameTooLarge {
            length: declared_length as usize,
            maximum: MAX_FRAME_BYTES,
        });
    }
    let length = declared_length as usize;

    let mut payload = vec![0u8; length];
    let payload_bytes = read_partial(&mut reader, &mut payload)?;
    if payload_bytes != length {
        return Err(FrameError::TruncatedPayload {
            expected: length,
            received: payload_bytes,
        });
    }
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(FrameError::Deserialize)
}

fn read_partial(reader: &mut impl Read, destination: &mut [u8]) -> Result<usize, FrameError> {
    let mut received = 0;
    while received < destination.len() {
        match reader.read(&mut destination[received..]) {
            Ok(0) => break,
            Ok(count) => received += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(received)
}

/// Register access class associated with an invalid raw byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegisterAccess {
    /// Read from the fixed profile-2 readable-register allowlist.
    Read,
    /// Write to one of the four profile-2 shadow registers.
    ShadowWrite,
}

impl fmt::Display for RegisterAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Read => "read",
            Self::ShadowWrite => "shadow write",
        })
    }
}

/// Error returned when a raw byte offset is outside a typed register allowlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRegisterOffset {
    byte_offset: u32,
    access: RegisterAccess,
}

impl InvalidRegisterOffset {
    fn new(byte_offset: u32, access: RegisterAccess) -> Self {
        Self {
            byte_offset,
            access,
        }
    }

    /// Rejected host API byte offset.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        self.byte_offset
    }

    /// Access class whose allowlist rejected the offset.
    #[must_use]
    pub const fn access(self) -> RegisterAccess {
        self.access
    }
}

impl fmt::Display for InvalidRegisterOffset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "profile-2 {} byte offset 0x{:02x} is not allowlisted",
            self.access, self.byte_offset
        )
    }
}

impl Error for InvalidRegisterOffset {}

/// Profile-2 registers that may be read by the adapter protocol.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRegister {
    /// Profile identifier and register-layout version.
    Id,
    /// Frozen capability word.
    Capabilities,
    /// Shadow phase word for channel 0.
    Shadow0,
    /// Shadow phase word for channel 1.
    Shadow1,
    /// Shadow phase word for channel 2.
    Shadow2,
    /// Shadow phase word for channel 3.
    Shadow3,
    /// Busy, outcome, RX-active, reason and fresh-write state.
    Status,
    /// Last successfully applied epoch.
    AppliedEpoch,
    /// Epoch associated with the last terminal outcome.
    OutcomeEpoch,
    /// Applied phase word for channel 0.
    Applied0,
    /// Applied phase word for channel 1.
    Applied1,
    /// Applied phase word for channel 2.
    Applied2,
    /// Applied phase word for channel 3.
    Applied3,
}

impl ReadRegister {
    /// Host API byte offset for this readable register.
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

    fn try_from(byte_offset: u32) -> Result<Self, Self::Error> {
        match byte_offset {
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
            _ => Err(InvalidRegisterOffset::new(
                byte_offset,
                RegisterAccess::Read,
            )),
        }
    }
}

/// One of the four writable profile-2 shadow phase-word channels.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowChannel {
    /// Channel 0 at host byte offset `0x10`.
    Channel0,
    /// Channel 1 at host byte offset `0x14`.
    Channel1,
    /// Channel 2 at host byte offset `0x18`.
    Channel2,
    /// Channel 3 at host byte offset `0x1c`.
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

    /// Host API byte offset for this channel's shadow register.
    #[must_use]
    pub const fn byte_offset(self) -> u32 {
        0x10 + (self.index() as u32 * 4)
    }
}

impl TryFrom<u32> for ShadowChannel {
    type Error = InvalidRegisterOffset;

    fn try_from(byte_offset: u32) -> Result<Self, Self::Error> {
        match byte_offset {
            0x10 => Ok(Self::Channel0),
            0x14 => Ok(Self::Channel1),
            0x18 => Ok(Self::Channel2),
            0x1c => Ok(Self::Channel3),
            _ => Err(InvalidRegisterOffset::new(
                byte_offset,
                RegisterAccess::ShadowWrite,
            )),
        }
    }
}

/// Operation carried by a [`Request`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum RequestOperation {
    /// Negotiate the fixed protocol version and server mode.
    Hello {},
    /// Read one allowlisted profile-2 register.
    ReadRegister {
        /// Register to read.
        register: ReadRegister,
    },
    /// Write one complete phase word to a typed shadow channel.
    WriteShadow {
        /// Shadow channel to update.
        channel: ShadowChannel,
        /// Complete 32-bit phase word.
        value: u32,
    },
    /// Write the frozen `CMIT` value to the profile-2 commit register.
    Commit {},
    /// Close the one-handle adapter session.
    Shutdown {},
}

/// One versioned client request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    version: u16,
    id: u64,
    operation: RequestOperation,
}

impl Request {
    /// Construct a request using [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn new(id: u64, operation: RequestOperation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            operation,
        }
    }

    /// Protocol version decoded from this request.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Caller-assigned request identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Borrow the typed request operation.
    #[must_use]
    pub const fn operation(&self) -> &RequestOperation {
        &self.operation
    }

    /// Split the request into its version, identifier and operation.
    #[must_use]
    pub fn into_parts(self) -> (u16, u64, RequestOperation) {
        (self.version, self.id, self.operation)
    }
}

/// Backend mode reported by a successful hello response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// Deterministic CPU-only fixture with no device access.
    Fixture,
    /// Native single-handle libbladeRF backend.
    Native,
}

/// Stable class for a terminal adapter error response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed frame, version, request identifier or operation order.
    Protocol,
    /// Operation is not valid in the server's current lifecycle state.
    InvalidState,
    /// The bounded request limit was reached.
    RequestLimit,
    /// The selected backend rejected or could not complete an operation.
    Backend,
}

/// Result carried by a [`Response`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum ResponseKind {
    /// Protocol negotiation succeeded.
    Hello {
        /// Whether this is a CPU fixture or native device server.
        mode: ServerMode,
    },
    /// An allowlisted register read succeeded.
    ReadRegister {
        /// Register that was read.
        register: ReadRegister,
        /// Complete 32-bit register value.
        value: u32,
    },
    /// A typed shadow write succeeded.
    WriteShadow {
        /// Shadow channel that was written.
        channel: ShadowChannel,
    },
    /// The fixed commit write completed.
    Commit {},
    /// The server completed explicit shutdown.
    Shutdown {},
    /// A request failed and the session is terminal.
    Error {
        /// Stable error class for machine handling.
        code: ErrorCode,
        /// Bounded human-readable diagnostic once framed.
        message: String,
    },
}

/// One versioned server response correlated to a request identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    version: u16,
    id: u64,
    result: ResponseKind,
}

impl Response {
    /// Construct a response using [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn new(id: u64, result: ResponseKind) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result,
        }
    }

    /// Construct a terminal error response using [`PROTOCOL_VERSION`].
    #[must_use]
    pub fn error(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::new(
            id,
            ResponseKind::Error {
                code,
                message: message.into(),
            },
        )
    }

    /// Protocol version decoded from this response.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Request identifier correlated to this response.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Borrow the typed response result.
    #[must_use]
    pub const fn result(&self) -> &ResponseKind {
        &self.result
    }

    /// Split the response into its version, identifier and typed result.
    #[must_use]
    pub fn into_parts(self) -> (u16, u64, ResponseKind) {
        (self.version, self.id, self.result)
    }
}

/// Errors produced by a synchronous [`FixtureClient`] or [`NativeClient`] session.
#[derive(Debug)]
pub enum ClientError {
    /// The configured per-transaction timeout was zero or exceeded the bound.
    InvalidTimeout {
        /// Rejected timeout.
        timeout: Duration,
        /// Largest accepted timeout.
        maximum: Duration,
    },
    /// Native mode is available only on the adapter's reviewed target.
    UnsupportedNativeTarget,
    /// Native mode requires a nonempty absolute adapter executable path.
    InvalidNativeAdapterPath {
        /// Rejected path.
        path: PathBuf,
    },
    /// Native mode requires a nonempty absolute library path.
    InvalidNativeLibraryPath {
        /// Rejected path.
        path: PathBuf,
    },
    /// Native mode requires exactly 64 lowercase hexadecimal digest digits.
    InvalidNativeSha256,
    /// Native mode requires exactly 32 lowercase hexadecimal serial digits.
    InvalidNativeSerial,
    /// The child process could not be started.
    Spawn(io::Error),
    /// A requested child pipe was unexpectedly unavailable.
    MissingPipe {
        /// Name of the missing standard-I/O pipe.
        name: &'static str,
    },
    /// The bounded standard-I/O worker could not be started.
    WorkerSpawn(io::Error),
    /// The worker could not accept the only outstanding request.
    Publish {
        /// Identifier of the unpublished request.
        request_id: u64,
    },
    /// The complete write-and-read transaction exceeded its deadline.
    Timeout {
        /// Identifier of the timed-out request.
        request_id: u64,
        /// Configured transaction timeout.
        timeout: Duration,
    },
    /// Framing or standard-I/O failed while publishing or reading a request.
    Transport {
        /// Identifier of the affected request.
        request_id: u64,
        /// Underlying frame error.
        source: FrameError,
    },
    /// The adapter closed stdout before returning a complete response.
    EarlyExit {
        /// Identifier awaiting a response.
        request_id: u64,
        /// Exit status, when it was already observable without blocking.
        status: Option<ExitStatus>,
    },
    /// The standard-I/O worker stopped without returning a response.
    WorkerStopped {
        /// Identifier awaiting a response.
        request_id: u64,
    },
    /// A response violated version, identifier or exact-result expectations.
    Protocol {
        /// Identifier of the affected request.
        request_id: u64,
        /// Bounded local diagnostic describing the mismatch.
        message: String,
    },
    /// The adapter returned its typed terminal error result.
    Server {
        /// Identifier correlated to the error response.
        request_id: u64,
        /// Stable server error class.
        code: ErrorCode,
        /// Server diagnostic.
        message: String,
    },
    /// Explicit shutdown produced a non-successful process exit status.
    ChildExit {
        /// Adapter exit status.
        status: ExitStatus,
    },
    /// Waiting for the adapter process failed.
    Wait(io::Error),
    /// The standard-I/O worker panicked.
    WorkerPanicked,
    /// The standard-I/O worker did not report completion within its cleanup bound.
    WorkerCompletionTimeout {
        /// Time allowed for worker completion before detaching it.
        timeout: Duration,
    },
    /// The session was already made terminal by an earlier error.
    Terminal,
    /// Terminal cleanup also encountered a failure.
    Cleanup {
        /// Error that made the session terminal.
        primary: Box<ClientError>,
        /// Additional kill, wait or worker-join diagnostic.
        message: String,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeout { timeout, maximum } => write!(
                formatter,
                "adapter transaction timeout {timeout:?} must be positive and at most {maximum:?}"
            ),
            Self::UnsupportedNativeTarget => {
                formatter.write_str("native adapter sessions are supported only on x86-64 Linux")
            }
            Self::InvalidNativeAdapterPath { path } => write!(
                formatter,
                "native adapter executable path must be nonempty and absolute: {}",
                path.display()
            ),
            Self::InvalidNativeLibraryPath { path } => write!(
                formatter,
                "native adapter library path must be nonempty and absolute: {}",
                path.display()
            ),
            Self::InvalidNativeSha256 => formatter.write_str(
                "native adapter library SHA-256 must be exactly 64 lowercase hexadecimal characters",
            ),
            Self::InvalidNativeSerial => formatter.write_str(
                "native adapter serial must be exactly 32 lowercase hexadecimal characters",
            ),
            Self::Spawn(error) => write!(formatter, "spawn profile-2 adapter: {error}"),
            Self::MissingPipe { name } => {
                write!(formatter, "spawned profile-2 adapter has no piped {name}")
            }
            Self::WorkerSpawn(error) => {
                write!(formatter, "spawn profile-2 adapter I/O worker: {error}")
            }
            Self::Publish { request_id } => write!(
                formatter,
                "profile-2 adapter worker rejected request {request_id}"
            ),
            Self::Timeout {
                request_id,
                timeout,
            } => write!(
                formatter,
                "profile-2 adapter request {request_id} exceeded {timeout:?}"
            ),
            Self::Transport { request_id, source } => write!(
                formatter,
                "profile-2 adapter request {request_id} transport failed: {source}"
            ),
            Self::EarlyExit { request_id, status } => match status {
                Some(status) => write!(
                    formatter,
                    "profile-2 adapter exited with {status} before response {request_id}"
                ),
                None => write!(
                    formatter,
                    "profile-2 adapter closed output before response {request_id}"
                ),
            },
            Self::WorkerStopped { request_id } => write!(
                formatter,
                "profile-2 adapter worker stopped before response {request_id}"
            ),
            Self::Protocol {
                request_id,
                message,
            } => write!(
                formatter,
                "profile-2 adapter response {request_id} is incompatible: {message}"
            ),
            Self::Server {
                request_id,
                code,
                message,
            } => write!(
                formatter,
                "profile-2 adapter request {request_id} failed ({code:?}): {message}"
            ),
            Self::ChildExit { status } => {
                write!(formatter, "profile-2 adapter shutdown exited with {status}")
            }
            Self::Wait(error) => write!(formatter, "wait for profile-2 adapter: {error}"),
            Self::WorkerPanicked => formatter.write_str("profile-2 adapter I/O worker panicked"),
            Self::WorkerCompletionTimeout { timeout } => write!(
                formatter,
                "profile-2 adapter I/O worker did not stop within {timeout:?}"
            ),
            Self::Terminal => formatter.write_str("profile-2 adapter session is terminal"),
            Self::Cleanup { primary, message } => {
                write!(
                    formatter,
                    "{primary}; terminal cleanup also failed: {message}"
                )
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::WorkerSpawn(error) | Self::Wait(error) => Some(error),
            Self::Transport { source, .. } => Some(source),
            Self::Cleanup { primary, .. } => Some(primary),
            Self::InvalidTimeout { .. }
            | Self::UnsupportedNativeTarget
            | Self::InvalidNativeAdapterPath { .. }
            | Self::InvalidNativeLibraryPath { .. }
            | Self::InvalidNativeSha256
            | Self::InvalidNativeSerial
            | Self::MissingPipe { .. }
            | Self::Publish { .. }
            | Self::Timeout { .. }
            | Self::EarlyExit { .. }
            | Self::WorkerStopped { .. }
            | Self::Protocol { .. }
            | Self::Server { .. }
            | Self::ChildExit { .. }
            | Self::WorkerPanicked
            | Self::WorkerCompletionTimeout { .. }
            | Self::Terminal => None,
        }
    }
}

struct WorkerJob {
    request: Request,
    response: SyncSender<Result<Response, WorkerFailure>>,
}

enum WorkerFailure {
    Frame(FrameError),
    EndOfStream,
}

enum WorkerCompletion {
    Completed,
    Panicked,
}

#[cfg(test)]
#[derive(Debug)]
struct CleanupProbe {
    group_signal_attempted: bool,
    leader_reaped: bool,
    worker_wait_completed: bool,
    diagnostic: Option<String>,
}

#[derive(Clone, Copy)]
enum ExpectedResponse {
    Hello(ServerMode),
    ReadRegister(ReadRegister),
    WriteShadow(ShadowChannel),
    Commit,
    Shutdown,
}

#[derive(Debug)]
enum ValidatedResponse {
    Hello,
    ReadRegister(u32),
    WriteShadow,
    Commit,
    Shutdown,
}

fn validate_response(
    response: Response,
    request_id: u64,
    expected: ExpectedResponse,
) -> Result<ValidatedResponse, ClientError> {
    let (version, response_id, result) = response.into_parts();
    if version != PROTOCOL_VERSION {
        return Err(ClientError::Protocol {
            request_id,
            message: format!("version {version} differs from {PROTOCOL_VERSION}"),
        });
    }
    if response_id != request_id {
        return Err(ClientError::Protocol {
            request_id,
            message: format!("response id {response_id} differs from request id {request_id}"),
        });
    }
    if let ResponseKind::Error { code, message } = result {
        return Err(ClientError::Server {
            request_id,
            code,
            message,
        });
    }
    match (expected, result) {
        (ExpectedResponse::Hello(expected), ResponseKind::Hello { mode }) if mode == expected => {
            Ok(ValidatedResponse::Hello)
        }
        (
            ExpectedResponse::ReadRegister(expected),
            ResponseKind::ReadRegister { register, value },
        ) if register == expected => Ok(ValidatedResponse::ReadRegister(value)),
        (ExpectedResponse::WriteShadow(expected), ResponseKind::WriteShadow { channel })
            if channel == expected =>
        {
            Ok(ValidatedResponse::WriteShadow)
        }
        (ExpectedResponse::Commit, ResponseKind::Commit {}) => Ok(ValidatedResponse::Commit),
        (ExpectedResponse::Shutdown, ResponseKind::Shutdown {}) => Ok(ValidatedResponse::Shutdown),
        (expected, actual) => Err(ClientError::Protocol {
            request_id,
            message: format!(
                "result {actual:?} does not match {}",
                match expected {
                    ExpectedResponse::Hello(ServerMode::Fixture) => "fixture hello",
                    ExpectedResponse::Hello(ServerMode::Native) => "native hello",
                    ExpectedResponse::ReadRegister(_) => "requested register read",
                    ExpectedResponse::WriteShadow(_) => "requested shadow write",
                    ExpectedResponse::Commit => "commit",
                    ExpectedResponse::Shutdown => "shutdown",
                }
            ),
        }),
    }
}

fn validate_transaction_timeout(timeout: Duration) -> Result<(), ClientError> {
    if timeout.is_zero() || timeout > MAX_TRANSACTION_TIMEOUT {
        return Err(ClientError::InvalidTimeout {
            timeout,
            maximum: MAX_TRANSACTION_TIMEOUT,
        });
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn is_exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn build_native_command(
    program: &Path,
    library: &Path,
    expected_sha256: &str,
    serial: &str,
) -> Result<Command, ClientError> {
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (program, library, expected_sha256, serial);
        return Err(ClientError::UnsupportedNativeTarget);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if program.as_os_str().is_empty() || !program.is_absolute() {
            return Err(ClientError::InvalidNativeAdapterPath {
                path: program.to_path_buf(),
            });
        }
        if library.as_os_str().is_empty() || !library.is_absolute() {
            return Err(ClientError::InvalidNativeLibraryPath {
                path: library.to_path_buf(),
            });
        }
        if !is_exact_lower_hex(expected_sha256, 64) {
            return Err(ClientError::InvalidNativeSha256);
        }
        if !is_exact_lower_hex(serial, 32) {
            return Err(ClientError::InvalidNativeSerial);
        }

        let mut command = Command::new(program);
        command
            .arg("native")
            .arg("--library")
            .arg(library)
            .arg("--sha256")
            .arg(expected_sha256)
            .arg("--serial")
            .arg(serial)
            // The selected main library is hash-qualified in the child. Remove
            // the common loader-injection controls before exec so they cannot
            // interpose the adapter or redirect its transitive dependencies.
            .env_remove("LD_PRELOAD")
            .env_remove("LD_AUDIT")
            .env_remove("LD_LIBRARY_PATH");
        Ok(command)
    }
}

fn adapter_worker(mut input: ChildStdin, mut output: ChildStdout, requests: Receiver<WorkerJob>) {
    while let Ok(job) = requests.recv() {
        let result = match write_frame(&mut input, &job.request) {
            Ok(()) => match read_frame(&mut output) {
                Ok(Some(response)) => Ok(response),
                Ok(None) => Err(WorkerFailure::EndOfStream),
                Err(error) => Err(WorkerFailure::Frame(error)),
            },
            Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                Err(WorkerFailure::EndOfStream)
            }
            Err(error) => Err(WorkerFailure::Frame(error)),
        };
        let terminal = result.is_err();
        let _ = job.response.try_send(result);
        if terminal {
            return;
        }
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

fn kill_and_reap_leader(child: &mut Child, diagnostics: &mut Vec<String>) -> bool {
    if let Err(error) = child.kill()
        && !missing_process(&error)
    {
        diagnostics.push(format!("kill child directly: {error}"));
        return false;
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
    let _ = kill_and_reap_leader(child, &mut diagnostics);
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

/// One synchronous, long-lived session with the CPU-only profile-2 fixture.
///
/// Each method blocks the calling thread and permits exactly one outstanding
/// request through its exclusive mutable borrow. A transaction deadline covers
/// both publication to child stdin and receipt from child stdout because one
/// worker owns both pipes. There is no separate cancellation token: a timeout
/// makes the session terminal, and dropping an idle client cancels it by killing
/// and waiting for the child. Leader reaping can extend beyond the protocol
/// deadline. Worker completion has a fixed bound, and a timeout leaves the
/// worker available for another cleanup attempt. Prefer [`FixtureClient::shutdown`]
/// for normal completion. [`Drop`] is a kill-and-wait fallback. Child stderr is
/// inherited, never piped, so an unread diagnostic stream cannot deadlock the
/// adapter. On Unix the fixture is placed in a private process group; terminal
/// cleanup signals the group before directly killing and reaping its leader.
pub struct FixtureClient {
    child: Option<Child>,
    requests: Option<SyncSender<WorkerJob>>,
    worker: Option<JoinHandle<()>>,
    worker_completion: Option<Receiver<WorkerCompletion>>,
    process_group: u32,
    process_group_active: bool,
    #[cfg(test)]
    cleanup_probe: Option<SyncSender<CleanupProbe>>,
    timeout: Duration,
    next_id: u64,
    terminal: bool,
}

impl fmt::Debug for FixtureClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FixtureClient")
            .field("timeout", &self.timeout)
            .field("next_id", &self.next_id)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl FixtureClient {
    /// Spawn the CPU-only fixture subcommand from `program` and negotiate it.
    ///
    /// The command's stdin and stdout settings are replaced with private pipes;
    /// stderr is inherited. The first transaction is an exact fixture-mode
    /// hello, so a native or incompatible server is rejected and terminal
    /// cleanup is attempted.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for an invalid timeout, spawn or worker failure,
    /// or any terminal failure of the initial hello transaction.
    pub fn spawn(
        program: impl AsRef<OsStr>,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let mut command = Command::new(program);
        command.arg("fixture");
        Self::spawn_command(command, transaction_timeout, ServerMode::Fixture)
    }

    fn spawn_command(
        command: Command,
        transaction_timeout: Duration,
        expected_mode: ServerMode,
    ) -> Result<Self, ClientError> {
        let mut client = Self::spawn_unnegotiated(command, transaction_timeout)?;
        client.hello(expected_mode)?;
        Ok(client)
    }

    fn spawn_unnegotiated(
        mut command: Command,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError> {
        validate_transaction_timeout(transaction_timeout)?;
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
        let (request_sender, request_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let worker = match thread::Builder::new()
            .name("profile2-adapter-io".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    adapter_worker(input, output, request_receiver);
                }));
                let completion = if result.is_ok() {
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
        Ok(Self {
            child: Some(child),
            requests: Some(request_sender),
            worker: Some(worker),
            worker_completion: Some(completion_receiver),
            process_group,
            process_group_active: true,
            #[cfg(test)]
            cleanup_probe: None,
            timeout: transaction_timeout,
            next_id: 1,
            terminal: false,
        })
    }

    /// Whether an earlier error has made this session terminal.
    ///
    /// The original error includes any cleanup failure. Drop retries
    /// unconfirmed cleanup. Use this status to avoid a redundant shutdown
    /// failure after a terminal error.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Read one allowlisted profile-2 register.
    ///
    /// This blocking call has the session's finite per-transaction deadline.
    /// Any error makes the session terminal and synchronously attempts child
    /// cleanup; cleanup failures are reported and worker waiting remains bounded.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for transport, deadline, server or exact-response
    /// validation failure, or if the session was already terminal.
    pub fn read_register(&mut self, register: ReadRegister) -> Result<u32, ClientError> {
        let (request_id, result) = self.exchange(
            RequestOperation::ReadRegister { register },
            ExpectedResponse::ReadRegister(register),
        )?;
        match result {
            ValidatedResponse::ReadRegister(value) => Ok(value),
            other => self.unexpected_validated(request_id, "register read", other),
        }
    }

    /// Write one complete phase word to a typed shadow channel.
    ///
    /// This blocking call has the session's finite per-transaction deadline.
    /// Any error makes the session terminal and synchronously attempts child
    /// cleanup; cleanup failures are reported and worker waiting remains bounded.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for transport, deadline, server or exact-response
    /// validation failure, or if the session was already terminal.
    pub fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<(), ClientError> {
        let (request_id, result) = self.exchange(
            RequestOperation::WriteShadow { channel, value },
            ExpectedResponse::WriteShadow(channel),
        )?;
        match result {
            ValidatedResponse::WriteShadow => Ok(()),
            other => self.unexpected_validated(request_id, "shadow write", other),
        }
    }

    /// Commit the complete four-channel shadow bank.
    ///
    /// This blocking call has the session's finite per-transaction deadline.
    /// Any error makes the session terminal and synchronously attempts child
    /// cleanup; cleanup failures are reported and worker waiting remains bounded.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for transport, deadline, server or exact-response
    /// validation failure, or if the session was already terminal.
    pub fn commit(&mut self) -> Result<(), ClientError> {
        let (request_id, result) =
            self.exchange(RequestOperation::Commit {}, ExpectedResponse::Commit)?;
        match result {
            ValidatedResponse::Commit => Ok(()),
            other => self.unexpected_validated(request_id, "commit", other),
        }
    }

    /// Request explicit shutdown, require a successful child exit and reap it.
    ///
    /// This consumes the session. One deadline covers request publication,
    /// response receipt and observation of process exit. As with other terminal
    /// paths, kill/wait cleanup can extend beyond that protocol deadline. On
    /// Linux, the private group is signalled while the leader is retained.
    /// On other targets, the tracked fixture must exit without descendants
    /// because exit observation can reap the leader before group cleanup.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for transaction failure, an inexact shutdown
    /// result, a non-successful process exit, or worker/wait failure.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let deadline = Instant::now() + self.timeout;
        let (request_id, result) = self.exchange_until(
            RequestOperation::Shutdown {},
            ExpectedResponse::Shutdown,
            deadline,
        )?;
        if !matches!(result, ValidatedResponse::Shutdown) {
            return self.unexpected_validated(request_id, "shutdown", result);
        }
        let status = match self.wait_for_shutdown_leader(request_id, deadline) {
            Ok(status) => status,
            Err(error) => return self.fail(error),
        };
        self.child.take();
        if !status.success() {
            return self.fail(ClientError::ChildExit { status });
        }
        self.requests.take();
        if let Err(error) = self.finish_worker(request_id) {
            return self.fail(error);
        }
        self.terminal = true;
        Ok(())
    }

    fn hello(&mut self, expected_mode: ServerMode) -> Result<(), ClientError> {
        let (request_id, result) = self.exchange(
            RequestOperation::Hello {},
            ExpectedResponse::Hello(expected_mode),
        )?;
        match result {
            ValidatedResponse::Hello => Ok(()),
            other => self.unexpected_validated(request_id, "adapter hello", other),
        }
    }

    fn exchange(
        &mut self,
        operation: RequestOperation,
        expected: ExpectedResponse,
    ) -> Result<(u64, ValidatedResponse), ClientError> {
        self.exchange_until(operation, expected, Instant::now() + self.timeout)
    }

    fn exchange_until(
        &mut self,
        operation: RequestOperation,
        expected: ExpectedResponse,
        deadline: Instant,
    ) -> Result<(u64, ValidatedResponse), ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        let request_id = self.next_id;
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        let job = WorkerJob {
            request: Request::new(request_id, operation),
            response: response_sender,
        };
        let Some(requests) = self.requests.as_ref() else {
            return self.fail(ClientError::Terminal);
        };
        if let Err(error) = requests.try_send(job) {
            let _ = match error {
                TrySendError::Full(job) | TrySendError::Disconnected(job) => job,
            };
            return self.fail(ClientError::Publish { request_id });
        }
        let now = Instant::now();
        if now >= deadline {
            return self.fail(ClientError::Timeout {
                request_id,
                timeout: self.timeout,
            });
        }
        let response = match response_receiver.recv_timeout(deadline - now) {
            Ok(Ok(response)) => response,
            Ok(Err(WorkerFailure::Frame(source))) => {
                return self.fail(ClientError::Transport { request_id, source });
            }
            Ok(Err(WorkerFailure::EndOfStream)) => {
                return self.fail(ClientError::EarlyExit {
                    request_id,
                    status: None,
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return self.fail(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return self.fail(ClientError::WorkerStopped { request_id });
            }
        };
        if Instant::now() >= deadline {
            return self.fail(ClientError::Timeout {
                request_id,
                timeout: self.timeout,
            });
        }
        let result = match validate_response(response, request_id, expected) {
            Ok(result) => result,
            Err(error) => return self.fail(error),
        };
        self.next_id += 1;
        Ok((request_id, result))
    }

    fn unexpected_validated<T>(
        &mut self,
        request_id: u64,
        expected: &str,
        actual: ValidatedResponse,
    ) -> Result<T, ClientError> {
        self.fail(ClientError::Protocol {
            request_id,
            message: format!(
                "internal response validation expected {expected}, received {actual:?}"
            ),
        })
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

    #[cfg(all(target_os = "linux", not(target_env = "uclibc")))]
    fn wait_for_shutdown_leader(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<ExitStatus, ClientError> {
        let child = self.child.as_mut().ok_or(ClientError::Terminal)?;
        let process = i32::try_from(child.id()).map_err(|_| ClientError::Protocol {
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
            let observed = Instant::now();
            if observed >= deadline {
                return Err(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            match observation {
                WaitStatus::StillAlive => {
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
                        return Err(ClientError::Wait(io::Error::other(diagnostics.join("; "))));
                    }
                    return child.wait().map_err(ClientError::Wait);
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

    #[cfg(not(all(target_os = "linux", not(target_env = "uclibc"))))]
    fn wait_for_shutdown_leader(
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
            let status = self
                .child
                .as_mut()
                .ok_or(ClientError::Terminal)?
                .try_wait()
                .map_err(ClientError::Wait)?;
            if status.is_some() {
                // The leader is reaped. Its group ID may now be reused.
                self.process_group_active = false;
                self.child.take();
            }
            let observed = Instant::now();
            if observed >= deadline {
                return Err(ClientError::Timeout {
                    request_id,
                    timeout: self.timeout,
                });
            }
            if let Some(status) = status {
                return Ok(status);
            }
            thread::sleep((deadline - observed).min(Duration::from_millis(2)));
        }
    }

    fn fail<T>(&mut self, primary: ClientError) -> Result<T, ClientError> {
        Err(with_cleanup(primary, self.abort_and_reap()))
    }

    fn abort_and_reap(&mut self) -> Option<String> {
        self.terminal = true;
        self.requests.take();
        let mut diagnostics = Vec::new();
        #[cfg(test)]
        let group_signal_attempted = self.process_group_active;
        if self.process_group_active {
            if self.child.is_none() {
                diagnostics.push("process-group cleanup has no leader handle".to_owned());
            } else if signal_private_process_group(self.process_group, &mut diagnostics) {
                self.process_group_active = false;
            }
        }
        // Keep the leader unreaped while group signaling is unconfirmed. Its PID
        // anchors the group identifier so a retry cannot target a reused ID.
        if !self.process_group_active
            && let Some(child) = self.child.as_mut()
            && kill_and_reap_leader(child, &mut diagnostics)
        {
            self.child.take();
        }
        let worker_result = self.finish_worker(self.next_id);
        if let Err(error) = &worker_result {
            diagnostics.push(error.to_string());
        }
        let diagnostic = (!diagnostics.is_empty()).then(|| diagnostics.join("; "));
        #[cfg(test)]
        if let Some(probe) = self.cleanup_probe.take() {
            let _ = probe.try_send(CleanupProbe {
                group_signal_attempted,
                leader_reaped: self.child.is_none(),
                worker_wait_completed: worker_result.is_ok(),
                diagnostic: diagnostic.clone(),
            });
        }
        diagnostic
    }

    #[cfg(test)]
    fn install_cleanup_probe(&mut self) -> Receiver<CleanupProbe> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.cleanup_probe = Some(sender);
        receiver
    }
}

impl Drop for FixtureClient {
    fn drop(&mut self) {
        if self.process_group_active
            || self.child.is_some()
            || self.worker.is_some()
            || self.worker_completion.is_some()
        {
            let _ = self.abort_and_reap();
        }
    }
}

/// One synchronous session with the explicit native profile-2 adapter.
///
/// This client shares the fixture client's lockstep framing, finite transaction
/// deadlines, private process group and terminal kill/reap behavior. Unlike
/// [`FixtureClient`], [`NativeClient::spawn`] is device-facing: its initial
/// hello makes the child qualify the selected library and open the exact
/// serial-selected bladeRF. That can claim USB, change transport alternate
/// settings and block in a native call until this client reaches its deadline
/// and kills the child. The serial is a device identifier, not a secret; it is
/// present in the child's argument vector. Child stderr remains inherited.
pub struct NativeClient {
    session: FixtureClient,
}

impl fmt::Debug for NativeClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeClient")
            .field("timeout", &self.session.timeout)
            .field("next_id", &self.session.next_id)
            .field("terminal", &self.session.terminal)
            .finish_non_exhaustive()
    }
}

impl NativeClient {
    /// Spawn the native subcommand with exact identity inputs and negotiate it.
    ///
    /// `program` and `library` must be nonempty absolute paths. The SHA-256 and
    /// serial must be complete lowercase hexadecimal values of 64 and 32
    /// characters respectively. The child receives the fixed argument sequence
    /// `native --library … --sha256 … --serial …`; common Linux loader
    /// injection variables are removed before exec. The library file itself is
    /// neither opened nor hashed in this parent process.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] before spawning for an unsupported target,
    /// invalid timeout or malformed identity input. After spawning, any child,
    /// transport, deadline, server or exact native-mode hello failure is
    /// terminal and triggers cleanup. The worker-completion wait is bounded,
    /// but direct child kill/wait reaping can extend beyond the transaction
    /// deadline during operating-system teardown.
    pub fn spawn(
        program: &Path,
        library: &Path,
        expected_sha256: &str,
        serial: &str,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError> {
        validate_transaction_timeout(transaction_timeout)?;
        let command = build_native_command(program, library, expected_sha256, serial)?;
        let session =
            FixtureClient::spawn_command(command, transaction_timeout, ServerMode::Native)?;
        Ok(Self { session })
    }

    /// Whether an earlier terminal failure has already triggered cleanup.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.session.is_terminal()
    }

    /// Read one allowlisted profile-2 register under the transaction deadline.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for terminal state, transport, deadline, server
    /// or exact-response failure.
    pub fn read_register(&mut self, register: ReadRegister) -> Result<u32, ClientError> {
        self.session.read_register(register)
    }

    /// Write one complete phase word to a typed profile-2 shadow channel.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for terminal state, transport, deadline, server
    /// or exact-response failure.
    pub fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<(), ClientError> {
        self.session.write_shadow(channel, value)
    }

    /// Issue the fixed profile-2 commit operation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for terminal state, transport, deadline, server
    /// or exact-response failure.
    pub fn commit(&mut self) -> Result<(), ClientError> {
        self.session.commit()
    }

    /// Request explicit adapter shutdown, require successful exit and reap it.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for transaction failure, an inexact response,
    /// unsuccessful child exit, wait failure or worker cleanup failure.
    pub fn shutdown(self) -> Result<(), ClientError> {
        self.session.shutdown()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    fn encoded<T: Serialize>(value: &T) -> Vec<u8> {
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

    fn cleanup_report(receiver: Receiver<CleanupProbe>) -> CleanupProbe {
        receiver.recv_timeout(Duration::from_secs(1)).unwrap()
    }

    fn assert_cleanup_completed(receiver: Receiver<CleanupProbe>) {
        let report = cleanup_report(receiver);
        assert!(
            report.group_signal_attempted
                && report.leader_reaped
                && report.worker_wait_completed
                && report.diagnostic.is_none(),
            "incomplete terminal cleanup: {report:?}"
        );
    }

    #[test]
    fn frames_accept_owned_and_trait_object_io() {
        let mut bytes = Vec::new();
        write_frame(Cursor::new(&mut bytes), &true).unwrap();
        write_frame(&mut bytes as &mut dyn Write, &false).unwrap();

        let mut input = Cursor::new(bytes);
        assert_eq!(read_frame(&mut input as &mut dyn Read).unwrap(), Some(true));
        assert_eq!(read_frame(input).unwrap(), Some(false));
    }

    #[test]
    fn request_and_response_frames_round_trip() {
        let request = Request::new(
            9,
            RequestOperation::WriteShadow {
                channel: ShadowChannel::Channel2,
                value: 0x89ab_cdef,
            },
        );
        let bytes = encoded(&request);
        assert_eq!(
            u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize,
            bytes.len() - 4
        );
        let mut input = bytes.as_slice();
        assert_eq!(read_frame::<_, Request>(&mut input).unwrap(), Some(request));
        assert!(read_frame::<_, Request>(&mut input).unwrap().is_none());

        let response = Response::new(
            9,
            ResponseKind::ReadRegister {
                register: ReadRegister::Status,
                value: 0x6000_0000,
            },
        );
        let mut input = Cursor::new(encoded(&response));
        assert_eq!(
            read_frame::<_, Response>(&mut input).unwrap(),
            Some(response)
        );
        assert!(read_frame::<_, Response>(&mut input).unwrap().is_none());
    }

    #[test]
    fn schema_tags_and_private_field_access_are_stable() {
        let request = Request::new(
            1,
            RequestOperation::ReadRegister {
                register: ReadRegister::AppliedEpoch,
            },
        );
        assert_eq!(request.version(), PROTOCOL_VERSION);
        assert_eq!(request.id(), 1);
        assert_eq!(
            request.operation(),
            &RequestOperation::ReadRegister {
                register: ReadRegister::AppliedEpoch
            }
        );
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"version":1,"id":1,"operation":{"kind":"read_register","register":"applied_epoch"}}"#
        );
        let (version, id, operation) = request.into_parts();
        assert_eq!((version, id), (PROTOCOL_VERSION, 1));
        assert_eq!(
            operation,
            RequestOperation::ReadRegister {
                register: ReadRegister::AppliedEpoch
            }
        );

        let response = Response::error(1, ErrorCode::InvalidState, "hello required");
        assert_eq!(response.version(), PROTOCOL_VERSION);
        assert_eq!(response.id(), 1);
        assert!(matches!(
            response.result(),
            ResponseKind::Error {
                code: ErrorCode::InvalidState,
                message
            } if message == "hello required"
        ));
        let (version, id, result) = response.into_parts();
        assert_eq!((version, id), (PROTOCOL_VERSION, 1));
        assert!(matches!(result, ResponseKind::Error { .. }));

        assert_eq!(
            serde_json::to_string(&Response::new(1, ResponseKind::Shutdown {})).unwrap(),
            r#"{"version":1,"id":1,"result":{"kind":"shutdown"}}"#
        );
    }

    #[test]
    fn every_wire_enum_spelling_has_a_golden_vector() {
        let registers = [
            (ReadRegister::Id, r#""id""#),
            (ReadRegister::Capabilities, r#""capabilities""#),
            (ReadRegister::Shadow0, r#""shadow0""#),
            (ReadRegister::Shadow1, r#""shadow1""#),
            (ReadRegister::Shadow2, r#""shadow2""#),
            (ReadRegister::Shadow3, r#""shadow3""#),
            (ReadRegister::Status, r#""status""#),
            (ReadRegister::AppliedEpoch, r#""applied_epoch""#),
            (ReadRegister::OutcomeEpoch, r#""outcome_epoch""#),
            (ReadRegister::Applied0, r#""applied0""#),
            (ReadRegister::Applied1, r#""applied1""#),
            (ReadRegister::Applied2, r#""applied2""#),
            (ReadRegister::Applied3, r#""applied3""#),
        ];
        for (value, golden) in registers {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }

        let channels = [
            (ShadowChannel::Channel0, r#""channel0""#),
            (ShadowChannel::Channel1, r#""channel1""#),
            (ShadowChannel::Channel2, r#""channel2""#),
            (ShadowChannel::Channel3, r#""channel3""#),
        ];
        for (value, golden) in channels {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }

        let operations = [
            (RequestOperation::Hello {}, r#"{"kind":"hello"}"#),
            (
                RequestOperation::ReadRegister {
                    register: ReadRegister::Status,
                },
                r#"{"kind":"read_register","register":"status"}"#,
            ),
            (
                RequestOperation::WriteShadow {
                    channel: ShadowChannel::Channel3,
                    value: 2_305_843_009,
                },
                r#"{"kind":"write_shadow","channel":"channel3","value":2305843009}"#,
            ),
            (RequestOperation::Commit {}, r#"{"kind":"commit"}"#),
            (RequestOperation::Shutdown {}, r#"{"kind":"shutdown"}"#),
        ];
        for (value, golden) in operations {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }

        let modes = [
            (ServerMode::Fixture, r#""fixture""#),
            (ServerMode::Native, r#""native""#),
        ];
        for (value, golden) in modes {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }

        let error_codes = [
            (ErrorCode::Protocol, r#""protocol""#),
            (ErrorCode::InvalidState, r#""invalid_state""#),
            (ErrorCode::RequestLimit, r#""request_limit""#),
            (ErrorCode::Backend, r#""backend""#),
        ];
        for (value, golden) in error_codes {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }

        let results = [
            (
                ResponseKind::Hello {
                    mode: ServerMode::Fixture,
                },
                r#"{"kind":"hello","mode":"fixture"}"#,
            ),
            (
                ResponseKind::ReadRegister {
                    register: ReadRegister::Capabilities,
                    value: 7,
                },
                r#"{"kind":"read_register","register":"capabilities","value":7}"#,
            ),
            (
                ResponseKind::WriteShadow {
                    channel: ShadowChannel::Channel1,
                },
                r#"{"kind":"write_shadow","channel":"channel1"}"#,
            ),
            (ResponseKind::Commit {}, r#"{"kind":"commit"}"#),
            (ResponseKind::Shutdown {}, r#"{"kind":"shutdown"}"#),
            (
                ResponseKind::Error {
                    code: ErrorCode::Backend,
                    message: "failed".to_owned(),
                },
                r#"{"kind":"error","code":"backend","message":"failed"}"#,
            ),
        ];
        for (value, golden) in results {
            assert_eq!(serde_json::to_string(&value).unwrap(), golden);
        }
    }

    #[test]
    fn pure_response_validation_is_exact_for_version_id_and_result() {
        assert!(matches!(
            validate_response(
                Response::new(
                    7,
                    ResponseKind::ReadRegister {
                        register: ReadRegister::Id,
                        value: PROFILE2_ID,
                    },
                ),
                7,
                ExpectedResponse::ReadRegister(ReadRegister::Id),
            )
            .unwrap(),
            ValidatedResponse::ReadRegister(PROFILE2_ID)
        ));
        for mode in [ServerMode::Fixture, ServerMode::Native] {
            assert!(matches!(
                validate_response(
                    Response::new(7, ResponseKind::Hello { mode }),
                    7,
                    ExpectedResponse::Hello(mode),
                )
                .unwrap(),
                ValidatedResponse::Hello
            ));
        }

        let wrong_version = Response {
            version: PROTOCOL_VERSION + 1,
            id: 7,
            result: ResponseKind::Commit {},
        };
        for error in [
            validate_response(wrong_version, 7, ExpectedResponse::Commit).unwrap_err(),
            validate_response(
                Response::new(8, ResponseKind::Commit {}),
                7,
                ExpectedResponse::Commit,
            )
            .unwrap_err(),
            validate_response(
                Response::new(7, ResponseKind::Shutdown {}),
                7,
                ExpectedResponse::Commit,
            )
            .unwrap_err(),
            validate_response(
                Response::new(
                    7,
                    ResponseKind::ReadRegister {
                        register: ReadRegister::Capabilities,
                        value: PROFILE2_ID,
                    },
                ),
                7,
                ExpectedResponse::ReadRegister(ReadRegister::Id),
            )
            .unwrap_err(),
            validate_response(
                Response::new(
                    7,
                    ResponseKind::Hello {
                        mode: ServerMode::Native,
                    },
                ),
                7,
                ExpectedResponse::Hello(ServerMode::Fixture),
            )
            .unwrap_err(),
            validate_response(
                Response::new(
                    7,
                    ResponseKind::Hello {
                        mode: ServerMode::Fixture,
                    },
                ),
                7,
                ExpectedResponse::Hello(ServerMode::Native),
            )
            .unwrap_err(),
        ] {
            assert!(matches!(error, ClientError::Protocol { .. }));
        }

        assert!(matches!(
            validate_response(
                Response::error(7, ErrorCode::Backend, "failed"),
                7,
                ExpectedResponse::Commit,
            ),
            Err(ClientError::Server {
                request_id: 7,
                code: ErrorCode::Backend,
                ..
            })
        ));
    }

    #[test]
    fn fixture_client_rejects_unbounded_transaction_timeouts_before_spawn() {
        for timeout in [
            Duration::ZERO,
            MAX_TRANSACTION_TIMEOUT + Duration::from_nanos(1),
        ] {
            assert!(matches!(
                FixtureClient::spawn("this-program-must-not-be-spawned", timeout),
                Err(ClientError::InvalidTimeout {
                    timeout: actual,
                    maximum: MAX_TRANSACTION_TIMEOUT,
                }) if actual == timeout
            ));
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_command_freezes_arguments_and_loader_environment() {
        use std::os::unix::ffi::OsStringExt;

        let program = Path::new("/opt/profile 2/bladerf-profile2-adapter");
        let library = Path::new("/opt/profile 2/libbladeRF.so.2");
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let serial = "0123456789abcdef0123456789abcdef";
        let command = build_native_command(program, library, sha256, serial).unwrap();

        assert_eq!(command.get_program(), program.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("native"),
                OsStr::new("--library"),
                library.as_os_str(),
                OsStr::new("--sha256"),
                OsStr::new(sha256),
                OsStr::new("--serial"),
                OsStr::new(serial),
            ]
        );
        for removed in ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == OsStr::new(removed) && value.is_none()),
                "{removed} was not removed"
            );
        }

        let opaque_library = PathBuf::from(std::ffi::OsString::from_vec(
            b"/opt/profile2/libbladeRF-\xff.so.2".to_vec(),
        ));
        let opaque = build_native_command(program, &opaque_library, sha256, serial).unwrap();
        assert_eq!(opaque.get_args().nth(2), Some(opaque_library.as_os_str()));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_inputs_fail_before_spawning_a_sentinel_program() {
        let program = Path::new("/this/program/must/not/be-spawned");
        let library = Path::new("/this/library/must/not/be-opened.so");
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let serial = "0123456789abcdef0123456789abcdef";

        assert!(matches!(
            NativeClient::spawn(program, library, sha256, serial, Duration::ZERO,),
            Err(ClientError::InvalidTimeout { .. })
        ));
        assert!(matches!(
            NativeClient::spawn(
                Path::new("relative-adapter"),
                library,
                sha256,
                serial,
                Duration::from_secs(1),
            ),
            Err(ClientError::InvalidNativeAdapterPath { .. })
        ));
        assert!(matches!(
            NativeClient::spawn(
                program,
                Path::new("relative-library"),
                sha256,
                serial,
                Duration::from_secs(1),
            ),
            Err(ClientError::InvalidNativeLibraryPath { .. })
        ));
        assert!(matches!(
            NativeClient::spawn(
                program,
                library,
                "not-a-digest",
                serial,
                Duration::from_secs(1),
            ),
            Err(ClientError::InvalidNativeSha256)
        ));
        assert!(matches!(
            NativeClient::spawn(
                program,
                library,
                sha256,
                "0123456789ABCDEF0123456789ABCDEF",
                Duration::from_secs(1),
            ),
            Err(ClientError::InvalidNativeSerial)
        ));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_client_negotiates_and_shuts_down_against_a_cpu_fake() {
        let directory = tempfile::tempdir().unwrap();
        let hello_path = directory.path().join("hello.frame");
        let shutdown_path = directory.path().join("shutdown.frame");
        let arguments_path = directory.path().join("arguments.txt");
        let script_path = directory.path().join("fake-adapter.sh");
        let library = directory.path().join("library with spaces.so");
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let serial = "0123456789abcdef0123456789abcdef";

        let hello_request = encoded(&Request::new(1, RequestOperation::Hello {}));
        let shutdown_request = encoded(&Request::new(2, RequestOperation::Shutdown {}));
        fs::write(
            &hello_path,
            encoded(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::Native,
                },
            )),
        )
        .unwrap();
        fs::write(
            &shutdown_path,
            encoded(&Response::new(2, ResponseKind::Shutdown {})),
        )
        .unwrap();
        for path in [&hello_path, &shutdown_path, &arguments_path] {
            assert!(!path.to_string_lossy().contains('\''));
        }
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{}'\n\
             dd bs=1 count={} of=/dev/null 2>/dev/null\n\
             cat '{}'\n\
             dd bs=1 count={} of=/dev/null 2>/dev/null\n\
             cat '{}'\n",
            arguments_path.display(),
            hello_request.len(),
            hello_path.display(),
            shutdown_request.len(),
            shutdown_path.display(),
        );
        fs::write(&script_path, script).unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script_path, permissions).unwrap();

        let client = NativeClient::spawn(
            &script_path,
            &library,
            sha256,
            serial,
            Duration::from_secs(2),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains(serial));
        assert!(!debug.contains(&library.to_string_lossy().into_owned()));
        client.shutdown().unwrap();

        assert_eq!(
            fs::read_to_string(arguments_path).unwrap(),
            format!(
                "native\n--library\n{}\n--sha256\n{sha256}\n--serial\n{serial}\n",
                library.display()
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_child_seam_covers_malformed_exit_timeout_and_drop_reaping() {
        let mut malformed =
            FixtureClient::spawn_unnegotiated(Command::new("cat"), Duration::from_millis(250))
                .unwrap();
        let malformed_cleanup = malformed.install_cleanup_probe();
        let error = malformed.hello(ServerMode::Fixture).unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Transport { .. }
        ));
        assert!(matches!(
            malformed.hello(ServerMode::Fixture),
            Err(ClientError::Terminal)
        ));
        assert!(malformed.is_terminal());
        assert_cleanup_completed(malformed_cleanup);

        let mut early =
            FixtureClient::spawn_unnegotiated(Command::new("true"), Duration::from_millis(250))
                .unwrap();
        let early_cleanup = early.install_cleanup_probe();
        let error = early.hello(ServerMode::Fixture).unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::EarlyExit { .. }
        ));
        assert_cleanup_completed(early_cleanup);

        let mut stalled_command = Command::new("sleep");
        stalled_command.arg("30");
        let mut stalled =
            FixtureClient::spawn_unnegotiated(stalled_command, Duration::from_millis(50)).unwrap();
        let stalled_cleanup = stalled.install_cleanup_probe();
        let started = Instant::now();
        let error = stalled.hello(ServerMode::Fixture).unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Timeout { request_id: 1, .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_cleanup_completed(stalled_cleanup);

        let mut sleep_command = Command::new("sleep");
        sleep_command.arg("30");
        let mut idle =
            FixtureClient::spawn_unnegotiated(sleep_command, Duration::from_millis(250)).unwrap();
        let idle_cleanup = idle.install_cleanup_probe();
        drop(idle);
        assert_cleanup_completed(idle_cleanup);
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_response_does_not_excuse_exit_after_the_deadline() {
        let mut responses = tempfile::NamedTempFile::new().unwrap();
        write_frame(
            responses.as_file_mut(),
            &Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::Fixture,
                },
            ),
        )
        .unwrap();
        write_frame(
            responses.as_file_mut(),
            &Response::new(2, ResponseKind::Shutdown {}),
        )
        .unwrap();
        responses.as_file_mut().sync_all().unwrap();

        let mut command = Command::new("tail");
        command.arg("-f").arg(responses.path());
        let mut client =
            FixtureClient::spawn_command(command, Duration::from_millis(50), ServerMode::Fixture)
                .unwrap();
        let cleanup = client.install_cleanup_probe();
        let started = Instant::now();
        let error = client.shutdown().unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Timeout { request_id: 2, .. }
            ),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_cleanup_completed(cleanup);
    }

    #[cfg(unix)]
    #[test]
    fn blocked_pipe_worker_is_retained_at_its_cleanup_bound() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut client =
            FixtureClient::spawn_unnegotiated(command, Duration::from_millis(250)).unwrap();
        let cleanup = client.install_cleanup_probe();
        let (response_sender, _response_receiver) = mpsc::sync_channel(1);
        client
            .requests
            .as_ref()
            .unwrap()
            .try_send(WorkerJob {
                request: Request::new(1, RequestOperation::Hello {}),
                response: response_sender,
            })
            .unwrap();

        let started = Instant::now();
        for _ in 0..2 {
            assert!(matches!(
                client.finish_worker(1),
                Err(ClientError::WorkerCompletionTimeout {
                    timeout: WORKER_COMPLETION_TIMEOUT
                })
            ));
            assert!(client.worker.is_some());
            assert!(client.worker_completion.is_some());
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(client.abort_and_reap().is_none());
        assert_cleanup_completed(cleanup);
        assert!(client.worker.is_none());
        assert!(client.worker_completion.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unconfirmed_process_group_keeps_the_unreaped_leader() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let mut client =
            FixtureClient::spawn_unnegotiated(command, Duration::from_millis(250)).unwrap();
        let leader = client.process_group;
        client.process_group = u32::MAX;

        let first = client.abort_and_reap();
        let second = client.abort_and_reap();
        let retained_leader = client.child.as_ref().map(Child::id);
        let group_active = client.process_group_active;

        // Restore the real group and clean up before asserting.
        client.process_group = leader;
        let final_cleanup = client.abort_and_reap();

        assert!(first.is_some());
        assert!(second.is_some());
        assert_eq!(retained_leader, Some(leader));
        assert!(group_active);
        assert!(final_cleanup.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn leader_exit_with_inherited_pipe_descendant_is_bounded_and_group_cleaned() {
        // This one-line shell is solely a POSIX fork/leader-exit topology
        // harness: the production client and fixture remain Rust programs.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 &"]);
        let mut client =
            FixtureClient::spawn_unnegotiated(command, Duration::from_millis(50)).unwrap();
        let cleanup = client.install_cleanup_probe();
        let started = Instant::now();
        let error = client.hello(ServerMode::Fixture).unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Timeout { request_id: 1, .. }
            ),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_cleanup_completed(cleanup);
    }

    #[test]
    fn register_types_freeze_the_allowlisted_byte_offsets() {
        assert_eq!(PROFILE2_ID, 0x5246_0201);
        assert_eq!(PROFILE2_CAPABILITIES, 0x0000_1f04);
        assert_eq!(COMMIT_BYTE_OFFSET, 0x20);
        assert_eq!(COMMIT_MAGIC, 0x434d_4954);
        assert_eq!(MAX_SESSION_REQUESTS, 256);
        let registers = [
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
        assert_eq!(
            registers.map(|(register, offset)| (register.byte_offset(), offset)),
            registers.map(|(_, offset)| (offset, offset))
        );
        for (register, offset) in registers {
            assert_eq!(ReadRegister::try_from(offset).unwrap(), register);
        }
        let invalid = ReadRegister::try_from(COMMIT_BYTE_OFFSET).unwrap_err();
        assert_eq!(invalid.byte_offset(), COMMIT_BYTE_OFFSET);
        assert_eq!(invalid.access(), RegisterAccess::Read);
        assert!(invalid.to_string().contains("0x20"));

        let channels = [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ];
        assert_eq!(channels.map(ShadowChannel::index), [0, 1, 2, 3]);
        assert_eq!(
            channels.map(ShadowChannel::byte_offset),
            [0x10, 0x14, 0x18, 0x1c]
        );
        for channel in channels {
            assert_eq!(
                ShadowChannel::try_from(channel.byte_offset()).unwrap(),
                channel
            );
        }
        let invalid = ShadowChannel::try_from(0x12).unwrap_err();
        assert_eq!(invalid.byte_offset(), 0x12);
        assert_eq!(invalid.access(), RegisterAccess::ShadowWrite);
    }

    #[test]
    fn clean_eof_and_truncated_frames_are_distinct() {
        assert!(
            read_frame::<_, Request>(&mut [].as_slice())
                .unwrap()
                .is_none()
        );

        for received in 1..FRAME_HEADER_BYTES {
            let input = vec![0u8; received];
            assert!(matches!(
                read_frame::<_, Request>(&mut input.as_slice()),
                Err(FrameError::TruncatedHeader { received: actual }) if actual == received
            ));
        }

        let mut input = Vec::from(5u32.to_le_bytes());
        input.extend_from_slice(b"{} ");
        assert!(matches!(
            read_frame::<_, Request>(&mut input.as_slice()),
            Err(FrameError::TruncatedPayload {
                expected: 5,
                received: 3
            })
        ));
    }

    #[test]
    fn zero_oversized_and_malformed_payloads_are_rejected() {
        assert!(matches!(
            read_frame::<_, Request>(&mut 0u32.to_le_bytes().as_slice()),
            Err(FrameError::EmptyFrame)
        ));

        let oversized = u32::try_from(MAX_FRAME_BYTES + 1).unwrap().to_le_bytes();
        assert!(matches!(
            read_frame::<_, Request>(&mut oversized.as_slice()),
            Err(FrameError::FrameTooLarge {
                length,
                maximum: MAX_FRAME_BYTES
            }) if length == MAX_FRAME_BYTES + 1
        ));

        let mut malformed = Vec::from(1u32.to_le_bytes());
        malformed.push(b'{');
        assert!(matches!(
            read_frame::<_, Request>(&mut malformed.as_slice()),
            Err(FrameError::Deserialize(_))
        ));

        let too_large = "x".repeat(MAX_FRAME_BYTES);
        let mut output = Vec::new();
        assert!(matches!(
            write_frame(&mut output, &too_large),
            Err(FrameError::FrameTooLarge {
                length,
                maximum: MAX_FRAME_BYTES
            }) if length > MAX_FRAME_BYTES
        ));
        assert!(
            output.is_empty(),
            "oversized serialization must not write even a frame prefix"
        );
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let unknown_request = br#"{"version":1,"id":1,"operation":{"kind":"hello"},"extra":0}"#;
        let mut framed = Vec::from(u32::try_from(unknown_request.len()).unwrap().to_le_bytes());
        framed.extend_from_slice(unknown_request);
        assert!(matches!(
            read_frame::<_, Request>(&mut framed.as_slice()),
            Err(FrameError::Deserialize(_))
        ));

        let unknown_operation = br#"{"version":1,"id":1,"operation":{"kind":"commit","extra":0}}"#;
        let mut framed = Vec::from(
            u32::try_from(unknown_operation.len())
                .unwrap()
                .to_le_bytes(),
        );
        framed.extend_from_slice(unknown_operation);
        assert!(matches!(
            read_frame::<_, Request>(&mut framed.as_slice()),
            Err(FrameError::Deserialize(_))
        ));
    }
}
