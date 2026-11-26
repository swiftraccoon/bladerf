//! Record one or more finite passes on one open stock-image receiver.
//!
//! Each dwell closes its receive stream before the next retune. Earlier complete
//! IQ and sidecars remain available if a later dwell fails or is interrupted.

use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::Serialize;
use soapysdr::Direction::Rx;

use super::{
    CaptureContinuity, CaptureFiles, CaptureMetadata, CaptureSession, DurationSeconds, FpgaVersion,
    FrequencyHz, RadioArgs, STOCK_FPGA_VERSION, SampleCount, SampleRateHz, interrupt_flag,
    require_absent, sample_count,
};

const SAMPLE_RATE_HZ: f64 = 20_000_000.0;
const MAX_TUNES: usize = 500;
const MAX_PASSES: usize = 16;
const MAX_TOTAL_DWELLS: usize = 2_000;
const MAX_DWELL_MS: u32 = 10_000;
const MAX_DWELL_SECONDS: f64 = MAX_DWELL_MS as f64 / 1_000.0;
const TIMING_FILE: &str = "acquisition-timing.json";

/// Host-clock offsets within one receiver batch.
#[derive(Serialize)]
struct SweepTiming {
    schema_version: u32,
    operation: &'static str,
    device_ready_ns: u64,
    device_closed_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    passes: Option<usize>,
    dwells: Vec<DwellTiming>,
}

#[derive(Serialize)]
struct DwellTiming {
    requested_center_hz: u64,
    started_ns: u64,
    tuned_ns: u64,
    files_ready_ns: u64,
    stream_ready_ns: u64,
    read_started_ns: u64,
    read_finished_ns: u64,
    stream_closed_ns: u64,
    published_ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct DwellOverride {
    center_hz: u64,
    duration: DurationSeconds,
}

impl DwellOverride {
    fn parse(value: &str) -> Result<Self> {
        let (center, milliseconds) = value
            .split_once(':')
            .context("expected a dwell override as CENTER_HZ:MS")?;
        let center_hz = center
            .parse()
            .context("invalid dwell override center in hertz")?;
        let milliseconds: u32 = milliseconds
            .parse()
            .context("invalid dwell override duration in milliseconds")?;
        ensure!(
            (1..=MAX_DWELL_MS).contains(&milliseconds),
            "sweep dwell override must be 1 through {MAX_DWELL_MS} milliseconds"
        );
        let duration = DurationSeconds::try_from(f64::from(milliseconds) / 1_000.0)?;
        Ok(Self {
            center_hz,
            duration,
        })
    }
}

struct Dwell {
    center_hz: u64,
    duration: DurationSeconds,
    samples: SampleCount,
}

#[derive(Args, Debug)]
pub(crate) struct SweepCaptureArgs {
    /// Center in integer hertz. Repeat in strictly increasing order, at most 500 times.
    #[arg(long = "center-hz", required = true)]
    centers_hz: Vec<u64>,
    /// Center to visit in each pass. Repeat in visit order, including every center at least once.
    #[arg(long = "visit-hz", value_name = "HZ")]
    visit_centers_hz: Vec<u64>,
    /// Number of times to repeat the visit order on one receiver handle.
    #[arg(long, default_value_t = 1)]
    passes: usize,
    /// Physical receive port, RX1 or RX2.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=2))]
    channel: u8,
    /// Manual receive gain in dB, checked at each center.
    #[arg(long, allow_negative_numbers = true)]
    gain: f64,
    /// Default capture duration per tune, in seconds. At most ten seconds at 20 MSps.
    #[arg(long)]
    duration: f64,
    /// Override a requested center's duration with 1 through 10,000 milliseconds. Repeat for distinct centers.
    #[arg(long, value_name = "CENTER_HZ:MS", value_parser = DwellOverride::parse)]
    dwell_at: Vec<DwellOverride>,
    /// Verify device timestamp continuity within each RX1 dwell on Linux GNU.
    #[arg(long)]
    check_continuity: bool,
    /// Existing private directory for numbered IQ and sidecar pairs.
    #[arg(long)]
    output: PathBuf,
}

impl SweepCaptureArgs {
    fn validate(&self) -> Result<(Vec<Dwell>, RadioArgs)> {
        if self.check_continuity {
            ensure!(self.channel == 1, "timestamp-checked sweeps require RX1");
            ensure!(
                cfg!(all(target_os = "linux", target_env = "gnu")),
                "timestamp-checked sweeps require Linux GNU"
            );
        }
        ensure!(
            (1..=MAX_TUNES).contains(&self.centers_hz.len()),
            "sweep requires 1 through {MAX_TUNES} centers"
        );
        ensure!(
            self.centers_hz.is_sorted_by(|left, right| left < right),
            "sweep centers must be strictly increasing"
        );
        ensure!(
            (1..=MAX_PASSES).contains(&self.passes),
            "sweep requires 1 through {MAX_PASSES} passes"
        );
        let visits = match self.visit_centers_hz.as_slice() {
            [] => &self.centers_hz,
            visits => visits,
        };
        ensure!(
            visits.len() <= MAX_TUNES,
            "sweep permits at most {MAX_TUNES} visits per pass"
        );
        ensure!(
            visits.len() * self.passes <= MAX_TOTAL_DWELLS,
            "sweep requires at most {MAX_TOTAL_DWELLS} total dwells"
        );
        for &center in &self.centers_hz {
            FrequencyHz::try_from(center as f64)?;
        }
        let duration = DurationSeconds::try_from(self.duration)?;
        ensure!(
            duration.0 <= MAX_DWELL_SECONDS,
            "sweep duration per tune must be at most {MAX_DWELL_SECONDS} seconds"
        );
        let rate = SampleRateHz(SAMPLE_RATE_HZ);
        let samples = sample_count(rate, duration)?;
        let mut overrides = vec![None; self.centers_hz.len()];
        for &dwell in &self.dwell_at {
            let index = self
                .centers_hz
                .binary_search(&dwell.center_hz)
                .map_err(|_| {
                    anyhow::anyhow!(
                        "dwell override center {} Hz requires a matching --center-hz",
                        dwell.center_hz
                    )
                })?;
            ensure!(
                overrides[index].is_none(),
                "duplicate dwell override for {} Hz",
                dwell.center_hz
            );
            overrides[index] = Some((dwell.duration, sample_count(rate, dwell.duration)?));
        }
        let mut visited = vec![false; self.centers_hz.len()];
        let mut dwells = Vec::with_capacity(visits.len());
        for &center_hz in visits {
            let index = self.centers_hz.binary_search(&center_hz).map_err(|_| {
                anyhow::anyhow!("visit at {center_hz} Hz requires a matching --center-hz")
            })?;
            visited[index] = true;
            let (duration, samples) = overrides[index].unwrap_or((duration, samples));
            dwells.push(Dwell {
                center_hz,
                duration,
                samples,
            });
        }
        ensure!(
            visited.into_iter().all(|center_visited| center_visited),
            "visit order must include every --center-hz"
        );
        let first = dwells.first().context("missing initial visit")?;
        let radio = RadioArgs {
            frequency: first.center_hz as f64,
            sample_rate: SAMPLE_RATE_HZ,
            gain: self.gain,
            channel: self.channel,
            serial: None,
        };
        radio.validate()?;
        Ok((dwells, radio))
    }
}

fn tune_path(output: &Path, index: usize) -> PathBuf {
    output.join(format!("tune-{:04}", index + 1))
}

fn pass_path(output: &Path, index: usize) -> PathBuf {
    output.join(format!("pass-{:04}", index + 1))
}

fn validate_output(output: &Path, tunes: usize, passes: usize) -> Result<()> {
    let metadata = fs::symlink_metadata(output)
        .with_context(|| format!("inspect native sweep output {}", output.display()))?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
        "native sweep output must be an existing real directory with mode 0700"
    );
    if passes == 1 {
        for index in 0..tunes {
            require_absent(&tune_path(output, index))?;
        }
    } else {
        for index in 0..passes {
            require_absent(&pass_path(output, index))?;
        }
    }
    require_absent(&output.join(TIMING_FILE))?;
    Ok(())
}

fn validate_dwell(
    readback: &CaptureMetadata,
    center_hz: u64,
    samples: SampleCount,
    gain: f64,
) -> Result<()> {
    // The observatory independently applies these bounds to completed IQ.
    ensure!(
        (readback.frequency_hz - center_hz as f64).abs() <= 10.0
            && (readback.sample_rate_hz - SAMPLE_RATE_HZ).abs() <= 1.0
            && (readback.bandwidth_hz - SAMPLE_RATE_HZ).abs() <= 1.0
            && (readback.gain_db - gain).abs() <= 0.01,
        "native sweep tuning, rate, bandwidth, or gain differs from request"
    );
    ensure!(
        readback.num_samples == samples.get(),
        "native sweep would save {} samples instead of the planned {}",
        readback.num_samples,
        samples.get()
    );
    Ok(())
}

fn offset_ns(origin: Instant, mark: Instant) -> Result<u64> {
    let offset = mark
        .checked_duration_since(origin)
        .context("native sweep timing mark predates the batch start")?;
    u64::try_from(offset.as_nanos()).context("native sweep timing exceeds u64 nanoseconds")
}

fn publish_timing(output: &Path, timing: &SweepTiming) -> Result<()> {
    let mut staged =
        tempfile::NamedTempFile::new_in(output).context("stage native sweep timing")?;
    serde_json::to_writer_pretty(&mut staged, timing).context("write native sweep timing")?;
    writeln!(staged)?;
    staged
        .as_file()
        .sync_all()
        .context("sync native sweep timing")?;
    staged
        .persist_noclobber(output.join(TIMING_FILE))
        .context("publish native sweep timing without replacing output")?;
    fs::File::open(output)?
        .sync_all()
        .context("sync native sweep output directory")
}

/// Keep the device open across dwells and publish each completed IQ pair.
///
/// Input or destination checks fail before radio open. Stream, tuning, and
/// publication errors stop the sweep, keeping earlier complete pairs in place.
/// Successful acquisition also records monotonic host phase offsets after the
/// receiver closes. These offsets carry no RF sample timestamps.
pub(crate) fn capture_sweep(args: SweepCaptureArgs) -> Result<()> {
    let (plan, radio) = args.validate()?;
    let (continuity, required_fpga_version) = if args.check_continuity {
        let required: FpgaVersion = STOCK_FPGA_VERSION.parse().map_err(anyhow::Error::msg)?;
        (CaptureContinuity::Checked, Some(required))
    } else {
        (CaptureContinuity::Unchecked, None)
    };
    let tune_count = plan.len();
    let dwell_count = tune_count * args.passes;
    validate_output(&args.output, tune_count, args.passes)?;
    let interrupted = interrupt_flag()?;
    let origin = Instant::now();
    let device = radio.open(required_fpga_version.as_ref())?;
    let device_ready_ns = offset_ns(origin, Instant::now())?;
    let mut dwells = Vec::with_capacity(dwell_count);
    {
        let mut session = CaptureSession::new(
            &device,
            &radio,
            required_fpga_version.as_ref(),
            &interrupted,
            continuity,
        );
        let mut directory = args.output.clone();
        for (index, dwell) in plan.iter().cycle().take(dwell_count).enumerate() {
            let center = dwell.center_hz;
            ensure!(
                !interrupted.load(Ordering::Relaxed),
                "capture sweep interrupted before tuning to {center} Hz"
            );
            let started = Instant::now();
            let tuned = if index == 0 {
                started
            } else {
                device
                    .set_frequency(Rx, radio.index(), center as f64, ())
                    .with_context(|| format!("retune RX{} to {center} Hz", radio.channel))?;
                radio
                    .set_manual_gain(&device)
                    .with_context(|| format!("set gain at {center} Hz"))?;
                Instant::now()
            };
            let tune_index = index % tune_count;
            if args.passes > 1 && tune_index == 0 {
                directory = pass_path(&args.output, index / tune_count);
                DirBuilder::new()
                    .mode(0o700)
                    .create(&directory)
                    .with_context(|| format!("create new sweep pass {}", directory.display()))?;
            }
            let path = tune_path(&directory, tune_index);
            DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .with_context(|| format!("create new tune directory {}", path.display()))?;
            let files = CaptureFiles::create(&path.join("wideband.sc16q11"))?;
            let files_ready = Instant::now();
            let capture = session
                .record(files, dwell.duration, 0, |readback| {
                    validate_dwell(readback, center, dwell.samples, radio.gain)
                })
                .with_context(|| {
                    if args.passes == 1 {
                        format!("capture tune {} at {center} Hz", tune_index + 1)
                    } else {
                        format!(
                            "capture pass {} tune {} at {center} Hz",
                            index / tune_count + 1,
                            tune_index + 1,
                        )
                    }
                })?;
            dwells.push(DwellTiming {
                requested_center_hz: center,
                started_ns: offset_ns(origin, started)?,
                tuned_ns: offset_ns(origin, tuned)?,
                files_ready_ns: offset_ns(origin, files_ready)?,
                stream_ready_ns: offset_ns(origin, capture.timing.stream_ready)?,
                read_started_ns: offset_ns(origin, capture.timing.read_started)?,
                read_finished_ns: offset_ns(origin, capture.timing.read_finished)?,
                stream_closed_ns: offset_ns(origin, capture.timing.stream_closed)?,
                published_ns: offset_ns(origin, capture.timing.published)?,
            });
        }
    }
    drop(device);
    let device_closed_ns = offset_ns(origin, Instant::now())?;
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "capture sweep interrupted before timing publication"
    );
    publish_timing(
        &args.output,
        &SweepTiming {
            schema_version: 1,
            operation: "native_rf_sweep_timing",
            device_ready_ns,
            device_closed_ns,
            passes: (args.passes > 1).then_some(args.passes),
            dwells,
        },
    )?;
    let mut stdout = io::stdout().lock();
    if args.passes == 1 {
        writeln!(
            stdout,
            "Captured {tune_count} tunes on RX{} at {}",
            radio.channel,
            args.output.display()
        )
    } else {
        writeln!(
            stdout,
            "Captured {} passes with {dwell_count} dwells on RX{} at {}",
            args.passes,
            radio.channel,
            args.output.display()
        )
    }
    .context("write native sweep summary")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use clap::Parser;

    use super::*;
    use crate::{Cli, Command};

    fn args(output: &Path) -> SweepCaptureArgs {
        SweepCaptureArgs {
            centers_hz: vec![70_000_000, 6_000_000_000],
            visit_centers_hz: Vec::new(),
            passes: 1,
            channel: 1,
            gain: 40.0,
            duration: 0.04,
            dwell_at: Vec::new(),
            check_continuity: false,
            output: output.to_path_buf(),
        }
    }

    #[test]
    fn timing_receipt_is_private_and_a_collision_preserves_existing_bytes() {
        let output = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let timing = SweepTiming {
            schema_version: 1,
            operation: "native_rf_sweep_timing",
            device_ready_ns: 1,
            device_closed_ns: 9,
            passes: None,
            dwells: vec![DwellTiming {
                requested_center_hz: 90_000_000,
                started_ns: 2,
                tuned_ns: 2,
                files_ready_ns: 3,
                stream_ready_ns: 4,
                read_started_ns: 5,
                read_finished_ns: 6,
                stream_closed_ns: 7,
                published_ns: 8,
            }],
        };
        validate_output(output.path(), 1, 1).unwrap();
        publish_timing(output.path(), &timing).unwrap();
        let path = output.path().join(TIMING_FILE);
        let original = fs::read(&path).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(document["operation"], "native_rf_sweep_timing");
        assert_eq!(document["dwells"][0]["published_ns"], 8);
        assert!(document.get("passes").is_none());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert!(validate_output(output.path(), 1, 1).is_err());

        let replacement = SweepTiming {
            device_closed_ns: 10,
            ..timing
        };
        assert!(publish_timing(output.path(), &replacement).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn invalid_sweep_extents_fail_before_radio_open() {
        let output = tempfile::tempdir().unwrap();
        let valid = args(output.path());
        valid.validate().unwrap();
        for centers_hz in [
            vec![],
            vec![70_000_000; MAX_TUNES + 1],
            vec![90_000_000, 80_000_000],
            vec![80_000_000, 80_000_000],
            vec![69_999_999],
            vec![6_000_000_001],
        ] {
            assert!(
                SweepCaptureArgs {
                    centers_hz,
                    ..args(output.path())
                }
                .validate()
                .is_err()
            );
        }
        for duration in [0.0, 1e-10, 10.001, f64::NAN, f64::INFINITY] {
            assert!(
                SweepCaptureArgs {
                    duration,
                    ..args(output.path())
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            SweepCaptureArgs {
                channel: 0,
                ..args(output.path())
            }
            .validate()
            .is_err()
        );
        assert!(
            SweepCaptureArgs {
                gain: f64::NAN,
                ..args(output.path())
            }
            .validate()
            .is_err()
        );
        assert!(output.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn repeated_sweep_checks_pass_and_total_dwell_limits() {
        let output = tempfile::tempdir().unwrap();
        for passes in [0, MAX_PASSES + 1, usize::MAX] {
            let result = SweepCaptureArgs {
                passes,
                ..args(output.path())
            }
            .validate();
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("1 through 16 passes")
            );
        }

        let mut request = SweepCaptureArgs {
            passes: MAX_PASSES,
            ..args(output.path())
        };
        request.validate().unwrap();
        request.centers_hz = (70_000_000..).take(MAX_TUNES).collect();
        request.passes = MAX_TOTAL_DWELLS / MAX_TUNES;
        request.validate().unwrap();
        request.passes += 1;
        assert!(
            request
                .validate()
                .err()
                .unwrap()
                .to_string()
                .contains("2000 total dwells")
        );
    }

    #[test]
    fn sweep_cli_accepts_repeated_integer_centers() {
        let cli = Cli::try_parse_from([
            "bladerf-lab",
            "capture-sweep",
            "--output",
            "captures/example",
            "--channel",
            "2",
            "--gain",
            "40",
            "--duration",
            "0.04",
            "--center-hz",
            "900000000",
            "--center-hz",
            "920000000",
        ])
        .unwrap();
        let Command::CaptureSweep(request) = cli.command else {
            panic!("capture-sweep command");
        };
        assert_eq!(request.centers_hz, [900_000_000, 920_000_000]);
        assert_eq!(request.channel, 2);
        assert_eq!(request.passes, 1);
        assert!(!request.check_continuity);
        request.validate().unwrap();
    }

    #[test]
    fn timestamp_checked_sweep_rejects_rx2_before_output_preflight() {
        let output = tempfile::tempdir().unwrap();
        let absent = output.path().join("absent");
        let error = capture_sweep(SweepCaptureArgs {
            check_continuity: true,
            channel: 2,
            ..args(&absent)
        })
        .unwrap_err();
        assert!(error.to_string().contains("require RX1"));
        assert!(!absent.exists());
    }

    #[test]
    fn sweep_cli_applies_dwell_overrides_in_requested_center_order() {
        let cli = Cli::try_parse_from([
            "bladerf-lab",
            "capture-sweep",
            "--output",
            "captures/example",
            "--channel",
            "1",
            "--gain",
            "40",
            "--duration",
            "0.04",
            "--passes",
            "2",
            "--center-hz",
            "900000000",
            "--center-hz",
            "920000000",
            "--center-hz",
            "940000000",
            "--dwell-at",
            "940000000:10000",
            "--dwell-at",
            "900000000:1",
        ])
        .unwrap();
        let Command::CaptureSweep(request) = cli.command else {
            panic!("capture-sweep command");
        };
        let (plan, radio) = request.validate().unwrap();
        assert_eq!(radio.frequency, 900_000_000.0);
        assert_eq!(request.passes, 2);
        let dwells: Vec<_> = plan
            .iter()
            .map(|dwell| (dwell.center_hz, dwell.duration.0, dwell.samples.get()))
            .collect();
        assert_eq!(
            dwells,
            [
                (900_000_000, 0.001, 20_000),
                (920_000_000, 0.04, 800_000),
                (940_000_000, 10.0, 200_000_000),
            ]
        );
    }

    #[test]
    fn sweep_cli_uses_visit_order_and_reuses_the_center_dwell_on_revisits() {
        let arguments = [
            "bladerf-lab",
            "capture-sweep",
            "--output",
            "captures/example",
            "--channel",
            "1",
            "--gain",
            "40",
            "--duration",
            "0.04",
            "--center-hz",
            "900000000",
            "--center-hz",
            "920000000",
            "--dwell-at",
            "920000000:15",
        ];
        assert!(Cli::try_parse_from(arguments.into_iter().chain(["--visit-hz"])).is_err());
        let cli = Cli::try_parse_from(arguments.into_iter().chain([
            "--visit-hz",
            "920000000",
            "--visit-hz",
            "900000000",
            "--visit-hz",
            "920000000",
        ]))
        .unwrap();
        let Command::CaptureSweep(request) = cli.command else {
            panic!("capture-sweep command");
        };
        let (plan, radio) = request.validate().unwrap();
        assert_eq!(radio.frequency, 920_000_000.0);
        let dwells: Vec<_> = plan
            .iter()
            .map(|dwell| (dwell.center_hz, dwell.samples.get()))
            .collect();
        assert_eq!(
            dwells,
            [
                (920_000_000, 300_000),
                (900_000_000, 800_000),
                (920_000_000, 300_000),
            ]
        );
    }

    #[test]
    fn ordered_visits_require_every_center_and_bound_actual_dwell_counts() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("uncreated");
        for (visit_centers_hz, expected) in [
            (vec![71_000_000], "requires a matching --center-hz"),
            (vec![70_000_000; 2], "must include every --center-hz"),
            (vec![70_000_000; MAX_TUNES + 1], "500 visits per pass"),
        ] {
            let request = SweepCaptureArgs {
                visit_centers_hz,
                ..args(&absent)
            };
            let error = capture_sweep(request).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }

        let mut request = SweepCaptureArgs {
            visit_centers_hz: vec![70_000_000; MAX_TUNES - 1],
            passes: MAX_TOTAL_DWELLS / MAX_TUNES,
            ..args(&absent)
        };
        request.visit_centers_hz.push(6_000_000_000);
        assert_eq!(request.validate().unwrap().0.len(), MAX_TUNES);
        request.passes += 1;
        let error = capture_sweep(request).unwrap_err();
        assert!(error.to_string().contains("2000 total dwells"));
        assert!(!absent.exists());
    }

    #[test]
    fn malformed_duplicate_and_unknown_dwell_overrides_are_rejected() {
        for invalid in [
            "70000000:0",
            "70000000:10001",
            "70000000:1.5",
            "70000000:1:2",
            "70e6:1",
        ] {
            assert!(DwellOverride::parse(invalid).is_err());
        }
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("uncreated");
        let first = DwellOverride::parse("70000000:20").unwrap();
        let duplicate = DwellOverride::parse("70000000:21").unwrap();
        let unknown = DwellOverride::parse("71000000:20").unwrap();
        for (dwell_at, message) in [
            (vec![first, duplicate], "duplicate dwell override"),
            (vec![unknown], "requires a matching --center-hz"),
        ] {
            let error = capture_sweep(SweepCaptureArgs {
                dwell_at,
                ..args(&absent)
            })
            .unwrap_err();
            assert!(error.to_string().contains(message));
        }
        assert!(!absent.exists());
    }

    #[test]
    fn readback_must_cover_the_requested_center_without_exceeding_the_sample_budget() {
        let mut readback = CaptureMetadata {
            format: "SC16_Q11".to_owned(),
            dtype: "<i2".to_owned(),
            interleave: "I,Q".to_owned(),
            complex_scale: 2048,
            frequency_hz: 890_000_010.0,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bandwidth_hz: SAMPLE_RATE_HZ,
            gain_db: 40.0,
            gain_mode: "manual".to_owned(),
            bias_tee_enabled: false,
            channel: "RX1".to_owned(),
            device_serial: None,
            firmware_version: None,
            fpga_version: None,
            required_fpga_version: None,
            num_samples: 800_000,
            duration_seconds: 0.04,
            discarded_initial_samples: None,
            started_utc: String::new(),
            timestamp_note: String::new(),
            continuity_note: String::new(),
            sample_timing: None,
            receive_api: String::new(),
        };
        let samples = SampleCount::try_from(800_000).unwrap();
        validate_dwell(&readback, 890_000_000, samples, 40.0).unwrap();
        readback.num_samples += 1;
        let error = validate_dwell(&readback, 890_000_000, samples, 40.0).unwrap_err();
        assert!(error.to_string().contains("instead of the planned 800000"));
        readback.num_samples = 800_000;
        readback.frequency_hz += 1.0;
        assert!(validate_dwell(&readback, 890_000_000, samples, 40.0).is_err());
    }

    #[test]
    fn output_preflight_preserves_existing_tunes_and_rejects_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let link = root.path().join("link");
        DirBuilder::new().mode(0o700).create(&output).unwrap();
        symlink(&output, &link).unwrap();
        let error = validate_output(&link, 2, 1).unwrap_err();
        assert!(error.to_string().contains("existing real directory"));
        validate_output(&output, 2, 1).unwrap();
        let existing = tune_path(&output, 1);
        fs::create_dir(&existing).unwrap();
        let evidence = existing.join("sentinel");
        fs::write(&evidence, b"keep").unwrap();
        let error = validate_output(&output, 2, 1).unwrap_err();
        assert!(error.to_string().contains("output already exists"));
        assert_eq!(fs::read(&evidence).unwrap(), b"keep");
        assert!(!tune_path(&output, 0).exists());
    }

    #[test]
    fn repeated_sweep_preflights_all_pass_paths_without_creating_them() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        DirBuilder::new().mode(0o700).create(&output).unwrap();
        validate_output(&output, 2, 2).unwrap();
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);

        let existing = pass_path(&output, 1);
        symlink(root.path(), &existing).unwrap();
        let error = validate_output(&output, 2, 2).unwrap_err();
        assert!(error.to_string().contains("pass-0002"));
        assert_eq!(fs::read_link(&existing).unwrap(), root.path());
        assert!(!pass_path(&output, 0).exists());
    }
}
