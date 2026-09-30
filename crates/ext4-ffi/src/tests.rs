use super::*;
use ext4_core::{BlockDevice, FileDevice};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Callback context: the device plus a per-device release counter.
struct Ctx {
    dev: FileDevice,
    releases: Arc<AtomicUsize>,
}

fn sbin(tool: &str) -> String {
    let d = std::env::var("E2FSPROGS_SBIN").unwrap_or_else(|_| "/opt/homebrew/opt/e2fsprogs/sbin".into());
    format!("{d}/{tool}")
}

fn mkfs(dir: &std::path::Path, opts: &[&str]) -> std::path::PathBuf {
    let p = dir.join("fs.img");
    std::fs::File::create(&p).unwrap().set_len(32 << 20).unwrap();
    let out = Command::new(sbin("mke2fs"))
        .args(["-F", "-q"])
        .args(opts)
        .arg(&p)
        .output()
        .unwrap();
    assert!(out.status.success());
    p
}

fn fsck_clean(p: &std::path::Path) {
    let out = Command::new(sbin("e2fsck")).arg("-fn").arg(p).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{s}");
}

unsafe extern "C" fn cb_read(ctx: *mut c_void, off: u64, buf: *mut u8, len: usize) -> i32 {
    let c = unsafe { &*(ctx as *const Ctx) };
    assert_eq!(off % 512, 0, "unaligned read offset");
    assert_eq!(len % 512, 0, "unaligned read length");
    let b = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    match c.dev.read_at(off, b) {
        Ok(()) => 0,
        Err(e) => e.errno(),
    }
}

unsafe extern "C" fn cb_write(ctx: *mut c_void, off: u64, buf: *const u8, len: usize) -> i32 {
    let c = unsafe { &*(ctx as *const Ctx) };
    assert_eq!(off % 512, 0, "unaligned write offset");
    assert_eq!(len % 512, 0, "unaligned write length");
    let b = unsafe { std::slice::from_raw_parts(buf, len) };
    match c.dev.write_at(off, b) {
        Ok(()) => 0,
        Err(e) => e.errno(),
    }
}

unsafe extern "C" fn cb_flush(ctx: *mut c_void) -> i32 {
    let c = unsafe { &*(ctx as *const Ctx) };
    match c.dev.flush() {
        Ok(()) => 0,
        Err(e) => e.errno(),
    }
}

unsafe extern "C" fn cb_release(ctx: *mut c_void) {
    let c = unsafe { Box::from_raw(ctx as *mut Ctx) };
    c.releases.fetch_add(1, Ordering::SeqCst);
}

fn ops(p: &std::path::Path, read_only: bool, sector: u32) -> Ext4DeviceOps {
    ops_counted(p, read_only, sector, Arc::new(AtomicUsize::new(0)))
}

/// Device ops over an image; `sector` exercises alignment.
fn ops_counted(p: &std::path::Path, read_only: bool, sector: u32, releases: Arc<AtomicUsize>) -> Ext4DeviceOps {
    let dev = FileDevice::open(p, read_only).unwrap();
    let size = dev.size();
    let ctx = Box::new(Ctx { dev, releases });
    Ext4DeviceOps {
        ctx: Box::into_raw(ctx) as *mut c_void,
        read: Some(cb_read),
        write: Some(cb_write),
        flush: Some(cb_flush),
        release: Some(cb_release),
        size,
        sector_size: sector,
        read_only,
    }
}

fn mount(o: &Ext4DeviceOps, opts: Option<Ext4MountOptions>) -> *mut Ext4Handle {
    let mut h: *mut Ext4Handle = std::ptr::null_mut();
    let op = opts.unwrap_or_default();
    let rc = unsafe { ext4_mount(o, &op, &mut h) };
    assert_eq!(rc, 0);
    assert!(!h.is_null());
    h
}

fn create(h: *mut Ext4Handle, dir: u32, name: &str, ft: u8) -> Ext4Attr {
    let mut a = Ext4Attr::default();
    let rc = unsafe { ext4_create(h, dir, name.as_ptr(), name.len(), ft, 0o644, 501, 20, 0, &mut a) };
    assert_eq!(rc, 0, "create {name}");
    a
}

fn lookup(h: *mut Ext4Handle, dir: u32, name: &str) -> Result<Ext4Attr> {
    let mut a = Ext4Attr::default();
    match unsafe { ext4_lookup(h, dir, name.as_ptr(), name.len(), &mut a) } {
        0 => Ok(a),
        e => Err(Error::Device(e)),
    }
}

#[test]
fn version_is_nonempty() {
    let v = unsafe { std::ffi::CStr::from_ptr(ext4_version()) };
    assert!(!v.to_bytes().is_empty());
}

#[test]
fn probe_reports_support_and_label() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4", "-L", "hello"]);
    let o = ops(&p, true, 4096);
    let mut info = Ext4ProbeInfo::default();
    assert_eq!(unsafe { ext4_probe(&o, &mut info) }, 0);
    assert_eq!(&info.label[..6], b"hello\0");
    assert_eq!(info.support, EXT4_SUPPORT_READ_WRITE);
    assert!(!info.needs_recovery);
    // probe must not release the device
    unsafe { cb_release(o.ctx) };
    let d3 = tempfile::tempdir().unwrap();
    let p3 = mkfs(d3.path(), &["-t", "ext3"]);
    let o = ops(&p3, true, 512);
    assert_eq!(unsafe { ext4_probe(&o, &mut info) }, 0);
    assert_eq!(info.support, EXT4_SUPPORT_READ_WRITE);
    unsafe { cb_release(o.ctx) };
    let d4 = tempfile::tempdir().unwrap();
    let p4 = mkfs(d4.path(), &["-t", "ext4", "-O", "bigalloc", "-C", "16384"]);
    let o = ops(&p4, true, 512);
    assert_eq!(unsafe { ext4_probe(&o, &mut info) }, 0);
    assert_eq!(info.support, EXT4_SUPPORT_READ_ONLY);
    unsafe { cb_release(o.ctx) };
}

#[test]
fn probe_rejects_non_ext4() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("zero");
    std::fs::write(&p, vec![0u8; 1 << 20]).unwrap();
    let o = ops(&p, true, 512);
    let mut info = Ext4ProbeInfo::default();
    assert_ne!(unsafe { ext4_probe(&o, &mut info) }, 0);
    unsafe { cb_release(o.ctx) };
    assert_eq!(
        unsafe { ext4_probe(std::ptr::null(), &mut info) },
        ext4_core::error::errno::EINVAL
    );
}

#[test]
fn full_lifecycle_through_c_abi() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4", "-b", "4096"]);
    let releases = Arc::new(AtomicUsize::new(0));
    let o = ops_counted(&p, false, 4096, releases.clone());
    let h = mount(&o, None);
    assert!(!unsafe { ext4_is_read_only(h) });

    // files and data
    let f = create(h, 2, "file.txt", EXT4_FT_REG);
    assert_eq!(f.mode & 0o170000, 0o100000);
    assert_eq!(f.uid, 501);
    let data = b"hello through the C ABI".repeat(1000);
    let mut n = 0usize;
    assert_eq!(
        unsafe { ext4_write(h, f.ino, 10, data.as_ptr(), data.len(), &mut n) },
        0
    );
    assert_eq!(n, data.len());
    let mut back = vec![0u8; data.len() + 100];
    assert_eq!(
        unsafe { ext4_read(h, f.ino, 10, back.as_mut_ptr(), back.len(), &mut n) },
        0
    );
    assert_eq!(&back[..n], &data[..]);
    let mut a = Ext4Attr::default();
    assert_eq!(unsafe { ext4_stat(h, f.ino, &mut a) }, 0);
    assert_eq!(a.size, 10 + data.len() as u64);

    // directories and enumeration
    let dd = create(h, 2, "dir", EXT4_FT_DIR);
    assert_eq!(dd.nlink, 2);
    for i in 0..50 {
        create(h, dd.ino, &format!("child-{i}"), EXT4_FT_REG);
    }
    struct Acc {
        names: Vec<String>,
        attrs: usize,
        stop_at: usize,
    }
    unsafe extern "C" fn collect(
        ctx: *mut c_void,
        name: *const u8,
        len: usize,
        _ino: u32,
        _ft: u8,
        _next: u64,
        attr: *const Ext4Attr,
    ) -> bool {
        let acc = unsafe { &mut *(ctx as *mut Acc) };
        let n = unsafe { std::slice::from_raw_parts(name, len) };
        acc.names.push(String::from_utf8_lossy(n).into_owned());
        if !attr.is_null() {
            acc.attrs += 1;
        }
        acc.names.len() < acc.stop_at
    }
    let mut acc = Acc {
        names: vec![],
        attrs: 0,
        stop_at: usize::MAX,
    };
    let rc = unsafe {
        ext4_readdir(
            h,
            dd.ino,
            0,
            false,
            false,
            Some(collect),
            &mut acc as *mut Acc as *mut c_void,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(acc.names.len(), 52);
    assert!(acc.names.contains(&".".to_string()) && acc.names.contains(&"..".to_string()));
    assert_eq!(acc.attrs, 0);
    let mut acc2 = Acc {
        names: vec![],
        attrs: 0,
        stop_at: usize::MAX,
    };
    let rc = unsafe {
        ext4_readdir(
            h,
            dd.ino,
            0,
            true,
            true,
            Some(collect),
            &mut acc2 as *mut Acc as *mut c_void,
        )
    };
    assert_eq!(rc, 0);
    assert_eq!(acc2.names.len(), 50);
    assert_eq!(acc2.attrs, 50);
    let mut acc3 = Acc {
        names: vec![],
        attrs: 0,
        stop_at: 7,
    };
    unsafe {
        ext4_readdir(
            h,
            dd.ino,
            0,
            true,
            false,
            Some(collect),
            &mut acc3 as *mut Acc as *mut c_void,
        )
    };
    assert_eq!(acc3.names.len(), 7);

    // xattrs with macOS names
    let name = "com.apple.FinderInfo";
    let val = [7u8; 32];
    assert_eq!(
        unsafe { ext4_setxattr(h, f.ino, name.as_ptr(), name.len(), val.as_ptr(), 32, EXT4_XATTR_ANY) },
        0
    );
    let mut len = 0usize;
    assert_eq!(
        unsafe { ext4_getxattr(h, f.ino, name.as_ptr(), name.len(), std::ptr::null_mut(), 0, &mut len) },
        0
    );
    assert_eq!(len, 32);
    let mut small = [0u8; 4];
    assert_eq!(
        unsafe { ext4_getxattr(h, f.ino, name.as_ptr(), name.len(), small.as_mut_ptr(), 4, &mut len) },
        ext4_core::error::errno::ERANGE
    );
    let mut got = [0u8; 64];
    assert_eq!(
        unsafe { ext4_getxattr(h, f.ino, name.as_ptr(), name.len(), got.as_mut_ptr(), 64, &mut len) },
        0
    );
    assert_eq!(&got[..32], &val);
    assert_eq!(
        unsafe { ext4_setxattr(h, f.ino, name.as_ptr(), name.len(), val.as_ptr(), 32, EXT4_XATTR_CREATE) },
        ext4_core::error::errno::EEXIST
    );
    let mut list = [0u8; 256];
    assert_eq!(unsafe { ext4_listxattr(h, f.ino, list.as_mut_ptr(), 256, &mut len) }, 0);
    assert_eq!(&list[..len], b"com.apple.FinderInfo\0");
    assert_eq!(unsafe { ext4_removexattr(h, f.ino, name.as_ptr(), name.len()) }, 0);
    assert_eq!(
        unsafe { ext4_getxattr(h, f.ino, name.as_ptr(), name.len(), got.as_mut_ptr(), 64, &mut len) },
        ext4_core::error::errno::ENOATTR
    );

    // symlinks
    let t = "file.txt";
    let mut sa = Ext4Attr::default();
    assert_eq!(
        unsafe { ext4_symlink(h, 2, b"sl".as_ptr(), 2, t.as_ptr(), t.len(), 0, 0, &mut sa) },
        0
    );
    assert_eq!(sa.file_type, EXT4_FT_LNK);
    let mut lb = [0u8; 64];
    assert_eq!(unsafe { ext4_readlink(h, sa.ino, lb.as_mut_ptr(), 64, &mut len) }, 0);
    assert_eq!(&lb[..len], t.as_bytes());

    // hard link, rename, remove + reclaim
    let mut la = Ext4Attr::default();
    assert_eq!(unsafe { ext4_link(h, f.ino, dd.ino, b"hl".as_ptr(), 2, &mut la) }, 0);
    assert_eq!(la.nlink, 2);
    assert_eq!(
        unsafe { ext4_rename(h, 2, b"file.txt".as_ptr(), 8, 2, b"renamed".as_ptr(), 7, 0) },
        0
    );
    assert!(lookup(h, 2, "file.txt").is_err());
    assert_eq!(lookup(h, 2, "renamed").unwrap().ino, f.ino);
    assert_eq!(
        unsafe {
            ext4_rename(
                h,
                2,
                b"renamed".as_ptr(),
                7,
                2,
                b"sl".as_ptr(),
                2,
                EXT4_RENAME_NOREPLACE,
            )
        },
        ext4_core::error::errno::EEXIST
    );
    assert_eq!(unsafe { ext4_remove(h, 2, b"renamed".as_ptr(), 7) }, 0);
    assert_eq!(unsafe { ext4_remove(h, dd.ino, b"hl".as_ptr(), 2) }, 0);
    // still readable until reclaimed
    assert_eq!(unsafe { ext4_read(h, f.ino, 10, back.as_mut_ptr(), 5, &mut n) }, 0);
    assert_eq!(&back[..5], b"hello");
    assert_eq!(unsafe { ext4_reclaim(h, f.ino) }, 0);
    assert_ne!(unsafe { ext4_stat(h, f.ino, &mut a) }, 0);
    // removing a non-empty dir fails, empty dir succeeds
    assert_eq!(
        unsafe { ext4_remove(h, 2, b"dir".as_ptr(), 3) },
        ext4_core::error::errno::ENOTEMPTY
    );

    // setattr: mode, owner, size, times, flags
    let g = create(h, 2, "g", EXT4_FT_REG);
    let req = Ext4SetAttr {
        valid: EXT4_SET_MODE | EXT4_SET_UID | EXT4_SET_SIZE | EXT4_SET_MTIME | EXT4_SET_BSD_FLAGS,
        mode: 0o600,
        uid: 1234,
        size: 5000,
        mtime: Ext4Time { sec: 1000, nsec: 5 },
        bsd_flags: UF_NODUMP,
        ..Default::default()
    };
    let mut ga = Ext4Attr::default();
    assert_eq!(unsafe { ext4_setattr(h, g.ino, &req, &mut ga) }, 0);
    assert_eq!(ga.mode & 0o7777, 0o600);
    assert_eq!(ga.uid, 1234);
    assert_eq!(ga.size, 5000);
    assert_eq!(ga.mtime, Ext4Time { sec: 1000, nsec: 5 });
    assert_eq!(ga.bsd_flags, UF_NODUMP);
    // size on a directory is ignored
    let req = Ext4SetAttr {
        valid: EXT4_SET_SIZE,
        size: 1,
        ..Default::default()
    };
    assert_eq!(unsafe { ext4_setattr(h, dd.ino, &req, &mut ga) }, 0);

    // preallocation and seek
    assert_eq!(unsafe { ext4_fallocate(h, g.ino, 0, 1 << 20, false) }, 0);
    let mut off = 0u64;
    assert_eq!(
        unsafe { ext4_seek(h, g.ino, 0, true, &mut off) },
        ext4_core::error::errno::ENXIO
    );
    assert_eq!(unsafe { ext4_write(h, g.ino, 8192, b"x".as_ptr(), 1, &mut n) }, 0);
    assert_eq!(unsafe { ext4_seek(h, g.ino, 0, true, &mut off) }, 0);
    assert_eq!(off, 8192);
    assert_eq!(unsafe { ext4_seek(h, g.ino, 8192, false, &mut off) }, 0);
    assert_eq!(off, 12288);
    assert_eq!(unsafe { ext4_punch_hole(h, g.ino, 8192, 4096) }, 0);

    // statfs, label, volume info
    let mut s = Ext4StatFs::default();
    assert_eq!(unsafe { ext4_statfs(h, &mut s) }, 0);
    assert_eq!(s.block_size, 4096);
    assert!(s.free_blocks > 0 && s.free_blocks <= s.blocks);
    assert_eq!(unsafe { ext4_set_label(h, b"newlabel".as_ptr(), 8) }, 0);
    let mut vi = Ext4ProbeInfo::default();
    assert_eq!(unsafe { ext4_volume_info(h, &mut vi) }, 0);
    assert_eq!(&vi.label[..9], b"newlabel\0");
    assert_eq!(
        unsafe { ext4_set_label(h, b"this-label-is-too-long".as_ptr(), 22) },
        ext4_core::error::errno::ENAMETOOLONG
    );

    assert_eq!(unsafe { ext4_sync(h) }, 0);
    assert_eq!(unsafe { ext4_unmount(h) }, 0);
    // after unmount, operations fail but the handle is still valid
    assert_eq!(unsafe { ext4_stat(h, 2, &mut a) }, ext4_core::error::errno::EBUSY);
    // unmount already released the device; close must not release twice
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    unsafe { ext4_close(h) };
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    fsck_clean(&p);
}

#[test]
fn close_without_unmount_still_cleans_up() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4"]);
    let o = ops(&p, false, 512);
    let h = mount(&o, None);
    create(h, 2, "a", EXT4_FT_REG);
    let x = create(h, 2, "deleted-open", EXT4_FT_REG);
    unsafe { ext4_write(h, x.ino, 0, [1u8; 9000].as_ptr(), 9000, std::ptr::null_mut()) };
    unsafe { ext4_remove(h, 2, b"deleted-open".as_ptr(), 12) };
    unsafe { ext4_close(h) };
    fsck_clean(&p);
}

#[test]
fn periodic_commit_thread_flushes() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4"]);
    let o = ops(&p, false, 512);
    let h = mount(
        &o,
        Some(Ext4MountOptions {
            commit_interval_secs: 1,
            ..Default::default()
        }),
    );
    create(h, 2, "committed-by-timer", EXT4_FT_REG);
    std::thread::sleep(Duration::from_millis(2500));
    // the entry must be on disk even though we never synced: check with a
    // read-only mount of the raw image (journal replay not needed)
    let out = Command::new(sbin("debugfs"))
        .arg("-R")
        .arg("ls -l /")
        .arg(&p)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("committed-by-timer"));
    unsafe { ext4_close(h) };
    fsck_clean(&p);
}

#[test]
fn read_only_mount_via_ffi() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4"]);
    let o = ops(&p, true, 512);
    let h = mount(
        &o,
        Some(Ext4MountOptions {
            read_only: true,
            ..Default::default()
        }),
    );
    assert!(unsafe { ext4_is_read_only(h) });
    let mut a = Ext4Attr::default();
    assert_eq!(
        unsafe { ext4_create(h, 2, b"x".as_ptr(), 1, EXT4_FT_REG, 0o644, 0, 0, 0, &mut a) },
        ext4_core::error::errno::EROFS
    );
    assert_eq!(unsafe { ext4_stat(h, 2, &mut a) }, 0);
    assert_eq!(a.file_type, EXT4_FT_DIR);
    unsafe { ext4_close(h) };
}

#[test]
fn null_arguments_are_rejected() {
    let mut a = Ext4Attr::default();
    assert_eq!(
        unsafe { ext4_stat(std::ptr::null(), 2, &mut a) },
        ext4_core::error::errno::EINVAL
    );
    assert_eq!(
        unsafe { ext4_mount(std::ptr::null(), std::ptr::null(), std::ptr::null_mut()) },
        ext4_core::error::errno::EINVAL
    );
    unsafe { ext4_close(std::ptr::null_mut()) };
    assert_eq!(unsafe { ext4_validate_name(b"ok".as_ptr(), 2) }, 0);
    assert_eq!(
        unsafe { ext4_validate_name(b"a/b".as_ptr(), 3) },
        ext4_core::error::errno::EINVAL
    );
    assert_eq!(
        unsafe { ext4_validate_name([b'x'; 256].as_ptr(), 256) },
        ext4_core::error::errno::ENAMETOOLONG
    );
    assert_eq!(
        unsafe { ext4_validate_name(std::ptr::null(), 0) },
        ext4_core::error::errno::EINVAL
    );
}

#[test]
fn concurrent_operations_are_serialized() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4", "-b", "4096"]);
    let o = ops(&p, false, 4096);
    let h = mount(&o, None) as usize;
    let threads: Vec<_> = (0..8)
        .map(|t| {
            std::thread::spawn(move || {
                let h = h as *mut Ext4Handle;
                for i in 0..40 {
                    let name = format!("t{t}-f{i}");
                    let a = create(h, 2, &name, EXT4_FT_REG);
                    let data = vec![t as u8; 3000 + i * 10];
                    let mut n = 0;
                    assert_eq!(unsafe { ext4_write(h, a.ino, 0, data.as_ptr(), data.len(), &mut n) }, 0);
                    let mut back = vec![0u8; data.len()];
                    assert_eq!(
                        unsafe { ext4_read(h, a.ino, 0, back.as_mut_ptr(), back.len(), &mut n) },
                        0
                    );
                    assert_eq!(back, data);
                    if i % 3 == 0 {
                        assert_eq!(unsafe { ext4_remove(h, 2, name.as_ptr(), name.len()) }, 0);
                        assert_eq!(unsafe { ext4_reclaim(h, a.ino) }, 0);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    unsafe { ext4_close(h as *mut Ext4Handle) };
    fsck_clean(&p);
}

#[test]
fn direct_io_mapping_through_c_abi() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4", "-b", "4096"]);
    let o = ops(&p, false, 4096);
    let h = mount(&o, None);
    let f = create(h, 2, "koio", EXT4_FT_REG);
    let mut exts: Vec<(u64, u64, u64, bool)> = Vec::new();
    unsafe extern "C" fn collect(ctx: *mut c_void, l: u64, p: u64, n: u64, z: bool) -> bool {
        let v = unsafe { &mut *(ctx as *mut Vec<(u64, u64, u64, bool)>) };
        v.push((l, p, n, z));
        true
    }
    let rc = unsafe {
        ext4_map_for_io(
            h,
            f.ino,
            0,
            65536,
            true,
            Some(collect),
            &mut exts as *mut _ as *mut c_void,
        )
    };
    assert_eq!(rc, 0);
    let total: u64 = exts.iter().map(|e| e.2).sum();
    assert_eq!(total, 65536);
    assert!(exts.iter().all(|e| !e.3));
    // write the data "as the kernel" through a second handle on the file
    {
        use std::os::unix::fs::FileExt;
        let file = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        for e in &exts {
            file.write_all_at(&vec![0xABu8; e.2 as usize], e.1).unwrap();
        }
    }
    // not completed yet: reads return zeros
    let mut buf = vec![1u8; 4096];
    let mut n = 0;
    assert_eq!(unsafe { ext4_read(h, f.ino, 0, buf.as_mut_ptr(), 4096, &mut n) }, 0);
    assert_eq!(n, 0, "size is still 0");
    assert_eq!(unsafe { ext4_complete_write(h, f.ino, 0, 65536) }, 0);
    assert_eq!(unsafe { ext4_read(h, f.ino, 0, buf.as_mut_ptr(), 4096, &mut n) }, 0);
    assert_eq!(n, 4096);
    assert!(buf.iter().all(|&b| b == 0xAB));
    let mut rd: Vec<(u64, u64, u64, bool)> = Vec::new();
    unsafe {
        ext4_map_for_io(
            h,
            f.ino,
            0,
            131072,
            false,
            Some(collect),
            &mut rd as *mut _ as *mut c_void,
        )
    };
    assert_eq!(rd.iter().filter(|e| !e.3).map(|e| e.2).sum::<u64>(), 65536);
    assert_eq!(rd.iter().filter(|e| e.3).map(|e| e.2).sum::<u64>(), 65536);
    unsafe { ext4_close(h) };
    fsck_clean(&p);
}

#[test]
fn finish_keeps_volume_usable_for_reclaims() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4"]);
    let o = ops(&p, false, 512);
    let h = mount(&o, None);
    let f = create(h, 2, "gone", EXT4_FT_REG);
    unsafe { ext4_write(h, f.ino, 0, [3u8; 5000].as_ptr(), 5000, std::ptr::null_mut()) };
    assert_eq!(unsafe { ext4_remove(h, 2, b"gone".as_ptr(), 4) }, 0);
    assert_eq!(unsafe { ext4_finish(h) }, 0);
    // FSKit reclaims items after unmount: must succeed, not EBUSY
    assert_eq!(unsafe { ext4_reclaim(h, f.ino) }, 0);
    assert_eq!(unsafe { ext4_sync(h) }, 0);
    let mut a = Ext4Attr::default();
    assert_eq!(unsafe { ext4_stat(h, 2, &mut a) }, 0);
    assert!(unsafe { ext4_is_read_only(h) }, "finished volumes are read-only");
    // the image is clean right after finish
    fsck_clean(&p);
    // mount again without re-activation
    assert_eq!(unsafe { ext4_remount(h) }, 0);
    create(h, 2, "again", EXT4_FT_REG);
    unsafe { ext4_close(h) };
    fsck_clean(&p);
    let o = ops(&p, true, 512);
    let mut info = Ext4ProbeInfo::default();
    assert_eq!(unsafe { ext4_probe(&o, &mut info) }, 0);
    assert!(info.has_journal);
    assert_eq!(info.subtype, 2);
    unsafe { cb_release(o.ctx) };
}

#[test]
fn probe_subtypes() {
    for (opts, sub, journal) in [
        (&["-t", "ext2"][..], 0u8, false),
        (&["-t", "ext3"][..], 1, true),
        (&["-t", "ext4", "-O", "^has_journal"][..], 2, false),
    ] {
        let d = tempfile::tempdir().unwrap();
        let p = mkfs(d.path(), opts);
        let o = ops(&p, true, 512);
        let mut info = Ext4ProbeInfo::default();
        assert_eq!(unsafe { ext4_probe(&o, &mut info) }, 0);
        assert_eq!((info.subtype, info.has_journal), (sub, journal), "{opts:?}");
        unsafe { cb_release(o.ctx) };
    }
}

#[test]
fn allocated_end_reports_physical_eof() {
    let d = tempfile::tempdir().unwrap();
    let p = mkfs(d.path(), &["-t", "ext4", "-b", "4096"]);
    let o = ops(&p, false, 4096);
    let h = mount(&o, None);
    let f = create(h, 2, "prealloc", EXT4_FT_REG);
    let mut end = u64::MAX;
    assert_eq!(unsafe { ext4_allocated_end(h, f.ino, &mut end) }, 0);
    assert_eq!(end, 0, "empty file");
    let data = [1u8; 100];
    let mut n = 0;
    assert_eq!(unsafe { ext4_write(h, f.ino, 0, data.as_ptr(), data.len(), &mut n) }, 0);
    assert_eq!(unsafe { ext4_allocated_end(h, f.ino, &mut end) }, 0);
    assert_eq!(end, 4096);
    // preallocation past EOF keeps the size but moves the physical end
    assert_eq!(unsafe { ext4_fallocate(h, f.ino, 4096, 3 * 4096, true) }, 0);
    assert_eq!(unsafe { ext4_allocated_end(h, f.ino, &mut end) }, 0);
    assert_eq!(end, 4 * 4096);
    let mut a = Ext4Attr::default();
    assert_eq!(unsafe { ext4_stat(h, f.ino, &mut a) }, 0);
    assert_eq!(a.size, 100);
    assert_ne!(a.flags & EXT4_FL_EXTENTS, 0);
    assert_eq!(a.flags & EXT4_FL_INLINE_DATA, 0);
    assert_ne!(unsafe { ext4_allocated_end(h, 999_999, &mut end) }, 0);
    // like the other queries, a null output pointer is simply not written
    assert_eq!(unsafe { ext4_allocated_end(h, f.ino, std::ptr::null_mut()) }, 0);
    assert_eq!(unsafe { ext4_unmount(h) }, 0);
    unsafe { ext4_close(h) };
    fsck_clean(&p);
}
