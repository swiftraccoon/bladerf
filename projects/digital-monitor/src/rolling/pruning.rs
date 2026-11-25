//! Checkpoint verified rolling evidence before retiring its detailed trees.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use data_encoding::HEXLOWER;
use nix::fcntl::{RenameFlags, renameat2};
use ring::digest::{self, SHA256};
use serde::Serialize;

use crate::report;
use crate::watch::storage::{remove_quarantined_tree, tree_bytes};

use super::burst::BurstCounts;
use super::cadence;
use super::detection::WifiActivityWindows;
use super::wifi_ht::WifiHtModes;
use super::{Record, SegmentReport};

const CHECKPOINT_NAME: &str = "retired.json";
const CHECKPOINT_MAX_BYTES: u64 = 4096;

#[derive(Default, Serialize)]
pub(super) struct RetiredPrefix {
    pub(super) retired_segments: usize,
    pub(super) end_sample_exclusive_ns: Option<i64>,
    pub(super) segment_report_chain_sha256: Option<String>,
    pub(super) direct_decoded_segments: usize,
    pub(super) ieee_nominal_decoded_segments: usize,
    validated_wifi_packets: usize,
    validated_bluetooth_packets: usize,
    validated_ieee802154_frames: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_activity_windows: Option<WifiActivityWindows>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_gain_clipped_components: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_ht_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_ht_modes: Option<WifiHtModes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_he_su_headers: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) wifi_he_su_signal_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) band_bursts: Option<BurstCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) short_band_burst_clips: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) short_burst_cadence: Option<cadence::Counts>,
}

#[derive(Serialize)]
struct Checkpoint<'a> {
    schema_version: u32,
    operation: &'static str,
    #[serde(flatten)]
    prefix: &'a RetiredPrefix,
}

impl RetiredPrefix {
    fn checkpoint(&self) -> Checkpoint<'_> {
        Checkpoint {
            schema_version: 1,
            operation: "rolling_rx1_retired_prefix",
            prefix: self,
        }
    }

    /// Condense the next verified record, then prune its released and unused trees.
    ///
    /// The caller joins readers of the record's overlap tail and verifies the
    /// decoder evidence before retirement. An error after the checkpoint may
    /// leave trees behind. The prefix still records their durable summary.
    pub(super) fn retire(
        &mut self,
        record: &Record,
        capture_dir: &Path,
        reports_dir: &Path,
    ) -> Result<()> {
        let receipt = &record.segment.receipt;
        ensure!(
            receipt.index == self.retired_segments
                && self
                    .end_sample_exclusive_ns
                    .is_none_or(|end| end == receipt.first_sample_ns),
            "rolling retirement must append a contiguous segment"
        );
        let release = record
            .iq_release
            .as_ref()
            .context("rolling retirement requires released IQ")?;
        let retired_segments = self
            .retired_segments
            .checked_add(1)
            .context("rolling retirement ordinal overflow")?;
        let ordinal = format!("{retired_segments:06}");
        let output_path = capture_dir
            .parent()
            .context("rolling capture directory has no parent")?;
        ensure!(
            reports_dir.parent() == Some(output_path) && capture_dir != reports_dir,
            "rolling retirement requires sibling capture and report roots"
        );
        let output = open_directory(output_path)?;
        let capture = open_directory(capture_dir)?;
        let reports = open_directory(reports_dir)?;
        let device = output.metadata()?.dev();
        let capture_stamp = snapshot_tree(&fd_path(&capture).join(&ordinal), device)?;
        let report_stamp = snapshot_tree(&fd_path(&reports).join(&ordinal), device)?;
        release.verify_after_release(&record.segment)?;

        // Chain the preceding binary digest, initially zero, with the compact
        // JSON encoding of this record. Order and complete receipts are bound.
        let previous_chain = self
            .segment_report_chain_sha256
            .as_ref()
            .map(|digest| HEXLOWER.decode(digest.as_bytes()))
            .transpose()?;
        let mut chain = digest::Context::new(&SHA256);
        chain.update(previous_chain.as_deref().unwrap_or(&[0; 32]));
        chain.update(&serde_json::to_vec(&SegmentReport::from(record))?);
        let (wifi, bluetooth) = record
            .direct_decode
            .as_ref()
            .map_or((0, 0), |direct| direct.validated_packets());
        let ieee802154 = record
            .ieee_nominal
            .as_ref()
            .map_or(Ok(0), |nominal| nominal.validated_frames())?;
        let wifi_gain_clipped_components = match record.wifi_gain_clipped_components() {
            Some(count) => Some(
                self.wifi_gain_clipped_components
                    .unwrap_or(0)
                    .checked_add(count)
                    .context("retired Wi-Fi input clipping count overflow")?,
            ),
            None => self.wifi_gain_clipped_components,
        };
        let next = Self {
            retired_segments,
            end_sample_exclusive_ns: Some(receipt.end_sample_exclusive_ns),
            segment_report_chain_sha256: Some(HEXLOWER.encode(chain.finish().as_ref())),
            direct_decoded_segments: self.direct_decoded_segments
                + usize::from(record.direct_decode.is_some()),
            ieee_nominal_decoded_segments: self.ieee_nominal_decoded_segments
                + usize::from(record.ieee_nominal.is_some()),
            validated_wifi_packets: self
                .validated_wifi_packets
                .checked_add(wifi)
                .context("retired Wi-Fi packet count overflow")?,
            validated_bluetooth_packets: self
                .validated_bluetooth_packets
                .checked_add(bluetooth)
                .context("retired Bluetooth packet count overflow")?,
            validated_ieee802154_frames: self
                .validated_ieee802154_frames
                .checked_add(ieee802154)
                .context("retired IEEE 802.15.4 frame count overflow")?,
            wifi_activity_windows: WifiActivityWindows::total(
                self.wifi_activity_windows
                    .into_iter()
                    .chain(record.wifi_activity_windows()),
            ),
            wifi_gain_clipped_components,
            wifi_ht_signal_clips: match record.wifi_ht_signal_clip_count() {
                Some(count) => Some(
                    self.wifi_ht_signal_clips
                        .unwrap_or(0)
                        .checked_add(count)
                        .context("retired Wi-Fi HT clip count overflow")?,
                ),
                None => self.wifi_ht_signal_clips,
            },
            wifi_ht_modes: WifiHtModes::total(
                self.wifi_ht_modes.iter().chain(record.wifi_ht_modes()),
            )?,
            wifi_he_su_headers: match record.wifi_he_su_header_count() {
                Some(count) => Some(
                    self.wifi_he_su_headers
                        .unwrap_or(0)
                        .checked_add(count)
                        .context("retired Wi-Fi HE20 SU header count overflow")?,
                ),
                None => self.wifi_he_su_headers,
            },
            wifi_he_su_signal_clips: match record.wifi_he_su_signal_clip_count() {
                Some(count) => Some(
                    self.wifi_he_su_signal_clips
                        .unwrap_or(0)
                        .checked_add(count)
                        .context("retired Wi-Fi HE20 SU clip count overflow")?,
                ),
                None => self.wifi_he_su_signal_clips,
            },
            band_bursts: BurstCounts::total(
                self.band_bursts
                    .into_iter()
                    .chain(record.band_burst_counts()),
            ),
            band_burst_clips: self
                .band_burst_clips
                .into_iter()
                .chain(record.band_burst_clip_count())
                .reduce(|total, count| total + count),
            short_band_burst_clips: match record.short_band_burst_clip_count() {
                Some(count) => Some(
                    self.short_band_burst_clips
                        .unwrap_or(0)
                        .checked_add(count)
                        .context("retired short band-burst clip count overflow")?,
                ),
                None => self.short_band_burst_clips,
            },
            short_burst_cadence: cadence::Counts::total(
                self.short_burst_cadence.into_iter().chain(
                    record
                        .short_burst_cadence
                        .as_ref()
                        .map(cadence::Score::counts),
                ),
            ),
        };
        self.publish_checkpoint(&next, &output)?;
        *self = next;

        prune_trees(
            &output,
            &ordinal,
            [
                (&capture, &capture_stamp, "capture"),
                (&reports, &report_stamp, "reports"),
            ],
        )
    }

    fn publish_checkpoint(&self, next: &Self, output: &File) -> Result<()> {
        let directory = fd_path(output);
        let path = directory.join(CHECKPOINT_NAME);
        self.verify_checkpoint_path(&path)?;
        let mut pending = tempfile::NamedTempFile::new_in(&directory)?;
        serde_json::to_writer_pretty(&mut pending, &next.checkpoint())?;
        pending.write_all(b"\n")?;
        ensure!(
            pending.as_file().metadata()?.len() <= next.checkpoint_byte_limit(),
            "rolling retirement checkpoint exceeds its byte limit"
        );
        pending.as_file().sync_all()?;
        if self.retired_segments == 0 {
            pending
                .persist_noclobber(&path)
                .context("publish first rolling retirement checkpoint")?;
        } else {
            pending
                .persist(&path)
                .context("replace rolling retirement checkpoint")?;
        }
        output
            .sync_all()
            .context("sync rolling retirement checkpoint")
    }

    pub(super) fn verify(&self, output: &Path) -> Result<()> {
        let directory = open_directory(output)?;
        self.verify_checkpoint_path(&fd_path(&directory).join(CHECKPOINT_NAME))
    }

    fn checkpoint_byte_limit(&self) -> u64 {
        CHECKPOINT_MAX_BYTES
            + self
                .wifi_ht_modes
                .as_ref()
                .map_or(0, WifiHtModes::json_budget)
    }

    fn verify_checkpoint_path(&self, path: &Path) -> Result<()> {
        if self.retired_segments == 0 {
            return match fs::symlink_metadata(path) {
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("inspect rolling retirement checkpoint"),
                Ok(_) => anyhow::bail!("rolling retirement checkpoint already exists"),
            };
        }
        let (published, _) =
            report::read_json_with_sha256_limit(path, self.checkpoint_byte_limit())?;
        ensure!(
            published == serde_json::to_value(self.checkpoint())?,
            "rolling retirement checkpoint changed"
        );
        Ok(())
    }
}

fn prune_trees(output: &File, ordinal: &str, sources: [(&File, &Metadata, &str); 2]) -> Result<()> {
    // Keep the quarantine on error so a failed or unexpected move never
    // triggers recursive deletion through a temporary directory destructor.
    let quarantine = tempfile::Builder::new()
        .prefix(".retire-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(fd_path(output))?
        .keep();
    let destination = open_directory(&quarantine)?;
    for (parent, stamp, name) in sources {
        renameat2(
            parent,
            Path::new(ordinal),
            &destination,
            Path::new(name),
            RenameFlags::RENAME_NOREPLACE,
        )
        .with_context(|| format!("quarantine retired rolling {name} tree"))?;
        verify_directory(&fd_path(&destination).join(name), stamp)?;
        parent.sync_all()?;
    }
    destination.sync_all()?;
    verify_directory(&quarantine, &destination.metadata()?)?;
    tree_bytes(&quarantine).context("validate quarantined rolling trees")?;
    remove_quarantined_tree(&quarantine)?;
    output.sync_all().context("sync rolling quarantine removal")
}

fn snapshot_tree(path: &Path, device: u64) -> Result<Metadata> {
    let stamp = fs::symlink_metadata(path)?;
    ensure!(
        stamp.is_dir() && stamp.dev() == device,
        "rolling retirement tree must be a direct directory on the output filesystem"
    );
    tree_bytes(path)?;
    verify_directory(path, &stamp)?;
    Ok(stamp)
}

fn open_directory(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open rolling retirement directory {}", path.display()))
}

fn fd_path(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn verify_directory(path: &Path, stamp: &Metadata) -> Result<()> {
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.is_dir() && current.dev() == stamp.dev() && current.ino() == stamp.ino(),
        "rolling retirement directory was replaced: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn checkpoint_replacement_requires_the_previous_summary() -> Result<()> {
        let root = tempfile::tempdir()?;
        let output = open_directory(root.path())?;
        let initial = RetiredPrefix::default();
        let first = RetiredPrefix {
            retired_segments: 1,
            ..RetiredPrefix::default()
        };
        let second = RetiredPrefix {
            retired_segments: 2,
            ..RetiredPrefix::default()
        };
        initial.verify(root.path())?;
        initial.publish_checkpoint(&first, &output)?;
        assert!(initial.publish_checkpoint(&second, &output).is_err());
        first.verify(root.path())?;
        first.publish_checkpoint(&second, &output)?;
        assert!(first.verify(root.path()).is_err());
        second.verify(root.path())?;
        Ok(())
    }

    #[test]
    fn retirement_rejects_symlinks_and_preserves_failed_quarantine() -> Result<()> {
        let root = tempfile::tempdir()?;
        let output = open_directory(root.path())?;
        let source = root.path().join("source");
        let segment = source.join("000001");
        fs::create_dir_all(&segment)?;
        let parent = open_directory(&source)?;
        let stamp = snapshot_tree(&segment, output.metadata()?.dev())?;
        symlink(root.path(), segment.join("link"))?;
        assert!(snapshot_tree(&segment, output.metadata()?.dev()).is_err());
        assert!(
            prune_trees(
                &output,
                "000001",
                [(&parent, &stamp, "capture"), (&parent, &stamp, "reports")],
            )
            .is_err()
        );
        let quarantines: Vec<_> = fs::read_dir(root.path())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".retire-"))
            .collect();
        assert_eq!(quarantines.len(), 1);
        assert!(quarantines[0].path().join("capture/link").is_symlink());
        Ok(())
    }
}
