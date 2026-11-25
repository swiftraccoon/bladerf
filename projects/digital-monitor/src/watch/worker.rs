use super::{
    AnalysisStep, AtomicBool, Instant, Ordering, Path, PathBuf, Receiver, Result, SourceReport,
    StepState, unix_ns,
};

pub(super) struct AnalysisJob {
    pub(super) index: usize,
    pub(super) source: SourceReport,
    pub(super) session: PathBuf,
    pub(super) release: Receiver<()>,
}

pub(super) struct AnalysisCompletion {
    pub(super) index: usize,
    pub(super) step: AnalysisStep,
    pub(super) analysis_ended_unix_ns: Option<u64>,
    pub(super) analysis_end_offset_seconds: Option<f64>,
    pub(super) interrupted: bool,
    pub(super) program_changed: bool,
    pub(super) worker_failed: bool,
}

pub(super) enum AnalysisWorkerEvent {
    Started {
        index: usize,
        unix_ns: u64,
        offset_seconds: f64,
    },
    Finished(Box<AnalysisCompletion>),
}

pub(super) fn failed_analysis(detail: impl Into<String>) -> AnalysisStep {
    AnalysisStep {
        state: StepState::Failed,
        compact: None,
        detail: Some(detail.into()),
    }
}

pub(super) fn analysis_worker<A, V>(
    stop: &AtomicBool,
    start: Instant,
    jobs: Receiver<AnalysisJob>,
    events: std::sync::mpsc::Sender<AnalysisWorkerEvent>,
    mut verify: V,
    mut analyze: A,
) where
    A: FnMut(&SourceReport, &Path, &AtomicBool) -> AnalysisStep,
    V: FnMut() -> Result<()>,
{
    while let Ok(job) = jobs.recv() {
        let index = job.index;
        let analysis_started_unix_ns = match unix_ns() {
            Ok(value) => value,
            Err(error) => {
                let _ = events.send(AnalysisWorkerEvent::Finished(Box::new(
                    AnalysisCompletion {
                        index,
                        step: failed_analysis(format!(
                            "record analysis start time in worker: {error:#}"
                        )),
                        analysis_ended_unix_ns: None,
                        analysis_end_offset_seconds: None,
                        interrupted: false,
                        program_changed: false,
                        worker_failed: true,
                    },
                )));
                break;
            }
        };
        if events
            .send(AnalysisWorkerEvent::Started {
                index,
                unix_ns: analysis_started_unix_ns,
                offset_seconds: start.elapsed().as_secs_f64(),
            })
            .is_err()
            || job.release.recv().is_err()
        {
            break;
        }
        let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if stop.load(Ordering::Relaxed) {
                return AnalysisCompletion {
                    index,
                    step: AnalysisStep::not_started(
                        "interrupted before queued offline analysis started",
                    ),
                    analysis_ended_unix_ns: None,
                    analysis_end_offset_seconds: None,
                    interrupted: true,
                    program_changed: false,
                    worker_failed: false,
                };
            }
            let mut program_changed = false;
            let mut step = if let Err(error) = verify() {
                program_changed = true;
                failed_analysis(format!(
                    "program identity changed before analysis: {error:#}"
                ))
            } else {
                analyze(&job.source, &job.session, stop)
            };
            if !program_changed
                && step.state != StepState::NotStarted
                && let Err(error) = verify()
            {
                step.state = StepState::Failed;
                step.detail = Some(format!(
                    "program identity changed after analysis: {error:#}"
                ));
                program_changed = true;
            }
            let interrupted = stop.load(Ordering::Relaxed) || step.state == StepState::Interrupted;
            match unix_ns() {
                Ok(analysis_ended_unix_ns) => AnalysisCompletion {
                    index,
                    step,
                    analysis_ended_unix_ns: Some(analysis_ended_unix_ns),
                    analysis_end_offset_seconds: Some(start.elapsed().as_secs_f64()),
                    interrupted,
                    program_changed,
                    worker_failed: false,
                },
                Err(error) => AnalysisCompletion {
                    index,
                    step: failed_analysis(format!("record analysis end time in worker: {error:#}")),
                    analysis_ended_unix_ns: None,
                    analysis_end_offset_seconds: Some(start.elapsed().as_secs_f64()),
                    interrupted,
                    program_changed,
                    worker_failed: true,
                },
            }
        }));
        let (completion, must_exit) = match guarded {
            Ok(completion) => (completion, false),
            Err(_) => {
                let (step, analysis_ended_unix_ns) = match unix_ns() {
                    Ok(value) => (
                        failed_analysis("offline analysis worker panicked"),
                        Some(value),
                    ),
                    Err(error) => (
                        failed_analysis(format!(
                            "offline analysis worker panicked; record end time: {error:#}"
                        )),
                        None,
                    ),
                };
                (
                    AnalysisCompletion {
                        index,
                        step,
                        analysis_ended_unix_ns,
                        analysis_end_offset_seconds: Some(start.elapsed().as_secs_f64()),
                        interrupted: stop.load(Ordering::Relaxed),
                        program_changed: false,
                        worker_failed: true,
                    },
                    true,
                )
            }
        };
        if events
            .send(AnalysisWorkerEvent::Finished(Box::new(completion)))
            .is_err()
            || must_exit
        {
            break;
        }
    }
}
