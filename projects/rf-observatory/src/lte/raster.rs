//! Replay one Q11 extent across a bounded LTE carrier grid.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::dsp::{LteRasterBank, LteSyncChannel};
use crate::{MAX_CENTER_HZ, SAMPLE_RATE};
use crate::{private_output, receiver};

use super::{
    Capture, ChannelEvidence, MAX_ANALYSIS_SAMPLES, MAX_CHANNEL_OFFSET_HZ, MIN_REPEAT_COUNT,
    REPEAT_CHANNEL_SAMPLES, REPEAT_TOLERANCE_SAMPLES, Replayer, check_channel, check_stop, pss,
    sss,
};

pub(crate) use super::channelized_count;

/// Spacing in hertz between candidate LTE centers.
pub(crate) const GRID_SPACING_HZ: u64 = LteRasterBank::GRID_SPACING_HZ;
/// Maximum nominal tune-to-carrier distance valid across accepted readbacks.
pub(crate) const GUARDED_CARRIER_OFFSET_HZ: u64 =
    MAX_CHANNEL_OFFSET_HZ - receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
/// Default number of 20 MSps input samples in a raster replay prefix.
pub(crate) const DEFAULT_ANALYSIS_SAMPLES: u64 = 1_000_000;
/// Maximum selectable prefix length at 20 MSps.
pub(crate) const MAX_PREFIX_SAMPLES: u64 = MAX_ANALYSIS_SAMPLES;
/// Maximum number of channelized complex samples held for a single raster.
pub(crate) const MAX_CHANNELIZED_SAMPLES: usize = 16_000_000;

const MIN_SWEEP_CHANNEL_SAMPLES: usize =
    (MIN_REPEAT_COUNT - 1) * REPEAT_CHANNEL_SAMPLES + sss::SYMBOL_START_DELTA + pss::SYMBOL_SAMPLES;
/// Nominal original-rate extent for three complete FDD synchronization pairs.
pub(crate) const MIN_SWEEP_SAMPLES: u64 = (MIN_SWEEP_CHANNEL_SAMPLES as u64 * SAMPLE_RATE as u64)
    .div_ceil(LteSyncChannel::OUTPUT_RATE_HZ as u64);

/// Validate inclusive absolute-grid endpoints independently of any one dwell.
///
/// # Errors
/// Rejects invalid order, alignment, and RF labels outside the accepted range.
pub(crate) fn validate_endpoints(lower_hz: u64, upper_hz: u64) -> Result<()> {
    ensure!(
        lower_hz > 0
            && lower_hz <= upper_hz
            && upper_hz <= MAX_CENTER_HZ + 8_000_000
            && lower_hz.is_multiple_of(GRID_SPACING_HZ)
            && upper_hz.is_multiple_of(GRID_SPACING_HZ),
        "LTE raster endpoints must be in ascending order on the supported 100 kHz grid"
    );
    Ok(())
}

/// Intersect a validated grid with the usable carrier centers of an admitted dwell.
pub(crate) fn covered_centers(center_hz: u64, lower_hz: u64, upper_hz: u64) -> Option<(u64, u64)> {
    let first_hz = center_hz
        .saturating_sub(MAX_CHANNEL_OFFSET_HZ)
        .div_ceil(GRID_SPACING_HZ)
        * GRID_SPACING_HZ;
    let last_hz = (center_hz + MAX_CHANNEL_OFFSET_HZ) / GRID_SPACING_HZ * GRID_SPACING_HZ;
    let lower_hz = lower_hz.max(first_hz);
    let upper_hz = upper_hz.min(last_hz);
    (lower_hz <= upper_hz).then_some((lower_hz, upper_hz))
}

/// Select an inclusive carrier grid in a saved 20 MSps capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ at 20 MSps.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// First candidate carrier in hertz, aligned to 100 kHz.
    #[arg(long)]
    lower_hz: u64,
    /// Last candidate carrier in hertz, aligned to 100 kHz.
    #[arg(long)]
    upper_hz: u64,
    /// Verify public LTE broadcast information for each synchronized cell.
    #[arg(long)]
    lte_pbch: bool,
    /// Maximum original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = DEFAULT_ANALYSIS_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_PREFIX_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// Selected absolute carrier centers and their corresponding mixer bins.
pub(crate) struct Grid {
    carrier_centers_hz: Vec<u64>,
    bins: Vec<usize>,
    offset_hz: f64,
}

impl Grid {
    /// Select each center in the inclusive request using actual LO readback.
    ///
    /// # Errors
    /// Rejects misaligned requests or carrier centers outside the usable band.
    pub(crate) fn new(center_hz: u64, lower_hz: u64, upper_hz: u64) -> Result<Self> {
        validate_endpoints(lower_hz, upper_hz)?;
        check_channel(center_hz, lower_hz)?;
        check_channel(center_hz, upper_hz)?;
        let reference_hz = (center_hz + GRID_SPACING_HZ / 2) / GRID_SPACING_HZ * GRID_SPACING_HZ;
        let carrier_centers_hz: Vec<_> = (lower_hz..=upper_hz)
            .step_by(GRID_SPACING_HZ as usize)
            .collect();
        let bins = carrier_centers_hz
            .iter()
            .map(|&carrier_hz| {
                let grid_delta = (carrier_hz as i64 - reference_hz as i64) / GRID_SPACING_HZ as i64;
                grid_delta.rem_euclid(LteRasterBank::BIN_COUNT as i64) as usize
            })
            .collect();
        Ok(Self {
            carrier_centers_hz,
            bins,
            offset_hz: reference_hz as f64 - center_hz as f64,
        })
    }

    /// Analyze one admitted IQ prefix, optionally verifying LTE broadcast blocks.
    ///
    /// # Errors
    /// Returns an error for an invalid extent, a read failure, invalid Q11,
    /// or interruption. The caller verifies the source identity for publication.
    pub(crate) fn replay(
        self,
        input: &mut dyn Read,
        analyzed_samples: u64,
        decode_pbch: bool,
        stop: &AtomicBool,
    ) -> Result<Replay> {
        check_stop(stop)?;
        ensure!(
            (1..=MAX_PREFIX_SAMPLES).contains(&analyzed_samples),
            "invalid LTE raster prefix sample count"
        );
        let channelized_samples = channelized_count(analyzed_samples);
        let total = channelized_samples * self.bins.len();
        ensure!(
            total <= MAX_CHANNELIZED_SAMPLES,
            "LTE raster needs {total} channelized samples, exceeding the {MAX_CHANNELIZED_SAMPLES} sample limit. Reduce --max-samples or narrow the range"
        );
        let mut bank = LteRasterBank::new(self.offset_hz, &self.bins)?;
        let mut streams: Vec<_> = self
            .bins
            .iter()
            .map(|_| Vec::with_capacity(channelized_samples))
            .collect();
        let analyzed_iq_sha256 =
            receiver::read_q11_prefix(input, analyzed_samples, stop, |sample, _| {
                if let Some(bins) = bank.push(sample) {
                    for (stream, &iq) in streams.iter_mut().zip(bins) {
                        stream.push(iq);
                    }
                }
                Ok(())
            })?;
        drop(bank);
        let mut replayer = Replayer::new(decode_pbch);
        let channels = self
            .carrier_centers_hz
            .into_iter()
            .zip(streams)
            .map(|(channel_frequency_hz, stream)| {
                ensure!(
                    stream.len() == channelized_samples,
                    "LTE raster stream length differs from the selected prefix"
                );
                let evidence = replayer.analyze(&stream, stop)?;
                Ok(CarrierReport {
                    channel_frequency_hz,
                    evidence,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Replay {
            analyzed_iq_sha256,
            channelized_samples_per_channel: channelized_samples,
            channels,
        })
    }
}

/// Synchronization evidence at one absolute carrier center.
#[derive(Serialize)]
pub(crate) struct CarrierReport {
    pub(crate) channel_frequency_hz: u64,
    #[serde(flatten)]
    pub(crate) evidence: ChannelEvidence,
}

/// Per-carrier evidence from one analyzed input prefix.
pub(crate) struct Replay {
    pub(crate) analyzed_iq_sha256: String,
    pub(crate) channelized_samples_per_channel: usize,
    pub(crate) channels: Vec<CarrierReport>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    lower_hz: u64,
    upper_hz: u64,
    raster_step_hz: u64,
    sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    channel_filter_delay_input_samples: usize,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    channelized_samples_per_channel: usize,
    minimum_sync_run: usize,
    repeat_tolerance_channel_samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch_cell_reports: Option<usize>,
    channels: Vec<CarrierReport>,
}

/// Verify repeated LTE synchronization on each aligned carrier in one saved window.
///
/// # Errors
/// Rejects invalid grid geometry, an excessive channelized extent, invalid or
/// changed IQ, interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let grid = Grid::new(args.center_hz, args.lower_hz, args.upper_hz)?;
    let mut capture = Capture::open(root.join(args.input))?;
    let analyzed_samples = capture.samples.min(args.max_samples);
    let replay = grid.replay(&mut capture.input, analyzed_samples, args.lte_pbch, stop)?;
    let report = Report {
        schema_version: 1,
        operation: "identify_lte_raster_in_saved_iq",
        sample_format: "SC16_Q11_LE",
        center_frequency_hz: args.center_hz,
        lower_hz: args.lower_hz,
        upper_hz: args.upper_hz,
        raster_step_hz: GRID_SPACING_HZ,
        sample_rate_hz: SAMPLE_RATE,
        channel_sample_rate_hz: LteSyncChannel::OUTPUT_RATE_HZ,
        channel_filter_delay_input_samples: LteSyncChannel::DELAY_INPUT_SAMPLES,
        source_samples: capture.samples,
        analyzed_samples,
        analyzed_iq_sha256: replay.analyzed_iq_sha256,
        channelized_samples_per_channel: replay.channelized_samples_per_channel,
        minimum_sync_run: MIN_REPEAT_COUNT,
        repeat_tolerance_channel_samples: REPEAT_TOLERANCE_SAMPLES,
        pbch_cell_reports: args.lte_pbch.then(|| {
            replay
                .channels
                .iter()
                .flat_map(|channel| &channel.evidence.cells)
                .filter(|cell| cell.pbch_confirmed())
                .count()
        }),
        channels: replay.channels,
    };
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    let cells = report
        .channels
        .iter()
        .map(|channel| channel.evidence.cells.len())
        .sum::<usize>();
    println!(
        "Verified {cells} LTE cells across {} candidate frequencies. Output: {}",
        report.channels.len(),
        output.display()
    );
    if let Some(count) = report.pbch_cell_reports {
        println!("Validated public LTE broadcast information for {count} cell reports.");
    }
    Ok(())
}
