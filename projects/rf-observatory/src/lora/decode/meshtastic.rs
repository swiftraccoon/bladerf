//! Read Meshtastic's clear routing header from a CRC-checked LoRa payload.
//!
//! The [wire format] documents byte order. The pinned firmware defines the
//! [header and limits], [radio admission], [routing rules] and [hop semantics].
//!
//! [wire format]: https://meshtastic.org/docs/overview/mesh-algo/#layer-1-unreliable-zero-hop-messaging
//! [header and limits]: https://github.com/meshtastic/firmware/blob/2ab8abdebb006e9bd4ef4fea8bdf95a258548583/src/mesh/RadioInterface.h#L21-L74
//! [radio admission]: https://github.com/meshtastic/firmware/blob/2ab8abdebb006e9bd4ef4fea8bdf95a258548583/src/mesh/RadioLibInterface.cpp#L979-L1016
//! [routing rules]: https://github.com/meshtastic/firmware/blob/2ab8abdebb006e9bd4ef4fea8bdf95a258548583/src/mesh/Router.cpp#L1847-L1852
//! [hop semantics]: https://github.com/meshtastic/firmware/blob/2ab8abdebb006e9bd4ef4fea8bdf95a258548583/src/mesh/NodeDB.cpp#L3768-L3786

use serde::Serialize;

const HEADER_BYTES: usize = 16;
const MAX_PAYLOAD_BYTES: usize = 255;
const BROADCAST: u32 = u32::MAX;
const NO_LORA_DESTINATION: u32 = 1;
const HOP_LIMIT_MASK: u8 = 0x07;
const WANT_ACK_MASK: u8 = 0x08;
const VIA_MQTT_MASK: u8 = 0x10;
const HOP_START_SHIFT: u32 = 5;

/// An unauthenticated observation of the on-air routing fields.
/// The firmware maps a node number ending in `0x00` to `0xFF` in its
/// one-byte routing projections, which can collide across multiple nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(super) struct Header {
    /// Claimed origin NodeID, retained across relays.
    origin_node_id: u32,
    /// Claimed destination NodeID, or the protocol broadcast value.
    destination_node_id: u32,
    /// Packet ID carried across retransmissions by the origin.
    packet_id: u32,
    /// Whether the destination is the on-air broadcast value.
    broadcast: bool,
    /// The channel hash carried as a routing and decoding hint.
    channel_hash: u8,
    /// Remaining hop allowance advertised by this transmission.
    hop_limit: u8,
    /// Original hop allowance. Zero also covers firmware without this field.
    hop_start: u8,
    /// State of the acknowledgment request bit.
    want_ack: bool,
    /// State of the MQTT ingress bit.
    via_mqtt: bool,
    /// The next-hop projection, with zero indicating no preference.
    /// Unavailable when the original hop allowance is zero.
    next_hop_byte: Option<u8>,
    /// The transmitting relay's projected NodeID byte, with zero unset.
    /// Unavailable when the original hop allowance is zero.
    relay_node_byte: Option<u8>,
}

impl Header {
    /// Parse a plausible routing header without inspecting application data.
    /// The length check requires room for a body and permits at most 255 bytes.
    /// Absent or broadcast origins, the [non-LoRa destination] and inconsistent
    /// nonzero hop starts yield `None`. Zero hop starts can occur on legacy
    /// frames and leave the routing bytes uninterpreted.
    ///
    /// [non-LoRa destination]: https://github.com/meshtastic/firmware/blob/2ab8abdebb006e9bd4ef4fea8bdf95a258548583/src/mesh/RadioLibInterface.cpp#L292-L295
    pub(super) fn parse(payload: &[u8]) -> Option<Self> {
        if !(HEADER_BYTES + 1..=MAX_PAYLOAD_BYTES).contains(&payload.len()) {
            return None;
        }
        let destination_node_id = u32::from_le_bytes(payload[0..4].try_into().ok()?);
        let origin_node_id = u32::from_le_bytes(payload[4..8].try_into().ok()?);
        let packet_id = u32::from_le_bytes(payload[8..12].try_into().ok()?);
        let flags = payload[12];
        let hop_start = flags >> HOP_START_SHIFT;
        let hop_limit = flags & HOP_LIMIT_MASK;
        if matches!(origin_node_id, 0 | BROADCAST)
            || destination_node_id == NO_LORA_DESTINATION
            || (hop_start != 0 && hop_start < hop_limit)
        {
            return None;
        }
        Some(Self {
            origin_node_id,
            destination_node_id,
            packet_id,
            broadcast: destination_node_id == BROADCAST,
            channel_hash: payload[13],
            hop_limit,
            hop_start,
            want_ack: flags & WANT_ACK_MASK != 0,
            via_mqtt: flags & VIA_MQTT_MASK != 0,
            next_hop_byte: (hop_start != 0).then_some(payload[14]),
            relay_node_byte: (hop_start != 0).then_some(payload[15]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic unicast header with distinct addressing and forwarding fields.
    const FRAME: [u8; 18] = [
        0x78, 0x56, 0x34, 0x12, 0xEF, 0xCD, 0xAB, 0x89, 0xD4, 0xC3, 0xB2, 0xA1, 0xBA, 0x37, 0x00,
        0xFF, 0x08, 0x01,
    ];

    #[test]
    fn parses_little_endian_ids_and_distinct_routing_fields() {
        assert_eq!(
            Header::parse(&FRAME),
            Some(Header {
                origin_node_id: 0x89AB_CDEF,
                destination_node_id: 0x1234_5678,
                packet_id: 0xA1B2_C3D4,
                broadcast: false,
                channel_hash: 0x37,
                hop_limit: 2,
                hop_start: 5,
                want_ack: true,
                via_mqtt: true,
                next_hop_byte: Some(0),
                relay_node_byte: Some(0xFF),
            })
        );
    }

    #[test]
    fn retains_legacy_hop_limit_and_an_exact_zero_packet_id() {
        let mut frame = FRAME;
        frame[..4].copy_from_slice(&BROADCAST.to_le_bytes());
        frame[8..12].fill(0);
        frame[12] = 3;
        let decoded = Header::parse(&frame).unwrap();
        assert!(decoded.broadcast);
        assert_eq!(decoded.packet_id, 0);
        assert_eq!((decoded.hop_start, decoded.hop_limit), (0, 3));
        assert_eq!(
            (decoded.next_hop_byte, decoded.relay_node_byte),
            (None, None)
        );
        assert!(!decoded.want_ack);
        assert!(!decoded.via_mqtt);
    }

    #[test]
    fn checks_both_payload_length_limits() {
        assert!(Header::parse(&FRAME[..HEADER_BYTES]).is_none());
        assert!(Header::parse(&FRAME[..HEADER_BYTES + 1]).is_some());
        let mut overlong = [0; MAX_PAYLOAD_BYTES + 1];
        overlong[..FRAME.len()].copy_from_slice(&FRAME);
        assert!(Header::parse(&overlong[..MAX_PAYLOAD_BYTES]).is_some());
        assert!(Header::parse(&overlong).is_none());
    }

    #[test]
    fn rejects_invalid_sender_destination_and_declared_hop_start() {
        for invalid_origin in [0, BROADCAST] {
            let mut frame = FRAME;
            frame[4..8].copy_from_slice(&invalid_origin.to_le_bytes());
            assert_eq!(Header::parse(&frame), None);
        }
        let mut prohibited_destination = FRAME;
        prohibited_destination[..4].copy_from_slice(&NO_LORA_DESTINATION.to_le_bytes());
        assert_eq!(Header::parse(&prohibited_destination), None);

        let mut inconsistent_hops = FRAME;
        inconsistent_hops[12] = (2 << HOP_START_SHIFT) | 5;
        assert_eq!(Header::parse(&inconsistent_hops), None);
    }
}
