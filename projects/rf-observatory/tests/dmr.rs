//! Verify DMR burst replay, sweep coverage and work admission through the CLI.

use std::f64::consts::TAU;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CENTER_HZ: u64 = 440_000_000;
const FIRST_CHANNEL_HZ: u64 = 444_000_000;
const SECOND_CHANNEL_HZ: u64 = 444_012_500;
const SAMPLE_RATE_HZ: u64 = 20_000_000;
const FRAME_SAMPLES: u64 = 550_000;
const DWELL_MS: u64 = 30;
const DWELL_SAMPLES: u64 = DWELL_MS * (SAMPLE_RATE_HZ / 1_000);
const DWELL_BYTES: u64 = DWELL_SAMPLES * 4;
const PROFILE_NAME: &str = "dmr-profile.json";

// MMDVM's VH_1K calibration burst after removing its serial control byte.
// https://github.com/g4klx/MMDVM/blob/1a3b2ec4c4401d699cf7743400ca2c63f67b7289/CalDMR.cpp#L27-L31
const VOICE_HEADER: [u8; 33] = [
    0x00, 0x20, 0x08, 0x08, 0x02, 0x38, 0x15, 0x00, 0x2c, 0xa0, 0x14, 0x60, 0x84, 0x6d, 0xff, 0x57,
    0xd7, 0x5d, 0xf5, 0xde, 0x30, 0x30, 0x01, 0x10, 0x01, 0x40, 0x03, 0xc0, 0x13, 0xc1, 0x1e, 0x80,
    0x6f,
];

fn write_json(path: &Path, document: Value) {
    fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

fn write_sweep(root: &Path) {
    let tune = root.join("tune-0001");
    fs::create_dir(&tune).unwrap();
    let mut iq = File::create(tune.join("wideband.sc16q11")).unwrap();
    iq.set_len(DWELL_BYTES).unwrap();
    // A sample outside SC16 Q11 proves that preview bypasses the IQ payload.
    iq.write_all(&i16::MAX.to_le_bytes()).unwrap();
    write_json(
        &tune.join("wideband.sc16q11.json"),
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
            "num_samples": DWELL_SAMPLES,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    write_json(
        &root.join("sweep.json"),
        json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ,
            "upper_hz": CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": DWELL_MS,
            "iq_bytes": DWELL_BYTES,
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
}

fn execute(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn preview(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn saved_iq_decodes_a_frequency_offset_and_inverted_voice_header() {
    const RATE_HZ: u32 = 96_000;
    const SYMBOL_SAMPLES: usize = 20;
    const SILENCE_SAMPLES: usize = 517;
    const CARRIER_OFFSET_HZ: i64 = 720;
    const DEVIATIONS_HZ: [f64; 4] = [648.0, 1_944.0, -648.0, -1_944.0];
    let mut iq = vec![0; 4 * SILENCE_SAMPLES];
    let mut phase = 0.417_f64;
    for byte in VOICE_HEADER {
        for shift in [6, 4, 2, 0] {
            let frequency =
                CARRIER_OFFSET_HZ as f64 - DEVIATIONS_HZ[usize::from((byte >> shift) & 3)];
            let step = TAU * frequency / f64::from(RATE_HZ);
            for _ in 0..SYMBOL_SAMPLES {
                let (sin, cos) = phase.sin_cos();
                iq.extend_from_slice(&((1_200.0 * cos).round() as i16).to_le_bytes());
                iq.extend_from_slice(&((1_200.0 * sin).round() as i16).to_le_bytes());
                phase += step;
            }
        }
    }
    let burst_end = iq.len() / 4;
    iq.resize(iq.len() + 4 * SILENCE_SAMPLES, 0);
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::write(root.join("voice.sc16q11"), &iq).unwrap();
    let channel = FIRST_CHANNEL_HZ.to_string();
    let result = execute(
        root,
        &[
            "identify-dmr",
            "voice.sc16q11",
            "--center-hz",
            &channel,
            "--channel-hz",
            &channel,
            "--sample-rate-hz",
            &RATE_HZ.to_string(),
            "--output",
            "report.json",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("report.json")).unwrap()).unwrap();
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    assert_eq!(report["validated_frames"], 1);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    let header = &frames[0];
    let expected = json!({
        "sync": "base_station",
        "burst_kind": "header",
        "source_id": 1,
        "destination_kind": "group",
        "destination_id": 9,
        "color_code": 1,
        "spectrum_inverted": true,
        "corrected_slot_type_bits": 0,
        "corrected_bptc_bits": 0,
    });
    for (field, value) in expected.as_object().unwrap() {
        assert_eq!(&header[field.as_str()], value, "{field}");
    }
    let start = header["first_input_sample"].as_u64().unwrap();
    let end = header["end_input_sample_exclusive"].as_u64().unwrap();
    let offset = header["estimated_carrier_offset_hz"].as_i64().unwrap();
    assert!(start.abs_diff(SILENCE_SAMPLES as u64) <= 5, "{header}");
    assert!(end.abs_diff(burst_end as u64) <= 5, "{header}");
    assert!(offset.abs_diff(CARRIER_OFFSET_HZ) <= 50, "{header}");
}

#[test]
fn saved_dmr_preview_charges_channels_and_skips_short_or_uncovered_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_sweep(root);
    write_json(
        &root.join(PROFILE_NAME),
        json!({
            "schema_version": 1,
            "dmr_channel_hz": [SECOND_CHANNEL_HZ, CENTER_HZ, FIRST_CHANNEL_HZ, SECOND_CHANNEL_HZ],
        }),
    );
    let plan = |total: u64, prefix: u64| {
        execute(
            root,
            &[
                "replay-sweep",
                ".",
                "--signal-profile",
                PROFILE_NAME,
                "--plan-only",
                "--share-iq",
                "--max-total-samples",
                &total.to_string(),
                "--max-samples-per-dwell",
                &prefix.to_string(),
            ],
        )
    };
    let requested = 2 * DWELL_SAMPLES;
    let full = preview(plan(requested, DWELL_SAMPLES));
    assert_eq!(
        full["analyzers"],
        json!([{
            "analyzer": "dmr",
            "eligible_attempts": 2,
            "short_attempts": 0,
            "replayed_iq_samples": requested,
            "budget_samples": requested,
            "max_budget_samples": requested,
            "budget_unit": "iq_samples",
        }])
    );
    assert_eq!(
        full["shared_iq"],
        json!({
            "source_iq_bytes": DWELL_BYTES,
            "max_buffered_iq_bytes": DWELL_BYTES,
            "shared_dwells": 1,
        })
    );
    let rejected = plan(requested - 1, DWELL_SAMPLES);
    assert!(!rejected.status.success());
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("DMR replay needs") && error.contains("--max-total-samples"),
        "{error}"
    );

    let minimum = preview(plan(2 * FRAME_SAMPLES, FRAME_SAMPLES));
    assert_eq!(minimum["analyzers"][0]["eligible_attempts"], 2);
    assert_eq!(minimum["shared_iq"]["source_iq_bytes"], FRAME_SAMPLES * 4);
    let short = preview(plan(1, FRAME_SAMPLES - 1));
    assert_eq!(short["analyzers"][0]["short_attempts"], 2);
    assert_eq!(short["analyzers"][0]["budget_samples"], 0);
    assert_eq!(short["shared_iq"]["source_iq_bytes"], 0);
}

#[test]
fn capture_plan_reserves_a_full_dmr_burst_and_bounds_replay_before_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write_json(
        &root.join(PROFILE_NAME),
        json!({
            "schema_version": 1,
            "dmr_channel_hz": [FIRST_CHANNEL_HZ, SECOND_CHANNEL_HZ],
        }),
    );
    let plan = |limit: u64| {
        execute(
            root,
            &[
                "sweep",
                "--signal-profile",
                PROFILE_NAME,
                "--dwell-ms",
                "1",
                "--replay",
                "--max-total-samples",
                &limit.to_string(),
                "--plan-only",
            ],
        )
    };
    let requested = 2 * DWELL_SAMPLES;
    let checked = preview(plan(requested));
    let centers = checked["centers_hz"].as_array().unwrap();
    assert_eq!(centers.len(), 1);
    assert_eq!(
        checked["dwell_ms_by_center"][centers[0].to_string()],
        DWELL_MS
    );
    assert_eq!(checked["planned_iq_bytes"], DWELL_BYTES);
    assert_eq!(
        checked["replay_preflight"]["analyzers"],
        json!([{
            "analyzer": "dmr",
            "maximum_budget_samples": requested,
            "max_budget_samples": requested,
            "budget_unit": "iq_samples",
        }])
    );

    let rejected = plan(requested - 1);
    assert!(!rejected.status.success());
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("dmr sweep replay") && error.contains("--max-total-samples"),
        "{error}"
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);
}
