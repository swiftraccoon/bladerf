//! Run the pinned offline analyzer on a selected, verified RX segment.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::input::SourceReport;
use crate::orchestrate::{self, ProgramPaths, ProgramSet, Request};
use crate::report::{self, OverallState};

const SAMPLE_RATE_HZ: u64 = 20_000_000;

pub(super) struct DecodePlan {
    programs: ProgramPaths,
    pinned: ProgramSet,
    center_mhz: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct DecodeReport {
    pub(super) state: OverallState,
    pub(super) analysis_sha256: String,
}

fn request(programs: ProgramPaths, center_mhz: u32, input: PathBuf, output: PathBuf) -> Request {
    Request {
        input,
        output,
        center_mhz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        max_samples: SAMPLE_RATE_HZ,
        spectrum_windows: 128,
        decoder_timeout_seconds: 120,
        child_timeout_seconds: 180,
        log_limit_mib: 1,
        programs,
    }
}

pub(super) fn prepare(programs: ProgramPaths, center_mhz: u32) -> Result<DecodePlan> {
    let pinned = orchestrate::preflight(&request(
        programs.clone(),
        center_mhz,
        PathBuf::new(),
        PathBuf::new(),
    ))
    .context("preflight rolling segment decoder programs")?;
    Ok(DecodePlan {
        programs,
        pinned,
        center_mhz,
    })
}

impl DecodePlan {
    /// Return a verified terminal receipt, including failed or interrupted analyses.
    pub(super) fn run(
        &self,
        source: &SourceReport,
        output: &Path,
        stop: &AtomicBool,
    ) -> Result<DecodeReport> {
        let request = request(
            self.programs.clone(),
            self.center_mhz,
            source.input.clone(),
            output.to_path_buf(),
        );
        let (output, state) = orchestrate::analyze_pinned(request, &self.pinned, stop)
            .context("analyze selected rolling RX segment")?;
        let (analysis, analysis_sha256) =
            report::read_json_with_sha256(&output.join("analysis.json"))?;
        ensure!(
            analysis["schema_version"].as_u64() == Some(2)
                && analysis["operation"].as_str() == Some("offline_multi_protocol_analysis")
                && analysis["overall_state"] == serde_json::to_value(state)?,
            "rolling decode manifest schema, operation or terminal state differs from analysis"
        );
        ensure!(
            analysis["source"] == serde_json::to_value(source)?,
            "rolling decode manifest source differs from the verified segment"
        );
        Ok(DecodeReport {
            state,
            analysis_sha256,
        })
    }
}
