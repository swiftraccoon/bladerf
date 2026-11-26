//! Stream an admitted, channel-centered CF32 file through a bounded source.

use std::fs::{File, Metadata, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result as InputResult, ensure};
use data_encoding::HEXLOWER;
use futuresdr::runtime::dev::prelude::*;
use sha2::{Digest, Sha256};

const SAMPLE_BYTES: usize = 8;
const BLOCK_SAMPLES: usize = 8192;
const MAX_SAMPLES: u64 = 200_000_000;

pub(super) struct Input {
    pub(super) sample_count: u64,
    path: PathBuf,
    file: File,
    stamp: Metadata,
}

impl Input {
    /// Admit the direct descriptor that will supply every sample to the graph.
    pub(super) fn open(path: &Path) -> InputResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .context("open direct CF32_LE input")?;
        let stamp = file.metadata()?;
        let byte_count = stamp.len();
        let sample_count = byte_count / SAMPLE_BYTES as u64;
        ensure!(
            stamp.is_file()
                && byte_count.is_multiple_of(SAMPLE_BYTES as u64)
                && (1..=MAX_SAMPLES).contains(&sample_count),
            "input must be a direct regular file of 1..={MAX_SAMPLES} complete CF32_LE samples"
        );

        let input = Self {
            sample_count,
            path: path.to_path_buf(),
            file,
            stamp,
        };
        input.verify()?;
        Ok(input)
    }

    /// Recheck the original descriptor and path after processing.
    fn verify(&self) -> InputResult<()> {
        let on_disk = self
            .path
            .symlink_metadata()
            .context("recheck CF32_LE path")?;
        let descriptor = self.file.metadata().context("recheck CF32_LE descriptor")?;
        ensure!(
            same_file(&self.stamp, &descriptor) && same_file(&self.stamp, &on_disk),
            "CF32_LE input changed after admission"
        );
        Ok(())
    }
}

/// Proof that the source produced the entire admitted extent and reached EOF.
pub(super) struct CompletedInput<'a> {
    input: &'a Input,
    pub(super) sample_count: u64,
    pub(super) sha256: &'a str,
}

impl CompletedInput<'_> {
    /// Recheck the same descriptor and path immediately before publication.
    pub(super) fn verify(&self) -> InputResult<()> {
        self.input.verify()
    }
}

/// Emit finite complex samples from the admitted descriptor in fixed-size chunks.
#[derive(Block)]
pub(super) struct Cf32LeSource {
    input: Input,
    buffer: Vec<u8>,
    digest: Sha256,
    samples_read: u64,
    source_sha256: Option<String>,
    #[output]
    output: DefaultCpuWriter<Complex32>,
}

impl Cf32LeSource {
    pub(super) fn new(input: Input) -> Self {
        Self {
            input,
            buffer: vec![0; BLOCK_SAMPLES * SAMPLE_BYTES],
            digest: Sha256::new(),
            samples_read: 0,
            source_sha256: None,
            output: DefaultCpuWriter::default(),
        }
    }

    /// Recover the verified extent and digest after the graph has terminated.
    pub(super) fn completed(&self) -> InputResult<CompletedInput<'_>> {
        let sha256 = self
            .source_sha256
            .as_deref()
            .context("CF32_LE source did not reach verified EOF")?;
        ensure!(
            self.samples_read == self.input.sample_count,
            "CF32_LE source did not produce its complete admitted extent"
        );
        self.input.verify()?;
        Ok(CompletedInput {
            input: &self.input,
            sample_count: self.samples_read,
            sha256,
        })
    }

    fn finish(&mut self) -> InputResult<()> {
        match self.input.file.read_exact(&mut [0]) {
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(error) => return Err(error).context("check exact CF32_LE EOF"),
            Ok(()) => anyhow::bail!("CF32_LE source grew while decoding"),
        }
        self.input.verify()?;
        self.source_sha256 = Some(HEXLOWER.encode(&std::mem::take(&mut self.digest).finalize()));
        Ok(())
    }
}

impl Kernel for Cf32LeSource {
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _outputs: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        let output = self.output.slice();
        let output_capacity = output.len();
        let available = (self.input.sample_count - self.samples_read).min(BLOCK_SAMPLES as u64);
        let count = output_capacity.min(available as usize);
        if count == 0 {
            return Ok(());
        }
        let bytes = &mut self.buffer[..count * SAMPLE_BYTES];
        self.input
            .file
            .read_exact(bytes)
            .context("read complete CF32_LE samples")?;
        for (slot, &[a, b, c, d, e, f, g, h]) in
            output.iter_mut().zip(bytes.as_chunks::<SAMPLE_BYTES>().0)
        {
            let re = f32::from_le_bytes([a, b, c, d]);
            let im = f32::from_le_bytes([e, f, g, h]);
            ensure!(
                re.is_finite() && im.is_finite(),
                "input contains a non-finite CF32_LE component"
            );
            *slot = Complex32::new(re, im);
        }
        self.digest.update(bytes);
        self.samples_read += count as u64;
        if self.samples_read == self.input.sample_count {
            self.finish()?;
            io.finished = true;
        }
        self.output.produce(count);
        // A synchronizer can wait for more than one chunk before consuming.
        io.call_again = !io.finished && count < output_capacity;
        Ok(())
    }
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::time::Duration;

    use futuresdr::prelude::{Flowgraph, Runtime, TerminatedFlowgraph};
    use futuresdr::runtime::Timer;
    use futuresdr::runtime::scheduler::SmolScheduler;

    const MIN_WINDOW_ITEMS: usize = 2 * BLOCK_SAMPLES + 2;

    #[derive(Block)]
    struct WindowSink {
        items: Vec<Complex32>,
        #[input]
        input: DefaultCpuReader<Complex32>,
    }

    impl WindowSink {
        fn new() -> Self {
            let mut input = DefaultCpuReader::default();
            input.set_min_items(MIN_WINDOW_ITEMS);
            Self {
                items: Vec::new(),
                input,
            }
        }
    }

    impl Kernel for WindowSink {
        async fn work(
            &mut self,
            io: &mut WorkIo,
            _outputs: &mut MessageOutputs,
            _meta: &BlockMeta,
        ) -> Result<()> {
            let finished = self.input.finished();
            let samples = self.input.slice();
            let count = samples.len();
            if count < MIN_WINDOW_ITEMS && !finished {
                return Ok(());
            }
            self.items.extend_from_slice(samples);
            self.input.consume(count);
            io.finished = finished;
            Ok(())
        }
    }

    fn replay(
        input: Input,
    ) -> InputResult<(
        TerminatedFlowgraph,
        BlockRef<Cf32LeSource>,
        BlockRef<WindowSink>,
    )> {
        let mut graph = Flowgraph::new();
        let source = graph.add(Cf32LeSource::new(input))?;
        let sink = graph.add(WindowSink::new())?;
        graph.stream_dyn(source, "output", sink, "input")?;
        let runtime = Runtime::with_scheduler(SmolScheduler::with_config(1, false));
        let running = runtime.start(graph)?;
        let handle = running.handle();
        let deadline = runtime.spawn(async move {
            Timer::after(Duration::from_secs(3)).await;
            let _ = handle.stop().await;
        });
        let terminated = running.wait()?;
        drop(deadline);
        Ok((terminated, source, sink))
    }

    #[test]
    fn streams_complete_blocks_and_tail_with_exact_digest() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let value = Complex32::new(-1.5, 0.25);
        let mut sample = Vec::from(value.re.to_le_bytes());
        sample.extend_from_slice(&value.im.to_le_bytes());
        let sample_count = MIN_WINDOW_ITEMS + 7;
        let bytes = sample.repeat(sample_count);
        file.write_all(&bytes).unwrap();

        let input = Input::open(file.path()).unwrap();
        let (graph, source, sink) = replay(input).unwrap();
        {
            let sink = graph.block(&sink).unwrap();
            assert_eq!(sink.items.len(), sample_count);
            assert!(sink.items.iter().all(|&item| item == value));
        }
        let source = graph.block(&source).unwrap();
        let complete = source.completed().unwrap();
        assert_eq!(complete.sample_count, sample_count as u64);
        assert_eq!(complete.sha256, HEXLOWER.encode(&Sha256::digest(&bytes)));

        file.write_all(&[0]).unwrap();
        assert!(complete.verify().is_err());
    }

    #[test]
    fn rejects_changed_extent_and_nonfinite_tail_during_work() {
        let mut grew = tempfile::NamedTempFile::new().unwrap();
        grew.write_all(&[0; SAMPLE_BYTES]).unwrap();
        let input = Input::open(grew.path()).unwrap();
        grew.write_all(&[0]).unwrap();
        assert!(replay(input).is_err());

        let mut shrank = tempfile::NamedTempFile::new().unwrap();
        shrank.write_all(&[0; SAMPLE_BYTES * 2]).unwrap();
        let input = Input::open(shrank.path()).unwrap();
        shrank.as_file().set_len(SAMPLE_BYTES as u64).unwrap();
        assert!(replay(input).is_err());

        let mut nonfinite = tempfile::NamedTempFile::new().unwrap();
        nonfinite
            .write_all(&vec![0; BLOCK_SAMPLES * SAMPLE_BYTES])
            .unwrap();
        nonfinite.write_all(&f32::NAN.to_le_bytes()).unwrap();
        nonfinite.write_all(&0.0f32.to_le_bytes()).unwrap();
        let input = Input::open(nonfinite.path()).unwrap();
        assert!(replay(input).is_err());
    }

    #[test]
    fn admits_only_a_complete_extent_within_the_sample_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let limit_bytes = MAX_SAMPLES * SAMPLE_BYTES as u64;
        file.as_file().set_len(limit_bytes).unwrap();
        let input = Input::open(file.path()).unwrap();
        assert_eq!(input.sample_count, MAX_SAMPLES);
        assert!(Cf32LeSource::new(input).completed().is_err());

        file.as_file()
            .set_len(limit_bytes + SAMPLE_BYTES as u64)
            .unwrap();
        assert!(Input::open(file.path()).is_err());
        file.as_file().set_len(SAMPLE_BYTES as u64 - 1).unwrap();
        assert!(Input::open(file.path()).is_err());
    }
}
