//! Test helpers: build ext4 images with e2fsprogs, inspect them with
//! debugfs, and validate them with e2fsck.
#![allow(dead_code)]

use ext4_core::{BlockDevice, FileDevice, Fs, MemDevice, MountOptions, SharedFs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub fn sbin() -> PathBuf {
    std::env::var_os("E2FSPROGS_SBIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/homebrew/opt/e2fsprogs/sbin"))
}

pub fn tool(name: &str) -> PathBuf {
    sbin().join(name)
}

pub struct Image {
    pub dir: tempfile::TempDir,
    pub path: PathBuf,
}

/// Feature profiles exercised by the parameterized tests.
pub const PROFILES: &[(&str, &[&str])] = &[
    ("ext4-4k", &["-t", "ext4", "-b", "4096"]),
    ("ext4-1k", &["-t", "ext4", "-b", "1024"]),
    ("ext4-2k", &["-t", "ext4", "-b", "2048"]),
    ("no-journal", &["-t", "ext4", "-O", "^has_journal"]),
    ("no-csum", &["-t", "ext4", "-O", "^metadata_csum,^metadata_csum_seed"]),
    (
        "gdt-csum",
        &["-t", "ext4", "-O", "^metadata_csum,^metadata_csum_seed,uninit_bg"],
    ),
    ("no-64bit-flex", &["-t", "ext4", "-O", "^64bit,^flex_bg"]),
    ("inode128", &["-t", "ext4", "-I", "128"]),
    ("meta-bg", &["-t", "ext4", "-O", "meta_bg,^resize_inode"]),
    ("no-dir-index", &["-t", "ext4", "-O", "^dir_index"]),
    ("no-orphan-file", &["-t", "ext4", "-O", "^orphan_file"]),
    ("ext3", &["-t", "ext3"]),
    ("ext3-1k", &["-t", "ext3", "-b", "1024"]),
    ("ext2", &["-t", "ext2"]),
];

impl Image {
    /// `mke2fs` an image of `size_mb` megabytes, optionally populated from
    /// `src`.
    pub fn create(size_mb: u64, opts: &[&str], src: Option<&Path>) -> Image {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fs.img");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(size_mb * 1024 * 1024).unwrap();
        drop(f);
        let mut cmd = Command::new(tool("mke2fs"));
        cmd.arg("-F").arg("-q").args(opts);
        // deterministic-ish and fast
        cmd.args(["-E", "lazy_itable_init=0,lazy_journal_init=0"]);
        cmd.args(["-L", "testvol"]);
        if let Some(s) = src {
            cmd.arg("-d").arg(s);
        }
        cmd.arg(&path);
        let out = cmd.output().expect("run mke2fs");
        assert!(
            out.status.success(),
            "mke2fs {opts:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        Image { dir, path }
    }

    pub fn new(size_mb: u64, opts: &[&str]) -> Image {
        Self::create(size_mb, opts, None)
    }

    /// An image of `bytes` bytes without a file system, filled with
    /// `fill` (sparse zeros when `fill` is 0).
    pub fn blank(bytes: u64, fill: u8) -> Image {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fs.img");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(bytes).unwrap();
        if fill != 0 {
            use std::os::unix::fs::FileExt;
            let chunk = vec![fill; 1 << 20];
            let mut off = 0;
            while off < bytes {
                let n = chunk.len().min((bytes - off) as usize);
                f.write_all_at(&chunk[..n], off).unwrap();
                off += n as u64;
            }
        }
        Image { dir, path }
    }

    pub fn device(&self, read_only: bool) -> Arc<FileDevice> {
        Arc::new(FileDevice::open(&self.path, read_only).unwrap())
    }

    pub fn mount(&self) -> Fs {
        self.mount_opts(MountOptions::default())
    }

    pub fn mount_ro(&self) -> Fs {
        self.mount_opts(MountOptions {
            read_only: true,
            ..Default::default()
        })
    }

    pub fn mount_opts(&self, opts: MountOptions) -> Fs {
        let ro = opts.read_only;
        Fs::mount(self.device(ro), opts).expect("mount")
    }

    /// Run debugfs commands (read-only unless `write`).
    pub fn debugfs_mode(&self, cmds: &[&str], write: bool) -> String {
        let cmdfile = self.dir.path().join("cmds.txt");
        std::fs::write(&cmdfile, cmds.join("\n") + "\n").unwrap();
        let mut c = Command::new(tool("debugfs"));
        if write {
            c.arg("-w");
        }
        let out = c.arg("-f").arg(&cmdfile).arg(&self.path).output().expect("run debugfs");
        let so = String::from_utf8_lossy(&out.stdout).into_owned();
        let se = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "debugfs failed: {so}{se}");
        so + &se
    }

    pub fn debugfs(&self, cmds: &[&str]) -> String {
        self.debugfs_mode(cmds, false)
    }

    pub fn debugfs_w(&self, cmds: &[&str]) {
        self.debugfs_mode(cmds, true);
    }

    /// `debugfs -R "cat path"` raw output bytes.
    pub fn debugfs_cat(&self, path: &str) -> Vec<u8> {
        let out = Command::new(tool("debugfs"))
            .arg("-R")
            .arg(format!("cat \"{path}\""))
            .arg(&self.path)
            .output()
            .expect("run debugfs");
        out.stdout
    }

    /// Names in a directory according to debugfs (`ls -p`).
    pub fn debugfs_ls(&self, path: &str) -> Vec<(String, u32)> {
        let out = Command::new(tool("debugfs"))
            .arg("-R")
            .arg(format!("ls -p \"{path}\""))
            .arg(&self.path)
            .output()
            .expect("run debugfs");
        let s = String::from_utf8_lossy(&out.stdout);
        let mut v = Vec::new();
        for line in s.lines() {
            // /ino/mode/uid/gid/name/size/
            let parts: Vec<&str> = line.split('/').collect();
            if parts.len() >= 7
                && let Ok(ino) = parts[1].parse::<u32>()
                && ino != 0
            {
                v.push((parts[5].to_string(), ino));
            }
        }
        v.sort();
        v
    }

    pub fn fsck(&self) -> (i32, String) {
        let out = Command::new(tool("e2fsck"))
            .arg("-fn")
            .arg(&self.path)
            .output()
            .expect("run e2fsck");
        let s = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), s)
    }

    /// Run `e2fsck -fy` (repairs, replays journal).
    pub fn fsck_fix(&self) -> (i32, String) {
        let out = Command::new(tool("e2fsck"))
            .arg("-fy")
            .arg(&self.path)
            .output()
            .expect("run e2fsck");
        let s = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), s)
    }

    /// `e2fsck -fyD`: rebuild directories (turns multi-block dirs into htrees).
    pub fn optimize_dirs(&self) {
        let out = Command::new(tool("e2fsck"))
            .arg("-fyD")
            .arg(&self.path)
            .output()
            .expect("run e2fsck");
        let code = out.status.code().unwrap_or(-1);
        assert!(
            code == 0 || code == 1,
            "e2fsck -fyD failed: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[track_caller]
    pub fn assert_clean(&self) {
        let (code, out) = self.fsck();
        assert!(
            code == 0 && !out.contains("Fix? no"),
            "e2fsck reported problems (exit {code}):\n{out}"
        );
    }

    pub fn dumpe2fs(&self) -> String {
        let out = Command::new(tool("dumpe2fs"))
            .arg("-h")
            .arg(&self.path)
            .output()
            .expect("run dumpe2fs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn copy(&self) -> Image {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fs.img");
        std::fs::copy(&self.path, &path).unwrap();
        Image { dir, path }
    }
}

/// Deterministic pseudo-random bytes.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// A source tree with a variety of file shapes for `mke2fs -d`.
pub fn sample_tree() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    std::fs::write(p.join("hello.txt"), b"hello, ext4\n").unwrap();
    std::fs::write(p.join("empty"), b"").unwrap();
    std::fs::write(p.join("one-block"), pattern(4096, 1)).unwrap();
    std::fs::write(p.join("odd-size"), pattern(10_001, 2)).unwrap();
    std::fs::write(p.join("big.bin"), pattern(3 * 1024 * 1024 + 123, 3)).unwrap();
    std::fs::create_dir_all(p.join("dir/sub/deeper")).unwrap();
    std::fs::write(p.join("dir/a"), b"a").unwrap();
    std::fs::write(p.join("dir/sub/b"), pattern(5000, 4)).unwrap();
    std::fs::write(p.join("dir/sub/deeper/c"), b"deep").unwrap();
    std::fs::write(p.join("中文名字.txt"), "你好".as_bytes()).unwrap();
    std::os::unix::fs::symlink("hello.txt", p.join("link-short")).unwrap();
    let long_target = "x".repeat(200);
    std::os::unix::fs::symlink(&long_target, p.join("link-long")).unwrap();
    std::fs::create_dir(p.join("many")).unwrap();
    for i in 0..500 {
        std::fs::write(p.join(format!("many/file-{i:04}")), format!("{i}")).unwrap();
    }
    d
}

/// xorshift64: reproducible randomness for concurrency tests.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Writes starting with these bytes fail on a [`SlowDevice`].
pub const FAIL_MARK: &[u8; 8] = b"FAIL-IO!";

/// A memory device whose data transfers (anything larger than a block)
/// take a while, so work done without the file system lock overlaps the
/// operations of other threads. Counts how many data writes ran at once,
/// and fails writes that start with [`FAIL_MARK`].
pub struct SlowDevice {
    pub inner: Arc<MemDevice>,
    pub block: usize,
    writing: AtomicUsize,
    pub most_writes_at_once: AtomicUsize,
    /// How long a data write takes on threads whose name starts with
    /// "slow", in microseconds (others take 300) ...
    pub write_micros: AtomicU64,
    /// ... if it starts with these bytes (when set).
    pub slow_mark: std::sync::Mutex<Vec<u8>>,
}

impl SlowDevice {
    pub fn new(inner: Arc<MemDevice>, block: usize) -> SlowDevice {
        SlowDevice {
            inner,
            block,
            writing: AtomicUsize::new(0),
            most_writes_at_once: AtomicUsize::new(0),
            write_micros: AtomicU64::new(300),
            slow_mark: Default::default(),
        }
    }
}

impl BlockDevice for SlowDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> ext4_core::Result<()> {
        if buf.len() > self.block {
            std::thread::sleep(Duration::from_micros(300));
        }
        self.inner.read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> ext4_core::Result<()> {
        if buf.starts_with(FAIL_MARK) {
            return Err(ext4_core::Error::Device(5));
        }
        if buf.len() > self.block {
            let now = self.writing.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_writes_at_once.fetch_max(now, Ordering::SeqCst);
            let slow = std::thread::current().name().is_some_and(|n| n.starts_with("slow"))
                && buf.starts_with(&self.slow_mark.lock().unwrap());
            let micros = if slow {
                self.write_micros.load(Ordering::Relaxed)
            } else {
                300
            };
            std::thread::sleep(Duration::from_micros(micros));
            self.writing.fetch_sub(1, Ordering::SeqCst);
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> ext4_core::Result<()> {
        self.inner.flush()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
}

/// Mount `img` from memory through a [`SlowDevice`] as a [`SharedFs`].
pub fn shared_mount(img: &Image, block: usize) -> (Arc<MemDevice>, Arc<SlowDevice>, SharedFs) {
    let mem = Arc::new(MemDevice::from_vec(std::fs::read(&img.path).unwrap()));
    let dev = Arc::new(SlowDevice::new(mem.clone(), block));
    let fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
    (mem, dev, SharedFs::new(fs, Duration::from_millis(20)))
}

/// Unmount, store the memory device back into `img` and run e2fsck.
pub fn shared_finish(img: &Image, mem: &MemDevice, shared: SharedFs) {
    shared.unmount().unwrap();
    drop(shared);
    std::fs::write(&img.path, mem.snapshot()).unwrap();
    img.assert_clean();
}
