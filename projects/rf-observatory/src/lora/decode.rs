//! Translate a saved Q11 interval and supervise a separate LoRa decoder.

mod lorawan;
mod meshtastic;
mod receipt;

use std::fs::{File, Metadata};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use data_encoding::HEXLOWER;
use nix::sys::statvfs::statvfs;
use ring::digest::{self, SHA256};
use serde::Serialize;

use crate::acquisition::AcquisitionChild;
use crate::dsp::{ChirpChannel, Iq16};
use crate::private_output::StagedDirectory;
use crate::{MAX_SAMPLES, SAMPLE_RATE, file_pin, receiver};

use super::{Capture, ChannelArgs, IQ_SAMPLE_BYTES, bank::Profile, check_stop};

/// Default workspace-relative executable for offline LoRa packet decoding.
pub(crate) const DEFAULT_WORKER: &str =
    "tools/bladerf-lora-worker/target/release/bladerf-lora-worker";
/// Default number of retained frames for one worker invocation.
pub(crate) const DEFAULT_MAX_FRAMES: u32 = 256;
/// Maximum number of retained frames for one worker invocation.
pub(crate) const MAX_FRAMES: u32 = 4_096;
/// Default deadline in seconds for one worker invocation.
pub(crate) const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
/// Maximum deadline in seconds for one worker invocation.
pub(crate) const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_WORKER_BYTES: u64 = 128 * 1024 * 1024;
const DECODE_RECEIPT: &str = "decode.json";
const WORKER_RECEIPT: &str = "worker.json";
const CF32_SAMPLE_BYTES: u64 = 8;
const WORKER_SAMPLES_PER_CHIP: u32 = 4;
const Q11_SCALE: f64 = 2_048.0;
const MESHTASTIC_SYNC_WORD: u8 = 0x2b;
const LORAWAN_SYNC_WORD: u8 = 0x34;
// Five acquisition upchirps, two sync symbols, two full delimiter symbols and
// eight explicit header symbols. The fractional delimiter and payload follow.
const MIN_PACKET_SYMBOLS: u64 = 17;

/// A labeled LoRa packet profile with an explicit compact sync word.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct PacketProfile {
    #[serde(flatten)]
    channel: Profile,
    sync_word: u8,
}

impl PacketProfile {
    /// Read a `CENTER_HZ:BANDWIDTH_HZ:SF:SYNC` profile with a decimal sync byte.
    ///
    /// # Errors
    /// Rejects missing, nonnumeric or unsupported settings.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (channel, sync_word) = value
            .rsplit_once(':')
            .context("expected a LoRa packet profile as CENTER_HZ:BANDWIDTH_HZ:SF:SYNC")?;
        let channel = Profile::parse(channel)?;
        check_worker_channel(channel.channel_hz())?;
        ensure!(
            sync_word.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid LoRa sync word as a decimal byte"
        );
        let sync_word = sync_word
            .parse()
            .context("invalid LoRa sync word as a decimal byte")?;
        Ok(Self { channel, sync_word })
    }

    /// Check whether the channel fits the scalar geometry at 20 MSps.
    pub(crate) fn is_covered(self, center_hz: u64) -> bool {
        self.channel.is_covered(center_hz)
    }

    /// Return the half-open RF passband when both endpoints fit in hertz.
    pub(crate) fn rf_bounds_hz(self) -> Option<Range<u64>> {
        self.channel.rf_bounds_hz()
    }

    /// Return a lower bound on the saved 20 MSps extent needed for acquisition and header.
    pub(crate) fn minimum_samples(self) -> u64 {
        MIN_PACKET_SYMBOLS * self.channel.samples_per_symbol()
    }

    /// Reserve capture time for a nominal complete Join-request when selected.
    /// Otherwise reserve the generic acquisition and header extent.
    pub(crate) fn minimum_capture_samples(self, interpretation: PacketInterpretation) -> u64 {
        if interpretation != PacketInterpretation::LorawanJoin {
            return self.minimum_samples();
        }
        let settings = self.channel.settings(self.channel.channel_hz());
        let quarter_symbols =
            lorawan::join_airtime_quarter_symbols(settings.spreading_factor, settings.bandwidth_hz);
        (quarter_symbols * self.channel.samples_per_symbol()).div_ceil(4)
    }

    /// Return the RF center and bandwidth that determine the translated stream.
    pub(crate) fn channel_key(self) -> (u64, u32) {
        let channel_hz = self.channel.channel_hz();
        (channel_hz, self.channel.settings(channel_hz).bandwidth_hz)
    }

    /// Count completed worker samples emitted from the given 20 MSps input extent.
    pub(crate) fn channelized_samples(self, original_samples: u64) -> u64 {
        let sample_rate_hz = self.channel_key().1 * WORKER_SAMPLES_PER_CHIP;
        original_samples / u64::from(SAMPLE_RATE / sample_rate_hz)
    }
}

/// Optional interpretations of retained payloads, selected by profile sync word.
#[derive(Args, Clone, Copy, Debug, Default)]
pub(crate) struct PacketInterpretations {
    /// Interpret clear Meshtastic headers on profiles with decimal sync word 43.
    #[arg(long)]
    pub(crate) meshtastic: bool,
    /// Interpret LoRaWAN Join-requests on profiles with decimal sync word 52.
    #[arg(long)]
    pub(crate) lorawan_join: bool,
}

impl PacketInterpretations {
    /// Require a matching profile for each requested interpretation.
    ///
    /// # Errors
    /// Returns an error when a requested sync word is absent from the plan.
    pub(crate) fn validate_profiles(self, profiles: &[PacketProfile]) -> Result<()> {
        ensure!(
            !self.meshtastic || profiles.iter().any(|p| p.sync_word == MESHTASTIC_SYNC_WORD),
            "Meshtastic annotation requires a LoRa packet profile with decimal sync word 43"
        );
        ensure!(
            !self.lorawan_join || profiles.iter().any(|p| p.sync_word == LORAWAN_SYNC_WORD),
            "LoRaWAN Join-request annotation requires a LoRa packet profile with decimal sync word 52"
        );
        Ok(())
    }

    /// Apply the enabled interpretation matching the packet profile.
    pub(crate) fn select(self, profile: PacketProfile) -> PacketInterpretation {
        self.for_sync(profile.sync_word)
    }

    fn for_sync(self, sync_word: u8) -> PacketInterpretation {
        match sync_word {
            MESHTASTIC_SYNC_WORD if self.meshtastic => PacketInterpretation::Meshtastic,
            LORAWAN_SYNC_WORD if self.lorawan_join => PacketInterpretation::LorawanJoin,
            _ => PacketInterpretation::None,
        }
    }

    fn for_interval(self, sync_word: u8) -> Result<PacketInterpretation> {
        ensure!(
            !self.meshtastic || sync_word == MESHTASTIC_SYNC_WORD,
            "Meshtastic header decoding requires sync word {MESHTASTIC_SYNC_WORD}"
        );
        ensure!(
            !self.lorawan_join || sync_word == LORAWAN_SYNC_WORD,
            "LoRaWAN Join-request decoding requires sync word {LORAWAN_SYNC_WORD}"
        );
        Ok(self.for_sync(sync_word))
    }
}

/// Interpretation chosen for a single worker attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PacketInterpretation {
    None,
    Meshtastic,
    LorawanJoin,
}

/// Requested decode of one admitted 20 MSps dwell from sample zero.
pub(crate) struct DwellRequest {
    /// Observed capture center in hertz.
    pub(crate) center_hz: u64,
    /// Channel, bandwidth, spreading factor and expected sync word.
    pub(crate) profile: PacketProfile,
    /// Total original samples in the admitted file.
    pub(crate) source_samples: u64,
    /// Original samples to process from sample zero.
    pub(crate) analyzed_samples: u64,
    /// Maximum verified frames to retain in the worker receipt.
    pub(crate) max_frames: u32,
    /// Worker timeout in seconds, beginning after channelization.
    pub(crate) timeout_seconds: u64,
}

struct DecodeRequest {
    channel: ChannelArgs,
    sync_word: u8,
    interpretation: PacketInterpretation,
    source_samples: u64,
    start_sample: u64,
    analyzed_samples: u64,
    max_frames: u32,
    timeout_seconds: u64,
}

impl DecodeRequest {
    fn worker_samples(&self, channel: &ChirpChannel) -> Result<u64> {
        ensure!(
            (1..=MAX_SAMPLES).contains(&self.source_samples)
                && self.analyzed_samples <= self.source_samples.saturating_sub(self.start_sample),
            "LoRa interval is outside the saved IQ"
        );
        ensure!(
            (1..=MAX_FRAMES).contains(&self.max_frames),
            "LoRa frame retention must be 1..={MAX_FRAMES}"
        );
        ensure!(
            (1..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds),
            "LoRa worker timeout must be 1..={MAX_TIMEOUT_SECONDS} seconds"
        );
        let worker_samples = self.analyzed_samples / channel.decimation() as u64;
        ensure!(
            worker_samples > 0,
            "LoRa interval contains no complete channelized samples"
        );
        Ok(worker_samples)
    }
}

fn check_worker_channel(channel_hz: u64) -> Result<()> {
    ensure!(
        u32::try_from(channel_hz).is_ok(),
        "LoRa worker channel must fit in 32-bit hertz"
    );
    Ok(())
}

fn downconverter(settings: &ChannelArgs) -> Result<ChirpChannel> {
    ChirpChannel::oversampled(
        settings.sample_rate_hz,
        settings.offset_hz()?,
        settings.bandwidth_hz,
    )
}

#[derive(Debug, Args)]
pub(crate) struct DecodeLoRa {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    #[command(flatten)]
    channel: ChannelArgs,
    /// Expected compact sync word as a decimal byte (43 Meshtastic, 52 LoRaWAN).
    #[arg(long)]
    sync_word: u8,
    #[command(flatten)]
    interpretations: PacketInterpretations,
    /// Original-rate index of the first sample to analyze.
    #[arg(long, default_value_t = 0)]
    start_sample: u64,
    /// Maximum original-rate samples to analyze from the selected start.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Number of verified frames to retain in the receipt.
    #[arg(long, default_value_t = DEFAULT_MAX_FRAMES, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_FRAMES)))]
    max_frames: u32,
    /// Maximum time to wait for the worker after channelization.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS, value_parser = clap::value_parser!(u64).range(1..=MAX_TIMEOUT_SECONDS))]
    timeout_seconds: u64,
    /// Prebuilt LoRa file worker executable.
    #[arg(long, default_value = DEFAULT_WORKER)]
    worker: PathBuf,
    /// New private directory for decode.json and the verified worker receipt.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    source_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    bandwidth_hz: u32,
    spreading_factor: u8,
    sync_word: u8,
    source_samples: u64,
    start_sample: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: &'a str,
    channelized_sample_count: u64,
    channelized_sample_rate_hz: u32,
    channelized_iq_sha256: &'a str,
    input_samples_per_channel_sample: usize,
    channel_delay_input_samples: f64,
    worker_sha256: &'a str,
    worker_receipt_sha256: &'a str,
    worker_receipt: &'static str,
    validated_frames: usize,
    omitted_valid_frames: usize,
    #[serde(flatten)]
    interpretations: &'a receipt::Interpretations,
}

struct Translated {
    samples: u64,
    sample_rate_hz: u32,
    analyzed_sha256: String,
    channelized_sha256: String,
}

/// Original-rate reads and buffered samples for one dwell's translations.
pub(crate) struct DwellSourceWork {
    pub(crate) buffered_samples: u64,
    pub(crate) source_iq_bytes: u64,
    pub(crate) buffered_iq_bytes: u64,
}

impl DwellSourceWork {
    /// Charge one bounded common prefix and a separate tail per translation.
    pub(crate) fn new(selected_samples: u64, distinct_channels: usize) -> Self {
        let buffered_samples = if distinct_channels > 1 {
            selected_samples.min(receiver::MAX_SHARED_IQ_SAMPLES)
        } else {
            0
        };
        let tail_samples = selected_samples - buffered_samples;
        Self {
            buffered_samples,
            source_iq_bytes: (buffered_samples + tail_samples * distinct_channels as u64)
                * IQ_SAMPLE_BYTES,
            buffered_iq_bytes: buffered_samples * size_of::<Iq16>() as u64,
        }
    }
}

struct BufferedPrefix {
    samples: Vec<Iq16>,
    hash: digest::Context,
}

/// A pinned dwell descriptor with an optional validated and hashed prefix.
pub(crate) struct DwellSource {
    input: File,
    prefix: Option<BufferedPrefix>,
}

impl DwellSource {
    /// Read the shared prefix from sample zero of an admitted dwell descriptor.
    ///
    /// # Errors
    /// Returns an error for an excessive buffer, invalid or incomplete IQ,
    /// failed allocation, or interruption.
    pub(crate) fn new(mut input: File, buffered_samples: u64, stop: &AtomicBool) -> Result<Self> {
        ensure!(
            buffered_samples <= receiver::MAX_SHARED_IQ_SAMPLES,
            "LoRa replay prefix exceeds the shared IQ buffer limit"
        );
        if buffered_samples == 0 {
            return Ok(Self {
                input,
                prefix: None,
            });
        }
        check_stop(stop)?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(buffered_samples as usize)
            .context("reserve shared LoRa IQ prefix")?;
        let mut hash = digest::Context::new(&SHA256);
        input.seek(SeekFrom::Start(0))?;
        receiver::read_q11_range(
            &mut input,
            0..buffered_samples,
            stop,
            &mut hash,
            |sample, _| {
                samples.push(sample);
                Ok(())
            },
        )?;
        Ok(Self {
            input,
            prefix: Some(BufferedPrefix { samples, hash }),
        })
    }
}

fn translate(
    input: &mut File,
    request: &DecodeRequest,
    prefix: Option<&BufferedPrefix>,
    output: &mut File,
    downconverter: &mut ChirpChannel,
    stop: &AtomicBool,
) -> Result<Translated> {
    let mut source_hash = prefix.map_or_else(|| digest::Context::new(&SHA256), |p| p.hash.clone());
    let cached = prefix.map_or(&[][..], |p| p.samples.as_slice());
    let buffered_samples = cached.len() as u64;
    ensure!(
        buffered_samples <= request.analyzed_samples
            && (prefix.is_none() || request.start_sample == 0),
        "LoRa replay interval differs from the shared prefix"
    );
    let mut output = BufWriter::with_capacity(64 * 1024, output);
    let mut hash = digest::Context::new(&SHA256);
    let mut samples = 0;
    let mut visit = |sample| -> Result<()> {
        if let Some(iq) = downconverter.push(sample) {
            let mut bytes = [0; 8];
            bytes[..4].copy_from_slice(&((iq.i / Q11_SCALE) as f32).to_le_bytes());
            bytes[4..].copy_from_slice(&((iq.q / Q11_SCALE) as f32).to_le_bytes());
            output
                .write_all(&bytes)
                .context("write channel-centered CF32")?;
            hash.update(&bytes);
            samples += 1;
        }
        Ok(())
    };
    for block in cached.chunks(receiver::IQ_REPLAY_BLOCK_SAMPLES) {
        check_stop(stop)?;
        for &sample in block {
            visit(sample)?;
        }
    }
    if buffered_samples < request.analyzed_samples {
        input.seek(SeekFrom::Start(
            (request.start_sample + buffered_samples) * IQ_SAMPLE_BYTES,
        ))?;
        receiver::read_q11_range(
            input,
            buffered_samples..request.analyzed_samples,
            stop,
            &mut source_hash,
            |sample, _| visit(sample),
        )?;
    }
    output.flush().context("flush channel-centered CF32")?;
    Ok(Translated {
        samples,
        sample_rate_hz: request.channel.bandwidth_hz * WORKER_SAMPLES_PER_CHIP,
        analyzed_sha256: HEXLOWER.encode(source_hash.finish().as_ref()),
        channelized_sha256: HEXLOWER.encode(hash.finish().as_ref()),
    })
}

/// One translated IQ interval that can be reused across matching packet profiles.
pub(crate) struct PreparedDwell {
    channelized: tempfile::NamedTempFile,
    stamp: Metadata,
    request: DecodeRequest,
    translated: Translated,
    decimation: usize,
    delay_input_samples: f64,
}

impl PreparedDwell {
    fn verify_channelized(&self) -> Result<()> {
        let observed = self
            .channelized
            .path()
            .symlink_metadata()
            .context("inspect temporary channel-centered IQ")?;
        ensure!(
            observed.is_file()
                && file_pin::same_file(&self.stamp, &observed)
                && file_pin::same_file(&self.stamp, &self.channelized.as_file().metadata()?),
            "temporary channel-centered IQ changed after translation"
        );
        Ok(())
    }

    /// Remove the translated file before its private directory is published.
    ///
    /// # Errors
    /// Returns an error if the translated input has changed or cannot be removed.
    pub(crate) fn close(self) -> Result<()> {
        self.verify_channelized()?;
        self.channelized
            .close()
            .context("remove temporary channel-centered IQ")
    }
}

/// Translate one admitted 20 MSps dwell prefix into a private worker input.
///
/// Reuses validated source samples when present and checks the same descriptor
/// after translation. The caller owns the private directory and must close
/// the prepared input before publication.
///
/// # Errors
/// Rejects invalid geometry or samples, changed source IQ, exhausted temporary
/// space, and interruption.
pub(crate) fn prepare_dwell_channel(
    source: &mut DwellSource,
    private_dir: &Path,
    request: DwellRequest,
    verify_source: impl FnOnce(&File) -> Result<()>,
    stop: &AtomicBool,
) -> Result<PreparedDwell> {
    let request = DecodeRequest {
        channel: request.profile.channel.settings(request.center_hz),
        sync_word: request.profile.sync_word,
        interpretation: PacketInterpretation::None,
        source_samples: request.source_samples,
        start_sample: 0,
        analyzed_samples: request.analyzed_samples,
        max_frames: request.max_frames,
        timeout_seconds: request.timeout_seconds,
    };
    let channel = downconverter(&request.channel)?;
    prepare_interval(
        &mut source.input,
        source.prefix.as_ref(),
        private_dir,
        request,
        channel,
        verify_source,
        stop,
    )
}

fn prepare_interval(
    input: &mut File,
    prefix: Option<&BufferedPrefix>,
    private_dir: &Path,
    request: DecodeRequest,
    mut channel: ChirpChannel,
    verify_source: impl FnOnce(&File) -> Result<()>,
    stop: &AtomicBool,
) -> Result<PreparedDwell> {
    check_stop(stop)?;
    let worker_samples = request.worker_samples(&channel)?;
    let space = statvfs(private_dir).context("read LoRa staging filesystem capacity")?;
    let available_bytes = u128::from(space.blocks_available()) * u128::from(space.fragment_size());
    let channelized_bytes = worker_samples * CF32_SAMPLE_BYTES;
    ensure!(
        u128::from(channelized_bytes) <= available_bytes,
        "LoRa decode needs {channelized_bytes} bytes for temporary IQ but the output filesystem has {available_bytes} available"
    );
    let mut channelized = tempfile::NamedTempFile::new_in(private_dir)?;
    let translated = translate(
        input,
        &request,
        prefix,
        channelized.as_file_mut(),
        &mut channel,
        stop,
    )?;
    let stamp = channelized.as_file().metadata()?;
    ensure!(
        translated.samples == worker_samples && stamp.len() == channelized_bytes,
        "channelized sample count differs from the selected interval"
    );
    verify_source(input)?;
    Ok(PreparedDwell {
        channelized,
        stamp,
        request,
        translated,
        decimation: channel.decimation(),
        delay_input_samples: channel.group_delay_input_samples(),
    })
}

/// Admitted worker executable, identity stamp and SHA-256.
pub(crate) struct Worker {
    path: PathBuf,
    stamp: Metadata,
    sha256: String,
}

impl Worker {
    /// Admit a bounded executable and hash it once before starting the worker.
    ///
    /// # Errors
    /// Rejects absent, unreadable, nonexecutable or changed files and interruption.
    pub(crate) fn open(path: &Path, stop: &AtomicBool) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve LoRa worker {}", path.display()))?;
        let (mut input, length) = file_pin::open_regular(&path, MAX_WORKER_BYTES)?;
        let stamp = input.metadata()?;
        ensure!(
            stamp.permissions().mode() & 0o111 != 0,
            "LoRa worker must be executable"
        );
        let mut hash = digest::Context::new(&SHA256);
        let mut buffer = vec![0; 64 * 1024];
        let mut remaining = length;
        while remaining != 0 {
            check_stop(stop)?;
            let count = remaining.min(buffer.len() as u64) as usize;
            input.read_exact(&mut buffer[..count])?;
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        ensure!(
            length == stamp.len() && file_pin::same_file(&stamp, &input.metadata()?),
            "LoRa worker changed while hashing"
        );
        let worker = Self {
            path,
            stamp,
            sha256: HEXLOWER.encode(hash.finish().as_ref()),
        };
        worker.verify()?;
        Ok(worker)
    }

    /// Check that the executable still matches its admitted identity and extent.
    ///
    /// # Errors
    /// Rejects a missing or changed executable.
    pub(crate) fn verify(&self) -> Result<()> {
        let observed = self.path.symlink_metadata()?;
        ensure!(
            observed.is_file() && file_pin::same_file(&self.stamp, &observed),
            "LoRa worker changed after admission"
        );
        Ok(())
    }

    /// Return the hex SHA-256 of the admitted worker executable.
    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Decode a matching profile using a prepared translated stream.
    ///
    /// The sync word and spreading factor may vary. The caller owns the private
    /// attempt directory and must discard it on failure.
    ///
    /// # Errors
    /// Rejects a mismatched channel, a changed prepared input, worker failure
    /// or timeout, invalid evidence and interruption.
    pub(crate) fn decode_prepared_dwell(
        &self,
        prepared: &PreparedDwell,
        private_attempt: &Path,
        profile: PacketProfile,
        interpretation: PacketInterpretation,
        stop: &AtomicBool,
    ) -> Result<Decoded> {
        let source = &prepared.request;
        let channel = profile.channel.settings(source.channel.center_hz);
        ensure!(
            (
                channel.channel_hz,
                channel.bandwidth_hz,
                channel.sample_rate_hz
            ) == (
                source.channel.channel_hz,
                source.channel.bandwidth_hz,
                source.channel.sample_rate_hz,
            ),
            "LoRa decode profile differs from the prepared channel"
        );
        let request = DecodeRequest {
            channel,
            sync_word: profile.sync_word,
            interpretation,
            source_samples: source.source_samples,
            start_sample: source.start_sample,
            analyzed_samples: source.analyzed_samples,
            max_frames: source.max_frames,
            timeout_seconds: source.timeout_seconds,
        };
        self.decode_prepared_interval(prepared, private_attempt, &request, stop)
    }

    fn decode_prepared_interval(
        &self,
        prepared: &PreparedDwell,
        private_dir: &Path,
        request: &DecodeRequest,
        stop: &AtomicBool,
    ) -> Result<Decoded> {
        check_stop(stop)?;
        let receipt_path = private_dir.join(WORKER_RECEIPT);
        prepared.verify_channelized()?;
        self.run(prepared.channelized.path(), &receipt_path, request, stop)?;
        prepared.verify_channelized()?;
        let translated = &prepared.translated;
        let (worker, interpretations) = receipt::read(&receipt_path, request, translated)?;
        let settings = &request.channel;
        let report = Report {
            schema_version: 1,
            operation: "decode_lora_saved_iq",
            source_format: "SC16_Q11_LE",
            center_frequency_hz: settings.center_hz,
            channel_frequency_hz: settings.channel_hz,
            sample_rate_hz: settings.sample_rate_hz,
            bandwidth_hz: settings.bandwidth_hz,
            spreading_factor: settings.spreading_factor,
            sync_word: request.sync_word,
            source_samples: request.source_samples,
            start_sample: request.start_sample,
            analyzed_samples: request.analyzed_samples,
            analyzed_iq_sha256: &translated.analyzed_sha256,
            channelized_sample_count: translated.samples,
            channelized_sample_rate_hz: translated.sample_rate_hz,
            channelized_iq_sha256: &translated.channelized_sha256,
            input_samples_per_channel_sample: prepared.decimation,
            channel_delay_input_samples: prepared.delay_input_samples,
            worker_sha256: self.sha256(),
            worker_receipt_sha256: worker.sha256(),
            worker_receipt: WORKER_RECEIPT,
            validated_frames: worker.validated_frames,
            omitted_valid_frames: worker.omitted_valid_frames,
            interpretations: &interpretations,
        };
        check_stop(stop)?;
        let decode = receipt::save(&private_dir.join(DECODE_RECEIPT), &report)?;
        File::open(private_dir)?
            .sync_all()
            .context("sync LoRa decode directory")?;
        Ok(Decoded {
            decode,
            worker,
            meshtastic_frames: interpretations.meshtastic_frames(),
            lorawan_join_requests: interpretations.lorawan_join_requests(),
        })
    }

    fn run(
        &self,
        input: &Path,
        receipt: &Path,
        request: &DecodeRequest,
        stop: &AtomicBool,
    ) -> Result<()> {
        self.verify()?;
        check_stop(stop)?;
        let settings = &request.channel;
        let child = Command::new(&self.path)
            .arg("decode")
            .arg(input)
            .args([
                "--channel-hz",
                &settings.channel_hz.to_string(),
                "--bandwidth-hz",
                &settings.bandwidth_hz.to_string(),
                "--spreading-factor",
                &settings.spreading_factor.to_string(),
                "--sync-word",
                &request.sync_word.to_string(),
                "--max-frames",
                &request.max_frames.to_string(),
                "--output",
            ])
            .arg(receipt)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .context("start LoRa worker")?;
        let mut child = AcquisitionChild::new(child);
        let deadline = Instant::now() + Duration::from_secs(request.timeout_seconds);
        loop {
            if let Some(status) = child.try_wait().context("reap LoRa worker")? {
                ensure!(status.success(), "LoRa worker exited with {status}");
                break;
            }
            if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
                child.stop().context("stop LoRa worker")?;
                bail!("LoRa worker interrupted or timed out");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.verify()
    }
}

/// Counts and admission stamps for the two saved, verified LoRa decode receipts.
pub(crate) struct Decoded {
    decode: receipt::Saved,
    worker: receipt::Verified,
    meshtastic_frames: Option<usize>,
    lorawan_join_requests: Option<usize>,
}

impl Decoded {
    /// Return the number of CRC- and sync-validated frames, including omissions.
    pub(crate) fn validated_frames(&self) -> usize {
        self.worker.validated_frames
    }

    /// Return validated frames beyond the worker's record capacity.
    pub(crate) fn omitted_valid_frames(&self) -> usize {
        self.worker.omitted_valid_frames
    }

    /// Return the number of retained compatible headers if requested.
    pub(crate) fn meshtastic_frames(&self) -> Option<usize> {
        self.meshtastic_frames
    }

    /// Return the number of retained compatible Join-requests if requested.
    pub(crate) fn lorawan_join_requests(&self) -> Option<usize> {
        self.lorawan_join_requests
    }

    /// Return the hex SHA-256 of the saved host decode receipt.
    pub(crate) fn decode_receipt_sha256(&self) -> &str {
        self.decode.sha256()
    }

    /// Return the hex SHA-256 of the verified worker receipt.
    pub(crate) fn worker_receipt_sha256(&self) -> &str {
        self.worker.sha256()
    }

    /// Verify the two saved receipt identities before publishing their directory.
    ///
    /// # Errors
    /// Rejects missing, replaced or modified receipts.
    pub(crate) fn verify(&self, directory: &Path) -> Result<()> {
        self.worker.verify(&directory.join(WORKER_RECEIPT))?;
        self.decode.verify(&directory.join(DECODE_RECEIPT))
    }
}

/// Decode a selected original-rate interval and privately publish verified evidence.
///
/// # Errors
/// Returns an error for invalid geometry, a changed input, a failed or late
/// worker, rejected worker evidence, interruption, or publication failure.
pub(crate) fn run(root: &Path, args: DecodeLoRa, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let interpretation = args.interpretations.for_interval(args.sync_word)?;
    check_worker_channel(args.channel.channel_hz)?;
    let channel = downconverter(&args.channel)?;
    let capture = Capture::open(root.join(args.input))?;
    ensure!(
        args.start_sample < capture.samples,
        "LoRa decode start is outside the saved IQ"
    );
    let request = DecodeRequest {
        channel: args.channel,
        sync_word: args.sync_word,
        interpretation,
        source_samples: capture.samples,
        start_sample: args.start_sample,
        analyzed_samples: (capture.samples - args.start_sample).min(args.max_samples),
        max_frames: args.max_frames,
        timeout_seconds: args.timeout_seconds,
    };
    request.worker_samples(&channel)?;
    let worker = Worker::open(&root.join(args.worker), stop)?;
    let destination = root.join(args.output);
    let stage = StagedDirectory::new(&destination, ".lora-decode-")?;
    let mut input = capture.input.try_clone()?;
    let prepared = prepare_interval(
        &mut input,
        None,
        stage.path(),
        request,
        channel,
        |_| capture.verify(),
        stop,
    )?;
    let decoded =
        worker.decode_prepared_interval(&prepared, stage.path(), &prepared.request, stop)?;
    prepared.close()?;
    let stage_path = stage.path().to_path_buf();
    stage.publish(|| {
        capture.verify()?;
        worker.verify()?;
        decoded.verify(&stage_path)?;
        check_stop(stop)
    })?;
    println!(
        "Validated {} LoRa frames. Output: {}",
        decoded.validated_frames(),
        destination.display()
    );
    if let Some(count) = decoded.meshtastic_frames() {
        println!("Read {count} compatible Meshtastic headers.");
    }
    if let Some(count) = decoded.lorawan_join_requests() {
        println!("Read {count} compatible LoRaWAN Join-requests.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_worker_sample_counts_match_every_supported_downconverter() {
        let center_hz = 906_875_000;
        let input_samples = 20_039;
        for bandwidth_hz in [125_000, 250_000, 500_000] {
            let profile =
                PacketProfile::parse(&format!("{center_hz}:{bandwidth_hz}:11:43")).unwrap();
            assert_eq!(profile.channel_key(), (center_hz, bandwidth_hz));
            let channel = downconverter(&profile.channel.settings(center_hz)).unwrap();
            assert_eq!(
                profile.channelized_samples(input_samples),
                input_samples / channel.decimation() as u64
            );
        }
    }

    #[test]
    fn rejects_a_changed_temporary_channel_before_reuse() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("input.iq");
        let samples = 100;
        std::fs::write(&input, vec![0; (samples * IQ_SAMPLE_BYTES) as usize])?;
        let stop = AtomicBool::new(false);
        let mut source = DwellSource::new(File::open(input)?, 0, &stop)?;
        let center_hz = 906_875_000;
        let prepared = prepare_dwell_channel(
            &mut source,
            directory.path(),
            DwellRequest {
                center_hz,
                profile: PacketProfile::parse("906875000:250000:11:43")?,
                source_samples: samples,
                analyzed_samples: samples,
                max_frames: DEFAULT_MAX_FRAMES,
                timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            },
            |_| Ok(()),
            &stop,
        )?;
        prepared.verify_channelized()?;
        let path = prepared.channelized.path().to_path_buf();
        std::fs::write(&path, [0])?;
        assert!(prepared.verify_channelized().is_err());
        assert!(prepared.close().is_err());
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn buffered_prefix_and_tail_preserve_exact_translation_bytes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("input.iq");
        let analyzed_samples = 20_039;
        let raw: Vec<_> = (0..analyzed_samples)
            .flat_map(|sample| {
                let i = (sample % 4_096) as i16 - 2_048;
                let q = (13 * sample % 4_096) as i16 - 2_048;
                i.to_le_bytes().into_iter().chain(q.to_le_bytes())
            })
            .collect();
        std::fs::write(&input, &raw)?;
        let stop = AtomicBool::new(false);
        let mut streaming = DwellSource::new(File::open(&input)?, 0, &stop)?;
        // Start the streamed tail between original read blocks and decimation groups.
        let mut partial = DwellSource::new(File::open(&input)?, 16_387, &stop)?;
        let mut buffered = DwellSource::new(File::open(&input)?, analyzed_samples, &stop)?;
        for settings in ["903900000:125000:10:52", "904600000:500000:8:52"] {
            let prepare = |source: &mut DwellSource| {
                prepare_dwell_channel(
                    source,
                    directory.path(),
                    DwellRequest {
                        center_hz: 907_650_000,
                        profile: PacketProfile::parse(settings)?,
                        source_samples: analyzed_samples,
                        analyzed_samples,
                        max_frames: DEFAULT_MAX_FRAMES,
                        timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
                    },
                    |_| Ok(()),
                    &stop,
                )
            };
            let reference = prepare(&mut streaming)?;
            for source in [&mut partial, &mut buffered] {
                let prepared = prepare(source)?;
                assert_eq!(
                    prepared.translated.analyzed_sha256,
                    reference.translated.analyzed_sha256
                );
                assert_eq!(
                    prepared.translated.channelized_sha256,
                    reference.translated.channelized_sha256
                );
                assert_eq!(
                    std::fs::read(prepared.channelized.path())?,
                    std::fs::read(reference.channelized.path())?
                );
                prepared.close()?;
            }
            reference.close()?;
        }
        Ok(())
    }

    #[test]
    fn packet_profile_uses_the_same_band_geometry_and_a_packet_extent() {
        let profile = PacketProfile::parse("906875000:250000:11:43").unwrap();
        assert!(profile.is_covered(914_750_000));
        assert!(!profile.is_covered(914_750_001));
        assert_eq!(profile.minimum_samples(), 2_785_280);
        assert_eq!(
            serde_json::to_value(profile).unwrap(),
            serde_json::json!({
                "channel_hz": 906_875_000,
                "bandwidth_hz": 250_000,
                "spreading_factor": 11,
                "sync_word": 43
            })
        );
        for value in [
            "906875000:250000:11",
            "906875000:250000:11:0x2b",
            "906875000:250000:11:256",
            "4294967296:250000:11:43",
        ] {
            assert!(PacketProfile::parse(value).is_err(), "{value}");
        }
    }
}
