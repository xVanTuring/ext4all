//! Block group descriptors (32 bytes, or `s_desc_size` with 64bit).

use super::le_fields;
use crate::bytes::lohi;
use crate::csum::{crc16, crc32c};

pub const BG_INODE_UNINIT: u16 = 0x0001;
pub const BG_BLOCK_UNINIT: u16 = 0x0002;
pub const BG_INODE_ZEROED: u16 = 0x0004;

/// Offset of `bg_checksum`.
const CSUM_OFF: usize = 0x1E;
/// Descriptor must be at least this big for the `_hi` bitmap csum fields.
const BLOCK_BITMAP_CSUM_HI_END: usize = 0x3A;
const INODE_BITMAP_CSUM_HI_END: usize = 0x3C;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupDesc {
    pub raw: Vec<u8>,
}

impl GroupDesc {
    le_fields! {
        block_bitmap_lo, set_block_bitmap_lo: u32 @ 0x0;
        inode_bitmap_lo, set_inode_bitmap_lo: u32 @ 0x4;
        inode_table_lo, set_inode_table_lo: u32 @ 0x8;
        free_blocks_count_lo, set_free_blocks_count_lo: u16 @ 0xC;
        free_inodes_count_lo, set_free_inodes_count_lo: u16 @ 0xE;
        used_dirs_count_lo, set_used_dirs_count_lo: u16 @ 0x10;
        flags, set_flags: u16 @ 0x12;
        block_bitmap_csum_lo, set_block_bitmap_csum_lo: u16 @ 0x18;
        inode_bitmap_csum_lo, set_inode_bitmap_csum_lo: u16 @ 0x1A;
        itable_unused_lo, set_itable_unused_lo: u16 @ 0x1C;
        checksum, set_checksum: u16 @ 0x1E;
    }

    pub fn new(raw: &[u8]) -> Self {
        GroupDesc { raw: raw.to_vec() }
    }

    pub fn zeroed(size: usize) -> Self {
        GroupDesc { raw: vec![0; size] }
    }

    fn wide(&self) -> bool {
        self.raw.len() >= 64
    }

    fn get_hi32(&self, off: usize) -> u32 {
        if self.wide() {
            crate::bytes::le32(&self.raw, off)
        } else {
            0
        }
    }

    fn get_hi16(&self, off: usize) -> u32 {
        if self.wide() {
            crate::bytes::le16(&self.raw, off) as u32
        } else {
            0
        }
    }

    fn set_hi32(&mut self, off: usize, v: u32) {
        if self.wide() {
            crate::bytes::set_le32(&mut self.raw, off, v);
        }
    }

    fn set_hi16(&mut self, off: usize, v: u16) {
        if self.wide() {
            crate::bytes::set_le16(&mut self.raw, off, v);
        }
    }

    pub fn block_bitmap(&self) -> u64 {
        lohi(self.block_bitmap_lo(), self.get_hi32(0x20))
    }

    pub fn set_block_bitmap(&mut self, v: u64) {
        self.set_block_bitmap_lo(v as u32);
        self.set_hi32(0x20, (v >> 32) as u32);
    }

    pub fn inode_bitmap(&self) -> u64 {
        lohi(self.inode_bitmap_lo(), self.get_hi32(0x24))
    }

    pub fn set_inode_bitmap(&mut self, v: u64) {
        self.set_inode_bitmap_lo(v as u32);
        self.set_hi32(0x24, (v >> 32) as u32);
    }

    pub fn inode_table(&self) -> u64 {
        lohi(self.inode_table_lo(), self.get_hi32(0x28))
    }

    pub fn set_inode_table(&mut self, v: u64) {
        self.set_inode_table_lo(v as u32);
        self.set_hi32(0x28, (v >> 32) as u32);
    }

    pub fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count_lo() as u32 | (self.get_hi16(0x2C) << 16)
    }

    pub fn set_free_blocks_count(&mut self, v: u32) {
        self.set_free_blocks_count_lo(v as u16);
        self.set_hi16(0x2C, (v >> 16) as u16);
    }

    pub fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count_lo() as u32 | (self.get_hi16(0x2E) << 16)
    }

    pub fn set_free_inodes_count(&mut self, v: u32) {
        self.set_free_inodes_count_lo(v as u16);
        self.set_hi16(0x2E, (v >> 16) as u16);
    }

    pub fn used_dirs_count(&self) -> u32 {
        self.used_dirs_count_lo() as u32 | (self.get_hi16(0x30) << 16)
    }

    pub fn set_used_dirs_count(&mut self, v: u32) {
        self.set_used_dirs_count_lo(v as u16);
        self.set_hi16(0x30, (v >> 16) as u16);
    }

    pub fn itable_unused(&self) -> u32 {
        self.itable_unused_lo() as u32 | (self.get_hi16(0x32) << 16)
    }

    pub fn set_itable_unused(&mut self, v: u32) {
        self.set_itable_unused_lo(v as u16);
        self.set_hi16(0x32, (v >> 16) as u16);
    }

    pub fn has_flag(&self, f: u16) -> bool {
        self.flags() & f != 0
    }

    pub fn clear_flag(&mut self, f: u16) {
        let v = self.flags() & !f;
        self.set_flags(v);
    }

    pub fn set_flag(&mut self, f: u16) {
        let v = self.flags() | f;
        self.set_flags(v);
    }

    /// Stored block bitmap checksum (16 or 32 bits depending on desc size).
    pub fn block_bitmap_csum(&self) -> u32 {
        let lo = self.block_bitmap_csum_lo() as u32;
        if self.raw.len() >= BLOCK_BITMAP_CSUM_HI_END {
            lo | ((crate::bytes::le16(&self.raw, 0x38) as u32) << 16)
        } else {
            lo
        }
    }

    pub fn set_block_bitmap_csum(&mut self, v: u32) {
        self.set_block_bitmap_csum_lo(v as u16);
        if self.raw.len() >= BLOCK_BITMAP_CSUM_HI_END {
            crate::bytes::set_le16(&mut self.raw, 0x38, (v >> 16) as u16);
        }
    }

    pub fn inode_bitmap_csum(&self) -> u32 {
        let lo = self.inode_bitmap_csum_lo() as u32;
        if self.raw.len() >= INODE_BITMAP_CSUM_HI_END {
            lo | ((crate::bytes::le16(&self.raw, 0x3A) as u32) << 16)
        } else {
            lo
        }
    }

    pub fn set_inode_bitmap_csum(&mut self, v: u32) {
        self.set_inode_bitmap_csum_lo(v as u16);
        if self.raw.len() >= INODE_BITMAP_CSUM_HI_END {
            crate::bytes::set_le16(&mut self.raw, 0x3A, (v >> 16) as u16);
        }
    }

    /// Mask for comparing bitmap checksums: only the stored bits count.
    pub fn bitmap_csum_mask(&self) -> u32 {
        if self.raw.len() >= BLOCK_BITMAP_CSUM_HI_END {
            u32::MAX
        } else {
            0xFFFF
        }
    }

    /// Descriptor checksum with metadata_csum (crc32c) semantics.
    pub fn csum_metadata(&self, seed: u32, group: u32) -> u16 {
        let mut c = crc32c(seed, &group.to_le_bytes());
        c = crc32c(c, &self.raw[..CSUM_OFF]);
        c = crc32c(c, &[0, 0]);
        if self.raw.len() > CSUM_OFF + 2 {
            c = crc32c(c, &self.raw[CSUM_OFF + 2..]);
        }
        (c & 0xFFFF) as u16
    }

    /// Descriptor checksum with the older gdt_csum (crc16) semantics.
    pub fn csum_gdt(&self, uuid: &[u8; 16], group: u32) -> u16 {
        let mut c = crc16(!0, uuid);
        c = crc16(c, &group.to_le_bytes());
        c = crc16(c, &self.raw[..CSUM_OFF]);
        if self.raw.len() > CSUM_OFF + 2 {
            c = crc16(c, &self.raw[CSUM_OFF + 2..]);
        }
        c
    }
}

/// Bitmap checksum: crc32c(seed, bitmap[..nbits/8]).
pub fn bitmap_csum(seed: u32, bitmap: &[u8], nbits: u32) -> u32 {
    crc32c(seed, &bitmap[..(nbits as usize).div_ceil(8)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_fields_roundtrip() {
        let mut g = GroupDesc::zeroed(64);
        g.set_block_bitmap(0x1_0000_0010);
        g.set_inode_bitmap(0x2_0000_0020);
        g.set_inode_table(0x3_0000_0030);
        g.set_free_blocks_count(0x12345);
        g.set_free_inodes_count(0x23456);
        g.set_used_dirs_count(0x34567);
        g.set_itable_unused(0x45678);
        assert_eq!(g.block_bitmap(), 0x1_0000_0010);
        assert_eq!(g.inode_bitmap(), 0x2_0000_0020);
        assert_eq!(g.inode_table(), 0x3_0000_0030);
        assert_eq!(g.free_blocks_count(), 0x12345);
        assert_eq!(g.free_inodes_count(), 0x23456);
        assert_eq!(g.used_dirs_count(), 0x34567);
        assert_eq!(g.itable_unused(), 0x45678);
        assert_eq!(g.block_bitmap_lo(), 0x10);
    }

    #[test]
    fn narrow_descriptor_truncates_hi() {
        let mut g = GroupDesc::zeroed(32);
        g.set_block_bitmap(0x1_0000_0010);
        g.set_free_blocks_count(0x12345);
        assert_eq!(g.block_bitmap(), 0x10);
        assert_eq!(g.free_blocks_count(), 0x2345);
        assert_eq!(g.bitmap_csum_mask(), 0xFFFF);
        g.set_block_bitmap_csum(0xAABBCCDD);
        assert_eq!(g.block_bitmap_csum(), 0xCCDD);
        g.set_inode_bitmap_csum(0x11223344);
        assert_eq!(g.inode_bitmap_csum(), 0x3344);
    }

    #[test]
    fn wide_bitmap_csums() {
        let mut g = GroupDesc::zeroed(64);
        g.set_block_bitmap_csum(0xAABBCCDD);
        g.set_inode_bitmap_csum(0x11223344);
        assert_eq!(g.block_bitmap_csum(), 0xAABBCCDD);
        assert_eq!(g.inode_bitmap_csum(), 0x11223344);
        assert_eq!(g.bitmap_csum_mask(), u32::MAX);
    }

    #[test]
    fn flags() {
        let mut g = GroupDesc::zeroed(64);
        g.set_flag(BG_INODE_UNINIT | BG_BLOCK_UNINIT);
        assert!(g.has_flag(BG_BLOCK_UNINIT));
        g.clear_flag(BG_BLOCK_UNINIT);
        assert!(!g.has_flag(BG_BLOCK_UNINIT));
        assert!(g.has_flag(BG_INODE_UNINIT));
    }

    #[test]
    fn checksum_ignores_checksum_field() {
        let mut g = GroupDesc::zeroed(64);
        g.set_block_bitmap(100);
        let a = g.csum_metadata(0x1234, 3);
        g.set_checksum(0xFFFF);
        assert_eq!(a, g.csum_metadata(0x1234, 3));
        // depends on the group number and the seed
        assert_ne!(a, g.csum_metadata(0x1234, 4));
        assert_ne!(a, g.csum_metadata(0x1235, 3));
        let uuid = [7u8; 16];
        let b = g.csum_gdt(&uuid, 3);
        g.set_checksum(0);
        assert_eq!(b, g.csum_gdt(&uuid, 3));
        assert_ne!(b, g.csum_gdt(&uuid, 2));
    }

    #[test]
    fn checksum_matches_manual_computation() {
        let mut g = GroupDesc::zeroed(32);
        g.set_inode_table(55);
        let mut buf = g.raw.clone();
        buf[0x1E] = 0;
        buf[0x1F] = 0;
        let manual = crc32c(crc32c(9, &7u32.to_le_bytes()), &buf) & 0xFFFF;
        assert_eq!(g.csum_metadata(9, 7) as u32, manual);
    }

    #[test]
    fn bitmap_csum_uses_prefix() {
        let mut bm = vec![0u8; 4096];
        bm[100] = 0xff;
        let a = bitmap_csum(1, &bm, 800);
        let b = bitmap_csum(1, &bm, 32768);
        assert_eq!(a, crc32c(1, &bm[..100]));
        assert_eq!(b, crc32c(1, &bm));
    }
}
