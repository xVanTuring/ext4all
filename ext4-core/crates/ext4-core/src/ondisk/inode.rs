//! On-disk inode (`s_inode_size` bytes, 128 byte "good old" core + extra).

use super::le_fields;
use crate::bytes::{le32, lohi, set_le32};
use crate::csum::crc32c;

pub const ROOT_INO: u32 = 2;
pub const RESIZE_INO: u32 = 7;
pub const JOURNAL_INO: u32 = 8;
pub const GOOD_OLD_INODE_SIZE: usize = 128;
pub const N_BLOCKS: usize = 15;
/// Max link count for a regular inode (and a directory without dir_nlink).
pub const LINK_MAX: u16 = 65000;

pub mod mode {
    pub const S_IFMT: u16 = 0o170000;
    pub const S_IFSOCK: u16 = 0o140000;
    pub const S_IFLNK: u16 = 0o120000;
    pub const S_IFREG: u16 = 0o100000;
    pub const S_IFBLK: u16 = 0o060000;
    pub const S_IFDIR: u16 = 0o040000;
    pub const S_IFCHR: u16 = 0o020000;
    pub const S_IFIFO: u16 = 0o010000;
    pub const S_ISUID: u16 = 0o4000;
    pub const S_ISGID: u16 = 0o2000;
    pub const S_ISVTX: u16 = 0o1000;
}

pub mod flags {
    pub const SECRM: u32 = 0x1;
    pub const UNRM: u32 = 0x2;
    pub const COMPR: u32 = 0x4;
    pub const SYNC: u32 = 0x8;
    pub const IMMUTABLE: u32 = 0x10;
    pub const APPEND: u32 = 0x20;
    pub const NODUMP: u32 = 0x40;
    pub const NOATIME: u32 = 0x80;
    pub const ENCRYPT: u32 = 0x800;
    pub const INDEX: u32 = 0x1000;
    pub const IMAGIC: u32 = 0x2000;
    pub const JOURNAL_DATA: u32 = 0x4000;
    pub const NOTAIL: u32 = 0x8000;
    pub const DIRSYNC: u32 = 0x10000;
    pub const TOPDIR: u32 = 0x20000;
    pub const HUGE_FILE: u32 = 0x40000;
    pub const EXTENTS: u32 = 0x80000;
    pub const VERITY: u32 = 0x100000;
    pub const EA_INODE: u32 = 0x200000;
    pub const DAX: u32 = 0x2000000;
    pub const INLINE_DATA: u32 = 0x10000000;
    pub const PROJINHERIT: u32 = 0x20000000;
    pub const CASEFOLD: u32 = 0x40000000;
    /// Flags a newly created child inherits from its parent directory
    /// (EXT4_FL_INHERITED).
    pub const INHERITED: u32 =
        SECRM | UNRM | COMPR | SYNC | NODUMP | NOATIME | JOURNAL_DATA | NOTAIL | DIRSYNC | PROJINHERIT | CASEFOLD | DAX;
}

/// File type as stored in directory entries (`EXT4_FT_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FileType {
    Unknown = 0,
    Regular = 1,
    Directory = 2,
    CharDev = 3,
    BlockDev = 4,
    Fifo = 5,
    Socket = 6,
    Symlink = 7,
}

impl FileType {
    pub fn from_dirent(v: u8) -> FileType {
        match v {
            1 => FileType::Regular,
            2 => FileType::Directory,
            3 => FileType::CharDev,
            4 => FileType::BlockDev,
            5 => FileType::Fifo,
            6 => FileType::Socket,
            7 => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }

    pub fn from_mode(m: u16) -> FileType {
        match m & mode::S_IFMT {
            mode::S_IFREG => FileType::Regular,
            mode::S_IFDIR => FileType::Directory,
            mode::S_IFCHR => FileType::CharDev,
            mode::S_IFBLK => FileType::BlockDev,
            mode::S_IFIFO => FileType::Fifo,
            mode::S_IFSOCK => FileType::Socket,
            mode::S_IFLNK => FileType::Symlink,
            _ => FileType::Unknown,
        }
    }

    pub fn mode_bits(self) -> u16 {
        match self {
            FileType::Regular => mode::S_IFREG,
            FileType::Directory => mode::S_IFDIR,
            FileType::CharDev => mode::S_IFCHR,
            FileType::BlockDev => mode::S_IFBLK,
            FileType::Fifo => mode::S_IFIFO,
            FileType::Socket => mode::S_IFSOCK,
            FileType::Symlink => mode::S_IFLNK,
            FileType::Unknown => 0,
        }
    }
}

/// A timestamp with nanosecond precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub sec: i64,
    pub nsec: u32,
}

impl Timestamp {
    pub fn new(sec: i64, nsec: u32) -> Self {
        Timestamp { sec, nsec }
    }

    pub fn now() -> Self {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp {
            sec: d.as_secs() as i64,
            nsec: d.subsec_nanos(),
        }
    }

    /// Decode `(lo, extra)` as ext4 stores them.
    pub fn decode(lo: u32, extra: Option<u32>) -> Self {
        let mut sec = lo as i32 as i64;
        let mut nsec = 0;
        if let Some(extra) = extra {
            sec += ((extra & 3) as i64) << 32;
            nsec = extra >> 2;
        }
        Timestamp { sec, nsec }
    }

    /// Encode into `(lo, extra)`.
    pub fn encode(self) -> (u32, u32) {
        let lo = self.sec as u32;
        let epoch = ((self.sec - (self.sec as i32 as i64)) >> 32) as u32 & 3;
        (lo, epoch | (self.nsec.min(999_999_999) << 2))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Inode {
    pub raw: Vec<u8>,
}

impl std::fmt::Debug for Inode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inode")
            .field("mode", &format_args!("{:o}", self.mode()))
            .field("size", &self.size())
            .field("links", &self.links_count())
            .field("flags", &format_args!("{:#x}", self.flags()))
            .finish()
    }
}

impl Inode {
    le_fields! {
        mode, set_mode: u16 @ 0x0;
        uid_lo, set_uid_lo: u16 @ 0x2;
        size_lo, set_size_lo: u32 @ 0x4;
        atime_lo, set_atime_lo: u32 @ 0x8;
        ctime_lo, set_ctime_lo: u32 @ 0xC;
        mtime_lo, set_mtime_lo: u32 @ 0x10;
        dtime, set_dtime: u32 @ 0x14;
        gid_lo, set_gid_lo: u16 @ 0x18;
        links_count, set_links_count: u16 @ 0x1A;
        blocks_lo, set_blocks_lo: u32 @ 0x1C;
        flags, set_flags: u32 @ 0x20;
        version_lo, set_version_lo: u32 @ 0x24;
        generation, set_generation: u32 @ 0x64;
        file_acl_lo, set_file_acl_lo: u32 @ 0x68;
        size_high, set_size_high: u32 @ 0x6C;
        blocks_high, set_blocks_high: u16 @ 0x74;
        file_acl_high, set_file_acl_high: u16 @ 0x76;
        uid_high, set_uid_high: u16 @ 0x78;
        gid_high, set_gid_high: u16 @ 0x7A;
        checksum_lo, set_checksum_lo: u16 @ 0x7C;
    }

    pub fn new(raw: &[u8]) -> Self {
        Inode { raw: raw.to_vec() }
    }

    pub fn zeroed(size: usize) -> Self {
        Inode { raw: vec![0; size] }
    }

    pub fn extra_isize(&self) -> u16 {
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            crate::bytes::le16(&self.raw, 0x80)
        } else {
            0
        }
    }

    pub fn set_extra_isize(&mut self, v: u16) {
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            crate::bytes::set_le16(&mut self.raw, 0x80, v);
        }
    }

    /// Whether the extra field ending at `end` (offset from inode start) is
    /// covered by `i_extra_isize` (EXT4_FITS_IN_INODE).
    pub fn fits(&self, end: usize) -> bool {
        self.raw.len() > GOOD_OLD_INODE_SIZE
            && end <= GOOD_OLD_INODE_SIZE + self.extra_isize() as usize
            && end <= self.raw.len()
    }

    fn extra32(&self, off: usize) -> Option<u32> {
        if self.fits(off + 4) {
            Some(le32(&self.raw, off))
        } else {
            None
        }
    }

    fn set_extra32(&mut self, off: usize, v: u32) {
        if self.fits(off + 4) {
            set_le32(&mut self.raw, off, v);
        }
    }

    pub fn file_type(&self) -> FileType {
        FileType::from_mode(self.mode())
    }

    pub fn is_dir(&self) -> bool {
        self.mode() & mode::S_IFMT == mode::S_IFDIR
    }

    pub fn is_reg(&self) -> bool {
        self.mode() & mode::S_IFMT == mode::S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode() & mode::S_IFMT == mode::S_IFLNK
    }

    pub fn perm(&self) -> u16 {
        self.mode() & !mode::S_IFMT
    }

    pub fn uid(&self) -> u32 {
        self.uid_lo() as u32 | ((self.uid_high() as u32) << 16)
    }

    pub fn set_uid(&mut self, v: u32) {
        self.set_uid_lo(v as u16);
        self.set_uid_high((v >> 16) as u16);
    }

    pub fn gid(&self) -> u32 {
        self.gid_lo() as u32 | ((self.gid_high() as u32) << 16)
    }

    pub fn set_gid(&mut self, v: u32) {
        self.set_gid_lo(v as u16);
        self.set_gid_high((v >> 16) as u16);
    }

    pub fn size(&self) -> u64 {
        lohi(self.size_lo(), self.size_high())
    }

    pub fn set_size(&mut self, v: u64) {
        self.set_size_lo(v as u32);
        self.set_size_high((v >> 32) as u32);
    }

    pub fn file_acl(&self) -> u64 {
        lohi(self.file_acl_lo(), self.file_acl_high() as u32)
    }

    pub fn set_file_acl(&mut self, v: u64) {
        self.set_file_acl_lo(v as u32);
        self.set_file_acl_high((v >> 32) as u16);
    }

    pub fn has_flag(&self, f: u32) -> bool {
        self.flags() & f != 0
    }

    pub fn set_flag(&mut self, f: u32, on: bool) {
        let v = if on { self.flags() | f } else { self.flags() & !f };
        self.set_flags(v);
    }

    /// Raw `i_blocks` count: 48 bit value, unit depends on HUGE_FILE flag.
    pub fn blocks_raw(&self) -> u64 {
        lohi(self.blocks_lo(), self.blocks_high() as u32)
    }

    /// Number of 512-byte sectors charged to this inode.
    pub fn sectors(&self, block_size: u32, huge_file_feature: bool) -> u64 {
        let raw = if huge_file_feature {
            self.blocks_raw()
        } else {
            self.blocks_lo() as u64
        };
        if huge_file_feature && self.has_flag(flags::HUGE_FILE) {
            raw * (block_size as u64 / 512)
        } else {
            raw
        }
    }

    /// Store a sector count (always in 512 byte units, clears HUGE_FILE).
    pub fn set_sectors(&mut self, sectors: u64) {
        self.set_flag(flags::HUGE_FILE, false);
        self.set_blocks_lo(sectors as u32);
        self.set_blocks_high((sectors >> 32) as u16);
    }

    /// The 60-byte `i_block` area (extent root, block map, or inline data).
    pub fn block_area(&self) -> &[u8] {
        &self.raw[0x28..0x28 + 60]
    }

    pub fn block_area_mut(&mut self) -> &mut [u8] {
        &mut self.raw[0x28..0x28 + 60]
    }

    /// Classic block map pointer `i_block[i]`.
    pub fn block_ptr(&self, i: usize) -> u32 {
        le32(&self.raw, 0x28 + i * 4)
    }

    pub fn set_block_ptr(&mut self, i: usize, v: u32) {
        set_le32(&mut self.raw, 0x28 + i * 4, v);
    }

    /// Device number for character/block special files (old or new encoding).
    pub fn rdev(&self) -> u32 {
        let old = self.block_ptr(0);
        if old != 0 {
            // old_decode_dev: major 8 bits, minor 8 bits
            let major = (old >> 8) & 0xff;
            let minor = old & 0xff;
            (major << 24) | minor
        } else {
            // new_decode_dev
            let new = self.block_ptr(1);
            let major = (new & 0xfff00) >> 8;
            let minor = (new & 0xff) | ((new >> 12) & 0xfff00);
            (major << 24) | minor
        }
    }

    /// Store a Darwin-style dev_t (major << 24 | minor).
    pub fn set_rdev(&mut self, dev: u32) {
        let major = dev >> 24;
        let minor = dev & 0xff_ffff;
        if major < 256 && minor < 256 {
            self.set_block_ptr(0, (major << 8) | minor);
            self.set_block_ptr(1, 0);
        } else {
            self.set_block_ptr(0, 0);
            self.set_block_ptr(1, (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12));
        }
        self.set_block_ptr(2, 0);
    }

    pub fn atime(&self) -> Timestamp {
        Timestamp::decode(self.atime_lo(), self.extra32(0x8C))
    }

    pub fn ctime(&self) -> Timestamp {
        Timestamp::decode(self.ctime_lo(), self.extra32(0x84))
    }

    pub fn mtime(&self) -> Timestamp {
        Timestamp::decode(self.mtime_lo(), self.extra32(0x88))
    }

    /// Creation time, if the inode has room for it.
    pub fn crtime(&self) -> Option<Timestamp> {
        let lo = self.extra32(0x90)?;
        Some(Timestamp::decode(lo, self.extra32(0x94)))
    }

    pub fn set_atime(&mut self, t: Timestamp) {
        let (lo, ex) = t.encode();
        self.set_atime_lo(lo);
        self.set_extra32(0x8C, ex);
    }

    pub fn set_ctime(&mut self, t: Timestamp) {
        let (lo, ex) = t.encode();
        self.set_ctime_lo(lo);
        self.set_extra32(0x84, ex);
    }

    pub fn set_mtime(&mut self, t: Timestamp) {
        let (lo, ex) = t.encode();
        self.set_mtime_lo(lo);
        self.set_extra32(0x88, ex);
    }

    pub fn set_crtime(&mut self, t: Timestamp) {
        let (lo, ex) = t.encode();
        self.set_extra32(0x90, lo);
        self.set_extra32(0x94, ex);
    }

    pub fn version(&self) -> u64 {
        let hi = self.extra32(0x98).unwrap_or(0);
        lohi(self.version_lo(), hi)
    }

    pub fn set_version(&mut self, v: u64) {
        self.set_version_lo(v as u32);
        self.set_extra32(0x98, (v >> 32) as u32);
    }

    pub fn projid(&self) -> u32 {
        self.extra32(0x9C).unwrap_or(0)
    }

    pub fn set_projid(&mut self, v: u32) {
        self.set_extra32(0x9C, v);
    }

    /// Byte range of the in-inode extended attribute area, if any.
    pub fn xattr_area(&self) -> Option<std::ops::Range<usize>> {
        if self.raw.len() <= GOOD_OLD_INODE_SIZE {
            return None;
        }
        let start = GOOD_OLD_INODE_SIZE + self.extra_isize() as usize;
        if start + 4 > self.raw.len() {
            return None;
        }
        Some(start..self.raw.len())
    }

    /// Per-inode checksum seed: crc32c(crc32c(fs_seed, ino), generation).
    pub fn csum_seed(fs_seed: u32, ino: u32, generation: u32) -> u32 {
        let c = crc32c(fs_seed, &ino.to_le_bytes());
        crc32c(c, &generation.to_le_bytes())
    }

    fn has_csum_hi(&self) -> bool {
        self.fits(0x84)
    }

    pub fn compute_checksum(&self, fs_seed: u32, ino: u32) -> u32 {
        let seed = Self::csum_seed(fs_seed, ino, self.generation());
        let mut c = crc32c(seed, &self.raw[..0x7C]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &self.raw[0x7E..GOOD_OLD_INODE_SIZE]);
        if self.raw.len() > GOOD_OLD_INODE_SIZE {
            c = crc32c(c, &self.raw[GOOD_OLD_INODE_SIZE..0x82]);
            let mut off = 0x82;
            if self.has_csum_hi() {
                c = crc32c(c, &[0, 0]);
                off = 0x84;
            }
            c = crc32c(c, &self.raw[off..]);
        }
        if self.has_csum_hi() { c } else { c & 0xFFFF }
    }

    pub fn stored_checksum(&self) -> u32 {
        let lo = self.checksum_lo() as u32;
        if self.has_csum_hi() {
            lo | ((crate::bytes::le16(&self.raw, 0x82) as u32) << 16)
        } else {
            lo
        }
    }

    pub fn verify_checksum(&self, fs_seed: u32, ino: u32) -> bool {
        self.compute_checksum(fs_seed, ino) == self.stored_checksum()
    }

    pub fn update_checksum(&mut self, fs_seed: u32, ino: u32) {
        let c = self.compute_checksum(fs_seed, ino);
        self.set_checksum_lo(c as u16);
        if self.has_csum_hi() {
            crate::bytes::set_le16(&mut self.raw, 0x82, (c >> 16) as u16);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inode256() -> Inode {
        let mut i = Inode::zeroed(256);
        i.set_extra_isize(32);
        i
    }

    #[test]
    fn basic_fields() {
        let mut i = inode256();
        i.set_mode(mode::S_IFREG | 0o644);
        i.set_uid(0x12345);
        i.set_gid(0x23456);
        i.set_size(0x1_2345_6789);
        i.set_links_count(3);
        i.set_file_acl(0x1234_5678_9a);
        assert!(i.is_reg());
        assert!(!i.is_dir());
        assert_eq!(i.perm(), 0o644);
        assert_eq!(i.uid(), 0x12345);
        assert_eq!(i.gid(), 0x23456);
        assert_eq!(i.size(), 0x1_2345_6789);
        assert_eq!(i.links_count(), 3);
        assert_eq!(i.file_acl(), 0x1234_5678_9a);
        assert_eq!(i.file_type(), FileType::Regular);
        assert!(format!("{i:?}").contains("100644"));
    }

    #[test]
    fn file_types() {
        for ft in [
            FileType::Regular,
            FileType::Directory,
            FileType::CharDev,
            FileType::BlockDev,
            FileType::Fifo,
            FileType::Socket,
            FileType::Symlink,
        ] {
            assert_eq!(FileType::from_mode(ft.mode_bits() | 0o755), ft);
            assert_eq!(FileType::from_dirent(ft as u8), ft);
        }
        assert_eq!(FileType::from_dirent(0xDE), FileType::Unknown);
        assert_eq!(FileType::from_mode(0), FileType::Unknown);
        assert_eq!(FileType::Unknown.mode_bits(), 0);
    }

    #[test]
    fn timestamps_roundtrip() {
        let cases = [
            Timestamp::new(0, 0),
            Timestamp::new(1_700_000_000, 123_456_789),
            Timestamp::new(-1, 5),
            Timestamp::new(-2_000_000_000, 0),
            Timestamp::new(i32::MAX as i64 + 1, 1),
            Timestamp::new(0x3_0000_0000 + 17, 999_999_999),
        ];
        for t in cases {
            let (lo, ex) = t.encode();
            assert_eq!(Timestamp::decode(lo, Some(ex)), t, "{t:?}");
        }
    }

    #[test]
    fn timestamp_without_extra_is_signed_32() {
        assert_eq!(Timestamp::decode(0xFFFF_FFFF, None), Timestamp::new(-1, 0));
        assert_eq!(Timestamp::decode(5, None), Timestamp::new(5, 0));
    }

    #[test]
    fn timestamp_nsec_clamped() {
        let (_, ex) = Timestamp::new(1, 2_000_000_000).encode();
        assert_eq!(ex >> 2, 999_999_999);
    }

    #[test]
    fn inode_times() {
        let mut i = inode256();
        let t = Timestamp::new(1_700_000_000, 42);
        i.set_atime(t);
        i.set_mtime(Timestamp::new(5, 6));
        i.set_ctime(Timestamp::new(7, 8));
        i.set_crtime(Timestamp::new(9, 10));
        assert_eq!(i.atime(), t);
        assert_eq!(i.mtime(), Timestamp::new(5, 6));
        assert_eq!(i.ctime(), Timestamp::new(7, 8));
        assert_eq!(i.crtime(), Some(Timestamp::new(9, 10)));
    }

    #[test]
    fn small_inode_has_no_extra_times() {
        let mut i = Inode::zeroed(128);
        i.set_mtime(Timestamp::new(5, 6));
        assert_eq!(i.mtime(), Timestamp::new(5, 0));
        assert_eq!(i.crtime(), None);
        assert_eq!(i.extra_isize(), 0);
        assert!(i.xattr_area().is_none());
        i.set_extra_isize(4);
        assert_eq!(i.extra_isize(), 0);
    }

    #[test]
    fn partial_extra_isize() {
        let mut i = Inode::zeroed(256);
        i.set_extra_isize(4);
        i.set_mtime(Timestamp::new(5, 6));
        assert_eq!(i.mtime(), Timestamp::new(5, 0));
        assert_eq!(i.projid(), 0);
        assert_eq!(i.xattr_area(), Some(132..256));
    }

    #[test]
    fn sectors_and_huge_file() {
        let mut i = inode256();
        i.set_sectors(0x1_0000_0008);
        assert_eq!(i.sectors(4096, true), 0x1_0000_0008);
        assert_eq!(i.sectors(4096, false), 8);
        i.set_flag(flags::HUGE_FILE, true);
        assert_eq!(i.sectors(4096, true), 0x1_0000_0008 * 8);
        i.set_sectors(16);
        assert!(!i.has_flag(flags::HUGE_FILE));
        assert_eq!(i.sectors(4096, true), 16);
    }

    #[test]
    fn rdev_encoding() {
        let mut i = inode256();
        let small = (8u32 << 24) | 1;
        i.set_rdev(small);
        assert_eq!(i.block_ptr(0), 0x0801);
        assert_eq!(i.rdev(), small);
        let big = (300u32 << 24) | 70000;
        i.set_rdev(big);
        assert_eq!(i.block_ptr(0), 0);
        assert_eq!(i.rdev(), big);
    }

    #[test]
    fn checksum_roundtrip() {
        let mut i = inode256();
        i.set_mode(mode::S_IFDIR | 0o755);
        i.set_generation(0xabcdef);
        i.update_checksum(0x1234, 12);
        assert!(i.verify_checksum(0x1234, 12));
        assert!(!i.verify_checksum(0x1234, 13));
        assert!(!i.verify_checksum(0x1235, 12));
        i.raw[200] ^= 1;
        assert!(!i.verify_checksum(0x1234, 12));
    }

    #[test]
    fn checksum_matches_zeroed_field_definition() {
        let mut i = inode256();
        i.set_mode(0o100600);
        i.set_generation(77);
        i.raw[0x7C] = 0xAA;
        i.raw[0x83] = 0xBB;
        let mut z = i.raw.clone();
        z[0x7C] = 0;
        z[0x7D] = 0;
        z[0x82] = 0;
        z[0x83] = 0;
        let seed = Inode::csum_seed(5, 99, 77);
        assert_eq!(i.compute_checksum(5, 99), crc32c(seed, &z));
    }

    #[test]
    fn checksum_128_byte_inode_is_16_bit() {
        let mut i = Inode::zeroed(128);
        i.set_mode(0o100600);
        i.update_checksum(1, 2);
        assert!(i.compute_checksum(1, 2) <= 0xFFFF);
        assert!(i.verify_checksum(1, 2));
    }

    #[test]
    fn version_and_projid() {
        let mut i = inode256();
        i.set_version(0x1_0000_0002);
        i.set_projid(9);
        assert_eq!(i.version(), 0x1_0000_0002);
        assert_eq!(i.projid(), 9);
    }

    #[test]
    fn set_flag_toggles() {
        let mut i = inode256();
        i.set_flag(flags::EXTENTS, true);
        i.set_flag(flags::INDEX, true);
        assert!(i.has_flag(flags::EXTENTS));
        i.set_flag(flags::EXTENTS, false);
        assert!(!i.has_flag(flags::EXTENTS));
        assert!(i.has_flag(flags::INDEX));
    }

    #[test]
    fn block_area_accessors() {
        let mut i = inode256();
        i.block_area_mut()[0] = 0x0A;
        i.block_area_mut()[1] = 0xF3;
        assert_eq!(i.block_area()[..2], [0x0A, 0xF3]);
        i.set_block_ptr(14, 99);
        assert_eq!(i.block_ptr(14), 99);
        assert_eq!(i.block_area().len(), 60);
    }
}
