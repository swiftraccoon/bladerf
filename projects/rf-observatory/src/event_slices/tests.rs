use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;

const CENTER: u64 = 2_437_000_000;
const WINDOW_SAMPLES: usize = 8192;

fn fixture(
    root: &Path,
    active_windows: &[usize],
    include_active_window_ranges: bool,
) -> SliceEvents {
    let input = root.join("capture.sc16q11");
    let events = root.join("events.json");
    let samples = WINDOW_SAMPLES * 8;
    let mut raw = Vec::with_capacity(samples * 4);
    for index in 0..samples {
        let (i, q) = if active_windows.contains(&(index / WINDOW_SAMPLES)) {
            let phase = std::f64::consts::TAU * 900.0 * index as f64 / WINDOW_SAMPLES as f64;
            (
                (phase.cos() * 600.0).round() as i16,
                (phase.sin() * 600.0).round() as i16,
            )
        } else {
            (1, 0)
        };
        raw.extend_from_slice(&i.to_le_bytes());
        raw.extend_from_slice(&q.to_le_bytes());
    }
    fs::write(&input, raw).unwrap();
    crate::json_file(
        &root.join("capture.sc16q11.json"),
        &json!({
            "format":"SC16_Q11","dtype":"<i2","interleave":"I,Q","complex_scale":2048,
            "frequency_hz":CENTER as f64,"sample_rate_hz":20_000_000.0,"bandwidth_hz":20_000_000.0,
            "num_samples":samples,"device_serial":"private-value",
        }),
    )
    .unwrap();
    let (_, report) = crate::spectrum::survey_with_events(
        &input,
        CENTER,
        samples as u64,
        crate::spectrum::EventBand::Inner,
        crate::spectrum::EventStride::Contiguous,
        include_active_window_ranges,
        &AtomicBool::new(false),
    )
    .unwrap();
    crate::json_file(&events, &report).unwrap();
    SliceEvents {
        input,
        events,
        output: root.join("slices"),
        pre_roll_samples: 0,
        post_roll_samples: 0,
        merge_gap_samples: 0,
        max_slices: 1,
        coalesce_to_limit: false,
        max_total_samples: 8400,
    }
}

#[test]
fn exports_exact_aligned_bytes_and_private_replay_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let args = fixture(directory.path(), &[2], false);
    let stop = AtomicBool::new(false);
    export(directory.path(), &args, &stop).unwrap();
    let manifest_path = args.output.join("slices.json");
    let manifest_bytes = fs::read(&manifest_path).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(
        manifest["limits"],
        json!({
            "pre_roll_samples": 0,
            "post_roll_samples": 0,
            "max_slices": 1,
            "max_total_samples": 8400,
        })
    );
    assert_eq!(manifest["slices"].as_array().unwrap().len(), 1);
    let slice = &manifest["slices"][0];
    assert_eq!(slice["start_sample"], 16380);
    assert_eq!(slice["end_sample_exclusive"], 24580);
    assert_eq!(manifest["exported_samples"], 8200);
    let name = slice["iq_file"].as_str().unwrap();
    let raw = fs::read(&args.input).unwrap();
    let output = fs::read(args.output.join(name)).unwrap();
    assert_eq!(output, raw[16380 * 4..24580 * 4]);
    assert_eq!(slice["iq_sha256"], format!("{:x}", Sha256::digest(&output)));
    let sidecar_text = fs::read_to_string(args.output.join(format!("{name}.json"))).unwrap();
    assert!(!sidecar_text.contains("private-value"));
    let sidecar: Value = serde_json::from_str(&sidecar_text).unwrap();
    assert_eq!(sidecar["frequency_hz"], CENTER);
    assert_eq!(sidecar["num_samples"], 8200);
    assert_eq!(sidecar["source_start_sample"], 16380);
    assert_eq!(sidecar["source_kind"], "derived_event_slice");
    assert_eq!(
        fs::metadata(&args.output).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&manifest_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(export(directory.path(), &args, &stop).is_err());
    assert_eq!(fs::read(manifest_path).unwrap(), manifest_bytes);
}

#[test]
fn copies_saved_samples_across_a_merged_gap() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = fixture(directory.path(), &[2, 4], false);
    args.merge_gap_samples = 8180;
    args.max_total_samples = 24580;
    export(directory.path(), &args, &AtomicBool::new(false)).unwrap();

    let manifest: Value =
        serde_json::from_slice(&fs::read(args.output.join("slices.json")).unwrap()).unwrap();
    assert_eq!(manifest["limits"]["merge_gap_samples"], 8180);
    assert_eq!(manifest["slices"].as_array().unwrap().len(), 1);
    let slice = &manifest["slices"][0];
    assert_eq!(slice["start_sample"], 16380);
    assert_eq!(slice["end_sample_exclusive"], 40960);
    assert_eq!(manifest["exported_samples"], 24580);
    let name = slice["iq_file"].as_str().unwrap();
    let raw = fs::read(&args.input).unwrap();
    let output = fs::read(args.output.join(name)).unwrap();
    assert_eq!(output, raw[16380 * 4..40960 * 4]);
}

#[test]
fn coalesces_only_when_requested_and_preserves_the_manifest_contract() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = fixture(directory.path(), &[2, 4], true);
    args.max_total_samples = 24580;
    let stop = AtomicBool::new(false);
    let error = export(directory.path(), &args, &stop).unwrap_err();
    assert!(error.to_string().contains("slice count exceeds limit"));
    assert!(!args.output.exists());

    args.coalesce_to_limit = true;
    export(directory.path(), &args, &stop).unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(args.output.join("slices.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 3);
    assert_eq!(
        manifest["limits"],
        json!({
            "pre_roll_samples": 0,
            "post_roll_samples": 0,
            "max_slices": 1,
            "max_total_samples": 24580,
        })
    );
    assert_eq!(manifest["slices"].as_array().unwrap().len(), 1);
    assert_eq!(manifest["exported_samples"], 24580);
}

#[test]
fn exports_active_windows_outside_retained_components() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = fixture(directory.path(), &[2, 4], true);
    let mut report: Value = serde_json::from_slice(&fs::read(&args.events).unwrap()).unwrap();
    assert_eq!(report["active_window_ranges"], json!([[2, 3], [4, 5]]));
    let retained = &report["events"][0];
    assert_eq!(retained["end_sample_exclusive"], 3 * WINDOW_SAMPLES);
    let omitted_hits =
        report["hit_tile_count"].as_u64().unwrap() - retained["hit_tile_count"].as_u64().unwrap();
    report["events"].as_array_mut().unwrap().truncate(1);
    report["event_limit"] = json!(1);
    report["omitted_component_count"] = json!(report["component_count"].as_u64().unwrap() - 1);
    report["omitted_hit_tile_count"] = json!(omitted_hits);
    fs::write(&args.events, serde_json::to_vec(&report).unwrap()).unwrap();

    args.max_slices = 2;
    args.max_total_samples = 20_000;
    export(directory.path(), &args, &AtomicBool::new(false)).unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(args.output.join("slices.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 3);
    assert_eq!(manifest["event_window_stride_samples"], WINDOW_SAMPLES);
    assert_eq!(manifest["event_count"], 1);
    assert_eq!(manifest["active_window_count"], 2);
    assert_eq!(manifest["active_window_range_count"], 2);
    let slices = manifest["slices"].as_array().unwrap();
    assert_eq!(slices.len(), 2);
    let second = &slices[1];
    assert_eq!(second["start_sample"], 32760);
    assert_eq!(second["end_sample_exclusive"], 40960);
    let raw = fs::read(&args.input).unwrap();
    let slice_path = args.output.join(second["iq_file"].as_str().unwrap());
    assert_eq!(fs::read(slice_path).unwrap(), raw[32760 * 4..40960 * 4]);
}

#[test]
fn a_changed_byte_in_a_skipped_range_prevents_publication() {
    let directory = tempfile::tempdir().unwrap();
    let args = fixture(directory.path(), &[2], false);
    let mut raw = fs::read(&args.input).unwrap();
    raw[0] ^= 1;
    fs::write(&args.input, raw).unwrap();
    let error = export(directory.path(), &args, &AtomicBool::new(false)).unwrap_err();
    assert!(error.to_string().contains("SHA-256 differs"));
    assert!(!args.output.exists());
    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".event-slices-")
    }));
}
