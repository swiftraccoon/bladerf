//! Exercise the saved-IQ Morse command with independently keyed RF.

use std::f64::consts::TAU;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RATE_HZ: usize = 2_000_000;
const DIT_SAMPLES: usize = RATE_HZ / 25;
const OFFSET_HZ: f64 = 450_077.0;
const SWEEP_RATE_HZ: usize = 20_000_000;
const CENTER_HZ: u64 = 147_105_000;
const CHANNEL_HZ: u64 = 147_555_000;
const IQ_BYTES: usize = 4;

fn fixture(nfm: bool) -> Vec<u8> {
    let mut key = vec![false; 4];
    for (letter, code) in [b"...", b"---", b"..."].into_iter().enumerate() {
        for (index, &element) in code.iter().enumerate() {
            if index != 0 {
                key.push(false);
            }
            key.extend(std::iter::repeat_n(
                true,
                if element == b'.' { 1 } else { 3 },
            ));
        }
        key.extend(std::iter::repeat_n(false, if letter == 2 { 4 } else { 3 }));
    }
    keyed_iq(nfm, &key, RATE_HZ, DIT_SAMPLES, OFFSET_HZ)
}

fn keyed_iq(
    nfm: bool,
    key: &[bool],
    rate_hz: usize,
    samples_per_unit: usize,
    offset_hz: f64,
) -> Vec<u8> {
    let sample_count = key.len() * samples_per_unit;
    let mut bytes = Vec::with_capacity(IQ_BYTES * sample_count);
    let mut phase = 0.31;
    for index in 0..sample_count {
        let on = key[index / samples_per_unit];
        let time = index as f64 / rate_hz as f64;
        let deviation = if nfm && on {
            1_800.0 * (TAU * 750.0 * time).cos()
        } else {
            0.0
        };
        phase += TAU * (offset_hz + deviation) / rate_hz as f64;
        let amplitude = if nfm || on { 850.0 } else { 0.0 };
        let (q, i) = phase.sin_cos();
        bytes.extend_from_slice(&((i * amplitude).round() as i16).to_le_bytes());
        bytes.extend_from_slice(&((q * amplitude).round() as i16).to_le_bytes());
    }
    bytes
}

fn write_receipt(path: &Path, record: &Value) -> String {
    let bytes = serde_json::to_vec(record).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn archive_sweep(root: &Path, observed_hz: u64, iq: &[u8]) -> (String, String) {
    let tune = root.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    let bytes_per_ms = IQ_BYTES * (SWEEP_RATE_HZ / 1_000);
    assert!(iq.len().is_multiple_of(bytes_per_ms));
    fs::write(tune.join("wideband.sc16q11"), iq).unwrap();
    let sidecar = write_receipt(
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
            "started_utc": "2000-01-01T00:00:00Z"
        }),
    );
    let sweep = write_receipt(
        &root.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ,
            "upper_hz": CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": iq.len() / bytes_per_ms,
            "iq_bytes": iq.len(),
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": CENTER_HZ,
                "observed_center_hz": observed_hz,
                "observed_bands": [
                    {"lower_hz": observed_hz - 8_000_000, "upper_hz": observed_hz - 100_000},
                    {"lower_hz": observed_hz + 100_000, "upper_hz": observed_hz + 8_000_000}
                ]
            }],
        }),
    );
    (sweep, sidecar)
}

fn sweep_command(root: &Path, action: &str, output: &str, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([action, ".", "--output", output])
        .args(options)
        .output()
        .unwrap()
}

fn read_report(root: &Path, output: &str, result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(output)).unwrap()).unwrap()
}

#[test]
fn replays_the_same_morse_phrase_in_both_rf_modes() {
    let directory = tempfile::tempdir().unwrap();
    for (mode, nfm) in [("cw", false), ("nfm", true)] {
        let input = directory.path().join(format!("{mode}.sc16q11"));
        let output = directory.path().join(format!("{mode}.json"));
        let mut iq = fixture(nfm);
        let bytes_analyzed = iq.len();
        let samples_analyzed = bytes_analyzed / 4;
        iq.extend([0; 28]);
        fs::write(&input, &iq).unwrap();

        let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
            .current_dir(directory.path())
            .arg("identify-morse")
            .arg(&input)
            .args([
                "--center-hz",
                "147105000",
                "--channel-hz",
                "147555000",
                "--sample-rate-hz",
                "2000000",
                "--mode",
                mode,
                "--wpm",
                "30",
                "--max-samples",
                &samples_analyzed.to_string(),
                "--output",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let report: Value = serde_json::from_slice(&fs::read(output).unwrap()).unwrap();
        assert_eq!(report["operation"], "identify_morse_in_saved_iq");
        assert_eq!(report["mode"], mode);
        assert_eq!(report["text"], "SOS", "{mode}: {report}");
        assert_eq!(report["source_samples"], samples_analyzed + 7);
        assert_eq!(report["analyzed_samples"], samples_analyzed);
        assert_eq!(report["envelope_samples"], samples_analyzed / 2_000);
        assert_eq!(
            report["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..bytes_analyzed]))
        );
    }
}

#[test]
fn sweep_reuses_an_envelope_and_preserves_scalar_morse_evidence() {
    const MINIMUM: usize = 1_920_000;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    // Three 30 ms marks, 30 ms element gaps, and 70 ms on either edge.
    let key: [bool; 29] = std::array::from_fn(|unit| matches!(unit, 7..=9 | 13..=15 | 19..=21));
    let observed = CENTER_HZ + 7;
    let mut iq = keyed_iq(
        true,
        &key,
        SWEEP_RATE_HZ,
        SWEEP_RATE_HZ / 100,
        CHANNEL_HZ as f64 - observed as f64 + 77.0,
    );
    let prefix = iq.len() / IQ_BYTES;
    let prefix_sha256 = format!("{:x}", Sha256::digest(&iq));
    let saved = (prefix + 1).next_multiple_of(SWEEP_RATE_HZ / 1_000);
    iq.resize(saved * IQ_BYTES, 0);
    iq[prefix * IQ_BYTES] = 1;
    let (sweep_sha256, sidecar_sha256) = archive_sweep(root, observed, &iq);
    drop(iq);

    let profiles = [
        "cw:181000000",
        "nfm:147555000:750:40",
        "nfm:147555000:750:38",
        "nfm:147555000:750:40",
    ];
    let prefix_arg = prefix.to_string();
    let options = |flag: &'static str| {
        let mut args = vec!["--max-samples-per-dwell", prefix_arg.as_str()];
        for profile in profiles {
            args.extend([flag, profile]);
        }
        args
    };
    let flags = options("--profile");
    let report_path = "sweep-morse.json";
    let result = sweep_command(root, "identify-morse-sweep", report_path, &flags);
    let report = read_report(root, report_path, result);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "identify_morse_in_saved_sweeps");
    assert_eq!(report["capture_kind"], "sweep");
    assert_eq!(report["source_capture_sha256"], sweep_sha256);
    assert_eq!(report["sample_rate_hz"], SWEEP_RATE_HZ);
    assert_eq!(report["minimum_dwell_samples"], MINIMUM);
    for count in [
        "pass_count",
        "dwell_count",
        "covered_dwells",
        "replayed_dwells",
    ] {
        assert_eq!(report[count], 1);
    }
    assert_eq!(report["short_dwells"], 0);
    assert_eq!(report["replayed_iq_samples"], prefix);
    assert_eq!(report["profile_samples"], 2 * prefix);
    assert_eq!(report["resolved_characters"], 2);

    let coverage = report["profiles"].as_array().unwrap();
    assert_eq!(coverage.len(), 3);
    for (profile, speed) in coverage[..2].iter().zip([38, 40]) {
        assert_eq!(profile["mode"], "nfm");
        assert_eq!(profile["channel_frequency_hz"], CHANNEL_HZ);
        assert_eq!(profile["tone_hz"], 750);
        assert_eq!(profile["requested_wpm"], speed);
        assert_eq!(profile["covered_dwells"], 1);
        assert_eq!(profile["covered_samples"], saved);
        assert_eq!(profile["replayed_samples"], prefix);
        assert_eq!(profile["replayed_time_ns"], 50 * prefix);
    }
    let uncovered = &coverage[2];
    assert_eq!(uncovered["mode"], "cw");
    assert_eq!(uncovered["channel_frequency_hz"], 181_000_000);
    assert_eq!(uncovered["requested_wpm"], Value::Null);
    assert!(uncovered.get("tone_hz").is_none());
    assert_eq!(uncovered["covered_dwells"], 0);
    assert_eq!(uncovered["short_dwells"], 0);

    let dwells = report["dwells"].as_array().unwrap();
    let [dwell] = dwells.as_slice() else {
        panic!("unexpected dwell count: {}", dwells.len());
    };
    assert_eq!(dwell["pass_index"], 1);
    assert!(dwell.get("pass_directory").is_none());
    assert_eq!(dwell["tune_directory"], "tune-0001");
    assert_eq!(dwell["source_sweep_sha256"], sweep_sha256);
    assert_eq!(dwell["source_sidecar_sha256"], sidecar_sha256);
    assert_eq!(dwell["observed_center_hz"], observed);
    assert_eq!(dwell["saved_samples"], saved);
    assert_eq!(dwell["selected_samples"], prefix);
    assert_eq!(dwell["status"], "replayed");
    let bank = &dwell["replay"];
    assert_eq!(bank["envelope_count"], 1);
    assert_eq!(bank["analyzed_iq_sha256"], prefix_sha256);
    let evidence = bank["profiles"].as_array().unwrap();
    assert_eq!(evidence.len(), 2);
    let input = "tune-0001/wideband.sc16q11";
    for (profile, speed) in evidence.iter().zip(["38", "40"]) {
        let output = format!("scalar-{speed}.json");
        let scalar = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
            .current_dir(root)
            .args([
                "identify-morse",
                input,
                "--center-hz",
                &observed.to_string(),
                "--channel-hz",
                &CHANNEL_HZ.to_string(),
                "--mode",
                "nfm",
                "--tone-hz",
                "750",
                "--wpm",
                speed,
                "--max-samples",
                &prefix_arg,
                "--output",
                &output,
            ])
            .output()
            .unwrap();
        let scalar = read_report(root, &output, scalar);
        assert_eq!(scalar["analyzed_samples"], prefix);
        assert_eq!(scalar["analyzed_iq_sha256"], prefix_sha256);
        assert_eq!(scalar["envelope_samples"], 290);
        assert_eq!(profile["text"], "S", "{profile}");
        assert_eq!(profile["characters"][0]["code"], "...");
        for (field, value) in profile.as_object().unwrap() {
            assert_eq!(
                value, &scalar[field],
                "scalar {speed} WPM field {field} differs"
            );
        }
    }

    let combined_flags = options("--morse-profile");
    let result = sweep_command(root, "replay-sweep", "combined.json", &combined_flags);
    let combined = read_report(root, "combined.json", result);
    assert_eq!(combined["operation"], "replay_saved_sweep");
    assert_eq!(combined["morse"], report);
    for key in ["aprs", "adsb", "lora", "lte", "rds"] {
        assert!(combined.get(key).is_none());
    }

    // An invalid component leaves the archived extent intact and detects early reads.
    fs::OpenOptions::new()
        .write(true)
        .open(root.join(input))
        .unwrap()
        .write_all(&2_048_i16.to_le_bytes())
        .unwrap();
    let budget = (2 * prefix - 1).to_string();
    let mut too_small = flags;
    too_small.extend(["--max-total-profile-samples", &budget]);
    let refused = sweep_command(root, "identify-morse-sweep", "over-budget.json", &too_small);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(stderr.contains("--max-total-profile-samples"), "{stderr}");
    assert!(stderr.contains(&(2 * prefix).to_string()), "{stderr}");
    assert!(!root.join("over-budget.json").exists());

    let short_limit = (MINIMUM - 1).to_string();
    let mut limits = [
        "--profile",
        "nfm:147555000:750:40",
        "--max-samples-per-dwell",
        &short_limit,
    ];
    let result = sweep_command(root, "identify-morse-sweep", "short.json", &limits);
    let short = read_report(root, "short.json", result);
    assert_eq!(short["short_dwells"], 1);
    assert_eq!(short["profile_samples"], 0);
    assert_eq!(short["replayed_iq_samples"], 0);
    assert_eq!(short["dwells"][0]["selected_samples"], MINIMUM - 1);
    assert_eq!(short["dwells"][0]["status"], "short_dwell");
    assert!(short["dwells"][0].get("replay").is_none());

    let old = fs::read(root.join(report_path)).unwrap();
    let collision = sweep_command(root, "identify-morse-sweep", report_path, &limits);
    let stderr = String::from_utf8_lossy(&collision.stderr);
    assert!(!collision.status.success());
    assert!(stderr.contains("without replacing output"), "{stderr}");
    assert_eq!(fs::read(root.join(report_path)).unwrap(), old);

    let minimum = MINIMUM.to_string();
    limits[3] = &minimum;
    let invalid = sweep_command(root, "identify-morse-sweep", "invalid.json", &limits);
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(!invalid.status.success());
    assert!(stderr.contains("outside SC16 Q11"), "{stderr}");
    assert!(!root.join("invalid.json").exists());
}
