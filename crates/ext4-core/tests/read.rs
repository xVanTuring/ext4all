//! Read path: mount images built by mke2fs and compare against the source
//! tree and debugfs.

mod common;
use common::*;
use ext4_core::{Error, FileType, Fs, MountOptions};
use std::path::Path;

fn read_all(fs: &mut Fs, ino: u32) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size + 100];
    let mut done = 0;
    while done < size {
        let n = fs.read(ino, done as u64, &mut buf[done..]).unwrap();
        assert!(n > 0, "short read at {done}/{size}");
        done += n;
    }
    buf.truncate(size);
    buf
}

/// Recursively compare the file system subtree at `ino` with `dir`.
fn compare_tree(fs: &mut Fs, ino: u32, dir: &Path) {
    let mut expect: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    expect.sort();
    let mut got: Vec<String> = fs
        .list_dir(ino)
        .unwrap()
        .into_iter()
        .map(|e| String::from_utf8(e.name).unwrap())
        .filter(|n| n != "." && n != ".." && n != "lost+found")
        .collect();
    got.sort();
    assert_eq!(got, expect, "entries of {dir:?}");
    for name in expect {
        let p = dir.join(&name);
        let child = fs.lookup(ino, name.as_bytes()).unwrap();
        let attr = fs.stat(child).unwrap();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        if meta.file_type().is_symlink() {
            assert_eq!(attr.file_type, FileType::Symlink);
            let t = std::fs::read_link(&p).unwrap();
            assert_eq!(fs.read_link(child).unwrap(), t.to_str().unwrap().as_bytes());
        } else if meta.is_dir() {
            assert_eq!(attr.file_type, FileType::Directory);
            compare_tree(fs, child, &p);
        } else {
            assert_eq!(attr.file_type, FileType::Regular);
            assert_eq!(attr.size, meta.len(), "{p:?}");
            assert_eq!(read_all(fs, child), std::fs::read(&p).unwrap(), "{p:?}");
        }
    }
}

#[test]
fn read_populated_images_all_profiles() {
    let tree = sample_tree();
    for (name, opts) in PROFILES {
        let img = Image::create(64, opts, Some(tree.path()));
        let mut fs = img.mount_ro();
        let root = fs.root();
        compare_tree(&mut fs, root, tree.path());
        assert!(fs.is_read_only(), "{name}");
    }
}

#[test]
fn listing_matches_debugfs() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4"], Some(tree.path()));
    let mut fs = img.mount_ro();
    for path in ["/", "/dir", "/dir/sub", "/many"] {
        let ino = fs.resolve(path).unwrap();
        let mut ours: Vec<(String, u32)> = fs
            .list_dir(ino)
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8(e.name).unwrap(), e.ino))
            .collect();
        ours.sort();
        assert_eq!(ours, img.debugfs_ls(path), "{path}");
    }
}

#[test]
fn big_directory_is_htree_and_lookups_work() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4", "-b", "1024"], Some(tree.path()));
    img.optimize_dirs();
    let out = img.debugfs(&["stat /many"]);
    assert!(out.contains("Flags: 0x81000"), "expected htree dir: {out}");
    let mut fs = img.mount_ro();
    let many = fs.resolve("/many").unwrap();
    fs.check_htree(many).unwrap();
    for i in (0..500).step_by(7) {
        let n = format!("file-{i:04}");
        let ino = fs.lookup(many, n.as_bytes()).unwrap();
        assert_eq!(read_all(&mut fs, ino), i.to_string().as_bytes());
    }
    assert!(matches!(fs.lookup(many, b"file-9999"), Err(Error::NotFound)));
}

#[test]
fn stat_matches_debugfs() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4"], Some(tree.path()));
    let mut fs = img.mount_ro();
    let ino = fs.resolve("/big.bin").unwrap();
    let a = fs.stat(ino).unwrap();
    let out = img.debugfs(&["stat /big.bin"]);
    assert!(out.contains(&format!("Size: {}", a.size)), "{out}");
    let blocks_line = out.lines().find(|l| l.contains("Blockcount:")).unwrap();
    let bc: u64 = blocks_line.split("Blockcount:").nth(1).unwrap().trim().parse().unwrap();
    assert_eq!(a.allocated, bc * 512);
    assert_eq!(a.nlink, 1);
}

#[test]
fn read_at_offsets_and_eof() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4"], Some(tree.path()));
    let mut fs = img.mount_ro();
    let ino = fs.resolve("/odd-size").unwrap();
    let data = pattern(10_001, 2);
    for (off, len) in [
        (0usize, 1usize),
        (1, 4095),
        (4095, 2),
        (4096, 4096),
        (9999, 100),
        (10_000, 1),
    ] {
        let mut buf = vec![0u8; len];
        let n = fs.read(ino, off as u64, &mut buf).unwrap();
        let want = &data[off..(off + len).min(data.len())];
        assert_eq!(&buf[..n], want, "off {off} len {len}");
    }
    let mut buf = [0u8; 10];
    assert_eq!(fs.read(ino, 10_001, &mut buf).unwrap(), 0);
    assert_eq!(fs.read(ino, 1 << 40, &mut buf).unwrap(), 0);
}

#[test]
fn sparse_and_unwritten_files_read_zeros() {
    let img = Image::new(32, &["-t", "ext4"]);
    std::fs::write(img.dir.path().join("x"), b"XYZ").unwrap();
    let x = img.dir.path().join("x");
    img.debugfs_w(&[
        &format!("write {} sparse", x.display()),
        "fallocate /sparse 10 19",
        "sif /sparse size 1048576",
        &format!("write {} tail", x.display()),
    ]);
    let mut fs = img.mount_ro();
    let ino = fs.resolve("/sparse").unwrap();
    let mut buf = vec![0xAAu8; 1048576];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(n, 1048576);
    assert_eq!(&buf[..3], b"XYZ");
    assert!(buf[3..].iter().all(|&b| b == 0));
    let exts = fs.file_extents(ino).unwrap();
    assert!(exts.iter().any(|e| e.unwritten), "{exts:?}");
}

#[test]
fn statfs_matches_superblock() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    let fs = img.mount_ro();
    let s = fs.statfs();
    let d = img.dumpe2fs();
    let free: u64 = d
        .lines()
        .find(|l| l.starts_with("Free blocks:"))
        .unwrap()
        .split(':')
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(s.free_blocks, free);
    assert_eq!(s.block_size, 4096);
    assert_eq!(s.blocks, 8192);
    assert!(s.avail_blocks <= s.free_blocks);
    assert_eq!(s.name_max, 255);
}

#[test]
fn xattrs_readable() {
    let img = Image::new(32, &["-t", "ext4", "-b", "4096"]);
    img.debugfs_w(&[
        "write /dev/null f",
        "ea_set /f user.small hello",
        "ea_set /f user.other world",
        "ea_set /f trusted.t value",
    ]);
    let big = "B".repeat(3000);
    let vf = img.dir.path().join("bigval");
    std::fs::write(&vf, &big).unwrap();
    img.debugfs_w(&[&format!("ea_set -f {} /f user.big", vf.display())]);
    let mut fs = img.mount_ro();
    let ino = fs.resolve("/f").unwrap();
    assert_eq!(fs.get_xattr(ino, b"user.small").unwrap(), b"hello");
    assert_eq!(fs.get_xattr(ino, b"user.other").unwrap(), b"world");
    assert_eq!(fs.get_xattr(ino, b"trusted.t").unwrap(), b"value");
    assert_eq!(fs.get_xattr(ino, b"user.big").unwrap(), big.as_bytes());
    assert!(matches!(fs.get_xattr(ino, b"user.none"), Err(Error::NoAttr)));
    let names = fs.list_xattr(ino).unwrap();
    for n in ["user.small", "user.other", "trusted.t", "user.big"] {
        assert!(names.contains(&n.as_bytes().to_vec()), "{n} in {names:?}");
    }
}

#[test]
fn ext3_image_mounts_read_only() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext3"], Some(tree.path()));
    let mut fs = img.mount_opts(MountOptions::default());
    assert!(fs.is_read_only());
    let root = fs.root();
    compare_tree(&mut fs, root, tree.path());
}

#[test]
fn ext2_image_mounts_read_only() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext2"], Some(tree.path()));
    let mut fs = img.mount();
    assert!(fs.is_read_only());
    let root = fs.root();
    compare_tree(&mut fs, root, tree.path());
}

#[test]
fn inline_data_image_readable() {
    let tree = sample_tree();
    let img = Image::create(64, &["-t", "ext4", "-O", "inline_data"], Some(tree.path()));
    let mut fs = img.mount_ro();
    let root = fs.root();
    compare_tree(&mut fs, root, tree.path());
}

#[test]
fn bigalloc_image_is_read_only() {
    let tree = sample_tree();
    let img = Image::create(128, &["-t", "ext4", "-O", "bigalloc", "-C", "16384"], Some(tree.path()));
    let mut fs = img.mount();
    assert!(fs.is_read_only());
    let root = fs.root();
    compare_tree(&mut fs, root, tree.path());
}

#[test]
fn not_ext4_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("zero.img");
    std::fs::write(&p, vec![0u8; 1 << 20]).unwrap();
    let dev = std::sync::Arc::new(ext4_core::FileDevice::open(&p, true).unwrap());
    assert!(Fs::mount(dev, MountOptions::default()).is_err());
}

#[test]
fn probe_reports_label_and_uuid() {
    let img = Image::new(16, &["-t", "ext4"]);
    let dev = img.device(true);
    let sb = Fs::probe(&*dev).unwrap();
    assert_eq!(sb.volume_name(), "testvol");
    let d = img.dumpe2fs();
    let uuid_line = d.lines().find(|l| l.starts_with("Filesystem UUID:")).unwrap();
    let u = sb.uuid();
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    let dashed = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    assert!(uuid_line.contains(&dashed), "{uuid_line} vs {dashed}");
}

#[test]
fn corrupted_superblock_checksum_fails_mount() {
    let img = Image::new(16, &["-t", "ext4"]);
    {
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(&img.path).unwrap();
        f.write_at(b"X", 1024 + 0x100).unwrap();
    }
    let dev = img.device(true);
    assert!(matches!(
        Fs::mount(dev, MountOptions::default()),
        Err(Error::Checksum(_))
    ));
}

#[test]
fn dx_hash_matches_debugfs() {
    use ext4_core::hash::dirhash;
    let img = Image::new(16, &["-t", "ext4"]);
    // debugfs hashes with the open file system's seed
    let seed = Fs::probe(&*img.device(true)).unwrap().hash_seed();
    for (alg, v) in [("half_md4", 1u8), ("tea", 2), ("legacy", 0)] {
        for name in [
            "a",
            "hello",
            "file-0001",
            "a-rather-long-file-name-that-crosses-32-bytes.txt",
            "Ünïcödé",
        ] {
            let out = img.debugfs(&[&format!("dx_hash -h {alg} \"{name}\"")]);
            // "Hash of <name> is 0x... (minor 0x...)"
            let line = out
                .lines()
                .find(|l| l.contains("Hash of"))
                .unwrap_or_else(|| panic!("{out}"));
            let hex = line.split(" is ").nth(1).unwrap().split_whitespace().next().unwrap();
            let want = u32::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap();
            let got = dirhash(name.as_bytes(), v, &seed).unwrap().major;
            assert_eq!(got, want, "{alg} {name}: {line}");
        }
    }
}
