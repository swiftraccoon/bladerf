use super::*;
use std::io::Write;

#[test]
fn replay_patch_check_accepts_later_application_changes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let source = root.join(Engine::BlueDragon.source());
    let app = source.join("crates/app/src/main.rs");
    let reader = source.join("crates/sdr/src/file.rs");
    let patch = root.join(OFFLINE_PATCH);
    for file in [&app, &reader, &patch] {
        fs::create_dir_all(file.parent().unwrap()).unwrap();
    }
    let initialized = Command::new("git")
        .current_dir(root)
        .args(["init", "--quiet", "--template="])
        .status()
        .unwrap();
    assert!(initialized.success());
    fs::write(
        &patch,
        concat!(
            "diff --git a/crates/app/src/main.rs b/crates/app/src/main.rs\n",
            "--- a/crates/app/src/main.rs\n",
            "+++ b/crates/app/src/main.rs\n",
            "@@ -1 +1 @@\n",
            "-const MODE: &str = \"upstream\";\n",
            "+const MODE: &str = \"file-replay\";\n",
            "diff --git a/crates/sdr/src/file.rs b/crates/sdr/src/file.rs\n",
            "--- a/crates/sdr/src/file.rs\n",
            "+++ b/crates/sdr/src/file.rs\n",
            "@@ -1 +1 @@\n",
            "-const SAMPLE_SCALE: u8 = 1;\n",
            "+const SAMPLE_SCALE: u8 = 16;\n",
        ),
    )
    .unwrap();
    fs::write(&app, "const MODE: &str = \"upstream\";\n").unwrap();
    fs::write(&reader, "const SAMPLE_SCALE: u8 = 1;\n").unwrap();
    assert!(check_patch(root).is_err());

    fs::write(&app, "const MODE: &str = \"file-replay\";\n").unwrap();
    fs::write(&reader, "const SAMPLE_SCALE: u8 = 16;\n").unwrap();
    check_patch(root).unwrap();
    fs::write(&app, "const MODE: &str = \"coded-replay\";\n").unwrap();
    check_patch(root).unwrap();

    fs::write(&reader, "const SAMPLE_SCALE: u8 = 8;\n").unwrap();
    assert!(check_patch(root).is_err());
}

fn fixture(directory: &Path, values: &[i16], samples: u64) -> PathBuf {
    let input = directory.join("input.sc16q11");
    let mut file = File::create(&input).unwrap();
    for value in values {
        file.write_all(&value.to_le_bytes()).unwrap();
    }
    drop(file);
    json_file(
        &adjacent_json(&input),
        &json!({
            "format":"SC16_Q11",
            "dtype":"<i2",
            "interleave":"I,Q",
            "complex_scale":2048,
            "frequency_hz":2_437_000_000.0,
            "sample_rate_hz":20_000_000.0,
            "bandwidth_hz":20_000_000.0,
            "num_samples":samples,
        }),
    )
    .unwrap();
    input
}

fn args(input: PathBuf) -> ReplayIq {
    ReplayIq {
        input,
        center_mhz: 2437,
        sample_rate: SAMPLE_RATE_HZ,
        max_samples: 10,
        squelch: -45.0,
        decoder_timeout: 1.0,
        burst_limit_mib: 1,
        output_limit_mib: 2,
        coded_scan: false,
        coded_scan_frequency_mhz: None,
        omit_scan_iq: false,
        omit_squelch_iq: false,
        output: None,
    }
}

#[test]
fn coded_scan_requires_an_exact_primary_advertising_frequency() {
    for (center_mhz, primary_frequencies_mhz) in [
        (2400, &[2402][..]),
        (2412, &[2402]),
        (2413, &[]),
        (2417, &[2426]),
        (2435, &[2426]),
        (2436, &[2426]),
        (2437, &[]),
        (2471, &[2480]),
        (2480, &[2480]),
    ] {
        assert_eq!(
            coded_scan_primary_frequencies_mhz(center_mhz),
            primary_frequencies_mhz
        );
    }

    let mut request = args(PathBuf::from("unused.sc16q11"));
    request.coded_scan = true;
    assert!(request.validate().is_err());
    request.center_mhz = 2435;
    request.validate().unwrap();
}

#[test]
fn explicit_coded_scan_requires_an_interior_data_channel() {
    let mut request = args(PathBuf::from("unused.sc16q11"));
    request.coded_scan_frequency_mhz = Some(2444);
    request.validate().unwrap();
    request.coded_scan = true;
    assert!(request.validate().is_err());
    request.coded_scan = false;
    for frequency in [2402, 2426, 2443, 2448, 2480] {
        request.coded_scan_frequency_mhz = Some(frequency);
        assert!(request.validate().is_err(), "{frequency} MHz");
    }
    request.center_mhz = 2438;
    request.coded_scan_frequency_mhz = Some(2428);
    assert!(request.validate().is_err(), "negative Nyquist bin");
}

#[test]
fn omitting_scan_iq_requires_an_explicit_frequency() {
    let mut request = args(PathBuf::from("unused.sc16q11"));
    request.omit_scan_iq = true;
    assert!(request.validate().is_err());
    request.coded_scan_frequency_mhz = Some(2444);
    request.validate().unwrap();

    let command = decoder_command(
        Path::new("."),
        Path::new("blue-dragon"),
        &request.input,
        Path::new("candidates.pcap"),
        Path::new("bursts.bdb"),
        &request,
    );
    assert!(command.get_args().any(|arg| arg == "--omit-scan-iq"));
    let mut receipt = json!({});
    request.record_coded_scan(&mut receipt);
    assert_eq!(receipt["coded_scan_iq_omitted"], true);
}

#[test]
fn omitting_squelch_iq_requires_selected_scan_iq_omission() {
    use clap::Parser;

    let base = ["bluetooth-monitor", "replay-iq", "unused.sc16q11"];
    for invalid in [
        &["--omit-squelch-iq"][..],
        &["--omit-squelch-iq", "--coded-scan-frequency-mhz", "2444"],
        &["--omit-squelch-iq", "--omit-scan-iq"],
    ] {
        assert!(
            crate::Cli::try_parse_from(base.into_iter().chain(invalid.iter().copied())).is_err()
        );
    }
    let cli = crate::Cli::try_parse_from(base.into_iter().chain([
        "--coded-scan-frequency-mhz",
        "2444",
        "--omit-scan-iq",
        "--omit-squelch-iq",
    ]))
    .unwrap();
    let crate::Action::ReplayIq(mut request) = cli.command else {
        panic!("expected replay");
    };
    request.validate().unwrap();
    let command = decoder_command(
        Path::new("."),
        Path::new("decoder"),
        &request.input,
        Path::new("candidates.pcap"),
        Path::new("bursts.bdb"),
        &request,
    );
    let forwarded: Vec<_> = command.get_args().collect();
    assert!(forwarded.ends_with(&["--omit-scan-iq".as_ref(), "--omit-squelch-iq".as_ref()]));

    let mut receipt = json!({});
    request.record_coded_scan(&mut receipt);
    assert_eq!(receipt["coded_scan_squelch_iq_omitted"], true);
    assert_eq!(request.terminal_schema(), 4);
    request.omit_scan_iq = false;
    assert!(request.validate().is_err());
    request.omit_squelch_iq = false;
    assert_eq!(request.terminal_schema(), 2);
}

fn sample_bdb_record(scan: bool) -> Vec<u8> {
    let mut bytes = Vec::from(*b"BDBURST\0\x01\0\x10\0\x80\x84\x1e\0BRST");
    bytes.extend_from_slice(&52u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&2_444u32.to_le_bytes());
    bytes.extend_from_slice(&(-45.0f32).to_le_bytes());
    bytes.extend_from_slice(&(-80.0f32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&u32::from(scan).to_le_bytes());
    bytes.extend_from_slice(&0.25f32.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}

#[test]
fn squelch_iq_omission_requires_a_complete_untruncated_empty_bdb() {
    const COMPLETE: &str = "done (test) samples: 10\ncoded scan coverage v2: 2444 1 0 1 0 1\n";
    let ordinary = sample_bdb_record(false);
    let scan = sample_bdb_record(true);
    let empty = &ordinary[..EMPTY_BURST_ARCHIVE_BYTES as usize];
    let mut bad_header = empty.to_vec();
    bad_header[8] = 2;
    let capped = format!("{COMPLETE}Burst capture size limit reached; recording stopped\n");

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let input = fixture(root, &[0; 20], 10);
    let decoder = root.join("decoder");
    fs::write(
        &decoder,
        r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --write) cp fixture.pcap "$2"; shift ;;
        --write-bursts) cp fixture.bdb "$2"; shift ;;
    esac
    shift
done
cat fixture.log
"#,
    )
    .unwrap();
    fs::set_permissions(&decoder, fs::Permissions::from_mode(0o700)).unwrap();
    let mut pcap = Vec::from(*b"\xd4\xc3\xb2\xa1\x02\0\x04\0");
    pcap.extend_from_slice(&[0; 8]);
    pcap.extend_from_slice(&4_096u32.to_le_bytes());
    pcap.extend_from_slice(&256u32.to_le_bytes());
    fs::write(root.join("fixture.pcap"), pcap).unwrap();

    for (name, archive, log, schema) in [
        ("omitted", empty, COMPLETE, Some(4)),
        ("scan-only-omitted", ordinary.as_slice(), COMPLETE, Some(3)),
        ("ordinary-record", ordinary.as_slice(), COMPLETE, None),
        ("scan-record", scan.as_slice(), COMPLETE, None),
        ("wrong-header", bad_header.as_slice(), COMPLETE, None),
        ("short-header", &empty[..15], COMPLETE, None),
        ("archive-limit", empty, capped.as_str(), None),
        ("no-coverage", empty, "done (test) samples: 10\n", None),
        (
            "legacy-coverage",
            empty,
            "done (test) samples: 10\ncoded scan coverage v1: 2444 0 0 0 0 0\n",
            None,
        ),
        (
            "wrong-coverage",
            empty,
            "done (test) samples: 10\ncoded scan coverage v2: 2444 1 0 1 0 2\n",
            None,
        ),
        (
            "partial-input",
            empty,
            "done (test) samples: 0\ncoded scan coverage v2: 2444 1 0 1 0 1\n",
            None,
        ),
    ] {
        fs::write(root.join("fixture.bdb"), archive).unwrap();
        fs::write(root.join("fixture.log"), log).unwrap();
        let output = root.join(name);
        let mut request = args(input.clone());
        request.output = Some(output.clone());
        request.coded_scan_frequency_mhz = Some(2444);
        request.omit_scan_iq = true;
        request.omit_squelch_iq = schema != Some(3);
        let result = run(root, &decoder, &request, &AtomicBool::new(false));
        if let Some(schema) = schema {
            result.unwrap_or_else(|error| panic!("{name}: {error:#}"));
            for (file, version) in [("request.json", 1), ("replay.json", schema)] {
                let receipt: Value =
                    serde_json::from_slice(&fs::read(output.join(file)).unwrap()).unwrap();
                assert_eq!(receipt["schema_version"], version);
                assert_eq!(receipt["coded_scan_iq_omitted"], true);
                assert_eq!(
                    receipt.get("coded_scan_squelch_iq_omitted"),
                    (schema == 4).then_some(&Value::Bool(true))
                );
            }
            let bursts: Value =
                serde_json::from_slice(&fs::read(output.join("bursts.json")).unwrap()).unwrap();
            assert_eq!(bursts["source_bytes"], archive.len());
            assert_eq!(bursts["scan_window_record_count"], 0);
            assert_eq!(bursts["squelch_record_count"], u8::from(schema == 3));
            assert_eq!(bursts["complex_sample_count"], u8::from(schema == 3));
        } else {
            assert!(result.is_err(), "{name} unexpectedly succeeded");
            assert!(!output.join("replay.json").exists(), "{name}");
        }
    }
}

#[test]
fn verifies_observed_scan_extent_in_channel_samples() {
    for (samples, fields) in [
        (327_679, "2444 0 0 0 0 0"),
        (327_680, "2444 1 0 32768 0 32768"),
        (655_360, "2444 2 0 32768 16384 49152"),
        (19_988_480, "2444 61 0 32768 1949696 49152"),
    ] {
        let report = CodedScanCoverage::parse(&format!("coded scan coverage v1: {fields}\n"))
            .unwrap()
            .unwrap();
        assert!(serde_json::to_value(&report).unwrap()["window_geometry_version"].is_null());
        verify_scan_coverage(&report, 2444, samples).unwrap();
        assert!(verify_scan_coverage(&report, 2442, samples).is_err());
        assert!(verify_scan_coverage(&report, 2444, samples + 327_680).is_err());
    }
    let wrong_span =
        CodedScanCoverage::parse("coded scan coverage v1: 2444 61 0 32768 1949697 49152\n")
            .unwrap()
            .unwrap();
    assert!(verify_scan_coverage(&wrong_span, 2444, 19_988_480).is_err());
}

#[test]
fn complete_scan_coverage_includes_short_files_and_the_final_partial_window() {
    for (samples, fields) in [
        (240_000, "2444 1 0 24000 0 24000"),
        (327_680, "2444 1 0 32768 0 32768"),
        (400_000, "2444 2 0 32768 0 40000"),
        (655_360, "2444 2 0 32768 0 65536"),
        (656_000, "2444 3 0 32768 31456 34144"),
        (19_988_480, "2444 61 0 32768 1932000 66848"),
    ] {
        let report = CodedScanCoverage::parse(&format!("coded scan coverage v2: {fields}\n"))
            .unwrap()
            .unwrap();
        verify_scan_coverage(&report, 2444, samples).unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap()["window_geometry_version"],
            2
        );
        assert!(verify_scan_coverage(&report, 2444, samples + 10).is_err());
    }
}

#[test]
fn retained_complete_scan_window_must_match_its_indexed_extent() {
    let report = CodedScanCoverage::parse("coded scan coverage v2: 2444 3 0 32768 31456 34144\n")
        .unwrap()
        .unwrap();
    let mut record = bursts::RecordMetadata {
        archive_index: 3,
        timestamp_seconds: 0,
        timestamp_nanoseconds: 15_728_000,
        frequency_mhz: 2444,
        relative_rssi_db: -50.0,
        relative_noise_db: -127.0,
        sample_count: 34_144,
        scan: true,
    };
    verify_complete_archive_window(&record, &report, 2, 656_000).unwrap();
    assert!(verify_complete_archive_window(&record, &report, 3, 656_000).is_err());
    record.timestamp_nanoseconds += 500;
    assert!(verify_complete_archive_window(&record, &report, 2, 656_000).is_err());
    record.timestamp_nanoseconds -= 500;
    record.sample_count -= 1;
    assert!(verify_complete_archive_window(&record, &report, 2, 656_000).is_err());
}

#[test]
fn requires_a_single_complete_scan_report() {
    let report = "coded scan coverage v1: 2444 1 0 32768 0 32768\n";
    assert!(CodedScanCoverage::parse(report).unwrap().is_some());
    assert!(
        CodedScanCoverage::parse("unrelated decoder log\n")
            .unwrap()
            .is_none()
    );
    for bad in [
        report.trim_end().to_owned(),
        "coded scan coverage\n".to_owned(),
        format!("{report}{report}"),
        format!("{report}{}", report.replace("v1:", "v2:")),
        report.replace("v1:", "v3:"),
        format!("{report}coded scan coverage v1:\n"),
        report.replace("2444", "+2444"),
        report.replace(" 0 32768\n", " 0 32768 1\n"),
        report.replace(" 0 32768\n", " 0\n"),
    ] {
        assert!(CodedScanCoverage::parse(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn decoder_command_opts_into_coded_scan_exactly_once() {
    let input = Path::new("input.sc16q11");
    let candidates = Path::new("candidates.pcap");
    let bursts = Path::new("bursts.bdb");
    let request = args(input.to_owned());
    let ordinary = decoder_command(
        Path::new("."),
        Path::new("decoder"),
        input,
        candidates,
        bursts,
        &request,
    );
    assert_eq!(
        ordinary
            .get_args()
            .filter(|argument| *argument == "--coded-scan")
            .count(),
        0
    );

    let mut coded = request;
    coded.center_mhz = 2435;
    coded.coded_scan = true;
    let command = decoder_command(
        Path::new("."),
        Path::new("decoder"),
        input,
        candidates,
        bursts,
        &coded,
    );
    assert_eq!(
        command
            .get_args()
            .filter(|argument| *argument == "--coded-scan")
            .count(),
        1
    );

    let mut explicit = coded;
    explicit.coded_scan = false;
    explicit.coded_scan_frequency_mhz = Some(2444);
    let command = decoder_command(
        Path::new("."),
        Path::new("decoder"),
        input,
        candidates,
        bursts,
        &explicit,
    );
    let forwarded = command.get_args().collect::<Vec<_>>();
    assert!(!forwarded.iter().any(|&arg| arg == "--coded-scan"));
    assert!(
        forwarded
            .windows(2)
            .any(|pair| pair == ["--coded-scan-frequency-mhz", "2444"])
    );
    let mut receipt = json!({});
    explicit.record_coded_scan(&mut receipt);
    assert_eq!(receipt["coded_scan_requested"], true);
    assert_eq!(receipt["coded_scan_primary_frequencies_mhz"], json!([]));
    assert_eq!(receipt["coded_scan_frequency_mhz"], 2444);
}

#[test]
fn validates_exact_q11_input_and_sidecar() {
    let directory = tempfile::tempdir().unwrap();
    let mut values = vec![0; 20];
    values[..4].copy_from_slice(&[-2048, 2047, -1, 1]);
    let input = fixture(directory.path(), &values, 10);
    let validated = validate_input(&args(input)).unwrap();
    assert_eq!(validated.acquisition.num_samples, 10);
    assert_eq!(validated.input_identity.length, 40);
}

#[test]
fn rejects_out_of_range_partial_and_over_limit_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let mut values = vec![0; 20];
    values[0] = 2048;
    let input = fixture(directory.path(), &values, 10);
    assert!(validate_input(&args(input)).is_err());

    let directory = tempfile::tempdir().unwrap();
    let input = fixture(directory.path(), &[0; 20], 10);
    let mut request = args(input);
    request.max_samples = 9;
    assert!(validate_input(&request).is_err());

    let directory = tempfile::tempdir().unwrap();
    let input = fixture(directory.path(), &[0], 1);
    assert!(validate_input(&args(input)).is_err());

    let directory = tempfile::tempdir().unwrap();
    let input = fixture(directory.path(), &[0, 0], 1);
    let error = validate_input(&args(input)).unwrap_err();
    assert!(error.to_string().contains("channelizer step"));
}

#[test]
fn q11_validation_reports_the_first_bad_component_after_a_chunk_boundary() {
    const COMPONENTS_PER_CHUNK: usize = 32 * 1024;
    let directory = tempfile::tempdir().unwrap();
    let mut values = vec![2047; COMPONENTS_PER_CHUNK + 4];
    values[COMPONENTS_PER_CHUNK + 1] = -2049;
    values[COMPONENTS_PER_CHUNK + 2] = 2048;
    let samples = values.len() as u64 / 2;
    let input = fixture(directory.path(), &values, samples);
    let error = validate_q11(&input, samples).unwrap_err();
    assert_eq!(
        error.to_string(),
        "IQ component 32769 is outside the SC16_Q11 range: -2049"
    );
}

#[test]
fn distinguishes_zero_unvalidated_and_validated_outcomes() {
    assert_eq!(replay_outcome(0, 0), "completed_zero_candidates");
    assert_eq!(
        replay_outcome(3, 0),
        "completed_candidates_without_validated_ble"
    );
    assert_eq!(replay_outcome(3, 1), "completed_with_validated_ble");
}

#[test]
fn publishes_terminal_manifest_without_partial_or_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("replay.json");
    terminal_json_file(&output, &json!({"outcome":"complete"})).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&output).unwrap()).unwrap(),
        json!({"outcome":"complete"})
    );
    assert_eq!(fs::metadata(&output).unwrap().mode() & 0o777, 0o600);
    assert!(!directory.path().join(".replay.json.partial").exists());

    let error = terminal_json_file(&output, &json!({"outcome":"replacement"})).unwrap_err();
    assert!(error.to_string().contains("without replacing"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&output).unwrap()).unwrap(),
        json!({"outcome":"complete"})
    );
    assert!(!directory.path().join(".replay.json.partial").exists());
}

#[test]
fn hardens_only_regular_decoder_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("candidate.pcap");
    fs::write(&artifact, b"pcap").unwrap();
    let mut permissions = fs::metadata(&artifact).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&artifact, permissions).unwrap();
    harden_decoder_artifact(&artifact, "test artifact").unwrap();
    assert_eq!(fs::metadata(&artifact).unwrap().mode() & 0o777, 0o600);

    let not_regular = directory.path().join("directory");
    fs::create_dir(&not_regular).unwrap();
    assert!(harden_decoder_artifact(&not_regular, "test artifact").is_err());
}

#[test]
fn decoder_program_is_content_addressed_and_path_free() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("private-decoder-name");
    fs::write(&executable, b"first decoder bytes").unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();

    let pinned = pin_decoder(&executable).unwrap();
    assert_eq!(pinned.program.schema_version, 1);
    assert_eq!(pinned.program.name, "blue-dragon");
    assert_eq!(pinned.program.bytes, 19);
    assert_eq!(
        pinned.program.sha256,
        "74c357891e1b886028f886c5d6eaf9d848df63d3f12d19f33711677888f238a3"
    );
    let serialized = serde_json::to_string(&pinned.program).unwrap();
    assert!(!serialized.contains("private-decoder-name"));
    assert!(!serialized.contains("device"));
    assert!(!serialized.contains("inode"));

    fs::write(&executable, b"other decoder bytes").unwrap();
    assert!(require_decoder_unchanged(&executable, &pinned).is_err());

    let link = directory.path().join("decoder-link");
    std::os::unix::fs::symlink(&executable, &link).unwrap();
    assert!(pin_decoder(&link).is_err());
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(&executable, permissions).unwrap();
    assert!(pin_decoder(&executable).is_err());
}
