//! Bounded CPU-only verification of the channel bank's VHDL implementation.
//!
//! GHDL analyzes and elaborates the authored files, simulates deterministic
//! vectors with transport stalls, and emits a generic synthesis netlist. Every
//! numeric output and clipping flag is compared with the fixed-point Rust model.
//! These checks do not fit Cyclone V hardware, establish timing closure, produce
//! an RBF, load an FPGA or open a radio. Quartus/Nios remain separate prerequisites.
//!
//! Subprocesses run in owned process groups. Cancellation/deadlines terminate
//! and reap the group; fallback Drop cleanup can block in OS process teardown.
//! Failure retains diagnostics and vectors. Only complete success publishes
//! result.json; a partial synthesis file is not evidence of successful synthesis.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::json;

use crate::dsp::{CIC_DECIMATION, FixedBank, Iq16, MAX_CHANNELS, SIN_Q15};

#[derive(Debug, Serialize)]
pub(crate) struct VerificationSummary {
    pub(crate) verifier: &'static str,
    pub(crate) ghdl_version: String,
    pub(crate) cases: Vec<CaseSummary>,
    pub(crate) stages: Vec<StageSummary>,
    pub(crate) total_input_samples: usize,
    pub(crate) total_output_frames: usize,
    pub(crate) transport_records_verified: usize,
    pub(crate) pipeline_control_contract_passed: bool,
    pub(crate) reset_and_loss_contract_passed: bool,
    pub(crate) native_fifo_contract_passed: bool,
    pub(crate) trigger_diagnostic_contract_passed: bool,
    pub(crate) numeric_model: &'static str,
    pub(crate) generic_synthesis_passed: bool,
    pub(crate) quartus_fitted: bool,
    pub(crate) timing_closed: bool,
    pub(crate) bitstream_generated: bool,
    pub(crate) hardware_loaded: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct StageSummary {
    name: String,
    elapsed_wall_seconds: f64,
    stdout_file: String,
    stderr_file: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct CaseSummary {
    name: &'static str,
    phase_words: [u32; MAX_CHANNELS],
    input_samples: usize,
    output_frames: usize,
    clipped_lane_outputs: usize,
}

#[derive(Clone, Copy, Debug)]
enum Pattern {
    NativeQ11,
    FullScale,
    ArbitraryPhase,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    name: &'static str,
    words: [u32; MAX_CHANNELS],
    samples: usize,
    pattern: Pattern,
}

const CASES: [Case; 3] = [
    Case {
        name: "q11-patterns",
        words: [0, 0x2000_0000, 0xf000_0000, 0x6abc_def0],
        samples: 8_193,
        pattern: Pattern::NativeQ11,
    },
    Case {
        name: "fullscale-wrap-and-clip",
        words: [0, 0x2000_0000, 0xf000_0000, 0x6abc_def0],
        samples: 12_289,
        pattern: Pattern::FullScale,
    },
    Case {
        name: "arbitrary-phase",
        words: [0x0123_4567, 0x1020_3040, 0xf123_4567, 0x7234_5678],
        samples: 10_003,
        pattern: Pattern::ArbitraryPhase,
    },
];

fn new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {} without replacement", path.display()))
}

fn json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = new_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

struct OwnedProcess {
    child: Child,
    reaped: bool,
}

impl OwnedProcess {
    fn signal(&self, signal: &str) -> Result<()> {
        let status = Command::new("kill")
            .args([signal, "--", &format!("-{}", self.child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        ensure!(
            status.success(),
            "could not signal verification process group"
        );
        Ok(())
    }

    fn terminate(&mut self) -> Result<()> {
        if self.reaped {
            return Ok(());
        }
        // Signal the group even if its leader has exited, to reap compiler work
        // still running below GHDL. The process group was created by this owner.
        let _ = self.signal("-TERM");
        // Keep the leader unreaped until both group signals have been sent.
        // Otherwise a quickly exiting leader can leave compiler descendants,
        // and reaping early permits its PID/process-group ID to be reused.
        thread::sleep(Duration::from_millis(500));
        let _ = self.signal("-KILL");
        let _ = self.child.kill();
        self.child.wait()?;
        self.reaped = true;
        Ok(())
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.terminate();
        }
    }
}

fn run_ghdl(
    name: &str,
    args: &[OsString],
    work: &Path,
    output: &Path,
    deadline: Instant,
    stop: &AtomicBool,
    netlist: bool,
) -> Result<StageSummary> {
    ensure!(
        !stop.load(Ordering::Relaxed) && Instant::now() < deadline,
        "verification cancelled or exceeded its time budget before {name}"
    );
    let stdout_name = if netlist {
        format!("{name}.netlist.vhd")
    } else {
        format!("{name}.log")
    };
    let stderr_name = if netlist {
        format!("{name}.stderr.log")
    } else {
        stdout_name.clone()
    };
    let stdout = new_file(&output.join(&stdout_name))?;
    let stderr = if netlist {
        new_file(&output.join(&stderr_name))?
    } else {
        stdout.try_clone()?
    };
    let child = Command::new("ghdl")
        .args(args)
        .current_dir(work)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("start GHDL {name}; install the documented GHDL simulator"))?;
    let mut process = OwnedProcess {
        child,
        reaped: false,
    };
    let start = Instant::now();
    loop {
        if let Some(status) = process.child.try_wait()? {
            process.reaped = true;
            ensure!(
                status.success(),
                "GHDL {name} failed ({status}); see {}",
                output.join(&stderr_name).display()
            );
            return Ok(StageSummary {
                name: name.into(),
                elapsed_wall_seconds: start.elapsed().as_secs_f64(),
                stdout_file: stdout_name,
                stderr_file: stderr_name,
            });
        }
        if stop.load(Ordering::Relaxed) || Instant::now() >= deadline {
            process
                .terminate()
                .context("terminate verification process group")?;
            bail!(
                "GHDL {name} cancelled or exceeded verification time budget; outputs retained in {}",
                output.display()
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn next_random(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn stimulus(case: Case) -> impl Iterator<Item = Iq16> {
    let mut state = 0x1234_5678_u32;
    (0..case.samples).map(move |index| match case.pattern {
        Pattern::NativeQ11 => match index {
            0..64 => Iq16::default(),
            64..128 => Iq16 {
                i: if index & 1 == 0 { 2047 } else { -2048 },
                q: 0,
            },
            128..256 => Iq16 {
                i: 0,
                q: if index & 1 == 0 { 2047 } else { -2048 },
            },
            _ => Iq16 {
                i: (next_random(&mut state) & 4095) as i16 - 2048,
                q: (next_random(&mut state) & 4095) as i16 - 2048,
            },
        },
        Pattern::FullScale => {
            if index < 8192 {
                Iq16 {
                    i: i16::MAX,
                    q: i16::MIN,
                }
            } else {
                let phase = (index * 32) & 255;
                Iq16 {
                    i: if SIN_Q15[(phase + 64) & 255] >= 0 {
                        i16::MAX
                    } else {
                        i16::MIN
                    },
                    q: if SIN_Q15[phase] >= 0 {
                        i16::MAX
                    } else {
                        i16::MIN
                    },
                }
            }
        }
        Pattern::ArbitraryPhase => Iq16 {
            i: next_random(&mut state) as i16,
            q: next_random(&mut state) as i16,
        },
    })
}

fn write_vectors(case: Case, output: &Path) -> Result<CaseSummary> {
    let mut input = BufWriter::new(new_file(&output.join(format!("{}.input.txt", case.name)))?);
    writeln!(
        input,
        "{:08x} {:08x} {:08x} {:08x}",
        case.words[0], case.words[1], case.words[2], case.words[3]
    )?;
    let mut expected = BufWriter::new(new_file(
        &output.join(format!("{}.expected.txt", case.name)),
    )?);
    let mut model = FixedBank::new(&case.words)?;
    let mut frames = 0;
    let mut clipped = 0;
    for (index, sample) in stimulus(case).enumerate() {
        writeln!(input, "{} {}", sample.i, sample.q)?;
        if let Some(frame) = model.push(sample) {
            write!(expected, "{index}")?;
            for (sample, clip) in frame.samples.iter().zip(frame.clipped) {
                write!(expected, " {} {} {}", sample.i, sample.q, u8::from(clip))?;
                clipped += usize::from(clip);
            }
            writeln!(expected)?;
            frames += 1;
        }
    }
    input.flush()?;
    input.get_ref().sync_all()?;
    expected.flush()?;
    expected.get_ref().sync_all()?;
    ensure!(
        frames == case.samples / CIC_DECIMATION,
        "reference frame count differs from decimation contract"
    );
    if matches!(case.pattern, Pattern::FullScale) {
        ensure!(clipped > 0, "full-scale vectors did not exercise clipping");
    }
    Ok(CaseSummary {
        name: case.name,
        phase_words: case.words,
        input_samples: case.samples,
        output_frames: frames,
        clipped_lane_outputs: clipped,
    })
}

fn columns(mut reader: impl BufRead) -> Result<Option<[i64; 13]>> {
    let mut line = String::new();
    let bytes = (&mut reader).take(513).read_line(&mut line)?;
    if bytes == 0 {
        return Ok(None);
    }
    ensure!(bytes <= 512, "verification output line exceeds 512 bytes");
    let fields = line
        .split_whitespace()
        .map(str::parse::<i64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let fields: [i64; 13] = fields
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected exactly 13 numeric output columns"))?;
    Ok(Some(fields))
}

fn compare_vectors(expected: &Path, actual: &Path, frames: usize) -> Result<()> {
    let mut expected = BufReader::new(File::open(expected)?);
    let mut actual = BufReader::new(File::open(actual).context("open GHDL numeric output")?);
    for row in 0..frames {
        let reference = columns(&mut expected)?.context("reference output ended early")?;
        let observed = columns(&mut actual)?.context("GHDL output ended early")?;
        ensure!(
            observed == reference,
            "RTL/model mismatch at output frame {row}: expected {reference:?}, observed {observed:?}"
        );
    }
    ensure!(
        columns(&mut expected)?.is_none() && columns(&mut actual)?.is_none(),
        "unexpected trailing RTL or reference output"
    );
    Ok(())
}

fn compare_transport(case: Case, actual: &Path, binary: &Path) -> Result<usize> {
    let mut reader = BufReader::new(File::open(actual)?);
    let mut encoded = BufWriter::new(new_file(binary)?);
    let mut model = FixedBank::new(&case.words)?;
    let mut records = 0;
    for (index, sample) in stimulus(case).enumerate() {
        let Some(expected) = model.push(sample) else {
            continue;
        };
        ensure!(
            !expected.clipped.iter().any(|clipped| *clipped),
            "transport qualification expects native Q11 inputs without saturation"
        );
        let mut raw = [0u8; crate::transport::RECORD_BYTES];
        for slot in 0..8 {
            let mut line = String::new();
            let bytes = (&mut reader).take(65).read_line(&mut line)?;
            ensure!(
                bytes > 0 && bytes <= 64,
                "framer output missing or exceeds its line limit"
            );
            let fields = line
                .split_whitespace()
                .map(|word| u16::from_str_radix(word, 16))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(
                fields.len() == 2,
                "framer transport slot must contain two hex words"
            );
            raw[slot * 4..slot * 4 + 2].copy_from_slice(&fields[0].to_le_bytes());
            raw[slot * 4 + 2..slot * 4 + 4].copy_from_slice(&fields[1].to_le_bytes());
        }
        let frame = crate::transport::parse(&raw).context("decode actual VHDL transport record")?;
        ensure!(
            frame.source_index == index as u64 && !frame.rx2 && frame.samples == expected.samples,
            "framer transport/reference mismatch at source index {index}"
        );
        encoded.write_all(&raw)?;
        records += 1;
    }
    let mut tail = String::new();
    ensure!(
        (&mut reader).take(65).read_line(&mut tail)? == 0,
        "unexpected trailing framer transport slots"
    );
    encoded.flush()?;
    encoded.get_ref().sync_all()?;
    Ok(records)
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn verify_inner(
    workspace: &Path,
    output: &Path,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<VerificationSummary> {
    ensure!(
        !timeout.is_zero() && timeout <= Duration::from_secs(3600),
        "verification timeout must be positive and at most one hour"
    );
    let deadline = Instant::now() + timeout;
    let sources = workspace
        .join("projects/rf-observatory/hdl")
        .canonicalize()
        .context("locate channel-bank HDL sources")?;
    let output = output.canonicalize()?;
    for entry in fs::read_dir(&output)? {
        ensure!(
            entry?.file_name() == "request.json",
            "verification requires a fresh output directory"
        );
    }
    ensure!(
        sources.as_os_str().as_encoded_bytes().len() <= 4096
            && output.as_os_str().as_encoded_bytes().len() <= 4096,
        "verification paths exceed the 4096-byte tool boundary"
    );
    let work = output.join("ghdl-work");
    DirBuilder::new()
        .mode(0o700)
        .create(&work)
        .context("create fresh GHDL work directory")?;
    json_file(
        &output.join("verification-plan.json"),
        &json!({
            "reference":"crate::dsp::FixedBank","generator":"deterministic-xorshift32-patterns-v1",
            "seed":0x1234_5678_u32,"timeout_seconds":timeout.as_secs_f64(),
        "source_files":["hdl/rf_observatory_p.vhd","hdl/rf_channel_bank.vhd","hdl/rf_channel_bank_tb.vhd",
            "hdl/rf_channel_bank_pipeline_tb.vhd",
            "hdl/rf_channel_framer.vhd","hdl/rf_channel_framer_tb.vhd","hdl/rf_channel_framer_contract_tb.vhd",
            "hdl/rf_channel_fifo_tb.vhd","hdl/rf_channel_trigger_cdc_tb.vhd"],
            "simulation_transport":"testbench inserts input gaps and output backpressure",
            "hardware_access":false,"scope":"generic synthesis and bit-exact simulation only"
        }),
    )?;
    let mut stages = vec![run_ghdl(
        "version",
        &args(&["--version"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?];
    let mut version = String::new();
    File::open(output.join("version.log"))?
        .take(4096)
        .read_to_string(&mut version)?;
    let version = version.lines().next().unwrap_or("unknown").to_owned();
    let mut analyze = args(&["-a", "--std=08"]);
    for name in [
        "rf_observatory_p.vhd",
        "rf_channel_bank.vhd",
        "rf_channel_bank_tb.vhd",
        "rf_channel_bank_pipeline_tb.vhd",
        "rf_channel_framer.vhd",
        "rf_channel_framer_tb.vhd",
        "rf_channel_framer_contract_tb.vhd",
    ] {
        analyze.push(sources.join(name).into_os_string());
    }
    stages.push(run_ghdl(
        "analyze", &analyze, &work, &output, deadline, stop, false,
    )?);
    stages.push(run_ghdl(
        "elaborate",
        &args(&["-e", "--std=08", "rf_channel_bank_tb"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    let mut summaries = Vec::new();
    for case in CASES {
        ensure!(
            !stop.load(Ordering::Relaxed) && Instant::now() < deadline,
            "verification cancelled or timed out while generating vectors"
        );
        let summary = write_vectors(case, &output)?;
        let actual = output.join(format!("{}.actual.txt", case.name));
        // The bench opens write_mode; pre-create our empty destination so its
        // permissions remain private and pre-existing destinations are refused.
        drop(new_file(&actual)?);
        let mut command = args(&["-r", "--std=08", "rf_channel_bank_tb"]);
        command.push(format!("-gG_INPUT_FILE=../{}.input.txt", case.name).into());
        command.push(format!("-gG_OUTPUT_FILE=../{}.actual.txt", case.name).into());
        command.push("--assert-level=error".into());
        stages.push(run_ghdl(
            case.name, &command, &work, &output, deadline, stop, false,
        )?);
        compare_vectors(
            &output.join(format!("{}.expected.txt", case.name)),
            &actual,
            summary.output_frames,
        )
        .with_context(|| format!("compare {}", case.name))?;
        summaries.push(summary);
    }
    stages.push(run_ghdl(
        "elaborate-pipeline-contract",
        &args(&["-e", "--std=08", "rf_channel_bank_pipeline_tb"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "simulate-pipeline-contract",
        &args(&[
            "-r",
            "--std=08",
            "rf_channel_bank_pipeline_tb",
            "--assert-level=error",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    let transport_case = Case {
        name: "framed-q11",
        words: crate::transport::PROFILE_WORDS,
        samples: 6003,
        pattern: Pattern::NativeQ11,
    };
    let transport_summary = write_vectors(transport_case, &output)?;
    let actual = output.join("framed-q11.actual.txt");
    drop(new_file(&actual)?);
    stages.push(run_ghdl(
        "elaborate-framer",
        &args(&["-e", "--std=08", "rf_channel_framer_tb"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "simulate-framer",
        &args(&[
            "-r",
            "--std=08",
            "rf_channel_framer_tb",
            "-gG_INPUT_FILE=../framed-q11.input.txt",
            "-gG_OUTPUT_FILE=../framed-q11.actual.txt",
            "--assert-level=error",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    let transport_records =
        compare_transport(transport_case, &actual, &output.join("framed-q11.sc16"))?;
    summaries.push(transport_summary);
    stages.push(run_ghdl(
        "elaborate-contract",
        &args(&["-e", "--std=08", "rf_channel_framer_contract_tb"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "simulate-contract",
        &args(&[
            "-r",
            "--std=08",
            "rf_channel_framer_contract_tb",
            "--assert-level=error",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    let mut vendor_analysis = args(&["-a", "--std=08", "-frelaxed-rules"]);
    for source in [
        "hdl/fpga/ip/nuand/synthesis/synchronizer.vhd",
        "hdl/fpga/ip/nuand/synthesis/fifo_readwrite_p.vhd",
        "hdl/fpga/platforms/common/bladerf/vhdl/fx3_gpif_p.vhd",
        "hdl/fpga/ip/nuand/synthesis/fifo_writer.vhd",
    ] {
        vendor_analysis.push(
            workspace
                .join("repos/bladeRF")
                .join(source)
                .into_os_string(),
        );
    }
    vendor_analysis.push(sources.join("rf_channel_fifo_tb.vhd").into_os_string());
    stages.push(run_ghdl(
        "analyze-native-fifo",
        &vendor_analysis,
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "elaborate-native-fifo",
        &args(&["-e", "--std=08", "-frelaxed-rules", "rf_channel_fifo_tb"]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "simulate-native-fifo",
        &args(&[
            "-r",
            "--std=08",
            "-frelaxed-rules",
            "rf_channel_fifo_tb",
            "--assert-level=error",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "analyze-trigger-diagnostic",
        &[
            "-a".into(),
            "--std=08".into(),
            sources
                .join("rf_channel_trigger_cdc_tb.vhd")
                .into_os_string(),
        ],
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "elaborate-trigger-diagnostic",
        &args(&[
            "-e",
            "--std=08",
            "-frelaxed-rules",
            "rf_channel_trigger_cdc_tb",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "simulate-trigger-diagnostic",
        &args(&[
            "-r",
            "--std=08",
            "-frelaxed-rules",
            "rf_channel_trigger_cdc_tb",
            "--assert-level=error",
        ]),
        &work,
        &output,
        deadline,
        stop,
        false,
    )?);
    stages.push(run_ghdl(
        "synthesize",
        &args(&["--synth", "--std=08", "rf_channel_bank"]),
        &work,
        &output,
        deadline,
        stop,
        true,
    )?);
    stages.push(run_ghdl(
        "synthesize-framer",
        &args(&["--synth", "--std=08", "rf_channel_framer"]),
        &work,
        &output,
        deadline,
        stop,
        true,
    )?);
    ensure!(
        fs::metadata(output.join("synthesize.netlist.vhd"))?.len() > 0
            && fs::metadata(output.join("synthesize-framer.netlist.vhd"))?.len() > 0,
        "GHDL emitted an empty generic synthesis netlist"
    );
    ensure!(
        !stop.load(Ordering::Relaxed) && Instant::now() < deadline,
        "verification cancelled or exceeded its time budget"
    );
    Ok(VerificationSummary {
        verifier: "ghdl-channel-bank-v1",
        ghdl_version: version,
        total_input_samples: summaries.iter().map(|s| s.input_samples).sum(),
        total_output_frames: summaries.iter().map(|s| s.output_frames).sum(),
        transport_records_verified: transport_records,
        pipeline_control_contract_passed: true,
        reset_and_loss_contract_passed: true,
        native_fifo_contract_passed: true,
        trigger_diagnostic_contract_passed: true,
        cases: summaries,
        stages,
        numeric_model: "four Q1.15 NCO mixers, signed 48-bit wrapping CIC3/R20, arithmetic shift13, saturated i16 with clipping flags",
        generic_synthesis_passed: true,
        quartus_fitted: false,
        timing_closed: false,
        bitstream_generated: false,
        hardware_loaded: false,
    })
}

/// Verify authored VHDL without opening hardware or invoking Quartus.
///
/// `output` is a fresh private run directory; a caller-owned request.json may
/// already exist. Owned files and the GHDL work directory are never replaced.
/// One deadline covers compilation, all simulation cases and generic synthesis.
/// Cancellation is polled during child execution and between bounded file work.
///
/// # Errors
///
/// Returns contextual errors for unavailable tools/sources, existing outputs,
/// invalid time/path bounds, subprocess failure, cancellation, timeout, I/O or
/// any RTL/reference mismatch. Failure retains evidence and publishes no success
/// result; best-effort failure.json records the diagnostic when possible.
pub(crate) fn verify_hdl_with_cancel(
    workspace: &Path,
    output: &Path,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<VerificationSummary> {
    if !output.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(output)?;
    }
    let result = verify_inner(workspace, output, timeout, stop);
    match &result {
        Ok(summary) => json_file(&output.join("result.json"), summary)?,
        Err(error) => {
            let _ = json_file(
                &output.join("failure.json"),
                &json!({"error":format!("{error:#}"),"hardware_access":false}),
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_rejects_wrong_clipping_missing_rows_and_trailing_data() {
        let directory = tempfile::tempdir().unwrap();
        let expected = directory.path().join("expected");
        let actual = directory.path().join("actual");
        let line = "19 0 0 0 1 -1 1 2 -2 0 3 -3 0\n";
        fs::write(&expected, line).unwrap();
        fs::write(&actual, line).unwrap();
        compare_vectors(&expected, &actual, 1).unwrap();
        for wrong in [
            "19 0 0 0 1 -1 0 2 -2 0 3 -3 0\n",
            "",
            "19 0 0 0 1 -1 1 2 -2 0 3 -3 0\n39 0 0 0 1 -1 1 2 -2 0 3 -3 0\n",
        ] {
            fs::write(&actual, wrong).unwrap();
            assert!(compare_vectors(&expected, &actual, 1).is_err());
        }
        fs::write(&actual, "1".repeat(513)).unwrap();
        assert!(compare_vectors(&expected, &actual, 1).is_err());
    }

    #[test]
    fn vector_matrix_has_partial_tails_and_exercises_saturation() {
        let directory = tempfile::tempdir().unwrap();
        let summaries: Vec<_> = CASES
            .iter()
            .map(|case| write_vectors(*case, directory.path()).unwrap())
            .collect();
        assert!(
            summaries
                .iter()
                .all(|s| s.input_samples % CIC_DECIMATION != 0)
        );
        assert!(summaries[1].clipped_lane_outputs > 0);
        assert!(write_vectors(CASES[0], directory.path()).is_err());
    }

    #[test]
    fn explicit_process_shutdown_reaps_owned_child() {
        let child = Command::new("sleep")
            .arg("5")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut process = OwnedProcess {
            child,
            reaped: false,
        };
        process.terminate().unwrap();
        assert!(process.reaped);
        assert!(process.child.try_wait().unwrap().is_some());
    }

    #[test]
    fn shutdown_signals_all_members_of_the_owned_process_group() {
        let child = Command::new("sleep")
            .arg("5")
            .process_group(0)
            .spawn()
            .unwrap();
        let mut member = Command::new("sleep")
            .arg("5")
            .process_group(child.id() as i32)
            .spawn()
            .unwrap();
        let mut process = OwnedProcess {
            child,
            reaped: false,
        };
        process.terminate().unwrap();
        assert!(member.try_wait().unwrap().is_some());
        member.wait().unwrap();
    }
}
