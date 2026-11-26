//! Identify LTE FDD cells and verify their public broadcast information.

mod pbch;
mod pss;
pub(crate) mod raster;
mod sss;

use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use rustfft::num_complex::Complex32;
use serde::Serialize;

use crate::dsp::LteSyncChannel;
use crate::spectrum::USABLE_HALF_SPAN_HZ;
use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

const IQ_SAMPLE_BYTES: u64 = 4;
const DEFAULT_ANALYSIS_SAMPLES: u64 = 2_000_000;
const MAX_ANALYSIS_SAMPLES: u64 = 10_000_000;
const CHANNEL_MARGIN_HZ: u64 = 1_000_000;
const MAX_CHANNEL_OFFSET_HZ: u64 = USABLE_HALF_SPAN_HZ as u64 - CHANNEL_MARGIN_HZ;
const REPEAT_CHANNEL_SAMPLES: usize = LteSyncChannel::OUTPUT_RATE_HZ as usize / 200;
const REPEAT_TOLERANCE_SAMPLES: usize = 6;
const MIN_REPEAT_COUNT: usize = 3;

/// Select a candidate LTE carrier in a saved 20 MSps capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ at 20 MSps.
    input: PathBuf,
    /// Actual acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Candidate LTE carrier center in hertz.
    #[arg(long)]
    channel_hz: u64,
    /// Verify public LTE broadcast information for each synchronized cell.
    #[arg(long)]
    lte_pbch: bool,
    /// Maximum original-rate samples to analyze from sample zero.
    #[arg(long, default_value_t = DEFAULT_ANALYSIS_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_ANALYSIS_SAMPLES))]
    max_samples: u64,
    /// New JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Clone, Copy, Serialize)]
struct Observation {
    pss_start_channel_sample: usize,
    subframe: u8,
    pss_score: f32,
    sss_score: f32,
}

#[derive(Serialize)]
pub(crate) struct Cell {
    physical_cell_id: u16,
    longest_sync_run: usize,
    observations: Vec<Observation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch: Option<pbch::Evidence>,
}

impl Cell {
    pub(crate) fn pbch_confirmed(&self) -> bool {
        self.pbch.as_ref().is_some_and(pbch::Evidence::confirmed)
    }
}

#[derive(Serialize)]
pub(crate) struct ChannelEvidence {
    pub(crate) pss_candidates: usize,
    pub(crate) sss_matches: usize,
    pub(crate) cells: Vec<Cell>,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    channel_filter_delay_input_samples: usize,
    source_samples: u64,
    analyzed_samples: u64,
    analyzed_iq_sha256: String,
    channelized_samples: usize,
    pss_candidates: usize,
    sss_matches: usize,
    minimum_sync_run: usize,
    repeat_tolerance_channel_samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch_cell_reports: Option<usize>,
    cells: Vec<Cell>,
}

fn best_run(mut observations: Vec<Observation>) -> Vec<Observation> {
    observations.sort_unstable_by_key(|item| item.pss_start_channel_sample);
    let mut runs: Vec<(usize, Option<usize>)> = Vec::with_capacity(observations.len());
    for (index, current) in observations.iter().enumerate() {
        let current_start = current.pss_start_channel_sample;
        let previous = current_start
            .checked_sub(REPEAT_CHANNEL_SAMPLES - REPEAT_TOLERANCE_SAMPLES)
            .and_then(|latest| {
                let earliest =
                    current_start.saturating_sub(REPEAT_CHANNEL_SAMPLES + REPEAT_TOLERANCE_SAMPLES);
                let begin = observations[..index]
                    .partition_point(|event| event.pss_start_channel_sample < earliest);
                observations[begin..index]
                    .iter()
                    .enumerate()
                    .take_while(|(_, event)| event.pss_start_channel_sample <= latest)
                    .filter(|(_, event)| event.subframe != current.subframe)
                    .map(|(offset, _)| begin + offset)
                    .max_by_key(|&previous| runs[previous].0)
            });
        runs.push((
            previous.map_or(1, |previous| runs[previous].0 + 1),
            previous,
        ));
    }
    let mut run = Vec::new();
    if let Some((index, (length, _))) = runs.iter().enumerate().max_by_key(|(_, value)| value.0)
        && *length >= MIN_REPEAT_COUNT
    {
        let mut next = Some(index);
        while let Some(index) = next {
            run.push(observations[index]);
            next = runs[index].1;
        }
        run.reverse();
    }
    run
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "LTE synchronization replay interrupted"
    );
    Ok(())
}

struct Capture {
    path: PathBuf,
    input: File,
    stamp: Metadata,
    samples: u64,
}

impl Capture {
    fn open(path: PathBuf) -> Result<Self> {
        let (input, length) = file_pin::open_regular(&path, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
        let stamp = input.metadata()?;
        ensure!(
            length == stamp.len() && length.is_multiple_of(IQ_SAMPLE_BYTES),
            "LTE input needs a stable extent of complete I/Q pairs"
        );
        Ok(Self {
            path,
            input,
            stamp,
            samples: length / IQ_SAMPLE_BYTES,
        })
    }

    fn verify(&self) -> Result<()> {
        let path_stamp = self.path.symlink_metadata()?;
        ensure!(
            path_stamp.is_file()
                && file_pin::same_file(&self.stamp, &path_stamp)
                && file_pin::same_file(&self.stamp, &self.input.metadata()?),
            "LTE IQ input changed after admission"
        );
        Ok(())
    }
}

fn check_channel(center_hz: u64, channel_hz: u64) -> Result<()> {
    let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
    ensure!(
        (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&center_hz)
            && channel_hz > 0
            && center_hz.abs_diff(channel_hz) <= MAX_CHANNEL_OFFSET_HZ,
        "LTE synchronization requires a 1 MHz margin inside the central 80% of the sampled band"
    );
    Ok(())
}

/// Number of complete 1.92 MSps output intervals in the selected 20 MSps prefix.
pub(crate) fn channelized_count(input_samples: u64) -> usize {
    (input_samples * u64::from(LteSyncChannel::OUTPUT_RATE_HZ) / u64::from(SAMPLE_RATE)) as usize
}

struct Replayer {
    searcher: pss::Searcher,
    matcher: sss::Matcher,
    pbch: Option<pbch::Decoder>,
}

impl Replayer {
    fn new(decode_pbch: bool) -> Self {
        Self {
            searcher: pss::Searcher::new(),
            matcher: sss::Matcher::new(),
            pbch: decode_pbch.then(pbch::Decoder::new),
        }
    }

    fn analyze(&mut self, channelized: &[Complex32], stop: &AtomicBool) -> Result<ChannelEvidence> {
        check_stop(stop)?;
        let peaks = self.searcher.search(channelized);
        let mut by_cell = BTreeMap::<u16, Vec<Observation>>::new();
        let mut sss_matches = 0;
        for peak in &peaks {
            check_stop(stop)?;
            if let Some(found) = self.matcher.detect(channelized, peak) {
                sss_matches += 1;
                by_cell.entry(found.cell_id).or_default().push(Observation {
                    pss_start_channel_sample: peak.start,
                    subframe: found.subframe,
                    pss_score: peak.score,
                    sss_score: found.score,
                });
            }
        }

        let mut cells = Vec::new();
        for (physical_cell_id, observations) in by_cell {
            let observations = best_run(observations);
            if observations.is_empty() {
                continue;
            }
            let pbch = self
                .pbch
                .as_mut()
                .map(|decoder| decoder.examine(channelized, physical_cell_id, &observations, stop))
                .transpose()?;
            cells.push(Cell {
                physical_cell_id,
                longest_sync_run: observations.len(),
                observations,
                pbch,
            });
        }
        Ok(ChannelEvidence {
            pss_candidates: peaks.len(),
            sss_matches,
            cells,
        })
    }
}

/// Verify repeated LTE FDD normal-prefix broadcast synchronization in one channel.
///
/// # Errors
/// Rejects invalid channel geometry, incomplete or invalid IQ, changed input,
/// interruption, and publication failures.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    check_channel(args.center_hz, args.channel_hz)?;
    let mut channel = LteSyncChannel::new(args.channel_hz as f64 - args.center_hz as f64)?;
    let mut capture = Capture::open(root.join(args.input))?;
    let analyzed_samples = capture.samples.min(args.max_samples);
    let mut channelized = Vec::with_capacity(channelized_count(analyzed_samples));
    let analyzed_iq_sha256 =
        receiver::read_q11_prefix(&mut capture.input, analyzed_samples, stop, |sample, _| {
            if let Some(iq) = channel.push(sample) {
                channelized.push(Complex32::new(iq.i as f32, iq.q as f32));
            }
            Ok(())
        })?;
    let evidence = Replayer::new(args.lte_pbch).analyze(&channelized, stop)?;
    let report = Report {
        schema_version: 1,
        operation: "identify_lte_in_saved_iq",
        sample_format: "SC16_Q11_LE",
        center_frequency_hz: args.center_hz,
        channel_frequency_hz: args.channel_hz,
        sample_rate_hz: SAMPLE_RATE,
        channel_sample_rate_hz: LteSyncChannel::OUTPUT_RATE_HZ,
        channel_filter_delay_input_samples: LteSyncChannel::DELAY_INPUT_SAMPLES,
        source_samples: capture.samples,
        analyzed_samples,
        analyzed_iq_sha256,
        channelized_samples: channelized.len(),
        pss_candidates: evidence.pss_candidates,
        sss_matches: evidence.sss_matches,
        minimum_sync_run: MIN_REPEAT_COUNT,
        repeat_tolerance_channel_samples: REPEAT_TOLERANCE_SAMPLES,
        pbch_cell_reports: args.lte_pbch.then(|| {
            evidence
                .cells
                .iter()
                .filter(|cell| cell.pbch_confirmed())
                .count()
        }),
        cells: evidence.cells,
    };
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    println!(
        "Verified {} LTE cells. Output: {}",
        report.cells.len(),
        output.display()
    );
    if let Some(count) = report.pbch_cell_reports {
        println!("Validated public LTE broadcast information for {count} cell reports.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(start: usize, subframe: u8) -> Observation {
        Observation {
            pss_start_channel_sample: start,
            subframe,
            pss_score: 1.0,
            sss_score: 1.0,
        }
    }

    #[test]
    fn requires_three_alternating_hits_at_successive_half_frames() {
        let first = sync(100, 0);
        let second = sync(100 + REPEAT_CHANNEL_SAMPLES - 3, 5);
        let third = sync(100 + 2 * REPEAT_CHANNEL_SAMPLES, 0);
        assert!(best_run(vec![first, second]).is_empty());
        assert!(best_run(vec![first, sync(second.pss_start_channel_sample, 0), third]).is_empty());
        assert!(
            best_run(vec![
                first,
                second,
                sync(third.pss_start_channel_sample + 7, 0)
            ])
            .is_empty()
        );
        let run = best_run(vec![
            third,
            sync(103 + REPEAT_CHANNEL_SAMPLES, 0),
            first,
            second,
        ]);
        assert_eq!(
            run.iter()
                .map(|hit| hit.pss_start_channel_sample)
                .collect::<Vec<_>>(),
            [
                first.pss_start_channel_sample,
                second.pss_start_channel_sample,
                third.pss_start_channel_sample
            ]
        );
    }
}
