use serde_json::{Value, json};

use super::parse_service_data;

fn decoded(service_data: &[u8]) -> Value {
    serde_json::to_value(parse_service_data(service_data).unwrap()).unwrap()
}

#[test]
fn published_service_data_vectors() {
    // https://bthome.io/format/
    assert_eq!(
        decoded(&[0xd2, 0xfc, 0x40, 0x02, 0xc4, 0x09, 0x03, 0xbf, 0x13]),
        json!({"trigger_based": false, "encoding": "unencrypted", "objects": [
            {"kind": "temperature_millicelsius", "value": 25_000},
            {"kind": "relative_humidity_ppm", "value": 505_500},
        ]})
    );

    // https://bthome.io/encryption/
    let encrypted = [
        0xd2, 0xfc, 0x41, 0xe4, 0x45, 0xf3, 0xc9, 0x96, 0x2b, 0x33, 0x22, 0x11, 0x00, 0x6c, 0x7c,
        0x45, 0x19,
    ];
    assert_eq!(
        decoded(&encrypted),
        json!({"trigger_based": false, "encoding": "encrypted"})
    );
    assert!(parse_service_data(&encrypted[..12]).is_none());
}

#[test]
fn door_window_fields_and_ordered_button_events() {
    // Shelly documents these field layouts and legacy hold code 0xfe.
    // https://shelly-api-docs.shelly.cloud/docs-ble/Devices/BLU/dw/
    let report = decoded(&[
        0xd2, 0xfc, 0x44, 0x00, 0xff, 0x01, 97, 0x05, 0x13, 0x8a, 0x14, 0x2d, 1, 0x3a, 0, 0x3a,
        0x80, 0x3a, 0xfe, 0x3f, 0x85, 0xff,
    ]);
    assert_eq!(report["trigger_based"], true);
    assert_eq!(
        report["objects"],
        json!([
            {"kind": "packet_id", "value": 255},
            {"kind": "battery_percent", "value": 97},
            {"kind": "illuminance_centilux", "value": 1_346_067},
            {"kind": "window_open", "value": true},
            {"kind": "button", "value": {"code": 0, "event": "none"}},
            {"kind": "button", "value": {"code": 128, "event": "hold_press"}},
            {"kind": "button", "value": {"code": 254, "event": null}},
            {"kind": "rotation_decidegrees", "value": -123},
        ])
    );
}

#[test]
fn optional_address_precedes_the_object_stream() {
    // The reference extension uses little-endian wire order for the MAC.
    let data = [0xd2, 0xfc, 0x42, 0x3a, 1, 2, 3, 4, 0x02, 0x2d, 0];
    let report = decoded(&data);
    assert_eq!(report["embedded_address"], "02:04:03:02:01:3a");
    assert_eq!(
        report["objects"],
        json!([{"kind": "window_open", "value": false}])
    );
    assert!(parse_service_data(&data[..8]).is_none());
    assert!(parse_service_data(&data[..9]).is_none());
}

#[test]
fn known_variable_lengths_and_unknown_object_boundary() {
    let data = [
        0xd2, 0xfc, 0x40, 0x3b, 1, 3, 0x05, 0x53, 1, b'A', 0x54, 2, 0xf0, 0x01, 0xf0, 0x34, 0x12,
        0xf1, 0, 1, 2, 4,
    ];
    assert_eq!(
        decoded(&data)["objects"],
        json!([
            {"kind": "device_type_id", "value": 0x1234},
            {"kind": "firmware_version", "value": "4.2.1.0"},
        ])
    );
    for end in [4, 5, 6, 8, 9, 11, 12, 13, 15, 16, 18, 20] {
        assert!(
            parse_service_data(&data[..end]).is_none(),
            "truncation at {end}"
        );
    }
    assert_eq!(
        decoded(&[0xd2, 0xfc, 0x40, 0, 42, 0x30, 0x3a, 1]),
        json!({"trigger_based": false, "encoding": "unencrypted", "first_unknown_object_id": 0x30,
            "objects": [{"kind": "packet_id", "value": 42}]}),
    );
}

#[test]
fn rejects_unrecognized_or_incomplete_headers() {
    for info in [0x20, 0x48, 0x50, 0x60] {
        assert!(parse_service_data(&[0xd2, 0xfc, info, 0x3a, 1]).is_none());
    }
    assert!(parse_service_data(&[0xfc, 0xd2, 0x40, 0x3a, 1]).is_none());
    assert!(parse_service_data(&[0xd2, 0xfc]).is_none());
    assert!(parse_service_data(&[0xd2, 0xfc, 0x40]).is_none());
}
