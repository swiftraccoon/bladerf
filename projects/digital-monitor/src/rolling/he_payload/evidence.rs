//! Verify bounded artifacts from the HE20 SU file replay worker.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};

use crate::report;

const RECEIVER_REVISION: &str = "00f1fed7ed8984cfc89360fd7ffbe19ba6c36a5b";
const SAMPLE_RATE_HZ: u64 = 20_000_000;
const MAX_SAMPLES: u64 = 200_000_000;
const MAX_FRAMES: u64 = 65_536;
const MAX_HEADERS: u64 = 131_072;
const HE_SU_PREFIX_SAMPLES: u64 = 640;
const REPLAY_REPORT_BYTES: u64 = 8 * 1024;
const PACKET_SUMMARY_BYTES: u64 = 1024 * 1024;
const FRAME_SPAN_BYTES: u64 = 2 * 1024 * 1024;
const PCAP_BYTES: u64 = 32 * 1024 * 1024;
const PCAP_HEADER_BYTES: u64 = 24;
const PCAP_RECORD_OVERHEAD_BYTES: u64 = 16 + 9;
const FCS_BYTES: u64 = 4;
const MPDU_BYTES: u64 = 16_383;

pub(super) struct Expected<'a> {
    pub(super) samples: u64,
    pub(super) center_frequency_hz: u64,
    pub(super) capture_frequency_hz: u64,
    pub(super) double_q11: bool,
    pub(super) decoder_sha256: &'a str,
}

#[derive(Serialize)]
pub(super) struct Decoded {
    pub(super) candidate_frames: u64,
    pub(super) validated_frames: u64,
    he_replay_sha256: String,
    packet_summary_sha256: String,
    frame_spans_sha256: String,
    candidate_pcap_sha256: String,
    validated_pcap_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    schema_version: u64,
    operation: String,
    reference_receiver_revision: String,
    iq_format: String,
    center_frequency_hz: u64,
    sample_rate_hz: u64,
    input_samples: u64,
    processed_samples: u64,
    sample_limit: u64,
    staged_cs8_sha256: String,
    #[serde(default, deserialize_with = "present")]
    capture_frequency_hz: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    frequency_shift_hz: Option<i64>,
    #[serde(default, deserialize_with = "present")]
    translation_filter: Option<String>,
    #[serde(default, deserialize_with = "present")]
    translation_clipped_samples: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    q11_gain: Option<Gain>,
    receiver: Receiver,
    he_su_frames: u64,
    total_frames: u64,
    he_su_headers: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Gain {
    factor: u64,
    clipped_components: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receiver {
    executable_sha256: String,
    records_sha256: String,
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn is_sha256(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Replay {
    fn verify(&self, expected: &Expected<'_>) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.operation == "offline_wifi_he20_su_payload_decode"
                && self.reference_receiver_revision == RECEIVER_REVISION
                && self.iq_format == "sc16-q11"
                && self.center_frequency_hz == expected.center_frequency_hz
                && self.sample_rate_hz == SAMPLE_RATE_HZ
                && self.input_samples == expected.samples
                && self.processed_samples == expected.samples
                && self.sample_limit == expected.samples,
            "HE payload replay terminal differs from the requested decode extent or tuning"
        );
        ensure!(
            self.receiver.executable_sha256 == expected.decoder_sha256
                && is_sha256(&self.receiver.executable_sha256)
                && is_sha256(&self.receiver.records_sha256)
                && is_sha256(&self.staged_cs8_sha256),
            "HE payload replay terminal differs from the pinned receiver or has invalid digests"
        );
        ensure!(
            self.he_su_frames <= self.total_frames
                && self.total_frames <= MAX_FRAMES
                && self.he_su_headers <= MAX_HEADERS
                && (self.he_su_frames == 0 || self.he_su_headers > 0),
            "HE payload replay terminal has inconsistent frame or header counts"
        );
        let distance = expected
            .capture_frequency_hz
            .abs_diff(expected.center_frequency_hz);
        ensure!(
            distance <= 5_000_000,
            "HE payload replay shift exceeds decoder bound"
        );
        if distance == 0 {
            ensure!(
                self.capture_frequency_hz.is_none()
                    && self.frequency_shift_hz.is_none()
                    && self.translation_filter.is_none()
                    && self.translation_clipped_samples.is_none(),
                "HE payload replay reports an unrequested translation"
            );
        } else {
            let shift = if expected.capture_frequency_hz < expected.center_frequency_hz {
                -(distance as i64)
            } else {
                distance as i64
            };
            ensure!(
                self.capture_frequency_hz == Some(expected.capture_frequency_hz)
                    && self.frequency_shift_hz == Some(shift)
                    && self.translation_filter.as_deref()
                        == (distance > 1_000_000).then_some("anti-wrap-kaiser-257")
                    && self
                        .translation_clipped_samples
                        .is_some_and(|clipped| clipped <= expected.samples),
                "HE payload replay translation differs from the requested preprocessing"
            );
        }
        ensure!(
            match &self.q11_gain {
                Some(gain) =>
                    expected.double_q11
                        && gain.factor == 2
                        && gain.clipped_components <= 2 * expected.samples,
                None => !expected.double_q11,
            },
            "HE payload replay Q11 gain differs from the requested preprocessing"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
struct Summary {
    schema_version: u64,
    operation: String,
    input_linktype: u64,
    total_records: u64,
    validated_frames: u64,
    malformed_frames: u64,
    invalid_fcs_frames: u64,
    unknown_fcs_frames: u64,
    absent_fcs_frames: u64,
    truncated_records: u64,
    records_exceeding_snaplen: u64,
    upstream_bad_fcs_claims: u64,
}

impl Summary {
    fn verify(&self, candidate_frames: u64) -> Result<()> {
        ensure!(
            self.schema_version == 4
                && self.operation == "inspect_wifi_packets"
                && self.input_linktype == 127
                && self.total_records == candidate_frames
                && self.validated_frames.checked_add(self.malformed_frames)
                    == Some(self.total_records)
                && self.invalid_fcs_frames == 0
                && self.unknown_fcs_frames == 0
                && self.absent_fcs_frames == 0
                && self.truncated_records == 0
                && self.records_exceeding_snaplen == 0
                && self.upstream_bad_fcs_claims == 0,
            "HE payload replay packet summary does not close candidate and validation accounting"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameSpans {
    schema_version: u64,
    sample_rate_hz: u64,
    input_samples: u64,
    processed_samples: u64,
    spans: Vec<[u64; 2]>,
}

impl FrameSpans {
    fn verify(self, samples: u64, candidate_frames: u64) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && self.sample_rate_hz == SAMPLE_RATE_HZ
                && self.input_samples == samples
                && self.processed_samples == samples
                && self.spans.len() as u64 == candidate_frames
                && self.spans.iter().all(|&[start, end]| {
                    end <= samples
                        && start
                            .checked_add(HE_SU_PREFIX_SAMPLES)
                            .is_some_and(|prefix_end| prefix_end <= end)
                }),
            "HE payload replay frame spans differ from candidate count or sample extent"
        );
        Ok(())
    }
}

fn hash_pcap(path: &Path, records: u64) -> Result<report::FileDigest> {
    let minimum = PCAP_HEADER_BYTES + records * (PCAP_RECORD_OVERHEAD_BYTES + FCS_BYTES);
    let maximum = PCAP_HEADER_BYTES + records * (PCAP_RECORD_OVERHEAD_BYTES + MPDU_BYTES);
    let digest = report::hash_direct_regular_file(path, maximum.min(PCAP_BYTES))?;
    ensure!(
        digest.bytes >= minimum,
        "HE payload replay PCAP is shorter than its reported record count"
    );
    Ok(digest)
}

pub(super) fn verify(directory: &Path, expected: Expected<'_>) -> Result<Decoded> {
    ensure!(
        (1..=MAX_SAMPLES).contains(&expected.samples)
            && expected.center_frequency_hz > 0
            && expected.capture_frequency_hz > 0,
        "invalid expected HE payload replay extent or tuning"
    );
    let (replay, he_replay_sha256) = report::read_json_with_sha256_limit(
        &directory.join("he-replay.json"),
        REPLAY_REPORT_BYTES,
    )?;
    let replay: Replay =
        serde_json::from_value(replay).context("parse HE payload replay terminal")?;
    replay.verify(&expected)?;
    let candidate_frames = replay.he_su_frames;

    let (summary, packet_summary_sha256) =
        report::read_json_with_sha256_limit(&directory.join("summary.json"), PACKET_SUMMARY_BYTES)?;
    let summary: Summary =
        serde_json::from_value(summary).context("parse HE payload packet summary")?;
    summary.verify(candidate_frames)?;
    let validated_frames = summary.validated_frames;

    let (spans, frame_spans_sha256) = report::read_json_with_sha256_limit(
        &directory.join("candidates.frame-spans.json"),
        FRAME_SPAN_BYTES,
    )?;
    let spans: FrameSpans =
        serde_json::from_value(spans).context("parse HE payload frame spans")?;
    spans.verify(expected.samples, candidate_frames)?;

    let candidate = hash_pcap(&directory.join("candidates.pcap"), candidate_frames)?;
    let validated = hash_pcap(&directory.join("validated.pcap"), validated_frames)?;
    ensure!(
        if candidate_frames == validated_frames {
            candidate == validated
        } else {
            validated.bytes < candidate.bytes
        },
        "HE payload replay PCAP artifacts differ from the inspector's accepted subset"
    );
    Ok(Decoded {
        candidate_frames,
        validated_frames,
        he_replay_sha256,
        packet_summary_sha256,
        frame_spans_sha256,
        candidate_pcap_sha256: candidate.sha256,
        validated_pcap_sha256: validated.sha256,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use serde_json::{Value, json};

    use super::*;

    const SAMPLES: u64 = 1_000;
    const CENTER: u64 = 2_437_000_000;
    const DECODER: &str = "7843085ce239726aa60c6edb88f38b4f128c1f7ca808931526f3d8671e88a404";

    fn expected() -> Expected<'static> {
        Expected {
            samples: SAMPLES,
            center_frequency_hz: CENTER,
            capture_frequency_hz: CENTER,
            double_q11: false,
            decoder_sha256: DECODER,
        }
    }

    fn replay(candidates: u64) -> Value {
        json!({
            "schema_version": 1,
            "operation": "offline_wifi_he20_su_payload_decode",
            "reference_receiver_revision": RECEIVER_REVISION,
            "iq_format": "sc16-q11",
            "center_frequency_hz": CENTER,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "input_samples": SAMPLES,
            "processed_samples": SAMPLES,
            "sample_limit": SAMPLES,
            "staged_cs8_sha256": "a".repeat(64),
            "receiver": {"executable_sha256": DECODER, "records_sha256": "b".repeat(64)},
            "he_su_frames": candidates,
            "total_frames": candidates + 3,
            "he_su_headers": 1
        })
    }

    fn json_file(directory: &Path, file: &str, value: &Value) {
        fs::write(directory.join(file), serde_json::to_vec(value).unwrap()).unwrap();
    }

    fn pcap(records: u64) -> Vec<u8> {
        let mut bytes = Vec::from(*b"\x4d\x3c\xb2\xa1\x02\0\x04\0");
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&65_536u32.to_le_bytes());
        bytes.extend_from_slice(&127u32.to_le_bytes());
        for _ in 0..records {
            bytes.extend_from_slice(&[0; 8]);
            bytes.extend_from_slice(&15u32.to_le_bytes());
            bytes.extend_from_slice(&15u32.to_le_bytes());
            bytes.extend_from_slice(&[0, 0, 9, 0, 2, 0, 0, 0, 0x10]);
            bytes.extend_from_slice(&[0; 6]);
        }
        bytes
    }

    fn fixture(directory: &Path, candidates: u64, validated: u64) {
        json_file(directory, "he-replay.json", &replay(candidates));
        json_file(
            directory,
            "summary.json",
            &json!({
                "schema_version": 4, "operation": "inspect_wifi_packets",
                "input_linktype": 127, "total_records": candidates,
                "validated_frames": validated, "malformed_frames": candidates - validated,
                "invalid_fcs_frames": 0, "unknown_fcs_frames": 0, "absent_fcs_frames": 0,
                "truncated_records": 0, "records_exceeding_snaplen": 0,
                "upstream_bad_fcs_claims": 0
            }),
        );
        json_file(
            directory,
            "candidates.frame-spans.json",
            &json!({
                "schema_version": 1, "sample_rate_hz": SAMPLE_RATE_HZ,
                "input_samples": SAMPLES, "processed_samples": SAMPLES,
                "spans": vec![[37, 900]; candidates as usize]
            }),
        );
        fs::write(directory.join("candidates.pcap"), pcap(candidates)).unwrap();
        fs::write(directory.join("validated.pcap"), pcap(validated)).unwrap();
    }

    #[test]
    fn verifies_empty_complete_and_filtered_inspector_outputs() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        for (candidate, validated) in [(0, 0), (2, 2), (2, 1)] {
            fixture(directory, candidate, validated);
            let verified = verify(directory, expected()).unwrap();
            assert_eq!(verified.candidate_frames, candidate);
            assert_eq!(verified.validated_frames, validated);
            let proof = serde_json::to_value(verified).unwrap();
            for name in [
                "he_replay_sha256",
                "packet_summary_sha256",
                "frame_spans_sha256",
                "candidate_pcap_sha256",
                "validated_pcap_sha256",
            ] {
                assert!(proof[name].as_str().is_some_and(is_sha256), "{name}");
            }
        }
    }

    #[test]
    fn rejects_mismatched_terminal_and_preprocessing() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        fixture(directory, 1, 1);
        for (field, value) in [
            ("reference_receiver_revision", json!("different")),
            ("iq_format", json!("cs8")),
            ("processed_samples", json!(SAMPLES - 1)),
            ("sample_limit", json!(SAMPLES + 1)),
            ("he_su_frames", json!(99)),
            ("he_su_headers", json!(0)),
            ("staged_cs8_sha256", json!("bad")),
            ("frequency_shift_hz", json!(0)),
            ("capture_frequency_hz", json!(null)),
            ("q11_gain", json!({"factor":2,"clipped_components":0})),
        ] {
            let mut terminal = replay(1);
            terminal[field] = value;
            json_file(directory, "he-replay.json", &terminal);
            assert!(verify(directory, expected()).is_err(), "{field}");
        }

        let mut terminal = replay(1);
        terminal["receiver"]["executable_sha256"] = json!("c".repeat(64));
        json_file(directory, "he-replay.json", &terminal);
        assert!(verify(directory, expected()).is_err());

        let mut query = expected();
        query.capture_frequency_hz = CENTER - 2_000_000;
        query.double_q11 = true;
        let mut terminal = replay(1);
        terminal["capture_frequency_hz"] = json!(query.capture_frequency_hz);
        terminal["frequency_shift_hz"] = json!(-2_000_000);
        terminal["translation_filter"] = json!("anti-wrap-kaiser-257");
        terminal["translation_clipped_samples"] = json!(SAMPLES);
        terminal["q11_gain"] = json!({"factor":2,"clipped_components":SAMPLES * 2});
        json_file(directory, "he-replay.json", &terminal);
        assert!(verify(directory, query).is_ok());
        for (field, value) in [
            ("frequency_shift_hz", json!(2_000_000)),
            ("translation_filter", json!("wrong")),
            ("translation_clipped_samples", json!(SAMPLES + 1)),
            (
                "q11_gain",
                json!({"factor":2,"clipped_components":SAMPLES * 2 + 1}),
            ),
        ] {
            let mut altered = terminal.clone();
            altered[field] = value;
            json_file(directory, "he-replay.json", &altered);
            assert!(
                verify(
                    directory,
                    Expected {
                        capture_frequency_hz: CENTER - 2_000_000,
                        double_q11: true,
                        ..expected()
                    }
                )
                .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_wrong_summary_spans_pcap_extent_and_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        fixture(directory, 1, 1);

        let summary_path = directory.join("summary.json");
        let mut summary: Value = serde_json::from_slice(&fs::read(summary_path).unwrap()).unwrap();
        summary["total_records"] = json!(2);
        json_file(directory, "summary.json", &summary);
        assert!(verify(directory, expected()).is_err());

        for span in [[37, 676], [0, SAMPLES + 1], [u64::MAX, 0]] {
            fixture(directory, 1, 1);
            let path = directory.join("candidates.frame-spans.json");
            let mut spans: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            spans["spans"] = json!([span]);
            json_file(directory, "candidates.frame-spans.json", &spans);
            assert!(verify(directory, expected()).is_err(), "{span:?}");
        }

        fixture(directory, 1, 1);
        let validated = directory.join("validated.pcap");
        fs::write(&validated, pcap(0)).unwrap();
        assert!(verify(directory, expected()).is_err());

        fs::write(&validated, pcap(1).into_iter().rev().collect::<Vec<_>>()).unwrap();
        assert!(verify(directory, expected()).is_err());

        fs::remove_file(&validated).unwrap();
        symlink(directory.join("candidates.pcap"), validated).unwrap();
        assert!(verify(directory, expected()).is_err());
    }
}
