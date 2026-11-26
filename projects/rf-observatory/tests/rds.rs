//! Exercise broadcast FM replay from an independently encoded IQ fixture.

use std::f64::consts::TAU;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE_HZ: f64 = 20_000_000.0;
const CENTER_HZ: u64 = 98_650_000;
const CHANNEL_HZ: u64 = 98_300_000;
const IQ_SAMPLE_BYTES: usize = 4;
const REPETITIONS: usize = 3;
const SECOND_CHANNEL_HZ: u64 = 100_100_000;
const MINIMUM_GROUP_SAMPLES: usize = 1_751_579;
// Wire blocks from the generator matrix in NRSC-4 Annex B.1.2, MSB first.
const WIRE_BLOCKS: [u32; 4] = [0x012d_4d65, 0x0086_9dd4, 0x0121_94c2, 0x01b1_b27b];
const INFORMATION_WORDS: [u16; 4] = [0x4b53, 0x21a7, 0x4865, 0x6c6c];
const SECOND_WIRE_BLOCKS: [u32; 4] = [0x014a_cf75, 0x0080_0237, 0x0131_84ef, 0x0188_34a3];
const SECOND_INFORMATION_WORDS: [u16; 4] = [0x52b3, 0x2000, 0x4c61, 0x620d];

fn broadcast_iq(center_hz: u64, channel_hz: u64, wire_blocks: [u32; 4], repeats: usize) -> Vec<u8> {
    let bits = std::iter::repeat_n(wire_blocks, repeats)
        .flatten()
        .flat_map(|block| (0..26).rev().map(move |bit| block & (1 << bit) != 0));
    let mut polarity = -1.0;
    let symbols: Vec<_> = std::iter::repeat_n(false, 12)
        .chain(bits)
        .chain(std::iter::repeat_n(false, 5))
        .map(|bit| {
            if bit {
                polarity = -polarity;
            }
            polarity
        })
        .collect();

    let subcarrier_hz = 57_005.0;
    let symbol_rate = subcarrier_hz / 48.0;
    let sample_count = (symbols.len() as f64 * SAMPLE_RATE_HZ / symbol_rate) as usize;
    let silence_samples = 14_797;
    let mut bytes = Vec::with_capacity((silence_samples + sample_count) * IQ_SAMPLE_BYTES);
    bytes.resize(silence_samples * IQ_SAMPLE_BYTES, 0);
    let offset_hz = channel_hz as f64 - center_hz as f64 + 231.0;
    let mut rf_phase = -0.42;
    for sample in 0..sample_count {
        let time = (sample as f64 + 0.5) / SAMPLE_RATE_HZ;
        let symbol_time = time * symbol_rate;
        let biphase = symbols[symbol_time as usize] * (TAU * symbol_time.fract()).sin();
        let deviation_hz = 3_300.0 * biphase * (TAU * subcarrier_hz * time + 1.17).cos()
            + 7_500.0 * (TAU * 19_000.0 * time).cos()
            + 11_000.0 * (TAU * 1_700.0 * time).sin();
        rf_phase += TAU * (offset_hz + deviation_hz) / SAMPLE_RATE_HZ;
        let (q, i) = rf_phase.sin_cos();
        bytes.extend_from_slice(&((900.0 * i).round() as i16).to_le_bytes());
        bytes.extend_from_slice(&((900.0 * q).round() as i16).to_le_bytes());
    }
    bytes
}

fn replay(
    root: &Path,
    input: &str,
    output: &str,
    center_hz: u64,
    channel_hz: u64,
    max_samples: usize,
) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-rds",
            input,
            "--center-hz",
            &center_hz.to_string(),
            "--channel-hz",
            &channel_hz.to_string(),
            "--max-samples",
            &max_samples.to_string(),
            "--output",
            output,
        ])
        .output()
        .unwrap()
}

#[test]
fn replay_binds_an_offset_broadcast_and_preserves_output_on_failure() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let input = "broadcast.sc16q11";
    let source = root.join(input);
    let mut iq = broadcast_iq(CENTER_HZ, CHANNEL_HZ, WIRE_BLOCKS, REPETITIONS);
    let prefix_bytes = iq.len();
    let analyzed_samples = prefix_bytes / IQ_SAMPLE_BYTES;
    iq.extend([0; 7 * IQ_SAMPLE_BYTES]);
    fs::write(&source, &iq).unwrap();

    let result = replay(
        root,
        input,
        "report.json",
        CENTER_HZ,
        CHANNEL_HZ,
        analyzed_samples,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report_path = root.join("report.json");
    let saved = fs::read(&report_path).unwrap();
    let report: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(report["operation"], "identify_rds_in_saved_iq");
    assert_eq!(report["source_samples"], analyzed_samples + 7);
    assert_eq!(report["analyzed_samples"], analyzed_samples);
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq[..prefix_bytes]))
    );
    assert_eq!(
        report["validated_groups"], REPETITIONS,
        "{}",
        report["groups"]
    );
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), REPETITIONS);
    for group in groups {
        assert_eq!(group["blocks"], json!(INFORMATION_WORDS));
    }

    let collision = replay(root, input, "report.json", CENTER_HZ, CHANNEL_HZ, 1);
    assert!(!collision.status.success());
    assert_eq!(fs::read(report_path).unwrap(), saved);

    fs::write(source, [0, 0, 0, 8]).unwrap();
    let invalid = replay(root, input, "invalid.json", CENTER_HZ, CHANNEL_HZ, 1);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("SC16 Q11"));
    assert!(!root.join("invalid.json").exists());
}

fn write_receipt(path: &Path, record: &Value) -> String {
    let bytes = serde_json::to_vec(record).unwrap();
    fs::write(path, &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn archive_sweep(root: &Path, observed_hz: u64, iq: &[u8]) -> (String, String) {
    let tune = root.join("tune-0001");
    fs::create_dir_all(&tune).unwrap();
    let sample_rate = SAMPLE_RATE_HZ as usize;
    let bytes_per_ms = IQ_SAMPLE_BYTES * (sample_rate / 1_000);
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
            "sample_rate_hz": sample_rate,
            "bandwidth_hz": sample_rate,
            "gain_db": 40,
            "gain_mode": "manual",
            "bias_tee_enabled": false,
            "channel": "RX1",
            "num_samples": iq.len() / IQ_SAMPLE_BYTES,
            "started_utc": "2000-01-01T00:00:00Z",
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

fn read_report(root: &Path, path: &str, result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(path)).unwrap()).unwrap()
}

#[test]
fn sweep_replays_station_bank_once_with_exact_scalar_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let actual_center = CENTER_HZ + 7;
    let mut iq = broadcast_iq(actual_center, CHANNEL_HZ, WIRE_BLOCKS, 1);
    let second = broadcast_iq(actual_center, SECOND_CHANNEL_HZ, SECOND_WIRE_BLOCKS, 1);
    assert_eq!(iq.len(), second.len());
    for (sample, &other) in iq
        .as_chunks_mut::<2>()
        .0
        .iter_mut()
        .zip(second.as_chunks::<2>().0)
    {
        *sample = (i16::from_le_bytes(*sample) + i16::from_le_bytes(other)).to_le_bytes();
    }
    drop(second);
    let prefix = iq.len() / IQ_SAMPLE_BYTES;
    let saved_samples = (prefix + 1).next_multiple_of(SAMPLE_RATE_HZ as usize / 1_000);
    let prefix_sha256 = format!("{:x}", Sha256::digest(&iq));
    iq.resize(saved_samples * IQ_SAMPLE_BYTES, 0);
    iq[prefix * IQ_SAMPLE_BYTES] = 1;
    let (sweep_sha256, sidecar_sha256) = archive_sweep(root, actual_center, &iq);
    drop(iq);

    let tuned = [
        CHANNEL_HZ,
        98_700_000,
        99_150_000,
        99_650_000,
        SECOND_CHANNEL_HZ,
    ];
    let requested = [
        107_900_000,
        SECOND_CHANNEL_HZ,
        tuned[2],
        CHANNEL_HZ,
        tuned[3],
        tuned[1],
        CHANNEL_HZ,
    ]
    .map(|frequency| frequency.to_string());
    let prefix_arg = prefix.to_string();
    let station_options = |flag: &'static str| {
        let mut options = vec!["--max-samples-per-dwell", prefix_arg.as_str()];
        for frequency in &requested {
            options.extend([flag, frequency.as_str()]);
        }
        options
    };
    let flags = station_options("--channel-hz");
    let charged = tuned.len() * prefix;
    let output = "sweep-rds.json";
    let result = sweep_command(root, "identify-rds-sweep", output, &flags);
    let report = read_report(root, output, result);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["operation"], "identify_rds_in_saved_sweeps");
    assert_eq!(report["capture_kind"], "sweep");
    assert_eq!(report["source_capture_sha256"], sweep_sha256);
    assert_eq!(report["minimum_group_samples"], MINIMUM_GROUP_SAMPLES);
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
    assert_eq!(report["station_samples"], charged);
    assert_eq!(report["validated_groups"], 2);
    assert_eq!(report["omitted_valid_groups"], 0);
    let stations = report["stations"].as_array().unwrap();
    assert_eq!(stations.len(), tuned.len() + 1);
    for (index, frequency) in tuned.into_iter().enumerate() {
        assert_eq!(stations[index]["channel_frequency_hz"], frequency);
        assert_eq!(stations[index]["covered_dwells"], 1);
        assert_eq!(stations[index]["covered_samples"], saved_samples);
        assert_eq!(stations[index]["replayed_samples"], prefix);
        assert_eq!(stations[index]["replayed_time_ns"], 50 * prefix);
    }
    let uncovered = stations.last().unwrap();
    assert_eq!(uncovered["channel_frequency_hz"], 107_900_000);
    assert_eq!(uncovered["covered_dwells"], 0);
    assert_eq!(uncovered["replayed_dwells"], 0);

    let dwells = report["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 1);
    let dwell = &dwells[0];
    assert_eq!(dwell["pass_index"], 1);
    assert!(dwell.get("pass_directory").is_none());
    assert_eq!(dwell["tune_directory"], "tune-0001");
    assert_eq!(dwell["source_sweep_sha256"], sweep_sha256);
    assert_eq!(dwell["source_sidecar_sha256"], sidecar_sha256);
    assert_eq!(dwell["observed_center_hz"], actual_center);
    assert_eq!(dwell["saved_samples"], saved_samples);
    assert_eq!(dwell["selected_samples"], prefix);
    assert_eq!(dwell["status"], "replayed");
    assert_eq!(dwell["channel_frequencies_hz"], json!(tuned));
    let bank = &dwell["replay"];
    assert_eq!(bank["analyzed_iq_sha256"], prefix_sha256);

    let input = "tune-0001/wideband.sc16q11";
    let evidence = bank["stations"].as_array().unwrap();
    assert_eq!(evidence.len(), tuned.len());
    for (index, channel_hz, expected_words) in [
        (0, CHANNEL_HZ, INFORMATION_WORDS),
        (4, SECOND_CHANNEL_HZ, SECOND_INFORMATION_WORDS),
    ] {
        let name = format!("scalar-{index}.json");
        let scalar = read_report(
            root,
            &name,
            replay(root, input, &name, actual_center, channel_hz, prefix),
        );
        assert_eq!(scalar["analyzed_samples"], prefix);
        assert_eq!(scalar["analyzed_iq_sha256"], prefix_sha256);
        assert_eq!(scalar["multiplex_sample_rate_hz"], 250_000);
        for field in [
            "channel_frequency_hz",
            "validated_groups",
            "omitted_valid_groups",
            "groups",
            "text",
        ] {
            assert_eq!(
                evidence[index][field], scalar[field],
                "station {channel_hz} field {field}"
            );
        }
        assert_eq!(
            evidence[index]["validated_groups"], 1,
            "{}",
            evidence[index]
        );
        assert_eq!(
            evidence[index]["groups"][0]["blocks"],
            json!(expected_words)
        );
    }
    for (station, channel_hz) in evidence[1..4].iter().zip(tuned[1..4].iter().copied()) {
        assert_eq!(station["channel_frequency_hz"], channel_hz);
        assert_eq!(station["validated_groups"], 0);
    }
    assert_eq!(evidence[4]["text"][0]["text"], "Lab");

    let combined_flags = station_options("--rds-channel-hz");
    let result = sweep_command(root, "replay-sweep", "combined.json", &combined_flags);
    let combined = read_report(root, "combined.json", result);
    assert_eq!(combined["operation"], "replay_saved_sweep");
    assert_eq!(combined["rds"], report);
    for key in ["aprs", "adsb", "lora", "lte"] {
        assert!(combined.get(key).is_none());
    }

    let raster_output = "raster-rds.json";
    let result = sweep_command(
        root,
        "identify-rds-sweep",
        raster_output,
        &[
            "--raster-hz",
            "98300000:100100000:900000",
            "--max-samples-per-dwell",
            &prefix_arg,
        ],
    );
    let raster = read_report(root, raster_output, result);
    assert_eq!(raster["validated_groups"], 2);
    assert_eq!(raster["station_samples"], 3 * prefix);
    let raster_dwell = &raster["dwells"][0];
    assert_eq!(
        raster_dwell["channel_frequencies_hz"],
        json!([CHANNEL_HZ, 99_200_000, SECOND_CHANNEL_HZ])
    );
    assert_eq!(raster_dwell["replay"]["analyzed_iq_sha256"], prefix_sha256);
    let raster_stations = raster_dwell["replay"]["stations"].as_array().unwrap();
    assert_eq!(raster_stations[0], evidence[0]);
    assert_eq!(raster_stations[2], evidence[4]);

    let original = fs::read(root.join(output)).unwrap();
    let short_arg = (MINIMUM_GROUP_SAMPLES - 1).to_string();
    let short_flags = [
        "--channel-hz",
        "98300000",
        "--max-samples-per-dwell",
        &short_arg,
    ];
    let collision = sweep_command(root, "identify-rds-sweep", output, &short_flags);
    let stderr = String::from_utf8_lossy(&collision.stderr);
    assert!(!collision.status.success());
    assert!(stderr.contains("without replacing output"), "{stderr}");
    assert_eq!(fs::read(root.join(output)).unwrap(), original);

    // Keep the saved extent valid and make the first sample fail IQ validation.
    fs::OpenOptions::new()
        .write(true)
        .open(root.join(input))
        .unwrap()
        .write_all(&2_048_i16.to_le_bytes())
        .unwrap();
    let budget_arg = (charged - 1).to_string();
    let mut options = flags;
    options.extend(["--max-total-station-samples", &budget_arg]);
    let refused = sweep_command(root, "identify-rds-sweep", "over-budget.json", &options);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(stderr.contains("--max-total-station-samples"), "{stderr}");
    assert!(stderr.contains(&charged.to_string()), "{stderr}");
    assert!(!root.join("over-budget.json").exists());

    let result = sweep_command(root, "identify-rds-sweep", "short.json", &short_flags);
    let short = read_report(root, "short.json", result);
    assert_eq!(short["covered_dwells"], 1);
    assert_eq!(short["short_dwells"], 1);
    assert_eq!(short["station_samples"], 0);
    assert_eq!(
        short["dwells"][0]["selected_samples"],
        MINIMUM_GROUP_SAMPLES - 1
    );
    assert_eq!(short["dwells"][0]["status"], "short_dwell");
    assert!(short["dwells"][0].get("replay").is_none());

    let minimum = MINIMUM_GROUP_SAMPLES.to_string();
    let invalid = sweep_command(
        root,
        "identify-rds-sweep",
        "invalid.json",
        &[
            "--channel-hz",
            "98300000",
            "--max-samples-per-dwell",
            &minimum,
        ],
    );
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(!invalid.status.success());
    assert!(stderr.contains("outside SC16 Q11"), "{stderr}");
    assert!(!root.join("invalid.json").exists());

    verify_raster_preview_and_limits(root, prefix);
}

fn verify_raster_preview_and_limits(root: &Path, prefix: usize) {
    const RASTER: &str = "98300000:100100000:50000";
    const CANDIDATES: usize = 37;
    let profile_sha256 = write_receipt(
        &root.join("raster-profile.json"),
        &json!({"schema_version": 1, "rds_raster_hz": RASTER}),
    );
    let prefix_arg = prefix.to_string();
    // The invalid first IQ sample from the parent test also guards these previews.
    let preview = |options: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
            .current_dir(root)
            .args([
                "replay-sweep",
                ".",
                "--plan-only",
                "--max-samples-per-dwell",
                &prefix_arg,
            ])
            .args(options)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<Value>(&result.stdout).unwrap()
    };
    let command = preview(&["--rds-raster-hz", RASTER]);
    let mut profile = preview(&["--signal-profile", "raster-profile.json"]);
    let profile_digest = profile
        .as_object_mut()
        .unwrap()
        .remove("signal_profile_sha256")
        .unwrap();
    assert_eq!(profile_digest, profile_sha256);
    assert_eq!(profile, command);
    let analyzers = command["analyzers"].as_array().unwrap();
    assert_eq!(analyzers.len(), 1);
    let work = &analyzers[0];
    let charged = CANDIDATES * prefix;
    assert_eq!(work["analyzer"], "rds");
    assert_eq!(work["eligible_attempts"], CANDIDATES);
    assert_eq!(work["budget_samples"], charged);
    assert_eq!(work["replayed_iq_samples"], prefix);

    let refused = sweep_command(
        root,
        "replay-sweep",
        "raster-over-budget.json",
        &[
            "--rds-raster-hz",
            RASTER,
            "--max-samples-per-dwell",
            &prefix_arg,
            "--max-total-station-samples",
            &(charged - 1).to_string(),
        ],
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(stderr.contains("--max-total-station-samples"), "{stderr}");
    assert!(stderr.contains(&charged.to_string()), "{stderr}");
    assert!(!root.join("raster-over-budget.json").exists());

    for (action, raster, channel) in [
        ("identify-rds-sweep", "--raster-hz", "--channel-hz"),
        ("replay-sweep", "--rds-raster-hz", "--rds-channel-hz"),
    ] {
        let bad_output = "conflicting-rds.json";
        let failed = sweep_command(
            root,
            action,
            bad_output,
            &[raster, RASTER, channel, "98300000"],
        );
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(!failed.status.success());
        assert!(stderr.contains("cannot be used with"), "{stderr}");
        assert!(!root.join(bad_output).exists());
    }
}
