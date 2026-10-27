//! Bounded inventories of independently validated Wi-Fi frame observations.
//!
//! Addresses, SSIDs and advertisements are untrusted on-air claims, not device
//! identities or authenticated network configuration. Only transmitter,
//! receiver-address and BSSID roles are retained; bridged payload endpoints are
//! not promoted into radio stations. Timestamp ranges stay within each source
//! because independent files may use unrelated sample-relative clocks.
//!
//! Adding a PCAP is transactional: format, capacity or I/O failure leaves the
//! previous inventory intact. This synchronous operation reads regular files,
//! allocates bounded state and can block on file I/O/CPU work. It opens no radio.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;

use crate::packets::{self, ValidatedPacket};

/// Interpretation of PCAP timestamps, supplied independently of their numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimestampBasis {
    Unknown,
    SampleRelative,
    HostBeforeRxEnablePlusSampleOffset,
    HostUdpReceive,
}

impl TimestampBasis {
    fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::SampleRelative => "sample_relative",
            Self::HostBeforeRxEnablePlusSampleOffset => "host_before_rx_enable_plus_sample_offset",
            Self::HostUdpReceive => "host_udp_receive",
        }
    }
}

/// Tuning is caller context, separate from channels advertised in frame bytes.
#[derive(Clone, Debug)]
pub(crate) struct SourceContext {
    pub(crate) label: String,
    pub(crate) tuned_frequency_hz: Option<u64>,
    pub(crate) timestamp_basis: TimestampBasis,
}

/// State/input caps are errors, never silent eviction or truncated inventories.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Limits {
    pub(crate) max_sources: usize,
    pub(crate) max_networks: usize,
    pub(crate) max_addresses: usize,
    pub(crate) max_variants_per_network: usize,
    pub(crate) max_sightings: usize,
    pub(crate) max_records_per_input: u64,
    pub(crate) max_input_bytes: u64,
    pub(crate) max_metadata_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_sources: 1_024,
            max_networks: 4_096,
            max_addresses: 65_536,
            max_variants_per_network: 16,
            max_sightings: 100_000,
            max_records_per_input: 1_000_000,
            max_input_bytes: 256 * 1024 * 1024,
            max_metadata_bytes: 16 * 1024 * 1024,
        }
    }
}

impl Limits {
    fn validate(self) -> Result<()> {
        ensure!(
            (1..=10_000).contains(&self.max_sources),
            "source limit must be 1..=10000"
        );
        ensure!(
            (1..=65_536).contains(&self.max_networks),
            "network limit must be 1..=65536"
        );
        ensure!(
            (1..=1_000_000).contains(&self.max_addresses),
            "address limit must be 1..=1000000"
        );
        ensure!(
            (1..=256).contains(&self.max_variants_per_network),
            "variant limit must be 1..=256"
        );
        ensure!(
            (1..=1_000_000).contains(&self.max_sightings),
            "sighting limit must be 1..=1000000"
        );
        ensure!(
            (1..=10_000_000).contains(&self.max_records_per_input),
            "record limit must be 1..=10000000"
        );
        ensure!(
            (24..=4 * 1024 * 1024 * 1024).contains(&self.max_input_bytes),
            "input byte limit must be 24 bytes through 4 GiB"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&self.max_metadata_bytes),
            "metadata byte limit must be 1 byte through 64 MiB"
        );
        Ok(())
    }
}

/// Bounds apply to validated packets only, including out-of-order timestamps.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct Seen {
    pub(crate) first_timestamp_ns: u64,
    pub(crate) last_timestamp_ns: u64,
    pub(crate) frames: u64,
}

impl Seen {
    fn observe(&mut self, timestamp_ns: u64) {
        self.first_timestamp_ns = self.first_timestamp_ns.min(timestamp_ns);
        self.last_timestamp_ns = self.last_timestamp_ns.max(timestamp_ns);
        self.frames += 1;
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SourceSummary {
    pub(crate) source_id: usize,
    pub(crate) label: String,
    pub(crate) input: String,
    pub(crate) tuned_frequency_hz: Option<u64>,
    pub(crate) timestamp_basis: TimestampBasis,
    pub(crate) first_timestamp_ns: Option<u64>,
    pub(crate) last_timestamp_ns: Option<u64>,
    pub(crate) summary: packets::Summary,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Ssid {
    pub(crate) bytes_hex: String,
    pub(crate) utf8: Option<String>,
    pub(crate) hidden_or_empty: bool,
    pub(crate) frames: u64,
}

/// Raw capability bits plus interpreted security claims from one advertisement.
/// An absent security IE is not by itself proof of an open network.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Advertisement {
    pub(crate) capability_bits: u16,
    pub(crate) privacy_capability: bool,
    pub(crate) ie_layout_complete: bool,
    pub(crate) security_malformed: bool,
    pub(crate) security: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Network {
    pub(crate) bssid: String,
    pub(crate) evidence: BTreeSet<String>,
    pub(crate) frames: u64,
    pub(crate) protected_frames: u64,
    pub(crate) frame_types: BTreeMap<String, u64>,
    pub(crate) ssids: BTreeMap<String, Ssid>,
    pub(crate) advertisements: Vec<Advertisement>,
    pub(crate) advertised_features: BTreeSet<String>,
    pub(crate) advertised_channels: BTreeSet<u8>,
    pub(crate) reported_frequencies_mhz: BTreeSet<u16>,
    pub(crate) per_source: BTreeMap<usize, Seen>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ObservedAddress {
    pub(crate) address: String,
    pub(crate) locally_administered: bool,
    pub(crate) roles: BTreeSet<String>,
    pub(crate) frames: u64,
    pub(crate) transmitter_frames: u64,
    pub(crate) receiver_address_frames: u64,
    pub(crate) bssid_reference_frames: u64,
    pub(crate) per_source: BTreeMap<usize, Seen>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) schema_version: u8,
    pub(crate) interpretation: &'static str,
    pub(crate) limits: Limits,
    pub(crate) metadata_bytes: usize,
    pub(crate) sources: Vec<SourceSummary>,
    pub(crate) networks: BTreeMap<String, Network>,
    pub(crate) observed_addresses: BTreeMap<String, ObservedAddress>,
}

#[derive(Clone, Debug)]
pub(crate) struct Inventory {
    report: Report,
    sightings: usize,
}

impl Inventory {
    pub(crate) fn new(limits: Limits) -> Result<Self> {
        limits.validate()?;
        Ok(Self {
            report: Report {
                schema_version: 1,
                interpretation: "observed frame claims; addresses are not physical identities; per-source PCAP timestamps are not certified RF time; frame counts do not establish channel occupancy or completeness",
                limits,
                metadata_bytes: 0,
                sources: Vec::new(),
                networks: BTreeMap::new(),
                observed_addresses: BTreeMap::new(),
            },
            sightings: 0,
        })
    }

    pub(crate) fn report(&self) -> &Report {
        &self.report
    }

    /// Revalidate and aggregate one regular PCAP. Failed inputs do not mutate
    /// prior observations; duplicate inputs are rejected by canonical identity.
    ///
    /// # Errors
    ///
    /// Returns errors for unsupported/truncated PCAP, invalid source context,
    /// duplicate input, exceeded input/state limits, or I/O/serialization failure.
    pub(crate) fn add_pcap(
        &mut self,
        input: &Path,
        context: SourceContext,
    ) -> Result<SourceSummary> {
        ensure!(
            !context.label.is_empty() && context.label.len() <= 256,
            "source label must contain 1..=256 bytes"
        );
        ensure!(
            context.tuned_frequency_hz.is_none_or(|hz| hz > 0),
            "tuned frequency must be positive when supplied"
        );
        ensure!(
            self.report.sources.len() < self.report.limits.max_sources,
            "inventory source limit exceeded"
        );
        let input = input.canonicalize().context("resolve inventory PCAP")?;
        let identity = input.to_string_lossy().into_owned();
        ensure!(identity.len() <= 4096, "input path exceeds 4096 bytes");
        ensure!(
            !self
                .report
                .sources
                .iter()
                .any(|source| source.input == identity),
            "duplicate inventory input"
        );
        let source_id = self.report.sources.len();
        let mut staged = self.clone();
        let context_bytes = identity.len() + context.label.len();
        staged.reserve_metadata(context_bytes)?;
        let mut bounds: Option<Seen> = None;
        let summary = packets::visit_validated(
            &input,
            self.report.limits.max_records_per_input,
            self.report.limits.max_input_bytes,
            |packet| {
                if let Some(bounds) = &mut bounds {
                    bounds.observe(packet.timestamp_ns);
                } else {
                    bounds = Some(Seen {
                        first_timestamp_ns: packet.timestamp_ns,
                        last_timestamp_ns: packet.timestamp_ns,
                        frames: 1,
                    });
                }
                staged.observe(source_id, packet)
            },
        )?;
        let source = SourceSummary {
            source_id,
            label: context.label,
            input: identity,
            tuned_frequency_hz: context.tuned_frequency_hz,
            timestamp_basis: context.timestamp_basis,
            first_timestamp_ns: bounds.as_ref().map(|seen| seen.first_timestamp_ns),
            last_timestamp_ns: bounds.as_ref().map(|seen| seen.last_timestamp_ns),
            summary,
        };
        // Include bounded per-source counter maps in the retained metadata
        // budget too; many distinct radiotap frequencies must not multiply
        // across all sources without consuming the global capacity.
        let source_bytes = serde_json::to_vec(&source)?.len();
        staged.reserve_metadata(
            source_bytes
                .checked_sub(context_bytes)
                .context("serialized source metadata is shorter than its text")?,
        )?;
        staged.report.sources.push(source.clone());
        *self = staged;
        Ok(source)
    }

    fn reserve_metadata(&mut self, bytes: usize) -> Result<()> {
        self.report.metadata_bytes = self
            .report
            .metadata_bytes
            .checked_add(bytes)
            .context("inventory metadata byte count overflow")?;
        ensure!(
            self.report.metadata_bytes <= self.report.limits.max_metadata_bytes,
            "inventory metadata byte limit exceeded"
        );
        Ok(())
    }

    fn observe(&mut self, source_id: usize, packet: ValidatedPacket<'_>) -> Result<()> {
        let frame = packet.frame;
        let mut roles: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (field, role) in [
            ("transmitter", "transmitter"),
            ("receiver", "receiver_address"),
            ("bssid", "bssid_reference"),
        ] {
            if let Some(mac) = frame[field].as_str().filter(|mac| unicast(mac)) {
                roles.entry(mac).or_default().insert(role);
            }
        }
        let bssid = frame["bssid"].as_str().filter(|mac| unicast(mac));
        if let Some(bssid) = bssid {
            if frame["type"] == 2
                && frame["to_ds"] == true
                && frame["from_ds"] == false
                && let Some(mac) = frame["transmitter"]
                    .as_str()
                    .filter(|mac| unicast(mac) && *mac != bssid)
            {
                roles
                    .entry(mac)
                    .or_default()
                    .insert("to_ds_station_transmitter");
            }
            if frame["type"] == 2
                && frame["from_ds"] == true
                && frame["to_ds"] == false
                && let Some(mac) = frame["receiver"]
                    .as_str()
                    .filter(|mac| unicast(mac) && *mac != bssid)
            {
                roles
                    .entry(mac)
                    .or_default()
                    .insert("from_ds_station_recipient_address");
            }
        }
        for (mac, observed_roles) in roles {
            if !self.report.observed_addresses.contains_key(mac) {
                ensure!(
                    self.report.observed_addresses.len() < self.report.limits.max_addresses,
                    "inventory address limit exceeded"
                );
                self.report.observed_addresses.insert(
                    mac.to_owned(),
                    ObservedAddress {
                        address: mac.to_owned(),
                        locally_administered: u8::from_str_radix(&mac[..2], 16)? & 2 != 0,
                        roles: BTreeSet::new(),
                        frames: 0,
                        transmitter_frames: 0,
                        receiver_address_frames: 0,
                        bssid_reference_frames: 0,
                        per_source: BTreeMap::new(),
                    },
                );
            }
            let address = self
                .report
                .observed_addresses
                .get_mut(mac)
                .context("missing staged address")?;
            address.frames += 1;
            address.transmitter_frames += u64::from(observed_roles.contains("transmitter"));
            address.receiver_address_frames +=
                u64::from(observed_roles.contains("receiver_address"));
            address.bssid_reference_frames += u64::from(observed_roles.contains("bssid_reference"));
            address
                .roles
                .extend(observed_roles.into_iter().map(str::to_owned));
            update_seen(
                &mut address.per_source,
                source_id,
                packet.timestamp_ns,
                &mut self.sightings,
                self.report.limits.max_sightings,
            )?;
        }
        let Some(bssid) = bssid else { return Ok(()) };
        let advertisement = frame["type"] == 0
            && (frame["subtype"] == 5 || frame["subtype"] == 8)
            && frame["capabilities"].as_u64().is_some();
        let infrastructure_reference =
            frame["type"] == 2 && frame["to_ds"].as_bool() != frame["from_ds"].as_bool();
        if !advertisement && !infrastructure_reference && !self.report.networks.contains_key(bssid)
        {
            return Ok(());
        }
        if !self.report.networks.contains_key(bssid) {
            ensure!(
                self.report.networks.len() < self.report.limits.max_networks,
                "inventory network limit exceeded"
            );
            self.report.networks.insert(
                bssid.to_owned(),
                Network {
                    bssid: bssid.to_owned(),
                    evidence: BTreeSet::new(),
                    frames: 0,
                    protected_frames: 0,
                    frame_types: BTreeMap::new(),
                    ssids: BTreeMap::new(),
                    advertisements: Vec::new(),
                    advertised_features: BTreeSet::new(),
                    advertised_channels: BTreeSet::new(),
                    reported_frequencies_mhz: BTreeSet::new(),
                    per_source: BTreeMap::new(),
                },
            );
        }
        // Temporarily owning a row keeps global state-budget accounting separate
        // from a mutable borrow of the map; failure discards the whole staged input.
        let mut network = self
            .report
            .networks
            .remove(bssid)
            .context("missing staged network")?;
        network.frames += 1;
        network.protected_frames += u64::from(frame["protected"] == true);
        if let Some(name) = frame["name"].as_str() {
            *network.frame_types.entry(name.to_owned()).or_default() += 1;
        }
        update_seen(
            &mut network.per_source,
            source_id,
            packet.timestamp_ns,
            &mut self.sightings,
            self.report.limits.max_sightings,
        )?;
        if let Some(frequency) = packet.reported_frequency_mhz {
            insert_bounded(
                &mut network.reported_frequencies_mhz,
                frequency,
                self.report.limits.max_variants_per_network,
            )?;
        }
        if infrastructure_reference {
            network
                .evidence
                .insert("infrastructure_bssid_reference".to_owned());
        }
        if advertisement {
            self.advertisement(&mut network, frame, packet.warnings)?;
        }
        self.report.networks.insert(bssid.to_owned(), network);
        Ok(())
    }

    fn advertisement(
        &mut self,
        network: &mut Network,
        frame: &Value,
        warnings: &[&str],
    ) -> Result<()> {
        let capability_bits = u16::try_from(
            frame["capabilities"]
                .as_u64()
                .context("missing advertisement capabilities")?,
        )?;
        network.evidence.insert(
            match capability_bits & 3 {
                1 => "infrastructure_advertisement",
                2 => "ibss_advertisement",
                0 => "unspecified_bss_advertisement",
                _ => "contradictory_bss_capabilities",
            }
            .to_owned(),
        );
        let max = self.report.limits.max_variants_per_network;
        let mut security = Vec::new();
        let mut seen_ssids = BTreeSet::new();
        for ie in frame["information_elements"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if ie["id"] == 0
                && let Some(raw) = ie["data_hex"].as_str().filter(|raw| raw.len() <= 64)
            {
                if !seen_ssids.insert(raw) {
                    continue;
                }
                if !network.ssids.contains_key(raw) {
                    ensure!(
                        network.ssids.len() < max,
                        "network SSID variant limit exceeded"
                    );
                    let utf8 = ie["ssid_utf8"].as_str().map(str::to_owned);
                    self.reserve_metadata(raw.len() * 2 + utf8.as_ref().map_or(0, String::len))?;
                    network.ssids.insert(
                        raw.to_owned(),
                        Ssid {
                            bytes_hex: raw.to_owned(),
                            utf8,
                            hidden_or_empty: ie["ssid_hidden_or_empty"] == true,
                            frames: 0,
                        },
                    );
                }
                network
                    .ssids
                    .get_mut(raw)
                    .context("missing SSID variant")?
                    .frames += 1;
            }
            for key in ["advertised_channel", "primary_channel"] {
                if let Some(channel) = ie[key].as_u64().and_then(|value| u8::try_from(value).ok()) {
                    insert_bounded(&mut network.advertised_channels, channel, max)?;
                }
            }
            if let Some(feature) = ie["interpretation"]
                .as_str()
                .filter(|feature| *feature != "unrecognized_extension")
            {
                network.advertised_features.insert(feature.to_owned());
            }
            for protocol in ["rsn", "wpa"] {
                if let Some(parsed) = ie.get(protocol) {
                    let mut value = serde_json::Map::new();
                    value.insert("protocol".to_owned(), Value::String(protocol.to_owned()));
                    for key in [
                        "version",
                        "group_cipher",
                        "pairwise_ciphers",
                        "akm_suites",
                        "capabilities",
                        "pmf_required",
                        "pmf_capable",
                        "group_management_cipher",
                    ] {
                        if let Some(field) = parsed.get(key) {
                            value.insert(key.to_owned(), field.clone());
                        }
                    }
                    let value = Value::Object(value);
                    if !security.contains(&value) {
                        ensure!(
                            security.len() < max,
                            "advertisement security IE limit exceeded"
                        );
                        security.push(value);
                    }
                }
            }
        }
        let advertisement = Advertisement {
            capability_bits,
            privacy_capability: capability_bits & 0x10 != 0,
            ie_layout_complete: !warnings
                .iter()
                .any(|warning| warning.starts_with("truncated_information_element")),
            security_malformed: warnings.iter().any(|warning| {
                matches!(
                    *warning,
                    "malformed_rsn_information_element" | "malformed_wpa_information_element"
                )
            }),
            security,
        };
        if !network.advertisements.contains(&advertisement) {
            ensure!(
                network.advertisements.len() < max,
                "network advertisement variant limit exceeded"
            );
            self.reserve_metadata(serde_json::to_vec(&advertisement)?.len())?;
            network.advertisements.push(advertisement);
        }
        Ok(())
    }
}

fn unicast(mac: &str) -> bool {
    mac.len() == 17
        && mac != "00:00:00:00:00:00"
        && u8::from_str_radix(&mac[..2], 16).is_ok_and(|first| first & 1 == 0)
}

fn update_seen(
    seen: &mut BTreeMap<usize, Seen>,
    source_id: usize,
    timestamp_ns: u64,
    total: &mut usize,
    maximum: usize,
) -> Result<()> {
    if let Some(seen) = seen.get_mut(&source_id) {
        seen.observe(timestamp_ns);
    } else {
        ensure!(
            *total < maximum,
            "inventory per-source sighting limit exceeded"
        );
        *total += 1;
        seen.insert(
            source_id,
            Seen {
                first_timestamp_ns: timestamp_ns,
                last_timestamp_ns: timestamp_ns,
                frames: 1,
            },
        );
    }
    Ok(())
}

fn insert_bounded<T: Ord>(values: &mut BTreeSet<T>, value: T, maximum: usize) -> Result<()> {
    ensure!(
        values.contains(&value) || values.len() < maximum,
        "network channel/frequency variant limit exceeded"
    );
    values.insert(value);
    Ok(())
}

/// Escape untrusted labels/SSIDs as text, including Markdown HTML and controls.
fn markdown_text(text: &str) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        match ch {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' | '#' | '!' | '~' => {
                result.push('\\');
                result.push(ch);
            }
            ch if ch.is_control()
                || matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
            {
                let _ = write!(result, "\\u{{{:x}}}", u32::from(ch));
            }
            ch => result.push(ch),
        }
    }
    result
}

impl Report {
    /// Human-readable local report. It contains observed identifiers and must
    /// receive the same private publication/retention treatment as capture data.
    pub(crate) fn markdown(&self) -> String {
        let mut output = String::from("# Wi-Fi inventory\n\n");
        output.push_str(self.interpretation);
        output.push_str(".\n\nTimestamp ranges below belong to their individual source clocks. Receiver addresses indicate frame recipients, not confirmed reception by those devices. Advertised features do not prove that those PHY waveforms were decoded.\n\n");
        let _ = writeln!(
            output,
            "Sources: {}. Observed BSSIDs: {}. Unicast address observations: {}.\n",
            self.sources.len(),
            self.networks.len(),
            self.observed_addresses.len()
        );
        output.push_str("## Sources\n\n| ID | Label | Timestamp basis | Tuned Hz | Records | FCS valid | First ns | Last ns |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n");
        for source in &self.sources {
            let _ = writeln!(
                output,
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                source.source_id,
                markdown_text(&source.label),
                source.timestamp_basis.name(),
                source
                    .tuned_frequency_hz
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                source.summary.total_records,
                source.summary.validated_frames,
                source
                    .first_timestamp_ns
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                source
                    .last_timestamp_ns
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string())
            );
        }
        output.push_str("\n## BSSID observations\n\n| BSSID | Evidence | SSID variants | Advertised channels | Frames | Protected | Features |\n| --- | --- | --- | --- | --- | --- | --- |\n");
        for network in self.networks.values() {
            let ssids = network
                .ssids
                .values()
                .map(|ssid| match &ssid.utf8 {
                    Some(text) if !ssid.hidden_or_empty => {
                        format!("{} (hex {})", markdown_text(text), ssid.bytes_hex)
                    }
                    _ => format!(
                        "{}hex {}",
                        if ssid.hidden_or_empty {
                            "hidden/empty "
                        } else {
                            "non-UTF8 "
                        },
                        ssid.bytes_hex
                    ),
                })
                .collect::<Vec<_>>()
                .join("; ");
            let _ = writeln!(
                output,
                "| {} | {} | {} | {:?} | {} | {} | {} |",
                network.bssid,
                network
                    .evidence
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                ssids,
                network.advertised_channels,
                network.frames,
                network.protected_frames,
                network
                    .advertised_features
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for network in self.networks.values() {
            let _ = writeln!(output, "\n### {}\n", network.bssid);
            output.push_str("Advertised security/capability variants (claims, not keys or proof of an open network):\n\n");
            for advertisement in &network.advertisements {
                // These objects contain only enum/number/hex protocol fields, no
                // arbitrary SSID strings or payloads that could escape the fence.
                let _ = writeln!(
                    output,
                    "```json\n{}\n```\n",
                    serde_json::to_string_pretty(advertisement).unwrap_or_default()
                );
            }
            output.push_str("Per-source observation ranges:\n\n");
            for (id, seen) in &network.per_source {
                let _ = writeln!(
                    output,
                    "- Source {id}: {}..{} ns, {} frames.",
                    seen.first_timestamp_ns, seen.last_timestamp_ns, seen.frames
                );
            }
        }
        output.push_str("\n## Observed unicast addresses\n\n| Address | Roles | Frames | TX frames | Receiver-address frames |\n| --- | --- | --- | --- | --- |\n");
        for address in self.observed_addresses.values() {
            let _ = writeln!(
                output,
                "| {} | {} | {} | {} | {} |",
                address.address,
                address
                    .roles
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                address.frames,
                address.transmitter_frames,
                address.receiver_address_frames
            );
        }
        output.push_str("\nFull per-address source ranges, security claims and original file identities are retained in inventory.json.\n");
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    // Synthetic locally administered addresses. Every literal's original FCS
    // was generated with Python zlib, independently of the Rust validator.
    const BEACON: &str = "80000000ffffffffffff02000000000102000000000100000000000000000000640011040008746573742d6e6574010482848b9603010630140100000fac040100000fac040100000fac02c000bff035d8";
    const NON_UTF8: &str = "80000000ffffffffffff02000000000102000000000100000000000000000000640011040004ff007c0a010482848b9603010630140100000fac040100000fac040100000fac02c000a934e532";
    const IBSS: &str = "80000000ffffffffffff02000000000102000000000100000000000000000000640012040008746573742d6e6574010482848b9603010630140100000fac040100000fac040100000fac02c000b2eb0111";
    const BAD_IE: &str = "80000000ffffffffffff02000000000102000000000100000000000000000000640011040008746573742d6e6574010482848b9603010630ff0100000fac040100000fac040100000fac02c000a08e43d7";
    const BAD_RSN: &str = "80000000ffffffffffff02000000000102000000000100000000000000000000640011040008746573742d6e6574010482848b9603010630140200000fac040100000fac040100000fac02c00075bd9c77";
    const TO_DS: &str = "08010000020000000001020000000002020000000003000021e47116";
    const FROM_DS: &str = "080200000200000000020200000000010200000000030000a08b6448";
    const WDS: &str = "08030000020000000004020000000005020000000006000002000000000784455822";
    const PROBE: &str = "40000000ffffffffffff020000000002ffffffffffff000000008a103f58";
    const SHORT: &str = "0000ff12d941";

    fn bytes(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    fn pcap(directory: &Path, name: &str, records: &[(u32, Vec<u8>)]) -> PathBuf {
        let mut output = Vec::new();
        output.extend_from_slice(&0xa1b2c3d4_u32.to_le_bytes());
        output.extend_from_slice(&2_u16.to_le_bytes());
        output.extend_from_slice(&4_u16.to_le_bytes());
        output.extend_from_slice(&[0; 8]);
        output.extend_from_slice(&65_536_u32.to_le_bytes());
        output.extend_from_slice(&127_u32.to_le_bytes());
        for (seconds, frame) in records {
            let length = (frame.len() + 9) as u32;
            output.extend_from_slice(&seconds.to_le_bytes());
            output.extend_from_slice(&123_u32.to_le_bytes());
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&length.to_le_bytes());
            output.extend_from_slice(&[0, 0, 9, 0, 2, 0, 0, 0, 16]);
            output.extend_from_slice(frame);
        }
        let path = directory.join(name);
        fs::write(&path, output).unwrap();
        path
    }

    fn context(basis: TimestampBasis) -> SourceContext {
        SourceContext {
            label: "synthetic".to_owned(),
            tuned_frequency_hz: Some(2_412_000_000),
            timestamp_basis: basis,
        }
    }

    #[test]
    fn inventories_beacon_claims_with_separate_tuning_and_independent_clocks() {
        let directory = tempfile::tempdir().unwrap();
        let first = pcap(
            directory.path(),
            "first.pcap",
            &[(9, bytes(BEACON)), (5, bytes(BEACON))],
        );
        let second = pcap(directory.path(), "second.pcap", &[(0, bytes(BEACON))]);
        let mut inventory = Inventory::new(Limits::default()).unwrap();
        let source = inventory
            .add_pcap(&first, context(TimestampBasis::SampleRelative))
            .unwrap();
        assert_eq!(source.summary.validated_frames, 2);
        assert_eq!(source.first_timestamp_ns, Some(5_000_123_000));
        assert_eq!(source.last_timestamp_ns, Some(9_000_123_000));
        inventory
            .add_pcap(&second, context(TimestampBasis::Unknown))
            .unwrap();
        let report = inventory.report();
        assert_eq!(report.networks.len(), 1);
        let network = &report.networks["02:00:00:00:00:01"];
        assert_eq!(network.frames, 3);
        assert_eq!(
            network.ssids["746573742d6e6574"].utf8.as_deref(),
            Some("test-net")
        );
        assert_eq!(network.ssids["746573742d6e6574"].frames, 3);
        assert!(network.evidence.contains("infrastructure_advertisement"));
        assert_eq!(network.advertised_channels, BTreeSet::from([6]));
        assert!(network.reported_frequencies_mhz.is_empty());
        assert_eq!(report.sources[0].tuned_frequency_hz, Some(2_412_000_000));
        assert_eq!(network.per_source[&0].first_timestamp_ns, 5_000_123_000);
        assert_eq!(network.per_source[&1].first_timestamp_ns, 123_000);
        assert_eq!(report.sources[1].timestamp_basis, TimestampBasis::Unknown);
        let rsn = &network.advertisements[0].security[0];
        assert_eq!(rsn["protocol"], "rsn");
        assert_eq!(rsn["akm_suites"][0]["name"], "PSK");
        assert_eq!(rsn["pairwise_ciphers"][0]["name"], "CCMP-128");
        assert_eq!(rsn["pmf_capable"], true);
        assert_eq!(report.observed_addresses.len(), 1);
    }

    #[test]
    fn raw_ssid_ibss_and_malformed_advertisements_remain_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let input = pcap(
            directory.path(),
            "claims.pcap",
            &[
                (1, bytes(NON_UTF8)),
                (2, bytes(IBSS)),
                (3, bytes(BAD_IE)),
                (4, bytes(BAD_RSN)),
            ],
        );
        let mut inventory = Inventory::new(Limits::default()).unwrap();
        inventory
            .add_pcap(&input, context(TimestampBasis::Unknown))
            .unwrap();
        let network = &inventory.report().networks["02:00:00:00:00:01"];
        assert_eq!(network.ssids["ff007c0a"].utf8, None);
        assert!(network.evidence.contains("ibss_advertisement"));
        assert!(network.advertisements.iter().any(|a| !a.ie_layout_complete));
        assert!(network.advertisements.iter().any(|a| a.security_malformed));
        assert!(network.advertisements.iter().all(|a| a.privacy_capability));
        assert!(!inventory.report().markdown().contains("open network: true"));
    }

    #[test]
    fn bridge_endpoints_wds_and_wildcard_probe_do_not_invent_stations_or_aps() {
        let directory = tempfile::tempdir().unwrap();
        let input = pcap(
            directory.path(),
            "roles.pcap",
            &[
                (1, bytes(PROBE)),
                (2, bytes(TO_DS)),
                (3, bytes(FROM_DS)),
                (4, bytes(WDS)),
            ],
        );
        let mut inventory = Inventory::new(Limits::default()).unwrap();
        inventory
            .add_pcap(&input, context(TimestampBasis::HostUdpReceive))
            .unwrap();
        let report = inventory.report();
        assert_eq!(report.networks.len(), 1);
        assert_eq!(
            report.networks["02:00:00:00:00:01"].evidence,
            BTreeSet::from(["infrastructure_bssid_reference".to_owned()])
        );
        for address in [
            "02:00:00:00:00:03",
            "02:00:00:00:00:06",
            "02:00:00:00:00:07",
            "ff:ff:ff:ff:ff:ff",
        ] {
            assert!(
                !report.observed_addresses.contains_key(address),
                "{address}"
            );
        }
        let station = &report.observed_addresses["02:00:00:00:00:02"];
        assert_eq!(station.transmitter_frames, 2);
        assert_eq!(station.receiver_address_frames, 1);
        assert!(station.roles.contains("to_ds_station_transmitter"));
        assert!(station.roles.contains("from_ds_station_recipient_address"));
        assert!(report.observed_addresses.contains_key("02:00:00:00:00:04"));
        assert!(report.observed_addresses.contains_key("02:00:00:00:00:05"));
    }

    #[test]
    fn corrupt_and_malformed_mac_records_cannot_establish_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let mut corrupt = bytes(BEACON);
        corrupt[10] ^= 1;
        let input = pcap(
            directory.path(),
            "invalid.pcap",
            &[(1, corrupt), (2, bytes(SHORT))],
        );
        let mut inventory = Inventory::new(Limits::default()).unwrap();
        let source = inventory
            .add_pcap(&input, context(TimestampBasis::Unknown))
            .unwrap();
        assert_eq!(source.summary.invalid_fcs_frames, 1);
        assert_eq!(source.summary.malformed_frames, 1);
        assert_eq!(source.first_timestamp_ns, None);
        assert!(inventory.report().networks.is_empty());
        assert!(inventory.report().observed_addresses.is_empty());
    }

    #[test]
    fn input_and_state_limits_are_transactional_and_never_evict_prior_results() {
        let directory = tempfile::tempdir().unwrap();
        let first = pcap(directory.path(), "first.pcap", &[(1, bytes(BEACON))]);
        let excess = pcap(
            directory.path(),
            "excess.pcap",
            &[(2, bytes(BEACON)), (3, bytes(NON_UTF8))],
        );
        for limits in [
            Limits {
                max_records_per_input: 1,
                ..Limits::default()
            },
            Limits {
                max_variants_per_network: 1,
                ..Limits::default()
            },
            Limits {
                max_sources: 1,
                ..Limits::default()
            },
        ] {
            let mut inventory = Inventory::new(limits).unwrap();
            inventory
                .add_pcap(&first, context(TimestampBasis::Unknown))
                .unwrap();
            let before = serde_json::to_value(inventory.report()).unwrap();
            assert!(
                inventory
                    .add_pcap(&excess, context(TimestampBasis::Unknown))
                    .is_err()
            );
            assert_eq!(serde_json::to_value(inventory.report()).unwrap(), before);
            assert!(
                inventory
                    .add_pcap(&first, context(TimestampBasis::Unknown))
                    .is_err()
            );
            assert_eq!(serde_json::to_value(inventory.report()).unwrap(), before);
        }
        for limits in [
            Limits {
                max_input_bytes: 24,
                ..Limits::default()
            },
            Limits {
                max_sightings: 1,
                ..Limits::default()
            },
            Limits {
                max_metadata_bytes: 1,
                ..Limits::default()
            },
        ] {
            let mut inventory = Inventory::new(limits).unwrap();
            assert!(
                inventory
                    .add_pcap(&first, context(TimestampBasis::Unknown))
                    .is_err()
            );
            assert!(inventory.report().sources.is_empty());
            assert!(inventory.report().networks.is_empty());
        }
        let mut truncated = fs::read(&excess).unwrap();
        truncated.pop();
        let input = directory.path().join("truncated.pcap");
        fs::write(&input, truncated).unwrap();
        let mut inventory = Inventory::new(Limits::default()).unwrap();
        inventory
            .add_pcap(&first, context(TimestampBasis::Unknown))
            .unwrap();
        let before = serde_json::to_value(inventory.report()).unwrap();
        assert!(
            inventory
                .add_pcap(&input, context(TimestampBasis::Unknown))
                .is_err()
        );
        assert_eq!(serde_json::to_value(inventory.report()).unwrap(), before);
    }

    #[test]
    fn markdown_escapes_untrusted_ssid_labels_and_bidirectional_controls() {
        let escaped = markdown_text("<img>|`\n\u{202e}name");
        assert_eq!(escaped, "&lt;img&gt;\\|\\`\\n\\u{202e}name");
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\u{202e}'));
        assert!(
            Inventory::new(Limits {
                max_sources: 0,
                ..Limits::default()
            })
            .is_err()
        );
    }
}
