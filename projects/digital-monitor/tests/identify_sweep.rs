//! Device-free entry-point checks for saved-sweep receipt validation and routing.

use std::fs::{self, DirBuilder, File};
use std::ops::Range;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const WIFI_RANGE_HZ: Range<u64> = 2_411_999_999..2_413_000_000;
// This range lies below the center guard of the single planned tune.
const CENTER_HZ: u64 = WIFI_RANGE_HZ.start + 8_000_000 - 10;
const REVISITED_WIFI_RANGE_HZ: Range<u64> = 2_436_999_999..2_438_000_000;
const WIFI_VISITS_HZ: [u64; 4] = [2_438_500_000, 2_430_000_000, 2_438_500_000, 2_431_500_000];

fn write_json(path: &Path, value: &Value) {
    serde_json::to_writer(File::create(path).unwrap(), value).unwrap();
}

fn saved_tune(tune: &Path, center_hz: u64) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(tune)
        .unwrap();
    File::create(tune.join("wideband.sc16q11"))
        .unwrap()
        .set_len(80_000)
        .unwrap();
    let sidecar = json!({
        "format": "SC16_Q11",
        "dtype": "<i2",
        "interleave": "I,Q",
        "complex_scale": 2048,
        "frequency_hz": center_hz,
        "sample_rate_hz": 20_000_000,
        "bandwidth_hz": 20_000_000,
        "gain_db": 40,
        "gain_mode": "manual",
        "bias_tee_enabled": false,
        "channel": "RX1",
        "num_samples": 20_000,
        "started_utc": "2000-01-01T00:00:00Z",
    });
    write_json(&tune.join("wideband.sc16q11.json"), &sidecar);
    write_json(&tune.join("capture.json"), &sidecar);
}

fn tune_receipt(directory: &str, requested_hz: u64, observed_hz: u64) -> Value {
    json!({
        "directory": directory,
        "requested_center_hz": requested_hz,
        "observed_center_hz": observed_hz,
        "observed_bands": [
            {"lower_hz": observed_hz - 8_000_000, "upper_hz": observed_hz - 100_000},
            {"lower_hz": observed_hz + 100_000, "upper_hz": observed_hz + 8_000_000},
        ],
        "spectrum_windows": 2,
        "activity_regions": [],
    })
}

fn saved_sweep(directory: &Path, tune_directory: &str, center_hz: u64, range_hz: Range<u64>) {
    saved_tune(&directory.join("tune-0001"), center_hz);
    write_json(
        &directory.join("plan.json"),
        &json!({
            "schema_version": 1,
            "operation": "sequential_rf_sweep_plan",
            "lower_hz": range_hz.start,
            "upper_hz": range_hz.end,
            "dwell_ms": 1,
            "samples_per_tune": 20_000,
            "planned_iq_bytes": 80_000,
            "centers_hz": [center_hz],
            "sample_rate_hz": 20_000_000,
            "max_iq_bytes": 1_073_741_824,
            "activity_half_span_hz": 8_000_000,
            "center_guard_hz": 100_000,
            "center_readback_tolerance_hz": 10,
        }),
    );
    write_json(
        &directory.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "sequential_rf_sweep",
            "lower_hz": range_hz.start,
            "upper_hz": range_hz.end,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": 1,
            "iq_bytes": 80_000,
            "elapsed_seconds": 0.3,
            "acquisition_elapsed_seconds": 0.2,
            "analysis_elapsed_seconds": 0.1,
            "tunes": [tune_receipt(tune_directory, center_hz, center_hz)],
        }),
    );
}

fn saved_targeted_sweep(directory: &Path, centers_hz: &[u64]) {
    let tunes: Vec<_> = centers_hz
        .iter()
        .enumerate()
        .map(|(index, &requested_hz)| {
            let name = format!("tune-{:04}", index + 1);
            let observed_hz = requested_hz + 7;
            saved_tune(&directory.join(&name), observed_hz);
            tune_receipt(&name, requested_hz, observed_hz)
        })
        .collect();
    write_json(
        &directory.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": centers_hz[0],
            "upper_hz": centers_hz.last().unwrap() + 1,
            "dwell_ms": 1,
            "iq_bytes": 80_000 * centers_hz.len(),
            "tunes": tunes,
        }),
    );
}

fn saved_scheduled_sweep(directory: &Path, visits_hz: &[u64], range_hz: Range<u64>) {
    let tunes: Vec<_> = visits_hz
        .iter()
        .enumerate()
        .map(|(index, &center_hz)| {
            let name = format!("tune-{:04}", index + 1);
            saved_tune(&directory.join(&name), center_hz);
            tune_receipt(&name, center_hz, center_hz)
        })
        .collect();
    write_json(
        &directory.join("sweep.json"),
        &json!({
            "schema_version": 1,
            "operation": "sequential_rf_sweep",
            "lower_hz": range_hz.start,
            "upper_hz": range_hz.end,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": 1,
            "iq_bytes": 80_000 * visits_hz.len(),
            "visit_centers_hz": visits_hz,
            "tunes": tunes,
        }),
    );
}

fn saved_revisited_wifi_series(directory: &Path) -> String {
    let passes: Vec<_> = ["pass-0001", "pass-0002"]
        .into_iter()
        .map(|name| {
            let pass = directory.join(name);
            saved_scheduled_sweep(&pass, &WIFI_VISITS_HZ, REVISITED_WIFI_RANGE_HZ);
            let sweep_digest = Sha256::digest(fs::read(pass.join("sweep.json")).unwrap());
            json!({"directory": name, "sweep_sha256": format!("{sweep_digest:x}")})
        })
        .collect();
    let manifest = directory.join("sweep-series.json");
    write_json(
        &manifest,
        &json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "iq_bytes": 80_000 * WIFI_VISITS_HZ.len() * passes.len(),
            "dwells": WIFI_VISITS_HZ.len() * passes.len(),
            "visit_centers_hz": WIFI_VISITS_HZ,
            "passes": passes,
        }),
    );
    format!("{:x}", Sha256::digest(fs::read(manifest).unwrap()))
}

fn identify(directory: &Path, decoder: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(directory)
        .args([
            "identify-sweep",
            "sweep",
            "--output",
            "identification",
            "--protocol",
            "wifi",
            "--wifi-monitor",
            decoder,
        ])
        .output()
        .unwrap()
}

fn identify_eligible(directory: &Path, output: &str, decoder: &str, limit: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(directory)
        .args([
            "identify-sweep",
            "sweep",
            "--output",
            output,
            "--protocol",
            "wifi",
            "--wifi-monitor",
            decoder,
            "--channel-scope",
            "eligible",
            "--max-attempts",
            limit,
        ])
        .output()
        .unwrap()
}

#[test]
fn scheduled_sweep_keeps_nearest_default_and_replays_each_requested_visit() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    saved_scheduled_sweep(
        &root.join("sweep"),
        &WIFI_VISITS_HZ,
        REVISITED_WIFI_RANGE_HZ,
    );

    let nearest = identify(root, "/bin/false");
    assert!(!nearest.status.success());
    let report: Value = serde_json::from_slice(
        &fs::read(root.join("identification/identification.json")).unwrap_or_else(|error| {
            panic!("{error}: {}", String::from_utf8_lossy(&nearest.stderr))
        }),
    )
    .unwrap();
    assert!(report.get("all_visits").is_none());
    assert_eq!(report["results"].as_array().unwrap().len(), 1);
    assert_eq!(report["results"][0]["tune"], "tune-0001");
    assert_eq!(report["results"][0]["evidence_directory"], "wifi-2437mhz");

    let replay = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(root)
        .args([
            "identify-sweep",
            "sweep",
            "--all-visits",
            "--output",
            "all-visits",
            "--protocol",
            "wifi",
            "--wifi-monitor",
            "/bin/false",
        ])
        .output()
        .unwrap();
    assert!(!replay.status.success());
    let output = root.join("all-visits");
    let file = File::open(output.join("identification.json"))
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&replay.stderr)));
    let report: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(report["all_visits"], true);
    assert_eq!(report["eligible_channel_count"], 1);
    assert_eq!(report["planned_decoder_attempt_count"], 2);
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    for (result, tune) in results.iter().zip(["tune-0001", "tune-0003"]) {
        let directory = format!("{tune}/wifi-2437mhz");
        assert_eq!(result["tune"], tune);
        assert_eq!(result["evidence_directory"], directory);
        assert!(result.get("pass_directory").is_none());
        assert!(output.join(directory).join("logs").is_dir());
    }
}

#[test]
fn series_replays_repeated_tunes_with_distinct_evidence() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    let series_sha256 = saved_revisited_wifi_series(&root.join("series"));
    let command = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(root)
        .args([
            "identify-sweep",
            "series",
            "--series",
            "--all-visits",
            "--output",
            "identification",
            "--protocol",
            "wifi",
            "--max-attempts",
            "4",
            "--max-total-samples",
            "80000",
            "--wifi-monitor",
            "/bin/false",
        ])
        .output()
        .unwrap();
    assert!(!command.status.success());
    let output = root.join("identification");
    let file = File::open(output.join("identification.json"))
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&command.stderr)));
    let report: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(report["operation"], "identify_saved_sweep_series");
    assert_eq!(report["sweep_series_sha256"], series_sha256);
    assert_eq!(report["all_visits"], true);
    assert_eq!(report["eligible_channel_count"], 1);
    assert_eq!(report["planned_decoder_attempt_count"], 4);
    assert_eq!(report["overall_state"], "failed");
    let results = report["results"].as_array().unwrap();
    let captures = report["captures"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    assert_eq!(captures.len(), 4);
    for ((result, capture), (pass, tune)) in results.iter().zip(captures).zip([
        ("pass-0001", "tune-0001"),
        ("pass-0001", "tune-0003"),
        ("pass-0002", "tune-0001"),
        ("pass-0002", "tune-0003"),
    ]) {
        let directory = format!("{pass}/{tune}/wifi-2437mhz");
        assert_eq!(result["channel"]["frequency_hz"], 2_437_000_000u64);
        assert_eq!(result["pass_directory"], pass);
        assert_eq!(result["tune"], tune);
        assert_eq!(result["evidence_directory"], directory);
        assert_eq!(result["state"], "failed");
        assert_eq!(capture["pass_directory"], pass);
        assert_eq!(capture["tune"], tune);
        assert!(output.join(directory).join("logs").is_dir());
    }
}

#[test]
fn series_preview_and_limits_apply_before_iq_or_decoder_access() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    let source = root.join("series");
    let digest = saved_revisited_wifi_series(&source);
    for pass in ["pass-0001", "pass-0002"] {
        for tune in ["tune-0001", "tune-0003"] {
            fs::remove_dir_all(source.join(pass).join(tune)).unwrap();
        }
    }
    let args = [
        "identify-sweep",
        "series",
        "--series",
        "--all-visits",
        "--protocol",
        "wifi",
        "--wifi-monitor",
        "missing-decoder",
    ];
    let preview = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(root)
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
    assert_eq!(plan["operation"], "plan_saved_sweep_identification");
    assert_eq!(plan["sweep_series_sha256"], digest);
    assert_eq!(plan["all_visits"], true);
    assert_eq!(plan["eligible_channel_count"], 1);
    assert_eq!(plan["planned_decoder_attempt_count"], 4);
    assert_eq!(plan["distinct_tune_count"], 4);
    assert_eq!(plan["admitted_iq_samples"], 80_000);
    assert_eq!(plan["admitted_iq_bytes"], 320_000);
    assert_eq!(fs::read_dir(root).unwrap().count(), 1);

    for (limit, count) in [("--max-attempts", "3"), ("--max-total-samples", "79999")] {
        let command = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
            .current_dir(root)
            .args(args)
            .args(["--output", "over-limit", limit, count])
            .output()
            .unwrap();
        assert!(!command.status.success());
        assert!(String::from_utf8_lossy(&command.stderr).contains(limit));
        assert!(!root.join("over-limit").exists());
    }
}

#[test]
fn series_manifest_rejects_a_modified_pass_before_creating_output() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    let source = root.join("series");
    saved_revisited_wifi_series(&source);
    let receipt = source.join("pass-0002/sweep.json");
    let mut bytes = fs::read(&receipt).unwrap();
    bytes.push(b'\n');
    fs::write(receipt, bytes).unwrap();

    let command = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(root)
        .args([
            "identify-sweep",
            "series",
            "--series",
            "--all-visits",
            "--output",
            "identification",
            "--protocol",
            "wifi",
        ])
        .output()
        .unwrap();
    assert!(!command.status.success());
    assert!(String::from_utf8_lossy(&command.stderr).contains("SHA-256"));
    assert!(!root.join("identification").exists());
}

#[test]
fn follow_up_plans_missing_decoder_coverage_from_the_saved_receipt() {
    let test = tempfile::tempdir().unwrap();
    let sweep = test.path().join("sweep");
    saved_sweep(&sweep, "tune-0001", CENTER_HZ, WIFI_RANGE_HZ);
    fs::remove_dir_all(sweep.join("tune-0001")).unwrap();
    let source_digest = Sha256::digest(fs::read(sweep.join("sweep.json")).unwrap());

    let output = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(test.path())
        .args(["plan-sweep-follow-up", "sweep", "--protocol", "wifi"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["schema_version"], 1);
    assert_eq!(plan["operation"], "plan_sweep_follow_up");
    assert_eq!(plan["source_sweep_sha256"], format!("{source_digest:x}"));
    assert_eq!(plan["centers_hz"], json!([2_412_000_000u64]));
    let targets = plan["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["protocol"], "wifi");
    assert_eq!(targets[0]["channel_number"], 1);
    assert_eq!(targets[0]["frequency_hz"], 2_412_000_000u64);
    assert_eq!(fs::read_dir(test.path()).unwrap().count(), 1);
}

#[test]
fn targeted_sweep_identifies_only_the_requested_centers() {
    let test = tempfile::tempdir().unwrap();
    let sweep = test.path().join("sweep");
    let centers_hz = [2_412_000_000u64, 2_422_000_000];
    saved_targeted_sweep(&sweep, &centers_hz);

    let output = identify(test.path(), "/bin/false");
    assert!(!output.status.success());
    let path = test.path().join("identification/identification.json");
    let file = File::open(path)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    let report: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(report["operation"], "identify_targeted_rf_sweep");
    assert_eq!(report["overall_state"], "failed");
    assert!(report.get("channel_scope").is_none());
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), centers_hz.len());
    for (index, (result, center_hz)) in results.iter().zip(centers_hz).enumerate() {
        assert_eq!(result["channel"]["frequency_hz"], center_hz);
        assert_eq!(result["tune"], format!("tune-{:04}", index + 1));
        assert_eq!(result["state"], "failed");
    }
}

#[test]
fn eligible_scope_routes_targeted_neighbors_by_readback_within_budget() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    saved_targeted_sweep(&root.join("sweep"), &[2_412_000_000, 2_422_000_000]);

    let too_many = identify_eligible(root, "over-limit", "missing-decoder", "3");
    assert!(!too_many.status.success());
    assert!(String::from_utf8_lossy(&too_many.stderr).contains("sweep needs 4 replays"));
    assert!(!root.join("over-limit").exists());

    let output = identify_eligible(root, "identification", "/bin/false", "4");
    assert!(!output.status.success());
    let file = File::open(root.join("identification/identification.json"))
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    let report: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(report["operation"], "identify_targeted_rf_sweep");
    assert_eq!(report["overall_state"], "failed");
    assert_eq!(report["channel_scope"], "eligible");
    assert_eq!(report["eligible_channel_count"], 4);
    assert_eq!(report["planned_decoder_attempt_count"], 4);
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 4);
    for (result, (frequency_hz, tune)) in results.iter().zip([
        (2_412_000_000u64, "tune-0001"),
        (2_417_000_000, "tune-0001"),
        (2_422_000_000, "tune-0002"),
        (2_427_000_000, "tune-0002"),
    ]) {
        assert_eq!(result["channel"]["frequency_hz"], frequency_hz);
        assert_eq!(result["tune"], tune);
        assert_eq!(result["state"], "failed");
    }
}

#[test]
fn eligible_scope_reports_zero_channels_without_resolving_a_decoder() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    saved_targeted_sweep(&root.join("sweep"), &[900_000_000]);
    let output = identify_eligible(root, "identification", "missing-decoder", "1");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let file = File::open(root.join("identification/identification.json")).unwrap();
    let report: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(report["overall_state"], "complete");
    assert_eq!(report["channel_scope"], "eligible");
    assert_eq!(report["eligible_channel_count"], 0);
    assert_eq!(report["planned_decoder_attempt_count"], 0);
    assert_eq!(report["programs"], json!([]));
    assert_eq!(report["results"], json!([]));
}

#[test]
fn no_eligible_tune_is_reported_without_resolving_a_decoder() {
    let test = tempfile::tempdir().unwrap();
    saved_sweep(
        &test.path().join("sweep"),
        "tune-0001",
        CENTER_HZ,
        WIFI_RANGE_HZ,
    );
    let result = identify(test.path(), "missing-decoder");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let receipt = fs::read(test.path().join("identification/identification.json")).unwrap();
    let receipt: Value = serde_json::from_slice(&receipt).unwrap();
    assert_eq!(receipt["schema_version"], 1);
    assert_eq!(receipt["operation"], "identify_saved_sweep");
    assert_eq!(receipt["overall_state"], "complete");
    assert_eq!(receipt["programs"], json!([]));
    assert_eq!(receipt["captures"], json!([]));
    let results = receipt["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["channel"]["protocol"], "wifi");
    assert_eq!(results[0]["channel"]["channel_number"], 1);
    assert_eq!(results[0]["channel"]["frequency_hz"], 2_412_000_000u64);
    assert_eq!(results[0]["state"], "no_eligible_tune");
}

#[test]
fn a_tune_path_with_traversal_fails_before_creating_output() {
    let test = tempfile::tempdir().unwrap();
    saved_sweep(
        &test.path().join("sweep"),
        "tune-0001/..",
        CENTER_HZ,
        WIFI_RANGE_HZ,
    );
    assert!(!identify(test.path(), "missing-decoder").status.success());
    assert!(!test.path().join("identification").exists());
}

#[test]
fn decoder_failure_retains_a_terminal_result_without_confirmation() {
    let test = tempfile::tempdir().unwrap();
    // This center covers the same request below its guard and enables replay.
    saved_sweep(
        &test.path().join("sweep"),
        "tune-0001",
        2_413_500_000,
        WIFI_RANGE_HZ,
    );
    assert!(!identify(test.path(), "/bin/false").status.success());
    let output = test.path().join("identification");
    let receipt = fs::read(output.join("identification.json")).unwrap();
    let receipt: Value = serde_json::from_slice(&receipt).unwrap();
    assert_eq!(receipt["overall_state"], "failed");
    let result = &receipt["results"][0];
    assert_eq!(result["state"], "failed");
    assert_eq!(result["tune"], "tune-0001");
    assert!(result.get("evidence").is_none());
    assert!(output.join("wifi-2412mhz/logs").is_dir());
}

#[test]
fn bluetooth_channels_share_one_tune_attempt_and_failure_evidence() {
    let test = tempfile::tempdir().unwrap();
    let root = test.path();
    let center_hz = 2_429_371_200;
    saved_sweep(
        &root.join("sweep"),
        "tune-0001",
        center_hz,
        (center_hz - 7_999_990)..2_427_000_000,
    );
    let workspace = root.join("workspace");
    fs::create_dir_all(workspace.join("repos")).unwrap();
    fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
    let command = Command::new(env!("CARGO_BIN_EXE_digital-monitor"))
        .current_dir(root)
        .args([
            "identify-sweep",
            "sweep",
            "--output",
            "identification",
            "--protocol",
            "bluetooth-le",
            "--max-attempts",
            "1",
            "--bluetooth-monitor",
            "/bin/false",
            "--blue-dragon",
            "/bin/false",
            "--bluetooth-workspace",
        ])
        .arg(workspace)
        .output()
        .unwrap();
    assert!(!command.status.success());
    let output = root.join("identification");
    let file = File::open(output.join("identification.json"))
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&command.stderr)));
    let receipt: Value = serde_json::from_reader(file).unwrap();
    assert_eq!(receipt["overall_state"], "failed");
    let results = receipt["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    let detail = results[0]["detail"].as_str().unwrap();
    assert!(detail.contains("bluetooth-monitor exited unsuccessfully"));
    let evidence = "bluetooth-le-tune-0001";
    for (result, frequency_hz) in
        results
            .iter()
            .zip([2_422_000_000u64, 2_424_000_000, 2_426_000_000])
    {
        assert_eq!(result["channel"]["frequency_hz"], frequency_hz);
        assert_eq!(result["state"], "failed");
        assert_eq!(result["tune"], "tune-0001");
        assert_eq!(result["evidence_directory"], evidence);
        assert_eq!(result["detail"], detail);
    }
    assert!(
        output
            .join(evidence)
            .join("logs/decode.stderr.log")
            .is_file()
    );
}
