//! Conservative soft-decision IEEE 802.15.4 2.4 GHz DSSS decoder.
//!
//! The input is the recovered `f32` chip metric emitted by the pinned
//! `ClockRecoveryMm` block. Magnitude is treated only as relative confidence;
//! it is not a calibrated log-likelihood ratio. Acquisition, SFD and PHR symbols
//! must lie inside the unique angular cap implied by the 30-chip codebook
//! geometry. Once the PHR has bounded the payload length, payload symbols use
//! the unique finite maximum-correlation decision and rely on independent FCS
//! inspection to promote a completed raw candidate.

use anyhow::anyhow;
#[cfg(test)]
use anyhow::ensure;
use futuresdr::runtime::dev::prelude::*;
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[cfg(test)]
use super::burst_clock::PositionedChip;

pub(crate) const CHIP_SEQUENCES: [u32; 16] = [
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

const SYMBOL_CHIPS: usize = 32;
const SCORED_CHIPS: usize = 30;
pub(crate) const UNIQUE_ANGULAR_CAP: f64 = 0.774_596_669_241_483_4; // sqrt(0.6)

#[derive(Clone, Copy, Debug)]
pub(crate) struct SymbolScores {
    pub(crate) best_symbol: u8,
    pub(crate) best_score: f64,
    pub(crate) runner_up_score: f64,
}

fn score_denominator(chips: &[f32; SYMBOL_CHIPS]) -> Option<f64> {
    let mut energy = 0.0f64;
    for chip in &chips[1..=SCORED_CHIPS] {
        let chip = f64::from(*chip);
        if !chip.is_finite() {
            return None;
        }
        energy += chip * chip;
    }
    if !energy.is_finite() || energy <= 0.0 {
        return None;
    }
    let denominator = (SCORED_CHIPS as f64 * energy).sqrt();
    (denominator.is_finite() && denominator > 0.0).then_some(denominator)
}

fn template_dot(chips: &[f32; SYMBOL_CHIPS], sequence: u32) -> f64 {
    chips[1..=SCORED_CHIPS]
        .iter()
        .enumerate()
        .map(|(offset, chip)| {
            let bit = 30 - offset;
            let template = if sequence & (1 << bit) == 0 {
                -1.0
            } else {
                1.0
            };
            f64::from(*chip) * template
        })
        .sum()
}

#[cfg(test)]
pub(crate) fn template_correlation(chips: &[f32; SYMBOL_CHIPS], symbol: u8) -> Option<f64> {
    let sequence = *CHIP_SEQUENCES.get(usize::from(symbol))?;
    Some(template_dot(chips, sequence) / score_denominator(chips)?)
}

/// Score one oldest-to-newest 32-chip window using the same logical positions
/// as the upstream decoder's `0x7fff_fffe` mask.
pub(crate) fn symbol_scores(chips: &[f32; SYMBOL_CHIPS]) -> Option<SymbolScores> {
    Some(score_templates(chips, score_denominator(chips)?))
}

fn score_templates(chips: &[f32; SYMBOL_CHIPS], denominator: f64) -> SymbolScores {
    let mut best_symbol = 0u8;
    let mut best_score = f64::NEG_INFINITY;
    let mut runner_up_score = f64::NEG_INFINITY;
    for (symbol, sequence) in CHIP_SEQUENCES.iter().enumerate() {
        let score = template_dot(chips, *sequence) / denominator;
        if score > best_score {
            runner_up_score = best_score;
            best_score = score;
            best_symbol = symbol as u8;
        } else if score > runner_up_score {
            runner_up_score = score;
        }
    }
    SymbolScores {
        best_symbol,
        best_score,
        runner_up_score,
    }
}

pub(crate) fn classify_symbol(chips: &[f32; SYMBOL_CHIPS]) -> Option<u8> {
    acquisition_symbol(symbol_scores(chips)?)
}

fn acquisition_symbol(scores: SymbolScores) -> Option<u8> {
    (scores.best_score > UNIQUE_ANGULAR_CAP
        && scores.best_score.is_finite()
        && scores.runner_up_score.is_finite()
        && scores.best_score > scores.runner_up_score)
        .then_some(scores.best_symbol)
}

pub(crate) fn is_preamble_symbol(chips: &[f32; SYMBOL_CHIPS]) -> bool {
    let Some(denominator) = score_denominator(chips) else {
        return false;
    };
    // Search accepts only symbol zero. Once it clears the cap, the remaining
    // templates still determine whether it is the unique winner.
    template_dot(chips, CHIP_SEQUENCES[0]) / denominator > UNIQUE_ANGULAR_CAP
        && acquisition_symbol(score_templates(chips, denominator)) == Some(0)
}

pub(crate) fn classify_payload_symbol(chips: &[f32; SYMBOL_CHIPS]) -> Option<u8> {
    let scores = symbol_scores(chips)?;
    (scores.best_score.is_finite()
        && scores.runner_up_score.is_finite()
        && scores.best_score > scores.runner_up_score)
        .then_some(scores.best_symbol)
}

#[derive(Debug)]
enum State {
    Search,
    PreambleFound,
    SearchSfd,
    SearchHeader {
        byte: Option<u8>,
    },
    Decode {
        len: usize,
        data: Vec<u8>,
        byte: Option<u8>,
    },
}

#[cfg(test)]
const MAX_DECODER_TRACE_EVENTS: usize = 2;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecoderTracePoint {
    pub(crate) source_sample_index: u64,
    pub(crate) event_chip_ordinal: u64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecoderDecisionPoint {
    pub(crate) point: DecoderTracePoint,
    pub(crate) observed_symbol: Option<u8>,
    pub(crate) best_symbol: u8,
    pub(crate) best_score_bits: u64,
    pub(crate) runner_up_score_bits: u64,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecodeEntry {
    pub(crate) length: u8,
    pub(crate) point: DecoderTracePoint,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DecoderTerminal {
    Search {
        chip_count: u32,
    },
    PreambleFound {
        chip_count: u32,
    },
    SearchSfd {
        chip_count: u32,
    },
    SearchHeader {
        low_nibble: Option<u8>,
        chip_count: u32,
    },
    Decode {
        length: u8,
        completed_bytes: u8,
        low_nibble: Option<u8>,
        chip_count: u32,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DecoderEventTrace {
    pub(crate) first_source_sample_index: u64,
    pub(crate) last_source_sample_index: u64,
    pub(crate) recovered_chips: u64,
    pub(crate) preamble_entries: u64,
    pub(crate) preamble_resets: u64,
    pub(crate) sfd7_entries: u64,
    pub(crate) sfd_resets: u64,
    pub(crate) sfd10_entries: u64,
    pub(crate) header_entries: u64,
    pub(crate) last_sfd7: Option<DecoderDecisionPoint>,
    pub(crate) last_sfd10: Option<DecoderDecisionPoint>,
    pub(crate) phr_low_decisions: u64,
    pub(crate) phr_high_decisions: u64,
    pub(crate) last_phr_low: Option<DecoderDecisionPoint>,
    pub(crate) last_phr_high: Option<DecoderDecisionPoint>,
    pub(crate) header_classification_resets: u64,
    pub(crate) invalid_phr_lengths: u64,
    pub(crate) decode_entries: u64,
    pub(crate) last_decode_entry: Option<DecodeEntry>,
    pub(crate) payload_symbols: u64,
    pub(crate) max_completed_payload_bytes: u8,
    pub(crate) payload_classification_resets: u64,
    pub(crate) completed_candidates: u64,
    pub(crate) last_completion: Option<DecoderTracePoint>,
    pub(crate) terminal: DecoderTerminal,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PositionedDecoderTraceStats {
    pub(crate) events: [Option<DecoderEventTrace>; MAX_DECODER_TRACE_EVENTS],
    pub(crate) overflow: u64,
}

#[cfg(test)]
impl Default for PositionedDecoderTraceStats {
    fn default() -> Self {
        Self {
            events: [None; MAX_DECODER_TRACE_EVENTS],
            overflow: 0,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecoderStateSnapshot {
    Search,
    PreambleFound,
    SearchSfd,
    SearchHeader {
        low_nibble: Option<u8>,
    },
    Decode {
        length: u8,
        completed_bytes: u8,
        low_nibble: Option<u8>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DecoderCoreSnapshot {
    state: DecoderStateSnapshot,
    chip_count: u32,
}

#[cfg(test)]
impl DecoderCoreSnapshot {
    fn terminal(self) -> DecoderTerminal {
        match self.state {
            DecoderStateSnapshot::Search => DecoderTerminal::Search {
                chip_count: self.chip_count,
            },
            DecoderStateSnapshot::PreambleFound => DecoderTerminal::PreambleFound {
                chip_count: self.chip_count,
            },
            DecoderStateSnapshot::SearchSfd => DecoderTerminal::SearchSfd {
                chip_count: self.chip_count,
            },
            DecoderStateSnapshot::SearchHeader { low_nibble } => DecoderTerminal::SearchHeader {
                low_nibble,
                chip_count: self.chip_count,
            },
            DecoderStateSnapshot::Decode {
                length,
                completed_bytes,
                low_nibble,
            } => DecoderTerminal::Decode {
                length,
                completed_bytes,
                low_nibble,
                chip_count: self.chip_count,
            },
        }
    }
}

#[cfg(test)]
impl DecoderEventTrace {
    fn new(first_source_sample_index: u64, terminal: DecoderTerminal) -> Self {
        Self {
            first_source_sample_index,
            last_source_sample_index: first_source_sample_index,
            recovered_chips: 0,
            preamble_entries: 0,
            preamble_resets: 0,
            sfd7_entries: 0,
            sfd_resets: 0,
            sfd10_entries: 0,
            header_entries: 0,
            last_sfd7: None,
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
            terminal,
        }
    }

    fn decision(
        point: DecoderTracePoint,
        scores: Option<SymbolScores>,
        observed_symbol: Option<u8>,
    ) -> Option<DecoderDecisionPoint> {
        scores.map(|scores| DecoderDecisionPoint {
            point,
            observed_symbol,
            best_symbol: scores.best_symbol,
            best_score_bits: scores.best_score.to_bits(),
            runner_up_score_bits: scores.runner_up_score.to_bits(),
        })
    }

    fn record_push(
        &mut self,
        source_sample_index: u64,
        before: DecoderCoreSnapshot,
        after: DecoderCoreSnapshot,
        scores: Option<SymbolScores>,
        observed_symbol: Option<u8>,
        completed_frame_len: Option<usize>,
    ) {
        self.recovered_chips += 1;
        self.last_source_sample_index = source_sample_index;
        let point = DecoderTracePoint {
            source_sample_index,
            event_chip_ordinal: self.recovered_chips,
        };
        let decision = || Self::decision(point, scores, observed_symbol);

        match (before.state, after.state) {
            (DecoderStateSnapshot::Search, DecoderStateSnapshot::PreambleFound) => {
                self.preamble_entries += 1;
            }
            (DecoderStateSnapshot::PreambleFound, DecoderStateSnapshot::SearchSfd) => {
                self.sfd7_entries += 1;
                self.last_sfd7 = decision();
            }
            (DecoderStateSnapshot::PreambleFound, DecoderStateSnapshot::Search) => {
                self.preamble_resets += 1;
            }
            (DecoderStateSnapshot::SearchSfd, DecoderStateSnapshot::SearchHeader { .. }) => {
                self.sfd10_entries += 1;
                self.header_entries += 1;
                self.last_sfd10 = decision();
            }
            (DecoderStateSnapshot::SearchSfd, DecoderStateSnapshot::Search) => {
                self.sfd_resets += 1;
            }
            (
                DecoderStateSnapshot::SearchHeader { low_nibble: None },
                DecoderStateSnapshot::SearchHeader {
                    low_nibble: Some(_),
                },
            ) => {
                self.phr_low_decisions += 1;
                self.last_phr_low = decision();
            }
            (
                DecoderStateSnapshot::SearchHeader {
                    low_nibble: Some(_),
                },
                DecoderStateSnapshot::Decode { length, .. },
            ) => {
                self.phr_high_decisions += 1;
                self.last_phr_high = decision();
                self.decode_entries += 1;
                self.last_decode_entry = Some(DecodeEntry { length, point });
            }
            (
                DecoderStateSnapshot::SearchHeader {
                    low_nibble: Some(low),
                },
                DecoderStateSnapshot::Search,
            ) => {
                if let Some(high) = observed_symbol {
                    self.phr_high_decisions += 1;
                    self.last_phr_high = decision();
                    let length = (high << 4) | low;
                    if !(1..128).contains(&length) {
                        self.invalid_phr_lengths += 1;
                    }
                } else {
                    self.header_classification_resets += 1;
                }
            }
            (
                DecoderStateSnapshot::SearchHeader { low_nibble: None },
                DecoderStateSnapshot::Search,
            ) => {
                self.header_classification_resets += 1;
            }
            (
                DecoderStateSnapshot::Decode { .. },
                DecoderStateSnapshot::Decode {
                    completed_bytes, ..
                },
            ) if after.chip_count == 0 && observed_symbol.is_some() => {
                self.payload_symbols += 1;
                self.max_completed_payload_bytes =
                    self.max_completed_payload_bytes.max(completed_bytes);
            }
            (DecoderStateSnapshot::Decode { length, .. }, DecoderStateSnapshot::Search)
                if completed_frame_len.is_some() =>
            {
                self.payload_symbols += 1;
                self.max_completed_payload_bytes = self.max_completed_payload_bytes.max(length);
                self.completed_candidates += 1;
                self.last_completion = Some(point);
            }
            (DecoderStateSnapshot::Decode { .. }, DecoderStateSnapshot::Search) => {
                self.payload_classification_resets += 1;
            }
            _ => {}
        }
        self.terminal = after.terminal();
    }
}

#[derive(Debug)]
struct DecoderCore {
    chips: [f32; SYMBOL_CHIPS],
    filled_chips: usize,
    chip_count: u32,
    state: State,
}

impl DecoderCore {
    fn new() -> Self {
        Self {
            chips: [0.0; SYMBOL_CHIPS],
            filled_chips: 0,
            chip_count: 0,
            state: State::Search,
        }
    }

    #[cfg(test)]
    fn reset(&mut self) {
        *self = Self::new();
    }

    #[cfg(test)]
    fn reset_to_decode(&mut self, length: u8) -> Result<()> {
        ensure!(
            (1..=127).contains(&length),
            "deferred decoder length must satisfy 1 <= length <= 127"
        );
        *self = Self {
            chips: [0.0; SYMBOL_CHIPS],
            filled_chips: 0,
            chip_count: 0,
            state: State::Decode {
                len: usize::from(length),
                data: Vec::with_capacity(usize::from(length)),
                byte: None,
            },
        };
        Ok(())
    }

    #[cfg(test)]
    fn snapshot(&self) -> DecoderCoreSnapshot {
        let state = match &self.state {
            State::Search => DecoderStateSnapshot::Search,
            State::PreambleFound => DecoderStateSnapshot::PreambleFound,
            State::SearchSfd => DecoderStateSnapshot::SearchSfd,
            State::SearchHeader { byte } => {
                DecoderStateSnapshot::SearchHeader { low_nibble: *byte }
            }
            State::Decode { len, data, byte } => DecoderStateSnapshot::Decode {
                length: *len as u8,
                completed_bytes: data.len() as u8,
                low_nibble: *byte,
            },
        };
        DecoderCoreSnapshot {
            state,
            chip_count: self.chip_count,
        }
    }

    #[cfg(test)]
    fn terminal(&self) -> DecoderTerminal {
        let chip_count = self.chip_count;
        match &self.state {
            State::Search => DecoderTerminal::Search { chip_count },
            State::PreambleFound => DecoderTerminal::PreambleFound { chip_count },
            State::SearchSfd => DecoderTerminal::SearchSfd { chip_count },
            State::SearchHeader { byte } => DecoderTerminal::SearchHeader {
                low_nibble: *byte,
                chip_count,
            },
            State::Decode { len, data, byte } => DecoderTerminal::Decode {
                length: *len as u8,
                completed_bytes: data.len() as u8,
                low_nibble: *byte,
                chip_count,
            },
        }
    }

    #[cfg(test)]
    fn current_scores(&self) -> Option<SymbolScores> {
        (self.filled_chips == SYMBOL_CHIPS)
            .then(|| symbol_scores(&self.chips))
            .flatten()
    }

    #[cfg(test)]
    fn current_observed_symbol(&self, payload: bool) -> Option<u8> {
        (self.filled_chips == SYMBOL_CHIPS)
            .then(|| {
                if payload {
                    classify_payload_symbol(&self.chips)
                } else {
                    classify_symbol(&self.chips)
                }
            })
            .flatten()
    }

    fn push(&mut self, chip: f32) -> Option<Vec<u8>> {
        self.chips.copy_within(1.., 0);
        self.chips[SYMBOL_CHIPS - 1] = chip;
        self.filled_chips = (self.filled_chips + 1).min(SYMBOL_CHIPS);
        self.chip_count = (self.chip_count + 1) % SYMBOL_CHIPS as u32;
        let observed_symbol = (self.filled_chips == SYMBOL_CHIPS)
            .then(|| match &self.state {
                State::Search => is_preamble_symbol(&self.chips).then_some(0),
                _ if self.chip_count != 0 => None,
                State::Decode { .. } => classify_payload_symbol(&self.chips),
                _ => classify_symbol(&self.chips),
            })
            .flatten();

        match &mut self.state {
            State::Search => {
                if observed_symbol == Some(0) {
                    self.state = State::PreambleFound;
                    self.chip_count = 0;
                }
            }
            State::PreambleFound => {
                if self.chip_count == 0 {
                    if observed_symbol == Some(7) {
                        self.state = State::SearchSfd;
                    } else if observed_symbol != Some(0) {
                        self.state = State::Search;
                    }
                }
            }
            State::SearchSfd => {
                if self.chip_count == 0 {
                    if observed_symbol == Some(10) {
                        self.state = State::SearchHeader { byte: None };
                    } else {
                        self.state = State::Search;
                    }
                }
            }
            State::SearchHeader { byte } => {
                if self.chip_count == 0 {
                    if let Some(symbol) = observed_symbol {
                        if let Some(low) = byte {
                            let len = (symbol << 4) | *low;
                            if (1..128).contains(&len) {
                                self.state = State::Decode {
                                    len: usize::from(len),
                                    data: Vec::with_capacity(usize::from(len)),
                                    byte: None,
                                };
                            } else {
                                self.state = State::Search;
                            }
                        } else {
                            *byte = Some(symbol);
                        }
                    } else {
                        self.state = State::Search;
                    }
                }
            }
            State::Decode { len, data, byte } => {
                if self.chip_count == 0 {
                    if let Some(symbol) = observed_symbol {
                        if let Some(low) = byte {
                            data.push((symbol << 4) | *low);
                            *byte = None;
                            if data.len() == *len {
                                let frame = std::mem::take(data);
                                self.state = State::Search;
                                return Some(frame);
                            }
                        } else {
                            *byte = Some(symbol);
                        }
                    } else {
                        self.state = State::Search;
                    }
                }
            }
        }
        None
    }
}

#[derive(Block)]
#[message_outputs(out, indexed)]
pub(crate) struct SoftDsssDecoder<I = DefaultCpuReader<f32>>
where
    I: CpuBufferReader<Item = f32>,
{
    #[input]
    input: I,
    total_chips: u64,
    core: DecoderCore,
}

impl<I> SoftDsssDecoder<I>
where
    I: CpuBufferReader<Item = f32>,
{
    pub(crate) fn new() -> Self {
        Self {
            input: I::default(),
            total_chips: 0,
            core: DecoderCore::new(),
        }
    }

    fn push_chip(&mut self, chip: f32) -> Result<Option<(u64, Vec<u8>)>> {
        self.total_chips = self
            .total_chips
            .checked_add(1)
            .ok_or_else(|| anyhow!("recovered chip counter overflow"))?;
        Ok(self.core.push(chip).map(|frame| (self.total_chips, frame)))
    }
}

impl<I> Kernel for SoftDsssDecoder<I>
where
    I: CpuBufferReader<Item = f32>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        mo: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        let input = self.input.slice().to_vec();
        let input_len = input.len();

        for chip in input {
            if let Some((completion_tick, frame)) = self.push_chip(chip)? {
                mo.post("out", Pmt::Blob(frame.clone())).await?;
                mo.post(
                    "indexed",
                    Pmt::VecPmt(vec![Pmt::U64(completion_tick), Pmt::Blob(frame)]),
                )
                .await?;
            }
        }

        if self.input.finished() {
            io.finished = true;
        }
        self.input.consume(input_len);
        Ok(())
    }
}

#[cfg(test)]
const fn source_sample_archive_tick(source_sample_index: u64) -> u64 {
    source_sample_index / 2 + 1
}

#[cfg(test)]
#[derive(Block)]
#[message_outputs(out, indexed)]
pub(crate) struct PositionedSoftDsssDecoder<I = DefaultCpuReader<PositionedChip>>
where
    I: CpuBufferReader<Item = PositionedChip>,
{
    #[input]
    input: I,
    prior_source_sample_index: Option<u64>,
    core: DecoderCore,
    trace_observer: Option<Arc<Mutex<Option<PositionedDecoderTraceStats>>>>,
    trace_stats: PositionedDecoderTraceStats,
    current_trace: Option<DecoderEventTrace>,
    trace_published: bool,
}

#[cfg(test)]
impl<I> PositionedSoftDsssDecoder<I>
where
    I: CpuBufferReader<Item = PositionedChip>,
{
    pub(crate) fn new() -> Self {
        Self {
            input: I::default(),
            prior_source_sample_index: None,
            core: DecoderCore::new(),
            trace_observer: None,
            trace_stats: PositionedDecoderTraceStats::default(),
            current_trace: None,
            trace_published: false,
        }
    }

    pub(crate) fn with_trace_observer(
        observer: Arc<Mutex<Option<PositionedDecoderTraceStats>>>,
    ) -> Self {
        Self {
            input: I::default(),
            prior_source_sample_index: None,
            core: DecoderCore::new(),
            trace_observer: Some(observer),
            trace_stats: PositionedDecoderTraceStats::default(),
            current_trace: None,
            trace_published: false,
        }
    }

    fn finalize_trace_event(&mut self) {
        let Some(trace) = self.current_trace.take() else {
            return;
        };
        if let Some(slot) = self
            .trace_stats
            .events
            .iter_mut()
            .find(|slot| slot.is_none())
        {
            *slot = Some(trace);
        } else {
            self.trace_stats.overflow += 1;
        }
    }

    fn publish_trace(&mut self) -> Result<()> {
        if self.trace_published {
            return Ok(());
        }
        self.finalize_trace_event();
        if let Some(observer) = &self.trace_observer {
            *observer
                .lock()
                .map_err(|_| anyhow!("positioned decoder trace lock was poisoned"))? =
                Some(self.trace_stats);
        }
        self.trace_published = true;
        Ok(())
    }

    fn push_positioned(&mut self, chip: PositionedChip) -> Result<Option<(u64, Vec<u8>)>> {
        // Marker metadata and coordinates are an input boundary: reject them
        // before resetting decoder state, finalizing traces, or advancing the
        // prior coordinate so a caller can recover from an invalid item.
        ensure!(
            chip.payload_length.is_none() || chip.burst_start,
            "positioned payload length requires a burst-start marker"
        );
        ensure!(
            chip.payload_length
                .is_none_or(|length| (1..=127).contains(&length)),
            "positioned payload length must satisfy 1 <= length <= 127"
        );
        ensure!(
            self.prior_source_sample_index
                .is_none_or(|prior| chip.source_sample_index > prior),
            "positioned chip source sample indices must be strictly increasing"
        );
        if chip.burst_start {
            self.finalize_trace_event();
            if let Some(length) = chip.payload_length {
                // Deferred acquisition already validated SHR, SFD, and PHR;
                // this marked metric is payload chip zero.
                self.core.reset_to_decode(length)?;
            } else {
                self.core.reset();
            }
            if self.trace_observer.is_some() {
                self.current_trace = Some(DecoderEventTrace::new(
                    chip.source_sample_index,
                    self.core.terminal(),
                ));
            }
        }
        self.prior_source_sample_index = Some(chip.source_sample_index);
        let frame = if let Some(trace) = &mut self.current_trace {
            let before = self.core.snapshot();
            let payload_decision = matches!(before.state, DecoderStateSnapshot::Decode { .. });
            let frame = self.core.push(chip.metric);
            let after = self.core.snapshot();
            let scores = self.core.current_scores();
            let observed_symbol = self.core.current_observed_symbol(payload_decision);
            trace.record_push(
                chip.source_sample_index,
                before,
                after,
                scores,
                observed_symbol,
                frame.as_ref().map(Vec::len),
            );
            frame
        } else {
            self.core.push(chip.metric)
        };
        Ok(frame.map(|frame| (source_sample_archive_tick(chip.source_sample_index), frame)))
    }
}

#[cfg(test)]
impl<I> Kernel for PositionedSoftDsssDecoder<I>
where
    I: CpuBufferReader<Item = PositionedChip>,
{
    async fn work(
        &mut self,
        io: &mut WorkIo,
        mo: &mut MessageOutputs,
        _meta: &BlockMeta,
    ) -> Result<()> {
        let input = self.input.slice().to_vec();
        let input_len = input.len();

        for chip in input {
            if let Some((completion_tick, frame)) = self.push_positioned(chip)? {
                mo.post("out", Pmt::Blob(frame.clone())).await?;
                mo.post(
                    "indexed",
                    Pmt::VecPmt(vec![Pmt::U64(completion_tick), Pmt::Blob(frame)]),
                )
                .await?;
            }
        }

        if self.input.finished() {
            self.publish_trace()?;
            io.finished = true;
        }
        self.input.consume(input_len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACK_PSDU: [u8; 5] = [0x02, 0x00, 0x53, 0xa6, 0xd5];

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

    fn append_shr(chips: &mut Vec<f32>) {
        for _ in 0..8 {
            append_symbol(chips, 0);
        }
        append_symbol(chips, 7);
        append_symbol(chips, 10);
    }

    fn push_positioned_event(
        decoder: &mut PositionedSoftDsssDecoder,
        chips: &[f32],
        first_source_sample_index: u64,
    ) -> Vec<(u64, Vec<u8>)> {
        chips
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(offset, metric)| {
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: first_source_sample_index + 2 * offset as u64,
                        burst_start: offset == 0,
                        payload_length: None,
                    })
                    .unwrap()
            })
            .collect()
    }

    fn take_trace(
        observer: &Arc<Mutex<Option<PositionedDecoderTraceStats>>>,
    ) -> PositionedDecoderTraceStats {
        observer
            .lock()
            .unwrap()
            .take()
            .expect("positioned decoder publishes trace")
    }

    #[test]
    fn ordinary_wrapper_preserves_completion_tick_and_payload() {
        let mut chips = Vec::new();
        append_phy_packet(&mut chips, &[]);
        append_phy_packet(&mut chips, &ACK_PSDU);
        let mut decoder: SoftDsssDecoder = SoftDsssDecoder::new();
        let observed = chips
            .into_iter()
            .filter_map(|chip| decoder.push_chip(chip).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(1_088, ACK_PSDU.to_vec())]);
    }

    #[test]
    fn positioned_wrapper_resets_before_marked_item_and_preserves_global_tick() {
        let mut interrupted = Vec::new();
        append_phy_packet(&mut interrupted, &[0x55; 127]);
        interrupted.truncate(400);
        let mut complete = Vec::new();
        append_phy_packet(&mut complete, &ACK_PSDU);

        let mut decoder: PositionedSoftDsssDecoder = PositionedSoftDsssDecoder::new();
        let mut observed = Vec::new();
        for (offset, metric) in interrupted.into_iter().enumerate() {
            observed.extend(
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: 10_000 + 2 * offset as u64,
                        burst_start: offset == 0,
                        payload_length: None,
                    })
                    .unwrap(),
            );
        }
        for (offset, metric) in complete.into_iter().enumerate() {
            observed.extend(
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: 20_000 + 2 * offset as u64,
                        burst_start: offset == 0,
                        payload_length: None,
                    })
                    .unwrap(),
            );
        }

        assert_eq!(observed, vec![(10_704, ACK_PSDU.to_vec())]);
    }

    #[test]
    fn positioned_payload_marker_enters_decode_at_the_validated_length() {
        let mut payload_chips = Vec::new();
        for byte in ACK_PSDU {
            append_byte(&mut payload_chips, byte);
        }
        let observer = Arc::new(Mutex::new(None));
        let mut decoder: PositionedSoftDsssDecoder =
            PositionedSoftDsssDecoder::with_trace_observer(observer.clone());
        let observed = payload_chips
            .into_iter()
            .enumerate()
            .filter_map(|(offset, metric)| {
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: 10_000 + 2 * offset as u64,
                        burst_start: offset == 0,
                        payload_length: (offset == 0).then_some(ACK_PSDU.len() as u8),
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(5_320, ACK_PSDU.to_vec())]);
        decoder.publish_trace().unwrap();
        let trace = take_trace(&observer);
        let event = trace.events[0].expect("payload marker must start one decoder event");
        assert_eq!(event.first_source_sample_index, 10_000);
        assert_eq!(event.last_source_sample_index, 10_638);
        assert_eq!(event.recovered_chips, 320);
        assert_eq!(event.preamble_entries, 0);
        assert_eq!(event.sfd7_entries, 0);
        assert_eq!(event.sfd10_entries, 0);
        assert_eq!(event.phr_low_decisions, 0);
        assert_eq!(event.phr_high_decisions, 0);
        assert_eq!(event.payload_symbols, 10);
        assert_eq!(event.max_completed_payload_bytes, ACK_PSDU.len() as u8);
        assert_eq!(event.completed_candidates, 1);
        assert_eq!(event.last_completion.unwrap().source_sample_index, 10_638);
        assert!(matches!(event.terminal, DecoderTerminal::Search { .. }));
        assert_eq!(trace.events[1], None);
        assert_eq!(trace.overflow, 0);
    }

    #[test]
    fn positioned_payload_metadata_rejection_is_atomic() {
        let mut chips = Vec::new();
        append_symbol(&mut chips, 0);
        let observer = Arc::new(Mutex::new(None));
        let mut decoder: PositionedSoftDsssDecoder =
            PositionedSoftDsssDecoder::with_trace_observer(observer);
        assert!(
            decoder
                .push_positioned(PositionedChip {
                    metric: chips[0],
                    source_sample_index: 42,
                    burst_start: true,
                    payload_length: None,
                })
                .unwrap()
                .is_none()
        );
        let before_core = decoder.core.snapshot();
        let before_current = decoder.current_trace;
        let before_stats = decoder.trace_stats;
        let before_prior = decoder.prior_source_sample_index;

        for invalid in [
            PositionedChip {
                metric: chips[1],
                source_sample_index: 44,
                burst_start: false,
                payload_length: Some(5),
            },
            PositionedChip {
                metric: chips[1],
                source_sample_index: 44,
                burst_start: true,
                payload_length: Some(0),
            },
            PositionedChip {
                metric: chips[1],
                source_sample_index: 44,
                burst_start: true,
                payload_length: Some(128),
            },
        ] {
            assert!(decoder.push_positioned(invalid).is_err());
            assert_eq!(decoder.core.snapshot(), before_core);
            assert_eq!(decoder.current_trace, before_current);
            assert_eq!(decoder.trace_stats, before_stats);
            assert_eq!(decoder.prior_source_sample_index, before_prior);
        }

        assert!(
            decoder
                .push_positioned(PositionedChip {
                    metric: chips[1],
                    source_sample_index: 44,
                    burst_start: true,
                    payload_length: Some(5),
                })
                .unwrap()
                .is_none()
        );
        assert_eq!(decoder.prior_source_sample_index, Some(44));
        assert!(matches!(
            decoder.core.snapshot().state,
            DecoderStateSnapshot::Decode { length: 5, .. }
        ));
    }

    #[test]
    fn positioned_wrapper_rejects_nonincreasing_source_indices_atomically() {
        let mut complete = Vec::new();
        append_phy_packet(&mut complete, &ACK_PSDU);
        let mut decoder: PositionedSoftDsssDecoder = PositionedSoftDsssDecoder::new();
        let first = PositionedChip {
            metric: complete[0],
            source_sample_index: 42,
            burst_start: true,
            payload_length: None,
        };
        assert!(decoder.push_positioned(first).unwrap().is_none());
        assert!(decoder.push_positioned(first).is_err());
        assert_eq!(decoder.prior_source_sample_index, Some(42));
        let observed = complete
            .into_iter()
            .enumerate()
            .skip(1)
            .filter_map(|(offset, metric)| {
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: 42 + 2 * offset as u64,
                        burst_start: false,
                        payload_length: None,
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(725, ACK_PSDU.to_vec())]);
        assert_eq!(decoder.prior_source_sample_index, Some(1_448));
        assert_eq!(source_sample_archive_tick(u64::MAX), u64::MAX / 2 + 1);
    }

    #[test]
    fn positioned_trace_is_passive_marker_delimited_and_bounded() {
        let mut interrupted = Vec::new();
        append_phy_packet(&mut interrupted, &[0x55; 127]);
        interrupted.truncate(400);
        let mut complete = Vec::new();
        append_phy_packet(&mut complete, &ACK_PSDU);

        let mut ordinary: PositionedSoftDsssDecoder = PositionedSoftDsssDecoder::new();
        let mut ordinary_output = push_positioned_event(&mut ordinary, &interrupted, 10_000);
        ordinary_output.extend(push_positioned_event(&mut ordinary, &complete, 20_000));

        let observer = Arc::new(Mutex::new(None));
        let mut traced: PositionedSoftDsssDecoder =
            PositionedSoftDsssDecoder::with_trace_observer(observer.clone());
        let mut traced_output = push_positioned_event(&mut traced, &interrupted, 10_000);
        traced_output.extend(push_positioned_event(&mut traced, &complete, 20_000));
        traced.publish_trace().unwrap();
        assert_eq!(traced_output, ordinary_output);

        let trace = take_trace(&observer);
        assert_eq!(trace.overflow, 0);
        let first = trace.events[0].unwrap();
        let second = trace.events[1].unwrap();
        assert_eq!(first.first_source_sample_index, 10_000);
        assert_eq!(first.last_source_sample_index, 10_798);
        assert_eq!(first.recovered_chips, 400);
        assert!(matches!(
            first.terminal,
            DecoderTerminal::Decode {
                length: 127,
                completed_bytes: 0,
                ..
            }
        ));
        assert_eq!(first.completed_candidates, 0);
        assert_eq!(second.first_source_sample_index, 20_000);
        assert_eq!(second.recovered_chips, complete.len() as u64);
        assert_eq!(second.completed_candidates, 1);
        assert_eq!(second.max_completed_payload_bytes, ACK_PSDU.len() as u8);
        assert_eq!(second.last_completion.unwrap().source_sample_index, 21_406);
        assert!(matches!(second.terminal, DecoderTerminal::Search { .. }));
    }

    #[test]
    fn positioned_trace_distinguishes_sfd_phr_and_payload_truncation() {
        let mut wrong_sfd = Vec::new();
        for _ in 0..8 {
            append_symbol(&mut wrong_sfd, 0);
        }
        append_symbol(&mut wrong_sfd, 7);
        append_symbol(&mut wrong_sfd, 9);

        let mut invalid_phr = Vec::new();
        append_shr(&mut invalid_phr);
        append_byte(&mut invalid_phr, 0);

        let mut truncated = Vec::new();
        append_shr(&mut truncated);
        append_byte(&mut truncated, 127);
        append_symbol(&mut truncated, 5);

        for (index, chips) in [&wrong_sfd, &invalid_phr, &truncated]
            .into_iter()
            .enumerate()
        {
            let observer = Arc::new(Mutex::new(None));
            let mut decoder: PositionedSoftDsssDecoder =
                PositionedSoftDsssDecoder::with_trace_observer(observer.clone());
            assert!(push_positioned_event(&mut decoder, chips, 10_000).is_empty());
            decoder.publish_trace().unwrap();
            let event = take_trace(&observer).events[0].unwrap();
            match index {
                0 => {
                    assert_eq!(event.sfd7_entries, 1);
                    assert_eq!(event.sfd10_entries, 0);
                    assert_eq!(event.sfd_resets, 1);
                    assert_eq!(event.invalid_phr_lengths, 0);
                    assert!(matches!(event.terminal, DecoderTerminal::Search { .. }));
                }
                1 => {
                    assert_eq!(event.sfd10_entries, 1);
                    assert_eq!(event.phr_low_decisions, 1);
                    assert_eq!(event.phr_high_decisions, 1);
                    assert_eq!(event.invalid_phr_lengths, 1);
                    assert_eq!(event.decode_entries, 0);
                    assert!(matches!(event.terminal, DecoderTerminal::Search { .. }));
                }
                2 => {
                    assert_eq!(event.decode_entries, 1);
                    assert_eq!(event.last_decode_entry.unwrap().length, 127);
                    assert!(matches!(
                        event.terminal,
                        DecoderTerminal::Decode {
                            length: 127,
                            completed_bytes: 0,
                            low_nibble: Some(5),
                            ..
                        }
                    ));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn positioned_trace_publication_is_one_shot_and_counts_overflow() {
        let mut chips = Vec::new();
        append_symbol(&mut chips, 0);
        let observer = Arc::new(Mutex::new(None));
        let mut decoder: PositionedSoftDsssDecoder =
            PositionedSoftDsssDecoder::with_trace_observer(observer.clone());
        for event_index in 0..3 {
            assert!(
                push_positioned_event(&mut decoder, &chips, 10_000 + event_index as u64 * 1_000,)
                    .is_empty()
            );
        }
        decoder.publish_trace().unwrap();
        let trace = take_trace(&observer);
        assert_eq!(trace.events.iter().flatten().count(), 2);
        assert_eq!(trace.overflow, 1);
        decoder.publish_trace().unwrap();
        assert!(observer.lock().unwrap().is_none());
    }

    #[test]
    fn positioned_trace_coordinate_rejection_is_atomic() {
        let mut chips = Vec::new();
        append_symbol(&mut chips, 0);
        let observer = Arc::new(Mutex::new(None));
        let mut decoder: PositionedSoftDsssDecoder =
            PositionedSoftDsssDecoder::with_trace_observer(observer.clone());
        let first = PositionedChip {
            metric: chips[0],
            source_sample_index: 42,
            burst_start: true,
            payload_length: None,
        };
        assert!(decoder.push_positioned(first).unwrap().is_none());
        let before_core = decoder.core.snapshot();
        let before_current = decoder.current_trace;
        let before_stats = decoder.trace_stats;
        assert!(decoder.push_positioned(first).is_err());
        assert_eq!(decoder.core.snapshot(), before_core);
        assert_eq!(decoder.current_trace, before_current);
        assert_eq!(decoder.trace_stats, before_stats);
        assert_eq!(decoder.prior_source_sample_index, Some(42));

        for (offset, metric) in chips.into_iter().enumerate().skip(1) {
            assert!(
                decoder
                    .push_positioned(PositionedChip {
                        metric,
                        source_sample_index: 42 + 2 * offset as u64,
                        burst_start: false,
                        payload_length: None,
                    })
                    .unwrap()
                    .is_none()
            );
        }
        decoder.publish_trace().unwrap();
        let trace = take_trace(&observer);
        let event = trace.events[0].unwrap();
        assert_eq!(event.recovered_chips, 32);
        assert_eq!(
            event.terminal,
            DecoderTerminal::PreambleFound { chip_count: 0 }
        );
        assert_eq!(trace.overflow, 0);
    }
}
