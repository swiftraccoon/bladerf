//! Bounded, offline 20 Msps Wi-Fi IQ decoding through libcrafter's radio API.
//!
//! This synchronous function reads regular files and performs CPU work. Its
//! caller must supervise it in a separate process when a wall-clock deadline is
//! required. Sample-time bounds do not bound decoder execution time. It never
//! opens a radio or enables transmission. SC16 Q11 conversion uses the high
//! eight bits of each component. Optional fixed gain precedes that conversion.
//!
//! Output has sample-relative timestamps unless the caller supplies a host
//! time origin. Neither choice establishes hardware timestamp accuracy. Minimal
//! radiotap records carry only the presence of the actual received FCS, which is
//! checked again independently here. No RF power or noise calibration is claimed.
//!
//! A private sibling staging file is published without replacement only after
//! successful decoding and synchronization. Ordinary errors remove staging;
//! terminating the process can leave a private `.wifi-decode-*.partial` file.
//! Optional evidence files are synced and linked before the PCAP. A later
//! publication error can leave evidence files alone. Accept them only after
//! worker success and terminal summary verification.

mod antiwrap;
pub(crate) mod modern;
mod modern_process;
mod modern_records;
mod modern_vht;
mod translate;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use crafter::LinkType;
use crafter::radio::{
    DecodeOutput, DecoderStats, FrameIntegrity, HtSignalFields, IqEvent, IqPosition, IqSource,
    OfdmDataFailureStats, OfdmRejectionStats, OfdmStrongLongTrainingStats, PhyDecoder,
    PhyDiagnostic, ReaderIqSource, RecoveredFrame, RxConfig, StreamEnd, WifiDecoder,
};
use serde::Serialize;

use crate::packets::Output;

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const FILTER_FREE_SHIFT_HZ: u64 = 1_000_000;
const NANOSECONDS_PER_SAMPLE: u128 = 50;
const MAX_SAMPLES: u64 = 200_000_000;
const CHUNK_SAMPLES: usize = 4_096;
const BUFFER_SAMPLES: usize = 1_048_576;
const FRAME_BYTES: usize = 65_535;
const PENDING_FRAMES: usize = 64;
const MAX_FRAME_SPANS: usize = 65_536;
const MAX_PREAMBLE_EVENTS: usize = 4_096;
const SNAPLEN: usize = 65_536;
const MIN_Q11: i16 = -2048;
const MAX_Q11: i16 = 2047;
const DOUBLE_Q11_FACTOR: i16 = 2;
const RADIOTAP: [u8; 9] = [0, 0, 9, 0, 2, 0, 0, 0, 0x10];
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Headerless, interleaved I then Q samples, with little-endian SC16 components.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IqFormat {
    Sc16Q11,
    Cs8,
}

impl IqFormat {
    fn bytes_per_sample(self) -> usize {
        match self {
            Self::Sc16Q11 => 4,
            Self::Cs8 => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DecodeSettings {
    pub(crate) time_origin_ns: Option<u128>,
    pub(crate) frame_spans: bool,
    pub(crate) ht_signal_events: bool,
    pub(crate) he_su_signal_events: bool,
    pub(crate) ampdu_error_events: bool,
    pub(crate) capture_frequency_hz: Option<u64>,
    pub(crate) double_q11: bool,
}

#[derive(Debug, Serialize)]
struct Q11Gain {
    factor: i16,
    clipped_components: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct DecodeSummary {
    pub(crate) schema_version: u32,
    pub(crate) operation: &'static str,
    pub(crate) decoder: &'static str,
    pub(crate) iq_format: IqFormat,
    pub(crate) center_frequency_hz: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) capture_frequency_hz: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) frequency_shift_hz: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) translation_filter: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) translation_clipped_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    q11_gain: Option<Q11Gain>,
    pub(crate) sample_rate_hz: u32,
    pub(crate) input_samples: u64,
    pub(crate) processed_samples: u64,
    pub(crate) sample_limit: u64,
    pub(crate) sample_limit_reached: bool,
    pub(crate) input_eof_observed: bool,
    pub(crate) timestamp_basis: &'static str,
    pub(crate) frames_written: u64,
    pub(crate) ofdm: OfdmStats,
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
pub(crate) struct OfdmStats {
    pub(crate) valid_frames: u64,
    pub(crate) invalid_fcs: u64,
    pub(crate) rejected_frames: u64,
    pub(crate) truncated_frames: u64,
    pub(crate) dropped_frames: u64,
    pub(crate) rejections: OfdmRejections,
    pub(crate) data_failures: OfdmDataFailures,
    pub(crate) strong_long_training: OfdmStrongLongTraining,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OfdmRejections {
    pub(crate) signal_or_probe_rejected: u64,
    pub(crate) greenfield_header_unsupported: u64,
    pub(crate) greenfield_probe_unrecognized: u64,
    pub(crate) mixed_header_unsupported: u64,
    pub(crate) legacy_fallback_unreservable: u64,
    pub(crate) data_decode_failed: u64,
    pub(crate) pending_capacity_rejected: u64,
    pub(crate) strong_long_training_not_found: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OfdmDataFailures {
    pub(crate) legacy: u64,
    pub(crate) ht_bcc: u64,
    pub(crate) ht_ldpc: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OfdmStrongLongTraining {
    pub(crate) strong_short_periodicity_timeout: u64,
    pub(crate) strong_long_repeat_not_found: u64,
    pub(crate) strong_long_reference_not_found: u64,
    pub(crate) strong_long_coordinates_unavailable: u64,
}

impl
    TryFrom<(
        DecoderStats,
        OfdmRejectionStats,
        OfdmDataFailureStats,
        OfdmStrongLongTrainingStats,
    )> for OfdmStats
{
    type Error = anyhow::Error;

    fn try_from(
        (stats, rejections, data_failures, strong_long_training): (
            DecoderStats,
            OfdmRejectionStats,
            OfdmDataFailureStats,
            OfdmStrongLongTrainingStats,
        ),
    ) -> Result<Self> {
        let rejection_total = rejections.checked_total().context(
            "OFDM rejection accounting overflow: reason counters cannot be summed as u64",
        )?;
        ensure!(
            rejection_total == stats.rejected_frames,
            "OFDM rejection accounting mismatch: reason total {rejection_total} does not equal rejected_frames {}",
            stats.rejected_frames
        );
        let data_failure_total = data_failures.checked_total().context(
            "OFDM data-failure accounting overflow: detail counters cannot be summed as u64",
        )?;
        ensure!(
            data_failure_total == rejections.data_decode_failed,
            "OFDM data-failure accounting mismatch: detail total {data_failure_total} does not equal data_decode_failed {}",
            rejections.data_decode_failed
        );
        let strong_long_training_total = strong_long_training.checked_total().context(
            "OFDM strong long-training accounting overflow: detail counters cannot be summed as u64",
        )?;
        ensure!(
            strong_long_training_total == rejections.strong_long_training_not_found,
            "OFDM strong long-training accounting mismatch: detail total {strong_long_training_total} does not equal strong_long_training_not_found {}",
            rejections.strong_long_training_not_found
        );
        Ok(Self {
            valid_frames: stats.valid_frames,
            invalid_fcs: stats.invalid_fcs,
            rejected_frames: stats.rejected_frames,
            truncated_frames: stats.truncated_frames,
            dropped_frames: stats.dropped_frames,
            rejections: OfdmRejections {
                signal_or_probe_rejected: rejections.signal_or_probe_rejected,
                greenfield_header_unsupported: rejections.greenfield_header_unsupported,
                greenfield_probe_unrecognized: rejections.greenfield_probe_unrecognized,
                mixed_header_unsupported: rejections.mixed_header_unsupported,
                legacy_fallback_unreservable: rejections.legacy_fallback_unreservable,
                data_decode_failed: rejections.data_decode_failed,
                pending_capacity_rejected: rejections.pending_capacity_rejected,
                strong_long_training_not_found: rejections.strong_long_training_not_found,
            },
            data_failures: OfdmDataFailures {
                legacy: data_failures.legacy,
                ht_bcc: data_failures.ht_bcc,
                ht_ldpc: data_failures.ht_ldpc,
            },
            strong_long_training: OfdmStrongLongTraining {
                strong_short_periodicity_timeout: strong_long_training
                    .strong_short_periodicity_timeout,
                strong_long_repeat_not_found: strong_long_training.strong_long_repeat_not_found,
                strong_long_reference_not_found: strong_long_training
                    .strong_long_reference_not_found,
                strong_long_coordinates_unavailable: strong_long_training
                    .strong_long_coordinates_unavailable,
            },
        })
    }
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
/// Optional frame spans and HT-SIG positions use local samples even when the
/// PCAP has a host origin.
/// A distinct capture frequency translates complex samples at the original
/// rate and leaves sample positions intact.
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
    settings: DecodeSettings,
) -> Result<DecodeSummary> {
    decode_with_ofdm_accounting(
        input,
        output,
        frequency_hz,
        format,
        max_samples,
        settings,
        |stats, rejections, data_failures, strong_long_training| {
            (stats, rejections, data_failures, strong_long_training)
        },
    )
}

fn cs8_reader<'a>(
    file: File,
    format: IqFormat,
    admitted_samples: u64,
    shift_hz: Option<i64>,
    translation_clipped: &'a Cell<u64>,
    q11_gain: Option<&'a Cell<u64>>,
) -> Result<Box<dyn Read + 'a>> {
    if let Some(shift_hz) = shift_hz {
        if shift_hz.unsigned_abs() > FILTER_FREE_SHIFT_HZ {
            return Ok(Box::new(antiwrap::FilteredReader::new(
                file.take(admitted_samples * format.bytes_per_sample() as u64),
                format,
                shift_hz,
                usize::try_from(admitted_samples)
                    .context("admitted IQ sample count exceeds usize")?,
                translation_clipped,
                q11_gain,
            )));
        }
        return Ok(Box::new(translate::TranslatedReader::new(
            file,
            format,
            shift_hz,
            translation_clipped,
            q11_gain,
        )));
    }
    Ok(match format {
        IqFormat::Sc16Q11 => Box::new(Sc16Q11Reader::new(file, q11_gain)),
        IqFormat::Cs8 => Box::new(BufReader::new(file)),
    })
}

fn decode_with_ofdm_accounting<F>(
    input: &Path,
    output: &Path,
    frequency_hz: u64,
    format: IqFormat,
    max_samples: u64,
    settings: DecodeSettings,
    accounting: F,
) -> Result<DecodeSummary>
where
    F: FnOnce(
        DecoderStats,
        OfdmRejectionStats,
        OfdmDataFailureStats,
        OfdmStrongLongTrainingStats,
    ) -> (
        DecoderStats,
        OfdmRejectionStats,
        OfdmDataFailureStats,
        OfdmStrongLongTrainingStats,
    ),
{
    ensure!(
        (1..=MAX_SAMPLES).contains(&max_samples),
        "decode sample limit must be 1..={MAX_SAMPLES}"
    );
    ensure!(frequency_hz != 0, "center frequency must be nonzero");
    ensure!(
        !settings.double_q11 || format == IqFormat::Sc16Q11,
        "--double-q11 requires SC16 Q11 input"
    );
    let frequency_shift_hz =
        crate::replay_frequency_shift(settings.capture_frequency_hz, frequency_hz)?;
    let time_origin_ns = settings.time_origin_ns;
    pcap_timestamp(0, time_origin_ns)?;
    let file = File::open(input).with_context(|| format!("open IQ input {}", input.display()))?;
    let metadata = file.metadata().context("inspect IQ input")?;
    ensure!(metadata.is_file(), "IQ input must be a regular file");
    let bytes_per_sample = format.bytes_per_sample() as u64;
    ensure!(
        metadata.len() % bytes_per_sample == 0,
        "IQ input ends within a complete I/Q sample"
    );
    let input_samples = metadata.len() / bytes_per_sample;
    let translation_clipped = Cell::new(0);
    let q11_gain_clipped = Cell::new(0);
    let q11_gain = settings.double_q11.then_some(&q11_gain_clipped);
    let filtered_shift_hz =
        frequency_shift_hz.filter(|shift_hz| shift_hz.unsigned_abs() > FILTER_FREE_SHIFT_HZ);
    let reader = cs8_reader(
        file,
        format,
        input_samples.min(max_samples),
        frequency_shift_hz,
        &translation_clipped,
        q11_gain,
    )?;
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
    decoder.set_he_su_signal_events(settings.he_su_signal_events);
    let mut destination = PcapOutput::new(output, settings)?;
    let mut summary = DecodeSummary {
        schema_version: 4,
        operation: "offline_wifi_iq_decode",
        decoder: "libcrafter-wifi-serial",
        iq_format: format,
        center_frequency_hz: frequency_hz,
        capture_frequency_hz: settings
            .capture_frequency_hz
            .filter(|_| frequency_shift_hz.is_some()),
        frequency_shift_hz,
        translation_filter: filtered_shift_hz.map(|_| antiwrap::FILTER_NAME),
        translation_clipped_samples: None,
        q11_gain: None,
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
        ofdm: OfdmStats::default(),
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
    let (ofdm_stats, ofdm_rejections, ofdm_data_failures, ofdm_strong_long_training) = accounting(
        decoder.ofdm_stats(),
        decoder.ofdm_rejection_stats(),
        decoder.ofdm_data_failure_stats(),
        decoder.ofdm_strong_long_training_stats(),
    );
    summary.ofdm = OfdmStats::try_from((
        ofdm_stats,
        ofdm_rejections,
        ofdm_data_failures,
        ofdm_strong_long_training,
    ))?;
    summary.dsss = decoder.dsss_stats().into();
    summary.translation_clipped_samples = frequency_shift_hz.map(|_| translation_clipped.get());
    summary.q11_gain = q11_gain.map(|clipped| Q11Gain {
        factor: DOUBLE_Q11_FACTOR,
        clipped_components: clipped.get(),
    });
    destination.publish(&summary)?;
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
        // Recovered frames carry their own copy, including one per aggregate member.
        match &diagnostic {
            PhyDiagnostic::HtSignal {
                fields,
                preamble_sample_index,
            } => {
                if let Some(events) = &mut destination.ht_signal_events {
                    events.record(*preamble_sample_index, (*fields).into());
                }
            }
            PhyDiagnostic::HeSuSignal {
                preamble_sample_index,
                mcs,
                space_time_streams,
                ldpc,
                guard_interval_ns,
                bss_color,
            } => {
                if let Some(events) = &mut destination.he_su_signal_events {
                    events.record(
                        *preamble_sample_index,
                        HeSuSignalEventFields {
                            mcs: *mcs,
                            space_time_streams: *space_time_streams,
                            ldpc: *ldpc,
                            guard_interval_ns: *guard_interval_ns,
                            bss_color: *bss_color,
                        },
                    );
                }
            }
            PhyDiagnostic::AmpduErrors {
                preamble_sample_index,
                invalid_delimiters,
                invalid_fcs,
                truncated_mpdus,
                oversized_mpdus,
            } => {
                if let Some(events) = &mut destination.ampdu_error_events {
                    events.record(
                        *preamble_sample_index,
                        AmpduErrorCounts {
                            invalid_delimiters: *invalid_delimiters,
                            invalid_fcs: *invalid_fcs,
                            truncated_mpdus: *truncated_mpdus,
                            oversized_mpdus: *oversized_mpdus,
                        },
                    );
                }
            }
            _ => {}
        }
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
        PhyDiagnostic::HeSuSignal { .. } => "he-su-signal",
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

fn q11_component(bytes: [u8; 2], gain_clipped: Option<&Cell<u64>>) -> io::Result<i16> {
    let component = i16::from_le_bytes(bytes);
    if !(MIN_Q11..=MAX_Q11).contains(&component) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SC16 Q11 component is outside -2048..=2047",
        ));
    }
    let Some(clipped) = gain_clipped else {
        return Ok(component);
    };
    let gained = component * DOUBLE_Q11_FACTOR;
    let saturated = gained.clamp(MIN_Q11, MAX_Q11);
    if gained != saturated {
        clipped.set(clipped.get() + 1);
    }
    Ok(saturated)
}

/// Converts signed Q11 components to cs8 in bounded blocks.
#[derive(Debug)]
struct Sc16Q11Reader<'a, R> {
    inner: R,
    raw: [u8; CHUNK_SAMPLES * 4],
    gain_clipped: Option<&'a Cell<u64>>,
}

impl<'a, R: Read> Sc16Q11Reader<'a, R> {
    fn new(inner: R, gain_clipped: Option<&'a Cell<u64>>) -> Self {
        Self {
            inner,
            raw: [0; CHUNK_SAMPLES * 4],
            gain_clipped,
        }
    }
}

impl<R: Read> Read for Sc16Q11Reader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let components = output.len().min(self.raw.len() / 2);
        let raw = &mut self.raw[..components * 2];
        let mut filled = 0;
        let mut converted = 0;
        while filled < raw.len() {
            match self.inner.read(&mut raw[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
            // Retain an incomplete component for the next short read.
            let (complete, _) = raw[converted * 2..filled].as_chunks::<2>();
            for (out, &bytes) in output[converted..].iter_mut().zip(complete) {
                let component = q11_component(bytes, self.gain_clipped)?;
                *out = (component >> 4) as i8 as u8;
            }
            converted += complete.len();
        }
        if !filled.is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated SC16 Q11 component",
            ));
        }
        Ok(converted)
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
    frame_spans: Option<FrameSpansOutput>,
    ht_signal_events: Option<PreambleEventsOutput<HtSignalEventFields>>,
    he_su_signal_events: Option<PreambleEventsOutput<HeSuSignalEventFields>>,
    ampdu_error_events: Option<PreambleEventsOutput<AmpduErrorCounts>>,
}

impl PcapOutput {
    fn new(destination: &Path, settings: DecodeSettings) -> Result<Self> {
        ensure!(!destination.exists(), "decode output already exists");
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let frame_spans = settings
            .frame_spans
            .then(|| FrameSpansOutput::new(&destination.with_extension("frame-spans.json")))
            .transpose()?;
        let ht_signal_events = settings
            .ht_signal_events
            .then(|| {
                PreambleEventsOutput::new(&destination.with_extension("ht-signal-events.json"))
            })
            .transpose()?;
        let he_su_signal_events = settings
            .he_su_signal_events
            .then(|| {
                PreambleEventsOutput::new(&destination.with_extension("he-su-signal-events.json"))
            })
            .transpose()?;
        let ampdu_error_events = settings
            .ampdu_error_events
            .then(|| {
                PreambleEventsOutput::new(&destination.with_extension("ampdu-error-events.json"))
            })
            .transpose()?;
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
                        frame_spans,
                        ht_signal_events,
                        he_su_signal_events,
                        ampdu_error_events,
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
        ensure!(frame.start.epoch == 0, "unexpected IQ epoch change");
        self.write_verified_mpdu(
            &frame.bytes,
            frame.start.sample_index,
            frame.end_sample_index,
            origin,
        )
    }

    fn write_verified_mpdu(
        &mut self,
        bytes: &[u8],
        start_sample: u64,
        end_sample_exclusive: u64,
        origin: Option<u128>,
    ) -> Result<()> {
        ensure!(valid_fcs(bytes), "independent frame FCS check failed");
        let length = RADIOTAP.len() + bytes.len();
        ensure!(
            length <= SNAPLEN,
            "recovered frame exceeds PCAP record bound"
        );
        let (seconds, nanoseconds) = pcap_timestamp(start_sample, origin)?;
        if let Some(spans) = &mut self.frame_spans {
            ensure!(
                spans.samples.len() < MAX_FRAME_SPANS,
                "Wi-Fi frame spans exceed the {MAX_FRAME_SPANS}-record limit"
            );
            spans.samples.push([start_sample, end_sample_exclusive]);
        }
        self.writer.write_all(&seconds.to_le_bytes())?;
        self.writer.write_all(&nanoseconds.to_le_bytes())?;
        self.writer.write_all(&(length as u32).to_le_bytes())?;
        self.writer.write_all(&(length as u32).to_le_bytes())?;
        self.writer.write_all(&RADIOTAP)?;
        self.writer.write_all(bytes)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.writer.flush().context("flush decoded PCAP")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("sync decoded PCAP")
    }

    fn publish(&mut self, summary: &DecodeSummary) -> Result<()> {
        self.sync()?;
        if let Some(spans) = &mut self.frame_spans {
            spans.publish(
                summary.input_samples,
                summary.processed_samples,
                summary.frames_written,
            )?;
        }
        if let Some(events) = &mut self.ht_signal_events {
            events.publish(summary, "offline_wifi_ht_signal_events")?;
        }
        if let Some(events) = &mut self.he_su_signal_events {
            events.publish(summary, "offline_wifi_he_su_signal_events")?;
        }
        if let Some(events) = &mut self.ampdu_error_events {
            events.publish(summary, "offline_wifi_ampdu_error_events")?;
        }
        self.publish_pcap()
    }

    fn publish_pcap(&mut self) -> Result<()> {
        fs::hard_link(&self.temporary, &self.destination)
            .context("publish decoded PCAP without replacing existing files")?;
        // Publication is complete. A cleanup failure must not report a failed
        // decode after a complete destination became visible; Drop retries it.
        let _ = fs::remove_file(&self.temporary);
        Ok(())
    }
}

#[derive(Debug)]
struct FrameSpansOutput {
    output: Output,
    samples: Vec<[u64; 2]>,
}

#[derive(Serialize)]
struct FrameSpans<'a> {
    schema_version: u32,
    sample_rate_hz: u32,
    input_samples: u64,
    processed_samples: u64,
    spans: &'a [[u64; 2]],
}

impl FrameSpansOutput {
    fn new(destination: &Path) -> Result<Self> {
        Ok(Self {
            output: Output::new(destination)?,
            samples: Vec::new(),
        })
    }

    fn publish(
        &mut self,
        input_samples: u64,
        processed_samples: u64,
        frames_written: u64,
    ) -> Result<()> {
        ensure!(
            self.samples.len() as u64 == frames_written
                && self
                    .samples
                    .iter()
                    .all(|&[start, end]| start < end && end <= processed_samples),
            "Wi-Fi frame spans differ from the processed sample extent or frame count"
        );
        let spans = FrameSpans {
            schema_version: 1,
            sample_rate_hz: SAMPLE_RATE_HZ,
            input_samples,
            processed_samples,
            spans: &self.samples,
        };
        serde_json::to_writer(&mut self.output.writer, &spans)
            .context("write Wi-Fi frame spans")?;
        self.output.writer.write_all(b"\n")?;
        self.output.prepare()?;
        self.output.publish()
    }
}

#[derive(Debug, Serialize)]
struct HtSignalEventFields {
    mcs: u8,
    psdu_bytes: u16,
    channel_width_40_mhz: bool,
    ldpc: bool,
    short_guard_interval: bool,
    aggregation: bool,
    stbc: u8,
    extension_spatial_streams: u8,
}

impl From<HtSignalFields> for HtSignalEventFields {
    fn from(fields: HtSignalFields) -> Self {
        Self {
            mcs: fields.mcs,
            psdu_bytes: fields.psdu_bytes,
            channel_width_40_mhz: fields.channel_width_40_mhz,
            ldpc: fields.ldpc,
            short_guard_interval: fields.short_guard_interval,
            aggregation: fields.aggregation,
            stbc: fields.stbc,
            extension_spatial_streams: fields.extension_spatial_streams,
        }
    }
}

#[derive(Debug, Serialize)]
struct HeSuSignalEventFields {
    mcs: u8,
    space_time_streams: u8,
    ldpc: bool,
    guard_interval_ns: u16,
    bss_color: u8,
}

/// Aggregate scan errors at a PPDU preamble. Resynchronization can count
/// multiple scan errors within one aggregate.
#[derive(Debug, Serialize)]
struct AmpduErrorCounts {
    invalid_delimiters: usize,
    invalid_fcs: usize,
    truncated_mpdus: usize,
    oversized_mpdus: usize,
}

#[derive(Debug, Serialize)]
struct PreambleEvent<T> {
    preamble_sample_index: u64,
    #[serde(flatten)]
    details: T,
}

#[derive(Debug)]
struct PreambleEventsOutput<T> {
    output: Output,
    events: Vec<PreambleEvent<T>>,
    events_over_limit: u64,
}

#[derive(Serialize)]
struct PreambleEvents<'a, T> {
    schema_version: u32,
    operation: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    input_samples: u64,
    processed_samples: u64,
    diagnostic_batches_at_capacity: u64,
    event_limit: usize,
    events_over_limit: u64,
    events: &'a [PreambleEvent<T>],
}

impl<T: Serialize> PreambleEventsOutput<T> {
    fn new(destination: &Path) -> Result<Self> {
        Ok(Self {
            output: Output::new(destination)?,
            events: Vec::new(),
            events_over_limit: 0,
        })
    }

    fn record(&mut self, preamble_sample_index: u64, details: T) {
        if self.events.len() == MAX_PREAMBLE_EVENTS {
            self.events_over_limit += 1;
            return;
        }
        self.events.push(PreambleEvent {
            preamble_sample_index,
            details,
        });
    }

    fn publish(&mut self, summary: &DecodeSummary, operation: &'static str) -> Result<()> {
        ensure!(
            self.events
                .iter()
                .all(|event| event.preamble_sample_index < summary.processed_samples),
            "Wi-Fi preamble position lies outside processed IQ ({operation})"
        );
        let events = PreambleEvents {
            schema_version: 1,
            operation,
            center_frequency_hz: summary.center_frequency_hz,
            sample_rate_hz: summary.sample_rate_hz,
            input_samples: summary.input_samples,
            processed_samples: summary.processed_samples,
            diagnostic_batches_at_capacity: summary.diagnostic_batches_at_capacity,
            event_limit: MAX_PREAMBLE_EVENTS,
            events_over_limit: self.events_over_limit,
            events: &self.events,
        };
        serde_json::to_writer(&mut self.output.writer, &events)
            .with_context(|| format!("write Wi-Fi preamble events ({operation})"))?;
        self.output.writer.write_all(b"\n")?;
        self.output.prepare()?;
        self.output.publish()
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
    const INVALID_SERVICE_IQ: &[u8] = include_bytes!(
        "../../../repos/libcrafter/crafter/tests/fixtures/iq/ofdm-6-invalid_service.cs8"
    );
    const BAD_FCS_IQ: &[u8] =
        include_bytes!("../../../repos/libcrafter/crafter/tests/fixtures/iq/ofdm-6-bad_fcs.cs8");
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

    fn rejection_total(rejections: &OfdmRejections) -> Option<u64> {
        [
            rejections.signal_or_probe_rejected,
            rejections.greenfield_header_unsupported,
            rejections.greenfield_probe_unrecognized,
            rejections.mixed_header_unsupported,
            rejections.legacy_fallback_unreservable,
            rejections.data_decode_failed,
            rejections.pending_capacity_rejected,
            rejections.strong_long_training_not_found,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
    }

    fn strong_long_training_total(details: &OfdmStrongLongTraining) -> Option<u64> {
        [
            details.strong_short_periodicity_timeout,
            details.strong_long_repeat_not_found,
            details.strong_long_reference_not_found,
            details.strong_long_coordinates_unavailable,
        ]
        .into_iter()
        .try_fold(0_u64, u64::checked_add)
    }

    fn data_failure_total(details: &OfdmDataFailures) -> Option<u64> {
        [details.legacy, details.ht_bcc, details.ht_ldpc]
            .into_iter()
            .try_fold(0_u64, u64::checked_add)
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
        let summary = decode(
            &input,
            &output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
        )
        .unwrap();
        assert_eq!(summary.schema_version, 4);
        assert_eq!(summary.operation, "offline_wifi_iq_decode");
        let serialized = serde_json::to_value(&summary).unwrap();
        assert!(serialized.get("q11_gain").is_none());
        assert_eq!(serialized["schema_version"], 4);
        assert_eq!(serialized["operation"], "offline_wifi_iq_decode");
        assert!(serialized["ofdm"]["rejections"].is_object());
        assert!(serialized["ofdm"]["data_failures"].is_object());
        assert!(serialized["ofdm"]["strong_long_training"].is_object());
        assert!(serialized["dsss"].get("rejections").is_none());
        assert!(serialized["dsss"].get("data_failures").is_none());
        assert!(serialized["dsss"].get("strong_long_training").is_none());
        assert_eq!(summary.frames_written, 1);
        assert_eq!(summary.ofdm.valid_frames, 1);
        assert_eq!(summary.ofdm.dropped_frames, 0);
        assert_eq!(
            rejection_total(&summary.ofdm.rejections),
            Some(summary.ofdm.rejected_frames)
        );
        assert_eq!(
            data_failure_total(&summary.ofdm.data_failures),
            Some(summary.ofdm.rejections.data_decode_failed)
        );
        assert_eq!(
            strong_long_training_total(&summary.ofdm.strong_long_training),
            Some(summary.ofdm.rejections.strong_long_training_not_found)
        );
        assert_eq!(summary.processed_samples, 2149);
        assert!(summary.input_eof_observed);
        assert!(!summary.sample_limit_reached);
        let pcap = fs::read(&output).unwrap();
        assert_eq!(&pcap[40..49], &RADIOTAP);
        assert_eq!(&pcap[49..], expected_bytes());
        let validated = crate::packets::inspect(&output, None, None).unwrap();
        assert_eq!(validated.validated_frames, 1);
        assert!(
            decode(
                &input,
                &output,
                2_412_000_000,
                IqFormat::Cs8,
                10_000,
                DecodeSettings::default()
            )
            .is_err()
        );
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
    fn frame_spans_follow_pcap_order_and_share_aggregate_phy_bounds() {
        // The independent OFDM fixture has a 37-sample prefix, then the
        // 400-sample preamble/SIGNAL and 21 80-sample DATA symbols.
        let legacy = &IQ[..2 * 2_117];
        let aggregate = include_bytes!(
            "../../../repos/libcrafter/crafter/tests/fixtures/iq/ht-ampdu-7-gi800-ldpc-duplicate.cs8"
        );
        for (bytes, expected) in [
            (legacy, &[[37, 2_117]][..]),
            (&aggregate[..], &[[37, 1_077], [37, 1_077]][..]),
        ] {
            let directory = TestDirectory::new();
            let input = directory.file("synthetic.cs8");
            fs::write(&input, bytes).unwrap();
            let with_spans = directory.file("candidates.pcap");
            let without_spans = directory.file("default.pcap");
            let decode_case = |output, frame_spans| {
                decode(
                    &input,
                    output,
                    2_412_000_000,
                    IqFormat::Cs8,
                    10_000,
                    DecodeSettings {
                        frame_spans,
                        ..DecodeSettings::default()
                    },
                )
                .unwrap()
            };
            let summary = decode_case(&with_spans, true);
            let default_summary = decode_case(&without_spans, false);
            assert_eq!(
                fs::read(&with_spans).unwrap(),
                fs::read(&without_spans).unwrap()
            );
            assert_eq!(
                serde_json::to_vec(&summary).unwrap(),
                serde_json::to_vec(&default_summary).unwrap()
            );
            assert!(!without_spans.with_extension("frame-spans.json").exists());
            assert_eq!(summary.frames_written, expected.len() as u64);
            let spans: serde_json::Value = serde_json::from_slice(
                &fs::read(with_spans.with_extension("frame-spans.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                spans,
                serde_json::json!({
                    "schema_version": 1,
                    "sample_rate_hz": 20_000_000,
                    "input_samples": bytes.len() / 2,
                    "processed_samples": bytes.len() / 2,
                    "spans": expected,
                })
            );

            let pcap = fs::read(&with_spans).unwrap();
            let mut records = &pcap[24..];
            for &[start, _] in expected {
                let (header, body) = records.split_at(16);
                let included = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
                assert_eq!(&header[..4], &[0; 4]);
                assert_eq!(
                    u32::from_le_bytes(header[4..8].try_into().unwrap()),
                    start * 50
                );
                records = &body[included..];
            }
            assert!(records.is_empty());
        }
    }

    #[test]
    fn an_existing_frame_span_file_prevents_new_pcap_publication() {
        let directory = TestDirectory::new();
        let input = directory.file("synthetic.cs8");
        fs::write(&input, IQ).unwrap();
        let output = directory.file("candidates.pcap");
        let spans = output.with_extension("frame-spans.json");
        fs::write(&spans, b"existing").unwrap();
        assert!(
            decode(
                &input,
                &output,
                2_412_000_000,
                IqFormat::Cs8,
                10_000,
                DecodeSettings {
                    frame_spans: true,
                    ..DecodeSettings::default()
                },
            )
            .is_err()
        );
        assert_eq!(fs::read(spans).unwrap(), b"existing");
        assert!(!output.exists());
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 2);
    }

    #[test]
    fn ht_signal_events_keep_one_header_per_observed_diagnostic() {
        // The independent aggregate fixture starts at sample 37. Its mixed
        // preamble and both SIGNAL fields fit in the next 560 samples.
        let aggregate = include_bytes!(
            "../../../repos/libcrafter/crafter/tests/fixtures/iq/ht-ampdu-7-gi800-ldpc-duplicate.cs8"
        );
        for (iq, frames) in [(&aggregate[..], 2), (&aggregate[..2 * 597], 0)] {
            let directory = TestDirectory::new();
            let input = directory.file("synthetic.cs8");
            fs::write(&input, iq).unwrap();
            let output = directory.file("candidates.pcap");
            let default = directory.file("default.pcap");
            let settings = DecodeSettings {
                ht_signal_events: true,
                he_su_signal_events: true,
                ..DecodeSettings::default()
            };
            let run = |pcap, settings| {
                decode(&input, pcap, 2_412_000_000, IqFormat::Cs8, 10_000, settings)
            };
            let summary = run(&output, settings).unwrap();
            let default_summary = run(&default, DecodeSettings::default()).unwrap();
            assert_eq!(summary.frames_written, frames);
            assert_eq!(fs::read(&output).unwrap(), fs::read(&default).unwrap());
            assert_eq!(
                serde_json::to_vec(&summary).unwrap(),
                serde_json::to_vec(&default_summary).unwrap()
            );
            assert!(!default.with_extension("ht-signal-events.json").exists());
            let he_events: serde_json::Value = serde_json::from_slice(
                &fs::read(output.with_extension("he-su-signal-events.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(he_events["events"], serde_json::json!([]));

            let events_file = output.with_extension("ht-signal-events.json");
            let events: serde_json::Value =
                serde_json::from_slice(&fs::read(&events_file).unwrap()).unwrap();
            assert_eq!(
                events,
                serde_json::json!({
                    "schema_version": 1,
                    "operation": "offline_wifi_ht_signal_events",
                    "center_frequency_hz": 2_412_000_000_u64,
                    "sample_rate_hz": 20_000_000,
                    "input_samples": iq.len() / 2,
                    "processed_samples": iq.len() / 2,
                    "diagnostic_batches_at_capacity": 0,
                    "event_limit": MAX_PREAMBLE_EVENTS,
                    "events_over_limit": 0,
                    "events": [{
                        "preamble_sample_index": 37,
                        "mcs": 7,
                        "psdu_bytes": 120,
                        "channel_width_40_mhz": false,
                        "ldpc": true,
                        "short_guard_interval": false,
                        "aggregation": true,
                        "stbc": 0,
                        "extension_spatial_streams": 0
                    }]
                })
            );
            fs::remove_file(&output).unwrap();
            assert!(run(&output, settings).is_err());
            assert!(!output.exists());
            assert!(events_file.exists());
        }
    }

    #[test]
    fn he_su_signal_events_require_both_rl_sig_and_complete_sig_a() {
        // Prefix of libcrafter's independent he-ampdu-iq-mcs0-ltf1-gi800-multi
        // vector at 00f1fed7ed8984cfc89360fd7ffbe19ba6c36a5b. The preamble
        // starts at sample 37 and HE-SIG-A ends at exclusive sample 677.
        let prefix = include_bytes!("../tests/fixtures/he-su20-preamble.cs8");
        let mut missing_rl_sig = prefix.to_vec();
        missing_rl_sig[2 * 437..2 * 517].fill(0);
        let mut missing_sig_a = prefix.to_vec();
        missing_sig_a[2 * 517..].fill(0);
        for (iq, expected) in [
            (&prefix[..], 1),
            (&prefix[..2 * 676], 0),
            (&missing_rl_sig[..], 0),
            (&missing_sig_a[..], 0),
        ] {
            let directory = TestDirectory::new();
            let input = directory.file("preamble.cs8");
            fs::write(&input, iq).unwrap();
            let pcap = directory.file("candidates.pcap");
            let default_pcap = directory.file("default.pcap");
            let run = |output, enabled| {
                decode(
                    &input,
                    output,
                    2_437_000_000,
                    IqFormat::Cs8,
                    10_000,
                    DecodeSettings {
                        he_su_signal_events: enabled,
                        ..DecodeSettings::default()
                    },
                )
                .unwrap()
            };
            let summary = run(&pcap, true);
            run(&default_pcap, false);
            assert_eq!(summary.frames_written, 0);
            assert_eq!(fs::read(&pcap).unwrap(), fs::read(&default_pcap).unwrap());
            assert!(
                !default_pcap
                    .with_extension("he-su-signal-events.json")
                    .exists()
            );
            let events: serde_json::Value = serde_json::from_slice(
                &fs::read(pcap.with_extension("he-su-signal-events.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(events["operation"], "offline_wifi_he_su_signal_events");
            let events = events["events"].as_array().unwrap();
            assert_eq!(events.len(), expected);
            if expected == 1 {
                assert_eq!(
                    events[0],
                    serde_json::json!({
                        "preamble_sample_index": 37,
                        "mcs": 0,
                        "space_time_streams": 1,
                        "ldpc": false,
                        "guard_interval_ns": 800,
                        "bss_color": 0,
                    })
                );
            }
        }
    }

    #[test]
    fn ampdu_error_events_keep_scanner_counts_at_the_preamble() {
        let bad_fcs = include_bytes!(
            "../../../repos/libcrafter/crafter/tests/fixtures/iq/ht-ampdu-7-gi800-bcc-bad_fcs.cs8"
        );
        let clean = include_bytes!(
            "../../../repos/libcrafter/crafter/tests/fixtures/iq/ht-ampdu-7-gi800-bcc-duplicate.cs8"
        );
        // The bad-FCS fixture contains a 56-byte failed member followed by a
        // valid member at delimiter offset 60. Four-byte resynchronization
        // examines the 14 intervening offsets after the first delimiter.
        let error = serde_json::json!({
            "preamble_sample_index": 37,
            "invalid_delimiters": 14,
            "invalid_fcs": 1,
            "truncated_mpdus": 0,
            "oversized_mpdus": 0,
        });
        for (iq, frames, expected) in [(&bad_fcs[..], 1, vec![error]), (&clean[..], 2, Vec::new())]
        {
            let directory = TestDirectory::new();
            let input = directory.file("synthetic.cs8");
            fs::write(&input, iq).unwrap();
            let output = directory.file("candidates.pcap");
            let default = directory.file("default.pcap");
            let settings = DecodeSettings {
                ampdu_error_events: true,
                ..DecodeSettings::default()
            };
            let run = |pcap, settings| {
                decode(&input, pcap, 2_412_000_000, IqFormat::Cs8, 10_000, settings)
            };
            let summary = run(&output, settings).unwrap();
            let default_summary = run(&default, DecodeSettings::default()).unwrap();
            assert_eq!(summary.frames_written, frames);
            assert_eq!(fs::read(&output).unwrap(), fs::read(&default).unwrap());
            assert_eq!(
                serde_json::to_vec(&summary).unwrap(),
                serde_json::to_vec(&default_summary).unwrap()
            );
            assert!(!default.with_extension("ampdu-error-events.json").exists());

            let events_file = output.with_extension("ampdu-error-events.json");
            let contents = fs::read(&events_file).unwrap();
            let events: serde_json::Value = serde_json::from_slice(&contents).unwrap();
            assert_eq!(
                events,
                serde_json::json!({
                    "schema_version": 1,
                    "operation": "offline_wifi_ampdu_error_events",
                    "center_frequency_hz": 2_412_000_000_u64,
                    "sample_rate_hz": 20_000_000,
                    "input_samples": iq.len() / 2,
                    "processed_samples": iq.len() / 2,
                    "diagnostic_batches_at_capacity": 0,
                    "event_limit": MAX_PREAMBLE_EVENTS,
                    "events_over_limit": 0,
                    "events": expected,
                })
            );
            fs::remove_file(&output).unwrap();
            assert!(run(&output, settings).is_err());
            assert!(!output.exists());
            assert_eq!(fs::read(events_file).unwrap(), contents);
        }
    }

    #[test]
    fn preamble_events_count_observed_omissions_at_capacity() {
        let directory = TestDirectory::new();
        let mut output =
            PreambleEventsOutput::<HtSignalEventFields>::new(&directory.file("events.json"))
                .unwrap();
        let fields = HtSignalFields {
            mcs: 0,
            channel_width_40_mhz: false,
            psdu_bytes: 100,
            smoothing: false,
            not_sounding: false,
            aggregation: false,
            stbc: 0,
            ldpc: false,
            short_guard_interval: false,
            extension_spatial_streams: 0,
        };
        for index in 0..MAX_PREAMBLE_EVENTS + 3 {
            output.record(index as u64, fields.into());
        }
        assert_eq!(output.events.len(), MAX_PREAMBLE_EVENTS);
        assert_eq!(output.events_over_limit, 3);
        assert_eq!(
            output.events.last().unwrap().preamble_sample_index,
            (MAX_PREAMBLE_EVENTS - 1) as u64
        );
    }

    #[test]
    fn ofdm_rejection_and_detail_conversion_requires_exact_closure() {
        let reasons = OfdmRejectionStats {
            signal_or_probe_rejected: 1,
            greenfield_header_unsupported: 2,
            greenfield_probe_unrecognized: 3,
            mixed_header_unsupported: 4,
            legacy_fallback_unreservable: 5,
            data_decode_failed: 6,
            pending_capacity_rejected: 7,
            strong_long_training_not_found: 8,
        };
        let strong_details = OfdmStrongLongTrainingStats {
            strong_short_periodicity_timeout: 1,
            strong_long_repeat_not_found: 2,
            strong_long_reference_not_found: 2,
            strong_long_coordinates_unavailable: 3,
        };
        let data_failure_details = OfdmDataFailureStats {
            legacy: 1,
            ht_bcc: 2,
            ht_ldpc: 3,
        };
        let converted = OfdmStats::try_from((
            DecoderStats {
                valid_frames: 9,
                invalid_fcs: 10,
                rejected_frames: 36,
                truncated_frames: 11,
                dropped_frames: 12,
            },
            reasons,
            data_failure_details,
            strong_details,
        ))
        .unwrap();
        assert_eq!(rejection_total(&converted.rejections), Some(36));
        assert_eq!(data_failure_total(&converted.data_failures), Some(6));
        assert_eq!(
            strong_long_training_total(&converted.strong_long_training),
            Some(8)
        );
        let serialized = serde_json::to_value(&converted).unwrap();
        assert_eq!(
            serialized["rejections"],
            serde_json::json!({
                "signal_or_probe_rejected": 1,
                "greenfield_header_unsupported": 2,
                "greenfield_probe_unrecognized": 3,
                "mixed_header_unsupported": 4,
                "legacy_fallback_unreservable": 5,
                "data_decode_failed": 6,
                "pending_capacity_rejected": 7,
                "strong_long_training_not_found": 8,
            })
        );
        assert_eq!(
            serialized["data_failures"],
            serde_json::json!({
                "legacy": 1,
                "ht_bcc": 2,
                "ht_ldpc": 3,
            })
        );
        assert_eq!(
            serialized["strong_long_training"],
            serde_json::json!({
                "strong_short_periodicity_timeout": 1,
                "strong_long_repeat_not_found": 2,
                "strong_long_reference_not_found": 2,
                "strong_long_coordinates_unavailable": 3,
            })
        );

        let mismatch = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: 35,
                ..DecoderStats::default()
            },
            reasons,
            data_failure_details,
            strong_details,
        ))
        .unwrap_err();
        assert_eq!(
            mismatch.to_string(),
            "OFDM rejection accounting mismatch: reason total 36 does not equal rejected_frames 35"
        );

        let overflow = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                signal_or_probe_rejected: u64::MAX,
                greenfield_header_unsupported: 1,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats::default(),
            OfdmStrongLongTrainingStats::default(),
        ))
        .unwrap_err();
        assert_eq!(
            overflow.to_string(),
            "OFDM rejection accounting overflow: reason counters cannot be summed as u64"
        );

        let saturated = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                signal_or_probe_rejected: u64::MAX,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats::default(),
            OfdmStrongLongTrainingStats::default(),
        ))
        .unwrap();
        assert_eq!(rejection_total(&saturated.rejections), Some(u64::MAX));

        let data_failure_mismatch = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: 36,
                ..DecoderStats::default()
            },
            reasons,
            OfdmDataFailureStats {
                legacy: 5,
                ..OfdmDataFailureStats::default()
            },
            strong_details,
        ))
        .unwrap_err();
        assert_eq!(
            data_failure_mismatch.to_string(),
            "OFDM data-failure accounting mismatch: detail total 5 does not equal data_decode_failed 6"
        );

        let data_failure_overflow = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                data_decode_failed: u64::MAX,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats {
                legacy: u64::MAX,
                ht_bcc: 1,
                ..OfdmDataFailureStats::default()
            },
            OfdmStrongLongTrainingStats::default(),
        ))
        .unwrap_err();
        assert_eq!(
            data_failure_overflow.to_string(),
            "OFDM data-failure accounting overflow: detail counters cannot be summed as u64"
        );

        let saturated_data_failures = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                data_decode_failed: u64::MAX,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats {
                legacy: u64::MAX,
                ..OfdmDataFailureStats::default()
            },
            OfdmStrongLongTrainingStats::default(),
        ))
        .unwrap();
        assert_eq!(
            data_failure_total(&saturated_data_failures.data_failures),
            Some(u64::MAX)
        );

        let detail_mismatch = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: 36,
                ..DecoderStats::default()
            },
            reasons,
            data_failure_details,
            OfdmStrongLongTrainingStats {
                strong_long_reference_not_found: 7,
                ..OfdmStrongLongTrainingStats::default()
            },
        ))
        .unwrap_err();
        assert_eq!(
            detail_mismatch.to_string(),
            "OFDM strong long-training accounting mismatch: detail total 7 does not equal strong_long_training_not_found 8"
        );

        let detail_overflow = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                strong_long_training_not_found: u64::MAX,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats::default(),
            OfdmStrongLongTrainingStats {
                strong_short_periodicity_timeout: u64::MAX,
                strong_long_repeat_not_found: 1,
                ..OfdmStrongLongTrainingStats::default()
            },
        ))
        .unwrap_err();
        assert_eq!(
            detail_overflow.to_string(),
            "OFDM strong long-training accounting overflow: detail counters cannot be summed as u64"
        );

        let saturated_details = OfdmStats::try_from((
            DecoderStats {
                rejected_frames: u64::MAX,
                ..DecoderStats::default()
            },
            OfdmRejectionStats {
                strong_long_training_not_found: u64::MAX,
                ..OfdmRejectionStats::default()
            },
            OfdmDataFailureStats::default(),
            OfdmStrongLongTrainingStats {
                strong_long_reference_not_found: u64::MAX,
                ..OfdmStrongLongTrainingStats::default()
            },
        ))
        .unwrap();
        assert_eq!(
            strong_long_training_total(&saturated_details.strong_long_training),
            Some(u64::MAX)
        );
    }

    #[test]
    fn bad_fcs_and_data_failure_have_distinct_closed_rejection_accounting() {
        let directory = TestDirectory::new();
        let invalid_service = directory.file("invalid-service.cs8");
        fs::write(&invalid_service, INVALID_SERVICE_IQ).unwrap();
        let invalid_summary = decode(
            &invalid_service,
            &directory.file("invalid-service.pcap"),
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
        )
        .unwrap();
        assert_eq!(invalid_summary.frames_written, 0);
        assert!(invalid_summary.ofdm.rejections.data_decode_failed > 0);
        assert_eq!(
            rejection_total(&invalid_summary.ofdm.rejections),
            Some(invalid_summary.ofdm.rejected_frames)
        );
        assert_eq!(
            data_failure_total(&invalid_summary.ofdm.data_failures),
            Some(invalid_summary.ofdm.rejections.data_decode_failed)
        );
        assert_eq!(
            invalid_summary.ofdm.data_failures.legacy,
            invalid_summary.ofdm.rejections.data_decode_failed
        );
        assert_eq!(invalid_summary.ofdm.data_failures.ht_bcc, 0);
        assert_eq!(invalid_summary.ofdm.data_failures.ht_ldpc, 0);
        assert_eq!(
            strong_long_training_total(&invalid_summary.ofdm.strong_long_training),
            Some(
                invalid_summary
                    .ofdm
                    .rejections
                    .strong_long_training_not_found
            )
        );

        let bad_fcs = directory.file("bad-fcs.cs8");
        fs::write(&bad_fcs, BAD_FCS_IQ).unwrap();
        let bad_fcs_summary = decode(
            &bad_fcs,
            &directory.file("bad-fcs.pcap"),
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
        )
        .unwrap();
        assert_eq!(bad_fcs_summary.frames_written, 0);
        assert!(bad_fcs_summary.ofdm.invalid_fcs > 0);
        assert_eq!(bad_fcs_summary.ofdm.rejections.data_decode_failed, 0);
        assert_eq!(
            rejection_total(&bad_fcs_summary.ofdm.rejections),
            Some(bad_fcs_summary.ofdm.rejected_frames)
        );
        assert_eq!(
            data_failure_total(&bad_fcs_summary.ofdm.data_failures),
            Some(bad_fcs_summary.ofdm.rejections.data_decode_failed)
        );
        assert_eq!(
            strong_long_training_total(&bad_fcs_summary.ofdm.strong_long_training),
            Some(
                bad_fcs_summary
                    .ofdm
                    .rejections
                    .strong_long_training_not_found
            )
        );
    }

    #[test]
    fn sc16_conversion_preserves_signed_bits_short_reads_and_rejects_bad_input() {
        struct Short {
            inner: Cursor<Vec<u8>>,
            interrupt_next: bool,
        }
        impl Read for Short {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.interrupt_next = !self.interrupt_next;
                if self.interrupt_next {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let limit = output.len().min(1);
                self.inner.read(&mut output[..limit])
            }
        }
        let values: [i16; 8] = [-2048, -2047, -17, -1, 0, 15, 16, 2047];
        let clipped = Cell::new(0);
        for (gain, expected) in [
            (None, [128, 128, 254, 255, 0, 0, 1, 127]),
            (Some(&clipped), [128, 128, 253, 255, 0, 1, 2, 127]),
        ] {
            let raw = values.into_iter().flat_map(i16::to_le_bytes).collect();
            let mut reader = Sc16Q11Reader::new(
                Short {
                    inner: Cursor::new(raw),
                    interrupt_next: false,
                },
                gain,
            );
            let mut converted = Vec::new();
            reader.read_to_end(&mut converted).unwrap();
            assert_eq!(converted, expected);
        }
        assert_eq!(clipped.get(), 3);
        assert!(
            Sc16Q11Reader::new(Cursor::new([1]), None)
                .read_to_end(&mut Vec::new())
                .is_err()
        );
        assert!(
            Sc16Q11Reader::new(Cursor::new(2048_i16.to_le_bytes()), Some(&clipped))
                .read_to_end(&mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn sc16_conversion_leaves_bytes_after_the_destination_and_block_limits_unread() {
        let mut raw = vec![0; CHUNK_SAMPLES * 4];
        raw.extend_from_slice(&2048_i16.to_le_bytes());
        let mut reader = Sc16Q11Reader::new(Cursor::new(raw), None);
        let mut output = vec![1; CHUNK_SAMPLES * 2];
        assert_eq!(reader.read(&mut output[..1]).unwrap(), 1);
        assert_eq!(reader.inner.position(), 2);

        reader.inner.set_position(0);
        output.push(1);
        assert_eq!(reader.read(&mut output).unwrap(), CHUNK_SAMPLES * 2);
        assert_eq!(reader.inner.position(), (CHUNK_SAMPLES * 4) as u64);
        assert!(output[..CHUNK_SAMPLES * 2].iter().all(|&byte| byte == 0));
        assert_eq!(output[CHUNK_SAMPLES * 2], 1);
        assert_eq!(
            reader.read(&mut output[..1]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn gain_receipt_counts_only_processed_q11_components() {
        let directory = TestDirectory::new();
        let input = directory.file("gain.sc16q11");
        let raw: Vec<_> = [1023_i16, -1024, 1024, -1025, 2047, -2048]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        fs::write(&input, raw).unwrap();
        let mut settings = DecodeSettings {
            double_q11: true,
            ..DecodeSettings::default()
        };
        for (shift_hz, name, filter) in [
            (0, "gain.pcap", None),
            (1_000_000, "gain-translated.pcap", None),
            (2_000_000, "gain-filtered.pcap", Some(antiwrap::FILTER_NAME)),
        ] {
            settings.capture_frequency_hz = (shift_hz > 0).then_some(2_412_000_000 + shift_hz);
            let output = directory.file(name);
            let summary = decode(
                &input,
                &output,
                2_412_000_000,
                IqFormat::Sc16Q11,
                2,
                settings,
            )
            .unwrap();
            assert_eq!(summary.input_samples, 3);
            assert_eq!(summary.processed_samples, 2);
            assert_eq!(summary.translation_filter, filter);
            assert_eq!(
                serde_json::to_value(summary).unwrap()["q11_gain"],
                serde_json::json!({"factor": 2, "clipped_components": 2}),
            );
        }
        let invalid = directory.file("gain-invalid.pcap");
        let error =
            decode(&input, &invalid, 2_412_000_000, IqFormat::Cs8, 2, settings).unwrap_err();
        assert!(error.to_string().contains("--double-q11 requires SC16 Q11"));
        assert!(!invalid.exists());
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
            DecodeSettings {
                time_origin_ns: Some(origin),
                ..DecodeSettings::default()
            },
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
        let summary = decode(
            &input,
            &output,
            2_412_000_000,
            IqFormat::Cs8,
            100,
            DecodeSettings::default(),
        )
        .unwrap();
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
                DecodeSettings::default()
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
                DecodeSettings::default()
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
    fn accounting_failure_after_successful_decode_does_not_publish_pcap() {
        let directory = TestDirectory::new();
        let input = directory.file("synthetic.cs8");
        fs::write(&input, IQ).unwrap();
        let rejection_output = directory.file("rejection-accounting-failed.pcap");

        let error = decode_with_ofdm_accounting(
            &input,
            &rejection_output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
            |mut stats, rejections, data_failures, strong_long_training| {
                assert_eq!(stats.valid_frames, 1);
                assert_eq!(rejections.checked_total(), Some(stats.rejected_frames));
                assert_eq!(
                    data_failures.checked_total(),
                    Some(rejections.data_decode_failed)
                );
                assert_eq!(
                    strong_long_training.checked_total(),
                    Some(rejections.strong_long_training_not_found)
                );
                stats.rejected_frames = stats.rejected_frames.checked_add(1).unwrap();
                (stats, rejections, data_failures, strong_long_training)
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("OFDM rejection accounting mismatch")
        );
        assert!(!rejection_output.exists());

        let data_failure_output = directory.file("data-failure-accounting-failed.pcap");
        let error = decode_with_ofdm_accounting(
            &input,
            &data_failure_output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
            |stats, rejections, mut data_failures, strong_long_training| {
                assert_eq!(stats.valid_frames, 1);
                assert_eq!(
                    data_failures.checked_total(),
                    Some(rejections.data_decode_failed)
                );
                data_failures.legacy += 1;
                (stats, rejections, data_failures, strong_long_training)
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OFDM data-failure accounting mismatch")
        );
        assert!(!data_failure_output.exists());

        let data_failure_overflow_output = directory.file("data-failure-overflow.pcap");
        let error = decode_with_ofdm_accounting(
            &input,
            &data_failure_overflow_output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
            |stats, rejections, mut data_failures, strong_long_training| {
                assert_eq!(stats.valid_frames, 1);
                data_failures.legacy = u64::MAX;
                data_failures.ht_bcc = 1;
                (stats, rejections, data_failures, strong_long_training)
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OFDM data-failure accounting overflow")
        );
        assert!(!data_failure_overflow_output.exists());

        let detail_output = directory.file("strong-detail-accounting-failed.pcap");
        let error = decode_with_ofdm_accounting(
            &input,
            &detail_output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
            |stats, rejections, data_failures, mut strong_long_training| {
                assert_eq!(stats.valid_frames, 1);
                assert_eq!(
                    strong_long_training.checked_total(),
                    Some(rejections.strong_long_training_not_found)
                );
                strong_long_training.strong_long_reference_not_found += 1;
                (stats, rejections, data_failures, strong_long_training)
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OFDM strong long-training accounting mismatch")
        );
        assert!(!detail_output.exists());

        let overflow_output = directory.file("strong-detail-overflow.pcap");
        let error = decode_with_ofdm_accounting(
            &input,
            &overflow_output,
            2_412_000_000,
            IqFormat::Cs8,
            10_000,
            DecodeSettings::default(),
            |stats, rejections, data_failures, mut strong_long_training| {
                assert_eq!(stats.valid_frames, 1);
                strong_long_training.strong_short_periodicity_timeout = u64::MAX;
                strong_long_training.strong_long_repeat_not_found = 1;
                (stats, rejections, data_failures, strong_long_training)
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OFDM strong long-training accounting overflow")
        );
        assert!(!overflow_output.exists());
        assert!(fs::read_dir(&directory.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".partial")
        }));
    }

    #[test]
    fn publication_requires_received_fcs_framing_and_independent_integrity() {
        use crafter::radio::FrameFraming;

        let directory = TestDirectory::new();
        let path = directory.file("untrusted.pcap");
        let mut destination = PcapOutput::new(&path, DecodeSettings::default()).unwrap();
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
