//! Align admitted sweep IQ to the Bluetooth decoder's whole-MHz grid.
//!
//! The caller owns the private scratch directory through replay verification.
//! Successful translation emits one Q11 sample per source sample. Its receipt
//! binds both inputs, including the source and derived sidecars. The routing
//! policy selects interior bins on both grids to exclude the Nyquist wrap region.

use std::f64::consts::TAU;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use serde::Serialize;
use serde_json::json;

use super::decoder_center;
use crate::identify_sweep::admission::SAMPLE_RATE_HZ;
use crate::input::{self, SourceReport};
use crate::report::{hash_direct_regular_file_with_snapshot, same_file_identity};

const MAX_SAMPLES: u64 = 200_000_000;
const MAX_SIDECAR_BYTES: u64 = 64 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const MIN_Q11: i16 = -2048;
const MAX_Q11: i16 = 2047;

#[derive(Debug, Serialize)]
pub(super) struct Translation {
    original_center_hz: u64,
    replay_center_hz: u64,
    applied_shift_hz: i64,
    complex_samples: u64,
    bytes: u64,
    original_iq_sha256: String,
    original_sidecar_sha256: String,
    replay_iq_sha256: String,
    replay_sidecar_sha256: String,
    clipped_components: u64,
}

pub(super) struct Prepared {
    pub(super) source: SourceReport,
    pub(super) provenance: Translation,
    iq_stamp: Metadata,
    sidecar_stamp: Metadata,
}

impl Prepared {
    /// Check the replay files' open-time identities before and after decoding.
    pub(super) fn verify(&self) -> Result<()> {
        verify_stamp(&self.source.input, &self.iq_stamp)?;
        verify_stamp(&self.source.sidecar, &self.sidecar_stamp)
    }
}

/// Prepare an exact-length private replay input or reuse the aligned source.
///
/// The caller releases scratch data on every exit path. A cancelled or failed
/// conversion cannot return a prepared input, even if it wrote partial data.
pub(super) fn prepare(
    original: &SourceReport,
    decoder_center_hz: u64,
    output: &Path,
    stop: &AtomicBool,
) -> Result<Prepared> {
    ensure_running(stop)?;
    ensure!(
        decoder_center(original.center_frequency_hz) == Some(decoder_center_hz),
        "Bluetooth replay center differs from the supported whole-MHz grid for this capture"
    );
    ensure!(
        original.format == input::NORMALIZED_SC16_Q11_FORMAT
            && original.sample_rate_hz == SAMPLE_RATE_HZ
            && original.bandwidth_hz == SAMPLE_RATE_HZ
            && (8192..=MAX_SAMPLES).contains(&original.complex_samples)
            && original.complex_samples.is_multiple_of(10)
            && original.bytes == original.complex_samples * 4,
        "Bluetooth translation requires complete 20 MSps Q11 in 10-sample steps"
    );
    let shift_hz = i64::try_from(original.center_frequency_hz)? - i64::try_from(decoder_center_hz)?;
    let original_iq = direct_file(&original.input)?;
    ensure!(
        original_iq.len() == original.bytes,
        "Bluetooth source extent changed after admission"
    );
    let (sidecar_digest, original_sidecar) =
        hash_direct_regular_file_with_snapshot(&original.sidecar, MAX_SIDECAR_BYTES)?;
    ensure!(
        sidecar_digest.sha256 == original.sidecar_sha256,
        "Bluetooth source sidecar content changed after admission"
    );

    let (source, iq_stamp, sidecar_stamp, clipped_components) = if shift_hz == 0 {
        (original.clone(), original_iq, original_sidecar, 0)
    } else {
        let input_path = output.join("translated.sc16q11");
        let sidecar_path = output.join("translated.sc16q11.json");
        let (replay_sha256, stamp, clipped_components) =
            translate(original, &original_iq, shift_hz, &input_path, stop)?;
        let sidecar = serde_json::to_vec(&json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": decoder_center_hz,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "num_samples": original.complex_samples,
        }))?;
        let mut file = private_file(&sidecar_path)?;
        file.write_all(&sidecar)
            .context("write translated IQ sidecar")?;
        file.sync_all().context("sync translated IQ sidecar")?;
        let sidecar_stamp = file.metadata()?;
        let sidecar_sha256 = HEXLOWER.encode(digest::digest(&SHA256, &sidecar).as_ref());
        File::open(output)?
            .sync_all()
            .context("sync Bluetooth replay input directory")?;
        verify_stamp(&original.sidecar, &original_sidecar)?;
        (
            SourceReport {
                input: input_path,
                sidecar: sidecar_path,
                center_frequency_hz: decoder_center_hz,
                sha256: replay_sha256,
                sidecar_sha256,
                ..original.clone()
            },
            stamp,
            sidecar_stamp,
            clipped_components,
        )
    };
    ensure_running(stop)?;
    let prepared = Prepared {
        provenance: Translation {
            original_center_hz: original.center_frequency_hz,
            replay_center_hz: source.center_frequency_hz,
            applied_shift_hz: shift_hz,
            complex_samples: source.complex_samples,
            bytes: source.bytes,
            original_iq_sha256: original.sha256.clone(),
            original_sidecar_sha256: original.sidecar_sha256.clone(),
            replay_iq_sha256: source.sha256.clone(),
            replay_sidecar_sha256: source.sidecar_sha256.clone(),
            clipped_components,
        },
        source,
        iq_stamp,
        sidecar_stamp,
    };
    prepared.verify()?;
    Ok(prepared)
}

fn ensure_running(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "Bluetooth translation interrupted"
    );
    Ok(())
}

fn direct_file(path: &Path) -> Result<Metadata> {
    let stamp = fs::symlink_metadata(path)
        .with_context(|| format!("inspect Bluetooth input {}", path.display()))?;
    ensure!(
        stamp.is_file(),
        "Bluetooth replay input must be a direct regular file"
    );
    Ok(stamp)
}

fn verify_stamp(path: &Path, expected: &Metadata) -> Result<()> {
    ensure!(
        same_file_identity(&direct_file(path)?, expected),
        "Bluetooth replay input changed: {}",
        path.display()
    );
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create Bluetooth replay input {}", path.display()))
}

fn translate(
    original: &SourceReport,
    original_stamp: &Metadata,
    shift_hz: i64,
    output: &Path,
    stop: &AtomicBool,
) -> Result<(String, Metadata, u64)> {
    let mut reader = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(&original.input)
        .context("open admitted Bluetooth source IQ")?;
    ensure!(
        same_file_identity(&reader.metadata()?, original_stamp),
        "Bluetooth input changed while opening"
    );
    let mut writer = BufWriter::with_capacity(CHUNK_BYTES, private_file(output)?);
    let mut mixer = Mixer::new(shift_hz);
    let mut input_digest = digest::Context::new(&SHA256);
    let mut output_digest = digest::Context::new(&SHA256);
    let mut buffer = [0; CHUNK_BYTES];
    let mut remaining = original.bytes;
    let mut clipped = 0;
    while remaining > 0 {
        ensure_running(stop)?;
        let count = usize::try_from(remaining.min(CHUNK_BYTES as u64))?;
        let chunk = &mut buffer[..count];
        reader
            .read_exact(chunk)
            .context("read admitted Bluetooth IQ")?;
        input::validate_q11(chunk)?;
        input_digest.update(chunk);
        for sample in chunk.as_chunks_mut::<4>().0 {
            let [i0, i1, q0, q1] = *sample;
            let iq = [i16::from_le_bytes([i0, i1]), i16::from_le_bytes([q0, q1])];
            let (rotated, clipped_components) = mixer.rotate(iq);
            let [[i0, i1], [q0, q1]] = rotated.map(i16::to_le_bytes);
            *sample = [i0, i1, q0, q1];
            clipped += clipped_components;
        }
        output_digest.update(chunk);
        writer
            .write_all(chunk)
            .context("write translated Bluetooth IQ")?;
        remaining -= count as u64;
    }
    let mut trailing = [0];
    ensure!(
        reader.read(&mut trailing)? == 0,
        "Bluetooth source grew while translating"
    );
    ensure!(
        same_file_identity(&reader.metadata()?, original_stamp)
            && HEXLOWER.encode(input_digest.finish().as_ref()) == original.sha256,
        "Bluetooth source IQ changed after admission"
    );
    verify_stamp(&original.input, original_stamp)?;
    ensure_running(stop)?;
    writer.flush().context("flush translated Bluetooth IQ")?;
    writer
        .get_ref()
        .sync_all()
        .context("sync translated Bluetooth IQ")?;
    let stamp = writer.get_ref().metadata()?;
    ensure!(
        stamp.len() == original.bytes,
        "translated Bluetooth IQ extent differs from source"
    );
    Ok((
        HEXLOWER.encode(output_digest.finish().as_ref()),
        stamp,
        clipped,
    ))
}

struct Mixer {
    phase_ticks: u64,
    step_ticks: u64,
    since_anchor: u16,
    phasor: (f64, f64),
    step: (f64, f64),
}

impl Mixer {
    fn new(shift_hz: i64) -> Self {
        Self {
            phase_ticks: 0,
            step_ticks: shift_hz.rem_euclid(SAMPLE_RATE_HZ as i64) as u64,
            since_anchor: 0,
            phasor: (0.0, 1.0),
            step: (TAU * shift_hz as f64 / SAMPLE_RATE_HZ as f64).sin_cos(),
        }
    }

    fn rotate(&mut self, iq: [i16; 2]) -> ([i16; 2], u64) {
        let (sine, cosine) = self.phasor;
        let [i, q] = iq.map(f64::from);
        let rounded = [cosine * i - sine * q, sine * i + cosine * q].map(f64::round_ties_even);
        let min = f64::from(MIN_Q11);
        let max = f64::from(MAX_Q11);
        let clipped = rounded
            .iter()
            .filter(|&&x| !(min..=max).contains(&x))
            .count() as u64;
        let converted = rounded.map(|x| x.clamp(min, max) as i16);

        self.phase_ticks += self.step_ticks;
        if self.phase_ticks >= SAMPLE_RATE_HZ {
            self.phase_ticks -= SAMPLE_RATE_HZ;
        }
        self.since_anchor += 1;
        // Anchor drift to the exact sample phase independently of read chunks.
        if self.since_anchor == 1024 || self.phase_ticks == 0 {
            self.phasor = (TAU * self.phase_ticks as f64 / SAMPLE_RATE_HZ as f64).sin_cos();
            self.since_anchor = 0;
        } else {
            let (step_sine, step_cosine) = self.step;
            self.phasor = (
                sine * step_cosine + cosine * step_sine,
                cosine * step_cosine - sine * step_sine,
            );
        }
        (converted, clipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CENTER_HZ: u64 = 2_437_000_000;
    const SAMPLES: usize = 20_000;

    fn original(dir: &Path, center_hz: u64) -> SourceReport {
        let path = dir.join("source.sc16q11");
        let raw = [2047_i16.to_le_bytes(), (-1800_i16).to_le_bytes()].concat();
        fs::write(&path, raw.repeat(SAMPLES)).unwrap();
        let sidecar = serde_json::to_vec(&json!({
            "format": "SC16_Q11",
            "dtype": "<i2",
            "interleave": "I,Q",
            "complex_scale": 2048,
            "frequency_hz": center_hz,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "bandwidth_hz": SAMPLE_RATE_HZ,
            "num_samples": SAMPLES,
        }))
        .unwrap();
        fs::write(dir.join("source.sc16q11.json"), sidecar).unwrap();
        input::validate(&path, center_hz, SAMPLE_RATE_HZ, SAMPLES as u64).unwrap()
    }

    #[test]
    fn signed_mixing_preserves_phase_and_extent_through_chunk_boundaries() {
        for shift_hz in [-500_000, 123_457] {
            let capture = tempfile::tempdir().unwrap();
            let scratch = tempfile::tempdir().unwrap();
            let source = original(
                capture.path(),
                CENTER_HZ.checked_add_signed(shift_hz).unwrap(),
            );
            let prepared =
                prepare(&source, CENTER_HZ, scratch.path(), &AtomicBool::new(false)).unwrap();
            let input = fs::read(&prepared.source.input).unwrap();
            let samples = input.as_chunks::<4>().0;
            assert_eq!(input.len(), SAMPLES * 4);
            assert_eq!(prepared.provenance.applied_shift_hz, shift_hz);

            let mut clipping = 0;
            for (index, &[i0, i1, q0, q1]) in samples.iter().enumerate() {
                let ticks = (index as i64 * shift_hz).rem_euclid(SAMPLE_RATE_HZ as i64);
                let (sine, cosine) = (TAU * ticks as f64 / SAMPLE_RATE_HZ as f64).sin_cos();
                let components = [
                    2047.0 * cosine + 1800.0 * sine,
                    2047.0 * sine - 1800.0 * cosine,
                ];
                let actual = [i16::from_le_bytes([i0, i1]), i16::from_le_bytes([q0, q1])];
                for (actual, expected) in actual.into_iter().zip(components) {
                    let expected = expected.round_ties_even();
                    clipping += u64::from(!(-2048.0..=2047.0).contains(&expected));
                    assert!(
                        (f64::from(actual) - expected.clamp(-2048.0, 2047.0)).abs() <= 1.0,
                        "phase differs at sample {index}"
                    );
                }
            }
            assert!(clipping > 0);
            assert_eq!(prepared.provenance.clipped_components, clipping);
            let admitted = input::validate(
                &prepared.source.input,
                CENTER_HZ,
                SAMPLE_RATE_HZ,
                SAMPLES as u64,
            )
            .unwrap();
            assert_eq!(admitted, prepared.source);
        }
    }

    #[test]
    fn reuses_aligned_source_and_detects_changed_replay_files() {
        let capture = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = original(capture.path(), CENTER_HZ);
        let prepared =
            prepare(&source, CENTER_HZ, scratch.path(), &AtomicBool::new(false)).unwrap();
        assert_eq!(prepared.source, source);
        assert_eq!(prepared.provenance.clipped_components, 0);
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        fs::OpenOptions::new()
            .append(true)
            .open(&source.sidecar)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        assert!(prepared.verify().is_err());
    }

    #[test]
    fn refuses_modified_source_content_or_cancellation() {
        let capture = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let source = original(capture.path(), CENTER_HZ + 10);
        let stopped = AtomicBool::new(true);
        assert!(prepare(&source, CENTER_HZ, scratch.path(), &stopped).is_err());
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);

        fs::write(&source.input, vec![0; source.bytes as usize]).unwrap();
        let result = prepare(&source, CENTER_HZ, scratch.path(), &AtomicBool::new(false));
        assert!(result.is_err());
        assert!(!scratch.path().join("translated.sc16q11.json").exists());
    }
}
