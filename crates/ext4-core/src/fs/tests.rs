//! Internal tests that exercise crate-private machinery directly.

use super::*;
use crate::device::MemDevice;
use crate::ondisk::extent::Extent;
use crate::ondisk::inode::FileType;
use std::collections::BTreeMap;
use std::process::Command;

fn mkfs(size_mb: u64, opts: &[&str]) -> Arc<MemDevice> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("img");
    std::fs::File::create(&p).unwrap().set_len(size_mb << 20).unwrap();
    let sbin = std::env::var("E2FSPROGS_SBIN").unwrap_or_else(|_| "/opt/homebrew/opt/e2fsprogs/sbin".into());
    let out = Command::new(format!("{sbin}/mke2fs"))
        .args(["-F", "-q"])
        .args(opts)
        .arg(&p)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    Arc::new(MemDevice::from_vec(std::fs::read(&p).unwrap()))
}

fn mount(dev: Arc<MemDevice>) -> Fs {
    Fs::mount(dev, MountOptions::default()).unwrap()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % m
    }
}

/// Expand extents into a per-block map.
fn blocks_of(exts: &[Extent]) -> BTreeMap<u32, (u64, bool)> {
    let mut m = BTreeMap::new();
    for e in exts {
        for i in 0..e.len {
            m.insert(e.block + i, (e.start + i as u64, e.unwritten));
        }
    }
    m
}

#[test]
fn extent_tree_random_insert_remove() {
    for (bs, seed) in [(1024u32, 1u64), (4096, 2), (1024, 3)] {
        let dev = mkfs(64, &["-t", "ext4", "-b", &bs.to_string()]);
        let mut fs = mount(dev);
        let root = fs.root();
        let ino = fs.create(root, b"t", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        let mut inode = fs.read_inode(ino).unwrap();
        let mut model: BTreeMap<u32, (u64, bool)> = BTreeMap::new();
        let mut rng = Rng((seed * 0x9E37_79B9) | 1);
        for step in 0..3000 {
            if rng.next(10) < 7 {
                // insert a random unmapped range
                let start = rng.next(200_000) as u32;
                let len = 1 + rng.next(20) as u32;
                if (start..start + len).any(|b| model.contains_key(&b)) {
                    continue;
                }
                let unwritten = rng.next(4) == 0;
                // physical: mostly contiguous with the logical position so
                // merges happen, sometimes random
                let phys = if rng.next(3) == 0 {
                    10_000_000 + rng.next(1_000_000)
                } else {
                    5_000_000 + start as u64
                };
                fs.ext_insert(
                    ino,
                    &mut inode,
                    Extent {
                        block: start,
                        len,
                        start: phys,
                        unwritten,
                    },
                )
                .unwrap();
                for i in 0..len {
                    model.insert(start + i, (phys + i as u64, unwritten));
                }
            } else {
                let from = rng.next(200_000) as u32;
                let to = from as u64 + 1 + rng.next(3000);
                let removed = fs.ext_remove_range(ino, &mut inode, from, to).unwrap();
                let mut want: u64 = 0;
                let keys: Vec<u32> = model
                    .range(from..to.min(u32::MAX as u64) as u32)
                    .map(|(&k, _)| k)
                    .collect();
                for k in keys {
                    model.remove(&k);
                    want += 1;
                }
                let got: u64 = removed.iter().map(|r| r.1).sum();
                assert_eq!(got, want, "step {step}: removed block count");
            }
            if step % 250 == 0 {
                fs.write_inode(ino, &inode).unwrap();
                fs.check_extent_tree(ino).unwrap();
                let all = fs.ext_all(ino, &inode).unwrap();
                assert_eq!(blocks_of(&all), model, "step {step}");
            }
        }
        fs.write_inode(ino, &inode).unwrap();
        fs.check_extent_tree(ino).unwrap();
        let all = fs.ext_all(ino, &inode).unwrap();
        assert_eq!(blocks_of(&all), model);
        // spot-check the mapping function against the model
        for _ in 0..2000 {
            let b = rng.next(210_000) as u32;
            match fs.ext_map(ino, &inode, b).unwrap() {
                extent::Mapping::Mapped { pblk, len, unwritten } => {
                    assert_eq!(model.get(&b), Some(&(pblk, unwritten)));
                    for i in 0..len.min(50) {
                        assert!(model.contains_key(&(b + i as u32)));
                    }
                }
                extent::Mapping::Hole { len } => {
                    assert!(!model.contains_key(&b));
                    let next = model.range(b..).next().map(|(&k, _)| k as u64).unwrap_or(1 << 32);
                    assert_eq!(b as u64 + len, next, "hole length at {b}");
                }
            }
        }
        // remove everything: tree collapses back to an empty root
        fs.ext_remove_range(ino, &mut inode, 0, 1 << 32).unwrap();
        let h = crate::ondisk::extent::ExtentHeader::parse(inode.block_area());
        assert_eq!(h.entries, 0);
        assert_eq!(h.depth, 0);
        std::mem::forget(fs);
    }
}

#[test]
fn mark_written_splits_unwritten_extent() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = mount(dev);
    let root = fs.root();
    let ino = fs.create(root, b"t", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let mut inode = fs.read_inode(ino).unwrap();
    fs.ext_insert(
        ino,
        &mut inode,
        Extent {
            block: 100,
            len: 50,
            start: 7000,
            unwritten: true,
        },
    )
    .unwrap();
    fs.ext_mark_written(ino, &mut inode, 110, 5).unwrap();
    let all = fs.ext_all(ino, &inode).unwrap();
    assert_eq!(
        all,
        vec![
            Extent {
                block: 100,
                len: 10,
                start: 7000,
                unwritten: true
            },
            Extent {
                block: 110,
                len: 5,
                start: 7010,
                unwritten: false
            },
            Extent {
                block: 115,
                len: 35,
                start: 7015,
                unwritten: true
            },
        ]
    );
    // marking the rest written merges everything back into one extent
    fs.ext_mark_written(ino, &mut inode, 100, 50).unwrap();
    let all = fs.ext_all(ino, &inode).unwrap();
    assert_eq!(all.len(), 1);
    assert!(!all[0].unwritten);
    assert_eq!(all[0].len, 50);
    std::mem::forget(fs);
}

#[test]
fn insert_rejects_overlap() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = mount(dev);
    let root = fs.root();
    let ino = fs.create(root, b"t", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let mut inode = fs.read_inode(ino).unwrap();
    let e = Extent {
        block: 10,
        len: 10,
        start: 500,
        unwritten: false,
    };
    fs.ext_insert(ino, &mut inode, e).unwrap();
    assert!(
        fs.ext_insert(
            ino,
            &mut inode,
            Extent {
                block: 15,
                len: 2,
                start: 900,
                unwritten: false
            }
        )
        .is_err()
    );
    assert!(
        fs.ext_insert(
            ino,
            &mut inode,
            Extent {
                block: 5,
                len: 6,
                start: 900,
                unwritten: false
            }
        )
        .is_err()
    );
    std::mem::forget(fs);
}

#[test]
fn allocator_prefers_goal_and_contiguity() {
    let dev = mkfs(64, &["-t", "ext4", "-b", "4096"]);
    let mut fs = mount(dev);
    let (a, n) = fs.alloc_blocks(0, 100).unwrap();
    assert_eq!(n, 100);
    let (b, _) = fs.alloc_blocks(a + 100, 10).unwrap();
    assert_eq!(b, a + 100, "goal block should be used when free");
    for x in a..a + 110 {
        assert!(fs.block_in_use(x).unwrap());
    }
    fs.free_blocks(a, 100).unwrap();
    // deferred until commit
    assert!(fs.block_in_use(a).unwrap());
    fs.commit().unwrap();
    assert!(!fs.block_in_use(a).unwrap());
    // out-of-range frees are rejected
    let total = fs.sb.blocks_count();
    assert!(fs.free_blocks(total, 1).is_err());
    std::mem::forget(fs);
}

#[test]
fn double_free_detected() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = mount(dev);
    let (a, _) = fs.alloc_blocks(0, 1).unwrap();
    fs.free_blocks(a, 1).unwrap();
    fs.free_blocks(a, 1).unwrap();
    assert!(matches!(fs.commit(), Err(Error::Corrupt(_))));
    std::mem::forget(fs);
}

#[test]
fn inode_allocation_skips_reserved() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = mount(dev);
    let first = fs.sb.first_ino();
    for _ in 0..5 {
        let ino = fs.alloc_inode(ROOT_INO, false).unwrap();
        assert!(ino >= first, "{ino}");
        assert!(fs.inode_in_use(ino).unwrap());
    }
    std::mem::forget(fs);
}

#[test]
fn directories_spread_across_groups() {
    let dev = mkfs(256, &["-t", "ext4", "-b", "1024"]);
    let mut fs = mount(dev);
    let ipg = fs.sb.inodes_per_group();
    let mut groups = std::collections::BTreeSet::new();
    let root = fs.root();
    for i in 0..20 {
        let d = fs.mkdir(root, format!("d{i}").as_bytes(), 0o755, 0, 0).unwrap().ino;
        groups.insert((d - 1) / ipg);
    }
    assert!(groups.len() > 3, "{groups:?}");
    std::mem::forget(fs);
}

#[test]
fn geometry_helpers() {
    let dev = mkfs(64, &["-t", "ext4", "-b", "1024"]);
    let fs = mount(dev);
    assert_eq!(fs.group_first_block(0), 1);
    assert_eq!(fs.group_of_block(1), 0);
    assert_eq!(fs.group_of_block(8193), 1);
    assert_eq!(fs.desc_block_location(0), 2);
    // group 0 has the superblock and descriptor table
    assert!(fs.base_meta_blocks(0) >= 2);
    assert_eq!(fs.base_meta_blocks(2), 0);
    let last = fs.group_count() - 1;
    assert!(fs.blocks_in_group(last) <= fs.sb.blocks_per_group());
    std::mem::forget(fs);
}

#[test]
fn checksum_errors_in_lenient_mode_are_counted() {
    let dev = mkfs(32, &["-t", "ext4"]);
    // corrupt the root inode's checksum
    let fs = mount(dev.clone());
    let (blk, off) = fs.inode_location(ROOT_INO).unwrap();
    let bs = fs.bs as u64;
    drop(fs);
    let mut b = [0u8; 1];
    dev.read_at(blk * bs + off as u64 + 0x7C, &mut b).unwrap();
    b[0] ^= 0xFF;
    dev.write_at(blk * bs + off as u64 + 0x7C, &b).unwrap();
    let mut strict = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
    assert!(matches!(strict.stat(ROOT_INO), Err(Error::Checksum(_))));
    std::mem::forget(strict);
    let mut lenient = Fs::mount(
        dev,
        MountOptions {
            strict_checksums: false,
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(lenient.stat(ROOT_INO).is_ok());
    assert!(lenient.checksum_errors > 0);
}

#[test]
fn pending_changes_tracking() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = Fs::mount(
        dev,
        MountOptions {
            commit_threshold: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!fs.has_pending_changes());
    let root = fs.root();
    fs.create(root, b"x", FileType::Regular, 0o644, 0, 0, 0).unwrap();
    assert!(fs.has_pending_changes());
    fs.sync().unwrap();
    assert!(!fs.has_pending_changes());
    fs.unmount().unwrap();
}

fn fsck_clean(dev: &MemDevice) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("img");
    std::fs::write(&p, dev.snapshot()).unwrap();
    let sbin = std::env::var("E2FSPROGS_SBIN").unwrap_or_else(|_| "/opt/homebrew/opt/e2fsprogs/sbin".into());
    let out = Command::new(format!("{sbin}/e2fsck"))
        .arg("-fn")
        .arg(&p)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Shrink the in-memory journal so every commit is too large for it and
/// takes the in-place path.
fn shrink_journal(fs: &mut Fs) {
    let j = fs.journal.as_mut().unwrap();
    let first = j.sb.first();
    j.sb.set_max_len(first + 3);
}

#[test]
fn oversized_commits_write_in_place_consistently() {
    let dev = mkfs(32, &["-t", "ext4", "-b", "1024"]);
    let mut fs = mount(dev.clone());
    shrink_journal(&mut fs);
    let root = fs.root();
    for i in 0..30 {
        let a = fs
            .create(root, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
            .unwrap();
        fs.write(a.ino, 0, &[i as u8; 5000]).unwrap();
    }
    assert_eq!(fs.journal_commits(), 0, "nothing may go through the journal");
    fs.unmount().unwrap();
    let (code, out) = fsck_clean(&dev);
    assert_eq!(code, 0, "{out}");
    let sb = Fs::probe(&*dev).unwrap();
    assert_ne!(sb.state() & crate::ondisk::superblock::STATE_VALID, 0);
}

#[test]
fn crash_during_in_place_commit_leaves_fs_not_clean() {
    let dev = mkfs(32, &["-t", "ext4", "-b", "1024"]);
    let mut fs = Fs::mount(
        dev.clone(),
        MountOptions {
            commit_threshold: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    shrink_journal(&mut fs);
    let root = fs.root();
    // with the tiny journal every operation commits in place right away;
    // the superblock "not clean" write goes first, then the metadata
    dev.fail_writes_after(Some(2));
    let r = fs.create(root, b"f", FileType::Regular, 0o644, 0, 0, 0);
    assert!(r.is_err(), "{r:?}");
    assert!(fs.is_read_only(), "failed commit aborts the volume");
    std::mem::forget(fs);
    dev.fail_writes_after(None);
    let sb = Fs::probe(&*dev).unwrap();
    assert_eq!(
        sb.state() & crate::ondisk::superblock::STATE_VALID,
        0,
        "crash must force fsck"
    );
}

#[test]
fn rollback_restores_group_counters_and_bitmaps() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = Fs::mount(
        dev,
        MountOptions {
            commit_threshold: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    let root = fs.root();
    let before_sb = fs.sb.raw.clone();
    let before_groups: Vec<_> = fs.groups.clone();
    let dirty_before = fs.cache.dirty_count();
    let r: Result<()> = fs.op(|fs| {
        let a = fs.create(root, b"x", FileType::Regular, 0o644, 0, 0, 0)?;
        fs.write(a.ino, 0, &[7u8; 100_000])?;
        fs.mkdir(root, b"d", 0o755, 0, 0)?;
        Err(Error::NoSpace)
    });
    assert!(r.is_err());
    assert_eq!(fs.sb.raw, before_sb);
    assert_eq!(fs.groups, before_groups);
    assert_eq!(fs.cache.dirty_count(), dirty_before);
    assert!(fs.lookup(root, b"x").is_err());
    assert!(fs.lookup(root, b"d").is_err());
    fs.unmount().unwrap();
}

/// A commit in the middle of an operation (ensure_space) must not switch
/// off rollback for the rest of the operation.
#[test]
fn rollback_after_mid_operation_commit() {
    let dev = mkfs(32, &["-t", "ext4"]);
    let mut fs = Fs::mount(
        dev.clone(),
        MountOptions {
            commit_threshold: usize::MAX,
            ..Default::default()
        },
    )
    .unwrap();
    let root = fs.root();
    let r: Result<()> = fs.op(|fs| {
        fs.create(root, b"before", FileType::Regular, 0o644, 0, 0, 0)?;
        fs.commit()?; // what ensure_space does when space is tight
        let a = fs.create(root, b"after", FileType::Regular, 0o644, 0, 0, 0)?;
        fs.write(a.ino, 0, &[9u8; 50_000])?;
        Err(Error::NoSpace)
    });
    assert!(r.is_err());
    assert!(fs.lookup(root, b"before").is_ok(), "committed part stays");
    assert!(fs.lookup(root, b"after").is_err(), "uncommitted part is rolled back");
    fs.unmount().unwrap();
    let (code, out) = fsck_clean(&dev);
    assert_eq!(code, 0, "{out}");
}
