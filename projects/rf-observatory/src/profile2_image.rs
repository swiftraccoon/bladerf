//! Offline verification for a locally approved Profile-2 FPGA image bundle.
//!
//! A checkout-local approval pins the exact manifest bytes. The verifier also
//! checks the requested digest, fixed target and timing contract, maintained
//! source hashes, image, and evidence files before returning their identities.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, ensure};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BUNDLE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const PROFILE2_IMAGE_FILE: &str = "rf-observatory-profile2-xA9.rbf";
const PROFILE2_APPROVAL: &str = "config/profile2-approved-manifest.sha256";
const PROFILE2_PATCH: &str = "patches/bladerf-rf-observatory-profile2.patch";
const HDL_DIRECTORY: &str = "projects/rf-observatory/hdl";
const NUAND_COMMIT: &str = "fcf9423325ae49f5fdd2e5b52ab68bfc2576ad47";
const NUAND_TREE: &str = "ac4b4fc4646030ad6ca7e5f38fd74137573ffc29";
const PROFILE2_SOURCES: &[&str] = &[
    "rf_channel_bank.vhd",
    "rf_observatory_p.vhd",
    "rf_profile2.sdc",
    "rf_profile2_control_cdc.vhd",
    "rf_profile2_framer.vhd",
    "rf_profile2_init_reset.vhd",
    "rf_profile2_integration.vhd",
    "rf_profile2_p.vhd",
    "rf_profile2_slot_packer.vhd",
    "rf_profile2_wb_registers.vhd",
];
const QUALIFICATION_TOOLS: &[&str] = &[
    "nios2eds/sdk2/bin/nios2-bsp",
    "quartus/linux64/quartus_asm",
    "quartus/linux64/quartus_fit",
    "quartus/linux64/quartus_map",
    "quartus/linux64/quartus_sh",
    "quartus/linux64/quartus_sta",
];
const QUALIFICATION_EVIDENCE: &[&str] = &[
    "hdl-profile2-host-control-54-result.json",
    "hosted.asm.rpt",
    "hosted.fit.rpt",
    "hosted.flow.rpt",
    "hosted.map.rpt",
    "hosted.sta.rpt",
    "profile2-Fast_1100mV_0C_Model.rpt",
    "profile2-Fast_1100mV_85C_Model.rpt",
    "profile2-Slow_1100mV_0C_Model.rpt",
    "profile2-Slow_1100mV_85C_Model.rpt",
    "profile2-timing-summary.tsv",
    "recovery-Fast_1100mV_0C_Model.rpt",
    "recovery-Fast_1100mV_85C_Model.rpt",
    "recovery-Slow_1100mV_0C_Model.rpt",
    "recovery-Slow_1100mV_85C_Model.rpt",
];

fn deserialize_unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct UniqueMapVisitor<V>(PhantomData<V>);

    impl<'de, V> Visitor<'de> for UniqueMapVisitor<V>
    where
        V: Deserialize<'de>,
    {
        type Value = BTreeMap<String, V>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique keys")
        }

        fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = object.next_entry::<String, V>()? {
                if values.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate manifest map key {key:?}"
                    )));
                }
            }
            Ok(values)
        }
    }

    deserializer.deserialize_map(UniqueMapVisitor(PhantomData))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct QualifiedFile {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FitQualification {
    alms: u32,
    registers: u32,
    dsp_blocks: u32,
    ram_blocks: u32,
    block_memory_bits: u32,
    plls: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TimingQualification {
    checks: u32,
    negative_checks: u32,
    minimum_setup_ns: f64,
    minimum_hold_ns: f64,
    minimum_recovery_ns: f64,
    minimum_removal_ns: f64,
    minimum_pulse_width_ns: f64,
    broad_constraints_relaxed: bool,
    bundled_max_delay_ns: f64,
    bundled_minimum_slack_ns: f64,
    bundled_source_registers: u32,
    bundled_destination_registers: u32,
    reset_clear_destination_pins: u32,
    fifo_recovery_source_registers: u32,
    fifo_recovery_destination_pins: u32,
    scoped_fifo_recovery_exception: bool,
    unconstrained_clocks: u32,
    unconstrained_input_ports: u32,
    unconstrained_output_ports: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HardwareValidation {
    ram_load_passed: bool,
    native_control_passed: bool,
    native_capture_passed: bool,
    stock_image_restored: bool,
    flash_written: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageManifest {
    schema_version: u32,
    profile_name: String,
    profile_id: u16,
    transport_version: u16,
    register_layout: u16,
    register_id: u32,
    capabilities: u32,
    image_file: String,
    image_sha256: String,
    image_bytes: u64,
    device: String,
    revision: String,
    top_entity: String,
    tracked_input_commit: String,
    nuand_source_commit: String,
    nuand_source_tree: String,
    integration_patch_sha256: String,
    #[serde(deserialize_with = "deserialize_unique_map")]
    hdl_source_sha256: BTreeMap<String, String>,
    quartus: String,
    nios_configuration: String,
    fitter_seed: u32,
    parallel_processors: u32,
    #[serde(deserialize_with = "deserialize_unique_map")]
    tool_sha256: BTreeMap<String, String>,
    fit: FitQualification,
    timing: TimingQualification,
    #[serde(deserialize_with = "deserialize_unique_map")]
    evidence: BTreeMap<String, QualifiedFile>,
    hardware_validation: HardwareValidation,
}

/// Exact result of validating the private profile-2 image bundle offline.
///
/// Its paths are diagnostic snapshots, not stable file handles. A future
/// loader must revalidate or stage the image from a pinned descriptor rather
/// than treating this value as authorization to reopen a mutable path.
#[derive(Debug)]
pub(crate) struct VerificationReport {
    manifest: PathBuf,
    manifest_sha256: String,
    image: PathBuf,
    image_sha256: String,
    image_bytes: u64,
}

impl VerificationReport {
    pub(crate) fn manifest(&self) -> &Path {
        &self.manifest
    }

    pub(crate) fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    pub(crate) fn image(&self) -> &Path {
        &self.image
    }

    pub(crate) fn image_sha256(&self) -> &str {
        &self.image_sha256
    }

    pub(crate) const fn image_bytes(&self) -> u64 {
        self.image_bytes
    }
}

fn exact_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn ensure_running(stop: &AtomicBool) -> Result<()> {
    ensure!(
        !stop.load(Ordering::Relaxed),
        "profile-2 image verification was interrupted"
    );
    Ok(())
}

fn read_and_hash(path: &Path, maximum: u64, stop: &AtomicBool) -> Result<(Vec<u8>, String)> {
    let (mut file, expected_bytes) = crate::file_pin::open_regular(path, maximum)?;
    let mut bytes = Vec::with_capacity(usize::try_from(expected_bytes)?);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_running(stop)?;
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes.extend_from_slice(&buffer[..count]);
        ensure!(
            bytes.len() as u64 <= maximum,
            "{} grew beyond {maximum} bytes while reading",
            path.display()
        );
    }
    ensure!(
        bytes.len() as u64 == expected_bytes && file.metadata()?.len() == expected_bytes,
        "{} changed while reading",
        path.display()
    );
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn hash_file(path: &Path, maximum: u64, stop: &AtomicBool) -> Result<(String, u64)> {
    let (mut file, expected_bytes) = crate::file_pin::open_regular(path, maximum)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        ensure_running(stop)?;
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .context("profile-2 bundle byte count overflow")?;
        ensure!(
            bytes <= maximum,
            "{} grew beyond {maximum} bytes while hashing",
            path.display()
        );
    }
    ensure!(
        bytes == expected_bytes && file.metadata()?.len() == expected_bytes,
        "{} changed while hashing",
        path.display()
    );
    Ok((format!("{:x}", hash.finalize()), bytes))
}

fn verify_file(
    path: &Path,
    expected: &QualifiedFile,
    maximum: u64,
    stop: &AtomicBool,
) -> Result<()> {
    let (sha256, bytes) = hash_file(path, maximum, stop)?;
    ensure!(
        sha256 == expected.sha256 && bytes == expected.bytes,
        "{} differs from the qualified profile-2 bundle",
        path.display()
    );
    Ok(())
}

fn has_exact_keys<T>(entries: &BTreeMap<String, T>, names: &[&str]) -> bool {
    entries.len() == names.len() && names.iter().all(|name| entries.contains_key(*name))
}

fn validate_manifest(actual: &ImageManifest) -> Result<()> {
    ensure!(
        actual.schema_version == 2
            && actual.profile_name == "rf-observatory-profile2"
            && actual.profile_id == 2
            && actual.transport_version == 2
            && actual.register_layout == 1
            && actual.register_id == bladerf_profile2_ipc::PROFILE2_ID
            && actual.capabilities == bladerf_profile2_ipc::PROFILE2_CAPABILITIES
            && actual.device == "5CEBA9F23C8"
            && actual.revision == "hosted"
            && actual.top_entity == "bladerf"
            && actual.nios_configuration == "tiny"
            && actual.nuand_source_commit == NUAND_COMMIT
            && actual.nuand_source_tree == NUAND_TREE
            && actual.image_file == PROFILE2_IMAGE_FILE,
        "manifest fields differ from the xA9 Profile-2 image contract"
    );
    ensure!(
        has_exact_keys(&actual.hdl_source_sha256, PROFILE2_SOURCES)
            && has_exact_keys(&actual.tool_sha256, QUALIFICATION_TOOLS)
            && has_exact_keys(&actual.evidence, QUALIFICATION_EVIDENCE),
        "manifest has missing or unexpected files in its qualification records"
    );
    ensure!(
        exact_lower_sha256(&actual.image_sha256)
            && exact_lower_sha256(&actual.integration_patch_sha256)
            && actual
                .hdl_source_sha256
                .values()
                .all(|value| exact_lower_sha256(value))
            && actual
                .tool_sha256
                .values()
                .all(|value| exact_lower_sha256(value))
            && actual
                .evidence
                .values()
                .all(|value| exact_lower_sha256(&value.sha256)),
        "manifest contains a malformed or non-lowercase SHA-256"
    );
    let timing = &actual.timing;
    ensure!(
        timing.checks > 0
            && timing.negative_checks == 0
            && [
                timing.minimum_setup_ns,
                timing.minimum_hold_ns,
                timing.minimum_recovery_ns,
                timing.minimum_removal_ns,
                timing.minimum_pulse_width_ns,
                timing.bundled_minimum_slack_ns,
            ]
            .into_iter()
            .all(|slack| slack.is_finite() && slack >= 0.0)
            && !timing.broad_constraints_relaxed
            && timing.bundled_max_delay_ns == 12.5
            && timing.bundled_source_registers == 160
            && timing.bundled_destination_registers == 160
            && timing.reset_clear_destination_pins == 2
            && timing.fifo_recovery_source_registers == 1
            && timing.fifo_recovery_destination_pins == 2
            && timing.scoped_fifo_recovery_exception
            && timing.unconstrained_clocks == 0
            && timing.unconstrained_input_ports == 0
            && timing.unconstrained_output_ports == 0,
        "manifest timing qualification does not meet the Profile-2 contract"
    );
    ensure!(
        actual.hardware_validation == HardwareValidation::default(),
        "offline qualification manifest must not claim board validation or flash writes"
    );
    Ok(())
}

pub(crate) fn verify(
    root: &Path,
    manifest_path: &Path,
    expected_manifest_sha256: &str,
    stop: &AtomicBool,
) -> Result<VerificationReport> {
    ensure!(
        exact_lower_sha256(expected_manifest_sha256),
        "profile-2 manifest SHA-256 must be exactly 64 lowercase hexadecimal characters"
    );
    ensure_running(stop)?;
    let (approval, _) = read_and_hash(&root.join(PROFILE2_APPROVAL), 65, stop)
        .context("read checkout-local Profile-2 approval")?;
    let approval = std::str::from_utf8(&approval).context("Profile-2 approval is not UTF-8")?;
    let approved_digest = approval.strip_suffix('\n').unwrap_or(approval);
    ensure!(
        exact_lower_sha256(approved_digest) && approved_digest == expected_manifest_sha256,
        "manifest digest differs from the checkout-local Profile-2 approval"
    );
    let (manifest_bytes, manifest_sha256) = read_and_hash(manifest_path, MAX_MANIFEST_BYTES, stop)?;
    ensure!(
        manifest_sha256 == expected_manifest_sha256,
        "profile-2 manifest SHA-256 differs from the separately supplied digest"
    );
    let manifest: ImageManifest =
        serde_json::from_slice(&manifest_bytes).context("parse strict profile-2 image manifest")?;
    validate_manifest(&manifest)?;

    let patch = root.join(PROFILE2_PATCH);
    let (patch_sha256, _) = hash_file(&patch, MAX_SOURCE_BYTES, stop)?;
    ensure!(
        patch_sha256 == manifest.integration_patch_sha256,
        "profile-2 integration patch differs from the qualified manifest"
    );
    for (name, expected_sha256) in &manifest.hdl_source_sha256 {
        let source = root.join(HDL_DIRECTORY).join(name);
        let (actual_sha256, _) = hash_file(&source, MAX_SOURCE_BYTES, stop)?;
        ensure!(
            actual_sha256 == *expected_sha256,
            "maintained profile-2 source differs from the qualified manifest: {name}"
        );
    }

    let bundle = manifest_path
        .parent()
        .context("profile-2 manifest has no parent directory")?;
    let image = bundle.join(&manifest.image_file);
    verify_file(
        &image,
        &QualifiedFile {
            sha256: manifest.image_sha256.clone(),
            bytes: manifest.image_bytes,
        },
        MAX_BUNDLE_FILE_BYTES,
        stop,
    )?;
    for (name, expected_file) in &manifest.evidence {
        verify_file(
            &bundle.join(name),
            expected_file,
            MAX_BUNDLE_FILE_BYTES,
            stop,
        )?;
    }
    ensure_running(stop)?;

    Ok(VerificationReport {
        manifest: manifest_path.to_path_buf(),
        manifest_sha256,
        image,
        image_sha256: manifest.image_sha256,
        image_bytes: manifest.image_bytes,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;

    use super::*;

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn write_synthetic_bundle(directory: &Path) -> (PathBuf, PathBuf, ImageManifest, String) {
        let root = directory.join("workspace");
        let bundle = directory.join("bundle");
        fs::create_dir_all(root.join("patches")).unwrap();
        fs::create_dir_all(root.join(HDL_DIRECTORY)).unwrap();
        fs::create_dir_all(&bundle).unwrap();

        let patch = b"synthetic profile-2 patch\n";
        fs::write(root.join(PROFILE2_PATCH), patch).unwrap();
        let hdl_source_sha256 = PROFILE2_SOURCES
            .iter()
            .map(|&name| {
                fs::write(root.join(HDL_DIRECTORY).join(name), name).unwrap();
                (name.to_owned(), sha256(name.as_bytes()))
            })
            .collect();
        let evidence = QUALIFICATION_EVIDENCE
            .iter()
            .map(|&name| {
                fs::write(bundle.join(name), name).unwrap();
                (
                    name.to_owned(),
                    QualifiedFile {
                        sha256: sha256(name.as_bytes()),
                        bytes: name.len() as u64,
                    },
                )
            })
            .collect();

        let image = b"synthetic image bytes\n";
        fs::write(bundle.join(PROFILE2_IMAGE_FILE), image).unwrap();
        let manifest = ImageManifest {
            schema_version: 2,
            profile_name: "rf-observatory-profile2".into(),
            profile_id: 2,
            transport_version: 2,
            register_layout: 1,
            register_id: bladerf_profile2_ipc::PROFILE2_ID,
            capabilities: bladerf_profile2_ipc::PROFILE2_CAPABILITIES,
            image_file: PROFILE2_IMAGE_FILE.into(),
            image_sha256: sha256(image),
            image_bytes: image.len() as u64,
            device: "5CEBA9F23C8".into(),
            revision: "hosted".into(),
            top_entity: "bladerf".into(),
            tracked_input_commit: "0".repeat(40),
            nuand_source_commit: NUAND_COMMIT.into(),
            nuand_source_tree: NUAND_TREE.into(),
            integration_patch_sha256: sha256(patch),
            hdl_source_sha256,
            quartus: "synthetic toolchain".into(),
            nios_configuration: "tiny".into(),
            fitter_seed: 1,
            parallel_processors: 1,
            tool_sha256: QUALIFICATION_TOOLS
                .iter()
                .map(|&name| (name.to_owned(), sha256(name.as_bytes())))
                .collect(),
            fit: FitQualification {
                alms: 1,
                registers: 1,
                dsp_blocks: 1,
                ram_blocks: 1,
                block_memory_bits: 1,
                plls: 1,
            },
            timing: TimingQualification {
                checks: 1,
                negative_checks: 0,
                minimum_setup_ns: 1.0,
                minimum_hold_ns: 1.0,
                minimum_recovery_ns: 1.0,
                minimum_removal_ns: 1.0,
                minimum_pulse_width_ns: 1.0,
                broad_constraints_relaxed: false,
                bundled_max_delay_ns: 12.5,
                bundled_minimum_slack_ns: 1.0,
                bundled_source_registers: 160,
                bundled_destination_registers: 160,
                reset_clear_destination_pins: 2,
                fifo_recovery_source_registers: 1,
                fifo_recovery_destination_pins: 2,
                scoped_fifo_recovery_exception: true,
                unconstrained_clocks: 0,
                unconstrained_input_ports: 0,
                unconstrained_output_ports: 0,
            },
            evidence,
            hardware_validation: HardwareValidation::default(),
        };

        let manifest_path = bundle.join("manifest.json");
        let mut manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        manifest_bytes.push(b'\n');
        fs::write(&manifest_path, &manifest_bytes).unwrap();
        let manifest_sha256 = sha256(&manifest_bytes);
        let approval = root.join(PROFILE2_APPROVAL);
        fs::create_dir_all(approval.parent().unwrap()).unwrap();
        fs::write(approval, format!("{manifest_sha256}\n")).unwrap();
        (root, manifest_path, manifest, manifest_sha256)
    }

    #[test]
    fn an_unapproved_bundle_is_rejected_even_with_its_correct_digest() {
        let directory = tempfile::tempdir().unwrap();
        let (root, manifest_path, _, digest) = write_synthetic_bundle(directory.path());
        let stop = AtomicBool::new(false);
        let approval = root.join(PROFILE2_APPROVAL);

        fs::remove_file(&approval).unwrap();
        let err = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("read checkout-local Profile-2 approval"));

        fs::write(&approval, "0".repeat(64)).unwrap();
        let err = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{err:#}").contains("differs from the checkout-local Profile-2 approval"));
        let err = verify(&root, &manifest_path, &"0".repeat(64), &stop).unwrap_err();
        assert!(format!("{err:#}").contains("differs from the separately supplied digest"));
    }

    #[test]
    fn synthetic_bundle_validates_and_detects_image_drift() {
        let directory = tempfile::tempdir().unwrap();
        let (root, manifest_path, expected, digest) = write_synthetic_bundle(directory.path());
        let stop = AtomicBool::new(false);
        let verified = verify(&root, &manifest_path, &digest, &stop).unwrap();
        assert_eq!(verified.image_bytes(), expected.image_bytes);
        assert_eq!(verified.image_sha256(), expected.image_sha256);

        fs::write(
            manifest_path.parent().unwrap().join(PROFILE2_IMAGE_FILE),
            b"synthetic image byteS\n",
        )
        .unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("differs from the qualified"));
    }

    #[test]
    fn external_digest_sources_patch_and_evidence_are_all_bound() {
        let directory = tempfile::tempdir().unwrap();
        let (root, manifest_path, _, digest) = write_synthetic_bundle(directory.path());
        let stop = AtomicBool::new(false);

        let error = verify(&root, &manifest_path, &"0".repeat(64), &stop).unwrap_err();
        assert!(format!("{error:#}").contains("checkout-local Profile-2 approval"));

        fs::write(root.join(PROFILE2_PATCH), b"synthetic profile-2 patcH\n").unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("integration patch differs"));
        fs::write(root.join(PROFILE2_PATCH), b"synthetic profile-2 patch\n").unwrap();

        fs::write(
            root.join(HDL_DIRECTORY).join("rf_channel_bank.vhd"),
            b"changed HDL",
        )
        .unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("maintained profile-2 source differs"));
        fs::write(
            root.join(HDL_DIRECTORY).join("rf_channel_bank.vhd"),
            b"rf_channel_bank.vhd",
        )
        .unwrap();

        let fit = manifest_path.parent().unwrap().join("hosted.fit.rpt");
        fs::write(&fit, b"changed report").unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("differs from the qualified"));
        fs::write(&fit, b"hosted.fit.rpt").unwrap();

        fs::remove_file(manifest_path.parent().unwrap().join("hosted.sta.rpt")).unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("pin file without following symlinks"));
    }

    #[test]
    fn strict_manifest_rejects_unknown_or_inexact_contract_fields() {
        let directory = tempfile::tempdir().unwrap();
        let (_, _, expected, _) = write_synthetic_bundle(directory.path());
        validate_manifest(&expected).unwrap();
        let mut wrong = expected.clone();
        wrong.schema_version = 1;
        assert!(validate_manifest(&wrong).is_err());
        let mut wrong = expected.clone();
        wrong.profile_id = 1;
        assert!(validate_manifest(&wrong).is_err());
        let mut wrong = expected.clone();
        wrong
            .hdl_source_sha256
            .insert("extra.vhd".to_owned(), "0".repeat(64));
        assert!(validate_manifest(&wrong).is_err());
        let mut wrong = expected.clone();
        wrong.evidence.remove("hosted.sta.rpt");
        assert!(validate_manifest(&wrong).is_err());
        let mut wrong = expected.clone();
        wrong.hardware_validation.ram_load_passed = true;
        assert!(validate_manifest(&wrong).is_err());

        let mut wrong = expected.clone();
        wrong.timing.minimum_setup_ns = -0.01;
        assert!(validate_manifest(&wrong).is_err());

        let mut value = serde_json::to_value(&expected).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".to_owned(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<ImageManifest>(value).is_err());

        let json = serde_json::to_string(&expected).unwrap();
        let entry = format!(
            "\"rf_channel_bank.vhd\":\"{}\"",
            expected.hdl_source_sha256["rf_channel_bank.vhd"]
        );
        let duplicated = json.replacen(&entry, &format!("{entry},{entry}"), 1);
        let error = serde_json::from_str::<ImageManifest>(&duplicated).unwrap_err();
        assert!(error.to_string().contains("duplicate manifest map key"));
    }

    #[test]
    fn digest_interrupt_and_nonregular_inputs_fail_before_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let (root, manifest_path, _, digest) = write_synthetic_bundle(directory.path());
        let stop = AtomicBool::new(false);

        let error = verify(
            &root,
            Path::new("/this/manifest/must/not/be-opened"),
            &digest.to_ascii_uppercase(),
            &stop,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("lowercase"));

        stop.store(true, Ordering::Relaxed);
        let error = verify(
            &root,
            Path::new("/this/manifest/must/not/be-opened"),
            &digest,
            &stop,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("interrupted"));
        stop.store(false, Ordering::Relaxed);

        let image = manifest_path.parent().unwrap().join(PROFILE2_IMAGE_FILE);
        let target = manifest_path.parent().unwrap().join("image-target.rbf");
        fs::rename(&image, &target).unwrap();
        symlink(&target, &image).unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("nonempty regular file"));

        fs::remove_file(&image).unwrap();
        mkfifo(&image, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let error = verify(&root, &manifest_path, &digest, &stop).unwrap_err();
        assert!(format!("{error:#}").contains("nonempty regular file"));
    }
}
