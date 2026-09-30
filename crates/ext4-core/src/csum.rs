//! Checksums used by ext4 and jbd2.
//!
//! The Linux kernel's `crc32c(seed, data)` is the *raw* CRC32C update: no
//! pre- or post-inversion. ext4 always seeds it with `~0` (or a derived seed)
//! and stores the result as-is. The `crc32c` crate implements the standard
//! (inverted) variant, so we convert at the boundary.

/// Linux-style raw CRC32C update: `crc32c_le(seed, data)`.
#[inline]
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    !crc32c::crc32c_append(!seed, data)
}

/// CRC16 (ANSI, polynomial 0x8005 reflected = 0xA001) as used by the old
/// `uninit_bg` (GDT_CSUM) group descriptor checksums.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ CRC16_TABLE[((crc ^ b as u16) & 0xff) as usize];
    }
    crc
}

static CRC16_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            if c & 1 != 0 {
                c = (c >> 1) ^ 0xA001;
            } else {
                c >>= 1;
            }
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_standard_check_value() {
        // Standard CRC32C("123456789") = 0xE3069283. Linux raw form with seed
        // ~0 returns the value before final inversion.
        assert_eq!(crc32c::crc32c(b"123456789"), 0xE3069283);
        assert_eq!(crc32c(!0, b"123456789"), !0xE3069283u32);
    }

    #[test]
    fn crc32c_is_incremental() {
        let whole = crc32c(!0, b"hello world");
        let part = crc32c(crc32c(!0, b"hello "), b"world");
        assert_eq!(whole, part);
    }

    #[test]
    fn crc32c_empty_is_identity() {
        assert_eq!(crc32c(0x1234_5678, b""), 0x1234_5678);
    }

    #[test]
    fn crc16_check_value() {
        // CRC-16/ARC("123456789") = 0xBB3D (init 0, reflected 0x8005)
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
        // CRC-16/MODBUS uses init 0xFFFF → 0x4B37
        assert_eq!(crc16(0xFFFF, b"123456789"), 0x4B37);
    }

    #[test]
    fn crc16_incremental() {
        assert_eq!(crc16(crc16(!0, b"abc"), b"def"), crc16(!0, b"abcdef"));
    }
}
