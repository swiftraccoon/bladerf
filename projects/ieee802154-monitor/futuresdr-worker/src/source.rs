//! Safe finite CF32 little-endian source block for the file-only worker.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, ensure};
use futuresdr::runtime::dev::prelude::*;

const MAX_ITEMS_PER_WORK: usize = 8192;

pub(crate) fn validate_cf32_le(file: &mut File, samples: u64) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = samples;
    let mut bytes = vec![0u8; MAX_ITEMS_PER_WORK * 8];
    while remaining > 0 {
        let count = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(MAX_ITEMS_PER_WORK);
        let byte_count = count
            .checked_mul(8)
            .context("CF32 validation byte count overflow")?;
        file.read_exact(&mut bytes[..byte_count])
            .context("read complete CF32_LE samples during validation")?;
        ensure_finite_cf32_le(&bytes[..byte_count])?;
        remaining -= count as u64;
    }
    let mut trailing = [0u8; 1];
    ensure!(
        file.read(&mut trailing)? == 0,
        "CF32_LE input grew during validation"
    );
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn ensure_finite_cf32_le(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len().is_multiple_of(8),
        "CF32_LE byte buffer is not sample-aligned"
    );
    for raw in bytes.as_chunks::<8>().0 {
        let re = f32::from_le_bytes(raw[..4].try_into().expect("fixed CF32 component"));
        let im = f32::from_le_bytes(raw[4..].try_into().expect("fixed CF32 component"));
        ensure!(
            re.is_finite() && im.is_finite(),
            "CF32_LE source contains a non-finite component"
        );
    }
    Ok(())
}

#[derive(Block)]
pub(crate) struct Cf32LeSource<O = DefaultCpuWriter<Complex32>>
where
    O: CpuBufferWriter<Item = Complex32>,
{
    file: File,
    remaining: u64,
    #[output]
    output: O,
}

impl Cf32LeSource<DefaultCpuWriter<Complex32>> {
    pub(crate) fn new(file: File, samples: u64) -> Self {
        Self {
            file,
            remaining: samples,
            output: DefaultCpuWriter::default(),
        }
    }
}

impl<O> Kernel for Cf32LeSource<O>
where
    O: CpuBufferWriter<Item = Complex32>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        if self.remaining == 0 {
            io.finished = true;
            return Ok(());
        }
        let output = self.output.slice();
        let count = output
            .len()
            .min(MAX_ITEMS_PER_WORK)
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if count == 0 {
            return Ok(());
        }
        let byte_count = count
            .checked_mul(8)
            .context("CF32 source byte count overflow")?;
        let mut bytes = vec![0u8; byte_count];
        self.file
            .read_exact(&mut bytes)
            .context("read complete CF32_LE source samples")?;
        ensure_finite_cf32_le(&bytes)?;
        for (slot, raw) in output[..count].iter_mut().zip(bytes.as_chunks::<8>().0) {
            let re = f32::from_le_bytes(raw[..4].try_into().expect("fixed CF32 component"));
            let im = f32::from_le_bytes(raw[4..].try_into().expect("fixed CF32 component"));
            *slot = Complex32::new(re, im);
        }
        self.remaining -= count as u64;
        self.output.produce(count);
        if self.remaining == 0 {
            let mut trailing = [0u8; 1];
            ensure!(
                self.file.read(&mut trailing)? == 0,
                "CF32_LE source grew while decoding"
            );
            io.finished = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn constants_bound_each_source_work_call() {
        assert_eq!(MAX_ITEMS_PER_WORK * 8, 64 * 1024);
    }

    #[test]
    fn validates_framing_and_finite_components_and_rewinds() {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(&1.25f32.to_le_bytes()).unwrap();
        input.write_all(&(-0.5f32).to_le_bytes()).unwrap();
        validate_cf32_le(&mut input, 1).unwrap();
        assert_eq!(input.stream_position().unwrap(), 0);

        let mut non_finite = tempfile::tempfile().unwrap();
        non_finite.write_all(&f32::NAN.to_le_bytes()).unwrap();
        non_finite.write_all(&0.0f32.to_le_bytes()).unwrap();
        assert!(validate_cf32_le(&mut non_finite, 1).is_err());

        let mut truncated = tempfile::tempfile().unwrap();
        truncated.write_all(&[0; 7]).unwrap();
        assert!(validate_cf32_le(&mut truncated, 1).is_err());
    }
}
