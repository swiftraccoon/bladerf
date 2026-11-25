use super::*;

#[test]
fn nested_decoder_mutation_stops_the_watch_and_preserves_both_attempts() {
    let root = run_root();
    let mut analyses = 0usize;
    let successful = run_plan(
        root.path(),
        &plan(3, 3),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| {
            analyses += 1;
            if analyses == 1 {
                analyzed_with_nested_decoder(session, &"d".repeat(64))
            } else {
                analyzed_with_nested_decoder(session, &"e".repeat(64))
            }
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(analyses, 2);
    assert!(root.path().join("sessions/000001").is_dir());
    assert!(root.path().join("sessions/000002").is_dir());
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["finished"], true);
    assert_eq!(run["successful"], false);
    assert_eq!(run["attempts_completed"], 2);
    assert_eq!(run["stop_reason"], "program_identity_changed");
    assert_eq!(run["attempts"][0]["analysis_state"], "complete");
    assert_eq!(run["attempts"][1]["analysis_state"], "failed");
    assert_eq!(run["nested_bluetooth_decoder"]["sha256"], "d".repeat(64));
    assert_eq!(
        run["attempts"][1]["analysis"]["metrics"]["bluetooth"]["decoder_program"]["sha256"],
        "e".repeat(64)
    );
}

#[test]
fn isolated_failure_continues_but_failed_evidence_is_never_pruned() {
    let root = run_root();
    let mut index = 0;
    let successful = run_plan(
        root.path(),
        &plan(3, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            index += 1;
            if index == 2 {
                fs::write(session.join("failed-evidence"), b"keep").unwrap();
                CaptureStep {
                    state: StepState::Failed,
                    source: None,
                    termination: None,
                    diagnostics: None,
                    detail: Some("synthetic".into()),
                }
            } else {
                captured(session)
            }
        },
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert!(!successful);
    assert!(
        root.path()
            .join("sessions/000002/failed-evidence")
            .is_file()
    );
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["plan_completed"], true);
    assert_eq!(run["successful"], false);
    assert_eq!(run["attempts"][1]["retained"], true);
}

#[test]
fn pending_failed_cycle_cannot_evict_the_only_completed_raw_session() {
    let root = run_root();
    let mut index = 0;
    let successful = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            index += 1;
            if index == 1 {
                captured(session)
            } else {
                fs::write(session.join("failed-evidence"), b"keep").unwrap();
                CaptureStep {
                    state: StepState::Failed,
                    source: None,
                    termination: None,
                    diagnostics: None,
                    detail: Some("synthetic".into()),
                }
            }
        },
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert!(!successful);
    assert!(root.path().join("sessions/000001/iq.sc16q11").is_file());
    assert!(
        root.path()
            .join("sessions/000002/failed-evidence")
            .is_file()
    );
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"][0]["retained"], true);
    assert_eq!(run["attempts"][1]["retained"], true);
}

#[test]
fn interruption_between_capture_and_analysis_retains_iq_and_stops() {
    let root = run_root();
    let stop = AtomicBool::new(false);
    let successful = run_plan(
        root.path(),
        &plan(3, 1),
        &stop,
        || Ok(()),
        |_, session, flag| {
            let result = captured(session);
            flag.store(true, Ordering::Relaxed);
            result
        },
        |_, _, _| panic!("analysis must not start after interruption"),
    )
    .unwrap();
    assert!(!successful);
    assert!(root.path().join("sessions/000001/iq.sc16q11").is_file());
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(run["attempts"][0]["analysis_state"], "not_started");
    assert_eq!(run["attempts"][0]["retained"], true);
    assert_eq!(run["stop_reason"], "interrupted");
}

#[test]
fn program_mutation_after_capture_stops_before_analysis() {
    let root = run_root();
    let mut verifications = 0;
    let successful = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || {
            verifications += 1;
            ensure!(verifications < 2, "changed");
            Ok(())
        },
        |_, session, _| captured(session),
        |_, _, _| panic!("analysis must not start after identity change"),
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "program_identity_changed");
    assert_eq!(run["attempts"][0]["capture_state"], "failed");
}

#[test]
fn interruption_during_identity_check_prevents_another_capture() {
    let root = run_root();
    let stop = AtomicBool::new(false);
    let successful = run_plan(
        root.path(),
        &plan(2, 1),
        &stop,
        || {
            stop.store(true, Ordering::Relaxed);
            Ok(())
        },
        |_, _, _| panic!("capture must not start after interruption"),
        |_, _, _| panic!("analysis must not start after interruption"),
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(
        fs::read_dir(root.path().join("sessions")).unwrap().count(),
        0
    );
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["interrupted"], true);
    assert_eq!(run["stop_reason"], "interrupted");
}

#[test]
fn consecutive_failure_limit_stops_finite_plan() {
    let root = run_root();
    let mut bounded = plan(5, 1);
    bounded.max_consecutive_failures = 2;
    let successful = run_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            fs::write(session.join("failed-evidence"), b"keep").unwrap();
            CaptureStep {
                state: StepState::Failed,
                source: None,
                termination: None,
                diagnostics: None,
                detail: Some("synthetic".into()),
            }
        },
        |_, _, _| panic!("analysis must not start after capture failure"),
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(run["stop_reason"], "consecutive_failure_limit_reached");
    assert!(
        root.path()
            .join("sessions/000001/failed-evidence")
            .is_file()
    );
    assert!(
        root.path()
            .join("sessions/000002/failed-evidence")
            .is_file()
    );
}

#[test]
fn retained_byte_threshold_stops_before_opening_an_unaffordable_capture() {
    let root = run_root();
    let mut bounded = plan(2, 1);
    bounded.retained_limit_bytes = 1;
    let successful = run_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        |_, _, _| panic!("capture must not start beyond the byte threshold"),
        |_, _, _| panic!("analysis must not start without a capture"),
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "retained_byte_threshold_before_capture");
    assert_eq!(run["attempts"].as_array().unwrap().len(), 0);
}

#[test]
fn pruning_rejects_symlink_without_touching_external_evidence() {
    let root = run_root();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"preserved").unwrap();
    let mut inserted = false;
    let result = run_plan(
        root.path(),
        &plan(2, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            let result = captured(session);
            if !inserted {
                std::os::unix::fs::symlink(outside.path(), session.join("hostile-link")).unwrap();
                inserted = true;
            }
            result
        },
        |_, session, _| analyzed(session),
    );
    assert!(result.is_err());
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"preserved");
}

#[test]
fn outputs_and_snapshots_are_private() {
    let root = run_root();
    run_plan(
        root.path(),
        &plan(1, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| captured(session),
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert_eq!(
        fs::metadata(root.path().join("run.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(root.path().join("sessions/000001"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!root.path().join(".run.json.next").exists());
}
