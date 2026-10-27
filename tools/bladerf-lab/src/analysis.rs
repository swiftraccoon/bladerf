//! Bounded SC16_Q11 analysis and a two-sided, window-normalized PSD plot.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Args;
use plotters::prelude::{BLUE, BitMapBackend, ChartBuilder, IntoDrawingArea, LineSeries, WHITE};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};

const MAX_METADATA_BYTES: u64 = 65_536;
const MAX_ANALYZED_SAMPLES: usize = 4_194_304;
const MAX_FFT_SIZE: usize = 1_048_576;
const Q11_SCALE: f64 = 2048.0;

/// Analyze a capture and its adjacent `INPUT.json` sidecar without opening a radio.
#[derive(Args, Debug)]
pub(crate) struct AnalyzeArgs {
    /// Little-endian SC16_Q11 IQ file with adjacent INPUT.json metadata.
    input: PathBuf,
    /// New PNG path; existing outputs are refused. Defaults to INPUT.png.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Maximum samples read from the start of the file (16..=4194304).
    #[arg(long, default_value_t = 1_048_576)]
    max_samples: usize,
    /// FFT length before clipping to the analyzed sample count (16..=1048576).
    #[arg(long, default_value_t = 4096)]
    nfft: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct CaptureMetadata {
    format: String,
    dtype: String,
    interleave: String,
    complex_scale: f64,
    frequency_hz: f64,
    sample_rate_hz: f64,
    num_samples: u64,
    channel: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Statistics {
    total_complex_samples: u64,
    analyzed_complex_samples: usize,
    duration_seconds: f64,
    nonzero_component_fraction: f64,
    rail_component_fraction: f64,
    rms_dbfs: f64,
    mean_i: f64,
    mean_q: f64,
    peak_frequency_hz: f64,
}

#[derive(Debug)]
struct Spectrum {
    statistics: Statistics,
    frequencies_hz: Vec<f64>,
    psd_per_hz: Vec<f64>,
    sample_rate_hz: f64,
    channel: String,
}

/// Print statistics and atomically create a PNG; input, format, and I/O failures
/// are returned with context. The input capture and existing outputs are preserved.
pub(crate) fn run(args: AnalyzeArgs) -> Result<()> {
    let output = args
        .output
        .unwrap_or_else(|| append_suffix(&args.input, ".png"));
    ensure_png_path(&output)?;
    ensure!(
        !output.try_exists()?,
        "output already exists: {}",
        output.display()
    );
    let spectrum = analyze_file(&args.input, args.max_samples, args.nfft)?;
    render_png(&spectrum, &args.input, &output)?;
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &spectrum.statistics)?;
    writeln!(stdout, "\nPSD: {}", output.display())?;
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn ensure_png_path(path: &Path) -> Result<()> {
    ensure!(
        path.extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("png")),
        "plot output must have a .png extension"
    );
    Ok(())
}

fn read_metadata(input: &Path) -> Result<CaptureMetadata> {
    let path = append_suffix(input, ".json");
    let mut bytes = Vec::new();
    File::open(&path)
        .with_context(|| format!("open metadata {}", path.display()))?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read metadata {}", path.display()))?;
    ensure!(
        bytes.len() as u64 <= MAX_METADATA_BYTES,
        "metadata exceeds 64 KiB"
    );
    let metadata: CaptureMetadata = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse metadata {}", path.display()))?;
    ensure!(
        metadata.format == "SC16_Q11" && metadata.dtype == "<i2" && metadata.interleave == "I,Q",
        "requires SC16_Q11 little-endian int16 I,Q metadata"
    );
    ensure!(
        metadata.complex_scale == Q11_SCALE,
        "SC16_Q11 complex_scale must be 2048"
    );
    ensure!(
        metadata.sample_rate_hz.is_finite() && metadata.sample_rate_hz > 0.0,
        "sample rate must be finite and positive"
    );
    ensure!(
        metadata.frequency_hz.is_finite(),
        "center frequency must be finite"
    );
    ensure!(
        (metadata.frequency_hz - metadata.sample_rate_hz / 2.0).is_finite()
            && (metadata.frequency_hz + metadata.sample_rate_hz / 2.0).is_finite(),
        "frequency range cannot be represented"
    );
    ensure!(
        metadata.num_samples > 0
            && (metadata.num_samples as f64 / metadata.sample_rate_hz).is_finite(),
        "invalid sample count or capture duration"
    );
    if let Some(channel) = &metadata.channel {
        ensure!(
            channel.len() <= 32
                && channel
                    .chars()
                    .all(|character| character.is_ascii_graphic()),
            "channel label must contain at most 32 printable ASCII characters"
        );
    }
    Ok(metadata)
}

fn analyze_file(input: &Path, max_samples: usize, nfft: usize) -> Result<Spectrum> {
    ensure!(
        (16..=MAX_ANALYZED_SAMPLES).contains(&max_samples),
        "max-samples must be between 16 and {MAX_ANALYZED_SAMPLES}"
    );
    ensure!(
        (16..=MAX_FFT_SIZE).contains(&nfft),
        "nfft must be between 16 and {MAX_FFT_SIZE}"
    );
    let metadata = read_metadata(input)?;
    let mut file = File::open(input).with_context(|| format!("open IQ {}", input.display()))?;
    let size = file.metadata()?.len();
    ensure!(
        size >= 16 * 4 && size % 4 == 0,
        "IQ requires at least 16 complete I,Q pairs"
    );
    ensure!(
        size / 4 == metadata.num_samples,
        "IQ length does not match metadata sample count"
    );
    let count = usize::try_from(metadata.num_samples.min(max_samples as u64))?;
    let mut bytes = vec![0; count.checked_mul(4).context("IQ byte count overflow")?];
    file.read_exact(&mut bytes)
        .context("read complete IQ sample data")?;
    let mut iq = Vec::with_capacity(count);
    let mut nonzero_components = 0_u64;
    let mut rail_components = 0_u64;
    let mut sum = Complex::new(0.0, 0.0);
    let mut energy = 0.0;
    for pair in bytes.as_chunks::<4>().0 {
        let i = i16::from_le_bytes([pair[0], pair[1]]);
        let q = i16::from_le_bytes([pair[2], pair[3]]);
        ensure!(
            (-2048..=2047).contains(&i) && (-2048..=2047).contains(&q),
            "IQ contains a component outside the SC16_Q11 range"
        );
        for component in [i, q] {
            nonzero_components += u64::from(component != 0);
            rail_components += u64::from(component == -2048 || component == 2047);
        }
        let value = Complex::new(f64::from(i) / Q11_SCALE, f64::from(q) / Q11_SCALE);
        energy += value.norm_sqr();
        sum += value;
        iq.push(value);
    }
    drop(bytes);
    let fft_size = nfft.min(count);
    let window: Vec<_> = (0..fft_size)
        .map(|index| {
            0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (fft_size - 1) as f64).cos()
        })
        .collect();
    let window_energy: f64 = window.iter().map(|value| value * value).sum();
    let blocks = count / fft_size;
    let normalization = metadata.sample_rate_hz * window_energy * blocks as f64;
    ensure!(
        normalization.is_finite() && normalization > 0.0,
        "PSD normalization cannot be represented"
    );
    let fft = FftPlanner::<f64>::new().plan_fft_forward(fft_size);
    let mut frame = vec![Complex::new(0.0, 0.0); fft_size];
    let mut scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut unshifted = vec![0.0; fft_size];
    for block in iq.chunks_exact(fft_size) {
        for ((target, value), weight) in frame.iter_mut().zip(block).zip(&window) {
            *target = *value * *weight;
        }
        fft.process_with_scratch(&mut frame, &mut scratch);
        for (total, value) in unshifted.iter_mut().zip(&frame) {
            *total += value.norm_sqr();
        }
    }
    let mut frequencies_hz = Vec::with_capacity(fft_size);
    let mut psd_per_hz = Vec::with_capacity(fft_size);
    for shifted_index in 0..fft_size {
        // NumPy-compatible fftshift: odd lengths rotate by ceil(N / 2).
        let raw_index = (shifted_index + fft_size.div_ceil(2)) % fft_size;
        let psd = unshifted[raw_index] / normalization;
        ensure!(
            psd.is_finite(),
            "PSD cannot be represented for this sample rate"
        );
        frequencies_hz.push(
            metadata.frequency_hz
                + ((shifted_index as f64 - (fft_size / 2) as f64) / fft_size as f64)
                    * metadata.sample_rate_hz,
        );
        psd_per_hz.push(psd);
    }
    ensure!(
        frequencies_hz[0] / 1e6 < frequencies_hz[fft_size - 1] / 1e6,
        "frequency bins cannot be resolved for this center frequency and sample rate"
    );
    let peak = psd_per_hz
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(index, _)| index)
        .context("empty PSD")?;
    let rms = (energy / count as f64).sqrt();
    let statistics = Statistics {
        total_complex_samples: metadata.num_samples,
        analyzed_complex_samples: count,
        duration_seconds: metadata.num_samples as f64 / metadata.sample_rate_hz,
        nonzero_component_fraction: nonzero_components as f64 / (count * 2) as f64,
        rail_component_fraction: rail_components as f64 / (count * 2) as f64,
        rms_dbfs: 20.0 * rms.max(1e-15).log10(),
        mean_i: sum.re / count as f64,
        mean_q: sum.im / count as f64,
        peak_frequency_hz: frequencies_hz[peak],
    };
    Ok(Spectrum {
        statistics,
        frequencies_hz,
        psd_per_hz,
        sample_rate_hz: metadata.sample_rate_hz,
        channel: metadata.channel.unwrap_or_else(|| "RX".to_owned()),
    })
}

fn render_png(spectrum: &Spectrum, input: &Path, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".bladerf-psd-")
        .suffix(".png")
        .tempfile_in(parent)?;
    let db_values: Vec<_> = spectrum
        .psd_per_hz
        .iter()
        .map(|power| 10.0 * power.max(1e-30).log10())
        .collect();
    let minimum_db = db_values.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum_db = db_values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    {
        let drawing = BitMapBackend::new(temporary.path(), (1760, 800)).into_drawing_area();
        drawing.fill(&WHITE).context("initialize PNG plot")?;
        let title = format!(
            "{} — {} — {} MS/s",
            spectrum.channel,
            input
                .file_name()
                .unwrap_or(input.as_os_str())
                .to_string_lossy(),
            spectrum.sample_rate_hz / 1e6
        );
        let mut chart = ChartBuilder::on(&drawing)
            .caption(title, ("sans-serif", 30))
            .margin(20)
            .x_label_area_size(65)
            .y_label_area_size(95)
            .build_cartesian_2d(
                spectrum.frequencies_hz[0] / 1e6
                    ..spectrum.frequencies_hz[spectrum.frequencies_hz.len() - 1] / 1e6,
                (minimum_db.floor() - 5.0)..(maximum_db.ceil() + 5.0),
            )
            .context("build PSD axes")?;
        chart
            .configure_mesh()
            .max_light_lines(0)
            .x_desc("Frequency (MHz)")
            .y_desc("PSD (dBFS/Hz; complex unit-power reference)")
            .axis_desc_style(("sans-serif", 22))
            .label_style(("sans-serif", 18))
            .draw()
            .context("draw PSD axes")?;
        chart
            .draw_series(LineSeries::new(
                spectrum
                    .frequencies_hz
                    .iter()
                    .zip(&db_values)
                    .map(|(&frequency, &power_db)| (frequency / 1e6, power_db)),
                &BLUE,
            ))
            .context("draw PSD trace")?;
        drawing.present().context("encode PNG plot")?;
    }
    temporary
        .persist_noclobber(output)
        .map_err(|error| error.error)
        .with_context(|| format!("create new PNG {}", output.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(
        directory: &Path,
        count: usize,
        fft_size: usize,
        tone_bin: isize,
    ) -> Result<PathBuf> {
        let path = directory.join("tone.sc16q11");
        let mut file = File::create(&path)?;
        for index in 0..count {
            let phase = std::f64::consts::TAU * tone_bin as f64 * index as f64 / fft_size as f64;
            for component in [512.0 * phase.cos(), 512.0 * phase.sin()] {
                file.write_all(&(component.round() as i16).to_le_bytes())?;
            }
        }
        fs::write(
            append_suffix(&path, ".json"),
            serde_json::to_vec(&serde_json::json!({
                "format": "SC16_Q11", "dtype": "<i2", "interleave": "I,Q", "complex_scale": 2048,
                "frequency_hz": 100_000_000.0, "sample_rate_hz": 65_536.0, "num_samples": count, "channel": "RX1"
            }))?,
        )?;
        Ok(path)
    }

    #[test]
    fn known_tone_has_expected_rms_peak_and_integrated_psd() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = fixture(directory.path(), 8192, 1024, 128)?;
        let spectrum = analyze_file(&path, 8192, 1024)?;
        assert_eq!(spectrum.statistics.total_complex_samples, 8192);
        assert!((spectrum.statistics.rms_dbfs - 20.0 * 0.25_f64.log10()).abs() < 0.01);
        assert!((spectrum.statistics.peak_frequency_hz - 100_008_192.0).abs() < 1e-6);
        let integrated = spectrum.psd_per_hz.iter().sum::<f64>() * 65_536.0 / 1024.0;
        assert!((integrated - 0.25_f64.powi(2)).abs() < 0.0001);
        assert_eq!(spectrum.statistics.rail_component_fraction, 0.0);
        Ok(())
    }

    #[test]
    fn odd_fft_length_places_negative_frequency_correctly() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = fixture(directory.path(), 17 * 8, 17, -3)?;
        let spectrum = analyze_file(&path, 17 * 8, 17)?;
        assert!(
            (spectrum.statistics.peak_frequency_hz - (100_000_000.0 - 3.0 * 65_536.0 / 17.0)).abs()
                < 1e-6
        );
        Ok(())
    }

    #[test]
    fn rejects_bad_file_length_and_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = fixture(directory.path(), 64, 32, 4)?;
        let sidecar = append_suffix(&path, ".json");
        let original = fs::read(&sidecar)?;
        for (key, bad_value) in [
            ("complex_scale", serde_json::json!(0)),
            ("sample_rate_hz", serde_json::json!(0)),
            ("sample_rate_hz", serde_json::json!(-1)),
            ("sample_rate_hz", serde_json::json!("NaN")),
            ("interleave", serde_json::json!("Q,I")),
            ("num_samples", serde_json::json!(65)),
        ] {
            let mut metadata: serde_json::Value = serde_json::from_slice(&original)?;
            metadata[key] = bad_value;
            fs::write(&sidecar, serde_json::to_vec(&metadata)?)?;
            assert!(
                analyze_file(&path, 64, 32).is_err(),
                "accepted invalid {key}"
            );
        }
        fs::write(&sidecar, &original)?;
        File::options().write(true).open(&path)?.set_len(255)?;
        assert!(analyze_file(&path, 64, 32).is_err());
        fs::write(&sidecar, [b' '; MAX_METADATA_BYTES as usize + 1])?;
        assert!(read_metadata(&path).is_err());
        Ok(())
    }

    #[test]
    fn rejects_unbounded_allocations_and_out_of_range_components() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = fixture(directory.path(), 64, 32, 4)?;
        assert!(analyze_file(&path, MAX_ANALYZED_SAMPLES + 1, 32).is_err());
        assert!(analyze_file(&path, 64, MAX_FFT_SIZE + 1).is_err());
        assert!(analyze_file(&path, 15, 32).is_err());
        let mut file = File::options().write(true).open(&path)?;
        file.write_all(&2048_i16.to_le_bytes())?;
        assert!(analyze_file(&path, 64, 32).is_err());
        Ok(())
    }

    #[test]
    fn png_is_encoded_and_existing_output_is_preserved() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let input = fixture(directory.path(), 256, 64, 8)?;
        let output = directory.path().join("spectrum.png");
        let spectrum = analyze_file(&input, 256, 64)?;
        render_png(&spectrum, &input, &output)?;
        let bytes = fs::read(&output)?;
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(bytes.len() > 1024);
        assert!(render_png(&spectrum, &input, &output).is_err());
        assert_eq!(fs::read(&output)?, bytes);
        assert!(ensure_png_path(&input).is_err());
        Ok(())
    }
}
