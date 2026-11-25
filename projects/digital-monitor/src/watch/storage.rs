use super::{
    Context, File, MAX_TREE_DEPTH, MAX_TREE_ENTRIES, MetadataExt, OpenOptions, OpenOptionsExt,
    Path, Result, ensure, fs,
};

pub(super) fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create private file {}", path.display()))
}

pub(super) fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[derive(Default)]
pub(super) struct TreeBudget {
    pub(super) entries: u64,
    pub(super) bytes: u64,
}

pub(super) fn inspect_tree(
    path: &Path,
    expected_device: u64,
    depth: usize,
    budget: &mut TreeBudget,
) -> Result<()> {
    ensure!(depth <= MAX_TREE_DEPTH, "managed tree exceeds depth bound");
    budget.entries = budget
        .entries
        .checked_add(1)
        .context("entry count overflow")?;
    ensure!(
        budget.entries <= MAX_TREE_ENTRIES,
        "managed tree exceeds entry bound"
    );
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.dev() == expected_device,
        "managed tree crosses a filesystem or mount boundary"
    );
    let kind = metadata.file_type();
    ensure!(!kind.is_symlink(), "managed tree contains a symbolic link");
    if kind.is_file() {
        budget.bytes = budget
            .bytes
            .checked_add(metadata.len())
            .context("byte count overflow")?;
        return Ok(());
    }
    ensure!(
        kind.is_dir(),
        "managed tree contains an unexpected file type"
    );
    for child in fs::read_dir(path)? {
        inspect_tree(&child?.path(), expected_device, depth + 1, budget)?;
    }
    Ok(())
}

pub(crate) fn tree_bytes(path: &Path) -> Result<u64> {
    let expected_device = fs::symlink_metadata(path)?.dev();
    let mut budget = TreeBudget::default();
    inspect_tree(path, expected_device, 0, &mut budget)?;
    Ok(budget.bytes)
}

pub(crate) fn remove_quarantined_tree(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "quarantine root is not an owned directory"
    );
    // Rust's Unix remove_dir_all implementation is specified not to follow
    // symbolic links. The tree has already been bounded and moved atomically
    // into this private quarantine, so an entry swapped to a symlink after the
    // validation walk is unlinked rather than traversed.
    fs::remove_dir_all(path).context("remove quarantined session without following links")
}
