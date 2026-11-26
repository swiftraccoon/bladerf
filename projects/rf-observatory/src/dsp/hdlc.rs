//! Link-layer integrity checks shared by HDLC-based radio protocols.

/// Compute the HDLC 16-bit frame check in host byte order.
///
/// CRC-16/IBM-SDLC uses reflected polynomial `0x8408`, initial value
/// `0xffff`, and a final complement. Its wire octets are little-endian.
pub(crate) fn fcs(bytes: &[u8]) -> u16 {
    let mut crc = 0xffff;
    for &byte in bytes {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            let polynomial = if crc & 1 == 0 { 0 } else { 0x8408 };
            crc = (crc >> 1) ^ polynomial;
        }
    }
    !crc
}
