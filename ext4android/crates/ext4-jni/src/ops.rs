//! Changes made through the documents provider: creating, deleting,
//! renaming, moving and copying documents, and writes through file
//! descriptors. Paths are encoded ([`crate::names`]); names people give
//! (UTF-8 from Android) are used as they are.
//!
//! Paths resolve like listings: symbolic links in the middle are followed;
//! deleting, renaming or moving a link changes the link, not its target.

use crate::docs::{self, Entry};
use crate::names;
use crate::volumes::Volume;
use ext4_core::{Attr, Error, FileType, Fs, Ino, RenameFlags, Result};

/// Bytes copied per hold of the file system lock, so other requests run
/// between chunks of a large copy.
const COPY_CHUNK: usize = 1 << 20;
/// Free names tried: "name (1).ext" to "name (100).ext".
const UNIQUE_TRIES: u32 = 100;
const NAME_MAX: usize = 255;
/// Deepest directory tree walked upwards when checking for copies into
/// themselves.
const MAX_DEPTH: usize = 4096;

const NO_REPLACE: RenameFlags = RenameFlags {
    no_replace: true,
    exchange: false,
};

/// The directory at an encoded path (symbolic links followed).
fn dir(fs: &mut Fs, path: &str) -> Result<Ino> {
    let mut stack = vec![fs.root()];
    let a = docs::walk(fs, &mut stack, names::decode_path(path)?, true)?;
    if !a.is_dir() {
        return Err(Error::NotDir);
    }
    Ok(*stack.last().unwrap())
}

/// The directory holding the last component of an encoded path (links
/// followed) and that component's name (not followed).
fn parent(fs: &mut Fs, path: &str) -> Result<(Ino, Vec<u8>)> {
    let mut comps = names::decode_path(path)?;
    let name = comps
        .pop()
        .ok_or_else(|| Error::invalid("the root of a volume cannot be changed"))?;
    let mut stack = vec![fs.root()];
    if !docs::walk(fs, &mut stack, comps, true)?.is_dir() {
        return Err(Error::NotDir);
    }
    Ok((*stack.last().unwrap(), name))
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn join(dir_path: &str, name: &[u8]) -> String {
    let n = names::encode(name);
    if dir_path.is_empty() { n } else { format!("{dir_path}/{n}") }
}

/// A name a person gave, as ext4 name bytes.
fn check_name(name: &str) -> Result<Vec<u8>> {
    let b = name.as_bytes();
    if b.is_empty() || name == "." || name == ".." || b.contains(&b'/') || b.contains(&0) {
        return Err(Error::invalid(format!("not a file name: {name:?}")));
    }
    if b.len() > NAME_MAX {
        return Err(Error::NameTooLong);
    }
    Ok(b.to_vec())
}

fn exists(fs: &mut Fs, dir: Ino, name: &[u8]) -> Result<bool> {
    match fs.lookup(dir, name) {
        Ok(_) => Ok(true),
        Err(Error::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

/// `name`, or the first free of "stem (n).ext" (directories: "name (n)").
fn unique(fs: &mut Fs, dir: Ino, name: &[u8], is_dir: bool) -> Result<Vec<u8>> {
    if !exists(fs, dir, name)? {
        return Ok(name.to_vec());
    }
    let (stem, ext) = match name.iter().rposition(|&b| b == b'.') {
        Some(i) if !is_dir && i > 0 => (&name[..i], &name[i..]),
        _ => (name, &b""[..]),
    };
    for n in 1..=UNIQUE_TRIES {
        let mut candidate = stem.to_vec();
        candidate.extend_from_slice(format!(" ({n})").as_bytes());
        candidate.extend_from_slice(ext);
        if candidate.len() > NAME_MAX {
            return Err(Error::NameTooLong);
        }
        if !exists(fs, dir, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(Error::Exists)
}

/// Create a file or directory in the directory at `parent_path`, owned
/// like that directory; a taken name gets a number. Returns the new entry
/// and its encoded path.
pub fn create(vol: &Volume, parent_path: &str, name: &str, is_dir: bool) -> Result<(Entry, String)> {
    let name = check_name(name)?;
    vol.fs.with(|fs| {
        let d = dir(fs, parent_path)?;
        let owner = fs.stat(d)?;
        let name = unique(fs, d, &name, is_dir)?;
        let a = if is_dir {
            fs.mkdir(d, &name, 0o755, owner.uid, owner.gid)?
        } else {
            fs.create(d, &name, FileType::Regular, 0o644, owner.uid, owner.gid, 0)?
        };
        let e = docs::entry(&name, &a).ok_or(Error::NotFound)?;
        Ok((e, join(parent_path, &name)))
    })
}

/// Delete a document; a directory with everything in it.
pub fn delete(vol: &Volume, path: &str) -> Result<()> {
    vol.fs.with(|fs| {
        let (p, name) = parent(fs, path)?;
        let ino = fs.lookup(p, &name)?;
        let mut unlinked = Vec::new();
        if fs.stat(ino)?.file_type == FileType::Directory {
            empty_dir(fs, ino, &mut unlinked)?;
            fs.rmdir(p, &name)?;
        } else {
            fs.unlink(p, &name)?;
        }
        unlinked.push(ino);
        vol.reclaim(fs, &unlinked)
    })
}

fn has_entries(fs: &mut Fs, dir: Ino) -> Result<bool> {
    Ok(fs.list_dir(dir)?.iter().any(|e| e.name != b"." && e.name != b".."))
}

/// Delete everything in a directory, depth first; links themselves, never
/// their targets.
fn empty_dir(fs: &mut Fs, dir: Ino, unlinked: &mut Vec<Ino>) -> Result<()> {
    let mut stack = vec![dir];
    while let Some(&d) = stack.last() {
        let mut descend = None;
        for e in fs.list_dir(d)? {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            if fs.stat(e.ino)?.file_type == FileType::Directory {
                if has_entries(fs, e.ino)? {
                    descend = Some(e.ino);
                    break;
                }
                fs.rmdir(d, &e.name)?;
            } else {
                fs.unlink(d, &e.name)?;
            }
            unlinked.push(e.ino);
        }
        match descend {
            Some(sub) => stack.push(sub),
            // `d` is empty now; its parent removes it on the next pass
            None => {
                stack.pop();
            }
        }
    }
    Ok(())
}

/// Give a document a new name in the same directory; returns its new path.
pub fn rename(vol: &Volume, path: &str, new_name: &str) -> Result<String> {
    let new = check_name(new_name)?;
    vol.fs.with(|fs| {
        let (p, old) = parent(fs, path)?;
        if old != new {
            fs.rename(p, &old, p, &new, NO_REPLACE)?;
        }
        Ok(join(parent_path(path), &new))
    })
}

/// Move a document into the directory at `target_dir_path`, keeping its
/// name; returns its new path.
pub fn move_to(vol: &Volume, path: &str, target_dir_path: &str) -> Result<String> {
    vol.fs.with(|fs| {
        let (p, name) = parent(fs, path)?;
        let d = dir(fs, target_dir_path)?;
        if p != d {
            // the core refuses moving a directory into itself
            fs.rename(p, &name, d, &name, NO_REPLACE)?;
        }
        Ok(join(target_dir_path, &name))
    })
}

/// Whether directory `d` is `ancestor` or inside it.
fn is_inside(fs: &mut Fs, mut d: Ino, ancestor: Ino) -> Result<bool> {
    let root = fs.root();
    for _ in 0..MAX_DEPTH {
        if d == ancestor {
            return Ok(true);
        }
        if d == root {
            return Ok(false);
        }
        d = fs.lookup(d, b"..")?;
    }
    Err(Error::corrupt("directory tree too deep or looping"))
}

/// Copy a document (a directory with everything in it) into the directory
/// at `target_dir_path`; a taken name gets a number. Returns the copy's
/// path. A link given by path is copied as what it points to; links inside
/// a copied directory stay links.
pub fn copy(vol: &Volume, path: &str, target_dir_path: &str) -> Result<String> {
    let (src, attr, name, d) = vol.fs.with(|fs| {
        let comps = names::decode_path(path)?;
        let name = comps.last().cloned().ok_or_else(|| Error::invalid("cannot copy the root"))?;
        let mut stack = vec![fs.root()];
        let a = docs::walk(fs, &mut stack, comps, true)?;
        let d = dir(fs, target_dir_path)?;
        if a.is_dir() && is_inside(fs, d, a.ino)? {
            return Err(Error::invalid("cannot copy a directory into itself"));
        }
        Ok((a.ino, a, name, d))
    })?;
    let (top, used) = copy_one(vol, src, &attr, d, &name, true)?;
    let mut pending = Vec::new();
    if attr.is_dir() {
        pending.push((src, top));
    }
    while let Some((from, to)) = pending.pop() {
        for e in vol.fs.with(|fs| fs.list_dir(from))? {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let a = vol.fs.with(|fs| fs.stat(e.ino))?;
            let (new, _) = copy_one(vol, e.ino, &a, to, &e.name, false)?;
            if a.is_dir() {
                pending.push((e.ino, new));
            }
        }
    }
    Ok(join(target_dir_path, &used))
}

/// Copy one file, (empty) directory or link; returns the new inode (0 for
/// skipped special files) and the name used.
fn copy_one(vol: &Volume, src: Ino, a: &Attr, to: Ino, name: &[u8], unique_name: bool) -> Result<(Ino, Vec<u8>)> {
    let (ino, used) = vol.fs.with(|fs| {
        let used = if unique_name {
            unique(fs, to, name, a.is_dir())?
        } else {
            name.to_vec()
        };
        let new = match a.file_type {
            FileType::Directory => fs.mkdir(to, &used, a.perm, a.uid, a.gid)?,
            FileType::Regular => fs.create(to, &used, FileType::Regular, a.perm, a.uid, a.gid, 0)?,
            FileType::Symlink => {
                let target = fs.read_link(src)?;
                fs.symlink(to, &used, &target, a.uid, a.gid)?
            }
            _ => return Ok((0, used)),
        };
        Ok((new.ino, used))
    })?;
    if a.file_type == FileType::Regular {
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut off = 0u64;
        while off < a.size {
            let n = vol.fs.with(|fs| docs::read(fs, src, off, &mut buf))?;
            if n == 0 {
                break;
            }
            vol.fs.with(|fs| write_all(fs, ino, off, &buf[..n]))?;
            off += n as u64;
        }
    }
    Ok((ino, used))
}

fn write_all(fs: &mut Fs, ino: Ino, offset: u64, data: &[u8]) -> Result<()> {
    let mut done = 0;
    while done < data.len() {
        done += fs.write(ino, offset + done as u64, &data[done..])?;
    }
    Ok(())
}

/// Open the regular file at `path` for a descriptor, emptied first with
/// `truncate`; returns its inode and size. Pair with [`Volume::closed`].
pub fn open(vol: &Volume, path: &str, truncate: bool) -> Result<(Ino, u64)> {
    let (ino, size) = vol.fs.with(|fs| {
        let (ino, size) = docs::open(fs, path)?;
        if truncate && size > 0 {
            fs.truncate(ino, 0)?;
            return Ok((ino, 0));
        }
        Ok((ino, size))
    })?;
    vol.opened(ino);
    Ok((ino, size))
}

pub fn write(vol: &Volume, ino: Ino, offset: u64, data: &[u8]) -> Result<()> {
    vol.fs.with(|fs| write_all(fs, ino, offset, data))
}

pub fn size(vol: &Volume, ino: Ino) -> Result<u64> {
    vol.fs.with(|fs| Ok(fs.stat(ino)?.size))
}

/// fsync: what was written so far is committed to the journal.
pub fn sync(vol: &Volume) -> Result<()> {
    vol.fs.with(|fs| fs.commit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sample, volumes};
    use ext4_core::{FormatOptions, MemDevice, MountOptions};
    use std::sync::Arc;

    /// A writable sample volume in memory.
    fn volume() -> (u32, Arc<Volume>) {
        let dev = Arc::new(MemDevice::new(32 << 20));
        ext4_core::format(&*dev, &FormatOptions::default(), &mut |_, _| {}).unwrap();
        let mut fs = ext4_core::Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        sample::populate(&mut fs, 3 << 20).unwrap();
        fs.unmount().unwrap();
        let id = volumes::mount_device(dev, false).unwrap();
        (id, volumes::get(id).unwrap())
    }

    fn names(vol: &Volume, path: &str) -> Vec<String> {
        let mut v: Vec<String> = vol.fs.with(|fs| docs::list(fs, path)).unwrap().into_iter().map(|e| e.name).collect();
        v.sort();
        v
    }

    fn read_all(vol: &Volume, path: &str) -> Vec<u8> {
        let (ino, size) = vol.fs.with(|fs| docs::open(fs, path)).unwrap();
        let mut b = vec![0u8; size as usize];
        vol.fs.with(|fs| docs::read(fs, ino, 0, &mut b)).unwrap();
        b
    }

    #[test]
    fn create_with_numbered_names_owned_like_the_parent() {
        let (id, vol) = volume();
        let (e, path) = create(&vol, "docs", "notes.md", false).unwrap();
        assert_eq!((e.name.as_str(), path.as_str()), ("notes (1).md", "docs/notes (1).md"));
        let (_, path) = create(&vol, "docs", "notes.md", false).unwrap();
        assert_eq!(path, "docs/notes (2).md");
        let (e, path) = create(&vol, "", "docs", true).unwrap();
        assert_eq!((e.kind, path.as_str()), (docs::Kind::Dir, "docs (1)"));
        let (_, path) = create(&vol, "docs-link", "新建 文件夹", true).unwrap();
        assert_eq!(path, "docs-link/新建 文件夹");
        assert!(names(&vol, "docs").contains(&"新建 文件夹".to_string()));
        // the sample's root belongs to root, its directories to 1000
        let owner = |dir: &str, name: &[u8]| {
            vol.fs
                .with(|fs| {
                    let d = super::dir(fs, dir)?;
                    fs.lookup_attr(d, name)
                })
                .map(|a| (a.uid, a.gid))
                .unwrap()
        };
        assert_eq!(owner("", b"docs (1)"), (0, 0));
        assert_eq!(owner("docs", b"notes (1).md"), (1000, 1000));
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(create(&vol, "", bad, false).is_err(), "{bad:?}");
        }
        assert!(matches!(create(&vol, "", &"x".repeat(256), false), Err(Error::NameTooLong)));
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    #[test]
    fn delete_files_trees_and_links_but_not_link_targets() {
        let (id, vol) = volume();
        delete(&vol, "docs-link").unwrap();
        assert!(names(&vol, "").contains(&"docs".to_string()), "the target stays");
        delete(&vol, "deep").unwrap();
        delete(&vol, "README.txt").unwrap();
        let root = names(&vol, "");
        assert!(!root.iter().any(|n| n == "deep" || n == "docs-link" || n == "README.txt"), "{root:?}");
        assert!(matches!(delete(&vol, "deep"), Err(Error::NotFound)));
        assert!(delete(&vol, "").is_err());
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    #[test]
    fn rename_and_move() {
        let (id, vol) = volume();
        assert_eq!(rename(&vol, "docs/notes.md", "日记.md").unwrap(), "docs/日记.md");
        assert!(matches!(rename(&vol, "docs/日记.md", "100%.txt"), Err(Error::Exists)));
        assert_eq!(move_to(&vol, "docs/日记.md", "photos").unwrap(), "photos/日记.md");
        assert_eq!(read_all(&vol, "photos/日记.md"), b"# Notes\n\nA file in a directory.\n");
        assert_eq!(move_to(&vol, "deep", "docs").unwrap(), "docs/deep");
        assert!(move_to(&vol, "docs", "docs/deep/nested").is_err(), "into itself");
        assert_eq!(read_all(&vol, "docs-link/deep/nested/dir/file.txt"), b"three levels down\n");
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    #[test]
    fn copy_files_and_trees() {
        let (id, vol) = volume();
        assert_eq!(copy(&vol, "README.txt", "").unwrap(), "README (1).txt");
        assert_eq!(read_all(&vol, "README (1).txt"), read_all(&vol, "README.txt"));
        assert_eq!(copy(&vol, "big.bin", "docs").unwrap(), "docs/big.bin");
        assert_eq!(read_all(&vol, "docs/big.bin"), read_all(&vol, "big.bin"));
        assert_eq!(copy(&vol, "deep", "photos").unwrap(), "photos/deep");
        assert_eq!(read_all(&vol, "photos/deep/nested/dir/file.txt"), b"three levels down\n");
        // a link inside a copied tree stays a link with the same target
        assert_eq!(copy(&vol, "docs", "photos").unwrap(), "photos/docs");
        let target = vol
            .fs
            .with(|fs| {
                let d = dir(fs, "photos/docs")?;
                let link = fs.lookup(d, b"up-link")?;
                fs.read_link(link)
            })
            .unwrap();
        assert_eq!(target, b"../photos/gradient.bmp");
        assert!(copy(&vol, "docs", "docs-link").is_err(), "into itself");
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    #[test]
    fn write_through_a_descriptor_and_delete_while_open() {
        let (id, vol) = volume();
        let free = || vol.fs.with(|fs| Ok(fs.statfs().free_files)).unwrap();
        let (_, path) = create(&vol, "", "new.txt", false).unwrap();
        let before = free();
        let (ino, size) = open(&vol, &path, false).unwrap();
        assert_eq!(size, 0);
        write(&vol, ino, 0, b"hello ").unwrap();
        write(&vol, ino, 6, b"world").unwrap();
        assert_eq!(super::size(&vol, ino).unwrap(), 11);
        sync(&vol).unwrap();

        // deleted while open: still readable, freed on close
        delete(&vol, &path).unwrap();
        assert!(matches!(vol.fs.with(|fs| docs::stat(fs, &path)), Err(Error::NotFound)));
        let mut b = [0u8; 11];
        vol.fs.with(|fs| docs::read(fs, ino, 0, &mut b)).unwrap();
        assert_eq!(&b, b"hello world");
        assert_eq!(free(), before, "not freed while open");
        vol.closed(ino, true).unwrap();
        assert_eq!(free(), before + 1);

        // "wt" empties the file
        let (ino, _) = open(&vol, "README.txt", false).unwrap();
        vol.closed(ino, false).unwrap();
        let (ino, size) = open(&vol, "README.txt", true).unwrap();
        assert_eq!(size, 0);
        vol.closed(ino, true).unwrap();
        assert!(read_all(&vol, "README.txt").is_empty());
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    /// e2fsck of e2fsprogs (Homebrew's, or `E2FSPROGS_SBIN`), if installed.
    fn e2fsck() -> Option<std::path::PathBuf> {
        let dir = std::env::var_os("E2FSPROGS_SBIN").unwrap_or_else(|| "/opt/homebrew/opt/e2fsprogs/sbin".into());
        let p = std::path::Path::new(&dir).join("e2fsck");
        p.exists().then_some(p)
    }

    #[test]
    fn changes_leave_a_file_system_e2fsck_finds_clean() {
        let Some(e2fsck) = e2fsck() else {
            eprintln!("e2fsck not found; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("rw.img");
        sample::create(&img, 64).unwrap();
        let id = volumes::mount_image(&img, false).unwrap();
        let vol = volumes::get(id).unwrap();

        let (_, path) = create(&vol, "docs", "report.txt", false).unwrap();
        let (ino, _) = open(&vol, &path, false).unwrap();
        for i in 0..64u64 {
            write(&vol, ino, i * 65536, &[i as u8; 65536]).unwrap();
        }
        vol.closed(ino, true).unwrap();
        let path = rename(&vol, &path, "报告.txt").unwrap();
        let path = move_to(&vol, &path, "photos").unwrap();
        create(&vol, "photos", "相册", true).unwrap();
        copy(&vol, "docs", "photos/相册").unwrap();
        copy(&vol, "big.bin", "").unwrap();
        delete(&vol, "deep").unwrap();
        // deleted while open, freed on close
        let (ino, _) = open(&vol, &path, false).unwrap();
        delete(&vol, &path).unwrap();
        vol.closed(ino, false).unwrap();
        for i in 0..200 {
            create(&vol, "docs", &format!("many-{i}.txt"), false).unwrap();
        }
        delete(&vol, "docs").unwrap();
        drop(vol);
        volumes::unmount(id).unwrap();

        let out = std::process::Command::new(e2fsck).arg("-fn").arg(&img).output().unwrap();
        assert!(
            out.status.success(),
            "e2fsck -fn:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn read_only_volume_refuses_changes() {
        let dev = Arc::new(MemDevice::new(16 << 20));
        ext4_core::format(&*dev, &FormatOptions::default(), &mut |_, _| {}).unwrap();
        let id = volumes::mount_device(dev, true).unwrap();
        let vol = volumes::get(id).unwrap();
        assert!(matches!(create(&vol, "", "x", false), Err(Error::ReadOnly)));
        assert!(matches!(delete(&vol, "lost+found"), Err(Error::ReadOnly)));
        drop(vol);
        volumes::unmount(id).unwrap();
    }
}
