mod deferred_selector;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use futuresdr::blocks::{FirBuilder, MessagePipe, VectorSink, VectorSource};
use futuresdr::prelude::*;
use futuresdr::runtime::dev::prelude::*;
use ieee802154_monitor::channelize;
use sha2::{Digest, Sha256};
use zigbee::{Decoder, IqDelay, modulator};

use super::burst_clock::{
    BurstClockStats, ClockBoundaryPoint, ClockTrajectoryTrace, DeferredShrRateClockStats,
    DeferredShrRateHandoff, ShrRateClockStats, ShrRateDiagnosticTrace, ShrRateEventTrace,
    ShrRateScoredCandidateTrace, ShrRateSelectorAudit, ShrRateTimingSelection, ShrRateTraceOutcome,
    ShrTimingSelection,
};
use super::burst_detector::{
    BurstStart, CausalBurstDetector, DetectedBurst, DetectorDiagnosticMode,
    DetectorDiagnosticSnapshot, MAX_BURST_SAMPLES, NOISE_WARMUP_SAMPLES, POWER_WINDOW_SAMPLES,
    PRETRIGGER_SAMPLES, RELEASE_PERSISTENCE_SAMPLES, TRIGGER_PERSISTENCE_SAMPLES,
};
use super::soft_decoder::{
    CHIP_SEQUENCES as SOFT_CHIP_SEQUENCES, DecodeEntry, DecoderDecisionPoint, DecoderEventTrace,
    DecoderTerminal, DecoderTracePoint, PositionedDecoderTraceStats, SoftDsssDecoder,
    UNIQUE_ANGULAR_CAP, classify_payload_symbol, classify_symbol, is_preamble_symbol,
    symbol_scores, template_correlation,
};
use super::{
    ArchiveWriter, Args, DecoderLane, StopReason, collect_candidates, configure_runtime, run,
    run_with_burst_deferred_shr_rate_soft, run_with_burst_shr_rate_soft,
    run_with_burst_shr_rate_trace_soft, run_with_burst_shr_soft, run_with_burst_soft,
    run_with_clock_gain_sweep, run_with_fixed_clock_mu, run_with_fixed_clock_rate,
};

// Canonical IEEE 802.15.4 2.4 GHz DSSS symbol sequences, represented in the
// bit order consumed by FutureSDR's recovered-chip decoder.
const CHIP_SEQUENCES: [u32; 16] = [
    1_618_456_172,
    1_309_113_062,
    1_826_650_030,
    1_724_778_362,
    778_887_287,
    2_061_946_375,
    2_007_919_840,
    125_494_990,
    529_027_475,
    838_370_585,
    320_833_617,
    422_705_285,
    1_368_596_360,
    85_537_272,
    139_563_807,
    2_021_988_657,
];

// Acknowledgment PSDU with sequence number 0x53 and little-endian FCS 0xd5a6.
const ACK_PSDU: [u8; 5] = [0x02, 0x00, 0x53, 0xa6, 0xd5];

#[derive(Block)]
struct TaggedByteSource<O = DefaultCpuWriter<u8>>
where
    O: CpuBufferWriter<Item = u8>,
{
    bytes: Vec<u8>,
    copied: usize,
    #[output]
    output: O,
}

impl TaggedByteSource<DefaultCpuWriter<u8>> {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            copied: 0,
            output: DefaultCpuWriter::default(),
        }
    }
}

impl<O> Kernel for TaggedByteSource<O>
where
    O: CpuBufferWriter<Item = u8>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        _mo: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        let (output, mut tags) = self.output.slice_with_tags();
        let count = output.len().min(self.bytes.len() - self.copied);
        if count == 0 {
            return Ok(());
        }
        if self.copied == 0 {
            tags.add_tag(0, Tag::Id(self.bytes.len() as u64));
        }
        output[..count].copy_from_slice(&self.bytes[self.copied..self.copied + count]);
        self.copied += count;
        self.output.produce(count);
        if self.copied == self.bytes.len() {
            io.finished = true;
        }
        Ok(())
    }
}

fn upstream_waveform(psdu: &[u8]) -> Vec<Complex32> {
    assert!((1..128).contains(&psdu.len()));
    configure_runtime();
    let mut phy = vec![0; 4];
    phy.push(0xa7);
    phy.push(psdu.len() as u8);
    phy.extend_from_slice(psdu);

    let expected_samples = 2 * 40_000 + 2 + phy.len() * 2 * 16 * 4;
    let mut flowgraph = Flowgraph::new();
    let source = flowgraph
        .add(TaggedByteSource::new(phy))
        .expect("add finite tagged PHY source");
    let modulator = block_on(modulator(&mut flowgraph)).expect("add pinned Zigbee modulator");
    let iq_delay: IqDelay = IqDelay::new();
    let iq_delay = flowgraph
        .add(iq_delay)
        .expect("add pinned O-QPSK I/Q delay");
    let sink = flowgraph
        .add(VectorSink::<Complex32>::new(expected_samples))
        .expect("add finite CF32 sink");
    flowgraph
        .stream_dyn(source, "output", modulator, "input")
        .expect("connect PHY source to modulator");
    flowgraph
        .stream_dyn(modulator, "output", iq_delay, "input")
        .expect("connect modulator to I/Q delay");
    flowgraph
        .stream_dyn(iq_delay, "output", sink, "input")
        .expect("connect I/Q delay to CF32 sink");

    let terminated = Runtime::new()
        .run(flowgraph)
        .expect("generate finite pinned-upstream waveform");
    let samples = terminated
        .block(&sink)
        .expect("read generated CF32 samples")
        .items()
        .clone();
    assert_eq!(samples.len(), expected_samples);
    samples
}

fn interpolate_4_to_20_msps(samples: Vec<Complex32>) -> Vec<Complex32> {
    configure_runtime();
    let mut flowgraph = Flowgraph::new();
    let source_samples = samples.len();
    let source = flowgraph
        .add(VectorSource::<Complex32>::new(samples))
        .expect("add finite 4 MSps source");
    let resampler = FirBuilder::resampling::<Complex32, Complex32>(5, 1);
    let taps = resampler.n_taps();
    assert_eq!(taps, 120, "pinned FutureSDR interpolator changed");
    let expected_samples = (source_samples - taps / 5) * 5;
    let resampler = flowgraph
        .add(resampler)
        .expect("add pinned 4-to-20 MSps interpolator");
    let sink = flowgraph
        .add(VectorSink::<Complex32>::new(expected_samples))
        .expect("add finite 20 MSps sink");
    flowgraph
        .stream_dyn(source, "output", resampler, "input")
        .expect("connect 4 MSps source to interpolator");
    flowgraph
        .stream_dyn(resampler, "output", sink, "input")
        .expect("connect interpolator to 20 MSps sink");

    let terminated = Runtime::new()
        .run(flowgraph)
        .expect("run pinned 4-to-20 MSps interpolation");
    let output = terminated
        .block(&sink)
        .expect("read interpolated samples")
        .items()
        .clone();
    assert_eq!(output.len(), expected_samples);
    output
}

fn append_symbol(chips: &mut Vec<f32>, symbol: u8) {
    let sequence = CHIP_SEQUENCES[usize::from(symbol)];
    for bit in (0..32).rev() {
        chips.push(if sequence & (1 << bit) == 0 {
            -1.0
        } else {
            1.0
        });
    }
}

fn append_byte(chips: &mut Vec<f32>, byte: u8) {
    append_symbol(chips, byte & 0x0f);
    append_symbol(chips, byte >> 4);
}

fn append_phy_packet(chips: &mut Vec<f32>, psdu: &[u8]) {
    assert!(psdu.len() < 128);
    // The four-byte PHY preamble is eight zero symbols.
    for _ in 0..8 {
        append_symbol(chips, 0);
    }
    append_symbol(chips, 7);
    append_symbol(chips, 10);
    append_byte(chips, psdu.len() as u8);
    for byte in psdu {
        append_byte(chips, *byte);
    }
}

fn symbol_window(symbol: u8) -> [f32; 32] {
    let mut chips = Vec::with_capacity(32);
    append_symbol(&mut chips, symbol);
    chips.try_into().expect("one symbol is exactly 32 chips")
}

#[test]
fn soft_scores_canonical_templates_on_the_masked_alignment() {
    assert_eq!(SOFT_CHIP_SEQUENCES, CHIP_SEQUENCES);
    for symbol in 0..16 {
        let mut chips = symbol_window(symbol);
        // The upstream 0x7fff_fffe mask ignores bits 31 and 0. In this
        // oldest-to-newest window those are the first and last entries.
        chips[0] = -1_000.0;
        chips[31] = 1_000.0;
        let scores = symbol_scores(&chips).expect("finite nonzero-energy symbol");
        assert_eq!(scores.best_symbol, symbol);
        assert!((scores.best_score - 1.0).abs() < f64::EPSILON);
        assert!(scores.runner_up_score <= 0.2 + f64::EPSILON);
        assert_eq!(classify_symbol(&chips), Some(symbol));
    }
}

#[test]
fn soft_scores_are_signed_and_reject_the_strict_codebook_boundary() {
    let mut antipodal = symbol_window(0);
    for chip in &mut antipodal {
        *chip = -*chip;
    }
    assert_eq!(template_correlation(&antipodal, 0), Some(-1.0));
    assert_eq!(template_correlation(&antipodal, 8), Some(1.0));
    assert_eq!(classify_symbol(&antipodal), Some(8));

    // Symbols 0 and 5 differ in 12 of the 30 scored positions. Their
    // unnormalized sum lies exactly at rho=sqrt(0.6) for both templates, so
    // the strict threshold and unique-best rule both reject it.
    let first = symbol_window(0);
    let second = symbol_window(5);
    let mut boundary = [0.0f32; 32];
    for index in 1..=30 {
        boundary[index] = first[index] + second[index];
    }
    let first_score = template_correlation(&boundary, 0).unwrap();
    let second_score = template_correlation(&boundary, 5).unwrap();
    assert!((first_score - 0.6f64.sqrt()).abs() < f64::EPSILON);
    assert_eq!(first_score, second_score);
    assert_eq!(classify_symbol(&boundary), None);
    assert_eq!(classify_payload_symbol(&boundary), None);
}

#[test]
fn payload_scores_accept_unique_sub_cap_symbols_and_reject_invalid_windows() {
    for symbol in 0..16 {
        let mut chips = symbol_window(symbol);
        for chip in &mut chips[1..=4] {
            *chip = -*chip;
        }
        assert_eq!(classify_symbol(&chips), None);
        assert_eq!(classify_payload_symbol(&chips), Some(symbol));
    }

    assert_eq!(classify_payload_symbol(&[0.0; 32]), None);
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut chips = symbol_window(0);
        chips[15] = value;
        assert_eq!(classify_payload_symbol(&chips), None);
    }
}

#[test]
fn soft_scores_reject_silence_and_nonfinite_scored_chips() {
    assert_eq!(classify_symbol(&[0.0; 32]), None);
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut chips = symbol_window(0);
        chips[15] = value;
        assert_eq!(classify_symbol(&chips), None);
    }
}

#[test]
fn preamble_shortcut_matches_full_classification() {
    let check = |chips| {
        let accepted = is_preamble_symbol(&chips);
        assert_eq!(accepted, classify_symbol(&chips) == Some(0));
        accepted
    };
    for symbol in 0..16 {
        assert_eq!(check(symbol_window(symbol)), symbol == 0);
    }
    assert!(!check([0.0; 32]));

    let preamble = symbol_window(0);
    let rival = symbol_window(5);
    // Equal weights tie at the cap. Unequal weights straddle the boundary.
    for (weight, expected) in [
        (0.75, true),
        (1.0 - f32::EPSILON, true),
        (1.0, false),
        (1.0 + f32::EPSILON, false),
        (1.25, false),
    ] {
        let mixed = std::array::from_fn(|i| preamble[i] + weight * rival[i]);
        assert_eq!(check(mixed), expected);
    }
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut chips = preamble;
        chips[0] = value;
        chips[31] = value;
        assert!(check(chips));
        chips[15] = value;
        assert!(!check(chips));
    }
}

fn decoder_flowgraph(
    chips: Vec<f32>,
    capacity: usize,
    lane: DecoderLane,
) -> anyhow::Result<(Flowgraph, mpsc::Receiver<Pmt>)> {
    let mut flowgraph = Flowgraph::new();
    let source = flowgraph
        .add(VectorSource::<f32>::new(chips))
        .expect("add recovered-chip source");
    let (sender, receiver) = mpsc::channel::<Pmt>(capacity);
    let message_pipe = flowgraph
        .add(MessagePipe::new(sender))
        .expect("add indexed-output message pipe");
    match lane {
        DecoderLane::Hard => {
            let decoder = flowgraph
                .add(Decoder::new(6))
                .expect("add hard IEEE 802.15.4 decoder");
            connect!(flowgraph, source > decoder);
            connect!(flowgraph, decoder.indexed | message_pipe);
        }
        DecoderLane::Soft => {
            let decoder = flowgraph
                .add(SoftDsssDecoder::new())
                .expect("add soft IEEE 802.15.4 decoder");
            connect!(flowgraph, source > decoder);
            connect!(flowgraph, decoder.indexed | message_pipe);
        }
    }
    Ok((flowgraph, receiver))
}

fn decode_messages(chips: Vec<f32>, lane: DecoderLane) -> Vec<Pmt> {
    let (flowgraph, receiver) = decoder_flowgraph(chips, 8, lane).unwrap();
    let runtime = Runtime::new();
    let running = runtime
        .start(flowgraph)
        .expect("start finite recovered-chip flowgraph");
    let handle = running.handle();
    let messages = runtime.spawn(async move {
        let mut messages = Vec::new();
        loop {
            let message = receiver.recv().await.expect("decoder message");
            let finished = message == Pmt::Finished;
            messages.push(message);
            if finished {
                let _ = handle.stop().await;
                return messages;
            }
        }
    });
    let terminated = running
        .wait()
        .expect("finite recovered-chip flowgraph completes");
    drop(terminated);
    block_on(messages)
}

#[test]
fn decoder_emits_exact_indexed_psdu_and_suppresses_zero_phr() {
    let mut chips = Vec::new();
    append_phy_packet(&mut chips, &[]);
    append_phy_packet(&mut chips, &ACK_PSDU);

    let messages = decode_messages(chips, DecoderLane::Hard);
    assert_eq!(
        messages,
        vec![
            Pmt::VecPmt(vec![Pmt::U64(1088), Pmt::Blob(ACK_PSDU.to_vec())]),
            Pmt::Finished,
        ]
    );
}

#[test]
fn clean_recovered_ack_is_identical_in_hard_and_soft_lanes() {
    let expected = vec![
        Pmt::VecPmt(vec![Pmt::U64(704), Pmt::Blob(ACK_PSDU.to_vec())]),
        Pmt::Finished,
    ];
    for lane in [DecoderLane::Hard, DecoderLane::Soft] {
        let mut chips = Vec::new();
        append_phy_packet(&mut chips, &ACK_PSDU);
        assert_eq!(decode_messages(chips, lane), expected);
    }
}

#[test]
fn soft_lane_recovers_low_magnitude_wrong_signs_that_hard_lane_rejects() {
    let mut chips = Vec::new();
    append_phy_packet(&mut chips, &ACK_PSDU);
    for symbol in chips.as_chunks_mut::<32>().0 {
        for chip in &mut symbol[1..=6] {
            *chip = -*chip * 0.01;
        }
    }

    assert_eq!(
        decode_messages(chips.clone(), DecoderLane::Hard),
        vec![Pmt::Finished]
    );
    assert_eq!(
        decode_messages(chips, DecoderLane::Soft),
        vec![
            Pmt::VecPmt(vec![Pmt::U64(704), Pmt::Blob(ACK_PSDU.to_vec())]),
            Pmt::Finished,
        ]
    );
}

fn flip_four_scored_chips(chips: &mut [f32], symbol_index: usize) {
    let start = symbol_index * 32;
    for chip in &mut chips[start + 1..=start + 4] {
        *chip = -*chip;
    }
}

#[test]
fn soft_lane_relaxes_payload_only_after_strict_sfd_and_phr() {
    let expected = vec![
        Pmt::VecPmt(vec![Pmt::U64(704), Pmt::Blob(ACK_PSDU.to_vec())]),
        Pmt::Finished,
    ];
    let mut payload_corrupted = Vec::new();
    append_phy_packet(&mut payload_corrupted, &ACK_PSDU);
    for symbol_index in 12..12 + ACK_PSDU.len() * 2 {
        flip_four_scored_chips(&mut payload_corrupted, symbol_index);
    }
    assert_eq!(
        decode_messages(payload_corrupted, DecoderLane::Soft),
        expected
    );

    for strict_symbol_index in [8, 10] {
        let mut chips = Vec::new();
        append_phy_packet(&mut chips, &ACK_PSDU);
        flip_four_scored_chips(&mut chips, strict_symbol_index);
        assert_eq!(
            decode_messages(chips, DecoderLane::Soft),
            vec![Pmt::Finished]
        );
    }
}

#[test]
fn limits_stop_and_publish_one_complete_indexed_record() {
    let directory = tempfile::tempdir().unwrap();
    for (name, max_frames, max_bytes, reason) in [
        ("frame-limit", 1, 1024, StopReason::FrameLimit),
        ("output-limit", 2, 49, StopReason::OutputLimit),
    ] {
        let output = directory.path().join(format!("{name}.i154raw"));
        let mut chips = Vec::new();
        append_phy_packet(&mut chips, &ACK_PSDU);
        append_phy_packet(&mut chips, &ACK_PSDU);
        let (flowgraph, receiver) = decoder_flowgraph(chips, 8, DecoderLane::Hard).unwrap();
        let archive = ArchiveWriter::create(&output, 17, max_frames, max_bytes).unwrap();

        let runtime = Runtime::new();
        let running = runtime
            .start(flowgraph)
            .expect("start finite decoder graph");
        let collector = runtime.spawn(collect_candidates(
            receiver,
            archive,
            None,
            running.handle(),
        ));
        let terminated = running.wait().expect("finite decoder graph completes");
        drop(terminated);
        let collected = block_on(collector).expect("collect first indexed candidate");

        assert_eq!(collected.reason, reason);
        assert_eq!(collected.archive.records(), 1);
        assert_eq!(collected.archive.bytes(), 49);
        assert_eq!(collected.archive.publish().unwrap(), (1, 49));

        let bytes = fs::read(&output).unwrap();
        assert_eq!(bytes.len(), 49);
        assert_eq!(&bytes[24..28], b"PSDU");
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 25);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 704);
        assert_eq!(u16::from_le_bytes(bytes[40..42].try_into().unwrap()), 5);
        assert_eq!(&bytes[44..49], &ACK_PSDU);
        assert!(
            !directory
                .path()
                .join(format!("{name}.i154raw.partial"))
                .exists()
        );
    }
}

#[test]
fn full_cf32_path_writes_the_same_exact_ack_archive_in_both_lanes() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("ack.cf32");
    let samples = upstream_waveform(&ACK_PSDU);
    assert_eq!(samples.len(), 81_410);

    let mut bytes = Vec::with_capacity(samples.len() * 8);
    for sample in samples {
        bytes.extend_from_slice(&sample.re.to_le_bytes());
        bytes.extend_from_slice(&sample.im.to_le_bytes());
    }
    assert_eq!(bytes.len(), 651_280);
    fs::write(&input, bytes).unwrap();

    let expected = [
        0x49, 0x31, 0x35, 0x34, 0x52, 0x41, 0x57, 0x00, 0x01, 0x00, 0x18, 0x00, 0x11, 0x00, 0x00,
        0x00, 0x00, 0x09, 0x3d, 0x00, 0x80, 0x84, 0x1e, 0x00, 0x50, 0x53, 0x44, 0x55, 0x19, 0x00,
        0x00, 0x00, 0xe0, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x02,
        0x00, 0x53, 0xa6, 0xd5,
    ];
    for (name, lane, executor_threads) in [
        ("hard.i154raw", DecoderLane::Hard, None),
        ("hard-one-executor.i154raw", DecoderLane::Hard, Some(1)),
        ("hard-two-executors.i154raw", DecoderLane::Hard, Some(2)),
        ("soft.i154raw", DecoderLane::Soft, None),
        ("soft-eight-executors.i154raw", DecoderLane::Soft, Some(8)),
    ] {
        let output = directory.path().join(name);
        run(Args {
            input: input.clone(),
            output: output.clone(),
            source_index_output: None,
            channel: 17,
            max_input_mib: 1,
            max_frames: 16,
            max_output_mib: 1,
            decoder_lane: lane,
            carrier_offset_hz: 0,
            executor_threads,
        })
        .unwrap();
        let archive = fs::read(output).unwrap();
        assert_eq!(archive, expected);
    }
}

const GRADED_SAMPLE_SNR_DB: [f64; 5] = [12.0, 9.0, 6.0, 3.0, 0.0];

// These cases characterize changes while a decoder implementation is being
// developed. They must not be substituted for the separately frozen holdout
// seeds below.
const DEVELOPMENT_NOISE_SEEDS: [u64; 2] = [0x2437_0016_0017_0018, 0x2437_1016_1017_1018];

// Holdout results are a final acceptance check, not decoder-tuning input. Keep
// this list distinct from DEVELOPMENT_NOISE_SEEDS when adding future cases.
const HOLDOUT_NOISE_SEEDS: [u64; 2] = [0x8021_5416_1718_0001, 0x8021_5416_1718_0002];

// This order is part of the H3 qualification contract: retain the established
// zero-offset evidence first, exercise the observed negative-CFO blind spot
// second, and then run the matching positive endpoint.
const H3_HYPOTHESES_HZ: [i32; 3] = [0, -100_000, 100_000];
const H3_DEVELOPMENT_OFFSETS_HZ: [i32; 9] = [
    -100_000, -75_000, -50_000, -25_000, 0, 25_000, 50_000, 75_000, 100_000,
];
// Offset values are fixed before inspecting the separately seeded holdout.
const H3_HOLDOUT_OFFSETS_HZ: [i32; 4] = [-75_000, -25_000, 25_000, 75_000];
const H3_AWGN_SAMPLE_SNR_DB: f64 = 12.0;

const TIMING_OFFSET_DENOMINATOR: i32 = 16;
const TIMING_DEVELOPMENT_OFFSETS: [i32; 9] = [-8, -6, -4, -2, 0, 2, 4, 6, 8];
// These interstitial phases are frozen before any timing-recovery candidate is
// developed. They are not evaluated by the development characterization.
const TIMING_HOLDOUT_OFFSETS: [i32; 8] = [-7, -5, -3, -1, 1, 3, 5, 7];
const TIMING_DEVELOPMENT_SEEDS: [u64; 2] = [0x2437_0017_4652_4143, 0x2437_0017_5449_4d45];
const TIMING_HOLDOUT_SEEDS: [u64; 2] = [0x8021_5417_4652_0001, 0x8021_5417_4652_0002];
const TIMING_AWGN_SAMPLE_SNR_DB: f64 = 6.0;
const TIMING_RATE_ERROR_PPM: [i32; 3] = [-100, 0, 100];
const TIMING_RATE_DEVELOPMENT_OFFSETS: [i32; 3] = [-4, 0, 8];
const TIMING_RATE_SCREEN_CASES: [(i32, i32); 3] = [(-4, 100), (0, -100), (8, 100)];
const TIMING_RATE_PPM_DENOMINATOR: f64 = 1_000_000.0;
const TIMING_BURST_DIAGNOSTIC_OFFSET: i32 = 0;
const TIMING_BURST_DIAGNOSTIC_RATE_ERROR_PPM: i32 = -100;
const TIMING_BURST_DIAGNOSTIC_SEED_INDEX: usize = 0;
const TIMING_BURST_DIAGNOSTIC_FLUSH_SAMPLES: usize = 8;
const TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS: u8 = 8;
// Fixed phases bracketing the midpoint of the ClockRecoveryMm interpolation
// interval. The ordinary adaptive arms remain at 8/16.
const TIMING_FIXED_CLOCK_MU_SIXTEENTHS: [u8; 2] = [4, 12];
// Shared detector-frozen provenance for the clean short-then-long -8/16 cell.
const CLEAN_NEGATIVE_HALF_PHASE_INDEX: usize = 0;
const CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS: i32 = -8;
const CLEAN_NEGATIVE_HALF_PHASE_SHA256: &str =
    "b30ea4b98d88af55f16146febbc90f99542f7378b2003d42a87b724fa9395e02";
const CLEAN_NEGATIVE_HALF_PHASE_EVENTS: [DetectedBurst; 2] = [
    DetectedBurst {
        replay_start: 39_925,
        detected_at: 39_988,
        end_exclusive: 41_521,
        forced_end: false,
    },
    DetectedBurst {
        replay_start: 121_335,
        detected_at: 121_398,
        end_exclusive: 138_547,
        forced_end: false,
    },
];
// Shared selector-positive provenance for the clean short-then-long 0/16 cell.
const CLEAN_ZERO_PHASE_EVENTS: [DetectedBurst; 2] = [
    DetectedBurst {
        replay_start: 39_941,
        detected_at: 40_004,
        end_exclusive: 41_505,
        forced_end: false,
    },
    DetectedBurst {
        replay_start: 121_351,
        detected_at: 121_414,
        end_exclusive: 138_531,
        forced_end: false,
    },
];
const CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS: [ShrRateTimingSelection; 2] = [
    shr_rate_selection(60, 15, 40_001, 100),
    shr_rate_selection(60, 15, 121_411, 100),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateCounts {
    recovered_mask: u8,
    invalid_fcs: u32,
    unexpected_valid_fcs: u32,
    duplicate_transmitted: u32,
}

impl CandidateCounts {
    const NONE: Self = Self {
        recovered_mask: 0,
        invalid_fcs: 0,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };

    const SHORT_ONLY: Self = Self {
        recovered_mask: 0b01,
        invalid_fcs: 0,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };

    const LONG_ONLY: Self = Self {
        recovered_mask: 0b10,
        invalid_fcs: 0,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };

    const BOTH_TRANSMITTED: Self = Self {
        recovered_mask: 0b11,
        invalid_fcs: 0,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };

    const ONE_INVALID_FCS: Self = Self {
        recovered_mask: 0,
        invalid_fcs: 1,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };

    fn valid_recoveries(self) -> u32 {
        self.recovered_mask.count_ones()
    }

    const fn has_no_anomalies(self) -> bool {
        self.invalid_fcs == 0 && self.unexpected_valid_fcs == 0 && self.duplicate_transmitted == 0
    }
}

// Rows follow TIMING_DEVELOPMENT_OFFSETS; each row is [hard, soft]. The
// complete records freeze anomaly counts as well as the recovery mask.
const EXPECTED_TIMING_CLEAN_DEVELOPMENT_COUNTS: [[CandidateCounts; 2]; 9] =
    [[CandidateCounts::BOTH_TRANSMITTED; 2]; 9];
const EXPECTED_TIMING_BAD_FCS_DEVELOPMENT_COUNTS: [[CandidateCounts; 2]; 9] =
    [[CandidateCounts::ONE_INVALID_FCS; 2]; 9];
const EXPECTED_TIMING_BAD_FCS_FIXED_CANDIDATE_COUNTS: [[CandidateCounts; 4]; 9] =
    [[CandidateCounts::ONE_INVALID_FCS; 4]; 9];
// Seed-major rows follow TIMING_DEVELOPMENT_SEEDS, then
// TIMING_DEVELOPMENT_OFFSETS; every cell is [hard, soft].
const EXPECTED_TIMING_6DB_DEVELOPMENT_COUNTS: [[[CandidateCounts; 2]; 9]; 2] = [
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
        ],
        [CandidateCounts::SHORT_ONLY; 2],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
        ],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::SHORT_ONLY, CandidateCounts::LONG_ONLY],
        [CandidateCounts::BOTH_TRANSMITTED, CandidateCounts::NONE],
        [CandidateCounts::BOTH_TRANSMITTED, CandidateCounts::NONE],
        [CandidateCounts::SHORT_ONLY; 2],
    ],
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::LONG_ONLY,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::LONG_ONLY,
        ],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [CandidateCounts::SHORT_ONLY, CandidateCounts::LONG_ONLY],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
    ],
];
// The four arms are ordinary hard, ordinary soft, then fixed hard at 4/16
// and 12/16 sample. Rows remain seed-major, then phase-major.
const EXPECTED_TIMING_6DB_FIXED_CANDIDATE_COUNTS: [[[CandidateCounts; 4]; 9]; 2] = [
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::LONG_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
    ],
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::LONG_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::LONG_ONLY,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::LONG_ONLY,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::NONE,
        ],
    ],
];
// Arms are the four established/fixed observations followed by fixed-rate
// (-100, mu 4/12) and (+100, mu 4/12). The middle case remains a rejection.
const EXPECTED_TIMING_RATE_ENDPOINT_SCREEN_COUNTS: [[CandidateCounts; 8]; 3] = [
    [
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::NONE,
        CandidateCounts::NONE,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::NONE,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::NONE,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::NONE,
        CandidateCounts::SHORT_ONLY,
    ],
    [
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::NONE,
    ],
];
// Seed-major, then TIMING_RATE_DEVELOPMENT_OFFSETS, then
// TIMING_RATE_ERROR_PPM. Each cell retains the established hard/soft and fixed
// hard 4/16 and 12/16 arms recorded before the adaptive-gain screen.
const EXPECTED_TIMING_RATE_FIXED_CANDIDATE_COUNTS: [[[[CandidateCounts; 4]; 3]; 3]; 2] = [
    [
        [
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
            ],
        ],
        [
            [CandidateCounts::SHORT_ONLY; 4],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
            ],
        ],
        [
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::NONE,
            ],
            [CandidateCounts::SHORT_ONLY; 4],
        ],
    ],
    [
        [
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::LONG_ONLY,
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::NONE,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
            ],
        ],
        [
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::LONG_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
            ],
        ],
        [
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::NONE,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::NONE,
            ],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
        ],
    ],
];

// The prior slow-loop result is an independent equivalence control for gain
// index 1 in the complete sweep.
const EXPECTED_TIMING_RATE_SLOW_CANDIDATE_COUNTS: [[[CandidateCounts; 3]; 3]; 2] = [
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
        ],
        [CandidateCounts::SHORT_ONLY; 3],
        [CandidateCounts::SHORT_ONLY; 3],
    ],
    [
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
        ],
        [CandidateCounts::SHORT_ONLY; 3],
    ],
];

// Seed-major, then phase-major, rate-major and gain-major. Gain mu order is
// 0.0075, 0.015, 0.03 and 0.06.
const EXPECTED_TIMING_RATE_GAIN_SWEEP_COUNTS: [[[[CandidateCounts; 4]; 3]; 3]; 2] = [
    [
        [
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
        ],
        [
            [CandidateCounts::SHORT_ONLY; 4],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
        ],
        [
            [
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
        ],
    ],
    [
        [
            [CandidateCounts::BOTH_TRANSMITTED; 4],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
            ],
            [CandidateCounts::SHORT_ONLY; 4],
        ],
        [
            [CandidateCounts::BOTH_TRANSMITTED; 4],
            [
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
            ],
            [
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
        ],
        [
            [
                CandidateCounts::NONE,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
            [CandidateCounts::SHORT_ONLY; 4],
            [
                CandidateCounts::LONG_ONLY,
                CandidateCounts::SHORT_ONLY,
                CandidateCounts::BOTH_TRANSMITTED,
                CandidateCounts::BOTH_TRANSMITTED,
            ],
        ],
    ],
];
// Seed-major, then phase-major and rate-major. This is the rejected causal
// burst soft arm, frozen before any detector or acquisition parameter change.
const EXPECTED_TIMING_RATE_BURST_COUNTS: [[[CandidateCounts; 3]; 3]; 2] = [
    [
        [
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::SHORT_ONLY,
            CandidateCounts::LONG_ONLY,
        ],
        [CandidateCounts::BOTH_TRANSMITTED; 3],
        [CandidateCounts::SHORT_ONLY; 3],
    ],
    [
        [
            CandidateCounts::LONG_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [
            CandidateCounts::BOTH_TRANSMITTED,
            CandidateCounts::LONG_ONLY,
            CandidateCounts::BOTH_TRANSMITTED,
        ],
        [CandidateCounts::BOTH_TRANSMITTED; 3],
    ],
];
// Rejected SHR initializer, frozen after its one predeclared full-grid capture.
// Rows are seed-major, phase-major and rate-major.
const EXPECTED_TIMING_RATE_SHR_OBSERVATIONS: [[[FrozenShrRateObservation; 3]; 3]; 2] = [
    [
        [
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(45, 7, 39_997),
                None,
                658,
                [Some(20_702), None],
            ),
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(45, 7, 40_001),
                Some(shr_selection(44, 9, 121_411)),
                659,
                [Some(20_704), None],
            ),
            frozen_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_selection(46, 7, 40_005),
                Some(shr_selection(46, 8, 121_423)),
                657,
                [None, Some(69_224)],
            ),
        ],
        [
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(42, 8, 39_997),
                Some(shr_selection(46, 8, 121_399)),
                661,
                [Some(20_702), Some(69_211)],
            ),
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(45, 8, 40_001),
                Some(shr_selection(44, 8, 121_411)),
                659,
                [Some(20_704), Some(69_217)],
            ),
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(45, 8, 40_005),
                Some(shr_selection(45, 8, 121_423)),
                658,
                [Some(20_706), Some(69_224)],
            ),
        ],
        [
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(41, 10, 39_997),
                None,
                662,
                [Some(20_702), None],
            ),
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(46, 6, 40_002),
                Some(shr_selection(45, 6, 121_412)),
                658,
                [Some(20_705), None],
            ),
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(43, 6, 40_006),
                Some(shr_selection(44, 9, 121_423)),
                660,
                [Some(20_707), None],
            ),
        ],
    ],
    [
        [
            frozen_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_selection(42, 7, 39_997),
                Some(shr_selection(35, 7, 121_399)),
                668,
                [None, Some(69_211)],
            ),
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(49, 8, 40_001),
                Some(shr_selection(49, 8, 121_411)),
                654,
                [Some(20_704), Some(69_217)],
            ),
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(45, 7, 40_005),
                None,
                658,
                [Some(20_706), None],
            ),
        ],
        [
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(43, 8, 39_997),
                Some(shr_selection(35, 11, 121_399)),
                668,
                [Some(20_702), Some(69_211)],
            ),
            frozen_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_selection(48, 8, 40_001),
                Some(shr_selection(48, 8, 121_411)),
                655,
                [None, Some(69_217)],
            ),
            frozen_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_selection(42, 8, 40_005),
                None,
                661,
                [Some(20_706), None],
            ),
        ],
        [
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(45, 6, 39_998),
                Some(shr_selection(39, 6, 121_400)),
                664,
                [Some(20_702), Some(69_211)],
            ),
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(48, 10, 40_001),
                Some(shr_selection(48, 6, 121_412)),
                655,
                [Some(20_704), Some(69_218)],
            ),
            frozen_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_selection(43, 6, 40_006),
                Some(shr_selection(47, 7, 121_424)),
                660,
                [Some(20_707), Some(69_224)],
            ),
        ],
    ],
];
// Rejected joint start/phase/rate SHR initializer, frozen after its one
// predeclared full-grid capture. Rows are seed-major, phase-major and
// input-rate-major. The selected rate is a search hypothesis, not an oracle
// for the injected fixture rate.
const EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS: [[[FrozenJointShrRateObservation; 3]; 3]; 2] = [
    [
        [
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(45, 7, 39_997, 100),
                None,
                659,
                [Some(20_702), None],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(45, 7, 40_001, 100),
                Some(shr_rate_selection(44, 8, 121_411, 100)),
                660,
                [Some(20_704), None],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_rate_selection(46, 7, 40_005, 0),
                Some(shr_rate_selection(46, 7, 121_423, 100)),
                658,
                [None, Some(69_224)],
            ),
        ],
        [
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(42, 8, 39_997, 0),
                Some(shr_rate_selection(46, 8, 121_399, 100)),
                662,
                [Some(20_702), Some(69_211)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(45, 8, 40_001, 0),
                Some(shr_rate_selection(44, 8, 121_411, 0)),
                660,
                [Some(20_704), Some(69_217)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(45, 8, 40_005, 0),
                Some(shr_rate_selection(45, 8, 121_423, 0)),
                659,
                [Some(20_706), Some(69_224)],
            ),
        ],
        [
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(41, 10, 39_997, 0),
                None,
                663,
                [Some(20_702), None],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(46, 6, 40_002, 100),
                Some(shr_rate_selection(45, 6, 121_412, 0)),
                659,
                [Some(20_705), None],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(43, 6, 40_006, 100),
                Some(shr_rate_selection(44, 10, 121_423, -100)),
                661,
                [Some(20_707), None],
            ),
        ],
    ],
    [
        [
            frozen_joint_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_rate_selection(42, 8, 39_997, -100),
                Some(shr_rate_selection(35, 8, 121_399, -100)),
                669,
                [None, Some(69_211)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(49, 8, 40_001, -100),
                Some(shr_rate_selection(49, 8, 121_411, -100)),
                655,
                [Some(20_704), Some(69_217)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(45, 7, 40_005, 100),
                None,
                659,
                [Some(20_706), None],
            ),
        ],
        [
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(43, 8, 39_997, 0),
                Some(shr_rate_selection(35, 10, 121_399, 100)),
                669,
                [Some(20_702), Some(69_211)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::LONG_ONLY,
                shr_rate_selection(48, 8, 40_001, 0),
                Some(shr_rate_selection(48, 9, 121_411, -100)),
                656,
                [None, Some(69_217)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::SHORT_ONLY,
                shr_rate_selection(42, 8, 40_005, 0),
                None,
                662,
                [Some(20_706), None],
            ),
        ],
        [
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(45, 6, 39_998, 0),
                Some(shr_rate_selection(39, 6, 121_400, 0)),
                665,
                [Some(20_702), Some(69_211)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(48, 10, 40_001, 0),
                Some(shr_rate_selection(48, 6, 121_412, 0)),
                656,
                [Some(20_704), Some(69_218)],
            ),
            frozen_joint_shr_rate(
                CandidateCounts::BOTH_TRANSMITTED,
                shr_rate_selection(43, 6, 40_006, 100),
                Some(shr_rate_selection(47, 7, 121_424, 0)),
                661,
                [Some(20_707), Some(69_224)],
            ),
        ],
    ],
];
// Seed-major rows follow TIMING_DEVELOPMENT_SEEDS, then
// TIMING_DEVELOPMENT_OFFSETS. These digest the complete little-endian CF32
// bytes before any timing-state hypothesis is evaluated.
const EXPECTED_TIMING_6DB_DEVELOPMENT_SHA256: [[&str; 9]; 2] = [
    [
        "6d5d1598f8b5bff061dc15c01ad3a16771254703504bf30a61ba687ff5b1236b",
        "e71af53293207d99d7e6906ffe2140d0631cda3cf452ab2ce634021f33fa4104",
        "0a3baa58c6de17ca453b4526d115702f063b43a9740fb2c4807cbad5121f8fa6",
        "2134ed0b771223190ab88ed85edff928792d547458d1e369e6781d2b6c734423",
        "24cf6a515f011a0449fe3e331f683b85343ae89632cf97e3f81bfcb555f516e4",
        "26b6a25685546ed0eb6fc921b6a480f336a58faa4b651a1f99368689af7cff8b",
        "02673246409d93b197faf1d958f84b484204403a44c58deb9a150c97214d142f",
        "a2bda5746e054230425d9003f3eaad704ef91e1517fcec9103b749525309cb29",
        "6fe100888a83f4bbb3d8c603c77d76dd5eac07199db3c89b103d534db3f2b68b",
    ],
    [
        "3956d8f09edccccc45e7989b52ea3c9195a04559ae5b918690454186e8e9a2ea",
        "1ed9d93884983bad138327149ae498af664ab5c7466576693b837735733b88f2",
        "5cfc9227cc1c005ac52dbf24dc086f6a030586499b98fdbb9f180a555e0bc125",
        "bf345573e800ce858b6ce73ff7cfae0834fccd7a972dec31ac3141d213d6dcc7",
        "aa315c3b8f854ccc1fd3f44b0a5f10c456f836f6485a906d2cdf30bb699212c0",
        "4c20b8a5e7d84a4baabe4ac80853124d17fc3df0e3fc1ac2e7f1ee57c2a2a911",
        "eaaf4336c51fe3eff479f6ec1ee542ad811a1d78af684c4cd8155131e8d49a88",
        "8ed6fa9fe1a02109d725cbb861f2ec98779f4182ce8b8c8f6cec5aba0181f9cc",
        "9e003a46857afd89ad849bd942d5de25cadfa948667273e6d6cfcd7b56d2f687",
    ],
];
// Seed-major, then TIMING_RATE_DEVELOPMENT_OFFSETS, then
// TIMING_RATE_ERROR_PPM. These are frozen before decoding the rate matrix.
const EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256: [[[&str; 3]; 3]; 2] = [
    [
        [
            "a1af71b150709737e86000214578bfec37ba9646a6356071f6859ec0cc6dd87a",
            "0a3baa58c6de17ca453b4526d115702f063b43a9740fb2c4807cbad5121f8fa6",
            "49ec3db1736c12837448f9d533f5bca24bd2e9e6696b21a76e0f1b75a099a12b",
        ],
        [
            "ad3b4ae01205e80023d3a3a70d1c057aba521293b494d411bfa68d9b894e5f0c",
            "24cf6a515f011a0449fe3e331f683b85343ae89632cf97e3f81bfcb555f516e4",
            "766b647d3d66277feb41a2e3438015143e601ac312f7e784a4d5b50e73a9d834",
        ],
        [
            "e76943e76eeec7a0a4bb0576da2bab8b417ba25f5893b1a77a92c19eb291456f",
            "6fe100888a83f4bbb3d8c603c77d76dd5eac07199db3c89b103d534db3f2b68b",
            "a1f2f46be675bb2e8359a8464b71c4a23a247646193ab596bee4fc7a52439236",
        ],
    ],
    [
        [
            "193c482f72a525d65656b80838dffdff31d5f5d8a76e0b79b1ae3e1c16748e07",
            "5cfc9227cc1c005ac52dbf24dc086f6a030586499b98fdbb9f180a555e0bc125",
            "7031fa5a13af4af8384b4e2f514abad71c6206c4fe02f635ef5ec58f84f21f9e",
        ],
        [
            "fe44a75a3e5eb7554a320bc6a10c000a15c384ddca24e5cd78154ae089a67d71",
            "aa315c3b8f854ccc1fd3f44b0a5f10c456f836f6485a906d2cdf30bb699212c0",
            "4b234647a157b2390661d3f7e68f08c56568fe0b248ac38fba675fb21b45263b",
        ],
        [
            "ed6f7d832efa9e8a93c0f6fa837dd874efe4d7807b78dc9c3167288ae7ae104c",
            "9e003a46857afd89ad849bd942d5de25cadfa948667273e6d6cfcd7b56d2f687",
            "30962bd8d686448be9e5c35319df5aba8bd6fbaf7f5d558162912fe0d231493a",
        ],
    ],
];
const EXPECTED_TIMING_BURST_DIAGNOSTIC_ACTIVE_BOUNDS: [(usize, usize); 2] =
    [(39_982, 41_421), (39_982, 57_036)];
const EXPECTED_TIMING_BURST_DIAGNOSTIC_LENGTHS: [usize; 4] = [178_418, 97_016, 17_063, 1_448];
const EXPECTED_TIMING_BURST_DIAGNOSTIC_SHA256: [&str; 4] = [
    "ad3b4ae01205e80023d3a3a70d1c057aba521293b494d411bfa68d9b894e5f0c",
    "ff23478447fdef529301c0c30b3ce2b6c961e59e5edb7f84d4f24e98693d1b19",
    "e9fbf8dd042cc4bbbc78a0d1ce6483aa9d4b367caa27b745805bafbe4c5b53a9",
    "51222749bc875dbb37d9bdb048ff6107203fbb07c54a860cec7734c4197eaf89",
];
// Cases are combined, full guarded long, cropped long and cropped short. Arms
// retain the established hard/soft and fixed hard 4/16 and 12/16 order.
const EXPECTED_TIMING_BURST_DIAGNOSTIC_COUNTS: [[CandidateCounts; 4]; 4] = [
    [CandidateCounts::SHORT_ONLY; 4],
    [CandidateCounts::NONE; 4],
    [
        CandidateCounts::NONE,
        CandidateCounts::LONG_ONLY,
        CandidateCounts::NONE,
        CandidateCounts::NONE,
    ],
    [CandidateCounts::SHORT_ONLY; 4],
];
const EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED: [DetectedBurst; 2] = [
    DetectedBurst {
        replay_start: 39_955,
        detected_at: 40_018,
        end_exclusive: 41_491,
        forced_end: false,
    },
    DetectedBurst {
        replay_start: 121_353,
        detected_at: 121_416,
        end_exclusive: 138_511,
        forced_end: false,
    },
];
const EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG: [DetectedBurst; 1] = [DetectedBurst {
    replay_start: 39_951,
    detected_at: 40_014,
    end_exclusive: 57_109,
    forced_end: false,
}];
const EXPECTED_CAUSAL_BURST_STREAMING_COMBINED_TICKS: [u64; 2] = [20_702, 69_211];
const EXPECTED_CAUSAL_BURST_STREAMING_FULL_LONG_TICKS: [u64; 1] = [28_510];
const EXPECTED_SHR_TIMING_COMBINED_SELECTIONS: [ShrTimingSelection; 2] = [
    ShrTimingSelection {
        start_offset: 42,
        mu_sixteenths: 8,
        source_sample_index: 39_997,
    },
    ShrTimingSelection {
        start_offset: 46,
        mu_sixteenths: 8,
        source_sample_index: 121_399,
    },
];
const EXPECTED_SHR_TIMING_FULL_LONG_SELECTION: ShrTimingSelection = ShrTimingSelection {
    start_offset: 46,
    mu_sixteenths: 8,
    source_sample_index: 39_997,
};
const EXPECTED_SHR_TIMING_COMBINED_TICKS: [u64; 2] = [20_702, 69_211];
const EXPECTED_SHR_TIMING_FULL_LONG_TICKS: [u64; 1] = [28_510];
// The opened fixture carries -100 ppm input error, but this deterministic
// parity result selects 0 ppm for the short burst and +100 ppm for the long
// burst. These values qualify repeatable acquisition and decode integration;
// they do not qualify clock-rate estimation accuracy.
const EXPECTED_SHR_RATE_TIMING_COMBINED_SELECTIONS: [ShrRateTimingSelection; 2] = [
    ShrRateTimingSelection {
        timing: ShrTimingSelection {
            start_offset: 42,
            mu_sixteenths: 8,
            source_sample_index: 39_997,
        },
        rate_error_ppm: 0,
    },
    ShrRateTimingSelection {
        timing: ShrTimingSelection {
            start_offset: 46,
            mu_sixteenths: 8,
            source_sample_index: 121_399,
        },
        rate_error_ppm: 100,
    },
];
const EXPECTED_SHR_RATE_TIMING_FULL_LONG_SELECTION: ShrRateTimingSelection =
    ShrRateTimingSelection {
        timing: ShrTimingSelection {
            start_offset: 46,
            mu_sixteenths: 8,
            source_sample_index: 39_997,
        },
        rate_error_ppm: 100,
    };
const EXPECTED_SHR_RATE_TIMING_COMBINED_TICKS: [u64; 2] = [20_702, 69_211];
const EXPECTED_SHR_RATE_TIMING_FULL_LONG_TICKS: [u64; 1] = [28_510];

// Rows are seed-major, then descending GRADED_SAMPLE_SNR_DB. Each row is
// [hard, soft]. All fields are frozen, including the zero invalid/duplicate
// counts; do not rewrite holdout expectations to accommodate decoder changes.
const EXPECTED_DEVELOPMENT_COUNTS: [[CandidateCounts; 2]; 10] = [
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [CandidateCounts::NONE, CandidateCounts::NONE],
    [CandidateCounts::NONE, CandidateCounts::NONE],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [CandidateCounts::NONE, CandidateCounts::LONG_ONLY],
    [CandidateCounts::NONE, CandidateCounts::NONE],
    [CandidateCounts::NONE, CandidateCounts::NONE],
];

const EXPECTED_HOLDOUT_COUNTS: [[CandidateCounts; 2]; 10] = [
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::SHORT_ONLY,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [CandidateCounts::SHORT_ONLY, CandidateCounts::NONE],
    [CandidateCounts::NONE, CandidateCounts::NONE],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [
        CandidateCounts::BOTH_TRANSMITTED,
        CandidateCounts::BOTH_TRANSMITTED,
    ],
    [CandidateCounts::NONE, CandidateCounts::BOTH_TRANSMITTED],
    [CandidateCounts::NONE, CandidateCounts::NONE],
    [CandidateCounts::NONE, CandidateCounts::NONE],
];

// Rows follow H3_DEVELOPMENT_OFFSETS_HZ; columns follow H3_HYPOTHESES_HZ;
// every cell is [hard, soft]. This matrix was recorded before the separate H3
// AWGN holdout was run and must not be rewritten to accommodate later changes.
const EXPECTED_H3_CLEAN_DEVELOPMENT_COUNTS: [H3Observations; 9] = [
    [
        [CandidateCounts::NONE; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::NONE; 2],
    ],
    [
        [CandidateCounts::NONE; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::NONE; 2],
    ],
    [
        [CandidateCounts::LONG_ONLY; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::NONE; 2],
    ],
    [
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::NONE; 2],
    ],
    [
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::NONE; 2],
    ],
    [[CandidateCounts::BOTH_TRANSMITTED; 2]; 3],
    [
        [CandidateCounts::BOTH_TRANSMITTED; 2],
        [CandidateCounts::LONG_ONLY; 2],
        [CandidateCounts::BOTH_TRANSMITTED; 2],
    ],
    [[CandidateCounts::BOTH_TRANSMITTED; 2]; 3],
    [[CandidateCounts::BOTH_TRANSMITTED; 2]; 3],
];

const H3_ALL_BOTH_TRANSMITTED: H3Observations =
    [[CandidateCounts::BOTH_TRANSMITTED; 2]; H3_HYPOTHESES_HZ.len()];

// Seed-major rows, then H3_DEVELOPMENT_OFFSETS_HZ; columns follow
// H3_HYPOTHESES_HZ and every cell is [hard, soft]. These development results
// were frozen before the separately seeded H3 holdout was run.
const EXPECTED_H3_12DB_DEVELOPMENT_COUNTS: [[H3Observations; 9]; 2] = [
    [H3_ALL_BOTH_TRANSMITTED; 9],
    [
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        [
            [CandidateCounts::BOTH_TRANSMITTED; 2],
            [CandidateCounts::LONG_ONLY; 2],
            [CandidateCounts::BOTH_TRANSMITTED; 2],
        ],
    ],
];

// Seed-major rows, then H3_HOLDOUT_OFFSETS_HZ; columns follow
// H3_HYPOTHESES_HZ and every cell is [hard, soft]. This separately seeded
// matrix was frozen after the predeclared coverage and anomaly gates passed.
const EXPECTED_H3_12DB_HOLDOUT_COUNTS: [[H3Observations; 4]; 2] = [
    [H3_ALL_BOTH_TRANSMITTED; 4],
    [
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        H3_ALL_BOTH_TRANSMITTED,
        [
            [CandidateCounts::BOTH_TRANSMITTED; 2],
            [CandidateCounts::SHORT_ONLY; 2],
            [CandidateCounts::BOTH_TRANSMITTED; 2],
        ],
    ],
];

#[derive(Clone, Copy, Debug)]
struct SplitMix64 {
    state: u64,
    spare_gaussian: Option<f64>,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            spare_gaussian: None,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn open_unit_f64(&mut self) -> f64 {
        const TWO_TO_53: f64 = 9_007_199_254_740_992.0;
        ((self.next_u64() >> 11) as f64 + 0.5) / TWO_TO_53
    }

    fn gaussian(&mut self) -> f64 {
        if let Some(spare) = self.spare_gaussian.take() {
            return spare;
        }
        let radius = (-2.0 * self.open_unit_f64().ln()).sqrt();
        let angle = std::f64::consts::TAU * self.open_unit_f64();
        let (sin, cos) = angle.sin_cos();
        self.spare_gaussian = Some(radius * sin);
        radius * cos
    }
}

fn ieee802154_fcs(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        let mut current = *byte;
        for _ in 0..8 {
            let feedback = (crc ^ u16::from(current)) & 1;
            crc >>= 1;
            if feedback != 0 {
                crc ^= 0x8408;
            }
            current >>= 1;
        }
    }
    crc
}

fn has_valid_fcs(psdu: &[u8]) -> bool {
    let Some((body, encoded)) = psdu.split_at_checked(psdu.len().saturating_sub(2)) else {
        return false;
    };
    if body.len() + 2 != psdu.len() {
        return false;
    }
    ieee802154_fcs(body) == u16::from_le_bytes(encoded.try_into().expect("two FCS bytes"))
}

fn longer_valid_psdu() -> Vec<u8> {
    // A synthetic version-2003 data frame with no addressing fields. The
    // deterministic payload has no relationship to retained RF observations.
    let mut psdu = vec![0x01, 0x00, 0x42];
    for index in 0..122u8 {
        psdu.push(index.wrapping_mul(73).wrapping_add(19));
    }
    let fcs = ieee802154_fcs(&psdu);
    psdu.extend_from_slice(&fcs.to_le_bytes());
    assert_eq!(psdu.len(), 127);
    assert!(has_valid_fcs(&psdu));
    psdu
}

fn active_waveform_power(samples: &[Complex32]) -> f64 {
    let first = samples
        .iter()
        .position(|sample| sample.norm_sqr() != 0.0)
        .expect("upstream waveform has active samples");
    let last = samples
        .iter()
        .rposition(|sample| sample.norm_sqr() != 0.0)
        .expect("upstream waveform has active samples");
    let active = &samples[first..=last];
    active
        .iter()
        .map(|sample| f64::from(sample.norm_sqr()))
        .sum::<f64>()
        / active.len() as f64
}

const FRACTIONAL_DELAY_TAPS: usize = 33;
const FRACTIONAL_DELAY_CENTER: usize = FRACTIONAL_DELAY_TAPS / 2;

fn fractional_delay_taps(offset_numerator: i32) -> [f64; FRACTIONAL_DELAY_TAPS] {
    assert!(
        (-TIMING_OFFSET_DENOMINATOR / 2..=TIMING_OFFSET_DENOMINATOR / 2)
            .contains(&offset_numerator)
    );
    let delay = f64::from(offset_numerator) / f64::from(TIMING_OFFSET_DENOMINATOR);
    let mut taps = [0.0; FRACTIONAL_DELAY_TAPS];
    for (index, tap) in taps.iter_mut().enumerate() {
        let position = index as f64 - FRACTIONAL_DELAY_CENTER as f64 - delay;
        let sinc = if position.abs() < f64::EPSILON {
            1.0
        } else {
            let angle = std::f64::consts::PI * position;
            angle.sin() / angle
        };
        let window_phase = index as f64 / (FRACTIONAL_DELAY_TAPS - 1) as f64;
        let window = 0.42 - 0.5 * (std::f64::consts::TAU * window_phase).cos()
            + 0.08 * (2.0 * std::f64::consts::TAU * window_phase).cos();
        *tap = sinc * window;
    }
    let sum = taps.iter().sum::<f64>();
    assert!(sum.is_finite() && sum.abs() > 0.5);
    for tap in &mut taps {
        *tap /= sum;
    }
    taps
}

fn total_waveform_energy(samples: &[Complex32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(sample.norm_sqr()))
        .sum()
}

/// Apply a bounded fractional-sample delay without changing output length.
///
/// Positive offsets implement `output[n] = input[n - delay]`. The fixed
/// 16-sample FIR group delay is removed and samples outside the input are zero.
fn apply_fractional_timing(samples: &[Complex32], offset_numerator: i32) -> Vec<Complex32> {
    if offset_numerator == 0 {
        return samples.to_vec();
    }
    assert!(!samples.is_empty());
    let taps = fractional_delay_taps(offset_numerator);
    let mut output = Vec::with_capacity(samples.len());
    for output_index in 0..samples.len() {
        let mut re = 0.0;
        let mut im = 0.0;
        for (tap_index, tap) in taps.iter().copied().enumerate() {
            let input_index =
                output_index as isize + FRACTIONAL_DELAY_CENTER as isize - tap_index as isize;
            if let Ok(input_index) = usize::try_from(input_index)
                && let Some(sample) = samples.get(input_index)
            {
                re += tap * f64::from(sample.re);
                im += tap * f64::from(sample.im);
            }
        }
        output.push(Complex32::new(re as f32, im as f32));
    }

    let input_energy = total_waveform_energy(samples);
    let output_energy = total_waveform_energy(&output);
    assert!(input_energy.is_finite() && input_energy > 0.0);
    assert!(output_energy.is_finite() && output_energy > 0.0);
    let gain = (input_energy / output_energy).sqrt();
    assert!(gain.is_finite() && (0.95..=1.05).contains(&gain));
    let gain = gain as f32;
    for sample in &mut output {
        *sample *= gain;
        assert!(sample.re.is_finite() && sample.im.is_finite());
    }
    output
}

/// Apply a bounded receiver sample-rate error with direct sinc interpolation.
///
/// Positive ppm means more observed samples per chip. The output therefore has
/// `round(input_len * scale)` samples and samples the input at `n / scale`.
fn apply_sample_rate_error(samples: &[Complex32], rate_error_ppm: i32) -> Vec<Complex32> {
    assert!(TIMING_RATE_ERROR_PPM.contains(&rate_error_ppm));
    if rate_error_ppm == 0 {
        return samples.to_vec();
    }
    assert!(!samples.is_empty());
    assert!(
        samples
            .iter()
            .all(|sample| sample.re.is_finite() && sample.im.is_finite())
    );

    let scale = 1.0 + f64::from(rate_error_ppm) / TIMING_RATE_PPM_DENOMINATOR;
    assert!(scale.is_finite() && scale > 0.0);
    let output_len = (samples.len() as f64 * scale).round() as usize;
    assert!(output_len > 0);
    let cutoff = scale.min(1.0);
    let mut output = Vec::with_capacity(output_len);
    for output_index in 0..output_len {
        let source_position = output_index as f64 / scale;
        let source_center = source_position.floor() as isize;
        let mut tap_sum = 0.0;
        let mut re = 0.0;
        let mut im = 0.0;
        for tap_index in 0..FRACTIONAL_DELAY_TAPS {
            let input_index = source_center + tap_index as isize - FRACTIONAL_DELAY_CENTER as isize;
            let distance = source_position - input_index as f64;
            let sinc_position = cutoff * distance;
            let sinc = if sinc_position.abs() < f64::EPSILON {
                1.0
            } else {
                let angle = std::f64::consts::PI * sinc_position;
                angle.sin() / angle
            };
            let window_phase = tap_index as f64 / (FRACTIONAL_DELAY_TAPS - 1) as f64;
            let window = 0.42 - 0.5 * (std::f64::consts::TAU * window_phase).cos()
                + 0.08 * (2.0 * std::f64::consts::TAU * window_phase).cos();
            let tap = cutoff * sinc * window;
            tap_sum += tap;
            if let Ok(input_index) = usize::try_from(input_index)
                && let Some(sample) = samples.get(input_index)
            {
                re += tap * f64::from(sample.re);
                im += tap * f64::from(sample.im);
            }
        }
        assert!(tap_sum.is_finite() && tap_sum.abs() > 0.5);
        output.push(Complex32::new((re / tap_sum) as f32, (im / tap_sum) as f32));
    }

    let input_energy = total_waveform_energy(samples);
    let output_energy = total_waveform_energy(&output);
    let target_energy = input_energy * scale;
    assert!(target_energy.is_finite() && target_energy > 0.0);
    assert!(output_energy.is_finite() && output_energy > 0.0);
    let gain = (target_energy / output_energy).sqrt();
    assert!(gain.is_finite() && (0.95..=1.05).contains(&gain));
    let gain = gain as f32;
    for sample in &mut output {
        *sample *= gain;
        assert!(sample.re.is_finite() && sample.im.is_finite());
    }
    output
}

fn apply_carrier_offset(
    samples: &[Complex32],
    carrier_offset_hz: f64,
    sample_rate_hz: f64,
) -> Vec<Complex32> {
    assert!(carrier_offset_hz.is_finite());
    assert!(sample_rate_hz.is_finite() && sample_rate_hz > 0.0);
    let first_active = samples
        .iter()
        .position(|sample| sample.norm_sqr() != 0.0)
        .expect("upstream waveform has active samples");
    samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let relative_index = index as f64 - first_active as f64;
            let phase = std::f64::consts::TAU * carrier_offset_hz * relative_index / sample_rate_hz;
            let (sin, cos) = phase.sin_cos();
            *sample * Complex32::new(cos as f32, sin as f32)
        })
        .collect()
}

fn add_awgn(samples: &[Complex32], sample_snr_db: f64, seed: u64) -> Vec<Complex32> {
    let signal_power = active_waveform_power(samples);
    let noise_power = signal_power / 10.0f64.powf(sample_snr_db / 10.0);
    let component_sigma = (noise_power / 2.0).sqrt();
    let mut noise = SplitMix64::new(seed);
    samples
        .iter()
        .map(|sample| {
            Complex32::new(
                sample.re + (component_sigma * noise.gaussian()) as f32,
                sample.im + (component_sigma * noise.gaussian()) as f32,
            )
        })
        .collect()
}

fn noise_only(
    sample_count: usize,
    reference_signal_power: f64,
    sample_snr_db: f64,
    seed: u64,
) -> Vec<Complex32> {
    let noise_power = reference_signal_power / 10.0f64.powf(sample_snr_db / 10.0);
    let component_sigma = (noise_power / 2.0).sqrt();
    let mut noise = SplitMix64::new(seed);
    (0..sample_count)
        .map(|_| {
            Complex32::new(
                (component_sigma * noise.gaussian()) as f32,
                (component_sigma * noise.gaussian()) as f32,
            )
        })
        .collect()
}

fn append_cf32_le(bytes: &mut Vec<u8>, samples: &[Complex32]) {
    bytes.reserve(samples.len() * 8);
    for sample in samples {
        bytes.extend_from_slice(&sample.re.to_le_bytes());
        bytes.extend_from_slice(&sample.im.to_le_bytes());
    }
}

fn archive_positioned_candidates(path: &Path, expected_channel: u8) -> Vec<(u64, Vec<u8>)> {
    let bytes = fs::read(path).expect("read impairment candidate archive");
    assert!(bytes.len() >= 24);
    assert_eq!(&bytes[..8], b"I154RAW\0");
    assert_eq!(u16::from_le_bytes(bytes[8..10].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(bytes[10..12].try_into().unwrap()), 24);
    assert_eq!(bytes[12], expected_channel);
    assert_eq!(&bytes[13..16], &[0, 0, 0]);
    assert_eq!(
        u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
        4_000_000
    );
    assert_eq!(
        u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        2_000_000
    );

    let mut candidates = Vec::new();
    let mut cursor = 24;
    let mut prior_chip_count = 0u64;
    while cursor < bytes.len() {
        assert!(bytes.len() - cursor >= 20);
        assert_eq!(&bytes[cursor..cursor + 4], b"PSDU");
        let record_len =
            u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;
        let chip_count = u64::from_le_bytes(bytes[cursor + 8..cursor + 16].try_into().unwrap());
        let psdu_len =
            u16::from_le_bytes(bytes[cursor + 16..cursor + 18].try_into().unwrap()) as usize;
        assert!(chip_count > prior_chip_count);
        assert!((1..=127).contains(&psdu_len));
        assert_eq!(&bytes[cursor + 18..cursor + 20], &[0, 0]);
        assert_eq!(record_len, 20 + psdu_len);
        assert!(record_len <= bytes.len() - cursor);
        candidates.push((chip_count, bytes[cursor + 20..cursor + record_len].to_vec()));
        cursor += record_len;
        prior_chip_count = chip_count;
    }
    assert_eq!(cursor, bytes.len());
    candidates
}

fn archive_candidates(path: &Path, expected_channel: u8) -> Vec<Vec<u8>> {
    archive_positioned_candidates(path, expected_channel)
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect()
}

fn count_candidates(candidates: &[Vec<u8>], transmitted: &[Vec<u8>]) -> CandidateCounts {
    assert!(transmitted.len() <= u8::BITS as usize);
    assert!(transmitted.iter().all(|psdu| has_valid_fcs(psdu)));
    let mut counts = CandidateCounts {
        recovered_mask: 0,
        invalid_fcs: 0,
        unexpected_valid_fcs: 0,
        duplicate_transmitted: 0,
    };
    for candidate in candidates {
        if !has_valid_fcs(candidate) {
            counts.invalid_fcs += 1;
            continue;
        }
        let Some(index) = transmitted.iter().position(|psdu| psdu == candidate) else {
            counts.unexpected_valid_fcs += 1;
            continue;
        };
        let bit = 1 << index;
        if counts.recovered_mask & bit == 0 {
            counts.recovered_mask |= bit;
        } else {
            counts.duplicate_transmitted += 1;
        }
    }
    counts
}

fn valid_ack(sequence: u8) -> Vec<u8> {
    let mut psdu = vec![0x02, 0x00, sequence];
    psdu.extend_from_slice(&ieee802154_fcs(&psdu).to_le_bytes());
    assert!(has_valid_fcs(&psdu));
    psdu
}

fn mix_and_quantize_20_msps(waveforms: &[Vec<Complex32>], offsets_hz: &[i64]) -> (Vec<u8>, i16) {
    const SAMPLE_RATE_HZ: i64 = 20_000_000;
    const MIXER_PERIOD_SAMPLES: usize = 20;
    const TARGET_COMPONENT_PEAK: f32 = 1535.0;

    assert_eq!(waveforms.len(), offsets_hz.len());
    let sample_count = waveforms[0].len();
    assert!(
        waveforms
            .iter()
            .all(|waveform| waveform.len() == sample_count)
    );
    let mut composite = vec![Complex32::new(0.0, 0.0); sample_count];
    for (waveform, offset_hz) in waveforms.iter().zip(offsets_hz) {
        assert_eq!(
            *offset_hz * MIXER_PERIOD_SAMPLES as i64 % SAMPLE_RATE_HZ,
            0,
            "mixer lookup table must span an integer number of cycles"
        );
        let oscillator: [Complex32; MIXER_PERIOD_SAMPLES] = std::array::from_fn(|index| {
            let phase =
                std::f64::consts::TAU * *offset_hz as f64 * index as f64 / SAMPLE_RATE_HZ as f64;
            Complex32::new(phase.cos() as f32, phase.sin() as f32)
        });
        for (index, sample) in waveform.iter().enumerate() {
            composite[index] += *sample * oscillator[index % MIXER_PERIOD_SAMPLES];
        }
    }

    let source_peak = composite
        .iter()
        .flat_map(|sample| [sample.re.abs(), sample.im.abs()])
        .fold(0.0f32, f32::max);
    assert!(source_peak.is_finite() && source_peak > 0.0);
    let gain = TARGET_COMPONENT_PEAK / source_peak;
    let mut bytes = Vec::with_capacity(sample_count * 4);
    let mut quantized_peak = 0i16;
    for sample in composite {
        for component in [sample.re, sample.im] {
            let scaled = (component * gain).round();
            assert!(scaled.is_finite());
            assert!((-TARGET_COMPONENT_PEAK..=TARGET_COMPONENT_PEAK).contains(&scaled));
            let quantized = scaled as i16;
            quantized_peak = quantized_peak.max(quantized.abs());
            bytes.extend_from_slice(&quantized.to_le_bytes());
        }
    }
    assert_eq!(quantized_peak, TARGET_COMPONENT_PEAK as i16);
    (bytes, quantized_peak)
}

fn appended_json(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    name.into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn full_20_msps_channelizer_fixture_routes_three_valid_frames_to_both_lanes() {
    const CENTER_HZ: u64 = 2_437_000_000;
    const SAMPLE_RATE_HZ: u64 = 20_000_000;
    const EXPECTED_SOURCE_SAMPLES: usize = 406_930;
    const EXPECTED_CHANNEL_SAMPLES: u64 = 81_386;
    const CHANNELS: [(u8, i64, u64, u8); 3] = [
        (16, -7_000_000, 1_000_000, 0x16),
        (17, -2_000_000, 6_000_000, 0x17),
        (18, 3_000_000, 5_000_000, 0x18),
    ];

    let suite_started = Instant::now();
    assert_eq!(ieee802154_fcs(&ACK_PSDU[..3]), 0xd5a6);
    assert!(has_valid_fcs(&ACK_PSDU));
    let psdus = CHANNELS
        .iter()
        .map(|(_, _, _, sequence)| valid_ack(*sequence))
        .collect::<Vec<_>>();
    assert!(psdus.iter().all(|psdu| has_valid_fcs(psdu)));
    assert!(psdus[0] != psdus[1] && psdus[0] != psdus[2] && psdus[1] != psdus[2]);

    let fixture_started = Instant::now();
    let waveforms = psdus
        .iter()
        .map(|psdu| interpolate_4_to_20_msps(upstream_waveform(psdu)))
        .collect::<Vec<_>>();
    assert!(
        waveforms
            .iter()
            .all(|waveform| waveform.len() == EXPECTED_SOURCE_SAMPLES)
    );
    let offsets = CHANNELS
        .iter()
        .map(|(_, offset, _, _)| *offset)
        .collect::<Vec<_>>();
    let (source_bytes, quantized_peak) = mix_and_quantize_20_msps(&waveforms, &offsets);
    assert_eq!(source_bytes.len(), EXPECTED_SOURCE_SAMPLES * 4);
    let fixture_elapsed = fixture_started.elapsed();

    let directory = tempfile::tempdir().expect("create 20 MSps fixture directory");
    let input = directory.path().join("three-channel.sc16q11");
    fs::write(&input, &source_bytes).expect("write deterministic SC16_Q11 fixture");
    let sidecar = serde_json::json!({
        "format": "SC16_Q11",
        "dtype": "<i2",
        "interleave": "I,Q",
        "complex_scale": 2048,
        "frequency_hz": CENTER_HZ,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "bandwidth_hz": SAMPLE_RATE_HZ,
        "num_samples": EXPECTED_SOURCE_SAMPLES,
        "channel": "synthetic-fixture"
    });
    fs::write(
        appended_json(&input),
        serde_json::to_vec_pretty(&sidecar).expect("serialize fixture sidecar"),
    )
    .expect("write fixture sidecar");

    let channelized = directory.path().join("channelized");
    let channelize_started = Instant::now();
    channelize::run(channelize::Request {
        input: input.clone(),
        output: channelized.clone(),
        channels: CHANNELS.iter().map(|(channel, _, _, _)| *channel).collect(),
        max_samples: EXPECTED_SOURCE_SAMPLES as u64,
    })
    .expect("run real 20-to-4 MSps channelizer");
    let channelize_elapsed = channelize_started.elapsed();

    let manifest_bytes =
        fs::read(channelized.join("channelize.json")).expect("read channelizer fixture manifest");
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest_bytes).expect("parse channelizer fixture manifest");
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["operation"], "offline_ieee802154_channelization");
    assert_eq!(manifest["source"]["format"], "SC16_Q11_LE");
    assert_eq!(manifest["source"]["center_frequency_hz"], CENTER_HZ);
    assert_eq!(manifest["source"]["sample_rate_hz"], SAMPLE_RATE_HZ);
    assert_eq!(manifest["source"]["bandwidth_hz"], SAMPLE_RATE_HZ);
    assert_eq!(
        manifest["source"]["capture_channel_label"],
        "synthetic-fixture"
    );
    assert_eq!(
        manifest["source"]["available_complex_samples"],
        EXPECTED_SOURCE_SAMPLES as u64
    );
    assert_eq!(
        manifest["source"]["processed_complex_samples"],
        EXPECTED_SOURCE_SAMPLES as u64
    );
    assert_eq!(manifest["source"]["processed_full_input"], true);
    assert_eq!(
        manifest["source"]["source_bytes"],
        source_bytes.len() as u64
    );
    assert_eq!(
        manifest["source"]["processed_prefix_bytes"],
        source_bytes.len() as u64
    );
    assert_eq!(
        manifest["source"]["processed_prefix_sha256"],
        sha256_hex(&source_bytes)
    );
    assert_eq!(manifest["filter"]["taps"], 225);
    assert_eq!(manifest["filter"]["decimation"], 5);
    assert_eq!(manifest["filter"]["tail_flushed"], false);

    let reports = manifest["channels"]
        .as_array()
        .expect("channel reports are an array");
    assert_eq!(reports.len(), CHANNELS.len());
    let mut channel_files = Vec::new();
    for (channel, offset_hz, margin_hz, _) in CHANNELS {
        let report = reports
            .iter()
            .find(|report| report["channel"] == channel)
            .expect("manifest contains requested channel");
        let center_frequency_hz = 2_405_000_000 + u64::from(channel - 11) * 5_000_000;
        assert_eq!(report["center_frequency_hz"], center_frequency_hz);
        assert_eq!(report["source_offset_hz"], offset_hz);
        assert_eq!(report["source_edge_margin_hz"], margin_hz);
        assert_eq!(report["output_format"], "CF32_LE");
        assert_eq!(report["output_sample_rate_hz"], 4_000_000);
        assert_eq!(report["output_complex_samples"], EXPECTED_CHANNEL_SAMPLES);
        let output_file = format!("channel-{channel}.cf32");
        assert_eq!(report["output_file"], output_file);
        let output = channelized.join(output_file);
        let bytes = fs::read(&output).expect("read channelized CF32 fixture");
        assert_eq!(bytes.len() as u64, EXPECTED_CHANNEL_SAMPLES * 8);
        assert_eq!(
            report["sha256"].as_str().expect("channel digest is text"),
            sha256_hex(&bytes)
        );
        channel_files.push((channel, output));
    }

    let worker_started = Instant::now();
    for (expected_index, (channel, input)) in channel_files.iter().enumerate() {
        let mut lane_archives = Vec::new();
        for lane in [DecoderLane::Hard, DecoderLane::Soft] {
            let output = directory
                .path()
                .join(format!("channel-{channel}-{}.i154raw", lane.as_str()));
            run(Args {
                input: input.clone(),
                output: output.clone(),
                source_index_output: None,
                channel: *channel,
                max_input_mib: 1,
                max_frames: 16,
                max_output_mib: 1,
                decoder_lane: lane,
                carrier_offset_hz: 0,
                executor_threads: None,
            })
            .expect("run production worker lane on channelized fixture");
            let candidates = archive_candidates(&output, *channel);
            assert_eq!(candidates, vec![psdus[expected_index].clone()]);
            assert!(candidates.iter().all(|candidate| has_valid_fcs(candidate)));
            assert_eq!(
                count_candidates(&candidates, &psdus),
                CandidateCounts {
                    recovered_mask: 1 << expected_index,
                    invalid_fcs: 0,
                    unexpected_valid_fcs: 0,
                    duplicate_transmitted: 0,
                }
            );
            lane_archives.push(fs::read(output).expect("read worker candidate archive"));
        }
        assert_eq!(lane_archives[0], lane_archives[1]);
    }
    let worker_elapsed = worker_started.elapsed();
    eprintln!(
        "20msps fixture source_samples={} channel_samples={} q11_component_peak={} fixture_ms={} channelizer_ms={} six_worker_runs_ms={} total_ms={}",
        EXPECTED_SOURCE_SAMPLES,
        EXPECTED_CHANNEL_SAMPLES,
        quantized_peak,
        fixture_elapsed.as_millis(),
        channelize_elapsed.as_millis(),
        worker_elapsed.as_millis(),
        suite_started.elapsed().as_millis(),
    );
}

#[test]
fn full_20_msps_channelizer_h3_cfo_controls_cover_channels_16_through_18() {
    const CENTER_HZ: u64 = 2_437_000_000;
    const SAMPLE_RATE_HZ: u64 = 20_000_000;
    const CHANNEL_SAMPLE_RATE_HZ: f64 = 4_000_000.0;
    const EXPECTED_SOURCE_SAMPLES: usize = 406_930;
    const CHANNELS: [(u8, i64, u8); 3] = [
        (16, -7_000_000, 0x16),
        (17, -2_000_000, 0x17),
        (18, 3_000_000, 0x18),
    ];
    // Channel 16 exercises both signed endpoints across the two cases. The
    // other channels cover an off-hypothesis midpoint and the opposite endpoint.
    const ACTUAL_OFFSETS_HZ: [[i32; 3]; 2] =
        [[-100_000, -50_000, 100_000], [100_000, 50_000, -100_000]];

    let psdus = CHANNELS
        .iter()
        .map(|(_, _, sequence)| valid_ack(*sequence))
        .collect::<Vec<_>>();
    assert!(psdus.iter().all(|psdu| has_valid_fcs(psdu)));

    for (case_index, actual_offsets_hz) in ACTUAL_OFFSETS_HZ.iter().enumerate() {
        let waveforms = psdus
            .iter()
            .zip(actual_offsets_hz)
            .map(|(psdu, actual_offset_hz)| {
                interpolate_4_to_20_msps(apply_carrier_offset(
                    &upstream_waveform(psdu),
                    f64::from(*actual_offset_hz),
                    CHANNEL_SAMPLE_RATE_HZ,
                ))
            })
            .collect::<Vec<_>>();
        assert!(
            waveforms
                .iter()
                .all(|waveform| waveform.len() == EXPECTED_SOURCE_SAMPLES)
        );
        let nominal_offsets_hz = CHANNELS
            .iter()
            .map(|(_, offset_hz, _)| *offset_hz)
            .collect::<Vec<_>>();
        let (source_bytes, _) = mix_and_quantize_20_msps(&waveforms, &nominal_offsets_hz);

        let directory = tempfile::tempdir().expect("create H3 channelizer fixture directory");
        let input = directory
            .path()
            .join(format!("h3-cfo-case-{case_index}.sc16q11"));
        fs::write(&input, &source_bytes).expect("write H3 channelizer fixture");
        let sidecar = serde_json::json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": CENTER_HZ,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "num_samples": EXPECTED_SOURCE_SAMPLES,
            "channel": format!("synthetic-h3-cfo-case-{case_index}")
        });
        fs::write(
            appended_json(&input),
            serde_json::to_vec_pretty(&sidecar).expect("serialize H3 fixture sidecar"),
        )
        .expect("write H3 fixture sidecar");

        let channelized = directory.path().join("channelized");
        channelize::run(channelize::Request {
            input,
            output: channelized.clone(),
            channels: CHANNELS.iter().map(|(channel, _, _)| *channel).collect(),
            max_samples: EXPECTED_SOURCE_SAMPLES as u64,
        })
        .expect("run production channelizer on H3 fixture");

        for (channel_index, (channel, _, _)) in CHANNELS.iter().enumerate() {
            let channel_input = channelized.join(format!("channel-{channel}.cf32"));
            let mut observations = [[CandidateCounts::NONE; 2]; H3_HYPOTHESES_HZ.len()];
            for (hypothesis_index, hypothesis_hz) in H3_HYPOTHESES_HZ.iter().copied().enumerate() {
                for (lane_index, lane) in [DecoderLane::Hard, DecoderLane::Soft]
                    .into_iter()
                    .enumerate()
                {
                    let output = directory.path().join(format!(
                        "case-{case_index}-channel-{channel}-hypothesis-{hypothesis_hz:+}-{}.i154raw",
                        lane.as_str(),
                    ));
                    run(Args {
                        input: channel_input.clone(),
                        output: output.clone(),
                        source_index_output: None,
                        channel: *channel,
                        max_input_mib: 1,
                        max_frames: 16,
                        max_output_mib: 1,
                        decoder_lane: lane,
                        carrier_offset_hz: hypothesis_hz,
                        executor_threads: None,
                    })
                    .expect("run H3 worker on channelized CFO fixture");
                    observations[hypothesis_index][lane_index] =
                        count_candidates(&archive_candidates(&output, *channel), &psdus);
                }
            }

            let expected_mask = 1 << channel_index;
            for lanes in observations {
                for counts in lanes {
                    assert!(counts.has_no_anomalies());
                    assert!(
                        counts.recovered_mask == 0 || counts.recovered_mask == expected_mask,
                        "channel {channel} recovered a cross-channel identity: {counts:?}"
                    );
                }
            }
            for lane_index in 0..2 {
                assert!(
                    observations
                        .iter()
                        .any(|lanes| lanes[lane_index].recovered_mask == expected_mask),
                    "case {case_index} channel {channel} actual offset {} Hz lacks H3 lane {lane_index} coverage: {observations:?}",
                    actual_offsets_hz[channel_index],
                );
            }
        }
    }
}

fn run_impairment_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> [CandidateCounts; 2] {
    run_impairment_case_with_carrier_offset(name, channel, samples, transmitted, 0)
}

fn run_impairment_case_with_carrier_offset(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
    carrier_offset_hz: i32,
) -> [CandidateCounts; 2] {
    let directory = tempfile::tempdir().expect("create impairment case directory");
    let input = directory.path().join(format!("{name}.cf32"));
    let mut bytes = Vec::with_capacity(samples.len() * 8);
    append_cf32_le(&mut bytes, samples);
    fs::write(&input, bytes).expect("write deterministic impairment samples");
    let input_mib = ((samples.len() as u64 * 8).div_ceil(1024 * 1024)) as u32;
    let mut observations = [CandidateCounts::NONE; 2];
    for (index, (lane_name, lane)) in [("hard", DecoderLane::Hard), ("soft", DecoderLane::Soft)]
        .into_iter()
        .enumerate()
    {
        let output = directory.path().join(format!("{name}-{lane_name}.i154raw"));
        let started = Instant::now();
        run(Args {
            input: input.clone(),
            output: output.clone(),
            source_index_output: None,
            channel,
            max_input_mib: input_mib.max(1),
            max_frames: 1024,
            max_output_mib: 1,
            decoder_lane: lane,
            carrier_offset_hz,
            executor_threads: None,
        })
        .expect("run deterministic impairment case");
        let elapsed = started.elapsed();
        observations[index] = count_candidates(&archive_candidates(&output, channel), transmitted);
        eprintln!(
            "impairment case={name} channel={channel} lane={lane_name} valid_recoveries={} recovered_mask={:#04b} invalid_fcs={} unexpected_valid_fcs={} duplicate_transmitted={} elapsed_ms={}",
            observations[index].valid_recoveries(),
            observations[index].recovered_mask,
            observations[index].invalid_fcs,
            observations[index].unexpected_valid_fcs,
            observations[index].duplicate_transmitted,
            elapsed.as_millis(),
        );
    }
    observations
}

#[derive(Clone, Copy)]
enum TimingClockMode {
    Established,
    Fixed,
    FixedRate(i32),
    GainSweep(usize),
}

#[derive(Clone, Copy)]
struct TimingArm {
    name: &'static str,
    lane: DecoderLane,
    mu_sixteenths: u8,
    mode: TimingClockMode,
}

const FIXED_TIMING_ARMS: [TimingArm; 4] = [
    TimingArm {
        name: "adaptive-hard-mu-8",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::Established,
    },
    TimingArm {
        name: "adaptive-soft-mu-8",
        lane: DecoderLane::Soft,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::Established,
    },
    TimingArm {
        name: "fixed-hard-mu-4",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[0],
        mode: TimingClockMode::Fixed,
    },
    TimingArm {
        name: "fixed-hard-mu-12",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[1],
        mode: TimingClockMode::Fixed,
    },
];

const RATE_SCREEN_TIMING_ARMS: [TimingArm; 8] = [
    FIXED_TIMING_ARMS[0],
    FIXED_TIMING_ARMS[1],
    FIXED_TIMING_ARMS[2],
    FIXED_TIMING_ARMS[3],
    TimingArm {
        name: "fixed-rate-minus-100-mu-4",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[0],
        mode: TimingClockMode::FixedRate(-100),
    },
    TimingArm {
        name: "fixed-rate-minus-100-mu-12",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[1],
        mode: TimingClockMode::FixedRate(-100),
    },
    TimingArm {
        name: "fixed-rate-plus-100-mu-4",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[0],
        mode: TimingClockMode::FixedRate(100),
    },
    TimingArm {
        name: "fixed-rate-plus-100-mu-12",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_FIXED_CLOCK_MU_SIXTEENTHS[1],
        mode: TimingClockMode::FixedRate(100),
    },
];

const GAIN_SWEEP_TIMING_ARMS: [TimingArm; 4] = [
    TimingArm {
        name: "adaptive-hard-gain-mu-0p0075",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::GainSweep(0),
    },
    TimingArm {
        name: "adaptive-hard-gain-mu-0p015",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::GainSweep(1),
    },
    TimingArm {
        name: "adaptive-hard-gain-mu-0p03",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::GainSweep(2),
    },
    TimingArm {
        name: "adaptive-hard-gain-mu-0p06",
        lane: DecoderLane::Hard,
        mu_sixteenths: TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS,
        mode: TimingClockMode::GainSweep(3),
    },
];

type TimingCandidateObservations = [CandidateCounts; 4];
type RateScreenTimingCandidateObservations = [CandidateCounts; 8];
type GainSweepTimingCandidateObservations = [CandidateCounts; 4];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BurstTimingCandidateObservation {
    counts: [CandidateCounts; 5],
    shr: ShrRateObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ShrRateObservation {
    counts: CandidateCounts,
    stats: BurstClockStats,
    identity_ticks: [Option<u64>; 2],
    input_samples: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrozenShrRateObservation {
    counts: CandidateCounts,
    markers: u64,
    shr_locks: u64,
    shr_rejections: u64,
    shr_selections: [Option<ShrTimingSelection>; 2],
    max_shr_metric_replay: usize,
    identity_ticks: [Option<u64>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JointShrRateObservation {
    counts: CandidateCounts,
    stats: ShrRateClockStats,
    identity_ticks: [Option<u64>; 2],
    input_samples: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeferredShrRateObservation {
    counts: CandidateCounts,
    stats: DeferredShrRateClockStats,
    identity_ticks: [Option<u64>; 2],
    candidate_lengths: [u8; 2],
    input_samples: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeferredDevelopmentObservation {
    seed_index: usize,
    seed: u64,
    phase_index: usize,
    rate_index: usize,
    phase_sixteenths: i32,
    input_rate_error_ppm: i32,
    input_sha256: String,
    events: [DetectedBurst; 2],
    deferred: DeferredShrRateObservation,
}

struct ThreeCellDiagnosticTrace {
    phase_sixteenths: i32,
    input_rate_error_ppm: i32,
    burst: ShrRateDiagnosticTrace,
    decoder: PositionedDecoderTraceStats,
}

// Frozen low, center and high cells opened by the diagnostic before deferred
// qualification; order is part of both tests' provenance contract.
const THREE_CELL_CASES: [(usize, usize, i32, i32); 3] =
    [(0, 0, -4, -100), (1, 1, 0, 0), (2, 2, 8, 100)];
const EXPECTED_THREE_CELL_EVENTS: [[DetectedBurst; 2]; 3] = [
    [
        DetectedBurst {
            replay_start: 39_952,
            detected_at: 40_015,
            end_exclusive: 41_491,
            forced_end: false,
        },
        DetectedBurst {
            replay_start: 121_353,
            detected_at: 121_416,
            end_exclusive: 138_508,
            forced_end: false,
        },
    ],
    [
        DetectedBurst {
            replay_start: 39_956,
            detected_at: 40_019,
            end_exclusive: 41_499,
            forced_end: false,
        },
        DetectedBurst {
            replay_start: 121_367,
            detected_at: 121_430,
            end_exclusive: 138_523,
            forced_end: false,
        },
    ],
    [
        DetectedBurst {
            replay_start: 39_963,
            detected_at: 40_026,
            end_exclusive: 41_505,
            forced_end: false,
        },
        DetectedBurst {
            replay_start: 121_379,
            detected_at: 121_442,
            end_exclusive: 138_537,
            forced_end: false,
        },
    ],
];

macro_rules! write_trace {
    ($output:expr, $($argument:tt)*) => {
        write!($output, $($argument)*).expect("write compact diagnostic trace")
    };
}

fn write_optional_u8(output: &mut String, value: Option<u8>) {
    match value {
        Some(value) => write_trace!(output, "s({value})"),
        None => output.push('n'),
    }
}

fn write_optional_u64(output: &mut String, value: Option<u64>) {
    match value {
        Some(value) => write_trace!(output, "s({value})"),
        None => output.push('n'),
    }
}

fn write_timing_selection(output: &mut String, selection: &ShrRateTimingSelection) {
    write_trace!(
        output,
        "r({},{},{},{})",
        selection.timing.start_offset,
        selection.timing.mu_sixteenths,
        selection.timing.source_sample_index,
        selection.rate_error_ppm
    );
}

fn write_scored_candidate(output: &mut String, candidate: &ShrRateScoredCandidateTrace) {
    output.push_str("c(");
    write_timing_selection(output, &candidate.selection);
    write_trace!(output, ",{:016x},[", candidate.full_score_bits);
    for (index, score_bits) in candidate.suffix_score_bits.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        match score_bits {
            Some(score_bits) => write_trace!(output, "s({score_bits:016x})"),
            None => output.push('n'),
        }
    }
    output.push_str("],");
    write_optional_u8(output, candidate.limiting_suffix_symbol);
    output.push(')');
}

fn write_optional_scored_candidate(
    output: &mut String,
    candidate: Option<&ShrRateScoredCandidateTrace>,
) {
    match candidate {
        Some(candidate) => {
            output.push_str("s(");
            write_scored_candidate(output, candidate);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_selector_audit(output: &mut String, selector: &ShrRateSelectorAudit) {
    write_trace!(
        output,
        "a({},{},",
        selector.evaluated_hypotheses,
        selector.eligible_hypotheses
    );
    write_optional_scored_candidate(output, selector.best_unconstrained.as_ref());
    output.push(',');
    write_optional_scored_candidate(output, selector.best_eligible.as_ref());
    output.push(')');
}

fn write_optional_selector(output: &mut String, selector: Option<&ShrRateSelectorAudit>) {
    match selector {
        Some(selector) => {
            output.push_str("s(");
            write_selector_audit(output, selector);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_trace_outcome(output: &mut String, outcome: &ShrRateTraceOutcome) {
    match outcome {
        ShrRateTraceOutcome::Locked(selection) => {
            output.push_str("l(");
            write_timing_selection(output, selection);
            output.push(')');
        }
        ShrRateTraceOutcome::RejectedNoEligible => output.push_str("rne"),
        ShrRateTraceOutcome::RejectedBeforeSelection => output.push_str("rbs"),
        ShrRateTraceOutcome::RejectedBeforeLock(selection) => {
            output.push_str("rbl(");
            write_timing_selection(output, selection);
            output.push(')');
        }
    }
}

fn write_clock_boundary(output: &mut String, boundary: &ClockBoundaryPoint) {
    write_trace!(
        output,
        "p({},{},{:08x},{:08x})",
        boundary.chip_ordinal,
        boundary.source_sample_index,
        boundary.omega_bits,
        boundary.mu_bits
    );
}

fn write_clock_trace(output: &mut String, clock: &ClockTrajectoryTrace) {
    write_trace!(
        output,
        "k({:08x},{:08x},{:08x},{:08x},{:08x},{:08x},{:08x},{:08x},{},{},",
        clock.initial_omega_bits,
        clock.initial_mu_bits,
        clock.final_omega_bits,
        clock.final_mu_bits,
        clock.min_omega_bits,
        clock.max_omega_bits,
        clock.min_mu_bits,
        clock.max_mu_bits,
        clock.emitted_chips,
        clock.markers
    );
    write_optional_u64(output, clock.first_source_sample_index);
    output.push(',');
    write_optional_u64(output, clock.last_source_sample_index);
    output.push_str(",[");
    for (index, count) in clock.advance_histogram.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        write_trace!(output, "{count}");
    }
    write_trace!(
        output,
        "],{},{}:[",
        clock.advance_overflow,
        clock.boundaries.len()
    );
    for (index, boundary) in clock.boundaries.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        write_clock_boundary(output, boundary);
    }
    write_trace!(output, "],{})", clock.boundary_overflow);
}

fn write_optional_clock(output: &mut String, clock: Option<&ClockTrajectoryTrace>) {
    match clock {
        Some(clock) => {
            output.push_str("s(");
            write_clock_trace(output, clock);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_burst_event(output: &mut String, event: &ShrRateEventTrace) {
    write_trace!(
        output,
        "e(d({},{},{},{}),",
        event.event.replay_start,
        event.event.detected_at,
        event.event.end_exclusive,
        u8::from(event.event.forced_end)
    );
    write_optional_selector(output, event.selector.as_ref());
    output.push(',');
    write_trace_outcome(output, &event.outcome);
    output.push(',');
    write_optional_clock(output, event.clock.as_ref());
    output.push(')');
}

fn write_optional_burst_event(output: &mut String, event: Option<&ShrRateEventTrace>) {
    match event {
        Some(event) => {
            output.push_str("s(");
            write_burst_event(output, event);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_burst_trace(output: &mut String, trace: &ShrRateDiagnosticTrace) {
    output.push_str("b(2:[");
    for (index, event) in trace.events.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        write_optional_burst_event(output, event.as_ref());
    }
    write_trace!(output, "],{})", trace.overflow);
}

fn write_decoder_point(output: &mut String, point: &DecoderTracePoint) {
    write_trace!(
        output,
        "p({},{})",
        point.source_sample_index,
        point.event_chip_ordinal
    );
}

fn write_decoder_decision(output: &mut String, decision: &DecoderDecisionPoint) {
    output.push_str("q(");
    write_decoder_point(output, &decision.point);
    output.push(',');
    write_optional_u8(output, decision.observed_symbol);
    write_trace!(
        output,
        ",{},{:016x},{:016x})",
        decision.best_symbol,
        decision.best_score_bits,
        decision.runner_up_score_bits
    );
}

fn write_optional_decoder_decision(output: &mut String, decision: Option<&DecoderDecisionPoint>) {
    match decision {
        Some(decision) => {
            output.push_str("s(");
            write_decoder_decision(output, decision);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_decode_entry(output: &mut String, entry: &DecodeEntry) {
    write_trace!(output, "x({},", entry.length);
    write_decoder_point(output, &entry.point);
    output.push(')');
}

fn write_optional_decode_entry(output: &mut String, entry: Option<&DecodeEntry>) {
    match entry {
        Some(entry) => {
            output.push_str("s(");
            write_decode_entry(output, entry);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_optional_decoder_point(output: &mut String, point: Option<&DecoderTracePoint>) {
    match point {
        Some(point) => {
            output.push_str("s(");
            write_decoder_point(output, point);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_decoder_terminal(output: &mut String, terminal: &DecoderTerminal) {
    match terminal {
        DecoderTerminal::Search { chip_count } => write_trace!(output, "s({chip_count})"),
        DecoderTerminal::PreambleFound { chip_count } => {
            write_trace!(output, "p({chip_count})")
        }
        DecoderTerminal::SearchSfd { chip_count } => write_trace!(output, "f({chip_count})"),
        DecoderTerminal::SearchHeader {
            low_nibble,
            chip_count,
        } => {
            write_trace!(output, "h(");
            write_optional_u8(output, *low_nibble);
            write_trace!(output, ",{chip_count})");
        }
        DecoderTerminal::Decode {
            length,
            completed_bytes,
            low_nibble,
            chip_count,
        } => {
            write_trace!(output, "d({length},{completed_bytes},");
            write_optional_u8(output, *low_nibble);
            write_trace!(output, ",{chip_count})");
        }
    }
}

fn write_decoder_event(output: &mut String, event: &DecoderEventTrace) {
    write_trace!(
        output,
        "e({},{},{},{},{},{},{},{},{},",
        event.first_source_sample_index,
        event.last_source_sample_index,
        event.recovered_chips,
        event.preamble_entries,
        event.preamble_resets,
        event.sfd7_entries,
        event.sfd_resets,
        event.sfd10_entries,
        event.header_entries
    );
    write_optional_decoder_decision(output, event.last_sfd7.as_ref());
    output.push(',');
    write_optional_decoder_decision(output, event.last_sfd10.as_ref());
    write_trace!(
        output,
        ",{},{},",
        event.phr_low_decisions,
        event.phr_high_decisions
    );
    write_optional_decoder_decision(output, event.last_phr_low.as_ref());
    output.push(',');
    write_optional_decoder_decision(output, event.last_phr_high.as_ref());
    write_trace!(
        output,
        ",{},{},{},",
        event.header_classification_resets,
        event.invalid_phr_lengths,
        event.decode_entries
    );
    write_optional_decode_entry(output, event.last_decode_entry.as_ref());
    write_trace!(
        output,
        ",{},{},{},{},",
        event.payload_symbols,
        event.max_completed_payload_bytes,
        event.payload_classification_resets,
        event.completed_candidates
    );
    write_optional_decoder_point(output, event.last_completion.as_ref());
    output.push(',');
    write_decoder_terminal(output, &event.terminal);
    output.push(')');
}

fn write_optional_decoder_event(output: &mut String, event: Option<&DecoderEventTrace>) {
    match event {
        Some(event) => {
            output.push_str("s(");
            write_decoder_event(output, event);
            output.push(')');
        }
        None => output.push('n'),
    }
}

fn write_decoder_trace(output: &mut String, trace: &PositionedDecoderTraceStats) {
    output.push_str("d(2:[");
    for (index, event) in trace.events.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        write_optional_decoder_event(output, event.as_ref());
    }
    write_trace!(output, "],{})", trace.overflow);
}

fn serialize_three_cell_trace(cases: &[ThreeCellDiagnosticTrace; 3]) -> String {
    let mut output = String::new();
    write_trace!(output, "joint-shr-rate-trace-v1({}:[", cases.len());
    for (case_index, case) in cases.iter().enumerate() {
        if case_index != 0 {
            output.push(',');
        }
        write_trace!(
            output,
            "c({case_index},{},{},",
            case.phase_sixteenths,
            case.input_rate_error_ppm
        );
        write_burst_trace(&mut output, &case.burst);
        output.push(',');
        write_decoder_trace(&mut output, &case.decoder);
        output.push(')');
    }
    output.push_str("])");
    output
}

fn write_deferred_handoff(output: &mut String, handoff: &DeferredShrRateHandoff) {
    output.push_str("h(");
    write_timing_selection(output, &handoff.selection);
    write_trace!(
        output,
        ",{},{},{},{:08x},{:08x})",
        handoff.phr_length,
        handoff.payload_left_offset,
        handoff.payload_source_sample_index,
        handoff.payload_mu_bits,
        handoff.prior_metric_bits
    );
}

fn write_optional_deferred_handoff(output: &mut String, handoff: Option<&DeferredShrRateHandoff>) {
    match handoff {
        Some(handoff) => {
            output.push_str("s(");
            write_deferred_handoff(output, handoff);
            output.push(')');
        }
        None => output.push('n'),
    }
}

// Version 1 orders cell, event, candidate-count, completion, resource and
// handoff (`c/e/n/q/r/h`) records. Its hash freezes deterministic host
// diagnostics; selected rates are not clock calibration or RF evidence.
fn serialize_deferred_development(cases: &[DeferredDevelopmentObservation]) -> String {
    let mut output = String::new();
    write_trace!(output, "deferred-shr-rate-development-v1({}:[", cases.len());
    for (case_index, case) in cases.iter().enumerate() {
        if case_index != 0 {
            output.push(',');
        }
        let counts = case.deferred.counts;
        let burst = case.deferred.stats.burst;
        write_trace!(
            output,
            "c({case_index},{},{:016x},{},{},{},{},{},e([",
            case.seed_index,
            case.seed,
            case.phase_index,
            case.rate_index,
            case.phase_sixteenths,
            case.input_rate_error_ppm,
            case.input_sha256
        );
        for (event_index, event) in case.events.iter().enumerate() {
            if event_index != 0 {
                output.push(',');
            }
            write_trace!(
                output,
                "e({},{},{},{})",
                event.replay_start,
                event.detected_at,
                event.end_exclusive,
                u8::from(event.forced_end)
            );
        }
        write_trace!(
            output,
            "]),n({},{},{},{}),q([q(0,",
            counts.recovered_mask,
            counts.invalid_fcs,
            counts.unexpected_valid_fcs,
            counts.duplicate_transmitted
        );
        write_optional_u64(&mut output, case.deferred.identity_ticks[0]);
        write_trace!(output, ",{}),q(1,", case.deferred.candidate_lengths[0]);
        write_optional_u64(&mut output, case.deferred.identity_ticks[1]);
        write_trace!(
            output,
            ",{})]),r({},{},{},{},{},{},{},{},{},{},{},{},{},{}),[",
            case.deferred.candidate_lengths[1],
            case.deferred.input_samples,
            burst.source_samples,
            burst.starts,
            burst.ends,
            burst.markers,
            burst.max_pretrigger,
            burst.max_replay,
            burst.max_metrics,
            burst.shr_locks,
            burst.shr_rejections,
            burst.shr_selection_overflow,
            burst.max_shr_selection_metrics,
            burst.max_shr_metric_replay,
            case.deferred
                .stats
                .deferred_selections
                .iter()
                .flatten()
                .count()
        );
        for (handoff_index, handoff) in case.deferred.stats.deferred_selections.iter().enumerate() {
            if handoff_index != 0 {
                output.push(',');
            }
            write_optional_deferred_handoff(&mut output, handoff.as_ref());
        }
        output.push_str("])");
    }
    output.push_str("])");
    output
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrozenJointShrRateObservation {
    counts: CandidateCounts,
    markers: u64,
    shr_locks: u64,
    shr_rejections: u64,
    shr_rate_selections: [Option<ShrRateTimingSelection>; 2],
    max_shr_metric_replay: usize,
    identity_ticks: [Option<u64>; 2],
}

impl JointShrRateObservation {
    const fn frozen(self) -> FrozenJointShrRateObservation {
        FrozenJointShrRateObservation {
            counts: self.counts,
            markers: self.stats.burst.markers,
            shr_locks: self.stats.burst.shr_locks,
            shr_rejections: self.stats.burst.shr_rejections,
            shr_rate_selections: self.stats.shr_rate_selections,
            max_shr_metric_replay: self.stats.burst.max_shr_metric_replay,
            identity_ticks: self.identity_ticks,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JointShrRateGridObservation {
    prior: BurstTimingCandidateObservation,
    joint: JointShrRateObservation,
}

impl ShrRateObservation {
    const fn frozen(self) -> FrozenShrRateObservation {
        FrozenShrRateObservation {
            counts: self.counts,
            markers: self.stats.markers,
            shr_locks: self.stats.shr_locks,
            shr_rejections: self.stats.shr_rejections,
            shr_selections: self.stats.shr_selections,
            max_shr_metric_replay: self.stats.max_shr_metric_replay,
            identity_ticks: self.identity_ticks,
        }
    }
}

const fn shr_selection(
    start_offset: u8,
    mu_sixteenths: u8,
    source_sample_index: u64,
) -> ShrTimingSelection {
    ShrTimingSelection {
        start_offset,
        mu_sixteenths,
        source_sample_index,
    }
}

const fn shr_rate_selection(
    start_offset: u8,
    mu_sixteenths: u8,
    source_sample_index: u64,
    rate_error_ppm: i32,
) -> ShrRateTimingSelection {
    ShrRateTimingSelection {
        timing: shr_selection(start_offset, mu_sixteenths, source_sample_index),
        rate_error_ppm,
    }
}

const fn deferred_handoff(
    selection: ShrRateTimingSelection,
    phr_length: u8,
    payload_left_offset: u16,
    payload_source_sample_index: u64,
    payload_mu_bits: u32,
    prior_metric_bits: u32,
) -> DeferredShrRateHandoff {
    DeferredShrRateHandoff {
        selection,
        phr_length,
        payload_left_offset,
        payload_source_sample_index,
        payload_mu_bits,
        prior_metric_bits,
    }
}

const fn frozen_shr_rate(
    counts: CandidateCounts,
    first: ShrTimingSelection,
    second: Option<ShrTimingSelection>,
    max_shr_metric_replay: usize,
    identity_ticks: [Option<u64>; 2],
) -> FrozenShrRateObservation {
    let shr_locks = if second.is_some() { 2 } else { 1 };
    FrozenShrRateObservation {
        counts,
        markers: shr_locks,
        shr_locks,
        shr_rejections: 2 - shr_locks,
        shr_selections: [Some(first), second],
        max_shr_metric_replay,
        identity_ticks,
    }
}

const fn frozen_joint_shr_rate(
    counts: CandidateCounts,
    first: ShrRateTimingSelection,
    second: Option<ShrRateTimingSelection>,
    max_shr_metric_replay: usize,
    identity_ticks: [Option<u64>; 2],
) -> FrozenJointShrRateObservation {
    let shr_locks = if second.is_some() { 2 } else { 1 };
    FrozenJointShrRateObservation {
        counts,
        markers: shr_locks,
        shr_locks,
        shr_rejections: 2 - shr_locks,
        shr_rate_selections: [Some(first), second],
        max_shr_metric_replay,
        identity_ticks,
    }
}

fn run_timing_candidate_case<const ARM_COUNT: usize>(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
    arms: [TimingArm; ARM_COUNT],
) -> [CandidateCounts; ARM_COUNT] {
    let directory = tempfile::tempdir().expect("create timing candidate case directory");
    let input = directory.path().join(format!("{name}.cf32"));
    let mut bytes = Vec::with_capacity(samples.len() * 8);
    append_cf32_le(&mut bytes, samples);
    fs::write(&input, &bytes).expect("write deterministic timing samples");
    let input_mib = (bytes.len() as u64).div_ceil(1024 * 1024) as u32;
    let mut observations = [CandidateCounts::NONE; ARM_COUNT];
    for (index, arm) in arms.into_iter().enumerate() {
        let output = directory
            .path()
            .join(format!("{name}-{}.i154raw", arm.name));
        let args = Args {
            input: input.clone(),
            output: output.clone(),
            source_index_output: None,
            channel,
            max_input_mib: input_mib.max(1),
            max_frames: 1024,
            max_output_mib: 1,
            decoder_lane: arm.lane,
            carrier_offset_hz: 0,
            executor_threads: None,
        };
        let started = Instant::now();
        match arm.mode {
            TimingClockMode::Established => {
                run(args).expect("run ordinary adaptive timing arm");
            }
            TimingClockMode::Fixed => {
                let initial_clock_mu =
                    f32::from(arm.mu_sixteenths) / TIMING_OFFSET_DENOMINATOR as f32;
                run_with_fixed_clock_mu(args, initial_clock_mu).expect("run fixed timing arm");
            }
            TimingClockMode::FixedRate(rate_error_ppm) => {
                let initial_clock_mu =
                    f32::from(arm.mu_sixteenths) / TIMING_OFFSET_DENOMINATOR as f32;
                run_with_fixed_clock_rate(args, rate_error_ppm, initial_clock_mu)
                    .expect("run fixed-rate timing arm");
            }
            TimingClockMode::GainSweep(gain_index) => {
                run_with_clock_gain_sweep(args, gain_index)
                    .expect("run adaptive clock gain sweep arm");
            }
        }
        observations[index] = count_candidates(&archive_candidates(&output, channel), transmitted);
        eprintln!(
            "timing candidate case={name} arm={} mu={}/{TIMING_OFFSET_DENOMINATOR} valid_recoveries={} recovered_mask={:#04b} invalid_fcs={} unexpected_valid_fcs={} duplicate_transmitted={} elapsed_ms={}",
            arm.name,
            arm.mu_sixteenths,
            observations[index].valid_recoveries(),
            observations[index].recovered_mask,
            observations[index].invalid_fcs,
            observations[index].unexpected_valid_fcs,
            observations[index].duplicate_transmitted,
            started.elapsed().as_millis(),
        );
    }
    assert_eq!(
        fs::read(&input).expect("reread timing candidate input"),
        bytes,
        "timing candidate input changed between arms"
    );
    observations
}

fn run_fixed_timing_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> TimingCandidateObservations {
    run_timing_candidate_case(name, channel, samples, transmitted, FIXED_TIMING_ARMS)
}

fn run_rate_screen_timing_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> RateScreenTimingCandidateObservations {
    run_timing_candidate_case(name, channel, samples, transmitted, RATE_SCREEN_TIMING_ARMS)
}

fn run_gain_sweep_timing_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> GainSweepTimingCandidateObservations {
    run_timing_candidate_case(name, channel, samples, transmitted, GAIN_SWEEP_TIMING_ARMS)
}

fn count_causal_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
    expected_events: usize,
) -> CandidateCounts {
    let events = detect_causal_bursts(samples);
    assert_eq!(events.len(), expected_events);
    assert_eq!(
        events.len(),
        transmitted.len(),
        "each transmitted identity must have one ordered detector event"
    );
    assert!(events.iter().all(|event| !event.forced_end));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].end_exclusive <= pair[1].replay_start)
    );
    let observation = run_causal_burst_streaming_case(name, channel, samples);
    assert_eq!(observation.stats.source_samples, samples.len() as u64);
    assert_eq!(observation.stats.starts, expected_events as u64);
    assert_eq!(observation.stats.ends, expected_events as u64);
    assert_eq!(observation.stats.markers, expected_events as u64);
    assert!(observation.stats.max_pretrigger <= 64);
    assert!(observation.stats.max_replay <= 64);
    assert!(observation.stats.max_metrics <= 4);
    for (position, payload) in &observation.candidates {
        let identity_index = transmitted
            .iter()
            .position(|expected| expected == payload)
            .unwrap_or_else(|| panic!("candidate at tick {position} has no transmitted identity"));
        let event = events[identity_index];
        let first_tick = event.replay_start / 2 + 1;
        let last_tick = (event.end_exclusive - 1) / 2 + 1;
        assert!(
            (first_tick..=last_tick).contains(position),
            "candidate identity {identity_index} at tick {position} is outside its detector event {event:?}"
        );
    }
    let candidates = observation
        .candidates
        .into_iter()
        .map(|(_, payload)| payload)
        .collect::<Vec<_>>();
    let counts = count_candidates(&candidates, transmitted);
    eprintln!(
        "causal burst candidate case={name} events={events:?} stats={:?} counts={counts:?}",
        observation.stats
    );
    counts
}

fn run_burst_timing_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> BurstTimingCandidateObservation {
    let baseline = run_fixed_timing_candidate_case(name, channel, samples, transmitted);
    let burst = count_causal_burst_streaming_case(
        &format!("{name}-causal-burst-soft"),
        channel,
        samples,
        transmitted,
        2,
    );
    let events = detect_causal_bursts(samples);
    assert_eq!(events.len(), transmitted.len());
    assert!(events.iter().all(|event| !event.forced_end));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].end_exclusive <= pair[1].replay_start)
    );
    let shr =
        run_shr_burst_streaming_case(&format!("{name}-causal-burst-shr-soft"), channel, samples);
    let selection_count = shr.stats.shr_selections.iter().flatten().count() as u64;
    assert_eq!(selection_count, shr.stats.shr_locks);
    let mut selected_events = vec![false; events.len()];
    for selection in shr.stats.shr_selections.iter().flatten() {
        let selected_replay_start = selection
            .source_sample_index
            .checked_sub(u64::from(selection.start_offset))
            .expect("SHR selection start offset must not precede source sample zero");
        let matching_events = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.replay_start == selected_replay_start)
            .map(|(event_index, _)| event_index)
            .collect::<Vec<_>>();
        assert_eq!(
            matching_events.len(),
            1,
            "SHR selection {selection:?} must derive from exactly one detector event in {events:?}"
        );
        let event_index = matching_events[0];
        assert!(
            !selected_events[event_index],
            "detector event {event_index} produced more than one retained SHR selection"
        );
        selected_events[event_index] = true;
    }
    let mut identity_ticks = [None; 2];
    for (position, payload) in &shr.candidates {
        if let Some(identity_index) = transmitted.iter().position(|expected| expected == payload) {
            let event = events[identity_index];
            let first_tick = event.replay_start / 2 + 1;
            let last_tick = (event.end_exclusive - 1) / 2 + 1;
            assert!(
                (first_tick..=last_tick).contains(position),
                "SHR candidate identity {identity_index} at tick {position} is outside its detector event {event:?}"
            );
            identity_ticks[identity_index].get_or_insert(*position);
        }
    }
    let shr_counts = count_candidates(
        &shr.candidates
            .into_iter()
            .map(|(_, payload)| payload)
            .collect::<Vec<_>>(),
        transmitted,
    );
    eprintln!(
        "SHR rate candidate case={name} events={events:?} stats={:?} counts={shr_counts:?}",
        shr.stats
    );
    BurstTimingCandidateObservation {
        counts: [baseline[0], baseline[1], baseline[2], baseline[3], burst],
        shr: ShrRateObservation {
            counts: shr_counts,
            stats: shr.stats,
            identity_ticks,
            input_samples: samples.len() as u64,
        },
    }
}

fn run_joint_shr_rate_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> JointShrRateObservation {
    let events = detect_causal_bursts(samples);
    assert_eq!(events.len(), transmitted.len());
    assert!(events.iter().all(|event| !event.forced_end));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].end_exclusive <= pair[1].replay_start)
    );
    let observation = run_shr_rate_burst_streaming_case(name, channel, samples);
    let selection_count = observation
        .stats
        .shr_rate_selections
        .iter()
        .flatten()
        .count() as u64;
    assert_eq!(selection_count, observation.stats.burst.shr_locks);
    let mut selected_events = vec![false; events.len()];
    for selection in observation.stats.shr_rate_selections.iter().flatten() {
        assert!(selection.timing.start_offset < 64);
        assert!(selection.timing.mu_sixteenths < 16);
        assert!([-100, 0, 100].contains(&selection.rate_error_ppm));
        let selected_replay_start = selection
            .timing
            .source_sample_index
            .checked_sub(u64::from(selection.timing.start_offset))
            .expect("joint SHR selection start offset must not precede source sample zero");
        let matching_events = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.replay_start == selected_replay_start)
            .map(|(event_index, _)| event_index)
            .collect::<Vec<_>>();
        assert_eq!(
            matching_events.len(),
            1,
            "joint SHR selection {selection:?} must derive from exactly one detector event in {events:?}"
        );
        let event_index = matching_events[0];
        assert!(
            !selected_events[event_index],
            "detector event {event_index} produced more than one retained joint SHR selection"
        );
        selected_events[event_index] = true;
    }

    let mut identity_ticks = [None; 2];
    for (position, payload) in &observation.candidates {
        if let Some(identity_index) = transmitted.iter().position(|expected| expected == payload) {
            let event = events[identity_index];
            let first_tick = event.replay_start / 2 + 1;
            let last_tick = (event.end_exclusive - 1) / 2 + 1;
            assert!(
                (first_tick..=last_tick).contains(position),
                "joint SHR candidate identity {identity_index} at tick {position} is outside its detector event {event:?}"
            );
            identity_ticks[identity_index].get_or_insert(*position);
        }
    }
    let counts = count_candidates(
        &observation
            .candidates
            .into_iter()
            .map(|(_, payload)| payload)
            .collect::<Vec<_>>(),
        transmitted,
    );
    eprintln!(
        "joint SHR rate candidate case={name} events={events:?} stats={:?} counts={counts:?}",
        observation.stats
    );
    JointShrRateObservation {
        counts,
        stats: observation.stats,
        identity_ticks,
        input_samples: samples.len() as u64,
    }
}

fn run_deferred_shr_rate_candidate_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
    expected_events: &[DetectedBurst; 2],
) -> DeferredShrRateObservation {
    let events = detect_causal_bursts(samples);
    assert_eq!(events.as_slice(), expected_events);
    assert!(events.iter().all(|event| !event.forced_end));
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].end_exclusive <= pair[1].replay_start)
    );
    let observation = run_deferred_shr_rate_burst_streaming_case(name, channel, samples);
    assert_eq!(observation.candidates.len(), transmitted.len());
    assert!(
        observation
            .candidates
            .iter()
            .map(|(_, payload)| payload)
            .eq(transmitted.iter())
    );
    assert_eq!(
        observation
            .stats
            .deferred_selections
            .iter()
            .flatten()
            .count() as u64,
        observation.stats.burst.shr_locks
    );
    let mut selected_events = [false; 2];
    for handoff in observation.stats.deferred_selections.iter().flatten() {
        assert!(handoff.selection.timing.start_offset < 64);
        assert!(handoff.selection.timing.mu_sixteenths < 16);
        assert!(TIMING_RATE_ERROR_PPM.contains(&handoff.selection.rate_error_ppm));
        assert!((767..=832).contains(&usize::from(handoff.payload_left_offset)));
        let selected_replay_start = handoff
            .selection
            .timing
            .source_sample_index
            .checked_sub(u64::from(handoff.selection.timing.start_offset))
            .expect("deferred selection must not precede source sample zero");
        let matching_events = events
            .iter()
            .enumerate()
            .filter(|(_, event)| event.replay_start == selected_replay_start)
            .map(|(event_index, _)| event_index)
            .collect::<Vec<_>>();
        assert_eq!(matching_events.len(), 1);
        let event_index = matching_events[0];
        assert!(!selected_events[event_index]);
        selected_events[event_index] = true;
        assert_eq!(
            handoff.payload_source_sample_index,
            selected_replay_start + u64::from(handoff.payload_left_offset)
        );
        assert!(handoff.payload_source_sample_index < events[event_index].end_exclusive);
    }
    assert_eq!(selected_events, [true; 2]);

    let mut identity_ticks = [None; 2];
    let mut candidate_lengths = [0; 2];
    for (position, payload) in &observation.candidates {
        if let Some(identity_index) = transmitted.iter().position(|expected| expected == payload) {
            let event = events[identity_index];
            let first_tick = event.replay_start / 2 + 1;
            let last_tick = (event.end_exclusive - 1) / 2 + 1;
            assert!((first_tick..=last_tick).contains(position));
            identity_ticks[identity_index].get_or_insert(*position);
            candidate_lengths[identity_index] =
                u8::try_from(payload.len()).expect("bounded PSDU length must fit u8");
        }
    }
    let counts = count_candidates(
        &observation
            .candidates
            .into_iter()
            .map(|(_, payload)| payload)
            .collect::<Vec<_>>(),
        transmitted,
    );
    DeferredShrRateObservation {
        counts,
        stats: observation.stats,
        identity_ticks,
        candidate_lengths,
        input_samples: samples.len() as u64,
    }
}

type H3Observations = [[CandidateCounts; 2]; H3_HYPOTHESES_HZ.len()];

fn run_h3_case(
    label: &str,
    channel: u8,
    samples: &[Complex32],
    transmitted: &[Vec<u8>],
) -> H3Observations {
    std::array::from_fn(|index| {
        let hypothesis_hz = H3_HYPOTHESES_HZ[index];
        run_impairment_case_with_carrier_offset(
            &format!("{label}-hypothesis-{hypothesis_hz:+}-hz"),
            channel,
            samples,
            transmitted,
            hypothesis_hz,
        )
    })
}

fn assert_h3_two_frame_coverage(label: &str, observations: &H3Observations) {
    for (hypothesis_index, lanes) in observations.iter().enumerate() {
        for (lane_index, counts) in lanes.iter().enumerate() {
            assert!(
                counts.has_no_anomalies(),
                "{label}: hypothesis {} lane {lane_index} emitted anomalous candidates: {counts:?}",
                H3_HYPOTHESES_HZ[hypothesis_index],
            );
        }
    }
    for lane_index in 0..2 {
        assert!(
            observations
                .iter()
                .any(|lanes| lanes[lane_index] == CandidateCounts::BOTH_TRANSMITTED),
            "{label}: H3 did not recover both transmitted frames in lane {lane_index}: {observations:?}",
        );
    }
}

fn cfo_samples(
    short: &[Complex32],
    longer: &[Complex32],
    carrier_offset_hz: i32,
) -> Vec<Complex32> {
    const SAMPLE_RATE_HZ: f64 = 4_000_000.0;

    let mut samples = apply_carrier_offset(short, f64::from(carrier_offset_hz), SAMPLE_RATE_HZ);
    samples.extend(apply_carrier_offset(
        longer,
        f64::from(carrier_offset_hz),
        SAMPLE_RATE_HZ,
    ));
    samples
}

fn cfo_awgn_samples(
    short: &[Complex32],
    longer: &[Complex32],
    carrier_offset_hz: i32,
    seed: u64,
) -> Vec<Complex32> {
    const SAMPLE_RATE_HZ: f64 = 4_000_000.0;

    let short = apply_carrier_offset(short, f64::from(carrier_offset_hz), SAMPLE_RATE_HZ);
    let longer = apply_carrier_offset(longer, f64::from(carrier_offset_hz), SAMPLE_RATE_HZ);
    let mut samples = add_awgn(&short, H3_AWGN_SAMPLE_SNR_DB, seed);
    samples.extend(add_awgn(
        &longer,
        H3_AWGN_SAMPLE_SNR_DB,
        seed ^ 0xa5a5_5a5a_c3c3_3c3c,
    ));
    samples
}

fn graded_samples(
    short: &[Complex32],
    longer: &[Complex32],
    sample_snr_db: f64,
    seed: u64,
) -> Vec<Complex32> {
    let mut samples = add_awgn(short, sample_snr_db, seed);
    samples.extend(add_awgn(
        longer,
        sample_snr_db,
        seed ^ 0xa5a5_5a5a_c3c3_3c3c,
    ));
    samples
}

fn fractional_timing_samples(
    short: &[Complex32],
    longer: &[Complex32],
    offset_numerator: i32,
) -> Vec<Complex32> {
    let mut samples = apply_fractional_timing(short, offset_numerator);
    samples.extend(apply_fractional_timing(longer, offset_numerator));
    samples
}

fn fractional_timing_awgn_samples(
    short: &[Complex32],
    longer: &[Complex32],
    offset_numerator: i32,
    seed: u64,
) -> Vec<Complex32> {
    let short = apply_fractional_timing(short, offset_numerator);
    let longer = apply_fractional_timing(longer, offset_numerator);
    let mut samples = add_awgn(&short, TIMING_AWGN_SAMPLE_SNR_DB, seed);
    samples.extend(add_awgn(
        &longer,
        TIMING_AWGN_SAMPLE_SNR_DB,
        seed ^ 0xa5a5_5a5a_c3c3_3c3c,
    ));
    samples
}

fn fractional_timing_rate_awgn_samples(
    short: &[Complex32],
    longer: &[Complex32],
    offset_numerator: i32,
    rate_error_ppm: i32,
    seed: u64,
) -> Vec<Complex32> {
    let [mut short, longer] =
        fractional_timing_rate_awgn_bursts(short, longer, offset_numerator, rate_error_ppm, seed);
    short.extend(longer);
    short
}

fn fractional_timing_rate_awgn_bursts(
    short: &[Complex32],
    longer: &[Complex32],
    offset_numerator: i32,
    rate_error_ppm: i32,
    seed: u64,
) -> [Vec<Complex32>; 2] {
    let short = apply_sample_rate_error(short, rate_error_ppm);
    let longer = apply_sample_rate_error(longer, rate_error_ppm);
    let short = apply_fractional_timing(&short, offset_numerator);
    let longer = apply_fractional_timing(&longer, offset_numerator);
    [
        add_awgn(&short, TIMING_AWGN_SAMPLE_SNR_DB, seed),
        add_awgn(
            &longer,
            TIMING_AWGN_SAMPLE_SNR_DB,
            seed ^ 0xa5a5_5a5a_c3c3_3c3c,
        ),
    ]
}

struct BurstDiagnosticInputs {
    combined: Vec<Complex32>,
    full_long: Vec<Complex32>,
    cropped_long: Vec<Complex32>,
    cropped_short: Vec<Complex32>,
    short_active_bounds: (usize, usize),
    long_active_bounds: (usize, usize),
}

impl BurstDiagnosticInputs {
    fn samples(&self) -> [&[Complex32]; 4] {
        [
            &self.combined,
            &self.full_long,
            &self.cropped_long,
            &self.cropped_short,
        ]
    }
}

fn active_sample_bounds(samples: &[Complex32]) -> (usize, usize) {
    let first = samples
        .iter()
        .position(|sample| sample.norm_sqr() != 0.0)
        .expect("transformed waveform has an active sample");
    let last = samples
        .iter()
        .rposition(|sample| sample.norm_sqr() != 0.0)
        .expect("transformed waveform has an active sample");
    assert!(first <= last);
    (first, last)
}

fn crop_active_with_flush(
    noisy: &[Complex32],
    noiseless: &[Complex32],
    active_bounds: (usize, usize),
) -> Vec<Complex32> {
    assert_eq!(noisy.len(), noiseless.len());
    let (first_active, last_active) = active_bounds;
    let end = last_active
        .checked_add(1 + TIMING_BURST_DIAGNOSTIC_FLUSH_SAMPLES)
        .expect("diagnostic crop end fits usize");
    assert!(end <= noiseless.len());
    assert!(
        noiseless[last_active + 1..end]
            .iter()
            .all(|sample| sample.norm_sqr() == 0.0),
        "diagnostic flush must be noiseless idle samples"
    );
    noisy[first_active..end].to_vec()
}

fn timing_burst_diagnostic_inputs(
    short: &[Complex32],
    longer: &[Complex32],
) -> BurstDiagnosticInputs {
    let seed = TIMING_DEVELOPMENT_SEEDS[TIMING_BURST_DIAGNOSTIC_SEED_INDEX];
    let [noisy_short, noisy_long] = fractional_timing_rate_awgn_bursts(
        short,
        longer,
        TIMING_BURST_DIAGNOSTIC_OFFSET,
        TIMING_BURST_DIAGNOSTIC_RATE_ERROR_PPM,
        seed,
    );
    let mut combined = noisy_short.clone();
    combined.extend_from_slice(&noisy_long);
    assert_eq!(
        combined,
        fractional_timing_rate_awgn_samples(
            short,
            longer,
            TIMING_BURST_DIAGNOSTIC_OFFSET,
            TIMING_BURST_DIAGNOSTIC_RATE_ERROR_PPM,
            seed,
        ),
        "separated diagnostic bursts changed the frozen combined fixture"
    );
    assert_eq!(&combined[..noisy_short.len()], noisy_short);
    assert_eq!(&combined[noisy_short.len()..], noisy_long);

    let noiseless_short = apply_fractional_timing(
        &apply_sample_rate_error(short, TIMING_BURST_DIAGNOSTIC_RATE_ERROR_PPM),
        TIMING_BURST_DIAGNOSTIC_OFFSET,
    );
    let noiseless_long = apply_fractional_timing(
        &apply_sample_rate_error(longer, TIMING_BURST_DIAGNOSTIC_RATE_ERROR_PPM),
        TIMING_BURST_DIAGNOSTIC_OFFSET,
    );
    let short_active_bounds = active_sample_bounds(&noiseless_short);
    let long_active_bounds = active_sample_bounds(&noiseless_long);
    let cropped_short = crop_active_with_flush(&noisy_short, &noiseless_short, short_active_bounds);
    let cropped_long = crop_active_with_flush(&noisy_long, &noiseless_long, long_active_bounds);

    BurstDiagnosticInputs {
        combined,
        full_long: noisy_long,
        cropped_long,
        cropped_short,
        short_active_bounds,
        long_active_bounds,
    }
}

fn sha256_cf32_le(samples: &[Complex32]) -> String {
    let mut bytes = Vec::with_capacity(samples.len() * 8);
    append_cf32_le(&mut bytes, samples);
    sha256_hex(&bytes)
}

fn detect_causal_bursts(samples: &[Complex32]) -> Vec<DetectedBurst> {
    let mut detector = CausalBurstDetector::new();
    let mut events = Vec::new();
    for sample in samples {
        if let Some(event) = detector
            .push(*sample)
            .expect("frozen detector input is finite")
        {
            events.push(event);
        }
    }
    if let Some(event) = detector.finish() {
        events.push(event);
    }
    events
}

#[derive(Debug)]
struct BurstDecodeObservation {
    event: DetectedBurst,
    candidates: Vec<Vec<u8>>,
}

fn run_causal_burst_soft_decode(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    events: &[DetectedBurst],
) -> Vec<BurstDecodeObservation> {
    let directory = tempfile::tempdir().expect("create causal burst decode directory");
    events
        .iter()
        .enumerate()
        .map(|(event_index, event)| {
            let start =
                usize::try_from(event.replay_start).expect("causal burst replay start fits usize");
            let detected_at =
                usize::try_from(event.detected_at).expect("causal burst detection fits usize");
            let end = usize::try_from(event.end_exclusive)
                .expect("causal burst exclusive end fits usize");
            assert!(start < detected_at);
            assert!(detected_at < end);
            assert!(end <= samples.len());

            let input = directory
                .path()
                .join(format!("{name}-{event_index}-{start}-{end}.cf32"));
            let mut bytes = Vec::with_capacity((end - start) * 8);
            append_cf32_le(&mut bytes, &samples[start..end]);
            fs::write(&input, &bytes).expect("write causal burst span");
            let output = directory
                .path()
                .join(format!("{name}-{event_index}-{start}-{end}-soft.i154raw"));
            let input_mib = (bytes.len() as u64).div_ceil(1024 * 1024) as u32;
            run(Args {
                input,
                output: output.clone(),
                source_index_output: None,
                channel,
                max_input_mib: input_mib.max(1),
                max_frames: 1024,
                max_output_mib: 1,
                decoder_lane: DecoderLane::Soft,
                carrier_offset_hz: 0,
                executor_threads: None,
            })
            .expect("run fresh soft worker on causal burst span");
            BurstDecodeObservation {
                event: *event,
                candidates: archive_candidates(&output, channel),
            }
        })
        .collect()
}

#[derive(Debug)]
struct BurstStreamingObservation<S = BurstClockStats> {
    stats: S,
    candidates: Vec<(u64, Vec<u8>)>,
}

fn run_causal_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
) -> BurstStreamingObservation {
    run_burst_streaming_case(
        name,
        channel,
        samples,
        run_with_burst_soft,
        "run causal burst streaming candidate",
    )
}

fn run_shr_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
) -> BurstStreamingObservation {
    run_burst_streaming_case(
        name,
        channel,
        samples,
        run_with_burst_shr_soft,
        "run SHR-initialized burst streaming candidate",
    )
}

fn run_shr_rate_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
) -> BurstStreamingObservation<ShrRateClockStats> {
    run_burst_streaming_case(
        name,
        channel,
        samples,
        run_with_burst_shr_rate_soft,
        "run joint SHR-initialized burst streaming candidate",
    )
}

fn run_deferred_shr_rate_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
) -> BurstStreamingObservation<DeferredShrRateClockStats> {
    run_burst_streaming_case(
        name,
        channel,
        samples,
        run_with_burst_deferred_shr_rate_soft,
        "run deferred SHR/PHR burst streaming candidate",
    )
}

fn run_shr_rate_trace_burst_streaming_case(
    name: &str,
    channel: u8,
    samples: &[Complex32],
) -> BurstStreamingObservation<super::ShrRateDiagnosticRun> {
    run_burst_streaming_case(
        name,
        channel,
        samples,
        run_with_burst_shr_rate_trace_soft,
        "run traced joint SHR-initialized burst streaming candidate",
    )
}

fn run_burst_streaming_case<S>(
    name: &str,
    channel: u8,
    samples: &[Complex32],
    runner: fn(Args) -> anyhow::Result<S>,
    failure: &str,
) -> BurstStreamingObservation<S> {
    let directory = tempfile::tempdir().expect("create causal burst streaming directory");
    let input = directory.path().join(format!("{name}.cf32"));
    let mut bytes = Vec::with_capacity(samples.len() * 8);
    append_cf32_le(&mut bytes, samples);
    fs::write(&input, &bytes).expect("write causal burst streaming input");
    let output = directory.path().join(format!("{name}.i154raw"));
    let input_mib = (bytes.len() as u64).div_ceil(1024 * 1024) as u32;
    let stats = runner(Args {
        input,
        output: output.clone(),
        source_index_output: None,
        channel,
        max_input_mib: input_mib.max(1),
        max_frames: 1024,
        max_output_mib: 1,
        decoder_lane: DecoderLane::Soft,
        carrier_offset_hz: 0,
        executor_threads: None,
    })
    .expect(failure);
    BurstStreamingObservation {
        stats,
        candidates: archive_positioned_candidates(&output, channel),
    }
}

fn run_graded_corpus(
    label: &str,
    seeds: &[u64],
    short: &[Complex32],
    longer: &[Complex32],
    transmitted: &[Vec<u8>],
) -> Vec<[CandidateCounts; 2]> {
    let mut observations = Vec::new();
    for (seed_index, seed) in seeds.iter().copied().enumerate() {
        for sample_snr_db in GRADED_SAMPLE_SNR_DB {
            let case = format!("{label}-seed-{seed_index}-snr-{sample_snr_db:.0}");
            let samples = graded_samples(short, longer, sample_snr_db, seed);
            observations.push(run_impairment_case(&case, 17, &samples, transmitted));
        }
    }
    observations
}

fn assert_graded_awgn_corpus(label: &str, seeds: &[u64], expected: &[[CandidateCounts; 2]]) {
    assert_eq!(ieee802154_fcs(&ACK_PSDU[..3]), 0xd5a6);
    assert!(has_valid_fcs(&ACK_PSDU));
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let mut clean = short_waveform.clone();
    clean.extend_from_slice(&longer_waveform);

    let suite_started = Instant::now();
    assert_eq!(
        run_impairment_case(&format!("{label}-clean"), 17, &clean, &transmitted),
        [CandidateCounts::BOTH_TRANSMITTED; 2]
    );

    let observations = run_graded_corpus(
        label,
        seeds,
        &short_waveform,
        &longer_waveform,
        &transmitted,
    );

    let reference_power = active_waveform_power(&short_waveform);
    for (seed_index, seed) in seeds.iter().copied().enumerate() {
        let samples = noise_only(clean.len(), reference_power, 0.0, seed);
        assert_eq!(
            run_impairment_case(
                &format!("{label}-noise-only-{seed_index}"),
                17,
                &samples,
                &[],
            ),
            [CandidateCounts::NONE; 2]
        );
    }

    assert_eq!(observations.as_slice(), expected);
    eprintln!(
        "impairment corpus={label} rows={} elapsed_ms={}",
        observations.len(),
        suite_started.elapsed().as_millis(),
    );
}

#[test]
fn fractional_timing_contracts_are_bounded_and_disjoint() {
    assert_eq!(FRACTIONAL_DELAY_TAPS, 33);
    assert_eq!(FRACTIONAL_DELAY_CENTER, 16);
    assert!(
        TIMING_DEVELOPMENT_OFFSETS
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
    assert!(
        TIMING_HOLDOUT_OFFSETS
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
    assert!(
        TIMING_DEVELOPMENT_OFFSETS
            .iter()
            .chain(&TIMING_HOLDOUT_OFFSETS)
            .all(|offset| (-8..=8).contains(offset))
    );
    assert!(TIMING_DEVELOPMENT_OFFSETS.contains(&0));
    assert!(!TIMING_HOLDOUT_OFFSETS.contains(&0));
    assert!(
        TIMING_DEVELOPMENT_OFFSETS
            .iter()
            .all(|development| { !TIMING_HOLDOUT_OFFSETS.contains(development) })
    );
    assert!(
        TIMING_DEVELOPMENT_SEEDS
            .iter()
            .all(|development| { !TIMING_HOLDOUT_SEEDS.contains(development) })
    );
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert!(
        TIMING_RATE_ERROR_PPM
            .windows(2)
            .all(|pair| pair[0] < pair[1])
    );
    assert_eq!(TIMING_RATE_ERROR_PPM[0], -TIMING_RATE_ERROR_PPM[2]);
    assert_eq!(TIMING_RATE_DEVELOPMENT_OFFSETS, [-4, 0, 8]);
    assert!(
        TIMING_RATE_DEVELOPMENT_OFFSETS
            .iter()
            .all(|offset| TIMING_DEVELOPMENT_OFFSETS.contains(offset))
    );
    assert!(
        TIMING_RATE_DEVELOPMENT_OFFSETS
            .iter()
            .all(|offset| !TIMING_HOLDOUT_OFFSETS.contains(offset))
    );
    for (index, (offset, rate_error_ppm)) in TIMING_RATE_SCREEN_CASES.iter().enumerate() {
        assert!(TIMING_RATE_DEVELOPMENT_OFFSETS.contains(offset));
        assert!(matches!(rate_error_ppm, -100 | 100));
        assert!(!TIMING_RATE_SCREEN_CASES[..index].contains(&(*offset, *rate_error_ppm)));
    }
    assert_eq!(TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS, 8);
    assert_eq!(
        f32::from(TIMING_ADAPTIVE_CLOCK_MU_SIXTEENTHS) / TIMING_OFFSET_DENOMINATOR as f32,
        super::ESTABLISHED_CLOCK_MU
    );
    assert_eq!(TIMING_FIXED_CLOCK_MU_SIXTEENTHS, [4, 12]);
    assert!(
        TIMING_FIXED_CLOCK_MU_SIXTEENTHS
            .iter()
            .all(|mu| *mu > 0 && *mu < TIMING_OFFSET_DENOMINATOR as u8)
    );
    for (index, mu) in TIMING_FIXED_CLOCK_MU_SIXTEENTHS.iter().enumerate() {
        assert!(!TIMING_FIXED_CLOCK_MU_SIXTEENTHS[..index].contains(mu));
    }
    for offset in TIMING_DEVELOPMENT_OFFSETS
        .iter()
        .chain(&TIMING_HOLDOUT_OFFSETS)
    {
        let taps = fractional_delay_taps(*offset);
        assert!((taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(taps.iter().all(|tap| tap.is_finite()));
    }
}

#[test]
fn full_cf32_fractional_timing_6db_development_inputs_are_frozen() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observed: [[String; 9]; 2] = std::array::from_fn(|seed_index| {
        let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
        TIMING_DEVELOPMENT_OFFSETS.map(|offset| {
            let samples =
                fractional_timing_awgn_samples(&short_waveform, &longer_waveform, offset, seed);
            let mut bytes = Vec::with_capacity(samples.len() * 8);
            append_cf32_le(&mut bytes, &samples);
            sha256_hex(&bytes)
        })
    });
    eprintln!("fractional timing 6 dB development input SHA-256 rows={observed:?}");
    for (observed_row, expected_row) in observed.iter().zip(EXPECTED_TIMING_6DB_DEVELOPMENT_SHA256)
    {
        assert_eq!(observed_row.each_ref().map(String::as_str), expected_row);
    }
}

#[test]
fn full_cf32_fractional_timing_rate_6db_development_inputs_are_frozen() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observed: [[[String; 3]; 3]; 2] = std::array::from_fn(|seed_index| {
        let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
        TIMING_RATE_DEVELOPMENT_OFFSETS.map(|offset| {
            TIMING_RATE_ERROR_PPM.map(|rate_error_ppm| {
                let samples = fractional_timing_rate_awgn_samples(
                    &short_waveform,
                    &longer_waveform,
                    offset,
                    rate_error_ppm,
                    seed,
                );
                let mut bytes = Vec::with_capacity(samples.len() * 8);
                append_cf32_le(&mut bytes, &samples);
                sha256_hex(&bytes)
            })
        })
    });
    eprintln!("fractional timing rate 6 dB development input SHA-256 rows={observed:?}");
    for seed_index in 0..TIMING_DEVELOPMENT_SEEDS.len() {
        for (offset_index, offset) in TIMING_RATE_DEVELOPMENT_OFFSETS.iter().enumerate() {
            assert_eq!(
                observed[seed_index][offset_index]
                    .each_ref()
                    .map(String::as_str),
                EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256[seed_index][offset_index]
            );
            let baseline_offset_index = TIMING_DEVELOPMENT_OFFSETS
                .iter()
                .position(|candidate| candidate == offset)
                .expect("rate phase is a development phase");
            assert_eq!(
                observed[seed_index][offset_index][1],
                EXPECTED_TIMING_6DB_DEVELOPMENT_SHA256[seed_index][baseline_offset_index],
                "zero-rate input drifted at seed {seed_index}, offset {offset}"
            );
        }
    }
}

#[test]
fn fractional_timing_operator_preserves_identity_energy_and_symmetry() {
    let mut impulse = vec![Complex32::new(0.0, 0.0); 129];
    impulse[64] = Complex32::new(1.0, -0.5);
    assert_eq!(apply_fractional_timing(&impulse, 0), impulse);

    let positive = apply_fractional_timing(&impulse, 4);
    let negative = apply_fractional_timing(&impulse, -4);
    assert_eq!(positive.len(), impulse.len());
    assert_eq!(negative.len(), impulse.len());
    assert!(
        positive
            .iter()
            .chain(&negative)
            .all(|sample| sample.re.is_finite() && sample.im.is_finite())
    );
    let reference_energy = total_waveform_energy(&impulse);
    assert!((total_waveform_energy(&positive) / reference_energy - 1.0).abs() < 1e-6);
    assert!((total_waveform_energy(&negative) / reference_energy - 1.0).abs() < 1e-6);
    for displacement in -16isize..=16 {
        let positive_index = usize::try_from(64isize + displacement).unwrap();
        let negative_index = usize::try_from(64isize - displacement).unwrap();
        assert!((positive[positive_index].re - negative[negative_index].re).abs() < 1e-6);
        assert!((positive[positive_index].im - negative[negative_index].im).abs() < 1e-6);
    }
    let centroid = |samples: &[Complex32]| {
        let energy = total_waveform_energy(samples);
        samples
            .iter()
            .enumerate()
            .map(|(index, sample)| index as f64 * f64::from(sample.norm_sqr()))
            .sum::<f64>()
            / energy
    };
    assert!(centroid(&positive) > 64.0);
    assert!(centroid(&negative) < 64.0);

    let constant = vec![Complex32::new(0.5, -0.25); 4096];
    let delayed_constant = apply_fractional_timing(&constant, 4);
    for sample in &delayed_constant[32..delayed_constant.len() - 32] {
        assert!((sample.re - 0.5).abs() < 0.01);
        assert!((sample.im + 0.25).abs() < 0.01);
    }
}

#[test]
fn full_cf32_timing_burst_diagnostic_inputs_are_frozen() {
    let longer_psdu = longer_valid_psdu();
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let lengths = inputs.samples().map(<[Complex32]>::len);
    let sha256 = inputs.samples().map(sha256_cf32_le);
    eprintln!(
        "timing burst diagnostic short_bounds={:?} long_bounds={:?} lengths={lengths:?} sha256={sha256:?}",
        inputs.short_active_bounds, inputs.long_active_bounds
    );
    assert_eq!(
        [inputs.short_active_bounds, inputs.long_active_bounds],
        EXPECTED_TIMING_BURST_DIAGNOSTIC_ACTIVE_BOUNDS
    );
    assert_eq!(lengths, EXPECTED_TIMING_BURST_DIAGNOSTIC_LENGTHS);
    assert_eq!(
        sha256.each_ref().map(String::as_str),
        EXPECTED_TIMING_BURST_DIAGNOSTIC_SHA256
    );
    assert_eq!(
        EXPECTED_TIMING_BURST_DIAGNOSTIC_SHA256[0],
        EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256[TIMING_BURST_DIAGNOSTIC_SEED_INDEX][1][0]
    );
}

#[test]
fn full_cf32_causal_burst_detector_events_are_frozen() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let inputs = timing_burst_diagnostic_inputs(&short_waveform, &longer_waveform);
    let combined_events = detect_causal_bursts(&inputs.combined);
    let full_long_events = detect_causal_bursts(&inputs.full_long);
    let scaled_events = [0.25f32, 1.0, 4.0].map(|scale| {
        detect_causal_bursts(
            &inputs
                .combined
                .iter()
                .map(|sample| *sample * scale)
                .collect::<Vec<_>>(),
        )
    });
    let reference_power = active_waveform_power(&short_waveform);
    let noise_events: [Vec<DetectedBurst>; 2] = TIMING_DEVELOPMENT_SEEDS.map(|seed| {
        detect_causal_bursts(&noise_only(
            inputs.combined.len(),
            reference_power,
            0.0,
            seed,
        ))
    });
    eprintln!(
        "causal burst detector combined={combined_events:?} full_long={full_long_events:?} scaled={scaled_events:?} noise={noise_events:?}"
    );

    assert_eq!(combined_events.len(), 2);
    assert_eq!(full_long_events.len(), 1);
    assert!(noise_events.iter().all(Vec::is_empty));
    assert!(
        scaled_events
            .iter()
            .all(|events| events == &scaled_events[1])
    );
    let short_bounds = inputs.short_active_bounds;
    let long_absolute_bounds = (
        inputs.combined.len() - inputs.full_long.len() + inputs.long_active_bounds.0,
        inputs.combined.len() - inputs.full_long.len() + inputs.long_active_bounds.1,
    );
    for (event, bounds) in combined_events
        .iter()
        .zip([short_bounds, long_absolute_bounds])
    {
        assert!(event.replay_start <= bounds.0 as u64);
        assert!(event.end_exclusive > bounds.1 as u64);
        assert!(!event.forced_end);
    }
    assert!(combined_events[0].end_exclusive <= long_absolute_bounds.0 as u64);
    assert!(full_long_events[0].replay_start <= inputs.long_active_bounds.0 as u64);
    assert!(full_long_events[0].end_exclusive > inputs.long_active_bounds.1 as u64);
    assert!(!full_long_events[0].forced_end);
    assert_eq!(combined_events, EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED);
    assert_eq!(full_long_events, EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG);
}

#[test]
fn full_cf32_causal_burst_soft_decode_recovers_detected_frames() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let combined_events = detect_causal_bursts(&inputs.combined);
    let full_long_events = detect_causal_bursts(&inputs.full_long);
    assert_eq!(combined_events, EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED);
    assert_eq!(full_long_events, EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG);

    let combined = run_causal_burst_soft_decode(
        "causal-burst-combined",
        17,
        &inputs.combined,
        &combined_events,
    );
    let full_long = run_causal_burst_soft_decode(
        "causal-burst-full-long",
        17,
        &inputs.full_long,
        &full_long_events,
    );
    assert_eq!(
        combined
            .iter()
            .map(|observation| observation.event)
            .collect::<Vec<_>>(),
        combined_events
    );
    assert_eq!(
        full_long
            .iter()
            .map(|observation| observation.event)
            .collect::<Vec<_>>(),
        full_long_events
    );

    let combined_per_burst = combined
        .iter()
        .map(|observation| count_candidates(&observation.candidates, &transmitted))
        .collect::<Vec<_>>();
    let combined_candidates = combined
        .iter()
        .flat_map(|observation| observation.candidates.iter().cloned())
        .collect::<Vec<_>>();
    let combined_union = count_candidates(&combined_candidates, &transmitted);
    let full_long_per_burst = full_long
        .iter()
        .map(|observation| count_candidates(&observation.candidates, &transmitted))
        .collect::<Vec<_>>();
    eprintln!(
        "causal burst soft decode combined_events={combined_events:?} combined_per_burst={combined_per_burst:?} combined_union={combined_union:?} full_long_events={full_long_events:?} full_long_per_burst={full_long_per_burst:?}"
    );

    let long_source_offset = inputs.combined.len() - inputs.full_long.len();
    let combined_long = combined_events[1];
    let full_long_event = full_long_events[0];
    assert_eq!(
        combined_long.replay_start,
        full_long_event.replay_start + long_source_offset as u64
    );
    assert_eq!(
        combined_long.end_exclusive,
        full_long_event.end_exclusive + long_source_offset as u64
    );
    assert_eq!(
        &inputs.combined[combined_long.replay_start as usize..combined_long.end_exclusive as usize],
        &inputs.full_long
            [full_long_event.replay_start as usize..full_long_event.end_exclusive as usize]
    );
    assert_eq!(
        combined_per_burst,
        [CandidateCounts::SHORT_ONLY, CandidateCounts::LONG_ONLY]
    );
    assert_eq!(combined_union, CandidateCounts::BOTH_TRANSMITTED);
    assert_eq!(full_long_per_burst, [CandidateCounts::LONG_ONLY]);
}

#[test]
fn full_cf32_causal_burst_streaming_soft_matches_span_oracle() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let combined =
        run_causal_burst_streaming_case("causal-burst-streaming-combined", 17, &inputs.combined);
    let full_long =
        run_causal_burst_streaming_case("causal-burst-streaming-full-long", 17, &inputs.full_long);
    let combined_payloads = combined
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let full_long_payloads = full_long
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let combined_counts = count_candidates(&combined_payloads, &transmitted);
    let full_long_counts = count_candidates(&full_long_payloads, &transmitted);
    let combined_ticks = combined
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    let full_long_ticks = full_long
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    eprintln!(
        "causal burst streaming combined_stats={:?} combined_ticks={combined_ticks:?} combined_counts={combined_counts:?} full_long_stats={:?} full_long_ticks={full_long_ticks:?} full_long_counts={full_long_counts:?}",
        combined.stats, full_long.stats,
    );

    assert_eq!(
        combined.stats,
        BurstClockStats {
            source_samples: inputs.combined.len() as u64,
            starts: 2,
            ends: 2,
            markers: 2,
            max_pretrigger: 64,
            max_replay: 64,
            max_metrics: 4,
            shr_locks: 0,
            shr_rejections: 0,
            shr_selections: [None; 2],
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 0,
            max_shr_metric_replay: 0,
        }
    );
    assert_eq!(
        full_long.stats,
        BurstClockStats {
            source_samples: inputs.full_long.len() as u64,
            starts: 1,
            ends: 1,
            markers: 1,
            max_pretrigger: 64,
            max_replay: 64,
            max_metrics: 4,
            shr_locks: 0,
            shr_rejections: 0,
            shr_selections: [None; 2],
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 0,
            max_shr_metric_replay: 0,
        }
    );
    for ((position, _), event) in combined
        .candidates
        .iter()
        .zip(EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED)
    {
        let first_tick = event.replay_start / 2 + 1;
        let last_tick = (event.end_exclusive - 1) / 2 + 1;
        assert!((first_tick..=last_tick).contains(position));
    }
    for ((position, _), event) in full_long
        .candidates
        .iter()
        .zip(EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG)
    {
        let first_tick = event.replay_start / 2 + 1;
        let last_tick = (event.end_exclusive - 1) / 2 + 1;
        assert!((first_tick..=last_tick).contains(position));
    }
    assert_eq!(
        combined_ticks,
        EXPECTED_CAUSAL_BURST_STREAMING_COMBINED_TICKS
    );
    assert_eq!(
        full_long_ticks,
        EXPECTED_CAUSAL_BURST_STREAMING_FULL_LONG_TICKS
    );
    assert_eq!(
        combined_ticks[1],
        full_long_ticks[0] + (inputs.combined.len() - inputs.full_long.len()) as u64 / 2
    );
    assert_eq!(combined_payloads, transmitted);
    assert_eq!(full_long_payloads, vec![longer_psdu]);
    assert_eq!(combined_counts, CandidateCounts::BOTH_TRANSMITTED);
    assert_eq!(full_long_counts, CandidateCounts::LONG_ONLY);
}

#[test]
fn full_cf32_clean_joint_trace_runner_is_observational() {
    let samples = upstream_waveform(&ACK_PSDU);
    let events = detect_causal_bursts(&samples);
    assert_eq!(events.len(), 1);
    let baseline = run_shr_rate_burst_streaming_case("clean-joint-trace-baseline", 17, &samples);
    let traced =
        run_shr_rate_trace_burst_streaming_case("clean-joint-trace-observed", 17, &samples);

    assert_eq!(traced.candidates, baseline.candidates);
    assert_eq!(traced.stats.stats, baseline.stats);
    assert_eq!(
        traced
            .candidates
            .iter()
            .map(|(_, payload)| payload.as_slice())
            .collect::<Vec<_>>(),
        [ACK_PSDU.as_slice()]
    );
    assert_eq!(traced.stats.burst_trace.overflow, 0);
    let burst = traced.stats.burst_trace.events[0]
        .as_ref()
        .expect("clean waveform must retain one burst trace");
    assert_eq!(burst.event, events[0]);
    assert_eq!(traced.stats.burst_trace.events[1], None);
    let selection = traced.stats.stats.shr_rate_selections[0]
        .expect("clean waveform must retain one joint selection");
    assert_eq!(burst.outcome, ShrRateTraceOutcome::Locked(selection));
    let clock = burst
        .clock
        .as_ref()
        .expect("locked clean waveform must retain one clock trace");
    assert_eq!(clock.advance_overflow, 0);
    assert_eq!(clock.boundary_overflow, 0);

    assert_eq!(traced.stats.decoder_trace.overflow, 0);
    let decoder =
        traced.stats.decoder_trace.events[0].expect("clean waveform must retain one decoder trace");
    assert_eq!(traced.stats.decoder_trace.events[1], None);
    assert_eq!(
        decoder.first_source_sample_index,
        selection.timing.source_sample_index
    );
    assert_eq!(decoder.recovered_chips, clock.emitted_chips);
    assert_eq!(decoder.completed_candidates, 1);
    assert!(decoder.last_completion.is_some());
}

#[test]
fn full_cf32_clean_deferred_shr_rate_handoff_recovers_both_frames() {
    // The selected +100 ppm grid point is a deterministic diagnostic of this
    // generated waveform and selector, not an oscillator-accuracy calibration.
    const EXPECTED_HANDOFFS: [Option<DeferredShrRateHandoff>; 2] = [
        Some(DeferredShrRateHandoff {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[0],
            phr_length: 5,
            payload_left_offset: 829,
            payload_source_sample_index: 40_770,
            payload_mu_bits: 0x3c6a_4a8c,
            prior_metric_bits: 0xbf4b_a894,
        }),
        Some(DeferredShrRateHandoff {
            selection: CLEAN_ZERO_PHASE_DEFERRED_SELECTIONS[1],
            phr_length: 127,
            payload_left_offset: 829,
            payload_source_sample_index: 122_180,
            payload_mu_bits: 0x3c6a_4a8c,
            prior_metric_bits: 0xbf4b_0717,
        }),
    ];
    let longer_psdu = longer_valid_psdu();
    let transmitted = [ACK_PSDU.to_vec(), longer_psdu.clone()];
    let mut samples = upstream_waveform(&ACK_PSDU);
    samples.extend(upstream_waveform(&longer_psdu));
    let events = detect_causal_bursts(&samples);
    assert_eq!(events, CLEAN_ZERO_PHASE_EVENTS);

    let observation =
        run_deferred_shr_rate_burst_streaming_case("clean-deferred-shr-rate-handoff", 17, &samples);
    assert_eq!(
        observation
            .candidates
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect::<Vec<_>>(),
        transmitted
    );
    let stats = observation.stats;
    eprintln!(
        "clean deferred SHR/PHR events={events:?} candidates={:?} stats={stats:?}",
        observation.candidates
    );
    assert_eq!(stats.burst.source_samples, samples.len() as u64);
    assert_eq!(stats.burst.starts, 2);
    assert_eq!(stats.burst.ends, 2);
    assert_eq!(stats.burst.markers, 2);
    assert_eq!(stats.burst.shr_locks, 2);
    assert_eq!(stats.burst.shr_rejections, 0);
    assert_eq!(stats.burst.max_pretrigger, 64);
    assert_eq!(stats.burst.max_replay, 64);
    assert_eq!(stats.burst.max_metrics, 4);
    assert_eq!(stats.burst.max_shr_selection_metrics, 832);
    assert!(stats.burst.max_shr_metric_replay <= 65);
    assert_eq!(stats.burst.shr_selections, [None; 2]);
    assert_eq!(stats.burst.shr_selection_overflow, 0);
    assert_eq!(stats.burst.max_shr_metric_replay, 3);
    assert_eq!(stats.deferred_selections, EXPECTED_HANDOFFS);
    assert_eq!(
        observation
            .candidates
            .iter()
            .map(|(tick, _)| *tick)
            .collect::<Vec<_>>(),
        [20_704, 69_217]
    );

    let selections = stats
        .deferred_selections
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(selections.len(), 2);
    assert_eq!(
        selections
            .iter()
            .map(|selection| selection.phr_length)
            .collect::<Vec<_>>(),
        [5, 127]
    );
    for (event_index, selection) in selections.iter().enumerate() {
        let replay_start = selection
            .selection
            .timing
            .source_sample_index
            .checked_sub(u64::from(selection.selection.timing.start_offset))
            .expect("deferred selection must not precede its detector event");
        assert_eq!(replay_start, events[event_index].replay_start);
        assert!(selection.payload_source_sample_index < events[event_index].end_exclusive);
        let first_tick = events[event_index].replay_start / 2 + 1;
        let last_tick = (events[event_index].end_exclusive - 1) / 2 + 1;
        assert!((first_tick..=last_tick).contains(&observation.candidates[event_index].0));
    }
}

#[test]
fn full_cf32_shr_timing_initializer_matches_opened_span_oracle() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let combined_events = detect_causal_bursts(&inputs.combined);
    let full_long_events = detect_causal_bursts(&inputs.full_long);
    assert_eq!(combined_events, EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED);
    assert_eq!(full_long_events, EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG);

    let combined =
        run_shr_burst_streaming_case("shr-timing-initializer-combined", 17, &inputs.combined);
    let full_long =
        run_shr_burst_streaming_case("shr-timing-initializer-full-long", 17, &inputs.full_long);
    let combined_payloads = combined
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let full_long_payloads = full_long
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let combined_counts = count_candidates(&combined_payloads, &transmitted);
    let full_long_counts = count_candidates(&full_long_payloads, &transmitted);
    let combined_ticks = combined
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    let full_long_ticks = full_long
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    eprintln!(
        "SHR timing initializer combined_stats={:?} combined_ticks={combined_ticks:?} combined_counts={combined_counts:?} full_long_stats={:?} full_long_ticks={full_long_ticks:?} full_long_counts={full_long_counts:?}",
        combined.stats, full_long.stats,
    );

    assert_eq!(
        combined.stats,
        BurstClockStats {
            source_samples: inputs.combined.len() as u64,
            starts: 2,
            ends: 2,
            markers: 2,
            max_pretrigger: 64,
            max_replay: 64,
            max_metrics: 4,
            shr_locks: 2,
            shr_rejections: 0,
            shr_selections: EXPECTED_SHR_TIMING_COMBINED_SELECTIONS.map(Some),
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 703,
            max_shr_metric_replay: 661,
        }
    );
    assert_eq!(
        full_long.stats,
        BurstClockStats {
            source_samples: inputs.full_long.len() as u64,
            starts: 1,
            ends: 1,
            markers: 1,
            max_pretrigger: 64,
            max_replay: 64,
            max_metrics: 4,
            shr_locks: 1,
            shr_rejections: 0,
            shr_selections: [Some(EXPECTED_SHR_TIMING_FULL_LONG_SELECTION), None],
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 703,
            max_shr_metric_replay: 657,
        }
    );
    assert_eq!(combined_ticks, EXPECTED_SHR_TIMING_COMBINED_TICKS);
    assert_eq!(full_long_ticks, EXPECTED_SHR_TIMING_FULL_LONG_TICKS);
    assert_eq!(combined_payloads, transmitted);
    assert_eq!(full_long_payloads, [longer_psdu]);
    assert_eq!(combined_counts, CandidateCounts::BOTH_TRANSMITTED);
    assert_eq!(full_long_counts, CandidateCounts::LONG_ONLY);
    for (((position, payload), expected), event) in combined
        .candidates
        .iter()
        .zip(&transmitted)
        .zip(combined_events.iter().copied())
    {
        assert_eq!(payload, expected);
        let first_tick = event.replay_start / 2 + 1;
        let last_tick = (event.end_exclusive - 1) / 2 + 1;
        assert!((first_tick..=last_tick).contains(position));
    }
    let (full_long_position, full_long_payload) = &full_long.candidates[0];
    assert_eq!(full_long_payload, &transmitted[1]);
    let full_long_event = full_long_events[0];
    assert!(
        (full_long_event.replay_start / 2 + 1..=(full_long_event.end_exclusive - 1) / 2 + 1)
            .contains(full_long_position)
    );

    assert_eq!(combined.stats.source_samples, inputs.combined.len() as u64);
    assert_eq!(combined.stats.starts, 2);
    assert_eq!(combined.stats.ends, 2);
    assert_eq!(combined.stats.markers, 2);
    assert_eq!(combined.stats.shr_locks, 2);
    assert_eq!(combined.stats.shr_rejections, 0);
    assert_eq!(combined.stats.shr_selection_overflow, 0);
    assert_eq!(combined.stats.max_pretrigger, 64);
    assert_eq!(combined.stats.max_replay, 64);
    assert_eq!(combined.stats.max_metrics, 4);
    assert_eq!(combined.stats.max_shr_selection_metrics, 703);
    let combined_selections = combined
        .stats
        .shr_selections
        .map(|selection| selection.expect("combined event must have an SHR lock"));
    assert_eq!(
        combined.stats.max_shr_metric_replay,
        combined_selections
            .iter()
            .map(|selection| 703 - usize::from(selection.start_offset))
            .max()
            .unwrap()
    );

    assert_eq!(
        full_long.stats.source_samples,
        inputs.full_long.len() as u64
    );
    assert_eq!(full_long.stats.starts, 1);
    assert_eq!(full_long.stats.ends, 1);
    assert_eq!(full_long.stats.markers, 1);
    assert_eq!(full_long.stats.shr_locks, 1);
    assert_eq!(full_long.stats.shr_rejections, 0);
    assert_eq!(full_long.stats.shr_selection_overflow, 0);
    assert_eq!(full_long.stats.max_pretrigger, 64);
    assert_eq!(full_long.stats.max_replay, 64);
    assert_eq!(full_long.stats.max_metrics, 4);
    assert_eq!(full_long.stats.max_shr_selection_metrics, 703);
    let full_long_selection =
        full_long.stats.shr_selections[0].expect("standalone long event must have an SHR lock");
    assert_eq!(full_long.stats.shr_selections[1], None);
    assert_eq!(
        full_long.stats.max_shr_metric_replay,
        703 - usize::from(full_long_selection.start_offset)
    );

    for (selection, event) in combined_selections
        .iter()
        .zip(combined_events.iter().copied())
    {
        assert!(selection.start_offset < 64);
        assert!(selection.mu_sixteenths < 16);
        assert_eq!(
            selection.source_sample_index,
            event.replay_start + u64::from(selection.start_offset)
        );
    }
    assert_eq!(
        full_long_selection.source_sample_index,
        full_long_event.replay_start + u64::from(full_long_selection.start_offset)
    );
    assert_eq!(
        (
            combined_selections[1].start_offset,
            combined_selections[1].mu_sixteenths,
        ),
        (
            full_long_selection.start_offset,
            full_long_selection.mu_sixteenths,
        )
    );
    let long_source_offset = inputs.combined.len() - inputs.full_long.len();
    assert!(long_source_offset.is_multiple_of(2));
    assert_eq!(
        combined_selections[1].source_sample_index,
        full_long_selection.source_sample_index + long_source_offset as u64
    );
    assert_eq!(
        combined_ticks[1],
        full_long_ticks[0] + long_source_offset as u64 / 2
    );
    assert_eq!(
        &inputs.combined
            [combined_events[1].replay_start as usize..combined_events[1].end_exclusive as usize],
        &inputs.full_long
            [full_long_event.replay_start as usize..full_long_event.end_exclusive as usize]
    );
}

#[test]
fn full_cf32_shr_rate_timing_initializer_matches_opened_span_oracle() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let combined_events = detect_causal_bursts(&inputs.combined);
    let full_long_events = detect_causal_bursts(&inputs.full_long);
    assert_eq!(combined_events, EXPECTED_CAUSAL_BURST_DETECTOR_COMBINED);
    assert_eq!(full_long_events, EXPECTED_CAUSAL_BURST_DETECTOR_FULL_LONG);

    let combined = run_shr_rate_burst_streaming_case(
        "shr-rate-timing-initializer-combined",
        17,
        &inputs.combined,
    );
    let full_long = run_shr_rate_burst_streaming_case(
        "shr-rate-timing-initializer-full-long",
        17,
        &inputs.full_long,
    );
    let combined_payloads = combined
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let full_long_payloads = full_long
        .candidates
        .iter()
        .map(|(_, payload)| payload.clone())
        .collect::<Vec<_>>();
    let combined_counts = count_candidates(&combined_payloads, &transmitted);
    let full_long_counts = count_candidates(&full_long_payloads, &transmitted);
    let combined_ticks = combined
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    let full_long_ticks = full_long
        .candidates
        .iter()
        .map(|(position, _)| *position)
        .collect::<Vec<_>>();
    eprintln!(
        "SHR rate timing initializer combined_stats={:?} combined_ticks={combined_ticks:?} combined_counts={combined_counts:?} full_long_stats={:?} full_long_ticks={full_long_ticks:?} full_long_counts={full_long_counts:?}",
        combined.stats, full_long.stats,
    );

    assert_eq!(
        combined.stats,
        ShrRateClockStats {
            burst: BurstClockStats {
                source_samples: inputs.combined.len() as u64,
                starts: 2,
                ends: 2,
                markers: 2,
                max_pretrigger: 64,
                max_replay: 64,
                max_metrics: 4,
                shr_locks: 2,
                shr_rejections: 0,
                shr_selections: [None; 2],
                shr_selection_overflow: 0,
                max_shr_selection_metrics: 704,
                max_shr_metric_replay: 662,
            },
            shr_rate_selections: EXPECTED_SHR_RATE_TIMING_COMBINED_SELECTIONS.map(Some),
        }
    );
    assert_eq!(
        full_long.stats,
        ShrRateClockStats {
            burst: BurstClockStats {
                source_samples: inputs.full_long.len() as u64,
                starts: 1,
                ends: 1,
                markers: 1,
                max_pretrigger: 64,
                max_replay: 64,
                max_metrics: 4,
                shr_locks: 1,
                shr_rejections: 0,
                shr_selections: [None; 2],
                shr_selection_overflow: 0,
                max_shr_selection_metrics: 704,
                max_shr_metric_replay: 658,
            },
            shr_rate_selections: [Some(EXPECTED_SHR_RATE_TIMING_FULL_LONG_SELECTION), None],
        }
    );
    assert_eq!(combined_ticks, EXPECTED_SHR_RATE_TIMING_COMBINED_TICKS);
    assert_eq!(full_long_ticks, EXPECTED_SHR_RATE_TIMING_FULL_LONG_TICKS);
    assert_eq!(combined_payloads, transmitted);
    assert_eq!(full_long_payloads, [longer_psdu]);
    assert_eq!(combined_counts, CandidateCounts::BOTH_TRANSMITTED);
    assert_eq!(full_long_counts, CandidateCounts::LONG_ONLY);
    assert!(combined_counts.has_no_anomalies());
    assert!(full_long_counts.has_no_anomalies());

    let phase_only_reference = EXPECTED_TIMING_RATE_SHR_OBSERVATIONS[0][1][0];
    assert_eq!(phase_only_reference.counts, combined_counts);
    assert_eq!(
        phase_only_reference.shr_selections,
        combined
            .stats
            .shr_rate_selections
            .map(|selection| selection.map(|selection| selection.timing))
    );
    assert_eq!(
        phase_only_reference.identity_ticks,
        [
            combined_ticks.first().copied(),
            combined_ticks.get(1).copied()
        ]
    );

    for (((position, payload), expected), event) in combined
        .candidates
        .iter()
        .zip(&transmitted)
        .zip(combined_events.iter().copied())
    {
        assert_eq!(payload, expected);
        let first_tick = event.replay_start / 2 + 1;
        let last_tick = (event.end_exclusive - 1) / 2 + 1;
        assert!((first_tick..=last_tick).contains(position));
    }
    let (full_long_position, full_long_payload) = &full_long.candidates[0];
    assert_eq!(full_long_payload, &transmitted[1]);
    let full_long_event = full_long_events[0];
    assert!(
        (full_long_event.replay_start / 2 + 1..=(full_long_event.end_exclusive - 1) / 2 + 1)
            .contains(full_long_position)
    );

    let assert_stats =
        |stats: &ShrRateClockStats, source_samples: usize, events: &[DetectedBurst]| {
            let burst = stats.burst;
            assert_eq!(burst.source_samples, source_samples as u64);
            assert_eq!(burst.starts, events.len() as u64);
            assert_eq!(burst.ends, events.len() as u64);
            assert_eq!(burst.markers, events.len() as u64);
            assert_eq!(burst.shr_locks, events.len() as u64);
            assert_eq!(burst.shr_rejections, 0);
            assert_eq!(burst.shr_selection_overflow, 0);
            assert_eq!(burst.shr_selections, [None; 2]);
            assert_eq!(burst.max_pretrigger, 64);
            assert_eq!(burst.max_replay, 64);
            assert_eq!(burst.max_metrics, 4);
            assert_eq!(burst.max_shr_selection_metrics, 704);
            let selections = stats
                .shr_rate_selections
                .iter()
                .copied()
                .flatten()
                .collect::<Vec<_>>();
            assert_eq!(selections.len(), events.len());
            assert_eq!(selections.len() as u64, burst.shr_locks);
            for (selection, event) in selections.iter().zip(events) {
                assert!(selection.timing.start_offset < 64);
                assert!(selection.timing.mu_sixteenths < 16);
                assert!([-100, 0, 100].contains(&selection.rate_error_ppm));
                assert_eq!(
                    selection.timing.source_sample_index,
                    event.replay_start + u64::from(selection.timing.start_offset)
                );
            }
            assert_eq!(
                burst.max_shr_metric_replay,
                selections
                    .iter()
                    .map(|selection| 704 - usize::from(selection.timing.start_offset))
                    .max()
                    .unwrap()
            );
            selections
        };
    let combined_selections =
        assert_stats(&combined.stats, inputs.combined.len(), &combined_events);
    let full_long_selections =
        assert_stats(&full_long.stats, inputs.full_long.len(), &full_long_events);
    let full_long_selection = full_long_selections[0];
    assert_eq!(
        (
            combined_selections[1].timing.start_offset,
            combined_selections[1].timing.mu_sixteenths,
            combined_selections[1].rate_error_ppm,
        ),
        (
            full_long_selection.timing.start_offset,
            full_long_selection.timing.mu_sixteenths,
            full_long_selection.rate_error_ppm,
        )
    );
    let long_source_offset = inputs.combined.len() - inputs.full_long.len();
    assert_eq!(long_source_offset, 81_402);
    assert!(long_source_offset.is_multiple_of(2));
    assert_eq!(long_source_offset / 2, 40_701);
    assert_eq!(
        combined_selections[1].timing.source_sample_index,
        full_long_selection.timing.source_sample_index + long_source_offset as u64
    );
    assert_eq!(
        combined_ticks[1],
        full_long_ticks[0] + long_source_offset as u64 / 2
    );
    assert_eq!(
        &inputs.combined
            [combined_events[1].replay_start as usize..combined_events[1].end_exclusive as usize],
        &inputs.full_long
            [full_long_event.replay_start as usize..full_long_event.end_exclusive as usize]
    );
}

#[test]
fn sample_rate_error_operator_preserves_identity_power_and_direction() {
    let mut impulse = vec![Complex32::new(0.0, 0.0); 8192];
    let impulse_index = 4096usize;
    impulse[impulse_index] = Complex32::new(1.0, -0.5);
    assert_eq!(apply_sample_rate_error(&impulse, 0), impulse);

    let negative = apply_sample_rate_error(&impulse, -100);
    let positive = apply_sample_rate_error(&impulse, 100);
    let scale = |ppm| 1.0 + f64::from(ppm) / TIMING_RATE_PPM_DENOMINATOR;
    assert_eq!(
        negative.len(),
        (impulse.len() as f64 * scale(-100)).round() as usize
    );
    assert_eq!(
        positive.len(),
        (impulse.len() as f64 * scale(100)).round() as usize
    );
    assert!(negative.len() < impulse.len());
    assert!(positive.len() > impulse.len());
    assert!(
        negative
            .iter()
            .chain(&positive)
            .all(|sample| sample.re.is_finite() && sample.im.is_finite())
    );

    let reference_energy = total_waveform_energy(&impulse);
    assert!((total_waveform_energy(&negative) / reference_energy - scale(-100)).abs() < 1e-6);
    assert!((total_waveform_energy(&positive) / reference_energy - scale(100)).abs() < 1e-6);
    let centroid = |samples: &[Complex32]| {
        let energy = total_waveform_energy(samples);
        samples
            .iter()
            .enumerate()
            .map(|(index, sample)| index as f64 * f64::from(sample.norm_sqr()))
            .sum::<f64>()
            / energy
    };
    let negative_centroid = centroid(&negative);
    let positive_centroid = centroid(&positive);
    let expected_negative_centroid = impulse_index as f64 * scale(-100);
    let expected_positive_centroid = impulse_index as f64 * scale(100);
    assert!(
        (negative_centroid - expected_negative_centroid).abs() < 0.15,
        "negative centroid {negative_centroid} != {expected_negative_centroid}"
    );
    assert!(
        (positive_centroid - expected_positive_centroid).abs() < 0.15,
        "positive centroid {positive_centroid} != {expected_positive_centroid}"
    );
    assert!(negative_centroid < impulse_index as f64);
    assert!(positive_centroid > impulse_index as f64);

    let constant = vec![Complex32::new(0.5, -0.25); 8192];
    for output in [
        apply_sample_rate_error(&constant, -100),
        apply_sample_rate_error(&constant, 100),
    ] {
        for sample in &output[64..output.len() - 64] {
            assert!((sample.re - 0.5).abs() < 0.002);
            assert!((sample.im + 0.25).abs() < 0.002);
        }
    }

    let tone_step = 0.2f64;
    let tone: Vec<Complex32> = (0..8192)
        .map(|index| {
            let phase = tone_step * index as f64;
            Complex32::new(phase.cos() as f32, phase.sin() as f32)
        })
        .collect();
    for rate_error_ppm in [-100, 100] {
        let output = apply_sample_rate_error(&tone, rate_error_ppm);
        let mean_step = output[64..output.len() - 64]
            .windows(2)
            .map(|pair| f64::from((pair[0].conj() * pair[1]).arg()))
            .sum::<f64>()
            / (output.len() - 129) as f64;
        assert!((mean_step - tone_step / scale(rate_error_ppm)).abs() < 1e-5);
    }
}

#[test]
fn full_cf32_fractional_timing_clean_development_grid_is_frozen() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observations = TIMING_DEVELOPMENT_OFFSETS.map(|offset| {
        run_fixed_timing_candidate_case(
            &format!("fractional-timing-clean-{offset:+}-sixteenths"),
            17,
            &fractional_timing_samples(&short_waveform, &longer_waveform, offset),
            &transmitted,
        )
    });
    for (offset_index, observed) in observations.iter().enumerate() {
        assert_eq!(
            [observed[0], observed[1]],
            EXPECTED_TIMING_CLEAN_DEVELOPMENT_COUNTS[offset_index]
        );
        assert_eq!(*observed, [CandidateCounts::BOTH_TRANSMITTED; 4]);
    }
    eprintln!(
        "fractional timing clean development offsets={TIMING_DEVELOPMENT_OFFSETS:?} observations={observations:?}"
    );
}

#[test]
fn full_cf32_fractional_timing_6db_development_grid_is_frozen() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observations: [[TimingCandidateObservations; 9]; 2] = std::array::from_fn(|seed_index| {
        let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
        TIMING_DEVELOPMENT_OFFSETS.map(|offset| {
            run_fixed_timing_candidate_case(
                &format!("fractional-timing-6db-seed-{seed_index}-{offset:+}-sixteenths"),
                17,
                &fractional_timing_awgn_samples(&short_waveform, &longer_waveform, offset, seed),
                &transmitted,
            )
        })
    });
    eprintln!(
        "fractional timing 6 dB development seeds={TIMING_DEVELOPMENT_SEEDS:?} offsets={TIMING_DEVELOPMENT_OFFSETS:?} observations={observations:?}"
    );
    assert_eq!(observations, EXPECTED_TIMING_6DB_FIXED_CANDIDATE_COUNTS);
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (offset_index, observed) in seed_observations.iter().enumerate() {
            assert_eq!(
                [observed[0], observed[1]],
                EXPECTED_TIMING_6DB_DEVELOPMENT_COUNTS[seed_index][offset_index],
                "ordinary timing arms drifted at seed {seed_index}, offset {}",
                TIMING_DEVELOPMENT_OFFSETS[offset_index]
            );
            assert!(
                observed.iter().all(|counts| counts.has_no_anomalies()),
                "fixed timing candidate emitted an anomalous record at seed {seed_index}, offset {}: {observed:?}",
                TIMING_DEVELOPMENT_OFFSETS[offset_index]
            );
            assert_eq!(
                observed
                    .iter()
                    .fold(0u8, |mask, counts| mask | counts.recovered_mask),
                CandidateCounts::BOTH_TRANSMITTED.recovered_mask,
                "fixed timing candidate missed a transmitted PSDU at seed {seed_index}, offset {}: {observed:?}",
                TIMING_DEVELOPMENT_OFFSETS[offset_index]
            );
        }
    }
}

#[test]
fn full_cf32_fractional_timing_rate_endpoint_screen_is_frozen() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let seed = TIMING_DEVELOPMENT_SEEDS[0];
    let observations: [RateScreenTimingCandidateObservations; 3] =
        TIMING_RATE_SCREEN_CASES.map(|(offset, rate_error_ppm)| {
            run_rate_screen_timing_candidate_case(
                &format!(
                    "fractional-timing-rate-screen-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
                ),
                17,
                &fractional_timing_rate_awgn_samples(
                    &short_waveform,
                    &longer_waveform,
                    offset,
                    rate_error_ppm,
                    seed,
                ),
                &transmitted,
            )
        });
    eprintln!(
        "fractional timing endpoint-rate screen cases={TIMING_RATE_SCREEN_CASES:?} observations={observations:?}"
    );
    assert_eq!(observations, EXPECTED_TIMING_RATE_ENDPOINT_SCREEN_COUNTS);
    let expected_union_masks = [
        CandidateCounts::BOTH_TRANSMITTED.recovered_mask,
        CandidateCounts::SHORT_ONLY.recovered_mask,
        CandidateCounts::BOTH_TRANSMITTED.recovered_mask,
    ];
    for (case_index, observed) in observations.iter().enumerate() {
        assert!(
            observed.iter().all(|counts| counts.has_no_anomalies()),
            "endpoint-rate screen emitted an anomalous record for case {}: {observed:?}",
            case_index
        );
        assert_eq!(
            observed[..4]
                .iter()
                .fold(0u8, |mask, counts| mask | counts.recovered_mask),
            CandidateCounts::SHORT_ONLY.recovered_mask,
            "established four arms no longer reproduce screen failure {case_index}: {observed:?}"
        );
        assert_eq!(
            observed
                .iter()
                .fold(0u8, |mask, counts| mask | counts.recovered_mask),
            expected_union_masks[case_index],
            "endpoint-rate screen changed at case {case_index}: {observed:?}"
        );
    }
    assert_ne!(
        expected_union_masks[1],
        CandidateCounts::BOTH_TRANSMITTED.recovered_mask,
        "the rejected middle screen case must not be mistaken for qualification"
    );
}

#[test]
fn full_cf32_fractional_timing_rate_gain_sweep_is_characterized() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observations: [[[GainSweepTimingCandidateObservations; 3]; 3]; 2] = std::array::from_fn(
        |seed_index| {
            let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
            TIMING_RATE_DEVELOPMENT_OFFSETS.map(|offset| {
                TIMING_RATE_ERROR_PPM.map(|rate_error_ppm| {
                    run_gain_sweep_timing_candidate_case(
                        &format!(
                            "fractional-timing-rate-gain-seed-{seed_index}-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
                        ),
                        17,
                        &fractional_timing_rate_awgn_samples(
                            &short_waveform,
                            &longer_waveform,
                            offset,
                            rate_error_ppm,
                            seed,
                        ),
                        &transmitted,
                    )
                })
            })
        },
    );
    eprintln!(
        "fractional timing rate adaptive gain sweep seeds={TIMING_DEVELOPMENT_SEEDS:?} offsets={TIMING_RATE_DEVELOPMENT_OFFSETS:?} ppm={TIMING_RATE_ERROR_PPM:?} observations={observations:?}"
    );
    assert_eq!(observations, EXPECTED_TIMING_RATE_GAIN_SWEEP_COUNTS);

    let mut fixed_bank_covered = 0;
    let mut fixed_bank_with_gain_covered = [0; 4];
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (phase_index, phase_observations) in seed_observations.iter().enumerate() {
            let development_offset_index = TIMING_DEVELOPMENT_OFFSETS
                .iter()
                .position(|offset| *offset == TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index])
                .expect("rate phase is a development phase");
            for (rate_index, gain_observations) in phase_observations.iter().enumerate() {
                let fixed_observations = EXPECTED_TIMING_RATE_FIXED_CANDIDATE_COUNTS[seed_index]
                    [phase_index][rate_index];
                assert!(
                    fixed_observations
                        .iter()
                        .chain(gain_observations)
                        .all(|counts| counts.has_no_anomalies()),
                    "adaptive gain screen emitted an anomaly at seed {seed_index}, phase {}, rate {}: fixed={fixed_observations:?} gain={gain_observations:?}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                assert_eq!(
                    gain_observations[1],
                    EXPECTED_TIMING_RATE_SLOW_CANDIDATE_COUNTS[seed_index][phase_index][rate_index],
                    "slow-loop equivalence control drifted at seed {seed_index}, phase {}, rate {}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                assert_eq!(
                    gain_observations[2], fixed_observations[0],
                    "established-loop equivalence control drifted at seed {seed_index}, phase {}, rate {}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index], TIMING_RATE_ERROR_PPM[rate_index]
                );
                if TIMING_RATE_ERROR_PPM[rate_index] == 0 {
                    assert_eq!(
                        fixed_observations,
                        EXPECTED_TIMING_6DB_FIXED_CANDIDATE_COUNTS[seed_index]
                            [development_offset_index],
                        "zero-rate fixed bank drifted at seed {seed_index}, phase {}",
                        TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index]
                    );
                }

                let fixed_mask = fixed_observations
                    .iter()
                    .fold(0u8, |mask, counts| mask | counts.recovered_mask);
                if fixed_mask == CandidateCounts::BOTH_TRANSMITTED.recovered_mask {
                    fixed_bank_covered += 1;
                }
                for (gain_index, counts) in gain_observations.iter().enumerate() {
                    if fixed_mask | counts.recovered_mask
                        == CandidateCounts::BOTH_TRANSMITTED.recovered_mask
                    {
                        fixed_bank_with_gain_covered[gain_index] += 1;
                    }
                }
            }
        }
    }
    assert_eq!(fixed_bank_covered, 14);
    assert_eq!(fixed_bank_with_gain_covered, [15, 15, 14, 16]);

    // This is the frozen selection order after excluding gain index 2, which
    // is the established-loop equivalence control. Indices 1 and 3 are tied
    // at one octave from the established gain, so the lower gain ranks first.
    let selection_order = [1, 3, 0];
    let closes_with_baseline = |gain_index: usize, baseline_arm_count: usize| {
        observations
            .iter()
            .enumerate()
            .all(|(seed_index, seed_observations)| {
                seed_observations
                    .iter()
                    .enumerate()
                    .all(|(phase_index, phase_observations)| {
                        phase_observations.iter().enumerate().all(
                            |(rate_index, gain_observations)| {
                                let baseline = EXPECTED_TIMING_RATE_FIXED_CANDIDATE_COUNTS
                                    [seed_index][phase_index][rate_index];
                                let baseline_mask = baseline[..baseline_arm_count]
                                    .iter()
                                    .fold(0u8, |mask, counts| mask | counts.recovered_mask);
                                baseline_mask | gain_observations[gain_index].recovered_mask
                                    == CandidateCounts::BOTH_TRANSMITTED.recovered_mask
                            },
                        )
                    })
            })
    };
    let ordinary_pair_selection = selection_order
        .into_iter()
        .find(|gain_index| closes_with_baseline(*gain_index, 2));
    let fixed_bank_selection = selection_order
        .into_iter()
        .find(|gain_index| closes_with_baseline(*gain_index, 4));
    assert_eq!(ordinary_pair_selection, None);
    assert_eq!(fixed_bank_selection, None);
}

#[test]
fn full_cf32_fractional_timing_rate_shr_initializer_is_characterized() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observations: [[[BurstTimingCandidateObservation; 3]; 3]; 2] = std::array::from_fn(
        |seed_index| {
            let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
            TIMING_RATE_DEVELOPMENT_OFFSETS.map(|offset| {
                TIMING_RATE_ERROR_PPM.map(|rate_error_ppm| {
                    let samples = fractional_timing_rate_awgn_samples(
                        &short_waveform,
                        &longer_waveform,
                        offset,
                        rate_error_ppm,
                        seed,
                    );
                    run_burst_timing_candidate_case(
                        &format!(
                            "fractional-timing-rate-burst-seed-{seed_index}-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
                        ),
                        17,
                        &samples,
                        &transmitted,
                    )
                })
            })
        },
    );
    let burst_observations: [[[CandidateCounts; 3]; 3]; 2] = std::array::from_fn(|seed_index| {
        std::array::from_fn(|phase_index| {
            std::array::from_fn(|rate_index| {
                observations[seed_index][phase_index][rate_index].counts[4]
            })
        })
    });
    let shr_observations: [[[ShrRateObservation; 3]; 3]; 2] = std::array::from_fn(|seed_index| {
        std::array::from_fn(|phase_index| {
            std::array::from_fn(|rate_index| observations[seed_index][phase_index][rate_index].shr)
        })
    });
    let frozen_shr_observations: [[[FrozenShrRateObservation; 3]; 3]; 2] =
        std::array::from_fn(|seed_index| {
            std::array::from_fn(|phase_index| {
                std::array::from_fn(|rate_index| {
                    shr_observations[seed_index][phase_index][rate_index].frozen()
                })
            })
        });

    let mut burst_covered = 0;
    let mut burst_failures = Vec::new();
    let mut shr_covered = 0;
    let mut shr_failures = Vec::new();
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (phase_index, phase_observations) in seed_observations.iter().enumerate() {
            for (rate_index, observed) in phase_observations.iter().enumerate() {
                let burst_mask = observed.counts[..5]
                    .iter()
                    .fold(0u8, |mask, counts| mask | counts.recovered_mask);
                if burst_mask == CandidateCounts::BOTH_TRANSMITTED.recovered_mask {
                    burst_covered += 1;
                } else {
                    burst_failures.push((seed_index, phase_index, rate_index, burst_mask));
                }
                let shr_mask = observed.shr.counts.recovered_mask;
                if shr_mask == CandidateCounts::BOTH_TRANSMITTED.recovered_mask {
                    shr_covered += 1;
                } else {
                    shr_failures.push((seed_index, phase_index, rate_index, shr_mask));
                }
            }
        }
    }
    eprintln!(
        "fractional timing rate SHR seeds={TIMING_DEVELOPMENT_SEEDS:?} offsets={TIMING_RATE_DEVELOPMENT_OFFSETS:?} ppm={TIMING_RATE_ERROR_PPM:?} burst={burst_observations:?} shr={frozen_shr_observations:?} burst_covered={burst_covered} burst_failures={burst_failures:?} shr_covered={shr_covered} shr_failures={shr_failures:?}"
    );
    assert_eq!(burst_observations, EXPECTED_TIMING_RATE_BURST_COUNTS);
    assert_eq!(
        frozen_shr_observations,
        EXPECTED_TIMING_RATE_SHR_OBSERVATIONS
    );
    assert_eq!(burst_covered, 16);
    assert_eq!(burst_failures, [(0, 0, 0, 0b01), (0, 2, 2, 0b01)]);
    assert_ne!(
        burst_covered, 18,
        "rejected burst candidate must not qualify"
    );
    assert_eq!(shr_covered, 8);
    assert_eq!(
        shr_failures,
        [
            (0, 0, 0, 0b01),
            (0, 0, 1, 0b01),
            (0, 0, 2, 0b10),
            (0, 2, 0, 0b01),
            (0, 2, 1, 0b01),
            (0, 2, 2, 0b01),
            (1, 0, 0, 0b10),
            (1, 0, 2, 0b01),
            (1, 1, 1, 0b10),
            (1, 1, 2, 0b01),
        ]
    );
    assert_ne!(shr_covered, 18, "rejected SHR initializer must not qualify");
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (phase_index, phase_observations) in seed_observations.iter().enumerate() {
            for (rate_index, observed) in phase_observations.iter().enumerate() {
                assert_eq!(
                    observed.counts[..4],
                    EXPECTED_TIMING_RATE_FIXED_CANDIDATE_COUNTS[seed_index][phase_index]
                        [rate_index],
                    "baseline drifted at seed {seed_index}, phase {}, rate {}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                assert!(
                    observed
                        .counts
                        .iter()
                        .all(|counts| counts.has_no_anomalies()),
                    "prior burst timing arm emitted an anomaly at seed {seed_index}, phase {}, rate {}: {observed:?}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                let shr = observed.shr;
                assert!(shr.counts.has_no_anomalies());
                assert_eq!(shr.stats.source_samples, shr.input_samples);
                assert_eq!(shr.stats.starts, 2);
                assert_eq!(shr.stats.ends, 2);
                assert_eq!(shr.stats.markers, shr.stats.shr_locks);
                assert_eq!(shr.stats.max_pretrigger, 64);
                assert_eq!(shr.stats.max_replay, 64);
                assert_eq!(shr.stats.max_metrics, 4);
                assert_eq!(shr.stats.shr_locks + shr.stats.shr_rejections, 2);
                assert_eq!(shr.stats.shr_selection_overflow, 0);
                assert_eq!(shr.stats.max_shr_selection_metrics, 703);
                let max_replay = shr
                    .stats
                    .shr_selections
                    .iter()
                    .flatten()
                    .map(|selection| {
                        assert!(selection.start_offset < 64);
                        assert!(selection.mu_sixteenths < 16);
                        703 - usize::from(selection.start_offset)
                    })
                    .max()
                    .expect("each rate cell must retain at least one SHR lock");
                assert_eq!(shr.stats.max_shr_metric_replay, max_replay);
                for (identity_index, tick) in shr.identity_ticks.iter().enumerate() {
                    assert_eq!(
                        tick.is_some(),
                        shr.counts.recovered_mask & (1 << identity_index) != 0
                    );
                }
            }
        }
    }
}

#[test]
fn full_cf32_fractional_timing_rate_joint_shr_initializer_is_characterized() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let observations: [[[JointShrRateGridObservation; 3]; 3]; 2] = std::array::from_fn(
        |seed_index| {
            let seed = TIMING_DEVELOPMENT_SEEDS[seed_index];
            TIMING_RATE_DEVELOPMENT_OFFSETS.map(|offset| {
                TIMING_RATE_ERROR_PPM.map(|rate_error_ppm| {
                    let samples = fractional_timing_rate_awgn_samples(
                        &short_waveform,
                        &longer_waveform,
                        offset,
                        rate_error_ppm,
                        seed,
                    );
                    let name = format!(
                        "fractional-timing-rate-joint-shr-seed-{seed_index}-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
                    );
                    JointShrRateGridObservation {
                        prior: run_burst_timing_candidate_case(
                            &format!("{name}-prior"),
                            17,
                            &samples,
                            &transmitted,
                        ),
                        joint: run_joint_shr_rate_candidate_case(
                            &format!("{name}-joint"),
                            17,
                            &samples,
                            &transmitted,
                        ),
                    }
                })
            })
        },
    );
    let burst_observations: [[[CandidateCounts; 3]; 3]; 2] = std::array::from_fn(|seed_index| {
        std::array::from_fn(|phase_index| {
            std::array::from_fn(|rate_index| {
                observations[seed_index][phase_index][rate_index]
                    .prior
                    .counts[4]
            })
        })
    });
    let frozen_shr_observations: [[[FrozenShrRateObservation; 3]; 3]; 2] =
        std::array::from_fn(|seed_index| {
            std::array::from_fn(|phase_index| {
                std::array::from_fn(|rate_index| {
                    observations[seed_index][phase_index][rate_index]
                        .prior
                        .shr
                        .frozen()
                })
            })
        });
    let frozen_joint_observations: [[[FrozenJointShrRateObservation; 3]; 3]; 2] =
        std::array::from_fn(|seed_index| {
            std::array::from_fn(|phase_index| {
                std::array::from_fn(|rate_index| {
                    observations[seed_index][phase_index][rate_index]
                        .joint
                        .frozen()
                })
            })
        });

    let mut joint_covered = 0;
    let mut joint_failures = Vec::new();
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (phase_index, phase_observations) in seed_observations.iter().enumerate() {
            for (rate_index, observed) in phase_observations.iter().enumerate() {
                let joint_mask = observed.joint.counts.recovered_mask;
                if joint_mask == CandidateCounts::BOTH_TRANSMITTED.recovered_mask {
                    joint_covered += 1;
                } else {
                    joint_failures.push((seed_index, phase_index, rate_index, joint_mask));
                }
            }
        }
    }
    eprintln!(
        "fractional timing rate joint SHR seeds={TIMING_DEVELOPMENT_SEEDS:?} offsets={TIMING_RATE_DEVELOPMENT_OFFSETS:?} ppm={TIMING_RATE_ERROR_PPM:?} burst={burst_observations:?} shr={frozen_shr_observations:?} joint={frozen_joint_observations:?} joint_covered={joint_covered} joint_failures={joint_failures:?}"
    );

    assert_eq!(burst_observations, EXPECTED_TIMING_RATE_BURST_COUNTS);
    assert_eq!(
        frozen_shr_observations,
        EXPECTED_TIMING_RATE_SHR_OBSERVATIONS
    );
    assert_eq!(
        frozen_joint_observations,
        EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS
    );
    for (seed_index, seed_observations) in observations.iter().enumerate() {
        for (phase_index, phase_observations) in seed_observations.iter().enumerate() {
            for (rate_index, observed) in phase_observations.iter().enumerate() {
                assert_eq!(
                    observed.prior.counts[..4],
                    EXPECTED_TIMING_RATE_FIXED_CANDIDATE_COUNTS[seed_index][phase_index]
                        [rate_index],
                    "baseline drifted at seed {seed_index}, phase {}, rate {}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                assert!(
                    observed
                        .prior
                        .counts
                        .iter()
                        .all(|counts| counts.has_no_anomalies()),
                    "prior timing arm emitted an anomaly at seed {seed_index}, phase {}, rate {}: {:?}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index],
                    observed.prior
                );

                let shr = observed.prior.shr;
                assert!(shr.counts.has_no_anomalies());
                assert_eq!(shr.stats.source_samples, shr.input_samples);
                assert_eq!(shr.stats.starts, 2);
                assert_eq!(shr.stats.ends, 2);
                assert_eq!(shr.stats.markers, shr.stats.shr_locks);
                assert_eq!(shr.stats.max_pretrigger, 64);
                assert_eq!(shr.stats.max_replay, 64);
                assert_eq!(shr.stats.max_metrics, 4);
                assert_eq!(shr.stats.shr_locks + shr.stats.shr_rejections, 2);
                assert_eq!(shr.stats.shr_selection_overflow, 0);
                assert_eq!(shr.stats.max_shr_selection_metrics, 703);
                let max_replay = shr
                    .stats
                    .shr_selections
                    .iter()
                    .flatten()
                    .map(|selection| 703 - usize::from(selection.start_offset))
                    .max()
                    .expect("each rate cell must retain at least one phase-only SHR lock");
                assert_eq!(shr.stats.max_shr_metric_replay, max_replay);
                for (identity_index, tick) in shr.identity_ticks.iter().enumerate() {
                    assert_eq!(
                        tick.is_some(),
                        shr.counts.recovered_mask & (1 << identity_index) != 0
                    );
                }

                let joint = observed.joint;
                assert!(
                    joint.counts.has_no_anomalies(),
                    "joint SHR arm emitted an anomaly at seed {seed_index}, phase {}, rate {}: {joint:?}",
                    TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index],
                    TIMING_RATE_ERROR_PPM[rate_index]
                );
                let stats = joint.stats.burst;
                assert_eq!(stats.source_samples, joint.input_samples);
                assert_eq!(stats.starts, 2);
                assert_eq!(stats.ends, 2);
                assert_eq!(stats.markers, stats.shr_locks);
                assert_eq!(stats.max_pretrigger, 64);
                assert_eq!(stats.max_replay, 64);
                assert_eq!(stats.max_metrics, 4);
                assert_eq!(stats.shr_locks + stats.shr_rejections, 2);
                assert_eq!(stats.shr_selections, [None; 2]);
                assert_eq!(stats.shr_selection_overflow, 0);
                assert_eq!(stats.max_shr_selection_metrics, 704);
                let max_replay = joint
                    .stats
                    .shr_rate_selections
                    .iter()
                    .flatten()
                    .map(|selection| 704 - usize::from(selection.timing.start_offset))
                    .max();
                assert_eq!(stats.max_shr_metric_replay, max_replay.unwrap_or_default());
                for (identity_index, tick) in joint.identity_ticks.iter().enumerate() {
                    assert_eq!(
                        tick.is_some(),
                        joint.counts.recovered_mask & (1 << identity_index) != 0
                    );
                }
            }
        }
    }
    assert_eq!(joint_covered, 8);
    assert_eq!(
        joint_failures,
        [
            (0, 0, 0, 0b01),
            (0, 0, 1, 0b01),
            (0, 0, 2, 0b10),
            (0, 2, 0, 0b01),
            (0, 2, 1, 0b01),
            (0, 2, 2, 0b01),
            (1, 0, 0, 0b10),
            (1, 0, 2, 0b01),
            (1, 1, 1, 0b10),
            (1, 1, 2, 0b01),
        ]
    );
    assert_ne!(
        joint_covered, 18,
        "rejected joint SHR initializer must not qualify"
    );
}

#[test]
fn full_cf32_joint_shr_rate_three_cell_trace_is_characterized() {
    const EXPECTED_TRACE_SHA256: &str =
        "22d52b3fc4c86bcaab925a8da2f4e598bf22815645c680dafbb8fddfc9a65a60";
    const EXPECTED_LOW_LONG_BEST_UNCONSTRAINED: ShrRateScoredCandidateTrace =
        ShrRateScoredCandidateTrace {
            selection: shr_rate_selection(46, 7, 121_399, 100),
            full_score_bits: 0x3fed_6266_6aac_5cb9,
            suffix_score_bits: [
                Some(0x3fef_60c1_4e91_ecd0),
                Some(0x3fee_a523_e62f_6d9a),
                Some(0x3fee_2e34_6699_f8d2),
                Some(0x3fed_7b7a_abd0_2540),
                Some(0x3fee_0076_66b6_d644),
                Some(0x3fe6_d4e0_cc19_e7c8),
            ],
            limiting_suffix_symbol: Some(9),
        };
    const EXPECTED_HIGH_LONG_DECODER_TRACE: DecoderEventTrace = DecoderEventTrace {
        first_source_sample_index: 121_423,
        last_source_sample_index: 138_533,
        recovered_chips: 8_555,
        preamble_entries: 45,
        preamble_resets: 43,
        sfd7_entries: 2,
        sfd_resets: 2,
        sfd10_entries: 0,
        header_entries: 0,
        last_sfd7: Some(DecoderDecisionPoint {
            point: DecoderTracePoint {
                source_sample_index: 138_439,
                event_chip_ordinal: 8_508,
            },
            observed_symbol: Some(7),
            best_symbol: 7,
            best_score_bits: 0x3fe9_86b8_0135_e0f5,
            runner_up_score_bits: 0x3fd4_a36a_70ef_e54d,
        }),
        last_sfd10: None,
        phr_low_decisions: 0,
        phr_high_decisions: 0,
        last_phr_low: None,
        last_phr_high: None,
        header_classification_resets: 0,
        invalid_phr_lengths: 0,
        decode_entries: 0,
        last_decode_entry: None,
        payload_symbols: 0,
        max_completed_payload_bytes: 0,
        payload_classification_resets: 0,
        completed_candidates: 0,
        last_completion: None,
        terminal: DecoderTerminal::Search { chip_count: 15 },
    };

    assert_eq!(
        EXPECTED_TIMING_RATE_GAIN_SWEEP_COUNTS[0][0][0][1],
        CandidateCounts::BOTH_TRANSMITTED,
        "the frozen gain-0.015 low-cell witness changed"
    );
    assert_eq!(
        GAIN_SWEEP_TIMING_ARMS[1].name,
        "adaptive-hard-gain-mu-0p015"
    );
    assert!(matches!(
        GAIN_SWEEP_TIMING_ARMS[1].mode,
        TimingClockMode::GainSweep(1)
    ));
    assert_eq!(TIMING_RATE_SCREEN_CASES[2], (8, 100));
    assert_eq!(RATE_SCREEN_TIMING_ARMS[6].name, "fixed-rate-plus-100-mu-4");
    assert_eq!(
        RATE_SCREEN_TIMING_ARMS[6].mu_sixteenths,
        TIMING_FIXED_CLOCK_MU_SIXTEENTHS[0]
    );
    assert!(matches!(
        RATE_SCREEN_TIMING_ARMS[6].mode,
        TimingClockMode::FixedRate(100)
    ));
    assert_eq!(
        EXPECTED_TIMING_RATE_ENDPOINT_SCREEN_COUNTS[2][6],
        CandidateCounts::BOTH_TRANSMITTED,
        "the frozen fixed +100-ppm mu-4/16 high-cell witness changed"
    );
    assert_eq!(
        EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[0][1][1].counts,
        CandidateCounts::BOTH_TRANSMITTED,
        "the frozen center joint-pass control changed"
    );

    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let seed = TIMING_DEVELOPMENT_SEEDS[0];
    let mut traces = Vec::with_capacity(THREE_CELL_CASES.len());

    for (case_index, (phase_index, rate_index, offset, rate_error_ppm)) in
        THREE_CELL_CASES.into_iter().enumerate()
    {
        assert_eq!(TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index], offset);
        assert_eq!(TIMING_RATE_ERROR_PPM[rate_index], rate_error_ppm);
        let samples = fractional_timing_rate_awgn_samples(
            &short_waveform,
            &longer_waveform,
            offset,
            rate_error_ppm,
            seed,
        );
        assert_eq!(
            sha256_cf32_le(&samples),
            EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256[0][phase_index][rate_index],
            "three-cell input digest drifted at case {case_index}"
        );
        let events = detect_causal_bursts(&samples);
        assert_eq!(
            events.as_slice(),
            &EXPECTED_THREE_CELL_EVENTS[case_index],
            "three-cell detector bounds drifted at case {case_index}"
        );
        assert!(events.iter().all(|event| !event.forced_end));
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].end_exclusive <= pair[1].replay_start)
        );

        let name =
            format!("joint-shr-rate-trace-seed-0-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm");
        let observation = run_shr_rate_trace_burst_streaming_case(&name, 17, &samples);
        let stats = observation.stats.stats;
        let mut identity_ticks = [None; 2];
        for (position, payload) in &observation.candidates {
            if let Some(identity_index) =
                transmitted.iter().position(|expected| expected == payload)
            {
                let event = events[identity_index];
                let first_tick = event.replay_start / 2 + 1;
                let last_tick = (event.end_exclusive - 1) / 2 + 1;
                assert!(
                    (first_tick..=last_tick).contains(position),
                    "case {case_index} identity {identity_index} tick {position} is outside {event:?}"
                );
                identity_ticks[identity_index].get_or_insert(*position);
            }
        }
        let payloads = observation
            .candidates
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect::<Vec<_>>();
        let counts = count_candidates(&payloads, &transmitted);
        let frozen = FrozenJointShrRateObservation {
            counts,
            markers: stats.burst.markers,
            shr_locks: stats.burst.shr_locks,
            shr_rejections: stats.burst.shr_rejections,
            shr_rate_selections: stats.shr_rate_selections,
            max_shr_metric_replay: stats.burst.max_shr_metric_replay,
            identity_ticks,
        };
        assert_eq!(
            frozen, EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[0][phase_index][rate_index],
            "frozen joint result drifted at case {case_index}"
        );
        assert!(counts.has_no_anomalies());
        assert_eq!(stats.burst.source_samples, samples.len() as u64);
        assert_eq!(stats.burst.starts, 2);
        assert_eq!(stats.burst.ends, 2);
        assert_eq!(stats.burst.markers, stats.burst.shr_locks);
        assert_eq!(stats.burst.shr_locks + stats.burst.shr_rejections, 2);
        assert_eq!(stats.burst.max_pretrigger, 64);
        assert_eq!(stats.burst.max_replay, 64);
        assert_eq!(stats.burst.max_metrics, 4);
        assert_eq!(stats.burst.shr_selections, [None; 2]);
        assert_eq!(stats.burst.shr_selection_overflow, 0);
        assert_eq!(stats.burst.max_shr_selection_metrics, 704);
        assert_eq!(
            stats.shr_rate_selections.iter().flatten().count() as u64,
            stats.burst.shr_locks
        );
        let max_metric_replay = stats
            .shr_rate_selections
            .iter()
            .flatten()
            .map(|selection| 704 - usize::from(selection.timing.start_offset))
            .max()
            .unwrap_or_default();
        assert_eq!(stats.burst.max_shr_metric_replay, max_metric_replay);

        let mut selected_events = [false; 2];
        for selection in stats.shr_rate_selections.iter().flatten() {
            assert!(selection.timing.start_offset < 64);
            assert!(selection.timing.mu_sixteenths < 16);
            assert!([-100, 0, 100].contains(&selection.rate_error_ppm));
            let selected_replay_start = selection
                .timing
                .source_sample_index
                .checked_sub(u64::from(selection.timing.start_offset))
                .expect("joint selection must not precede source sample zero");
            let matching_events = events
                .iter()
                .enumerate()
                .filter(|(_, event)| event.replay_start == selected_replay_start)
                .map(|(event_index, _)| event_index)
                .collect::<Vec<_>>();
            assert_eq!(
                matching_events.len(),
                1,
                "case {case_index} selection {selection:?} lost unique event provenance"
            );
            let event_index = matching_events[0];
            assert!(!selected_events[event_index]);
            selected_events[event_index] = true;
        }

        traces.push(ThreeCellDiagnosticTrace {
            phase_sixteenths: offset,
            input_rate_error_ppm: rate_error_ppm,
            burst: observation.stats.burst_trace,
            decoder: observation.stats.decoder_trace,
        });
    }

    let traces: [ThreeCellDiagnosticTrace; 3] = match traces.try_into() {
        Ok(traces) => traces,
        Err(_) => panic!("the diagnostic trace must retain exactly three ordered cases"),
    };
    let compact = serialize_three_cell_trace(&traces);
    let compact_sha256 = sha256_hex(compact.as_bytes());
    eprintln!("joint SHR-rate three-cell trace={compact}");
    eprintln!("joint SHR-rate three-cell trace sha256={compact_sha256}");
    assert_eq!(compact_sha256, EXPECTED_TRACE_SHA256);

    for (case_index, trace) in traces.iter().enumerate() {
        let (phase_index, rate_index, _, _) = THREE_CELL_CASES[case_index];
        let expected = EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[0][phase_index][rate_index];
        assert_eq!(trace.burst.overflow, 0);
        assert_eq!(trace.decoder.overflow, 0);
        let burst_events = trace
            .burst
            .events
            .iter()
            .map(|event| {
                event
                    .as_ref()
                    .expect("each case must retain two burst events")
            })
            .collect::<Vec<_>>();
        let decoder_events = trace.decoder.events.iter().flatten().collect::<Vec<_>>();
        assert_eq!(burst_events.len(), 2);
        assert_eq!(
            decoder_events.len() as u64,
            expected.shr_locks,
            "case {case_index} decoder records must equal locks"
        );
        assert_eq!(
            decoder_events
                .iter()
                .map(|event| event.completed_candidates)
                .sum::<u64>(),
            u64::from(expected.counts.valid_recoveries()),
            "case {case_index} trace completions must equal archived candidates"
        );

        for (event_index, event) in burst_events.iter().enumerate() {
            assert_eq!(
                event.event,
                EXPECTED_THREE_CELL_EVENTS[case_index][event_index]
            );
            let selector = event
                .selector
                .as_ref()
                .expect("each bounded event must publish its selector audit");
            assert_eq!(selector.evaluated_hypotheses, 3_072);
            assert!(selector.best_unconstrained.is_some());
            match expected.shr_rate_selections[event_index] {
                Some(selection) => {
                    assert_eq!(event.outcome, ShrRateTraceOutcome::Locked(selection));
                    assert_eq!(
                        selector
                            .best_eligible
                            .as_ref()
                            .expect("a lock must retain its eligible winner")
                            .selection,
                        selection
                    );
                    let clock = event
                        .clock
                        .as_ref()
                        .expect("a locked event must retain a clock trajectory");
                    assert_eq!(clock.markers, 1);
                    assert_eq!(clock.advance_overflow, 0);
                    assert_eq!(clock.boundary_overflow, 0);
                    assert_eq!(
                        clock.first_source_sample_index,
                        Some(selection.timing.source_sample_index)
                    );
                    assert!(
                        clock
                            .last_source_sample_index
                            .is_some_and(|last| last < event.event.end_exclusive)
                    );
                    assert_eq!(
                        clock.advance_histogram.iter().sum::<u64>(),
                        clock.emitted_chips
                    );
                    assert_eq!(clock.boundaries.len() as u64, clock.emitted_chips / 32);
                    for (boundary_index, boundary) in clock.boundaries.iter().enumerate() {
                        assert_eq!(boundary.chip_ordinal, (boundary_index as u64 + 1) * 32);
                    }
                    assert_eq!(
                        decoder_events
                            .iter()
                            .filter(|decoder| {
                                decoder.first_source_sample_index
                                    == selection.timing.source_sample_index
                            })
                            .count(),
                        1,
                        "case {case_index} event {event_index} lost coordinate-associated decoder provenance"
                    );
                }
                None => {
                    assert!(event.clock.is_none());
                }
            }
        }
        for decoder in &decoder_events {
            assert_eq!(
                burst_events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event.outcome,
                            ShrRateTraceOutcome::Locked(selection)
                                if selection.timing.source_sample_index
                                    == decoder.first_source_sample_index
                        )
                    })
                    .count(),
                1,
                "case {case_index} decoder record lost unique locked-event provenance"
            );
        }
    }

    let low_long_event = traces[0].burst.events[1]
        .as_ref()
        .expect("low cell must retain its rejected long event");
    assert_eq!(
        low_long_event.outcome,
        ShrRateTraceOutcome::RejectedNoEligible
    );
    assert!(low_long_event.clock.is_none());
    let low_long_selector = low_long_event
        .selector
        .as_ref()
        .expect("low long rejection must retain selector evidence");
    assert_eq!(low_long_selector.evaluated_hypotheses, 3_072);
    assert_eq!(low_long_selector.eligible_hypotheses, 0);
    assert_eq!(low_long_selector.best_eligible, None);
    assert_eq!(
        low_long_selector.best_unconstrained,
        Some(EXPECTED_LOW_LONG_BEST_UNCONSTRAINED)
    );
    let low_long_best = low_long_selector
        .best_unconstrained
        .expect("low long rejection must have an unconstrained winner");
    assert!(f64::from_bits(low_long_best.full_score_bits).is_finite());
    assert!(f64::from_bits(low_long_best.full_score_bits) > UNIQUE_ANGULAR_CAP);
    assert_eq!(
        low_long_best
            .suffix_score_bits
            .iter()
            .map(|score| f64::from_bits(score.expect("all six suffix scores must be finite")))
            .filter(|score| *score <= UNIQUE_ANGULAR_CAP)
            .count(),
        1,
        "exactly the limiting suffix must fail eligibility"
    );
    assert_eq!(low_long_best.limiting_suffix_symbol, Some(9));
    assert!(traces[0].decoder.events[1].is_none());

    const COMPLETIONS: [(usize, DecodeEntry, u64, DecoderTerminal); 4] = [
        (
            0,
            DecodeEntry {
                length: 5,
                point: DecoderTracePoint {
                    source_sample_index: 40_762,
                    event_chip_ordinal: 384,
                },
            },
            41_402,
            DecoderTerminal::Search { chip_count: 10 },
        ),
        (
            1,
            DecodeEntry {
                length: 5,
                point: DecoderTracePoint {
                    source_sample_index: 40_767,
                    event_chip_ordinal: 384,
                },
            },
            41_407,
            DecoderTerminal::Search { chip_count: 12 },
        ),
        (
            1,
            DecodeEntry {
                length: 127,
                point: DecoderTracePoint {
                    source_sample_index: 122_177,
                    event_chip_ordinal: 384,
                },
            },
            138_433,
            DecoderTerminal::Search { chip_count: 11 },
        ),
        (
            2,
            DecodeEntry {
                length: 5,
                point: DecoderTracePoint {
                    source_sample_index: 40_771,
                    event_chip_ordinal: 384,
                },
            },
            41_412,
            DecoderTerminal::Search { chip_count: 13 },
        ),
    ];
    for (case_index, entry, completion_source, terminal) in COMPLETIONS {
        let decoder = traces[case_index]
            .decoder
            .events
            .iter()
            .flatten()
            .find(|event| event.last_decode_entry == Some(entry))
            .expect("frozen completion must retain its exact PHR decode entry");
        assert_eq!(decoder.decode_entries, 1);
        assert_eq!(decoder.payload_symbols, u64::from(entry.length) * 2);
        assert_eq!(decoder.max_completed_payload_bytes, entry.length);
        assert_eq!(decoder.payload_classification_resets, 0);
        assert_eq!(decoder.completed_candidates, 1);
        assert_eq!(
            decoder.last_completion,
            Some(DecoderTracePoint {
                source_sample_index: completion_source,
                event_chip_ordinal: if entry.length == 5 { 704 } else { 8_512 },
            })
        );
        assert_eq!(decoder.terminal, terminal);
    }
    assert_eq!(traces[0].decoder.events.iter().flatten().count(), 1);
    assert_eq!(traces[1].decoder.events.iter().flatten().count(), 2);
    assert_eq!(traces[2].decoder.events.iter().flatten().count(), 2);

    let high_long_selection = EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[0][2][2]
        .shr_rate_selections[1]
        .expect("the high long event must retain its frozen lock");
    assert_eq!(high_long_selection.rate_error_ppm, -100);
    assert_eq!(traces[2].input_rate_error_ppm, 100);
    assert_ne!(
        high_long_selection.rate_error_ppm, traces[2].input_rate_error_ppm,
        "selected rate is diagnostic, not an injected-rate calibration oracle"
    );
    assert_eq!(
        traces[2].decoder.events[1],
        Some(EXPECTED_HIGH_LONG_DECODER_TRACE)
    );
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.sfd7_entries, 2);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.sfd_resets, 2);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.sfd10_entries, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.header_entries, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.phr_low_decisions, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.phr_high_decisions, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.decode_entries, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.completed_candidates, 0);
    assert_eq!(EXPECTED_HIGH_LONG_DECODER_TRACE.last_completion, None);
}

#[test]
fn full_cf32_deferred_shr_rate_three_cell_qualification_gate() {
    const EXPECTED_TICKS: [[u64; 2]; 3] = [[20_702, 69_211], [20_704, 69_217], [20_707, 69_224]];
    // Selected rate hypotheses are deterministic selector diagnostics for
    // these frozen inputs, not oscillator-calibration measurements.
    const EXPECTED_HANDOFFS: [[DeferredShrRateHandoff; 2]; 3] = [
        [
            deferred_handoff(
                shr_rate_selection(45, 7, 39_997, 100),
                5,
                813,
                40_765,
                0x3f03_a92a,
                0xbf19_1e2f,
            ),
            deferred_handoff(
                shr_rate_selection(46, 7, 121_399, 100),
                127,
                814,
                122_167,
                0x3f03_a92a,
                0xbf23_205c,
            ),
        ],
        [
            deferred_handoff(
                shr_rate_selection(45, 8, 40_001, 0),
                5,
                813,
                40_769,
                0x3f00_0000,
                0xbf31_51ea,
            ),
            deferred_handoff(
                shr_rate_selection(44, 8, 121_411, 0),
                127,
                812,
                122_179,
                0x3f00_0000,
                0xbf3f_53bd,
            ),
        ],
        [
            deferred_handoff(
                shr_rate_selection(43, 6, 40_006, 100),
                5,
                811,
                40_774,
                0x3ee7_5254,
                0xbf4b_0dc3,
            ),
            deferred_handoff(
                shr_rate_selection(44, 10, 121_423, -100),
                127,
                812,
                122_191,
                0x3f0c_56d6,
                0xbf1c_8b7a,
            ),
        ],
    ];

    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_AWGN_SAMPLE_SNR_DB, 6.0);
    assert_eq!(TIMING_DEVELOPMENT_SEEDS[0], 0x2437_0017_4652_4143);
    assert_eq!(
        THREE_CELL_CASES.map(|(_, _, offset, _)| offset),
        TIMING_RATE_DEVELOPMENT_OFFSETS
    );
    assert_eq!(
        THREE_CELL_CASES.map(|(_, _, _, rate_error_ppm)| rate_error_ppm),
        TIMING_RATE_ERROR_PPM
    );
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let seed = TIMING_DEVELOPMENT_SEEDS[0];
    let mut prepared = Vec::with_capacity(THREE_CELL_CASES.len());

    // No deferred result is produced until every already-opened input and
    // prior joint result has passed its frozen gate.
    for (case_index, (phase_index, rate_index, offset, rate_error_ppm)) in
        THREE_CELL_CASES.into_iter().enumerate()
    {
        assert_eq!(TIMING_RATE_DEVELOPMENT_OFFSETS[phase_index], offset);
        assert_eq!(TIMING_RATE_ERROR_PPM[rate_index], rate_error_ppm);
        let samples = fractional_timing_rate_awgn_samples(
            &short_waveform,
            &longer_waveform,
            offset,
            rate_error_ppm,
            seed,
        );
        assert_eq!(
            sha256_cf32_le(&samples),
            EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256[0][phase_index][rate_index],
            "three-cell input digest drifted at case {case_index}"
        );
        assert_eq!(
            detect_causal_bursts(&samples).as_slice(),
            &EXPECTED_THREE_CELL_EVENTS[case_index],
            "three-cell detector bounds drifted at case {case_index}"
        );
        let prior = run_joint_shr_rate_candidate_case(
            &format!(
                "deferred-qualification-prior-joint-seed-0-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
            ),
            17,
            &samples,
            &transmitted,
        );
        assert!(prior.counts.has_no_anomalies());
        assert_eq!(prior.input_samples, samples.len() as u64);
        assert_eq!(
            prior.frozen(),
            EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[0][phase_index][rate_index],
            "prior joint candidate oracle drifted at case {case_index}"
        );
        prepared.push((case_index, offset, rate_error_ppm, samples));
    }

    let mut qualified_cases = 0;
    for (case_index, offset, rate_error_ppm, samples) in prepared {
        let observation = run_deferred_shr_rate_burst_streaming_case(
            &format!(
                "deferred-shr-rate-qualification-seed-0-{offset:+}-sixteenths-{rate_error_ppm:+}-ppm"
            ),
            17,
            &samples,
        );
        let payloads = observation
            .candidates
            .iter()
            .map(|(_, payload)| payload.clone())
            .collect::<Vec<_>>();
        let counts = count_candidates(&payloads, &transmitted);
        assert_eq!(counts, CandidateCounts::BOTH_TRANSMITTED);
        assert!(counts.has_no_anomalies());
        assert_eq!(payloads, transmitted);
        assert_eq!(observation.candidates.len(), 2);
        assert_eq!(
            [observation.candidates[0].0, observation.candidates[1].0],
            EXPECTED_TICKS[case_index]
        );

        let stats = observation.stats;
        assert_eq!(stats.burst.source_samples, samples.len() as u64);
        assert_eq!(stats.burst.starts, 2);
        assert_eq!(stats.burst.ends, 2);
        assert_eq!(stats.burst.markers, 2);
        assert_eq!(stats.burst.shr_locks, 2);
        assert_eq!(stats.burst.shr_rejections, 0);
        assert_eq!(stats.burst.shr_selections, [None; 2]);
        assert_eq!(stats.burst.shr_selection_overflow, 0);
        assert_eq!(stats.burst.max_pretrigger, 64);
        assert_eq!(stats.burst.max_replay, 64);
        assert_eq!(stats.burst.max_metrics, 4);
        assert_eq!(stats.burst.max_shr_selection_metrics, 832);
        assert!(stats.burst.max_shr_metric_replay <= 65);
        assert_eq!(
            stats.deferred_selections,
            EXPECTED_HANDOFFS[case_index].map(Some)
        );

        let handoffs = stats
            .deferred_selections
            .map(|handoff| handoff.expect("each qualified burst must commit one handoff"));
        assert_eq!(handoffs.map(|handoff| handoff.phr_length), [5, 127]);
        assert!(
            handoffs
                .iter()
                .all(|handoff| (767..=832).contains(&usize::from(handoff.payload_left_offset)))
        );
        assert_eq!(
            stats.burst.max_shr_metric_replay,
            handoffs
                .iter()
                .map(|handoff| 832 - usize::from(handoff.payload_left_offset))
                .max()
                .unwrap()
        );
        for (event_index, handoff) in handoffs.iter().enumerate() {
            let event = EXPECTED_THREE_CELL_EVENTS[case_index][event_index];
            assert!(handoff.selection.timing.start_offset < 64);
            assert!(handoff.selection.timing.mu_sixteenths < 16);
            assert!([-100, 0, 100].contains(&handoff.selection.rate_error_ppm));
            assert_eq!(
                handoff.selection.timing.source_sample_index,
                event.replay_start + u64::from(handoff.selection.timing.start_offset)
            );
            assert_eq!(
                handoff.payload_source_sample_index,
                event.replay_start + u64::from(handoff.payload_left_offset)
            );
            assert!(handoff.payload_source_sample_index < event.end_exclusive);
            assert_eq!(
                EXPECTED_THREE_CELL_EVENTS[case_index]
                    .iter()
                    .filter(|candidate| {
                        candidate.replay_start
                            == handoff.selection.timing.source_sample_index
                                - u64::from(handoff.selection.timing.start_offset)
                    })
                    .count(),
                1,
                "case {case_index} handoff {event_index} lost unique event provenance"
            );

            let (completion_tick, payload) = &observation.candidates[event_index];
            assert_eq!(payload, &transmitted[event_index]);
            assert_eq!(payload.len(), usize::from(handoff.phr_length));
            let first_tick = event.replay_start / 2 + 1;
            let last_tick = (event.end_exclusive - 1) / 2 + 1;
            assert!((first_tick..=last_tick).contains(completion_tick));
            assert!(
                *completion_tick > handoff.payload_source_sample_index / 2,
                "case {case_index} completion {event_index} preceded its payload handoff"
            );
        }
        qualified_cases += 1;
    }
    assert_eq!(qualified_cases, THREE_CELL_CASES.len());
}

#[test]
fn full_cf32_deferred_shr_rate_development_grid_qualifies() {
    const EXPECTED_SERIALIZED_SHA256: &str =
        "ac45e16335dc78fff55e648264d60d81d78bfd9cbd416be0545eca9ebcf5854d";

    struct PreparedCell {
        seed_index: usize,
        seed: u64,
        phase_index: usize,
        rate_index: usize,
        phase_sixteenths: i32,
        input_rate_error_ppm: i32,
        input_sha256: String,
        events: [DetectedBurst; 2],
        samples: Vec<Complex32>,
    }

    assert_eq!(
        TIMING_DEVELOPMENT_SEEDS,
        [0x2437_0017_4652_4143, 0x2437_0017_5449_4d45]
    );
    assert_eq!(TIMING_RATE_DEVELOPMENT_OFFSETS, [-4, 0, 8]);
    assert_eq!(TIMING_RATE_ERROR_PPM, [-100, 0, 100]);
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(TIMING_AWGN_SAMPLE_SNR_DB, 6.0);

    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let cell_count = TIMING_DEVELOPMENT_SEEDS.len()
        * TIMING_RATE_DEVELOPMENT_OFFSETS.len()
        * TIMING_RATE_ERROR_PPM.len();
    assert_eq!(cell_count, 18);
    let mut prepared = Vec::with_capacity(cell_count);

    for (seed_index, seed) in TIMING_DEVELOPMENT_SEEDS.into_iter().enumerate() {
        for (phase_index, phase_sixteenths) in
            TIMING_RATE_DEVELOPMENT_OFFSETS.into_iter().enumerate()
        {
            for (rate_index, input_rate_error_ppm) in TIMING_RATE_ERROR_PPM.into_iter().enumerate()
            {
                let case_index = prepared.len();
                assert_eq!(case_index, seed_index * 9 + phase_index * 3 + rate_index);
                let samples = fractional_timing_rate_awgn_samples(
                    &short_waveform,
                    &longer_waveform,
                    phase_sixteenths,
                    input_rate_error_ppm,
                    seed,
                );
                let input_sha256 = sha256_cf32_le(&samples);
                assert_eq!(
                    input_sha256,
                    EXPECTED_TIMING_RATE_6DB_DEVELOPMENT_SHA256[seed_index][phase_index]
                        [rate_index],
                    "development input digest drifted at case {case_index}"
                );
                let events = detect_causal_bursts(&samples);
                assert_eq!(events.len(), 2);
                assert!(events.iter().all(|event| !event.forced_end));
                assert!(events[0].end_exclusive <= events[1].replay_start);
                let events: [DetectedBurst; 2] = events
                    .try_into()
                    .expect("each development cell must retain exactly two events");
                let prior = run_joint_shr_rate_candidate_case(
                    &format!(
                        "deferred-development-prior-joint-seed-{seed_index}-phase-{phase_sixteenths:+}-rate-{input_rate_error_ppm:+}"
                    ),
                    17,
                    &samples,
                    &transmitted,
                );
                assert!(prior.counts.has_no_anomalies());
                assert_eq!(prior.input_samples, samples.len() as u64);
                assert_eq!(
                    prior.frozen(),
                    EXPECTED_TIMING_RATE_JOINT_SHR_OBSERVATIONS[seed_index][phase_index]
                        [rate_index],
                    "prior joint candidate oracle drifted at case {case_index}"
                );
                prepared.push(PreparedCell {
                    seed_index,
                    seed,
                    phase_index,
                    rate_index,
                    phase_sixteenths,
                    input_rate_error_ppm,
                    input_sha256,
                    events,
                    samples,
                });
            }
        }
    }
    assert_eq!(prepared.len(), cell_count);

    let mut qualified = Vec::with_capacity(cell_count);
    for cell in prepared {
        let case_index = qualified.len();
        let deferred = run_deferred_shr_rate_candidate_case(
            &format!(
                "deferred-development-seed-{}-phase-{:+}-rate-{:+}",
                cell.seed_index, cell.phase_sixteenths, cell.input_rate_error_ppm
            ),
            17,
            &cell.samples,
            &transmitted,
            &cell.events,
        );
        assert_eq!(deferred.counts, CandidateCounts::BOTH_TRANSMITTED);
        assert!(deferred.counts.has_no_anomalies());
        assert_eq!(deferred.counts.valid_recoveries(), 2);
        assert_eq!(deferred.identity_ticks.iter().flatten().count(), 2);
        assert_eq!(deferred.candidate_lengths, [5, 127]);
        assert_eq!(deferred.input_samples, cell.samples.len() as u64);

        let burst = deferred.stats.burst;
        assert_eq!(burst.source_samples, cell.samples.len() as u64);
        assert_eq!(burst.starts, 2);
        assert_eq!(burst.ends, 2);
        assert_eq!(burst.markers, 2);
        assert_eq!(burst.shr_locks, 2);
        assert_eq!(burst.shr_rejections, 0);
        assert_eq!(burst.shr_selections, [None; 2]);
        assert_eq!(burst.shr_selection_overflow, 0);
        assert_eq!(burst.max_pretrigger, 64);
        assert_eq!(burst.max_replay, 64);
        assert_eq!(burst.max_metrics, 4);
        assert_eq!(burst.max_shr_selection_metrics, 832);
        assert!(burst.max_shr_metric_replay <= 65);

        let handoffs = deferred
            .stats
            .deferred_selections
            .map(|handoff| handoff.expect("qualified event must retain its exact handoff"));
        assert_eq!(handoffs.map(|handoff| handoff.phr_length), [5, 127]);
        assert!(
            handoffs
                .iter()
                .all(|handoff| (767..=832).contains(&usize::from(handoff.payload_left_offset)))
        );
        assert_eq!(
            burst.max_shr_metric_replay,
            handoffs
                .iter()
                .map(|handoff| 832 - usize::from(handoff.payload_left_offset))
                .max()
                .unwrap()
        );
        for (event_index, handoff) in handoffs.iter().enumerate() {
            let event = cell.events[event_index];
            assert!(handoff.selection.timing.start_offset < 64);
            assert!(handoff.selection.timing.mu_sixteenths < 16);
            assert!(TIMING_RATE_ERROR_PPM.contains(&handoff.selection.rate_error_ppm));
            assert_eq!(
                handoff.selection.timing.source_sample_index,
                event.replay_start + u64::from(handoff.selection.timing.start_offset)
            );
            assert_eq!(
                handoff.payload_source_sample_index,
                event.replay_start + u64::from(handoff.payload_left_offset)
            );
            assert!(handoff.payload_source_sample_index < event.end_exclusive);
            let completion_tick = deferred.identity_ticks[event_index]
                .expect("qualified identity must retain its exact completion tick");
            assert!(completion_tick > handoff.payload_source_sample_index / 2);
            assert_eq!(
                usize::from(handoff.phr_length),
                transmitted[event_index].len()
            );
        }

        qualified.push(DeferredDevelopmentObservation {
            seed_index: cell.seed_index,
            seed: cell.seed,
            phase_index: cell.phase_index,
            rate_index: cell.rate_index,
            phase_sixteenths: cell.phase_sixteenths,
            input_rate_error_ppm: cell.input_rate_error_ppm,
            input_sha256: cell.input_sha256,
            events: cell.events,
            deferred,
        });
        assert_eq!(qualified.len(), case_index + 1);
    }
    assert_eq!(qualified.len(), cell_count);
    let serialized = serialize_deferred_development(&qualified);
    let serialized_sha256 = sha256_hex(serialized.as_bytes());
    assert_eq!(serialized_sha256, EXPECTED_SERIALIZED_SHA256);
    eprintln!("deferred SHR-rate development={serialized}");
    eprintln!("deferred SHR-rate development sha256={serialized_sha256}");
}

#[test]
fn full_cf32_clean_negative_half_phase_detector_gate_recovers_two_events() {
    const EXPECTED_ACTIVE_BOUNDS: [(u64, u64); 2] = [(39_985, 41_425), (121_395, 138_451)];

    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(
        (
            POWER_WINDOW_SAMPLES,
            TRIGGER_PERSISTENCE_SAMPLES,
            RELEASE_PERSISTENCE_SAMPLES,
            PRETRIGGER_SAMPLES,
            NOISE_WARMUP_SAMPLES,
            MAX_BURST_SAMPLES,
        ),
        (32, 4, 64, 64, 4_096, 20_000)
    );
    assert_eq!(
        EXPECTED_TIMING_CLEAN_DEVELOPMENT_COUNTS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        [CandidateCounts::BOTH_TRANSMITTED; 2]
    );

    let short_waveform = upstream_waveform(&ACK_PSDU);
    let short_samples = short_waveform.len();
    let longer_waveform = upstream_waveform(&longer_valid_psdu());
    let samples = fractional_timing_samples(
        &short_waveform,
        &longer_waveform,
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS,
    );
    assert_eq!(sha256_cf32_le(&samples), CLEAN_NEGATIVE_HALF_PHASE_SHA256);

    let segment_bounds = [(0, short_samples), (short_samples, samples.len())];
    let active_bounds = segment_bounds.map(|(base, end)| {
        let segment = &samples[base..end];
        let first = segment
            .iter()
            .position(|sample| sample.re != 0.0 || sample.im != 0.0)
            .expect("segment retains a first active sample");
        let last = segment
            .iter()
            .rposition(|sample| sample.re != 0.0 || sample.im != 0.0)
            .expect("segment retains a last active sample");
        assert!(
            segment[..first]
                .iter()
                .all(|sample| sample.re == 0.0 && sample.im == 0.0)
        );
        assert!(
            segment[last + 1..]
                .iter()
                .all(|sample| sample.re == 0.0 && sample.im == 0.0)
        );
        ((base + first) as u64, (base + last) as u64)
    });
    assert_eq!(active_bounds, EXPECTED_ACTIVE_BOUNDS);
    let expected_events = active_bounds.map(|(first, last)| {
        let detected_at = first + TRIGGER_PERSISTENCE_SAMPLES as u64 - 1;
        let replay_start = (detected_at + 1)
            .checked_sub(PRETRIGGER_SAMPLES)
            .expect("active sample follows the detector warmup");
        let end_exclusive = last + POWER_WINDOW_SAMPLES as u64 + RELEASE_PERSISTENCE_SAMPLES as u64;
        let event = DetectedBurst {
            replay_start,
            detected_at,
            end_exclusive,
            forced_end: false,
        };
        assert!(event.end_exclusive - event.replay_start < MAX_BURST_SAMPLES);
        event
    });
    assert_eq!(expected_events, CLEAN_NEGATIVE_HALF_PHASE_EVENTS);
    assert!(expected_events[0].end_exclusive <= expected_events[1].replay_start);
    assert!(expected_events[0].end_exclusive <= short_samples as u64);
    assert!(expected_events[1].replay_start >= short_samples as u64);
    assert!(expected_events[1].end_exclusive <= samples.len() as u64);

    let mut detector = CausalBurstDetector::new();
    let mut warmup_completion: Option<DetectorDiagnosticSnapshot> = None;
    let mut starts: [Option<BurstStart>; 2] = [None; 2];
    let mut zero_active: [Option<DetectorDiagnosticSnapshot>; 2] = [None; 2];
    let mut ends: [Option<DetectedBurst>; 2] = [None; 2];
    let mut end_snapshots: [Option<DetectorDiagnosticSnapshot>; 2] = [None; 2];
    let mut second_first_nonzero_idle: Option<DetectorDiagnosticSnapshot> = None;
    let mut start_count = 0usize;
    let mut end_count = 0usize;
    let mut forced_ends = 0usize;
    let mut inhibited_samples = 0u64;

    // This frozen -8/16 gate characterizes detector segmentation only. It is
    // not timing or power calibration and invokes no candidate or decoder.
    for (sample_index, sample) in samples.iter().enumerate() {
        let update = detector
            .push_with_transition(*sample)
            .expect("frozen detector input is finite");
        let snapshot = detector.diagnostic_snapshot();
        inhibited_samples += u64::from(snapshot.mode == DetectorDiagnosticMode::Inhibited);
        if snapshot.samples_seen == NOISE_WARMUP_SAMPLES {
            assert!(warmup_completion.replace(snapshot).is_none());
        }
        if snapshot.samples_seen == active_bounds[1].0 + 1 {
            assert_eq!(sample_index as u64, active_bounds[1].0);
            assert!(sample.re != 0.0 || sample.im != 0.0);
            assert!(second_first_nonzero_idle.replace(snapshot).is_none());
        }
        if let DetectorDiagnosticMode::Active { .. } = snapshot.mode
            && snapshot.window_nonzero_count == 0
        {
            let event_index = start_count
                .checked_sub(1)
                .expect("an active zero window follows a detector start");
            if zero_active[event_index].is_none() {
                zero_active[event_index] = Some(snapshot);
            }
        }
        if let Some(started) = update.started {
            assert!(start_count < starts.len());
            assert!(starts[start_count].replace(started).is_none());
            start_count += 1;
        }
        if let Some(ended) = update.ended {
            assert!(end_count < ends.len());
            forced_ends += usize::from(ended.forced_end);
            assert!(ends[end_count].replace(ended).is_none());
            assert!(end_snapshots[end_count].replace(snapshot).is_none());
            end_count += 1;
        }
    }
    let prefinish = detector.diagnostic_snapshot();
    let finish_event = detector.finish();
    let finished = detector.diagnostic_snapshot();

    assert_eq!(start_count, starts.len());
    assert_eq!(end_count, ends.len());
    assert_eq!(forced_ends, 0);
    assert_eq!(inhibited_samples, 0);
    assert_eq!(
        starts,
        expected_events.map(|event| Some(BurstStart {
            replay_start: event.replay_start,
            detected_at: event.detected_at,
        }))
    );
    assert_eq!(ends, expected_events.map(Some));

    let zero_bits = 0.0f64.to_bits();
    let warmup_completion = warmup_completion.expect("retain warmup completion");
    assert_eq!(warmup_completion.samples_seen, NOISE_WARMUP_SAMPLES);
    assert_eq!(warmup_completion.mode, DetectorDiagnosticMode::Idle);
    assert_eq!(warmup_completion.window_len, POWER_WINDOW_SAMPLES);
    assert_eq!(warmup_completion.window_nonzero_count, 0);
    assert_eq!(warmup_completion.stored_window_sum_bits, zero_bits);
    assert_eq!(warmup_completion.recomputed_window_sum_bits, zero_bits);
    assert_eq!(warmup_completion.mean_power_bits, zero_bits);
    assert_eq!(warmup_completion.noise_floor_bits, zero_bits);
    assert_eq!(warmup_completion.release_threshold_bits, zero_bits);

    for event_index in 0..expected_events.len() {
        let zero = zero_active[event_index].expect("retain first all-zero active window");
        assert_eq!(
            zero.mode,
            DetectorDiagnosticMode::Active {
                replay_start: expected_events[event_index].replay_start,
                detected_at: expected_events[event_index].detected_at,
            }
        );
        assert_eq!(
            zero.samples_seen,
            active_bounds[event_index].1 + POWER_WINDOW_SAMPLES as u64 + 1
        );
        assert_eq!(zero.window_len, POWER_WINDOW_SAMPLES);
        assert_eq!(zero.window_nonzero_count, 0);
        assert_eq!(zero.stored_window_sum_bits, zero_bits);
        assert_eq!(zero.recomputed_window_sum_bits, zero_bits);
        assert_eq!(zero.mean_power_bits, zero_bits);
        assert_eq!(zero.noise_floor_bits, zero_bits);
        assert_eq!(zero.release_threshold_bits, zero_bits);
        assert_eq!(zero.release_run, 1);

        let end = end_snapshots[event_index].expect("retain normal end snapshot");
        assert_eq!(end.samples_seen, expected_events[event_index].end_exclusive);
        assert_eq!(end.mode, DetectorDiagnosticMode::Idle);
        assert_eq!(end.release_run, 0);
        assert_eq!(
            end.samples_seen - zero.samples_seen,
            RELEASE_PERSISTENCE_SAMPLES as u64 - 1
        );
    }

    let second_first_nonzero_idle =
        second_first_nonzero_idle.expect("retain second waveform's first active sample");
    assert_eq!(
        second_first_nonzero_idle.samples_seen,
        active_bounds[1].0 + 1
    );
    assert_eq!(second_first_nonzero_idle.mode, DetectorDiagnosticMode::Idle);
    assert_eq!(second_first_nonzero_idle.window_nonzero_count, 1);
    assert!(f64::from_bits(second_first_nonzero_idle.recomputed_window_sum_bits) > 0.0);
    assert_eq!(
        second_first_nonzero_idle.stored_window_sum_bits,
        second_first_nonzero_idle.recomputed_window_sum_bits
    );
    assert_eq!(prefinish.samples_seen, samples.len() as u64);
    assert_eq!(prefinish.mode, DetectorDiagnosticMode::Idle);
    assert_eq!(finish_event, None);
    assert_eq!(finished.mode, DetectorDiagnosticMode::Finished);
}

#[test]
fn full_cf32_deferred_clean_zero_phase_selector_positive_control_is_audited() {
    deferred_selector::clean_zero_phase_selector_positive_control_case();
}

#[test]
fn full_cf32_deferred_clean_negative_half_phase_selector_rejection_is_audited() {
    deferred_selector::clean_negative_half_phase_selector_rejection_case();
}

#[test]
fn full_cf32_deferred_clean_negative_half_phase_late_starts_are_observed_after_rejection() {
    deferred_selector::clean_negative_half_phase_late_start_observation_case();
}

#[test]
fn full_cf32_deferred_score_counterfactual_audits_zero_then_negative_half_phase() {
    deferred_selector::score_counterfactual_zero_then_negative_half_phase_case();
}

#[test]
fn full_cf32_deferred_clean_negative_half_phase_emits_no_archive_candidates() {
    const EXPECTED_STATS: DeferredShrRateClockStats = DeferredShrRateClockStats {
        burst: BurstClockStats {
            source_samples: 178_436,
            starts: 2,
            ends: 2,
            markers: 0,
            max_pretrigger: 64,
            max_replay: 64,
            max_metrics: 0,
            shr_locks: 0,
            shr_rejections: 2,
            shr_selections: [None; 2],
            shr_selection_overflow: 0,
            max_shr_selection_metrics: 832,
            max_shr_metric_replay: 0,
        },
        deferred_selections: [None; 2],
    };

    assert_eq!(
        TIMING_DEVELOPMENT_OFFSETS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        CLEAN_NEGATIVE_HALF_PHASE_SIXTEENTHS
    );
    assert_eq!(TIMING_OFFSET_DENOMINATOR, 16);
    assert_eq!(
        EXPECTED_TIMING_CLEAN_DEVELOPMENT_COUNTS[CLEAN_NEGATIVE_HALF_PHASE_INDEX],
        [CandidateCounts::BOTH_TRANSMITTED; 2]
    );

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

    // This fixture classifies empty output relative to deferred handoff commit;
    // selected rate hypotheses are diagnostics, not clock or RF calibration.
    let observation = run_deferred_shr_rate_burst_streaming_case(
        "deferred-clean-negative-half-phase-diagnostic",
        17,
        &samples,
    );
    assert!(observation.candidates.is_empty());

    let burst = observation.stats.burst;
    assert_eq!(burst.source_samples, samples.len() as u64);
    assert_eq!(burst.starts, 2);
    assert_eq!(burst.ends, 2);
    assert_eq!(burst.markers, burst.shr_locks);
    assert_eq!(burst.shr_locks + burst.shr_rejections, 2);
    assert_eq!(burst.shr_selections, [None; 2]);
    assert_eq!(burst.shr_selection_overflow, 0);
    assert!(burst.max_pretrigger <= 64);
    assert!(burst.max_replay <= 64);
    assert!(burst.max_metrics <= 4);
    assert!(burst.max_shr_selection_metrics <= 832);
    assert!(burst.max_shr_metric_replay <= 65);

    let handoff_count = observation
        .stats
        .deferred_selections
        .iter()
        .flatten()
        .count() as u64;
    assert_eq!(handoff_count, burst.shr_locks);

    assert!(matches!(
        (burst.shr_locks, burst.shr_rejections),
        (0, 2) | (1, 1) | (2, 0)
    ));

    let mut matched_events = [false; 2];
    for handoff in observation.stats.deferred_selections.iter().flatten() {
        assert!(handoff.selection.timing.start_offset < 64);
        assert!(handoff.selection.timing.mu_sixteenths < 16);
        assert!(TIMING_RATE_ERROR_PPM.contains(&handoff.selection.rate_error_ppm));
        assert!((767..=832).contains(&usize::from(handoff.payload_left_offset)));
        let replay_start = handoff
            .selection
            .timing
            .source_sample_index
            .checked_sub(u64::from(handoff.selection.timing.start_offset))
            .expect("deferred selection must not precede source sample zero");
        let mut matching_events = CLEAN_NEGATIVE_HALF_PHASE_EVENTS
            .iter()
            .enumerate()
            .filter(|(_, event)| event.replay_start == replay_start);
        let (event_index, event) = matching_events
            .next()
            .expect("retained handoff must derive from a frozen detector event");
        assert!(matching_events.next().is_none());
        assert!(!matched_events[event_index]);
        matched_events[event_index] = true;
        assert_eq!(
            handoff.selection.timing.source_sample_index,
            event.replay_start + u64::from(handoff.selection.timing.start_offset)
        );
        assert_eq!(
            handoff.payload_source_sample_index,
            event.replay_start + u64::from(handoff.payload_left_offset)
        );
        assert!(handoff.payload_source_sample_index < event.end_exclusive);
        assert!(832 - usize::from(handoff.payload_left_offset) <= burst.max_shr_metric_replay);
    }
    assert_eq!(
        matched_events.iter().filter(|matched| **matched).count() as u64,
        handoff_count
    );
    if burst.shr_rejections == 0 {
        let expected_tail = observation
            .stats
            .deferred_selections
            .iter()
            .flatten()
            .map(|handoff| 832 - usize::from(handoff.payload_left_offset))
            .max()
            .expect("two committed handoffs retain replay tails");
        assert_eq!(burst.max_shr_metric_replay, expected_tail);
    }

    // This exact result is before handoff commit; it does not identify every
    // possible internal selector state without a selector trace.
    assert_eq!(observation.stats, EXPECTED_STATS);
}

#[test]
fn full_cf32_timing_burst_diagnostic_is_characterized() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let inputs = timing_burst_diagnostic_inputs(
        &upstream_waveform(&ACK_PSDU),
        &upstream_waveform(&longer_psdu),
    );
    let samples = inputs.samples();
    let labels = ["combined", "full-long", "cropped-long", "cropped-short"];
    let observations: [TimingCandidateObservations; 4] = std::array::from_fn(|case_index| {
        run_fixed_timing_candidate_case(
            &format!("timing-burst-diagnostic-{}", labels[case_index]),
            17,
            samples[case_index],
            &transmitted,
        )
    });
    eprintln!("timing burst diagnostic labels={labels:?} observations={observations:?}");
    assert_eq!(observations, EXPECTED_TIMING_BURST_DIAGNOSTIC_COUNTS);
    assert!(
        observations
            .iter()
            .flatten()
            .all(|counts| counts.has_no_anomalies())
    );
    assert_eq!(observations[0], [CandidateCounts::SHORT_ONLY; 4]);
    assert_eq!(observations[1], [CandidateCounts::NONE; 4]);
    assert!(
        observations[1..=2].iter().flatten().all(|counts| {
            counts.recovered_mask & CandidateCounts::SHORT_ONLY.recovered_mask == 0
        })
    );
    assert_eq!(observations[2][1], CandidateCounts::LONG_ONLY);
    assert_eq!(observations[3], [CandidateCounts::SHORT_ONLY; 4]);
    assert_eq!(observations[3][1], CandidateCounts::SHORT_ONLY);
}

#[test]
fn full_cf32_fractional_timing_bad_fcs_development_grid_is_frozen() {
    let mut bad_fcs_ack = ACK_PSDU;
    bad_fcs_ack[3] ^= 1;
    assert!(!has_valid_fcs(&bad_fcs_ack));
    let waveform = upstream_waveform(&bad_fcs_ack);
    let observations = TIMING_DEVELOPMENT_OFFSETS.map(|offset| {
        run_fixed_timing_candidate_case(
            &format!("fractional-timing-bad-fcs-{offset:+}-sixteenths"),
            17,
            &apply_fractional_timing(&waveform, offset),
            &[],
        )
    });
    assert_eq!(observations, EXPECTED_TIMING_BAD_FCS_FIXED_CANDIDATE_COUNTS);
    for (offset_index, observed) in observations.iter().enumerate() {
        assert_eq!(
            [observed[0], observed[1]],
            EXPECTED_TIMING_BAD_FCS_DEVELOPMENT_COUNTS[offset_index]
        );
        for counts in observed {
            assert_eq!(counts.recovered_mask, 0);
            assert_eq!(counts.unexpected_valid_fcs, 0);
            assert_eq!(counts.duplicate_transmitted, 0);
            assert!(counts.invalid_fcs > 0);
        }
    }
    eprintln!(
        "fractional timing bad-FCS development offsets={TIMING_DEVELOPMENT_OFFSETS:?} observations={observations:?}"
    );
}

#[test]
fn full_cf32_fractional_timing_noise_development_controls_remain_empty() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let sample_count = short_waveform.len() + longer_waveform.len();
    let reference_power = active_waveform_power(&short_waveform);
    for (seed_index, seed) in TIMING_DEVELOPMENT_SEEDS.iter().copied().enumerate() {
        let noise = noise_only(sample_count, reference_power, 0.0, seed);
        for offset in TIMING_DEVELOPMENT_OFFSETS {
            assert_eq!(
                run_impairment_case(
                    &format!("fractional-timing-noise-seed-{seed_index}-{offset:+}-sixteenths"),
                    17,
                    &apply_fractional_timing(&noise, offset),
                    &[],
                ),
                [CandidateCounts::NONE; 2],
            );
        }
    }
}

#[test]
fn full_cf32_fractional_timing_raw_noise_fixed_candidate_controls_remain_empty() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let sample_count = short_waveform.len() + longer_waveform.len();
    let reference_power = active_waveform_power(&short_waveform);
    for (seed_index, seed) in TIMING_DEVELOPMENT_SEEDS.iter().copied().enumerate() {
        let noise = noise_only(sample_count, reference_power, 0.0, seed);
        assert_eq!(
            run_fixed_timing_candidate_case(
                &format!("fractional-timing-raw-noise-seed-{seed_index}"),
                17,
                &noise,
                &[],
            ),
            [CandidateCounts::NONE; 4]
        );
    }
}

#[test]
fn full_cf32_graded_awgn_development_corpus_is_frozen() {
    assert_graded_awgn_corpus(
        "development",
        &DEVELOPMENT_NOISE_SEEDS,
        &EXPECTED_DEVELOPMENT_COUNTS,
    );
}

#[test]
fn full_cf32_graded_awgn_holdout_corpus_is_frozen() {
    assert_graded_awgn_corpus("holdout", &HOLDOUT_NOISE_SEEDS, &EXPECTED_HOLDOUT_COUNTS);
}

#[test]
fn full_cf32_explicit_cfo_correction_controls_are_frozen() {
    const SAMPLE_RATE_HZ: f64 = 4_000_000.0;

    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);

    for (injected_offset_hz, uncorrected_expected, wrong_sign_expected) in [
        (
            -100_000,
            [CandidateCounts::NONE; 2],
            [CandidateCounts::NONE; 2],
        ),
        (
            100_000,
            [CandidateCounts::BOTH_TRANSMITTED; 2],
            [CandidateCounts::BOTH_TRANSMITTED; 2],
        ),
    ] {
        let mut samples = apply_carrier_offset(
            &short_waveform,
            f64::from(injected_offset_hz),
            SAMPLE_RATE_HZ,
        );
        samples.extend(apply_carrier_offset(
            &longer_waveform,
            f64::from(injected_offset_hz),
            SAMPLE_RATE_HZ,
        ));
        let case = format!("development-cfo-{injected_offset_hz:+}-hz");
        let observations = run_impairment_case(&case, 17, &samples, &transmitted);
        assert_eq!(observations, uncorrected_expected);
        assert_eq!(
            run_impairment_case_with_carrier_offset(
                &format!("{case}-corrected"),
                17,
                &samples,
                &transmitted,
                injected_offset_hz,
            ),
            [CandidateCounts::BOTH_TRANSMITTED; 2]
        );
        assert_eq!(
            run_impairment_case_with_carrier_offset(
                &format!("{case}-wrong-sign"),
                17,
                &samples,
                &transmitted,
                -injected_offset_hz,
            ),
            wrong_sign_expected
        );
    }

    let mut clean = short_waveform.clone();
    clean.extend_from_slice(&longer_waveform);
    let reference_power = active_waveform_power(&short_waveform);
    let noise = noise_only(
        clean.len(),
        reference_power,
        0.0,
        DEVELOPMENT_NOISE_SEEDS[0],
    );
    for injected_offset_hz in [-100_000, 100_000] {
        let offset_noise =
            apply_carrier_offset(&noise, f64::from(injected_offset_hz), SAMPLE_RATE_HZ);
        assert_eq!(
            run_impairment_case_with_carrier_offset(
                &format!("development-cfo-noise-only-{injected_offset_hz:+}-hz"),
                17,
                &offset_noise,
                &[],
                injected_offset_hz,
            ),
            [CandidateCounts::NONE; 2]
        );
    }
}

#[test]
fn full_cf32_h3_clean_development_grid_has_per_lane_coverage() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let mut matrix = Vec::new();

    for actual_offset_hz in H3_DEVELOPMENT_OFFSETS_HZ {
        let observations = run_h3_case(
            &format!("h3-clean-actual-{actual_offset_hz:+}-hz"),
            17,
            &cfo_samples(&short_waveform, &longer_waveform, actual_offset_hz),
            &transmitted,
        );
        assert_h3_two_frame_coverage(
            &format!("clean actual offset {actual_offset_hz:+} Hz"),
            &observations,
        );
        matrix.push(observations);
    }

    assert_eq!(matrix.as_slice(), &EXPECTED_H3_CLEAN_DEVELOPMENT_COUNTS);

    eprintln!(
        "H3 clean development offsets={H3_DEVELOPMENT_OFFSETS_HZ:?} hypotheses={H3_HYPOTHESES_HZ:?} matrix={matrix:?}"
    );
}

#[test]
fn full_cf32_h3_12db_awgn_development_grid_has_per_lane_coverage() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let mut matrices = Vec::new();

    for (seed_index, seed) in DEVELOPMENT_NOISE_SEEDS.iter().copied().enumerate() {
        let mut matrix = Vec::new();
        for actual_offset_hz in H3_DEVELOPMENT_OFFSETS_HZ {
            let observations = run_h3_case(
                &format!("h3-development-seed-{seed_index}-actual-{actual_offset_hz:+}-hz"),
                17,
                &cfo_awgn_samples(&short_waveform, &longer_waveform, actual_offset_hz, seed),
                &transmitted,
            );
            assert_h3_two_frame_coverage(
                &format!(
                    "12 dB development seed {seed_index} actual offset {actual_offset_hz:+} Hz"
                ),
                &observations,
            );
            matrix.push(observations);
        }
        matrices.push(matrix);
    }

    assert_eq!(matrices.len(), EXPECTED_H3_12DB_DEVELOPMENT_COUNTS.len());
    for (observed, expected) in matrices
        .iter()
        .zip(EXPECTED_H3_12DB_DEVELOPMENT_COUNTS.iter())
    {
        assert_eq!(observed.as_slice(), expected);
    }

    eprintln!(
        "H3 12 dB development offsets={H3_DEVELOPMENT_OFFSETS_HZ:?} hypotheses={H3_HYPOTHESES_HZ:?} matrices={matrices:?}"
    );
}

#[test]
fn full_cf32_h3_off_hypothesis_12db_awgn_holdout_has_per_lane_coverage() {
    let longer_psdu = longer_valid_psdu();
    let transmitted = vec![ACK_PSDU.to_vec(), longer_psdu.clone()];
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let mut matrices = Vec::new();

    for (seed_index, seed) in HOLDOUT_NOISE_SEEDS.iter().copied().enumerate() {
        let mut matrix = Vec::new();
        for actual_offset_hz in H3_HOLDOUT_OFFSETS_HZ {
            let observations = run_h3_case(
                &format!("h3-holdout-seed-{seed_index}-actual-{actual_offset_hz:+}-hz"),
                17,
                &cfo_awgn_samples(&short_waveform, &longer_waveform, actual_offset_hz, seed),
                &transmitted,
            );
            assert_h3_two_frame_coverage(
                &format!("12 dB holdout seed {seed_index} actual offset {actual_offset_hz:+} Hz"),
                &observations,
            );
            matrix.push(observations);
        }
        matrices.push(matrix);
    }

    assert_eq!(matrices.len(), EXPECTED_H3_12DB_HOLDOUT_COUNTS.len());
    for (observed, expected) in matrices.iter().zip(EXPECTED_H3_12DB_HOLDOUT_COUNTS.iter()) {
        assert_eq!(observed.as_slice(), expected);
    }

    eprintln!(
        "H3 12 dB holdout offsets={H3_HOLDOUT_OFFSETS_HZ:?} hypotheses={H3_HYPOTHESES_HZ:?} matrices={matrices:?}"
    );
}

#[test]
fn full_cf32_h3_pure_noise_controls_remain_empty() {
    let longer_psdu = longer_valid_psdu();
    let short_waveform = upstream_waveform(&ACK_PSDU);
    let longer_waveform = upstream_waveform(&longer_psdu);
    let sample_count = short_waveform.len() + longer_waveform.len();
    let reference_power = active_waveform_power(&short_waveform);

    for (seed_class, seeds) in [
        ("development", &DEVELOPMENT_NOISE_SEEDS),
        ("holdout", &HOLDOUT_NOISE_SEEDS),
    ] {
        for (seed_index, seed) in seeds.iter().copied().enumerate() {
            let samples = noise_only(sample_count, reference_power, 0.0, seed);
            for hypothesis_hz in H3_HYPOTHESES_HZ {
                assert_eq!(
                    run_impairment_case_with_carrier_offset(
                        &format!(
                            "h3-{seed_class}-noise-seed-{seed_index}-hypothesis-{hypothesis_hz:+}-hz"
                        ),
                        17,
                        &samples,
                        &[],
                        hypothesis_hz,
                    ),
                    [CandidateCounts::NONE; 2],
                );
            }
        }
    }
}
