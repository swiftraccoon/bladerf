//! Bounded offline statistics for validated profile-2 framed captures.
//!
//! Complete-file analysis is the default. An explicit DATA-record limit analyzes
//! only CONFIG plus the selected DATA prefix, and the report labels that narrower
//! validation and hash scope. Payload values remain raw signed FPGA transport
//! counts; this module does not open a radio or infer calibrated RF properties,
//! signal identity, occupancy, antenna response, or analog-ADC clipping.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::dsp::{self, Iq16};
use crate::transport_v2::{self, Data, InspectedFile};

const I16_COMPONENT_FULL_SCALE_POWER: f64 = 32768.0 * 32768.0;

#[derive(Debug, Serialize)]
struct ChannelReport {
    channel: usize,
    phase_word: u32,
    phase_word_hex: String,
    samples: u64,
    minimum_i_counts: i16,
    maximum_i_counts: i16,
    minimum_q_counts: i16,
    maximum_q_counts: i16,
    mean_i_counts: f64,
    mean_q_counts: f64,
    dc_magnitude_counts: f64,
    mean_power_counts_squared: f64,
    rms_magnitude_counts: f64,
    ac_power_counts_squared: f64,
    ac_rms_magnitude_counts: f64,
    mean_power_db_relative_to_i16_component_full_scale: Option<f64>,
    ac_power_db_relative_to_i16_component_full_scale: Option<f64>,
    mean_power_db_relative_to_strongest_channel: Option<f64>,
    dc_power_fraction_db: Option<f64>,
    peak_magnitude_counts: f64,
    zero_i_components: u64,
    nonzero_i_components: u64,
    zero_q_components: u64,
    nonzero_q_components: u64,
    zero_complex_samples: u64,
    nonzero_complex_samples: u64,
    numeric_limit_component_hits: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    mode: &'static str,
    selection_mode: &'static str,
    hash_scope: &'static str,
    input_extent_bytes: u64,
    available_data_record_slots_from_extent: u64,
    requested_max_data_records: Option<u64>,
    processed_records_including_config: u64,
    processed_data_records: u64,
    unprocessed_data_record_slots_from_extent: u64,
    processed_bytes: u64,
    processed_sha256: String,
    complete_file_validated: bool,
    format_version: u16,
    profile_id: u16,
    receiver: &'static str,
    configuration_epoch: u32,
    configuration_epoch_tag: u8,
    required_source_sample_rate_hz: u32,
    channel_sample_rate_hz: u32,
    phase_words: [u32; 4],
    phase_words_hex: [String; 4],
    first_source_index: u64,
    last_source_index: u64,
    selected_duration_seconds: f64,
    channels: Vec<ChannelReport>,
    statistics_note: &'static str,
    validation_note: &'static str,
    timing_note: &'static str,
    receiver_note: &'static str,
}

#[derive(Debug)]
pub(crate) struct Analysis {
    report: Report,
}

impl Analysis {
    pub(crate) fn data_records(&self) -> u64 {
        self.report.processed_data_records
    }

    pub(crate) fn receiver(&self) -> &'static str {
        self.report.receiver
    }
}

#[derive(Clone, Copy, Debug)]
struct Moments {
    samples: u64,
    sum_i: i128,
    sum_q: i128,
    sum_power: u128,
    peak_power: u64,
    minimum_i: i16,
    maximum_i: i16,
    minimum_q: i16,
    maximum_q: i16,
    zero_i_components: u64,
    zero_q_components: u64,
    zero_complex_samples: u64,
    numeric_limit_component_hits: u64,
}

impl Default for Moments {
    fn default() -> Self {
        Self {
            samples: 0,
            sum_i: 0,
            sum_q: 0,
            sum_power: 0,
            peak_power: 0,
            minimum_i: i16::MAX,
            maximum_i: i16::MIN,
            minimum_q: i16::MAX,
            maximum_q: i16::MIN,
            zero_i_components: 0,
            zero_q_components: 0,
            zero_complex_samples: 0,
            numeric_limit_component_hits: 0,
        }
    }
}

impl Moments {
    fn push(&mut self, sample: Iq16) -> Result<()> {
        let i = i64::from(sample.i);
        let q = i64::from(sample.q);
        let power = u64::try_from(i * i + q * q).context("profile-2 sample power")?;
        self.samples = self
            .samples
            .checked_add(1)
            .context("profile-2 analysis sample count overflow")?;
        self.sum_i = self
            .sum_i
            .checked_add(i128::from(sample.i))
            .context("profile-2 I sum overflow")?;
        self.sum_q = self
            .sum_q
            .checked_add(i128::from(sample.q))
            .context("profile-2 Q sum overflow")?;
        self.sum_power = self
            .sum_power
            .checked_add(u128::from(power))
            .context("profile-2 power sum overflow")?;
        self.peak_power = self.peak_power.max(power);
        self.minimum_i = self.minimum_i.min(sample.i);
        self.maximum_i = self.maximum_i.max(sample.i);
        self.minimum_q = self.minimum_q.min(sample.q);
        self.maximum_q = self.maximum_q.max(sample.q);
        self.zero_i_components = self
            .zero_i_components
            .checked_add(u64::from(sample.i == 0))
            .context("profile-2 I-zero count overflow")?;
        self.zero_q_components = self
            .zero_q_components
            .checked_add(u64::from(sample.q == 0))
            .context("profile-2 Q-zero count overflow")?;
        self.zero_complex_samples = self
            .zero_complex_samples
            .checked_add(u64::from(sample.i == 0 && sample.q == 0))
            .context("profile-2 complex-zero count overflow")?;
        let limit_hits = u64::from(matches!(sample.i, i16::MIN | i16::MAX))
            + u64::from(matches!(sample.q, i16::MIN | i16::MAX));
        self.numeric_limit_component_hits = self
            .numeric_limit_component_hits
            .checked_add(limit_hits)
            .context("profile-2 numeric-limit count overflow")?;
        Ok(())
    }

    fn values(self) -> Result<MomentValues> {
        ensure!(self.samples > 0, "profile-2 analysis channel is empty");
        let samples = self.samples as f64;
        let mean_i = self.sum_i as f64 / samples;
        let mean_q = self.sum_q as f64 / samples;
        let mean_power = self.sum_power as f64 / samples;
        let dc_power = mean_i * mean_i + mean_q * mean_q;
        let ac_power = (mean_power - dc_power).max(0.0);
        Ok(MomentValues {
            moments: self,
            mean_i,
            mean_q,
            mean_power,
            dc_power,
            ac_power,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct MomentValues {
    moments: Moments,
    mean_i: f64,
    mean_q: f64,
    mean_power: f64,
    dc_power: f64,
    ac_power: f64,
}

#[derive(Default)]
struct Accumulator {
    moments: [Moments; 4],
}

impl Accumulator {
    fn push(&mut self, data: Data) -> Result<()> {
        for (moments, sample) in self.moments.iter_mut().zip(data.samples) {
            moments.push(sample)?;
        }
        Ok(())
    }

    fn finish(self, inspected: InspectedFile) -> Result<Analysis> {
        let stream = inspected.stream;
        ensure!(
            self.moments
                .iter()
                .all(|moments| moments.samples == stream.data_records),
            "profile-2 channel statistics do not cover the selected DATA records"
        );
        let values = self
            .moments
            .map(Moments::values)
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let strongest_channel_power = values
            .iter()
            .map(|value| value.mean_power)
            .fold(0.0_f64, f64::max);
        let channels = values
            .into_iter()
            .enumerate()
            .map(|(channel, value)| {
                let samples = value.moments.samples;
                Ok(ChannelReport {
                    channel,
                    phase_word: stream.config.phase_words[channel],
                    phase_word_hex: format!("0x{:08x}", stream.config.phase_words[channel]),
                    samples,
                    minimum_i_counts: value.moments.minimum_i,
                    maximum_i_counts: value.moments.maximum_i,
                    minimum_q_counts: value.moments.minimum_q,
                    maximum_q_counts: value.moments.maximum_q,
                    mean_i_counts: value.mean_i,
                    mean_q_counts: value.mean_q,
                    dc_magnitude_counts: value.dc_power.sqrt(),
                    mean_power_counts_squared: value.mean_power,
                    rms_magnitude_counts: value.mean_power.sqrt(),
                    ac_power_counts_squared: value.ac_power,
                    ac_rms_magnitude_counts: value.ac_power.sqrt(),
                    mean_power_db_relative_to_i16_component_full_scale: power_db(
                        value.mean_power,
                        I16_COMPONENT_FULL_SCALE_POWER,
                    ),
                    ac_power_db_relative_to_i16_component_full_scale: power_db(
                        value.ac_power,
                        I16_COMPONENT_FULL_SCALE_POWER,
                    ),
                    mean_power_db_relative_to_strongest_channel: power_db(
                        value.mean_power,
                        strongest_channel_power,
                    ),
                    dc_power_fraction_db: power_db(value.dc_power, value.mean_power),
                    peak_magnitude_counts: (value.moments.peak_power as f64).sqrt(),
                    zero_i_components: value.moments.zero_i_components,
                    nonzero_i_components: samples
                        .checked_sub(value.moments.zero_i_components)
                        .context("profile-2 nonzero I count")?,
                    zero_q_components: value.moments.zero_q_components,
                    nonzero_q_components: samples
                        .checked_sub(value.moments.zero_q_components)
                        .context("profile-2 nonzero Q count")?,
                    zero_complex_samples: value.moments.zero_complex_samples,
                    nonzero_complex_samples: samples
                        .checked_sub(value.moments.zero_complex_samples)
                        .context("profile-2 nonzero complex count")?,
                    numeric_limit_component_hits: value.moments.numeric_limit_component_hits,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let processed_records_including_config = stream
            .data_records
            .checked_add(1)
            .context("profile-2 processed record count overflow")?;
        let expected_processed_bytes = processed_records_including_config
            .checked_mul(u64::try_from(transport_v2::RECORD_BYTES)?)
            .context("profile-2 processed byte count overflow")?;
        ensure!(
            stream.processed_bytes == expected_processed_bytes,
            "profile-2 processed extent disagrees with the record count"
        );
        let available_records_including_config = inspected
            .available_data_record_slots
            .checked_add(1)
            .context("profile-2 available record count overflow")?;
        let expected_input_extent = available_records_including_config
            .checked_mul(u64::try_from(transport_v2::RECORD_BYTES)?)
            .context("profile-2 input extent overflow")?;
        ensure!(
            inspected.input_extent_bytes == expected_input_extent,
            "profile-2 input extent disagrees with the available record count"
        );
        ensure!(
            inspected.complete_file_validated
                == (stream.data_records == inspected.available_data_record_slots),
            "profile-2 complete-file provenance disagrees with selected records"
        );
        let unprocessed_data_record_slots_from_extent = inspected
            .available_data_record_slots
            .checked_sub(stream.data_records)
            .context("profile-2 selected DATA count exceeds the input extent")?;
        let decimation = u32::try_from(dsp::CIC_DECIMATION)?;
        ensure!(
            stream
                .config
                .required_source_sample_rate_hz
                .is_multiple_of(decimation),
            "profile-2 source rate is not divisible by the decimation factor"
        );
        let channel_sample_rate_hz = stream.config.required_source_sample_rate_hz / decimation;
        let complete_file = inspected.complete_file_validated;
        Ok(Analysis {
            report: Report {
                schema_version: 1,
                mode: "offline_relative_profile2_analysis",
                selection_mode: if complete_file {
                    "complete_file"
                } else {
                    "data_prefix"
                },
                hash_scope: if complete_file {
                    "complete_file"
                } else {
                    "config_plus_selected_data_prefix"
                },
                input_extent_bytes: inspected.input_extent_bytes,
                available_data_record_slots_from_extent: inspected.available_data_record_slots,
                requested_max_data_records: inspected.requested_max_data_records,
                processed_records_including_config,
                processed_data_records: stream.data_records,
                unprocessed_data_record_slots_from_extent,
                processed_bytes: stream.processed_bytes,
                processed_sha256: stream.processed_sha256,
                complete_file_validated: complete_file,
                format_version: transport_v2::FORMAT_VERSION,
                profile_id: transport_v2::PROFILE_ID,
                receiver: if stream.config.status.rx2 {
                    "RX2"
                } else {
                    "RX1"
                },
                configuration_epoch: stream.config.epoch,
                configuration_epoch_tag: stream.config.status.epoch_tag,
                required_source_sample_rate_hz: stream.config.required_source_sample_rate_hz,
                channel_sample_rate_hz,
                phase_words: stream.config.phase_words,
                phase_words_hex: std::array::from_fn(|channel| {
                    format!("0x{:08x}", stream.config.phase_words[channel])
                }),
                first_source_index: stream.first_source_index,
                last_source_index: stream.last_source_index,
                selected_duration_seconds: stream.data_records as f64
                    / f64::from(channel_sample_rate_hz),
                channels,
                statistics_note: "Payload statistics are raw signed FPGA channel-bank transport counts. Log powers use 32768^2 counts^2 as the explicit 0 dB reference; simultaneous near-full-scale I and Q can approach +3.01 dB. Values are not dBm, RSSI, calibrated RF power, antenna response, channel occupancy, signal or station identity, or evidence that the analog ADC did not clip.",
                validation_note: "A complete_file result parsed and validated every record and hashes the complete file. A data_prefix result parsed only CONFIG plus the selected DATA prefix; available and unprocessed record-slot counts come from the stable file extent and do not claim that the suffix was parsed or valid. Descriptor metadata checks do not make an externally mutable file an immutable snapshot.",
                timing_note: "The channel sample rate and selected duration are derived from CONFIG's required 20 MHz source-rate contract and the profile's factor-20 decimation. They are not measured or timestamped elapsed time.",
                receiver_note: "The validated RX1/RX2 transport flag identifies the selected bladeRF input, not which connector is physically attached to an antenna or distribution path.",
            },
        })
    }
}

fn power_db(power: f64, reference: f64) -> Option<f64> {
    (power > 0.0 && reference > 0.0).then(|| 10.0 * (power / reference).log10())
}

/// Analyze every DATA record, or an explicit DATA prefix, with fixed memory.
pub(crate) fn analyze(
    path: &Path,
    max_data_records: Option<u64>,
    stop: &AtomicBool,
) -> Result<Analysis> {
    let mut accumulator = Accumulator::default();
    let inspected = transport_v2::inspect_file(
        path,
        max_data_records,
        stop,
        |data_ordinal, selected_data_records, data| {
            ensure!(
                data_ordinal < selected_data_records,
                "profile-2 DATA observer ordinal exceeds the selected extent"
            );
            accumulator.push(data)
        },
    )?;
    accumulator.finish(inspected)
}

/// Publish a synced terminal report without replacing an existing result.
///
/// A late write, synchronization, cancellation or publication failure can leave
/// `analysis.json.partial`, but `analysis.json` exists only after a complete
/// report has been synchronized and hard-link-published without replacement.
pub(crate) fn publish(directory: &Path, analysis: &Analysis, stop: &AtomicBool) -> Result<()> {
    let destination = directory.join("analysis.json");
    let stage = directory.join("analysis.json.partial");
    ensure!(
        !destination.exists() && !stage.exists(),
        "profile-2 analysis output already exists: {}",
        destination.display()
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 analysis publication interrupted before output creation"
    );
    let mut report = BufWriter::new(crate::new_file(&stage)?);
    serde_json::to_writer_pretty(&mut report, &analysis.report)?;
    report.write_all(b"\n")?;
    report.flush()?;
    report.get_ref().sync_all()?;
    drop(report);
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 analysis publication interrupted before terminal publication"
    );
    fs::hard_link(&stage, &destination).with_context(|| {
        format!(
            "publish complete profile-2 analysis output {}",
            destination.display()
        )
    })?;
    let _ = fs::remove_file(stage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use sha2::{Digest, Sha256};

    fn config(rx2: bool) -> [u8; transport_v2::RECORD_BYTES] {
        let mut bytes = [0u8; transport_v2::RECORD_BYTES];
        bytes[..2].copy_from_slice(&0x5246u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&0x4f42u16.to_le_bytes());
        let flags: u16 = 0x2000 | 0x0010 | 0x0001 | if rx2 { 0x0020 } else { 0 };
        bytes[4..6].copy_from_slice(&flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&transport_v2::PROFILE_ID.to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&transport_v2::SOURCE_SAMPLE_RATE_HZ.to_le_bytes());
        for (channel, word) in [0x0123_4567u32, 0x89ab_cdef, 0x8000_0001, 0xdead_beef]
            .into_iter()
            .enumerate()
        {
            bytes[16 + channel * 4..20 + channel * 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    fn data(ordinal: u64, samples: [Iq16; 4], rx2: bool) -> [u8; transport_v2::RECORD_BYTES] {
        let mut bytes = [0u8; transport_v2::RECORD_BYTES];
        bytes[..2].copy_from_slice(&0x5246u16.to_le_bytes());
        bytes[2..4].copy_from_slice(&0x4f42u16.to_le_bytes());
        let flags: u16 = 0x2000 | 0x0001 | if rx2 { 0x0020 } else { 0 };
        bytes[4..6].copy_from_slice(&flags.to_le_bytes());
        bytes[6..8].copy_from_slice(&transport_v2::PROFILE_ID.to_le_bytes());
        bytes[8..16].copy_from_slice(&(ordinal * 20 + 19).to_le_bytes());
        for (channel, sample) in samples.into_iter().enumerate() {
            bytes[16 + channel * 4..18 + channel * 4].copy_from_slice(&sample.i.to_le_bytes());
            bytes[18 + channel * 4..20 + channel * 4].copy_from_slice(&sample.q.to_le_bytes());
        }
        bytes
    }

    fn fixture(rx2: bool) -> Vec<u8> {
        let records = [
            data(
                0,
                [
                    Iq16 {
                        i: i16::MIN,
                        q: i16::MAX,
                    },
                    Iq16 { i: 3, q: 4 },
                    Iq16::default(),
                    Iq16 { i: -3, q: 4 },
                ],
                rx2,
            ),
            data(
                1,
                [
                    Iq16::default(),
                    Iq16 { i: 3, q: 4 },
                    Iq16::default(),
                    Iq16 { i: 3, q: -4 },
                ],
                rx2,
            ),
            data(
                2,
                [
                    Iq16 {
                        i: i16::MAX,
                        q: i16::MIN,
                    },
                    Iq16 { i: 3, q: 4 },
                    Iq16::default(),
                    Iq16::default(),
                ],
                rx2,
            ),
        ];
        config(rx2)
            .into_iter()
            .chain(records.into_iter().flatten())
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_analysis_freezes_lane_statistics_and_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("profile2.frames");
        let bytes = fixture(true);
        std::fs::write(&input, &bytes).unwrap();
        let analysis = analyze(&input, None, &AtomicBool::new(false)).unwrap();
        assert_eq!(analysis.data_records(), 3);
        assert_eq!(analysis.receiver(), "RX2");
        assert!(analysis.report.complete_file_validated);
        assert_eq!(
            analysis.report.processed_sha256,
            format!("{:x}", Sha256::digest(&bytes))
        );
        let report = serde_json::to_value(&analysis.report).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["selection_mode"], "complete_file");
        assert_eq!(report["hash_scope"], "complete_file");
        assert_eq!(report["input_extent_bytes"], 128);
        assert_eq!(report["available_data_record_slots_from_extent"], 3);
        assert!(report["requested_max_data_records"].is_null());
        assert_eq!(report["processed_records_including_config"], 4);
        assert_eq!(report["processed_data_records"], 3);
        assert_eq!(report["unprocessed_data_record_slots_from_extent"], 0);
        assert_eq!(report["processed_bytes"], 128);
        assert_eq!(report["first_source_index"], 19);
        assert_eq!(report["last_source_index"], 59);
        assert_eq!(report["configuration_epoch"], 1);
        assert_eq!(report["configuration_epoch_tag"], 1);
        assert_eq!(report["channel_sample_rate_hz"], 1_000_000);
        assert_eq!(report["selected_duration_seconds"], 0.000003);
        assert_eq!(report["phase_words_hex"][3], "0xdeadbeef");

        let channel0 = &report["channels"][0];
        assert_eq!(channel0["minimum_i_counts"], i16::MIN);
        assert_eq!(channel0["maximum_i_counts"], i16::MAX);
        assert_eq!(channel0["minimum_q_counts"], i16::MIN);
        assert_eq!(channel0["maximum_q_counts"], i16::MAX);
        assert!((channel0["mean_i_counts"].as_f64().unwrap() + 1.0 / 3.0).abs() < 1e-12);
        assert!((channel0["mean_q_counts"].as_f64().unwrap() + 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(channel0["zero_i_components"], 1);
        assert_eq!(channel0["nonzero_i_components"], 2);
        assert_eq!(channel0["zero_q_components"], 1);
        assert_eq!(channel0["nonzero_q_components"], 2);
        assert_eq!(channel0["zero_complex_samples"], 1);
        assert_eq!(channel0["nonzero_complex_samples"], 2);
        assert_eq!(channel0["numeric_limit_component_hits"], 4);
        assert!(channel0["peak_magnitude_counts"].as_f64().unwrap() > 46_000.0);

        let channel1 = &report["channels"][1];
        assert_eq!(channel1["mean_i_counts"], 3.0);
        assert_eq!(channel1["mean_q_counts"], 4.0);
        assert_eq!(channel1["mean_power_counts_squared"], 25.0);
        assert_eq!(channel1["rms_magnitude_counts"], 5.0);
        assert_eq!(channel1["ac_power_counts_squared"], 0.0);
        assert!(channel1["ac_power_db_relative_to_i16_component_full_scale"].is_null());

        let channel2 = &report["channels"][2];
        assert_eq!(channel2["zero_complex_samples"], 3);
        assert_eq!(channel2["nonzero_complex_samples"], 0);
        assert!(channel2["mean_power_db_relative_to_i16_component_full_scale"].is_null());

        let channel3 = &report["channels"][3];
        assert_eq!(channel3["minimum_i_counts"], -3);
        assert_eq!(channel3["maximum_i_counts"], 3);
        assert_eq!(channel3["minimum_q_counts"], -4);
        assert_eq!(channel3["maximum_q_counts"], 4);
        assert!(
            (channel3["mean_power_counts_squared"].as_f64().unwrap() - 50.0 / 3.0).abs() < 1e-12
        );
        assert_eq!(channel3["zero_complex_samples"], 1);
        assert!(channel3["ac_power_counts_squared"].as_f64().unwrap() > 0.0);
        assert!(channel3["ac_rms_magnitude_counts"].as_f64().unwrap() > 0.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prefix_scope_and_larger_limit_are_unambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("profile2.frames");
        let bytes = fixture(false);
        std::fs::write(&input, &bytes).unwrap();

        let prefix = analyze(&input, Some(2), &AtomicBool::new(false)).unwrap();
        let report = serde_json::to_value(&prefix.report).unwrap();
        assert_eq!(report["selection_mode"], "data_prefix");
        assert_eq!(report["hash_scope"], "config_plus_selected_data_prefix");
        assert_eq!(report["requested_max_data_records"], 2);
        assert_eq!(report["processed_data_records"], 2);
        assert_eq!(report["unprocessed_data_record_slots_from_extent"], 1);
        assert_eq!(report["processed_bytes"], 96);
        assert_eq!(report["last_source_index"], 39);
        assert_eq!(
            report["processed_sha256"],
            format!("{:x}", Sha256::digest(&bytes[..96]))
        );
        assert_eq!(report["complete_file_validated"], false);

        let oversized = analyze(&input, Some(10), &AtomicBool::new(false)).unwrap();
        let report = serde_json::to_value(&oversized.report).unwrap();
        assert_eq!(report["selection_mode"], "complete_file");
        assert_eq!(report["requested_max_data_records"], 10);
        assert_eq!(report["processed_data_records"], 3);
        assert_eq!(report["complete_file_validated"], true);

        let error = analyze(
            Path::new("/this/path/must/not/be/opened"),
            Some(0),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("positive"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn publication_is_terminal_private_collision_safe_and_cancellable() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("profile2.frames");
        std::fs::write(&input, fixture(false)).unwrap();
        let analysis = analyze(&input, None, &AtomicBool::new(false)).unwrap();

        let output = temp.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let stop = AtomicBool::new(false);
        publish(&output, &analysis, &stop).unwrap();
        let destination = output.join("analysis.json");
        assert!(destination.is_file());
        assert!(!output.join("analysis.json.partial").exists());
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let original = std::fs::read(&destination).unwrap();
        assert!(publish(&output, &analysis, &stop).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), original);

        let staged = temp.path().join("staged");
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(staged.join("analysis.json.partial"), b"preserve-stage").unwrap();
        assert!(publish(&staged, &analysis, &stop).is_err());
        assert!(!staged.join("analysis.json").exists());
        assert_eq!(
            std::fs::read(staged.join("analysis.json.partial")).unwrap(),
            b"preserve-stage"
        );

        let interrupted = temp.path().join("interrupted");
        std::fs::create_dir(&interrupted).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(publish(&interrupted, &analysis, &stop).is_err());
        assert_eq!(std::fs::read_dir(interrupted).unwrap().count(), 0);
    }
}
