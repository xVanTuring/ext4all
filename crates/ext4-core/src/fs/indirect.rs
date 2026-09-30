//! Classic (ext2/ext3) indirect block maps: allocation and truncation.
//!
//! `i_block[0..12]` point at data blocks, `i_block[12..15]` at single,
//! double and triple indirect blocks holding 32-bit pointers. Indirect
//! blocks are metadata and go through the cache (and journal).

use super::extent::Mapping;
use super::{Fs, Ino};
use crate::bytes::{le32, set_le32};
use crate::error::{Error, Result};
use crate::ondisk::inode::Inode;

const DIRECT: u64 = 12;

impl Fs {
    fn ptrs_per_block(&self) -> u64 {
        self.bs as u64 / 4
    }

    /// Number of logical blocks addressable by a block map.
    pub(crate) fn ind_max_blocks(&self) -> u64 {
        let p = self.ptrs_per_block();
        DIRECT + p + p * p + p * p * p
    }

    /// Root slot in `i_block` and per-level offsets for `lblk`.
    fn ind_path(&self, lblk: u64) -> Result<(usize, Vec<u64>)> {
        let p = self.ptrs_per_block();
        if lblk < DIRECT {
            return Ok((lblk as usize, Vec::new()));
        }
        let mut rel = lblk - DIRECT;
        if rel < p {
            return Ok((12, vec![rel]));
        }
        rel -= p;
        if rel < p * p {
            return Ok((13, vec![rel / p, rel % p]));
        }
        rel -= p * p;
        if rel < p * p * p {
            return Ok((14, vec![rel / (p * p), (rel / p) % p, rel % p]));
        }
        Err(Error::TooBig)
    }

    fn adjust_blocks(&self, inode: &mut Inode, delta: i64) {
        let per = self.bs as i64 / 512;
        let cur = inode.sectors(self.bs, self.huge_file()) as i64;
        inode.set_sectors((cur + delta * per).max(0) as u64);
    }

    /// A fresh zeroed indirect block near `goal`.
    fn ind_new_block(&mut self, inode: &mut Inode, goal: u64) -> Result<u32> {
        let (b, _) = self.alloc_blocks(goal, 1)?;
        if b > u32::MAX as u64 {
            self.free_blocks(b, 1)?;
            return Err(Error::NoSpace);
        }
        self.cache.zeroed(b);
        self.adjust_blocks(inode, 1);
        Ok(b as u32)
    }

    fn set_ptr(&mut self, blk: u64, i: u64, v: u32) -> Result<()> {
        let b = self.cache.get_mut(&*self.dev, blk)?;
        set_le32(b, (i * 4) as usize, v);
        Ok(())
    }

    /// Map logical block `lblk` to the (already allocated) data block
    /// `pblk`, allocating missing indirect blocks. Does not account for the
    /// data block in `i_blocks`.
    pub(crate) fn ind_set(&mut self, inode: &mut Inode, lblk: u64, pblk: u64, goal: u64) -> Result<()> {
        if pblk > u32::MAX as u64 {
            return Err(Error::invalid("block number does not fit a block map"));
        }
        let (slot, offs) = self.ind_path(lblk)?;
        if offs.is_empty() {
            inode.set_block_ptr(slot, pblk as u32);
            return Ok(());
        }
        let mut blk = inode.block_ptr(slot) as u64;
        if blk == 0 {
            blk = self.ind_new_block(inode, goal)? as u64;
            inode.set_block_ptr(slot, blk as u32);
        }
        for (depth, &off) in offs.iter().enumerate() {
            if depth + 1 == offs.len() {
                self.set_ptr(blk, off, pblk as u32)?;
                break;
            }
            let mut next = self.ind_ptr(blk, off)? as u64;
            if next == 0 {
                next = self.ind_new_block(inode, goal)? as u64;
                self.set_ptr(blk, off, next as u32)?;
            }
            blk = next;
        }
        Ok(())
    }

    /// Allocation goal for block `lblk` of a block-mapped inode.
    pub(crate) fn ind_goal(&mut self, ino: Ino, inode: &Inode, lblk: u64) -> Result<u64> {
        if lblk > 0
            && let Mapping::Mapped { pblk, .. } = self.map_block(ino, inode, lblk - 1)?
        {
            return Ok(pblk + 1);
        }
        let ipg = self.sb.inodes_per_group();
        Ok(self.group_first_block((ino - 1) / ipg))
    }

    /// Unmap and free logical blocks `[from, to)` and every indirect block
    /// that becomes empty.
    pub(crate) fn ind_free_range(&mut self, inode: &mut Inode, from: u64, to: u64) -> Result<()> {
        let to = to.min(self.ind_max_blocks());
        if from >= to {
            return Ok(());
        }
        for i in from.min(DIRECT)..to.min(DIRECT) {
            let p = inode.block_ptr(i as usize);
            if p != 0 {
                self.free_blocks(p as u64, 1)?;
                inode.set_block_ptr(i as usize, 0);
                self.adjust_blocks(inode, -1);
            }
        }
        let p = self.ptrs_per_block();
        let roots = [
            (12usize, DIRECT, 1u32, 1u64),
            (13, DIRECT + p, 2, p),
            (14, DIRECT + p + p * p, 3, p * p),
        ];
        for (slot, base, level, span) in roots {
            let root = inode.block_ptr(slot) as u64;
            if root == 0 {
                continue;
            }
            let hi = base + span * p;
            if hi <= from || base >= to {
                continue;
            }
            if self.ind_trim(inode, root, level, base, span, from, to)? {
                self.free_blocks(root, 1)?;
                inode.set_block_ptr(slot, 0);
                self.adjust_blocks(inode, -1);
            }
        }
        Ok(())
    }

    /// Free the part of the subtree at `blk` covering `[from, to)`. Each
    /// pointer of this node covers `span` logical blocks from `base`.
    /// Returns whether the node is now empty.
    fn ind_trim(
        &mut self,
        inode: &mut Inode,
        blk: u64,
        level: u32,
        base: u64,
        span: u64,
        from: u64,
        to: u64,
    ) -> Result<bool> {
        if blk >= self.sb.blocks_count() {
            return Err(Error::corrupt(format!("indirect block {blk} out of range")));
        }
        let per = self.ptrs_per_block();
        let mut data = self.cache.read(&*self.dev, blk)?;
        let mut changed = false;
        let mut left = false;
        for i in 0..per {
            let p = le32(&data, (i * 4) as usize) as u64;
            if p == 0 {
                continue;
            }
            let lo = base + i * span;
            let hi = lo + span;
            if hi <= from || lo >= to {
                left = true;
                continue;
            }
            let empty = if level == 1 {
                true
            } else {
                self.ind_trim(inode, p, level - 1, lo, span / per, from, to)?
            };
            if empty {
                self.free_blocks(p, 1)?;
                self.adjust_blocks(inode, -1);
                set_le32(&mut data, (i * 4) as usize, 0);
                changed = true;
            } else {
                left = true;
            }
        }
        if changed && left {
            self.cache.put(blk, &data);
        }
        Ok(!left)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn path_boundaries() {
        // exercised through the integration tests with real ext3 images;
        // here only the arithmetic of the level boundaries
        let p: u64 = 256; // 1K blocks
        let single_end = 12 + p;
        let double_end = single_end + p * p;
        assert_eq!(single_end, 268);
        assert_eq!(double_end, 268 + 65536);
    }
}
