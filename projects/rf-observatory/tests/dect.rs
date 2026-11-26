//! Verify DECT fixed-part identification through the saved-IQ CLI.

use std::f64::consts::TAU;
use std::fs;
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

#[test]
fn saved_iq_identifies_an_inverted_fixed_part_broadcast() {
    const RATE_HZ: u32 = 4_608_000;
    const CHANNEL_HZ: &str = "1924992000";
    const SAMPLES_PER_SYMBOL: usize = 4;
    const GUARD_SAMPLES: usize = 139;
    const OFFSET_HZ: i64 = 51_000;
    // Conventional S and A fields with the ETSI R-CRC final-bit inversion.
    // https://www.etsi.org/deliver/etsi_en/300100_300199/30017503/02.09.01_60/en_30017503v020901p.pdf#page=104
    const PREFIX: [u8; 12] = [
        0xaa, 0xaa, 0xe9, 0x8a, 0x6e, 0x00, 0x4e, 0xb4, 0xbd, 0x50, 0x8a, 0x9e,
    ];
    let mut iq = vec![0; 4 * GUARD_SAMPLES];
    let mut phase = 0.417_f64;
    for byte in PREFIX {
        for shift in (0..8).rev() {
            // Rectangular frequency pulses exercise the IQ input and polarity
            // independently of the decoder's Gaussian waveform model.
            let deviation = if byte & (1 << shift) == 0 {
                288_000.0
            } else {
                -288_000.0
            };
            let step = TAU * (OFFSET_HZ as f64 + deviation) / f64::from(RATE_HZ);
            for _ in 0..SAMPLES_PER_SYMBOL {
                let (sin, cos) = phase.sin_cos();
                iq.extend_from_slice(&((1_200.0 * cos).round() as i16).to_le_bytes());
                iq.extend_from_slice(&((1_200.0 * sin).round() as i16).to_le_bytes());
                phase += step;
            }
        }
    }
    let end_sample = (iq.len() / 4) as u64;
    iq.resize(iq.len() + 4 * GUARD_SAMPLES, 0);

    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("base.sc16q11"), &iq).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rf-observatory"))
        .current_dir(directory.path())
        .args([
            "identify-dect",
            "base.sc16q11",
            "--center-hz",
            CHANNEL_HZ,
            "--channel-hz",
            CHANNEL_HZ,
            "--sample-rate-hz",
            &RATE_HZ.to_string(),
            "--output",
            "report.json",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(directory.path().join("report.json")).unwrap();
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        report["analyzed_iq_sha256"],
        format!("{:x}", Sha256::digest(iq))
    );
    assert_eq!(report["validated_frames"], 1, "{report}");
    let frames = report["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    let frame = &frames[0];
    assert_eq!(frame["rfpi"], "004eb4bd50");
    assert_eq!(frame["a_field_header"], 0x6e);
    assert_eq!(frame["spectrum_inverted"], true);
    let first = frame["first_input_sample"].as_u64().unwrap();
    let end = frame["end_input_sample_exclusive"].as_u64().unwrap();
    let offset = frame["estimated_carrier_offset_hz"].as_i64().unwrap();
    assert!(first.abs_diff(GUARD_SAMPLES as u64) <= 4, "{frame}");
    assert!(end.abs_diff(end_sample) <= 4, "{frame}");
    assert!(offset.abs_diff(OFFSET_HZ) <= 8_000, "{frame}");
}
