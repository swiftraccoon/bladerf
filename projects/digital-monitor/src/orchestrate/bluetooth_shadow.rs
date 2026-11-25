use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::input::{NORMALIZED_SC16_Q11_FORMAT, SourceReport};
use crate::process::{self, CommandReport};

use super::{ProgramSet, Request, path, revalidate_programs, text, validate_request};

const CODED_SHADOW_CENTER_MHZ: u32 = 2435;
const CODED_SHADOW_FREQUENCY_HZ: u64 = 2_435_000_000;
const CODED_SHADOW_SAMPLE_RATE_HZ: u64 = 20_000_000;

fn coded_shadow_arguments(
    request: &Request,
    source: &SourceReport,
    destination: &Path,
) -> Vec<OsString> {
    vec![
        text("replay-iq"),
        path(&source.input),
        text("--center-mhz"),
        text(request.center_mhz),
        text("--sample-rate"),
        text(request.sample_rate_hz),
        text("--max-samples"),
        text(source.complex_samples),
        text("--squelch"),
        text(-45),
        text("--decoder-timeout"),
        text(request.decoder_timeout_seconds),
        text("--burst-limit-mib"),
        text(128),
        text("--output-limit-mib"),
        text(512),
        text("--coded-scan"),
        text("--output"),
        path(destination),
    ]
}

fn validate_shadow_contract(
    request: &Request,
    source: &SourceReport,
    destination: &Path,
) -> Result<u64> {
    let (center_frequency_hz, log_limit_bytes) = validate_request(request)?;
    ensure!(
        request.center_mhz == CODED_SHADOW_CENTER_MHZ
            && center_frequency_hz == CODED_SHADOW_FREQUENCY_HZ,
        "Bluetooth coded shadow requires a 2435 MHz capture"
    );
    ensure!(
        request.input == source.input,
        "Bluetooth coded shadow input differs from the accepted capture"
    );
    ensure!(
        source.format == NORMALIZED_SC16_Q11_FORMAT
            && source.center_frequency_hz == CODED_SHADOW_FREQUENCY_HZ
            && source.sample_rate_hz == CODED_SHADOW_SAMPLE_RATE_HZ
            && source.bandwidth_hz == CODED_SHADOW_SAMPLE_RATE_HZ
            && source.complex_samples <= request.max_samples,
        "Bluetooth coded shadow source settings differ from the eligible capture contract"
    );
    ensure!(
        request.output.parent().is_some()
            && destination.parent() == request.output.parent()
            && destination != request.output,
        "Bluetooth coded shadow output must be a distinct sibling of ordinary analysis"
    );
    Ok(log_limit_bytes)
}

/// Run one opt-in LE Coded replay over an already accepted 2435 MHz capture.
///
/// The caller validates the resulting files separately. This helper freezes the
/// same wrapper executable set used by ordinary analysis, supervises the child
/// as a process group and revalidates every executable after it exits.
pub(crate) fn run_bluetooth_coded_shadow(
    request: &Request,
    expected: &ProgramSet,
    source: &SourceReport,
    destination: &Path,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    let log_limit_bytes = validate_shadow_contract(request, source, destination)?;
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect Bluetooth coded-shadow output"),
        Ok(_) => anyhow::bail!("Bluetooth coded-shadow output already exists"),
    }
    revalidate_programs(request, expected)?;
    let command = process::run(
        &expected.0.bluetooth,
        &coded_shadow_arguments(request, source, destination),
        &request.output,
        "bluetooth-coded-shadow",
        log_limit_bytes,
        Duration::from_secs(request.child_timeout_seconds),
        stop,
    );
    let identity = revalidate_programs(request, expected);
    let command = command?;
    identity?;
    process::require_success(&command)?;
    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::orchestrate::ProgramPaths;

    fn request() -> Request {
        Request {
            input: PathBuf::from("/session/iq.sc16q11"),
            output: PathBuf::from("/session/analysis"),
            center_mhz: CODED_SHADOW_CENTER_MHZ,
            sample_rate_hz: CODED_SHADOW_SAMPLE_RATE_HZ,
            max_samples: 20_000_000,
            spectrum_windows: 128,
            decoder_timeout_seconds: 120,
            child_timeout_seconds: 180,
            log_limit_mib: 1,
            programs: ProgramPaths {
                wifi: "wifi".into(),
                rf: "rf".into(),
                ieee: "ieee".into(),
                ieee_worker: "ieee-worker".into(),
                bluetooth: "bluetooth".into(),
            },
        }
    }

    fn source() -> SourceReport {
        SourceReport {
            input: PathBuf::from("/session/iq.sc16q11"),
            sidecar: PathBuf::from("/session/iq.sc16q11.json"),
            format: NORMALIZED_SC16_Q11_FORMAT,
            center_frequency_hz: CODED_SHADOW_FREQUENCY_HZ,
            sample_rate_hz: CODED_SHADOW_SAMPLE_RATE_HZ,
            bandwidth_hz: CODED_SHADOW_SAMPLE_RATE_HZ,
            complex_samples: 20_000_000,
            bytes: 80_000_000,
            sha256: "0".repeat(64),
            sidecar_sha256: "1".repeat(64),
            continuity_guaranteed: false,
        }
    }

    #[test]
    fn coded_shadow_arguments_are_file_only_and_bounded() {
        let request = request();
        let source = source();
        let destination = Path::new("/session/bluetooth-coded-shadow");
        validate_shadow_contract(&request, &source, destination).unwrap();
        let arguments = coded_shadow_arguments(&request, &source, destination)
            .into_iter()
            .map(|argument| argument.into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "replay-iq",
                "/session/iq.sc16q11",
                "--center-mhz",
                "2435",
                "--sample-rate",
                "20000000",
                "--max-samples",
                "20000000",
                "--squelch",
                "-45",
                "--decoder-timeout",
                "120",
                "--burst-limit-mib",
                "128",
                "--output-limit-mib",
                "512",
                "--coded-scan",
                "--output",
                "/session/bluetooth-coded-shadow",
            ]
        );
    }

    #[test]
    fn coded_shadow_contract_rejects_ineligible_or_unbound_inputs() {
        let destination = Path::new("/session/bluetooth-coded-shadow");
        let mut ineligible_request = request();
        ineligible_request.center_mhz = 2437;
        assert!(validate_shadow_contract(&ineligible_request, &source(), destination).is_err());

        let eligible_request = request();
        let mut unbound_source = source();
        unbound_source.input = "/other/iq.sc16q11".into();
        assert!(validate_shadow_contract(&eligible_request, &unbound_source, destination).is_err());

        let mut unnormalized_source = source();
        unnormalized_source.format = "SC16_Q11";
        assert!(
            validate_shadow_contract(&eligible_request, &unnormalized_source, destination).is_err()
        );

        let accepted_source = source();
        assert!(
            validate_shadow_contract(
                &eligible_request,
                &accepted_source,
                Path::new("/other/shadow")
            )
            .is_err()
        );
    }

    #[test]
    fn coded_shadow_bounds_replay_to_a_shorter_accepted_capture() {
        let request = request();
        let mut source = source();
        source.complex_samples = 10_000_000;
        source.bytes = 40_000_000;
        let arguments = coded_shadow_arguments(
            &request,
            &source,
            Path::new("/session/bluetooth-coded-shadow"),
        )
        .into_iter()
        .map(|argument| argument.into_string().unwrap())
        .collect::<Vec<_>>();
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--max-samples", "10000000"])
        );
    }
}
