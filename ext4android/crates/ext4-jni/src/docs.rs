//! What the documents provider shows of a volume: regular files and
//! directories (symbolic links appear as their targets), found by the
//! encoded paths of [`crate::names`].
//!
//! Symbolic links are followed when they are relative and stay inside the
//! volume. Absolute targets point into the system the disk came from, so
//! such links, broken links and loops are left out of listings. Device
//! nodes, FIFOs and sockets are left out too.

use crate::names;
use ext4_core::{Attr, Error, FileType, Fs, Ino, Result};
use std::collections::VecDeque;

/// Symbolic links followed in one lookup at most (as Linux).
const MAX_LINKS: u32 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    File = 1,
    Dir = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Encoded name: the last component of the document's path.
    pub name: String,
    /// Name for people.
    pub display: String,
    pub kind: Kind,
    /// Inode of the file or directory (of the target, for a link).
    pub ino: Ino,
    pub size: u64,
    pub mtime_ms: i64,
    pub perm: u16,
}

pub(crate) fn entry(name: &[u8], a: &Attr) -> Option<Entry> {
    let kind = match a.file_type {
        FileType::Regular => Kind::File,
        FileType::Directory => Kind::Dir,
        _ => return None,
    };
    Some(Entry {
        name: names::encode(name),
        display: names::display(name),
        kind,
        ino: a.ino,
        size: if kind == Kind::File { a.size } else { 0 },
        mtime_ms: a.mtime.sec.saturating_mul(1000) + (a.mtime.nsec / 1_000_000) as i64,
        perm: a.perm,
    })
}

/// Walk `comps` from the directory at the top of `stack` (the inodes from
/// the root down), following symbolic links in the middle of the path and,
/// with `follow_last`, at its end. Returns the attributes of the result,
/// which is then at the top of `stack`.
pub(crate) fn walk(fs: &mut Fs, stack: &mut Vec<Ino>, comps: Vec<Vec<u8>>, follow_last: bool) -> Result<Attr> {
    let mut queue: VecDeque<Vec<u8>> = comps.into();
    let mut links = 0;
    let mut attr = fs.stat(*stack.last().expect("root on the stack"))?;
    while let Some(c) = queue.pop_front() {
        if c.is_empty() || c == b"." {
            continue;
        }
        if c == b".." {
            if stack.len() == 1 {
                return Err(Error::NotFound); // above the root of the volume
            }
            stack.pop();
            attr = fs.stat(*stack.last().unwrap())?;
            continue;
        }
        if !attr.is_dir() {
            return Err(Error::NotDir);
        }
        let ino = fs.lookup(*stack.last().unwrap(), &c)?;
        let a = fs.stat(ino)?;
        if a.file_type == FileType::Symlink && (follow_last || !queue.is_empty()) {
            links += 1;
            if links > MAX_LINKS {
                return Err(Error::invalid("too many levels of symbolic links"));
            }
            let target = fs.read_link(ino)?;
            if target.is_empty() || target[0] == b'/' {
                return Err(Error::NotFound);
            }
            for part in target.split(|&b| b == b'/').rev() {
                queue.push_front(part.to_vec());
            }
            // the target is relative to the directory holding the link
            continue;
        }
        stack.push(ino);
        attr = a;
    }
    Ok(attr)
}

/// The document at an encoded path ("" is the root).
pub fn stat(fs: &mut Fs, path: &str) -> Result<Entry> {
    let comps = names::decode_path(path)?;
    let name = comps.last().cloned().unwrap_or_default();
    let mut stack = vec![fs.root()];
    let a = walk(fs, &mut stack, comps, true)?;
    entry(&name, &a).ok_or(Error::NotFound)
}

/// The documents in the directory at an encoded path.
pub fn list(fs: &mut Fs, path: &str) -> Result<Vec<Entry>> {
    let mut stack = vec![fs.root()];
    let a = walk(fs, &mut stack, names::decode_path(path)?, true)?;
    if !a.is_dir() {
        return Err(Error::NotDir);
    }
    let dir = *stack.last().unwrap();
    let mut out = Vec::new();
    for e in fs.list_dir(dir)? {
        if e.name == b"." || e.name == b".." {
            continue;
        }
        let a = match fs.stat(e.ino) {
            Ok(a) if a.file_type == FileType::Symlink => walk(fs, &mut stack.clone(), vec![e.name.clone()], true),
            r => r,
        };
        match a {
            Ok(a) => out.extend(entry(&e.name, &a)),
            Err(err) => log::debug!("left out {:?}: {err}", names::display(&e.name)),
        }
    }
    Ok(out)
}

/// Inode and size of the regular file at an encoded path.
pub fn open(fs: &mut Fs, path: &str) -> Result<(Ino, u64)> {
    let e = stat(fs, path)?;
    if e.kind != Kind::File {
        return Err(Error::IsDir);
    }
    Ok((e.ino, e.size))
}

/// Read from `offset` until `buf` is full or the file ends.
pub fn read(fs: &mut Fs, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = fs.read(ino, offset + done as u64, &mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Entries for Kotlin (`EntryReader`), little-endian, one after another:
/// name and display name (u16 length + UTF-8), kind u8, inode u32, size
/// u64, modification time i64 (ms), permissions u16.
pub fn encode_entries(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * 64);
    for e in entries {
        put_str(&mut out, &e.name);
        put_str(&mut out, &e.display);
        out.push(e.kind as u8);
        out.extend_from_slice(&e.ino.to_le_bytes());
        out.extend_from_slice(&e.size.to_le_bytes());
        out.extend_from_slice(&e.mtime_ms.to_le_bytes());
        out.extend_from_slice(&e.perm.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;
    use ext4_core::{FormatOptions, MemDevice, MountOptions};
    use std::sync::Arc;

    fn sample_fs() -> Fs {
        let dev = Arc::new(MemDevice::new(16 << 20));
        ext4_core::format(&*dev, &FormatOptions::default(), &mut |_, _| {}).unwrap();
        let mut fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        sample::populate(&mut fs, 300_000).unwrap();
        fs
    }

    fn names(entries: &[Entry]) -> Vec<(String, Kind)> {
        let mut v: Vec<_> = entries.iter().map(|e| (e.name.clone(), e.kind)).collect();
        v.sort();
        v
    }

    #[test]
    fn root_listing_shows_files_dirs_and_good_links_only() {
        let mut fs = sample_fs();
        let got = names(&list(&mut fs, "").unwrap());
        let want: Vec<(String, Kind)> = [
            ("README.txt", Kind::File),
            ("big.bin", Kind::File),
            ("deep", Kind::Dir),
            ("docs", Kind::Dir),
            ("docs-link", Kind::Dir),
            ("lost+found", Kind::Dir),
            ("photos", Kind::Dir),
            ("readme-link", Kind::File),
        ]
        .iter()
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn odd_names_are_encoded_and_links_resolve_through_parents() {
        let mut fs = sample_fs();
        let docs = list(&mut fs, "docs").unwrap();
        let got = names(&docs);
        assert!(got.contains(&("100%25.txt".into(), Kind::File)), "{got:?}");
        assert!(got.contains(&("bad-%FF-name.txt".into(), Kind::File)), "{got:?}");
        let up = docs.iter().find(|e| e.name == "up-link").unwrap();
        let bmp = stat(&mut fs, "photos/gradient.bmp").unwrap();
        assert_eq!((up.kind, up.size, up.ino), (Kind::File, bmp.size, bmp.ino));
        assert_eq!(docs.iter().find(|e| e.name.starts_with("bad")).unwrap().display, "bad-\u{FFFD}-name.txt");

        // through a link to a directory, and the encoded names back again
        let e = stat(&mut fs, "docs-link/100%25.txt").unwrap();
        assert_eq!((e.kind, e.display.as_str()), (Kind::File, "100%.txt"));
        assert_eq!(stat(&mut fs, "docs/bad-%FF-name.txt").unwrap().kind, Kind::File);
    }

    #[test]
    fn broken_absolute_and_looping_links_are_not_found() {
        let mut fs = sample_fs();
        assert!(matches!(stat(&mut fs, "broken-link"), Err(Error::NotFound)));
        assert!(matches!(stat(&mut fs, "absolute-link"), Err(Error::NotFound)));
        assert!(stat(&mut fs, "loop-a").is_err());
        assert!(matches!(stat(&mut fs, "fifo"), Err(Error::NotFound)));
        assert!(matches!(stat(&mut fs, "missing"), Err(Error::NotFound)));
        assert!(matches!(list(&mut fs, "README.txt"), Err(Error::NotDir)));
        assert!(matches!(stat(&mut fs, "README.txt/x"), Err(Error::NotDir)));
        assert!(stat(&mut fs, "docs/..").is_err(), "not a valid encoded name");
    }

    #[test]
    fn root_and_nested_paths() {
        let mut fs = sample_fs();
        let root = stat(&mut fs, "").unwrap();
        assert_eq!((root.kind, root.name.as_str()), (Kind::Dir, ""));
        assert_eq!(stat(&mut fs, "deep/nested/dir/file.txt").unwrap().kind, Kind::File);
        assert_eq!(list(&mut fs, "deep/nested").unwrap().len(), 1);
    }

    #[test]
    fn open_and_read_whole_and_partial() {
        let mut fs = sample_fs();
        let (ino, size) = open(&mut fs, "big.bin").unwrap();
        assert_eq!(size, 300_000);
        let mut buf = vec![0u8; 400_000];
        assert_eq!(read(&mut fs, ino, 0, &mut buf).unwrap(), 300_000);
        assert!((0..300_000).all(|i| buf[i] == sample::pattern(i as u64)));
        let mut tail = [0u8; 10];
        assert_eq!(read(&mut fs, ino, 299_995, &mut tail).unwrap(), 5);
        assert_eq!(read(&mut fs, ino, 300_000, &mut tail).unwrap(), 0);
        assert!(matches!(open(&mut fs, "docs"), Err(Error::IsDir)));
    }

    #[test]
    fn entry_encoding_layout() {
        let e = Entry {
            name: "a%25".into(),
            display: "a%".into(),
            kind: Kind::File,
            ino: 12,
            size: 5,
            mtime_ms: -1,
            perm: 0o644,
        };
        let b = encode_entries(&[e]);
        assert_eq!(&b[0..2], &4u16.to_le_bytes());
        assert_eq!(&b[2..6], b"a%25");
        assert_eq!(&b[6..8], &2u16.to_le_bytes());
        assert_eq!(b[10], 1);
        assert_eq!(b.len(), 2 + 4 + 2 + 2 + 1 + 4 + 8 + 8 + 2);
        assert_eq!(&b[b.len() - 2..], &0o644u16.to_le_bytes());
    }
}
