//! One stock RX1 stream with bounded, independently published IQ segments.

use std::fs::{self, DirBuilder, File};
use std::hint::black_box;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use num_complex::Complex;
use soapysdr::Direction::Rx;

use super::segment_output::{Segment, publish_segment};
use super::segment_stream::{SegmentEvent, SegmentPlan, stream_segments};
use super::{
    CHUNK_SAMPLES, END_WITNESS_SAMPLES, FpgaVersion, MAX_DISCARD_INITIAL_SAMPLES, READ_TIMEOUT_US,
    RadioArgs, STOCK_FPGA_VERSION, SampleRateHz, TIMESTAMPED_SAMPLE_RATE_HZ,
    TIMESTAMPED_USB_BUFFER_SAMPLES, capture_metadata, interrupt_flag, receive_stream_args,
    timed_read, with_active_stream,
};

const SEGMENT_SAMPLES: usize = 19_988_480;
const MAX_UNSPOOLED_SEGMENTS: u16 = 12;
const MAX_SPOOLED_SEGMENTS: u16 = 120;
const MAX_RETIRED_RUN_SEGMENTS: u16 = 3_600;
const MAX_SPOOL_SLOTS: u16 = 120;
const MAX_WRITER_QUEUE_DEPTH: u8 = 8;
const MAX_USB_BUFFER_SAMPLES: u32 = 65_536;
// SC16 Q11 uses four bytes per complex sample. Align to a full 8 KiB
// metadata buffer so libbladeRF can use the requested size without rounding.
const USB_BUFFER_SAMPLE_ALIGNMENT: u32 = 2048;

type SegmentSamples = Vec<Complex<i16>>;

#[derive(Args, Debug)]
pub(crate) struct SegmentCaptureArgs {
    /// Center frequency in hertz.
    #[arg(long, default_value_t = 2_437_000_000.0)]
    frequency: f64,
    /// Manual RX1 gain in dB.
    #[arg(long, default_value_t = 40.0, allow_negative_numbers = true)]
    gain: f64,
    /// Analog RX1 bandwidth in hertz. The sample rate stays at 20 MSps.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=20_000_000))]
    bandwidth_hz: Option<u32>,
    /// Optional bladeRF hexadecimal serial for device selection.
    #[arg(long)]
    serial: Option<String>,
    /// Maximum number of 19,988,480-sample segments to save.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_RETIRED_RUN_SEGMENTS)))]
    segments: u16,
    /// Capture through the next witnessed boundary after a sibling finish file appears.
    #[arg(long, requires = "retire_prefix", conflicts_with = "segments")]
    until_finished: bool,
    /// Maximum live IQ payloads when a consumer removes surveyed IQ files.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=i64::from(MAX_SPOOL_SLOTS)))]
    spool_segments: Option<u16>,
    /// Number of witnessed segments that can wait in memory for the publisher.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=i64::from(MAX_WRITER_QUEUE_DEPTH)))]
    writer_queue_depth: u8,
    /// Complex samples per USB transfer, in multiples of 2,048.
    #[arg(long, default_value_t = TIMESTAMPED_USB_BUFFER_SAMPLES, value_parser = usb_buffer_samples)]
    usb_buffer_samples: u32,
    /// Allow the consumer to retire released segments using a durable checkpoint.
    #[arg(long, requires = "spool_segments")]
    retire_prefix: bool,
    /// Initial complex samples to discard once before segment one.
    #[arg(long, default_value_t = 2_000_000, value_parser = clap::value_parser!(u32).range(0..=i64::from(MAX_DISCARD_INITIAL_SAMPLES)))]
    discard_initial_samples: u32,
    /// New private directory for the numbered segments.
    #[arg(long)]
    output: PathBuf,
}

fn usb_buffer_samples(value: &str) -> Result<u32, String> {
    let samples: u32 = value
        .parse()
        .map_err(|_| format!("invalid USB buffer size {value}"))?;
    if (USB_BUFFER_SAMPLE_ALIGNMENT..=MAX_USB_BUFFER_SAMPLES).contains(&samples)
        && samples.is_multiple_of(USB_BUFFER_SAMPLE_ALIGNMENT)
    {
        Ok(samples)
    } else {
        Err(format!(
            "USB buffer size must be a multiple of {USB_BUFFER_SAMPLE_ALIGNMENT} from {USB_BUFFER_SAMPLE_ALIGNMENT} through {MAX_USB_BUFFER_SAMPLES} samples"
        ))
    }
}

fn finish_requested(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && metadata.len() == 0,
                "finish marker {} must be a direct empty regular file",
                path.display()
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect finish marker {}", path.display()))
        }
    }
}

fn enqueue(sender: &SyncSender<Segment>, segment: Segment) -> Result<()> {
    match sender.try_send(segment) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => bail!("segment writer backlog exhausted"),
        Err(TrySendError::Disconnected(_)) => bail!("segment writer stopped"),
    }
}

fn prepare_segment_buffers(
    segments: Option<usize>,
    writer_queue_depth: usize,
    samples: usize,
) -> Result<(SyncSender<SegmentSamples>, Receiver<SegmentSamples>)> {
    // Capture and publication each own one buffer in addition to the ready queue.
    let pipeline_buffers = writer_queue_depth + 2;
    let count = segments.map_or(pipeline_buffers, |count| count.min(pipeline_buffers));
    let (recycle, available) = sync_channel(count);
    for _ in 0..count {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(samples)
            .context("allocate reusable RX segment buffer")?;
        buffer.resize(samples, Complex::new(0, 0));
        // Materialize the pages while the stream is still inactive.
        black_box(buffer.as_slice());
        buffer.clear();
        recycle
            .try_send(buffer)
            .context("prime reusable RX segment buffers")?;
    }
    Ok((recycle, available))
}

pub(crate) fn capture_segments(args: SegmentCaptureArgs) -> Result<()> {
    ensure!(
        args.spool_segments.is_some() || args.segments <= MAX_UNSPOOLED_SEGMENTS,
        "more than {MAX_UNSPOOLED_SEGMENTS} segments requires --spool-segments"
    );
    ensure!(
        args.segments <= MAX_SPOOLED_SEGMENTS || args.retire_prefix,
        "more than {MAX_SPOOLED_SEGMENTS} segments requires --retire-prefix"
    );
    let segments = (!args.until_finished).then_some(usize::from(args.segments));
    let spool_segments = args.spool_segments.map(usize::from);
    let writer_queue_depth = usize::from(args.writer_queue_depth);
    let retire_prefix = args.retire_prefix;
    let discard = usize::try_from(args.discard_initial_samples)?;
    let duration = segments
        .map(|count| -> Result<_> {
            let capture_samples = count
                .checked_mul(SEGMENT_SAMPLES)
                .and_then(|samples| samples.checked_add(discard))
                .context("segmented capture sample count overflow")?;
            Ok(Duration::from_secs_f64(
                capture_samples as f64 / TIMESTAMPED_SAMPLE_RATE_HZ + 15.0,
            ))
        })
        .transpose()?;
    let radio = RadioArgs {
        frequency: args.frequency,
        sample_rate: TIMESTAMPED_SAMPLE_RATE_HZ,
        gain: args.gain,
        channel: 1,
        serial: args.serial,
    };
    radio.validate()?;
    let interrupted = interrupt_flag()?;
    let parent = args
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).context("create capture output parent")?;
    DirBuilder::new()
        .mode(0o700)
        .create(&args.output)
        .with_context(|| format!("create new output directory {}", args.output.display()))?;
    File::open(parent)?
        .sync_all()
        .context("sync capture output parent")?;
    let finish_marker = parent.join("finish");

    let (recycle, available) =
        prepare_segment_buffers(segments, writer_queue_depth, SEGMENT_SAMPLES)?;
    let required: FpgaVersion = STOCK_FPGA_VERSION.parse().map_err(anyhow::Error::msg)?;
    let device = radio.open(Some(&required))?;
    if let Some(bandwidth) = args.bandwidth_hz {
        device
            .set_bandwidth(Rx, radio.index(), f64::from(bandwidth))
            .context("set RX1 bandwidth for timestamped capture")?;
    }
    let rate = SampleRateHz::try_from(device.sample_rate(Rx, radio.index())?)?;
    let mut settings = capture_metadata(
        &device,
        &radio,
        rate,
        SEGMENT_SAMPLES,
        args.discard_initial_samples,
        Some(&required),
    )?;
    ensure!(
        settings.sample_rate_hz == TIMESTAMPED_SAMPLE_RATE_HZ,
        "timestamp-checked segments require 20 MSps"
    );
    ensure!(
        if args.bandwidth_hz.is_some() {
            settings.bandwidth_hz > 0.0 && settings.bandwidth_hz <= TIMESTAMPED_SAMPLE_RATE_HZ
        } else {
            settings.bandwidth_hz == TIMESTAMPED_SAMPLE_RATE_HZ
        },
        "unexpected RX1 bandwidth readback: {} Hz",
        settings.bandwidth_hz
    );
    settings.timestamp_note =
        "Host wall clock before stream setup. Device clock bounds are in segment.json.".into();
    settings.continuity_note =
        "Consecutive device timestamps and a successful end witness were checked.".into();

    let mut read_buffer = vec![Complex::new(0i16, 0); CHUNK_SAMPLES];
    let mut segment_samples = Vec::new();
    let mut stream_args = receive_stream_args();
    // Keep more samples queued on USB during a continuous segmented run.
    stream_args.set("buflen", args.usb_buffer_samples.to_string());
    // Successful timestamp flags are interpreted only in explicit metadata mode.
    stream_args.set("meta", "meta");

    let saved_segments = thread::scope(|scope| {
        let (sender, receiver) = sync_channel(writer_queue_depth);
        let output = &args.output;
        let mut writer = None;

        let acquisition = (|| {
            let mut stream = device.rx_stream_args(&[radio.index()], stream_args)?;
            let mtu_samples = stream.mtu().context("read timestamped RX1 stream MTU")?;
            ensure!(
                mtu_samples == args.usb_buffer_samples as usize,
                "RX1 stream MTU is {mtu_samples} samples, requested {}",
                args.usb_buffer_samples
            );
            eprintln!("Timestamped RX1 stream MTU: {mtu_samples} samples");
            let deadline = duration.map(|duration| Instant::now() + duration);
            with_active_stream(&mut stream, |stream| {
                // Read gain after RX is active, as for ordinary capture.
                settings.gain_db = device.gain(Rx, radio.index())?;
                if args.bandwidth_hz.is_some() {
                    ensure!(
                        device.bandwidth(Rx, radio.index())? == settings.bandwidth_hz,
                        "RX1 bandwidth changed after stream activation"
                    );
                }
                writer = Some(
                    thread::Builder::new()
                        .name("rx1-segment-writer".into())
                        .spawn_scoped(scope, move || -> Result<()> {
                            for mut segment in receiver {
                                publish_segment(
                                    output,
                                    &settings,
                                    &segment,
                                    spool_segments,
                                    retire_prefix,
                                )?;
                                segment.samples.clear();
                                recycle
                                    .try_send(segment.samples)
                                    .context("return published RX segment buffer")?;
                            }
                            Ok(())
                        })
                        .context("start segment writer")?,
                );
                stream_segments(
                    &mut read_buffer,
                    SegmentPlan {
                        segment_samples: SEGMENT_SAMPLES,
                        segment_count: segments,
                        discard_samples: discard,
                        witness_samples: END_WITNESS_SAMPLES,
                    },
                    |buffer| {
                        ensure!(
                            !interrupted.load(Ordering::Relaxed),
                            "segmented RX interrupted"
                        );
                        ensure!(
                            deadline.is_none_or(|deadline| Instant::now() < deadline),
                            "segmented RX exceeded its deadline"
                        );
                        let metadata = stream
                            .read_with_metadata(&mut [buffer], READ_TIMEOUT_US)
                            .context("read timestamped RX1 IQ")?;
                        ensure!(
                            !interrupted.load(Ordering::Relaxed),
                            "segmented RX interrupted"
                        );
                        ensure!(
                            deadline.is_none_or(|deadline| Instant::now() < deadline),
                            "segmented RX exceeded its deadline"
                        );
                        timed_read(metadata)
                    },
                    |event| match event {
                        SegmentEvent::Samples(samples) => {
                            if segment_samples.capacity() == 0 {
                                segment_samples = available
                                    .try_recv()
                                    .context("take reusable RX segment buffer")?;
                            }
                            segment_samples.extend_from_slice(samples);
                            Ok(())
                        }
                        SegmentEvent::Closed {
                            index,
                            timing,
                            finished_on_request,
                            ..
                        } => enqueue(
                            &sender,
                            Segment {
                                index,
                                timing,
                                finished_on_request,
                                samples: std::mem::take(&mut segment_samples),
                            },
                        ),
                    },
                    || finish_requested(&finish_marker),
                )
            })
        })();

        // Close the queue after stopping RX. An error still lets already
        // witnessed segments finish publication.
        drop(sender);
        let publication = match writer {
            Some(writer) => writer
                .join()
                .map_err(|_| anyhow::anyhow!("segment writer panicked"))
                .and_then(|result| result),
            None => Ok(()),
        };
        match (acquisition, publication) {
            (Err(primary), Err(writer)) => {
                Err(primary.context(format!("segment writer also failed: {writer:#}")))
            }
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Ok(segments), Ok(())) => Ok(segments),
        }
    })?;
    writeln!(
        std::io::stdout().lock(),
        "Saved {saved_segments} timestamp-checked RX1 segments to {}",
        args.output.display()
    )
    .context("write segmented capture summary")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radio::segment_stream::SegmentTiming;
    use crate::{Cli, Command};
    use clap::{Parser, error::ErrorKind as CliErrorKind};

    #[test]
    fn analog_bandwidth_is_opt_in_and_bounded_by_sample_rate() {
        let base = ["bladerf-lab", "capture-segments", "--output", "capture"];
        let Command::CaptureSegments(default) = Cli::try_parse_from(base).unwrap().command else {
            panic!("capture-segments command");
        };
        assert_eq!(default.bandwidth_hz, None);
        let cli =
            Cli::try_parse_from(base.into_iter().chain(["--bandwidth-hz", "6000000"])).unwrap();
        let Command::CaptureSegments(narrow) = cli.command else {
            panic!("capture-segments command");
        };
        assert_eq!(narrow.bandwidth_hz, Some(6_000_000));
        assert_eq!(
            Cli::try_parse_from(base.into_iter().chain(["--bandwidth-hz", "20000001"]))
                .unwrap_err()
                .kind(),
            CliErrorKind::ValueValidation
        );
    }

    #[test]
    fn usb_buffer_size_uses_an_exact_aligned_sample_count() {
        let base = ["bladerf-lab", "capture-segments", "--output", "capture"];
        for (arguments, expected) in [
            (&[][..], 32_768),
            (&["--usb-buffer-samples", "65536"][..], 65_536),
            (&["--usb-buffer-samples", "2048"][..], 2048),
        ] {
            let cli =
                Cli::try_parse_from(base.into_iter().chain(arguments.iter().copied())).unwrap();
            let Command::CaptureSegments(args) = cli.command else {
                panic!("capture-segments command")
            };
            assert_eq!(args.usb_buffer_samples, expected);
        }
        for invalid in ["0", "1024", "3072", "32769", "67584"] {
            let error =
                Cli::try_parse_from(base.into_iter().chain(["--usb-buffer-samples", invalid]))
                    .err()
                    .unwrap();
            assert_eq!(error.kind(), CliErrorKind::ValueValidation);
        }
    }

    #[test]
    fn finish_mode_requires_retiring_spool_and_excludes_explicit_count() {
        let base = [
            "bladerf-lab",
            "capture-segments",
            "--output",
            "capture",
            "--until-finished",
        ];
        let retiring_spool = ["--retire-prefix", "--spool-segments", "4"];
        let cli = Cli::try_parse_from(base.into_iter().chain(retiring_spool)).unwrap();
        let Command::CaptureSegments(args) = cli.command else {
            panic!("capture-segments command")
        };
        assert!(args.until_finished);
        for extra in [
            &[][..],
            &["--retire-prefix"][..],
            &["--spool-segments", "4"][..],
        ] {
            let error = Cli::try_parse_from(base.into_iter().chain(extra.iter().copied())).err();
            assert_eq!(
                error.map(|error| error.kind()),
                Some(CliErrorKind::MissingRequiredArgument)
            );
        }
        let error = Cli::try_parse_from(
            base.into_iter()
                .chain(retiring_spool)
                .chain(["--segments", "4"]),
        )
        .err();
        assert_eq!(
            error.map(|error| error.kind()),
            Some(CliErrorKind::ArgumentConflict)
        );
    }

    #[test]
    fn finish_marker_requires_a_direct_empty_regular_file() {
        let parent = tempfile::tempdir().unwrap();
        let marker = parent.path().join("finish");
        assert!(!finish_requested(&marker).unwrap());
        fs::create_dir(&marker).unwrap();
        assert!(finish_requested(&marker).is_err());
        fs::remove_dir(&marker).unwrap();
        let target = parent.path().join("target");
        fs::write(&target, []).unwrap();
        std::os::unix::fs::symlink(&target, &marker).unwrap();
        assert!(finish_requested(&marker).is_err());
        fs::remove_file(&marker).unwrap();
        fs::write(&marker, b"unexpected data").unwrap();
        assert!(finish_requested(&marker).is_err());
        fs::write(&marker, []).unwrap();
        assert!(finish_requested(&marker).unwrap());
    }

    #[test]
    fn queue_pressure_stops_admission_and_preserves_earlier_segment() {
        let segment = |index| Segment {
            index,
            timing: SegmentTiming {
                first_sample_ns: 0,
                end_sample_exclusive_ns: 50,
            },
            finished_on_request: false,
            samples: vec![Complex::new(0, 0)],
        };
        for depth in [1, 4] {
            let (sender, receiver) = sync_channel(depth);
            for index in 0..depth {
                enqueue(&sender, segment(index)).unwrap();
            }
            assert!(
                enqueue(&sender, segment(depth))
                    .unwrap_err()
                    .to_string()
                    .contains("backlog exhausted")
            );
            for index in 0..depth {
                assert_eq!(receiver.try_recv().unwrap().index, index);
            }
            assert!(receiver.try_recv().is_err());
        }
    }

    #[test]
    fn prepares_queue_and_in_flight_buffers_up_to_the_finite_segment_count() {
        for (segments, depth, expected) in [
            (Some(1), 8, 1),
            (Some(2), 4, 2),
            (Some(12), 4, 6),
            (None, 8, 10),
        ] {
            let (_recycle, available) = prepare_segment_buffers(segments, depth, 8).unwrap();
            assert_eq!(available.try_iter().count(), expected);
        }
    }

    #[test]
    fn cycles_prepared_buffers_without_reallocating_or_carrying_samples() {
        let (recycle, available) = prepare_segment_buffers(Some(5), 1, 8).unwrap();
        let mut buffers = Vec::new();
        for _ in 0..3 {
            buffers.push(available.try_recv().unwrap());
        }
        assert!(available.try_recv().is_err());
        let pointers: Vec<_> = buffers.iter().map(|buffer| buffer.as_ptr()).collect();
        let capacities: Vec<_> = buffers.iter().map(Vec::capacity).collect();
        for buffer in buffers {
            recycle.try_send(buffer).unwrap();
        }
        for index in 0..9 {
            let mut buffer = available.try_recv().unwrap();
            assert!(buffer.is_empty());
            buffer.extend((0..8).map(|_| Complex::new(index, -index)));
            let slot = index as usize % pointers.len();
            assert_eq!(buffer.as_ptr(), pointers[slot]);
            assert_eq!(buffer.capacity(), capacities[slot]);
            assert!(
                buffer
                    .iter()
                    .all(|sample| *sample == Complex::new(index, -index))
            );
            buffer.clear();
            recycle.try_send(buffer).unwrap();
        }
    }
}
