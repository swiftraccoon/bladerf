//! Structural decoder and strict stream validator for profile-2 records.
//!
//! A profile-2 stream starts with one CONFIG record followed by one or more
//! DATA records. Structural decoding deliberately preserves diagnostic CONFIG
//! records; only the stateful validator makes continuity and clean-run claims.

#[cfg(target_os = "linux")]
use std::io::{BufReader, Read};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::dsp::Iq16;

pub(crate) const RECORD_BYTES: usize = 32;
pub(crate) const FORMAT_VERSION: u16 = 2;
pub(crate) const PROFILE_ID: u16 = 2;
pub(crate) const SOURCE_SAMPLE_RATE_HZ: u32 = 20_000_000;

const FORMAT_VERSION_MASK: u16 = 0xf000;
const CLIP_MASK: u16 = 0x0f00;
const LOSS_MASK: u16 = 0x0080;
const CONFIGURATION_ERROR_MASK: u16 = 0x0040;
const RX2_MASK: u16 = 0x0020;
const CONFIG_RECORD_MASK: u16 = 0x0010;
const EPOCH_TAG_MASK: u16 = 0x000f;
const FIRST_SOURCE_INDEX: u64 = 19;
const SOURCE_INDEX_STEP: u64 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Status {
    pub(crate) clipped_channels: u8,
    pub(crate) loss: bool,
    pub(crate) configuration_error: bool,
    pub(crate) rx2: bool,
    pub(crate) epoch_tag: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Config {
    pub(crate) status: Status,
    pub(crate) epoch: u32,
    pub(crate) required_source_sample_rate_hz: u32,
    pub(crate) phase_words: [u32; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Data {
    pub(crate) status: Status,
    pub(crate) source_index: u64,
    pub(crate) samples: [Iq16; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Record {
    Config(Config),
    Data(Data),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedStream {
    pub(crate) config: Config,
    pub(crate) data_records: u64,
    pub(crate) first_source_index: u64,
    pub(crate) last_source_index: u64,
    pub(crate) processed_bytes: u64,
    pub(crate) processed_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InspectedFile {
    pub(crate) stream: ValidatedStream,
    pub(crate) input_extent_bytes: u64,
    pub(crate) available_data_record_slots: u64,
    pub(crate) requested_max_data_records: Option<u64>,
    pub(crate) complete_file_validated: bool,
}

fn word(raw: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([raw[offset], raw[offset + 1]])
}

fn dword(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

fn qword(raw: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
        raw[offset + 4],
        raw[offset + 5],
        raw[offset + 6],
        raw[offset + 7],
    ])
}

fn status(flags: u16) -> Status {
    Status {
        clipped_channels: ((flags & CLIP_MASK) >> 8) as u8,
        loss: flags & LOSS_MASK != 0,
        configuration_error: flags & CONFIGURATION_ERROR_MASK != 0,
        rx2: flags & RX2_MASK != 0,
        epoch_tag: (flags & EPOCH_TAG_MASK) as u8,
    }
}

/// Decode one complete profile-2 record without claiming that it belongs to a
/// valid stream. In particular, diagnostic CONFIG records and asserted status
/// flags remain observable to callers.
pub(crate) fn decode(raw: &[u8]) -> Result<Record> {
    ensure!(
        raw.len() == RECORD_BYTES,
        "profile-2 record must be 32 bytes"
    );
    ensure!(
        word(raw, 0) == 0x5246 && word(raw, 2) == 0x4f42,
        "profile-2 magic mismatch"
    );

    let flags = word(raw, 4);
    ensure!(
        flags & FORMAT_VERSION_MASK == FORMAT_VERSION << 12,
        "unsupported profile-2 transport version"
    );
    ensure!(
        word(raw, 6) == PROFILE_ID,
        "unsupported profile-2 gateware profile"
    );

    let decoded_status = status(flags);
    if flags & CONFIG_RECORD_MASK != 0 {
        Ok(Record::Config(Config {
            status: decoded_status,
            epoch: dword(raw, 8),
            required_source_sample_rate_hz: dword(raw, 12),
            phase_words: std::array::from_fn(|channel| dword(raw, 16 + channel * 4)),
        }))
    } else {
        Ok(Record::Data(Data {
            status: decoded_status,
            source_index: qword(raw, 8),
            samples: std::array::from_fn(|channel| Iq16 {
                i: word(raw, 16 + channel * 4) as i16,
                q: word(raw, 18 + channel * 4) as i16,
            }),
        }))
    }
}

fn validate_clean_status(status: Status, record_number: u64) -> Result<()> {
    ensure!(
        status.clipped_channels == 0,
        "profile-2 record {record_number} reports saturated channel samples"
    );
    ensure!(
        !status.loss,
        "profile-2 record {record_number} reports lost ADC samples or transport backpressure"
    );
    ensure!(
        !status.configuration_error,
        "profile-2 record {record_number} reports invalid receive configuration"
    );
    Ok(())
}

fn validate_config(config: Config) -> Result<()> {
    validate_clean_status(config.status, 0)?;
    ensure!(config.epoch != 0, "profile-2 configuration epoch is zero");
    ensure!(
        u32::from(config.status.epoch_tag) == config.epoch & u32::from(EPOCH_TAG_MASK),
        "profile-2 configuration epoch tag does not match the full epoch"
    );
    ensure!(
        config.required_source_sample_rate_hz == SOURCE_SAMPLE_RATE_HZ,
        "profile-2 stream requires an unsupported source sample rate"
    );
    Ok(())
}

fn next_source_index(previous: u64) -> Result<u64> {
    previous
        .checked_add(SOURCE_INDEX_STEP)
        .context("profile-2 source index overflow")
}

#[derive(Default)]
struct StreamState {
    config: Option<Config>,
    data_records: u64,
    first_source_index: Option<u64>,
    previous_source_index: Option<u64>,
}

impl StreamState {
    fn push(&mut self, record_number: u64, record: Record) -> Result<()> {
        match record {
            Record::Config(config) => {
                ensure!(
                    record_number == 0 && self.config.is_none(),
                    "profile-2 CONFIG must be the single first record"
                );
                validate_config(config)?;
                self.config = Some(config);
            }
            Record::Data(data) => {
                let config = self
                    .config
                    .context("profile-2 DATA appeared before the required CONFIG")?;
                validate_clean_status(data.status, record_number)?;
                ensure!(
                    data.status.rx2 == config.status.rx2,
                    "profile-2 receive input changed at record {record_number}"
                );
                ensure!(
                    data.status.epoch_tag == config.status.epoch_tag,
                    "profile-2 epoch tag changed at record {record_number}"
                );

                match self.previous_source_index {
                    None => ensure!(
                        data.source_index == FIRST_SOURCE_INDEX,
                        "profile-2 first DATA source index must be 19"
                    ),
                    Some(previous) => ensure!(
                        data.source_index == next_source_index(previous)?,
                        "profile-2 source index gap or reorder at record {record_number}"
                    ),
                }

                self.first_source_index.get_or_insert(data.source_index);
                self.previous_source_index = Some(data.source_index);
                self.data_records = self
                    .data_records
                    .checked_add(1)
                    .context("profile-2 DATA record count overflow")?;
            }
        }
        Ok(())
    }

    fn finish(self, processed_bytes: u64, processed_sha256: String) -> Result<ValidatedStream> {
        let config = self
            .config
            .context("profile-2 stream is missing its CONFIG record")?;
        ensure!(
            self.data_records > 0,
            "profile-2 stream must contain at least one DATA record"
        );
        let first_source_index = self
            .first_source_index
            .context("profile-2 stream is missing its first DATA index")?;
        let last_source_index = self
            .previous_source_index
            .context("profile-2 stream is missing its last DATA index")?;
        Ok(ValidatedStream {
            config,
            data_records: self.data_records,
            first_source_index,
            last_source_index,
            processed_bytes,
            processed_sha256,
        })
    }
}

/// Validate a complete in-memory profile-2 stream. The returned summary is
/// produced only after every record passes ordering, source, epoch and source
/// index continuity checks.
pub(crate) fn validate_stream(raw: &[u8]) -> Result<ValidatedStream> {
    ensure!(!raw.is_empty(), "empty profile-2 stream");
    let (records, remainder) = raw.as_chunks::<RECORD_BYTES>();
    ensure!(
        remainder.is_empty(),
        "profile-2 stream must contain complete 32-byte records"
    );

    let mut state = StreamState::default();
    for (record_number, record_bytes) in records.iter().enumerate() {
        let record_number =
            u64::try_from(record_number).context("profile-2 record number does not fit u64")?;
        let record = decode(record_bytes)
            .with_context(|| format!("decode profile-2 record {record_number}"))?;
        state
            .push(record_number, record)
            .with_context(|| format!("validate profile-2 record {record_number}"))?;
    }
    state.finish(
        u64::try_from(raw.len()).context("profile-2 stream length does not fit u64")?,
        format!("{:x}", Sha256::digest(raw)),
    )
}

/// Shared scanner with a per-record observer used by complete validation and
/// explicitly bounded prefix inspection.
#[cfg(target_os = "linux")]
fn scan_file_selection(
    path: &Path,
    stop: &AtomicBool,
    max_bytes: u64,
    max_data_records: Option<u64>,
    mut after_record: impl FnMut(u64, u64, Record) -> Result<()>,
) -> Result<InspectedFile> {
    ensure!(
        max_data_records != Some(0),
        "profile-2 DATA record limit must be positive"
    );
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 file validation was interrupted before opening the input"
    );
    let (file, initial_bytes) = crate::file_pin::open_regular(path, max_bytes)?;
    let initial_metadata = file
        .metadata()
        .with_context(|| format!("inspect profile-2 input: {}", path.display()))?;
    ensure!(
        initial_bytes % u64::try_from(RECORD_BYTES)? == 0,
        "profile-2 stream must contain complete 32-byte records"
    );

    let input_records = initial_bytes / u64::try_from(RECORD_BYTES)?;
    ensure!(
        input_records >= 2,
        "profile-2 stream must contain one CONFIG and at least one DATA record"
    );
    let available_data_record_slots = input_records - 1;
    let selected_data_records = max_data_records.map_or(available_data_record_slots, |limit| {
        available_data_record_slots.min(limit)
    });
    let selected_records = selected_data_records
        .checked_add(1)
        .context("profile-2 selected record count overflow")?;
    let complete_file_validated = selected_data_records == available_data_record_slots;
    let mut input = BufReader::new(file);
    let mut state = StreamState::default();
    let mut hash = Sha256::new();
    for record_number in 0..selected_records {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "profile-2 file validation was interrupted at record {record_number}"
        );
        let mut raw = [0u8; RECORD_BYTES];
        input
            .read_exact(&mut raw)
            .with_context(|| format!("read profile-2 record {record_number}"))?;
        let record =
            decode(&raw).with_context(|| format!("decode profile-2 record {record_number}"))?;
        state
            .push(record_number, record)
            .with_context(|| format!("validate profile-2 record {record_number}"))?;
        hash.update(raw);
        after_record(record_number, selected_data_records, record)?;
    }

    if complete_file_validated {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "profile-2 file validation was interrupted after the final record"
        );
    } else {
        ensure!(
            !stop.load(Ordering::Relaxed),
            "profile-2 file validation was interrupted after the selected prefix"
        );
    }
    let trailing_bytes = if complete_file_validated {
        let mut trailing = [0u8; 1];
        input.read(&mut trailing)?
    } else {
        0
    };
    let final_metadata = input
        .get_ref()
        .metadata()
        .with_context(|| format!("reinspect profile-2 input: {}", path.display()))?;
    ensure!(
        trailing_bytes == 0
            && final_metadata.dev() == initial_metadata.dev()
            && final_metadata.ino() == initial_metadata.ino()
            && final_metadata.len() == initial_bytes
            && final_metadata.mtime() == initial_metadata.mtime()
            && final_metadata.mtime_nsec() == initial_metadata.mtime_nsec()
            && final_metadata.ctime() == initial_metadata.ctime()
            && final_metadata.ctime_nsec() == initial_metadata.ctime_nsec(),
        "profile-2 stream content or extent changed while it was being validated"
    );
    let processed_bytes = selected_records
        .checked_mul(u64::try_from(RECORD_BYTES)?)
        .context("profile-2 processed byte count overflow")?;
    let stream = state.finish(processed_bytes, format!("{:x}", hash.finalize()))?;
    Ok(InspectedFile {
        stream,
        input_extent_bytes: initial_bytes,
        available_data_record_slots,
        requested_max_data_records: max_data_records,
        complete_file_validated,
    })
}

/// Shared complete-file scanner with a per-record observer used to test input
/// changes while retaining the original validation contract.
#[cfg(target_os = "linux")]
fn scan_file(
    path: &Path,
    stop: &AtomicBool,
    max_bytes: u64,
    mut after_record: impl FnMut(u64) -> Result<()>,
) -> Result<ValidatedStream> {
    Ok(
        scan_file_selection(path, stop, max_bytes, None, |record_number, _, _| {
            after_record(record_number)
        })?
        .stream,
    )
}

/// Inspect a complete profile-2 file or an explicit DATA-record prefix.
///
/// CONFIG is always included and validated. The callback observes each selected
/// DATA record only after strict stream validation; it receives the zero-based
/// DATA ordinal and the selected DATA count. Prefix mode intentionally makes no
/// semantic claim about the unprocessed suffix. The returned digest covers
/// CONFIG plus exactly the selected DATA records, while the input extent and
/// extent-derived available count retain that distinction.
#[cfg(target_os = "linux")]
pub(crate) fn inspect_file(
    path: &Path,
    max_data_records: Option<u64>,
    stop: &AtomicBool,
    mut consume: impl FnMut(u64, u64, Data) -> Result<()>,
) -> Result<InspectedFile> {
    scan_file_selection(
        path,
        stop,
        u64::MAX,
        max_data_records,
        |record_number, selected_data_records, record| match record {
            Record::Config(_) => Ok(()),
            Record::Data(data) => consume(record_number - 1, selected_data_records, data),
        },
    )
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn inspect_file(
    _path: &Path,
    _max_data_records: Option<u64>,
    _stop: &AtomicBool,
    _consume: impl FnMut(u64, u64, Data) -> Result<()>,
) -> Result<InspectedFile> {
    anyhow::bail!("profile-2 file inspection requires Linux /proc/self/fd support")
}

/// Validate a complete profile-2 file with bounded memory and no derived output.
///
/// The descriptor identity, extent, and Linux modification/change timestamps
/// must stay fixed for the whole scan. Cancellation is checked before opening,
/// before every record, and after the final record; interruption returns no
/// summary and leaves the input unchanged.
#[cfg(target_os = "linux")]
pub(crate) fn validate_file(path: &Path, stop: &AtomicBool) -> Result<ValidatedStream> {
    scan_file(path, stop, u64::MAX, |_| Ok(()))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn validate_file(_path: &Path, _stop: &AtomicBool) -> Result<ValidatedStream> {
    anyhow::bail!("profile-2 file validation requires Linux /proc/self/fd support")
}

/// Validate one complete profile-2 file with an exact caller-declared extent.
///
/// The size bound is applied while pinning the input, before any record scan.
/// This prevents an unexpectedly large replacement path from causing an
/// unbounded validation pass. The returned stream must then have exactly the
/// declared byte count.
#[cfg(target_os = "linux")]
pub(crate) fn validate_file_exact(
    path: &Path,
    expected_bytes: u64,
    stop: &AtomicBool,
) -> Result<ValidatedStream> {
    let stream = scan_file(path, stop, expected_bytes, |_| Ok(()))?;
    ensure!(
        stream.processed_bytes == expected_bytes,
        "profile-2 stream byte count differs from the exact expected extent"
    );
    Ok(stream)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn validate_file_exact(
    _path: &Path,
    _expected_bytes: u64,
    _stop: &AtomicBool,
) -> Result<ValidatedStream> {
    anyhow::bail!("profile-2 file validation requires Linux /proc/self/fd support")
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::fs::File;

    #[cfg(target_os = "linux")]
    use nix::sys::stat::Mode;
    #[cfg(target_os = "linux")]
    use nix::unistd::mkfifo;

    use super::*;

    const VALID_CONFIG: [u8; RECORD_BYTES] = [
        0x46, 0x52, 0x42, 0x4f, 0x38, 0x20, 0x02, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x2d, 0x31,
        0x01, 0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab, 0x89, 0x01, 0x00, 0x00, 0x80, 0xef, 0xbe,
        0xad, 0xde,
    ];
    const VALID_DATA: [u8; RECORD_BYTES] = [
        0x46, 0x52, 0x42, 0x4f, 0x28, 0x20, 0x02, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x80, 0xff, 0x7f, 0xff, 0xff, 0x01, 0x00, 0x34, 0x12, 0xcc, 0xed, 0x2e, 0xfb,
        0x29, 0x09,
    ];
    const SECOND_DATA: [u8; RECORD_BYTES] = [
        0x46, 0x52, 0x42, 0x4f, 0x28, 0x20, 0x02, 0x00, 0x27, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x80, 0xff, 0x7f, 0xff, 0xff, 0x01, 0x00, 0x34, 0x12, 0xcc, 0xed, 0x2e, 0xfb,
        0x29, 0x09,
    ];
    const ERROR_CONFIG: [u8; RECORD_BYTES] = [
        0x46, 0x52, 0x42, 0x4f, 0x50, 0x20, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2d, 0x31,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    const VALID_V1_DATA: [u8; RECORD_BYTES] = [
        0x46, 0x52, 0x42, 0x4f, 0x20, 0x10, 0x01, 0x00, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x80, 0xff, 0x7f, 0xff, 0xff, 0x00, 0x00, 0x34, 0x12, 0xcc, 0xed, 0x01, 0x00,
        0xfe, 0xff,
    ];

    fn stream(records: &[[u8; RECORD_BYTES]]) -> Vec<u8> {
        records.iter().flatten().copied().collect()
    }

    #[test]
    fn decodes_golden_config_and_data_structurally() {
        assert_eq!(
            decode(&VALID_CONFIG).unwrap(),
            Record::Config(Config {
                status: Status {
                    clipped_channels: 0,
                    loss: false,
                    configuration_error: false,
                    rx2: true,
                    epoch_tag: 8,
                },
                epoch: 0x1234_5678,
                required_source_sample_rate_hz: 20_000_000,
                phase_words: [0x0123_4567, 0x89ab_cdef, 0x8000_0001, 0xdead_beef],
            })
        );
        assert_eq!(
            decode(&VALID_DATA).unwrap(),
            Record::Data(Data {
                status: Status {
                    clipped_channels: 0,
                    loss: false,
                    configuration_error: false,
                    rx2: true,
                    epoch_tag: 8,
                },
                source_index: 19,
                samples: [
                    Iq16 {
                        i: i16::MIN,
                        q: i16::MAX,
                    },
                    Iq16 { i: -1, q: 1 },
                    Iq16 {
                        i: 0x1234,
                        q: -0x1234,
                    },
                    Iq16 { i: -1234, q: 2345 },
                ],
            })
        );
    }

    #[test]
    fn golden_hashes_and_valid_stream_are_stable() {
        assert_eq!(
            format!("{:x}", Sha256::digest(VALID_CONFIG)),
            "df5bf4861727dfc66de462e2fe1056d70495be9649ff2234d536d24f83306353"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(VALID_DATA)),
            "01dea24aa8e35d72da4b3656ef43779b1245bc9679618013f14bf781e1ad8139"
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(ERROR_CONFIG)),
            "6cb9300f08297471dd3847998bcfb5cc7ce7e378acf0f64c2c818e63118cc46b"
        );

        let bytes = stream(&[VALID_CONFIG, VALID_DATA]);
        let validated = validate_stream(&bytes).unwrap();
        assert_eq!(validated.config.epoch, 0x1234_5678);
        assert_eq!(validated.data_records, 1);
        assert_eq!(validated.first_source_index, 19);
        assert_eq!(validated.last_source_index, 19);
        assert_eq!(validated.processed_bytes, 64);
        assert_eq!(
            validated.processed_sha256,
            "f1b8afbaef08fa143d125e35059341a40e9a707f3dba77e8f621f44ec3dca0bb"
        );

        let bytes = stream(&[VALID_CONFIG, VALID_DATA, SECOND_DATA]);
        let validated = validate_stream(&bytes).unwrap();
        assert_eq!(validated.data_records, 2);
        assert_eq!(validated.last_source_index, 39);

        let mut tag_zero_config = VALID_CONFIG;
        tag_zero_config[4] = 0x30;
        tag_zero_config[8..12].copy_from_slice(&[0x10, 0x00, 0x00, 0x00]);
        let mut tag_zero_data = VALID_DATA;
        tag_zero_data[4] = 0x20;
        assert!(validate_stream(&stream(&[tag_zero_config, tag_zero_data])).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_validation_matches_memory_and_rejects_input_drift() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile2.frames");
        let bytes = stream(&[VALID_CONFIG, VALID_DATA, SECOND_DATA]);
        std::fs::write(&path, &bytes).unwrap();

        let expected = validate_stream(&bytes).unwrap();
        let actual = validate_file(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(actual, expected);
        let exact = validate_file_exact(
            &path,
            u64::try_from(bytes.len()).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(exact, expected);
        assert!(
            validate_file_exact(
                &path,
                u64::try_from(bytes.len() - 1).unwrap(),
                &AtomicBool::new(false),
            )
            .is_err()
        );
        assert!(
            validate_file_exact(
                &path,
                u64::try_from(bytes.len() + 1).unwrap(),
                &AtomicBool::new(false),
            )
            .is_err()
        );

        let error = scan_file(&path, &AtomicBool::new(false), u64::MAX, |record_number| {
            if record_number == 0 {
                use std::io::Write as _;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)?
                    .write_all(&SECOND_DATA)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("extent changed"));

        std::fs::write(&path, &bytes).unwrap();
        let error = scan_file(&path, &AtomicBool::new(false), u64::MAX, |record_number| {
            if record_number == 0 {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(u64::try_from(RECORD_BYTES)?)?;
            }
            Ok(())
        })
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("content or extent changed")
                || message.contains("read profile-2 record")
        );

        std::fs::write(&path, &bytes).unwrap();
        let error = scan_file(&path, &AtomicBool::new(false), u64::MAX, |record_number| {
            if record_number == 0 {
                use std::io::{Seek as _, SeekFrom, Write as _};
                let mut output = std::fs::OpenOptions::new().write(true).open(&path)?;
                output.seek(SeekFrom::Start(0))?;
                output.write_all(&[0u8; RECORD_BYTES])?;
                output.sync_data()?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("content or extent changed"));

        let fifo = directory.path().join("profile2.fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let error = validate_file(&fifo, &AtomicBool::new(false)).unwrap_err();
        assert!(format!("{error:#}").contains("nonempty regular file"));

        use std::os::unix::fs::symlink;
        let symlink_path = directory.path().join("profile2-link.frames");
        symlink(&path, &symlink_path).unwrap();
        let error = validate_file(&symlink_path, &AtomicBool::new(false)).unwrap_err();
        assert!(format!("{error:#}").contains("nonempty regular file"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_inspection_distinguishes_complete_input_from_a_validated_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile2.frames");
        let bytes = stream(&[VALID_CONFIG, VALID_DATA, SECOND_DATA]);
        std::fs::write(&path, &bytes).unwrap();

        let mut full_observations = Vec::new();
        let full = inspect_file(
            &path,
            None,
            &AtomicBool::new(false),
            |ordinal, selected, data| {
                full_observations.push((ordinal, selected, data.source_index));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            full_observations,
            vec![(0, 2, FIRST_SOURCE_INDEX), (1, 2, 39)]
        );
        assert_eq!(full.input_extent_bytes, 96);
        assert_eq!(full.available_data_record_slots, 2);
        assert_eq!(full.requested_max_data_records, None);
        assert!(full.complete_file_validated);
        assert_eq!(full.stream.data_records, 2);
        assert_eq!(full.stream.processed_bytes, 96);
        assert_eq!(
            full.stream.processed_sha256,
            format!("{:x}", Sha256::digest(&bytes))
        );

        let mut prefix_observations = Vec::new();
        let prefix = inspect_file(
            &path,
            Some(1),
            &AtomicBool::new(false),
            |ordinal, selected, data| {
                prefix_observations.push((ordinal, selected, data.source_index));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(prefix_observations, vec![(0, 1, FIRST_SOURCE_INDEX)]);
        assert_eq!(prefix.input_extent_bytes, 96);
        assert_eq!(prefix.available_data_record_slots, 2);
        assert_eq!(prefix.requested_max_data_records, Some(1));
        assert!(!prefix.complete_file_validated);
        assert_eq!(prefix.stream.data_records, 1);
        assert_eq!(prefix.stream.first_source_index, FIRST_SOURCE_INDEX);
        assert_eq!(prefix.stream.last_source_index, FIRST_SOURCE_INDEX);
        assert_eq!(prefix.stream.processed_bytes, 64);
        assert_eq!(
            prefix.stream.processed_sha256,
            format!("{:x}", Sha256::digest(&bytes[..64]))
        );

        let oversized =
            inspect_file(&path, Some(99), &AtomicBool::new(false), |_, _, _| Ok(())).unwrap();
        assert_eq!(oversized.requested_max_data_records, Some(99));
        assert!(oversized.complete_file_validated);
        assert_eq!(oversized.stream.data_records, 2);
        assert_eq!(
            oversized.stream.processed_sha256,
            full.stream.processed_sha256
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_inspection_validates_only_the_selected_prefix_and_checks_its_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile2.frames");
        let mut invalid_suffix = SECOND_DATA;
        invalid_suffix[0] ^= 0x01;
        std::fs::write(&path, stream(&[VALID_CONFIG, VALID_DATA, invalid_suffix])).unwrap();

        let prefix =
            inspect_file(&path, Some(1), &AtomicBool::new(false), |_, _, _| Ok(())).unwrap();
        assert_eq!(prefix.available_data_record_slots, 2);
        assert_eq!(prefix.stream.data_records, 1);
        assert!(!prefix.complete_file_validated);
        assert!(inspect_file(&path, None, &AtomicBool::new(false), |_, _, _| Ok(())).is_err());

        let missing = directory.path().join("must-not-be-opened.frames");
        let error =
            inspect_file(&missing, Some(0), &AtomicBool::new(false), |_, _, _| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("limit must be positive"));

        let bytes = stream(&[VALID_CONFIG, VALID_DATA, SECOND_DATA]);
        std::fs::write(&path, &bytes).unwrap();
        let error = inspect_file(&path, Some(1), &AtomicBool::new(false), |_, _, _| {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(&SECOND_DATA)?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("extent changed"));

        std::fs::write(&path, &bytes).unwrap();
        let error = inspect_file(&path, Some(1), &AtomicBool::new(false), |_, _, _| {
            use std::io::{Seek as _, SeekFrom, Write as _};
            let mut output = std::fs::OpenOptions::new().write(true).open(&path)?;
            output.seek(SeekFrom::Start(0))?;
            output.write_all(&[0])?;
            output.sync_data()?;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("content or extent changed"));

        let stop = AtomicBool::new(true);
        let missing = directory.path().join("stopped-before-open.frames");
        let error = inspect_file(&missing, Some(1), &stop, |_, _, _| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("before opening"));

        std::fs::write(&path, &bytes).unwrap();
        let stop = AtomicBool::new(false);
        let error = inspect_file(&path, Some(1), &stop, |_, _, _| {
            stop.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("after the selected prefix"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_validation_keeps_the_pinned_inode_after_path_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile2.frames");
        let replacement = directory.path().join("replacement.frames");
        let displaced = directory.path().join("displaced.frames");
        let bytes = stream(&[VALID_CONFIG, VALID_DATA, SECOND_DATA]);
        std::fs::write(&path, &bytes).unwrap();
        std::fs::write(&replacement, vec![0u8; bytes.len()]).unwrap();

        let expected = validate_stream(&bytes).unwrap();
        let result = scan_file(&path, &AtomicBool::new(false), u64::MAX, |record_number| {
            if record_number == 0 {
                std::fs::rename(&path, &displaced)?;
                std::fs::rename(&replacement, &path)?;
            }
            Ok(())
        });
        match result {
            Ok(actual) => assert_eq!(actual, expected),
            Err(error) => {
                assert!(format!("{error:#}").contains("content or extent changed"));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_validation_is_bounded_and_checks_cancellation_before_input_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("long-profile2.frames");
        let mut output = std::io::BufWriter::new(File::create(&path).unwrap());
        use std::io::Write as _;
        output.write_all(&VALID_CONFIG).unwrap();
        for record in 0..=1_000_000u64 {
            let mut data = VALID_DATA;
            data[8..16].copy_from_slice(&(record * 20 + FIRST_SOURCE_INDEX).to_le_bytes());
            output.write_all(&data).unwrap();
        }
        output.flush().unwrap();
        drop(output);

        let validated = validate_file(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(validated.data_records, 1_000_001);
        assert_eq!(validated.first_source_index, FIRST_SOURCE_INDEX);
        assert_eq!(validated.last_source_index, 20_000_019);
        assert_eq!(
            validated.processed_bytes,
            1_000_002 * u64::try_from(RECORD_BYTES).unwrap()
        );

        let stop = AtomicBool::new(true);
        let error = validate_file(Path::new("/input/must/not/be/opened"), &stop).unwrap_err();
        assert!(format!("{error:#}").contains("before opening"));

        let stop = AtomicBool::new(false);
        let error = scan_file(&path, &stop, u64::MAX, |record_number| {
            if record_number == 0 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted at record 1"));

        let final_only = directory.path().join("two-record-profile2.frames");
        std::fs::write(&final_only, stream(&[VALID_CONFIG, VALID_DATA])).unwrap();
        let stop = AtomicBool::new(false);
        let error = scan_file(&final_only, &stop, u64::MAX, |record_number| {
            if record_number == 1 {
                stop.store(true, Ordering::Relaxed);
            }
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("after the final record"));
    }

    #[test]
    fn preserves_diagnostic_config_but_strict_validation_rejects_it() {
        assert_eq!(
            decode(&ERROR_CONFIG).unwrap(),
            Record::Config(Config {
                status: Status {
                    clipped_channels: 0,
                    loss: false,
                    configuration_error: true,
                    rx2: false,
                    epoch_tag: 0,
                },
                epoch: 0,
                required_source_sample_rate_hz: 20_000_000,
                phase_words: [0; 4],
            })
        );
        assert!(validate_stream(&stream(&[ERROR_CONFIG, VALID_DATA])).is_err());
    }

    #[test]
    fn rejects_bad_headers_byte_swaps_and_profile_crossovers() {
        assert!(decode(&VALID_CONFIG[..31]).is_err());
        assert!(decode(&[0; 33]).is_err());

        for offset in [0, 1, 2, 3] {
            let mut bad = VALID_CONFIG;
            bad[offset] ^= 0x01;
            assert!(decode(&bad).is_err(), "magic byte {offset}");
        }

        let mut bad = VALID_CONFIG;
        bad.swap(0, 1);
        bad.swap(2, 3);
        assert!(decode(&bad).is_err());
        let mut bad = VALID_CONFIG;
        bad[..4].rotate_left(2);
        assert!(decode(&bad).is_err());

        let mut bad = VALID_CONFIG;
        bad[5] = 0x10;
        assert!(decode(&bad).is_err());
        let mut bad = VALID_CONFIG;
        bad[5] = 0x30;
        assert!(decode(&bad).is_err());

        let mut bad = VALID_CONFIG;
        bad[6] = 0x01;
        assert!(decode(&bad).is_err());
        let mut bad = VALID_CONFIG;
        bad.swap(6, 7);
        assert!(decode(&bad).is_err());

        assert!(crate::transport::parse(&VALID_DATA).is_err());
        assert!(crate::transport::parse(&VALID_V1_DATA).is_ok());
        assert!(decode(&VALID_V1_DATA).is_err());
    }

    #[test]
    fn rejects_invalid_config_fields_rate_and_status() {
        let mut bad = VALID_CONFIG;
        bad[8..12].fill(0);
        assert!(validate_stream(&stream(&[bad, VALID_DATA])).is_err());

        let mut bad = VALID_CONFIG;
        bad[4] = 0x30;
        assert!(validate_stream(&stream(&[bad, VALID_DATA])).is_err());

        let mut bad = VALID_CONFIG;
        bad[8] = 0x05;
        assert!(validate_stream(&stream(&[bad, VALID_DATA])).is_err());

        for invalid_rate in [
            [0x00, 0x00, 0x00, 0x00],
            [0xff, 0x2c, 0x31, 0x01],
            [0x01, 0x2d, 0x31, 0x01],
            [0x01, 0x31, 0x2d, 0x00],
            [0x31, 0x01, 0x00, 0x2d],
        ] {
            let mut bad = VALID_CONFIG;
            bad[12..16].copy_from_slice(&invalid_rate);
            assert!(validate_stream(&stream(&[bad, VALID_DATA])).is_err());
        }

        for asserted_clip in [0x01, 0x02, 0x04, 0x08] {
            let mut bad = VALID_CONFIG;
            bad[5] |= asserted_clip;
            assert!(
                validate_stream(&stream(&[bad, VALID_DATA])).is_err(),
                "clip flag {asserted_clip:#04x}"
            );
        }
        for asserted_status in [0x40, 0x80] {
            let mut bad = VALID_CONFIG;
            bad[4] |= asserted_status;
            assert!(
                validate_stream(&stream(&[bad, VALID_DATA])).is_err(),
                "status flag {asserted_status:#04x}"
            );
        }
    }

    #[test]
    fn rejects_bad_record_order_duplicate_config_and_config_only() {
        assert!(validate_stream(&VALID_DATA).is_err());
        assert!(validate_stream(&VALID_CONFIG).is_err());
        assert!(validate_stream(&stream(&[VALID_CONFIG, VALID_CONFIG])).is_err());
        assert!(validate_stream(&stream(&[VALID_CONFIG, VALID_DATA, VALID_CONFIG])).is_err());
    }

    #[test]
    fn rejects_data_source_tag_flags_and_index_discontinuities() {
        let mut bad = VALID_DATA;
        bad[4] ^= 0x20;
        assert!(validate_stream(&stream(&[VALID_CONFIG, bad])).is_err());

        let mut bad = VALID_DATA;
        bad[4] = 0x25;
        assert!(validate_stream(&stream(&[VALID_CONFIG, bad])).is_err());

        for asserted_clip in [0x01, 0x02, 0x04, 0x08] {
            let mut bad = VALID_DATA;
            bad[5] |= asserted_clip;
            assert!(
                validate_stream(&stream(&[VALID_CONFIG, bad])).is_err(),
                "clip flag {asserted_clip:#04x}"
            );
        }
        for asserted_status in [0x40, 0x80] {
            let mut bad = VALID_DATA;
            bad[4] |= asserted_status;
            assert!(
                validate_stream(&stream(&[VALID_CONFIG, bad])).is_err(),
                "status flag {asserted_status:#04x}"
            );
        }

        let mut bad = VALID_DATA;
        bad[8] = 0x27;
        assert!(validate_stream(&stream(&[VALID_CONFIG, bad])).is_err());
        let mut bad = VALID_DATA;
        bad[8] = 0x14;
        assert!(validate_stream(&stream(&[VALID_CONFIG, bad])).is_err());
        let mut bad = VALID_DATA;
        bad[8..16].reverse();
        assert!(validate_stream(&stream(&[VALID_CONFIG, bad])).is_err());

        let mut gap = SECOND_DATA;
        gap[8] = 0x3b;
        assert!(validate_stream(&stream(&[VALID_CONFIG, VALID_DATA, gap])).is_err());
        assert!(validate_stream(&stream(&[VALID_CONFIG, VALID_DATA, VALID_DATA])).is_err());
        assert!(
            validate_stream(&stream(&[
                VALID_CONFIG,
                VALID_DATA,
                SECOND_DATA,
                VALID_DATA
            ]))
            .is_err()
        );

        let Record::Config(config) = decode(&VALID_CONFIG).unwrap() else {
            unreachable!()
        };
        let Record::Data(data) = decode(&VALID_DATA).unwrap() else {
            unreachable!()
        };
        let mut overflow_state = StreamState {
            config: Some(config),
            data_records: 1,
            first_source_index: Some(FIRST_SOURCE_INDEX),
            previous_source_index: Some(u64::MAX - 16),
        };
        assert!(overflow_state.push(u64::MAX, Record::Data(data)).is_err());
    }

    #[test]
    fn rejects_empty_and_incomplete_streams() {
        assert!(validate_stream(&[]).is_err());
        for length in 1..RECORD_BYTES {
            assert!(validate_stream(&VALID_CONFIG[..length]).is_err());
        }

        let bytes = stream(&[VALID_CONFIG, VALID_DATA]);
        for length in [33, 63] {
            assert!(validate_stream(&bytes[..length]).is_err());
        }
        let mut trailing_partial = bytes;
        trailing_partial.push(0);
        assert_eq!(trailing_partial.len(), 65);
        assert!(validate_stream(&trailing_partial).is_err());
    }
}
