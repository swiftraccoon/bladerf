use super::*;

#[test]
fn duplicate_accounting_enforces_the_closed_bounded_partition() {
    let packet = |capacity: u64,
                  distinct: u64,
                  repeated: u64,
                  unresolved: u64,
                  capacity_reached: bool,
                  exact: bool| {
        json!({
            "duplicate_accounting": {
                "normalized_frame_body_capacity": capacity,
                "distinct_validated_frame_bodies": distinct,
                "repeated_validated_packets": repeated,
                "unresolved_validated_packets": unresolved,
                "capacity_reached": capacity_reached,
                "exact": exact
            }
        })
    };

    let empty = packet(
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        0,
        0,
        0,
        false,
        true,
    );
    let empty_metrics = bluetooth_duplicate_accounting(&empty, 0).unwrap();
    assert!(empty_metrics.exact);
    assert!(!empty_metrics.capacity_reached);

    let exact_capacity = packet(
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        0,
        0,
        true,
        true,
    );
    let exact_capacity_metrics =
        bluetooth_duplicate_accounting(&exact_capacity, BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY)
            .unwrap();
    assert!(exact_capacity_metrics.exact);
    assert!(exact_capacity_metrics.capacity_reached);

    let post_capacity = packet(
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        3,
        2,
        true,
        false,
    );
    let post_capacity_metrics = bluetooth_duplicate_accounting(
        &post_capacity,
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY + 5,
    )
    .unwrap();
    assert!(!post_capacity_metrics.exact);
    assert_eq!(post_capacity_metrics.unresolved_validated_packets, 2);

    let exact_duplicates = packet(
        BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        2,
        3,
        0,
        false,
        true,
    );
    assert!(bluetooth_duplicate_accounting(&exact_duplicates, 5).is_ok());

    assert!(bluetooth_duplicate_accounting(&json!({}), 0).is_err());
    let mut missing_key = empty.clone();
    missing_key["duplicate_accounting"]
        .as_object_mut()
        .unwrap()
        .remove("repeated_validated_packets");
    assert!(bluetooth_duplicate_accounting(&missing_key, 0).is_err());

    let mut extra_private_field = empty.clone();
    extra_private_field["duplicate_accounting"]["normalized_body_sha256"] = json!("not-retained");
    assert!(bluetooth_duplicate_accounting(&extra_private_field, 0).is_err());

    let mut non_counter = empty.clone();
    non_counter["duplicate_accounting"]["distinct_validated_frame_bodies"] = json!("zero");
    assert!(bluetooth_duplicate_accounting(&non_counter, 0).is_err());
    let mut non_bool = empty.clone();
    non_bool["duplicate_accounting"]["capacity_reached"] = json!(0);
    assert!(bluetooth_duplicate_accounting(&non_bool, 0).is_err());

    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY - 1,
                0,
                0,
                0,
                false,
                true,
            ),
            0,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY + 1,
                0,
                0,
                true,
                true,
            ),
            BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY + 1,
        )
        .is_err()
    );

    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                0,
                0,
                false,
                true,
            ),
            BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                1,
                0,
                0,
                true,
                true,
            ),
            1,
        )
        .is_err()
    );

    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                1,
                0,
                0,
                false,
                false,
            ),
            1,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                0,
                1,
                true,
                true,
            ),
            BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY + 1,
        )
        .is_err()
    );

    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                0,
                1,
                0,
                false,
                true,
            ),
            1,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                1,
                0,
                1,
                false,
                false,
            ),
            2,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                1,
                0,
                0,
                false,
                true,
            ),
            2,
        )
        .is_err()
    );
    assert!(
        bluetooth_duplicate_accounting(
            &packet(
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                BLUETOOTH_NORMALIZED_FRAME_BODY_CAPACITY,
                u64::MAX,
                0,
                true,
                true,
            ),
            u64::MAX
        )
        .is_err()
    );
}

#[test]
fn command_is_fixed_receive_only_rx1_stock_capture() {
    let arguments: Vec<String> =
        capture_arguments(&plan(1, 1), DEFAULT_TEST_CENTER_HZ, Path::new("iq"))
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
    assert_eq!(arguments[0], "capture");
    assert!(
        !arguments
            .iter()
            .any(|value| value == "--discard-initial-samples")
    );
    for pair in [
        ["--frequency", "2437000000"],
        ["--sample-rate", "20000000"],
        ["--channel", "1"],
        ["--require-fpga-version", "0.16.0"],
    ] {
        let position = arguments.iter().position(|value| value == pair[0]).unwrap();
        assert_eq!(arguments[position + 1], pair[1]);
    }
    for forbidden in ["tx", "bias", "flash", "load-fpga", "loopback"] {
        assert!(!arguments.iter().any(|value| value.contains(forbidden)));
    }
}

#[test]
fn configured_center_binds_capture_analysis_validation_and_report() {
    let mut selected = plan(1, 1);
    selected.center_frequency_schedule_hz = vec![2_433_000_000];
    let arguments: Vec<String> = capture_arguments(&selected, 2_433_000_000, Path::new("iq"))
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    let frequency = arguments
        .iter()
        .position(|value| value == "--frequency")
        .unwrap();
    assert_eq!(arguments[frequency + 1], "2433000000");
    assert_eq!(
        live_expectation(&selected, 2_433_000_000).center_frequency_hz,
        2_433_000_000
    );

    let mut request = request_for_validation();
    request.cycles = 1;
    request.center_mhz = 2433;
    request.alternate_center_mhz = None;
    request.keep_sessions = 1;
    assert_eq!(
        analysis_request(
            &request,
            2433,
            PathBuf::from("iq"),
            PathBuf::from("analysis")
        )
        .center_mhz,
        2433
    );

    let directory = run_root();
    let state = RunState {
        attempts: Vec::new(),
        retained_routine_successes: VecDeque::new(),
        retained_interesting_successes: VecDeque::new(),
        nested_bluetooth_decoder: None,
    };
    save_run(directory.path(), &selected, &state, 0.0, false, None).unwrap();
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["center_frequency_hz"], 2_433_000_000u64);
    assert!(report.get("center_frequency_schedule_hz").is_none());
}

#[test]
fn ordinary_watch_prepares_two_seconds_and_keeps_shorter_analysis_limits() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("program");
    fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let mut request = request_for_validation();
    request.duration_seconds = 2.0;
    request.alternate_center_mhz = None;
    request.capture_timeout_seconds = 22;
    request.retained_limit_mib = 153;
    request.receiver = executable.clone();
    request.programs = ProgramPaths {
        wifi: executable.clone(),
        rf: executable.clone(),
        ieee: executable.clone(),
        ieee_worker: executable.clone(),
        bluetooth: executable,
    };

    let prepared = prepare(&request).unwrap();
    assert_eq!(prepared.plan.samples, 40_000_000);
    assert_eq!(
        live_expectation(&prepared.plan, DEFAULT_TEST_CENTER_HZ).exact_samples,
        40_000_000
    );

    for (duration, expected_samples) in [
        (2.0, 40_000_000),
        (1.5, 30_000_000),
        (1.0, 20_000_000),
        (0.5, 20_000_000),
    ] {
        request.duration_seconds = duration;
        let analysis = analysis_request(
            &request,
            request.center_mhz,
            PathBuf::from("iq"),
            PathBuf::from("analysis"),
        );
        assert_eq!(analysis.max_samples, expected_samples);
    }
}

#[test]
fn watch_rejects_longer_durations_for_ordinary_and_coded_shadow_modes() {
    let mut request = request_for_validation();
    request.duration_seconds = 2.000_001;
    assert!(
        prepare(&request)
            .err()
            .unwrap()
            .to_string()
            .contains("at most 2 seconds")
    );

    request.duration_seconds = 1.000_000_001;
    request.bluetooth_coded_shadow = true;
    assert!(
        prepare(&request)
            .err()
            .unwrap()
            .to_string()
            .contains("bluetooth-coded-shadow requires a duration of at most one second")
    );
}

#[test]
fn initial_discard_reaches_capture_validation_and_run_provenance() {
    let mut selected = plan(1, 1);
    selected.discard_initial_samples = 2_000_000;
    let arguments = capture_arguments(&selected, DEFAULT_TEST_CENTER_HZ, Path::new("iq"));
    assert_eq!(
        &arguments[arguments.len() - 2..],
        ["--discard-initial-samples", "2000000"]
    );
    let expected = live_expectation(&selected, DEFAULT_TEST_CENTER_HZ);
    assert_eq!(expected.discarded_initial_samples, 2_000_000);
    assert_eq!(expected.exact_samples, 10_000);

    let directory = run_root();
    let state = RunState {
        attempts: Vec::new(),
        retained_routine_successes: VecDeque::new(),
        retained_interesting_successes: VecDeque::new(),
        nested_bluetooth_decoder: None,
    };
    save_run(directory.path(), &selected, &state, 0.0, false, None).unwrap();
    let report: Value =
        serde_json::from_slice(&fs::read(directory.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["discard_initial_samples_per_capture"], 2_000_000);
    assert_eq!(report["samples_per_capture"], 10_000);

    let mut request = request_for_validation();
    request.duration_seconds = 1.0;
    request.discard_initial_samples = 2_000_000;
    assert!(
        prepare(&request)
            .err()
            .unwrap()
            .to_string()
            .contains("capture-timeout")
    );
}

#[test]
fn alternating_request_rejects_duplicate_unbalanced_or_out_of_range_centers() {
    let mut duplicate = request_for_validation();
    duplicate.alternate_center_mhz = Some(duplicate.center_mhz);
    assert!(
        format!("{:#}", prepare(&duplicate).err().unwrap()).contains("must differ from center-mhz")
    );

    let mut one_cycle = request_for_validation();
    one_cycle.cycles = 1;
    assert!(
        format!("{:#}", prepare(&one_cycle).err().unwrap())
            .contains("requires at least two cycles")
    );

    let mut one_slot = request_for_validation();
    one_slot.keep_sessions = 1;
    assert!(
        format!("{:#}", prepare(&one_slot).err().unwrap())
            .contains("keep-sessions of at least two")
    );

    let mut out_of_range = request_for_validation();
    out_of_range.center_mhz = 2432;
    out_of_range.alternate_center_mhz = None;
    assert!(
        format!("{:#}", prepare(&out_of_range).err().unwrap()).contains("within 2433..=2437 MHz")
    );
}

#[test]
fn coded_shadow_requires_an_eligible_center_and_one_scan_window() {
    let mut short = request_for_validation();
    short.bluetooth_coded_shadow = true;
    assert!(
        format!("{:#}", prepare(&short).err().unwrap())
            .contains("requires at least 327680 complex samples")
    );

    let mut ineligible = request_for_validation();
    ineligible.bluetooth_coded_shadow = true;
    ineligible.duration_seconds = 0.02;
    ineligible.center_mhz = 2437;
    ineligible.alternate_center_mhz = None;
    assert!(
        format!("{:#}", prepare(&ineligible).err().unwrap())
            .contains("requires a 2435 MHz watch center")
    );
}

#[test]
fn coded_shadow_selects_schema_five_without_changing_legacy_reports() {
    let root = run_root();
    let state = RunState {
        attempts: Vec::new(),
        retained_routine_successes: VecDeque::new(),
        retained_interesting_successes: VecDeque::new(),
        nested_bluetooth_decoder: None,
    };
    let mut ordinary = plan(1, 1);
    save_run(root.path(), &ordinary, &state, 0.0, false, None).unwrap();
    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 2);
    assert!(report.get("bluetooth_coded_shadow_requested").is_none());
    assert!(report.get("discard_initial_samples_per_capture").is_none());
    assert!(
        !report["retention_policy"]["interesting_reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("bluetooth_coded_validated_packets"))
    );

    ordinary.bluetooth_coded_shadow = true;
    save_run(root.path(), &ordinary, &state, 0.0, false, None).unwrap();
    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 5);
    assert_eq!(report["bluetooth_coded_shadow_requested"], true);
    assert!(
        report["retention_policy"]["interesting_reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("bluetooth_coded_validated_packets"))
    );
}

#[test]
fn coded_shadow_publication_requires_the_center_specific_result() {
    let root = run_root();
    let mut shadow_plan = plan(1, 1);
    shadow_plan.bluetooth_coded_shadow = true;
    shadow_plan.center_frequency_schedule_hz = vec![2_435_000_000];
    let error = run_plan(
        root.path(),
        &shadow_plan,
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured_at_center(session, 2_435_000_000),
        |_, session, _| analyzed(session),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("lacks the coded-shadow result"));

    let root = run_root();
    let mut mixed_plan = plan(2, 2);
    mixed_plan.bluetooth_coded_shadow = true;
    mixed_plan.center_frequency_schedule_hz = vec![2_435_000_000, 2_437_000_000];
    let error = run_plan(
        root.path(),
        &mixed_plan,
        &AtomicBool::new(false),
        || Ok(()),
        |center, session, _| captured_at_center(session, center),
        |source, session, _| {
            if source.center_frequency_hz == 2_435_000_000 {
                analyzed_with_coded_shadow(session, 0, 0)
            } else {
                let mut step = analyzed(session);
                step.compact.as_mut().unwrap().coded_shadow =
                    Some(BluetoothCodedShadow::NotEligible {
                        center_frequency_hz: 2_436_000_000,
                    });
                step
            }
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("lacks the coded-shadow result"));
}

#[test]
fn alternating_centers_bind_attempts_and_preserve_one_session_per_center() {
    let root = run_root();
    let plan = alternating_plan(4, 2, 0);
    let successful = run_plan(
        root.path(),
        &plan,
        &AtomicBool::new(false),
        || Ok(()),
        |center_frequency_hz, session, _| captured_at_center(session, center_frequency_hz),
        |source, session, _| {
            if source.center_frequency_hz == 2_435_000_000 {
                analyzed_with_bluetooth_candidate(session)
            } else {
                analyzed(session)
            }
        },
    )
    .unwrap();
    assert!(successful);

    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 4);
    assert_eq!(
        report["center_frequency_schedule_hz"],
        json!([2_435_000_000u64, 2_437_000_000u64])
    );
    assert_eq!(report["center_selection"], "round_robin_by_attempt_index");
    assert_eq!(report["pending_capture_limit"], 0);
    assert!(report.get("center_frequency_hz").is_none());
    assert_eq!(
        report["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|attempt| attempt["center_frequency_hz"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2_435_000_000, 2_437_000_000, 2_435_000_000, 2_437_000_000]
    );
    assert!(
        report["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| {
                attempt["center_frequency_hz"] == attempt["source"]["center_frequency_hz"]
            })
    );
    assert_eq!(report["attempts"][0]["retained"], false);
    assert_eq!(report["attempts"][1]["retained"], false);
    assert_eq!(report["attempts"][2]["retained"], true);
    assert_eq!(report["attempts"][3]["retained"], true);
    assert_eq!(
        fs::read_dir(root.path().join("sessions")).unwrap().count(),
        2
    );
}

#[test]
fn selected_attempt_center_rejects_a_mismatched_capture_source() {
    let root = run_root();
    let mut plan = alternating_plan(2, 2, 0);
    plan.max_consecutive_failures = 1;
    let successful = run_plan(
        root.path(),
        &plan,
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, _, _| panic!("a center-mismatched capture must not be analyzed"),
    )
    .unwrap();
    assert!(!successful);
    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["attempts"][0]["capture_state"], "failed");
    assert!(
        report["attempts"][0]["capture_detail"]
            .as_str()
            .unwrap()
            .contains("differs from the center selected")
    );
}

#[test]
fn failed_attempt_still_advances_the_alternating_center_schedule() {
    let root = run_root();
    let plan = alternating_plan(2, 2, 0);
    let mut capture_centers = Vec::new();
    let mut analysis_centers = Vec::new();
    let successful = run_plan(
        root.path(),
        &plan,
        &AtomicBool::new(false),
        || Ok(()),
        |center_frequency_hz, session, _| {
            capture_centers.push(center_frequency_hz);
            if center_frequency_hz == 2_435_000_000 {
                CaptureStep {
                    state: StepState::Failed,
                    source: None,
                    termination: None,
                    diagnostics: None,
                    detail: Some("synthetic first-center failure".into()),
                }
            } else {
                captured_at_center(session, center_frequency_hz)
            }
        },
        |source, session, _| {
            analysis_centers.push(source.center_frequency_hz);
            analyzed(session)
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(capture_centers, vec![2_435_000_000, 2_437_000_000]);
    assert_eq!(analysis_centers, vec![2_437_000_000]);

    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(
        report["attempts"][0]["center_frequency_hz"],
        2_435_000_000u64
    );
    assert_eq!(report["attempts"][0]["capture_state"], "failed");
    assert_eq!(
        report["attempts"][1]["center_frequency_hz"],
        2_437_000_000u64
    );
    assert_eq!(report["attempts"][1]["capture_state"], "complete");
}

#[test]
fn pipeline_keeps_each_analysis_bound_to_its_alternating_capture_center() {
    let root = run_root();
    let captured_centers = Arc::new(Mutex::new(Vec::new()));
    let analyzed_centers = Arc::new(Mutex::new(Vec::new()));
    let captured_for_run = Arc::clone(&captured_centers);
    let analyzed_for_run = Arc::clone(&analyzed_centers);
    let successful = run_pipelined_plan(
        root.path(),
        &alternating_plan(4, 2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |center_frequency_hz, session, _| {
            captured_for_run.lock().unwrap().push(center_frequency_hz);
            captured_at_center(session, center_frequency_hz)
        },
        move |source, session, _| {
            analyzed_for_run
                .lock()
                .unwrap()
                .push(source.center_frequency_hz);
            let expected = if session_index(session).is_multiple_of(2) {
                2_437_000_000
            } else {
                2_435_000_000
            };
            assert_eq!(source.center_frequency_hz, expected);
            analyzed(session)
        },
    )
    .unwrap();
    assert!(successful);
    let expected = vec![2_435_000_000, 2_437_000_000, 2_435_000_000, 2_437_000_000];
    assert_eq!(*captured_centers.lock().unwrap(), expected);
    assert_eq!(*analyzed_centers.lock().unwrap(), expected);

    let report: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(report["schema_version"], 4);
    assert_eq!(report["pending_capture_limit"], 1);
    assert_eq!(report["attempts_started"], 4);
    assert_eq!(report["attempts_finished"], 4);
}

#[test]
fn sequential_byte_pressure_stops_when_each_center_has_one_success() {
    let root = run_root();
    let mut bounded = alternating_plan(2, 2, 0);
    bounded.retained_limit_bytes = 500_000;
    let successful = run_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        |center_frequency_hz, session, _| captured_at_center(session, center_frequency_hz),
        |_, session, _| {
            if session_index(session) == 2 {
                fs::write(session.join("analysis-padding"), vec![0u8; 600_000]).unwrap();
            }
            analyzed(session)
        },
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "retained_byte_threshold_after_analysis");
    assert_eq!(run["attempts_completed"], 2);
    assert!(root.path().join("sessions/000001").is_dir());
    assert!(root.path().join("sessions/000002").is_dir());
}

#[test]
fn pipeline_byte_pressure_stops_when_each_center_has_one_success() {
    let root = run_root();
    let mut bounded = alternating_plan(2, 2, 1);
    bounded.retained_limit_bytes = 500_000;
    let successful = run_pipelined_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        |center_frequency_hz, session, _| captured_at_center(session, center_frequency_hz),
        |_, session, _| {
            if session_index(session) == 2 {
                fs::write(session.join("analysis-padding"), vec![0u8; 600_000]).unwrap();
            }
            analyzed(session)
        },
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "retained_byte_threshold_after_analysis");
    assert_eq!(run["attempts_completed"], 2);
    assert!(root.path().join("sessions/000001").is_dir());
    assert!(root.path().join("sessions/000002").is_dir());
}

#[test]
fn capture_diagnostics_require_complete_clean_logs() {
    let session = run_root();
    fs::create_dir(session.path().join("logs")).unwrap();
    let stdout_path = PathBuf::from("logs/acquisition.stdout.log");
    let stderr_path = PathBuf::from("logs/acquisition.stderr.log");
    fs::write(session.path().join(&stdout_path), b"capture complete\n").unwrap();
    fs::write(session.path().join(&stderr_path), b"normal diagnostics\n").unwrap();
    let mut report = CommandReport {
        program: "bladerf-lab",
        arguments: Vec::new(),
        exit_code: Some(0),
        exit_signal: None,
        success: true,
        timed_out: false,
        interrupted: false,
        sigkill_cleanup_sent: true,
        stdout: LogReport {
            path: stdout_path,
            observed_bytes: 17,
            retained_bytes: 17,
            truncated: false,
        },
        stderr: LogReport {
            path: stderr_path,
            observed_bytes: 19,
            retained_bytes: 19,
            truncated: false,
        },
    };
    let clean = capture_diagnostics(session.path(), &report).unwrap();
    assert_eq!(clean.known_io_error_markers, 0);
    assert_eq!(clean.overrun_markers, 0);

    let error = b"[ERROR] receive overflow\n";
    fs::write(session.path().join(&report.stderr.path), error).unwrap();
    report.stderr.observed_bytes = error.len() as u64;
    report.stderr.retained_bytes = error.len() as u64;
    let rejected = capture_diagnostics(session.path(), &report).unwrap();
    assert!(rejected.known_io_error_markers > 0);
    assert!(rejected.overrun_markers > 0);

    report.stderr.truncated = true;
    assert!(capture_diagnostics(session.path(), &report).is_err());
}

#[test]
fn quarantine_removal_does_not_follow_a_post_validation_symlink_swap() {
    let root = run_root();
    let target = root.path().join("quarantine/session");
    let nested = target.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("owned"), b"owned").unwrap();
    let mut budget = TreeBudget::default();
    inspect_tree(
        &target,
        fs::symlink_metadata(&target).unwrap().dev(),
        0,
        &mut budget,
    )
    .unwrap();

    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("sentinel"), b"preserved").unwrap();
    fs::remove_dir_all(&nested).unwrap();
    std::os::unix::fs::symlink(outside.path(), &nested).unwrap();
    remove_quarantined_tree(&target).unwrap();

    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"preserved"
    );
    assert!(!target.exists());
}

#[test]
fn compact_manifest_must_bind_the_exact_accepted_source_and_schema() {
    let directory = tempfile::tempdir().unwrap();
    let accepted = source(directory.path());
    let manifest = directory.path().join("analysis.json");
    let baseline = json!({
        "schema_version":2,
        "operation":"offline_multi_protocol_analysis",
        "overall_state":"complete",
        "source":{
            "sha256":accepted.sha256,
            "sidecar_sha256":accepted.sidecar_sha256,
            "complex_samples":accepted.complex_samples,
            "bytes":accepted.bytes,
            "center_frequency_hz":accepted.center_frequency_hz,
            "sample_rate_hz":accepted.sample_rate_hz,
            "bandwidth_hz":accepted.bandwidth_hz
        },
        "stages":[
            {"name":"wifi","state":"complete"},
            {"name":"rf_survey","state":"complete"},
            {"name":"ieee802154","state":"complete"},
            {"name":"bluetooth","state":"complete"},
            {"name":"burst_families","state":"complete"},
            {"name":"terminal_integrity","state":"complete"}
        ]
    });
    fs::write(&manifest, serde_json::to_vec(&baseline).unwrap()).unwrap();
    assert!(compact_analysis(&manifest, OverallState::Complete, &accepted).is_ok());

    let mut wrong_schema = baseline.clone();
    wrong_schema["schema_version"] = json!(1);
    fs::write(&manifest, serde_json::to_vec(&wrong_schema).unwrap()).unwrap();
    assert!(compact_analysis(&manifest, OverallState::Complete, &accepted).is_err());

    let mut wrong_source = baseline;
    wrong_source["source"]["sha256"] = json!("f".repeat(64));
    fs::write(&manifest, serde_json::to_vec(&wrong_source).unwrap()).unwrap();
    assert!(compact_analysis(&manifest, OverallState::Complete, &accepted).is_err());

    let mut arbitrary_outcome = wrong_source;
    arbitrary_outcome["source"]["sha256"] = json!(accepted.sha256.clone());
    arbitrary_outcome["stages"][5]["outcome"] = json!("identifier-like-string");
    fs::write(&manifest, serde_json::to_vec(&arbitrary_outcome).unwrap()).unwrap();
    assert!(compact_analysis(&manifest, OverallState::Complete, &accepted).is_err());

    arbitrary_outcome["stages"][5]
        .as_object_mut()
        .unwrap()
        .remove("outcome");
    arbitrary_outcome["stages"][0]["state"] = json!("identifier-like-string");
    fs::write(&manifest, serde_json::to_vec(&arbitrary_outcome).unwrap()).unwrap();
    assert!(compact_analysis(&manifest, OverallState::Complete, &accepted).is_err());
}
