//! Verify Itron endpoint channels through saved-IQ and sweep replay.

use std::f64::consts::TAU;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CENTER_HZ: u64 = 914_000_000;
const SAMPLE_RATE_HZ: usize = 20_000_000;
const CHIP_RATE_HZ: usize = 32_768;
const DWELL_MS: usize = 16;
const DWELL_SAMPLES: usize = DWELL_MS * (SAMPLE_RATE_HZ / 1_000);
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";
const SIDECAR_FILE: &str = "tune-0001/wideband.sc16q11.json";
const PROFILE_FILE: &str = "scm-plus-profile.json";

struct Burst {
    channel_hz: u64,
    first_sample: usize,
    lead_in_chips: usize,
    frame: &'static [u8],
    endpoint_id: u32,
    endpoint_type: u8,
    crc: u16,
}

// Synthetic frames with independently calculated CRCs. The overlapping
// bursts occupy different carriers on either side of the receiver center.
const BURSTS: [Burst; 2] = [
    Burst {
        channel_hz: 912_000_000,
        first_sample: 60_000,
        lead_in_chips: 1,
        frame: &[
            0x16, 0xa3, 0x1e, 0xab, 0x89, 0xab, 0xcd, 0xef, 0x13, 0x57, 0x9b, 0xdf, 0x24, 0x68,
            0xa7, 0xbf,
        ],
        endpoint_id: 0x89ab_cdef,
        endpoint_type: 0xab,
        crc: 0xa7bf,
    },
    Burst {
        channel_hz: 917_000_000,
        first_sample: 80_000,
        lead_in_chips: 1,
        frame: &[
            0x16, 0xa3, 0x1e, 0x43, 0xfe, 0xdc, 0xba, 0x98, 0x0b, 0xad, 0xc0, 0xde, 0x12, 0x34,
            0x57, 0x6e,
        ],
        endpoint_id: 0xfedc_ba98,
        endpoint_type: 0x43,
        crc: 0x576e,
    },
];

fn iq_fixture(bursts: &[&Burst]) -> Vec<u8> {
    let mut samples = vec![[0_i16; 2]; DWELL_SAMPLES];
    for burst in bursts {
        let offset_hz = burst.channel_hz as f64 - CENTER_HZ as f64;
        // The chosen offsets complete an integer number of cycles in 20 samples.
        let tone: [[i16; 2]; 20] = std::array::from_fn(|index| {
            let phase = TAU * offset_hz * index as f64 / SAMPLE_RATE_HZ as f64;
            let (sin, cos) = phase.sin_cos();
            [(875.0 * cos).round() as i16, (875.0 * sin).round() as i16]
        });
        for (index, sample) in samples[burst.first_sample..].iter_mut().enumerate() {
            let chip = index * CHIP_RATE_HZ / SAMPLE_RATE_HZ;
            // The SCM+ examples have a keyed lead-in before their sync.
            let keyed = if chip < burst.lead_in_chips {
                true
            } else {
                let frame_chip = chip - burst.lead_in_chips;
                let bit = frame_chip / 2;
                let Some(byte) = burst.frame.get(bit / 8) else {
                    break;
                };
                let one = byte & (0x80 >> (bit % 8)) != 0;
                one == frame_chip.is_multiple_of(2)
            };
            if keyed {
                let [i, q] = tone[index % tone.len()];
                sample[0] += i;
                sample[1] += q;
            }
        }
    }
    samples
        .into_iter()
        .flatten()
        .flat_map(i16::to_le_bytes)
        .collect()
}

fn write_json(root: &Path, name: &str, document: Value) -> String {
    let bytes = serde_json::to_vec(&document).unwrap();
    fs::write(root.join(name), &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn write_sweep(root: &Path, iq: &[u8]) -> (String, String) {
    fs::create_dir(root.join("tune-0001")).unwrap();
    fs::write(root.join(IQ_FILE), iq).unwrap();
    let sidecar = write_json(
        root,
        SIDECAR_FILE,
        json!({
            "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q", "complex_scale": 2048,
            "frequency_hz": CENTER_HZ, "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ, "gain_db": 40, "gain_mode": "manual",
            "bias_tee_enabled": false, "channel": "RX1", "num_samples": DWELL_SAMPLES,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep = write_json(
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
    (sweep, sidecar)
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn read_report(root: &Path, args: &[&str], name: &str) -> Value {
    let mut command = args.to_vec();
    command.extend(["--output", name]);
    let result = run(root, &command);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap()
}

fn read_channel(root: &Path, channel_hz: u64) -> Value {
    read_report(
        root,
        &[
            "identify-scm-plus",
            IQ_FILE,
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &channel_hz.to_string(),
        ],
        &format!("scalar-{channel_hz}.json"),
    )
}

#[test]
fn two_scm_plus_channels_share_one_bound_iq_prefix_and_keep_separate_budgets() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let iq = iq_fixture(&BURSTS.each_ref());
    let iq_digest = format!("{:x}", Sha256::digest(&iq));
    let (sweep_digest, sidecar_digest) = write_sweep(root, &iq);
    let [first_hz, second_hz] = BURSTS.each_ref().map(|burst| burst.channel_hz);
    let profile_digest = write_json(
        root,
        PROFILE_FILE,
        json!({ "schema_version": 1, "scm_plus_channel_hz": [second_hz, CENTER_HZ, first_hz] }),
    );

    let limit = 2 * DWELL_SAMPLES;
    let dedicated = read_report(
        root,
        &[
            "identify-scm-plus-sweep",
            ".",
            "--channel-hz",
            &second_hz.to_string(),
            "--channel-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &first_hz.to_string(),
            "--max-total-samples",
            &limit.to_string(),
        ],
        "dedicated.json",
    );
    assert_eq!(dedicated["source_capture_sha256"], sweep_digest);
    assert_eq!(dedicated["decoded_samples"], limit);
    assert_eq!(dedicated["validated_frames"], 2, "{dedicated}");
    let channels = dedicated["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 3);
    assert_eq!(channels[1]["channel_frequency_hz"], CENTER_HZ);
    assert_eq!(channels[1]["covered_dwells"], 0);
    assert_eq!(channels[1]["attempts"], json!([]));

    for (burst, channel) in BURSTS.iter().zip([&channels[0], &channels[2]]) {
        assert_eq!(channel["channel_frequency_hz"], burst.channel_hz);
        assert_eq!(channel["covered_dwells"], 1);
        let attempt = &channel["attempts"][0];
        assert_eq!(attempt["source_sweep_sha256"], sweep_digest);
        assert_eq!(attempt["source_sidecar_sha256"], sidecar_digest);
        let scalar = read_channel(root, burst.channel_hz);
        assert_eq!(attempt["replay"], scalar);
        assert_eq!(scalar["analyzed_iq_sha256"], iq_digest);
        assert_eq!(scalar["analyzed_samples"], DWELL_SAMPLES);
        assert_eq!(scalar["validated_frames"], 1, "{scalar}");
        let frame = &scalar["frames"][0];
        assert_eq!(frame["endpoint_id"], burst.endpoint_id);
        assert_eq!(frame["endpoint_type"], burst.endpoint_type);
        assert_eq!(frame["crc16"], burst.crc);
        let expected_sample = |chip| burst.first_sample + chip * SAMPLE_RATE_HZ / CHIP_RATE_HZ;
        let allowance = SAMPLE_RATE_HZ.div_ceil(CHIP_RATE_HZ);
        let start = frame["first_input_sample"].as_u64().unwrap() as usize;
        let end = frame["end_input_sample_exclusive"].as_u64().unwrap() as usize;
        assert!(start.abs_diff(expected_sample(burst.lead_in_chips)) <= allowance);
        assert!(
            end.abs_diff(expected_sample(
                burst.lead_in_chips + burst.frame.len() * 16
            )) <= allowance
        );
    }

    // Reject the adjacent selection and an image at one output-sample rate.
    for offset_hz in [200_000, 500_000] {
        let wrong_channel = first_hz + offset_hz;
        let report = read_channel(root, wrong_channel);
        assert_eq!(report["analyzed_iq_sha256"], iq_digest);
        assert_eq!(report["validated_frames"], 0, "{wrong_channel}: {report}");
    }

    let replay = [
        "replay-sweep",
        ".",
        "--signal-profile",
        PROFILE_FILE,
        "--share-iq",
    ];
    let mut args = replay.to_vec();
    let max_samples = limit.to_string();
    args.extend(["--max-total-samples", &max_samples]);
    let combined = read_report(root, &args, "combined.json");
    assert_eq!(combined["scm_plus"], dedicated);
    assert_eq!(combined["signal_profile_sha256"], profile_digest);
    assert_eq!(
        combined["shared_iq"],
        json!({
            "source_iq_bytes": iq.len(),
            "max_buffered_iq_bytes": iq.len(),
            "shared_dwells": 1,
        })
    );

    // An invalid IQ word distinguishes budget refusal from a payload read.
    fs::OpenOptions::new()
        .write(true)
        .open(root.join(IQ_FILE))
        .unwrap()
        .write_all(&i16::MAX.to_le_bytes())
        .unwrap();
    let mut args = replay.to_vec();
    let short_budget = (limit - 1).to_string();
    args.extend([
        "--max-total-samples",
        &short_budget,
        "--output",
        "refused.json",
    ]);
    let result = run(root, &args);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("SCM+ replay needs") && error.contains("--max-total-samples"),
        "{error}"
    );
    assert!(!root.join("refused.json").exists());
}

#[test]
fn legacy_scm_and_scm_plus_retain_distinct_identities_in_one_shared_dwell() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let legacy = Burst {
        channel_hz: BURSTS[0].channel_hz,
        first_sample: BURSTS[0].first_sample,
        lead_in_chips: 0,
        frame: &[
            0xf9, 0x53, 0x07, 0xa7, 0x00, 0x00, 0x00, 0x89, 0xab, 0xcd, 0x48, 0x92,
        ],
        endpoint_id: 0x0389_abcd,
        endpoint_type: 9,
        crc: 0x4892,
    };
    let plus = &BURSTS[1];
    let iq = iq_fixture(&[&legacy, plus]);
    write_sweep(root, &iq);
    let legacy_hz = legacy.channel_hz.to_string();
    let plus_hz = plus.channel_hz.to_string();
    let scalar = read_report(
        root,
        &[
            "identify-scm",
            IQ_FILE,
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &legacy_hz,
        ],
        "legacy.json",
    );
    assert_eq!(scalar["operation"], "identify_scm_in_saved_iq");
    assert_eq!(scalar["identifier_scope"], "scm_endpoint_id");
    assert_eq!(
        scalar["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(&iq))
    );
    assert_eq!(scalar["validated_frames"], 1, "{scalar}");
    let frame = &scalar["frames"][0];
    assert_eq!(frame["endpoint_id"], legacy.endpoint_id);
    assert_eq!(frame["endpoint_type"], legacy.endpoint_type);
    assert_eq!(frame["bch16"], legacy.crc);
    assert_eq!(frame["physical_tamper"], 2);
    assert_eq!(frame["encoder_tamper"], 3);
    assert!(frame.get("crc16").is_none());
    let dedicated = read_report(
        root,
        &[
            "identify-scm-sweep",
            ".",
            "--channel-hz",
            &legacy_hz,
            "--channel-hz",
            &plus_hz,
        ],
        "legacy-sweep.json",
    );
    assert_eq!(dedicated["operation"], "identify_scm_in_saved_sweeps");
    assert_eq!(dedicated["channels"][0]["attempts"][0]["replay"], scalar);
    assert_eq!(
        dedicated["channels"][1]["attempts"][0]["replay"]["validated_frames"],
        0
    );

    let channels = [legacy.channel_hz, plus.channel_hz];
    write_json(
        root,
        PROFILE_FILE,
        json!({ "schema_version": 1, "scm_channel_hz": channels, "scm_plus_channel_hz": channels }),
    );
    let shared = read_report(
        root,
        &[
            "replay-sweep",
            ".",
            "--signal-profile",
            PROFILE_FILE,
            "--share-iq",
        ],
        "shared.json",
    );
    let separate = read_report(
        root,
        &[
            "replay-sweep",
            ".",
            "--scm-channel-hz",
            &legacy_hz,
            "--scm-channel-hz",
            &plus_hz,
            "--scm-plus-channel-hz",
            &legacy_hz,
            "--scm-plus-channel-hz",
            &plus_hz,
        ],
        "separate.json",
    );
    assert_eq!(shared["scm"], dedicated);
    assert_eq!(separate["scm"], dedicated);
    assert_eq!(shared["scm_plus"], separate["scm_plus"]);
    assert_eq!(shared["scm_plus"]["validated_frames"], 1);
    let plus_channels = &shared["scm_plus"]["channels"];
    assert_eq!(
        plus_channels[0]["attempts"][0]["replay"]["validated_frames"],
        0
    );
    let plus_scalar = read_channel(root, plus.channel_hz);
    assert_eq!(plus_channels[1]["attempts"][0]["replay"], plus_scalar);
    assert_eq!(plus_scalar["frames"][0]["crc16"], plus.crc);
    assert!(plus_scalar["frames"][0].get("bch16").is_none());
    assert_eq!(shared["shared_iq"]["source_iq_bytes"], iq.len());
}
