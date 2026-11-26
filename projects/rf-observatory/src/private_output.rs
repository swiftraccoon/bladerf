//! Publish private output after writing and syncing its contents.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Write a private JSON file, run a final check, then publish without replacement.
/// The destination's parent must exist. An error syncing the parent after
/// publication leaves the completed file at its destination.
///
/// # Errors
/// Returns an error on I/O or serialization failure, a destination collision,
/// or rejection by the caller's final check.
pub(crate) fn publish_json_file(
    destination: &Path,
    report: &impl Serialize,
    ready: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, report)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    ready()?;
    temporary
        .persist_noclobber(destination)
        .with_context(|| format!("publish {} without replacing output", destination.display()))?;
    File::open(parent)?
        .sync_all()
        .context("output was published, but syncing its parent failed")
}

pub(crate) struct StagedDirectory {
    stage: tempfile::TempDir,
    parent: File,
    destination: PathBuf,
}

impl StagedDirectory {
    /// Start a fresh, private directory on the destination filesystem.
    pub(crate) fn new(destination: &Path, prefix: &str) -> Result<Self> {
        destination
            .file_name()
            .context("output needs a directory name")?;
        match destination.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect output path"),
            Ok(_) => bail!("output already exists: {}", destination.display()),
        }
        let parent_path = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent_path).context("create output parent")?;
        let parent = File::open(parent_path).context("open output parent")?;
        let stage = tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent_path)
            .context("create private staging directory")?;
        Ok(Self {
            stage,
            parent,
            destination: destination.to_path_buf(),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.stage.path()
    }

    /// Sync the stage, run the final caller check, and publish without replacement.
    pub(crate) fn publish(self, ready: impl FnOnce() -> Result<()>) -> Result<()> {
        File::open(self.stage.path())?
            .sync_all()
            .context("sync staged directory")?;
        let stage_name = self
            .stage
            .path()
            .file_name()
            .context("missing staging name")?;
        let destination_name = self
            .destination
            .file_name()
            .context("missing output name")?;
        ready()?;
        rename(
            &self.parent,
            Path::new(stage_name),
            Path::new(destination_name),
        )
        .with_context(|| {
            format!(
                "publish {} without replacing output",
                self.destination.display()
            )
        })?;
        // Disarm cleanup for the staging name after ownership moves to the output path.
        let _ = self.stage.keep();
        self.parent
            .sync_all()
            .context("output was published, but syncing its parent failed")
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename(parent: &File, stage: &Path, destination: &Path) -> Result<()> {
    use nix::fcntl::{RenameFlags, renameat2};

    renameat2(
        parent,
        stage,
        parent,
        destination,
        RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn rename(_parent: &File, _stage: &Path, _destination: &Path) -> Result<()> {
    bail!("atomic directory publication requires Linux GNU renameat2 support")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_final_check_discards_the_synced_stage() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("output");
        let output = StagedDirectory::new(&destination, ".stage-").unwrap();
        let stage = output.path().to_path_buf();
        crate::json_file(&stage.join("receipt.json"), &1).unwrap();

        let result = output.publish(|| bail!("cancel after sync"));

        assert_eq!(result.unwrap_err().to_string(), "cancel after sync");
        assert!(!stage.exists());
        assert!(!destination.exists());
    }
}
