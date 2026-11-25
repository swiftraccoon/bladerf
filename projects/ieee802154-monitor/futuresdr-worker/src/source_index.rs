//! Bounded sidecar for input positions of archived hard-decoder candidates.
//!
//! The 24-byte I154IDX v1 header and records are little-endian. Each record
//! pairs the archive's one-based recovered chip count with the zero-based
//! CF32 input sample index through which M&M required input at completion.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::archive::append_suffix;

const MAGIC: &[u8; 8] = b"I154IDX\0";
const VERSION: u16 = 1;
const HEADER_BYTES: u16 = 24;
const CF32_SAMPLE_RATE_HZ: u32 = 4_000_000;
const RECOVERED_CHIP_RATE_HZ: u32 = 2_000_000;

#[derive(Debug)]
pub(crate) struct SourceIndexWriter {
    final_path: PathBuf,
    partial_path: PathBuf,
    writer: BufWriter<File>,
    max_frames: u64,
    input_samples: u64,
    records: u64,
    last_chip_count: u64,
    last_required_through: u64,
}

impl SourceIndexWriter {
    /// Stage a private header in a new `.partial` file beside the output.
    pub(crate) fn create(output: &Path, max_frames: u64, input_samples: u64) -> Result<Self> {
        ensure!(max_frames > 0, "source index frame limit must be positive");
        ensure!(
            input_samples > 0,
            "source index needs a nonempty CF32 input"
        );
        ensure!(
            !output.try_exists()?,
            "source index output already exists: {}",
            output.display()
        );
        let partial_path = append_suffix(output, ".partial");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)
            .with_context(|| format!("create source index {}", partial_path.display()))?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);
        writer.write_all(MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&HEADER_BYTES.to_le_bytes())?;
        writer.write_all(&0u32.to_le_bytes())?;
        writer.write_all(&CF32_SAMPLE_RATE_HZ.to_le_bytes())?;
        writer.write_all(&RECOVERED_CHIP_RATE_HZ.to_le_bytes())?;
        Ok(Self {
            final_path: output.to_path_buf(),
            partial_path,
            writer,
            max_frames,
            input_samples,
            records: 0,
            last_chip_count: 0,
            last_required_through: 0,
        })
    }

    /// Append the input position for one record already accepted by the archive.
    pub(crate) fn append(&mut self, chip_count: u64, required_through: u64) -> Result<()> {
        ensure!(
            self.records < self.max_frames,
            "source index frame limit exceeded"
        );
        ensure!(
            chip_count > self.last_chip_count,
            "source index recovered chip counts must increase strictly"
        );
        ensure!(
            required_through < self.input_samples && required_through >= self.last_required_through,
            "source index input positions must be nondecreasing and within the CF32 input"
        );
        let mut record = [0u8; 16];
        record[..8].copy_from_slice(&chip_count.to_le_bytes());
        record[8..].copy_from_slice(&required_through.to_le_bytes());
        self.writer
            .write_all(&record)
            .context("write source index record")?;
        self.records += 1;
        self.last_chip_count = chip_count;
        self.last_required_through = required_through;
        Ok(())
    }

    /// Check archive cardinality and publish without replacing an output path.
    /// An unpublished staging file is retained for diagnosis.
    pub(crate) fn finish(mut self, expected_records: u64) -> Result<()> {
        ensure!(
            self.records == expected_records,
            "source index has {} records for {expected_records} archived candidates",
            self.records
        );
        self.writer.flush().context("flush source index")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("sync source index")?;
        fs::hard_link(&self.partial_path, &self.final_path).with_context(|| {
            format!(
                "publish source index {} without replacing an existing path",
                self.final_path.display()
            )
        })?;
        fs::remove_file(&self.partial_path)
            .context("remove published source index partial link")?;
        let parent = self
            .final_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_version_one_pairs_in_little_endian_order() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("positions.i154idx");
        let mut writer = SourceIndexWriter::create(&output, 2, 10_000).unwrap();
        writer.append(0x0102, 0x0304).unwrap();
        writer.append(0x0506, 0x0708).unwrap();
        writer.finish(2).unwrap();

        assert_eq!(
            fs::read(&output).unwrap(),
            [
                b"I154IDX\0".as_slice(),
                &[1, 0, 24, 0, 0, 0, 0, 0],
                &4_000_000u32.to_le_bytes(),
                &2_000_000u32.to_le_bytes(),
                &[2, 1, 0, 0, 0, 0, 0, 0],
                &[4, 3, 0, 0, 0, 0, 0, 0],
                &[6, 5, 0, 0, 0, 0, 0, 0],
                &[8, 7, 0, 0, 0, 0, 0, 0],
            ]
            .concat()
        );
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!directory.path().join("positions.i154idx.partial").exists());
    }

    #[test]
    fn enforces_archive_count_and_input_position_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("positions.i154idx");
        let mut writer = SourceIndexWriter::create(&output, 2, 10).unwrap();
        assert!(writer.append(0, 0).is_err());
        assert!(writer.append(1, 10).is_err());
        writer.append(1, 9).unwrap();
        assert!(writer.append(1, 9).is_err());
        assert!(writer.append(2, 8).is_err());
        writer.append(2, 9).unwrap();
        assert!(writer.append(3, 9).is_err());
        assert!(writer.finish(1).is_err());
        assert!(!output.exists());
        assert!(directory.path().join("positions.i154idx.partial").exists());
    }

    #[test]
    fn preserves_existing_paths_during_staging_and_publication() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("positions.i154idx");
        let partial = directory.path().join("positions.i154idx.partial");
        fs::write(&partial, b"existing partial").unwrap();
        assert!(SourceIndexWriter::create(&output, 1, 10).is_err());
        assert_eq!(fs::read(&partial).unwrap(), b"existing partial");
        fs::remove_file(&partial).unwrap();

        let writer = SourceIndexWriter::create(&output, 1, 10).unwrap();
        fs::write(&output, b"new competing output").unwrap();
        assert!(writer.finish(0).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"new competing output");
        assert!(partial.is_file());
    }

    #[test]
    fn publishes_a_header_for_an_empty_candidate_archive() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("positions.i154idx");
        let writer = SourceIndexWriter::create(&output, 1, 10).unwrap();
        writer.finish(0).unwrap();
        assert_eq!(
            fs::metadata(&output).unwrap().len(),
            u64::from(HEADER_BYTES)
        );
    }
}
