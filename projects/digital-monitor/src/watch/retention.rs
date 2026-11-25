use super::{
    AtomicBool, Context, File, Instant, MetadataExt, Ordering, Path, Plan, ProtocolMetrics, Result,
    RetentionPriority, RetentionPruneReason, RetentionReason, RunState, TreeBudget, bail, ensure,
    fs, inspect_tree, journal, json, orchestrate, remove_quarantined_tree, save_run,
    sync_directory, tree_bytes,
};

pub(super) fn retention_reasons(metrics: &ProtocolMetrics) -> Vec<RetentionReason> {
    let mut reasons = Vec::new();
    if metrics.bluetooth.candidate_records > 0 {
        reasons.push(RetentionReason::BluetoothCandidateRecords);
    }
    if metrics
        .ieee802154
        .channels
        .iter()
        .flat_map(|channel| channel.sources.iter())
        .any(|source| source.candidate_records > 0)
    {
        reasons.push(RetentionReason::Ieee802154CandidateRecords);
    }
    let binary_fsk_label = orchestrate::BURST_FAMILY_LABELS[2];
    if metrics
        .burst_families
        .family_order
        .iter()
        .position(|label| label == binary_fsk_label)
        .and_then(|index| metrics.burst_families.families.get(index))
        .is_some_and(|family| family.group_count > 0)
    {
        reasons.push(RetentionReason::NarrowbandBinaryFskLikeGroups);
    }
    if metrics
        .wifi
        .decoder_health
        .ofdm
        .rejections
        .data_decode_failed
        > 0
    {
        reasons.push(RetentionReason::WifiOfdmDataDecodeFailures);
    }
    if metrics.wifi.decoder_health.ofdm.invalid_fcs > 0 {
        reasons.push(RetentionReason::WifiOfdmInvalidFcsFrames);
    }
    reasons
}

pub(super) fn prune_one(
    directory: &Path,
    sessions: &Path,
    quarantine: &Path,
    state: &mut RunState,
    log: &mut File,
    stop: &AtomicBool,
    prune_reason: RetentionPruneReason,
) -> Result<bool> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "interrupted before retention prune"
    );
    let Some((index, retention_priority)) = state.next_prune_candidate() else {
        return Ok(false);
    };
    let (session, retention_reasons) = {
        let attempt = state
            .attempts
            .iter()
            .find(|attempt| attempt.index == index)
            .context("retention index is absent")?;
        ensure!(
            attempt.succeeded()
                && attempt.retained
                && attempt.retention_priority == Some(retention_priority),
            "session is not prune eligible"
        );
        (attempt.session.clone(), attempt.retention_reasons.clone())
    };
    let source = directory.join(&session);
    ensure!(
        source.parent() == Some(sessions),
        "session is outside the managed session root"
    );
    let metadata = fs::symlink_metadata(&source)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "session is not an owned directory"
    );
    ensure!(
        metadata.dev() == fs::symlink_metadata(sessions)?.dev(),
        "session crosses a mount boundary"
    );
    let mut budget = TreeBudget::default();
    inspect_tree(&source, metadata.dev(), 0, &mut budget)?;
    journal(
        log,
        &json!({"event":"prune_intent","index":index,"session":session,
            "bytes":budget.bytes,"entries":budget.entries,
            "retention_priority":retention_priority,
            "retention_reasons":retention_reasons,
            "prune_reason":prune_reason}),
    )?;
    let transition = (|| -> Result<u64> {
        let target = quarantine.join(format!("session-{index:06}.deleting"));
        match fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect quarantine target"),
            Ok(_) => bail!("quarantine target already exists"),
        }
        fs::rename(&source, &target)?;
        sync_directory(sessions)?;
        sync_directory(quarantine)?;
        let moved = fs::symlink_metadata(&target)?;
        ensure!(
            moved.is_dir() && moved.dev() == metadata.dev() && moved.ino() == metadata.ino(),
            "quarantined session identity changed"
        );
        journal(log, &json!({"event":"prune_moved","index":index}))?;
        remove_quarantined_tree(&target)?;
        sync_directory(quarantine)?;
        journal(
            log,
            &json!({"event":"prune_completed","index":index,"deleted_entries":budget.entries}),
        )?;
        Ok(budget.entries)
    })();
    match transition {
        Ok(_) => {}
        Err(error) => {
            if let Err(journal_error) = journal(
                log,
                &json!({"event":"prune_failed","index":index,"error":format!("{error:#}")}),
            ) {
                return Err(error).context(format!(
                    "retention transition failed and its failure record could not be synced: {journal_error:#}"
                ));
            }
            return Err(error).context("retention transition failed after durable intent");
        }
    }
    let queue = match retention_priority {
        RetentionPriority::Routine => &mut state.retained_routine_successes,
        RetentionPriority::Interesting => &mut state.retained_interesting_successes,
    };
    let position = queue
        .iter()
        .position(|candidate| *candidate == index)
        .context("prune candidate disappeared from its retention queue")?;
    let removed = queue.remove(position);
    ensure!(
        removed == Some(index),
        "retention queue changed during prune"
    );
    state
        .attempts
        .iter_mut()
        .find(|attempt| attempt.index == index)
        .context("retention attempt disappeared")?
        .retained = false;
    Ok(true)
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep retention paths, the journal and shutdown control under separate borrows"
)]
pub(super) fn reserve_capture_capacity(
    directory: &Path,
    sessions: &Path,
    quarantine: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    stop: &AtomicBool,
    start: Instant,
) -> Result<bool> {
    let estimated = plan
        .samples
        .checked_mul(4)
        .context("capture byte overflow")?;
    loop {
        let should_prune = state.retained_success_count() > 1
            && tree_bytes(directory)?.saturating_add(estimated) > plan.retained_limit_bytes;
        if !should_prune {
            break;
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if !prune_one(
            directory,
            sessions,
            quarantine,
            state,
            log,
            stop,
            RetentionPruneReason::RetainedBytePressureBeforeCapture,
        )? {
            break;
        }
        save_run(
            directory,
            plan,
            state,
            start.elapsed().as_secs_f64(),
            false,
            None,
        )?;
    }
    Ok(tree_bytes(directory)?.saturating_add(estimated) <= plan.retained_limit_bytes)
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep retention paths, the journal and shutdown control under separate borrows"
)]
pub(super) fn prune_after_pipeline_attempt(
    directory: &Path,
    sessions: &Path,
    quarantine: &Path,
    plan: &Plan,
    state: &mut RunState,
    log: &mut File,
    stop: &AtomicBool,
    start: Instant,
) -> Result<Option<String>> {
    while state.retained_success_count() > 1
        && (state.retained_success_count() > plan.keep_sessions
            || tree_bytes(directory)? > plan.retained_limit_bytes)
    {
        if stop.load(Ordering::Relaxed) {
            return Ok(Some("interrupted".into()));
        }
        let prune_reason = if state.retained_success_count() > plan.keep_sessions {
            RetentionPruneReason::SuccessfulSessionCountLimit
        } else {
            RetentionPruneReason::RetainedByteLimitAfterAnalysis
        };
        if !prune_one(
            directory,
            sessions,
            quarantine,
            state,
            log,
            stop,
            prune_reason,
        )? {
            break;
        }
        save_run(
            directory,
            plan,
            state,
            start.elapsed().as_secs_f64(),
            false,
            None,
        )?;
    }
    if tree_bytes(directory)? > plan.retained_limit_bytes {
        return Ok(Some("retained_byte_threshold_after_analysis".into()));
    }
    Ok(None)
}
