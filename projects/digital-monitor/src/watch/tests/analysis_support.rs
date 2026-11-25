use super::*;

pub(super) fn analyzed(session: &Path) -> AnalysisStep {
    report::private_directory(&session.join("analysis")).unwrap();
    fs::write(session.join("analysis/analysis.json"), b"{}").unwrap();
    let mut protocol_metrics = metrics();
    protocol_metrics.bluetooth.candidate_records = 0;
    protocol_metrics.bluetooth.unknown_crc_packets = 0;
    AnalysisStep {
        state: StepState::Complete,
        compact: Some(CompactAnalysis {
            overall_state: "complete".into(),
            manifest_sha256: "2".repeat(64),
            stages: vec![],
            metrics: Some(protocol_metrics),
            coded_shadow: None,
        }),
        detail: None,
    }
}

pub(super) fn analyzed_with_bluetooth_candidate(session: &Path) -> AnalysisStep {
    let mut step = analyzed(session);
    let bluetooth = &mut step
        .compact
        .as_mut()
        .unwrap()
        .metrics
        .as_mut()
        .unwrap()
        .bluetooth;
    bluetooth.candidate_records = 1;
    bluetooth.unknown_crc_packets = 1;
    step
}

pub(super) fn analyzed_with_coded_shadow(
    session: &Path,
    validated_coded_s8_packets: u64,
    validated_coded_s2_packets: u64,
) -> AnalysisStep {
    let mut step = analyzed(session);
    step.compact.as_mut().unwrap().coded_shadow = Some(BluetoothCodedShadow::Complete {
        metrics: Box::new(coded_shadow_metrics(
            validated_coded_s8_packets,
            validated_coded_s2_packets,
        )),
    });
    step
}

pub(super) fn analyzed_with_binary_fsk_group(session: &Path) -> AnalysisStep {
    let mut step = analyzed(session);
    let families = &mut step
        .compact
        .as_mut()
        .unwrap()
        .metrics
        .as_mut()
        .unwrap()
        .burst_families
        .families;
    families[2].group_count = 1;
    families[2].record_count = 2;
    families[2].complex_sample_count = 4;
    step
}

pub(super) fn analyzed_with_wifi_ofdm_data_failure(session: &Path) -> AnalysisStep {
    let mut step = analyzed(session);
    let ofdm = &mut step
        .compact
        .as_mut()
        .unwrap()
        .metrics
        .as_mut()
        .unwrap()
        .wifi
        .decoder_health
        .ofdm;
    ofdm.rejections.data_decode_failed = 1;
    ofdm.data_failures.legacy = 1;
    ofdm.rejected_frames += 1;
    step
}

pub(super) fn analyzed_with_wifi_ofdm_invalid_fcs(session: &Path) -> AnalysisStep {
    let mut step = analyzed(session);
    step.compact
        .as_mut()
        .unwrap()
        .metrics
        .as_mut()
        .unwrap()
        .wifi
        .decoder_health
        .ofdm
        .invalid_fcs = 1;
    step
}

pub(super) fn analyzed_with_nested_decoder(session: &Path, digest: &str) -> AnalysisStep {
    let mut step = analyzed(session);
    step.compact
        .as_mut()
        .unwrap()
        .metrics
        .as_mut()
        .unwrap()
        .bluetooth
        .decoder_program
        .sha256 = digest.to_owned();
    step
}

pub(super) fn run_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}
