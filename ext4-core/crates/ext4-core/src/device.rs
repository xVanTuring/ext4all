//! Block device abstraction.
//!
//! The ext4 core never touches the OS directly: everything goes through
//! [`BlockDevice`]. In production the FFI layer implements it with callbacks
//! into FSKit's `FSBlockDeviceResource`; tests use [`FileDevice`] over an image
//! file or [`MemDevice`].

use crate::error::{Error, Result};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

/// A byte-addressable, random access block device.
pub trait BlockDevice: Send + Sync {
    /// Read exactly `buf.len()` bytes at `offset`.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Write all of `buf` at `offset`.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()>;
    /// Make previously written data durable (write barrier).
    fn flush(&self) -> Result<()>;
    /// Device size in bytes.
    fn size(&self) -> u64;
    /// Whether the device refuses writes.
    fn is_read_only(&self) -> bool {
        false
    }
    /// Minimal I/O granularity. Accesses are aligned by [`AlignedDevice`].
    fn sector_size(&self) -> u32 {
        512
    }
}

fn check_range(offset: u64, len: usize, size: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(()),
        _ => Err(Error::invalid(format!(
            "I/O beyond end of device: offset {offset} len {len} size {size}"
        ))),
    }
}

/// Device backed by a regular file (disk image).
pub struct FileDevice {
    file: File,
    size: u64,
    read_only: bool,
}

impl FileDevice {
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileDevice { file, size, read_only })
    }
}

impl BlockDevice for FileDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        check_range(offset, buf.len(), self.size)?;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if !self.read_only {
            self.file.sync_data()?;
        }
        Ok(())
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// In-memory device, handy for unit tests and crash simulation.
pub struct MemDevice {
    data: Mutex<Vec<u8>>,
    read_only: bool,
    /// When set, writes fail with `EIO` after this many more write calls.
    fail_after: Mutex<Option<usize>>,
    flushes: Mutex<usize>,
}

impl MemDevice {
    pub fn new(size: usize) -> Self {
        Self::from_vec(vec![0; size])
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        MemDevice {
            data: Mutex::new(data),
            read_only: false,
            fail_after: Mutex::new(None),
            flushes: Mutex::new(0),
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }

    /// Make every write after the next `n` writes fail (simulated power loss).
    pub fn fail_writes_after(&self, n: Option<usize>) {
        *self.fail_after.lock().unwrap() = n;
    }

    pub fn flush_count(&self) -> usize {
        *self.flushes.lock().unwrap()
    }
}

impl BlockDevice for MemDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let data = self.data.lock().unwrap();
        check_range(offset, buf.len(), data.len() as u64)?;
        let o = offset as usize;
        buf.copy_from_slice(&data[o..o + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        {
            let mut fail = self.fail_after.lock().unwrap();
            if let Some(n) = fail.as_mut() {
                if *n == 0 {
                    return Err(Error::Device(crate::error::errno::EIO));
                }
                *n -= 1;
            }
        }
        let mut data = self.data.lock().unwrap();
        check_range(offset, buf.len(), data.len() as u64)?;
        let o = offset as usize;
        data[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }

    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Wraps a device whose I/O must be aligned to its sector size (e.g. a raw
/// disk with 4K sectors accessed through FSKit) and turns unaligned accesses
/// into aligned read-modify-write cycles.
pub struct AlignedDevice<D: BlockDevice> {
    inner: D,
    sector: u64,
}

impl<D: BlockDevice> AlignedDevice<D> {
    pub fn new(inner: D) -> Self {
        let sector = inner.sector_size().max(1) as u64;
        AlignedDevice { inner, sector }
    }

    pub fn inner(&self) -> &D {
        &self.inner
    }

    fn span(&self, offset: u64, len: usize) -> (u64, usize) {
        let start = offset / self.sector * self.sector;
        let end = (offset + len as u64).div_ceil(self.sector) * self.sector;
        (start, (end - start) as usize)
    }

    fn aligned(&self, offset: u64, len: usize) -> bool {
        offset % self.sector == 0 && (len as u64) % self.sector == 0
    }
}

impl<D: BlockDevice> BlockDevice for AlignedDevice<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.aligned(offset, buf.len()) {
            return self.inner.read_at(offset, buf);
        }
        let (start, len) = self.span(offset, buf.len());
        let mut tmp = vec![0u8; len];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.aligned(offset, buf.len()) {
            return self.inner.write_at(offset, buf);
        }
        let (start, len) = self.span(offset, buf.len());
        let mut tmp = vec![0u8; len];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        tmp[skip..skip + buf.len()].copy_from_slice(buf);
        self.inner.write_at(start, &tmp)
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn sector_size(&self) -> u32 {
        self.sector as u32
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        (**self).flush()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for std::sync::Arc<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        (**self).write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        (**self).flush()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
    fn sector_size(&self) -> u32 {
        (**self).sector_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Device that rejects unaligned I/O, to test [`AlignedDevice`].
    struct StrictDevice {
        mem: MemDevice,
        sector: u32,
    }

    impl BlockDevice for StrictDevice {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            assert_eq!(offset % self.sector as u64, 0, "unaligned read offset");
            assert_eq!(buf.len() % self.sector as usize, 0, "unaligned read len");
            self.mem.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            assert_eq!(offset % self.sector as u64, 0, "unaligned write offset");
            assert_eq!(buf.len() % self.sector as usize, 0, "unaligned write len");
            self.mem.write_at(offset, buf)
        }
        fn flush(&self) -> Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.mem.size()
        }
        fn sector_size(&self) -> u32 {
            self.sector
        }
    }

    #[test]
    fn mem_device_rw() {
        let d = MemDevice::new(4096);
        d.write_at(100, b"hello").unwrap();
        let mut b = [0u8; 5];
        d.read_at(100, &mut b).unwrap();
        assert_eq!(&b, b"hello");
        assert_eq!(d.size(), 4096);
    }

    #[test]
    fn mem_device_bounds() {
        let d = MemDevice::new(16);
        let mut b = [0u8; 8];
        assert!(d.read_at(9, &mut b).is_err());
        assert!(d.write_at(u64::MAX, &b).is_err());
        assert!(d.read_at(8, &mut b).is_ok());
    }

    #[test]
    fn mem_device_read_only() {
        let d = MemDevice::new(16).read_only();
        assert!(matches!(d.write_at(0, b"x"), Err(Error::ReadOnly)));
        assert!(d.is_read_only());
    }

    #[test]
    fn mem_device_fail_after() {
        let d = MemDevice::new(16);
        d.fail_writes_after(Some(1));
        d.write_at(0, b"a").unwrap();
        assert!(d.write_at(1, b"b").is_err());
        d.fail_writes_after(None);
        d.write_at(1, b"b").unwrap();
        assert_eq!(&d.snapshot()[..2], b"ab");
    }

    #[test]
    fn mem_device_counts_flushes() {
        let d = MemDevice::new(16);
        d.flush().unwrap();
        d.flush().unwrap();
        assert_eq!(d.flush_count(), 2);
    }

    #[test]
    fn file_device_rw() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("img");
        std::fs::write(&p, vec![0u8; 8192]).unwrap();
        let d = FileDevice::open(&p, false).unwrap();
        assert_eq!(d.size(), 8192);
        d.write_at(4000, b"abcdef").unwrap();
        d.flush().unwrap();
        let mut b = [0u8; 6];
        d.read_at(4000, &mut b).unwrap();
        assert_eq!(&b, b"abcdef");
        assert!(d.read_at(8190, &mut b).is_err());
        drop(d);
        let ro = FileDevice::open(&p, true).unwrap();
        assert!(ro.is_read_only());
        assert!(matches!(ro.write_at(0, b"x"), Err(Error::ReadOnly)));
        let mut b = [0u8; 6];
        ro.read_at(4000, &mut b).unwrap();
        assert_eq!(&b, b"abcdef");
    }

    #[test]
    fn aligned_device_unaligned_rw() {
        let strict = StrictDevice {
            mem: MemDevice::new(16384),
            sector: 4096,
        };
        let d = AlignedDevice::new(strict);
        d.write_at(1024, &[7u8; 1024]).unwrap();
        d.write_at(4090, b"crossing").unwrap();
        let mut b = vec![0u8; 1024];
        d.read_at(1024, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 7));
        let mut c = [0u8; 8];
        d.read_at(4090, &mut c).unwrap();
        assert_eq!(&c, b"crossing");
        // untouched bytes stay zero
        let mut z = [0xffu8; 4];
        d.read_at(0, &mut z).unwrap();
        assert_eq!(z, [0; 4]);
        // aligned fast path
        d.write_at(8192, &[1u8; 4096]).unwrap();
        let mut a = vec![0u8; 4096];
        d.read_at(8192, &mut a).unwrap();
        assert!(a.iter().all(|&x| x == 1));
        assert_eq!(d.sector_size(), 4096);
    }

    #[test]
    fn arc_and_box_forwarding() {
        let d: Arc<dyn BlockDevice> = Arc::new(MemDevice::new(64));
        d.write_at(0, b"xy").unwrap();
        let b: Box<dyn BlockDevice> = Box::new(MemDevice::new(64));
        b.write_at(1, b"z").unwrap();
        let mut r = [0u8; 2];
        d.read_at(0, &mut r).unwrap();
        assert_eq!(&r, b"xy");
        assert_eq!(b.size(), 64);
        assert_eq!(d.sector_size(), 512);
    }
}
