use super::{
    CompactAnalysis, Context, Path, Result, RfMetrics, RfRegionAggregate, SourceReport, Value,
    ensure, orchestrate, report, require_outcome, required_f64, terminal_outcome,
};

pub(in crate::watch) fn rf_metrics(
    output: &Path,
    source: &SourceReport,
    terminal: &CompactAnalysis,
    requested_windows: usize,
) -> Result<RfMetrics> {
    let (summary, summary_sha256) =
        report::read_json_with_sha256(&output.join("rf-survey/spectrum.json"))?;
    let outcome = orchestrate::rf_outcome(&summary, source)?;
    require_outcome(
        outcome,
        terminal_outcome(terminal, "rf_survey")?,
        "RF survey",
    )?;
    ensure!(
        report::required_u64(&summary, "fft_size")? == 8192,
        "RF survey FFT size differs from the reviewed contract"
    );
    let input_samples = report::required_u64(&summary, "input_samples")?;
    let inspected_prefix_samples = report::required_u64(&summary, "inspected_prefix_samples")?;
    let windows = report::required_u64(&summary, "windows")?;
    let expected_windows = u64::try_from(requested_windows)?.min(inspected_prefix_samples / 8192);
    ensure!(
        (1..=512).contains(&windows) && windows == expected_windows,
        "RF survey window count differs from the requested/full-window extent"
    );
    let bins = summary
        .get("bins")
        .and_then(Value::as_array)
        .context("RF survey lacks FFT bins")?;
    ensure!(bins.len() == 8192, "RF survey FFT bin count differs");
    let cells = summary
        .pointer("/activity_detection/cells")
        .and_then(Value::as_array)
        .context("RF survey lacks activity cells")?;
    ensure!(
        cells.len() == 160,
        "RF survey activity grid must contain 160 cells"
    );
    let activity = summary
        .get("activity_detection")
        .context("RF survey lacks activity detector metadata")?;
    ensure!(
        activity.get("method").and_then(Value::as_str)
            == Some("distributed_Hann_FFT_with_per_window_weighted_DC_removal_and_100_kHz_cells")
            && (required_f64(activity, "cell_width_hz")? - 100_000.0).abs() <= 0.001
            && (required_f64(activity, "usable_half_span_hz")? - 8_000_000.0).abs() <= 0.001
            && (required_f64(activity, "center_guard_hz")? - 100_000.0).abs() <= 0.001
            && (required_f64(activity, "threshold_db")? - 6.0).abs() <= 0.001
            && activity.get("temporal_baseline").and_then(Value::as_str) == Some("per_cell_median")
            && activity.get("frequency_baseline").and_then(Value::as_str)
                == Some("lower_quartile_of_usable_cell_means")
            && activity.get("region_peak_rule").and_then(Value::as_str)
                == Some(
                    "cell_center_maximizing_the_larger_of_mean_frequency_excess_and_peak_temporal_excess",
                ),
        "RF survey activity detector contract differs"
    );
    let grid_low = source.center_frequency_hz as f64 - 8_000_000.0;
    let grid_high = source.center_frequency_hz as f64 + 8_000_000.0;
    let threshold = required_f64(activity, "threshold_db")?;
    let mut excluded_cells = 0u64;
    let mut triggered = Vec::with_capacity(cells.len());
    let mut cell_metrics = Vec::with_capacity(cells.len());
    for (index, cell) in cells.iter().enumerate() {
        let expected_low = grid_low + index as f64 * 100_000.0;
        let expected_high = expected_low + 100_000.0;
        ensure!(
            (required_f64(cell, "start_frequency_hz")? - expected_low).abs() <= 0.001
                && (required_f64(cell, "end_frequency_hz")? - expected_high).abs() <= 0.001,
            "RF activity cell grid differs from the reviewed 100 kHz layout"
        );
        let excluded_value = cell
            .get("excluded_reason")
            .context("RF activity cell lacks exclusion state")?;
        let excluded = if excluded_value.is_null() {
            None
        } else {
            Some(
                excluded_value
                    .as_str()
                    .context("RF activity exclusion state is not a string or null")?,
            )
        };
        let should_be_excluded = matches!(index, 79 | 80);
        ensure!(
            (should_be_excluded && excluded == Some("center_guard"))
                || (!should_be_excluded && excluded.is_none()),
            "RF activity center-guard cell set differs"
        );
        excluded_cells += u64::from(should_be_excluded);
        let is_triggered = cell
            .get("activity_triggered")
            .and_then(Value::as_bool)
            .context("RF activity cell lacks trigger state")?;
        let mean_excess = required_f64(cell, "mean_excess_over_frequency_baseline_db")?;
        let transient_excess = required_f64(cell, "peak_excess_over_temporal_median_db")?;
        let windows_above = report::required_u64(cell, "windows_above_threshold")?;
        let hit_fraction = required_f64(cell, "sampled_window_hit_fraction")?;
        ensure!(
            windows_above <= windows
                && (hit_fraction - windows_above as f64 / windows as f64).abs() <= 1e-12
                && (0.0..=1.0).contains(&hit_fraction)
                && is_triggered
                    == (!should_be_excluded
                        && (mean_excess >= threshold || transient_excess >= threshold)),
            "RF activity cell trigger or hit fraction is inconsistent"
        );
        triggered.push(is_triggered);
        cell_metrics.push((
            mean_excess,
            transient_excess,
            hit_fraction,
            (expected_low + expected_high) / 2.0,
        ));
    }
    ensure!(
        excluded_cells == 2,
        "RF survey must exclude two center-guard cells"
    );
    let triggered_cells = u64::try_from(triggered.iter().filter(|value| **value).count())?;
    ensure!(
        triggered_cells <= 158,
        "RF triggered cell count exceeds usable cells"
    );
    let mut groups = Vec::new();
    let mut index = 0usize;
    while index < triggered.len() {
        if !triggered[index] {
            index += 1;
            continue;
        }
        let start = index;
        while index < triggered.len() && triggered[index] {
            index += 1;
        }
        groups.push((start, index));
    }
    let regions = summary
        .pointer("/activity_detection/regions")
        .and_then(Value::as_array)
        .context("RF survey lacks activity regions")?;
    ensure!(
        regions.len() == groups.len(),
        "RF survey region count differs from contiguous trigger groups"
    );
    let mut lowest = f64::INFINITY;
    let mut highest = f64::NEG_INFINITY;
    let mut maximum_mean = f64::NEG_INFINITY;
    let mut maximum_transient = f64::NEG_INFINITY;
    let mut maximum_hit = 0.0f64;
    let mut region_cell_sum = 0u64;
    for (region, (start, end)) in regions.iter().zip(groups) {
        ensure!(
            region.get("classification").and_then(Value::as_str)
                == Some("unclassified_spectral_activity"),
            "RF activity region has an unexpected classification"
        );
        let lower = required_f64(region, "lower_frequency_hz")?;
        let upper = required_f64(region, "upper_frequency_hz")?;
        let strongest = required_f64(region, "strongest_trigger_cell_frequency_hz")?;
        let mean = required_f64(region, "maximum_mean_excess_db")?;
        let transient = required_f64(region, "maximum_transient_excess_db")?;
        let hit = required_f64(region, "maximum_sampled_window_hit_fraction")?;
        let cells = report::required_u64(region, "cell_count")?;
        let selected = &cell_metrics[start..end];
        let expected_mean = selected
            .iter()
            .map(|cell| cell.0)
            .max_by(f64::total_cmp)
            .context("RF activity region is empty")?;
        let expected_transient = selected
            .iter()
            .map(|cell| cell.1)
            .max_by(f64::total_cmp)
            .context("RF activity region is empty")?;
        let expected_hit = selected
            .iter()
            .map(|cell| cell.2)
            .max_by(f64::total_cmp)
            .context("RF activity region is empty")?;
        let expected_strongest = selected
            .iter()
            .max_by(|left, right| left.0.max(left.1).total_cmp(&right.0.max(right.1)))
            .map(|cell| cell.3)
            .context("RF activity region is empty")?;
        let expected_lower = grid_low + start as f64 * 100_000.0;
        let expected_upper = grid_low + end as f64 * 100_000.0;
        ensure!(
            (lower - expected_lower).abs() <= 0.001
                && (upper - expected_upper).abs() <= 0.001
                && lower >= grid_low
                && upper <= grid_high
                && (strongest - expected_strongest).abs() <= 0.001
                && cells == u64::try_from(end - start)?
                && (upper - lower - cells as f64 * 100_000.0).abs() <= 0.001
                && (mean - expected_mean).abs() <= 1e-9
                && (transient - expected_transient).abs() <= 1e-9
                && (hit - expected_hit).abs() <= 1e-12,
            "RF activity region has inconsistent bounds or counts"
        );
        region_cell_sum = region_cell_sum
            .checked_add(cells)
            .context("RF activity region cell total overflow")?;
        lowest = lowest.min(lower);
        highest = highest.max(upper);
        maximum_mean = maximum_mean.max(mean);
        maximum_transient = maximum_transient.max(transient);
        maximum_hit = maximum_hit.max(hit);
    }
    ensure!(
        region_cell_sum == triggered_cells,
        "RF activity region cells differ from triggered cells"
    );
    let region_aggregate = (!regions.is_empty()).then_some(RfRegionAggregate {
        lowest_frequency_hz: lowest,
        highest_frequency_hz: highest,
        maximum_mean_excess_db: maximum_mean,
        maximum_transient_excess_db: maximum_transient,
        maximum_sampled_window_hit_fraction: maximum_hit,
    });
    Ok(RfMetrics {
        summary_sha256,
        input_samples,
        inspected_prefix_samples,
        windows,
        total_cells: 160,
        excluded_cells,
        triggered_cells,
        region_count: u64::try_from(regions.len())?,
        region_aggregate,
    })
}
