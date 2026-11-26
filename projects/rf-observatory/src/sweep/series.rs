//! Publish reusable per-pass receipts and one terminal series receipt.

use std::io::{self, Read, Seek, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    CaptureCadence, CaptureRun, CompletedSweep, Elapsed, SpectrumOutput, analyze_prefix,
    combine_capture_and_analysis, publish_receipt, timing, verify_coverage,
};

pub(crate) fn pass_directory_name(index: usize) -> String {
    format!("pass-{:04}", index + 1)
}

#[derive(Serialize)]
struct SeriesReceipt<'a> {
    schema_version: u32,
    operation: &'static str,
    iq_bytes: u64,
    dwells: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    visit_centers_hz: Option<&'a [u64]>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    check_continuity: bool,
    #[serde(flatten)]
    elapsed: Elapsed,
    capture_cadence: CaptureCadence,
    #[serde(skip_serializing_if = "SpectrumOutput::is_full")]
    spectrum_output: SpectrumOutput,
    same_center_revisit_ns: timing::Spacing,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_triage_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_triage_overlap_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_activity_sha256: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_series_sha256: Option<&'a str>,
    passes: Vec<PassReceipt>,
}

#[derive(Serialize)]
struct PassReceipt {
    directory: String,
    sweep_sha256: String,
}

impl CaptureRun<'_> {
    /// A valid pass stays independently replayable if a later pass fails.
    /// The series receipt requires a complete capture and ordered native timing.
    pub(super) fn finish_series(self, capture: Result<()>, stop: &AtomicBool) -> Result<()> {
        let evidence = capture.and_then(|()| {
            timing::read(
                self.output,
                self.requests,
                self.acquisition_elapsed,
                self.plan.passes(),
            )?
            .context("repeated sweep requires native timing evidence")
        });
        let analysis_start = Instant::now();
        let analysis = self.analyze_passes(evidence.as_ref().ok(), stop);
        let (evidence, passes) = combine_capture_and_analysis(evidence, analysis, stop)?;
        let revisit = evidence
            .same_center_revisit_ns
            .context("completed series has no revisit timing")?;
        ensure!(
            !stop.load(Ordering::Relaxed),
            "sweep series interrupted before publication"
        );
        let acquisition = Some(self.acquisition_elapsed.as_secs_f64());
        let receipt = SeriesReceipt {
            schema_version: 1,
            operation: "rf_sweep_series",
            iq_bytes: self.plan.planned_iq_bytes,
            dwells: self.requests.len(),
            visit_centers_hz: self.plan.visit_centers_hz.as_deref(),
            check_continuity: self.plan.check_continuity,
            elapsed: Elapsed {
                elapsed_seconds: Some(self.start.elapsed().as_secs_f64()),
                acquisition_elapsed_seconds: acquisition,
                analysis_elapsed_seconds: analysis_start.elapsed().as_secs_f64(),
            },
            capture_cadence: CaptureCadence::new(
                self.plan.planned_iq_bytes,
                acquisition,
                Some(evidence.summary),
            ),
            spectrum_output: self.spectrum_output,
            same_center_revisit_ns: revisit,
            signal_profile_sha256: self.plan.signal_profile_sha256.as_deref(),
            source_series_triage_sha256: self.plan.source_series_triage_sha256.as_deref(),
            source_series_triage_overlap_sha256: self
                .plan
                .source_series_triage_overlap_sha256
                .as_deref(),
            source_series_activity_sha256: self.plan.source_series_activity_sha256.as_deref(),
            source_series_sha256: self.plan.source_series_sha256.as_deref(),
            passes,
        };
        publish_receipt(self.output, "sweep-series.json", &receipt)?;
        let mut stdout = io::stdout().lock();
        writeln!(
            stdout,
            "Completed {} sweep passes ({} dwells). Output: {}",
            receipt.passes.len(),
            receipt.dwells,
            self.output.display()
        )?;
        stdout.flush().context("flush sweep series result")
    }

    fn analyze_passes(
        &self,
        evidence: Option<&timing::Evidence>,
        stop: &AtomicBool,
    ) -> Result<Vec<PassReceipt>> {
        let mut passes = Vec::with_capacity(self.plan.passes());
        for (index, requests) in self
            .requests
            .chunks_exact(self.plan.visits().len())
            .enumerate()
        {
            let directory = pass_directory_name(index);
            let output = self.output.join(&directory);
            let started = Instant::now();
            let tunes = analyze_prefix(
                requests,
                self.plan,
                self.spectrum_output,
                evidence.is_some(),
                stop,
            )
            .with_context(|| format!("process {directory}"))?;
            if tunes.len() < requests.len() {
                break;
            }
            verify_coverage(self.plan, &tunes, &output)?;
            ensure!(
                !stop.load(Ordering::Relaxed),
                "sweep series interrupted while processing {directory}"
            );
            let acquisition = evidence.map(|evidence| evidence.pass_spans[index].as_secs_f64());
            let analysis = started.elapsed().as_secs_f64();
            let timing = Elapsed {
                elapsed_seconds: acquisition.map(|seconds| seconds + analysis),
                acquisition_elapsed_seconds: acquisition,
                analysis_elapsed_seconds: analysis,
            };
            let receipt = CompletedSweep::new(
                self.plan,
                requests[0].settings,
                &tunes,
                timing,
                None,
                self.spectrum_output,
            );
            let mut file = publish_receipt(&output, "sweep.json", &receipt)?;
            file.rewind()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            passes.push(PassReceipt {
                directory,
                sweep_sha256: format!("{:x}", Sha256::digest(&bytes)),
            });
        }
        Ok(passes)
    }
}
