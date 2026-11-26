//! Replay a known sensor channel from guard-safe dwells in a completed capture.

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::dsp::Iq16;
use crate::private_output::publish_json_file;
use crate::{MAX_SAMPLES, SAMPLE_RATE, efergy, rs41, tpms, wh51, ws90};

use super::admission::{CompletedCapture, Tune};
use super::check_stop;
use super::replay::shared::{self, Request, StreamingTask, Task};
use super::replay::{BudgetUnit, Work};

/// Maximum original-rate samples charged across a sensor channel's dwells.
pub(super) const MAX_TOTAL_SAMPLES: u64 = 2_000_000_000;

/// Sensor decoder selected for one channel replay.
#[derive(Clone, Copy)]
pub(crate) enum Sensor {
    Efergy,
    Wh51,
    Ws90,
    Tpms,
    Rs41,
}

impl Sensor {
    fn name(self) -> &'static str {
        match self {
            Self::Efergy => "Efergy",
            Self::Wh51 => "WH51",
            Self::Ws90 => "WS90",
            Self::Tpms => "TPMS",
            Self::Rs41 => "RS41",
        }
    }

    fn operation(self) -> &'static str {
        match self {
            Self::Efergy => "identify_efergy_in_saved_sweeps",
            Self::Wh51 => "identify_wh51_in_saved_sweeps",
            Self::Ws90 => "identify_ws90_in_saved_sweeps",
            Self::Tpms => "identify_tpms_in_saved_sweeps",
            Self::Rs41 => "identify_rs41_in_saved_sweeps",
        }
    }

    fn default_channel_hz(self) -> Option<u64> {
        match self {
            Self::Efergy => Some(efergy::DEFAULT_CHANNEL_HZ),
            Self::Wh51 => Some(wh51::DEFAULT_CHANNEL_HZ),
            Self::Ws90 => Some(ws90::DEFAULT_CHANNEL_HZ),
            Self::Tpms => Some(tpms::DEFAULT_CHANNEL_HZ),
            Self::Rs41 => None,
        }
    }

    fn minimum_dwell_samples(self) -> u64 {
        match self {
            Self::Efergy => efergy::MIN_SWEEP_SAMPLES,
            Self::Wh51 => wh51::MIN_SWEEP_SAMPLES,
            Self::Ws90 => ws90::MIN_SWEEP_SAMPLES,
            Self::Tpms => tpms::MIN_SWEEP_SAMPLES,
            Self::Rs41 => rs41::MIN_SWEEP_SAMPLES,
        }
    }
}

/// Select a completed capture, sensor channel and replay limits.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or series receipt and its saved IQ.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Known channel center in hertz. Required for RS41. Defaults to 433.55 MHz for Efergy,
    /// 915 MHz for WH51 and WS90, or 315 MHz for TPMS.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    channel_hz: Option<u64>,
    /// Maximum original-rate IQ samples to replay per eligible dwell.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    max_samples_per_dwell: u64,
    /// Refuse a replay plan above this total sample count across eligible dwells.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=MAX_TOTAL_SAMPLES))]
    max_total_samples: u64,
    /// Maximum retained frame records across the entire capture.
    #[arg(long, default_value_t = wh51::DEFAULT_RETAINED_FRAMES, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=4_096))]
    max_frames: usize,
    /// New private JSON report in an existing directory.
    #[arg(long)]
    output: PathBuf,
}

/// Capture-bound sensor frame counts and per-dwell replay evidence.
#[derive(Serialize)]
pub(super) struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    channel_frequency_hz: u64,
    sample_rate_hz: u32,
    minimum_dwell_samples: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_frames: usize,
    pass_count: usize,
    dwell_count: usize,
    covered_dwells: usize,
    short_dwells: usize,
    replayed_dwells: usize,
    replayed_iq_samples: u64,
    validated_frames: usize,
    retained_frames: usize,
    omitted_valid_frames: usize,
    dwells: Vec<Attempt<'a>>,
}

impl Report<'_> {
    /// Count validated frame occurrences, including those omitted by the retention cap.
    pub(super) fn validated_frames(&self) -> usize {
        self.validated_frames
    }
}

#[derive(Serialize)]
struct Attempt<'a> {
    pass_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune_directory: &'a str,
    source_sidecar_sha256: &'a str,
    observed_center_hz: u64,
    saved_samples: u64,
    selected_samples: u64,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    ShortDwell,
    Replayed { replay: Reading },
}

#[derive(Serialize)]
#[serde(untagged)]
enum Reading {
    Efergy(efergy::Report),
    Wh51(wh51::Report),
    Ws90(ws90::Report),
    Tpms(tpms::Report),
    Rs41(rs41::Report),
}

impl Reading {
    fn frame_counts(&self) -> (usize, usize) {
        match self {
            Self::Efergy(report) => (report.validated_frames(), report.retained_frames()),
            Self::Wh51(report) => (report.validated_frames(), report.retained_frames()),
            Self::Ws90(report) => (report.validated_frames(), report.retained_frames()),
            Self::Tpms(report) => (report.validated_frames(), report.retained_frames()),
            Self::Rs41(report) => (report.validated_frames(), report.retained_frames()),
        }
    }
}

#[derive(Clone, Copy)]
enum Channel {
    Efergy(efergy::Channel),
    Wh51(wh51::Channel),
    Ws90(ws90::Channel),
    Tpms(tpms::Channel),
    Rs41(rs41::Channel),
}

impl Channel {
    fn new(sensor: Sensor, center_hz: u64, channel_hz: u64) -> Self {
        match sensor {
            Sensor::Efergy => Self::Efergy(efergy::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }),
            Sensor::Wh51 => Self::Wh51(wh51::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }),
            Sensor::Ws90 => Self::Ws90(ws90::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }),
            Sensor::Tpms => Self::Tpms(tpms::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }),
            Sensor::Rs41 => Self::Rs41(rs41::Channel {
                center_hz,
                channel_hz,
                sample_rate_hz: SAMPLE_RATE,
            }),
        }
    }

    fn sensor(self) -> Sensor {
        match self {
            Self::Efergy(_) => Sensor::Efergy,
            Self::Wh51(_) => Sensor::Wh51,
            Self::Ws90(_) => Sensor::Ws90,
            Self::Tpms(_) => Sensor::Tpms,
            Self::Rs41(_) => Sensor::Rs41,
        }
    }

    fn is_covered(self) -> bool {
        match self {
            Self::Efergy(channel) => channel.is_covered(),
            Self::Wh51(channel) | Self::Ws90(channel) => channel.is_covered(),
            Self::Tpms(channel) => channel.is_covered(),
            Self::Rs41(channel) => channel.is_covered(),
        }
    }

    fn replay(
        self,
        input: &mut dyn Read,
        source_samples: u64,
        sample_limit: u64,
        max_frames: usize,
        stop: &AtomicBool,
    ) -> Result<Reading> {
        match self {
            Self::Efergy(channel) => channel
                .replay(input, source_samples, sample_limit, max_frames, stop)
                .map(Reading::Efergy),
            Self::Wh51(channel) => wh51::replay(
                channel,
                input,
                source_samples,
                sample_limit,
                max_frames,
                stop,
            )
            .map(Reading::Wh51),
            Self::Ws90(channel) => ws90::replay(
                channel,
                input,
                source_samples,
                sample_limit,
                max_frames,
                stop,
            )
            .map(Reading::Ws90),
            Self::Tpms(channel) => channel
                .replay(input, source_samples, sample_limit, max_frames, stop)
                .map(Reading::Tpms),
            Self::Rs41(channel) => channel
                .replay(input, source_samples, sample_limit, max_frames, stop)
                .map(Reading::Rs41),
        }
    }
}

struct Route<'a> {
    channel: Channel,
    pass_index: usize,
    pass_directory: Option<&'a str>,
    source_sweep_sha256: &'a str,
    tune: &'a Tune,
    selected_samples: u64,
}

impl<'a> Route<'a> {
    fn is_short(&self) -> bool {
        self.selected_samples < self.channel.sensor().minimum_dwell_samples()
    }

    fn request(&self) -> Request<'a> {
        Request {
            pass_index: self.pass_index,
            tune: self.tune,
            samples: if self.is_short() {
                0
            } else {
                self.selected_samples
            },
        }
    }
}

/// Ordered sensor dwell routes with an admitted sample budget.
pub(super) struct Plan<'a> {
    sensor: Sensor,
    report: Report<'a>,
    routes: VecDeque<Route<'a>>,
    active: Option<(Route<'a>, efergy::Stream)>,
}

impl<'a> Plan<'a> {
    /// Summarize prefix reads and coverage for the selected channel.
    pub(super) fn work(&self) -> Work {
        Work {
            replayed_iq_samples: self.report.replayed_iq_samples,
            eligible_attempts: self.report.replayed_dwells,
            short_attempts: self.report.short_dwells,
            budget_samples: self.report.replayed_iq_samples,
            max_budget_samples: self.report.max_total_samples,
            budget_unit: BudgetUnit::Iq,
        }
    }

    /// List the admitted source prefixes without reading IQ payloads.
    pub(super) fn requests(&self) -> impl Iterator<Item = Request<'a>> + '_ {
        self.routes.iter().map(Route::request)
    }

    /// Replay pending dwells with fresh decoder state and return the report.
    ///
    /// # Errors
    /// Rejects invalid or changed IQ and interruption.
    pub(super) fn replay(mut self, stop: &AtomicBool) -> Result<Report<'a>> {
        shared::replay_standalone(&mut self, stop)?;
        Ok(self.report)
    }

    fn remaining(&self) -> usize {
        self.report.max_frames - self.report.retained_frames
    }

    fn record(&mut self, route: Route<'a>, replay: Option<Reading>) {
        let report = &mut self.report;
        let outcome = if let Some(replay) = replay {
            let (validated, retained) = replay.frame_counts();
            report.validated_frames += validated;
            report.retained_frames += retained;
            report.omitted_valid_frames += validated - retained;
            Outcome::Replayed { replay }
        } else {
            Outcome::ShortDwell
        };
        let tune = route.tune;
        report.dwells.push(Attempt {
            pass_index: route.pass_index,
            pass_directory: route.pass_directory,
            source_sweep_sha256: route.source_sweep_sha256,
            tune_directory: &tune.directory,
            source_sidecar_sha256: &tune.sidecar_sha256,
            observed_center_hz: tune.observed_center_hz,
            saved_samples: tune.samples,
            selected_samples: route.selected_samples,
            outcome,
        });
    }
}

impl<'a> Task<'a> for Plan<'a> {
    fn next_request(&self) -> Option<Request<'a>> {
        self.requests().next()
    }

    fn advance(&mut self, input: &mut dyn Read, stop: &AtomicBool) -> Result<()> {
        check_stop(stop)?;
        let name = self.sensor.name();
        let route = self
            .routes
            .pop_front()
            .with_context(|| format!("missing planned {name} route"))?;
        let replay = if route.is_short() {
            None
        } else {
            let tune = route.tune;
            Some(
                route
                    .channel
                    .replay(
                        input,
                        tune.samples,
                        route.selected_samples,
                        self.remaining(),
                        stop,
                    )
                    .with_context(|| {
                        format!(
                            "replay {name} from pass {}/{}",
                            route.pass_index, tune.directory
                        )
                    })?,
            )
        };
        self.record(route, replay);
        Ok(())
    }
}

// Only Efergy is registered for streaming. The other sensors use Task.
impl<'a> StreamingTask<'a> for Plan<'a> {
    fn begin_dwell(&mut self, request: Request<'a>, stop: &AtomicBool) -> Result<(usize, usize)> {
        check_stop(stop)?;
        ensure!(
            self.active.is_none(),
            "Efergy replay has an unfinished dwell"
        );
        let Some(next) = self
            .next_request()
            .filter(|next| next.key() == request.key())
        else {
            return Ok((0, 0));
        };
        ensure!(
            std::ptr::eq(next.tune, request.tune)
                && (next.samples == 0 || next.samples == request.samples),
            "Efergy replay route differs from its planned source extent"
        );
        let route = self
            .routes
            .pop_front()
            .context("missing planned Efergy route")?;
        let Channel::Efergy(channel) = route.channel else {
            bail!("sensor replay route lacks a streaming decoder");
        };
        if route.is_short() {
            self.record(route, None);
            return Ok((0, 1));
        }
        let stream = channel
            .start(route.tune.samples, route.selected_samples, self.remaining())
            .with_context(|| {
                format!(
                    "replay Efergy from pass {}/{}",
                    route.pass_index, route.tune.directory
                )
            })?;
        self.active = Some((route, stream));
        Ok((1, 0))
    }

    fn push_sample(&mut self, sample: Iq16, index: u64) {
        if let Some((_, stream)) = &mut self.active {
            stream.push(sample, index);
        }
    }

    fn finish_dwell(&mut self, analyzed_iq_sha256: &str, stop: &AtomicBool) -> Result<()> {
        check_stop(stop)?;
        let (route, stream) = self.active.take().context("missing active Efergy dwell")?;
        let replay = stream.finish(analyzed_iq_sha256.to_owned());
        check_stop(stop)?;
        self.record(route, Some(Reading::Efergy(replay)));
        Ok(())
    }
}

/// Prepare sensor replay and validate its sample budget before reading IQ.
///
/// Uncovered dwells are skipped and short dwells retain their provenance.
///
/// # Errors
/// Rejects excessive replay work and interruption.
pub(super) fn prepare<'a>(
    capture: &'a CompletedCapture,
    sensor: Sensor,
    channel_hz: u64,
    max_samples_per_dwell: u64,
    max_total_samples: u64,
    max_frames: usize,
    stop: &AtomicBool,
) -> Result<Plan<'a>> {
    check_stop(stop)?;
    let passes = capture.passes();
    let mut report = Report {
        schema_version: 1,
        operation: sensor.operation(),
        capture_kind: capture.kind(),
        source_capture_sha256: capture.source_receipt_sha256(),
        channel_frequency_hz: channel_hz,
        sample_rate_hz: SAMPLE_RATE,
        minimum_dwell_samples: sensor.minimum_dwell_samples(),
        max_samples_per_dwell,
        max_total_samples,
        max_frames,
        pass_count: passes.len(),
        dwell_count: 0,
        covered_dwells: 0,
        short_dwells: 0,
        replayed_dwells: 0,
        replayed_iq_samples: 0,
        validated_frames: 0,
        retained_frames: 0,
        omitted_valid_frames: 0,
        dwells: Vec::new(),
    };
    let mut routes = VecDeque::new();
    for (index, pass) in passes.into_iter().enumerate() {
        check_stop(stop)?;
        report.dwell_count += pass.sweep.tunes.len();
        for tune in &pass.sweep.tunes {
            let channel = Channel::new(sensor, tune.observed_center_hz, channel_hz);
            if !channel.is_covered() {
                continue;
            }
            let route = Route {
                channel,
                pass_index: index + 1,
                pass_directory: pass.directory,
                source_sweep_sha256: &pass.sweep.source_receipt_sha256,
                tune,
                selected_samples: tune.samples.min(max_samples_per_dwell),
            };
            report.covered_dwells += 1;
            if route.is_short() {
                report.short_dwells += 1;
            } else {
                report.replayed_dwells += 1;
                report.replayed_iq_samples += route.selected_samples;
            }
            routes.push_back(route);
        }
    }
    let mut plan = Plan {
        sensor,
        report,
        routes,
        active: None,
    };
    let budget = plan.work();
    ensure!(
        budget.budget_samples <= budget.max_budget_samples,
        "{} replay needs {} samples, exceeding --max-total-samples ({})",
        sensor.name(),
        budget.budget_samples,
        budget.max_budget_samples
    );
    plan.report.dwells.reserve(plan.routes.len());
    Ok(plan)
}

/// Verify a completed capture and publish the selected sensor channel's report.
///
/// # Errors
/// Returns an error if admission, budget validation, replay, final source
/// verification or output publication fails, or the caller interrupts.
pub(crate) fn run(root: &Path, args: Args, sensor: Sensor, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let channel_hz = args
        .channel_hz
        .or_else(|| sensor.default_channel_hz())
        .context("--channel-hz is required for RS41 sweep replay")?;
    let capture = CompletedCapture::open(&root.join(&args.capture_directory), args.series)?;
    let report = prepare(
        &capture,
        sensor,
        channel_hz,
        args.max_samples_per_dwell,
        args.max_total_samples,
        args.max_frames,
        stop,
    )?
    .replay(stop)?;
    publish_json_file(&root.join(args.output), &report, || {
        check_stop(stop)?;
        capture.verify()
    })
}
