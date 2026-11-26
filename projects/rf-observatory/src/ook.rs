//! Replay amplitude-keyed sensor identifiers from saved IQ.

mod acurite;
mod envelope;
mod govee;
mod lacrosse;
mod oregon;

use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{Iq, Iq16, SensorChannel, SensorChannelPair};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::spectrum::CENTER_GUARD_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

/// Nominal center of the supported North American OOK sensor channel.
pub(crate) const DEFAULT_CHANNEL_HZ: u64 = 433_920_000;
/// Original-rate sweep samples in the conservative minimum 20 ms frame window.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = SAMPLE_RATE as u64 / 50;
/// Minimum planned dwell for one nominal OOK frame, including Atlas lightning.
pub(crate) const FRAME_DWELL_MS: u64 = 60;
/// Planned dwell with time for three Govee frame copies and their gaps.
pub(crate) const GOVEE_DWELL_MS: u64 = 250;
/// One-sided guard margin for the wider Oregon Scientific receive path.
pub(crate) const OREGON_HALF_WIDTH_HZ: u64 = 220_000;
/// Default maximum number of detailed records in an OOK report.
pub(crate) const DEFAULT_RETAINED_TRANSMISSIONS: usize = 256;
/// Largest supported OOK report retention cap.
pub(crate) const MAX_RETAINED_TRANSMISSIONS: usize = 4_096;
const IQ_BYTES: u64 = 4;
// Even the shortest admitted Oregon sync and body need over 60 ms of IQ.
const OREGON_MIN_CAPTURE_US: u64 = 60_000;
// A Tower repeat puts its stop pulse and four sync cells between payloads.
const TOWER_COPY_GAP_US: u64 = 10_000;

/// Measured high and following low durations in microseconds.
#[derive(Clone, Copy)]
struct Pulse {
    high_us: u32,
    low_us: u32,
}

/// An integrity-checked frame's protocol-local identity and copy marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "model")]
enum Reading {
    #[serde(rename = "acurite_iris")]
    Acurite {
        id: u16,
        channel: u8,
        message_type: u8,
        #[serde(skip)]
        sequence: u8,
    },
    #[serde(rename = "acurite_tower")]
    AcuriteTower {
        id: u16,
        channel: u8,
        #[serde(skip)]
        body: [u8; 7],
    },
    #[serde(rename = "acurite_atlas")]
    AcuriteAtlas {
        id: u16,
        channel: u8,
        message_type: u8,
        #[serde(skip)]
        sequence: u8,
    },
    #[serde(rename = "lacrosse_tx141th_bv2")]
    LaCrosse { id: u8, channel: u8 },
    #[serde(rename = "govee_h5054")]
    GoveeH5054(govee::H5054),
    #[serde(rename = "oregon_thgr122n")]
    OregonThgr122n(oregon::Thgr122n),
    #[serde(rename = "oregon_wgr800")]
    OregonWgr800(oregon::Wgr800),
}

impl Reading {
    fn without_sequence(mut self) -> Self {
        if let Self::Acurite { sequence, .. } | Self::AcuriteAtlas { sequence, .. } = &mut self {
            *sequence = 0;
        }
        self
    }

    fn sequence_bit(self) -> u8 {
        match self {
            Self::Acurite { sequence, .. } | Self::AcuriteAtlas { sequence, .. } => 1 << sequence,
            _ => 0,
        }
    }

    fn identifier_scope(self) -> &'static str {
        match self {
            Self::Acurite { .. } | Self::AcuriteTower { .. } | Self::AcuriteAtlas { .. } => {
                "protocol_sensor_id"
            }
            Self::GoveeH5054(reading) => {
                if reading.id.is_some() {
                    "protocol_sensor_id"
                } else {
                    "unattributed"
                }
            }
            Self::LaCrosse { .. } | Self::OregonThgr122n(_) | Self::OregonWgr800(_) => {
                "power_cycle_session"
            }
        }
    }
}

/// Select a narrow RF channel and a bounded prefix of a saved Q11 file.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved SC16 Q11 little-endian IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Nominal RF channel in hertz.
    #[arg(long, default_value_t = DEFAULT_CHANNEL_HZ)]
    channel_hz: u64,
    /// IQ rate in samples per second. Use 250 or 500 kSps for centered input.
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Maximum original samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// Maximum number of transmission records to retain.
    #[arg(long, default_value_t = DEFAULT_RETAINED_TRANSMISSIONS, value_parser = parse_retention)]
    max_transmissions: usize,
    /// New private JSON report in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// Parse a nonzero cap on reported transmission details.
///
/// # Errors
/// Rejects malformed, zero and excessive counts.
pub(crate) fn parse_retention(value: &str) -> Result<usize> {
    let count = value.parse()?;
    ensure!(
        (1..=MAX_RETAINED_TRANSMISSIONS).contains(&count),
        "retain 1..={MAX_RETAINED_TRANSMISSIONS} OOK transmissions"
    );
    Ok(count)
}

#[derive(Debug, Serialize)]
struct Transmission {
    first_input_sample: u64,
    end_input_sample_exclusive: u64,
    valid_copies: usize,
    identifier_scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    copy_sequence_mask: Option<u8>,
    #[serde(flatten)]
    reading: Reading,
}

/// Source-bound counts and bounded transmission evidence for one channel.
#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    envelope_sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    oversize_pulse_bursts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    oregon_oversize_pulse_bursts: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    acurite_oversize_pulse_bursts: Option<usize>,
    validated_frame_copies: usize,
    observed_transmissions: usize,
    omitted_transmissions: usize,
    transmissions: Vec<Transmission>,
}

impl Report {
    /// Number of bursts skipped because their pulse train exceeded the bound.
    pub(crate) fn oversize_pulse_bursts(&self) -> usize {
        self.oversize_pulse_bursts
    }

    /// Number of integrity-checked physical copies, including repeats.
    pub(crate) fn validated_frame_copies(&self) -> usize {
        self.validated_frame_copies
    }

    /// Number of grouped observations, including omitted detail.
    pub(crate) fn observed_transmissions(&self) -> usize {
        self.observed_transmissions
    }

    /// Number of detailed records retained in this report.
    pub(crate) fn retained_transmissions(&self) -> usize {
        self.transmissions.len()
    }
}

/// RF and sample-rate labels for one saved IQ channel.
#[derive(Clone, Copy)]
pub(crate) struct Channel {
    pub(crate) center_hz: u64,
    pub(crate) channel_hz: u64,
    pub(crate) sample_rate_hz: u32,
}

impl Channel {
    /// Whether the channel fits in the saved capture at its actual readback.
    pub(crate) fn is_covered(self) -> bool {
        self.covers(SensorChannel::HALF_WIDTH_HZ)
    }

    fn covers(self, width: u64) -> bool {
        let readback_range = (MIN_CENTER_HZ - ACQUISITION_CENTER_TOLERANCE_HZ)
            ..=(MAX_CENTER_HZ + ACQUISITION_CENTER_TOLERANCE_HZ);
        if !readback_range.contains(&self.center_hz) || self.channel_hz == 0 {
            return false;
        }
        let offset = self.center_hz.abs_diff(self.channel_hz);
        match self.sample_rate_hz {
            250_000 | 500_000 => offset == 0,
            1_000_000 | 2_000_000 | SAMPLE_RATE => {
                let outer = u64::from(self.sample_rate_hz) * 2 / 5;
                (CENTER_GUARD_HZ as u64 + width..=outer - width).contains(&offset)
            }
            _ => false,
        }
    }

    /// Analyze exactly a bounded Q11 prefix with independent burst state.
    ///
    /// # Errors
    /// Rejects invalid RF geometry or IQ, failed reads, and interruption.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_transmissions: usize,
        stop: &AtomicBool,
    ) -> Result<Report> {
        let mut stream = self.start(source_samples, sample_limit, max_transmissions)?;
        let digest = receiver::read_q11_prefix(
            input,
            stream.collection.analyzed_samples,
            stop,
            |sample, index| {
                stream.push(sample, index);
                Ok(())
            },
        )?;
        check_stop(stop)?;
        Ok(stream.finish(digest))
    }

    /// Prepare one channel to consume a bounded IQ prefix.
    ///
    /// # Errors
    /// Rejects invalid RF geometry or an excessive transmission cap.
    pub(crate) fn start(
        self,
        source_samples: u64,
        sample_limit: u64,
        max_transmissions: usize,
    ) -> Result<Stream> {
        ensure!(
            self.is_covered(),
            "OOK channel needs centered 250 or 500 kSps IQ, or guarded 1, 2 or 20 MSps IQ"
        );
        ensure!(
            max_transmissions <= MAX_RETAINED_TRANSMISSIONS,
            "invalid OOK retention cap"
        );
        let analyzed_samples = source_samples.min(sample_limit);
        let search_oregon =
            analyzed_samples >= u64::from(self.sample_rate_hz) * OREGON_MIN_CAPTURE_US / 1_000_000;
        let translator = Source::new(self, search_oregon)?;
        let envelope_sample_rate_hz = self.sample_rate_hz.min(SensorChannel::OUTPUT_RATE_HZ);
        let collection = Collection {
            stride: u64::from(self.sample_rate_hz / envelope_sample_rate_hz),
            tick_us: 1_000_000 / u64::from(envelope_sample_rate_hz),
            delay: translator.delay_input_samples(),
            analyzed_samples,
            limit: max_transmissions,
            copies: 0,
            count: 0,
            pending_tower: None,
            retained: Vec::new(),
        };
        Ok(Stream {
            channel: self,
            source_samples,
            envelope_sample_rate_hz,
            translator,
            detector: envelope::Detector::new(envelope_sample_rate_hz),
            peak_detector: envelope::Detector::with_peak_tracking(envelope_sample_rate_hz),
            govee: govee::Decoder::default(),
            search_oregon,
            collection,
        })
    }
}

/// Decoder state for one admitted channel prefix.
pub(crate) struct Stream {
    channel: Channel,
    source_samples: u64,
    envelope_sample_rate_hz: u32,
    translator: Source,
    detector: envelope::Detector,
    peak_detector: envelope::Detector,
    govee: govee::Decoder,
    search_oregon: bool,
    collection: Collection,
}

impl Stream {
    /// Consume one validated Q11 sample in source order.
    pub(crate) fn push(&mut self, sample: Iq16, _index: u64) {
        let Some((standard, oregon_iq)) = self.translator.push(sample) else {
            return;
        };
        self.detector.push(standard.power(), &mut |pulses, starts| {
            self.collection
                .standard_burst(&mut self.govee, pulses, starts);
        });
        self.peak_detector.push(
            oregon_iq.unwrap_or(standard).power(),
            &mut |pulses, starts| {
                self.collection.burst(
                    pulses,
                    starts,
                    Envelope::Peak {
                        search_oregon: self.search_oregon,
                    },
                );
            },
        );
    }

    /// Flush burst state at the end of the prefix and bind the source digest.
    pub(crate) fn finish(mut self, analyzed_iq_sha256: String) -> Report {
        self.detector.finish(&mut |pulses, starts| {
            self.collection
                .standard_burst(&mut self.govee, pulses, starts);
        });
        if let Some(packet) = self.govee.finish() {
            self.collection.govee_packet(packet);
        }
        self.peak_detector.finish(&mut |pulses, starts| {
            self.collection.burst(
                pulses,
                starts,
                Envelope::Peak {
                    search_oregon: self.search_oregon,
                },
            );
        });
        if let Some(tower) = self.collection.pending_tower.take() {
            self.collection.record(tower);
        }
        let peak_oversize = self.peak_detector.oversize_bursts();
        Report {
            schema_version: 1,
            operation: "identify_ook_in_saved_iq",
            sample_format: "SC16_Q11_LE",
            center_frequency_hz: self.channel.center_hz,
            channel_frequency_hz: self.channel.channel_hz,
            sample_rate_hz: self.channel.sample_rate_hz,
            envelope_sample_rate_hz: self.envelope_sample_rate_hz,
            source_samples: self.source_samples,
            analyzed_samples: self.collection.analyzed_samples,
            analyzed_iq_sha256,
            oversize_pulse_bursts: self.detector.oversize_bursts(),
            oregon_oversize_pulse_bursts: (self.search_oregon && peak_oversize != 0)
                .then_some(peak_oversize),
            acurite_oversize_pulse_bursts: (!self.search_oregon && peak_oversize != 0)
                .then_some(peak_oversize),
            validated_frame_copies: self.collection.copies,
            observed_transmissions: self.collection.count,
            omitted_transmissions: self.collection.count - self.collection.retained.len(),
            transmissions: self.collection.retained,
        }
    }
}

enum Source {
    Centered,
    Narrow(SensorChannel),
    Paired(SensorChannelPair),
}

impl Source {
    fn new(channel: Channel, search_oregon: bool) -> Result<Self> {
        if channel.sample_rate_hz <= SensorChannel::OUTPUT_RATE_HZ {
            return Ok(Self::Centered);
        }
        let offset = channel.channel_hz as f64 - channel.center_hz as f64;
        if search_oregon {
            let cutoff_hz = if channel.covers(OREGON_HALF_WIDTH_HZ) {
                OREGON_HALF_WIDTH_HZ
            } else {
                SensorChannel::HALF_WIDTH_HZ
            };
            SensorChannelPair::new(channel.sample_rate_hz, offset, cutoff_hz).map(Self::Paired)
        } else {
            SensorChannel::new(channel.sample_rate_hz, offset).map(Self::Narrow)
        }
    }

    fn delay_input_samples(&self) -> u64 {
        match self {
            Self::Centered => 0,
            Self::Narrow(filter) => filter.delay_input_samples(),
            Self::Paired(filter) => filter.delay_input_samples(),
        }
    }

    fn push(&mut self, sample: Iq16) -> Option<(Iq, Option<Iq>)> {
        match self {
            Self::Centered => Some((Iq::from(sample), None)),
            Self::Narrow(filter) => filter.push(sample).map(|iq| (iq, None)),
            Self::Paired(filter) => filter
                .push(sample)
                .map(|(standard, interpolated)| (standard, Some(interpolated))),
        }
    }
}

enum Envelope {
    Standard,
    Peak { search_oregon: bool },
}

struct Collection {
    stride: u64,
    tick_us: u64,
    delay: u64,
    analyzed_samples: u64,
    limit: usize,
    copies: usize,
    count: usize,
    pending_tower: Option<Transmission>,
    retained: Vec<Transmission>,
}

impl Collection {
    fn input_sample(&self, time_us: u64) -> u64 {
        ((time_us / self.tick_us + 1) * self.stride - 1)
            .saturating_sub(self.delay)
            .min(self.analyzed_samples)
    }

    fn standard_burst(&mut self, govee: &mut govee::Decoder, pulses: &[Pulse], starts: &[u64]) {
        self.burst(pulses, starts, Envelope::Standard);
        govee.push(pulses, starts, |packet| self.govee_packet(packet));
    }

    fn govee_packet(&mut self, packet: govee::Packet) {
        let reading = Reading::GoveeH5054(packet.reading);
        self.record(Transmission {
            first_input_sample: self.input_sample(packet.first_us),
            end_input_sample_exclusive: self.input_sample(packet.end_us),
            valid_copies: packet.copies,
            identifier_scope: reading.identifier_scope(),
            copy_sequence_mask: None,
            reading,
        });
    }

    fn burst(&mut self, pulses: &[Pulse], starts: &[u64], envelope: Envelope) {
        let mut transmissions: Vec<Transmission> = Vec::new();
        let mut emit = |reading: Reading, first: u64, end: u64| {
            let identity = reading.without_sequence();
            let sequences = reading.sequence_bit();
            if let Some(existing) = transmissions
                .iter_mut()
                .find(|entry| entry.reading == identity)
            {
                existing.first_input_sample = existing.first_input_sample.min(first);
                existing.end_input_sample_exclusive = existing.end_input_sample_exclusive.max(end);
                existing.valid_copies += 1;
                if let Some(mask) = &mut existing.copy_sequence_mask {
                    *mask |= sequences;
                }
            } else {
                transmissions.push(Transmission {
                    first_input_sample: first,
                    end_input_sample_exclusive: end,
                    valid_copies: 1,
                    identifier_scope: identity.identifier_scope(),
                    copy_sequence_mask: (sequences != 0).then_some(sequences),
                    reading: identity,
                });
            }
        };
        let mut emit_pulses = |reading, range: Range<usize>| {
            let first = self.input_sample(starts[range.start]);
            let last = range.end - 1;
            let end = self.input_sample(starts[last] + u64::from(pulses[last].high_us));
            emit(reading, first, end);
        };
        match envelope {
            Envelope::Standard => {
                acurite::decode(pulses, acurite::Models::Iris, &mut emit_pulses);
                lacrosse::decode(pulses, &mut emit_pulses);
            }
            Envelope::Peak { search_oregon } => {
                acurite::decode(pulses, acurite::Models::TowerAndAtlas, &mut emit_pulses);
                if search_oregon {
                    oregon::decode(pulses, starts, |reading, first_us, last_us| {
                        let first = self.input_sample(first_us);
                        let end = (self.input_sample(last_us) + 1).min(self.analyzed_samples);
                        emit(reading, first, end);
                    });
                }
            }
        }
        for entry in transmissions {
            if matches!(entry.reading, Reading::AcuriteTower { .. }) {
                self.group_tower(entry);
            } else {
                self.record(entry);
            }
        }
    }

    fn group_tower(&mut self, entry: Transmission) {
        // The intervening low can exceed the envelope detector's burst limit.
        let max_gap = TOWER_COPY_GAP_US / self.tick_us * self.stride;
        if let Some(previous) = &mut self.pending_tower
            && previous.reading == entry.reading
            && entry
                .first_input_sample
                .saturating_sub(previous.end_input_sample_exclusive)
                <= max_gap
        {
            previous.end_input_sample_exclusive = entry.end_input_sample_exclusive;
            previous.valid_copies += entry.valid_copies;
        } else if let Some(previous) = self.pending_tower.replace(entry) {
            self.record(previous);
        }
    }

    fn record(&mut self, entry: Transmission) {
        self.count += 1;
        self.copies += entry.valid_copies;
        // The envelope paths and repeat grouping can finish observations out of order.
        let index = self
            .retained
            .partition_point(|earlier| earlier.first_input_sample <= entry.first_input_sample);
        if index < self.limit {
            if self.retained.len() == self.limit {
                self.retained.pop();
            }
            self.retained.insert(index, entry);
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "OOK replay interrupted");
    Ok(())
}

/// Identify supported sensor frames and publish a nonclobber report.
///
/// # Errors
/// Returns an error for invalid or changed IQ, interruption or output failure.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let source = root.join(args.input);
    let output = root.join(args.output);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length == before.len() && length.is_multiple_of(IQ_BYTES),
        "OOK input needs complete SC16 Q11 I/Q pairs"
    );
    let report = Channel {
        center_hz: args.center_hz,
        channel_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
    }
    .replay(
        &mut input,
        length / IQ_BYTES,
        args.max_samples,
        args.max_transmissions,
        stop,
    )?;
    private_output::publish_json_file(&output, &report, || {
        let path_stamp = source.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&before, &path_stamp)
                && file_pin::same_file(&before, &input.metadata()?),
            "OOK IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Observed {} OOK transmissions. Output: {}",
        report.observed_transmissions(),
        output.display()
    );
    Ok(())
}
