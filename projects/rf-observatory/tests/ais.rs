//! Exercise simultaneous AIS channels through the saved-IQ and profile CLI.

use std::f64::consts::TAU;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use data_encoding::HEXLOWER;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CENTER_HZ: u64 = 157_950_000;
const CHANNELS_HZ: [u64; 2] = [161_975_000, 162_025_000];
const RATE_HZ: usize = 20_000_000;
const SYMBOL_RATE: usize = 9_600;
const SHAPING_RATE_HZ: usize = 96_000;
const SAMPLES_PER_SYMBOL: usize = SHAPING_RATE_HZ / SYMBOL_RATE;
const DWELL_MS: usize = 75;
const SAMPLES: usize = DWELL_MS * (RATE_HZ / 1_000);
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";
const PROFILE: &str = "ais-profile.json";

// Fictitious source IDs with independently calculated CRC-16/X25 octets.
const BASE_STATION: &str = "1202f185380000000000000000000000000000000085d3";
const CLASS_B: &str = "491d6f345400000000000000000000000004000000f815";

fn lsb_bits(byte: u8) -> impl Iterator<Item = bool> {
    (0..8).map(move |bit| byte & (1 << bit) != 0)
}

fn shaped_phase(frame: &[u8]) -> Vec<f64> {
    let mut wire: Vec<_> = (0..24).map(|bit| bit % 2 == 1).collect();
    wire.extend(lsb_bits(0x7e));
    let mut ones = 0;
    for bit in frame.iter().copied().flat_map(lsb_bits) {
        wire.push(bit);
        ones = if bit { ones + 1 } else { 0 };
        if ones == 5 {
            wire.push(false);
            ones = 0;
        }
    }
    wire.extend(lsb_bits(0x7e));

    let mut tone = 1.0;
    let levels: Vec<_> = wire
        .into_iter()
        .flat_map(|bit| {
            if !bit {
                tone = -tone;
            }
            std::iter::repeat_n(tone, SAMPLES_PER_SYMBOL)
        })
        .collect();
    // Sample the Gaussian impulse response with a 0.4-symbol BT product,
    // then integrate the shaped frequency at the 2,400 Hz MSK deviation.
    let half = 2 * SAMPLES_PER_SYMBOL;
    let sigma = 2.0_f64.ln().sqrt() * SAMPLES_PER_SYMBOL as f64 / (TAU * 0.4);
    let response: Vec<_> = (0..=2 * half)
        .map(|tap| {
            let distance = (tap as f64 - half as f64) / sigma;
            (-0.5 * distance * distance).exp()
        })
        .collect();
    let weight = response.iter().sum::<f64>();
    let step = TAU * 2_400.0 / SHAPING_RATE_HZ as f64 / weight;
    let mut phase = Vec::with_capacity(levels.len() + 1);
    phase.push(0.0);
    for index in 0..levels.len() {
        let frequency: f64 = response
            .iter()
            .enumerate()
            .map(|(tap, scale)| {
                let input = (index + tap).saturating_sub(half).min(levels.len() - 1);
                levels[input] * scale
            })
            .sum();
        phase.push(phase[index] + step * frequency);
    }
    phase
}

fn add_burst(samples: &mut [[i16; 2]], first_ms: usize, carrier_hz: u64, frame: &[u8]) {
    let phase = shaped_phase(frame);
    let carrier_step = TAU * (carrier_hz as f64 - CENTER_HZ as f64) / RATE_HZ as f64;
    let first = first_ms * (RATE_HZ / 1_000);
    for (index, sample) in samples[first..].iter_mut().enumerate() {
        let time = index as f64 * SHAPING_RATE_HZ as f64 / RATE_HZ as f64;
        let base = time as usize;
        let Some(next) = phase.get(base + 1) else {
            break;
        };
        let shaped = phase[base] + (next - phase[base]) * time.fract();
        let (sin, cos) = (shaped + carrier_step * index as f64).sin_cos();
        sample[0] += (720.0 * cos).round() as i16;
        sample[1] += (720.0 * sin).round() as i16;
    }
}

fn iq_fixture() -> Vec<u8> {
    let mut samples = vec![[0; 2]; SAMPLES];
    let mut first = HEXLOWER.decode(BASE_STATION.as_bytes()).unwrap();
    let second = HEXLOWER.decode(CLASS_B.as_bytes()).unwrap();
    add_burst(&mut samples, 4, CHANNELS_HZ[0] + 700, &first);
    add_burst(&mut samples, 9, CHANNELS_HZ[1] - 1_250, &second);
    first[10] ^= 0x01;
    add_burst(&mut samples, 43, CHANNELS_HZ[0], &first);
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

fn write_sweep(root: &Path, iq: &[u8]) -> String {
    fs::create_dir(root.join("tune-0001")).unwrap();
    fs::write(root.join(IQ_FILE), iq).unwrap();
    write_json(
        root,
        "tune-0001/wideband.sc16q11.json",
        json!({
            "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q", "complex_scale": 2048,
            "frequency_hz": CENTER_HZ, "sample_rate_hz": RATE_HZ, "bandwidth_hz": RATE_HZ,
            "gain_db": 40, "gain_mode": "manual", "bias_tee_enabled": false, "channel": "RX1",
            "num_samples": SAMPLES, "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    write_json(
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
    )
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn report(root: &Path, options: &[&str], name: &str) -> Value {
    let mut args = options.to_vec();
    args.extend(["--output", name]);
    let result = run(root, &args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&fs::read(root.join(name)).unwrap()).unwrap()
}

#[test]
fn simultaneous_ais_channels_share_a_bound_wideband_dwell() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let iq = iq_fixture();
    let iq_digest = format!("{:x}", Sha256::digest(&iq));
    let sweep_digest = write_sweep(root, &iq);
    let profile_digest = write_json(
        root,
        PROFILE,
        json!({"schema_version": 1, "ais_channel_hz": CHANNELS_HZ}),
    );

    let [one, two] = CHANNELS_HZ.map(|hz| hz.to_string());
    let limit = (2 * SAMPLES).to_string();
    let dedicated = report(
        root,
        &[
            "identify-ais-sweep",
            ".",
            "--channel-hz",
            &one,
            "--channel-hz",
            &two,
            "--max-total-samples",
            &limit,
        ],
        "dedicated.json",
    );
    assert_eq!(dedicated["source_capture_sha256"], sweep_digest);
    assert_eq!(dedicated["validated_frames"], 2, "{dedicated}");
    assert_eq!(dedicated["decoded_samples"], 2 * SAMPLES);

    for (index, (hz, message_type, mmsi)) in [(&one, 4, "012345678"), (&two, 18, "123456789")]
        .into_iter()
        .enumerate()
    {
        let scalar = report(
            root,
            &[
                "identify-ais",
                IQ_FILE,
                "--center-hz",
                &CENTER_HZ.to_string(),
                "--channel-hz",
                hz,
            ],
            &format!("scalar-{hz}.json"),
        );
        assert_eq!(
            dedicated["channels"][index]["attempts"][0]["replay"],
            scalar
        );
        assert_eq!(scalar["analyzed_iq_sha256"], iq_digest);
        assert_eq!(scalar["validated_frames"], 1, "{scalar}");
        assert_eq!(scalar["frames"][0]["message_type"], message_type);
        assert_eq!(scalar["frames"][0]["mmsi"], mmsi);
    }
    let first = &dedicated["channels"][0]["attempts"][0]["replay"]["frames"][0];
    let second = &dedicated["channels"][1]["attempts"][0]["replay"]["frames"][0];
    assert!(
        first["end_input_sample_exclusive"].as_u64().unwrap()
            > second["first_input_sample"].as_u64().unwrap()
    );

    let replay = [
        "replay-sweep",
        ".",
        "--signal-profile",
        PROFILE,
        "--share-iq",
    ];
    let mut args = replay.to_vec();
    args.extend(["--max-total-samples", &limit]);
    let profile = report(root, &args, "profile.json");
    assert_eq!(profile["signal_profile_sha256"], profile_digest);
    assert_eq!(profile["ais"], dedicated);
    assert_eq!(
        profile["shared_iq"],
        json!({
            "source_iq_bytes": iq.len(), "max_buffered_iq_bytes": iq.len(), "shared_dwells": 1,
        })
    );

    // An invalid IQ word distinguishes a preflight refusal from sample decoding.
    fs::OpenOptions::new()
        .write(true)
        .open(root.join(IQ_FILE))
        .unwrap()
        .write_all(&i16::MAX.to_le_bytes())
        .unwrap();
    let mut args = replay.to_vec();
    let short = (2 * SAMPLES - 1).to_string();
    args.extend(["--max-total-samples", &short, "--plan-only"]);
    let refusal = run(root, &args);
    assert!(!refusal.status.success());
    let error = String::from_utf8_lossy(&refusal.stderr);
    assert!(
        error.contains("AIS replay needs") && error.contains("--max-total-samples"),
        "{error}"
    );
    assert!(refusal.stdout.is_empty());
}
