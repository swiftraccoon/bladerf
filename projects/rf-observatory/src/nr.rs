//! Identify repeated 5G NR downlink synchronization from saved IQ.

mod batch;
mod channel;
mod pbch;
mod pss;
pub(crate) mod raster;
mod sss;

use std::collections::BTreeMap;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use rustfft::num_complex::Complex32;
use serde::Serialize;

use crate::{
    MAX_CENTER_HZ, MAX_SAMPLES, MIN_CENTER_HZ, SAMPLE_RATE, file_pin, private_output, receiver,
};

pub(crate) const MAX_PREFIX_SAMPLES: u64 = 4_000_000;
pub(crate) const MIN_DWELL_SAMPLES: u64 = 120_000;
pub(crate) const MAX_PROFILE_COUNT: usize = 16;
pub(crate) const MAX_DWELL_CHANNEL_SAMPLES: u64 = 8_000_000;
pub(crate) use self::pbch::MAX_OBSERVATIONS as PBCH_MAX_OBSERVATIONS;
const IQ_SAMPLE_BYTES: u64 = 4;
// TS 38.331 v17.1.0, ServingCellConfigCommon, ssb-PeriodicityServingCell.
const SPACINGS_MS: [u32; 6] = [5, 10, 20, 40, 80, 160];
const SPACING_TOLERANCE_SAMPLES: usize = 8;

/// An SS raster reference and synchronization block subcarrier spacing.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Profile {
    ss_ref_hz: u64,
    subcarrier_spacing_hz: u32,
}

impl Profile {
    /// Parse `SS_REF_HZ:SCS_KHZ`, with SCS 15 or 30.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (reference, spacing) = value
            .split_once(':')
            .context("expected SS_REF_HZ:SCS_KHZ")?;
        let ss_ref_hz = reference
            .parse()
            .context("invalid NR synchronization frequency")?;
        let subcarrier_spacing_hz = match spacing {
            "15" => 15_000,
            "30" => 30_000,
            _ => anyhow::bail!("NR synchronization requires 15 or 30 kHz spacing"),
        };
        ensure!(
            (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&ss_ref_hz),
            "NR frequency is outside the supported tuning range"
        );
        Ok(Self {
            ss_ref_hz,
            subcarrier_spacing_hz,
        })
    }

    /// Require PSS and SSS, with carrier-offset margin, in one guarded sideband.
    pub(crate) fn covered_by(self, center_hz: u64) -> bool {
        self.sideband_covers(center_hz, false)
    }

    /// Return half-open RF bounds with margin for carrier offset.
    ///
    /// Include the full SS/PBCH block when broadcast verification is enabled.
    pub(crate) fn guarded_frequency_range(self, include_pbch: bool) -> Range<u64> {
        let (low_bins, high_bins) = if include_pbch { (121, 120) } else { (65, 64) };
        let spacing = u64::from(self.subcarrier_spacing_hz);
        self.ss_ref_hz - low_bins * spacing..self.ss_ref_hz + high_bins * spacing
    }

    fn sideband_covers(self, center_hz: u64, include_pbch: bool) -> bool {
        let range = self.guarded_frequency_range(include_pbch);
        crate::sweep::plan::observed_bands(center_hz, 0)
            .iter()
            .any(|span| span.contains_interval(range.start, range.end))
    }

    pub(crate) fn channelized_count(self, samples: u64) -> usize {
        channel::output_count(samples, self.subcarrier_spacing_hz)
    }

    /// Synchronization raster reference position in hertz.
    pub(crate) fn ss_ref_hz(self) -> u64 {
        self.ss_ref_hz
    }

    fn sample_rate(self) -> u32 {
        self.subcarrier_spacing_hz * pss::SYMBOL_SAMPLES as u32
    }

    fn channelizer(self, center_hz: u64) -> Result<channel::Channelizer> {
        let offset = self.ss_ref_hz as f64 - center_hz as f64;
        channel::Channelizer::new(offset, self.subcarrier_spacing_hz)
    }
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "NR synchronization replay interrupted"
    );
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
struct Observation {
    pss_start_channel_sample: usize,
    pss_score: f32,
    sss_score: f32,
    carrier_offset_hz: f32,
}

#[derive(Serialize)]
struct Cell {
    physical_cell_id: u16,
    matched_spacing_ms: u32,
    observations: Vec<Observation>,
}

fn repeated_run(observations: &[Observation], spacing: usize) -> Vec<Observation> {
    let mut links: Vec<(usize, Option<usize>)> = Vec::with_capacity(observations.len());
    for (index, current) in observations.iter().enumerate() {
        let start = current.pss_start_channel_sample;
        let previous = start
            .checked_sub(spacing - SPACING_TOLERANCE_SAMPLES)
            .and_then(|latest| {
                let earlier = &observations[..index];
                let earliest = start.saturating_sub(spacing + SPACING_TOLERANCE_SAMPLES);
                let low = earlier.partition_point(|item| item.pss_start_channel_sample < earliest);
                let high = earlier.partition_point(|item| item.pss_start_channel_sample <= latest);
                (low..high).max_by_key(|&candidate| links[candidate].0)
            });
        links.push((previous.map_or(1, |prior| links[prior].0 + 1), previous));
    }
    let mut run = Vec::new();
    if let Some((index, (length, _))) = links.iter().enumerate().max_by_key(|(_, value)| value.0)
        && *length >= 2
    {
        let mut next = Some(index);
        while let Some(index) = next {
            run.push(observations[index]);
            next = links[index].1;
        }
        run.reverse();
    }
    run
}

#[derive(Serialize)]
pub(crate) struct ChannelEvidence {
    profile: Profile,
    channel_sample_rate_hz: u32,
    channelized_samples: usize,
    pss_candidates: usize,
    omitted_pss_candidates: usize,
    sss_matches: usize,
    cells: Vec<Cell>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch: Option<pbch::Evidence>,
}

impl ChannelEvidence {
    pub(crate) fn cell_count(&self) -> usize {
        self.cells.len()
    }
}

#[derive(Serialize)]
pub(crate) struct Evidence {
    analyzed_iq_sha256: String,
    channel_filter_delay_input_samples: usize,
    channels: Vec<ChannelEvidence>,
}

impl Evidence {
    pub(crate) fn cell_count(&self) -> usize {
        self.channels.iter().map(ChannelEvidence::cell_count).sum()
    }

    pub(crate) fn pbch_count(&self) -> usize {
        self.channels
            .iter()
            .filter_map(|channel| channel.pbch.as_ref())
            .map(pbch::Evidence::frame_count)
            .sum()
    }
}

/// Reuse matched templates and FFT workspaces across already planned dwells.
pub(crate) struct Replayer {
    pss: pss::Searcher,
    sss: sss::Matcher,
    decode_pbch: bool,
}

impl Replayer {
    pub(crate) fn new() -> Self {
        Self {
            pss: pss::Searcher::new(),
            sss: sss::Matcher::new(),
            decode_pbch: false,
        }
    }

    pub(crate) fn with_pbch(mut self, decode_pbch: bool) -> Self {
        self.decode_pbch = decode_pbch;
        self
    }

    fn analyze(
        &mut self,
        profile: Profile,
        iq: &[Complex32],
        stop: &AtomicBool,
    ) -> Result<ChannelEvidence> {
        let search = self.pss.search(iq, stop)?;
        let mut by_cell = BTreeMap::<u16, Vec<Observation>>::new();
        let mut sss_matches = 0;
        for peak in &search.peaks {
            check_stop(stop)?;
            if let Some(found) = self.sss.detect(iq, peak) {
                sss_matches += 1;
                by_cell.entry(found.cell_id).or_default().push(Observation {
                    pss_start_channel_sample: peak.start,
                    pss_score: peak.score,
                    sss_score: found.score,
                    carrier_offset_hz: found.carrier_offset * profile.subcarrier_spacing_hz as f32,
                });
            }
        }
        let rate = profile.sample_rate();
        let mut cells = Vec::new();
        for (physical_cell_id, mut candidates) in by_cell {
            check_stop(stop)?;
            candidates.sort_unstable_by_key(|item| item.pss_start_channel_sample);
            let mut longest = Vec::new();
            let mut matched_spacing_ms = 0;
            for spacing in SPACINGS_MS {
                let run = repeated_run(&candidates, (rate / 1_000 * spacing) as usize);
                if run.len() > longest.len() {
                    longest = run;
                    matched_spacing_ms = spacing;
                }
            }
            if !longest.is_empty() {
                cells.push(Cell {
                    physical_cell_id,
                    matched_spacing_ms,
                    observations: longest,
                });
            }
        }
        Ok(ChannelEvidence {
            profile,
            channel_sample_rate_hz: rate,
            channelized_samples: iq.len(),
            pss_candidates: search.peaks.len(),
            omitted_pss_candidates: search.omitted_peaks,
            sss_matches,
            cells,
            pbch: None,
        })
    }

    /// Channelize all planned profiles in one original IQ pass and bind its digest.
    pub(crate) fn replay(
        &mut self,
        input: &mut dyn Read,
        center_hz: u64,
        samples: u64,
        profiles: &[Profile],
        stop: &AtomicBool,
    ) -> Result<Evidence> {
        if self.decode_pbch {
            return self.replay_buffered(input, center_hz, samples, profiles, stop);
        }
        check_stop(stop)?;
        let mut channels = profiles
            .iter()
            .map(|&profile| {
                Ok((
                    profile.channelizer(center_hz)?,
                    Vec::with_capacity(profile.channelized_count(samples)),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let analyzed_iq_sha256 = receiver::read_q11_prefix(input, samples, stop, |sample, _| {
            for (channel, iq) in &mut channels {
                if let Some(sample) = channel.push(sample) {
                    iq.push(sample);
                }
            }
            Ok(())
        })?;
        let channels = channels
            .into_iter()
            .zip(profiles)
            .map(|((_, iq), &profile)| self.analyze(profile, &iq, stop))
            .collect::<Result<_>>()?;
        Ok(Evidence {
            analyzed_iq_sha256,
            channel_filter_delay_input_samples: channel::Channelizer::DELAY_INPUT_SAMPLES,
            channels,
        })
    }
}

/// Verify one NR SS reference in a saved 20 MSps capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Saved little-endian SC16 Q11 IQ.
    input: PathBuf,
    /// Observed acquisition center in hertz.
    #[arg(long)]
    center_hz: u64,
    /// Synchronization profile as SS_REF_HZ:SCS_KHZ.
    #[arg(long, value_parser = Profile::parse)]
    profile: Profile,
    /// Decode and CRC-check the public NR PBCH for repeated cells.
    #[arg(long)]
    nr_pbch: bool,
    /// Maximum original-rate samples analyzed from sample zero.
    #[arg(long, default_value_t = MAX_PREFIX_SAMPLES, value_parser = clap::value_parser!(u64).range(MIN_DWELL_SAMPLES..=MAX_PREFIX_SAMPLES))]
    max_samples: u64,
    /// New private JSON report file in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    operation: &'static str,
    sample_format: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    source_samples: u64,
    analyzed_samples: u64,
    cell_reports: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pbch_reports: Option<usize>,
    #[serde(flatten)]
    evidence: Evidence,
}

/// Search for matching PSS and SSS that repeat at a supported spacing.
pub(crate) fn identify(root: &Path, args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let tolerance = receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
    ensure!(
        (MIN_CENTER_HZ - tolerance..=MAX_CENTER_HZ + tolerance).contains(&args.center_hz)
            && args.profile.covered_by(args.center_hz),
        "NR synchronization signals must fit in one guarded receive sideband"
    );
    let path = root.join(args.input);
    let (mut input, bytes) = file_pin::open_regular(&path, MAX_SAMPLES * IQ_SAMPLE_BYTES)?;
    ensure!(
        bytes.is_multiple_of(IQ_SAMPLE_BYTES),
        "NR input requires complete I/Q pairs"
    );
    let stamp = input.metadata()?;
    ensure!(
        stamp.len() == bytes,
        "NR input extent changed after opening"
    );
    let source_samples = bytes / IQ_SAMPLE_BYTES;
    let analyzed_samples = source_samples.min(args.max_samples);
    ensure!(
        analyzed_samples >= MIN_DWELL_SAMPLES,
        "NR synchronization requires at least {MIN_DWELL_SAMPLES} samples"
    );
    let evidence = Replayer::new().with_pbch(args.nr_pbch).replay(
        &mut input,
        args.center_hz,
        analyzed_samples,
        &[args.profile],
        stop,
    )?;
    let report = Report {
        schema_version: 1,
        operation: "identify_nr_in_saved_iq",
        sample_format: "SC16_Q11_LE",
        center_frequency_hz: args.center_hz,
        sample_rate_hz: SAMPLE_RATE,
        source_samples,
        analyzed_samples,
        cell_reports: evidence.cell_count(),
        pbch_reports: args.nr_pbch.then(|| evidence.pbch_count()),
        evidence,
    };
    let output = root.join(args.output);
    private_output::publish_json_file(&output, &report, || {
        let current = path.symlink_metadata()?;
        ensure!(
            current.is_file()
                && file_pin::same_file(&stamp, &current)
                && file_pin::same_file(&stamp, &input.metadata()?),
            "NR IQ input changed after admission"
        );
        check_stop(stop)
    })?;
    if let Some(count) = report.pbch_reports {
        println!("Verified {count} NR PBCH observations.");
    }
    println!(
        "Found {} repeated NR cell reports. Output: {}",
        report.cell_reports,
        output.display()
    );
    Ok(())
}
