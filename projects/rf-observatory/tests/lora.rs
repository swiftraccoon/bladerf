//! Exercise LoRa preamble detection and the decoder receipt boundary.

use std::f64::consts::TAU;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE: usize = 2_000_000;
const BANDWIDTH: usize = 125_000;
const CHIPS: usize = 128;
const PREFIX_SAMPLES: usize = 347;
const UPCHIRPS: usize = 8;
const SYMBOL_SAMPLES: usize = CHIPS * (SAMPLE_RATE / BANDWIDTH);
const IQ_BYTES: usize = 4;
const SWEEP_RATE: usize = 20_000_000;
const SWEEP_CENTER_HZ: u64 = 915_800_000;
const CHANNEL_HZ: u64 = 915_300_000;

const DECODER_WORKER: &str = r#"#!/bin/sh
set -eu
test "$#" -eq 14
test "$1" = decode
input="$2"
if [ -n "${LORA_FIXTURE_TRANSLATED_PATHS:-}" ]; then
    printf '%s\n' "$input" >> "$LORA_FIXTURE_TRANSLATED_PATHS"
fi
shift 2
test "$1 $3 $5 $7 $9 ${11}" = '--channel-hz --bandwidth-hz --spreading-factor --sync-word --max-frames --output'
source_bytes=$(wc -c < "$input")
test "$((source_bytes % 8))" -eq 0
source_digest=$(sha256sum < "$input")
source_digest=${source_digest%% *}
payload=4c6f526120706173736976652049512074657374
crc=7689
first_symbol=$((($8 / 16) * 8))
expected_second_symbol=$((($8 % 16) * 8))
second_symbol="$expected_second_symbol"
case "$LORA_FIXTURE_MODE" in
    valid) ;;
    bad-crc) crc=7688 ;;
    second-bad-crc)
        if [ "$(wc -l < "$LORA_FIXTURE_TRANSLATED_PATHS")" -eq 2 ]; then
            crc=7688
        fi
        ;;
    bad-sync) second_symbol=80 ;;
    changed-source) printf '\001' >> packet.sc16q11 ;;
    meshtastic|meshtastic-bad-crc)
        payload=ffffffff78563412efcdab90b35a23a7a13c
        crc=56369
        if [ "$LORA_FIXTURE_MODE" = meshtastic-bad-crc ]; then
            crc=56368
        fi
        ;;
    meshtastic-short) payload=a13c; crc=41276 ;;
    lorawan|lorawan-bad-crc)
        # Published Join-request fixture. The LoRa PHY CRC is independent of its MIC.
        payload=00040302010403020105040302050403022d106a990e12
        crc=2726
        if [ "$LORA_FIXTURE_MODE" = lorawan-bad-crc ]; then
            crc=2727
        fi
        ;;
    lorawan-short) payload=00040302010403020105040302050403022d106a990e; crc=14156 ;;
    *) exit 9 ;;
esac
cat > "${12}" <<EOF
{
  "schema_version": 1,
  "operation": "decode_lora_cf32",
  "source_format": "CF32_LE",
  "source_sample_count": $((source_bytes / 8)),
  "source_sha256": "$source_digest",
  "channel_hz": $2,
  "bandwidth_hz": $4,
  "sample_rate_hz": $((4 * $4)),
  "spreading_factor": $6,
  "sync_word": $8,
  "expected_sync_symbols": [$first_symbol,$expected_second_symbol],
  "max_frames": ${10},
  "validated_frames": 1,
  "omitted_valid_frames": 0,
  "rejected_crc_absent": 0,
  "rejected_invalid_crc": 0,
  "rejected_wrong_sync": 0,
  "records": [{
    "synchronization_sample": 73,
    "code_rate_index": 1,
    "observed_sync_symbols": [$first_symbol,$second_symbol],
    "payload_hex": "$payload",
    "payload_crc": $crc
  }]
}
EOF
"#;

fn preamble_iq(sample_rate: usize, channel_offset_hz: f64) -> Vec<u8> {
    let samples_per_chip = sample_rate / BANDWIDTH;
    let symbol_samples = CHIPS * samples_per_chip;
    let prefix = PREFIX_SAMPLES * (sample_rate / SAMPLE_RATE);
    let mut bytes = vec![0; IQ_BYTES * prefix];
    let mut phase: f64 = 1.5;
    let radians_per_hz = TAU / sample_rate as f64;
    let carrier_hz = channel_offset_hz + 214.0;
    let symbols = std::iter::repeat_n((1.0, 0, symbol_samples), UPCHIRPS).chain([
        (1.0, 16, symbol_samples),
        (1.0, 32, symbol_samples),
        (-1.0, 0, 9 * symbol_samples / 4),
    ]);
    for (slope, chip_shift, count) in symbols {
        for offset in 0..count {
            let position = (offset + chip_shift * samples_per_chip) % symbol_samples;
            let fraction = (position as f64 + 0.5) / symbol_samples as f64;
            let shift_hz = slope * (fraction - 0.5) * BANDWIDTH as f64;
            phase += radians_per_hz * (carrier_hz + shift_hz);
            let (q, i) = phase.sin_cos();
            bytes.extend_from_slice(&((850.0 * i).round() as i16).to_le_bytes());
            bytes.extend_from_slice(&((850.0 * q).round() as i16).to_le_bytes());
        }
    }
    bytes.resize(bytes.len() + 2 * IQ_BYTES * symbol_samples, 0);
    bytes
}

fn probe(root: &Path, output: &str, max_samples: Option<usize>) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rf-observatory"));
    command.current_dir(root).args([
        "probe-lora",
        "preamble.sc16q11",
        "--center-hz",
        "915800000",
        "--channel-hz",
        "915300000",
        "--sample-rate-hz",
        "2000000",
        "--bandwidth-hz",
        "125000",
        "--spreading-factor",
        "7",
        "--output",
        output,
    ]);
    if let Some(limit) = max_samples {
        command.arg("--max-samples").arg(limit.to_string());
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(output)).unwrap()).unwrap()
}

#[test]
fn probe_binds_a_shifted_preamble_and_withholds_an_upchirp_only_prefix() {
    let root = tempfile::tempdir().unwrap();
    let iq = preamble_iq(SAMPLE_RATE, -500_000.0);
    fs::write(root.path().join("preamble.sc16q11"), &iq).unwrap();
    let full = probe(root.path(), "full.json", None);
    assert_eq!(full["operation"], "probe_lora_preamble");
    assert_eq!(full["source_samples"], iq.len() / 4);
    assert_eq!(full["analyzed_samples"], iq.len() / 4);
    assert_eq!(
        full["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    assert_eq!(full["source_samples_per_window"], SYMBOL_SAMPLES);
    assert_eq!(full["candidate_count"], 1);
    let candidates = full["candidates"].as_array().unwrap();
    let [candidate] = candidates.as_slice() else {
        panic!("expected one candidate, found {}", candidates.len());
    };
    assert!(candidate["up_windows"].as_u64().unwrap() >= 5);
    assert!((10..=12).contains(&candidate["delimiter_window"].as_u64().unwrap()));

    let limit = PREFIX_SAMPLES + UPCHIRPS * SYMBOL_SAMPLES;
    let prefix = probe(root.path(), "prefix.json", Some(limit));
    assert_eq!(prefix["source_samples"], iq.len() / 4);
    assert_eq!(prefix["analyzed_samples"], limit);
    assert_eq!(prefix["sufficient_extent_for_search"], true);
    assert_eq!(prefix["candidate_count"], 0);
    assert!(prefix["candidates"].as_array().unwrap().is_empty());
    assert_eq!(
        prefix["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..4 * limit]))
    );
}

fn write_receipt(path: &Path, record: &Value) -> String {
    let bytes = serde_json::to_vec(record).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn archive_sweep(root: &Path, observed_hz: u64, iq: &[u8]) -> (String, String) {
    let tune = root.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    let bytes_per_ms = IQ_BYTES * (SWEEP_RATE / 1_000);
    assert!(iq.len().is_multiple_of(bytes_per_ms));
    fs::write(tune.join("wideband.sc16q11"), iq).unwrap();
    let sidecar_sha256 = write_receipt(
        &tune.join("wideband.sc16q11.json"),
        &json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": observed_hz,
            "sample_rate_hz": SWEEP_RATE,
            "bandwidth_hz": SWEEP_RATE,
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
            "lower_hz": SWEEP_CENTER_HZ,
            "upper_hz": SWEEP_CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": iq.len() / bytes_per_ms,
            "iq_bytes": iq.len(),
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": SWEEP_CENTER_HZ,
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

fn sweep_probe(root: &Path, capture: &str, output: &str, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(["probe-lora-sweep", capture, "--output", output])
        .args(options)
        .output()
        .unwrap()
}

fn read_report(root: &Path, path: &str, result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(path)).unwrap()).unwrap()
}

const SWEEP_WINDOW_SAMPLES: usize = CHIPS * (SWEEP_RATE / BANDWIDTH);
const MINIMUM_SF7_SAMPLES: usize = 6 * SWEEP_WINDOW_SAMPLES;

#[test]
fn sweep_probe_shares_channels_and_keeps_per_profile_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let observed = [SWEEP_CENTER_HZ + 7, SWEEP_CENTER_HZ - 7];
    let mut witness = preamble_iq(SWEEP_RATE, CHANNEL_HZ as f64 - observed[0] as f64);
    let prefix = witness.len() / IQ_BYTES;
    let samples_per_ms = SWEEP_RATE / 1_000;
    let saved = prefix.div_ceil(samples_per_ms) * samples_per_ms;
    witness.resize(saved * IQ_BYTES, 0);
    witness[prefix * IQ_BYTES] = 1;
    let quiet = vec![0; saved * IQ_BYTES];
    let pass_names = ["pass-0001", "pass-0002"];
    let first = archive_sweep(&root.join(pass_names[0]), observed[0], &witness);
    let second = archive_sweep(&root.join(pass_names[1]), observed[1], &quiet);
    let manifest = write_receipt(
        &root.join("sweep-series.json"),
        &json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "dwells": 2,
            "iq_bytes": witness.len() + quiet.len(),
            "passes": [
                {"directory": pass_names[0], "sweep_sha256": first.0},
                {"directory": pass_names[1], "sweep_sha256": second.0},
            ],
        }),
    );

    // Deliberately give the same SF7 profile twice and pass profiles out of order.
    let profiles = [
        "--series",
        "--profile",
        "923800000:125000:7",
        "--profile",
        "915300000:125000:8",
        "--profile",
        "915300000:125000:7",
        "--profile",
        "915300000:125000:9",
        "--profile",
        "915300000:125000:7",
    ];
    let prefix_arg = prefix.to_string();
    let mut options = profiles.to_vec();
    options.extend(["--max-samples-per-dwell", &prefix_arg]);
    let output = "probe-sweep.json";
    let report = read_report(root, output, sweep_probe(root, ".", output, &options));
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "probe_lora_in_saved_sweeps");
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["source_capture_sha256"], manifest);
    assert_eq!(report["sample_rate_hz"], SWEEP_RATE);
    for field in ["pass_count", "dwell_count", "replayed_dwells"] {
        assert_eq!(report[field], 2);
    }
    assert_eq!(report["replayed_iq_samples"], 2 * prefix);
    assert_eq!(report["profile_samples"], 4 * prefix);
    assert_eq!(report["candidate_count"], 1);
    assert_eq!(report["omitted_candidates"], 0);

    let coverage = report["profiles"].as_array().unwrap();
    assert_eq!(coverage.len(), 4);
    for (index, profile) in coverage[..3].iter().enumerate() {
        assert_eq!(profile["channel_hz"], CHANNEL_HZ);
        assert_eq!(profile["bandwidth_hz"], BANDWIDTH);
        assert_eq!(profile["spreading_factor"], 7 + index);
        assert_eq!(profile["minimum_samples"], MINIMUM_SF7_SAMPLES << index);
        assert_eq!(profile["covered_dwells"], 2);
        assert_eq!(profile["covered_samples"], 2 * saved);
        assert_eq!(profile["covered_time_ns"], 2 * saved * 50);
        let eligible = index < 2;
        assert_eq!(profile["short_dwells"], 2 * usize::from(!eligible));
        assert_eq!(profile["replayed_dwells"], 2 * usize::from(eligible));
        assert_eq!(
            profile["replayed_samples"],
            2 * prefix * usize::from(eligible)
        );
        assert_eq!(
            profile["replayed_time_ns"],
            2 * prefix * 50 * usize::from(eligible)
        );
    }
    assert_eq!(coverage[3]["channel_hz"], 923_800_000);
    for field in [
        "covered_dwells",
        "short_dwells",
        "replayed_dwells",
        "replayed_samples",
    ] {
        assert_eq!(coverage[3][field], 0);
    }

    let dwells = report["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 2);
    let sources = [(first, &witness), (second, &quiet)];
    for (index, (dwell, ((sweep, sidecar), iq))) in dwells.iter().zip(sources).enumerate() {
        assert_eq!(dwell["pass_index"], index + 1);
        assert_eq!(dwell["pass_directory"], pass_names[index]);
        assert_eq!(dwell["tune_directory"], "tune-0001");
        assert_eq!(dwell["observed_center_hz"], observed[index]);
        assert_eq!(dwell["source_sweep_sha256"], sweep);
        assert_eq!(dwell["source_sidecar_sha256"], sidecar);
        assert_eq!(dwell["saved_samples"], saved);
        assert_eq!(dwell["selected_samples"], prefix);
        assert_eq!(
            dwell["short_profiles"],
            json!([{
                "channel_hz": CHANNEL_HZ, "bandwidth_hz": BANDWIDTH, "spreading_factor": 9
            }])
        );
        let replay = &dwell["replay"];
        assert_eq!(
            replay["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..prefix * IQ_BYTES]))
        );
        assert_eq!(replay["channelizer_count"], 1);
        let replays = replay["profiles"].as_array().unwrap();
        assert_eq!(replays.len(), 2);
        for (factor_index, profile) in replays.iter().enumerate() {
            assert_eq!(profile["channel_hz"], CHANNEL_HZ);
            assert_eq!(profile["bandwidth_hz"], BANDWIDTH);
            assert_eq!(profile["spreading_factor"], 7 + factor_index);
            let window_samples = SWEEP_WINDOW_SAMPLES << factor_index;
            assert_eq!(profile["source_samples_per_window"], window_samples);
            assert_eq!(profile["processed_windows"], prefix / window_samples);
            assert_eq!(
                profile["candidate_count"],
                usize::from(index == 0 && factor_index == 0)
            );
        }
    }
    let candidate = &dwells[0]["replay"]["profiles"][0]["candidates"][0];
    assert!(candidate["up_windows"].as_u64().unwrap() >= 5);
    assert!((10..=12).contains(&candidate["delimiter_window"].as_u64().unwrap()));

    let saved_report = fs::read(root.join(output)).unwrap();
    let short_arg = (MINIMUM_SF7_SAMPLES - 1).to_string();
    let mut short_options = profiles.to_vec();
    short_options.extend(["--max-samples-per-dwell", &short_arg]);
    let collision = sweep_probe(root, ".", output, &short_options);
    let stderr = String::from_utf8_lossy(&collision.stderr);
    assert!(!collision.status.success());
    assert!(stderr.contains("without replacing output"), "{stderr}");
    assert_eq!(fs::read(root.join(output)).unwrap(), saved_report);

    // Keep the extent valid while making the first sample fail IQ validation.
    witness[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    fs::write(root.join("pass-0001/tune-0001/wideband.sc16q11"), witness).unwrap();
    let small_budget = (4 * prefix - 1).to_string();
    options.extend(["--max-total-profile-samples", &small_budget]);
    let failure = sweep_probe(root, ".", "budget.json", &options);
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(!failure.status.success());
    assert!(stderr.contains("--max-total-profile-samples"), "{stderr}");
    assert!(stderr.contains(&(4 * prefix).to_string()), "{stderr}");
    assert!(!root.join("budget.json").exists());
}

#[test]
fn sweep_probe_requires_six_complete_windows_before_reading_iq() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let samples = MINIMUM_SF7_SAMPLES.next_multiple_of(SWEEP_RATE / 1_000);
    let mut iq = vec![0; samples * IQ_BYTES];
    iq[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    let (digest, sidecar) = archive_sweep(root, SWEEP_CENTER_HZ, &iq);
    let short = (MINIMUM_SF7_SAMPLES - 1).to_string();
    let mut flags = [
        "--profile",
        "915300000:125000:7",
        "--max-samples-per-dwell",
        &short,
    ];
    let report = read_report(
        root,
        "short.json",
        sweep_probe(root, ".", "short.json", &flags),
    );
    assert_eq!(report["capture_kind"], "sweep");
    assert_eq!(report["source_capture_sha256"], digest);
    assert_eq!(
        report["profiles"][0]["minimum_samples"],
        MINIMUM_SF7_SAMPLES
    );
    assert_eq!(report["profiles"][0]["short_dwells"], 1);
    for key in [
        "replayed_dwells",
        "replayed_iq_samples",
        "profile_samples",
        "candidate_count",
    ] {
        assert_eq!(report[key], 0);
    }
    let dwell = &report["dwells"][0];
    assert!(dwell.get("pass_directory").is_none());
    assert_eq!(dwell["source_sidecar_sha256"], sidecar);
    assert_eq!(dwell["saved_samples"], samples);
    assert_eq!(dwell["selected_samples"], MINIMUM_SF7_SAMPLES - 1);
    assert_eq!(
        dwell["short_profiles"],
        json!([{
            "channel_hz": CHANNEL_HZ, "bandwidth_hz": BANDWIDTH, "spreading_factor": 7
        }])
    );
    assert!(dwell.get("replay").is_none());

    let complete = MINIMUM_SF7_SAMPLES.to_string();
    flags[3] = &complete;
    let failure = sweep_probe(root, ".", "invalid.json", &flags);
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(!failure.status.success());
    assert!(stderr.contains("outside SC16 Q11"), "{stderr}");
    assert!(!root.join("invalid.json").exists());
}

fn decode(root: &Path, output: &str, mode: &str, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .env("LORA_FIXTURE_MODE", mode)
        .args([
            "decode-lora",
            "packet.sc16q11",
            "--center-hz",
            "915800000",
            "--channel-hz",
            "915300000",
            "--sample-rate-hz",
            "2000000",
            "--bandwidth-hz",
            "125000",
            "--spreading-factor",
            "7",
            "--start-sample",
            "7",
            "--max-samples",
            "1031",
            "--max-frames",
            "1",
            "--worker",
            "worker.sh",
            "--output",
            output,
        ])
        .args(options)
        .output()
        .unwrap()
}

fn directory_names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn install_worker(root: &Path) {
    let worker = root.join("worker.sh");
    fs::write(&worker, DECODER_WORKER).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
}

fn decode_sweep(root: &Path, output: &str, mode: &str, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .env("LORA_FIXTURE_MODE", mode)
        .env(
            "LORA_FIXTURE_TRANSLATED_PATHS",
            root.join("worker-inputs.log"),
        )
        .args([
            "decode-lora-sweep",
            ".",
            "--worker",
            "worker.sh",
            "--max-frames",
            "1",
            "--timeout-seconds",
            "5",
            "--output",
            output,
        ])
        .args(options)
        .output()
        .unwrap()
}

#[test]
fn decode_checks_worker_receipts_and_cleans_up_after_rejection() {
    const START: usize = 7;
    const SAMPLES: usize = 1_031;
    const TRANSLATED: usize = SAMPLES / 4;
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    install_worker(root);
    // The translated interval contains only zeros, with sentinels outside it.
    let mut iq = vec![0; 4 * (START + SAMPLES + 7)];
    iq[0] = 1;
    iq[4 * (START + SAMPLES)] = 2;
    fs::write(root.join("packet.sc16q11"), &iq).unwrap();

    let result = decode(root, "decoded", "valid", &["--sync-word", "43"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = root.join("decoded");
    assert_eq!(directory_names(&output), ["decode.json", "worker.json"]);
    let receipt = fs::read(output.join("worker.json")).unwrap();
    let worker_record: Value = serde_json::from_slice(&receipt).unwrap();
    let report: Value =
        serde_json::from_slice(&fs::read(output.join("decode.json")).unwrap()).unwrap();
    assert!(report.get("meshtastic").is_none());
    let cf32_digest = format!("{:x}", Sha256::digest([0; 8 * TRANSLATED]));
    assert_eq!(report["operation"], "decode_lora_saved_iq");
    assert_eq!(report["start_sample"], START);
    assert_eq!(report["analyzed_samples"], SAMPLES);
    assert_eq!(report["source_samples"], iq.len() / 4);
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!(
            "{:x}",
            Sha256::digest(&iq[4 * START..4 * (START + SAMPLES)])
        )
    );
    assert_eq!(report["channelized_sample_count"], TRANSLATED);
    assert_eq!(report["channelized_sample_rate_hz"], 4 * BANDWIDTH);
    assert_eq!(report["channelized_iq_sha256"], cf32_digest);
    assert_eq!(
        report["worker_receipt_sha256"],
        format!("{:x}", Sha256::digest(&receipt))
    );
    assert_eq!(
        report["worker_sha256"],
        format!("{:x}", Sha256::digest(DECODER_WORKER))
    );
    assert_eq!(report["validated_frames"], 1);
    assert_eq!(worker_record["source_sample_count"], TRANSLATED);
    assert_eq!(worker_record["source_sha256"], cf32_digest);
    assert_eq!(worker_record["expected_sync_symbols"], json!([16, 88]));
    assert_eq!(
        worker_record["records"][0]["payload_hex"],
        "4c6f526120706173736976652049512074657374"
    );
    assert_eq!(worker_record["records"][0]["payload_crc"], 7_689);

    for (mode, error) in [
        ("bad-crc", "LoRa worker payload CRC mismatch"),
        ("bad-sync", "invalid LoRa frame metadata"),
        ("changed-source", "LoRa IQ input changed after admission"),
    ] {
        let result = decode(root, mode, mode, &["--sync-word", "43"]);
        assert!(!result.status.success(), "{mode} unexpectedly succeeded");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(stderr.contains(error), "{mode}: {stderr}");
        assert_eq!(
            directory_names(root),
            ["decoded", "packet.sc16q11", "worker.sh"]
        );
    }
}

#[test]
fn meshtastic_opt_in_annotates_verified_headers_without_replacing_worker_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    install_worker(root);
    fs::write(root.join("packet.sc16q11"), [0; IQ_BYTES * (7 + 1_031)]).unwrap();

    let generic = read_report(
        root,
        "generic/decode.json",
        decode(root, "generic", "meshtastic", &["--sync-word", "43"]),
    );
    let meshtastic_options = ["--sync-word", "43", "--meshtastic"];
    let mut verified = read_report(
        root,
        "typed/decode.json",
        decode(root, "typed", "meshtastic", &meshtastic_options),
    );
    let annotation = verified
        .as_object_mut()
        .unwrap()
        .remove("meshtastic")
        .unwrap();
    assert_eq!(verified, generic);
    assert_eq!(
        annotation,
        json!({
            "checked_retained_frames": 1,
            "incompatible_retained_frames": 0,
            "records": [{
                "worker_record_index": 0,
                "synchronization_sample": 73,
                "origin_node_id": 0x1234_5678_u32,
                "destination_node_id": u32::MAX,
                "packet_id": 0x90ab_cdef_u32,
                "broadcast": true,
                "channel_hash": 0x5a,
                "hop_limit": 3,
                "hop_start": 5,
                "want_ack": false,
                "via_mqtt": true,
                "next_hop_byte": 0x23,
                "relay_node_byte": 0xa7,
            }],
        }),
    );
    assert_eq!(
        directory_names(&root.join("typed")),
        ["decode.json", "worker.json"]
    );
    assert_eq!(
        fs::read(root.join("typed/worker.json")).unwrap(),
        fs::read(root.join("generic/worker.json")).unwrap(),
    );

    let short = read_report(
        root,
        "short/decode.json",
        decode(root, "short", "meshtastic-short", &meshtastic_options),
    );
    assert_eq!(short["validated_frames"], 1);
    assert_eq!(
        short["meshtastic"],
        json!({
            "checked_retained_frames": 1,
            "incompatible_retained_frames": 1,
            "records": [],
        })
    );
    let before = directory_names(root);
    let bad = decode(root, "bad", "meshtastic-bad-crc", &meshtastic_options);
    assert!(!bad.status.success());
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("LoRa worker payload CRC mismatch"),
        "{stderr}"
    );
    assert_eq!(directory_names(root), before);

    let wrong_sync = decode(
        root,
        "bad-sync",
        "meshtastic",
        &["--sync-word", "44", "--meshtastic"],
    );
    assert!(!wrong_sync.status.success());
    let stderr = String::from_utf8_lossy(&wrong_sync.stderr);
    assert!(
        stderr.contains("Meshtastic") && stderr.contains("43"),
        "{stderr}"
    );
    assert_eq!(directory_names(root), before);

    check_meshtastic_sweep(root, &annotation);
}

fn check_meshtastic_sweep(root: &Path, annotation: &Value) {
    let samples = (17 * SWEEP_WINDOW_SAMPLES).next_multiple_of(SWEEP_RATE / 1_000);
    let iq = vec![0; samples * IQ_BYTES];
    archive_sweep(root, SWEEP_CENTER_HZ, &iq);
    let profile = "915300000:125000:7:43";
    let profile_sha = write_receipt(
        &root.join("mesh.json"),
        &json!({
            "schema_version": 1,
            "lora_packet_profile": [profile],
            "lora_meshtastic": true,
        }),
    );
    for (output, options) in [
        (
            "flag",
            [
                "--profile",
                "915300000:125000:7:34",
                "--profile",
                profile,
                "--meshtastic",
            ]
            .as_slice(),
        ),
        ("profile", ["--signal-profile", "mesh.json"].as_slice()),
    ] {
        let report = read_report(
            root,
            &format!("{output}/decode-sweep.json"),
            decode_sweep(root, output, "meshtastic", options),
        );
        assert_eq!(report["meshtastic_headers"], 1);
        let attempts = report["attempts"].as_array().unwrap();
        if output == "profile" {
            assert_eq!(report["signal_profile_sha256"], profile_sha);
            assert_eq!(attempts.len(), 1);
        } else {
            assert_eq!(attempts.len(), 2);
            let generic = &attempts[0];
            assert_eq!(generic["sync_word"], 34);
            assert!(generic.get("meshtastic_headers").is_none());
            let path = root
                .join(output)
                .join(generic["decode_receipt"].as_str().unwrap());
            let receipt: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert!(receipt.get("meshtastic").is_none());
        }
        let attempt = attempts.last().unwrap();
        assert_eq!(attempt["sync_word"], 43);
        assert_eq!(attempt["meshtastic_headers"], 1);
        let receipt_path = root
            .join(output)
            .join(attempt["decode_receipt"].as_str().unwrap());
        let receipt = fs::read(&receipt_path).unwrap();
        assert_eq!(
            attempt["decode_receipt_sha256"],
            format!("{:x}", Sha256::digest(&receipt))
        );
        let receipt: Value = serde_json::from_slice(&receipt).unwrap();
        assert_eq!(receipt["meshtastic"], *annotation);
        assert_eq!(
            directory_names(receipt_path.parent().unwrap()),
            ["decode.json", "worker.json"]
        );
    }

    let log = root.join("worker-inputs.log");
    let launches = fs::read_to_string(&log).unwrap();
    let paths: Vec<_> = launches.lines().collect();
    assert_eq!(paths.len(), 3);
    assert_eq!(paths[0], paths[1]);
    for path in paths {
        assert!(!root.join(path).exists(), "translated IQ remains at {path}");
    }

    write_receipt(
        &root.join("wrong-sync.json"),
        &json!({
            "schema_version": 1,
            "lora_packet_profile": ["915300000:125000:7:44"],
            "lora_meshtastic": true,
        }),
    );
    // Invalid Q11 with the right extent proves profile rejection precedes replay.
    fs::OpenOptions::new()
        .write(true)
        .open(root.join("tune-0001/wideband.sc16q11"))
        .unwrap()
        .write_all(&2_048_i16.to_le_bytes())
        .unwrap();
    let rejected = decode_sweep(
        root,
        "wrong-sync",
        "meshtastic",
        &["--signal-profile", "wrong-sync.json"],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains("Meshtastic") && stderr.contains("43"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(log).unwrap(), launches);
    assert!(!root.join("wrong-sync").exists());
}

#[test]
fn lorawan_join_opt_in_requires_the_phy_and_preserves_original_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    install_worker(root);
    fs::write(root.join("packet.sc16q11"), [0; IQ_BYTES * (7 + 1_031)]).unwrap();

    let generic = read_report(
        root,
        "generic/decode.json",
        decode(root, "generic", "lorawan", &["--sync-word", "52"]),
    );
    let options = ["--sync-word", "52", "--lorawan-join"];
    let mut typed = read_report(
        root,
        "typed/decode.json",
        decode(root, "typed", "lorawan", &options),
    );
    let annotation = typed
        .as_object_mut()
        .unwrap()
        .remove("lorawan_join")
        .unwrap();
    assert_eq!(typed, generic);
    assert_eq!(
        annotation,
        json!({
            "checked_retained_frames": 1,
            "incompatible_retained_frames": 0,
            "records": [{
                "worker_record_index": 0,
                "synchronization_sample": 73,
                "join_eui": "0102030401020304",
                "dev_eui": "0203040502030405",
                "dev_nonce": 4141,
                "mic_hex": "6a990e12",
            }],
        })
    );
    assert_eq!(
        fs::read(root.join("typed/worker.json")).unwrap(),
        fs::read(root.join("generic/worker.json")).unwrap(),
    );

    let short = read_report(
        root,
        "short/decode.json",
        decode(root, "short", "lorawan-short", &options),
    );
    assert_eq!(short["validated_frames"], 1);
    assert_eq!(
        short["lorawan_join"],
        json!({"checked_retained_frames": 1, "incompatible_retained_frames": 1, "records": []})
    );

    let before = directory_names(root);
    for (mode, options, error) in [
        (
            "lorawan-bad-crc",
            options,
            "LoRa worker payload CRC mismatch",
        ),
        (
            "lorawan",
            ["--sync-word", "43", "--lorawan-join"],
            "LoRaWAN Join-request decoding requires sync word 52",
        ),
    ] {
        let rejected = decode(root, "rejected", mode, &options);
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains(error), "{stderr}");
        assert_eq!(directory_names(root), before);
    }

    let samples = (17 * SWEEP_WINDOW_SAMPLES).next_multiple_of(SWEEP_RATE / 1_000);
    archive_sweep(root, SWEEP_CENTER_HZ, &vec![0; samples * IQ_BYTES]);
    let report = read_report(
        root,
        "lora/decode-sweep.json",
        decode_sweep(
            root,
            "lora",
            "lorawan",
            &[
                "--profile",
                "915300000:125000:7:43",
                "--profile",
                "915300000:125000:7:52",
                "--lorawan-join",
            ],
        ),
    );
    let attempts = report["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts[0].get("lorawan_join_requests").is_none());
    assert_eq!(attempts[1]["lorawan_join_requests"], 1);
    assert_eq!(report["lorawan_join_requests"], 1);
    let annotated = root
        .join("lora")
        .join(attempts[1]["decode_receipt"].as_str().unwrap());
    let receipt: Value = serde_json::from_slice(&fs::read(annotated).unwrap()).unwrap();
    assert_eq!(receipt["lorawan_join"], annotation);
}

#[test]
fn sweep_reuses_one_original_prefix_across_distinct_channels() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    install_worker(root);
    let samples = (17 * SWEEP_WINDOW_SAMPLES).next_multiple_of(SWEEP_RATE / 1_000);
    let mut iq = vec![0; samples * IQ_BYTES];
    iq[..2].copy_from_slice(&1024_i16.to_le_bytes());
    iq.last_chunk_mut::<2>()
        .unwrap()
        .copy_from_slice(&(-1024_i16).to_le_bytes());
    archive_sweep(root, SWEEP_CENTER_HZ, &iq);
    let profiles = [
        "--profile",
        "915300000:125000:7:43",
        "--profile",
        "916300000:250000:7:52",
    ];
    let preview = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(["decode-lora-sweep", ".", "--plan-only"])
        .args(profiles)
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["attempt_count"], 2);
    assert_eq!(plan["channelization_count"], 2);
    assert_eq!(plan["source_iq_bytes"], iq.len());
    assert_eq!(plan["max_buffered_iq_bytes"], iq.len());
    assert_eq!(plan["peak_temporary_iq_bytes"], samples / 20 * 8);

    let report = read_report(
        root,
        "decoded/decode-sweep.json",
        decode_sweep(root, "decoded", "valid", &profiles),
    );
    let source_digest = format!("{:x}", Sha256::digest(&iq));
    let mut translated = Vec::new();
    for attempt in report["attempts"].as_array().unwrap() {
        let path = root
            .join("decoded")
            .join(attempt["decode_receipt"].as_str().unwrap());
        let receipt: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(receipt["analyzed_iq_sha256"], source_digest);
        translated.push(receipt["channelized_iq_sha256"].clone());
    }
    assert_eq!(translated.len(), 2);
    assert_ne!(translated[0], translated[1]);
    assert_eq!(
        directory_names(&root.join("decoded")),
        ["attempt-0001", "attempt-0002", "decode-sweep.json"]
    );
}

#[test]
fn sweep_decode_publishes_verified_attempts_and_cleans_up_as_one_transaction() {
    const MINIMUM: usize = 17 * SWEEP_WINDOW_SAMPLES;
    const PREFIX: usize = MINIMUM + 1;
    const TRANSLATED: usize = PREFIX / (SWEEP_RATE / (4 * BANDWIDTH));
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let log = root.join("worker-inputs.log");
    fs::write(&log, []).unwrap();
    let assert_launches = |count| {
        let launches = fs::read_to_string(&log).unwrap();
        assert_eq!(launches.lines().count(), count, "{launches}");
        for path in launches.lines() {
            assert!(!root.join(path).exists(), "translated IQ remains at {path}");
        }
    };

    let saved = PREFIX.next_multiple_of(SWEEP_RATE / 1_000);
    let mut iq = vec![0; saved * IQ_BYTES];
    iq[PREFIX * IQ_BYTES] = 1;
    let centers = [SWEEP_CENTER_HZ + 7, SWEEP_CENTER_HZ - 7];
    let digest_first = format!("{:x}", Sha256::digest(&iq[..PREFIX * IQ_BYTES]));
    let first = archive_sweep(&root.join("pass-0001"), centers[0], &iq);
    // The final incomplete decimation group changes the raw hash only.
    iq[(PREFIX - 1) * IQ_BYTES] = 2;
    let digest_second = format!("{:x}", Sha256::digest(&iq[..PREFIX * IQ_BYTES]));
    let second = archive_sweep(&root.join("pass-0002"), centers[1], &iq);
    let manifest = write_receipt(
        &root.join("sweep-series.json"),
        &json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "dwells": 2,
            "iq_bytes": 2 * iq.len(),
            "passes": [
                {"directory": "pass-0001", "sweep_sha256": first.0},
                {"directory": "pass-0002", "sweep_sha256": second.0},
            ],
        }),
    );

    let prefix = PREFIX.to_string();
    let profiles = [
        "--series",
        "--profile",
        "923800000:125000:7:43",
        "--profile",
        "915300000:125000:8:43",
        "--profile",
        "915300000:125000:7:43",
        "--profile",
        "915300000:125000:7:43",
    ];
    let mut options = profiles.to_vec();
    options.extend(["--max-samples-per-dwell", &prefix]);
    let mut paired_options = options.clone();
    paired_options.extend(["--profile", "915300000:125000:7:44"]);
    let before_preview = directory_names(root);
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(["decode-lora-sweep", ".", "--plan-only"])
        .args(&paired_options)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let preview: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(preview["schema_version"], 1);
    assert_eq!(preview["operation"], "plan_lora_decode_sweep");
    assert_eq!(preview["source_capture_sha256"], manifest);
    assert_eq!(preview["attempt_count"], 4);
    assert_eq!(preview["profile_samples"], 4 * PREFIX);
    assert_eq!(preview["channelization_count"], 2);
    assert_eq!(preview["source_iq_bytes"], 2 * PREFIX * IQ_BYTES);
    assert_eq!(preview["peak_temporary_iq_bytes"], 8 * TRANSLATED);
    assert_launches(0);
    assert_eq!(directory_names(root), before_preview);

    install_worker(root);
    let output = "decoded-sweep";
    let result = decode_sweep(root, output, "valid", &options);
    let report = read_report(root, "decoded-sweep/decode-sweep.json", result);
    assert_launches(2);
    assert_eq!(
        directory_names(&root.join(output)),
        ["attempt-0001", "attempt-0002", "decode-sweep.json"]
    );
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "decode_lora_in_saved_sweeps");
    assert!(report.get("meshtastic_headers").is_none());
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["source_capture_sha256"], manifest);
    assert_eq!(report["sample_rate_hz"], SWEEP_RATE);
    assert_eq!(
        report["worker_sha256"],
        format!("{:x}", Sha256::digest(DECODER_WORKER))
    );
    for field in [
        "pass_count",
        "dwell_count",
        "attempt_count",
        "validated_frames",
    ] {
        assert_eq!(report[field], 2);
    }
    assert_eq!(report["omitted_valid_frames"], 0);
    assert_eq!(report["profile_samples"], 2 * PREFIX);

    let summaries = report["profiles"].as_array().unwrap();
    assert_eq!(summaries.len(), 3);
    for (index, (channel, sf, covered, short)) in [
        (CHANNEL_HZ, 7, 2, 0),
        (CHANNEL_HZ, 8, 2, 2),
        (923_800_000, 7, 0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        let profile = &summaries[index];
        assert_eq!(profile["channel_hz"], channel);
        assert_eq!(profile["bandwidth_hz"], BANDWIDTH);
        assert_eq!(profile["spreading_factor"], sf);
        assert_eq!(profile["sync_word"], 43);
        assert_eq!(profile["minimum_samples"], MINIMUM << (sf - 7));
        assert_eq!(profile["covered_dwells"], covered);
        assert_eq!(profile["short_dwells"], short);
        assert_eq!(profile["replayed_dwells"], covered - short);
    }

    let attempts = report["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    let translated_digest = format!("{:x}", Sha256::digest(vec![0; 8 * TRANSLATED]));
    for (index, (attempt, ((sweep, sidecar), raw_digest))) in attempts
        .iter()
        .zip([(first, digest_first), (second, digest_second)])
        .enumerate()
    {
        let attempt_name = format!("attempt-{:04}", index + 1);
        let pass_name = format!("pass-{:04}", index + 1);
        assert_eq!(attempt["directory"], attempt_name);
        assert_eq!(attempt["pass_index"], index + 1);
        assert_eq!(attempt["pass_directory"], pass_name);
        assert_eq!(attempt["tune_directory"], "tune-0001");
        assert_eq!(attempt["source_sweep_sha256"], sweep);
        assert_eq!(attempt["source_sidecar_sha256"], sidecar);
        assert_eq!(attempt["observed_center_hz"], centers[index]);
        assert_eq!(attempt["saved_samples"], saved);
        assert_eq!(attempt["selected_samples"], PREFIX);
        assert_eq!(attempt["validated_frames"], 1);
        assert!(attempt.get("meshtastic_headers").is_none());
        assert_eq!(
            attempt["decode_receipt"],
            format!("{attempt_name}/decode.json")
        );
        assert_eq!(
            attempt["worker_receipt"],
            format!("{attempt_name}/worker.json")
        );
        let path = root.join(output).join(attempt_name);
        assert_eq!(directory_names(&path), ["decode.json", "worker.json"]);
        let decode = fs::read(path.join("decode.json")).unwrap();
        let worker = fs::read(path.join("worker.json")).unwrap();
        assert_eq!(
            attempt["decode_receipt_sha256"],
            format!("{:x}", Sha256::digest(&decode))
        );
        assert_eq!(
            attempt["worker_receipt_sha256"],
            format!("{:x}", Sha256::digest(&worker))
        );
        let receipt: Value = serde_json::from_slice(&decode).unwrap();
        assert_eq!(receipt["operation"], "decode_lora_saved_iq");
        assert_eq!(receipt["start_sample"], 0);
        assert_eq!(receipt["analyzed_samples"], PREFIX);
        assert_eq!(receipt["analyzed_iq_sha256"], raw_digest);
        assert_eq!(receipt["channelized_sample_count"], TRANSLATED);
        assert_eq!(receipt["channelized_iq_sha256"], translated_digest);
        assert_eq!(
            receipt["worker_receipt_sha256"],
            attempt["worker_receipt_sha256"]
        );
        let worker: Value = serde_json::from_slice(&worker).unwrap();
        assert_eq!(worker["source_sha256"], translated_digest);
        assert_eq!(worker["validated_frames"], 1);
        assert_eq!(
            worker["records"][0]["payload_hex"],
            "4c6f526120706173736976652049512074657374"
        );
        assert_eq!(worker["records"][0]["payload_crc"], 7_689);
    }

    fs::write(&log, []).unwrap();
    let result = decode_sweep(root, "paired", "valid", &paired_options);
    let paired = read_report(root, "paired/decode-sweep.json", result);
    assert_eq!(paired["validated_frames"], 4);
    assert_eq!(
        directory_names(&root.join("paired")),
        [
            "attempt-0001",
            "attempt-0002",
            "attempt-0003",
            "attempt-0004",
            "decode-sweep.json"
        ]
    );
    assert_launches(4);
    let launches = fs::read_to_string(&log).unwrap();
    let routes: Vec<_> = launches.lines().collect();
    for paths in routes.as_chunks::<2>().0 {
        assert_eq!(paths[0], paths[1]);
    }
    let paired_attempts = paired["attempts"].as_array().unwrap();
    assert_eq!(paired_attempts.len(), 4);
    for (baseline, group) in attempts.iter().zip(paired_attempts.as_chunks::<2>().0) {
        verify_shared_packet_group(root, baseline, group, &translated_digest);
    }

    fs::write(&log, []).unwrap();
    let below_minimum = (MINIMUM - 1).to_string();
    let mut skip = profiles.to_vec();
    skip.extend(["--max-samples-per-dwell", &below_minimum]);
    let result = decode_sweep(root, "skipped", "valid", &skip);
    let skipped = read_report(root, "skipped/decode-sweep.json", result);
    assert_launches(0);
    assert_eq!(
        directory_names(&root.join("skipped")),
        ["decode-sweep.json"]
    );
    assert_eq!(skipped["attempt_count"], 0);
    assert_eq!(skipped["profile_samples"], 0);
    assert_eq!(skipped["profiles"][0]["short_dwells"], 2);
    assert_eq!(skipped["profiles"][2]["covered_dwells"], 0);
    assert!(skipped.get("worker_sha256").is_none());

    let baseline = directory_names(root);
    let rejected = decode_sweep(root, "bad-second", "second-bad-crc", &paired_options);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success());
    assert!(
        stderr.contains("LoRa worker payload CRC mismatch"),
        "{stderr}"
    );
    assert_launches(2);
    assert_eq!(directory_names(root), baseline);

    fs::write(&log, []).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(root.join("pass-0001/tune-0001/wideband.sc16q11"))
        .unwrap()
        .write_all(&2_048_i16.to_le_bytes())
        .unwrap();
    let over_budget = (2 * PREFIX - 1).to_string();
    for (flag, value) in [
        ("--max-attempts", "1"),
        ("--max-total-profile-samples", &over_budget),
    ] {
        let mut exceeded = options.clone();
        exceeded.extend([flag, value]);
        let refused = decode_sweep(root, "budget", "valid", &exceeded);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success());
        assert!(stderr.contains(flag), "{stderr}");
        assert_launches(0);
        assert_eq!(directory_names(root), baseline);
    }
}

fn verify_shared_packet_group(root: &Path, baseline: &Value, group: &[Value], translated: &str) {
    let mut raw_digest = None;
    for (attempt, sync) in group.iter().zip([43, 44]) {
        for field in [
            "pass_index",
            "pass_directory",
            "tune_directory",
            "source_sweep_sha256",
            "source_sidecar_sha256",
            "observed_center_hz",
            "saved_samples",
            "selected_samples",
            "channel_hz",
            "bandwidth_hz",
            "spreading_factor",
        ] {
            assert_eq!(attempt[field], baseline[field]);
        }
        assert_eq!(attempt["sync_word"], sync);
        assert_eq!(attempt["validated_frames"], 1);
        let directory = root
            .join("paired")
            .join(attempt["directory"].as_str().unwrap());
        assert_eq!(directory_names(&directory), ["decode.json", "worker.json"]);
        for (file, path_key, digest_key) in [
            ("decode.json", "decode_receipt", "decode_receipt_sha256"),
            ("worker.json", "worker_receipt", "worker_receipt_sha256"),
        ] {
            let bytes = fs::read(directory.join(file)).unwrap();
            assert_eq!(attempt[digest_key], format!("{:x}", Sha256::digest(&bytes)));
            if sync == 43 {
                let original = root
                    .join("decoded-sweep")
                    .join(baseline[path_key].as_str().unwrap());
                assert_eq!(bytes, fs::read(original).unwrap());
            }
            let receipt: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(receipt["sync_word"], sync);
            if file == "decode.json" {
                assert_eq!(receipt["channelized_iq_sha256"], translated);
                let digest = receipt["analyzed_iq_sha256"].as_str().unwrap();
                if let Some(previous) = &raw_digest {
                    assert_eq!(previous, digest);
                } else {
                    raw_digest = Some(digest.to_owned());
                }
            } else {
                assert_eq!(receipt["source_sha256"], translated);
            }
        }
    }
}
