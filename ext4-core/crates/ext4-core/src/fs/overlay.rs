//! Read-only overlay: serves block reads from an in-memory map first.
//!
//! Used when a journal needs replay but the mount (or device) is read-only:
//! the replayed blocks are kept in memory instead of being written.

use crate::device::BlockDevice;
use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct OverlayDevice {
    inner: Arc<dyn BlockDevice>,
    block_size: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl OverlayDevice {
    pub fn new(inner: Arc<dyn BlockDevice>, block_size: u32, blocks: BTreeMap<u64, Vec<u8>>) -> Self {
        OverlayDevice {
            inner,
            block_size: block_size as u64,
            blocks,
        }
    }
}

impl BlockDevice for OverlayDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)?;
        if self.blocks.is_empty() {
            return Ok(());
        }
        let bs = self.block_size;
        let first = offset / bs;
        let last = (offset + buf.len() as u64).div_ceil(bs);
        for (&b, data) in self.blocks.range(first..last) {
            let bstart = b * bs;
            let s = bstart.max(offset);
            let e = (bstart + bs).min(offset + buf.len() as u64);
            buf[(s - offset) as usize..(e - offset) as usize]
                .copy_from_slice(&data[(s - bstart) as usize..(e - bstart) as usize]);
        }
        Ok(())
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<()> {
        Err(Error::ReadOnly)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    #[test]
    fn overlay_reads() {
        let base = Arc::new(MemDevice::from_vec(vec![1u8; 4096]));
        let mut m = BTreeMap::new();
        m.insert(1, vec![9u8; 1024]);
        let o = OverlayDevice::new(base, 1024, m);
        let mut b = vec![0u8; 2048];
        o.read_at(512, &mut b).unwrap();
        assert!(b[..512].iter().all(|&x| x == 1));
        assert!(b[512..1536].iter().all(|&x| x == 9));
        assert!(b[1536..].iter().all(|&x| x == 1));
        let mut c = [0u8; 4];
        o.read_at(1030, &mut c).unwrap();
        assert_eq!(c, [9; 4]);
        assert!(o.write_at(0, &[0]).is_err());
        assert!(o.is_read_only());
        assert_eq!(o.size(), 4096);
        o.flush().unwrap();
    }
}
