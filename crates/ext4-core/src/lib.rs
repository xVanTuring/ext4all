//! Pure Rust ext4 implementation with read/write and jbd2 journal support.
//!
//! The crate is layered:
//! - [`device`]: block device abstraction
//! - [`ondisk`]: raw on-disk structures with checksums
//! - [`fs`]: the mounted file system and its high level operations

pub mod bytes;
pub mod cache;
pub mod crypto;
pub mod csum;
pub mod device;
pub mod error;
pub mod features;
pub mod fscrypt;
pub mod hash;
pub mod journal;
pub mod luks;
pub mod mkfs;
pub mod ondisk;

pub use device::{AlignedDevice, BlockDevice, FileDevice, MemDevice};
pub use error::{Error, Result};
pub mod fs;

pub use fs::{Attr, DirEntryInfo, Fs, Ino, IoExtent, MountOptions, RenameFlags, SetAttr, StatFs, XattrSetMode};
pub use mkfs::{FormatOptions, FormatSummary, format};
pub use ondisk::inode::{FileType, Timestamp};
