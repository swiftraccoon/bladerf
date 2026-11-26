//! Replay broadcast RDS from labeled stations in saved SC16 Q11 IQ.

mod demod;
mod frame;
mod text;

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::{self, FixedBank, FmMultiplex, Iq16};
use crate::spectrum::USABLE_HALF_SPAN_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

const FM_BAND_HZ: std::ops::RangeInclusive<u64> = 64_000_000..=108_000_000;
/// Required receive span on either side of the broadcast FM center.
pub(crate) const FM_CHANNEL_HALF_WIDTH_HZ: u64 = 100_000;
const IQ_SAMPLE_BYTES: u64 = 4;
const MAX_RETAINED_GROUPS: usize = 512;
const BIT_RATE_TIMES_TWO: u64 = 2_375;
const GROUP_BITS: u64 = 104;
/// Original-rate samples for the nominal extent of one complete RDS group.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    (2 * GROUP_BITS * SAMPLE_RATE as u64).div_ceil(BIT_RATE_TIMES_TWO);
// Four 1,187.5 bit/s periods on the original-rate grid. A new group is 104 bits.
const DUPLICATE_WINDOW_SAMPLES: u64 = (8 * SAMPLE_RATE as u64).div_ceil(BIT_RATE_TIMES_TWO);

/// Select a broadcast FM station in a saved 20 MSps capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ at 20 MSps.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Broadcast FM station center in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// Maximum original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Clone, Copy, Serialize)]
struct Observation {
    decision_input_sample: u64,
    blocks: [u16; 4],
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    multiplex_sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    #[serde(flatten)]
    evidence: GroupEvidence,
}

#[derive(Serialize)]
struct GroupEvidence {
    validated_groups: usize,
    omitted_valid_groups: usize,
    groups: Vec<Observation>,
    text: Vec<text::Observation>,
}

#[derive(Serialize)]
struct StationReport {
    channel_frequency_hz: u64,
    #[serde(flatten)]
    evidence: GroupEvidence,
}

/// One source-prefix digest and ordered evidence from each labeled station.
#[derive(Serialize)]
pub(crate) struct BankReport {
    analyzed_iq_sha256: String,
    stations: Vec<StationReport>,
}

impl BankReport {
    /// Number of validated groups across stations, including omitted groups.
    pub(crate) fn validated_groups(&self) -> usize {
        self.stations
            .iter()
            .map(|station| station.evidence.validated_groups)
            .sum()
    }

    /// Number of stored group records across stations.
    pub(crate) fn retained_groups(&self) -> usize {
        self.stations
            .iter()
            .map(|station| station.evidence.groups.len())
            .sum()
    }
}

/// Whether the proposed station center lies in the supported broadcast band.
pub(crate) fn is_fm_channel_hz(channel_hz: u64) -> bool {
    FM_BAND_HZ.contains(&channel_hz)
}

/// Whether the station and its required RF margin fit in the 20 MSps capture.
pub(crate) fn is_covered(center_hz: u64, channel_hz: u64) -> bool {
    let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
    (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&center_hz)
        && is_fm_channel_hz(channel_hz)
        && center_hz.abs_diff(channel_hz) <= USABLE_HALF_SPAN_HZ as u64 - FM_CHANNEL_HALF_WIDTH_HZ
}

fn check_channel(center_hz: u64, channel_hz: u64) -> Result<()> {
    ensure!(
        is_covered(center_hz, channel_hz),
        "RDS requires a 64..=108 MHz FM station with a 100 kHz margin inside the central 80% of the capture"
    );
    Ok(())
}

struct ChannelBank {
    bank: FixedBank,
    stations: Vec<ChannelDecoder>,
}

impl ChannelBank {
    fn new(center_hz: u64, channels: &[u64]) -> Result<Self> {
        let mut words = [0; dsp::MAX_CHANNELS];
        for (word, &channel_hz) in words.iter_mut().zip(channels) {
            check_channel(center_hz, channel_hz)?;
            *word = dsp::tuning_word(channel_hz as f64 - center_hz as f64, SAMPLE_RATE)?;
        }
        let bank = FixedBank::new(&words[..channels.len()])?;
        let stations = channels.iter().copied().map(ChannelDecoder::new).collect();
        Ok(Self { bank, stations })
    }

    fn push(&mut self, input: Iq16, index: u64, remaining_groups: &mut usize) {
        if let Some(cic) = self.bank.push(input) {
            for (station, sample) in self.stations.iter_mut().zip(cic.samples) {
                station.push(sample, index, remaining_groups);
            }
        }
    }
}

struct ChannelDecoder {
    channel_frequency_hz: u64,
    fm: FmMultiplex,
    demod: demod::Demodulator,
    framing: [frame::Decoder; demod::LANES],
    recent: VecDeque<Observation>,
    groups: Vec<Observation>,
    omitted_valid_groups: usize,
}

impl ChannelDecoder {
    fn new(channel_frequency_hz: u64) -> Self {
        Self {
            channel_frequency_hz,
            fm: FmMultiplex::new(),
            demod: demod::Demodulator::default(),
            framing: std::array::from_fn(|_| frame::Decoder::default()),
            recent: VecDeque::new(),
            groups: Vec::new(),
            omitted_valid_groups: 0,
        }
    }

    fn push(&mut self, input: Iq16, index: u64, remaining_groups: &mut usize) {
        let Some(multiplex) = self.fm.push(input) else {
            return;
        };
        for (framing, bit) in self.framing.iter_mut().zip(self.demod.push(multiplex)) {
            let Some(blocks) = bit.and_then(|bit| framing.push(bit)) else {
                continue;
            };
            // Timing lanes can finish the same group on different samples.
            while self.recent.front().is_some_and(|earlier| {
                index - earlier.decision_input_sample > DUPLICATE_WINDOW_SAMPLES
            }) {
                self.recent.pop_front();
            }
            if self.recent.iter().any(|earlier| earlier.blocks == blocks) {
                continue;
            }
            let observation = Observation {
                decision_input_sample: index,
                blocks,
            };
            self.recent.push_back(observation);
            if self.groups.len() < MAX_RETAINED_GROUPS && *remaining_groups > 0 {
                self.groups.push(observation);
                *remaining_groups -= 1;
            } else {
                self.omitted_valid_groups += 1;
            }
        }
    }

    fn finish(self) -> StationReport {
        let text = text::decode(&self.groups);
        StationReport {
            channel_frequency_hz: self.channel_frequency_hz,
            evidence: GroupEvidence {
                validated_groups: self.groups.len() + self.omitted_valid_groups,
                omitted_valid_groups: self.omitted_valid_groups,
                groups: self.groups,
                text,
            },
        }
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "RDS replay interrupted");
    Ok(())
}

/// Replay channels in the supplied order from an admitted reader at sample zero.
/// Groups are retained in decision order, with ties following station order.
/// Text is assembled independently from each station's retained groups.
/// The caller verifies the source descriptor and paths before publication.
///
/// # Errors
/// Returns an error for an invalid extent or channel geometry, invalid IQ, or interruption.
pub(crate) fn replay(
    input: &mut dyn Read,
    center_hz: u64,
    analyzed_samples: u64,
    channels: &[u64],
    max_group_records: usize,
    stop: &AtomicBool,
) -> Result<BankReport> {
    check_stop(stop)?;
    ensure!(
        !channels.is_empty(),
        "select at least one broadcast FM station"
    );
    ensure!(
        (1..=MAX_SAMPLES).contains(&analyzed_samples),
        "RDS replay prefix is outside the supported IQ extent"
    );
    let mut banks = channels
        .chunks(dsp::MAX_CHANNELS)
        .map(|group| ChannelBank::new(center_hz, group))
        .collect::<Result<Vec<_>>>()?;
    let mut remaining_groups = max_group_records;
    let analyzed_iq_sha256 =
        receiver::read_q11_prefix(input, analyzed_samples, stop, |sample, index| {
            for bank in &mut banks {
                bank.push(sample, index, &mut remaining_groups);
            }
            Ok(())
        })?;
    check_stop(stop)?;
    let stations = banks
        .into_iter()
        .flat_map(|bank| bank.stations.into_iter().map(ChannelDecoder::finish))
        .collect();
    Ok(BankReport {
        analyzed_iq_sha256,
        stations,
    })
}

/// Decode one labeled broadcast station and publish its source-bound report.
///
/// # Errors
/// Rejects invalid RF geometry, incomplete or invalid IQ, changed input,
/// interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    check_channel(args.center_hz, args.channel_hz)?;
    let source = root.join(args.input);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
    let before = input.metadata()?;
    ensure!(
        length == before.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
        "RDS input needs a stable extent of complete I/Q pairs"
    );
    let source_samples = length / IQ_SAMPLE_BYTES;
    let analyzed_samples = source_samples.min(args.max_samples);
    let BankReport {
        analyzed_iq_sha256,
        mut stations,
    } = replay(
        &mut input,
        args.center_hz,
        analyzed_samples,
        &[args.channel_hz],
        MAX_RETAINED_GROUPS,
        stop,
    )?;
    let station = stations.pop().context("missing RDS station evidence")?;
    let report = Report {
        schema_version: 1,
        operation: "identify_rds_in_saved_iq",
        sample_format: "SC16_Q11_LE",
        center_frequency_hz: args.center_hz,
        channel_frequency_hz: station.channel_frequency_hz,
        sample_rate_hz: SAMPLE_RATE,
        multiplex_sample_rate_hz: dsp::FM_MULTIPLEX_RATE_HZ,
        source_samples,
        analyzed_samples,
        analyzed_iq_sha256,
        evidence: station.evidence,
    };
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        let path_stamp = source.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&before, &path_stamp)
                && file_pin::same_file(&before, &input.metadata()?),
            "RDS IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Validated {} RDS groups. Output: {}",
        report.evidence.validated_groups,
        output.display()
    );
    Ok(())
}
