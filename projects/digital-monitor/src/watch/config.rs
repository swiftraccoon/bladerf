use super::{
    CompactProgram, Context, DEFAULT_ANALYSIS_SAMPLE_LIMIT, Duration, MAX_CAPTURE_SAMPLES,
    MAX_CAPTURE_SECONDS, MAX_DISCARD_INITIAL_SAMPLES, PathBuf, Plan, ProgramProvenance, ProgramSet,
    Request, Result, SAMPLE_RATE_HZ, ensure, orchestrate, process,
};

pub(super) struct Prepared {
    pub(super) plan: Plan,
    pub(super) receiver: ProgramProvenance,
    pub(super) analysis_programs: ProgramSet,
}

fn capture_samples(duration_seconds: f64) -> u64 {
    (duration_seconds * SAMPLE_RATE_HZ as f64).round() as u64
}

pub(super) fn analysis_sample_limit(samples: u64) -> u64 {
    // Preserve Wi-Fi sample-limit and EOF reporting for captures up to one second.
    DEFAULT_ANALYSIS_SAMPLE_LIMIT.max(samples)
}

pub(super) fn analysis_request(
    request: &Request,
    center_mhz: u32,
    input: PathBuf,
    output: PathBuf,
) -> orchestrate::Request {
    orchestrate::Request {
        input,
        output,
        center_mhz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        max_samples: analysis_sample_limit(capture_samples(request.duration_seconds)),
        spectrum_windows: request.spectrum_windows,
        decoder_timeout_seconds: request.decoder_timeout_seconds,
        child_timeout_seconds: request.child_timeout_seconds,
        log_limit_mib: request.log_limit_mib,
        programs: request.programs.clone(),
    }
}

pub(super) fn prepare(request: &Request) -> Result<Prepared> {
    ensure!(
        (1..=1000).contains(&request.cycles),
        "cycles must be 1..=1000"
    );
    ensure!(
        request.duration_seconds.is_finite()
            && request.duration_seconds > 0.0
            && request.duration_seconds <= MAX_CAPTURE_SECONDS as f64,
        "duration must be finite, positive and at most {MAX_CAPTURE_SECONDS} seconds"
    );
    ensure!(
        !request.bluetooth_coded_shadow || request.duration_seconds <= 1.0,
        "bluetooth-coded-shadow requires a duration of at most one second"
    );
    let samples = capture_samples(request.duration_seconds);
    ensure!(
        (8192..=MAX_CAPTURE_SAMPLES).contains(&samples) && samples.is_multiple_of(10),
        "duration must produce 8192..={MAX_CAPTURE_SAMPLES} samples aligned to ten samples"
    );
    ensure!(
        !request.bluetooth_coded_shadow || samples >= 327_680,
        "bluetooth-coded-shadow requires at least 327680 complex samples"
    );
    ensure!(
        request.gain_db.is_finite() && (0.0..=71.0).contains(&request.gain_db),
        "gain must be finite and between 0 and 71 dB"
    );
    ensure!(
        (1..=100).contains(&request.max_consecutive_failures),
        "max-consecutive-failures must be 1..=100"
    );
    ensure!(
        (1..=100).contains(&request.keep_sessions),
        "keep-sessions must be 1..=100"
    );
    let mut center_schedule_mhz = vec![request.center_mhz];
    if let Some(alternate_center_mhz) = request.alternate_center_mhz {
        ensure!(
            alternate_center_mhz != request.center_mhz,
            "alternate-center-mhz must differ from center-mhz"
        );
        ensure!(
            request.cycles >= 2,
            "an alternating watch requires at least two cycles"
        );
        ensure!(
            request.keep_sessions >= 2,
            "an alternating watch requires keep-sessions of at least two"
        );
        center_schedule_mhz.push(alternate_center_mhz);
    }
    ensure!(
        center_schedule_mhz
            .iter()
            .all(|center| (2433..=2437).contains(center)),
        "watch centers must be within 2433..=2437 MHz"
    );
    ensure!(
        !request.bluetooth_coded_shadow || center_schedule_mhz.contains(&2435),
        "bluetooth-coded-shadow requires a 2435 MHz watch center"
    );
    ensure!(
        (1..=16_384).contains(&request.retained_limit_mib),
        "retained-limit-mib must be 1..=16384"
    );
    ensure!(
        request.pending_captures <= 1,
        "pending-captures must be 0 or 1"
    );
    ensure!(
        request.discard_initial_samples <= MAX_DISCARD_INITIAL_SAMPLES,
        "discard-initial-samples must be at most {MAX_DISCARD_INITIAL_SAMPLES}"
    );
    let discard_seconds = request.discard_initial_samples as f64 / SAMPLE_RATE_HZ as f64;
    ensure!(
        (21..=300).contains(&request.capture_timeout_seconds)
            && request.capture_timeout_seconds as f64
                >= request.duration_seconds + discard_seconds + 20.0,
        "capture-timeout must be 21..=300 seconds and cover duration, initial discard and a 20-second margin"
    );
    ensure!(
        (1..=64).contains(&request.log_limit_mib),
        "log-limit-mib must be 1..=64"
    );
    let retained_limit_bytes = u64::from(request.retained_limit_mib)
        .checked_mul(1024 * 1024)
        .context("retention byte limit overflow")?;
    let capture_bytes = samples.checked_mul(4).context("capture byte overflow")?;
    ensure!(
        capture_bytes <= retained_limit_bytes,
        "one requested IQ file exceeds the retained-byte threshold"
    );
    let log_limit_bytes = request
        .log_limit_mib
        .checked_mul(1024 * 1024)
        .context("log byte limit overflow")?;
    let receiver = process::resolve_program("bladerf-lab", &request.receiver)?;
    let mut analysis_programs = None;
    for center_mhz in &center_schedule_mhz {
        let observed = orchestrate::preflight(&analysis_request(
            request,
            *center_mhz,
            PathBuf::from("preflight-input"),
            PathBuf::from("preflight-output"),
        ))?;
        if let Some(expected) = &analysis_programs {
            ensure!(
                observed == *expected,
                "analysis executable identities differ between center preflights"
            );
        } else {
            analysis_programs = Some(observed);
        }
    }
    let analysis_programs = analysis_programs.context("watch center schedule is empty")?;
    let center_frequency_schedule_hz = center_schedule_mhz
        .into_iter()
        .map(|center_mhz| {
            u64::from(center_mhz)
                .checked_mul(1_000_000)
                .context("watch center frequency overflow")
        })
        .collect::<Result<Vec<_>>>()?;
    let receiver_identity = CompactProgram::from(&receiver);
    let analysis_program_identities = analysis_programs
        .reports()
        .iter()
        .map(CompactProgram::from)
        .collect();
    Ok(Prepared {
        plan: Plan {
            cycles: request.cycles,
            center_frequency_schedule_hz,
            duration_seconds: request.duration_seconds,
            samples,
            discard_initial_samples: request.discard_initial_samples,
            gain_db: request.gain_db,
            max_consecutive_failures: request.max_consecutive_failures,
            keep_sessions: usize::from(request.keep_sessions),
            retained_limit_bytes,
            pending_captures: request.pending_captures,
            bluetooth_coded_shadow: request.bluetooth_coded_shadow,
            capture_timeout: Duration::from_secs(request.capture_timeout_seconds),
            log_limit_bytes,
            receiver_identity: Some(receiver_identity),
            analysis_program_identities,
        },
        receiver,
        analysis_programs,
    })
}

pub(super) fn verify_programs(
    request: &Request,
    receiver: &ProgramProvenance,
    analysis_programs: &ProgramSet,
) -> Result<()> {
    ensure!(
        process::resolve_program("bladerf-lab", &request.receiver)? == *receiver,
        "pinned bladerf-lab executable changed during the watch"
    );
    orchestrate::revalidate_programs(
        &analysis_request(
            request,
            request.center_mhz,
            PathBuf::from("identity-input"),
            PathBuf::from("identity-output"),
        ),
        analysis_programs,
    )
}
