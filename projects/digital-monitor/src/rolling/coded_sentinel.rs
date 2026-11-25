//! One optional, bounded selected-frequency replay behind the rolling receiver.

mod replay;
mod stage;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use tempfile::TempDir;

use super::intake::ValidatedSegment;
use crate::process::{self, ProgramProvenance};

const SNAPSHOT: &str = "coded-sentinel.json";
const PCAP: &str = "coded-sentinel.pcap";
const FAILURE_TEXT_BYTES: usize = 4096;

#[derive(Serialize)]
pub(super) struct Summary {
    schema_version: u32,
    release_interval: usize,
    max_pending_iq: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    squelch_iq_omitted: Option<bool>,
    prior_release_opportunities: usize,
    release_opportunities: usize,
    started: usize,
    completed: usize,
    failed: usize,
    cancelled: usize,
    skipped_busy: usize,
    skipped_pressure: usize,
    bins: Vec<Bin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest: Option<Attempt>,
    // Keep the most recent failure across later successes and cancellations.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_failure: Option<FailedAttempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retained_pcap: Option<RetainedPcap>,
}

#[derive(Serialize)]
struct Bin {
    frequency_mhz: u32,
    completed_windows: u64,
    le_coded_s2: u64,
    le_coded_s8: u64,
}

#[derive(Clone, Serialize)]
struct Capture {
    ordinal: usize,
    first_sample_ns: i64,
    end_sample_exclusive_ns: i64,
    segment_receipt_sha256: String,
    source_sha256: String,
    sidecar_sha256: String,
}

#[derive(Serialize)]
struct Attempt {
    frequency_mhz: u32,
    capture: Capture,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Outcome {
    Complete { evidence: Box<replay::Evidence> },
    Failed { error: String },
    Cancelled,
}

#[derive(Serialize)]
struct FailedAttempt {
    frequency_mhz: u32,
    capture: Capture,
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wrapper_stderr: Option<LogExcerpt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decoder_log: Option<LogExcerpt>,
}

#[derive(Serialize)]
struct LogExcerpt {
    text: String,
    truncated: bool,
}

impl LogExcerpt {
    fn read(path: &Path) -> Option<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.len() == 0 {
            return None;
        }
        let limit = FAILURE_TEXT_BYTES as u64;
        let offset = metadata.len().saturating_sub(limit);
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let start = text.ceil_char_boundary(text.len().saturating_sub(FAILURE_TEXT_BYTES));
        Some(Self {
            text: text[start..].to_owned(),
            truncated: offset > 0 || start > 0,
        })
    }
}

struct WorkerFailure {
    error: anyhow::Error,
    wrapper_stderr: Option<LogExcerpt>,
    decoder_log: Option<LogExcerpt>,
}

impl From<anyhow::Error> for WorkerFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            wrapper_stderr: None,
            decoder_log: None,
        }
    }
}

impl WorkerFailure {
    // Read terminal diagnostics in the worker while scratch, including IQ,
    // is still available. Only the bounded text survives scratch cleanup.
    fn from_replay(error: anyhow::Error, scratch: TempDir) -> Self {
        let root = scratch.path();
        Self {
            error,
            wrapper_stderr: LogExcerpt::read(&root.join("logs/coded-sentinel.stderr.log")),
            decoder_log: LogExcerpt::read(&root.join("replay/decoder.log")),
        }
    }
}

#[derive(Serialize)]
struct RetainedPcap {
    path: &'static str,
    sha256: String,
    ordinal: usize,
    frequency_mhz: u32,
}

type WorkerResult = Result<(replay::Evidence, TempDir), WorkerFailure>;

struct Job {
    bin: usize,
    capture: Capture,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<WorkerResult>,
}

/// Owns the replay thread and its separate stop flag for one capture epoch.
/// The epoch settles it explicitly. Drop cancels and joins on early unwinding.
pub(super) struct Sentinel {
    output: PathBuf,
    wrapper: ProgramProvenance,
    decoder: ProgramProvenance,
    job: Option<Job>,
    summary: Summary,
    pending_snapshot: bool,
}

pub(super) struct Options {
    pub(super) every: usize,
    pub(super) spool_slots: usize,
    pub(super) omit_squelch_iq: bool,
}

impl Sentinel {
    pub(super) fn prepare(
        output: &Path,
        options: Options,
        center_mhz: u32,
        wrapper: &Path,
        decoder: &Path,
        prior_release_opportunities: usize,
    ) -> Result<Self> {
        ensure!(
            options.every > 0 && options.spool_slots > 0,
            "Coded sentinel needs an IQ spool and nonzero cadence"
        );
        let bins: Vec<_> = (2404_u32..=2478)
            .step_by(2)
            .filter(|&frequency| frequency != 2426 && frequency.abs_diff(center_mhz) < 10)
            .map(|frequency_mhz| Bin {
                frequency_mhz,
                completed_windows: 0,
                le_coded_s2: 0,
                le_coded_s8: 0,
            })
            .collect();
        ensure!(
            (2400..=2480).contains(&center_mhz) && !bins.is_empty(),
            "Coded sentinel needs an interior BLE data bin at the capture center"
        );
        let mut sentinel = Self {
            output: output.to_path_buf(),
            wrapper: process::resolve_program("bluetooth-monitor", wrapper)?,
            decoder: process::resolve_program("blue-dragon", decoder)?,
            job: None,
            pending_snapshot: true,
            summary: Summary {
                schema_version: 1,
                release_interval: options.every,
                max_pending_iq: options.spool_slots.div_ceil(4),
                squelch_iq_omitted: options.omit_squelch_iq.then_some(true),
                prior_release_opportunities,
                release_opportunities: 0,
                started: 0,
                completed: 0,
                failed: 0,
                cancelled: 0,
                skipped_busy: 0,
                skipped_pressure: 0,
                bins,
                latest: None,
                latest_failure: None,
                retained_pcap: None,
            },
        };
        sentinel.publish_pending();
        Ok(sentinel)
    }

    pub(super) fn summary(&self) -> &Summary {
        &self.summary
    }

    pub(super) fn next_release_offset(&self) -> usize {
        self.summary.prior_release_opportunities + self.summary.release_opportunities
    }

    /// Pin one admitted source before removal. Copying and replay run off the
    /// release path, with capacity for one job and zero queued jobs.
    pub(super) fn offer(
        &mut self,
        segment: &ValidatedSegment,
        stop: &AtomicBool,
        pending_iq: impl FnOnce() -> Result<usize>,
    ) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            self.finish(false);
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        self.summary.release_opportunities += 1;
        let session_releases = self.next_release_offset();
        let summary = &mut self.summary;
        let every = summary.release_interval;
        if !session_releases.is_multiple_of(every) {
            return;
        }
        let bin = (session_releases / every - 1) % summary.bins.len();
        if self.job.is_some() {
            summary.skipped_busy += 1;
        } else if !pending_iq().is_ok_and(|pending| pending <= summary.max_pending_iq) {
            summary.skipped_pressure += 1;
        } else {
            match self.start(bin, segment) {
                Ok(()) => self.summary.started += 1,
                Err(error) => self.fail(bin, capture_identity(segment), error.into()),
            }
        }
        self.pending_snapshot = true;
    }

    fn start(&mut self, bin: usize, segment: &ValidatedSegment) -> Result<()> {
        let worker_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&worker_stop);
        let output = self.output.clone();
        let wrapper = self.wrapper.clone();
        let decoder = self.decoder.clone();
        let frequency = self.summary.bins[bin].frequency_mhz;
        let omit_squelch_iq = self.summary.squelch_iq_omitted.unwrap_or(false);
        let input = stage::pin(segment)?;
        let handle = thread::Builder::new()
            .name("rolling-coded-sentinel".into())
            .spawn(move || {
                super::lower_replay_priority()?;
                let scratch = tempfile::Builder::new()
                    .prefix(".coded-sentinel-")
                    .tempdir_in(&output)
                    .context("create Coded sentinel scratch directory")?;
                let source = input.stage(scratch.path(), &stop)?;
                match replay::run(
                    scratch.path(),
                    &source,
                    frequency,
                    &wrapper,
                    &decoder,
                    omit_squelch_iq,
                    &stop,
                ) {
                    Ok(evidence) => Ok((evidence, scratch)),
                    Err(error) => Err(WorkerFailure::from_replay(error, scratch)),
                }
            })
            .context("start Coded sentinel worker")?;
        self.job = Some(Job {
            bin,
            capture: capture_identity(segment),
            stop: worker_stop,
            handle,
        });
        Ok(())
    }

    fn fail(&mut self, bin: usize, capture: Capture, failure: WorkerFailure) {
        self.summary.failed += 1;
        let frequency_mhz = self.summary.bins[bin].frequency_mhz;
        let mut error = format!("{:#}", failure.error);
        error.truncate(error.floor_char_boundary(FAILURE_TEXT_BYTES));
        self.summary.latest_failure = Some(FailedAttempt {
            frequency_mhz,
            capture: capture.clone(),
            error: error.clone(),
            wrapper_stderr: failure.wrapper_stderr,
            decoder_log: failure.decoder_log,
        });
        self.summary.latest = Some(Attempt {
            frequency_mhz,
            capture,
            outcome: Outcome::Failed { error },
        });
    }

    /// Drain finite runs and cancel before recovery or interruption. Calling
    /// this on a finished handle during acquisition never waits on replay.
    pub(super) fn finish(&mut self, cancel: bool) {
        self.pending_snapshot = true;
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
            .map_err(|_| WorkerFailure::from(anyhow::anyhow!("Coded sentinel worker panicked")))
            .and_then(|result| result);
        let bin = &mut self.summary.bins[job.bin];
        let (capture, frequency_mhz) = (job.capture, bin.frequency_mhz);
        let outcome = match result {
            Ok((evidence, scratch)) => {
                let counts = &evidence.selected_coded_packets;
                bin.completed_windows += evidence.coverage.completed_windows;
                bin.le_coded_s2 += counts.le_coded_s2;
                bin.le_coded_s8 += counts.le_coded_s8;
                self.summary.completed += 1;
                if counts.le_coded_s2 != 0 || counts.le_coded_s8 != 0 {
                    match fs::rename(
                        scratch.path().join("replay/validated.pcap"),
                        self.output.join(PCAP),
                    ) {
                        Ok(()) => {
                            self.summary.retained_pcap = Some(RetainedPcap {
                                path: PCAP,
                                sha256: evidence.validated_pcap_sha256.clone(),
                                ordinal: capture.ordinal,
                                frequency_mhz,
                            })
                        }
                        Err(error) => crate::print_error(format_args!(
                            "Could not retain Coded sentinel packets: {error}"
                        )),
                    }
                }
                Outcome::Complete {
                    evidence: Box::new(evidence),
                }
            }
            Err(_) if cancelled => {
                self.summary.cancelled += 1;
                Outcome::Cancelled
            }
            Err(error) => {
                self.fail(job.bin, capture, error);
                return;
            }
        };
        self.summary.latest = Some(Attempt {
            frequency_mhz,
            capture,
            outcome,
        });
    }

    pub(super) fn publish_pending(&mut self) {
        if !std::mem::take(&mut self.pending_snapshot) {
            return;
        }
        let result = (|| -> Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(&self.output)?;
            serde_json::to_writer(&mut file, &self.summary)?;
            file.write_all(b"\n")?;
            file.as_file().sync_all()?;
            file.persist(self.output.join(SNAPSHOT))?;
            File::open(&self.output)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            crate::print_error(format_args!(
                "Could not publish Coded sentinel status: {error:#}"
            ));
        }
    }
}

impl Drop for Sentinel {
    fn drop(&mut self) {
        if self.job.is_some() {
            self.finish(true);
            self.publish_pending();
        }
    }
}

fn capture_identity(segment: &ValidatedSegment) -> Capture {
    Capture {
        ordinal: segment.receipt.index + 1,
        first_sample_ns: segment.receipt.first_sample_ns,
        end_sample_exclusive_ns: segment.receipt.end_sample_exclusive_ns,
        segment_receipt_sha256: segment.receipt_sha256.clone(),
        source_sha256: segment.source.sha256.clone(),
        sidecar_sha256: segment.source.sidecar_sha256.clone(),
    }
}
