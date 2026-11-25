use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

use nix::libc;

use super::{
    AtomicBool, CaptureDiagnostics, CaptureStep, CommandReport, Context, LiveExpectation,
    LogReport, MAX_CAPTURE_LOG_SCAN_BYTES, Ordering, OsString, Path, Plan, ProgramProvenance,
    RX_CHANNEL, Read, Result, SAMPLE_RATE_HZ, STOCK_FPGA_VERSION, StepState, Termination, ensure,
    fs, process,
};
use crate::report::same_file_identity;

pub(super) fn capture_arguments(plan: &Plan, center_frequency_hz: u64, iq: &Path) -> Vec<OsString> {
    let mut arguments = vec![
        "capture".into(),
        "--frequency".into(),
        center_frequency_hz.to_string().into(),
        "--sample-rate".into(),
        SAMPLE_RATE_HZ.to_string().into(),
        "--gain".into(),
        plan.gain_db.to_string().into(),
        "--channel".into(),
        "1".into(),
        "--duration".into(),
        plan.duration_seconds.to_string().into(),
        "--require-fpga-version".into(),
        STOCK_FPGA_VERSION.into(),
        "--output".into(),
        iq.as_os_str().to_owned(),
    ];
    if plan.discard_initial_samples > 0 {
        arguments.extend([
            "--discard-initial-samples".into(),
            plan.discard_initial_samples.to_string().into(),
        ]);
    }
    arguments
}

pub(super) fn live_expectation(plan: &Plan, center_frequency_hz: u64) -> LiveExpectation<'static> {
    LiveExpectation {
        center_frequency_hz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        exact_samples: plan.samples,
        discarded_initial_samples: plan.discard_initial_samples,
        gain_db: plan.gain_db,
        channel: RX_CHANNEL,
        fpga_version: STOCK_FPGA_VERSION,
    }
}

pub(crate) fn read_complete_capture_log(
    session: &Path,
    report: &LogReport,
    expected_name: &str,
) -> Result<Vec<u8>> {
    ensure!(
        report.path == Path::new("logs").join(expected_name),
        "capture log path differs from the supervisor contract"
    );
    ensure!(
        !report.truncated && report.observed_bytes == report.retained_bytes,
        "capture log was not retained completely"
    );
    ensure!(
        report.retained_bytes <= MAX_CAPTURE_LOG_SCAN_BYTES,
        "capture log exceeds the diagnostic scan bound"
    );
    let path = session.join(&report.path);
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "capture log is not a direct regular file"
    );
    ensure!(
        metadata.len() == report.retained_bytes,
        "capture log extent differs from the supervisor report"
    );
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file() && same_file_identity(&metadata, &opened),
        "capture log changed before scanning"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(report.retained_bytes)?);
    (&mut file)
        .take(report.retained_bytes + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let named_after = fs::symlink_metadata(&path)?;
    ensure!(
        bytes.len() as u64 == report.retained_bytes
            && same_file_identity(&opened, &after)
            && same_file_identity(&opened, &named_after),
        "capture log changed while scanning"
    );
    Ok(bytes)
}

pub(super) fn capture_diagnostics(
    session: &Path,
    report: &CommandReport,
) -> Result<CaptureDiagnostics> {
    let stdout = read_complete_capture_log(session, &report.stdout, "acquisition.stdout.log")?;
    let stderr = read_complete_capture_log(session, &report.stderr, "acquisition.stderr.log")?;
    let text = String::from_utf8_lossy(&[stdout.as_slice(), b"\n", stderr.as_slice()].concat())
        .to_ascii_lowercase();
    let known_io_error_markers = [
        "transfer error",
        "transfer timed out",
        "stream error",
        "seify source error",
        "panicked at",
        "[error]",
    ]
    .iter()
    .try_fold(0u64, |count, marker| {
        count
            .checked_add(u64::try_from(text.matches(marker).count())?)
            .context("capture diagnostic marker count overflow")
    })?;
    let overrun_markers =
        ["overrun", "overflow"]
            .iter()
            .try_fold(0u64, |count, marker| -> Result<u64> {
                count
                    .checked_add(u64::try_from(text.matches(marker).count())?)
                    .context("capture overrun marker count overflow")
            })?;
    Ok(CaptureDiagnostics {
        stdout_bytes: report.stdout.observed_bytes,
        stderr_bytes: report.stderr.observed_bytes,
        logs_complete: true,
        known_io_error_markers,
        overrun_markers,
    })
}

pub(super) fn capture_once(
    receiver: &ProgramProvenance,
    plan: &Plan,
    center_frequency_hz: u64,
    session: &Path,
    stop: &AtomicBool,
) -> CaptureStep {
    if stop.load(Ordering::Relaxed) {
        return CaptureStep {
            state: StepState::Interrupted,
            source: None,
            termination: None,
            diagnostics: None,
            detail: Some("interrupted before acquisition child start".into()),
        };
    }
    let iq = session.join("iq.sc16q11");
    let command = process::run(
        receiver,
        &capture_arguments(plan, center_frequency_hz, &iq),
        session,
        "acquisition",
        plan.log_limit_bytes,
        plan.capture_timeout,
        stop,
    );
    let report = match command {
        Ok(report) => report,
        Err(error) => {
            return CaptureStep {
                state: if stop.load(Ordering::Relaxed) {
                    StepState::Interrupted
                } else {
                    StepState::Failed
                },
                source: None,
                termination: None,
                diagnostics: None,
                detail: Some(format!("{error:#}")),
            };
        }
    };
    let termination = Some(Termination::from(&report));
    let diagnostics = match capture_diagnostics(session, &report) {
        Ok(diagnostics) => Some(diagnostics),
        Err(error) => {
            return CaptureStep {
                state: if report.interrupted || stop.load(Ordering::Relaxed) {
                    StepState::Interrupted
                } else {
                    StepState::Failed
                },
                source: None,
                termination,
                diagnostics: None,
                detail: Some(format!("capture diagnostics were incomplete: {error:#}")),
            };
        }
    };
    if let Err(error) = process::require_success(&report) {
        return CaptureStep {
            state: if report.interrupted || stop.load(Ordering::Relaxed) {
                StepState::Interrupted
            } else {
                StepState::Failed
            },
            source: None,
            termination,
            diagnostics,
            detail: Some(format!("{error:#}")),
        };
    }
    if diagnostics.as_ref().is_some_and(|diagnostics| {
        diagnostics.known_io_error_markers > 0 || diagnostics.overrun_markers > 0
    }) {
        return CaptureStep {
            state: StepState::Failed,
            source: None,
            termination,
            diagnostics,
            detail: Some("capture diagnostics reported an I/O error or overrun".into()),
        };
    }
    match crate::input::validate_live(&iq, live_expectation(plan, center_frequency_hz)) {
        Ok(source) => CaptureStep {
            state: StepState::Complete,
            source: Some(source),
            termination,
            diagnostics,
            detail: None,
        },
        Err(error) => CaptureStep {
            state: StepState::Failed,
            source: None,
            termination,
            diagnostics,
            detail: Some(format!("strict live capture validation failed: {error:#}")),
        },
    }
}

pub(super) fn enforce_capture_center(capture: &mut CaptureStep, expected_center_frequency_hz: u64) {
    if capture.state == StepState::Complete
        && capture
            .source
            .as_ref()
            .map(|source| source.center_frequency_hz)
            != Some(expected_center_frequency_hz)
    {
        capture.state = StepState::Failed;
        capture.detail = Some(
            "accepted capture center differs from the center selected for this attempt".into(),
        );
    }
}
