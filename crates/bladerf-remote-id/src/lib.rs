//! Read OpenDroneID Basic ID observations from Bluetooth Service Data and
//! Wi-Fi Beacon vendor information elements.
//!
//! Pass complete field contents from packets whose link integrity was
//! checked by the caller. The parser uses fixed-size observations and
//! borrows the input while iterating. Both entry points recognize protocol
//! versions 0 through 2.
//!
//! ```
//! use bladerf_remote_id::bluetooth_basic_ids;
//!
//! let mut service_data = [0; 29];
//! service_data[..6].copy_from_slice(&[0xfa, 0xff, 0x0d, 7, 0x02, 0x12]);
//! service_data[6..13].copy_from_slice(b"DEMO123");
//! let basic_id = bluetooth_basic_ids(&service_data).next().unwrap();
//! assert_eq!(basic_id.message_counter(), 7);
//! assert_eq!(basic_id.protocol_version(), 2);
//! assert_eq!(basic_id.id_text(), Some("DEMO123"));
//! ```
//!
//! [OpenDroneID wire layouts](https://github.com/opendroneid/opendroneid-core-c/blob/8384b6a4cbe4a3a7ecfff2fbf71f03d38852e366/libopendroneid/opendroneid.h#L423-L437)
//! · [Bluetooth service dispatch](https://github.com/opendroneid/wireshark-dissector/blob/d12670fede3aa4e336cfa326528782abfc864a10/opendroneid-dissector.lua#L705-L723)
//! · [Wi-Fi Beacon transport](https://github.com/opendroneid/opendroneid-core-c/blob/8384b6a4cbe4a3a7ecfff2fbf71f03d38852e366/libopendroneid/wifi.c#L506-L535)

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::iter::FusedIterator;
use std::slice::Iter;

use serde::ser::{Error as _, Serialize, SerializeStruct, Serializer};

const MESSAGE_BYTES: usize = 25;
const ID_BYTES: usize = 20;
const MAX_PACK_MESSAGES: u8 = 9;
const MAX_PROTOCOL_VERSION: u8 = 2;
const FULL_PACK_BYTES: usize = 3 + MAX_PACK_MESSAGES as usize * MESSAGE_BYTES;

/// One Basic ID message with the advertised type codes and exact ID bytes.
///
/// JSON includes the transport counter, protocol version, ID type, UA
/// type, and all 20 ID bytes as lowercase hexadecimal. A printable
/// serial or registration ID also supplies `id_text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BasicId {
    message_counter: u8,
    protocol_version: u8,
    id_type: u8,
    ua_type: u8,
    id: [u8; ID_BYTES],
}

impl BasicId {
    /// Return the transport's message counter. All messages in a pack
    /// share the same counter.
    pub fn message_counter(&self) -> u8 {
        self.message_counter
    }

    /// Return the Basic ID message's protocol version.
    pub fn protocol_version(&self) -> u8 {
        self.protocol_version
    }

    /// Return the advertised ID namespace as a wire code.
    pub fn id_type(&self) -> u8 {
        self.id_type
    }

    /// Return the advertised unmanned-aircraft type as a wire code.
    pub fn ua_type(&self) -> u8 {
        self.ua_type
    }

    /// Return all 20 ID bytes, including any original padding.
    pub fn id_bytes(&self) -> &[u8; ID_BYTES] {
        &self.id
    }

    /// Return a nonempty printable ASCII serial or registration ID.
    ///
    /// Text IDs may fill the field or end in NUL padding. Binary and
    /// unrecognized namespaces are available through [Self::id_bytes].
    pub fn id_text(&self) -> Option<&str> {
        if !matches!(self.id_type, 1 | 2) {
            return None;
        }
        let end = self
            .id
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(ID_BYTES);
        let (text, padding) = self.id.split_at(end);
        if text.is_empty()
            || text.iter().any(|byte| !(b' '..=b'~').contains(byte))
            || padding.iter().any(|&byte| byte != 0)
        {
            return None;
        }
        std::str::from_utf8(text).ok()
    }
}

impl Serialize for BasicId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = [0; ID_BYTES * 2];
        for (&byte, [high, low]) in self.id.iter().zip(hex.as_chunks_mut::<2>().0) {
            *high = HEX[usize::from(byte >> 4)];
            *low = HEX[usize::from(byte & 0x0f)];
        }
        let id_hex = std::str::from_utf8(&hex).map_err(S::Error::custom)?;
        let text = self.id_text();
        let mut record = serializer.serialize_struct("BasicId", 5 + usize::from(text.is_some()))?;
        record.serialize_field("message_counter", &self.message_counter)?;
        record.serialize_field("protocol_version", &self.protocol_version)?;
        record.serialize_field("id_type", &self.id_type)?;
        record.serialize_field("ua_type", &self.ua_type)?;
        record.serialize_field("id_hex", id_hex)?;
        if let Some(text) = text {
            record.serialize_field("id_text", text)?;
        }
        record.end()
    }
}

/// Bounded iterator over the recognized Basic ID messages in one field.
///
/// Messages appear in their original order. Unrecognized fields, malformed
/// framing and unsupported Basic ID versions yield no matching items.
#[derive(Debug, Clone)]
pub struct BasicIds<'a> {
    message_counter: u8,
    messages: Iter<'a, [u8; MESSAGE_BYTES]>,
}

impl<'a> BasicIds<'a> {
    fn new(message_counter: u8, messages: &'a [u8]) -> Self {
        Self {
            message_counter,
            messages: messages.as_chunks::<MESSAGE_BYTES>().0.iter(),
        }
    }
}

impl Iterator for BasicIds<'_> {
    type Item = BasicId;

    fn next(&mut self) -> Option<Self::Item> {
        self.messages.find_map(|[header, types, id @ .., _, _, _]| {
            if *header > MAX_PROTOCOL_VERSION {
                return None;
            }
            Some(BasicId {
                message_counter: self.message_counter,
                protocol_version: *header,
                id_type: types >> 4,
                ua_type: types & 0x0f,
                id: *id,
            })
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.messages.len()))
    }
}

impl FusedIterator for BasicIds<'_> {}

/// Read Basic IDs from the contents of a Bluetooth Service Data field
/// with a 16-bit UUID.
///
/// Pass the UUID in wire order followed by its payload. An OpenDroneID
/// field has UUID bytes `fa ff`, application code `0d`, a message
/// counter and either a single 25-byte message or a complete Message Pack.
/// A nine-slot pack buffer may have zero-filled unused slots.
pub fn bluetooth_basic_ids(service_data: &[u8]) -> BasicIds<'_> {
    let [0xfa, 0xff, 0x0d, counter, payload @ ..] = service_data else {
        return BasicIds::new(0, &[]);
    };
    let messages = match payload.len() {
        MESSAGE_BYTES => payload,
        _ => match message_pack(payload) {
            Some((messages, [])) => messages,
            Some((messages, padding))
                if payload.len() == FULL_PACK_BYTES && padding.iter().all(|&byte| byte == 0) =>
            {
                messages
            }
            _ => &[],
        },
    };
    BasicIds::new(*counter, messages)
}

/// Read Basic IDs from an OpenDroneID vendor information element in a
/// Wi-Fi Beacon.
///
/// Pass the value of element 221 beginning with vendor OUI `fa:0b:bc`.
/// The transport carries type `0d`, a message counter and a complete
/// Message Pack.
pub fn wifi_beacon_basic_ids(vendor_data: &[u8]) -> BasicIds<'_> {
    let [0xfa, 0x0b, 0xbc, 0x0d, counter, payload @ ..] = vendor_data else {
        return BasicIds::new(0, &[]);
    };
    let messages = match message_pack(payload) {
        Some((messages, [])) => messages,
        _ => &[],
    };
    BasicIds::new(*counter, messages)
}

fn message_pack(payload: &[u8]) -> Option<(&[u8], &[u8])> {
    let [0xf0..=0xf2, 25, count @ 1..=MAX_PACK_MESSAGES, body @ ..] = payload else {
        return None;
    };
    let (messages, tail) = body.split_at_checked(usize::from(*count) * MESSAGE_BYTES)?;
    if messages
        .as_chunks::<MESSAGE_BYTES>()
        .0
        .iter()
        .any(|message| message[0] >> 4 > 5)
    {
        return None;
    }
    Some((messages, tail))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(header: u8, types: u8, id: &[u8]) -> [u8; MESSAGE_BYTES] {
        let mut message = [0; MESSAGE_BYTES];
        message[0] = header;
        message[1] = types;
        message[2..2 + id.len()].copy_from_slice(id);
        message
    }

    #[test]
    fn bluetooth_single_reports_wire_id_and_serial_text() {
        let mut field = vec![0xfa, 0xff, 0x0d, 7];
        field.extend(message(0x02, 0x12, b"DEMO123"));
        let id = bluetooth_basic_ids(&field).next().unwrap();
        assert_eq!(id.id_bytes()[..7], *b"DEMO123");
        assert_eq!(
            serde_json::to_value(id).unwrap(),
            json!({
                "message_counter": 7,
                "protocol_version": 2,
                "id_type": 1,
                "ua_type": 2,
                "id_hex": "44454d4f31323300000000000000000000000000",
                "id_text": "DEMO123"
            })
        );
        assert_eq!(bluetooth_basic_ids(&field).count(), 1);
        for index in [0, 1, 2] {
            let mut wrong_transport = field.clone();
            wrong_transport[index] ^= 1;
            assert_eq!(bluetooth_basic_ids(&wrong_transport).count(), 0);
        }
    }

    #[test]
    fn packs_preserve_distinct_id_types_and_slot_order() {
        let mut pack = vec![0xf2, 25, 3];
        pack.extend(message(0x12, 0, b""));
        pack.extend(message(0x00, 0x22, b"DEMOREG"));
        pack.extend(message(0x01, 0x42, &[1, 0, 0xff, 0x20]));
        let mut bluetooth = vec![0xfa, 0xff, 0x0d, 255];
        let mut wifi = vec![0xfa, 0x0b, 0xbc, 0x0d, 255];
        bluetooth.extend_from_slice(&pack);
        wifi.extend_from_slice(&pack);
        let ids: Vec<_> = wifi_beacon_basic_ids(&wifi).collect();
        assert_eq!(bluetooth_basic_ids(&bluetooth).collect::<Vec<_>>(), ids);
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].protocol_version(), 0);
        assert_eq!(ids[0].id_text(), Some("DEMOREG"));
        assert_eq!(ids[1].protocol_version(), 1);
        assert_eq!(ids[1].id_type(), 4);
        assert_eq!(ids[1].ua_type(), 2);
        assert_eq!(ids[1].message_counter(), 255);
        assert_eq!(ids[1].id_bytes()[..4], [1, 0, 0xff, 0x20]);
        assert_eq!(ids[1].id_text(), None);
        assert!(
            serde_json::to_value(ids[1])
                .unwrap()
                .get("id_text")
                .is_none()
        );
    }

    #[test]
    fn malformed_beacon_packs_yield_no_ids() {
        let mut valid = vec![0xfa, 0x0b, 0xbc, 0x0d, 0, 0xf2, 25, 1];
        valid.extend(message(0x02, 0x12, b"DEMO123"));
        assert_eq!(wifi_beacon_basic_ids(&valid).count(), 1);
        for end in 0..valid.len() {
            assert_eq!(wifi_beacon_basic_ids(&valid[..end]).count(), 0);
        }
        for (index, value) in [
            (0, 0),
            (3, 0),
            (5, 0xf3),
            (6, 24),
            (7, 0),
            (7, 2),
            (7, 10),
            (8, 0xf2),
        ] {
            let mut bad = valid.clone();
            bad[index] = value;
            assert_eq!(wifi_beacon_basic_ids(&bad).count(), 0);
        }
        valid.push(0);
        assert_eq!(wifi_beacon_basic_ids(&valid).count(), 0);
    }

    #[test]
    fn bluetooth_accepts_only_full_zero_padded_pack_buffers() {
        let mut field = vec![0xfa, 0xff, 0x0d, 8, 0xf0, 25, 1];
        field.extend(message(0x00, 0x12, b"DEMO123"));
        let exact_len = field.len();
        field.resize(FULL_PACK_BYTES + 4, 0);
        assert_eq!(bluetooth_basic_ids(&field).count(), 1);
        assert_eq!(bluetooth_basic_ids(&field[..field.len() - 1]).count(), 0);
        field[exact_len] = 0x02;
        assert_eq!(bluetooth_basic_ids(&field).count(), 0);
        field[exact_len] = 0;
        field[6] = 0;
        assert_eq!(bluetooth_basic_ids(&field).count(), 0);
        field[6] = 1;
        let mut wifi = vec![0xfa, 0x0b, 0xbc, 0x0d, 8];
        wifi.extend_from_slice(&field[4..]);
        assert_eq!(wifi_beacon_basic_ids(&wifi).count(), 0);
    }

    #[test]
    fn opaque_or_malformed_text_keeps_all_wire_bytes() {
        let mut field = vec![0xfa, 0xff, 0x0d, 0];
        field.extend(message(0x02, 0x12, b"DEMO\0X"));
        let id = bluetooth_basic_ids(&field).next().unwrap();
        assert_eq!(id.id_text(), None);
        assert_eq!(id.id_bytes()[..6], *b"DEMO\0X");
        field[5] = 0xf2;
        let opaque = bluetooth_basic_ids(&field).next().unwrap();
        assert_eq!(opaque.id_type(), 15);
        assert_eq!(opaque.id_bytes(), id.id_bytes());
        assert_eq!(opaque.id_text(), None);
        field[4] = 0x03;
        assert_eq!(bluetooth_basic_ids(&field).count(), 0);
    }
}
