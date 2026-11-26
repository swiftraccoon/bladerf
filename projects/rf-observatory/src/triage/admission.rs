//! Admit saved stock-image sweeps and pin their direct input files for reading.

mod series;

#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::ops::Range;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
#[cfg(target_os = "linux")]
use nix::fcntl::OFlag;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::file_pin::same_file;
use crate::receiver::{
    self, ACQUISITION_CENTER_TOLERANCE_HZ, ACQUISITION_SIDECAR_LIMIT, SampleTiming,
};
use crate::sweep::{CompletedOperation, plan, tune_directory_name};
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE, file_pin};

pub(super) use self::series::Series;

const SWEEP_RECEIPT: &str = "sweep.json";
const MAX_SWEEP_RECEIPT_BYTES: u64 = 64 * 1024 * 1024;
const IQ_NAME: &str = "wideband.sc16q11";
const BYTES_PER_SAMPLE: u64 = 4;

/// An admitted single sweep or a complete series of sweeps.
pub(super) enum CompletedCapture {
    Single(Sweep),
    Repeated(Series),
}

pub(super) struct PassRef<'a> {
    pub(super) directory: Option<&'a str>,
    pub(super) sweep: &'a Sweep,
}

impl CompletedCapture {
    /// Admit either a single completed sweep or a complete sweep series.
    ///
    /// # Errors
    /// Returns an error when the requested source cannot be validated.
    pub(super) fn open(directory: &Path, series: bool) -> Result<Self> {
        if series {
            Ok(Self::Repeated(Series::open(directory)?))
        } else {
            Ok(Self::Single(Sweep::open(directory)?))
        }
    }

    /// Receipt kind used in saved replay reports.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Single(_) => "sweep",
            Self::Repeated(_) => "series",
        }
    }

    pub(super) fn source_receipt_sha256(&self) -> &str {
        match self {
            Self::Single(sweep) => &sweep.source_receipt_sha256,
            Self::Repeated(series) => &series.source_series_sha256,
        }
    }

    /// Whether every admitted dwell has matching sample continuity evidence.
    pub(super) fn continuity_checked(&self) -> bool {
        match self {
            Self::Single(sweep) => sweep.check_continuity,
            Self::Repeated(series) => series.check_continuity,
        }
    }

    /// Native timing digest recorded by the terminal receipt, when available.
    pub(super) fn timing_sha256(&self) -> Option<&str> {
        match self {
            Self::Single(sweep) => sweep.timing_sha256.as_deref(),
            Self::Repeated(series) => series.timing_sha256.as_deref(),
        }
    }

    /// Bands declared by a targeted sweep and shared by every pass.
    pub(super) fn requested_bands(&self) -> Option<&[plan::Band]> {
        match self {
            Self::Single(sweep) => sweep.requested_bands.as_deref(),
            Self::Repeated(series) => series.passes.first()?.sweep.requested_bands.as_deref(),
        }
    }

    pub(super) fn passes(&self) -> Vec<PassRef<'_>> {
        match self {
            Self::Single(sweep) => vec![PassRef {
                directory: None,
                sweep,
            }],
            Self::Repeated(series) => series
                .passes
                .iter()
                .map(|pass| PassRef {
                    directory: Some(&pass.directory),
                    sweep: &pass.sweep,
                })
                .collect(),
        }
    }

    /// Recheck the admitted source before publishing an offline report.
    ///
    /// # Errors
    /// Returns an error if the source receipt or admitted inputs changed.
    pub(super) fn verify(&self) -> Result<()> {
        match self {
            Self::Single(sweep) => sweep.verify(),
            Self::Repeated(series) => series.verify(),
        }
    }

    pub(super) fn verify_pinned(&self, root: &File) -> Result<()> {
        match self {
            Self::Single(sweep) => sweep.verify_pinned(root),
            Self::Repeated(series) => series.verify_pinned(root),
        }
    }
}

#[derive(Debug)]
pub(super) struct SeriesSource {
    source_series_sha256: Option<String>,
    source_series_triage_sha256: Option<String>,
    source_series_triage_overlap_sha256: Option<String>,
    source_series_activity_sha256: Option<String>,
}

impl SeriesSource {
    pub(super) fn matches(
        &self,
        series_sha256: &str,
        triage_sha256: &str,
        overlap_sha256: Option<&str>,
    ) -> bool {
        self.source_series_sha256.as_deref() == Some(series_sha256)
            && self.source_series_triage_sha256.as_deref() == Some(triage_sha256)
            && self.source_series_triage_overlap_sha256.as_deref() == overlap_sha256
            && self.source_series_activity_sha256.is_none()
    }
}

#[derive(Debug, Deserialize)]
struct Receipt {
    schema_version: u32,
    operation: CompletedOperation,
    lower_hz: u64,
    upper_hz: u64,
    requested_bands: Option<Vec<plan::Band>>,
    requested_band_dwells: Option<Vec<plan::BandDwell>>,
    antenna: u8,
    gain_db: f64,
    dwell_ms: u64,
    #[serde(default)]
    check_continuity: bool,
    iq_bytes: u64,
    capture_cadence: Option<CaptureCadence>,
    source_series_sha256: Option<String>,
    source_series_triage_sha256: Option<String>,
    source_series_triage_overlap_sha256: Option<String>,
    source_series_activity_sha256: Option<String>,
    visit_centers_hz: Option<Vec<u64>>,
    tunes: Vec<TuneRecord>,
}

#[derive(Debug, Deserialize)]
struct CaptureCadence {
    native: Option<NativeTiming>,
}

impl CaptureCadence {
    fn into_timing_sha256(self) -> Option<String> {
        self.native.map(|native| native.sha256)
    }
}

#[derive(Debug, Deserialize)]
struct NativeTiming {
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct TuneRecord {
    directory: String,
    requested_center_hz: u64,
    observed_center_hz: u64,
    observed_bands: [plan::Band; 2],
    dwell_ms: Option<u64>,
    sample_timing: Option<SampleTiming>,
}

impl TuneRecord {
    fn samples(&self, default_dwell_ms: u64) -> Result<u64> {
        let dwell_ms = self.dwell_ms.unwrap_or(default_dwell_ms);
        ensure!(
            (1..=plan::MAX_DWELL_MS).contains(&dwell_ms),
            "invalid sweep dwell duration at {}",
            self.directory
        );
        Ok(dwell_ms * (u64::from(SAMPLE_RATE) / 1_000))
    }
}

impl Receipt {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported sweep receipt version"
        );
        ensure!(
            (1..=plan::MAX_TUNES).contains(&self.tunes.len())
                && (1..=plan::MAX_DWELL_MS).contains(&self.dwell_ms),
            "invalid sweep tune count or dwell duration"
        );
        ensure!(
            (1..=2).contains(&self.antenna) && (0.0..=71.0).contains(&self.gain_db),
            "invalid sweep antenna or gain"
        );
        ensure!(
            !self.check_continuity || self.antenna == 1,
            "sweep continuity checks require RX1"
        );
        if let Some(visits) = &self.visit_centers_hz {
            ensure!(
                self.tunes
                    .iter()
                    .map(|tune| tune.requested_center_hz)
                    .eq(visits.iter().copied()),
                "sweep visit order differs from the requested tune centers"
            );
        } else {
            ensure!(
                self.tunes.is_sorted_by(
                    |left, right| left.requested_center_hz < right.requested_center_hz
                ),
                "sweep requested centers must be strictly increasing"
            );
        }
        let mut iq_bytes = 0;
        for (index, tune) in self.tunes.iter().enumerate() {
            ensure!(
                tune.directory == tune_directory_name(index),
                "unexpected sweep tune directory at index {index}"
            );
            ensure!(
                (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&tune.requested_center_hz)
                    && tune.observed_center_hz.abs_diff(tune.requested_center_hz)
                        <= ACQUISITION_CENTER_TOLERANCE_HZ,
                "invalid requested or observed center at {}",
                tune.directory
            );
            ensure!(
                tune.observed_bands == plan::observed_bands(tune.observed_center_hz, 0),
                "invalid observed band geometry at {}",
                tune.directory
            );
            ensure!(
                tune.sample_timing.is_some() == self.check_continuity,
                "sweep sample timing differs from the continuity request at {}",
                tune.directory
            );
            iq_bytes += tune.samples(self.dwell_ms)? * BYTES_PER_SAMPLE;
        }
        ensure!(
            self.iq_bytes == iq_bytes,
            "sweep IQ bytes differ from the sum of its tune extents"
        );
        match self.operation {
            CompletedOperation::Sequential => {
                ensure!(
                    self.lower_hz >= MIN_CENTER_HZ
                        && self.lower_hz < self.upper_hz
                        && self.upper_hz <= MAX_CENTER_HZ,
                    "invalid requested sweep bounds"
                );
                ensure!(
                    plan::covers(
                        self.lower_hz,
                        self.upper_hz,
                        self.tunes.iter().flat_map(|tune| tune.observed_bands)
                    ),
                    "completed sweep receipt leaves a coverage gap"
                );
            }
            CompletedOperation::Targeted => {
                let centers = self.tunes.iter().map(|tune| tune.requested_center_hz);
                ensure!(
                    centers.clone().min() == Some(self.lower_hz)
                        && centers.max().map(|center| center + 1) == Some(self.upper_hz),
                    "targeted sweep bounds differ from the requested centers"
                );
            }
        }
        ensure!(
            self.requested_band_dwells.is_none() || self.requested_bands.is_some(),
            "requested band dwell overrides require requested bands"
        );
        if let Some(bands) = &self.requested_bands {
            ensure!(
                self.operation == CompletedOperation::Targeted,
                "requested bands require a targeted sweep receipt"
            );
            plan::validate_requested_bands(bands)?;
            let overrides = self.requested_band_dwells.as_deref();
            ensure!(
                overrides.is_none_or(|dwells| !dwells.is_empty()),
                "requested band dwell overrides must be nonempty when present"
            );
            plan::verify_band_dwells(
                bands,
                overrides.unwrap_or_default(),
                self.dwell_ms,
                self.tunes
                    .iter()
                    .map(|tune| (tune.observed_bands, tune.dwell_ms.unwrap_or(self.dwell_ms))),
            )?;
        }
        Ok(())
    }
}

/// A terminal receipt and the identities of its admitted direct inputs.
#[derive(Debug)]
pub(super) struct Sweep {
    pub(super) source_receipt_sha256: String,
    pub(super) operation: CompletedOperation,
    pub(super) requested_range: Option<Range<u64>>,
    requested_bands: Option<Vec<plan::Band>>,
    requested_band_dwells: Option<Vec<plan::BandDwell>>,
    dwell_ms: u64,
    check_continuity: bool,
    pub(super) visit_centers_hz: Option<Vec<u64>>,
    pub(super) tunes: Vec<Tune>,
    pub(super) series_source: SeriesSource,
    timing_sha256: Option<String>,
    antenna: u8,
    gain_db: f64,
    root: PathBuf,
    root_stamp: Metadata,
    receipt_stamp: Metadata,
}

/// Binding to one pass in an admitted series manifest.
struct ExpectedPass<'a> {
    sha256: &'a str,
    tunes: usize,
    iq_bytes: u64,
    check_continuity: bool,
    visit_centers_hz: Option<&'a [u64]>,
}

impl Sweep {
    /// Read a bounded terminal receipt and validate every tune's extent and metadata.
    /// Open descriptors are scoped to one tune at a time.
    ///
    /// # Errors
    /// Returns an error for invalid geometry, missing or indirect files, unreadable
    /// inputs, and changes detected during admission.
    pub(super) fn open(directory: &Path) -> Result<Self> {
        let root = directory
            .canonicalize()
            .with_context(|| format!("resolve saved sweep directory {}", directory.display()))?;
        let pinned_root = open_directory(&root)?;
        Self::open_pinned(root, &pinned_root, None)
    }

    fn open_pinned(
        root: PathBuf,
        pinned_root: &File,
        expected: Option<ExpectedPass<'_>>,
    ) -> Result<Self> {
        let root_stamp = pinned_root.metadata()?;
        let root_fd = descriptor_path(pinned_root);
        let (receipt_bytes, receipt_stamp) =
            read_bounded(&root_fd.join(SWEEP_RECEIPT), MAX_SWEEP_RECEIPT_BYTES)
                .context("read completed sweep receipt")?;
        let source_receipt_sha256 = format!("{:x}", Sha256::digest(&receipt_bytes));
        ensure!(
            expected
                .as_ref()
                .is_none_or(|pass| pass.sha256 == source_receipt_sha256),
            "sweep receipt SHA-256 differs from the series manifest"
        );
        let receipt: Receipt =
            serde_json::from_slice(&receipt_bytes).context("parse completed sweep receipt")?;
        drop(receipt_bytes);
        receipt.validate()?;
        ensure!(
            expected
                .as_ref()
                .is_none_or(|pass| pass.check_continuity == receipt.check_continuity),
            "sweep continuity policy differs from the series manifest"
        );
        ensure!(
            expected
                .as_ref()
                .is_none_or(|pass| pass.visit_centers_hz == receipt.visit_centers_hz.as_deref()),
            "sweep visit order differs from the series manifest"
        );
        ensure!(
            expected.is_none_or(|pass| {
                receipt.tunes.len() == pass.tunes && receipt.iq_bytes == pass.iq_bytes
            }),
            "sweep tune count or IQ extent differs from the series manifest"
        );
        let tunes = receipt
            .tunes
            .into_iter()
            .map(|record| {
                let samples = record.samples(receipt.dwell_ms)?;
                Tune::open(
                    &root,
                    &root_fd,
                    record,
                    samples,
                    receipt.antenna,
                    receipt.gain_db,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let sweep = Self {
            source_receipt_sha256,
            operation: receipt.operation,
            requested_range: (receipt.operation == CompletedOperation::Sequential)
                .then_some(receipt.lower_hz..receipt.upper_hz),
            requested_bands: receipt.requested_bands,
            requested_band_dwells: receipt.requested_band_dwells,
            dwell_ms: receipt.dwell_ms,
            check_continuity: receipt.check_continuity,
            visit_centers_hz: receipt.visit_centers_hz,
            tunes,
            series_source: SeriesSource {
                source_series_sha256: receipt.source_series_sha256,
                source_series_triage_sha256: receipt.source_series_triage_sha256,
                source_series_triage_overlap_sha256: receipt.source_series_triage_overlap_sha256,
                source_series_activity_sha256: receipt.source_series_activity_sha256,
            },
            timing_sha256: receipt
                .capture_cadence
                .and_then(CaptureCadence::into_timing_sha256),
            antenna: receipt.antenna,
            gain_db: receipt.gain_db,
            root,
            root_stamp,
            receipt_stamp,
        };
        sweep.verify()?;
        Ok(sweep)
    }

    /// Apply the requested range to an ordinary sweep. For a scheduled sweep,
    /// that range guarantees background coverage and priority visits may extend past it.
    pub(super) fn analysis_range(&self) -> Option<&Range<u64>> {
        self.requested_range
            .as_ref()
            .filter(|_| self.visit_centers_hz.is_none())
    }

    /// Recheck the receipt and all source file identities before publishing results.
    ///
    /// # Errors
    /// Returns an error if any admitted path can no longer be verified.
    pub(super) fn verify(&self) -> Result<()> {
        let root = open_directory(&self.root)?;
        self.verify_pinned(&root)
    }

    pub(super) fn verify_pinned(&self, root: &File) -> Result<()> {
        ensure!(
            same_inode(&self.root_stamp, &root.metadata()?),
            "sweep directory changed after admission: {}",
            self.root.display()
        );
        let root_fd = descriptor_path(root);
        verify_path(&root_fd.join(SWEEP_RECEIPT), &self.receipt_stamp)
            .context("verify completed sweep receipt")?;
        for tune in &self.tunes {
            let directory = reopen_directory(&root_fd.join(&tune.directory), &tune.directory_stamp)
                .with_context(|| format!("verify saved sweep tune {}", tune.path.display()))?;
            tune.verify_paths(&descriptor_path(&directory))?;
        }
        Ok(())
    }
}

/// One native SC16 Q11 dwell with its requested and observed tuning centers.
#[derive(Debug)]
pub(super) struct Tune {
    pub(super) directory: String,
    pub(super) requested_center_hz: u64,
    pub(super) observed_center_hz: u64,
    pub(super) samples: u64,
    pub(super) sidecar_sha256: String,
    path: PathBuf,
    directory_stamp: Metadata,
    iq_stamp: Metadata,
    sidecar_stamp: Metadata,
}

impl Tune {
    fn open(
        root: &Path,
        root_fd: &Path,
        record: TuneRecord,
        samples: u64,
        antenna: u8,
        gain: f64,
    ) -> Result<Self> {
        let path = root.join(&record.directory);
        let directory = open_directory(&root_fd.join(&record.directory))
            .with_context(|| format!("admit direct sweep tune {}", path.display()))?;
        let directory_stamp = directory.metadata()?;
        let iq_path = descriptor_path(&directory).join(IQ_NAME);
        let expected_bytes = samples * BYTES_PER_SAMPLE;
        let (iq, bytes) = file_pin::open_regular(&iq_path, expected_bytes)
            .with_context(|| format!("admit IQ from {}", path.display()))?;
        let iq_stamp = iq.metadata()?;
        ensure!(
            bytes == expected_bytes && iq_stamp.len() == expected_bytes,
            "acquisition byte length differs from {} dwell",
            path.display()
        );
        let (sidecar, sidecar_stamp) = read_bounded(
            &receiver::acquisition_sidecar(&iq_path),
            ACQUISITION_SIDECAR_LIMIT,
        )
        .with_context(|| format!("admit acquisition sidecar from {}", path.display()))?;
        let metadata = receiver::validate_acquisition_metadata(
            &sidecar,
            record.requested_center_hz,
            samples,
            antenna,
            gain,
        )
        .with_context(|| format!("validate acquisition sidecar from {}", path.display()))?;
        ensure!(
            metadata.frequency_hz() == record.observed_center_hz,
            "sidecar observed center differs from sweep receipt for {}",
            path.display()
        );
        ensure!(
            metadata.sample_timing() == record.sample_timing,
            "sidecar sample timing differs from sweep receipt for {}",
            path.display()
        );
        Ok(Self {
            directory: record.directory,
            requested_center_hz: record.requested_center_hz,
            observed_center_hz: record.observed_center_hz,
            samples,
            sidecar_sha256: format!("{:x}", Sha256::digest(sidecar)),
            path,
            directory_stamp,
            iq_stamp,
            sidecar_stamp,
        })
    }

    /// Open the admitted IQ inode at offset zero and confirm its source metadata.
    ///
    /// # Errors
    /// Returns an error if either input has changed or the IQ file cannot be opened.
    pub(super) fn open_iq(&self) -> Result<File> {
        let directory = self.reopen()?;
        let iq_path = descriptor_path(&directory).join(IQ_NAME);
        let (iq, _) = file_pin::open_regular(&iq_path, self.iq_stamp.len())
            .with_context(|| format!("open admitted IQ from {}", self.path.display()))?;
        self.verify_descriptor(&iq)?;
        self.verify_paths(&descriptor_path(&directory))?;
        Ok(iq)
    }

    /// Verify the used descriptor and its source paths after reading IQ bytes.
    ///
    /// # Errors
    /// Returns an error if the descriptor or either admitted source path has changed.
    pub(super) fn verify_iq(&self, iq: &File) -> Result<()> {
        self.verify_descriptor(iq)?;
        let directory = self.reopen()?;
        self.verify_paths(&descriptor_path(&directory))
    }

    fn reopen(&self) -> Result<File> {
        reopen_directory(&self.path, &self.directory_stamp)
    }

    fn verify_descriptor(&self, iq: &File) -> Result<()> {
        ensure!(
            same_file(&self.iq_stamp, &iq.metadata()?),
            "IQ changed after admission for {}",
            self.path.display()
        );
        Ok(())
    }

    fn verify_paths(&self, directory: &Path) -> Result<()> {
        let iq_path = directory.join(IQ_NAME);
        verify_path(&iq_path, &self.iq_stamp)
            .with_context(|| format!("verify admitted IQ for {}", self.path.display()))?;
        verify_path(
            &receiver::acquisition_sidecar(&iq_path),
            &self.sidecar_stamp,
        )
        .with_context(|| {
            format!(
                "verify admitted acquisition sidecar for {}",
                self.path.display()
            )
        })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn open_directory(path: &Path) -> Result<File> {
    let flags = OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW;
    OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits())
        .open(path)
        .with_context(|| format!("pin direct sweep directory {}", path.display()))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn open_directory(_path: &Path) -> Result<File> {
    anyhow::bail!("saved sweep admission requires Linux /proc/self/fd support")
}

pub(super) fn reopen_directory(path: &Path, stamp: &Metadata) -> Result<File> {
    let directory = open_directory(path)?;
    ensure!(
        same_inode(stamp, &directory.metadata()?),
        "sweep directory changed after admission: {}",
        path.display()
    );
    Ok(directory)
}

pub(super) fn descriptor_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

pub(super) fn read_bounded(path: &Path, maximum: u64) -> Result<(Vec<u8>, Metadata)> {
    let (mut file, length) = file_pin::open_regular(path, maximum)?;
    let before = file.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&mut file).take(maximum + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == length && same_file(&before, &file.metadata()?),
        "sweep input changed while reading: {}",
        path.display()
    );
    Ok((bytes, before))
}

pub(super) fn verify_path(path: &Path, expected: &Metadata) -> Result<()> {
    let observed = fs::symlink_metadata(path)?;
    ensure!(
        observed.is_file() && same_file(expected, &observed),
        "sweep input changed after admission: {}",
        path.display()
    );
    Ok(())
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}
