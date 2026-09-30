//! Extended attribute layout (in-inode area and external block).

use crate::bytes::{le16, le32, set_le16, set_le32};
use crate::csum::crc32c;
use crate::error::{Error, Result};

pub const XATTR_MAGIC: u32 = 0xEA02_0000;
pub const BLOCK_HEADER_SIZE: usize = 32;
pub const ENTRY_HEADER_SIZE: usize = 16;
pub const XATTR_PAD: usize = 4;

pub const INDEX_USER: u8 = 1;
pub const INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const INDEX_TRUSTED: u8 = 4;
pub const INDEX_LUSTRE: u8 = 5;
pub const INDEX_SECURITY: u8 = 6;
pub const INDEX_SYSTEM: u8 = 7;
pub const INDEX_RICHACL: u8 = 8;
pub const INDEX_ENCRYPTION: u8 = 9;
pub const INDEX_HURD: u8 = 10;

/// Known name prefixes, longest first where they overlap.
const PREFIXES: &[(u8, &str)] = &[
    (INDEX_POSIX_ACL_ACCESS, "system.posix_acl_access"),
    (INDEX_POSIX_ACL_DEFAULT, "system.posix_acl_default"),
    (INDEX_RICHACL, "system.richacl"),
    (INDEX_USER, "user."),
    (INDEX_TRUSTED, "trusted."),
    (INDEX_SECURITY, "security."),
    (INDEX_SYSTEM, "system."),
];

pub fn pad(n: usize) -> usize {
    (n + XATTR_PAD - 1) & !(XATTR_PAD - 1)
}

/// Split a full attribute name into (index, suffix).
pub fn split_name(full: &[u8]) -> (u8, &[u8]) {
    for &(idx, p) in PREFIXES {
        let pb = p.as_bytes();
        if full.starts_with(pb) {
            // the ACL names are exact matches with an empty suffix
            if (idx == INDEX_POSIX_ACL_ACCESS || idx == INDEX_POSIX_ACL_DEFAULT || idx == INDEX_RICHACL)
                && full.len() != pb.len()
            {
                continue;
            }
            return (idx, &full[pb.len()..]);
        }
    }
    (0, full)
}

/// Rebuild the full attribute name from (index, suffix).
pub fn full_name(index: u8, suffix: &[u8]) -> Vec<u8> {
    let prefix: &str = match index {
        INDEX_USER => "user.",
        INDEX_POSIX_ACL_ACCESS => "system.posix_acl_access",
        INDEX_POSIX_ACL_DEFAULT => "system.posix_acl_default",
        INDEX_TRUSTED => "trusted.",
        INDEX_LUSTRE => "lustre.",
        INDEX_SECURITY => "security.",
        INDEX_SYSTEM => "system.",
        INDEX_RICHACL => "system.richacl",
        INDEX_ENCRYPTION => "encryption.",
        INDEX_HURD => "gnu.",
        _ => "",
    };
    let mut v = prefix.as_bytes().to_vec();
    v.extend_from_slice(suffix);
    v
}

/// A decoded entry with its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XattrEntry {
    pub index: u8,
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    /// Non-zero if the value lives in a separate EA inode.
    pub value_inum: u32,
    pub hash: u32,
}

impl XattrEntry {
    pub fn full_name(&self) -> Vec<u8> {
        full_name(self.index, &self.name)
    }

    pub fn entry_size(&self) -> usize {
        pad(ENTRY_HEADER_SIZE + self.name.len())
    }

    pub fn value_size(&self) -> usize {
        if self.value_inum != 0 { 0 } else { pad(self.value.len()) }
    }
}

/// `ext4_xattr_hash_entry` (unsigned char variant).
pub fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &c in name {
        hash = (hash << 5) ^ (hash >> 27) ^ c as u32;
    }
    value_hash(hash, value)
}

fn value_hash(mut hash: u32, value: &[u8]) -> u32 {
    let mut padded = value.to_vec();
    padded.resize(pad(value.len()), 0);
    for w in padded.as_chunks::<4>().0 {
        hash = (hash << 16) ^ (hash >> 16) ^ u32::from_le_bytes(*w);
    }
    hash
}

/// Legacy signed-char variant accepted by e2fsck for old images.
pub fn entry_hash_signed(name: &[u8], value: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &c in name {
        hash = (hash << 5) ^ (hash >> 27) ^ (c as i8 as i32 as u32);
    }
    value_hash(hash, value)
}

/// Block hash from entry hashes (`ext4_xattr_rehash`).
pub fn block_hash(entries: &[XattrEntry]) -> u32 {
    let mut hash: u32 = 0;
    for e in entries {
        if e.hash == 0 {
            return 0;
        }
        hash = (hash << 16) ^ (hash >> 16) ^ e.hash;
    }
    hash
}

/// Parse entries in a region. `entries_off` is where the entry table
/// starts; value offsets are relative to `value_base`.
fn parse_entries(buf: &[u8], entries_off: usize, value_base: usize) -> Result<Vec<XattrEntry>> {
    let mut out = Vec::new();
    let mut off = entries_off;
    loop {
        if off + 4 > buf.len() {
            return Err(Error::corrupt("xattr entry table not terminated"));
        }
        if le32(buf, off) == 0 {
            break;
        }
        if off + ENTRY_HEADER_SIZE > buf.len() {
            return Err(Error::corrupt("xattr entry overruns region"));
        }
        let name_len = buf[off] as usize;
        let index = buf[off + 1];
        let value_offs = le16(buf, off + 2) as usize;
        let value_inum = le32(buf, off + 4);
        let value_size = le32(buf, off + 8) as usize;
        let hash = le32(buf, off + 12);
        let name_end = off + ENTRY_HEADER_SIZE + name_len;
        if name_end > buf.len() {
            return Err(Error::corrupt("xattr name overruns region"));
        }
        let name = buf[off + ENTRY_HEADER_SIZE..name_end].to_vec();
        let value = if value_inum != 0 {
            Vec::new()
        } else {
            let vs = value_base + value_offs;
            if vs + value_size > buf.len() || (value_size > 0 && vs < name_end) {
                return Err(Error::corrupt("xattr value out of bounds"));
            }
            buf[vs..vs + value_size].to_vec()
        };
        out.push(XattrEntry {
            index,
            name,
            value,
            value_inum,
            hash,
        });
        let _ = value_size;
        off = pad(name_end);
    }
    Ok(out)
}

/// Parse the in-inode xattr area (starting with the 4-byte magic).
/// Returns an empty list if the magic is absent.
pub fn parse_ibody(area: &[u8]) -> Result<Vec<XattrEntry>> {
    if area.len() < 4 || le32(area, 0) != XATTR_MAGIC {
        return Ok(Vec::new());
    }
    let region = &area[4..];
    parse_entries(region, 0, 0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XattrBlockHeader {
    pub refcount: u32,
    pub blocks: u32,
    pub hash: u32,
    pub checksum: u32,
}

pub fn parse_block(block: &[u8]) -> Result<(XattrBlockHeader, Vec<XattrEntry>)> {
    if le32(block, 0) != XATTR_MAGIC {
        return Err(Error::corrupt("bad xattr block magic"));
    }
    let h = XattrBlockHeader {
        refcount: le32(block, 4),
        blocks: le32(block, 8),
        hash: le32(block, 12),
        checksum: le32(block, 16),
    };
    if h.blocks != 1 {
        return Err(Error::corrupt("xattr block h_blocks != 1"));
    }
    let entries = parse_entries(block, BLOCK_HEADER_SIZE, 0)?;
    Ok((h, entries))
}

/// Space needed to store entries (table + terminator + values).
pub fn space_needed(entries: &[XattrEntry]) -> usize {
    entries.iter().map(|e| e.entry_size() + e.value_size()).sum::<usize>() + 4
}

/// Serialize entries into a region laid out as: entry table from
/// `entries_off`, values packed downward from the end. Value offsets are
/// stored relative to `value_base`.
fn write_entries(buf: &mut [u8], entries_off: usize, value_base: usize, entries: &[XattrEntry]) -> Result<()> {
    let mut off = entries_off;
    let mut vend = buf.len();
    for e in entries {
        let esz = e.entry_size();
        let vsz = e.value_size();
        if vsz > vend || off + esz + 4 > vend - vsz {
            return Err(Error::NoSpace);
        }
        let voff = if vsz > 0 { vend - vsz } else { 0 };
        buf[off] = e.name.len() as u8;
        buf[off + 1] = e.index;
        let rel = if vsz > 0 { voff - value_base } else { 0 };
        set_le16(buf, off + 2, rel as u16);
        set_le32(buf, off + 4, e.value_inum);
        set_le32(buf, off + 8, e.value.len() as u32);
        set_le32(buf, off + 12, e.hash);
        buf[off + ENTRY_HEADER_SIZE..off + ENTRY_HEADER_SIZE + e.name.len()].copy_from_slice(&e.name);
        for b in &mut buf[off + ENTRY_HEADER_SIZE + e.name.len()..off + esz] {
            *b = 0;
        }
        if vsz > 0 {
            buf[voff..voff + e.value.len()].copy_from_slice(&e.value);
            for b in &mut buf[voff + e.value.len()..voff + vsz] {
                *b = 0;
            }
            vend = voff;
        }
        off += esz;
    }
    // terminator + zero the gap
    for b in &mut buf[off..vend] {
        *b = 0;
    }
    Ok(())
}

/// Write in-inode xattrs into `area` (which includes the magic). An empty
/// list clears the area.
pub fn write_ibody(area: &mut [u8], entries: &[XattrEntry]) -> Result<()> {
    if entries.is_empty() {
        area.fill(0);
        return Ok(());
    }
    if area.len() < 8 {
        return Err(Error::NoSpace);
    }
    set_le32(area, 0, XATTR_MAGIC);
    write_entries(&mut area[4..], 0, 0, entries)
}

/// Build a complete xattr block (header, sorted entries, values, checksum).
pub fn build_block(block: &mut [u8], refcount: u32, entries: &[XattrEntry], csum: Option<(u32, u64)>) -> Result<()> {
    block.fill(0);
    set_le32(block, 0, XATTR_MAGIC);
    set_le32(block, 4, refcount);
    set_le32(block, 8, 1);
    write_entries(block, BLOCK_HEADER_SIZE, 0, entries)?;
    set_le32(block, 12, block_hash(entries));
    if let Some((seed, blocknr)) = csum {
        set_block_csum(block, seed, blocknr);
    }
    Ok(())
}

pub fn block_csum(block: &[u8], seed: u32, blocknr: u64) -> u32 {
    let mut c = crc32c(seed, &blocknr.to_le_bytes());
    c = crc32c(c, &block[..16]);
    c = crc32c(c, &[0, 0, 0, 0]);
    crc32c(c, &block[20..])
}

pub fn verify_block_csum(block: &[u8], seed: u32, blocknr: u64) -> bool {
    le32(block, 16) == block_csum(block, seed, blocknr)
}

pub fn set_block_csum(block: &mut [u8], seed: u32, blocknr: u64) {
    let c = block_csum(block, seed, blocknr);
    set_le32(block, 16, c);
}

pub fn set_block_refcount(block: &mut [u8], refcount: u32) {
    set_le32(block, 4, refcount);
}

/// Sort order used by Linux for block entries.
pub fn sort_entries(entries: &mut [XattrEntry]) {
    entries.sort_by(|a, b| (a.index, a.name.len(), &a.name).cmp(&(b.index, b.name.len(), &b.name)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u8, name: &str, value: &[u8]) -> XattrEntry {
        XattrEntry {
            index,
            name: name.as_bytes().to_vec(),
            value: value.to_vec(),
            value_inum: 0,
            hash: entry_hash(name.as_bytes(), value),
        }
    }

    #[test]
    fn name_split_and_join() {
        assert_eq!(split_name(b"user.foo"), (INDEX_USER, &b"foo"[..]));
        assert_eq!(split_name(b"trusted.x"), (INDEX_TRUSTED, &b"x"[..]));
        assert_eq!(split_name(b"security.selinux"), (INDEX_SECURITY, &b"selinux"[..]));
        assert_eq!(split_name(b"system.data"), (INDEX_SYSTEM, &b"data"[..]));
        assert_eq!(
            split_name(b"system.posix_acl_access"),
            (INDEX_POSIX_ACL_ACCESS, &b""[..])
        );
        assert_eq!(
            split_name(b"system.posix_acl_default"),
            (INDEX_POSIX_ACL_DEFAULT, &b""[..])
        );
        assert_eq!(
            split_name(b"system.posix_acl_accessX"),
            (INDEX_SYSTEM, &b"posix_acl_accessX"[..])
        );
        assert_eq!(split_name(b"weird"), (0, &b"weird"[..]));
        for n in [
            "user.a",
            "trusted.b",
            "security.c",
            "system.d",
            "system.posix_acl_access",
        ] {
            let (i, s) = split_name(n.as_bytes());
            assert_eq!(full_name(i, s), n.as_bytes());
        }
        assert_eq!(full_name(INDEX_LUSTRE, b"x"), b"lustre.x");
    }

    #[test]
    fn padding() {
        assert_eq!(pad(0), 0);
        assert_eq!(pad(1), 4);
        assert_eq!(pad(4), 4);
        assert_eq!(pad(17), 20);
    }

    #[test]
    fn hash_known_values() {
        // name only: "a" → 0x61
        assert_eq!(entry_hash(b"a", b""), 0x61);
        // ((0x61 << 5) ^ 0x62)
        assert_eq!(entry_hash(b"ab", b""), (0x61 << 5) ^ 0x62);
        // value word folds in with 16 bit rotation
        let h = entry_hash(b"a", b"\x01\x00\x00\x00");
        assert_eq!(h, (0x61u32 << 16) ^ (0x61 >> 16) ^ 1);
        // signed variant differs only for high-bit chars
        assert_eq!(entry_hash_signed(b"ab", b"xyz"), entry_hash(b"ab", b"xyz"));
        assert_ne!(entry_hash_signed(b"\xff", b""), entry_hash(b"\xff", b""));
    }

    #[test]
    fn block_hash_zero_if_any_entry_unhashed() {
        let mut es = vec![entry(1, "a", b"1"), entry(1, "b", b"2")];
        assert_ne!(block_hash(&es), 0);
        es[1].hash = 0;
        assert_eq!(block_hash(&es), 0);
    }

    #[test]
    fn ibody_roundtrip() {
        let mut area = vec![0u8; 96];
        let es = vec![
            entry(INDEX_USER, "foo", b"bar"),
            entry(INDEX_SECURITY, "selinux", b"ctx\0"),
        ];
        write_ibody(&mut area, &es).unwrap();
        let back = parse_ibody(&area).unwrap();
        assert_eq!(back, es);
        write_ibody(&mut area, &[]).unwrap();
        assert!(area.iter().all(|&b| b == 0));
        assert!(parse_ibody(&area).unwrap().is_empty());
    }

    #[test]
    fn ibody_no_space() {
        let mut area = vec![0u8; 32];
        let es = vec![entry(INDEX_USER, "foo", &[1u8; 64])];
        assert!(matches!(write_ibody(&mut area, &es), Err(Error::NoSpace)));
    }

    #[test]
    fn block_roundtrip_with_csum() {
        let mut blk = vec![0u8; 1024];
        let mut es = vec![
            entry(INDEX_USER, "zeta", b"last"),
            entry(INDEX_USER, "a", b"first"),
            entry(INDEX_TRUSTED, "t", &[9u8; 100]),
        ];
        sort_entries(&mut es);
        assert_eq!(es[0].name, b"a");
        build_block(&mut blk, 1, &es, Some((77, 1234))).unwrap();
        let (h, back) = parse_block(&blk).unwrap();
        assert_eq!(h.refcount, 1);
        assert_eq!(h.blocks, 1);
        assert_eq!(h.hash, block_hash(&es));
        assert_eq!(back, es);
        assert!(verify_block_csum(&blk, 77, 1234));
        assert!(!verify_block_csum(&blk, 77, 1235));
        set_block_refcount(&mut blk, 2);
        assert!(!verify_block_csum(&blk, 77, 1234));
        set_block_csum(&mut blk, 77, 1234);
        assert!(verify_block_csum(&blk, 77, 1234));
    }

    #[test]
    fn block_space_accounting() {
        let es = vec![entry(INDEX_USER, "abc", b"12345")];
        // entry: pad(16+3)=20, value pad(5)=8, terminator 4
        assert_eq!(space_needed(&es), 32);
        let mut blk = vec![0u8; 1024];
        let big = vec![entry(INDEX_USER, "x", &vec![0u8; 1000])];
        assert!(build_block(&mut blk, 1, &big, None).is_err());
    }

    #[test]
    fn corrupt_block_rejected() {
        let mut blk = vec![0u8; 1024];
        assert!(parse_block(&blk).is_err());
        build_block(&mut blk, 1, &[entry(1, "a", b"b")], None).unwrap();
        set_le32(&mut blk, 8, 2);
        assert!(parse_block(&blk).is_err());
        build_block(&mut blk, 1, &[entry(1, "a", b"b")], None).unwrap();
        // value offset out of range
        set_le16(&mut blk, BLOCK_HEADER_SIZE + 2, 2000);
        assert!(parse_block(&blk).is_err());
    }

    #[test]
    fn unterminated_table_rejected() {
        let mut area = vec![0xFFu8; 24];
        set_le32(&mut area, 0, XATTR_MAGIC);
        area[4] = 1; // name_len 1
        assert!(parse_ibody(&area).is_err());
    }

    #[test]
    fn empty_value_entry() {
        let mut area = vec![0u8; 64];
        let es = vec![entry(INDEX_USER, "empty", b"")];
        write_ibody(&mut area, &es).unwrap();
        assert_eq!(parse_ibody(&area).unwrap(), es);
    }

    #[test]
    fn full_name_of_entry() {
        let e = entry(INDEX_USER, "com.apple.FinderInfo", b"x");
        assert_eq!(e.full_name(), b"user.com.apple.FinderInfo");
        assert_eq!(e.entry_size(), pad(16 + 20));
        assert_eq!(e.value_size(), 4);
    }
}
