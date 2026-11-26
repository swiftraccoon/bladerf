//! Bound replay work for a sweep whose actual tuning readbacks are pending.

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::lte::raster as lte;
use crate::receiver::ACQUISITION_CENTER_TOLERANCE_HZ;
use crate::signal_profile::{LteRange, SignalProfile};
use crate::spectrum::USABLE_HALF_SPAN_HZ;
use crate::{
    MAX_CENTER_HZ, MIN_CENTER_HZ, SAMPLE_RATE, adsb, aprs, dmr, efergy, lora, morse, nr, ook, rds,
    rs41, tpms, uat, wh51, ws90,
};

use super::{BudgetUnit, ReplayLimits};
use crate::triage::channel_sweep::Protocol;

/// Independent upper bounds over all admitted integer-hertz readbacks.
#[derive(Debug, Serialize)]
pub(crate) struct Preflight {
    center_readback_tolerance_hz: u64,
    analyzers: Vec<AnalyzerBudget>,
}

#[derive(Debug, Serialize)]
struct AnalyzerBudget {
    analyzer: &'static str,
    maximum_budget_samples: u64,
    max_budget_samples: u64,
    budget_unit: BudgetUnit,
}

struct Prefixes {
    general: u64,
    lte: u64,
    nr: u64,
}

impl Prefixes {
    fn new(saved_samples: u64, limits: &ReplayLimits) -> Self {
        Self {
            general: saved_samples.min(limits.general_prefix()),
            lte: saved_samples.min(limits.lte_prefix()),
            nr: saved_samples.min(limits.nr_prefix()),
        }
    }
}

#[derive(Clone, Copy)]
enum Analyzer<'a> {
    Aprs(&'a [u64]),
    Channels(Protocol, &'a [u64]),
    DmrRasterSource(dmr::raster::Bounds),
    DmrRaster(dmr::raster::Bounds),
    Adsb,
    Lora(&'a [lora::bank::Profile]),
    Lte(LteRange),
    Nr {
        profiles: &'a [nr::Profile],
        raster: bool,
    },
    Rds(&'a [u64]),
    Morse(&'a [morse::Profile]),
    Ook(u64),
    Efergy(u64),
    Wh51(u64),
    Ws90(u64),
    Tpms(u64),
    Rs41(u64),
    Rs41RasterSource(rs41::raster::Bounds),
    Rs41Raster(rs41::raster::Bounds),
}

impl Analyzer<'_> {
    fn budget(self, limits: &ReplayLimits) -> AnalyzerBudget {
        let (analyzer, max_budget_samples, budget_unit) = match self {
            Self::Aprs(_) => ("aprs", limits.max_total_samples, BudgetUnit::Iq),
            Self::Channels(protocol, _) => {
                let analyzer = match protocol {
                    Protocol::Ais => "ais",
                    Protocol::Dmr => "dmr",
                    Protocol::Dect => "dect",
                    Protocol::Halow => "halow",
                    Protocol::Scm => "scm",
                    Protocol::ScmPlus => "scm_plus",
                    Protocol::Uat => "uat",
                };
                (analyzer, limits.max_total_samples, BudgetUnit::Iq)
            }
            Self::DmrRasterSource(_) => (
                "dmr_raster_source",
                limits.max_total_samples,
                BudgetUnit::Iq,
            ),
            Self::DmrRaster(_) => (
                "dmr_raster",
                limits.max_total_channelized_samples,
                BudgetUnit::Channelized,
            ),
            Self::Adsb => ("adsb", limits.max_total_samples, BudgetUnit::Iq),
            Self::Lora(_) => (
                "lora",
                limits.max_total_profile_samples,
                BudgetUnit::Profile,
            ),
            Self::Lte(_) => (
                "lte",
                limits.max_total_channelized_samples,
                BudgetUnit::Channelized,
            ),
            Self::Nr { .. } => (
                "nr",
                limits.max_total_channelized_samples,
                BudgetUnit::Channelized,
            ),
            Self::Rds(_) => ("rds", limits.max_total_station_samples, BudgetUnit::Station),
            Self::Morse(_) => (
                "morse",
                limits.max_total_profile_samples,
                BudgetUnit::Profile,
            ),
            Self::Ook(_) => ("ook", limits.max_total_samples, BudgetUnit::Iq),
            Self::Efergy(_) => ("efergy", limits.max_total_samples, BudgetUnit::Iq),
            Self::Wh51(_) => ("wh51", limits.max_total_samples, BudgetUnit::Iq),
            Self::Ws90(_) => ("ws90", limits.max_total_samples, BudgetUnit::Iq),
            Self::Tpms(_) => ("tpms", limits.max_total_samples, BudgetUnit::Iq),
            Self::Rs41(_) => ("rs41", limits.max_total_samples, BudgetUnit::Iq),
            Self::Rs41RasterSource(_) => (
                "rs41_raster_source",
                limits.max_total_samples,
                BudgetUnit::Iq,
            ),
            Self::Rs41Raster(_) => (
                "rs41_raster",
                limits.max_total_channelized_samples,
                BudgetUnit::Channelized,
            ),
        };
        AnalyzerBudget {
            analyzer,
            maximum_budget_samples: 0,
            max_budget_samples,
            budget_unit,
        }
    }

    fn samples(self, center_hz: u64, prefixes: &Prefixes) -> Result<u64> {
        let samples = prefixes.general;
        let attempts = match self {
            Self::Aprs(channels) if samples >= aprs::MIN_SWEEP_SAMPLES => channels
                .iter()
                .filter(|&&channel_hz| {
                    aprs::Channel {
                        center_hz,
                        channel_hz,
                        sample_rate_hz: SAMPLE_RATE,
                    }
                    .is_covered()
                })
                .count(),
            Self::Channels(protocol, channels) if samples >= protocol.minimum_frame_samples() => {
                channels
                    .iter()
                    .filter(|&&channel_hz| protocol.is_covered(center_hz, channel_hz))
                    .count()
            }
            Self::DmrRasterSource(bounds) | Self::DmrRaster(bounds)
                if samples >= dmr::MIN_SWEEP_SAMPLES =>
            {
                let grid = dmr::raster::Grid::new(center_hz, bounds.lower_hz, bounds.upper_hz)?;
                return Ok(if matches!(self, Self::DmrRasterSource(_)) {
                    samples * u64::from(grid.channel_count() > 0)
                } else {
                    grid.channelized_samples(samples)
                });
            }
            Self::Adsb if samples >= adsb::MIN_SWEEP_SAMPLES => usize::from(
                adsb::Channel {
                    center_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Lora(profiles) => profiles
                .iter()
                .filter(|profile| {
                    samples >= profile.minimum_samples() && profile.is_covered(center_hz)
                })
                .count(),
            Self::Lte(range) => return lte_samples(center_hz, range, prefixes.lte),
            Self::Nr { profiles, raster } => {
                return nr_samples(center_hz, profiles, raster, prefixes.nr);
            }
            Self::Rds(channels) if samples >= rds::MIN_SWEEP_SAMPLES => channels
                .iter()
                .filter(|&&channel_hz| rds::is_covered(center_hz, channel_hz))
                .count(),
            Self::Morse(profiles) if samples >= morse::MIN_SWEEP_SAMPLES => profiles
                .iter()
                .filter(|profile| profile.is_covered(center_hz))
                .count(),
            Self::Ook(channel_hz) if samples >= ook::MIN_SWEEP_SAMPLES => usize::from(
                ook::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Efergy(channel_hz) if samples >= efergy::MIN_SWEEP_SAMPLES => usize::from(
                efergy::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Wh51(channel_hz) if samples >= wh51::MIN_SWEEP_SAMPLES => usize::from(
                wh51::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Ws90(channel_hz) if samples >= ws90::MIN_SWEEP_SAMPLES => usize::from(
                ws90::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Tpms(channel_hz) if samples >= tpms::MIN_SWEEP_SAMPLES => usize::from(
                tpms::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Rs41(channel_hz) if samples >= rs41::MIN_SWEEP_SAMPLES => usize::from(
                rs41::Channel {
                    center_hz,
                    channel_hz,
                    sample_rate_hz: SAMPLE_RATE,
                }
                .is_covered(),
            ),
            Self::Rs41RasterSource(bounds) | Self::Rs41Raster(bounds)
                if samples >= rs41::MIN_SWEEP_SAMPLES =>
            {
                let grid = rs41::raster::Grid::new(center_hz, bounds.lower_hz, bounds.upper_hz)?;
                return Ok(if matches!(self, Self::Rs41RasterSource(_)) {
                    samples * u64::from(!grid.channels_hz().is_empty())
                } else {
                    grid.channelized_samples(samples)
                });
            }
            _ => 0,
        };
        Ok(samples * attempts as u64)
    }
}

fn lte_samples(center_hz: u64, range: LteRange, samples: u64) -> Result<u64> {
    if samples < lte::MIN_SWEEP_SAMPLES {
        return Ok(0);
    }
    let Some((first, last)) = lte::covered_centers(center_hz, range.lower_hz, range.upper_hz)
    else {
        return Ok(0);
    };
    let carriers = (last - first) / lte::GRID_SPACING_HZ + 1;
    let channel_samples = lte::channelized_count(samples) as u64 * carriers;
    ensure!(
        channel_samples <= lte::MAX_CHANNELIZED_SAMPLES as u64,
        "LTE sweep needs {channel_samples} channelized samples, exceeding the {}-sample per-dwell limit",
        lte::MAX_CHANNELIZED_SAMPLES
    );
    Ok(channel_samples)
}

fn nr_samples(center_hz: u64, profiles: &[nr::Profile], raster: bool, samples: u64) -> Result<u64> {
    if samples < nr::MIN_DWELL_SAMPLES {
        return Ok(0);
    }
    let half_span_hz = USABLE_HALF_SPAN_HZ as u64;
    let start = profiles.partition_point(|profile| profile.ss_ref_hz() < center_hz - half_span_hz);
    let end = profiles.partition_point(|profile| profile.ss_ref_hz() <= center_hz + half_span_hz);
    let mut total_samples = 0;
    let mut largest_profile_samples = 0;
    for profile in profiles[start..end]
        .iter()
        .filter(|profile| profile.covered_by(center_hz))
    {
        let channel_samples = profile.channelized_count(samples) as u64;
        total_samples += channel_samples;
        largest_profile_samples = largest_profile_samples.max(channel_samples);
    }
    // The raster reuses one channelized buffer while explicit profiles can
    // run concurrently. Both paths charge every selected profile's work.
    let buffered_samples = if raster {
        largest_profile_samples
    } else {
        total_samples
    };
    ensure!(
        buffered_samples <= nr::MAX_DWELL_CHANNEL_SAMPLES,
        "NR sweep needs {buffered_samples} channelized samples, exceeding the {}-sample per-dwell limit",
        nr::MAX_DWELL_CHANNEL_SAMPLES
    );
    Ok(total_samples)
}

fn nr_profiles(profile: &SignalProfile) -> Result<Vec<nr::Profile>> {
    let Some(bounds) = profile.nr_raster_hz else {
        return Ok(profile.nr_profile.clone());
    };
    let spacings_hz = if profile.nr_scs_hz.is_empty() {
        &nr::raster::SPACINGS_HZ
    } else {
        profile.nr_scs_hz.as_slice()
    };
    Ok(nr::raster::entries(bounds, spacings_hz)?
        .into_iter()
        .map(|entry| entry.profile)
        .collect())
}

/// Bound replay work before tuning, using the nominal center and saved
/// sample count of each planned dwell in one pass.
///
/// Each analyzer gets the sum of its per-dwell maxima over every integer
/// readback allowed by admission. The maxima of different analyzers may
/// occur at different readbacks. The final replay uses the observed geometry.
///
/// # Errors
/// Returns an error for invalid tuning geometry or if any allowed readback
/// could exceed a per-dwell or aggregate replay budget.
pub(crate) fn bound(
    profile: &SignalProfile,
    limits: ReplayLimits,
    dwells: impl IntoIterator<Item = (u64, u64)>,
    passes: usize,
) -> Result<Preflight> {
    let mut nr_profiles = nr_profiles(profile)?;
    nr_profiles.sort_unstable();
    nr_profiles.dedup();
    let raster = profile.nr_raster_hz.is_some();
    let mut analyzers: Vec<_> = [
        (!profile.aprs_channel_hz.is_empty()).then_some(Analyzer::Aprs(&profile.aprs_channel_hz)),
        (!profile.ais_channel_hz.is_empty())
            .then_some(Analyzer::Channels(Protocol::Ais, &profile.ais_channel_hz)),
        (!profile.dmr_channel_hz.is_empty())
            .then_some(Analyzer::Channels(Protocol::Dmr, &profile.dmr_channel_hz)),
        profile.dmr_raster_hz.map(Analyzer::DmrRasterSource),
        profile.dmr_raster_hz.map(Analyzer::DmrRaster),
        (!profile.dect_channel_hz.is_empty())
            .then_some(Analyzer::Channels(Protocol::Dect, &profile.dect_channel_hz)),
        (!profile.halow_channel_hz.is_empty()).then_some(Analyzer::Channels(
            Protocol::Halow,
            &profile.halow_channel_hz,
        )),
        (!profile.scm_channel_hz.is_empty())
            .then_some(Analyzer::Channels(Protocol::Scm, &profile.scm_channel_hz)),
        (!profile.scm_plus_channel_hz.is_empty()).then_some(Analyzer::Channels(
            Protocol::ScmPlus,
            &profile.scm_plus_channel_hz,
        )),
        profile.adsb.then_some(Analyzer::Adsb),
        profile
            .uat
            .then_some(Analyzer::Channels(Protocol::Uat, &[uat::CHANNEL_HZ])),
        (!profile.lora_profile.is_empty()).then_some(Analyzer::Lora(&profile.lora_profile)),
        profile.lte_range_hz.map(Analyzer::Lte),
        (raster || !nr_profiles.is_empty()).then_some(Analyzer::Nr {
            profiles: &nr_profiles,
            raster,
        }),
        (!profile.rds_channel_hz.is_empty()).then_some(Analyzer::Rds(&profile.rds_channel_hz)),
        (!profile.morse_profile.is_empty()).then_some(Analyzer::Morse(&profile.morse_profile)),
        profile.ook_channel_hz.map(Analyzer::Ook),
        profile.efergy_channel_hz.map(Analyzer::Efergy),
        profile.wh51_channel_hz.map(Analyzer::Wh51),
        profile.ws90_channel_hz.map(Analyzer::Ws90),
        profile.tpms_channel_hz.map(Analyzer::Tpms),
        profile.rs41_channel_hz.map(Analyzer::Rs41),
        profile.rs41_raster_hz.map(Analyzer::Rs41RasterSource),
        profile.rs41_raster_hz.map(Analyzer::Rs41Raster),
    ]
    .into_iter()
    .flatten()
    .map(|analyzer| (analyzer, analyzer.budget(&limits)))
    .collect();
    let tolerance = ACQUISITION_CENTER_TOLERANCE_HZ;
    let passes = u64::try_from(passes)?;
    for (center_hz, saved_samples) in dwells {
        ensure!(
            (MIN_CENTER_HZ..=MAX_CENTER_HZ).contains(&center_hz),
            "invalid nominal replay center {center_hz} Hz"
        );
        let prefixes = Prefixes::new(saved_samples, &limits);
        for (analyzer, budget) in &mut analyzers {
            let mut maximum = 0;
            for observed_hz in center_hz - tolerance..=center_hz + tolerance {
                let samples = analyzer.samples(observed_hz, &prefixes).with_context(|| {
                    format!(
                        "preflight {} dwell at {center_hz} Hz with readback {observed_hz} Hz",
                        budget.analyzer
                    )
                })?;
                maximum = maximum.max(samples);
            }
            let repeated = maximum
                .checked_mul(passes)
                .context("preflight pass count exceeds replay accounting")?;
            budget.maximum_budget_samples = budget
                .maximum_budget_samples
                .checked_add(repeated)
                .context("preflight dwell sum exceeds replay accounting")?;
        }
    }
    let analyzers: Vec<_> = analyzers.into_iter().map(|(_, budget)| budget).collect();
    for budget in &analyzers {
        let limit_option = match budget.budget_unit {
            BudgetUnit::Iq => "--max-total-samples",
            BudgetUnit::Profile => "--max-total-profile-samples",
            BudgetUnit::Station => "--max-total-station-samples",
            BudgetUnit::Channelized => "--max-total-channelized-samples",
        };
        ensure!(
            budget.maximum_budget_samples <= budget.max_budget_samples,
            "{} sweep replay can need up to {} budget samples, exceeding {limit_option} ({})",
            budget.analyzer,
            budget.maximum_budget_samples,
            budget.max_budget_samples
        );
    }
    Ok(Preflight {
        center_readback_tolerance_hz: tolerance,
        analyzers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_profile() -> SignalProfile {
        SignalProfile {
            sha256: String::new(),
            aprs_channel_hz: Vec::new(),
            ais_channel_hz: Vec::new(),
            dmr_channel_hz: Vec::new(),
            dmr_raster_hz: None,
            dect_channel_hz: Vec::new(),
            halow_channel_hz: Vec::new(),
            scm_channel_hz: Vec::new(),
            scm_plus_channel_hz: Vec::new(),
            adsb: false,
            uat: false,
            lora_profile: Vec::new(),
            lora_packet_profile: Vec::new(),
            packet_interpretations: Default::default(),
            lte_range_hz: None,
            lte_pbch: false,
            nr_profile: Vec::new(),
            nr_raster_hz: None,
            nr_scs_hz: Vec::new(),
            nr_pbch: false,
            rds_channel_hz: Vec::new(),
            morse_profile: Vec::new(),
            ook_channel_hz: None,
            ook_govee_h5054: false,
            ook_oregon_scientific: false,
            efergy_channel_hz: None,
            wh51_channel_hz: None,
            ws90_channel_hz: None,
            tpms_channel_hz: None,
            rs41_channel_hz: None,
            rs41_raster_hz: None,
        }
    }

    #[test]
    fn lte_carrier_outside_nominal_reach_still_consumes_budget() -> Result<()> {
        let mut profile = empty_profile();
        let range = LteRange::parse("908000000:908000000")?;
        profile.lte_range_hz = Some(range);
        let center_hz = 915_000_001;
        let samples = 800_000;
        assert_eq!(lte_samples(center_hz, range, samples)?, 0);
        let expected = lte::channelized_count(samples) as u64;
        let limits = ReplayLimits::default();
        let preflight = bound(&profile, limits, [(center_hz, samples)], 2)?;
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 2 * expected);
        Ok(())
    }

    #[test]
    fn ook_charges_guard_edge_readback_and_skips_short_dwells() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = ook::DEFAULT_CHANNEL_HZ;
        profile.ook_channel_hz = Some(channel_hz);
        let guard =
            crate::spectrum::CENTER_GUARD_HZ as u64 + crate::dsp::SensorChannel::HALF_WIDTH_HZ;
        let center_hz = channel_hz + guard - 1;
        let samples = ook::MIN_SWEEP_SAMPLES;
        let limits = ReplayLimits::default();
        let analyzer = Analyzer::Ook(channel_hz);
        let prefixes = Prefixes::new(samples, &limits);
        assert_eq!(analyzer.samples(center_hz, &prefixes)?, 0);
        assert_eq!(analyzer.samples(center_hz + 1, &prefixes)?, samples);
        let dwells = [(center_hz, samples - 1), (center_hz, samples)];
        let preflight = bound(&profile, limits, dwells, 3)?;
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 3 * samples);
        let short_budget = ReplayLimits {
            max_total_samples: 3 * samples - 1,
            ..limits
        };
        assert!(bound(&profile, short_budget, dwells, 3).is_err());
        Ok(())
    }

    #[test]
    fn dmr_charges_each_covered_channel_only_at_the_frame_floor() -> Result<()> {
        let mut profile = empty_profile();
        let center_hz = 440_000_000;
        profile.dmr_channel_hz = vec![center_hz, 444_000_000, 444_012_500];
        let floor = Protocol::Dmr.minimum_frame_samples();
        let limits = ReplayLimits {
            max_total_samples: 4 * floor,
            ..ReplayLimits::default()
        };
        let dwells = [(center_hz, floor - 1), (center_hz, floor)];
        let checked = bound(&profile, limits, dwells, 2)?;
        assert_eq!(checked.analyzers.len(), 1);
        assert_eq!(checked.analyzers[0].analyzer, "dmr");
        assert_eq!(checked.analyzers[0].maximum_budget_samples, 4 * floor);
        let short_budget = ReplayLimits {
            max_total_samples: limits.max_total_samples - 1,
            ..limits
        };
        assert!(bound(&profile, short_budget, dwells, 2).is_err());
        let short_prefix = ReplayLimits {
            max_samples_per_dwell: Some(floor - 1),
            ..short_budget
        };
        let checked = bound(&profile, short_prefix, dwells, 2)?;
        assert_eq!(checked.analyzers[0].maximum_budget_samples, 0);
        Ok(())
    }

    #[test]
    fn scm_formats_charge_independent_budgets_on_one_carrier() -> Result<()> {
        let mut profile = empty_profile();
        let center_hz = 910_000_000;
        let channel_hz = 914_000_000;
        profile.scm_channel_hz = vec![channel_hz];
        profile.scm_plus_channel_hz = vec![channel_hz];
        let samples = Protocol::Scm
            .minimum_frame_samples()
            .max(Protocol::ScmPlus.minimum_frame_samples());
        let limits = ReplayLimits {
            max_total_samples: samples,
            ..ReplayLimits::default()
        };
        let preflight = bound(&profile, limits, [(center_hz, samples)], 1)?;
        let analyzers: Vec<_> = preflight
            .analyzers
            .iter()
            .map(|budget| (budget.analyzer, budget.maximum_budget_samples))
            .collect();
        assert_eq!(analyzers, [("scm", samples), ("scm_plus", samples)]);
        Ok(())
    }

    #[test]
    fn dmr_raster_bounds_source_and_channels_independently_at_guard_edge() -> Result<()> {
        let lower_hz = 444_000_000;
        let upper_hz = lower_hz + dmr::raster::GRID_SPACING_HZ;
        let bounds = dmr::raster::Bounds { lower_hz, upper_hz };
        let mut profile = empty_profile();
        profile.dmr_raster_hz = Some(bounds);

        let guard = crate::spectrum::CENTER_GUARD_HZ as u64 + dmr::CHANNEL_HALF_WIDTH_HZ;
        let center_hz = upper_hz + guard - ACQUISITION_CENTER_TOLERANCE_HZ;
        let floor = dmr::MIN_SWEEP_SAMPLES;
        let nominal = dmr::raster::Grid::new(center_hz, lower_hz, upper_hz)?;
        assert_eq!(nominal.channel_count(), 1);
        let edge = dmr::raster::Grid::new(upper_hz + guard, lower_hz, upper_hz)?;
        assert_eq!(edge.channel_count(), 2);

        let limits = ReplayLimits {
            max_samples_per_dwell: Some(floor),
            max_total_samples: 2 * floor,
            max_total_channelized_samples: 2 * edge.channelized_samples(floor),
            ..ReplayLimits::default()
        };
        let dwells = [
            (lower_hz, floor),
            (center_hz, floor - 1),
            (center_hz, floor + 1),
        ];
        let checked = bound(&profile, limits, dwells, 2)?;
        let [source, raster] = checked.analyzers.as_slice() else {
            panic!("expected source and channelized DMR raster budgets");
        };
        assert_eq!(source.analyzer, "dmr_raster_source");
        assert!(matches!(source.budget_unit, BudgetUnit::Iq));
        assert_eq!(source.maximum_budget_samples, limits.max_total_samples);
        assert_eq!(raster.analyzer, "dmr_raster");
        assert!(matches!(raster.budget_unit, BudgetUnit::Channelized));
        assert_eq!(
            raster.maximum_budget_samples,
            limits.max_total_channelized_samples
        );

        let short_source = ReplayLimits {
            max_total_samples: limits.max_total_samples - 1,
            ..limits
        };
        let short_channels = ReplayLimits {
            max_total_channelized_samples: limits.max_total_channelized_samples - 1,
            ..limits
        };
        assert!(
            bound(&profile, short_source, dwells, 2)
                .unwrap_err()
                .to_string()
                .contains("--max-total-samples")
        );
        assert!(
            bound(&profile, short_channels, dwells, 2)
                .unwrap_err()
                .to_string()
                .contains("--max-total-channelized-samples")
        );
        Ok(())
    }

    #[test]
    fn dect_charges_frame_floor_routes_on_both_sides_of_the_guard() -> Result<()> {
        let mut profile = empty_profile();
        let center_hz = 1_922_400_000;
        profile.dect_channel_hz = vec![1_921_536_000, center_hz, 1_923_264_000];
        let floor = Protocol::Dect.minimum_frame_samples();
        let limits = ReplayLimits {
            max_total_samples: 2 * floor,
            ..ReplayLimits::default()
        };
        let dwells = [(center_hz, floor - 1), (center_hz, floor)];
        let checked = bound(&profile, limits, dwells, 1)?;
        assert_eq!(checked.analyzers.len(), 1);
        assert_eq!(checked.analyzers[0].analyzer, "dect");
        assert_eq!(checked.analyzers[0].maximum_budget_samples, 2 * floor);
        let short_budget = ReplayLimits {
            max_total_samples: limits.max_total_samples - 1,
            ..limits
        };
        assert!(bound(&profile, short_budget, dwells, 1).is_err());
        Ok(())
    }

    #[test]
    fn efergy_charges_guard_edge_readback_at_the_frame_floor() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = efergy::DEFAULT_CHANNEL_HZ;
        profile.efergy_channel_hz = Some(channel_hz);
        let guard = crate::spectrum::CENTER_GUARD_HZ as u64 + efergy::CHANNEL_HALF_WIDTH_HZ;
        let center_hz = channel_hz + guard - ACQUISITION_CENTER_TOLERANCE_HZ;
        let floor = efergy::MIN_SWEEP_SAMPLES;
        let dwells = [
            (center_hz - 1, floor),
            (center_hz, floor - 1),
            (center_hz, floor),
        ];
        let limits = ReplayLimits {
            max_total_samples: 2 * floor,
            ..ReplayLimits::default()
        };
        let preflight = bound(&profile, limits, dwells, 2)?;
        assert_eq!(preflight.analyzers.len(), 1);
        assert_eq!(preflight.analyzers[0].analyzer, "efergy");
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 2 * floor);
        let insufficient = ReplayLimits {
            max_total_samples: 2 * floor - 1,
            ..limits
        };
        let error = bound(&profile, insufficient, dwells, 2).unwrap_err();
        assert!(error.to_string().contains("efergy sweep replay"));
        Ok(())
    }

    #[test]
    fn wh51_charges_the_shortest_admitted_frame_prefix() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = wh51::DEFAULT_CHANNEL_HZ;
        profile.wh51_channel_hz = Some(channel_hz);
        let center_hz = channel_hz - 2_000_000;
        let floor = wh51::MIN_SWEEP_SAMPLES;
        let dwells = [(center_hz, floor - 1), (center_hz, floor)];
        let preflight = bound(&profile, ReplayLimits::default(), dwells, 1)?;
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, floor);
        Ok(())
    }

    #[test]
    fn ws90_charges_readback_coverage_at_the_frame_floor() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = ws90::DEFAULT_CHANNEL_HZ;
        profile.ws90_channel_hz = Some(channel_hz);
        let guard =
            crate::spectrum::CENTER_GUARD_HZ as u64 + crate::dsp::SensorChannel::HALF_WIDTH_HZ;
        let center_hz = channel_hz + guard - ACQUISITION_CENTER_TOLERANCE_HZ;
        let floor = ws90::MIN_SWEEP_SAMPLES;
        let dwells = [
            (center_hz - 1, floor),
            (center_hz, floor - 1),
            (center_hz, floor),
        ];
        let limits = ReplayLimits::default();
        let preflight = bound(&profile, limits, dwells, 2)?;
        assert_eq!(preflight.analyzers[0].analyzer, "ws90");
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 2 * floor);
        let short_budget = ReplayLimits {
            max_total_samples: 2 * floor - 1,
            ..limits
        };
        assert!(bound(&profile, short_budget, dwells, 2).is_err());
        Ok(())
    }

    #[test]
    fn tpms_charges_the_guard_edge_readback_only_at_the_sample_floor() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = tpms::DEFAULT_CHANNEL_HZ;
        profile.tpms_channel_hz = Some(channel_hz);
        let guard =
            crate::spectrum::CENTER_GUARD_HZ as u64 + crate::dsp::SensorChannel::HALF_WIDTH_HZ;
        let center_hz = channel_hz + guard - ACQUISITION_CENTER_TOLERANCE_HZ;
        let floor = tpms::MIN_SWEEP_SAMPLES;
        let limits = ReplayLimits::default();
        let dwells = [
            (center_hz - 1, floor),
            (center_hz, floor - 1),
            (center_hz, floor),
        ];
        let preflight = bound(&profile, limits, dwells, 2)?;
        assert_eq!(preflight.analyzers[0].analyzer, "tpms");
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 2 * floor);
        let short_budget = ReplayLimits {
            max_total_samples: 2 * floor - 1,
            ..limits
        };
        assert!(bound(&profile, short_budget, dwells, 2).is_err());
        Ok(())
    }

    #[test]
    fn rs41_charges_only_covered_dwells_at_its_sample_floor() -> Result<()> {
        let mut profile = empty_profile();
        let channel_hz = 404_200_000;
        profile.rs41_channel_hz = Some(channel_hz);
        let center_hz = channel_hz + 2_000_000;
        let floor = rs41::MIN_SWEEP_SAMPLES;
        let dwells = [
            (channel_hz, floor),
            (center_hz, floor - 1),
            (center_hz, floor),
        ];
        let preflight = bound(&profile, ReplayLimits::default(), dwells, 2)?;
        assert_eq!(preflight.analyzers[0].analyzer, "rs41");
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, 2 * floor);
        Ok(())
    }

    #[test]
    fn rs41_raster_bounds_source_and_channels_independently_at_guard_edge() -> Result<()> {
        let lower_hz = 404_200_000;
        let upper_hz = lower_hz + rs41::raster::GRID_SPACING_HZ;
        let mut profile = empty_profile();
        profile.rs41_raster_hz = Some(rs41::raster::Bounds::new(lower_hz, upper_hz)?);

        let guard = crate::spectrum::CENTER_GUARD_HZ as u64 + rs41::CHANNEL_HALF_WIDTH_HZ;
        let center_hz = upper_hz + guard - ACQUISITION_CENTER_TOLERANCE_HZ;
        let floor = rs41::MIN_SWEEP_SAMPLES;
        let nominal = rs41::raster::Grid::new(center_hz, lower_hz, upper_hz)?;
        assert_eq!(nominal.channels_hz(), [lower_hz]);
        let edge = rs41::raster::Grid::new(upper_hz + guard, lower_hz, upper_hz)?;
        assert_eq!(edge.channels_hz(), [lower_hz, upper_hz]);

        let limits = ReplayLimits {
            max_samples_per_dwell: Some(floor),
            max_total_samples: 2 * floor,
            max_total_channelized_samples: 2 * edge.channelized_samples(floor),
            ..ReplayLimits::default()
        };
        let dwells = [
            (lower_hz, floor),
            (center_hz, floor - 1),
            (center_hz, floor + 1),
        ];
        let checked = bound(&profile, limits, dwells, 2)?;
        let [source, raster] = checked.analyzers.as_slice() else {
            panic!("expected source and channelized RS41 raster budgets");
        };
        assert_eq!(source.analyzer, "rs41_raster_source");
        assert!(matches!(source.budget_unit, BudgetUnit::Iq));
        assert_eq!(source.maximum_budget_samples, limits.max_total_samples);
        assert_eq!(raster.analyzer, "rs41_raster");
        assert!(matches!(raster.budget_unit, BudgetUnit::Channelized));
        assert_eq!(
            raster.maximum_budget_samples,
            limits.max_total_channelized_samples
        );

        let short_source = ReplayLimits {
            max_total_samples: limits.max_total_samples - 1,
            ..limits
        };
        let short_channels = ReplayLimits {
            max_total_channelized_samples: limits.max_total_channelized_samples - 1,
            ..limits
        };
        assert!(
            bound(&profile, short_source, dwells, 2)
                .unwrap_err()
                .to_string()
                .contains("--max-total-samples")
        );
        assert!(
            bound(&profile, short_channels, dwells, 2)
                .unwrap_err()
                .to_string()
                .contains("--max-total-channelized-samples")
        );
        Ok(())
    }

    #[test]
    fn nr_budget_does_not_combine_mutually_exclusive_guard_edge_routes() -> Result<()> {
        let mut profile = empty_profile();
        // The lower profile fits only below the nominal readback and the
        // upper profile fits only above it.
        profile.nr_profile = vec![
            nr::Profile::parse("907974999:15")?,
            nr::Profile::parse("922040001:15")?,
        ];
        let center_hz = 915_000_000;
        let samples = 800_000;
        assert_eq!(
            nr_samples(center_hz, &profile.nr_profile, false, samples)?,
            0
        );
        let expected = profile.nr_profile[0].channelized_count(samples) as u64;
        let preflight = bound(&profile, ReplayLimits::default(), [(center_hz, samples)], 1)?;
        assert_eq!(preflight.analyzers[0].maximum_budget_samples, expected);
        Ok(())
    }
}
