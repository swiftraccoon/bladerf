//! Positive, hardware-free checks for workspace discovery after package relocation.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

fn repository_root() -> Result<PathBuf> {
    let member = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let root = member
        .parent()
        .and_then(Path::parent)
        .context("package must live under tools/<name>")?
        .to_path_buf();
    ensure!(
        root.join(".gitmodules").is_file(),
        "repository marker missing"
    );
    ensure!(
        root.join("config/images.sha256").is_file(),
        "shared image manifest missing"
    );
    Ok(root)
}

fn assert_setup_plan(output: Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "dry-run failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).context("setup plan must be UTF-8")?;
    ensure!(
        stdout.contains("Dry run: no package, filesystem, group, or hardware changes."),
        "successful command did not print the setup plan"
    );
    for source in [
        "repos/bladeRF/host",
        "repos/SoapyBladeRF",
        "repos/gr-bladeRF",
    ] {
        ensure!(
            stdout.contains(&format!("cmake \"-S\" \"{source}\"")),
            "setup plan is missing root-relative native source {source}"
        );
    }
    Ok(())
}

#[test]
fn discovers_repository_from_member_directory() -> Result<()> {
    let root = repository_root()?;
    let member = root.join("tools/bladerf-lab");
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-lab"))
        .current_dir(member)
        .args(["setup-fedora", "--dry-run"])
        .output()?;
    assert_setup_plan(output)
}

#[test]
fn explicit_workspace_works_from_unrelated_directory() -> Result<()> {
    let root = repository_root()?;
    let directory = tempfile::tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_bladerf-lab"))
        .current_dir(directory.path())
        .arg("--workspace")
        .arg(root)
        .args(["setup-fedora", "--dry-run"])
        .output()?;
    assert_setup_plan(output)?;
    ensure!(
        std::fs::read_dir(directory.path())?.next().is_none(),
        "dry-run created files in the caller's directory"
    );
    Ok(())
}
