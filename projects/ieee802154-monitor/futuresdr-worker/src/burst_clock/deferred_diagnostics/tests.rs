use super::super::{
    CHIP_SEQUENCES, PHASE_IIR_ALPHA, SYMBOL_CHIPS, UNIQUE_ANGULAR_CAP, shr_rate_position,
};
use super::*;

const SYNTHETIC_SOURCE_START: u64 = 40_000;

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
    }
    chips
}

fn synthetic_metrics(
    start_offset: usize,
    mu_sixteenths: usize,
    rate_error_ppm: i32,
    chips: &[f32; DEFERRED_STRUCTURAL_CHIPS],
    distinguish_timing: bool,
) -> Vec<PositionedMetric> {
    assert!(start_offset < DEFERRED_LATE_START_END);
    assert!(mu_sixteenths < SHR_PHASE_HYPOTHESES);
    assert!(SHR_RATE_HYPOTHESES_PPM.contains(&rate_error_ppm));
    let mut values: [f32; DEFERRED_LATE_SELECTION_METRICS] = std::array::from_fn(|index| {
        let pseudonoise = ((index * 29 + 7) % 31) as f32 - 15.0;
        0.07 * pseudonoise
    });
    let mut assigned = [false; DEFERRED_LATE_SELECTION_METRICS];
    for (chip_index, chip) in chips.iter().copied().enumerate() {
        let (left_index, fraction) =
            shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
        let slope = if distinguish_timing {
            let pseudonoise = ((chip_index * 43 + 11) % 67) as f32 - 33.0;
            0.11 * pseudonoise
        } else {
            0.0
        };
        if fraction == 0.0 {
            assert!(!assigned[left_index]);
            values[left_index] = chip;
            assigned[left_index] = true;
            if !assigned[left_index + 1] {
                values[left_index + 1] = chip + slope;
                assigned[left_index + 1] = true;
            }
        } else if assigned[left_index] {
            assert!(!assigned[left_index + 1]);
            values[left_index + 1] = (chip - (1.0 - fraction) * values[left_index]) / fraction;
            assigned[left_index + 1] = true;
        } else {
            assert!(!assigned[left_index + 1]);
            values[left_index] = chip - fraction * slope;
            values[left_index + 1] = chip + (1.0 - fraction) * slope;
            assigned[left_index] = true;
            assigned[left_index + 1] = true;
        }
    }
    let metrics = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| PositionedMetric {
            value,
            source_sample_index: SYNTHETIC_SOURCE_START + index as u64,
        })
        .collect::<Vec<_>>();
    for (chip_index, expected) in chips.iter().copied().enumerate() {
        let actual = interpolated_shr_rate_chip(
            &metrics,
            start_offset,
            mu_sixteenths,
            rate_error_ppm,
            chip_index,
        );
        assert!((actual - expected).abs() < 1.0e-4);
    }
    metrics
}

fn flat_metrics(source_start: u64) -> Vec<PositionedMetric> {
    (0..DEFERRED_LATE_SELECTION_METRICS)
        .map(|offset| PositionedMetric {
            value: 0.0,
            source_sample_index: source_start + offset as u64,
        })
        .collect()
}

fn samples_from_metrics(metrics: &[PositionedMetric]) -> Vec<Complex32> {
    assert_eq!(metrics[0].value.to_bits(), 0.0f32.to_bits());
    let mut phase_iir = 0.0f32;
    let mut accumulated_phase = 0.0f32;
    metrics
        .iter()
        .enumerate()
        .map(|(index, metric)| {
            if index != 0 {
                let phase_difference = phase_iir + metric.value / (1.0 - PHASE_IIR_ALPHA);
                phase_iir =
                    (1.0 - PHASE_IIR_ALPHA) * phase_iir + PHASE_IIR_ALPHA * phase_difference;
                accumulated_phase += phase_difference;
            }
            Complex32::new(accumulated_phase.cos(), accumulated_phase.sin())
        })
        .collect()
}

#[test]
fn late_start_geometry_is_exact_and_bounded() {
    assert_eq!(DEFERRED_LATE_START_BEGIN, 64);
    assert_eq!(DEFERRED_LATE_START_END, 80);
    assert_eq!(DEFERRED_LATE_START_COUNT, 16);
    assert_eq!(DEFERRED_LATE_SELECTION_METRICS, 848);
    assert_eq!(DEFERRED_LATE_HYPOTHESIS_COUNT, 768);

    let mut hypotheses = 0usize;
    for start_offset in DEFERRED_LATE_START_BEGIN..DEFERRED_LATE_START_END {
        for mu_sixteenths in 0..SHR_PHASE_HYPOTHESES {
            for rate_error_ppm in SHR_RATE_HYPOTHESES_PPM {
                for chip_index in 0..DEFERRED_STRUCTURAL_CHIPS {
                    let (left_index, fraction) =
                        shr_rate_position(start_offset, mu_sixteenths, rate_error_ppm, chip_index);
                    assert!(left_index + 1 < DEFERRED_LATE_SELECTION_METRICS);
                    assert!((0.0..1.0).contains(&fraction));
                }
                hypotheses += 1;
            }
        }
    }
    assert_eq!(hypotheses, DEFERRED_LATE_HYPOTHESIS_COUNT as usize);

    let (last_left, last_mu) = shr_rate_position(79, 15, 100, 383);
    assert_eq!((last_left, last_left + 1), (846, 847));
    assert_eq!(last_mu.to_bits(), 0x3c67_03b0);
    let (excluded_left, excluded_mu) = shr_rate_position(80, 15, 100, 383);
    assert_eq!((excluded_left, excluded_left + 1), (847, 848));
    assert_eq!(excluded_mu.to_bits(), last_mu.to_bits());

    let (minimum_payload_left, minimum_payload_mu) =
        shr_rate_position(64, 0, -100, DEFERRED_STRUCTURAL_CHIPS);
    assert_eq!(minimum_payload_left, 831);
    assert_eq!(minimum_payload_mu.to_bits(), 0x3f6c_56d6);
    let (maximum_payload_left, maximum_payload_mu) =
        shr_rate_position(79, 15, 100, DEFERRED_STRUCTURAL_CHIPS);
    assert_eq!(maximum_payload_left, 848);
    assert_eq!(maximum_payload_mu.to_bits(), 0x3c6a_4a8c);
}

#[test]
fn late_start_audit_validates_exact_input() {
    let metrics = flat_metrics(5_000);
    let unchanged = metrics.clone();
    let audit = audit_deferred_shr_rate_late_starts(&metrics).unwrap();
    assert_eq!(metrics, unchanged);
    assert_eq!(audit.evaluated_hypotheses, 768);
    assert_eq!(audit.first_failures.correlation_undefined, 768);
    assert_eq!(audit.first_failures.checked_total(), Some(768));
    assert_eq!(audit.best_eligible, None);

    assert!(audit_deferred_shr_rate_late_starts(&metrics[..847]).is_err());
    let mut extra = metrics.clone();
    extra.push(PositionedMetric {
        value: 0.0,
        source_sample_index: 5_848,
    });
    assert!(audit_deferred_shr_rate_late_starts(&extra).is_err());

    let mut nonfinite = metrics.clone();
    nonfinite[400].value = f32::NAN;
    assert!(audit_deferred_shr_rate_late_starts(&nonfinite).is_err());
    let mut gap = metrics.clone();
    gap[400].source_sample_index += 1;
    assert!(audit_deferred_shr_rate_late_starts(&gap).is_err());
    let mut duplicate = metrics;
    duplicate[400].source_sample_index = duplicate[399].source_sample_index;
    assert!(audit_deferred_shr_rate_late_starts(&duplicate).is_err());

    let max_start = u64::MAX - (DEFERRED_LATE_SELECTION_METRICS as u64 - 1);
    assert!(audit_deferred_shr_rate_late_starts(&flat_metrics(max_start)).is_ok());
}

#[test]
fn late_start_audit_uses_the_authoritative_cap_and_tie_order() {
    let metrics = synthetic_metrics(64, 0, 0, &deferred_template(5), false);
    let audit = audit_deferred_shr_rate_late_starts(&metrics).unwrap();
    assert_eq!(audit.evaluated_hypotheses, 768);
    assert_eq!(audit.first_failures.checked_total(), Some(768));
    assert!(audit.first_failures.eligible > 1);
    assert_eq!(
        audit.best_eligible,
        Some(DeferredShrRateCandidateSummary {
            selection: ShrRateTimingSelection {
                timing: ShrTimingSelection {
                    start_offset: 64,
                    mu_sixteenths: 0,
                    source_sample_index: SYNTHETIC_SOURCE_START + 64,
                },
                rate_error_ppm: 0,
            },
            full_score_bits: 1.0f64.to_bits(),
        })
    );

    let weakened = below_cap_deferred_template(5);
    let weakened_metrics = synthetic_metrics(64, 0, 0, &weakened, false);
    let DeferredStructureOutcome::ScoreAtOrBelowCap { score } =
        deferred_late_start_outcome(&weakened_metrics, 64, 0, 0)
    else {
        panic!("weakened late-start hypothesis must stop at the authoritative score cap");
    };
    assert!(score <= UNIQUE_ANGULAR_CAP);
    let weakened_audit = audit_deferred_shr_rate_late_starts(&weakened_metrics).unwrap();
    assert!(weakened_audit.first_failures.score_at_or_below_cap > 0);
}

#[test]
fn late_start_audit_respects_range_and_phr_boundaries() {
    for (start_offset, mu_sixteenths, rate_error_ppm, phr_length) in
        [(64, 0, 0, 1), (79, 15, 100, 127)]
    {
        let metrics = synthetic_metrics(
            start_offset,
            mu_sixteenths,
            rate_error_ppm,
            &deferred_template(phr_length),
            true,
        );
        assert!(matches!(
            deferred_late_start_outcome(&metrics, start_offset, mu_sixteenths, rate_error_ppm),
            DeferredStructureOutcome::Eligible { .. }
        ));
        let DeferredShrRateLateStartAudit {
            evaluated_hypotheses,
            first_failures,
            best_eligible,
        } = audit_deferred_shr_rate_late_starts(&metrics).unwrap();
        assert_eq!(evaluated_hypotheses, 768);
        assert_eq!(first_failures.checked_total(), Some(768));
        let best = best_eligible.expect("valid endpoint hypothesis must remain eligible");
        assert_eq!(best.selection.timing.start_offset, start_offset as u8);
        assert_eq!(best.selection.timing.mu_sixteenths, mu_sixteenths as u8);
        assert_eq!(best.selection.rate_error_ppm, rate_error_ppm);
        assert_eq!(
            best.selection.timing.source_sample_index,
            SYNTHETIC_SOURCE_START + start_offset as u64
        );
    }

    for (start_offset, phr_length) in [(64, 0), (79, 128)] {
        let metrics = synthetic_metrics(start_offset, 0, 0, &deferred_template(phr_length), true);
        let outcome = deferred_late_start_outcome(&metrics, start_offset, 0, 0);
        if phr_length == 0 {
            assert!(matches!(
                outcome,
                DeferredStructureOutcome::LengthZero { .. }
            ));
        } else {
            assert!(matches!(
                outcome,
                DeferredStructureOutcome::LengthOversize { .. }
            ));
        }
    }
}

#[test]
fn late_start_wrapper_rejects_an_authoritative_winner_before_extended_input() {
    let mut metrics = synthetic_metrics(1, 0, 0, &deferred_template(5), false);
    metrics[0].value = 0.0;
    let samples = samples_from_metrics(&metrics[..DEFERRED_SELECTION_METRICS]);
    let event = DetectedBurst {
        replay_start: 0,
        detected_at: PRETRIGGER_SAMPLES - 1,
        end_exclusive: DEFERRED_SELECTION_METRICS as u64,
        forced_end: false,
    };
    let authoritative = diagnose_deferred_shr_rate_selection(&samples, event).unwrap();
    assert!(authoritative.accepted_winner.is_some());

    let error = diagnose_rejected_deferred_shr_rate_late_starts(&samples, event).unwrap_err();
    assert_eq!(
        error.to_string(),
        "late-start diagnostic requires authoritative 832-metric rejection"
    );
}

#[test]
fn late_start_wrapper_validates_extent_after_authoritative_rejection() {
    let event = DetectedBurst {
        replay_start: 0,
        detected_at: PRETRIGGER_SAMPLES - 1,
        end_exclusive: DEFERRED_LATE_SELECTION_METRICS as u64,
        forced_end: false,
    };
    let short = vec![Complex32::new(1.0, 0.0); DEFERRED_LATE_SELECTION_METRICS - 1];
    let error = diagnose_rejected_deferred_shr_rate_late_starts(&short, event).unwrap_err();
    assert_eq!(
        error.to_string(),
        "deferred late-start diagnostic span exceeded its event or input"
    );

    let exact = vec![Complex32::new(1.0, 0.0); DEFERRED_LATE_SELECTION_METRICS];
    assert_eq!(
        diagnose_rejected_deferred_shr_rate_late_starts(&exact, event).unwrap(),
        DeferredShrRateLateStartAudit {
            evaluated_hypotheses: 768,
            first_failures: DeferredShrRateFirstFailureHistogram {
                correlation_undefined: 768,
                ..DeferredShrRateFirstFailureHistogram::default()
            },
            best_eligible: None,
        }
    );
}
