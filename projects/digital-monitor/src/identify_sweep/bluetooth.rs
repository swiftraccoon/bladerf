//! One CRC-checked Bluetooth LE replay per selected tune.

mod translate;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::event_replay::replay::{self, LaneEvidence, ReplayLane};
use crate::input::SourceReport;
use crate::process::ProgramProvenance;
use crate::report;

const HZ_PER_MHZ: u64 = 1_000_000;
const MAX_TRANSLATION_HZ: u64 = HZ_PER_MHZ / 2;
const MAX_CHANNEL_OFFSET_HZ: u64 = 8_000_000;
const MAX_REPLAY_RECEIPT_BYTES: u64 = 64 * 1024;

pub(super) struct Programs {
    pub(super) monitor: ProgramProvenance,
    pub(super) decoder: ProgramProvenance,
    pub(super) workspace: PathBuf,
}

/// Put 1 MHz decoder bins on the absolute channel raster.
fn decoder_center(capture_hz: u64) -> Option<u64> {
    let rounded_mhz =
        (capture_hz.saturating_add(MAX_TRANSLATION_HZ) / HZ_PER_MHZ).clamp(2400, 2480);
    let center_hz = rounded_mhz * HZ_PER_MHZ;
    (capture_hz.abs_diff(center_hz) <= MAX_TRANSLATION_HZ).then_some(center_hz)
}

/// Admit only bins in the interior of both the original and shifted grids.
pub(super) fn eligible(capture_hz: u64, channel_hz: u64) -> bool {
    capture_hz.abs_diff(channel_hz) <= MAX_CHANNEL_OFFSET_HZ
        && decoder_center(capture_hz)
            .is_some_and(|center| center.abs_diff(channel_hz) <= MAX_CHANNEL_OFFSET_HZ)
}

/// Enumerate the logical LE channel numbers on the physical 2 MHz raster.
pub(super) fn channels() -> impl Iterator<Item = (u16, u64)> {
    (2402..=2480).step_by(2).map(|frequency_mhz: u16| {
        let channel = match frequency_mhz {
            2402 => 37,
            2426 => 38,
            2480 => 39,
            2404..=2424 => (frequency_mhz - 2404) / 2,
            _ => 11 + (frequency_mhz - 2428) / 2,
        };
        (channel, u64::from(frequency_mhz) * HZ_PER_MHZ)
    })
}

#[derive(Debug)]
pub(super) struct TuneEvidence {
    pub(super) decoder_center_hz: u64,
    pub(super) receipt_sha256: String,
    counts: BTreeMap<u16, u64>,
}

impl TuneEvidence {
    pub(super) fn validated_frames(&self, frequency_mhz: u16) -> u64 {
        self.counts.get(&frequency_mhz).copied().unwrap_or(0)
    }
}

#[derive(Serialize)]
struct ReplayEvidence<'a> {
    schema_version: u32,
    operation: &'static str,
    translation: &'a translate::Translation,
    validated_frames_by_frequency_mhz: &'a BTreeMap<u16, u64>,
    replay: &'a LaneEvidence,
}

/// Align the admitted IQ to the decoder grid, then verify its replay products.
pub(super) fn run(
    programs: &Programs,
    source: &SourceReport,
    output: &Path,
    stop: &AtomicBool,
) -> Result<TuneEvidence> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "Bluetooth LE replay interrupted before staging"
    );
    let decoder_center_hz = decoder_center(source.center_frequency_hz)
        .context("capture center is outside the Bluetooth replay tuning range")?;
    let parent = output
        .parent()
        .context("Bluetooth replay needs an output parent")?;
    let scratch = tempfile::Builder::new()
        .prefix(".bluetooth-iq-")
        .tempdir_in(parent)?;
    let prepared = translate::prepare(source, decoder_center_hz, scratch.path(), stop)?;
    prepared.verify()?;
    let replay = replay::run_full(
        &programs.monitor,
        ReplayLane::BluetoothSweep {
            decoder: &programs.decoder,
            workspace: &programs.workspace,
        },
        &prepared.source,
        None,
        output,
        stop,
    )?;
    prepared.verify()?;
    replay.verify()?;
    replay.verify_bluetooth_input(&prepared.source)?;
    let counts = replay
        .validated_ble_counts()
        .iter()
        .filter(|&(&frequency, _)| {
            eligible(
                source.center_frequency_hz,
                u64::from(frequency) * HZ_PER_MHZ,
            )
        })
        .map(|(&frequency, &count)| (frequency, count))
        .collect();
    scratch.close().context("remove temporary Bluetooth IQ")?;
    report::publish_json(
        output,
        "bluetooth.json",
        &ReplayEvidence {
            schema_version: 1,
            operation: "identify_bluetooth_le_tune",
            translation: &prepared.provenance,
            validated_frames_by_frequency_mhz: &counts,
            replay: &replay,
        },
    )?;
    let receipt =
        report::hash_direct_regular_file(&output.join("bluetooth.json"), MAX_REPLAY_RECEIPT_BYTES)?;
    Ok(TuneEvidence {
        decoder_center_hz,
        receipt_sha256: receipt.sha256,
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ble_raster_has_all_logical_channels_and_advertising_centers() {
        let raster = channels().collect::<Vec<_>>();
        let mut logical = raster
            .iter()
            .map(|&(channel, _)| channel)
            .collect::<Vec<_>>();
        logical.sort_unstable();
        assert_eq!(logical, (0..40).collect::<Vec<_>>());
        for channel in [
            (37, 2_402_000_000),
            (38, 2_426_000_000),
            (39, 2_480_000_000),
        ] {
            assert!(raster.contains(&channel));
        }
    }

    #[test]
    fn grid_rounding_and_source_edge_exclusions_are_bounded() {
        assert_eq!(decoder_center(2_429_371_200), Some(2_429_000_000));
        assert_eq!(decoder_center(2_480_500_000), Some(2_480_000_000));
        assert_eq!(decoder_center(2_480_500_001), None);
        assert!(eligible(2_429_371_200, 2_426_000_000));
        assert!(!eligible(2_430_371_200, 2_422_000_000));
        assert!(!eligible(2_430_500_000, 2_422_600_000));
    }
}
