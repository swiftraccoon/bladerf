//! Plan and replay selected protocols from one admitted capture.

pub(crate) mod preflight;
pub(super) mod shared;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::MAX_SAMPLES;
use crate::dmr::raster::Bounds as DmrBounds;
use crate::lora::bank::Profile;
use crate::lte::raster::{DEFAULT_ANALYSIS_SAMPLES, MAX_PREFIX_SAMPLES};
use crate::morse::Profile as MorseProfile;
use crate::nr::{self, Profile as NrProfile};
use crate::ook::DEFAULT_RETAINED_TRANSMISSIONS;
use crate::private_output::publish_json_file;
use crate::rs41::raster::Bounds as Rs41Bounds;
use crate::signal_profile::{self, LteRange, SignalProfile};
use crate::{ais, dect, dmr, efergy, halow, rs41, scm, tpms, uat, wh51, ws90};

use super::admission::CompletedCapture;
use super::channel_sweep::{self, Protocol};
use super::{
    adsb_sweep, aprs_sweep, check_stop, dmr_raster, lora_sweep, lte_sweep, morse_sweep, nr_sweep,
    ook_sweep, rds_sweep, rs41_raster,
    sensor_sweep::{self, Sensor},
};

const _: () = assert!(aprs_sweep::MAX_TOTAL_SAMPLES == adsb_sweep::MAX_TOTAL_SAMPLES);
const _: () = assert!(aprs_sweep::MAX_TOTAL_SAMPLES == ook_sweep::MAX_TOTAL_SAMPLES);
const _: () = assert!(aprs_sweep::MAX_TOTAL_SAMPLES == sensor_sweep::MAX_TOTAL_SAMPLES);
const _: () =
    assert!(lora_sweep::MAX_TOTAL_PROFILE_SAMPLES == morse_sweep::MAX_TOTAL_PROFILE_SAMPLES);
const IQ_SAMPLE_BYTES: u64 = 4;

/// Select which protocol analyzers to run on a completed capture.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Directory containing a completed sweep or sweep series.
    capture_directory: PathBuf,
    /// Read the capture as a repeated sweep series.
    #[arg(long)]
    series: bool,
    /// Versioned decoder selections shared with sweep capture planning.
    #[arg(long, conflicts_with_all = ["aprs_channel_hz", "ais_channel_hz", "dmr_channel_hz", "dmr_raster_hz", "dect_channel_hz", "halow_channel_hz", "scm_channel_hz", "scm_plus_channel_hz", "adsb", "uat", "lora_profile",
        "lte_range_hz", "lte_pbch", "nr_profile", "nr_raster", "nr_raster_hz",
        "nr_scs_hz", "nr_pbch", "rds_channel_hz", "rds_raster_hz", "morse_profile",
        "ook_channel_hz", "efergy_channel_hz", "wh51_channel_hz", "ws90_channel_hz", "tpms_channel_hz",
        "rs41_channel_hz", "rs41_raster_hz"])]
    signal_profile: Option<PathBuf>,
    #[command(flatten)]
    selection: Selection,
    #[command(flatten)]
    limits: ReplayLimits,
    /// Reuse common IQ prefixes in memory across compatible dwell routes.
    #[arg(long)]
    share_iq: bool,
    /// Print the validated replay plan to stdout without reading IQ payloads.
    #[arg(long)]
    plan_only: bool,
    /// New private JSON report file in an existing directory.
    #[arg(
        long,
        required_unless_present = "plan_only",
        conflicts_with = "plan_only"
    )]
    output: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct Selection {
    /// APRS channel in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    aprs_channel_hz: Vec<u64>,
    /// AIS channel center in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    ais_channel_hz: Vec<u64>,
    /// DMR channel center in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    dmr_channel_hz: Vec<u64>,
    /// Inclusive DMR raster limits in hertz, anchored at the first channel.
    #[arg(long, value_name = "LOWER:UPPER", value_parser = DmrBounds::parse)]
    dmr_raster_hz: Option<DmrBounds>,
    /// DECT channel center in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    dect_channel_hz: Vec<u64>,
    /// Wi-Fi HaLow 2 MHz channel center in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    halow_channel_hz: Vec<u64>,
    /// Legacy SCM carrier in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    scm_channel_hz: Vec<u64>,
    /// SCM+ carrier in hertz. Repeat for additional channels.
    #[arg(long, value_parser = clap::value_parser!(u64).range(crate::MIN_CENTER_HZ..=crate::MAX_CENTER_HZ))]
    scm_plus_channel_hz: Vec<u64>,
    /// Decode extended squitters on the 1090 MHz channel.
    #[arg(long)]
    adsb: bool,
    /// Decode airborne frames on the 978 MHz UAT channel.
    #[arg(long)]
    uat: bool,
    /// LoRa hypothesis as CENTER_HZ:BANDWIDTH_HZ:SF. Repeat for additional profiles.
    #[arg(long, value_name = "CENTER_HZ:BANDWIDTH_HZ:SF", value_parser = Profile::parse)]
    lora_profile: Vec<Profile>,
    /// First and last LTE carrier candidates in hertz, inclusive, aligned to 100 kHz.
    #[arg(long, value_name = "LOWER:UPPER", value_parser = LteRange::parse)]
    lte_range_hz: Option<LteRange>,
    /// Verify public LTE broadcast information for the selected LTE carriers.
    #[arg(long, requires = "lte_range_hz")]
    lte_pbch: bool,
    /// NR synchronization profile as SS_REF_HZ:SCS_KHZ. Repeat for additional profiles.
    #[arg(long, conflicts_with = "nr_raster", value_name = "SS_REF_HZ:SCS_KHZ", value_parser = NrProfile::parse)]
    nr_profile: Vec<NrProfile>,
    #[command(flatten)]
    nr_raster: nr_sweep::RasterArgs,
    /// Verify public NR broadcast blocks with the selected NR profiles.
    #[arg(long)]
    nr_pbch: bool,
    /// Broadcast FM station center in hertz. Repeat for additional stations.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    rds_channel_hz: Vec<u64>,
    /// First station, last station, and step in hertz. Both endpoints are included.
    #[arg(long, conflicts_with = "rds_channel_hz", value_name = "LOWER:UPPER:STEP",
        value_parser = rds_sweep::StationRaster::parse)]
    rds_raster_hz: Option<rds_sweep::StationRaster>,
    /// Morse hypothesis as `cw:CHANNEL_HZ[:WPM]` or `nfm:CHANNEL_HZ:TONE_HZ[:WPM]`.
    #[arg(long, value_name = "PROFILE", value_parser = MorseProfile::parse)]
    morse_profile: Vec<MorseProfile>,
    /// RF channel in hertz for the supported OOK sensor formats.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    ook_channel_hz: Option<u64>,
    /// RF channel in hertz for the Efergy e2 FSK format.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    efergy_channel_hz: Option<u64>,
    /// RF channel in hertz for the WH51 FSK format.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    wh51_channel_hz: Option<u64>,
    /// RF channel in hertz for the WS90 FSK format.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    ws90_channel_hz: Option<u64>,
    /// RF channel in hertz for the supported Pacific TPMS formats.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    tpms_channel_hz: Option<u64>,
    /// RF channel in hertz for Vaisala RS41 radiosondes.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    rs41_channel_hz: Option<u64>,
    /// Inclusive RS41 raster limits in hertz, on the 12.5 kHz grid.
    #[arg(long, value_name = "LOWER:UPPER", value_parser = Rs41Bounds::parse)]
    rs41_raster_hz: Option<Rs41Bounds>,
}

impl Selection {
    fn from_profile(profile: SignalProfile) -> (Self, String) {
        let SignalProfile {
            sha256,
            aprs_channel_hz,
            ais_channel_hz,
            dmr_channel_hz,
            dmr_raster_hz,
            dect_channel_hz,
            halow_channel_hz,
            scm_channel_hz,
            scm_plus_channel_hz,
            adsb,
            uat,
            lora_profile,
            lte_range_hz,
            lte_pbch,
            nr_profile,
            nr_raster_hz,
            nr_scs_hz,
            nr_pbch,
            rds_channel_hz,
            morse_profile,
            ook_channel_hz,
            efergy_channel_hz,
            wh51_channel_hz,
            ws90_channel_hz,
            tpms_channel_hz,
            rs41_channel_hz,
            rs41_raster_hz,
            ..
        } = profile;
        (
            Self {
                aprs_channel_hz,
                ais_channel_hz,
                dmr_channel_hz,
                dmr_raster_hz,
                dect_channel_hz,
                halow_channel_hz,
                scm_channel_hz,
                scm_plus_channel_hz,
                adsb,
                uat,
                lora_profile,
                lte_range_hz,
                lte_pbch,
                nr_profile,
                nr_raster: nr_sweep::RasterArgs::from_profile(nr_raster_hz, nr_scs_hz),
                nr_pbch,
                rds_channel_hz,
                rds_raster_hz: None,
                morse_profile,
                ook_channel_hz,
                efergy_channel_hz,
                wh51_channel_hz,
                ws90_channel_hz,
                tpms_channel_hz,
                rs41_channel_hz,
                rs41_raster_hz,
            },
            sha256,
        )
    }
}

/// Prefix and work limits shared by saved replay and capture followed by replay.
#[derive(Debug, Clone, Copy, Eq, PartialEq, clap::Args)]
pub(crate) struct ReplayLimits {
    /// Maximum original-rate samples per dwell. Uses each analyzer's default when omitted.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_SAMPLES))]
    pub(crate) max_samples_per_dwell: Option<u64>,
    /// Total original-rate samples allowed per analyzer for IQ channels and raster sources.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=aprs_sweep::MAX_TOTAL_SAMPLES))]
    pub(crate) max_total_samples: u64,
    /// Total original-rate samples allowed separately for LoRa and Morse across eligible profiles.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=lora_sweep::MAX_TOTAL_PROFILE_SAMPLES))]
    pub(crate) max_total_profile_samples: u64,
    /// Total channelized samples allowed independently for LTE, NR, DMR raster, and RS41 raster replay.
    #[arg(long, default_value_t = lte_sweep::DEFAULT_TOTAL_CHANNELIZED_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=lte_sweep::MAX_TOTAL_CHANNELIZED_SAMPLES))]
    pub(crate) max_total_channelized_samples: u64,
    /// Total original-rate samples charged once per eligible RDS station.
    #[arg(long, default_value_t = MAX_SAMPLES, value_parser = clap::value_parser!(u64).range(1..=rds_sweep::MAX_TOTAL_STATION_SAMPLES))]
    pub(crate) max_total_station_samples: u64,
}

impl Default for ReplayLimits {
    fn default() -> Self {
        Self {
            max_samples_per_dwell: None,
            max_total_samples: MAX_SAMPLES,
            max_total_profile_samples: MAX_SAMPLES,
            max_total_channelized_samples: lte_sweep::DEFAULT_TOTAL_CHANNELIZED_SAMPLES,
            max_total_station_samples: MAX_SAMPLES,
        }
    }
}

impl ReplayLimits {
    /// Original-rate prefix limit for analyzers with the general sample cap.
    pub(crate) fn general_prefix(&self) -> u64 {
        self.max_samples_per_dwell.unwrap_or(MAX_SAMPLES)
    }

    /// Original-rate LTE prefix limit after applying the LTE default and cap.
    pub(crate) fn lte_prefix(&self) -> u64 {
        self.max_samples_per_dwell
            .unwrap_or(DEFAULT_ANALYSIS_SAMPLES)
            .min(MAX_PREFIX_SAMPLES)
    }

    /// Original-rate NR prefix limit after applying the NR cap.
    pub(crate) fn nr_prefix(&self) -> u64 {
        self.general_prefix().min(nr::MAX_PREFIX_SAMPLES)
    }
}

/// Unit used to charge a protocol's replay budget.
#[derive(Clone, Copy, Debug, Serialize)]
pub(super) enum BudgetUnit {
    #[serde(rename = "iq_samples")]
    Iq,
    #[serde(rename = "profile_samples")]
    Profile,
    #[serde(rename = "station_samples")]
    Station,
    #[serde(rename = "channelized_samples")]
    Channelized,
}

/// Work derived from the same routes that will be replayed.
#[derive(Clone, Copy, Serialize)]
pub(super) struct Work {
    pub(super) replayed_iq_samples: u64,
    pub(super) eligible_attempts: usize,
    pub(super) short_attempts: usize,
    pub(super) budget_samples: u64,
    pub(super) max_budget_samples: u64,
    pub(super) budget_unit: BudgetUnit,
}

#[derive(Serialize)]
struct AnalyzerWork {
    analyzer: &'static str,
    #[serde(flatten)]
    work: Work,
}

#[derive(Serialize)]
struct CaptureScope<'a> {
    capture_kind: &'static str,
    source_capture_sha256: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_continuity_checked: Option<bool>,
    pass_count: usize,
    dwell_count: usize,
}

#[derive(Serialize)]
struct Preview<'a> {
    schema_version: u32,
    operation: &'static str,
    #[serde(flatten)]
    source: CaptureScope<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<String>,
    total_replayed_iq_samples: u64,
    total_replayed_iq_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    shared_iq: Option<shared::Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lte_pbch: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nr_pbch_max_observations_per_profile: Option<usize>,
    analyzers: Vec<AnalyzerWork>,
}

impl Preview<'_> {
    fn print(&self) -> Result<()> {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer_pretty(&mut stdout, self).context("write sweep replay plan")?;
        writeln!(stdout)?;
        stdout.flush().context("flush sweep replay plan")
    }
}

#[derive(Serialize)]
struct Report<'a> {
    schema_version: u32,
    operation: &'static str,
    #[serde(flatten)]
    source: CaptureScope<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal_profile_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shared_iq: Option<shared::Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aprs: Option<aprs_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ais: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dmr: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dmr_raster: Option<dmr_raster::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dect: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    halow: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scm: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scm_plus: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adsb: Option<adsb_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uat: Option<channel_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lora: Option<lora_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lte: Option<lte_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nr: Option<nr_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rds: Option<rds_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    morse: Option<morse_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ook: Option<ook_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    efergy: Option<sensor_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wh51: Option<sensor_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ws90: Option<sensor_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tpms: Option<sensor_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rs41: Option<sensor_sweep::Report<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rs41_raster: Option<rs41_raster::Report<'a>>,
}

/// Run selected offline analyzers and publish one source-bound report.
///
/// All selected analyzers check coverage and their replay budgets before any
/// reads of IQ payloads. A plan-only run prints the work without decoding.
/// All selected analyzers must finish before report publication.
///
/// # Errors
/// Fails for missing selections, invalid or changed inputs, exceeded budgets,
/// analysis errors, interruption or report publication failure.
pub(crate) fn run(root: &Path, mut args: Args, stop: &AtomicBool) -> Result<()> {
    check_stop(stop)?;
    let signal_profile_sha256 = if let Some(path) = args.signal_profile.take() {
        let profile = signal_profile::read(&path)?;
        ensure!(
            profile.lora_packet_profile.is_empty(),
            "LoRa packet profiles require decode-lora-sweep --signal-profile or sweep --replay"
        );
        let (selection, sha256) = Selection::from_profile(profile);
        args.selection = selection;
        Some(sha256)
    } else {
        args.selection.rds_channel_hz = rds_sweep::select_stations(
            std::mem::take(&mut args.selection.rds_channel_hz),
            args.selection.rds_raster_hz.take(),
        )?;
        None
    };
    args.capture_directory = root.join(args.capture_directory);
    args.output = args.output.map(|output| root.join(output));
    run_resolved(args, signal_profile_sha256, stop)
}

/// Replay a completed capture using the same admitted profile used to plan it.
///
/// The existing sweep or series receipt binds the saved IQ. The private
/// replay receipt is published as `replay.json` in the capture directory.
///
/// # Errors
/// Returns an error for invalid or changed capture inputs, exceeded budgets,
/// analysis failure, interruption or replay receipt publication failure.
pub(crate) fn run_profile(
    capture_path: &Path,
    series: bool,
    profile: SignalProfile,
    limits: ReplayLimits,
    stop: &AtomicBool,
) -> Result<()> {
    let (selection, sha256) = Selection::from_profile(profile);
    let args = Args {
        capture_directory: capture_path.to_path_buf(),
        series,
        signal_profile: None,
        selection,
        limits,
        share_iq: true,
        plan_only: false,
        output: Some(capture_path.join("replay.json")),
    };
    run_resolved(args, Some(sha256), stop)
}

fn run_resolved(
    args: Args,
    signal_profile_sha256: Option<String>,
    stop: &AtomicBool,
) -> Result<()> {
    check_stop(stop)?;
    let selection = args.selection;
    let limits = args.limits;
    ensure!(
        !selection.nr_pbch || !selection.nr_profile.is_empty() || selection.nr_raster.nr_raster,
        "--nr-pbch requires --nr-profile or --nr-raster"
    );
    let count = [
        !selection.aprs_channel_hz.is_empty(),
        !selection.ais_channel_hz.is_empty(),
        !selection.dmr_channel_hz.is_empty(),
        selection.dmr_raster_hz.is_some(),
        !selection.dect_channel_hz.is_empty(),
        !selection.halow_channel_hz.is_empty(),
        !selection.scm_channel_hz.is_empty(),
        !selection.scm_plus_channel_hz.is_empty(),
        selection.adsb,
        selection.uat,
        !selection.lora_profile.is_empty(),
        selection.lte_range_hz.is_some(),
        !selection.nr_profile.is_empty() || selection.nr_raster.nr_raster,
        !selection.rds_channel_hz.is_empty(),
        !selection.morse_profile.is_empty(),
        selection.ook_channel_hz.is_some(),
        selection.efergy_channel_hz.is_some(),
        selection.wh51_channel_hz.is_some(),
        selection.ws90_channel_hz.is_some(),
        selection.tpms_channel_hz.is_some(),
        selection.rs41_channel_hz.is_some(),
        selection.rs41_raster_hz.is_some(),
    ]
    .into_iter()
    .filter(|&selected| selected)
    .count();
    ensure!(count > 0, "select at least one replay analyzer");

    let capture = CompletedCapture::open(&args.capture_directory, args.series)?;
    let passes = capture.passes();
    let source = CaptureScope {
        capture_kind: capture.kind(),
        source_capture_sha256: capture.source_receipt_sha256(),
        sample_continuity_checked: capture.continuity_checked().then_some(true),
        pass_count: passes.len(),
        dwell_count: passes.iter().map(|pass| pass.sweep.tunes.len()).sum(),
    };
    let max_samples_per_dwell = limits.general_prefix();
    let mut aprs = (!selection.aprs_channel_hz.is_empty())
        .then(|| {
            aprs_sweep::prepare(
                &capture,
                &selection.aprs_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                stop,
            )
        })
        .transpose()?;
    let mut ais = (!selection.ais_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Ais,
                &selection.ais_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                ais::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut dmr = (!selection.dmr_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Dmr,
                &selection.dmr_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                dmr::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut dmr_raster = selection
        .dmr_raster_hz
        .map(|bounds| {
            dmr_raster::Plan::prepare(
                &capture,
                bounds,
                dmr_raster::Limits {
                    max_samples_per_dwell,
                    max_total_samples: limits.max_total_samples,
                    max_channelized_samples: limits.max_total_channelized_samples,
                    max_frames: dmr::DEFAULT_RETAINED_FRAMES,
                },
                stop,
            )
        })
        .transpose()?;
    let mut dect = (!selection.dect_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Dect,
                &selection.dect_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                dect::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut halow = (!selection.halow_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Halow,
                &selection.halow_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                halow::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut scm = (!selection.scm_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Scm,
                &selection.scm_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                scm::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut scm_plus = (!selection.scm_plus_channel_hz.is_empty())
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::ScmPlus,
                &selection.scm_plus_channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                scm::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut adsb = selection
        .adsb
        .then(|| {
            adsb_sweep::prepare(
                &capture,
                max_samples_per_dwell,
                limits.max_total_samples,
                stop,
            )
        })
        .transpose()?;
    let mut uat = selection
        .uat
        .then(|| {
            channel_sweep::prepare(
                &capture,
                Protocol::Uat,
                &[uat::CHANNEL_HZ],
                max_samples_per_dwell,
                limits.max_total_samples,
                uat::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut lora = (!selection.lora_profile.is_empty())
        .then(|| {
            lora_sweep::prepare(
                &capture,
                &selection.lora_profile,
                max_samples_per_dwell,
                limits.max_total_profile_samples,
                stop,
            )
        })
        .transpose()?;
    let mut lte = selection
        .lte_range_hz
        .map(|range| {
            lte_sweep::prepare(
                &capture,
                range.lower_hz,
                range.upper_hz,
                limits.lte_prefix(),
                limits.max_total_channelized_samples,
                stop,
            )
            .map(|plan| plan.with_pbch(selection.lte_pbch))
        })
        .transpose()?;
    let mut rds = (!selection.rds_channel_hz.is_empty())
        .then(|| {
            rds_sweep::prepare(
                &capture,
                selection.rds_channel_hz,
                max_samples_per_dwell,
                limits.max_total_station_samples,
                stop,
            )
        })
        .transpose()?;
    let nr_prefix = limits.nr_prefix();
    let mut nr = if selection.nr_raster.nr_raster {
        Some(nr_sweep::prepare_raster(
            &capture,
            &selection.nr_raster,
            nr_prefix,
            limits.max_total_channelized_samples,
            stop,
        )?)
    } else {
        (!selection.nr_profile.is_empty())
            .then(|| {
                nr_sweep::prepare(
                    &capture,
                    &selection.nr_profile,
                    nr_prefix,
                    limits.max_total_channelized_samples,
                    stop,
                )
            })
            .transpose()?
    }
    .map(|plan| plan.with_pbch(selection.nr_pbch));
    let mut morse = (!selection.morse_profile.is_empty())
        .then(|| {
            morse_sweep::prepare(
                &capture,
                &selection.morse_profile,
                max_samples_per_dwell,
                limits.max_total_profile_samples,
                stop,
            )
        })
        .transpose()?;
    let mut ook = selection
        .ook_channel_hz
        .map(|channel_hz| {
            ook_sweep::prepare(
                &capture,
                channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                DEFAULT_RETAINED_TRANSMISSIONS,
                stop,
            )
        })
        .transpose()?;
    let mut efergy = selection
        .efergy_channel_hz
        .map(|channel| {
            sensor_sweep::prepare(
                &capture,
                sensor_sweep::Sensor::Efergy,
                channel,
                max_samples_per_dwell,
                limits.max_total_samples,
                efergy::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut wh51 = selection
        .wh51_channel_hz
        .map(|channel_hz| {
            sensor_sweep::prepare(
                &capture,
                Sensor::Wh51,
                channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                wh51::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut ws90 = selection
        .ws90_channel_hz
        .map(|channel_hz| {
            sensor_sweep::prepare(
                &capture,
                Sensor::Ws90,
                channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                ws90::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut tpms = selection
        .tpms_channel_hz
        .map(|channel_hz| {
            sensor_sweep::prepare(
                &capture,
                Sensor::Tpms,
                channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                tpms::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut rs41 = selection
        .rs41_channel_hz
        .map(|channel_hz| {
            sensor_sweep::prepare(
                &capture,
                Sensor::Rs41,
                channel_hz,
                max_samples_per_dwell,
                limits.max_total_samples,
                rs41::DEFAULT_RETAINED_FRAMES,
                stop,
            )
        })
        .transpose()?;
    let mut rs41_raster = selection
        .rs41_raster_hz
        .map(|bounds| {
            rs41_raster::Plan::prepare(
                &capture,
                bounds,
                rs41_raster::Limits {
                    max_samples_per_dwell,
                    max_total_samples: Some(limits.max_total_samples),
                    max_channelized_samples: limits.max_total_channelized_samples,
                    max_frames: rs41::DEFAULT_RETAINED_FRAMES,
                },
                stop,
            )
        })
        .transpose()?;
    let mut shared_plan = args.share_iq.then(shared::Plan::default);
    let shared_iq = if let Some(plan) = &mut shared_plan {
        plan.add(aprs.iter().flat_map(aprs_sweep::Plan::requests))?;
        plan.add_streaming(ais.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add_streaming(dmr.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add(dmr_raster.iter().flat_map(dmr_raster::Plan::requests))?;
        plan.add_streaming(dect.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add_streaming(halow.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add_streaming(scm.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add_streaming(scm_plus.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add(adsb.iter().flat_map(adsb_sweep::Plan::requests))?;
        plan.add_streaming(uat.iter().flat_map(channel_sweep::Plan::requests))?;
        plan.add(lora.iter().flat_map(lora_sweep::Plan::requests))?;
        plan.add(lte.iter().flat_map(lte_sweep::Plan::requests))?;
        plan.add(nr.iter().flat_map(nr_sweep::Plan::requests))?;
        plan.add(rds.iter().flat_map(rds_sweep::Plan::requests))?;
        plan.add(morse.iter().flat_map(morse_sweep::Plan::requests))?;
        plan.add_streaming(ook.iter().flat_map(ook_sweep::Plan::requests))?;
        plan.add_streaming(efergy.iter().flat_map(sensor_sweep::Plan::requests))?;
        plan.add(wh51.iter().flat_map(sensor_sweep::Plan::requests))?;
        plan.add(ws90.iter().flat_map(sensor_sweep::Plan::requests))?;
        plan.add(tpms.iter().flat_map(sensor_sweep::Plan::requests))?;
        plan.add(rs41.iter().flat_map(sensor_sweep::Plan::requests))?;
        plan.add(rs41_raster.iter().flat_map(rs41_raster::Plan::requests))?;
        Some(plan.prepare())
    } else {
        None
    };
    if args.plan_only {
        let analyzers: Vec<_> = [
            ("aprs", aprs.as_ref().map(aprs_sweep::Plan::work)),
            ("ais", ais.as_ref().map(channel_sweep::Plan::work)),
            ("dmr", dmr.as_ref().map(channel_sweep::Plan::work)),
            (
                "dmr_raster",
                dmr_raster.as_ref().map(dmr_raster::Plan::work),
            ),
            (
                "dmr_raster_source",
                dmr_raster.as_ref().map(dmr_raster::Plan::source_work),
            ),
            ("dect", dect.as_ref().map(channel_sweep::Plan::work)),
            ("halow", halow.as_ref().map(channel_sweep::Plan::work)),
            ("scm", scm.as_ref().map(channel_sweep::Plan::work)),
            ("scm_plus", scm_plus.as_ref().map(channel_sweep::Plan::work)),
            ("adsb", adsb.as_ref().map(adsb_sweep::Plan::work)),
            ("uat", uat.as_ref().map(channel_sweep::Plan::work)),
            ("lora", lora.as_ref().map(lora_sweep::Plan::work)),
            ("lte", lte.as_ref().map(lte_sweep::Plan::work)),
            ("nr", nr.as_ref().map(nr_sweep::Plan::work)),
            ("rds", rds.as_ref().map(rds_sweep::Plan::work)),
            ("morse", morse.as_ref().map(morse_sweep::Plan::work)),
            ("ook", ook.as_ref().map(ook_sweep::Plan::work)),
            ("efergy", efergy.as_ref().map(sensor_sweep::Plan::work)),
            ("wh51", wh51.as_ref().map(sensor_sweep::Plan::work)),
            ("ws90", ws90.as_ref().map(sensor_sweep::Plan::work)),
            ("tpms", tpms.as_ref().map(sensor_sweep::Plan::work)),
            ("rs41", rs41.as_ref().map(sensor_sweep::Plan::work)),
            (
                "rs41_raster",
                rs41_raster.as_ref().map(rs41_raster::Plan::work),
            ),
            (
                "rs41_raster_source",
                rs41_raster.as_ref().map(rs41_raster::Plan::source_work),
            ),
        ]
        .into_iter()
        .filter_map(|(analyzer, work)| work.map(|work| AnalyzerWork { analyzer, work }))
        .collect();
        let total_replayed_iq_samples = analyzers.iter().map(|a| a.work.replayed_iq_samples).sum();
        capture.verify()?;
        check_stop(stop)?;
        return Preview {
            schema_version: 1,
            operation: "plan_saved_sweep_replay",
            source,
            signal_profile_sha256,
            total_replayed_iq_samples,
            total_replayed_iq_bytes: total_replayed_iq_samples * IQ_SAMPLE_BYTES,
            shared_iq,
            lte_pbch: selection.lte_pbch.then_some(true),
            nr_pbch_max_observations_per_profile: selection
                .nr_pbch
                .then_some(nr::PBCH_MAX_OBSERVATIONS),
            analyzers,
        }
        .print();
    }
    if let Some(plan) = shared_plan {
        let mut tasks: Vec<_> = [
            aprs.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            dmr_raster
                .as_mut()
                .map(|task| task as &mut dyn shared::Task<'_>),
            adsb.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            lora.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            lte.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            nr.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            rds.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            morse.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            wh51.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            ws90.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            tpms.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            rs41.as_mut().map(|task| task as &mut dyn shared::Task<'_>),
            rs41_raster
                .as_mut()
                .map(|task| task as &mut dyn shared::Task<'_>),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut streamed: Vec<&mut dyn shared::StreamingTask<'_>> = [
            ais.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            dmr.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            dect.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            halow
                .as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            scm.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            scm_plus
                .as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            uat.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            ook.as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
            efergy
                .as_mut()
                .map(|task| task as &mut dyn shared::StreamingTask<'_>),
        ]
        .into_iter()
        .flatten()
        .collect();
        plan.replay(&mut tasks, &mut streamed, stop)?;
    }
    // In shared mode the task cursors are exhausted and only report assembly
    // remains. Unshared plans stream their pending routes here.
    let report = Report {
        schema_version: 1,
        operation: "replay_saved_sweep",
        source,
        signal_profile_sha256,
        shared_iq,
        aprs: aprs.map(|plan| plan.replay(stop)).transpose()?,
        ais: ais.map(|plan| plan.replay(stop)).transpose()?,
        dmr: dmr.map(|plan| plan.replay(stop)).transpose()?,
        dmr_raster: dmr_raster.map(|plan| plan.replay(stop)).transpose()?,
        dect: dect.map(|plan| plan.replay(stop)).transpose()?,
        halow: halow.map(|plan| plan.replay(stop)).transpose()?,
        scm: scm.map(|plan| plan.replay(stop)).transpose()?,
        scm_plus: scm_plus.map(|plan| plan.replay(stop)).transpose()?,
        adsb: adsb.map(|plan| plan.replay(stop)).transpose()?,
        uat: uat.map(|plan| plan.replay(stop)).transpose()?,
        lora: lora.map(|plan| plan.replay(stop)).transpose()?,
        lte: lte.map(|plan| plan.replay(stop)).transpose()?,
        nr: nr.map(|plan| plan.replay(stop)).transpose()?,
        rds: rds.map(|plan| plan.replay(stop)).transpose()?,
        morse: morse.map(|plan| plan.replay(stop)).transpose()?,
        ook: ook.map(|plan| plan.replay(stop)).transpose()?,
        efergy: efergy.map(|plan| plan.replay(stop)).transpose()?,
        wh51: wh51.map(|plan| plan.replay(stop)).transpose()?,
        ws90: ws90.map(|plan| plan.replay(stop)).transpose()?,
        tpms: tpms.map(|plan| plan.replay(stop)).transpose()?,
        rs41: rs41.map(|plan| plan.replay(stop)).transpose()?,
        rs41_raster: rs41_raster.map(|plan| plan.replay(stop)).transpose()?,
    };
    let output = args.output.context("supply --output for sweep replay")?;
    publish_json_file(&output, &report, || {
        capture.verify()?;
        check_stop(stop)
    })?;
    if let Some(aprs) = &report.aprs {
        println!("APRS validated {} AX.25 frames.", aprs.validated_frames());
    }
    if let Some(ais) = &report.ais {
        println!("Validated {} AIS frames.", ais.validated_frames());
    }
    if let Some(dmr) = &report.dmr {
        println!(
            "DMR validated {} Voice Link Control frames.",
            dmr.validated_frames()
        );
    }
    if let Some(dmr_raster) = &report.dmr_raster {
        println!(
            "Validated {} DMR raster frames.",
            dmr_raster.validated_frames()
        );
    }
    if let Some(dect) = &report.dect {
        println!(
            "DECT validated {} fixed-part identity frames.",
            dect.validated_frames()
        );
    }
    if let Some(halow) = &report.halow {
        println!(
            "Wi-Fi HaLow validated {} S1G beacons.",
            halow.validated_frames()
        );
    }
    if let Some(scm) = &report.scm {
        println!("SCM validated {} endpoint frames.", scm.validated_frames());
    }
    if let Some(scm_plus) = &report.scm_plus {
        println!(
            "SCM+ validated {} endpoint frames.",
            scm_plus.validated_frames()
        );
    }
    if let Some(adsb) = &report.adsb {
        println!(
            "ADS-B validated {} extended squitters.",
            adsb.validated_frames()
        );
    }
    if let Some(uat) = &report.uat {
        println!("Validated {} UAT airborne frames.", uat.validated_frames());
    }
    if let Some(lora) = &report.lora {
        println!("LoRa found {} preamble candidates.", lora.candidate_count);
    }
    if let Some(lte) = &report.lte {
        println!("LTE recorded {} cell reports.", lte.cell_reports);
        if let Some(count) = lte.pbch_cell_reports {
            println!("Validated public LTE broadcast information for {count} cell reports.");
        }
    }
    if let Some(nr) = &report.nr {
        println!("NR recorded {} repeated cell reports.", nr.cell_reports);
        if let Some(count) = nr.pbch_reports {
            println!("Validated {count} NR PBCH observations.");
        }
    }
    if let Some(rds) = &report.rds {
        println!("RDS validated {} broadcast groups.", rds.validated_groups());
    }
    if let Some(morse) = &report.morse {
        println!("Resolved {} Morse characters.", morse.resolved_characters());
    }
    if let Some(ook) = &report.ook {
        println!(
            "Observed {} OOK transmissions.",
            ook.observed_transmissions()
        );
    }
    if let Some(efergy) = &report.efergy {
        println!("Validated {} Efergy frames.", efergy.validated_frames());
    }
    if let Some(wh51) = &report.wh51 {
        println!("Validated {} WH51 frames.", wh51.validated_frames());
    }
    if let Some(ws90) = &report.ws90 {
        println!("Validated {} WS90 frames.", ws90.validated_frames());
    }
    if let Some(tpms) = &report.tpms {
        println!("Validated {} TPMS frames.", tpms.validated_frames());
    }
    if let Some(rs41) = &report.rs41 {
        println!("Validated {} RS41 frames.", rs41.validated_frames());
    }
    if let Some(rs41_raster) = &report.rs41_raster {
        println!(
            "Validated {} RS41 raster frames.",
            rs41_raster.validated_frames()
        );
    }
    println!(
        "Completed {count} sweep analyzers. Output: {}",
        output.display()
    );
    Ok(())
}
