//! Typed bridge from the profile-2 controller to isolated adapter processes.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use bladerf_profile2_ipc::{
    COMMIT_BYTE_OFFSET, COMMIT_MAGIC, FixtureClient, MAX_SESSION_REQUESTS, NativeClient,
    ReadRegister, ShadowChannel,
};

use crate::profile2_control::{
    AppliedConfiguration, CaptureRequest, Controller, Receiver, RegisterAddress, RegisterIo,
    validate_status_poll_bound,
};

const VERIFICATION_WORDS: [u32; 4] = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
// One hello, 20 non-poll Controller operations, and one explicit shutdown must
// fit beside every status poll in the adapter's complete session allowance.
const NATIVE_FIXED_SESSION_REQUESTS: usize = 22;
const MAX_NATIVE_STATUS_POLLS: usize = MAX_SESSION_REQUESTS - NATIVE_FIXED_SESSION_REQUESTS;

/// One bounded native control transaction. Construction is CPU-only and rejects
/// an invalid poll bound before an adapter path is resolved or a child starts.
/// The receiver remains parent-side reconciliation context and is not encoded as
/// a register write by this adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NativeApply {
    request: CaptureRequest,
    max_status_polls: usize,
}

impl NativeApply {
    pub(crate) fn new(request: CaptureRequest, max_status_polls: usize) -> Result<Self> {
        validate_status_poll_bound(max_status_polls)?;
        ensure!(
            max_status_polls <= MAX_NATIVE_STATUS_POLLS,
            "native profile-2 status poll bound must be 1 through {MAX_NATIVE_STATUS_POLLS} so readback and shutdown fit within the {MAX_SESSION_REQUESTS}-request adapter session"
        );
        Ok(Self {
            request,
            max_status_polls,
        })
    }
}

trait TypedClient {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32>;
    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn shutdown(self) -> Result<()>;
    fn is_terminal(&self) -> bool;
}

impl TypedClient for FixtureClient {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        FixtureClient::read_register(self, register).map_err(anyhow::Error::new)
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        FixtureClient::write_shadow(self, channel, value).map_err(anyhow::Error::new)
    }

    fn commit(&mut self) -> Result<()> {
        FixtureClient::commit(self).map_err(anyhow::Error::new)
    }

    fn shutdown(self) -> Result<()> {
        FixtureClient::shutdown(self).map_err(anyhow::Error::new)
    }

    fn is_terminal(&self) -> bool {
        FixtureClient::is_terminal(self)
    }
}

impl TypedClient for NativeClient {
    fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
        NativeClient::read_register(self, register).map_err(anyhow::Error::new)
    }

    fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
        NativeClient::write_shadow(self, channel, value).map_err(anyhow::Error::new)
    }

    fn commit(&mut self) -> Result<()> {
        NativeClient::commit(self).map_err(anyhow::Error::new)
    }

    fn shutdown(self) -> Result<()> {
        NativeClient::shutdown(self).map_err(anyhow::Error::new)
    }

    fn is_terminal(&self) -> bool {
        NativeClient::is_terminal(self)
    }
}

struct IpcRegisterIo<'a, C> {
    client: C,
    stop: Option<&'a AtomicBool>,
}

impl IpcRegisterIo<'_, FixtureClient> {
    fn spawn_fixture(program: &Path, timeout: Duration) -> Result<Self> {
        let program = program
            .canonicalize()
            .with_context(|| format!("resolve fixture adapter {}", program.display()))?;
        let metadata = fs::metadata(&program)
            .with_context(|| format!("inspect fixture adapter {}", program.display()))?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            "fixture adapter must be an executable file: {}",
            program.display()
        );
        let client = FixtureClient::spawn(&program, timeout)
            .with_context(|| format!("start CPU-only fixture adapter {}", program.display()))?;
        Ok(Self { client, stop: None })
    }
}

fn exact_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_not_interrupted(stop: Option<&AtomicBool>) -> Result<()> {
    ensure!(
        !stop.is_some_and(|stop| stop.load(Ordering::Relaxed)),
        "profile-2 adapter transaction was interrupted"
    );
    Ok(())
}

impl<'a> IpcRegisterIo<'a, NativeClient> {
    fn spawn_native(
        program: &Path,
        library: &Path,
        expected_sha256: &str,
        serial: &str,
        timeout: Duration,
        stop: &'a AtomicBool,
    ) -> Result<Self> {
        ensure_not_interrupted(Some(stop))?;
        ensure!(
            exact_lower_hex(expected_sha256, 64),
            "native library SHA-256 must be exactly 64 lowercase hexadecimal characters"
        );
        ensure!(
            exact_lower_hex(serial, 32),
            "bladeRF serial must be exactly 32 lowercase hexadecimal characters"
        );

        let program = program
            .canonicalize()
            .with_context(|| format!("resolve native adapter {}", program.display()))?;
        let metadata = fs::metadata(&program)
            .with_context(|| format!("inspect native adapter {}", program.display()))?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            "native adapter must be an executable file: {}",
            program.display()
        );

        let library = library
            .canonicalize()
            .with_context(|| format!("resolve native library {}", library.display()))?;
        let metadata = fs::metadata(&library)
            .with_context(|| format!("inspect native library {}", library.display()))?;
        ensure!(
            metadata.is_file(),
            "native library must be a regular file: {}",
            library.display()
        );

        ensure_not_interrupted(Some(stop))?;
        let client = NativeClient::spawn(&program, &library, expected_sha256, serial, timeout)
            .with_context(|| {
                format!(
                    "start native profile-2 adapter {} with library {}",
                    program.display(),
                    library.display()
                )
            })?;
        Ok(Self {
            client,
            stop: Some(stop),
        })
    }
}

impl<C: TypedClient> IpcRegisterIo<'_, C> {
    fn check_interrupted(&self) -> Result<()> {
        ensure_not_interrupted(self.stop)
    }

    fn shutdown(self) -> Result<()> {
        self.client
            .shutdown()
            .context("explicitly shut down profile-2 adapter")
    }

    fn is_terminal(&self) -> bool {
        self.client.is_terminal()
    }
}

impl<C: TypedClient> RegisterIo for IpcRegisterIo<'_, C> {
    fn read(&mut self, address: RegisterAddress) -> Result<u32> {
        self.check_interrupted()?;
        let register = ReadRegister::try_from(address.byte_offset())
            .context("map Controller read to the profile-2 IPC allowlist")?;
        self.client
            .read_register(register)
            .context("read profile-2 adapter register")
    }

    fn write(&mut self, address: RegisterAddress, value: u32) -> Result<()> {
        self.check_interrupted()?;
        if address.byte_offset() == COMMIT_BYTE_OFFSET {
            ensure!(
                value == COMMIT_MAGIC,
                "Controller supplied an incompatible profile-2 commit value"
            );
            return self
                .client
                .commit()
                .context("commit profile-2 adapter shadow bank");
        }
        let channel = ShadowChannel::try_from(address.byte_offset())
            .context("map Controller write to the profile-2 IPC allowlist")?;
        self.client
            .write_shadow(channel, value)
            .context("write profile-2 adapter shadow register")
    }
}

fn apply_client<C: TypedClient>(
    mut io: IpcRegisterIo<'_, C>,
    request: CaptureRequest,
    max_status_polls: usize,
) -> Result<AppliedConfiguration> {
    let stop = io.stop;
    let transaction = (|| Controller::new(&mut io, max_status_polls)?.apply_stopped(request))();
    let shutdown = if io.is_terminal() {
        drop(io);
        Ok(())
    } else {
        io.shutdown()
    };

    match (transaction, shutdown) {
        (Ok(applied), Ok(())) => {
            ensure_not_interrupted(stop)?;
            ensure!(
                applied.phase_words() == request.phase_words(),
                "profile-2 adapter returned an unexpected applied configuration"
            );
            Ok(applied)
        }
        (Err(error), Ok(())) => {
            Err(error).context("apply Controller through profile-2 adapter IPC")
        }
        (Ok(_), Err(error)) => Err(error),
        (Err(transaction_error), Err(shutdown_error)) => Err(anyhow!(
            "apply Controller through profile-2 adapter IPC: {transaction_error:#}; additionally failed to shut down adapter: {shutdown_error:#}"
        )),
    }
}

fn verify_client<C: TypedClient>(io: IpcRegisterIo<'_, C>) -> Result<AppliedConfiguration> {
    apply_client(
        io,
        CaptureRequest::new(VERIFICATION_WORDS, Receiver::Rx2),
        4,
    )
}

pub(crate) fn verify_fixture(program: &Path, timeout: Duration) -> Result<AppliedConfiguration> {
    let applied = verify_client(IpcRegisterIo::spawn_fixture(program, timeout)?)?;
    ensure!(
        applied.epoch() == 1,
        "CPU-only fixture returned an unexpected applied epoch"
    );
    Ok(applied)
}

pub(crate) fn verify_native(
    program: &Path,
    library: &Path,
    expected_sha256: &str,
    serial: &str,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<AppliedConfiguration> {
    apply_native(
        program,
        library,
        expected_sha256,
        serial,
        timeout,
        NativeApply::new(CaptureRequest::new(VERIFICATION_WORDS, Receiver::Rx2), 4)?,
        stop,
    )
}

/// Apply caller-selected phase words through the typed native adapter and return
/// them only after the adapter has explicitly shut down. This does not load an
/// FPGA image, configure RF, capture samples, or provide a recovery workflow.
/// Cancellation after a write or commit returns an error without rollback; the
/// FPGA may contain partial shadows or an already applied configuration.
pub(crate) fn apply_native(
    program: &Path,
    library: &Path,
    expected_sha256: &str,
    serial: &str,
    timeout: Duration,
    apply: NativeApply,
    stop: &AtomicBool,
) -> Result<AppliedConfiguration> {
    apply_client(
        IpcRegisterIo::spawn_native(program, library, expected_sha256, serial, timeout, stop)?,
        apply.request,
        apply.max_status_polls,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::fs;
    use std::rc::Rc;

    use bladerf_profile2_ipc::{
        PROFILE2_CAPABILITIES, PROFILE2_ID, Request, RequestOperation, Response, ResponseKind,
        ServerMode, write_frame,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct MockState {
        shadows: [u32; 4],
        written_mask: u8,
        applied: [u32; 4],
        committed: bool,
        status_reads_after_commit: usize,
        reads: Vec<ReadRegister>,
        writes: Vec<(ShadowChannel, u32)>,
        commits: usize,
        shutdowns: usize,
        terminal: bool,
        fail_first_read: bool,
        fail_shutdown: bool,
        applied_epoch: u32,
        interrupt_after_reads: Option<usize>,
        interrupt_after_writes: Option<usize>,
        interrupt_after_commit: bool,
        stop: Option<Rc<AtomicBool>>,
    }

    struct MockClient {
        state: Rc<RefCell<MockState>>,
    }

    impl TypedClient for MockClient {
        fn read_register(&mut self, register: ReadRegister) -> Result<u32> {
            let mut state = self.state.borrow_mut();
            state.reads.push(register);
            if state.interrupt_after_reads == Some(state.reads.len())
                && let Some(stop) = &state.stop
            {
                stop.store(true, Ordering::Relaxed);
            }
            if state.fail_first_read && state.reads.len() == 1 {
                state.terminal = true;
                return Err(anyhow!("injected terminal read failure"));
            }
            let value = match register {
                ReadRegister::Id => PROFILE2_ID,
                ReadRegister::Capabilities => PROFILE2_CAPABILITIES,
                ReadRegister::Shadow0 => state.shadows[0],
                ReadRegister::Shadow1 => state.shadows[1],
                ReadRegister::Shadow2 => state.shadows[2],
                ReadRegister::Shadow3 => state.shadows[3],
                ReadRegister::Status if !state.committed => u32::from(state.written_mask) << 20,
                ReadRegister::Status => {
                    state.status_reads_after_commit += 1;
                    if state.status_reads_after_commit == 1 {
                        1 << 31
                    } else {
                        (1 << 30) | (1 << 29)
                    }
                }
                ReadRegister::AppliedEpoch => state.applied_epoch,
                ReadRegister::OutcomeEpoch => {
                    if state.committed {
                        state.applied_epoch
                    } else {
                        0
                    }
                }
                ReadRegister::Applied0 => state.applied[0],
                ReadRegister::Applied1 => state.applied[1],
                ReadRegister::Applied2 => state.applied[2],
                ReadRegister::Applied3 => state.applied[3],
            };
            Ok(value)
        }

        fn write_shadow(&mut self, channel: ShadowChannel, value: u32) -> Result<()> {
            let mut state = self.state.borrow_mut();
            state.shadows[channel.index()] = value;
            state.written_mask |= 1 << channel.index();
            state.writes.push((channel, value));
            if state.interrupt_after_writes == Some(state.writes.len())
                && let Some(stop) = &state.stop
            {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            let mut state = self.state.borrow_mut();
            ensure!(state.written_mask == 0x0f, "mock commit is incomplete");
            state.applied = state.shadows;
            state.written_mask = 0;
            state.committed = true;
            state.applied_epoch = state
                .applied_epoch
                .checked_add(1)
                .context("mock applied epoch exhausted")?;
            state.commits += 1;
            if state.interrupt_after_commit
                && let Some(stop) = &state.stop
            {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        }

        fn shutdown(self) -> Result<()> {
            let mut state = self.state.borrow_mut();
            state.shutdowns += 1;
            ensure!(!state.fail_shutdown, "injected shutdown failure");
            Ok(())
        }

        fn is_terminal(&self) -> bool {
            self.state.borrow().terminal
        }
    }

    fn mock(state: &Rc<RefCell<MockState>>) -> IpcRegisterIo<'_, MockClient> {
        IpcRegisterIo {
            client: MockClient {
                state: Rc::clone(state),
            },
            stop: None,
        }
    }

    fn push_exchange(
        exchanges: &mut Vec<(Request, Response)>,
        operation: RequestOperation,
        result: ResponseKind,
    ) {
        let id = u64::try_from(exchanges.len()).unwrap() + 1;
        exchanges.push((Request::new(id, operation), Response::new(id, result)));
    }

    fn encoded_request(request: &Request) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, request).unwrap();
        bytes
    }

    fn encoded_response(response: &Response) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, response).unwrap();
        bytes
    }

    #[test]
    fn controller_maps_typed_operations_and_explicitly_shuts_down() {
        let state = Rc::new(RefCell::new(MockState::default()));
        let applied = verify_client(mock(&state)).unwrap();

        assert_eq!(applied.epoch(), 1);
        assert_eq!(applied.phase_words(), VERIFICATION_WORDS);
        let state = state.borrow();
        assert_eq!(
            state.writes,
            [
                ShadowChannel::Channel0,
                ShadowChannel::Channel1,
                ShadowChannel::Channel2,
                ShadowChannel::Channel3,
            ]
            .into_iter()
            .zip(VERIFICATION_WORDS)
            .collect::<Vec<_>>()
        );
        assert_eq!(state.commits, 1);
        assert_eq!(state.status_reads_after_commit, 2);
        assert_eq!(state.shutdowns, 1);
        assert!(!state.terminal);
    }

    #[test]
    fn parameterized_apply_preserves_rx1_as_context_and_enforces_poll_bound() {
        const WORDS: [u32; 4] = [0, 1, 0x7fff_ffff, u32::MAX];
        let request = CaptureRequest::new(WORDS, Receiver::Rx1);
        let rx1 = Rc::new(RefCell::new(MockState::default()));
        let applied = apply_client(mock(&rx1), request, 2).unwrap();
        assert_eq!(applied.phase_words(), WORDS);

        let rx2 = Rc::new(RefCell::new(MockState::default()));
        apply_client(mock(&rx2), CaptureRequest::new(WORDS, Receiver::Rx2), 2).unwrap();
        let rx1 = rx1.borrow();
        let rx2 = rx2.borrow();
        assert_eq!(rx1.reads, rx2.reads);
        assert_eq!(rx1.writes, rx2.writes);
        assert_eq!(rx1.commits, 1);
        assert_eq!(rx1.shutdowns, 1);
        drop(rx1);
        drop(rx2);

        let bounded = Rc::new(RefCell::new(MockState::default()));
        let error = apply_client(mock(&bounded), request, 1).unwrap_err();
        assert!(format!("{error:#}").contains("did not complete within the poll bound"));
        let bounded = bounded.borrow();
        assert_eq!(bounded.commits, 1);
        assert_eq!(bounded.status_reads_after_commit, 1);
        assert_eq!(bounded.shutdowns, 1);
    }

    #[test]
    fn shutdown_failure_never_publishes_an_apply_and_combines_transaction_error() {
        let request = CaptureRequest::new(VERIFICATION_WORDS, Receiver::Rx1);
        let state = Rc::new(RefCell::new(MockState {
            fail_shutdown: true,
            ..MockState::default()
        }));
        let error = apply_client(mock(&state), request, 2).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(
            diagnostic.contains("injected shutdown failure"),
            "{diagnostic}"
        );
        let state_ref = state.borrow();
        assert_eq!(state_ref.commits, 1);
        assert_eq!(state_ref.shutdowns, 1);
        drop(state_ref);

        let state = Rc::new(RefCell::new(MockState {
            fail_shutdown: true,
            ..MockState::default()
        }));
        let error = apply_client(mock(&state), request, 1).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("did not complete within the poll bound"));
        assert!(diagnostic.contains("additionally failed to shut down adapter"));
        assert!(diagnostic.contains("injected shutdown failure"));
        let state = state.borrow();
        assert_eq!(state.commits, 1);
        assert_eq!(state.shutdowns, 1);
    }

    #[test]
    fn terminal_client_error_is_not_misreported_as_shutdown_failure() {
        let state = Rc::new(RefCell::new(MockState {
            fail_first_read: true,
            ..MockState::default()
        }));
        let error = verify_client(mock(&state)).unwrap_err();
        let diagnostic = format!("{error:#}");

        assert!(diagnostic.contains("injected terminal read failure"));
        assert!(!diagnostic.contains("additionally failed to shut down"));
        let state = state.borrow();
        assert!(state.terminal);
        assert_eq!(state.shutdowns, 0);
    }

    #[test]
    fn native_verification_accepts_a_nonzero_previous_epoch() {
        let state = Rc::new(RefCell::new(MockState {
            applied_epoch: 7,
            ..MockState::default()
        }));
        let applied = verify_client(mock(&state)).unwrap();

        assert_eq!(applied.epoch(), 8);
        assert_eq!(applied.phase_words(), VERIFICATION_WORDS);
        let state = state.borrow();
        assert_eq!(state.commits, 1);
        assert_eq!(state.shutdowns, 1);
    }

    #[test]
    fn interrupt_after_startup_prevents_writes_and_still_shuts_down() {
        let stop = Rc::new(AtomicBool::new(false));
        let state = Rc::new(RefCell::new(MockState {
            interrupt_after_reads: Some(1),
            stop: Some(Rc::clone(&stop)),
            ..MockState::default()
        }));
        let io = IpcRegisterIo {
            client: MockClient {
                state: Rc::clone(&state),
            },
            stop: Some(stop.as_ref()),
        };
        let error = verify_client(io).unwrap_err();
        let diagnostic = format!("{error:#}");

        assert!(diagnostic.contains("interrupted"), "{diagnostic}");
        let state = state.borrow();
        assert_eq!(state.reads, [ReadRegister::Id]);
        assert!(state.writes.is_empty());
        assert_eq!(state.commits, 0);
        assert_eq!(state.shutdowns, 1);
    }

    #[test]
    fn interrupt_does_not_claim_rollback_after_a_write_or_commit() {
        let stop = Rc::new(AtomicBool::new(false));
        let state = Rc::new(RefCell::new(MockState {
            interrupt_after_writes: Some(1),
            stop: Some(Rc::clone(&stop)),
            ..MockState::default()
        }));
        let io = IpcRegisterIo {
            client: MockClient {
                state: Rc::clone(&state),
            },
            stop: Some(stop.as_ref()),
        };
        let error = verify_client(io).unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        let state = state.borrow();
        assert_eq!(state.writes.len(), 1);
        assert_eq!(state.written_mask, 1);
        assert_eq!(state.commits, 0);
        assert_eq!(state.shutdowns, 1);
        drop(state);

        let stop = Rc::new(AtomicBool::new(false));
        let state = Rc::new(RefCell::new(MockState {
            interrupt_after_commit: true,
            stop: Some(Rc::clone(&stop)),
            ..MockState::default()
        }));
        let io = IpcRegisterIo {
            client: MockClient {
                state: Rc::clone(&state),
            },
            stop: Some(stop.as_ref()),
        };
        let error = verify_client(io).unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        let state = state.borrow();
        assert_eq!(state.writes.len(), 4);
        assert_eq!(state.commits, 1);
        assert_eq!(state.applied_epoch, 1);
        assert_eq!(state.shutdowns, 1);
        drop(state);

        let stop = Rc::new(AtomicBool::new(false));
        let state = Rc::new(RefCell::new(MockState {
            interrupt_after_reads: Some(17),
            stop: Some(Rc::clone(&stop)),
            ..MockState::default()
        }));
        let io = IpcRegisterIo {
            client: MockClient {
                state: Rc::clone(&state),
            },
            stop: Some(stop.as_ref()),
        };
        let error = apply_client(
            io,
            CaptureRequest::new(VERIFICATION_WORDS, Receiver::Rx1),
            2,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        let state = state.borrow();
        assert_eq!(state.reads.len(), 17);
        assert_eq!(state.applied, VERIFICATION_WORDS);
        assert_eq!(state.commits, 1);
        assert_eq!(state.shutdowns, 1);
    }

    fn run_native_boundary(words: [u32; 4], apply: Option<NativeApply>) -> AppliedConfiguration {
        let previous_epoch = 7;
        let applied_epoch = previous_epoch + 1;
        let mut exchanges = Vec::new();
        push_exchange(
            &mut exchanges,
            RequestOperation::Hello {},
            ResponseKind::Hello {
                mode: ServerMode::Native,
            },
        );
        for (register, value) in [
            (ReadRegister::Id, PROFILE2_ID),
            (ReadRegister::Capabilities, PROFILE2_CAPABILITIES),
            (ReadRegister::Status, 0),
            (ReadRegister::AppliedEpoch, previous_epoch),
        ] {
            push_exchange(
                &mut exchanges,
                RequestOperation::ReadRegister { register },
                ResponseKind::ReadRegister { register, value },
            );
        }
        for (channel, value) in [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ]
        .into_iter()
        .zip(words)
        {
            push_exchange(
                &mut exchanges,
                RequestOperation::WriteShadow { channel, value },
                ResponseKind::WriteShadow { channel },
            );
            let register = ReadRegister::try_from(channel.byte_offset()).unwrap();
            push_exchange(
                &mut exchanges,
                RequestOperation::ReadRegister { register },
                ResponseKind::ReadRegister { register, value },
            );
        }
        push_exchange(
            &mut exchanges,
            RequestOperation::ReadRegister {
                register: ReadRegister::Status,
            },
            ResponseKind::ReadRegister {
                register: ReadRegister::Status,
                value: 0x00f0_0000,
            },
        );
        push_exchange(
            &mut exchanges,
            RequestOperation::Commit {},
            ResponseKind::Commit {},
        );
        for (register, value) in [
            (ReadRegister::Status, 0x8000_0000),
            (ReadRegister::Status, 0x6000_0000),
            (ReadRegister::OutcomeEpoch, applied_epoch),
            (ReadRegister::AppliedEpoch, applied_epoch),
            (ReadRegister::Applied0, words[0]),
            (ReadRegister::Applied1, words[1]),
            (ReadRegister::Applied2, words[2]),
            (ReadRegister::Applied3, words[3]),
        ] {
            push_exchange(
                &mut exchanges,
                RequestOperation::ReadRegister { register },
                ResponseKind::ReadRegister { register, value },
            );
        }
        push_exchange(
            &mut exchanges,
            RequestOperation::Shutdown {},
            ResponseKind::Shutdown {},
        );

        let directory = tempfile::tempdir().unwrap();
        let script_path = directory.path().join("fake-native-adapter.sh");
        let library_path = directory.path().join("fake-libbladeRF.so.2");
        let completion_path = directory.path().join("complete.txt");
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        for (index, (request, response)) in exchanges.iter().enumerate() {
            let request_path = directory.path().join(format!("request-{index}.frame"));
            let response_path = directory.path().join(format!("response-{index}.frame"));
            let observed_path = directory.path().join(format!("observed-{index}.frame"));
            for path in [&request_path, &response_path, &observed_path] {
                assert!(!path.to_string_lossy().contains('\''));
            }
            let request = encoded_request(request);
            fs::write(&request_path, &request).unwrap();
            fs::write(&response_path, encoded_response(response)).unwrap();
            writeln!(
                script,
                "dd bs=1 count={} of='{}' 2>/dev/null\ncmp -s '{}' '{}'\ncat '{}'",
                request.len(),
                observed_path.display(),
                request_path.display(),
                observed_path.display(),
                response_path.display(),
            )
            .unwrap();
        }
        assert!(!completion_path.to_string_lossy().contains('\''));
        writeln!(
            script,
            "printf '%s\\n' complete > '{}'",
            completion_path.display()
        )
        .unwrap();
        fs::write(&script_path, script).unwrap();
        fs::write(&library_path, b"CPU fake; never loaded").unwrap();
        let mut permissions = fs::metadata(&script_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script_path, permissions).unwrap();

        let stop = AtomicBool::new(false);
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let serial = "0123456789abcdef0123456789abcdef";
        let applied = match apply {
            Some(apply) => apply_native(
                &script_path,
                &library_path,
                digest,
                serial,
                Duration::from_secs(2),
                apply,
                &stop,
            ),
            None => verify_native(
                &script_path,
                &library_path,
                digest,
                serial,
                Duration::from_secs(2),
                &stop,
            ),
        }
        .unwrap();

        assert_eq!(applied.epoch(), applied_epoch);
        assert_eq!(applied.phase_words(), words);
        assert_eq!(fs::read_to_string(completion_path).unwrap(), "complete\n");
        applied
    }

    #[test]
    fn native_verifier_runs_the_fixed_controller_transaction_against_a_cpu_fake() {
        run_native_boundary(VERIFICATION_WORDS, None);
    }

    #[test]
    fn native_apply_carries_non_oracle_words_without_encoding_the_receiver() {
        let words = [0, 0x1357_9bdf, 0x2468_ace0, u32::MAX];
        let request = CaptureRequest::new(words, Receiver::Rx1);
        let apply = NativeApply::new(request, 2).unwrap();
        let applied = run_native_boundary(words, Some(apply));
        assert_eq!(applied.phase_words(), request.phase_words());

        assert!(NativeApply::new(request, MAX_NATIVE_STATUS_POLLS).is_ok());
        assert!(NativeApply::new(request, MAX_NATIVE_STATUS_POLLS + 1).is_err());
        assert!(NativeApply::new(request, 0).is_err());
        assert!(NativeApply::new(request, 4097).is_err());
    }

    #[test]
    fn native_identity_syntax_fails_before_path_or_process_access() {
        let missing_adapter = Path::new("/this/adapter/must/not/be-resolved");
        let missing_library = Path::new("/this/library/must/not/be-resolved");
        let serial = "0123456789abcdef0123456789abcdef";
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let stop = AtomicBool::new(false);

        let error = match IpcRegisterIo::<NativeClient>::spawn_native(
            missing_adapter,
            missing_library,
            "not-a-digest",
            serial,
            Duration::from_secs(1),
            &stop,
        ) {
            Ok(_) => panic!("invalid digest unexpectedly spawned a native adapter"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("SHA-256"), "{diagnostic}");
        assert!(
            !diagnostic.contains("resolve native adapter"),
            "{diagnostic}"
        );

        let error = match IpcRegisterIo::<NativeClient>::spawn_native(
            missing_adapter,
            missing_library,
            sha256,
            "0123456789ABCDEF0123456789ABCDEF",
            Duration::from_secs(1),
            &stop,
        ) {
            Ok(_) => panic!("invalid serial unexpectedly spawned a native adapter"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("serial"), "{diagnostic}");
        assert!(
            !diagnostic.contains("resolve native adapter"),
            "{diagnostic}"
        );

        stop.store(true, Ordering::Relaxed);
        let error = match IpcRegisterIo::<NativeClient>::spawn_native(
            missing_adapter,
            missing_library,
            sha256,
            serial,
            Duration::from_secs(1),
            &stop,
        ) {
            Ok(_) => panic!("interrupted launch unexpectedly spawned a native adapter"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("interrupted"), "{diagnostic}");
        assert!(
            !diagnostic.contains("resolve native adapter"),
            "{diagnostic}"
        );
    }
}
