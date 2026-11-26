//! Exercise WS90 and WH51 replay on the same guarded 20 MSps dwell.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const SAMPLES_PER_US: usize = 20;
const CENTER_HZ: u64 = 913_000_000;
const CHANNEL_HZ: u64 = 915_000_000;
const SYMBOL_SAMPLES: usize = 58 * SAMPLES_PER_US;
const IQ_BYTES_PER_SAMPLE: usize = 4;
const DWELL_MS: usize = 75;
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";
const PREAMBLE_AND_SYNC: [u8; 8] = [0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0x2d, 0xd4];
const WS90_START_US: [usize; 2] = [2_000, 54_000];

// Both bodies come from public rtl_433 examples and carry their wire integrity fields.
const WS90: [u8; 32] = [
    0x90, 0x01, 0x2f, 0x58, 0x16, 0x32, 0x9c, 0xa2, 0xc5, 0x3f, 0x00, 0x32, 0x00, 0x28, 0x3f, 0xff,
    0xa9, 0x80, 0x02, 0x02, 0x3c, 0x35, 0xda, 0x2f, 0xff, 0xef, 0xfe, 0x00, 0x00, 0xa1, 0x54, 0xc2,
];
const WH51: [u8; 14] = [
    0x51, 0x00, 0x6b, 0x58, 0x6e, 0x7f, 0x24, 0xf8, 0xd2, 0xff, 0xff, 0xff, 0x3c, 0x28,
];

fn synthetic_iq() -> Vec<u8> {
    let mut invalid = WS90;
    invalid[30] += 1;
    invalid[31] += 1; // Preserve the additive checksum while corrupting the CRC.
    let bursts: [(usize, &[u8]); 4] = [
        (WS90_START_US[0], &WS90),
        (22_000, &WH51),
        (34_000, &invalid),
        (WS90_START_US[1], &WS90),
    ];
    let byte_count = DWELL_MS * 1_000 * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE;
    let mut iq = Vec::with_capacity(byte_count);
    let mut phase: f64 = 0.0;
    // Both FSK tones sit above the labeled channel after translation.
    let channel_offset_hz = (CHANNEL_HZ - CENTER_HZ) as f64;
    let steps = [3_000.0, 73_000.0]
        .map(|tone_hz| TAU * (channel_offset_hz + tone_hz) / f64::from(SAMPLE_RATE_HZ));
    for (start_us, payload) in bursts {
        iq.resize(start_us * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE, 0);
        for byte in PREAMBLE_AND_SYNC.into_iter().chain(payload.iter().copied()) {
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
    iq.resize(byte_count, 0);
    iq
}

fn write_json(root: &Path, name: &str, value: Value) -> String {
    let bytes = serde_json::to_vec(&value).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    fs::write(root.join(name), bytes).unwrap();
    digest
}

fn write_saved_sweep(root: &Path, iq: &[u8]) -> (String, String) {
    fs::create_dir(root.join("tune-0001")).unwrap();
    fs::write(root.join(IQ_FILE), iq).unwrap();
    let sidecar_sha256 = write_json(
        root,
        "tune-0001/wideband.sc16q11.json",
        json!({
            "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q", "complex_scale": 2048,
            "frequency_hz": CENTER_HZ, "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ, "gain_db": 40, "gain_mode": "manual",
            "bias_tee_enabled": false, "channel": "RX1", "num_samples": iq.len() / IQ_BYTES_PER_SAMPLE,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep_sha256 = write_json(
        root,
        "sweep.json",
        json!({
            "schema_version": 1, "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ, "upper_hz": CENTER_HZ + 1,
            "antenna": 1, "gain_db": 40, "dwell_ms": DWELL_MS, "iq_bytes": iq.len(),
            "tunes": [{
                "directory": "tune-0001", "requested_center_hz": CENTER_HZ,
                "observed_center_hz": CENTER_HZ,
                "observed_bands": [
                    {"lower_hz": CENTER_HZ - 8_000_000, "upper_hz": CENTER_HZ - 100_000},
                    {"lower_hz": CENTER_HZ + 100_000, "upper_hz": CENTER_HZ + 8_000_000},
                ],
            }],
        }),
    );
    (sweep_sha256, sidecar_sha256)
}

fn replay(root: &Path, args: &[&str], name: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
        .args(["--output", name])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap()
}

#[test]
fn replay_distinguishes_ecowitt_families_and_shares_the_saved_iq_prefix() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let iq = synthetic_iq();
    let (sweep_sha256, sidecar_sha256) = write_saved_sweep(root, &iq);
    let scalar = replay(
        root,
        &[
            "identify-ws90",
            IQ_FILE,
            "--center-hz",
            &CENTER_HZ.to_string(),
        ],
        "ws90-scalar.json",
    );
    assert_eq!(scalar["validated_frames"], 2, "{scalar}");
    assert_eq!(
        scalar["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    assert_eq!(scalar["omitted_valid_frames"], 0);
    let frames = scalar["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for (start_us, frame) in WS90_START_US.into_iter().zip(frames) {
        assert_eq!(frame["id"], 77_656);
        assert_eq!(frame["firmware"], 161);
        let first = frame["first_input_sample"].as_u64().unwrap();
        let end = frame["end_input_sample_exclusive"].as_u64().unwrap();
        let burst_start = (start_us * SAMPLES_PER_US) as u64;
        let burst_end =
            burst_start + ((PREAMBLE_AND_SYNC.len() + WS90.len()) * 8 * SYMBOL_SAMPLES) as u64;
        assert!(burst_start <= first && first < end, "{frame}");
        assert!(
            end.abs_diff(burst_end) <= (2 * SYMBOL_SAMPLES) as u64,
            "{frame}"
        );
    }

    let single = replay(root, &["identify-ws90-sweep", "."], "ws90-sweep.json");
    assert_eq!(single["source_capture_sha256"], sweep_sha256);
    assert_eq!(single["dwells"][0]["source_sidecar_sha256"], sidecar_sha256);
    assert_eq!(single["dwells"][0]["replay"], scalar);
    let capped = replay(
        root,
        &["identify-ws90-sweep", ".", "--max-frames", "1"],
        "ws90-capped.json",
    );
    assert_eq!(capped["validated_frames"], 2);
    assert_eq!(capped["retained_frames"], 1);
    assert_eq!(capped["omitted_valid_frames"], 1);
    let retained = capped["dwells"][0]["replay"]["frames"].as_array().unwrap();
    assert_eq!(retained.as_slice(), &frames[..1]);

    let profile_sha256 = write_json(
        root,
        "sensors.json",
        json!({"schema_version": 1, "ws90_channel_hz": CHANNEL_HZ, "wh51_channel_hz": CHANNEL_HZ}),
    );
    let combined = replay(
        root,
        &[
            "replay-sweep",
            ".",
            "--signal-profile",
            "sensors.json",
            "--share-iq",
        ],
        "combined.json",
    );
    assert_eq!(combined["signal_profile_sha256"], profile_sha256);
    assert_eq!(combined["ws90"], single);
    assert_eq!(combined["wh51"]["validated_frames"], 1);
    assert_eq!(
        combined["wh51"]["dwells"][0]["replay"]["frames"][0]["id"],
        0x006b58
    );
    assert_eq!(combined["shared_iq"]["source_iq_bytes"], iq.len());
    assert_eq!(combined["shared_iq"]["shared_dwells"], 1);
}
