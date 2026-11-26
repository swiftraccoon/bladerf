//! Plan guarded receive dwells for the protocols named in a signal profile.

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use anyhow::{Context, Result, ensure};

use crate::dsp::{AdsbChannel, SensorChannel};
use crate::lte::raster::{self as lte, GRID_SPACING_HZ, GUARDED_CARRIER_OFFSET_HZ};
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::signal_profile::SignalProfile;
use crate::spectrum::USABLE_HALF_SPAN_HZ;
use crate::{
    MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE, adsb, ais, aprs, dect, dmr, efergy, halow, morse,
    nr, ook, rds, rs41, scm, tpms, uat, wh51, ws90,
};

use super::plan::{self, Band};

const SAMPLES_PER_MS: u64 = SAMPLE_RATE as u64 / 1_000;
// Allow room for a nominal WGR800 three-copy burst of about 330 ms.
const OREGON_DWELL_MS: u64 = 400;

fn guarded_band(center_hz: u64, half_width_hz: u64) -> Result<Band> {
    let lower_hz = center_hz
        .checked_sub(half_width_hz)
        .filter(|&lower_hz| lower_hz > 0)
        .context("signal profile band must begin above zero hertz")?;
    let upper_hz = center_hz
        .checked_add(half_width_hz)
        .context("signal profile band upper frequency overflows")?;
    Ok(Band { lower_hz, upper_hz })
}

fn from_range(range: Range<u64>) -> Band {
    Band {
        lower_hz: range.start,
        upper_hz: range.end,
    }
}

fn dwell_ms(default_dwell_ms: u64, minimum_samples: u64) -> u64 {
    default_dwell_ms.max(minimum_samples.div_ceil(SAMPLES_PER_MS))
}

fn narrow_requests(profile: &SignalProfile, default_dwell_ms: u64) -> Result<Vec<(Band, u64)>> {
    let mut requests = Vec::new();
    let aprs_dwell_ms = dwell_ms(default_dwell_ms, aprs::MIN_SWEEP_SAMPLES);
    for &channel_hz in &profile.aprs_channel_hz {
        requests.push((
            guarded_band(channel_hz, aprs::CHANNEL_MARGIN_HZ)?,
            aprs_dwell_ms,
        ));
    }
    for &channel_hz in &profile.ais_channel_hz {
        requests.push((
            guarded_band(channel_hz, ais::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(ais::FRAME_DWELL_MS),
        ));
    }
    for &channel_hz in &profile.dmr_channel_hz {
        requests.push((
            guarded_band(channel_hz, dmr::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(dmr::FRAME_DWELL_MS),
        ));
    }
    for &channel_hz in &profile.dect_channel_hz {
        requests.push((
            guarded_band(channel_hz, dect::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(dect::FRAME_DWELL_MS),
        ));
    }
    for &channel_hz in &profile.halow_channel_hz {
        requests.push((
            guarded_band(channel_hz, halow::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(halow::FRAME_DWELL_MS),
        ));
    }
    for (channels, minimum_ms) in [
        (&profile.scm_channel_hz, scm::LEGACY_FRAME_DWELL_MS),
        (&profile.scm_plus_channel_hz, scm::FRAME_DWELL_MS),
    ] {
        for &channel_hz in channels {
            requests.push((
                guarded_band(channel_hz, scm::CHANNEL_HALF_WIDTH_HZ)?,
                default_dwell_ms.max(minimum_ms),
            ));
        }
    }
    if profile.adsb {
        requests.push((
            guarded_band(adsb::CHANNEL_HZ, AdsbChannel::HALF_WIDTH_HZ)?,
            dwell_ms(default_dwell_ms, adsb::MIN_SWEEP_SAMPLES),
        ));
    }
    if profile.uat {
        requests.push((
            guarded_band(uat::CHANNEL_HZ, uat::CHANNEL_HALF_WIDTH_HZ)?,
            dwell_ms(default_dwell_ms, uat::PLANNED_FRAME_SAMPLES),
        ));
    }
    for &entry in &profile.lora_profile {
        let bounds = entry
            .rf_bounds_hz()
            .context("LoRa signal profile has invalid RF bounds")?;
        requests.push((
            from_range(bounds),
            dwell_ms(default_dwell_ms, entry.minimum_samples()),
        ));
    }
    for &entry in &profile.lora_packet_profile {
        let bounds = entry
            .rf_bounds_hz()
            .context("LoRa packet signal profile has invalid RF bounds")?;
        requests.push((
            from_range(bounds),
            dwell_ms(
                default_dwell_ms,
                entry.minimum_capture_samples(profile.packet_interpretations.select(entry)),
            ),
        ));
    }
    let nr_dwell_ms = dwell_ms(default_dwell_ms, nr::MIN_DWELL_SAMPLES);
    for &entry in &profile.nr_profile {
        requests.push((
            from_range(entry.guarded_frequency_range(profile.nr_pbch)),
            nr_dwell_ms,
        ));
    }
    if let Some(channel_hz) = profile.ook_channel_hz {
        let (minimum_ms, half_width_hz) = if profile.ook_oregon_scientific {
            (OREGON_DWELL_MS, ook::OREGON_HALF_WIDTH_HZ)
        } else if profile.ook_govee_h5054 {
            (ook::GOVEE_DWELL_MS, SensorChannel::HALF_WIDTH_HZ)
        } else {
            (ook::FRAME_DWELL_MS, SensorChannel::HALF_WIDTH_HZ)
        };
        requests.push((
            guarded_band(channel_hz, half_width_hz)?,
            default_dwell_ms.max(minimum_ms),
        ));
    }
    if let Some(channel_hz) = profile.efergy_channel_hz {
        requests.push((
            guarded_band(channel_hz, efergy::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(efergy::FRAME_DWELL_MS),
        ));
    }
    if let Some(channel_hz) = profile.wh51_channel_hz {
        requests.push((
            guarded_band(channel_hz, SensorChannel::HALF_WIDTH_HZ)?,
            default_dwell_ms.max(wh51::FRAME_DWELL_MS),
        ));
    }
    if let Some(channel_hz) = profile.ws90_channel_hz {
        requests.push((
            guarded_band(channel_hz, SensorChannel::HALF_WIDTH_HZ)?,
            default_dwell_ms.max(ws90::FRAME_DWELL_MS),
        ));
    }
    if let Some(channel_hz) = profile.tpms_channel_hz {
        requests.push((
            guarded_band(channel_hz, SensorChannel::HALF_WIDTH_HZ)?,
            default_dwell_ms.max(tpms::FRAME_DWELL_MS),
        ));
    }
    if let Some(channel_hz) = profile.rs41_channel_hz {
        requests.push((
            guarded_band(channel_hz, rs41::CHANNEL_HALF_WIDTH_HZ)?,
            default_dwell_ms.max(rs41::FRAME_DWELL_MS),
        ));
    }
    let rds_dwell_ms = dwell_ms(default_dwell_ms, rds::MIN_SWEEP_SAMPLES);
    for &channel_hz in &profile.rds_channel_hz {
        requests.push((
            guarded_band(channel_hz, rds::FM_CHANNEL_HALF_WIDTH_HZ)?,
            rds_dwell_ms,
        ));
    }
    let morse_dwell_ms = dwell_ms(default_dwell_ms, morse::MIN_SWEEP_SAMPLES);
    for &entry in &profile.morse_profile {
        requests.push((
            guarded_band(entry.channel_frequency_hz(), morse::CHANNEL_MARGIN_HZ)?,
            morse_dwell_ms,
        ));
    }
    Ok(requests)
}

fn include_center(centers: &mut BTreeMap<u64, u64>, center_hz: u64, dwell_ms: u64) -> Result<()> {
    centers
        .entry(center_hz)
        .and_modify(|existing| *existing = (*existing).max(dwell_ms))
        .or_insert(dwell_ms);
    ensure!(
        centers.len() <= plan::MAX_TUNES,
        "signal profile requires more than {} distinct tuning centers",
        plan::MAX_TUNES
    );
    Ok(())
}

fn cover_nr_raster(
    centers: &mut BTreeMap<u64, u64>,
    bounds: nr::raster::Bounds,
    spacings_hz: &[u32],
    include_pbch: bool,
    minimum_ms: u64,
) -> Result<()> {
    let spacings_hz = if spacings_hz.is_empty() {
        &nr::raster::SPACINGS_HZ
    } else {
        spacings_hz
    };
    let mut bands: Vec<_> = nr::raster::entries(bounds, spacings_hz)?
        .into_iter()
        .map(|entry| from_range(entry.profile.guarded_frequency_range(include_pbch)))
        .collect();
    ensure!(
        !bands.is_empty(),
        "NR raster contains no selected references"
    );
    // Visit the earliest lower edges first. The latest legal center for each
    // missing band leaves the most room for subsequent bands at that edge.
    bands.sort_unstable();
    for band in bands {
        if centers.iter().any(|(&center_hz, &dwell_ms)| {
            dwell_ms >= minimum_ms
                && band.is_covered_by(plan::observed_bands(
                    center_hz,
                    ACQUISITION_CENTER_TOLERANCE_HZ,
                ))
        }) {
            continue;
        }
        let center_hz = plan::preferred_profile_center(band)?;
        include_center(centers, center_hz, minimum_ms)?;
    }
    Ok(())
}

fn cover_lte_raster(
    centers: &mut BTreeMap<u64, u64>,
    carrier_range: RangeInclusive<u64>,
    minimum_ms: u64,
) -> Result<()> {
    let mut carrier_hz = *carrier_range.start();
    let last_hz = *carrier_range.end();
    loop {
        let reach_hz = GUARDED_CARRIER_OFFSET_HZ;
        let earliest_hz = carrier_hz.saturating_sub(reach_hz).max(MIN_CENTER_HZ);
        let latest_hz = (carrier_hz + reach_hz).min(MAX_CENTER_HZ);
        ensure!(
            earliest_hz <= latest_hz,
            "LTE carrier {carrier_hz} Hz cannot be covered from a supported tuning center"
        );
        let center_hz = centers
            .range(earliest_hz..=latest_hz)
            .rev()
            .find_map(|(&center_hz, &dwell_ms)| (dwell_ms >= minimum_ms).then_some(center_hz))
            .unwrap_or(latest_hz);
        include_center(centers, center_hz, minimum_ms)?;
        let covered_through_hz = (center_hz + reach_hz) / GRID_SPACING_HZ * GRID_SPACING_HZ;
        if covered_through_hz >= last_hz {
            return Ok(());
        }
        carrier_hz = covered_through_hz + GRID_SPACING_HZ;
    }
}

fn cover_guarded_raster(
    centers: &mut BTreeMap<u64, u64>,
    channels_hz: RangeInclusive<u64>,
    step_hz: u64,
    half_width_hz: u64,
    minimum_ms: u64,
) -> Result<()> {
    let reach_hz = USABLE_HALF_SPAN_HZ as u64;
    let mut channel_hz = *channels_hz.start();
    while channel_hz <= *channels_hz.end() {
        let band = guarded_band(channel_hz, half_width_hz)?;
        let covered_through = |center_hz| {
            plan::observed_bands(center_hz, ACQUISITION_CENTER_TOLERANCE_HZ)
                .into_iter()
                .find(|span| span.contains_interval(band.lower_hz, band.upper_hz))
                .map(|span| span.upper_hz - half_width_hz)
        };
        let existing = centers
            .range(channel_hz.saturating_sub(reach_hz)..=channel_hz + reach_hz)
            .filter(|&(_, &dwell_ms)| dwell_ms >= minimum_ms)
            .filter_map(|(&center_hz, _)| covered_through(center_hz))
            .max();
        let covered_hz = if let Some(covered_hz) = existing {
            covered_hz
        } else {
            let center_hz = plan::preferred_profile_center(band)?;
            include_center(centers, center_hz, minimum_ms)?;
            covered_through(center_hz).context("raster tune leaves its channel uncovered")?
        };
        // Each guarded span covers a contiguous run of the anchored lattice.
        channel_hz += ((covered_hz - channel_hz) / step_hz + 1) * step_hz;
    }
    Ok(())
}

/// Translate a typed protocol profile into exact saved dwell extents.
///
/// The saved duration is at least the decoder's minimum admitted extent.
/// Actual detection still depends on the waveform and when it arrives.
///
/// # Errors
/// Returns an error for unsupported guard geometry, too many required
/// tunings, or a dwell or raw IQ extent beyond the sweep limits.
pub(super) fn plan(
    profile: &SignalProfile,
    default_dwell_ms: u64,
    max_iq_bytes: u64,
) -> Result<plan::Plan> {
    ensure!(
        (1..=plan::MAX_DWELL_MS).contains(&default_dwell_ms),
        "sweep dwell must be 1..={} milliseconds",
        plan::MAX_DWELL_MS
    );
    let narrow = narrow_requests(profile, default_dwell_ms)?;
    let mut centers: BTreeMap<_, _> = plan::schedule_profile_bands(&narrow)?.into_iter().collect();
    if let Some(bounds) = profile.nr_raster_hz {
        cover_nr_raster(
            &mut centers,
            bounds,
            &profile.nr_scs_hz,
            profile.nr_pbch,
            dwell_ms(default_dwell_ms, nr::MIN_DWELL_SAMPLES),
        )?;
    }
    if let Some(range) = &profile.lte_range_hz {
        cover_lte_raster(
            &mut centers,
            range.lower_hz..=range.upper_hz,
            dwell_ms(default_dwell_ms, lte::MIN_SWEEP_SAMPLES),
        )?;
    }
    if let Some(bounds) = profile.rs41_raster_hz {
        cover_guarded_raster(
            &mut centers,
            bounds.lower_hz..=bounds.upper_hz,
            rs41::raster::GRID_SPACING_HZ,
            rs41::CHANNEL_HALF_WIDTH_HZ,
            default_dwell_ms.max(rs41::FRAME_DWELL_MS),
        )?;
    }
    if let Some(bounds) = profile.dmr_raster_hz {
        cover_guarded_raster(
            &mut centers,
            bounds.lower_hz..=bounds.upper_hz,
            dmr::raster::GRID_SPACING_HZ,
            dmr::CHANNEL_HALF_WIDTH_HZ,
            default_dwell_ms.max(dmr::FRAME_DWELL_MS),
        )?;
    }
    let overrides = centers
        .iter()
        .filter(|&(_, &dwell_ms)| dwell_ms != default_dwell_ms)
        .map(|(&center_hz, &dwell_ms)| plan::DwellOverride::new(center_hz, dwell_ms))
        .collect::<Result<Vec<_>>>()?;
    let centers: Vec<_> = centers.into_keys().collect();
    plan::targeted_with_dwells(&centers, default_dwell_ms, max_iq_bytes, &overrides)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_shifted_dmr_channels_and_band_ends_at_readback_extremes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        for (lower_hz, upper_hz) in [
            (MIN_CENTER_HZ, MIN_CENTER_HZ + 20_000_000),
            (420_006_250, 480_006_250),
            (MAX_CENTER_HZ - 20_000_000, MAX_CENTER_HZ),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "dmr_raster_hz": format!("{lower_hz}:{upper_hz}"),
            }))?;
            std::fs::write(&path, bytes)?;
            let profile = crate::signal_profile::read(&path)?;
            let result = plan(&profile, 1, u64::MAX)?;
            assert!((2..=8).contains(&result.centers_hz.len()));
            assert_eq!(
                result.planned_iq_bytes,
                result.centers_hz.len() as u64 * dmr::FRAME_DWELL_MS * SAMPLES_PER_MS * 4
            );
            for channel_hz in (lower_hz..=upper_hz).step_by(dmr::raster::GRID_SPACING_HZ as usize) {
                assert!(
                    result.centers_hz.iter().any(|&nominal| {
                        [
                            nominal - ACQUISITION_CENTER_TOLERANCE_HZ,
                            nominal + ACQUISITION_CENTER_TOLERANCE_HZ,
                        ]
                        .into_iter()
                        .all(|center_hz| {
                            dmr::Channel {
                                center_hz,
                                channel_hz,
                                sample_rate_hz: SAMPLE_RATE,
                            }
                            .is_covered()
                        })
                    }),
                    "DMR raster channel {channel_hz} Hz has no guaranteed visit"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn reuses_an_adequate_existing_dmr_dwell() -> Result<()> {
        let existing = BTreeMap::from([(443_500_000, 60)]);
        let mut centers = existing.clone();
        cover_guarded_raster(
            &mut centers,
            444_006_250..=444_506_250,
            dmr::raster::GRID_SPACING_HZ,
            dmr::CHANNEL_HALF_WIDTH_HZ,
            dmr::FRAME_DWELL_MS,
        )?;
        assert_eq!(centers, existing);
        Ok(())
    }

    #[test]
    fn plans_one_full_rs41_raster_dwell_at_readback_extremes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        std::fs::write(
            &path,
            br#"{"schema_version":1,"rs41_raster_hz":"400150000:406000000"}"#,
        )?;
        let profile = crate::signal_profile::read(&path)?;
        let expected_bytes = rs41::FRAME_DWELL_MS * SAMPLES_PER_MS * 4;
        let result = plan(&profile, 1, expected_bytes)?;
        let &[center_hz] = result.centers_hz.as_slice() else {
            panic!("expected one RS41 tune, received {:?}", result.centers_hz);
        };
        assert_eq!(result.dwell_ms_by_center[&center_hz], rs41::FRAME_DWELL_MS);
        assert_eq!(result.planned_iq_bytes, expected_bytes);
        for readback_hz in [
            center_hz - ACQUISITION_CENTER_TOLERANCE_HZ,
            center_hz + ACQUISITION_CENTER_TOLERANCE_HZ,
        ] {
            let grid = rs41::raster::Grid::new(
                readback_hz,
                rs41::raster::BAND_LOWER_HZ,
                rs41::raster::BAND_UPPER_HZ,
            )?;
            assert_eq!(grid.channels_hz().len(), 469);
        }
        Ok(())
    }

    #[test]
    fn plans_one_ha_low_dwell_for_nearby_channels() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        std::fs::write(
            &path,
            br#"{"schema_version":1,"halow_channel_hz":[868000000,866000000,866000000]}"#,
        )?;
        let profile = crate::signal_profile::read(&path)?;
        assert_eq!(profile.halow_channel_hz, [866_000_000, 868_000_000]);
        assert!(profile.has_inline_replay());

        let expected_bytes = halow::FRAME_DWELL_MS * SAMPLES_PER_MS * 4;
        let result = plan(&profile, 1, expected_bytes)?;
        let &[center_hz] = result.centers_hz.as_slice() else {
            panic!("expected one HaLow tune, received {:?}", result.centers_hz);
        };
        assert_eq!(result.dwell_ms_by_center[&center_hz], halow::FRAME_DWELL_MS);
        assert_eq!(result.planned_iq_bytes, expected_bytes);
        for readback_hz in [
            center_hz - ACQUISITION_CENTER_TOLERANCE_HZ,
            center_hz + ACQUISITION_CENTER_TOLERANCE_HZ,
        ] {
            for &channel_hz in &profile.halow_channel_hz {
                assert!(
                    halow::Channel {
                        center_hz: readback_hz,
                        channel_hz,
                        sample_rate_hz: SAMPLE_RATE,
                    }
                    .is_covered(),
                    "HaLow channel {channel_hz} Hz at readback {readback_hz} Hz"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn plans_one_guarded_dwell_for_five_dect_channels() -> Result<()> {
        let channels = [
            1_921_536_000,
            1_923_264_000,
            1_924_992_000,
            1_926_720_000,
            1_928_448_000,
        ];
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "dect_channel_hz": channels,
        }))?;
        std::fs::write(&path, bytes)?;
        let profile = crate::signal_profile::read(&path)?;
        let expected_bytes = dect::FRAME_DWELL_MS * SAMPLES_PER_MS * 4;
        let result = plan(&profile, 1, expected_bytes)?;
        let &[center_hz] = result.centers_hz.as_slice() else {
            panic!("expected one DECT tune, received {:?}", result.centers_hz);
        };
        assert_eq!(result.dwell_ms_by_center[&center_hz], dect::FRAME_DWELL_MS);
        assert_eq!(result.planned_iq_bytes, expected_bytes);
        assert!(plan(&profile, 1, expected_bytes - 1).is_err());
        for center_hz in [
            center_hz - ACQUISITION_CENTER_TOLERANCE_HZ,
            center_hz + ACQUISITION_CENTER_TOLERANCE_HZ,
        ] {
            for channel_hz in channels {
                assert!(
                    dect::Channel {
                        center_hz,
                        channel_hz,
                        sample_rate_hz: SAMPLE_RATE,
                    }
                    .is_covered(),
                    "DECT channel {channel_hz} Hz at readback {center_hz} Hz"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn plans_one_guarded_dwell_for_nearby_efergy_and_ook_channels() -> Result<()> {
        let efergy_hz = efergy::DEFAULT_CHANNEL_HZ;
        let ook_hz = ook::DEFAULT_CHANNEL_HZ;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "efergy_channel_hz": efergy_hz,
            "ook_channel_hz": ook_hz,
            "ook_govee_h5054": true,
        }))?;
        std::fs::write(&path, bytes)?;
        let mut profile = crate::signal_profile::read(&path)?;
        let minimum_ms = efergy::FRAME_DWELL_MS.max(ook::GOVEE_DWELL_MS);
        let planned_iq_bytes = minimum_ms * SAMPLES_PER_MS * 4;
        let result = plan(&profile, 1, planned_iq_bytes)?;
        assert_eq!(result.centers_hz.len(), 1);
        assert_eq!(result.planned_iq_bytes, planned_iq_bytes);
        let center_hz = result.centers_hz[0];
        assert_eq!(result.dwell_at(center_hz), minimum_ms);
        for observed_hz in [
            center_hz - ACQUISITION_CENTER_TOLERANCE_HZ,
            center_hz + ACQUISITION_CENTER_TOLERANCE_HZ,
        ] {
            assert!(
                efergy::Channel {
                    center_hz: observed_hz,
                    channel_hz: efergy_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered()
            );
            assert!(
                ook::Channel {
                    center_hz: observed_hz,
                    channel_hz: ook_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered()
            );
        }
        assert!(plan(&profile, 1, planned_iq_bytes - 1).is_err());

        profile.ook_channel_hz = None;
        profile.ook_govee_h5054 = false;
        let standalone = plan(&profile, 1, planned_iq_bytes)?;
        assert_eq!(standalone.centers_hz.len(), 1);
        assert_eq!(
            standalone.dwell_at(standalone.centers_hz[0]),
            efergy::FRAME_DWELL_MS
        );
        Ok(())
    }

    #[test]
    fn plans_one_guarded_dwell_for_ws90_and_same_channel_wh51() -> Result<()> {
        let channel_hz = ws90::DEFAULT_CHANNEL_HZ;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("profile.json");
        for wh51_channel_hz in [None, Some(channel_hz)] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "ws90_channel_hz": channel_hz,
                "wh51_channel_hz": wh51_channel_hz,
            }))?;
            std::fs::write(&path, bytes)?;
            let profile = crate::signal_profile::read(&path)?;
            let result = plan(&profile, 1, u64::MAX)?;
            assert_eq!(result.centers_hz.len(), 1);
            let center_hz = result.centers_hz[0];
            let minimum_ms = wh51_channel_hz.map_or(ws90::FRAME_DWELL_MS, |_| {
                ws90::FRAME_DWELL_MS.max(wh51::FRAME_DWELL_MS)
            });
            assert_eq!(result.dwell_ms_by_center[&center_hz], minimum_ms);
            assert_eq!(result.planned_iq_bytes, minimum_ms * SAMPLES_PER_MS * 4);
            for observed_hz in [
                center_hz - ACQUISITION_CENTER_TOLERANCE_HZ,
                center_hz + ACQUISITION_CENTER_TOLERANCE_HZ,
            ] {
                assert!(
                    ws90::Channel {
                        center_hz: observed_hz,
                        channel_hz,
                        sample_rate_hz: SAMPLE_RATE,
                    }
                    .is_covered()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn covers_a_wide_lte_grid_at_both_readback_edges() -> Result<()> {
        let first_hz = 702_000_000;
        let last_hz = 917_000_000;
        let retained_hz = first_hz + GUARDED_CARRIER_OFFSET_HZ;
        let mut centers = BTreeMap::from([(retained_hz, 150)]);
        cover_lte_raster(&mut centers, first_hz..=last_hz, 40)?;
        assert_eq!(centers[&retained_hz], 150);
        assert_eq!(centers.len(), 16);
        for carrier_hz in (first_hz..=last_hz).step_by(GRID_SPACING_HZ as usize) {
            assert!(centers.iter().any(|(&nominal, &dwell_ms)| {
                dwell_ms >= 40
                    && [
                        nominal - ACQUISITION_CENTER_TOLERANCE_HZ,
                        nominal + ACQUISITION_CENTER_TOLERANCE_HZ,
                    ]
                    .into_iter()
                    .all(|observed| {
                        lte::covered_centers(observed, carrier_hz, carrier_hz).is_some()
                    })
            }));
        }
        Ok(())
    }

    #[test]
    fn covers_full_nr_blocks_without_counting_short_dwells() -> Result<()> {
        let bounds = nr::raster::Bounds::parse("617000000:652000000")?;
        let mut centers = BTreeMap::from([(631_650_000, 1)]);
        cover_nr_raster(&mut centers, bounds, &[], true, 40)?;
        assert!(centers.len() > 1 && centers.len() < 100);
        for entry in nr::raster::entries(bounds, &nr::raster::SPACINGS_HZ)? {
            let bounds = entry.profile.guarded_frequency_range(true);
            assert!(centers.iter().any(|(&nominal, &dwell_ms)| {
                dwell_ms >= 40
                    && [
                        nominal - ACQUISITION_CENTER_TOLERANCE_HZ,
                        nominal + ACQUISITION_CENTER_TOLERANCE_HZ,
                    ]
                    .into_iter()
                    .all(|observed| {
                        plan::observed_bands(observed, 0)
                            .iter()
                            .any(|band| band.contains_interval(bounds.start, bounds.end))
                    })
            }));
        }
        Ok(())
    }
}
