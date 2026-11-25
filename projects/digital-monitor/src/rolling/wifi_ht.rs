//! Count verified HT preambles and exact frame-start matches by header mode.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Serialize, Serializer, ser::SerializeSeq};

use crate::event_replay::replay::{WifiHtMode, WifiHtObservation};

const JSON_BYTES_PER_ROW: u64 = 512;

#[derive(Clone, Copy, Default, Serialize)]
struct Counts {
    headers: u64,
    matched: u64,
}

#[derive(Default, Serialize)]
pub(super) struct WifiHtModes {
    segments: u64,
    limited_segments: u64,
    #[serde(serialize_with = "serialize_modes")]
    modes: BTreeMap<WifiHtMode, Counts>,
}

impl WifiHtModes {
    pub(super) fn from_segment(observations: &[WifiHtObservation], limited: bool) -> Self {
        let mut report = Self {
            segments: 1,
            limited_segments: u64::from(limited),
            ..Self::default()
        };
        for observation in observations {
            let counts = report.modes.entry(observation.mode).or_default();
            counts.headers += 1;
            counts.matched += u64::from(observation.validated_frame);
        }
        report
    }

    pub(super) fn total<'a>(mut sources: impl Iterator<Item = &'a Self>) -> Result<Option<Self>> {
        let Some(first) = sources.next() else {
            return Ok(None);
        };
        let mut total = Self {
            segments: first.segments,
            limited_segments: first.limited_segments,
            modes: first.modes.clone(),
        };
        for source in sources {
            total.segments = total
                .segments
                .checked_add(source.segments)
                .context("Wi-Fi HT segment count overflow")?;
            total.limited_segments = total
                .limited_segments
                .checked_add(source.limited_segments)
                .context("Wi-Fi HT limited segment count overflow")?;
            for (&mode, counts) in &source.modes {
                let current = total.modes.entry(mode).or_default();
                current.headers = current
                    .headers
                    .checked_add(counts.headers)
                    .context("Wi-Fi HT header count overflow")?;
                current.matched = current
                    .matched
                    .checked_add(counts.matched)
                    .context("Wi-Fi HT match count overflow")?;
            }
        }
        Ok(Some(total))
    }

    /// Allow fixed-width counts and field names plus pretty-print whitespace.
    pub(super) fn json_budget(&self) -> u64 {
        JSON_BYTES_PER_ROW * (1 + self.modes.len() as u64)
    }
}

fn serialize_modes<S: Serializer>(
    modes: &BTreeMap<WifiHtMode, Counts>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Row<'a> {
        #[serde(flatten)]
        mode: &'a WifiHtMode,
        #[serde(flatten)]
        counts: &'a Counts,
    }

    let mut seq = serializer.serialize_seq(Some(modes.len()))?;
    for (mode, counts) in modes {
        seq.serialize_element(&Row { mode, counts })?;
    }
    seq.end()
}

#[cfg(test)]
#[test]
fn rollup_preserves_mode_counts_and_capacity_through_retirement() {
    let mode = WifiHtMode {
        mcs: 7,
        channel_width_40_mhz: false,
        ldpc: false,
        short_guard_interval: true,
        stbc: 1,
        aggregation: true,
        extension_spatial_streams: 0,
    };
    let header = |preamble_sample_index, mode, validated_frame| WifiHtObservation {
        preamble_sample_index,
        mode,
        validated_frame,
    };
    let retired = WifiHtModes::from_segment(&[header(10, mode, true)], false);
    let retained = WifiHtModes::from_segment(
        &[
            header(30, mode, false),
            header(40, WifiHtMode { mcs: 0, ..mode }, true),
        ],
        true,
    );
    let total = WifiHtModes::total([&retired, &retained].into_iter())
        .unwrap()
        .unwrap();
    let report = serde_json::to_value(&total).unwrap();
    assert_eq!(report["segments"], 2);
    assert_eq!(report["limited_segments"], 1);
    assert_eq!(report["modes"][0]["mcs"], 0);
    assert_eq!(report["modes"][0]["headers"], 1);
    assert_eq!(report["modes"][0]["matched"], 1);
    assert_eq!(report["modes"][1]["mcs"], 7);
    assert_eq!(report["modes"][1]["headers"], 2);
    assert_eq!(report["modes"][1]["matched"], 1);
    assert!(serde_json::to_vec_pretty(&total).unwrap().len() as u64 <= total.json_budget());
}
