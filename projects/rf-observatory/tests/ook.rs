//! Exercise OOK identification and mixed wideband profile replay through the CLI.

use std::f64::consts::TAU;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const CENTER_HZ: u64 = 431_920_000;
const CHANNEL_HZ: u64 = 433_920_000;
const EFERGY_CHANNEL_HZ: u64 = 433_550_000;
const SAMPLES_PER_US: usize = 20;
const COMPLEX_BYTES: usize = 4;
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";
const PROFILE_FILE: &str = "sensor-profile.json";
const LORA_PROFILE: &str = "433920000:125000:7";
// Synthetic frames for known IDs, with independently calculated check bytes.
const ACURITE_FIRST: [u8; 8] = [0xc4, 0xa3, 0x78, 0x03, 0x88, 0x60, 0x2d, 0xf7];
const ACURITE_REPEAT: [u8; 8] = [0xd4, 0xa3, 0x78, 0x03, 0x88, 0x60, 0x2d, 0x07];
const LACROSSE: [u8; 5] = [0x5a, 0x21, 0x7c, 0x37, 0xb7];

struct IqFixture {
    output: BufWriter<File>,
    digest: Sha256,
    noise: u32,
    duration_us: usize,
    tones: [[(i16, i16); 10]; 2],
}

impl IqFixture {
    fn new(path: &Path) -> Self {
        let tones = [32.0, 336.0].map(|amplitude| {
            // At a +2 MHz offset, ten wideband samples complete one cycle.
            std::array::from_fn(|index| {
                let (q, i) = (TAU * index as f64 / 10.0).sin_cos();
                (
                    (amplitude * i).round() as i16,
                    (amplitude * q).round() as i16,
                )
            })
        });
        Self {
            output: BufWriter::new(File::create(path).unwrap()),
            digest: Sha256::new(),
            noise: 73,
            duration_us: 0,
            tones,
        }
    }

    fn level(&mut self, duration_us: usize, keyed: bool) {
        let mut tone = self.tones[usize::from(keyed)].into_iter().cycle();
        self.write_level(duration_us, || tone.next().unwrap());
    }

    fn write_level(&mut self, duration_us: usize, mut next_sample: impl FnMut() -> (i16, i16)) {
        self.duration_us += duration_us;
        let mut bytes = [0; SAMPLES_PER_US * COMPLEX_BYTES];
        for _ in 0..duration_us {
            for slot in bytes.as_chunks_mut::<4>().0 {
                let (i, q) = next_sample();
                self.noise = self
                    .noise
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                let i = i + ((self.noise >> 28) as i16 - 8);
                let q = q + (((self.noise >> 24) & 15) as i16 - 8);
                let [i_lo, i_hi] = i.to_le_bytes();
                let [q_lo, q_hi] = q.to_le_bytes();
                *slot = [i_lo, i_hi, q_lo, q_hi];
            }
            self.output.write_all(&bytes).unwrap();
            self.digest.update(bytes);
        }
    }

    fn pulse(&mut self, high_us: usize, low_us: usize) {
        self.level(high_us, true);
        self.level(low_us, false);
    }

    fn frame(
        &mut self,
        bytes: &[u8],
        sync_us: (usize, usize),
        highs_us: [usize; 2],
        period_us: usize,
    ) {
        for _ in 0..4 {
            self.pulse(sync_us.0, sync_us.1);
        }
        for &byte in bytes {
            for shift in (0..8).rev() {
                let high_us = highs_us[usize::from((byte >> shift) & 1)];
                self.pulse(high_us, period_us - high_us);
            }
        }
    }

    fn acurite(&mut self, bytes: &[u8]) {
        self.frame(bytes, (620, 596), [220, 408], 612);
        self.pulse(220, 580);
    }

    fn govee(&mut self, bytes: [u8; 6]) {
        for (index, bit) in bytes
            .into_iter()
            .flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1))
            .enumerate()
        {
            let high_us = [296, 856][usize::from(bit)];
            let period_us = if index == 47 { 2_252 } else { 1_120 };
            self.pulse(high_us, period_us - high_us);
        }
        self.pulse(296, 8_660);
    }

    fn efergy(&mut self, bytes: [u8; 8]) {
        let mut angle = 0.0f64;
        let mut tone = |relative_hz: f64, width_us| {
            let offset_hz = (EFERGY_CHANNEL_HZ - CENTER_HZ) as f64 + relative_hz;
            let step = TAU * offset_hz / f64::from(SAMPLE_RATE_HZ);
            self.write_level(width_us, || {
                let (q, i) = angle.sin_cos();
                angle = (angle + step).rem_euclid(TAU);
                ((336.0 * i).round() as i16, (336.0 * q).round() as i16)
            });
        };
        tone(38_000.0, 500);
        for bit in bytes
            .into_iter()
            .flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1))
        {
            let width_us = [136, 64][usize::from(bit)];
            tone(-58_000.0, width_us);
            tone(38_000.0, 200 - width_us);
        }
    }

    fn finish(mut self) -> String {
        self.level(
            self.duration_us.next_multiple_of(1_000) - self.duration_us,
            false,
        );
        self.output.flush().unwrap();
        format!("{:x}", self.digest.finalize())
    }
}

fn identify(root: &Path, channel_hz: u64, limit: usize, output: &str) -> Value {
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-ook",
            IQ_FILE,
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &channel_hz.to_string(),
            "--sample-rate-hz",
            &SAMPLE_RATE_HZ.to_string(),
            "--max-transmissions",
            &limit.to_string(),
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

fn write_json(root: &Path, path: &str, value: Value) -> String {
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(root.join(path), &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn write_targeted_receipts(root: &Path, samples: u64) -> (String, String) {
    let sidecar = write_json(
        root,
        "tune-0001/wideband.sc16q11.json",
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
            "num_samples": samples,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep = write_json(
        root,
        "sweep.json",
        json!({
            "schema_version": 1,
            "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ,
            "upper_hz": CENTER_HZ + 1,
            "antenna": 1,
            "gain_db": 40,
            "dwell_ms": samples / u64::from(SAMPLE_RATE_HZ / 1_000),
            "iq_bytes": samples * COMPLEX_BYTES as u64,
            "tunes": [{
                "directory": "tune-0001",
                "requested_center_hz": CENTER_HZ,
                "observed_center_hz": CENTER_HZ,
                "observed_bands": [
                    {"lower_hz": CENTER_HZ - 8_000_000, "upper_hz": CENTER_HZ - 100_000},
                    {"lower_hz": CENTER_HZ + 100_000, "upper_hz": CENTER_HZ + 8_000_000}
                ]
            }],
        }),
    );
    (sweep, sidecar)
}

fn identify_sweep(root: &Path, max_samples: u64, output: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-ook-sweep",
            ".",
            "--channel-hz",
            &CHANNEL_HZ.to_string(),
            "--max-total-samples",
            &max_samples.to_string(),
            "--output",
            output,
        ])
        .output()
        .unwrap()
}

fn replay_sweep(root: &Path, max_samples: u64, options: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "replay-sweep",
            ".",
            "--max-total-samples",
            &max_samples.to_string(),
        ])
        .args(options)
        .output()
        .unwrap()
}

fn parse_preview(result: Output) -> Value {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

fn verify_profile_replay(root: &Path, samples: u64, standalone: &Value) {
    let profile_digest = write_json(
        root,
        PROFILE_FILE,
        json!({
            "schema_version": 1,
            "ook_channel_hz": CHANNEL_HZ,
            "lora_profile": [LORA_PROFILE],
        }),
    );
    let channel_hz = CHANNEL_HZ.to_string();
    let preview = |selection: &[&str]| {
        let mut options = vec!["--plan-only", "--share-iq"];
        options.extend_from_slice(selection);
        parse_preview(replay_sweep(root, samples, &options))
    };
    let mut profile = preview(&["--signal-profile", PROFILE_FILE]);
    let recorded_digest = profile
        .as_object_mut()
        .unwrap()
        .remove("signal_profile_sha256")
        .unwrap();
    assert_eq!(recorded_digest, profile_digest);
    assert_eq!(
        profile,
        preview(&[
            "--ook-channel-hz",
            &channel_hz,
            "--lora-profile",
            LORA_PROFILE
        ])
    );
    assert_eq!(
        profile["source_capture_sha256"],
        standalone["source_capture_sha256"]
    );
    let analyzers = profile["analyzers"].as_array().unwrap();
    assert_eq!(analyzers.len(), 2);
    let ook = analyzers
        .iter()
        .find(|row| row["analyzer"] == "ook")
        .unwrap();
    assert_eq!(
        *ook,
        json!({
            "analyzer": "ook",
            "eligible_attempts": 1,
            "short_attempts": 0,
            "replayed_iq_samples": samples,
            "budget_samples": samples,
            "max_budget_samples": samples,
            "budget_unit": "iq_samples",
        })
    );
    let bytes = samples * COMPLEX_BYTES as u64;
    assert_eq!(profile["total_replayed_iq_bytes"], bytes * 2);
    assert_eq!(
        profile["shared_iq"],
        json!({
            "source_iq_bytes": bytes,
            "max_buffered_iq_bytes": bytes,
            "shared_dwells": 1,
        })
    );

    let result = replay_sweep(
        root,
        samples,
        &[
            "--signal-profile",
            PROFILE_FILE,
            "--share-iq",
            "--output",
            "combined.json",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let combined: Value =
        serde_json::from_slice(&fs::read(root.join("combined.json")).unwrap()).unwrap();
    assert_eq!(combined["signal_profile_sha256"], profile_digest);
    assert_eq!(combined["shared_iq"], profile["shared_iq"]);
    assert_eq!(combined["ook"], *standalone);

    let conflict = replay_sweep(
        root,
        samples,
        &[
            "--plan-only",
            "--signal-profile",
            PROFILE_FILE,
            "--ook-channel-hz",
            &channel_hz,
        ],
    );
    assert!(!conflict.status.success());
    let stderr = String::from_utf8_lossy(&conflict.stderr);
    assert!(
        stderr.contains("--signal-profile") && stderr.contains("--ook-channel-hz"),
        "{stderr}"
    );
}

#[test]
fn ook_profile_raises_dwell_and_bounds_replay_before_acquisition() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let plan = |limit: u64| {
        Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
            .current_dir(root)
            .args([
                "sweep",
                "--signal-profile",
                PROFILE_FILE,
                "--replay",
                "--plan-only",
                "--dwell-ms",
                "1",
                "--passes",
                "2",
                "--max-total-samples",
                &limit.to_string(),
            ])
            .output()
            .unwrap()
    };
    let cases = [
        (
            json!({"schema_version": 1, "ook_channel_hz": CHANNEL_HZ}),
            60,
        ),
        (
            json!({
                "schema_version": 1,
                "ook_channel_hz": CHANNEL_HZ,
                "ook_govee_h5054": true,
            }),
            250,
        ),
        (
            json!({
                "schema_version": 1,
                "ook_channel_hz": CHANNEL_HZ,
                "ook_govee_h5054": true,
                "ook_oregon_scientific": true,
            }),
            400,
        ),
    ];
    for (profile, dwell_ms) in cases {
        let digest = write_json(root, PROFILE_FILE, profile);
        let replay_samples = 2 * dwell_ms * u64::from(SAMPLE_RATE_HZ / 1_000);
        let reported = parse_preview(plan(replay_samples));
        assert_eq!(reported["signal_profile_sha256"], digest);
        assert_eq!(
            reported["planned_iq_bytes"],
            replay_samples * COMPLEX_BYTES as u64
        );
        let centers = reported["centers_hz"].as_array().unwrap();
        assert_eq!(centers.len(), 1);
        assert_eq!(
            reported["dwell_ms_by_center"][centers[0].to_string()],
            dwell_ms
        );
        assert_eq!(
            reported["replay_preflight"]["analyzers"],
            json!([{
                "analyzer": "ook",
                "maximum_budget_samples": replay_samples,
                "max_budget_samples": replay_samples,
                "budget_unit": "iq_samples",
            }])
        );

        let rejected = plan(replay_samples - 1);
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            stderr.contains("ook") && stderr.contains("--max-total-samples"),
            "{stderr}"
        );
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
    }
}

#[test]
fn identifies_checked_govee_rows_across_burst_boundaries_in_saved_replay() {
    const LEGACY: [u8; 6] = [0x12, 0x34, 0xfb, 0xfb, 0xf8, 0xa7];
    const CRC: [u8; 6] = [0x56, 0x78, 0x31, 0x64, 0x37, 0x9d];
    const PLACEHOLDER: [u8; 6] = [0xff, 0xff, 0x30, 0x54, 0x95, 0x34];
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("tune-0001")).unwrap();
    let mut fixture = IqFixture::new(&root.join(IQ_FILE));
    fixture.level(2_000, false);
    fixture.govee(LEGACY);
    fixture.govee(LEGACY);
    let mut corrupt = LEGACY;
    corrupt[0] ^= 4;
    fixture.govee(corrupt);
    fixture.govee(LEGACY);
    fixture.level(20_000, false);
    fixture.acurite(&ACURITE_FIRST);
    fixture.level(20_000, false);
    fixture.govee(LEGACY);
    fixture.level(20_000, false);
    let mut corrupt = CRC;
    corrupt[3] ^= 1;
    fixture.govee(corrupt);
    fixture.govee(CRC);
    fixture.level(20_000, false);
    fixture.govee(PLACEHOLDER);
    let digest = fixture.finish();

    let report = identify(root, CHANNEL_HZ, 4, "govee.json");
    assert_eq!(report["analyzed_iq_sha256"], digest);
    assert_eq!(report["validated_frame_copies"], 6, "{report}");
    assert_eq!(report["observed_transmissions"], 4);
    let observations = report["transmissions"].as_array().unwrap();
    let projected: Vec<_> = observations
        .iter()
        .map(|entry| {
            json!([
                entry["model"],
                entry["id"],
                entry["valid_copies"],
                entry["identifier_scope"],
                entry["integrity"],
                entry["event"],
                entry["event_data"],
            ])
        })
        .collect();
    assert_eq!(
        projected,
        [
            json!([
                "govee_h5054",
                0x1234,
                3,
                "protocol_sensor_id",
                "legacy_nibble_parity",
                "water_leak",
                0xfb
            ]),
            json!([
                "acurite_iris",
                0x4a3,
                1,
                "protocol_sensor_id",
                null,
                null,
                null
            ]),
            json!([
                "govee_h5054",
                0x5678,
                1,
                "protocol_sensor_id",
                "crc16_aug_ccitt",
                "battery_report",
                100
            ]),
            json!([
                "govee_h5054",
                null,
                1,
                "unattributed",
                "crc16_aug_ccitt",
                "button_press",
                0x54
            ]),
        ]
    );

    let samples = report["analyzed_samples"].as_u64().unwrap();
    write_targeted_receipts(root, samples);
    let profile_sha = write_json(
        root,
        PROFILE_FILE,
        json!({
            "schema_version": 1,
            "ook_channel_hz": CHANNEL_HZ,
            "ook_govee_h5054": true,
        }),
    );
    let output = replay_sweep(
        root,
        samples,
        &[
            "--signal-profile",
            PROFILE_FILE,
            "--share-iq",
            "--output",
            "govee-profile.json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined: Value =
        serde_json::from_slice(&fs::read(root.join("govee-profile.json")).unwrap()).unwrap();
    assert_eq!(combined["signal_profile_sha256"], profile_sha);
    assert_eq!(combined["ook"]["observed_transmissions"], 4);
    assert_eq!(combined["ook"]["dwells"][0]["replay"], report);

    let capped = identify(root, CHANNEL_HZ, 1, "govee-capped.json");
    assert_eq!(capped["validated_frame_copies"], 6);
    assert_eq!(capped["omitted_transmissions"], 3);
    assert_eq!(capped["transmissions"], json!(&observations[..1]));
}

#[test]
fn efergy_and_ook_plan_one_bounded_read_for_a_long_dwell() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("tune-0001")).unwrap();
    let source = File::create(root.join(IQ_FILE)).unwrap();
    write_json(
        root,
        PROFILE_FILE,
        json!({
            "schema_version": 1,
            "efergy_channel_hz": EFERGY_CHANNEL_HZ,
            "ook_channel_hz": CHANNEL_HZ,
        }),
    );

    for (dwell_ms, buffered_bytes) in [(500, 40_000_000), (501, 65_536)] {
        let samples = dwell_ms * u64::from(SAMPLE_RATE_HZ / 1_000);
        let iq_bytes = samples * COMPLEX_BYTES as u64;
        source.set_len(iq_bytes).unwrap();
        write_targeted_receipts(root, samples);
        let plan = parse_preview(replay_sweep(
            root,
            samples,
            &[
                "--signal-profile",
                PROFILE_FILE,
                "--share-iq",
                "--plan-only",
            ],
        ));
        assert_eq!(plan["total_replayed_iq_bytes"], 2 * iq_bytes);
        assert_eq!(
            plan["shared_iq"],
            json!({
                "source_iq_bytes": iq_bytes,
                "max_buffered_iq_bytes": buffered_bytes,
                "shared_dwells": 1,
            })
        );
    }
}

#[test]
fn efergy_and_ook_replay_one_shared_wideband_dwell() {
    // Synthetic ID 0x1234 with a six-second interval and byte-sum check.
    const EFERGY_FRAME: [u8; 8] = [0x0b, 0x34, 0x12, 0x40, 0x4f, 0x5b, 0x00, 0x3b];
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("tune-0001")).unwrap();
    let mut fixture = IqFixture::new(&root.join(IQ_FILE));
    fixture.level(2_000, false);
    fixture.efergy(EFERGY_FRAME);
    fixture.level(3_000, false);
    fixture.acurite(&ACURITE_FIRST);
    fixture.level(4_000, false);
    let digest = fixture.finish();
    let iq_bytes = fs::metadata(root.join(IQ_FILE)).unwrap().len();
    let samples = iq_bytes / COMPLEX_BYTES as u64;
    write_targeted_receipts(root, samples);

    let scalar = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args([
            "identify-efergy",
            IQ_FILE,
            "--center-hz",
            &CENTER_HZ.to_string(),
            "--channel-hz",
            &EFERGY_CHANNEL_HZ.to_string(),
            "--output",
            "efergy.json",
        ])
        .output()
        .unwrap();
    assert!(
        scalar.status.success(),
        "{}",
        String::from_utf8_lossy(&scalar.stderr)
    );
    let scalar: Value =
        serde_json::from_slice(&fs::read(root.join("efergy.json")).unwrap()).unwrap();
    assert_eq!(scalar["analyzed_iq_sha256"], digest);
    assert_eq!(scalar["validated_frames"], 1, "{scalar}");
    assert_eq!(scalar["frames"][0]["id"], 0x1234);
    assert_eq!(scalar["frames"][0]["interval_seconds"], 6);

    let profile_sha = write_json(
        root,
        PROFILE_FILE,
        json!({
            "schema_version": 1,
            "efergy_channel_hz": EFERGY_CHANNEL_HZ,
            "ook_channel_hz": CHANNEL_HZ,
        }),
    );
    let output = replay_sweep(
        root,
        samples,
        &[
            "--signal-profile",
            PROFILE_FILE,
            "--share-iq",
            "--output",
            "mixed.json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined: Value =
        serde_json::from_slice(&fs::read(root.join("mixed.json")).unwrap()).unwrap();
    assert_eq!(combined["signal_profile_sha256"], profile_sha);
    assert_eq!(combined["efergy"]["dwells"][0]["replay"], scalar);
    let ook = &combined["ook"]["dwells"][0]["replay"];
    assert_eq!(ook["analyzed_iq_sha256"], digest);
    assert_eq!(ook["observed_transmissions"], 1);
    assert_eq!(ook["transmissions"][0]["model"], "acurite_iris");
    assert_eq!(ook["transmissions"][0]["id"], 0x4a3);
    assert_eq!(
        combined["shared_iq"],
        json!({
            "source_iq_bytes": iq_bytes,
            "max_buffered_iq_bytes": iq_bytes,
            "shared_dwells": 1,
        })
    );

    // Fifteen ms is admissible for Efergy and shorter than the OOK floor.
    // An out-of-range first sample exposes accidental decoding before refusal.
    let cap = 15 * u64::from(SAMPLE_RATE_HZ / 1_000);
    File::options()
        .write(true)
        .open(root.join(IQ_FILE))
        .unwrap()
        .write_all(&i16::MAX.to_le_bytes())
        .unwrap();
    let rejected = replay_sweep(
        root,
        cap - 1,
        &[
            "--signal-profile",
            PROFILE_FILE,
            "--share-iq",
            "--max-samples-per-dwell",
            &cap.to_string(),
            "--output",
            "rejected.json",
        ],
    );
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains(&format!("Efergy replay needs {cap} samples"))
            && stderr.contains("--max-total-samples"),
        "{stderr}"
    );
    assert!(!root.join("rejected.json").exists());
}

#[test]
fn recognizes_tower_and_both_atlas_lengths_in_one_ook_burst() {
    // Channel A. The Tower ID exercises the upper bits omitted by Atlas.
    const TOWER: [u8; 7] = [0xd4, 0xa3, 0x44, 0x2d, 0x09, 0xd2, 0xc3];
    const ATLAS: [u8; 8] = [0xc1, 0xa3, 0xc5, 0x05, 0xc6, 0x11, 0x2d, 0x32];
    const ATLAS_REPEAT: [u8; 8] = [0xc9, 0xa3, 0xc5, 0x05, 0xc6, 0x11, 0x2d, 0x3a];
    const ATLAS_LIGHTNING: [u8; 10] = [0xc5, 0xa3, 0x65, 0x05, 0xc6, 0x11, 0x2d, 0x03, 0x11, 0xea];
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("tune-0001")).unwrap();
    let mut fixture = IqFixture::new(&root.join(IQ_FILE));
    fixture.level(2_000, false);
    let mut corrupt_tower = TOWER;
    corrupt_tower[1] ^= 1;
    fixture.acurite(&corrupt_tower);
    fixture.acurite(&TOWER);
    // The gap exceeds the envelope burst boundary but fits a Tower repeat.
    fixture.level(1_600, false);
    fixture.acurite(&TOWER);
    fixture.acurite(&ATLAS);
    fixture.acurite(&ATLAS_REPEAT);
    fixture.acurite(&ATLAS_LIGHTNING);
    fixture.level(3_000, false);
    let digest = fixture.finish();

    let report = identify(root, CHANNEL_HZ, 3, "acurite.json");
    assert_eq!(report["analyzed_iq_sha256"], digest);
    assert_eq!(report["validated_frame_copies"], 5, "{report}");
    assert_eq!(report["observed_transmissions"], 3);
    assert_eq!(report["omitted_transmissions"], 0);
    let observations = report["transmissions"].as_array().unwrap();
    let identities: Vec<_> = observations
        .iter()
        .map(|row| {
            assert_eq!(row["channel"], 0);
            assert_eq!(row["identifier_scope"], "protocol_sensor_id");
            json!([
                row["model"],
                row["id"],
                row["message_type"],
                row["valid_copies"],
                row["copy_sequence_mask"],
            ])
        })
        .collect();
    assert_eq!(
        identities,
        [
            json!(["acurite_tower", 0x14a3, null, 2, null]),
            json!(["acurite_atlas", 0x1a3, 0x05, 2, 0b101]),
            json!(["acurite_atlas", 0x1a3, 0x25, 1, 0b010]),
        ]
    );
    assert!(observations[0].get("copy_sequence_mask").is_none());
}

#[test]
fn one_wideband_input_separates_devices_and_groups_only_burst_repeats() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir(root.join("tune-0001")).unwrap();
    let mut fixture = IqFixture::new(&root.join(IQ_FILE));
    let mut corrupt = ACURITE_FIRST;
    corrupt[7] ^= 1;
    fixture.level(2_000, false);
    fixture.acurite(&corrupt);
    fixture.acurite(&ACURITE_FIRST);
    fixture.acurite(&ACURITE_REPEAT);
    fixture.level(8_000, false);
    fixture.frame(&LACROSSE, (833, 833), [208, 417], 625);
    fixture.level(8_000, false);
    fixture.acurite(&ACURITE_FIRST);
    fixture.level(3_000, false);
    let digest = fixture.finish();

    let report = identify(root, CHANNEL_HZ, 3, "all.json");
    assert_eq!(report["analyzed_iq_sha256"], digest);
    assert_eq!(report["validated_frame_copies"], 4, "{report}");
    assert_eq!(report["observed_transmissions"], 3);
    assert_eq!(report["omitted_transmissions"], 0);
    let source_samples = fs::metadata(root.join(IQ_FILE)).unwrap().len() / COMPLEX_BYTES as u64;
    assert_eq!(report["source_samples"], source_samples);
    assert_eq!(report["analyzed_samples"], source_samples);
    let transmissions = report["transmissions"].as_array().unwrap();
    assert_eq!(transmissions.len(), 3);
    let identity: Vec<_> = transmissions
        .iter()
        .map(|item| {
            json!([
                item["model"],
                item["id"],
                item["channel"],
                item["valid_copies"]
            ])
        })
        .collect();
    assert_eq!(
        identity,
        [
            json!(["acurite_iris", 0x4a3, 0, 2]),
            json!(["lacrosse_tx141th_bv2", 0x5a, 2, 1]),
            json!(["acurite_iris", 0x4a3, 0, 1]),
        ]
    );
    assert_eq!(transmissions[0]["copy_sequence_mask"], 0b11);
    assert_eq!(transmissions[0]["identifier_scope"], "protocol_sensor_id");
    assert!(transmissions[1].get("copy_sequence_mask").is_none());
    assert_eq!(transmissions[1]["identifier_scope"], "power_cycle_session");
    assert_eq!(transmissions[2]["copy_sequence_mask"], 0b01);
    let mut previous_end = 0;
    for transmission in transmissions {
        let first = transmission["first_input_sample"].as_u64().unwrap();
        let end = transmission["end_input_sample_exclusive"].as_u64().unwrap();
        assert!(first > previous_end && end > first && end <= source_samples);
        previous_end = end;
    }

    let capped = identify(root, CHANNEL_HZ, 1, "capped.json");
    assert_eq!(capped["analyzed_iq_sha256"], digest);
    assert_eq!(capped["validated_frame_copies"], 4);
    assert_eq!(capped["observed_transmissions"], 3);
    assert_eq!(capped["omitted_transmissions"], 2);
    assert_eq!(capped["transmissions"], json!(&transmissions[..1]));

    let wrong = identify(root, CHANNEL_HZ + 1_200_000, 3, "wrong-channel.json");
    assert_eq!(wrong["analyzed_iq_sha256"], digest);
    assert_eq!(wrong["validated_frame_copies"], 0);
    assert_eq!(wrong["observed_transmissions"], 0);
    assert_eq!(wrong["omitted_transmissions"], 0);
    assert_eq!(wrong["transmissions"], json!([]));

    let (capture_digest, sidecar_digest) = write_targeted_receipts(root, source_samples);
    let result = identify_sweep(root, source_samples, "replay.json");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let replay: Value =
        serde_json::from_slice(&fs::read(root.join("replay.json")).unwrap()).unwrap();
    assert_eq!(replay["source_capture_sha256"], capture_digest);
    assert_eq!(replay["dwell_count"], 1);
    assert_eq!(replay["covered_dwells"], 1);
    assert_eq!(replay["replayed_dwells"], 1);
    assert_eq!(replay["replayed_iq_samples"], source_samples);
    let dwells = replay["dwells"].as_array().unwrap();
    assert_eq!(dwells.len(), 1);
    assert_eq!(dwells[0]["status"], "replayed");
    assert_eq!(dwells[0]["source_sidecar_sha256"], sidecar_digest);
    assert_eq!(dwells[0]["replay"], report);
    for counter in [
        "validated_frame_copies",
        "observed_transmissions",
        "omitted_transmissions",
    ] {
        assert_eq!(replay[counter], report[counter]);
    }

    let budget = identify_sweep(root, source_samples - 1, "over-budget.json");
    assert!(!budget.status.success());
    assert!(String::from_utf8_lossy(&budget.stderr).contains("--max-total-samples"));
    assert!(!root.join("over-budget.json").exists());

    verify_profile_replay(root, source_samples, &replay);
}
