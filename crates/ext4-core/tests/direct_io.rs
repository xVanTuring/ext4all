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

/// Write exactly `data` at `offset` like the kernel would for a sub-block
/// write: map the range, write only the requested bytes to the device.
fn kernel_write_exact(fs: &mut Fs, dev: &dyn BlockDevice, ino: u32, offset: u64, data: &[u8]) {
    kernel_start_exact(fs, dev, ino, offset, data);
    fs.complete_direct_write(ino, offset, data.len() as u64).unwrap();
}

/// Map and write exactly `data` at `offset` without reporting completion
/// (a direct write still in flight).
fn kernel_start_exact(fs: &mut Fs, dev: &dyn BlockDevice, ino: u32, offset: u64, data: &[u8]) {
    let exts = fs.map_for_io(ino, offset, data.len() as u64, true).unwrap();
    let end = offset + data.len() as u64;
    for e in &exts {
        let s = e.logical.max(offset);
        let t = (e.logical + e.length).min(end);
        if s < t {
            let src = &data[(s - offset) as usize..(t - offset) as usize];
            dev.write_at(e.physical + (s - e.logical), src).unwrap();
        }
    }
}

/// Fill most free blocks with `byte` and free them again, so later
/// allocations get blocks with stale contents.
fn dirty_free_space(fs: &mut Fs, byte: u8) {
    let junk = fs.create(2, b"junk", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let free = fs.statfs().free_blocks * fs.block_size() as u64;
    let chunk = vec![byte; 1 << 20];
    let mut off = 0;
    while off + (chunk.len() as u64) < free * 8 / 10 {
        fs.write(junk, off, &chunk).unwrap();
        off += chunk.len() as u64;
    }
    fs.sync().unwrap();
    fs.unlink(2, b"junk").unwrap();
    fs.sync().unwrap();
}

#[test]
fn partial_block_direct_writes_never_expose_stale_data() {
    for bs in ["1024", "4096"] {
        let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", bs]);
        dirty_free_space(&mut fs, 0xA5);
        let b = fs.block_size() as u64;
        // sub-block write into a hole of a new file
        let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        kernel_write_exact(&mut fs, &*dev, f, 100, &[7u8; 10]);
        let mut back = vec![0xFFu8; b as usize];
        assert_eq!(fs.read(f, 0, &mut back).unwrap(), 110);
        assert!(
            back[..100].iter().all(|&x| x == 0),
            "head of new block must read as zeros"
        );
        assert!(back[100..110].iter().all(|&x| x == 7));
        // the whole block on disk is clean beyond the data as well (it can
        // be exposed by a later extension of the file)
        fs.truncate(f, b).unwrap();
        let mut whole = vec![0xFFu8; b as usize];
        fs.read(f, 0, &mut whole).unwrap();
        assert!(
            whole[110..].iter().all(|&x| x == 0),
            "tail of new block must read as zeros"
        );

        // write straddling two new blocks in the middle of a sparse file
        let g = fs.create(2, b"g", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.truncate(g, 10 * b).unwrap();
        kernel_write_exact(&mut fs, &*dev, g, 3 * b - 5, &[9u8; 10]);
        let mut two = vec![0xFFu8; 2 * b as usize];
        fs.read(g, 2 * b, &mut two).unwrap();
        let split = b as usize - 5;
        assert!(two[..split].iter().all(|&x| x == 0));
        assert!(two[split..split + 10].iter().all(|&x| x == 9));
        assert!(two[split + 10..].iter().all(|&x| x == 0));

        // sub-block write into preallocated (unwritten) blocks whose device
        // contents are stale
        let h = fs.create(2, b"h", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.fallocate(h, 0, 4 * b, false).unwrap();
        for e in fs.map_for_io(h, 0, 4 * b, false).unwrap() {
            assert!(e.zero_fill);
        }
        for e in fs.file_extents(h).unwrap() {
            let mut junk = vec![0x5Au8; (e.len as u64 * b) as usize];
            junk[0] = 1;
            dev.write_at(e.start * b, &junk).unwrap();
        }
        kernel_write_exact(&mut fs, &*dev, h, b + 1, &[3u8; 2]);
        let mut all = vec![0xFFu8; 4 * b as usize];
        assert_eq!(fs.read(h, 0, &mut all).unwrap(), 4 * b as usize);
        for (i, &x) in all.iter().enumerate() {
            let want = if i == b as usize + 1 || i == b as usize + 2 {
                3
            } else {
                0
            };
            assert_eq!(x, want, "byte {i}");
        }
        finish(&img, &dev, fs);
    }
}

#[test]
fn completion_without_mapping_is_harmless() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &pattern(10000, 4)).unwrap();
    // FSKit may complete I/O it never mapped (operation ID unspecified):
    // written blocks stay, holes stay holes, the size grows as reported
    fs.complete_direct_write(f, 0, 10000).unwrap();
    fs.complete_direct_write(f, 40000, 100).unwrap();
    assert_eq!(fs.stat(f).unwrap().size, 40100);
    let mut b = vec![0xFFu8; 40100];
    fs.read(f, 0, &mut b).unwrap();
    assert_eq!(&b[..10000], &pattern(10000, 4)[..]);
    assert!(b[10000..].iter().all(|&x| x == 0));
    finish(&img, &dev, fs);
}

/// Two sub-block direct writes into the same new or unwritten block, the
/// second mapped before the first completes: the second mapping must not
/// zero the block again and erase the first write's data.
#[test]
fn overlapping_in_flight_writes_keep_each_others_data() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    dirty_free_space(&mut fs, 0xA5);
    // new block (hole) and preallocated (unwritten) block
    for prealloc in [false, true] {
        let name = if prealloc { &b"pre"[..] } else { &b"new"[..] };
        let f = fs.create(2, name, FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        if prealloc {
            fs.fallocate(f, 0, 4096, true).unwrap();
        }
        kernel_start_exact(&mut fs, &*dev, f, 0, &[1u8; 100]);
        kernel_start_exact(&mut fs, &*dev, f, 200, &[2u8; 100]);
        fs.complete_direct_write(f, 0, 100).unwrap();
        fs.complete_direct_write(f, 200, 100).unwrap();
        let mut b = vec![0xFFu8; 300];
        assert_eq!(fs.read(f, 0, &mut b).unwrap(), 300);
        assert!(
            b[..100].iter().all(|&x| x == 1),
            "prealloc {prealloc}: first write lost"
        );
        assert!(b[100..200].iter().all(|&x| x == 0), "prealloc {prealloc}");
        assert!(b[200..].iter().all(|&x| x == 2), "prealloc {prealloc}");
        // nothing stays tracked after completion: a later sub-block write
        // into fresh blocks is zeroed again
        kernel_write_exact(&mut fs, &*dev, f, 3 * 4096 + 10, &[3u8; 5]);
        let mut t = vec![0xFFu8; 4096];
        fs.read(f, 3 * 4096, &mut t).unwrap();
        assert!(t[..10].iter().all(|&x| x == 0) && t[10..15].iter().all(|&x| x == 3));
    }
    finish(&img, &dev, fs);
}

/// Extending a file past a partial last block: bytes between the old end
/// of file and the write read as zeros even if a failed earlier write left
/// data on the device there, unless a write still in flight covers them.
#[test]
fn failed_extending_writes_leave_zeros_past_eof() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &[7u8; 100]).unwrap();
    // a direct write past EOF fails after reaching the device
    kernel_start_exact(&mut fs, &*dev, f, 100, &[0xEEu8; 3996]);
    fs.abort_direct_write(f, 100, 3996).unwrap();
    // a later write in the same block, beyond EOF
    kernel_write_exact(&mut fs, &*dev, f, 1000, &[8u8; 10]);
    let mut b = vec![0xFFu8; 1010];
    fs.read(f, 0, &mut b).unwrap();
    assert!(b[..100].iter().all(|&x| x == 7));
    assert!(b[100..1000].iter().all(|&x| x == 0), "gap in the same block");
    assert!(b[1000..].iter().all(|&x| x == 8));
    // a failed write spilling into new blocks, then a write further on
    kernel_start_exact(&mut fs, &*dev, f, 1010, &[0xEEu8; 3086 + 4096]);
    fs.abort_direct_write(f, 1010, 3086 + 4096).unwrap();
    kernel_write_exact(&mut fs, &*dev, f, 3 * 4096, &[9u8; 4096]);
    let mut t = vec![0xFFu8; 3 * 4096 - 1010];
    fs.read(f, 1010, &mut t).unwrap();
    assert!(t.iter().all(|&x| x == 0), "old tail and the never written blocks");
    finish(&img, &dev, fs);
}

/// macOS writes through mappings it obtained earlier and asks only for the
/// blocks it has no mapping for: mapping that new block, beyond the end of
/// file, must not touch the last block the kernel may be filling (seen as
/// zeros past the old end of file after an uncached write).
#[test]
fn extending_mapping_keeps_tail_written_through_earlier_mappings() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let mut want = pattern(5000, 1);
    kernel_write_exact(&mut fs, &*dev, f, 0, &want);
    fs.truncate(f, 10324).unwrap();
    want.resize(10324, 0);
    // a memory mapped write pushes the third block
    let tail = pattern(2132, 2);
    kernel_write_exact(&mut fs, &*dev, f, 8192, &tail);
    want[8192..].copy_from_slice(&tail);
    // one write of 1347..13347: the first three blocks through the
    // mappings already known, then a mapping for the fourth only
    let data = pattern(12000, 3);
    let pblk = fs.file_extents(f).unwrap();
    for (lblk, chunk) in [(0u64, 1347usize..4096), (1, 4096..8192), (2, 8192..12288)] {
        let e = pblk
            .iter()
            .find(|e| (e.block as u64) <= lblk && lblk < (e.block + e.len) as u64)
            .unwrap();
        let phys = (e.start + lblk - e.block as u64) * 4096;
        let src = &data[chunk.start - 1347..chunk.end - 1347];
        dev.write_at(phys + (chunk.start as u64 % 4096), src).unwrap();
    }
    kernel_start_exact(&mut fs, &*dev, f, 12288, &data[12288 - 1347..]);
    fs.complete_direct_write(f, 0, 13347).unwrap();
    want.resize(13347, 0);
    want[1347..].copy_from_slice(&data);
    let mut back = vec![0u8; 13347];
    assert_eq!(fs.read(f, 0, &mut back).unwrap(), 13347);
    assert!(
        back == want,
        "first difference at {:?}",
        back.iter().zip(&want).position(|(a, b)| a != b)
    );

    // an in-flight write filling the tail is not wiped by a later
    // extending write mapped before it completes
    let g = fs.create(2, b"g", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(g, 0, &[1u8; 100]).unwrap();
    kernel_start_exact(&mut fs, &*dev, g, 100, &[2u8; 100]);
    kernel_start_exact(&mut fs, &*dev, g, 8192, &[3u8; 100]);
    fs.complete_direct_write(g, 8192, 100).unwrap();
    fs.complete_direct_write(g, 100, 100).unwrap();
    let mut c = vec![0xFFu8; 200];
    fs.read(g, 0, &mut c).unwrap();
    assert!(c[..100].iter().all(|&x| x == 1));
    assert!(c[100..].iter().all(|&x| x == 2), "in-flight data erased");
    finish(&img, &dev, fs);
}

/// A failed direct write makes nothing visible and is no longer tracked.
#[test]
fn aborted_direct_write_stays_invisible() {
    let (img, dev, mut fs) = setup(&["-t", "ext4", "-b", "4096"]);
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &[1u8; 4096]).unwrap();
    kernel_start_exact(&mut fs, &*dev, f, 4096, &[0xEEu8; 8192]);
    fs.abort_direct_write(f, 4096, 8192).unwrap();
    assert_eq!(fs.stat(f).unwrap().size, 4096);
    let r = kernel_read(&mut fs, &*dev, f, 0, 12288);
    assert!(r[4096..].iter().all(|&x| x == 0));
    // the size can still grow over the unwritten blocks: zeros
    fs.truncate(f, 12288).unwrap();
    let mut b = vec![0xFFu8; 8192];
    fs.read(f, 4096, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0));
    finish(&img, &dev, fs);
}
