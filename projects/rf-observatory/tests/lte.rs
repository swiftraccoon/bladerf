//! Exercise LTE FDD synchronization over an independently generated IQ capture.

use std::f64::consts::{PI, TAU};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rustfft::num_complex::Complex64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RATE_HZ: u64 = 20_000_000;
const CHANNEL_RATE_HZ: u64 = 1_920_000;
const CENTER_HZ: u64 = 750_300_000;
const CHANNEL_HZ: u64 = 747_900_000;
const HALF_FRAME_INPUT_SAMPLES: usize = 100_000;
const FIRST_PSS_INPUT_SAMPLE: f64 = 17_413.25;
const ANALYZED_SAMPLES: usize = 320_000;
const SOURCE_SAMPLES: usize = ANALYZED_SAMPLES + 137;
const IQ_BYTES: usize = 4;
const SUBCARRIER_SPACING_HZ: f64 = 15_000.0;
const SYMBOL_INPUT_SAMPLES: f64 = RATE_HZ as f64 / SUBCARRIER_SPACING_HZ;
const PREFIX_INPUT_SAMPLES: f64 = 9.0 * RATE_HZ as f64 / CHANNEL_RATE_HZ as f64;
// Independently encoded signs for PCI 179 from TS 36.211 Table 6.11.2.1-1.
// A set bit marks a negative sign. Bits run from the lowest tone upward.
const SSS_SIGNS: [u64; 2] = [0x1819_ee50_3ecf_1290, 0x03c0_2e0d_dd83_a834];

fn write_symbol(iq: &mut [u8], useful_start: f64, tones: [Complex64; 62]) {
    let begin = (useful_start - PREFIX_INPUT_SAMPLES).ceil() as usize;
    let end = (useful_start + SYMBOL_INPUT_SAMPLES).ceil() as usize;
    let offset_hz = CHANNEL_HZ as f64 - CENTER_HZ as f64 + 1_600.0;
    for sample in begin..end {
        let symbol_time = (sample as f64 - useful_start) / RATE_HZ as f64;
        let baseband: Complex64 = (-31..0)
            .chain(1..=31)
            .zip(tones)
            .map(|(tone, value)| {
                let phase = TAU * f64::from(tone) * SUBCARRIER_SPACING_HZ * symbol_time;
                value * Complex64::from_polar(1.0, phase)
            })
            .sum();
        let phase = TAU * offset_hz * sample as f64 / RATE_HZ as f64 - 0.41;
        let shifted = baseband * Complex64::from_polar(22.0, phase);
        let index = sample * IQ_BYTES;
        iq[index..index + 2].copy_from_slice(&(shifted.re.round() as i16).to_le_bytes());
        iq[index + 2..index + IQ_BYTES].copy_from_slice(&(shifted.im.round() as i16).to_le_bytes());
    }
}

fn synchronization_iq() -> Vec<u8> {
    let primary = std::array::from_fn(|index| {
        let n = if index < 31 { index } else { index + 1 } as f64;
        Complex64::from_polar(1.0, -PI * 34.0 * n * (n + 1.0) / 63.0)
    });
    let mut iq = vec![0; SOURCE_SAMPLES * IQ_BYTES];
    for half_frame in 0..4 {
        let mask = SSS_SIGNS[half_frame % SSS_SIGNS.len()];
        let secondary = std::array::from_fn(|index| {
            let sign = if mask & (1 << index) == 0 { 1.0 } else { -1.0 };
            Complex64::new(sign, 0.0)
        });
        let primary_start = FIRST_PSS_INPUT_SAMPLE + (half_frame * HALF_FRAME_INPUT_SAMPLES) as f64;
        let secondary_start = primary_start - PREFIX_INPUT_SAMPLES - SYMBOL_INPUT_SAMPLES;
        write_symbol(&mut iq, secondary_start, secondary);
        write_symbol(&mut iq, primary_start, primary);
    }
    iq
}

fn replay(root: &Path, output: &str, analyzed_samples: usize) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-lte",
            "downlink.sc16q11",
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &CHANNEL_HZ.to_string(),
            "--max-samples",
            &analyzed_samples.to_string(),
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

fn raster_replay(root: &Path, output: &str, lower_hz: u64, upper_hz: u64) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-lte-raster",
            "downlink.sc16q11",
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--lower-hz",
            &lower_hz.to_string(),
            "--upper-hz",
            &upper_hz.to_string(),
            "--max-samples",
            &ANALYZED_SAMPLES.to_string(),
            "--output",
            output,
        ])
        .output()
        .unwrap()
}

fn write_evidence(path: &Path, value: Value) -> String {
    let encoded = serde_json::to_vec(&value).unwrap();
    fs::write(path, &encoded).unwrap();
    format!("{:x}", Sha256::digest(encoded))
}

fn archive_sweep(root: &Path, iq: &[u8], observed_center_hz: u64) -> (String, String) {
    let tune = root.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    fs::write(tune.join("wideband.sc16q11"), iq).unwrap();
    let samples = iq.len() as u64 / IQ_BYTES as u64;
    let sidecar = write_evidence(
        &tune.join("wideband.sc16q11.json"),
        json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": observed_center_hz,
            "sample_rate_hz": RATE_HZ,
            "bandwidth_hz": RATE_HZ,
            "gain_db": 31.5,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "num_samples": samples,
            "started_utc": "2000-01-01T00:00:00Z"
        }),
    );
    let sweep = write_evidence(
        &root.join("sweep.json"),
        json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ,
            "upper_hz": CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 31.5,
            "dwell_ms": samples * 1_000 / RATE_HZ,
            "iq_bytes": iq.len(),
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": CENTER_HZ,
                "observed_center_hz": observed_center_hz,
                "observed_bands": [
                    {"lower_hz": observed_center_hz - 8_000_000, "upper_hz": observed_center_hz - 100_000},
                    {"lower_hz": observed_center_hz + 100_000, "upper_hz": observed_center_hz + 8_000_000}
                ]
            }]
        }),
    );
    (sweep, sidecar)
}

fn archive_series(root: &Path, sweeps: [&str; 2]) -> String {
    let dwells = [0, 20_000_000].map(|offset| {
        json!({
            "requested_center_hz": CENTER_HZ,
            "started_ns": offset + 500_000,
            "tuned_ns": offset + 600_000,
            "files_ready_ns": offset + 700_000,
            "stream_ready_ns": offset + 800_000,
            "read_started_ns": offset + 1_000_000,
            "read_finished_ns": offset + 17_000_000,
            "stream_closed_ns": offset + 17_200_000,
            "published_ns": offset + 18_000_000
        })
    });
    let timing_sha256 = write_evidence(
        &root.join("acquisition-timing.json"),
        json!({
            "schema_version": 1,
            "operation": "native_rf_sweep_timing",
            "passes": 2,
            "device_ready_ns": 300_000,
            "device_closed_ns": 39_000_000,
            "dwells": dwells
        }),
    );
    write_evidence(
        &root.join("sweep-series.json"),
        json!({
            "schema_version": 1,
            "operation": "rf_sweep_series",
            "iq_bytes": 2 * ANALYZED_SAMPLES * IQ_BYTES,
            "dwells": 2,
            "acquisition_elapsed_seconds": 0.04,
            "capture_cadence": {"nominal_iq_seconds": 0.032, "native": {"sha256": timing_sha256}},
            "passes": [
                {"directory": "pass-0001", "sweep_sha256": sweeps[0]},
                {"directory": "pass-0002", "sweep_sha256": sweeps[1]}
            ]
        }),
    )
}

fn sweep_replay(root: &Path, output: &str, bounds: [u64; 2], extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-lte-sweep",
            ".",
            "--lower-hz",
            &bounds[0].to_string(),
            "--upper-hz",
            &bounds[1].to_string(),
            "--output",
            output,
        ])
        .args(extra)
        .output()
        .unwrap()
}

#[test]
fn identifies_the_cell_only_after_repeated_alternating_syncs() {
    let directory = tempfile::tempdir().unwrap();
    let iq = synchronization_iq();
    fs::write(directory.path().join("downlink.sc16q11"), &iq).unwrap();

    let report = replay(directory.path(), "lte.json", ANALYZED_SAMPLES);
    assert_eq!(report["operation"], "identify_lte_in_saved_iq");
    assert_eq!(report["source_samples"], SOURCE_SAMPLES);
    assert_eq!(report["analyzed_samples"], ANALYZED_SAMPLES);
    assert_eq!(report["channel_sample_rate_hz"], CHANNEL_RATE_HZ);
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..ANALYZED_SAMPLES * IQ_BYTES]))
    );
    let cells = report["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 1, "{report}");
    let cell = &cells[0];
    assert_eq!(cell["physical_cell_id"], 179);
    assert_eq!(cell["longest_sync_run"], 4);
    let observations = cell["observations"].as_array().unwrap();
    assert_eq!(observations.len(), 4);
    let filter_delay = report["channel_filter_delay_input_samples"]
        .as_u64()
        .unwrap() as f64;
    let expected_first = ((FIRST_PSS_INPUT_SAMPLE + filter_delay + 1.0) * CHANNEL_RATE_HZ as f64
        / RATE_HZ as f64
        - 1.0)
        .round() as u64;
    let indices: Vec<_> = observations
        .iter()
        .enumerate()
        .map(|(half_frame, observation)| {
            assert_eq!(observation["subframe"], half_frame % 2 * 5);
            observation["pss_start_channel_sample"].as_u64().unwrap()
        })
        .collect();
    assert!(indices[0].abs_diff(expected_first) <= 2, "{indices:?}");
    assert_eq!(report["repeat_tolerance_channel_samples"], 6);
    assert!(
        indices
            .windows(2)
            .all(|pair| (pair[1] - pair[0]).abs_diff(CHANNEL_RATE_HZ / 200) <= 6)
    );

    let short = replay(
        directory.path(),
        "two-syncs.json",
        2 * HALF_FRAME_INPUT_SAMPLES,
    );
    assert!(short["sss_matches"].as_u64().unwrap() >= 2);
    assert_eq!(short["cells"], json!([]));
}

#[test]
fn raster_matches_scalar_cell_and_preserves_analyzed_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let iq = synchronization_iq();
    fs::write(root.join("downlink.sc16q11"), &iq).unwrap();

    let frequencies = [CHANNEL_HZ - 100_000, CHANNEL_HZ, CHANNEL_HZ + 100_000];
    let output = raster_replay(root, "raster.json", frequencies[0], frequencies[2]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("raster.json")).unwrap()).unwrap();
    assert_eq!(report["operation"], "identify_lte_raster_in_saved_iq");
    assert_eq!(report["source_samples"], SOURCE_SAMPLES);
    assert_eq!(report["analyzed_samples"], ANALYZED_SAMPLES);
    assert_eq!(report["channel_sample_rate_hz"], CHANNEL_RATE_HZ);
    assert_eq!(
        report["channelized_samples_per_channel"],
        ANALYZED_SAMPLES as u64 * CHANNEL_RATE_HZ / RATE_HZ
    );
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..ANALYZED_SAMPLES * IQ_BYTES]))
    );

    let channels = report["channels"].as_array().unwrap();
    assert_eq!(channels.len(), frequencies.len());
    for (channel, frequency) in channels.iter().zip(frequencies) {
        assert_eq!(channel["channel_frequency_hz"], frequency);
        let sss_matches = channel["sss_matches"].as_u64().unwrap();
        assert!(channel["pss_candidates"].as_u64().unwrap() >= sss_matches);
        if frequency == CHANNEL_HZ {
            assert!(sss_matches >= 4);
        } else {
            assert_eq!(channel["cells"], json!([]));
        }
    }

    let scalar = replay(root, "scalar.json", ANALYZED_SAMPLES);
    assert_eq!(
        report["channelized_samples_per_channel"],
        scalar["channelized_samples"]
    );
    assert_eq!(report["analyzed_iq_sha256"], scalar["analyzed_iq_sha256"]);
    let cells = channels[1]["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 1, "{}", channels[1]);
    let cell = &cells[0];
    assert_eq!(cell["physical_cell_id"], 179);
    assert_eq!(
        cell["physical_cell_id"],
        scalar["cells"][0]["physical_cell_id"]
    );
    assert_eq!(cell["longest_sync_run"], 4);
    let observations = cell["observations"].as_array().unwrap();
    let scalar_observations = scalar["cells"][0]["observations"].as_array().unwrap();
    assert_eq!(observations.len(), 4);
    assert_eq!(observations.len(), scalar_observations.len());
    for (raster, scalar) in observations.iter().zip(scalar_observations) {
        assert_eq!(raster["subframe"], scalar["subframe"]);
        let raster_start = raster["pss_start_channel_sample"].as_u64().unwrap();
        let scalar_start = scalar["pss_start_channel_sample"].as_u64().unwrap();
        assert!(
            raster_start.abs_diff(scalar_start) <= 1,
            "{raster_start} vs {scalar_start}"
        );
    }

    let invalid = raster_replay(root, "off-grid.json", frequencies[0] + 1, frequencies[2]);
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(!invalid.status.success());
    assert!(stderr.contains("100 kHz grid"), "{stderr}");
    assert!(!root.join("off-grid.json").exists());
}

#[test]
fn sweep_series_reports_only_positive_carriers_and_preflights_aggregate_budget() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut signal = synchronization_iq();
    let saved_bytes = ANALYZED_SAMPLES * IQ_BYTES;
    let quiet = vec![0; saved_bytes];
    let first = archive_sweep(&root.join("pass-0001"), &signal[..saved_bytes], CENTER_HZ);
    let second = archive_sweep(&root.join("pass-0002"), &quiet, CENTER_HZ + 7);
    let series_sha256 = archive_series(root, [&first.0, &second.0]);

    let bounds = [CHANNEL_HZ - 100_000, CHANNEL_HZ + 100_000];
    let selected = ANALYZED_SAMPLES - 73;
    let channelized = selected as u64 * CHANNEL_RATE_HZ / RATE_HZ;
    let planned = 2 * 3 * channelized;
    let output = sweep_replay(
        root,
        "series.json",
        bounds,
        &[
            "--series",
            "--lte-pbch",
            "--max-samples-per-dwell",
            &selected.to_string(),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("series.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "identify_lte_in_saved_sweeps");
    assert_eq!(report["capture_kind"], "series");
    assert_eq!(report["source_capture_sha256"], series_sha256);
    assert_eq!(report["replayed_iq_samples"], 2 * selected);
    assert_eq!(report["planned_channelized_samples"], planned);
    assert_eq!(report["carrier_attempts"], 6);
    assert_eq!(report["cell_reports"], 1);
    assert_eq!(report["pbch_cell_reports"], 0);
    assert_eq!(report["uncovered_ranges"], json!([]));
    assert_eq!(report["short_only_ranges"], json!([]));

    let dwells = report["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 2);
    for (index, (dwell, (sweep, sidecar))) in dwells.iter().zip([first, second]).enumerate() {
        assert_eq!(dwell["pass_index"], index + 1);
        assert_eq!(dwell["pass_directory"], format!("pass-{:04}", index + 1));
        assert_eq!(dwell["tune_directory"], "tune-0001");
        assert_eq!(dwell["source_sweep_sha256"], sweep);
        assert_eq!(dwell["source_sidecar_sha256"], sidecar);
        assert_eq!(dwell["lower_hz"], bounds[0]);
        assert_eq!(dwell["upper_hz"], bounds[1]);
        assert_eq!(dwell["observed_center_hz"], CENTER_HZ + 7 * index as u64);
        assert_eq!(dwell["saved_samples"], ANALYZED_SAMPLES);
        assert_eq!(dwell["selected_samples"], selected);
        assert_eq!(dwell["status"], "replayed");
        assert_eq!(
            dwell["replay"]["channelized_samples_per_channel"],
            channelized
        );
        let iq = if index == 0 { &signal } else { &quiet };
        assert_eq!(
            dwell["replay"]["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..selected * IQ_BYTES]))
        );
    }
    let first_replay = &dwells[0]["replay"];
    assert!(first_replay["pss_candidates"].as_u64().unwrap() >= 4);
    assert!(first_replay["sss_matches"].as_u64().unwrap() >= 4);
    let carriers = first_replay["carriers"].as_array().unwrap();
    assert_eq!(carriers.len(), 1);
    assert_eq!(carriers[0]["channel_frequency_hz"], CHANNEL_HZ);
    assert_eq!(carriers[0]["cells"][0]["physical_cell_id"], 179);
    assert_eq!(carriers[0]["cells"][0]["longest_sync_run"], 4);
    let pbch = &carriers[0]["cells"][0]["pbch"];
    assert_eq!(pbch["subframe_zero_observations"], 2);
    assert_eq!(pbch["confirmations"], json!([]));
    assert_eq!(dwells[1]["replay"]["carriers"], json!([]));

    // The archived byte extent stays valid. Replay would reject the Q11 sample.
    signal[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    fs::write(
        root.join("pass-0001/tune-0001/wideband.sc16q11"),
        &signal[..saved_bytes],
    )
    .unwrap();
    let preview = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "replay-sweep",
            ".",
            "--series",
            "--lte-range-hz",
            &format!("{}:{}", bounds[0], bounds[1]),
            "--lte-pbch",
            "--max-samples-per-dwell",
            &selected.to_string(),
            "--plan-only",
        ])
        .output()
        .unwrap();
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(plan["lte_pbch"], true);
    assert_eq!(plan["total_replayed_iq_bytes"], 2 * selected * IQ_BYTES);
    assert_eq!(plan["analyzers"][0]["budget_samples"], planned);

    let output = sweep_replay(
        root,
        "over-budget.json",
        bounds,
        &[
            "--series",
            "--max-samples-per-dwell",
            &selected.to_string(),
            "--max-total-channelized-samples",
            &(planned - 1).to_string(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(&planned.to_string()), "{stderr}");
    assert!(stderr.contains("channelized samples"), "{stderr}");
    assert!(!root.join("over-budget.json").exists());
}

#[test]
fn untimed_sweep_distinguishes_uncovered_carriers_from_short_dwells() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut iq = vec![0; ANALYZED_SAMPLES * IQ_BYTES];
    // Short dwells must not reach the decoder even when the saved extent is long.
    iq[..2].copy_from_slice(&2_048_i16.to_le_bytes());
    let (sweep_sha256, _) = archive_sweep(root, &iq, CENTER_HZ + 7);
    let edge = CENTER_HZ - 7_000_000;
    let output = sweep_replay(
        root,
        "short.json",
        [edge, edge + 200_000],
        &["--max-samples-per-dwell", "200000"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value =
        serde_json::from_slice(&fs::read(root.join("short.json")).unwrap()).unwrap();
    assert_eq!(report["capture_kind"], "sweep");
    assert_eq!(report["source_capture_sha256"], sweep_sha256);
    assert_eq!(report["minimum_dwell_samples"], 202_761);
    for key in [
        "replayed_iq_samples",
        "planned_channelized_samples",
        "carrier_attempts",
    ] {
        assert_eq!(report[key], 0);
    }
    assert_eq!(
        report["uncovered_ranges"],
        json!([{"lower_hz": edge, "upper_hz": edge}])
    );
    assert_eq!(
        report["short_only_ranges"],
        json!([{"lower_hz": edge + 100_000, "upper_hz": edge + 200_000}])
    );
    let dwells = report["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 1);
    let dwell = &dwells[0];
    assert_eq!(dwell["pass_index"], 1);
    assert!(dwell.get("pass_directory").is_none());
    assert_eq!(dwell["lower_hz"], edge + 100_000);
    assert_eq!(dwell["upper_hz"], edge + 200_000);
    assert_eq!(dwell["observed_center_hz"], CENTER_HZ + 7);
    assert_eq!(dwell["saved_samples"], ANALYZED_SAMPLES);
    assert_eq!(dwell["selected_samples"], 200_000);
    assert_eq!(dwell["status"], "short_dwell");
    assert!(dwell.get("replay").is_none());
}
