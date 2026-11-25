//! Stream-validate I154IDX v1 against the candidate archive in record order.

use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, open};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::candidates::{CHANNEL_SAMPLE_RATE_HZ, RECOVERED_CHIP_RATE_HZ, u16_le, u32_le, u64_le};

const MAGIC: &[u8; 8] = b"I154IDX\0";
const HEADER_BYTES: usize = 24;
const RECORD_BYTES: usize = 16;

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) file: PathBuf,
    pub(crate) source_complex_samples: u64,
    pub(crate) owned_samples: Range<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    file: PathBuf,
    bytes: u64,
    sha256: String,
    source_complex_samples: u64,
    owned_start_channel_sample: u64,
    owned_end_channel_sample: u64,
    indexed_record_count: u64,
}

#[derive(Debug)]
pub(crate) struct Reader {
    request: Request,
    reader: BufReader<File>,
    before: Metadata,
    digest: Sha256,
    record_count: u64,
    records_read: u64,
    last_required_through: u64,
}

impl Reader {
    pub(crate) fn open(request: Request, max_records: u64) -> Result<Self> {
        ensure!(
            request.source_complex_samples > 0
                && request.owned_samples.start <= request.owned_samples.end
                && request.owned_samples.end <= request.source_complex_samples,
            "owned channel sample range must fit within a nonempty CF32 input"
        );
        let path_metadata = fs::symlink_metadata(&request.file)
            .with_context(|| format!("inspect source index {}", request.file.display()))?;
        ensure!(
            path_metadata.is_file(),
            "source index must be a direct regular file"
        );
        let file = File::from(
            open(
                &request.file,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .with_context(|| format!("open source index {}", request.file.display()))?,
        );
        let before = file.metadata()?;
        ensure!(
            same_regular_file(&before, &path_metadata),
            "source index path changed while opening"
        );
        let record_extent = before
            .len()
            .checked_sub(HEADER_BYTES as u64)
            .context("truncated source index header")?;
        let record_count = record_extent / RECORD_BYTES as u64;
        ensure!(
            record_extent.is_multiple_of(RECORD_BYTES as u64) && record_count <= max_records,
            "source index byte length is invalid or exceeds the record limit"
        );
        let mut reader = BufReader::new(file);
        let mut header = [0; HEADER_BYTES];
        reader
            .read_exact(&mut header)
            .context("read source index header")?;
        ensure!(
            &header[..8] == MAGIC
                && u16_le(&header, 8) == 1
                && usize::from(u16_le(&header, 10)) == HEADER_BYTES
                && u32_le(&header, 12) == 0
                && u32_le(&header, 16) == CHANNEL_SAMPLE_RATE_HZ
                && u32_le(&header, 20) == RECOVERED_CHIP_RATE_HZ,
            "invalid I154IDX v1 header"
        );
        let mut digest = Sha256::new();
        digest.update(header);
        Ok(Self {
            request,
            reader,
            before,
            digest,
            record_count,
            records_read: 0,
            last_required_through: 0,
        })
    }

    /// Match the next pair and return its zero-based CF32 lookahead horizon.
    pub(crate) fn next(&mut self, chip_count: u64) -> Result<u64> {
        ensure!(
            self.records_read < self.record_count,
            "source index has fewer records than the candidate archive"
        );
        let mut record = [0; RECORD_BYTES];
        self.reader
            .read_exact(&mut record)
            .context("read source index record")?;
        let required_through = u64_le(&record, 8);
        ensure!(
            u64_le(&record, 0) == chip_count,
            "source index chip count does not match candidate {}",
            self.records_read
        );
        ensure!(
            required_through >= self.last_required_through
                && required_through < self.request.source_complex_samples,
            "source index positions must be nondecreasing and within the CF32 input"
        );
        self.digest.update(record);
        self.last_required_through = required_through;
        self.records_read += 1;
        Ok(required_through)
    }

    pub(crate) fn owns(&self, required_through: u64) -> bool {
        self.request.owned_samples.contains(&required_through)
    }

    /// Accept only a complete, unchanged sidecar before publishing outputs.
    pub(crate) fn finish(mut self) -> Result<Report> {
        ensure!(
            self.records_read == self.record_count,
            "source index has more records than the candidate archive"
        );
        let mut trailing = [0];
        ensure!(
            self.reader.read(&mut trailing)? == 0,
            "source index grew while reading"
        );
        let after = self.reader.get_ref().metadata()?;
        let path_after = fs::symlink_metadata(&self.request.file)
            .context("inspect source index path after reading")?;
        ensure!(
            same_regular_file(&self.before, &after) && same_regular_file(&self.before, &path_after),
            "source index changed while reading"
        );
        Ok(Report {
            file: self.request.file,
            bytes: self.before.len(),
            sha256: format!("{:x}", self.digest.finalize()),
            source_complex_samples: self.request.source_complex_samples,
            owned_start_channel_sample: self.request.owned_samples.start,
            owned_end_channel_sample: self.request.owned_samples.end,
            indexed_record_count: self.records_read,
        })
    }
}

fn same_regular_file(before: &Metadata, after: &Metadata) -> bool {
    after.is_file()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}
