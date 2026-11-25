use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::super::CONTROL_SAMPLE_RATE_HZ;
use super::super::matched_rate::{FILTER_HALF_LEN, matched_views};
use super::contract::{
    ManifestFilter, ManifestMapping, ManifestSidecar, ManifestSource, ManifestTarget,
    OUTPUT_COMPLEX_SAMPLES, SOURCE_BYTES, SOURCE_COMPLEX_SAMPLES, SOURCE_SAMPLE_RATE_HZ,
};
use super::lattice::{
    ContextWindow, FIRST_OUTPUT_INDEX, WINDOW_COUNT, WINDOW_OUTPUT_SAMPLES, analyze_lattice,
    context_window, lattice_settings, window_start,
};
use crate::burst_families::{ComplexSample, complex_direct_feature};

fn args(directory: &Path) -> BackgroundControls {
    BackgroundControls {
        cf32_input: directory.join("frequency-2438000000.cf32"),
        manifest: directory.join("channelize-frequency.json"),
        frequency_mhz: 2438,
        output: None,
        max_cf32_input_mib: DEFAULT_MAX_CF32_INPUT_MIB,
        max_output_mib: DEFAULT_MAX_OUTPUT_MIB,
    }
}

fn manifest() -> FrequencyManifest {
    FrequencyManifest {
        schema_version: 1,
        operation: "offline_frequency_channelization".to_owned(),
        acquisition_sidecar: ManifestSidecar {
            bytes: 128,
            sha256: "1".repeat(64),
        },
        source: ManifestSource {
            format: "SC16_Q11_LE".to_owned(),
            center_frequency_hz: 2_437_000_000,
            sample_rate_hz: SOURCE_SAMPLE_RATE_HZ,
            bandwidth_hz: SOURCE_SAMPLE_RATE_HZ,
            available_complex_samples: SOURCE_COMPLEX_SAMPLES,
            processed_complex_samples: SOURCE_COMPLEX_SAMPLES,
            processed_full_input: true,
            source_bytes: SOURCE_BYTES,
            processed_prefix_bytes: SOURCE_BYTES,
            processed_prefix_sha256: "2".repeat(64),
            continuity_guaranteed: false,
        },
        filter: ManifestFilter {
            taps: 225,
            decimation: 5,
            group_delay_source_samples: 112,
            first_output_source_sample_index: 4,
            startup_transient_output_samples: 44,
            tail_flushed: false,
        },
        sample_index_mapping: ManifestMapping {
            output_stride_source_samples: 5,
            latest_source_index_for_output_zero: 4,
            group_delay_corrected_source_index_for_output_zero: -108,
        },
        target: ManifestTarget {
            center_frequency_hz: 2_438_000_000,
            source_offset_hz: 1_000_000,
            source_edge_margin_hz: 7_000_000,
            output_file: "frequency-2438000000.cf32".to_owned(),
            output_format: "CF32_LE".to_owned(),
            output_sample_rate_hz: u64::from(CONTROL_SAMPLE_RATE_HZ),
            output_complex_samples: OUTPUT_COMPLEX_SAMPLES,
            sha256: "3".repeat(64),
        },
        continuity_guaranteed: false,
    }
}

#[test]
fn fixed_lattice_has_exact_disjoint_bounds_and_balanced_parity() {
    let settings = lattice_settings().unwrap();
    assert_eq!(settings.first_context_start_index, 399_984);
    assert_eq!(settings.last_output_start_index, 3_480_239);
    assert_eq!(settings.last_output_end_exclusive, 3_480_751);
    assert_eq!(settings.last_context_end_exclusive, 3_480_767);
    assert_eq!(settings.context_samples_per_window, 544);
    assert_eq!(settings.inter_context_gap_samples, 64_993);
    assert_eq!(settings.absolute_even_start_windows, 24);
    assert_eq!(settings.absolute_odd_start_windows, 24);

    let mut previous_context_end = None;
    for index in 0..WINDOW_COUNT {
        let output_start = window_start(index).unwrap();
        let ContextWindow::Available { start, count } =
            context_window(output_start, 44, OUTPUT_COMPLEX_SAMPLES).unwrap()
        else {
            panic!("fixed context must be available");
        };
        if let Some(previous) = previous_context_end {
            assert_eq!(start - previous, 64_993);
        }
        previous_context_end = Some(start + count);
    }
    assert_eq!(previous_context_end, Some(3_480_767));
    assert!(window_start(WINDOW_COUNT).is_err());
}

#[test]
fn context_bounds_distinguish_start_and_short_tail() {
    assert_eq!(
        context_window(59, 44, OUTPUT_COMPLEX_SAMPLES).unwrap(),
        ContextWindow::StartUnavailable
    );
    assert_eq!(
        context_window(FIRST_OUTPUT_INDEX, 44, FIRST_OUTPUT_INDEX + 527).unwrap(),
        ContextWindow::TailUnavailable
    );
    assert_eq!(
        context_window(FIRST_OUTPUT_INDEX, 44, FIRST_OUTPUT_INDEX + 528).unwrap(),
        ContextWindow::Available {
            start: 399_984,
            count: 544,
        }
    );
    assert!(context_window(u64::MAX, 44, u64::MAX).is_err());
}

#[test]
fn manifest_validation_enforces_full_extent_frequency_and_continuity() {
    let directory = tempfile::tempdir().unwrap();
    let args = args(directory.path());
    let limits = args.limits().unwrap();
    let mut manifest = manifest();
    validate_manifest(&args, limits, &manifest).unwrap();

    manifest.source.processed_full_input = false;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
    manifest.source.processed_full_input = true;
    manifest.target.source_offset_hz = 999_999;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
    manifest.target.source_offset_hz = 1_000_000;
    manifest.target.output_complex_samples -= 1;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
    manifest.target.output_complex_samples = OUTPUT_COMPLEX_SAMPLES;
    manifest.target.center_frequency_hz += 1_000_000;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
    manifest.target.center_frequency_hz -= 1_000_000;
    manifest.source.available_complex_samples -= 1;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
    manifest.source.available_complex_samples = SOURCE_COMPLEX_SAMPLES;
    manifest.source.continuity_guaranteed = true;
    assert!(validate_manifest(&args, limits, &manifest).is_err());
}

#[test]
fn one_sample_absolute_origin_shift_swaps_phase_membership() {
    let context = (0..WINDOW_OUTPUT_SAMPLES as usize + 2 * FILTER_HALF_LEN)
        .map(|index| ComplexSample {
            re: (index as f32 * 0.017).sin(),
            im: (index as f32 * 0.031).cos(),
        })
        .collect::<Vec<_>>();
    let even_origin = matched_views(
        &context,
        FIRST_OUTPUT_INDEX,
        WINDOW_OUTPUT_SAMPLES as usize,
        &AtomicBool::new(false),
    )
    .unwrap();
    let odd_origin = matched_views(
        &context,
        FIRST_OUTPUT_INDEX + 1,
        WINDOW_OUTPUT_SAMPLES as usize,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(even_origin.four_msps, odd_origin.four_msps);
    assert_eq!(
        even_origin.two_msps_global_even,
        odd_origin.two_msps_global_odd
    );
    assert_eq!(
        even_origin.two_msps_global_odd,
        odd_origin.two_msps_global_even
    );
}

#[test]
fn fixed_window_recovers_a_positive_two_tone_control_on_both_origins() {
    let mut phase = 0.0f64;
    let context = (0..WINDOW_OUTPUT_SAMPLES as usize + 2 * FILTER_HALF_LEN)
        .map(|index| {
            let frequency_hz = if (index / 32) % 2 == 0 {
                -470_000.0
            } else {
                470_000.0
            };
            phase += std::f64::consts::TAU * frequency_hz / f64::from(CONTROL_SAMPLE_RATE_HZ);
            ComplexSample {
                re: phase.cos() as f32,
                im: phase.sin() as f32,
            }
        })
        .collect::<Vec<_>>();
    for origin in [FIRST_OUTPUT_INDEX, FIRST_OUTPUT_INDEX + 1] {
        let views = matched_views(
            &context,
            origin,
            WINDOW_OUTPUT_SAMPLES as usize,
            &AtomicBool::new(false),
        )
        .unwrap();
        for (samples, sample_rate_hz) in [
            (views.four_msps.as_slice(), CONTROL_SAMPLE_RATE_HZ),
            (
                views.two_msps_global_even.as_slice(),
                CONTROL_SAMPLE_RATE_HZ / 2,
            ),
            (
                views.two_msps_global_odd.as_slice(),
                CONTROL_SAMPLE_RATE_HZ / 2,
            ),
        ] {
            let feature = complex_direct_feature(samples, sample_rate_hz, &AtomicBool::new(false))
                .unwrap()
                .unwrap();
            assert!(feature.gate_evaluation.all_gates_passed);
        }
    }
}

#[test]
fn zero_background_analysis_is_deterministic_and_closes_accounting() {
    let byte_count = usize::try_from(3_480_767u64 * 8).unwrap();
    let cf32 = vec![0u8; byte_count];
    let (first, first_accounting) =
        analyze_lattice(&cf32, 44, OUTPUT_COMPLEX_SAMPLES, &AtomicBool::new(false)).unwrap();
    let (second, second_accounting) =
        analyze_lattice(&cf32, 44, OUTPUT_COMPLEX_SAMPLES, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert_eq!(first_accounting.analyzed_windows, WINDOW_COUNT);
    assert_eq!(first_accounting.analyzed_output_samples, 24_576);
    assert_eq!(
        serde_json::to_vec(&first_accounting).unwrap(),
        serde_json::to_vec(&second_accounting).unwrap()
    );
    let value = serde_json::to_value(&first).unwrap();
    assert_eq!(
        value["outcome_partition"]["filtered_4msps_feature_insufficient"],
        WINDOW_COUNT
    );
    assert_eq!(value["outcome_partition_is_closed"], true);

    let directory = tempfile::tempdir().unwrap();
    let mut args = args(directory.path());
    args.cf32_input = directory.path().join("PRIVATE_PATH_SENTINEL.cf32");
    args.manifest = directory.path().join("PRIVATE_PATH_SENTINEL.json");
    args.output = Some(directory.path().join("PRIVATE_PATH_SENTINEL-output"));
    let manifest = manifest();
    let first_report = build_report(ReportInputs {
        args: &args,
        manifest: &manifest,
        manifest_identity: ContentIdentity {
            format: "frequency_channelizer_manifest_v1",
            bytes: 1,
            sha256: "4".repeat(64),
        },
        cf32_identity: ContentIdentity {
            format: "CF32_LE",
            bytes: OUTPUT_COMPLEX_SAMPLES * 8,
            sha256: "3".repeat(64),
        },
        lattice: lattice_settings().unwrap(),
        accounting: first_accounting,
        matched_rate: first,
    });
    let second_report = build_report(ReportInputs {
        args: &args,
        manifest: &manifest,
        manifest_identity: ContentIdentity {
            format: "frequency_channelizer_manifest_v1",
            bytes: 1,
            sha256: "4".repeat(64),
        },
        cf32_identity: ContentIdentity {
            format: "CF32_LE",
            bytes: OUTPUT_COMPLEX_SAMPLES * 8,
            sha256: "3".repeat(64),
        },
        lattice: lattice_settings().unwrap(),
        accounting: second_accounting,
        matched_rate: second,
    });
    let first_encoded = serde_json::to_vec(&first_report).unwrap();
    let second_encoded = serde_json::to_vec(&second_report).unwrap();
    assert_eq!(first_encoded, second_encoded);
    assert!(
        !first_encoded
            .windows(b"PRIVATE_PATH_SENTINEL".len())
            .any(|window| window == b"PRIVATE_PATH_SENTINEL")
    );
    let report = serde_json::to_value(first_report).unwrap();
    assert!(report.get("cf32_input").is_none());
    assert!(report.get("manifest").is_none());
    assert!(report.get("output").is_none());
    assert_eq!(
        report["bandwidth_matched_same_stream"]["record_unit"],
        "fixed_background_window"
    );
    assert!(report.get("matched_rate_accounting_unit").is_none());
    assert_eq!(report["thresholds_tuned_on_background_windows"], false);
    assert_eq!(report["statistical_independence_established"], false);
    assert_eq!(report["signal_absence_established"], false);
}

#[test]
fn bounds_and_interruption_fail_before_cf32_access() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = args(directory.path());
    args.max_cf32_input_mib = 31;
    assert!(args.limits().is_err());
    args.max_cf32_input_mib = 64;
    args.max_output_mib = 257;
    assert!(args.limits().is_err());
    args.max_output_mib = DEFAULT_MAX_OUTPUT_MIB;
    assert!(
        run(directory.path(), &args, &AtomicBool::new(true))
            .unwrap_err()
            .to_string()
            .contains("before manifest access")
    );
    assert!(
        analyze_lattice(&[], 44, OUTPUT_COMPLEX_SAMPLES, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("exceeds the retained stream")
    );
    assert!(
        analyze_lattice(&[], 44, OUTPUT_COMPLEX_SAMPLES, &AtomicBool::new(true))
            .unwrap_err()
            .to_string()
            .contains("interrupted")
    );
}

#[test]
fn report_publication_is_private_and_never_replaces() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("background-controls.json");
    publish_report(&path, b"{}\n").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"{}\n");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(publish_report(&path, b"replacement\n").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"{}\n");
    assert!(
        !directory
            .path()
            .join("background-controls.json.partial")
            .exists()
    );
}
