//! Linux regular-file pinning shared by offline verification paths.

use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use anyhow::Result;
#[cfg(target_os = "linux")]
use anyhow::{Context, ensure};
#[cfg(target_os = "linux")]
use nix::fcntl::{OFlag, open};
#[cfg(target_os = "linux")]
use nix::sys::stat::{Mode, SFlag, fstat};

/// Inspect a final path component without opening the underlying object for I/O,
/// require a nonempty bounded regular file, then open that pinned inode for
/// reading. Parent path components use ordinary Linux path resolution.
#[cfg(target_os = "linux")]
pub(crate) fn open_regular(path: &Path, maximum: u64) -> Result<(File, u64)> {
    let pinned = open(
        path,
        OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("pin file without following symlinks: {}", path.display()))?;
    let pinned_stat =
        fstat(&pinned).with_context(|| format!("inspect pinned file: {}", path.display()))?;
    let file_type = SFlag::from_bits_truncate(pinned_stat.st_mode) & SFlag::S_IFMT;
    let length = u64::try_from(pinned_stat.st_size)
        .with_context(|| format!("inspect file length: {}", path.display()))?;
    ensure!(
        file_type == SFlag::S_IFREG && length > 0 && length <= maximum,
        "{} must be a nonempty regular file no larger than {maximum} bytes",
        path.display()
    );

    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
    let file = File::open(&descriptor_path)
        .with_context(|| format!("open validated file inode: {}", path.display()))?;
    let readable_stat =
        fstat(&file).with_context(|| format!("reinspect readable file: {}", path.display()))?;
    ensure!(
        readable_stat.st_dev == pinned_stat.st_dev
            && readable_stat.st_ino == pinned_stat.st_ino
            && readable_stat.st_size == pinned_stat.st_size,
        "{} changed before its validated inode was opened",
        path.display()
    );
    Ok((file, length))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_regular(_path: &Path, _maximum: u64) -> Result<(File, u64)> {
    anyhow::bail!("regular-file pinning requires Linux /proc/self/fd support")
}
