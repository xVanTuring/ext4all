//! `SharedFs::write_parallel`: data written outside the file system lock,
//! checked against a model, in arrival order where writes overlap, raced
//! against reads, truncation and deletion, and after failed device writes.

mod common;
use common::*;
use ext4_core::{Error, FileType, Fs, MemDevice, MountOptions, Result, SharedFs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn write(shared: &SharedFs, ino: u32, offset: u64, data: &[u8]) -> Result<usize> {
    let ticket = shared.reserve_write(ino, offset, data.len());
    shared.write_parallel(ticket, ino, offset, data)
}

fn create(shared: &SharedFs, name: &str) -> u32 {
    shared
        .with(|fs| Ok(fs.create(2, name.as_bytes(), FileType::Regular, 0o644, 0, 0, 0)?.ino))
        .unwrap()
}

fn contents(shared: &SharedFs, ino: u32) -> Vec<u8> {
    shared
        .with(|fs| {
            let mut b = vec![0u8; fs.stat(ino)?.size as usize];
            let n = fs.read(ino, 0, &mut b)?;
            b.truncate(n);
            Ok(b)
        })
        .unwrap()
}

fn apply(model: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let (o, end) = (offset as usize, offset as usize + data.len());
    if model.len() < end {
        model.resize(end, 0);
    }
    model[o..end].copy_from_slice(data);
}

#[test]
fn parallel_writes_match_a_model() {
    for opts in [
        &["-t", "ext4"][..],
        &["-t", "ext4", "-b", "1024"],
        &["-t", "ext4", "-O", "inline_data"],
        &["-t", "ext3"],
    ] {
        let img = Image::new(32, opts);
        let (mem, _, shared) = shared_mount(&img, 4096);
        let bs = shared.with(|fs| Ok(fs.block_size() as u64)).unwrap();
        let files: Vec<u32> = (0..3).map(|i| create(&shared, &format!("f{i}"))).collect();
        let mut model = vec![Vec::new(); files.len()];
        if opts.contains(&"ext4") {
            // preallocated (unwritten) blocks for writes to land in
            shared.with(|fs| fs.fallocate(files[2], 0, 1 << 20, true)).unwrap();
        }
        let mut rng = Rng(0xFEED_0001);
        for step in 0..300 {
            let k = rng.below(files.len() as u64) as usize;
            let f = files[k];
            if rng.below(8) == 0 {
                let size = rng.below(1 << 21);
                shared.with(|fs| fs.truncate(f, size)).unwrap();
                model[k].resize(size as usize, 0);
            } else {
                // mostly whole blocks; sometimes unaligned, or at the end
                let mut offset = rng.below(1 << 21);
                let mut len = rng.below(300_000) + 1;
                if rng.below(4) != 0 {
                    offset = offset / bs * bs;
                }
                if rng.below(3) != 0 {
                    len = len.div_ceil(bs) * bs;
                }
                if rng.below(5) == 0 {
                    offset = model[k].len() as u64;
                }
                let data = pattern(len as usize, step);
                assert_eq!(write(&shared, f, offset, &data).unwrap(), data.len());
                apply(&mut model[k], offset, &data);
            }
            let differs = contents(&shared, f) != model[k];
            assert!(!differs, "{opts:?} step {step}: file {k} differs");
        }
        shared_finish(&img, &mem, shared);
    }
}

#[test]
fn overlapping_writes_land_in_arrival_order() {
    let img = Image::new(32, &["-t", "ext4"]);
    let (mem, dev, shared) = shared_mount(&img, 4096);
    let f = create(&shared, "f");
    let writes: Vec<(u64, Vec<u8>)> = (0..12u64)
        .map(|i| {
            let len = 3 * 65536 + 4096 * (i % 3);
            ((i % 4) * 65536, vec![i as u8 + 1; len as usize])
        })
        .collect();
    // reserved in order but started in reverse: later writes must wait for
    // the earlier ones they overlap
    let tickets: Vec<u64> = writes
        .iter()
        .map(|(o, d)| shared.reserve_write(f, *o, d.len()))
        .collect();
    std::thread::scope(|s| {
        for (i, (o, d)) in writes.iter().enumerate().rev() {
            let (shared, ticket) = (&shared, tickets[i]);
            s.spawn(move || assert_eq!(shared.write_parallel(ticket, f, *o, d).unwrap(), d.len()));
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let mut model = Vec::new();
    for (o, d) in &writes {
        apply(&mut model, *o, d);
    }
    assert!(contents(&shared, f) == model, "overlapping writes applied out of order");

    // disjoint writes do reach the device together
    let g = create(&shared, "g");
    std::thread::scope(|s| {
        for i in 0..8u64 {
            let shared = &shared;
            s.spawn(move || write(shared, g, i << 20, &pattern(1 << 20, i)).unwrap());
        }
    });
    let most = dev.most_writes_at_once.load(Ordering::SeqCst);
    assert!(most > 1, "data writes never overlapped ({most} at most)");
    let mut model = Vec::new();
    for i in 0..8u64 {
        apply(&mut model, i << 20, &pattern(1 << 20, i));
    }
    assert!(contents(&shared, g) == model);
    shared_finish(&img, &mem, shared);
}

/// Every 8-byte word written to a file holds its inode number, so a read
/// or write that used blocks freed and reused meanwhile leaves a foreign
/// word behind.
fn word(ino: u32) -> u64 {
    (ino as u64) << 32 | 0x5EED_CAFE
}

fn fill(ino: u32, len: usize) -> Vec<u8> {
    let w = word(ino).to_le_bytes();
    (0..len).map(|i| w[i % 8]).collect()
}

fn check_words(f: u32, offset: u64, data: &[u8]) {
    for (i, w) in data.as_chunks::<8>().0.iter().enumerate() {
        let w = u64::from_le_bytes(*w);
        assert!(
            w == 0 || w == word(f),
            "inode {f} at {}: {w:#x}, which it was never given",
            offset + i as u64 * 8
        );
    }
}

/// Write (`writer`) or read random ranges of the files in `names` until
/// `stop`, checking every word read.
fn race_worker(
    seed: u64,
    writer: bool,
    shared: &SharedFs,
    stop: &AtomicBool,
    names: &[Vec<u8>],
    writes: &AtomicUsize,
    reads: &AtomicUsize,
) {
    let mut rng = Rng(0x9E37_79B9 + seed);
    let mut buf = vec![0u8; 200 * 1024];
    while !stop.load(Ordering::Relaxed) {
        let name = &names[rng.below(names.len() as u64) as usize];
        let Ok(f) = shared.with(|fs| fs.lookup(2, name)) else {
            continue;
        };
        // whole words; mostly whole blocks
        let mut offset = rng.below(400_000) & !7;
        let mut len = (rng.below(150_000) as usize + 8) & !7;
        if rng.below(10) != 0 {
            offset &= !1023;
            len = len.div_ceil(1024) * 1024;
        }
        if writer {
            match write(shared, f, offset, &fill(f, len)) {
                // short when the disk is full
                Ok(n) => assert!(n <= len),
                // deleted meanwhile, or full
                Err(Error::NotFound | Error::NoSpace) => continue,
                Err(e) => panic!("write to inode {f}: {e}"),
            }
            writes.fetch_add(1, Ordering::Relaxed);
        } else {
            let Ok(n) = shared.read_parallel(f, offset, &mut buf[..len]) else {
                continue;
            };
            check_words(f, offset, &buf[..n]);
            reads.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[test]
fn parallel_writes_race_reads_truncation_and_deletion() {
    let img = Image::new(32, &["-t", "ext4", "-b", "1024"]);
    let (mem, dev, shared) = shared_mount(&img, 1024);
    let shared = Arc::new(shared);
    const FILES: usize = 6;
    let names: Vec<Vec<u8>> = (0..FILES).map(|i| format!("f{i}").into_bytes()).collect();
    for n in &names {
        let f = create(&shared, std::str::from_utf8(n).unwrap());
        write(&shared, f, 0, &fill(f, 100_000)).unwrap();
    }
    // Little free space, so freed blocks are soon handed out again, and
    // writes in flight long enough for that to happen meanwhile.
    shared
        .with(|fs| {
            let filler = fs.create(2, b"filler", FileType::Regular, 0o644, 0, 0, 0)?.ino;
            let keep = 3 << 20;
            let s = fs.statfs();
            let len = (s.free_blocks * s.block_size as u64).saturating_sub(keep);
            fs.fallocate(filler, 0, len, false)
        })
        .unwrap();
    dev.write_micros.store(10_000, Ordering::Relaxed);
    *dev.slow_mark.lock().unwrap() = 0x5EED_CAFEu32.to_le_bytes().to_vec();
    let stop = Arc::new(AtomicBool::new(false));
    let (writes, reads) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut threads = Vec::new();
    for t in 0..6u64 {
        let (shared, stop, names) = (shared.clone(), stop.clone(), names.clone());
        let (writes, reads) = (writes.clone(), reads.clone());
        // the writers' data writes are slow (see SlowDevice)
        let name = if t < 3 { "slow-writer" } else { "reader" };
        let worker = move || race_worker(t, t < 3, &shared, &stop, &names, &writes, &reads);
        threads.push(std::thread::Builder::new().name(name.into()).spawn(worker).unwrap());
    }
    let mut rng = Rng(0xC0FF_EE11);
    let start = Instant::now();
    let mut ops = 0;
    while start.elapsed() < Duration::from_secs(4) {
        let name = &names[rng.below(FILES as u64) as usize];
        let other = &names[rng.below(FILES as u64) as usize];
        let (a, b) = (rng.below(300_000) & !7, (rng.below(200_000) + 8) & !7);
        let c = rng.below(300_000) & !1023;
        let r = shared.with(|fs| {
            let f = match fs.lookup(2, name) {
                // a re-creation that ran out of space
                Err(Error::NotFound) => fs.create(2, name, FileType::Regular, 0o644, 0, 0, 0)?.ino,
                r => r?,
            };
            match rng.below(3) {
                0 => {
                    fs.truncate(f, a)?;
                }
                1 => {
                    fs.unlink(2, name)?;
                    let f = fs.create(2, name, FileType::Regular, 0o644, 0, 0, 0)?.ino;
                    fs.write(f, 0, &fill(f, b as usize))?;
                }
                _ => fs.punch_hole(f, a & !1023, b)?,
            }
            // make the freed blocks free now and hand them out again
            fs.sync()?;
            if let Ok(g) = fs.lookup(2, other) {
                fs.write(g, c, &fill(g, 64 * 1024))?;
            }
            Ok(())
        });
        match r {
            Ok(()) | Err(Error::NoSpace) => {}
            Err(e) => panic!("{e}"),
        }
        ops += 1;
        // let the other threads in
        std::thread::sleep(Duration::from_millis(3));
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    let (writes, reads) = (writes.load(Ordering::Relaxed), reads.load(Ordering::Relaxed));
    assert!(
        ops > 60 && writes > 100 && reads > 100,
        "too little overlap: {ops} changes, {writes} writes, {reads} reads"
    );
    assert!(dev.most_writes_at_once.load(Ordering::SeqCst) > 1);
    for name in &names {
        if let Ok(f) = shared.with(|fs| fs.lookup(2, name)) {
            check_words(f, 0, &contents(&shared, f));
        }
    }
    let shared = Arc::into_inner(shared).unwrap();
    shared_finish(&img, &mem, shared);
}

#[test]
fn write_that_does_not_fit_changes_nothing() {
    let img = Image::new(16, &["-t", "ext4"]);
    let (mem, _, shared) = shared_mount(&img, 4096);
    let f = create(&shared, "f");
    write(&shared, f, 0, &pattern(8192, 1)).unwrap();
    let free = shared.with_shared(|fs| Ok(fs.statfs().free_blocks)).unwrap();
    let before = contents(&shared, f);
    let too_big = vec![7u8; (free as usize + 100) * 4096];
    assert!(matches!(write(&shared, f, 4096, &too_big), Err(Error::NoSpace)));
    assert!(contents(&shared, f) == before, "a failed write changed the file");
    assert_eq!(shared.with_shared(|fs| Ok(fs.statfs().free_blocks)).unwrap(), free);
    // what fits still goes in
    let fits = vec![9u8; 64 * 4096];
    assert_eq!(write(&shared, f, 8192, &fits).unwrap(), fits.len());
    shared_finish(&img, &mem, shared);
}

#[test]
fn failed_device_write_exposes_nothing() {
    let img = Image::new(32, &["-t", "ext4"]);
    let (mem, _, shared) = shared_mount(&img, 4096);
    // blocks holding old data, then freed
    let old = shared
        .with(|fs| {
            let a = fs.create(2, b"old", FileType::Regular, 0o644, 0, 0, 0)?.ino;
            fs.write(a, 0, &vec![0xEE; 1 << 20])?;
            let ext = fs.file_extents(a)?;
            fs.sync()?;
            fs.unlink(2, b"old")?;
            fs.sync()?;
            Ok(ext
                .iter()
                .map(|e| (e.start, e.start + e.len as u64))
                .collect::<Vec<_>>())
        })
        .unwrap();
    let f = create(&shared, "f");
    let mut data = vec![0x11u8; 1 << 20];
    data[..8].copy_from_slice(FAIL_MARK);
    assert!(write(&shared, f, 0, &data).is_err());
    let new = shared.with(|fs| fs.file_extents(f)).unwrap();
    assert!(
        new.iter()
            .any(|e| old.iter().any(|&(s, t)| e.start < t && s < e.start + e.len as u64)),
        "the failed write should have been given the freed blocks: {new:?}, old {old:?}"
    );
    assert_eq!(shared.with(|fs| fs.stat(f)).unwrap().size, 0);
    // what it allocated reads as zeros once the file covers it
    shared.with(|fs| fs.truncate(f, 1 << 20)).unwrap();
    assert!(contents(&shared, f).iter().all(|&b| b == 0));
    // and after a power loss
    shared.with(|fs| fs.sync()).unwrap();
    let crashed = Image::new(32, &["-t", "ext4"]);
    std::fs::write(&crashed.path, mem.snapshot()).unwrap();
    shared_finish(&img, &mem, shared);
    let dev = Arc::new(MemDevice::from_vec(std::fs::read(&crashed.path).unwrap()));
    let mut fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
    let f = fs.lookup(2, b"f").unwrap();
    let mut b = vec![0xAAu8; 1 << 20];
    assert_eq!(fs.read(f, 0, &mut b).unwrap(), 1 << 20);
    assert!(b.iter().all(|&x| x == 0), "stale data after power loss");
    fs.unmount().unwrap();
    std::fs::write(&crashed.path, dev.snapshot()).unwrap();
    crashed.assert_clean();
}
