//! jbd2 on-disk format (all fields big endian).

use crate::bytes::{be32, set_be32};
use crate::csum::crc32c;
use crate::error::{Error, Result};

pub const JBD2_MAGIC: u32 = 0xC03B_3998;
pub const JOURNAL_HEADER_SIZE: usize = 12;
pub const JSB_SIZE: usize = 1024;

pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
pub const JBD2_COMMIT_BLOCK: u32 = 2;
pub const JBD2_SUPERBLOCK_V1: u32 = 3;
pub const JBD2_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_REVOKE_BLOCK: u32 = 5;
pub const JBD2_FC_BLOCK: u32 = 6;

pub const JBD2_FEATURE_COMPAT_CHECKSUM: u32 = 0x1;
pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x1;
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x2;
pub const JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x8;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x10;
pub const JBD2_FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x20;

pub const JBD2_CRC32C_CHKSUM: u8 = 4;

pub const JBD2_FLAG_ESCAPE: u32 = 1;
pub const JBD2_FLAG_SAME_UUID: u32 = 2;
pub const JBD2_FLAG_DELETED: u32 = 4;
pub const JBD2_FLAG_LAST_TAG: u32 = 8;

#[derive(Clone)]
pub struct JournalSuperblock {
    pub raw: Box<[u8; JSB_SIZE]>,
}

macro_rules! be_fields {
    ($($get:ident, $set:ident @ $off:expr;)*) => {
        $(
            pub fn $get(&self) -> u32 {
                be32(&self.raw[..], $off)
            }
            pub fn $set(&mut self, v: u32) {
                set_be32(&mut self.raw[..], $off, v)
            }
        )*
    };
}

impl JournalSuperblock {
    be_fields! {
        magic, set_magic @ 0x0;
        blocktype, set_blocktype @ 0x4;
        block_size, set_block_size @ 0xC;
        max_len, set_max_len @ 0x10;
        first, set_first @ 0x14;
        sequence, set_sequence @ 0x18;
        start, set_start @ 0x1C;
        errno, set_errno @ 0x20;
        feature_compat, set_feature_compat @ 0x24;
        feature_incompat, set_feature_incompat @ 0x28;
        feature_ro_compat, set_feature_ro_compat @ 0x2C;
        nr_users, set_nr_users @ 0x40;
        max_transaction, set_max_transaction @ 0x48;
        num_fc_blocks, set_num_fc_blocks @ 0x54;
        checksum, set_checksum @ 0xFC;
    }

    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() < JSB_SIZE {
            return Err(Error::corrupt("journal superblock too short"));
        }
        let mut b = Box::new([0u8; JSB_SIZE]);
        b.copy_from_slice(&raw[..JSB_SIZE]);
        let sb = JournalSuperblock { raw: b };
        if sb.magic() != JBD2_MAGIC {
            return Err(Error::corrupt("bad journal superblock magic"));
        }
        match sb.blocktype() {
            JBD2_SUPERBLOCK_V1 | JBD2_SUPERBLOCK_V2 => {}
            t => return Err(Error::corrupt(format!("bad journal superblock type {t}"))),
        }
        if sb.blocktype() == JBD2_SUPERBLOCK_V1 {
            // v1 has no feature fields
            let mut s = sb;
            s.raw[0x24..0x30].fill(0);
            return Ok(s);
        }
        if sb.has_csum_v2v3() {
            if sb.checksum_type() != JBD2_CRC32C_CHKSUM {
                return Err(Error::corrupt("unknown journal checksum type"));
            }
            if sb.compute_checksum() != sb.checksum() {
                return Err(Error::Checksum("journal superblock".into()));
            }
        }
        Ok(sb)
    }

    pub fn checksum_type(&self) -> u8 {
        self.raw[0x50]
    }

    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0x30..0x40]);
        u
    }

    pub fn has_incompat(&self, f: u32) -> bool {
        self.feature_incompat() & f != 0
    }

    pub fn has_csum_v2v3(&self) -> bool {
        self.has_incompat(JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_CSUM_V3)
    }

    pub fn has_csum_v3(&self) -> bool {
        self.has_incompat(JBD2_FEATURE_INCOMPAT_CSUM_V3)
    }

    pub fn is_64bit(&self) -> bool {
        self.has_incompat(JBD2_FEATURE_INCOMPAT_64BIT)
    }

    pub fn csum_seed(&self) -> u32 {
        crc32c(!0, &self.raw[0x30..0x40])
    }

    pub fn compute_checksum(&self) -> u32 {
        let mut tmp = self.raw.clone();
        tmp[0xFC..0x100].fill(0);
        crc32c(!0, &tmp[..])
    }

    pub fn update_checksum(&mut self) {
        if self.has_csum_v2v3() {
            let c = self.compute_checksum();
            self.set_checksum(c);
        }
    }

    /// Size of one block tag in descriptor blocks.
    pub fn tag_bytes(&self) -> usize {
        if self.has_csum_v3() {
            return 16;
        }
        let mut sz = 12;
        if self.has_incompat(JBD2_FEATURE_INCOMPAT_CSUM_V2) {
            sz += 2;
        }
        if self.is_64bit() { sz } else { sz - 4 }
    }
}

pub fn write_header(b: &mut [u8], blocktype: u32, seq: u32) {
    set_be32(b, 0, JBD2_MAGIC);
    set_be32(b, 4, blocktype);
    set_be32(b, 8, seq);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub magic: u32,
    pub blocktype: u32,
    pub sequence: u32,
}

pub fn read_header(b: &[u8]) -> Header {
    Header {
        magic: be32(b, 0),
        blocktype: be32(b, 4),
        sequence: be32(b, 8),
    }
}

/// A parsed descriptor tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub blocknr: u64,
    pub flags: u32,
    pub checksum: u32,
}

pub fn write_tag(b: &mut [u8], sb: &JournalSuperblock, blocknr: u64, flags: u32, csum: u32, v3: bool) {
    set_be32(b, 0, blocknr as u32);
    if v3 {
        set_be32(b, 4, flags);
        set_be32(b, 8, (blocknr >> 32) as u32);
        set_be32(b, 12, csum);
    } else {
        b[4..6].copy_from_slice(&(csum as u16).to_be_bytes());
        b[6..8].copy_from_slice(&(flags as u16).to_be_bytes());
        if sb.is_64bit() {
            set_be32(b, 8, (blocknr >> 32) as u32);
        }
    }
}

pub fn read_tag(b: &[u8], sb: &JournalSuperblock) -> Tag {
    if sb.has_csum_v3() {
        let hi = if sb.is_64bit() { be32(b, 8) as u64 } else { 0 };
        Tag {
            blocknr: be32(b, 0) as u64 | (hi << 32),
            flags: be32(b, 4),
            checksum: be32(b, 12),
        }
    } else {
        let hi = if sb.is_64bit() { be32(b, 8) as u64 } else { 0 };
        Tag {
            blocknr: be32(b, 0) as u64 | (hi << 32),
            flags: u16::from_be_bytes([b[6], b[7]]) as u32,
            checksum: u16::from_be_bytes([b[4], b[5]]) as u32,
        }
    }
}

/// Parse all tags of a descriptor block.
pub fn parse_descriptor(b: &[u8], sb: &JournalSuperblock) -> Vec<Tag> {
    let tb = sb.tag_bytes();
    let end = b.len() - if sb.has_csum_v2v3() { 4 } else { 0 };
    let mut off = JOURNAL_HEADER_SIZE;
    let mut out = Vec::new();
    while off + tb <= end {
        let t = read_tag(&b[off..off + tb], sb);
        off += tb;
        if t.flags & JBD2_FLAG_SAME_UUID == 0 {
            off += 16;
        }
        out.push(t);
        if t.flags & JBD2_FLAG_LAST_TAG != 0 {
            break;
        }
    }
    out
}

pub fn tag_checksum(seed: u32, seq: u32, data: &[u8]) -> u32 {
    let c = crc32c(seed, &seq.to_be_bytes());
    crc32c(c, data)
}

pub fn tag_checksum_matches(sb: &JournalSuperblock, seq: u32, data: &[u8], stored: u32) -> bool {
    if !sb.has_csum_v2v3() {
        return true;
    }
    let c = tag_checksum(sb.csum_seed(), seq, data);
    if sb.has_csum_v3() {
        c == stored
    } else {
        (c & 0xFFFF) == stored
    }
}

fn block_tail_csum(seed: u32, b: &[u8]) -> u32 {
    let n = b.len();
    let mut c = crc32c(seed, &b[..n - 4]);
    c = crc32c(c, &[0, 0, 0, 0]);
    c
}

pub fn set_descriptor_tail(seed: u32, b: &mut [u8]) {
    let n = b.len();
    let c = block_tail_csum(seed, b);
    set_be32(b, n - 4, c);
}

pub fn verify_descriptor_tail(seed: u32, b: &[u8]) -> bool {
    be32(b, b.len() - 4) == block_tail_csum(seed, b)
}

/// Commit checksum: crc over the whole block with `h_chksum[0]` zeroed.
fn commit_csum(seed: u32, b: &[u8]) -> u32 {
    let mut c = crc32c(seed, &b[..0x10]);
    c = crc32c(c, &[0, 0, 0, 0]);
    crc32c(c, &b[0x14..])
}

pub fn set_commit_checksum(seed: u32, b: &mut [u8]) {
    b[0xC] = 0; // h_chksum_type
    b[0xD] = 0; // h_chksum_size
    let c = commit_csum(seed, b);
    set_be32(b, 0x10, c);
}

pub fn verify_commit_checksum(seed: u32, b: &[u8]) -> bool {
    be32(b, 0x10) == commit_csum(seed, b)
}

/// Parse a revoke block into block numbers.
pub fn parse_revoke(b: &[u8], sb: &JournalSuperblock) -> Result<Vec<u64>> {
    let count = be32(b, 12) as usize;
    let rec = if sb.is_64bit() { 8 } else { 4 };
    let end = b.len() - if sb.has_csum_v2v3() { 4 } else { 0 };
    if count < 16 || count > end {
        return Err(Error::corrupt("bad revoke block count"));
    }
    let mut out = Vec::new();
    let mut off = 16;
    while off + rec <= count {
        let v = if rec == 8 {
            crate::bytes::be64(b, off)
        } else {
            be32(b, off) as u64
        };
        out.push(v);
        off += rec;
    }
    Ok(out)
}
