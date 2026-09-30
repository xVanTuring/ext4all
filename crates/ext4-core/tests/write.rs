//! Write path: every test modifies an image through ext4-core, unmounts,
//! and requires `e2fsck -fn` to find nothing wrong.

mod common;
use common::*;
use ext4_core::{Error, FileType, Fs, MountOptions, RenameFlags, SetAttr, Timestamp, XattrSetMode};

fn read_all(fs: &mut Fs, ino: u32) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = fs.read(ino, done as u64, &mut buf[done..]).unwrap();
        assert!(n > 0);
        done += n;
    }
    buf
}

fn mkfile(fs: &mut Fs, dir: u32, name: &str, data: &[u8]) -> u32 {
    let a = fs
        .create(dir, name.as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
        .unwrap();
    if !data.is_empty() {
        assert_eq!(fs.write(a.ino, 0, data).unwrap(), data.len());
    }
    a.ino
}

#[test]
fn mount_rw_and_unmount_is_clean() {
    for (name, opts) in PROFILES {
        let img = Image::new(32, opts);
        let fs = img.mount();
        assert!(!fs.is_read_only(), "{name}");
        fs.unmount().unwrap();
        img.assert_clean();
        let d = img.dumpe2fs();
        assert!(!d.contains("needs_recovery"), "{name}: {d}");
    }
}

#[test]
fn needs_recovery_set_while_mounted() {
    let img = Image::new(32, &["-t", "ext4"]);
    let fs = img.mount();
    let d = img.dumpe2fs();
    assert!(d.contains("needs_recovery"), "{d}");
    fs.unmount().unwrap();
    assert!(!img.dumpe2fs().contains("needs_recovery"));
}

#[test]
fn create_write_read_back() {
    let img = Image::new(64, &["-t", "ext4"]);
    let data = pattern(123_457, 9);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let ino = mkfile(&mut fs, root, "file.bin", &data);
        assert_eq!(read_all(&mut fs, ino), data);
        assert_eq!(fs.stat(ino).unwrap().size, data.len() as u64);
        fs.unmount().unwrap();
    }
    img.assert_clean();
    assert_eq!(img.debugfs_cat("/file.bin"), data);
    let mut fs = img.mount_ro();
    let ino = fs.resolve("/file.bin").unwrap();
    assert_eq!(read_all(&mut fs, ino), data);
}

fn scenario(fs: &mut Fs) {
    let root = fs.root();
    let d1 = fs.mkdir(root, b"d1", 0o755, 1000, 1000).unwrap().ino;
    let d2 = fs.mkdir(d1, b"d2", 0o700, 1000, 1000).unwrap().ino;
    mkfile(fs, root, "small", b"tiny");
    mkfile(fs, d1, "medium", &pattern(50_000, 1));
    mkfile(fs, d2, "large", &pattern(2_000_000, 2));
    mkfile(fs, root, "empty", b"");
    fs.symlink(root, b"sl-short", b"d1/medium", 0, 0).unwrap();
    fs.symlink(root, b"sl-long", "l".repeat(300).as_bytes(), 0, 0).unwrap();
    let small = fs.lookup(root, b"small").unwrap();
    fs.link(small, d2, b"small-link").unwrap();
    fs.create(root, b"fifo", FileType::Fifo, 0o600, 0, 0, 0).unwrap();
    fs.create(root, b"chr", FileType::CharDev, 0o600, 0, 0, (4 << 24) | 64)
        .unwrap();
    fs.create(root, b"blk", FileType::BlockDev, 0o600, 0, 0, (300 << 24) | 70000)
        .unwrap();
    fs.create(root, b"sock", FileType::Socket, 0o600, 0, 0, 0).unwrap();
    fs.rename(root, b"small", d1, b"small-moved", RenameFlags::default())
        .unwrap();
    let tmp = mkfile(fs, root, "tmp", &pattern(10_000, 3));
    fs.unlink(root, b"tmp").unwrap();
    assert!(matches!(fs.stat(tmp), Err(Error::NotFound)));
    let gone = fs.mkdir(root, b"gone", 0o755, 0, 0).unwrap().ino;
    fs.rmdir(root, b"gone").unwrap();
    assert!(fs.stat(gone).is_err());
    let m = fs.lookup(d1, b"medium").unwrap();
    fs.truncate(m, 12_345).unwrap();
    fs.set_xattr(m, b"user.color", b"blue", XattrSetMode::Any).unwrap();
    // larger than the in-inode area, smaller than a 1K block
    fs.set_xattr(m, b"user.big", &pattern(900, 4), XattrSetMode::Any)
        .unwrap();
}

fn verify_scenario(img: &Image) {
    assert_eq!(img.debugfs_cat("/d1/small-moved"), b"tiny");
    assert_eq!(img.debugfs_cat("/d1/d2/small-link"), b"tiny");
    assert_eq!(img.debugfs_cat("/d1/medium"), pattern(50_000, 1)[..12_345].to_vec());
    assert_eq!(img.debugfs_cat("/d1/d2/large"), pattern(2_000_000, 2));
    let names: Vec<String> = img.debugfs_ls("/").into_iter().map(|(n, _)| n).collect();
    for n in ["d1", "empty", "sl-short", "sl-long", "fifo", "chr", "blk", "sock"] {
        assert!(names.contains(&n.to_string()), "{n} missing in {names:?}");
    }
    for n in ["small", "tmp", "gone"] {
        assert!(!names.contains(&n.to_string()), "{n} still in {names:?}");
    }
    let mut fs = img.mount_ro();
    let root = fs.root();
    let sl = fs.lookup(root, b"sl-long").unwrap();
    assert_eq!(fs.read_link(sl).unwrap(), "l".repeat(300).as_bytes());
    let sl = fs.lookup(root, b"sl-short").unwrap();
    assert_eq!(fs.read_link(sl).unwrap(), b"d1/medium");
    let chr = fs.lookup_attr(root, b"chr").unwrap();
    assert_eq!(chr.rdev, (4 << 24) | 64);
    let blk = fs.lookup_attr(root, b"blk").unwrap();
    assert_eq!(blk.rdev, (300 << 24) | 70000);
    let m = fs.resolve("/d1/medium").unwrap();
    assert_eq!(fs.get_xattr(m, b"user.color").unwrap(), b"blue");
    assert_eq!(fs.get_xattr(m, b"user.big").unwrap(), pattern(900, 4));
    let small = fs.resolve("/d1/small-moved").unwrap();
    assert_eq!(fs.stat(small).unwrap().nlink, 2);
    let d1 = fs.resolve("/d1").unwrap();
    assert_eq!(fs.stat(d1).unwrap().nlink, 3);
}

#[test]
fn scenario_all_profiles() {
    for (name, opts) in PROFILES {
        let img = Image::new(64, opts);
        {
            let mut fs = img.mount();
            scenario(&mut fs);
            fs.unmount().unwrap();
        }
        let (code, out) = img.fsck();
        assert!(
            code == 0 && !out.contains("Fix? no"),
            "profile {name}: e2fsck exit {code}\n{out}"
        );
        verify_scenario(&img);
    }
}

#[test]
fn scenario_with_commit_after_every_op() {
    let img = Image::new(64, &["-t", "ext4"]);
    {
        let mut fs = img.mount_opts(MountOptions {
            commit_threshold: 1,
            ..Default::default()
        });
        scenario(&mut fs);
        assert!(fs.journal_commits() > 10);
        fs.unmount().unwrap();
    }
    img.assert_clean();
    verify_scenario(&img);
}

#[test]
fn fragmented_files_grow_extent_tree_depth() {
    // 1K blocks: 84 extents per leaf → > 4*84 extents needs depth 2
    let img = Image::new(64, &["-t", "ext4", "-b", "1024"]);
    let (a, b);
    {
        let mut fs = img.mount();
        let root = fs.root();
        a = mkfile(&mut fs, root, "a", b"");
        b = mkfile(&mut fs, root, "b", b"");
        for i in 0..800u64 {
            let blk = pattern(1024, i);
            fs.write(a, i * 1024, &blk).unwrap();
            fs.write(b, i * 1024, &blk).unwrap();
        }
        let ea = fs.file_extents(a).unwrap();
        assert!(ea.len() > 400, "expected fragmentation, got {} extents", ea.len());
        fs.check_extent_tree(a).unwrap();
        fs.check_extent_tree(b).unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
    let out = img.debugfs(&["stat /a"]);
    assert!(out.contains("Level Entries") || out.contains("(1)"), "{out}");
    let mut expect = Vec::new();
    for i in 0..800u64 {
        expect.extend(pattern(1024, i));
    }
    assert_eq!(img.debugfs_cat("/a"), expect);
    assert_eq!(img.debugfs_cat("/b"), expect);

    // shrink in steps, then delete
    {
        let mut fs = img.mount();
        for size in [700 * 1024 + 5, 300 * 1024, 5 * 1024, 100, 0] {
            fs.truncate(a, size).unwrap();
            fs.check_extent_tree(a).unwrap();
            let got = read_all(&mut fs, a);
            assert_eq!(got, expect[..size as usize].to_vec());
        }
        let root = fs.root();
        fs.unlink(root, b"b").unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn punch_holes_split_extents() {
    let img = Image::new(64, &["-t", "ext4", "-b", "1024"]);
    let data = pattern(400 * 1024, 5);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "f", &data);
        let mut expect = data.clone();
        for i in 0..150u64 {
            let off = i * 2 * 1024 + 1024 + 17;
            fs.punch_hole(f, off, 1024).unwrap();
            for b in &mut expect[off as usize..off as usize + 1024] {
                *b = 0;
            }
        }
        fs.check_extent_tree(f).unwrap();
        assert_eq!(read_all(&mut fs, f), expect);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn many_files_make_htree_directory() {
    for (name, opts) in [
        ("4k", &["-t", "ext4", "-b", "4096"][..]),
        ("1k", &["-t", "ext4", "-b", "1024"][..]),
        ("no-csum", &["-t", "ext4", "-b", "1024", "-O", "^metadata_csum"][..]),
    ] {
        let img = Image::new(128, opts);
        let n = 6000;
        {
            let mut fs = img.mount();
            let root = fs.root();
            let d = fs.mkdir(root, b"big", 0o755, 0, 0).unwrap().ino;
            for i in 0..n {
                let nm = format!("entry-with-a-longish-name-{i:06}");
                fs.create(d, nm.as_bytes(), FileType::Regular, 0o644, 0, 0, 0).unwrap();
            }
            fs.check_htree(d).unwrap();
            for i in (0..n).step_by(97) {
                let nm = format!("entry-with-a-longish-name-{i:06}");
                fs.lookup(d, nm.as_bytes()).unwrap();
            }
            assert_eq!(fs.list_dir(d).unwrap().len(), n + 2);
            fs.unmount().unwrap();
        }
        let (code, out) = img.fsck();
        assert!(code == 0 && !out.contains("Fix? no"), "{name}: exit {code}\n{out}");
        let out = img.debugfs(&["htree_dump /big"]);
        assert!(
            out.contains("Number of entries") || out.contains("Root node"),
            "{name}: {out}"
        );
        // delete half, rename some, verify again
        {
            let mut fs = img.mount();
            let d = fs.resolve("/big").unwrap();
            for i in (0..n).step_by(2) {
                let nm = format!("entry-with-a-longish-name-{i:06}");
                fs.unlink(d, nm.as_bytes()).unwrap();
            }
            for i in (1..n).step_by(10) {
                let a = format!("entry-with-a-longish-name-{i:06}");
                let b = format!("renamed-{i}");
                fs.rename(d, a.as_bytes(), d, b.as_bytes(), RenameFlags::default())
                    .unwrap();
            }
            fs.check_htree(d).unwrap();
            assert_eq!(fs.list_dir(d).unwrap().len(), n / 2 + 2);
            fs.unmount().unwrap();
        }
        let (code, out) = img.fsck();
        assert!(
            code == 0 && !out.contains("Fix? no"),
            "{name} after delete: exit {code}\n{out}"
        );
    }
}

#[test]
fn linear_dir_without_dir_index_grows() {
    let img = Image::new(64, &["-t", "ext4", "-O", "^dir_index", "-b", "1024"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        for i in 0..800 {
            mkfile(&mut fs, root, &format!("f{i}"), b"");
        }
        for i in 0..800 {
            fs.lookup(root, format!("f{i}").as_bytes()).unwrap();
        }
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn fill_disk_gives_enospc_and_stays_consistent() {
    let img = Image::new(16, &["-t", "ext4", "-b", "1024"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "filler", b"");
        let chunk = pattern(256 * 1024, 7);
        let mut off = 0u64;
        loop {
            match fs.write(f, off, &chunk) {
                Ok(n) if n == chunk.len() => off += n as u64,
                Ok(n) => {
                    off += n as u64;
                    break;
                }
                Err(Error::NoSpace) => break,
                Err(e) => panic!("unexpected error {e:?}"),
            }
        }
        assert!(off > 8 * 1024 * 1024, "wrote only {off}");
        assert!(matches!(fs.write(f, off, &chunk), Err(Error::NoSpace) | Ok(0)));
        // deleting makes room again
        fs.unlink(root, b"filler").unwrap();
        fs.sync().unwrap();
        let g = mkfile(&mut fs, root, "after", &pattern(1024 * 1024, 8));
        assert_eq!(fs.stat(g).unwrap().size, 1024 * 1024);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn inode_exhaustion() {
    let img = Image::new(8, &["-t", "ext4", "-N", "64", "-b", "1024"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let mut made = 0;
        loop {
            match fs.create(root, format!("n{made}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0) {
                Ok(_) => made += 1,
                Err(Error::NoSpace) => break,
                Err(e) => panic!("{e:?}"),
            }
        }
        assert!(made > 40, "{made}");
        assert_eq!(fs.statfs().free_files, 0);
        fs.unlink(root, b"n0").unwrap();
        fs.create(root, b"again", FileType::Regular, 0o644, 0, 0, 0).unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn rename_semantics() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let a = fs.mkdir(root, b"a", 0o755, 0, 0).unwrap().ino;
        let b = fs.mkdir(root, b"b", 0o755, 0, 0).unwrap().ino;
        let sub = fs.mkdir(a, b"sub", 0o755, 0, 0).unwrap().ino;
        mkfile(&mut fs, sub, "inner", b"x");
        // dir across parents: .. updated, link counts move
        fs.rename(a, b"sub", b, b"sub2", RenameFlags::default()).unwrap();
        assert_eq!(fs.lookup(sub, b"..").unwrap(), b);
        assert_eq!(fs.stat(a).unwrap().nlink, 2);
        assert_eq!(fs.stat(b).unwrap().nlink, 3);
        // into own subtree
        assert!(matches!(
            fs.rename(root, b"b", sub, b"loop", RenameFlags::default()),
            Err(Error::Invalid(_))
        ));
        // replace file
        mkfile(&mut fs, root, "f1", b"one");
        mkfile(&mut fs, root, "f2", b"two");
        fs.rename(root, b"f1", root, b"f2", RenameFlags::default()).unwrap();
        let f2 = fs.lookup(root, b"f2").unwrap();
        assert_eq!(read_all(&mut fs, f2), b"one");
        assert!(fs.lookup(root, b"f1").is_err());
        // no_replace
        mkfile(&mut fs, root, "f3", b"three");
        assert!(matches!(
            fs.rename(
                root,
                b"f3",
                root,
                b"f2",
                RenameFlags {
                    no_replace: true,
                    exchange: false
                }
            ),
            Err(Error::Exists)
        ));
        // exchange file <-> dir across parents
        fs.rename(
            root,
            b"f3",
            b,
            b"sub2",
            RenameFlags {
                no_replace: false,
                exchange: true,
            },
        )
        .unwrap();
        assert_eq!(fs.lookup(root, b"f3").unwrap(), sub);
        assert_eq!(fs.lookup(sub, b"..").unwrap(), root);
        assert_eq!(fs.stat(b).unwrap().nlink, 2);
        // replace empty dir with dir
        let e1 = fs.mkdir(root, b"e1", 0o755, 0, 0).unwrap().ino;
        fs.mkdir(b, b"e2", 0o755, 0, 0).unwrap();
        fs.rename(root, b"e1", b, b"e2", RenameFlags::default()).unwrap();
        assert_eq!(fs.lookup(b, b"e2").unwrap(), e1);
        // replacing non-empty dir fails
        fs.mkdir(root, b"x", 0o755, 0, 0).unwrap();
        assert!(matches!(
            fs.rename(root, b"x", root, b"f3", RenameFlags::default()),
            Err(Error::NotEmpty)
        ));
        // dir over file / file over dir
        assert!(matches!(
            fs.rename(root, b"x", root, b"f2", RenameFlags::default()),
            Err(Error::NotDir)
        ));
        assert!(matches!(
            fs.rename(root, b"f2", root, b"x", RenameFlags::default()),
            Err(Error::IsDir)
        ));
        // hard links to the same inode: no-op
        let f2 = fs.lookup(root, b"f2").unwrap();
        fs.link(f2, root, b"f2-link").unwrap();
        fs.rename(root, b"f2", root, b"f2-link", RenameFlags::default())
            .unwrap();
        assert!(fs.lookup(root, b"f2").is_ok());
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn error_cases() {
    let img = Image::new(32, &["-t", "ext4"]);
    let mut fs = img.mount();
    let root = fs.root();
    mkfile(&mut fs, root, "f", b"data");
    let f = fs.lookup(root, b"f").unwrap();
    assert!(matches!(
        fs.create(root, b"f", FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::Exists)
    ));
    assert!(matches!(fs.mkdir(root, b"f", 0o755, 0, 0), Err(Error::Exists)));
    assert!(matches!(fs.mkdir(f, b"x", 0o755, 0, 0), Err(Error::NotDir)));
    assert!(matches!(fs.unlink(root, b"nope"), Err(Error::NotFound)));
    assert!(matches!(fs.rmdir(root, b"f"), Err(Error::NotDir)));
    assert!(matches!(fs.unlink(root, b"lost+found"), Err(Error::IsDir)));
    assert!(matches!(
        fs.create(root, &[b'a'; 256], FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::NameTooLong)
    ));
    assert!(matches!(
        fs.create(root, b"a/b", FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        fs.create(root, b".", FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        fs.create(root, b"", FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::Invalid(_))
    ));
    let d = fs.mkdir(root, b"d", 0o755, 0, 0).unwrap().ino;
    mkfile(&mut fs, d, "inner", b"");
    assert!(matches!(fs.rmdir(root, b"d"), Err(Error::NotEmpty)));
    assert!(matches!(fs.write(d, 0, b"x"), Err(Error::IsDir)));
    assert!(matches!(fs.link(d, root, b"dlink"), Err(Error::NotPermitted)));
    assert!(matches!(fs.symlink(root, b"s", b"", 0, 0), Err(Error::Invalid(_))));
    assert!(matches!(fs.read_link(f), Err(Error::Invalid(_))));
    // 255-byte names are fine
    let long = vec![b'n'; 255];
    mkfile(&mut fs, root, std::str::from_utf8(&long).unwrap(), b"ok");
    assert!(fs.lookup(root, &long).is_ok());
    fs.unmount().unwrap();
    img.assert_clean();
}

#[test]
fn read_only_mount_rejects_writes() {
    let img = Image::new(32, &["-t", "ext4"]);
    let mut fs = img.mount_ro();
    let root = fs.root();
    assert!(matches!(
        fs.create(root, b"x", FileType::Regular, 0o644, 0, 0, 0),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(fs.mkdir(root, b"x", 0o755, 0, 0), Err(Error::ReadOnly)));
    assert!(matches!(fs.unlink(root, b"lost+found"), Err(Error::ReadOnly)));
}

#[test]
fn xattr_storage_transitions() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "f", b"");
        // in-inode
        fs.set_xattr(f, b"user.a", b"1", XattrSetMode::Any).unwrap();
        // spills into a block
        for i in 0..30 {
            fs.set_xattr(f, format!("user.k{i}").as_bytes(), &pattern(64, i), XattrSetMode::Any)
                .unwrap();
        }
        assert!(fs.stat(f).unwrap().allocated >= 4096);
        assert!(matches!(
            fs.set_xattr(f, b"user.a", b"2", XattrSetMode::Create),
            Err(Error::Exists)
        ));
        assert!(matches!(
            fs.set_xattr(f, b"user.zz", b"2", XattrSetMode::Replace),
            Err(Error::NoAttr)
        ));
        fs.set_xattr(f, b"user.a", b"replaced", XattrSetMode::Replace).unwrap();
        assert_eq!(fs.get_xattr(f, b"user.a").unwrap(), b"replaced");
        assert!(matches!(
            fs.set_xattr(f, b"user.huge", &vec![0u8; 5000], XattrSetMode::Any),
            Err(Error::NoSpace)
        ));
        fs.remove_xattr(f, b"user.k3").unwrap();
        assert!(matches!(fs.remove_xattr(f, b"user.k3"), Err(Error::NoAttr)));
        assert!(matches!(fs.get_xattr(f, b"user.k3"), Err(Error::NoAttr)));
        let names = fs.list_xattr(f).unwrap();
        assert_eq!(names.len(), 30);
        fs.set_xattr(f, b"security.selinux", b"system_u:object_r:x\0", XattrSetMode::Any)
            .unwrap();
        fs.set_xattr(f, b"trusted.t", b"t", XattrSetMode::Any).unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
    let out = img.debugfs(&["ea_get /f user.k7"]);
    assert!(!out.contains("not found"), "{out}");
    {
        let mut fs = img.mount();
        let f = fs.resolve("/f").unwrap();
        for n in fs.list_xattr(f).unwrap() {
            fs.remove_xattr(f, &n).unwrap();
        }
        assert!(fs.list_xattr(f).unwrap().is_empty());
        assert_eq!(fs.stat(f).unwrap().allocated, 0);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn xattr_on_small_inodes_uses_block() {
    let img = Image::new(32, &["-t", "ext4", "-I", "128"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "f", b"x");
        fs.set_xattr(f, b"user.x", b"y", XattrSetMode::Any).unwrap();
        assert_eq!(fs.get_xattr(f, b"user.x").unwrap(), b"y");
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn deleting_file_with_xattr_block() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "f", b"x");
        fs.set_xattr(f, b"user.big", &pattern(800, 1), XattrSetMode::Any)
            .unwrap();
        fs.unlink(root, b"f").unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn inline_data_files_convert_on_write() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4", "-O", "inline_data"], Some(tree.path()));
    {
        let mut fs = img.mount();
        let root = fs.root();
        let h = fs.lookup(root, b"hello.txt").unwrap();
        fs.write(h, 12, b" appended").unwrap();
        assert_eq!(read_all(&mut fs, h), b"hello, ext4\n appended");
        let d = fs.resolve("/dir").unwrap();
        for i in 0..50 {
            mkfile(&mut fs, d, &format!("new-{i}"), b"z");
        }
        fs.unlink(d, b"a").unwrap();
        let c = fs.resolve("/dir/sub/deeper/c").unwrap();
        fs.truncate(c, 2).unwrap();
        fs.unmount().unwrap();
    }
    img.assert_clean();
    assert_eq!(img.debugfs_cat("/hello.txt"), b"hello, ext4\n appended");
    assert_eq!(img.debugfs_cat("/dir/sub/deeper/c"), b"de");
}

#[test]
fn fallocate_then_write_and_read() {
    let img = Image::new(64, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "pre", b"");
        fs.fallocate(f, 0, 1 << 20, false).unwrap();
        assert_eq!(fs.stat(f).unwrap().size, 1 << 20);
        assert!(read_all(&mut fs, f).iter().all(|&b| b == 0));
        fs.write(f, 5000, b"inside unwritten").unwrap();
        fs.write(f, 700_000, &pattern(10_000, 3)).unwrap();
        let data = read_all(&mut fs, f);
        assert_eq!(&data[5000..5016], b"inside unwritten");
        assert!(data[..5000].iter().all(|&b| b == 0));
        assert!(data[5016..700_000].iter().all(|&b| b == 0));
        assert_eq!(&data[700_000..710_000], &pattern(10_000, 3)[..]);
        fs.check_extent_tree(f).unwrap();
        // keep_size preallocation past EOF
        fs.fallocate(f, 1 << 20, 1 << 20, true).unwrap();
        assert_eq!(fs.stat(f).unwrap().size, 1 << 20);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn sparse_writes_far_offsets() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "sparse", b"");
        fs.write(f, 1 << 30, b"one gig").unwrap();
        fs.write(f, 1 << 40, b"one tera").unwrap();
        let a = fs.stat(f).unwrap();
        assert_eq!(a.size, (1 << 40) + 8);
        assert!(a.allocated <= 3 * 4096);
        let mut b = [0u8; 8];
        fs.read(f, 1 << 40, &mut b).unwrap();
        assert_eq!(&b, b"one tera");
        fs.read(f, 12345, &mut b).unwrap();
        assert_eq!(b, [0; 8]);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn extending_truncate_zeroes_tail() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "t", &[0xFFu8; 4096]);
        fs.truncate(f, 10).unwrap();
        fs.truncate(f, 4096).unwrap();
        let d = read_all(&mut fs, f);
        assert!(d[..10].iter().all(|&b| b == 0xFF));
        assert!(d[10..].iter().all(|&b| b == 0), "stale data exposed");
        // write past EOF also zeroes the old tail
        fs.truncate(f, 20).unwrap();
        fs.write(f, 100, b"x").unwrap();
        let d = read_all(&mut fs, f);
        assert!(d[20..100].iter().all(|&b| b == 0));
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn overwrite_middle_partial_blocks() {
    let img = Image::new(32, &["-t", "ext4"]);
    let mut expect = pattern(50_000, 11);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "o", &expect);
        for (off, len, seed) in [
            (1usize, 10usize, 1u64),
            (4090, 20, 2),
            (8192, 4096, 3),
            (12_000, 9000, 4),
            (49_990, 30, 5),
        ] {
            let d = pattern(len, seed);
            fs.write(f, off as u64, &d).unwrap();
            if off + len > expect.len() {
                expect.resize(off + len, 0);
            }
            expect[off..off + len].copy_from_slice(&d);
        }
        assert_eq!(read_all(&mut fs, f), expect);
        fs.unmount().unwrap();
    }
    img.assert_clean();
    assert_eq!(img.debugfs_cat("/o"), expect);
}

#[test]
fn set_attr_fields() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        let f = mkfile(&mut fs, root, "a", b"x");
        let t = Timestamp::new(1_600_000_000, 123_456_789);
        let a = fs
            .set_attr(
                f,
                &SetAttr {
                    perm: Some(0o4751),
                    uid: Some(70000),
                    gid: Some(80000),
                    atime: Some(t),
                    mtime: Some(Timestamp::new(-5, 7)),
                    crtime: Some(Timestamp::new(3, 4)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(a.perm, 0o4751);
        assert_eq!(a.uid, 70000);
        assert_eq!(a.gid, 80000);
        assert_eq!(a.atime, t);
        assert_eq!(a.mtime, Timestamp::new(-5, 7));
        assert_eq!(a.crtime, Some(Timestamp::new(3, 4)));
        fs.unmount().unwrap();
    }
    img.assert_clean();
    let mut fs = img.mount_ro();
    let a = fs.lookup_attr(2, b"a").unwrap();
    assert_eq!(a.perm, 0o4751);
    assert_eq!(a.uid, 70000);
    assert_eq!(a.atime, Timestamp::new(1_600_000_000, 123_456_789));
    let out = img.debugfs(&["stat /a"]);
    assert!(out.contains("User: 70000"), "{out}");
}

/// Everything written must be readable before it is committed (the cache
/// holds metadata, data goes straight to the device).
#[test]
fn uncommitted_changes_are_visible() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    let mut fs = img.mount_opts(MountOptions {
        commit_threshold: usize::MAX,
        ..Default::default()
    });
    let root = fs.root();
    let long = "t".repeat(3000);
    let s = fs.symlink(root, b"slow", long.as_bytes(), 0, 0).unwrap().ino;
    assert_eq!(fs.read_link(s).unwrap(), long.as_bytes());
    let short = fs.symlink(root, b"fast", b"target", 0, 0).unwrap().ino;
    assert_eq!(fs.read_link(short).unwrap(), b"target");
    let d = fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap().ino;
    let f = mkfile(&mut fs, d, "f", &pattern(70_000, 1));
    assert_eq!(read_all(&mut fs, f), pattern(70_000, 1));
    fs.set_xattr(f, b"user.k", &pattern(700, 2), XattrSetMode::Any).unwrap();
    assert_eq!(fs.get_xattr(f, b"user.k").unwrap(), pattern(700, 2));
    assert_eq!(fs.lookup(d, b"f").unwrap(), f);
    assert!(fs.has_pending_changes());
    fs.unmount().unwrap();
    img.assert_clean();
    assert_eq!(img.debugfs_cat("/dir/f"), pattern(70_000, 1));
}

#[test]
fn write_clears_setuid() {
    let img = Image::new(32, &["-t", "ext4"]);
    let mut fs = img.mount();
    let root = fs.root();
    let f = fs.create(root, b"s", FileType::Regular, 0o6755, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, b"x").unwrap();
    assert_eq!(fs.stat(f).unwrap().perm & 0o6000, 0);
}

#[test]
fn setgid_directory_inheritance() {
    let img = Image::new(32, &["-t", "ext4"]);
    let mut fs = img.mount();
    let root = fs.root();
    let d = fs.mkdir(root, b"g", 0o2775, 0, 500).unwrap().ino;
    let f = fs.create(d, b"f", FileType::Regular, 0o644, 0, 7, 0).unwrap();
    assert_eq!(f.gid, 500);
    let sub = fs.mkdir(d, b"sub", 0o755, 0, 7).unwrap();
    assert_eq!(sub.gid, 500);
    assert_ne!(sub.perm & 0o2000, 0);
    fs.unmount().unwrap();
    img.assert_clean();
}

#[test]
fn deep_directory_tree() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let mut d = fs.root();
        for i in 0..200 {
            d = fs.mkdir(d, format!("level{i}").as_bytes(), 0o755, 0, 0).unwrap().ino;
        }
        mkfile(&mut fs, d, "bottom", b"deep");
        fs.unmount().unwrap();
    }
    img.assert_clean();
    let mut path = String::new();
    for i in 0..200 {
        path.push_str(&format!("/level{i}"));
    }
    path.push_str("/bottom");
    assert_eq!(img.debugfs_cat(&path), b"deep");
}

#[test]
fn remount_cycles_accumulate_changes() {
    let img = Image::new(64, &["-t", "ext4"]);
    for round in 0..5u64 {
        let mut fs = img.mount();
        let root = fs.root();
        mkfile(
            &mut fs,
            root,
            &format!("round-{round}"),
            &pattern(100_000 + round as usize, round),
        );
        if round > 0 {
            fs.unlink(root, format!("round-{}", round - 1).as_bytes()).unwrap();
        }
        fs.unmount().unwrap();
        img.assert_clean();
    }
    assert_eq!(img.debugfs_cat("/round-4"), pattern(100_004, 4));
}

#[test]
fn statfs_tracks_allocation() {
    let img = Image::new(64, &["-t", "ext4", "-b", "4096"]);
    let mut fs = img.mount();
    let before = fs.statfs();
    let root = fs.root();
    mkfile(&mut fs, root, "f", &vec![1u8; 4096 * 100]);
    let mid = fs.statfs();
    assert!(before.free_blocks - mid.free_blocks >= 100);
    assert_eq!(before.free_files - mid.free_files, 1);
    fs.unlink(root, b"f").unwrap();
    let after = fs.statfs();
    assert_eq!(after.free_blocks, before.free_blocks);
    fs.sync().unwrap();
    assert_eq!(fs.statfs().free_blocks, before.free_blocks);
    fs.unmount().unwrap();
    img.assert_clean();
}

#[test]
fn directory_listing_cookies_resume() {
    let img = Image::new(32, &["-t", "ext4", "-b", "1024"]);
    let mut fs = img.mount();
    let root = fs.root();
    let d = fs.mkdir(root, b"d", 0o755, 0, 0).unwrap().ino;
    for i in 0..300 {
        mkfile(&mut fs, d, &format!("e{i}"), b"");
    }
    let all = fs.list_dir(d).unwrap();
    // resume from every 17th cookie and check the remainder matches
    for k in (0..all.len()).step_by(17) {
        let cookie = all[k].next_cookie;
        let mut rest = Vec::new();
        fs.read_dir(d, cookie, |e| {
            rest.push(e.name);
            true
        })
        .unwrap();
        let expect: Vec<Vec<u8>> = all[k + 1..].iter().map(|e| e.name.clone()).collect();
        assert_eq!(rest, expect, "resume after {k}");
    }
    // early stop
    let mut n = 0;
    fs.read_dir(d, 0, |_| {
        n += 1;
        n < 5
    })
    .unwrap();
    assert_eq!(n, 5);
}
