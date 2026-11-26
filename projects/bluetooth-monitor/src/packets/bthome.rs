//! Interpret [BTHome v2] service data inside an independently validated BLE PDU.
//!
//! [BTHome v2]: https://bthome.io/format/

use serde::Serialize;

use super::{format_address, le16, le24};

/// The observed trigger flag, optional payload address, and payload status.
#[derive(Debug, Serialize)]
pub(super) struct V2 {
    trigger_based: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedded_address: Option<String>,
    #[serde(flatten)]
    payload: Payload,
}

#[derive(Debug, Serialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
enum Payload {
    Encrypted,
    Unencrypted {
        objects: Vec<Object>,
        #[serde(skip_serializing_if = "Option::is_none")]
        first_unknown_object_id: Option<u8>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum Object {
    PacketId(u8),
    BatteryPercent(u8),
    TemperatureMillicelsius(i32),
    RelativeHumidityPpm(u32),
    IlluminanceCentilux(u32),
    Opening(bool),
    DoorOpen(bool),
    MotionDetected(bool),
    WindowOpen(bool),
    Button {
        code: u8,
        event: Option<&'static str>,
    },
    RotationDecidegrees(i16),
    DeviceTypeId(u16),
    FirmwareVersion(String),
}

/// Parse the UUID and v2 device-info flags, with an optional address prefix.
/// Encrypted advertisements retain only clear envelope metadata. Truncated
/// known objects leave the original generic AD bytes available to the caller.
pub(super) fn parse_service_data(data: &[u8]) -> Option<V2> {
    let [0xd2, 0xfc, info, body @ ..] = data else {
        return None;
    };
    if info >> 5 != 2 || info & 0x18 != 0 {
        return None;
    }

    // The reference receiver supports the optional MAC prefix in bit 1.
    // https://github.com/Bluetooth-Devices/bthome-ble/commit/1059aa1
    let (address, body) = if info & 0x02 != 0 {
        let (mac, objects) = body.split_first_chunk::<6>()?;
        (Some(mac), objects)
    } else {
        (None, body)
    };

    let payload = if info & 0x01 != 0 {
        // At least one object (two bytes), then the four-byte counter and MIC.
        body.get(10..)?;
        Payload::Encrypted
    } else {
        parse_objects(body)?
    };
    Some(V2 {
        trigger_based: info & 0x04 != 0,
        embedded_address: address.map(|mac| format_address(mac)),
        payload,
    })
}

fn parse_objects(mut body: &[u8]) -> Option<Payload> {
    body.get(2..)?;
    let mut objects = Vec::new();
    let mut first_unknown_object_id = None;
    while let Some((&id, data)) = body.split_first() {
        let width = match id {
            // Length octet, opcode, and the specified number of command arguments.
            0x3b => 2 + usize::from(data.first()? & 0x1f),
            // Text and raw objects prefix their data with a byte count.
            0x53 | 0x54 => 1 + usize::from(*data.first()?),
            _ => match fixed_width(id) {
                Some(width) => width,
                None => {
                    first_unknown_object_id = Some(id);
                    break;
                }
            },
        };
        let (value, rest) = data.split_at_checked(width)?;
        if let Some(object) = interpret(id, value) {
            objects.push(object);
        }
        body = rest;
    }
    Some(Payload::Unencrypted {
        objects,
        first_unknown_object_id,
    })
}

// Published BTHome v2 object sizes, excluding the ID octet. Known measurements
// can be skipped by size. Unknown IDs have no skippable length.
fn fixed_width(id: u8) -> Option<usize> {
    Some(match id {
        0x00
        | 0x01
        | 0x09
        | 0x0f..=0x11
        | 0x15..=0x2f
        | 0x3a
        | 0x46
        | 0x57..=0x59
        | 0x60
        | 0x64
        | 0x65
        | 0x67 => 1,
        0x02
        | 0x03
        | 0x06..=0x08
        | 0x0c..=0x0e
        | 0x12..=0x14
        | 0x3c
        | 0x3d
        | 0x3f..=0x41
        | 0x43..=0x45
        | 0x47..=0x4a
        | 0x51
        | 0x52
        | 0x56
        | 0x5a
        | 0x5d..=0x5f
        | 0x61
        | 0x66
        | 0xf0 => 2,
        0x04 | 0x05 | 0x0a | 0x0b | 0x42 | 0x4b | 0xf2 => 3,
        0x3e | 0x4c..=0x50 | 0x55 | 0x5b | 0x5c | 0x62 | 0x63 | 0xf1 => 4,
        _ => return None,
    })
}

fn interpret(id: u8, value: &[u8]) -> Option<Object> {
    let binary = || match value[0] {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let signed16 = || i16::from_le_bytes([value[0], value[1]]);
    let signed8 = || i32::from(i8::from_le_bytes([value[0]]));
    Some(match id {
        0x00 => Object::PacketId(value[0]),
        0x01 => Object::BatteryPercent(value[0]),
        0x02 => Object::TemperatureMillicelsius(i32::from(signed16()) * 10),
        0x03 => Object::RelativeHumidityPpm(u32::from(le16(value)) * 100),
        0x05 => Object::IlluminanceCentilux(le24(value)),
        0x11 => Object::Opening(binary()?),
        0x1a => Object::DoorOpen(binary()?),
        0x21 => Object::MotionDetected(binary()?),
        0x2d => Object::WindowOpen(binary()?),
        0x2e => Object::RelativeHumidityPpm(u32::from(value[0]) * 10_000),
        0x3a => Object::Button {
            code: value[0],
            event: match value[0] {
                0x00 => Some("none"),
                0x01 => Some("press"),
                0x02 => Some("double_press"),
                0x03 => Some("triple_press"),
                0x04 => Some("long_press"),
                0x05 => Some("long_double_press"),
                0x06 => Some("long_triple_press"),
                0x80 => Some("hold_press"),
                0x81 => Some("hold_release"),
                _ => None,
            },
        },
        0x3f => Object::RotationDecidegrees(signed16()),
        0x45 => Object::TemperatureMillicelsius(i32::from(signed16()) * 100),
        0x57 => Object::TemperatureMillicelsius(signed8() * 1000),
        0x58 => Object::TemperatureMillicelsius(signed8() * 350),
        0xf0 => Object::DeviceTypeId(le16(value)),
        0xf1 => Object::FirmwareVersion(format!(
            "{}.{}.{}.{}",
            value[3], value[2], value[1], value[0]
        )),
        0xf2 => Object::FirmwareVersion(format!("{}.{}.{}", value[2], value[1], value[0])),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
