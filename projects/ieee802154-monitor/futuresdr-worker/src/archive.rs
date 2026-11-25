//! Versioned, bounded raw IEEE 802.15.4 candidate archive writer.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

pub(crate) const FILE_HEADER_BYTES: u64 = 24;
const RECORD_HEADER_BYTES: u64 = 20;
const FILE_MAGIC: &[u8; 8] = b"I154RAW\0";
const RECORD_MAGIC: &[u8; 4] = b"PSDU";
const VERSION: u16 = 1;
const CHANNEL_SAMPLE_RATE_HZ: u32 = 4_000_000;
const RECOVERED_CHIP_RATE_HZ: u32 = 2_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppendOutcome {
    Continue,
    FrameLimit,
    OutputLimit,
}

#[derive(Debug)]
pub(crate) struct ArchiveWriter {
    final_path: PathBuf,
    partial_path: PathBuf,
    writer: BufWriter<File>,
    max_frames: u64,
    max_bytes: u64,
    records: u64,
    bytes: u64,
    last_chip_count: u64,
}

impl ArchiveWriter {
    pub(crate) fn create(
        output: &Path,
        channel: u8,
        max_frames: u64,
        max_bytes: u64,
    ) -> Result<Self> {
        ensure!((11..=26).contains(&channel), "channel must be 11..=26");
        ensure!(max_frames > 0, "frame limit must be positive");
        ensure!(
            max_bytes >= FILE_HEADER_BYTES,
            "output byte limit is smaller than the archive header"
        );
        ensure!(
            !output.try_exists()?,
            "output already exists: {}",
            output.display()
        );
        let partial_path = append_suffix(output, ".partial");
        ensure!(
            !partial_path.try_exists()?,
            "partial output already exists: {}",
            partial_path.display()
        );
        if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
            ensure!(
                parent.is_dir(),
                "output parent must already be a directory: {}",
                parent.display()
            );
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial_path)
            .with_context(|| format!("create {}", partial_path.display()))?;
        let mut writer = BufWriter::with_capacity(64 * 1024, file);
        writer.write_all(FILE_MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&(FILE_HEADER_BYTES as u16).to_le_bytes())?;
        writer.write_all(&[channel, 0])?;
        writer.write_all(&0u16.to_le_bytes())?;
        writer.write_all(&CHANNEL_SAMPLE_RATE_HZ.to_le_bytes())?;
        writer.write_all(&RECOVERED_CHIP_RATE_HZ.to_le_bytes())?;
        Ok(Self {
            final_path: output.to_path_buf(),
            partial_path,
            writer,
            max_frames,
            max_bytes,
            records: 0,
            bytes: FILE_HEADER_BYTES,
            last_chip_count: 0,
        })
    }

    pub(crate) fn append(&mut self, chip_count: u64, psdu: &[u8]) -> Result<AppendOutcome> {
        ensure!(
            (1..=127).contains(&psdu.len()),
            "candidate PSDU length must be 1..=127"
        );
        ensure!(
            chip_count > self.last_chip_count,
            "candidate recovered chip counts must increase strictly"
        );
        if self.records >= self.max_frames {
            return Ok(AppendOutcome::FrameLimit);
        }
        let record_bytes = RECORD_HEADER_BYTES
            .checked_add(u64::try_from(psdu.len())?)
            .context("candidate record byte count overflow")?;
        let next_bytes = self
            .bytes
            .checked_add(record_bytes)
            .context("candidate archive byte count overflow")?;
        if next_bytes > self.max_bytes {
            return Ok(AppendOutcome::OutputLimit);
        }

        self.writer.write_all(RECORD_MAGIC)?;
        self.writer
            .write_all(&u32::try_from(record_bytes)?.to_le_bytes())?;
        self.writer.write_all(&chip_count.to_le_bytes())?;
        self.writer
            .write_all(&u16::try_from(psdu.len())?.to_le_bytes())?;
        self.writer.write_all(&0u16.to_le_bytes())?;
        self.writer.write_all(psdu)?;
        self.records += 1;
        self.bytes = next_bytes;
        self.last_chip_count = chip_count;

        if self.records == self.max_frames {
            Ok(AppendOutcome::FrameLimit)
        } else if self.bytes == self.max_bytes {
            Ok(AppendOutcome::OutputLimit)
        } else {
            Ok(AppendOutcome::Continue)
        }
    }

    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn publish(mut self) -> Result<(u64, u64)> {
        self.writer.flush().context("flush candidate archive")?;
        self.writer
            .get_ref()
            .sync_all()
            .context("sync candidate archive")?;
        fs::hard_link(&self.partial_path, &self.final_path).with_context(|| {
            format!(
                "publish {} without replacing an existing path",
                self.final_path.display()
            )
        })?;
        fs::remove_file(&self.partial_path).context("remove published partial link")?;
        let parent = self
            .final_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
        Ok((self.records, self.bytes))
    }
}

pub(crate) fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn writes_exact_header_and_record_layout() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("candidates.i154raw");
        let mut writer = ArchiveWriter::create(&output, 17, 2, 1024).unwrap();
        assert_eq!(
            writer.append(100, &[0x02, 0x00, 0x53]).unwrap(),
            AppendOutcome::Continue
        );
        assert_eq!(
            writer.append(200, &[0x41]).unwrap(),
            AppendOutcome::FrameLimit
        );
        assert_eq!(writer.publish().unwrap(), (2, 68));
        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len(), 68);
        assert_eq!(&bytes[0..8], FILE_MAGIC);
        assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[10..12].try_into().unwrap()), 24);
        assert_eq!(&bytes[12..16], &[17, 0, 0, 0]);
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            4_000_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            2_000_000
        );
        assert_eq!(&bytes[24..28], RECORD_MAGIC);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 23);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 100);
        assert_eq!(u16::from_le_bytes(bytes[40..42].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[42..44].try_into().unwrap()), 0);
        assert_eq!(&bytes[44..47], &[0x02, 0x00, 0x53]);
        assert_eq!(&bytes[47..51], RECORD_MAGIC);
        assert_eq!(u32::from_le_bytes(bytes[51..55].try_into().unwrap()), 21);
        assert_eq!(u64::from_le_bytes(bytes[55..63].try_into().unwrap()), 200);
        assert_eq!(u16::from_le_bytes(bytes[63..65].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(bytes[65..67].try_into().unwrap()), 0);
        assert_eq!(bytes[67], 0x41);
        assert_eq!(
            fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!append_suffix(&output, ".partial").exists());
    }

    #[test]
    fn enforces_positions_lengths_limits_and_no_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("bounded.i154raw");
        let mut writer = ArchiveWriter::create(&output, 18, 3, 46).unwrap();
        assert!(writer.append(0, &[1]).is_err());
        assert!(writer.append(1, &[]).is_err());
        assert!(writer.append(1, &[0; 128]).is_err());
        assert_eq!(writer.append(1, &[1]).unwrap(), AppendOutcome::Continue);
        assert!(writer.append(1, &[2]).is_err());
        assert_eq!(writer.append(2, &[2]).unwrap(), AppendOutcome::OutputLimit);
        assert_eq!(writer.records(), 1);
        writer.publish().unwrap();
        assert!(ArchiveWriter::create(&output, 18, 3, 46).is_err());
    }
}
