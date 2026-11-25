use super::{
    CommandReport, Duration, ProgramProvenance, Serialize, SourceReport, orchestrate,
    retention_reasons,
};

#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub(super) cycles: u16,
    pub(super) center_frequency_schedule_hz: Vec<u64>,
    pub(super) duration_seconds: f64,
    pub(super) samples: u64,
    pub(super) discard_initial_samples: u64,
    pub(super) gain_db: f64,
    pub(super) max_consecutive_failures: u16,
    pub(super) keep_sessions: usize,
    pub(super) retained_limit_bytes: u64,
    pub(super) pending_captures: u8,
    pub(super) bluetooth_coded_shadow: bool,
    pub(super) capture_timeout: Duration,
    pub(super) log_limit_bytes: u64,
    pub(super) receiver_identity: Option<CompactProgram>,
    pub(super) analysis_program_identities: Vec<CompactProgram>,
}

impl Plan {
    pub(super) fn has_multiple_centers(&self) -> bool {
        self.center_frequency_schedule_hz.len() > 1
    }

    pub(super) fn primary_center_frequency_hz(&self) -> u64 {
        self.center_frequency_schedule_hz[0]
    }

    pub(super) fn center_frequency_hz_for_attempt(&self, index: usize) -> u64 {
        debug_assert!(index > 0);
        self.center_frequency_schedule_hz[(index - 1) % self.center_frequency_schedule_hz.len()]
    }

    pub(super) fn reported_attempt_center(&self, center_frequency_hz: u64) -> Option<u64> {
        self.has_multiple_centers().then_some(center_frequency_hz)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactProgram {
    pub(super) name: &'static str,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

impl From<&ProgramProvenance> for CompactProgram {
    fn from(program: &ProgramProvenance) -> Self {
        Self {
            name: program.name,
            bytes: program.bytes,
            sha256: program.sha256.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum StepState {
    NotStarted,
    InProgress,
    Complete,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AttemptPhase {
    CaptureFinished,
    AnalysisRunning,
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RetentionPriority {
    Routine,
    Interesting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RetentionReason {
    BluetoothCandidateRecords,
    BluetoothCodedValidatedPackets,
    Ieee802154CandidateRecords,
    NarrowbandBinaryFskLikeGroups,
    WifiOfdmDataDecodeFailures,
    WifiOfdmInvalidFcsFrames,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RetentionPruneReason {
    SuccessfulSessionCountLimit,
    RetainedBytePressureBeforeCapture,
    RetainedByteLimitAfterAnalysis,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Termination {
    pub(super) exit_code: Option<i32>,
    pub(super) exit_signal: Option<i32>,
    pub(super) success: bool,
    pub(super) timed_out: bool,
    pub(super) interrupted: bool,
    pub(super) sigkill_cleanup_sent: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CaptureDiagnostics {
    pub(super) stdout_bytes: u64,
    pub(super) stderr_bytes: u64,
    pub(super) logs_complete: bool,
    pub(super) known_io_error_markers: u64,
    pub(super) overrun_markers: u64,
}

impl From<&CommandReport> for Termination {
    fn from(report: &CommandReport) -> Self {
        Self {
            exit_code: report.exit_code,
            exit_signal: report.exit_signal,
            success: report.success,
            timed_out: report.timed_out,
            interrupted: report.interrupted,
            sigkill_cleanup_sent: report.sigkill_cleanup_sent,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) center_frequency_hz: Option<u64>,
    pub(super) complex_samples: u64,
    pub(super) bytes: u64,
    pub(super) sha256: String,
    pub(super) sidecar_sha256: String,
}

impl CompactSource {
    pub(super) fn from_source(source: &SourceReport, include_center: bool) -> Self {
        Self {
            center_frequency_hz: include_center.then_some(source.center_frequency_hz),
            complex_samples: source.complex_samples,
            bytes: source.bytes,
            sha256: source.sha256.clone(),
            sidecar_sha256: source.sidecar_sha256.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactStage {
    pub(super) name: String,
    pub(super) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) outcome: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactAnalysis {
    pub(super) overall_state: String,
    pub(super) manifest_sha256: String,
    pub(super) stages: Vec<CompactStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) metrics: Option<ProtocolMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) coded_shadow: Option<BluetoothCodedShadow>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum BluetoothCodedShadow {
    NotEligible {
        center_frequency_hz: u64,
    },
    Complete {
        metrics: Box<BluetoothCodedShadowMetrics>,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BluetoothCodedShadowMetrics {
    pub(super) request_sha256: String,
    pub(super) replay_sha256: String,
    pub(super) packet_summary_sha256: String,
    pub(super) burst_summary_sha256: String,
    pub(super) candidate_pcap_sha256: String,
    pub(super) validated_pcap_sha256: String,
    pub(super) burst_archive_sha256: String,
    pub(super) decoder_program: NestedDecoderProgram,
    pub(super) input_complex_samples: u64,
    pub(super) decoder_processed_complex_samples: u64,
    pub(super) channelizer_steps_processed: u64,
    pub(super) processed_full_input: bool,
    pub(super) candidate_records: u64,
    pub(super) validated_ble_packets: u64,
    pub(super) validated_le_1m_packets: u64,
    pub(super) validated_le_2m_packets: u64,
    pub(super) validated_coded_s8_packets: u64,
    pub(super) validated_coded_s2_packets: u64,
    pub(super) invalid_crc_packets: u64,
    pub(super) unknown_crc_packets: u64,
    pub(super) malformed_packets: u64,
    pub(super) unsupported_records: u64,
    pub(super) truncated_records: u64,
    pub(super) records_exceeding_snaplen: u64,
    pub(super) classic_candidate_records: u64,
    pub(super) classic_upstream_crc_claims: u64,
    pub(super) scan_window_records: u64,
    pub(super) burst_archive_truncated_by_limit: bool,
    pub(super) radio_opened: bool,
    pub(super) network_requested: bool,
    pub(super) transmit_requested: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProtocolMetrics {
    pub(super) metrics_schema_version: u32,
    pub(super) wifi: WifiMetrics,
    pub(super) rf_survey: RfMetrics,
    pub(super) ieee802154: IeeeMetrics,
    pub(super) bluetooth: BluetoothMetrics,
    pub(super) burst_families: BurstFamilyMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WifiMetrics {
    pub(super) summary_sha256: String,
    pub(super) decoder_summary_sha256: String,
    pub(super) decoder_health: WifiDecoderHealth,
    pub(super) total_records: u64,
    pub(super) validated_frames: u64,
    pub(super) invalid_fcs_frames: u64,
    pub(super) unknown_fcs_frames: u64,
    pub(super) absent_fcs_frames: u64,
    pub(super) malformed_frames: u64,
    pub(super) truncated_records: u64,
    pub(super) records_exceeding_snaplen: u64,
    pub(super) upstream_bad_fcs_claims: u64,
    pub(super) management_frames: u64,
    pub(super) control_frames: u64,
    pub(super) data_frames: u64,
    pub(super) protected_frames: u64,
    pub(super) frame_types: orchestrate::WifiFrameTypeMetrics,
    pub(super) block_ack: orchestrate::WifiBlockAckMetrics,
    pub(super) eapol: orchestrate::WifiEapolMetrics,
    pub(super) advertisements: orchestrate::WifiAdvertisementMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WifiDecoderHealth {
    pub(super) ofdm: WifiOfdmDecoderHealth,
    pub(super) dsss: WifiPhyDecoderHealth,
    pub(super) diagnostic_counts_are_lower_bounds: bool,
    pub(super) diagnostic_batches_at_capacity: u64,
    /// Decoder diagnostics only; the pinned producer does not currently emit
    /// clipping diagnostics, so zero is not an ADC headroom measurement.
    pub(super) clipped_samples: u64,
    pub(super) pending_frame_limit: u64,
    pub(super) buffer_sample_limit: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WifiOfdmDecoderHealth {
    /// FCS-valid frames recovered by this PHY, including any output drops.
    pub(super) valid_frames: u64,
    /// Complete payload decodes whose received FCS did not validate.
    pub(super) invalid_fcs: u64,
    /// Broad decoder rejection count; not protocol or packet evidence.
    pub(super) rejected_frames: u64,
    pub(super) truncated_frames: u64,
    /// FCS-valid frames not published because the output bound was reached.
    pub(super) dropped_frames: u64,
    pub(super) rejections: orchestrate::WifiOfdmRejectionMetrics,
    pub(super) data_failures: orchestrate::WifiOfdmDataFailureMetrics,
    pub(super) strong_long_training: orchestrate::WifiOfdmStrongLongTrainingMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct WifiPhyDecoderHealth {
    /// FCS-valid frames recovered by this PHY, including any output drops.
    pub(super) valid_frames: u64,
    /// Complete payload decodes whose received FCS did not validate.
    pub(super) invalid_fcs: u64,
    /// Broad decoder rejection count; not protocol or packet evidence.
    pub(super) rejected_frames: u64,
    pub(super) truncated_frames: u64,
    /// FCS-valid frames not published because the output bound was reached.
    pub(super) dropped_frames: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RfRegionAggregate {
    pub(super) lowest_frequency_hz: f64,
    pub(super) highest_frequency_hz: f64,
    pub(super) maximum_mean_excess_db: f64,
    pub(super) maximum_transient_excess_db: f64,
    pub(super) maximum_sampled_window_hit_fraction: f64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct RfMetrics {
    pub(super) summary_sha256: String,
    pub(super) input_samples: u64,
    pub(super) inspected_prefix_samples: u64,
    pub(super) windows: u64,
    pub(super) total_cells: u64,
    pub(super) excluded_cells: u64,
    pub(super) triggered_cells: u64,
    pub(super) region_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) region_aggregate: Option<RfRegionAggregate>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct IeeeHypothesisMetrics {
    pub(super) source_index: usize,
    pub(super) decoder_lane: &'static str,
    pub(super) carrier_offset_hz: i32,
    pub(super) source_role: &'static str,
    pub(super) candidate_archive_bytes: u64,
    pub(super) candidate_archive_sha256: String,
    pub(super) inspection_summary_sha256: String,
    pub(super) candidate_records: u64,
    pub(super) valid_fcs_records: u64,
    pub(super) invalid_fcs_records: u64,
    pub(super) invalid_length_records: u64,
    pub(super) validated_psdu_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct IeeeMembershipPartitionMetrics {
    pub(super) source_membership_mask: u8,
    pub(super) source_membership_count: u32,
    pub(super) identity_records: u64,
    pub(super) valid_fcs_records: u64,
    pub(super) invalid_fcs_records: u64,
    pub(super) invalid_length_records: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct IeeeChannelMetrics {
    pub(super) channel: u8,
    pub(super) channelized_input_bytes: u64,
    pub(super) channelized_input_sha256: String,
    pub(super) sources: Vec<IeeeHypothesisMetrics>,
    pub(super) comparison_summary_sha256: String,
    pub(super) source_candidate_records_sum: u64,
    pub(super) source_valid_fcs_records_sum: u64,
    pub(super) source_invalid_fcs_records_sum: u64,
    pub(super) source_invalid_length_records_sum: u64,
    pub(super) identity_union_records: u64,
    pub(super) identity_union_valid_fcs_records: u64,
    pub(super) identity_union_invalid_fcs_records: u64,
    pub(super) identity_union_invalid_length_records: u64,
    pub(super) duplicate_records: u64,
    pub(super) duplicate_valid_fcs_records: u64,
    pub(super) duplicate_invalid_fcs_records: u64,
    pub(super) duplicate_invalid_length_records: u64,
    pub(super) membership_partitions: Vec<IeeeMembershipPartitionMetrics>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct IeeeMetrics {
    pub(super) channelize_summary_sha256: String,
    pub(super) channels: Vec<IeeeChannelMetrics>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct NestedDecoderProgram {
    pub(super) schema_version: u32,
    pub(super) name: &'static str,
    pub(super) bytes: u64,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BluetoothDuplicateAccounting {
    pub(super) normalized_frame_body_capacity: u64,
    pub(super) distinct_validated_frame_bodies: u64,
    pub(super) repeated_validated_packets: u64,
    pub(super) unresolved_validated_packets: u64,
    pub(super) capacity_reached: bool,
    pub(super) exact: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BluetoothMetrics {
    pub(super) replay_sha256: String,
    pub(super) packet_summary_sha256: String,
    pub(super) burst_summary_sha256: String,
    pub(super) decoder_program: NestedDecoderProgram,
    pub(super) input_complex_samples: u64,
    pub(super) decoder_processed_complex_samples: u64,
    pub(super) channelizer_step_complex_samples: u64,
    pub(super) decoder_file_block_complex_samples: u64,
    pub(super) channelizer_steps_processed: u64,
    pub(super) candidate_records: u64,
    pub(super) validated_advertising_packets: u64,
    pub(super) validated_data_packets: u64,
    pub(super) validated_ble_packets: u64,
    pub(super) duplicate_accounting: BluetoothDuplicateAccounting,
    pub(super) invalid_crc_packets: u64,
    pub(super) unknown_crc_packets: u64,
    pub(super) malformed_packets: u64,
    pub(super) truncated_records: u64,
    pub(super) unsupported_records: u64,
    pub(super) records_exceeding_snaplen: u64,
    pub(super) classic_candidate_records: u64,
    pub(super) classic_upstream_crc_claims: u64,
    pub(super) burst_archive_records: u64,
    pub(super) burst_archive_source_bytes: u64,
    pub(super) burst_archive_complex_samples: u64,
    pub(super) burst_archive_squelch_records: u64,
    pub(super) burst_archive_scan_window_records: u64,
    pub(super) burst_archive_truncated_by_limit: bool,
    pub(super) radio_opened: bool,
    pub(super) network_requested: bool,
    pub(super) transmit_requested: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFamilySettings {
    pub(super) channel_sample_rate_hz: u64,
    pub(super) sample_period_ns: u64,
    pub(super) group_connectivity_gap_ns: u64,
    pub(super) group_frequency_step_mhz: u64,
    pub(super) simultaneous_interval: String,
    pub(super) active_power_quantile: [u64; 2],
    pub(super) active_power_divisor: u64,
    pub(super) minimum_usable_phase_differences: u64,
    pub(super) maximum_envelope_cv: f64,
    pub(super) minimum_cluster_balance: f64,
    pub(super) minimum_variance_reduction: f64,
    pub(super) minimum_mode_contrast: f64,
    pub(super) mode_contrast_std_floor_hz: f64,
    pub(super) minimum_phase_concentration: f64,
    pub(super) separation_hz: [f64; 2],
    pub(super) minimum_transitions: u64,
    pub(super) kmeans_iterations: u64,
    pub(super) adjacent_multibin_minimum_width: u64,
    pub(super) wideband_multibin_minimum_width: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFamilyAccounting {
    pub(super) archive_record_count: u64,
    pub(super) squelch_record_count: u64,
    pub(super) scan_window_record_count: u64,
    pub(super) archive_complex_sample_count: u64,
    pub(super) squelch_complex_sample_count: u64,
    pub(super) scan_window_complex_sample_count: u64,
    pub(super) feature_eligible_record_count: u64,
    pub(super) feature_insufficient_record_count: u64,
    pub(super) group_count: u64,
    pub(super) grouped_squelch_record_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFamilyCount {
    pub(super) group_count: u64,
    pub(super) record_count: u64,
    pub(super) complex_sample_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFrequencyBin {
    pub(super) frequency_mhz: u64,
    pub(super) group_incidence_count: [u64; 5],
    pub(super) record_count: [u64; 5],
    pub(super) complex_sample_count: [u64; 5],
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFeatureSummary {
    pub(super) observations: u64,
    pub(super) min: Option<f64>,
    pub(super) max: Option<f64>,
    pub(super) mean: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFeatureSummaries {
    pub(super) envelope_cv: BurstFeatureSummary,
    pub(super) usable_phase_differences: BurstFeatureSummary,
    pub(super) phase_concentration: BurstFeatureSummary,
    pub(super) two_level_separation_hz: BurstFeatureSummary,
    pub(super) two_level_variance_reduction: BurstFeatureSummary,
    pub(super) two_level_mode_contrast: BurstFeatureSummary,
    pub(super) two_level_cluster_balance: BurstFeatureSummary,
    pub(super) two_level_transitions: BurstFeatureSummary,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BurstFamilyMetrics {
    pub(super) summary_sha256: String,
    pub(super) source_bytes: u64,
    pub(super) source_sha256: String,
    pub(super) protocol_identity_established: bool,
    pub(super) settings: BurstFamilySettings,
    pub(super) accounting: BurstFamilyAccounting,
    pub(super) family_order: Vec<String>,
    pub(super) families: Vec<BurstFamilyCount>,
    pub(super) frequency_bins: Vec<BurstFrequencyBin>,
    pub(super) eligible_record_features: BurstFeatureSummaries,
}

#[derive(Debug)]
pub(super) struct CaptureStep {
    pub(super) state: StepState,
    pub(super) source: Option<SourceReport>,
    pub(super) termination: Option<Termination>,
    pub(super) diagnostics: Option<CaptureDiagnostics>,
    pub(super) detail: Option<String>,
}

#[derive(Debug)]
pub(super) struct AnalysisStep {
    pub(super) state: StepState,
    pub(super) compact: Option<CompactAnalysis>,
    pub(super) detail: Option<String>,
}

impl AnalysisStep {
    pub(super) fn not_started(detail: impl Into<String>) -> Self {
        Self {
            state: StepState::NotStarted,
            compact: None,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Attempt {
    pub(super) index: usize,
    pub(super) session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) center_frequency_hz: Option<u64>,
    pub(super) started_unix_ns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ended_unix_ns: Option<u64>,
    pub(super) start_offset_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) end_offset_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capture_ended_unix_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capture_end_offset_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis_started_unix_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis_start_offset_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis_ended_unix_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis_end_offset_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) phase: Option<AttemptPhase>,
    pub(super) capture_state: StepState,
    pub(super) analysis_state: StepState,
    pub(super) interrupted: bool,
    pub(super) retained: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) retention_priority: Option<RetentionPriority>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) retention_reasons: Vec<RetentionReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) source: Option<CompactSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capture_termination: Option<Termination>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capture_diagnostics: Option<CaptureDiagnostics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis: Option<CompactAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capture_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) analysis_detail: Option<String>,
}

impl Attempt {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(self.phase, None | Some(AttemptPhase::Finished))
    }

    pub(super) fn succeeded(&self) -> bool {
        self.capture_state == StepState::Complete
            && self.analysis_state == StepState::Complete
            && !self.interrupted
            && self
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.metrics.as_ref())
                .is_some()
    }

    pub(super) fn coded_shadow_matches_center(&self, center_frequency_hz: u64) -> bool {
        match self
            .analysis
            .as_ref()
            .and_then(|analysis| analysis.coded_shadow.as_ref())
        {
            Some(BluetoothCodedShadow::Complete { .. }) => center_frequency_hz == 2_435_000_000,
            Some(BluetoothCodedShadow::NotEligible {
                center_frequency_hz: observed,
            }) => center_frequency_hz != 2_435_000_000 && *observed == center_frequency_hz,
            None => false,
        }
    }

    pub(super) fn classify_retention(&mut self) {
        if !self.succeeded() {
            return;
        }
        let metrics = self
            .analysis
            .as_ref()
            .and_then(|analysis| analysis.metrics.as_ref())
            .expect("successful attempts have validated protocol metrics");
        self.retention_reasons = retention_reasons(metrics);
        if self
            .analysis
            .as_ref()
            .and_then(|analysis| analysis.coded_shadow.as_ref())
            .is_some_and(|shadow| {
                matches!(
                    shadow,
                    BluetoothCodedShadow::Complete { metrics }
                        if metrics.validated_coded_s8_packets > 0
                            || metrics.validated_coded_s2_packets > 0
                )
            })
        {
            self.retention_reasons
                .push(RetentionReason::BluetoothCodedValidatedPackets);
        }
        self.retention_priority = Some(if self.retention_reasons.is_empty() {
            RetentionPriority::Routine
        } else {
            RetentionPriority::Interesting
        });
    }
}
