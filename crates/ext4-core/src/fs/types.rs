//! Public value types of the file system API.

use crate::ondisk::inode::{FileType, Timestamp};

/// Inode number.
pub type Ino = u32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub file_type: FileType,
    /// Permission bits (without the file type bits).
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// Bytes allocated on disk.
    pub allocated: u64,
    pub atime: Timestamp,
    pub mtime: Timestamp,
    pub ctime: Timestamp,
    pub crtime: Option<Timestamp>,
    pub flags: u32,
    pub generation: u32,
    /// Darwin-style dev_t for device nodes.
    pub rdev: u32,
}

impl Attr {
    pub fn mode(&self) -> u16 {
        self.file_type.mode_bits() | self.perm
    }

    pub fn is_dir(&self) -> bool {
        self.file_type == FileType::Directory
    }
}

/// A byte range of a file mapped for direct (kernel offloaded) I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoExtent {
    /// Offset in the file.
    pub logical: u64,
    /// Offset on the device (meaningless for `zero_fill`).
    pub physical: u64,
    pub length: u64,
    /// Reads must return zeros (hole or not yet written).
    pub zero_fill: bool,
}

/// One directory entry as returned by `read_dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryInfo {
    pub name: Vec<u8>,
    pub ino: Ino,
    pub file_type: FileType,
    /// Cookie to resume enumeration right after this entry.
    pub next_cookie: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatFs {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    /// Free blocks available to unprivileged users.
    pub avail_blocks: u64,
    pub files: u64,
    pub free_files: u64,
    pub name_max: u32,
}

/// Attribute changes for `set_attr`. `None` leaves a field untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub perm: Option<u16>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<Timestamp>,
    pub mtime: Option<Timestamp>,
    pub ctime: Option<Timestamp>,
    pub crtime: Option<Timestamp>,
    pub flags: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct MountOptions {
    pub read_only: bool,
    /// Metadata cache capacity in blocks.
    pub cache_blocks: usize,
    /// Fail on checksum mismatches (otherwise only log them).
    pub strict_checksums: bool,
    /// Commit automatically once this many metadata blocks are dirty
    /// (capped by the journal size).
    pub commit_threshold: usize,
}

impl Default for MountOptions {
    fn default() -> Self {
        MountOptions {
            read_only: false,
            cache_blocks: 16384,
            strict_checksums: true,
            commit_threshold: 4096,
        }
    }
}

/// Flags for `rename`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenameFlags {
    /// Fail with EEXIST if the target exists.
    pub no_replace: bool,
    /// Atomically swap source and target (both must exist).
    pub exchange: bool,
}

/// How `set_xattr` treats an existing / missing attribute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XattrSetMode {
    #[default]
    Any,
    /// Fail with EEXIST if it already exists.
    Create,
    /// Fail with ENOATTR if it does not exist.
    Replace,
}

/// Summary of what happened while mounting.
#[derive(Clone, Debug, Default)]
pub struct MountReport {
    pub journal_replayed: bool,
    pub replayed_transactions: u32,
    pub replayed_blocks: usize,
    pub orphans_processed: u32,
    pub read_only_reasons: Vec<String>,
}
