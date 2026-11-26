//! Read clear Join-request fields from a CRC-checked LoRaWAN PHY payload.
//!
//! [TS001-1.0.4] sections 1.1, 4.2, and 6.2.5 define field order, sizes,
//! and receiver handling of reserved bits.
//!
//! [TS001-1.0.4]: https://lora-alliance.org/wp-content/uploads/2021/11/LoRaWAN-Link-Layer-Specification-v1.0.4.pdf

use data_encoding::HEXLOWER;
use serde::Serialize;

// MType and Major occupy bits 7–5 and 1–0. Both are zero for a v1.x Join-request.
const MHDR_TYPE_AND_MAJOR: u8 = 0xe3;
const JOIN_REQUEST_BYTES: u64 = 23;

/// Nominal Join-request airtime in quarter symbols for SF7 through SF12.
/// Uses the uplink's explicit header, PHY CRC, CR4/5, and eight-symbol preamble.
/// The [time-on-air formula] includes low data rate optimization when the
/// symbol time exceeds 16 ms.
///
/// [time-on-air formula]: https://github.com/Lora-net/LoRaMac-node/blob/cf779a46f52f9b22395a207c6853f3efb47f0e5b/src/radio/sx1276/sx1276.c#L1435-L1497
pub(super) fn join_airtime_quarter_symbols(spreading_factor: u8, bandwidth_hz: u32) -> u64 {
    let sf = u64::from(spreading_factor);
    let optimized = u64::from((1_u64 << sf) * 1_000 > 16 * u64::from(bandwidth_hz));
    let payload_blocks =
        (8 * JOIN_REQUEST_BYTES + 28 + 16 - 4 * sf).div_ceil(4 * (sf - 2 * optimized));
    // Eight preamble and eight first payload-block symbols, plus the delimiter.
    4 * (16 + 5 * payload_blocks) + 17
}

/// Claimed join identifiers and the unverified MIC in wire order.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub(super) struct JoinRequest {
    join_eui: String,
    dev_eui: String,
    dev_nonce: u16,
    mic_hex: String,
}

impl JoinRequest {
    /// Check the message type, major version, and exact Join-request extent.
    pub(super) fn parse(payload: &[u8]) -> Option<Self> {
        let (&header, body) = payload.split_first()?;
        if header & MHDR_TYPE_AND_MAJOR != 0 {
            return None;
        }
        let (&join_eui, body) = body.split_first_chunk::<8>()?;
        let (&dev_eui, body) = body.split_first_chunk::<8>()?;
        let (&dev_nonce, mic) = body.split_first_chunk::<2>()?;
        let mic: &[u8; 4] = mic.try_into().ok()?;
        Some(Self {
            join_eui: format!("{:016x}", u64::from_le_bytes(join_eui)),
            dev_eui: format!("{:016x}", u64::from_le_bytes(dev_eui)),
            dev_nonce: u16::from_le_bytes(dev_nonce),
            mic_hex: HEXLOWER.encode(mic),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The published PHYPayload from Brocaar's TestPHYPayloadJoinRequest.
    // https://github.com/brocaar/lorawan/blob/4bb09b3bdacba097680894e8d35dbcbc0d260c7e/phypayload_test.go#L474-L503
    const JOIN: [u8; 23] = [
        0x00, 0x04, 0x03, 0x02, 0x01, 0x04, 0x03, 0x02, 0x01, 0x05, 0x04, 0x03, 0x02, 0x05, 0x04,
        0x03, 0x02, 0x2d, 0x10, 0x6a, 0x99, 0x0e, 0x12,
    ];

    #[test]
    fn decodes_published_join_fields_in_display_order() {
        let expected = JoinRequest {
            join_eui: "0102030401020304".into(),
            dev_eui: "0203040502030405".into(),
            dev_nonce: 4141,
            mic_hex: "6a990e12".into(),
        };
        assert_eq!(JoinRequest::parse(&JOIN).unwrap(), expected);
        let mut reserved = JOIN;
        reserved[0] = 0x1c;
        assert_eq!(JoinRequest::parse(&reserved).unwrap(), expected);
    }

    #[test]
    fn rejects_wrong_types_versions_and_extents() {
        for end in 0..JOIN.len() {
            assert!(JoinRequest::parse(&JOIN[..end]).is_none());
        }
        let mut overlong = JOIN.to_vec();
        overlong.push(0);
        assert!(JoinRequest::parse(&overlong).is_none());
        for header in [0x01, 0x20, 0x40, 0xc0, 0xe0] {
            let mut wrong = JOIN;
            wrong[0] = header;
            assert!(JoinRequest::parse(&wrong).is_none());
        }
    }

    #[test]
    fn nominal_airtime_covers_both_us915_join_rates() {
        assert_eq!(join_airtime_quarter_symbols(10, 125_000), 181);
        assert_eq!(join_airtime_quarter_symbols(8, 500_000), 221);
        assert_eq!(join_airtime_quarter_symbols(11, 125_000), 201);
        assert_eq!(join_airtime_quarter_symbols(11, 250_000), 181);
    }
}
