//! Supervise a bounded replay of the first archived HE20 SU clip.

mod evidence;
mod replay;

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::process::{self, ProgramProvenance};

const STATUS_FILE: &str = "he-su-payload.json";
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Serialize)]
pub(super) struct Status {
    schema_version: u8,
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    archive_sha256: Option<String>,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Outcome {
    Waiting,
    Running,
    Complete { evidence: replay::ReplayEvidence },
    Failed { error: String },
    Cancelled,
}

struct Job {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<replay::ReplayEvidence>>,
}

/// Own the epoch's sole payload replay. Dropping the worker stops and joins it.
pub(super) struct Worker {
    output: PathBuf,
    monitor: ProgramProvenance,
    decoder: ProgramProvenance,
    job: Option<Job>,
    status: Status,
}

impl Worker {
    pub(super) fn prepare(output: &Path, monitor: &Path, decoder: &Path) -> Result<Self> {
        let worker = Self {
            output: output.to_owned(),
            monitor: process::resolve_program("wifi-monitor", monitor)?,
            decoder: process::resolve_program("radio_receive", decoder)?,
            job: None,
            status: Status {
                schema_version: 1,
                operation: "rolling_he20_su_payload_replay",
                archive_sha256: None,
                outcome: Outcome::Waiting,
            },
        };
        worker.publish()?;
        Ok(worker)
    }

    pub(super) fn status(&self) -> &Status {
        &self.status
    }

    pub(super) fn start(&mut self, archive_sha256: &str) {
        if self.status.archive_sha256.is_some() {
            return;
        }
        self.status.archive_sha256 = Some(archive_sha256.to_owned());
        let archive_sha256 = archive_sha256.to_owned();
        let output = self.output.clone();
        let monitor = self.monitor.clone();
        let decoder = self.decoder.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let job = thread::Builder::new()
            .name("rolling-he-payload".into())
            .spawn(move || {
                super::lower_replay_priority()?;
                replay::run(&output, &archive_sha256, &monitor, &decoder, &worker_stop)
            })
            .context("start HE payload replay worker");
        self.status.outcome = match job {
            Ok(handle) => {
                self.job = Some(Job { stop, handle });
                Outcome::Running
            }
            Err(error) => Self::failure(error),
        };
        self.publish_progress();
    }

    pub(super) fn poll(&mut self) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            self.finish(false);
        }
    }

    pub(super) fn finish(&mut self, cancel: bool) {
        let Some(job) = self.job.take() else {
            return;
        };
        let cancelled = cancel && !job.handle.is_finished();
        if cancelled {
            job.stop.store(true, Ordering::Relaxed);
        }
        let result = job
            .handle
            .join()
            .map_err(|_| anyhow::anyhow!("HE payload replay worker panicked"))
            .and_then(|result| result);
        self.status.outcome = match result {
            Ok(evidence) => Outcome::Complete { evidence },
            Err(_) if cancelled => Outcome::Cancelled,
            Err(error) => Self::failure(error),
        };
        self.publish_progress();
    }

    fn failure(error: anyhow::Error) -> Outcome {
        let mut message = format!("{error:#}");
        message.truncate(message.floor_char_boundary(MAX_ERROR_BYTES));
        Outcome::Failed { error: message }
    }

    fn publish_progress(&self) {
        if let Err(error) = self.publish() {
            crate::print_error(format_args!(
                "Could not publish HE payload replay status: {error:#}"
            ));
        }
    }

    fn publish(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.output)?;
        serde_json::to_writer(&mut file, &self.status)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.output.join(STATUS_FILE))?;
        File::open(&self.output)?.sync_all()?;
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.finish(true);
    }
}
