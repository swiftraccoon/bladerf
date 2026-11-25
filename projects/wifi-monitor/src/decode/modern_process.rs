//! Supervise the native Wi-Fi file replay and capture bounded output streams.

use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, absolute};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{OwnedChild, new_file};

const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const RECORDS_BYTES: usize = 64 * 1024 * 1024;
const DIAGNOSTICS_BYTES: usize = 2 * 1024 * 1024;
const BUFFER_BYTES: usize = 16 * 1024;
const IDLE_POLL: Duration = Duration::from_millis(10);

pub(super) struct Replay<'a> {
    pub(super) program: &'a Path,
    pub(super) input: &'a Path,
    pub(super) frequency_hz: u64,
    pub(super) samples: u64,
    pub(super) records: &'a Path,
    pub(super) diagnostics: &'a Path,
    pub(super) budget: Duration,
    pub(super) interrupted: &'a AtomicBool,
}

#[derive(Debug, Serialize)]
pub(super) struct NativeEvidence {
    pub(super) executable_sha256: String,
    pub(super) records_sha256: String,
}

struct Deadline<'a> {
    end: Instant,
    interrupted: &'a AtomicBool,
}

impl Deadline<'_> {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.interrupted.load(Ordering::Relaxed),
            "native Wi-Fi replay interrupted"
        );
        ensure!(
            Instant::now() < self.end,
            "native Wi-Fi replay exceeded its wall-clock budget"
        );
        Ok(())
    }
}

fn same_file(first: &Metadata, second: &Metadata) -> bool {
    first.dev() == second.dev()
        && first.ino() == second.ino()
        && first.mode() == second.mode()
        && first.len() == second.len()
        && first.mtime() == second.mtime()
        && first.mtime_nsec() == second.mtime_nsec()
        && first.ctime() == second.ctime()
        && first.ctime_nsec() == second.ctime_nsec()
}

fn fingerprint(path: &Path, deadline: &Deadline<'_>) -> Result<(Metadata, String)> {
    let mut file = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open native Wi-Fi executable as a direct file")?,
    );
    let first = file.metadata()?;
    ensure!(
        first.is_file()
            && first.mode() & 0o111 != 0
            && (1..=MAX_EXECUTABLE_BYTES).contains(&first.len()),
        "native Wi-Fi executable must be a bounded executable regular file"
    );
    let mut remaining = first.len();
    let mut hash = Sha256::new();
    let mut buffer = [0; BUFFER_BYTES];
    while remaining > 0 {
        deadline.check()?;
        let count = remaining.min(BUFFER_BYTES as u64) as usize;
        file.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    ensure!(
        file.read(&mut buffer[..1])? == 0
            && same_file(&first, &file.metadata()?)
            && same_file(&first, &fs::symlink_metadata(path)?),
        "native Wi-Fi executable changed while hashing"
    );
    Ok((first, format!("{:x}", hash.finalize())))
}

struct Stream<Pipe> {
    pipe: Pipe,
    file: File,
    label: &'static str,
    remaining: usize,
    eof: bool,
    hash: Sha256,
}

impl<Pipe: Read + AsFd> Stream<Pipe> {
    fn new(pipe: Pipe, file: File, label: &'static str, limit: usize) -> Result<Self> {
        fcntl_setfl(&pipe, fcntl_getfl(&pipe)? | OFlags::NONBLOCK)?;
        Ok(Self {
            pipe,
            file,
            label,
            remaining: limit,
            eof: false,
            hash: Sha256::new(),
        })
    }

    fn drain(&mut self, buffer: &mut [u8]) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let request = buffer.len().min(self.remaining + 1);
        let count = match self.pipe.read(&mut buffer[..request]) {
            Ok(0) => {
                self.eof = true;
                return Ok(true);
            }
            Ok(count) => count,
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                return Ok(false);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read native Wi-Fi {}", self.label));
            }
        };
        let accepted = count.min(self.remaining);
        self.file
            .write_all(&buffer[..accepted])
            .with_context(|| format!("write native Wi-Fi {}", self.label))?;
        self.hash.update(&buffer[..accepted]);
        self.remaining -= accepted;
        ensure!(
            count == accepted,
            "native Wi-Fi {} exceeded its byte cap",
            self.label
        );
        Ok(true)
    }
}

fn supervise(
    process: &mut OwnedChild,
    records: &mut Stream<ChildStdout>,
    diagnostics: &mut Stream<ChildStderr>,
    deadline: &Deadline<'_>,
) -> Result<()> {
    let mut buffer = [0; BUFFER_BYTES];
    loop {
        deadline.check()?;
        let records_progress = records.drain(&mut buffer)?;
        let diagnostics_progress = diagnostics.drain(&mut buffer)?;
        if let Some(status) = process.poll()?
            && records.eof
            && diagnostics.eof
        {
            ensure!(status.success(), "native Wi-Fi replay failed ({status})");
            return Ok(());
        }
        if !records_progress && !diagnostics_progress {
            thread::sleep(
                deadline
                    .end
                    .saturating_duration_since(Instant::now())
                    .min(IDLE_POLL),
            );
        }
    }
}

/// Run file replay with output caps. Failure leaves private diagnostic files
/// and shuts down the child with the existing bounded grace period.
pub(super) fn run(replay: Replay<'_>) -> Result<NativeEvidence> {
    let deadline = Deadline {
        end: Instant::now()
            .checked_add(replay.budget)
            .context("native Wi-Fi replay budget overflow")?,
        interrupted: replay.interrupted,
    };
    deadline.check()?;
    let program = absolute(replay.program)?;
    let input = absolute(replay.input)?;
    let input_metadata = fs::symlink_metadata(&input)?;
    ensure!(
        input_metadata.is_file() && replay.samples.checked_mul(2) == Some(input_metadata.len()),
        "native Wi-Fi replay needs the exact staged cs8 file"
    );
    let sample_limit = replay
        .samples
        .checked_add(1)
        .context("native Wi-Fi replay sample limit overflow")?;
    // Limits exceed the admitted extent so EOF supplies the terminal reason.
    let seconds = replay.samples / SAMPLE_RATE_HZ + 1;
    let (program_metadata, executable_sha256) = fingerprint(&program, &deadline)?;
    let records_file = new_file(replay.records)?;
    let diagnostics_file = new_file(replay.diagnostics)?;
    let mut command = Command::new(&program);
    command
        .args(["--modern", "--replay"])
        .arg(&input)
        .arg(replay.frequency_hz.to_string())
        .arg(seconds.to_string())
        .arg(sample_limit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    deadline.check()?;
    let mut process = OwnedChild::spawn_configured(&mut command)?;
    let mut records = Stream::new(
        process
            .child
            .stdout
            .take()
            .context("missing native Wi-Fi stdout pipe")?,
        records_file,
        "records",
        RECORDS_BYTES,
    )?;
    let mut diagnostics = Stream::new(
        process
            .child
            .stderr
            .take()
            .context("missing native Wi-Fi stderr pipe")?,
        diagnostics_file,
        "diagnostics",
        DIAGNOSTICS_BYTES,
    )?;
    if let Err(error) = supervise(&mut process, &mut records, &mut diagnostics, &deadline) {
        drop(records);
        drop(diagnostics);
        process
            .stop()
            .with_context(|| format!("stop native Wi-Fi replay after {error:#}"))?;
        return Err(error);
    }
    records.file.sync_all()?;
    diagnostics.file.sync_all()?;
    let (final_program_metadata, final_sha256) = fingerprint(&program, &deadline)?;
    ensure!(
        same_file(&program_metadata, &final_program_metadata) && executable_sha256 == final_sha256,
        "native Wi-Fi executable changed during replay"
    );
    ensure!(
        same_file(&input_metadata, &fs::symlink_metadata(&input)?),
        "staged native Wi-Fi IQ changed during replay"
    );
    deadline.check()?;
    Ok(NativeEvidence {
        executable_sha256,
        records_sha256: format!("{:x}", records.hash.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn run_stub(directory: &Path, script: &str) -> Result<NativeEvidence> {
        let program = directory.join("radio_receive");
        fs::write(&program, script)?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
        let input = directory.join("input.cs8");
        fs::write(&input, [0; 4])?;
        run(Replay {
            program: &program,
            input: &input,
            frequency_hz: 2_437_000_000,
            samples: 2,
            records: &directory.join("records.jsonl"),
            diagnostics: &directory.join("diagnostics.log"),
            budget: Duration::from_secs(5),
            interrupted: &AtomicBool::new(false),
        })
    }

    #[test]
    fn native_replay_separates_streams_and_passes_eof_limits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let script = r#"#!/bin/sh
set -eu
[ "$#" -eq 6 ]
[ "$1" = "--modern" ] && [ "$2" = "--replay" ] || exit 10
case "$3" in /*) ;; *) exit 11 ;; esac
[ "$4" = "2437000000" ] && [ "$5" = "1" ] && [ "$6" = "3" ] || exit 12
printf 'records\n'
printf 'diagnostics\n' >&2
"#;
        let evidence = run_stub(directory.path(), script)?;
        let records = directory.path().join("records.jsonl");
        let diagnostics = directory.path().join("diagnostics.log");
        assert_eq!(fs::read(records)?, b"records\n");
        assert_eq!(fs::read(diagnostics)?, b"diagnostics\n");
        assert_eq!(
            evidence.executable_sha256,
            format!("{:x}", Sha256::digest(script))
        );
        assert_eq!(
            evidence.records_sha256,
            format!("{:x}", Sha256::digest(b"records\n"))
        );
        Ok(())
    }

    #[test]
    fn native_replay_rejects_diagnostics_past_the_hard_cap() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let script = format!(
            "#!/bin/sh\nexec head -c {} /dev/zero >&2\n",
            DIAGNOSTICS_BYTES + 1
        );
        let error = run_stub(directory.path(), &script).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("diagnostics exceeded its byte cap")
        );
        assert_eq!(
            fs::metadata(directory.path().join("diagnostics.log"))?.len(),
            DIAGNOSTICS_BYTES as u64
        );
        Ok(())
    }
}
