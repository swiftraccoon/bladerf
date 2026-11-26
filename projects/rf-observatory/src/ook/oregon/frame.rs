//! Validate Oregon Scientific temperature, humidity, and wind payloads.
//!
//! The [measured protocol] documents nibble order and sensor fields. The
//! [pinned v2.1 decoder] and [pinned v3 decoder] specify model checks and the
//! eight-bit sum of nibbles. Both supported formats carry two unchecked
//! nibbles after the checksum.
//!
//! [measured protocol]: https://www.osengr.org/Articles/OS-RF-Protocols-IV.pdf#page=28
//! [pinned v2.1 decoder]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/oregon_scientific.c#L139-L245
//! [pinned v3 decoder]: https://github.com/merbanan/rtl_433/blob/39542a090627861a649718f1ed302b4573120311/src/devices/oregon_scientific.c#L786-L821

use serde::Serialize;

const THGR122N_CHECKSUM_INDEX: usize = 15;
const WGR800_CHECKSUM_INDEX: usize = 17;

/// Complete reflected body length in nibbles for the THGR122N.
pub(super) const THGR122N_NIBBLES: usize = THGR122N_CHECKSUM_INDEX + 4;
/// Complete reflected body length in nibbles for the WGR800 family.
pub(super) const WGR800_NIBBLES: usize = WGR800_CHECKSUM_INDEX + 4;

/// A validated THGR122N temperature and humidity reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(in crate::ook) struct Thgr122n {
    /// Sensor's eight-bit power-session code.
    id: u8,
    /// Encoded channel. Switch positions 1, 2, and 3 use 1, 2, and 4.
    channel: u8,
    battery_ok: bool,
    /// Signed temperature in tenths of a degree Celsius.
    temperature_deci_c: i16,
    humidity_percent: u8,
    #[serde(skip)]
    covered_body: [u8; THGR122N_CHECKSUM_INDEX],
}

/// A validated WGR800-family wind reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(in crate::ook) struct Wgr800 {
    /// Encoded WGR800 family type, including its variant discriminator.
    sensor_type: u16,
    /// Sensor's eight-bit power-session code.
    id: u8,
    /// Channel nibble as sent on the wire.
    channel: u8,
    battery_ok: bool,
    /// Gust speed in tenths of a meter per second.
    wind_max_deci_m_s: u16,
    /// Average speed in tenths of a meter per second.
    wind_avg_deci_m_s: u16,
    /// Wind direction in tenths of a degree.
    wind_direction_deci_degrees: u16,
    #[serde(skip)]
    covered_body: [u8; WGR800_CHECKSUM_INDEX],
}

/// Parse a complete THGR122N body with one reflected nibble per byte.
/// Returns `None` for the wrong extent, type, checksum, or field encoding.
pub(super) fn thgr122n(nibbles: &[u8]) -> Option<Thgr122n> {
    let body = checked_body::<THGR122N_CHECKSUM_INDEX>(nibbles)?;
    let channel = body[4];
    if body[..4] != [1, 0xd, 2, 0] || !matches!(channel, 1 | 2 | 4) {
        return None;
    }
    let temperature = i16::try_from(decimal(&body[8..11])?).ok()?;
    Some(Thgr122n {
        id: body[6] << 4 | body[5],
        channel,
        battery_ok: body[7] & 4 == 0,
        temperature_deci_c: if body[11] & 8 == 0 {
            temperature
        } else {
            -temperature
        },
        humidity_percent: u8::try_from(decimal(&body[12..14])?).ok()?,
        covered_body: body,
    })
}

/// Parse a complete WGR800-family body with one reflected nibble per byte.
/// Returns `None` for the wrong extent, type, checksum, or field encoding.
pub(super) fn wgr800(nibbles: &[u8]) -> Option<Wgr800> {
    let body = checked_body::<WGR800_CHECKSUM_INDEX>(nibbles)?;
    let [first, second, third, fourth, ..] = body;
    let sensor_type = u16::from_be_bytes([first << 4 | second, third << 4 | fourth]);
    if !matches!(sensor_type, 0x1984 | 0x1994) {
        return None;
    }
    let wind_max_deci_m_s = decimal(&body[11..14])?;
    let wind_avg_deci_m_s = decimal(&body[14..17])?;
    if wind_max_deci_m_s > 560 || wind_avg_deci_m_s > 560 {
        return None;
    }
    Some(Wgr800 {
        sensor_type,
        id: body[6] << 4 | body[5],
        channel: body[4],
        battery_ok: body[7] & 4 == 0,
        wind_max_deci_m_s,
        wind_avg_deci_m_s,
        wind_direction_deci_degrees: 225 * u16::from(body[8]),
        covered_body: body,
    })
}

fn checked_body<const N: usize>(nibbles: &[u8]) -> Option<[u8; N]> {
    let (body, tail) = nibbles.split_at_checked(N)?;
    let &[checksum_low, checksum_high, _, _] = tail else {
        return None;
    };
    if nibbles.iter().any(|&digit| digit > 0xf) {
        return None;
    }
    let expected = checksum_high << 4 | checksum_low;
    let sum = body.iter().copied().fold(0u8, u8::wrapping_add);
    if sum != expected {
        return None;
    }
    body.try_into().ok()
}

fn decimal(nibbles: &[u8]) -> Option<u16> {
    nibbles.iter().rev().try_fold(0, |value, &digit| {
        (digit <= 9).then_some(value * 10 + u16::from(digit))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reflected frame fixture from the public rtl_433 recordings.
    const THGR122N: [u8; THGR122N_NIBBLES] =
        [1, 13, 2, 0, 1, 8, 15, 0, 8, 8, 1, 0, 4, 5, 0, 2, 4, 5, 3];
    const WGR800: [u8; WGR800_NIBBLES] = [
        1, 9, 8, 4, 0, 0, 14, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 3, 13, 7,
    ];

    #[test]
    fn decodes_the_public_sensor_readings() {
        let temperature = thgr122n(&THGR122N).unwrap();
        assert_eq!(temperature.id, 248);
        assert_eq!(temperature.channel, 1);
        assert!(temperature.battery_ok);
        assert_eq!(temperature.temperature_deci_c, 188);
        assert_eq!(temperature.humidity_percent, 54);

        let wind = wgr800(&WGR800).unwrap();
        assert_eq!(wind.sensor_type, 0x1984);
        assert_eq!(wind.id, 224);
        assert_eq!(wind.channel, 0);
        assert!(wind.battery_ok);
        assert_eq!(wind.wind_max_deci_m_s, 0);
        assert_eq!(wind.wind_avg_deci_m_s, 0);
        assert_eq!(wind.wind_direction_deci_degrees, 0);
    }

    #[test]
    fn preserves_negative_temperature_and_encoded_channel() {
        // Negative-temperature example in the measured protocol, page 34.
        let body = [1, 13, 2, 0, 4, 8, 5, 12, 4, 8, 0, 8, 8, 2, 8, 3, 5, 0, 0];
        let reading = thgr122n(&body).unwrap();
        assert_eq!(reading.channel, 4);
        assert!(!reading.battery_ok);
        assert_eq!(reading.temperature_deci_c, -84);
        assert_eq!(reading.humidity_percent, 28);
    }

    #[test]
    fn requires_complete_payloads_and_exact_model_codes() {
        // The same model code has a shorter body on the THGR228N.
        assert_eq!(thgr122n(&THGR122N[..17]), None);
        assert_eq!(wgr800(&WGR800[..20]), None);
        assert_eq!(thgr122n(&WGR800), None);
        let mut thgr = THGR122N;
        thgr[2] = 3;
        set_checksum(&mut thgr, THGR122N_CHECKSUM_INDEX);
        assert_eq!(thgr122n(&thgr), None);

        let mut wind = WGR800;
        wind[2] = 10;
        set_checksum(&mut wind, WGR800_CHECKSUM_INDEX);
        assert_eq!(wgr800(&wind), None);
    }

    #[test]
    fn decodes_the_older_wgr800_type_and_nonzero_wind_fields() {
        let mut wind = WGR800;
        wind[2] = 9;
        wind[8] = 5;
        wind[11..17].copy_from_slice(&[2, 3, 4, 5, 6, 0]);
        set_checksum(&mut wind, WGR800_CHECKSUM_INDEX);

        let wind = wgr800(&wind).unwrap();
        assert_eq!(wind.sensor_type, 0x1994);
        assert_eq!(wind.id, 224);
        assert_eq!(wind.wind_max_deci_m_s, 432);
        assert_eq!(wind.wind_avg_deci_m_s, 65);
        assert_eq!(wind.wind_direction_deci_degrees, 1125);
    }

    #[test]
    fn separates_checksum_failures_from_invalid_field_encodings() {
        let mut thgr = THGR122N;
        thgr[12] = 6;
        assert_eq!(thgr122n(&thgr), None);

        thgr[12] = 0xa;
        set_checksum(&mut thgr, THGR122N_CHECKSUM_INDEX);
        assert_eq!(thgr122n(&thgr), None);

        let mut wind = WGR800;
        wind[11] = 0xa;
        set_checksum(&mut wind, WGR800_CHECKSUM_INDEX);
        assert_eq!(wgr800(&wind), None);
    }

    #[test]
    fn groups_copies_by_checked_bits_without_requiring_a_matching_trailer() {
        let mut repeat = WGR800;
        repeat[19] ^= 1;
        assert_eq!(wgr800(&repeat), wgr800(&WGR800));

        repeat[9] ^= 1;
        set_checksum(&mut repeat, WGR800_CHECKSUM_INDEX);
        let original = wgr800(&WGR800).unwrap();
        let changed = wgr800(&repeat).unwrap();
        assert_ne!(changed, original);
        assert_eq!(
            serde_json::to_value(changed).unwrap(),
            serde_json::to_value(original).unwrap()
        );
    }

    fn set_checksum(frame: &mut [u8], index: usize) {
        let sum = frame[..index]
            .iter()
            .map(|&digit| u16::from(digit))
            .sum::<u16>();
        frame[index] = (sum as u8) & 0xf;
        frame[index + 1] = (sum as u8) >> 4;
    }
}
