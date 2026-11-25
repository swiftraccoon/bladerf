//! Rotate complete IQ pairs before conversion into the decoder's CS8 input.

use std::cell::Cell;
use std::f64::consts::TAU;
use std::io::{self, Read};

use super::{CHUNK_SAMPLES, IqFormat, MAX_Q11, MIN_Q11, SAMPLE_RATE_HZ, q11_component};

const REANCHOR_SAMPLES: u32 = 1024;

pub(super) struct Rotation {
    step_ticks: u32,
    ticks: u32,
    since_anchor: u32,
    step: (f64, f64),
    phasor: (f64, f64),
}

impl Rotation {
    pub(super) fn new(shift_hz: i64) -> Self {
        let step_ticks = shift_hz.rem_euclid(i64::from(SAMPLE_RATE_HZ)) as u32;
        Self {
            step_ticks,
            ticks: 0,
            since_anchor: 0,
            step: Self::phasor(step_ticks),
            phasor: (0.0, 1.0),
        }
    }

    fn phasor(ticks: u32) -> (f64, f64) {
        (TAU * f64::from(ticks) / f64::from(SAMPLE_RATE_HZ)).sin_cos()
    }

    pub(super) fn rotate(&mut self, i: f64, q: f64) -> [f64; 2] {
        let (sine, cosine) = self.phasor;
        let result = [
            (cosine * i - sine * q).round_ties_even(),
            (sine * i + cosine * q).round_ties_even(),
        ];
        self.ticks += self.step_ticks;
        if self.ticks >= SAMPLE_RATE_HZ {
            self.ticks -= SAMPLE_RATE_HZ;
        }
        self.since_anchor += 1;
        // Anchor to the exact sample phase, independent of caller read sizes.
        if self.since_anchor == REANCHOR_SAMPLES || self.ticks == 0 {
            self.phasor = Self::phasor(self.ticks);
            self.since_anchor = 0;
        } else {
            let (step_sine, step_cosine) = self.step;
            self.phasor = (
                sine * step_cosine + cosine * step_sine,
                cosine * step_cosine - sine * step_sine,
            );
        }
        result
    }
}

pub(super) fn iq_pair(
    sample: &[u8],
    format: IqFormat,
    gain_clipped: Option<&Cell<u64>>,
) -> io::Result<[i16; 2]> {
    Ok(match format {
        IqFormat::Sc16Q11 => [
            q11_component([sample[0], sample[1]], gain_clipped)?,
            q11_component([sample[2], sample[3]], gain_clipped)?,
        ],
        IqFormat::Cs8 => [
            i16::from(sample[0] as i8) << 4,
            i16::from(sample[1] as i8) << 4,
        ],
    })
}

pub(super) fn quantize_cs8(rounded: [f64; 2]) -> ([u8; 2], bool) {
    let q11_range = f64::from(MIN_Q11)..=f64::from(MAX_Q11);
    let clipped = rounded.iter().any(|value| !q11_range.contains(value));
    let pair = rounded.map(|value| {
        let q11 = value.clamp(f64::from(MIN_Q11), f64::from(MAX_Q11)) as i16;
        (q11 >> 4) as i8 as u8
    });
    (pair, clipped)
}

pub(super) struct TranslatedReader<'a, R> {
    input: R,
    format: IqFormat,
    raw: [u8; CHUNK_SAMPLES * 4],
    rotation: Rotation,
    pending_q: Option<(u8, bool)>,
    clipped: &'a Cell<u64>,
    gain_clipped: Option<&'a Cell<u64>>,
}

impl<'a, R: Read> TranslatedReader<'a, R> {
    pub(super) fn new(
        input: R,
        format: IqFormat,
        shift_hz: i64,
        clipped: &'a Cell<u64>,
        gain_clipped: Option<&'a Cell<u64>>,
    ) -> Self {
        Self {
            input,
            format,
            raw: [0; CHUNK_SAMPLES * 4],
            rotation: Rotation::new(shift_hz),
            pending_q: None,
            clipped,
            gain_clipped,
        }
    }
}

impl<R: Read> Read for TranslatedReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if let Some((q, clipped)) = self.pending_q.take() {
            output[0] = q;
            self.clipped.set(self.clipped.get() + u64::from(clipped));
            return Ok(1);
        }
        let bytes_per_sample = self.format.bytes_per_sample();
        let samples = output.len().div_ceil(2).min(CHUNK_SAMPLES);
        let raw = &mut self.raw[..samples * bytes_per_sample];
        let mut filled = 0;
        while filled < raw.len() {
            match self.input.read(&mut raw[filled..]) {
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(error) => return Err(error),
            }
        }
        if !filled.is_multiple_of(bytes_per_sample) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IQ input ends within a complete I/Q sample",
            ));
        }
        let mut written = 0;
        let mut clipped_count = 0;
        for sample in raw[..filled].chunks_exact(bytes_per_sample) {
            let [i, q] = iq_pair(sample, self.format, self.gain_clipped)?;
            let rotated = self.rotation.rotate(f64::from(i), f64::from(q));
            let ([i, q], clipped) = quantize_cs8(rotated);
            output[written] = i;
            written += 1;
            if written == output.len() {
                self.pending_q = Some((q, clipped));
            } else {
                output[written] = q;
                written += 1;
                clipped_count += u64::from(clipped);
            }
        }
        self.clipped.set(self.clipped.get() + clipped_count);
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ShortReads<'a>(&'a [u8]);

    impl Read for ShortReads<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let count = output.len().min(3);
            self.0.read(&mut output[..count])
        }
    }

    #[test]
    fn rotation_preserves_pairs_and_phase_across_odd_reads() {
        let sample = [2016, 1984];
        for format in [IqFormat::Sc16Q11, IqFormat::Cs8] {
            let pair: Vec<_> = match format {
                IqFormat::Sc16Q11 => sample.into_iter().flat_map(i16::to_le_bytes).collect(),
                IqFormat::Cs8 => sample.into_iter().map(|value| (value >> 4) as u8).collect(),
            };
            let input = pair.repeat(4100);
            for shift in [-1_000_000, 123_457] {
                let clipped = Cell::new(0);
                let mut source =
                    TranslatedReader::new(ShortReads(&input), format, shift, &clipped, None);
                let mut actual = Vec::new();
                let mut buffer = [0; 117];
                loop {
                    let count = source.read(&mut buffer).unwrap();
                    if count == 0 {
                        break;
                    }
                    actual.extend_from_slice(&buffer[..count]);
                }
                let mut expected = Vec::new();
                let mut saturation_count = 0;
                for index in 0..4100 {
                    let phase = (i64::from(index) * shift).rem_euclid(i64::from(SAMPLE_RATE_HZ));
                    let (sine, cosine) = (phase as f64 * TAU / f64::from(SAMPLE_RATE_HZ)).sin_cos();
                    let [i, q] = sample.map(f64::from);
                    let rotated =
                        [cosine * i - sine * q, sine * i + cosine * q].map(f64::round_ties_even);
                    saturation_count += u64::from(
                        rotated
                            .iter()
                            .any(|&value| !(-2048.0..=2047.0).contains(&value)),
                    );
                    expected.extend(
                        rotated.map(|value| ((value.clamp(-2048.0, 2047.0) as i16) >> 4) as u8),
                    );
                }
                assert_eq!(actual, expected);
                assert!(saturation_count > 0);
                assert_eq!(clipped.get(), saturation_count);
            }
        }
    }

    #[test]
    fn gain_and_its_saturation_precede_translation() {
        let input: Vec<u8> = [1024_i16, 1024, -1024, 1000, 17, -17]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let pre_scaled: Vec<u8> = [2047_i16, 2047, -2048, 2000, 34, -34]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let translation_clipped = Cell::new(0);
        let expected_clipped = Cell::new(0);
        let gain_clipped = Cell::new(0);
        let mut actual = Vec::new();
        let mut expected = Vec::new();
        TranslatedReader::new(
            ShortReads(&input),
            IqFormat::Sc16Q11,
            1_000_000,
            &translation_clipped,
            Some(&gain_clipped),
        )
        .read_to_end(&mut actual)
        .unwrap();
        TranslatedReader::new(
            ShortReads(&pre_scaled),
            IqFormat::Sc16Q11,
            1_000_000,
            &expected_clipped,
            None,
        )
        .read_to_end(&mut expected)
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(gain_clipped.get(), 2);
        assert!(translation_clipped.get() > 0);
        assert_eq!(translation_clipped.get(), expected_clipped.get());
    }

    #[test]
    fn rejects_partial_and_out_of_range_q11_pairs() {
        let clipped = Cell::new(0);
        for input in [[1, 0, 0, 8].as_slice(), [1, 0, 0].as_slice()] {
            let mut source =
                TranslatedReader::new(input, IqFormat::Sc16Q11, 123_457, &clipped, None);
            assert_eq!(
                source.read(&mut [0; 2]).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
            );
        }
    }
}
