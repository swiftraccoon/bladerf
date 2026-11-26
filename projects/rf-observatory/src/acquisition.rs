//! One validated stock-image capture through the native receive adapter.
//!
//! The caller supplies an existing private output directory. The adapter
//! receives an explicit center, channel, gain and duration. On error, the
//! request and any available native output stay in that directory.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::json;

use crate::{
    MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE, budget, json_file, new_file, receiver, spectrum,
};

const IQ_FILE_NAME: &str = "wideband.sc16q11";

#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub(crate) center: u64,
    pub(crate) duration: f64,
    pub(crate) samples: u64,
    pub(crate) antenna: u8,
    pub(crate) gain: f64,
}

impl Settings {
    pub(crate) fn new(center: u64, duration: f64, antenna: u8, gain: f64) -> Result<Self> {
        ensure!(
            (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&center),
            "center must be 70..=6000 MHz"
        );
        budget(duration, 10.0)?;
        ensure!((1..=2).contains(&antenna), "select RX1 or RX2");
        ensure!((0.0..=71.0).contains(&gain), "gain must be 0..=71 dB");
        let samples = (duration * f64::from(SAMPLE_RATE)).round() as u64;
        ensure!(
            samples >= spectrum::FFT as u64,
            "capture requires at least {} samples for its spectrum report",
            spectrum::FFT
        );
        Ok(Self {
            center,
            duration,
            samples,
            antenna,
            gain,
        })
    }

    pub(crate) fn write_request(self, output: &Path) -> Result<()> {
        json_file(
            &output.join("request.json"),
            &json!({
                "center_frequency_hz": self.center,
                "sample_rate_hz": SAMPLE_RATE,
                "samples_requested": self.samples,
                "antenna_requested": self.antenna,
                "gain_db_requested": self.gain,
                "mode": "hosted_iq_capture_not_custom_fpga_channelizer",
            }),
        )
    }
}

pub(crate) fn resolve_program(root: &Path, chosen: Option<&Path>) -> Result<PathBuf> {
    let path = chosen.map_or_else(
        || root.join("target/release/bladerf-lab"),
        Path::to_path_buf,
    );
    let program = path
        .canonicalize()
        .with_context(|| format!("resolve native acquisition binary {}", path.display()))?;
    ensure!(
        program.is_file() && fs::metadata(&program)?.permissions().mode() & 0o111 != 0,
        "native acquisition binary must be an executable file"
    );
    Ok(program)
}

pub(crate) struct Window {
    pub(crate) iq: PathBuf,
    pub(crate) center_hz: u64,
}

/// Capture one window, validate its readback and write the bounded spectrum.
///
/// An interrupt requests adapter shutdown and waits for the child before
/// returning. Errors preserve available evidence for the caller to inspect.
pub(crate) fn record(
    root: &Path,
    program: &Path,
    output: &Path,
    settings: Settings,
    stop: &AtomicBool,
) -> Result<Window> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "acquisition interrupted before startup"
    );
    let Settings {
        center,
        duration,
        antenna,
        gain,
        ..
    } = settings;
    let iq = output.join(IQ_FILE_NAME);
    settings.write_request(output)?;
    let log = new_file(&output.join("acquisition.log"))?;
    let mut command = Command::new(program);
    command
        .arg("--workspace")
        .arg(root)
        .args([
            "capture",
            "--frequency",
            &center.to_string(),
            "--sample-rate",
            &SAMPLE_RATE.to_string(),
            "--channel",
            &antenna.to_string(),
            "--gain",
            &gain.to_string(),
            "--duration",
            &duration.to_string(),
            "--output",
        ])
        .arg(&iq)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = AcquisitionChild::new(command.spawn().context("start native acquisition")?);
    let start = Instant::now();
    let result = loop {
        if let Some(status) = child.try_wait()? {
            break if status.success() {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "acquisition failed ({status}); see {}",
                    output.join("acquisition.log").display()
                ))
            };
        }
        if stop.load(Ordering::Relaxed) || start.elapsed().as_secs_f64() > duration + 20.0 {
            let forced = child.stop()?;
            json_file(
                &output.join("acquisition-failure.json"),
                &json!({"interrupted":stop.load(Ordering::Relaxed),"forced_kill":forced}),
            )?;
            break Err(anyhow::anyhow!(
                "acquisition interrupted or timed out; evidence in {}",
                output.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    result?;
    analyze_recording(output, settings, stop)
}

/// The native worker publishes the sidecar after the final raw IQ file.
pub(crate) fn recording_ready(output: &Path) -> io::Result<bool> {
    receiver::acquisition_sidecar(&output.join(IQ_FILE_NAME)).try_exists()
}

/// Readback and extent of a closed native IQ recording.
pub(crate) struct Recording<'a> {
    output: &'a Path,
    iq: PathBuf,
    metadata: receiver::Acquisition,
    samples: u64,
}

impl<'a> Recording<'a> {
    /// Validate the native sidecar and IQ byte extent.
    ///
    /// # Errors
    /// Returns an error if either file is missing or inconsistent with the request.
    pub(crate) fn open(output: &'a Path, settings: Settings) -> Result<Self> {
        let iq = output.join(IQ_FILE_NAME);
        let metadata = receiver::validate_acquisition(
            &iq,
            settings.center,
            settings.samples,
            settings.antenna,
            settings.gain,
        )
        .with_context(|| {
            format!(
                "validate acquisition; source and logs retained in {}",
                output.display()
            )
        })?;
        Ok(Self {
            output,
            iq,
            metadata,
            samples: settings.samples,
        })
    }

    pub(crate) fn center_hz(&self) -> u64 {
        self.metadata.frequency_hz()
    }

    pub(crate) fn sample_timing(&self) -> Option<receiver::SampleTiming> {
        self.metadata.sample_timing()
    }

    /// Inspect sparse windows at the recorded tuning, validating each sampled IQ component.
    ///
    /// # Errors
    /// Returns an error on invalid sampled IQ, input failure or interruption.
    pub(crate) fn survey(&self, stop: &AtomicBool) -> Result<spectrum::Report> {
        spectrum::survey(&self.iq, self.center_hz(), self.samples, 128, stop)
    }

    /// Write the validated readback as `capture.json`.
    ///
    /// # Errors
    /// Returns an error if the metadata file cannot be exclusively created and synced.
    pub(crate) fn write_metadata(&self) -> Result<()> {
        json_file(&self.output.join("capture.json"), &self.metadata)
    }
}

fn analyze_recording(output: &Path, settings: Settings, stop: &AtomicBool) -> Result<Window> {
    let recording = Recording::open(output, settings)?;
    let survey = recording.survey(stop)?;
    recording.write_metadata()?;
    json_file(&output.join("spectrum.json"), &survey)?;
    Ok(Window {
        center_hz: recording.center_hz(),
        iq: recording.iq,
    })
}

/// Own and reap a synchronous native acquisition process. Explicit stop first
/// allows native SIGINT cleanup, then kills a stalled worker. Fallback Drop
/// cleanup kills and waits and may block in operating-system teardown.
pub(crate) struct AcquisitionChild {
    child: Child,
    reaped: bool,
}
impl AcquisitionChild {
    pub(crate) const fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped = status.is_some();
        Ok(status)
    }

    pub(crate) fn stop(&mut self) -> Result<bool> {
        if self.try_wait()?.is_some() {
            return Ok(false);
        }
        let pid = Pid::from_raw(
            i32::try_from(self.child.id()).context("native worker PID out of range")?,
        );
        match kill(pid, Signal::SIGINT) {
            Ok(()) | Err(Errno::ESRCH) => {}
            Err(error) => return Err(error).context("interrupt native worker"),
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.try_wait()?.is_some() {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.child.kill()?;
        self.child.wait()?;
        self.reaped = true;
        Ok(true)
    }
}
impl Drop for AcquisitionChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_worker_interrupt_is_reaped() {
        let mut child = AcquisitionChild::new(Command::new("sleep").arg("5").spawn().unwrap());
        assert!(!child.stop().unwrap());
        assert!(child.reaped);
    }
}
