use super::{
    AnalysisStep, AtomicBool, Attempt, CaptureStep, CompactSource, Context, Instant, Ordering,
    Path, Plan, Result, RetentionPriority, RetentionPruneReason, RunState, SourceReport, StepState,
    VecDeque, enforce_capture_center, journal, json, private_file, prune_one, report, save_run,
    tree_bytes, unix_ns,
};

pub(super) fn run_plan<C, A, V>(
    directory: &Path,
    plan: &Plan,
    stop: &AtomicBool,
    mut verify: V,
    mut capture: C,
    mut analyze: A,
) -> Result<bool>
where
    C: FnMut(u64, &Path, &AtomicBool) -> CaptureStep,
    A: FnMut(&SourceReport, &Path, &AtomicBool) -> AnalysisStep,
    V: FnMut() -> Result<()>,
{
    let start = Instant::now();
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
    save_run(directory, plan, &state, 0.0, false, None)?;

    'cycles: for index in 1..=usize::from(plan.cycles) {
        if stop.load(Ordering::Relaxed) {
            stop_reason = Some("interrupted".into());
            break;
        }
        if let Err(error) = verify() {
            stop_reason = Some(format!("program_identity_changed: {error:#}"));
            break;
        }
        if stop.load(Ordering::Relaxed) {
            stop_reason = Some("interrupted".into());
            break;
        }
        loop {
            let estimated = plan
                .samples
                .checked_mul(4)
                .context("capture byte overflow")?;
            let should_prune = state.retained_success_count() > 1
                && tree_bytes(directory)?.saturating_add(estimated) > plan.retained_limit_bytes;
            if !should_prune {
                break;
            }
            if stop.load(Ordering::Relaxed) {
                stop_reason = Some("interrupted".into());
                break 'cycles;
            }
            if !prune_one(
                directory,
                &sessions,
                &quarantine,
                &mut state,
                &mut log,
                stop,
                RetentionPruneReason::RetainedBytePressureBeforeCapture,
            )? {
                break;
            }
            save_run(
                directory,
                plan,
                &state,
                start.elapsed().as_secs_f64(),
                false,
                None,
            )?;
        }
        let estimated = plan
            .samples
            .checked_mul(4)
            .context("capture byte overflow")?;
        if tree_bytes(directory)?.saturating_add(estimated) > plan.retained_limit_bytes {
            stop_reason = Some("retained_byte_threshold_before_capture".into());
            break;
        }
        if stop.load(Ordering::Relaxed) {
            stop_reason = Some("interrupted".into());
            break;
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
        journal(&mut log, &started)?;

        let mut capture_step = capture(center_frequency_hz, &session, stop);
        enforce_capture_center(&mut capture_step, center_frequency_hz);
        let mut program_changed = false;
        if capture_step.state == StepState::Complete
            && let Err(error) = verify()
        {
            capture_step.state = StepState::Failed;
            capture_step.detail =
                Some(format!("program identity changed after capture: {error:#}"));
            program_changed = true;
        }
        let mut analysis_step = if capture_step.state == StepState::Complete
            && !stop.load(Ordering::Relaxed)
        {
            analyze(
                capture_step
                    .source
                    .as_ref()
                    .context("complete capture lacks source")?,
                &session,
                stop,
            )
        } else if stop.load(Ordering::Relaxed) || capture_step.state == StepState::Interrupted {
            AnalysisStep::not_started("interrupted after capture; offline analysis was not started")
        } else {
            AnalysisStep::not_started("capture was not accepted")
        };
        if analysis_step.state != StepState::NotStarted
            && let Err(error) = verify()
        {
            analysis_step.state = StepState::Failed;
            analysis_step.detail = Some(format!(
                "program identity changed after analysis: {error:#}"
            ));
            program_changed = true;
        }
        if analysis_step.state == StepState::Complete {
            let observed = analysis_step
                .compact
                .as_ref()
                .and_then(|analysis| analysis.metrics.as_ref())
                .map(|metrics| metrics.bluetooth.decoder_program.clone())
                .context("complete analysis lacks nested Bluetooth decoder provenance")?;
            if let Some(expected) = &state.nested_bluetooth_decoder {
                if observed != *expected {
                    analysis_step.state = StepState::Failed;
                    analysis_step.detail = Some(
                        "nested Blue Dragon content identity changed between watch attempts".into(),
                    );
                    program_changed = true;
                }
            } else {
                state.nested_bluetooth_decoder = Some(observed);
            }
        }
        let interrupted = stop.load(Ordering::Relaxed)
            || capture_step.state == StepState::Interrupted
            || analysis_step.state == StepState::Interrupted;
        let mut attempt = Attempt {
            index,
            session: format!("sessions/{name}"),
            center_frequency_hz: plan.reported_attempt_center(center_frequency_hz),
            started_unix_ns,
            ended_unix_ns: Some(unix_ns()?),
            start_offset_seconds,
            end_offset_seconds: Some(start.elapsed().as_secs_f64()),
            capture_ended_unix_ns: None,
            capture_end_offset_seconds: None,
            analysis_started_unix_ns: None,
            analysis_start_offset_seconds: None,
            analysis_ended_unix_ns: None,
            analysis_end_offset_seconds: None,
            phase: None,
            capture_state: capture_step.state,
            analysis_state: analysis_step.state,
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
            analysis: analysis_step.compact,
            capture_detail: capture_step.detail,
            analysis_detail: analysis_step.detail,
        };
        attempt.classify_retention();
        let succeeded = attempt.succeeded();
        let retention_priority = attempt.retention_priority;
        journal(
            &mut log,
            &json!({"event":"attempt_finished","attempt":attempt}),
        )?;
        state.attempts.push(attempt);
        if succeeded {
            consecutive_failures = 0;
            match retention_priority.context("successful attempt lacks retention priority")? {
                RetentionPriority::Routine => {
                    state.retained_routine_successes.push_back(index);
                }
                RetentionPriority::Interesting => {
                    state.retained_interesting_successes.push_back(index);
                }
            }
        } else if !interrupted {
            consecutive_failures += 1;
        }
        save_run(
            directory,
            plan,
            &state,
            start.elapsed().as_secs_f64(),
            false,
            None,
        )?;

        if program_changed {
            stop_reason = Some("program_identity_changed".into());
            break;
        }
        if interrupted {
            stop_reason = Some("interrupted".into());
            break;
        }
        if stop.load(Ordering::Relaxed) {
            stop_reason = Some("interrupted".into());
            break;
        }
        while state.retained_success_count() > 1
            && (state.retained_success_count() > plan.keep_sessions
                || tree_bytes(directory)? > plan.retained_limit_bytes)
        {
            if stop.load(Ordering::Relaxed) {
                stop_reason = Some("interrupted".into());
                break 'cycles;
            }
            let prune_reason = if state.retained_success_count() > plan.keep_sessions {
                RetentionPruneReason::SuccessfulSessionCountLimit
            } else {
                RetentionPruneReason::RetainedByteLimitAfterAnalysis
            };
            if !prune_one(
                directory,
                &sessions,
                &quarantine,
                &mut state,
                &mut log,
                stop,
                prune_reason,
            )? {
                break;
            }
            save_run(
                directory,
                plan,
                &state,
                start.elapsed().as_secs_f64(),
                false,
                None,
            )?;
        }
        if tree_bytes(directory)? > plan.retained_limit_bytes {
            stop_reason = Some("retained_byte_threshold_after_analysis".into());
            break;
        }
        if consecutive_failures >= plan.max_consecutive_failures {
            stop_reason = Some("consecutive_failure_limit_reached".into());
            break;
        }
    }

    if stop_reason.is_none()
        && !stop.load(Ordering::Relaxed)
        && let Err(error) = verify()
    {
        stop_reason = Some(format!("program_identity_changed: {error:#}"));
    }
    if stop.load(Ordering::Relaxed) && stop_reason.is_none() {
        stop_reason = Some("interrupted".into());
    }
    save_run(
        directory,
        plan,
        &state,
        start.elapsed().as_secs_f64(),
        true,
        stop_reason.as_deref(),
    )?;
    let successful = stop_reason.is_none()
        && state.attempts.len() == usize::from(plan.cycles)
        && state.attempts.iter().all(Attempt::succeeded);
    Ok(successful)
}
