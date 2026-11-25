//! Bounded, device-independent IPC for guarded profile-2 image transitions.
//!
//! This is a separate protocol family from `bladerf-profile2-ipc`. Frames use
//! a four-byte little-endian JSON payload length, but this crate has its own
//! schema version and lifecycle. The primary role can load and verify the
//! custom image before restoring and verifying stock; the recovery role can
//! only restore and verify stock. Successful responses report the exact next
//! state. Any framing, protocol, child, deadline, or server error makes the
//! client session terminal.
//!
//! Bundle attestations contain values only: library version, digest and marker,
//! plus image digests and byte counts. Paths and device identifiers never cross
//! this wire contract. Incompatible changes to fields, tags, spellings, or
//! required lifecycle behavior must increment [`PROTOCOL_VERSION`].
//!
//! Use [`PrimaryFixtureClient`] to exercise the complete image transition and
//! [`RecoveryFixtureClient`] to exercise the stock-only path. Both validate
//! [`BundleFacts`] during the initial exchange.
//!
//! ```
//! use bladerf_profile2_loader_ipc::{
//!     Request, RequestOperation, ServerMode, read_frame, write_frame,
//! };
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let request = Request::new(
//!     1,
//!     RequestOperation::Hello {
//!         mode: ServerMode::FixturePrimary,
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

/// Current loader request and response schema version.
pub const PROTOCOL_VERSION: u16 = 1;

/// Fixed discriminator for this loader protocol family.
pub const PROTOCOL_FAMILY: ProtocolFamily = ProtocolFamily::Loader;

/// Maximum JSON payload bytes in one loader request or response.
pub const MAX_FRAME_BYTES: usize = 4096;

/// Maximum UTF-8 bytes retained in one terminal server diagnostic.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 512;

/// Maximum requests in a complete primary session, including hello and shutdown.
pub const MAX_PRIMARY_SESSION_REQUESTS: usize = 6;

/// Maximum requests in a complete recovery session, including hello and shutdown.
pub const MAX_RECOVERY_SESSION_REQUESTS: usize = 4;

/// Largest accepted deadline for one loader-client transaction.
pub const MAX_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Exact retained libbladeRF semantic version required by this protocol.
pub const REQUIRED_LIBRARY_VERSION: &str = "2.6.0";

/// Exact B2L1 loader-neutral open-contract marker.
pub const REQUIRED_LIBRARY_MARKER: u32 = 0x4232_4c01;

/// Largest accepted custom or stock image attestation.
pub const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

/// Frozen stock xA9 v0.16.0 image digest required for recovery.
pub const REQUIRED_STOCK_SHA256: &str =
    "3004467110e2b2790c8f6128f998c2149905a88eed91a87f11af3db524d60c65";

/// Frozen stock xA9 v0.16.0 image byte count required for recovery.
pub const REQUIRED_STOCK_BYTES: u64 = 12_858_972;

const FRAME_HEADER_BYTES: usize = 4;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_millis(100);

/// Errors produced while reading or writing a bounded loader-protocol frame.
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
        /// Declared length, or encoded lower bound reached before stopping.
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
            Self::Io(error) => write!(formatter, "loader protocol I/O failed: {error}"),
            Self::Serialize(error) => write!(formatter, "serialize loader protocol frame: {error}"),
            Self::Deserialize(error) => write!(formatter, "decode loader protocol frame: {error}"),
            Self::EmptyFrame => formatter.write_str("loader protocol frame payload is empty"),
            Self::FrameTooLarge { length, maximum } => write!(
                formatter,
                "loader protocol frame reached {length} payload bytes; maximum is {maximum}"
            ),
            Self::TruncatedHeader { received } => write!(
                formatter,
                "loader protocol frame ended after {received} of {FRAME_HEADER_BYTES} header bytes"
            ),
            Self::TruncatedPayload { expected, received } => write!(
                formatter,
                "loader protocol frame ended after {received} of {expected} payload bytes"
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
            return Err(io::Error::other(
                "loader protocol frame exceeds its payload bound",
            ));
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
/// Returns a [`FrameError`] for serialization, size, or transport failure. An
/// I/O error may leave a partial frame; discard the transport after any error.
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
/// Clean EOF before a new prefix returns `Ok(None)`. EOF after any prefix or
/// payload byte is a terminal truncation error.
///
/// Pass a mutable reference to reuse the reader for additional frames.
///
/// # Errors
///
/// Returns a [`FrameError`] for I/O, truncation, bounds, or decoding failure.
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

/// Fixed loader protocol family encoded in every request and response envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProtocolFamily {
    /// Guarded profile-2 loader protocol, independent of register IPC v1.
    #[serde(rename = "bladerf_profile2_loader")]
    Loader,
}

/// Value-only attestation claim for the retained loader-neutral library.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryFacts {
    version: String,
    sha256: String,
    marker: u32,
}

impl LibraryFacts {
    /// Construct an unchecked value-only claim without a path or live owner.
    /// Typed client construction validates the required version, digest syntax,
    /// and marker before it starts a process.
    #[must_use]
    pub fn new(version: impl Into<String>, sha256: impl Into<String>, marker: u32) -> Self {
        Self {
            version: version.into(),
            sha256: sha256.into(),
            marker,
        }
    }

    /// Claimed semantic version string reported by the retained library.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Claimed SHA-256 of the qualified library bytes.
    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Claimed loader-neutral contract marker returned by the library.
    #[must_use]
    pub const fn marker(&self) -> u32 {
        self.marker
    }
}

/// Value-only attestation claim for one sealed FPGA image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImageFacts {
    sha256: String,
    bytes: u64,
}

impl ImageFacts {
    /// Construct an unchecked claim without a source or sealed-descriptor path.
    /// Typed client construction validates digest syntax and byte bounds.
    #[must_use]
    pub fn new(sha256: impl Into<String>, bytes: u64) -> Self {
        Self {
            sha256: sha256.into(),
            bytes,
        }
    }

    /// Claimed SHA-256 of the image bytes.
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

/// Value-only attestation for one loader worker's retained bundle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFacts {
    library: LibraryFacts,
    custom: Option<ImageFacts>,
    stock: ImageFacts,
}

impl BundleFacts {
    /// Construct bundle facts. Primary mode requires `custom`; recovery mode
    /// requires it to be absent, which the typed client validates before spawn.
    #[must_use]
    pub const fn new(library: LibraryFacts, custom: Option<ImageFacts>, stock: ImageFacts) -> Self {
        Self {
            library,
            custom,
            stock,
        }
    }

    /// Retained loader-library facts.
    #[must_use]
    pub const fn library(&self) -> &LibraryFacts {
        &self.library
    }

    /// Custom-image facts, present only in primary mode.
    #[must_use]
    pub const fn custom(&self) -> Option<&ImageFacts> {
        self.custom.as_ref()
    }

    /// Frozen stock-image facts.
    #[must_use]
    pub const fn stock(&self) -> &ImageFacts {
        &self.stock
    }

    /// Split this value into library, optional custom, and stock facts.
    #[must_use]
    pub fn into_parts(self) -> (LibraryFacts, Option<ImageFacts>, ImageFacts) {
        (self.library, self.custom, self.stock)
    }
}

/// Fixed role and backend mode of one loader process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerMode {
    /// Server-declared primary worker fixture.
    FixturePrimary,
    /// Server-declared independent stock-recovery worker fixture.
    FixtureRecovery,
}

impl ServerMode {
    /// Maximum complete request count for this role, including lifecycle calls.
    #[must_use]
    pub const fn max_session_requests(self) -> usize {
        match self {
            Self::FixturePrimary => MAX_PRIMARY_SESSION_REQUESTS,
            Self::FixtureRecovery => MAX_RECOVERY_SESSION_REQUESTS,
        }
    }
}

/// Last exact loader state confirmed by a successful response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoaderState {
    /// Worker opened and no image dispatch has started.
    Ready,
    /// Custom load returned success but its identity is not verified.
    CustomUnverified,
    /// Exact custom profile identity was verified.
    CustomVerified,
    /// Stock load returned success but its identity is not verified.
    StockUnverified,
    /// Exact stock identity was verified.
    StockVerified,
    /// Orderly worker shutdown completed.
    Closed,
}

/// Operation carried by one loader [`Request`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum RequestOperation {
    /// Negotiate this protocol and require one exact server role.
    Hello {
        /// Exact role the client intends to use.
        mode: ServerMode,
    },
    /// Dispatch the retained sealed custom profile-2 image.
    LoadProfile2 {},
    /// Verify exact profile-2 identity after custom dispatch.
    VerifyProfile2 {},
    /// Dispatch the retained sealed stock image.
    RestoreStock {},
    /// Verify exact stock identity after stock dispatch.
    VerifyStock {},
    /// Orderly close from an untouched or stock-verified worker.
    Shutdown {},
}

/// One versioned loader client request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    operation: RequestOperation,
}

impl Request {
    /// Construct a request using [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn new(id: u64, operation: RequestOperation) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            operation,
        }
    }

    /// Protocol-family discriminator decoded from this request.
    #[must_use]
    pub const fn family(&self) -> ProtocolFamily {
        self.family
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

    /// Borrow the typed operation.
    #[must_use]
    pub const fn operation(&self) -> &RequestOperation {
        &self.operation
    }

    /// Split the request into its version, ID, and operation.
    #[must_use]
    pub fn into_parts(self) -> (ProtocolFamily, u16, u64, RequestOperation) {
        (self.family, self.version, self.id, self.operation)
    }
}

/// Stable class for a terminal loader-server error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed version, request ID, mode, or operation order.
    Protocol,
    /// Operation is not legal in the current loader state.
    InvalidState,
    /// The fixed role request allowance was reached.
    RequestLimit,
    /// Worker startup or one guarded transition failed.
    Backend,
}

/// Result carried by one loader [`Response`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum ResponseKind {
    /// Protocol negotiation and value-only bundle attestation succeeded.
    Hello {
        /// Exact server role.
        mode: ServerMode,
        /// Initial state, required to be [`LoaderState::Ready`].
        state: LoaderState,
        /// Exact retained library and image facts.
        bundle: BundleFacts,
    },
    /// One transition completed in the reported exact state.
    State {
        /// State after the correlated operation.
        state: LoaderState,
    },
    /// A request failed and the session is terminal.
    Error {
        /// Stable machine-readable error class.
        code: ErrorCode,
        /// Bounded human-readable diagnostic.
        message: String,
    },
}

/// One versioned loader response correlated to a request ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    family: ProtocolFamily,
    version: u16,
    id: u64,
    result: ResponseKind,
}

impl Response {
    /// Construct a response using [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn new(id: u64, result: ResponseKind) -> Self {
        Self {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id,
            result,
        }
    }

    /// Protocol-family discriminator decoded from this response.
    #[must_use]
    pub const fn family(&self) -> ProtocolFamily {
        self.family
    }

    /// Construct a terminal error response and bound its UTF-8 diagnostic.
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

    /// Protocol version decoded from this response.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Request ID correlated to this response.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Borrow the typed response result.
    #[must_use]
    pub const fn result(&self) -> &ResponseKind {
        &self.result
    }

    /// Split the response into its version, ID, and result.
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

/// Errors produced by a synchronous primary or recovery fixture client.
#[derive(Debug)]
pub enum ClientError {
    /// The configured per-transaction timeout was zero or exceeded the bound.
    InvalidTimeout {
        /// Rejected timeout.
        timeout: Duration,
        /// Largest accepted timeout.
        maximum: Duration,
    },
    /// The expected library version was not exactly [`REQUIRED_LIBRARY_VERSION`].
    InvalidLibraryVersion,
    /// The expected library digest was not exactly 64 lowercase hex digits.
    InvalidLibrarySha256,
    /// The expected marker was not exactly [`REQUIRED_LIBRARY_MARKER`].
    InvalidLibraryMarker,
    /// Primary facts omitted custom facts or recovery facts included them.
    InvalidCustomPresence {
        /// Role whose custom-image presence was invalid.
        mode: ServerMode,
    },
    /// The expected custom-image digest was not exactly 64 lowercase hex digits.
    InvalidCustomSha256,
    /// The expected custom-image byte count was zero or exceeded the bound.
    InvalidCustomBytes {
        /// Rejected byte count.
        bytes: u64,
    },
    /// The expected stock-image digest differed from [`REQUIRED_STOCK_SHA256`].
    InvalidStockSha256,
    /// The expected stock-image byte count differed from [`REQUIRED_STOCK_BYTES`].
    InvalidStockBytes {
        /// Rejected byte count.
        bytes: u64,
    },
    /// Custom and stock facts claimed the same digest.
    AliasedImageSha256,
    /// The child process could not be started.
    Spawn(io::Error),
    /// A requested child pipe was unexpectedly unavailable.
    MissingPipe {
        /// Name of the unavailable standard-I/O pipe.
        name: &'static str,
    },
    /// The bounded standard-I/O worker could not be started.
    WorkerSpawn(io::Error),
    /// The worker could not accept the only outstanding request.
    Publish {
        /// Identifier of the unpublished request.
        request_id: u64,
    },
    /// A complete write-and-read transaction exceeded its deadline.
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
    /// The child closed stdout before returning a complete response.
    EarlyExit {
        /// Identifier awaiting a response.
        request_id: u64,
        /// Exit status when already observable without blocking.
        status: Option<ExitStatus>,
    },
    /// The standard-I/O worker stopped without returning a response.
    WorkerStopped {
        /// Identifier awaiting a response.
        request_id: u64,
    },
    /// A response violated version, ID, mode, bundle, or state expectations.
    Protocol {
        /// Identifier of the affected request.
        request_id: u64,
        /// Diagnostic describing the mismatch.
        message: String,
    },
    /// The loader server returned its typed terminal error result.
    Server {
        /// Identifier correlated to the error response.
        request_id: u64,
        /// Stable server error class.
        code: ErrorCode,
        /// Bounded server diagnostic.
        message: String,
    },
    /// A client operation was not legal in its last confirmed state.
    InvalidState {
        /// Attempted operation.
        operation: &'static str,
        /// Required state or states.
        expected: &'static str,
        /// Last state confirmed by a complete response.
        actual: LoaderState,
    },
    /// Explicit shutdown produced a non-successful process exit status.
    ChildExit {
        /// Child exit status.
        status: ExitStatus,
    },
    /// Waiting for the child process failed.
    Wait(io::Error),
    /// The standard-I/O worker panicked.
    WorkerPanicked,
    /// The standard-I/O worker did not finish within its cleanup bound.
    WorkerCompletionTimeout {
        /// Time allowed before detaching the worker.
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeout { timeout, maximum } => write!(
                formatter,
                "loader transaction timeout {timeout:?} must be positive and at most {maximum:?}"
            ),
            Self::InvalidLibraryVersion => write!(
                formatter,
                "loader library version must be exactly {REQUIRED_LIBRARY_VERSION}"
            ),
            Self::InvalidLibrarySha256 => formatter.write_str(
                "loader library SHA-256 must be exactly 64 lowercase hexadecimal characters",
            ),
            Self::InvalidLibraryMarker => write!(
                formatter,
                "loader library marker must be exactly 0x{REQUIRED_LIBRARY_MARKER:08x}"
            ),
            Self::InvalidCustomPresence { mode } => write!(
                formatter,
                "loader bundle custom-image presence is incompatible with {mode:?}"
            ),
            Self::InvalidCustomSha256 => formatter.write_str(
                "custom image SHA-256 must be exactly 64 lowercase hexadecimal characters",
            ),
            Self::InvalidCustomBytes { bytes } => write!(
                formatter,
                "custom image byte count {bytes} must be 1 through {MAX_IMAGE_BYTES}"
            ),
            Self::InvalidStockSha256 => formatter
                .write_str("stock image SHA-256 differs from the frozen xA9 v0.16.0 digest"),
            Self::InvalidStockBytes { bytes } => write!(
                formatter,
                "stock image byte count {bytes} differs from required {REQUIRED_STOCK_BYTES}"
            ),
            Self::AliasedImageSha256 => {
                formatter.write_str("custom and stock image SHA-256 values must differ")
            }
            Self::Spawn(error) => write!(formatter, "spawn profile-2 loader fixture: {error}"),
            Self::MissingPipe { name } => {
                write!(
                    formatter,
                    "spawned profile-2 loader fixture has no piped {name}"
                )
            }
            Self::WorkerSpawn(error) => {
                write!(formatter, "spawn profile-2 loader I/O worker: {error}")
            }
            Self::Publish { request_id } => {
                write!(formatter, "loader I/O worker rejected request {request_id}")
            }
            Self::Timeout {
                request_id,
                timeout,
            } => write!(
                formatter,
                "profile-2 loader request {request_id} exceeded {timeout:?}"
            ),
            Self::Transport { request_id, source } => write!(
                formatter,
                "profile-2 loader request {request_id} transport failed: {source}"
            ),
            Self::EarlyExit { request_id, status } => match status {
                Some(status) => write!(
                    formatter,
                    "profile-2 loader exited with {status} before response {request_id}"
                ),
                None => write!(
                    formatter,
                    "profile-2 loader closed output before response {request_id}"
                ),
            },
            Self::WorkerStopped { request_id } => write!(
                formatter,
                "profile-2 loader I/O worker stopped before response {request_id}"
            ),
            Self::Protocol {
                request_id,
                message,
            } => write!(
                formatter,
                "profile-2 loader response {request_id} is incompatible: {message}"
            ),
            Self::Server {
                request_id,
                code,
                message,
            } => write!(
                formatter,
                "profile-2 loader request {request_id} failed ({code:?}): {message}"
            ),
            Self::InvalidState {
                operation,
                expected,
                actual,
            } => write!(
                formatter,
                "cannot {operation} from loader state {actual:?}; required {expected}"
            ),
            Self::ChildExit { status } => {
                write!(formatter, "profile-2 loader shutdown exited with {status}")
            }
            Self::Wait(error) => write!(formatter, "wait for profile-2 loader: {error}"),
            Self::WorkerPanicked => formatter.write_str("profile-2 loader I/O worker panicked"),
            Self::WorkerCompletionTimeout { timeout } => write!(
                formatter,
                "profile-2 loader I/O worker did not stop within {timeout:?}"
            ),
            Self::Terminal => formatter.write_str("profile-2 loader session is terminal"),
            Self::Termination { message } => {
                write!(formatter, "terminate profile-2 loader process: {message}")
            }
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
            | Self::InvalidLibraryVersion
            | Self::InvalidLibrarySha256
            | Self::InvalidLibraryMarker
            | Self::InvalidCustomPresence { .. }
            | Self::InvalidCustomSha256
            | Self::InvalidCustomBytes { .. }
            | Self::InvalidStockSha256
            | Self::InvalidStockBytes { .. }
            | Self::AliasedImageSha256
            | Self::MissingPipe { .. }
            | Self::Publish { .. }
            | Self::Timeout { .. }
            | Self::EarlyExit { .. }
            | Self::WorkerStopped { .. }
            | Self::Protocol { .. }
            | Self::Server { .. }
            | Self::InvalidState { .. }
            | Self::ChildExit { .. }
            | Self::WorkerPanicked
            | Self::WorkerCompletionTimeout { .. }
            | Self::Terminal
            | Self::Termination { .. } => None,
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

enum ExpectedResponse<'a> {
    Hello {
        mode: ServerMode,
        bundle: &'a BundleFacts,
    },
    State(LoaderState),
}

fn validate_response(
    response: Response,
    request_id: u64,
    expected: ExpectedResponse<'_>,
) -> Result<LoaderState, ClientError> {
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
                    "server error diagnostic has {} bytes; maximum is {MAX_ERROR_MESSAGE_BYTES}",
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
                state: LoaderState::Ready,
                bundle,
            },
        ) if mode == expected_mode && &bundle == expected_bundle => Ok(LoaderState::Ready),
        (ExpectedResponse::State(expected_state), ResponseKind::State { state })
            if state == expected_state =>
        {
            Ok(state)
        }
        (expected, actual) => Err(ClientError::Protocol {
            request_id,
            message: format!(
                "result {actual:?} does not match {}",
                match expected {
                    ExpectedResponse::Hello { mode, .. } => match mode {
                        ServerMode::FixturePrimary => {
                            "the exact primary Ready hello and bundle"
                        }
                        ServerMode::FixtureRecovery => {
                            "the exact recovery Ready hello and bundle"
                        }
                    },
                    ExpectedResponse::State(_) => "the exact next loader state",
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

fn exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_bundle(mode: ServerMode, bundle: &BundleFacts) -> Result<(), ClientError> {
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
    match (mode, bundle.custom.as_ref()) {
        (ServerMode::FixturePrimary, Some(custom)) => {
            if !exact_lower_hex(&custom.sha256, 64) {
                return Err(ClientError::InvalidCustomSha256);
            }
            if !(1..=MAX_IMAGE_BYTES).contains(&custom.bytes) {
                return Err(ClientError::InvalidCustomBytes {
                    bytes: custom.bytes,
                });
            }
            if custom.sha256 == bundle.stock.sha256 {
                return Err(ClientError::AliasedImageSha256);
            }
        }
        (ServerMode::FixturePrimary, None) => {
            return Err(ClientError::InvalidCustomPresence { mode });
        }
        (ServerMode::FixtureRecovery, Some(_)) => {
            return Err(ClientError::InvalidCustomPresence { mode });
        }
        (ServerMode::FixtureRecovery, None) => {}
    }
    Ok(())
}

fn loader_worker(mut input: ChildStdin, mut output: ChildStdout, requests: Receiver<WorkerJob>) {
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

/// Shared synchronous child session behind the two role-typed clients.
///
/// One I/O worker owns both pipes, so a transaction deadline covers request
/// publication plus response receipt. Each request marks the session terminal
/// before publication and only an exact correlated response advances and clears
/// that uncertainty. Child stderr is inherited. The supplied executable
/// arguments retain their exact order, while `LD_PRELOAD`, `LD_AUDIT`, and
/// `LD_LIBRARY_PATH` are removed before exec. Unix children run in a private
/// process group so failure, timeout, explicit termination, and Drop can clean
/// up descendants before directly killing and reaping the leader. Linux
/// orderly shutdown observes the leader without reaping before it signals that
/// group. On other targets, a child used with orderly shutdown must not leave
/// descendants because the conservative exit-observation fallback may reap the
/// leader before any group signal.
struct SessionClient {
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
    state: LoaderState,
    terminal: bool,
}

impl fmt::Debug for SessionClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionClient")
            .field("timeout", &self.timeout)
            .field("next_id", &self.next_id)
            .field("state", &self.state)
            .field("terminal", &self.terminal)
            .finish_non_exhaustive()
    }
}

impl SessionClient {
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
        validate_transaction_timeout(transaction_timeout)?;
        validate_bundle(mode, expected_bundle)?;

        let command = build_command(program, arguments);
        let mut client = Self::spawn_unnegotiated(command, transaction_timeout)?;
        client.hello(mode, expected_bundle)?;
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
            .name("profile2-loader-io".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    loader_worker(input, output, request_receiver);
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
            #[cfg(test)]
            cleanup_probe: None,
            timeout: transaction_timeout,
            next_id: 1,
            state: LoaderState::Ready,
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
        if state == LoaderState::Ready {
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
        required: LoaderState,
        next: LoaderState,
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
        if !matches!(self.state, LoaderState::Ready | LoaderState::StockVerified) {
            return self.fail(ClientError::InvalidState {
                operation: "shut down the loader worker",
                expected: "Ready or StockVerified",
                actual: self.state,
            });
        }

        let deadline = Instant::now() + self.timeout;
        let request_id = self.next_id;
        let state = self.exchange_until(
            RequestOperation::Shutdown {},
            ExpectedResponse::State(LoaderState::Closed),
            deadline,
        )?;
        if state != LoaderState::Closed {
            return self.fail(ClientError::Protocol {
                request_id,
                message: format!("shutdown advanced to unexpected state {state:?}"),
            });
        }

        // No request is legal after a completed shutdown response. Preserve
        // terminal state while waiting for process exit and worker completion.
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
    ) -> Result<LoaderState, ClientError> {
        self.exchange_until(operation, expected, Instant::now() + self.timeout)
    }

    fn exchange_until(
        &mut self,
        operation: RequestOperation,
        expected: ExpectedResponse<'_>,
        deadline: Instant,
    ) -> Result<LoaderState, ClientError> {
        if self.terminal {
            return Err(ClientError::Terminal);
        }
        let request_id = self.next_id;

        // Fail closed before publication. A timeout, panic, broken pipe, or
        // uncertain write must never leave the preceding state usable.
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
                // Do not call `try_wait` here: it may reap the leader before
                // terminal cleanup signals its private process group, allowing
                // numeric process-group reuse. Orderly shutdown uses WNOWAIT.
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

impl Drop for SessionClient {
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

const fn state_name(state: LoaderState) -> &'static str {
    match state {
        LoaderState::Ready => "Ready",
        LoaderState::CustomUnverified => "CustomUnverified",
        LoaderState::CustomVerified => "CustomVerified",
        LoaderState::StockUnverified => "StockUnverified",
        LoaderState::StockVerified => "StockVerified",
        LoaderState::Closed => "Closed",
    }
}

/// One synchronous client requiring the declared primary-fixture server mode.
///
/// [`PrimaryFixtureClient::spawn`] preserves the caller-supplied argument order,
/// removes `LD_PRELOAD`, `LD_AUDIT`, and `LD_LIBRARY_PATH`, starts the child in
/// a private process group, inherits stderr, and immediately completes an exact
/// primary Hello and bundle attestation. Any method error makes the session
/// terminal and attempts kill/reap. [`Self::terminate`] is process-only and
/// idempotent; Drop uses the same best-effort cleanup.
///
/// The Hello attests a protocol mode and value-only bundle; it cannot prove that
/// an arbitrary caller-selected executable is CPU-only. The caller must select
/// the trusted fixture command intended for this protocol.
pub struct PrimaryFixtureClient {
    session: SessionClient,
}

impl fmt::Debug for PrimaryFixtureClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrimaryFixtureClient")
            .field("timeout", &self.session.timeout)
            .field("next_id", &self.session.next_id)
            .field("state", &self.session.state)
            .field("terminal", &self.session.terminal)
            .finish_non_exhaustive()
    }
}

impl PrimaryFixtureClient {
    /// Spawn a supplied executable with supplied arguments and negotiate an
    /// exact primary fixture and bundle before returning.
    ///
    /// Arguments are appended without injecting or reordering values. One
    /// deadline covers each complete request/response. Direct kill/wait cleanup
    /// can extend beyond it during operating-system teardown; I/O-worker
    /// completion has a fixed bound.
    ///
    /// # Errors
    ///
    /// Returns a validation error before spawn for an invalid timeout or bundle.
    /// After spawn, any error terminalizes the child and attempts kill/reap.
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
        let session = SessionClient::spawn(
            program,
            arguments,
            ServerMode::FixturePrimary,
            &expected_bundle,
            transaction_timeout,
        )?;
        Ok(Self { session })
    }

    /// Last state established by an exact complete response.
    ///
    /// After failure, physical state may have advanced beyond this confirmed
    /// value; consult [`Self::is_terminal`].
    #[must_use]
    pub const fn state(&self) -> LoaderState {
        self.session.state
    }

    /// Whether the client is closed or an error made further use unsafe.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.session.terminal
    }

    /// Dispatch the sealed custom profile-2 image.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn load_profile2(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "load the custom profile-2 image",
            RequestOperation::LoadProfile2 {},
            LoaderState::Ready,
            LoaderState::CustomUnverified,
        )
    }

    /// Verify exact profile identity after custom dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn verify_profile2(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "verify the custom profile-2 image",
            RequestOperation::VerifyProfile2 {},
            LoaderState::CustomUnverified,
            LoaderState::CustomVerified,
        )
    }

    /// Dispatch stock on the same verified primary worker.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn restore_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "restore the stock image",
            RequestOperation::RestoreStock {},
            LoaderState::CustomVerified,
            LoaderState::StockUnverified,
        )
    }

    /// Verify exact stock identity after same-worker restoration.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn verify_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "verify the stock image",
            RequestOperation::VerifyStock {},
            LoaderState::StockUnverified,
            LoaderState::StockVerified,
        )
    }

    /// Orderly close an untouched or stock-verified worker and reap the child.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state, transaction, exit, wait, or
    /// cleanup failure. Any error leaves the client terminal.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.session.shutdown()
    }

    /// Signal the process group and reap its leader without sending a request.
    ///
    /// Retry this operation if process or worker cleanup remains unconfirmed.
    /// It performs no loader, FPGA, RF, or device operation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Termination`] if process cleanup reported failure.
    pub fn terminate(&mut self) -> Result<(), ClientError> {
        self.session.terminate()
    }
}

/// One synchronous client requiring the declared recovery-fixture server mode.
///
/// Construction and cleanup have the same deadline, process-group, inherited-
/// stderr, loader-environment, immediate-Hello, and terminal-error behavior as
/// [`PrimaryFixtureClient`]. This type exposes no custom-image operation.
pub struct RecoveryFixtureClient {
    session: SessionClient,
}

impl fmt::Debug for RecoveryFixtureClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveryFixtureClient")
            .field("timeout", &self.session.timeout)
            .field("next_id", &self.session.next_id)
            .field("state", &self.session.state)
            .field("terminal", &self.session.terminal)
            .finish_non_exhaustive()
    }
}

impl RecoveryFixtureClient {
    /// Spawn a supplied executable with supplied arguments and negotiate an
    /// exact recovery fixture and bundle before returning.
    ///
    /// Arguments are preserved exactly. Stdin/stdout become private pipes,
    /// stderr is inherited, and common loader-injection environment variables
    /// are removed before exec.
    ///
    /// # Errors
    ///
    /// Returns a validation error before spawn for an invalid timeout or bundle.
    /// After spawn, any error terminalizes the child and attempts kill/reap.
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
        let session = SessionClient::spawn(
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
    pub const fn state(&self) -> LoaderState {
        self.session.state
    }

    /// Whether the client is closed or an error made further use unsafe.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.session.terminal
    }

    /// Dispatch the independently retained sealed stock image.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn restore_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "restore the stock image in a fresh recovery worker",
            RequestOperation::RestoreStock {},
            LoaderState::Ready,
            LoaderState::StockUnverified,
        )
    }

    /// Verify exact stock identity after recovery dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state or any transaction failure.
    pub fn verify_stock(&mut self) -> Result<(), ClientError> {
        self.session.transition(
            "verify the stock image in a fresh recovery worker",
            RequestOperation::VerifyStock {},
            LoaderState::StockUnverified,
            LoaderState::StockVerified,
        )
    }

    /// Orderly close an untouched or stock-verified worker and reap the child.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] for invalid state, transaction, exit, wait, or
    /// cleanup failure. Any error leaves the client terminal.
    pub fn shutdown(&mut self) -> Result<(), ClientError> {
        self.session.shutdown()
    }

    /// Signal the process group and reap its leader without sending a request.
    ///
    /// Retry this operation if process or worker cleanup remains unconfirmed.
    /// It performs no loader, FPGA, RF, or device operation.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Termination`] if process cleanup reported failure.
    pub fn terminate(&mut self) -> Result<(), ClientError> {
        self.session.terminate()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde::Deserialize;

    use super::*;

    fn library_facts() -> LibraryFacts {
        LibraryFacts::new(
            REQUIRED_LIBRARY_VERSION,
            "1".repeat(64),
            REQUIRED_LIBRARY_MARKER,
        )
    }

    fn custom_facts() -> ImageFacts {
        ImageFacts::new("2".repeat(64), 12_858_972)
    }

    fn stock_facts() -> ImageFacts {
        ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES)
    }

    fn primary_bundle() -> BundleFacts {
        BundleFacts::new(library_facts(), Some(custom_facts()), stock_facts())
    }

    fn recovery_bundle() -> BundleFacts {
        BundleFacts::new(library_facts(), None, stock_facts())
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
        let mut client = SessionClient {
            child: None,
            requests: None,
            worker: Some(worker),
            worker_completion: Some(completion),
            process_group: 0,
            process_group_active: false,
            cleanup_probe: None,
            timeout: Duration::from_secs(1),
            next_id: 1,
            state: LoaderState::Ready,
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

    fn framed<T: Serialize + ?Sized>(value: &T) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, value).unwrap();
        bytes
    }

    fn primary_requests() -> Vec<Request> {
        vec![
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                },
            ),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(4, RequestOperation::RestoreStock {}),
            Request::new(5, RequestOperation::VerifyStock {}),
            Request::new(6, RequestOperation::Shutdown {}),
        ]
    }

    fn primary_responses() -> Vec<Response> {
        vec![
            Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: LoaderState::Ready,
                    bundle: primary_bundle(),
                },
            ),
            Response::new(
                2,
                ResponseKind::State {
                    state: LoaderState::CustomUnverified,
                },
            ),
            Response::new(
                3,
                ResponseKind::State {
                    state: LoaderState::CustomVerified,
                },
            ),
            Response::new(
                4,
                ResponseKind::State {
                    state: LoaderState::StockUnverified,
                },
            ),
            Response::new(
                5,
                ResponseKind::State {
                    state: LoaderState::StockVerified,
                },
            ),
            Response::new(
                6,
                ResponseKind::State {
                    state: LoaderState::Closed,
                },
            ),
        ]
    }

    fn recovery_requests() -> Vec<Request> {
        vec![
            Request::new(
                1,
                RequestOperation::Hello {
                    mode: ServerMode::FixtureRecovery,
                },
            ),
            Request::new(2, RequestOperation::RestoreStock {}),
            Request::new(3, RequestOperation::VerifyStock {}),
            Request::new(4, RequestOperation::Shutdown {}),
        ]
    }

    fn recovery_responses() -> Vec<Response> {
        vec![
            Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: LoaderState::Ready,
                    bundle: recovery_bundle(),
                },
            ),
            Response::new(
                2,
                ResponseKind::State {
                    state: LoaderState::StockUnverified,
                },
            ),
            Response::new(
                3,
                ResponseKind::State {
                    state: LoaderState::StockVerified,
                },
            ),
            Response::new(
                4,
                ResponseKind::State {
                    state: LoaderState::Closed,
                },
            ),
        ]
    }

    #[test]
    fn frame_round_trip_preserves_requests_responses_and_clean_eof() {
        for request in primary_requests().into_iter().chain(recovery_requests()) {
            let bytes = framed(&request);
            let mut input = bytes.as_slice();
            assert_eq!(read_frame::<_, Request>(&mut input).unwrap(), Some(request));
            assert!(read_frame::<_, Request>(&mut input).unwrap().is_none());
        }
        for response in primary_responses()
            .into_iter()
            .chain(recovery_responses())
            .chain([Response::error(7, ErrorCode::Backend, "failed")])
        {
            let bytes = framed(&response);
            let mut input = bytes.as_slice();
            assert_eq!(
                read_frame::<_, Response>(&mut input).unwrap(),
                Some(response)
            );
            assert!(read_frame::<_, Response>(&mut input).unwrap().is_none());
        }
    }

    #[test]
    fn protocol_family_and_schema_spellings_are_frozen() {
        let hello = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        );
        assert_eq!(hello.family(), PROTOCOL_FAMILY);
        assert_eq!(hello.version(), PROTOCOL_VERSION);
        assert_eq!(hello.id(), 1);
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#
        );
        let (family, version, id, operation) = hello.into_parts();
        assert_eq!(family, PROTOCOL_FAMILY);
        assert_eq!((version, id), (PROTOCOL_VERSION, 1));
        assert!(matches!(
            operation,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary
            }
        ));

        let hello_response = Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixtureRecovery,
                state: LoaderState::Ready,
                bundle: BundleFacts::new(
                    LibraryFacts::new("2.6.0", "a", 1),
                    None,
                    ImageFacts::new("b", 2),
                ),
            },
        );
        assert_eq!(
            serde_json::to_string(&hello_response).unwrap(),
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"result":{"kind":"hello","mode":"fixture_recovery","state":"ready","bundle":{"library":{"version":"2.6.0","sha256":"a","marker":1},"custom":null,"stock":{"sha256":"b","bytes":2}}}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::new(
                2,
                ResponseKind::State {
                    state: LoaderState::StockUnverified,
                },
            ))
            .unwrap(),
            r#"{"family":"bladerf_profile2_loader","version":1,"id":2,"result":{"kind":"state","state":"stock_unverified"}}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::error(3, ErrorCode::Backend, "failed")).unwrap(),
            r#"{"family":"bladerf_profile2_loader","version":1,"id":3,"result":{"kind":"error","code":"backend","message":"failed"}}"#
        );

        let operations = [
            (
                RequestOperation::LoadProfile2 {},
                r#"{"kind":"load_profile2"}"#,
            ),
            (
                RequestOperation::VerifyProfile2 {},
                r#"{"kind":"verify_profile2"}"#,
            ),
            (
                RequestOperation::RestoreStock {},
                r#"{"kind":"restore_stock"}"#,
            ),
            (
                RequestOperation::VerifyStock {},
                r#"{"kind":"verify_stock"}"#,
            ),
            (RequestOperation::Shutdown {}, r#"{"kind":"shutdown"}"#),
        ];
        for (operation, expected) in operations {
            assert_eq!(serde_json::to_string(&operation).unwrap(), expected);
        }

        for (mode, spelling, maximum) in [
            (
                ServerMode::FixturePrimary,
                r#""fixture_primary""#,
                MAX_PRIMARY_SESSION_REQUESTS,
            ),
            (
                ServerMode::FixtureRecovery,
                r#""fixture_recovery""#,
                MAX_RECOVERY_SESSION_REQUESTS,
            ),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), spelling);
            assert_eq!(mode.max_session_requests(), maximum);
        }

        for (state, spelling) in [
            (LoaderState::Ready, r#""ready""#),
            (LoaderState::CustomUnverified, r#""custom_unverified""#),
            (LoaderState::CustomVerified, r#""custom_verified""#),
            (LoaderState::StockUnverified, r#""stock_unverified""#),
            (LoaderState::StockVerified, r#""stock_verified""#),
            (LoaderState::Closed, r#""closed""#),
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
    fn family_is_required_and_cross_family_envelopes_are_rejected() {
        let register_style = br#"{"version":1,"id":1,"operation":{"kind":"hello"}}"#;
        assert!(serde_json::from_slice::<Request>(register_style).is_err());

        let wrong_family = br#"{"family":"register","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#;
        assert!(serde_json::from_slice::<Request>(wrong_family).is_err());

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RegisterEnvelope {
            #[allow(
                dead_code,
                reason = "field participates in Serde test-envelope validation"
            )]
            version: u16,
            #[allow(
                dead_code,
                reason = "field participates in Serde test-envelope validation"
            )]
            id: u64,
            #[allow(
                dead_code,
                reason = "field participates in Serde test-envelope validation"
            )]
            operation: serde_json::Value,
        }

        let loader = serde_json::to_vec(&Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        ))
        .unwrap();
        assert!(serde_json::from_slice::<RegisterEnvelope>(&loader).is_err());

        let missing_response_family =
            br#"{"version":1,"id":1,"result":{"kind":"state","state":"ready"}}"#;
        assert!(serde_json::from_slice::<Response>(missing_response_family).is_err());
    }

    #[test]
    fn facts_are_value_only_and_accessors_preserve_values() {
        let bundle = primary_bundle();
        assert_eq!(bundle.library().version(), REQUIRED_LIBRARY_VERSION);
        assert_eq!(bundle.library().sha256(), "1".repeat(64));
        assert_eq!(bundle.library().marker(), REQUIRED_LIBRARY_MARKER);
        assert_eq!(bundle.custom().unwrap().sha256(), "2".repeat(64));
        assert_eq!(bundle.custom().unwrap().bytes(), 12_858_972);
        assert_eq!(bundle.stock().sha256(), REQUIRED_STOCK_SHA256);
        assert_eq!(bundle.stock().bytes(), REQUIRED_STOCK_BYTES);
        let (library, custom, stock) = bundle.into_parts();
        assert_eq!(library.version(), REQUIRED_LIBRARY_VERSION);
        assert!(custom.is_some());
        assert_eq!(stock.bytes(), REQUIRED_STOCK_BYTES);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_schema_layer() {
        for json in [
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"extra":0,"operation":{"kind":"shutdown"}}"#,
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"shutdown","extra":0}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(json).is_err());
        }
        for json in [
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"result":{"kind":"state","state":"ready","extra":0}}"#,
            r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"result":{"kind":"hello","mode":"fixture_recovery","state":"ready","bundle":{"library":{"version":"2.6.0","sha256":"x","marker":1,"extra":0},"custom":null,"stock":{"sha256":"y","bytes":1}}}}"#,
        ] {
            assert!(serde_json::from_str::<Response>(json).is_err());
        }
        assert!(
            serde_json::from_str::<LibraryFacts>(
                r#"{"version":"2.6.0","sha256":"x","marker":1,"extra":0}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ImageFacts>(r#"{"sha256":"x","bytes":1,"extra":0}"#).is_err()
        );
    }

    #[test]
    fn bounded_error_constructor_is_utf8_safe() {
        let message = "é".repeat(MAX_ERROR_MESSAGE_BYTES);
        let response = Response::error(1, ErrorCode::Backend, message);
        let ResponseKind::Error { message, .. } = response.result() else {
            panic!("expected error response");
        };
        assert_eq!(message.len(), MAX_ERROR_MESSAGE_BYTES - 4 + 3);
        assert!(message.ends_with("..."));
        assert!(message.is_char_boundary(message.len()));
    }

    #[test]
    fn response_validation_requires_exact_family_version_id_mode_bundle_and_state() {
        let bundle = primary_bundle();
        let expected = || ExpectedResponse::Hello {
            mode: ServerMode::FixturePrimary,
            bundle: &bundle,
        };
        assert_eq!(
            validate_response(
                Response::new(
                    7,
                    ResponseKind::Hello {
                        mode: ServerMode::FixturePrimary,
                        state: LoaderState::Ready,
                        bundle: bundle.clone(),
                    },
                ),
                7,
                expected(),
            )
            .unwrap(),
            LoaderState::Ready
        );

        let mut wrong_version = Response::new(
            7,
            ResponseKind::State {
                state: LoaderState::Ready,
            },
        );
        wrong_version.version += 1;
        assert!(matches!(
            validate_response(
                wrong_version,
                7,
                ExpectedResponse::State(LoaderState::Ready)
            ),
            Err(ClientError::Protocol { .. })
        ));
        assert!(matches!(
            validate_response(
                Response::new(
                    8,
                    ResponseKind::State {
                        state: LoaderState::Ready
                    }
                ),
                7,
                ExpectedResponse::State(LoaderState::Ready)
            ),
            Err(ClientError::Protocol { .. })
        ));
        assert!(matches!(
            validate_response(
                Response::new(
                    7,
                    ResponseKind::Hello {
                        mode: ServerMode::FixtureRecovery,
                        state: LoaderState::Ready,
                        bundle: recovery_bundle(),
                    }
                ),
                7,
                expected()
            ),
            Err(ClientError::Protocol { .. })
        ));
        let mut wrong_bundle = bundle.clone();
        wrong_bundle.library.sha256 = "3".repeat(64);
        assert!(matches!(
            validate_response(
                Response::new(
                    7,
                    ResponseKind::Hello {
                        mode: ServerMode::FixturePrimary,
                        state: LoaderState::Ready,
                        bundle: wrong_bundle,
                    }
                ),
                7,
                expected()
            ),
            Err(ClientError::Protocol { .. })
        ));
        assert!(matches!(
            validate_response(
                Response::new(
                    7,
                    ResponseKind::State {
                        state: LoaderState::StockVerified
                    }
                ),
                7,
                ExpectedResponse::State(LoaderState::StockUnverified)
            ),
            Err(ClientError::Protocol { .. })
        ));
    }

    #[test]
    fn overlong_decoded_error_is_protocol_failure() {
        let response = Response {
            family: PROTOCOL_FAMILY,
            version: PROTOCOL_VERSION,
            id: 3,
            result: ResponseKind::Error {
                code: ErrorCode::Backend,
                message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
            },
        };
        assert!(matches!(
            validate_response(response, 3, ExpectedResponse::State(LoaderState::Ready)),
            Err(ClientError::Protocol { .. })
        ));
    }

    #[test]
    fn framing_rejects_empty_truncated_oversized_and_malformed_values() {
        assert!(
            read_frame::<_, Request>(&mut [].as_slice())
                .unwrap()
                .is_none()
        );
        for received in 1..FRAME_HEADER_BYTES {
            let mut bytes = [0u8; FRAME_HEADER_BYTES];
            bytes[0] = 1;
            assert!(matches!(
                read_frame::<_, Request>(&mut bytes[..received].as_ref()),
                Err(FrameError::TruncatedHeader { received: actual }) if actual == received
            ));
        }
        assert!(matches!(
            read_frame::<_, Request>(&mut 0u32.to_le_bytes().as_slice()),
            Err(FrameError::EmptyFrame)
        ));
        let oversized = (MAX_FRAME_BYTES as u32 + 1).to_le_bytes();
        assert!(matches!(
            read_frame::<_, Request>(&mut oversized.as_slice()),
            Err(FrameError::FrameTooLarge {
                maximum: MAX_FRAME_BYTES,
                ..
            })
        ));
        let mut truncated = Vec::from(10u32.to_le_bytes());
        truncated.extend_from_slice(b"{}");
        assert!(matches!(
            read_frame::<_, Request>(&mut truncated.as_slice()),
            Err(FrameError::TruncatedPayload {
                expected: 10,
                received: 2
            })
        ));
        let mut malformed = Vec::from(1u32.to_le_bytes());
        malformed.push(b'{');
        assert!(matches!(
            read_frame::<_, Request>(&mut malformed.as_slice()),
            Err(FrameError::Deserialize(_))
        ));

        let mut output = Vec::new();
        let too_large = "x".repeat(MAX_FRAME_BYTES);
        assert!(matches!(
            write_frame(&mut output, &too_large),
            Err(FrameError::FrameTooLarge {
                maximum: MAX_FRAME_BYTES,
                ..
            })
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn invalid_timeout_and_each_bundle_invariant_fail_before_spawn() {
        let missing = "/this/program/must/not/be/spawned";
        for timeout in [
            Duration::ZERO,
            MAX_TRANSACTION_TIMEOUT + Duration::from_nanos(1),
        ] {
            assert!(matches!(
                PrimaryFixtureClient::spawn(missing, [] as [&str; 0], primary_bundle(), timeout),
                Err(ClientError::InvalidTimeout { .. })
            ));
        }

        let mut value = primary_bundle();
        value.library.version = "2.6.1".to_owned();
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidLibraryVersion)
        ));
        let mut value = primary_bundle();
        value.library.sha256 = "A".repeat(64);
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidLibrarySha256)
        ));
        let mut value = primary_bundle();
        value.library.marker = 0;
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidLibraryMarker)
        ));
        let mut value = primary_bundle();
        value.custom = None;
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidCustomPresence {
                mode: ServerMode::FixturePrimary
            })
        ));
        let mut value = primary_bundle();
        value.custom.as_mut().unwrap().sha256 = "bad".to_owned();
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidCustomSha256)
        ));
        let mut value = primary_bundle();
        value.custom.as_mut().unwrap().bytes = 0;
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidCustomBytes { bytes: 0 })
        ));
        let mut value = primary_bundle();
        value.custom.as_mut().unwrap().bytes = MAX_IMAGE_BYTES + 1;
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidCustomBytes { bytes }) if bytes == MAX_IMAGE_BYTES + 1
        ));
        let mut value = primary_bundle();
        value.stock.sha256 = "3".repeat(64);
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidStockSha256)
        ));
        let mut value = primary_bundle();
        value.stock.bytes += 1;
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::InvalidStockBytes { bytes }) if bytes == REQUIRED_STOCK_BYTES + 1
        ));
        let mut value = primary_bundle();
        value.custom.as_mut().unwrap().sha256 = REQUIRED_STOCK_SHA256.to_owned();
        assert!(matches!(
            PrimaryFixtureClient::spawn(missing, [] as [&str; 0], value, Duration::from_secs(1)),
            Err(ClientError::AliasedImageSha256)
        ));

        let error = RecoveryFixtureClient::spawn(
            missing,
            [] as [&str; 0],
            primary_bundle(),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ClientError::InvalidCustomPresence {
                mode: ServerMode::FixtureRecovery
            }
        ));
    }

    #[test]
    fn command_builder_preserves_arguments_and_removes_loader_injection_environment() {
        let command = build_command(
            "/sentinel/loader",
            ["loader-fixture-primary", "two words", "--literal"],
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["loader-fixture-primary", "two words", "--literal"]
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
            let script_path = directory.path().join("loader-child.sh");
            let program = PathBuf::from("/bin/sh");
            let mut script = String::from("#!/bin/sh\nset -eu\n");
            let mut arguments = vec![script_path.clone()];
            let mut captures = Vec::new();
            for (index, (request, response)) in requests.iter().zip(responses).enumerate() {
                let response_path = directory.path().join(format!("response-{index}.bin"));
                let capture_path = directory.path().join(format!("request-{index}.bin"));
                fs::write(&response_path, framed(response)).unwrap();
                let request_bytes = framed(request).len();
                script.push_str(&format!(
                    "dd bs=1 count={request_bytes} of=\"$2\" 2>/dev/null\ndd if=\"$1\" bs=1 2>/dev/null\nshift 2\n"
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
    fn primary_client_runs_exact_six_request_lifecycle() {
        let requests = primary_requests();
        let responses = primary_responses();
        let child = ScriptedChild::new(&requests, &responses, "exit 0");
        let mut client = PrimaryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            primary_bundle(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(client.state(), LoaderState::Ready);
        assert!(!client.is_terminal());
        client.load_profile2().unwrap();
        assert_eq!(client.state(), LoaderState::CustomUnverified);
        client.verify_profile2().unwrap();
        assert_eq!(client.state(), LoaderState::CustomVerified);
        client.restore_stock().unwrap();
        assert_eq!(client.state(), LoaderState::StockUnverified);
        client.verify_stock().unwrap();
        assert_eq!(client.state(), LoaderState::StockVerified);
        client.shutdown().unwrap();
        assert_eq!(client.state(), LoaderState::Closed);
        assert!(client.is_terminal());
        assert!(matches!(client.shutdown(), Err(ClientError::Terminal)));
        client.terminate().unwrap();
        client.terminate().unwrap();
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_client_runs_exact_four_request_lifecycle() {
        let requests = recovery_requests();
        let responses = recovery_responses();
        let child = ScriptedChild::new(&requests, &responses, "exit 0");
        let mut client = RecoveryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            recovery_bundle(),
            Duration::from_secs(2),
        )
        .unwrap();
        client.restore_stock().unwrap();
        client.verify_stock().unwrap();
        client.shutdown().unwrap();
        assert_eq!(client.state(), LoaderState::Closed);
        assert!(client.is_terminal());
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn ready_shutdown_is_legal_for_both_roles() {
        for (mode, bundle) in [
            (ServerMode::FixturePrimary, primary_bundle()),
            (ServerMode::FixtureRecovery, recovery_bundle()),
        ] {
            let requests = vec![
                Request::new(1, RequestOperation::Hello { mode }),
                Request::new(2, RequestOperation::Shutdown {}),
            ];
            let responses = vec![
                Response::new(
                    1,
                    ResponseKind::Hello {
                        mode,
                        state: LoaderState::Ready,
                        bundle: bundle.clone(),
                    },
                ),
                Response::new(
                    2,
                    ResponseKind::State {
                        state: LoaderState::Closed,
                    },
                ),
            ];
            let child = ScriptedChild::new(&requests, &responses, "exit 0");
            match mode {
                ServerMode::FixturePrimary => {
                    let mut client = PrimaryFixtureClient::spawn(
                        &child.program,
                        path_arguments(&child.arguments),
                        bundle,
                        Duration::from_secs(2),
                    )
                    .unwrap();
                    client.shutdown().unwrap();
                }
                ServerMode::FixtureRecovery => {
                    let mut client = RecoveryFixtureClient::spawn(
                        &child.program,
                        path_arguments(&child.arguments),
                        bundle,
                        Duration::from_secs(2),
                    )
                    .unwrap();
                    client.shutdown().unwrap();
                }
            }
            child.assert_requests(&requests);
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_invalid_state_is_terminal_and_publishes_no_request() {
        let requests = vec![Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        )];
        let responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixturePrimary,
                state: LoaderState::Ready,
                bundle: primary_bundle(),
            },
        )];
        let child = ScriptedChild::new(&requests, &responses, "sleep 10");
        let mut client = PrimaryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            primary_bundle(),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(matches!(
            client.verify_profile2(),
            Err(ClientError::InvalidState { .. })
        ));
        assert!(client.is_terminal());
        assert!(matches!(client.load_profile2(), Err(ClientError::Terminal)));
        client.terminate().unwrap();
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn every_lost_primary_transition_response_preserves_only_last_confirmed_state() {
        let all_requests = primary_requests();
        let all_responses = primary_responses();
        let confirmed_states = [
            LoaderState::Ready,
            LoaderState::CustomUnverified,
            LoaderState::CustomVerified,
            LoaderState::StockUnverified,
        ];

        for uncertain_index in 1..=4 {
            let uncertain_request = &all_requests[uncertain_index];
            let suffix = format!(
                "dd bs=1 count={} of=\"$1\" 2>/dev/null\nsleep 10",
                framed(uncertain_request).len()
            );
            let mut child = ScriptedChild::new(
                &all_requests[..uncertain_index],
                &all_responses[..uncertain_index],
                &suffix,
            );
            let uncertain_capture = child
                ._directory
                .path()
                .join(format!("uncertain-{uncertain_index}.bin"));
            child.arguments.push(uncertain_capture.clone());
            child.captures.push(uncertain_capture);

            let mut client = PrimaryFixtureClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                primary_bundle(),
                Duration::from_millis(200),
            )
            .unwrap();
            if uncertain_index > 1 {
                client.load_profile2().unwrap();
            }
            if uncertain_index > 2 {
                client.verify_profile2().unwrap();
            }
            if uncertain_index > 3 {
                client.restore_stock().unwrap();
            }

            let error = match uncertain_index {
                1 => client.load_profile2().unwrap_err(),
                2 => client.verify_profile2().unwrap_err(),
                3 => client.restore_stock().unwrap_err(),
                4 => client.verify_stock().unwrap_err(),
                _ => unreachable!(),
            };
            assert!(
                matches!(
                    primary_error(&error),
                    ClientError::Timeout { request_id, .. }
                        if *request_id == uncertain_index as u64 + 1
                ),
                "stage {uncertain_index}: {error:?}"
            );
            assert_eq!(client.state(), confirmed_states[uncertain_index - 1]);
            assert!(client.is_terminal());
            assert!(matches!(client.terminate(), Ok(())));
            let later = match uncertain_index {
                1 => client.load_profile2(),
                2 => client.verify_profile2(),
                3 => client.restore_stock(),
                4 => client.verify_stock(),
                _ => unreachable!(),
            };
            assert!(matches!(later, Err(ClientError::Terminal)));
            child.assert_requests(&all_requests[..=uncertain_index]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn lost_recovery_transition_response_is_terminal_without_state_advance() {
        let hello = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        );
        let restore = Request::new(2, RequestOperation::RestoreStock {});
        let suffix = format!(
            "dd bs=1 count={} of=\"$1\" 2>/dev/null\nsleep 10",
            framed(&restore).len()
        );
        let mut child = ScriptedChild::new(
            std::slice::from_ref(&hello),
            &[Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: LoaderState::Ready,
                    bundle: recovery_bundle(),
                },
            )],
            &suffix,
        );
        let capture = child._directory.path().join("lost-restore.bin");
        child.arguments.push(capture.clone());
        child.captures.push(capture);

        let mut client = RecoveryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            recovery_bundle(),
            Duration::from_millis(200),
        )
        .unwrap();
        let error = client.restore_stock().unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Timeout { request_id: 2, .. }
        ));
        assert_eq!(client.state(), LoaderState::Ready);
        assert!(client.is_terminal());
        assert!(matches!(client.verify_stock(), Err(ClientError::Terminal)));
        child.assert_requests(&[hello, restore]);
    }

    #[cfg(unix)]
    #[test]
    fn lost_shutdown_response_is_terminal_without_closed_state() {
        let hello = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        );
        let shutdown = Request::new(2, RequestOperation::Shutdown {});
        let suffix = format!(
            "dd bs=1 count={} of=\"$1\" 2>/dev/null\nsleep 10",
            framed(&shutdown).len()
        );
        let mut child = ScriptedChild::new(
            std::slice::from_ref(&hello),
            &[Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: LoaderState::Ready,
                    bundle: recovery_bundle(),
                },
            )],
            &suffix,
        );
        let capture = child._directory.path().join("lost-shutdown.bin");
        child.arguments.push(capture.clone());
        child.captures.push(capture);

        let mut client = RecoveryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            recovery_bundle(),
            Duration::from_millis(200),
        )
        .unwrap();
        let error = client.shutdown().unwrap_err();
        assert!(matches!(
            primary_error(&error),
            ClientError::Timeout { request_id: 2, .. }
        ));
        assert_eq!(client.state(), LoaderState::Ready);
        assert!(client.is_terminal());
        child.assert_requests(&[hello, shutdown]);
    }

    #[cfg(unix)]
    #[test]
    fn live_process_termination_is_process_only_and_idempotent() {
        let requests = vec![Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        )];
        let responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixtureRecovery,
                state: LoaderState::Ready,
                bundle: recovery_bundle(),
            },
        )];
        let child = ScriptedChild::new(&requests, &responses, "sleep 10");
        let mut client = RecoveryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            recovery_bundle(),
            Duration::from_secs(1),
        )
        .unwrap();
        client.terminate().unwrap();
        client.terminate().unwrap();
        assert!(client.is_terminal());
        assert_eq!(client.state(), LoaderState::Ready);
        assert!(matches!(client.restore_stock(), Err(ClientError::Terminal)));
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_exit_deadline_is_terminal_and_bounded() {
        let mode = ServerMode::FixtureRecovery;
        let bundle = recovery_bundle();
        let requests = vec![
            Request::new(1, RequestOperation::Hello { mode }),
            Request::new(2, RequestOperation::Shutdown {}),
        ];
        let responses = vec![
            Response::new(
                1,
                ResponseKind::Hello {
                    mode,
                    state: LoaderState::Ready,
                    bundle: bundle.clone(),
                },
            ),
            Response::new(
                2,
                ResponseKind::State {
                    state: LoaderState::Closed,
                },
            ),
        ];
        let child = ScriptedChild::new(&requests, &responses, "sleep 10");
        let mut client = RecoveryFixtureClient::spawn(
            &child.program,
            path_arguments(&child.arguments),
            bundle,
            Duration::from_millis(200),
        )
        .unwrap();
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
        assert_eq!(client.state(), LoaderState::Closed);
        assert!(client.is_terminal());
        assert!(matches!(client.shutdown(), Err(ClientError::Terminal)));
        child.assert_requests(&requests);
    }

    #[cfg(unix)]
    #[test]
    fn leader_exit_with_inherited_pipe_descendant_is_bounded_and_group_cleaned() {
        let command = build_command("sh", ["-c", "sleep 30 &"]);
        let mut client =
            SessionClient::spawn_unnegotiated(command, Duration::from_millis(100)).unwrap();
        let cleanup = client.install_cleanup_probe();
        let bundle = primary_bundle();
        let started = Instant::now();
        let error = client
            .hello(ServerMode::FixturePrimary, &bundle)
            .unwrap_err();
        assert!(
            matches!(
                primary_error(&error),
                ClientError::Timeout { request_id: 1, .. }
            ),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(client.terminal);
        let report = cleanup.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(report.group_signal_attempted, "{report:?}");
        assert!(report.leader_reaped, "{report:?}");
        assert!(report.worker_wait_completed, "{report:?}");
        assert!(report.diagnostic.is_none(), "{report:?}");
    }

    #[cfg(unix)]
    #[test]
    fn wrong_bundle_and_overlong_error_terminate_during_hello() {
        let request = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
        );
        let mut wrong = primary_bundle();
        wrong.library.sha256 = "3".repeat(64);
        let child = ScriptedChild::new(
            std::slice::from_ref(&request),
            &[Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixturePrimary,
                    state: LoaderState::Ready,
                    bundle: wrong,
                },
            )],
            "sleep 10",
        );
        assert!(matches!(
            PrimaryFixtureClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                primary_bundle(),
                Duration::from_secs(2)
            ),
            Err(ClientError::Protocol { .. })
        ));

        let child = ScriptedChild::new(
            std::slice::from_ref(&request),
            &[Response {
                family: PROTOCOL_FAMILY,
                version: PROTOCOL_VERSION,
                id: 1,
                result: ResponseKind::Error {
                    code: ErrorCode::Backend,
                    message: "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1),
                },
            }],
            "sleep 10",
        );
        assert!(matches!(
            PrimaryFixtureClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                primary_bundle(),
                Duration::from_secs(2)
            ),
            Err(ClientError::Protocol { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_early_exit_malformed_output_and_drop_are_bounded() {
        let start = Instant::now();
        assert!(matches!(
            PrimaryFixtureClient::spawn(
                "sleep",
                ["10"],
                primary_bundle(),
                Duration::from_millis(50)
            ),
            Err(ClientError::Timeout { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));
        assert!(start.elapsed() < Duration::from_secs(2));

        assert!(matches!(
            PrimaryFixtureClient::spawn(
                "true",
                [] as [&str; 0],
                primary_bundle(),
                Duration::from_millis(250)
            ),
            Err(ClientError::EarlyExit { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));

        assert!(matches!(
            PrimaryFixtureClient::spawn(
                "sh",
                ["-c", "printf '\\001\\000'"],
                primary_bundle(),
                Duration::from_millis(250)
            ),
            Err(ClientError::Transport { request_id: 1, .. }) | Err(ClientError::Cleanup { .. })
        ));

        let requests = vec![Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        )];
        let responses = vec![Response::new(
            1,
            ResponseKind::Hello {
                mode: ServerMode::FixtureRecovery,
                state: LoaderState::Ready,
                bundle: recovery_bundle(),
            },
        )];
        let child = ScriptedChild::new(&requests, &responses, "sleep 10");
        let start = Instant::now();
        {
            let _client = RecoveryFixtureClient::spawn(
                &child.program,
                path_arguments(&child.arguments),
                recovery_bundle(),
                Duration::from_secs(1),
            )
            .unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn supplied_arguments_are_preserved_and_debug_is_redacted() {
        let directory = tempfile::tempdir().unwrap();
        let arguments_path = directory.path().join("arguments.txt");
        let response_path = directory.path().join("response.bin");
        let capture_path = directory.path().join("request.bin");
        let request = Request::new(
            1,
            RequestOperation::Hello {
                mode: ServerMode::FixtureRecovery,
            },
        );
        fs::write(
            &response_path,
            framed(&Response::new(
                1,
                ResponseKind::Hello {
                    mode: ServerMode::FixtureRecovery,
                    state: LoaderState::Ready,
                    bundle: recovery_bundle(),
                },
            )),
        )
        .unwrap();
        let script_path = directory.path().join("record-arguments.sh");
        let script = format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"{}\"\ndd bs=1 count={} of=\"{}\" 2>/dev/null\ndd if=\"{}\" bs=1 2>/dev/null\nsleep 10\n",
            arguments_path.display(),
            framed(&request).len(),
            capture_path.display(),
            response_path.display(),
        );
        fs::write(&script_path, script).unwrap();

        let mut client = RecoveryFixtureClient::spawn(
            "/bin/sh",
            [
                script_path.as_os_str(),
                OsStr::new("first"),
                OsStr::new("two words"),
                OsStr::new("--literal"),
            ],
            recovery_bundle(),
            Duration::from_secs(1),
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("record-arguments"));
        assert!(!debug.contains("first"));
        assert!(!debug.contains("bladerf_profile2_loader"));
        client.terminate().unwrap();
        assert_eq!(
            fs::read_to_string(arguments_path).unwrap(),
            "first\ntwo words\n--literal\n"
        );
    }
}
