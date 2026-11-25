use super::{
    CompactAnalysis, Context, IeeeChannelMetrics, IeeeHypothesisMetrics,
    IeeeMembershipPartitionMetrics, IeeeMetrics, Path, Result, SourceReport, Value, ensure,
    orchestrate, report, require_outcome, terminal_outcome,
};

pub(in crate::watch) fn ieee_hypothesis_metrics(
    root: &Path,
    channel: u8,
    spec: orchestrate::IeeeHypothesisSpec,
) -> Result<(report::FileDigest, Value, IeeeHypothesisMetrics)> {
    let archive_path = root.join(spec.archive_name(channel));
    let candidate_archive =
        report::hash_direct_regular_file(&archive_path, orchestrate::IEEE_ARCHIVE_MAX_BYTES)?;
    let summary_path = root
        .join(spec.inspection_name(channel))
        .join("summary.json");
    let (summary, inspection_summary_sha256) = report::read_json_with_sha256(&summary_path)?;
    orchestrate::validate_ieee_inspection(&summary, channel, &candidate_archive)?;
    let valid_fcs_records = report::required_u64(&summary, "valid_fcs_record_count")?;
    let validated_psdu_bytes = report::required_u64(&summary, "validated_psdu_bytes")?;
    ensure!(
        (valid_fcs_records == 0) == (validated_psdu_bytes == 0)
            && validated_psdu_bytes
                <= valid_fcs_records
                    .checked_mul(127)
                    .context("IEEE 802.15.4 validated PSDU byte bound overflow")?,
        "IEEE 802.15.4 validated PSDU extent differs from valid frame count"
    );
    let metrics = IeeeHypothesisMetrics {
        source_index: spec.source_index,
        decoder_lane: spec.decoder_lane,
        carrier_offset_hz: spec.carrier_offset_hz,
        source_role: spec.source_role,
        candidate_archive_bytes: candidate_archive.bytes,
        candidate_archive_sha256: candidate_archive.sha256.clone(),
        inspection_summary_sha256,
        candidate_records: report::required_u64(&summary, "candidate_record_count")?,
        valid_fcs_records,
        invalid_fcs_records: report::required_u64(&summary, "invalid_fcs_record_count")?,
        invalid_length_records: report::required_u64(&summary, "invalid_length_record_count")?,
        validated_psdu_bytes,
    };
    Ok((candidate_archive, summary, metrics))
}

pub(in crate::watch) fn ieee_membership_metrics(
    summary: &Value,
) -> Result<Vec<IeeeMembershipPartitionMetrics>> {
    summary
        .get("membership_partitions")
        .and_then(Value::as_array)
        .context("IEEE 802.15.4 comparison lacks membership partitions")?
        .iter()
        .map(|partition| {
            Ok(IeeeMembershipPartitionMetrics {
                source_membership_mask: u8::try_from(report::required_u64(
                    partition,
                    "source_membership_mask",
                )?)?,
                source_membership_count: u32::try_from(report::required_u64(
                    partition,
                    "source_membership_count",
                )?)?,
                identity_records: report::required_u64(partition, "identity_record_count")?,
                valid_fcs_records: report::required_u64(partition, "valid_fcs_record_count")?,
                invalid_fcs_records: report::required_u64(partition, "invalid_fcs_record_count")?,
                invalid_length_records: report::required_u64(
                    partition,
                    "invalid_length_record_count",
                )?,
            })
        })
        .collect()
}

pub(in crate::watch) fn ieee_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
) -> Result<IeeeMetrics> {
    let root = output.join("ieee802154");
    let channelized = root.join("channelized");
    let (channelize, channelize_summary_sha256) =
        report::read_json_with_sha256(&channelized.join("channelize.json"))?;
    let channelization = orchestrate::validate_channelize(&channelize, source, &channelized)?;
    let mut channels = Vec::with_capacity(3);
    let mut comparisons = Vec::with_capacity(3);
    for channel in [16u8, 17, 18] {
        let channelized_input = channelization.channel(channel)?.digest.clone();
        let mut archives = Vec::with_capacity(orchestrate::IEEE_HYPOTHESIS_COUNT);
        let mut inspections = Vec::with_capacity(orchestrate::IEEE_HYPOTHESIS_COUNT);
        let mut sources = Vec::with_capacity(orchestrate::IEEE_HYPOTHESIS_COUNT);
        for spec in orchestrate::IEEE_HYPOTHESES {
            let (archive, inspection, metrics) = ieee_hypothesis_metrics(&root, channel, spec)?;
            archives.push(archive);
            inspections.push(inspection);
            sources.push(metrics);
        }
        let comparison_path = root.join(format!("channel-{channel}-comparison/summary.json"));
        let (comparison, comparison_summary_sha256) =
            report::read_json_with_sha256(&comparison_path)?;
        orchestrate::validate_ieee_comparison(&comparison, channel, &archives, &inspections)?;
        for (spec, expected) in orchestrate::IEEE_HYPOTHESES.iter().zip(&archives) {
            let observed = report::hash_direct_regular_file(
                &root.join(spec.archive_name(channel)),
                orchestrate::IEEE_ARCHIVE_MAX_BYTES,
            )?;
            ensure!(
                &observed == expected,
                "IEEE 802.15.4 source {} archive changed while extracting compact metrics",
                spec.source_index
            );
        }
        let membership_partitions = ieee_membership_metrics(&comparison)?;
        comparisons.push(comparison.clone());
        channels.push(IeeeChannelMetrics {
            channel,
            channelized_input_bytes: channelized_input.bytes,
            channelized_input_sha256: channelized_input.sha256,
            sources,
            comparison_summary_sha256,
            source_candidate_records_sum: report::required_u64(
                &comparison,
                "source_candidate_record_count_sum",
            )?,
            source_valid_fcs_records_sum: report::required_u64(
                &comparison,
                "source_valid_fcs_record_count_sum",
            )?,
            source_invalid_fcs_records_sum: report::required_u64(
                &comparison,
                "source_invalid_fcs_record_count_sum",
            )?,
            source_invalid_length_records_sum: report::required_u64(
                &comparison,
                "source_invalid_length_record_count_sum",
            )?,
            identity_union_records: report::required_u64(
                &comparison,
                "identity_union_record_count",
            )?,
            identity_union_valid_fcs_records: report::required_u64(
                &comparison,
                "identity_union_valid_fcs_record_count",
            )?,
            identity_union_invalid_fcs_records: report::required_u64(
                &comparison,
                "identity_union_invalid_fcs_record_count",
            )?,
            identity_union_invalid_length_records: report::required_u64(
                &comparison,
                "identity_union_invalid_length_record_count",
            )?,
            duplicate_records: report::required_u64(&comparison, "duplicate_record_count")?,
            duplicate_valid_fcs_records: report::required_u64(
                &comparison,
                "duplicate_valid_fcs_record_count",
            )?,
            duplicate_invalid_fcs_records: report::required_u64(
                &comparison,
                "duplicate_invalid_fcs_record_count",
            )?,
            duplicate_invalid_length_records: report::required_u64(
                &comparison,
                "duplicate_invalid_length_record_count",
            )?,
            membership_partitions,
        });
    }
    let outcome = orchestrate::ieee_comparison_outcome(&comparisons)?;
    require_outcome(
        outcome,
        terminal_outcome(terminal, "ieee802154")?,
        "IEEE 802.15.4",
    )?;
    Ok(IeeeMetrics {
        channelize_summary_sha256,
        channels,
    })
}
