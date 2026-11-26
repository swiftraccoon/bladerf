//! Exercise NR synchronization on independently synthesized original-rate IQ.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rustfft::num_complex::Complex64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RATE_HZ: usize = 20_000_000;
const CENTER_HZ: u64 = 915_000_000;
const IQ_BYTES: usize = 4;
const SAMPLES: usize = 280_000;
const SHORT_SAMPLES: usize = 100_000;
const REPEAT_SAMPLES: usize = RATE_HZ / 200;

#[derive(Clone, Copy)]
struct Signal {
    ss_ref_hz: u64,
    spacing_hz: u32,
    cell_group: usize,
    identity: usize,
    carrier_error_hz: f64,
    first_start: f64,
}

impl Signal {
    fn profile(self) -> String {
        format!("{}:{}", self.ss_ref_hz, self.spacing_hz / 1_000)
    }

    fn cell_id(self) -> usize {
        3 * self.cell_group + self.identity
    }
}

const SIGNALS: [Signal; 2] = [
    Signal {
        ss_ref_hz: 912_000_000,
        spacing_hz: 15_000,
        cell_group: 79,
        identity: 2,
        carrier_error_hz: 3_200.0,
        first_start: 20_813.25,
    },
    Signal {
        ss_ref_hz: 918_000_000,
        spacing_hz: 30_000,
        cell_group: 271,
        identity: 1,
        carrier_error_hz: -7_100.0,
        first_start: 25_107.625,
    },
];

fn sequence(mut register: [u8; 7], feedback_tap: usize) -> [f64; 127] {
    std::array::from_fn(|_| {
        let emitted = register[0];
        let feedback = emitted ^ register[feedback_tap];
        register.rotate_left(1);
        register[6] = feedback;
        if emitted == 0 { 1.0 } else { -1.0 }
    })
}

fn add_symbol(iq: &mut [Complex64], signal: Signal, useful_start: f64, tones: [f64; 127]) {
    let spacing = f64::from(signal.spacing_hz);
    let rate = RATE_HZ as f64;
    let useful_samples = rate / spacing;
    let prefix_samples = 18.0 * useful_samples / 256.0;
    let begin = (useful_start - prefix_samples).ceil() as usize;
    let end = (useful_start + useful_samples).ceil() as usize;
    let rf_offset_hz = signal.ss_ref_hz as f64 - CENTER_HZ as f64 + signal.carrier_error_hz;
    for (index, sample) in iq.iter_mut().enumerate().take(end).skip(begin) {
        let time_since_useful_start = (index as f64 - useful_start) / rate;
        // SS_REF lies on subcarrier 120. The 127 signal tones begin at 56.
        let baseband: Complex64 = (-64..=62)
            .zip(tones)
            .map(|(bin, amplitude)| {
                let phase = TAU * f64::from(bin) * spacing * time_since_useful_start;
                Complex64::from_polar(1.0, phase) * amplitude
            })
            .sum();
        let rf_phase = TAU * rf_offset_hz * index as f64 / rate + 0.37;
        *sample += baseband * Complex64::from_polar(14.0, rf_phase);
    }
}

fn fixture(signals: &[Signal]) -> Vec<u8> {
    let mut state = 0x582f_6da1_u32;
    let mut noise = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from((state >> 16) as u16) / 8192.0 - 4.0
    };
    let mut iq: Vec<_> = (0..SAMPLES)
        .map(|_| Complex64::new(noise(), noise()))
        .collect();
    let primary = sequence([0, 1, 1, 0, 1, 1, 1], 4);
    let secondary_first = sequence([1, 0, 0, 0, 0, 0, 0], 4);
    let secondary_second = sequence([1, 0, 0, 0, 0, 0, 0], 1);
    for &signal in signals {
        let pss = std::array::from_fn(|index| primary[(index + 43 * signal.identity) % 127]);
        let m0 = 15 * (signal.cell_group / 112) + 5 * signal.identity;
        let m1 = signal.cell_group % 112;
        let sss = std::array::from_fn(|index| {
            secondary_first[(index + m0) % 127] * secondary_second[(index + m1) % 127]
        });
        let symbol_with_prefix =
            RATE_HZ as f64 / f64::from(signal.spacing_hz) * (1.0 + 18.0 / 256.0);
        for repetition in 0..3 {
            let start = signal.first_start + (repetition * REPEAT_SAMPLES) as f64;
            add_symbol(&mut iq, signal, start, pss);
            add_symbol(&mut iq, signal, start + 2.0 * symbol_with_prefix, sss);
        }
    }
    let mut bytes = Vec::with_capacity(SAMPLES * IQ_BYTES);
    for sample in iq {
        for component in [sample.re, sample.im] {
            assert!((-2048.0..=2047.0).contains(&component.round()));
            bytes.extend((component.round() as i16).to_le_bytes());
        }
    }
    bytes
}

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(arguments)
        .output()
        .unwrap()
}

fn success(result: &Output) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn read_report(root: &Path, output: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(output)).unwrap()).unwrap()
}

fn identify(root: &Path, signal: Signal, maximum: usize, output: &str) -> Value {
    success(&run(
        root,
        &[
            "identify-nr",
            "downlink.sc16q11",
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--profile",
            &signal.profile(),
            "--max-samples",
            &maximum.to_string(),
            "--output",
            output,
        ],
    ));
    read_report(root, output)
}

fn assert_channel(channel: &Value, signal: Signal, delay: u64) {
    let rate_hz = u64::from(signal.spacing_hz) * 256;
    assert_eq!(channel["channel_sample_rate_hz"], rate_hz);
    let cells = channel["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 1, "{channel}");
    assert_eq!(cells[0]["physical_cell_id"], signal.cell_id());
    assert_eq!(cells[0]["matched_spacing_ms"], 5);
    let observations = cells[0]["observations"].as_array().unwrap();
    assert_eq!(observations.len(), 3);
    let first = ((signal.first_start + delay as f64 + 1.0) * rate_hz as f64 / RATE_HZ as f64 - 1.0)
        .round() as u64;
    for (index, observation) in observations.iter().enumerate() {
        let expected = first + index as u64 * (rate_hz / 200);
        let observed = observation["pss_start_channel_sample"].as_u64().unwrap();
        assert!(observed.abs_diff(expected) <= 2, "{observation}");
        let carrier_error = observation["carrier_offset_hz"].as_f64().unwrap();
        assert!(
            (carrier_error - signal.carrier_error_hz).abs() <= 500.0,
            "{observation}"
        );
    }
}

#[test]
fn identifies_repeated_nr_sync_at_both_subcarrier_spacings() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let iq = fixture(&SIGNALS);
    fs::write(root.join("downlink.sc16q11"), &iq).unwrap();
    let maximum = 240_000;
    for signal in SIGNALS {
        let name = format!("nr-{}.json", signal.spacing_hz);
        let report = identify(root, signal, maximum, &name);
        assert_eq!(report["operation"], "identify_nr_in_saved_iq");
        assert_eq!(report["source_samples"], SAMPLES);
        assert_eq!(report["analyzed_samples"], maximum);
        assert_eq!(report["cell_reports"], 1);
        assert_eq!(
            report["analyzed_iq_sha256"],
            format!("{:x}", Sha256::digest(&iq[..maximum * IQ_BYTES]))
        );
        assert_channel(
            &report["channels"][0],
            signal,
            report["channel_filter_delay_input_samples"]
                .as_u64()
                .unwrap(),
        );
    }
    let single = identify(root, SIGNALS[0], 120_000, "one-block.json");
    assert_eq!(single["cell_reports"], 0);
    assert!(single["channels"][0]["sss_matches"].as_u64().unwrap() > 0);
}

fn write_receipt(path: &Path, value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn archive(root: &Path, iq: &[u8]) -> String {
    let capture = root.join("sweep");
    let mut tunes = Vec::new();
    for (index, samples) in [SAMPLES, SHORT_SAMPLES].into_iter().enumerate() {
        let directory = format!("tune-{:04}", index + 1);
        let tune = capture.join(&directory);
        fs::create_dir_all(&tune).unwrap();
        fs::write(tune.join("wideband.sc16q11"), &iq[..samples * IQ_BYTES]).unwrap();
        let center_hz = CENTER_HZ + index as u64;
        write_receipt(
            &tune.join("wideband.sc16q11.json"),
            &json!({
                "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q",
                "complex_scale": 2048, "frequency_hz": center_hz,
                "sample_rate_hz": RATE_HZ, "bandwidth_hz": RATE_HZ,
                "gain_db": 40, "gain_mode": "manual", "bias_tee_enabled": false,
                "channel": "RX1", "num_samples": samples, "started_utc": "2000-01-01T00:00:00Z"
            }),
        );
        tunes.push(json!({
            "directory": directory, "requested_center_hz": center_hz,
            "observed_center_hz": center_hz,
            "dwell_ms": samples / (RATE_HZ / 1_000),
            "observed_bands": [
                {"lower_hz": center_hz - 8_000_000, "upper_hz": center_hz - 100_000},
                {"lower_hz": center_hz + 100_000, "upper_hz": center_hz + 8_000_000},
            ],
        }));
    }
    write_receipt(
        &capture.join("sweep.json"),
        &json!({
            "schema_version": 1, "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ, "upper_hz": CENTER_HZ + 2,
            "antenna": 1, "gain_db": 40, "dwell_ms": SAMPLES / (RATE_HZ / 1_000),
            "iq_bytes": (SAMPLES + SHORT_SAMPLES) * IQ_BYTES, "tunes": tunes,
        }),
    )
}

#[test]
fn sweep_replay_plans_guarded_profiles_before_reading_iq() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let mut iq = fixture(&SIGNALS);
    let sweep_sha256 = archive(root, &iq);
    let [narrow, wide] = SIGNALS.map(Signal::profile);
    let outside = "902000000:15";
    success(&run(
        root,
        &[
            "identify-nr-sweep",
            "sweep",
            "--profile",
            &narrow,
            "--profile",
            &wide,
            "--profile",
            outside,
            "--output",
            "sweep-nr.json",
        ],
    ));
    let report = read_report(root, "sweep-nr.json");
    assert_eq!(report["source_capture_sha256"], sweep_sha256);
    assert!(report.get("raster").is_none());
    assert!(report["profiles"][0].get("gscn").is_none());
    assert_eq!(report["cell_reports"], 2);
    assert_eq!(report["profiles"][0]["covered_dwells"], 0);
    assert_eq!(report["dwells"][1]["status"], "short_dwell");
    assert_eq!(report["dwells"][1]["selected_samples"], SHORT_SAMPLES);
    let replay = &report["dwells"][0]["replay"];
    let delay = replay["channel_filter_delay_input_samples"]
        .as_u64()
        .unwrap();
    for (channel, signal) in replay["channels"].as_array().unwrap().iter().zip(SIGNALS) {
        assert_channel(channel, signal, delay);
    }
    let budget = SAMPLES * (24 + 48) / 125;
    let combined_args = [
        "replay-sweep",
        "sweep",
        "--nr-profile",
        &narrow,
        "--nr-profile",
        &wide,
        "--nr-profile",
        outside,
    ];
    let mut combined = combined_args.to_vec();
    combined.extend(["--output", "combined.json"]);
    success(&run(root, &combined));
    assert_eq!(read_report(root, "combined.json")["nr"], report);

    let mut pbch = combined_args.to_vec();
    pbch.extend(["--nr-pbch", "--output", "pbch.json"]);
    success(&run(root, &pbch));
    let pbch = &read_report(root, "pbch.json")["nr"];
    assert!(report.get("pbch_reports").is_none());
    assert_eq!(pbch["cell_reports"], report["cell_reports"]);
    assert_eq!(pbch["pbch_reports"], 0);
    let pbch_replay = &pbch["dwells"][0]["replay"];
    assert_eq!(
        pbch_replay["analyzed_iq_sha256"],
        replay["analyzed_iq_sha256"]
    );
    assert_eq!(
        pbch_replay["channels"][0]["pbch"],
        json!({"status": "checked", "examined_observations": 3, "omitted_observations": 0, "frames": []})
    );
    assert_eq!(
        pbch_replay["channels"][1]["pbch"],
        json!({"status": "outside_guarded_sideband"})
    );
    let missing_selection = run(root, &["replay-sweep", "sweep", "--nr-pbch", "--plan-only"]);
    assert!(!missing_selection.status.success());
    assert!(
        String::from_utf8_lossy(&missing_selection.stderr)
            .contains("--nr-pbch requires --nr-profile or --nr-raster")
    );

    iq[..2].copy_from_slice(&4096_i16.to_le_bytes());
    fs::write(root.join("sweep/tune-0001/wideband.sc16q11"), iq).unwrap();
    let mut preview_args = combined_args.to_vec();
    preview_args.extend(["--nr-pbch", "--plan-only"]);
    let preview = run(root, &preview_args);
    success(&preview);
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview["total_replayed_iq_bytes"], SAMPLES * IQ_BYTES);
    assert_eq!(preview["nr_pbch_max_observations_per_profile"], 64);
    let work = &preview["analyzers"][0];
    assert_eq!(work["analyzer"], "nr");
    assert_eq!(work["eligible_attempts"], 2);
    assert_eq!(work["short_attempts"], 2);
    assert_eq!(work["budget_unit"], "channelized_samples");
    assert_eq!(work["budget_samples"], budget);

    let maximum = (budget - 1).to_string();
    let mut over_budget = combined_args.to_vec();
    over_budget.extend([
        "--max-total-channelized-samples",
        &maximum,
        "--output",
        "overflow.json",
    ]);
    let failed = run(root, &over_budget);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("exceeding --max-total-channelized-samples")
    );
    assert!(!root.join("overflow.json").exists());
}

#[test]
fn raster_replay_accounts_for_guarded_visits_and_preflights_iq() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let signals = SIGNALS.map(|signal| Signal {
        ss_ref_hz: signal.ss_ref_hz + 150_000,
        ..signal
    });
    let mut iq = fixture(&signals);
    let sweep_sha256 = archive(root, &iq);
    let mut arguments = vec![
        "identify-nr-sweep",
        "sweep",
        "--nr-raster",
        "--nr-raster-hz",
        "911950000:918150001",
        "--nr-scs-khz",
        "30",
        "--nr-scs-khz",
        "15",
        "--nr-scs-khz",
        "30",
    ];
    let mut standalone = arguments.clone();
    standalone.extend(["--output", "raster.json"]);
    success(&run(root, &standalone));
    let report = read_report(root, "raster.json");
    assert_eq!(report["source_capture_sha256"], sweep_sha256);
    assert_eq!(report["cell_reports"], 2);
    assert_eq!(report["raster"]["lower_hz"], 911_950_000);
    assert_eq!(report["raster"]["upper_hz"], 918_150_001);
    assert_eq!(
        report["raster"]["subcarrier_spacings_hz"],
        json!([15_000, 30_000])
    );
    let profiles = report["profiles"].as_array().unwrap();
    let keys = profiles.iter().map(|row| {
        (
            row["gscn"].as_u64().unwrap(),
            row["profile"]["subcarrier_spacing_hz"].as_u64().unwrap(),
        )
    });
    let expected = (2279..=2295).flat_map(|gscn| [15_000, 30_000].map(|spacing| (gscn, spacing)));
    assert!(keys.eq(expected));
    assert_eq!(report["dwells"][1]["status"], "short_dwell");

    let coverage = |gscn, spacing| {
        profiles
            .iter()
            .find(|row| row["gscn"] == gscn && row["profile"]["subcarrier_spacing_hz"] == spacing)
            .unwrap()
    };
    let overlapping = coverage(2280, 15_000);
    assert_eq!(overlapping["covered_dwells"], 2);
    assert_eq!(overlapping["short_dwells"], 1);
    assert_eq!(overlapping["replayed_samples"], SAMPLES);
    assert_eq!(coverage(2286, 15_000)["covered_dwells"], 0);
    // This reference fits the longer tune exactly and misses the shifted short tune by one hertz.
    let edge = coverage(2293, 30_000);
    assert_eq!(edge["profile"]["ss_ref_hz"], 917_050_000);
    assert_eq!(edge["covered_dwells"], 1);
    assert_eq!(edge["short_dwells"], 0);

    let replay = &report["dwells"][0]["replay"];
    let delay = replay["channel_filter_delay_input_samples"]
        .as_u64()
        .unwrap();
    for signal in signals {
        let channel = replay["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| {
                channel["profile"]["ss_ref_hz"] == signal.ss_ref_hz
                    && channel["profile"]["subcarrier_spacing_hz"] == signal.spacing_hz
            })
            .unwrap();
        assert_channel(channel, signal, delay);
    }
    let budget = SAMPLES * (11 * 24 + 6 * 48) / 125;
    let work = &report["work"];
    assert_eq!(work["replayed_iq_samples"], SAMPLES);
    assert_eq!(work["eligible_attempts"], 17);
    assert_eq!(work["short_attempts"], 16);
    assert_eq!(work["budget_samples"], budget);

    arguments[0] = "replay-sweep";
    let mut combined = arguments.clone();
    combined.extend(["--output", "combined-raster.json"]);
    success(&run(root, &combined));
    assert_eq!(read_report(root, "combined-raster.json")["nr"], report);

    iq[..2].copy_from_slice(&4096_i16.to_le_bytes());
    fs::write(root.join("sweep/tune-0001/wideband.sc16q11"), iq).unwrap();
    arguments[0] = "identify-nr-sweep";
    let mut preview_args = arguments.clone();
    preview_args.push("--plan-only");
    let preview = run(root, &preview_args);
    success(&preview);
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    for field in ["source_capture_sha256", "raster", "profiles", "work"] {
        assert_eq!(preview[field], report[field], "{field}");
    }

    let inferred = run(
        root,
        &["identify-nr-sweep", "sweep", "--nr-raster", "--plan-only"],
    );
    success(&inferred);
    let inferred: Value = serde_json::from_slice(&inferred.stdout).unwrap();
    assert_eq!(inferred["raster"]["lower_hz"], 907_000_000);
    assert_eq!(inferred["raster"]["upper_hz"], 923_000_001);
    assert_eq!(
        inferred["raster"]["subcarrier_spacings_hz"],
        json!([15_000, 30_000])
    );

    preview_args[0] = "replay-sweep";
    let combined_preview = run(root, &preview_args);
    success(&combined_preview);
    let combined_preview: Value = serde_json::from_slice(&combined_preview.stdout).unwrap();
    assert_eq!(
        combined_preview["total_replayed_iq_bytes"],
        SAMPLES * IQ_BYTES
    );
    assert_eq!(combined_preview["analyzers"][0]["analyzer"], "nr");
    for field in ["eligible_attempts", "short_attempts", "budget_samples"] {
        assert_eq!(
            combined_preview["analyzers"][0][field], work[field],
            "{field}"
        );
    }

    preview_args[0] = "identify-nr-sweep";
    preview_args.extend(["--output", "conflict.json"]);
    let conflict = run(root, &preview_args);
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("--plan-only"));
    assert!(!root.join("conflict.json").exists());

    let maximum = (budget - 1).to_string();
    arguments.extend([
        "--max-total-channelized-samples",
        &maximum,
        "--output",
        "raster-overflow.json",
    ]);
    let failed = run(root, &arguments);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr)
            .contains("exceeding --max-total-channelized-samples")
    );
    assert!(!root.join("raster-overflow.json").exists());
}
