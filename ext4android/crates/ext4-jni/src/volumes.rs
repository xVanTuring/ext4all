//! Mounted volumes, known to Kotlin by number.

use ext4_core::{BlockDevice, Error, FileDevice, Fs, MountOptions, Result, SharedFs, Slice};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

/// Changes reach the journal at least this often.
const COMMIT_INTERVAL: Duration = Duration::from_secs(5);

pub struct Volume {
    pub fs: SharedFs,
    pub label: String,
    pub uuid: [u8; 16],
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

/// The file system of a device: the device itself, or else the first
/// partition holding ext2/3/4.
fn find_volume(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<Arc<dyn BlockDevice>> {
    if Fs::probe(&*dev).is_ok() {
        return Ok(dev);
    }
    let mut read = |o: u64, b: &mut [u8]| dev.read_at(o, b);
    let table = part::read(&mut read, dev.sector_size(), dev.size()).map_err(|e| Error::invalid(e.to_string()))?;
    for p in table.partitions() {
        let s = Slice::new(dev.clone(), p.start, p.len, read_only)?;
        if Fs::probe(&s).is_ok() {
            return Ok(Arc::new(s));
        }
    }
    Err(Error::unsupported("no ext2/3/4 file system found"))
}

fn mount_on(dev: Arc<dyn BlockDevice>, read_only: bool) -> Result<u32> {
    let mut fs = Fs::mount(
        dev,
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
    }))
}

/// Mount the ext4 volume in an image file (a file system image, or a disk
/// image with a partition table).
pub fn mount_image(path: &Path, read_only: bool) -> Result<u32> {
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(path, read_only)?);
    mount_on(find_volume(dev, read_only)?, read_only)
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
