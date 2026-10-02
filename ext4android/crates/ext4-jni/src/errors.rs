//! Linux errno values for file system errors. The core's `Error::errno`
//! gives Darwin numbers (for FSKit); Android needs Linux ones, e.g. for the
//! ErrnoException a proxy file descriptor reports.

use ext4_core::Error;

pub fn linux_errno(e: &Error) -> i32 {
    match e {
        Error::Io(io) => io.raw_os_error().unwrap_or(5),
        // the device layers only produce EIO and ENXIO, which agree
        Error::Device(e) => *e,
        Error::Corrupt(_) | Error::Checksum(_) => 5, // EIO
        Error::Unsupported(_) => 95,                 // EOPNOTSUPP
        Error::NotFound => 2,                        // ENOENT
        Error::Exists => 17,                         // EEXIST
        Error::NotDir => 20,                         // ENOTDIR
        Error::IsDir => 21,                          // EISDIR
        Error::NotEmpty => 39,                       // ENOTEMPTY
        Error::NoSpace => 28,                        // ENOSPC
        Error::ReadOnly => 30,                       // EROFS
        Error::Invalid(_) => 22,                     // EINVAL
        Error::NameTooLong => 36,                    // ENAMETOOLONG
        Error::TooManyLinks => 31,                   // EMLINK
        Error::TooBig => 27,                         // EFBIG
        Error::Range => 34,                          // ERANGE
        Error::NoAttr => 61,                         // ENODATA
        Error::NotPermitted => 1,                    // EPERM
        Error::Busy => 16,                           // EBUSY
        Error::NoSuchOffset => 6,                    // ENXIO
        Error::StaleCookie => 116,                   // ESTALE
        Error::NoKey => 126,                         // ENOKEY
        Error::CrossDevice => 18,                    // EXDEV
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_numbers_differ_from_darwin_where_they_should() {
        assert_eq!(linux_errno(&Error::NotEmpty), 39);
        assert_eq!(Error::NotEmpty.errno(), 66);
        assert_eq!(linux_errno(&Error::NameTooLong), 36);
        assert_eq!(linux_errno(&Error::NoSpace), 28);
        assert_eq!(linux_errno(&Error::Device(5)), 5);
        let io: Error = std::io::Error::from_raw_os_error(13).into();
        assert_eq!(linux_errno(&io), 13);
    }
}
