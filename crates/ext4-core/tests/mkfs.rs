//! Formatting: every new file system must pass `e2fsck -fn`, match
//! mke2fs's geometry, mount, take writes spread over many groups, and stay
//! clean.

mod common;
use common::*;
use ext4_core::{FileDevice, FileType, FormatOptions, FormatSummary, format};
use std::collections::BTreeMap;

const MIB: u64 = 1 << 20;

fn make(bytes: u64, fill: u8, o: &FormatOptions) -> (Image, FormatSummary) {
    let img = Image::blank(bytes, fill);
    let dev = FileDevice::open(&img.path, false).unwrap();
    let mut last = (0, 1);
    let mut calls = 0;
    let s = format(&dev, o, &mut |done, total| {
        assert!(done <= total);
        assert!(done >= last.0, "progress goes forward");
        last = (done, total);
        calls += 1;
    })
    .unwrap();
    assert_eq!(last.0, last.1, "progress ends at the total");
    assert!(calls > 1);
    (img, s)
}

/// `dumpe2fs -h` fields by name.
fn fields(img: &Image) -> BTreeMap<String, String> {
    img.dumpe2fs()
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Physical start of the journal according to debugfs.
fn journal_start(img: &Image) -> String {
    let out = img.debugfs(&["stat <8>"]);
    let line = out
        .lines()
        .skip_while(|l| !l.starts_with("EXTENTS"))
        .nth(1)
        .unwrap_or("")
        .to_string();
    line.split(':')
        .nth(1)
        .unwrap_or("")
        .split('-')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Directories and files spread over the groups (directories go to
/// different groups), then verify the contents after a remount.
fn exercise(img: &Image, dirs: usize, files: usize) {
    let mut fs = img.mount();
    let root = fs.root();
    for d in 0..dirs {
        let dir = fs.mkdir(root, format!("d{d}").as_bytes(), 0o755, 0, 0).unwrap().ino;
        for f in 0..files {
            let a = fs
                .create(dir, format!("f{f}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                .unwrap();
            fs.write(a.ino, 0, &pattern(100 + (d * 7 + f * 13) % 9000, (d * 1000 + f) as u64))
                .unwrap();
        }
    }
    fs.unmount().unwrap();
    img.assert_clean();
    let mut fs = img.mount_ro();
    let root = fs.root();
    for d in (0..dirs).step_by(dirs.div_ceil(5).max(1)) {
        let dir = fs.lookup(root, format!("d{d}").as_bytes()).unwrap();
        for f in (0..files).step_by(files.div_ceil(3).max(1)) {
            let ino = fs.lookup(dir, format!("f{f}").as_bytes()).unwrap();
            let want = pattern(100 + (d * 7 + f * 13) % 9000, (d * 1000 + f) as u64);
            let mut got = vec![0u8; want.len()];
            assert_eq!(fs.read(ino, 0, &mut got).unwrap(), want.len());
            assert_eq!(got, want, "d{d}/f{f}");
        }
    }
}

#[test]
fn new_file_systems_are_clean_and_usable() {
    for (bytes, fill) in [(8 * MIB, 0u8), (64 * MIB, 0xA5), (600 * MIB, 0), (3 << 30, 0x5A)] {
        let o = FormatOptions {
            label: "fresh".into(),
            ..Default::default()
        };
        let (img, s) = make(bytes, fill, &o);
        img.assert_clean();
        let f = fields(&img);
        assert_eq!(
            f["Filesystem features"],
            "has_journal ext_attr dir_index filetype extent 64bit flex_bg sparse_super large_file huge_file \
             dir_nlink extra_isize metadata_csum",
            "{bytes}"
        );
        assert_eq!(f["Filesystem volume name"], "fresh");
        assert_eq!(f["Filesystem state"], "clean");
        assert_eq!(f["Block count"], s.blocks.to_string());
        assert_eq!(f["Inode count"], s.inodes.to_string());
        let ls = img.debugfs_ls("/");
        assert!(ls.iter().any(|(n, i)| n == "lost+found" && *i == 11), "{ls:?}");
        exercise(&img, (s.groups as usize * 2).clamp(8, 60), 15);
    }
}

#[test]
fn geometry_matches_mke2fs() {
    for bytes in [2 * MIB, 64 * MIB, 600 * MIB, 3 << 30] {
        let (ours, _) = make(bytes, 0, &FormatOptions::default());
        let theirs = Image::blank(bytes, 0);
        let out = std::process::Command::new(tool("mke2fs"))
            .args([
                "-q",
                "-F",
                "-t",
                "ext4",
                "-O",
                "^resize_inode,^orphan_file,^metadata_csum_seed",
            ])
            .arg(&theirs.path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let (a, b) = (fields(&ours), fields(&theirs));
        for k in [
            "Block size",
            "Block count",
            "Reserved block count",
            "Inode count",
            "Inodes per group",
            "Blocks per group",
            "Inode size",
            "Flex block group size",
            "Required extra isize",
            "Default mount options",
            "Total journal blocks",
        ] {
            assert_eq!(a.get(k), b.get(k), "{bytes}: {k}");
        }
        if bytes > 2 * MIB {
            // mke2fs fragments the journal of tiny file systems
            assert_eq!(
                journal_start(&ours),
                journal_start(&theirs),
                "{bytes}: journal location"
            );
        }
    }
}

#[test]
fn tiny_file_systems_have_no_journal() {
    let (img, s) = make(1536 * 1024, 0, &FormatOptions::default());
    assert_eq!(s.journal_blocks, 0);
    assert!(!fields(&img)["Filesystem features"].contains("has_journal"));
    img.assert_clean();
    exercise(&img, 4, 5);
    assert!(Image::blank(32 * 1024, 0).path.exists());
    let small = Image::blank(32 * 1024, 0);
    let dev = FileDevice::open(&small.path, false).unwrap();
    assert!(
        format(&dev, &FormatOptions::default(), &mut |_, _| {}).is_err(),
        "32 KiB is too small"
    );
}

#[test]
fn options_are_honoured() {
    let uuid = *b"\x12\x34\x56\x78\x9a\xbc\x4d\xef\x81\x23\x45\x67\x89\xab\xcd\xef";
    let o = FormatOptions {
        label: "sixteen-bytes-ok".into(),
        block_size: Some(2048),
        inode_count: Some(5000),
        reserved_percent: 0.0,
        uuid: Some(uuid),
        root_owner: (1000, 1000),
        time: Some(1_700_000_000),
        ..Default::default()
    };
    let (img, s) = make(256 * MIB, 0, &o);
    img.assert_clean();
    let f = fields(&img);
    assert_eq!(f["Block size"], "2048");
    assert_eq!(f["Reserved block count"], "0");
    assert!(s.inodes >= 5000 && s.inodes < 6000, "{}", s.inodes);
    assert_eq!(f["Filesystem UUID"], "12345678-9abc-4def-8123-456789abcdef");
    assert_eq!(f["Filesystem volume name"], "sixteen-bytes-ok");
    let root = img.debugfs(&["stat /"]);
    assert!(root.contains("User:  1000   Group:  1000"), "{root}");
    assert!(f["Filesystem created"].contains("2023"), "{}", f["Filesystem created"]);
    exercise(&img, 10, 10);

    // long labels are cut to 16 bytes at a character boundary, like mke2fs
    let dev = FileDevice::open(&img.path, false).unwrap();
    let long = FormatOptions {
        label: "外置硬盘数据盘一号".into(),
        ..Default::default()
    };
    format(&dev, &long, &mut |_, _| {}).unwrap();
    assert_eq!(fields(&img)["Filesystem volume name"], "外置硬盘数");
    img.assert_clean();
    let bad = FormatOptions {
        block_size: Some(3000),
        ..Default::default()
    };
    assert!(format(&dev, &bad, &mut |_, _| {}).is_err());
}

#[test]
fn small_last_group_is_dropped() {
    let o = FormatOptions {
        block_size: Some(4096),
        ..Default::default()
    };
    let (img, s) = make((2 * 32768 + 100) * 4096, 0, &o);
    assert_eq!((s.blocks, s.groups), (65536, 2));
    img.assert_clean();
}

#[test]
fn reformatting_replaces_an_existing_file_system() {
    let img = Image::new(64, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        let root = fs.root();
        fs.mkdir(root, b"old", 0o755, 0, 0).unwrap();
        fs.unmount().unwrap();
    }
    let dev = FileDevice::open(&img.path, false).unwrap();
    format(&dev, &FormatOptions::default(), &mut |_, _| {}).unwrap();
    img.assert_clean();
    let ls = img.debugfs_ls("/");
    assert!(!ls.iter().any(|(n, _)| n == "old"), "{ls:?}");
    exercise(&img, 6, 6);
}

/// A 64 GiB image: 512 groups, 32 flex groups, a 512 MiB journal with four
/// extents.
#[test]
fn large_sparse_image() {
    let (img, s) = make(64 << 30, 0, &FormatOptions::default());
    assert_eq!((s.groups, s.journal_blocks), (512, 131072));
    img.assert_clean();
    exercise(&img, 40, 3);
}
