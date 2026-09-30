//! Extent tree node layout.
//!
//! A node is a 12-byte header followed by 12-byte entries: index entries in
//! interior nodes, extents in leaves. The root lives in the inode's 60-byte
//! `i_block`; other nodes occupy one block each and carry a 4-byte checksum
//! tail right after `eh_max` entries.

use crate::bytes::{le16, le32, set_le16, set_le32};
use crate::csum::crc32c;

pub const EXTENT_MAGIC: u16 = 0xF30A;
pub const HEADER_SIZE: usize = 12;
pub const ENTRY_SIZE: usize = 12;
/// Longest initialized extent.
pub const MAX_INIT_LEN: u32 = 32768;
/// Longest unwritten (preallocated) extent.
pub const MAX_UNWRITTEN_LEN: u32 = 32767;
/// Deepest tree Linux will build.
pub const MAX_DEPTH: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentHeader {
    pub magic: u16,
    pub entries: u16,
    pub max: u16,
    pub depth: u16,
    pub generation: u32,
}

impl ExtentHeader {
    pub fn parse(b: &[u8]) -> Self {
        ExtentHeader {
            magic: le16(b, 0),
            entries: le16(b, 2),
            max: le16(b, 4),
            depth: le16(b, 6),
            generation: le32(b, 8),
        }
    }

    pub fn write(&self, b: &mut [u8]) {
        set_le16(b, 0, self.magic);
        set_le16(b, 2, self.entries);
        set_le16(b, 4, self.max);
        set_le16(b, 6, self.depth);
        set_le32(b, 8, self.generation);
    }

    /// A fresh header for a node of `node_len` bytes.
    pub fn empty(node_len: usize, depth: u16) -> Self {
        ExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: max_entries(node_len),
            depth,
            generation: 0,
        }
    }
}

/// Max entries that fit in a node of `node_len` bytes (block nodes reserve
/// room for the checksum tail).
pub fn max_entries(node_len: usize) -> u16 {
    if node_len == 60 {
        4
    } else {
        ((node_len - HEADER_SIZE - 4) / ENTRY_SIZE) as u16
    }
}

/// A leaf entry: maps `len` logical blocks at `block` to physical `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent {
    pub block: u32,
    pub len: u32,
    pub start: u64,
    pub unwritten: bool,
}

impl Extent {
    pub fn parse(b: &[u8]) -> Self {
        let raw_len = le16(b, 4) as u32;
        let (len, unwritten) = if raw_len > MAX_INIT_LEN {
            (raw_len - MAX_INIT_LEN, true)
        } else {
            (raw_len, false)
        };
        Extent {
            block: le32(b, 0),
            len,
            start: (le32(b, 8) as u64) | ((le16(b, 6) as u64) << 32),
            unwritten,
        }
    }

    pub fn write(&self, b: &mut [u8]) {
        set_le32(b, 0, self.block);
        let raw_len = if self.unwritten {
            self.len + MAX_INIT_LEN
        } else {
            self.len
        };
        set_le16(b, 4, raw_len as u16);
        set_le16(b, 6, (self.start >> 32) as u16);
        set_le32(b, 8, self.start as u32);
    }

    /// One past the last logical block.
    pub fn end(&self) -> u64 {
        self.block as u64 + self.len as u64
    }

    pub fn contains(&self, lblk: u32) -> bool {
        lblk >= self.block && (lblk as u64) < self.end()
    }

    pub fn max_len(&self) -> u32 {
        if self.unwritten {
            MAX_UNWRITTEN_LEN
        } else {
            MAX_INIT_LEN
        }
    }
}

/// An interior entry: subtree covering logical blocks from `block` lives at
/// physical block `leaf`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentIndex {
    pub block: u32,
    pub leaf: u64,
}

impl ExtentIndex {
    pub fn parse(b: &[u8]) -> Self {
        ExtentIndex {
            block: le32(b, 0),
            leaf: (le32(b, 4) as u64) | ((le16(b, 8) as u64) << 32),
        }
    }

    pub fn write(&self, b: &mut [u8]) {
        set_le32(b, 0, self.block);
        set_le32(b, 4, self.leaf as u32);
        set_le16(b, 8, (self.leaf >> 32) as u16);
        set_le16(b, 10, 0);
    }
}

/// Offset of the checksum tail in a block node.
pub fn tail_offset(max: u16) -> usize {
    HEADER_SIZE + ENTRY_SIZE * max as usize
}

pub fn block_csum(inode_seed: u32, node: &[u8]) -> u32 {
    let h = ExtentHeader::parse(node);
    crc32c(inode_seed, &node[..tail_offset(h.max)])
}

pub fn verify_block_csum(inode_seed: u32, node: &[u8]) -> bool {
    let h = ExtentHeader::parse(node);
    let off = tail_offset(h.max);
    off + 4 <= node.len() && le32(node, off) == block_csum(inode_seed, node)
}

pub fn set_block_csum(inode_seed: u32, node: &mut [u8]) {
    let h = ExtentHeader::parse(node);
    let off = tail_offset(h.max);
    let c = block_csum(inode_seed, node);
    set_le32(node, off, c);
}

/// Entry `i` of a node (either kind), as a byte slice.
pub fn entry(node: &[u8], i: usize) -> &[u8] {
    let o = HEADER_SIZE + i * ENTRY_SIZE;
    &node[o..o + ENTRY_SIZE]
}

pub fn entry_mut(node: &mut [u8], i: usize) -> &mut [u8] {
    let o = HEADER_SIZE + i * ENTRY_SIZE;
    &mut node[o..o + ENTRY_SIZE]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = ExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 3,
            max: 4,
            depth: 1,
            generation: 7,
        };
        let mut b = [0u8; 12];
        h.write(&mut b);
        assert_eq!(b[0..2], [0x0A, 0xF3]);
        assert_eq!(ExtentHeader::parse(&b), h);
    }

    #[test]
    fn max_entries_by_size() {
        assert_eq!(max_entries(60), 4);
        assert_eq!(max_entries(1024), 84);
        assert_eq!(max_entries(4096), 340);
        assert_eq!(ExtentHeader::empty(4096, 2).max, 340);
        assert_eq!(ExtentHeader::empty(60, 0).depth, 0);
    }

    #[test]
    fn extent_roundtrip() {
        let e = Extent {
            block: 100,
            len: 32768,
            start: 0x1234_5678_9a,
            unwritten: false,
        };
        let mut b = [0u8; 12];
        e.write(&mut b);
        assert_eq!(Extent::parse(&b), e);
        let u = Extent {
            block: 5,
            len: 32767,
            start: 9,
            unwritten: true,
        };
        u.write(&mut b);
        assert_eq!(le16(&b, 4), 65535);
        assert_eq!(Extent::parse(&b), u);
        assert_eq!(u.max_len(), MAX_UNWRITTEN_LEN);
        assert_eq!(e.max_len(), MAX_INIT_LEN);
    }

    #[test]
    fn extent_ranges() {
        let e = Extent {
            block: 10,
            len: 5,
            start: 0,
            unwritten: false,
        };
        assert!(!e.contains(9));
        assert!(e.contains(10));
        assert!(e.contains(14));
        assert!(!e.contains(15));
        assert_eq!(e.end(), 15);
        let top = Extent {
            block: u32::MAX,
            len: 1,
            start: 0,
            unwritten: false,
        };
        assert!(top.contains(u32::MAX));
        assert_eq!(top.end(), 1u64 << 32);
    }

    #[test]
    fn index_roundtrip() {
        let i = ExtentIndex {
            block: 77,
            leaf: 0xABCD_1234_5678,
        };
        let mut b = [0xffu8; 12];
        i.write(&mut b);
        assert_eq!(ExtentIndex::parse(&b), i);
        assert_eq!(le16(&b, 10), 0);
    }

    #[test]
    fn block_checksum() {
        let mut node = vec![0u8; 1024];
        ExtentHeader::empty(1024, 0).write(&mut node);
        set_block_csum(42, &mut node);
        assert!(verify_block_csum(42, &node));
        assert!(!verify_block_csum(43, &node));
        assert_eq!(tail_offset(84), 1020);
        node[20] = 1;
        assert!(!verify_block_csum(42, &node));
    }

    #[test]
    fn entry_slices() {
        let mut node = vec![0u8; 60];
        let e = Extent {
            block: 1,
            len: 2,
            start: 3,
            unwritten: false,
        };
        e.write(entry_mut(&mut node, 3));
        assert_eq!(Extent::parse(entry(&node, 3)), e);
        assert_eq!(entry(&node, 0), &[0u8; 12]);
    }
}
