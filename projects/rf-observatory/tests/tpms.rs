//! Exercise TPMS replay with controlled, frequency-offset FSK bursts.

use std::f64::consts::TAU;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const SAMPLE_RATE_HZ: u32 = 20_000_000;
const SAMPLES_PER_US: usize = 20;
const CENTER_HZ: u64 = 313_000_000;
const CHANNEL_HZ: u64 = 315_000_000;
const CAPTURE_US: usize = 72_000;
const IQ_BYTES_PER_SAMPLE: usize = 4;
const IQ_FILE: &str = "tune-0001/wideband.sc16q11";
const START_US: [usize; 4] = [2_000, 15_000, 28_000, 52_000];

// Independently calculated CRCs over synthetic sensor fields. The 107J
// vector includes six leading alignment bits that are omitted on air.
const C210_PAYLOAD: [u8; 9] = [0x12, 0x34, 0xab, 0xcd, 0xce, 0xa1, 0x85, 0x62, 0x72];
const PMV107J_PAYLOAD: [u8; 9] = [0x02, 0xaf, 0x37, 0xbc, 0x73, 0x84, 0x7b, 0x0c, 0x65];

#[derive(Clone, Copy)]
struct Waveform {
    chip_us: usize,
    prefix: &'static [u8],
    padding_bits: usize,
    trailer: &'static [u8],
    tone_offsets_hz: [f64; 2],
}

const C210: Waveform = Waveform {
    chip_us: 50,
    prefix: b"01010101001111",
    padding_bits: 0,
    trailer: b"111",
    tone_offsets_hz: [8_000.0, 48_000.0],
};
const PMV107J: Waveform = Waveform {
    chip_us: 101,
    prefix: b"00011111100",
    padding_bits: 6,
    trailer: b"100",
    tone_offsets_hz: [-72_000.0, 8_000.0],
};

fn append_burst(iq: &mut Vec<u8>, start_us: usize, waveform: Waveform, payload: [u8; 9]) {
    iq.resize(start_us * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE, 0);
    let nominal_offset_hz = (CHANNEL_HZ - CENTER_HZ) as f64;
    let steps = waveform
        .tone_offsets_hz
        .map(|offset| TAU * (nominal_offset_hz + offset) / f64::from(SAMPLE_RATE_HZ));
    let samples_per_chip = waveform.chip_us * SAMPLES_PER_US;
    let mut phase: f64 = 0.0;
    let mut emit_chip = |high: bool| {
        for _ in 0..samples_per_chip {
            let (q, i) = phase.sin_cos();
            iq.extend_from_slice(&((875.0 * i).round() as i16).to_le_bytes());
            iq.extend_from_slice(&((875.0 * q).round() as i16).to_le_bytes());
            phase = (phase + steps[usize::from(high)]).rem_euclid(TAU);
        }
    };
    let mut last = false;
    for &chip in waveform.prefix {
        last = chip == b'1';
        emit_chip(last);
    }
    for index in waveform.padding_bits..payload.len() * 8 {
        let bit = payload[index / 8] & (0x80 >> (index % 8)) != 0;
        emit_chip(!last);
        last ^= bit;
        emit_chip(last);
    }
    for &chip in waveform.trailer {
        emit_chip(chip == b'1');
    }
}

fn synthetic_iq() -> Vec<u8> {
    let bytes = CAPTURE_US * SAMPLES_PER_US * IQ_BYTES_PER_SAMPLE;
    let mut iq = Vec::with_capacity(bytes);
    let mut bad_crc = C210_PAYLOAD;
    bad_crc[8] ^= 1;
    for (start_us, (waveform, payload)) in START_US.into_iter().zip([
        (C210, C210_PAYLOAD),
        (C210, bad_crc),
        (PMV107J, PMV107J_PAYLOAD),
        (PMV107J, PMV107J_PAYLOAD),
    ]) {
        append_burst(&mut iq, start_us, waveform, payload);
    }
    iq.resize(bytes, 0);
    iq
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(root)
        .args(args)
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

fn replay(root: &Path, channel_hz: u64, output: &str, extra: &[&str]) -> Value {
    let center_hz = CENTER_HZ.to_string();
    let channel_hz = channel_hz.to_string();
    let rate = SAMPLE_RATE_HZ.to_string();
    let mut args = vec![
        "identify-tpms",
        IQ_FILE,
        "--center-hz",
        &center_hz,
        "--channel-hz",
        &channel_hz,
        "--sample-rate-hz",
        &rate,
        "--output",
        output,
    ];
    args.extend_from_slice(extra);
    read_report(root, output, run(root, &args))
}

fn check_frame(frame: &Value, waveform: Waveform, start_us: usize, mut expected: Value) {
    let first = frame["first_input_sample"].as_u64().unwrap() as usize;
    let end = frame["end_input_sample_exclusive"].as_u64().unwrap() as usize;
    let start = start_us * SAMPLES_PER_US;
    let chip_samples = waveform.chip_us * SAMPLES_PER_US;
    let payload_chips = (C210_PAYLOAD.len() * 8 - waveform.padding_bits) * 2;
    let payload_end = start + (waveform.prefix.len() + payload_chips) * chip_samples;
    assert!(first.abs_diff(start) <= 6 * chip_samples, "{frame}");
    assert!(
        first < end && end.abs_diff(payload_end) <= 2 * chip_samples,
        "{frame}"
    );
    expected["first_input_sample"] = json!(first);
    expected["end_input_sample_exclusive"] = json!(end);
    assert_eq!(*frame, expected);
}

fn write_json(root: &Path, path: &str, value: Value) -> String {
    let bytes = serde_json::to_vec(&value).unwrap();
    fs::write(root.join(path), &bytes).unwrap();
    format!("{:x}", Sha256::digest(bytes))
}

fn check_sweep_replay(root: &Path, scalar: &Value, samples: usize) {
    let iq_bytes = samples * IQ_BYTES_PER_SAMPLE;
    let sidecar_digest = write_json(
        root,
        "tune-0001/wideband.sc16q11.json",
        json!({
            "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q", "complex_scale": 2048,
            "frequency_hz": CENTER_HZ, "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ, "gain_db": 40, "gain_mode": "manual",
            "bias_tee_enabled": false, "channel": "RX1", "num_samples": samples,
            "started_utc": "2000-01-01T00:00:00Z",
        }),
    );
    let sweep_digest = write_json(
        root,
        "sweep.json",
        json!({
            "schema_version": 1, "operation": "targeted_rf_sweep",
            "lower_hz": CENTER_HZ, "upper_hz": CENTER_HZ + 1,
            "antenna": 1, "gain_db": 40, "dwell_ms": CAPTURE_US / 1_000, "iq_bytes": iq_bytes,
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
    let channel_hz = CHANNEL_HZ.to_string();
    let output = "sweep-replay.json";
    let standalone = read_report(
        root,
        output,
        run(
            root,
            &[
                "identify-tpms-sweep",
                ".",
                "--channel-hz",
                &channel_hz,
                "--output",
                output,
            ],
        ),
    );
    assert_eq!(standalone["source_capture_sha256"], sweep_digest);
    assert_eq!(standalone["validated_frames"], 3);
    assert_eq!(standalone["replayed_dwells"], 1);
    let dwell = &standalone["dwells"][0];
    assert_eq!(dwell["source_sweep_sha256"], sweep_digest);
    assert_eq!(dwell["source_sidecar_sha256"], sidecar_digest);
    assert_eq!(dwell["replay"], *scalar);

    let output = "combined.json";
    let combined = read_report(
        root,
        output,
        run(
            root,
            &[
                "replay-sweep",
                ".",
                "--tpms-channel-hz",
                &channel_hz,
                "--ook-channel-hz",
                &channel_hz,
                "--share-iq",
                "--output",
                output,
            ],
        ),
    );
    assert_eq!(combined["tpms"], standalone);
    assert_eq!(combined["ook"]["replayed_dwells"], 1);
    assert_eq!(combined["shared_iq"]["source_iq_bytes"], iq_bytes);
    assert_eq!(combined["shared_iq"]["shared_dwells"], 1);
}

#[test]
fn saved_iq_replay_accepts_both_formats_and_counts_separate_occurrences() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let iq = synthetic_iq();
    fs::create_dir(root.join("tune-0001")).unwrap();
    fs::write(root.join(IQ_FILE), &iq).unwrap();
    let samples = iq.len() / IQ_BYTES_PER_SAMPLE;
    let digest = format!("{:x}", Sha256::digest(&iq));

    let report = replay(root, CHANNEL_HZ, "all.json", &[]);
    assert_eq!(report["analyzed_iq_sha256"], digest);
    assert_eq!(report["analyzed_samples"], samples);
    assert_eq!(report["source_samples"], samples);
    assert_eq!(report["validated_frames"], 3, "{report}");
    assert_eq!(report["omitted_valid_frames"], 0);
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3);
    check_frame(
        &frames[0],
        C210,
        START_US[0],
        json!({
            "model": "pmv_c210", "id": 0x1234_abcd, "status": 0x85,
            "pressure_quarter_psi": 129, "temperature_c": 27,
        }),
    );
    for (frame, start_us) in frames[1..].iter().zip(START_US[2..].iter().copied()) {
        check_frame(
            frame,
            PMV107J,
            start_us,
            json!({
                "model": "pmv_107j", "id": 0x0abc_def1, "status": 0x33,
                "battery_ok": false, "repeat_counter": 2, "rapid_change": true, "failed": true,
                "pressure_pa": 228_160, "temperature_c": -28,
            }),
        );
    }

    let capped = replay(root, CHANNEL_HZ, "capped.json", &["--max-frames", "1"]);
    assert_eq!(capped["analyzed_iq_sha256"], digest);
    assert_eq!(capped["validated_frames"], 3);
    assert_eq!(capped["omitted_valid_frames"], 2);
    assert_eq!(
        capped["frames"].as_array().unwrap().as_slice(),
        &frames[..1]
    );

    let elsewhere = replay(root, CHANNEL_HZ + 700_000, "wrong-channel.json", &[]);
    assert_eq!(elsewhere["validated_frames"], 0);
    check_sweep_replay(root, &report, samples);
}
