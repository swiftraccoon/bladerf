use super::deferred_diagnostics::{
    audit_deferred_shr_rate_score_counterfactual, audit_deferred_shr_rate_timing,
};
use super::*;
use futuresdr::runtime::mocker::{Mocker, Reader, Writer};

fn sample(amplitude: f32, index: usize) -> Complex32 {
    let phase = 0.071 * index as f32 + 0.23 * (index % 11) as f32;
    Complex32::new(amplitude * phase.cos(), amplitude * phase.sin())
}

fn synthetic_input(active_samples: usize, trailing_idle: usize) -> Vec<Complex32> {
    let mut input = (0..4_096)
        .map(|index| sample(1.0, index))
        .collect::<Vec<_>>();
    let active_start = input.len();
    input.extend((0..active_samples).map(|offset| sample(3.0, active_start + offset)));
    let trailing_start = input.len();
    input.extend((0..trailing_idle).map(|offset| sample(1.0, trailing_start + offset)));
    input
}

fn synthetic_shr_input() -> Vec<Complex32> {
    let mut input = vec![Complex32::new(1.0, 0.0); 4_096];
    let mut phase_iir = 0.0f32;
    let mut phase = 0.0f32;
    let metrics = shr_template(SHR_SYMBOLS)
        .into_iter()
        .flat_map(|chip| [0.4 * chip; 2])
        .chain(std::iter::repeat_n(0.0, 360));
    for metric in metrics {
        let phase_difference = phase_iir + metric / (1.0 - PHASE_IIR_ALPHA);
        phase_iir = (1.0 - PHASE_IIR_ALPHA) * phase_iir + PHASE_IIR_ALPHA * phase_difference;
        phase += phase_difference;
        input.push(Complex32::new(3.0 * phase.cos(), 3.0 * phase.sin()));
    }
    input.extend(std::iter::repeat_n(
        Complex32::new(phase.cos(), phase.sin()),
        256,
    ));
    input
}

fn synthetic_deferred_input(phr_length: u8) -> Vec<Complex32> {
    let mut input = vec![Complex32::new(1.0, 0.0); 4_096];
    let mut phase_iir = 0.0f32;
    let mut phase = 0.0f32;
    let metrics = deferred_template(phr_length)
        .into_iter()
        .flat_map(|chip| [0.4 * chip; 2])
        .chain(std::iter::repeat_n(0.0, 640));
    for metric in metrics {
        let phase_difference = phase_iir + metric / (1.0 - PHASE_IIR_ALPHA);
        phase_iir = (1.0 - PHASE_IIR_ALPHA) * phase_iir + PHASE_IIR_ALPHA * phase_difference;
        phase += phase_difference;
        input.push(Complex32::new(3.0 * phase.cos(), 3.0 * phase.sin()));
    }
    input.extend(std::iter::repeat_n(
        Complex32::new(phase.cos(), phase.sin()),
        256,
    ));
    input
}

fn synthetic_zero_tail_eof_input(post_selection_metrics: usize) -> Vec<Complex32> {
    assert!(post_selection_metrics <= MAX_METRIC_QUEUE);
    let mut metrics = synthetic_deferred_selection_metrics(63, 15, 100, 0.25, 127, true)
        .into_iter()
        .map(|metric| metric.value)
        .collect::<Vec<_>>();
    metrics[0] = 0.0;
    metrics.extend(
        [0.2, -0.1, 0.3, -0.2][..post_selection_metrics]
            .iter()
            .copied(),
    );

    let replay_start = 4_044usize;
    let active_start = 4_096usize;
    let mut input = vec![Complex32::new(1.0, 0.0); replay_start];
    let mut phase_iir = 0.0f32;
    let mut phase = 0.0f32;
    for (offset, metric) in metrics.into_iter().enumerate() {
        if offset != 0 {
            let phase_difference = phase_iir + metric / (1.0 - PHASE_IIR_ALPHA);
            phase_iir = (1.0 - PHASE_IIR_ALPHA) * phase_iir + PHASE_IIR_ALPHA * phase_difference;
            phase += phase_difference;
        }
        let amplitude = if replay_start + offset < active_start {
            1.0
        } else {
            3.0
        };
        input.push(Complex32::new(
            amplitude * phase.cos(),
            amplitude * phase.sin(),
        ));
    }
    input
}

fn drain_core(core: &mut BurstClockCore, output: &mut Vec<PositionedChip>) {
    while core.has_pending() {
        if let Some(chip) = core.take_ready() {
            output.push(chip);
        } else if core.timing_state == BurstTimingState::Selected && !core.shr_metrics.is_empty() {
            core.advance_metric_replay().unwrap();
        } else {
            core.advance_replay().unwrap();
        }
    }
}

fn run_core_chunks_with(
    mut core: BurstClockCore,
    input: &[Complex32],
    chunks: &[usize],
) -> (Vec<PositionedChip>, BurstClockStats) {
    let mut output = Vec::new();
    let mut cursor = 0;
    for chunk in chunks.iter().copied().chain(std::iter::once(input.len())) {
        let end = (cursor + chunk).min(input.len());
        for sample in &input[cursor..end] {
            drain_core(&mut core, &mut output);
            core.accept_source(*sample).unwrap();
            drain_core(&mut core, &mut output);
        }
        cursor = end;
        if cursor == input.len() {
            break;
        }
    }
    assert_eq!(cursor, input.len());
    core.finish().unwrap();
    assert_eq!(core.starts, core.ends);
    assert!(core.max_pretrigger_observed <= PRETRIGGER_CAPACITY);
    assert!(core.max_replay_observed <= PRETRIGGER_CAPACITY);
    assert!(core.clock.max_metric_queue_observed <= MAX_METRIC_QUEUE);
    let stats = core.stats();
    (output, stats)
}

fn run_core_chunks(input: &[Complex32], chunks: &[usize]) -> Vec<PositionedChip> {
    run_core_chunks_with(BurstClockCore::new(), input, chunks).0
}

fn run_mock_with_capacity(input: &[Complex32], capacity: usize) -> Vec<PositionedChip> {
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::new();
    let mut mock = Mocker::new(block);
    mock.input().set(input.to_vec());
    let mut output = Vec::new();
    loop {
        mock.output().reserve(capacity);
        mock.run();
        output.extend(mock.output().take().0);
        if mock.core.finished {
            break;
        }
    }
    output
}

fn run_shr_mock_with_capacity(
    input: &[Complex32],
    capacity: usize,
) -> (Vec<PositionedChip>, BurstClockStats) {
    let observer = Arc::new(Mutex::new(None));
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_shr_stats_observer(observer);
    let mut mock = Mocker::new(block);
    mock.input().set(input.to_vec());
    let mut output = Vec::new();
    loop {
        mock.output().reserve(capacity);
        mock.run();
        output.extend(mock.output().take().0);
        if mock.core.finished {
            break;
        }
    }
    (output, mock.core.stats())
}

fn run_shr_rate_mock_with_capacity(
    input: &[Complex32],
    capacity: usize,
) -> (Vec<PositionedChip>, ShrRateClockStats) {
    let observer = Arc::new(Mutex::new(None));
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_shr_rate_stats_observer(observer.clone());
    let mut mock = Mocker::new(block);
    mock.input().set(input.to_vec());
    let mut output = Vec::new();
    loop {
        mock.output().reserve(capacity);
        mock.run();
        output.extend(mock.output().take().0);
        if mock.core.finished {
            break;
        }
    }
    assert!(mock.stats_published);
    let core_stats = mock.core.rate_stats();
    let published = observer
        .lock()
        .unwrap()
        .take()
        .expect("joint SHR clock must publish one EOF statistics snapshot");
    assert_eq!(published, core_stats);
    mock.publish_stats().unwrap();
    assert!(observer.lock().unwrap().is_none());
    (output, published)
}

fn run_deferred_mock_with_capacity(
    input: &[Complex32],
    capacity: usize,
) -> (Vec<PositionedChip>, DeferredShrRateClockStats) {
    let observer = Arc::new(Mutex::new(None));
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_deferred_shr_rate_stats_observer(observer.clone());
    let mut mock = Mocker::new(block);
    mock.input().set(input.to_vec());
    let mut output = Vec::new();
    loop {
        mock.output().reserve(capacity);
        mock.run();
        output.extend(mock.output().take().0);
        if mock.core.finished {
            break;
        }
    }
    assert!(mock.stats_published);
    let core_stats = mock.core.deferred_stats();
    let published = observer
        .lock()
        .unwrap()
        .take()
        .expect("deferred clock must publish one EOF statistics snapshot");
    assert_eq!(published, core_stats);
    mock.publish_stats().unwrap();
    assert!(observer.lock().unwrap().is_none());
    (output, published)
}

fn run_shr_rate_core_chunks_with(
    mut core: BurstClockCore,
    input: &[Complex32],
    chunks: &[usize],
) -> (Vec<PositionedChip>, ShrRateClockStats) {
    let mut output = Vec::new();
    let mut cursor = 0;
    for chunk in chunks.iter().copied().chain(std::iter::once(input.len())) {
        let end = (cursor + chunk).min(input.len());
        for sample in &input[cursor..end] {
            drain_core(&mut core, &mut output);
            core.accept_source(*sample).unwrap();
            drain_core(&mut core, &mut output);
        }
        cursor = end;
        if cursor == input.len() {
            break;
        }
    }
    assert_eq!(cursor, input.len());
    core.finish().unwrap();
    assert_eq!(core.starts, core.ends);
    assert!(core.max_pretrigger_observed <= PRETRIGGER_CAPACITY);
    assert!(core.max_replay_observed <= PRETRIGGER_CAPACITY);
    assert!(core.clock.max_metric_queue_observed <= MAX_METRIC_QUEUE);
    (output, core.rate_stats())
}

fn run_deferred_core_chunks(
    input: &[Complex32],
    chunks: &[usize],
) -> (Vec<PositionedChip>, DeferredShrRateClockStats) {
    let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
    let mut output = Vec::new();
    let mut cursor = 0;
    for chunk in chunks.iter().copied().chain(std::iter::once(input.len())) {
        let end = (cursor + chunk).min(input.len());
        for sample in &input[cursor..end] {
            drain_core(&mut core, &mut output);
            core.accept_source(*sample).unwrap();
            drain_core(&mut core, &mut output);
        }
        cursor = end;
        if cursor == input.len() {
            break;
        }
    }
    assert_eq!(cursor, input.len());
    core.finish().unwrap();
    assert_eq!(core.starts, core.ends);
    assert!(core.max_pretrigger_observed <= PRETRIGGER_CAPACITY);
    assert!(core.max_replay_observed <= PRETRIGGER_CAPACITY);
    assert!(core.clock.max_metric_queue_observed <= MAX_METRIC_QUEUE);
    (output, core.deferred_stats())
}

fn run_shr_rate_diagnostic_core_chunks(
    input: &[Complex32],
    chunks: &[usize],
) -> (
    Vec<PositionedChip>,
    ShrRateClockStats,
    ShrRateDiagnosticTrace,
) {
    let mut core = BurstClockCore::with_shr_rate_diagnostic();
    let mut output = Vec::new();
    let mut cursor = 0;
    for chunk in chunks.iter().copied().chain(std::iter::once(input.len())) {
        let end = (cursor + chunk).min(input.len());
        for sample in &input[cursor..end] {
            drain_core(&mut core, &mut output);
            core.accept_source(*sample).unwrap();
            drain_core(&mut core, &mut output);
        }
        cursor = end;
        if cursor == input.len() {
            break;
        }
    }
    assert_eq!(cursor, input.len());
    core.finish().unwrap();
    let stats = core.rate_stats();
    let trace = core
        .diagnostic_trace()
        .expect("diagnostic core must retain one trace snapshot");
    (output, stats, trace)
}

fn run_shr_rate_diagnostic_mock_with_capacity(
    input: &[Complex32],
    capacity: usize,
) -> (
    Vec<PositionedChip>,
    ShrRateClockStats,
    ShrRateDiagnosticTrace,
) {
    let stats_observer = Arc::new(Mutex::new(None));
    let diagnostic_observer = Arc::new(Mutex::new(None));
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_shr_rate_diagnostic_observers(
            stats_observer.clone(),
            diagnostic_observer.clone(),
        );
    let mut mock = Mocker::new(block);
    mock.input().set(input.to_vec());
    let mut output = Vec::new();
    loop {
        mock.output().reserve(capacity);
        mock.run();
        output.extend(mock.output().take().0);
        if mock.core.finished {
            break;
        }
    }
    assert!(mock.stats_published);
    let stats = stats_observer
        .lock()
        .unwrap()
        .take()
        .expect("diagnostic clock must publish statistics");
    let trace = diagnostic_observer
        .lock()
        .unwrap()
        .take()
        .expect("diagnostic clock must publish its bounded trace");
    assert_eq!(stats, mock.core.rate_stats());
    assert_eq!(Some(trace.clone()), mock.core.diagnostic_trace());
    mock.publish_stats().unwrap();
    assert!(stats_observer.lock().unwrap().is_none());
    assert!(diagnostic_observer.lock().unwrap().is_none());
    (output, stats, trace)
}

fn reference_fresh_span(samples: &[Complex32], source_start: u64) -> Vec<PositionedChip> {
    let mut last_complex = Complex32::new(0.0, 0.0);
    let mut phase_iir = 0.0f32;
    let metrics = samples
        .iter()
        .enumerate()
        .map(|(offset, sample)| {
            let phase = (last_complex.conj() * sample).arg();
            last_complex = *sample;
            phase_iir = (1.0 - PHASE_IIR_ALPHA) * phase_iir + PHASE_IIR_ALPHA * phase;
            PositionedMetric {
                value: phase - phase_iir,
                source_sample_index: source_start + offset as u64,
            }
        })
        .collect::<Vec<_>>();

    let mut omega = CLOCK_OMEGA;
    let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    let mut mu = CLOCK_INITIAL_MU;
    let mut last_mm = 0.0f32;
    let mut input_index = 0;
    let mut output = Vec::new();
    while input_index + CLOCK_LOOK_AHEAD < metrics.len() {
        let left = metrics[input_index];
        let right = metrics[input_index + 1];
        let recovered = left.value + mu * (right.value - left.value);
        let mm = slice(last_mm) * recovered - slice(recovered) * last_mm;
        last_mm = recovered;
        omega += CLOCK_GAIN_OMEGA * mm;
        omega = CLOCK_OMEGA + (omega - CLOCK_OMEGA).clamp(-omega_limit, omega_limit);
        mu += omega + CLOCK_GAIN_MU * mm;
        let advance = mu.floor();
        mu -= advance;
        input_index += advance as usize;
        output.push(PositionedChip {
            metric: recovered,
            source_sample_index: left.source_sample_index,
            burst_start: output.is_empty(),
            payload_length: None,
        });
    }
    output
}

fn shr_template(symbols: [u8; SHR_SYMBOLS.len()]) -> [f32; SHR_CHIPS] {
    let mut chips = [0.0f32; SHR_CHIPS];
    for (symbol_index, symbol) in symbols.into_iter().enumerate() {
        let sequence = CHIP_SEQUENCES[usize::from(symbol)];
        for chip_index in 0..SYMBOL_CHIPS {
            let bit = 31 - chip_index;
            chips[symbol_index * SYMBOL_CHIPS + chip_index] = if sequence & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
        }
    }
    chips
}

fn synthetic_selection_metrics(
    start_offset: usize,
    mu_sixteenths: usize,
    scale: f32,
    symbols: [u8; SHR_SYMBOLS.len()],
    distinguish_phase: bool,
) -> Vec<PositionedMetric> {
    assert!(start_offset < SHR_START_HYPOTHESES);
    assert!(mu_sixteenths < SHR_PHASE_HYPOTHESES);
    assert!(scale.is_finite() && scale > 0.0);
    let mut values = (0..SHR_SELECTION_METRICS)
        .map(|index| {
            let pseudonoise = ((index * 29 + 7) % 31) as f32 - 15.0;
            0.07 * scale * pseudonoise
        })
        .collect::<Vec<_>>();
    let chips = shr_template(symbols);
    let mu = mu_sixteenths as f32 / SHR_PHASE_HYPOTHESES as f32;
    for (chip_index, chip) in chips.into_iter().enumerate() {
        let phase_noise = if distinguish_phase {
            let pseudonoise = ((chip_index * 43 + 11) % 67) as f32 - 33.0;
            0.11 * scale * pseudonoise
        } else {
            0.0
        };
        let expected = scale * chip;
        values[start_offset + 2 * chip_index] = expected - mu * phase_noise;
        values[start_offset + 2 * chip_index + 1] = expected + (1.0 - mu) * phase_noise;
    }
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| PositionedMetric {
            value,
            source_sample_index: 10_000 + index as u64,
        })
        .collect()
}

fn synthetic_rate_selection_metrics(
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    scale: f32,
    symbols: [u8; SHR_SYMBOLS.len()],
    distinguish_timing: bool,
) -> Vec<PositionedMetric> {
    assert!(start_offset < SHR_START_HYPOTHESES);
    assert!(mu_sixteenths < SHR_PHASE_HYPOTHESES);
    assert!(SHR_RATE_HYPOTHESES_PPM.contains(&rate_error_ppm));
    assert!(scale.is_finite() && scale > 0.0);
    let mut values = (0..SHR_RATE_SELECTION_METRICS)
        .map(|index| {
            let pseudonoise = ((index * 29 + 7) % 31) as f32 - 15.0;
            0.07 * scale * pseudonoise
        })
        .collect::<Vec<_>>();
    let mut assigned = vec![false; SHR_RATE_SELECTION_METRICS];
    for (chip_index, chip) in shr_template(symbols).into_iter().enumerate() {
        let (left_index, fraction) =
            shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
        let expected = scale * chip;
        let slope = if distinguish_timing {
            let pseudonoise = ((chip_index * 43 + 11) % 67) as f32 - 33.0;
            0.11 * scale * pseudonoise
        } else {
            0.0
        };
        if fraction == 0.0 {
            assert!(!assigned[left_index]);
            values[left_index] = expected;
            assigned[left_index] = true;
            if !assigned[left_index + 1] {
                values[left_index + 1] = expected + slope;
                assigned[left_index + 1] = true;
            }
        } else if assigned[left_index] {
            assert!(!assigned[left_index + 1]);
            values[left_index + 1] = (expected - (1.0 - fraction) * values[left_index]) / fraction;
            assigned[left_index + 1] = true;
        } else {
            assert!(!assigned[left_index + 1]);
            values[left_index] = expected - fraction * slope;
            values[left_index + 1] = expected + (1.0 - fraction) * slope;
            assigned[left_index] = true;
            assigned[left_index + 1] = true;
        }
    }
    let metrics = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| PositionedMetric {
            value,
            source_sample_index: 20_000 + index as u64,
        })
        .collect::<Vec<_>>();
    for (chip_index, expected) in shr_template(symbols).into_iter().enumerate() {
        let actual = interpolated_shr_rate_chip(
            &metrics,
            start_offset,
            mu_sixteenths,
            rate_error_ppm,
            chip_index,
        );
        assert!((actual - scale * expected).abs() < 1.0e-4 * scale.max(1.0));
    }
    metrics
}

fn deferred_template(phr_length: u8) -> [f32; DEFERRED_STRUCTURAL_CHIPS] {
    let symbols = SHR_SYMBOLS
        .into_iter()
        .chain([phr_length & 0x0f, phr_length >> 4]);
    let mut chips = [0.0; DEFERRED_STRUCTURAL_CHIPS];
    for (symbol_index, symbol) in symbols.enumerate() {
        let sequence = CHIP_SEQUENCES[usize::from(symbol)];
        for chip_index in 0..SYMBOL_CHIPS {
            let bit = 31 - chip_index;
            chips[symbol_index * SYMBOL_CHIPS + chip_index] = if sequence & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
        }
    }
    chips
}

fn below_cap_deferred_template(phr_length: u8) -> [f32; DEFERRED_STRUCTURAL_CHIPS] {
    let mut chips = deferred_template(phr_length);
    for (symbol_index, expected_symbol) in SHR_SYMBOLS.into_iter().enumerate() {
        let expected = CHIP_SEQUENCES[usize::from(expected_symbol)];
        let competitor = CHIP_SEQUENCES[usize::from((expected_symbol + 1) % 16)];
        let start = symbol_index * SYMBOL_CHIPS;
        for chip_index in 0..SYMBOL_CHIPS {
            let bit = 31 - chip_index;
            let expected_chip = if expected & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
            let competitor_chip = if competitor & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
            chips[start + chip_index] = expected_chip + 0.99 * competitor_chip;
        }
        let window: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
            .try_into()
            .expect("weakened SHR window has fixed length");
        assert_eq!(classify_payload_symbol(window), Some(expected_symbol));
    }
    let weakened_shr: &[f32; SHR_CHIPS] = chips[..SHR_CHIPS]
        .try_into()
        .expect("weakened SHR has fixed length");
    assert!(full_shr_correlation(weakened_shr).unwrap() <= UNIQUE_ANGULAR_CAP);
    chips
}

fn replace_deferred_symbol(
    chips: &mut [f32; DEFERRED_STRUCTURAL_CHIPS],
    symbol_index: usize,
    symbol: u8,
) {
    let sequence = CHIP_SEQUENCES[usize::from(symbol)];
    for chip_index in 0..SYMBOL_CHIPS {
        let bit = 31 - chip_index;
        chips[symbol_index * SYMBOL_CHIPS + chip_index] = if sequence & (1 << bit) == 0 {
            -1.0
        } else {
            1.0
        };
    }
}

fn synthetic_deferred_selection_metrics_from_chips(
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    scale: f32,
    expected_chips: &[f32; DEFERRED_STRUCTURAL_CHIPS],
    distinguish_timing: bool,
) -> Vec<PositionedMetric> {
    assert!(start_offset < SHR_START_HYPOTHESES);
    assert!(mu_sixteenths < SHR_PHASE_HYPOTHESES);
    assert!(SHR_RATE_HYPOTHESES_PPM.contains(&rate_error_ppm));
    assert!(scale.is_finite() && scale > 0.0);
    let mut values = (0..DEFERRED_SELECTION_METRICS)
        .map(|index| {
            let pseudonoise = ((index * 29 + 7) % 31) as f32 - 15.0;
            0.07 * scale * pseudonoise
        })
        .collect::<Vec<_>>();
    let mut assigned = vec![false; DEFERRED_SELECTION_METRICS];
    for (chip_index, chip) in expected_chips.iter().copied().enumerate() {
        let (left_index, fraction) =
            shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
        let expected = scale * chip;
        let slope = if distinguish_timing {
            let pseudonoise = ((chip_index * 43 + 11) % 67) as f32 - 33.0;
            0.11 * scale * pseudonoise
        } else {
            0.0
        };
        if fraction == 0.0 {
            assert!(!assigned[left_index]);
            values[left_index] = expected;
            assigned[left_index] = true;
            if !assigned[left_index + 1] {
                values[left_index + 1] = expected + slope;
                assigned[left_index + 1] = true;
            }
        } else if assigned[left_index] {
            assert!(!assigned[left_index + 1]);
            values[left_index + 1] = (expected - (1.0 - fraction) * values[left_index]) / fraction;
            assigned[left_index + 1] = true;
        } else {
            assert!(!assigned[left_index + 1]);
            values[left_index] = expected - fraction * slope;
            values[left_index + 1] = expected + (1.0 - fraction) * slope;
            assigned[left_index] = true;
            assigned[left_index + 1] = true;
        }
    }
    let metrics = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| PositionedMetric {
            value,
            source_sample_index: 30_000 + index as u64,
        })
        .collect::<Vec<_>>();
    for (chip_index, expected) in expected_chips.iter().copied().enumerate() {
        let actual = interpolated_shr_rate_chip(
            &metrics,
            start_offset,
            mu_sixteenths,
            rate_error_ppm,
            chip_index,
        );
        assert!((actual - scale * expected).abs() < 1.0e-4 * scale.max(1.0));
    }
    metrics
}

fn synthetic_deferred_selection_metrics(
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    scale: f32,
    phr_length: u8,
    distinguish_timing: bool,
) -> Vec<PositionedMetric> {
    synthetic_deferred_selection_metrics_from_chips(
        start_offset,
        mu_sixteenths,
        rate_error_ppm,
        scale,
        &deferred_template(phr_length),
        distinguish_timing,
    )
}

fn exact_selection(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
) -> ScoredShrTimingSelection {
    let selected = select_shr_timing(metrics)
        .expect("select synthetic SHR timing")
        .expect("synthetic SHR must lock");
    assert_eq!(selected.selection.start_offset, start_offset as u8);
    assert_eq!(selected.selection.mu_sixteenths, mu_sixteenths as u8);
    assert_eq!(
        selected.selection.source_sample_index,
        metrics[start_offset].source_sample_index
    );
    selected
}

fn exact_rate_selection(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
) -> ScoredShrRateTimingSelection {
    let selected = select_shr_rate_timing(metrics)
        .expect("select synthetic SHR rate timing")
        .expect("synthetic rate-adjusted SHR must lock");
    assert_eq!(selected.selection.timing.start_offset, start_offset as u8);
    assert_eq!(selected.selection.timing.mu_sixteenths, mu_sixteenths as u8);
    assert_eq!(selected.selection.rate_error_ppm, rate_error_ppm);
    assert_eq!(
        selected.selection.timing.source_sample_index,
        metrics[start_offset].source_sample_index
    );
    selected
}

fn exact_deferred_selection(
    metrics: &[PositionedMetric],
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    phr_length: u8,
) -> ScoredDeferredShrRateTimingSelection {
    let selected = select_deferred_shr_rate_timing(metrics)
        .expect("select synthetic deferred SHR/PHR timing")
        .expect("synthetic deferred SHR/PHR input must lock");
    assert_eq!(
        selected.handoff.selection.timing.start_offset,
        start_offset as u8
    );
    assert_eq!(
        selected.handoff.selection.timing.mu_sixteenths,
        mu_sixteenths as u8
    );
    assert_eq!(selected.handoff.selection.rate_error_ppm, rate_error_ppm);
    assert_eq!(
        selected.handoff.selection.timing.source_sample_index,
        metrics[start_offset].source_sample_index
    );
    assert_eq!(selected.handoff.phr_length, phr_length);
    let (payload_left_offset, payload_mu) = shr_rate_position(
        start_offset,
        mu_sixteenths,
        rate_error_ppm,
        DEFERRED_PAYLOAD_CHIP_INDEX,
    );
    assert_eq!(
        usize::from(selected.handoff.payload_left_offset),
        payload_left_offset
    );
    assert_eq!(selected.handoff.payload_mu_bits, payload_mu.to_bits());
    assert_eq!(
        selected.handoff.payload_source_sample_index,
        metrics[0].source_sample_index + payload_left_offset as u64
    );
    let prior = interpolated_shr_rate_chip(
        metrics,
        start_offset,
        mu_sixteenths,
        rate_error_ppm,
        DEFERRED_STRUCTURAL_CHIPS - 1,
    );
    assert_eq!(selected.handoff.prior_metric_bits, prior.to_bits());
    selected
}

fn independent_clock_replay(
    metrics: &[PositionedMetric],
    initial_omega: f32,
    initial_mu: f32,
) -> Vec<PositionedChip> {
    let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    let mut omega = initial_omega;
    let mut mu = initial_mu;
    let mut last_mm = 0.0f32;
    let mut input_index = 0usize;
    let mut output = Vec::new();
    while input_index + CLOCK_LOOK_AHEAD < metrics.len() {
        let left = metrics[input_index];
        let right = metrics[input_index + 1];
        let recovered = left.value + mu * (right.value - left.value);
        let mm = slice(last_mm) * recovered - slice(recovered) * last_mm;
        last_mm = recovered;
        omega += CLOCK_GAIN_OMEGA * mm;
        omega = CLOCK_OMEGA + (omega - CLOCK_OMEGA).clamp(-omega_limit, omega_limit);
        mu += omega + CLOCK_GAIN_MU * mm;
        let advance = mu.floor();
        mu -= advance;
        input_index += advance as usize;
        output.push(PositionedChip {
            metric: recovered,
            source_sample_index: left.source_sample_index,
            burst_start: output.is_empty(),
            payload_length: None,
        });
    }
    output
}

fn independent_deferred_handoff_replay(
    metrics: &[PositionedMetric],
    initial_omega: f32,
    initial_mu: f32,
    prior_metric: f32,
    payload_length: u8,
) -> Vec<PositionedChip> {
    let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    let mut omega = initial_omega;
    let mut mu = initial_mu;
    let mut last_mm = prior_metric;
    let mut input_index = 0usize;
    let mut output = Vec::new();
    while input_index + CLOCK_LOOK_AHEAD < metrics.len() {
        let left = metrics[input_index];
        let right = metrics[input_index + 1];
        let recovered = left.value + mu * (right.value - left.value);
        let mm = slice(last_mm) * recovered - slice(recovered) * last_mm;
        last_mm = recovered;
        omega += CLOCK_GAIN_OMEGA * mm;
        omega = CLOCK_OMEGA + (omega - CLOCK_OMEGA).clamp(-omega_limit, omega_limit);
        mu += omega + CLOCK_GAIN_MU * mm;
        let advance = mu.floor();
        mu -= advance;
        input_index += advance as usize;
        output.push(PositionedChip {
            metric: recovered,
            source_sample_index: left.source_sample_index,
            burst_start: output.is_empty(),
            payload_length: output.is_empty().then_some(payload_length),
        });
    }
    output
}

#[test]
fn candidate_constants_and_storage_bounds_are_frozen() {
    assert_eq!(PRETRIGGER_CAPACITY, 64);
    assert_eq!(PHASE_IIR_ALPHA, 0.00016);
    assert_eq!(CLOCK_OMEGA, 2.0);
    assert_eq!(CLOCK_GAIN_OMEGA, 0.000225);
    assert_eq!(CLOCK_INITIAL_MU, 0.5);
    assert_eq!(CLOCK_GAIN_MU, 0.03);
    assert_eq!(CLOCK_RELATIVE_LIMIT, 0.0002);
    assert_eq!(CLOCK_LOOK_AHEAD, 3);
    assert_eq!(MAX_METRIC_QUEUE, 4);
    assert_eq!(SHR_SYMBOLS, [0, 0, 0, 0, 0, 0, 0, 0, 7, 10]);
    assert_eq!(SHR_SUFFIX_START_SYMBOL, 4);
    assert_eq!(SHR_CHIPS, 320);
    assert_eq!(SHR_START_HYPOTHESES, 64);
    assert_eq!(SHR_PHASE_HYPOTHESES, 16);
    assert_eq!(SHR_SELECTION_METRICS, 703);
    assert_eq!(SHR_RATE_HYPOTHESES_PPM, [-100, 0, 100]);
    assert_eq!(SHR_RATE_PPM_DENOMINATOR, 1_000_000);
    assert_eq!(SHR_RATE_POSITION_DENOMINATOR, 16_000_000);
    assert_eq!(SHR_RATE_SELECTION_METRICS, 704);
    assert_eq!(
        SHR_START_HYPOTHESES * SHR_PHASE_HYPOTHESES * SHR_RATE_HYPOTHESES_PPM.len(),
        3_072
    );
    assert_eq!(MAX_SELECTION_RECORDS, 2);
    assert_eq!(
        (CLOCK_OMEGA + CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT + CLOCK_GAIN_MU).ceil() as usize,
        CLOCK_LOOK_AHEAD
    );

    let established = BurstClockCore::new().stats();
    assert_eq!(established.shr_locks, 0);
    assert_eq!(established.shr_rejections, 0);
    assert_eq!(established.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(established.shr_selection_overflow, 0);
    assert_eq!(established.max_shr_selection_metrics, 0);
    assert_eq!(established.max_shr_metric_replay, 0);
    assert_eq!(
        BurstClockCore::new().rate_stats().shr_rate_selections,
        [None; MAX_SELECTION_RECORDS]
    );
    assert_eq!(
        BurstClockCore::with_shr_initializer()
            .rate_stats()
            .shr_rate_selections,
        [None; MAX_SELECTION_RECORDS]
    );
    assert_eq!(
        BurstClockCore::with_deferred_shr_rate_initializer()
            .deferred_stats()
            .deferred_selections,
        [None; MAX_SELECTION_RECORDS]
    );
    for established in [
        BurstClockCore::new(),
        BurstClockCore::with_shr_initializer(),
        BurstClockCore::with_shr_rate_initializer(),
    ] {
        assert_eq!(
            established.shr_metrics.capacity(),
            SHR_RATE_SELECTION_METRICS
        );
    }
    assert_eq!(
        BurstClockCore::with_deferred_shr_rate_initializer()
            .shr_metrics
            .capacity(),
        DEFERRED_SELECTION_METRICS
    );

    let observer = Arc::new(Mutex::new(None));
    let block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_shr_stats_observer(observer);
    assert_eq!(block.core.timing_mode, TimingMode::ShrInitializer);
    let rate_observer = Arc::new(Mutex::new(None));
    let rate_block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_shr_rate_stats_observer(rate_observer);
    assert_eq!(rate_block.core.timing_mode, TimingMode::ShrRateInitializer);
    let deferred_observer = Arc::new(Mutex::new(None));
    let deferred_block: BurstClockRecovery<Reader<Complex32>, Writer<PositionedChip>> =
        BurstClockRecovery::with_deferred_shr_rate_stats_observer(deferred_observer);
    assert_eq!(
        deferred_block.core.timing_mode,
        TimingMode::DeferredShrRateInitializer
    );
}

#[test]
fn shr_rate_coordinate_grid_and_extreme_endpoint_are_exact_and_bounded() {
    let mut hypotheses = 0usize;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                for chip_index in 0..SHR_CHIPS {
                    let (left_index, fraction) =
                        shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
                    assert!(left_index + 1 < SHR_RATE_SELECTION_METRICS);
                    assert!((0.0..1.0).contains(&fraction));
                }
                hypotheses += 1;
            }
        }
    }
    assert_eq!(hypotheses, 3_072);

    let (left_index, fraction) = shr_rate_position(63, 15, 100, 319);
    assert_eq!(left_index, 702);
    assert_eq!(left_index + 1, 703);
    assert!((fraction - 0.0013).abs() < 1.0e-7);

    let negative = shr_rate_clock_omega(-100);
    let nominal = shr_rate_clock_omega(0);
    let positive = shr_rate_clock_omega(100);
    assert!(negative < nominal && nominal < positive);
    assert!((negative - 1.9998).abs() < 1.0e-6);
    assert_eq!(nominal, CLOCK_OMEGA);
    assert!((positive - 2.0002).abs() < 1.0e-6);
    let limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    assert!((CLOCK_OMEGA - limit..=CLOCK_OMEGA + limit).contains(&negative));
    assert!((CLOCK_OMEGA - limit..=CLOCK_OMEGA + limit).contains(&positive));
}

#[test]
fn deferred_coordinate_grid_and_handoff_bounds_are_exact() {
    assert_eq!(DEFERRED_STRUCTURAL_SYMBOLS, 12);
    assert_eq!(DEFERRED_STRUCTURAL_CHIPS, 384);
    assert_eq!(DEFERRED_SELECTION_METRICS, 832);
    assert_eq!(DEFERRED_PAYLOAD_CHIP_INDEX, 384);
    assert_eq!(MIN_DEFERRED_PAYLOAD_LEFT_OFFSET, 767);
    assert_eq!(MAX_DEFERRED_HANDOFF_TAIL_METRICS, 65);
    assert_eq!(DEFERRED_SELECTION_METRICS + MAX_METRIC_QUEUE, 836);

    let mut hypotheses = 0usize;
    let mut minimum_payload_left = usize::MAX;
    let mut maximum_payload_left = 0usize;
    for start_offset in 0..SHR_START_HYPOTHESES {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                for chip_index in 0..DEFERRED_STRUCTURAL_CHIPS {
                    let (left_index, fraction) =
                        shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
                    assert!(left_index + 1 < DEFERRED_SELECTION_METRICS);
                    assert!((0.0..1.0).contains(&fraction));
                }
                let (payload_left, payload_mu) = shr_rate_position(
                    start_offset,
                    mu_sixteenths,
                    rate_error_ppm,
                    DEFERRED_PAYLOAD_CHIP_INDEX,
                );
                assert!(payload_left <= DEFERRED_SELECTION_METRICS);
                assert!((0.0..1.0).contains(&payload_mu));
                minimum_payload_left = minimum_payload_left.min(payload_left);
                maximum_payload_left = maximum_payload_left.max(payload_left);
                hypotheses += 1;
            }
        }
    }
    assert_eq!(hypotheses, SHR_RATE_HYPOTHESIS_COUNT as usize);
    assert_eq!(minimum_payload_left, MIN_DEFERRED_PAYLOAD_LEFT_OFFSET);
    assert_eq!(maximum_payload_left, DEFERRED_SELECTION_METRICS);
    assert_eq!(
        DEFERRED_SELECTION_METRICS - minimum_payload_left,
        MAX_DEFERRED_HANDOFF_TAIL_METRICS
    );

    let (last_left, last_mu) = shr_rate_position(63, 15, 100, 383);
    assert_eq!(last_left, 830);
    assert_eq!(last_left + 1, 831);
    assert_eq!(last_mu.to_bits(), 0x3c67_03b0);
    let (minimum_left, minimum_mu) = shr_rate_position(0, 0, -100, 384);
    assert_eq!(minimum_left, 767);
    assert_eq!(minimum_mu.to_bits(), 0x3f6c_56d6);
    let (maximum_left, maximum_mu) = shr_rate_position(63, 15, 100, 384);
    assert_eq!(maximum_left, 832);
    assert_eq!(maximum_mu.to_bits(), 0x3c6a_4a8c);
}

#[test]
fn deferred_structure_requires_exact_shr_and_bounded_phr() {
    for phr_length in [1, 127] {
        let chips = deferred_template(phr_length);
        assert!(matches!(
            deferred_structure(&chips),
            DeferredStructureOutcome::Eligible {
                phr_length: observed,
                score,
            } if observed == phr_length && score > UNIQUE_ANGULAR_CAP
        ));
    }
    assert!(matches!(
        deferred_structure(&deferred_template(0)),
        DeferredStructureOutcome::LengthZero { .. }
    ));
    for phr_length in [128, 255] {
        assert!(matches!(
            deferred_structure(&deferred_template(phr_length)),
            DeferredStructureOutcome::LengthOversize { .. }
        ));
    }

    assert_eq!(
        deferred_structure(&[0.0; DEFERRED_STRUCTURAL_CHIPS]),
        DeferredStructureOutcome::CorrelationUndefined
    );

    let below_cap = below_cap_deferred_template(5);
    assert!(matches!(
        deferred_structure(&below_cap),
        DeferredStructureOutcome::ScoreAtOrBelowCap { .. }
    ));

    for (symbol_index, expected_symbol) in SHR_SYMBOLS.into_iter().enumerate() {
        let mut chips = deferred_template(5);
        replace_deferred_symbol(&mut chips, symbol_index, (expected_symbol + 1) % 16);
        assert!(
            matches!(
                deferred_structure(&chips),
                DeferredStructureOutcome::ShrWrong {
                    symbol_index: observed,
                    ..
                } if observed == symbol_index
            ),
            "wrong SHR symbol at index {symbol_index} reached the wrong stage"
        );
    }

    for tied_symbol_index in [0, SHR_SYMBOLS.len(), SHR_SYMBOLS.len() + 1] {
        let mut chips = deferred_template(5);
        let zero = CHIP_SEQUENCES[0];
        let one = CHIP_SEQUENCES[1];
        let start = tied_symbol_index * SYMBOL_CHIPS;
        for chip_index in 0..SYMBOL_CHIPS {
            let bit = 31 - chip_index;
            let zero_chip = if zero & (1 << bit) == 0 { -1.0 } else { 1.0 };
            let one_chip = if one & (1 << bit) == 0 { -1.0 } else { 1.0 };
            chips[start + chip_index] = zero_chip + one_chip;
        }
        let tied: &[f32; SYMBOL_CHIPS] = chips[start..start + SYMBOL_CHIPS]
            .try_into()
            .expect("tied symbol window has fixed length");
        assert_eq!(classify_payload_symbol(tied), None);
        let outcome = deferred_structure(&chips);
        match tied_symbol_index {
            0 => assert!(matches!(
                outcome,
                DeferredStructureOutcome::ShrUnclassified {
                    symbol_index: 0,
                    ..
                }
            )),
            index if index == SHR_SYMBOLS.len() => assert!(matches!(
                outcome,
                DeferredStructureOutcome::PhrLowUnclassified { .. }
            )),
            _ => assert!(matches!(
                outcome,
                DeferredStructureOutcome::PhrHighUnclassified { .. }
            )),
        }
    }

    let mut nonfinite = deferred_template(5);
    nonfinite[SHR_CHIPS] = f32::NAN;
    assert_eq!(
        deferred_structure(&nonfinite),
        DeferredStructureOutcome::InterpolationNonfinite
    );
}

#[test]
fn deferred_score_counterfactual_reuses_later_predicate_order() {
    for phr_length in [1, 127] {
        let chips = below_cap_deferred_template(phr_length);
        let DeferredStructureOutcome::ScoreAtOrBelowCap { score } = deferred_structure(&chips)
        else {
            panic!("weakened deferred structure must stop at the score cap");
        };
        assert!(matches!(
            deferred_structure_after_score(&chips, score),
            DeferredStructureOutcome::Eligible {
                phr_length: observed,
                score: observed_score,
            } if observed == phr_length && observed_score.to_bits() == score.to_bits()
        ));
    }

    let zero = below_cap_deferred_template(0);
    let DeferredStructureOutcome::ScoreAtOrBelowCap { score } = deferred_structure(&zero) else {
        panic!("zero-length deferred structure must stop at the score cap");
    };
    assert_eq!(
        deferred_structure_after_score(&zero, score),
        DeferredStructureOutcome::LengthZero { score }
    );

    let oversize = below_cap_deferred_template(128);
    let DeferredStructureOutcome::ScoreAtOrBelowCap { score } = deferred_structure(&oversize)
    else {
        panic!("oversize deferred structure must stop at the score cap");
    };
    assert_eq!(
        deferred_structure_after_score(&oversize, score),
        DeferredStructureOutcome::LengthOversize { score }
    );
}

#[test]
fn deferred_score_counterfactual_audit_is_bounded_sanitized_and_validated() {
    const EXPECTED_COUNTERFACTUAL: DeferredShrRateScoreCounterfactualAudit =
        DeferredShrRateScoreCounterfactualAudit {
            evaluated_hypotheses: 3_072,
            score_rejected_hypotheses: 3_072,
            first_failures: DeferredShrRatePostScoreFirstFailureHistogram {
                shr_unclassified: [198, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                shr_wrong: [2_734, 1, 0, 0, 1, 0, 0, 56, 29, 0],
                phr_low_unclassified: 0,
                phr_high_unclassified: 0,
                length_zero: 0,
                length_oversize: 0,
                eligible: 53,
            },
            best_structurally_eligible: Some(DeferredShrRateCandidateSummary {
                selection: ShrRateTimingSelection {
                    timing: ShrTimingSelection {
                        start_offset: 0,
                        mu_sixteenths: 0,
                        source_sample_index: 30_000,
                    },
                    rate_error_ppm: -100,
                },
                full_score_bits: 0x3fe7_54d7_6e64_da80,
            }),
        };
    let below_cap = below_cap_deferred_template(5);
    let metrics = synthetic_deferred_selection_metrics_from_chips(0, 0, 0, 1.0, &below_cap, false);
    let authoritative = audit_deferred_shr_rate_timing(&metrics).unwrap();
    let counterfactual = audit_deferred_shr_rate_score_counterfactual(&metrics).unwrap();

    assert_eq!(
        counterfactual.evaluated_hypotheses,
        SHR_RATE_HYPOTHESIS_COUNT
    );
    assert_eq!(
        counterfactual.evaluated_hypotheses,
        authoritative.evaluated_hypotheses
    );
    assert_eq!(
        counterfactual.score_rejected_hypotheses,
        authoritative.first_failures.score_at_or_below_cap
    );
    assert_eq!(
        counterfactual.first_failures.checked_total(),
        Some(counterfactual.score_rejected_hypotheses)
    );
    assert!(authoritative.first_failures.score_at_or_below_cap > 0);
    assert!(counterfactual.first_failures.eligible > 1);
    let best = counterfactual
        .best_structurally_eligible
        .expect("repeated below-cap timing hypotheses must retain one exact best");
    assert!(f64::from_bits(best.full_score_bits) <= UNIQUE_ANGULAR_CAP);
    assert!(usize::from(best.selection.timing.start_offset) < SHR_START_HYPOTHESES);
    assert!(usize::from(best.selection.timing.mu_sixteenths) < SHR_PHASE_HYPOTHESES);
    assert!(SHR_RATE_HYPOTHESES_PPM.contains(&best.selection.rate_error_ppm));
    assert_eq!(
        best.selection.timing.source_sample_index,
        metrics[usize::from(best.selection.timing.start_offset)].source_sample_index
    );

    assert!(
        audit_deferred_shr_rate_score_counterfactual(&metrics[..DEFERRED_SELECTION_METRICS - 1])
            .is_err()
    );
    let mut nonfinite = metrics.clone();
    nonfinite[400].value = f32::INFINITY;
    assert!(audit_deferred_shr_rate_score_counterfactual(&nonfinite).is_err());
    let mut gapped = metrics;
    gapped[400].source_sample_index += 1;
    assert!(audit_deferred_shr_rate_score_counterfactual(&gapped).is_err());
    assert_eq!(counterfactual, EXPECTED_COUNTERFACTUAL);
}

#[test]
fn deferred_selector_covers_every_start_phase_and_rate_axis() {
    for start_offset in 0..SHR_START_HYPOTHESES {
        let metrics = synthetic_deferred_selection_metrics(start_offset, 5, 0, 1.0, 1, true);
        exact_deferred_selection(&metrics, start_offset, 5, 0, 1);
    }
    for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
        let metrics = synthetic_deferred_selection_metrics(31, mu_sixteenths, 0, 1.0, 127, true);
        exact_deferred_selection(&metrics, 31, mu_sixteenths, 0, 127);
    }
    for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
        let metrics = synthetic_deferred_selection_metrics(31, 8, rate_error_ppm, 1.0, 5, true);
        exact_deferred_selection(&metrics, 31, 8, rate_error_ppm, 5);
    }

    let extreme = synthetic_deferred_selection_metrics(63, 15, 100, 1.0, 127, true);
    let selected = exact_deferred_selection(&extreme, 63, 15, 100, 127);
    assert_eq!(selected.handoff.payload_left_offset, 832);
    assert_eq!(selected.handoff.payload_mu_bits, 0x3c6a_4a8c);
    assert_eq!(selected.handoff.payload_source_sample_index, 30_832);
}

#[test]
fn deferred_selector_validates_extent_values_coordinates_and_payload_boundary() {
    let metrics = synthetic_deferred_selection_metrics(23, 11, 100, 1.0, 5, true);
    let baseline = exact_deferred_selection(&metrics, 23, 11, 100, 5);

    assert!(select_deferred_shr_rate_timing(&metrics[..831]).is_err());
    let mut extra = metrics.clone();
    extra.push(PositionedMetric {
        value: -123.0,
        source_sample_index: 30_832,
    });
    assert!(select_deferred_shr_rate_timing(&extra).is_err());

    let mut nonfinite = metrics.clone();
    nonfinite[400].value = f32::INFINITY;
    assert!(select_deferred_shr_rate_timing(&nonfinite).is_err());
    let mut gapped = metrics.clone();
    gapped[400].source_sample_index += 1;
    assert!(select_deferred_shr_rate_timing(&gapped).is_err());

    let mut extended = metrics.clone();
    extended.extend((0..MAX_METRIC_QUEUE).map(|offset| PositionedMetric {
        value: offset as f32,
        source_sample_index: 30_832 + offset as u64,
    }));
    let before = select_deferred_shr_rate_timing(&extended[..DEFERRED_SELECTION_METRICS]).unwrap();
    for metric in &mut extended[DEFERRED_SELECTION_METRICS..] {
        metric.value = metric.value.mul_add(-17.0, 91.0);
    }
    let after = select_deferred_shr_rate_timing(&extended[..DEFERRED_SELECTION_METRICS]).unwrap();
    assert_eq!(before, after);
    assert_eq!(before, Some(baseline));

    let mut overflowing = synthetic_deferred_selection_metrics(63, 15, 100, 1.0, 5, true);
    let first = u64::MAX - (DEFERRED_SELECTION_METRICS as u64 - 1);
    for (offset, metric) in overflowing.iter_mut().enumerate() {
        metric.source_sample_index = first + offset as u64;
    }
    assert!(select_deferred_shr_rate_timing(&overflowing).is_err());
}

#[test]
fn deferred_selector_uses_full_shr_score_and_frozen_tie_order() {
    let candidate =
        |start_offset, mu_sixteenths, rate_error_ppm, score| ScoredDeferredShrRateTimingSelection {
            handoff: DeferredShrRateHandoff {
                selection: ShrRateTimingSelection {
                    timing: ShrTimingSelection {
                        start_offset,
                        mu_sixteenths,
                        source_sample_index: 10_000 + u64::from(start_offset),
                    },
                    rate_error_ppm,
                },
                phr_length: 5,
                payload_left_offset: 800,
                payload_source_sample_index: 10_800,
                payload_mu_bits: 0.5f32.to_bits(),
                prior_metric_bits: 1.0f32.to_bits(),
            },
            score,
        };
    let prior = candidate(23, 11, 100, 0.9);
    assert!(deferred_candidate_is_better(
        candidate(63, 15, -100, 0.91),
        prior
    ));
    assert!(!deferred_candidate_is_better(
        candidate(0, 0, 0, 0.89),
        prior
    ));
    assert!(deferred_candidate_is_better(
        candidate(22, 15, -100, 0.9),
        prior
    ));
    assert!(deferred_candidate_is_better(
        candidate(23, 10, -100, 0.9),
        prior
    ));
    assert!(deferred_candidate_is_better(
        candidate(23, 11, 0, 0.9),
        prior
    ));
    assert!(deferred_candidate_is_better(
        candidate(23, 11, -100, 0.9),
        prior
    ));
    assert!(!deferred_candidate_is_better(
        candidate(23, 11, 100, 0.9),
        prior
    ));
}

#[test]
fn shr_rate_selector_covers_every_start_phase_and_rate_axis() {
    for start_offset in 0..SHR_START_HYPOTHESES {
        let metrics = synthetic_rate_selection_metrics(start_offset, 5, 0, 1.0, SHR_SYMBOLS, true);
        exact_rate_selection(&metrics, start_offset, 5, 0);
    }
    for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
        let metrics =
            synthetic_rate_selection_metrics(31, mu_sixteenths, 0, 1.0, SHR_SYMBOLS, true);
        exact_rate_selection(&metrics, 31, mu_sixteenths, 0);
    }
    for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
        let metrics =
            synthetic_rate_selection_metrics(31, 8, rate_error_ppm, 1.0, SHR_SYMBOLS, true);
        exact_rate_selection(&metrics, 31, 8, rate_error_ppm);
    }
}

#[test]
fn shr_rate_diagnostic_audit_is_independent_exact_and_bounded() {
    let metrics = synthetic_rate_selection_metrics(23, 11, 100, 1.0, SHR_SYMBOLS, true);
    let authoritative = exact_rate_selection(&metrics, 23, 11, 100);
    let audit = audit_shr_rate_timing(&metrics).unwrap();
    assert_eq!(audit.evaluated_hypotheses, SHR_RATE_HYPOTHESIS_COUNT);
    assert!((1..=SHR_RATE_HYPOTHESIS_COUNT).contains(&audit.eligible_hypotheses));
    let selected = audit
        .best_eligible
        .expect("synthetic SHR must have an audited eligible hypothesis");
    assert_eq!(selected.selection, authoritative.selection);
    assert_eq!(selected.full_score_bits, authoritative.score.to_bits());
    assert_eq!(selected.suffix_score_bits.len(), 6);
    assert!(
        selected
            .suffix_score_bits
            .iter()
            .all(|score| { score.is_some_and(|score| f64::from_bits(score) > UNIQUE_ANGULAR_CAP) })
    );
    assert!((4..10).contains(&selected.limiting_suffix_symbol.unwrap()));
    assert!(audit.best_unconstrained.is_some());

    let mut wrong_symbols = SHR_SYMBOLS;
    wrong_symbols[9] = 0;
    let wrong_sfd = synthetic_rate_selection_metrics(17, 6, 100, 1.0, wrong_symbols, true);
    assert_eq!(select_shr_rate_timing(&wrong_sfd).unwrap(), None);
    let rejected = audit_shr_rate_timing(&wrong_sfd).unwrap();
    assert_eq!(rejected.evaluated_hypotheses, SHR_RATE_HYPOTHESIS_COUNT);
    assert_eq!(rejected.eligible_hypotheses, 0);
    assert_eq!(rejected.best_eligible, None);
    assert!(rejected.best_unconstrained.is_some());
}

#[test]
fn clock_diagnostic_boundaries_and_overflow_accounting_are_exact() {
    assert_eq!(MAX_CLOCK_BOUNDARY_POINTS, 625);
    let mut trace = ClockTrajectoryTrace::new(CLOCK_OMEGA, CLOCK_INITIAL_MU);
    let emitted = (MAX_CLOCK_BOUNDARY_POINTS as u64 + 1) * CLOCK_BOUNDARY_INTERVAL;
    for index in 0..emitted {
        trace
            .observe(
                PositionedChip {
                    metric: 0.25,
                    source_sample_index: index,
                    burst_start: index == 0,
                    payload_length: None,
                },
                index as usize % MAX_METRIC_QUEUE + 1,
                CLOCK_OMEGA,
                CLOCK_INITIAL_MU,
            )
            .unwrap();
    }
    assert_eq!(trace.emitted_chips, emitted);
    assert_eq!(trace.markers, 1);
    assert_eq!(trace.first_source_sample_index, Some(0));
    assert_eq!(trace.last_source_sample_index, Some(emitted - 1));
    assert_eq!(trace.boundaries.len(), MAX_CLOCK_BOUNDARY_POINTS);
    assert_eq!(trace.boundary_overflow, 1);
    assert_eq!(trace.advance_overflow, 0);
    assert_eq!(trace.advance_histogram, [emitted / 4; MAX_METRIC_QUEUE]);
    assert_eq!(trace.initial_omega_bits, CLOCK_OMEGA.to_bits());
    assert_eq!(trace.final_omega_bits, CLOCK_OMEGA.to_bits());
    assert_eq!(trace.min_omega_bits, CLOCK_OMEGA.to_bits());
    assert_eq!(trace.max_omega_bits, CLOCK_OMEGA.to_bits());
    assert_eq!(trace.initial_mu_bits, CLOCK_INITIAL_MU.to_bits());
    assert_eq!(trace.final_mu_bits, CLOCK_INITIAL_MU.to_bits());
    assert_eq!(trace.min_mu_bits, CLOCK_INITIAL_MU.to_bits());
    assert_eq!(trace.max_mu_bits, CLOCK_INITIAL_MU.to_bits());
    for (index, point) in trace.boundaries.iter().enumerate() {
        let chip_ordinal = (index as u64 + 1) * CLOCK_BOUNDARY_INTERVAL;
        assert_eq!(point.chip_ordinal, chip_ordinal);
        assert_eq!(point.source_sample_index, chip_ordinal - 1);
        assert_eq!(point.omega_bits, CLOCK_OMEGA.to_bits());
        assert_eq!(point.mu_bits, CLOCK_INITIAL_MU.to_bits());
    }

    trace
        .observe(
            PositionedChip {
                metric: 0.25,
                source_sample_index: emitted,
                burst_start: false,
                payload_length: None,
            },
            MAX_METRIC_QUEUE + 1,
            CLOCK_OMEGA,
            CLOCK_INITIAL_MU,
        )
        .unwrap();
    assert_eq!(trace.advance_overflow, 1);
}

#[test]
fn shr_rate_diagnostic_distinguishes_selector_and_prelock_rejections() {
    let start = BurstStart {
        replay_start: 20_000,
        detected_at: 20_063,
    };
    let event = DetectedBurst {
        replay_start: start.replay_start,
        detected_at: start.detected_at,
        end_exclusive: 20_800,
        forced_end: false,
    };

    let mut wrong_symbols = SHR_SYMBOLS;
    wrong_symbols[9] = 0;
    let mut no_eligible = BurstClockCore::with_shr_rate_diagnostic();
    no_eligible.active_diagnostic_trace = Some(ActiveShrRateDiagnosticTrace {
        start,
        selector: None,
        outcome: None,
        clock: None,
    });
    no_eligible.timing_state = BurstTimingState::Selecting;
    no_eligible
        .shr_metrics
        .extend(synthetic_rate_selection_metrics(
            17,
            6,
            100,
            1.0,
            wrong_symbols,
            true,
        ));
    no_eligible.finish_shr_selection().unwrap();
    assert_eq!(no_eligible.timing_state, BurstTimingState::Rejected);
    no_eligible.finalize_diagnostic_event(event).unwrap();
    let trace = no_eligible.diagnostic_trace().unwrap();
    let rejected = trace.events[0].as_ref().unwrap();
    assert_eq!(rejected.outcome, ShrRateTraceOutcome::RejectedNoEligible);
    assert_eq!(rejected.selector.unwrap().eligible_hypotheses, 0);
    assert_eq!(rejected.clock, None);
    assert_eq!(trace.events[1], None);
    assert_eq!(trace.overflow, 0);

    let mut before_lock = BurstClockCore::with_shr_rate_diagnostic();
    before_lock.active_diagnostic_trace = Some(ActiveShrRateDiagnosticTrace {
        start,
        selector: None,
        outcome: None,
        clock: None,
    });
    before_lock.timing_state = BurstTimingState::Selecting;
    before_lock
        .shr_metrics
        .extend(synthetic_rate_selection_metrics(
            23,
            11,
            100,
            1.0,
            SHR_SYMBOLS,
            true,
        ));
    before_lock.finish_shr_selection().unwrap();
    let selection = match before_lock.pending_selection {
        Some(PendingShrTimingSelection::PhaseRate(selection)) => selection,
        _ => panic!("joint SHR selection must remain pending before its first clock output"),
    };
    before_lock.end_active_timing().unwrap();
    before_lock.finalize_diagnostic_event(event).unwrap();
    let trace = before_lock.diagnostic_trace().unwrap();
    let rejected = trace.events[0].as_ref().unwrap();
    assert_eq!(
        rejected.outcome,
        ShrRateTraceOutcome::RejectedBeforeLock(selection)
    );
    assert_eq!(
        rejected.selector.unwrap().best_eligible.unwrap().selection,
        selection
    );
    assert!(rejected.clock.is_some());
    assert_eq!(trace.events[1], None);
    assert_eq!(trace.overflow, 0);
}

#[test]
fn shr_rate_diagnostic_event_capacity_is_fixed_and_counts_overflow() {
    let mut core = BurstClockCore::with_shr_rate_diagnostic();
    for event_index in 0..3_u64 {
        let replay_start = 10_000 + event_index * 1_000;
        let start = BurstStart {
            replay_start,
            detected_at: replay_start + 63,
        };
        core.active_diagnostic_trace = Some(ActiveShrRateDiagnosticTrace {
            start,
            selector: None,
            outcome: Some(ShrRateTraceOutcome::RejectedBeforeSelection),
            clock: None,
        });
        core.finalize_diagnostic_event(DetectedBurst {
            replay_start,
            detected_at: start.detected_at,
            end_exclusive: replay_start + 700,
            forced_end: false,
        })
        .unwrap();
    }
    let trace = core.diagnostic_trace().unwrap();
    assert_eq!(trace.events.iter().flatten().count(), MAX_SELECTION_RECORDS);
    assert_eq!(trace.events[0].as_ref().unwrap().event.replay_start, 10_000);
    assert_eq!(trace.events[1].as_ref().unwrap().event.replay_start, 11_000);
    assert_eq!(trace.overflow, 1);
}

#[test]
fn zero_rate_hypothesis_matches_phase_only_math_and_ignores_metric_703() {
    let start_offset = 23;
    let mu_sixteenths = 11;
    let mut metrics =
        synthetic_rate_selection_metrics(start_offset, mu_sixteenths, 0, 1.0, SHR_SYMBOLS, true);
    let old = exact_selection(
        &metrics[..SHR_SELECTION_METRICS],
        start_offset,
        mu_sixteenths,
    );
    let joint = exact_rate_selection(&metrics, start_offset, mu_sixteenths, 0);
    assert_eq!(joint.selection.timing, old.selection);
    assert_eq!(joint.score, old.score);

    let before: [f32; SHR_CHIPS] = std::array::from_fn(|chip_index| {
        let old_chip = interpolated_shr_chip(&metrics, start_offset, mu_sixteenths, chip_index);
        let joint_chip =
            interpolated_shr_rate_chip(&metrics, start_offset, mu_sixteenths, 0, chip_index);
        assert_eq!(joint_chip, old_chip);
        joint_chip
    });
    metrics[703].value = 1_000.0;
    let after: [f32; SHR_CHIPS] = std::array::from_fn(|chip_index| {
        interpolated_shr_rate_chip(&metrics, start_offset, mu_sixteenths, 0, chip_index)
    });
    assert_eq!(after, before);
    assert_eq!(full_shr_correlation(&after), Some(joint.score));
}

#[test]
fn shr_rate_selector_is_scale_invariant_and_uses_frozen_tie_order() {
    let selections = [0.25, 1.0, 4.0].map(|scale| {
        let metrics = synthetic_rate_selection_metrics(23, 11, 100, scale, SHR_SYMBOLS, true);
        exact_rate_selection(&metrics, 23, 11, 100).selection
    });
    assert!(
        selections
            .iter()
            .all(|selection| *selection == selections[0])
    );

    let candidate = |start_offset, mu_sixteenths, rate_error_ppm| ScoredShrRateTimingSelection {
        selection: ShrRateTimingSelection {
            timing: ShrTimingSelection {
                start_offset,
                mu_sixteenths,
                source_sample_index: 0,
            },
            rate_error_ppm,
        },
        score: 0.75,
    };
    assert!(shr_rate_candidate_is_better(
        candidate(2, 15, 100),
        candidate(3, 0, 0)
    ));
    assert!(shr_rate_candidate_is_better(
        candidate(2, 3, 100),
        candidate(2, 4, 0)
    ));
    assert!(shr_rate_candidate_is_better(
        candidate(2, 3, 0),
        candidate(2, 3, -100)
    ));
    assert!(shr_rate_candidate_is_better(
        candidate(2, 3, -100),
        candidate(2, 3, 100)
    ));
    assert!(!shr_rate_candidate_is_better(
        candidate(2, 3, 100),
        candidate(2, 3, -100)
    ));
}

#[test]
fn shr_selector_covers_every_start_and_fractional_phase_axis() {
    for start_offset in 0..SHR_START_HYPOTHESES {
        let metrics = synthetic_selection_metrics(start_offset, 5, 1.0, SHR_SYMBOLS, true);
        exact_selection(&metrics, start_offset, 5);
    }
    for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
        let metrics = synthetic_selection_metrics(31, mu_sixteenths, 1.0, SHR_SYMBOLS, true);
        exact_selection(&metrics, 31, mu_sixteenths);
    }
}

#[test]
fn shr_selector_is_scale_invariant_and_breaks_exact_ties_lexicographically() {
    let selections = [0.25, 1.0, 4.0].map(|scale| {
        let metrics = synthetic_selection_metrics(23, 11, scale, SHR_SYMBOLS, true);
        exact_selection(&metrics, 23, 11).selection
    });
    assert!(
        selections
            .iter()
            .all(|selection| *selection == selections[0])
    );

    let tied = synthetic_selection_metrics(0, 9, 1.0, SHR_SYMBOLS, false);
    let selected = exact_selection(&tied, 0, 0);
    assert_eq!(selected.score, 1.0);
}

#[test]
fn shr_selection_is_invariant_to_later_symbol_content() {
    let start_offset = 19;
    let mu_sixteenths = 7;
    let mut first =
        synthetic_selection_metrics(start_offset, mu_sixteenths, 1.0, SHR_SYMBOLS, true);
    let mut second = first.clone();
    let tail_start = start_offset + 2 * SHR_CHIPS;
    for (offset, metric) in first[tail_start..].iter_mut().enumerate() {
        metric.value = if offset.is_multiple_of(2) { -0.4 } else { 0.9 };
    }
    for (offset, metric) in second[tail_start..].iter_mut().enumerate() {
        metric.value = match offset % 3 {
            0 => 1.2,
            1 => -0.7,
            _ => 0.2,
        };
    }
    let first_selection = exact_selection(&first, start_offset, mu_sixteenths).selection;
    let second_selection = exact_selection(&second, start_offset, mu_sixteenths).selection;
    assert_eq!(first_selection, second_selection);
}

#[test]
fn shr_selector_rejects_wrong_sfd_silence_truncation_and_coordinate_gaps() {
    let mut wrong_symbols = SHR_SYMBOLS;
    wrong_symbols[9] = 0;
    let wrong_sfd = synthetic_selection_metrics(17, 6, 1.0, wrong_symbols, true);
    assert_eq!(select_shr_timing(&wrong_sfd).unwrap(), None);

    let silence = (0..SHR_SELECTION_METRICS)
        .map(|index| PositionedMetric {
            value: 0.0,
            source_sample_index: index as u64,
        })
        .collect::<Vec<_>>();
    assert_eq!(select_shr_timing(&silence).unwrap(), None);
    assert!(select_shr_timing(&silence[..SHR_SELECTION_METRICS - 1]).is_err());

    let mut gapped = synthetic_selection_metrics(3, 2, 1.0, SHR_SYMBOLS, true);
    gapped[400].source_sample_index += 1;
    assert!(select_shr_timing(&gapped).is_err());

    let mut nonfinite = synthetic_selection_metrics(3, 2, 1.0, SHR_SYMBOLS, true);
    nonfinite[0].value = f32::NAN;
    assert!(select_shr_timing(&nonfinite).is_err());
}

#[test]
fn shr_rate_selection_is_invariant_to_later_content() {
    let start_offset = 19;
    let mu_sixteenths = 7;
    let rate_error_ppm = 0;
    let mut first = synthetic_rate_selection_metrics(
        start_offset,
        mu_sixteenths,
        rate_error_ppm,
        1.0,
        SHR_SYMBOLS,
        true,
    );
    let mut second = first.clone();
    let (last_left, _) =
        shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, SHR_CHIPS - 1);
    let tail_start = last_left + 2;
    for (offset, metric) in first[tail_start..].iter_mut().enumerate() {
        metric.value = if offset.is_multiple_of(2) { -0.4 } else { 0.9 };
    }
    for (offset, metric) in second[tail_start..].iter_mut().enumerate() {
        metric.value = match offset % 3 {
            0 => 1.2,
            1 => -0.7,
            _ => 0.2,
        };
    }
    let first_selection =
        exact_rate_selection(&first, start_offset, mu_sixteenths, rate_error_ppm).selection;
    let second_selection =
        exact_rate_selection(&second, start_offset, mu_sixteenths, rate_error_ppm).selection;
    assert_eq!(first_selection, second_selection);
}

#[test]
fn shr_rate_selector_rejects_wrong_sfd_silence_truncation_and_coordinate_gaps() {
    let mut wrong_symbols = SHR_SYMBOLS;
    wrong_symbols[9] = 0;
    let wrong_sfd = synthetic_rate_selection_metrics(17, 6, 100, 1.0, wrong_symbols, true);
    assert_eq!(select_shr_rate_timing(&wrong_sfd).unwrap(), None);

    let silence = (0..SHR_RATE_SELECTION_METRICS)
        .map(|index| PositionedMetric {
            value: 0.0,
            source_sample_index: index as u64,
        })
        .collect::<Vec<_>>();
    assert_eq!(select_shr_rate_timing(&silence).unwrap(), None);
    assert!(select_shr_rate_timing(&silence[..SHR_SELECTION_METRICS]).is_err());

    let mut gapped = synthetic_rate_selection_metrics(3, 2, -100, 1.0, SHR_SYMBOLS, true);
    gapped[400].source_sample_index += 1;
    assert!(select_shr_rate_timing(&gapped).is_err());

    let mut nonfinite = synthetic_rate_selection_metrics(3, 2, 100, 1.0, SHR_SYMBOLS, true);
    nonfinite[703].value = f32::NAN;
    assert!(select_shr_rate_timing(&nonfinite).is_err());

    let endpoint = synthetic_rate_selection_metrics(63, 15, 100, 1.0, SHR_SYMBOLS, true);
    exact_rate_selection(&endpoint, 63, 15, 100);
}

#[test]
fn selected_metric_replay_matches_an_independent_clock_and_marks_once() {
    let metrics = synthetic_selection_metrics(27, 13, 1.0, SHR_SYMBOLS, true);
    let selected = exact_selection(&metrics, 27, 13).selection;
    let replay = metrics[usize::from(selected.start_offset)..].to_vec();

    let mut expected_clock = ClockRecoveryState::new();
    expected_clock
        .reset(f32::from(selected.mu_sixteenths) / SHR_PHASE_HYPOTHESES as f32)
        .unwrap();
    let expected = replay
        .iter()
        .copied()
        .filter_map(|metric| expected_clock.push(metric).unwrap())
        .collect::<Vec<_>>();

    let mut core = BurstClockCore::with_shr_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    let mut observed = Vec::new();
    while core.has_pending() {
        if let Some(chip) = core.take_ready() {
            observed.push(chip);
        } else {
            core.advance_metric_replay().unwrap();
        }
    }
    assert_eq!(observed, expected);
    assert_eq!(observed.iter().filter(|chip| chip.burst_start).count(), 1);
    assert_eq!(
        observed[0].source_sample_index,
        selected.source_sample_index
    );
    assert_eq!(core.shr_locks, 1);
    assert_eq!(core.shr_rejections, 0);
    assert_eq!(core.shr_selections, [Some(selected), None]);
    assert_eq!(core.shr_rate_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(core.max_shr_selection_metrics, SHR_SELECTION_METRICS);
    assert_eq!(
        core.max_shr_metric_replay,
        SHR_SELECTION_METRICS - usize::from(selected.start_offset)
    );
    assert!(core.shr_metrics.is_empty());
}

#[test]
fn selected_rate_metric_replay_matches_independent_clock_and_marks_once() {
    let metrics = synthetic_rate_selection_metrics(27, 13, 100, 1.0, SHR_SYMBOLS, true);
    let selected = exact_rate_selection(&metrics, 27, 13, 100).selection;
    let replay = metrics[usize::from(selected.timing.start_offset)..].to_vec();
    let expected = independent_clock_replay(
        &replay,
        shr_rate_clock_omega(selected.rate_error_ppm),
        f32::from(selected.timing.mu_sixteenths) / SHR_PHASE_HYPOTHESES as f32,
    );

    let mut core = BurstClockCore::with_shr_rate_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    let mut observed = Vec::new();
    while core.has_pending() {
        if let Some(chip) = core.take_ready() {
            observed.push(chip);
        } else {
            core.advance_metric_replay().unwrap();
        }
    }
    assert_eq!(observed, expected);
    assert_eq!(observed.iter().filter(|chip| chip.burst_start).count(), 1);
    assert_eq!(
        observed[0].source_sample_index,
        selected.timing.source_sample_index
    );
    assert_eq!(core.shr_locks, 1);
    assert_eq!(core.shr_rejections, 0);
    assert_eq!(core.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(core.shr_rate_selections, [Some(selected), None]);
    assert_eq!(core.max_shr_selection_metrics, SHR_RATE_SELECTION_METRICS);
    assert_eq!(
        core.max_shr_metric_replay,
        SHR_RATE_SELECTION_METRICS - usize::from(selected.timing.start_offset)
    );
    let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    assert!((CLOCK_OMEGA - omega_limit..=CLOCK_OMEGA + omega_limit).contains(&core.clock.omega));
    assert!(core.shr_metrics.is_empty());
}

#[test]
fn deferred_handoff_replay_seeds_prior_and_commits_on_first_payload_output() {
    let metrics = synthetic_deferred_selection_metrics(0, 0, -100, 1.0, 5, true);
    let handoff = exact_deferred_selection(&metrics, 0, 0, -100, 5).handoff;
    let replay = metrics[usize::from(handoff.payload_left_offset)..].to_vec();
    assert_eq!(replay.len(), MAX_DEFERRED_HANDOFF_TAIL_METRICS);
    let initial_omega = shr_rate_clock_omega(handoff.selection.rate_error_ppm);
    let initial_mu = f32::from_bits(handoff.payload_mu_bits);
    let prior_metric = f32::from_bits(handoff.prior_metric_bits);
    let expected = independent_deferred_handoff_replay(
        &replay,
        initial_omega,
        initial_mu,
        prior_metric,
        handoff.phr_length,
    );
    assert!(!expected.is_empty());

    let left = replay[0];
    let right = replay[1];
    let recovered = left.value + initial_mu * (right.value - left.value);
    let mm = slice(prior_metric) * recovered - slice(recovered) * prior_metric;
    let omega_limit = CLOCK_OMEGA * CLOCK_RELATIVE_LIMIT;
    let expected_omega = CLOCK_OMEGA
        + (initial_omega + CLOCK_GAIN_OMEGA * mm - CLOCK_OMEGA).clamp(-omega_limit, omega_limit);
    let mut expected_mu = initial_mu + expected_omega + CLOCK_GAIN_MU * mm;
    let expected_advance = expected_mu.floor() as usize;
    expected_mu -= expected_advance as f32;
    assert_ne!(mm.to_bits(), 0.0f32.to_bits());

    let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Selected);
    assert_eq!(
        core.max_shr_metric_replay,
        MAX_DEFERRED_HANDOFF_TAIL_METRICS
    );
    assert_eq!(core.shr_locks, 0);
    assert_eq!(core.markers, 0);
    assert_eq!(core.deferred_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(
        core.pending_selection,
        Some(PendingShrTimingSelection::Deferred(handoff))
    );

    for _ in 0..CLOCK_LOOK_AHEAD {
        core.advance_metric_replay().unwrap();
        assert!(core.ready.is_none());
        assert_eq!(core.shr_locks, 0);
        assert_eq!(
            core.pending_selection,
            Some(PendingShrTimingSelection::Deferred(handoff))
        );
    }
    core.advance_metric_replay().unwrap();
    assert_eq!(core.shr_locks, 1);
    assert_eq!(core.pending_selection, None);
    assert_eq!(core.deferred_selections, [Some(handoff), None]);
    assert_eq!(core.clock.omega.to_bits(), expected_omega.to_bits());
    assert_eq!(core.clock.mu.to_bits(), expected_mu.to_bits());
    assert_eq!(core.clock.last_mm.to_bits(), recovered.to_bits());
    assert_eq!(
        core.clock.metrics.len(),
        MAX_METRIC_QUEUE - expected_advance
    );

    let mut observed = vec![
        core.take_ready()
            .expect("four metrics must yield payload chip"),
    ];
    while core.has_pending() {
        if let Some(chip) = core.take_ready() {
            observed.push(chip);
        } else {
            core.advance_metric_replay().unwrap();
        }
    }
    assert_eq!(observed, expected);
    assert_eq!(
        observed[0].source_sample_index,
        handoff.payload_source_sample_index
    );
    assert!(observed[0].burst_start);
    assert_eq!(observed[0].payload_length, Some(5));
    assert!(
        observed[1..]
            .iter()
            .all(|chip| { !chip.burst_start && chip.payload_length.is_none() })
    );
    assert_eq!(core.markers, 1);
    assert_eq!(core.shr_rejections, 0);
    assert_eq!(core.clock.max_metric_queue_observed, MAX_METRIC_QUEUE);
}

#[test]
fn deferred_zero_tail_waits_for_new_input_and_rejects_before_first_output() {
    let metrics = synthetic_deferred_selection_metrics(63, 15, 100, 1.0, 127, true);
    let handoff = exact_deferred_selection(&metrics, 63, 15, 100, 127).handoff;
    assert_eq!(
        usize::from(handoff.payload_left_offset),
        DEFERRED_SELECTION_METRICS
    );
    let new_metrics = [0.75, -0.25, 1.25, -0.75].map(|value| PositionedMetric {
        value,
        source_sample_index: handoff.payload_source_sample_index,
    });

    for available in 0..=CLOCK_LOOK_AHEAD {
        let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
        core.active = true;
        core.timing_state = BurstTimingState::Selecting;
        core.shr_metrics.extend(metrics.iter().copied());
        core.max_shr_selection_metrics = core.shr_metrics.len();
        core.finish_shr_selection().unwrap();
        assert!(core.shr_metrics.is_empty());
        assert!(!core.has_pending());
        for (offset, mut metric) in new_metrics[..available].iter().copied().enumerate() {
            metric.source_sample_index += offset as u64;
            assert!(core.push_selected_clock_metric(metric).unwrap().is_none());
        }
        core.end_active_timing().unwrap();
        assert_eq!(core.timing_state, BurstTimingState::Idle);
        assert_eq!(core.shr_locks, 0);
        assert_eq!(core.shr_rejections, 1);
        assert_eq!(core.markers, 0);
        assert_eq!(core.deferred_selections, [None; MAX_SELECTION_RECORDS]);
        assert_eq!(core.pending_selection, None);
        assert!(core.clock.metrics.is_empty());
    }

    let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    for (offset, mut metric) in new_metrics.into_iter().enumerate() {
        metric.source_sample_index += offset as u64;
        let output = core.push_selected_clock_metric(metric).unwrap();
        if offset < CLOCK_LOOK_AHEAD {
            assert!(output.is_none());
        } else {
            core.ready = output;
        }
    }
    assert_eq!(core.shr_locks, 1);
    assert_eq!(core.shr_rejections, 0);
    assert_eq!(core.deferred_selections, [Some(handoff), None]);
    let chip = core
        .take_ready()
        .expect("fourth new metric must yield payload chip");
    assert_eq!(
        chip.source_sample_index,
        handoff.payload_source_sample_index
    );
    assert!(chip.burst_start);
    assert_eq!(chip.payload_length, Some(127));
    core.end_active_timing().unwrap();
    assert_eq!(core.markers, 1);
    assert_eq!(core.shr_rejections, 0);
}

#[test]
fn deferred_zero_tail_forced_eof_requires_four_new_metrics_to_lock() {
    for available in 0..=MAX_METRIC_QUEUE {
        let input = synthetic_zero_tail_eof_input(available);
        let (output, stats) = run_deferred_core_chunks(&input, &[1, 17, 64, 511]);
        assert_eq!(stats.burst.starts, 1);
        assert_eq!(stats.burst.ends, 1);
        assert_eq!(
            stats.burst.max_shr_selection_metrics,
            DEFERRED_SELECTION_METRICS
        );
        assert_eq!(stats.burst.max_shr_metric_replay, 0);
        assert_eq!(stats.burst.shr_selections, [None; MAX_SELECTION_RECORDS]);
        if available < MAX_METRIC_QUEUE {
            assert!(output.is_empty());
            assert_eq!(stats.burst.markers, 0);
            assert_eq!(stats.burst.shr_locks, 0);
            assert_eq!(stats.burst.shr_rejections, 1);
            assert_eq!(stats.deferred_selections, [None; MAX_SELECTION_RECORDS]);
        } else {
            assert_eq!(output.len(), 1);
            assert!(output[0].burst_start);
            assert_eq!(output[0].payload_length, Some(127));
            assert_eq!(stats.burst.markers, 1);
            assert_eq!(stats.burst.shr_locks, 1);
            assert_eq!(stats.burst.shr_rejections, 0);
            let handoff = stats.deferred_selections[0]
                .expect("four new metrics must commit the zero-tail handoff");
            assert_eq!(
                handoff.payload_left_offset,
                DEFERRED_SELECTION_METRICS as u16
            );
            assert_eq!(
                output[0].source_sample_index,
                handoff.payload_source_sample_index
            );
            assert_eq!(stats.deferred_selections[1], None);
        }
    }
}

#[test]
fn deferred_selection_storage_is_bounded_and_counts_overflow() {
    let metrics = synthetic_deferred_selection_metrics(23, 11, 100, 1.0, 5, true);
    let handoff = exact_deferred_selection(&metrics, 23, 11, 100, 5).handoff;
    let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
    core.pending_selection = Some(PendingShrTimingSelection::Deferred(handoff));
    assert!(core.commit_pending_selection().is_err());
    assert_eq!(
        core.pending_selection,
        Some(PendingShrTimingSelection::Deferred(handoff))
    );
    assert_eq!(core.shr_locks, 0);
    assert_eq!(core.deferred_selections, [None; MAX_SELECTION_RECORDS]);

    for event_index in 0..3 {
        core.clock
            .reset_with_handoff(
                shr_rate_clock_omega(handoff.selection.rate_error_ppm),
                f32::from_bits(handoff.payload_mu_bits),
                f32::from_bits(handoff.prior_metric_bits),
                Some(handoff.phr_length),
            )
            .unwrap();
        core.pending_selection = Some(PendingShrTimingSelection::Deferred(handoff));
        for offset in 0..MAX_METRIC_QUEUE {
            let output = core
                .push_selected_clock_metric(PositionedMetric {
                    value: (event_index * MAX_METRIC_QUEUE + offset) as f32 - 3.5,
                    source_sample_index: handoff.payload_source_sample_index + offset as u64,
                })
                .unwrap();
            assert_eq!(output.is_some(), offset == CLOCK_LOOK_AHEAD);
        }
        assert_eq!(core.pending_selection, None);
        assert!(!core.deferred_output_produced);
    }
    let stats = core.deferred_stats();
    assert_eq!(stats.burst.shr_locks, 3);
    assert_eq!(stats.burst.shr_selection_overflow, 1);
    assert_eq!(
        stats.deferred_selections,
        [Some(handoff); MAX_SELECTION_RECORDS]
    );
    assert_eq!(
        stats.deferred_selections.iter().flatten().count() as u64
            + stats.burst.shr_selection_overflow,
        stats.burst.shr_locks
    );
}

#[test]
fn deferred_wrong_invalid_and_truncated_structure_never_hands_off() {
    let silence = (0..DEFERRED_SELECTION_METRICS)
        .map(|index| PositionedMetric {
            value: 0.0,
            source_sample_index: index as u64,
        })
        .collect::<Vec<_>>();
    let invalid_length = synthetic_deferred_selection_metrics(23, 11, 100, 1.0, 0, true);
    for metrics in [silence, invalid_length] {
        let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
        core.active = true;
        core.timing_state = BurstTimingState::Selecting;
        core.shr_metrics.extend(metrics);
        core.max_shr_selection_metrics = core.shr_metrics.len();
        core.finish_shr_selection().unwrap();
        assert_eq!(core.timing_state, BurstTimingState::Rejected);
        core.end_active_timing().unwrap();
        assert_eq!(core.timing_state, BurstTimingState::Idle);
        assert_eq!(core.shr_locks, 0);
        assert_eq!(core.shr_rejections, 1);
        assert_eq!(core.markers, 0);
        assert_eq!(core.pending_selection, None);
        assert_eq!(core.deferred_selections, [None; MAX_SELECTION_RECORDS]);
    }

    let truncated = synthetic_deferred_selection_metrics(23, 11, 100, 1.0, 5, true);
    let mut core = BurstClockCore::with_deferred_shr_rate_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics
        .extend(truncated[..DEFERRED_SELECTION_METRICS - 1].iter().copied());
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.end_active_timing().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Idle);
    assert_eq!(core.shr_locks, 0);
    assert_eq!(core.shr_rejections, 1);
    assert_eq!(core.markers, 0);
    assert_eq!(core.pending_selection, None);
    assert_eq!(core.deferred_selections, [None; MAX_SELECTION_RECORDS]);
}

#[test]
fn selection_on_the_ending_metric_is_rejected_before_lock_or_marker() {
    let metrics = synthetic_selection_metrics(12, 4, 1.0, SHR_SYMBOLS, true);
    let mut core = BurstClockCore::with_shr_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Selected);
    assert!(core.pending_selection.is_some());
    assert!(!core.shr_metrics.is_empty());

    core.end_active_timing().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Idle);
    assert_eq!(core.shr_locks, 0);
    assert_eq!(core.shr_rejections, 1);
    assert_eq!(core.markers, 0);
    assert_eq!(core.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert!(core.pending_selection.is_none());
    assert!(core.shr_metrics.is_empty());
}

#[test]
fn rate_selection_on_the_ending_metric_is_rejected_before_lock_or_marker() {
    let metrics = synthetic_rate_selection_metrics(12, 4, -100, 1.0, SHR_SYMBOLS, true);
    let mut core = BurstClockCore::with_shr_rate_initializer();
    core.active = true;
    core.timing_state = BurstTimingState::Selecting;
    core.shr_metrics.extend(metrics);
    core.max_shr_selection_metrics = core.shr_metrics.len();
    core.finish_shr_selection().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Selected);
    assert!(core.pending_selection.is_some());
    assert!(!core.shr_metrics.is_empty());

    core.end_active_timing().unwrap();
    assert_eq!(core.timing_state, BurstTimingState::Idle);
    assert_eq!(core.shr_locks, 0);
    assert_eq!(core.shr_rejections, 1);
    assert_eq!(core.markers, 0);
    assert_eq!(core.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(core.shr_rate_selections, [None; MAX_SELECTION_RECORDS]);
    assert!(core.pending_selection.is_none());
    assert!(core.shr_metrics.is_empty());
}

#[test]
fn early_end_and_eof_reject_once_without_output() {
    for trailing_idle in [0, 256] {
        let input = synthetic_input(100, trailing_idle);
        let mut core = BurstClockCore::with_shr_initializer();
        let mut output = Vec::new();
        for sample in input {
            while core.has_pending() {
                if let Some(chip) = core.take_ready() {
                    output.push(chip);
                } else if core.timing_state == BurstTimingState::Selected
                    && !core.shr_metrics.is_empty()
                {
                    core.advance_metric_replay().unwrap();
                } else {
                    core.advance_replay().unwrap();
                }
            }
            core.accept_source(sample).unwrap();
        }
        while core.has_pending() {
            if let Some(chip) = core.take_ready() {
                output.push(chip);
            } else if core.timing_state == BurstTimingState::Selected
                && !core.shr_metrics.is_empty()
            {
                core.advance_metric_replay().unwrap();
            } else {
                core.advance_replay().unwrap();
            }
        }
        core.finish().unwrap();
        assert!(output.is_empty());
        assert_eq!(core.starts, 1);
        assert_eq!(core.ends, 1);
        assert_eq!(core.shr_locks, 0);
        assert_eq!(core.shr_rejections, 1);
        assert_eq!(core.markers, 0);
        assert_eq!(core.timing_state, BurstTimingState::Idle);
        assert!(core.shr_metrics.is_empty());
    }
}

#[test]
fn shr_rate_early_end_and_eof_reject_once_without_output() {
    for trailing_idle in [0, 256] {
        let input = synthetic_input(100, trailing_idle);
        let mut core = BurstClockCore::with_shr_rate_initializer();
        let mut output = Vec::new();
        for sample in input {
            drain_core(&mut core, &mut output);
            core.accept_source(sample).unwrap();
            drain_core(&mut core, &mut output);
        }
        core.finish().unwrap();
        assert!(output.is_empty());
        assert_eq!(core.starts, 1);
        assert_eq!(core.ends, 1);
        assert_eq!(core.shr_locks, 0);
        assert_eq!(core.shr_rejections, 1);
        assert_eq!(core.markers, 0);
        assert_eq!(core.timing_state, BurstTimingState::Idle);
        assert!(core.shr_metrics.is_empty());
        assert_eq!(core.shr_rate_selections, [None; MAX_SELECTION_RECORDS]);
    }
}

#[test]
fn shr_chunking_and_output_backpressure_preserve_selected_stream() {
    let input = synthetic_shr_input();
    let (expected, expected_stats) = run_core_chunks_with(
        BurstClockCore::with_shr_initializer(),
        &input,
        &[input.len()],
    );
    let (chunked, chunked_stats) = run_core_chunks_with(
        BurstClockCore::with_shr_initializer(),
        &input,
        &[1, 7, 64, 511, 3, 2_048],
    );
    let (one_item, one_item_stats) = run_shr_mock_with_capacity(&input, 1);
    let (large, large_stats) = run_shr_mock_with_capacity(&input, 65_536);

    assert_eq!(chunked, expected);
    assert_eq!(one_item, expected);
    assert_eq!(large, expected);
    assert_eq!(chunked_stats, expected_stats);
    assert_eq!(one_item_stats, expected_stats);
    assert_eq!(large_stats, expected_stats);
    assert_eq!(expected_stats.starts, 1);
    assert_eq!(expected_stats.ends, 1);
    assert_eq!(expected_stats.markers, 1);
    assert_eq!(expected_stats.shr_locks, 1);
    assert_eq!(expected_stats.shr_rejections, 0);
    assert_eq!(expected_stats.shr_selection_overflow, 0);
    assert_eq!(expected_stats.max_pretrigger, PRETRIGGER_CAPACITY);
    assert_eq!(expected_stats.max_replay, PRETRIGGER_CAPACITY);
    assert_eq!(expected_stats.max_metrics, MAX_METRIC_QUEUE);
    assert_eq!(
        expected_stats.max_shr_selection_metrics,
        SHR_SELECTION_METRICS
    );
    let selection = expected_stats.shr_selections[0].expect("synthetic SHR must lock");
    assert_eq!(expected_stats.shr_selections[1], None);
    assert_eq!(
        expected_stats.max_shr_metric_replay,
        SHR_SELECTION_METRICS - usize::from(selection.start_offset)
    );
    assert_eq!(expected.iter().filter(|chip| chip.burst_start).count(), 1);
    assert!(expected.iter().all(|chip| chip.payload_length.is_none()));
    assert_eq!(
        expected[0].source_sample_index,
        selection.source_sample_index
    );
    assert!(
        expected
            .windows(2)
            .all(|pair| pair[0].source_sample_index < pair[1].source_sample_index)
    );
}

#[test]
fn shr_rate_chunking_and_output_backpressure_preserve_selected_stream() {
    let input = synthetic_shr_input();
    let (expected, expected_stats) = run_shr_rate_core_chunks_with(
        BurstClockCore::with_shr_rate_initializer(),
        &input,
        &[input.len()],
    );
    let (chunked, chunked_stats) = run_shr_rate_core_chunks_with(
        BurstClockCore::with_shr_rate_initializer(),
        &input,
        &[1, 7, 64, 511, 3, 2_048],
    );
    let (one_item, one_item_stats) = run_shr_rate_mock_with_capacity(&input, 1);
    let (large, large_stats) = run_shr_rate_mock_with_capacity(&input, 65_536);

    assert_eq!(chunked, expected);
    assert_eq!(one_item, expected);
    assert_eq!(large, expected);
    assert_eq!(chunked_stats, expected_stats);
    assert_eq!(one_item_stats, expected_stats);
    assert_eq!(large_stats, expected_stats);
    let burst = expected_stats.burst;
    assert_eq!(burst.starts, 1);
    assert_eq!(burst.ends, 1);
    assert_eq!(burst.markers, 1);
    assert_eq!(burst.shr_locks, 1);
    assert_eq!(burst.shr_rejections, 0);
    assert_eq!(burst.shr_selection_overflow, 0);
    assert_eq!(burst.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(burst.max_pretrigger, PRETRIGGER_CAPACITY);
    assert_eq!(burst.max_replay, PRETRIGGER_CAPACITY);
    assert_eq!(burst.max_metrics, MAX_METRIC_QUEUE);
    assert_eq!(burst.max_shr_selection_metrics, SHR_RATE_SELECTION_METRICS);
    let selection = expected_stats.shr_rate_selections[0]
        .expect("synthetic SHR must produce a joint timing lock");
    assert_eq!(expected_stats.shr_rate_selections[1], None);
    assert!(
        SHR_RATE_HYPOTHESES_PPM.contains(&selection.rate_error_ppm),
        "joint selector returned a rate outside its frozen grid"
    );
    assert_eq!(
        burst.max_shr_metric_replay,
        SHR_RATE_SELECTION_METRICS - usize::from(selection.timing.start_offset)
    );
    assert_eq!(
        expected_stats.shr_rate_selections.iter().flatten().count() as u64
            + burst.shr_selection_overflow,
        burst.shr_locks
    );
    assert_eq!(expected.iter().filter(|chip| chip.burst_start).count(), 1);
    assert!(expected.iter().all(|chip| chip.payload_length.is_none()));
    assert_eq!(
        expected[0].source_sample_index,
        selection.timing.source_sample_index
    );
    assert!(
        expected
            .windows(2)
            .all(|pair| pair[0].source_sample_index < pair[1].source_sample_index)
    );
}

#[test]
fn deferred_chunking_backpressure_and_two_events_preserve_payload_stream() {
    let mut input = synthetic_deferred_input(5);
    input.extend(synthetic_deferred_input(127));
    let (expected, expected_stats) = run_deferred_core_chunks(&input, &[input.len()]);
    let (chunked, chunked_stats) = run_deferred_core_chunks(&input, &[1, 7, 64, 511, 3, 2_048]);
    let (one_item, one_item_stats) = run_deferred_mock_with_capacity(&input, 1);
    let (large, large_stats) = run_deferred_mock_with_capacity(&input, 65_536);

    assert_eq!(chunked, expected);
    assert_eq!(one_item, expected);
    assert_eq!(large, expected);
    assert_eq!(chunked_stats, expected_stats);
    assert_eq!(one_item_stats, expected_stats);
    assert_eq!(large_stats, expected_stats);
    let burst = expected_stats.burst;
    assert_eq!(burst.starts, 2);
    assert_eq!(burst.ends, 2);
    assert_eq!(burst.markers, 2);
    assert_eq!(burst.shr_locks, 2);
    assert_eq!(burst.shr_rejections, 0);
    assert_eq!(burst.shr_selection_overflow, 0);
    assert_eq!(burst.shr_selections, [None; MAX_SELECTION_RECORDS]);
    assert_eq!(burst.max_pretrigger, PRETRIGGER_CAPACITY);
    assert_eq!(burst.max_replay, PRETRIGGER_CAPACITY);
    assert_eq!(burst.max_metrics, MAX_METRIC_QUEUE);
    assert_eq!(burst.max_shr_selection_metrics, DEFERRED_SELECTION_METRICS);
    assert!(burst.max_shr_metric_replay <= MAX_DEFERRED_HANDOFF_TAIL_METRICS);
    assert_eq!(
        expected_stats
            .deferred_selections
            .iter()
            .flatten()
            .map(|handoff| handoff.phr_length)
            .collect::<Vec<_>>(),
        [5, 127]
    );
    assert_eq!(
        expected
            .iter()
            .filter_map(|chip| chip.payload_length)
            .collect::<Vec<_>>(),
        [5, 127]
    );
    assert!(
        expected
            .iter()
            .all(|chip| { chip.payload_length.is_none() || chip.burst_start })
    );
    assert!(
        expected
            .windows(2)
            .all(|pair| { pair[0].source_sample_index < pair[1].source_sample_index })
    );
    for (chip, handoff) in expected
        .iter()
        .filter(|chip| chip.burst_start)
        .zip(expected_stats.deferred_selections.iter().flatten())
    {
        assert_eq!(chip.payload_length, Some(handoff.phr_length));
        assert_eq!(
            chip.source_sample_index,
            handoff.payload_source_sample_index
        );
    }
}

#[test]
fn shr_rate_diagnostic_is_passive_chunk_stable_and_published_once() {
    let input = synthetic_shr_input();
    let (expected, expected_stats) = run_shr_rate_core_chunks_with(
        BurstClockCore::with_shr_rate_initializer(),
        &input,
        &[input.len()],
    );
    let (contiguous, contiguous_stats, contiguous_trace) =
        run_shr_rate_diagnostic_core_chunks(&input, &[input.len()]);
    let (chunked, chunked_stats, chunked_trace) =
        run_shr_rate_diagnostic_core_chunks(&input, &[1, 7, 64, 511, 3, 2_048]);
    let (one_item, one_item_stats, one_item_trace) =
        run_shr_rate_diagnostic_mock_with_capacity(&input, 1);
    let (large, large_stats, large_trace) =
        run_shr_rate_diagnostic_mock_with_capacity(&input, 65_536);

    assert_eq!(contiguous, expected);
    assert_eq!(chunked, expected);
    assert_eq!(one_item, expected);
    assert_eq!(large, expected);
    assert_eq!(contiguous_stats, expected_stats);
    assert_eq!(chunked_stats, expected_stats);
    assert_eq!(one_item_stats, expected_stats);
    assert_eq!(large_stats, expected_stats);
    assert_eq!(chunked_trace, contiguous_trace);
    assert_eq!(one_item_trace, contiguous_trace);
    assert_eq!(large_trace, contiguous_trace);
    assert_eq!(contiguous_trace.overflow, 0);
    let event = contiguous_trace.events[0]
        .as_ref()
        .expect("synthetic SHR must produce one diagnostic event");
    assert_eq!(contiguous_trace.events[1], None);
    let selection = expected_stats.shr_rate_selections[0]
        .expect("synthetic SHR must produce one joint selection");
    assert_eq!(event.outcome, ShrRateTraceOutcome::Locked(selection));
    let audit = event
        .selector
        .expect("completed selector must publish its independent audit");
    assert_eq!(audit.evaluated_hypotheses, SHR_RATE_HYPOTHESIS_COUNT);
    assert_eq!(audit.best_eligible.unwrap().selection, selection);
    let clock = event
        .clock
        .as_ref()
        .expect("locked event must publish its clock trajectory");
    assert_eq!(clock.emitted_chips, expected.len() as u64);
    assert_eq!(clock.markers, 1);
    assert_eq!(
        clock.first_source_sample_index,
        Some(expected[0].source_sample_index)
    );
    assert_eq!(
        clock.last_source_sample_index,
        expected.last().map(|chip| chip.source_sample_index)
    );
    assert_eq!(
        clock.advance_histogram.iter().sum::<u64>(),
        clock.emitted_chips
    );
    assert_eq!(clock.advance_overflow, 0);
    assert_eq!(clock.boundary_overflow, 0);
    assert_eq!(
        clock.boundaries.len(),
        (clock.emitted_chips / CLOCK_BOUNDARY_INTERVAL) as usize
    );
}

#[test]
fn shr_rate_diagnostic_classifies_early_rejection_without_output() {
    for trailing_idle in [0, 256] {
        let input = synthetic_input(100, trailing_idle);
        let (output, stats, trace) = run_shr_rate_diagnostic_core_chunks(&input, &[1, 17, 64, 511]);
        assert!(output.is_empty());
        assert_eq!(stats.burst.shr_locks, 0);
        assert_eq!(stats.burst.shr_rejections, 1);
        assert_eq!(trace.overflow, 0);
        let event = trace.events[0]
            .as_ref()
            .expect("early burst must retain one diagnostic event");
        assert_eq!(event.outcome, ShrRateTraceOutcome::RejectedBeforeSelection);
        assert_eq!(event.selector, None);
        assert_eq!(event.clock, None);
        assert_eq!(trace.events[1], None);
    }
}

#[test]
fn chunking_and_output_backpressure_preserve_positioned_chips() {
    let input = synthetic_input(1_000, 256);
    let expected = run_core_chunks(&input, &[input.len()]);
    assert_eq!(expected, run_core_chunks(&input, &[1, 7, 64, 511, 3]));
    assert_eq!(expected, run_mock_with_capacity(&input, 1));
    assert_eq!(expected, run_mock_with_capacity(&input, 65_536));
    assert_eq!(expected.iter().filter(|chip| chip.burst_start).count(), 1);
    assert!(expected.iter().all(|chip| chip.payload_length.is_none()));
    assert!(
        expected
            .windows(2)
            .all(|pair| pair[0].source_sample_index < pair[1].source_sample_index)
    );
}

#[test]
fn each_burst_matches_a_fresh_acquisition_and_marks_once() {
    let first = synthetic_input(1_000, 256);
    let second = synthetic_input(2_000, 256);
    let mut combined = first.clone();
    combined.extend_from_slice(&second);
    let combined_output = run_core_chunks(&combined, &[17, 2_048, 5]);
    let marker_indices = combined_output
        .iter()
        .enumerate()
        .filter_map(|(index, chip)| chip.burst_start.then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(marker_indices.len(), 2);

    let first_output = run_core_chunks(&first, &[first.len()]);
    let second_output = run_core_chunks(&second, &[second.len()]);
    let split = marker_indices[1];
    assert_eq!(&combined_output[..split], first_output);
    let second_offset = first.len() as u64;
    assert_eq!(combined_output[split..].len(), second_output.len());
    for (combined_chip, fresh_chip) in combined_output[split..].iter().zip(second_output) {
        assert_eq!(combined_chip.metric, fresh_chip.metric);
        assert_eq!(
            combined_chip.source_sample_index,
            fresh_chip.source_sample_index + second_offset
        );
        assert_eq!(combined_chip.burst_start, fresh_chip.burst_start);
        assert_eq!(combined_chip.payload_length, fresh_chip.payload_length);
        assert!(combined_chip.payload_length.is_none());
    }
}

#[test]
fn detected_span_matches_the_independent_pinned_equations_exactly() {
    let input = synthetic_input(2_000, 256);
    let mut detector = CausalBurstDetector::new();
    let mut events = Vec::new();
    for sample in &input {
        events.extend(detector.push(*sample).unwrap());
    }
    events.extend(detector.finish());
    assert_eq!(events.len(), 1);
    let event = events[0];
    let start = usize::try_from(event.replay_start).unwrap();
    let end = usize::try_from(event.end_exclusive).unwrap();
    let expected = reference_fresh_span(&input[start..end], event.replay_start);
    let observed = run_core_chunks(&input, &[19, 257, 4_096]);
    assert_eq!(observed, expected);
}

#[test]
fn active_eof_finishes_once_and_maximum_burst_stays_inhibited() {
    let eof_input = synthetic_input(1_000, 0);
    let eof_output = run_core_chunks(&eof_input, &[13, 29]);
    assert_eq!(eof_output.iter().filter(|chip| chip.burst_start).count(), 1);

    let long_input = synthetic_input(30_000, 0);
    let long_output = run_core_chunks(&long_input, &[257, 1_024]);
    assert_eq!(
        long_output.iter().filter(|chip| chip.burst_start).count(),
        1
    );
}
