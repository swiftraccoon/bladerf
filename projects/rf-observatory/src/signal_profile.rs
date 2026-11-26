//! Admit a bounded set of reusable protocol selections from a JSON file.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::lora::bank::Profile as LoraProfile;
use crate::lora::decode::{PacketInterpretations, PacketProfile};
use crate::lte::raster;
use crate::morse::Profile as MorseProfile;
use crate::nr::{self, Profile as NrProfile, raster::Bounds as NrBounds};
use crate::rs41::raster::Bounds as Rs41Bounds;
use crate::triage::channel_sweep::Protocol;
use crate::triage::{aprs_sweep, lora_decode_sweep, lora_sweep, morse_sweep, rds_sweep};
use crate::{dmr::raster::Bounds as DmrBounds, file_pin};

const MAX_PROFILE_BYTES: u64 = 64 * 1024;

/// Inclusive carrier bounds on the 100 kHz LTE raster.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LteRange {
    pub(crate) lower_hz: u64,
    pub(crate) upper_hz: u64,
}

impl LteRange {
    /// Parse `LOWER:UPPER` in hertz using the LTE raster endpoint constraints.
    ///
    /// # Errors
    /// Returns an error for malformed, reversed or unsupported endpoints.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER:UPPER in hertz")?;
        let lower_hz = lower
            .parse()
            .context("invalid LTE lower carrier in hertz")?;
        let upper_hz = upper
            .parse()
            .context("invalid LTE upper carrier in hertz")?;
        raster::validate_endpoints(lower_hz, upper_hz)?;
        Ok(Self { lower_hz, upper_hz })
    }
}

/// Validated protocol selections and the digest of their original JSON bytes.
#[derive(Debug)]
pub(crate) struct SignalProfile {
    pub(crate) sha256: String,
    pub(crate) aprs_channel_hz: Vec<u64>,
    pub(crate) ais_channel_hz: Vec<u64>,
    pub(crate) dmr_channel_hz: Vec<u64>,
    pub(crate) dmr_raster_hz: Option<DmrBounds>,
    pub(crate) dect_channel_hz: Vec<u64>,
    pub(crate) halow_channel_hz: Vec<u64>,
    pub(crate) scm_channel_hz: Vec<u64>,
    pub(crate) scm_plus_channel_hz: Vec<u64>,
    pub(crate) adsb: bool,
    pub(crate) uat: bool,
    pub(crate) lora_profile: Vec<LoraProfile>,
    pub(crate) lora_packet_profile: Vec<PacketProfile>,
    /// Interpret retained LoRa packets according to their selected sync words.
    pub(crate) packet_interpretations: PacketInterpretations,
    pub(crate) lte_range_hz: Option<LteRange>,
    pub(crate) lte_pbch: bool,
    pub(crate) nr_profile: Vec<NrProfile>,
    pub(crate) nr_raster_hz: Option<NrBounds>,
    pub(crate) nr_scs_hz: Vec<u32>,
    pub(crate) nr_pbch: bool,
    pub(crate) ook_channel_hz: Option<u64>,
    /// Reserve time for repeated Govee H5054 frames in an OOK dwell.
    pub(crate) ook_govee_h5054: bool,
    /// Use the longer OOK dwell intended for Oregon Scientific bursts.
    pub(crate) ook_oregon_scientific: bool,
    pub(crate) efergy_channel_hz: Option<u64>,
    pub(crate) wh51_channel_hz: Option<u64>,
    pub(crate) ws90_channel_hz: Option<u64>,
    pub(crate) tpms_channel_hz: Option<u64>,
    pub(crate) rs41_channel_hz: Option<u64>,
    pub(crate) rs41_raster_hz: Option<Rs41Bounds>,
    pub(crate) rds_channel_hz: Vec<u64>,
    pub(crate) morse_profile: Vec<MorseProfile>,
}

impl SignalProfile {
    /// Whether the profile selects any in-process replay analyzer.
    pub(crate) fn has_inline_replay(&self) -> bool {
        !self.aprs_channel_hz.is_empty()
            || !self.ais_channel_hz.is_empty()
            || !self.dmr_channel_hz.is_empty()
            || self.dmr_raster_hz.is_some()
            || !self.dect_channel_hz.is_empty()
            || !self.halow_channel_hz.is_empty()
            || !self.scm_channel_hz.is_empty()
            || !self.scm_plus_channel_hz.is_empty()
            || self.adsb
            || self.uat
            || !self.lora_profile.is_empty()
            || self.lte_range_hz.is_some()
            || !self.nr_profile.is_empty()
            || self.nr_raster_hz.is_some()
            || self.ook_channel_hz.is_some()
            || self.efergy_channel_hz.is_some()
            || self.wh51_channel_hz.is_some()
            || self.ws90_channel_hz.is_some()
            || self.tpms_channel_hz.is_some()
            || self.rs41_channel_hz.is_some()
            || self.rs41_raster_hz.is_some()
            || !self.rds_channel_hz.is_empty()
            || !self.morse_profile.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    #[serde(default)]
    aprs_channel_hz: Vec<u64>,
    #[serde(default)]
    ais_channel_hz: Vec<u64>,
    #[serde(default)]
    dmr_channel_hz: Vec<u64>,
    dmr_raster_hz: Option<String>,
    #[serde(default)]
    dect_channel_hz: Vec<u64>,
    #[serde(default)]
    halow_channel_hz: Vec<u64>,
    #[serde(default)]
    scm_channel_hz: Vec<u64>,
    #[serde(default)]
    scm_plus_channel_hz: Vec<u64>,
    #[serde(default)]
    adsb: bool,
    #[serde(default)]
    uat: bool,
    #[serde(default)]
    lora_profile: Vec<String>,
    #[serde(default)]
    lora_packet_profile: Vec<String>,
    #[serde(default)]
    lora_meshtastic: bool,
    #[serde(default)]
    lorawan_join: bool,
    lte_range_hz: Option<String>,
    #[serde(default)]
    lte_pbch: bool,
    #[serde(default)]
    nr_profile: Vec<String>,
    nr_raster_hz: Option<String>,
    #[serde(default)]
    nr_scs_khz: Vec<u32>,
    #[serde(default)]
    nr_pbch: bool,
    ook_channel_hz: Option<u64>,
    #[serde(default)]
    ook_govee_h5054: bool,
    #[serde(default)]
    ook_oregon_scientific: bool,
    efergy_channel_hz: Option<u64>,
    wh51_channel_hz: Option<u64>,
    ws90_channel_hz: Option<u64>,
    tpms_channel_hz: Option<u64>,
    rs41_channel_hz: Option<u64>,
    rs41_raster_hz: Option<String>,
    #[serde(default)]
    rds_channel_hz: Vec<u64>,
    rds_raster_hz: Option<String>,
    #[serde(default)]
    morse_profile: Vec<String>,
}

/// Read a stable regular-file snapshot of a signal profile.
///
/// # Errors
/// Returns an error for an oversized, replaced or unreadable input, invalid
/// JSON, unsupported schema version or incompatible protocol selections.
pub(crate) fn read(path: &Path) -> Result<SignalProfile> {
    let (mut input, length) = file_pin::open_regular(path, MAX_PROFILE_BYTES)
        .with_context(|| format!("open signal profile {}", path.display()))?;
    let before = input.metadata()?;
    let mut bytes = Vec::with_capacity(usize::try_from(length)?);
    (&mut input).take(length + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == length
            && file_pin::same_file(&before, &input.metadata()?)
            && file_pin::same_file(&before, &path.symlink_metadata()?),
        "signal profile changed while reading: {}",
        path.display()
    );
    parse(&bytes).with_context(|| format!("validate signal profile {}", path.display()))
}

fn parse(bytes: &[u8]) -> Result<SignalProfile> {
    ensure!(
        bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{'),
        "signal profile must be a JSON object"
    );
    let document: Document = serde_json::from_slice(bytes)?;
    ensure!(
        document.schema_version == 1,
        "unsupported signal profile schema version {}",
        document.schema_version
    );
    ensure!(
        document
            .aprs_channel_hz
            .iter()
            .all(|&frequency| frequency > 0),
        "APRS channel frequency must be positive"
    );
    ensure!(
        document.ook_channel_hz != Some(0),
        "OOK channel frequency must be positive"
    );
    ensure!(
        !document.ook_govee_h5054 || document.ook_channel_hz.is_some(),
        "ook_govee_h5054 requires ook_channel_hz"
    );
    ensure!(
        !document.ook_oregon_scientific || document.ook_channel_hz.is_some(),
        "ook_oregon_scientific requires ook_channel_hz"
    );
    ensure!(
        document.efergy_channel_hz != Some(0),
        "Efergy channel frequency must be positive"
    );
    ensure!(
        document.wh51_channel_hz != Some(0),
        "WH51 channel frequency must be positive"
    );
    ensure!(
        document.ws90_channel_hz != Some(0),
        "WS90 channel frequency must be positive"
    );
    ensure!(
        document.tpms_channel_hz != Some(0),
        "TPMS channel frequency must be positive"
    );
    ensure!(
        document.rs41_channel_hz != Some(0),
        "RS41 channel frequency must be positive"
    );
    let aprs_channel_hz = aprs_sweep::requested_frequencies(&document.aprs_channel_hz)?
        .into_iter()
        .collect::<Vec<_>>();
    let ais_channel_hz = Protocol::Ais.requested_frequencies(&document.ais_channel_hz)?;
    let dmr_channel_hz = Protocol::Dmr.requested_frequencies(&document.dmr_channel_hz)?;
    let dmr_raster_hz = document
        .dmr_raster_hz
        .as_deref()
        .map(DmrBounds::parse)
        .transpose()
        .context("parse dmr_raster_hz")?;
    let rs41_raster_hz = document
        .rs41_raster_hz
        .as_deref()
        .map(Rs41Bounds::parse)
        .transpose()
        .context("parse rs41_raster_hz")?;
    let dect_channel_hz = Protocol::Dect.requested_frequencies(&document.dect_channel_hz)?;
    let halow_channel_hz = Protocol::Halow.requested_frequencies(&document.halow_channel_hz)?;
    let scm_channel_hz = Protocol::Scm.requested_frequencies(&document.scm_channel_hz)?;
    let scm_plus_channel_hz =
        Protocol::ScmPlus.requested_frequencies(&document.scm_plus_channel_hz)?;
    let lora_profile = lora_sweep::distinct_profiles(parse_profiles(
        &document.lora_profile,
        "lora_profile",
        LoraProfile::parse,
    )?)?
    .into_iter()
    .collect::<Vec<_>>();
    let lora_packet_profile = lora_decode_sweep::distinct_profiles(parse_profiles(
        &document.lora_packet_profile,
        "lora_packet_profile",
        PacketProfile::parse,
    )?)?;
    let packet_interpretations = PacketInterpretations {
        meshtastic: document.lora_meshtastic,
        lorawan_join: document.lorawan_join,
    };
    packet_interpretations.validate_profiles(&lora_packet_profile)?;
    let lte_range_hz = document
        .lte_range_hz
        .as_deref()
        .map(LteRange::parse)
        .transpose()?;
    ensure!(
        !document.lte_pbch || lte_range_hz.is_some(),
        "lte_pbch requires lte_range_hz"
    );
    ensure!(
        document.nr_profile.len() <= nr::MAX_PROFILE_COUNT,
        "supply up to {} NR profiles",
        nr::MAX_PROFILE_COUNT
    );
    let nr_profile = parse_profiles(&document.nr_profile, "nr_profile", NrProfile::parse)?;
    let nr_raster_hz = document
        .nr_raster_hz
        .as_deref()
        .map(NrBounds::parse)
        .transpose()?;
    ensure!(
        nr_profile.is_empty() || nr_raster_hz.is_none(),
        "nr_profile and nr_raster_hz are mutually exclusive"
    );
    ensure!(
        document.nr_scs_khz.is_empty() || nr_raster_hz.is_some(),
        "nr_scs_khz requires nr_raster_hz"
    );
    ensure!(
        !document.nr_pbch || nr_raster_hz.is_some() || !nr_profile.is_empty(),
        "nr_pbch requires nr_profile or nr_raster_hz"
    );
    let mut nr_scs_hz = document.nr_scs_khz;
    for spacing in &mut nr_scs_hz {
        ensure!(
            matches!(*spacing, 15 | 30),
            "NR raster spacing must be 15 or 30 kHz"
        );
        *spacing *= 1_000;
    }
    nr_scs_hz.sort_unstable();
    nr_scs_hz.dedup();
    let rds_raster = document
        .rds_raster_hz
        .as_deref()
        .map(rds_sweep::StationRaster::parse)
        .transpose()
        .context("parse rds_raster_hz")?;
    let rds_channel_hz = rds_sweep::select_stations(document.rds_channel_hz, rds_raster)?;
    let mut morse_profile = parse_profiles(
        &document.morse_profile,
        "morse_profile",
        MorseProfile::parse,
    )?;
    if !morse_profile.is_empty() {
        morse_profile = morse_sweep::distinct_profiles(morse_profile)?;
    }
    let profile = SignalProfile {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        aprs_channel_hz,
        ais_channel_hz,
        dmr_channel_hz,
        dmr_raster_hz,
        dect_channel_hz,
        halow_channel_hz,
        scm_channel_hz,
        scm_plus_channel_hz,
        adsb: document.adsb,
        uat: document.uat,
        lora_profile,
        lora_packet_profile,
        packet_interpretations,
        lte_range_hz,
        lte_pbch: document.lte_pbch,
        nr_profile,
        nr_raster_hz,
        nr_scs_hz,
        nr_pbch: document.nr_pbch,
        ook_channel_hz: document.ook_channel_hz,
        ook_govee_h5054: document.ook_govee_h5054,
        ook_oregon_scientific: document.ook_oregon_scientific,
        efergy_channel_hz: document.efergy_channel_hz,
        wh51_channel_hz: document.wh51_channel_hz,
        ws90_channel_hz: document.ws90_channel_hz,
        tpms_channel_hz: document.tpms_channel_hz,
        rs41_channel_hz: document.rs41_channel_hz,
        rs41_raster_hz,
        rds_channel_hz,
        morse_profile,
    };
    ensure!(
        profile.has_inline_replay() || !profile.lora_packet_profile.is_empty(),
        "signal profile must select at least one analyzer"
    );
    Ok(profile)
}

fn parse_profiles<T>(
    values: &[String],
    field: &str,
    parse_value: fn(&str) -> Result<T>,
) -> Result<Vec<T>> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_value(value).with_context(|| format!("parse {field}[{index}]")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_typed_selections_and_uses_the_original_bytes_for_the_digest() {
        let bytes = br#"{
            "schema_version": 1,
            "aprs_channel_hz": [144390000, 144390000],
            "scm_channel_hz": [914000000, 912000000, 914000000],
            "scm_plus_channel_hz": [914000000],
            "adsb": true,
            "lora_profile": ["906875000:250000:11"],
            "lte_range_hz": "744000000:758000000",
            "lte_pbch": true,
            "nr_raster_hz": "617000000:652000000",
            "nr_scs_khz": [30, 15, 30],
            "nr_pbch": true,
            "rds_channel_hz": [94500000, 93300000],
            "morse_profile": ["nfm:147555000:750:20"]
        }"#;
        let profile = parse(bytes).unwrap();
        assert_eq!(
            profile.sha256,
            data_encoding::HEXLOWER
                .encode(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
        );
        assert_eq!(profile.aprs_channel_hz, [144_390_000]);
        assert!(profile.dmr_channel_hz.is_empty());
        assert!(profile.dect_channel_hz.is_empty());
        assert_eq!(profile.scm_channel_hz, [912_000_000, 914_000_000]);
        assert_eq!(profile.scm_plus_channel_hz, [914_000_000]);
        assert!(profile.adsb);
        assert_eq!(profile.lora_profile.len(), 1);
        let lte = profile.lte_range_hz.unwrap();
        assert_eq!((lte.lower_hz, lte.upper_hz), (744_000_000, 758_000_000));
        assert!(profile.lte_pbch);
        assert!(profile.nr_profile.is_empty());
        let nr = profile.nr_raster_hz.unwrap();
        assert_eq!((nr.lower_hz, nr.upper_hz), (617_000_000, 652_000_000));
        assert_eq!(profile.nr_scs_hz, [15_000, 30_000]);
        assert!(profile.nr_pbch);
        assert_eq!(profile.ook_channel_hz, None);
        assert!(!profile.ook_govee_h5054);
        assert!(!profile.ook_oregon_scientific);
        assert_eq!(profile.efergy_channel_hz, None);
        assert_eq!(profile.tpms_channel_hz, None);
        assert_eq!(profile.rs41_channel_hz, None);
        assert_eq!(profile.rds_channel_hz, [93_300_000, 94_500_000]);
        assert_eq!(profile.morse_profile.len(), 1);
        assert!(profile.has_inline_replay());
        assert!(profile.lora_packet_profile.is_empty());
    }

    #[test]
    fn rejects_conflicting_unsupported_and_empty_profiles() {
        let inputs = [
            ("[1, [], true]", "must be a JSON object"),
            (r#"{"schema_version":2,"adsb":true}"#, "schema version"),
            (
                r#"{"schema_version":1,"adsb":true,"gain":40}"#,
                "unknown field",
            ),
            (
                r#"{"schema_version":1,"adsb":true,"adsb":false}"#,
                "duplicate field",
            ),
            (r#"{"schema_version":1}"#, "at least one analyzer"),
            (
                r#"{"schema_version":1,"lora_packet_profile":["906875000:250000:11:0x2b"]}"#,
                "parse lora_packet_profile[0]",
            ),
            (
                r#"{"schema_version":1,"lora_meshtastic":true,"adsb":true}"#,
                "decimal sync word 43",
            ),
            (
                r#"{"schema_version":1,"lora_meshtastic":true,"lora_packet_profile":["906875000:250000:11:52"]}"#,
                "decimal sync word 43",
            ),
            (
                r#"{"schema_version":1,"lorawan_join":true,"adsb":true}"#,
                "decimal sync word 52",
            ),
            (
                r#"{"schema_version":1,"lorawan_join":true,"lora_packet_profile":["906875000:250000:11:43"]}"#,
                "decimal sync word 52",
            ),
            (
                r#"{"schema_version":1,"aprs_channel_hz":[0]}"#,
                "must be positive",
            ),
            (
                r#"{"schema_version":1,"dmr_channel_hz":[0]}"#,
                "DMR channel must be within",
            ),
            (
                r#"{"schema_version":1,"dect_channel_hz":[0]}"#,
                "DECT channel must be within",
            ),
            (
                r#"{"schema_version":1,"ook_channel_hz":0}"#,
                "OOK channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"adsb":true,"ook_govee_h5054":true}"#,
                "ook_govee_h5054 requires ook_channel_hz",
            ),
            (
                r#"{"schema_version":1,"adsb":true,"ook_oregon_scientific":true}"#,
                "ook_oregon_scientific requires ook_channel_hz",
            ),
            (
                r#"{"schema_version":1,"efergy_channel_hz":0}"#,
                "Efergy channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"wh51_channel_hz":0}"#,
                "WH51 channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"ws90_channel_hz":0}"#,
                "WS90 channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"tpms_channel_hz":0}"#,
                "TPMS channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"rs41_channel_hz":0}"#,
                "RS41 channel frequency must be positive",
            ),
            (
                r#"{"schema_version":1,"rds_channel_hz":[145000000]}"#,
                "RDS station center",
            ),
            (
                r#"{"schema_version":1,"lte_range_hz":"750000001:752000000"}"#,
                "100 kHz grid",
            ),
            (
                r#"{"schema_version":1,"lte_pbch":true}"#,
                "lte_pbch requires",
            ),
            (r#"{"schema_version":1,"nr_pbch":true}"#, "nr_pbch requires"),
            (
                r#"{"schema_version":1,"nr_scs_khz":[15]}"#,
                "nr_scs_khz requires",
            ),
            (
                r#"{"schema_version":1,"nr_raster_hz":"617000000:652000000","nr_scs_khz":[60]}"#,
                "spacing must be 15 or 30",
            ),
            (
                r#"{"schema_version":1,"nr_raster_hz":"617000000:652000000","nr_profile":["627650000:15"]}"#,
                "mutually exclusive",
            ),
        ];
        for (input, expected) in inputs {
            let error = parse(input.as_bytes()).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    #[test]
    fn accepts_a_sensor_channel_as_the_only_analyzer() -> Result<()> {
        let profile = parse(br#"{"schema_version":1,"ook_channel_hz":433920000}"#)?;
        assert_eq!(profile.ook_channel_hz, Some(433_920_000));
        assert!(profile.has_inline_replay());

        let profile = parse(br#"{"schema_version":1,"efergy_channel_hz":433550000}"#)?;
        assert_eq!(profile.efergy_channel_hz, Some(433_550_000));
        assert!(profile.has_inline_replay());

        let profile = parse(br#"{"schema_version":1,"tpms_channel_hz":315000000}"#)?;
        assert_eq!(profile.tpms_channel_hz, Some(315_000_000));
        assert!(profile.has_inline_replay());

        let profile = parse(br#"{"schema_version":1,"rs41_channel_hz":404200000}"#)?;
        assert_eq!(profile.rs41_channel_hz, Some(404_200_000));
        assert!(profile.has_inline_replay());
        Ok(())
    }

    #[test]
    fn accepts_ordered_distinct_dmr_channels_as_the_only_analyzer() -> Result<()> {
        let profile =
            parse(br#"{"schema_version":1,"dmr_channel_hz":[444012500,444000000,444012500]}"#)?;
        assert_eq!(profile.dmr_channel_hz, [444_000_000, 444_012_500]);
        assert!(profile.has_inline_replay());
        Ok(())
    }

    #[test]
    fn expands_an_rds_raster_as_the_only_analyzer() {
        let profile = parse(
            br#"{"schema_version":1,"rds_raster_hz":"87900000:107900000:200000","rds_channel_hz":[]}"#,
        )
        .unwrap();
        let expected: Vec<u64> = (0..=100)
            .map(|index| 87_900_000 + 200_000 * index)
            .collect();
        assert_eq!(profile.rds_channel_hz, expected);

        let conflicting = br#"{"schema_version":1,"rds_raster_hz":"87900000:107900000:200000","rds_channel_hz":[93300000]}"#;
        assert!(parse(conflicting).is_err());
    }

    #[test]
    fn accepts_packet_profiles_with_or_without_inline_replay() -> Result<()> {
        let packet = "906875000:250000:11:43";
        let packet_only = json!({
            "schema_version": 1,
            "lora_packet_profile": [packet, packet],
        });
        let profile = parse(packet_only.to_string().as_bytes())?;
        assert_eq!(profile.lora_packet_profile, [PacketProfile::parse(packet)?]);
        assert!(!profile.packet_interpretations.meshtastic);
        assert!(!profile.packet_interpretations.lorawan_join);
        assert!(!profile.has_inline_replay());

        let mixed = json!({
            "schema_version": 1,
            "lora_packet_profile": [packet, "903900000:125000:10:52"],
            "lora_profile": ["906875000:250000:11"],
            "lora_meshtastic": true,
            "lorawan_join": true,
        });
        let profile = parse(mixed.to_string().as_bytes())?;
        assert_eq!(profile.lora_packet_profile.len(), 2);
        assert!(profile.packet_interpretations.meshtastic);
        assert!(profile.packet_interpretations.lorawan_join);
        assert!(profile.has_inline_replay());
        Ok(())
    }

    #[test]
    fn preserves_distinct_channel_and_raw_nr_profile_limits() {
        let duplicate_aprs = json!({
            "schema_version": 1,
            "aprs_channel_hz": vec![144_390_000; 33],
        });
        assert_eq!(
            parse(duplicate_aprs.to_string().as_bytes())
                .unwrap()
                .aprs_channel_hz,
            [144_390_000]
        );
        let too_many_channels = json!({
            "schema_version": 1,
            "aprs_channel_hz": (144_390_000..144_390_033).collect::<Vec<_>>(),
        });
        assert!(parse(too_many_channels.to_string().as_bytes()).is_err());
        let too_many_nr = json!({
            "schema_version": 1,
            "nr_profile": vec!["627650000:15"; nr::MAX_PROFILE_COUNT + 1],
        });
        assert!(parse(too_many_nr.to_string().as_bytes()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn admits_only_bounded_regular_profile_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        std::fs::write(&path, br#"{"schema_version":1,"adsb":true}"#)?;
        assert!(read(&path)?.adsb);

        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link)?;
        assert!(read(&link).is_err());
        let oversized = std::fs::File::create(directory.path().join("oversized.json"))?;
        oversized.set_len(MAX_PROFILE_BYTES + 1)?;
        let error = read(&directory.path().join("oversized.json")).unwrap_err();
        assert!(format!("{error:#}").contains("no larger than"));
        Ok(())
    }
}
