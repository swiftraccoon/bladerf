//! Framed, role-fixed server for the device-free loader fixtures.
//!
//! The startup factory remains dormant until a complete, compatible hello has
//! been decoded. Every error is terminal. In particular, error handling never
//! calls orderly backend shutdown: a failed or unacknowledged image transition
//! may already have made the retained handle unsafe for another foreign call.

#![forbid(unsafe_code)]

use std::io::{Read, Write};

use anyhow::{Error, Result, anyhow};
use bladerf_profile2_loader_ipc::{
    BundleFacts, ErrorCode, LoaderState, MAX_ERROR_MESSAGE_BYTES, PROTOCOL_FAMILY,
    PROTOCOL_VERSION, Request, RequestOperation, Response, ResponseKind, ServerMode, read_frame,
    write_frame,
};

/// Fixed operations supplied by one primary or recovery fixture.
pub(crate) trait LoaderBackend {
    fn bundle(&self) -> &BundleFacts;
    fn load_profile2(&mut self) -> Result<()>;
    fn verify_profile2(&mut self) -> Result<()>;
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
            "; publish terminal loader response: {response_error}"
        ));
    }
    anyhow!(diagnostic)
}

fn write_response<W: Write>(output: &mut W, response: &Response) -> Result<()> {
    write_frame(output, response)
        .map_err(|error| anyhow!("publish loader response {}: {error}", response.id()))
}

fn bundle_matches_mode(mode: ServerMode, bundle: &BundleFacts) -> bool {
    match mode {
        ServerMode::FixturePrimary => bundle.custom().is_some(),
        ServerMode::FixtureRecovery => bundle.custom().is_none(),
    }
}

/// Serve one lazily constructed fake loader backend until shutdown or error.
pub(crate) fn serve_with_factory<R, W, B, F>(
    input: &mut R,
    output: &mut W,
    mode: ServerMode,
    backend_factory: F,
) -> Result<()>
where
    R: Read,
    W: Write,
    B: LoaderBackend,
    F: FnOnce() -> Result<B>,
{
    let mut backend_factory = Some(backend_factory);
    let mut backend = None;
    let mut state = None;
    let mut expected_id = 1u64;
    let mut handled = 0usize;

    loop {
        let request = match read_frame::<_, Request>(&mut *input) {
            Ok(Some(request)) => request,
            Ok(None) => return Err(anyhow!("loader input ended before explicit shutdown")),
            Err(error) => return Err(anyhow!(error)),
        };
        let (family, version, id, operation) = request.into_parts();
        if family != PROTOCOL_FAMILY {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("loader request family {family:?} differs from {PROTOCOL_FAMILY:?}"),
            ));
        }
        if version != PROTOCOL_VERSION {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!(
                    "loader request protocol version {version} differs from {PROTOCOL_VERSION}"
                ),
            ));
        }
        if id != expected_id {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::Protocol,
                anyhow!("loader request id {id} is not the expected id {expected_id}"),
            ));
        }
        if handled == mode.max_session_requests() {
            return Err(terminal_error(
                output,
                id,
                ErrorCode::RequestLimit,
                anyhow!(
                    "loader request limit {} reached for {mode:?}",
                    mode.max_session_requests()
                ),
            ));
        }

        if state.is_none() {
            let RequestOperation::Hello {
                mode: requested_mode,
            } = operation
            else {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("hello must be the first loader operation"),
                ));
            };
            if requested_mode != mode {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!(
                        "loader hello requested {requested_mode:?}, but this child is {mode:?}"
                    ),
                ));
            }
            let Some(factory) = backend_factory.take() else {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("loader backend factory was already consumed"),
                ));
            };
            let created = match factory() {
                Ok(created) => created,
                Err(error) => {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
            };
            if !bundle_matches_mode(mode, created.bundle()) {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::Backend,
                    anyhow!("qualified loader bundle does not match {mode:?}"),
                ));
            }
            let bundle = created.bundle().clone();
            backend = Some(created);
            state = Some(LoaderState::Ready);
            write_response(
                output,
                &Response::new(
                    id,
                    ResponseKind::Hello {
                        mode,
                        state: LoaderState::Ready,
                        bundle,
                    },
                ),
            )?;
            handled += 1;
            expected_id += 1;
            continue;
        }

        let current = state.expect("loader state is present after hello");
        let Some(active) = backend.as_mut() else {
            return Err(anyhow!("loader backend is unavailable after hello"));
        };
        let (next, shutdown) = match (mode, current, operation) {
            (_, _, RequestOperation::Hello { .. }) => {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("loader hello was already completed"),
                ));
            }
            (ServerMode::FixturePrimary, LoaderState::Ready, RequestOperation::LoadProfile2 {}) => {
                if let Err(error) = active.load_profile2() {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
                (LoaderState::CustomUnverified, false)
            }
            (
                ServerMode::FixturePrimary,
                LoaderState::CustomUnverified,
                RequestOperation::VerifyProfile2 {},
            ) => {
                if let Err(error) = active.verify_profile2() {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
                (LoaderState::CustomVerified, false)
            }
            (
                ServerMode::FixturePrimary,
                LoaderState::CustomVerified,
                RequestOperation::RestoreStock {},
            )
            | (
                ServerMode::FixtureRecovery,
                LoaderState::Ready,
                RequestOperation::RestoreStock {},
            ) => {
                if let Err(error) = active.restore_stock() {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
                (LoaderState::StockUnverified, false)
            }
            (_, LoaderState::StockUnverified, RequestOperation::VerifyStock {}) => {
                if let Err(error) = active.verify_stock() {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
                (LoaderState::StockVerified, false)
            }
            (_, LoaderState::Ready | LoaderState::StockVerified, RequestOperation::Shutdown {}) => {
                if let Err(error) = active.shutdown() {
                    return Err(terminal_error(output, id, ErrorCode::Backend, error));
                }
                (LoaderState::Closed, true)
            }
            (_, state, operation) => {
                return Err(terminal_error(
                    output,
                    id,
                    ErrorCode::InvalidState,
                    anyhow!("loader operation {operation:?} is invalid while state is {state:?}"),
                ));
            }
        };

        state = Some(next);
        write_response(
            output,
            &Response::new(id, ResponseKind::State { state: next }),
        )?;
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
    use std::io::{self, Cursor};
    use std::rc::Rc;

    use anyhow::bail;
    use bladerf_profile2_loader_ipc::{
        ImageFacts, LibraryFacts, REQUIRED_LIBRARY_MARKER, REQUIRED_LIBRARY_VERSION,
        REQUIRED_STOCK_BYTES, REQUIRED_STOCK_SHA256,
    };

    use super::*;

    const LIBRARY_SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const CUSTOM_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn bundle(mode: ServerMode) -> BundleFacts {
        BundleFacts::new(
            LibraryFacts::new(
                REQUIRED_LIBRARY_VERSION,
                LIBRARY_SHA256,
                REQUIRED_LIBRARY_MARKER,
            ),
            (mode == ServerMode::FixturePrimary).then(|| ImageFacts::new(CUSTOM_SHA256, 101)),
            ImageFacts::new(REQUIRED_STOCK_SHA256, REQUIRED_STOCK_BYTES),
        )
    }

    fn framed(requests: impl IntoIterator<Item = Request>) -> Vec<u8> {
        let mut bytes = Vec::new();
        for request in requests {
            write_frame(&mut bytes, &request).unwrap();
        }
        bytes
    }

    fn raw_frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn responses(mut bytes: &[u8]) -> Vec<Response> {
        let mut decoded = Vec::new();
        while let Some(response) = read_frame(&mut bytes).unwrap() {
            decoded.push(response);
        }
        decoded
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FailAt {
        Load,
        VerifyCustom,
        Restore,
        VerifyStock,
        Shutdown,
    }

    struct FakeBackend {
        bundle: BundleFacts,
        events: Rc<RefCell<Vec<&'static str>>>,
        fail_at: Option<FailAt>,
    }

    impl FakeBackend {
        fn new(mode: ServerMode, events: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                bundle: bundle(mode),
                events,
                fail_at: None,
            }
        }

        fn call(&self, event: &'static str, point: FailAt) -> Result<()> {
            self.events.borrow_mut().push(event);
            if self.fail_at == Some(point) {
                bail!("injected {event} failure");
            }
            Ok(())
        }
    }

    impl LoaderBackend for FakeBackend {
        fn bundle(&self) -> &BundleFacts {
            &self.bundle
        }

        fn load_profile2(&mut self) -> Result<()> {
            self.call("load-profile2", FailAt::Load)
        }

        fn verify_profile2(&mut self) -> Result<()> {
            self.call("verify-profile2", FailAt::VerifyCustom)
        }

        fn restore_stock(&mut self) -> Result<()> {
            self.call("restore-stock", FailAt::Restore)
        }

        fn verify_stock(&mut self) -> Result<()> {
            self.call("verify-stock", FailAt::VerifyStock)
        }

        fn shutdown(&mut self) -> Result<()> {
            self.call("shutdown", FailAt::Shutdown)
        }
    }

    fn hello(mode: ServerMode, id: u64) -> Request {
        Request::new(id, RequestOperation::Hello { mode })
    }

    #[test]
    fn backend_factory_stays_dormant_until_exact_hello() {
        let cases = [
            Vec::new(),
            raw_frame(
                br#"{"version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
            ),
            raw_frame(
                br#"{"family":"wrong","version":1,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
            ),
            raw_frame(
                br#"{"family":"bladerf_profile2_loader","version":2,"id":1,"operation":{"kind":"hello","mode":"fixture_primary"}}"#,
            ),
            framed([hello(ServerMode::FixtureRecovery, 1)]),
            framed([Request::new(1, RequestOperation::LoadProfile2 {})]),
            framed([hello(ServerMode::FixturePrimary, 2)]),
        ];
        for bytes in cases {
            let factory_calls = Rc::new(Cell::new(0));
            let observed = Rc::clone(&factory_calls);
            let mut input = Cursor::new(bytes);
            let mut output = Vec::new();
            let result = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixturePrimary,
                move || {
                    observed.set(observed.get() + 1);
                    Ok(FakeBackend::new(
                        ServerMode::FixturePrimary,
                        Rc::new(RefCell::new(Vec::new())),
                    ))
                },
            );
            assert!(result.is_err());
            assert_eq!(factory_calls.get(), 0);
        }
    }

    #[test]
    fn primary_and_recovery_transcripts_reach_only_exact_states() {
        let primary_events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&primary_events);
        let mut primary_input = Cursor::new(framed([
            hello(ServerMode::FixturePrimary, 1),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::VerifyProfile2 {}),
            Request::new(4, RequestOperation::RestoreStock {}),
            Request::new(5, RequestOperation::VerifyStock {}),
            Request::new(6, RequestOperation::Shutdown {}),
        ]));
        let mut primary_output = Vec::new();
        serve_with_factory(
            &mut primary_input,
            &mut primary_output,
            ServerMode::FixturePrimary,
            move || Ok(FakeBackend::new(ServerMode::FixturePrimary, observed)),
        )
        .unwrap();
        assert_eq!(
            primary_events.borrow().as_slice(),
            [
                "load-profile2",
                "verify-profile2",
                "restore-stock",
                "verify-stock",
                "shutdown"
            ]
        );
        let primary_states: Vec<_> = responses(&primary_output)
            .into_iter()
            .map(|response| match response.into_parts().3 {
                ResponseKind::Hello { state, .. } | ResponseKind::State { state } => state,
                ResponseKind::Error { .. } => panic!("primary transcript returned an error"),
            })
            .collect();
        assert_eq!(
            primary_states,
            [
                LoaderState::Ready,
                LoaderState::CustomUnverified,
                LoaderState::CustomVerified,
                LoaderState::StockUnverified,
                LoaderState::StockVerified,
                LoaderState::Closed,
            ]
        );

        let recovery_events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&recovery_events);
        let mut recovery_input = Cursor::new(framed([
            hello(ServerMode::FixtureRecovery, 1),
            Request::new(2, RequestOperation::RestoreStock {}),
            Request::new(3, RequestOperation::VerifyStock {}),
            Request::new(4, RequestOperation::Shutdown {}),
        ]));
        let mut recovery_output = Vec::new();
        serve_with_factory(
            &mut recovery_input,
            &mut recovery_output,
            ServerMode::FixtureRecovery,
            move || Ok(FakeBackend::new(ServerMode::FixtureRecovery, observed)),
        )
        .unwrap();
        assert_eq!(
            recovery_events.borrow().as_slice(),
            ["restore-stock", "verify-stock", "shutdown"]
        );
        let recovery_states: Vec<_> = responses(&recovery_output)
            .into_iter()
            .map(|response| match response.into_parts().3 {
                ResponseKind::Hello { state, .. } | ResponseKind::State { state } => state,
                ResponseKind::Error { .. } => panic!("recovery transcript returned an error"),
            })
            .collect();
        assert_eq!(
            recovery_states,
            [
                LoaderState::Ready,
                LoaderState::StockUnverified,
                LoaderState::StockVerified,
                LoaderState::Closed,
            ]
        );
    }

    #[test]
    fn illegal_or_failed_transition_is_terminal_without_shutdown() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        let mut illegal_input = Cursor::new(framed([
            hello(ServerMode::FixturePrimary, 1),
            Request::new(2, RequestOperation::VerifyProfile2 {}),
            Request::new(3, RequestOperation::Shutdown {}),
        ]));
        let mut illegal_output = Vec::new();
        assert!(
            serve_with_factory(
                &mut illegal_input,
                &mut illegal_output,
                ServerMode::FixturePrimary,
                move || { Ok(FakeBackend::new(ServerMode::FixturePrimary, observed,)) },
            )
            .is_err()
        );
        assert!(events.borrow().is_empty());
        assert!(matches!(
            responses(&illegal_output).last().unwrap().result(),
            ResponseKind::Error {
                code: ErrorCode::InvalidState,
                ..
            }
        ));

        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&events);
        let mut failed_input = Cursor::new(framed([
            hello(ServerMode::FixturePrimary, 1),
            Request::new(2, RequestOperation::LoadProfile2 {}),
            Request::new(3, RequestOperation::Shutdown {}),
        ]));
        let mut failed_output = Vec::new();
        assert!(
            serve_with_factory(
                &mut failed_input,
                &mut failed_output,
                ServerMode::FixturePrimary,
                move || {
                    let mut backend = FakeBackend::new(ServerMode::FixturePrimary, observed);
                    backend.fail_at = Some(FailAt::Load);
                    Ok(backend)
                },
            )
            .is_err()
        );
        assert_eq!(events.borrow().as_slice(), ["load-profile2"]);
        assert!(matches!(
            responses(&failed_output).last().unwrap().result(),
            ResponseKind::Error {
                code: ErrorCode::Backend,
                ..
            }
        ));
    }

    #[test]
    fn role_and_state_reject_every_representative_illegal_operation() {
        let cases = vec![
            (
                ServerMode::FixturePrimary,
                vec![RequestOperation::Hello {
                    mode: ServerMode::FixturePrimary,
                }],
                vec![],
            ),
            (
                ServerMode::FixturePrimary,
                vec![RequestOperation::RestoreStock {}],
                vec![],
            ),
            (
                ServerMode::FixturePrimary,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::Shutdown {},
                ],
                vec!["load-profile2"],
            ),
            (
                ServerMode::FixturePrimary,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::VerifyProfile2 {},
                    RequestOperation::VerifyStock {},
                ],
                vec!["load-profile2", "verify-profile2"],
            ),
            (
                ServerMode::FixturePrimary,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::VerifyProfile2 {},
                    RequestOperation::RestoreStock {},
                    RequestOperation::LoadProfile2 {},
                ],
                vec!["load-profile2", "verify-profile2", "restore-stock"],
            ),
            (
                ServerMode::FixtureRecovery,
                vec![RequestOperation::LoadProfile2 {}],
                vec![],
            ),
            (
                ServerMode::FixtureRecovery,
                vec![RequestOperation::VerifyProfile2 {}],
                vec![],
            ),
            (
                ServerMode::FixtureRecovery,
                vec![
                    RequestOperation::RestoreStock {},
                    RequestOperation::Shutdown {},
                ],
                vec!["restore-stock"],
            ),
            (
                ServerMode::FixtureRecovery,
                vec![
                    RequestOperation::RestoreStock {},
                    RequestOperation::VerifyStock {},
                    RequestOperation::VerifyStock {},
                ],
                vec!["restore-stock", "verify-stock"],
            ),
        ];

        for (mode, operations, expected_events) in cases {
            let mut requests = vec![hello(mode, 1)];
            requests.extend(
                operations
                    .into_iter()
                    .enumerate()
                    .map(|(index, operation)| Request::new(index as u64 + 2, operation)),
            );
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input = Cursor::new(framed(requests));
            let mut output = Vec::new();
            assert!(
                serve_with_factory(&mut input, &mut output, mode, move || {
                    Ok(FakeBackend::new(mode, observed))
                })
                .is_err()
            );
            assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
            assert!(!events.borrow().contains(&"shutdown"));
            assert!(matches!(
                responses(&output).last().unwrap().result(),
                ResponseKind::Error {
                    code: ErrorCode::InvalidState,
                    ..
                }
            ));
        }
    }

    #[test]
    fn every_backend_failure_stops_at_the_exact_call_without_shutdown_retry() {
        let cases = vec![
            (
                ServerMode::FixturePrimary,
                FailAt::Load,
                vec![RequestOperation::LoadProfile2 {}],
                vec!["load-profile2"],
            ),
            (
                ServerMode::FixturePrimary,
                FailAt::VerifyCustom,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::VerifyProfile2 {},
                ],
                vec!["load-profile2", "verify-profile2"],
            ),
            (
                ServerMode::FixturePrimary,
                FailAt::Restore,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::VerifyProfile2 {},
                    RequestOperation::RestoreStock {},
                ],
                vec!["load-profile2", "verify-profile2", "restore-stock"],
            ),
            (
                ServerMode::FixturePrimary,
                FailAt::VerifyStock,
                vec![
                    RequestOperation::LoadProfile2 {},
                    RequestOperation::VerifyProfile2 {},
                    RequestOperation::RestoreStock {},
                    RequestOperation::VerifyStock {},
                ],
                vec![
                    "load-profile2",
                    "verify-profile2",
                    "restore-stock",
                    "verify-stock",
                ],
            ),
            (
                ServerMode::FixturePrimary,
                FailAt::Shutdown,
                vec![RequestOperation::Shutdown {}],
                vec!["shutdown"],
            ),
            (
                ServerMode::FixtureRecovery,
                FailAt::Restore,
                vec![RequestOperation::RestoreStock {}],
                vec!["restore-stock"],
            ),
            (
                ServerMode::FixtureRecovery,
                FailAt::VerifyStock,
                vec![
                    RequestOperation::RestoreStock {},
                    RequestOperation::VerifyStock {},
                ],
                vec!["restore-stock", "verify-stock"],
            ),
        ];

        for (mode, fail_at, operations, expected_events) in cases {
            let mut requests = vec![hello(mode, 1)];
            requests.extend(
                operations
                    .into_iter()
                    .enumerate()
                    .map(|(index, operation)| Request::new(index as u64 + 2, operation)),
            );
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input = Cursor::new(framed(requests));
            let mut output = Vec::new();
            assert!(
                serve_with_factory(&mut input, &mut output, mode, move || {
                    let mut backend = FakeBackend::new(mode, observed);
                    backend.fail_at = Some(fail_at);
                    Ok(backend)
                })
                .is_err()
            );
            assert_eq!(events.borrow().as_slice(), expected_events.as_slice());
            assert_eq!(
                events
                    .borrow()
                    .iter()
                    .filter(|event| **event == "shutdown")
                    .count(),
                usize::from(fail_at == FailAt::Shutdown)
            );
            assert!(matches!(
                responses(&output).last().unwrap().result(),
                ResponseKind::Error {
                    code: ErrorCode::Backend,
                    ..
                }
            ));
        }
    }

    struct LoseResponse {
        frames: usize,
        fail_on_frame: usize,
    }

    impl Write for LoseResponse {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.frames == self.fail_on_frame {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected lost response",
                ));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.frames += 1;
            Ok(())
        }
    }

    #[test]
    fn every_lost_primary_response_exits_at_the_exact_call_prefix() {
        let expected_prefixes: &[&[&str]] = &[
            &[],
            &["load-profile2"],
            &["load-profile2", "verify-profile2"],
            &["load-profile2", "verify-profile2", "restore-stock"],
            &[
                "load-profile2",
                "verify-profile2",
                "restore-stock",
                "verify-stock",
            ],
            &[
                "load-profile2",
                "verify-profile2",
                "restore-stock",
                "verify-stock",
                "shutdown",
            ],
        ];
        for (fail_on_frame, expected) in expected_prefixes.iter().enumerate() {
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input = Cursor::new(framed([
                hello(ServerMode::FixturePrimary, 1),
                Request::new(2, RequestOperation::LoadProfile2 {}),
                Request::new(3, RequestOperation::VerifyProfile2 {}),
                Request::new(4, RequestOperation::RestoreStock {}),
                Request::new(5, RequestOperation::VerifyStock {}),
                Request::new(6, RequestOperation::Shutdown {}),
            ]));
            let mut output = LoseResponse {
                frames: 0,
                fail_on_frame,
            };
            let error = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixturePrimary,
                move || Ok(FakeBackend::new(ServerMode::FixturePrimary, observed)),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("lost response"));
            assert_eq!(events.borrow().as_slice(), *expected);
            if fail_on_frame < expected_prefixes.len() - 1 {
                assert!(!events.borrow().contains(&"shutdown"));
            }
        }
    }

    #[test]
    fn every_lost_recovery_response_exits_at_the_exact_call_prefix() {
        let expected_prefixes: &[&[&str]] = &[
            &[],
            &["restore-stock"],
            &["restore-stock", "verify-stock"],
            &["restore-stock", "verify-stock", "shutdown"],
        ];
        for (fail_on_frame, expected) in expected_prefixes.iter().enumerate() {
            let events = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&events);
            let mut input = Cursor::new(framed([
                hello(ServerMode::FixtureRecovery, 1),
                Request::new(2, RequestOperation::RestoreStock {}),
                Request::new(3, RequestOperation::VerifyStock {}),
                Request::new(4, RequestOperation::Shutdown {}),
            ]));
            let mut output = LoseResponse {
                frames: 0,
                fail_on_frame,
            };
            let error = serve_with_factory(
                &mut input,
                &mut output,
                ServerMode::FixtureRecovery,
                move || Ok(FakeBackend::new(ServerMode::FixtureRecovery, observed)),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("lost response"));
            assert_eq!(events.borrow().as_slice(), *expected);
            if fail_on_frame < expected_prefixes.len() - 1 {
                assert!(!events.borrow().contains(&"shutdown"));
            }
        }
    }
}
