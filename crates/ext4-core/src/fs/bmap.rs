//! Logical → physical block mapping for both extent-mapped and classic
//! indirect-block-mapped inodes.

use super::extent::Mapping;
use super::{Fs, Ino};
use crate::bytes::le32;
use crate::error::{Error, Result};
use crate::ondisk::extent::Extent;
use crate::ondisk::inode::{Inode, flags};

const DIRECT: u64 = 12;

impl Fs {
    /// Map one logical block.
    pub(crate) fn map_block(&mut self, ino: Ino, inode: &Inode, lblk: u64) -> Result<Mapping> {
        if lblk >= 1 << 32 {
            return Ok(Mapping::Hole { len: u64::MAX });
        }
        if inode.has_flag(flags::EXTENTS) {
            self.ext_map(ino, inode, lblk as u32)
        } else {
            self.ind_map(inode, lblk)
        }
    }

    pub(crate) fn ind_ptr(&mut self, blk: u64, i: u64) -> Result<u32> {
        if blk >= self.sb.blocks_count() {
            return Err(Error::corrupt(format!("indirect block {blk} out of range")));
        }
        let b = self.cache.get(&*self.dev, blk)?;
        Ok(le32(b, (i * 4) as usize))
    }

    /// Indirect mapping: returns a single-block mapping (plus a contiguous
    /// run length when neighbouring pointers are consecutive).
    fn ind_map(&mut self, inode: &Inode, lblk: u64) -> Result<Mapping> {
        let per = self.bs as u64 / 4;
        let (ptr, table, idx) = if lblk < DIRECT {
            (inode.block_ptr(lblk as usize), None, lblk)
        } else {
            let mut rel = lblk - DIRECT;
            let mut levels = 1;
            let mut span = per;
            while rel >= span {
                rel -= span;
                levels += 1;
                if levels > 3 {
                    return Ok(Mapping::Hole { len: u64::MAX });
                }
                span *= per;
            }
            let root = inode.block_ptr(11 + levels) as u64;
            if root == 0 {
                return Ok(Mapping::Hole { len: span - rel });
            }
            let mut blk = root;
            let mut sub = span / per;
            for _ in 1..levels {
                let p = self.ind_ptr(blk, rel / sub)? as u64;
                if p == 0 {
                    return Ok(Mapping::Hole { len: sub - rel % sub });
                }
                rel %= sub;
                blk = p;
                sub /= per;
            }
            (self.ind_ptr(blk, rel)?, Some(blk), rel)
        };
        if ptr == 0 {
            // the hole extends over the following zero pointers of this table
            let mut len = 1u64;
            match table {
                None => {
                    while idx + len < DIRECT && inode.block_ptr((idx + len) as usize) == 0 {
                        len += 1;
                    }
                }
                Some(t) => {
                    while idx + len < per && self.ind_ptr(t, idx + len)? == 0 {
                        len += 1;
                    }
                }
            }
            return Ok(Mapping::Hole { len });
        }
        // extend the run while pointers are consecutive in the same table
        let mut len = 1u64;
        match table {
            None => {
                while idx + len < DIRECT && inode.block_ptr((idx + len) as usize) as u64 == ptr as u64 + len {
                    len += 1;
                }
            }
            Some(t) => {
                while idx + len < per && self.ind_ptr(t, idx + len)? as u64 == ptr as u64 + len {
                    len += 1;
                }
            }
        }
        Ok(Mapping::Mapped {
            pblk: ptr as u64,
            len,
            unwritten: false,
        })
    }

    /// All mapped extents of an inode, in logical order.
    pub(crate) fn all_extents(&mut self, ino: Ino, inode: &Inode) -> Result<Vec<Extent>> {
        if inode.has_flag(flags::EXTENTS) {
            return self.ext_all(ino, inode);
        }
        let mut out: Vec<Extent> = Vec::new();
        let blocks = inode.size().div_ceil(self.bs as u64);
        let mut l = 0u64;
        while l < blocks {
            match self.ind_map(inode, l)? {
                Mapping::Mapped { pblk, len, .. } => {
                    let len = len.min(blocks - l);
                    if let Some(last) = out.last_mut()
                        && last.end() == l
                        && last.start + last.len as u64 == pblk
                        && last.len as u64 + len <= 32768
                    {
                        last.len += len as u32;
                    } else {
                        out.push(Extent {
                            block: l as u32,
                            len: len as u32,
                            start: pblk,
                            unwritten: false,
                        });
                    }
                    l += len;
                }
                Mapping::Hole { len } => l = l.saturating_add(len),
            }
        }
        Ok(out)
    }

    /// Indirect (metadata) blocks of a block-mapped inode.
    pub(crate) fn ind_meta_blocks(&mut self, inode: &Inode) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        let per = self.bs as u64 / 4;
        for level in 1..=3u32 {
            let root = inode.block_ptr(11 + level as usize) as u64;
            if root != 0 {
                self.ind_collect(root, level, per, &mut out)?;
            }
        }
        Ok(out)
    }

    fn ind_collect(&mut self, blk: u64, level: u32, per: u64, out: &mut Vec<u64>) -> Result<()> {
        out.push(blk);
        if level == 1 {
            return Ok(());
        }
        for i in 0..per {
            let p = self.ind_ptr(blk, i)? as u64;
            if p != 0 {
                self.ind_collect(p, level - 1, per, out)?;
            }
        }
        Ok(())
    }
}
