use std::fs::FileTimes;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

const ANALYZED_SAMPLES: u64 = 3 * WINDOW_SAMPLES;
const INPUT_SAMPLES: u64 = ANALYZED_SAMPLES + 7;
const CENTER_HZ: u64 = 2_437_000_000;
const FILENAME: &str = "slice-0001.sc16q11";
const START: u64 = 8190;
const END: u64 = 16390;

struct Fixture {
    _temp: TempDir,
    input: PathBuf,
    root: PathBuf,
    iq: Vec<u8>,
    manifest: Value,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

impl Fixture {
    fn new() -> Self {
        Self::with_slice(START, END)
    }

    fn with_slice(start: u64, end: u64) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("source.sc16q11");
        let acquisition = temp.path().join("source.sc16q11.json");
        let root = temp.path().join("export");
        fs::create_dir(&root).unwrap();
        let iq: Vec<_> = (0..INPUT_SAMPLES)
            .flat_map(|index| {
                let value = (index % 2039) as i16;
                [value.to_le_bytes(), (-value).to_le_bytes()]
                    .into_iter()
                    .flatten()
            })
            .collect();
        fs::write(&input, &iq).unwrap();
        let metadata = json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": CENTER_HZ,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "num_samples": INPUT_SAMPLES
        });
        write_json(&acquisition, &metadata);
        let slice_bytes = &iq[(start * SAMPLE_BYTES) as usize..(end * SAMPLE_BYTES) as usize];
        fs::write(root.join(FILENAME), slice_bytes).unwrap();
        let mut sidecar = metadata;
        sidecar["num_samples"] = json!(end - start);
        sidecar["source_kind"] = json!("derived_event_slice");
        sidecar["source_start_sample"] = json!(start);
        sidecar["continuity_guaranteed"] = json!(false);
        write_json(&root.join(sidecar_name(FILENAME)), &sidecar);

        let manifest = json!({
            "schema_version": 1,
            "operation": "offline_rf_event_slice_export",
            "source_format": "SC16_Q11_LE",
            "center_frequency_hz": CENTER_HZ,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "input_samples": INPUT_SAMPLES,
            "analyzed_samples": ANALYZED_SAMPLES,
            "analyzed_prefix_sha256": hash(&iq[..(ANALYZED_SAMPLES * SAMPLE_BYTES) as usize]),
            "event_report_sha256": "1".repeat(64),
            "acquisition_sha256": hash(&fs::read(acquisition).unwrap()),
            "event_count": 1,
            "omitted_component_count": 0,
            "alignment_samples": ALIGNMENT_SAMPLES,
            "alignment_omitted_tail_samples": ANALYZED_SAMPLES % ALIGNMENT_SAMPLES,
            "limits": {
                "pre_roll_samples": 0,
                "post_roll_samples": 0,
                "max_slices": MAX_SLICES,
                "max_total_samples": MAX_SAMPLES
            },
            "exported_samples": end - start,
            "slices": [{
                "iq_file": FILENAME,
                "start_sample": start,
                "end_sample_exclusive": end,
                "iq_sha256": hash(slice_bytes)
            }]
        });

        Self {
            _temp: temp,
            root,
            input,
            iq,
            manifest,
        }
    }

    fn add_slice(&mut self, start: u64, end: u64) {
        let slices = self.manifest["slices"].as_array_mut().unwrap();
        let filename = format!("slice-{:04}.sc16q11", slices.len() + 1);
        let bytes = &self.iq[(start * SAMPLE_BYTES) as usize..(end * SAMPLE_BYTES) as usize];
        fs::write(self.root.join(&filename), bytes).unwrap();
        let mut sidecar: Value =
            serde_json::from_slice(&fs::read(self.root.join(sidecar_name(FILENAME))).unwrap())
                .unwrap();
        sidecar["num_samples"] = json!(end - start);
        sidecar["source_start_sample"] = json!(start);
        write_json(&self.root.join(sidecar_name(&filename)), &sidecar);
        slices.push(json!({
            "iq_file": filename,
            "start_sample": start,
            "end_sample_exclusive": end,
            "iq_sha256": hash(bytes)
        }));
        let exported = self.manifest["exported_samples"].as_u64().unwrap();
        self.manifest["exported_samples"] = json!(exported + end - start);
    }

    fn use_active_windows(&mut self, stride: u64, active: u64, ranges: u64) {
        self.manifest["schema_version"] = json!(3);
        self.manifest["event_window_stride_samples"] = json!(stride);
        self.manifest["active_window_count"] = json!(active);
        self.manifest["active_window_range_count"] = json!(ranges);
    }

    fn verify(&self) -> Result<Plan> {
        write_json(&self.root.join(MANIFEST_NAME), &self.manifest);
        verify(&self.input, &self.root, &AtomicBool::new(false))
    }
}

#[test]
fn verifies_an_interior_source_range_and_binds_full_capture_and_manifest() {
    let fixture = Fixture::new();
    let plan = fixture.verify().unwrap();
    assert_eq!(plan.source.sha256, hash(&fixture.iq));
    assert_eq!(plan.source.complex_samples, INPUT_SAMPLES);
    assert_eq!(plan.analyzed_samples, ANALYZED_SAMPLES);
    assert_eq!(plan.exported_samples, END - START);
    assert_eq!(plan.omitted_component_count, 0);
    assert_eq!(plan.slices[0].iq_file, FILENAME);
    assert_eq!(plan.slices[0].samples(), END - START);
    assert_eq!(
        plan.manifest_sha256,
        hash(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap())
    );
}

#[test]
fn rejects_same_content_slice_rewrite_with_restored_mtime() {
    let fixture = Fixture::new();
    let plan = fixture.verify().unwrap();
    let path = fixture.root.join(FILENAME);
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    let before = file.metadata().unwrap();
    let modified = before.modified().unwrap();
    let content = fs::read(&path).unwrap();

    std::thread::sleep(Duration::from_millis(20));
    fs::write(&path, &content).unwrap();
    file.set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    let after = file.metadata().unwrap();
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.modified().unwrap(), modified);
    assert_eq!(fs::read(&path).unwrap(), content);

    let error = plan
        .verify_snapshot(&AtomicBool::new(false))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("changed since verification") && error.contains(FILENAME),
        "{error}"
    );
}

#[test]
fn rejects_source_path_replacement_with_identical_bytes() {
    let fixture = Fixture::new();
    let plan = fixture.verify().unwrap();
    let before = fs::metadata(&fixture.input).unwrap();
    let replacement = fixture.input.with_extension("replacement");
    fs::write(&replacement, &fixture.iq).unwrap();
    File::open(&replacement)
        .unwrap()
        .set_times(FileTimes::new().set_modified(before.modified().unwrap()))
        .unwrap();
    fs::rename(&replacement, &fixture.input).unwrap();
    let after = fs::metadata(&fixture.input).unwrap();
    assert_ne!(after.ino(), before.ino());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert_eq!(hash(&fs::read(&fixture.input).unwrap()), plan.source.sha256);

    let error = plan
        .verify_snapshot(&AtomicBool::new(false))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("changed since verification") && error.contains("source.sc16q11"),
        "{error}"
    );
}

#[test]
fn validates_q11_and_hashes_samples_beyond_the_analyzed_prefix() {
    let mut fixture = Fixture::new();
    let original = fixture.verify().unwrap();
    let tail_start = (ANALYZED_SAMPLES * SAMPLE_BYTES) as usize;

    fixture.iq[tail_start..tail_start + 2].copy_from_slice(&2047i16.to_le_bytes());
    fs::write(&fixture.input, &fixture.iq).unwrap();
    let changed = fixture.verify().unwrap();
    assert_eq!(changed.source.sha256, hash(&fixture.iq));
    assert_ne!(changed.source.sha256, original.source.sha256);
    assert_eq!(changed.manifest_sha256, original.manifest_sha256);

    fixture.iq[tail_start..tail_start + 2].copy_from_slice(&2048i16.to_le_bytes());
    fs::write(&fixture.input, &fixture.iq).unwrap();
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("outside the SC16_Q11 range"), "{error}");
}

#[test]
fn verifies_overlap_extent_and_bounds_components_by_its_exact_window_count() {
    let mut fixture = Fixture::new();
    let analyzed_samples = ANALYZED_SAMPLES - OVERLAP_STRIDE_SAMPLES;
    let prefix = &fixture.iq[..(analyzed_samples * SAMPLE_BYTES) as usize];
    let omitted = 4 * MAX_EVENT_CELLS_PER_WINDOW - 1;
    fixture.manifest["schema_version"] = json!(2);
    fixture.manifest["event_window_stride_samples"] = json!(OVERLAP_STRIDE_SAMPLES);
    fixture.manifest["analyzed_samples"] = json!(analyzed_samples);
    fixture.manifest["analyzed_prefix_sha256"] = json!(hash(prefix));
    fixture.manifest["alignment_omitted_tail_samples"] =
        json!(analyzed_samples % ALIGNMENT_SAMPLES);
    fixture.manifest["omitted_component_count"] = json!(omitted);

    let plan = fixture.verify().unwrap();
    assert_eq!(plan.analyzed_samples, analyzed_samples);
    assert_eq!(plan.omitted_component_count, omitted);

    fixture.manifest["omitted_component_count"] = json!(omitted + 1);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("component accounting"), "{error}");

    for invalid_extent in [WINDOW_SAMPLES - 1, analyzed_samples + 1] {
        fixture.manifest["analyzed_samples"] = json!(invalid_extent);
        fixture.manifest["alignment_omitted_tail_samples"] =
            json!(invalid_extent % ALIGNMENT_SAMPLES);
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("sample extent or alignment"), "{error}");
    }
}

#[test]
fn enforces_stride_presence_and_value_by_schema() {
    for (version, stride) in [
        (1, Some(json!(OVERLAP_STRIDE_SAMPLES))),
        (1, Some(Value::Null)),
        (2, None),
        (2, Some(Value::Null)),
        (2, Some(json!(WINDOW_SAMPLES))),
        (3, None),
        (3, Some(Value::Null)),
        (3, Some(json!(OVERLAP_STRIDE_SAMPLES + 1))),
        (4, Some(json!(OVERLAP_STRIDE_SAMPLES))),
    ] {
        let mut fixture = Fixture::new();
        fixture.manifest["schema_version"] = json!(version);
        if let Some(stride) = stride {
            fixture.manifest["event_window_stride_samples"] = stride;
        }
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("event slice manifest"), "{error}");
    }
}

#[test]
fn requires_active_counts_in_schema_three_and_omits_them_in_legacy_schemas() {
    for field in ["active_window_count", "active_window_range_count"] {
        let mut fixture = Fixture::new();
        fixture.use_active_windows(WINDOW_SAMPLES, 1, 1);
        fixture.manifest.as_object_mut().unwrap().remove(field);
        let error = fixture.verify().unwrap_err().to_string();
        assert!(
            error.contains("manifest schema or active window counts"),
            "{error}"
        );
        fixture.manifest[field] = Value::Null;
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("parse event slice manifest"), "{error}");

        for version in [1, 2] {
            let mut fixture = Fixture::new();
            fixture.manifest["schema_version"] = json!(version);
            if version == 2 {
                fixture.manifest["event_window_stride_samples"] = json!(OVERLAP_STRIDE_SAMPLES);
            }
            fixture.manifest[field] = json!(1);
            let error = fixture.verify().unwrap_err().to_string();
            assert!(
                error.contains("manifest schema or active window counts"),
                "{error}"
            );
            fixture.manifest[field] = Value::Null;
            let error = fixture.verify().unwrap_err().to_string();
            assert!(error.contains("parse event slice manifest"), "{error}");
        }
    }
}

#[test]
fn counts_active_windows_using_the_declared_stride() {
    for (stride, windows) in [(WINDOW_SAMPLES, 3), (OVERLAP_STRIDE_SAMPLES, 5)] {
        let mut fixture = Fixture::new();
        fixture.use_active_windows(stride, windows, 1);
        assert_eq!(fixture.verify().unwrap().slices.len(), 1);

        fixture.manifest["active_window_count"] = json!(windows + 1);
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains("active window accounting"), "{error}");
    }
}

#[test]
fn constrains_active_window_ranges_and_component_accounting() {
    let mut fixture = Fixture::new();
    for (active, ranges, retained, omitted, message) in [
        (1, 2, 1, 1, "active window accounting"),
        (3, 2, 1, 1, "active window accounting"),
        (1, 0, 1, 0, "active window accounting"),
        (0, 1, 1, 0, "active window accounting"),
        (0, 0, 1, 0, "active window accounting"),
        (1, 1, 0, 0, "active window accounting"),
        (2, 2, 1, 0, "component accounting"),
        (1, 1, MAX_EVENTS + 1, 0, "component accounting"),
        (1, 1, 1, MAX_EVENT_CELLS_PER_WINDOW, "component accounting"),
        (1, 1, 1, u64::MAX, "component accounting"),
    ] {
        fixture.use_active_windows(WINDOW_SAMPLES, active, ranges);
        fixture.manifest["event_count"] = json!(retained);
        fixture.manifest["omitted_component_count"] = json!(omitted);
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn verifies_slices_from_active_ranges_when_components_were_omitted() {
    let align_down = |samples| samples / ALIGNMENT_SAMPLES * ALIGNMENT_SAMPLES;
    let mut fixture = Fixture::with_slice(0, WINDOW_SAMPLES.next_multiple_of(ALIGNMENT_SAMPLES));
    fixture.add_slice(align_down(2 * WINDOW_SAMPLES), align_down(ANALYZED_SAMPLES));
    fixture.use_active_windows(WINDOW_SAMPLES, 2, 2);
    fixture.manifest["omitted_component_count"] = json!(1);
    assert_eq!(fixture.verify().unwrap().slices.len(), 2);

    fixture.manifest["active_window_range_count"] = json!(1);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(
        error.contains("slice count for the declared ranges"),
        "{error}"
    );
}

#[test]
fn rejects_substituted_slice_with_updated_manifest_and_source_edits_outside_slice() {
    let mut fixture = Fixture::new();
    let path = fixture.root.join(FILENAME);
    let mut slice = fs::read(&path).unwrap();
    slice[0] ^= 1;
    fs::write(&path, &slice).unwrap();
    fixture.manifest["slices"][0]["iq_sha256"] = json!(hash(&slice));
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("exact source IQ range"), "{error}");

    let mut fixture = Fixture::new();
    fixture.iq[0] ^= 1;
    fs::write(&fixture.input, &fixture.iq).unwrap();
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("analyzed prefix SHA-256"), "{error}");
}

#[test]
fn rejects_wrong_ordinals_unaligned_spans_and_excess_budgets() {
    for (field, replacement, message) in [
        ("iq_file", json!("../slice-0001.sc16q11"), "filename"),
        ("iq_file", json!("slice-0002.sc16q11"), "filename"),
        ("start_sample", json!(START + 1), "aligned"),
        ("end_sample_exclusive", json!(ANALYZED_SAMPLES), "aligned"),
    ] {
        let mut fixture = Fixture::new();
        fixture.manifest["slices"][0][field] = replacement;
        let error = fixture.verify().unwrap_err().to_string();
        assert!(error.contains(message), "{error}");
    }

    let mut fixture = Fixture::new();
    fixture.manifest["limits"]["max_total_samples"] = json!(END - START - 1);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("export limits"), "{error}");

    fixture.manifest["limits"]["max_total_samples"] = json!(MAX_SAMPLES);
    fixture.manifest["limits"]["merge_gap_samples"] = json!(SAMPLE_RATE_HZ);
    fixture.verify().unwrap();
    fixture.manifest["limits"]["merge_gap_samples"] = json!(SAMPLE_RATE_HZ + 1);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(error.contains("export limits"), "{error}");
}

#[test]
fn requires_adjacent_slices_to_exceed_the_declared_merge_gap() {
    let mut fixture = Fixture::new();
    let start = END + ALIGNMENT_SAMPLES;
    let end = start + ALIGNMENT_SAMPLES;
    fixture.manifest["event_count"] = json!(2);
    fixture.add_slice(start, end);

    assert_eq!(fixture.verify().unwrap().slices.len(), 2);
    fixture.manifest["limits"]["merge_gap_samples"] = json!(ALIGNMENT_SAMPLES - 1);
    assert_eq!(fixture.verify().unwrap().slices.len(), 2);
    fixture.manifest["limits"]["merge_gap_samples"] = json!(ALIGNMENT_SAMPLES);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(
        error.contains("gaps larger than merge_gap_samples"),
        "{error}"
    );

    fixture.manifest["limits"]
        .as_object_mut()
        .unwrap()
        .remove("merge_gap_samples");
    fixture.manifest["slices"][1]["start_sample"] = json!(END);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(
        error.contains("gaps larger than merge_gap_samples"),
        "{error}"
    );
}

#[test]
fn rejects_unexpected_files_symlinked_sidecars_and_sidecar_provenance_changes() {
    let fixture = Fixture::new();
    let extra = fixture.root.join("extra");
    fs::write(&extra, b"extra").unwrap();
    let error = fixture.verify().unwrap_err().to_string();
    assert!(
        error.contains("unexpected event export directory"),
        "{error}"
    );
    fs::remove_file(&extra).unwrap();

    let path = fixture.root.join(sidecar_name(FILENAME));
    let replacement = fixture.input.with_file_name("replacement.json");
    fs::rename(&path, &replacement).unwrap();
    symlink(&replacement, &path).unwrap();
    let error = format!("{:#}", fixture.verify().unwrap_err());
    assert!(error.contains("must be a direct regular file"), "{error}");
    fs::remove_file(&path).unwrap();
    let mut sidecar: Value = serde_json::from_slice(&fs::read(&replacement).unwrap()).unwrap();
    sidecar["source_start_sample"] = json!(START + 10);
    write_json(&path, &sidecar);
    let error = format!("{:#}", fixture.verify().unwrap_err());
    assert!(
        error.contains("sidecar differs from the export geometry"),
        "{error}"
    );
}

#[test]
fn verifies_an_empty_export() {
    let mut fixture = Fixture::new();
    fs::remove_file(fixture.root.join(FILENAME)).unwrap();
    fs::remove_file(fixture.root.join(sidecar_name(FILENAME))).unwrap();
    fixture.manifest["event_count"] = json!(0);
    fixture.manifest["exported_samples"] = json!(0);
    fixture.manifest["slices"] = json!([]);
    let plan = fixture.verify().unwrap();
    assert!(plan.slices.is_empty());
    assert_eq!(plan.exported_samples, 0);

    fixture.use_active_windows(OVERLAP_STRIDE_SAMPLES, 0, 0);
    let plan = fixture.verify().unwrap();
    assert!(plan.slices.is_empty());
    assert_eq!(plan.exported_samples, 0);

    fixture.use_active_windows(OVERLAP_STRIDE_SAMPLES, 1, 1);
    fixture.manifest["event_count"] = json!(1);
    let error = fixture.verify().unwrap_err().to_string();
    assert!(
        error.contains("slice count for the declared ranges"),
        "{error}"
    );
}
