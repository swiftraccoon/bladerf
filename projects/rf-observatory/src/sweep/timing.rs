//! Validate native host-clock phase boundaries and summarize sweep cadence.

use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file_pin;

use super::TuneRequest;

const REPORT_FILE: &str = "acquisition-timing.json";
const MAX_TIMING_BYTES: u64 = 1024 * 1024;

/// A host-clock interval for reading and writing one completed dwell.
/// Both timestamps are offsets from the start of the native session.
pub(crate) struct HostRead {
    /// Host read start in nanoseconds.
    pub(crate) started_ns: u64,
    /// Host read completion in nanoseconds.
    pub(crate) finished_ns: u64,
}

/// Ordered host read intervals from a digest-bound native timing receipt.
pub(crate) struct BoundTiming {
    path: PathBuf,
    stamp: Metadata,
    reads: Vec<HostRead>,
}

impl BoundTiming {
    /// Open the timing file under `root` and match its SHA-256 and dwell order.
    /// Supply every requested center in pass order, including repeated passes.
    pub(crate) fn open(
        root: &Path,
        expected_sha256: &str,
        centers_hz: &[u64],
        passes: usize,
    ) -> Result<Self> {
        let path = root.join(REPORT_FILE);
        let (report, sha256, stamp) = Report::load(&path)?;
        ensure!(
            sha256 == expected_sha256,
            "native sweep timing SHA-256 differs from the completed sweep receipt"
        );
        report.validate(centers_hz.iter().copied(), passes)?;
        let reads = report
            .dwells
            .iter()
            .map(|dwell| HostRead {
                started_ns: dwell.read_started_ns,
                finished_ns: dwell.read_finished_ns,
            })
            .collect();
        Ok(Self { path, stamp, reads })
    }

    /// Return the completed dwell intervals in native acquisition order.
    pub(crate) fn reads(&self) -> &[HostRead] {
        &self.reads
    }

    /// Recheck the receipt identity and modification stamps before publication.
    /// Keep the directory descriptor alive when `root` uses `/proc/self/fd`.
    pub(crate) fn verify(&self) -> Result<()> {
        verify_path(&self.path, &self.stamp)
    }
}

#[derive(Deserialize)]
struct Report {
    schema_version: u32,
    operation: String,
    #[serde(default = "single_pass")]
    passes: usize,
    device_ready_ns: u64,
    device_closed_ns: u64,
    dwells: Vec<Dwell>,
}

const fn single_pass() -> usize {
    1
}

#[derive(Deserialize)]
struct Dwell {
    requested_center_hz: u64,
    started_ns: u64,
    tuned_ns: u64,
    files_ready_ns: u64,
    stream_ready_ns: u64,
    read_started_ns: u64,
    read_finished_ns: u64,
    stream_closed_ns: u64,
    published_ns: u64,
}

impl Dwell {
    fn ordered_between(&self, lower: u64, upper: u64) -> bool {
        [
            lower,
            self.started_ns,
            self.tuned_ns,
            self.files_ready_ns,
            self.stream_ready_ns,
            self.read_started_ns,
            self.read_finished_ns,
            self.stream_closed_ns,
            self.published_ns,
            upper,
        ]
        .is_sorted()
    }
}

#[derive(Serialize)]
pub(super) struct Summary {
    sha256: String,
    native_elapsed_ns: u64,
    device_open_setup_ns: u64,
    after_last_publish_ns: u64,
    before_first_dwell_ns: u64,
    phase_totals_ns: PhaseTotals,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_read_start_spacing_ns: Option<Spacing>,
}

/// Validated host timing for the native session and its passes.
pub(super) struct Evidence {
    pub(super) summary: Summary,
    /// Time from the first dwell's start through the last dwell's publication.
    pub(super) pass_spans: Vec<Duration>,
    /// Host read-start spacings for matching requested centers across adjacent passes.
    pub(super) same_center_revisit_ns: Option<Spacing>,
}

#[derive(Default, Serialize)]
struct PhaseTotals {
    retune_and_gain: u64,
    file_preparation: u64,
    stream_setup_and_pre_activation_readback: u64,
    activation_and_final_readback: u64,
    read_and_write: u64,
    stream_shutdown: u64,
    publication: u64,
    between_dwells: u64,
}

#[derive(Serialize)]
pub(super) struct Spacing {
    min: u64,
    median: u64,
    max: u64,
}

impl Spacing {
    fn from_offsets(mut intervals: Vec<u64>) -> Option<Self> {
        intervals.sort_unstable();
        let min = *intervals.first()?;
        let max = *intervals.last()?;
        let median = intervals[(intervals.len() - 1) / 2].midpoint(intervals[intervals.len() / 2]);
        Some(Self { min, median, max })
    }
}

impl Report {
    fn load(path: &Path) -> Result<(Self, String, Metadata)> {
        let (mut file, length) = file_pin::open_regular(path, MAX_TIMING_BYTES)?;
        let before = file.metadata()?;
        let mut bytes = Vec::with_capacity(usize::try_from(length)?);
        (&mut file)
            .take(length + 1)
            .read_to_end(&mut bytes)
            .context("read native sweep timing")?;
        let after = file.metadata()?;
        ensure!(
            bytes.len() as u64 == length
                && before.len() == length
                && file_pin::same_file(&before, &after),
            "native sweep timing changed while reading"
        );
        verify_path(path, &after)?;
        let report = serde_json::from_slice(&bytes).context("parse native sweep timing")?;
        Ok((report, format!("{:x}", Sha256::digest(&bytes)), after))
    }

    fn validate(
        &self,
        centers_hz: impl ExactSizeIterator<Item = u64>,
        passes: usize,
    ) -> Result<usize> {
        ensure!(
            self.schema_version == 1 && self.operation == "native_rf_sweep_timing",
            "unsupported native sweep timing contract"
        );
        let dwell_count = centers_hz.len();
        ensure!(
            dwell_count > 0 && self.dwells.len() == dwell_count,
            "native timing dwell count differs from the completed sweep"
        );
        ensure!(
            passes > 0 && self.passes == passes && dwell_count.is_multiple_of(passes),
            "native timing pass count differs from the completed sweep"
        );
        ensure!(
            self.device_ready_ns <= self.device_closed_ns,
            "invalid native timing device phase order"
        );
        let tunes_per_pass = dwell_count / passes;
        let mut previous_published = self.device_ready_ns;
        for (index, (dwell, center_hz)) in self.dwells.iter().zip(centers_hz).enumerate() {
            ensure!(
                dwell.requested_center_hz == center_hz
                    && dwell.requested_center_hz
                        == self.dwells[index % tunes_per_pass].requested_center_hz
                    && dwell.ordered_between(previous_published, self.device_closed_ns),
                "invalid native timing center or phase order for dwell {}",
                index + 1
            );
            previous_published = dwell.published_ns;
        }
        Ok(tunes_per_pass)
    }

    fn summarize(&self, sha256: String, tunes_per_pass: usize) -> Evidence {
        let mut phases = PhaseTotals::default();
        let mut previous_published = self.device_ready_ns;
        for (index, dwell) in self.dwells.iter().enumerate() {
            // Disjoint validated intervals make each total at most device_closed_ns.
            if index > 0 {
                phases.between_dwells += dwell.started_ns - previous_published;
            }
            phases.retune_and_gain += dwell.tuned_ns - dwell.started_ns;
            phases.file_preparation += dwell.files_ready_ns - dwell.tuned_ns;
            phases.stream_setup_and_pre_activation_readback +=
                dwell.stream_ready_ns - dwell.files_ready_ns;
            phases.activation_and_final_readback += dwell.read_started_ns - dwell.stream_ready_ns;
            phases.read_and_write += dwell.read_finished_ns - dwell.read_started_ns;
            phases.stream_shutdown += dwell.stream_closed_ns - dwell.read_finished_ns;
            phases.publication += dwell.published_ns - dwell.stream_closed_ns;
            previous_published = dwell.published_ns;
        }
        let spacing = self
            .dwells
            .windows(2)
            .map(|dwells| dwells[1].read_started_ns - dwells[0].read_started_ns)
            .collect();
        let pass_spans = self
            .dwells
            .chunks_exact(tunes_per_pass)
            .map(|pass| {
                Duration::from_nanos(pass[tunes_per_pass - 1].published_ns - pass[0].started_ns)
            })
            .collect();
        let mut last_start_by_center = BTreeMap::new();
        let revisit = self
            .dwells
            .iter()
            .filter_map(|dwell| {
                let previous = last_start_by_center
                    .insert(dwell.requested_center_hz, dwell.read_started_ns)?;
                Some(dwell.read_started_ns - previous)
            })
            .collect();
        Evidence {
            summary: Summary {
                sha256,
                native_elapsed_ns: self.device_closed_ns,
                device_open_setup_ns: self.device_ready_ns,
                after_last_publish_ns: self.device_closed_ns - previous_published,
                before_first_dwell_ns: self.dwells[0].started_ns - self.device_ready_ns,
                phase_totals_ns: phases,
                host_read_start_spacing_ns: Spacing::from_offsets(spacing),
            },
            pass_spans,
            same_center_revisit_ns: Spacing::from_offsets(revisit),
        }
    }
}

fn verify_path(path: &Path, stamp: &Metadata) -> Result<()> {
    let current = fs::symlink_metadata(path).context("reinspect native sweep timing")?;
    ensure!(
        current.is_file() && file_pin::same_file(stamp, &current),
        "native sweep timing changed after admission"
    );
    Ok(())
}

/// Read the optional timing evidence after the native process exits successfully.
/// Legacy receivers may omit it. A present report must agree with the completed batch.
pub(super) fn read(
    output: &Path,
    requests: &[TuneRequest],
    wall: Duration,
    passes: usize,
) -> Result<Option<Evidence>> {
    let path = output.join(REPORT_FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect native sweep timing"),
        Ok(_) => {}
    }
    let (report, sha256, _) = Report::load(&path)?;
    let tunes_per_pass = report.validate(
        requests.iter().map(|request| request.settings.center),
        passes,
    )?;
    ensure!(
        u128::from(report.device_closed_ns) <= wall.as_nanos(),
        "native timing exceeds the measured acquisition wall time"
    );
    Ok(Some(report.summarize(sha256, tunes_per_pass)))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::fd::AsRawFd;

    use serde_json::{Value, json};

    use super::*;

    const CENTERS_HZ: [u64; 4] = [915_000_000, 916_000_000, 915_000_000, 916_000_000];

    fn timing_report() -> Value {
        let dwells: Vec<_> = CENTERS_HZ
            .iter()
            .enumerate()
            .map(|(index, center_hz)| {
                let base = index as u64 * 10;
                json!({
                    "requested_center_hz": center_hz,
                    "started_ns": base + 1,
                    "tuned_ns": base + 2,
                    "files_ready_ns": base + 3,
                    "stream_ready_ns": base + 4,
                    "read_started_ns": base + 5,
                    "read_finished_ns": base + 6,
                    "stream_closed_ns": base + 7,
                    "published_ns": base + 8,
                })
            })
            .collect();
        json!({
            "schema_version": 1,
            "operation": "native_rf_sweep_timing",
            "passes": 2,
            "device_ready_ns": 0,
            "device_closed_ns": 40,
            "dwells": dwells,
        })
    }

    #[test]
    fn binds_dwell_reads_to_the_admitted_timing_file() {
        let directory = tempfile::tempdir().unwrap();
        let descriptor = fs::File::open(directory.path()).unwrap();
        let root = PathBuf::from(format!("/proc/self/fd/{}", descriptor.as_raw_fd()));
        let path = directory.path().join(REPORT_FILE);
        let mut report = timing_report();
        let bytes = serde_json::to_vec(&report).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        fs::write(&path, &bytes).unwrap();

        let timing = BoundTiming::open(&root, &sha256, &CENTERS_HZ, 2).unwrap();
        let reads = timing
            .reads()
            .iter()
            .map(|interval| (interval.started_ns, interval.finished_ns))
            .collect::<Vec<_>>();
        assert_eq!(reads, [(5, 6), (15, 16), (25, 26), (35, 36)]);
        timing.verify().unwrap();

        let replacement = directory.path().join("replacement.json");
        fs::write(&replacement, &bytes).unwrap();
        fs::rename(replacement, &path).unwrap();
        assert!(timing.verify().is_err());

        let timing = BoundTiming::open(&root, &sha256, &CENTERS_HZ, 2).unwrap();
        report["device_closed_ns"] = json!(41);
        let changed = serde_json::to_vec(&report).unwrap();
        assert_eq!(changed.len(), bytes.len());
        fs::write(&path, changed).unwrap();
        fs::File::open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        assert!(timing.verify().is_err());
        assert!(BoundTiming::open(&root, &sha256, &CENTERS_HZ, 2).is_err());
    }

    #[test]
    fn rejects_mismatched_passes_centers_and_phase_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let mut report = timing_report();
        let bytes = serde_json::to_vec(&report).unwrap();
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let path = root.join(REPORT_FILE);
        fs::write(&path, bytes).unwrap();
        assert!(BoundTiming::open(root, &sha256, &CENTERS_HZ, 1).is_err());
        assert!(BoundTiming::open(root, &sha256, &CENTERS_HZ[..2], 2).is_err());
        let mut wrong_centers = CENTERS_HZ;
        wrong_centers.swap(0, 1);
        assert!(BoundTiming::open(root, &sha256, &wrong_centers, 2).is_err());

        report["dwells"][3]["read_finished_ns"] = json!(34);
        let invalid = serde_json::to_vec(&report).unwrap();
        let invalid_sha256 = format!("{:x}", Sha256::digest(&invalid));
        fs::write(path, invalid).unwrap();
        assert!(BoundTiming::open(root, &invalid_sha256, &CENTERS_HZ, 2).is_err());
    }
}
