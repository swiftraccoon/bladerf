//! Framed complete-session server for role-fixed primary backends.
//!
//! The startup factory remains dormant until a complete, compatible hello has
//! been decoded. Before capture starts, every error is terminal and receives at
//! most one bounded correlated error. Capture is different: after publishing
//! `CaptureStarted`, stdout contains a fixed raw byte extent. Any backend,
//! output, or evidence failure from that point exits without an error frame so
//! raw bytes can never be mistaken for framing.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use anyhow::{Error, Result, anyhow, ensure};
use bladerf_profile2_ipc::{
    ReadRegister as WorkerReadRegister, ShadowChannel as WorkerShadowChannel,
};
use bladerf_profile2_session_ipc::{
    ActiveReceiveReadback, BundleFacts, CaptureReport, ConfiguredReceiveReadback, ErrorCode,
    MAX_ERROR_MESSAGE_BYTES, PROTOCOL_FAMILY, PROTOCOL_VERSION, ReadRegister, ReceiveChannel,
    ReceivePlan as WireReceivePlan, Request, RequestOperation, Response, ResponseKind, ServerMode,
    SessionState, ShadowChannel, read_frame, write_frame,
};
use sha2::{Digest, Sha256};

use crate::loader_worker::{
    ActiveReceiveReadback as WorkerActiveReadback, CaptureReport as WorkerCaptureReport,
    ConfiguredReceiveReadback as WorkerConfiguredReadback, ReceiveChannel as WorkerReceiveChannel,
    ReceivePlan as WorkerReceivePlan,
};

/// Fixed operations supplied by one complete-session backend.
pub(crate) trait SessionBackend {
    fn bundle(&self) -> &BundleFacts;
    fn load_profile2(&mut self) -> Result<()>;
    fn verify_profile2(&mut self) -> Result<()>;
    fn read_register(&mut self, register: WorkerReadRegister) -> Result<u32>;
    fn write_shadow(&mut self, channel: WorkerShadowChannel, value: u32) -> Result<()>;
    fn commit(&mut self) -> Result<()>;
    fn configure_receive(&mut self, plan: WorkerReceivePlan) -> Result<WorkerConfiguredReadback>;
    fn start_receive(&mut self) -> Result<WorkerActiveReadback>;
    fn capture_receive(&mut self, output: &mut dyn Write) -> Result<WorkerCaptureReport>;
    fn stop_receive(&mut self) -> Result<()>;
    fn restore_stock(&mut self) -> Result<()>;
    fn verify_stock(&mut self) -> Result<()>;
    fn shutdown(&mut self) -> Result<()>;
}

fn bounded_message(mut message: String) -> String {
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

fn terminal_error<W: Write>(output: &mut W, id: u64, code: ErrorCode, error: Error) -> Error {
    let mut diagnostic = format!("{error:#}");
    let response = Response::error(id, code, bounded_message(diagnostic.clone()));
    if let Err(response_error) = write_frame(output, &response) {
        diagnostic.push_str(&format!(
            "; publish terminal session response: {response_error}"
        ));
    }
    anyhow!(diagnostic)
}

fn write_response<W: Write>(output: &mut W, response: &Response) -> Result<()> {
    write_frame(output, response)
        .map_err(|error| anyhow!("publish session response {}: {error}", response.id()))
}

fn worker_register(register: ReadRegister) -> WorkerReadRegister {
    match register {
        ReadRegister::Id => WorkerReadRegister::Id,
        ReadRegister::Capabilities => WorkerReadRegister::Capabilities,
        ReadRegister::Shadow0 => WorkerReadRegister::Shadow0,
        ReadRegister::Shadow1 => WorkerReadRegister::Shadow1,
        ReadRegister::Shadow2 => WorkerReadRegister::Shadow2,
        ReadRegister::Shadow3 => WorkerReadRegister::Shadow3,
        ReadRegister::Status => WorkerReadRegister::Status,
        ReadRegister::AppliedEpoch => WorkerReadRegister::AppliedEpoch,
        ReadRegister::OutcomeEpoch => WorkerReadRegister::OutcomeEpoch,
        ReadRegister::Applied0 => WorkerReadRegister::Applied0,
        ReadRegister::Applied1 => WorkerReadRegister::Applied1,
        ReadRegister::Applied2 => WorkerReadRegister::Applied2,
        ReadRegister::Applied3 => WorkerReadRegister::Applied3,
    }
}

fn worker_shadow(channel: ShadowChannel) -> WorkerShadowChannel {
    match channel {
        ShadowChannel::Channel0 => WorkerShadowChannel::Channel0,
        ShadowChannel::Channel1 => WorkerShadowChannel::Channel1,
        ShadowChannel::Channel2 => WorkerShadowChannel::Channel2,
        ShadowChannel::Channel3 => WorkerShadowChannel::Channel3,
    }
}

fn worker_channel(channel: ReceiveChannel) -> WorkerReceiveChannel {
    match channel {
        ReceiveChannel::Rx1 => WorkerReceiveChannel::Rx1,
        ReceiveChannel::Rx2 => WorkerReceiveChannel::Rx2,
    }
}

fn wire_channel(channel: WorkerReceiveChannel) -> ReceiveChannel {
    match channel {
        WorkerReceiveChannel::Rx1 => ReceiveChannel::Rx1,
        WorkerReceiveChannel::Rx2 => ReceiveChannel::Rx2,
    }
}

fn worker_plan(plan: WireReceivePlan) -> Result<WorkerReceivePlan> {
    WorkerReceivePlan::new(
        worker_channel(plan.channel()),
        plan.center_frequency_hz(),
        plan.gain_db(),
        plan.data_records(),
    )
}

fn configured_readback(readback: WorkerConfiguredReadback) -> ConfiguredReceiveReadback {
    ConfiguredReceiveReadback::new(
        wire_channel(readback.channel()),
        readback.center_frequency_hz(),
        readback.sample_rate_hz(),
        readback.bandwidth_hz(),
    )
}

fn active_readback(readback: WorkerActiveReadback) -> ActiveReceiveReadback {
    ActiveReceiveReadback::new(
        configured_readback(readback.configured()),
        readback.gain_db(),
    )
}

fn capture_report(report: &WorkerCaptureReport) -> CaptureReport {
    CaptureReport::new(report.samples(), report.bytes(), report.sha256().to_owned())
}

struct CaptureEvidenceWriter<'a, W> {
    output: &'a mut W,
    maximum_bytes: u64,
    bytes: u64,
    hasher: Sha256,
}

impl<'a, W> CaptureEvidenceWriter<'a, W> {
    fn new(output: &'a mut W, maximum_bytes: u64) -> Self {
        Self {
            output,
            maximum_bytes,
            bytes: 0,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes, format!("{:x}", self.hasher.finalize()))
    }
}

impl<W: Write> Write for CaptureEvidenceWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.maximum_bytes.saturating_sub(self.bytes);
        if remaining == 0 {
            return Err(io::Error::other(
                "session backend exceeded the declared raw capture extent",
            ));
        }
        let allowed = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| io::Error::other("capture extent exceeds usize"))?;
        let count = self.output.write(&buffer[..allowed])?;
        self.bytes = self
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("capture evidence byte count overflow"))?;
        self.hasher.update(&buffer[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Serve one lazily constructed complete-session backend.
pub(crate) fn serve_with_factory<R, W, B, F>(
    input: &mut R,
    output: &mut W,
    mode: ServerMode,
    backend_factory: F,
) -> Result<()>
where
    R: Read,
    W: Write,
    B: SessionBackend,
    F: FnOnce() -> Result<B>,
{
    let mut backend_factory = Some(backend_factory);
    let mut backend = None;
    let mut state = SessionState::Ready;
    let mut negotiated = false;
    let mut expected_id = 1u64;
    let mut handled = 0usize;
    let mut receive_plan = None;

    loop {
        let request = match read_frame::<_, Request>(&mut *input) {
            Ok(Some(request)) => request,
            Ok(None) => return Err(anyhow!("session input ended before explicit shutdown")),
            Err(error) => return Err(anyhow!(error)),
        };
        let (family, version, id, operation) = request.into_parts();
        if family != PROTOCOL_FAMILY {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("session request family {family:?} differs from {PROTOCOL_FAMILY:?}"),
            ));
        }
        if version != PROTOCOL_VERSION {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!(
                    "session request protocol version {version} differs from {PROTOCOL_VERSION}"
                ),
            ));
        }
        if id != expected_id {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("session request id {id} is not the expected id {expected_id}"),
            ));
        }
        if handled >= mode.max_session_requests() {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::RequestLimit,
                anyhow!(
                    "session request limit {} reached for {mode:?}",
                    mode.max_session_requests()
                ),
            ));
        }

        if !negotiated {
            let RequestOperation::Hello {
                mode: requested_mode,
            } = operation
            else {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("hello must be the first session operation"),
                ));
            };
            if requested_mode != mode {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!(
                        "session hello requested {requested_mode:?}, but this child is {mode:?}"
                    ),
                ));
            }
            let Some(factory) = backend_factory.take() else {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("session backend factory was already consumed"),
                ));
            };
            let created = match factory() {
                Ok(created) => created,
                Err(error) => {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
            };
            let bundle = created.bundle().clone();
            backend = Some(created);
            negotiated = true;
            write_response(
                output,
                &Response::new(
                    id,
                    ResponseKind::Hello {
                        mode,
                        state,
                        bundle,
                    },
                ),
            )?;
            handled += 1;
            expected_id += 1;
            continue;
        }

        let Some(active) = backend.as_mut() else {
            return Err(anyhow!("session backend is unavailable after hello"));
        };
        let response_kind = match (state, operation) {
            (_, RequestOperation::Hello { .. }) => {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("session hello was already completed"),
                ));
            }
            (SessionState::Ready, RequestOperation::LoadProfile2 {}) => {
                active
                    .load_profile2()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::CustomUnverified;
                ResponseKind::State { state }
            }
            (SessionState::CustomUnverified, RequestOperation::VerifyProfile2 {}) => {
                active
                    .verify_profile2()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::CustomVerified;
                ResponseKind::State { state }
            }
            (SessionState::CustomVerified, RequestOperation::ReadRegister { register }) => {
                let value = active
                    .read_register(worker_register(register))
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                ResponseKind::RegisterValue {
                    state,
                    register,
                    value,
                }
            }
            (SessionState::CustomVerified, RequestOperation::WriteShadow { channel, value }) => {
                active
                    .write_shadow(worker_shadow(channel), value)
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                ResponseKind::State { state }
            }
            (SessionState::CustomVerified, RequestOperation::Commit {}) => {
                active
                    .commit()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                ResponseKind::State { state }
            }
            (SessionState::CustomVerified, RequestOperation::ConfigureReceive { plan }) => {
                let worker_plan = worker_plan(plan)
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                let readback = active
                    .configure_receive(worker_plan)
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                receive_plan = Some(plan);
                state = SessionState::ReceiveConfigured;
                ResponseKind::Configured {
                    state,
                    readback: configured_readback(readback),
                }
            }
            (SessionState::ReceiveConfigured, RequestOperation::StartReceive {}) => {
                let readback = active
                    .start_receive()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::ReceiveActive;
                ResponseKind::Active {
                    state,
                    readback: active_readback(readback),
                }
            }
            (SessionState::ReceiveActive, RequestOperation::Capture {}) => {
                let plan = receive_plan.ok_or_else(|| {
                    terminal_error(
                        output,
                        id,
                        ErrorCode::Backend,
                        anyhow!("session server lost its receive plan"),
                    )
                })?;
                let transport_samples = plan.expected_transport_samples();
                let raw_bytes = plan.expected_raw_bytes();
                write_response(
                    output,
                    &Response::new(
                        id,
                        ResponseKind::CaptureStarted {
                            state,
                            transport_samples,
                            raw_bytes,
                        },
                    ),
                )?;

                // From this point onward no framed error may be emitted.
                let mut evidence = CaptureEvidenceWriter::new(output, raw_bytes);
                let report = active.capture_receive(&mut evidence)?;
                evidence.flush()?;
                let (observed_bytes, observed_sha256) = evidence.finish();
                ensure!(
                    observed_bytes == raw_bytes,
                    "session backend wrote {observed_bytes} raw bytes; expected {raw_bytes}"
                );
                ensure!(
                    report.samples() == transport_samples
                        && report.bytes() == observed_bytes
                        && report.sha256() == observed_sha256,
                    "session backend capture report differs from child-observed raw output"
                );
                let report = capture_report(&report);
                state = SessionState::CaptureComplete;
                write_response(
                    output,
                    &Response::new(id, ResponseKind::CaptureComplete { state, report }),
                )?;
                handled += 1;
                expected_id += 1;
                continue;
            }
            (
                SessionState::ReceiveActive | SessionState::CaptureComplete,
                RequestOperation::StopReceive {},
            ) => {
                active
                    .stop_receive()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::ReceiveStopped;
                ResponseKind::State { state }
            }
            (
                SessionState::CustomVerified
                | SessionState::ReceiveConfigured
                | SessionState::ReceiveStopped,
                RequestOperation::RestoreStock {},
            ) => {
                active
                    .restore_stock()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                receive_plan = None;
                state = SessionState::StockUnverified;
                ResponseKind::State { state }
            }
            (SessionState::StockUnverified, RequestOperation::VerifyStock {}) => {
                active
                    .verify_stock()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::StockVerified;
                ResponseKind::State { state }
            }
            (SessionState::Ready | SessionState::StockVerified, RequestOperation::Shutdown {}) => {
                active
                    .shutdown()
                    .map_err(|error| terminal_error(output, id, ErrorCode::Backend, error))?;
                state = SessionState::Closed;
                write_response(output, &Response::new(id, ResponseKind::State { state }))?;
                return Ok(());
            }
            (current, operation) => {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!(
                        "session operation {operation:?} is invalid while state is {current:?}"
                    ),
                ));
            }
        };

        write_response(output, &Response::new(id, response_kind))?;
        handled += 1;
        expected_id += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    use anyhow::bail;
    use bladerf_profile2_session_ipc::{
        ImageFacts, LibraryFacts, MIN_DATA_RECORDS, REQUIRED_LIBRARY_MARKER,
        REQUIRED_LIBRARY_VERSION, REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256,
    };

    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    struct MockBackend {
        bundle: BundleFacts,
        plan: Option<WorkerReceivePlan>,
        fail_capture: bool,
        mismatch_capture_report: bool,
        load_calls: Rc<Cell<usize>>,
        shutdown_calls: Rc<Cell<usize>>,
    }

    impl MockBackend {
        fn new(fail_capture: bool) -> Self {
            Self {
                bundle: BundleFacts::new(
                    LibraryFacts::new(REQUIRED_LIBRARY_VERSION, DIGEST, REQUIRED_LIBRARY_MARKER),
                    ImageFacts::new(DIGEST, 1),
                    ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
                )
                .unwrap(),
                plan: None,
                fail_capture,
                mismatch_capture_report: false,
                load_calls: Rc::new(Cell::new(0)),
                shutdown_calls: Rc::new(Cell::new(0)),
            }
        }
    }

    impl SessionBackend for MockBackend {
        fn bundle(&self) -> &BundleFacts {
            &self.bundle
        }

        fn load_profile2(&mut self) -> Result<()> {
            self.load_calls.set(self.load_calls.get() + 1);
            Ok(())
        }

        fn verify_profile2(&mut self) -> Result<()> {
            Ok(())
        }

        fn read_register(&mut self, _register: WorkerReadRegister) -> Result<u32> {
            Ok(0)
        }

        fn write_shadow(&mut self, _channel: WorkerShadowChannel, _value: u32) -> Result<()> {
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            Ok(())
        }

        fn configure_receive(
            &mut self,
            plan: WorkerReceivePlan,
        ) -> Result<WorkerConfiguredReadback> {
            self.plan = Some(plan);
            Ok(WorkerConfiguredReadback::new(
                plan.channel(),
                plan.center_hz(),
                crate::loader_worker::SOURCE_SAMPLE_RATE_HZ,
                crate::loader_worker::SOURCE_BANDWIDTH_HZ,
            ))
        }

        fn start_receive(&mut self) -> Result<WorkerActiveReadback> {
            let plan = self.plan.expect("mock plan configured before start");
            Ok(WorkerActiveReadback::new(
                WorkerConfiguredReadback::new(
                    plan.channel(),
                    plan.center_hz(),
                    crate::loader_worker::SOURCE_SAMPLE_RATE_HZ,
                    crate::loader_worker::SOURCE_BANDWIDTH_HZ,
                ),
                plan.gain_db(),
            ))
        }

        fn capture_receive(&mut self, output: &mut dyn Write) -> Result<WorkerCaptureReport> {
            let plan = self.plan.expect("mock plan configured before capture");
            if self.fail_capture {
                output.write_all(&[0xa5])?;
                bail!("injected capture failure after raw output started");
            }
            let bytes = vec![0xa5; usize::try_from(plan.expected_bytes())?];
            output.write_all(&bytes)?;
            Ok(WorkerCaptureReport::new(
                plan.expected_samples(),
                plan.expected_bytes(),
                if self.mismatch_capture_report {
                    "0".repeat(64)
                } else {
                    format!("{:x}", Sha256::digest(&bytes))
                },
            ))
        }

        fn stop_receive(&mut self) -> Result<()> {
            Ok(())
        }

        fn restore_stock(&mut self) -> Result<()> {
            Ok(())
        }

        fn verify_stock(&mut self) -> Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.shutdown_calls.set(self.shutdown_calls.get() + 1);
            Ok(())
        }
    }

    struct FlushFailWriter {
        bytes: Vec<u8>,
        flushes: usize,
        fail_on_flush: usize,
    }

    impl Write for FlushFailWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == self.fail_on_flush {
                Err(io::Error::other("injected lost session response"))
            } else {
                Ok(())
            }
        }
    }

    fn raw_json_frame(json: &str) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(json.len() as u32).to_le_bytes());
        frame.extend_from_slice(json.as_bytes());
        frame
    }

    fn request_frames(operations: impl IntoIterator<Item = RequestOperation>) -> Vec<u8> {
        let mut frames = Vec::new();
        for (offset, operation) in operations.into_iter().enumerate() {
            write_frame(&mut frames, &Request::new(offset as u64 + 1, operation)).unwrap();
        }
        frames
    }

    #[test]
    fn typed_register_and_channel_conversions_are_exhaustive() {
        assert_eq!(worker_register(ReadRegister::Id), WorkerReadRegister::Id);
        assert_eq!(
            worker_register(ReadRegister::Applied3),
            WorkerReadRegister::Applied3
        );
        assert_eq!(
            worker_shadow(ShadowChannel::Channel0),
            WorkerShadowChannel::Channel0
        );
        assert_eq!(
            worker_shadow(ShadowChannel::Channel3),
            WorkerShadowChannel::Channel3
        );
    }

    #[test]
    fn capture_writer_caps_raw_output_and_hashes_only_written_bytes() {
        let mut output = Vec::new();
        let mut writer = CaptureEvidenceWriter::new(&mut output, 3);
        assert_eq!(writer.write(b"abcde").unwrap(), 3);
        assert!(writer.write(b"de").is_err());
        let (bytes, sha256) = writer.finish();
        assert_eq!(bytes, 3);
        assert_eq!(output, b"abc");
        assert_eq!(sha256, format!("{:x}", Sha256::digest(b"abc")));
    }

    #[test]
    fn incompatible_or_out_of_order_first_request_never_runs_factory() {
        let cases = [
            raw_json_frame(
                r#"{"family":"bladerf_profile2_loader","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
            ),
            raw_json_frame(
                r#"{"family":"bladerf_profile2_session","version":2,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
            ),
            request_frames([RequestOperation::LoadProfile2 {}]),
        ];

        for input in cases {
            let factory_called = Rc::new(Cell::new(false));
            let marker = Rc::clone(&factory_called);
            let mut input = Cursor::new(input);
            let mut output = Vec::new();
            let result = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixturePrimary,
                move || {
                    marker.set(true);
                    Ok(MockBackend::new(false))
                },
            );
            assert!(result.is_err());
            assert!(!factory_called.get());
        }
    }

    #[test]
    fn capture_failure_after_started_emits_raw_prefix_without_error_frame() {
        let plan =
            WireReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MIN_DATA_RECORDS).unwrap();
        let input = request_frames([
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
            RequestOperation::LoadProfile2 {},
            RequestOperation::VerifyProfile2 {},
            RequestOperation::ConfigureReceive { plan },
            RequestOperation::StartReceive {},
            RequestOperation::Capture {},
        ]);
        let mut input = Cursor::new(input);
        let mut output = Vec::new();
        let result =
            serve_with_factory(&mut input, &mut output, ServerMode::FixturePrimary, || {
                Ok(MockBackend::new(true))
            });
        assert!(result.is_err());

        let mut output = Cursor::new(output);
        for _ in 0..5 {
            let response = read_frame::<_, Response>(&mut output).unwrap().unwrap();
            assert!(!matches!(response.result(), ResponseKind::Error { .. }));
        }
        let started = read_frame::<_, Response>(&mut output).unwrap().unwrap();
        assert!(matches!(
            started.result(),
            ResponseKind::CaptureStarted { raw_bytes, .. }
                if *raw_bytes == plan.expected_raw_bytes()
        ));
        let mut remainder = Vec::new();
        output.read_to_end(&mut remainder).unwrap();
        assert_eq!(remainder, [0xa5]);
    }

    #[test]
    fn mismatched_capture_report_exits_after_exact_raw_extent_without_a_frame() {
        let plan =
            WireReceivePlan::new(ReceiveChannel::Rx2, 98_100_000, 40, MIN_DATA_RECORDS).unwrap();
        let input = request_frames([
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
            RequestOperation::LoadProfile2 {},
            RequestOperation::VerifyProfile2 {},
            RequestOperation::ConfigureReceive { plan },
            RequestOperation::StartReceive {},
            RequestOperation::Capture {},
        ]);
        let mut input = Cursor::new(input);
        let mut output = Vec::new();
        let result =
            serve_with_factory(&mut input, &mut output, ServerMode::FixturePrimary, || {
                let mut backend = MockBackend::new(false);
                backend.mismatch_capture_report = true;
                Ok(backend)
            });
        assert!(result.is_err());

        let mut output = Cursor::new(output);
        for _ in 0..5 {
            let response = read_frame::<_, Response>(&mut output).unwrap().unwrap();
            assert!(!matches!(response.result(), ResponseKind::Error { .. }));
        }
        let started = read_frame::<_, Response>(&mut output).unwrap().unwrap();
        assert!(matches!(
            started.result(),
            ResponseKind::CaptureStarted { raw_bytes, .. }
                if *raw_bytes == plan.expected_raw_bytes()
        ));
        let mut raw = vec![0_u8; usize::try_from(plan.expected_raw_bytes()).unwrap()];
        output.read_exact(&mut raw).unwrap();
        assert!(raw.iter().all(|byte| *byte == 0xa5));
        let mut remainder = Vec::new();
        output.read_to_end(&mut remainder).unwrap();
        assert!(remainder.is_empty());
    }

    #[test]
    fn lost_transition_response_never_retries_or_orders_shutdown() {
        let input = request_frames([
            RequestOperation::Hello {
                mode: ServerMode::FixturePrimary,
            },
            RequestOperation::LoadProfile2 {},
        ]);
        let backend = MockBackend::new(false);
        let load_calls = Rc::clone(&backend.load_calls);
        let shutdown_calls = Rc::clone(&backend.shutdown_calls);
        let mut backend = Some(backend);
        let mut input = Cursor::new(input);
        let mut output = FlushFailWriter {
            bytes: Vec::new(),
            flushes: 0,
            fail_on_flush: 2,
        };
        let result =
            serve_with_factory(&mut input, &mut output, ServerMode::FixturePrimary, || {
                Ok(backend.take().unwrap())
            });
        assert!(result.is_err());
        assert_eq!(load_calls.get(), 1);
        assert_eq!(shutdown_calls.get(), 0);
        assert_eq!(output.flushes, 2);
    }
}
