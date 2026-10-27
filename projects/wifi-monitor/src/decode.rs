//! Bounded, offline 20 Msps Wi-Fi IQ decoding through libcrafter's radio API.
//!
//! This synchronous function reads regular files and performs CPU work. Its
//! caller must supervise it in a separate process when a wall-clock deadline is
//! required. Sample-time bounds do not bound decoder execution time. It never
//! opens a radio or enables transmission. SC16 Q11 conversion preserves the
//! signed high eight bits of each component; no automatic gain is applied.
//!
//! Output has sample-relative timestamps unless the caller supplies a host
//! time origin. Neither choice establishes hardware timestamp accuracy. Minimal
//! radiotap records carry only the presence of the actual received FCS, which is
//! checked again independently here. No RF power or noise calibration is claimed.
//!
//! A private sibling staging file is published without replacement only after
//! successful decoding and synchronization. Ordinary errors remove staging;
//! terminating the process can leave a private `.wifi-decode-*.partial` file.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use crafter::LinkType;
use crafter::radio::{
    DecodeOutput, DecoderStats, FrameIntegrity, IqEvent, IqPosition, IqSource, PhyDecoder,
    PhyDiagnostic, ReaderIqSource, RecoveredFrame, RxConfig, StreamEnd, WifiDecoder,
};
use serde::Serialize;

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const NANOSECONDS_PER_SAMPLE: u128 = 50;
const MAX_SAMPLES: u64 = 200_000_000;
const CHUNK_SAMPLES: usize = 4_096;
const BUFFER_SAMPLES: usize = 1_048_576;
const FRAME_BYTES: usize = 65_535;
const PENDING_FRAMES: usize = 64;
const SNAPLEN: usize = 65_536;
const RADIOTAP: [u8; 9] = [0, 0, 9, 0, 2, 0, 0, 0, 0x10];
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Headerless, interleaved I then Q samples, with little-endian SC16 components.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IqFormat {
    Sc16Q11,
    Cs8,
}

#[derive(Debug, Serialize)]
pub(crate) struct DecodeSummary {
    pub(crate) decoder: &'static str,
    pub(crate) iq_format: IqFormat,
    pub(crate) center_frequency_hz: u64,
    pub(crate) sample_rate_hz: u32,
    pub(crate) input_samples: u64,
    pub(crate) processed_samples: u64,
    pub(crate) sample_limit: u64,
    pub(crate) sample_limit_reached: bool,
    pub(crate) input_eof_observed: bool,
    pub(crate) timestamp_basis: &'static str,
    pub(crate) frames_written: u64,
    pub(crate) ofdm: PhyStats,
    pub(crate) dsss: PhyStats,
    pub(crate) diagnostic_counts: BTreeMap<&'static str, u64>,
    /// The upstream decoder bounds and may truncate diagnostic vectors. Its
    /// separate PHY counters remain available even when details are omitted.
    pub(crate) diagnostic_counts_are_lower_bounds: bool,
    pub(crate) diagnostic_batches_at_capacity: u64,
    pub(crate) clipped_samples: u64,
    pub(crate) pending_frame_limit: usize,
    pub(crate) buffer_sample_limit: usize,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct PhyStats {
    pub(crate) valid_frames: u64,
    pub(crate) invalid_fcs: u64,
    pub(crate) rejected_frames: u64,
    pub(crate) truncated_frames: u64,
    pub(crate) dropped_frames: u64,
}

impl From<DecoderStats> for PhyStats {
    fn from(stats: DecoderStats) -> Self {
        Self {
            valid_frames: stats.valid_frames,
            invalid_fcs: stats.invalid_fcs,
            rejected_frames: stats.rejected_frames,
            truncated_frames: stats.truncated_frames,
            dropped_frames: stats.dropped_frames,
        }
    }
}

/// Decode a regular IQ file at exactly 20 Msps, with a cumulative sample cap.
///
/// Reaching the cap is reported even when it coincides with the input length:
/// EOF was not observed and trailing input was not examined. Unsupported PHYs,
/// interference, truncation and decoder limits can prevent frame recovery. A
/// zero-frame result is therefore distinct from absence of Wi-Fi activity.
///
/// # Errors
///
/// Returns an error for invalid limits, sample framing or Q11 components;
/// unreadable input; unsupported or independently invalid recovered framing;
/// decoder capacity failures; timestamps outside classic PCAP's range; and
/// output creation, writing or publication errors. Existing output is preserved.
pub(crate) fn decode(
    input: &Path,
    output: &Path,
    frequency_hz: u64,
    format: IqFormat,
    max_samples: u64,
    time_origin_ns: Option<u128>,
) -> Result<DecodeSummary> {
    ensure!(
        (1..=MAX_SAMPLES).contains(&max_samples),
        "decode sample limit must be 1..={MAX_SAMPLES}"
    );
    ensure!(frequency_hz != 0, "center frequency must be nonzero");
    pcap_timestamp(0, time_origin_ns)?;
    let file = File::open(input).with_context(|| format!("open IQ input {}", input.display()))?;
    let metadata = file.metadata().context("inspect IQ input")?;
    ensure!(metadata.is_file(), "IQ input must be a regular file");
    let bytes_per_sample = match format {
        IqFormat::Sc16Q11 => 4,
        IqFormat::Cs8 => 2,
    };
    ensure!(
        metadata.len() % bytes_per_sample == 0,
        "IQ input ends within a complete I/Q sample"
    );
    let input_samples = metadata.len() / bytes_per_sample;
    let reader: Box<dyn Read> = match format {
        IqFormat::Sc16Q11 => Box::new(Sc16Q11Reader::new(BufReader::new(file))),
        IqFormat::Cs8 => Box::new(BufReader::new(file)),
    };
    let config = RxConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        center_frequency_hz: frequency_hz,
        max_chunk_samples: CHUNK_SAMPLES,
        max_buffer_samples: BUFFER_SAMPLES,
        max_frame_bytes: FRAME_BYTES,
        max_pending_frames: PENDING_FRAMES,
        max_capture_samples: max_samples,
        max_duration: Duration::from_nanos(max_samples * NANOSECONDS_PER_SAMPLE as u64),
    };
    let position = IqPosition {
        epoch: 0,
        sequence: 0,
        sample_index: 0,
        time_anchor: None,
        discontinuity: None,
    };
    let mut source = ReaderIqSource::new(reader, config, position)
        .context("configure bounded offline IQ source")?;
    let mut decoder = WifiDecoder::new();
    let mut destination = PcapOutput::new(output)?;
    let mut summary = DecodeSummary {
        decoder: "libcrafter-wifi-serial",
        iq_format: format,
        center_frequency_hz: frequency_hz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        input_samples,
        processed_samples: 0,
        sample_limit: max_samples,
        sample_limit_reached: false,
        input_eof_observed: false,
        timestamp_basis: if time_origin_ns.is_some() {
            "caller-host-origin-plus-sample-offset"
        } else {
            "sample-relative"
        },
        frames_written: 0,
        ofdm: PhyStats::default(),
        dsss: PhyStats::default(),
        diagnostic_counts: BTreeMap::new(),
        diagnostic_counts_are_lower_bounds: true,
        diagnostic_batches_at_capacity: 0,
        clipped_samples: 0,
        pending_frame_limit: PENDING_FRAMES,
        buffer_sample_limit: BUFFER_SAMPLES,
    };
    loop {
        let event = source.next_event().context("read offline IQ samples")?;
        let end = match &event {
            IqEvent::Chunk(chunk) => {
                summary.processed_samples += chunk.len() as u64;
                None
            }
            IqEvent::End(end) => Some(*end),
        };
        let decoded = decoder.consume(event).with_context(|| {
            format!(
                "Wi-Fi decode failed after {} samples (OFDM dropped {}, DSSS dropped {})",
                summary.processed_samples,
                decoder.ofdm_stats().dropped_frames,
                decoder.dsss_stats().dropped_frames
            )
        })?;
        handle_output(decoded, &mut destination, &mut summary, time_origin_ns)?;
        if let Some(end) = end {
            summary.sample_limit_reached = end == StreamEnd::LimitReached;
            summary.input_eof_observed = end == StreamEnd::Eof;
            break;
        }
    }
    summary.ofdm = decoder.ofdm_stats().into();
    summary.dsss = decoder.dsss_stats().into();
    destination.publish()?;
    Ok(summary)
}

fn handle_output(
    decoded: DecodeOutput,
    destination: &mut PcapOutput,
    summary: &mut DecodeSummary,
    time_origin_ns: Option<u128>,
) -> Result<()> {
    ensure!(
        decoded.frames.len() <= PENDING_FRAMES,
        "decoder exceeded its pending frame limit"
    );
    if decoded.diagnostics.len() >= PENDING_FRAMES {
        summary.diagnostic_batches_at_capacity += 1;
    }
    for diagnostic in decoded.diagnostics {
        count_diagnostic(diagnostic, summary);
    }
    for frame in decoded.frames {
        destination.write_frame(&frame, time_origin_ns)?;
        summary.frames_written += 1;
        for diagnostic in frame.diagnostics {
            count_diagnostic(diagnostic, summary);
        }
    }
    Ok(())
}

fn count_diagnostic(diagnostic: PhyDiagnostic, summary: &mut DecodeSummary) {
    let name = match diagnostic {
        PhyDiagnostic::Ampdu { .. } => "ampdu-member",
        PhyDiagnostic::AmpduErrors { .. } => "ampdu-errors",
        PhyDiagnostic::HtGreenfield { .. } => "ht-greenfield",
        PhyDiagnostic::HtSignal { .. } => "ht-signal",
        PhyDiagnostic::Reset(_) => "reset",
        PhyDiagnostic::TruncatedFrame => "truncated-frame",
        PhyDiagnostic::InvalidHeader => "invalid-header",
        PhyDiagnostic::InvalidFcs => "invalid-fcs",
        PhyDiagnostic::InvalidData => "invalid-data",
        PhyDiagnostic::Ldpc { .. } => "ldpc",
        PhyDiagnostic::LdpcPartial { .. } => "ldpc-partial",
        PhyDiagnostic::LdpcNonconvergence { .. } => "ldpc-nonconvergence",
        PhyDiagnostic::OfdmTracking { .. } => "ofdm-tracking",
        PhyDiagnostic::OfdmRecovery { .. } => "ofdm-recovery",
        PhyDiagnostic::Ofdm { .. } => "ofdm",
        PhyDiagnostic::Dsss { .. } => "dsss",
        PhyDiagnostic::UnsupportedPhy => "unsupported-phy",
        PhyDiagnostic::Clipping { samples } => {
            summary.clipped_samples = summary.clipped_samples.saturating_add(samples);
            "clipping"
        }
    };
    let count = summary.diagnostic_counts.entry(name).or_default();
    *count = count.saturating_add(1);
}

/// Converts a bounded stream of signed Q11 components to cs8 without buffering
/// the file. Invalid Q11 values are rejected instead of wrapping or clipping.
#[derive(Debug)]
struct Sc16Q11Reader<R> {
    inner: R,
}

impl<R: Read> Sc16Q11Reader<R> {
    fn new(inner: R) -> Self {
        Self { inner }
    }
}

impl<R: Read> Read for Sc16Q11Reader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let mut count = 0;
        for out in output {
            let mut bytes = [0; 2];
            match self.inner.read_exact(&mut bytes[..1]) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
            self.inner.read_exact(&mut bytes[1..]).map_err(|error| {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    io::Error::new(io::ErrorKind::InvalidData, "truncated SC16 Q11 component")
                } else {
                    error
                }
            })?;
            let component = i16::from_le_bytes(bytes);
            if !(-2048..=2047).contains(&component) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SC16 Q11 component is outside -2048..=2047",
                ));
            }
            *out = (component >> 4) as i8 as u8;
            count += 1;
        }
        Ok(count)
    }
}

fn pcap_timestamp(sample_index: u64, origin: Option<u128>) -> Result<(u32, u32)> {
    let ns = origin
        .unwrap_or(0)
        .checked_add(u128::from(sample_index) * NANOSECONDS_PER_SAMPLE)
        .context("packet timestamp overflow")?;
    let seconds = u32::try_from(ns / 1_000_000_000)
        .context("timestamp exceeds classic PCAP's unsigned 32-bit seconds range")?;
    Ok((seconds, (ns % 1_000_000_000) as u32))
}

/// Independent reflected IEEE CRC-32, including initial and final inversion.
fn valid_fcs(bytes: &[u8]) -> bool {
    let Some(split) = bytes.len().checked_sub(4) else {
        return false;
    };
    let mut crc = u32::MAX;
    for byte in &bytes[..split] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    let expected = [
        bytes[split],
        bytes[split + 1],
        bytes[split + 2],
        bytes[split + 3],
    ];
    !crc == u32::from_le_bytes(expected)
}

#[derive(Debug)]
struct PcapOutput {
    destination: PathBuf,
    temporary: PathBuf,
    writer: BufWriter<File>,
}

impl PcapOutput {
    fn new(destination: &Path) -> Result<Self> {
        ensure!(!destination.exists(), "decode output already exists");
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        for _ in 0..128 {
            let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".wifi-decode-{}-{sequence}.partial",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temporary) {
                Ok(file) => {
                    let mut output = Self {
                        destination: destination.to_path_buf(),
                        temporary,
                        writer: BufWriter::new(file),
                    };
                    output.writer.write_all(&0xa1b2_3c4d_u32.to_le_bytes())?;
                    output.writer.write_all(&2_u16.to_le_bytes())?;
                    output.writer.write_all(&4_u16.to_le_bytes())?;
                    output.writer.write_all(&[0; 8])?;
                    output.writer.write_all(&(SNAPLEN as u32).to_le_bytes())?;
                    output.writer.write_all(&127_u32.to_le_bytes())?;
                    return Ok(output);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create private decode output"),
            }
        }
        bail!("cannot allocate a unique decode staging file")
    }

    fn write_frame(&mut self, frame: &RecoveredFrame, origin: Option<u128>) -> Result<()> {
        ensure!(
            frame.link_type == LinkType::Ieee80211
                && frame.integrity == FrameIntegrity::ValidFcs
                && frame.framing.trailer_bytes == 4,
            "decoder returned unsupported frame framing or integrity"
        );
        ensure!(
            valid_fcs(&frame.bytes),
            "independent frame FCS check failed"
        );
        ensure!(frame.start.epoch == 0, "unexpected IQ epoch change");
        let length = RADIOTAP.len() + frame.bytes.len();
        ensure!(
            length <= SNAPLEN,
            "recovered frame exceeds PCAP record bound"
        );
        let (seconds, nanoseconds) = pcap_timestamp(frame.start.sample_index, origin)?;
        self.writer.write_all(&seconds.to_le_bytes())?;
        self.writer.write_all(&nanoseconds.to_le_bytes())?;
        self.writer.write_all(&(length as u32).to_le_bytes())?;
        self.writer.write_all(&(length as u32).to_le_bytes())?;
        self.writer.write_all(&RADIOTAP)?;
        self.writer.write_all(&frame.bytes)?;
        Ok(())
    }

    fn publish(&mut self) -> Result<()> {
        self.writer.flush().context("flush decoded PCAP")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("sync decoded PCAP")?;
        fs::hard_link(&self.temporary, &self.destination)
            .context("publish decoded PCAP without replacing existing files")?;
        // Publication is complete. A cleanup failure must not report a failed
        // decode after a complete destination became visible; Drop retries it.
        let _ = fs::remove_file(&self.temporary);
        Ok(())
    }
}

impl Drop for PcapOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use sha2::{Digest, Sha256};

    // Independent synthetic vector from libcrafter f9ef000bc402eebc676f6b4708395aa8ac169450.
    // Its Python oracle cross-checks IEEE 802.11-2007 Annex G intermediates.
    // The expected bytes below come from ofdm-index.tsv, not this receiver.
    const IQ: &[u8] =
        include_bytes!("../../../repos/libcrafter/crafter/tests/fixtures/iq/ofdm-6-clean.cs8");
    const EXPECTED: &str = "080000000200000000010200000000020200000000030000aaaa030000000800450000170001000040fd8db2c0000201c6336402000102fb56c32d";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wifi-decode-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn expected_bytes() -> Vec<u8> {
        EXPECTED
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn pinned_independent_iq_recovers_original_psdu_and_real_fcs() {
        assert_eq!(
            format!("{:x}", Sha256::digest(IQ)),
            "eb8fb8ddb7c3366584eda24812316c2dbd3aa0688ea26450e39ea9c0ea321d0b"
        );
        let directory = TestDirectory::new();
        let input = directory.file("synthetic.cs8");
        fs::write(&input, IQ).unwrap();
        let output = directory.file("decoded.pcap");
        let summary = decode(&input, &output, 2_412_000_000, IqFormat::Cs8, 10_000, None).unwrap();
        assert_eq!(summary.frames_written, 1);
        assert_eq!(summary.ofdm.valid_frames, 1);
        assert_eq!(summary.ofdm.dropped_frames, 0);
        assert_eq!(summary.processed_samples, 2149);
        assert!(summary.input_eof_observed);
        assert!(!summary.sample_limit_reached);
        let pcap = fs::read(&output).unwrap();
        assert_eq!(&pcap[40..49], &RADIOTAP);
        assert_eq!(&pcap[49..], expected_bytes());
        let validated = crate::packets::inspect(&output, None, None).unwrap();
        assert_eq!(validated.validated_frames, 1);
        assert!(decode(&input, &output, 2_412_000_000, IqFormat::Cs8, 10_000, None).is_err());
        assert_eq!(fs::read(output).unwrap(), pcap);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(directory.file("decoded.pcap"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn sc16_conversion_preserves_signed_bits_short_reads_and_rejects_bad_input() {
        struct Short(Cursor<Vec<u8>>);
        impl Read for Short {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let limit = output.len().min(1);
                self.0.read(&mut output[..limit])
            }
        }
        let values: [i16; 8] = [-2048, -2047, -17, -1, 0, 15, 16, 2047];
        let raw = values.into_iter().flat_map(i16::to_le_bytes).collect();
        let mut reader = Sc16Q11Reader::new(Short(Cursor::new(raw)));
        let mut converted = Vec::new();
        reader.read_to_end(&mut converted).unwrap();
        assert_eq!(converted, [128, 128, 254, 255, 0, 0, 1, 127]);
        assert!(
            Sc16Q11Reader::new(Cursor::new([1]))
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        assert!(
            Sc16Q11Reader::new(Cursor::new(2048_i16.to_le_bytes()))
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn sc16_iq_matches_cs8_decode_and_host_origin_is_explicit() {
        let directory = TestDirectory::new();
        let input = directory.file("synthetic.sc16q11");
        let sc16: Vec<u8> = IQ
            .iter()
            .flat_map(|byte| (i16::from(*byte as i8) * 16).to_le_bytes())
            .collect();
        fs::write(&input, sc16).unwrap();
        let output = directory.file("host-origin.pcap");
        let origin = 1_234_000_000_007;
        let summary = decode(
            &input,
            &output,
            2_412_000_000,
            IqFormat::Sc16Q11,
            10_000,
            Some(origin),
        )
        .unwrap();
        assert_eq!(summary.frames_written, 1);
        assert_eq!(
            summary.timestamp_basis,
            "caller-host-origin-plus-sample-offset"
        );
        let pcap = fs::read(output).unwrap();
        assert_eq!(&pcap[49..], expected_bytes());
        assert_eq!(u32::from_le_bytes(pcap[24..28].try_into().unwrap()), 1234);
        let subsecond = u32::from_le_bytes(pcap[28..32].try_into().unwrap());
        assert!(subsecond >= 7);
        assert_eq!((subsecond - 7) % 50, 0);
    }

    #[test]
    fn sample_limits_and_failure_never_publish_a_success_file() {
        let directory = TestDirectory::new();
        let input = directory.file("synthetic.cs8");
        fs::write(&input, IQ).unwrap();
        let output = directory.file("limited.pcap");
        let summary = decode(&input, &output, 2_412_000_000, IqFormat::Cs8, 100, None).unwrap();
        assert_eq!(summary.processed_samples, 100);
        assert!(summary.sample_limit_reached);
        assert!(!summary.input_eof_observed);
        assert_eq!(summary.frames_written, 0);
        let malformed = directory.file("malformed.sc16q11");
        fs::write(&malformed, [0, 8, 0, 0]).unwrap();
        let failed = directory.file("failed.pcap");
        assert!(
            decode(
                &malformed,
                &failed,
                2_412_000_000,
                IqFormat::Sc16Q11,
                10_000,
                None
            )
            .is_err()
        );
        assert!(!failed.exists());
        assert!(fs::read_dir(&directory.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }));
        assert!(
            decode(
                &input,
                &failed,
                2_412_000_000,
                IqFormat::Cs8,
                MAX_SAMPLES + 1,
                None
            )
            .is_err()
        );
        assert!(pcap_timestamp(0, Some(u128::MAX)).is_err());
        let mut expected = expected_bytes();
        assert!(valid_fcs(&expected));
        expected[0] ^= 1;
        assert!(!valid_fcs(&expected));
        assert!(!valid_fcs(&[0, 0, 0]));
    }

    #[test]
    fn publication_requires_received_fcs_framing_and_independent_integrity() {
        use crafter::radio::FrameFraming;

        let directory = TestDirectory::new();
        let path = directory.file("untrusted.pcap");
        let mut destination = PcapOutput::new(&path).unwrap();
        let mut frame = RecoveredFrame {
            bytes: expected_bytes(),
            link_type: LinkType::Ieee80211,
            integrity: FrameIntegrity::ValidFcs,
            framing: FrameFraming { trailer_bytes: 0 },
            config: RxConfig {
                sample_rate_hz: SAMPLE_RATE_HZ,
                center_frequency_hz: 2_412_000_000,
                max_chunk_samples: 1,
                max_buffer_samples: 512,
                max_frame_bytes: 4096,
                max_pending_frames: 3,
                max_capture_samples: 1,
                max_duration: Duration::from_secs(1),
            },
            start: IqPosition {
                epoch: 0,
                sequence: 0,
                sample_index: 0,
                time_anchor: None,
                discontinuity: None,
            },
            end_sample_index: 1,
            rate_bps: 6_000_000,
            diagnostics: Vec::new(),
        };
        // Matching trailing bytes alone cannot establish received FCS framing.
        assert!(destination.write_frame(&frame, None).is_err());
        frame.framing.trailer_bytes = 4;
        frame.integrity = FrameIntegrity::FcsAbsent;
        assert!(destination.write_frame(&frame, None).is_err());
        frame.integrity = FrameIntegrity::ValidFcs;
        frame.bytes[0] ^= 1;
        assert!(destination.write_frame(&frame, None).is_err());
        frame.bytes[0] ^= 1;
        frame.start.epoch = 1;
        assert!(destination.write_frame(&frame, None).is_err());
        drop(destination);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 0);
        assert!(
            pcap_timestamp(1, Some(u128::from(u32::MAX) * 1_000_000_000 + 999_999_999)).is_err()
        );
        assert!(pcap_timestamp(1, Some(u128::MAX)).is_err());
    }
}
