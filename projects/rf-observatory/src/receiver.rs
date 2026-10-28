//! Bounded file acquisition validation and multichannel reference reception.
//!
//! Native IQ is interleaved SC16 Q11 little-endian. Derived IQ uses signed i16
//! with the original nominal scale; the CIC's 8000/8192 gain remains explicit.
//! WAV files contain mono 50 kHz PCM, not calibrated RF power or stereo/RDS.
//! Synchronous processing checks cancellation/deadline between bounded chunks.
//! Private `.partial` files remain on failure; only completed outputs are linked
//! to final names without replacing existing files.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dsp::{self, FixedBank, FloatBank, HostPost, Iq16};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Acquisition {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: u16,
    frequency_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    gain_db: f64,
    gain_mode: String,
    bias_tee_enabled: bool,
    channel: String,
    num_samples: u64,
    started_utc: String,
}

impl Acquisition {
    pub(crate) fn frequency_hz(&self) -> u64 {
        self.frequency_hz.round() as u64
    }
}

pub(crate) fn validate_acquisition(
    iq: &Path,
    center: u64,
    samples: u64,
    antenna: u8,
    gain: f64,
) -> Result<Acquisition> {
    let mut bytes = Vec::new();
    File::open(iq.with_extension("sc16q11.json"))?
        .take(65_537)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65_536, "acquisition sidecar exceeds 64 KiB");
    let metadata: Acquisition = serde_json::from_slice(&bytes)?;
    ensure!(
        metadata.format == "SC16_Q11"
            && metadata.dtype == "<i2"
            && metadata.interleave == "I,Q"
            && metadata.complex_scale == 2048,
        "unsupported acquisition encoding"
    );
    ensure!(
        (metadata.sample_rate_hz - 20_000_000.0).abs() <= 1.0
            // The synthesizer has finite tuning resolution; retain its readback.
            && (metadata.frequency_hz - center as f64).abs() <= 10.0
            && (metadata.bandwidth_hz - 20_000_000.0).abs() <= 1.0,
        "acquisition tuning/rate/bandwidth differs from request"
    );
    ensure!(
        !metadata.bias_tee_enabled
            && metadata.gain_mode == "manual"
            && metadata.channel == format!("RX{antenna}")
            && (metadata.gain_db - gain).abs() <= 0.01,
        "acquisition receive state differs from request"
    );
    ensure!(
        metadata.num_samples == samples && fs::metadata(iq)?.len() == samples * 4,
        "acquisition count/byte length differs from request"
    );
    chrono::DateTime::parse_from_rfc3339(&metadata.started_utc)?;
    Ok(metadata)
}

pub(crate) fn input(path: &Path) -> Result<(File, u64)> {
    let before = fs::metadata(path)?;
    ensure!(before.is_file(), "IQ input must be a regular file");
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() % 4 == 0 && metadata.len() > 0,
        "IQ input must contain complete little-endian complex i16 samples"
    );
    Ok((file, metadata.len() / 4))
}

pub(crate) fn parse_sample(raw: &[u8]) -> Result<Iq16> {
    ensure!(raw.len() == 4, "one complex sample requires four bytes");
    let i = i16::from_le_bytes([raw[0], raw[1]]);
    let q = i16::from_le_bytes([raw[2], raw[3]]);
    ensure!(
        (-2048..=2047).contains(&i) && (-2048..=2047).contains(&q),
        "input contains a component outside SC16 Q11"
    );
    Ok(Iq16 { i, q })
}

#[derive(Debug, Serialize)]
pub(crate) struct ChannelReport {
    index: usize,
    tuning_word: u32,
    offset_hz: f64,
    channel_iq_samples: u64,
    audio_samples: u64,
    cic_clipped_samples: u64,
    post_iq_clipped_samples: u64,
    audio_clipped_samples: u64,
    low_power_samples: u64,
    integer_vs_float_rms_error: Option<f64>,
    audio_reference_rms_error: Option<f64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) input_samples: u64,
    available_input_samples: u64,
    sample_limit: u64,
    input_truncated_to_limit: bool,
    processed_prefix_sha256: String,
    source_sample_rate_hz: u32,
    cic_output_rate_hz: u32,
    channel_output_rate_hz: u32,
    audio_output_rate_hz: u32,
    elapsed_seconds: f64,
    channels: Vec<ChannelReport>,
    mode: &'static str,
    limitations: &'static str,
    cic_amplitude_at_100khz: f64,
    cic_amplitude_at_900khz: f64,
}

pub(crate) struct ChannelFiles {
    iq: BufWriter<File>,
    audio: BufWriter<File>,
    iq_stage: PathBuf,
    audio_stage: PathBuf,
    iq_destination: PathBuf,
    audio_destination: PathBuf,
    pub(crate) iq_samples: u64,
    pub(crate) audio_samples: u64,
    pub(crate) clipped: u64,
}

impl ChannelFiles {
    pub(crate) fn new(directory: &Path, index: usize) -> Result<Self> {
        let iq_destination = directory.join(format!("channel-{index}.iq250.cs16"));
        let audio_destination = directory.join(format!("channel-{index}.wav"));
        ensure!(
            !iq_destination.exists() && !audio_destination.exists(),
            "channel output already exists"
        );
        let iq_stage = directory.join(format!("channel-{index}.iq250.cs16.partial"));
        let audio_stage = directory.join(format!("channel-{index}.wav.partial"));
        let iq = BufWriter::new(crate::new_file(&iq_stage)?);
        let mut audio = BufWriter::new(crate::new_file(&audio_stage)?);
        audio.write_all(&wav_header(0)?)?;
        Ok(Self {
            iq,
            audio,
            iq_stage,
            audio_stage,
            iq_destination,
            audio_destination,
            iq_samples: 0,
            audio_samples: 0,
            clipped: 0,
        })
    }

    pub(crate) fn write(&mut self, sample: dsp::PostSample) -> Result<()> {
        let i = quantize(sample.iq.i, &mut self.clipped);
        let q = quantize(sample.iq.q, &mut self.clipped);
        self.iq.write_all(&i.to_le_bytes())?;
        self.iq.write_all(&q.to_le_bytes())?;
        self.iq_samples += 1;
        if let Some(audio) = sample.audio {
            self.audio.write_all(&audio.to_le_bytes())?;
            self.audio_samples += 1;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.iq.flush()?;
        self.iq.get_ref().sync_all()?;
        self.audio.flush()?;
        self.audio.seek(SeekFrom::Start(0))?;
        self.audio.write_all(&wav_header(self.audio_samples)?)?;
        self.audio.flush()?;
        self.audio.get_ref().sync_all()?;
        fs::hard_link(&self.iq_stage, &self.iq_destination)?;
        fs::hard_link(&self.audio_stage, &self.audio_destination)?;
        fs::remove_file(self.iq_stage)?;
        fs::remove_file(self.audio_stage)?;
        Ok(())
    }
}

fn quantize(value: f64, clipped: &mut u64) -> i16 {
    if value < i16::MIN as f64 || value > i16::MAX as f64 {
        *clipped += 1;
    }
    value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

fn wav_header(samples: u64) -> Result<Vec<u8>> {
    let bytes = u32::try_from(samples.checked_mul(2).context("WAV size overflow")?)?;
    let riff = bytes.checked_add(36).context("WAV RIFF size overflow")?;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&dsp::AUDIO_OUTPUT_RATE_HZ.to_le_bytes());
    header.extend_from_slice(&(dsp::AUDIO_OUTPUT_RATE_HZ * 2).to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&bytes.to_le_bytes());
    Ok(header)
}

pub(crate) fn receive(
    path: &Path,
    directory: &Path,
    words: &[u32],
    max_samples: u64,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<Report> {
    ensure!(
        (1..=crate::MAX_SAMPLES).contains(&max_samples),
        "invalid sample limit"
    );
    let (file, available) = input(path)?;
    let samples = available.min(max_samples);
    let mut reader = BufReader::new(file);
    let mut fixed = FixedBank::new(words)?;
    let channels = fixed.channels();
    let mut floating = FloatBank::new(words)?;
    let mut posts = (0..words.len())
        .map(|_| HostPost::new(1_000_000))
        .collect::<Result<Vec<_>>>()?;
    let mut float_posts = (0..channels)
        .map(|_| HostPost::new(1_000_000))
        .collect::<Result<Vec<_>>>()?;
    let mut files = (0..words.len())
        .map(|i| ChannelFiles::new(directory, i))
        .collect::<Result<Vec<_>>>()?;
    let mut clipped = vec![0u64; words.len()];
    let mut squared_error = vec![0.0; words.len()];
    let mut compared = 0u64;
    let mut audio_error = vec![0.0; channels];
    let mut audio_compared = vec![0u64; channels];
    let mut hash = Sha256::new();
    let start = Instant::now();
    let mut processed = 0u64;
    let mut raw = vec![0u8; 4096 * 4];
    while processed < samples {
        ensure!(
            !stop.load(Ordering::Relaxed) && start.elapsed() < timeout,
            "reference receiver interrupted or exceeded deadline; partial output retained"
        );
        let count = (samples - processed).min(4096) as usize;
        reader
            .read_exact(&mut raw[..count * 4])
            .context("read complete IQ chunk")?;
        hash.update(&raw[..count * 4]);
        for bytes in raw[..count * 4].as_chunks::<4>().0 {
            let sample = parse_sample(bytes)?;
            let integer = fixed.push(sample);
            let reference = floating.push(sample);
            ensure!(
                integer.is_some() == reference.is_some(),
                "reference decimation disagreement"
            );
            if let (Some(integer), Some(reference)) = (integer, reference) {
                compared += 1;
                for channel in 0..channels {
                    clipped[channel] += u64::from(integer.clipped[channel]);
                    squared_error[channel] += (f64::from(integer.samples[channel].i)
                        - reference[channel].i)
                        .powi(2)
                        + (f64::from(integer.samples[channel].q) - reference[channel].q).powi(2);
                    let post = posts[channel].push(integer.samples[channel]);
                    let float_post = float_posts[channel].push_float(reference[channel])?;
                    ensure!(
                        post.is_some() == float_post.is_some(),
                        "postprocessor decimation disagreement"
                    );
                    if let (Some(post), Some(float_post)) = (post, float_post) {
                        if let (Some(a), Some(b)) = (post.audio, float_post.audio) {
                            audio_error[channel] += (f64::from(a) - f64::from(b)).powi(2);
                            audio_compared[channel] += 1;
                        }
                        files[channel].write(post)?;
                    }
                }
            }
        }
        processed += count as u64;
    }
    ensure!(
        !stop.load(Ordering::Relaxed),
        "reference receiver interrupted; partial output retained"
    );
    let channels = files
        .iter()
        .enumerate()
        .map(|(index, file)| ChannelReport {
            index,
            tuning_word: words[index],
            offset_hz: dsp::tuning_frequency_hz(words[index], crate::SAMPLE_RATE),
            channel_iq_samples: file.iq_samples,
            audio_samples: file.audio_samples,
            cic_clipped_samples: clipped[index],
            post_iq_clipped_samples: file.clipped,
            audio_clipped_samples: posts[index].audio_clipped_samples(),
            low_power_samples: posts[index].low_power_samples(),
            integer_vs_float_rms_error: (compared > 0)
                .then(|| (squared_error[index] / (compared * 2) as f64).sqrt()),
            audio_reference_rms_error: (audio_compared[index] > 0)
                .then(|| (audio_error[index] / audio_compared[index] as f64).sqrt()),
        })
        .collect();
    for file in files {
        file.finish()?;
    }
    Ok(Report {
        input_samples: processed,
        available_input_samples: available,
        sample_limit: max_samples,
        input_truncated_to_limit: available > max_samples,
        processed_prefix_sha256: format!("{:x}", hash.finalize()),
        source_sample_rate_hz: crate::SAMPLE_RATE,
        cic_output_rate_hz: 1_000_000,
        channel_output_rate_hz: dsp::CHANNEL_OUTPUT_RATE_HZ,
        audio_output_rate_hz: dsp::AUDIO_OUTPUT_RATE_HZ,
        elapsed_seconds: start.elapsed().as_secs_f64(),
        channels,
        mode: "host_bit_exact_model_and_floating_reference_not_fpga_execution",
        cic_amplitude_at_100khz: dsp::cic_amplitude(100_000.0, crate::SAMPLE_RATE)?,
        cic_amplitude_at_900khz: dsp::cic_amplitude(900_000.0, crate::SAMPLE_RATE)?,
        limitations: "CIC/LUT quantization, passband droop and aliasing remain; mono FM only, no stereo/RDS or calibrated power. Files use sample-relative time; host throughput and RF continuity are not certified.",
    })
}

pub(crate) fn fixture(directory: &Path, samples: u64, stop: &AtomicBool) -> Result<()> {
    let stage = directory.join("synthetic.sc16q11.partial");
    let mut output = BufWriter::new(crate::new_file(&stage)?);
    let offsets = [-6_000_000.0, -2_000_000.0, 2_000_000.0, 6_000_000.0];
    let tones = [500.0, 1000.0, 1500.0, 2000.0];
    let mut hash = Sha256::new();
    for n in 0..samples {
        if n % 4096 == 0 {
            ensure!(
                !stop.load(Ordering::Relaxed),
                "fixture generation interrupted"
            );
        }
        let t = n as f64 / f64::from(crate::SAMPLE_RATE);
        let mut i = 0.0;
        let mut q = 0.0;
        for channel in 0..4 {
            let phase = std::f64::consts::TAU * offsets[channel] * t
                - 30_000.0 / tones[channel] * (std::f64::consts::TAU * tones[channel] * t).cos();
            i += 350.0 * phase.cos();
            q += 350.0 * phase.sin();
        }
        let mut raw = [0u8; 4];
        raw[..2].copy_from_slice(&(i.round() as i16).to_le_bytes());
        raw[2..].copy_from_slice(&(q.round() as i16).to_le_bytes());
        output.write_all(&raw)?;
        hash.update(raw);
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    fs::hard_link(&stage, directory.join("synthetic.sc16q11"))?;
    fs::remove_file(stage)?;
    crate::json_file(
        &directory.join("fixture.json"),
        &serde_json::json!({"sample_rate_hz":crate::SAMPLE_RATE,"samples":samples,"center_frequency_hz":98_000_000,
        "station_frequencies_mhz":[92.0,96.0,100.0,104.0],"modulation":"mono_FM","audio_tones_hz":tones,"frequency_deviation_hz":30_000,
        "carrier_amplitude_per_iq_component":350,"sha256":format!("{:x}",hash.finalize()),"provenance":"deterministic_analytic_software_fixture_not_RF_capture"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_or_invalid_q11_and_partial_files_do_not_publish_audio() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("bad.iq");
        fs::write(&input, [0u8; 3]).unwrap();
        assert!(self::input(&input).is_err());
        fs::write(&input, [0xff, 0x7f, 0, 0]).unwrap();
        let out = temp.path().join("out");
        fs::create_dir(&out).unwrap();
        assert!(
            receive(
                &input,
                &out,
                &[0],
                1000,
                Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(!out.join("channel-0.wav").exists());
    }
    #[test]
    fn pcm_header_lengths_and_output_preservation_are_exact() {
        let temp = tempfile::tempdir().unwrap();
        let mut files = ChannelFiles::new(temp.path(), 0).unwrap();
        files
            .write(dsp::PostSample {
                iq: dsp::Iq { i: 12.0, q: -3.0 },
                audio: Some(1234),
            })
            .unwrap();
        files.finish().unwrap();
        let bytes = fs::read(temp.path().join("channel-0.wav")).unwrap();
        assert_eq!(bytes.len(), 46);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 2);
        assert_eq!(i16::from_le_bytes(bytes[44..46].try_into().unwrap()), 1234);
        assert!(ChannelFiles::new(temp.path(), 0).is_err());
    }
    #[test]
    fn native_readbacks_accept_small_lo_quantization_and_reject_wrong_modes() {
        let temp = tempfile::tempdir().unwrap();
        let iq = temp.path().join("input.sc16q11");
        fs::write(&iq, [0u8; 80]).unwrap();
        let baseline = serde_json::json!({"format":"SC16_Q11","dtype":"<i2","interleave":"I,Q","complex_scale":2048,
            "frequency_hz":97_999_998,"sample_rate_hz":20_000_000,"bandwidth_hz":20_000_000,"gain_db":40,
            "gain_mode":"manual","bias_tee_enabled":false,"channel":"RX2","num_samples":20,"started_utc":"2000-01-01T00:00:00Z"});
        let sidecar = iq.with_extension("sc16q11.json");
        fs::write(&sidecar, serde_json::to_vec(&baseline).unwrap()).unwrap();
        assert_eq!(
            validate_acquisition(&iq, 98_000_000, 20, 2, 40.0)
                .unwrap()
                .frequency_hz(),
            97_999_998
        );
        for (field, value) in [
            ("frequency_hz", serde_json::json!(97_999_900)),
            ("sample_rate_hz", serde_json::json!(10_000_000)),
            ("bias_tee_enabled", serde_json::json!(true)),
            ("num_samples", serde_json::json!(21)),
            ("dtype", serde_json::json!(">i2")),
        ] {
            let mut wrong = baseline.clone();
            wrong[field] = value;
            fs::write(&sidecar, serde_json::to_vec(&wrong).unwrap()).unwrap();
            assert!(
                validate_acquisition(&iq, 98_000_000, 20, 2, 40.0).is_err(),
                "{field}"
            );
        }
    }
}
