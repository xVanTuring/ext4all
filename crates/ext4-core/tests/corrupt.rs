//! Robustness against corrupted media: random metadata damage must lead to
//! errors, never to panics, hangs or runaway memory use.

mod common;
use common::*;
use ext4_core::{FileType, Fs, MemDevice, MountOptions};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

struct Rng(u64);
impl Rng {
    fn next(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % m
    }
}

/// Walk everything reachable, bounded in work. Errors are fine.
fn walk(fs: &mut Fs) -> usize {
    let mut visited = 0usize;
    let mut stack = vec![(fs.root(), 0usize)];
    let mut buf = vec![0u8; 64 * 1024];
    while let Some((dir, depth)) = stack.pop() {
        if visited > 5000 || depth > 32 {
            break;
        }
        let Ok(entries) = fs.list_dir(dir) else { continue };
        for e in entries.into_iter().take(2000) {
            visited += 1;
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let _ = fs.lookup(dir, &e.name);
            let Ok(a) = fs.stat(e.ino) else { continue };
            let _ = fs.list_xattr(e.ino);
            let _ = fs.get_xattr(e.ino, b"user.k");
            match a.file_type {
                FileType::Directory => stack.push((e.ino, depth + 1)),
                FileType::Symlink => {
                    let _ = fs.read_link(e.ino);
                }
                FileType::Regular => {
                    // read the start, the end and a middle chunk
                    for off in [0, a.size / 2, a.size.saturating_sub(4096)] {
                        let _ = fs.read(e.ino, off, &mut buf);
                    }
                    let _ = fs.file_extents(e.ino);
                }
                _ => {}
            }
        }
    }
    visited
}

fn populated_image(opts: &[&str]) -> Vec<u8> {
    let img = Image::new(16, opts);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let d = fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap().ino;
        for i in 0..150 {
            let a = fs
                .create(d, format!("file-{i:03}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                .unwrap();
            fs.write(a.ino, 0, &pattern(100 + i * 97, i as u64)).unwrap();
            if i % 10 == 0 {
                fs.set_xattr(a.ino, b"user.k", &pattern(300, i as u64), ext4_core::XattrSetMode::Any)
                    .unwrap();
            }
        }
        let f = fs.create(root, b"frag", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        let g = fs
            .create(root, b"frag2", FileType::Regular, 0o644, 0, 0, 0)
            .unwrap()
            .ino;
        for i in 0..300u64 {
            fs.write(f, i * 2048, &pattern(1024, i)).unwrap();
            fs.write(g, i * 2048, &pattern(1024, i)).unwrap();
        }
        fs.symlink(root, b"slow", "s".repeat(300).as_bytes(), 0, 0).unwrap();
        fs.symlink(root, b"fast", b"dir/file-001", 0, 0).unwrap();
        let mut p = root;
        for i in 0..10 {
            p = fs.mkdir(p, format!("n{i}").as_bytes(), 0o755, 0, 0).unwrap().ino;
        }
        fs.unmount().unwrap();
    }
    std::fs::read(&img.path).unwrap()
}

/// Run `f` on another thread; fail the test if it does not finish in time.
fn with_timeout<F: FnOnce() + Send + 'static>(what: String, f: F) {
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let _ = tx.send(r.is_ok());
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(true) => {
            h.join().unwrap();
        }
        Ok(false) => panic!("{what}: panicked"),
        Err(_) => panic!("{what}: did not finish (hang)"),
    }
}

fn fuzz(opts: &[&str], seeds: std::ops::Range<u64>, flips: u64) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let base = populated_image(opts);
    // metadata lives mostly in the first few MB (group 0 tables, inode
    // tables, directories); damage bytes there
    let span = (base.len() as u64).min(4 << 20);
    let mounted = Arc::new(AtomicUsize::new(0));
    let visited = Arc::new(AtomicUsize::new(0));
    let total = seeds.end - seeds.start;
    for seed in seeds {
        let mut img = base.clone();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for _ in 0..flips {
            let off = 2048 + rng.next(span - 2048) as usize; // keep the superblock
            img[off] ^= 1 << rng.next(8);
        }
        let (m, v) = (mounted.clone(), visited.clone());
        with_timeout(format!("seed {seed}"), move || {
            let dev = Arc::new(MemDevice::from_vec(img));
            let opts = MountOptions {
                read_only: true,
                strict_checksums: false,
                ..Default::default()
            };
            if let Ok(mut fs) = Fs::mount(dev, opts) {
                m.fetch_add(1, Ordering::SeqCst);
                v.fetch_add(walk(&mut fs), Ordering::SeqCst);
            }
        });
    }
    let m = mounted.load(Ordering::SeqCst) as u64;
    let v = visited.load(Ordering::SeqCst) as u64;
    eprintln!("mounted {m}/{total}, visited {v} entries");
    assert!(m * 2 > total, "most damaged images should still mount ({m}/{total})");
    assert!(v > m * 50, "the walk should reach most of the tree ({v} entries)");
}

#[test]
fn corrupted_metadata_never_panics_4k() {
    fuzz(&["-t", "ext4", "-b", "4096"], 0..150, 40);
}

#[test]
fn corrupted_metadata_never_panics_1k() {
    fuzz(&["-t", "ext4", "-b", "1024"], 1000..1150, 60);
}

#[test]
fn corrupted_metadata_never_panics_ext3() {
    fuzz(&["-t", "ext3", "-b", "1024"], 2000..2100, 60);
}

/// With strict checksums (the default) damaged metadata is refused.
#[test]
fn strict_mode_detects_damage() {
    let base = populated_image(&["-t", "ext4", "-b", "4096"]);
    let mut detected = 0;
    for seed in 0..40u64 {
        let mut img = base.clone();
        let mut rng = Rng(seed + 1);
        for _ in 0..20 {
            let off = 4096 + rng.next(2 << 20) as usize;
            img[off] ^= 0xFF;
        }
        let dev = Arc::new(MemDevice::from_vec(img));
        match Fs::mount(
            dev,
            MountOptions {
                read_only: true,
                ..Default::default()
            },
        ) {
            Err(ext4_core::Error::Checksum(_)) => detected += 1,
            Err(_) => detected += 1,
            Ok(mut fs) => {
                let before = fs.checksum_errors;
                walk(&mut fs);
                if fs.checksum_errors > before {
                    detected += 1;
                }
            }
        }
    }
    assert!(detected > 0);
}

/// Superblock damage is rejected at mount, never trusted.
#[test]
fn corrupted_superblock_fields() {
    let base = populated_image(&["-t", "ext4", "-b", "4096"]);
    for field in [0x0usize, 0x4, 0x14, 0x18, 0x20, 0x28, 0x58, 0xFE, 0x150] {
        for val in [0u32, 1, 0xFFFF_FFFF, 0x8000_0000] {
            let mut img = base.clone();
            img[1024 + field..1024 + field + 4].copy_from_slice(&val.to_le_bytes());
            // fix the checksum so only the semantic check can reject it
            let mut sb = ext4_core::ondisk::superblock::Superblock {
                raw: Box::new(img[1024..2048].try_into().unwrap()),
            };
            sb.update_checksum();
            img[1024..2048].copy_from_slice(&sb.raw[..]);
            with_timeout(format!("field {field:#x} = {val:#x}"), move || {
                let dev = Arc::new(MemDevice::from_vec(img));
                if let Ok(mut fs) = Fs::mount(
                    dev,
                    MountOptions {
                        read_only: true,
                        strict_checksums: false,
                        ..Default::default()
                    },
                ) {
                    walk(&mut fs);
                }
            });
        }
    }
}

/// Block numbers holding metadata worth damaging: the start of group 0's
/// inode table, every directory block, and extent tree nodes.
fn metadata_targets(image: &[u8], bs: u64) -> Vec<u64> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("img");
    std::fs::write(&p, image).unwrap();
    let out = std::process::Command::new(tool("dumpe2fs")).arg(&p).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    let itable: u64 = s
        .lines()
        .find_map(|l| l.trim().strip_prefix("Inode table at "))
        .and_then(|r| r.split('-').next())
        .and_then(|n| n.trim().parse().ok())
        .expect("inode table location");
    let mut targets: Vec<u64> = (itable..itable + 8).collect();
    let dev = Arc::new(MemDevice::from_vec(image.to_vec()));
    let mut fs = Fs::mount(
        dev,
        MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut stack = vec![fs.root()];
    while let Some(d) = stack.pop() {
        for e in fs.file_extents(d).unwrap() {
            targets.extend(e.start..e.start + e.len as u64);
        }
        for e in fs.list_dir(d).unwrap() {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            targets.extend(fs.extent_tree_blocks(e.ino).unwrap_or_default());
            if e.file_type == FileType::Directory {
                stack.push(e.ino);
            }
        }
    }
    let _ = bs;
    targets.sort_unstable();
    targets.dedup();
    targets
}

fn targeted_fuzz(opts: &[&str], seeds: std::ops::Range<u64>) {
    let base = populated_image(opts);
    let bs = u32::from_le_bytes(base[1024 + 0x18..1024 + 0x1C].try_into().unwrap());
    let bs = 1024u64 << bs;
    let targets = metadata_targets(&base, bs);
    assert!(targets.len() > 10, "{targets:?}");
    for seed in seeds {
        let mut img = base.clone();
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) | 1);
        let n = 1 + rng.next(8);
        for _ in 0..n {
            let blk = targets[rng.next(targets.len() as u64) as usize];
            let off = blk * bs + rng.next(bs);
            let v = rng.next(256) as u8;
            img[off as usize] = if rng.next(2) == 0 {
                v
            } else {
                img[off as usize] ^ (1 << (v % 8))
            };
        }
        with_timeout(format!("targeted seed {seed}"), move || {
            let dev = Arc::new(MemDevice::from_vec(img));
            if let Ok(mut fs) = Fs::mount(
                dev,
                MountOptions {
                    read_only: true,
                    strict_checksums: false,
                    ..Default::default()
                },
            ) {
                walk(&mut fs);
            }
        });
    }
}

#[test]
fn targeted_metadata_damage_4k() {
    targeted_fuzz(&["-t", "ext4", "-b", "4096"], 0..400);
}

#[test]
fn targeted_metadata_damage_1k() {
    targeted_fuzz(&["-t", "ext4", "-b", "1024"], 500..900);
}

#[test]
fn targeted_metadata_damage_ext3() {
    targeted_fuzz(&["-t", "ext3", "-b", "1024"], 1000..1300);
}

/// Writing to a damaged file system may fail, but must not panic or hang.
fn targeted_rw_fuzz(opts: &[&str], seeds: std::ops::Range<u64>) {
    let base = populated_image(opts);
    let bs = 1024u64 << u32::from_le_bytes(base[1024 + 0x18..1024 + 0x1C].try_into().unwrap());
    let targets = metadata_targets(&base, bs);
    for seed in seeds {
        let mut img = base.clone();
        let mut rng = Rng(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
        for _ in 0..1 + rng.next(6) {
            let blk = targets[rng.next(targets.len() as u64) as usize];
            let off = (blk * bs + rng.next(bs)) as usize;
            img[off] ^= 1 << rng.next(8);
        }
        with_timeout(format!("rw seed {seed}"), move || {
            let dev = Arc::new(MemDevice::from_vec(img));
            let Ok(mut fs) = Fs::mount(
                dev,
                MountOptions {
                    strict_checksums: false,
                    ..Default::default()
                },
            ) else {
                return;
            };
            let root = fs.root();
            let d = fs.lookup(root, b"dir").unwrap_or(root);
            for i in 0..40u64 {
                let name = format!("file-{:03}", (i * 7) % 150);
                let _ = match i % 6 {
                    0 => fs.unlink(d, name.as_bytes()),
                    1 => fs
                        .create(d, format!("new-{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                        .map(|_| ()),
                    2 => fs
                        .lookup(d, name.as_bytes())
                        .and_then(|ino| fs.write(ino, i * 1000, &[1u8; 5000]).map(|_| ())),
                    3 => fs
                        .lookup(root, b"frag")
                        .and_then(|ino| fs.truncate(ino, i * 3000).map(|_| ())),
                    4 => fs.rename(
                        d,
                        name.as_bytes(),
                        root,
                        format!("moved-{i}").as_bytes(),
                        Default::default(),
                    ),
                    _ => fs.mkdir(d, format!("sub-{i}").as_bytes(), 0o755, 0, 0).map(|_| ()),
                };
            }
            let _ = fs.sync();
            let _ = fs.unmount();
        });
    }
}

#[test]
fn damaged_file_system_writes_do_not_panic() {
    targeted_rw_fuzz(&["-t", "ext4", "-b", "4096"], 0..150);
    targeted_rw_fuzz(&["-t", "ext4", "-b", "1024"], 200..350);
    targeted_rw_fuzz(&["-t", "ext3", "-b", "1024"], 400..500);
}

/// Random values in random superblock fields (checksum kept valid) must
/// be rejected or handled without panicking (debug build: overflow checks).
#[test]
fn random_superblock_fields() {
    let base = populated_image(&["-t", "ext4", "-b", "4096"]);
    let mut rng = Rng(0x5EED);
    for case in 0..600 {
        let mut img = base.clone();
        for _ in 0..1 + rng.next(3) {
            let field = (rng.next(0x3FC / 2) * 2) as usize; // 16-bit aligned
            let width = if rng.next(2) == 0 { 2 } else { 4 };
            if field + width > 0x3FC {
                continue;
            }
            for b in 0..width {
                img[1024 + field + b] = rng.next(256) as u8;
            }
        }
        let mut sb = ext4_core::ondisk::superblock::Superblock {
            raw: Box::new(img[1024..2048].try_into().unwrap()),
        };
        sb.update_checksum();
        img[1024..2048].copy_from_slice(&sb.raw[..]);
        with_timeout(format!("superblock case {case}"), move || {
            let dev = Arc::new(MemDevice::from_vec(img));
            if let Ok(mut fs) = Fs::mount(
                dev,
                MountOptions {
                    read_only: true,
                    strict_checksums: false,
                    ..Default::default()
                },
            ) {
                walk(&mut fs);
            }
        });
    }
}
