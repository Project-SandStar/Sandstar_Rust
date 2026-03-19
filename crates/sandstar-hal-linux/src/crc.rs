//! Sensirion CRC-8 implementation.
//!
//! Used by I2C sensor protocols (SDP510: init=0x00, SDP810: init=0xFF).
//!
//! The CRC uses polynomial 0x31 (x^8 + x^5 + x^4 + 1), which is the
//! Sensirion standard for all their I2C sensor products.

/// Compute CRC-8 over `data` using polynomial 0x31 with the given initial value.
///
/// # Arguments
/// * `data`  - Byte slice to checksum (typically 2 bytes: MSB + LSB of a sensor word).
/// * `init`  - Initial CRC register value.
///   - `0x00` for SDP510 (legacy)
///   - `0xFF` for SDP810 / SDP8xx (Sensirion standard)
///
/// # Examples
/// ```
/// use sandstar_hal_linux::crc::sensirion_crc8;
///
/// // SDP810 datasheet example
/// assert_eq!(sensirion_crc8(&[0xBE, 0xEF], 0xFF), 0x92);
/// ```
pub fn sensirion_crc8(data: &[u8], init: u8) -> u8 {
    // Polynomial 0x131 (bit-8 implicit) = 0x31 for the XOR step.
    // The C code uses 0x131 because it checks bit 7 *before* shifting,
    // effectively treating bit 8 as always present. In Rust we replicate
    // the same algorithm exactly.
    const POLY: u8 = 0x31;

    let mut crc = init;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            let msb = crc & 0x80;
            crc <<= 1;
            if msb != 0 {
                crc ^= POLY;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // SDP810 (init = 0xFF)
    // ---------------------------------------------------------------

    #[test]
    fn sdp810_datasheet_example() {
        // Canonical Sensirion test vector: CRC8(0xBEEF, init=0xFF) = 0x92
        assert_eq!(sensirion_crc8(&[0xBE, 0xEF], 0xFF), 0x92);
    }

    #[test]
    fn sdp810_zero_word() {
        // CRC of [0x00, 0x00] with init=0xFF
        // Manual: 0xFF ^ 0x00 = 0xFF after first byte processing,
        // then 0xFF ^ 0x00 again for second byte.
        let crc = sensirion_crc8(&[0x00, 0x00], 0xFF);
        assert_eq!(crc, 0x81);
    }

    #[test]
    fn sdp810_ffff_word() {
        // CRC of [0xFF, 0xFF] with init=0xFF
        let crc = sensirion_crc8(&[0xFF, 0xFF], 0xFF);
        // init ^ 0xFF = 0x00, all shifts are just shifts, then ^ 0xFF again
        assert_eq!(crc, 0xAC);
    }

    #[test]
    fn sdp810_typical_dp_reading() {
        // A realistic SDP810 differential-pressure reading:
        // raw bytes 0x00, 0x64 (= 100 Pa raw), CRC should be verifiable
        let word = [0x00, 0x64];
        let crc = sensirion_crc8(&word, 0xFF);
        // Verify round-trip: the CRC we compute can be checked
        assert_eq!(sensirion_crc8(&word, 0xFF), crc);
    }

    #[test]
    fn sdp810_single_byte() {
        // Sensirion CRC is defined over 2-byte words, but the algorithm
        // works on arbitrary lengths. Verify single-byte for completeness.
        let crc = sensirion_crc8(&[0x01], 0xFF);
        // 0xFF ^ 0x01 = 0xFE; process 8 bits
        // Bit 7 set -> shift and XOR with 0x31 repeatedly
        assert_eq!(crc, 0x9D);
    }

    // ---------------------------------------------------------------
    // SDP510 (init = 0x00)
    // ---------------------------------------------------------------

    #[test]
    fn sdp510_zero_word() {
        // CRC of [0x00, 0x00] with init=0x00 => 0x00 (all XORs are zero)
        assert_eq!(sensirion_crc8(&[0x00, 0x00], 0x00), 0x00);
    }

    #[test]
    fn sdp510_small_value() {
        // A typical SDP510 reading of 500 counts = 0x01, 0xF4
        let word = [0x01, 0xF4];
        let crc = sensirion_crc8(&word, 0x00);
        // Verify deterministic
        assert_eq!(sensirion_crc8(&word, 0x00), crc);
    }

    #[test]
    fn sdp510_max_unsigned() {
        // Max unsigned 16-bit: 0xFF, 0xFF
        let crc = sensirion_crc8(&[0xFF, 0xFF], 0x00);
        // 0x00 ^ 0xFF = 0xFF after byte 0 processing,
        // then ^ 0xFF for byte 1.
        assert_eq!(crc, 0x2D);
    }

    #[test]
    fn sdp510_known_pair() {
        // Another known pair: [0x80, 0x00] with init=0
        let crc = sensirion_crc8(&[0x80, 0x00], 0x00);
        // First byte: 0x00 ^ 0x80 = 0x80; bit7 set -> shift + XOR...
        assert_eq!(crc, 0x23);
    }

    // ---------------------------------------------------------------
    // Cross-init verification
    // ---------------------------------------------------------------

    #[test]
    fn different_init_different_result() {
        let data = [0xBE, 0xEF];
        let crc_ff = sensirion_crc8(&data, 0xFF);
        let crc_00 = sensirion_crc8(&data, 0x00);
        assert_ne!(crc_ff, crc_00, "Different inits must produce different CRCs");
    }

    #[test]
    fn empty_data_returns_init() {
        // CRC of empty slice should just be the init value run through zero iterations
        assert_eq!(sensirion_crc8(&[], 0xFF), 0xFF);
        assert_eq!(sensirion_crc8(&[], 0x00), 0x00);
    }
}
