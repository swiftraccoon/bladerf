//! Exercise sweep publication and replay with a device-free receiver fixture.

use std::f64::consts::TAU;
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BINARY: &str = env!("CARGO_BIN_EXE_rf-observatory");
const SWEEP_ARGS: &[&str] = &[
    "sweep",
    "--lower-hz",
    "902000000",
    "--upper-hz",
    "928000000",
    "--dwell-ms",
    "1",
];
const SERIES_ARGS: &[&str] = &[
    "sweep",
    "--center-hz",
    "910000000",
    "--center-hz",
    "918000000",
    "--dwell-ms",
    "1",
    "--passes",
    "3",
];
const RECEIVER: &str = r#"#!/bin/sh
set -eu
test "$1" = --workspace
workspace="$2"
test "$3" = capture-sweep
shift 3
echo "fixture started"
centers=""
visits=""
dwell_overrides=""
passes=1
check_continuity=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --check-continuity) check_continuity=1; shift; continue ;;
        --center-hz) centers="$centers $2" ;;
        --visit-hz) visits="$visits $2" ;;
        --dwell-at) dwell_overrides="$dwell_overrides $2" ;;
        --passes) passes="$2" ;;
        --channel) channel="$2" ;;
        --gain) gain="$2" ;;
        --duration) duration="$2" ;;
        --output) output="$2" ;;
        *) exit 8 ;;
    esac
    shift 2
done
test "$output" = "$workspace/scan"
test "$channel" = "$(cat "$workspace/receiver-channel")"
test "$gain" = 31.5
case "$duration" in
    0.001) default_samples=20000 ;;
    0.012) default_samples=240000 ;;
    *) exit 8 ;;
esac
if [ -n "$dwell_overrides" ]; then
    printf 'fixture centers%s\nfixture dwells%s\nfixture default duration %s\n' \
        "$centers" "$dwell_overrides" "$duration"
fi
if [ -n "$visits" ]; then
    printf 'fixture visits%s\n' "$visits"
fi
visit_order="${visits:-$centers}"
mode=$(cat "$workspace/receiver-mode")
if [ "$check_continuity" -eq 1 ]; then
    test "$channel" = 1
    echo "fixture received --check-continuity"
fi
case "$mode" in
    checked|invalid-sample-timing|wrong-fpga-version) test "$check_continuity" -eq 1 ;;
esac
pass=1
while [ "$pass" -le "$passes" ]; do
    batch="$output"
    if [ "$passes" -gt 1 ]; then
        batch="$output/$(printf 'pass-%04d' "$pass")"
        mkdir -p "$batch"
    fi
    waveform="$workspace/waveform.sc16q11"
    readback_offset=7
    if [ "$mode" = series-activity ]; then
        waveform="$workspace/$(printf 'waveform-pass-%04d.sc16q11' "$pass")"
        readback_offset=$((7 - 5 * (pass - 1)))
    fi
    tune=0
    for center in $visit_order; do
        tune=$((tune + 1))
        samples="$default_samples"
        for dwell in $dwell_overrides; do
            if [ "${dwell%:*}" = "$center" ]; then
                samples=$((20000 * ${dwell#*:}))
                break
            fi
        done
        tune_name=$(printf 'tune-%04d' "$tune")
        directory="$batch/$tune_name"
        if [ "$tune" -eq 2 ]; then
            case "$mode:$pass" in
                failed:1|failed-second-pass:2)
                    mkdir "$directory"
                    echo "fixture failed at second tune" >&2
                    exit 9
                    ;;
                incomplete:1) exit 0 ;;
            esac
        fi
        mkdir "$directory"
        iq="$directory/wideband.sc16q11"
        selected="$workspace/waveform-$tune_name.sc16q11"
        if [ "$tune" -eq 1 ] && [ ! -f "$selected" ]; then
            selected="$waveform"
        fi
        if [ -f "$selected" ]; then
            cp "$selected" "$iq.tmp"
        else
            if [ "$passes" -gt 1 ]; then
                # Put both indices in the first Q11 pair before adding zeros.
                printf '%b' "$(printf '\\0%03o\\000\\0%03o\\000' "$pass" "$tune")" > "$iq.tmp"
            fi
            truncate -s "$((samples * 4))" "$iq.tmp"
        fi
        mv "$iq.tmp" "$iq"
        {
            cat <<EOF
{
  "format": "SC16_Q11",
  "dtype": "<i2",
  "interleave": "I,Q",
  "complex_scale": 2048,
  "frequency_hz": $((center + readback_offset)),
  "sample_rate_hz": 20000000,
  "bandwidth_hz": 20000000,
  "gain_db": 31.5,
  "gain_mode": "manual",
  "bias_tee_enabled": false,
  "channel": "RX$channel",
  "num_samples": $samples,
  "started_utc": "2000-01-01T00:00:00Z"
EOF
            case "$mode" in
                checked|invalid-sample-timing|wrong-fpga-version)
                    first_ns=$((50000000 * (pass - 1) + 20000000 * (tune - 1)))
                    end_ns=$((first_ns + samples * 50))
                    if [ "$mode" = invalid-sample-timing ]; then
                        end_ns=$((end_ns + 50))
                    fi
                    fpga_version=0.16.0
                    if [ "$mode" = wrong-fpga-version ]; then
                        fpga_version=0.15.0
                    fi
                    printf ',"fpga_version":"%s","required_fpga_version":"0.16.0"\n' \
                        "$fpga_version"
                    printf ',"sample_timing":{"first_sample_ns":%s,' "$first_ns"
                    printf '"end_sample_exclusive_ns":%s,"witness_samples":2048}\n' "$end_ns"
                    ;;
            esac
            printf '}\n'
        } > "$iq.json.tmp"
        mv "$iq.json.tmp" "$iq.json"
    done
    pass=$((pass + 1))
done
timing="$output/acquisition-timing.json"
case "$mode" in
    checked|timed|misordered-timing|series-activity)
        {
            printf '%s\n' '{"schema_version":1,"operation":"native_rf_sweep_timing",'
            if [ "$passes" -gt 1 ]; then
                printf '"passes":%s,\n' "$passes"
            fi
            dwells=$((tune * passes))
            last_start=$((500000 + (dwells - 1) * (dwells + 2) * 500000))
            if [ -n "$visits" ]; then
                last_start=$((500000 + (dwells - 1) * 2000000))
            fi
            printf '"device_ready_ns":200000,"device_closed_ns":%s,"dwells":[\n' \
                "$((last_start + 1900000))"
            start=500000
            spacing=2000000
            comma=""
            pass=1
            while [ "$pass" -le "$passes" ]; do
                for center in $visit_order; do
                    read_end=$((start + 1400000))
                    if [ "$mode" = misordered-timing ]; then
                        read_end=$((start + 350000))
                    fi
                    printf '%s{"requested_center_hz":%s,"started_ns":%s,"tuned_ns":%s,' \
                        "$comma" "$center" "$start" "$((start + 100000))"
                    printf '"files_ready_ns":%s,"stream_ready_ns":%s,"read_started_ns":%s,' \
                        "$((start + 200000))" "$((start + 300000))" "$((start + 400000))"
                    printf '"read_finished_ns":%s,"stream_closed_ns":%s,"published_ns":%s}\n' \
                        "$read_end" "$((start + 1550000))" "$((start + 1700000))"
                    start=$((start + spacing))
                    if [ -z "$visits" ]; then
                        spacing=$((spacing + 1000000))
                    fi
                    comma=,
                done
                pass=$((pass + 1))
            done
            printf '%s\n' ']}'
        } > "$timing.tmp"
        mv "$timing.tmp" "$timing"
        if [ "$passes" -gt 1 ]; then
            # Keep the simulated timestamp span inside the measured subprocess time.
            sleep 0.1
        fi
        ;;
    malformed-timing)
        printf '{' > "$timing"
        ;;
esac
"#;

enum ReceiverMode {
    Complete,
    Checked,
    InvalidSampleTiming,
    WrongFpgaVersion,
    Failed,
    Incomplete,
    Timed,
    MisorderedTiming,
    MalformedTiming,
    FailedSecondPass,
    SeriesActivity,
}

fn run_sweep(workspace: &Path, mode: ReceiverMode, arguments: &[&str]) -> Output {
    run_sweep_on_antenna(workspace, mode, arguments, 2)
}

fn run_sweep_on_antenna(
    workspace: &Path,
    mode: ReceiverMode,
    arguments: &[&str],
    antenna: u8,
) -> Output {
    fs::write(workspace.join("Cargo.toml"), "").unwrap();
    fs::create_dir_all(workspace.join("repos/bladeRF")).unwrap();
    let receiver = workspace.join("receiver");
    fs::write(&receiver, RECEIVER).unwrap();
    fs::set_permissions(&receiver, fs::Permissions::from_mode(0o700)).unwrap();
    let mode = match mode {
        ReceiverMode::Complete => "complete",
        ReceiverMode::Checked => "checked",
        ReceiverMode::InvalidSampleTiming => "invalid-sample-timing",
        ReceiverMode::WrongFpgaVersion => "wrong-fpga-version",
        ReceiverMode::Failed => "failed",
        ReceiverMode::Incomplete => "incomplete",
        ReceiverMode::Timed => "timed",
        ReceiverMode::MisorderedTiming => "misordered-timing",
        ReceiverMode::MalformedTiming => "malformed-timing",
        ReceiverMode::FailedSecondPass => "failed-second-pass",
        ReceiverMode::SeriesActivity => "series-activity",
    };
    fs::write(workspace.join("receiver-mode"), mode).unwrap();
    let antenna = antenna.to_string();
    fs::write(workspace.join("receiver-channel"), &antenna).unwrap();
    Command::new(BINARY)
        .current_dir(workspace)
        .arg("--workspace")
        .arg(workspace)
        .args(arguments)
        .args([
            "--antenna",
            &antenna,
            "--gain",
            "31.5",
            "--output",
            "scan",
            "--receiver",
        ])
        .arg(receiver)
        .output()
        .unwrap()
}

fn session_log(directory: &Path) -> String {
    let log = fs::read_to_string(directory.join("acquisition.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| *line == "fixture started")
            .count(),
        1
    );
    log
}

fn read_json(path: &Path) -> Value {
    let file = File::open(path).unwrap_or_else(|error| panic!("open {}: {error}", path.display()));
    serde_json::from_reader(file)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

#[test]
fn spectrum_output_modes_preserve_capture_evidence_across_passes() {
    for passes in ["1", "2"] {
        let roots = ["full", "summary", "skip"].map(|mode| {
            let workspace = tempfile::tempdir().unwrap();
            let mut args = SWEEP_ARGS.to_vec();
            args.extend(["--passes", passes, "--spectrum-output", mode]);
            let output = run_sweep(workspace.path(), ReceiverMode::Timed, &args);
            assert!(
                output.status.success(),
                "{mode} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            workspace
        });
        let scans = roots.each_ref().map(|root| root.path().join("scan"));
        let plans = scans
            .each_ref()
            .map(|scan| read_json(&scan.join("plan.json")));
        assert_eq!(plans[0], plans[1]);
        assert_eq!(plans[0], plans[2]);

        if passes == "1" {
            compare_spectrum_modes(&scans);
        } else {
            for (scan, mode) in scans
                .iter()
                .zip([Value::Null, "summary".into(), "skip".into()])
            {
                assert_eq!(
                    read_json(&scan.join("sweep-series.json"))["spectrum_output"],
                    mode
                );
            }
            for pass in ["pass-0001", "pass-0002"] {
                let directories = scans.each_ref().map(|scan| scan.join(pass));
                compare_spectrum_modes(&directories);
            }
            let output = triage_series(roots[2].path(), "triage", &[]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

fn compare_spectrum_modes(directories: &[PathBuf; 3]) {
    let [mut full, summary, skip] = directories
        .each_ref()
        .map(|dir| read_json(&dir.join("sweep.json")));
    assert!(full.get("spectrum_output").is_none());
    assert_eq!(summary["spectrum_output"], "summary");
    assert_eq!(skip["spectrum_output"], "skip");
    assert_eq!(full["tunes"], summary["tunes"]);

    for tune in full["tunes"].as_array_mut().unwrap() {
        let name = tune["directory"].as_str().unwrap();
        let [full, summary, skip] = directories.each_ref().map(|path| path.join(name));
        assert!(full.join("spectrum.json").is_file());
        assert!(!summary.join("spectrum.json").exists());
        assert!(!skip.join("spectrum.json").exists());
        for file in [
            "wideband.sc16q11",
            "wideband.sc16q11.json",
            "capture.json",
            "request.json",
        ] {
            let expected = fs::read(full.join(file)).unwrap();
            assert_eq!(fs::read(summary.join(file)).unwrap(), expected);
            assert_eq!(fs::read(skip.join(file)).unwrap(), expected);
        }
        let tune = tune.as_object_mut().unwrap();
        assert!(tune.remove("spectrum_windows").is_some());
        assert!(tune.remove("activity_regions").is_some());
    }
    assert_eq!(full["tunes"], skip["tunes"]);
}

fn assert_validated_tune(directory: &Path, requested_center: u64, antenna: u8) {
    let readback = requested_center + 7;
    assert_eq!(
        fs::metadata(directory.join("wideband.sc16q11"))
            .unwrap()
            .len(),
        80_000
    );
    assert!(directory.join("wideband.sc16q11.json").is_file());
    let request = read_json(&directory.join("request.json"));
    assert_eq!(request["center_frequency_hz"], requested_center);
    assert_eq!(request["antenna_requested"], antenna);
    assert_eq!(request["gain_db_requested"], 31.5);
    let capture = read_json(&directory.join("capture.json"));
    assert_eq!(capture["frequency_hz"].as_f64(), Some(readback as f64));
    assert_eq!(capture["channel"], format!("RX{antenna}"));
    assert_eq!(capture["num_samples"], 20_000);
    let spectrum = read_json(&directory.join("spectrum.json"));
    assert_eq!(spectrum["operation"], "offline_rf_spectrum_survey");
    assert_eq!(spectrum["center_frequency_hz"], readback);
}

#[test]
fn plan_only_succeeds_outside_a_workspace_without_writing_files() {
    let directory = tempfile::tempdir().unwrap();
    for check_continuity in [false, true] {
        let mut command = Command::new(BINARY);
        command
            .current_dir(directory.path())
            .args(SWEEP_ARGS)
            .arg("--plan-only");
        if check_continuity {
            command.arg("--check-continuity");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(plan["centers_hz"].as_array().unwrap().len() > 1);
        assert_eq!(
            plan.get("check_continuity"),
            check_continuity.then_some(&Value::Bool(true))
        );
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn signal_profile_binds_capture_and_replay_with_matching_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let profile = b"{\n  \"schema_version\": 1, \"adsb\": true\n}\n";
    fs::write(workspace.join("signals.json"), profile).unwrap();
    let profile_sha256 = format!("{:x}", Sha256::digest(profile));
    let sweep = [
        "sweep",
        "--signal-profile",
        "signals.json",
        "--dwell-ms",
        "1",
        "--max-iq-bytes",
        "80000",
        "--spectrum-output",
        "skip",
    ];
    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(sweep)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["signal_profile_sha256"], profile_sha256);
    assert_eq!(plan["planned_iq_bytes"], 80_000);
    assert_eq!(fs::read_dir(workspace).unwrap().count(), 1);

    let capture = run_sweep_on_antenna(workspace, ReceiverMode::Complete, &sweep, 1);
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let scan = workspace.join("scan");
    let log = session_log(&scan);
    assert_eq!(read_json(&scan.join("plan.json")), plan);
    let receipt = read_json(&scan.join("sweep.json"));
    assert_eq!(receipt["signal_profile_sha256"], profile_sha256);
    assert_eq!(receipt["iq_bytes"], 80_000);
    let tune = &receipt["tunes"][0];
    let center = plan["centers_hz"][0].as_u64().unwrap();
    assert_eq!(tune["requested_center_hz"], center);
    assert_eq!(tune["observed_center_hz"], center + 7);
    let sidebands = tune["observed_bands"].as_array().unwrap();
    assert!(sidebands.iter().any(|band| {
        band["lower_hz"].as_u64().unwrap() <= 1_088_700_000
            && band["upper_hz"].as_u64().unwrap() >= 1_091_300_000
    }));
    let request = read_json(&scan.join("tune-0001/request.json"));
    assert_eq!(request["antenna_requested"], 1);
    assert_eq!(request["gain_db_requested"], 31.5);
    assert_eq!(request["samples_requested"], 20_000);

    let replay = ["replay-sweep", "scan", "--signal-profile", "signals.json"];
    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(replay)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    let receipt_sha256 = format!(
        "{:x}",
        Sha256::digest(fs::read(scan.join("sweep.json")).unwrap())
    );
    assert_eq!(preview["signal_profile_sha256"], profile_sha256);
    assert_eq!(preview["source_capture_sha256"], receipt_sha256);
    assert_eq!(preview["total_replayed_iq_samples"], 20_000);
    let analyzers = preview["analyzers"].as_array().unwrap();
    assert_eq!(analyzers.len(), 1);
    assert_eq!(analyzers[0]["analyzer"], "adsb");
    assert_eq!(analyzers[0]["eligible_attempts"], 1);
    assert_eq!(analyzers[0]["short_attempts"], 0);

    let result = Command::new(BINARY)
        .current_dir(workspace)
        .args(replay)
        .args(["--output", "profile-replay.json"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = read_json(&workspace.join("profile-replay.json"));
    assert_eq!(report["signal_profile_sha256"], profile_sha256);
    assert_eq!(report["source_capture_sha256"], receipt_sha256);
    assert_eq!(report["adsb"]["covered_dwells"], 1);
    assert_eq!(report["adsb"]["replayed_dwells"], 1);
    assert_eq!(report["adsb"]["replayed_iq_samples"], 20_000);
    let explicit = Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "replay-sweep",
            "scan",
            "--adsb",
            "--output",
            "explicit.json",
        ])
        .output()
        .unwrap();
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert_eq!(
        report["adsb"],
        read_json(&workspace.join("explicit.json"))["adsb"]
    );
    assert_eq!(session_log(&scan), log);
}

#[test]
fn signal_profile_replay_publishes_shared_series_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let profile = br#"{"schema_version":1,"adsb":true,"lora_profile":["1090000000:500000:7"]}"#;
    fs::write(workspace.join("signals.json"), profile).unwrap();
    let limits = [
        "--max-samples-per-dwell",
        "35000",
        "--max-total-samples",
        "70000",
        "--max-total-profile-samples",
        "70000",
        "--max-total-channelized-samples",
        "1",
        "--max-total-station-samples",
        "1",
    ];
    let mut sweep = vec![
        "sweep",
        "--signal-profile",
        "signals.json",
        "--replay",
        "--dwell-ms",
        "1",
        "--passes",
        "2",
        "--spectrum-output",
        "skip",
        "--max-iq-bytes",
        "320000",
    ];
    sweep.extend(limits);
    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(&sweep)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(
        preview["replay_preflight"]["center_readback_tolerance_hz"],
        10
    );
    assert_eq!(preview["planned_iq_bytes"], 320_000);
    assert_eq!(fs::read_dir(workspace).unwrap().count(), 1);

    let captured = run_sweep_on_antenna(workspace, ReceiverMode::Timed, &sweep, 1);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let scan = workspace.join("scan");
    let log = session_log(&scan);
    let source = fs::read(scan.join("sweep-series.json")).unwrap();
    let report = read_json(&scan.join("replay.json"));
    assert_eq!(
        report["signal_profile_sha256"],
        format!("{:x}", Sha256::digest(profile))
    );
    assert_eq!(
        report["source_capture_sha256"],
        format!("{:x}", Sha256::digest(source))
    );
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["pass_count"], 2);
    assert_eq!(report["dwell_count"], 2);
    assert_eq!(report["shared_iq"]["shared_dwells"], 2);
    assert_eq!(report["shared_iq"]["source_iq_bytes"], 280_000);
    for (index, pass) in ["pass-0001", "pass-0002"].into_iter().enumerate() {
        let bytes = fs::read(scan.join(pass).join("tune-0001/wideband.sc16q11")).unwrap();
        let prefix_digest = format!("{:x}", Sha256::digest(&bytes[..140_000]));
        for analyzer in ["adsb", "lora"] {
            let dwell = &report[analyzer]["dwells"][index];
            assert_eq!(dwell["pass_directory"], pass);
            assert_eq!(dwell["selected_samples"], 35_000);
            assert_eq!(dwell["replay"]["analyzed_iq_sha256"], prefix_digest);
        }
    }

    let replay = Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "replay-sweep",
            "scan",
            "--series",
            "--signal-profile",
            "signals.json",
            "--share-iq",
            "--output",
            "standalone.json",
        ])
        .args(limits)
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(report, read_json(&workspace.join("standalone.json")));
    assert_eq!(session_log(&scan), log);
}

#[test]
fn signal_profile_replay_rejects_small_budget_and_preserves_failed_replay_source() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    fs::write(
        workspace.join("signals.json"),
        br#"{"schema_version":1,"adsb":true}"#,
    )
    .unwrap();
    let sweep = [
        "sweep",
        "--signal-profile",
        "signals.json",
        "--replay",
        "--dwell-ms",
        "1",
        "--spectrum-output",
        "skip",
    ];
    let scan = workspace.join("scan");
    let mut over_budget = sweep.to_vec();
    over_budget.extend(["--max-total-samples", "19999"]);
    let rejected = run_sweep(workspace, ReceiverMode::Complete, &over_budget);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success());
    assert!(
        stderr.contains("adsb") && stderr.contains("19999"),
        "{stderr}"
    );
    assert!(!scan.exists());

    let mut bad_iq = vec![0; 80_000];
    bad_iq[..2].copy_from_slice(&2048_i16.to_le_bytes());
    fs::write(workspace.join("waveform.sc16q11"), &bad_iq).unwrap();
    let failed = run_sweep(workspace, ReceiverMode::Complete, &sweep);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success());
    assert!(stderr.contains("outside SC16 Q11"), "{stderr}");
    session_log(&scan);
    let receipt = read_json(&scan.join("sweep.json"));
    assert_eq!(receipt["spectrum_output"], "skip");
    assert_eq!(receipt["iq_bytes"], 80_000);
    assert_eq!(
        fs::read(scan.join("tune-0001/wideband.sc16q11")).unwrap(),
        bad_iq
    );
    assert!(!scan.join("replay.json").exists());
}

#[test]
fn packet_signal_profile_checks_limits_and_decodes_the_completed_capture() {
    const WORKER: &str = r#"#!/bin/sh
set -eu
test "$1" = decode
test "$#" -eq 14
size=$(wc -c < "$2")
digest=$(sha256sum < "$2")
digest=${digest%% *}
shift 2
test "$1 $3 $5 $7 $9 ${11}" = '--channel-hz --bandwidth-hz --spreading-factor --sync-word --max-frames --output'
cat > "${12}" <<EOF
{
  "schema_version": 1,
  "operation": "decode_lora_cf32",
  "source_format": "CF32_LE",
  "source_sample_count": $((size / 8)),
  "source_sha256": "$digest",
  "channel_hz": $2,
  "bandwidth_hz": $4,
  "sample_rate_hz": $((4 * $4)),
  "spreading_factor": $6,
  "sync_word": $8,
  "expected_sync_symbols": [$((($8 / 16) * 8)), $((($8 % 16) * 8))],
  "max_frames": ${10},
  "validated_frames": 0,
  "omitted_valid_frames": 0,
  "rejected_crc_absent": 0,
  "rejected_invalid_crc": 0,
  "rejected_wrong_sync": 0,
  "records": []
}
EOF
"#;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let profile = br#"{
        "schema_version":1,
        "lora_packet_profile":["904600000:500000:8:43","904600000:500000:8:52"],
        "lorawan_join":true
    }"#;
    fs::write(workspace.join("signals.json"), profile).unwrap();
    let profile_sha256 = format!("{:x}", Sha256::digest(profile));
    let sweep = [
        "sweep",
        "--signal-profile",
        "signals.json",
        "--replay",
        "--dwell-ms",
        "12",
        "--spectrum-output",
        "skip",
        "--lora-worker",
        "lora-fixture-worker",
    ];
    let work_limits = [
        "--lora-max-attempts",
        "2",
        "--max-total-profile-samples",
        "1160000",
    ];
    let scan = workspace.join("scan");
    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(sweep)
        .args(work_limits)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["signal_profile_sha256"], profile_sha256);
    assert_eq!(plan["planned_iq_bytes"], 2_320_000);
    assert!(plan.get("replay_preflight").is_none());
    let work = &plan["packet_decode_preflight"];
    assert_eq!(work["lorawan_join"], true);
    assert!(work.get("meshtastic").is_none());
    assert_eq!(work["maximum_attempts"], 2);
    assert_eq!(work["maximum_profile_samples"], 1_160_000);
    assert_eq!(work["maximum_source_iq_bytes"], 2_320_000);
    assert_eq!(work["peak_temporary_iq_bytes"], 464_000);
    assert_eq!(fs::read_dir(workspace).unwrap().count(), 1);

    for (limits, diagnostic) in [
        (&["--lora-max-attempts", "1"][..], "--lora-max-attempts"),
        (
            &["--max-total-profile-samples", "1159999"],
            "--max-total-profile-samples",
        ),
        (&[], "lora-fixture-worker"),
    ] {
        let mut args = sweep.to_vec();
        args.extend_from_slice(limits);
        let failed = run_sweep_on_antenna(workspace, ReceiverMode::Complete, &args, 1);
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(!failed.status.success());
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!scan.exists());
    }

    let worker = workspace.join("lora-fixture-worker");
    fs::write(&worker, WORKER).unwrap();
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o700)).unwrap();
    let mut args = sweep.to_vec();
    args.extend(work_limits);
    let captured = run_sweep_on_antenna(workspace, ReceiverMode::Complete, &args, 1);
    assert!(
        captured.status.success(),
        "{}",
        String::from_utf8_lossy(&captured.stderr)
    );
    session_log(&scan);
    assert_eq!(read_json(&scan.join("plan.json")), plan);
    let source = fs::read(scan.join("sweep.json")).unwrap();
    let capture: Value = serde_json::from_slice(&source).unwrap();
    assert_eq!(capture["signal_profile_sha256"], profile_sha256);
    let packets = scan.join("lora-packets");
    let report = read_json(&packets.join("decode-sweep.json"));
    assert_eq!(report["signal_profile_sha256"], profile_sha256);
    assert_eq!(
        report["source_capture_sha256"],
        format!("{:x}", Sha256::digest(source))
    );
    assert_eq!(report["attempt_count"], 2);
    assert_eq!(report["profile_samples"], 1_160_000);
    assert_eq!(report["validated_frames"], 0);
    assert_eq!(report["lorawan_join_requests"], 0);
    assert!(!scan.join("replay.json").exists());
    let iq = fs::read(scan.join("tune-0001/wideband.sc16q11")).unwrap();
    let iq_sha256 = format!("{:x}", Sha256::digest(iq));
    let attempts = report["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    for (attempt, sync_word) in attempts.iter().zip([43, 52]) {
        let receipt = read_json(&packets.join(attempt["decode_receipt"].as_str().unwrap()));
        assert_eq!(attempt["sync_word"], sync_word);
        assert_eq!(receipt["sync_word"], sync_word);
        assert_eq!(receipt["analyzed_iq_sha256"], iq_sha256);
        assert_eq!(receipt["channelized_sample_count"], 58_000);
    }
    assert!(attempts[0].get("lorawan_join_requests").is_none());
    assert_eq!(attempts[1]["lorawan_join_requests"], 0);
}

#[test]
fn rs41_raster_plans_guarded_work_and_rejects_budget_before_reading_iq() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let capture = run_sweep(
        workspace,
        ReceiverMode::Complete,
        &[
            "sweep",
            "--center-hz",
            "402000000",
            "--center-hz",
            "404000000",
            "--center-hz",
            "405000000",
            "--dwell-ms",
            "1",
            "--dwell-at",
            "405000000:535",
            "--spectrum-output",
            "skip",
        ],
    );
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    // A replay would reject this sample. The plan and budget depend on metadata.
    File::options()
        .write(true)
        .open(workspace.join("scan/tune-0003/wideband.sc16q11"))
        .unwrap()
        .write_all_at(&i16::MAX.to_le_bytes(), 0)
        .unwrap();

    let raster = |options: &[&str]| {
        Command::new(BINARY)
            .arg("--workspace")
            .arg(workspace)
            .args([
                "identify-rs41-raster-sweep",
                "scan",
                "--lower-hz",
                "401987500",
                "--upper-hz",
                "402012500",
            ])
            .args(options)
            .output()
            .unwrap()
    };
    let output = raster(&["--plan-only", "--max-channelized-samples", "80250"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["dwell_count"], 3);
    assert_eq!(plan["covered_dwells"], 2);
    assert_eq!(plan["short_dwells"], 1);
    assert_eq!(plan["replayed_dwells"], 1);
    assert_eq!(plan["requested_channels"], 3);
    assert_eq!(plan["covered_channels"], 3);
    assert_eq!(plan["replayed_channels"], 3);
    assert_eq!(plan["replayed_iq_samples"], 10_700_000);
    assert_eq!(plan["source_iq_bytes"], 42_800_000);
    assert_eq!(plan["channelized_samples"], 80_250);
    assert_eq!(plan["dwells"][0]["tune_directory"], "tune-0002");
    assert_eq!(plan["dwells"][0]["status"], "short_dwell");
    assert_eq!(plan["dwells"][1]["tune_directory"], "tune-0003");
    assert_eq!(plan["dwells"][1]["status"], "scheduled");
    assert_eq!(plan["channels"][1]["channel_frequency_hz"], 402_000_000);
    assert_eq!(plan["channels"][1]["covered_dwells"], 2);
    assert_eq!(plan["channels"][1]["replayed_dwells"], 1);

    let output = raster(&[
        "--output",
        "raster.json",
        "--max-channelized-samples",
        "80249",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--max-channelized-samples"), "{stderr}");
    assert!(!workspace.join("raster.json").exists());
}

#[test]
fn signal_profile_plans_per_band_minimum_dwells() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("signals.json");
    fs::write(
        &profile,
        br#"{"schema_version":1,"adsb":true,"rds_channel_hz":[93300000],"rs41_channel_hz":404200000}"#,
    )
    .unwrap();
    let output = Command::new(BINARY)
        .current_dir(directory.path())
        .args(["sweep", "--signal-profile"])
        .arg(profile.file_name().unwrap())
        .args(["--dwell-ms", "1", "--plan-only"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let centers = plan["centers_hz"].as_array().unwrap();
    let dwell_ms = |center: u64| {
        plan["dwell_ms_by_center"][center.to_string()]
            .as_u64()
            .unwrap_or(plan["dwell_ms"].as_u64().unwrap())
    };
    let centers: Vec<_> = centers
        .iter()
        .map(|center| center.as_u64().unwrap())
        .collect();
    assert!(
        centers
            .iter()
            .any(|&center| { center.abs_diff(93_300_000) <= 7_900_000 && dwell_ms(center) >= 88 })
    );
    assert!(
        centers.iter().any(|&center| {
            center.abs_diff(1_090_000_000) <= 6_700_000 && dwell_ms(center) == 1
        })
    );
    assert!(centers.iter().any(|&center| {
        (112_000..=7_988_000).contains(&center.abs_diff(404_200_000)) && dwell_ms(center) == 900
    }));
    let milliseconds: u64 = centers.into_iter().map(dwell_ms).sum();
    assert_eq!(plan["planned_iq_bytes"], milliseconds * 80_000);
}

#[test]
fn signal_profile_rejects_bad_schema_and_mixed_selectors_before_output() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    fs::write(
        workspace.join("signals.json"),
        br#"{"schema_version":1,"adsb":true}"#,
    )
    .unwrap();
    fs::write(
        workspace.join("unsupported.json"),
        br#"{"schema_version":2,"adsb":true}"#,
    )
    .unwrap();
    for arguments in [
        &[
            "sweep",
            "--signal-profile",
            "unsupported.json",
            "--plan-only",
        ][..],
        &[
            "sweep",
            "--signal-profile",
            "signals.json",
            "--center-hz",
            "1090000000",
            "--plan-only",
        ],
        &[
            "replay-sweep",
            "missing-sweep",
            "--signal-profile",
            "signals.json",
            "--adsb",
            "--plan-only",
        ],
        &[
            "sweep",
            "--center-hz",
            "1090000000",
            "--replay",
            "--plan-only",
        ],
    ] {
        let output = Command::new(BINARY)
            .current_dir(workspace)
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("profile"), "{arguments:?}: {stderr}");
    }
    assert_eq!(fs::read_dir(workspace).unwrap().count(), 2);
}

#[test]
fn completed_sweep_publishes_all_tunes_and_the_terminal_report() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_sweep(directory.path(), ReceiverMode::Complete, SWEEP_ARGS);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = directory.path().join("scan");
    assert!(!session_log(&destination).contains("--check-continuity"));
    let plan = read_json(&destination.join("plan.json"));
    let centers = plan["centers_hz"].as_array().unwrap();
    assert!(centers.len() > 1);
    let report = read_json(&destination.join("sweep.json"));
    assert_eq!(report["operation"], "sequential_rf_sweep");
    assert!(report.get("check_continuity").is_none());
    assert!(report["acquisition_elapsed_seconds"].as_f64().unwrap() > 0.0);
    assert!(report["analysis_elapsed_seconds"].as_f64().unwrap() > 0.0);
    assert_eq!(
        report["capture_cadence"]["nominal_iq_seconds"],
        centers.len() as f64 / 1_000.0
    );
    assert!(report["capture_cadence"].get("native").is_none());
    assert!(!destination.join("acquisition-timing.json").exists());
    assert!(!destination.join("sweep-series.json").exists());
    assert!(!destination.join("pass-0001").exists());
    let tunes = report["tunes"].as_array().unwrap();
    assert_eq!(tunes.len(), centers.len());
    for (index, (tune, center)) in tunes.iter().zip(centers).enumerate() {
        let center = center.as_u64().unwrap();
        let name = format!("tune-{:04}", index + 1);
        assert_eq!(tune["directory"], name);
        assert_eq!(tune["observed_center_hz"], center + 7);
        assert_validated_tune(&destination.join(name), center, 2);
    }
    let coverage = sweep_coverage(directory.path(), "scan", "map.json", &["--frequency-map"]);
    assert!(
        coverage.status.success(),
        "{}",
        String::from_utf8_lossy(&coverage.stderr)
    );
    let coverage = read_json(&directory.path().join("map.json"));
    let map = &coverage["frequency_map"];
    assert_eq!(map["lower_hz"], 902_000_000);
    assert_eq!(map["upper_hz"], 928_000_000);
    assert_eq!(map["covered_bandwidth_hz"], 26_000_000);
    assert_eq!(map["qualifying_bandwidth_hz"], 26_000_000);
    assert_eq!(coverage["bands"], json!([]));
}

#[test]
fn completed_timed_sweep_binds_native_timing_to_its_terminal_report() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_sweep(
        directory.path(),
        ReceiverMode::Timed,
        &[
            "sweep",
            "--center-hz",
            "902000000",
            "--center-hz",
            "919000000",
            "--center-hz",
            "927000000",
            "--dwell-ms",
            "1",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = directory.path().join("scan");
    let timing = fs::read(destination.join("acquisition-timing.json")).unwrap();
    let report = read_json(&destination.join("sweep.json"));
    let cadence = &report["capture_cadence"];
    assert_eq!(cadence["nominal_iq_seconds"], 0.003);
    let native = &cadence["native"];
    assert_eq!(native["sha256"], format!("{:x}", Sha256::digest(timing)));
    assert_eq!(native["native_elapsed_ns"], 7_400_000);
    assert_eq!(native["device_open_setup_ns"], 200_000);
    assert_eq!(native["before_first_dwell_ns"], 300_000);
    assert_eq!(native["after_last_publish_ns"], 200_000);
    assert_eq!(
        native["host_read_start_spacing_ns"],
        json!({"min": 2_000_000, "median": 2_500_000, "max": 3_000_000})
    );
    assert_eq!(
        native["phase_totals_ns"],
        json!({
            "retune_and_gain": 300_000,
            "file_preparation": 300_000,
            "stream_setup_and_pre_activation_readback": 300_000,
            "activation_and_final_readback": 300_000,
            "read_and_write": 3_000_000,
            "stream_shutdown": 450_000,
            "publication": 450_000,
            "between_dwells": 1_600_000,
        })
    );

    let output = sweep_coverage(
        directory.path(),
        "scan",
        "coverage.json",
        &[
            "--band",
            "900000000:900100000",
            "--band",
            "930000000:930100000",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&directory.path().join("coverage.json"));
    let bands = report["bands"].as_array().unwrap();
    assert_eq!(
        bands[0]["qualifying_host_read_gaps"],
        json!({"leading_ns": 0, "trailing_ns": 5_000_000, "longest_ns": 5_000_000})
    );
    assert_eq!(
        bands[1]["qualifying_host_read_gaps"],
        json!({"leading_ns": 5_000_000, "trailing_ns": 0, "longest_ns": 5_000_000})
    );
}

fn sweep_coverage(workspace: &Path, capture: &str, output: &str, arguments: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(workspace)
        .args(["sweep-coverage", capture, "--output", output])
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn checked_sweep_rejects_rx2_before_starting_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let mut arguments = SWEEP_ARGS.to_vec();
    arguments.push("--check-continuity");
    let output = run_sweep(directory.path(), ReceiverMode::Complete, &arguments);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("--check-continuity") && stderr.contains("RX1"),
        "{stderr}"
    );
    assert!(!directory.path().join("scan").exists());
}

#[test]
#[cfg_attr(
    not(all(target_os = "linux", target_env = "gnu")),
    ignore = "checked capture requires Linux GNU"
)]
fn checked_sweep_requires_valid_proof_from_a_successful_receiver() {
    for (mode, expected_error) in [
        (ReceiverMode::Complete, "continuity request"),
        (ReceiverMode::InvalidSampleTiming, "sample timing extent"),
        (
            ReceiverMode::WrongFpgaVersion,
            "checked FPGA version of 0.16.0",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut arguments = SWEEP_ARGS.to_vec();
        arguments.extend(["--check-continuity", "--spectrum-output", "skip"]);
        let output = run_sweep_on_antenna(directory.path(), mode, &arguments, 1);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(expected_error), "{stderr}");
        let scan = directory.path().join("scan");
        assert!(session_log(&scan).contains("fixture received --check-continuity"));
        let tune = scan.join("tune-0001");
        assert_eq!(
            fs::metadata(tune.join("wideband.sc16q11")).unwrap().len(),
            80_000
        );
        assert!(tune.join("wideband.sc16q11.json").is_file());
        assert!(!tune.join("capture.json").exists());
        assert!(!scan.join("sweep.json").exists());
    }
}

fn capture_checked_series(workspace: &Path) {
    let mut arguments = SERIES_ARGS.to_vec();
    arguments.extend(["--check-continuity", "--spectrum-output", "skip"]);
    let output = run_sweep_on_antenna(workspace, ReceiverMode::Checked, &arguments, 1);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg_attr(
    not(all(target_os = "linux", target_env = "gnu")),
    ignore = "checked capture requires Linux GNU"
)]
fn checked_series_publishes_and_replays_per_dwell_sample_timing() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    capture_checked_series(workspace);
    let scan = workspace.join("scan");
    assert!(session_log(&scan).contains("fixture received --check-continuity"));
    for name in ["plan.json", "acquisition-request.json", "sweep-series.json"] {
        assert_eq!(read_json(&scan.join(name))["check_continuity"], true);
    }
    let series = read_json(&scan.join("sweep-series.json"));
    for (pass_index, pass) in series["passes"].as_array().unwrap().iter().enumerate() {
        let pass = scan.join(pass["directory"].as_str().unwrap());
        let receipt = read_json(&pass.join("sweep.json"));
        assert_eq!(receipt["check_continuity"], true);
        for (tune_index, tune) in receipt["tunes"].as_array().unwrap().iter().enumerate() {
            let tune_directory = pass.join(tune["directory"].as_str().unwrap());
            let first_ns = 50_000_000 * pass_index + 20_000_000 * tune_index;
            let proof = json!({
                "first_sample_ns": first_ns,
                "end_sample_exclusive_ns": first_ns + 1_000_000,
                "witness_samples": 2048,
            });
            assert_eq!(tune["sample_timing"], proof);
            for name in ["wideband.sc16q11.json", "capture.json"] {
                let metadata = read_json(&tune_directory.join(name));
                assert_eq!(metadata["sample_timing"], proof);
                assert_eq!(metadata["fpga_version"], "0.16.0");
                assert_eq!(metadata["required_fpga_version"], "0.16.0");
            }
        }
    }

    let output = sweep_coverage(
        workspace,
        "scan",
        "coverage.json",
        &["--series", "--band", "911000000:912000000"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("coverage.json"));
    assert_eq!(report["sample_continuity_checked"], true);
    assert_eq!(report["dwell_count"], 6);
    assert_eq!(report["bands"][0]["covered_samples"], 120_000);
}

#[test]
#[cfg_attr(
    not(all(target_os = "linux", target_env = "gnu")),
    ignore = "checked capture requires Linux GNU"
)]
fn checked_archive_rejects_missing_inconsistent_or_changed_sample_timing() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    capture_checked_series(workspace);
    let scan = workspace.join("scan");
    let series_path = scan.join("sweep-series.json");
    let mut series = read_json(&series_path);
    series.as_object_mut().unwrap().remove("check_continuity");
    fs::write(series_path, serde_json::to_vec(&series).unwrap()).unwrap();
    let output = sweep_coverage(
        workspace,
        "scan",
        "invalid.json",
        &["--series", "--band", "911000000:912000000"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("continuity policy"), "{stderr}");
    assert!(!workspace.join("invalid.json").exists());

    let pass = scan.join("pass-0001");
    let path = pass.join("tune-0001/wideband.sc16q11.json");
    let original = fs::read(&path).unwrap();
    let mut metadata: Value = serde_json::from_slice(&original).unwrap();
    metadata.as_object_mut().unwrap().remove("sample_timing");
    let missing = metadata.clone();
    metadata["sample_timing"] = json!({
        "first_sample_ns": 50,
        "end_sample_exclusive_ns": 1_000_050,
        "witness_samples": 2048,
    });
    for changed in [missing, metadata] {
        fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        let output = sweep_coverage(
            workspace,
            "scan/pass-0001",
            "invalid.json",
            &["--band", "911000000:912000000"],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("sample timing"), "{stderr}");
        assert!(!workspace.join("invalid.json").exists());
    }
    fs::write(path, original).unwrap();

    let receipt_path = pass.join("sweep.json");
    let mut receipt = read_json(&receipt_path);
    receipt["tunes"][0]
        .as_object_mut()
        .unwrap()
        .remove("sample_timing");
    fs::write(receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    let output = sweep_coverage(
        workspace,
        "scan/pass-0001",
        "invalid.json",
        &["--band", "911000000:912000000"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("sample timing"), "{stderr}");
    assert!(!workspace.join("invalid.json").exists());
}

#[test]
fn band_sweep_packs_requests_and_preserves_series_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let mut arguments = vec![
        "sweep",
        "--band",
        "1089000000:1096899980",
        "--band",
        "909880000:909899980",
        "--band",
        "902000000:902012000",
        "--band",
        "902000000:902012000",
        "--dwell-ms",
        "1",
        "--passes",
        "2",
        "--max-iq-bytes",
        "319999",
    ];
    let rejected = run_sweep(workspace, ReceiverMode::Timed, &arguments);
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success());
    assert!(stderr.contains("320000"), "{stderr}");
    let scan = workspace.join("scan");
    assert!(!scan.exists());

    *arguments.last_mut().unwrap() = "320000";
    let capture = run_sweep(workspace, ReceiverMode::Timed, &arguments);
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    session_log(&scan);
    let plan = read_json(&scan.join("plan.json"));
    let bands = [
        (902_000_000, 902_012_000),
        (909_880_000, 909_899_980),
        (1_089_000_000, 1_096_899_980),
    ];
    let requested_bands = json!(
        bands.map(|(lower_hz, upper_hz)| json!({"lower_hz": lower_hz, "upper_hz": upper_hz}))
    );
    assert_eq!(plan["operation"], "targeted_rf_sweep_plan");
    assert_eq!(plan["requested_bands"], requested_bands);
    assert_eq!(plan["planned_iq_bytes"], 320_000);
    let centers: [u64; 2] = serde_json::from_value(plan["centers_hz"].clone()).unwrap();
    // Check both endpoints of the allowed readback error.
    for ((lower, upper), center) in bands.into_iter().zip([centers[0], centers[0], centers[1]]) {
        for observed in [center - 10, center + 10] {
            let fits_lower = lower >= observed - 8_000_000 && upper <= observed - 100_000;
            let fits_upper = lower >= observed + 100_000 && upper <= observed + 8_000_000;
            assert!(
                fits_lower || fits_upper,
                "{lower}..{upper} Hz misses {observed} Hz tune"
            );
        }
    }
    for pass in ["pass-0001", "pass-0002"] {
        let pass = scan.join(pass);
        let receipt = read_json(&pass.join("sweep.json"));
        assert_eq!(receipt["operation"], "targeted_rf_sweep");
        assert_eq!(receipt["requested_bands"], requested_bands);
        for (index, center) in centers.into_iter().enumerate() {
            let tune_name = format!("tune-{:04}", index + 1);
            assert_eq!(receipt["tunes"][index]["observed_center_hz"], center + 7);
            assert_validated_tune(&pass.join(tune_name), center, 2);
        }
    }

    let output = sweep_coverage(workspace, "scan", "coverage.json", &["--series"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("coverage.json"));
    let rows = report["bands"].as_array().unwrap();
    assert_eq!(rows.len(), bands.len());
    for (row, (lower, upper)) in rows.iter().zip(bands) {
        assert_eq!(row["lower_hz"], lower);
        assert_eq!(row["upper_hz"], upper);
        assert_eq!(row["covered_dwells"], 2);
        assert_eq!(row["covered_time_ns"], 2_000_000);
        assert_eq!(row["passes_with_coverage"], 2);
    }

    let output = sweep_coverage(
        workspace,
        "scan",
        "explicit.json",
        &["--series", "--band", "935000000:935025000"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("explicit.json"));
    assert_eq!(report["bands"].as_array().unwrap().len(), 1);
    assert_eq!(report["bands"][0]["lower_hz"], 935_000_000);
    assert_eq!(report["bands"][0]["covered_dwells"], 0);

    // Replay the pass alone to test its coverage claim independently of the series hash.
    let path = scan.join("pass-0001/sweep.json");
    let mut receipt = read_json(&path);
    receipt["requested_bands"]
        .as_array_mut()
        .unwrap()
        .insert(2, json!({"lower_hz": 935_000_000, "upper_hz": 935_025_000}));
    fs::write(path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    let output = sweep_coverage(workspace, "scan/pass-0001", "tampered.json", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("no single dwell covers"), "{stderr}");
    assert!(!workspace.join("tampered.json").exists());
}

#[test]
fn band_dwell_sweep_packs_durations_and_preserves_minimums() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let mut args = vec![
        "sweep",
        "--band",
        "941143000:941301000",
        "--band",
        "1205700000:1206110000:1",
        "--band",
        "936002000:936048000:3",
        "--band",
        "941143000:941301000:2",
        "--band",
        "936002000:936048000:2",
        "--dwell-ms",
        "12",
        "--passes",
        "2",
        "--spectrum-output",
        "skip",
        "--max-iq-bytes",
        "2079999",
    ];
    let scan = workspace.join("scan");
    let output = run_sweep(workspace, ReceiverMode::Timed, &args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("2080000"), "{stderr}");
    assert!(!scan.exists());

    *args.last_mut().unwrap() = "2080000";
    let output = run_sweep(workspace, ReceiverMode::Timed, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = read_json(&scan.join("plan.json"));
    let bands = [
        (936_002_000, 936_048_000),
        (941_143_000, 941_301_000),
        (1_205_700_000, 1_206_110_000),
    ];
    let requested = json!(
        bands.map(|(lower_hz, upper_hz)| json!({"lower_hz": lower_hz, "upper_hz": upper_hz}))
    );
    let minimums = json!([
        {"lower_hz": bands[0].0, "upper_hz": bands[0].1, "minimum_dwell_ms": 3},
        {"lower_hz": bands[2].0, "upper_hz": bands[2].1, "minimum_dwell_ms": 1}
    ]);
    assert_eq!(plan["requested_bands"], requested);
    assert_eq!(plan["requested_band_dwells"], minimums);
    assert_eq!(plan["dwell_ms"], 12);
    assert_eq!(plan["planned_iq_bytes"], 2_080_000);
    let centers: [u64; 2] = serde_json::from_value(plan["centers_hz"].clone()).unwrap();
    let overrides = plan["dwell_ms_by_center"].as_object().unwrap();
    assert_eq!(overrides.len(), 1);
    assert_eq!(overrides[&centers[1].to_string()], 1);
    let log = session_log(&scan);
    let expected_override = format!("fixture dwells {}:1", centers[1]);
    assert!(log.lines().any(|line| line == expected_override));

    let series = read_json(&scan.join("sweep-series.json"));
    assert_eq!(series["iq_bytes"], 2_080_000);
    assert_eq!(series["dwells"], 4);
    for name in ["pass-0001", "pass-0002"] {
        let pass = scan.join(name);
        let receipt = read_json(&pass.join("sweep.json"));
        assert_eq!(receipt["dwell_ms"], 12);
        assert_eq!(receipt["iq_bytes"], 1_040_000);
        assert_eq!(receipt["requested_bands"], requested);
        assert_eq!(receipt["requested_band_dwells"], minimums);
        let tunes = receipt["tunes"].as_array().unwrap();
        assert_eq!(tunes.len(), 2);
        assert!(tunes[0].get("dwell_ms").is_none());
        assert_eq!(tunes[1]["dwell_ms"], 1);
        for (index, (tune, samples)) in tunes.iter().zip([240_000, 20_000]).enumerate() {
            assert_eq!(tune["requested_center_hz"], centers[index]);
            let path = pass.join(format!("tune-{:04}", index + 1));
            assert_eq!(
                read_json(&path.join("capture.json"))["num_samples"],
                samples
            );
            assert_eq!(
                fs::metadata(path.join("wideband.sc16q11")).unwrap().len(),
                4 * samples
            );
        }
    }

    let output = sweep_coverage(workspace, "scan", "band-dwell-coverage.json", &["--series"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let coverage = read_json(&workspace.join("band-dwell-coverage.json"));
    let rows = coverage["bands"].as_array().unwrap();
    assert_eq!(rows.len(), bands.len());
    for (row, ((lower, upper), covered_ms)) in rows.iter().zip(bands.into_iter().zip([24, 24, 2])) {
        assert_eq!(row["lower_hz"], lower);
        assert_eq!(row["upper_hz"], upper);
        assert_eq!(row["covered_dwells"], 2);
        assert_eq!(row["covered_time_ns"], covered_ms * 1_000_000);
    }

    // Leave IQ unchanged and make the remote band's claimed minimum too long.
    let path = scan.join("pass-0001/sweep.json");
    let mut defaulted = read_json(&path);
    let mut raised = defaulted.clone();
    raised["requested_band_dwells"][1]["minimum_dwell_ms"] = json!(2);
    defaulted
        .as_object_mut()
        .unwrap()
        .remove("requested_band_dwells");
    for (receipt, minimum) in [(raised, 2), (defaulted, 12)] {
        fs::write(&path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let output = sweep_coverage(workspace, "scan/pass-0001", "bad-minimum.json", &[]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains(&format!("1205700000..1206110000 Hz for {minimum} ms")),
            "{stderr}"
        );
        assert!(!workspace.join("bad-minimum.json").exists());
    }
}

#[test]
fn band_dwell_sweep_rejects_malformed_durations_and_excess_inputs_before_output() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    for invalid in [
        "936002000:936048000:",
        "936002000:936048000:0",
        "936002000:936048000:10001",
        "936002000:936048000:1.5",
        "936002000:936048000:2:3",
    ] {
        let output = run_sweep(
            workspace,
            ReceiverMode::Complete,
            &["sweep", "--band", invalid],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("--band"), "{stderr}");
        assert!(!workspace.join("scan").exists());
    }

    let mut args = vec!["sweep", "--dwell-ms", "1"];
    args.extend(["--band", "936002000:936048000:2"].repeat(33));
    let output = run_sweep(workspace, ReceiverMode::Complete, &args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("32"), "{stderr}");
    assert!(!workspace.join("scan").exists());
}

#[test]
fn band_sweep_rejects_impossible_width_and_conflicting_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let output = run_sweep(
        workspace,
        ReceiverMode::Complete,
        &["sweep", "--band", "902000000:909899981", "--dwell-ms", "1"],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("band"), "{stderr}");
    assert!(!workspace.join("scan").exists());

    let conflicts: &[&[&str]] = &[
        &["--lower-hz", "902000000", "--upper-hz", "903000000"],
        &["--center-hz", "905000000"],
        &["--follow-up-plan", "missing.json"],
        &["--triage-report", "missing.json"],
        &["--series-triage-report", "missing.json"],
    ];
    for &conflict in conflicts {
        let mut arguments = vec!["sweep", "--band", "902000000:902012000", "--dwell-ms", "1"];
        arguments.extend_from_slice(conflict);
        let output = run_sweep(workspace, ReceiverMode::Complete, &arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("cannot be used with"), "{stderr}");
        assert!(!workspace.join("scan").exists());
    }
}

#[test]
fn sweep_coverage_counts_exact_readback_bounds_without_native_timing() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let capture = run_sweep(
        workspace,
        ReceiverMode::Complete,
        &[
            "sweep",
            "--center-hz",
            "910000000",
            "--dwell-ms",
            "1",
            "--spectrum-output",
            "skip",
        ],
    );
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let output = sweep_coverage(
        workspace,
        "scan",
        "coverage.json",
        &[
            "--band",
            "910100007:918000007",
            "--band",
            "909900007:910100007",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("coverage.json"));
    let receipt = fs::read(workspace.join("scan/sweep.json")).unwrap();
    assert_eq!(
        report["source_capture_sha256"],
        format!("{:x}", Sha256::digest(receipt))
    );
    assert!(report.get("source_timing_sha256").is_none());
    assert!(report.get("host_read_window").is_none());
    assert!(report.get("sample_continuity_checked").is_none());
    assert_eq!(report["pass_count"], 1);
    assert_eq!(report["dwell_count"], 1);
    assert_eq!(report["sample_rate_hz"], 20_000_000);
    assert_eq!(report["minimum_dwell_ms"], 1);
    assert!(report.get("frequency_map").is_none());
    assert_eq!(
        report["bands"],
        json!([
            {
                "lower_hz": 909_900_007,
                "upper_hz": 910_100_007,
                "covered_dwells": 0,
                "qualifying_dwells": 0,
                "covered_samples": 0,
                "qualifying_samples": 0,
                "covered_time_ns": 0,
                "qualifying_time_ns": 0,
                "longest_dwell_ns": 0,
                "passes_with_coverage": 0
            },
            {
                "lower_hz": 910_100_007,
                "upper_hz": 918_000_007,
                "covered_dwells": 1,
                "qualifying_dwells": 1,
                "covered_samples": 20_000,
                "qualifying_samples": 20_000,
                "covered_time_ns": 1_000_000,
                "qualifying_time_ns": 1_000_000,
                "longest_dwell_ns": 1_000_000,
                "passes_with_coverage": 1
            }
        ])
    );
}

#[test]
fn sweep_coverage_uses_covering_dwells_and_bound_series_timing() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let capture = run_sweep(
        workspace,
        ReceiverMode::Timed,
        &[
            "sweep",
            "--center-hz",
            "910000000",
            "--center-hz",
            "917800000",
            "--dwell-ms",
            "1",
            "--passes",
            "3",
            "--spectrum-output",
            "skip",
        ],
    );
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    let arguments = [
        "--series",
        "--band",
        "919000007:920000007",
        "--band",
        "912000007:916000007",
        "--band",
        "909850007:918000007",
        "--min-dwell-ms",
        "2",
    ];
    let output = sweep_coverage(workspace, "scan", "coverage.json", &arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("coverage.json"));
    let receipt = fs::read(workspace.join("scan/sweep-series.json")).unwrap();
    let timing_path = workspace.join("scan/acquisition-timing.json");
    let mut timing = fs::read(&timing_path).unwrap();
    assert_eq!(
        report["source_capture_sha256"],
        format!("{:x}", Sha256::digest(receipt))
    );
    assert_eq!(
        report["source_timing_sha256"],
        format!("{:x}", Sha256::digest(&timing))
    );
    assert_eq!(
        report["host_read_window"],
        json!({"started_ns": 900_000, "finished_ns": 21_900_000})
    );
    assert_eq!(report["pass_count"], 3);
    assert_eq!(report["dwell_count"], 6);
    assert_eq!(report["minimum_dwell_ms"], 2);
    let bands = report["bands"].as_array().unwrap();
    assert_eq!(bands.len(), 3);
    // The union across both centers spans this band, but no single dwell does.
    assert_eq!(bands[0]["lower_hz"], 909_850_007);
    assert_eq!(bands[0]["covered_dwells"], 0);
    assert!(bands[0].get("longest_host_read_gap_ns").is_none());
    let full_window_gap = json!({"longest_ns": 21_000_000});
    assert_eq!(bands[0]["covered_host_read_gaps"], full_window_gap);
    assert_eq!(bands[0]["qualifying_host_read_gaps"], full_window_gap);
    assert_eq!(bands[1]["lower_hz"], 912_000_007);
    assert_eq!(bands[1]["covered_dwells"], 6);
    assert_eq!(bands[1]["covered_samples"], 120_000);
    assert_eq!(bands[1]["covered_time_ns"], 6_000_000);
    assert_eq!(bands[1]["qualifying_dwells"], 0);
    assert_eq!(bands[1]["qualifying_samples"], 0);
    assert_eq!(bands[1]["qualifying_time_ns"], 0);
    assert_eq!(bands[1]["longest_dwell_ns"], 1_000_000);
    assert_eq!(bands[1]["passes_with_coverage"], 3);
    assert_eq!(bands[1]["longest_host_read_gap_ns"], 5_000_000);
    assert_eq!(
        bands[1]["covered_host_read_gaps"],
        json!({"leading_ns": 0, "trailing_ns": 0, "longest_ns": 5_000_000})
    );
    assert_eq!(bands[1]["qualifying_host_read_gaps"], full_window_gap);
    assert_eq!(bands[2]["lower_hz"], 919_000_007);
    assert_eq!(bands[2]["covered_dwells"], 3);
    assert_eq!(bands[2]["passes_with_coverage"], 3);
    assert_eq!(bands[2]["longest_host_read_gap_ns"], 10_000_000);
    assert_eq!(
        bands[2]["covered_host_read_gaps"],
        json!({"leading_ns": 2_000_000, "trailing_ns": 0, "longest_ns": 10_000_000})
    );

    let map_arguments = [
        "--series",
        "--frequency-map",
        "--range-hz",
        "901900007:925900007",
        "--min-dwell-ms",
        "2",
    ];
    for (arguments, diagnostic) in [
        (&["--series", "--frequency-map"][..], "--range-hz"),
        (
            &[
                "--series",
                "--frequency-map",
                "--band",
                "910000000:912000000",
            ][..],
            "--band",
        ),
    ] {
        let rejected = sweep_coverage(workspace, "scan", "map.json", arguments);
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(!rejected.status.success(), "{arguments:?}");
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!workspace.join("map.json").exists());
    }
    let measured = sweep_coverage(workspace, "scan", "map.json", &map_arguments);
    assert!(
        measured.status.success(),
        "{}",
        String::from_utf8_lossy(&measured.stderr)
    );
    let measured = read_json(&workspace.join("map.json"));
    assert_eq!(
        measured["source_timing_sha256"],
        report["source_timing_sha256"]
    );
    assert_eq!(measured["host_read_window"], report["host_read_window"]);
    let map = &measured["frequency_map"];
    assert_eq!(map["lower_hz"], 901_900_007);
    assert_eq!(map["upper_hz"], 925_900_007);
    assert_eq!(map["covered_bandwidth_hz"], 23_800_000);
    assert_eq!(map["qualifying_bandwidth_hz"], 0);
    let regions = map["regions"].as_array().unwrap();
    let edges = [
        901_900_007,
        902_000_007,
        909_800_007,
        909_900_007,
        910_100_007,
        917_700_007,
        917_900_007,
        918_000_007,
        925_800_007,
        925_900_007,
    ];
    assert_eq!(regions.len(), edges.len() - 1);
    for ((region, bounds), count) in regions
        .iter()
        .zip(edges.windows(2))
        .zip([0, 3, 6, 3, 6, 3, 6, 3, 0])
    {
        assert_eq!(region["lower_hz"], bounds[0]);
        assert_eq!(region["upper_hz"], bounds[1]);
        assert_eq!(region["covered_dwells"], count);
        assert_eq!(region["covered_time_ns"], count * 1_000_000);
        assert_eq!(region["qualifying_dwells"], 0);
        assert_eq!(region["qualifying_host_read_gaps"], full_window_gap);
    }
    assert_eq!(regions[0]["covered_host_read_gaps"], full_window_gap);
    assert_eq!(
        regions[4]["covered_host_read_gaps"],
        bands[1]["covered_host_read_gaps"]
    );
    assert_eq!(
        regions[7]["covered_host_read_gaps"],
        bands[2]["covered_host_read_gaps"]
    );

    // Whitespace preserves the JSON document and still changes the bound digest.
    timing.push(b' ');
    fs::write(timing_path, timing).unwrap();
    for arguments in [&arguments[..], &map_arguments[..]] {
        let output = sweep_coverage(workspace, "scan", "tampered.json", arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("timing"), "{stderr}");
        assert!(!workspace.join("tampered.json").exists());
    }
}

#[test]
fn invalid_native_timing_retains_iq_without_a_terminal_report() {
    for mode in [
        ReceiverMode::MisorderedTiming,
        ReceiverMode::MalformedTiming,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let output = run_sweep(directory.path(), mode, SWEEP_ARGS);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("timing"), "{stderr}");
        let destination = directory.path().join("scan");
        let plan = read_json(&destination.join("plan.json"));
        let tune_count = plan["centers_hz"].as_array().unwrap().len();
        for index in 0..tune_count {
            let tune = destination.join(format!("tune-{:04}", index + 1));
            assert_eq!(
                fs::metadata(tune.join("wideband.sc16q11")).unwrap().len(),
                80_000
            );
            assert!(tune.join("wideband.sc16q11.json").is_file());
        }
        assert!(destination.join("acquisition-timing.json").is_file());
        assert!(!destination.join("sweep.json").exists());
    }
}

#[test]
fn repeated_sweep_charges_all_passes_before_acquisition() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = SERIES_ARGS.to_vec();
    args.extend(["--max-iq-bytes", "479999"]);
    let output = run_sweep(directory.path(), ReceiverMode::Timed, &args);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("480000"), "{stderr}");
    assert!(!directory.path().join("scan").exists());
}

const PRIORITY_BAND: &str = "1089000000:1091000000";
const PRIORITY_ARGUMENTS: &[&str] = &[
    "--priority-band",
    "1089000000:1091000000:3",
    "--passes",
    "2",
    "--spectrum-output",
    "skip",
];

#[test]
fn priority_sweep_preserves_background_and_reduces_qualifying_host_gaps() {
    let background = [909_999_990, 917_799_990, 933_799_970];
    let mut longest_gaps = Vec::new();
    for (revisit_every, visits_per_pass, exact_budget) in [(1, 6, "1920000"), (3, 4, "960000")] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path();
        let scan = workspace.join("scan");
        let interval = revisit_every.to_string();
        let mut arguments = SWEEP_ARGS.to_vec();
        arguments.extend_from_slice(PRIORITY_ARGUMENTS);
        arguments.extend(["--revisit-every", &interval, "--max-iq-bytes", exact_budget]);
        let capture = run_sweep(workspace, ReceiverMode::Timed, &arguments);
        assert!(
            capture.status.success(),
            "{}",
            String::from_utf8_lossy(&capture.stderr)
        );

        let plan = read_json(&scan.join("plan.json"));
        let priority = plan["priority"]["centers_hz"].as_array().unwrap();
        assert_eq!(priority.len(), 1);
        let center = priority[0].as_u64().unwrap();
        assert_eq!(plan["priority"]["revisit_every"], revisit_every);
        assert_eq!(
            plan["centers_hz"],
            json!([background[0], background[1], background[2], center])
        );
        assert_eq!(plan["dwell_ms_by_center"], json!({(center.to_string()): 3}));
        let expected_visits: Vec<_> = background
            .chunks(revisit_every)
            .flat_map(|chunk| std::iter::once(center).chain(chunk.iter().copied()))
            .collect();
        assert_eq!(expected_visits.len(), visits_per_pass);
        let visit_order = json!(expected_visits);
        assert_eq!(plan["visit_centers_hz"], visit_order);
        assert_eq!(
            plan["planned_iq_bytes"],
            exact_budget.parse::<u64>().unwrap()
        );

        let log = session_log(&scan);
        let visits = log
            .lines()
            .find_map(|line| line.strip_prefix("fixture visits "))
            .unwrap()
            .split_whitespace()
            .map(|center| center.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(visits, expected_visits);
        let request = read_json(&scan.join("acquisition-request.json"));
        assert_eq!(request["visit_centers_hz"], visit_order);
        assert_eq!(request["tunes"], 2 * visits_per_pass);
        let series = read_json(&scan.join("sweep-series.json"));
        assert_eq!(series["visit_centers_hz"], visit_order);
        assert_eq!(series["dwells"], request["tunes"]);
        assert_eq!(series["iq_bytes"], plan["planned_iq_bytes"]);

        for name in ["pass-0001", "pass-0002"] {
            let pass = scan.join(name);
            let receipt = read_json(&pass.join("sweep.json"));
            assert_eq!(receipt["operation"], "sequential_rf_sweep");
            assert_eq!(receipt["visit_centers_hz"], visit_order);
            let tunes = receipt["tunes"].as_array().unwrap();
            assert_eq!(tunes.len(), visits_per_pass);
            for (tune, &expected_center) in tunes.iter().zip(&expected_visits) {
                assert_eq!(tune["requested_center_hz"], expected_center);
                let directory = tune["directory"].as_str().unwrap();
                let bytes = fs::metadata(pass.join(directory).join("wideband.sc16q11"))
                    .unwrap()
                    .len();
                assert_eq!(
                    bytes,
                    if expected_center == center {
                        240_000
                    } else {
                        80_000
                    }
                );
            }
        }

        let output = sweep_coverage(
            workspace,
            "scan",
            "coverage.json",
            &["--series", "--band", PRIORITY_BAND, "--min-dwell-ms", "3"],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let coverage = read_json(&workspace.join("coverage.json"));
        let band = &coverage["bands"][0];
        assert_eq!(
            band["qualifying_dwells"],
            2 * (visits_per_pass - background.len())
        );
        assert_eq!(band["covered_dwells"], band["qualifying_dwells"]);
        assert_eq!(band["longest_dwell_ns"], 3_000_000);
        longest_gaps.push(
            band["qualifying_host_read_gaps"]["longest_ns"]
                .as_u64()
                .unwrap(),
        );
    }
    assert_eq!(longest_gaps, [3_000_000, 7_000_000]);
}

#[test]
fn priority_sweep_rejects_excess_iq_and_inconsistent_saved_visits() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let scan = workspace.join("scan");
    let mut arguments = SWEEP_ARGS.to_vec();
    arguments.extend_from_slice(PRIORITY_ARGUMENTS);
    arguments.extend(["--revisit-every", "1", "--max-iq-bytes", "1919999"]);
    let output = run_sweep(workspace, ReceiverMode::Timed, &arguments);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("1920000"), "{stderr}");
    assert!(!scan.exists());

    *arguments.last_mut().unwrap() = "1920000";
    let capture = run_sweep(workspace, ReceiverMode::Timed, &arguments);
    assert!(
        capture.status.success(),
        "{}",
        String::from_utf8_lossy(&capture.stderr)
    );

    let pass_path = scan.join("pass-0001/sweep.json");
    let mut wrong_visit = read_json(&pass_path);
    let mut missing_order = wrong_visit.clone();
    missing_order
        .as_object_mut()
        .unwrap()
        .remove("visit_centers_hz");
    wrong_visit["visit_centers_hz"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    for (receipt, message) in [
        (wrong_visit, "visit order"),
        (missing_order, "strictly increasing"),
    ] {
        fs::write(&pass_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let output = sweep_coverage(
            workspace,
            "scan/pass-0001",
            "bad-order.json",
            &["--band", PRIORITY_BAND],
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(message), "{stderr}");
        assert!(!workspace.join("bad-order.json").exists());
    }
}

#[test]
fn mixed_dwell_sweep_records_sorted_schedule_and_replays_exact_extents() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let mut args = vec![
        "sweep",
        "--center-hz",
        "918000000",
        "--center-hz",
        "926000000",
        "--center-hz",
        "910000000",
        "--dwell-ms",
        "1",
        "--dwell-at",
        "918000000:3",
        "--dwell-at",
        "926000000:1",
        "--dwell-at",
        "910000000:2",
        "--passes",
        "2",
        "--spectrum-output",
        "skip",
        "--max-iq-bytes",
        "959999",
    ];
    let scan = workspace.join("scan");
    let over_budget = run_sweep(workspace, ReceiverMode::Timed, &args);
    let stderr = String::from_utf8_lossy(&over_budget.stderr);
    assert!(!over_budget.status.success());
    assert!(stderr.contains("960000"), "{stderr}");
    assert!(!scan.exists());

    *args.last_mut().unwrap() = "960000";
    let output = run_sweep(workspace, ReceiverMode::Timed, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log = session_log(&scan);
    assert!(
        log.lines()
            .any(|line| line == "fixture centers 910000000 918000000 926000000")
    );
    assert!(
        log.lines()
            .any(|line| line == "fixture dwells 910000000:2 918000000:3")
    );
    assert!(
        log.lines()
            .any(|line| line == "fixture default duration 0.001")
    );

    let overrides = json!({"910000000": 2, "918000000": 3});
    let centers = [910_000_000, 918_000_000, 926_000_000];
    let plan = read_json(&scan.join("plan.json"));
    assert_eq!(plan["operation"], "targeted_rf_sweep_plan");
    assert_eq!(plan["centers_hz"], json!(centers));
    assert_eq!(plan["dwell_ms"], 1);
    assert_eq!(plan["samples_per_tune"], 20_000);
    assert_eq!(plan["dwell_ms_by_center"], overrides);
    assert_eq!(plan["planned_iq_bytes"], 960_000);
    let request = read_json(&scan.join("acquisition-request.json"));
    assert_eq!(request["samples_per_tune"], 20_000);
    assert_eq!(request["dwell_ms_by_center"], overrides);
    assert_eq!(request["tunes"], 6);

    let series = read_json(&scan.join("sweep-series.json"));
    assert_eq!(series["iq_bytes"], 960_000);
    assert_eq!(series["dwells"], 6);
    assert_eq!(series["capture_cadence"]["nominal_iq_seconds"], 0.012);
    for name in ["pass-0001", "pass-0002"] {
        let pass = scan.join(name);
        let receipt = read_json(&pass.join("sweep.json"));
        assert_eq!(receipt["dwell_ms"], 1);
        assert_eq!(receipt["iq_bytes"], 480_000);
        assert_eq!(receipt["capture_cadence"]["nominal_iq_seconds"], 0.006);
        let tunes = receipt["tunes"].as_array().unwrap();
        assert_eq!(tunes.len(), 3);
        for (index, ((tune, center), dwell_ms)) in
            tunes.iter().zip(centers).zip([2, 3, 1]).enumerate()
        {
            assert_eq!(tune["requested_center_hz"], center);
            assert_eq!(tune["observed_center_hz"], center + 7);
            if dwell_ms == 1 {
                assert!(tune.get("dwell_ms").is_none());
            } else {
                assert_eq!(tune["dwell_ms"], dwell_ms);
            }
            let path = pass.join(format!("tune-{:04}", index + 1));
            let samples = 20_000 * dwell_ms;
            assert_eq!(
                read_json(&path.join("request.json"))["samples_requested"],
                samples
            );
            assert_eq!(
                read_json(&path.join("capture.json"))["num_samples"],
                samples
            );
            assert_eq!(
                fs::metadata(path.join("wideband.sc16q11")).unwrap().len(),
                4 * samples
            );
        }
    }

    let coverage = sweep_coverage(
        workspace,
        "scan",
        "mixed-coverage.json",
        &[
            "--series",
            "--band",
            "920000000:920100000",
            "--min-dwell-ms",
            "2",
        ],
    );
    assert!(
        coverage.status.success(),
        "{}",
        String::from_utf8_lossy(&coverage.stderr)
    );
    let report = read_json(&workspace.join("mixed-coverage.json"));
    let band = &report["bands"][0];
    assert_eq!(band["covered_dwells"], 4);
    assert_eq!(band["qualifying_dwells"], 2);
    assert_eq!(band["covered_samples"], 160_000);
    assert_eq!(band["qualifying_samples"], 120_000);
    assert_eq!(band["longest_dwell_ns"], 3_000_000);
    assert_eq!(
        band["covered_host_read_gaps"],
        json!({"leading_ns": 2_000_000, "trailing_ns": 0, "longest_ns": 8_000_000})
    );
    assert_eq!(
        band["qualifying_host_read_gaps"],
        json!({"leading_ns": 2_000_000, "trailing_ns": 6_000_000, "longest_ns": 11_000_000})
    );
}

#[test]
fn mixed_dwell_sweep_rejects_invalid_overrides_before_output() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    for (extra, frequency) in [
        (
            &["--dwell-at", "910000000:1", "--dwell-at", "910000000:2"][..],
            "910000000",
        ),
        (&["--dwell-at", "918000000:2"][..], "918000000"),
    ] {
        let mut args = vec!["sweep", "--center-hz", "910000000", "--dwell-ms", "1"];
        args.extend_from_slice(extra);
        let output = run_sweep(workspace, ReceiverMode::Complete, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains(frequency), "{stderr}");
        assert!(!workspace.join("scan").exists());
    }

    let output = run_sweep(
        workspace,
        ReceiverMode::Complete,
        &[
            "sweep",
            "--band",
            "910000000:910100000",
            "--dwell-at",
            "910000000:2",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--center-hz"), "{stderr}");
    assert!(!workspace.join("scan").exists());
}

#[test]
fn repeated_sweep_publishes_ordered_passes_with_replayable_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let output = run_sweep(workspace, ReceiverMode::Timed, SERIES_ARGS);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = workspace.join("scan");
    assert!(!session_log(&destination).contains("fixture visits"));
    let plan = read_json(&destination.join("plan.json"));
    assert_eq!(plan["passes"], 3);
    assert_eq!(plan["planned_iq_bytes"], 480_000);
    assert!(plan.get("visit_centers_hz").is_none());
    let centers_hz = [910_000_000u64, 918_000_000];
    assert_eq!(plan["centers_hz"], json!(centers_hz));
    assert!(!destination.join("sweep.json").exists());
    assert!(!destination.join("tune-0001").exists());

    let report = read_json(&destination.join("sweep-series.json"));
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "rf_sweep_series");
    assert_eq!(report["iq_bytes"], 480_000);
    assert!(report.get("visit_centers_hz").is_none());
    let native = &report["capture_cadence"]["native"];
    assert_eq!(
        report["same_center_revisit_ns"],
        json!({"min": 5_000_000, "median": 8_000_000, "max": 11_000_000})
    );
    let timing_bytes = fs::read(destination.join("acquisition-timing.json")).unwrap();
    let timing: Value = serde_json::from_slice(&timing_bytes).unwrap();
    assert_eq!(
        native["sha256"],
        format!("{:x}", Sha256::digest(&timing_bytes))
    );
    assert_eq!(timing["passes"], 3);
    let timings = timing["dwells"].as_array().unwrap();
    assert_eq!(timings.len(), 6);

    let passes = report["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 3);
    for (index, pass) in passes.iter().enumerate() {
        let pass_number = index + 1;
        let pass_name = format!("pass-{pass_number:04}");
        assert_eq!(pass["directory"], pass_name);
        let pass_path = destination.join(&pass_name);
        let bytes = fs::read(pass_path.join("sweep.json")).unwrap();
        assert_eq!(
            pass["sweep_sha256"],
            format!("{:x}", Sha256::digest(&bytes))
        );
        let report: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["operation"], "targeted_rf_sweep");
        assert_eq!(report["iq_bytes"], 160_000);
        assert!(report.get("visit_centers_hz").is_none());
        assert_eq!(
            report["acquisition_elapsed_seconds"].as_f64(),
            Some([0.0037, 0.0057, 0.0077][index])
        );
        let pass_tunes = report["tunes"].as_array().unwrap();
        assert_eq!(pass_tunes.len(), centers_hz.len());
        for (tune_index, (tune, center_hz)) in pass_tunes.iter().zip(centers_hz).enumerate() {
            let tune_number = tune_index + 1;
            let tune_name = format!("tune-{tune_number:04}");
            assert_eq!(tune["directory"], tune_name);
            assert_eq!(tune["requested_center_hz"], center_hz);
            assert_eq!(
                timings[index * 2 + tune_index]["requested_center_hz"],
                center_hz
            );
            let tune_path = pass_path.join(tune_name);
            assert_validated_tune(&tune_path, center_hz, 2);
            let iq = fs::read(tune_path.join("wideband.sc16q11")).unwrap();
            assert_eq!(iq[..4], [pass_number as u8, 0, tune_number as u8, 0]);
        }
    }

    let output = Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "triage-sweep",
            "--sweep-directory",
            "scan/pass-0002",
            "--output-directory",
            "triage",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replay = read_json(&workspace.join("triage/triage.json"));
    assert_eq!(replay["source_receipt_sha256"], passes[1]["sweep_sha256"]);
    let iq = fs::read(destination.join("pass-0002/tune-0001/wideband.sc16q11")).unwrap();
    assert_eq!(
        replay["tunes"][0]["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..16_384 * 4]))
    );
}

#[test]
fn repeated_sweep_preserves_complete_pass_on_later_adapter_failure() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_sweep(
        directory.path(),
        ReceiverMode::FailedSecondPass,
        SERIES_ARGS,
    );
    assert!(!output.status.success());
    let destination = directory.path().join("scan");
    assert!(session_log(&destination).contains("fixture failed at second tune"));
    assert!(!destination.join("sweep-series.json").exists());
    assert!(!destination.join("acquisition-timing.json").exists());
    let first = destination.join("pass-0001");
    let report = read_json(&first.join("sweep.json"));
    assert_eq!(report["iq_bytes"], 160_000);
    assert!(report["capture_cadence"].get("native").is_none());
    assert_validated_tune(&first.join("tune-0001"), 910_000_000, 2);
    assert_validated_tune(&first.join("tune-0002"), 918_000_000, 2);
    let second = destination.join("pass-0002");
    assert_validated_tune(&second.join("tune-0001"), 910_000_000, 2);
    assert!(second.join("tune-0002/request.json").is_file());
    assert!(!second.join("tune-0002/spectrum.json").exists());
    assert!(!second.join("sweep.json").exists());
    assert!(!destination.join("pass-0003/sweep.json").exists());
}

#[test]
fn repeated_sweep_preserves_complete_passes_without_valid_timing() {
    for mode in [ReceiverMode::Complete, ReceiverMode::MalformedTiming] {
        let directory = tempfile::tempdir().unwrap();
        let output = run_sweep(directory.path(), mode, SERIES_ARGS);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("timing"), "{stderr}");
        let destination = directory.path().join("scan");
        assert!(!destination.join("sweep-series.json").exists());
        for pass in 1..=3 {
            let path = destination.join(format!("pass-{pass:04}/sweep.json"));
            let receipt = read_json(&path);
            assert_eq!(receipt["iq_bytes"], 160_000);
            assert!(receipt["capture_cadence"].get("native").is_none());
            assert!(receipt.get("acquisition_elapsed_seconds").is_none());
        }
    }
}

#[test]
fn follow_up_plan_previews_and_captures_only_the_requested_centers() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let frequencies_hz = [2_412_000_000u64, 2_462_000_000];
    let follow_up = workspace.join("follow-up.json");
    let file = File::create(&follow_up).unwrap();
    let source_digest = "ab".repeat(32);
    serde_json::to_writer(
        file,
        &json!({
            "schema_version": 1,
            "operation": "plan_sweep_follow_up",
            "source_sweep_sha256": source_digest,
            "centers_hz": frequencies_hz,
            "targets": [
                {"protocol": "wifi", "channel_number": 1, "frequency_hz": frequencies_hz[0]},
                {"protocol": "wifi", "channel_number": 11, "frequency_hz": frequencies_hz[1]},
            ],
        }),
    )
    .unwrap();
    let arguments = [
        "sweep",
        "--follow-up-plan",
        "follow-up.json",
        "--dwell-ms",
        "1",
        "--max-iq-bytes",
        "160000",
    ];

    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(arguments)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["operation"], "targeted_rf_sweep_plan");
    assert_eq!(preview["centers_hz"], json!(frequencies_hz));
    assert_eq!(preview["planned_iq_bytes"], 160_000);
    assert_eq!(preview["source_sweep_sha256"], source_digest);
    let plan_digest = Sha256::digest(fs::read(follow_up).unwrap());
    assert_eq!(preview["follow_up_plan_sha256"], format!("{plan_digest:x}"));

    let manual = Command::new(BINARY)
        .current_dir(workspace)
        .args(["sweep", "--plan-only"])
        .args(["--center-hz", "2462000000", "--center-hz", "2412000000"])
        .output()
        .unwrap();
    assert!(
        manual.status.success(),
        "{}",
        String::from_utf8_lossy(&manual.stderr)
    );
    let manual: Value = serde_json::from_slice(&manual.stdout).unwrap();
    assert_eq!(manual["centers_hz"], preview["centers_hz"]);
    assert_eq!(fs::read_dir(workspace).unwrap().count(), 1);

    let output = run_sweep(workspace, ReceiverMode::Complete, &arguments);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let destination = workspace.join("scan");
    session_log(&destination);
    assert_eq!(read_json(&destination.join("plan.json")), preview);
    let receipt = read_json(&destination.join("sweep.json"));
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["operation"], "targeted_rf_sweep");
    assert_eq!(receipt["lower_hz"], frequencies_hz[0]);
    assert_eq!(receipt["upper_hz"], frequencies_hz[1] + 1);
    assert_eq!(receipt["iq_bytes"], 160_000);
    let tunes = receipt["tunes"].as_array().unwrap();
    assert_eq!(tunes.len(), frequencies_hz.len());
    for (index, (tune, center_hz)) in tunes.iter().zip(frequencies_hz).enumerate() {
        let name = format!("tune-{:04}", index + 1);
        assert_eq!(tune["directory"], name);
        assert_eq!(tune["requested_center_hz"], center_hz);
        assert_eq!(tune["observed_center_hz"], center_hz + 7);
        assert_validated_tune(&destination.join(name), center_hz, 2);
    }
}

#[test]
fn later_adapter_failure_preserves_the_validated_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_sweep(directory.path(), ReceiverMode::Failed, SWEEP_ARGS);
    assert!(!output.status.success());
    let destination = directory.path().join("scan");
    let plan = read_json(&destination.join("plan.json"));
    let first_center = plan["centers_hz"][0].as_u64().unwrap();
    assert_validated_tune(&destination.join("tune-0001"), first_center, 2);
    let failed_tune = destination.join("tune-0002");
    let log = session_log(&destination);
    assert!(log.contains("fixture failed at second tune"));
    assert!(failed_tune.join("request.json").is_file());
    assert!(!failed_tune.join("spectrum.json").exists());
    assert!(!destination.join("tune-0003").exists());
    assert!(!destination.join("sweep.json").exists());
}

#[test]
fn successful_adapter_exit_requires_every_planned_tune() {
    let directory = tempfile::tempdir().unwrap();
    let output = run_sweep(directory.path(), ReceiverMode::Incomplete, SWEEP_ARGS);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("native sweep exited successfully without completing tune-0002")
    );
    let destination = directory.path().join("scan");
    let plan = read_json(&destination.join("plan.json"));
    let first_center = plan["centers_hz"][0].as_u64().unwrap();
    assert_validated_tune(&destination.join("tune-0001"), first_center, 2);
    session_log(&destination);
    assert!(!destination.join("sweep.json").exists());
}

const TRIAGE_FIXTURE_SAMPLES: usize = 240_000;
const TRIAGE_FFT_SAMPLES: usize = 8_192;
const BURST_SAMPLES: usize = 2 * TRIAGE_FFT_SAMPLES;
const BURSTS: [(usize, i32, f64); 2] = [(32_768, -5_250_000, 42.0), (147_456, 5_250_000, 72.0)];
const MIXED_BURST: (usize, i32, f64) = (BURSTS[0].0, BURSTS[0].1, 56.0);
const STRONG_BROADBAND_START: usize = 16 * TRIAGE_FFT_SAMPLES;
const TRIAGE_TARGETED_ARGS: &[&str] = &[
    "sweep",
    "--center-hz",
    "910000000",
    "--center-hz",
    "918000000",
    "--dwell-ms",
    "12",
];

enum TriageFixture {
    SeparateBursts,
    OverlappingBursts,
    BroadbandOnly,
}

fn triage_fixture(fixture: TriageFixture) -> Vec<u8> {
    const INITIAL_SEED: u32 = 543_210;
    let mut seed = INITIAL_SEED;
    let noise = |seed: &mut u32| {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from((*seed >> 27) as i16 - 16)
    };
    let bursts = match fixture {
        TriageFixture::SeparateBursts => &BURSTS[..],
        TriageFixture::OverlappingBursts => std::slice::from_ref(&MIXED_BURST),
        TriageFixture::BroadbandOnly => &[],
    };
    let mixed_start = MIXED_BURST.0;
    let mut iq = Vec::with_capacity(TRIAGE_FIXTURE_SAMPLES * 4);
    for index in 0..TRIAGE_FIXTURE_SAMPLES {
        let broadband_gain = if matches!(fixture, TriageFixture::SeparateBursts) {
            1.0
        } else {
            // Repeat the same noise block so whole-window scaling creates flat excess.
            if index.is_multiple_of(TRIAGE_FFT_SAMPLES) {
                seed = INITIAL_SEED;
            }
            if (STRONG_BROADBAND_START..STRONG_BROADBAND_START + BURST_SAMPLES).contains(&index) {
                80.0
            } else if (mixed_start..mixed_start + BURST_SAMPLES).contains(&index) {
                2.0
            } else {
                1.0
            }
        };
        let mut i = broadband_gain * noise(&mut seed);
        let mut q = broadband_gain * noise(&mut seed);
        for &(first_sample, center_offset_hz, amplitude) in bursts {
            if (first_sample..first_sample + BURST_SAMPLES).contains(&index) {
                let position = index - first_sample;
                let edge_distance = position.min(BURST_SAMPLES - position - 1).min(512);
                let gate = 0.5 - 0.5 * (TAU * edge_distance as f64 / 1024.0).cos();
                // Integrate alternating 320-sample frequency levels continuously.
                let cycle_position = position % 640;
                let keyed_phase = 32_000.0 * cycle_position.min(640 - cycle_position) as f64;
                let phase =
                    TAU * (f64::from(center_offset_hz) * index as f64 + keyed_phase) / 20_000_000.0;
                i += amplitude * gate * phase.cos();
                q += amplitude * gate * phase.sin();
            }
        }
        for component in [i, q] {
            iq.extend((component.round() as i16).to_le_bytes());
        }
    }
    iq
}

fn capture_triage_fixture(workspace: &Path, args: &[&str], fixture: TriageFixture) -> Vec<u8> {
    let iq = triage_fixture(fixture);
    fs::write(workspace.join("waveform.sc16q11"), &iq).unwrap();
    let output = run_sweep(workspace, ReceiverMode::Complete, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    iq
}

fn triage_series(workspace: &Path, output_directory: &str, args: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "triage-series",
            "--series-directory",
            "scan",
            "--output-directory",
        ])
        .arg(output_directory)
        .args(args)
        .output()
        .unwrap()
}

fn capture_triaged_series(workspace: &Path) -> Value {
    let iq = triage_fixture(TriageFixture::SeparateBursts);
    fs::write(workspace.join("waveform-pass-0001.sc16q11"), &iq).unwrap();
    let mut shifted = iq;
    shifted.rotate_right(TRIAGE_FFT_SAMPLES * 4);
    fs::write(workspace.join("waveform-pass-0002.sc16q11"), &shifted).unwrap();
    let mut args = TRIAGE_TARGETED_ARGS.to_vec();
    args.extend(["--passes", "3"]);
    let output = run_sweep(workspace, ReceiverMode::SeriesActivity, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = triage_series(workspace, "triage", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_json(&workspace.join("triage/series-triage.json"))
}

#[test]
fn triage_series_tracks_narrow_activity_by_pass_and_preserves_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let report = capture_triaged_series(workspace);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "offline_rf_sweep_series_triage");
    let series_bytes = fs::read(workspace.join("scan/sweep-series.json")).unwrap();
    assert_eq!(
        report["source_series_sha256"],
        format!("{:x}", Sha256::digest(&series_bytes))
    );
    assert_eq!(report["min_passes"], 2);
    let manifest: Value = serde_json::from_slice(&series_bytes).unwrap();
    let passes = report["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 3);
    let analyzed = TRIAGE_FIXTURE_SAMPLES / TRIAGE_FFT_SAMPLES * TRIAGE_FFT_SAMPLES;
    for (index, pass) in passes.iter().enumerate() {
        let pass_directory = format!("pass-{:04}", index + 1);
        assert_eq!(pass["directory"], pass_directory);
        assert_eq!(
            pass["source_sweep_sha256"],
            manifest["passes"][index]["sweep_sha256"]
        );
        let tunes = pass["tunes"].as_array().unwrap();
        assert_eq!(tunes.len(), 2);
        for (tune_index, tune) in tunes.iter().enumerate() {
            let tune_directory = format!("tune-{:04}", tune_index + 1);
            assert_eq!(tune["directory"], tune_directory);
            let requested = [910_000_000u64, 918_000_000][tune_index];
            let observed = requested.checked_add_signed([7, 2, -3][index]).unwrap();
            assert_eq!(tune["requested_center_hz"], requested);
            assert_eq!(tune["observed_center_hz"], observed);
            assert_eq!(tune["analyzed_samples"], analyzed);
            let iq = fs::read(
                workspace
                    .join("scan")
                    .join(&pass_directory)
                    .join(tune_directory)
                    .join("wideband.sc16q11"),
            )
            .unwrap();
            assert_eq!(
                tune["analyzed_iq_sha256"],
                format!("{:x}", Sha256::digest(&iq[..analyzed * 4]))
            );
        }
    }

    let bands = report["narrow_bands"].as_array().unwrap();
    for (start, offset_hz, _) in BURSTS {
        let frequency = 910_000_000u64
            .checked_add_signed(i64::from(offset_hz))
            .unwrap();
        let band = bands
            .iter()
            .find(|band| {
                (band["lower_hz"].as_u64().unwrap()..band["upper_hz"].as_u64().unwrap())
                    .contains(&frequency)
            })
            .unwrap_or_else(|| panic!("missing narrow activity at {frequency} Hz: {bands:?}"));
        assert_eq!(band["requested_center_hz"], 910_000_000);
        let contributions = band["passes"].as_array().unwrap();
        assert_eq!(contributions.len(), 2);
        for (index, pass) in contributions.iter().enumerate() {
            assert_eq!(pass["pass_index"], index + 1);
            assert!(pass["score_db"].as_f64().unwrap() >= 3.0);
            let window = &pass["narrow_window"];
            let midpoint = start + BURST_SAMPLES / 2 + index * TRIAGE_FFT_SAMPLES;
            assert!(
                (window["start_sample"].as_u64().unwrap() as usize
                    ..window["end_sample_exclusive"].as_u64().unwrap() as usize)
                    .contains(&midpoint)
            );
        }
    }
    let mean_power = report["mean_power_bands"].as_array().unwrap();
    assert!(!mean_power.is_empty());
    for band in mean_power {
        let contributors = band["passes"].as_array().unwrap();
        assert_eq!(contributors.len(), 2);
        for (index, contributor) in contributors.iter().enumerate() {
            assert_eq!(contributor["pass_index"], index + 1);
            assert!(contributor["score_db"].as_f64().unwrap() >= 6.0);
            assert!(contributor.get("narrow_window").is_none());
        }
    }

    let output = triage_series(workspace, "all-three", &["--min-passes", "3"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("all-three/series-triage.json"));
    assert_eq!(report["min_passes"], 3);
    assert_eq!(report["narrow_bands"], json!([]));
    assert_eq!(report["mean_power_bands"], json!([]));

    let receipt = workspace.join("scan/pass-0002/sweep.json");
    let mut bytes = fs::read(&receipt).unwrap();
    bytes.push(b'\n');
    fs::write(receipt, bytes).unwrap();
    let output = triage_series(workspace, "tampered", &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("sweep receipt SHA-256 differs from the series manifest")
    );
    assert!(!workspace.join("tampered").exists());
}

#[test]
fn series_activity_exposes_one_off_bursts_and_interval_power_within_caps() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let window = TRIAGE_FFT_SAMPLES;
    let shift = 6 * window;
    let mut burst = triage_fixture(TriageFixture::BroadbandOnly);

    // Repeat a quiet FFT window with a +5 MHz tone. The fixed noise floor
    // keeps tone leakage from marking an entire usable sideband as active.
    let tone_cycle = [600i16, 0, 0, 600, -600, 0, 0, -600];
    let tone: Vec<_> = burst[..window * 4]
        .as_chunks::<2>()
        .0
        .iter()
        .zip(tone_cycle.into_iter().cycle())
        .flat_map(|(&noise, tone)| (i16::from_le_bytes(noise) + tone).to_le_bytes())
        .cycle()
        .take(TRIAGE_FIXTURE_SAMPLES * 4)
        .collect();
    burst.rotate_right(shift * 4);
    fs::write(workspace.join("waveform-pass-0001.sc16q11"), &burst).unwrap();
    fs::write(workspace.join("waveform-pass-0002.sc16q11"), tone).unwrap();
    let mut args = TRIAGE_TARGETED_ARGS.to_vec();
    args.extend(["--passes", "2"]);
    let output = run_sweep(workspace, ReceiverMode::SeriesActivity, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let budget = (16 * window).to_string();
    let common = ["--max-samples-per-tune", &budget, "--time-slices", "2"];
    let inspect = |destination, options: &[&str]| {
        let mut args = common.to_vec();
        args.extend_from_slice(options);
        let output = triage_series(workspace, destination, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    inspect("legacy", &[]);
    inspect("activity", &["--max-activity-per-dwell", "1"]);
    let legacy = fs::read(workspace.join("legacy/series-triage.json")).unwrap();
    assert!(!workspace.join("legacy/activity.json").exists());
    assert_eq!(
        fs::read(workspace.join("activity/series-triage.json")).unwrap(),
        legacy
    );
    let activity = read_json(&workspace.join("activity/activity.json"));
    let manifest = fs::read(workspace.join("scan/sweep-series.json")).unwrap();
    assert_eq!(activity["schema_version"], 1);
    assert_eq!(activity["operation"], "offline_rf_sweep_series_activity");
    assert_eq!(
        activity["source_series_sha256"],
        format!("{:x}", Sha256::digest(manifest))
    );
    assert_eq!(
        activity["series_triage_sha256"],
        format!("{:x}", Sha256::digest(&legacy))
    );
    assert_eq!(activity["max_activity_per_dwell"], 1);
    let legacy: Value = serde_json::from_slice(&legacy).unwrap();
    let passes = activity["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 2);
    for (pass_index, pass) in passes.iter().enumerate() {
        let expected = &legacy["passes"][pass_index];
        assert_eq!(pass["directory"], expected["directory"]);
        let tunes = pass["tunes"].as_array().unwrap();
        assert_eq!(tunes.len(), 2);
        for (tune_index, tune) in tunes.iter().enumerate() {
            let expected = &expected["tunes"][tune_index];
            assert_eq!(tune["directory"], expected["directory"]);
            assert_eq!(tune["observed_center_hz"], expected["observed_center_hz"]);
            let intervals = expected["analyzed_intervals"].as_array().unwrap();
            for key in ["transients", "mean_power_regions"] {
                let evidence = tune[key].as_array().unwrap();
                assert!(evidence.len() <= 1);
                for row in evidence {
                    let start = row["start_sample"].as_u64().unwrap();
                    let end = row["end_sample_exclusive"].as_u64().unwrap();
                    assert!(start < end);
                    assert!(intervals.iter().any(|interval| {
                        interval["start_sample"].as_u64().unwrap() <= start
                            && end <= interval["end_sample_exclusive"].as_u64().unwrap()
                    }));
                }
            }
            assert!(tune["transient_detector_omitted_count"].is_u64());
        }
    }

    let burst_tune = &passes[0]["tunes"][0];
    assert!(burst_tune["transient_event_count"].as_u64().unwrap() >= 2);
    assert!(
        burst_tune["transient_eligible_retained_count"]
            .as_u64()
            .unwrap()
            >= 2
    );
    let event = &burst_tune["transients"][0];
    assert_eq!(event["start_sample"], STRONG_BROADBAND_START + shift);
    assert_eq!(
        event["end_sample_exclusive"],
        STRONG_BROADBAND_START + shift + BURST_SAMPLES
    );
    assert!(
        event["upper_frequency_hz_exclusive"].as_u64().unwrap()
            - event["lower_frequency_hz"].as_u64().unwrap()
            > 1_000_000
    );

    let tone_tune = &passes[1]["tunes"][0];
    assert_eq!(tone_tune["transient_event_count"], 0);
    assert!(tone_tune["mean_power_region_count"].as_u64().unwrap() >= 2);
    let region = &tone_tune["mean_power_regions"][0];
    assert!(
        (region["lower_frequency_hz"].as_u64().unwrap()
            ..region["upper_frequency_hz_exclusive"].as_u64().unwrap())
            .contains(&915_000_002)
    );
    assert_eq!(region["start_sample"], 0);
    assert_eq!(region["end_sample_exclusive"], 8 * window);

    verify_series_activity_follow_up(workspace, &activity, &legacy);

    let output = triage_series(workspace, "invalid", &["--max-activity-per-dwell", "17"]);
    assert!(!output.status.success());
    assert!(!workspace.join("invalid").exists());
}

fn verify_series_activity_follow_up(workspace: &Path, activity: &Value, triage: &Value) {
    let capture = workspace.join("scan");
    let activity_path = workspace.join("activity/activity.json");
    let triage_path = workspace.join("activity/series-triage.json");
    let arguments = [
        "sweep",
        "--series-triage-report",
        triage_path.to_str().unwrap(),
        "--series-activity-source",
        capture.to_str().unwrap(),
        "--activity-target",
        "2:1:mean-power:1",
        "--dwell-ms",
        "1",
        "--passes",
        "2",
        "--spectrum-output",
        "skip",
    ];
    let preview = |report: &Path, options: &[&str]| {
        Command::new(BINARY)
            .current_dir(workspace)
            .args(arguments)
            .arg("--series-activity-report")
            .arg(report)
            .args(options)
            .arg("--plan-only")
            .output()
            .unwrap()
    };

    let output = preview(&activity_path, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["planned_iq_bytes"], 320_000);
    let digest = format!("{:x}", Sha256::digest(fs::read(&activity_path).unwrap()));
    assert_eq!(plan["source_series_activity_sha256"], digest);
    assert_eq!(
        plan["source_series_sha256"],
        activity["source_series_sha256"]
    );
    assert_eq!(
        plan["source_series_triage_sha256"],
        activity["series_triage_sha256"]
    );
    let targets = plan["activity_follow_up"]["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    let target = &targets[0];
    assert_eq!(
        target["selection"],
        json!({"pass": 2, "tune": 1, "kind": "mean-power", "row": 1})
    );
    let tune = &activity["passes"][1]["tunes"][0];
    let region = &tune["mean_power_regions"][0];
    assert_eq!(target["source_center_hz"], tune["observed_center_hz"]);
    assert_eq!(target["start_sample"], region["start_sample"]);
    assert_eq!(
        target["end_sample_exclusive"],
        region["end_sample_exclusive"]
    );
    assert_eq!(target["lower_hz"], region["lower_frequency_hz"]);
    assert_eq!(target["upper_hz"], region["upper_frequency_hz_exclusive"]);
    let lower_hz = target["lower_hz"].as_u64().unwrap();
    let upper_hz = target["upper_hz"].as_u64().unwrap();
    let centers = target["centers_hz"].as_array().unwrap();
    assert_eq!(centers.len(), 2);
    assert_eq!(&plan["centers_hz"], &target["centers_hz"]);
    for center in centers {
        let center = center.as_u64().unwrap();
        for observed in [center - 10, center + 10] {
            assert!(observed.abs_diff(915_000_002) >= 100_000);
            assert!(observed.abs_diff(910_000_002) >= 500_000);
            let fits_above = observed + 100_000 <= lower_hz && upper_hz <= observed + 8_000_000;
            let fits_below = observed - 8_000_000 <= lower_hz && upper_hz <= observed - 100_000;
            assert!(fits_above || fits_below);
        }
    }

    let mut forged = activity.clone();
    let intervals = &triage["passes"][1]["tunes"][0]["analyzed_intervals"];
    let row = &mut forged["passes"][1]["tunes"][0]["mean_power_regions"][0];
    row["end_sample_exclusive"] = intervals[1]["start_sample"].clone();
    let forged_path = workspace.join("bridged-activity.json");
    fs::write(&forged_path, serde_json::to_vec(&forged).unwrap()).unwrap();
    assert!(!preview(&forged_path, &[]).status.success());
    let output = preview(&activity_path, &["--max-iq-bytes", "319999"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("budget"));

    let change_sample = |dwell: &str, sample: u64| {
        let path = capture.join(dwell).join("wideband.sc16q11");
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .write_all_at(&[1, 0, 0, 0], sample * 4)
            .unwrap();
    };
    change_sample("pass-0001/tune-0002", 0);
    let output = preview(&activity_path, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        plan
    );

    let follow_up = tempfile::tempdir().unwrap();
    let mut capture_args = arguments.to_vec();
    capture_args.extend(["--series-activity-report", activity_path.to_str().unwrap()]);
    let output = run_sweep(follow_up.path(), ReceiverMode::Timed, &capture_args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let follow_up = follow_up.path().join("scan");
    assert_eq!(read_json(&follow_up.join("plan.json")), plan);
    for path in [
        "sweep-series.json",
        "pass-0001/sweep.json",
        "pass-0002/sweep.json",
    ] {
        let receipt = read_json(&follow_up.join(path));
        for key in [
            "source_series_activity_sha256",
            "source_series_sha256",
            "source_series_triage_sha256",
        ] {
            assert_eq!(receipt[key], plan[key]);
        }
    }
    change_sample("pass-0002/tune-0001", 0);
    let output = preview(&activity_path, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SHA-256"), "{stderr}");
}

#[test]
fn paired_series_follow_up_reuses_centers_and_binds_both_reports() {
    let source = tempfile::tempdir().unwrap();
    let primary = capture_triaged_series(source.path());
    let output = triage_series(source.path(), "overlap", &["--overlap-events"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let primary_path = source.path().join("triage/series-triage.json");
    let overlap_path = source.path().join("overlap/series-triage.json");
    let overlap = read_json(&overlap_path);
    let primary_bands = primary["narrow_bands"].as_array().unwrap();
    let overlap_bands = overlap["narrow_bands"].as_array().unwrap();
    let row_key = |band: &Value| {
        (
            band["requested_center_hz"].as_u64().unwrap(),
            band["lower_hz"].as_u64().unwrap(),
            band["upper_hz"].as_u64().unwrap(),
        )
    };
    let unique = primary_bands
        .iter()
        .chain(overlap_bands)
        .map(row_key)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(unique.len() < primary_bands.len() + overlap_bands.len());

    let destination = tempfile::tempdir().unwrap();
    let workspace = destination.path();
    let sweep_args = [
        "sweep",
        "--series-triage-report",
        primary_path.to_str().unwrap(),
        "--overlap-series-triage-report",
        overlap_path.to_str().unwrap(),
        "--dwell-ms",
        "1",
        "--passes",
        "2",
        "--spectrum-output",
        "skip",
    ];
    let preview = |args: &[&str]| {
        Command::new(BINARY)
            .current_dir(workspace)
            .args(args)
            .arg("--plan-only")
            .output()
            .unwrap()
    };
    let output = preview(&sweep_args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let companion_digest = format!("{:x}", Sha256::digest(fs::read(&overlap_path).unwrap()));
    assert_eq!(
        plan["source_series_triage_overlap_sha256"],
        companion_digest
    );
    let follow_up = &plan["series_follow_up"];
    assert_eq!(follow_up["unplaced_rows"], json!([]));
    let targets = follow_up["targets"].as_array().unwrap();
    assert_eq!(targets.len(), unique.len());
    assert_eq!(
        follow_up["reported_band_count"],
        primary_bands.len() + overlap_bands.len()
    );

    let mut centers = std::collections::BTreeSet::new();
    for target in targets {
        let key = (
            target["requested_center_hz"].as_u64().unwrap(),
            target["nominal_lower_hz"].as_u64().unwrap(),
            target["nominal_upper_hz"].as_u64().unwrap(),
        );
        let primary_index = primary_bands.iter().position(|row| row_key(row) == key);
        let overlap_index = overlap_bands.iter().position(|row| row_key(row) == key);
        let combined_row = primary_index.unwrap_or_else(|| {
            primary_bands.len() + overlap_index.expect("target needs a source row")
        });
        assert_eq!(target["source_row"], combined_row + 1);
        if let Some(index) = overlap_index {
            assert_eq!(target["overlap_source_row"], index + 1);
        } else {
            assert!(target.get("overlap_source_row").is_none());
        }
        let support = primary_index
            .map(|index| &primary_bands[index])
            .into_iter()
            .chain(overlap_index.map(|index| &overlap_bands[index]))
            .flat_map(|band| band["passes"].as_array().unwrap())
            .map(|pass| pass["pass_index"].as_u64().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(target["support_count"], support.len());
        centers.extend(
            target["centers_hz"]
                .as_array()
                .unwrap()
                .iter()
                .map(|center| center.as_u64().unwrap()),
        );
    }
    assert_eq!(plan["centers_hz"], json!(centers));
    assert_eq!(plan["planned_iq_bytes"], centers.len() * 2 * 80_000);
    let mut standalone_centers = 0;
    for path in [&primary_path, &overlap_path] {
        let output = preview(&["sweep", "--series-triage-report", path.to_str().unwrap()]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let standalone: Value = serde_json::from_slice(&output.stdout).unwrap();
        standalone_centers += standalone["centers_hz"].as_array().unwrap().len();
    }
    assert!(centers.len() < standalone_centers);

    let mismatched = source.path().join("different-pass.json");
    let mut different = overlap;
    different["passes"][0]["source_sweep_sha256"] = json!("0".repeat(64));
    fs::write(&mismatched, serde_json::to_vec(&different).unwrap()).unwrap();
    let mut mismatched_args = sweep_args;
    mismatched_args[4] = mismatched.to_str().unwrap();
    assert!(!preview(&mismatched_args).status.success());

    let output = run_sweep_on_antenna(workspace, ReceiverMode::Timed, &sweep_args, 1);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read_json(&workspace.join("scan/plan.json")), plan);
    for receipt in [
        "scan/sweep-series.json",
        "scan/pass-0001/sweep.json",
        "scan/pass-0002/sweep.json",
    ] {
        assert_eq!(
            read_json(&workspace.join(receipt))["source_series_triage_overlap_sha256"],
            companion_digest
        );
    }
    let output = analyze_follow_up(workspace, &primary_path, "missing", &[]);
    assert!(!output.status.success());
    assert!(!workspace.join("missing").exists());
    let output = analyze_follow_up(
        workspace,
        &primary_path,
        "analysis",
        &["--overlap-series-triage-report", sweep_args[4]],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let analysis = read_json(&workspace.join("analysis/follow-up-analysis.json"));
    assert_eq!(
        analysis["source_series_triage_overlap_sha256"],
        companion_digest
    );
    let analyzed = analysis["targets"].as_array().unwrap();
    assert_eq!(analyzed.len(), targets.len());
    for (actual, planned) in analyzed.iter().zip(targets) {
        assert_eq!(actual["source_row"], planned["source_row"]);
        assert_eq!(actual["overlap_source_row"], planned["overlap_source_row"]);
        assert_eq!(actual["observations"].as_array().unwrap().len(), 4);
    }
}

#[test]
fn series_triage_follow_up_previews_and_captures_supported_bands_on_rx1() {
    let source = tempfile::tempdir().unwrap();
    let report = capture_triaged_series(source.path());
    let report_path = source.path().join("triage/series-triage.json");
    let digest = format!("{:x}", Sha256::digest(fs::read(&report_path).unwrap()));
    let arguments = [
        "sweep",
        "--series-triage-report",
        report_path.to_str().unwrap(),
        "--dwell-ms",
        "1",
    ];
    let output = Command::new(BINARY)
        .current_dir(source.path())
        .args(arguments)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["operation"], "targeted_rf_sweep_plan");
    assert_eq!(plan["source_series_triage_sha256"], digest);
    assert_eq!(plan["source_series_sha256"], report["source_series_sha256"]);
    let follow_up = &plan["series_follow_up"];
    assert_eq!(follow_up["narrow_band_count"], report["narrow_band_count"]);
    assert_eq!(follow_up["unplaced_rows"], json!([]));
    let bands = report["narrow_bands"].as_array().unwrap();
    assert_eq!(follow_up["reported_band_count"], bands.len());
    let targets = follow_up["targets"].as_array().unwrap();
    assert_eq!(targets.len(), bands.len());
    assert!(targets.len() >= BURSTS.len());
    let mut centers = Vec::new();
    for (index, (target, band)) in targets.iter().zip(bands).enumerate() {
        assert_eq!(target["source_row"], index + 1);
        assert_eq!(target["requested_center_hz"], band["requested_center_hz"]);
        assert_eq!(target["nominal_lower_hz"], band["lower_hz"]);
        assert_eq!(target["nominal_upper_hz"], band["upper_hz"]);
        assert_eq!(
            target["source_centers_hz"],
            json!([910_000_002, 910_000_007])
        );
        assert_eq!(target["support_count"], 2);
        let lower = target["lower_hz"].as_u64().unwrap();
        let upper = target["upper_hz"].as_u64().unwrap();
        let nominal_lower = band["lower_hz"].as_u64().unwrap();
        let nominal_upper = band["upper_hz"].as_u64().unwrap();
        assert_eq!(lower, nominal_lower);
        assert_eq!(upper, nominal_upper + 7);
        let frequency = target["target_hz"].as_u64().unwrap();
        assert_eq!(frequency, nominal_lower.midpoint(nominal_upper));
        let pair = target["centers_hz"].as_array().unwrap();
        assert_eq!(pair.len(), 2);
        let offsets = [pair[0].as_u64().unwrap(), pair[1].as_u64().unwrap()];
        assert!(offsets[0] + 500_020 <= offsets[1]);
        for center in offsets {
            // Both extremes of the receiver's 10 Hz readback tolerance must fit.
            assert!(center - 7_999_990 <= lower && upper <= center + 7_999_990);
            assert!(upper <= center - 100_010 || center + 100_010 <= lower);
            assert!(frequency.abs_diff(center) >= 500_010);
            assert!(center + 500_010 <= 910_000_002 || center >= 910_500_017);
        }
        centers.extend(offsets);
    }
    centers.sort_unstable();
    centers.dedup();
    assert_eq!(plan["centers_hz"], json!(centers));
    let budget = centers.len() * 80_000;
    assert_eq!(plan["planned_iq_bytes"], budget);

    let destination = tempfile::tempdir().unwrap();
    let smaller_budget = (budget - 1).to_string();
    let mut under_budget = arguments.to_vec();
    under_budget.extend(["--max-iq-bytes", &smaller_budget]);
    let output = run_sweep_on_antenna(destination.path(), ReceiverMode::Complete, &under_budget, 1);
    let scan = destination.path().join("scan");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("byte budget"));
    assert!(!scan.exists());

    let mut forged = report;
    let witness = &mut forged["narrow_bands"][0]["passes"][0]["narrow_window"];
    witness["end_sample_exclusive"] = witness["start_sample"].clone();
    let forged_path = source.path().join("triage/forged.json");
    serde_json::to_writer(File::create(&forged_path).unwrap(), &forged).unwrap();
    let mut forged_args = arguments;
    forged_args[2] = forged_path.to_str().unwrap();
    let output = run_sweep_on_antenna(destination.path(), ReceiverMode::Complete, &forged_args, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("witness"), "{stderr}");
    assert!(!scan.exists());

    let output = run_sweep_on_antenna(destination.path(), ReceiverMode::Complete, &arguments, 1);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    session_log(&scan);
    assert_eq!(read_json(&scan.join("plan.json")), plan);
    let receipt = read_json(&scan.join("sweep.json"));
    assert_eq!(receipt["operation"], "targeted_rf_sweep");
    assert_eq!(receipt["source_series_triage_sha256"], digest);
    assert_eq!(
        receipt["source_series_sha256"],
        plan["source_series_sha256"]
    );
    assert_eq!(receipt["antenna"], 1);
    assert_eq!(receipt["iq_bytes"], budget);
    let tunes = receipt["tunes"].as_array().unwrap();
    assert_eq!(tunes.len(), centers.len());
    for (index, (tune, center)) in tunes.iter().zip(centers).enumerate() {
        assert_eq!(tune["requested_center_hz"], center);
        assert_validated_tune(&scan.join(format!("tune-{:04}", index + 1)), center, 1);
    }
}

const FOLLOW_UP_FFT_SAMPLES: usize = 1024;
const FOLLOW_UP_BURSTS: [(usize, usize); 2] = [(4, 6), (22, 31)];

fn follow_up_iq(center: u64, target: u64) -> Vec<u8> {
    let offset_hz = target as f64 - center as f64;
    let bin = (offset_hz * FOLLOW_UP_FFT_SAMPLES as f64 / 20_000_000.0).round();
    let mut state = 108_564u32;
    let mut noise = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from((state >> 27) as i16 - 16)
    };
    let mut quiet = Vec::with_capacity(4 * FOLLOW_UP_FFT_SAMPLES);
    let mut burst = Vec::with_capacity(4 * FOLLOW_UP_FFT_SAMPLES);
    // Each window has the same positive baseline, including every companion band.
    for sample in 0..FOLLOW_UP_FFT_SAMPLES {
        let phase = TAU * bin * sample as f64 / FOLLOW_UP_FFT_SAMPLES as f64;
        for component in [phase.cos(), phase.sin()] {
            let background = noise();
            quiet.extend((background as i16).to_le_bytes());
            burst.extend(((background + 256.0 * component).round() as i16).to_le_bytes());
        }
    }
    let mut iq = Vec::with_capacity(TRIAGE_FIXTURE_SAMPLES * 4);
    for window in 0..TRIAGE_FIXTURE_SAMPLES / FOLLOW_UP_FFT_SAMPLES {
        let gated = FOLLOW_UP_BURSTS
            .iter()
            .any(|&(start, end)| (start..end).contains(&window));
        iq.extend(if gated { &burst } else { &quiet });
    }
    iq.extend(&quiet[..(TRIAGE_FIXTURE_SAMPLES % FOLLOW_UP_FFT_SAMPLES) * 4]);
    iq
}

fn analyze_follow_up(workspace: &Path, source: &Path, output: &str, extra: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "analyze-follow-up",
            "--capture-directory",
            "scan",
            "--series-triage-report",
        ])
        .arg(source)
        .args(["--output-directory", output])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn follow_up_analysis_binds_isolated_runs_to_exact_source_dwells() {
    let source = tempfile::tempdir().unwrap();
    let reference = capture_triaged_series(source.path());
    let report_path = source.path().join("triage/series-triage.json");
    let destination = tempfile::tempdir().unwrap();
    let workspace = destination.path();
    let args = [
        "sweep",
        "--series-triage-report",
        report_path.to_str().unwrap(),
        "--dwell-ms",
        "12",
        "--passes",
        "2",
    ];
    let preview = Command::new(BINARY)
        .current_dir(workspace)
        .args(args)
        .arg("--plan-only")
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    let target = &plan["series_follow_up"]["targets"][0];
    let target_hz = target["target_hz"].as_u64().unwrap();
    let pair = target["centers_hz"].as_array().unwrap();
    for (index, center) in plan["centers_hz"].as_array().unwrap().iter().enumerate() {
        if pair.contains(center) {
            let iq = follow_up_iq(center.as_u64().unwrap() + 7, target_hz);
            fs::write(
                workspace.join(format!("waveform-tune-{:04}.sc16q11", index + 1)),
                iq,
            )
            .unwrap();
        }
    }
    let output = run_sweep_on_antenna(workspace, ReceiverMode::Timed, &args, 1);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = analyze_follow_up(workspace, &report_path, "analysis", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&workspace.join("analysis/follow-up-analysis.json"));
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "offline_rf_series_follow_up_analysis");
    for (field, file) in [
        (
            "source_capture_sha256",
            workspace.join("scan/sweep-series.json"),
        ),
        ("source_plan_sha256", workspace.join("scan/plan.json")),
        ("source_series_triage_sha256", report_path.clone()),
    ] {
        assert_eq!(
            report[field],
            format!("{:x}", Sha256::digest(fs::read(file).unwrap()))
        );
    }
    assert_eq!(
        report["source_series_sha256"],
        reference["source_series_sha256"]
    );
    let passes = report["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 2);
    let analyzed = TRIAGE_FIXTURE_SAMPLES;
    for (index, pass) in passes.iter().enumerate() {
        let name = format!("pass-{:04}", index + 1);
        let pass_path = workspace.join("scan").join(name);
        let tunes = pass["tunes"].as_array().unwrap();
        assert_eq!(tunes.len(), plan["centers_hz"].as_array().unwrap().len());
        for (tune_index, tune) in tunes.iter().enumerate() {
            let file = pass_path.join(format!("tune-{:04}/wideband.sc16q11", tune_index + 1));
            let iq = fs::read(file).unwrap();
            assert_eq!(tune["analyzed_samples"], analyzed);
            assert_eq!(
                tune["analyzed_iq_sha256"],
                format!("{:x}", Sha256::digest(&iq[..4 * analyzed]))
            );
        }
    }
    let targets = report["targets"].as_array().unwrap();
    assert_eq!(
        targets.len(),
        plan["series_follow_up"]["targets"]
            .as_array()
            .unwrap()
            .len()
    );
    let selected = targets
        .iter()
        .find(|row| row["source_row"] == target["source_row"])
        .unwrap();
    assert_eq!(selected["centers_with_isolated_runs"], target["centers_hz"]);
    let lower = target["lower_hz"].as_u64().unwrap();
    let upper = target["upper_hz"].as_u64().unwrap();
    let width = upper - lower;
    let source_center = target["requested_center_hz"].as_u64().unwrap();
    let observations = selected["observations"].as_array().unwrap();
    assert_eq!(observations.len(), 4);
    for (index, observation) in observations.iter().enumerate() {
        assert_eq!(observation["pass_index"], index / 2 + 1);
        let center = pair[index % 2].as_u64().unwrap();
        let observed = center + 7;
        assert_eq!(observation["requested_center_hz"], center);
        assert_eq!(observation["observed_center_hz"], observed);
        let controls = observation["controls"].as_array().unwrap();
        let shift = observed as i64 - source_center as i64;
        let expected_controls = [
            ("lower_flank", lower - width, lower),
            ("upper_flank", upper, upper + width),
            (
                "source_if",
                lower.checked_add_signed(shift).unwrap(),
                upper.checked_add_signed(shift).unwrap(),
            ),
            ("mirror", 2 * observed - upper, 2 * observed - lower),
        ];
        assert_eq!(controls.len(), expected_controls.len());
        for (control, (kind, a, b)) in controls.iter().zip(expected_controls) {
            assert_eq!(control["kind"], kind);
            assert_eq!(control["lower_hz"], a);
            assert_eq!(control["upper_hz"], b);
            assert_eq!(control["status"], "measured");
            assert!(control["baseline_band_power"].as_f64().unwrap() > 0.0);
            assert!(control["integrated_fft_bins"].as_u64().unwrap() > 0);
        }
        assert_eq!(observation["target_detections"], FOLLOW_UP_BURSTS.len());
        let isolated_runs = observation["isolated_runs"].as_array().unwrap();
        assert_eq!(isolated_runs.len(), FOLLOW_UP_BURSTS.len());
        for (run, (start, end)) in isolated_runs.iter().zip(FOLLOW_UP_BURSTS) {
            assert_eq!(run["start_sample"], start * FOLLOW_UP_FFT_SAMPLES);
            assert_eq!(run["end_sample_exclusive"], end * FOLLOW_UP_FFT_SAMPLES);
            assert!(run["peak_excess_db"].as_f64().unwrap() >= 6.0);
        }
    }
    let blank = targets
        .iter()
        .flat_map(|row| row["observations"].as_array().unwrap())
        .find(|observation| !pair.contains(&observation["requested_center_hz"]))
        .unwrap();
    assert_eq!(blank["target_detections"], 0);
    assert!(blank["isolated_runs"].is_null());
    assert!(
        blank["controls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|control| control["status"] == "zero_baseline")
    );

    verify_follow_up_waveform_probes(workspace, &report_path, &report, selected);

    let mut bytes = fs::read(&report_path).unwrap();
    bytes.push(b'\n');
    let forged = source.path().join("triage/forged-follow-up.json");
    fs::write(&forged, bytes).unwrap();
    let output = analyze_follow_up(workspace, &forged, "tampered", &["--waveform-probes", "3"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("saved follow-up plan differs from the rebuilt plan")
    );
    assert!(!workspace.join("tampered").exists());
}

fn verify_follow_up_waveform_probes(
    workspace: &Path,
    report_path: &Path,
    report: &Value,
    selected: &Value,
) {
    let output = analyze_follow_up(
        workspace,
        report_path,
        "waveforms",
        &["--waveform-probes", "3"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut measured = read_json(&workspace.join("waveforms/follow-up-analysis.json"));
    let waveforms = measured
        .as_object_mut()
        .unwrap()
        .remove("waveform_analysis")
        .unwrap();
    assert_eq!(&measured, report);
    assert_eq!(waveforms["max_probes"], 3);
    assert_eq!(waveforms["eligible_runs"], 4);
    assert_eq!(waveforms["omitted_eligible_runs"], 1);
    assert_eq!(waveforms["too_short_runs"], 4);
    assert_eq!(waveforms["missing_filter_context_runs"], 0);
    let probes = waveforms["probes"].as_array().unwrap();
    assert_eq!(probes.len(), 3);
    let (start, end) = FOLLOW_UP_BURSTS[1];
    let start = start * FOLLOW_UP_FFT_SAMPLES;
    let end = end * FOLLOW_UP_FFT_SAMPLES;
    let observations = selected["observations"].as_array().unwrap();
    let passes = report["passes"].as_array().unwrap();
    for (probe, observation) in probes.iter().zip(observations) {
        let pass_index = probe["pass_index"].as_u64().unwrap() as usize;
        let pass = &passes[pass_index - 1];
        let tune = pass["tunes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tune| tune["requested_center_hz"] == probe["requested_center_hz"])
            .unwrap();
        let iq = fs::read(
            workspace
                .join("scan")
                .join(pass["directory"].as_str().unwrap())
                .join(tune["directory"].as_str().unwrap())
                .join("wideband.sc16q11"),
        )
        .unwrap();
        assert_eq!(probe["source_row"], selected["source_row"]);
        assert_eq!(probe["pass_index"], observation["pass_index"]);
        assert_eq!(
            probe["requested_center_hz"],
            observation["requested_center_hz"]
        );
        assert_eq!(probe["run_start_sample"], start);
        assert_eq!(probe["run_end_sample_exclusive"], end);
        assert_eq!(probe["measured_start_sample"], start);
        assert_eq!(probe["measured_end_sample_exclusive"], end);
        assert_eq!(probe["clip_start_sample"], start - 400);
        assert_eq!(probe["clip_end_sample_exclusive"], end + 400);
        assert_eq!(
            probe["clip_sha256"],
            format!(
                "{:x}",
                Sha256::digest(&iq[4 * (start - 400)..4 * (end + 400)])
            )
        );
        assert!(probe["evidence"]["phase_order_hint"].is_null());
    }

    let output = analyze_follow_up(
        workspace,
        report_path,
        "short-prefix",
        &["--waveform-probes", "3", "--max-samples-per-dwell", "31744"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let capped = read_json(&workspace.join("short-prefix/follow-up-analysis.json"));
    let waveforms = &capped["waveform_analysis"];
    assert_eq!(waveforms["eligible_runs"], 0);
    assert_eq!(waveforms["too_short_runs"], 4);
    assert_eq!(waveforms["missing_filter_context_runs"], 4);
    assert!(waveforms["probes"].as_array().unwrap().is_empty());
}

fn triage(workspace: &Path, output_directory: &str, args: &[&str]) -> Output {
    Command::new(BINARY)
        .current_dir(workspace)
        .args([
            "triage-sweep",
            "--sweep-directory",
            "scan",
            "--output-directory",
        ])
        .arg(output_directory)
        .args(args)
        .output()
        .unwrap()
}

fn triage_report(workspace: &Path, output_directory: &str, args: &[&str]) -> Value {
    let output = triage(workspace, output_directory, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    read_json(&workspace.join(output_directory).join("triage.json"))
}

fn includes_burst(clip: &Value, center_hz: u64, burst: (usize, i32, f64)) -> bool {
    let (first_sample, offset_hz, _) = burst;
    let frequency_hz = center_hz.checked_add_signed(i64::from(offset_hz)).unwrap();
    (clip["lower_hz"].as_u64().unwrap()..clip["upper_hz"].as_u64().unwrap()).contains(&frequency_hz)
        && (clip["start_sample"].as_u64().unwrap() as usize
            ..clip["end_sample_exclusive"].as_u64().unwrap() as usize)
            .contains(&(first_sample + BURST_SAMPLES / 2))
}

#[test]
fn overlapping_sweep_triage_finds_a_short_boundary_burst_on_the_same_prefix() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path();
    let window = TRIAGE_FFT_SAMPLES;
    let stride = window / 2;
    let boundary = 2 * window;
    let tone_bin = 900.0;
    let mut iq = triage_fixture(TriageFixture::SeparateBursts);
    for (sample, pair) in iq
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .enumerate()
        .skip(boundary - 63)
        .take(127)
    {
        let phase = TAU * tone_bin * (sample % window) as f64 / window as f64;
        let (sine, cosine) = phase.sin_cos();
        for (component, tone) in pair.as_chunks_mut::<2>().0.iter_mut().zip([cosine, sine]) {
            let background = i16::from_le_bytes(*component);
            *component = (background + (700.0 * tone).round() as i16).to_le_bytes();
        }
    }
    fs::write(workspace.join("waveform.sc16q11"), &iq).unwrap();
    let output = run_sweep(workspace, ReceiverMode::Complete, TRIAGE_TARGETED_ARGS);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let analyzed = 8 * window;
    let cap = (analyzed + 11).to_string();
    let baseline = triage_report(workspace, "baseline", &["--max-samples-per-tune", &cap]);
    let overlapping = triage_report(
        workspace,
        "overlapping",
        &["--max-samples-per-tune", &cap, "--overlap-events"],
    );
    let frequency = 910_000_007.0 + tone_bin * 20_000_000.0 / window as f64;
    let covers_burst = |clip: &Value| {
        clip["event_start_sample"].as_u64().unwrap() <= boundary as u64
            && clip["event_end_sample_exclusive"].as_u64().unwrap() > boundary as u64
            && clip["lower_hz"].as_f64().unwrap() <= frequency
            && clip["upper_hz"].as_f64().unwrap() > frequency
    };
    assert!(
        !baseline["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(covers_burst)
    );
    let (index, clip) = overlapping["clips"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .find(|(_, clip)| covers_burst(clip))
        .unwrap();
    assert_eq!(clip["peak_window_start_sample"], boundary - stride);
    let start = clip["start_sample"].as_u64().unwrap() as usize;
    let end = clip["end_sample_exclusive"].as_u64().unwrap() as usize;
    assert!(end <= analyzed);
    assert_eq!(
        fs::read(
            workspace
                .join("overlapping")
                .join(clip["file"].as_str().unwrap())
        )
        .unwrap(),
        iq[start * 4..end * 4]
    );
    assert_eq!(baseline["schema_version"], 2);
    assert!(baseline.get("event_window_stride_samples").is_none());
    assert_eq!(overlapping["schema_version"], 4);
    assert_eq!(overlapping["event_window_stride_samples"], stride);
    assert!(overlapping.get("time_slices").is_none());
    let expected_digest = format!("{:x}", Sha256::digest(&iq[..analyzed * 4]));
    for report in [&baseline, &overlapping] {
        assert_eq!(report["tunes"][0]["analyzed_samples"], analyzed);
        assert_eq!(report["tunes"][0]["analyzed_iq_sha256"], expected_digest);
    }

    let report = workspace.join("overlapping/triage.json");
    let output = preview_triage_follow_up(workspace, &report, 1_000_000);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut forged = overlapping;
    forged["clips"][index]["peak_window_start_sample"] = json!(boundary - stride + 1);
    let report = workspace.join("forged-overlap.json");
    fs::write(&report, serde_json::to_vec(&forged).unwrap()).unwrap();
    let output = preview_triage_follow_up(workspace, &report, 1_000_000);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("peak-window sample extent"));
}

#[test]
fn distributed_triage_finds_late_bursts_with_the_same_sample_budget() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let window = TRIAGE_FFT_SAMPLES;
    let shift = 4 * window;
    let mut iq = triage_fixture(TriageFixture::SeparateBursts);
    iq.rotate_right(shift * 4);
    for pass in 1..=2 {
        fs::write(
            workspace.join(format!("waveform-pass-{pass:04}.sc16q11")),
            &iq,
        )
        .unwrap();
    }
    let mut args = TRIAGE_TARGETED_ARGS.to_vec();
    args.extend(["--passes", "3", "--dwell-at", "918000000:13"]);
    let output = run_sweep(workspace, ReceiverMode::SeriesActivity, &args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let budget = (16 * window).to_string();
    let inspect = |destination, options: &[&str]| {
        let output = Command::new(BINARY)
            .current_dir(workspace)
            .args([
                "triage-sweep",
                "--sweep-directory",
                "scan/pass-0001",
                "--output-directory",
                destination,
                "--max-samples-per-tune",
                &budget,
            ])
            .args(options)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        read_json(&workspace.join(destination).join("triage.json"))
    };
    let prefix = inspect("prefix", &[]);
    let distributed = inspect("distributed", &["--time-slices", "2"]);
    assert_eq!(prefix["schema_version"], 2);
    assert_eq!(distributed["schema_version"], 3);
    assert_eq!(distributed["time_slices"], 2);
    let prefix_tune = &prefix["tunes"][0];
    let distributed_tune = &distributed["tunes"][0];
    assert_eq!(prefix_tune["analyzed_samples"], 16 * window);
    assert_eq!(distributed_tune["analyzed_samples"], 16 * window);
    assert!(distributed_tune.get("analyzed_iq_sha256").is_none());
    let ranges = [0..8 * window, 21 * window..29 * window];
    let evidence = distributed_tune["analyzed_intervals"].as_array().unwrap();
    assert_eq!(evidence.len(), ranges.len());
    for (actual, expected) in evidence.iter().zip(&ranges) {
        assert_eq!(actual["start_sample"], expected.start);
        assert_eq!(actual["end_sample_exclusive"], expected.end);
        assert_eq!(
            actual["analyzed_iq_sha256"],
            format!(
                "{:x}",
                Sha256::digest(&iq[expected.start * 4..expected.end * 4])
            )
        );
    }

    let burst = (BURSTS[1].0 + shift, BURSTS[1].1, BURSTS[1].2);
    let is_late_burst = |clip: &Value| includes_burst(clip, 910_000_007, burst);
    assert!(
        !prefix["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(is_late_burst)
    );
    let late = distributed["clips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|clip| is_late_burst(clip) && clip["waveform"]["status"] == "measured")
        .unwrap();
    let start = late["start_sample"].as_u64().unwrap() as usize;
    let end = late["end_sample_exclusive"].as_u64().unwrap() as usize;
    assert!(start >= ranges[1].start && end <= ranges[1].end);
    let clip_path = workspace
        .join("distributed")
        .join(late["file"].as_str().unwrap());
    assert_eq!(fs::read(clip_path).unwrap(), iq[start * 4..end * 4]);
    let report_path = workspace.join("distributed/triage.json");
    let output = preview_triage_follow_up(workspace, &report_path, 1_000_000);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut bridged = distributed.clone();
    let clip = late["file"].as_str().unwrap();
    let bridged_clip = bridged["clips"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|item| item["file"] == clip)
        .unwrap();
    bridged_clip["start_sample"] = json!(ranges[0].end - 1);
    let forged_clip_path = workspace.join("forged-clip.json");
    fs::write(&forged_clip_path, serde_json::to_vec(&bridged).unwrap()).unwrap();
    let output = preview_triage_follow_up(workspace, &forged_clip_path, 1_000_000);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("clip") && error.contains("sample extent"),
        "{error}"
    );

    let series_args = [
        "--max-samples-per-tune",
        &budget,
        "--time-slices",
        "2",
        "--max-activity-per-dwell",
        "4",
    ];
    let output = triage_series(workspace, "distributed-series", &series_args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = "distributed-series/series-triage.json";
    let report = read_json(&workspace.join(path));
    assert_eq!(report["schema_version"], 2);
    assert!(report.get("visit_centers_hz").is_none());
    assert_eq!(report["passes"][0]["tunes"][1]["saved_samples"], 260_000);
    assert_eq!(
        report["passes"][0]["tunes"][0]["analyzed_intervals"],
        distributed_tune["analyzed_intervals"]
    );
    let bands = report["narrow_bands"].as_array().unwrap();
    let band = bands
        .iter()
        .find(|band| {
            band["requested_center_hz"] == 910_000_000
                && (band["lower_hz"].as_u64().unwrap()..band["upper_hz"].as_u64().unwrap())
                    .contains(&915_250_000)
        })
        .unwrap();
    assert_eq!(
        band["passes"].as_array().unwrap().len(),
        2,
        "support counts distinct passes"
    );
    for pass in band["passes"].as_array().unwrap() {
        assert!(pass["narrow_window"]["start_sample"].as_u64().unwrap() >= ranges[1].start as u64);
    }
    let preview = |report: &str| {
        Command::new(BINARY)
            .current_dir(workspace)
            .args(["sweep", "--series-triage-report", report, "--plan-only"])
            .output()
            .unwrap()
    };

    let mut overlap_args = series_args.to_vec();
    overlap_args.push("--overlap-events");
    let output = triage_series(workspace, "overlap-series", &overlap_args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let overlap_path = "overlap-series/series-triage.json";
    let mut overlap = read_json(&workspace.join(overlap_path));
    assert_eq!(overlap["schema_version"], 3);
    assert_eq!(overlap["time_slices"], 2);
    assert_eq!(overlap["event_window_stride_samples"], window / 2);
    assert_eq!(
        overlap["passes"][0]["tunes"][0]["analyzed_intervals"],
        report["passes"][0]["tunes"][0]["analyzed_intervals"]
    );
    assert!(overlap["narrow_band_count"].as_u64().unwrap() > 0);

    let mut both_args = series_args.to_vec();
    both_args.push("--both-event-strides");
    let output = triage_series(workspace, "both-strides", &both_args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (standalone, suffix) in [("distributed-series", ""), ("overlap-series", "-overlap")] {
        for stem in ["series-triage", "activity"] {
            let combined = format!("both-strides/{stem}{suffix}.json");
            let reference = format!("{standalone}/{stem}.json");
            assert_eq!(
                fs::read(workspace.join(&combined)).unwrap(),
                fs::read(workspace.join(reference)).unwrap(),
                "{combined} differs from standalone output"
            );
        }
        let output = preview(&format!("both-strides/series-triage{suffix}.json"));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    both_args.push("--overlap-events");
    let output = triage_series(workspace, "conflicting-strides", &both_args);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot be used with"), "{error}");
    assert!(!workspace.join("conflicting-strides").exists());

    overlap["event_window_stride_samples"] = json!(window);
    let forged_path = "wrong-stride-series.json";
    fs::write(
        workspace.join(forged_path),
        serde_json::to_vec(&overlap).unwrap(),
    )
    .unwrap();
    let output = preview(forged_path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("event stride"));

    // A valid-frequency witness cannot claim a window in the skipped gap.
    let mut forged = report;
    forged["narrow_bands"][0]["passes"][0]["narrow_window"]["start_sample"] = json!(12 * window);
    forged["narrow_bands"][0]["passes"][0]["narrow_window"]["end_sample_exclusive"] =
        json!(14 * window);
    fs::write(
        workspace.join("forged-series.json"),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    let output = preview("forged-series.json");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("narrow") && error.contains("sample"),
        "{error}"
    );
}

#[test]
fn sweep_triage_preserves_observed_bursts_and_respects_clip_budget() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let iq = capture_triage_fixture(
        workspace,
        TRIAGE_TARGETED_ARGS,
        TriageFixture::SeparateBursts,
    );
    let report = triage_report(workspace, "triage", &[]);
    let saved_receipt = fs::read(workspace.join("scan/sweep.json")).unwrap();
    let source: Value = serde_json::from_slice(&saved_receipt).unwrap();
    let first_burst_hz = 904_750_007.0;
    assert!(first_burst_hz < source["lower_hz"].as_f64().unwrap());
    assert!(
        source["tunes"][0]["activity_regions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|region| {
                region["lower_frequency_hz"].as_f64().unwrap() <= first_burst_hz
                    && first_burst_hz < region["upper_frequency_hz"].as_f64().unwrap()
            })
    );
    assert_eq!(
        report["source_receipt_sha256"],
        format!("{:x}", Sha256::digest(saved_receipt))
    );
    let tunes = report["tunes"].as_array().unwrap();
    assert_eq!(tunes.len(), 2);
    assert_eq!(tunes[1]["event_count"], 0);
    let analyzed_samples = tunes[0]["analyzed_samples"].as_u64().unwrap() as usize;
    assert_eq!(analyzed_samples, 237_568);
    assert_eq!(
        tunes[0]["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..analyzed_samples * 4]))
    );

    let clips = report["clips"].as_array().unwrap();
    for burst in BURSTS {
        assert!(
            clips
                .iter()
                .any(|clip| includes_burst(clip, 910_000_007, burst))
        );
    }
    for clip in clips {
        assert_eq!(clip["tune_directory"], "tune-0001");
        let start = clip["start_sample"].as_u64().unwrap() as usize;
        let end = clip["end_sample_exclusive"].as_u64().unwrap() as usize;
        let path = workspace
            .join("triage")
            .join(clip["file"].as_str().unwrap());
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, iq[start * 4..end * 4]);
        assert_eq!(clip["sha256"], format!("{:x}", Sha256::digest(&bytes)));
        let sidecar = read_json(&path.with_extension("sc16q11.json"));
        assert_eq!(
            sidecar,
            json!({
                "format": "SC16_Q11",
                "dtype": "<i2",
                "interleave": "I,Q",
                "complex_scale": 2048,
                "frequency_hz": 910_000_007,
                "sample_rate_hz": 20_000_000,
                "bandwidth_hz": 20_000_000,
                "num_samples": end - start,
                "source_kind": "derived_sweep_triage_clip",
                "source_start_sample": start,
                "continuity_guaranteed": false,
            })
        );
        assert_eq!(clip["waveform"]["status"], "measured");
        assert_eq!(
            clip["waveform"]["evidence"]["frequency_plateaus"]["binary_fsk_hint"],
            true
        );
    }

    let capped = triage_report(workspace, "one-clip", &["--max-clips", "1"]);
    let clips = capped["clips"].as_array().unwrap();
    assert_eq!(clips.len(), 1);
    assert!(includes_burst(&clips[0], 910_000_007, BURSTS[1]));
}

#[test]
fn sweep_triage_retains_local_contrast_under_broadband_clip_pressure() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    capture_triage_fixture(
        workspace,
        TRIAGE_TARGETED_ARGS,
        TriageFixture::OverlappingBursts,
    );

    let peak_only = triage_report(
        workspace,
        "peak-only",
        &["--max-clips", "2", "--narrow-clips", "0"],
    );
    assert_eq!(peak_only["max_narrow_clips"], 0);
    let peaks = peak_only["clips"].as_array().unwrap();
    assert_eq!(peaks.len(), 2);
    assert!(
        peaks[0]["peak_excess_db"].as_f64().unwrap()
            >= peaks[1]["peak_excess_db"].as_f64().unwrap()
    );
    for clip in peaks {
        assert_eq!(clip["detector"], "connected_component");
        assert_eq!(clip["waveform"]["reason"], "event_wider_than_narrow_probe");
        let start = clip["event_start_sample"].as_u64().unwrap() as usize;
        let end = clip["event_end_sample_exclusive"].as_u64().unwrap() as usize;
        assert!((start..end).contains(&(STRONG_BROADBAND_START + TRIAGE_FFT_SAMPLES)));
    }

    let balanced = triage_report(workspace, "balanced", &["--max-clips", "2"]);
    assert_eq!(balanced["schema_version"], 2);
    assert_eq!(balanced["max_narrow_clips"], 1);
    let clips = balanced["clips"].as_array().unwrap();
    assert_eq!(clips.len(), 2);
    let broad = clips
        .iter()
        .find(|clip| clip["detector"] == "connected_component")
        .unwrap();
    assert_eq!(broad["sha256"], peaks[0]["sha256"]);
    assert_eq!(broad["lower_hz"], peaks[0]["lower_hz"]);
    assert_eq!(broad["upper_hz"], peaks[0]["upper_hz"]);
    let narrow = clips
        .iter()
        .find(|clip| clip["detector"] == "adjacent_contrast")
        .unwrap();
    assert!(includes_burst(narrow, 910_000_007, MIXED_BURST));
    assert!(narrow["narrow_contrast_db"].as_f64().unwrap() >= 3.0);
    assert_eq!(narrow["waveform"]["status"], "measured");
    assert_eq!(
        narrow["waveform"]["evidence"]["frequency_plateaus"]["binary_fsk_hint"],
        true
    );

    let spare = triage_report(
        workspace,
        "spare-narrow-slots",
        &["--max-clips", "8", "--narrow-clips", "7"],
    );
    let eligible = spare["tunes"][0]["narrow_search"]["eligible_retained_count"]
        .as_u64()
        .unwrap();
    let retained: Vec<_> = spare["clips"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|clip| clip["detector"] == "adjacent_contrast")
        .collect();
    assert!(eligible > 7);
    assert!((1..7).contains(&retained.len()));
    let width =
        |clip: &Value| clip["upper_hz"].as_u64().unwrap() - clip["lower_hz"].as_u64().unwrap();
    // The shifted versions of the leading band overlap enough to be duplicates.
    let matching_width: Vec<_> = retained
        .into_iter()
        .filter(|clip| width(clip) == width(narrow))
        .collect();
    assert_eq!(matching_width.len(), 1);
    assert_eq!(matching_width[0]["lower_hz"], narrow["lower_hz"]);

    let control = tempfile::tempdir().unwrap();
    capture_triage_fixture(
        control.path(),
        TRIAGE_TARGETED_ARGS,
        TriageFixture::BroadbandOnly,
    );
    let broad_only = triage_report(control.path(), "balanced", &["--max-clips", "2"]);
    assert_eq!(
        broad_only["tunes"][0]["narrow_search"]["candidate_count"],
        0
    );
    let clips = broad_only["clips"].as_array().unwrap();
    assert_eq!(clips.len(), 2);
    for clip in clips {
        assert_eq!(clip["detector"], "connected_component");
        assert_eq!(clip["waveform"]["reason"], "event_wider_than_narrow_probe");
    }
}

fn capture_triage_follow_up_source(workspace: &Path) -> Value {
    capture_triage_fixture(
        workspace,
        TRIAGE_TARGETED_ARGS,
        TriageFixture::OverlappingBursts,
    );
    triage_report(workspace, "triage", &["--max-clips", "2"])
}

fn preview_triage_follow_up(workspace: &Path, report: &Path, max_iq_bytes: u64) -> Output {
    Command::new(BINARY)
        .current_dir(workspace)
        .args(["sweep", "--triage-report"])
        .arg(report)
        .args(["--dwell-ms", "1", "--plan-only", "--max-iq-bytes"])
        .arg(max_iq_bytes.to_string())
        .output()
        .unwrap()
}

#[test]
fn triage_follow_up_previews_and_captures_an_offset_pair_on_rx1() {
    let source = tempfile::tempdir().unwrap();
    let report = capture_triage_follow_up_source(source.path());
    let report_path = source.path().join("triage/triage.json");
    let digest = Sha256::digest(fs::read(&report_path).unwrap());
    let relative_report = Path::new("triage/triage.json");
    let preview = preview_triage_follow_up(source.path(), relative_report, 160_000);
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["operation"], "targeted_rf_sweep_plan");
    assert_eq!(preview["source_triage_sha256"], format!("{digest:x}"));
    assert_eq!(
        preview["source_sweep_sha256"],
        report["source_receipt_sha256"]
    );
    assert_eq!(preview["planned_iq_bytes"], 160_000);
    let follow_up = &preview["triage_follow_up"];
    assert_eq!(follow_up["source_clip_count"], 2);
    assert_eq!(follow_up["eligible_clip_count"], 1);
    let targets = follow_up["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    let clip = report["clips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|clip| clip["waveform"]["status"] == "measured")
        .unwrap();
    let target = clip["waveform"]["target_hz"].as_u64().unwrap();
    let centers = [target - 2_000_000, target + 2_000_000];
    assert_eq!(preview["centers_hz"], json!(centers));
    assert_eq!(
        targets[0],
        json!({
            "file": clip["file"],
            "sha256": clip["sha256"],
            "source_tune_directory": clip["tune_directory"],
            "source_center_hz": 910_000_007,
            "lower_hz": clip["lower_hz"],
            "upper_hz": clip["upper_hz"],
            "target_hz": target,
            "centers_hz": centers,
        })
    );

    let under_budget = preview_triage_follow_up(source.path(), relative_report, 159_999);
    assert!(!under_budget.status.success());
    assert!(under_budget.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&under_budget.stderr).contains("exceeding the 159999-byte budget")
    );

    let mut legacy = report;
    legacy["schema_version"] = json!(1);
    legacy.as_object_mut().unwrap().remove("max_narrow_clips");
    for tune in legacy["tunes"].as_array_mut().unwrap() {
        tune.as_object_mut().unwrap().remove("narrow_search");
    }
    for clip in legacy["clips"].as_array_mut().unwrap() {
        let fields = clip.as_object_mut().unwrap();
        fields.remove("detector");
        fields.remove("narrow_contrast_db");
    }
    let legacy_path = source.path().join("triage/schema-1.json");
    let legacy_bytes = serde_json::to_vec(&legacy).unwrap();
    fs::write(&legacy_path, &legacy_bytes).unwrap();
    let legacy_preview = preview_triage_follow_up(source.path(), &legacy_path, 160_000);
    assert!(
        legacy_preview.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy_preview.stderr)
    );
    let legacy_preview: Value = serde_json::from_slice(&legacy_preview.stdout).unwrap();
    let mut expected = preview.clone();
    expected["source_triage_sha256"] = json!(format!("{:x}", Sha256::digest(legacy_bytes)));
    assert_eq!(legacy_preview, expected);

    let destination = tempfile::tempdir().unwrap();
    let output = run_sweep_on_antenna(
        destination.path(),
        ReceiverMode::Complete,
        &[
            "sweep",
            "--triage-report",
            report_path.to_str().unwrap(),
            "--dwell-ms",
            "1",
            "--max-iq-bytes",
            "160000",
        ],
        1,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let scan = destination.path().join("scan");
    session_log(&scan);
    assert_eq!(read_json(&scan.join("plan.json")), preview);
    let receipt = read_json(&scan.join("sweep.json"));
    assert_eq!(receipt["operation"], "targeted_rf_sweep");
    assert_eq!(
        receipt["source_triage_sha256"],
        preview["source_triage_sha256"]
    );
    assert_eq!(
        receipt["source_sweep_sha256"],
        preview["source_sweep_sha256"]
    );
    assert_eq!(receipt["antenna"], 1);
    assert_eq!(receipt["iq_bytes"], 160_000);
    let tunes = receipt["tunes"].as_array().unwrap();
    assert_eq!(tunes.len(), centers.len());
    for (index, (tune, center_hz)) in tunes.iter().zip(centers).enumerate() {
        assert_eq!(tune["requested_center_hz"], center_hz);
        assert_validated_tune(&scan.join(format!("tune-{:04}", index + 1)), center_hz, 1);
    }
}

#[test]
fn triage_follow_up_rejects_unusable_reports_before_capture() {
    let source = tempfile::tempdir().unwrap();
    let report = capture_triage_follow_up_source(source.path());
    let clips = report["clips"].as_array().unwrap();
    let index = clips
        .iter()
        .position(|clip| clip["waveform"]["status"] == "measured")
        .unwrap();
    let target = clips[index]["waveform"]["target_hz"].as_u64().unwrap();
    let mut inconsistent = report;
    inconsistent["clips"][index]["waveform"]["target_hz"] = json!(target + 1);
    let inconsistent_path = source.path().join("triage/inconsistent.json");
    serde_json::to_writer(File::create(&inconsistent_path).unwrap(), &inconsistent).unwrap();

    let skipped = triage_report(
        source.path(),
        "peak-only",
        &["--max-clips", "2", "--narrow-clips", "0"],
    );
    let clips = skipped["clips"].as_array().unwrap();
    assert_eq!(clips.len(), 2);
    assert!(
        clips
            .iter()
            .all(|clip| clip["waveform"]["status"] == "skipped")
    );
    let destination = tempfile::tempdir().unwrap();
    for (path, diagnostic) in [
        (
            inconsistent_path,
            "measured waveform target is outside the usable probe geometry",
        ),
        (
            source.path().join("peak-only/triage.json"),
            "triage report has no measured narrow clips",
        ),
    ] {
        let rejected = run_sweep_on_antenna(
            destination.path(),
            ReceiverMode::Complete,
            &[
                "sweep",
                "--triage-report",
                path.to_str().unwrap(),
                "--dwell-ms",
                "1",
            ],
            1,
        );
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!destination.path().join("scan").exists());
    }
}

#[test]
fn sweep_triage_limits_analysis_to_the_requested_range_and_saved_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let iq = capture_triage_fixture(
        workspace,
        &[
            "sweep",
            "--lower-hz",
            "902000000",
            "--upper-hz",
            "913000000",
            "--dwell-ms",
            "12",
        ],
        TriageFixture::SeparateBursts,
    );
    let receipt = read_json(&workspace.join("scan/sweep.json"));
    let center_hz = receipt["tunes"][0]["observed_center_hz"].as_u64().unwrap();
    let report = triage_report(workspace, "triage", &[]);
    let clips = report["clips"].as_array().unwrap();
    assert!(
        clips
            .iter()
            .any(|clip| includes_burst(clip, center_hz, BURSTS[0]))
    );
    assert!(
        clips
            .iter()
            .all(|clip| clip["lower_hz"].as_u64().unwrap() < 913_000_000)
    );

    let capped = triage_report(workspace, "prefix", &["--max-samples-per-tune", "81921"]);
    let tune = &capped["tunes"][0];
    assert_eq!(tune["analyzed_samples"], 81_920);
    assert_eq!(
        tune["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..81_920 * 4]))
    );
    let clips = capped["clips"].as_array().unwrap();
    assert!(
        clips
            .iter()
            .any(|clip| includes_burst(clip, center_hz, BURSTS[0]))
    );
    assert!(
        clips
            .iter()
            .all(|clip| clip["end_sample_exclusive"].as_u64().unwrap() <= 81_920)
    );

    let shared_cap = triage_report(
        workspace,
        "shared-prefix",
        &["--max-total-samples", "163842"],
    );
    assert_eq!(shared_cap["tunes"], capped["tunes"]);
    assert_eq!(shared_cap["clips"], capped["clips"]);
}

#[test]
fn sweep_triage_rejects_inconsistent_receipt_without_publishing() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path();
    let output = run_sweep(workspace, ReceiverMode::Complete, SWEEP_ARGS);
    assert!(output.status.success());
    let receipt = workspace.join("scan/sweep.json");
    let mut record = read_json(&receipt);
    let observed_center = record["tunes"][1]["observed_center_hz"].as_u64().unwrap();
    record["tunes"][1]["observed_center_hz"] = json!(observed_center + 1);
    serde_json::to_writer(File::create(receipt).unwrap(), &record).unwrap();

    let output = triage(workspace, "triage", &[]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("invalid observed band geometry at tune-0002")
    );
    assert!(!workspace.join("triage").exists());
}
