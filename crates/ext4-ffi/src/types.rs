//! `#[repr(C)]` types shared with Swift.

use ext4_core::{Attr, FileType, StatFs, Timestamp};
use std::ffi::c_void;

/// Block device callbacks. All callbacks return 0 or a Darwin errno.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ext4DeviceOps {
    pub ctx: *mut c_void,
    pub read: Option<unsafe extern "C" fn(ctx: *mut c_void, offset: u64, buf: *mut u8, len: usize) -> i32>,
    pub write: Option<unsafe extern "C" fn(ctx: *mut c_void, offset: u64, buf: *const u8, len: usize) -> i32>,
    pub flush: Option<unsafe extern "C" fn(ctx: *mut c_void) -> i32>,
    /// Called once when the device is no longer used (may be null).
    pub release: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
    /// Device size in bytes.
    pub size: u64,
    /// Required I/O alignment (physical block size); 0 means 512.
    pub sector_size: u32,
    pub read_only: bool,
}

/// File types (same values as ext4 directory entry types).
pub const EXT4_FT_UNKNOWN: u8 = 0;
pub const EXT4_FT_REG: u8 = 1;
pub const EXT4_FT_DIR: u8 = 2;
pub const EXT4_FT_CHR: u8 = 3;
pub const EXT4_FT_BLK: u8 = 4;
pub const EXT4_FT_FIFO: u8 = 5;
pub const EXT4_FT_SOCK: u8 = 6;
pub const EXT4_FT_LNK: u8 = 7;

/// ext4 inode flags reported in [`Ext4Attr::flags`] that decide whether a
/// file's data can be mapped for kernel offloaded I/O.
pub const EXT4_FL_EXTENTS: u32 = 0x0008_0000;
pub const EXT4_FL_INLINE_DATA: u32 = 0x1000_0000;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ext4Time {
    pub sec: i64,
    pub nsec: u32,
}

impl From<Timestamp> for Ext4Time {
    fn from(t: Timestamp) -> Self {
        Ext4Time {
            sec: t.sec,
            nsec: t.nsec,
        }
    }
}

impl From<Ext4Time> for Timestamp {
    fn from(t: Ext4Time) -> Self {
        Timestamp::new(t.sec, t.nsec)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ext4Attr {
    pub ino: u32,
    /// One of the `EXT4_FT_*` values.
    pub file_type: u8,
    pub has_crtime: bool,
    /// Full mode including the file type bits.
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub allocated: u64,
    pub atime: Ext4Time,
    pub mtime: Ext4Time,
    pub ctime: Ext4Time,
    pub crtime: Ext4Time,
    /// ext4 inode flags (`EXT4_*_FL`).
    pub flags: u32,
    /// BSD file flags derived from the ext4 flags (UF_IMMUTABLE, ...).
    pub bsd_flags: u32,
    pub generation: u32,
    /// Darwin dev_t for device nodes.
    pub rdev: u32,
}

pub const UF_NODUMP: u32 = 0x0000_0001;
pub const UF_IMMUTABLE: u32 = 0x0000_0002;
pub const UF_APPEND: u32 = 0x0000_0004;

const FL_IMMUTABLE: u32 = 0x10;
const FL_APPEND: u32 = 0x20;
const FL_NODUMP: u32 = 0x40;

pub fn bsd_flags_from_ext4(fl: u32) -> u32 {
    let mut b = 0;
    if fl & FL_IMMUTABLE != 0 {
        b |= UF_IMMUTABLE;
    }
    if fl & FL_APPEND != 0 {
        b |= UF_APPEND;
    }
    if fl & FL_NODUMP != 0 {
        b |= UF_NODUMP;
    }
    b
}

/// Merge BSD flags into existing ext4 flags.
pub fn ext4_flags_from_bsd(current: u32, bsd: u32) -> u32 {
    let mut f = current & !(FL_IMMUTABLE | FL_APPEND | FL_NODUMP);
    if bsd & UF_IMMUTABLE != 0 {
        f |= FL_IMMUTABLE;
    }
    if bsd & UF_APPEND != 0 {
        f |= FL_APPEND;
    }
    if bsd & UF_NODUMP != 0 {
        f |= FL_NODUMP;
    }
    f
}

pub fn ft_to_u8(ft: FileType) -> u8 {
    ft as u8
}

pub fn ft_from_u8(v: u8) -> FileType {
    FileType::from_dirent(v)
}

impl From<&Attr> for Ext4Attr {
    fn from(a: &Attr) -> Self {
        Ext4Attr {
            ino: a.ino,
            file_type: ft_to_u8(a.file_type),
            has_crtime: a.crtime.is_some(),
            mode: a.mode(),
            nlink: a.nlink,
            uid: a.uid,
            gid: a.gid,
            size: a.size,
            allocated: a.allocated,
            atime: a.atime.into(),
            mtime: a.mtime.into(),
            ctime: a.ctime.into(),
            crtime: a.crtime.unwrap_or_default().into(),
            flags: a.flags,
            bsd_flags: bsd_flags_from_ext4(a.flags),
            generation: a.generation,
            rdev: a.rdev,
        }
    }
}

/// Bits of [`Ext4SetAttr::valid`].
pub const EXT4_SET_MODE: u32 = 1 << 0;
pub const EXT4_SET_UID: u32 = 1 << 1;
pub const EXT4_SET_GID: u32 = 1 << 2;
pub const EXT4_SET_SIZE: u32 = 1 << 3;
pub const EXT4_SET_ATIME: u32 = 1 << 4;
pub const EXT4_SET_MTIME: u32 = 1 << 5;
pub const EXT4_SET_CTIME: u32 = 1 << 6;
pub const EXT4_SET_CRTIME: u32 = 1 << 7;
pub const EXT4_SET_BSD_FLAGS: u32 = 1 << 8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Ext4SetAttr {
    pub valid: u32,
    /// Permission bits (file type bits are ignored).
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Ext4Time,
    pub mtime: Ext4Time,
    pub ctime: Ext4Time,
    pub crtime: Ext4Time,
    pub bsd_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ext4StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub files: u64,
    pub free_files: u64,
    pub name_max: u32,
}

impl From<&StatFs> for Ext4StatFs {
    fn from(s: &StatFs) -> Self {
        Ext4StatFs {
            block_size: s.block_size,
            blocks: s.blocks,
            free_blocks: s.free_blocks,
            avail_blocks: s.avail_blocks,
            files: s.files,
            free_files: s.free_files,
            name_max: s.name_max,
        }
    }
}

/// Result of probing a device.
pub const EXT4_SUPPORT_READ_WRITE: i32 = 0;
pub const EXT4_SUPPORT_READ_ONLY: i32 = 1;
pub const EXT4_SUPPORT_UNSUPPORTED: i32 = 2;

/// What [`crate::ext4_format`] created.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Ext4FormatSummary {
    pub block_size: u32,
    pub blocks: u64,
    pub inodes: u64,
    pub groups: u32,
    pub journal_blocks: u64,
    pub uuid: [u8; 16],
}

/// Format progress: bytes written so far and in total.
pub type Ext4ProgressFn = Option<unsafe extern "C" fn(ctx: *mut c_void, done: u64, total: u64)>;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Ext4ProbeInfo {
    /// NUL-terminated volume label (UTF-8, may be empty).
    pub label: [u8; 17],
    pub uuid: [u8; 16],
    pub block_size: u32,
    pub blocks: u64,
    /// One of the `EXT4_SUPPORT_*` values.
    pub support: i32,
    /// Whether the journal needs recovery (volume was not cleanly unmounted).
    pub needs_recovery: bool,
    /// Whether the file system has an internal journal.
    pub has_journal: bool,
    /// 0 = ext2, 1 = ext3, 2 = ext4 (by feature set).
    pub subtype: u8,
}

impl Default for Ext4ProbeInfo {
    fn default() -> Self {
        Ext4ProbeInfo {
            label: [0; 17],
            uuid: [0; 16],
            block_size: 0,
            blocks: 0,
            support: EXT4_SUPPORT_UNSUPPORTED,
            needs_recovery: false,
            has_journal: false,
            subtype: 2,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Ext4MountOptions {
    pub read_only: bool,
    /// Metadata cache size in blocks (0 = default).
    pub cache_blocks: u32,
    /// Seconds between automatic commits (0 = default 5 s).
    pub commit_interval_secs: u32,
    /// Keep unlinked inodes until `ext4_reclaim` (true for FSKit).
    pub defer_unlinked: bool,
}

impl Default for Ext4MountOptions {
    fn default() -> Self {
        Ext4MountOptions {
            read_only: false,
            cache_blocks: 0,
            commit_interval_secs: 0,
            defer_unlinked: true,
        }
    }
}

/// Rename flags.
pub const EXT4_RENAME_NOREPLACE: u32 = 1;
pub const EXT4_RENAME_EXCHANGE: u32 = 2;

/// xattr set modes.
pub const EXT4_XATTR_ANY: u32 = 0;
pub const EXT4_XATTR_CREATE: u32 = 1;
pub const EXT4_XATTR_REPLACE: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsd_flag_mapping_roundtrip() {
        let ext = FL_IMMUTABLE | FL_NODUMP | 0x80000;
        let bsd = bsd_flags_from_ext4(ext);
        assert_eq!(bsd, UF_IMMUTABLE | UF_NODUMP);
        assert_eq!(ext4_flags_from_bsd(0x80000, bsd), ext);
        assert_eq!(ext4_flags_from_bsd(ext, 0), 0x80000);
        assert_eq!(ext4_flags_from_bsd(0, UF_APPEND), FL_APPEND);
    }

    #[test]
    fn time_conversion() {
        let t = Timestamp::new(-3, 9);
        let e: Ext4Time = t.into();
        assert_eq!(e, Ext4Time { sec: -3, nsec: 9 });
        assert_eq!(Timestamp::from(e), t);
    }

    #[test]
    fn file_type_codes() {
        assert_eq!(ft_to_u8(FileType::Directory), EXT4_FT_DIR);
        assert_eq!(ft_to_u8(FileType::Symlink), EXT4_FT_LNK);
        assert_eq!(ft_from_u8(EXT4_FT_REG), FileType::Regular);
        assert_eq!(ft_from_u8(EXT4_FT_SOCK), FileType::Socket);
        assert_eq!(ft_from_u8(EXT4_FT_FIFO), FileType::Fifo);
        assert_eq!(ft_from_u8(EXT4_FT_CHR), FileType::CharDev);
        assert_eq!(ft_from_u8(EXT4_FT_BLK), FileType::BlockDev);
        assert_eq!(ft_from_u8(EXT4_FT_UNKNOWN), FileType::Unknown);
    }

    #[test]
    fn defaults() {
        let o = Ext4MountOptions::default();
        assert!(o.defer_unlinked);
        assert!(!o.read_only);
        let p = Ext4ProbeInfo::default();
        assert_eq!(p.support, EXT4_SUPPORT_UNSUPPORTED);
    }
}
