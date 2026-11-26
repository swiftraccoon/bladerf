//! Decode the [Ruuvi RAWv2] manufacturer data carried in BLE advertisements.
//!
//! [Ruuvi RAWv2]: https://docs.ruuvi.com/communication/bluetooth-advertisements/data-format-5-rawv2

use serde::Serialize;

const BODY_BYTES: usize = 24;

/// Reported sensor ID and exact integer measurements. Unavailable values are null.
#[derive(Debug, Serialize)]
pub(super) struct RawV2 {
    sensor_id: Option<String>,
    temperature_millicelsius: Option<i32>,
    relative_humidity_ppm: Option<u32>,
    pressure_pa: Option<u32>,
    acceleration_mg: [Option<i16>; 3],
    battery_mv: Option<u16>,
    transmit_power_dbm: Option<i16>,
    movement_counter: Option<u8>,
    measurement_sequence: Option<u16>,
}

/// Parse the little-endian company prefix and exact 24-byte format-5 body.
/// The sensor ID uses the MAC bytes in payload order.
pub(super) fn parse_manufacturer_data(data: &[u8]) -> Option<RawV2> {
    let [0x99, 0x04, body @ ..] = data else {
        return None;
    };
    let body: &[u8; BODY_BYTES] = body.try_into().ok()?;
    if body[0] != 0x05 {
        return None;
    }

    let read_unsigned = |offset: usize| u16::from_be_bytes([body[offset], body[offset + 1]]);
    let unsigned = |offset| {
        let value = read_unsigned(offset);
        (value != u16::MAX).then_some(value)
    };
    let signed = |offset: usize| {
        let value = i16::from_be_bytes([body[offset], body[offset + 1]]);
        (value != i16::MIN).then_some(value)
    };
    let battery_raw = read_unsigned(13) >> 5;
    let transmit_power_raw = body[14] & 0x1F;
    let movement = body[15];
    let &[a, b, c, d, e, f] = body.last_chunk::<6>()?;
    let sensor_id = ([a, b, c, d, e, f] != [u8::MAX; 6])
        .then(|| format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}"));

    Some(RawV2 {
        sensor_id,
        temperature_millicelsius: signed(1).map(|value| i32::from(value) * 5),
        relative_humidity_ppm: unsigned(3).map(|value| u32::from(value) * 25),
        pressure_pa: unsigned(5).map(|value| u32::from(value) + 50_000),
        acceleration_mg: [signed(7), signed(9), signed(11)],
        battery_mv: (battery_raw != 0x7FF).then_some(battery_raw + 1_600),
        transmit_power_dbm: (transmit_power_raw != 0x1F)
            .then_some(i16::from(transmit_power_raw) * 2 - 40),
        movement_counter: (movement != u8::MAX).then_some(movement),
        measurement_sequence: unsigned(16),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Ruuvi's valid test vector with a synthetic locally administered MAC.
    const VALID: [u8; BODY_BYTES + 2] = [
        0x99, 0x04, 0x05, 0x12, 0xFC, 0x53, 0x94, 0xC3, 0x7C, 0x00, 0x04, 0xFF, 0xFC, 0x04, 0x0C,
        0xAC, 0x36, 0x42, 0x00, 0xCD, 0x02, 0x12, 0x34, 0x56, 0x78, 0x9A,
    ];

    #[test]
    fn decodes_the_exact_values_and_forward_mac() {
        let decoded = parse_manufacturer_data(&VALID).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            json!({
                "sensor_id": "02:12:34:56:78:9a",
                "temperature_millicelsius": 24_300,
                "relative_humidity_ppm": 534_900,
                "pressure_pa": 100_044,
                "acceleration_mg": [4, -4, 1036],
                "battery_mv": 2977,
                "transmit_power_dbm": 4,
                "movement_counter": 66,
                "measurement_sequence": 205,
            })
        );
    }

    #[test]
    fn serializes_all_unavailable_values_as_null() {
        let mut data = [u8::MAX; BODY_BYTES + 2];
        data[..3].copy_from_slice(&VALID[..3]);
        for offset in [3, 9, 11, 13] {
            data[offset..offset + 2].copy_from_slice(&i16::MIN.to_be_bytes());
        }
        let decoded = parse_manufacturer_data(&data).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            json!({
                "sensor_id": null,
                "temperature_millicelsius": null,
                "relative_humidity_ppm": null,
                "pressure_pa": null,
                "acceleration_mg": [null, null, null],
                "battery_mv": null,
                "transmit_power_dbm": null,
                "movement_counter": null,
                "measurement_sequence": null,
            })
        );
    }

    #[test]
    fn preserves_available_fields_independently_at_wire_boundaries() {
        let mut data = VALID;
        data[3..5].copy_from_slice(&(-32_767i16).to_be_bytes());
        data[5..7].copy_from_slice(&(u16::MAX - 1).to_be_bytes());
        data[7..9].copy_from_slice(&(u16::MAX - 1).to_be_bytes());
        data[11..13].copy_from_slice(&i16::MIN.to_be_bytes());
        data[15..17].copy_from_slice(&[0xFF, 0xFE]);
        let decoded = parse_manufacturer_data(&data).unwrap();
        assert_eq!(decoded.temperature_millicelsius, Some(-163_835));
        assert_eq!(decoded.relative_humidity_ppm, Some(1_638_350));
        assert_eq!(decoded.pressure_pa, Some(115_534));
        assert_eq!(decoded.acceleration_mg, [Some(4), None, Some(1_036)]);
        assert_eq!(decoded.battery_mv, None);
        assert_eq!(decoded.transmit_power_dbm, Some(20));

        data[15..17].copy_from_slice(&[0xFF, 0xDF]);
        let decoded = parse_manufacturer_data(&data).unwrap();
        assert_eq!(decoded.battery_mv, Some(3_646));
        assert_eq!(decoded.transmit_power_dbm, None);
    }

    #[test]
    fn accepts_only_the_company_format_and_exact_length() {
        let mut other_company = VALID;
        other_company.swap(0, 1);
        assert!(parse_manufacturer_data(&other_company).is_none());

        let mut other_format = VALID;
        other_format[2] = 3;
        assert!(parse_manufacturer_data(&other_format).is_none());

        assert!(parse_manufacturer_data(&VALID[..VALID.len() - 1]).is_none());
        let mut overlong = [0; VALID.len() + 1];
        overlong[..VALID.len()].copy_from_slice(&VALID);
        assert!(parse_manufacturer_data(&overlong).is_none());
    }
}
