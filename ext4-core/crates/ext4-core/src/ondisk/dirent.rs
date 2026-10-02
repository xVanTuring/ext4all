//! Directory entry blocks (linear format + checksum tail) and htree nodes.

use crate::bytes::{le16, le32, set_le16, set_le32};
use crate::csum::crc32c;
use crate::error::{Error, Result};

pub const DIRENT_HEADER: usize = 8;
pub const TAIL_SIZE: usize = 12;
pub const TAIL_FT: u8 = 0xDE;
pub const MAX_NAME_LEN: usize = 255;

/// Space a dirent with a `name_len` byte name needs (rounded to 4).
pub fn rec_len_for(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len + 3) & !3
}

/// Decode an on-disk rec_len (handles 64K blocks).
pub fn rec_len_from_disk(v: u16, block_size: usize) -> usize {
    let len = v as usize;
    if block_size < 65536 {
        len
    } else if len == 65535 || len == 0 {
        block_size
    } else {
        (len & 65532) | ((len & 3) << 16)
    }
}

pub fn rec_len_to_disk(len: usize, block_size: usize) -> u16 {
    if block_size < 65536 {
        len as u16
    } else if len == block_size {
        if block_size == 65536 { 65535 } else { 0 }
    } else {
        ((len & 65532) | ((len >> 16) & 3)) as u16
    }
}

/// A parsed directory entry located at `offset` within its block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub offset: usize,
    pub inode: u32,
    pub rec_len: usize,
    pub name_len: usize,
    pub file_type: u8,
}

impl DirEntry {
    pub fn name<'a>(&self, block: &'a [u8]) -> &'a [u8] {
        &block[self.offset + DIRENT_HEADER..self.offset + DIRENT_HEADER + self.name_len]
    }

    /// Bytes actually used by this entry (0 for an unused slot).
    pub fn used_len(&self) -> usize {
        if self.inode == 0 { 0 } else { rec_len_for(self.name_len) }
    }
}

/// Write a dirent header + name at `off`.
pub fn write_entry(
    block: &mut [u8],
    off: usize,
    inode: u32,
    rec_len: usize,
    name: &[u8],
    file_type: u8,
    block_size: usize,
) {
    set_le32(block, off, inode);
    set_le16(block, off + 4, rec_len_to_disk(rec_len, block_size));
    block[off + 6] = name.len() as u8;
    block[off + 7] = file_type;
    block[off + 8..off + 8 + name.len()].copy_from_slice(name);
    // zero padding up to the 4-byte boundary for determinism
    let used = rec_len_for(name.len());
    let pad_end = (off + used).min(off + rec_len);
    for b in &mut block[off + 8 + name.len()..pad_end] {
        *b = 0;
    }
}

pub fn set_rec_len(block: &mut [u8], off: usize, rec_len: usize, block_size: usize) {
    set_le16(block, off + 4, rec_len_to_disk(rec_len, block_size));
}

pub fn set_inode(block: &mut [u8], off: usize, inode: u32) {
    set_le32(block, off, inode);
}

/// Iterate entries of a linear block. `limit` excludes the checksum tail.
/// Validates structure (Linux `ext4_check_dir_entry`).
pub fn parse_block(block: &[u8], limit: usize, block_size: usize) -> Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < limit {
        if off + DIRENT_HEADER > limit {
            return Err(Error::corrupt(format!("dirent header overruns block at {off}")));
        }
        let inode = le32(block, off);
        let rec_len = rec_len_from_disk(le16(block, off + 4), block_size);
        let name_len = block[off + 6] as usize;
        let file_type = block[off + 7];
        if rec_len < rec_len_for(1) {
            return Err(Error::corrupt(format!("dirent rec_len {rec_len} too small at {off}")));
        }
        if rec_len % 4 != 0 {
            return Err(Error::corrupt(format!("dirent rec_len {rec_len} unaligned at {off}")));
        }
        if rec_len < rec_len_for(name_len) {
            return Err(Error::corrupt(format!(
                "dirent rec_len {rec_len} < name_len {name_len} at {off}"
            )));
        }
        if off + rec_len > limit {
            return Err(Error::corrupt(format!("dirent overruns block at {off}")));
        }
        out.push(DirEntry {
            offset: off,
            inode,
            rec_len,
            name_len,
            file_type,
        });
        off += rec_len;
    }
    Ok(out)
}

/// Whether a block ends with a valid checksum tail dirent.
pub fn has_tail(block: &[u8]) -> bool {
    let bs = block.len();
    let t = bs - TAIL_SIZE;
    le32(block, t) == 0 && le16(block, t + 4) as usize == TAIL_SIZE && block[t + 6] == 0 && block[t + 7] == TAIL_FT
}

pub fn init_tail(block: &mut [u8]) {
    let bs = block.len();
    let t = bs - TAIL_SIZE;
    block[t..].fill(0);
    set_le16(block, t + 4, TAIL_SIZE as u16);
    block[t + 7] = TAIL_FT;
}

pub fn leaf_csum(inode_seed: u32, block: &[u8]) -> u32 {
    crc32c(inode_seed, &block[..block.len() - TAIL_SIZE])
}

pub fn verify_leaf_csum(inode_seed: u32, block: &[u8]) -> bool {
    has_tail(block) && le32(block, block.len() - 4) == leaf_csum(inode_seed, block)
}

pub fn set_leaf_csum(inode_seed: u32, block: &mut [u8]) {
    let c = leaf_csum(inode_seed, block);
    let n = block.len();
    set_le32(block, n - 4, c);
}

/// Initialize an empty linear directory block (one unused entry spanning
/// the block, plus the checksum tail when `csum` is set).
pub fn init_empty_block(block: &mut [u8], csum: bool) {
    let bs = block.len();
    block.fill(0);
    let limit = if csum { bs - TAIL_SIZE } else { bs };
    set_le32(block, 0, 0);
    set_le16(block, 4, rec_len_to_disk(limit, bs));
    if csum {
        init_tail(block);
    }
}

// ---------------------------------------------------------------------------
// htree (dx) nodes

pub const DX_ROOT_INFO_OFF: usize = 24;
pub const DX_ROOT_ENTRIES_OFF: usize = 32;
pub const DX_NODE_ENTRIES_OFF: usize = 8;
pub const DX_TAIL_SIZE: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DxRootInfo {
    pub hash_version: u8,
    pub info_length: u8,
    pub indirect_levels: u8,
    pub unused_flags: u8,
}

impl DxRootInfo {
    pub fn parse(block: &[u8]) -> Self {
        let o = DX_ROOT_INFO_OFF;
        DxRootInfo {
            hash_version: block[o + 4],
            info_length: block[o + 5],
            indirect_levels: block[o + 6],
            unused_flags: block[o + 7],
        }
    }

    pub fn write(&self, block: &mut [u8]) {
        let o = DX_ROOT_INFO_OFF;
        set_le32(block, o, 0);
        block[o + 4] = self.hash_version;
        block[o + 5] = self.info_length;
        block[o + 6] = self.indirect_levels;
        block[o + 7] = self.unused_flags;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DxEntry {
    pub hash: u32,
    pub block: u32,
}

/// View over the count/limit header + entries at `off`.
pub fn dx_count(block: &[u8], off: usize) -> u16 {
    le16(block, off + 2)
}

pub fn dx_limit(block: &[u8], off: usize) -> u16 {
    le16(block, off)
}

pub fn set_dx_count(block: &mut [u8], off: usize, v: u16) {
    set_le16(block, off + 2, v);
}

pub fn set_dx_limit(block: &mut [u8], off: usize, v: u16) {
    set_le16(block, off, v);
}

/// Entry `i` (entry 0's hash slot holds count/limit and reads as hash 0).
pub fn dx_entry(block: &[u8], off: usize, i: usize) -> DxEntry {
    let o = off + i * 8;
    DxEntry {
        hash: if i == 0 { 0 } else { le32(block, o) },
        block: le32(block, o + 4),
    }
}

pub fn set_dx_entry(block: &mut [u8], off: usize, i: usize, e: DxEntry) {
    let o = off + i * 8;
    if i != 0 {
        set_le32(block, o, e.hash);
    }
    set_le32(block, o + 4, e.block);
}

pub fn dx_root_limit(block_size: usize, csum: bool) -> u16 {
    ((block_size - DX_ROOT_ENTRIES_OFF - if csum { DX_TAIL_SIZE } else { 0 }) / 8) as u16
}

pub fn dx_node_limit(block_size: usize, csum: bool) -> u16 {
    ((block_size - DX_NODE_ENTRIES_OFF - if csum { DX_TAIL_SIZE } else { 0 }) / 8) as u16
}

/// dx node checksum: covers header up to `count` entries, then the tail's
/// reserved word and a zeroed checksum slot.
pub fn dx_csum(inode_seed: u32, block: &[u8], entries_off: usize) -> u32 {
    let count = dx_count(block, entries_off) as usize;
    let limit = dx_limit(block, entries_off) as usize;
    let size = entries_off + count * 8;
    let tail = entries_off + limit * 8;
    let mut c = crc32c(inode_seed, &block[..size]);
    c = crc32c(c, &block[tail..tail + 4]);
    crc32c(c, &[0, 0, 0, 0])
}

pub fn verify_dx_csum(inode_seed: u32, block: &[u8], entries_off: usize) -> bool {
    let limit = dx_limit(block, entries_off) as usize;
    let tail = entries_off + limit * 8;
    if tail + DX_TAIL_SIZE > block.len() {
        return false;
    }
    le32(block, tail + 4) == dx_csum(inode_seed, block, entries_off)
}

pub fn set_dx_csum(inode_seed: u32, block: &mut [u8], entries_off: usize) {
    let limit = dx_limit(block, entries_off) as usize;
    let tail = entries_off + limit * 8;
    let c = dx_csum(inode_seed, block, entries_off);
    set_le32(block, tail, 0);
    set_le32(block, tail + 4, c);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rec_len_sizes() {
        assert_eq!(rec_len_for(1), 12);
        assert_eq!(rec_len_for(2), 12);
        assert_eq!(rec_len_for(4), 12);
        assert_eq!(rec_len_for(5), 16);
        assert_eq!(rec_len_for(255), 264);
    }

    #[test]
    fn rec_len_disk_encoding() {
        for bs in [1024usize, 4096] {
            assert_eq!(rec_len_from_disk(rec_len_to_disk(bs, bs), bs), bs);
            assert_eq!(rec_len_from_disk(rec_len_to_disk(12, bs), bs), 12);
        }
        let bs = 65536;
        assert_eq!(rec_len_to_disk(65536, bs), 65535);
        assert_eq!(rec_len_from_disk(65535, bs), 65536);
        assert_eq!(rec_len_from_disk(0, bs), 65536);
        assert_eq!(rec_len_from_disk(rec_len_to_disk(65532, bs), bs), 65532);
        assert_eq!(rec_len_from_disk(rec_len_to_disk(12, bs), bs), 12);
    }

    fn sample_block(csum: bool) -> Vec<u8> {
        let bs = 1024;
        let mut b = vec![0u8; bs];
        let limit = if csum { bs - TAIL_SIZE } else { bs };
        write_entry(&mut b, 0, 2, 12, b".", 2, bs);
        write_entry(&mut b, 12, 2, 12, b"..", 2, bs);
        write_entry(&mut b, 24, 12, limit - 24, b"hello.txt", 1, bs);
        if csum {
            init_tail(&mut b);
        }
        b
    }

    #[test]
    fn parse_linear_block() {
        let b = sample_block(false);
        let es = parse_block(&b, 1024, 1024).unwrap();
        assert_eq!(es.len(), 3);
        assert_eq!(es[0].name(&b), b".");
        assert_eq!(es[1].name(&b), b"..");
        assert_eq!(es[2].name(&b), b"hello.txt");
        assert_eq!(es[2].inode, 12);
        assert_eq!(es[2].file_type, 1);
        assert_eq!(es[2].rec_len, 1000);
        assert_eq!(es[2].used_len(), 20);
    }

    #[test]
    fn parse_with_tail_and_checksum() {
        let mut b = sample_block(true);
        assert!(has_tail(&b));
        let es = parse_block(&b, 1024 - TAIL_SIZE, 1024).unwrap();
        assert_eq!(es.len(), 3);
        set_leaf_csum(99, &mut b);
        assert!(verify_leaf_csum(99, &b));
        assert!(!verify_leaf_csum(98, &b));
        b[30] ^= 1;
        assert!(!verify_leaf_csum(99, &b));
    }

    #[test]
    fn corrupt_blocks_rejected() {
        let mut b = sample_block(false);
        set_le16(&mut b, 4, 13); // unaligned
        assert!(parse_block(&b, 1024, 1024).is_err());

        let mut b = sample_block(false);
        set_le16(&mut b, 28, 2000); // overruns
        assert!(parse_block(&b, 1024, 1024).is_err());

        let mut b = sample_block(false);
        b[12 + 6] = 5; // name longer than rec_len 12
        assert!(parse_block(&b, 1024, 1024).is_err());

        let mut b = sample_block(false);
        set_le16(&mut b, 4, 4); // too small
        assert!(parse_block(&b, 1024, 1024).is_err());
    }

    #[test]
    fn empty_block() {
        let mut b = vec![0xAAu8; 4096];
        init_empty_block(&mut b, true);
        assert!(has_tail(&b));
        let es = parse_block(&b, 4096 - TAIL_SIZE, 4096).unwrap();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].inode, 0);
        assert_eq!(es[0].rec_len, 4096 - 12);
        assert_eq!(es[0].used_len(), 0);

        let mut b = vec![0xAAu8; 4096];
        init_empty_block(&mut b, false);
        assert!(!has_tail(&b));
        let es = parse_block(&b, 4096, 4096).unwrap();
        assert_eq!(es[0].rec_len, 4096);
    }

    #[test]
    fn modify_entries() {
        let mut b = sample_block(false);
        set_inode(&mut b, 24, 0);
        set_rec_len(&mut b, 12, 1012, 1024);
        let es = parse_block(&b, 1024, 1024).unwrap();
        assert_eq!(es.len(), 2);
        assert_eq!(es[1].rec_len, 1012);
    }

    #[test]
    fn dx_root_info_roundtrip() {
        let mut b = vec![0u8; 4096];
        let info = DxRootInfo {
            hash_version: 1,
            info_length: 8,
            indirect_levels: 1,
            unused_flags: 0,
        };
        info.write(&mut b);
        assert_eq!(DxRootInfo::parse(&b), info);
    }

    #[test]
    fn dx_limits() {
        assert_eq!(dx_root_limit(4096, false), 508);
        assert_eq!(dx_root_limit(4096, true), 507);
        assert_eq!(dx_node_limit(4096, false), 511);
        assert_eq!(dx_node_limit(4096, true), 510);
        assert_eq!(dx_root_limit(1024, true), 123);
    }

    #[test]
    fn dx_entries_and_csum() {
        let mut b = vec![0u8; 1024];
        let off = DX_NODE_ENTRIES_OFF;
        set_dx_limit(&mut b, off, dx_node_limit(1024, true));
        set_dx_count(&mut b, off, 3);
        set_dx_entry(&mut b, off, 0, DxEntry { hash: 0, block: 1 });
        set_dx_entry(&mut b, off, 1, DxEntry { hash: 100, block: 2 });
        set_dx_entry(&mut b, off, 2, DxEntry { hash: 200, block: 3 });
        assert_eq!(dx_count(&b, off), 3);
        assert_eq!(dx_entry(&b, off, 0), DxEntry { hash: 0, block: 1 });
        assert_eq!(dx_entry(&b, off, 2), DxEntry { hash: 200, block: 3 });
        set_dx_csum(5, &mut b, off);
        assert!(verify_dx_csum(5, &b, off));
        assert!(!verify_dx_csum(6, &b, off));
        // bytes past `count` entries are not covered
        b[off + 3 * 8 + 1] = 0xFF;
        assert!(verify_dx_csum(5, &b, off));
        b[off + 8] ^= 1;
        assert!(!verify_dx_csum(5, &b, off));
    }
}
