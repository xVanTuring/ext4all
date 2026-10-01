//! Creating a new file system, like `mke2fs -t ext4`.
//!
//! The layout follows mke2fs's defaults for ext4: 64-bit group
//! descriptors, flex_bg with 16 groups (the bitmaps and inode tables of a
//! flex group are packed at its start), sparse superblock backups,
//! metadata checksums, extents, and an internal journal in the middle of
//! the device. No resize inode is created. Like mke2fs with
//! `lazy_itable_init`, only group 0's inode table is zeroed: the other
//! groups are marked INODE_UNINIT with every inode unused, which the
//! kernel, e2fsck and this crate honour (inodes are handed out from the
//! start of a group's table and slots past `bg_itable_unused` are never
//! read).

use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::journal::format::{
    JBD2_CRC32C_CHKSUM, JBD2_FEATURE_INCOMPAT_64BIT, JBD2_FEATURE_INCOMPAT_CSUM_V3, JBD2_MAGIC, JBD2_SUPERBLOCK_V2,
    JSB_SIZE, JournalSuperblock,
};
use crate::ondisk::dirent as de;
use crate::ondisk::extent::{self as ext, Extent, ExtentHeader, ExtentIndex, MAX_INIT_LEN};
use crate::ondisk::group::{BG_INODE_UNINIT, BG_INODE_ZEROED, GroupDesc, bitmap_csum};
use crate::ondisk::inode::{Inode, JOURNAL_INO, ROOT_INO, Timestamp, flags, mode};
use crate::ondisk::superblock::{
    CHECKSUM_TYPE_CRC32C, EXT4_MAGIC, FLAGS_SIGNED_HASH, STATE_VALID, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE, Superblock,
    compat, hash_version, incompat, ro_compat,
};

const INODE_SIZE: u64 = 256;
const DESC_SIZE: u64 = 64;
const LOG_GROUPS_PER_FLEX: u32 = 4;
const FIRST_INO: u32 = 11;
const LOST_FOUND_INO: u32 = 11;
const EXTRA_ISIZE: u16 = 32;
/// `s_default_mount_opts`: user_xattr and acl, as mke2fs sets them.
const DEFAULT_MOUNT_OPTS: u32 = 0x0004 | 0x0008;
/// Dirent file type of a directory.
const FT_DIR: u8 = 2;
const MIB: u64 = 1 << 20;

/// What to create. `Default` gives mke2fs's choices for the device size.
#[derive(Clone, Debug)]
pub struct FormatOptions {
    /// Volume label (at most 16 bytes).
    pub label: String,
    /// 1024, 2048 or 4096; `None` picks 1024 below 512 MiB, else 4096.
    pub block_size: Option<u32>,
    /// Bytes of space per inode; `None` picks by size like mke2fs.
    pub inode_ratio: Option<u64>,
    /// Exact number of inodes (rounded up to fill inode table blocks).
    pub inode_count: Option<u64>,
    /// Blocks reserved for root, in percent (mke2fs `-m`, default 5).
    pub reserved_percent: f64,
    /// Create a journal (skipped on file systems below 2048 blocks).
    pub journal: bool,
    /// Journal size in MiB (mke2fs `-J size=`); `None` picks by size
    /// like mke2fs.
    pub journal_mib: Option<u64>,
    /// File system UUID; `None` generates a random one.
    pub uuid: Option<[u8; 16]>,
    /// Owner of the root directory.
    pub root_owner: (u32, u32),
    /// Creation time in seconds since the epoch; `None` for now.
    pub time: Option<i64>,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            label: String::new(),
            block_size: None,
            inode_ratio: None,
            inode_count: None,
            reserved_percent: 5.0,
            journal: true,
            journal_mib: None,
            uuid: None,
            root_owner: (0, 0),
            time: None,
        }
    }
}

/// What was created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatSummary {
    pub block_size: u32,
    pub blocks: u64,
    pub inodes: u64,
    pub groups: u32,
    pub journal_blocks: u64,
    pub uuid: [u8; 16],
}

fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || s.len() != 36 {
        return Err(Error::invalid(format!("bad UUID {s}")));
    }
    let mut u = [0u8; 16];
    for (i, b) in u.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| Error::invalid(format!("bad UUID {s}")))?;
    }
    Ok(u)
}

fn parse_num<T: std::str::FromStr>(opt: &str, v: &str) -> Result<T> {
    v.parse()
        .map_err(|_| Error::invalid(format!("invalid value '{v}' for {opt}")))
}

/// Parse mke2fs-style options: `-L label`, `-b block-size`,
/// `-i bytes-per-inode`, `-N inodes`, `-m reserved-percent`,
/// `-U uuid|random`, `-J size=MiB`, `-O ^has_journal`,
/// `-E root_owner[=uid:gid]`. `-F`, `-q` and the `-E` options `discard`,
/// `nodiscard`, `lazy_itable_init` and `lazy_journal_init` are accepted and
/// have no effect. Values may follow the option or be attached to it.
/// A bare `root_owner` uses `caller` (the requesting user's uid and gid).
pub fn parse_args(args: &[String], caller: (u32, u32)) -> Result<FormatOptions> {
    let mut o = FormatOptions::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let Some(flag) = a.strip_prefix('-').and_then(|f| f.chars().next()) else {
            return Err(Error::invalid(format!("unexpected argument '{a}'")));
        };
        if matches!(flag, 'F' | 'q') && a.len() == 2 {
            i += 1;
            continue;
        }
        let value = if a.len() > 2 {
            a[2..].to_string()
        } else {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| Error::invalid(format!("-{flag} needs a value")))?
        };
        let opt = format!("-{flag}");
        match flag {
            'L' => o.label = value,
            'b' => o.block_size = Some(parse_num(&opt, &value)?),
            'i' => o.inode_ratio = Some(parse_num(&opt, &value)?),
            'N' => o.inode_count = Some(parse_num(&opt, &value)?),
            'm' => o.reserved_percent = parse_num(&opt, &value)?,
            'U' => {
                o.uuid = match value.as_str() {
                    "random" => None,
                    _ => Some(parse_uuid(&value)?),
                }
            }
            'J' => {
                for part in value.split(',') {
                    match part.split_once('=') {
                        Some(("size", mib)) => o.journal_mib = Some(parse_num(&opt, mib)?),
                        _ => return Err(Error::invalid(format!("unsupported journal option '{part}'"))),
                    }
                }
            }
            'O' => {
                for f in value.split(',') {
                    match f {
                        "^has_journal" => o.journal = false,
                        "has_journal" => o.journal = true,
                        _ => return Err(Error::invalid(format!("unsupported feature change '{f}'"))),
                    }
                }
            }
            'E' => {
                for part in value.split(',') {
                    let (k, v) = part.split_once('=').map_or((part, None), |(k, v)| (k, Some(v)));
                    match (k, v) {
                        ("root_owner", None) => o.root_owner = caller,
                        ("root_owner", Some(v)) => {
                            let (u, g) = v
                                .split_once(':')
                                .ok_or_else(|| Error::invalid("root_owner needs uid:gid"))?;
                            o.root_owner = (parse_num("root_owner", u)?, parse_num("root_owner", g)?);
                        }
                        ("discard" | "nodiscard" | "lazy_itable_init" | "lazy_journal_init", _) => {}
                        _ => return Err(Error::invalid(format!("unsupported extended option '{part}'"))),
                    }
                }
            }
            _ => return Err(Error::invalid(format!("unsupported option '{a}'"))),
        }
        i += 1;
    }
    Ok(o)
}

/// mke2fs's default journal size (`ext2fs_default_journal_size`), in
/// blocks; `None` for file systems too small to have one.
pub fn default_journal_blocks(blocks: u64) -> Option<u64> {
    Some(match blocks {
        0..2048 => return None,
        2048..32768 => 1024,
        32768..262144 => 4096,
        262144..524288 => 8192,
        524288..4194304 => 16384,
        4194304..8388608 => 32768,
        8388608..16777216 => 65536,
        16777216..33554432 => 131072,
        _ => 262144,
    })
}

/// mke2fs's bytes per inode for the size class of a device
/// (floppy, small, default, big, huge in `mke2fs.conf`).
fn default_inode_ratio(bytes: u64) -> u64 {
    match bytes {
        b if b < 3 * MIB => 8192,
        b if b < 512 * MIB => 4096,
        b if b < (4 << 40) => 16384,
        b if b < (16 << 40) => 32768,
        _ => 65536,
    }
}

/// Whether group `g` holds a superblock backup with `sparse_super`.
fn has_super(g: u32) -> bool {
    fn power_of(mut g: u32, base: u32) -> bool {
        while g > 1 && g % base == 0 {
            g /= base;
        }
        g == 1
    }
    g <= 1 || (g & 1 == 1 && (power_of(g, 3) || power_of(g, 5) || power_of(g, 7)))
}

/// Block positions of the new file system.
#[derive(Debug)]
struct Layout {
    bs: u64,
    first: u64,
    blocks: u64,
    bpg: u64,
    groups: u32,
    ipg: u32,
    itb: u64,
    desc_blocks: u64,
    /// (block bitmap, inode bitmap, inode table) per group
    meta: Vec<(u64, u64, u64)>,
    root_block: u64,
    lost_found: (u64, u64),
    /// journal extents (logical block, start, length) and the extent tree
    /// leaf when they do not fit in the inode
    journal: Vec<(u32, u64, u32)>,
    journal_leaf: Option<u64>,
    journal_blocks: u64,
    /// every used range `[start, end)`, sorted and non-overlapping
    used: Vec<(u64, u64)>,
}

impl Layout {
    fn group_start(&self, g: u32) -> u64 {
        self.first + g as u64 * self.bpg
    }

    fn blocks_in_group(&self, g: u32) -> u64 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    fn backup(&self, g: u32) -> Option<(u64, u64)> {
        has_super(g).then(|| {
            let s = self.group_start(g);
            (s, s + 1 + self.desc_blocks)
        })
    }
}

fn too_small(what: &str) -> Error {
    Error::invalid(format!("device too small for an ext4 file system ({what})"))
}

/// Geometry: block size, groups, inodes per group. Drops a last group too
/// small to be useful, as mke2fs does.
fn geometry(bytes: u64, o: &FormatOptions) -> Result<(u64, u64, u64, u32, u32, u64, u64)> {
    let bs = o.block_size.unwrap_or(if bytes < 512 * MIB { 1024 } else { 4096 }) as u64;
    if ![1024, 2048, 4096].contains(&bs) {
        return Err(Error::invalid(format!("unsupported block size {bs}")));
    }
    let mut blocks = (bytes / bs).min(1 << 48);
    let first = (bs == 1024) as u64;
    let bpg = 8 * bs;
    let ratio = o.inode_ratio.unwrap_or_else(|| default_inode_ratio(bytes));
    if !(1024..=(64 * MIB)).contains(&ratio) {
        return Err(Error::invalid(format!("unsupported bytes per inode {ratio}")));
    }
    let per_block = bs / INODE_SIZE;
    loop {
        if blocks < 64 + first {
            return Err(too_small("fewer than 64 blocks"));
        }
        let groups64 = (blocks - first).div_ceil(bpg);
        if groups64 > u32::MAX as u64 {
            return Err(Error::invalid("too many block groups"));
        }
        let groups = groups64 as u32;
        let desc_blocks = (groups as u64 * DESC_SIZE).div_ceil(bs);
        let want = o.inode_count.unwrap_or(blocks * bs / ratio).max(FIRST_INO as u64 + 1);
        let unit = per_block.max(8);
        let mut ipg = want.div_ceil(groups as u64).div_ceil(unit) * unit;
        ipg = ipg.clamp(16, bpg);
        let max_ipg = (u32::MAX as u64 / groups as u64) / unit * unit;
        ipg = ipg.min(max_ipg);
        if ipg < 16 {
            return Err(Error::invalid("too many block groups for the inode count"));
        }
        let itb = ipg * INODE_SIZE / bs;
        let rem = (blocks - first) % bpg;
        let overhead = 3 + itb + if has_super(groups - 1) { 1 + desc_blocks } else { 0 };
        if rem != 0 && rem < overhead + 50 {
            if groups == 1 {
                return Err(too_small("no room for the group's metadata"));
            }
            blocks -= rem;
            continue;
        }
        return Ok((bs, first, blocks, groups, ipg as u32, itb, desc_blocks));
    }
}

/// Cursor allocator over the device, skipping superblock backups.
struct Alloc<'a> {
    backups: &'a [(u64, u64)],
    blocks: u64,
    cursor: u64,
    used: Vec<(u64, u64)>,
}

impl Alloc<'_> {
    fn take(&mut self, n: u64) -> Result<u64> {
        loop {
            let i = self.backups.partition_point(|r| r.1 <= self.cursor);
            match self.backups.get(i) {
                Some(&(s, e)) if s < self.cursor + n => self.cursor = e,
                _ => break,
            }
        }
        if self.cursor + n > self.blocks {
            return Err(too_small("metadata does not fit"));
        }
        let at = self.cursor;
        self.cursor += n;
        match self.used.last_mut() {
            Some(last) if last.1 == at => last.1 += n,
            _ => self.used.push((at, at + n)),
        }
        Ok(at)
    }
}

fn plan(bytes: u64, o: &FormatOptions) -> Result<Layout> {
    let (bs, first, blocks, groups, ipg, itb, desc_blocks) = geometry(bytes, o)?;
    let bpg = 8 * bs;
    let mut l = Layout {
        bs,
        first,
        blocks,
        bpg,
        groups,
        ipg,
        itb,
        desc_blocks,
        meta: vec![(0, 0, 0); groups as usize],
        root_block: 0,
        lost_found: (0, 0),
        journal: Vec::new(),
        journal_leaf: None,
        journal_blocks: 0,
        used: Vec::new(),
    };
    let backups: Vec<(u64, u64)> = (0..groups).filter_map(|g| l.backup(g)).collect();
    let mut a = Alloc {
        backups: &backups,
        blocks,
        cursor: 0,
        used: Vec::new(),
    };
    let flex = 1u32 << LOG_GROUPS_PER_FLEX;
    for leader in (0..groups).step_by(flex as usize) {
        let members = leader..(leader + flex).min(groups);
        a.cursor = a.cursor.max(l.group_start(leader));
        for g in members.clone() {
            l.meta[g as usize].0 = a.take(1)?;
        }
        for g in members.clone() {
            l.meta[g as usize].1 = a.take(1)?;
        }
        for g in members {
            l.meta[g as usize].2 = a.take(itb)?;
        }
        if leader == 0 {
            l.root_block = a.take(1)?;
            let n = (16384 / bs).max(1);
            l.lost_found = (a.take(n)?, n);
        }
    }
    let mut used = a.used;
    used.extend(backups.iter().copied());
    used.sort_unstable();
    l.used = merge(used);

    let jblocks = match (o.journal, o.journal_mib) {
        (false, _) => None,
        (true, Some(mib)) => Some(mib.saturating_mul(MIB) / bs),
        (true, None) => default_journal_blocks(blocks),
    };
    if let Some(n) = jblocks {
        if n < 1024 || n > blocks / 2 || n > u32::MAX as u64 {
            return Err(Error::invalid(format!("journal of {n} blocks does not fit")));
        }
        place_journal(&mut l, n)?;
    }
    Ok(l)
}

fn merge(v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// Free runs at or after `from`, in order, wrapping around once.
fn free_runs(l: &Layout, from: u64) -> Vec<(u64, u64)> {
    let mut runs = Vec::new();
    let mut add = |lo: u64, hi: u64| {
        let mut pos = lo;
        let i = l.used.partition_point(|r| r.1 <= pos);
        for &(s, e) in &l.used[i..] {
            if s >= hi {
                break;
            }
            if s > pos {
                runs.push((pos, s));
            }
            pos = pos.max(e);
        }
        if pos < hi {
            runs.push((pos, hi));
        }
    };
    add(from, l.blocks);
    add(l.first, from);
    runs
}

/// The journal starts at the emptiest group around the middle of the
/// device (`get_midpoint_journal_block` in mke2fs), so it lands where
/// mke2fs would put it.
fn place_journal(l: &mut Layout, n: u64) -> Result<()> {
    let flex = 1u32 << LOG_GROUPS_PER_FLEX;
    let free_in = |g: u32| {
        let (s, e) = (l.group_start(g), l.group_start(g) + l.blocks_in_group(g));
        let i = l.used.partition_point(|r| r.1 <= s);
        let used: u64 = l.used[i..]
            .iter()
            .take_while(|r| r.0 < e)
            .map(|r| r.1.min(e) - r.0.max(s))
            .sum();
        e - s - used
    };
    let mut group = ((l.blocks - l.first) / 2 / l.bpg) as u32;
    let start = if group > flex {
        group &= !(flex - 1);
        while group < l.groups && free_in(group) == 0 {
            group += 1;
        }
        if group == l.groups {
            group = 0;
        }
        group
    } else {
        group.saturating_sub(1)
    };
    let end = (group + 1).min(l.groups - 1);
    let mut best = start;
    for g in start + 1..=end {
        if free_in(g) > free_in(best) {
            best = g;
        }
    }
    let mut runs = free_runs(l, l.group_start(best));
    // the first n free blocks of `runs`, as extents of at most
    // MAX_INIT_LEN blocks
    let gather = |runs: &[(u64, u64)]| -> Result<Vec<(u32, u64, u32)>> {
        let mut out = Vec::new();
        let mut lblk = 0u64;
        for &(s, e) in runs {
            let mut pos = s;
            while pos < e && lblk < n {
                let k = (e - pos).min(n - lblk).min(MAX_INIT_LEN as u64);
                out.push((lblk as u32, pos, k as u32));
                lblk += k;
                pos += k;
            }
        }
        if lblk < n {
            return Err(too_small("no room for the journal"));
        }
        Ok(out)
    };
    let mut extents = gather(&runs)?;
    let mut leaf = None;
    if extents.len() > 4 {
        // the extents need a tree leaf block: the first free block
        let first = runs.first_mut().ok_or_else(|| too_small("no room for the journal"))?;
        leaf = Some(first.0);
        first.0 += 1;
        extents = gather(&runs)?;
        if extents.len() > ext::max_entries(l.bs as usize) as usize {
            return Err(Error::invalid("journal too fragmented"));
        }
    }
    let mut taken: Vec<(u64, u64)> = extents.iter().map(|&(_, s, k)| (s, s + k as u64)).collect();
    taken.extend(leaf.map(|b| (b, b + 1)));
    l.journal = extents;
    l.journal_leaf = leaf;
    l.journal_blocks = n;
    let mut used = std::mem::take(&mut l.used);
    used.extend(taken);
    used.sort_unstable();
    l.used = merge(used);
    Ok(())
}

/// Fill 16 random bytes.
fn random16() -> Result<[u8; 16]> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

fn new_inode(m: u16, owner: (u32, u32), links: u16, t: Timestamp) -> Inode {
    let mut i = Inode::zeroed(INODE_SIZE as usize);
    i.set_mode(m);
    i.set_uid(owner.0);
    i.set_gid(owner.1);
    i.set_links_count(links);
    i.set_extra_isize(EXTRA_ISIZE);
    i.set_atime(t);
    i.set_ctime(t);
    i.set_mtime(t);
    i.set_crtime(t);
    i.set_flag(flags::EXTENTS, true);
    i
}

/// Extent root of an inode whose data is `extents` (at most 4).
fn inline_extents(i: &mut Inode, extents: &[(u32, u64, u32)]) {
    let area = i.block_area_mut();
    let mut h = ExtentHeader::empty(60, 0);
    h.entries = extents.len() as u16;
    h.write(area);
    for (k, &(block, start, len)) in extents.iter().enumerate() {
        Extent {
            block,
            len,
            start,
            unwritten: false,
        }
        .write(&mut area[ext::HEADER_SIZE + k * ext::ENTRY_SIZE..]);
    }
}

struct Writer<'a> {
    dev: &'a dyn BlockDevice,
    bs: u64,
    done: u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64),
}

impl Writer<'_> {
    fn write(&mut self, blk: u64, data: &[u8]) -> Result<()> {
        self.dev.write_at(blk * self.bs, data)?;
        self.done += data.len() as u64;
        (self.progress)(self.done.min(self.total), self.total);
        Ok(())
    }

    fn zero(&mut self, blk: u64, count: u64) -> Result<()> {
        let chunk_blocks = (MIB / self.bs).max(1);
        let zeros = vec![0u8; (chunk_blocks * self.bs) as usize];
        let mut pos = 0;
        while pos < count {
            let k = (count - pos).min(chunk_blocks);
            self.write(blk + pos, &zeros[..(k * self.bs) as usize])?;
            pos += k;
        }
        Ok(())
    }
}

/// Create a new ext4 file system on `dev`, reporting `(bytes written,
/// bytes to write)` to `progress`.
pub fn format(dev: &dyn BlockDevice, o: &FormatOptions, progress: &mut dyn FnMut(u64, u64)) -> Result<FormatSummary> {
    if dev.is_read_only() {
        return Err(Error::ReadOnly);
    }
    if o.label.len() > 16 {
        return Err(Error::invalid("label longer than 16 bytes"));
    }
    if !(0.0..=50.0).contains(&o.reserved_percent) {
        return Err(Error::invalid("reserved percentage must be between 0 and 50"));
    }
    let l = plan(dev.size(), o)?;
    let bs = l.bs;
    let uuid = match o.uuid {
        Some(u) => u,
        None => {
            let mut u = random16()?;
            u[6] = (u[6] & 0x0F) | 0x40;
            u[8] = (u[8] & 0x3F) | 0x80;
            u
        }
    };
    let hash_seed = random16()?;
    let now = o.time.unwrap_or_else(|| Timestamp::now().sec);
    let t = Timestamp::new(now, 0);
    let csum_seed = crate::csum::crc32c(!0, &uuid);

    // superblock fields first: inode checksums need nothing else, the
    // group summary fields are filled in once the bitmaps are known
    let mut sb = Superblock {
        raw: Box::new([0u8; SUPERBLOCK_SIZE]),
    };
    sb.set_feature_compat(
        compat::EXT_ATTR | compat::DIR_INDEX | if l.journal_blocks > 0 { compat::HAS_JOURNAL } else { 0 },
    );
    sb.set_feature_incompat(incompat::FILETYPE | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG);
    sb.set_feature_ro_compat(
        ro_compat::SPARSE_SUPER
            | ro_compat::LARGE_FILE
            | ro_compat::HUGE_FILE
            | ro_compat::DIR_NLINK
            | ro_compat::EXTRA_ISIZE
            | ro_compat::METADATA_CSUM,
    );
    let inodes = l.groups as u64 * l.ipg as u64;
    sb.set_inodes_count(inodes as u32);
    sb.set_blocks_count_lo(l.blocks as u32);
    sb.set_blocks_count_hi((l.blocks >> 32) as u32);
    let reserved = (l.blocks as f64 * o.reserved_percent / 100.0) as u64;
    sb.set_r_blocks_count_lo(reserved as u32);
    sb.set_r_blocks_count_hi((reserved >> 32) as u32);
    sb.set_first_data_block(l.first as u32);
    let log = (bs / 1024).trailing_zeros();
    sb.set_log_block_size(log);
    sb.set_log_cluster_size(log);
    sb.set_blocks_per_group(l.bpg as u32);
    sb.set_clusters_per_group(l.bpg as u32);
    sb.set_inodes_per_group(l.ipg);
    sb.set_wtime(now as u32);
    sb.set_wtime_hi((now >> 32) as u8);
    sb.set_max_mnt_count(0xFFFF);
    sb.set_magic(EXT4_MAGIC);
    sb.set_state(STATE_VALID);
    sb.set_errors(1);
    sb.set_lastcheck(now as u32);
    sb.set_rev_level(1);
    sb.set_first_ino_raw(FIRST_INO);
    sb.set_inode_size_raw(INODE_SIZE as u16);
    sb.raw[0x68..0x78].copy_from_slice(&uuid);
    sb.set_volume_name(&o.label);
    let mut seed = [0u32; 4];
    for (k, s) in seed.iter_mut().enumerate() {
        *s = u32::from_le_bytes(hash_seed[k * 4..k * 4 + 4].try_into().unwrap());
    }
    sb.set_hash_seed(seed);
    sb.set_def_hash_version(hash_version::HALF_MD4);
    sb.set_desc_size_raw(DESC_SIZE as u16);
    sb.set_default_mount_opts(DEFAULT_MOUNT_OPTS);
    sb.set_mkfs_time(now as u32);
    sb.set_min_extra_isize(EXTRA_ISIZE);
    sb.set_want_extra_isize(EXTRA_ISIZE);
    sb.set_flags(FLAGS_SIGNED_HASH);
    sb.set_log_groups_per_flex(LOG_GROUPS_PER_FLEX as u8);
    sb.set_checksum_type(CHECKSUM_TYPE_CRC32C);

    // amount of writing, for progress
    let total = SUPERBLOCK_OFFSET * 64
        + l.journal_blocks * bs
        + l.itb * bs
        + (1 + l.lost_found.1) * bs
        + l.groups as u64 * 2 * bs
        + (0..l.groups).filter(|&g| has_super(g)).count() as u64 * (1 + l.desc_blocks) * bs;
    let mut w = Writer {
        dev,
        bs,
        done: 0,
        total,
        progress,
    };

    // invalidate whatever file system was there before anything else
    dev.write_at(0, &vec![0u8; (SUPERBLOCK_OFFSET * 64).min(dev.size()) as usize])?;
    w.done += SUPERBLOCK_OFFSET * 64;

    // journal: zeroed, superblock in its first block
    let mut journal_inode = None;
    if l.journal_blocks > 0 {
        for &(_, start, len) in &l.journal {
            w.zero(start, len as u64)?;
        }
        let mut jsb = JournalSuperblock {
            raw: Box::new([0u8; JSB_SIZE]),
        };
        jsb.set_magic(JBD2_MAGIC);
        jsb.set_blocktype(JBD2_SUPERBLOCK_V2);
        jsb.set_block_size(bs as u32);
        jsb.set_max_len(l.journal_blocks as u32);
        jsb.set_first(1);
        jsb.set_sequence(1);
        jsb.set_nr_users(1);
        jsb.set_feature_incompat(JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_CSUM_V3);
        jsb.raw[0x30..0x40].copy_from_slice(&uuid);
        jsb.raw[0x50] = JBD2_CRC32C_CHKSUM;
        jsb.update_checksum();
        let mut first = vec![0u8; bs as usize];
        first[..JSB_SIZE].copy_from_slice(&jsb.raw[..]);
        w.write(l.journal[0].1, &first)?;

        let mut ji = new_inode(mode::S_IFREG | 0o600, (0, 0), 1, t);
        ji.set_size(l.journal_blocks * bs);
        let mut sectors = l.journal_blocks * (bs / 512);
        if let Some(leaf) = l.journal_leaf {
            sectors += bs / 512;
            let area = ji.block_area_mut();
            let mut h = ExtentHeader::empty(60, 1);
            h.entries = 1;
            h.write(area);
            ExtentIndex { block: 0, leaf }.write(&mut area[ext::HEADER_SIZE..]);
            let mut node = vec![0u8; bs as usize];
            let mut h = ExtentHeader::empty(bs as usize, 0);
            h.entries = l.journal.len() as u16;
            h.write(&mut node);
            for (k, &(block, start, len)) in l.journal.iter().enumerate() {
                Extent {
                    block,
                    len,
                    start,
                    unwritten: false,
                }
                .write(&mut node[ext::HEADER_SIZE + k * ext::ENTRY_SIZE..]);
            }
            ext::set_block_csum(Inode::csum_seed(csum_seed, JOURNAL_INO, 0), &mut node);
            w.write(leaf, &node)?;
        } else {
            inline_extents(&mut ji, &l.journal);
        }
        ji.set_sectors(sectors);
        ji.update_checksum(csum_seed, JOURNAL_INO);
        // backup of the journal inode's block map in the superblock
        sb.raw[0x10C..0x10C + 60].copy_from_slice(ji.block_area());
        crate::bytes::set_le32(&mut sb.raw[..], 0x10C + 15 * 4, ji.size_high());
        crate::bytes::set_le32(&mut sb.raw[..], 0x10C + 16 * 4, ji.size_lo());
        sb.set_jnl_backup_type(1);
        sb.set_journal_inum(JOURNAL_INO);
        journal_inode = Some(ji);
    }

    // root directory and lost+found
    let leaf_limit = bs as usize - de::TAIL_SIZE;
    let mut root = vec![0u8; bs as usize];
    de::write_entry(&mut root, 0, ROOT_INO, 12, b".", FT_DIR, bs as usize);
    de::write_entry(&mut root, 12, ROOT_INO, 12, b"..", FT_DIR, bs as usize);
    de::write_entry(
        &mut root,
        24,
        LOST_FOUND_INO,
        leaf_limit - 24,
        b"lost+found",
        FT_DIR,
        bs as usize,
    );
    de::init_tail(&mut root);
    de::set_leaf_csum(Inode::csum_seed(csum_seed, ROOT_INO, 0), &mut root);
    w.write(l.root_block, &root)?;
    let lpf_seed = Inode::csum_seed(csum_seed, LOST_FOUND_INO, 0);
    let (lpf_start, lpf_n) = l.lost_found;
    let mut lpf = vec![0u8; (lpf_n * bs) as usize];
    for (k, b) in lpf.chunks_mut(bs as usize).enumerate() {
        if k == 0 {
            de::write_entry(b, 0, LOST_FOUND_INO, 12, b".", FT_DIR, bs as usize);
            de::write_entry(b, 12, ROOT_INO, leaf_limit - 12, b"..", FT_DIR, bs as usize);
            de::init_tail(b);
        } else {
            de::init_empty_block(b, true);
        }
        de::set_leaf_csum(lpf_seed, b);
    }
    w.write(lpf_start, &lpf)?;

    // group 0's inode table: zeroed, then the root, journal and lost+found
    let (_, _, itable0) = l.meta[0];
    w.zero(itable0, l.itb)?;
    let mut ri = new_inode(mode::S_IFDIR | 0o755, o.root_owner, 3, t);
    ri.set_size(bs);
    ri.set_sectors(bs / 512);
    inline_extents(&mut ri, &[(0, l.root_block, 1)]);
    ri.update_checksum(csum_seed, ROOT_INO);
    let mut li = new_inode(mode::S_IFDIR | 0o700, (0, 0), 2, t);
    li.set_size(lpf_n * bs);
    li.set_sectors(lpf_n * bs / 512);
    inline_extents(&mut li, &[(0, lpf_start, lpf_n as u32)]);
    li.update_checksum(csum_seed, LOST_FOUND_INO);
    let mut table_head = vec![0u8; (INODE_SIZE * FIRST_INO as u64).div_ceil(bs) as usize * bs as usize];
    let mut put = |ino: u32, i: &Inode| {
        let off = ((ino - 1) as u64 * INODE_SIZE) as usize;
        table_head[off..off + INODE_SIZE as usize].copy_from_slice(&i.raw);
    };
    put(ROOT_INO, &ri);
    put(LOST_FOUND_INO, &li);
    if let Some(ji) = &journal_inode {
        put(JOURNAL_INO, ji);
    }
    w.write(itable0, &table_head)?;

    // bitmaps, one flex group at a time
    let flex = 1u32 << LOG_GROUPS_PER_FLEX;
    let mut gds: Vec<GroupDesc> = Vec::with_capacity(l.groups as usize);
    let mut free_total = 0u64;
    let mut ui = 0usize;
    let bits = (bs * 8) as usize;
    for leader in (0..l.groups).step_by(flex as usize) {
        for g in leader..(leader + flex).min(l.groups) {
            let start = l.group_start(g);
            let n = l.blocks_in_group(g);
            let end = start + n;
            let mut bb = vec![0u8; bs as usize];
            while ui < l.used.len() && l.used[ui].1 <= start {
                ui += 1;
            }
            let mut j = ui;
            let mut used = 0u64;
            while j < l.used.len() && l.used[j].0 < end {
                let (s, e) = (l.used[j].0.max(start), l.used[j].1.min(end));
                for b in s..e {
                    let bit = (b - start) as usize;
                    bb[bit / 8] |= 1 << (bit % 8);
                }
                used += e - s;
                j += 1;
            }
            for bit in n as usize..bits {
                bb[bit / 8] |= 1 << (bit % 8);
            }
            let free = n - used;
            free_total += free;
            let mut ib = vec![0u8; bs as usize];
            let used_inodes = if g == 0 { FIRST_INO } else { 0 };
            for bit in (0..used_inodes as usize).chain(l.ipg as usize..bits) {
                ib[bit / 8] |= 1 << (bit % 8);
            }
            let (bbm, ibm, it) = l.meta[g as usize];
            w.write(bbm, &bb)?;
            w.write(ibm, &ib)?;
            let mut gd = GroupDesc::zeroed(DESC_SIZE as usize);
            gd.set_block_bitmap(bbm);
            gd.set_inode_bitmap(ibm);
            gd.set_inode_table(it);
            gd.set_free_blocks_count(free as u32);
            gd.set_free_inodes_count(l.ipg - used_inodes);
            gd.set_used_dirs_count(if g == 0 { 2 } else { 0 });
            gd.set_itable_unused(l.ipg - used_inodes);
            gd.set_flag(if g == 0 { BG_INODE_ZEROED } else { BG_INODE_UNINIT });
            gd.set_block_bitmap_csum(bitmap_csum(csum_seed, &bb, l.bpg as u32));
            gd.set_inode_bitmap_csum(bitmap_csum(csum_seed, &ib, l.ipg));
            let c = gd.csum_metadata(csum_seed, g);
            gd.set_checksum(c);
            gds.push(gd);
        }
    }
    sb.set_free_blocks_count(free_total);
    sb.set_free_inodes_count((inodes - FIRST_INO as u64) as u32);

    // descriptor table and superblock copies; the primary superblock last
    let mut gdt = vec![0u8; (l.desc_blocks * bs) as usize];
    for (g, gd) in gds.iter().enumerate() {
        gdt[g * DESC_SIZE as usize..(g + 1) * DESC_SIZE as usize].copy_from_slice(&gd.raw);
    }
    for g in (0..l.groups).filter(|&g| has_super(g)).rev() {
        let start = l.group_start(g);
        let mut copy = sb.clone();
        copy.set_block_group_nr(g as u16);
        copy.update_checksum();
        if g == 0 {
            w.write(start + 1, &gdt)?;
            dev.write_at(SUPERBLOCK_OFFSET, &copy.raw[..])?;
        } else {
            let mut blk = vec![0u8; bs as usize];
            blk[..SUPERBLOCK_SIZE].copy_from_slice(&copy.raw[..]);
            w.write(start, &blk)?;
            w.write(start + 1, &gdt)?;
        }
    }
    dev.flush()?;
    (w.progress)(w.total, w.total);
    Ok(FormatSummary {
        block_size: bs as u32,
        blocks: l.blocks,
        inodes,
        groups: l.groups,
        journal_blocks: l.journal_blocks,
        uuid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn mke2fs_style_options() {
        let o = parse_args(
            &args(&[
                "-F",
                "-q",
                "-L",
                "data",
                "-b4096",
                "-i",
                "65536",
                "-m",
                "0.5",
                "-U",
                "12345678-9abc-4def-8123-456789abcdef",
                "-J",
                "size=64",
                "-E",
                "nodiscard,root_owner=1000:100",
            ]),
            (501, 20),
        )
        .unwrap();
        assert_eq!(o.label, "data");
        assert_eq!(o.block_size, Some(4096));
        assert_eq!(o.inode_ratio, Some(65536));
        assert_eq!(o.reserved_percent, 0.5);
        assert_eq!(o.uuid.unwrap()[0], 0x12);
        assert_eq!(o.journal_mib, Some(64));
        assert_eq!(o.root_owner, (1000, 100));
        assert!(o.journal);
        let o = parse_args(
            &args(&["-O", "^has_journal", "-E", "root_owner", "-N", "5000"]),
            (501, 20),
        )
        .unwrap();
        assert!(!o.journal);
        assert_eq!(o.root_owner, (501, 20));
        assert_eq!(o.inode_count, Some(5000));
        assert_eq!(parse_args(&[], (0, 0)).unwrap().reserved_percent, 5.0);
        for bad in [
            &["-L"][..],
            &["-b", "x"],
            &["-O", "inline_data"],
            &["-E", "stride=4"],
            &["-U", "nope"],
            &["-Z", "1"],
            &["label"],
            &["-J", "device=/dev/x"],
        ] {
            assert!(parse_args(&args(bad), (0, 0)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn journal_sizes_follow_mke2fs() {
        assert_eq!(default_journal_blocks(2047), None);
        assert_eq!(default_journal_blocks(2048), Some(1024));
        assert_eq!(default_journal_blocks(131072), Some(4096));
        assert_eq!(default_journal_blocks(4194304 - 1), Some(16384));
        assert_eq!(default_journal_blocks(124_942_924), Some(262144));
    }

    #[test]
    fn backup_groups() {
        let v: Vec<u32> = (0..100).filter(|&g| has_super(g)).collect();
        assert_eq!(v, vec![0, 1, 3, 5, 7, 9, 25, 27, 49, 81]);
    }

    #[test]
    fn geometry_matches_mke2fs_for_common_sizes() {
        // 512 GB disk: 4K blocks, 16384 bytes per inode
        let o = FormatOptions::default();
        let (bs, first, blocks, groups, ipg, itb, desc) = geometry(511_832_064_000, &o).unwrap();
        assert_eq!((bs, first), (4096, 0));
        assert_eq!(groups as u64, (blocks).div_ceil(32768));
        assert_eq!(ipg, 8192);
        assert_eq!(itb, 512);
        assert_eq!(desc, (groups as u64 * 64).div_ceil(4096));
        // small: 1K blocks, 4096 bytes per inode
        let (bs, first, _, groups, ipg, _, _) = geometry(64 * MIB, &o).unwrap();
        assert_eq!((bs, first, groups), (1024, 1, 8));
        assert_eq!(ipg, 2048);
        assert!(geometry(32 * 1024, &o).is_err());
    }

    #[test]
    fn last_group_too_small_is_dropped() {
        let o = FormatOptions {
            block_size: Some(4096),
            ..Default::default()
        };
        // two full groups and 100 blocks
        let (_, _, blocks, groups, ..) = geometry((2 * 32768 + 100) * 4096, &o).unwrap();
        assert_eq!((blocks, groups), (65536, 2));
        let (_, _, blocks, groups, ..) = geometry((2 * 32768 + 4000) * 4096, &o).unwrap();
        assert_eq!((blocks, groups), (2 * 32768 + 4000, 3));
    }

    #[test]
    fn layout_is_disjoint() {
        for size in [8 * MIB, 600 * MIB, 3 << 30, (2 * 32768 + 4000) * 4096] {
            let l = plan(size, &FormatOptions::default()).unwrap();
            let mut ranges: Vec<(u64, u64)> = Vec::new();
            for g in 0..l.groups {
                let (b, i, t) = l.meta[g as usize];
                ranges.extend([(b, b + 1), (i, i + 1), (t, t + l.itb)]);
                if let Some(r) = l.backup(g) {
                    ranges.push(r);
                }
            }
            ranges.push((l.root_block, l.root_block + 1));
            ranges.push((l.lost_found.0, l.lost_found.0 + l.lost_found.1));
            for &(_, s, n) in &l.journal {
                ranges.push((s, s + n as u64));
            }
            if let Some(leaf) = l.journal_leaf {
                ranges.push((leaf, leaf + 1));
            }
            ranges.sort_unstable();
            for w in ranges.windows(2) {
                assert!(w[0].1 <= w[1].0, "{size}: {:?} overlaps {:?}", w[0], w[1]);
            }
            assert!(ranges.last().unwrap().1 <= l.blocks);
            let journal: u64 = l.journal.iter().map(|e| e.2 as u64).sum();
            assert_eq!(journal, l.journal_blocks);
        }
    }
}
