//! Stream, verify and publish a slice directory on the destination filesystem.

use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{private_output::StagedDirectory, receiver, spectrum::EventStride};

use super::check_stop;
use super::input::{Input, same_extent_and_times};
use super::plan::{ALIGNMENT_SAMPLES, Limits, Span};

const COMPLEX_BYTES: u64 = 4;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct Slice {
    iq_file: String,
    #[serde(flatten)]
    span: Span,
    iq_sha256: String,
}

#[derive(Serialize)]
struct Manifest<'a> {
    schema_version: u32,
    operation: &'static str,
    source_format: &'static str,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    input_samples: u64,
    analyzed_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_window_stride_samples: Option<usize>,
    analyzed_prefix_sha256: &'a str,
    event_report_sha256: &'a str,
    acquisition_sha256: &'a str,
    event_count: usize,
    omitted_component_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_window_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_window_range_count: Option<usize>,
    alignment_samples: u64,
    alignment_omitted_tail_samples: u64,
    limits: Limits,
    exported_samples: u64,
    slices: Vec<Slice>,
}

struct PrefixReader<'a> {
    file: &'a mut File,
    buffer: Vec<u8>,
    digest: Sha256,
    position_samples: u64,
}

impl<'a> PrefixReader<'a> {
    fn new(file: &'a mut File) -> Self {
        Self {
            file,
            buffer: vec![0; CHUNK_BYTES],
            digest: Sha256::new(),
            position_samples: 0,
        }
    }

    fn consume(
        &mut self,
        end_sample_exclusive: u64,
        stop: &AtomicBool,
        mut sink: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            self.position_samples <= end_sample_exclusive,
            "IQ slice plan is out of order"
        );
        while self.position_samples < end_sample_exclusive {
            check_stop(stop)?;
            let count = (end_sample_exclusive - self.position_samples)
                .min(CHUNK_BYTES as u64 / COMPLEX_BYTES);
            let bytes = &mut self.buffer[..(count * COMPLEX_BYTES) as usize];
            self.file
                .read_exact(bytes)
                .context("read analyzed IQ prefix")?;
            ensure!(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|component| { (-2048..=2047).contains(&i16::from_le_bytes(*component)) }),
                "analyzed IQ prefix contains an out-of-range SC16 Q11 component"
            );
            self.digest.update(&*bytes);
            sink(bytes)?;
            self.position_samples += count;
        }
        Ok(())
    }

    fn finish(self, before: &Metadata, expected_sha256: &str) -> Result<()> {
        let after = self.file.metadata().context("recheck pinned IQ source")?;
        ensure!(
            same_extent_and_times(before, &after),
            "IQ source changed during slice export"
        );
        ensure!(
            format!("{:x}", self.digest.finalize()) == expected_sha256,
            "analyzed IQ prefix SHA-256 differs from the event report"
        );
        Ok(())
    }
}

pub(super) fn export(
    mut source: Input,
    spans: &[Span],
    limits: Limits,
    destination: &Path,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    let stage = StagedDirectory::new(destination, ".event-slices-")?;

    let mut reader = PrefixReader::new(&mut source.iq);
    let mut slices = Vec::with_capacity(spans.len());
    for (ordinal, span) in spans.iter().copied().enumerate() {
        check_stop(stop)?;
        reader.consume(span.start_sample, stop, |_| Ok(()))?;
        let iq_file = format!("slice-{:04}.sc16q11", ordinal + 1);
        let iq_path = stage.path().join(&iq_file);
        let mut file = crate::new_file(&iq_path)?;
        let mut digest = Sha256::new();
        reader.consume(span.end_sample_exclusive, stop, |bytes| {
            file.write_all(bytes).context("write IQ slice")?;
            digest.update(bytes);
            Ok(())
        })?;
        file.sync_all().context("sync IQ slice")?;
        receiver::write_derived_iq_sidecar(
            &iq_path,
            source.center_frequency_hz,
            "derived_event_slice",
            span.start_sample,
            span.samples(),
        )?;
        slices.push(Slice {
            iq_file,
            span,
            iq_sha256: format!("{:x}", digest.finalize()),
        });
    }
    reader.consume(source.analyzed_samples, stop, |_| Ok(()))?;
    reader.finish(&source.source_metadata, &source.analyzed_prefix_sha256)?;
    check_stop(stop)?;
    let (schema_version, event_window_stride_samples) =
        match (source.active_window_count, source.event_stride) {
            (Some(_), stride) => (3, Some(stride.samples())),
            (None, EventStride::HalfWindow) => (2, Some(EventStride::HalfWindow.samples())),
            (None, EventStride::Contiguous) => (1, None),
        };
    let manifest = Manifest {
        schema_version,
        operation: "offline_rf_event_slice_export",
        source_format: "SC16_Q11_LE",
        center_frequency_hz: source.center_frequency_hz,
        sample_rate_hz: crate::SAMPLE_RATE,
        input_samples: source.input_samples,
        analyzed_samples: source.analyzed_samples,
        event_window_stride_samples,
        analyzed_prefix_sha256: &source.analyzed_prefix_sha256,
        event_report_sha256: &source.event_report_sha256,
        acquisition_sha256: &source.acquisition_sha256,
        event_count: source.event_count,
        omitted_component_count: source.omitted_component_count,
        active_window_count: source.active_window_count,
        active_window_range_count: source
            .active_window_count
            .map(|_| source.replay_spans.len()),
        alignment_samples: ALIGNMENT_SAMPLES,
        alignment_omitted_tail_samples: source.analyzed_samples % ALIGNMENT_SAMPLES,
        limits,
        exported_samples: spans.iter().map(|span| span.samples()).sum(),
        slices,
    };
    crate::json_file(&stage.path().join("slices.json"), &manifest)?;
    stage.publish(|| check_stop(stop))
}
