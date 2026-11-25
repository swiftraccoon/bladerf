//! Supervise one saved-IQ waveform probe and retain only validated compact evidence.

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io::ErrorKind;
use std::ops::Range;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::process::{self, ProgramProvenance};
use crate::report;

const SOURCE_SAMPLE_RATE_HZ: u64 = 20_000_000;
const BASEBAND_SAMPLE_RATE_HZ: u64 = 2_000_000;
const SAMPLE_BYTES: u64 = 4;
const MAX_SOURCE_SAMPLES: u64 = 245_760;
const FILTER_CONTEXT_SAMPLES: u64 = 400;
const MIN_INTERVAL_SAMPLES: u64 = 8_192;
const DECIMATION: u64 = 10;
const FILTER_TAPS: u64 = 801;
const FILTER_CUTOFF_HZ: u64 = 350_000;
const MIN_SYMBOL_RATE_HZ: f64 = 8_000.0;
const MAX_SYMBOL_RATE_HZ: f64 = 200_000.0;
const MIN_SYMBOLS: usize = 12;
const REPORT_BYTES: u64 = 64 * 1024;
const LOG_BYTES: u64 = 64 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct ProbeRequest<'a> {
    pub(super) iq: &'a Path,
    pub(super) expected_sha256: &'a str,
    pub(super) source_samples: u64,
    pub(super) center_frequency_hz: u64,
    pub(super) target_frequency_hz: u64,
    pub(super) interval: Range<u64>,
    pub(super) report_path: &'a Path,
    pub(super) report_dir: &'a Path,
}

pub(super) struct ProbeResult {
    pub(super) report: ClipProbeReport,
    pub(super) stamp: Metadata,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Modulation {
    Bpsk,
    Qpsk,
    BinaryFsk,
}

impl Modulation {
    fn label_count(self) -> u8 {
        match self {
            Self::Bpsk | Self::BinaryFsk => 2,
            Self::Qpsk => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProbeOutcome {
    InsufficientModulationEvidence,
    AmbiguousModulationEvidence,
    InsufficientSymbols,
    NoQualifiedClock,
    AmbiguousClockCandidates,
    InsufficientTimingEvidence,
    AmbiguousSymbolRate,
    AmbiguousConstellation,
    InsufficientAmplitudeConsistency,
    Recovered,
}

#[derive(Serialize)]
pub(super) struct ClipProbeReport {
    pub(super) report_file: String,
    pub(super) report_sha256: String,
    pub(super) outcome: ProbeOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) modulation: Option<Modulation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) symbol_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) recovered_symbol_count: Option<usize>,
}

#[derive(Deserialize)]
struct ClockEvidence {
    transitions: u64,
    folded_phase_alignment: f64,
    early_alignment: f64,
    late_alignment: f64,
}

#[derive(Deserialize)]
struct BlindRecovery {
    outcome: ProbeOutcome,
    modulation: Option<Modulation>,
    symbol_rate_hz: Option<f64>,
    carrier_offset_hz: Option<f64>,
    first_center_baseband_sample: Option<f64>,
    clock: Option<ClockEvidence>,
    constellation_coherence: Option<f64>,
    amplitude_p20_p80_ratio: Option<f64>,
    symbols: Option<Vec<u8>>,
}

impl BlindRecovery {
    fn validate(&self, baseband_samples: u64) -> Result<Option<usize>> {
        if self.outcome != ProbeOutcome::Recovered {
            ensure!(
                self.symbol_rate_hz.is_none() && self.symbols.is_none(),
                "only a recovered waveform may publish a rate or symbols"
            );
            return Ok(None);
        }
        let labels = self
            .modulation
            .context("recovered symbols lack a modulation")?
            .label_count();
        let rate = self
            .symbol_rate_hz
            .filter(|value| (MIN_SYMBOL_RATE_HZ..=MAX_SYMBOL_RATE_HZ).contains(value))
            .context("recovered symbols lack a valid symbol rate")?;
        let symbols = self
            .symbols
            .as_ref()
            .context("recovered waveform lacks symbol labels")?;
        let first = self
            .first_center_baseband_sample
            .context("recovered waveform lacks its first symbol center")?;
        let last =
            first + symbols.len().saturating_sub(1) as f64 * BASEBAND_SAMPLE_RATE_HZ as f64 / rate;
        ensure!(
            symbols.len() >= MIN_SYMBOLS
                && symbols.iter().all(|&label| label < labels)
                && first >= 0.0
                && last < baseband_samples as f64,
            "recovered symbol labels or positions lie outside the analyzed interval"
        );
        let clock = self
            .clock
            .as_ref()
            .context("recovered waveform lacks clock evidence")?;
        ensure!(
            self.carrier_offset_hz
                .is_some_and(|value| value.abs() <= BASEBAND_SAMPLE_RATE_HZ as f64 / 2.0)
                && (1..=baseband_samples).contains(&clock.transitions)
                && [
                    Some(clock.folded_phase_alignment),
                    Some(clock.early_alignment),
                    Some(clock.late_alignment),
                    self.constellation_coherence,
                    self.amplitude_p20_p80_ratio
                ]
                .into_iter()
                .all(|fraction| fraction.is_some_and(|value| (0.0..=1.0).contains(&value))),
            "recovered waveform measurement is outside its numeric bounds"
        );
        Ok(Some(symbols.len()))
    }
}

#[derive(Deserialize)]
struct WaveformReport {
    schema_version: u32,
    operation: String,
    source_sha256: String,
    source_complex_samples: u64,
    source_sample_rate_hz: u64,
    center_frequency_hz: u64,
    target_frequency_hz: u64,
    start_sample: u64,
    end_sample_exclusive: u64,
    baseband_sample_rate_hz: u64,
    baseband_complex_samples: u64,
    decimation: u64,
    filter_taps: u64,
    filter_cutoff_hz: u64,
    filter_window: String,
    line_window: String,
    blind_symbol_recovery: BlindRecovery,
}

impl WaveformReport {
    fn validate(
        self,
        request: &ProbeRequest<'_>,
        report_file: String,
        report_sha256: String,
    ) -> Result<ClipProbeReport> {
        let interval_samples = request.interval.end - request.interval.start;
        ensure!(
            self.schema_version == 3
                && self.operation == "probe_saved_iq_waveform"
                && self.source_sha256 == request.expected_sha256
                && self.source_complex_samples == request.source_samples
                && self.source_sample_rate_hz == SOURCE_SAMPLE_RATE_HZ
                && self.center_frequency_hz == request.center_frequency_hz
                && self.target_frequency_hz == request.target_frequency_hz
                && self.start_sample == request.interval.start
                && self.end_sample_exclusive == request.interval.end
                && self.baseband_sample_rate_hz == BASEBAND_SAMPLE_RATE_HZ
                && self.baseband_complex_samples == interval_samples.div_ceil(DECIMATION)
                && self.decimation == DECIMATION
                && self.filter_taps == FILTER_TAPS
                && self.filter_cutoff_hz == FILTER_CUTOFF_HZ
                && self.filter_window == "hamming"
                && self.line_window == "hann",
            "waveform report differs from the requested source, interval or analysis shape"
        );
        let recovery = self.blind_symbol_recovery;
        Ok(ClipProbeReport {
            report_file,
            report_sha256,
            recovered_symbol_count: recovery.validate(self.baseband_complex_samples)?,
            outcome: recovery.outcome,
            modulation: recovery.modulation,
            symbol_rate_hz: recovery.symbol_rate_hz,
        })
    }
}

fn prepare_paths(request: &ProbeRequest<'_>) -> Result<(String, String)> {
    let directory = fs::symlink_metadata(request.report_dir).with_context(|| {
        format!(
            "inspect waveform-probe directory {}",
            request.report_dir.display()
        )
    })?;
    ensure!(
        directory.is_dir()
            && !directory.file_type().is_symlink()
            && directory.permissions().mode() & 0o777 == 0o700,
        "waveform-probe report directory must be a direct private directory"
    );
    ensure!(
        request.iq.parent() == Some(request.report_dir)
            && request.report_path.parent() == Some(request.report_dir),
        "waveform-probe IQ and report must be direct children of the report directory"
    );
    let report_file = request
        .report_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("waveform-probe report name must be UTF-8")?;
    ensure!(
        request
            .report_path
            .extension()
            .and_then(|value| value.to_str())
            == Some("json"),
        "waveform-probe report must have a .json suffix"
    );
    let log_stem = report_file
        .strip_suffix(".waveform.json")
        .context("waveform-probe report must have a .waveform.json suffix")?;
    ensure!(
        !log_stem.is_empty()
            && log_stem.len() <= 96
            && log_stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "waveform-probe report stem must contain only ASCII letters, digits, '-' or '_'"
    );
    match fs::symlink_metadata(request.report_path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Ok(_) => anyhow::bail!("waveform-probe report path already exists"),
        Err(error) => return Err(error).context("inspect waveform-probe report path"),
    }
    let logs = request.report_dir.join("logs");
    match fs::symlink_metadata(&logs) {
        Err(error) if error.kind() == ErrorKind::NotFound => report::private_directory(&logs)?,
        Ok(metadata) => ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.permissions().mode() & 0o777 == 0o700,
            "waveform-probe log directory must be a direct private directory"
        ),
        Err(error) => return Err(error).context("inspect waveform-probe log directory"),
    }
    Ok((report_file.to_owned(), log_stem.to_owned()))
}

fn validate_request(request: &ProbeRequest<'_>) -> Result<()> {
    ensure!(
        (1..=MAX_SOURCE_SAMPLES).contains(&request.source_samples),
        "waveform-probe source sample count is outside its bounded clip extent"
    );
    ensure!(
        request.center_frequency_hz > 0
            && request.target_frequency_hz > 0
            && request
                .center_frequency_hz
                .abs_diff(request.target_frequency_hz)
                <= SOURCE_SAMPLE_RATE_HZ / 2 - 500_000,
        "waveform-probe target lacks the required sampled-band margin"
    );
    let interval_samples = request
        .interval
        .end
        .checked_sub(request.interval.start)
        .context("waveform-probe interval is reversed")?;
    ensure!(
        interval_samples >= MIN_INTERVAL_SAMPLES
            && request.interval.start >= FILTER_CONTEXT_SAMPLES
            && request
                .interval
                .end
                .checked_add(FILTER_CONTEXT_SAMPLES)
                .is_some_and(|end| end <= request.source_samples),
        "waveform-probe interval lacks samples or filter context"
    );
    usize::try_from(request.interval.start)
        .context("waveform-probe interval start exceeds the child platform")?;
    usize::try_from(request.interval.end)
        .context("waveform-probe interval end exceeds the child platform")?;
    Ok(())
}

/// Run a file-only blind symbol-rate probe and validate its terminal report.
pub(super) fn probe(
    program: &ProgramProvenance,
    request: ProbeRequest<'_>,
    stop: &AtomicBool,
) -> Result<ProbeResult> {
    validate_request(&request)?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "waveform-probe analysis interrupted"
    );
    let (report_file, log_stem) = prepare_paths(&request)?;
    let source_bytes = request
        .source_samples
        .checked_mul(SAMPLE_BYTES)
        .context("waveform-probe source byte extent overflow")?;
    let (source_digest, source_stamp) =
        report::hash_direct_regular_file_with_snapshot(request.iq, source_bytes)?;
    ensure!(
        source_digest.bytes == source_bytes && source_digest.sha256 == request.expected_sha256,
        "staged waveform-probe IQ differs from its retained clip identity"
    );

    let arguments: Vec<OsString> = vec![
        "probe-waveform".into(),
        request.iq.as_os_str().to_owned(),
        "--center-hz".into(),
        request.center_frequency_hz.to_string().into(),
        "--target-hz".into(),
        request.target_frequency_hz.to_string().into(),
        "--start-sample".into(),
        request.interval.start.to_string().into(),
        "--end-sample".into(),
        request.interval.end.to_string().into(),
        "--expect-sha256".into(),
        request.expected_sha256.into(),
        "--search-symbol-rate".into(),
        "--output".into(),
        request.report_path.as_os_str().to_owned(),
    ];
    let command = process::run(
        program,
        &arguments,
        request.report_dir,
        &log_stem,
        LOG_BYTES,
        PROBE_TIMEOUT,
        stop,
    )?;
    process::require_success(&command).context("rolling retained-clip waveform probe")?;
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "waveform probe exceeded its retained log bound"
    );
    ensure!(
        process::resolve_program(program.name, &program.path)? == *program,
        "RF observatory executable changed during waveform probing"
    );
    let source_after = fs::symlink_metadata(request.iq)?;
    ensure!(
        source_after.is_file()
            && !source_after.file_type().is_symlink()
            && report::same_file_identity(&source_after, &source_stamp),
        "staged waveform-probe IQ changed during analysis"
    );
    let (value, report_sha256, stamp) =
        report::read_json_with_snapshot(request.report_path, REPORT_BYTES)?;
    let waveform: WaveformReport =
        serde_json::from_value(value).context("parse waveform-probe report")?;
    let compact = waveform.validate(&request, report_file, report_sha256)?;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "waveform-probe analysis interrupted"
    );
    let report_file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(request.report_path)?;
    ensure!(
        report::same_file_identity(&report_file.metadata()?, &stamp)
            && stamp.permissions().mode() & 0o777 == 0o600,
        "waveform-probe evidence changed or lacks private permissions"
    );
    report_file.sync_all()?;
    Ok(ProbeResult {
        report: compact,
        stamp,
    })
}
