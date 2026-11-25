use super::{
    AnalysisStep, AtomicBool, BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY, BluetoothCodedShadow,
    BluetoothCodedShadowMetrics, BluetoothDuplicateAccounting, BluetoothMetrics,
    BurstFamilyAccounting, BurstFamilyCount, BurstFamilyMetrics, BurstFamilySettings,
    BurstFeatureSummaries, BurstFeatureSummary, BurstFrequencyBin, CompactAnalysis, CompactStage,
    Context, Digest, File, HASH_BUFFER_BYTES, IeeeChannelMetrics, IeeeHypothesisMetrics,
    IeeeMembershipPartitionMetrics, IeeeMetrics, MAX_MANIFEST_BYTES, MAX_NESTED_DECODER_BYTES,
    MetadataExt, NestedDecoderProgram, Ordering, OverallState, Path, ProgramSet, ProtocolMetrics,
    Read, Request, Result, RfMetrics, RfRegionAggregate, Sha256, SourceReport, StepState, Value,
    WIFI_BUFFER_SAMPLE_LIMIT, WIFI_PENDING_FRAME_LIMIT, WifiDecoderHealth, WifiMetrics,
    WifiOfdmDecoderHealth, WifiPhyDecoderHealth, analysis_request, ensure, fs, orchestrate, report,
};

mod bluetooth;
mod burst;
mod coded_shadow;
mod ieee802154;
mod rf;
mod wifi;

#[cfg(test)]
pub(super) use bluetooth::bluetooth_duplicate_accounting;
pub(super) use bluetooth::bluetooth_metrics;
use burst::burst_family_metrics;
use coded_shadow::bluetooth_coded_shadow_metrics;
#[cfg(test)]
pub(super) use ieee802154::ieee_membership_metrics;
use ieee802154::ieee_metrics;
use rf::rf_metrics;
pub(super) use wifi::wifi_metrics;

pub(super) fn hash_regular_file(path: &Path, maximum: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "manifest is not a regular file");
    ensure!(
        (1..=maximum).contains(&before.len()),
        "manifest size is out of bounds"
    );
    let modified = before.modified().ok();
    let mut digest = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    let mut remaining = before.len();
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..wanted])?;
        digest.update(&buffer[..wanted]);
        remaining -= wanted as u64;
    }
    let after = file.metadata()?;
    ensure!(
        after.len() == before.len()
            && modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "manifest changed while hashing"
    );
    Ok(format!("{:x}", digest.finalize()))
}

pub(super) fn compact_analysis(
    path: &Path,
    expected: OverallState,
    source: &SourceReport,
) -> Result<CompactAnalysis> {
    let before = hash_regular_file(path, MAX_MANIFEST_BYTES)?;
    let value = report::read_json(path)?;
    let after = hash_regular_file(path, MAX_MANIFEST_BYTES)?;
    ensure!(
        before == after,
        "analysis manifest changed while summarizing"
    );
    ensure!(
        value.get("operation").and_then(Value::as_str) == Some("offline_multi_protocol_analysis"),
        "analysis manifest operation differs"
    );
    ensure!(
        value.get("schema_version").and_then(Value::as_u64) == Some(2),
        "analysis manifest schema differs"
    );
    let manifest_source = value
        .get("source")
        .context("analysis manifest lacks source identity")?;
    ensure!(
        manifest_source.get("sha256").and_then(Value::as_str) == Some(source.sha256.as_str())
            && manifest_source
                .get("sidecar_sha256")
                .and_then(Value::as_str)
                == Some(source.sidecar_sha256.as_str())
            && manifest_source
                .get("complex_samples")
                .and_then(Value::as_u64)
                == Some(source.complex_samples)
            && manifest_source.get("bytes").and_then(Value::as_u64) == Some(source.bytes)
            && manifest_source
                .get("center_frequency_hz")
                .and_then(Value::as_u64)
                == Some(source.center_frequency_hz)
            && manifest_source
                .get("sample_rate_hz")
                .and_then(Value::as_u64)
                == Some(source.sample_rate_hz)
            && manifest_source.get("bandwidth_hz").and_then(Value::as_u64)
                == Some(source.bandwidth_hz),
        "analysis manifest source identity differs from the accepted capture"
    );
    let expected_state = match expected {
        OverallState::Complete => "complete",
        OverallState::Failed => "failed",
        OverallState::Interrupted => "interrupted",
    };
    ensure!(
        value.get("overall_state").and_then(Value::as_str) == Some(expected_state),
        "analysis manifest state differs from the analyzer result"
    );
    let values = value
        .get("stages")
        .and_then(Value::as_array)
        .context("analysis manifest lacks stages")?;
    let expected_names = [
        "wifi",
        "rf_survey",
        "ieee802154",
        "bluetooth",
        "burst_families",
        "terminal_integrity",
    ];
    ensure!(
        values.len() == expected_names.len(),
        "analysis manifest has a different stage set"
    );
    let mut stages = Vec::with_capacity(values.len());
    for (stage, expected_name) in values.iter().zip(expected_names) {
        let name = stage
            .get("name")
            .and_then(Value::as_str)
            .context("stage lacks name")?;
        let state = stage
            .get("state")
            .and_then(Value::as_str)
            .context("stage lacks state")?;
        ensure!(
            name == expected_name
                && matches!(state, "complete" | "failed" | "interrupted" | "skipped"),
            "analysis manifest stage name, order or state differs"
        );
        if expected == OverallState::Complete {
            ensure!(
                state == "complete",
                "complete analysis contains an incomplete stage"
            );
        }
        let outcome = stage
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_owned);
        ensure!(
            if expected_name == "terminal_integrity" {
                outcome.is_none()
            } else {
                outcome.as_deref().is_none_or(|value| {
                    matches!(
                        value,
                        "evidence_found"
                            | "candidates_without_valid_frames"
                            | "no_valid_candidates"
                            | "unclassified_activity"
                            | "no_unclassified_activity_detected"
                            | "completed_zero_candidates"
                    )
                })
            },
            "analysis manifest stage outcome differs from the fixed vocabulary"
        );
        stages.push(CompactStage {
            name: name.to_owned(),
            state: state.to_owned(),
            outcome,
        });
    }
    Ok(CompactAnalysis {
        overall_state: expected_state.to_owned(),
        manifest_sha256: before,
        stages,
        metrics: None,
        coded_shadow: None,
    })
}

pub(super) fn terminal_outcome<'a>(
    analysis: &'a CompactAnalysis,
    stage_name: &str,
) -> Result<&'a str> {
    analysis
        .stages
        .iter()
        .find(|stage| stage.name == stage_name)
        .and_then(|stage| stage.outcome.as_deref())
        .with_context(|| format!("terminal analysis lacks {stage_name} outcome"))
}

pub(super) fn require_outcome(
    actual: crate::report::Outcome,
    expected: &str,
    stage: &str,
) -> Result<()> {
    ensure!(
        actual.as_str() == expected,
        "{stage} metrics classify as {}, not terminal outcome {expected}",
        actual.as_str()
    );
    Ok(())
}

pub(super) fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .with_context(|| format!("summary lacks boolean {field}"))
}

pub(super) fn required_f64(value: &Value, field: &str) -> Result<f64> {
    let number = value
        .get(field)
        .and_then(Value::as_f64)
        .with_context(|| format!("summary lacks number {field}"))?;
    ensure!(number.is_finite(), "summary field {field} is not finite");
    Ok(number)
}

pub(super) fn required_sha256(value: &Value, field: &str) -> Result<String> {
    let digest = value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("summary lacks string {field}"))?;
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "summary field {field} is not a lowercase SHA-256 digest"
    );
    Ok(digest.to_owned())
}

pub(super) fn stable_regular_file_len(path: &Path) -> Result<u64> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect derived file {}", path.display()))?;
    ensure!(
        before.is_file() && !before.file_type().is_symlink(),
        "derived file must be a direct regular file"
    );
    let file = File::open(path).with_context(|| format!("open derived file {}", path.display()))?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    ensure!(
        opened.dev() == before.dev()
            && opened.ino() == before.ino()
            && opened.len() == before.len()
            && opened.mtime() == before.mtime()
            && opened.mtime_nsec() == before.mtime_nsec()
            && opened.ctime() == before.ctime()
            && opened.ctime_nsec() == before.ctime_nsec()
            && after.is_file()
            && !after.file_type().is_symlink()
            && after.dev() == opened.dev()
            && after.ino() == opened.ino()
            && after.len() == opened.len()
            && after.mtime() == opened.mtime()
            && after.mtime_nsec() == opened.mtime_nsec()
            && after.ctime() == opened.ctime()
            && after.ctime_nsec() == opened.ctime_nsec(),
        "derived file identity changed while inspecting its extent"
    );
    Ok(opened.len())
}

pub(super) fn protocol_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
    spectrum_windows: usize,
) -> Result<ProtocolMetrics> {
    Ok(ProtocolMetrics {
        metrics_schema_version: 14,
        wifi: wifi_metrics(output, source, terminal)?,
        rf_survey: rf_metrics(output, source, terminal, spectrum_windows)?,
        ieee802154: ieee_metrics(output, source, terminal)?,
        bluetooth: bluetooth_metrics(output, source, terminal)?,
        burst_families: burst_family_metrics(output, source, terminal)?,
    })
}

pub(super) fn attach_protocol_metrics(
    output: &Path,
    source: &SourceReport,
    mut compact: CompactAnalysis,
    spectrum_windows: usize,
) -> std::result::Result<(CompactAnalysis, NestedDecoderProgram), Box<AnalysisStep>> {
    let metrics = match protocol_metrics(output, source, &compact, spectrum_windows) {
        Ok(metrics) => metrics,
        Err(error) => {
            return Err(Box::new(AnalysisStep {
                state: StepState::Failed,
                compact: Some(compact),
                detail: Some(format!(
                    "protocol metric validation failed before retention eligibility: {error:#}"
                )),
            }));
        }
    };
    let ordinary_decoder = metrics.bluetooth.decoder_program.clone();
    compact.metrics = Some(metrics);
    Ok((compact, ordinary_decoder))
}

pub(super) fn analyze_once(
    request: &Request,
    programs: &ProgramSet,
    source: &SourceReport,
    session: &Path,
    stop: &AtomicBool,
) -> AnalysisStep {
    let center_mhz = match source
        .center_frequency_hz
        .checked_div(1_000_000)
        .filter(|_| source.center_frequency_hz.is_multiple_of(1_000_000))
        .and_then(|center| u32::try_from(center).ok())
    {
        Some(center_mhz) => center_mhz,
        None => {
            return AnalysisStep {
                state: StepState::Failed,
                compact: None,
                detail: Some(
                    "accepted capture center is not an exact whole-MHz analysis center".into(),
                ),
            };
        }
    };
    let result = orchestrate::analyze_pinned(
        analysis_request(
            request,
            center_mhz,
            source.input.clone(),
            session.join("analysis"),
        ),
        programs,
        stop,
    );
    let (output, overall) = match result {
        Ok(result) => result,
        Err(error) => {
            return AnalysisStep {
                state: if stop.load(Ordering::Relaxed) {
                    StepState::Interrupted
                } else {
                    StepState::Failed
                },
                compact: None,
                detail: Some(format!("{error:#}")),
            };
        }
    };
    let mut compact = match compact_analysis(&output.join("analysis.json"), overall, source) {
        Ok(compact) => compact,
        Err(error) => {
            return AnalysisStep {
                state: StepState::Failed,
                compact: None,
                detail: Some(format!("terminal analysis validation failed: {error:#}")),
            };
        }
    };
    if overall == OverallState::Complete {
        let ordinary_decoder;
        (compact, ordinary_decoder) =
            match attach_protocol_metrics(&output, source, compact, request.spectrum_windows) {
                Ok(attached) => attached,
                Err(failed) => return *failed,
            };
        if request.bluetooth_coded_shadow {
            if center_mhz == 2435 {
                let destination = session.join("bluetooth-coded-shadow");
                let shadow_request =
                    analysis_request(request, center_mhz, source.input.clone(), output.clone());
                if let Err(error) = orchestrate::run_bluetooth_coded_shadow(
                    &shadow_request,
                    programs,
                    source,
                    &destination,
                    stop,
                ) {
                    return AnalysisStep {
                        state: if stop.load(Ordering::Relaxed) {
                            StepState::Interrupted
                        } else {
                            StepState::Failed
                        },
                        compact: Some(compact),
                        detail: Some(format!("Bluetooth coded shadow failed: {error:#}")),
                    };
                }
                let shadow = match bluetooth_coded_shadow_metrics(
                    &destination,
                    source,
                    &ordinary_decoder,
                ) {
                    Ok(metrics) => metrics,
                    Err(error) => {
                        return AnalysisStep {
                            state: StepState::Failed,
                            compact: Some(compact),
                            detail: Some(format!(
                                "Bluetooth coded-shadow validation failed before retention eligibility: {error:#}"
                            )),
                        };
                    }
                };
                compact.coded_shadow = Some(BluetoothCodedShadow::Complete {
                    metrics: Box::new(shadow),
                });
            } else {
                compact.coded_shadow = Some(BluetoothCodedShadow::NotEligible {
                    center_frequency_hz: source.center_frequency_hz,
                });
            }
        }
    }
    AnalysisStep {
        state: match overall {
            OverallState::Complete => StepState::Complete,
            OverallState::Failed => StepState::Failed,
            OverallState::Interrupted => StepState::Interrupted,
        },
        compact: Some(compact),
        detail: None,
    }
}
