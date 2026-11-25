//! Copy a pinned rolling segment after the capture spool can release it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use ring::digest::{self, SHA256};

use crate::{input::SourceReport, report};

use super::super::{
    intake::{IQ_NAME, ValidatedSegment},
    overlap::open_admitted_iq,
};

const MAX_SIDECAR_BYTES: u64 = 64 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub(super) struct PinnedInput {
    source: SourceReport,
    iq: File,
    sidecar: Vec<u8>,
}

/// Open the admitted IQ and snapshot its sidecar before the coordinator unlinks either.
pub(super) fn pin(segment: &ValidatedSegment) -> Result<PinnedInput> {
    let iq = open_admitted_iq(segment)?;
    let (directory, _) = segment.open_for_release()?;
    let sidecar_name = segment
        .source
        .sidecar
        .file_name()
        .context("admitted Coded replay sidecar has no filename")?;
    let mut file = File::from(
        openat(
            &directory,
            sidecar_name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .context("open admitted Coded replay sidecar")?,
    );
    let initial = file.metadata()?;
    ensure!(
        initial.is_file() && (1..=MAX_SIDECAR_BYTES).contains(&initial.len()),
        "admitted Coded replay sidecar is not a bounded regular file"
    );
    let mut sidecar = Vec::new();
    (&mut file)
        .take(MAX_SIDECAR_BYTES + 1)
        .read_to_end(&mut sidecar)
        .context("read admitted Coded replay sidecar")?;
    let latest = fs::symlink_metadata(&segment.source.sidecar)?;
    ensure!(
        sidecar.len() as u64 == initial.len()
            && latest.is_file()
            && report::same_file_identity(&initial, &file.metadata()?)
            && report::same_file_identity(&initial, &latest)
            && HEXLOWER.encode(digest::digest(&SHA256, &sidecar).as_ref())
                == segment.source.sidecar_sha256,
        "admitted Coded replay sidecar changed while pinning"
    );
    segment.verify_unchanged()?;
    Ok(PinnedInput {
        source: segment.source.clone(),
        iq,
        sidecar,
    })
}

impl PinnedInput {
    /// Stage the admitted bytes in caller-owned scratch, including after source unlink.
    /// Check cancellation between bounded reads. Failure can leave partial scratch files.
    pub(super) fn stage(mut self, directory: &Path, stop: &AtomicBool) -> Result<SourceReport> {
        check_stop(stop)?;
        self.source.input = directory.join(IQ_NAME);
        let sidecar_name = self
            .source
            .sidecar
            .file_name()
            .context("admitted Coded replay sidecar has no filename")?;
        self.source.sidecar = directory.join(sidecar_name);
        let mut output = private_file(&self.source.input)?;
        let mut remaining = self.source.bytes;
        let mut hash = digest::Context::new(&SHA256);
        let mut buffer = [0u8; COPY_BUFFER_BYTES];
        while remaining > 0 {
            check_stop(stop)?;
            let count = remaining.min(buffer.len() as u64) as usize;
            let chunk = &mut buffer[..count];
            self.iq
                .read_exact(chunk)
                .context("read the admitted Coded replay IQ extent")?;
            output
                .write_all(chunk)
                .context("write staged Coded replay IQ")?;
            hash.update(chunk);
            remaining -= count as u64;
        }
        check_stop(stop)?;
        let mut trailing = [0u8; 1];
        ensure!(
            self.iq.read(&mut trailing)? == 0
                && HEXLOWER.encode(hash.finish().as_ref()) == self.source.sha256,
            "pinned Coded replay IQ differs from its admitted extent or SHA-256"
        );
        private_file(&self.source.sidecar)?
            .write_all(&self.sidecar)
            .context("write staged Coded replay sidecar")?;
        Ok(self.source)
    }
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create Coded replay scratch file {}", path.display()))
}

fn check_stop(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "Coded replay staging interrupted"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IQ: &[u8] = &[1, 0, 2, 0, 3, 0, 4, 0];
    const SIDECAR_NAME: &str = "iq.sc16q11.json";
    const SIDECAR: &[u8] = br#"{"num_samples":2}"#;

    fn fixture(directory: &Path) -> Result<PinnedInput> {
        let input = directory.join(IQ_NAME);
        let sidecar = directory.join(SIDECAR_NAME);
        fs::write(&input, IQ)?;
        fs::write(&sidecar, SIDECAR)?;
        let iq = File::open(&input)?;
        Ok(PinnedInput {
            source: SourceReport {
                input,
                sidecar,
                format: crate::input::NORMALIZED_SC16_Q11_FORMAT,
                center_frequency_hz: 2_437_000_000,
                sample_rate_hz: 20_000_000,
                bandwidth_hz: 20_000_000,
                complex_samples: 2,
                bytes: IQ.len() as u64,
                sha256: HEXLOWER.encode(digest::digest(&SHA256, IQ).as_ref()),
                sidecar_sha256: HEXLOWER.encode(digest::digest(&SHA256, SIDECAR).as_ref()),
                continuity_guaranteed: false,
            },
            iq,
            sidecar: SIDECAR.to_vec(),
        })
    }

    #[test]
    fn stages_admitted_bytes_after_original_files_are_unlinked() -> Result<()> {
        let original = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let input = fixture(original.path())?;
        let mut expected = input.source.clone();
        fs::remove_file(&expected.input)?;
        fs::remove_file(&expected.sidecar)?;
        expected.input = scratch.path().join(IQ_NAME);
        expected.sidecar = scratch.path().join(SIDECAR_NAME);
        let staged = input.stage(scratch.path(), &AtomicBool::new(false))?;
        assert_eq!(staged, expected);
        assert_eq!(fs::read(staged.input)?, IQ);
        assert_eq!(fs::read(staged.sidecar)?, SIDECAR);
        Ok(())
    }

    #[test]
    fn rejects_same_size_mutation_of_the_open_source() -> Result<()> {
        let original = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let input = fixture(original.path())?;
        fs::write(&input.source.input, [0; IQ.len()])?;
        let error = input
            .stage(scratch.path(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("admitted extent or SHA-256"));
        assert!(!scratch.path().join(SIDECAR_NAME).exists());
        Ok(())
    }

    #[test]
    fn cancellation_before_staging_leaves_scratch_empty() -> Result<()> {
        let original = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let input = fixture(original.path())?;
        let error = input
            .stage(scratch.path(), &AtomicBool::new(true))
            .unwrap_err();
        assert!(error.to_string().contains("staging interrupted"));
        assert!(fs::read_dir(scratch.path())?.next().is_none());
        Ok(())
    }
}
