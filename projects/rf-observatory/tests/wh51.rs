//! Exercise wideband WH51 identification with controlled FSK bursts.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const SAMPLES_PER_US: usize = 20;
const CENTER_HZ: u64 = 913_000_000;
const CHANNEL_HZ: u64 = 915_000_000;
const SYMBOL_SAMPLES: usize = 58 * SAMPLES_PER_US;
const BURST_START_US: [usize; 3] = [2_000, 52_000, 102_000];
const PREAMBLE_AND_SYNC: [u8; 8] = [0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x2d, 0xd4];
const IQ_BYTES_PER_SAMPLE: usize = 4;
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";

// Documented payload with independently calculated CRC and sum bytes.
const PAYLOAD: [u8; 14] = [
    0x51, 0x00, 0x6b, 0x58, 0x6e, 0x7f, 0x24, 0xf8, 0xd2, 0xff, 0xff, 0xff, 0x3c, 0x28,
];

fn synthetic_iq() -> Vec<u8> {
    let mut bad_crc = PAYLOAD;
    bad_crc[12] = 0x3d;
    bad_crc[13] = 0x29; // Keep the wrapping sum valid while corrupting the CRC.
    let scenario = [PAYLOAD, bad_crc, PAYLOAD];
    let mut iq = Vec::new();
    let mut phase: f64 = 0.0;
    let nominal_offset_hz = (CHANNEL_HZ - CENTER_HZ) as f64;
    // Both tones are above the nominal channel center. Decisions need a
    // carrier estimate even after the IQ has been shifted into the channel.
    let steps = [-35_000.0, 35_000.0].map(|deviation_hz| {
        TAU * (nominal_offset_hz + 38_000.0 + deviation_hz) / f64::from(SAMPLE_RATE_HZ)
    });
    for (start_us, payload) in BURST_START_US.into_iter().zip(scenario) {
        iq.resize(start_us * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE, 0);
        for byte in PREAMBLE_AND_SYNC.into_iter().chain(payload) {
            for shift in (0..8).rev() {
                let step = steps[usize::from((byte >> shift) & 1)];
                for _ in 0..SYMBOL_SAMPLES {
                    let (q, i) = phase.sin_cos();
                    iq.extend_from_slice(&((875.0 * i).round() as i16).to_le_bytes());
                    iq.extend_from_slice(&((875.0 * q).round() as i16).to_le_bytes());
                    phase += step;
                    if phase >= TAU {
                        phase -= TAU;
                    }
                }
            }
        }
    }
    iq.resize(113_000 * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE, 0);
    iq
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn read_report(root: &Path, name: &str, run: Output) -> Value {
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap()
}

fn replay(root: &Path, channel_hz: u64, output: &str, extra: &[&str]) -> Value {
    let center_hz = CENTER_HZ.to_string();
    let channel_hz = channel_hz.to_string();
    let rate = SAMPLE_RATE_HZ.to_string();
    let mut args = vec![
        "identify-wh51",
        IQ_FILE,
        "--center-hz",
        &center_hz,
        "--channel-hz",
        &channel_hz,
        "--sample-rate-hz",
        &rate,
        "--output",
        output,
    ];
    args.extend_from_slice(extra);
    read_report(root, output, run(root, &args))
}

fn write_json(root: &Path, path: &str, value: Value) -> String {
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(root.join(path), &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_sweep_replay(root: &Path, samples: usize, scalar: &Value) {
    let iq_bytes = samples * IQ_BYTES_PER_SAMPLE;
    let sidecar_digest = write_json(
        root,
        "tune-0001/wideband.sc16q11.json",
        json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": CENTER_HZ,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "gain_db": 40,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "num_samples": samples,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep_digest = write_json(
        root,
        "sweep.json",
        json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ,
            "upper_hz": CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": samples / (SAMPLES_PER_US * 1_000),
            "iq_bytes": iq_bytes,
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": CENTER_HZ,
                "observed_center_hz": CENTER_HZ,
                "observed_bands": [
                    {"lower_hz": CENTER_HZ - 8_000_000, "upper_hz": CENTER_HZ - 100_000},
                    {"lower_hz": CENTER_HZ + 100_000, "upper_hz": CENTER_HZ + 8_000_000},
                ],
            }],
        }),
    );
    let identify_sweep = |limit: usize, output| {
        run(
            root,
            &[
                "identify-wh51-sweep",
                ".",
                "--channel-hz",
                &CHANNEL_HZ.to_string(),
                "--max-total-samples",
                &limit.to_string(),
                "--output",
                output,
            ],
        )
    };
    let standalone = read_report(
        root,
        "sweep-replay.json",
        identify_sweep(samples, "sweep-replay.json"),
    );
    assert_eq!(standalone["source_capture_sha256"], sweep_digest);
    assert_eq!(standalone["validated_frames"], 2);
    assert_eq!(standalone["replayed_dwells"], 1);
    assert_eq!(standalone["dwells"][0]["source_sweep_sha256"], sweep_digest);
    assert_eq!(
        standalone["dwells"][0]["source_sidecar_sha256"],
        sidecar_digest
    );
    assert_eq!(standalone["dwells"][0]["replay"], *scalar);

    let profile_digest = write_json(
        root,
        "signals.json",
        json!({"schema_version": 1, "wh51_channel_hz": CHANNEL_HZ, "ook_channel_hz": CHANNEL_HZ}),
    );
    let combined = run(
        root,
        &[
            "replay-sweep",
            ".",
            "--max-total-samples",
            &samples.to_string(),
            "--signal-profile",
            "signals.json",
            "--share-iq",
            "--output",
            "combined.json",
        ],
    );
    let combined = read_report(root, "combined.json", combined);
    assert_eq!(combined["signal_profile_sha256"], profile_digest);
    assert_eq!(combined["wh51"], standalone);
    assert_eq!(combined["ook"]["replayed_dwells"], 1);
    assert_eq!(combined["shared_iq"]["source_iq_bytes"], iq_bytes);
    assert_eq!(combined["shared_iq"]["shared_dwells"], 1);

    let rejected = identify_sweep(samples - 1, "budget-rejected.json");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--max-total-samples"));
    assert!(!root.join("budget-rejected.json").exists());
}

#[test]
fn wideband_replay_keeps_separate_valid_occurrences_across_carrier_offset() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let iq = synthetic_iq();
    fs::create_dir(root.join("tune-0001")).unwrap();
    fs::write(root.join(IQ_FILE), &iq).unwrap();
    let digest = format!("{:x}", Sha256::digest(&iq));
    let samples = iq.len() / IQ_BYTES_PER_SAMPLE;

    let report = replay(root, CHANNEL_HZ, "all.json", &[]);
    assert_eq!(report["analyzed_iq_sha256"], digest);
    assert_eq!(report["analyzed_samples"], samples);
    assert_eq!(report["validated_frames"], 2, "{report}");
    assert_eq!(report["omitted_valid_frames"], 0);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for (&burst_start_us, frame) in [BURST_START_US[0], BURST_START_US[2]].iter().zip(frames) {
        assert_eq!(frame["id"], 0x006b58);
        assert_eq!(frame["battery_mv"], 1_400);
        assert_eq!(frame["moisture_percent"], 36);
        assert_eq!(frame["transmission_boost"], 3);
        assert_eq!(frame["moisture_adc"], 210);
        let burst_start = (burst_start_us * SAMPLES_PER_US) as u64;
        let burst_end =
            burst_start + ((PREAMBLE_AND_SYNC.len() + PAYLOAD.len()) * 8 * SYMBOL_SAMPLES) as u64;
        let margin = (2 * SYMBOL_SAMPLES) as u64;
        let first = frame["first_input_sample"].as_u64().unwrap();
        let end = frame["end_input_sample_exclusive"].as_u64().unwrap();
        assert!(first >= burst_start && first < end, "{frame}");
        assert!(burst_end.abs_diff(end) <= margin, "{frame}");
    }

    let bounded = replay(root, CHANNEL_HZ, "bounded.json", &["--max-frames", "1"]);
    assert_eq!(bounded["analyzed_iq_sha256"], digest);
    assert_eq!(bounded["validated_frames"], 2);
    assert_eq!(bounded["omitted_valid_frames"], 1);
    assert_eq!(
        bounded["frames"].as_array().unwrap().as_slice(),
        &frames[..1]
    );

    let different_channel = replay(root, CHANNEL_HZ + 700_000, "different-channel.json", &[]);
    assert_eq!(different_channel["validated_frames"], 0);
    assert!(different_channel["frames"].as_array().unwrap().is_empty());

    verify_sweep_replay(root, samples, &report);
}
