use super::{
    AnalysisCompletion, AnalysisJob, AnalysisStep, AnalysisWorkerEvent, AtomicBool, Attempt,
    AttemptPhase, CaptureStep, CompactSource, Context, File, Instant, Ordering, Path, PathBuf,
    Plan, Receiver, Result, RetentionPriority, RunState, SourceReport, StepState, SyncSender,
    VecDeque, analysis_worker, channel, enforce_capture_center, ensure, failed_analysis, journal,
    json, private_file, prune_after_pipeline_attempt, report, reserve_capture_capacity, save_run,
    sync_channel, thread, unix_ns,
};

pub(super) struct PendingCapture {
    pub(super) index: usize,
    pub(super) session: PathBuf,
    pub(super) source: Option<SourceReport>,
    pub(super) capture_state: StepState,
    pub(super) interrupted: bool,
    pub(super) program_changed: bool,
}

pub(super) struct TerminalAttempt {
    pub(super) succeeded: bool,
    pub(super) interrupted: bool,
    pub(super) program_changed: bool,
    pub(super) worker_failed: bool,
}

pub(super) struct PipelineCoordinatorOutcome<V> {
    pub(super) state: RunState,
    pub(super) stop_reason: Option<String>,
    pub(super) verify: V,
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep capture callbacks and coordinator journal state under separate borrows"
)]
pub(super) fn capture_for_pipeline<C, V>(
    directory: &Path,
    sessions: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    stop: &AtomicBool,
    start: Instant,
    index: usize,
    verify: &mut V,
    capture: &mut C,
) -> Result<std::result::Result<PendingCapture, String>>
where
    C: FnMut(u64, &Path, &AtomicBool) -> CaptureStep,
    V: FnMut() -> Result<()>,
{
    if stop.load(Ordering::Relaxed) {
        return Ok(Err("interrupted".into()));
    }
    if let Err(error) = verify() {
        return Ok(Err(format!("program_identity_changed: {error:#}")));
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(Err("interrupted".into()));
    }

    let name = format!("{index:06}");
    let session = sessions.join(&name);
    report::private_directory(&session)?;
    report::private_directory(&session.join("logs"))?;
    let started_unix_ns = unix_ns()?;
    let start_offset_seconds = start.elapsed().as_secs_f64();
    let center_frequency_hz = plan.center_frequency_hz_for_attempt(index);
    let mut started = json!({"event":"attempt_started","index":index,
        "session":format!("sessions/{name}"),
        "start_offset_seconds":start_offset_seconds});
    if plan.has_multiple_centers() {
        started["center_frequency_hz"] = json!(center_frequency_hz);
    }
    journal(log, &started)?;

    let mut capture_step = capture(center_frequency_hz, &session, stop);
    enforce_capture_center(&mut capture_step, center_frequency_hz);
    let mut program_changed = false;
    if capture_step.state == StepState::Complete
        && let Err(error) = verify()
    {
        capture_step.state = StepState::Failed;
        capture_step.detail = Some(format!("program identity changed after capture: {error:#}"));
        program_changed = true;
    }
    let capture_ended_unix_ns = unix_ns()?;
    let capture_end_offset_seconds = start.elapsed().as_secs_f64();
    let interrupted = stop.load(Ordering::Relaxed) || capture_step.state == StepState::Interrupted;
    let analysis_detail = if interrupted {
        "interrupted after capture; offline analysis awaits coordinator shutdown"
    } else if capture_step.state == StepState::Complete {
        "capture is durable; offline analysis is pending"
    } else {
        "capture was not accepted"
    };
    let source = capture_step.source.clone();
    let attempt = Attempt {
        index,
        session: format!("sessions/{name}"),
        center_frequency_hz: plan.reported_attempt_center(center_frequency_hz),
        started_unix_ns,
        ended_unix_ns: None,
        start_offset_seconds,
        end_offset_seconds: None,
        capture_ended_unix_ns: Some(capture_ended_unix_ns),
        capture_end_offset_seconds: Some(capture_end_offset_seconds),
        analysis_started_unix_ns: None,
        analysis_start_offset_seconds: None,
        analysis_ended_unix_ns: None,
        analysis_end_offset_seconds: None,
        phase: Some(AttemptPhase::CaptureFinished),
        capture_state: capture_step.state,
        analysis_state: StepState::NotStarted,
        interrupted,
        retained: true,
        retention_priority: None,
        retention_reasons: Vec::new(),
        source: capture_step
            .source
            .as_ref()
            .map(|source| CompactSource::from_source(source, plan.has_multiple_centers())),
        capture_termination: capture_step.termination,
        capture_diagnostics: capture_step.diagnostics,
        analysis: None,
        capture_detail: capture_step.detail,
        analysis_detail: Some(analysis_detail.into()),
    };
    journal(log, &json!({"event":"capture_finished","attempt":attempt}))?;
    state.attempts.push(attempt);
    save_run(
        directory,
        plan,
        state,
        start.elapsed().as_secs_f64(),
        false,
        None,
    )?;
    Ok(Ok(PendingCapture {
        index,
        session,
        source,
        capture_state: capture_step.state,
        interrupted,
        program_changed,
    }))
}

#[allow(
    clippy::too_many_arguments,
    reason = "coordinate the journal and worker handshake with separately borrowed handles"
)]
pub(super) fn begin_pipeline_analysis(
    directory: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    start: Instant,
    jobs: &SyncSender<AnalysisJob>,
    events: &Receiver<AnalysisWorkerEvent>,
    pending: &PendingCapture,
) -> Result<bool> {
    let source = pending
        .source
        .clone()
        .context("complete pipeline capture lacks source")?;
    let (release_tx, release_rx) = sync_channel(0);
    if jobs
        .send(AnalysisJob {
            index: pending.index,
            source,
            session: pending.session.clone(),
            release: release_rx,
        })
        .is_err()
    {
        return Ok(false);
    }
    let (unix_ns, offset_seconds) = match events.recv() {
        Ok(AnalysisWorkerEvent::Started {
            index,
            unix_ns,
            offset_seconds,
        }) => {
            if index != pending.index {
                return Ok(false);
            }
            (unix_ns, offset_seconds)
        }
        Ok(AnalysisWorkerEvent::Finished(_)) => return Ok(false),
        Err(_) => return Ok(false),
    };
    let attempt = state
        .attempts
        .iter_mut()
        .find(|attempt| attempt.index == pending.index)
        .context("pending pipeline attempt disappeared before analysis")?;
    ensure!(
        attempt.phase == Some(AttemptPhase::CaptureFinished),
        "pipeline analysis did not start from capture-finished state"
    );
    attempt.phase = Some(AttemptPhase::AnalysisRunning);
    attempt.analysis_state = StepState::InProgress;
    attempt.analysis_started_unix_ns = Some(unix_ns);
    attempt.analysis_start_offset_seconds = Some(offset_seconds);
    attempt.analysis_detail = Some("offline analysis is running".into());
    journal(
        log,
        &json!({"event":"analysis_started","index":pending.index,
            "analysis_started_unix_ns":unix_ns,
            "analysis_start_offset_seconds":offset_seconds}),
    )?;
    save_run(
        directory,
        plan,
        state,
        start.elapsed().as_secs_f64(),
        false,
        None,
    )?;
    Ok(release_tx.send(()).is_ok())
}

pub(super) fn finish_pipeline_attempt(
    directory: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    _stop: &AtomicBool,
    start: Instant,
    completion: AnalysisCompletion,
) -> Result<TerminalAttempt> {
    let mut step = completion.step;
    let mut program_changed = completion.program_changed;
    if step.state == StepState::Complete {
        let observed = step
            .compact
            .as_ref()
            .and_then(|analysis| analysis.metrics.as_ref())
            .map(|metrics| metrics.bluetooth.decoder_program.clone())
            .context("complete analysis lacks nested Bluetooth decoder provenance")?;
        if let Some(expected) = &state.nested_bluetooth_decoder {
            if observed != *expected {
                step.state = StepState::Failed;
                step.detail = Some(
                    "nested Blue Dragon content identity changed between watch attempts".into(),
                );
                program_changed = true;
            }
        } else {
            state.nested_bluetooth_decoder = Some(observed);
        }
    }

    let attempt = state
        .attempts
        .iter_mut()
        .find(|attempt| attempt.index == completion.index)
        .context("pending pipeline attempt disappeared")?;
    ensure!(
        matches!(
            attempt.phase,
            Some(AttemptPhase::CaptureFinished | AttemptPhase::AnalysisRunning)
        ),
        "pipeline attempt was already terminal"
    );
    let analysis_was_started = attempt.phase == Some(AttemptPhase::AnalysisRunning);
    let interrupted = completion.interrupted
        || attempt.capture_state == StepState::Interrupted
        || step.state == StepState::Interrupted;
    attempt.analysis_state = step.state;
    attempt.analysis = step.compact;
    attempt.analysis_detail = step.detail;
    attempt.analysis_ended_unix_ns = completion.analysis_ended_unix_ns;
    attempt.analysis_end_offset_seconds = completion.analysis_end_offset_seconds;
    attempt.interrupted = interrupted;
    attempt.ended_unix_ns = Some(unix_ns()?);
    attempt.end_offset_seconds = Some(start.elapsed().as_secs_f64());
    attempt.phase = Some(AttemptPhase::Finished);
    attempt.classify_retention();
    let succeeded = attempt.succeeded();
    let retention_priority = attempt.retention_priority;
    if analysis_was_started {
        journal(
            log,
            &json!({"event":"analysis_finished","index":completion.index,
                "analysis_state":attempt.analysis_state,
                "analysis_ended_unix_ns":attempt.analysis_ended_unix_ns,
                "analysis_end_offset_seconds":attempt.analysis_end_offset_seconds}),
        )?;
    } else {
        journal(
            log,
            &json!({"event":"analysis_skipped","index":completion.index,
                "analysis_state":attempt.analysis_state,
                "detail":attempt.analysis_detail}),
        )?;
    }
    journal(log, &json!({"event":"attempt_finished","attempt":attempt}))?;
    if succeeded {
        match retention_priority.context("successful attempt lacks retention priority")? {
            RetentionPriority::Routine => {
                state.retained_routine_successes.push_back(completion.index)
            }
            RetentionPriority::Interesting => state
                .retained_interesting_successes
                .push_back(completion.index),
        }
    }
    save_run(
        directory,
        plan,
        state,
        start.elapsed().as_secs_f64(),
        false,
        None,
    )?;
    Ok(TerminalAttempt {
        succeeded,
        interrupted,
        program_changed,
        worker_failed: completion.worker_failed,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep coordinator journal state and the failed attempt evidence distinct"
)]
pub(super) fn finish_pipeline_without_analysis(
    directory: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    stop: &AtomicBool,
    start: Instant,
    pending: &PendingCapture,
    detail: impl Into<String>,
) -> Result<TerminalAttempt> {
    finish_pipeline_attempt(
        directory,
        plan,
        state,
        log,
        stop,
        start,
        AnalysisCompletion {
            index: pending.index,
            step: AnalysisStep::not_started(detail),
            analysis_ended_unix_ns: None,
            analysis_end_offset_seconds: None,
            interrupted: pending.interrupted || stop.load(Ordering::Relaxed),
            program_changed: pending.program_changed,
            worker_failed: false,
        },
    )
}

pub(super) fn overlap_capture_admissible(
    consecutive_failures: u16,
    unresolved_attempts: u16,
    maximum_failures: u16,
) -> bool {
    consecutive_failures
        .saturating_add(unresolved_attempts)
        .saturating_add(1)
        <= maximum_failures
}

pub(super) fn disconnected_analysis(index: usize, detail: impl Into<String>) -> AnalysisCompletion {
    AnalysisCompletion {
        index,
        step: failed_analysis(detail),
        analysis_ended_unix_ns: None,
        analysis_end_offset_seconds: None,
        interrupted: false,
        program_changed: false,
        worker_failed: true,
    }
}

pub(super) fn receive_pipeline_analysis(
    events: &Receiver<AnalysisWorkerEvent>,
    expected_index: usize,
) -> AnalysisCompletion {
    match events.recv() {
        Ok(AnalysisWorkerEvent::Finished(completion)) if completion.index == expected_index => {
            *completion
        }
        Ok(AnalysisWorkerEvent::Finished(completion)) => disconnected_analysis(
            expected_index,
            format!(
                "analysis worker violated FIFO completion order: expected {expected_index}, got {}",
                completion.index
            ),
        ),
        Ok(AnalysisWorkerEvent::Started { index, .. }) => disconnected_analysis(
            expected_index,
            format!("analysis worker emitted an unexpected start event for attempt {index}"),
        ),
        Err(_) => disconnected_analysis(expected_index, "offline analysis worker disconnected"),
    }
}

pub(super) fn update_failure_streak(terminal: &TerminalAttempt, consecutive_failures: &mut u16) {
    if terminal.succeeded {
        *consecutive_failures = 0;
    } else if !terminal.interrupted {
        *consecutive_failures = consecutive_failures.saturating_add(1);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "take independently owned capture hooks and worker channels"
)]
pub(super) fn run_pipelined_coordinator<C, V>(
    directory: &Path,
    plan: &Plan,
    stop: &AtomicBool,
    start: Instant,
    mut verify: V,
    mut capture: C,
    jobs: &SyncSender<AnalysisJob>,
    events: &Receiver<AnalysisWorkerEvent>,
) -> Result<PipelineCoordinatorOutcome<V>>
where
    C: FnMut(u64, &Path, &AtomicBool) -> CaptureStep,
    V: FnMut() -> Result<()>,
{
    let sessions = directory.join("sessions");
    let quarantine = directory.join("quarantine");
    report::private_directory(&sessions)?;
    report::private_directory(&quarantine)?;
    let mut log = private_file(&directory.join("attempts.jsonl"))?;
    let mut state = RunState {
        attempts: Vec::new(),
        retained_routine_successes: VecDeque::new(),
        retained_interesting_successes: VecDeque::new(),
        nested_bluetooth_decoder: None,
    };
    let mut stop_reason: Option<String> = None;
    let mut consecutive_failures = 0u16;
    let mut next_index = 1usize;
    let mut pending: Option<PendingCapture> = None;
    save_run(directory, plan, &state, 0.0, false, None)?;

    while pending.is_some() || next_index <= usize::from(plan.cycles) {
        if pending.is_none() {
            if stop.load(Ordering::Relaxed) {
                stop_reason = Some("interrupted".into());
                break;
            }
            if !reserve_capture_capacity(
                directory,
                &sessions,
                &quarantine,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
            )? {
                stop_reason = Some(if stop.load(Ordering::Relaxed) {
                    "interrupted".into()
                } else {
                    "retained_byte_threshold_before_capture".into()
                });
                break;
            }
            match capture_for_pipeline(
                directory,
                &sessions,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
                next_index,
                &mut verify,
                &mut capture,
            )? {
                Ok(captured) => {
                    pending = Some(captured);
                    next_index += 1;
                }
                Err(reason) => {
                    stop_reason = Some(reason);
                    break;
                }
            }
        }

        let current = pending.take().context("pipeline lost pending capture")?;
        if current.capture_state != StepState::Complete
            || current.interrupted
            || current.program_changed
        {
            let detail = if current.interrupted {
                "interrupted after capture; offline analysis was not started"
            } else if current.program_changed {
                "program identity changed after capture; offline analysis was not started"
            } else {
                "capture was not accepted"
            };
            let terminal = finish_pipeline_without_analysis(
                directory, plan, &mut state, &mut log, stop, start, &current, detail,
            )?;
            update_failure_streak(&terminal, &mut consecutive_failures);
            if terminal.program_changed {
                stop_reason = Some("program_identity_changed".into());
            } else if terminal.interrupted || stop.load(Ordering::Relaxed) {
                stop_reason = Some("interrupted".into());
            } else if let Some(reason) = prune_after_pipeline_attempt(
                directory,
                &sessions,
                &quarantine,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
            )? {
                stop_reason = Some(reason);
            } else if consecutive_failures >= plan.max_consecutive_failures {
                stop_reason = Some("consecutive_failure_limit_reached".into());
            }
            if stop_reason.is_some() {
                break;
            }
            continue;
        }

        let overlap_failure_safe =
            overlap_capture_admissible(consecutive_failures, 1, plan.max_consecutive_failures);
        let has_later_cycle = next_index <= usize::from(plan.cycles);
        let overlap_capacity = if has_later_cycle && overlap_failure_safe {
            reserve_capture_capacity(
                directory,
                &sessions,
                &quarantine,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
            )?
        } else {
            false
        };
        if stop.load(Ordering::Relaxed) {
            let terminal = finish_pipeline_without_analysis(
                directory,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
                &current,
                "interrupted before offline analysis started",
            )?;
            update_failure_streak(&terminal, &mut consecutive_failures);
            stop_reason = Some("interrupted".into());
            break;
        }

        let analysis_started = begin_pipeline_analysis(
            directory, plan, &mut state, &mut log, start, jobs, events, &current,
        )?;
        if !analysis_started {
            let terminal = finish_pipeline_attempt(
                directory,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
                disconnected_analysis(current.index, "offline analysis worker failed to start"),
            )?;
            update_failure_streak(&terminal, &mut consecutive_failures);
            stop_reason = Some("analysis_worker_failed".into());
            break;
        }

        let mut later_pending = None;
        let mut capture_stop_reason = None;
        if has_later_cycle && overlap_failure_safe && overlap_capacity {
            match capture_for_pipeline(
                directory,
                &sessions,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
                next_index,
                &mut verify,
                &mut capture,
            )? {
                Ok(captured) => {
                    later_pending = Some(captured);
                    next_index += 1;
                }
                Err(reason) => capture_stop_reason = Some(reason),
            }
        }

        let completion = receive_pipeline_analysis(events, current.index);
        let terminal = finish_pipeline_attempt(
            directory, plan, &mut state, &mut log, stop, start, completion,
        )?;
        update_failure_streak(&terminal, &mut consecutive_failures);
        pending = later_pending;

        if terminal.worker_failed {
            stop_reason = Some("analysis_worker_failed".into());
        } else if terminal.program_changed {
            stop_reason = Some("program_identity_changed".into());
        } else if terminal.interrupted || stop.load(Ordering::Relaxed) {
            stop_reason = Some("interrupted".into());
        } else if let Some(reason) = capture_stop_reason {
            stop_reason = Some(reason);
        } else if pending
            .as_ref()
            .is_some_and(|attempt| attempt.program_changed)
        {
            stop_reason = Some("program_identity_changed".into());
        } else if pending.as_ref().is_some_and(|attempt| attempt.interrupted) {
            stop_reason = Some("interrupted".into());
        } else if pending
            .as_ref()
            .is_none_or(|attempt| attempt.capture_state == StepState::Complete)
            && let Some(reason) = prune_after_pipeline_attempt(
                directory,
                &sessions,
                &quarantine,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
            )?
        {
            stop_reason = Some(reason);
        } else if consecutive_failures >= plan.max_consecutive_failures {
            stop_reason = Some("consecutive_failure_limit_reached".into());
        }

        if let Some(reason) = stop_reason.as_deref()
            && let Some(later) = pending.take()
        {
            let terminal = finish_pipeline_without_analysis(
                directory,
                plan,
                &mut state,
                &mut log,
                stop,
                start,
                &later,
                format!("watch stopped before offline analysis: {reason}"),
            )?;
            update_failure_streak(&terminal, &mut consecutive_failures);
        }
        if stop_reason.is_some() {
            break;
        }
    }

    Ok(PipelineCoordinatorOutcome {
        state,
        stop_reason,
        verify,
    })
}

pub(super) fn run_pipelined_plan<C, A, CV, WV>(
    directory: &Path,
    plan: &Plan,
    stop: &AtomicBool,
    coordinator_verify: CV,
    worker_verify: WV,
    capture: C,
    analyze: A,
) -> Result<bool>
where
    C: FnMut(u64, &Path, &AtomicBool) -> CaptureStep,
    A: FnMut(&SourceReport, &Path, &AtomicBool) -> AnalysisStep + Send,
    CV: FnMut() -> Result<()>,
    WV: FnMut() -> Result<()> + Send,
{
    ensure!(
        plan.pending_captures == 1,
        "pipeline runner requires one pending capture"
    );
    let start = Instant::now();
    let (job_tx, job_rx) = sync_channel(0);
    let (event_tx, event_rx) = channel();
    thread::scope(|scope| -> Result<bool> {
        let worker = thread::Builder::new()
            .name("digital-monitor-analysis".into())
            .spawn_scoped(scope, move || {
                analysis_worker(stop, start, job_rx, event_tx, worker_verify, analyze);
            })
            .context("spawn offline analysis worker")?;
        let result = run_pipelined_coordinator(
            directory,
            plan,
            stop,
            start,
            coordinator_verify,
            capture,
            &job_tx,
            &event_rx,
        );
        drop(job_tx);
        let joined = worker.join();
        let mut outcome = result?;
        if joined.is_err() {
            outcome.stop_reason = Some("analysis_worker_failed".into());
        }
        if outcome.stop_reason.is_none()
            && !stop.load(Ordering::Relaxed)
            && let Err(error) = (outcome.verify)()
        {
            outcome.stop_reason = Some(format!("program_identity_changed: {error:#}"));
        }
        if stop.load(Ordering::Relaxed) && outcome.stop_reason.is_none() {
            outcome.stop_reason = Some("interrupted".into());
        }
        save_run(
            directory,
            plan,
            &outcome.state,
            start.elapsed().as_secs_f64(),
            true,
            outcome.stop_reason.as_deref(),
        )?;
        Ok(outcome.stop_reason.is_none()
            && outcome.state.attempts.len() == usize::from(plan.cycles)
            && outcome.state.attempts.iter().all(Attempt::succeeded))
    })
}
