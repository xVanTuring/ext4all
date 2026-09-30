//! Kernel offloaded (direct) I/O mapping: map, write the device directly
//! the way the kernel would, complete, and verify.

mod common;
use common::*;
use ext4_core::{BlockDevice, FileType, Fs, IoExtent, MemDevice, MountOptions};
use std::sync::Arc;

/// Write `data` at `offset` like the kernel: map for write, write whole
/// blocks straight to the device, then complete.
fn kernel_write(fs: &mut Fs, dev: &dyn BlockDevice, ino: u32, offset: u64, data: &[u8], complete: bool) {
    let bs = fs.block_size() as u64;
    assert_eq!(offset % bs, 0);
    let len = (data.len() as u64).div_ceil(bs) * bs;
    let mut padded = data.to_vec();
    padded.resize(len as usize, 0);
    let exts = fs.map_for_io(ino, offset, len, true).unwrap();
    let mut covered = 0;
    for e in &exts {
        assert!(!e.zero_fill);
        let s = (e.logical - offset) as usize;
        dev.write_at(e.physical, &padded[s..s + e.length as usize]).unwrap();
        covered += e.length;
    }
    assert_eq!(covered, len);
    if complete {
        fs.complete_direct_write(ino, offset, data.len() as u64).unwrap();
    }
}

/// Read `[offset, offset+len)` like the kernel from a read mapping.
fn kernel_read(fs: &mut Fs, dev: &dyn BlockDevice, ino: u32, offset: u64, len: u64) -> Vec<u8> {
    let exts: Vec<IoExtent> = fs.map_for_io(ino, offset, len, false).unwrap();
    let mut out = vec![0u8; len as usize];
    for e in exts {
        let s = e.logical.saturating_sub(offset) as usize;
        let n = (e.length as usize).min(out.len() - s);
        if !e.zero_fill {
            dev.read_at(e.physical, &mut out[s..s + n]).unwrap();
        }
    }
    out
}

fn setup(opts: &[&str]) -> (Image, Arc<MemDevice>, Fs) {
    let img = Image::new(32, opts);
    let dev = Arc::new(MemDevice::from_vec(std::fs::read(&img.path).unwrap()));
    let fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
    (img, dev, fs)
}

fn finish(img: &Image, dev: &MemDevice, fs: Fs) {
    fs.unmount().unwrap();
    std::fs::write(&img.path, dev.snapshot()).unwrap();
    img.assert_clean();
}

#[test]
fn direct_write_then_read_back() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let data = pattern(300_000, 1);
    kernel_write(&mut fs, &*dev, f, 0, &data, true);
    assert_eq!(fs.stat(f).unwrap().size, 300_000);
    let mut back = vec![0u8; 300_000];
    assert_eq!(fs.read(f, 0, &mut back).unwrap(), 300_000);
    assert_eq!(back, data);
    assert_eq!(&kernel_read(&mut fs, &*dev, f, 0, 300_000)[..], &data[..]);
    // overwrite in the middle: existing blocks are mapped, nothing new
    let before = fs.statfs().free_blocks;
    kernel_write(&mut fs, &*dev, f, 8192, &pattern(8192, 2), true);
    assert_eq!(fs.statfs().free_blocks, before);
    let mut b = vec![0u8; 8192];
    fs.read(f, 8192, &mut b).unwrap();
    assert_eq!(b, pattern(8192, 2));
    finish(&img, &dev, fs);
    let mut expect = data.clone();
    expect[8192..16384].copy_from_slice(&pattern(8192, 2));
    assert_eq!(img.debugfs_cat("/f"), expect);
}

#[test]
fn uncompleted_write_exposes_no_data() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &[1u8; 4096]).unwrap();
    // the kernel writes beyond EOF but never reports completion
    kernel_write(&mut fs, &*dev, f, 4096, &[0xEEu8; 16384], false);
    assert_eq!(fs.stat(f).unwrap().size, 4096, "size grows only on completion");
    let r = kernel_read(&mut fs, &*dev, f, 0, 20480);
    assert!(r[..4096].iter().all(|&b| b == 1));
    assert!(r[4096..].iter().all(|&b| b == 0), "unwritten blocks must read as zeros");
    // a later completion for part of it makes exactly that part visible
    fs.complete_direct_write(f, 4096, 4096).unwrap();
    let mut b = vec![0u8; 8192];
    assert_eq!(fs.read(f, 4096, &mut b).unwrap(), 4096);
    assert!(b[..4096].iter().all(|&x| x == 0xEE));
    finish(&img, &dev, fs);
}

#[test]
fn read_mapping_describes_holes_and_eof() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &[5u8; 4096]).unwrap();
    fs.write(f, 40960, &[6u8; 100]).unwrap();
    fs.fallocate(f, 16384, 8192, true).unwrap();
    let exts = fs.map_for_io(f, 0, 65536, false).unwrap();
    // data, hole, unwritten, hole, data (last block), beyond EOF
    let kinds: Vec<(u64, u64, bool)> = exts.iter().map(|e| (e.logical, e.length, e.zero_fill)).collect();
    assert_eq!(kinds.first(), Some(&(0, 4096, false)));
    assert!(kinds.contains(&(40960, 4096, false)), "{kinds:?}");
    let zero: u64 = exts.iter().filter(|e| e.zero_fill).map(|e| e.length).sum();
    assert_eq!(zero, 65536 - 8192);
    let r = kernel_read(&mut fs, &*dev, f, 0, 45056);
    let mut b = vec![0u8; 45056];
    fs.read(f, 0, &mut b).unwrap();
    assert_eq!(&r[..40960 + 100], &b[..40960 + 100]);
    finish(&img, &dev, fs);
}

#[test]
fn many_direct_writes_stay_consistent() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "1024"]);
    let files: Vec<u32> = (0..4)
        .map(|i| {
            fs.create(2, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                .unwrap()
                .ino
        })
        .collect();
    // interleaved appends fragment the files: exercises extent splits of
    // unwritten extents during completion
    for round in 0..60u64 {
        for (k, &f) in files.iter().enumerate() {
            let off = round * 3072;
            kernel_write(&mut fs, &*dev, f, off, &pattern(3072, round * 7 + k as u64), true);
        }
    }
    for (k, &f) in files.iter().enumerate() {
        fs.check_extent_tree(f).unwrap();
        let mut b = vec![0u8; 3072];
        fs.read(f, 59 * 3072, &mut b).unwrap();
        assert_eq!(b, pattern(3072, 59 * 7 + k as u64));
    }
    finish(&img, &dev, fs);
}

#[test]
fn direct_io_rejections() {
    let (img, dev, mut fs) = setup(&["-t", "ext4"]);
    let d = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    assert!(fs.map_for_io(d, 0, 4096, false).is_err());
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    assert!(fs.map_for_io(f, 0, 0, true).unwrap().is_empty());
    assert!(fs.map_for_io(f, u64::MAX - 10, 100, true).is_err());
    finish(&img, &dev, fs);
    let ro = img.mount_ro();
    let mut ro = ro;
    let f = ro.resolve("/f").unwrap();
    assert!(matches!(
        ro.map_for_io(f, 0, 4096, true),
        Err(ext4_core::Error::ReadOnly)
    ));
}
