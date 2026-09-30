//! The ext4 superblock (1024 bytes at byte offset 1024).

use super::le_fields;
use crate::bytes::{cstr, le32, lohi, set_le32};
use crate::csum::crc32c;
use crate::error::{Error, Result};

pub const SUPERBLOCK_OFFSET: u64 = 1024;
pub const SUPERBLOCK_SIZE: usize = 1024;
pub const EXT4_MAGIC: u16 = 0xEF53;

pub const STATE_VALID: u16 = 0x0001;
pub const STATE_ERROR: u16 = 0x0002;
pub const STATE_ORPHAN: u16 = 0x0004;

pub const FLAGS_SIGNED_HASH: u32 = 0x0001;
pub const FLAGS_UNSIGNED_HASH: u32 = 0x0002;

pub const CHECKSUM_TYPE_CRC32C: u8 = 1;

pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x0001;
    pub const IMAGIC_INODES: u32 = 0x0002;
    pub const HAS_JOURNAL: u32 = 0x0004;
    pub const EXT_ATTR: u32 = 0x0008;
    pub const RESIZE_INODE: u32 = 0x0010;
    pub const DIR_INDEX: u32 = 0x0020;
    pub const LAZY_BG: u32 = 0x0040;
    pub const EXCLUDE_INODE: u32 = 0x0080;
    pub const EXCLUDE_BITMAP: u32 = 0x0100;
    pub const SPARSE_SUPER2: u32 = 0x0200;
    pub const FAST_COMMIT: u32 = 0x0400;
    pub const STABLE_INODES: u32 = 0x0800;
    pub const ORPHAN_FILE: u32 = 0x1000;
}

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x0001;
    pub const LARGE_FILE: u32 = 0x0002;
    pub const BTREE_DIR: u32 = 0x0004;
    pub const HUGE_FILE: u32 = 0x0008;
    pub const GDT_CSUM: u32 = 0x0010;
    pub const DIR_NLINK: u32 = 0x0020;
    pub const EXTRA_ISIZE: u32 = 0x0040;
    pub const HAS_SNAPSHOT: u32 = 0x0080;
    pub const QUOTA: u32 = 0x0100;
    pub const BIGALLOC: u32 = 0x0200;
    pub const METADATA_CSUM: u32 = 0x0400;
    pub const REPLICA: u32 = 0x0800;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
    pub const SHARED_BLOCKS: u32 = 0x4000;
    pub const VERITY: u32 = 0x8000;
    pub const ORPHAN_PRESENT: u32 = 0x10000;
}

pub mod incompat {
    pub const COMPRESSION: u32 = 0x0001;
    pub const FILETYPE: u32 = 0x0002;
    pub const RECOVER: u32 = 0x0004;
    pub const JOURNAL_DEV: u32 = 0x0008;
    pub const META_BG: u32 = 0x0010;
    pub const EXTENTS: u32 = 0x0040;
    pub const BIT64: u32 = 0x0080;
    pub const MMP: u32 = 0x0100;
    pub const FLEX_BG: u32 = 0x0200;
    pub const EA_INODE: u32 = 0x0400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
    pub const CASEFOLD: u32 = 0x20000;
}

/// Hash algorithms for htree directories (`s_def_hash_version`).
pub mod hash_version {
    pub const LEGACY: u8 = 0;
    pub const HALF_MD4: u8 = 1;
    pub const TEA: u8 = 2;
    pub const LEGACY_UNSIGNED: u8 = 3;
    pub const HALF_MD4_UNSIGNED: u8 = 4;
    pub const TEA_UNSIGNED: u8 = 5;
    pub const SIPHASH: u8 = 6;
}

#[derive(Clone)]
pub struct Superblock {
    pub raw: Box<[u8; SUPERBLOCK_SIZE]>,
}

impl std::fmt::Debug for Superblock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Superblock")
            .field("blocks", &self.blocks_count())
            .field("inodes", &self.inodes_count())
            .field("block_size", &self.block_size())
            .field("compat", &format_args!("{:#x}", self.feature_compat()))
            .field("incompat", &format_args!("{:#x}", self.feature_incompat()))
            .field("ro_compat", &format_args!("{:#x}", self.feature_ro_compat()))
            .finish()
    }
}

impl Superblock {
    le_fields! {
        inodes_count, set_inodes_count: u32 @ 0x0;
        blocks_count_lo, set_blocks_count_lo: u32 @ 0x4;
        r_blocks_count_lo, set_r_blocks_count_lo: u32 @ 0x8;
        free_blocks_count_lo, set_free_blocks_count_lo: u32 @ 0xC;
        free_inodes_count, set_free_inodes_count: u32 @ 0x10;
        first_data_block, set_first_data_block: u32 @ 0x14;
        log_block_size, set_log_block_size: u32 @ 0x18;
        log_cluster_size, set_log_cluster_size: u32 @ 0x1C;
        blocks_per_group, set_blocks_per_group: u32 @ 0x20;
        clusters_per_group, set_clusters_per_group: u32 @ 0x24;
        inodes_per_group, set_inodes_per_group: u32 @ 0x28;
        mtime, set_mtime: u32 @ 0x2C;
        wtime, set_wtime: u32 @ 0x30;
        mnt_count, set_mnt_count: u16 @ 0x34;
        max_mnt_count, set_max_mnt_count: u16 @ 0x36;
        magic, set_magic: u16 @ 0x38;
        state, set_state: u16 @ 0x3A;
        errors, set_errors: u16 @ 0x3C;
        minor_rev_level, set_minor_rev_level: u16 @ 0x3E;
        lastcheck, set_lastcheck: u32 @ 0x40;
        checkinterval, set_checkinterval: u32 @ 0x44;
        creator_os, set_creator_os: u32 @ 0x48;
        rev_level, set_rev_level: u32 @ 0x4C;
        def_resuid, set_def_resuid: u16 @ 0x50;
        def_resgid, set_def_resgid: u16 @ 0x52;
        first_ino_raw, set_first_ino_raw: u32 @ 0x54;
        inode_size_raw, set_inode_size_raw: u16 @ 0x58;
        block_group_nr, set_block_group_nr: u16 @ 0x5A;
        feature_compat, set_feature_compat: u32 @ 0x5C;
        feature_incompat, set_feature_incompat: u32 @ 0x60;
        feature_ro_compat, set_feature_ro_compat: u32 @ 0x64;
        reserved_gdt_blocks, set_reserved_gdt_blocks: u16 @ 0xCE;
        journal_inum, set_journal_inum: u32 @ 0xE0;
        journal_dev, set_journal_dev: u32 @ 0xE4;
        last_orphan, set_last_orphan: u32 @ 0xE8;
        def_hash_version, set_def_hash_version: u8 @ 0xFC;
        jnl_backup_type, set_jnl_backup_type: u8 @ 0xFD;
        desc_size_raw, set_desc_size_raw: u16 @ 0xFE;
        default_mount_opts, set_default_mount_opts: u32 @ 0x100;
        first_meta_bg, set_first_meta_bg: u32 @ 0x104;
        mkfs_time, set_mkfs_time: u32 @ 0x108;
        blocks_count_hi, set_blocks_count_hi: u32 @ 0x150;
        r_blocks_count_hi, set_r_blocks_count_hi: u32 @ 0x154;
        free_blocks_count_hi, set_free_blocks_count_hi: u32 @ 0x158;
        min_extra_isize, set_min_extra_isize: u16 @ 0x15C;
        want_extra_isize, set_want_extra_isize: u16 @ 0x15E;
        flags, set_flags: u32 @ 0x160;
        mmp_interval, set_mmp_interval: u16 @ 0x166;
        mmp_block, set_mmp_block: u64 @ 0x168;
        log_groups_per_flex, set_log_groups_per_flex: u8 @ 0x174;
        checksum_type, set_checksum_type: u8 @ 0x175;
        kbytes_written, set_kbytes_written: u64 @ 0x178;
        usr_quota_inum, set_usr_quota_inum: u32 @ 0x240;
        grp_quota_inum, set_grp_quota_inum: u32 @ 0x244;
        backup_bg0, set_backup_bg0: u32 @ 0x24C;
        backup_bg1, set_backup_bg1: u32 @ 0x250;
        lpf_ino, set_lpf_ino: u32 @ 0x268;
        prj_quota_inum, set_prj_quota_inum: u32 @ 0x26C;
        checksum_seed, set_checksum_seed: u32 @ 0x270;
        wtime_hi, set_wtime_hi: u8 @ 0x274;
        mtime_hi, set_mtime_hi: u8 @ 0x275;
        encoding, set_encoding: u16 @ 0x27C;
        orphan_file_inum, set_orphan_file_inum: u32 @ 0x280;
        checksum, set_checksum: u32 @ 0x3FC;
    }

    /// Parse and validate a raw superblock. Checksum is verified when the
    /// `metadata_csum` feature is enabled.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        if raw.len() < SUPERBLOCK_SIZE {
            return Err(Error::corrupt("superblock too short"));
        }
        let mut b = Box::new([0u8; SUPERBLOCK_SIZE]);
        b.copy_from_slice(&raw[..SUPERBLOCK_SIZE]);
        let sb = Superblock { raw: b };
        sb.validate()?;
        Ok(sb)
    }

    /// Only checks the magic number; used by probing.
    pub fn has_magic(raw: &[u8]) -> bool {
        raw.len() >= 0x3A && crate::bytes::le16(raw, 0x38) == EXT4_MAGIC
    }

    fn validate(&self) -> Result<()> {
        if self.magic() != EXT4_MAGIC {
            return Err(Error::corrupt(format!("bad magic {:#x}", self.magic())));
        }
        if self.log_block_size() > 6 {
            return Err(Error::corrupt(format!(
                "invalid block size log {}",
                self.log_block_size()
            )));
        }
        if self.has_ro_compat(ro_compat::METADATA_CSUM) {
            if self.checksum_type() != CHECKSUM_TYPE_CRC32C {
                return Err(Error::corrupt("unknown checksum type"));
            }
            let want = self.compute_checksum();
            if want != self.checksum() {
                return Err(Error::Checksum(format!(
                    "superblock: stored {:#010x} computed {:#010x}",
                    self.checksum(),
                    want
                )));
            }
        }
        if self.blocks_per_group() == 0 || self.inodes_per_group() == 0 {
            return Err(Error::corrupt("zero blocks/inodes per group"));
        }
        if self.clusters_per_group() > self.block_size() * 8 {
            return Err(Error::corrupt("clusters per group exceeds bitmap size"));
        }
        if self.inodes_per_group() > self.block_size() * 8 {
            return Err(Error::corrupt("inodes per group exceeds bitmap size"));
        }
        let isz = self.inode_size() as u32;
        if isz < 128 || !isz.is_power_of_two() || isz > self.block_size() {
            return Err(Error::corrupt(format!("invalid inode size {isz}")));
        }
        if self.is_64bit() {
            let ds = self.desc_size_raw() as u32;
            if !(64..=1024).contains(&ds) || !ds.is_power_of_two() {
                return Err(Error::corrupt(format!("invalid descriptor size {ds}")));
            }
        }
        if self.blocks_count() <= self.first_data_block() as u64 {
            return Err(Error::corrupt("block count smaller than first data block"));
        }
        // ext4 addresses at most 2^48 blocks; also keeps byte sizes in u64
        if self.blocks_count() > 1 << 48 {
            return Err(Error::corrupt("block count too large"));
        }
        let groups64 = (self.blocks_count() - self.first_data_block() as u64).div_ceil(self.blocks_per_group() as u64);
        if groups64 > u32::MAX as u64 {
            return Err(Error::corrupt("too many block groups"));
        }
        let groups = self.group_count();
        if groups as u64 * self.inodes_per_group() as u64 != self.inodes_count() as u64 {
            return Err(Error::corrupt(format!(
                "inode count {} != groups {} * inodes per group {}",
                self.inodes_count(),
                groups,
                self.inodes_per_group()
            )));
        }
        Ok(())
    }

    pub fn compute_checksum(&self) -> u32 {
        crc32c(!0, &self.raw[..0x3FC])
    }

    /// Recompute and store the checksum (only meaningful with metadata_csum).
    pub fn update_checksum(&mut self) {
        if self.has_ro_compat(ro_compat::METADATA_CSUM) {
            let c = self.compute_checksum();
            self.set_checksum(c);
        }
    }

    pub fn block_size(&self) -> u32 {
        1024u32 << self.log_block_size()
    }

    pub fn blocks_count(&self) -> u64 {
        if self.is_64bit() {
            lohi(self.blocks_count_lo(), self.blocks_count_hi())
        } else {
            self.blocks_count_lo() as u64
        }
    }

    pub fn r_blocks_count(&self) -> u64 {
        if self.is_64bit() {
            lohi(self.r_blocks_count_lo(), self.r_blocks_count_hi())
        } else {
            self.r_blocks_count_lo() as u64
        }
    }

    pub fn free_blocks_count(&self) -> u64 {
        if self.is_64bit() {
            lohi(self.free_blocks_count_lo(), self.free_blocks_count_hi())
        } else {
            self.free_blocks_count_lo() as u64
        }
    }

    pub fn set_free_blocks_count(&mut self, v: u64) {
        self.set_free_blocks_count_lo(v as u32);
        if self.is_64bit() {
            self.set_free_blocks_count_hi((v >> 32) as u32);
        }
    }

    pub fn inode_size(&self) -> u16 {
        if self.rev_level() == 0 {
            128
        } else {
            self.inode_size_raw()
        }
    }

    pub fn first_ino(&self) -> u32 {
        if self.rev_level() == 0 {
            11
        } else {
            self.first_ino_raw()
        }
    }

    pub fn desc_size(&self) -> u32 {
        if self.is_64bit() {
            self.desc_size_raw() as u32
        } else {
            32
        }
    }

    pub fn group_count(&self) -> u32 {
        (self.blocks_count() - self.first_data_block() as u64).div_ceil(self.blocks_per_group() as u64) as u32
    }

    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0x68..0x78]);
        u
    }

    pub fn volume_name(&self) -> String {
        String::from_utf8_lossy(cstr(&self.raw[0x78..0x88])).into_owned()
    }

    pub fn set_volume_name(&mut self, name: &str) {
        let field = &mut self.raw[0x78..0x88];
        field.fill(0);
        let n = name.len().min(16);
        field[..n].copy_from_slice(&name.as_bytes()[..n]);
    }

    pub fn last_mounted(&self) -> String {
        String::from_utf8_lossy(cstr(&self.raw[0x88..0xC8])).into_owned()
    }

    pub fn set_last_mounted(&mut self, path: &str) {
        let field = &mut self.raw[0x88..0xC8];
        field.fill(0);
        let n = path.len().min(63);
        field[..n].copy_from_slice(&path.as_bytes()[..n]);
    }

    pub fn journal_uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[0xD0..0xE0]);
        u
    }

    pub fn hash_seed(&self) -> [u32; 4] {
        [
            le32(&self.raw[..], 0xEC),
            le32(&self.raw[..], 0xF0),
            le32(&self.raw[..], 0xF4),
            le32(&self.raw[..], 0xF8),
        ]
    }

    pub fn set_hash_seed(&mut self, seed: [u32; 4]) {
        for (i, s) in seed.iter().enumerate() {
            set_le32(&mut self.raw[..], 0xEC + i * 4, *s);
        }
    }

    /// Backup of the journal inode's `i_block` + size (`s_jnl_blocks`).
    pub fn jnl_blocks(&self) -> [u32; 17] {
        let mut out = [0u32; 17];
        for (i, o) in out.iter_mut().enumerate() {
            *o = le32(&self.raw[..], 0x10C + i * 4);
        }
        out
    }

    pub fn has_compat(&self, f: u32) -> bool {
        self.feature_compat() & f != 0
    }

    pub fn has_incompat(&self, f: u32) -> bool {
        self.feature_incompat() & f != 0
    }

    pub fn has_ro_compat(&self, f: u32) -> bool {
        self.feature_ro_compat() & f != 0
    }

    pub fn is_64bit(&self) -> bool {
        self.has_incompat(incompat::BIT64)
    }

    pub fn has_metadata_csum(&self) -> bool {
        self.has_ro_compat(ro_compat::METADATA_CSUM)
    }

    pub fn has_gdt_csum(&self) -> bool {
        self.has_ro_compat(ro_compat::GDT_CSUM)
    }

    /// Seed for all metadata checksums: `s_checksum_seed` when `csum_seed` is
    /// enabled, otherwise crc32c over the file system UUID.
    pub fn csum_seed(&self) -> u32 {
        if self.has_incompat(incompat::CSUM_SEED) {
            self.checksum_seed()
        } else {
            crc32c(!0, &self.raw[0x68..0x78])
        }
    }

    /// Groups per flex group (1 when flex_bg is disabled).
    pub fn groups_per_flex(&self) -> u32 {
        if self.has_incompat(incompat::FLEX_BG) && self.log_groups_per_flex() < 32 {
            1 << self.log_groups_per_flex()
        } else {
            1
        }
    }

    /// Whether group `g` holds a superblock backup (and GDT copy).
    pub fn group_has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if self.has_compat(compat::SPARSE_SUPER2) {
            return g == self.backup_bg0() || g == self.backup_bg1();
        }
        if g <= 1 || !self.has_ro_compat(ro_compat::SPARSE_SUPER) {
            return true;
        }
        if g & 1 == 0 {
            return false;
        }
        is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
    }

    /// Effective htree hash version, accounting for the unsigned-char flag.
    pub fn effective_hash_version(&self, v: u8) -> u8 {
        if v <= hash_version::TEA && self.flags() & FLAGS_UNSIGNED_HASH != 0 {
            v + 3
        } else {
            v
        }
    }
}

fn is_power_of(mut g: u32, base: u32) -> bool {
    while g > 1 {
        if g % base != 0 {
            return false;
        }
        g /= base;
    }
    g == 1
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A plausible 64bit/metadata_csum superblock for unit tests:
    /// 4K blocks, 32768 blocks, 1 group, 8192 inodes.
    pub fn sample() -> Superblock {
        let mut sb = Superblock {
            raw: Box::new([0u8; SUPERBLOCK_SIZE]),
        };
        sb.set_inodes_count(8192);
        sb.set_blocks_count_lo(32768);
        sb.set_free_blocks_count_lo(30000);
        sb.set_free_inodes_count(8000);
        sb.set_first_data_block(0);
        sb.set_log_block_size(2);
        sb.set_log_cluster_size(2);
        sb.set_blocks_per_group(32768);
        sb.set_clusters_per_group(32768);
        sb.set_inodes_per_group(8192);
        sb.set_magic(EXT4_MAGIC);
        sb.set_state(STATE_VALID);
        sb.set_rev_level(1);
        sb.set_first_ino_raw(11);
        sb.set_inode_size_raw(256);
        sb.set_feature_compat(compat::HAS_JOURNAL | compat::EXT_ATTR | compat::DIR_INDEX);
        sb.set_feature_incompat(incompat::FILETYPE | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG);
        sb.set_feature_ro_compat(
            ro_compat::SPARSE_SUPER
                | ro_compat::LARGE_FILE
                | ro_compat::HUGE_FILE
                | ro_compat::DIR_NLINK
                | ro_compat::EXTRA_ISIZE
                | ro_compat::METADATA_CSUM,
        );
        sb.set_desc_size_raw(64);
        sb.set_log_groups_per_flex(4);
        sb.set_checksum_type(CHECKSUM_TYPE_CRC32C);
        sb.raw[0x68..0x78].copy_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 1, 2, 3, 4, 5, 6, 7, 8]);
        sb.set_volume_name("testvol");
        sb.set_journal_inum(8);
        sb.update_checksum();
        sb
    }

    #[test]
    fn parse_sample() {
        let sb = sample();
        let p = Superblock::parse(&sb.raw[..]).unwrap();
        assert_eq!(p.block_size(), 4096);
        assert_eq!(p.blocks_count(), 32768);
        assert_eq!(p.group_count(), 1);
        assert_eq!(p.inode_size(), 256);
        assert_eq!(p.desc_size(), 64);
        assert_eq!(p.volume_name(), "testvol");
        assert_eq!(p.groups_per_flex(), 16);
        assert_eq!(p.first_ino(), 11);
        assert!(p.is_64bit());
        assert!(p.has_metadata_csum());
        assert!(format!("{p:?}").contains("block_size: 4096"));
    }

    #[test]
    fn bad_magic_rejected() {
        let mut sb = sample();
        sb.set_magic(0x1234);
        sb.update_checksum();
        assert!(matches!(Superblock::parse(&sb.raw[..]), Err(Error::Corrupt(_))));
        assert!(!Superblock::has_magic(&sb.raw[..]));
        assert!(Superblock::has_magic(&sample().raw[..]));
    }

    #[test]
    fn bad_checksum_rejected() {
        let mut sb = sample();
        sb.raw[0x100] ^= 1;
        assert!(matches!(Superblock::parse(&sb.raw[..]), Err(Error::Checksum(_))));
    }

    #[test]
    fn checksum_ignored_without_metadata_csum() {
        let mut sb = sample();
        sb.set_feature_ro_compat(sb.feature_ro_compat() & !ro_compat::METADATA_CSUM);
        sb.set_checksum(0xdeadbeef);
        assert!(Superblock::parse(&sb.raw[..]).is_ok());
    }

    #[test]
    fn short_buffer_rejected() {
        assert!(Superblock::parse(&[0u8; 100]).is_err());
    }

    #[test]
    fn invalid_geometry_rejected() {
        let mut sb = sample();
        sb.set_log_block_size(7);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_inode_size_raw(100);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_inode_size_raw(384);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_inodes_count(1000);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_desc_size_raw(32);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_blocks_per_group(0);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());

        let mut sb = sample();
        sb.set_clusters_per_group(40000);
        sb.update_checksum();
        assert!(Superblock::parse(&sb.raw[..]).is_err());
    }

    #[test]
    fn sixty_four_bit_counts() {
        let mut sb = sample();
        sb.set_blocks_count_hi(1);
        assert_eq!(sb.blocks_count(), (1 << 32) + 32768);
        sb.set_free_blocks_count((5u64 << 32) | 7);
        assert_eq!(sb.free_blocks_count_lo(), 7);
        assert_eq!(sb.free_blocks_count_hi(), 5);
        assert_eq!(sb.free_blocks_count(), (5u64 << 32) | 7);
        // without 64bit the hi half is ignored
        sb.set_feature_incompat(sb.feature_incompat() & !incompat::BIT64);
        assert_eq!(sb.blocks_count(), 32768);
        assert_eq!(sb.free_blocks_count(), 7);
        assert_eq!(sb.desc_size(), 32);
    }

    #[test]
    fn rev0_defaults() {
        let mut sb = sample();
        sb.set_rev_level(0);
        assert_eq!(sb.inode_size(), 128);
        assert_eq!(sb.first_ino(), 11);
    }

    #[test]
    fn sparse_super_groups() {
        let sb = sample();
        let with: Vec<u32> = (0..100).filter(|&g| sb.group_has_super(g)).collect();
        assert_eq!(with, vec![0, 1, 3, 5, 7, 9, 25, 27, 49, 81]);
    }

    #[test]
    fn non_sparse_groups() {
        let mut sb = sample();
        sb.set_feature_ro_compat(sb.feature_ro_compat() & !ro_compat::SPARSE_SUPER);
        assert!((0..20).all(|g| sb.group_has_super(g)));
    }

    #[test]
    fn sparse_super2_groups() {
        let mut sb = sample();
        sb.set_feature_compat(sb.feature_compat() | compat::SPARSE_SUPER2);
        sb.set_backup_bg0(1);
        sb.set_backup_bg1(9);
        let with: Vec<u32> = (0..20).filter(|&g| sb.group_has_super(g)).collect();
        assert_eq!(with, vec![0, 1, 9]);
    }

    #[test]
    fn csum_seed_variants() {
        let mut sb = sample();
        let from_uuid = crc32c(!0, &sb.uuid());
        assert_eq!(sb.csum_seed(), from_uuid);
        sb.set_feature_incompat(sb.feature_incompat() | incompat::CSUM_SEED);
        sb.set_checksum_seed(0x11223344);
        assert_eq!(sb.csum_seed(), 0x11223344);
    }

    #[test]
    fn names_roundtrip_and_truncate() {
        let mut sb = sample();
        sb.set_volume_name("a-very-long-volume-name");
        assert_eq!(sb.volume_name(), "a-very-long-volu");
        sb.set_last_mounted("/Volumes/x");
        assert_eq!(sb.last_mounted(), "/Volumes/x");
        sb.set_hash_seed([1, 2, 3, 4]);
        assert_eq!(sb.hash_seed(), [1, 2, 3, 4]);
    }

    #[test]
    fn unsigned_hash_flag() {
        let mut sb = sample();
        assert_eq!(sb.effective_hash_version(hash_version::HALF_MD4), 1);
        sb.set_flags(FLAGS_UNSIGNED_HASH);
        assert_eq!(sb.effective_hash_version(hash_version::HALF_MD4), 4);
        assert_eq!(sb.effective_hash_version(hash_version::LEGACY), 3);
        assert_eq!(sb.effective_hash_version(hash_version::SIPHASH), 6);
    }

    #[test]
    fn groups_per_flex_disabled() {
        let mut sb = sample();
        sb.set_feature_incompat(sb.feature_incompat() & !incompat::FLEX_BG);
        assert_eq!(sb.groups_per_flex(), 1);
    }

    #[test]
    fn group_count_rounds_up() {
        let mut sb = sample();
        sb.set_blocks_count_lo(32769);
        assert_eq!(sb.group_count(), 2);
        sb.set_first_data_block(1);
        sb.set_blocks_count_lo(32769);
        assert_eq!(sb.group_count(), 1);
    }
}
