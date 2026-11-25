//! Supervise independent rolling capture epochs within one bounded session.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::report::{self, OverallState};

use super::{Arguments, run_epoch};

const RESTART_BACKOFF: Duration = Duration::from_secs(1);
const BACKOFF_POLL: Duration = Duration::from_millis(25);
const RUN_REPORT_BYTES: u64 = 16 * 1024 * 1024;
const MIN_PRODUCTIVE_SEGMENTS: usize = 30;

#[derive(Clone, Copy, Debug, Serialize)]
pub(super) struct EpochEnd {
    pub(super) overall_state: OverallState,
    pub(super) capture_fault: Option<&'static str>,
    pub(super) published_segments: Option<usize>,
    pub(super) verified_segments: usize,
    pub(super) released_iq_segments: Option<usize>,
    pub(super) restartable: bool,
    #[serde(skip)]
    pub(super) extra_run_report_bytes: u64,
}

#[derive(Serialize)]
struct EpochReport {
    epoch: usize,
    directory: PathBuf,
    #[serde(flatten)]
    terminal: Option<EpochEnd>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_report_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StopReason {
    Finished,
    Interrupted,
    FinishAfterFailure,
    RestartLimit,
    ShortEpochRestartLimit,
    EpochFailed,
    EpochError,
}

impl StopReason {
    fn state(self) -> OverallState {
        match self {
            Self::Finished => OverallState::Complete,
            Self::Interrupted => OverallState::Interrupted,
            _ => OverallState::Failed,
        }
    }
}

#[derive(Serialize)]
struct SessionReport {
    schema_version: u8,
    operation: &'static str,
    recoverable_restarts: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    short_epoch_restarts: Option<u8>,
    restarts_used: usize,
    overall_state: OverallState,
    stop_reason: StopReason,
    epochs: Vec<EpochReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn finish_requested(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && metadata.len() == 0,
                "finish marker {} must be a direct empty regular file",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect finish marker {}", path.display()))
        }
    }
}

/// Relay the session stop request to the active receiver's epoch directory.
pub(super) fn relay_finish(session_marker: &Path, epoch_output: &Path) -> Result<()> {
    if !finish_requested(session_marker)? {
        return Ok(());
    }
    let marker = epoch_output.join("finish");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&marker)
    {
        Ok(file) => {
            file.sync_all()?;
            File::open(epoch_output)?.sync_all()?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            ensure!(
                finish_requested(&marker)?,
                "epoch finish marker disappeared"
            );
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("create finish marker {}", marker.display()));
        }
    }
    Ok(())
}

/// Run one capture at a time and keep each epoch's own terminal evidence.
pub(super) fn run(args: &Arguments, restarts: u8, stop: &AtomicBool) -> Result<ExitCode> {
    let mut coded_release_offset = 0;
    run_session(
        &args.output,
        restarts,
        args.short_epoch_restarts,
        stop,
        |directory, marker| {
            run_epoch(
                args,
                directory,
                Some(marker),
                stop,
                &mut coded_release_offset,
            )
        },
    )
}

fn run_session(
    output_path: &Path,
    restarts: u8,
    short_epoch_restarts: Option<u8>,
    stop: &AtomicBool,
    mut capture_epoch: impl FnMut(&Path, &Path) -> Result<EpochEnd>,
) -> Result<ExitCode> {
    let output = report::fresh_output(output_path)?;
    let marker = output.join("finish");
    let mut epochs = Vec::new();
    let mut consecutive_short_epochs = 0;
    let completion = (|| -> Result<StopReason> {
        let epochs_output = output.join("epochs");
        report::private_directory(&epochs_output)?;
        File::open(&output)?.sync_all()?;
        let mut restart_after = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(StopReason::Interrupted);
            }
            if finish_requested(&marker)? {
                return Ok(if epochs.is_empty() {
                    StopReason::Finished
                } else {
                    StopReason::FinishAfterFailure
                });
            }
            let backoff = restart_after.saturating_duration_since(Instant::now());
            if !backoff.is_zero() {
                thread::sleep(backoff.min(BACKOFF_POLL));
                continue;
            }

            let epoch = epochs.len() + 1;
            let directory = PathBuf::from("epochs").join(format!("{epoch:06}"));
            let epoch_output = output.join(&directory);
            let mut record = EpochReport {
                epoch,
                directory,
                terminal: None,
                run_report_sha256: None,
                error: None,
            };
            let result = (|| -> Result<EpochEnd> {
                let end = capture_epoch(&epoch_output, &marker)?;
                record.terminal = Some(end);
                let run_report = epoch_output.join("run.json");
                record.run_report_sha256 = Some(
                    report::hash_direct_regular_file(
                        &run_report,
                        RUN_REPORT_BYTES + end.extra_run_report_bytes,
                    )?
                    .sha256,
                );
                File::open(&epochs_output)?.sync_all()?;
                Ok(end)
            })();
            let end = match result {
                Ok(end) => end,
                Err(error) => {
                    record.error = Some(format!("{error:#}"));
                    epochs.push(record);
                    return Err(error).with_context(|| format!("settle rolling epoch {epoch}"));
                }
            };
            epochs.push(record);
            match end.overall_state {
                OverallState::Complete => return Ok(StopReason::Finished),
                OverallState::Interrupted => return Ok(StopReason::Interrupted),
                OverallState::Failed => {}
            }
            if finish_requested(&marker)? {
                return Ok(StopReason::FinishAfterFailure);
            }
            if !end.restartable {
                return Ok(StopReason::EpochFailed);
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(StopReason::Interrupted);
            }
            if epoch > usize::from(restarts) {
                return Ok(StopReason::RestartLimit);
            }
            // Restartable epochs have a nonempty, fully verified and released prefix.
            consecutive_short_epochs = if end.verified_segments < MIN_PRODUCTIVE_SEGMENTS {
                consecutive_short_epochs + 1
            } else {
                0
            };
            if short_epoch_restarts
                .is_some_and(|limit| consecutive_short_epochs > usize::from(limit))
            {
                return Ok(StopReason::ShortEpochRestartLimit);
            }
            restart_after = Instant::now() + RESTART_BACKOFF;
        }
    })();
    let (stop_reason, error) = match completion {
        Ok(reason) => (reason, None),
        Err(error) => (StopReason::EpochError, Some(format!("{error:#}"))),
    };
    let overall_state = stop_reason.state();
    let session = SessionReport {
        schema_version: 1,
        operation: "rolling_rx1_recovery_session",
        recoverable_restarts: restarts,
        short_epoch_restarts,
        restarts_used: epochs.len().saturating_sub(1),
        overall_state,
        stop_reason,
        epochs,
        error,
    };
    report::publish_json(&output, "session.json", &session)?;
    let status_output = crate::print_status(format_args!(
        "Terminal rolling recovery report: {}",
        output.join("session.json").display()
    ));
    if let Some(error) = &session.error {
        crate::print_error(format_args!("Rolling recovery failed: {error}"));
    }
    status_output?;
    Ok(if overall_state == OverallState::Complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn synthetic_epoch(path: &Path, recoverable: bool, segments: usize) -> Result<EpochEnd> {
        let end = EpochEnd {
            overall_state: if recoverable {
                OverallState::Failed
            } else {
                OverallState::Complete
            },
            capture_fault: recoverable.then_some("receive_overflow"),
            published_segments: Some(segments),
            verified_segments: segments,
            released_iq_segments: Some(segments),
            restartable: recoverable,
            extra_run_report_bytes: 0,
        };
        report::private_directory(path)?;
        report::publish_json(path, "run.json", &end)?;
        Ok(end)
    }

    #[test]
    fn receiver_fault_restarts_in_a_distinct_epoch() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("watch");
        let stop = AtomicBool::new(false);
        let code = run_session(&output, 1, None, &stop, |epoch, _| {
            synthetic_epoch(epoch, epoch.ends_with("000001"), 1)
        })?;
        assert_eq!(code, ExitCode::SUCCESS);
        let proof = report::read_json(&output.join("session.json"))?;
        assert_eq!(proof["restarts_used"], 1);
        assert!(proof.get("short_epoch_restarts").is_none());
        assert_eq!(proof["stop_reason"], "finished");
        assert_eq!(proof["epochs"][0]["capture_fault"], "receive_overflow");
        for index in 0..2 {
            let directory = format!("epochs/{:06}", index + 1);
            assert_eq!(proof["epochs"][index]["directory"], directory);
            let receipt = output.join(directory).join("run.json");
            assert_eq!(
                proof["epochs"][index]["run_report_sha256"],
                report::hash_direct_regular_file(&receipt, RUN_REPORT_BYTES)?.sha256
            );
        }
        Ok(())
    }

    #[test]
    fn requested_finish_preserves_fault_and_prevents_restart() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("watch");
        let stop = AtomicBool::new(false);
        let code = run_session(&output, 1, Some(0), &stop, |epoch, marker| {
            assert!(epoch.ends_with("000001"));
            let result = synthetic_epoch(epoch, true, 1)?;
            File::create(marker)?;
            Ok(result)
        })?;
        assert_eq!(code, ExitCode::FAILURE);
        let proof = report::read_json(&output.join("session.json"))?;
        assert_eq!(proof["restarts_used"], 0);
        assert_eq!(proof["stop_reason"], "finish_after_failure");
        assert_eq!(proof["epochs"][0]["capture_fault"], "receive_overflow");
        assert!(!output.join("epochs/000002").exists());
        Ok(())
    }

    #[test]
    fn productive_epoch_clears_short_failure_streak() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("watch");
        let stop = AtomicBool::new(false);
        let mut prefixes = [29, 30, 29, 29].into_iter();
        let code = run_session(&output, 6, Some(1), &stop, |epoch, _| {
            synthetic_epoch(epoch, true, prefixes.next().unwrap())
        })?;
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(prefixes.len(), 0);
        let proof = report::read_json(&output.join("session.json"))?;
        assert_eq!(proof["short_epoch_restarts"], 1);
        assert_eq!(proof["restarts_used"], 3);
        assert_eq!(proof["stop_reason"], "short_epoch_restart_limit");
        assert_eq!(proof["epochs"][3]["released_iq_segments"], 29);
        assert!(!output.join("epochs/000005").exists());
        Ok(())
    }

    #[test]
    fn cumulative_limit_takes_priority_when_both_limits_are_exhausted() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("watch");
        let stop = AtomicBool::new(false);
        let code = run_session(&output, 1, Some(1), &stop, |epoch, _| {
            synthetic_epoch(epoch, true, 1)
        })?;
        assert_eq!(code, ExitCode::FAILURE);
        let proof = report::read_json(&output.join("session.json"))?;
        assert_eq!(proof["restarts_used"], 1);
        assert_eq!(proof["stop_reason"], "restart_limit");
        assert!(!output.join("epochs/000003").exists());
        Ok(())
    }

    #[test]
    fn relay_requires_a_direct_empty_marker_in_both_directories() -> Result<()> {
        let session = tempfile::tempdir()?;
        let marker = session.path().join("finish");
        let epoch = session.path().join("epoch");
        report::private_directory(&epoch)?;
        relay_finish(&marker, &epoch)?;
        assert!(!epoch.join("finish").exists());
        symlink(epoch.join("finish"), &marker)?;
        assert!(relay_finish(&marker, &epoch).is_err());
        fs::remove_file(&marker)?;
        File::create(&marker)?;
        relay_finish(&marker, &epoch)?;
        relay_finish(&marker, &epoch)?;
        assert!(finish_requested(&epoch.join("finish"))?);
        fs::write(epoch.join("finish"), b"invalid")?;
        assert!(relay_finish(&marker, &epoch).is_err());
        Ok(())
    }
}
