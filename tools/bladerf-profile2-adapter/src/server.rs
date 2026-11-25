use std::io::{Read, Write};

use anyhow::{Error, Result, anyhow};
use bladerf_profile2_ipc::{
    ErrorCode, MAX_SESSION_REQUESTS, PROTOCOL_VERSION, Request, RequestOperation, Response,
    ResponseKind, ServerMode, read_frame, write_frame,
};

use crate::backend::Backend;

const MAX_ERROR_MESSAGE_BYTES: usize = 512;

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

fn terminal_error<B: Backend, W: Write>(
    backend: Option<&mut B>,
    output: &mut W,
    id: u64,
    code: ErrorCode,
    error: Error,
) -> Error {
    let mut diagnostic = format!("{error:#}");
    if let Some(backend) = backend
        && let Err(shutdown_error) = backend.shutdown()
    {
        diagnostic.push_str(&format!("; shut down backend: {shutdown_error:#}"));
    }
    let response = Response::error(id, code, bounded_message(diagnostic.clone()));
    if let Err(response_error) = write_frame(output, &response) {
        diagnostic.push_str(&format!("; publish terminal response: {response_error}"));
    }
    anyhow!(diagnostic)
}

fn terminal_without_response<B: Backend>(backend: Option<&mut B>, error: Error) -> Error {
    let mut diagnostic = format!("{error:#}");
    if let Some(backend) = backend
        && let Err(shutdown_error) = backend.shutdown()
    {
        diagnostic.push_str(&format!("; shut down backend: {shutdown_error:#}"));
    }
    anyhow!(diagnostic)
}

fn write_response<B: Backend, W: Write>(
    backend: Option<&mut B>,
    output: &mut W,
    response: &Response,
) -> Result<()> {
    if let Err(error) = write_frame(output, response) {
        let mut diagnostic = format!("publish response {}: {error}", response.id());
        if let Some(backend) = backend
            && let Err(shutdown_error) = backend.shutdown()
        {
            diagnostic.push_str(&format!("; shut down backend: {shutdown_error:#}"));
        }
        return Err(anyhow!(diagnostic));
    }
    Ok(())
}

struct BorrowedBackend<'a, B: Backend + ?Sized>(&'a mut B);

impl<B: Backend + ?Sized> Backend for BorrowedBackend<'_, B> {
    fn read_register(&mut self, register: bladerf_profile2_ipc::ReadRegister) -> Result<u32> {
        self.0.read_register(register)
    }

    fn write_shadow(
        &mut self,
        channel: bladerf_profile2_ipc::ShadowChannel,
        value: u32,
    ) -> Result<()> {
        self.0.write_shadow(channel, value)
    }

    fn commit(&mut self) -> Result<()> {
        self.0.commit()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.0.shutdown()
    }
}

pub(crate) fn serve<R: Read, W: Write, B: Backend>(
    input: &mut R,
    output: &mut W,
    backend: &mut B,
) -> Result<()> {
    serve_inner(
        input,
        output,
        ServerMode::Fixture,
        Some(BorrowedBackend(backend)),
        || None,
    )
}

pub(crate) fn serve_with_factory<R, W, B, F>(
    input: &mut R,
    output: &mut W,
    mode: ServerMode,
    backend_factory: F,
) -> Result<()>
where
    R: Read,
    W: Write,
    B: Backend,
    F: FnOnce() -> Result<B>,
{
    let mut backend_factory = Some(backend_factory);
    serve_inner(input, output, mode, None, move || {
        backend_factory.take().map(|factory| factory())
    })
}

fn serve_inner<R, W, B, F>(
    input: &mut R,
    output: &mut W,
    mode: ServerMode,
    mut backend: Option<B>,
    mut take_backend: F,
) -> Result<()>
where
    R: Read,
    W: Write,
    B: Backend,
    F: FnMut() -> Option<Result<B>>,
{
    let mut hello_complete = false;
    let mut expected_id = 1u64;
    let mut handled = 0usize;

    loop {
        let request = match read_frame::<_, Request>(&mut *input) {
            Ok(Some(request)) => request,
            Ok(None) => {
                return Err(terminal_without_response(
                    backend.as_mut(),
                    anyhow!("adapter input ended before explicit shutdown"),
                ));
            }
            Err(error) => {
                return Err(terminal_without_response(backend.as_mut(), anyhow!(error)));
            }
        };
        let (version, id, operation) = request.into_parts();
        if version != PROTOCOL_VERSION {
            return Err(terminal_error(
                backend.as_mut(),
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("request protocol version {version} differs from {PROTOCOL_VERSION}"),
            ));
        }
        if id != expected_id {
            return Err(terminal_error(
                backend.as_mut(),
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("request id {id} is not the expected id {expected_id}"),
            ));
        }
        if handled == MAX_SESSION_REQUESTS {
            return Err(terminal_error(
                backend.as_mut(),
                output,
                id,
                ErrorCode::RequestLimit,
                anyhow!("adapter request limit {MAX_SESSION_REQUESTS} reached"),
            ));
        }

        if !hello_complete && !matches!(&operation, RequestOperation::Hello {}) {
            return Err(terminal_error(
                backend.as_mut(),
                output,
                id,
                ErrorCode::InvalidState,
                anyhow!("hello must be the first adapter operation"),
            ));
        }

        let (response, shutdown) = match operation {
            RequestOperation::Hello {} if !hello_complete => {
                if backend.is_none() {
                    let Some(created_backend) = take_backend() else {
                        return Err(terminal_error(
                            None::<&mut B>,
                            output,
                            id,
                            ErrorCode::InvalidState,
                            anyhow!("adapter backend factory was already consumed"),
                        ));
                    };
                    backend = match created_backend {
                        Ok(backend) => Some(backend),
                        Err(error) => {
                            return Err(terminal_error(
                                None::<&mut B>,
                                output,
                                id,
                                ErrorCode::Backend,
                                error,
                            ));
                        }
                    };
                }
                hello_complete = true;
                (Response::new(id, ResponseKind::Hello { mode }), false)
            }
            RequestOperation::Hello {} => {
                return Err(terminal_error(
                    backend.as_mut(),
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("hello was already completed"),
                ));
            }
            RequestOperation::ReadRegister { register } => {
                let Some(backend) = backend.as_mut() else {
                    return Err(anyhow!("adapter backend is unavailable after hello"));
                };
                let value = match backend.read_register(register) {
                    Ok(value) => value,
                    Err(error) => {
                        return Err(terminal_error(
                            Some(backend),
                            output,
                            id,
                            ErrorCode::Backend,
                            error,
                        ));
                    }
                };
                (
                    Response::new(id, ResponseKind::ReadRegister { register, value }),
                    false,
                )
            }
            RequestOperation::WriteShadow { channel, value } => {
                let Some(backend) = backend.as_mut() else {
                    return Err(anyhow!("adapter backend is unavailable after hello"));
                };
                if let Err(error) = backend.write_shadow(channel, value) {
                    return Err(terminal_error(
                        Some(backend),
                        output,
                        id,
                        ErrorCode::Backend,
                        error,
                    ));
                }
                (
                    Response::new(id, ResponseKind::WriteShadow { channel }),
                    false,
                )
            }
            RequestOperation::Commit {} => {
                let Some(backend) = backend.as_mut() else {
                    return Err(anyhow!("adapter backend is unavailable after hello"));
                };
                if let Err(error) = backend.commit() {
                    return Err(terminal_error(
                        Some(backend),
                        output,
                        id,
                        ErrorCode::Backend,
                        error,
                    ));
                }
                (Response::new(id, ResponseKind::Commit {}), false)
            }
            RequestOperation::Shutdown {} => {
                let Some(backend) = backend.as_mut() else {
                    return Err(anyhow!("adapter backend is unavailable after hello"));
                };
                if let Err(error) = backend.shutdown() {
                    return Err(terminal_error(
                        Some(backend),
                        output,
                        id,
                        ErrorCode::Backend,
                        error,
                    ));
                }
                (Response::new(id, ResponseKind::Shutdown {}), true)
            }
        };

        write_response(backend.as_mut(), output, &response)?;
        handled += 1;
        expected_id += 1;
        if shutdown {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io::Cursor;
    use std::rc::Rc;

    use anyhow::{Result, bail};
    use bladerf_profile2_ipc::{
        PROFILE2_CAPABILITIES, PROFILE2_ID, ReadRegister, RequestOperation, ResponseKind,
        ShadowChannel,
    };

    use super::*;
    use crate::backend::FixtureBackend;

    fn framed_requests(requests: impl IntoIterator<Item = Request>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for request in requests {
            write_frame(&mut bytes, &request).unwrap();
        }
        bytes
    }

    fn decoded_responses(mut bytes: &[u8]) -> Vec<Response> {
        let mut responses = Vec::new();
        while let Some(response) = read_frame(&mut bytes).unwrap() {
            responses.push(response);
        }
        responses
    }

    fn next(id: &mut u64, operation: RequestOperation) -> Request {
        let request = Request::new(*id, operation);
        *id += 1;
        request
    }

    #[test]
    fn fixture_serves_exact_controller_style_transaction() {
        let words = [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef];
        let channels = [
            ShadowChannel::Channel0,
            ShadowChannel::Channel1,
            ShadowChannel::Channel2,
            ShadowChannel::Channel3,
        ];
        let shadows = [
            ReadRegister::Shadow0,
            ReadRegister::Shadow1,
            ReadRegister::Shadow2,
            ReadRegister::Shadow3,
        ];
        let applied = [
            ReadRegister::Applied0,
            ReadRegister::Applied1,
            ReadRegister::Applied2,
            ReadRegister::Applied3,
        ];
        let mut id = 1;
        let mut requests = vec![
            next(&mut id, RequestOperation::Hello {}),
            next(
                &mut id,
                RequestOperation::ReadRegister {
                    register: ReadRegister::Id,
                },
            ),
            next(
                &mut id,
                RequestOperation::ReadRegister {
                    register: ReadRegister::Capabilities,
                },
            ),
            next(
                &mut id,
                RequestOperation::ReadRegister {
                    register: ReadRegister::Status,
                },
            ),
            next(
                &mut id,
                RequestOperation::ReadRegister {
                    register: ReadRegister::AppliedEpoch,
                },
            ),
        ];
        for ((channel, register), word) in channels.into_iter().zip(shadows).zip(words) {
            requests.push(next(
                &mut id,
                RequestOperation::WriteShadow {
                    channel,
                    value: word,
                },
            ));
            requests.push(next(&mut id, RequestOperation::ReadRegister { register }));
        }
        for operation in [
            RequestOperation::ReadRegister {
                register: ReadRegister::Status,
            },
            RequestOperation::Commit {},
            RequestOperation::ReadRegister {
                register: ReadRegister::Status,
            },
            RequestOperation::ReadRegister {
                register: ReadRegister::Status,
            },
            RequestOperation::ReadRegister {
                register: ReadRegister::OutcomeEpoch,
            },
            RequestOperation::ReadRegister {
                register: ReadRegister::AppliedEpoch,
            },
        ] {
            requests.push(next(&mut id, operation));
        }
        for register in applied {
            requests.push(next(&mut id, RequestOperation::ReadRegister { register }));
        }
        requests.push(next(&mut id, RequestOperation::Shutdown {}));

        let mut input = Cursor::new(framed_requests(requests));
        let mut output = Vec::new();
        let mut backend = FixtureBackend::new();
        serve(&mut input, &mut output, &mut backend).unwrap();
        let responses = decoded_responses(&output);
        assert_eq!(responses.len(), 24);
        for (index, response) in responses.iter().enumerate() {
            assert_eq!(response.id(), index as u64 + 1);
        }
        assert!(matches!(
            responses[0].result(),
            ResponseKind::Hello {
                mode: ServerMode::Fixture
            }
        ));
        assert!(matches!(
            responses[1].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::Id,
                value: PROFILE2_ID
            }
        ));
        assert!(matches!(
            responses[2].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::Capabilities,
                value: PROFILE2_CAPABILITIES
            }
        ));
        assert!(matches!(
            responses[13].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::Status,
                value: 0x00f0_0000
            }
        ));
        assert!(matches!(
            responses[15].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::Status,
                value: 0x8000_0000
            }
        ));
        assert!(matches!(
            responses[16].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::Status,
                value: 0x6000_0000
            }
        ));
        assert!(matches!(
            responses[17].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::OutcomeEpoch,
                value: 1
            }
        ));
        assert!(matches!(
            responses[18].result(),
            ResponseKind::ReadRegister {
                register: ReadRegister::AppliedEpoch,
                value: 1
            }
        ));
        for (response, (register, value)) in
            responses[19..23].iter().zip(applied.into_iter().zip(words))
        {
            assert!(matches!(
                response.result(),
                ResponseKind::ReadRegister {
                    register: actual_register,
                    value: actual_value
                } if *actual_register == register && *actual_value == value
            ));
        }
        assert!(matches!(
            responses.last().unwrap().result(),
            ResponseKind::Shutdown {}
        ));
    }

    #[derive(Default)]
    struct TestBackend {
        reads: usize,
        shutdowns: usize,
        fail_reads: bool,
        fail_shutdowns: usize,
    }

    impl Backend for TestBackend {
        fn read_register(&mut self, _register: ReadRegister) -> Result<u32> {
            self.reads += 1;
            if self.fail_reads {
                bail!("injected read failure");
            }
            Ok(0)
        }

        fn write_shadow(&mut self, _channel: ShadowChannel, _value: u32) -> Result<()> {
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.shutdowns += 1;
            if self.fail_shutdowns > 0 {
                self.fail_shutdowns -= 1;
                bail!("injected shutdown failure");
            }
            Ok(())
        }
    }

    fn hello(id: u64) -> Request {
        Request::new(id, RequestOperation::Hello {})
    }

    fn read(id: u64) -> Request {
        Request::new(
            id,
            RequestOperation::ReadRegister {
                register: ReadRegister::Status,
            },
        )
    }

    #[test]
    fn hello_reports_the_explicit_server_mode() {
        for mode in [ServerMode::Fixture, ServerMode::Native] {
            let mut input = Cursor::new(framed_requests([
                hello(1),
                Request::new(2, RequestOperation::Shutdown {}),
            ]));
            let mut output = Vec::new();
            let factory_calls = Rc::new(Cell::new(0));
            let calls = Rc::clone(&factory_calls);

            serve_with_factory(&mut input, &mut output, mode, move || {
                calls.set(calls.get() + 1);
                Ok(TestBackend::default())
            })
            .unwrap();

            assert_eq!(factory_calls.get(), 1);
            let responses = decoded_responses(&output);
            assert_eq!(responses.len(), 2);
            assert!(matches!(
                responses[0].result(),
                ResponseKind::Hello { mode: actual } if *actual == mode
            ));
            assert!(matches!(responses[1].result(), ResponseKind::Shutdown {}));
        }
    }

    #[test]
    fn backend_factory_is_not_invoked_before_a_valid_hello() {
        let wrong_version = br#"{"version":2,"id":1,"operation":{"kind":"hello"}}"#;
        let mut wrong_version_frame = Vec::from((wrong_version.len() as u32).to_le_bytes());
        wrong_version_frame.extend_from_slice(wrong_version);

        for bytes in [
            Vec::new(),
            vec![1, 0],
            framed_requests([read(1)]),
            wrong_version_frame,
        ] {
            let mut input = Cursor::new(bytes);
            let mut output = Vec::new();
            let factory_calls = Rc::new(Cell::new(0));
            let calls = Rc::clone(&factory_calls);

            let result = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::Native,
                move || -> Result<TestBackend> {
                    calls.set(calls.get() + 1);
                    Ok(TestBackend::default())
                },
            );

            assert!(result.is_err());
            assert_eq!(factory_calls.get(), 0);
        }
    }

    #[test]
    fn backend_startup_failure_is_a_correlated_terminal_response() {
        let mut input = Cursor::new(framed_requests([
            hello(1),
            Request::new(2, RequestOperation::Shutdown {}),
        ]));
        let mut output = Vec::new();
        let factory_calls = Rc::new(Cell::new(0));
        let calls = Rc::clone(&factory_calls);

        let error = serve_with_factory(
            &mut input,
            &mut output,
            ServerMode::Native,
            move || -> Result<TestBackend> {
                calls.set(calls.get() + 1);
                bail!("injected native startup failure")
            },
        )
        .unwrap_err();

        assert!(
            format!("{error:#}").contains("injected native startup failure"),
            "{error:#}"
        );
        assert_eq!(factory_calls.get(), 1);
        let responses = decoded_responses(&output);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].id(), 1);
        assert!(matches!(
            responses[0].result(),
            ResponseKind::Error {
                code: ErrorCode::Backend,
                message
            } if message.contains("injected native startup failure")
        ));
    }

    #[test]
    fn operation_order_and_ids_are_terminal() {
        for requests in [
            vec![read(1), hello(2)],
            vec![hello(2), read(1)],
            vec![hello(1), hello(2), read(3)],
            vec![hello(1), read(3), read(2)],
        ] {
            let mut input = Cursor::new(framed_requests(requests));
            let mut output = Vec::new();
            let mut backend = TestBackend::default();
            assert!(serve(&mut input, &mut output, &mut backend).is_err());
            let responses = decoded_responses(&output);
            assert!(matches!(
                responses.last().unwrap().result(),
                ResponseKind::Error { .. }
            ));
            assert_eq!(backend.shutdowns, 1);
        }
    }

    #[test]
    fn wrong_version_and_malformed_frame_are_terminal() {
        let raw = br#"{"version":2,"id":1,"operation":{"kind":"hello"}}"#;
        let mut bytes = Vec::from((raw.len() as u32).to_le_bytes());
        bytes.extend_from_slice(raw);
        bytes.extend_from_slice(&framed_requests([hello(1)]));
        let mut input = Cursor::new(bytes);
        let mut output = Vec::new();
        let mut backend = TestBackend::default();
        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        let responses = decoded_responses(&output);
        assert_eq!(responses.len(), 1);
        assert!(matches!(
            responses[0].result(),
            ResponseKind::Error {
                code: ErrorCode::Protocol,
                ..
            }
        ));
        assert_eq!(backend.shutdowns, 1);

        let mut input = Cursor::new(vec![1, 0]);
        let mut output = Vec::new();
        let mut backend = TestBackend::default();
        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        assert!(output.is_empty());
        assert_eq!(backend.shutdowns, 1);
    }

    #[test]
    fn backend_failure_is_reported_once_and_closes_session() {
        let mut input = Cursor::new(framed_requests([hello(1), read(2), read(3)]));
        let mut output = Vec::new();
        let mut backend = TestBackend {
            fail_reads: true,
            ..TestBackend::default()
        };
        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        let responses = decoded_responses(&output);
        assert_eq!(responses.len(), 2);
        assert!(matches!(
            responses[1].result(),
            ResponseKind::Error {
                code: ErrorCode::Backend,
                message
            } if message.contains("injected read failure")
        ));
        assert_eq!(backend.reads, 1);
        assert_eq!(backend.shutdowns, 1);
    }

    #[test]
    fn eof_before_shutdown_and_request_limit_are_terminal() {
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let mut backend = TestBackend::default();
        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        assert!(output.is_empty());
        assert_eq!(backend.shutdowns, 1);

        let mut requests = Vec::with_capacity(MAX_SESSION_REQUESTS + 1);
        requests.push(hello(1));
        for id in 2..=MAX_SESSION_REQUESTS as u64 + 1 {
            requests.push(read(id));
        }
        let mut input = Cursor::new(framed_requests(requests));
        let mut output = Vec::new();
        let mut backend = TestBackend::default();
        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        let responses = decoded_responses(&output);
        assert_eq!(responses.len(), MAX_SESSION_REQUESTS + 1);
        assert!(matches!(
            responses.last().unwrap().result(),
            ResponseKind::Error {
                code: ErrorCode::RequestLimit,
                ..
            }
        ));
        assert_eq!(
            responses.last().unwrap().id(),
            MAX_SESSION_REQUESTS as u64 + 1
        );
        assert_eq!(backend.reads, MAX_SESSION_REQUESTS - 1);
        assert_eq!(backend.shutdowns, 1);
    }

    struct OrderedBackend {
        shutdown: Rc<Cell<bool>>,
    }

    impl Backend for OrderedBackend {
        fn read_register(&mut self, _register: ReadRegister) -> Result<u32> {
            Ok(0)
        }

        fn write_shadow(&mut self, _channel: ShadowChannel, _value: u32) -> Result<()> {
            Ok(())
        }

        fn commit(&mut self) -> Result<()> {
            Ok(())
        }

        fn shutdown(&mut self) -> Result<()> {
            self.shutdown.set(true);
            Ok(())
        }
    }

    struct ShutdownAwareWriter {
        shutdown: Rc<Cell<bool>>,
        wrote_before_shutdown: Rc<Cell<bool>>,
        bytes: Vec<u8>,
    }

    impl Write for ShutdownAwareWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if !self.shutdown.get() {
                self.wrote_before_shutdown.set(true);
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn terminal_errors_shut_down_before_publishing() {
        let shutdown = Rc::new(Cell::new(false));
        let wrote_before_shutdown = Rc::new(Cell::new(false));
        let mut backend = OrderedBackend {
            shutdown: Rc::clone(&shutdown),
        };
        let mut output = ShutdownAwareWriter {
            shutdown,
            wrote_before_shutdown: Rc::clone(&wrote_before_shutdown),
            bytes: Vec::new(),
        };
        let mut input = Cursor::new(framed_requests([read(1)]));

        assert!(serve(&mut input, &mut output, &mut backend).is_err());
        assert!(!wrote_before_shutdown.get());
        assert!(matches!(
            decoded_responses(&output.bytes)[0].result(),
            ResponseKind::Error {
                code: ErrorCode::InvalidState,
                ..
            }
        ));
    }

    #[derive(Default)]
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected output failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn response_write_failure_shuts_down_backend() {
        let mut input = Cursor::new(framed_requests([hello(1)]));
        let mut output = FailingWriter;
        let mut backend = TestBackend::default();
        let error = serve(&mut input, &mut output, &mut backend).unwrap_err();
        assert!(format!("{error:#}").contains("publish response 1"));
        assert_eq!(backend.shutdowns, 1);
    }

    #[test]
    fn native_hello_publish_failure_shuts_down_the_lazy_backend() {
        let shutdown = Rc::new(Cell::new(false));
        let probe = Rc::clone(&shutdown);
        let mut input = Cursor::new(framed_requests([hello(1)]));
        let mut output = FailingWriter;

        let error = serve_with_factory(
            &mut input,
            &mut output,
            ServerMode::Native,
            move || -> Result<OrderedBackend> {
                Ok(OrderedBackend {
                    shutdown: Rc::clone(&probe),
                })
            },
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("publish response 1"));
        assert!(shutdown.get());
    }

    struct FailAfterFrames {
        bytes: Vec<u8>,
        completed_frames: usize,
        allowed_frames: usize,
    }

    impl Write for FailAfterFrames {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.completed_frames >= self.allowed_frames {
                return Err(std::io::Error::other("injected terminal publish failure"));
            }
            self.bytes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.completed_frames += 1;
            Ok(())
        }
    }

    #[test]
    fn shutdown_and_terminal_publish_failures_are_combined() {
        let mut input = Cursor::new(framed_requests([
            hello(1),
            Request::new(2, RequestOperation::Shutdown {}),
        ]));
        let mut output = FailAfterFrames {
            bytes: Vec::new(),
            completed_frames: 0,
            allowed_frames: 1,
        };
        let mut backend = TestBackend {
            fail_shutdowns: 2,
            ..TestBackend::default()
        };

        let error = serve(&mut input, &mut output, &mut backend).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("injected shutdown failure"));
        assert!(diagnostic.contains("publish terminal response"));
        assert_eq!(backend.shutdowns, 2);
        let responses = decoded_responses(&output.bytes);
        assert_eq!(responses.len(), 1);
        assert!(matches!(responses[0].result(), ResponseKind::Hello { .. }));
    }
}
