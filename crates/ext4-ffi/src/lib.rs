//! C ABI over `ext4-core`, consumed by the Swift FSKit extension.
//!
//! Conventions:
//! - every function returns 0 on success or a Darwin errno;
//! - names are byte strings (pointer + length), not NUL-terminated;
//! - panics never cross the boundary: they become `EIO` and disable the
//!   volume;
//! - callbacks run while the volume lock is held and must not call back
//!   into this library.

pub mod device;
pub mod handle;
pub mod types;
pub mod xattr_names;

use device::CallbackDevice;
use ext4_core::error::errno::EIO;
use ext4_core::{AlignedDevice, Error, FileType, Fs, MountOptions, RenameFlags, Result, SetAttr, XattrSetMode};
pub use handle::Ext4Handle;
use std::ffi::{c_char, c_void};
use std::sync::Arc;
use std::time::Duration;
pub use types::*;

fn guard(f: impl FnOnce() -> Result<()>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e.errno(),
        Err(_) => EIO,
    }
}

/// # Safety
/// `p` must point to `len` readable bytes (or be null with `len == 0`).
unsafe fn bytes<'a>(p: *const u8, len: usize) -> Result<&'a [u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    if p.is_null() {
        return Err(Error::invalid("null pointer"));
    }
    // SAFETY: caller contract
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

/// # Safety
/// `h` must be a live handle from [`ext4_mount`].
unsafe fn handle<'a>(h: *const Ext4Handle) -> Result<&'a Ext4Handle> {
    if h.is_null() {
        return Err(Error::invalid("null handle"));
    }
    // SAFETY: caller contract
    Ok(unsafe { &*h })
}

/// # Safety
/// `p` must be null or valid for a write of `T`.
unsafe fn put<T>(p: *mut T, v: T) {
    if !p.is_null() {
        // SAFETY: caller contract
        unsafe { p.write(v) };
    }
}

// --- logging -------------------------------------------------------------------

/// Log sink: `level` is error=1 … trace=5, `msg` is NUL-terminated.
pub type Ext4LogCallback = Option<unsafe extern "C" fn(level: i32, msg: *const c_char)>;

struct CallbackLogger {
    cb: unsafe extern "C" fn(level: i32, msg: *const c_char),
}

impl log::Log for CallbackLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let level = match r.level() {
            log::Level::Error => 1,
            log::Level::Warn => 2,
            log::Level::Info => 3,
            log::Level::Debug => 4,
            log::Level::Trace => 5,
        };
        let msg = format!("{}: {}", r.target(), r.args());
        let c = std::ffi::CString::new(msg.replace('\0', " ")).unwrap_or_default();
        // SAFETY: the callback accepts a NUL-terminated string
        unsafe { (self.cb)(level, c.as_ptr()) };
    }

    fn flush(&self) {}
}

/// Route library log messages (error=1 … trace=5) to `cb`. Call once.
#[unsafe(no_mangle)]
pub extern "C" fn ext4_set_log_callback(cb: Ext4LogCallback) {
    if let Some(cb) = cb {
        let logger = Box::leak(Box::new(CallbackLogger { cb }));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}

/// Library version as a NUL-terminated string.
#[unsafe(no_mangle)]
pub extern "C" fn ext4_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// --- probe / mount -----------------------------------------------------------------

fn fill_probe(sb: &ext4_core::ondisk::superblock::Superblock) -> Ext4ProbeInfo {
    let mut info = Ext4ProbeInfo::default();
    let label = sb.volume_name();
    let lb = label.as_bytes();
    let n = lb.len().min(16);
    info.label[..n].copy_from_slice(&lb[..n]);
    info.uuid = sb.uuid();
    info.block_size = sb.block_size();
    info.blocks = sb.blocks_count();
    {
        use ext4_core::ondisk::superblock::{compat, incompat, ro_compat};
        info.needs_recovery = sb.has_incompat(incompat::RECOVER);
        info.has_journal = sb.has_compat(compat::HAS_JOURNAL);
        let ext4_only = incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG | incompat::INLINE_DATA;
        info.subtype = if sb.feature_incompat() & ext4_only != 0
            || sb.has_ro_compat(ro_compat::HUGE_FILE | ro_compat::DIR_NLINK | ro_compat::METADATA_CSUM)
        {
            2
        } else if info.has_journal {
            1
        } else {
            0
        };
    }
    info.support = match ext4_core::features::check(sb) {
        ext4_core::features::Support::ReadWrite => EXT4_SUPPORT_READ_WRITE,
        ext4_core::features::Support::ReadOnly(_) => EXT4_SUPPORT_READ_ONLY,
        ext4_core::features::Support::Unsupported(_) => EXT4_SUPPORT_UNSUPPORTED,
    };
    info
}

/// Check whether a device holds an ext2/3/4 file system.
/// Returns 0 and fills `out` if it does, `EINVAL`/`EIO` otherwise.
/// The device's `release` callback is not called.
///
/// # Safety
/// `ops` must be valid; `out` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_probe(ops: *const Ext4DeviceOps, out: *mut Ext4ProbeInfo) -> i32 {
    guard(|| {
        if ops.is_null() {
            return Err(Error::invalid("null ops"));
        }
        // SAFETY: caller contract
        let mut o = unsafe { *ops };
        o.release = None;
        o.read_only = true;
        // SAFETY: callbacks valid for the duration of this call
        let dev = AlignedDevice::new(unsafe { CallbackDevice::new(o)? });
        let sb = Fs::probe(&dev)?;
        // SAFETY: caller contract
        unsafe { put(out, fill_probe(&sb)) };
        Ok(())
    })
}

/// Mount the file system. On success `*out` receives a handle to release
/// with [`ext4_close`]. The device's `release` callback runs when the
/// handle is closed (or immediately if mounting fails).
///
/// # Safety
/// `ops` and `opts` must be valid (opts may be null); `out` valid for write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_mount(
    ops: *const Ext4DeviceOps,
    opts: *const Ext4MountOptions,
    out: *mut *mut Ext4Handle,
) -> i32 {
    guard(|| {
        if ops.is_null() || out.is_null() {
            return Err(Error::invalid("null argument"));
        }
        // SAFETY: caller contract
        let o = unsafe { *ops };
        let mo = if opts.is_null() {
            Ext4MountOptions::default()
        } else {
            // SAFETY: caller contract
            unsafe { *opts }
        };
        // SAFETY: callbacks valid until release
        let dev = AlignedDevice::new(unsafe { CallbackDevice::new(o)? });
        let mut mopts = MountOptions {
            read_only: mo.read_only,
            ..Default::default()
        };
        if mo.cache_blocks > 0 {
            mopts.cache_blocks = mo.cache_blocks as usize;
        }
        let mut fs = Fs::mount(Arc::new(dev), mopts)?;
        fs.set_defer_unlinked(mo.defer_unlinked);
        let interval = Duration::from_secs(if mo.commit_interval_secs == 0 {
            5
        } else {
            mo.commit_interval_secs as u64
        });
        let h = Box::new(Ext4Handle::new(fs, interval));
        // SAFETY: checked non-null
        unsafe { *out = Box::into_raw(h) };
        Ok(())
    })
}

/// Commit and mark the file system clean, keeping the volume open
/// read-only (for FSKit's unmount, which is followed by reclaims).
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_finish(h: *const Ext4Handle) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.finish())
}

/// Make a volume closed with [`ext4_finish`] writable again.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_remount(h: *const Ext4Handle) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.remount())
}

/// Flush everything and mark the file system clean. The handle stays
/// allocated (further calls fail with EBUSY) until [`ext4_close`].
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_unmount(h: *const Ext4Handle) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.unmount())
}

/// Release a handle (unmounting first if needed).
///
/// # Safety
/// `h` must come from [`ext4_mount`] and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_close(h: *mut Ext4Handle) {
    if !h.is_null() {
        let _ = std::panic::catch_unwind(|| {
            // SAFETY: caller contract
            drop(unsafe { Box::from_raw(h) });
        });
    }
}

/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_sync(h: *const Ext4Handle) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.with(|fs| fs.sync()))
}

/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_is_read_only(h: *const Ext4Handle) -> bool {
    // SAFETY: caller contract
    unsafe { handle(h) }.map(|h| h.is_read_only()).unwrap_or(true)
}

/// Volume label/UUID/geometry of a mounted volume.
///
/// # Safety
/// `h` must be a live handle; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_volume_info(h: *const Ext4Handle, out: *mut Ext4ProbeInfo) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let info = unsafe { handle(h) }?.with(|fs| Ok(fill_probe(fs.superblock())))?;
        // SAFETY: caller contract
        unsafe { put(out, info) };
        Ok(())
    })
}

/// # Safety
/// `h` must be a live handle; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_statfs(h: *const Ext4Handle, out: *mut Ext4StatFs) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let s = unsafe { handle(h) }?.with(|fs| Ok(fs.statfs()))?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4StatFs::from(&s)) };
        Ok(())
    })
}

/// Change the volume label (at most 16 bytes).
///
/// # Safety
/// `h` must be a live handle; `name` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_set_label(h: *const Ext4Handle, name: *const u8, len: usize) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        let s = std::str::from_utf8(n).map_err(|_| Error::invalid("label is not UTF-8"))?;
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| fs.set_label(s))
    })
}

// --- inodes ----------------------------------------------------------------------

/// # Safety
/// `h` must be a live handle; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_stat(h: *const Ext4Handle, ino: u32, out: *mut Ext4Attr) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| fs.stat(ino))?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// # Safety
/// `h` must be a live handle; `name` valid for `len` bytes; `out` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_lookup(
    h: *const Ext4Handle,
    dir: u32,
    name: *const u8,
    len: usize,
    out: *mut Ext4Attr,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| fs.lookup_attr(dir, n))?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// Directory entry callback: return false to stop. `attr` is null unless
/// attributes were requested.
pub type Ext4DirCallback = Option<
    unsafe extern "C" fn(
        ctx: *mut c_void,
        name: *const u8,
        len: usize,
        ino: u32,
        file_type: u8,
        next_cookie: u64,
        attr: *const Ext4Attr,
    ) -> bool,
>;

/// Enumerate a directory from `cookie` (0 = start). "." and ".." are
/// included unless `skip_dots`. With `want_attrs`, every entry carries its
/// attributes.
///
/// # Safety
/// `h` must be a live handle; `cb` must not call into this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_readdir(
    h: *const Ext4Handle,
    dir: u32,
    cookie: u64,
    skip_dots: bool,
    want_attrs: bool,
    cb: Ext4DirCallback,
    ctx: *mut c_void,
) -> i32 {
    guard(|| {
        let cb = cb.ok_or_else(|| Error::invalid("null callback"))?;
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| {
            let mut cookie = cookie;
            loop {
                // gather a batch, then stat outside the enumeration borrow
                let mut batch = Vec::with_capacity(256);
                fs.read_dir(dir, cookie, |e| {
                    batch.push(e);
                    batch.len() < 256
                })?;
                if batch.is_empty() {
                    return Ok(());
                }
                let full = batch.len() == 256;
                for e in batch {
                    cookie = e.next_cookie;
                    if skip_dots && (e.name == b"." || e.name == b"..") {
                        continue;
                    }
                    let attr = if want_attrs {
                        match fs.stat(e.ino) {
                            Ok(a) => Some(Ext4Attr::from(&a)),
                            Err(err) => {
                                // one damaged inode must not make the whole
                                // directory unlistable
                                if !matches!(err, Error::NotFound) {
                                    log::warn!("skipping entry with unreadable inode {}: {err}", e.ino);
                                }
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let ap = attr.as_ref().map_or(std::ptr::null(), |a| a as *const Ext4Attr);
                    // SAFETY: callback contract; pointers valid during the call
                    let more = unsafe {
                        cb(
                            ctx,
                            e.name.as_ptr(),
                            e.name.len(),
                            e.ino,
                            ft_to_u8(e.file_type),
                            e.next_cookie,
                            ap,
                        )
                    };
                    if !more {
                        return Ok(());
                    }
                }
                if !full {
                    return Ok(());
                }
            }
        })
    })
}

/// # Safety
/// `h` must be a live handle; `buf` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_read(
    h: *const Ext4Handle,
    ino: u32,
    offset: u64,
    buf: *mut u8,
    len: usize,
    nread: *mut usize,
) -> i32 {
    guard(|| {
        if len > 0 && buf.is_null() {
            return Err(Error::invalid("null buffer"));
        }
        let out: &mut [u8] = if len == 0 {
            &mut []
        } else {
            // SAFETY: caller contract
            unsafe { std::slice::from_raw_parts_mut(buf, len) }
        };
        // SAFETY: caller contract
        let n = unsafe { handle(h) }?.with(|fs| fs.read(ino, offset, out))?;
        // SAFETY: caller contract
        unsafe { put(nread, n) };
        Ok(())
    })
}

/// # Safety
/// `h` must be a live handle; `buf` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_write(
    h: *const Ext4Handle,
    ino: u32,
    offset: u64,
    buf: *const u8,
    len: usize,
    nwritten: *mut usize,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let data = unsafe { bytes(buf, len) }?;
        // SAFETY: caller contract
        let n = unsafe { handle(h) }?.with(|fs| fs.write(ino, offset, data))?;
        // SAFETY: caller contract
        unsafe { put(nwritten, n) };
        Ok(())
    })
}

/// Create a node of type `file_type` (regular, fifo, socket, char/block
/// device) or a directory (`EXT4_FT_DIR`).
///
/// # Safety
/// `h` must be a live handle; `name` valid for `len` bytes; `out` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_create(
    h: *const Ext4Handle,
    dir: u32,
    name: *const u8,
    len: usize,
    file_type: u8,
    perm: u16,
    uid: u32,
    gid: u32,
    rdev: u32,
    out: *mut Ext4Attr,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        let ft = ft_from_u8(file_type);
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| match ft {
            FileType::Directory => fs.mkdir(dir, n, perm, uid, gid),
            FileType::Symlink | FileType::Unknown => Err(Error::invalid("bad file type")),
            _ => fs.create(dir, n, ft, perm, uid, gid, rdev),
        })?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// # Safety
/// `h` must be a live handle; pointers valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_symlink(
    h: *const Ext4Handle,
    dir: u32,
    name: *const u8,
    len: usize,
    target: *const u8,
    target_len: usize,
    uid: u32,
    gid: u32,
    out: *mut Ext4Attr,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        // SAFETY: caller contract
        let t = unsafe { bytes(target, target_len) }?;
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| fs.symlink(dir, n, t, uid, gid))?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// Copy a symlink target into `buf` (`ERANGE` if `cap` is too small;
/// `*len` receives the full length either way).
///
/// # Safety
/// `h` must be a live handle; `buf` valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_readlink(
    h: *const Ext4Handle,
    ino: u32,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let t = unsafe { handle(h) }?.with(|fs| fs.read_link(ino))?;
        // SAFETY: caller contract
        unsafe { copy_out(&t, buf, cap, len) }
    })
}

/// # Safety
/// `buf` valid for `cap` bytes (or null); `len` valid for write.
unsafe fn copy_out(data: &[u8], buf: *mut u8, cap: usize, len: *mut usize) -> Result<()> {
    // SAFETY: caller contract
    unsafe { put(len, data.len()) };
    if buf.is_null() {
        return Ok(());
    }
    if cap < data.len() {
        return Err(Error::Range);
    }
    // SAFETY: caller contract, sizes checked
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len()) };
    Ok(())
}

/// # Safety
/// `h` must be a live handle; `name` valid for `len` bytes; `out` valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_link(
    h: *const Ext4Handle,
    ino: u32,
    dir: u32,
    name: *const u8,
    len: usize,
    out: *mut Ext4Attr,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| fs.link(ino, dir, n))?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// Remove a name (file or empty directory). The inode itself is released
/// by [`ext4_reclaim`] once no longer referenced.
///
/// # Safety
/// `h` must be a live handle; `name` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_remove(h: *const Ext4Handle, dir: u32, name: *const u8, len: usize) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| {
            let a = fs.lookup_attr(dir, n)?;
            if a.is_dir() {
                fs.rmdir(dir, n)
            } else {
                fs.unlink(dir, n)
            }
        })
    })
}

/// # Safety
/// `h` must be a live handle; names valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_rename(
    h: *const Ext4Handle,
    src_dir: u32,
    src_name: *const u8,
    src_len: usize,
    dst_dir: u32,
    dst_name: *const u8,
    dst_len: usize,
    flags: u32,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let s = unsafe { bytes(src_name, src_len) }?;
        // SAFETY: caller contract
        let d = unsafe { bytes(dst_name, dst_len) }?;
        let fl = RenameFlags {
            no_replace: flags & EXT4_RENAME_NOREPLACE != 0,
            exchange: flags & EXT4_RENAME_EXCHANGE != 0,
        };
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| fs.rename(src_dir, s, dst_dir, d, fl))
    })
}

/// # Safety
/// `h` must be a live handle; `attr` valid; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_setattr(
    h: *const Ext4Handle,
    ino: u32,
    attr: *const Ext4SetAttr,
    out: *mut Ext4Attr,
) -> i32 {
    guard(|| {
        if attr.is_null() {
            return Err(Error::invalid("null attr"));
        }
        // SAFETY: caller contract
        let s = unsafe { *attr };
        // SAFETY: caller contract
        let a = unsafe { handle(h) }?.with(|fs| {
            let mut req = SetAttr::default();
            if s.valid & EXT4_SET_MODE != 0 {
                req.perm = Some(s.mode & 0o7777);
            }
            if s.valid & EXT4_SET_UID != 0 {
                req.uid = Some(s.uid);
            }
            if s.valid & EXT4_SET_GID != 0 {
                req.gid = Some(s.gid);
            }
            if s.valid & EXT4_SET_ATIME != 0 {
                req.atime = Some(s.atime.into());
            }
            if s.valid & EXT4_SET_MTIME != 0 {
                req.mtime = Some(s.mtime.into());
            }
            if s.valid & EXT4_SET_CTIME != 0 {
                req.ctime = Some(s.ctime.into());
            }
            if s.valid & EXT4_SET_CRTIME != 0 {
                req.crtime = Some(s.crtime.into());
            }
            if s.valid & EXT4_SET_BSD_FLAGS != 0 {
                let cur = fs.stat(ino)?.flags;
                req.flags = Some(ext4_flags_from_bsd(cur, s.bsd_flags));
            }
            if s.valid & EXT4_SET_SIZE != 0 {
                let cur = fs.stat(ino)?;
                // FSKit: ignore size changes on directories and symlinks
                if cur.file_type == FileType::Regular {
                    req.size = Some(s.size);
                }
            }
            fs.set_attr(ino, &req)
        })?;
        // SAFETY: caller contract
        unsafe { put(out, Ext4Attr::from(&a)) };
        Ok(())
    })
}

/// The kernel dropped its last reference to `ino`.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_reclaim(h: *const Ext4Handle, ino: u32) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.with(|fs| fs.reclaim(ino)))
}

/// Preallocate `[offset, offset+len)`.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_fallocate(h: *const Ext4Handle, ino: u32, offset: u64, len: u64, keep_size: bool) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.with(|fs| fs.fallocate(ino, offset, len, keep_size)))
}

/// Deallocate `[offset, offset+len)`, keeping the size.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_punch_hole(h: *const Ext4Handle, ino: u32, offset: u64, len: u64) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.with(|fs| fs.punch_hole(ino, offset, len)))
}

/// Byte offset just past the last allocated block of a file (its
/// "physical end of file", including preallocated blocks past EOF).
///
/// # Safety
/// `h` must be a live handle; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_allocated_end(h: *const Ext4Handle, ino: u32, out: *mut u64) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let end = unsafe { handle(h) }?.with(|fs| {
            let bs = fs.block_size() as u64;
            Ok(fs.file_extents(ino)?.last().map_or(0, |e| e.end() * bs))
        })?;
        // SAFETY: caller contract
        unsafe { put(out, end) };
        Ok(())
    })
}

/// SEEK_DATA (`data = true`) / SEEK_HOLE from `offset`.
///
/// # Safety
/// `h` must be a live handle; `out` valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_seek(h: *const Ext4Handle, ino: u32, offset: u64, data: bool, out: *mut u64) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let r = unsafe { handle(h) }?.with(|fs| fs.seek_data_hole(ino, offset, data))?;
        // SAFETY: caller contract
        unsafe { put(out, r) };
        Ok(())
    })
}

/// Extent callback for [`ext4_map_for_io`]: offsets and length in bytes;
/// `zero_fill` extents have no device location. Return false to stop.
pub type Ext4ExtentCallback =
    Option<unsafe extern "C" fn(ctx: *mut c_void, logical: u64, physical: u64, length: u64, zero_fill: bool) -> bool>;

/// Map a file range for kernel offloaded I/O. For writes, missing blocks
/// are allocated as unwritten; report completion with
/// [`ext4_complete_write`].
///
/// # Safety
/// `h` must be a live handle; `cb` must not call into this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_map_for_io(
    h: *const Ext4Handle,
    ino: u32,
    offset: u64,
    len: u64,
    write: bool,
    cb: Ext4ExtentCallback,
    ctx: *mut c_void,
) -> i32 {
    guard(|| {
        let cb = cb.ok_or_else(|| Error::invalid("null callback"))?;
        // SAFETY: caller contract
        let exts = unsafe { handle(h) }?.with(|fs| fs.map_for_io(ino, offset, len, write))?;
        for e in exts {
            // SAFETY: callback contract
            if !unsafe { cb(ctx, e.logical, e.physical, e.length, e.zero_fill) } {
                break;
            }
        }
        Ok(())
    })
}

/// The kernel finished writing `[offset, offset+len)` of `ino` directly.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_complete_write(h: *const Ext4Handle, ino: u32, offset: u64, len: u64) -> i32 {
    // SAFETY: caller contract
    guard(|| unsafe { handle(h) }?.with(|fs| fs.complete_direct_write(ino, offset, len)))
}

/// The kernel reports that a direct write of `[offset, offset+len)` of
/// `ino` failed: nothing becomes visible.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_abort_write(h: *const Ext4Handle, ino: u32, offset: u64, len: u64) -> i32 {
    // SAFETY: caller contract
    guard(|| {
        unsafe { handle(h) }?.with(|fs| {
            fs.abort_direct_write(ino, offset, len);
            Ok(())
        })
    })
}

// --- extended attributes (macOS names) --------------------------------------------

/// Read xattr `name` (macOS naming). With `buf == NULL` only the size is
/// returned in `*len`.
///
/// # Safety
/// `h` must be a live handle; pointers valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_getxattr(
    h: *const Ext4Handle,
    ino: u32,
    name: *const u8,
    name_len: usize,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, name_len) }?;
        // SAFETY: caller contract
        let v = unsafe { handle(h) }?.with(|fs| fs.get_xattr(ino, &xattr_names::to_ext4(n)))?;
        // SAFETY: caller contract
        unsafe { copy_out(&v, buf, cap, len) }
    })
}

/// # Safety
/// `h` must be a live handle; pointers valid for their lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_setxattr(
    h: *const Ext4Handle,
    ino: u32,
    name: *const u8,
    name_len: usize,
    value: *const u8,
    value_len: usize,
    mode: u32,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, name_len) }?;
        // SAFETY: caller contract
        let v = unsafe { bytes(value, value_len) }?;
        let m = match mode {
            EXT4_XATTR_ANY => XattrSetMode::Any,
            EXT4_XATTR_CREATE => XattrSetMode::Create,
            EXT4_XATTR_REPLACE => XattrSetMode::Replace,
            _ => return Err(Error::Invalid("bad xattr mode".into())),
        };
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| fs.set_xattr(ino, &xattr_names::to_ext4(n), v, m))
    })
}

/// # Safety
/// `h` must be a live handle; `name` valid for `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_removexattr(h: *const Ext4Handle, ino: u32, name: *const u8, name_len: usize) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, name_len) }?;
        // SAFETY: caller contract
        unsafe { handle(h) }?.with(|fs| fs.remove_xattr(ino, &xattr_names::to_ext4(n)))
    })
}

/// List visible xattr names as NUL-terminated strings back to back.
/// With `buf == NULL` only the size is returned.
///
/// # Safety
/// `h` must be a live handle; `buf` valid for `cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_listxattr(
    h: *const Ext4Handle,
    ino: u32,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let names = unsafe { handle(h) }?.with(|fs| fs.list_xattr(ino))?;
        let mut out = Vec::new();
        for n in &names {
            if let Some(m) = xattr_names::from_ext4(n) {
                out.extend_from_slice(m);
                out.push(0);
            }
        }
        // SAFETY: caller contract
        unsafe { copy_out(&out, buf, cap, len) }
    })
}

/// Validate a name as ext4 would (for early rejection in Swift).
///
/// # Safety
/// `name` valid for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ext4_validate_name(name: *const u8, len: usize) -> i32 {
    guard(|| {
        // SAFETY: caller contract
        let n = unsafe { bytes(name, len) }?;
        if n.is_empty() || n.contains(&b'/') || n.contains(&0) {
            return Err(Error::Invalid("bad name".into()));
        }
        if n.len() > 255 {
            return Err(Error::NameTooLong);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
