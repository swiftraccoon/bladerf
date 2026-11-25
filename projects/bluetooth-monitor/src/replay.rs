//! Bounded offline replay of retained bladeRF SC16_Q11 IQ through Blue Dragon.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    CodedScanCoverage, DecoderChild, Engine, bursts, diagnostics, json_file, new_directory,
    output_path, packets,
};

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const NUM_CHANNELS: u32 = SAMPLE_RATE_HZ / 1_000_000;
const CHANNELIZER_STEP_COMPLEX_SAMPLES: u64 = (NUM_CHANNELS / 2) as u64;
const CODED_SCAN_STEP_CHANNEL_SAMPLES: u64 = 32_768;
const CODED_SCAN_OVERLAP_CHANNEL_SAMPLES: u64 = 16_384;
const COMPLETE_CODED_SCAN_OVERLAP_CHANNEL_SAMPLES: u64 = 34_080;
const CODED_SCAN_STRIDE_COMPLEX_SAMPLES: u64 =
    CODED_SCAN_STEP_CHANNEL_SAMPLES * CHANNELIZER_STEP_COMPLEX_SAMPLES;
const DECODER_BLOCK_COMPLEX_SAMPLES: u64 =
    65_536 / CHANNELIZER_STEP_COMPLEX_SAMPLES * CHANNELIZER_STEP_COMPLEX_SAMPLES;
const MAX_SAMPLES: u64 = 200_000_000;
const MAX_SIDECAR_BYTES: u64 = 65_536;
const EMPTY_BURST_ARCHIVE_BYTES: u64 = 16;
const MAX_DECODER_BYTES: u64 = 512 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const REPLAY_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OFFLINE_PATCH: &str = "patches/blue-dragon-sc16q11-file-source.patch";
const PRIMARY_ADVERTISING_FREQUENCIES_MHZ: [u32; 3] = [2402, 2426, 2480];

fn channel_frequencies_mhz(center_mhz: u32) -> Vec<u32> {
    (0..NUM_CHANNELS)
        .map(|index| {
            if index < NUM_CHANNELS / 2 {
                center_mhz + index
            } else {
                center_mhz - NUM_CHANNELS + index
            }
        })
        .collect()
}

fn coded_scan_primary_frequencies_mhz(center_mhz: u32) -> Vec<u32> {
    let channel_frequencies = channel_frequencies_mhz(center_mhz);
    PRIMARY_ADVERTISING_FREQUENCIES_MHZ
        .into_iter()
        .filter(|frequency| channel_frequencies.contains(frequency))
        .collect()
}

#[derive(Debug, Args)]
pub(crate) struct ReplayIq {
    /// Retained SC16_Q11 IQ with an adjacent INPUT.json acquisition sidecar.
    pub(crate) input: PathBuf,
    /// Expected center frequency in MHz; must match the sidecar.
    #[arg(long, default_value_t = 2437)]
    pub(crate) center_mhz: u32,
    /// Expected sample rate in Hz; only exact 20 MSps is supported.
    #[arg(long, default_value_t = SAMPLE_RATE_HZ)]
    pub(crate) sample_rate: u32,
    /// Full-input sample ceiling; replay never silently processes a prefix.
    #[arg(long, default_value_t = 20_000_000)]
    pub(crate) max_samples: u64,
    /// Blue Dragon relative-power squelch threshold in dB.
    #[arg(long, allow_hyphen_values = true, default_value_t = -45.0)]
    pub(crate) squelch: f32,
    /// Decoder wall-time limit in seconds.
    #[arg(long, default_value_t = 120.0)]
    pub(crate) decoder_timeout: f64,
    /// Maximum derived Blue Dragon burst archive size in MiB.
    #[arg(long, default_value_t = 128)]
    pub(crate) burst_limit_mib: u32,
    /// Soft combined decoder PCAP, burst and log limit in MiB.
    #[arg(long, default_value_t = 512)]
    pub(crate) output_limit_mib: u32,
    /// Continuously search in-band primary advertising channels for LE Coded packets.
    #[arg(long)]
    pub(crate) coded_scan: bool,
    /// Continuously search one interior BLE data-channel bin for LE Coded packets.
    #[arg(long, conflicts_with = "coded_scan")]
    coded_scan_frequency_mhz: Option<u32>,
    /// Keep selected-bin scan coverage without archiving overlapping scan IQ.
    #[arg(long, requires = "coded_scan_frequency_mhz")]
    omit_scan_iq: bool,
    /// Keep selected-bin scan coverage and packet output with a header-only burst archive.
    #[arg(long, requires_all = ["omit_scan_iq", "coded_scan_frequency_mhz"])]
    omit_squelch_iq: bool,
    /// New output directory; defaults under artifacts/bluetooth-replay/.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

impl ReplayIq {
    fn validate(&self) -> Result<()> {
        ensure!(
            (2400..=2480).contains(&self.center_mhz),
            "center MHz must be between 2400 and 2480"
        );
        ensure!(
            self.sample_rate == SAMPLE_RATE_HZ,
            "offline Bluetooth replay currently requires exactly {SAMPLE_RATE_HZ} samples/s"
        );
        ensure!(
            (1..=MAX_SAMPLES).contains(&self.max_samples),
            "max samples must be 1..={MAX_SAMPLES}"
        );
        ensure!(
            self.squelch.is_finite() && (-120.0..=0.0).contains(&self.squelch),
            "squelch must be finite and between -120 and 0 dB"
        );
        ensure!(
            self.decoder_timeout.is_finite() && (0.1..=3600.0).contains(&self.decoder_timeout),
            "decoder timeout must be finite and between 0.1 and 3600 seconds"
        );
        ensure!(
            (1..=8192).contains(&self.burst_limit_mib),
            "burst limit must be 1..=8192 MiB"
        );
        ensure!(
            (1..=8192).contains(&self.output_limit_mib),
            "output limit must be 1..=8192 MiB"
        );
        ensure!(
            self.burst_limit_mib <= self.output_limit_mib,
            "burst limit cannot exceed the combined output limit"
        );
        if self.coded_scan {
            ensure!(
                !coded_scan_primary_frequencies_mhz(self.center_mhz).is_empty(),
                "coded scan requires the channelizer grid to contain 2402, 2426 or 2480 MHz"
            );
        }
        if let Some(frequency) = self.coded_scan_frequency_mhz {
            ensure!(
                !self.coded_scan,
                "select primary or explicit-frequency coded scan"
            );
            ensure!(
                (2404..=2478).contains(&frequency)
                    && frequency.is_multiple_of(2)
                    && frequency != 2426
                    && frequency.abs_diff(self.center_mhz) < NUM_CHANNELS / 2,
                "coded scan frequency must be an interior BLE data-channel bin"
            );
        }
        ensure!(
            !self.omit_scan_iq || self.coded_scan_frequency_mhz.is_some(),
            "omit-scan-iq requires an explicit coded scan frequency"
        );
        ensure!(
            !self.omit_squelch_iq || (self.omit_scan_iq && self.coded_scan_frequency_mhz.is_some()),
            "omit-squelch-iq requires omit-scan-iq and an explicit coded scan frequency"
        );
        Ok(())
    }

    fn record_coded_scan(&self, report: &mut Value) {
        let primary_frequencies = if self.coded_scan {
            coded_scan_primary_frequencies_mhz(self.center_mhz)
        } else {
            Vec::new()
        };
        report["coded_scan_requested"] =
            json!(self.coded_scan || self.coded_scan_frequency_mhz.is_some());
        report["coded_scan_primary_frequencies_mhz"] = json!(primary_frequencies);
        if let Some(frequency) = self.coded_scan_frequency_mhz {
            report["coded_scan_frequency_mhz"] = json!(frequency);
        }
        if self.omit_scan_iq {
            report["coded_scan_iq_omitted"] = json!(true);
        }
        if self.omit_squelch_iq {
            report["coded_scan_squelch_iq_omitted"] = json!(true);
        }
    }

    fn terminal_schema(&self) -> u8 {
        if self.omit_squelch_iq {
            4
        } else if self.omit_scan_iq {
            3
        } else {
            2
        }
    }
}

fn verify_scan_coverage(
    coverage: &CodedScanCoverage,
    frequency_mhz: u32,
    input_complex_samples: u64,
) -> Result<()> {
    let channel_samples = input_complex_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES;
    let (window_end, overlap) = match coverage.window_geometry_version {
        None => (
            channel_samples / CODED_SCAN_STEP_CHANNEL_SAMPLES * CODED_SCAN_STEP_CHANNEL_SAMPLES,
            CODED_SCAN_OVERLAP_CHANNEL_SAMPLES,
        ),
        Some(2) => (channel_samples, COMPLETE_CODED_SCAN_OVERLAP_CHANNEL_SAMPLES),
        _ => bail!("unsupported coded scan window geometry"),
    };
    let completed_windows = window_end.div_ceil(CODED_SCAN_STEP_CHANNEL_SAMPLES);
    let last_start_channel_sample = (completed_windows.saturating_sub(1)
        * CODED_SCAN_STEP_CHANNEL_SAMPLES)
        .saturating_sub(overlap);
    let expected = CodedScanCoverage {
        window_geometry_version: coverage.window_geometry_version,
        frequency_mhz: u64::from(frequency_mhz),
        completed_windows,
        first_start_channel_sample: 0,
        first_window_channel_samples: window_end.min(CODED_SCAN_STEP_CHANNEL_SAMPLES),
        last_start_channel_sample,
        last_window_channel_samples: window_end - last_start_channel_sample,
    };
    ensure!(
        *coverage == expected,
        "decoder reported coded scan coverage {coverage:?}, expected {expected:?}"
    );
    Ok(())
}

fn verify_complete_archive_window(
    record: &bursts::RecordMetadata,
    coverage: &CodedScanCoverage,
    index: u64,
    input_complex_samples: u64,
) -> Result<()> {
    ensure!(
        index < coverage.completed_windows,
        "extra coded scan archive window"
    );
    let previous_end = index * CODED_SCAN_STEP_CHANNEL_SAMPLES;
    let start = previous_end.saturating_sub(COMPLETE_CODED_SCAN_OVERLAP_CHANNEL_SAMPLES);
    let end = (previous_end + CODED_SCAN_STEP_CHANNEL_SAMPLES)
        .min(input_complex_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES);
    let rate = u64::from(bursts::CHANNEL_SAMPLE_RATE_HZ);
    ensure!(
        u64::from(record.frequency_mhz) == coverage.frequency_mhz
            && record.timestamp_seconds == start / rate
            && u64::from(record.timestamp_nanoseconds) == (start % rate) * (1_000_000_000 / rate)
            && u64::from(record.sample_count) == end - start,
        "archived coded scan window {index} differs from the verified schedule"
    );
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct Acquisition {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: u16,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    num_samples: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} must be a regular file",
            path.display()
        );
        Ok(Self::from_metadata(&metadata))
    }
}

/// Path-free content identity for the exact nested decoder used by replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct DecoderProgram {
    schema_version: u32,
    name: &'static str,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PinnedDecoder {
    identity: FileIdentity,
    program: DecoderProgram,
}

fn pin_decoder(path: &Path) -> Result<PinnedDecoder> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect decoder executable {}", path.display()))?;
    ensure!(
        before.file_type().is_file(),
        "decoder executable must be a direct regular file"
    );
    ensure!(
        before.mode() & 0o111 != 0,
        "decoder executable has no execute permission"
    );
    ensure!(
        (1..=MAX_DECODER_BYTES).contains(&before.len()),
        "decoder executable size is outside 1..={MAX_DECODER_BYTES} bytes"
    );
    let mut file =
        File::open(path).with_context(|| format!("open decoder executable {}", path.display()))?;
    let opened = file.metadata()?;
    let before_identity = FileIdentity::from_metadata(&before);
    ensure!(
        FileIdentity::from_metadata(&opened) == before_identity,
        "decoder executable identity changed while opening"
    );
    let mut digest = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_BYTES];
    let mut remaining = opened.len();
    while remaining > 0 {
        let count = usize::try_from(remaining.min(buffer.len() as u64))?;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut trailing = [0u8; 1];
    ensure!(
        file.read(&mut trailing)? == 0,
        "decoder executable grew while hashing"
    );
    let after_file = FileIdentity::from_metadata(&file.metadata()?);
    let after_path = fs::symlink_metadata(path)?;
    ensure!(
        after_file == before_identity
            && after_path.file_type().is_file()
            && FileIdentity::from_metadata(&after_path) == before_identity,
        "decoder executable changed while hashing"
    );
    Ok(PinnedDecoder {
        identity: before_identity,
        program: DecoderProgram {
            schema_version: 1,
            name: "blue-dragon",
            bytes: opened.len(),
            sha256: format!("{:x}", digest.finalize()),
        },
    })
}

fn require_decoder_unchanged(path: &Path, expected: &PinnedDecoder) -> Result<()> {
    ensure!(
        pin_decoder(path)? == *expected,
        "decoder executable content or identity changed during replay"
    );
    Ok(())
}

#[derive(Debug)]
struct ValidatedInput {
    input: PathBuf,
    sidecar: PathBuf,
    acquisition: Acquisition,
    input_identity: FileIdentity,
    sidecar_identity: FileIdentity,
}

fn adjacent_json(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    PathBuf::from(name)
}

fn finite_integer(value: f64, label: &str) -> Result<u64> {
    ensure!(
        value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64,
        "acquisition {label} must be a finite nonnegative integer"
    );
    Ok(value as u64)
}

fn validate_q11(path: &Path, expected_samples: u64) -> Result<FileIdentity> {
    let before = FileIdentity::read(path)?;
    let expected_bytes = expected_samples
        .checked_mul(4)
        .context("IQ byte length overflow")?;
    ensure!(
        before.length == expected_bytes,
        "IQ byte length {} does not match sidecar sample count {}",
        before.length,
        expected_samples
    );
    let file = File::open(path).with_context(|| format!("open IQ input {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut bytes = [0u8; 64 * 1024];
    let mut component_index = 0u64;
    let mut remaining = expected_bytes;
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(bytes.len() as u64))?;
        reader
            .read_exact(&mut bytes[..chunk])
            .context("validate SC16_Q11 input")?;
        let (components, remainder) = bytes[..chunk].as_chunks::<2>();
        ensure!(
            remainder.is_empty(),
            "IQ input ends within an i16 component"
        );
        let in_range = |value| (-2048..=2047).contains(&value);
        // Keep the normal scan vectorizable and locate the first bad component on failure.
        let valid = components.iter().fold(true, |valid, raw| {
            valid & in_range(i16::from_le_bytes(*raw))
        });
        if !valid {
            for (offset, raw) in components.iter().enumerate() {
                let value = i16::from_le_bytes(*raw);
                ensure!(
                    in_range(value),
                    "IQ component {} is outside the SC16_Q11 range: {value}",
                    component_index + offset as u64
                );
            }
        }
        component_index += components.len() as u64;
        remaining -= chunk as u64;
    }
    let mut trailing = [0u8; 1];
    ensure!(
        reader.read(&mut trailing)? == 0,
        "IQ input grew while validating"
    );
    ensure!(
        component_index == expected_samples * 2,
        "IQ component count changed while validating"
    );
    ensure!(
        FileIdentity::read(path)? == before,
        "IQ input changed while validating"
    );
    Ok(before)
}

fn validate_input(args: &ReplayIq) -> Result<ValidatedInput> {
    args.validate()?;
    let input = args
        .input
        .canonicalize()
        .with_context(|| format!("resolve IQ input {}", args.input.display()))?;
    let sidecar = adjacent_json(&input);
    let sidecar_identity = FileIdentity::read(&sidecar)?;
    ensure!(
        sidecar_identity.length <= MAX_SIDECAR_BYTES,
        "acquisition sidecar exceeds {MAX_SIDECAR_BYTES} bytes"
    );
    let mut bytes = Vec::with_capacity(sidecar_identity.length as usize);
    File::open(&sidecar)
        .with_context(|| format!("open acquisition sidecar {}", sidecar.display()))?
        .take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_SIDECAR_BYTES,
        "acquisition sidecar exceeds {MAX_SIDECAR_BYTES} bytes"
    );
    let acquisition: Acquisition = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse acquisition sidecar {}", sidecar.display()))?;
    ensure!(
        acquisition.format == "SC16_Q11"
            && acquisition.dtype == "<i2"
            && acquisition.interleave == "I,Q"
            && acquisition.complex_scale == 2048,
        "acquisition encoding must be SC16_Q11 little-endian interleaved I,Q with scale 2048"
    );
    ensure!(
        finite_integer(acquisition.sample_rate_hz, "sample rate")? == u64::from(args.sample_rate),
        "acquisition sample rate differs from the requested replay rate"
    );
    ensure!(
        finite_integer(acquisition.frequency_hz, "center frequency")?
            == u64::from(args.center_mhz) * 1_000_000,
        "acquisition center frequency differs from the requested replay center"
    );
    ensure!(
        finite_integer(acquisition.bandwidth_hz, "bandwidth")? == u64::from(args.sample_rate),
        "acquisition bandwidth must be exactly 20 MHz"
    );
    ensure!(
        (1..=args.max_samples).contains(&acquisition.num_samples),
        "full IQ input has {} samples, above the {}-sample replay ceiling; prefix replay is not supported",
        acquisition.num_samples,
        args.max_samples
    );
    ensure!(
        acquisition
            .num_samples
            .is_multiple_of(CHANNELIZER_STEP_COMPLEX_SAMPLES),
        "full IQ input sample count must be a multiple of the {CHANNELIZER_STEP_COMPLEX_SAMPLES}-sample Blue Dragon channelizer step"
    );
    let input_identity = validate_q11(&input, acquisition.num_samples)?;
    ensure!(
        FileIdentity::read(&sidecar)? == sidecar_identity,
        "acquisition sidecar changed while validating"
    );
    Ok(ValidatedInput {
        input,
        sidecar,
        acquisition,
        input_identity,
        sidecar_identity,
    })
}

pub(crate) fn check_patch(root: &Path) -> Result<()> {
    // Later Coded scan patches change the application and protocol hunks.
    // Check the file reader hunk from the decoder root, even for source archives.
    let status = Command::new("git")
        .env("GIT_CEILING_DIRECTORIES", root.join("repos"))
        .arg("-C")
        .arg(root.join(Engine::BlueDragon.source()))
        .args([
            "apply",
            "--reverse",
            "--check",
            "--include=crates/sdr/src/file.rs",
        ])
        .arg(root.join(OFFLINE_PATCH))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("check Blue Dragon SC16_Q11 file reader")?;
    ensure!(
        status.success(),
        "required Blue Dragon SC16_Q11 file-reader changes are missing; follow the project build instructions"
    );
    Ok(())
}

fn decoder_bytes(paths: &[&Path]) -> u64 {
    paths
        .iter()
        .filter_map(|path| fs::metadata(path).ok().map(|metadata| metadata.len()))
        .sum()
}

fn replay_outcome(candidate_records: u64, validated_ble_packets: u64) -> &'static str {
    if candidate_records == 0 {
        "completed_zero_candidates"
    } else if validated_ble_packets == 0 {
        "completed_candidates_without_validated_ble"
    } else {
        "completed_with_validated_ble"
    }
}

fn harden_decoder_artifact(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect decoder {label} {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "decoder {label} must be a regular file"
    );
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("protect decoder {label} {}", path.display()))?;
    ensure!(
        fs::symlink_metadata(path)?.mode() & 0o777 == 0o600,
        "decoder {label} permissions are not mode 0600"
    );
    Ok(())
}

fn harden_existing_decoder_artifacts(candidates: &Path, bursts: &Path) -> Result<()> {
    for (path, label) in [(candidates, "candidate PCAP"), (bursts, "burst archive")] {
        match fs::symlink_metadata(path) {
            Ok(_) => harden_decoder_artifact(path, label)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect partial decoder {label} {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn terminal_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("terminal manifest has no parent")?;
    let file_name = path
        .file_name()
        .context("terminal manifest has no file name")?
        .to_string_lossy();
    let partial = parent.join(format!(".{file_name}.partial"));
    let mut published = false;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)
            .with_context(|| {
                format!(
                    "create terminal manifest staging file {}",
                    partial.display()
                )
            })?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::hard_link(&partial, path).with_context(|| {
            format!(
                "publish terminal manifest {} without replacing an existing file",
                path.display()
            )
        })?;
        published = true;
        File::open(parent)?.sync_all()?;
        fs::remove_file(&partial)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        if published {
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_file(&partial);
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
    }
    result
}

fn decoder_command(
    root: &Path,
    executable: &Path,
    input: &Path,
    candidates: &Path,
    burst_path: &Path,
    args: &ReplayIq,
) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .arg("--file")
        .arg(input)
        .args([
            "--format",
            "sc16q11",
            "--sample-rate",
            &args.sample_rate.to_string(),
            "--center-freq",
            &args.center_mhz.to_string(),
            "--channels",
            &NUM_CHANNELS.to_string(),
            "--check-crc",
            "--stats",
            "--no-gpu",
        ])
        .arg(format!("--squelch={}", args.squelch))
        .arg("--write")
        .arg(candidates)
        .arg("--write-bursts")
        .arg(burst_path)
        .arg("--burst-limit-mb")
        .arg(args.burst_limit_mib.to_string());
    if args.coded_scan {
        command.arg("--coded-scan");
    }
    if let Some(frequency) = args.coded_scan_frequency_mhz {
        command
            .arg("--coded-scan-frequency-mhz")
            .arg(frequency.to_string());
    }
    if args.omit_scan_iq {
        command.arg("--omit-scan-iq");
    }
    if args.omit_squelch_iq {
        command.arg("--omit-squelch-iq");
    }
    command
}

pub(crate) fn run(
    root: &Path,
    executable: &Path,
    args: &ReplayIq,
    interrupted: &AtomicBool,
) -> Result<()> {
    let validated = validate_input(args)?;
    let decoder = pin_decoder(executable)?;
    let output = root.join(output_path(
        args.output.as_deref(),
        "artifacts/bluetooth-replay",
    ));
    new_directory(&output)?;
    let output = output.canonicalize()?;
    let candidates = output.join("candidates.pcap");
    let burst_path = output.join("bursts.bdb");
    let log_path = output.join("decoder.log");
    let mut command = decoder_command(
        root,
        executable,
        &validated.input,
        &candidates,
        &burst_path,
        args,
    );
    let decoder_arguments: Vec<_> = command
        .get_args()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    let mut request = json!({
        "schema_version":1,
        "operation":"offline_sc16q11_replay",
        "engine":Engine::BlueDragon,
        "input":validated.input,
        "sidecar":validated.sidecar,
        "input_identity":validated.input_identity,
        "acquisition":validated.acquisition,
        "decoder_arguments":decoder_arguments,
        "decoder_executable":executable,
        "decoder_executable_identity":decoder.identity,
        "decoder_program":&decoder.program,
        "max_samples":args.max_samples,
        "channelizer_step_complex_samples":CHANNELIZER_STEP_COMPLEX_SAMPLES,
        "decoder_file_block_complex_samples":DECODER_BLOCK_COMPLEX_SAMPLES,
        "decoder_timeout_seconds":args.decoder_timeout,
        "burst_limit_mib":args.burst_limit_mib,
        "decoder_output_soft_limit_mib":args.output_limit_mib,
        "full_input_required":true,
        "radio_opened":false,
        "network_requested":false,
        "transmit_requested":false,
    });
    args.record_coded_scan(&mut request);
    json_file(&output.join("request.json"), &request)?;
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&log_path)?;
    command
        .stdout(log.try_clone()?)
        .stderr(log)
        .stdin(Stdio::null());
    require_decoder_unchanged(executable, &decoder)
        .context("revalidate decoder immediately before replay launch")?;
    let start = Instant::now();
    let budget = Duration::from_secs_f64(args.decoder_timeout);
    let output_limit = u64::from(args.output_limit_mib) * 1024 * 1024;
    let mut child = DecoderChild {
        child: command.spawn().context("start offline Bluetooth decoder")?,
        reaped: false,
    };
    let status = loop {
        match child.poll() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                drop(child);
                harden_existing_decoder_artifacts(&candidates, &burst_path)?;
                return Err(error).context("poll offline decoder");
            }
        }
        if interrupted.load(Ordering::Relaxed) {
            if let Err(error) = child.stop() {
                drop(child);
                harden_existing_decoder_artifacts(&candidates, &burst_path)?;
                return Err(error).context("stop interrupted offline decoder");
            }
            harden_existing_decoder_artifacts(&candidates, &burst_path)?;
            bail!(
                "offline replay interrupted; partial evidence is retained in {}",
                output.display()
            );
        }
        if decoder_bytes(&[&candidates, &burst_path, &log_path]) >= output_limit {
            if let Err(error) = child.stop() {
                drop(child);
                harden_existing_decoder_artifacts(&candidates, &burst_path)?;
                return Err(error).context("stop output-limited offline decoder");
            }
            harden_existing_decoder_artifacts(&candidates, &burst_path)?;
            bail!(
                "offline replay output limit reached; partial evidence is retained in {}",
                output.display()
            );
        }
        let remaining = budget.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            if let Err(error) = child.stop() {
                drop(child);
                harden_existing_decoder_artifacts(&candidates, &burst_path)?;
                return Err(error).context("stop timed-out offline decoder");
            }
            harden_existing_decoder_artifacts(&candidates, &burst_path)?;
            bail!(
                "offline replay timed out; partial evidence is retained in {}",
                output.display()
            );
        }
        thread::sleep(remaining.min(REPLAY_POLL_INTERVAL));
    };
    harden_existing_decoder_artifacts(&candidates, &burst_path)?;
    ensure!(
        status.success(),
        "offline decoder exited with {status}; partial evidence is retained in {}",
        output.display()
    );
    ensure!(
        decoder_bytes(&[&candidates, &burst_path, &log_path]) <= output_limit,
        "offline replay exceeded its output limit; partial evidence is retained in {}",
        output.display()
    );
    harden_decoder_artifact(&candidates, "candidate PCAP")?;
    harden_decoder_artifact(&burst_path, "burst archive")?;
    let diagnostic = diagnostics(&log_path)?;
    ensure!(
        !diagnostic.prefix_only,
        "offline decoder log exceeds the complete diagnostic scan limit; partial evidence is retained in {}",
        output.display()
    );
    ensure!(
        diagnostic.stream_error_mentions == 0
            && diagnostic.file_reader_error_mentions == 0
            && diagnostic.pcap_write_error_mentions == 0
            && diagnostic.burst_write_error_mentions == 0,
        "offline decoder reported an input or output error; partial evidence is retained in {}",
        output.display()
    );
    ensure!(
        diagnostic.file_sample_reports == 1
            && diagnostic.file_samples_processed == Some(validated.acquisition.num_samples),
        "offline decoder did not report consuming the exact full input extent; partial evidence is retained in {}",
        output.display()
    );
    ensure!(
        FileIdentity::read(&validated.input)? == validated.input_identity
            && FileIdentity::read(&validated.sidecar)? == validated.sidecar_identity
            && FileIdentity::read(executable)? == decoder.identity,
        "IQ input, sidecar or decoder changed during replay; decoder output is retained but not accepted"
    );
    require_decoder_unchanged(executable, &decoder)
        .context("revalidate decoder after replay completion")?;
    let (packet_summary, selected_coded) = packets::inspect_with_selected_coded(
        &candidates,
        Some(&output.join("validated.pcap")),
        Some(&output.join("packets.jsonl")),
        args.coded_scan_frequency_mhz,
    )
    .context("inspect offline candidate PCAP")?;
    json_file(&output.join("summary.json"), &packet_summary)?;
    let input_samples = validated.acquisition.num_samples;
    let scan_coverage = diagnostic
        .coded_scan_coverage
        .filter(|_| args.coded_scan_frequency_mhz.is_some());
    if let Some(frequency) = args.coded_scan_frequency_mhz {
        if let Some(coverage) = &scan_coverage {
            ensure!(
                !args.omit_squelch_iq || coverage.window_geometry_version == Some(2),
                "omit-squelch-iq requires complete coded scan coverage v2"
            );
            verify_scan_coverage(coverage, frequency, input_samples)?;
        } else if args.omit_scan_iq {
            bail!("decoder must report one complete coded scan coverage line");
        }
    }
    // The BDB v1 parser requires the full header and validates its bytes. An
    // exact header-size bound leaves no records or samples in either lane.
    let archive_limit = if args.omit_squelch_iq {
        EMPTY_BURST_ARCHIVE_BYTES
    } else {
        u64::from(args.burst_limit_mib) * 1024 * 1024
    };
    let mut scan_index = 0;
    let burst_summary = bursts::inspect_with_scan_check(&burst_path, archive_limit, |record| {
        if let Some(coverage) = scan_coverage
            .as_ref()
            .filter(|c| c.window_geometry_version == Some(2))
        {
            verify_complete_archive_window(record, coverage, scan_index, input_samples)?;
        }
        scan_index += 1;
        Ok(())
    })
    .context("inspect offline Blue Dragon burst archive")?;
    let burst_archive_truncated = diagnostic.burst_limit_mentions > 0;
    if args.omit_squelch_iq {
        ensure!(
            burst_summary.archive_record_count == 0
                && burst_summary.squelch_record_count == 0
                && burst_summary.scan_window_record_count == 0
                && !burst_archive_truncated,
            "omit-squelch-iq requires an empty burst archive without output truncation"
        );
    }
    if let Some(frequency) = args.coded_scan_frequency_mhz {
        let retained_windows = burst_summary.scan_window_record_count;
        let expected_windows = scan_coverage.as_ref().map_or(
            input_samples / CODED_SCAN_STRIDE_COMPLEX_SAMPLES,
            |coverage| coverage.completed_windows,
        );
        if args.omit_scan_iq {
            ensure!(
                retained_windows == 0,
                "decoder archived samples from a coded scan window"
            );
        } else {
            ensure!(
                burst_summary.scan_windows_on(frequency) == retained_windows,
                "decoder retained coded scan windows outside the requested frequency"
            );
            ensure!(
                retained_windows <= expected_windows
                    && (burst_archive_truncated || retained_windows == expected_windows),
                "decoder retained {retained_windows} coded scan windows, expected {expected_windows}"
            );
        }
    }
    json_file(&output.join("bursts.json"), &burst_summary)?;
    let validated_ble =
        packet_summary.validated_advertising_packets + packet_summary.validated_data_packets;
    let outcome = replay_outcome(packet_summary.total_records, validated_ble);
    let mut replay = json!({
        "schema_version":args.terminal_schema(),
        "outcome":outcome,
        "decoder_exit_code":status.code(),
        "elapsed_wall_seconds":start.elapsed().as_secs_f64(),
        "processed_full_input":true,
        "decoder_processed_complex_samples":diagnostic.file_samples_processed,
        "channelizer_step_complex_samples":CHANNELIZER_STEP_COMPLEX_SAMPLES,
        "decoder_file_block_complex_samples":DECODER_BLOCK_COMPLEX_SAMPLES,
        "channelizer_steps_processed":validated.acquisition.num_samples / CHANNELIZER_STEP_COMPLEX_SAMPLES,
        "decoder_executable":executable,
        "decoder_executable_identity":decoder.identity,
        "decoder_program":&decoder.program,
        "input_complex_samples":validated.acquisition.num_samples,
        "candidate_records":packet_summary.total_records,
        "validated_ble_packets":validated_ble,
        "invalid_crc_packets":packet_summary.invalid_crc_packets,
        "unknown_crc_packets":packet_summary.unknown_crc_packets,
        "malformed_packets":packet_summary.malformed_packets,
        "classic_candidate_records":packet_summary.classic_records,
        "classic_upstream_crc_claims":packet_summary.classic_upstream_crc_claims,
        "burst_archive_records":burst_summary.archive_record_count,
        "burst_archive_truncated_by_limit":burst_archive_truncated,
        "zero_candidates_does_not_establish_absence":true,
        "classic_candidates_are_not_independently_validated":true,
        "radio_opened":false,
        "network_requested":false,
        "transmit_requested":false,
    });
    args.record_coded_scan(&mut replay);
    if args.coded_scan_frequency_mhz.is_some() {
        replay["coded_scan_window_records"] = json!(burst_summary.scan_window_record_count);
    }
    if let Some(coverage) = scan_coverage {
        replay["coded_scan_coverage"] = json!(coverage);
    }
    if let Some(counts) = selected_coded {
        replay["selected_coded_packets"] = json!(counts);
    }
    terminal_json_file(&output.join("replay.json"), &replay)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "Offline replay outcome: {outcome}")?;
    writeln!(stdout, "Replay directory: {}", output.display())?;
    stdout.flush().context("flush replay output")
}

#[cfg(test)]
mod tests;
