//! Replay International Morse on labeled RF channels in saved Q11 IQ.

mod envelope;
mod timing;

use std::collections::{BTreeMap, btree_map::Entry};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::Serialize;

use crate::dsp::Iq16;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

const TARGETED_RATE_HZ: u32 = 2_000_000;
/// Required passband margin on either side of a Morse channel center.
pub(crate) const CHANNEL_MARGIN_HZ: u64 = 12_000;
const DEFAULT_TONE_HZ: u32 = 750;
const IQ_SAMPLE_BYTES: u64 = 4;
const TONE_RANGE_HZ: std::ops::RangeInclusive<u32> = 300..=2_000;
const PROFILE_SYNTAX: &str = "expected cw:CHANNEL_HZ[:WPM] or nfm:CHANNEL_HZ:TONE_HZ[:WPM]";
/// Original-rate samples for the minimum 96-sample Morse timing envelope.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    timing::MIN_ENVELOPE_SAMPLES as u64 * (SAMPLE_RATE / envelope::RATE_HZ) as u64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum Mode {
    /// Detect an interrupted RF carrier near the channel center.
    Cw,
    /// Demodulate narrowband FM and detect the selected audio tone.
    Nfm,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Channel {
    channel_frequency_hz: u64,
    mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    tone_hz: Option<u32>,
}

/// A labeled Morse RF channel and an optional fixed character speed.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Profile {
    #[serde(flatten)]
    channel: Channel,
    requested_wpm: Option<u32>,
}

impl Profile {
    /// Parse `cw:CHANNEL_HZ[:WPM]` or `nfm:CHANNEL_HZ:TONE_HZ[:WPM]`.
    ///
    /// # Errors
    /// Rejects incorrect field counts, unsupported mode, tone or speed, and zero frequency.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let mut fields = value.split(':');
        let mode = match fields.next() {
            Some("cw") => Mode::Cw,
            Some("nfm") => Mode::Nfm,
            _ => bail!("{PROFILE_SYNTAX}"),
        };
        let channel_frequency_hz = fields
            .next()
            .context(PROFILE_SYNTAX)?
            .parse()
            .context("invalid Morse channel frequency in hertz")?;
        ensure!(
            channel_frequency_hz > 0,
            "Morse channel frequency must be positive"
        );
        let tone_hz = if mode == Mode::Nfm {
            let tone_hz = fields
                .next()
                .context(PROFILE_SYNTAX)?
                .parse()
                .context("invalid Morse audio tone in hertz")?;
            ensure!(
                TONE_RANGE_HZ.contains(&tone_hz),
                "Morse audio tone must be 300..=2000 Hz"
            );
            Some(tone_hz)
        } else {
            None
        };
        let requested_wpm = fields
            .next()
            .map(|field| field.parse().context("invalid Morse character speed"))
            .transpose()?;
        ensure!(fields.next().is_none(), "{PROFILE_SYNTAX}");
        ensure!(
            requested_wpm.is_none_or(|wpm| timing::WPM_RANGE.contains(&wpm)),
            "Morse character speed must be 8..=40 WPM"
        );
        Ok(Self {
            channel: Channel {
                channel_frequency_hz,
                mode,
                tone_hz,
            },
            requested_wpm,
        })
    }

    /// Check the required channel margin against an observed 20 MSps center.
    pub(crate) fn is_covered(self, center_hz: u64) -> bool {
        is_covered(center_hz, self.channel.channel_frequency_hz, SAMPLE_RATE)
    }

    /// Return the RF center in hertz for this keying profile.
    pub(crate) fn channel_frequency_hz(self) -> u64 {
        self.channel.channel_frequency_hz
    }
}

/// Select a keyed RF carrier or an FM audio tone in a saved capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Morse channel center in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// Actual acquisition rate in samples per second (2 or 20 MSps).
    #[arg(long, default_value_t = SAMPLE_RATE)]
    sample_rate_hz: u32,
    /// Modulation used to carry Morse.
    #[arg(long, value_enum, default_value = "cw")]
    mode: Mode,
    /// Audio tone in hertz for NFM only (default 750 Hz).
    #[arg(long, value_parser = clap::value_parser!(u32).range(300..=2_000))]
    tone_hz: Option<u32>,
    /// Character speed in words per minute, or estimate it when omitted.
    #[arg(long, value_parser = clap::value_parser!(u32).range(8..=40))]
    wpm: Option<u32>,
    /// Maximum original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    tone_hz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_wpm: Option<u32>,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    envelope_sample_rate_hz: u32,
    envelope_samples: usize,
    #[serde(flatten)]
    decoded: timing::Decoded,
}

#[derive(Serialize)]
struct ProfileReport {
    #[serde(flatten)]
    profile: Profile,
    envelope_sample_rate_hz: u32,
    envelope_samples: usize,
    #[serde(flatten)]
    decoded: timing::Decoded,
}

/// Evidence from one validated IQ prefix, in the supplied profile order.
#[derive(Serialize)]
pub(crate) struct BankReport {
    analyzed_iq_sha256: String,
    envelope_count: usize,
    profiles: Vec<ProfileReport>,
}

impl BankReport {
    /// Return recognized characters across profiles, including omitted observations.
    pub(crate) fn resolved_characters(&self) -> usize {
        self.profiles
            .iter()
            .map(|profile| profile.decoded.resolved_characters)
            .sum()
    }
}

struct Envelope {
    detector: envelope::Detector,
    power: Vec<f64>,
}

impl Envelope {
    fn new(
        channel: Channel,
        center_hz: u64,
        sample_rate_hz: u32,
        analyzed_samples: u64,
    ) -> Result<Self> {
        check_channel(center_hz, channel.channel_frequency_hz, sample_rate_hz)?;
        let offset_hz = channel.channel_frequency_hz as f64 - center_hz as f64;
        let detector = envelope::Detector::new(sample_rate_hz, offset_hz, channel.tone_hz)?;
        let stride = u64::from(sample_rate_hz / envelope::RATE_HZ);
        Ok(Self {
            detector,
            power: Vec::with_capacity((analyzed_samples / stride) as usize),
        })
    }

    fn push(&mut self, sample: Iq16) {
        self.power.extend(self.detector.push(sample));
    }
}

fn is_covered(center_hz: u64, channel_hz: u64, sample_rate_hz: u32) -> bool {
    let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
    let offset_limit = (u64::from(sample_rate_hz) * 2 / 5).saturating_sub(CHANNEL_MARGIN_HZ);
    (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&center_hz)
        && channel_hz > 0
        && center_hz.abs_diff(channel_hz) <= offset_limit
}

fn check_channel(center_hz: u64, channel_hz: u64, sample_rate_hz: u32) -> Result<()> {
    ensure!(
        is_covered(center_hz, channel_hz, sample_rate_hz),
        "Morse channel requires {CHANNEL_MARGIN_HZ} Hz of margin inside the central 80% of the capture"
    );
    Ok(())
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "Morse replay interrupted");
    Ok(())
}

/// Replay profiles using one 20 MSps IQ prefix read from an admitted reader
/// positioned at sample zero.
///
/// Frequency, mode and tone select the shared envelope. Each profile applies
/// its own timing and text reconstruction to that envelope. The caller checks
/// the source identity again before publication.
///
/// # Errors
/// Rejects missing profiles, an invalid extent, unsupported geometry, invalid IQ
/// or interruption.
pub(crate) fn replay(
    input: &mut dyn Read,
    center_hz: u64,
    analyzed_samples: u64,
    profiles: &[Profile],
    stop: &AtomicBool,
) -> Result<BankReport> {
    replay_at_rate(
        input,
        center_hz,
        SAMPLE_RATE,
        analyzed_samples,
        profiles,
        stop,
    )
}

fn replay_at_rate(
    input: &mut dyn Read,
    center_hz: u64,
    sample_rate_hz: u32,
    analyzed_samples: u64,
    profiles: &[Profile],
    stop: &AtomicBool,
) -> Result<BankReport> {
    check_stop(stop)?;
    ensure!(!profiles.is_empty(), "select at least one Morse profile");
    ensure!(
        (1..=MAX_SAMPLES).contains(&analyzed_samples),
        "Morse replay prefix is outside the supported IQ extent"
    );
    let mut envelope_by_channel = BTreeMap::new();
    let mut envelopes = Vec::new();
    let mut profile_envelopes = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let index = match envelope_by_channel.entry(profile.channel) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let index = envelopes.len();
                envelopes.push(Envelope::new(
                    profile.channel,
                    center_hz,
                    sample_rate_hz,
                    analyzed_samples,
                )?);
                entry.insert(index);
                index
            }
        };
        profile_envelopes.push(index);
    }
    let analyzed_iq_sha256 =
        receiver::read_q11_prefix(input, analyzed_samples, stop, |sample, _| {
            for envelope in &mut envelopes {
                envelope.push(sample);
            }
            Ok(())
        })?;
    let mut reports = Vec::with_capacity(profiles.len());
    for (&profile, index) in profiles.iter().zip(profile_envelopes) {
        check_stop(stop)?;
        let power = &envelopes[index].power;
        reports.push(ProfileReport {
            profile,
            envelope_sample_rate_hz: envelope::RATE_HZ,
            envelope_samples: power.len(),
            decoded: timing::decode(power, profile.requested_wpm),
        });
    }
    check_stop(stop)?;
    Ok(BankReport {
        analyzed_iq_sha256,
        envelope_count: envelopes.len(),
        profiles: reports,
    })
}

/// Recover timed Morse markings and publish a source-bound text report.
///
/// # Errors
/// Returns an error for invalid RF labels or modes, incomplete or invalid IQ,
/// changed input, interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    ensure!(
        [TARGETED_RATE_HZ, SAMPLE_RATE].contains(&args.sample_rate_hz),
        "Morse replay requires 2 or 20 MSps IQ input"
    );
    check_channel(args.center_hz, args.channel_hz, args.sample_rate_hz)?;
    ensure!(
        args.mode == Mode::Nfm || args.tone_hz.is_none(),
        "--tone-hz requires --mode nfm"
    );
    let profile = Profile {
        channel: Channel {
            channel_frequency_hz: args.channel_hz,
            mode: args.mode,
            tone_hz: (args.mode == Mode::Nfm).then_some(args.tone_hz.unwrap_or(DEFAULT_TONE_HZ)),
        },
        requested_wpm: args.wpm,
    };
    let source = root.join(args.input);
    let (mut input, length) = file_pin::open_regular(&source, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
    let stamp = input.metadata()?;
    ensure!(
        length == stamp.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
        "Morse input needs a stable extent of complete I/Q pairs"
    );
    let source_samples = length / IQ_SAMPLE_BYTES;
    let analyzed_samples = source_samples.min(args.max_samples);
    let BankReport {
        analyzed_iq_sha256,
        mut profiles,
        ..
    } = replay_at_rate(
        &mut input,
        args.center_hz,
        args.sample_rate_hz,
        analyzed_samples,
        &[profile],
        stop,
    )?;
    let result = profiles.pop().context("missing Morse profile evidence")?;
    let report = Report {
        schema_version: 1,
        operation: "identify_morse_in_saved_iq",
        sample_format: "SC16_Q11_LE",
        mode: args.mode,
        tone_hz: profile.channel.tone_hz,
        requested_wpm: args.wpm,
        center_frequency_hz: args.center_hz,
        channel_frequency_hz: args.channel_hz,
        sample_rate_hz: args.sample_rate_hz,
        source_samples,
        analyzed_samples,
        analyzed_iq_sha256,
        envelope_sample_rate_hz: result.envelope_sample_rate_hz,
        envelope_samples: result.envelope_samples,
        decoded: result.decoded,
    };
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        let current = source.symlink_metadata()?;
        ensure!(
            current.is_file()
                && file_pin::same_file(&stamp, &current)
                && file_pin::same_file(&stamp, &input.metadata()?),
            "Morse IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    println!(
        "Resolved {} Morse characters. Output: {}",
        report.decoded.resolved_characters,
        output.display()
    );
    Ok(())
}
