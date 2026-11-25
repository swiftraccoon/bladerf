//! Parent-side device-free qualification of one complete session process.
//!
//! The parent performs only lexical expectation validation and process
//! supervision before Hello. The child independently stages both images,
//! qualifies the exact loader-neutral library, and binds one model handle to
//! the entire custom/control/RX/capture/stop/stock lifecycle. No serial or
//! device selector exists in this flow.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use bladerf_profile2_ipc::{PROFILE2_CAPABILITIES, PROFILE2_ID};
use bladerf_profile2_session_ipc::{
    BundleFacts, CaptureReport, ImageFacts, LibraryFacts, MAX_DATA_RECORDS,
    MAX_TRANSACTION_TIMEOUT, MIN_DATA_RECORDS, REQUIRED_LIBRARY_MARKER, REQUIRED_LIBRARY_VERSION,
    REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256, ReadRegister, ReceiveChannel, ReceivePlan,
    ServerMode, SessionClient, ShadowChannel,
};
use sha2::{Digest, Sha256};

use crate::loader_fixture::validate_library_expectation;
use crate::sealed_image::{CustomImage, ExpectedImage, StagedImages, StockImage};

const EXPECTED_REQUESTS: u64 = 31;
const EXPECTED_REFERENCE_MODEL_CAPTURE_SHA256: &str =
    "284863be84327eead3b5f9c4f2873f55f93664861b3c70693df62415002fa12e";
const EXPECTED_MAXIMUM_MODEL_CAPTURE_SHA256: &str =
    "0151cdb0719ce7e6671fa2650fed7f32db0767f5f4e9c1cc48fc8b1be3aa1d87";
const MAXIMUM_PIPE_READER_PAUSE: Duration = Duration::from_millis(250);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionProcessQualification {
    Reference,
    MaximumPipeStress,
}

impl SessionProcessQualification {
    const fn data_records(self) -> u64 {
        match self {
            Self::Reference => MIN_DATA_RECORDS,
            Self::MaximumPipeStress => MAX_DATA_RECORDS,
        }
    }

    const fn forces_reader_pause(self) -> bool {
        matches!(self, Self::MaximumPipeStress)
    }

    const fn expected_sha256(self) -> &'static str {
        match self {
            Self::Reference => EXPECTED_REFERENCE_MODEL_CAPTURE_SHA256,
            Self::MaximumPipeStress => EXPECTED_MAXIMUM_MODEL_CAPTURE_SHA256,
        }
    }
}

/// Value-only report from one complete fake subprocess lifecycle.
#[derive(Debug)]
pub(crate) struct SessionProcessReport {
    library_sha256: String,
    custom_sha256: String,
    custom_bytes: u64,
    capture: CaptureReport,
    data_records: u64,
    capture_elapsed: Duration,
    reader_pause: Option<Duration>,
}

impl SessionProcessReport {
    pub(crate) fn library_sha256(&self) -> &str {
        &self.library_sha256
    }

    pub(crate) fn custom_sha256(&self) -> &str {
        &self.custom_sha256
    }

    pub(crate) const fn custom_bytes(&self) -> u64 {
        self.custom_bytes
    }

    pub(crate) const fn capture(&self) -> &CaptureReport {
        &self.capture
    }

    pub(crate) const fn data_records(&self) -> u64 {
        self.data_records
    }

    pub(crate) const fn capture_elapsed(&self) -> Duration {
        self.capture_elapsed
    }

    pub(crate) fn deadline_headroom(&self) -> Duration {
        MAX_TRANSACTION_TIMEOUT.saturating_sub(self.capture_elapsed)
    }

    pub(crate) const fn reader_pause(&self) -> Option<Duration> {
        self.reader_pause
    }
}

pub(crate) struct SessionProcessCheck<'a> {
    pub(crate) program: &'a Path,
    pub(crate) library: &'a Path,
    pub(crate) library_sha256: &'a str,
    pub(crate) custom_image: &'a Path,
    pub(crate) custom_sha256: &'a str,
    pub(crate) custom_bytes: u64,
    pub(crate) stock_image: &'a Path,
    pub(crate) qualification: SessionProcessQualification,
}

/// Run one complete session client against this binary's fake primary child.
///
/// Validation before spawn is lexical only: it does not open, canonicalize or
/// hash the library or image paths. The child repeats validation and performs
/// all file access inside its bounded Hello transaction.
pub(crate) fn check_profile2_session_process(
    check: SessionProcessCheck<'_>,
) -> Result<SessionProcessReport> {
    let SessionProcessCheck {
        program,
        library,
        library_sha256,
        custom_image,
        custom_sha256,
        custom_bytes,
        stock_image,
        qualification,
    } = check;
    ensure!(
        !program.as_os_str().is_empty() && program.is_absolute(),
        "session fixture executable must be an absolute path: {}",
        program.display()
    );
    validate_library_expectation(library, library_sha256)?;
    let custom = ExpectedImage::<CustomImage>::custom(custom_image, custom_sha256, custom_bytes)?;
    let stock = ExpectedImage::<StockImage>::stock(stock_image)?;
    StagedImages::validate_roles(&custom, &stock)?;

    let bundle = BundleFacts::new(
        LibraryFacts::new(
            REQUIRED_LIBRARY_VERSION,
            library_sha256,
            REQUIRED_LIBRARY_MARKER,
        ),
        ImageFacts::new(custom_sha256, custom_bytes),
        ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
    )
    .context("construct exact expected session bundle")?;
    // Open all parent-owned output state before the child exists. No writable
    // path or descriptor number crosses the session protocol. The maximum-pipe
    // sink keeps fixed-size count/hash working state plus one anonymous file and
    // deliberately pauses the first raw write. That reader stall exercises the
    // finite operating-system pipe with a raw extent much larger than its
    // capacity; it does not directly time how long the child writer is blocked.
    let sink = match qualification {
        SessionProcessQualification::Reference => ParentCaptureSink::File(
            tempfile::tempfile().context("create private parent capture sink")?,
        ),
        SessionProcessQualification::MaximumPipeStress => ParentCaptureSink::MaximumPipe(
            MaximumPipeSink::new(MAXIMUM_PIPE_READER_PAUSE)
                .context("create private maximum-pipe capture sink")?,
        ),
    };
    let mut client = SessionClient::spawn(
        program,
        session_arguments(
            library,
            library_sha256,
            custom_image,
            custom_sha256,
            custom_bytes,
            stock_image,
        ),
        ServerMode::FixturePrimary,
        &bundle,
        MAX_TRANSACTION_TIMEOUT,
    )
    .context("start exact complete-session fixture process")?;

    client
        .load_profile2()
        .context("dispatch custom image through fake complete-session worker")?;
    client
        .verify_profile2()
        .context("verify fake complete-session custom identity")?;
    verify_control(&mut client)?;

    let plan = ReceivePlan::new(
        ReceiveChannel::Rx2,
        98_100_000,
        40,
        qualification.data_records(),
    )?;
    let configured = client
        .configure_receive(plan)
        .context("configure fake RX2 and verify disabled-RX readback")?;
    ensure!(
        configured.channel() == ReceiveChannel::Rx2
            && configured.center_frequency_hz() == plan.center_frequency_hz()
            && configured.sample_rate_hz() == bladerf_profile2_session_ipc::SOURCE_SAMPLE_RATE_HZ
            && configured.bandwidth_hz() == bladerf_profile2_session_ipc::SOURCE_BANDWIDTH_HZ,
        "complete-session configured readback differs from the exact release plan"
    );
    let active = client
        .start_receive()
        .context("start fake RX2 and verify active readback")?;
    ensure!(
        active.configured() == configured && active.gain_db() == plan.gain_db(),
        "complete-session active readback differs from the exact release plan"
    );

    // Transfer the already-open parent-owned sink to the client by ownership.
    let capture_started = Instant::now();
    let (capture, sink) = client
        .capture(sink)
        .context("stream exact fake raw capture into the parent-owned sink")?;
    let capture_elapsed = capture_started.elapsed();
    client
        .stop_receive()
        .context("stop fake RX2 after raw capture")?;
    client
        .restore_stock()
        .context("restore stock through the same fake session handle")?;
    client
        .verify_stock()
        .context("verify fake same-handle stock identity")?;
    client
        .shutdown()
        .context("shut down complete-session fixture process")?;
    ensure!(
        client.request_count() == EXPECTED_REQUESTS,
        "complete-session client issued {} requests; expected {EXPECTED_REQUESTS}",
        client.request_count()
    );

    // Do not inspect or accept the captured artifact until RX stop, stock
    // restoration, stock verification, orderly child shutdown, and reaping
    // have all succeeded. Any earlier failure drops the unvalidated sink.
    let evidence = sink.finish()?;
    ensure!(
        evidence.bytes == capture.raw_bytes()
            && evidence.sha256 == capture.sha256()
            && capture.transport_samples() == plan.expected_transport_samples()
            && capture.raw_bytes() == plan.expected_raw_bytes(),
        "parent-owned capture evidence differs from the exact session report"
    );
    ensure!(
        evidence.sha256 == qualification.expected_sha256(),
        "model capture digest changed for {qualification:?}"
    );
    ensure!(
        capture_elapsed < MAX_TRANSACTION_TIMEOUT,
        "complete capture transaction exceeded its configured deadline"
    );
    ensure!(
        evidence.reader_pause.is_some() == qualification.forces_reader_pause(),
        "maximum-pipe qualification did not enter and release its forced reader pause"
    );
    if let Some(reader_pause) = evidence.reader_pause {
        ensure!(
            reader_pause >= MAXIMUM_PIPE_READER_PAUSE,
            "maximum-pipe reader pause was shorter than its configured duration"
        );
    }

    Ok(SessionProcessReport {
        library_sha256: library_sha256.to_owned(),
        custom_sha256: custom_sha256.to_owned(),
        custom_bytes,
        capture,
        data_records: plan.data_records(),
        capture_elapsed,
        reader_pause: evidence.reader_pause,
    })
}

struct CaptureEvidence {
    bytes: u64,
    sha256: String,
    reader_pause: Option<Duration>,
}

enum ParentCaptureSink {
    File(File),
    MaximumPipe(MaximumPipeSink),
}

impl ParentCaptureSink {
    fn finish(self) -> Result<CaptureEvidence> {
        match self {
            Self::File(mut file) => {
                let (bytes, sha256) = hash_capture_sink(&mut file)?;
                Ok(CaptureEvidence {
                    bytes,
                    sha256,
                    reader_pause: None,
                })
            }
            Self::MaximumPipe(sink) => sink.finish(),
        }
    }
}

impl Write for ParentCaptureSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(buffer),
            Self::MaximumPipe(sink) => sink.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::MaximumPipe(sink) => sink.flush(),
        }
    }
}

struct MaximumPipeSink {
    file: File,
    bytes: u64,
    hasher: Sha256,
    reader_pause: Duration,
    observed_pause: Option<Duration>,
    synced: bool,
}

impl MaximumPipeSink {
    fn new(reader_pause: Duration) -> Result<Self> {
        let file = tempfile::tempfile().context("create anonymous maximum-pipe file")?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("set maximum-pipe file mode")?;
        let metadata = file
            .metadata()
            .context("inspect anonymous maximum-pipe file")?;
        ensure!(
            metadata.is_file()
                && metadata.len() == 0
                && metadata.permissions().mode() & 0o777 == 0o600
                && metadata.nlink() == 0,
            "maximum-pipe sink must be an empty, anonymous mode-0600 regular file"
        );
        Ok(Self {
            file,
            bytes: 0,
            hasher: Sha256::new(),
            reader_pause,
            observed_pause: None,
            synced: false,
        })
    }

    fn finish(mut self) -> Result<CaptureEvidence> {
        ensure!(
            self.synced,
            "maximum-pipe file was not flushed and synchronized inside capture"
        );
        let streamed_bytes = self.bytes;
        let streamed_sha256 = format!("{:x}", self.hasher.finalize());
        let (bytes, sha256) = hash_capture_sink(&mut self.file)
            .context("independently rehash synchronized maximum-pipe file")?;
        ensure!(
            bytes == streamed_bytes && sha256 == streamed_sha256,
            "maximum-pipe file reread differs from streamed sink evidence"
        );
        Ok(CaptureEvidence {
            bytes,
            sha256,
            reader_pause: self.observed_pause,
        })
    }
}

impl Write for MaximumPipeSink {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.observed_pause.is_none() {
            let started = Instant::now();
            thread::sleep(self.reader_pause);
            self.observed_pause = Some(started.elapsed());
        }
        let written = self.file.write(buffer)?;
        if written != 0 {
            self.synced = false;
        }
        self.bytes = self
            .bytes
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("maximum-pipe sink byte count overflow"))?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()?;
        self.synced = true;
        Ok(())
    }
}

fn verify_control(client: &mut SessionClient) -> Result<()> {
    ensure!(
        client.read_register(ReadRegister::Id)? == PROFILE2_ID,
        "session fixture profile ID differs"
    );
    ensure!(
        client.read_register(ReadRegister::Capabilities)? == PROFILE2_CAPABILITIES,
        "session fixture capabilities differ"
    );
    ensure!(
        client.read_register(ReadRegister::Status)? == 0,
        "session fixture initial status differs"
    );
    ensure!(
        client.read_register(ReadRegister::AppliedEpoch)? == 0,
        "session fixture initial applied epoch differs"
    );
    for ((channel, register), word) in CHANNELS.into_iter().zip(SHADOW_READS).zip(WORDS) {
        client.write_shadow(channel, word)?;
        ensure!(
            client.read_register(register)? == word,
            "session fixture shadow readback differs"
        );
    }
    ensure!(
        client.read_register(ReadRegister::Status)? == 0x00f0_0000,
        "session fixture staged status differs"
    );
    client.commit()?;
    ensure!(
        client.read_register(ReadRegister::Status)? == 0x6000_0000,
        "session fixture committed status differs"
    );
    ensure!(
        client.read_register(ReadRegister::OutcomeEpoch)? == 1
            && client.read_register(ReadRegister::AppliedEpoch)? == 1,
        "session fixture applied epoch differs"
    );
    for (register, word) in APPLIED_READS.into_iter().zip(WORDS) {
        ensure!(
            client.read_register(register)? == word,
            "session fixture applied readback differs"
        );
    }
    Ok(())
}

fn hash_capture_sink(sink: &mut File) -> Result<(u64, String)> {
    sink.seek(SeekFrom::Start(0))
        .context("rewind parent capture sink")?;
    let mut bytes = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = sink.read(&mut buffer).context("read parent capture sink")?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .context("parent capture byte count overflow")?;
        hasher.update(&buffer[..count]);
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

fn session_arguments(
    library: &Path,
    library_sha256: &str,
    custom_image: &Path,
    custom_sha256: &str,
    custom_bytes: u64,
    stock_image: &Path,
) -> Vec<OsString> {
    vec![
        "session-fixture-primary".into(),
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
    ]
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn child_arguments_are_role_fixed_and_contain_no_device_selector() {
        let arguments = session_arguments(
            Path::new("/opt/profile 2/libbladeRF.so.2"),
            SHA256,
            Path::new("/opt/profile 2/custom.rbf"),
            &"a".repeat(64),
            12_858_972,
            Path::new("/opt/profile 2/stock.rbf"),
        );
        assert_eq!(arguments[0], OsStr::new("session-fixture-primary"));
        assert!(arguments.contains(&OsString::from("/opt/profile 2/custom.rbf")));
        assert!(!arguments.iter().any(|value| value == "--serial"));
        assert!(!arguments.iter().any(|value| value == "--device"));
    }

    #[test]
    fn malformed_expectations_fail_before_spawning_the_program() {
        let error = check_profile2_session_process(SessionProcessCheck {
            program: Path::new("/this/program/must/not/be/spawned"),
            library: Path::new("/this/library/must/not/be-opened.so"),
            library_sha256: "not-a-digest",
            custom_image: Path::new("/this/custom/must/not/be-opened.rbf"),
            custom_sha256: &"a".repeat(64),
            custom_bytes: 12_858_972,
            stock_image: Path::new("/this/stock/must/not/be-opened.rbf"),
            qualification: SessionProcessQualification::Reference,
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("SHA-256"));
    }

    #[test]
    fn maximum_pipe_mode_freezes_extent_and_fixed_working_memory_sink() {
        let plan = ReceivePlan::new(
            ReceiveChannel::Rx2,
            98_100_000,
            40,
            SessionProcessQualification::MaximumPipeStress.data_records(),
        )
        .unwrap();
        assert_eq!(plan.data_records(), MAX_DATA_RECORDS);
        assert_eq!(plan.expected_transport_samples(), 80_000_008);
        assert_eq!(plan.expected_raw_bytes(), 320_000_032);
        assert_eq!(
            SessionProcessQualification::MaximumPipeStress.expected_sha256(),
            "0151cdb0719ce7e6671fa2650fed7f32db0767f5f4e9c1cc48fc8b1be3aa1d87"
        );
        assert!(std::mem::size_of::<MaximumPipeSink>() <= 256);

        let mut sink = MaximumPipeSink::new(Duration::ZERO).unwrap();
        sink.write_all(b"bounded").unwrap();
        sink.flush().unwrap();
        let evidence = sink.finish().unwrap();
        assert_eq!(evidence.bytes, 7);
        assert_eq!(evidence.sha256, format!("{:x}", Sha256::digest(b"bounded")));
        assert!(evidence.reader_pause.is_some());
    }
}
