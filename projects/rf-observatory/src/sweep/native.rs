//! Supervise one ordered native receive batch with a per-progress deadline.

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::json;

use super::{TuneRequest, plan::Plan};
use crate::acquisition::{AcquisitionChild, recording_ready};
use crate::{SAMPLE_RATE, json_file, new_file};

enum CaptureOutcome {
    Exited(ExitStatus),
    Stop { interrupted: bool, completed: usize },
}

/// Run the native batch to exit or until its cancellation or progress deadline.
///
/// A new closed sidecar starts the next dwell's deadline. Shutdown allows the
/// worker to deactivate RX before the forced-reap fallback. Logs and closed
/// files remain at the existing output directory when an error is returned.
pub(super) fn capture(
    root: &Path,
    program: &Path,
    output: &Path,
    requests: &[TuneRequest],
    plan: &Plan,
    stop: &AtomicBool,
) -> Result<()> {
    let settings = requests.first().context("sweep has no tunes")?.settings;
    let passes = plan.passes();
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep interrupted before capture"
    );
    let mut acquisition_request = json!({
        "schema_version": 1,
        "operation": "native_sweep_acquisition",
        "receiver_channel": settings.antenna,
        "manual_gain_db": settings.gain,
        "sample_rate_hz": SAMPLE_RATE,
        "samples_per_tune": plan.samples_per_tune,
        "tunes": requests.len(),
    });
    if passes > 1 {
        acquisition_request["passes"] = json!(passes);
    }
    if !plan.dwell_ms_by_center.is_empty() {
        acquisition_request["dwell_ms_by_center"] = json!(plan.dwell_ms_by_center);
    }
    if plan.check_continuity {
        acquisition_request["check_continuity"] = json!(true);
    }
    if let Some(visits) = &plan.visit_centers_hz {
        acquisition_request["visit_centers_hz"] = json!(visits);
    }
    json_file(
        &output.join("acquisition-request.json"),
        &acquisition_request,
    )?;
    let log_path = output.join("acquisition.log");
    let log = new_file(&log_path)?;
    let mut command = Command::new(program);
    command
        .arg("--workspace")
        .arg(root)
        .args([
            "capture-sweep",
            "--channel",
            &settings.antenna.to_string(),
            "--gain",
            &settings.gain.to_string(),
            "--duration",
            &(plan.dwell_ms as f64 / 1_000.0).to_string(),
            "--output",
        ])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if passes > 1 {
        command.arg("--passes").arg(passes.to_string());
    }
    if plan.check_continuity {
        command.arg("--check-continuity");
    }
    for center in &plan.centers_hz {
        command.arg("--center-hz").arg(center.to_string());
    }
    if let Some(visits) = &plan.visit_centers_hz {
        for center in visits {
            command.arg("--visit-hz").arg(center.to_string());
        }
    }
    for (center, dwell_ms) in &plan.dwell_ms_by_center {
        command
            .arg("--dwell-at")
            .arg(format!("{center}:{dwell_ms}"));
    }

    let mut child =
        AcquisitionChild::new(command.spawn().context("start native sweep acquisition")?);
    match monitor(&mut child, requests, Duration::from_secs(20), stop) {
        Ok(CaptureOutcome::Exited(status)) => {
            ensure!(
                status.success(),
                "native sweep acquisition failed ({status}). See {}",
                log_path.display()
            );
            Ok(())
        }
        Ok(CaptureOutcome::Stop {
            interrupted,
            completed,
        }) => {
            let forced_kill = child.stop().context("stop native sweep acquisition")?;
            json_file(
                &output.join("acquisition-failure.json"),
                &json!({
                    "interrupted": interrupted,
                    "completed_sidecars": completed,
                    "forced_kill": forced_kill,
                }),
            )?;
            let reason = if interrupted {
                "interrupted"
            } else {
                "made no progress before its deadline"
            };
            bail!(
                "native sweep acquisition {reason}. See {}",
                log_path.display()
            );
        }
        Err(primary) => match child.stop().context("stop native sweep acquisition") {
            Ok(_) => Err(primary),
            Err(shutdown) => {
                Err(primary.context(format!("stop after supervision error: {shutdown:#}")))
            }
        },
    }
}

fn monitor(
    child: &mut AcquisitionChild,
    requests: &[TuneRequest],
    progress_slack: Duration,
    stop: &AtomicBool,
) -> Result<CaptureOutcome> {
    let mut last_progress_at = Instant::now();
    let mut completed = 0;
    loop {
        if let Some(status) = child.try_wait().context("check native sweep acquisition")? {
            return Ok(CaptureOutcome::Exited(status));
        }
        while let Some(request) = requests.get(completed) {
            if !recording_ready(&request.path)
                .with_context(|| format!("check native output {}", request.path.display()))?
            {
                break;
            }
            completed += 1;
            last_progress_at = Instant::now();
        }
        let interrupted = stop.load(Ordering::Relaxed);
        let expected_duration = requests
            .get(completed)
            .or_else(|| requests.last())
            .map_or(Duration::ZERO, |request| {
                Duration::from_secs_f64(request.settings.duration)
            });
        let progress_timeout = expected_duration + progress_slack;
        if interrupted || last_progress_at.elapsed() >= progress_timeout {
            return Ok(CaptureOutcome::Stop {
                interrupted,
                completed,
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_worker_is_stopped_without_waiting_for_normal_exit() {
        let mut child = AcquisitionChild::new(Command::new("sleep").arg("5").spawn().unwrap());
        let stop = AtomicBool::new(false);
        let outcome = monitor(&mut child, &[], Duration::ZERO, &stop).unwrap();
        assert!(matches!(
            outcome,
            CaptureOutcome::Stop {
                interrupted: false,
                completed: 0
            }
        ));
        child.stop().unwrap();
        assert!(child.try_wait().unwrap().is_some());
    }
}
