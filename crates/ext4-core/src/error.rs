//! Error type shared by the whole crate.
//!
//! Every variant maps to a Darwin `errno` value so the FFI layer can hand the
//! code straight to FSKit as a `POSIXError`.

use std::fmt;

/// Darwin errno values (see `<sys/errno.h>`).
pub mod errno {
    pub const EPERM: i32 = 1;
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const ENXIO: i32 = 6;
    pub const EBADF: i32 = 9;
    pub const ENOMEM: i32 = 12;
    pub const EACCES: i32 = 13;
    pub const EBUSY: i32 = 16;
    pub const EEXIST: i32 = 17;
    pub const EXDEV: i32 = 18;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const EINVAL: i32 = 22;
    pub const EFBIG: i32 = 27;
    pub const ENOSPC: i32 = 28;
    pub const EROFS: i32 = 30;
    pub const EMLINK: i32 = 31;
    pub const ERANGE: i32 = 34;
    pub const ENOTSUP: i32 = 45;
    pub const ELOOP: i32 = 62;
    pub const ESTALE: i32 = 70;
    pub const ENAMETOOLONG: i32 = 63;
    pub const ENOTEMPTY: i32 = 66;
    pub const EFTYPE: i32 = 79;
    pub const ENOATTR: i32 = 93;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("device I/O error (errno {0})")]
    Device(i32),
    #[error("file system corrupted: {0}")]
    Corrupt(String),
    #[error("checksum mismatch: {0}")]
    Checksum(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("no such file or directory")]
    NotFound,
    #[error("file exists")]
    Exists,
    #[error("not a directory")]
    NotDir,
    #[error("is a directory")]
    IsDir,
    #[error("directory not empty")]
    NotEmpty,
    #[error("no space left on device")]
    NoSpace,
    #[error("read-only file system")]
    ReadOnly,
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("file name too long")]
    NameTooLong,
    #[error("too many links")]
    TooManyLinks,
    #[error("file too large")]
    TooBig,
    #[error("result too large")]
    Range,
    #[error("attribute not found")]
    NoAttr,
    #[error("operation not permitted")]
    NotPermitted,
    #[error("resource busy")]
    Busy,
    #[error("no data or hole past the given offset")]
    NoSuchOffset,
    #[error("directory cookie no longer valid")]
    StaleCookie,
}

impl Error {
    pub fn corrupt(msg: impl fmt::Display) -> Self {
        Error::Corrupt(msg.to_string())
    }

    pub fn invalid(msg: impl fmt::Display) -> Self {
        Error::Invalid(msg.to_string())
    }

    pub fn unsupported(msg: impl fmt::Display) -> Self {
        Error::Unsupported(msg.to_string())
    }

    /// Darwin errno for this error.
    pub fn errno(&self) -> i32 {
        use errno::*;
        match self {
            Error::Io(e) => e.raw_os_error().unwrap_or(EIO),
            Error::Device(e) => *e,
            Error::Corrupt(_) | Error::Checksum(_) => EIO,
            Error::Unsupported(_) => ENOTSUP,
            Error::NotFound => ENOENT,
            Error::Exists => EEXIST,
            Error::NotDir => ENOTDIR,
            Error::IsDir => EISDIR,
            Error::NotEmpty => ENOTEMPTY,
            Error::NoSpace => ENOSPC,
            Error::ReadOnly => EROFS,
            Error::Invalid(_) => EINVAL,
            Error::NameTooLong => ENAMETOOLONG,
            Error::TooManyLinks => EMLINK,
            Error::TooBig => EFBIG,
            Error::Range => ERANGE,
            Error::NoAttr => ENOATTR,
            Error::NotPermitted => EPERM,
            Error::Busy => EBUSY,
            Error::NoSuchOffset => ENXIO,
            Error::StaleCookie => ESTALE,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping_is_darwin() {
        assert_eq!(Error::NotFound.errno(), 2);
        assert_eq!(Error::Exists.errno(), 17);
        assert_eq!(Error::NotDir.errno(), 20);
        assert_eq!(Error::IsDir.errno(), 21);
        assert_eq!(Error::NoSpace.errno(), 28);
        assert_eq!(Error::ReadOnly.errno(), 30);
        assert_eq!(Error::NameTooLong.errno(), 63);
        assert_eq!(Error::NotEmpty.errno(), 66);
        assert_eq!(Error::NoAttr.errno(), 93);
        assert_eq!(Error::unsupported("x").errno(), 45);
        assert_eq!(Error::corrupt("x").errno(), 5);
        assert_eq!(Error::Device(16).errno(), 16);
    }

    #[test]
    fn io_error_keeps_os_code() {
        let e: Error = std::io::Error::from_raw_os_error(28).into();
        assert_eq!(e.errno(), 28);
        let e: Error = std::io::Error::other("x").into();
        assert_eq!(e.errno(), errno::EIO);
    }

    #[test]
    fn display_messages() {
        assert_eq!(Error::corrupt("bad sb").to_string(), "file system corrupted: bad sb");
        assert_eq!(Error::NotFound.to_string(), "no such file or directory");
    }
}
