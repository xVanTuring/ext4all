//! Mounted volumes, known to Kotlin by number.

use ext4_core::{BlockDevice, Error, FileDevice, Fs, Ino, MountOptions, Result, SharedFs, Slice};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use crate::files::OpenFile;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Changes reach the journal at least this often.
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Volume {
    pub fs: SharedFs,
    pub label: String,
    pub uuid: [u8; 16],
    /// What the file system is on (a partition of a USB disk, an image).
    dev: Arc<dyn BlockDevice>,
    /// Inodes with open descriptors. The volume is mounted with deferred
    /// unlinking: a deleted inode is freed only once no descriptor uses it
    /// (Linux semantics).
    open: Mutex<HashMap<Ino, OpenInode>>,
    /// Descriptors handed out, by handle.
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    next_file: AtomicU64,
}

/// Descriptors of an open inode, and how often its data changed since it
/// was opened: read-ahead of the descriptors is stale after a change.
#[derive(Default)]
struct OpenInode {
    descriptors: u32,
    changes: u64,
}

impl Volume {
    fn open_table(&self) -> MutexGuard<'_, HashMap<Ino, OpenInode>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn files_table(&self) -> MutexGuard<'_, HashMap<u64, Arc<OpenFile>>> {
        self.files.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_open(&self, ino: Ino) -> bool {
        self.open_table().contains_key(&ino)
    }

    /// Hand out a descriptor for `ino`; returns its handle.
    pub fn open_file(&self, ino: Ino) -> u64 {
        self.open_table().entry(ino).or_default().descriptors += 1;
        let handle = self.next_file.fetch_add(1, Ordering::Relaxed);
        self.files_table().insert(handle, Arc::new(OpenFile::new(ino)));
        handle
    }

    pub fn file(&self, handle: u64) -> Result<Arc<OpenFile>> {
        self.files_table()
            .get(&handle)
            .cloned()
            .ok_or_else(|| Error::invalid(format!("no open file {handle}")))
    }

    /// The descriptor `handle` was closed: free its inode if it was deleted
    /// meanwhile and this was its last descriptor; commit what was written.
    pub fn close_file(&self, handle: u64, written: bool) -> Result<()> {
        let ino = self
            .files_table()
            .remove(&handle)
            .ok_or_else(|| Error::invalid(format!("no open file {handle}")))?
            .ino;
        let last = {
            let mut t = self.open_table();
            match t.get_mut(&ino) {
                Some(o) if o.descriptors > 1 => {
                    o.descriptors -= 1;
                    false
                }
                Some(_) => {
                    t.remove(&ino);
                    true
                }
                None => false,
            }
        };
        // the table lock is released before the file system lock is taken
        self.fs.with(|fs| {
            if last {
                fs.reclaim(ino)?;
            }
            if written { fs.commit() } else { Ok(()) }
        })
    }

    /// The data of `ino` changed (written, truncated): what descriptors
    /// read ahead of it is stale. Call it once the change is done.
    pub fn changed(&self, ino: Ino) {
        if let Some(o) = self.open_table().get_mut(&ino) {
            o.changes += 1;
        }
    }

    /// Changes of `ino`'s data while it has been open (see
    /// [`Volume::changed`]).
    pub fn changes(&self, ino: Ino) -> u64 {
        self.open_table().get(&ino).map_or(0, |o| o.changes)
    }

    /// Free inodes an operation unlinked, unless a descriptor still uses
    /// them (then [`Volume::close_file`] frees them).
    pub fn reclaim(&self, fs: &mut Fs, inos: &[Ino]) -> Result<()> {
        for &ino in inos {
            if !self.is_open(ino) {
                fs.reclaim(ino)?;
            }
        }
        Ok(())
    }
}

struct Registry {
    next: u32,
    volumes: BTreeMap<u32, Arc<Volume>>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        next: 1,
        volumes: BTreeMap::new(),
    })
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn get(id: u32) -> Result<Arc<Volume>> {
    registry()
        .volumes
        .get(&id)
        .cloned()
        .ok_or_else(|| Error::invalid(format!("no mounted volume {id}")))
}

fn add(v: Volume) -> u32 {
    let mut r = registry();
    let id = r.next;
    r.next += 1;
    r.volumes.insert(id, Arc::new(v));
    id
}

/// Commit, mark the file system clean and forget the volume. Reads still
/// running on other threads finish first (they hold the lock).
pub fn unmount(id: u32) -> Result<()> {
    let v = registry()
        .volumes
        .remove(&id)
        .ok_or_else(|| Error::invalid(format!("no mounted volume {id}")))?;
    v.fs.unmount()
}

/// Forget the volume without writing anything more: its disk is gone.
/// Operations still running fail.
pub fn abandon(id: u32) -> Result<()> {
    let v = registry()
        .volumes
        .remove(&id)
        .ok_or_else(|| Error::invalid(format!("no mounted volume {id}")))?;
    v.fs.abandon();
    Ok(())
}

/// The file systems of a device: the device itself, or else every
/// partition holding ext2/3/4, in table order.
pub fn find_volumes(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<Vec<Arc<dyn BlockDevice>>> {
    if Fs::probe(&*dev).is_ok() {
        return Ok(vec![dev]);
    }
    let mut read = |o: u64, b: &mut [u8]| dev.read_at(o, b);
    let table = part::read(&mut read, dev.sector_size(), dev.size()).map_err(|e| Error::invalid(e.to_string()))?;
    let mut found: Vec<Arc<dyn BlockDevice>> = Vec::new();
    let mut encrypted = matches!(table, part::Table::None) && is_luks(&*dev);
    for p in table.partitions() {
        let s = Slice::new(dev.clone(), p.start, p.len, read_only)?;
        if Fs::probe(&s).is_ok() {
            found.push(Arc::new(s));
        } else {
            encrypted |= is_luks(&s);
        }
    }
    if found.is_empty() {
        // unlocking comes with milestone M5
        return Err(Error::unsupported(if encrypted {
            "encrypted (LUKS) volumes are not supported yet"
        } else {
            "no ext2/3/4 file system found"
        }));
    }
    Ok(found)
}

fn is_luks(dev: &dyn BlockDevice) -> bool {
    matches!(ext4_core::luks::Header::read(dev), Ok(Some(_)))
}

pub fn mount_on(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<u32> {
    let mut fs = Fs::mount(
        dev.clone(),
        MountOptions {
            read_only,
            ..Default::default()
        },
    )?;
    // a file deleted while open stays readable until it is closed
    fs.set_defer_unlinked(true);
    let label = fs.label();
    let uuid = fs.superblock().uuid();
    Ok(add(Volume {
        fs: SharedFs::new(fs, COMMIT_INTERVAL),
        label,
        uuid,
        dev,
        open: Mutex::new(HashMap::new()),
        files: Mutex::new(HashMap::new()),
        next_file: AtomicU64::new(1),
    }))
}

/// MB/s reading up to `bytes` from the start of the volume's device in
/// 1 MiB reads, beside the file system: the speed of the disk (for a USB
/// disk, of its transfers) without the file system and the documents
/// provider.
pub fn device_read_speed(v: &Volume, bytes: u64) -> Result<f64> {
    const CHUNK: u64 = 1 << 20;
    let total = bytes.min(v.dev.size()) / CHUNK * CHUNK;
    let mut buf = vec![0u8; CHUNK as usize];
    let started = Instant::now();
    let mut done = 0;
    while done < total {
        v.dev.read_at(done, &mut buf)?;
        done += CHUNK;
    }
    Ok(total as f64 / 1e6 / started.elapsed().as_secs_f64().max(1e-9))
}

/// Mount the file system of a device (tests: an in-memory device).
#[cfg(test)]
pub fn mount_device(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<u32> {
    mount_on(dev, read_only)
}

/// Mount the ext4 volume in an image file (a file system image, or a disk
/// image with a partition table).
pub fn mount_image(path: &Path, read_only: bool) -> Result<u32> {
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(path, read_only)?);
    let first = find_volumes(dev, read_only)?.swap_remove(0);
    mount_on(first, read_only)
}

/// Volume facts for Kotlin (`VolumeInfo`), little-endian: label (u16
/// length + UTF-8), UUID (16 bytes), total and available bytes (u64 each),
/// read-only (u8).
pub fn info(v: &Volume) -> Result<Vec<u8>> {
    let (st, read_only) = v.fs.with(|fs| Ok((fs.statfs(), fs.is_read_only())))?;
    let bs = st.block_size as u64;
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&(v.label.len() as u16).to_le_bytes());
    out.extend_from_slice(v.label.as_bytes());
    out.extend_from_slice(&v.uuid);
    out.extend_from_slice(&(st.blocks * bs).to_le_bytes());
    out.extend_from_slice(&(st.avail_blocks * bs).to_le_bytes());
    out.push(read_only as u8);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{docs, sample};
    use ext4_core::FormatOptions;

    #[test]
    fn mount_list_info_and_unmount_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("sample.img");
        sample::create(&img, 64).unwrap();
        let id = mount_image(&img, true).unwrap();
        let v = get(id).unwrap();
        assert_eq!(v.label, "SAMPLE");
        let entries = v.fs.with(|fs| docs::list(fs, "")).unwrap();
        assert!(entries.iter().any(|e| e.name == "big.bin" && e.size == 16 << 20));
        assert!(device_read_speed(&v, 8 << 20).unwrap() > 0.0);
        let info = info(&v).unwrap();
        assert_eq!(&info[0..2], &6u16.to_le_bytes());
        assert_eq!(&info[2..8], b"SAMPLE");
        assert_eq!(*info.last().unwrap(), 1, "read-only");
        drop(v);
        unmount(id).unwrap();
        assert!(get(id).is_err());
        assert!(unmount(id).is_err());
    }

    #[test]
    fn finds_the_ext4_partition_of_a_disk_image() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.img");
        let f = std::fs::File::create(&img).unwrap();
        f.set_len(40 << 20).unwrap();
        drop(f);
        // MBR with one Linux partition from 1 MiB to the end
        let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&img, false).unwrap());
        let (start, count) = (2048u32, (40u32 << 11) - 2048);
        let mut mbr = [0u8; 512];
        mbr[446 + 4] = 0x83;
        mbr[446 + 8..446 + 12].copy_from_slice(&start.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&count.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        dev.write_at(0, &mbr).unwrap();
        let part = Slice::new(dev, start as u64 * 512, count as u64 * 512, false).unwrap();
        let opts = FormatOptions {
            label: "PART".into(),
            ..Default::default()
        };
        ext4_core::format(&part, &opts, &mut |_, _| {}).unwrap();

        let id = mount_image(&img, true).unwrap();
        assert_eq!(get(id).unwrap().label, "PART");
        unmount(id).unwrap();
    }

    #[test]
    fn image_without_ext4_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("zero.img");
        std::fs::File::create(&img).unwrap().set_len(4 << 20).unwrap();
        assert!(matches!(mount_image(&img, true), Err(Error::Unsupported(_))));
    }
}
