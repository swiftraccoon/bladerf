//! Share source IQ across independent replay routes with bounded buffering.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::Iq16;
use crate::receiver::{self, IQ_REPLAY_BLOCK_SAMPLES, MAX_SHARED_IQ_SAMPLES};
use crate::triage::{admission::Tune, check_stop};

use super::IQ_SAMPLE_BYTES;

const READ_BLOCK_BYTES: usize = IQ_REPLAY_BLOCK_SAMPLES * IQ_SAMPLE_BYTES as usize;

type DwellKey<'a> = (usize, &'a str);

/// One admitted route in pass and tune order. Zero samples denotes a short route.
#[derive(Clone, Copy)]
pub(in crate::triage) struct Request<'a> {
    pub(in crate::triage) pass_index: usize,
    pub(in crate::triage) tune: &'a Tune,
    pub(in crate::triage) samples: u64,
}

impl<'a> Request<'a> {
    pub(in crate::triage) fn key(self) -> DwellKey<'a> {
        (self.pass_index, &self.tune.directory)
    }
}

/// An independent analyzer whose next route is in nondecreasing dwell order.
pub(in crate::triage) trait Task<'a> {
    fn next_request(&self) -> Option<Request<'a>>;

    /// Consume one route, starting at sample zero of the admitted IQ stream.
    /// Short routes receive an empty stream.
    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()>;
}

/// An analyzer capable of processing all its routes for one dwell together.
pub(in crate::triage) trait StreamingTask<'a>: Task<'a> {
    /// Admit the routes at this dwell and return the positive and short counts.
    /// Positive routes must match the supplied tune and prefix length.
    fn begin_dwell(&mut self, request: Request<'a>, stop: &AtomicBool) -> Result<(usize, usize)>;

    /// Deliver the next validated sample at its original prefix index.
    fn push_sample(&mut self, sample: Iq16, index: u64);

    /// Complete the active routes in their usual order with the source digest.
    fn finish_dwell(&mut self, analyzed_iq_sha256: &str, stop: &AtomicBool) -> Result<()>;
}

fn replay_prefix(
    task: &mut dyn Task<'_>,
    input: &mut dyn Read,
    samples: u64,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    let mut bounded = input.take(samples * IQ_SAMPLE_BYTES);
    task.advance(&mut bounded, stop)?;
    ensure!(
        bounded.limit() == 0,
        "replay left part of an admitted IQ prefix unread"
    );
    Ok(())
}

/// Replay an analyzer from ordinary file descriptors without buffering prefixes.
///
/// # Errors
/// Returns a source admission, replay, interruption or integrity error.
pub(in crate::triage) fn replay_standalone(
    task: &mut dyn Task<'_>,
    stop: &AtomicBool,
) -> Result<()> {
    while let Some(request) = task.next_request() {
        check_stop(stop)?;
        if request.samples == 0 {
            replay_prefix(task, &mut io::empty(), 0, stop)?;
        } else {
            let mut source = request.tune.open_iq()?;
            replay_prefix(task, &mut source, request.samples, stop)?;
            request.tune.verify_iq(&source)?;
        }
    }
    check_stop(stop)
}

#[derive(Clone, Copy, Default, Serialize)]
pub(in crate::triage) struct Work {
    source_iq_bytes: u64,
    max_buffered_iq_bytes: u64,
    shared_dwells: usize,
}

struct Dwell<'a> {
    tune: &'a Tune,
    ordinary_prefixes: Vec<u64>,
    streaming_prefixes: Vec<u64>,
    buffered_samples: u64,
    streamed_samples: u64,
}

fn common_long_prefix(prefixes: &[u64]) -> u64 {
    let mut positive = prefixes.iter().copied().filter(|&samples| samples > 0);
    match positive.next() {
        Some(samples)
            if samples > MAX_SHARED_IQ_SAMPLES
                && positive.next() == Some(samples)
                && positive.all(|other| other == samples) =>
        {
            samples
        }
        _ => 0,
    }
}

#[derive(Default)]
pub(in crate::triage) struct Plan<'a> {
    dwells: BTreeMap<DwellKey<'a>, Dwell<'a>>,
}

impl<'a> Plan<'a> {
    /// Add routes without opening any IQ payload.
    pub(in crate::triage) fn add(
        &mut self,
        requests: impl Iterator<Item = Request<'a>>,
    ) -> Result<()> {
        self.add_routes(requests, false)
    }

    /// Add routes that can consume the same validated samples together.
    pub(in crate::triage) fn add_streaming(
        &mut self,
        requests: impl Iterator<Item = Request<'a>>,
    ) -> Result<()> {
        self.add_routes(requests, true)
    }

    fn add_routes(
        &mut self,
        requests: impl Iterator<Item = Request<'a>>,
        streaming: bool,
    ) -> Result<()> {
        for request in requests {
            ensure!(
                request.samples <= request.tune.samples,
                "replay prefix exceeds its admitted dwell"
            );
            let dwell = self.dwells.entry(request.key()).or_insert_with(|| Dwell {
                tune: request.tune,
                ordinary_prefixes: Vec::new(),
                streaming_prefixes: Vec::new(),
                buffered_samples: 0,
                streamed_samples: 0,
            });
            ensure!(
                std::ptr::eq(dwell.tune, request.tune),
                "conflicting replay dwell identity"
            );
            let prefixes = if streaming {
                &mut dwell.streaming_prefixes
            } else {
                &mut dwell.ordinary_prefixes
            };
            prefixes.push(request.samples);
        }
        Ok(())
    }

    /// Schedule each long streaming group as one reader for prefix sharing.
    /// Ordinary readers can share up to 40 MB common to their source prefixes.
    pub(in crate::triage) fn prepare(&mut self) -> Work {
        let mut work = Work::default();
        for dwell in self.dwells.values_mut() {
            dwell.ordinary_prefixes.sort_unstable();
            dwell.streaming_prefixes.sort_unstable();
            dwell.streamed_samples = common_long_prefix(&dwell.streaming_prefixes);
            let stream_reads = if dwell.streamed_samples == 0 {
                dwell.streaming_prefixes.as_slice()
            } else {
                std::slice::from_ref(&dwell.streamed_samples)
            };
            let mut source_prefixes: Vec<_> = dwell
                .ordinary_prefixes
                .iter()
                .chain(stream_reads)
                .copied()
                .collect();
            source_prefixes.sort_unstable();
            let overlap = source_prefixes.iter().rev().nth(1).copied().unwrap_or(0);
            let buffered = overlap.min(MAX_SHARED_IQ_SAMPLES);
            dwell.buffered_samples = buffered;
            let tail_samples: u64 = source_prefixes
                .iter()
                .map(|&length| length.saturating_sub(buffered))
                .sum();
            work.source_iq_bytes += (buffered + tail_samples) * IQ_SAMPLE_BYTES;
            let streamed = dwell.streamed_samples > 0;
            let stream_block = if streamed { READ_BLOCK_BYTES as u64 } else { 0 };
            work.max_buffered_iq_bytes = work
                .max_buffered_iq_bytes
                .max(buffered * IQ_SAMPLE_BYTES + stream_block);
            work.shared_dwells += usize::from(buffered > 0 || streamed);
        }
        work
    }

    /// Replay in dwell order. Eligible streaming routes read the dwell once
    /// as a group and can share the common prefix with ordinary routes.
    ///
    /// # Errors
    /// Returns an error for changed or invalid IQ, interruption, or a mismatch
    /// between a planned and consumed source prefix.
    pub(in crate::triage) fn replay(
        &self,
        tasks: &mut [&mut dyn Task<'a>],
        stream_tasks: &mut [&mut dyn StreamingTask<'a>],
        stop: &AtomicBool,
    ) -> Result<()> {
        let mut buffer = Vec::new();
        for (&key, dwell) in &self.dwells {
            check_stop(stop)?;
            let mut active = Vec::new();
            if dwell.streamed_samples > 0 {
                let request = Request {
                    pass_index: key.0,
                    tune: dwell.tune,
                    samples: dwell.streamed_samples,
                };
                let mut positive = 0;
                let mut short = 0;
                for task in &mut *stream_tasks {
                    let (task_positive, task_short) = task.begin_dwell(request, stop)?;
                    ensure!(
                        task.next_request().is_none_or(|next| next.key() > key),
                        "streaming replay left a route at the current or an earlier dwell"
                    );
                    positive += task_positive;
                    short += task_short;
                    if task_positive > 0 {
                        active.push(&mut **task);
                    }
                }
                let planned_short = dwell
                    .streaming_prefixes
                    .partition_point(|&samples| samples == 0);
                ensure!(
                    (positive, short)
                        == (
                            dwell.streaming_prefixes.len() - planned_short,
                            planned_short
                        ),
                    "streaming replay routes differ from the admitted dwell"
                );
            }

            let needs_source = [&dwell.ordinary_prefixes, &dwell.streaming_prefixes]
                .iter()
                .any(|prefixes| prefixes.last().is_some_and(|&samples| samples > 0));
            let mut source = needs_source.then(|| dwell.tune.open_iq()).transpose()?;
            let buffered_bytes = (dwell.buffered_samples * IQ_SAMPLE_BYTES) as usize;
            // Release an oversized previous prefix before allocating a stream
            // block so the live allocation follows the planned peak.
            if dwell.streamed_samples > 0 && buffer.capacity() > buffered_bytes {
                buffer = Vec::new();
            }
            buffer.try_reserve_exact(buffered_bytes.saturating_sub(buffer.len()))?;
            buffer.resize(buffered_bytes, 0);
            if let Some(file) = &mut source {
                for block in buffer.chunks_mut(READ_BLOCK_BYTES) {
                    check_stop(stop)?;
                    file.read_exact(block).context("read shared IQ prefix")?;
                }
            }
            if dwell.streamed_samples > 0 {
                let file = source
                    .as_mut()
                    .context("missing admitted streaming IQ descriptor")?;
                let mut input = buffer.as_slice().chain(file);
                let digest = receiver::read_q11_prefix(
                    &mut input,
                    dwell.streamed_samples,
                    stop,
                    |sample, index| {
                        for task in &mut active {
                            task.push_sample(sample, index);
                        }
                        Ok(())
                    },
                )?;
                for task in &mut active {
                    task.finish_dwell(&digest, stop)?;
                }
            }
            drop(active);

            let mut replay = BufferedDwell {
                key,
                tune: dwell.tune,
                source: source.as_mut(),
                prefix: &buffer,
                stop,
            };
            let mut consumed = Vec::with_capacity(dwell.ordinary_prefixes.len());
            for task in &mut *tasks {
                replay.advance(*task, &dwell.ordinary_prefixes, &mut consumed)?;
            }
            consumed.sort_unstable();
            ensure!(
                consumed == dwell.ordinary_prefixes,
                "replay did not consume its planned routes"
            );
            if dwell.streamed_samples == 0 {
                consumed.clear();
                for task in &mut *stream_tasks {
                    replay.advance(*task, &dwell.streaming_prefixes, &mut consumed)?;
                }
                consumed.sort_unstable();
                ensure!(
                    consumed == dwell.streaming_prefixes,
                    "replay did not consume its planned streaming routes"
                );
            }
            if let Some(file) = source {
                dwell.tune.verify_iq(&file)?;
            }
        }
        ensure!(
            tasks.iter().all(|task| task.next_request().is_none())
                && stream_tasks
                    .iter()
                    .all(|task| task.next_request().is_none()),
            "unplanned replay routes remain"
        );
        check_stop(stop)
    }
}

struct BufferedDwell<'reader, 'capture> {
    key: DwellKey<'capture>,
    tune: &'capture Tune,
    source: Option<&'reader mut File>,
    prefix: &'reader [u8],
    stop: &'reader AtomicBool,
}

impl<'capture> BufferedDwell<'_, 'capture> {
    fn advance(
        &mut self,
        task: &mut dyn Task<'capture>,
        planned: &[u64],
        consumed: &mut Vec<u64>,
    ) -> Result<()> {
        while let Some(request) = task.next_request().filter(|item| item.key() == self.key) {
            ensure!(
                std::ptr::eq(self.tune, request.tune)
                    && planned.binary_search(&request.samples).is_ok(),
                "replay route differs from its admitted IQ prefix"
            );
            if request.samples * IQ_SAMPLE_BYTES <= self.prefix.len() as u64 {
                let mut input = self.prefix;
                replay_prefix(task, &mut input, request.samples, self.stop)?;
            } else {
                let file = self
                    .source
                    .as_deref_mut()
                    .context("missing admitted IQ descriptor")?;
                file.seek(SeekFrom::Start(self.prefix.len() as u64))?;
                let mut input = self.prefix.chain(file);
                replay_prefix(task, &mut input, request.samples, self.stop)?;
            }
            consumed.push(request.samples);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SHARED_IQ_SAMPLES, common_long_prefix};

    #[test]
    fn streams_only_matching_long_prefixes_with_multiple_eligible_routes() {
        let limit = MAX_SHARED_IQ_SAMPLES;
        assert_eq!(common_long_prefix(&[0, 0]), 0);
        assert_eq!(common_long_prefix(&[0, limit + 1]), 0);
        assert_eq!(common_long_prefix(&[limit, limit]), 0);
        assert_eq!(common_long_prefix(&[limit + 1, limit + 1, limit + 2]), 0);
        assert_eq!(common_long_prefix(&[0, limit + 1, limit + 1]), limit + 1);
    }
}
