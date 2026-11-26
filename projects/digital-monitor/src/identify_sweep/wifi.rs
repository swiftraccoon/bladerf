//! Wi-Fi identification through the existing validated whole-capture replay.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};

use crate::event_replay::replay::{self, ReplayLane, WifiReplayOptions, WifiTuning};
use crate::{input, process};

/// Maximum capture-to-channel displacement admitted by the replay lane.
pub(super) const MAX_CENTER_OFFSET_HZ: u64 = WifiTuning::SWEEP_MAX_OFFSET_HZ;

/// Replay one admitted tune on the original sample grid and verify its evidence.
pub(super) fn run(
    program: &process::ProgramProvenance,
    source: &input::SourceReport,
    channel_mhz: u32,
    output: &Path,
    stop: &AtomicBool,
) -> Result<replay::LaneEvidence> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "sweep Wi-Fi replay interrupted before launch"
    );
    let options = WifiReplayOptions {
        frame_spans: true,
        tuning: WifiTuning::for_sweep(source.center_frequency_hz, channel_mhz)?,
        ..WifiReplayOptions::default()
    };
    let evidence = replay::run_full(
        program,
        ReplayLane::Wifi(options),
        source,
        None,
        output,
        stop,
    )
    .with_context(|| format!("replay Wi-Fi at {channel_mhz} MHz"))?;
    evidence.verify().context("verify Wi-Fi replay evidence")?;
    Ok(evidence)
}
