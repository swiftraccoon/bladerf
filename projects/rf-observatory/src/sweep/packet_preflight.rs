//! Bound LoRa packet work, shared IQ memory, and scratch space before a sweep.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::lora::decode::{DwellSourceWork, PacketInterpretations, PacketProfile};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::triage::lora_decode_sweep::PacketReplayLimits;

const CF32_SAMPLE_BYTES: u64 = 8;

#[derive(Debug, Serialize)]
pub(super) struct PacketPreflight {
    #[serde(skip_serializing_if = "Option::is_none")]
    meshtastic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lorawan_join: Option<bool>,
    center_readback_tolerance_hz: u64,
    pub(super) maximum_attempts: usize,
    max_attempts: u32,
    maximum_profile_samples: u64,
    max_total_profile_samples: u64,
    maximum_source_iq_bytes: u64,
    max_buffered_iq_bytes: u64,
    pub(super) peak_temporary_iq_bytes: u64,
}

/// Check the maximum packet work across every allowed tuning readback.
///
/// Profile work and source reads use separate maxima per dwell. Each dwell
/// reuses a bounded source prefix and stages one channelized file at a time.
pub(super) fn bound(
    profiles: &[PacketProfile],
    interpretations: PacketInterpretations,
    limits: PacketReplayLimits,
    dwells: impl IntoIterator<Item = (u64, u64)>,
    passes: usize,
) -> Result<PacketPreflight> {
    let tolerance = ACQUISITION_CENTER_TOLERANCE_HZ;
    let mut maximum_attempts = 0;
    let mut maximum_profile_samples = 0;
    let mut maximum_source_iq_bytes = 0;
    let mut max_buffered_iq_bytes = 0;
    let mut peak_temporary_iq_bytes = 0;
    let mut channels = BTreeSet::new();
    for (nominal_hz, saved_samples) in dwells {
        let selected_samples = limits.max_samples_per_dwell.min(saved_samples);
        let mut most_attempts = 0;
        let mut most_channels = 0;
        for observed_hz in nominal_hz - tolerance..=nominal_hz + tolerance {
            channels.clear();
            let mut attempts = 0;
            for profile in profiles.iter().copied().filter(|profile| {
                selected_samples >= profile.minimum_samples() && profile.is_covered(observed_hz)
            }) {
                attempts += 1;
                if channels.insert(profile.channel_key()) {
                    let bytes = profile.channelized_samples(selected_samples) * CF32_SAMPLE_BYTES;
                    peak_temporary_iq_bytes = peak_temporary_iq_bytes.max(bytes);
                }
            }
            most_attempts = most_attempts.max(attempts);
            most_channels = most_channels.max(channels.len());
        }
        maximum_attempts += most_attempts * passes;
        maximum_profile_samples += most_attempts as u64 * selected_samples * passes as u64;
        let source = DwellSourceWork::new(selected_samples, most_channels);
        maximum_source_iq_bytes += source.source_iq_bytes * passes as u64;
        max_buffered_iq_bytes = max_buffered_iq_bytes.max(source.buffered_iq_bytes);
    }
    ensure!(
        maximum_attempts <= limits.max_attempts as usize,
        "LoRa packet sweep can need {maximum_attempts} attempts, exceeding --lora-max-attempts ({})",
        limits.max_attempts
    );
    ensure!(
        maximum_profile_samples <= limits.max_total_profile_samples,
        "LoRa packet sweep can need {maximum_profile_samples} profile samples, exceeding --max-total-profile-samples ({})",
        limits.max_total_profile_samples
    );
    Ok(PacketPreflight {
        meshtastic: interpretations.meshtastic.then_some(true),
        lorawan_join: interpretations.lorawan_join.then_some(true),
        center_readback_tolerance_hz: tolerance,
        maximum_attempts,
        max_attempts: limits.max_attempts,
        maximum_profile_samples,
        max_total_profile_samples: limits.max_total_profile_samples,
        maximum_source_iq_bytes,
        max_buffered_iq_bytes,
        peak_temporary_iq_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_channel_work_at_one_readback_per_dwell() -> Result<()> {
        let profiles = [
            PacketProfile::parse("892249999:500000:7:43")?,
            PacketProfile::parse("907750001:500000:7:43")?,
            PacketProfile::parse("907750001:500000:7:52")?,
        ];
        let samples = 200_000;
        let preview = bound(
            &profiles,
            PacketInterpretations::default(),
            PacketReplayLimits::default(),
            [(900_000_000, samples)],
            2,
        )?;
        assert_eq!(preview.maximum_attempts, 4);
        assert_eq!(preview.maximum_profile_samples, 4 * samples);
        // The two positive-side hypotheses share one translation. The
        // negative-side profile fits only at a different readback.
        assert_eq!(preview.maximum_source_iq_bytes, 2 * samples * 4);
        assert_eq!(preview.max_buffered_iq_bytes, 0);
        assert_eq!(preview.peak_temporary_iq_bytes, samples / 10 * 8);
        Ok(())
    }

    #[test]
    fn bounds_shared_prefix_and_separate_tail_reads() -> Result<()> {
        let profiles = [
            PacketProfile::parse("915300000:125000:7:43")?,
            PacketProfile::parse("915300000:125000:7:52")?,
            PacketProfile::parse("916600000:500000:8:52")?,
        ];
        let samples = 15_000_000;
        let preview = bound(
            &profiles,
            PacketInterpretations::default(),
            PacketReplayLimits::default(),
            [(915_800_000, samples), (400_000_000, samples)],
            2,
        )?;
        assert_eq!(preview.maximum_attempts, 6);
        assert_eq!(preview.maximum_profile_samples, 6 * samples);
        // Each pass reads the 40 MB prefix once and the 20 MB tail twice.
        assert_eq!(preview.maximum_source_iq_bytes, 160_000_000);
        assert_eq!(preview.max_buffered_iq_bytes, 40_000_000);
        assert_eq!(preview.peak_temporary_iq_bytes, samples / 10 * 8);
        Ok(())
    }
}
