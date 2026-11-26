//! Verify only the saved IQ intervals used by selected activity rows.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::sweep::CompletedOperation;
use crate::triage::admission::Series;
use crate::triage::{COMPLEX_BYTES, check_stop};

/// Series identity, acquisition scope and visit order reported by triage.
pub(crate) struct ExpectedSeries<'a> {
    pub(crate) sha256: &'a str,
    pub(crate) operation: CompletedOperation,
    pub(crate) requested_range: Option<&'a Range<u64>>,
    pub(crate) visit_centers_hz: Option<&'a [u64]>,
}

/// Selected interval and its lineage in an admitted series triage report.
pub(crate) struct Witness<'a> {
    /// One-based pass and tune positions in acquisition order.
    pub(crate) pass: usize,
    pub(crate) tune: usize,
    pub(crate) source_sweep_sha256: &'a str,
    pub(crate) source_sidecar_sha256: &'a str,
    pub(crate) requested_center_hz: u64,
    pub(crate) observed_center_hz: u64,
    pub(crate) saved_samples: u64,
    /// The full analyzed interval containing the row, in complex sample coordinates.
    pub(crate) analyzed_samples: Range<u64>,
    pub(crate) analyzed_iq_sha256: &'a str,
}

/// Distinct analyzed intervals checked against the bound source capture.
pub(crate) struct VerifiedIq {
    pub(crate) interval_count: usize,
    pub(crate) bytes: u64,
}

/// Reopen the completed series and hash the selected analyzed IQ intervals.
/// Each distinct interval is read once. Other files are admitted by metadata.
///
/// # Errors
/// Returns an error for interruption, inconsistent or changed evidence,
/// or source access failure.
pub(crate) fn verify<'a>(
    directory: &Path,
    expected: ExpectedSeries<'_>,
    witnesses: impl IntoIterator<Item = Witness<'a>>,
    stop: &AtomicBool,
) -> Result<VerifiedIq> {
    check_stop(stop)?;
    let series = Series::open(directory)?;
    ensure!(
        series.source_series_sha256 == expected.sha256,
        "saved series digest differs from the activity report"
    );
    let reference = &series.passes[0].sweep;
    ensure!(
        reference.operation == expected.operation
            && reference.requested_range.as_ref() == expected.requested_range
            && reference.visit_centers_hz.as_deref() == expected.visit_centers_hz,
        "saved series scope or visit order differs from the companion triage report"
    );
    let mut intervals = BTreeMap::new();
    for witness in witnesses {
        let pass = witness
            .pass
            .checked_sub(1)
            .and_then(|index| series.passes.get(index))
            .context("activity source pass is absent from the saved series")?;
        let tune = witness
            .tune
            .checked_sub(1)
            .and_then(|index| pass.sweep.tunes.get(index))
            .context("activity source tune is absent from the saved pass")?;
        ensure!(
            pass.sweep.source_receipt_sha256 == witness.source_sweep_sha256
                && tune.sidecar_sha256 == witness.source_sidecar_sha256
                && tune.requested_center_hz == witness.requested_center_hz
                && tune.observed_center_hz == witness.observed_center_hz
                && tune.samples == witness.saved_samples,
            "activity source metadata differs from {}/{}",
            pass.directory,
            tune.directory
        );
        let extent = witness.analyzed_samples;
        ensure!(
            extent.start < extent.end && extent.end <= tune.samples,
            "activity interval lies outside {}/{}",
            pass.directory,
            tune.directory
        );
        let key = (witness.pass, witness.tune, extent.start, extent.end);
        let (_, digest) = intervals
            .entry(key)
            .or_insert((tune, witness.analyzed_iq_sha256));
        ensure!(
            *digest == witness.analyzed_iq_sha256,
            "activity IQ digests disagree for the same analyzed interval"
        );
    }

    let mut buffer = [0; 16 * 1024];
    let mut verified = VerifiedIq {
        interval_count: intervals.len(),
        bytes: 0,
    };
    for ((pass, visit, start, end), (tune, expected)) in intervals {
        check_stop(stop)?;
        let mut iq = tune.open_iq()?;
        iq.seek(SeekFrom::Start(start * COMPLEX_BYTES))?;
        let mut remaining = (end - start) * COMPLEX_BYTES;
        let mut digest = Sha256::new();
        while remaining > 0 {
            check_stop(stop)?;
            let count = remaining.min(buffer.len() as u64) as usize;
            iq.read_exact(&mut buffer[..count])?;
            digest.update(&buffer[..count]);
            remaining -= count as u64;
        }
        tune.verify_iq(&iq)?;
        ensure!(
            format!("{:x}", digest.finalize()) == expected,
            "activity IQ SHA-256 differs for pass {pass} tune {visit}, samples {start}..{end}"
        );
        verified.bytes += (end - start) * COMPLEX_BYTES;
    }
    series.verify()?;
    check_stop(stop)?;
    Ok(verified)
}
