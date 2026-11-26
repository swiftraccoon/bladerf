//! Share a saved IQ read and channelizers across LoRa preamble probes.

use std::io::Read;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use rustfft::num_complex::Complex32;
use serde::Serialize;

use crate::dsp::ChirpChannel;
use crate::{MAX_SAMPLES, SAMPLE_RATE, receiver};

use super::{ChannelArgs, check_stop, preamble};

/// A labeled LoRa channel center, bandwidth and spreading factor.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Profile {
    channel_hz: u64,
    bandwidth_hz: u32,
    spreading_factor: u8,
}

impl Profile {
    /// Read a `CENTER_HZ:BANDWIDTH_HZ:SF` profile.
    ///
    /// # Errors
    /// Rejects missing, nonnumeric or unsupported settings.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (center, rest) = value
            .split_once(':')
            .context("expected a LoRa profile as CENTER_HZ:BANDWIDTH_HZ:SF")?;
        let (bandwidth, factor) = rest
            .split_once(':')
            .context("expected a LoRa profile as CENTER_HZ:BANDWIDTH_HZ:SF")?;
        let profile = Self {
            channel_hz: center
                .parse()
                .context("invalid LoRa channel center in hertz")?,
            bandwidth_hz: bandwidth
                .parse()
                .context("invalid LoRa bandwidth in hertz")?,
            spreading_factor: factor.parse().context("invalid LoRa spreading factor")?,
        };
        ensure!(
            profile.is_supported(),
            "LoRa profiles require a positive channel center, 125, 250 or 500 kHz bandwidth, and spreading factor 7 through 12"
        );
        Ok(profile)
    }

    fn is_supported(self) -> bool {
        self.channel_hz > 0
            && [125_000, 250_000, 500_000].contains(&self.bandwidth_hz)
            && (7..=12).contains(&self.spreading_factor)
    }

    /// Return the requested channel center in hertz.
    pub(super) fn channel_hz(self) -> u64 {
        self.channel_hz
    }

    /// Return the half-open RF passband, if both bounds fit in integer hertz.
    pub(crate) fn rf_bounds_hz(self) -> Option<std::ops::Range<u64>> {
        let half_bandwidth_hz = u64::from(self.bandwidth_hz) / 2;
        Some(
            self.channel_hz.checked_sub(half_bandwidth_hz)?
                ..self.channel_hz.checked_add(half_bandwidth_hz)?,
        )
    }

    /// Set the observed acquisition center for a saved 20 MSps dwell.
    pub(super) fn settings(self, center_hz: u64) -> ChannelArgs {
        ChannelArgs {
            center_hz,
            channel_hz: self.channel_hz,
            sample_rate_hz: SAMPLE_RATE,
            bandwidth_hz: self.bandwidth_hz,
            spreading_factor: self.spreading_factor,
        }
    }

    /// Check the profile's width against the scalar probe geometry at 20 MSps.
    pub(crate) fn is_covered(self, center_hz: u64) -> bool {
        self.settings(center_hz).offset_hz().is_ok()
    }

    /// Number of input samples for the first searchable extent at 20 MSps.
    pub(crate) fn minimum_samples(self) -> u64 {
        (preamble::MIN_UP_WINDOWS + 1) as u64 * self.samples_per_symbol()
    }

    /// Return the number of original samples per LoRa symbol at 20 MSps.
    pub(super) fn samples_per_symbol(self) -> u64 {
        (1_u64 << self.spreading_factor) * u64::from(SAMPLE_RATE / self.bandwidth_hz)
    }
}

/// One profile's detector and bounded record storage.
pub(super) struct ProfileProbe {
    chips_per_symbol: usize,
    detector: preamble::Detector,
    candidate_count: usize,
    candidates: Vec<preamble::Detection>,
}

impl ProfileProbe {
    /// Start a probe for a supported spreading factor from 7 through 12.
    pub(super) fn new(spreading_factor: u8) -> Self {
        let chips_per_symbol = 1 << spreading_factor;
        Self {
            chips_per_symbol,
            detector: preamble::Detector::new(chips_per_symbol),
            candidate_count: 0,
            candidates: Vec::new(),
        }
    }

    /// Advance the detector by one chip and spend capacity only for a retained record.
    pub(super) fn push(&mut self, chip: Complex32, remaining_records: &mut usize) {
        if let Some(candidate) = self.detector.push(chip) {
            self.candidate_count += 1;
            if *remaining_records > 0 {
                self.candidates.push(candidate);
                *remaining_records -= 1;
            }
        }
    }

    /// Return evidence including detector windows and nominal channel delay.
    pub(super) fn finish(self, channel: &ChirpChannel) -> ProbeEvidence {
        let processed_windows = self.detector.processed_windows();
        ProbeEvidence {
            chips_per_symbol: self.chips_per_symbol,
            source_samples_per_window: self.chips_per_symbol * channel.decimation(),
            channel_delay_input_samples: channel.group_delay_input_samples(),
            processed_windows,
            minimum_up_windows: preamble::MIN_UP_WINDOWS,
            minimum_peak_to_mean: preamble::MIN_PEAK_TO_MEAN,
            minimum_peak_fraction: self.detector.minimum_peak_fraction(),
            delimiter_search_windows: preamble::DELIMITER_SEARCH_WINDOWS,
            sufficient_extent_for_search: processed_windows > preamble::MIN_UP_WINDOWS,
            candidate_count: self.candidate_count,
            omitted_candidates: self.candidate_count - self.candidates.len(),
            candidates: self.candidates,
        }
    }
}

/// Preamble evidence shared by scalar and bank replays.
#[derive(Serialize)]
pub(super) struct ProbeEvidence {
    chips_per_symbol: usize,
    source_samples_per_window: usize,
    channel_delay_input_samples: f64,
    processed_windows: usize,
    minimum_up_windows: usize,
    minimum_peak_to_mean: f32,
    minimum_peak_fraction: f32,
    delimiter_search_windows: usize,
    sufficient_extent_for_search: bool,
    candidate_count: usize,
    omitted_candidates: usize,
    candidates: Vec<preamble::Detection>,
}

impl ProbeEvidence {
    /// Number of preamble candidates, including omitted records.
    pub(super) fn candidate_count(&self) -> usize {
        self.candidate_count
    }
}

/// Preamble evidence labeled with its requested profile.
#[derive(Serialize)]
struct ProfileReport {
    #[serde(flatten)]
    profile: Profile,
    #[serde(flatten)]
    evidence: ProbeEvidence,
}

/// Shared source digest and independently measured profile evidence.
#[derive(Serialize)]
pub(crate) struct BankReport {
    analyzed_iq_sha256: String,
    /// Number of distinct center and bandwidth pairs sent through channelizers.
    channelizer_count: usize,
    profiles: Vec<ProfileReport>,
}

impl BankReport {
    /// Number of preamble candidates across profiles, including omissions.
    pub(crate) fn candidate_count(&self) -> usize {
        self.profiles
            .iter()
            .map(|profile| profile.evidence.candidate_count())
            .sum()
    }

    /// Number of candidate records retained across all profiles.
    pub(crate) fn retained_candidates(&self) -> usize {
        self.profiles
            .iter()
            .map(|profile| profile.evidence.candidates.len())
            .sum()
    }
}

struct ChannelProbes {
    channel: ChirpChannel,
    probes: Vec<(Profile, ProfileProbe)>,
}

/// Read and hash a 20 MSps prefix from an admitted reader at sample zero.
/// Profiles must be supported, covered, sorted and distinct. The caller
/// rechecks the source descriptor and paths after replay. Records use detection
/// order, with profile order for simultaneous decisions.
///
/// # Errors
/// Rejects invalid extent or profile settings, invalid IQ, and interruption.
pub(crate) fn replay(
    input: &mut dyn Read,
    center_hz: u64,
    selected_samples: u64,
    profiles: &[Profile],
    max_candidate_records: usize,
    stop: &AtomicBool,
) -> Result<BankReport> {
    check_stop(stop)?;
    ensure!(
        (1..=MAX_SAMPLES).contains(&selected_samples),
        "LoRa replay prefix is outside the supported IQ extent"
    );
    ensure!(
        !profiles.is_empty()
            && profiles.is_sorted_by(|left, right| left < right)
            && profiles.iter().all(|profile| profile.is_covered(center_hz)),
        "LoRa replay requires sorted, distinct profiles inside the sampled band"
    );
    let mut channels = profiles
        .chunk_by(|left, right| {
            left.channel_hz == right.channel_hz && left.bandwidth_hz == right.bandwidth_hz
        })
        .map(|group| {
            let profile = group[0];
            let channel = ChirpChannel::new(
                SAMPLE_RATE,
                profile.settings(center_hz).offset_hz()?,
                profile.bandwidth_hz,
            )?;
            let probes = group
                .iter()
                .map(|&profile| (profile, ProfileProbe::new(profile.spreading_factor)))
                .collect();
            Ok(ChannelProbes { channel, probes })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut remaining_records = max_candidate_records;
    let analyzed_iq_sha256 =
        receiver::read_q11_prefix(input, selected_samples, stop, |sample, _| {
            for group in &mut channels {
                if let Some(chip) = group.channel.push(sample) {
                    let iq = Complex32::new(chip.i as f32, chip.q as f32);
                    for (_, probe) in &mut group.probes {
                        probe.push(iq, &mut remaining_records);
                    }
                }
            }
            Ok(())
        })?;
    check_stop(stop)?;
    let channelizer_count = channels.len();
    let profiles = channels
        .into_iter()
        .flat_map(|group| {
            group
                .probes
                .into_iter()
                .map(move |(profile, probe)| ProfileReport {
                    profile,
                    evidence: probe.finish(&group.channel),
                })
        })
        .collect();
    Ok(BankReport {
        analyzed_iq_sha256,
        channelizer_count,
        profiles,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Seek;

    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn shares_only_matching_channels_and_keeps_each_profiles_windows() {
        let profiles = [
            "906875000:250000:7",
            "906875000:500000:7",
            "906875000:500000:8",
        ]
        .map(|profile| Profile::parse(profile).unwrap());
        assert_eq!(
            profiles.map(Profile::minimum_samples),
            [61_440, 30_720, 61_440]
        );
        assert!(Profile::parse("906875000:500000:8:1").is_err());
        assert!(Profile::parse("906875000:62500:8").is_err());
        assert!(Profile::parse("906875000:500000:6").is_err());

        let selected_samples = 61_453;
        let mut input = tempfile::tempfile().unwrap();
        input.set_len(4 * (selected_samples + 7)).unwrap();
        let stop = AtomicBool::new(false);
        let report = replay(
            &mut input,
            907_100_000,
            selected_samples,
            &profiles,
            0,
            &stop,
        )
        .unwrap();
        assert_eq!(report.channelizer_count, 2);
        assert_eq!(report.candidate_count(), 0);
        assert_eq!(report.retained_candidates(), 0);
        assert_eq!(input.stream_position().unwrap(), 4 * selected_samples);
        assert_eq!(
            report.analyzed_iq_sha256,
            format!(
                "{:x}",
                Sha256::digest(vec![0; 4 * selected_samples as usize])
            )
        );
        let windows: Vec<_> = report
            .profiles
            .into_iter()
            .map(|profile| {
                assert!(profile.evidence.sufficient_extent_for_search);
                profile.evidence.processed_windows
            })
            .collect();
        assert_eq!(windows, [6, 12, 6]);
    }
}
