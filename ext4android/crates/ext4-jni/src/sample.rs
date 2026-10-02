//! The sample volume of the debug build: an image file with names, links
//! and file types the documents provider has to handle, and a large file
//! for measuring reads.

use ext4_core::{FileDevice, FileType, FormatOptions, Fs, Ino, MountOptions, Result};
use std::path::Path;
use std::sync::Arc;

/// Owner of the sample's files (the first user on most Linux systems).
const UID: u32 = 1000;
const GID: u32 = 1000;

/// Contents of `big.bin` at `offset`: not all zeros, and not repeating
/// with a period that divides block sizes.
pub fn pattern(offset: u64) -> u8 {
    (offset % 251) as u8
}

fn file(fs: &mut Fs, dir: Ino, name: &[u8], data: &[u8]) -> Result<Ino> {
    let a = fs.create(dir, name, FileType::Regular, 0o644, UID, GID, 0)?;
    let mut done = 0;
    while done < data.len() {
        done += fs.write(a.ino, done as u64, &data[done..])?;
    }
    Ok(a.ino)
}

fn dir(fs: &mut Fs, parent: Ino, name: &[u8]) -> Result<Ino> {
    Ok(fs.mkdir(parent, name, 0o755, UID, GID)?.ino)
}

/// A 24-bit BMP with a colour gradient (Android decodes BMP).
fn gradient_bmp(w: u32, h: u32) -> Vec<u8> {
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut b = Vec::with_capacity(size as usize);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0; 24]);
    for y in 0..h {
        for x in 0..w {
            // blue, green, red
            b.extend_from_slice(&[(255 * y / h) as u8, 128, (255 * x / w) as u8]);
        }
        b.resize(b.len() + (row - w * 3) as usize, 0);
    }
    b
}

/// Fill a freshly formatted volume; `big` is the size of `big.bin`.
pub fn populate(fs: &mut Fs, big: u64) -> Result<()> {
    let root = fs.root();
    file(
        fs,
        root,
        b"README.txt",
        "ext4android sample volume\next4android 的测试镜像\n".as_bytes(),
    )?;
    let docs = dir(fs, root, b"docs")?;
    file(fs, docs, b"notes.md", b"# Notes\n\nA file in a directory.\n")?;
    file(fs, docs, b"100%.txt", b"a name with a percent sign\n")?;
    file(fs, docs, b"bad-\xff-name.txt", b"a name that is not UTF-8\n")?;
    let photos = dir(fs, root, b"photos")?;
    file(fs, photos, b"gradient.bmp", &gradient_bmp(256, 256))?;
    let mut d = dir(fs, root, b"deep")?;
    for name in [&b"nested"[..], b"dir"] {
        d = dir(fs, d, name)?;
    }
    file(fs, d, b"file.txt", b"three levels down\n")?;

    let ino = fs.create(root, b"big.bin", FileType::Regular, 0o644, UID, GID, 0)?.ino;
    let mut chunk = vec![0u8; 1 << 20];
    let mut off = 0;
    while off < big {
        let n = chunk.len().min((big - off) as usize);
        for (i, b) in chunk[..n].iter_mut().enumerate() {
            *b = pattern(off + i as u64);
        }
        fs.write(ino, off, &chunk[..n])?;
        off += n as u64;
    }

    fs.symlink(root, b"docs-link", b"docs", UID, GID)?;
    fs.symlink(root, b"readme-link", b"README.txt", UID, GID)?;
    fs.symlink(docs, b"up-link", b"../photos/gradient.bmp", UID, GID)?;
    fs.symlink(root, b"broken-link", b"missing", UID, GID)?;
    fs.symlink(root, b"absolute-link", b"/etc/passwd", UID, GID)?;
    fs.symlink(root, b"loop-a", b"loop-b", UID, GID)?;
    fs.symlink(root, b"loop-b", b"loop-a", UID, GID)?;
    fs.create(root, b"fifo", FileType::Fifo, 0o644, UID, GID, 0)?;
    Ok(())
}

/// Create (or replace) the image file at `path`: `size_mib` MiB, label
/// "SAMPLE", populated with a `big.bin` of 32 MiB or a quarter of the size.
pub fn create(path: &Path, size_mib: u32) -> Result<()> {
    let size = size_mib as u64 * (1 << 20);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.set_len(size)?;
    drop(f);
    let dev = Arc::new(FileDevice::open(path, false)?);
    let opts = FormatOptions {
        label: "SAMPLE".into(),
        reserved_percent: 0.0,
        root_owner: (UID, GID),
        ..Default::default()
    };
    ext4_core::format(&*dev, &opts, &mut |_, _| {})?;
    let mut fs = Fs::mount(dev, MountOptions::default())?;
    populate(&mut fs, (32 << 20).min(size / 4))?;
    fs.unmount()
}

/// Copy `src` into the root directory of the (unmounted) image at `path`
/// as `name`; returns the bytes copied. A debug aid for putting a video
/// or other test file on the sample volume.
pub fn import(path: &Path, name: &[u8], src: &mut dyn std::io::Read) -> Result<u64> {
    let dev = Arc::new(FileDevice::open(path, false)?);
    let mut fs = Fs::mount(dev, MountOptions::default())?;
    let root = fs.root();
    let ino = fs.create(root, name, FileType::Regular, 0o644, UID, GID, 0)?.ino;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        fs.write(ino, off, &buf[..n])?;
        off += n as u64;
    }
    fs.unmount()?;
    Ok(off)
}

#[cfg(test)]
mod tests {
    #[test]
    fn import_into_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("s.img");
        super::create(&img, 32).unwrap();
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 7) as u8).collect();
        assert_eq!(super::import(&img, b"movie.mp4", &mut &data[..]).unwrap(), 3_000_000);
        // a second import of the same name is refused
        assert!(super::import(&img, b"movie.mp4", &mut &data[..10]).is_err());
        let id = crate::volumes::mount_image(&img, true).unwrap();
        let v = crate::volumes::get(id).unwrap();
        let (ino, size) = v.fs.with(|fs| crate::docs::open(fs, "movie.mp4")).unwrap();
        assert_eq!(size, 3_000_000);
        let mut back = vec![0u8; 3_000_000];
        v.fs.with(|fs| crate::docs::read(fs, ino, 0, &mut back)).unwrap();
        assert_eq!(back, data);
        drop(v);
        crate::volumes::unmount(id).unwrap();
    }

    #[test]
    fn bmp_header_and_size() {
        let b = super::gradient_bmp(3, 2);
        assert_eq!(&b[0..2], b"BM");
        // rows of 9 bytes padded to 12
        assert_eq!(b.len(), 54 + 12 * 2);
        assert_eq!(u32::from_le_bytes(b[2..6].try_into().unwrap()) as usize, b.len());
    }
}
