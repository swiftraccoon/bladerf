//! Assemble contiguous segments from timestamped 20 MSps reads.

use anyhow::{Context, Result, ensure};

pub(super) const SAMPLE_PERIOD_NS: i64 = 50;

pub(super) struct ReadBlock {
    pub(super) count: usize,
    pub(super) timestamp_ns: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SegmentTiming {
    pub(super) first_sample_ns: i64,
    pub(super) end_sample_exclusive_ns: i64,
}

pub(super) enum SegmentEvent<'a, T> {
    Samples(&'a [T]),
    Closed {
        index: usize,
        timing: SegmentTiming,
        finished_on_request: bool,
    },
}

pub(super) struct SegmentPlan {
    pub(super) segment_samples: usize,
    pub(super) segment_count: Option<usize>,
    pub(super) discard_samples: usize,
    pub(super) witness_samples: usize,
}

fn read_adjacent<T>(
    buffer: &mut [T],
    expected: Option<i64>,
    read: &mut impl FnMut(&mut [T]) -> Result<ReadBlock>,
) -> Result<ReadBlock> {
    let block = read(buffer)?;
    ensure!(
        (1..=buffer.len()).contains(&block.count),
        "RX returned {} samples for a request of {}",
        block.count,
        buffer.len()
    );
    if let Some(expected) = expected {
        ensure!(
            block.timestamp_ns == expected,
            "RX timestamp discontinuity, expected {expected} ns and received {} ns",
            block.timestamp_ns
        );
    }
    Ok(block)
}

fn read_samples<T>(
    buffer: &mut [T],
    mut remaining: usize,
    next_timestamp_ns: &mut Option<i64>,
    read: &mut impl FnMut(&mut [T]) -> Result<ReadBlock>,
    mut consume: impl FnMut(&[T], SegmentTiming) -> Result<()>,
) -> Result<()> {
    while remaining > 0 {
        let wanted = remaining.min(buffer.len());
        let block = read_adjacent(&mut buffer[..wanted], *next_timestamp_ns, read)?;
        let duration_ns = i64::try_from(block.count)?
            .checked_mul(SAMPLE_PERIOD_NS)
            .context("RX timestamp duration overflow")?;
        let end_sample_exclusive_ns = block
            .timestamp_ns
            .checked_add(duration_ns)
            .context("RX timestamp end overflow")?;
        *next_timestamp_ns = Some(end_sample_exclusive_ns);
        consume(
            &buffer[..block.count],
            SegmentTiming {
                first_sample_ns: block.timestamp_ns,
                end_sample_exclusive_ns,
            },
        )?;
        remaining -= block.count;
    }
    Ok(())
}

/// Forward fixed-size, zero-indexed segments after an optional contiguous discard.
///
/// The reader supplies the first sample's validated hardware timestamp for every
/// positive read. Each segment stays provisional until the next read starts
/// at its exclusive end. The final witness is read separately and never stored.
/// With no segment limit, check for finish requests after each complete segment.
/// A read, timestamp, finish request, or sink error stops without closing an
/// unwitnessed segment. The sink owns provisional output and its cleanup.
pub(super) fn stream_segments<T>(
    buffer: &mut [T],
    plan: SegmentPlan,
    mut read: impl FnMut(&mut [T]) -> Result<ReadBlock>,
    mut sink: impl FnMut(SegmentEvent<'_, T>) -> Result<()>,
    mut finish_requested: impl FnMut() -> Result<bool>,
) -> Result<usize> {
    ensure!(
        plan.segment_samples > 0 && plan.segment_count != Some(0),
        "segment sample count and segment count must be positive"
    );
    ensure!(
        (1..=buffer.len()).contains(&plan.witness_samples),
        "final witness sample request must fit the read buffer"
    );
    let mut next_timestamp_ns = None;
    read_samples(
        buffer,
        plan.discard_samples,
        &mut next_timestamp_ns,
        &mut read,
        |_, _| Ok(()),
    )?;

    let mut pending = None;
    let mut index = 0usize;
    loop {
        let mut timing = None;
        read_samples(
            buffer,
            plan.segment_samples,
            &mut next_timestamp_ns,
            &mut read,
            |samples, chunk_timing| {
                if let Some((prev_index, prev_timing)) = pending.take() {
                    sink(SegmentEvent::Closed {
                        index: prev_index,
                        timing: prev_timing,
                        finished_on_request: false,
                    })?;
                }
                timing.get_or_insert(chunk_timing).end_sample_exclusive_ns =
                    chunk_timing.end_sample_exclusive_ns;
                sink(SegmentEvent::Samples(samples))
            },
        )?;
        let timing = timing.context("completed segment had no samples")?;
        let completed = index.checked_add(1).context("segment ordinal overflow")?;
        let finished_on_request = plan.segment_count.is_none() && finish_requested()?;
        if plan.segment_count == Some(completed) || finished_on_request {
            read_adjacent(
                &mut buffer[..plan.witness_samples],
                next_timestamp_ns,
                &mut read,
            )?;
            sink(SegmentEvent::Closed {
                index,
                timing,
                finished_on_request,
            })?;
            return Ok(completed);
        }
        pending = Some((index, timing));
        index = completed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    enum Event {
        Samples(Vec<usize>),
        Closed(usize, SegmentTiming),
        Finished(usize, SegmentTiming),
    }

    fn block(count: usize, timestamp_ns: i64) -> Result<ReadBlock> {
        Ok(ReadBlock {
            count,
            timestamp_ns,
        })
    }

    fn simulate(
        segment_samples: usize,
        segment_count: usize,
        discard_samples: usize,
        blocks: impl IntoIterator<Item = Result<ReadBlock>>,
    ) -> (Vec<usize>, Vec<Event>, Result<usize>) {
        simulate_stream(
            segment_samples,
            Some(segment_count),
            discard_samples,
            blocks,
            || panic!("bounded streams should not poll for a finish request"),
        )
    }

    fn simulate_stream(
        segment_samples: usize,
        segment_count: Option<usize>,
        discard_samples: usize,
        blocks: impl IntoIterator<Item = Result<ReadBlock>>,
        finish_requested: impl FnMut() -> Result<bool>,
    ) -> (Vec<usize>, Vec<Event>, Result<usize>) {
        let mut buffer = [0; 4];
        let mut blocks = blocks.into_iter();
        let mut requests = Vec::new();
        let mut events = Vec::new();
        let mut sample = 0;
        let result = stream_segments(
            &mut buffer,
            SegmentPlan {
                segment_samples,
                segment_count,
                discard_samples,
                witness_samples: 4,
            },
            |buffer| {
                requests.push(buffer.len());
                let block = blocks.next().expect("unexpected read")?;
                for slot in buffer.iter_mut().take(block.count) {
                    *slot = sample;
                    sample += 1;
                }
                Ok(block)
            },
            |event| {
                events.push(match event {
                    SegmentEvent::Samples(samples) => Event::Samples(samples.to_vec()),
                    SegmentEvent::Closed {
                        index,
                        timing,
                        finished_on_request,
                    } => {
                        if finished_on_request {
                            Event::Finished(index, timing)
                        } else {
                            Event::Closed(index, timing)
                        }
                    }
                });
                Ok(())
            },
            finish_requested,
        );
        (requests, events, result)
    }

    #[test]
    fn clips_reads_and_closes_segments_only_after_boundary_witnesses() {
        let (requests, events, result) = simulate(
            5,
            2,
            3,
            [
                block(2, 0),
                block(1, 100),
                block(3, 150),
                block(2, 300),
                block(4, 400),
                block(1, 600),
                block(1, 650),
            ],
        );
        assert_eq!(result.unwrap(), 2);
        assert_eq!(requests, [3, 1, 4, 2, 4, 1, 4]);
        assert_eq!(
            events,
            [
                Event::Samples(vec![3, 4, 5]),
                Event::Samples(vec![6, 7]),
                Event::Closed(
                    0,
                    SegmentTiming {
                        first_sample_ns: 150,
                        end_sample_exclusive_ns: 400
                    },
                ),
                Event::Samples(vec![8, 9, 10, 11]),
                Event::Samples(vec![12]),
                Event::Closed(
                    1,
                    SegmentTiming {
                        first_sample_ns: 400,
                        end_sample_exclusive_ns: 650
                    },
                ),
            ]
        );
    }

    #[test]
    fn boundary_gaps_prevent_writes_and_closure() {
        let (_, events, result) = simulate(2, 2, 0, [block(2, 0), block(2, 150)]);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("timestamp discontinuity")
        );
        assert_eq!(events, [Event::Samples(vec![0, 1])]);

        let (_, events, result) = simulate(2, 1, 1, [block(1, 0), block(2, 100)]);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("timestamp discontinuity")
        );
        assert!(events.is_empty());
    }

    #[test]
    fn invalid_final_witness_keeps_the_last_segment_provisional() {
        for (witness, reason) in [
            (Err(anyhow::anyhow!("overflow")), "overflow"),
            (block(1, 250), "timestamp discontinuity"),
        ] {
            let (requests, events, result) =
                simulate(2, 2, 0, [block(2, 0), block(2, 100), witness]);
            assert!(result.unwrap_err().to_string().contains(reason));
            assert_eq!(requests, [2, 2, 4]);
            assert_eq!(
                events,
                [
                    Event::Samples(vec![0, 1]),
                    Event::Closed(
                        0,
                        SegmentTiming {
                            first_sample_ns: 0,
                            end_sample_exclusive_ns: 100
                        },
                    ),
                    Event::Samples(vec![2, 3]),
                ]
            );
        }
    }

    #[test]
    fn invalid_reads_fail_before_reaching_the_sink() {
        for invalid in [block(0, 0), block(3, 0), block(2, i64::MAX - 50)] {
            let (_, events, result) = simulate(2, 1, 0, [invalid]);
            assert!(result.is_err());
            assert!(events.is_empty());
        }
    }

    #[test]
    fn finish_request_closes_only_the_witnessed_final_segment() {
        let mut requests = [false, true].into_iter();
        let (reads, events, result) = simulate_stream(
            2,
            None,
            0,
            [block(2, 0), block(2, 100), block(1, 200)],
            || Ok(requests.next().expect("unexpected finish poll")),
        );
        assert_eq!(result.unwrap(), 2);
        assert_eq!(reads, [2, 2, 4]);
        assert_eq!(
            events,
            [
                Event::Samples(vec![0, 1]),
                Event::Closed(
                    0,
                    SegmentTiming {
                        first_sample_ns: 0,
                        end_sample_exclusive_ns: 100
                    }
                ),
                Event::Samples(vec![2, 3]),
                Event::Finished(
                    1,
                    SegmentTiming {
                        first_sample_ns: 100,
                        end_sample_exclusive_ns: 200
                    }
                ),
            ]
        );
    }

    #[test]
    fn finish_request_with_a_missing_witness_keeps_the_segment_provisional() {
        let (reads, events, result) =
            simulate_stream(2, None, 0, [block(2, 0), block(0, 100)], || Ok(true));
        assert!(result.unwrap_err().to_string().contains("RX returned 0"));
        assert_eq!(reads, [2, 4]);
        assert_eq!(events, [Event::Samples(vec![0, 1])]);
    }

    #[test]
    fn invalid_finish_request_fails_without_reading_a_witness() {
        let (reads, events, result) = simulate_stream(2, None, 0, [block(2, 0)], || {
            Err(anyhow::anyhow!("invalid marker"))
        });
        assert!(result.unwrap_err().to_string().contains("invalid marker"));
        assert_eq!(reads, [2]);
        assert_eq!(events, [Event::Samples(vec![0, 1])]);
    }
}
