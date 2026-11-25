//! Bounded direct subprocess supervision for acquisition and analysis stages.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use data_encoding::HEXLOWER;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use nix::unistd::Pid;
use ring::digest::{self, SHA256};
use serde::Serialize;

const IO_BUFFER_BYTES: usize = 16 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const LOG_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_POLL_INTERVAL: Duration = Duration::from_millis(5);
const PROCESS_GROUP_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

struct Timing<'a> {
    timeout: Duration,
    shutdown_grace: Duration,
    progress: Option<&'a mut dyn FnMut() -> Result<usize>>,
}

/// Immutable identity recorded before an executable is launched.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ProgramProvenance {
    pub(crate) name: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

/// Bounded retained prefix of one drained child stream.
#[derive(Debug, Serialize)]
pub(crate) struct LogReport {
    pub(crate) path: PathBuf,
    pub(crate) observed_bytes: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) truncated: bool,
}

/// Terminal result of one direct child invocation.
#[derive(Debug, Serialize)]
pub(crate) struct CommandReport {
    pub(crate) program: &'static str,
    pub(crate) arguments: Vec<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) exit_signal: Option<i32>,
    pub(crate) success: bool,
    pub(crate) timed_out: bool,
    pub(crate) interrupted: bool,
    pub(crate) sigkill_cleanup_sent: bool,
    pub(crate) stdout: LogReport,
    pub(crate) stderr: LogReport,
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create private log {}", path.display()))
}

fn hash_file(path: &Path, bytes: u64) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("open executable {}", path.display()))?;
    let mut digest = digest::Context::new(&SHA256);
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    let mut remaining = bytes;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(IO_BUFFER_BYTES as u64))?;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut trailing = [0u8; 1];
    ensure!(
        file.read(&mut trailing)? == 0,
        "executable grew while hashing"
    );
    Ok(HEXLOWER.encode(digest.finish().as_ref()))
}

/// Resolve and hash one bounded executable before any analysis output exists.
pub(crate) fn resolve_program(name: &'static str, path: &Path) -> Result<ProgramProvenance> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve {name} executable {}", path.display()))?;
    ensure!(
        path.to_str().is_some(),
        "{name} executable path must be valid UTF-8 for the JSON manifest"
    );
    let before = path.metadata()?;
    ensure!(before.is_file(), "{name} must be a regular file");
    ensure!(
        before.permissions().mode() & 0o111 != 0,
        "{name} must be executable"
    );
    ensure!(
        (1..=MAX_EXECUTABLE_BYTES).contains(&before.len()),
        "{name} executable size is outside 1..={MAX_EXECUTABLE_BYTES} bytes"
    );
    let before_modified = before.modified().ok();
    let sha256 = hash_file(&path, before.len())?;
    let after = path.metadata()?;
    ensure!(
        after.len() == before.len()
            && before_modified
                .zip(after.modified().ok())
                .is_none_or(|(old, new)| old == new),
        "{name} executable changed while hashing"
    );
    Ok(ProgramProvenance {
        name,
        path,
        bytes: before.len(),
        sha256,
    })
}

fn drain(
    mut source: impl Read,
    mut destination: File,
    relative_path: PathBuf,
    limit: u64,
    cancel: &AtomicBool,
) -> Result<LogReport> {
    let mut buffer = [0u8; IO_BUFFER_BYTES];
    let mut observed = 0u64;
    let mut retained = 0u64;
    loop {
        ensure!(!cancel.load(Ordering::Relaxed), "log drain cancelled");
        let count = match source.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(LOG_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(error).context("read child pipe"),
        };
        if count == 0 {
            break;
        }
        observed = observed.saturating_add(count as u64);
        let writable = usize::try_from((limit - retained).min(count as u64))?;
        if writable > 0 {
            destination.write_all(&buffer[..writable])?;
            retained += writable as u64;
        }
    }
    destination.flush()?;
    destination.sync_all()?;
    Ok(LogReport {
        path: relative_path,
        observed_bytes: observed,
        retained_bytes: retained,
        truncated: observed > retained,
    })
}

/// Owns one process-group leader. Normal supervision explicitly reaps it;
/// fallback destruction sends SIGKILL to the group and waits for the leader,
/// so destruction can block in the operating system's wait implementation.
struct OwnedChild {
    child: Child,
    process_group: Pid,
    reaped: bool,
}

impl OwnedChild {
    fn leader_exited(&self) -> Result<bool> {
        let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
        match waitid(Id::Pid(self.process_group), flags)? {
            WaitStatus::StillAlive => Ok(false),
            WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _)
                if pid == self.process_group =>
            {
                Ok(true)
            }
            status => bail!("unexpected child process-group leader status {status:?}"),
        }
    }

    fn signal_group(&self, signal: Signal) -> Result<()> {
        match killpg(self.process_group, signal) {
            Ok(()) => Ok(()),
            Err(error) => Err(error).context("signal child process group"),
        }
    }

    fn stop(&mut self, grace: Duration) -> Result<ExitStatus> {
        self.signal_group(Signal::SIGINT)?;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
        }
        self.signal_group(Signal::SIGKILL)?;
        self.reap()
    }

    fn finish_normally(&mut self) -> Result<ExitStatus> {
        ensure!(self.leader_exited()?, "child process-group leader is live");
        self.signal_group(Signal::SIGKILL)?;
        self.reap()
    }

    fn reap(&mut self) -> Result<ExitStatus> {
        let status = self
            .child
            .wait()
            .context("reap child process-group leader")?;
        self.reaped = true;
        Ok(status)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal_group(Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

fn spawn_log(
    source: impl AsFd + Read + Send + 'static,
    destination: File,
    relative_path: PathBuf,
    limit: u64,
    cancel: &Arc<AtomicBool>,
) -> Result<JoinHandle<Result<LogReport>>> {
    let flags = OFlag::from_bits_retain(fcntl(source.as_fd(), FcntlArg::F_GETFL)?);
    fcntl(source.as_fd(), FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    let cancel = Arc::clone(cancel);
    thread::Builder::new()
        .spawn(move || drain(source, destination, relative_path, limit, &cancel))
        .context("start child log drain")
}

/// Wait for all log workers under one budget. Cancellation stops nonblocking
/// pipe reads, but a worker already in filesystem I/O can outlive the wait.
fn wait_for_log_drains(
    workers: &[&JoinHandle<Result<LogReport>>],
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    while workers.iter().any(|worker| !worker.is_finished()) {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            cancel.store(true, Ordering::Relaxed);
            bail!(
                "child log drains did not finish before the shared deadline after process-group shutdown"
            );
        }
        thread::sleep(remaining.min(LOG_POLL_INTERVAL));
    }
    Ok(())
}

fn join_log(worker: JoinHandle<Result<LogReport>>, label: &str) -> Result<LogReport> {
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("{label} log-drain thread panicked"))?
        .with_context(|| format!("drain {label} log"))
}

/// Execute one command directly, drain both pipes, and always reap the child.
pub(crate) fn run(
    program: &ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    log_stem: &str,
    log_limit: u64,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    run_with_grace(
        program,
        arguments,
        output,
        log_stem,
        log_limit,
        Timing {
            timeout,
            shutdown_grace: PROCESS_GROUP_SHUTDOWN_GRACE,
            progress: None,
        },
        stop,
    )
}

/// Supervise capture with an idle deadline. The counter starts at zero, and an
/// observation above its previous high restarts the deadline. The probe must
/// return promptly. A probe error shuts down the child group and reaps its leader.
#[allow(
    clippy::too_many_arguments,
    reason = "match the basic runner while taking a caller-owned progress probe"
)]
pub(crate) fn run_with_idle_timeout(
    program: &ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    log_stem: &str,
    log_limit: u64,
    idle_timeout: Duration,
    stop: &AtomicBool,
    mut progress: impl FnMut() -> Result<usize>,
) -> Result<CommandReport> {
    run_with_grace(
        program,
        arguments,
        output,
        log_stem,
        log_limit,
        Timing {
            timeout: idle_timeout,
            shutdown_grace: PROCESS_GROUP_SHUTDOWN_GRACE,
            progress: Some(&mut progress),
        },
        stop,
    )
}

/// Execute one command with its standard temporary-file directory scoped to a
/// direct private directory owned by the current analysis.
#[allow(
    clippy::too_many_arguments,
    reason = "match the basic runner while taking a caller-owned temporary directory"
)]
pub(crate) fn run_with_temp_directory(
    program: &ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    log_stem: &str,
    log_limit: u64,
    timeout: Duration,
    temp_directory: &Path,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    let metadata = fs::symlink_metadata(temp_directory).with_context(|| {
        format!(
            "inspect scoped child temporary directory {}",
            temp_directory.display()
        )
    })?;
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "scoped child temporary directory must be a direct directory"
    );
    ensure!(
        metadata.permissions().mode() & 0o777 == 0o700,
        "scoped child temporary directory must have mode 0700"
    );
    run_with_grace_and_temp_directory(
        program,
        arguments,
        output,
        log_stem,
        log_limit,
        Timing {
            timeout,
            shutdown_grace: PROCESS_GROUP_SHUTDOWN_GRACE,
            progress: None,
        },
        Some(temp_directory),
        stop,
    )
}

fn run_with_grace(
    program: &ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    log_stem: &str,
    log_limit: u64,
    timing: Timing<'_>,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    run_with_grace_and_temp_directory(
        program, arguments, output, log_stem, log_limit, timing, None, stop,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "keep child identity, log limits and shutdown controls explicit"
)]
fn run_with_grace_and_temp_directory(
    program: &ProgramProvenance,
    arguments: &[OsString],
    output: &Path,
    log_stem: &str,
    log_limit: u64,
    mut timing: Timing<'_>,
    temp_directory: Option<&Path>,
    stop: &AtomicBool,
) -> Result<CommandReport> {
    ensure!(log_limit > 0, "log byte limit must be positive");
    ensure!(!timing.timeout.is_zero(), "child timeout must be positive");
    ensure!(
        !timing.shutdown_grace.is_zero(),
        "process-group shutdown grace must be positive"
    );
    ensure!(
        !log_stem.is_empty()
            && log_stem
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "log stem must contain only ASCII letters, digits, '-' or '_'"
    );
    let stdout_relative = PathBuf::from("logs").join(format!("{log_stem}.stdout.log"));
    let stderr_relative = PathBuf::from("logs").join(format!("{log_stem}.stderr.log"));
    let stdout_file = private_file(&output.join(&stdout_relative))?;
    let stderr_file = private_file(&output.join(&stderr_relative))?;

    let mut command = Command::new(&program.path);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(temp_directory) = temp_directory {
        command.env("TMPDIR", temp_directory);
    }
    let child = command
        .spawn()
        .with_context(|| format!("start {}", program.name))?;
    let process_group = Pid::from_raw(i32::try_from(child.id()).context("child PID exceeds i32")?);
    let mut child = OwnedChild {
        child,
        process_group,
        reaped: false,
    };
    let stdout = child
        .child
        .stdout
        .take()
        .context("child stdout pipe missing")?;
    let stderr = child
        .child
        .stderr
        .take()
        .context("child stderr pipe missing")?;
    let cancel_drains = Arc::new(AtomicBool::new(false));
    let stdout_worker = spawn_log(
        stdout,
        stdout_file,
        stdout_relative,
        log_limit,
        &cancel_drains,
    )
    .context("start stdout log drain")?;
    let stderr_worker = match spawn_log(
        stderr,
        stderr_file,
        stderr_relative,
        log_limit,
        &cancel_drains,
    )
    .context("start stderr log drain")
    {
        Ok(worker) => worker,
        Err(error) => {
            cancel_drains.store(true, Ordering::Relaxed);
            drop(child);
            if wait_for_log_drains(&[&stdout_worker], &cancel_drains, LOG_DRAIN_TIMEOUT).is_ok() {
                let _ = join_log(stdout_worker, "stdout");
            }
            return Err(error);
        }
    };
    let outcome = (|| -> Result<_> {
        let mut last_progress_at = Instant::now();
        let mut highest_progress = 0;
        loop {
            if child.leader_exited()? {
                return Ok((child.finish_normally()?, false, false));
            }
            if stop.load(Ordering::Relaxed) {
                return Ok((child.stop(timing.shutdown_grace)?, false, true));
            }
            if let Some(progress) = timing.progress.as_mut() {
                let observed = progress().context("sample child progress")?;
                if observed > highest_progress {
                    highest_progress = observed;
                    last_progress_at = Instant::now();
                }
            }
            if last_progress_at.elapsed() >= timing.timeout {
                return Ok((child.stop(timing.shutdown_grace)?, true, false));
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    drop(child);
    let logs = wait_for_log_drains(
        &[&stdout_worker, &stderr_worker],
        &cancel_drains,
        LOG_DRAIN_TIMEOUT,
    )
    .map(|()| {
        (
            join_log(stdout_worker, "stdout"),
            join_log(stderr_worker, "stderr"),
        )
    });
    let (status, timed_out, interrupted) = outcome?;
    let (stdout, stderr) = logs?;
    let success = status.success() && !timed_out && !interrupted;
    Ok(CommandReport {
        program: program.name,
        arguments: arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect(),
        exit_code: status.code(),
        exit_signal: status.signal(),
        success,
        timed_out,
        interrupted,
        sigkill_cleanup_sent: true,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// Convert an unsuccessful command report into a contextual stage error.
pub(crate) fn require_success(report: &CommandReport) -> Result<()> {
    if report.success {
        return Ok(());
    }
    if report.interrupted {
        bail!("{} was interrupted", report.program);
    }
    if report.timed_out {
        bail!("{} exceeded its wall-clock deadline", report.program);
    }
    bail!(
        "{} exited unsuccessfully (code {:?}, signal {:?})",
        report.program,
        report.exit_code,
        report.exit_signal
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::net::UnixStream;

    use nix::errno::Errno;
    use nix::sys::signal::kill;

    use super::*;

    #[test]
    fn log_drain_retains_only_the_bounded_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("log");
        let file = private_file(&path).unwrap();
        let report = drain(
            Cursor::new(vec![7u8; 4096]),
            file,
            PathBuf::from("logs/test.log"),
            1024,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(report.observed_bytes, 4096);
        assert_eq!(report.retained_bytes, 1024);
        assert!(report.truncated);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 1024);
    }

    #[test]
    fn log_drain_deadline_cancels_workers_with_open_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (mut writers, mut drains) = (Vec::new(), Vec::new());
        for label in ["stdout", "stderr"] {
            let (reader, writer) = UnixStream::pair().unwrap();
            writers.push(writer);
            drains.push(
                spawn_log(
                    reader,
                    private_file(&directory.path().join(label)).unwrap(),
                    PathBuf::from(label),
                    1024,
                    &cancel,
                )
                .unwrap(),
            );
        }
        let error =
            wait_for_log_drains(&[&drains[0], &drains[1]], &cancel, Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("shared deadline"));
        assert!(cancel.load(Ordering::Relaxed));
        wait_for_log_drains(&[&drains[0], &drains[1]], &cancel, Duration::from_secs(1)).unwrap();
        for drain in drains {
            let error = drain.join().unwrap().unwrap_err();
            assert_eq!(error.to_string(), "log drain cancelled");
        }
        drop(writers);
    }

    #[test]
    fn direct_child_is_reaped_and_has_private_logs() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let program = resolve_program("true", Path::new("/bin/true")).unwrap();
        let report = run(
            &program,
            &[],
            directory.path(),
            "true",
            1024,
            Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.success);
        for log in [&report.stdout, &report.stderr] {
            assert_eq!(
                std::fs::metadata(directory.path().join(&log.path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn scoped_temp_directory_reaches_child_without_entering_report() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let scoped = directory.path().join("scoped-runtime");
        std::fs::create_dir(&scoped).unwrap();
        std::fs::set_permissions(&scoped, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(scoped.join("probe"), b"expected directory").unwrap();
        let program = resolve_program("shell-fixture", Path::new("/bin/sh")).unwrap();
        let report = run_with_temp_directory(
            &program,
            &[
                OsString::from("-c"),
                OsString::from("test -f \"$TMPDIR/probe\""),
            ],
            directory.path(),
            "scoped-temp",
            1024,
            Duration::from_secs(1),
            &scoped,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.success);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(scoped.to_str().unwrap())
        );
    }

    #[test]
    fn scoped_temp_directory_rejects_links_and_nonprivate_modes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let program = resolve_program("true", Path::new("/bin/true")).unwrap();
        let nonprivate = directory.path().join("nonprivate");
        std::fs::create_dir(&nonprivate).unwrap();
        std::fs::set_permissions(&nonprivate, std::fs::Permissions::from_mode(0o755)).unwrap();
        let result = run_with_temp_directory(
            &program,
            &[],
            directory.path(),
            "nonprivate",
            1024,
            Duration::from_secs(1),
            &nonprivate,
            &AtomicBool::new(false),
        );
        assert!(result.unwrap_err().to_string().contains("mode 0700"));

        let private = directory.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = directory.path().join("linked");
        std::os::unix::fs::symlink(private, &link).unwrap();
        let result = run_with_temp_directory(
            &program,
            &[],
            directory.path(),
            "linked",
            1024,
            Duration::from_secs(1),
            &link,
            &AtomicBool::new(false),
        );
        assert!(result.unwrap_err().to_string().contains("direct directory"));
    }

    #[test]
    fn deadline_interrupts_and_reaps_the_process_group_leader() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let program = resolve_program("sleep", Path::new("/bin/sleep")).unwrap();
        let started = Instant::now();
        let report = run_with_grace(
            &program,
            &[OsString::from("5")],
            directory.path(),
            "sleep",
            1024,
            Timing {
                timeout: Duration::from_millis(40),
                shutdown_grace: Duration::from_millis(100),
                progress: None,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(!report.success);
        assert!(report.timed_out);
        assert!(!report.interrupted);
        assert!(report.sigkill_cleanup_sent);
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn idle_deadline_ignores_repeated_and_regressing_progress() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        let program = resolve_program("sleep", Path::new("/bin/sleep")).unwrap();
        let mut advanced = false;
        let report = run_with_grace(
            &program,
            &[OsString::from("1")],
            directory.path(),
            "stalled",
            1024,
            Timing {
                timeout: Duration::from_millis(150),
                shutdown_grace: Duration::from_millis(100),
                progress: Some(&mut || {
                    advanced = !advanced;
                    Ok(usize::from(advanced))
                }),
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.timed_out);
        assert!(!report.success);
        assert!(!report.interrupted);
    }

    #[test]
    fn advancing_progress_allows_capture_to_run_beyond_the_initial_deadline() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("logs")).unwrap();
        let program = resolve_program("sleep", Path::new("/bin/sleep")).unwrap();
        let mut observed = 0;
        let report = run_with_idle_timeout(
            &program,
            &[OsString::from("0.5")],
            directory.path(),
            "advancing",
            1024,
            Duration::from_millis(200),
            &AtomicBool::new(false),
            || {
                observed += 1;
                Ok(observed)
            },
        )
        .unwrap();
        assert!(report.success);
        assert!(!report.timed_out);
        assert!(observed > 1);
    }

    #[test]
    fn leader_exit_cannot_leave_an_int_ignoring_pipe_holder() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let descendant_pid = directory.path().join("descendant.pid");
        let program = resolve_program("shell-fixture", Path::new("/bin/sh")).unwrap();
        let script = "trap 'exit 0' INT; (trap '' INT; sleep 30) & echo $! > \"$1\"; wait";
        let started = Instant::now();
        let report = run_with_grace(
            &program,
            &[
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("digital-monitor-process-fixture"),
                descendant_pid.as_os_str().to_owned(),
            ],
            directory.path(),
            "descendant",
            1024,
            Timing {
                timeout: Duration::from_millis(100),
                shutdown_grace: Duration::from_millis(100),
                progress: None,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.timed_out);
        assert!(report.sigkill_cleanup_sent);
        assert!(started.elapsed() >= Duration::from_millis(180));
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(descendant_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match kill(Pid::from_raw(pid), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                result => panic!("descendant process survived group cleanup: {result:?}"),
            }
        }
    }

    #[test]
    fn normal_leader_exit_cleans_a_pipe_holding_descendant_before_reap() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("logs")).unwrap();
        let descendant_pid = directory.path().join("orphan.pid");
        let program = resolve_program("shell-fixture", Path::new("/bin/sh")).unwrap();
        let script = "(trap '' INT; sleep 30) & echo $! > \"$1\"; exit 0";
        let started = Instant::now();
        let report = run(
            &program,
            &[
                OsString::from("-c"),
                OsString::from(script),
                OsString::from("digital-monitor-orphan-fixture"),
                descendant_pid.as_os_str().to_owned(),
            ],
            directory.path(),
            "orphan",
            1024,
            Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(report.success);
        assert!(report.sigkill_cleanup_sent);
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = std::fs::read_to_string(descendant_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match kill(Pid::from_raw(pid), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                result => panic!("orphan process survived group cleanup: {result:?}"),
            }
        }
    }
}
