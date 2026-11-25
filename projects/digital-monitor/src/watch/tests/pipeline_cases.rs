use super::*;

#[test]
fn capture_returns_before_analysis_and_retention_keeps_only_successes() {
    let root = run_root();
    let capture_active = Cell::new(false);
    let mut analyses = 0;
    let successful = run_plan(
        root.path(),
        &plan(3, 1),
        &AtomicBool::new(false),
        || Ok(()),
        |_, session, _| {
            capture_active.set(true);
            let result = captured(session);
            capture_active.set(false);
            result
        },
        |_, session, _| {
            assert!(!capture_active.get());
            analyses += 1;
            analyzed(session)
        },
    )
    .unwrap();
    assert!(successful);
    assert_eq!(analyses, 3);
    assert_eq!(
        fs::read_dir(root.path().join("sessions")).unwrap().count(),
        1
    );
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["schema_version"], 2);
    assert_eq!(run["successful"], true);
    assert_eq!(run["attempts"][0]["retained"], false);
    assert_eq!(run["attempts"][2]["retained"], true);
    assert_eq!(run["attempts"][2]["retention_priority"], "routine");
    assert_eq!(run["analysis_programs"][0]["name"], "digital-monitor");
    assert_eq!(run["analysis_programs"][0]["sha256"], "b".repeat(64));
    assert_eq!(
        run["attempts"][0]["analysis"]["metrics"]["wifi"]["validated_frames"],
        5
    );
    assert_eq!(
        run["attempts"][0]["analysis"]["metrics"]["bluetooth"]["unknown_crc_packets"],
        0
    );
    assert_eq!(
        run["attempts"][0]["analysis"]["metrics"]["burst_families"]["accounting"]["group_count"],
        1
    );
    assert_eq!(
        run["attempts"][0]["analysis"]["metrics"]["burst_families"]["frequency_bins"][0]["record_count"]
            [4],
        2
    );
    assert_eq!(
        run["attempts"][0]["analysis"]["metrics"]["burst_families"]["eligible_record_features"]["two_level_mode_contrast"]
            ["mean"],
        5.0
    );
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(journal.contains("\"validated_frames\":5"));
    assert!(journal.contains(&"3".repeat(64)));
}

#[test]
fn pipeline_overlaps_capture_with_fifo_analysis_after_durable_phase_publication() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let captures = Arc::new(Mutex::new(Vec::new()));
    let analyses = Arc::new(Mutex::new(Vec::new()));
    let captures_for_run = Arc::clone(&captures);
    let analyses_for_run = Arc::clone(&analyses);
    let (analysis_one_started_tx, analysis_one_started_rx) = channel();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(3, 3),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            let index = session_index(session);
            if index == 2 {
                analysis_one_started_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            captures_for_run.lock().unwrap().push(index);
            captured(session)
        },
        move |_, session, _| {
            let index = session_index(session);
            analyses_for_run.lock().unwrap().push(index);
            if index == 1 {
                analysis_one_started_tx.send(()).unwrap();
                wait_for_durable_capture(&root_path, 2);
            }
            analyzed(session)
        },
    )
    .unwrap();
    assert!(successful);
    assert_eq!(*captures.lock().unwrap(), vec![1, 2, 3]);
    assert_eq!(*analyses.lock().unwrap(), vec![1, 2, 3]);

    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["schema_version"], 3);
    assert_eq!(run["pending_capture_limit"], 1);
    assert_eq!(run["attempts_started"], 3);
    assert_eq!(run["captures_finished"], 3);
    assert_eq!(run["analyses_started"], 3);
    assert_eq!(run["attempts_finished"], 3);
    assert_eq!(run["attempts_completed"], 3);
    assert!(
        run["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| attempt["phase"] == "finished")
    );

    let events: Vec<Value> = fs::read_to_string(root.path().join("attempts.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let position = |name: &str, index: usize| {
        events
            .iter()
            .position(|event| {
                event["event"] == name
                    && (event["index"] == index || event["attempt"]["index"] == index)
            })
            .unwrap()
    };
    assert!(position("analysis_started", 1) < position("attempt_started", 2));
    assert!(position("attempt_started", 2) < position("capture_finished", 2));
    assert!(position("capture_finished", 2) < position("analysis_finished", 1));
    assert!(position("analysis_finished", 1) < position("analysis_started", 2));
    assert!(position("analysis_finished", 1) < position("attempt_started", 3));
}

#[test]
fn pipeline_and_sequential_terminal_semantics_are_normalized_equivalent() {
    let sequential_root = run_root();
    let pipeline_root = run_root();
    let sequential_plan = plan(3, 2);
    let pipelined_plan = pipeline_plan(3, 2);
    assert!(
        run_plan(
            sequential_root.path(),
            &sequential_plan,
            &AtomicBool::new(false),
            || Ok(()),
            |_, session, _| captured(session),
            |_, session, _| analyzed(session),
        )
        .unwrap()
    );
    assert!(
        run_pipelined_plan(
            pipeline_root.path(),
            &pipelined_plan,
            &AtomicBool::new(false),
            || Ok(()),
            || Ok(()),
            |_, session, _| captured(session),
            |_, session, _| analyzed(session),
        )
        .unwrap()
    );
    let sequential: Value =
        serde_json::from_slice(&fs::read(sequential_root.path().join("run.json")).unwrap())
            .unwrap();
    let pipeline: Value =
        serde_json::from_slice(&fs::read(pipeline_root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(sequential["schema_version"], 2);
    assert!(sequential.get("pending_capture_limit").is_none());
    assert!(sequential.get("attempts_started").is_none());
    assert!(sequential["attempts"][0].get("phase").is_none());
    assert!(
        sequential["attempts"][0]
            .get("capture_ended_unix_ns")
            .is_none()
    );
    assert_eq!(pipeline["schema_version"], 3);
    assert_eq!(
        normalized_terminal_run(sequential),
        normalized_terminal_run(pipeline)
    );
    for index in 1..=3 {
        assert_eq!(
            sequential_root
                .path()
                .join(format!("sessions/{index:06}"))
                .exists(),
            pipeline_root
                .path()
                .join(format!("sessions/{index:06}"))
                .exists()
        );
    }
}

#[test]
fn overlap_admission_is_conservative_at_the_failure_boundary() {
    assert!(overlap_capture_admissible(0, 1, 2));
    assert!(overlap_capture_admissible(1, 1, 3));
    assert!(!overlap_capture_admissible(0, 1, 1));
    assert!(!overlap_capture_admissible(1, 1, 2));

    let root = run_root();
    let mut bounded = pipeline_plan(3, 3);
    bounded.max_consecutive_failures = 1;
    let captures = Arc::new(AtomicUsize::new(0));
    let captures_for_run = Arc::clone(&captures);
    let successful = run_pipelined_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            captures_for_run.fetch_add(1, Ordering::SeqCst);
            captured(session)
        },
        |_, _, _| failed_analysis("synthetic analysis failure"),
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["attempts_finished"], 1);
    assert_eq!(run["stop_reason"], "consecutive_failure_limit_reached");
}

#[test]
fn pipeline_interrupt_preserves_the_active_and_later_capture_without_starting_later_analysis() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_analysis = Arc::clone(&stop);
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captures_for_run = Arc::clone(&captures);
    let analyses = Arc::new(Mutex::new(Vec::new()));
    let analyses_for_run = Arc::clone(&analyses);
    let (analysis_started_tx, analysis_started_rx) = channel();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(3, 1),
        stop.as_ref(),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            let index = session_index(session);
            if index == 2 {
                analysis_started_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            captures_for_run.lock().unwrap().push(index);
            captured(session)
        },
        move |_, session, _| {
            let index = session_index(session);
            analyses_for_run.lock().unwrap().push(index);
            assert_eq!(index, 1);
            analysis_started_tx.send(()).unwrap();
            wait_for_durable_capture(&root_path, 2);
            stop_for_analysis.store(true, Ordering::SeqCst);
            analyzed(session)
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(*captures.lock().unwrap(), vec![1, 2]);
    assert_eq!(*analyses.lock().unwrap(), vec![1]);
    for index in [1, 2] {
        assert!(
            root.path()
                .join(format!("sessions/{index:06}/iq.sc16q11"))
                .is_file()
        );
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "interrupted");
    assert_eq!(run["attempts_finished"], 2);
    assert_eq!(run["analyses_started"], 1);
    assert_eq!(run["attempts"][0]["analysis_state"], "complete");
    assert_eq!(run["attempts"][0]["interrupted"], true);
    assert_eq!(run["attempts"][1]["analysis_state"], "not_started");
    assert_eq!(run["attempts"][1]["retained"], true);
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(!journal.contains("\"event\":\"analysis_started\",\"index\":2"));
    assert!(journal.contains("\"event\":\"analysis_skipped\",\"index\":2"));
}

#[test]
fn pipeline_worker_panic_is_durable_joined_and_preserves_the_later_capture() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captures_for_run = Arc::clone(&captures);
    let (analysis_started_tx, analysis_started_rx) = channel();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(3, 3),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            let index = session_index(session);
            if index == 2 {
                analysis_started_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            captures_for_run.lock().unwrap().push(index);
            captured(session)
        },
        move |_, session, _| -> AnalysisStep {
            assert_eq!(session_index(session), 1);
            analysis_started_tx.send(()).unwrap();
            wait_for_durable_capture(&root_path, 2);
            panic!("synthetic worker panic")
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(*captures.lock().unwrap(), vec![1, 2]);
    for index in [1, 2] {
        assert!(
            root.path()
                .join(format!("sessions/{index:06}/iq.sc16q11"))
                .is_file()
        );
    }
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "analysis_worker_failed");
    assert_eq!(run["attempts_finished"], 2);
    assert_eq!(run["attempts"][0]["analysis_state"], "failed");
    assert_eq!(
        run["attempts"][0]["analysis_detail"],
        "offline analysis worker panicked"
    );
    assert_eq!(run["attempts"][1]["analysis_state"], "not_started");
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(journal.contains("\"event\":\"analysis_finished\",\"index\":1"));
    assert!(journal.contains("\"event\":\"analysis_skipped\",\"index\":2"));
}

#[test]
fn pipeline_publishes_terminal_success_only_after_worker_exit_and_final_identity_check() {
    struct ExitFlag(Arc<AtomicBool>);

    impl Drop for ExitFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let root = run_root();
    let worker_exited = Arc::new(AtomicBool::new(false));
    let worker_guard = ExitFlag(Arc::clone(&worker_exited));
    let coordinator_checks = Arc::new(AtomicUsize::new(0));
    let coordinator_checks_for_run = Arc::clone(&coordinator_checks);
    let worker_exited_for_run = Arc::clone(&worker_exited);
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(1, 1),
        &AtomicBool::new(false),
        move || {
            let check = coordinator_checks_for_run.fetch_add(1, Ordering::SeqCst) + 1;
            if check == 3 {
                ensure!(worker_exited_for_run.load(Ordering::SeqCst));
            }
            Ok(())
        },
        move || {
            let _ = &worker_guard;
            Ok(())
        },
        |_, session, _| captured(session),
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert!(successful);
    assert!(worker_exited.load(Ordering::SeqCst));
    assert_eq!(coordinator_checks.load(Ordering::SeqCst), 3);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["finished"], true);
    assert_eq!(run["successful"], true);
}

#[test]
fn pipeline_unexpected_join_panic_is_published_as_terminal_worker_failure() {
    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("synthetic worker-exit panic");
        }
    }

    let root = run_root();
    let guard = PanicOnDrop;
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(1, 1),
        &AtomicBool::new(false),
        || Ok(()),
        move || {
            let _ = &guard;
            Ok(())
        },
        |_, session, _| captured(session),
        |_, session, _| analyzed(session),
    )
    .unwrap();
    assert!(!successful);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["finished"], true);
    assert_eq!(run["successful"], false);
    assert_eq!(run["stop_reason"], "analysis_worker_failed");
    assert_eq!(run["attempts_finished"], 1);
}

#[test]
fn pipeline_program_mutation_finishes_only_the_running_capture_and_preserves_it() {
    let root = run_root();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_for_coordinator = Arc::clone(&changed);
    let changed_for_worker = Arc::clone(&changed);
    let changed_for_analysis = Arc::clone(&changed);
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captures_for_run = Arc::clone(&captures);
    let analyses = Arc::new(Mutex::new(Vec::new()));
    let analyses_for_run = Arc::clone(&analyses);
    let (capture_two_entered_tx, capture_two_entered_rx) = channel();
    let (mutation_done_tx, mutation_done_rx) = channel();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(3, 3),
        &AtomicBool::new(false),
        move || {
            ensure!(!changed_for_coordinator.load(Ordering::SeqCst), "changed");
            Ok(())
        },
        move || {
            ensure!(!changed_for_worker.load(Ordering::SeqCst), "changed");
            Ok(())
        },
        move |_, session, _| {
            let index = session_index(session);
            captures_for_run.lock().unwrap().push(index);
            if index == 2 {
                capture_two_entered_tx.send(()).unwrap();
                mutation_done_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            captured(session)
        },
        move |_, session, _| {
            let index = session_index(session);
            analyses_for_run.lock().unwrap().push(index);
            assert_eq!(index, 1);
            capture_two_entered_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            changed_for_analysis.store(true, Ordering::SeqCst);
            mutation_done_tx.send(()).unwrap();
            analyzed(session)
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(*captures.lock().unwrap(), vec![1, 2]);
    assert_eq!(*analyses.lock().unwrap(), vec![1]);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "program_identity_changed");
    assert_eq!(run["attempts_finished"], 2);
    assert_eq!(run["attempts"][0]["analysis_state"], "failed");
    assert_eq!(run["attempts"][1]["capture_state"], "failed");
    assert_eq!(run["attempts"][1]["analysis_state"], "not_started");
    for index in [1, 2] {
        assert!(
            root.path()
                .join(format!("sessions/{index:06}/iq.sc16q11"))
                .is_file()
        );
    }
}

#[test]
fn pipeline_pending_iq_counts_toward_pressure_and_is_not_a_prune_candidate() {
    let root = run_root();
    let mut bounded = pipeline_plan(3, 1);
    bounded.retained_limit_bytes = 120_000;
    let captures = Arc::new(Mutex::new(Vec::new()));
    let captures_for_run = Arc::clone(&captures);
    let analyses = Arc::new(Mutex::new(Vec::new()));
    let analyses_for_run = Arc::clone(&analyses);
    let successful = run_pipelined_plan(
        root.path(),
        &bounded,
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            let index = session_index(session);
            captures_for_run.lock().unwrap().push(index);
            let step = captured(session);
            fs::write(session.join("iq.sc16q11"), vec![0u8; 40_000]).unwrap();
            step
        },
        move |_, session, _| {
            let index = session_index(session);
            analyses_for_run.lock().unwrap().push(index);
            if index == 1 {
                analyzed(session)
            } else {
                failed_analysis("synthetic pending-evidence failure")
            }
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(*captures.lock().unwrap(), vec![1, 2]);
    assert!(matches!(analyses.lock().unwrap().as_slice(), [1] | [1, 2]));
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert!(matches!(
        run["stop_reason"].as_str(),
        Some("retained_byte_threshold_before_capture" | "retained_byte_threshold_after_analysis")
    ));
    assert_eq!(run["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(run["attempts"][1]["retained"], true);
    assert!(root.path().join("sessions/000002/iq.sc16q11").is_file());
    let journal = fs::read_to_string(root.path().join("attempts.jsonl")).unwrap();
    assert!(!journal.contains("\"event\":\"prune_intent\",\"index\":2"));
}

#[test]
fn pipeline_never_walks_the_managed_tree_while_analysis_is_writing() {
    let root = run_root();
    let root_path = root.path().to_path_buf();
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().to_path_buf();
    let (analysis_started_tx, analysis_started_rx) = channel();
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(2, 2),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        move |_, session, _| {
            if session_index(session) == 2 {
                analysis_started_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            captured(session)
        },
        move |_, session, _| {
            if session_index(session) == 1 {
                let hostile = session.join("analysis-active-link");
                std::os::unix::fs::symlink(&outside_path, &hostile).unwrap();
                analysis_started_tx.send(()).unwrap();
                wait_for_durable_capture(&root_path, 2);
                fs::remove_file(hostile).unwrap();
            }
            analyzed(session)
        },
    )
    .unwrap();
    assert!(successful);
    assert_eq!(
        fs::read_dir(root.path().join("sessions")).unwrap().count(),
        2
    );
}

#[test]
fn pipeline_nested_decoder_mutation_stops_before_the_later_capture_is_analyzed() {
    let root = run_root();
    let analyses = Arc::new(AtomicUsize::new(0));
    let analyses_for_run = Arc::clone(&analyses);
    let successful = run_pipelined_plan(
        root.path(),
        &pipeline_plan(4, 4),
        &AtomicBool::new(false),
        || Ok(()),
        || Ok(()),
        |_, session, _| captured(session),
        move |_, session, _| {
            let index = analyses_for_run.fetch_add(1, Ordering::SeqCst) + 1;
            if index == 1 {
                analyzed_with_nested_decoder(session, &"d".repeat(64))
            } else {
                analyzed_with_nested_decoder(session, &"e".repeat(64))
            }
        },
    )
    .unwrap();
    assert!(!successful);
    assert_eq!(analyses.load(Ordering::SeqCst), 2);
    let run: Value =
        serde_json::from_slice(&fs::read(root.path().join("run.json")).unwrap()).unwrap();
    assert_eq!(run["stop_reason"], "program_identity_changed");
    assert_eq!(run["attempts_finished"], 3);
    assert_eq!(run["attempts"][0]["analysis_state"], "complete");
    assert_eq!(run["attempts"][1]["analysis_state"], "failed");
    assert_eq!(run["attempts"][2]["analysis_state"], "not_started");
    assert_eq!(run["nested_bluetooth_decoder"]["sha256"], "d".repeat(64));
    assert!(root.path().join("sessions/000003/iq.sc16q11").is_file());
}
