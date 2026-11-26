//! Exercise saved IQ replay through narrow FM, AFSK and frame validation.

use std::f64::consts::TAU;
use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};

use data_encoding::HEXLOWER;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TARGETED_SAMPLE_RATE: usize = 2_000_000;
const SWEEP_SAMPLE_RATE: usize = 20_000_000;
const CHANNEL_HZ: u64 = 144_390_000;
// Complete AX.25 bytes including the little-endian FCS.
const FRAME_HEX: &str = "82a0a4a64040609c60868298986303f03e746573742036ccf8";

fn wire_bits(byte: u8) -> impl Iterator<Item = bool> {
    (0..8).map(move |bit| byte & (1 << bit) != 0)
}

fn synthesize_packets(sample_rate: usize, offset_hz: f64, repetitions: usize) -> Vec<u8> {
    let frame = HEXLOWER.decode(FRAME_HEX.as_bytes()).unwrap();
    let preamble: Vec<_> = std::iter::repeat_n(0x7e, 14).flat_map(wire_bits).collect();
    let mut packet = preamble;
    let mut ones = 0;
    for bit in frame.into_iter().flat_map(wire_bits) {
        packet.push(bit);
        ones = if bit { ones + 1 } else { 0 };
        if ones == 5 {
            packet.push(false);
            ones = 0;
        }
    }
    packet.extend(std::iter::repeat_n(0x7e, 4).flat_map(wire_bits));
    let mut mark = false;
    let tones: Vec<_> = packet
        .iter()
        .cycle()
        .take(repetitions * packet.len())
        .map(|&bit| {
            mark ^= !bit;
            if mark { 1_200.0 } else { 2_200.0 }
        })
        .collect();

    let step = TAU / sample_rate as f64;
    let mut audio_phase: f64 = 1.2;
    let mut rf_phase: f64 = 0.0;
    let sample_count = tones.len() * sample_rate / 1_200;
    let mut bytes = Vec::with_capacity(4 * (sample_count + 317));
    bytes.resize(4 * 317, 0);
    for sample in 0..sample_count {
        let tone = tones[sample * 1_200 / sample_rate];
        audio_phase += step * tone;
        rf_phase += step * (offset_hz + 450.0 + 2_800.0 * audio_phase.sin());
        let (q, i) = rf_phase.sin_cos();
        bytes.extend_from_slice(&((900.0 * i).round() as i16).to_le_bytes());
        bytes.extend_from_slice(&((900.0 * q).round() as i16).to_le_bytes());
    }
    bytes
}

fn replay(root: &Path, output: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-aprs",
            "packet.sc16q11",
            "--center-hz",
            "144570000",
            "--channel-hz",
            "144390000",
            "--sample-rate-hz",
            "2000000",
            "--output",
            output,
        ])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn replay_verifies_exact_frames_and_keeps_separate_occurrences() {
    let root = tempfile::tempdir().unwrap();
    let iq = synthesize_packets(TARGETED_SAMPLE_RATE, -180_000.0, 2);
    fs::write(root.path().join("packet.sc16q11"), &iq).unwrap();
    let output = replay(root.path(), "aprs.json", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = root.path().join("aprs.json");
    let bytes = fs::read(&path).unwrap();
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(report["operation"], "identify_aprs_in_saved_iq");
    assert_eq!(report["analyzed_samples"], iq.len() / 4);
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    assert_eq!(report["validated_frames"], 2);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    assert!(
        frames[0]["decision_input_sample"].as_u64().unwrap()
            < frames[1]["decision_input_sample"].as_u64().unwrap()
    );
    for frame in frames {
        assert_eq!(frame["frame_hex"], FRAME_HEX);
        assert_eq!(frame["information_text"], ">test 6");
    }

    let output = replay(root.path(), "prefix.json", &["--max-samples", "317"]);
    assert!(output.status.success());
    let prefix: Value =
        serde_json::from_slice(&fs::read(root.path().join("prefix.json")).unwrap()).unwrap();
    assert_eq!(prefix["source_samples"], iq.len() / 4);
    assert_eq!(prefix["analyzed_samples"], 317);
    assert_eq!(prefix["validated_frames"], 0);
    assert_eq!(
        prefix["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..317 * 4]))
    );

    let collision = replay(root.path(), "aprs.json", &["--max-samples", "1"]);
    assert!(!collision.status.success());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn replay_rejects_invalid_complex_samples_without_publishing() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("packet.sc16q11"), [0, 0, 0, 8]).unwrap();
    let output = replay(root.path(), "aprs.json", &[]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SC16 Q11"));
    assert!(!root.path().join("aprs.json").exists());
}

fn write_receipt(path: &Path, receipt: &Value) -> String {
    let bytes = serde_json::to_vec(receipt).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn write_sweep(
    directory: &Path,
    dwell_ms: usize,
    observed_hz: u64,
    iq: Option<&[u8]>,
) -> (String, String) {
    let tune = directory.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    let samples = dwell_ms * (SWEEP_SAMPLE_RATE / 1_000);
    let iq_bytes = 4 * samples;
    let iq_path = tune.join("wideband.sc16q11");
    if let Some(iq) = iq {
        assert_eq!(iq.len(), iq_bytes);
        fs::write(iq_path, iq).unwrap();
    } else {
        File::create(iq_path)
            .unwrap()
            .set_len(iq_bytes as u64)
            .unwrap();
    }
    let sidecar_sha256 = write_receipt(
        &tune.join("wideband.sc16q11.json"),
        &json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": observed_hz,
            "sample_rate_hz": SWEEP_SAMPLE_RATE,
            "bandwidth_hz": SWEEP_SAMPLE_RATE,
            "gain_db": 40,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "num_samples": samples,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep_sha256 = write_receipt(
        &directory.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CHANNEL_HZ,
            "upper_hz": CHANNEL_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": dwell_ms,
            "iq_bytes": iq_bytes,
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": CHANNEL_HZ,
                "observed_center_hz": observed_hz,
                "observed_bands": [
                    {"lower_hz": observed_hz - 8_000_000, "upper_hz": observed_hz - 100_000},
                    {"lower_hz": observed_hz + 100_000, "upper_hz": observed_hz + 8_000_000},
                ],
            }],
        }),
    );
    (sweep_sha256, sidecar_sha256)
}

fn replay_sweep(root: &Path, input: &str, output: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-aprs-sweep",
            input,
            "--channel-hz",
            &CHANNEL_HZ.to_string(),
            "--channel-hz",
            "408390000",
            "--output",
            output,
        ])
        .args(extra)
        .output()
        .unwrap()
}

fn read_report(root: &Path, path: &str, output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(path)).unwrap()).unwrap()
}

#[test]
fn sweep_replay_binds_exact_targeted_capture_and_series_passes() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let mut iq = synthesize_packets(SWEEP_SAMPLE_RATE, 0.0, 1);
    let dwell_ms = iq.len().div_ceil(4 * (SWEEP_SAMPLE_RATE / 1_000));
    let samples = dwell_ms * (SWEEP_SAMPLE_RATE / 1_000);
    iq.resize(samples * 4, 0);
    let iq_sha256 = format!("{:x}", Sha256::digest(&iq));
    let (first_sweep, first_sidecar) = write_sweep(
        &root.join("series/pass-0001"),
        dwell_ms,
        CHANNEL_HZ,
        Some(&iq),
    );
    let (second_sweep, second_sidecar) = write_sweep(
        &root.join("series/pass-0002"),
        dwell_ms,
        CHANNEL_HZ + 7,
        None,
    );
    let series_path = root.join("series/sweep-series.json");
    let mut series = json!({
        "schema_version": 1,
        "operation": "rf_sweep_series",
        "dwells": 2,
        "iq_bytes": iq.len() * 2,
        "passes": [
            {"directory": "pass-0001", "sweep_sha256": first_sweep},
            {"directory": "pass-0002", "sweep_sha256": second_sweep},
        ],
    });
    let series_sha256 = write_receipt(&series_path, &series);

    let one = read_report(
        root,
        "single.json",
        replay_sweep(root, "series/pass-0001", "single.json", &[]),
    );
    assert_eq!(one["schema_version"], 1);
    assert_eq!(one["operation"], "identify_aprs_in_saved_sweeps");
    assert_eq!(one["capture_kind"], "sweep");
    assert_eq!(one["source_capture_sha256"], first_sweep);
    assert_eq!(one["minimum_frame_samples"], 2_800_000);
    assert_eq!(one["decoded_samples"], samples);
    assert_eq!(one["validated_frames"], 1);
    assert_eq!(one["channels"][0]["channel_frequency_hz"], CHANNEL_HZ);
    assert_eq!(one["channels"][0]["covered_dwells"], 1);
    assert_eq!(one["channels"][0]["short_dwells"], 0);
    assert_eq!(one["channels"][1]["channel_frequency_hz"], 408_390_000);
    assert_eq!(one["channels"][1]["covered_dwells"], 0);
    assert_eq!(one["channels"][1]["short_dwells"], 0);
    assert_eq!(one["channels"][1]["attempts"], json!([]));
    let first = &one["channels"][0]["attempts"][0];
    assert_eq!(first["pass_index"], 1);
    assert_eq!(first["pass_directory"], Value::Null);
    assert_eq!(first["source_sweep_sha256"], first_sweep);
    assert_eq!(first["tune_directory"], "tune-0001");
    assert_eq!(first["source_sidecar_sha256"], first_sidecar);
    assert_eq!(first["replay"]["analyzed_samples"], samples);
    assert_eq!(first["replay"]["analyzed_iq_sha256"], iq_sha256);
    assert_eq!(first["replay"]["frames"][0]["frame_hex"], FRAME_HEX);

    let output = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "replay-sweep",
            "series/pass-0001",
            "--aprs-channel-hz",
            &(CHANNEL_HZ + 200_000).to_string(),
            "--aprs-channel-hz",
            &CHANNEL_HZ.to_string(),
            "--share-iq",
            "--output",
            "shared.json",
        ])
        .output()
        .unwrap();
    let shared = read_report(root, "shared.json", output);
    assert_eq!(shared["shared_iq"]["source_iq_bytes"], samples * 4);
    assert_eq!(shared["shared_iq"]["max_buffered_iq_bytes"], samples * 4);
    assert_eq!(shared["shared_iq"]["shared_dwells"], 1);
    assert_eq!(shared["aprs"]["decoded_samples"], 2 * samples);
    let channels = shared["aprs"]["channels"].as_array().unwrap();
    assert_eq!(channels[0], one["channels"][0]);
    assert_eq!(channels[1]["channel_frequency_hz"], CHANNEL_HZ + 200_000);
    assert_eq!(
        channels[1]["attempts"][0]["replay"]["analyzed_iq_sha256"],
        iq_sha256
    );

    let all = read_report(
        root,
        "all.json",
        replay_sweep(root, "series", "all.json", &["--series"]),
    );
    assert_eq!(all["capture_kind"], "series");
    assert_eq!(all["source_capture_sha256"], series_sha256);
    assert_eq!(all["decoded_samples"], samples * 2);
    assert_eq!(all["validated_frames"], 1);
    assert_eq!(all["channels"][0]["covered_dwells"], 2);
    let attempts = all["channels"][0]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["pass_index"], 1);
    assert_eq!(attempts[0]["pass_directory"], "pass-0001");
    assert_eq!(attempts[0]["replay"], first["replay"]);
    assert_eq!(attempts[1]["pass_index"], 2);
    assert_eq!(attempts[1]["pass_directory"], "pass-0002");
    assert_eq!(attempts[1]["source_sweep_sha256"], second_sweep);
    assert_eq!(attempts[1]["source_sidecar_sha256"], second_sidecar);
    assert_eq!(attempts[1]["replay"]["analyzed_samples"], samples);
    assert_eq!(attempts[1]["replay"]["validated_frames"], 0);

    let short = read_report(
        root,
        "short.json",
        replay_sweep(
            root,
            "series/pass-0001",
            "short.json",
            &["--max-samples-per-dwell", "2799999"],
        ),
    );
    assert_eq!(short["decoded_samples"], 0);
    assert_eq!(short["channels"][0]["covered_dwells"], 1);
    assert_eq!(short["channels"][0]["short_dwells"], 1);
    assert_eq!(short["channels"][0]["attempts"], json!([]));

    let budget = replay_sweep(
        root,
        "series",
        "budget.json",
        &["--series", "--max-total-samples", &samples.to_string()],
    );
    assert!(!budget.status.success());
    assert!(String::from_utf8_lossy(&budget.stderr).contains("--max-total-samples"));
    assert!(!root.join("budget.json").exists());

    series["passes"][1]["sweep_sha256"] = first_sweep.into();
    write_receipt(&series_path, &series);
    let invalid = replay_sweep(root, "series", "bad-digest.json", &["--series"]);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("sweep receipt SHA-256 differs"));
    assert!(!root.join("bad-digest.json").exists());
}
