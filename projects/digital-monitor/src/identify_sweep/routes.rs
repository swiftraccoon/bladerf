//! Plan decoder routes from validated sweep receipts.

use std::collections::BTreeSet;

use clap::ValueEnum;
use serde::Serialize;

use super::{admission, bluetooth, wifi};

const WIFI_FIVE_GHZ_CHANNELS: [u16; 25] = [
    36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144,
    149, 153, 157, 161, 165,
];
const IEEE_MAX_OFFSET_HZ: u64 = 7_999_999;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum Protocol {
    Wifi,
    Ieee802154,
    BluetoothLe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum ChannelScope {
    Requested,
    Eligible,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct Channel {
    pub(super) protocol: Protocol,
    pub(super) channel_number: u16,
    pub(super) frequency_hz: u64,
}

impl Channel {
    pub(super) fn directory_name(self, tune: &str) -> String {
        let protocol = match self.protocol {
            Protocol::Wifi => "wifi",
            Protocol::Ieee802154 => "ieee802154",
            Protocol::BluetoothLe => return format!("bluetooth-le-{tune}"),
        };
        format!("{protocol}-{}mhz", self.frequency_hz / 1_000_000)
    }

    fn covered_by(self, capture_hz: u64) -> bool {
        let offset = capture_hz.abs_diff(self.frequency_hz);
        match self.protocol {
            Protocol::Wifi => offset <= wifi::MAX_CENTER_OFFSET_HZ,
            Protocol::Ieee802154 => offset <= IEEE_MAX_OFFSET_HZ,
            Protocol::BluetoothLe => bluetooth::eligible(capture_hz, self.frequency_hz),
        }
    }
}

/// A zero-based pass and tune in acquisition order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Visit {
    pub(super) pass: usize,
    pub(super) tune: usize,
}

pub(super) struct Route {
    pub(super) channel: Channel,
    pub(super) tune: Option<Visit>,
}

/// Choose the nearest eligible visit per channel or plan all eligible visits.
/// Routes are channel-major, with eligible visits in acquisition order and
/// equal-offset candidates resolved in favor of the earliest visit.
pub(super) fn routes(
    batch: &admission::Batch,
    protocols: &[Protocol],
    scope: ChannelScope,
    all_visits: bool,
) -> Vec<Route> {
    let reference = &batch.passes[0].sweep.receipt;
    let mut routes = Vec::new();
    for channel in channels().filter(|channel| protocols.contains(&channel.protocol)) {
        if scope == ChannelScope::Requested && !reference.includes_channel(channel.frequency_hz) {
            continue;
        }
        let require_exact_center = scope == ChannelScope::Requested && reference.is_targeted();
        let eligible = batch.passes.iter().enumerate().flat_map(|(pass, saved)| {
            saved
                .sweep
                .receipt
                .tunes
                .iter()
                .enumerate()
                .filter(move |(_, capture)| {
                    (!require_exact_center || capture.requested_center_hz == channel.frequency_hz)
                        && channel.covered_by(capture.observed_center_hz)
                })
                .map(move |(tune, capture)| {
                    (
                        capture.observed_center_hz.abs_diff(channel.frequency_hz),
                        Visit { pass, tune },
                    )
                })
        });
        let selected = |visit| Route {
            channel,
            tune: Some(visit),
        };
        let before = routes.len();
        if all_visits {
            routes.extend(eligible.map(|(_, visit)| selected(visit)));
        } else if let Some((_, visit)) = eligible.min() {
            routes.push(selected(visit));
        }
        if scope == ChannelScope::Requested && routes.len() == before {
            routes.push(Route {
                channel,
                tune: None,
            });
        }
    }
    routes
}

/// Count decoder runs, sharing one Bluetooth LE run across a visit's channels.
pub(super) fn count_attempts(routes: &[Route]) -> usize {
    let mut bluetooth_visits = BTreeSet::new();
    routes
        .iter()
        .filter(|route| {
            route.tune.is_some_and(|visit| {
                route.channel.protocol != Protocol::BluetoothLe || bluetooth_visits.insert(visit)
            })
        })
        .count()
}

fn channels() -> impl Iterator<Item = Channel> {
    let wifi_channels = (1..=13)
        .map(|channel| (channel, 2407 + 5 * u64::from(channel)))
        .chain(
            WIFI_FIVE_GHZ_CHANNELS
                .into_iter()
                .map(|channel| (channel, 5000 + 5 * u64::from(channel))),
        )
        .map(|(channel, mhz)| Channel {
            protocol: Protocol::Wifi,
            channel_number: channel,
            frequency_hz: mhz * 1_000_000,
        });
    let ieee = (11..=26).map(|channel| Channel {
        protocol: Protocol::Ieee802154,
        channel_number: channel,
        frequency_hz: (2405 + 5 * u64::from(channel - 11)) * 1_000_000,
    });
    let bluetooth = bluetooth::channels().map(|(channel_number, frequency_hz)| Channel {
        protocol: Protocol::BluetoothLe,
        channel_number,
        frequency_hz,
    });
    wifi_channels.chain(ieee).chain(bluetooth)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::ops::Range;

    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    use super::*;

    fn sweep(
        operation: &str,
        bounds: Range<u64>,
        centers: &[(u64, u64)],
    ) -> (TempDir, admission::Batch) {
        let directory = tempdir().unwrap();
        let tunes: Vec<_> = centers
            .iter()
            .enumerate()
            .map(|(index, &(requested, observed))| {
                json!({
                    "directory": format!("tune-{:04}", index + 1),
                    "requested_center_hz": requested,
                    "observed_center_hz": observed,
                    "observed_bands": [
                        { "lower_hz": observed - 8_000_000, "upper_hz": observed - 100_000 },
                        { "lower_hz": observed + 100_000, "upper_hz": observed + 8_000_000 }
                    ]
                })
            })
            .collect();
        let receipt = json!({
            "schema_version": 1,
            "operation": operation,
            "lower_hz": bounds.start,
            "upper_hz": bounds.end,
            "dwell_ms": 1,
            "iq_bytes": tunes.len() * 80_000,
            "visit_centers_hz": centers.iter().map(|&(requested, _)| requested).collect::<Vec<_>>(),
            "tunes": tunes,
        });
        fs::write(
            directory.path().join("sweep.json"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let batch = admission::Batch::open(directory.path(), false).unwrap();
        (directory, batch)
    }

    #[test]
    fn all_visits_keep_exact_targets_and_nearest_policy_breaks_ties_by_order() {
        let center = 2_412_000_000;
        let neighbor = 2_417_000_000;
        let (_directory, batch) = sweep(
            "targeted_rf_sweep",
            center..neighbor + 1,
            &[
                (center, center + 7),
                (center, center - 3),
                (center, center - 3),
                (neighbor, neighbor),
            ],
        );
        let selected = |scope, all_visits| {
            routes(&batch, &[Protocol::Wifi], scope, all_visits)
                .into_iter()
                .filter(|route| route.channel.frequency_hz == center)
                .map(|route| route.tune.unwrap().tune)
                .collect::<Vec<_>>()
        };
        assert_eq!(selected(ChannelScope::Requested, false), [1]);
        assert_eq!(selected(ChannelScope::Requested, true), [0, 1, 2]);
        assert_eq!(selected(ChannelScope::Eligible, true), [0, 1, 2, 3]);
    }

    #[test]
    fn all_visits_reports_a_missing_requested_channel_once() {
        let channel_hz = 2_412_000_000;
        let capture_hz = channel_hz - 7_800_000;
        let (_directory, batch) = sweep(
            "sequential_rf_sweep",
            channel_hz..channel_hz + 1,
            &[(capture_hz, capture_hz); 2],
        );
        let planned = routes(&batch, &[Protocol::Wifi], ChannelScope::Requested, true);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].channel.frequency_hz, channel_hz);
        assert_eq!(planned[0].tune, None);
        assert_eq!(count_attempts(&planned), 0);
    }

    #[test]
    fn attempt_budget_shares_bluetooth_runs_within_each_pass_and_tune() {
        let routes = [
            (Protocol::Wifi, 1, 2_412_000_000, Some(0)),
            (Protocol::Ieee802154, 12, 2_410_000_000, Some(0)),
            (Protocol::BluetoothLe, 3, 2_410_000_000, Some(0)),
            (Protocol::BluetoothLe, 4, 2_412_000_000, Some(0)),
            (Protocol::BluetoothLe, 4, 2_412_000_000, Some(1)),
            (Protocol::BluetoothLe, 5, 2_414_000_000, None),
        ]
        .map(|(protocol, channel_number, frequency_hz, pass)| Route {
            channel: Channel {
                protocol,
                channel_number,
                frequency_hz,
            },
            tune: pass.map(|pass| Visit { pass, tune: 0 }),
        });
        assert_eq!(count_attempts(&routes), 4);
    }
}
