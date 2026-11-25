use super::super::burst_clock::{
    DeferredShrRateCandidateSummary, DeferredShrRateFirstFailureHistogram,
    DeferredShrRateLateStartAudit, DeferredShrRatePostScoreFirstFailureHistogram,
    DeferredShrRateScoreCounterfactualAudit, DeferredShrRateSelectorAudit,
    diagnose_deferred_shr_rate_score_counterfactual, diagnose_deferred_shr_rate_selection,
    diagnose_rejected_deferred_shr_rate_late_starts,
};
use super::super::burst_detector::{DetectedBurst, PRETRIGGER_SAMPLES};
use super::super::soft_decoder::UNIQUE_ANGULAR_CAP;
use super::{
    ACK_PSDU, CLEAN_NEGATIVE_HALF_PHASE_EVENTS, CLEAN_NEGATIVE_HALF_PHASE_INDEX,
    CLEAN_NEGATIVE_HALF_PHASE_SHA256, CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS,
    CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS, CLEAN_ZERO_PHASE_EVENTS, TIMING_DEVELOPMENT_OFFSETS,
    TIMING_OFFSET_DENOMINATOR, TIMING_RATE_ERROR_PPM, append_cf32_le, detect_causal_bursts,
    fractional_timing_samples, longer_valid_psdu, sha256_cf32_le, shr_rate_selection,
    upstream_waveform,
};

const CLEAN_ZERO_PHASE_INDEX: usize = 4;
const CLEAN_ZERO_PHASE_SIXTEENTHS: i32 = 0;

const CLEAN_ZERO_PHASE_SELECTOR_HISTOGRAM: DeferredShrRateFirstFailureHistogram =
    DeferredShrRateFirstFailureHistogram {
        interpolation_nonfinite: 0,
        correlation_undefined: 0,
        score_at_or_below_cap: 2_977,
        shr_unclassified: [0; 10],
        shr_wrong: [0; 10],
        phr_low_unclassified: 0,
        phr_high_unclassified: 0,
        length_zero: 0,
        length_oversize: 0,
        eligible: 95,
    };
const CLEAN_ZERO_PHASE_SELECTOR_AUDITS: [DeferredShrRateSelectorAudit; 2] = [
    DeferredShrRateSelectorAudit {
        evaluated_hypotheses: 3_072,
        first_failures: CLEAN_ZERO_PHASE_SELECTOR_HISTOGRAM,
        best_defined: Some(DeferredShrRateCandidateSummary {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[0],
            full_score_bits: 0x3fef_ffcf_4b2e_9477,
        }),
        accepted_winner: Some(DeferredShrRateCandidateSummary {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[0],
            full_score_bits: 0x3fef_ffcf_4b2e_9477,
        }),
    },
    DeferredShrRateSelectorAudit {
        evaluated_hypotheses: 3_072,
        first_failures: CLEAN_ZERO_PHASE_SELECTOR_HISTOGRAM,
        best_defined: Some(DeferredShrRateCandidateSummary {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[1],
            full_score_bits: 0x3fef_ffcf_4b2e_9477,
        }),
        accepted_winner: Some(DeferredShrRateCandidateSummary {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[1],
            full_score_bits: 0x3fef_ffcf_4b2e_9477,
        }),
    },
];
const CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_HISTOGRAM: DeferredShrRateFirstFailureHistogram =
    DeferredShrRateFirstFailureHistogram {
        interpolation_nonfinite: 0,
        correlation_undefined: 0,
        score_at_or_below_cap: 3_072,
        shr_unclassified: [0; 10],
        shr_wrong: [0; 10],
        phr_low_unclassified: 0,
        phr_high_unclassified: 0,
        length_zero: 0,
        length_oversize: 0,
        eligible: 0,
    };
const CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS: [DeferredShrRateSelectorAudit; 2] = [
    DeferredShrRateSelectorAudit {
        evaluated_hypotheses: 3_072,
        first_failures: CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_HISTOGRAM,
        best_defined: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(11, 14, 39_936, 100),
            full_score_bits: 0x3fe4_6b55_fcdf_84f4,
        }),
        accepted_winner: None,
    },
    DeferredShrRateSelectorAudit {
        evaluated_hypotheses: 3_072,
        first_failures: CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_HISTOGRAM,
        best_defined: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(11, 14, 121_346, 100),
            full_score_bits: 0x3fe4_6b55_fd37_5a19,
        }),
        accepted_winner: None,
    },
];
const CLEAN_NEGATIVE_HALF_PHASE_LATE_START_HISTOGRAM: DeferredShrRateFirstFailureHistogram =
    DeferredShrRateFirstFailureHistogram {
        interpolation_nonfinite: 0,
        correlation_undefined: 0,
        score_at_or_below_cap: 677,
        shr_unclassified: [0; 10],
        shr_wrong: [0; 10],
        phr_low_unclassified: 0,
        phr_high_unclassified: 0,
        length_zero: 0,
        length_oversize: 0,
        eligible: 91,
    };
const CLEAN_NEGATIVE_HALF_PHASE_LATE_START_AUDITS: [DeferredShrRateLateStartAudit; 2] = [
    DeferredShrRateLateStartAudit {
        evaluated_hypotheses: 768,
        first_failures: CLEAN_NEGATIVE_HALF_PHASE_LATE_START_HISTOGRAM,
        best_eligible: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(75, 13, 40_000, 0),
            full_score_bits: 0x3fef_d7f9_62cd_ce81,
        }),
    },
    DeferredShrRateLateStartAudit {
        evaluated_hypotheses: 768,
        first_failures: CLEAN_NEGATIVE_HALF_PHASE_LATE_START_HISTOGRAM,
        best_eligible: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(75, 13, 121_410, 0),
            full_score_bits: 0x3fef_d7dc_c129_19ff,
        }),
    },
];
const CLEAN_ZERO_PHASE_SCORE_COUNTERFACTUALS: [DeferredShrRateScoreCounterfactualAudit; 2] = [
    DeferredShrRateScoreCounterfactualAudit {
        evaluated_hypotheses: 3_072,
        score_rejected_hypotheses: 2_977,
        first_failures: DeferredShrRatePostScoreFirstFailureHistogram {
            shr_unclassified: [345, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            shr_wrong: [2_502, 89, 0, 0, 0, 0, 0, 0, 4, 0],
            phr_low_unclassified: 0,
            phr_high_unclassified: 0,
            length_zero: 0,
            length_oversize: 0,
            eligible: 37,
        },
        best_structurally_eligible: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(59, 8, 40_000, 0),
            full_score_bits: 0x3fe8_02f3_40d8_e9ce,
        }),
    },
    DeferredShrRateScoreCounterfactualAudit {
        evaluated_hypotheses: 3_072,
        score_rejected_hypotheses: 2_977,
        first_failures: DeferredShrRatePostScoreFirstFailureHistogram {
            shr_unclassified: [345, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            shr_wrong: [2_502, 89, 0, 0, 0, 0, 0, 0, 4, 0],
            phr_low_unclassified: 0,
            phr_high_unclassified: 0,
            length_zero: 0,
            length_oversize: 1,
            eligible: 36,
        },
        best_structurally_eligible: Some(DeferredShrRateCandidateSummary {
            selection: shr_rate_selection(59, 8, 121_410, 0),
            full_score_bits: 0x3fe8_02f3_40d8_e9ce,
        }),
    },
];
const CLEAN_NEGATIVE_HALF_PHASE_SCORE_COUNTERFACTUALS: [DeferredShrRateScoreCounterfactualAudit;
    2] = [DeferredShrRateScoreCounterfactualAudit {
    evaluated_hypotheses: 3_072,
    score_rejected_hypotheses: 3_072,
    first_failures: DeferredShrRatePostScoreFirstFailureHistogram {
        shr_unclassified: [411, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        shr_wrong: [2_481, 180, 0, 0, 0, 0, 0, 0, 0, 0],
        phr_low_unclassified: 0,
        phr_high_unclassified: 0,
        length_zero: 0,
        length_oversize: 0,
        eligible: 0,
    },
    best_structurally_eligible: None,
}; 2];

fn assert_deferred_score_counterfactual_bounds(
    counterfactual: &DeferredShrRateScoreCounterfactualAudit,
    authoritative: &DeferredShrRateSelectorAudit,
    event: DetectedBurst,
) {
    assert_eq!(counterfactual.evaluated_hypotheses, 3_072);
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
    assert!(counterfactual.score_rejected_hypotheses <= counterfactual.evaluated_hypotheses);
    assert_eq!(
        counterfactual.best_structurally_eligible.is_some(),
        counterfactual.first_failures.eligible != 0
    );
    if let Some(best) = counterfactual.best_structurally_eligible {
        let score = f64::from_bits(best.full_score_bits);
        assert!(score.is_finite() && score <= UNIQUE_ANGULAR_CAP);
        assert!(u64::from(best.selection.timing.start_offset) < PRETRIGGER_SAMPLES);
        assert!(usize::from(best.selection.timing.mu_sixteenths) < 16);
        assert!(TIMING_RATE_ERROR_PPM.contains(&best.selection.rate_error_ppm));
        assert_eq!(
            best.selection.timing.source_sample_index,
            event
                .replay_start
                .checked_add(u64::from(best.selection.timing.start_offset))
                .expect("bounded counterfactual source coordinate")
        );
    }
}

fn assert_deferred_late_start_bounds(audit: &DeferredShrRateLateStartAudit, event: DetectedBurst) {
    assert_eq!(audit.evaluated_hypotheses, 768);
    assert_eq!(
        audit.first_failures.checked_total(),
        Some(audit.evaluated_hypotheses)
    );
    assert_eq!(
        audit.best_eligible.is_some(),
        audit.first_failures.eligible != 0
    );
    if let Some(best) = audit.best_eligible {
        let start_offset = u64::from(best.selection.timing.start_offset);
        assert!((PRETRIGGER_SAMPLES..PRETRIGGER_SAMPLES + 16).contains(&start_offset));
        assert!(usize::from(best.selection.timing.mu_sixteenths) < 16);
        assert!(TIMING_RATE_ERROR_PPM.contains(&best.selection.rate_error_ppm));
        let score = f64::from_bits(best.full_score_bits);
        assert!(score.is_finite() && score > UNIQUE_ANGULAR_CAP);
        assert_eq!(
            best.selection.timing.source_sample_index,
            event
                .replay_start
                .checked_add(start_offset)
                .expect("bounded late-start source coordinate")
        );
    }
}

pub(super) fn clean_zero_phase_selector_positive_control_case() {
    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_ZERO_PHASE_INDEX],
        CLEAN_ZERO_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert_eq!(UNIQUE_ANGULAR_CAP.to_bits(), 0x3fe8_c97e_f43f_7248);

    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let long_waveform = upstream_waveform(&longer_psdu);
    let mut raw_samples = short_waveform.clone();
    raw_samples.extend_from_slice(&long_waveform);
    let samples =
        fractional_timing_samples(&short_waveform, &long_waveform, CLEAN_ZERO_PHASE_SIXTEENTHS);
    let mut raw_bytes = Vec::new();
    append_cf32_le(&mut raw_bytes, &raw_samples);
    let mut zero_phase_bytes = Vec::new();
    append_cf32_le(&mut zero_phase_bytes, &samples);
    assert_eq!(zero_phase_bytes, raw_bytes);
    assert_eq!(
        detect_causal_bursts(&samples).as_slice(),
        &CLEAN_ZERO_PHASE_EVENTS
    );

    // This fixture-dependent selector control does not compare or expose PSDU
    // identity; the selected rate is not oscillator calibration.
    let first = diagnose_deferred_shr_rate_selection(&samples, CLEAN_ZERO_PHASE_EVENTS[0])
        .expect("audit first clean zero-phase deferred selector event");
    assert_eq!(first.evaluated_hypotheses, 3_072);
    assert_eq!(
        first.first_failures.checked_total(),
        Some(first.evaluated_hypotheses)
    );
    let first_winner = first
        .accepted_winner
        .expect("first clean zero-phase event must pass deferred selection");
    assert_eq!(
        first_winner.selection,
        CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[0]
    );
    assert!(f64::from_bits(first_winner.full_score_bits) > UNIQUE_ANGULAR_CAP);
    assert_eq!(first, CLEAN_ZERO_PHASE_SELECTOR_AUDITS[0]);

    let second = diagnose_deferred_shr_rate_selection(&samples, CLEAN_ZERO_PHASE_EVENTS[1])
        .expect("audit second clean zero-phase deferred selector event");
    assert_eq!(second.evaluated_hypotheses, 3_072);
    assert_eq!(
        second.first_failures.checked_total(),
        Some(second.evaluated_hypotheses)
    );
    let second_winner = second
        .accepted_winner
        .expect("second clean zero-phase event must pass deferred selection");
    assert_eq!(
        second_winner.selection,
        CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[1]
    );
    assert!(f64::from_bits(second_winner.full_score_bits) > UNIQUE_ANGULAR_CAP);
    assert_eq!(second, CLEAN_ZERO_PHASE_SELECTOR_AUDITS[1]);

    // All 95 above-cap hypotheses pass the later structural predicates. The
    // remaining 2,977 short-circuit at the cap and are not evaluated further.
    assert_eq!(CLEAN_ZERO_PHASE_SELECTOR_HISTOGRAM.eligible, 95);
}

pub(super) fn clean_negative_half_phase_selector_rejection_case() {
    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert_eq!(UNIQUE_ANGULAR_CAP.to_bits(), 0x3fe8_c97e_f43f_7248);

    let longer_psdu = longer_valid_psdu();
    let samples = fractional_timing_samples(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS,
    );
    assert_eq!(sha256_cf32_le(&samples), CLEAN_NEGATIVE_HALF_PHASE_SHA256);
    assert_eq!(
        detect_causal_bursts(&samples).as_slice(),
        &CLEAN_NEGATIVE_HALF_PHASE_EVENTS
    );

    // This fixture-dependent selector instrumentation does not compare or
    // expose PSDU identity; selected rates are not oscillator calibration.
    let first = diagnose_deferred_shr_rate_selection(&samples, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0])
        .expect("audit first frozen deferred selector event");
    assert_eq!(first.evaluated_hypotheses, 3_072);
    assert_eq!(
        first.first_failures.checked_total(),
        Some(first.evaluated_hypotheses)
    );
    assert_eq!(first.accepted_winner, None);
    assert_eq!(first, CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[0]);

    let second =
        diagnose_deferred_shr_rate_selection(&samples, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1])
            .expect("audit second frozen deferred selector event");
    assert_eq!(second.evaluated_hypotheses, 3_072);
    assert_eq!(
        second.first_failures.checked_total(),
        Some(second.evaluated_hypotheses)
    );
    assert_eq!(second.accepted_winner, None);
    assert_eq!(second, CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[1]);

    // Every hypothesis stops at the frozen aggregate score gate; no SHR
    // symbol, PHR, or length predicate is reached on this fixture.
    assert!(
        CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS
            .iter()
            .all(|audit| {
                audit
                    .best_defined
                    .is_some_and(|best| f64::from_bits(best.full_score_bits) <= UNIQUE_ANGULAR_CAP)
            })
    );
}

pub(super) fn clean_negative_half_phase_late_start_observation_case() {
    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert_eq!(UNIQUE_ANGULAR_CAP.to_bits(), 0x3fe8_c97e_f43f_7248);

    let longer_psdu = longer_valid_psdu();
    let samples = fractional_timing_samples(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS,
    );
    assert_eq!(sha256_cf32_le(&samples), CLEAN_NEGATIVE_HALF_PHASE_SHA256);
    assert_eq!(
        detect_causal_bursts(&samples).as_slice(),
        &CLEAN_NEGATIVE_HALF_PHASE_EVENTS
    );

    let authoritative_first =
        diagnose_deferred_shr_rate_selection(&samples, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0])
            .expect("audit first clean -8/16 authoritative selector event");
    assert_eq!(
        authoritative_first,
        CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[0]
    );
    assert_eq!(authoritative_first.accepted_winner, None);
    let authoritative_second =
        diagnose_deferred_shr_rate_selection(&samples, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1])
            .expect("audit second clean -8/16 authoritative selector event");
    assert_eq!(
        authoritative_second,
        CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[1]
    );
    assert_eq!(authoritative_second.accepted_winner, None);

    let late_first = diagnose_rejected_deferred_shr_rate_late_starts(
        &samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0],
    )
    .expect("audit first clean -8/16 deferred late-start event");
    let late_second = diagnose_rejected_deferred_shr_rate_late_starts(
        &samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1],
    )
    .expect("audit second clean -8/16 deferred late-start event");
    assert_deferred_late_start_bounds(&late_first, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0]);
    assert_deferred_late_start_bounds(&late_second, CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1]);
    let late_audits = [late_first, late_second];
    assert_eq!(late_audits, CLEAN_NEGATIVE_HALF_PHASE_LATE_START_AUDITS);
}

pub(super) fn score_counterfactual_zero_then_negative_half_phase_case() {
    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_ZERO_PHASE_INDEX],
        CLEAN_ZERO_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert_eq!(UNIQUE_ANGULAR_CAP.to_bits(), 0x3fe8_c97e_f43f_7248);

    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let long_waveform = upstream_waveform(&longer_psdu);
    let mut raw_samples = short_waveform.clone();
    raw_samples.extend_from_slice(&long_waveform);
    let zero_phase_samples =
        fractional_timing_samples(&short_waveform, &long_waveform, CLEAN_ZERO_PHASE_SIXTEENTHS);
    let mut raw_bytes = Vec::new();
    append_cf32_le(&mut raw_bytes, &raw_samples);
    let mut zero_phase_bytes = Vec::new();
    append_cf32_le(&mut zero_phase_bytes, &zero_phase_samples);
    assert_eq!(zero_phase_bytes, raw_bytes);
    assert_eq!(
        detect_causal_bursts(&zero_phase_samples).as_slice(),
        &CLEAN_ZERO_PHASE_EVENTS
    );

    let zero_authoritative_first =
        diagnose_deferred_shr_rate_selection(&zero_phase_samples, CLEAN_ZERO_PHASE_EVENTS[0])
            .expect("audit first clean zero-phase authoritative selector event");
    assert_eq!(
        zero_authoritative_first,
        CLEAN_ZERO_PHASE_SELECTOR_AUDITS[0]
    );
    let zero_authoritative_second =
        diagnose_deferred_shr_rate_selection(&zero_phase_samples, CLEAN_ZERO_PHASE_EVENTS[1])
            .expect("audit second clean zero-phase authoritative selector event");
    assert_eq!(
        zero_authoritative_second,
        CLEAN_ZERO_PHASE_SELECTOR_AUDITS[1]
    );

    // This diagnostic compares no PSDU identity and exposes no decoded PHR,
    // payload, FCS, or raw selector metrics.
    let zero_counterfactual_first = diagnose_deferred_shr_rate_score_counterfactual(
        &zero_phase_samples,
        CLEAN_ZERO_PHASE_EVENTS[0],
    )
    .expect("audit first clean zero-phase score counterfactual");
    assert_deferred_score_counterfactual_bounds(
        &zero_counterfactual_first,
        &zero_authoritative_first,
        CLEAN_ZERO_PHASE_EVENTS[0],
    );
    assert!(zero_counterfactual_first.first_failures.eligible > 0);
    let zero_counterfactual_second = diagnose_deferred_shr_rate_score_counterfactual(
        &zero_phase_samples,
        CLEAN_ZERO_PHASE_EVENTS[1],
    )
    .expect("audit second clean zero-phase score counterfactual");
    assert_deferred_score_counterfactual_bounds(
        &zero_counterfactual_second,
        &zero_authoritative_second,
        CLEAN_ZERO_PHASE_EVENTS[1],
    );
    assert!(zero_counterfactual_second.first_failures.eligible > 0);
    let zero_counterfactuals = [zero_counterfactual_first, zero_counterfactual_second];
    assert_eq!(zero_counterfactuals, CLEAN_ZERO_PHASE_SCORE_COUNTERFACTUALS);

    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS
    );
    let negative_half_phase_samples = fractional_timing_samples(
        &short_waveform,
        &long_waveform,
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS,
    );
    assert_eq!(
        sha256_cf32_le(&negative_half_phase_samples),
        CLEAN_NEGATIVE_HALF_PHASE_SHA256
    );
    assert_eq!(
        detect_causal_bursts(&negative_half_phase_samples).as_slice(),
        &CLEAN_NEGATIVE_HALF_PHASE_EVENTS
    );

    let negative_authoritative_first = diagnose_deferred_shr_rate_selection(
        &negative_half_phase_samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0],
    )
    .expect("audit first clean -8/16 authoritative selector event");
    assert_eq!(
        negative_authoritative_first,
        CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[0]
    );
    let negative_authoritative_second = diagnose_deferred_shr_rate_selection(
        &negative_half_phase_samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1],
    )
    .expect("audit second clean -8/16 authoritative selector event");
    assert_eq!(
        negative_authoritative_second,
        CLEAN_NEGATIVE_HALF_PHASE_SELECTOR_AUDITS[1]
    );

    let negative_counterfactual_first = diagnose_deferred_shr_rate_score_counterfactual(
        &negative_half_phase_samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0],
    )
    .expect("audit first clean -8/16 score counterfactual");
    assert_deferred_score_counterfactual_bounds(
        &negative_counterfactual_first,
        &negative_authoritative_first,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[0],
    );
    let negative_counterfactual_second = diagnose_deferred_shr_rate_score_counterfactual(
        &negative_half_phase_samples,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1],
    )
    .expect("audit second clean -8/16 score counterfactual");
    assert_deferred_score_counterfactual_bounds(
        &negative_counterfactual_second,
        &negative_authoritative_second,
        CLEAN_NEGATIVE_HALF_PHASE_EVENTS[1],
    );
    let negative_counterfactuals = [
        negative_counterfactual_first,
        negative_counterfactual_second,
    ];
    assert_eq!(
        negative_counterfactuals,
        CLEAN_NEGATIVE_HALF_PHASE_SCORE_COUNTERFACTUALS
    );
}
