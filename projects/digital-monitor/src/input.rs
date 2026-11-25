//! Strict validation of one completed SC16_Q11 capture and its sidecar.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use serde::{Deserialize, Serialize};

const BYTES_PER_COMPLEX_SAMPLE: u64 = 4;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;
const IO_BUFFER_BYTES: usize = 64 * 1024;
const Q11_SCALE: f64 = 2048.0;
pub(crate) const NORMALIZED_SC16_Q11_FORMAT: &str = "SC16_Q11_LE";

#[derive(Debug, Deserialize)]
struct Acquisition {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: f64,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    num_samples: u64,
    #[serde(default)]
    discarded_initial_samples: u64,
    gain_db: Option<f64>,
    gain_mode: Option<String>,
    bias_tee_enabled: Option<bool>,
    channel: Option<String>,
    fpga_version: Option<String>,
    required_fpga_version: Option<String>,
}

/// Additional facts required before a newly acquired file may enter the live
/// watch pipeline. These fields are checked but are not copied into the
/// privacy-minimized [`SourceReport`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct LiveExpectation<'a> {
    pub(crate) center_frequency_hz: u64,
    pub(crate) sample_rate_hz: u64,
    pub(crate) exact_samples: u64,
    pub(crate) discarded_initial_samples: u64,
    pub(crate) gain_db: f64,
    pub(crate) channel: &'a str,
    pub(crate) fpga_version: &'a str,
}

/// Stable, privacy-minimized identity of a validated capture.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SourceReport {
    pub(crate) input: PathBuf,
    pub(crate) sidecar: PathBuf,
    pub(crate) format: &'static str,
    pub(crate) center_frequency_hz: u64,
    pub(crate) sample_rate_hz: u64,
    pub(crate) bandwidth_hz: u64,
    pub(crate) complex_samples: u64,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) sidecar_sha256: String,
    pub(crate) continuity_guaranteed: bool,
}

fn appended_json(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    PathBuf::from(name)
}

fn exact_positive_integer(value: f64, label: &str) -> Result<u64> {
    ensure!(
        value.is_finite() && value > 0.0,
        "{label} must be finite and positive"
    );
    let rounded = value.round();
    ensure!(
        (value - rounded).abs() <= 0.001 && rounded <= u64::MAX as f64,
        "{label} must be an integer number of hertz"
    );
    Ok(rounded as u64)
}

fn read_sidecar(path: &Path) -> Result<(Acquisition, String)> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open acquisition sidecar {}", path.display()))?;
    let before = file.metadata()?;
    ensure!(
        before.is_file(),
        "acquisition sidecar must be a regular file"
    );
    ensure!(
        before.len() <= MAX_SIDECAR_BYTES,
        "acquisition sidecar exceeds {MAX_SIDECAR_BYTES} bytes"
    );
    let before_modified = before.modified().ok();
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read acquisition sidecar {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_SIDECAR_BYTES,
        "acquisition sidecar exceeds {MAX_SIDECAR_BYTES} bytes"
    );
    let after = file.metadata()?;
    ensure!(
        after.len() == before.len()
            && before_modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "acquisition sidecar changed while reading"
    );
    let digest = HEXLOWER.encode(digest::digest(&SHA256, &bytes).as_ref());
    let acquisition = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse acquisition sidecar {}", path.display()))?;
    Ok((acquisition, digest))
}

/// Check the alignment and component range of a little-endian SC16_Q11 chunk.
pub(crate) fn validate_q11(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes
            .len()
            .is_multiple_of(BYTES_PER_COMPLEX_SAMPLE as usize),
        "internal IQ validation lost sample alignment"
    );
    // A non-short-circuit reduction lets the compiler vectorize the range check.
    let in_range = bytes.as_chunks::<2>().0.iter().fold(true, |valid, raw| {
        valid & (-2048..=2047).contains(&i16::from_le_bytes(*raw))
    });
    ensure!(
        in_range,
        "IQ contains a component outside the SC16_Q11 range"
    );
    Ok(())
}

fn validate_and_hash_iq(path: &Path, expected_bytes: u64) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open IQ input {}", path.display()))?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "IQ input must be a regular file");
    ensure!(
        before.len() == expected_bytes,
        "IQ byte length does not match the sidecar sample count"
    );
    let before_modified = before.modified().ok();
    let mut digest = digest::Context::new(&SHA256);
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    let mut remaining = expected_bytes;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(IO_BUFFER_BYTES as u64))?;
        file.read_exact(&mut buffer[..count])
            .context("read complete IQ input")?;
        digest.update(&buffer[..count]);
        validate_q11(&buffer[..count])?;
        remaining -= count as u64;
    }
    let after = file.metadata()?;
    ensure!(
        after.len() == before.len()
            && before_modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "IQ input changed while validating"
    );
    Ok(HEXLOWER.encode(digest.finish().as_ref()))
}

/// Validate the complete file/sidecar contract before any decoder is started.
pub(crate) fn validate(
    input: &Path,
    requested_center_hz: u64,
    requested_sample_rate_hz: u64,
    max_samples: u64,
) -> Result<SourceReport> {
    validate_with_iq(
        input,
        requested_center_hz,
        requested_sample_rate_hz,
        max_samples,
        |path, bytes, _| validate_and_hash_iq(path, bytes),
    )
}

/// Validate acquisition metadata before delegating the complete IQ read.
///
/// The verifier receives the canonical IQ path, expected byte count and
/// acquisition sidecar SHA-256. It must validate the file extent and Q11
/// samples and return the SHA-256 of the stable file it read.
pub(crate) fn validate_with_iq(
    input: &Path,
    requested_center_hz: u64,
    requested_sample_rate_hz: u64,
    max_samples: u64,
    verify_iq: impl FnOnce(&Path, u64, &str) -> Result<String>,
) -> Result<SourceReport> {
    ensure!(max_samples > 0, "max-samples must be positive");
    let input = input
        .canonicalize()
        .with_context(|| format!("resolve IQ input {}", input.display()))?;
    let sidecar = appended_json(&input);
    let sidecar = sidecar
        .canonicalize()
        .with_context(|| format!("resolve acquisition sidecar {}", sidecar.display()))?;
    ensure!(
        input.to_str().is_some() && sidecar.to_str().is_some(),
        "IQ input and sidecar paths must be valid UTF-8 for the JSON manifest"
    );
    let (acquisition, sidecar_sha256) = read_sidecar(&sidecar)?;
    ensure!(
        acquisition.format == "SC16_Q11"
            && acquisition.dtype == "<i2"
            && acquisition.interleave == "I,Q"
            && acquisition.complex_scale == Q11_SCALE,
        "requires SC16_Q11 little-endian int16 I,Q metadata with scale 2048"
    );
    let center_frequency_hz = exact_positive_integer(acquisition.frequency_hz, "center frequency")?;
    let sample_rate_hz = exact_positive_integer(acquisition.sample_rate_hz, "sample rate")?;
    let bandwidth_hz = exact_positive_integer(acquisition.bandwidth_hz, "bandwidth")?;
    ensure!(
        center_frequency_hz == requested_center_hz,
        "sidecar center frequency differs from --center-mhz"
    );
    ensure!(
        sample_rate_hz == requested_sample_rate_hz,
        "sidecar sample rate differs from --sample-rate"
    );
    ensure!(
        bandwidth_hz == requested_sample_rate_hz,
        "sidecar bandwidth must equal the requested sample rate"
    );
    ensure!(
        acquisition.num_samples >= 8192 && acquisition.num_samples <= max_samples,
        "sidecar sample count must be 8192..=max-samples"
    );
    let bytes = acquisition
        .num_samples
        .checked_mul(BYTES_PER_COMPLEX_SAMPLE)
        .context("IQ byte count overflow")?;
    let sha256 = verify_iq(&input, bytes, &sidecar_sha256)?;
    Ok(SourceReport {
        input,
        sidecar,
        format: NORMALIZED_SC16_Q11_FORMAT,
        center_frequency_hz,
        sample_rate_hz,
        bandwidth_hz,
        complex_samples: acquisition.num_samples,
        bytes,
        sha256,
        sidecar_sha256,
        continuity_guaranteed: false,
    })
}

/// Validate a just-completed receive-only acquisition before any decoder is
/// started. Ordinary offline analysis deliberately does not impose these
/// machine-specific live-capture facts.
pub(crate) fn validate_live(
    input: &Path,
    expectation: LiveExpectation<'_>,
) -> Result<SourceReport> {
    ensure!(
        expectation.gain_db.is_finite(),
        "expected live gain must be finite"
    );
    let parent = input
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "live capture parent must be a direct directory"
    );
    let sidecar = appended_json(input);
    for (path, label) in [
        (input, "IQ input"),
        (sidecar.as_path(), "acquisition sidecar"),
    ] {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect live {label} {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "live {label} must be a direct regular file"
        );
        ensure!(
            path.parent() == input.parent(),
            "live capture files must be direct siblings"
        );
    }
    let source = validate(
        input,
        expectation.center_frequency_hz,
        expectation.sample_rate_hz,
        expectation.exact_samples,
    )?;
    ensure!(
        source.complex_samples == expectation.exact_samples,
        "live capture sample count differs from the exact request"
    );
    let (acquisition, sidecar_sha256) = read_sidecar(&source.sidecar)?;
    ensure!(
        sidecar_sha256 == source.sidecar_sha256,
        "live acquisition sidecar changed during validation"
    );
    ensure!(
        acquisition.discarded_initial_samples == expectation.discarded_initial_samples,
        "live acquisition initial discard differs from the request"
    );
    ensure!(
        acquisition.channel.as_deref() == Some(expectation.channel),
        "live acquisition used a different receive channel"
    );
    ensure!(
        acquisition.bias_tee_enabled == Some(false),
        "live acquisition did not record the RX bias tee as disabled"
    );
    ensure!(
        acquisition.gain_mode.as_deref() == Some("manual")
            && acquisition
                .gain_db
                .is_some_and(|gain| gain.is_finite() && (gain - expectation.gain_db).abs() <= 0.01),
        "live acquisition gain mode or readback differs from the request"
    );
    ensure!(
        acquisition.fpga_version.as_deref() == Some(expectation.fpga_version)
            && acquisition.required_fpga_version.as_deref() == Some(expectation.fpga_version),
        "live acquisition observed or required FPGA version differs from the stock image"
    );
    Ok(source)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn fixture(directory: &Path, samples: usize) -> PathBuf {
        let input = directory.join("iq.sc16q11");
        let mut bytes = Vec::with_capacity(samples * 4);
        for index in 0..samples {
            let i = i16::try_from(index % 1024).unwrap();
            let q = -i;
            bytes.extend_from_slice(&i.to_le_bytes());
            bytes.extend_from_slice(&q.to_le_bytes());
        }
        fs::write(&input, bytes).unwrap();
        fs::write(
            appended_json(&input),
            serde_json::to_vec(&json!({
                "format":"SC16_Q11", "dtype":"<i2", "interleave":"I,Q",
                "complex_scale":2048, "frequency_hz":2_437_000_000.0,
                "sample_rate_hz":20_000_000.0, "bandwidth_hz":20_000_000.0,
                "num_samples":samples, "gain_db":40.0, "gain_mode":"manual",
                "bias_tee_enabled":false, "channel":"RX1", "fpga_version":"0.16.0",
                "required_fpga_version":"0.16.0",
                "device_serial":"must-not-be-copied"
            }))
            .unwrap(),
        )
        .unwrap();
        input
    }

    #[test]
    fn validates_complete_q11_extent_without_copying_extra_sidecar_fields() {
        let directory = tempfile::tempdir().unwrap();
        let input = fixture(directory.path(), 8192);
        let report = validate(&input, 2_437_000_000, 20_000_000, 8192).unwrap();
        assert_eq!(report.format, NORMALIZED_SC16_Q11_FORMAT);
        assert_eq!(report.complex_samples, 8192);
        assert_eq!(report.bytes, 32_768);
        assert_eq!(report.sha256.len(), 64);
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("must-not-be-copied"));
    }

    #[test]
    fn delegates_verified_extent_and_sidecar_identity_for_the_canonical_iq_path() {
        let directory = tempfile::tempdir().unwrap();
        let input = fixture(directory.path(), 8192);
        let baseline = validate(&input, 2_437_000_000, 20_000_000, 8192).unwrap();
        let alias = directory.path().join("alias.iq");
        std::os::unix::fs::symlink(&input, &alias).unwrap();
        let report = validate_with_iq(
            &alias,
            2_437_000_000,
            20_000_000,
            8192,
            |path, bytes, sidecar_sha256| {
                assert_eq!(path, baseline.input.as_path());
                assert_eq!(bytes, baseline.bytes);
                assert_eq!(sidecar_sha256, baseline.sidecar_sha256.as_str());
                validate_and_hash_iq(path, bytes)
            },
        )
        .unwrap();
        assert_eq!(report, baseline);
    }

    #[test]
    fn rejects_mismatches_and_out_of_range_q11() {
        let directory = tempfile::tempdir().unwrap();
        let input = fixture(directory.path(), 8192);
        assert!(validate(&input, 2_426_000_000, 20_000_000, 8192).is_err());
        assert!(validate(&input, 2_437_000_000, 20_000_000, 8191).is_err());
        let mut bytes = fs::read(&input).unwrap();
        bytes[..2].copy_from_slice(&2048i16.to_le_bytes());
        fs::write(&input, bytes).unwrap();
        assert!(validate(&input, 2_437_000_000, 20_000_000, 8192).is_err());
    }

    #[test]
    fn live_validation_requires_exact_safe_receive_facts() {
        let directory = tempfile::tempdir().unwrap();
        let input = fixture(directory.path(), 8192);
        let expected = LiveExpectation {
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: 20_000_000,
            exact_samples: 8192,
            discarded_initial_samples: 0,
            gain_db: 40.0,
            channel: "RX1",
            fpga_version: "0.16.0",
        };
        assert_eq!(
            validate_live(&input, expected).unwrap().complex_samples,
            8192
        );

        let sidecar = appended_json(&input);
        let baseline: serde_json::Value =
            serde_json::from_slice(&fs::read(&sidecar).unwrap()).unwrap();
        for (field, value) in [
            ("channel", json!("RX2")),
            ("bias_tee_enabled", json!(true)),
            ("gain_mode", json!("automatic")),
            ("gain_db", json!(39.0)),
            ("fpga_version", json!("0.15.0")),
            ("required_fpga_version", json!("0.15.0")),
            ("discarded_initial_samples", json!(2_000_000)),
        ] {
            let mut changed = baseline.clone();
            changed[field] = value;
            fs::write(&sidecar, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(validate_live(&input, expected).is_err(), "accepted {field}");
        }
        let mut warmed_sidecar = baseline.clone();
        warmed_sidecar["discarded_initial_samples"] = json!(2_000_000);
        fs::write(&sidecar, serde_json::to_vec(&warmed_sidecar).unwrap()).unwrap();
        let warmed = LiveExpectation {
            discarded_initial_samples: 2_000_000,
            ..expected
        };
        assert_eq!(validate_live(&input, warmed).unwrap().complex_samples, 8192);
        fs::write(&sidecar, serde_json::to_vec(&baseline).unwrap()).unwrap();
        assert!(validate_live(&input, warmed).is_err());
    }

    #[test]
    fn live_validation_rejects_symlinked_capture_files() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let input = fixture(directory.path(), 8192);
        let expected = LiveExpectation {
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: 20_000_000,
            exact_samples: 8192,
            discarded_initial_samples: 0,
            gain_db: 40.0,
            channel: "RX1",
            fpga_version: "0.16.0",
        };

        let external_iq = external.path().join("iq.sc16q11");
        fs::rename(&input, &external_iq).unwrap();
        std::os::unix::fs::symlink(&external_iq, &input).unwrap();
        assert!(validate_live(&input, expected).is_err());

        fs::remove_file(&input).unwrap();
        fs::rename(&external_iq, &input).unwrap();
        let sidecar = appended_json(&input);
        let external_sidecar = external.path().join("iq.sc16q11.json");
        fs::rename(&sidecar, &external_sidecar).unwrap();
        std::os::unix::fs::symlink(&external_sidecar, &sidecar).unwrap();
        assert!(validate_live(&input, expected).is_err());
    }
}
