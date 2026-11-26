//! Exercise Mode S and UAT replay from independently synthesized aviation IQ.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use data_encoding::HEXLOWER;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CHANNEL_HZ: u64 = 1_090_000_000;
const CHANNEL_RATE_HZ: usize = 4_000_000;
const IQ_BYTES: usize = 4;
const CARRIER_OFFSET_HZ: f64 = 31_250.0;
const STARTS: [usize; 3] = [63, 811, 1_499];
const PREFIX_CHANNEL_SAMPLES: usize = 2_160;
const SWEEP_RATE_HZ: usize = 20_000_000;
const SWEEP_SAMPLES: usize = SWEEP_RATE_HZ / 1_000;
const SWEEP_STRIDE: usize = SWEEP_RATE_HZ / CHANNEL_RATE_HZ;
// Sun, The 1090 Megahertz Riddle, 2nd ed., section 5.3, first message.
const FRAME_HEX: &str = "8d40621d58c382d690c8ac2863a7";

fn write_squitter(
    iq: &mut [u8],
    frame: [u8; 14],
    start: usize,
    input_rate_hz: usize,
    center_hz: u64,
    phase_offset: f64,
) {
    // One tick is half a microsecond. Preamble pulses start at 0, 1,
    // 3.5 and 4.5 microseconds. The 112 PPM data bits follow at 8 microseconds.
    let preamble = [0, 2, 7, 9];
    let data = frame.iter().enumerate().flat_map(|(byte_index, byte)| {
        (0..8).map(move |bit| {
            let late = byte & (0x80 >> bit) == 0;
            16 + 2 * (8 * byte_index + bit) + usize::from(late)
        })
    });
    let samples_per_tick = input_rate_hz / 2_000_000;
    let offset_hz = CHANNEL_HZ as f64 - center_hz as f64 + CARRIER_OFFSET_HZ;
    for tick in preamble.into_iter().chain(data) {
        let first = start + tick * samples_per_tick;
        for sample in first..first + samples_per_tick {
            let phase = TAU * offset_hz * sample as f64 / input_rate_hz as f64 + phase_offset;
            let (q, i) = phase.sin_cos();
            let index = sample * IQ_BYTES;
            iq[index..index + 2].copy_from_slice(&((900.0 * i).round() as i16).to_le_bytes());
            iq[index + 2..index + IQ_BYTES]
                .copy_from_slice(&((900.0 * q).round() as i16).to_le_bytes());
        }
    }
}

fn replay(root: &Path, output: &str, input_rate_hz: usize, center_hz: u64, prefix: usize) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-adsb",
            "squitters.sc16q11",
            "--center-hz",
            &center_hz.to_string(),
            "--sample-rate-hz",
            &input_rate_hz.to_string(),
            "--max-samples",
            &prefix.to_string(),
            "--output",
            output,
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(output)).unwrap()).unwrap()
}

fn write_receipt(path: &Path, record: &Value) -> String {
    let bytes = serde_json::to_vec(record).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn archive_sweep(root: &Path, nominal_hz: u64, observed_hz: u64, iq: &[u8]) -> (String, String) {
    let tune = root.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    let bytes_per_ms = SWEEP_SAMPLES * IQ_BYTES;
    assert!(!iq.is_empty() && iq.len().is_multiple_of(bytes_per_ms));
    fs::write(tune.join("wideband.sc16q11"), iq).unwrap();
    let sidecar_sha256 = write_receipt(
        &tune.join("wideband.sc16q11.json"),
        &json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": observed_hz,
            "sample_rate_hz": SWEEP_RATE_HZ,
            "bandwidth_hz": SWEEP_RATE_HZ,
            "gain_db": 40,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "num_samples": iq.len() / IQ_BYTES,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep_sha256 = write_receipt(
        &root.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": nominal_hz,
            "upper_hz": nominal_hz + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": iq.len() / bytes_per_ms,
            "iq_bytes": iq.len(),
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": nominal_hz,
                "observed_center_hz": observed_hz,
                "observed_bands": [
                    {"lower_hz": observed_hz - 8_000_000, "upper_hz": observed_hz - 100_000},
                    {"lower_hz": observed_hz + 100_000, "upper_hz": observed_hz + 8_000_000}
                ]
            }],
        }),
    );
    (sweep_sha256, sidecar_sha256)
}

fn saved_sweep_command(
    root: &Path,
    action: &str,
    capture: &str,
    output: &str,
    extra: &[&str],
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([action, capture, "--output", output])
        .args(extra)
        .output()
        .unwrap()
}

fn sweep_replay(root: &Path, capture: &str, output: &str, extra: &[&str]) -> Output {
    saved_sweep_command(root, "identify-adsb-sweep", capture, output, extra)
}

fn read_report(root: &Path, path: &str, result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(path)).unwrap()).unwrap()
}

fn write_uat(iq: &mut [u8], center_hz: u64, start: usize, word: &[u8]) {
    const SYNC: u64 = 0xeacdda4e2;
    let bits: Vec<_> = (0..36)
        .rev()
        .map(|bit| SYNC & (1 << bit) != 0)
        .chain(
            word.iter()
                .flat_map(|byte| (0..8).rev().map(move |bit| byte & (1 << bit) != 0)),
        )
        .collect();
    let offset_hz = (978_000_000 - center_hz) as f64;
    let mut phase = 0.31_f64;
    for (index, sample) in iq
        .as_chunks_mut::<IQ_BYTES>()
        .0
        .iter_mut()
        .skip(start)
        .enumerate()
    {
        // At 20 MSps, five UAT symbols occupy 96 samples.
        let Some(&bit) = bits.get(index * 5 / 96) else {
            break;
        };
        let deviation_hz = if bit { 312_500.0 } else { -312_500.0 };
        phase += TAU * (offset_hz + deviation_hz) / SWEEP_RATE_HZ as f64;
        let (q, i) = phase.sin_cos();
        sample[..2].copy_from_slice(&((600.0 * i).round() as i16).to_le_bytes());
        sample[2..].copy_from_slice(&((600.0 * q).round() as i16).to_le_bytes());
    }
}

#[test]
fn uat_profile_reuses_the_fixed_channel_and_bounds_shared_sweep_replay() {
    // Appendix G of the ICAO UAT Implementation Manual publishes this codeword.
    const BASIC: &str = "00faa123555555c000000354064432c02800fe97c434e1ff5365cf8fafe4";
    const CENTER_HZ: u64 = 974_000_000;
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let mut iq = vec![0; SWEEP_SAMPLES * IQ_BYTES];
    let mut word = HEXLOWER.decode(BASIC.as_bytes()).unwrap();
    word[1] ^= 0x19;
    write_uat(&mut iq, CENTER_HZ, 1_000, &word);
    archive_sweep(&root.join("capture"), CENTER_HZ, CENTER_HZ, &iq);
    let dedicated = read_report(
        root,
        "uat.json",
        saved_sweep_command(root, "identify-uat-sweep", "capture", "uat.json", &[]),
    );
    assert_eq!(dedicated["operation"], "identify_uat_in_saved_sweeps");
    assert_eq!(dedicated["decoded_samples"], SWEEP_SAMPLES);
    assert_eq!(dedicated["validated_frames"], 1, "{dedicated}");
    let replay = &dedicated["channels"][0]["attempts"][0]["replay"];
    assert_eq!(
        replay["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    let frame = &replay["frames"][0];
    assert_eq!(frame["frame_hex"], BASIC);
    assert_eq!(frame["address_field"], "faa123");
    assert_eq!(frame["corrected_symbols"], 1);
    assert!(
        frame["first_input_sample"]
            .as_u64()
            .unwrap()
            .abs_diff(1_000)
            <= 20
    );

    write_receipt(
        &root.join("profile.json"),
        &json!({"schema_version": 1, "uat": true}),
    );
    let shared = read_report(
        root,
        "shared.json",
        saved_sweep_command(
            root,
            "replay-sweep",
            "capture",
            "shared.json",
            &["--signal-profile", "profile.json", "--share-iq"],
        ),
    );
    assert_eq!(shared["uat"], dedicated);
    let shortfall = saved_sweep_command(
        root,
        "replay-sweep",
        "capture",
        "rejected.json",
        &[
            "--uat",
            "--share-iq",
            "--max-total-samples",
            &(SWEEP_SAMPLES - 1).to_string(),
        ],
    );
    assert!(!shortfall.status.success());
    assert!(String::from_utf8_lossy(&shortfall.stderr).contains("--max-total-samples"));
    assert!(!root.join("rejected.json").exists());
}

#[test]
fn recovers_repeated_squitters_and_rejects_an_isolated_bad_crc_at_both_rates() {
    let frame: [u8; 14] = HEXLOWER
        .decode(FRAME_HEX.as_bytes())
        .unwrap()
        .try_into()
        .unwrap();
    let mut corrupted = frame;
    corrupted[9] ^= 1;

    for (input_rate_hz, center_hz) in [(4_000_000, 1_090_200_000), (20_000_000, 1_092_400_000)] {
        let directory = tempfile::tempdir().unwrap();
        let stride = input_rate_hz / CHANNEL_RATE_HZ;
        let prefix = PREFIX_CHANNEL_SAMPLES * stride + stride - 1;
        let source_samples = prefix + 7;
        let starts = STARTS.map(|start| (start + 1) * stride - 1);
        let mut iq = vec![0; source_samples * IQ_BYTES];
        for (index, (start, wire)) in starts
            .into_iter()
            .zip([corrupted, frame, frame])
            .enumerate()
        {
            write_squitter(
                &mut iq,
                wire,
                start,
                input_rate_hz,
                center_hz,
                0.7 + index as f64,
            );
        }
        fs::write(directory.path().join("squitters.sc16q11"), &iq).unwrap();

        let report = replay(
            directory.path(),
            "full.json",
            input_rate_hz,
            center_hz,
            prefix,
        );
        assert_eq!(report["operation"], "identify_adsb_in_saved_iq");
        assert_eq!(report["source_samples"], source_samples);
        assert_eq!(report["analyzed_samples"], prefix);
        assert_eq!(
            report["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..prefix * IQ_BYTES]))
        );
        assert_eq!(report["input_samples_per_channel_sample"], stride);
        assert_eq!(report["channelized_samples"], PREFIX_CHANNEL_SAMPLES);
        assert_eq!(report["validated_frames"], 2, "{report}");
        let frames = report["frames"].as_array().unwrap();
        assert_eq!(frames.len(), 2);
        let delay = report["channel_filter_delay_input_samples"]
            .as_u64()
            .unwrap() as usize;
        for (record, raw_start) in frames.iter().zip(&starts[1..]) {
            assert_eq!(record["frame_hex"], FRAME_HEX);
            let channel_start = record["preamble_channel_sample"].as_u64().unwrap() as usize;
            let mapped_start = (channel_start + 1) * stride - 1 - delay;
            assert!(mapped_start.abs_diff(*raw_start) <= stride, "{record}");
            let estimated_offset = record["estimated_carrier_offset_hz"].as_f64().unwrap();
            // Pulse edges bias this short-window estimate after channel filtering.
            assert!(
                (estimated_offset - CARRIER_OFFSET_HZ).abs() <= 20_000.0,
                "{record}"
            );
        }

        let corrupt_prefix = 670 * stride;
        let rejected = replay(
            directory.path(),
            "bad.json",
            input_rate_hz,
            center_hz,
            corrupt_prefix,
        );
        assert_eq!(rejected["analyzed_samples"], corrupt_prefix);
        assert_eq!(
            rejected["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..corrupt_prefix * IQ_BYTES]))
        );
        assert_eq!(rejected["crc_valid_candidates"], 0, "{rejected}");
        assert_eq!(rejected["validated_frames"], 0);
        assert!(rejected["frames"].as_array().unwrap().is_empty());
    }
}

#[test]
fn sweep_series_retains_repeated_frames_with_dwell_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let frame: [u8; 14] = HEXLOWER
        .decode(FRAME_HEX.as_bytes())
        .unwrap()
        .try_into()
        .unwrap();
    let mut corrupt = frame;
    corrupt[9] ^= 1;
    let observed_centers = [CHANNEL_HZ, CHANNEL_HZ + 7];
    let starts = STARTS.map(|start| (start + 1) * SWEEP_STRIDE - 1);
    let prefix = PREFIX_CHANNEL_SAMPLES * SWEEP_STRIDE + SWEEP_STRIDE - 1;
    let [mut first, mut second] = std::array::from_fn(|_| vec![0; SWEEP_SAMPLES * IQ_BYTES]);
    for (index, (start, wire)) in starts.into_iter().zip([corrupt, frame, frame]).enumerate() {
        write_squitter(
            &mut first,
            wire,
            start,
            SWEEP_RATE_HZ,
            observed_centers[0],
            0.7 + index as f64,
        );
    }
    write_squitter(
        &mut second,
        frame,
        starts[1],
        SWEEP_RATE_HZ,
        observed_centers[1],
        -0.9,
    );
    write_squitter(
        &mut second,
        frame,
        prefix + 200,
        SWEEP_RATE_HZ,
        observed_centers[1],
        -1.4,
    );
    let passes = ["pass-0001", "pass-0002"];
    let (first_sweep, first_sidecar) = archive_sweep(
        &root.join(passes[0]),
        CHANNEL_HZ,
        observed_centers[0],
        &first,
    );
    let (second_sweep, second_sidecar) = archive_sweep(
        &root.join(passes[1]),
        CHANNEL_HZ,
        observed_centers[1],
        &second,
    );
    let series_sha256 = write_receipt(
        &root.join("sweep-series.json"),
        &json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "dwells": 2,
            "iq_bytes": first.len() + second.len(),
            "passes": [
                {"directory": passes[0], "sweep_sha256": first_sweep},
                {"directory": passes[1], "sweep_sha256": second_sweep},
            ],
        }),
    );

    let report = read_report(
        root,
        "series.json",
        sweep_replay(
            root,
            ".",
            "series.json",
            &["--series", "--max-samples-per-dwell", &prefix.to_string()],
        ),
    );
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "identify_adsb_in_saved_sweeps");
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["source_capture_sha256"], series_sha256);
    assert_eq!(report["channel_frequency_hz"], CHANNEL_HZ);
    assert_eq!(report["sample_rate_hz"], SWEEP_RATE_HZ);
    assert_eq!(report["minimum_frame_samples"], 2_440);
    for field in [
        "pass_count",
        "dwell_count",
        "covered_dwells",
        "replayed_dwells",
        "dc_guard_overlap_dwells",
    ] {
        assert_eq!(report[field], 2);
    }
    assert_eq!(report["short_dwells"], 0);
    assert_eq!(report["covered_samples"], 2 * SWEEP_SAMPLES);
    assert_eq!(report["replayed_iq_samples"], 2 * prefix);
    assert_eq!(report["covered_time_ns"], 2_000_000);
    assert_eq!(report["replayed_time_ns"], 2 * prefix * 50);
    assert_eq!(report["validated_frames"], 3);
    assert_eq!(report["omitted_valid_frames"], 0);

    let dwells = report["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 2);
    let sources = [
        (&first, first_sweep, first_sidecar, &starts[1..]),
        (&second, second_sweep, second_sidecar, &starts[1..2]),
    ];
    for (index, (dwell, (iq, sweep, sidecar, wanted))) in dwells.iter().zip(sources).enumerate() {
        assert_eq!(dwell["pass_index"], index + 1);
        assert_eq!(dwell["pass_directory"], passes[index]);
        assert_eq!(dwell["tune_directory"], "tune-0001");
        assert_eq!(dwell["source_sweep_sha256"], sweep);
        assert_eq!(dwell["source_sidecar_sha256"], sidecar);
        assert_eq!(dwell["observed_center_hz"], observed_centers[index]);
        assert_eq!(dwell["saved_samples"], SWEEP_SAMPLES);
        assert_eq!(dwell["selected_samples"], prefix);
        assert_eq!(dwell["overlaps_spectrum_dc_guard"], true);
        assert_eq!(dwell["status"], "replayed");
        let replay = &dwell["replay"];
        assert_eq!(replay["center_frequency_hz"], observed_centers[index]);
        assert_eq!(replay["analyzed_samples"], prefix);
        assert_eq!(replay["channelized_samples"], PREFIX_CHANNEL_SAMPLES);
        assert_eq!(
            replay["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..prefix * IQ_BYTES]))
        );
        let frames = replay["frames"].as_array().unwrap();
        assert_eq!(frames.len(), wanted.len());
        let delay = replay["channel_filter_delay_input_samples"]
            .as_u64()
            .unwrap() as usize;
        for (record, &expected) in frames.iter().zip(wanted) {
            assert_eq!(record["frame_hex"], FRAME_HEX);
            let channel_start = record["preamble_channel_sample"].as_u64().unwrap() as usize;
            let raw_start = (channel_start + 1) * SWEEP_STRIDE - 1 - delay;
            assert!(raw_start.abs_diff(expected) <= SWEEP_STRIDE, "{record}");
        }
    }

    let report_path = root.join("series.json");
    let original = fs::read(&report_path).unwrap();
    let collision = sweep_replay(
        root,
        ".",
        "series.json",
        &[
            "--series",
            "--max-samples-per-dwell",
            &(prefix + 1).to_string(),
        ],
    );
    let stderr = String::from_utf8_lossy(&collision.stderr);
    assert!(!collision.status.success());
    assert!(stderr.contains("without replacing output"), "{stderr}");
    assert_eq!(fs::read(report_path).unwrap(), original);

    // An invalid first sample would fail decoding if replay preceded budget admission.
    first[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    fs::write(root.join("pass-0001/tune-0001/wideband.sc16q11"), first).unwrap();
    let output = sweep_replay(
        root,
        ".",
        "budget.json",
        &[
            "--series",
            "--max-samples-per-dwell",
            &prefix.to_string(),
            "--max-total-samples",
            &(2 * prefix - 1).to_string(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--max-total-samples"), "{stderr}");
    assert!(!root.join("budget.json").exists());
}

#[test]
fn sweep_replay_distinguishes_short_and_uncovered_at_the_readback_edge() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut iq = vec![0; SWEEP_SAMPLES * IQ_BYTES];
    // Both files must be admitted by metadata without parsing their invalid IQ.
    iq[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    let edge = CHANNEL_HZ + 6_700_000;
    let (inside_receipt, inside_sidecar) = archive_sweep(&root.join("inside"), edge, edge - 7, &iq);
    let (outside_receipt, _) = archive_sweep(&root.join("outside"), edge, edge + 7, &iq);

    let short = read_report(
        root,
        "short.json",
        sweep_replay(
            root,
            "inside",
            "short.json",
            &["--max-samples-per-dwell", "2439"],
        ),
    );
    assert_eq!(short["capture_kind"], "sweep");
    assert_eq!(short["source_capture_sha256"], inside_receipt);
    assert_eq!(short["dwell_count"], 1);
    assert_eq!(short["covered_dwells"], 1);
    assert_eq!(short["short_dwells"], 1);
    assert_eq!(short["replayed_dwells"], 0);
    assert_eq!(short["replayed_iq_samples"], 0);
    let dwells = short["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 1);
    let dwell = &dwells[0];
    assert_eq!(dwell["pass_index"], 1);
    assert!(dwell.get("pass_directory").is_none());
    assert_eq!(dwell["source_sidecar_sha256"], inside_sidecar);
    assert_eq!(dwell["observed_center_hz"], edge - 7);
    assert_eq!(dwell["selected_samples"], 2_439);
    assert_eq!(dwell["overlaps_spectrum_dc_guard"], false);
    assert_eq!(dwell["status"], "short_dwell");
    assert!(dwell.get("replay").is_none());

    let outside = read_report(
        root,
        "outside.json",
        sweep_replay(root, "outside", "outside.json", &[]),
    );
    assert_eq!(outside["source_capture_sha256"], outside_receipt);
    assert_eq!(outside["dwell_count"], 1);
    for field in [
        "covered_dwells",
        "short_dwells",
        "replayed_dwells",
        "replayed_iq_samples",
    ] {
        assert_eq!(outside[field], 0);
    }
    assert_eq!(outside["dwells"], json!([]));

    let invalid = sweep_replay(
        root,
        "inside",
        "invalid.json",
        &["--max-samples-per-dwell", "2440"],
    );
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(!invalid.status.success());
    assert!(stderr.contains("outside SC16 Q11"), "{stderr}");
    assert!(!root.join("invalid.json").exists());
}

#[test]
fn combined_replay_matches_standalone_sections_and_preserves_existing_report() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let center_hz = 1_092_400_000;
    let frame = HEXLOWER
        .decode(FRAME_HEX.as_bytes())
        .unwrap()
        .try_into()
        .unwrap();
    let mut iq = vec![0; SWEEP_SAMPLES * IQ_BYTES];
    let start = (STARTS[1] + 1) * SWEEP_STRIDE - 1;
    write_squitter(&mut iq, frame, start, SWEEP_RATE_HZ, center_hz + 7, 0.7);
    let (first, _) = archive_sweep(&root.join("pass-0001"), center_hz, center_hz + 7, &iq);
    iq.fill(0);
    let (second, _) = archive_sweep(&root.join("pass-0002"), center_hz, center_hz - 7, &iq);
    let digest = write_receipt(
        &root.join("sweep-series.json"),
        &json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "dwells": 2,
            "iq_bytes": 2 * iq.len(),
            "passes": [
                {"directory": "pass-0001", "sweep_sha256": first},
                {"directory": "pass-0002", "sweep_sha256": second},
            ],
        }),
    );
    let output = "combined.json";
    let result = saved_sweep_command(
        root,
        "replay-sweep",
        ".",
        output,
        &[
            "--series",
            "--adsb",
            "--aprs-channel-hz",
            "1090000000",
            "--aprs-channel-hz",
            "144390000",
            "--lora-profile",
            "1090000000:125000:7",
            "--lte-range-hz",
            "1089900000:1090000000",
        ],
    );
    let report = read_report(root, output, result);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "replay_saved_sweep");
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["source_capture_sha256"], digest);
    assert_eq!(report["pass_count"], 2);
    assert_eq!(report["dwell_count"], 2);

    let lanes: [(&str, &str, &[&str]); 4] = [
        (
            "aprs",
            "identify-aprs-sweep",
            &["--channel-hz", "1090000000", "--channel-hz", "144390000"],
        ),
        ("adsb", "identify-adsb-sweep", &[]),
        (
            "lora",
            "probe-lora-sweep",
            &["--profile", "1090000000:125000:7"],
        ),
        (
            "lte",
            "identify-lte-sweep",
            &["--lower-hz", "1089900000", "--upper-hz", "1090000000"],
        ),
    ];
    for (section, command, flags) in lanes {
        let name = format!("{section}.json");
        let mut options = vec!["--series"];
        options.extend_from_slice(flags);
        let result = saved_sweep_command(root, command, ".", &name, &options);
        let standalone = read_report(root, &name, result);
        assert_eq!(
            report[section], standalone,
            "standalone {section} report differs"
        );
    }

    assert_eq!(report["adsb"]["validated_frames"], 1);
    assert_eq!(
        report["adsb"]["dwells"][0]["replay"]["frames"][0]["frame_hex"],
        FRAME_HEX
    );
    assert_eq!(report["aprs"]["channels"][0]["covered_dwells"], 0);
    assert_eq!(report["aprs"]["channels"][1]["short_dwells"], 2);
    assert_eq!(report["lora"]["profiles"][0]["short_dwells"], 2);
    assert_eq!(
        report["lte"]["short_only_ranges"],
        json!([{
            "lower_hz": 1_089_900_000, "upper_hz": 1_090_000_000
        }])
    );

    let flags = ["--series", "--adsb"];
    let adsb_only = read_report(
        root,
        "adsb-only.json",
        saved_sweep_command(root, "replay-sweep", ".", "adsb-only.json", &flags),
    );
    assert_eq!(adsb_only["adsb"], report["adsb"]);
    for omitted in ["aprs", "lora", "lte"] {
        assert!(adsb_only.get(omitted).is_none());
    }
    let bytes = fs::read(root.join(output)).unwrap();
    let collision = saved_sweep_command(root, "replay-sweep", ".", output, &flags);
    let stderr = String::from_utf8_lossy(&collision.stderr);
    assert!(!collision.status.success());
    assert!(stderr.contains("without replacing output"), "{stderr}");
    assert_eq!(fs::read(root.join(output)).unwrap(), bytes);
}

#[test]
fn combined_replay_leaves_no_report_if_no_lane_is_selected_or_a_lane_fails() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut iq = vec![0; SWEEP_SAMPLES * IQ_BYTES];
    iq[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    archive_sweep(root, CHANNEL_HZ, CHANNEL_HZ, &iq);

    // APRS is covered but short, so only the ADS-B lane reads the malformed IQ.
    for (flags, error) in [
        (&[][..], "at least one"),
        (
            &["--aprs-channel-hz", "1090000000", "--adsb"][..],
            "outside SC16 Q11",
        ),
    ] {
        let result = saved_sweep_command(root, "replay-sweep", ".", "invalid.json", flags);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(stderr.contains(error), "{stderr}");
        assert!(!root.join("invalid.json").exists());
    }
}

#[test]
fn combined_replay_plans_all_lanes_before_reading_iq() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut iq = vec![0; 50 * SWEEP_SAMPLES * IQ_BYTES];
    iq[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    let (digest, _) = archive_sweep(root, CHANNEL_HZ, CHANNEL_HZ, &iq);
    let selection = [
        "--aprs-channel-hz",
        "1090000000",
        "--adsb",
        "--lora-profile",
        "1090000000:125000:7",
        "--lora-profile",
        "1090000000:125000:8",
        "--lte-range-hz",
        "1089900000:1090000000",
        "--rds-channel-hz",
        "93300000",
        "--morse-profile",
        "cw:1090000000",
    ];
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(["replay-sweep", ".", "--plan-only", "--share-iq"])
        .args(selection)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let preview: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["operation"], "plan_saved_sweep_replay");
    assert_eq!(preview["capture_kind"], "sweep");
    assert_eq!(preview["source_capture_sha256"], digest);
    assert_eq!(preview["pass_count"], 1);
    assert_eq!(preview["dwell_count"], 1);
    assert_eq!(preview["total_replayed_iq_samples"], 3_000_000);
    assert_eq!(preview["total_replayed_iq_bytes"], 12_000_000);
    assert_eq!(
        preview["shared_iq"],
        json!({
            "source_iq_bytes": 4_000_000,
            "max_buffered_iq_bytes": 4_000_000,
            "shared_dwells": 1,
        })
    );
    let expected = [
        ("aprs", "iq_samples", 0, 0, 1, 0, 200_000_000),
        (
            "adsb",
            "iq_samples",
            1_000_000,
            1,
            0,
            1_000_000,
            200_000_000,
        ),
        (
            "lora",
            "profile_samples",
            1_000_000,
            2,
            0,
            2_000_000,
            200_000_000,
        ),
        (
            "lte",
            "channelized_samples",
            1_000_000,
            2,
            0,
            192_000,
            256_000_000,
        ),
        ("rds", "station_samples", 0, 0, 0, 0, 200_000_000),
        ("morse", "profile_samples", 0, 0, 1, 0, 200_000_000),
    ];
    let observed = preview["analyzers"].as_array().unwrap();
    assert_eq!(observed.len(), expected.len());
    for (row, (analyzer, unit, iq, eligible, short, budget, limit)) in observed.iter().zip(expected)
    {
        assert_eq!(
            *row,
            json!({
                "analyzer": analyzer,
                "budget_unit": unit,
                "replayed_iq_samples": iq,
                "eligible_attempts": eligible,
                "short_attempts": short,
                "budget_samples": budget,
                "max_budget_samples": limit,
            })
        );
    }
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);

    // A later budget failure must take precedence over the malformed first sample.
    let mut rejected = selection.to_vec();
    rejected.extend(["--max-total-channelized-samples", "1"]);
    for (flags, cause) in [
        (&rejected[..], "exceeding --max-total-channelized-samples"),
        (&selection[..], "outside SC16 Q11"),
    ] {
        let mut shared_flags = flags.to_vec();
        shared_flags.push("--share-iq");
        let result =
            saved_sweep_command(root, "replay-sweep", ".", "unpublished.json", &shared_flags);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(stderr.contains(cause), "{stderr}");
        assert!(!root.join("unpublished.json").exists());
    }

    iq[..2].copy_from_slice(&0_i16.to_le_bytes());
    fs::write(root.join("tune-0001/wideband.sc16q11"), iq).unwrap();
    let ordinary = read_report(
        root,
        "ordinary.json",
        saved_sweep_command(root, "replay-sweep", ".", "ordinary.json", &selection),
    );
    let mut shared_flags = selection.to_vec();
    shared_flags.push("--share-iq");
    let mut shared = read_report(
        root,
        "shared.json",
        saved_sweep_command(root, "replay-sweep", ".", "shared.json", &shared_flags),
    );
    assert_eq!(
        shared.as_object_mut().unwrap().remove("shared_iq").unwrap(),
        preview["shared_iq"]
    );
    assert_eq!(shared, ordinary);
}
