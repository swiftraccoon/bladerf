//! Bounded, device-independent IPC for fresh stock-image recovery.
//!
//! This protocol is deliberately narrower than the profile-2 loader and
//! complete-session families. A fresh child may restore the frozen stock xA9
//! image, verify the existing stock-identity contract, and shut down. It has no
//! custom-image, register, receive, transmit, flash, bias-tee, or generic device
//! operation.
//!
//! The typed clients allow one outstanding request. They enter the terminal
//! state before publishing a request and leave it after validating an exact
//! response. A failed exchange starts cleanup of the private child process
//! group. [`NativeRecoveryClient`]
//! fixes the native child arguments, and [`FixtureRecoveryClient`] exercises the
//! same recovery lifecycle with a CPU fixture. [`serve_with_factory`] creates a
//! [`RecoveryBackend`] after validating the initial Hello.
//!
//! ```
//! use bladerf_profile2_recovery_ipc::{
//!     Request, RequestOperation, ServerMode, read_frame, write_frame,
//! };
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let request = Request::new(
//!     1,
//!     RequestOperation::Hello {
//!         mode: ServerMode::FixtureRecovery,
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
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
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

/// Current fresh-recovery request and response schema version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Fixed discriminator for the fresh-recovery protocol family.
pub const PROTOCOL_FAMILY: ProtocolFamily = ProtocolFamily::Recovery;
/// Maximum JSON payload bytes in one request or response.
pub const MAX_FRAME_BYTES: usize = 4096;
/// Maximum UTF-8 bytes retained in one terminal server diagnostic.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 512;
/// Maximum requests in a complete recovery session, including lifecycle calls.
pub const MAX_SESSION_REQUESTS: usize = 4;
/// Largest accepted deadline for one client transaction.
pub const MAX_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);
/// Exact retained libbladeRF semantic version required by this protocol.
pub const REQUIRED_LIBRARY_VERSION: &str = "2.6.0";
/// Exact B2L1 loader-neutral open-contract marker.
pub const REQUIRED_LIBRARY_MARKER: u32 = 0x4232_4c01;
/// Frozen stock xA9 v0.16.0 image digest required for recovery.
pub const REQUIRED_STOCK_SHA256: &str =
    "3004467110e2b2790c8f6128f998c2149905a88eed91a87f11af3db524d60c65";
/// Frozen stock xA9 v0.16.0 image byte count required for recovery.
pub const REQUIRED_STOCK_BYTES: u64 = 12_858_972;

const FRAME_HEADER_BYTES: usize = 4;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

/// Errors while reading or writing one bounded recovery frame.
#[derive(Debug)]
pub enum FrameError {
    /// The underlying reader or writer failed.
    Io(io::Error),
    /// JSON serialization failed.
    Serialize(serde_json::Error),
    /// A bounded payload was not a valid requested value.
    Deserialize(serde_json::Error),
    /// A frame declared no payload.
    EmptyFrame,
    /// A frame exceeded [`MAX_FRAME_BYTES`].
    FrameTooLarge {
        /// Declared or attempted payload bytes.
        length: usize,
        /// Maximum accepted payload bytes.
        maximum: usize,
    },
    /// EOF occurred inside the four-byte length prefix.
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
            Self::Io(error) => write!(f, "recovery protocol I/O failed: {error}"),
            Self::Serialize(error) => write!(f, "serialize recovery frame: {error}"),
            Self::Deserialize(error) => write!(f, "decode recovery frame: {error}"),
            Self::EmptyFrame => f.write_str("recovery frame payload is empty"),
            Self::FrameTooLarge { length, maximum } => write!(
                f,
                "recovery frame has {length} payload bytes; maximum is {maximum}"
            ),
            Self::TruncatedHeader { received } => write!(
                f,
                "recovery frame ended after {received} of {FRAME_HEADER_BYTES} header bytes"
            ),
            Self::TruncatedPayload { expected, received } => write!(
                f,
                "recovery frame ended after {received} of {expected} payload bytes"
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

impl Write for BoundedPayload {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let attempted = self.bytes.len().saturating_add(input.len());
        if attempted > MAX_FRAME_BYTES {
            self.overflow_length = Some(attempted);
            return Err(io::Error::other("recovery frame exceeds payload bound"));
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
/// Returns a [`FrameError`] for serialization, bounds, or transport failure.
/// A transport error may leave a partial frame, so discard that transport.
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
    if payload.bytes.is_empty() {
        return Err(FrameError::EmptyFrame);
    }
    let length = u32::try_from(payload.bytes.len()).expect("frame bound fits u32");
    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload.bytes)?;
    writer.flush()?;
    Ok(())
}

/// Read and decode one bounded length-prefixed JSON frame.
///
/// Clean EOF before a prefix returns `Ok(None)`. EOF after any prefix or
/// payload byte is terminal truncation.
///
/// Pass a mutable reference to reuse the reader for additional frames.
///
/// # Errors
///
/// Returns a [`FrameError`] for I/O, truncation, bounds, or decoding failure.
pub fn read_frame<R: Read, T: DeserializeOwned>(mut reader: R) -> Result<Option<T>, FrameError> {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    let received = read_partial(&mut reader, &mut header)?;
    if received == 0 {
        return Ok(None);
    }
    if received != FRAME_HEADER_BYTES {
        return Err(FrameError::TruncatedHeader { received });
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

/// Fixed family encoded in every fresh-recovery envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolFamily {
    /// Stock-only fresh-recovery protocol.
    #[serde(rename = "bladerf_profile2_recovery")]
    Recovery,
}

/// Value-only claim for the retained loader-neutral library.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryFacts {
    version: String,
    sha256: String,
    marker: u32,
}

impl LibraryFacts {
    /// Construct an unchecked value-only claim.
    ///
    /// Typed client construction validates all fields before spawning.
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

    /// Claimed lowercase SHA-256.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Claimed B2L1 contract marker.
    #[must_use]
    pub const fn marker(&self) -> u32 {
        self.marker
    }
}

/// Value-only claim for the frozen stock image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageFacts {
    sha256: String,
    bytes: u64,
}

impl ImageFacts {
    /// Construct an unchecked value-only claim.
    ///
    /// Typed client construction validates the frozen digest and size.
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

/// Exact retained library and frozen stock-image facts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFacts {
    library: LibraryFacts,
    stock: ImageFacts,
}

impl BundleFacts {
    /// Construct unchecked value-only bundle facts.
    ///
    /// Typed client construction validates the complete bundle before spawn.
    #[must_use]
    pub const fn new(library: LibraryFacts, stock: ImageFacts) -> Self {
        Self { library, stock }
    }

    /// Retained loader-library facts.
    #[must_use]
    pub const fn library(&self) -> &LibraryFacts {
        &self.library
    }

    /// Frozen stock-image facts.
    #[must_use]
    pub const fn stock(&self) -> &ImageFacts {
        &self.stock
    }

    /// Split into owned library and stock claims.
    #[must_use]
    pub fn into_parts(self) -> (LibraryFacts, ImageFacts) {
        (self.library, self.stock)
    }
}

/// Fixed backend mode of one recovery child.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// Deterministic CPU-only recovery fixture.
    FixtureRecovery,
    /// Native fresh-handle recovery worker.
    NativeRecovery,
}

impl ServerMode {
    /// Fixed complete request allowance for either mode.
    #[must_use]
    pub const fn max_session_requests(self) -> usize {
        MAX_SESSION_REQUESTS
    }
}

/// Last exact recovery state confirmed by a successful response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    /// Worker opened and no image dispatch has started.
    Ready,
    /// Stock load returned success but identity remains unverified.
    StockUnverified,
    /// Frozen stock identity was verified under the existing readback contract.
    StockVerified,
    /// Orderly worker shutdown completed.
    Closed,
}

/// Operation carried by one recovery [`Request`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum RequestOperation {
    /// Negotiate this protocol and require one exact server mode.
    Hello {
        /// Exact mode the client intends to use.
        mode: ServerMode,
    },
    /// Dispatch the independently retained frozen stock image.
    RestoreStock {},
    /// Verify stock identity after dispatch.
    VerifyStock {},
    /// Orderly close from untouched or stock-verified state.
    Shutdown {},
}

/// One versioned recovery request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    operation: RequestOperation,
}

impl Request {
    /// Construct a request using the frozen family and version.
    #[must_use]
    pub const fn new(id: u64, operation: RequestOperation) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            operation,
        }
    }

    /// Decoded family discriminator.
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

/// Stable class for a terminal recovery-server error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed version, identifier, mode, or operation order.
    Protocol,
    /// Operation is not legal in the confirmed state.
    InvalidState,
    /// Fixed four-request allowance was reached.
    RequestLimit,
    /// Startup or one guarded backend transition failed.
    Backend,
}

/// Result carried by one recovery [`Response`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum ResponseKind {
    /// Negotiation and exact value-only bundle attestation succeeded.
    Hello {
        /// Exact server mode.
        mode: ServerMode,
        /// Initial state, required to be [`RecoveryState::Ready`].
        state: RecoveryState,
        /// Exact retained library and stock-image facts.
        bundle: BundleFacts,
    },
    /// One transition completed in the reported exact state.
    State {
        /// State after the correlated operation.
        state: RecoveryState,
    },
    /// A request failed and the session is terminal.
    Error {
        /// Stable machine-readable error class.
        code: ErrorCode,
        /// Bounded human-readable diagnostic.
        message: String,
    },
}

/// One versioned recovery response correlated to a request identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    result: ResponseKind,
}

impl Response {
    /// Construct a response using the frozen family and version.
    #[must_use]
    pub const fn new(id: u64, result: ResponseKind) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            result,
        }
    }

    /// Construct a terminal error response with a bounded UTF-8 diagnostic.
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

    /// Decoded family discriminator.
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

    /// Split into family, version, identifier, and result.
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

/// Errors produced by a synchronous fixture or native recovery client.
#[derive(Debug)]
pub enum ClientError {
    /// Native recovery is available only on the qualified x86-64 Linux target.
    UnsupportedNativeTarget,
    /// A required native executable or input path was empty or not absolute.
    InvalidPath {
        /// Stable argument name whose path was rejected.
        name: &'static str,
    },
    /// The requested device serial was not exactly 32 lowercase hex digits.
    InvalidSerial,
    /// The per-transaction timeout was zero or exceeded the bound.
    InvalidTimeout {
        /// Rejected timeout.
        timeout: Duration,
        /// Largest accepted timeout.
        maximum: Duration,
    },
    /// The expected library version differed from [`REQUIRED_LIBRARY_VERSION`].
    InvalidLibraryVersion,
    /// The expected library digest was not 64 lowercase hex digits.
    InvalidLibrarySha256,
    /// The expected library marker differed from [`REQUIRED_LIBRARY_MARKER`].
    InvalidLibraryMarker,
    /// The expected stock digest differed from [`REQUIRED_STOCK_SHA256`].
    InvalidStockSha256,
    /// The expected stock size differed from [`REQUIRED_STOCK_BYTES`].
    InvalidStockBytes {
        /// Rejected byte count.
        bytes: u64,
    },
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
    /// The four-request protocol allowance was exhausted.
    RequestLimit,
    /// A complete request/response exceeded its deadline.
    Timeout {
        /// Identifier of the timed-out request.
        request_id: u64,
        /// Configured transaction timeout.
        timeout: Duration,
    },
    /// Framing or standard-I/O failed during a request.
    Transport {
        /// Identifier of the affected request.
        request_id: u64,
        /// Underlying frame error.
        source: FrameError,
    },
    /// The child closed stdout before a complete response.
    EarlyExit {
        /// Identifier awaiting a response.
        request_id: u64,
    },
    /// The I/O worker stopped without a response.
    WorkerStopped {
        /// Identifier awaiting a response.
        request_id: u64,
    },
    /// A response violated family, version, ID, mode, bundle, or state.
    Protocol {
        /// Identifier of the affected request.
        request_id: u64,
        /// Diagnostic describing the mismatch.
        message: String,
    },
    /// The recovery server returned a typed terminal error.
    Server {
        /// Identifier correlated to the error.
        request_id: u64,
        /// Stable server error class.
        code: ErrorCode,
        /// Bounded server diagnostic.
        message: String,
    },
    /// A client operation was not legal in the last confirmed state.
    InvalidState {
        /// Attempted operation.
        operation: &'static str,
        /// Required state or states.
        expected: &'static str,
        /// Last state confirmed by a complete response.
        actual: RecoveryState,
    },
    /// Orderly shutdown produced a non-successful exit status.
    ChildExit {
        /// Observed child exit status.
        status: ExitStatus,
    },
    /// Waiting for the child failed.
    Wait(io::Error),
    /// The standard-I/O worker panicked.
    WorkerPanicked,
    /// The standard-I/O worker did not finish within its cleanup bound.
    WorkerCompletionTimeout {
        /// Fixed worker-completion bound.
        timeout: Duration,
    },
    /// The session was already closed or made terminal by an earlier action.
    Terminal,
    /// Process-only termination encountered cleanup failures.
    Termination {
        /// Combined kill, reap, or worker diagnostic.
        message: String,
    },
    /// Terminal cleanup also encountered a failure.
    Cleanup {
        /// Error that first made the session terminal.
        primary: Box<ClientError>,
        /// Additional kill, reap, or worker diagnostic.
        message: String,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedNativeTarget => f.write_str("native recovery requires x86-64 Linux"),
            Self::InvalidPath { name } => {
                write!(
                    f,
                    "native recovery {name} path must be nonempty and absolute"
                )
            }
            Self::InvalidSerial => f.write_str(
                "native recovery serial must be exactly 32 lowercase hexadecimal characters",
            ),
            Self::InvalidTimeout { timeout, maximum } => write!(
                f,
                "recovery transaction timeout {timeout:?} must be positive and at most {maximum:?}"
            ),
            Self::InvalidLibraryVersion => write!(
                f,
                "recovery library version must be exactly {REQUIRED_LIBRARY_VERSION}"
            ),
            Self::InvalidLibrarySha256 => f.write_str(
                "recovery library SHA-256 must be exactly 64 lowercase hexadecimal characters",
            ),
            Self::InvalidLibraryMarker => write!(
                f,
                "recovery library marker must be exactly 0x{REQUIRED_LIBRARY_MARKER:08x}"
            ),
            Self::InvalidStockSha256 => {
                f.write_str("stock image SHA-256 differs from the frozen xA9 v0.16.0 digest")
            }
            Self::InvalidStockBytes { bytes } => write!(
                f,
                "stock image byte count {bytes} differs from required {REQUIRED_STOCK_BYTES}"
            ),
            Self::Spawn(error) => write!(f, "spawn profile-2 recovery child: {error}"),
            Self::MissingPipe { name } => write!(f, "recovery child has no piped {name}"),
            Self::WorkerSpawn(error) => write!(f, "spawn recovery I/O worker: {error}"),
            Self::Publish { request_id } => {
                write!(f, "recovery I/O worker rejected request {request_id}")
            }
            Self::RequestLimit => {
                write!(f, "recovery request limit {MAX_SESSION_REQUESTS} reached")
            }
            Self::Timeout {
                request_id,
                timeout,
            } => write!(f, "recovery request {request_id} exceeded {timeout:?}"),
            Self::Transport { request_id, source } => {
                write!(
                    f,
                    "recovery request {request_id} transport failed: {source}"
                )
            }
            Self::EarlyExit { request_id } => {
                write!(
                    f,
                    "recovery child closed output before response {request_id}"
                )
            }
            Self::WorkerStopped { request_id } => {
                write!(
                    f,
                    "recovery I/O worker stopped before response {request_id}"
                )
            }
            Self::Protocol {
                request_id,
                message,
            } => write!(
                f,
                "recovery response {request_id} is incompatible: {message}"
            ),
            Self::Server {
                request_id,
                code,
                message,
            } => write!(
                f,
                "recovery request {request_id} failed ({code:?}): {message}"
            ),
            Self::InvalidState {
                operation,
                expected,
                actual,
            } => write!(
                f,
                "cannot {operation} from recovery state {actual:?}; required {expected}"
            ),
            Self::ChildExit { status } => {
                write!(f, "recovery shutdown exited with {status}")
            }
            Self::Wait(error) => write!(f, "wait for recovery child: {error}"),
            Self::WorkerPanicked => f.write_str("recovery I/O worker panicked"),
            Self::WorkerCompletionTimeout { timeout } => {
                write!(f, "recovery I/O worker did not stop within {timeout:?}")
            }
            Self::Terminal => f.write_str("recovery session is terminal"),
            Self::Termination { message } => {
                write!(f, "terminate recovery process: {message}")
            }
            Self::Cleanup { primary, message } => {
                write!(f, "{primary}; terminal cleanup also failed: {message}")
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

fn validate_timeout(timeout: Duration) -> Result<(), ClientError> {
    if timeout.is_zero() || timeout > MAX_TRANSACTION_TIMEOUT {
        return Err(ClientError::InvalidTimeout {
            timeout,
            maximum: MAX_TRANSACTION_TIMEOUT,
        });
    }
    Ok(())
}

fn check_request_allowance(next_id: u64) -> Result<(), ClientError> {
    if next_id > MAX_SESSION_REQUESTS as u64 {
        return Err(ClientError::RequestLimit);
    }
    Ok(())
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
    if bundle.stock.sha256 != REQUIRED_STOCK_SHA256 {
        return Err(ClientError::InvalidStockSha256);
    }
    if bundle.stock.bytes != REQUIRED_STOCK_BYTES {
        return Err(ClientError::InvalidStockBytes {
            bytes: bundle.stock.bytes,
        });
    }
    Ok(())
}

fn validate_native_path(name: &'static str, path: &Path) -> Result<(), ClientError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(ClientError::InvalidPath { name });
    }
    Ok(())
}

fn build_native_command(
    adapter: &Path,
    library: &Path,
    library_sha256: &str,
    serial: &str,
    stock_image: &Path,
) -> Result<(Command, BundleFacts), ClientError> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(ClientError::UnsupportedNativeTarget);
    }
    validate_native_path("adapter", adapter)?;
    validate_native_path("library", library)?;
    validate_native_path("stock image", stock_image)?;
    if !exact_lower_hex(library_sha256, 64) {
        return Err(ClientError::InvalidLibrarySha256);
    }
    if !exact_lower_hex(serial, 32) {
        return Err(ClientError::InvalidSerial);
    }
    let bundle = BundleFacts::new(
        LibraryFacts::new(
            REQUIRED_LIBRARY_VERSION,
            library_sha256,
            REQUIRED_LIBRARY_MARKER,
        ),
        ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
    );
    validate_bundle(&bundle)?;
    let arguments = vec![
        OsString::from("recovery-native"),
        OsString::from("--library"),
        library.as_os_str().to_owned(),
        OsString::from("--sha256"),
        OsString::from(library_sha256),
        OsString::from("--serial"),
        OsString::from(serial),
        OsString::from("--stock-image"),
        stock_image.as_os_str().to_owned(),
    ];
    Ok((build_command(adapter.as_os_str(), &arguments), bundle))
}

enum ExpectedResponse<'a> {
    Hello {
        mode: ServerMode,
        bundle: &'a BundleFacts,
    },
    State(RecoveryState),
}

fn validate_response(
    response: Response,
    request_id: u64,
    expected: ExpectedResponse<'_>,
) -> Result<RecoveryState, ClientError> {
    let (family, version, response_id, result) = response.into_parts();
    if family != PROTOCOL_FAMILY {
        return Err(ClientError::Protocol {
            request_id,
            message: format!("family {family:?} differs from {PROTOCOL_FAMILY:?}"),
        });
    }
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
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err(ClientError::Protocol {
                request_id,
                message: format!(
                    "server diagnostic has {} bytes; maximum is {MAX_ERROR_MESSAGE_BYTES}",
                    message.len()
                ),
            });
        }
        return Err(ClientError::Server {
            request_id,
            code,
            message,
        });
    }
    match (expected, result) {
        (
            ExpectedResponse::Hello {
                mode: expected_mode,
                bundle: expected_bundle,
            },
            ResponseKind::Hello {
                mode,
                state: RecoveryState::Ready,
                bundle,
            },
        ) if mode == expected_mode && &bundle == expected_bundle => Ok(RecoveryState::Ready),
        (ExpectedResponse::State(expected), ResponseKind::State { state }) if state == expected => {
            Ok(state)
        }
        (expected, actual) => Err(ClientError::Protocol {
            request_id,
            message: format!(
                "result {actual:?} does not match {}",
                match expected {
                    ExpectedResponse::Hello { .. } => "the exact Ready hello and bundle",
                    ExpectedResponse::State(_) => "the exact next recovery state",
                }
            ),
        }),
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

fn recovery_worker(mut input: ChildStdin, mut output: ChildStdout, requests: Receiver<WorkerJob>) {
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
    kill_and_reap_leader(child, &mut diagnostics);
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

/// Shared synchronous child session behind the two mode-typed clients.
///
/// One worker owns both pipes and one request may be outstanding. Every
/// transaction marks the session terminal before publication; only an exact
/// correlated response clears that uncertainty. Child stderr is inherited.
/// Common dynamic-loader injection variables are removed. Unix children use a
/// private process group so timeout, failure, explicit termination, and Drop
/// can clean up descendants before directly reaping the leader.
struct RecoverySession {
    child: Option<Child>,
    requests: Option<SyncSender<WorkerJob>>,
    worker: Option<JoinHandle<()>>,
    worker_completion: Option<Receiver<WorkerCompletion>>,
    process_group: u32,
    process_group_active: bool,
    timeout: Duration,
    next_id: u64,
    state: RecoveryState,
    terminal: bool,
}

impl fmt::Debug for RecoverySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoverySession")
            .field("timeout", &self.timeout)
            .field("next_id", &self.next_id)
            .field("state", &self.state)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl RecoverySession {
    fn spawn<P, I, S>(
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
        let command = build_command(program, arguments);
        Self::spawn_command(command, mode, expected_bundle, transaction_timeout)
    }

    fn spawn_command(
        command: Command,
        mode: ServerMode,
        expected_bundle: &BundleFacts,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError> {
        validate_timeout(transaction_timeout)?;
        validate_bundle(expected_bundle)?;
        let mut client = Self::spawn_unnegotiated(command, transaction_timeout)?;
        client.hello(mode, expected_bundle)?;
        Ok(client)
    }

    fn spawn_unnegotiated(
        mut command: Command,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError> {
        validate_timeout(transaction_timeout)?;
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
            .name("profile2-recovery-io".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recovery_worker(input, output, request_receiver);
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

        Ok(Self {
            child: Some(child),
            requests: Some(request_sender),
            worker: Some(worker),
            worker_completion: Some(completion_receiver),
            process_group,
            process_group_active: true,
            timeout: transaction_timeout,
            next_id: 1,
            state: RecoveryState::Ready,
            terminal: false,
        })
    }

    fn hello(
        &mut self,
        mode: ServerMode,
        expected_bundle: &BundleFacts,
    ) -> Result<(), ClientError> {
        let state = self.exchange(
            RequestOperation::Hello { mode },
            ExpectedResponse::Hello {
                mode,
                bundle: expected_bundle,
            },
        )?;
        if state == RecoveryState::Ready {
            Ok(())
        } else {
            self.fail(ClientError::Protocol {
                request_id: 1,
                message: format!("hello advanced to unexpected state {state:?}"),
            })
        }
    }

    fn transition(
        &mut self,
        operation_name: &'static str,
        operation: RequestOperation,
        required: RecoveryState,
        next: RecoveryState,
    ) -> Result<(), ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        if self.state != required {
            return self.fail(ClientError::InvalidState {
                operation: operation_name,
                expected: state_name(required),
                actual: self.state,
            });
        }
        let state = self.exchange(operation, ExpectedResponse::State(next))?;
        if state == next {
            Ok(())
        } else {
            self.fail(ClientError::Protocol {
                request_id: self.next_id.saturating_sub(1),
                message: format!("transition advanced to unexpected state {state:?}"),
            })
        }
    }

    fn shutdown(&mut self) -> Result<(), ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        if !matches!(
            self.state,
            RecoveryState::Ready | RecoveryState::StockVerified
        ) {
            return self.fail(ClientError::InvalidState {
                operation: "shut down the recovery worker",
                expected: "Ready or StockVerified",
                actual: self.state,
            });
        }
        let deadline = Instant::now() + self.timeout;
        let request_id = self.next_id;
        let state = self.exchange_until(
            RequestOperation::Shutdown {},
            ExpectedResponse::State(RecoveryState::Closed),
            deadline,
        )?;
        if state != RecoveryState::Closed {
            return self.fail(ClientError::Protocol {
                request_id,
                message: format!("shutdown advanced to unexpected state {state:?}"),
            });
        }

        self.terminal = true;
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
        Ok(())
    }

    fn exchange(
        &mut self,
        operation: RequestOperation,
        expected: ExpectedResponse<'_>,
    ) -> Result<RecoveryState, ClientError> {
        self.exchange_until(operation, expected, Instant::now() + self.timeout)
    }

    fn exchange_until(
        &mut self,
        operation: RequestOperation,
        expected: ExpectedResponse<'_>,
        deadline: Instant,
    ) -> Result<RecoveryState, ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        if let Err(error) = check_request_allowance(self.next_id) {
            return self.fail(error);
        }
        let request_id = self.next_id;
        self.terminal = true;
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
                return self.fail(ClientError::EarlyExit { request_id });
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
        let state = match validate_response(response, request_id, expected) {
            Ok(state) => state,
            Err(error) => return self.fail(error),
        };
        self.state = state;
        self.next_id += 1;
        self.terminal = false;
        Ok(state)
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

    fn terminate(&mut self) -> Result<(), ClientError> {
        match self.abort_and_reap() {
            Some(message) => Err(ClientError::Termination { message }),
            None => Ok(()),
        }
    }

    fn abort_and_reap(&mut self) -> Option<String> {
        self.terminal = true;
        self.requests.take();
        let mut diagnostics = Vec::new();
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
        if let Err(error) = self.finish_worker(self.next_id) {
            diagnostics.push(error.to_string());
        }
        (!diagnostics.is_empty()).then(|| diagnostics.join("; "))
    }
}

impl Drop for RecoverySession {
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

const fn state_name(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Ready => "Ready",
        RecoveryState::StockUnverified => "StockUnverified",
        RecoveryState::StockVerified => "StockVerified",
        RecoveryState::Closed => "Closed",
    }
}

/// Synchronous client requiring an honestly declared recovery fixture.
///
/// [`Self::spawn`] preserves caller arguments, removes `LD_PRELOAD`,
/// `LD_AUDIT`, and `LD_LIBRARY_PATH`, creates a private process group, inherits
/// stderr, and immediately completes an exact Hello and bundle attestation.
/// Any method error makes the session terminal. [`Self::terminate`] is
/// process-only and idempotent; Drop performs the same best-effort cleanup.
/// Direct operating-system kill and wait during termination or Drop can outlast
/// the transaction deadline; I/O-worker completion is separately bounded to
/// 100 ms. A later cleanup attempt can wait for the same worker after a timeout.
///
/// The Hello attests values and mode; it cannot prove that an arbitrary
/// caller-selected executable is CPU-only. Select only the trusted fixture.
pub struct FixtureRecoveryClient {
    session: RecoverySession,
}

impl fmt::Debug for FixtureRecoveryClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FixtureRecoveryClient")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl FixtureRecoveryClient {
    /// Spawn a supplied fixture and negotiate the exact bundle before return.
    ///
    /// # Errors
    ///
    /// Returns validation errors before spawn. After spawn, any failure
    /// terminalizes the child and attempts kill/reap.
    pub fn spawn<P, I, S>(
        program: P,
        arguments: I,
        expected_bundle: BundleFacts,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let session = RecoverySession::spawn(
            program,
            arguments,
            ServerMode::FixtureRecovery,
            &expected_bundle,
            transaction_timeout,
        )?;
        Ok(Self { session })
    }

    /// Last state established by an exact complete response.
    #[must_use]
    pub const fn state(&self) -> RecoveryState {
        self.session.state
    }

    /// Whether the client is closed or an error made further use unsafe.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.session.terminal
    }

    /// Dispatch the independently retained frozen stock image.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or transaction failure.
    pub fn restore_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "restore stock in a fresh fixture worker",
            RequestOperation::RestoreStock {},
            RecoveryState::Ready,
            RecoveryState::StockUnverified,
        )
    }

    /// Verify stock identity under the existing stock readback contract.
    ///
    /// The attested staged/dispatched bytes and configured/host-source/version
    /// readback do not constitute cryptographic FPGA-configuration readback.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or transaction failure.
    pub fn verify_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "verify stock in a fresh fixture worker",
            RequestOperation::VerifyStock {},
            RecoveryState::StockUnverified,
            RecoveryState::StockVerified,
        )
    }

    /// Orderly close from Ready or StockVerified and reap the child.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for state, transaction, exit, wait, or cleanup
    /// failure. Any error leaves the client terminal.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.session.shutdown()
    }

    /// Signal the process group and reap its leader without sending a request.
    ///
    /// This is idempotent and performs no loader, FPGA, RF, or device call.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Termination`] if process cleanup reported a
    /// failure.
    pub fn terminate(&mut self) -> Result<(), ClientError> {
        self.session.terminate()
    }
}

/// Synchronous client for the fixed native fresh-recovery child interface.
///
/// Construction accepts only absolute nonempty paths, a 64-lowercase-hex
/// library digest, and a 32-lowercase-hex device serial. It passes the fixed
/// argument sequence `recovery-native --library ... --sha256 ... --serial ...
/// --stock-image ...`, then requires `NativeRecovery` Hello facts. Paths and
/// serial never cross the recovery wire protocol.
///
/// This type starts a process which may open the selected bladeRF and RAM-load
/// the frozen stock image when its methods are driven. Callers must select the
/// trusted adapter executable and separately satisfy physical lab policy.
/// Direct operating-system kill and wait during termination or Drop can outlast
/// the transaction deadline; I/O-worker completion is separately bounded to
/// 100 ms. A later cleanup attempt can wait for the same worker after a timeout.
pub struct NativeRecoveryClient {
    session: RecoverySession,
}

impl fmt::Debug for NativeRecoveryClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeRecoveryClient")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl NativeRecoveryClient {
    /// Spawn the fixed native child and negotiate exact recovery facts.
    ///
    /// This validates syntax and paths but deliberately does not open or hash
    /// them in the parent; the lazy child stages and qualifies its own inputs
    /// only after a valid Hello.
    ///
    /// # Errors
    ///
    /// Returns validation errors before spawn, or a terminal process/protocol
    /// error after spawn.
    pub fn spawn<A, L, S>(
        adapter: A,
        library: L,
        library_sha256: &str,
        serial: &str,
        stock_image: S,
        transaction_timeout: Duration,
    ) -> Result<Self, ClientError>
    where
        A: AsRef<Path>,
        L: AsRef<Path>,
        S: AsRef<Path>,
    {
        validate_timeout(transaction_timeout)?;
        let (command, expected_bundle) = build_native_command(
            adapter.as_ref(),
            library.as_ref(),
            library_sha256,
            serial,
            stock_image.as_ref(),
        )?;
        let session = RecoverySession::spawn_command(
            command,
            ServerMode::NativeRecovery,
            &expected_bundle,
            transaction_timeout,
        )?;
        Ok(Self { session })
    }

    /// Last state established by an exact complete response.
    #[must_use]
    pub const fn state(&self) -> RecoveryState {
        self.session.state
    }

    /// Whether the client is closed or an error made further use unsafe.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.session.terminal
    }

    /// Dispatch the retained frozen stock image on the fresh native handle.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or transaction failure.
    pub fn restore_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "restore stock in a fresh native worker",
            RequestOperation::RestoreStock {},
            RecoveryState::Ready,
            RecoveryState::StockUnverified,
        )
    }

    /// Verify stock identity under the existing stock readback contract.
    ///
    /// The contract binds exact staged/dispatched bytes and successful load to
    /// configured/host-source/version 0.16.0 readback; it is not cryptographic
    /// FPGA-configuration readback.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or transaction failure.
    pub fn verify_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "verify stock in a fresh native worker",
            RequestOperation::VerifyStock {},
            RecoveryState::StockUnverified,
            RecoveryState::StockVerified,
        )
    }

    /// Orderly close from Ready or StockVerified and reap the child.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for state, transaction, exit, wait, or cleanup
    /// failure. Any error leaves the client terminal.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.session.shutdown()
    }

    /// Signal the process group and reap its leader without sending a request.
    ///
    /// Retry this operation if process or worker cleanup remains unconfirmed.
    /// It performs no loader or device operation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Termination`] if process cleanup reported a
    /// failure.
    pub fn terminate(&mut self) -> Result<(), ClientError> {
        self.session.terminate()
    }
}

/// Narrow backend required by the fresh-recovery server.
///
/// Implementations own one fresh recovery resource. A method error is terminal:
/// [`serve_with_factory`] writes at most one bounded error response, returns,
/// and never invokes [`Self::shutdown`] as error cleanup.
pub trait RecoveryBackend {
    /// Backend-specific operation error retained in the local error chain.
    type Error: Into<Box<dyn Error + Send + Sync>>;

    /// Exact value-only facts retained by this backend.
    fn bundle(&self) -> &BundleFacts;

    /// Dispatch the frozen stock image.
    ///
    /// # Errors
    ///
    /// Returns the terminal backend failure.
    fn restore_stock(&mut self) -> Result<(), Self::Error>;

    /// Verify stock under the existing configured/source/version contract.
    ///
    /// # Errors
    ///
    /// Returns the terminal backend failure.
    fn verify_stock(&mut self) -> Result<(), Self::Error>;

    /// Perform orderly shutdown from Ready or StockVerified.
    ///
    /// # Errors
    ///
    /// Returns the terminal backend failure. The server does not retry.
    fn shutdown(&mut self) -> Result<(), Self::Error>;
}

/// A factory input failure with a fixed wire diagnostic and a local source.
#[derive(Debug)]
pub struct BackendInputError {
    diagnostic: &'static str,
    source: Box<dyn Error + Send + Sync>,
}

impl BackendInputError {
    /// Report an invalid device serial without publishing the supplied value.
    pub fn invalid_serial(source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self {
            diagnostic: "bladeRF serial must be exactly 32 lowercase hexadecimal characters",
            source: source.into(),
        }
    }

    /// Report invalid static library inputs without publishing their values.
    pub fn invalid_library_expectation(source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        Self {
            diagnostic: "invalid recovery library path or SHA-256",
            source: source.into(),
        }
    }
}

impl fmt::Display for BackendInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.diagnostic)
    }
}

impl Error for BackendInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Terminal errors from [`serve_with_factory`].
#[derive(Debug)]
pub enum ServerError {
    /// Input framing or decoding failed before a correlated response.
    Frame(FrameError),
    /// Input ended before explicit shutdown.
    UnexpectedEof,
    /// A correlated terminal response was attempted.
    Terminal {
        /// Request identifier correlated to the error.
        request_id: u64,
        /// Stable error class placed on the wire.
        code: ErrorCode,
        /// Local diagnostic used to build the bounded wire message.
        message: String,
        /// Underlying backend error and its local context, if any.
        source: Option<Box<dyn Error + Send + Sync>>,
        /// Failure to publish the bounded error response, if any.
        publish: Option<FrameError>,
    },
    /// A successful response could not be published.
    Publish {
        /// Request identifier correlated to the response.
        request_id: u64,
        /// Underlying framing or output error.
        source: FrameError,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => write!(f, "read recovery request: {error}"),
            Self::UnexpectedEof => f.write_str("recovery input ended before explicit shutdown"),
            Self::Terminal {
                request_id,
                code,
                message,
                publish: None,
                ..
            } => write!(
                f,
                "recovery request {request_id} failed ({code:?}): {message}"
            ),
            Self::Terminal {
                request_id,
                code,
                message,
                publish: Some(error),
                ..
            } => write!(
                f,
                "recovery request {request_id} failed ({code:?}): {message}; publish terminal response: {error}"
            ),
            Self::Publish { request_id, source } => {
                write!(f, "publish recovery response {request_id}: {source}")
            }
        }
    }
}

impl Error for ServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Terminal {
                source: Some(error),
                ..
            } => Some(error.as_ref()),
            Self::Terminal {
                publish: Some(error),
                ..
            }
            | Self::Publish { source: error, .. } => Some(error),
            Self::UnexpectedEof | Self::Terminal { publish: None, .. } => None,
        }
    }
}

fn terminal_server_error(
    output: &mut impl Write,
    request_id: u64,
    code: ErrorCode,
    message: String,
) -> ServerError {
    publish_terminal_error(output, request_id, code, message, None)
}

fn backend_server_error(
    output: &mut impl Write,
    request_id: u64,
    message: &str,
    source: impl Into<Box<dyn Error + Send + Sync>>,
) -> ServerError {
    publish_terminal_error(
        output,
        request_id,
        ErrorCode::Backend,
        message.to_owned(),
        Some(source.into()),
    )
}

fn publish_terminal_error(
    output: &mut impl Write,
    request_id: u64,
    code: ErrorCode,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
) -> ServerError {
    let response = Response::error(request_id, code, message.clone());
    let publish = write_frame(output, &response).err();
    ServerError::Terminal {
        request_id,
        code,
        message,
        source,
        publish,
    }
}

fn write_server_response(output: &mut impl Write, response: &Response) -> Result<(), ServerError> {
    write_frame(output, response).map_err(|source| ServerError::Publish {
        request_id: response.id(),
        source,
    })
}

/// Serve one lazily constructed fixture or native recovery backend.
///
/// The factory remains dormant until a complete request has the exact family,
/// version, first ID, Hello operation, and required mode. Every error is
/// terminal. Backend failures publish a bounded operation diagnostic and retain
/// the underlying error as a local source. The factory can return a
/// [`BackendInputError`] to publish a fixed input diagnostic. Clean EOF is an
/// error until a successful Shutdown response has been written.
///
/// # Errors
///
/// Returns [`ServerError`] for framing, protocol, backend, response-publication,
/// or premature-EOF failure.
pub fn serve_with_factory<R, W, B, F, E>(
    input: &mut R,
    output: &mut W,
    mode: ServerMode,
    backend_factory: F,
) -> Result<(), ServerError>
where
    R: Read,
    W: Write,
    B: RecoveryBackend,
    F: FnOnce() -> Result<B, E>,
    E: Into<Box<dyn Error + Send + Sync>>,
{
    let mut backend_factory = Some(backend_factory);
    let mut backend = None;
    let mut state = None;
    let mut expected_id = 1_u64;
    let mut handled = 0_usize;

    loop {
        let request = match read_frame::<_, Request>(&mut *input) {
            Ok(Some(request)) => request,
            Ok(None) => return Err(ServerError::UnexpectedEof),
            Err(error) => return Err(ServerError::Frame(error)),
        };
        let (family, version, id, operation) = request.into_parts();
        if family != PROTOCOL_FAMILY {
            return Err(terminal_server_error(
                output,
                id,
                ErrorCode::Protocol,
                format!("request family {family:?} differs from {PROTOCOL_FAMILY:?}"),
            ));
        }
        if version != PROTOCOL_VERSION {
            return Err(terminal_server_error(
                output,
                id,
                ErrorCode::Protocol,
                format!("protocol version {version} differs from {PROTOCOL_VERSION}"),
            ));
        }
        if id != expected_id {
            return Err(terminal_server_error(
                output,
                id,
                ErrorCode::Protocol,
                format!("request id {id} is not the expected id {expected_id}"),
            ));
        }
        if handled == mode.max_session_requests() {
            return Err(terminal_server_error(
                output,
                id,
                ErrorCode::RequestLimit,
                format!("recovery request limit {MAX_SESSION_REQUESTS} reached"),
            ));
        }

        if state.is_none() {
            let RequestOperation::Hello {
                mode: requested_mode,
            } = operation
            else {
                return Err(terminal_server_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    "hello must be the first recovery operation".to_owned(),
                ));
            };
            if requested_mode != mode {
                return Err(terminal_server_error(
                    output,
                    id,
                    ErrorCode::Protocol,
                    format!("hello requested {requested_mode:?}, but child is {mode:?}"),
                ));
            }
            let Some(factory) = backend_factory.take() else {
                return Err(terminal_server_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    "recovery backend factory was already consumed".to_owned(),
                ));
            };
            let created = match factory() {
                Ok(created) => created,
                Err(error) => {
                    let (diagnostic, source) = match error.into().downcast::<BackendInputError>() {
                        Ok(input) => (input.diagnostic, input.source),
                        Err(source) => ("open recovery backend", source),
                    };
                    return Err(backend_server_error(output, id, diagnostic, source));
                }
            };
            if let Err(error) = validate_bundle(created.bundle()) {
                return Err(backend_server_error(
                    output,
                    id,
                    "qualified recovery bundle is invalid",
                    error,
                ));
            }
            let bundle = created.bundle().clone();
            backend = Some(created);
            state = Some(RecoveryState::Ready);
            write_server_response(
                output,
                &Response::new(
                    id,
                    ResponseKind::Hello {
                        mode,
                        state: RecoveryState::Ready,
                        bundle,
                    },
                ),
            )?;
            handled += 1;
            expected_id += 1;
            continue;
        }

        let current = state.expect("state is present after a successful hello");
        let Some(active) = backend.as_mut() else {
            return Err(terminal_server_error(
                output,
                id,
                ErrorCode::Backend,
                "recovery backend is unavailable after hello".to_owned(),
            ));
        };
        let (next, shutdown) = match (current, operation) {
            (_, RequestOperation::Hello { .. }) => {
                return Err(terminal_server_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    "recovery hello was already completed".to_owned(),
                ));
            }
            (RecoveryState::Ready, RequestOperation::RestoreStock {}) => {
                if let Err(error) = active.restore_stock() {
                    return Err(backend_server_error(output, id, "restore stock", error));
                }
                (RecoveryState::StockUnverified, false)
            }
            (RecoveryState::StockUnverified, RequestOperation::VerifyStock {}) => {
                if let Err(error) = active.verify_stock() {
                    return Err(backend_server_error(output, id, "verify stock", error));
                }
                (RecoveryState::StockVerified, false)
            }
            (
                RecoveryState::Ready | RecoveryState::StockVerified,
                RequestOperation::Shutdown {},
            ) => {
                if let Err(error) = active.shutdown() {
                    return Err(backend_server_error(
                        output,
                        id,
                        "shut down recovery backend",
                        error,
                    ));
                }
                (RecoveryState::Closed, true)
            }
            (state, operation) => {
                return Err(terminal_server_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    format!("operation {operation:?} is invalid while state is {state:?}"),
                ));
            }
        };

        write_server_response(
            output,
            &Response::new(id, ResponseKind::State { state: next }),
        )?;
        state = Some(next);
        handled += 1;
        expected_id += 1;
        if shutdown {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    #[cfg(unix)]
    use nix::sys::signal::kill;

    use super::*;

    const LIBRARY_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SERIAL: &str = "0123456789abcdef0123456789abcdef";

    fn library_facts() -> LibraryFacts {
        LibraryFacts::new(
            REQUIRED_LIBRARY_VERSION,
            LIBRARY_SHA256,
            REQUIRED_LIBRARY_MARKER,
        )
    }

    fn stock_facts() -> ImageFacts {
        ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES)
    }

    fn bundle() -> BundleFacts {
        BundleFacts::new(library_facts(), stock_facts())
    }

    fn requests(mode: ServerMode) -> Vec<Request> {
        vec![
            Request::new(1, RequestOperation::Hello { mode }),
            Request::new(2, RequestOperation::RestoreStock {}),
            Request::new(3, RequestOperation::VerifyStock {}),
            Request::new(4, RequestOperation::Shutdown {}),
        ]
    }

    fn responses(mode: ServerMode) -> Vec<Response> {
        vec![
            Response::new(
                1,
                ResponseKind::Hello {
                    mode,
                    state: RecoveryState::Ready,
                    bundle: bundle(),
                },
            ),
            Response::new(
                2,
                ResponseKind::State {
                    state: RecoveryState::StockUnverified,
                },
            ),
            Response::new(
                3,
                ResponseKind::State {
                    state: RecoveryState::StockVerified,
                },
            ),
            Response::new(
                4,
                ResponseKind::State {
                    state: RecoveryState::Closed,
                },
            ),
        ]
    }

    fn framed<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, value).unwrap();
        bytes
    }

    fn framed_many<T: Serialize>(values: impl IntoIterator<Item = T>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            write_frame(&mut bytes, &value).unwrap();
        }
        bytes
    }

    fn decode_responses(mut bytes: &[u8]) -> Vec<Response> {
        let mut values = Vec::new();
        while let Some(response) = read_frame(&mut bytes).unwrap() {
            values.push(response);
        }
        values
    }

    #[test]
    fn termination_retries_unconfirmed_worker_cleanup() {
        let (done, completion) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            if blocked.recv().is_ok() {
                let _ = done.send(WorkerCompletion::Completed);
            }
        });
        let mut client = RecoverySession {
            child: None,
            requests: None,
            worker: Some(worker),
            worker_completion: Some(completion),
            process_group: 0,
            process_group_active: false,
            timeout: Duration::from_secs(1),
            next_id: 1,
            state: RecoveryState::Ready,
            terminal: false,
        };

        for _ in 0..2 {
            assert!(matches!(
                client.terminate(),
                Err(ClientError::Termination { .. })
            ));
            assert!(client.terminal);
            assert!(client.worker.is_some());
            assert!(client.worker_completion.is_some());
        }

        release.send(()).unwrap();
        client.terminate().unwrap();
        assert!(client.worker.is_none());
        assert!(client.worker_completion.is_none());
        client.terminate().unwrap();
    }

    fn primary_error(mut error: &ClientError) -> &ClientError {
        while let ClientError::Cleanup { primary, .. } = error {
            error = primary;
        }
        error
    }

    #[test]
    fn protocol_schema_and_exact_spellings_are_frozen() {
        let hello = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::NativeRecovery,
            },
        );
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"operation":{"kind":"hello","mode":"native_recovery"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: RecoveryState::Ready,
                    bundle: BundleFacts::new(
                        LibraryFacts::new("2.6.0", "a", 1),
                        ImageFacts::new("b", 2),
                    ),
                },
            ))
            .unwrap(),
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"result":{"kind":"hello","mode":"fixture_recovery","state":"ready","bundle":{"library":{"version":"2.6.0","sha256":"a","marker":1},"stock":{"sha256":"b","bytes":2}}}}"#
        );
        for (operation, spelling) in [
            (
                RequestOperation::RestoreStock {},
                r#"{"kind":"restore_stock"}"#,
            ),
            (
                RequestOperation::VerifyStock {},
                r#"{"kind":"verify_stock"}"#,
            ),
            (RequestOperation::Shutdown {}, r#"{"kind":"shutdown"}"#),
        ] {
            assert_eq!(serde_json::to_string(&operation).unwrap(), spelling);
        }
        for (mode, spelling) in [
            (ServerMode::FixtureRecovery, r#""fixture_recovery""#),
            (ServerMode::NativeRecovery, r#""native_recovery""#),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), spelling);
            assert_eq!(mode.max_session_requests(), MAX_SESSION_REQUESTS);
        }
        for (state, spelling) in [
            (RecoveryState::Ready, r#""ready""#),
            (RecoveryState::StockUnverified, r#""stock_unverified""#),
            (RecoveryState::StockVerified, r#""stock_verified""#),
            (RecoveryState::Closed, r#""closed""#),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), spelling);
        }
        for (code, spelling) in [
            (ErrorCode::Protocol, r#""protocol""#),
            (ErrorCode::InvalidState, r#""invalid_state""#),
            (ErrorCode::RequestLimit, r#""request_limit""#),
            (ErrorCode::Backend, r#""backend""#),
        ] {
            assert_eq!(serde_json::to_string(&code).unwrap(), spelling);
        }
    }

    #[test]
    fn shared_bundle_constants_match_both_frozen_predecessor_families() {
        assert_eq!(
            REQUIRED_LIBRARY_VERSION,
            bladerf_profile2_loader_ipc::REQUIRED_LIBRARY_VERSION
        );
        assert_eq!(
            REQUIRED_LIBRARY_VERSION,
            bladerf_profile2_session_ipc::REQUIRED_LIBRARY_VERSION
        );
        assert_eq!(
            REQUIRED_LIBRARY_MARKER,
            bladerf_profile2_loader_ipc::REQUIRED_LIBRARY_MARKER
        );
        assert_eq!(
            REQUIRED_LIBRARY_MARKER,
            bladerf_profile2_session_ipc::REQUIRED_LIBRARY_MARKER
        );
        assert_eq!(
            REQUIRED_STOCK_SHA256,
            bladerf_profile2_loader_ipc::REQUIRED_STOCK_SHA256
        );
        assert_eq!(
            REQUIRED_STOCK_SHA256,
            bladerf_profile2_session_ipc::REQUIRED_STOCK_SHA256
        );
        assert_eq!(
            REQUIRED_STOCK_BYTES,
            bladerf_profile2_loader_ipc::REQUIRED_STOCK_BYTES
        );
        assert_eq!(
            REQUIRED_STOCK_BYTES,
            bladerf_profile2_session_ipc::REQUIRED_STOCK_BYTES
        );
        assert_ne!(
            serde_json::to_string(&PROTOCOL_FAMILY).unwrap(),
            serde_json::to_string(&bladerf_profile2_loader_ipc::PROTOCOL_FAMILY).unwrap()
        );
        assert_ne!(
            serde_json::to_string(&PROTOCOL_FAMILY).unwrap(),
            serde_json::to_string(&bladerf_profile2_session_ipc::PROTOCOL_FAMILY).unwrap()
        );
    }

    #[test]
    fn frame_round_trip_and_accessors_preserve_every_value() {
        for request in requests(ServerMode::FixtureRecovery) {
            let bytes = framed(&request);
            let decoded: Request = read_frame(&mut bytes.as_slice()).unwrap().unwrap();
            assert_eq!(decoded, request);
        }
        for response in responses(ServerMode::NativeRecovery)
            .into_iter()
            .chain([Response::error(5, ErrorCode::Backend, "failed")])
        {
            let bytes = framed(&response);
            let mut input = bytes.as_slice();
            assert_eq!(
                read_frame::<_, Response>(&mut input).unwrap(),
                Some(response)
            );
            assert!(read_frame::<_, Response>(&mut input).unwrap().is_none());
        }

        let value = bundle();
        assert_eq!(value.library().version(), REQUIRED_LIBRARY_VERSION);
        assert_eq!(value.library().sha256(), LIBRARY_SHA256);
        assert_eq!(value.library().marker(), REQUIRED_LIBRARY_MARKER);
        assert_eq!(value.stock().sha256(), REQUIRED_STOCK_SHA256);
        assert_eq!(value.stock().bytes(), REQUIRED_STOCK_BYTES);
        let (library, stock) = value.into_parts();
        assert_eq!(library.sha256(), LIBRARY_SHA256);
        assert_eq!(stock.bytes(), REQUIRED_STOCK_BYTES);

        let request = Request::new(9, RequestOperation::VerifyStock {});
        assert_eq!(request.family(), PROTOCOL_FAMILY);
        assert_eq!(request.version(), PROTOCOL_VERSION);
        assert_eq!(request.id(), 9);
        assert!(matches!(
            request.operation(),
            RequestOperation::VerifyStock {}
        ));
        let (_, _, _, operation) = request.into_parts();
        assert!(matches!(operation, RequestOperation::VerifyStock {}));
    }

    #[test]
    fn family_and_unknown_fields_are_rejected_at_every_layer() {
        for json in [
            r#"{"version":1,"id":1,"operation":{"kind":"shutdown"}}"#,
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"shutdown"}}"#,
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"extra":0,"operation":{"kind":"shutdown"}}"#,
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"operation":{"kind":"shutdown","extra":0}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err(), "{json}");
        }
        for json in [
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"result":{"kind":"state","state":"ready","extra":0}}"#,
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"result":{"kind":"hello","mode":"fixture_recovery","state":"ready","bundle":{"library":{"version":"2.6.0","sha256":"x","marker":1,"extra":0},"stock":{"sha256":"y","bytes":1}}}}"#,
            r#"{"family":"bladerf_profile2_recovery","version":1,"id":1,"result":{"kind":"hello","mode":"fixture_recovery","state":"ready","bundle":{"library":{"version":"2.6.0","sha256":"x","marker":1},"stock":{"sha256":"y","bytes":1},"custom":null}}}"#,
        ] {
            assert!(serde_json::from_str::<Response>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn framing_rejects_empty_truncated_oversized_and_malformed_values() {
        assert!(
            read_frame::<_, Request>(&mut [].as_slice())
                .unwrap()
                .is_none()
        );
        for received in 1..FRAME_HEADER_BYTES {
            let bytes = [1_u8; FRAME_HEADER_BYTES];
            assert!(matches!(
                read_frame::<_, Request>(&mut bytes[..received].as_ref()),
                Err(FrameError::TruncatedHeader { received: actual }) if actual == received
            ));
        }
        assert!(matches!(
            read_frame::<_, Request>(&mut 0_u32.to_le_bytes().as_slice()),
            Err(FrameError::EmptyFrame)
        ));
        assert!(matches!(
            read_frame::<_, Request>(&mut (MAX_FRAME_BYTES as u32 + 1).to_le_bytes().as_slice()),
            Err(FrameError::FrameTooLarge { .. })
        ));
        let mut truncated = Vec::from(10_u32.to_le_bytes());
        truncated.extend_from_slice(b"{}");
        assert!(matches!(
            read_frame::<_, Request>(&mut truncated.as_slice()),
            Err(FrameError::TruncatedPayload {
                expected: 10,
                received: 2
            })
        ));
        let mut malformed = Vec::from(1_u32.to_le_bytes());
        malformed.push(b'{');
        assert!(matches!(
            read_frame::<_, Request>(&mut malformed.as_slice()),
            Err(FrameError::Deserialize(_))
        ));
        let mut output = Vec::new();
        assert!(matches!(
            write_frame(&mut output, &"x".repeat(MAX_FRAME_BYTES)),
            Err(FrameError::FrameTooLarge { .. })
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn error_diagnostics_are_bounded_without_splitting_utf8() {
        let response = Response::error(1, ErrorCode::Backend, "é".repeat(MAX_ERROR_MESSAGE_BYTES));
        let ResponseKind::Error { message, .. } = response.result() else {
            panic!("expected error response");
        };
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(message.ends_with("..."));
        assert!(message.is_char_boundary(message.len()));

        let overlong = Response {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id: 1,
            result: ResponseKind::Error {
                code: ErrorCode::Backend,
                message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
            },
        };
        assert!(matches!(
            validate_response(overlong, 1, ExpectedResponse::State(RecoveryState::Ready)),
            Err(ClientError::Protocol { .. })
        ));
    }

    #[test]
    fn exact_response_family_version_id_mode_bundle_and_state_are_required() {
        let expected = bundle();
        assert_eq!(
            validate_response(
                responses(ServerMode::FixtureRecovery).remove(0),
                1,
                ExpectedResponse::Hello {
                    mode: ServerMode::FixtureRecovery,
                    bundle: &expected,
                },
            )
            .unwrap(),
            RecoveryState::Ready
        );
        let mut wrong_version = Response::new(
            1,
            ResponseKind::State {
                state: RecoveryState::Ready,
            },
        );
        wrong_version.version += 1;
        assert!(matches!(
            validate_response(
                wrong_version,
                1,
                ExpectedResponse::State(RecoveryState::Ready)
            ),
            Err(ClientError::Protocol { .. })
        ));
        for response in [
            Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::NativeRecovery,
                    state: RecoveryState::Ready,
                    bundle: expected.clone(),
                },
            ),
            Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: RecoveryState::StockUnverified,
                    bundle: expected.clone(),
                },
            ),
        ] {
            assert!(matches!(
                validate_response(
                    response,
                    1,
                    ExpectedResponse::Hello {
                        mode: ServerMode::FixtureRecovery,
                        bundle: &expected,
                    }
                ),
                Err(ClientError::Protocol { .. })
            ));
        }
        let mut wrong_bundle = bundle();
        wrong_bundle.library.sha256 = "f".repeat(64);
        assert!(matches!(
            validate_response(
                Response::new(
                    1,
                    ResponseKind::Hello {
                        mode: ServerMode::FixtureRecovery,
                        state: RecoveryState::Ready,
                        bundle: wrong_bundle,
                    },
                ),
                1,
                ExpectedResponse::Hello {
                    mode: ServerMode::FixtureRecovery,
                    bundle: &expected,
                },
            ),
            Err(ClientError::Protocol { .. })
        ));
        assert!(matches!(
            validate_response(
                Response::new(
                    2,
                    ResponseKind::State {
                        state: RecoveryState::StockVerified,
                    },
                ),
                1,
                ExpectedResponse::State(RecoveryState::StockUnverified)
            ),
            Err(ClientError::Protocol { .. })
        ));
    }

    #[test]
    fn client_request_allowance_accepts_four_and_rejects_five() {
        assert!(check_request_allowance(MAX_SESSION_REQUESTS as u64).is_ok());
        assert!(matches!(
            check_request_allowance(MAX_SESSION_REQUESTS as u64 + 1),
            Err(ClientError::RequestLimit)
        ));
    }

    #[test]
    fn invalid_timeout_bundle_and_native_inputs_fail_before_spawn() {
        let missing = "/this/program/must/not/be/spawned";
        for timeout in [
            Duration::ZERO,
            MAX_TRANSACTION_TIMEOUT + Duration::from_nanos(1),
        ] {
            assert!(matches!(
                FixtureRecoveryClient::spawn(missing, [] as [&str; 0], bundle(), timeout),
                Err(ClientError::InvalidTimeout { .. })
            ));
        }
        let mut invalid = bundle();
        invalid.library.version = "2.6.1".to_owned();
        assert!(matches!(
            FixtureRecoveryClient::spawn(missing, [] as [&str; 0], invalid, Duration::from_secs(1)),
            Err(ClientError::InvalidLibraryVersion)
        ));
        let mut invalid = bundle();
        invalid.stock.bytes += 1;
        assert!(matches!(
            FixtureRecoveryClient::spawn(missing, [] as [&str; 0], invalid, Duration::from_secs(1)),
            Err(ClientError::InvalidStockBytes { .. })
        ));

        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            for (adapter, library, sha, serial, stock, expected) in [
                (
                    Path::new("relative"),
                    Path::new("/library"),
                    LIBRARY_SHA256,
                    SERIAL,
                    Path::new("/stock"),
                    "path",
                ),
                (
                    Path::new("/adapter"),
                    Path::new("relative"),
                    LIBRARY_SHA256,
                    SERIAL,
                    Path::new("/stock"),
                    "path",
                ),
                (
                    Path::new("/adapter"),
                    Path::new("/library"),
                    "A",
                    SERIAL,
                    Path::new("/stock"),
                    "sha",
                ),
                (
                    Path::new("/adapter"),
                    Path::new("/library"),
                    LIBRARY_SHA256,
                    "ABC",
                    Path::new("/stock"),
                    "serial",
                ),
                (
                    Path::new("/adapter"),
                    Path::new("/library"),
                    LIBRARY_SHA256,
                    SERIAL,
                    Path::new("relative"),
                    "path",
                ),
            ] {
                let error = build_native_command(adapter, library, sha, serial, stock).unwrap_err();
                match expected {
                    "path" => assert!(matches!(error, ClientError::InvalidPath { .. })),
                    "sha" => assert!(matches!(error, ClientError::InvalidLibrarySha256)),
                    "serial" => assert!(matches!(error, ClientError::InvalidSerial)),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn native_command_has_fixed_arguments_and_sanitized_environment() {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return;
        }
        let (command, expected) = build_native_command(
            Path::new("/trusted/adapter"),
            Path::new("/sealed/library.so"),
            LIBRARY_SHA256,
            SERIAL,
            Path::new("/sealed/stock.rbf"),
        )
        .unwrap();
        assert_eq!(command.get_program(), OsStr::new("/trusted/adapter"));
        let expected_arguments = [
            "recovery-native",
            "--library",
            "/sealed/library.so",
            "--sha256",
            LIBRARY_SHA256,
            "--serial",
            SERIAL,
            "--stock-image",
            "/sealed/stock.rbf",
        ];
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            expected_arguments
                .iter()
                .map(OsStr::new)
                .collect::<Vec<_>>()
        );
        let removals: Vec<_> = command
            .get_envs()
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect();
        for name in ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"] {
            assert!(removals.contains(&OsStr::new(name)), "missing {name}");
        }
        assert_eq!(expected, bundle());
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailAt {
        Restore,
        Verify,
        Shutdown,
    }

    #[derive(Debug)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Debug)]
    struct ContextError {
        message: String,
        source: FakeError,
    }

    impl fmt::Display for ContextError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.message)
        }
    }

    impl Error for ContextError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(&self.source)
        }
    }

    struct FakeBackend {
        bundle: BundleFacts,
        events: Rc<RefCell<Vec<&'static str>>>,
        fail_at: Option<FailAt>,
    }

    impl FakeBackend {
        fn new(events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                bundle: bundle(),
                events,
                fail_at: None,
            }
        }

        fn call(&self, event: &'static str, point: FailAt) -> Result<(), FakeError> {
            self.events.borrow_mut().push(event);
            if self.fail_at == Some(point) {
                Err(FakeError(event))
            } else {
                Ok(())
            }
        }
    }

    impl RecoveryBackend for FakeBackend {
        type Error = FakeError;

        fn bundle(&self) -> &BundleFacts {
            &self.bundle
        }

        fn restore_stock(&mut self) -> Result<(), Self::Error> {
            self.call("restore", FailAt::Restore)
        }

        fn verify_stock(&mut self) -> Result<(), Self::Error> {
            self.call("verify", FailAt::Verify)
        }

        fn shutdown(&mut self) -> Result<(), Self::Error> {
            self.call("shutdown", FailAt::Shutdown)
        }
    }

    #[test]
    fn server_runs_exact_four_request_lifecycle_in_both_modes() {
        for mode in [ServerMode::FixtureRecovery, ServerMode::NativeRecovery] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input = Cursor::new(framed_many(requests(mode)));
            let mut output = Vec::new();
            serve_with_factory(&mut input, &mut output, mode, move || {
                Ok::<_, FakeError>(FakeBackend::new(observed))
            })
            .unwrap();
            assert_eq!(&*events.borrow(), &["restore", "verify", "shutdown"]);
            assert_eq!(decode_responses(&output), responses(mode));
            assert_eq!(mode.max_session_requests(), 4);
        }
    }

    #[test]
    fn server_allows_ready_shutdown_without_image_dispatch() {
        let mode = ServerMode::FixtureRecovery;
        let input_requests = [
            Request::new(1, RequestOperation::Hello { mode }),
            Request::new(2, RequestOperation::Shutdown {}),
        ];
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        let mut input = Cursor::new(framed_many(input_requests));
        let mut output = Vec::new();
        serve_with_factory(&mut input, &mut output, mode, move || {
            Ok::<_, FakeError>(FakeBackend::new(observed))
        })
        .unwrap();
        assert_eq!(&*events.borrow(), &["shutdown"]);
        assert_eq!(decode_responses(&output).len(), 2);
    }

    #[test]
    fn server_factory_is_lazy_until_exact_hello() {
        for request in [
            Request::new(
                2,
                RequestOperation::Hello {
                    mode: ServerMode::FixtureRecovery,
                },
            ),
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::NativeRecovery,
                },
            ),
            Request::new(1, RequestOperation::RestoreStock {}),
        ] {
            let calls = Rc::new(Cell::new(0));
            let observed = Rc::clone(&calls);
            let mut input = Cursor::new(framed(&request));
            let mut output = Vec::new();
            let error = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixtureRecovery,
                move || {
                    observed.set(observed.get() + 1);
                    Ok::<_, FakeError>(FakeBackend::new(Rc::new(RefCell::new(Vec::new()))))
                },
            )
            .unwrap_err();
            assert!(matches!(error, ServerError::Terminal { .. }));
            assert_eq!(calls.get(), 0);
            assert_eq!(decode_responses(&output).len(), 1);
        }

        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let mut input = Cursor::new(vec![1, 0]);
        let mut output = Vec::new();
        assert!(matches!(
            serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixtureRecovery,
                move || {
                    observed.set(observed.get() + 1);
                    Ok::<_, FakeError>(FakeBackend::new(Rc::new(RefCell::new(Vec::new()))))
                }
            ),
            Err(ServerError::Frame(_))
        ));
        assert_eq!(calls.get(), 0);
        assert!(output.is_empty());
    }

    #[test]
    fn server_factory_and_bundle_failures_are_bounded_backend_errors() {
        let mode = ServerMode::FixtureRecovery;
        let hello = framed(&Request::new(1, RequestOperation::Hello { mode }));

        let mut input = Cursor::new(hello.clone());
        let mut output = Vec::new();
        let error =
            serve_with_factory::<_, _, FakeBackend, _, _>(&mut input, &mut output, mode, || {
                Err(BackendInputError::invalid_library_expectation(FakeError(
                    "/private/recovery/library",
                )))
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ServerError::Terminal {
                code: ErrorCode::Backend,
                ..
            }
        ));
        assert_eq!(
            error.source().unwrap().to_string(),
            "/private/recovery/library"
        );
        assert!(matches!(
            decode_responses(&output)[0].result(),
            ResponseKind::Error {
                code: ErrorCode::Backend,
                message,
            } if message == "invalid recovery library path or SHA-256"
        ));

        let mut input = Cursor::new(hello);
        let mut output = Vec::new();
        let error = serve_with_factory(&mut input, &mut output, mode, || {
            let mut backend = FakeBackend::new(Rc::new(RefCell::new(Vec::new())));
            backend.bundle.library.sha256 = "bad".to_owned();
            Ok::<_, FakeError>(backend)
        })
        .unwrap_err();
        assert!(matches!(
            error,
            ServerError::Terminal {
                code: ErrorCode::Backend,
                ..
            }
        ));
        let ResponseKind::Error { message, .. } =
            decode_responses(&output).remove(0).into_parts().3
        else {
            panic!("expected backend error");
        };
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);

        let local_message = "/local/backend/config".repeat(MAX_ERROR_MESSAGE_BYTES);
        let expected_local_message = local_message.clone();
        let mut input = Cursor::new(framed(&Request::new(1, RequestOperation::Hello { mode })));
        let mut output = Vec::new();
        let error = serve_with_factory::<_, _, FakeBackend, _, _>(
            &mut input,
            &mut output,
            mode,
            move || {
                Err(ContextError {
                    message: local_message,
                    source: FakeError("private backend failure"),
                })
            },
        )
        .unwrap_err();
        let source = error.source().unwrap();
        assert_eq!(source.to_string(), expected_local_message);
        assert_eq!(
            source.source().unwrap().to_string(),
            "private backend failure"
        );
        let ResponseKind::Error { message, .. } =
            decode_responses(&output).remove(0).into_parts().3
        else {
            panic!("expected bounded wire error");
        };
        assert_eq!(message, "open recovery backend");
    }

    #[test]
    fn backend_error_is_terminal_and_never_calls_shutdown_cleanup() {
        for point in [FailAt::Restore, FailAt::Verify, FailAt::Shutdown] {
            let mode = ServerMode::FixtureRecovery;
            let request_count = match point {
                FailAt::Restore => 2,
                FailAt::Verify => 3,
                FailAt::Shutdown => 4,
            };
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input =
                Cursor::new(framed_many(requests(mode).into_iter().take(request_count)));
            let mut output = Vec::new();
            let error = serve_with_factory(&mut input, &mut output, mode, move || {
                let mut backend = FakeBackend::new(observed);
                backend.fail_at = Some(point);
                Ok::<_, FakeError>(backend)
            })
            .unwrap_err();
            assert!(matches!(
                error,
                ServerError::Terminal {
                    code: ErrorCode::Backend,
                    ..
                }
            ));
            assert!(error.source().unwrap().is::<FakeError>());
            let expected = match point {
                FailAt::Restore => vec!["restore"],
                FailAt::Verify => vec!["restore", "verify"],
                FailAt::Shutdown => vec!["restore", "verify", "shutdown"],
            };
            assert_eq!(*events.borrow(), expected);
            let decoded = decode_responses(&output);
            assert_eq!(decoded.len(), request_count);
            assert!(matches!(
                decoded.last().unwrap().result(),
                ResponseKind::Error {
                    code: ErrorCode::Backend,
                    ..
                }
            ));
        }
    }

    #[test]
    fn server_rejects_invalid_transition_and_premature_eof() {
        let mode = ServerMode::FixtureRecovery;
        let input_requests = [
            Request::new(1, RequestOperation::Hello { mode }),
            Request::new(2, RequestOperation::VerifyStock {}),
        ];
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        let mut input = Cursor::new(framed_many(input_requests));
        let mut output = Vec::new();
        assert!(matches!(
            serve_with_factory(&mut input, &mut output, mode, move || {
                Ok::<_, FakeError>(FakeBackend::new(observed))
            }),
            Err(ServerError::Terminal {
                code: ErrorCode::InvalidState,
                ..
            })
        ));
        assert!(events.borrow().is_empty());

        let mut input = Cursor::new(framed(&Request::new(1, RequestOperation::Hello { mode })));
        let mut output = Vec::new();
        assert!(matches!(
            serve_with_factory(&mut input, &mut output, mode, || {
                Ok::<_, FakeError>(FakeBackend::new(Rc::new(RefCell::new(Vec::new()))))
            }),
            Err(ServerError::UnexpectedEof)
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
        fn new(requests: &[Request], responses: &[Response], suffix: &str) -> Self {
            assert_eq!(requests.len(), responses.len());
            let directory = tempfile::tempdir().unwrap();
            let script_path = directory.path().join("recovery-child.sh");
            let program = PathBuf::from("/bin/sh");
            let mut script = String::from("#!/bin/sh\nset -eu\n");
            let mut arguments = vec![script_path.clone()];
            let mut captures = Vec::new();
            for (index, (request, response)) in requests.iter().zip(responses).enumerate() {
                let response_path = directory.path().join(format!("response-{index}.bin"));
                let capture_path = directory.path().join(format!("request-{index}.bin"));
                fs::write(&response_path, framed(response)).unwrap();
                script.push_str(&format!(
                    "dd bs=1 count={} of=\"$2\" 2>/dev/null\ndd if=\"$1\" bs=1 2>/dev/null\nshift 2\n",
                    framed(request).len()
                ));
                arguments.push(response_path);
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
                let mut input = bytes.as_slice();
                assert_eq!(
                    read_frame::<_, Request>(&mut input).unwrap(),
                    Some(expected.clone())
                );
                assert!(input.is_empty());
            }
        }
    }

    #[cfg(unix)]
    fn path_arguments(paths: &[PathBuf]) -> impl Iterator<Item = &Path> {
        paths.iter().map(PathBuf::as_path)
    }

    #[cfg(unix)]
    #[test]
    fn fixture_client_runs_exact_four_request_lifecycle() {
        let expected_requests = requests(ServerMode::FixtureRecovery);
        let expected_responses = responses(ServerMode::FixtureRecovery);
        let child = ScriptedChild::new(&expected_requests, &expected_responses, "exit 0");
        let mut client = FixtureRecoveryClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            bundle(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(client.state(), RecoveryState::Ready);
        assert!(!client.is_terminal());
        client.restore_stock().unwrap();
        assert_eq!(client.state(), RecoveryState::StockUnverified);
        client.verify_stock().unwrap();
        assert_eq!(client.state(), RecoveryState::StockVerified);
        client.shutdown().unwrap();
        assert_eq!(client.state(), RecoveryState::Closed);
        assert!(client.is_terminal());
        assert!(matches!(client.shutdown(), Err(ClientError::Terminal)));
        client.terminate().unwrap();
        client.terminate().unwrap();
        child.assert_requests(&expected_requests);
    }

    #[cfg(unix)]
    #[test]
    fn fixture_client_ready_shutdown_is_legal() {
        let mode = ServerMode::FixtureRecovery;
        let expected_requests = vec![
            Request::new(1, RequestOperation::Hello { mode }),
            Request::new(2, RequestOperation::Shutdown {}),
        ];
        let expected_responses = vec![
            Response::new(
                1,
                ResponseKind::Hello {
                    mode,
                    state: RecoveryState::Ready,
                    bundle: bundle(),
                },
            ),
            Response::new(
                2,
                ResponseKind::State {
                    state: RecoveryState::Closed,
                },
            ),
        ];
        let child = ScriptedChild::new(&expected_requests, &expected_responses, "exit 0");
        let mut client = FixtureRecoveryClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            bundle(),
            Duration::from_secs(2),
        )
        .unwrap();
        client.shutdown().unwrap();
        child.assert_requests(&expected_requests);
    }

    #[cfg(unix)]
    #[test]
    fn client_invalid_state_is_terminal_without_publication() {
        let mode = ServerMode::FixtureRecovery;
        let expected_requests = vec![Request::new(1, RequestOperation::Hello { mode })];
        let expected_responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode,
                state: RecoveryState::Ready,
                bundle: bundle(),
            },
        )];
        let child = ScriptedChild::new(&expected_requests, &expected_responses, "sleep 10");
        let mut client = FixtureRecoveryClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            bundle(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            client.verify_stock(),
            Err(ClientError::InvalidState { .. })
        ));
        assert!(client.is_terminal());
        assert!(matches!(client.restore_stock(), Err(ClientError::Terminal)));
        child.assert_requests(&expected_requests);
    }

    #[cfg(unix)]
    #[test]
    fn every_lost_transition_response_is_terminal_without_state_advance() {
        let mode = ServerMode::FixtureRecovery;
        let all_requests = requests(mode);
        let all_responses = responses(mode);
        let confirmed_states = [
            RecoveryState::Ready,
            RecoveryState::StockUnverified,
            RecoveryState::StockVerified,
        ];

        for uncertain_index in 1..=3 {
            let uncertain = &all_requests[uncertain_index];
            let suffix = format!(
                "dd bs=1 count={} of=\"$1\" 2>/dev/null\nsleep 10",
                framed(uncertain).len()
            );
            let mut child = ScriptedChild::new(
                &all_requests[..uncertain_index],
                &all_responses[..uncertain_index],
                &suffix,
            );
            let capture = child
                ._directory
                .path()
                .join(format!("lost-{uncertain_index}.bin"));
            child.arguments.push(capture.clone());
            child.captures.push(capture);
            let mut client = FixtureRecoveryClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                bundle(),
                Duration::from_millis(150),
            )
            .unwrap();
            if uncertain_index > 1 {
                client.restore_stock().unwrap();
            }
            if uncertain_index > 2 {
                client.verify_stock().unwrap();
            }
            let error = match uncertain_index {
                1 => client.restore_stock().unwrap_err(),
                2 => client.verify_stock().unwrap_err(),
                3 => client.shutdown().unwrap_err(),
                _ => unreachable!(),
            };
            assert!(matches!(
                primary_error(&error),
                ClientError::Timeout { request_id, .. }
                    if *request_id == uncertain_index as u64 + 1
            ));
            assert_eq!(client.state(), confirmed_states[uncertain_index - 1]);
            assert!(client.is_terminal());
            assert!(matches!(client.terminate(), Ok(())));
            child.assert_requests(&all_requests[..=uncertain_index]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_early_exit_malformed_output_drop_and_descendant_pipe_are_bounded() {
        let started = Instant::now();
        assert!(matches!(
            FixtureRecoveryClient::spawn("sleep", ["10"], bundle(), Duration::from_millis(50)),
            Err(ClientError::Timeout { .. }) | Err(ClientError::Cleanup { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));

        assert!(matches!(
            FixtureRecoveryClient::spawn(
                "true",
                [] as [&str; 0],
                bundle(),
                Duration::from_millis(250)
            ),
            Err(ClientError::EarlyExit { .. }) | Err(ClientError::Cleanup { .. })
        ));
        assert!(matches!(
            FixtureRecoveryClient::spawn(
                "sh",
                ["-c", "printf '\\001\\000'"],
                bundle(),
                Duration::from_millis(250)
            ),
            Err(ClientError::Transport { .. }) | Err(ClientError::Cleanup { .. })
        ));

        let probe_directory = tempfile::tempdir().unwrap();
        let descendant_pid_path = probe_directory.path().join("descendant.pid");
        let descendant_arguments = vec![
            OsString::from("-c"),
            OsString::from(
                "sleep 30 & descendant=$!; printf '%s\\n' \"$descendant\" > \"$1\"; wait",
            ),
            OsString::from("recovery-cleanup-probe"),
            descendant_pid_path.as_os_str().to_owned(),
        ];
        let command = build_command("sh", &descendant_arguments);
        let mut session =
            RecoverySession::spawn_unnegotiated(command, Duration::from_millis(250)).unwrap();
        let started = Instant::now();
        let error = session
            .hello(ServerMode::FixtureRecovery, &bundle())
            .unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Timeout { request_id: 1, .. }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        let descendant_pid = fs::read_to_string(&descendant_pid_path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let gone_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match kill(Pid::from_raw(descendant_pid), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) if Instant::now() < gone_deadline => {
                    thread::sleep(Duration::from_millis(2));
                }
                result => {
                    panic!("descendant {descendant_pid} survived private-group cleanup: {result:?}")
                }
            }
        }

        let expected_requests = vec![Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        )];
        let expected_responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixtureRecovery,
                state: RecoveryState::Ready,
                bundle: bundle(),
            },
        )];
        let child = ScriptedChild::new(&expected_requests, &expected_responses, "sleep 10");
        let started = Instant::now();
        {
            let _client = FixtureRecoveryClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                bundle(),
                Duration::from_secs(1),
            )
            .unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn process_only_termination_is_live_idempotent_and_debug_redacts_arguments() {
        let mode = ServerMode::FixtureRecovery;
        let expected_requests = vec![Request::new(1, RequestOperation::Hello { mode })];
        let expected_responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode,
                state: RecoveryState::Ready,
                bundle: bundle(),
            },
        )];
        let child = ScriptedChild::new(&expected_requests, &expected_responses, "sleep 10");
        let mut client = FixtureRecoveryClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            bundle(),
            Duration::from_secs(1),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("recovery-child"));
        assert!(!debug.contains(LIBRARY_SHA256));
        client.terminate().unwrap();
        client.terminate().unwrap();
        assert!(client.is_terminal());
        assert_eq!(client.state(), RecoveryState::Ready);
        child.assert_requests(&expected_requests);
    }
}
