//! Validate Wi-Fi decoder header evidence and locate it on the replay sample grid.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{EvidenceFile, SampleOwnership, WifiHtMode, WifiHtObservation};
use crate::report;

const EVENT_LIMIT: usize = 4_096;
const REPORT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HtEvent {
    preamble_sample_index: u64,
    mcs: u8,
    psdu_bytes: u16,
    channel_width_40_mhz: bool,
    ldpc: bool,
    short_guard_interval: bool,
    aggregation: bool,
    stbc: u8,
    extension_spatial_streams: u8,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HeSuEvent {
    preamble_sample_index: u64,
    mcs: u8,
    space_time_streams: u8,
    #[serde(rename = "ldpc")]
    _ldpc: bool,
    guard_interval_ns: u16,
    bss_color: u8,
}

pub(super) trait SignalEvent: DeserializeOwned {
    const LABEL: &'static str;
    const FILE: &'static str;
    const OPERATION: &'static str;
    const DIAGNOSTIC: &'static str;
    const COMPLETION_OFFSET: u64;
    const REQUIRE_EXACT_TERMINAL_COUNT: bool;

    fn preamble(&self) -> u64;
    fn valid_fields(&self) -> bool;

    fn ht_fields(&self) -> Option<(WifiHtMode, u16)> {
        None
    }
}

impl SignalEvent for HtEvent {
    const LABEL: &'static str = "HT-SIG";
    const FILE: &'static str = "candidates.ht-signal-events.json";
    const OPERATION: &'static str = "offline_wifi_ht_signal_events";
    const DIAGNOSTIC: &'static str = "ht-signal";
    const COMPLETION_OFFSET: u64 = 0;
    const REQUIRE_EXACT_TERMINAL_COUNT: bool = false;

    fn preamble(&self) -> u64 {
        self.preamble_sample_index
    }

    fn valid_fields(&self) -> bool {
        self.mcs <= 127 && self.stbc <= 3 && self.extension_spatial_streams <= 3
    }

    fn ht_fields(&self) -> Option<(WifiHtMode, u16)> {
        Some((
            WifiHtMode {
                mcs: self.mcs,
                channel_width_40_mhz: self.channel_width_40_mhz,
                ldpc: self.ldpc,
                short_guard_interval: self.short_guard_interval,
                stbc: self.stbc,
                aggregation: self.aggregation,
                extension_spatial_streams: self.extension_spatial_streams,
            },
            self.psdu_bytes,
        ))
    }
}

impl SignalEvent for HeSuEvent {
    const LABEL: &'static str = "HE-SIG-A";
    const FILE: &'static str = "candidates.he-su-signal-events.json";
    const OPERATION: &'static str = "offline_wifi_he_su_signal_events";
    const DIAGNOSTIC: &'static str = "he-su-signal";
    // L-STF/L-LTF plus L-SIG, RL-SIG and two HE-SIG-A symbols at 20 MSps.
    const COMPLETION_OFFSET: u64 = 639;
    const REQUIRE_EXACT_TERMINAL_COUNT: bool = true;

    fn preamble(&self) -> u64 {
        self.preamble_sample_index
    }

    fn valid_fields(&self) -> bool {
        self.mcs <= 11
            && (1..=8).contains(&self.space_time_streams)
            && matches!(self.guard_interval_ns, 800 | 1600 | 3200)
            && self.bss_color <= 63
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventReport<E> {
    schema_version: u32,
    operation: String,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    input_samples: u64,
    processed_samples: u64,
    diagnostic_batches_at_capacity: u64,
    event_limit: usize,
    events_over_limit: u64,
    events: Vec<E>,
}

#[derive(Serialize)]
pub(super) struct WifiSignalEvidence {
    // Counts include repeated diagnostic observations at the same preamble.
    observed_events: usize,
    owned_events: usize,
    events_over_limit: u64,
    diagnostic_batches_at_capacity: u64,
    #[serde(skip)]
    preambles: Vec<u64>,
    #[serde(skip)]
    completion_offset: u64,
    #[serde(skip)]
    ht_headers: Vec<(u64, WifiHtMode, u16)>,
}

impl WifiSignalEvidence {
    pub(super) fn preambles(&self) -> &[u64] {
        &self.preambles
    }

    pub(super) fn owned_events(&self) -> usize {
        self.owned_events
    }

    pub(super) fn at_capacity(&self) -> bool {
        self.events_over_limit != 0 || self.diagnostic_batches_at_capacity != 0
    }

    pub(super) fn ht_observations(
        &self,
        frame_spans: &[Range<u64>],
        ownership: Option<SampleOwnership>,
    ) -> Result<Vec<WifiHtObservation>> {
        let mut headers = BTreeMap::new();
        for &(start, mode, psdu_bytes) in &self.ht_headers {
            let fields = (mode, psdu_bytes);
            if let Some(previous) = headers.insert(start, fields) {
                ensure!(
                    previous == fields,
                    "conflicting Wi-Fi HT-SIG events at sample {start}"
                );
            }
        }
        let mut observations: Vec<_> = headers
            .into_iter()
            .filter(|(start, _)| ownership.is_none_or(|window| window.owns(*start)))
            .map(|(preamble_sample_index, (mode, _))| WifiHtObservation {
                preamble_sample_index,
                mode,
                validated_frame: false,
            })
            .collect();
        if observations.is_empty() {
            return Ok(observations);
        }
        for span in frame_spans {
            if let Ok(index) = observations
                .binary_search_by_key(&span.start, |observation| observation.preamble_sample_index)
            {
                observations[index].validated_frame = true;
            }
        }
        Ok(observations)
    }

    pub(super) fn count_owned(&mut self, window: SampleOwnership) {
        self.owned_events = self
            .preambles
            .iter()
            .filter(|&&start| window.owns(start + self.completion_offset))
            .count();
    }
}

pub(super) fn read<E: SignalEvent>(
    directory: &Path,
    decode: &Value,
    decoded_start: u64,
) -> Result<(WifiSignalEvidence, EvidenceFile)> {
    let path = directory.join(E::FILE);
    let (value, file) = EvidenceFile::read_json(path, REPORT_BYTES)?;
    let events: EventReport<E> = serde_json::from_value(value)
        .with_context(|| format!("parse Wi-Fi {} events", E::LABEL))?;
    Ok((events.verify(decode, decoded_start)?, file))
}

impl<E: SignalEvent> EventReport<E> {
    fn verify(self, decode: &Value, decoded_start: u64) -> Result<WifiSignalEvidence> {
        ensure!(
            self.schema_version == 1
                && self.operation == E::OPERATION
                && self.center_frequency_hz == report::required_u64(decode, "center_frequency_hz")?
                && self.sample_rate_hz == report::required_u64(decode, "sample_rate_hz")?
                && self.input_samples == report::required_u64(decode, "input_samples")?
                && self.processed_samples == report::required_u64(decode, "processed_samples")?,
            "Wi-Fi {} events differ from the complete decode extent or tuning",
            E::LABEL
        );
        ensure!(
            self.diagnostic_batches_at_capacity
                == report::required_u64(decode, "diagnostic_batches_at_capacity")?,
            "Wi-Fi {} diagnostic capacity count differs from the terminal decode",
            E::LABEL
        );

        let observed = self.events.len();
        ensure!(
            self.event_limit == EVENT_LIMIT
                && observed <= EVENT_LIMIT
                && (self.events_over_limit == 0 || observed == EVENT_LIMIT),
            "Wi-Fi {} events exceed or misstate their report capacity",
            E::LABEL
        );
        let total_observed = self
            .events_over_limit
            .checked_add(observed as u64)
            .with_context(|| format!("Wi-Fi {} observation count overflow", E::LABEL))?;
        let diagnostics = decode.get("diagnostic_counts");
        ensure!(
            !E::REQUIRE_EXACT_TERMINAL_COUNT || diagnostics.is_some(),
            "Wi-Fi {} events lack terminal diagnostic counts",
            E::LABEL
        );
        if let Some(counts) = diagnostics {
            let counts = counts
                .as_object()
                .context("Wi-Fi terminal diagnostics are not an object")?;
            let terminal_count = counts.get(E::DIAGNOSTIC).map_or(Some(0), Value::as_u64);
            ensure!(
                terminal_count.is_some_and(|count| {
                    total_observed == count
                        || (!E::REQUIRE_EXACT_TERMINAL_COUNT && total_observed < count)
                }),
                "Wi-Fi {} observations differ from the terminal diagnostic count",
                E::LABEL
            );
        }

        decoded_start
            .checked_add(self.processed_samples)
            .with_context(|| format!("Wi-Fi {} decode extent overflow", E::LABEL))?;
        let mut preambles = Vec::with_capacity(observed);
        let mut ht_headers = Vec::new();
        for event in self.events {
            let preamble = event.preamble();
            ensure!(
                event.valid_fields()
                    && preamble
                        .checked_add(E::COMPLETION_OFFSET)
                        .is_some_and(|completion| completion < self.processed_samples),
                "invalid Wi-Fi {} event fields or sample extent",
                E::LABEL
            );
            let start = decoded_start + preamble;
            preambles.push(start);
            if let Some((mode, psdu_bytes)) = event.ht_fields() {
                ht_headers.push((start, mode, psdu_bytes));
            }
        }
        Ok(WifiSignalEvidence {
            observed_events: observed,
            owned_events: observed,
            events_over_limit: self.events_over_limit,
            diagnostic_batches_at_capacity: self.diagnostic_batches_at_capacity,
            preambles,
            completion_offset: E::COMPLETION_OFFSET,
            ht_headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn decode() -> Value {
        json!({
            "center_frequency_hz": 2_437_000_000u64,
            "sample_rate_hz": 20_000_000,
            "input_samples": 200,
            "processed_samples": 200,
            "diagnostic_batches_at_capacity": 3,
            "diagnostic_counts": {"ht-signal": 4100},
        })
    }

    fn event(sample: u64) -> Value {
        json!({
            "preamble_sample_index": sample,
            "mcs": 127,
            "psdu_bytes": 0,
            "channel_width_40_mhz": true,
            "ldpc": false,
            "short_guard_interval": true,
            "aggregation": true,
            "stbc": 3,
            "extension_spatial_streams": 3,
        })
    }

    fn report(events: Vec<Value>) -> Value {
        json!({
            "schema_version": 1,
            "operation": "offline_wifi_ht_signal_events",
            "center_frequency_hz": 2_437_000_000u64,
            "sample_rate_hz": 20_000_000,
            "input_samples": 200,
            "processed_samples": 200,
            "diagnostic_batches_at_capacity": 3,
            "event_limit": EVENT_LIMIT,
            "events_over_limit": 0,
            "events": events,
        })
    }

    fn verify(report: Value, decoded_start: u64) -> Result<WifiSignalEvidence> {
        serde_json::from_value::<EventReport<HtEvent>>(report)?.verify(&decode(), decoded_start)
    }

    #[test]
    fn preserves_observations_and_partitions_overlapping_input_by_start() {
        let mut evidence = verify(
            report(vec![event(50), event(20), event(50), event(51)]),
            1_000,
        )
        .unwrap();
        assert_eq!(evidence.preambles(), [1_050, 1_020, 1_050, 1_051]);
        evidence.count_owned(SampleOwnership {
            decoded_start_sample: 1_000,
            owned_start_sample: 1_050,
            owned_end_sample_exclusive: 1_051,
        });
        assert_eq!(evidence.preambles(), [1_050, 1_020, 1_050, 1_051]);
        assert_eq!(
            serde_json::to_value(evidence).unwrap(),
            json!({
                "observed_events": 4,
                "owned_events": 2,
                "events_over_limit": 0,
                "diagnostic_batches_at_capacity": 3,
            }),
        );
    }

    #[test]
    fn ht_modes_deduplicate_headers_and_match_only_exact_owned_frame_starts() {
        let mut matched = event(51);
        matched["mcs"] = json!(4);
        matched["ldpc"] = json!(true);
        let evidence = verify(
            report(vec![
                event(52),
                matched.clone(),
                event(50),
                event(49),
                event(50),
                matched,
            ]),
            1_000,
        )
        .unwrap();
        let ownership = SampleOwnership {
            decoded_start_sample: 1_000,
            owned_start_sample: 1_050,
            owned_end_sample_exclusive: 1_052,
        };
        let spans = [1_052..1_060, 1_049..1_052, 1_051..1_053, 1_051..1_059];
        let observations = evidence.ht_observations(&spans, Some(ownership)).unwrap();
        let mode = WifiHtMode {
            mcs: 127,
            channel_width_40_mhz: true,
            ldpc: false,
            short_guard_interval: true,
            stbc: 3,
            aggregation: true,
            extension_spatial_streams: 3,
        };
        assert_eq!(
            observations,
            [
                WifiHtObservation {
                    preamble_sample_index: 1_050,
                    mode,
                    validated_frame: false,
                },
                WifiHtObservation {
                    preamble_sample_index: 1_051,
                    mode: WifiHtMode {
                        mcs: 4,
                        ldpc: true,
                        ..mode
                    },
                    validated_frame: true,
                },
            ],
        );
        assert_eq!(evidence.ht_observations(&spans, None).unwrap().len(), 4);
    }

    #[test]
    fn ht_modes_reject_conflicting_headers_at_the_same_preamble() {
        for field in ["mcs", "psdu_bytes"] {
            let mut different = event(50);
            different[field] = json!(1);
            let evidence = verify(report(vec![event(50), different]), 1_000).unwrap();
            let error = evidence.ht_observations(&[], None).unwrap_err();
            assert!(error.to_string().contains("conflicting Wi-Fi HT-SIG"));
        }
    }

    #[test]
    fn rejects_mismatched_sidecar_and_out_of_range_events() {
        let valid = report(vec![event(199)]);
        assert!(verify(valid.clone(), u64::MAX - 199).is_err());
        for (field, value) in [
            ("schema_version", json!(2)),
            ("operation", json!("other")),
            ("center_frequency_hz", json!(2_435_000_000u64)),
            ("sample_rate_hz", json!(19_999_999)),
            ("input_samples", json!(199)),
            ("processed_samples", json!(199)),
            ("diagnostic_batches_at_capacity", json!(0)),
            ("event_limit", json!(4_095)),
            ("events_over_limit", json!(1)),
            ("unexpected", json!(1)),
        ] {
            let mut altered = valid.clone();
            altered[field] = value;
            assert!(verify(altered, 0).is_err(), "{field}");
        }
        for (field, value) in [
            ("preamble_sample_index", json!(200)),
            ("mcs", json!(128)),
            ("psdu_bytes", json!(65_536)),
            ("stbc", json!(4)),
            ("extension_spatial_streams", json!(4)),
            ("ldpc", json!(0)),
            ("unexpected", json!(true)),
        ] {
            let mut altered = valid.clone();
            altered["events"][0][field] = value;
            assert!(verify(altered, 0).is_err(), "{field}");
        }
    }

    #[test]
    fn reports_bounded_incomplete_observations() {
        let mut sidecar = report(vec![event(20); EVENT_LIMIT]);
        sidecar["events_over_limit"] = json!(4);
        let evidence = verify(sidecar.clone(), 0).unwrap();
        assert_eq!(evidence.observed_events, EVENT_LIMIT);
        assert_eq!(evidence.owned_events, EVENT_LIMIT);
        assert_eq!(evidence.events_over_limit, 4);
        assert!(evidence.at_capacity());
        sidecar["events_over_limit"] = json!(5);
        assert!(verify(sidecar.clone(), 0).is_err());
        sidecar["events_over_limit"] = json!(u64::MAX);
        assert!(verify(sidecar, 0).is_err());
    }

    #[test]
    fn ht_capacity_includes_native_diagnostics_even_without_sidecar_overflow() {
        let mut sidecar = report(vec![event(20)]);
        assert!(verify(sidecar.clone(), 0).unwrap().at_capacity());
        sidecar["diagnostic_batches_at_capacity"] = json!(0);
        let mut terminal = decode();
        terminal["diagnostic_batches_at_capacity"] = json!(0);
        let evidence = serde_json::from_value::<EventReport<HtEvent>>(sidecar)
            .unwrap()
            .verify(&terminal, 0)
            .unwrap();
        assert!(!evidence.at_capacity());
    }

    fn he_event(sample: u64) -> Value {
        json!({
            "preamble_sample_index": sample,
            "mcs": 11,
            "space_time_streams": 8,
            "ldpc": true,
            "guard_interval_ns": 1600,
            "bss_color": 63,
        })
    }

    fn he_reports(samples: u64, events: Vec<Value>) -> (Value, Value) {
        let mut terminal = decode();
        terminal["input_samples"] = json!(samples);
        terminal["processed_samples"] = json!(samples);
        terminal["diagnostic_counts"] = json!({"he-su-signal": events.len()});
        let mut sidecar = report(events);
        sidecar["operation"] = json!(HeSuEvent::OPERATION);
        sidecar["input_samples"] = json!(samples);
        sidecar["processed_samples"] = json!(samples);
        (terminal, sidecar)
    }

    fn verify_he(
        terminal: &Value,
        sidecar: Value,
        decoded_start: u64,
    ) -> Result<WifiSignalEvidence> {
        serde_json::from_value::<EventReport<HeSuEvent>>(sidecar)?.verify(terminal, decoded_start)
    }

    #[test]
    fn he_signal_ownership_uses_header_completion_in_each_half_open_window() {
        let (terminal, sidecar) = he_reports(720, vec![he_event(80)]);
        let mut previous = verify_he(&terminal, sidecar, 1_000).unwrap();
        previous.count_owned(SampleOwnership {
            decoded_start_sample: 1_000,
            owned_start_sample: 1_000,
            owned_end_sample_exclusive: 1_720,
        });
        assert_eq!(previous.owned_events(), 1);

        // The successor sees the earlier preambles in context. An observation
        // ending before its owned interval remains owned by the previous replay.
        let (terminal, sidecar) = he_reports(
            720,
            vec![he_event(0), he_event(1), he_event(2), he_event(1)],
        );
        let mut next = verify_he(&terminal, sidecar, 1_080).unwrap();
        next.count_owned(SampleOwnership {
            decoded_start_sample: 1_080,
            owned_start_sample: 1_720,
            owned_end_sample_exclusive: 1_721,
        });
        assert_eq!(next.preambles(), [1_080, 1_081, 1_082, 1_081]);
        assert_eq!(next.owned_events(), 2);
        assert_eq!(
            serde_json::to_value(next).unwrap(),
            json!({
                "observed_events": 4,
                "owned_events": 2,
                "events_over_limit": 0,
                "diagnostic_batches_at_capacity": 3,
            }),
        );
    }

    #[test]
    fn he_signal_requires_complete_prefix_and_typed_fields() {
        let (terminal, valid) = he_reports(640, vec![he_event(0)]);
        assert!(verify_he(&terminal, valid.clone(), 1_000).is_ok());
        assert!(verify_he(&terminal, valid.clone(), u64::MAX - 639).is_err());
        for (field, value) in [
            ("preamble_sample_index", json!(1)),
            ("preamble_sample_index", json!(u64::MAX)),
            ("mcs", json!(12)),
            ("space_time_streams", json!(0)),
            ("space_time_streams", json!(9)),
            ("ldpc", json!(1)),
            ("guard_interval_ns", json!(400)),
            ("bss_color", json!(64)),
            ("unexpected", json!(true)),
        ] {
            let mut altered = valid.clone();
            altered["events"][0][field] = value;
            assert!(verify_he(&terminal, altered, 1_000).is_err(), "{field}");
        }
        let mut altered = valid.clone();
        altered["events"][0].as_object_mut().unwrap().remove("ldpc");
        assert!(verify_he(&terminal, altered, 1_000).is_err());
        let (terminal, sidecar) = he_reports(639, vec![he_event(0)]);
        assert!(verify_he(&terminal, sidecar, 1_000).is_err());
    }

    #[test]
    fn he_signal_binds_terminal_geometry_capacity_and_exact_count() {
        let (terminal, valid) = he_reports(640, vec![he_event(0)]);
        for (field, value) in [
            ("schema_version", json!(2)),
            ("operation", json!(HtEvent::OPERATION)),
            ("center_frequency_hz", json!(2_436_000_000u64)),
            ("sample_rate_hz", json!(19_999_999)),
            ("input_samples", json!(641)),
            ("processed_samples", json!(641)),
            ("diagnostic_batches_at_capacity", json!(1)),
            ("event_limit", json!(EVENT_LIMIT + 1)),
            ("events_over_limit", json!(1)),
        ] {
            let mut altered = valid.clone();
            altered[field] = value;
            assert!(verify_he(&terminal, altered, 0).is_err(), "{field}");
        }
        for count in [
            json!({}),
            json!({"he-su-signal": 2}),
            json!({"he-su-signal": "1"}),
            json!(null),
        ] {
            let mut altered = terminal.clone();
            altered["diagnostic_counts"] = count;
            assert!(verify_he(&altered, valid.clone(), 0).is_err());
        }
        let mut altered = terminal.clone();
        altered.as_object_mut().unwrap().remove("diagnostic_counts");
        assert!(verify_he(&altered, valid, 0).is_err());

        let (mut terminal, sidecar) = he_reports(640, vec![]);
        terminal["diagnostic_counts"] = json!({});
        assert!(verify_he(&terminal, sidecar, 0).is_ok());

        let (mut terminal, mut sidecar) = he_reports(640, vec![he_event(0); EVENT_LIMIT]);
        terminal["diagnostic_counts"]["he-su-signal"] = json!(EVENT_LIMIT + 2);
        sidecar["events_over_limit"] = json!(2);
        assert!(verify_he(&terminal, sidecar.clone(), 0).is_ok());
        sidecar["events_over_limit"] = json!(1);
        assert!(verify_he(&terminal, sidecar, 0).is_err());
    }

    #[test]
    fn he_signal_evidence_pins_the_read_file_and_rejects_a_symlink() {
        use std::{fs, os::unix::fs::symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(HeSuEvent::FILE);
        let (terminal, sidecar) = he_reports(640, vec![he_event(0)]);
        let bytes = serde_json::to_vec(&sidecar).unwrap();
        fs::write(&path, &bytes).unwrap();
        let (_, file) = read::<HeSuEvent>(directory.path(), &terminal, 0).unwrap();
        file.verify().unwrap();
        fs::write(&path, b"changed evidence").unwrap();
        assert!(file.verify().is_err());

        let replacement = directory.path().join("replacement.json");
        fs::write(&replacement, bytes).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(replacement, path).unwrap();
        assert!(read::<HeSuEvent>(directory.path(), &terminal, 0).is_err());
    }
}
