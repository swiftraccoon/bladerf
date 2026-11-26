//! Find 2 MHz S1G bursts and extract FCS-checked beacon identities.

use std::collections::VecDeque;
use std::f64::consts::TAU;

use rustfft::num_complex::{Complex32, Complex64};

use super::bits::{self, MAX_DATA_SYMBOLS, Sig};
use super::ofdm::{DataParameters, Format, Ofdm, Preamble};

const STF_PERIOD: usize = 16;
const STF_WINDOW: usize = 144;
const STF_SAMPLES: usize = STF_PERIOD + STF_WINDOW;
const MIN_STF_SCORE_SQUARED: f64 = 0.65 * 0.65;
const LOCK_LOOKBEHIND: u64 = 64;
const HEADER_LOOKAHEAD: u64 = 576;
const HISTORY_LIMIT: usize = 1_024 + MAX_DATA_SYMBOLS * 80;
const MAX_PENDING: usize = 16;
const REPEAT_TOLERANCE: u64 = 24;

/// One FCS-verified S1G beacon on the 2 MHz channel grid.
pub(super) struct Beacon {
    pub(super) first_sample: u64,
    pub(super) end_sample_exclusive: u64,
    pub(super) carrier_offset_hz: f32,
    pub(super) format: Format,
    pub(super) mcs: u8,
    pub(super) short_gi: bool,
    pub(super) bssid: [u8; 6],
    pub(super) fcs: u32,
    pub(super) mpdu_bytes: usize,
}

/// Correlation and integrity counts for the replayed prefix.
#[derive(Default)]
pub(super) struct Counts {
    pub(super) stf_candidates: usize,
    pub(super) validated_sig_headers: usize,
    pub(super) validated_mpdus: usize,
    pub(super) skipped_candidates: usize,
}

/// Keep only the channelized history and outstanding candidates needed to decode.
pub(super) struct Receiver {
    detector: Stf,
    ofdm: Ofdm,
    history: VecDeque<Complex32>,
    pending: Vec<Pending>,
    last_packet: Option<u64>,
    next: u64,
    dc: Complex32,
    pub(super) counts: Counts,
}

impl Receiver {
    /// Start a fresh decode operation on contiguous 2 MSps channel samples.
    pub(super) fn new() -> Self {
        Self {
            detector: Stf::new(),
            ofdm: Ofdm::new(),
            history: VecDeque::with_capacity(HISTORY_LIMIT),
            pending: Vec::with_capacity(MAX_PENDING),
            last_packet: None,
            next: 0,
            dc: Complex32::default(),
            counts: Counts::default(),
        }
    }

    /// Advance the channel grid and emit completed beacon observations.
    pub(super) fn push(&mut self, iq: Complex32, emit: &mut impl FnMut(Beacon)) {
        // S1G leaves the DC bin unused. Track stationary leakage more slowly
        // than the lowest occupied subcarrier to keep STF acquisition stable.
        self.dc += (iq - self.dc) / 256.0;
        let value = iq - self.dc;
        let index = self.next;
        self.next += 1;
        if self.history.len() == HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(value);
        if let Some(peak) = self.detector.push(value, index) {
            self.schedule(peak);
        }
        self.service(false, emit);
    }

    /// Try candidates with enough measured samples after the prefix ends.
    pub(super) fn finish(&mut self, emit: &mut impl FnMut(Beacon)) {
        if let Some(peak) = self.detector.peak.take() {
            self.schedule(peak);
        }
        self.service(true, emit);
    }

    fn schedule(&mut self, peak: Peak) {
        self.counts.stf_candidates += 1;
        if self.pending.len() == MAX_PENDING {
            self.counts.skipped_candidates += 1;
            return;
        }
        self.pending.push(Pending {
            base: peak.start.saturating_sub(LOCK_LOOKBEHIND),
            due: peak.start + HEADER_LOOKAHEAD,
            stage: Stage::Header(peak),
        });
    }

    fn service(&mut self, finished: bool, emit: &mut impl FnMut(Beacon)) {
        while let Some(index) = self
            .pending
            .iter()
            .position(|candidate| finished || self.next >= candidate.due)
        {
            let candidate = self.pending.remove(index);
            let history_origin = self.next - self.history.len() as u64;
            let Some(begin) = candidate.base.checked_sub(history_origin) else {
                self.counts.skipped_candidates += 1;
                continue;
            };
            let samples = &self.history.make_contiguous()[begin as usize..];
            match candidate.stage {
                Stage::Header(peak) => {
                    let coarse = (peak.start - candidate.base) as usize;
                    if let Some(preamble) = self.ofdm.lock(samples, coarse, peak.cfo_hz)
                        && let Some(observed) = self.ofdm.sig(samples, &preamble)
                        && let Some(sig) =
                            bits::decode_sig(&observed.soft, observed.format == Format::Long)
                    {
                        let params = DataParameters {
                            format: observed.format,
                            symbols: sig.data_symbols,
                            short_gi: sig.short_gi,
                            traveling_pilots: sig.traveling_pilots,
                        };
                        self.counts.validated_sig_headers += 1;
                        if let Some(end) = Ofdm::data_end(&preamble, params) {
                            self.pending.push(Pending {
                                base: candidate.base,
                                due: candidate.base + end as u64,
                                stage: Stage::Data {
                                    preamble: Box::new(preamble),
                                    params,
                                    sig,
                                },
                            });
                        }
                    }
                }
                Stage::Data {
                    preamble,
                    params,
                    sig,
                } if self.next >= candidate.due => {
                    let first = candidate.base + preamble.start as u64;
                    if self
                        .last_packet
                        .is_some_and(|last| last.abs_diff(first) <= REPEAT_TOLERANCE)
                    {
                        continue;
                    }
                    if let Some(data) = self.ofdm.data(samples, &preamble, params)
                        && let Some(count) = bits::decode_data(sig, &data.tones, |mpdu| {
                            if let Some((bssid, fcs)) = beacon_fields(mpdu) {
                                emit(Beacon {
                                    first_sample: first,
                                    end_sample_exclusive: candidate.base + data.end as u64,
                                    carrier_offset_hz: preamble.cfo_hz,
                                    format: params.format,
                                    mcs: sig.mcs,
                                    short_gi: sig.short_gi,
                                    bssid,
                                    fcs,
                                    mpdu_bytes: mpdu.len(),
                                });
                            }
                        })
                        && count != 0
                    {
                        self.counts.validated_mpdus += count;
                        self.last_packet = Some(first);
                    }
                }
                Stage::Data { .. } => (),
            }
        }
    }
}

fn beacon_fields(mpdu: &[u8]) -> Option<([u8; 6], u32)> {
    // S1G Beacon is extension subtype 1. Its fixed header includes the
    // source address, timestamp and change sequence before optional fields.
    if mpdu.len() < 19 || mpdu[0] != 0x1c {
        return None;
    }
    let bssid = mpdu[4..10].try_into().ok()?;
    let fcs = u32::from_le_bytes(mpdu.last_chunk::<4>().copied()?);
    Some((bssid, fcs))
}

struct Pending {
    base: u64,
    due: u64,
    stage: Stage,
}

enum Stage {
    Header(Peak),
    Data {
        preamble: Box<Preamble>,
        params: DataParameters,
        sig: Sig,
    },
}

#[derive(Clone, Copy)]
struct Peak {
    start: u64,
    cfo_hz: f32,
    score: f64,
}

struct Stf {
    lag: VecDeque<Complex32>,
    sums: VecDeque<(Complex64, f64)>,
    correlation: Complex64,
    energy: f64,
    peak: Option<Peak>,
    run_start: u64,
    quiet: u64,
    exhausted: bool,
}

impl Stf {
    fn new() -> Self {
        Self {
            lag: VecDeque::with_capacity(STF_PERIOD),
            sums: VecDeque::with_capacity(STF_WINDOW),
            correlation: Complex64::default(),
            energy: 0.0,
            peak: None,
            run_start: 0,
            quiet: 0,
            exhausted: false,
        }
    }

    fn push(&mut self, value: Complex32, index: u64) -> Option<Peak> {
        let previous = (self.lag.len() == STF_PERIOD)
            .then(|| self.lag.pop_front())
            .flatten();
        self.lag.push_back(value);
        let previous = previous?;
        if self.sums.len() == STF_WINDOW
            && let Some((correlation, energy)) = self.sums.pop_front()
        {
            self.correlation -= correlation;
            self.energy -= energy;
        }
        let product = value * previous.conj();
        let correlation = Complex64::new(f64::from(product.re), f64::from(product.im));
        let energy = f64::from(value.norm_sqr() + previous.norm_sqr()) * 0.5;
        self.sums.push_back((correlation, energy));
        self.correlation += correlation;
        self.energy += energy;
        if self.sums.len() < STF_WINDOW {
            return None;
        }
        let score = self.correlation.norm_sqr() / self.energy.max(1.0).powi(2);
        if self.energy >= STF_WINDOW as f64 && score >= MIN_STF_SCORE_SQUARED {
            self.quiet = 0;
            if self.exhausted {
                return None;
            }
            if self.peak.is_none() {
                self.run_start = index;
            }
            if self.peak.is_none_or(|old| score > old.score) {
                self.peak = Some(Peak {
                    start: index + 1 - STF_SAMPLES as u64,
                    cfo_hz: (self.correlation.arg() * 2_000_000.0 / (TAU * STF_PERIOD as f64))
                        as f32,
                    score,
                });
            }
            // Persistent CW must yield bounded work even when correlation
            // stays high for the rest of the capture.
            if index - self.run_start >= STF_SAMPLES as u64 {
                self.exhausted = true;
                return self.peak.take();
            }
        } else {
            self.quiet += 1;
            if self.quiet >= STF_PERIOD as u64 {
                self.exhausted = false;
                return self.peak.take();
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_persistent_periodic_carrier_does_not_keep_scheduling_candidates() {
        let mut detector = Stf::new();
        let mut peaks = Vec::new();
        for index in 0..2_048 {
            let in_carrier = !(768..1_280).contains(&index);
            let value = if in_carrier {
                Complex32::from_polar(30.0, std::f32::consts::TAU * index as f32 / 128.0)
            } else {
                Complex32::default()
            };
            if let Some(peak) = detector.push(value, index) {
                peaks.push(peak);
            }
        }
        assert_eq!(peaks.len(), 2);
        assert!(
            peaks
                .iter()
                .all(|peak| (peak.cfo_hz - 15_625.0).abs() < 1.0)
        );
    }

    #[test]
    fn only_a_complete_s1g_beacon_header_yields_an_identity() {
        let mut beacon = [0; 19];
        beacon[0] = 0x1c;
        let address = [2, 3, 5, 7, 11, 13];
        beacon[4..10].copy_from_slice(&address);
        beacon[15..].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        assert_eq!(beacon_fields(&beacon), Some((address, 0x1234_5678)));
        assert_eq!(beacon_fields(&beacon[..18]), None);
        beacon[0] = 0x0c;
        assert_eq!(beacon_fields(&beacon), None);
    }
}
