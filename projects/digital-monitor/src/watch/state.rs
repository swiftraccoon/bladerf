use super::{
    Attempt, Context, File, NestedDecoderProgram, Path, Plan, RX_CHANNEL, Result,
    RetentionPriority, SAMPLE_RATE_HZ, STOCK_FPGA_VERSION, SystemTime, UNIX_EPOCH, Value, VecDeque,
    Write, ensure, fs, json, private_file, sync_directory,
};

pub(super) struct RunState {
    pub(super) attempts: Vec<Attempt>,
    pub(super) retained_routine_successes: VecDeque<usize>,
    pub(super) retained_interesting_successes: VecDeque<usize>,
    pub(super) nested_bluetooth_decoder: Option<NestedDecoderProgram>,
}

impl RunState {
    pub(super) fn retained_success_count(&self) -> usize {
        self.retained_routine_successes.len() + self.retained_interesting_successes.len()
    }

    pub(super) fn retained_successes_at_center(&self, center_frequency_hz: u64) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| {
                attempt.retained
                    && attempt.succeeded()
                    && attempt.center_frequency_hz == Some(center_frequency_hz)
            })
            .count()
    }

    pub(super) fn prune_preserves_center(&self, index: usize) -> bool {
        let Some(center_frequency_hz) = self
            .attempts
            .iter()
            .find(|attempt| attempt.index == index)
            .and_then(|attempt| attempt.center_frequency_hz)
        else {
            return true;
        };
        self.retained_successes_at_center(center_frequency_hz) > 1
    }

    pub(super) fn next_prune_candidate(&self) -> Option<(usize, RetentionPriority)> {
        self.retained_routine_successes
            .iter()
            .find(|index| self.prune_preserves_center(**index))
            .copied()
            .map(|index| (index, RetentionPriority::Routine))
            .or_else(|| {
                self.retained_interesting_successes
                    .iter()
                    .find(|index| self.prune_preserves_center(**index))
                    .copied()
                    .map(|index| (index, RetentionPriority::Interesting))
            })
    }
}

pub(super) fn snapshot(directory: &Path, value: &Value) -> Result<()> {
    let stage = directory.join(".run.json.next");
    let final_path = directory.join("run.json");
    match fs::symlink_metadata(&final_path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "managed run snapshot is not a regular file"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect managed run snapshot"),
    }
    let mut file = private_file(&stage)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&stage, &final_path)?;
    sync_directory(directory)
}

pub(super) fn journal(file: &mut File, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn unix_ns() -> Result<u64> {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())
        .context("host time exceeds u64 nanoseconds")
}

pub(super) fn save_run(
    directory: &Path,
    plan: &Plan,
    state: &RunState,
    elapsed_seconds: f64,
    finished: bool,
    stop_reason: Option<&str>,
) -> Result<()> {
    let interrupted = stop_reason == Some("interrupted")
        || state.attempts.iter().any(|attempt| attempt.interrupted);
    let attempts_completed = state
        .attempts
        .iter()
        .filter(|attempt| attempt.is_terminal())
        .count();
    let plan_completed = finished
        && !interrupted
        && stop_reason.is_none()
        && attempts_completed == usize::from(plan.cycles);
    if plan.bluetooth_coded_shadow {
        for attempt in state.attempts.iter().filter(|attempt| attempt.succeeded()) {
            ensure!(
                (1..=usize::from(plan.cycles)).contains(&attempt.index),
                "successful coded-shadow attempt index is outside its finite plan"
            );
            let center_frequency_hz = plan.center_frequency_hz_for_attempt(attempt.index);
            ensure!(
                attempt.coded_shadow_matches_center(center_frequency_hz),
                "successful attempt {} lacks the coded-shadow result required for its configured center",
                attempt.index
            );
        }
    }
    let successful = plan_completed && state.attempts.iter().all(Attempt::succeeded);
    let mut value = json!({
        "schema_version":if plan.bluetooth_coded_shadow { 5 } else if plan.has_multiple_centers() { 4 } else if plan.pending_captures == 0 { 2 } else { 3 },
        "operation":"finite_rx1_multi_protocol_watch",
        "finished":finished,
        "plan_completed":plan_completed,
        "successful":successful,
        "interrupted":interrupted,
        "stop_reason":stop_reason,
        "cycles_requested":plan.cycles,
        "attempts_completed":attempts_completed,
        "center_frequency_hz":plan.primary_center_frequency_hz(),
        "sample_rate_hz":SAMPLE_RATE_HZ,
        "bandwidth_hz":SAMPLE_RATE_HZ,
        "rx_channel":RX_CHANNEL,
        "gain_db":plan.gain_db,
        "duration_seconds":plan.duration_seconds,
        "samples_per_capture":plan.samples,
        "required_fpga_version":STOCK_FPGA_VERSION,
        "acquisition_program":plan.receiver_identity,
        "analysis_programs":plan.analysis_program_identities,
        "nested_bluetooth_decoder":state.nested_bluetooth_decoder,
        "receive_only":true,
        "bias_tee_requested":false,
        "transmit_requested":false,
        "max_consecutive_failures":plan.max_consecutive_failures,
        "keep_successful_sessions":plan.keep_sessions,
        "retained_limit_bytes":plan.retained_limit_bytes,
        "retention_policy":{
            "strategy":"prefer_interesting_successes_within_existing_limits",
            "interesting_reasons":[
                "bluetooth_candidate_records",
                "ieee802154_candidate_records",
                "narrowband_binary_fsk_like_groups",
                "wifi_ofdm_data_decode_failures",
                "wifi_ofdm_invalid_fcs_frames"
            ],
            "minimum_retained_successful_sessions_when_available":1
        },
        "elapsed_wall_seconds":elapsed_seconds,
        "continuity_guaranteed":false,
        "interpretation":"Each cycle is a bounded RX1 snapshot followed by closed-file analysis. Startup, analysis and retention leave unobserved intervals. Valid frame checks do not establish identity, authorization, occupancy, decryption or complete protocol coverage.",
        "attempts":state.attempts,
    });
    if plan.discard_initial_samples > 0 {
        value["discard_initial_samples_per_capture"] = json!(plan.discard_initial_samples);
    }
    if plan.bluetooth_coded_shadow {
        value["bluetooth_coded_shadow_requested"] = json!(true);
        value["retention_policy"]["interesting_reasons"]
            .as_array_mut()
            .context("watch retention reasons are not an array")?
            .push(json!("bluetooth_coded_validated_packets"));
    }
    if plan.has_multiple_centers() {
        value
            .as_object_mut()
            .context("watch report root is not an object")?
            .remove("center_frequency_hz");
        value["center_frequency_schedule_hz"] = json!(plan.center_frequency_schedule_hz);
        value["center_selection"] = json!("round_robin_by_attempt_index");
        value["pending_capture_limit"] = json!(plan.pending_captures);
        value["retention_policy"]["minimum_retained_successful_sessions_per_center_when_available"] =
            json!(1);
        value["interpretation"] = json!(
            "Each attempt is a bounded RX1 snapshot at the center selected round-robin by its one-based attempt index. Closed IQ may be analyzed concurrently with at most one later receive-only capture when pending_capture_limit is one. Startup, retention and pipeline boundaries leave unobserved intervals. Valid frame checks do not establish identity, authorization, occupancy, decryption or complete protocol coverage."
        );
    }
    if plan.pending_captures != 0 {
        value["pending_capture_limit"] = json!(plan.pending_captures);
        value["attempts_started"] = json!(state.attempts.len());
        value["captures_finished"] = json!(state.attempts.len());
        value["analyses_started"] = json!(
            state
                .attempts
                .iter()
                .filter(|attempt| attempt.analysis_started_unix_ns.is_some())
                .count()
        );
        value["attempts_finished"] = json!(attempts_completed);
        if !plan.has_multiple_centers() {
            value["interpretation"] = json!(
                "Each cycle is a bounded RX1 snapshot whose closed IQ file may be analyzed concurrently with the next receive-only capture. At most one later capture is unresolved; startup, retention and pipeline boundaries leave unobserved intervals. Valid frame checks do not establish identity, authorization, occupancy, decryption or complete protocol coverage."
            );
        }
    }
    snapshot(directory, &value)
}
