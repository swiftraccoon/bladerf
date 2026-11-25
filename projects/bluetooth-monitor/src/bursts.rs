//! Bounded validation and aggregate reporting for Blue Dragon burst archives.
//!
//! A record is a 2 Msps channelized squelch burst or continuous-scan window. It
//! is not a decoded packet, continuous baseband, channel occupancy or protocol identity.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

const FILE_MAGIC: &[u8; 8] = b"BDBURST\0";
const RECORD_MAGIC: &[u8; 4] = b"BRST";
const VERSION: u16 = 1;
const FILE_HEADER_LEN: usize = 16;
const RECORD_HEADER_LEN: usize = 48;
pub(crate) const CHANNEL_SAMPLE_RATE_HZ: u32 = 2_000_000;
const FLAG_SCAN: u32 = 1;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_RECORDS: u64 = 10_000_000;
const MAX_SAMPLES_PER_RECORD: u32 = 1_048_576;
pub(crate) const LOWEST_FREQUENCY_MHZ: u32 = 2402;
pub(crate) const HIGHEST_FREQUENCY_MHZ: u32 = 2480;
const FREQUENCY_COUNT: usize = (HIGHEST_FREQUENCY_MHZ - LOWEST_FREQUENCY_MHZ + 1) as usize;
const READ_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParseLimits {
    pub(crate) max_input_bytes: u64,
    pub(crate) max_records: u64,
    pub(crate) max_total_samples: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ArchiveFacts {
    pub(crate) source_bytes: u64,
    pub(crate) sha256: String,
    pub(crate) archive_record_count: u64,
    pub(crate) squelch_record_count: u64,
    pub(crate) scan_window_record_count: u64,
    pub(crate) complex_sample_count: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecordMetadata {
    pub(crate) archive_index: u64,
    pub(crate) timestamp_seconds: u64,
    pub(crate) timestamp_nanoseconds: u32,
    pub(crate) frequency_mhz: u32,
    pub(crate) relative_rssi_db: f32,
    pub(crate) relative_noise_db: f32,
    pub(crate) sample_count: u32,
    pub(crate) scan: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Statistics {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct FrequencySummary {
    frequency_mhz: u32,
    archive_record_count: u64,
    squelch_record_count: u64,
    scan_window_record_count: u64,
    complex_sample_count: u64,
    relative_rssi_db: Statistics,
    relative_noise_db: Statistics,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    schema_version: u32,
    source_format: &'static str,
    source_bytes: u64,
    channel_sample_rate_hz: u32,
    pub(crate) archive_record_count: u64,
    pub(crate) squelch_record_count: u64,
    pub(crate) scan_window_record_count: u64,
    complex_sample_count: u64,
    summed_record_duration_seconds: f64,
    samples_per_record: Statistics,
    relative_rssi_db: Statistics,
    relative_noise_db: Statistics,
    frequencies: Vec<FrequencySummary>,
    decode_correlation: &'static str,
    interpretation: &'static str,
}

impl Summary {
    pub(crate) fn scan_windows_on(&self, frequency_mhz: u32) -> u64 {
        self.frequencies
            .iter()
            .find(|entry| entry.frequency_mhz == frequency_mhz)
            .map_or(0, |entry| entry.scan_window_record_count)
    }
}

#[derive(Clone, Debug, Default)]
struct FloatStatistics {
    observations: u64,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
}

impl FloatStatistics {
    fn observe(&mut self, value: f64) {
        self.observations += 1;
        self.min = Some(self.min.map_or(value, |current| current.min(value)));
        self.max = Some(self.max.map_or(value, |current| current.max(value)));
        self.sum += value;
    }

    fn report(&self) -> Statistics {
        Statistics {
            observations: self.observations,
            min: self.min,
            max: self.max,
            mean: (self.observations > 0).then_some(self.sum / self.observations as f64),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CountStatistics {
    observations: u64,
    min: Option<u32>,
    max: Option<u32>,
    sum: u64,
}

impl CountStatistics {
    fn observe(&mut self, value: u32) -> Result<()> {
        self.observations += 1;
        self.min = Some(self.min.map_or(value, |current| current.min(value)));
        self.max = Some(self.max.map_or(value, |current| current.max(value)));
        self.sum = self
            .sum
            .checked_add(u64::from(value))
            .context("burst sample total overflow")?;
        Ok(())
    }

    fn report(&self) -> Statistics {
        Statistics {
            observations: self.observations,
            min: self.min.map(f64::from),
            max: self.max.map(f64::from),
            mean: (self.observations > 0).then_some(self.sum as f64 / self.observations as f64),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct FrequencyAccumulator {
    records: u64,
    scan_records: u64,
    samples: u64,
    rssi: FloatStatistics,
    noise: FloatStatistics,
}

fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed header"))
}

fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

fn f32_le(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed header"))
}

fn read_body(
    file: &mut File,
    byte_count: u64,
    retain: bool,
    digest: &mut Sha256,
    interrupted: &AtomicBool,
) -> Result<Vec<u8>> {
    let mut body = if retain {
        vec![0u8; usize::try_from(byte_count).context("burst body size does not fit memory")?]
    } else {
        Vec::new()
    };
    let mut scratch = [0u8; READ_BUFFER_BYTES];
    let mut offset = 0usize;
    let mut remaining = byte_count;
    while remaining > 0 {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "burst processing interrupted"
        );
        let count = usize::try_from(remaining.min(READ_BUFFER_BYTES as u64))
            .expect("read chunk is bounded by the fixed buffer");
        let target = if retain {
            &mut body[offset..offset + count]
        } else {
            &mut scratch[..count]
        };
        file.read_exact(target).context("read burst IQ body")?;
        digest.update(&*target);
        offset += count;
        remaining -= count as u64;
    }
    Ok(body)
}

/// Validate and stream one stable direct BDB v1 file.
///
/// This blocking, file-only parser allocates at most one packed squelch-record
/// body when `retain_squelch_iq` is true. Scan bodies are hashed and discarded;
/// the visitor must not retain the borrowed body.
pub(crate) fn stream_archive(
    path: &Path,
    limits: ParseLimits,
    interrupted: &AtomicBool,
    retain_squelch_iq: bool,
    mut visitor: impl FnMut(RecordMetadata, &[u8]) -> Result<()>,
) -> Result<ArchiveFacts> {
    ensure!(
        (FILE_HEADER_LEN as u64..=MAX_INPUT_BYTES).contains(&limits.max_input_bytes),
        "burst input limit must be 16..={MAX_INPUT_BYTES} bytes"
    );
    ensure!(
        limits.max_records <= MAX_RECORDS,
        "burst record limit must not exceed {MAX_RECORDS}"
    );
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "burst processing interrupted"
    );

    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspect burst input {}", path.display()))?;
    ensure!(
        before.file_type().is_file(),
        "burst input must be a direct regular file"
    );
    let before_identity = FileIdentity::from_metadata(&before);
    let source_bytes = before.len();
    ensure!(
        source_bytes >= FILE_HEADER_LEN as u64,
        "burst input is shorter than its file header"
    );
    ensure!(
        source_bytes <= limits.max_input_bytes,
        "burst input is {source_bytes} bytes, above the {}-byte limit",
        limits.max_input_bytes
    );
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        FileIdentity::from_metadata(&file.metadata()?) == before_identity,
        "burst input identity changed while opening"
    );

    let mut digest = Sha256::new();
    let mut file_header = [0u8; FILE_HEADER_LEN];
    file.read_exact(&mut file_header)
        .context("read Blue Dragon burst file header")?;
    digest.update(file_header);
    ensure!(&file_header[..8] == FILE_MAGIC, "invalid burst file magic");
    ensure!(
        u16_le(&file_header, 8) == VERSION,
        "unsupported burst file version"
    );
    ensure!(
        usize::from(u16_le(&file_header, 10)) == FILE_HEADER_LEN,
        "unsupported burst file header length"
    );
    ensure!(
        u32_le(&file_header, 12) == CHANNEL_SAMPLE_RATE_HZ,
        "unsupported burst channel sample rate"
    );

    let mut consumed = FILE_HEADER_LEN as u64;
    let mut records = 0u64;
    let mut scans = 0u64;
    let mut total_samples = 0u64;
    while consumed < source_bytes {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "burst processing interrupted"
        );
        ensure!(
            source_bytes - consumed >= RECORD_HEADER_LEN as u64,
            "truncated burst record header"
        );
        ensure!(records < limits.max_records, "burst record limit exceeded");
        let mut header = [0u8; RECORD_HEADER_LEN];
        file.read_exact(&mut header)
            .context("read burst record header")?;
        digest.update(header);
        consumed += RECORD_HEADER_LEN as u64;
        ensure!(&header[..4] == RECORD_MAGIC, "invalid burst record magic");
        let record_length = u64::from(u32_le(&header, 4));
        let nanoseconds = u32_le(&header, 16);
        ensure!(
            nanoseconds < 1_000_000_000,
            "invalid burst timestamp nanoseconds"
        );
        let frequency = u32_le(&header, 20);
        ensure!(
            (LOWEST_FREQUENCY_MHZ..=HIGHEST_FREQUENCY_MHZ).contains(&frequency),
            "burst frequency is outside 2402..=2480 MHz"
        );
        let rssi = f32_le(&header, 24);
        let noise = f32_le(&header, 28);
        ensure!(rssi.is_finite(), "burst RSSI metadata is not finite");
        ensure!(noise.is_finite(), "burst noise metadata is not finite");
        let sample_count = u32_le(&header, 36);
        ensure!(
            sample_count <= MAX_SAMPLES_PER_RECORD,
            "burst sample count exceeds {MAX_SAMPLES_PER_RECORD}"
        );
        let flags = u32_le(&header, 40);
        ensure!(flags & !FLAG_SCAN == 0, "unknown burst record flags");
        let scale = f32_le(&header, 44);
        ensure!(
            scale.is_finite() && scale > 0.0,
            "burst IQ scale must be finite and positive"
        );
        let sample_bytes = u64::from(sample_count)
            .checked_mul(4)
            .context("burst sample byte count overflow")?;
        let expected_length = (RECORD_HEADER_LEN as u64)
            .checked_add(sample_bytes)
            .context("burst record length overflow")?;
        ensure!(
            record_length == expected_length,
            "burst record length does not match its sample count"
        );
        ensure!(
            sample_bytes <= source_bytes - consumed,
            "truncated burst IQ body"
        );
        let next_total_samples = total_samples
            .checked_add(u64::from(sample_count))
            .context("burst sample total overflow")?;
        ensure!(
            next_total_samples <= limits.max_total_samples,
            "burst sample limit exceeded"
        );
        let scan = flags & FLAG_SCAN != 0;
        let body = read_body(
            &mut file,
            sample_bytes,
            retain_squelch_iq && !scan,
            &mut digest,
            interrupted,
        )?;
        consumed += sample_bytes;

        visitor(
            RecordMetadata {
                archive_index: records,
                timestamp_seconds: u64::from_le_bytes(
                    header[8..16].try_into().expect("fixed header"),
                ),
                timestamp_nanoseconds: nanoseconds,
                frequency_mhz: frequency,
                relative_rssi_db: rssi,
                relative_noise_db: noise,
                sample_count,
                scan,
            },
            &body,
        )?;
        records += 1;
        scans += u64::from(scan);
        total_samples = next_total_samples;
    }
    ensure!(
        consumed == source_bytes,
        "burst input length changed while reading"
    );
    let mut trailing = [0u8; 1];
    ensure!(
        file.read(&mut trailing)? == 0,
        "burst input grew while reading"
    );
    ensure!(
        !interrupted.load(Ordering::Relaxed),
        "burst processing interrupted"
    );
    let after_file = FileIdentity::from_metadata(&file.metadata()?);
    let after_path = fs::symlink_metadata(path)
        .with_context(|| format!("reinspect burst input {}", path.display()))?;
    ensure!(
        after_file == before_identity
            && after_path.file_type().is_file()
            && FileIdentity::from_metadata(&after_path) == before_identity,
        "burst input changed while reading"
    );

    Ok(ArchiveFacts {
        source_bytes,
        sha256: format!("{:x}", digest.finalize()),
        archive_record_count: records,
        squelch_record_count: records - scans,
        scan_window_record_count: scans,
        complex_sample_count: total_samples,
    })
}

pub(crate) fn inspect(path: &Path, max_input_bytes: u64) -> Result<Summary> {
    inspect_with_scan_check(path, max_input_bytes, |_| Ok(()))
}

pub(crate) fn inspect_with_scan_check(
    path: &Path,
    max_input_bytes: u64,
    mut check_scan: impl FnMut(&RecordMetadata) -> Result<()>,
) -> Result<Summary> {
    let mut sample_stats = CountStatistics::default();
    let mut rssi_stats = FloatStatistics::default();
    let mut noise_stats = FloatStatistics::default();
    let mut frequencies: [FrequencyAccumulator; FREQUENCY_COUNT] =
        std::array::from_fn(|_| FrequencyAccumulator::default());
    let interrupted = AtomicBool::new(false);
    let facts = stream_archive(
        path,
        ParseLimits {
            max_input_bytes,
            max_records: MAX_RECORDS,
            max_total_samples: u64::MAX,
        },
        &interrupted,
        false,
        |metadata, _| {
            if metadata.scan {
                check_scan(&metadata)?;
            }
            sample_stats.observe(metadata.sample_count)?;
            rssi_stats.observe(f64::from(metadata.relative_rssi_db));
            if !metadata.scan {
                noise_stats.observe(f64::from(metadata.relative_noise_db));
            }
            let frequency_state =
                &mut frequencies[(metadata.frequency_mhz - LOWEST_FREQUENCY_MHZ) as usize];
            frequency_state.records += 1;
            frequency_state.scan_records += u64::from(metadata.scan);
            frequency_state.samples = frequency_state
                .samples
                .checked_add(u64::from(metadata.sample_count))
                .context("per-frequency burst sample total overflow")?;
            frequency_state
                .rssi
                .observe(f64::from(metadata.relative_rssi_db));
            if !metadata.scan {
                frequency_state
                    .noise
                    .observe(f64::from(metadata.relative_noise_db));
            }
            Ok(())
        },
    )?;

    let frequencies = frequencies
        .iter()
        .enumerate()
        .filter(|(_, state)| state.records > 0)
        .map(|(index, state)| FrequencySummary {
            frequency_mhz: LOWEST_FREQUENCY_MHZ + index as u32,
            archive_record_count: state.records,
            squelch_record_count: state.records - state.scan_records,
            scan_window_record_count: state.scan_records,
            complex_sample_count: state.samples,
            relative_rssi_db: state.rssi.report(),
            relative_noise_db: state.noise.report(),
        })
        .collect();
    Ok(Summary {
        schema_version: 1,
        source_format: "blue_dragon_burst_v1",
        source_bytes: facts.source_bytes,
        channel_sample_rate_hz: CHANNEL_SAMPLE_RATE_HZ,
        archive_record_count: facts.archive_record_count,
        squelch_record_count: facts.squelch_record_count,
        scan_window_record_count: facts.scan_window_record_count,
        complex_sample_count: facts.complex_sample_count,
        summed_record_duration_seconds: facts.complex_sample_count as f64
            / f64::from(CHANNEL_SAMPLE_RATE_HZ),
        samples_per_record: sample_stats.report(),
        relative_rssi_db: rssi_stats.report(),
        relative_noise_db: noise_stats.report(),
        frequencies,
        decode_correlation: "not_encoded",
        interpretation: "Channelized squelch-triggered records and continuous-scan windows, not decoded packets or continuous baseband. Records may overlap and have no encoded one-to-one correlation with packet PCAP records. Counts and summed record durations do not establish protocol identity, capture continuity, unique on-air duration or channel occupancy. RSSI/noise metadata are relative receiver estimates, not calibrated dBm.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file_header() -> Vec<u8> {
        let mut bytes = Vec::from(*FILE_MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&(FILE_HEADER_LEN as u16).to_le_bytes());
        bytes.extend_from_slice(&CHANNEL_SAMPLE_RATE_HZ.to_le_bytes());
        bytes
    }

    #[derive(Clone, Copy)]
    struct Record {
        frequency: u32,
        samples: u32,
        flags: u32,
        rssi: f32,
        noise: f32,
        scale: f32,
        nanoseconds: u32,
    }

    fn append_record(bytes: &mut Vec<u8>, record: Record, include_body: bool) {
        let length = RECORD_HEADER_LEN as u32 + record.samples.saturating_mul(4);
        bytes.extend_from_slice(RECORD_MAGIC);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&record.nanoseconds.to_le_bytes());
        bytes.extend_from_slice(&record.frequency.to_le_bytes());
        bytes.extend_from_slice(&record.rssi.to_le_bytes());
        bytes.extend_from_slice(&record.noise.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&record.samples.to_le_bytes());
        bytes.extend_from_slice(&record.flags.to_le_bytes());
        bytes.extend_from_slice(&record.scale.to_le_bytes());
        if include_body {
            bytes.resize(bytes.len() + record.samples as usize * 4, 0);
        }
    }

    fn baseline() -> Record {
        Record {
            frequency: 2437,
            samples: 2,
            flags: 0,
            rssi: -40.0,
            noise: -80.0,
            scale: 0.25,
            nanoseconds: 123,
        }
    }

    fn inspect_bytes(bytes: &[u8], limit: u64) -> Result<Summary> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bursts.bdb");
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        inspect(&path, limit)
    }

    #[test]
    fn validates_and_aggregates_squelch_scan_and_frequencies() {
        let mut bytes = file_header();
        append_record(&mut bytes, baseline(), true);
        append_record(
            &mut bytes,
            Record {
                samples: 0,
                flags: FLAG_SCAN,
                rssi: -50.0,
                noise: -127.0,
                ..baseline()
            },
            true,
        );
        append_record(
            &mut bytes,
            Record {
                frequency: 2426,
                samples: 4,
                rssi: -60.0,
                noise: -90.0,
                ..baseline()
            },
            true,
        );
        let report = inspect_bytes(&bytes, bytes.len() as u64).unwrap();
        assert_eq!(report.archive_record_count, 3);
        assert_eq!(report.squelch_record_count, 2);
        assert_eq!(report.scan_window_record_count, 1);
        assert_eq!(report.scan_windows_on(2437), 1);
        assert_eq!(report.scan_windows_on(2426), 0);
        assert_eq!(report.scan_windows_on(2444), 0);
        assert_eq!(report.complex_sample_count, 6);
        assert_eq!(report.summed_record_duration_seconds, 0.000003);
        assert_eq!(report.samples_per_record.min, Some(0.0));
        assert_eq!(report.samples_per_record.max, Some(4.0));
        assert_eq!(report.samples_per_record.mean, Some(2.0));
        assert_eq!(report.relative_rssi_db.mean, Some(-50.0));
        assert_eq!(report.relative_noise_db.observations, 2);
        assert_eq!(report.relative_noise_db.mean, Some(-85.0));
        assert_eq!(report.frequencies.len(), 2);
        assert_eq!(report.frequencies[0].frequency_mhz, 2426);
        assert_eq!(report.frequencies[1].frequency_mhz, 2437);
        assert_eq!(report.frequencies[1].relative_noise_db.observations, 1);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["archive_record_count"], 3);
        assert!(value.get("detector_record_count").is_none());
        assert_eq!(value["frequencies"][1]["archive_record_count"], 2);
    }

    #[test]
    fn accepts_empty_archive_and_zero_sample_record_with_null_statistics() {
        let header = file_header();
        let empty = inspect_bytes(&header, header.len() as u64).unwrap();
        assert_eq!(empty.archive_record_count, 0);
        assert_eq!(empty.samples_per_record.mean, None);
        assert_eq!(empty.relative_rssi_db.min, None);
        assert!(empty.frequencies.is_empty());

        let mut zero = file_header();
        append_record(
            &mut zero,
            Record {
                samples: 0,
                ..baseline()
            },
            true,
        );
        let report = inspect_bytes(&zero, zero.len() as u64).unwrap();
        assert_eq!(report.archive_record_count, 1);
        assert_eq!(report.complex_sample_count, 0);
    }

    #[test]
    fn rejects_bad_file_headers() {
        for offset in [0usize, 8, 10, 12] {
            let mut bytes = file_header();
            bytes[offset] ^= 0xff;
            assert!(inspect_bytes(&bytes, bytes.len() as u64).is_err());
        }
    }

    #[test]
    fn rejects_invalid_record_metadata() {
        let cases = [
            Record {
                frequency: 2401,
                ..baseline()
            },
            Record {
                frequency: 2481,
                ..baseline()
            },
            Record {
                flags: 2,
                ..baseline()
            },
            Record {
                nanoseconds: 1_000_000_000,
                ..baseline()
            },
            Record {
                rssi: f32::NAN,
                ..baseline()
            },
            Record {
                noise: f32::INFINITY,
                ..baseline()
            },
            Record {
                scale: 0.0,
                ..baseline()
            },
            Record {
                scale: f32::NAN,
                ..baseline()
            },
        ];
        for record in cases {
            let mut bytes = file_header();
            append_record(&mut bytes, record, true);
            assert!(inspect_bytes(&bytes, bytes.len() as u64).is_err());
        }
    }

    #[test]
    fn rejects_bad_record_framing_without_allocating_declared_body() {
        let mut bad_magic = file_header();
        append_record(&mut bad_magic, baseline(), true);
        bad_magic[FILE_HEADER_LEN] ^= 0xff;
        assert!(inspect_bytes(&bad_magic, bad_magic.len() as u64).is_err());

        let mut bad_length = file_header();
        append_record(&mut bad_length, baseline(), true);
        bad_length[FILE_HEADER_LEN + 4..FILE_HEADER_LEN + 8]
            .copy_from_slice(&(RECORD_HEADER_LEN as u32).to_le_bytes());
        assert!(inspect_bytes(&bad_length, bad_length.len() as u64).is_err());

        let mut oversized = file_header();
        append_record(
            &mut oversized,
            Record {
                samples: MAX_SAMPLES_PER_RECORD + 1,
                ..baseline()
            },
            false,
        );
        assert!(inspect_bytes(&oversized, oversized.len() as u64).is_err());

        let mut truncated = file_header();
        append_record(&mut truncated, baseline(), false);
        assert!(inspect_bytes(&truncated, truncated.len() as u64).is_err());

        let mut partial = file_header();
        partial.push(0);
        assert!(inspect_bytes(&partial, partial.len() as u64).is_err());
    }

    #[test]
    fn enforces_input_limits_and_regular_files() {
        let header = file_header();
        assert!(inspect_bytes(&header, (FILE_HEADER_LEN - 1) as u64).is_err());
        assert!(inspect_bytes(&header, MAX_INPUT_BYTES + 1).is_err());
        assert!(inspect_bytes(&header, header.len() as u64 - 1).is_err());

        let directory = tempfile::tempdir().unwrap();
        assert!(inspect(directory.path(), FILE_HEADER_LEN as u64).is_err());
    }
}
