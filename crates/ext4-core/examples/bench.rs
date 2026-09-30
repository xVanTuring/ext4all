//! Throughput benchmark on an image file.
//!
//! `cargo run --release -p ext4-core --example bench -- IMAGE`
//! (the image must be a freshly formatted ext4 file system of >= 3 GiB).

use ext4_core::{FileDevice, FileType, Fs, MountOptions};
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench IMAGE");
    let dev = Arc::new(FileDevice::open(&path, false).unwrap());
    let mut fs = Fs::mount(dev, MountOptions::default()).unwrap();
    let root = fs.root();

    // sequential write: 1 GiB in 1 MiB chunks
    let chunk = vec![0x5Au8; 1 << 20];
    let f = fs.create(root, b"big", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    let t = Instant::now();
    for i in 0..1024u64 {
        fs.write(f, i << 20, &chunk).unwrap();
    }
    fs.sync().unwrap();
    let s = t.elapsed().as_secs_f64();
    println!("sequential write 1 GiB: {:.2}s ({:.0} MiB/s)", s, 1024.0 / s);
    println!("  extents: {}", fs.file_extents(f).unwrap().len());

    // sequential read
    let mut buf = vec![0u8; 1 << 20];
    let t = Instant::now();
    for i in 0..1024u64 {
        fs.read(f, i << 20, &mut buf).unwrap();
    }
    let s = t.elapsed().as_secs_f64();
    println!("sequential read 1 GiB: {:.2}s ({:.0} MiB/s)", s, 1024.0 / s);

    // small files
    let d = fs.mkdir(root, b"small", 0o755, 0, 0).unwrap().ino;
    let data = vec![1u8; 4096];
    let t = Instant::now();
    for i in 0..20_000 {
        let a = fs
            .create(d, format!("file-{i:06}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
            .unwrap();
        fs.write(a.ino, 0, &data).unwrap();
    }
    fs.sync().unwrap();
    let s = t.elapsed().as_secs_f64();
    println!(
        "create+write 20k x 4 KiB files: {:.2}s ({:.0} files/s)",
        s,
        20_000.0 / s
    );

    let t = Instant::now();
    let n = fs.list_dir(d).unwrap().len();
    println!("list {n} entries: {:.3}s", t.elapsed().as_secs_f64());

    let t = Instant::now();
    for i in 0..20_000 {
        fs.lookup(d, format!("file-{i:06}").as_bytes()).unwrap();
    }
    println!("20k lookups: {:.3}s", t.elapsed().as_secs_f64());

    let t = Instant::now();
    for i in 0..20_000 {
        fs.unlink(d, format!("file-{i:06}").as_bytes()).unwrap();
    }
    fs.unlink(root, b"big").unwrap();
    fs.sync().unwrap();
    println!("delete all: {:.2}s", t.elapsed().as_secs_f64());
    println!("journal commits: {}", fs.journal_commits());
    fs.unmount().unwrap();
}
