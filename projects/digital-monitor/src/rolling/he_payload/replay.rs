//! Replay one sealed HE20 SU clip and publish its verified payload evidence.

use std::ffi::OsString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use nix::fcntl::{RenameFlags, renameat2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event_replay::replay::WifiTuning;
use crate::input::{self, NORMALIZED_SC16_Q11_FORMAT};
use crate::process::{self, ProgramProvenance};
use crate::report::{self, required_u64};

use super::evidence::{self, Expected};

const ARCHIVE: &str = "first-he-su-clip";
const PAYLOAD: &str = "first-he-su-payload";
const RECEIPT: &str = "payload.json";
const MANIFEST_LIMIT: u64 = 64 * 1024;
const LOG_LIMIT: u64 = 256 * 1024;
const ERROR_EXCERPT_BYTES: usize = 1024;
const IQ_LIMIT: u64 = 1024 * 1024;
const SAMPLE_RATE_HZ: u64 = 20_000_000;
const HE_SIGNAL_SAMPLES: u64 = 640;
const DECODE_BUDGET: Duration = Duration::from_secs(25);
const REPLAY_BUDGET: Duration = Duration::from_secs(35);

#[derive(Debug, Serialize)]
pub(super) struct ReplayEvidence {
    pub(super) validated_frames: u64,
    pub(super) receipt_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u8,
    operation: String,
    segment: u64,
    iq_file: String,
    sidecar_file: String,
    iq_sha256: String,
    sidecar_sha256: String,
    he_su_preamble_stream_sample: u64,
    clip_receipt_sha256: String,
    direct_receipt_sha256: String,
    he_su_signal_events_sha256: String,
}

struct Archive {
    directory: PathBuf,
    files: Vec<(PathBuf, Metadata)>,
}

impl Archive {
    fn open(directory: PathBuf) -> Result<Self> {
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir(),
            "first HE clip archive must be a direct directory"
        );
        Ok(Self {
            files: vec![(directory.clone(), metadata)],
            directory,
        })
    }

    fn json(&mut self, name: &str, expected_sha256: &str) -> Result<Value> {
        let path = self.directory.join(name);
        let (value, sha256, stamp) = report::read_json_with_snapshot(&path, MANIFEST_LIMIT)?;
        self.record(path, stamp, &sha256, expected_sha256)?;
        Ok(value)
    }

    fn hash(&mut self, name: &str, limit: u64, expected_sha256: &str) -> Result<u64> {
        let path = self.directory.join(name);
        let (digest, stamp) = report::hash_direct_regular_file_with_snapshot(&path, limit)?;
        self.record(path, stamp, &digest.sha256, expected_sha256)?;
        Ok(digest.bytes)
    }

    fn record(
        &mut self,
        path: PathBuf,
        stamp: Metadata,
        sha256: &str,
        expected_sha256: &str,
    ) -> Result<()> {
        ensure!(
            sha256 == expected_sha256,
            "HE archive digest mismatch for {}",
            path.display()
        );
        self.files.push((path, stamp));
        Ok(())
    }

    fn verify_unchanged(&self) -> Result<()> {
        for (path, stamp) in &self.files {
            let current = fs::symlink_metadata(path)?;
            ensure!(
                current.file_type() == stamp.file_type()
                    && report::same_file_identity(stamp, &current),
                "HE archive changed during payload replay at {}",
                path.display()
            );
        }
        Ok(())
    }
}

struct Clip {
    samples: u64,
    capture_frequency_hz: u64,
    center_frequency_hz: u64,
    double_q11: bool,
}

fn frequency_is_supported(frequency: u64) -> bool {
    (2_400_000_000..=2_480_000_000).contains(&frequency) && frequency.is_multiple_of(1_000_000)
}

fn read_clip(archive: &mut Archive, manifest: &Manifest) -> Result<Clip> {
    ensure!(
        manifest.schema_version == 1
            && manifest.operation == "rolling_he_su_clip_archive"
            && manifest.segment > 0
            && Path::new(&manifest.iq_file)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(manifest.iq_file.as_str())
            && manifest.iq_file.ends_with(".sc16q11")
            && manifest.sidecar_file == format!("{}.json", manifest.iq_file),
        "invalid first HE clip archive manifest"
    );
    let sidecar = archive.json(&manifest.sidecar_file, &manifest.sidecar_sha256)?;
    let direct = archive.json("direct.json", &manifest.direct_receipt_sha256)?;
    archive.hash("clips.json", MANIFEST_LIMIT, &manifest.clip_receipt_sha256)?;
    archive.hash(
        "candidates.he-su-signal-events.json",
        2 * IQ_LIMIT,
        &manifest.he_su_signal_events_sha256,
    )?;
    let bytes = archive.hash(&manifest.iq_file, IQ_LIMIT, &manifest.iq_sha256)?;
    let samples = required_u64(&sidecar, "num_samples")?;
    let capture_frequency_hz = required_u64(&sidecar, "frequency_hz")?;
    let stream_start = required_u64(&sidecar, "stream_start_sample")?;
    ensure!(
        sidecar["format"].as_str() == Some("SC16_Q11")
            && sidecar["dtype"].as_str() == Some("<i2")
            && sidecar["interleave"].as_str() == Some("I,Q")
            && sidecar["complex_scale"].as_u64() == Some(2048)
            && sidecar["source_kind"].as_str() == Some("derived_rolling_survey_clip")
            && required_u64(&sidecar, "sample_rate_hz")? == SAMPLE_RATE_HZ
            && required_u64(&sidecar, "bandwidth_hz")? == SAMPLE_RATE_HZ
            && frequency_is_supported(capture_frequency_hz)
            && samples.checked_mul(4) == Some(bytes)
            && manifest
                .he_su_preamble_stream_sample
                .checked_sub(stream_start)
                .and_then(|position| position.checked_add(HE_SIGNAL_SAMPLES))
                .is_some_and(|end| end <= samples),
        "HE clip has an invalid IQ extent or capture settings"
    );
    ensure!(
        matches!(direct["schema_version"].as_u64(), Some(1 | 2))
            && direct["operation"].as_str() == Some("rolling_direct_decode")
            && direct["source_format"].as_str() == Some(NORMALIZED_SC16_Q11_FORMAT)
            && required_u64(&direct, "sample_rate_hz")? == SAMPLE_RATE_HZ
            && required_u64(&direct, "bandwidth_hz")? == SAMPLE_RATE_HZ
            && required_u64(&direct, "center_frequency_hz")? == capture_frequency_hz
            && direct["wifi"]["he_su_signal_events_sha256"].as_str()
                == Some(manifest.he_su_signal_events_sha256.as_str()),
        "HE clip settings differ from the direct decode receipt"
    );
    let center_frequency_hz = match direct.get("wifi_tuning") {
        Some(tuning) => {
            let center = required_u64(tuning, "center_frequency_hz")?;
            ensure!(
                frequency_is_supported(center),
                "invalid direct Wi-Fi tuning"
            );
            let expected = WifiTuning::new(capture_frequency_hz, (center / 1_000_000) as u32)?
                .context("direct Wi-Fi tuning must describe a nonzero translation")?;
            ensure!(
                serde_json::to_value(expected)? == *tuning,
                "invalid archived Wi-Fi translation"
            );
            center
        }
        None => capture_frequency_hz,
    };
    let double_q11 = if let Some(gain) = direct["wifi"].get("q11_gain") {
        ensure!(
            required_u64(gain, "factor")? == 2 && required_u64(gain, "clipped_components").is_ok(),
            "invalid archived Wi-Fi Q11 gain"
        );
        true
    } else {
        false
    };
    // Validate all IQ components and the same sidecar contract as offline replay.
    let source = input::validate(
        &archive.directory.join(&manifest.iq_file),
        capture_frequency_hz,
        SAMPLE_RATE_HZ,
        IQ_LIMIT / 4,
    )?;
    ensure!(
        source.complex_samples == samples
            && source.sha256 == manifest.iq_sha256
            && source.sidecar_sha256 == manifest.sidecar_sha256,
        "verified IQ differs from the archived HE clip"
    );
    archive.verify_unchanged()?;
    Ok(Clip {
        samples,
        capture_frequency_hz,
        center_frequency_hz,
        double_q11,
    })
}

fn verify_programs(monitor: &ProgramProvenance, decoder: &ProgramProvenance) -> Result<()> {
    for program in [monitor, decoder] {
        ensure!(
            process::resolve_program(program.name, &program.path)? == *program,
            "HE payload program identity changed for {}",
            program.name
        );
    }
    Ok(())
}

fn log_tail(path: &Path) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let limit = ERROR_EXCERPT_BYTES as u64;
    file.seek(SeekFrom::Start(metadata.len().saturating_sub(limit)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let start = text.ceil_char_boundary(text.len().saturating_sub(ERROR_EXCERPT_BYTES));
    let tail = text[start..].trim();
    (!tail.is_empty()).then(|| tail.to_owned())
}

/// Revalidate the archived clip before and after the bounded wrapper run.
/// Publish the complete verified payload directory without replacing a result.
pub(super) fn run(
    output: &Path,
    archive_sha256: &str,
    monitor: &ProgramProvenance,
    decoder: &ProgramProvenance,
    stop: &AtomicBool,
) -> Result<ReplayEvidence> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "HE payload replay interrupted"
    );
    let mut archive = Archive::open(output.join(ARCHIVE))?;
    let manifest: Manifest = serde_json::from_value(archive.json("archive.json", archive_sha256)?)
        .context("parse first HE clip archive manifest")?;
    let clip = read_clip(&mut archive, &manifest)?;
    verify_programs(monitor, decoder)?;
    let stage = tempfile::Builder::new()
        .prefix(".he-su-payload-")
        .tempdir_in(output)?;
    report::private_directory(&stage.path().join("logs"))?;
    let decoded = stage.path().join("decoded");
    let iq = archive.directory.join(&manifest.iq_file);
    let mut arguments = vec![
        OsString::from("decode-he-iq"),
        iq.into_os_string(),
        "--format".into(),
        "sc16q11".into(),
        "--frequency-mhz".into(),
        (clip.center_frequency_hz / 1_000_000).to_string().into(),
        "--max-samples".into(),
        clip.samples.to_string().into(),
        "--decode-timeout".into(),
        DECODE_BUDGET.as_secs().to_string().into(),
        "--decoder".into(),
        decoder.path.as_os_str().to_owned(),
        "--output".into(),
        decoded.as_os_str().to_owned(),
    ];
    if clip.capture_frequency_hz != clip.center_frequency_hz {
        arguments.extend([
            "--capture-frequency-hz".into(),
            clip.capture_frequency_hz.to_string().into(),
        ]);
    }
    if clip.double_q11 {
        arguments.push("--double-q11".into());
    }
    let command = process::run(
        monitor,
        &arguments,
        stage.path(),
        "he-payload",
        LOG_LIMIT,
        REPLAY_BUDGET,
        stop,
    )?;
    if let Err(error) = process::require_success(&command) {
        let mut description = String::from("HE payload wrapper failed");
        for (label, path) in [
            ("wrapper stderr", stage.path().join(&command.stderr.path)),
            ("native decoder", decoded.join("decoder.log")),
        ] {
            if let Some(tail) = log_tail(&path) {
                description.push_str(&format!("\n{label}\n{tail}"));
            }
        }
        return Err(error).context(description);
    }
    ensure!(
        !command.stdout.truncated && !command.stderr.truncated,
        "HE payload wrapper exceeded its log budget"
    );
    ensure!(
        fs::symlink_metadata(&decoded)?.is_dir(),
        "HE payload output must be a direct directory"
    );
    let verified = evidence::verify(
        &decoded,
        Expected {
            samples: clip.samples,
            center_frequency_hz: clip.center_frequency_hz,
            capture_frequency_hz: clip.capture_frequency_hz,
            double_q11: clip.double_q11,
            decoder_sha256: &decoder.sha256,
        },
    )?;
    verify_programs(monitor, decoder)?;
    archive.verify_unchanged()?;
    let validated_frames = verified.validated_frames;
    report::publish_json(
        &decoded,
        RECEIPT,
        &json!({
            "schema_version": 1,
            "operation": "rolling_he_su_payload_replay",
            "archive_sha256": archive_sha256,
            "archive_iq_sha256": manifest.iq_sha256,
            "archive_sidecar_sha256": manifest.sidecar_sha256,
            "samples": clip.samples,
            "capture_frequency_hz": clip.capture_frequency_hz,
            "center_frequency_hz": clip.center_frequency_hz,
            "double_q11": clip.double_q11,
            "monitor_sha256": monitor.sha256,
            "decoder_sha256": decoder.sha256,
            "decoded": verified,
        }),
    )?;
    let receipt_sha256 =
        report::hash_direct_regular_file(&decoded.join(RECEIPT), MANIFEST_LIMIT)?.sha256;
    ensure!(
        !stop.load(Ordering::Relaxed),
        "HE payload publication interrupted"
    );
    let parent = File::open(output)?;
    let scratch = File::open(stage.path())?;
    renameat2(
        &scratch,
        "decoded",
        &parent,
        PAYLOAD,
        RenameFlags::RENAME_NOREPLACE,
    )
    .context("publish verified first HE payload")?;
    if let Err(error) = parent.sync_all() {
        renameat2(
            &parent,
            PAYLOAD,
            &scratch,
            "decoded",
            RenameFlags::RENAME_NOREPLACE,
        )
        .with_context(|| format!("withdraw HE payload after directory sync failed: {error}"))?;
        return Err(error).context("sync first HE payload publication");
    }
    Ok(ReplayEvidence {
        validated_frames,
        receipt_sha256,
    })
}
