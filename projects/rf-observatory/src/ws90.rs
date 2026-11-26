//! Replay Ecowitt WS90 weather station frames.

mod frame;

use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::Result;

use crate::ecowitt;
pub(crate) use crate::ecowitt::{Args, Channel, DEFAULT_CHANNEL_HZ, DEFAULT_RETAINED_FRAMES};

/// Original-rate minimum for a complete WS90 sync and body.
pub(crate) const MIN_SWEEP_SAMPLES: u64 =
    ecowitt::min_sweep_samples::<frame::Frame, { frame::PAYLOAD_BYTES }>();
/// Planned dwell in milliseconds for the nominal preamble and body.
pub(crate) const FRAME_DWELL_MS: u64 = 20;

/// Source-bound WS90 frame observations from one IQ prefix.
pub(crate) type Report = ecowitt::Report<frame::Frame>;

/// Replay one labeled sensor channel with the WS90 frame format.
///
/// # Errors
/// Rejects interrupted reads, invalid IQ, and invalid channel geometry.
pub(crate) fn replay(
    channel: Channel,
    input: &mut dyn Read,
    source_samples: u64,
    sample_limit: u64,
    max_frames: usize,
    stop: &AtomicBool,
) -> Result<Report> {
    channel.replay::<frame::Frame, { frame::PAYLOAD_BYTES }>(
        input,
        source_samples,
        sample_limit,
        max_frames,
        stop,
    )
}

/// Identify WS90 frames and publish a fresh source-bound report.
///
/// # Errors
/// Rejects invalid or changed IQ, interruption, and output failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    ecowitt::identify::<frame::Frame, { frame::PAYLOAD_BYTES }>(root, args, stop)
}
