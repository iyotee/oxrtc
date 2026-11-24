//! The CRC of a PSI section (ISO/IEC 13818-1 annex A): CRC-32 with the
//! polynomial `0x04C11DB7`, from `0xFFFFFFFF`, not reflected, not inverted.
//! Bit by bit: the writer computes it once per table, and a table is a few
//! dozen bytes.

/// The CRC of `data`: written after a section, the CRC of the section and
/// its CRC together is 0.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    data.iter().fold(0xFFFF_FFFF, |mut crc: u32, byte| {
        crc ^= u32::from(*byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn is_crc32_mpeg2() {
        // The check value of CRC-32/MPEG-2.
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
        let mut section = b"any section".to_vec();
        section.extend_from_slice(&crc32(&section).to_be_bytes());
        assert_eq!(crc32(&section), 0);
    }
}
