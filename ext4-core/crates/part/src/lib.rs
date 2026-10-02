//! Partition tables of a disk: GPT (behind its protective MBR) and MBR with
//! extended partitions. A disk without a table is one whole volume.
//!
//! Damaged or implausible tables are not errors: they are skipped the way
//! the Linux kernel skips them, so a disk formatted as one file system is
//! never mistaken for a partitioned one. Only failed reads are errors.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading the partition table failed: {0}")]
    Read(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid(pub [u8; 16]);

impl Guid {
    /// Linux file system data (ext2/3/4, XFS, ...).
    pub const LINUX_DATA: Guid = Guid([
        0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
    ]);

    pub fn is_zero(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }
}

impl fmt::Display for Guid {
    /// The usual text form: the first three fields are little-endian.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u16::from_le_bytes([b[4], b[5]]),
            u16::from_le_bytes([b[6], b[7]]),
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15]
        )
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Gpt { type_guid: Guid, guid: Guid, name: String },
    Mbr { type_id: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Number as Linux counts it (`sda1` is 1): the GPT entry index plus
    /// one; for MBR 1 to 4 are the primary slots and logical partitions
    /// start at 5.
    pub number: u32,
    /// Offset on the disk in bytes.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Table {
    Gpt {
        /// Logical block size the table was written with (a USB enclosure
        /// may report another size than the one the disk was partitioned
        /// with).
        block_size: u32,
        /// The primary header was damaged; the backup at the end was used.
        from_backup: bool,
        partitions: Vec<Partition>,
    },
    Mbr {
        partitions: Vec<Partition>,
    },
    /// No partition table: the whole disk is one volume.
    None,
}

impl Table {
    pub fn partitions(&self) -> &[Partition] {
        match self {
            Table::Gpt { partitions, .. } | Table::Mbr { partitions } => partitions,
            Table::None => &[],
        }
    }
}

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const GPT_MIN_HEADER: usize = 92;
const GPT_MAX_ENTRIES_BYTES: usize = 1 << 20;
const MBR_SIGNATURE: [u8; 2] = [0x55, 0xAA];
const MBR_ENTRIES: usize = 446;
const PROTECTIVE: u8 = 0xEE;
const EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];
const MAX_LOGICAL: usize = 256;

/// Reads bytes at a byte offset of the disk.
pub type ReadAt<'a, E> = dyn FnMut(u64, &mut [u8]) -> std::result::Result<(), E> + 'a;

struct Disk<'a, 'r, E> {
    read_at: &'a mut ReadAt<'r, E>,
    len: u64,
}

impl<E: fmt::Display> Disk<'_, '_, E> {
    fn read(&mut self, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
        if offset.checked_add(len as u64).is_none_or(|end| end > self.len) {
            return Ok(None);
        }
        let mut b = vec![0u8; len];
        (self.read_at)(offset, &mut b).map_err(|e| Error::Read(e.to_string()))?;
        Ok(Some(b))
    }
}

/// Read the partition table. `read_at` reads bytes at a byte offset; reads
/// of 512 bytes happen even on disks with larger blocks, so it must handle
/// unaligned reads. `block_size` is the block size the disk reports and
/// `disk_len` its size in bytes.
pub fn read<E: fmt::Display>(read_at: &mut ReadAt<'_, E>, block_size: u32, disk_len: u64) -> Result<Table> {
    let mut disk = Disk { read_at, len: disk_len };
    let mut sizes = vec![block_size];
    for bs in [512, 4096] {
        if !sizes.contains(&bs) {
            sizes.push(bs);
        }
    }
    for &bs in &sizes {
        if let Some(t) = read_gpt(&mut disk, bs)? {
            return Ok(t);
        }
    }
    read_mbr(&mut disk, block_size)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn read_gpt<E: fmt::Display>(disk: &mut Disk<'_, '_, E>, bs: u32) -> Result<Option<Table>> {
    let blocks = disk.len / bs as u64;
    if blocks < 3 {
        return Ok(None);
    }
    for (lba, from_backup) in [(1, false), (blocks - 1, true)] {
        if let Some(partitions) = gpt_at(disk, bs, lba)? {
            return Ok(Some(Table::Gpt {
                block_size: bs,
                from_backup,
                partitions,
            }));
        }
    }
    Ok(None)
}

/// Partitions of the GPT whose header is at `lba`, if it is valid.
fn gpt_at<E: fmt::Display>(disk: &mut Disk<'_, '_, E>, bs: u32, lba: u64) -> Result<Option<Vec<Partition>>> {
    let bsu = bs as u64;
    let blocks = disk.len / bsu;
    let Some(h) = disk.read(lba * bsu, bs as usize)? else {
        return Ok(None);
    };
    if &h[0..8] != GPT_SIGNATURE {
        return Ok(None);
    }
    let hsize = le32(&h, 12) as usize;
    if !(GPT_MIN_HEADER..=bs as usize).contains(&hsize) {
        return Ok(None);
    }
    let mut hc = h[..hsize].to_vec();
    hc[16..20].fill(0);
    if crc32(&hc) != le32(&h, 16) || le64(&h, 24) != lba {
        return Ok(None);
    }
    let last_usable = le64(&h, 48);
    let entries_lba = le64(&h, 72);
    let count = le32(&h, 80) as usize;
    let esize = le32(&h, 84) as usize;
    if esize < 128 || esize % 8 != 0 || count.checked_mul(esize).is_none_or(|n| n > GPT_MAX_ENTRIES_BYTES) {
        return Ok(None);
    }
    if last_usable >= blocks {
        return Ok(None);
    }
    let bytes = count * esize;
    let Some(e) = disk.read(entries_lba * bsu, bytes.div_ceil(bs as usize) * bs as usize)? else {
        return Ok(None);
    };
    if crc32(&e[..bytes]) != le32(&h, 88) {
        return Ok(None);
    }
    let mut parts = Vec::new();
    for i in 0..count {
        let r = &e[i * esize..i * esize + 128];
        let type_guid = Guid(r[0..16].try_into().unwrap());
        if type_guid.is_zero() {
            continue;
        }
        let first = le64(r, 32);
        let last = le64(r, 40);
        if last < first || last >= blocks {
            continue;
        }
        let units: Vec<u16> = r[56..128]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        parts.push(Partition {
            number: i as u32 + 1,
            start: first * bsu,
            len: (last - first + 1) * bsu,
            kind: Kind::Gpt {
                type_guid,
                guid: Guid(r[16..32].try_into().unwrap()),
                name: String::from_utf16_lossy(&units),
            },
        });
    }
    Ok(Some(parts))
}

struct MbrEntry {
    boot: u8,
    type_id: u8,
    lba: u64,
    count: u64,
}

fn mbr_entries(s: &[u8]) -> [MbrEntry; 4] {
    std::array::from_fn(|i| {
        let e = &s[MBR_ENTRIES + i * 16..MBR_ENTRIES + (i + 1) * 16];
        MbrEntry {
            boot: e[0],
            type_id: e[4],
            lba: le32(e, 8) as u64,
            count: le32(e, 12) as u64,
        }
    })
}

fn read_mbr<E: fmt::Display>(disk: &mut Disk<'_, '_, E>, bs: u32) -> Result<Table> {
    let bsu = bs as u64;
    let blocks = disk.len / bsu;
    let Some(s) = disk.read(0, 512)? else {
        return Ok(Table::None);
    };
    if s[510..512] != MBR_SIGNATURE {
        return Ok(Table::None);
    }
    let entries = mbr_entries(&s);
    // a boot sector of a file system also ends in 55 AA: its boot code
    // where the entries would be fails these checks
    let plausible = entries.iter().all(|e| {
        (e.boot == 0 || e.boot == 0x80) && (e.type_id == 0 || (e.lba >= 1 && e.count > 0 && e.lba + e.count <= blocks))
    });
    if !plausible || entries.iter().all(|e| e.type_id == 0 || e.type_id == PROTECTIVE) {
        return Ok(Table::None);
    }
    let mut parts = Vec::new();
    let mut extended = None;
    for (i, e) in entries.iter().enumerate() {
        if e.type_id == 0 {
            continue;
        }
        if EXTENDED.contains(&e.type_id) {
            extended.get_or_insert((e.lba, e.count));
            continue;
        }
        parts.push(Partition {
            number: i as u32 + 1,
            start: e.lba * bsu,
            len: e.count * bsu,
            kind: Kind::Mbr { type_id: e.type_id },
        });
    }
    if let Some((ext_start, ext_len)) = extended {
        read_logical(disk, bs, ext_start, ext_start + ext_len, &mut parts)?;
    }
    Ok(Table::Mbr { partitions: parts })
}

/// Walk the chain of extended boot records inside the extended partition.
fn read_logical<E: fmt::Display>(
    disk: &mut Disk<'_, '_, E>,
    bs: u32,
    ext_start: u64,
    ext_end: u64,
    parts: &mut Vec<Partition>,
) -> Result<()> {
    let bsu = bs as u64;
    let mut ebr = ext_start;
    let mut number = 5;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MAX_LOGICAL {
        if !seen.insert(ebr) {
            break; // a loop in the chain
        }
        let Some(s) = disk.read(ebr * bsu, 512)? else {
            break;
        };
        if s[510..512] != MBR_SIGNATURE {
            break;
        }
        let [this, next, ..] = mbr_entries(&s);
        if this.type_id != 0 && this.count > 0 && ebr + this.lba + this.count <= ext_end {
            parts.push(Partition {
                number,
                start: (ebr + this.lba) * bsu,
                len: this.count * bsu,
                kind: Kind::Mbr { type_id: this.type_id },
            });
            number += 1;
        }
        if !EXTENDED.contains(&next.type_id) || next.lba == 0 {
            break;
        }
        ebr = ext_start + next.lba;
        if ebr >= ext_end {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
