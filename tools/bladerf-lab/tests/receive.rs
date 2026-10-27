//! CLI input/ownership regressions, plus an opt-in test for real-device shutdown.
//! The ignored test requires a compatible micro xA9 receive setup and exclusive access.

#![cfg(feature = "radio")]

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};

#[test]
fn invalid_capture_never_creates_outputs_or_opens_hardware() -> Result<()> {
    let directory = tempfile::tempdir()?;
    for value in ["0", "NaN", "inf", "61"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bladerf-lab"))
            .args(["capture", "--duration", value, "--output"])
            .arg(directory.path().join("invalid.sc16q11"))
            .output()?;
        ensure!(!output.status.success(), "invalid duration was accepted");
        ensure!(std::fs::read_dir(directory.path())?.count() == 0);
        ensure!(!String::from_utf8_lossy(&output.stderr).contains("bladerf_open"));
    }
    Ok(())
}

#[test]
#[ignore = "requires a compatible bladeRF micro xA9 receive setup and exclusive access"]
fn interrupted_hardware_capture_cleans_up() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("interrupted.sc16q11");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bladerf-lab"))
        .args(["capture", "--duration", "10", "--output"])
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::sleep(Duration::from_secs(1));
    if let Some(status) = child.try_wait()? {
        anyhow::bail!("capture ended before interruption test: {status}");
    }
    let signal = Command::new("kill")
        .arg("-INT")
        .arg(child.id().to_string())
        .status();
    if !matches!(signal, Ok(status) if status.success()) {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("could not interrupt capture");
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("capture did not stop after SIGINT");
        }
        thread::sleep(Duration::from_millis(50));
    };
    ensure!(!status.success(), "interrupted capture reported success");
    ensure!(!output.exists(), "incomplete capture was published");
    ensure!(
        std::fs::read_dir(directory.path())?.count() == 0,
        "partial temporary output remained after interrupted capture"
    );
    Ok(())
}
