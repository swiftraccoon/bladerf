//! Buffer one dwell for raster channelization and broadcast demodulation.

use std::io::Read;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};

use crate::receiver;

use super::channel::Channelizer;
use super::pbch;
use super::{
    Evidence, MAX_DWELL_CHANNEL_SAMPLES, MAX_PREFIX_SAMPLES, Profile, Replayer, check_stop,
};

const CONVERSION_BLOCK_SAMPLES: usize = 4_096;

impl Replayer {
    /// Read and hash one prefix for all eligible synchronization profiles.
    ///
    /// Reuse one channelized IQ buffer in profile order. Broadcast demodulation
    /// uses the original samples when enabled. The caller supplies the admitted
    /// prefix and rechecks the identity of its source after replay.
    ///
    /// # Errors
    /// Returns an error for an invalid profile or extent, an interrupted
    /// replay, or a short or invalid IQ read.
    pub(crate) fn replay_buffered(
        &mut self,
        input: &mut dyn Read,
        center_hz: u64,
        samples: u64,
        profiles: &[Profile],
        stop: &AtomicBool,
    ) -> Result<Evidence> {
        check_stop(stop)?;
        ensure!(
            (1..=MAX_PREFIX_SAMPLES).contains(&samples),
            "NR prefix must contain 1 through {MAX_PREFIX_SAMPLES} samples"
        );
        ensure!(
            !profiles.is_empty() && profiles.iter().all(|profile| profile.covered_by(center_hz)),
            "NR profiles must fit in guarded receive sidebands"
        );
        let channel_capacity = profiles
            .iter()
            .map(|profile| profile.channelized_count(samples))
            .max()
            .unwrap_or_default();
        ensure!(
            u64::try_from(channel_capacity)? <= MAX_DWELL_CHANNEL_SAMPLES,
            "NR channel exceeds the {MAX_DWELL_CHANNEL_SAMPLES}-sample buffer limit"
        );

        let mut raw = Vec::with_capacity(usize::try_from(samples)?);
        let analyzed_iq_sha256 = receiver::read_q11_prefix(input, samples, stop, |sample, _| {
            raw.push(sample);
            Ok(())
        })?;
        let mut channelized = Vec::with_capacity(channel_capacity);
        let mut channels = Vec::with_capacity(profiles.len());
        for &profile in profiles {
            check_stop(stop)?;
            channelized.clear();
            let mut channelizer = profile.channelizer(center_hz)?;
            for block in raw.chunks(CONVERSION_BLOCK_SAMPLES) {
                check_stop(stop)?;
                channelized.extend(
                    block
                        .iter()
                        .copied()
                        .filter_map(|sample| channelizer.push(sample)),
                );
            }
            let mut evidence = self.analyze(profile, &channelized, stop)?;
            if self.decode_pbch {
                evidence.pbch = Some(pbch::examine(
                    &raw,
                    center_hz,
                    profile,
                    &evidence.cells,
                    stop,
                )?);
            }
            channels.push(evidence);
        }
        Ok(Evidence {
            analyzed_iq_sha256,
            channel_filter_delay_input_samples: Channelizer::DELAY_INPUT_SAMPLES,
            channels,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};

    use super::*;

    #[test]
    fn buffered_raster_matches_streaming_replay_and_consumes_one_prefix() -> Result<()> {
        let center_hz = 915_000_000;
        let samples = 20_000;
        let profiles = [
            Profile::parse("918000000:30")?,
            Profile::parse("912000000:15")?,
        ];
        let stop = AtomicBool::new(false);
        let mut input = tempfile::tempfile()?;
        let mut bytes = Vec::new();
        for index in 0..samples {
            let i = i16::try_from(index % 971)? - 485;
            let q = i16::try_from(index % 1_021)? - 510;
            bytes.extend(i.to_le_bytes());
            bytes.extend(q.to_le_bytes());
        }
        input.write_all(&bytes)?;
        // Leave an invalid sample past the selected prefix to detect overreads.
        input.write_all(&[0xff, 0x7f, 0, 0])?;
        input.rewind()?;

        let mut replayer = Replayer::new();
        let raster = replayer.replay_buffered(&mut input, center_hz, samples, &profiles, &stop)?;
        assert_eq!(input.stream_position()?, samples * 4);
        assert_eq!(raster.channels.len(), profiles.len());
        input.rewind()?;
        let streaming = replayer.replay(&mut input, center_hz, samples, &profiles, &stop)?;
        assert_eq!(
            serde_json::to_value(raster)?,
            serde_json::to_value(streaming)?
        );
        Ok(())
    }
}
