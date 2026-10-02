//! `SharedFs::read_parallel`: device reads outside the file system lock,
//! compared with locked reads and raced against writes, truncation,
//! hole punching and deletion.

mod common;
use common::*;
use ext4_core::{BlockDevice, Error, FileType, Fs, MemDevice, MountOptions, Result, SharedFs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A memory device whose data reads take a while, so that a read running
/// without the lock overlaps the operations of other threads.
struct SlowReads {
    inner: Arc<MemDevice>,
    block: usize,
}

impl BlockDevice for SlowReads {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.len() > self.block {
            std::thread::sleep(Duration::from_micros(300));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

fn mount(img: &Image, block: usize) -> (Arc<MemDevice>, SharedFs) {
    let mem = Arc::new(MemDevice::from_vec(std::fs::read(&img.path).unwrap()));
    let dev = Arc::new(SlowReads {
        inner: mem.clone(),
        block,
    });
    let fs = Fs::mount(dev, MountOptions::default()).unwrap();
    (mem, SharedFs::new(fs, Duration::from_millis(20)))
}

fn finish(img: &Image, mem: &MemDevice, shared: SharedFs) {
    shared.unmount().unwrap();
    drop(shared);
    std::fs::write(&img.path, mem.snapshot()).unwrap();
    img.assert_clean();
}

#[test]
fn parallel_reads_match_locked_reads() {
    for opts in [
        &["-t", "ext4"][..],
        &["-t", "ext4", "-b", "1024", "-O", "inline_data"],
        &["-t", "ext3"],
    ] {
        let img = Image::new(32, opts);
        let (mem, shared) = mount(&img, 4096);
        let extents = !opts.contains(&"ext3");
        let files: Vec<u32> = (0..4)
            .map(|i| {
                shared
                    .with(|fs| {
                        let name = format!("f{i}");
                        let f = fs.create(2, name.as_bytes(), FileType::Regular, 0o644, 0, 0, 0)?.ino;
                        match i {
                            // tiny: inline data where enabled
                            0 => fs.write(f, 0, &pattern(40, 1))?,
                            // contiguous
                            1 => fs.write(f, 0, &pattern(700_000, 2))?,
                            // sparse, with a hole at the start and between
                            2 => {
                                fs.write(f, 50_000, &pattern(30_000, 3))?;
                                fs.write(f, 400_123, &pattern(77_777, 4))?
                            }
                            // preallocated (unwritten) blocks around data
                            _ => {
                                if extents {
                                    fs.fallocate(f, 0, 600_000, false)?;
                                }
                                fs.write(f, 123_456, &pattern(100_000, 5))?
                            }
                        };
                        Ok(f)
                    })
                    .unwrap()
            })
            .collect();
        let mut rng = Rng(0x1234_5678);
        for _ in 0..400 {
            let f = files[rng.below(files.len() as u64) as usize];
            let offset = rng.below(800_000);
            let len = rng.below(300_000) as usize;
            let mut a = vec![0xAAu8; len];
            let mut b = vec![0x55u8; len];
            let na = shared.read_parallel(f, offset, &mut a).unwrap();
            let nb = shared.with(|fs| fs.read(f, offset, &mut b)).unwrap();
            assert_eq!(na, nb, "{opts:?} inode {f} at {offset}+{len}");
            assert!(
                a[..na] == b[..nb],
                "{opts:?} inode {f} at {offset}+{len}: contents differ"
            );
        }
        let dir = shared.with(|fs| Ok(fs.root())).unwrap();
        assert!(matches!(
            shared.read_parallel(dir, 0, &mut [0u8; 16]),
            Err(Error::IsDir)
        ));
        finish(&img, &mem, shared);
    }
}

/// Every 8-byte word written to a file holds its inode number, so a read
/// that returned blocks freed and reused by another file (or by metadata)
/// meanwhile would see a foreign word.
fn word(ino: u32) -> u64 {
    (ino as u64) << 32 | 0x5EED_CAFE
}

fn fill(ino: u32, len: usize) -> Vec<u8> {
    let w = word(ino).to_le_bytes();
    (0..len).map(|i| w[i % 8]).collect()
}

#[test]
fn parallel_reads_race_block_reuse() {
    let img = Image::new(16, &["-t", "ext4", "-b", "1024"]);
    let (mem, shared) = mount(&img, 1024);
    let shared = Arc::new(shared);
    const FILES: usize = 6;
    let names: Vec<Vec<u8>> = (0..FILES).map(|i| format!("f{i}").into_bytes()).collect();
    for n in &names {
        shared
            .with(|fs| {
                let f = fs.create(2, n, FileType::Regular, 0o644, 0, 0, 0)?.ino;
                fs.write(f, 0, &fill(f, 200_000))
            })
            .unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = (0..4)
        .map(|t| {
            let (shared, stop, reads, names) = (shared.clone(), stop.clone(), reads.clone(), names.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(0x9E37_79B9 + t);
                let mut buf = vec![0u8; 256 * 1024];
                while !stop.load(Ordering::Relaxed) {
                    let name = &names[rng.below(FILES as u64) as usize];
                    let Ok(f) = shared.with(|fs| fs.lookup(2, name)) else {
                        continue;
                    };
                    // whole words, so every word read belongs to one write
                    let offset = rng.below(300_000) & !7;
                    let len = (rng.below(buf.len() as u64) as usize).max(8) & !7;
                    // the file may be deleted meanwhile: that is an error, not data
                    let Ok(n) = shared.read_parallel(f, offset, &mut buf[..len]) else {
                        continue;
                    };
                    // as the extension does for its reply
                    let _ = shared.with_shared(|fs| fs.stat(f));
                    for (i, w) in buf[..n].as_chunks::<8>().0.iter().enumerate() {
                        let w = u64::from_le_bytes(*w);
                        assert!(
                            w == 0 || w == word(f),
                            "inode {f} at {}: read {w:#x}, which it never contained",
                            offset + i as u64 * 8
                        );
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    let mut rng = Rng(0xC0FF_EE11);
    let start = Instant::now();
    let mut ops = 0;
    while start.elapsed() < Duration::from_secs(4) {
        let name = &names[rng.below(FILES as u64) as usize];
        let choice = rng.below(4);
        let (a, b) = (rng.below(300_000) & !7, (rng.below(200_000) as usize + 8) & !7);
        shared
            .with(|fs| {
                let f = fs.lookup(2, name)?;
                match choice {
                    // shrink, then grow again with new blocks
                    0 => {
                        fs.truncate(f, a)?;
                        fs.write(f, a, &fill(f, b))?;
                    }
                    // a new inode under the same name
                    1 => {
                        fs.unlink(2, name)?;
                        let f = fs.create(2, name, FileType::Regular, 0o644, 0, 0, 0)?.ino;
                        fs.write(f, 0, &fill(f, b))?;
                    }
                    2 => fs.punch_hole(f, a & !4095, b as u64)?,
                    _ => {
                        fs.write(f, a, &fill(f, b))?;
                    }
                }
                Ok(())
            })
            .unwrap();
        ops += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    let reads = reads.load(Ordering::Relaxed);
    assert!(
        ops > 100 && reads > 1000,
        "too little overlap: {ops} changes, {reads} reads"
    );
    let shared = Arc::into_inner(shared).unwrap();
    finish(&img, &mem, shared);
}
