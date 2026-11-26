//! Read the bounded frequency-only input for a sparse decoder follow-up.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::plan::{self, Plan};

const MAX_PLAN_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
struct FollowUp {
    schema_version: u32,
    operation: String,
    source_sweep_sha256: String,
    centers_hz: Vec<u64>,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    frequency_hz: u64,
}

/// Import centers and retain the identities of the input plan and its source sweep.
pub(super) fn read(path: &Path, dwell_ms: u64, max_iq_bytes: u64) -> Result<Plan> {
    let (mut file, length) = crate::file_pin::open_regular(path, MAX_PLAN_BYTES)?;
    let before = file.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&mut file)
        .take(MAX_PLAN_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read sweep follow-up plan {}", path.display()))?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() as u64 == length
            && after.len() == length
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec(),
        "sweep follow-up plan changed while reading"
    );
    let imported: FollowUp = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse sweep follow-up plan {}", path.display()))?;
    ensure!(
        imported.schema_version == 1 && imported.operation == "plan_sweep_follow_up",
        "unsupported sweep follow-up plan contract"
    );
    ensure!(
        imported.source_sweep_sha256.len() == 64
            && HEXLOWER
                .decode(imported.source_sweep_sha256.as_bytes())
                .is_ok(),
        "follow-up plan needs a lowercase source sweep SHA-256"
    );
    ensure!(
        !imported.centers_hz.is_empty(),
        "follow-up plan has no missing decoder centers"
    );
    ensure!(
        imported.centers_hz.is_sorted_by(|left, right| left < right),
        "follow-up plan centers must be strictly increasing"
    );
    let mut target_hz: Vec<_> = imported
        .targets
        .iter()
        .map(|target| target.frequency_hz)
        .collect();
    target_hz.sort_unstable();
    target_hz.dedup();
    ensure!(
        target_hz == imported.centers_hz,
        "follow-up plan centers differ from its decoder targets"
    );
    let mut plan = plan::targeted(&imported.centers_hz, dwell_ms, max_iq_bytes)?;
    plan.follow_up_plan_sha256 = Some(HEXLOWER.encode(&Sha256::digest(&bytes)));
    plan.source_sweep_sha256 = Some(imported.source_sweep_sha256);
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_capture_centers_must_match_decoder_targets() {
        let root = tempfile::tempdir().unwrap();
        let plan_file = root.path().join("follow-up.json");
        let mut plan = serde_json::json!({
            "schema_version": 1,
            "operation": "plan_sweep_follow_up",
            "source_sweep_sha256": "a".repeat(64),
            "centers_hz": [2_412_000_000u64],
            "targets": [{"frequency_hz": 2_437_000_000u64}],
        });
        std::fs::write(&plan_file, serde_json::to_vec(&plan).unwrap()).unwrap();
        let error = read(&plan_file, 1, 80_000).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differ from its decoder targets")
        );

        plan["targets"][0]["frequency_hz"] = serde_json::json!(2_412_000_000u64);
        let bytes = serde_json::to_vec(&plan).unwrap();
        std::fs::write(&plan_file, &bytes).unwrap();
        let verified = read(&plan_file, 1, 80_000).unwrap();
        assert!(verified.is_targeted());
        assert_eq!(verified.centers_hz, [2_412_000_000]);
        assert_eq!(
            verified.follow_up_plan_sha256,
            Some(HEXLOWER.encode(&Sha256::digest(&bytes)))
        );
    }
}
