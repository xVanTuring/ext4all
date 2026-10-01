//! Regression tests for issues found in code review. Each test reproduces
//! the original failure scenario.

mod common;
use common::*;
use ext4_core::{Error, FileType, Fs, MemDevice, MountOptions, RenameFlags, XattrSetMode};
use std::sync::Arc;

fn load(img: &Image) -> Arc<MemDevice> {
    Arc::new(MemDevice::from_vec(std::fs::read(&img.path).unwrap()))
}

fn save(img: &Image, dev: &MemDevice) {
    std::fs::write(&img.path, dev.snapshot()).unwrap();
}

fn clean(img: &Image, what: &str) {
    let (code, out) = img.fsck();
    assert!(
        code == 0 && !out.contains("Fix? no"),
        "{what}: e2fsck exit {code}\n{out}"
    );
}

/// Read-write mounts upgrade the journal like the kernel does.
#[test]
fn rw_mount_enables_journal_64bit_and_csum_v3() {
    let img = Image::new(32, &["-t", "ext4"]);
    assert!(img.dumpe2fs().contains("Journal features:         (none)"));
    img.mount().unmount().unwrap();
    let d = img.dumpe2fs();
    let line = d.lines().find(|l| l.starts_with("Journal features:")).unwrap();
    assert!(
        line.contains("journal_64bit") && line.contains("journal_checksum_v3"),
        "{line}"
    );
    clean(&img, "after upgrade");
    // no metadata_csum: 64bit only
    let img = Image::new(32, &["-t", "ext4", "-O", "^metadata_csum,^metadata_csum_seed"]);
    img.mount().unmount().unwrap();
    let d = img.dumpe2fs();
    let line = d.lines().find(|l| l.starts_with("Journal features:")).unwrap();
    assert!(line.contains("journal_64bit") && !line.contains("checksum"), "{line}");
}

/// A journaled block starting with the jbd2 magic (escaped in the log)
/// must replay under e2fsck's (= the kernel's) checksum rules.
#[test]
fn escaped_block_replays_with_checksum_v3() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    img.mount().unmount().unwrap(); // upgrade journal to csum v3
    let mut target = vec![0xC0u8, 0x3B, 0x39, 0x98];
    target.extend(std::iter::repeat_n(b'x', 200));
    for fail_after in [4usize, 5, 6, 8] {
        let c = img.copy();
        let dev = load(&c);
        let mut fs = Fs::mount(
            dev.clone(),
            MountOptions {
                commit_threshold: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        fs.symlink(2, b"magic-link", &target, 0, 0).unwrap();
        dev.fail_writes_after(Some(fail_after));
        let _ = fs.commit();
        std::mem::forget(fs);
        dev.fail_writes_after(None);
        save(&c, &dev);
        let e = c.copy();
        let (code, out) = e.fsck_fix();
        assert!(
            code <= 1 && !out.contains("Invalid checksum"),
            "fail_after {fail_after}\n{out}"
        );
        assert!(!out.contains("Fix? yes"), "fail_after {fail_after}\n{out}");
        clean(&e, "after e2fsck replay");
        let mut fs = c.mount();
        if let Ok(ino) = fs.lookup(2, b"magic-link") {
            assert_eq!(fs.read_link(ino).unwrap(), target);
        }
        fs.unmount().unwrap();
        clean(&c, "after our replay");
    }
}

/// A failed commit aborts the volume: no further writes, and the
/// committed transaction is recovered on the next mount.
#[test]
fn failed_commit_aborts_and_recovers() {
    let img = Image::new(32, &["-t", "ext4"]);
    // write failures during the commit (journal superblock, log, commit
    // block) and during the checkpoint that follows it in sync()
    for fail_after in [0usize, 1, 2, 3, 4, 5] {
        let c = img.copy();
        let dev = load(&c);
        let mut fs = Fs::mount(
            dev.clone(),
            MountOptions {
                commit_threshold: usize::MAX,
                ..Default::default()
            },
        )
        .unwrap();
        let d = fs.mkdir(2, b"dir", 0o755, 0, 0).unwrap().ino;
        for i in 0..20 {
            fs.create(d, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                .unwrap();
        }
        dev.fail_writes_after(Some(fail_after));
        assert!(fs.sync().is_err(), "fail_after {fail_after}");
        dev.fail_writes_after(None);
        // the device works again, but the volume must stay aborted
        assert!(fs.is_read_only());
        assert!(fs.commit().is_err());
        assert!(matches!(
            fs.create(2, b"after", FileType::Regular, 0o644, 0, 0, 0),
            Err(Error::ReadOnly)
        ));
        assert!(fs.unmount_in_place().is_err());
        drop(fs);
        save(&c, &dev);
        let e = c.copy();
        let (code, out) = e.fsck_fix();
        assert!(code <= 1 && !out.contains("Fix? yes"), "fail_after {fail_after}\n{out}");
        let fs = c.mount();
        fs.unmount().unwrap();
        clean(&c, "after recovery");
    }
}

/// Create/Replace checks happen before anything is modified.
#[test]
fn xattr_create_on_existing_changes_nothing() {
    let img = Image::new(32, &["-t", "ext4", "-I", "128"]);
    {
        let mut fs = img.mount();
        let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.set_xattr(f, b"user.a", b"1", XattrSetMode::Any).unwrap();
        fs.set_xattr(f, b"user.b", b"2", XattrSetMode::Any).unwrap();
        assert!(matches!(
            fs.set_xattr(f, b"user.a", b"x", XattrSetMode::Create),
            Err(Error::Exists)
        ));
        assert!(matches!(
            fs.set_xattr(f, b"user.z", b"x", XattrSetMode::Replace),
            Err(Error::NoAttr)
        ));
        assert!(matches!(
            fs.set_xattr(f, b"user.big", &vec![0u8; 5000], XattrSetMode::Any),
            Err(Error::NoSpace)
        ));
        assert_eq!(fs.get_xattr(f, b"user.a").unwrap(), b"1");
        assert_eq!(fs.get_xattr(f, b"user.b").unwrap(), b"2");
        // moving a value between the inode body and the block
        fs.set_xattr(f, b"user.a", &pattern(600, 1), XattrSetMode::Replace)
            .unwrap();
        assert_eq!(fs.get_xattr(f, b"user.a").unwrap(), pattern(600, 1));
        fs.unmount().unwrap();
    }
    clean(&img, "xattr create on existing");
    let img = Image::new(32, &["-t", "ext4", "-I", "256"]);
    {
        let mut fs = img.mount();
        let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.set_xattr(f, b"user.big", &pattern(600, 2), XattrSetMode::Any)
            .unwrap();
        fs.set_xattr(f, b"user.small", b"s", XattrSetMode::Any).unwrap();
        // shrinking a block value so it now fits the inode body
        fs.set_xattr(f, b"user.big", b"tiny", XattrSetMode::Replace).unwrap();
        assert_eq!(fs.get_xattr(f, b"user.big").unwrap(), b"tiny");
        let mut names = fs.list_xattr(f).unwrap();
        names.sort();
        assert_eq!(names, vec![b"user.big".to_vec(), b"user.small".to_vec()]);
        fs.unmount().unwrap();
    }
    clean(&img, "xattr move between storages");
}

fn fill(fs: &mut Fs, name: &[u8]) {
    let f = fs.create(2, name, FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let chunk = pattern(1 << 20, 3);
    let mut off = 0u64;
    loop {
        match fs.write(f, off, &chunk) {
            Ok(n) if n == chunk.len() => off += n as u64,
            _ => break,
        }
    }
    // squeeze the last blocks
    let small = pattern(1024, 4);
    loop {
        let size = fs.stat(f).unwrap().size;
        match fs.write(f, size, &small) {
            Ok(n) if n > 0 => {}
            _ => break,
        }
    }
}

/// Operations failing with ENOSPC leave no half-done changes behind.
#[test]
fn enospc_rolls_back_every_operation() {
    let img = Image::new(16, &["-t", "ext4", "-b", "1024"]);
    {
        let mut fs = img.mount();
        let big = fs.create(2, b"pre", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fill(&mut fs, b"filler");
        fs.sync().unwrap();
        let free_before = fs.statfs();
        // fallocate far beyond the free space
        assert!(matches!(fs.fallocate(big, 0, 64 << 20, false), Err(Error::NoSpace)));
        assert_eq!(fs.stat(big).unwrap().size, 0);
        assert_eq!(fs.stat(big).unwrap().allocated, 0);
        // a directory needs a block: mkdir fails cleanly
        let r = fs.mkdir(2, b"newdir", 0o755, 0, 0);
        assert!(matches!(r, Err(Error::NoSpace)), "{r:?}");
        assert!(matches!(fs.lookup(2, b"newdir"), Err(Error::NotFound)));
        // slow symlinks need a block
        let r = fs.symlink(2, b"slow", &[b'a'; 300], 0, 0);
        assert!(matches!(r, Err(Error::NoSpace)), "{r:?}");
        assert!(matches!(fs.lookup(2, b"slow"), Err(Error::NotFound)));
        // xattr blocks need a block
        let r = fs.set_xattr(big, b"user.x", &pattern(800, 5), XattrSetMode::Any);
        assert!(matches!(r, Err(Error::NoSpace)), "{r:?}");
        // many creates until the root directory cannot grow
        let mut made = 0;
        for i in 0..5000 {
            let n = format!("a-long-file-name-to-fill-directory-blocks-{i:05}");
            match fs.create(2, n.as_bytes(), FileType::Regular, 0o644, 0, 0, 0) {
                Ok(_) => made += 1,
                Err(Error::NoSpace) => break,
                Err(e) => panic!("{e:?}"),
            }
        }
        let after = fs.statfs();
        assert_eq!(after.free_files, free_before.free_files - made);
        fs.unmount().unwrap();
    }
    clean(&img, "after ENOSPC operations");
}

/// Files may not reach logical block 0xFFFFFFFF.
#[test]
fn file_size_limit_matches_linux() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    let mut fs = img.mount();
    let f = fs.create(2, b"f", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let last = (u32::MAX as u64 - 1) * 4096;
    fs.write(f, last, b"ok").unwrap();
    assert!(matches!(fs.write(f, u32::MAX as u64 * 4096, b"x"), Err(Error::TooBig)));
    assert!(matches!(fs.truncate(f, u32::MAX as u64 * 4096 + 1), Err(Error::TooBig)));
    fs.unmount().unwrap();
    clean(&img, "near the size limit");
}

/// ".." resolves in htree directories and directory moves into large
/// directories work.
#[test]
fn dotdot_in_htree_directory() {
    let img = Image::new(64, &["-t", "ext4", "-b", "1024"]);
    let mut fs = img.mount();
    let big = fs.mkdir(2, b"big", 0o755, 0, 0).unwrap().ino;
    for i in 0..300 {
        fs.create(
            big,
            format!("entry-{i:04}").as_bytes(),
            FileType::Regular,
            0o644,
            0,
            0,
            0,
        )
        .unwrap();
    }
    fs.check_htree(big).unwrap();
    assert_eq!(fs.lookup(big, b"..").unwrap(), 2);
    assert_eq!(fs.lookup(big, b".").unwrap(), big);
    let x = fs.mkdir(2, b"x", 0o755, 0, 0).unwrap().ino;
    let sub = fs.mkdir(x, b"sub", 0o755, 0, 0).unwrap().ino;
    fs.mkdir(sub, b"inner", 0o755, 0, 0).unwrap();
    fs.rename(x, b"sub", big, b"sub", RenameFlags::default()).unwrap();
    assert_eq!(fs.lookup(sub, b"..").unwrap(), big);
    // and back out, below another large directory
    let deep = fs.lookup(sub, b"inner").unwrap();
    assert!(matches!(
        fs.rename(big, b"sub", deep, b"loop", RenameFlags::default()),
        Err(Error::Invalid(_))
    ));
    fs.unmount().unwrap();
    clean(&img, "htree dotdot");
}

/// Orphan-file blocks processed at mount keep a valid checksum.
#[test]
fn orphan_file_entries_are_processed_with_valid_checksum() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    // create an unlinked inode and register it in the orphan file by hand
    let data = img.dir.path().join("payload");
    std::fs::write(&data, pattern(20_000, 9)).unwrap();
    img.debugfs_w(&[&format!("write {} victim", data.display())]);
    let out = img.debugfs(&["stat /victim", "stat <12>"]);
    let ino: u32 = out
        .split("Inode: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let oblk: u64 = {
        // first data block of the orphan file (inode 12 on mke2fs 1.47)
        let s = img.debugfs(&["blocks <12>"]);
        s.split_whitespace()
            .rev()
            .find_map(|w| w.parse().ok())
            .expect("orphan file block")
    };
    img.debugfs_w(&["unlink /victim", &format!("sif <{ino}> links_count 0")]);
    {
        // put the inode number into slot 0 of the orphan block and set
        // orphan_present; the block checksum is fixed up by debugfs? no:
        // write it ourselves with the kernel formula via a raw edit
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img.path)
            .unwrap();
        let mut blk = vec![0u8; 4096];
        f.read_exact_at(&mut blk, oblk * 4096).unwrap();
        blk[0..4].copy_from_slice(&ino.to_le_bytes());
        f.write_all_at(&blk, oblk * 4096).unwrap();
    }
    img.debugfs_w(&["feature orphan_present"]);
    // the checksum of that block is now stale; e2fsck would complain, but
    // our mount must process the entry and write a correct checksum
    let fs = img.mount_opts(MountOptions {
        strict_checksums: false,
        ..Default::default()
    });
    assert_eq!(fs.mount_report().orphans_processed, 1);
    fs.unmount().unwrap();
    clean(&img, "after orphan file processing");
    assert!(!img.dumpe2fs().contains("orphan_present"));
}

/// Deleting a file spanning many groups with the smallest journal stays
/// consistent (the forced in-place path is tested in fs/tests.rs).
#[test]
fn large_unlink_with_tiny_journal_is_consistent() {
    // smallest journal mke2fs allows
    let img = Image::new(64, &["-t", "ext4", "-b", "1024", "-J", "size=1"]);
    {
        let mut fs = img.mount_opts(MountOptions {
            commit_threshold: usize::MAX,
            ..Default::default()
        });
        // one unlink of a file spanning many groups dirties many bitmaps
        let f = fs.create(2, b"wide", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        let chunk = pattern(1 << 20, 1);
        for i in 0..40u64 {
            fs.write(f, i << 20, &chunk).unwrap();
        }
        fs.sync().unwrap();
        fs.unlink(2, b"wide").unwrap();
        fs.sync().unwrap();
        fs.unmount().unwrap();
    }
    clean(&img, "after oversized transaction");
    assert!(img.dumpe2fs().contains("Filesystem state:         clean"));
}

fn collect_from(fs: &mut Fs, dir: u32, cookie: u64, limit: usize) -> (Vec<Vec<u8>>, u64) {
    let mut names = Vec::new();
    let mut last = cookie;
    fs.read_dir(dir, cookie, |e| {
        if names.len() >= limit {
            return false;
        }
        last = e.next_cookie;
        names.push(e.name);
        true
    })
    .unwrap();
    (names, last)
}

/// Unchanged entries are returned exactly once even when htree leaves
/// split while a directory is being enumerated.
#[test]
fn readdir_stable_across_htree_splits() {
    let img = Image::new(64, &["-t", "ext4", "-b", "1024"]);
    let mut fs = img.mount();
    let d = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    let orig: Vec<Vec<u8>> = (0..300)
        .map(|i| format!("original-entry-{i:04}").into_bytes())
        .collect();
    for n in &orig {
        fs.create(d, n, FileType::Regular, 0o644, 0, 0, 0).unwrap();
    }
    for stop in [1usize, 50, 100, 250] {
        let (mut seen, cookie) = collect_from(&mut fs, d, 0, stop);
        // another client adds many entries, splitting leaves
        for i in 0..200 {
            fs.create(
                d,
                format!("new-{stop}-{i:04}").as_bytes(),
                FileType::Regular,
                0o644,
                0,
                0,
                0,
            )
            .unwrap();
        }
        let (rest, _) = collect_from(&mut fs, d, cookie, usize::MAX);
        seen.extend(rest);
        for n in &orig {
            let c = seen.iter().filter(|s| *s == n).count();
            assert_eq!(c, 1, "stop {stop}: {} seen {c} times", String::from_utf8_lossy(n));
        }
        let mut sorted = seen.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "stop {stop}: duplicates");
    }
    // complete listing equals the directory contents
    let all = fs.list_dir(d).unwrap();
    assert_eq!(all.len(), 2 + 300 + 4 * 200);
    fs.unmount().unwrap();
    clean(&img, "readdir stability");
}

/// A byte-offset cookie from before a directory became an htree is
/// reported as stale instead of producing skips or duplicates.
#[test]
fn stale_linear_cookie_after_conversion() {
    let img = Image::new(32, &["-t", "ext4", "-b", "1024"]);
    let mut fs = img.mount();
    let d = fs.mkdir(2, b"d", 0o755, 0, 0).unwrap().ino;
    for i in 0..5 {
        fs.create(d, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
            .unwrap();
    }
    let (_, cookie) = collect_from(&mut fs, d, 0, 4);
    assert!(cookie < 1 << 63);
    for i in 0..200 {
        fs.create(
            d,
            format!("grow-{i:04}-padding-padding").as_bytes(),
            FileType::Regular,
            0o644,
            0,
            0,
            0,
        )
        .unwrap();
    }
    let r = fs.read_dir(d, cookie, |_| true);
    assert!(matches!(r, Err(Error::StaleCookie)), "{r:?}");
    assert_eq!(Error::StaleCookie.errno(), 70);
    fs.unmount().unwrap();
}

/// Superblock free counts are recomputed from the group descriptors.
#[test]
fn bogus_superblock_free_count_is_recomputed() {
    let img = Image::new(8, &["-t", "ext4"]);
    img.debugfs_w(&["ssv free_blocks_count 9999999999", "ssv free_inodes_count 999999"]);
    let fs = img.mount_opts(MountOptions {
        read_only: true,
        ..Default::default()
    });
    let s = fs.statfs();
    assert!(s.free_blocks <= s.blocks && s.free_files <= s.files, "{s:?}");
    drop(fs);
    let fs = img.mount();
    fs.unmount().unwrap();
    clean(&img, "recomputed counts written back");
}

/// Block pointers into metadata are rejected instead of being written to
/// or freed.
#[test]
fn pointers_into_metadata_are_refused() {
    for (fsopts, name) in [
        (&["-t", "ext3", "-b", "1024"][..], "ext3"),
        (&["-t", "ext4", "-b", "1024"][..], "ext4"),
    ] {
        let img = Image::new(16, fsopts);
        let data = img.dir.path().join("data");
        std::fs::write(&data, pattern(3000, 1)).unwrap();
        img.debugfs_w(&[&format!("write {} victim", data.display())]);
        let out = img.debugfs(&["stat /victim"]);
        let ino: u32 = out
            .split("Inode: ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        if name == "ext3" {
            // direct pointer 0 -> group descriptor block
            img.debugfs_w(&[&format!("sif <{ino}> block[0] 2")]);
        } else {
            // first extent -> group descriptor block (i_block[3..] holds
            // the extent: ee_block, ee_len|hi, ee_start_lo)
            img.debugfs_w(&[&format!("sif <{ino}> block[5] 2")]);
        }
        let before = std::fs::read(&img.path).unwrap();
        {
            let mut fs = img.mount_opts(MountOptions {
                strict_checksums: false,
                ..Default::default()
            });
            let f = fs.lookup(2, b"victim").unwrap();
            let mut b = vec![0u8; 100];
            assert!(matches!(fs.read(f, 0, &mut b), Err(Error::Corrupt(_))), "{name}: read");
            assert!(
                matches!(fs.write(f, 0, b"overwrite"), Err(Error::Corrupt(_))),
                "{name}: write"
            );
            assert!(matches!(fs.truncate(f, 0), Err(Error::Corrupt(_))), "{name}: truncate");
            assert!(
                fs.unlink(2, b"victim").is_err(),
                "{name}: unlink must not free metadata"
            );
            fs.unmount().unwrap();
        }
        // the group descriptor block itself was never touched
        let after = std::fs::read(&img.path).unwrap();
        assert_eq!(&before[2048..3072], &after[2048..3072], "{name}: GDT modified");
    }
}

/// bigalloc descriptors count free clusters; the recount at mount must
/// convert them to blocks (statfs used to report 1/ratio of the space).
#[test]
fn bigalloc_free_counts_are_in_blocks() {
    for (cluster, bs) in [("16384", "4096"), ("65536", "4096"), ("4096", "1024")] {
        let img = Image::new(128, &["-t", "ext4", "-O", "bigalloc", "-b", bs, "-C", cluster]);
        let dump = img.dumpe2fs();
        let field = |name: &str| -> u64 {
            dump.lines()
                .find_map(|l| l.strip_prefix(name))
                .unwrap_or_else(|| panic!("{name} missing"))
                .trim()
                .parse()
                .unwrap()
        };
        let fs = img.mount_ro();
        let s = fs.statfs();
        assert_eq!(s.free_blocks, field("Free blocks:"), "cluster {cluster}");
        assert_eq!(s.blocks, field("Block count:"));
        assert!(
            s.avail_blocks > 0 && s.avail_blocks <= s.free_blocks,
            "cluster {cluster}: {s:?}"
        );
    }
}

/// Group 0's metadata locations from dumpe2fs: (block bitmap, inode
/// bitmap, inode table).
fn group0_metadata(img: &Image) -> (u64, u64, u64) {
    let out = std::process::Command::new(tool("dumpe2fs"))
        .arg(&img.path)
        .output()
        .unwrap();
    let dump = String::from_utf8_lossy(&out.stdout).into_owned();
    let g0 = dump.split("Group 0:").nth(1).unwrap().split("Group 1:").next().unwrap();
    let at = |label: &str| -> u64 {
        let rest = g0.split(label).nth(1).unwrap_or_else(|| panic!("{label} missing"));
        rest.trim_start()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    (at("Block bitmap at"), at("Inode bitmap at"), at("Inode table at"))
}

/// Two structures sharing blocks (a group claiming another group's inode
/// table or bitmap, a bitmap inside an inode table, a bitmap on the
/// descriptor table) would let one overwrite the other: mounting must fail
/// like Linux's system zone check, read-only as well.
#[test]
fn overlapping_metadata_is_rejected_at_mount() {
    for csum in [true, false] {
        let base: &[&str] = if csum {
            &["-t", "ext4", "-b", "1024"]
        } else {
            &["-t", "ext4", "-b", "1024", "-O", "^metadata_csum,uninit_bg"]
        };
        let pristine = Image::new(64, base);
        let (bb, ib, it) = group0_metadata(&pristine);
        let cases = [
            format!("set_bg 1 inode_table {it}"),
            format!("set_bg 1 block_bitmap {bb}"),
            format!("set_bg 1 inode_bitmap {ib}"),
            format!("set_bg 1 block_bitmap {}", it + 3),
            format!("set_bg 0 inode_bitmap {bb}"),
            "set_bg 0 block_bitmap 2".to_string(), // the descriptor table
        ];
        for cmd in &cases {
            let img = pristine.copy();
            let group = cmd.split_whitespace().nth(1).unwrap();
            img.debugfs_w(&[cmd, &format!("set_bg {group} checksum calc")]);
            for ro in [false, true] {
                let opts = MountOptions {
                    read_only: ro,
                    ..MountOptions::default()
                };
                match Fs::mount(img.device(ro), opts) {
                    Err(Error::Corrupt(msg)) => assert!(msg.contains("overlaps"), "{cmd}: {msg}"),
                    Err(e) => panic!("csum {csum} {cmd}: unexpected error {e:?}"),
                    Ok(_) => panic!("csum {csum} {cmd}: mounted (read-only {ro})"),
                }
            }
        }
        // the untouched image still mounts
        pristine.mount().unmount().unwrap();
    }
}
