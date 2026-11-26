//! Bounded saved IQ reads and centered frequency translation.

use std::f64::consts::TAU;
use std::fs::{File, Metadata};
use std::io::{BufReader, Read, Seek};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use ring::digest::{self, SHA256};
use rustfft::num_complex::Complex;

use crate::{SAMPLE_RATE, file_pin};

use super::check_stop;

pub(super) const MAX_INPUT_SAMPLES: usize = 245_760;
pub(super) const DECIMATION: usize = 10;
pub(super) const BASEBAND_RATE: u32 = SAMPLE_RATE / DECIMATION as u32;
pub(super) const FILTER_HALF_TAPS: usize = 400;
pub(super) const FILTER_CUTOFF_HZ: u64 = 350_000;

pub(super) struct SavedIq {
    input: File,
    before: Metadata,
    source_samples: usize,
}

impl SavedIq {
    pub(super) fn open(path: &Path, max_samples: usize, stop: &AtomicBool) -> Result<Self> {
        check_stop(stop)?;
        let (input, length) = file_pin::open_regular(path, (4 * max_samples) as u64)?;
        let source = Self::from_file(input, max_samples)?;
        ensure!(
            source.before.len() == length,
            "waveform clip extent changed before reading"
        );
        Ok(source)
    }

    /// Read from sample zero of the supplied inode. The clone shares its cursor.
    pub(super) fn from_pinned(file: &File, max_samples: usize, stop: &AtomicBool) -> Result<Self> {
        check_stop(stop)?;
        Self::from_file(
            file.try_clone().context("clone pinned IQ descriptor")?,
            max_samples,
        )
    }

    fn from_file(mut input: File, max_samples: usize) -> Result<Self> {
        let before = input.metadata()?;
        let length = before.len();
        ensure!(
            before.is_file() && (1..=(4 * max_samples) as u64).contains(&length),
            "saved IQ must be a nonempty regular file of at most {max_samples} complex samples"
        );
        ensure!(length.is_multiple_of(4), "clip ends within an I/Q sample");
        input.rewind().context("seek to the start of saved IQ")?;
        Ok(Self {
            input,
            before,
            source_samples: length as usize / 4,
        })
    }

    pub(super) fn source_samples(&self) -> usize {
        self.source_samples
    }

    fn finish(&mut self, whole_file: bool, stop: &AtomicBool) -> Result<()> {
        if whole_file {
            ensure!(
                self.input.read(&mut [0])? == 0,
                "waveform clip grew while reading"
            );
        }
        let after = self.input.metadata()?;
        let before = &self.before;
        ensure!(
            (
                before.len(),
                before.mtime(),
                before.mtime_nsec(),
                before.ctime(),
                before.ctime_nsec()
            ) == (
                after.len(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec()
            ),
            "waveform clip changed while reading"
        );
        check_stop(stop)
    }

    /// Visit complete windows in file order and hash the selected prefix.
    /// A partial final window is validated and hashed without being visited.
    pub(super) fn read_windows<const SIZE: usize>(
        mut self,
        samples: usize,
        stop: &AtomicBool,
        mut visit: impl FnMut(&[Complex<f64>; SIZE]),
    ) -> Result<String> {
        ensure!(
            (1..=self.source_samples).contains(&samples),
            "waveform prefix must be nonempty and inside the saved IQ"
        );
        let mut digest = digest::Context::new(&SHA256);
        let mut raw = vec![0; 4 * SIZE];
        let mut window = [Complex::default(); SIZE];
        let mut input =
            BufReader::with_capacity(65_536, (&mut self.input).take(4 * samples as u64));
        for first in (0..samples).step_by(SIZE) {
            check_stop(stop)?;
            let count = SIZE.min(samples - first);
            let bytes = &mut raw[..4 * count];
            input
                .read_exact(bytes)
                .context("read saved waveform samples")?;
            digest.update(bytes);
            for (offset, (sample, &bytes)) in
                window.iter_mut().zip(bytes.as_chunks::<4>().0).enumerate()
            {
                *sample = parse_q11_sample(bytes, first + offset)?;
            }
            if count == SIZE {
                visit(&window);
            }
        }
        drop(input);
        self.finish(samples == self.source_samples, stop)?;
        Ok(HEXLOWER.encode(digest.finish().as_ref()))
    }
}

fn parse_q11_sample([i0, i1, q0, q1]: [u8; 4], index: usize) -> Result<Complex<f64>> {
    let i = i16::from_le_bytes([i0, i1]);
    let q = i16::from_le_bytes([q0, q1]);
    ensure!(
        (-2048..=2047).contains(&i) && (-2048..=2047).contains(&q),
        "waveform clip sample {index} is outside the SC16 Q11 range"
    );
    Ok(Complex::new(f64::from(i), f64::from(q)))
}

pub(super) struct Baseband {
    pub(super) source_sha256: String,
    pub(super) source_samples: usize,
    pub(super) samples: Vec<Complex<f64>>,
}

pub(super) enum ClipInput<'a> {
    Path(&'a Path),
    Bytes(Vec<u8>),
}

pub(super) struct SavedClip {
    pub(super) source_sha256: String,
    bytes: Vec<u8>,
}

impl SavedClip {
    fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        ensure!(
            (1..=4 * MAX_INPUT_SAMPLES).contains(&bytes.len()),
            "saved IQ must contain 1 through {MAX_INPUT_SAMPLES} complex samples"
        );
        ensure!(
            bytes.len().is_multiple_of(4),
            "clip ends within an I/Q sample"
        );
        Ok(Self {
            source_sha256: HEXLOWER.encode(digest::digest(&SHA256, &bytes).as_ref()),
            bytes,
        })
    }

    pub(super) fn source_samples(&self) -> usize {
        self.bytes.len() / 4
    }

    /// Check the complete Q11 representation and return raw counts in the requested span.
    pub(super) fn samples(
        &self,
        extent: Range<usize>,
        stop: &AtomicBool,
    ) -> Result<Vec<Complex<f64>>> {
        ensure!(
            extent.start < extent.end && extent.end <= self.source_samples(),
            "waveform interval must be nonempty and inside the saved clip"
        );
        let mut samples = Vec::with_capacity(extent.len());
        for (index, &bytes) in self.bytes.as_chunks::<4>().0.iter().enumerate() {
            if index.is_multiple_of(16_384) {
                check_stop(stop)?;
            }
            let sample = parse_q11_sample(bytes, index)?;
            if extent.contains(&index) {
                samples.push(sample);
            }
        }
        Ok(samples)
    }
}

pub(super) fn read_clip(path: &Path, stop: &AtomicBool) -> Result<SavedClip> {
    let mut source = SavedIq::open(path, MAX_INPUT_SAMPLES, stop)?;
    let mut bytes = vec![0; 4 * source.source_samples()];
    source
        .input
        .read_exact(&mut bytes)
        .context("read saved waveform clip")?;
    source.finish(true, stop)?;
    SavedClip::from_bytes(bytes)
}

fn low_pass() -> Vec<f64> {
    let normalized = 2.0 * FILTER_CUTOFF_HZ as f64 / f64::from(SAMPLE_RATE);
    let mut taps: Vec<_> = (0..=2 * FILTER_HALF_TAPS)
        .map(|index| {
            let distance = index as f64 - FILTER_HALF_TAPS as f64;
            let argument = std::f64::consts::PI * normalized * distance;
            let sinc = if index == FILTER_HALF_TAPS {
                1.0
            } else {
                argument.sin() / argument
            };
            let hamming = 0.54 - 0.46 * (TAU * index as f64 / (2 * FILTER_HALF_TAPS) as f64).cos();
            normalized * sinc * hamming
        })
        .collect();
    let gain = taps.iter().sum::<f64>();
    taps.iter_mut().for_each(|tap| *tap /= gain);
    taps
}

/// Output sample zero is centered on the first requested input sample.
pub(super) fn channel(
    input: ClipInput<'_>,
    center_hz: u64,
    target_hz: u64,
    extent: Range<usize>,
    stop: &AtomicBool,
) -> Result<Baseband> {
    check_stop(stop)?;
    let half_rate = u64::from(SAMPLE_RATE) / 2;
    ensure!(
        center_hz > 0 && target_hz > 0 && center_hz.abs_diff(target_hz) <= half_rate - 500_000,
        "waveform target requires 500 kHz of margin inside the sampled band"
    );
    let clip = match input {
        ClipInput::Path(path) => read_clip(path, stop)?,
        ClipInput::Bytes(bytes) => SavedClip::from_bytes(bytes)?,
    };
    let source_samples = clip.source_samples();
    let padded_end = extent
        .end
        .checked_add(FILTER_HALF_TAPS)
        .context("clip extent overflow")?;
    ensure!(
        extent.start >= FILTER_HALF_TAPS
            && extent.start < extent.end
            && padded_end <= source_samples,
        "waveform interval needs {FILTER_HALF_TAPS} saved context samples on each side"
    );
    let padded_start = extent.start - FILTER_HALF_TAPS;
    let offset = center_hz.abs_diff(target_hz) as f64;
    let sign = if target_hz < center_hz { 1.0 } else { -1.0 };
    let radians = sign * TAU * offset / f64::from(SAMPLE_RATE);
    let mut mixed = clip.samples(padded_start..padded_end, stop)?;
    for (index, sample) in (padded_start..padded_end).zip(&mut mixed) {
        let oscillator = Complex::from_polar(1.0, radians * index as f64);
        *sample = *sample * oscillator / 2048.0;
    }

    let taps = low_pass();
    let mut samples = Vec::with_capacity(extent.len().div_ceil(DECIMATION));
    for offset in (0..extent.len()).step_by(DECIMATION) {
        if offset.is_multiple_of(DECIMATION * 256) {
            check_stop(stop)?;
        }
        let filtered = mixed[offset..offset + taps.len()]
            .iter()
            .zip(&taps)
            .map(|(&sample, &tap)| sample * tap)
            .sum();
        samples.push(filtered);
    }
    Ok(Baseband {
        source_sha256: clip.source_sha256,
        source_samples,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn centered_decimator_recovers_the_target_and_attenuates_an_adjacent_tone() {
        let mut input = tempfile::NamedTempFile::new().unwrap();
        let center = 2_437_000_000;
        let target = center - 1_250_000;
        for index in 0..16_384 {
            let time = f64::from(index) / f64::from(SAMPLE_RATE);
            let signal = Complex::from_polar(600.0, -TAU * 1_250_000.0 * time)
                + Complex::from_polar(600.0, -TAU * 700_000.0 * time);
            for component in [signal.re, signal.im] {
                input
                    .write_all(&(component.round() as i16).to_le_bytes())
                    .unwrap();
            }
        }
        let result = channel(
            ClipInput::Path(input.path()),
            center,
            target,
            400..15_000,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result.samples.len(), 1460);
        let expected = Complex::new(600.0 / 2048.0, 0.0);
        for value in result.samples {
            assert!((value - expected).norm() < 0.002);
        }
    }
}
