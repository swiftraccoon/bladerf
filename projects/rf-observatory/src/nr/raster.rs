//! Enumerate the global NR synchronization raster within the tuning range.
//!
//! [3GPP TS 38.104 v16.20.0, Table 5.4.3.1-1][raster] defines the SS reference
//! frequencies and corresponding Global Synchronization Channel Numbers (GSCNs).
//! The supported spacings here describe receiver hypotheses. Operating-band
//! assignments are defined separately in Table 5.4.3.3-1.
//!
//! [raster]: https://www.etsi.org/deliver/etsi_ts/138100_138199/138104/16.20.00_60/ts_138104v162000p.pdf

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use super::Profile;
use crate::{MAX_CENTER_HZ, MIN_CENTER_HZ};

const LOW_STEP_HZ: u64 = 1_200_000;
const LOW_OFFSET_STEP_HZ: u64 = 50_000;
const MID_START_HZ: u64 = 3_000_000_000;
const MID_STEP_HZ: u64 = 1_440_000;
const MID_FIRST_GSCN: u32 = 7_499;
/// Receiver spacings used when a raster request does not select one.
pub(crate) const SPACINGS_HZ: [u32; 2] = [15_000, 30_000];

/// A half-open range of SS reference frequencies in hertz.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Bounds {
    pub(crate) lower_hz: u64,
    pub(crate) upper_hz: u64,
}

impl Bounds {
    /// Parse `LOWER:UPPER` as an inclusive lower and exclusive upper limit.
    ///
    /// # Errors
    /// Rejects malformed, empty, reversed, or out-of-range bounds.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let (lower, upper) = value
            .split_once(':')
            .context("expected LOWER:UPPER in hertz")?;
        let bounds = Self {
            lower_hz: lower.parse().context("invalid NR raster lower frequency")?,
            upper_hz: upper.parse().context("invalid NR raster upper frequency")?,
        };
        bounds.validate()?;
        Ok(bounds)
    }

    fn validate(self) -> Result<()> {
        ensure!(
            self.lower_hz >= MIN_CENTER_HZ
                && self.lower_hz < self.upper_hz
                && self.upper_hz <= MAX_CENTER_HZ + 1,
            "NR raster requires ascending half-open bounds within {MIN_CENTER_HZ}..={} Hz",
            MAX_CENTER_HZ + 1
        );
        Ok(())
    }

    fn contains(self, frequency_hz: u64) -> bool {
        (self.lower_hz..self.upper_hz).contains(&frequency_hz)
    }
}

/// A global synchronization channel and one receiver spacing hypothesis.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Entry {
    pub(crate) gscn: u32,
    pub(crate) profile: Profile,
}

/// Enumerate the SS references within the bounds in frequency and spacing order.
///
/// Repeated spacings yield one entry per reference and spacing. A valid range
/// between raster frequencies yields an empty list.
///
/// # Errors
/// Rejects invalid bounds and empty or unsupported spacing selections.
pub(crate) fn entries(bounds: Bounds, spacings_hz: &[u32]) -> Result<Vec<Entry>> {
    bounds.validate()?;
    ensure!(
        !spacings_hz.is_empty()
            && spacings_hz
                .iter()
                .all(|spacing| SPACINGS_HZ.contains(spacing)),
        "NR raster requires 15 or 30 kHz spacing"
    );
    let selected_spacings = SPACINGS_HZ
        .into_iter()
        .filter(|spacing| spacings_hz.contains(spacing));

    let below_3_ghz = (1..=2_499_u32).flat_map(|n| {
        [(1, 0), (3, 1), (5, 2)].map(move |(m, index)| {
            let frequency_hz = u64::from(n) * LOW_STEP_HZ + m * LOW_OFFSET_STEP_HZ;
            let gscn = 3 * n - 1 + index;
            (frequency_hz, gscn)
        })
    });
    let at_or_above_3_ghz = (0..=14_756_u32).map(|n| {
        (
            MID_START_HZ + u64::from(n) * MID_STEP_HZ,
            MID_FIRST_GSCN + n,
        )
    });
    Ok(below_3_ghz
        .chain(at_or_above_3_ghz)
        .take_while(|&(frequency_hz, _)| frequency_hz < bounds.upper_hz)
        .filter(|&(frequency_hz, _)| bounds.contains(frequency_hz))
        .flat_map(|(ss_ref_hz, gscn)| {
            selected_spacings
                .clone()
                .map(move |subcarrier_spacing_hz| Entry {
                    gscn,
                    profile: Profile {
                        ss_ref_hz,
                        subcarrier_spacing_hz,
                    },
                })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(lower_hz: u64, upper_hz: u64, spacings_hz: &[u32]) -> Vec<(u64, u32, u32)> {
        entries(Bounds { lower_hz, upper_hz }, spacings_hz)
            .unwrap()
            .into_iter()
            .map(|entry| {
                (
                    entry.profile.ss_ref_hz,
                    entry.gscn,
                    entry.profile.subcarrier_spacing_hz,
                )
            })
            .collect()
    }

    #[test]
    fn enumerates_all_three_offsets_and_uses_half_open_bounds() {
        assert_eq!(
            profiles(627_650_000, 628_850_000, &[15_000]),
            [
                (627_650_000, 1_568, 15_000),
                (627_750_000, 1_569, 15_000),
                (627_850_000, 1_570, 15_000),
            ]
        );
        assert!(profiles(627_650_001, 627_750_000, &[15_000]).is_empty());
    }

    #[test]
    fn crosses_three_ghz_without_inventing_gap_entries() {
        assert_eq!(
            profiles(2_999_050_000, 3_002_880_000, &[30_000]),
            [
                (2_999_050_000, 7_498, 30_000),
                (3_000_000_000, 7_499, 30_000),
                (3_001_440_000, 7_500, 30_000),
            ]
        );
    }

    #[test]
    fn covers_the_tuning_range_and_canonicalizes_spacings() {
        let found = profiles(MIN_CENTER_HZ, MAX_CENTER_HZ + 1, &[30_000, 15_000, 30_000]);
        assert_eq!(found.len(), 18_814);
        assert_eq!(found[0], (70_850_000, 176, 15_000));
        assert_eq!(found[1], (70_850_000, 176, 30_000));
        assert_eq!(found[found.len() - 1], (5_999_520_000, 9_582, 30_000));
        assert!(found.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(profiles(MAX_CENTER_HZ, MAX_CENTER_HZ + 1, &[30_000]).is_empty());
    }

    #[test]
    fn rejects_invalid_bounds_and_spacing_selections() {
        assert_eq!(
            Bounds::parse("70000000:6000000001").unwrap(),
            Bounds {
                lower_hz: MIN_CENTER_HZ,
                upper_hz: MAX_CENTER_HZ + 1
            }
        );
        for input in [
            "70000000",
            "70000000:75000000:80000000",
            "70000000:70000000",
            "80000000:70000000",
            "69999999:80000000",
            "70000000:6000000002",
        ] {
            assert!(Bounds::parse(input).is_err(), "{input}");
        }
        let bounds = Bounds {
            lower_hz: MIN_CENTER_HZ,
            upper_hz: MAX_CENTER_HZ + 1,
        };
        assert!(entries(bounds, &[]).is_err());
        assert!(entries(bounds, &[15_000, 60_000]).is_err());
        assert!(
            entries(
                Bounds {
                    lower_hz: 1,
                    ..bounds
                },
                &[15_000]
            )
            .is_err()
        );
    }
}
