use super::{
    BurstFamilyAccounting, BurstFamilyCount, BurstFamilyMetrics, BurstFamilySettings,
    BurstFeatureSummaries, BurstFeatureSummary, BurstFrequencyBin, CompactAnalysis, Context, Path,
    Result, SourceReport, Value, ensure, orchestrate, report, require_outcome, required_f64,
    terminal_outcome,
};

pub(in crate::watch) fn burst_feature_summary(value: &Value) -> Result<BurstFeatureSummary> {
    let optional = |field: &str| -> Result<Option<f64>> {
        match value.get(field) {
            Some(Value::Null) => Ok(None),
            Some(number) => {
                let number = number
                    .as_f64()
                    .with_context(|| format!("burst feature lacks number {field}"))?;
                ensure!(number.is_finite(), "burst feature {field} is not finite");
                Ok(Some(number))
            }
            None => anyhow::bail!("burst feature lacks field {field}"),
        }
    };
    Ok(BurstFeatureSummary {
        observations: report::required_u64(value, "observations")?,
        min: optional("min")?,
        max: optional("max")?,
        mean: optional("mean")?,
    })
}

pub(in crate::watch) fn burst_family_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
) -> Result<BurstFamilyMetrics> {
    let archive_path = output.join("bluetooth/bursts.bdb");
    let archive =
        report::hash_direct_regular_file(&archive_path, orchestrate::BURST_FAMILY_MAX_INPUT_BYTES)?;
    let bursts = report::read_json(&output.join("bluetooth/bursts.json"))?;
    let (summary, summary_sha256) =
        report::read_json_with_sha256(&output.join("burst-families/burst-families.json"))?;
    let outcome = orchestrate::burst_family_outcome(&summary, &bursts, &archive)?;
    require_outcome(
        outcome,
        terminal_outcome(terminal, "burst_families")?,
        "burst-family classifier",
    )?;

    let settings = summary.get("settings").context("missing burst settings")?;
    let quantile = settings
        .get("active_power_quantile")
        .and_then(Value::as_array)
        .context("missing active-power quantile")?;
    let separation = settings
        .get("separation_hz")
        .and_then(Value::as_array)
        .context("missing burst separation range")?;
    ensure!(
        quantile.len() == 2 && separation.len() == 2,
        "burst classifier range settings have different lengths"
    );
    let settings = BurstFamilySettings {
        channel_sample_rate_hz: report::required_u64(settings, "channel_sample_rate_hz")?,
        sample_period_ns: report::required_u64(settings, "sample_period_ns")?,
        group_connectivity_gap_ns: report::required_u64(settings, "group_connectivity_gap_ns")?,
        group_frequency_step_mhz: report::required_u64(settings, "group_frequency_step_mhz")?,
        simultaneous_interval: settings
            .get("simultaneous_interval")
            .and_then(Value::as_str)
            .context("missing simultaneous interval")?
            .to_owned(),
        active_power_quantile: [
            quantile[0].as_u64().context("invalid active quantile")?,
            quantile[1].as_u64().context("invalid active quantile")?,
        ],
        active_power_divisor: report::required_u64(settings, "active_power_divisor")?,
        minimum_usable_phase_differences: report::required_u64(
            settings,
            "minimum_usable_phase_differences",
        )?,
        maximum_envelope_cv: required_f64(settings, "maximum_envelope_cv")?,
        minimum_cluster_balance: required_f64(settings, "minimum_cluster_balance")?,
        minimum_variance_reduction: required_f64(settings, "minimum_variance_reduction")?,
        minimum_mode_contrast: required_f64(settings, "minimum_mode_contrast")?,
        mode_contrast_std_floor_hz: required_f64(settings, "mode_contrast_std_floor_hz")?,
        minimum_phase_concentration: required_f64(settings, "minimum_phase_concentration")?,
        separation_hz: [
            separation[0]
                .as_f64()
                .context("invalid separation minimum")?,
            separation[1]
                .as_f64()
                .context("invalid separation maximum")?,
        ],
        minimum_transitions: report::required_u64(settings, "minimum_transitions")?,
        kmeans_iterations: report::required_u64(settings, "kmeans_iterations")?,
        adjacent_multibin_minimum_width: report::required_u64(
            settings,
            "adjacent_multibin_minimum_width",
        )?,
        wideband_multibin_minimum_width: report::required_u64(
            settings,
            "wideband_multibin_minimum_width",
        )?,
    };

    let accounting = summary
        .get("accounting")
        .context("missing burst accounting")?;
    let accounting = BurstFamilyAccounting {
        archive_record_count: report::required_u64(accounting, "archive_record_count")?,
        squelch_record_count: report::required_u64(accounting, "squelch_record_count")?,
        scan_window_record_count: report::required_u64(accounting, "scan_window_record_count")?,
        archive_complex_sample_count: report::required_u64(
            accounting,
            "archive_complex_sample_count",
        )?,
        squelch_complex_sample_count: report::required_u64(
            accounting,
            "squelch_complex_sample_count",
        )?,
        scan_window_complex_sample_count: report::required_u64(
            accounting,
            "scan_window_complex_sample_count",
        )?,
        feature_eligible_record_count: report::required_u64(
            accounting,
            "feature_eligible_record_count",
        )?,
        feature_insufficient_record_count: report::required_u64(
            accounting,
            "feature_insufficient_record_count",
        )?,
        group_count: report::required_u64(accounting, "group_count")?,
        grouped_squelch_record_count: report::required_u64(
            accounting,
            "grouped_squelch_record_count",
        )?,
    };
    let families = summary
        .get("families")
        .and_then(Value::as_array)
        .context("missing burst families")?
        .iter()
        .map(|family| {
            Ok(BurstFamilyCount {
                group_count: report::required_u64(family, "group_count")?,
                record_count: report::required_u64(family, "record_count")?,
                complex_sample_count: report::required_u64(family, "complex_sample_count")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        source.sample_rate_hz.is_multiple_of(2),
        "burst-family source sample rate is not even"
    );
    let half_sample_rate_hz = source.sample_rate_hz / 2;
    let lowest_capture_frequency_hz = source
        .center_frequency_hz
        .checked_sub(half_sample_rate_hz)
        .context("burst-family source band underflows zero")?;
    let highest_capture_frequency_hz = source
        .center_frequency_hz
        .checked_add(half_sample_rate_hz)
        .context("burst-family source band overflows")?;
    let mut frequency_bins = vec![None; 79];
    for row in summary
        .get("frequency_family_counts")
        .and_then(Value::as_array)
        .context("missing burst frequency-family counts")?
    {
        let label = row
            .get("candidate_family")
            .and_then(Value::as_str)
            .context("missing frequency-family label")?;
        let family_index = orchestrate::BURST_FAMILY_LABELS
            .iter()
            .position(|expected| *expected == label)
            .context("unknown frequency-family label")?;
        let frequency_mhz = report::required_u64(row, "frequency_mhz")?;
        let frequency_index = usize::try_from(
            frequency_mhz
                .checked_sub(2402)
                .context("burst frequency is below 2402 MHz")?,
        )?;
        ensure!(
            frequency_index < frequency_bins.len(),
            "burst frequency is above 2480 MHz"
        );
        let frequency_hz = frequency_mhz
            .checked_mul(1_000_000)
            .context("burst frequency overflows hertz")?;
        ensure!(
            (lowest_capture_frequency_hz..highest_capture_frequency_hz).contains(&frequency_hz),
            "burst frequency lies outside the nominal capture band"
        );
        let bin = frequency_bins[frequency_index].get_or_insert(BurstFrequencyBin {
            frequency_mhz,
            group_incidence_count: [0; 5],
            record_count: [0; 5],
            complex_sample_count: [0; 5],
        });
        bin.group_incidence_count[family_index] =
            report::required_u64(row, "group_incidence_count")?;
        bin.record_count[family_index] = report::required_u64(row, "record_count")?;
        bin.complex_sample_count[family_index] = report::required_u64(row, "complex_sample_count")?;
    }
    let frequency_bins = frequency_bins.into_iter().flatten().collect();
    let features = summary
        .get("eligible_record_features")
        .context("missing burst features")?;
    let eligible_record_features = BurstFeatureSummaries {
        envelope_cv: burst_feature_summary(
            features.get("envelope_cv").context("missing envelope CV")?,
        )?,
        usable_phase_differences: burst_feature_summary(
            features
                .get("usable_phase_differences")
                .context("missing usable phase differences")?,
        )?,
        phase_concentration: burst_feature_summary(
            features
                .get("phase_concentration")
                .context("missing phase concentration")?,
        )?,
        two_level_separation_hz: burst_feature_summary(
            features
                .get("two_level_separation_hz")
                .context("missing two-level separation")?,
        )?,
        two_level_variance_reduction: burst_feature_summary(
            features
                .get("two_level_variance_reduction")
                .context("missing variance reduction")?,
        )?,
        two_level_mode_contrast: burst_feature_summary(
            features
                .get("two_level_mode_contrast")
                .context("missing mode contrast")?,
        )?,
        two_level_cluster_balance: burst_feature_summary(
            features
                .get("two_level_cluster_balance")
                .context("missing cluster balance")?,
        )?,
        two_level_transitions: burst_feature_summary(
            features
                .get("two_level_transitions")
                .context("missing transitions")?,
        )?,
    };
    let archive_after =
        report::hash_direct_regular_file(&archive_path, orchestrate::BURST_FAMILY_MAX_INPUT_BYTES)?;
    ensure!(
        archive_after == archive,
        "burst archive changed while extracting compact metrics"
    );
    Ok(BurstFamilyMetrics {
        summary_sha256,
        source_bytes: archive.bytes,
        source_sha256: archive.sha256,
        protocol_identity_established: false,
        settings,
        accounting,
        family_order: orchestrate::BURST_FAMILY_LABELS
            .into_iter()
            .map(str::to_owned)
            .collect(),
        families,
        frequency_bins,
        eligible_record_features,
    })
}
