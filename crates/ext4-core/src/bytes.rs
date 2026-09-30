//! Little/big endian field access over raw byte buffers.
//!
//! ext4 on-disk structures are little endian, jbd2 structures are big endian.
//! All structures in this crate keep their raw bytes and read/write fields in
//! place, so unknown fields survive a read-modify-write cycle untouched.

#[inline]
pub fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

#[inline]
pub fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn le64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

#[inline]
pub fn set_le16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn set_le32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn set_le64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

#[inline]
pub fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
pub fn be64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_be_bytes(a)
}

#[inline]
pub fn set_be32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn set_be64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_be_bytes());
}

/// Combine a `lo` and `hi` half into a 64 bit value.
#[inline]
pub fn lohi(lo: u32, hi: u32) -> u64 {
    (lo as u64) | ((hi as u64) << 32)
}

/// Round `v` up to a multiple of `align` (power of two not required).
#[inline]
pub fn round_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

/// Returns the trimmed (NUL-terminated) prefix of a fixed-size byte field.
pub fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(p) => &b[..p],
        None => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_roundtrip() {
        let mut b = [0u8; 16];
        set_le16(&mut b, 1, 0xBEEF);
        set_le32(&mut b, 3, 0xDEADBEEF);
        set_le64(&mut b, 7, 0x0102030405060708);
        assert_eq!(le16(&b, 1), 0xBEEF);
        assert_eq!(le32(&b, 3), 0xDEADBEEF);
        assert_eq!(le64(&b, 7), 0x0102030405060708);
        assert_eq!(b[1], 0xEF);
        assert_eq!(b[2], 0xBE);
        assert_eq!(b[7], 0x08);
    }

    #[test]
    fn be_roundtrip() {
        let mut b = [0u8; 12];
        set_be32(&mut b, 0, 0xC03B3998);
        set_be64(&mut b, 4, 0x1122334455667788);
        assert_eq!(b[0], 0xC0);
        assert_eq!(be32(&b, 0), 0xC03B3998);
        assert_eq!(be64(&b, 4), 0x1122334455667788);
        assert_eq!(b[4], 0x11);
    }

    #[test]
    fn helpers() {
        assert_eq!(lohi(1, 2), (2u64 << 32) | 1);
        assert_eq!(round_up(0, 4), 0);
        assert_eq!(round_up(1, 4), 4);
        assert_eq!(round_up(4, 4), 4);
        assert_eq!(round_up(13, 12), 24);
        assert_eq!(cstr(b"abc\0def"), b"abc");
        assert_eq!(cstr(b"abc"), b"abc");
        assert_eq!(cstr(b"\0"), b"");
    }
}
