//! Replay bounded IQ with the pinned native receiver and publish checked MPDUs.

use std::cell::Cell;
use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags, open};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::ModernPhy;

use super::{
    DecodeSettings, FILTER_FREE_SHIFT_HZ, IqFormat, MAX_SAMPLES, PcapOutput, Q11Gain,
    SAMPLE_RATE_HZ, antiwrap, cs8_reader, modern_process, modern_records,
};

const RECEIVER_REVISION: &str = "00f1fed7ed8984cfc89360fd7ffbe19ba6c36a5b";

pub(crate) struct Request<'a> {
    pub(crate) input: &'a Path,
    pub(crate) directory: &'a Path,
    pub(crate) decoder: &'a Path,
    pub(crate) format: IqFormat,
    pub(crate) frequency_hz: u64,
    pub(crate) capture_frequency_hz: Option<u64>,
    pub(crate) max_samples: u64,
    pub(crate) double_q11: bool,
    pub(crate) budget: Duration,
    pub(crate) stop: &'a AtomicBool,
    pub(crate) phy: ModernPhy,
}

#[derive(Serialize)]
pub(crate) struct Evidence {
    schema_version: u8,
    operation: &'static str,
    reference_receiver_revision: &'static str,
    iq_format: IqFormat,
    center_frequency_hz: u64,
    sample_rate_hz: u32,
    input_samples: u64,
    processed_samples: u64,
    sample_limit: u64,
    staged_cs8_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_frequency_hz: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_shift_hz: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    translation_filter: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    translation_clipped_samples: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    q11_gain: Option<Q11Gain>,
    receiver: modern_process::NativeEvidence,
    #[serde(flatten)]
    counts: Counts,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Counts {
    He {
        he_su_frames: u64,
        total_frames: u64,
        he_su_headers: u64,
    },
    Vht {
        vht_su_frames: u64,
        total_frames: u64,
        vht_su_headers: u64,
    },
}

fn same_source(initial: &Metadata, current: &Metadata) -> bool {
    current.is_file()
        && initial.dev() == current.dev()
        && initial.ino() == current.ino()
        && initial.len() == current.len()
        && initial.mtime() == current.mtime()
        && initial.mtime_nsec() == current.mtime_nsec()
        && initial.ctime() == current.ctime()
        && initial.ctime_nsec() == current.ctime_nsec()
}

/// Require EOF on a stable staged CS8 prefix. Scratch IQ and native records
/// are removed on return. The bounded stderr log stays available.
pub(crate) fn replay(request: Request<'_>) -> Result<Evidence> {
    let start = Instant::now();
    ensure!(
        (1..=MAX_SAMPLES).contains(&request.max_samples),
        "modern Wi-Fi decode sample limit must be 1..={MAX_SAMPLES}"
    );
    let file = File::from(
        open(
            request.input,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("open direct modern Wi-Fi replay IQ")?,
    );
    let initial = file.metadata()?;
    let bytes_per_sample = request.format.bytes_per_sample() as u64;
    ensure!(
        initial.is_file() && initial.len() > 0 && initial.len().is_multiple_of(bytes_per_sample),
        "modern Wi-Fi replay requires a nonempty regular file of complete IQ samples"
    );
    let input_samples = initial.len() / bytes_per_sample;
    let processed_samples = input_samples.min(request.max_samples);
    let shift = crate::replay_frequency_shift(request.capture_frequency_hz, request.frequency_hz)?;
    let translation_clipped = Cell::new(0);
    let q11_gain_clipped = Cell::new(0);
    let q11_gain = request.double_q11.then_some(&q11_gain_clipped);
    let scratch = tempfile::Builder::new()
        .prefix(".wifi-replay-")
        .tempdir_in(request.directory)?;
    let iq = scratch.path().join("input.cs8");
    let records = scratch.path().join("receiver.jsonl");
    let mut reader = cs8_reader(
        file,
        request.format,
        processed_samples,
        shift,
        &translation_clipped,
        q11_gain,
    )?;
    let mut staged = crate::new_file(&iq)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut remaining = processed_samples * 2;
    while remaining != 0 {
        ensure!(
            !request.stop.load(Ordering::Relaxed) && start.elapsed() < request.budget,
            "Wi-Fi IQ staging interrupted or exceeded its time budget"
        );
        let count = remaining.min(buffer.len() as u64) as usize;
        let chunk = &mut buffer[..count];
        reader
            .read_exact(chunk)
            .context("convert admitted modern Wi-Fi replay IQ")?;
        staged.write_all(chunk)?;
        hash.update(&*chunk);
        remaining -= count as u64;
    }
    staged.sync_all()?;
    drop(staged);
    drop(reader);
    ensure!(
        same_source(&initial, &fs::symlink_metadata(request.input)?),
        "modern Wi-Fi replay source changed during staging"
    );
    let staged_cs8_sha256 = format!("{:x}", hash.finalize());
    let receiver = modern_process::run(modern_process::Replay {
        program: request.decoder,
        input: &iq,
        frequency_hz: request.frequency_hz,
        samples: processed_samples,
        records: &records,
        diagnostics: &request.directory.join("decoder.log"),
        budget: request.budget.saturating_sub(start.elapsed()),
        interrupted: request.stop,
    })?;
    let mut pcap = PcapOutput::new(
        &request.directory.join("candidates.pcap"),
        DecodeSettings {
            frame_spans: true,
            ..DecodeSettings::default()
        },
    )?;
    let counts = modern_records::parse(
        BufReader::new(File::open(records)?),
        request.frequency_hz,
        processed_samples,
        request.phy,
        |bytes, start_sample, end_sample_exclusive| {
            pcap.write_verified_mpdu(bytes, start_sample, end_sample_exclusive, None)
        },
    )?;
    ensure!(
        !request.stop.load(Ordering::Relaxed) && start.elapsed() < request.budget,
        "modern Wi-Fi replay interrupted or exceeded its time budget before publication"
    );
    pcap.sync()?;
    pcap.frame_spans
        .as_mut()
        .context("modern Wi-Fi replay did not allocate frame spans")?
        .publish(input_samples, processed_samples, counts.selected_frames)?;
    pcap.publish_pcap()?;
    let (operation, counts) = match request.phy {
        ModernPhy::He20Su => (
            "offline_wifi_he20_su_payload_decode",
            Counts::He {
                he_su_frames: counts.selected_frames,
                total_frames: counts.total_frames,
                he_su_headers: counts.selected_headers,
            },
        ),
        ModernPhy::Vht20Su => (
            "offline_wifi_vht20_su_payload_decode",
            Counts::Vht {
                vht_su_frames: counts.selected_frames,
                total_frames: counts.total_frames,
                vht_su_headers: counts.selected_headers,
            },
        ),
    };
    Ok(Evidence {
        schema_version: 1,
        operation,
        reference_receiver_revision: RECEIVER_REVISION,
        iq_format: request.format,
        center_frequency_hz: request.frequency_hz,
        sample_rate_hz: SAMPLE_RATE_HZ,
        input_samples,
        processed_samples,
        sample_limit: request.max_samples,
        staged_cs8_sha256,
        capture_frequency_hz: request.capture_frequency_hz.filter(|_| shift.is_some()),
        frequency_shift_hz: shift,
        translation_filter: shift
            .filter(|value| value.unsigned_abs() > FILTER_FREE_SHIFT_HZ)
            .map(|_| antiwrap::FILTER_NAME),
        translation_clipped_samples: shift.map(|_| translation_clipped.get()),
        q11_gain: q11_gain.map(|clipped| Q11Gain {
            factor: super::DOUBLE_Q11_FACTOR,
            clipped_components: clipped.get(),
        }),
        receiver,
        counts,
    })
}
