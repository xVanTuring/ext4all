//! Block and inode allocation (bitmaps, group counters, uninit groups).

use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::group::{BG_BLOCK_UNINIT, BG_INODE_UNINIT, bitmap_csum};

fn test_bit(b: &[u8], i: u32) -> bool {
    b[(i / 8) as usize] & (1 << (i % 8)) != 0
}

fn set_bit(b: &mut [u8], i: u32) {
    b[(i / 8) as usize] |= 1 << (i % 8);
}

fn clear_bit(b: &mut [u8], i: u32) {
    b[(i / 8) as usize] &= !(1 << (i % 8));
}

/// First zero bit in `[from, to)`.
pub(crate) fn find_zero(b: &[u8], from: u32, to: u32) -> Option<u32> {
    let mut i = from;
    while i < to {
        if i % 64 == 0 && i + 64 <= to {
            let o = (i / 8) as usize;
            let w = u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
            if w == u64::MAX {
                i += 64;
                continue;
            }
            let z = (!w).trailing_zeros();
            return Some(i + z);
        }
        if !test_bit(b, i) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Length of the zero run starting at `from`, up to `max` and `to`.
fn zero_run(b: &[u8], from: u32, to: u32, max: u32) -> u32 {
    let mut n = 0;
    while from + n < to && n < max && !test_bit(b, from + n) {
        n += 1;
    }
    n
}

/// Mark bits `[from, to)` of a bitmap block as used (padding).
fn mark_end(b: &mut [u8], from: u32, to: u32) {
    for i in from..to {
        set_bit(b, i);
    }
}

impl Fs {
    // --- block bitmaps ----------------------------------------------------

    /// Contents of the block bitmap for `g`, computing it for BLOCK_UNINIT
    /// groups (`ext4_init_block_bitmap`).
    pub(crate) fn block_bitmap(&mut self, g: u32) -> Result<Vec<u8>> {
        let gd = &self.groups[g as usize];
        if gd.has_flag(BG_BLOCK_UNINIT) && self.has_group_csums() {
            return Ok(self.init_block_bitmap(g));
        }
        let loc = gd.block_bitmap();
        let data = self.cache.read(&*self.dev, loc)?;
        self.verify_block_bitmap(g, &data)?;
        Ok(data)
    }

    fn verify_block_bitmap(&mut self, g: u32, data: &[u8]) -> Result<()> {
        if !self.sb.has_metadata_csum() || self.cache.is_dirty(self.groups[g as usize].block_bitmap()) {
            return Ok(());
        }
        let gd = &self.groups[g as usize];
        let want = bitmap_csum(self.csum_seed, data, self.sb.clusters_per_group()) & gd.bitmap_csum_mask();
        if want != gd.block_bitmap_csum() {
            self.checksum_error(format!("block bitmap of group {g}"))?;
        }
        Ok(())
    }

    pub(crate) fn has_group_csums(&self) -> bool {
        self.sb.has_metadata_csum() || self.sb.has_gdt_csum()
    }

    fn init_block_bitmap(&self, g: u32) -> Vec<u8> {
        let bs = self.bs;
        let mut b = vec![0u8; bs as usize];
        let start = self.group_first_block(g);
        for i in 0..self.base_meta_blocks(g) {
            set_bit(&mut b, i);
        }
        let gd = &self.groups[g as usize];
        let in_group = |x: u64| x >= start && x < start + self.sb.blocks_per_group() as u64;
        for x in [gd.block_bitmap(), gd.inode_bitmap()] {
            if in_group(x) {
                set_bit(&mut b, (x - start) as u32);
            }
        }
        for x in gd.inode_table()..gd.inode_table() + self.itb_per_group as u64 {
            if in_group(x) {
                set_bit(&mut b, (x - start) as u32);
            }
        }
        mark_end(&mut b, self.blocks_in_group(g), bs * 8);
        b
    }

    /// Mutable block bitmap for `g`, initializing BLOCK_UNINIT groups.
    fn block_bitmap_mut(&mut self, g: u32) -> Result<&mut [u8]> {
        let gd = &self.groups[g as usize];
        let loc = gd.block_bitmap();
        if gd.has_flag(BG_BLOCK_UNINIT) && self.has_group_csums() {
            let init = self.init_block_bitmap(g);
            self.cache.put(loc, &init);
            self.group_mut(g).clear_flag(BG_BLOCK_UNINIT);
            self.dirty_group(g);
        } else if !self.cache.is_dirty(loc) {
            let data = self.cache.read(&*self.dev, loc)?;
            self.verify_block_bitmap(g, &data)?;
        }
        self.dirty_block_bitmap(g);
        self.cache.get_mut(&*self.dev, loc)
    }

    fn dirty_block_bitmap(&mut self, g: u32) {
        self.bitmaps_dirty.insert((g, false));
    }

    fn dirty_inode_bitmap(&mut self, g: u32) {
        self.bitmaps_dirty.insert((g, true));
    }

    /// Recompute bitmap checksums of modified bitmaps (called at commit).
    pub(crate) fn stage_bitmap_csums(&mut self) -> Result<()> {
        let dirty: Vec<(u32, bool)> = std::mem::take(&mut self.bitmaps_dirty).into_iter().collect();
        if !self.sb.has_metadata_csum() {
            return Ok(());
        }
        for (g, inode) in dirty {
            let gd = &self.groups[g as usize];
            if inode {
                let loc = gd.inode_bitmap();
                let data = self.cache.read(&*self.dev, loc)?;
                let c = bitmap_csum(self.csum_seed, &data, self.sb.inodes_per_group());
                self.groups[g as usize].set_inode_bitmap_csum(c);
            } else {
                let loc = gd.block_bitmap();
                let data = self.cache.read(&*self.dev, loc)?;
                let c = bitmap_csum(self.csum_seed, &data, self.sb.clusters_per_group());
                self.groups[g as usize].set_block_bitmap_csum(c);
            }
            self.dirty_group(g);
        }
        Ok(())
    }

    fn adjust_free_blocks(&mut self, g: u32, delta: i64) {
        let gd = self.group_mut(g);
        gd.set_free_blocks_count((gd.free_blocks_count() as i64 + delta) as u32);
        self.dirty_group(g);
        let f = self.sb.free_blocks_count() as i64 + delta;
        self.sb.set_free_blocks_count(f as u64);
        self.dirty_super();
    }

    /// Allocate a contiguous run of 1..=`want` blocks, preferring `goal`.
    pub(crate) fn alloc_blocks(&mut self, goal: u64, want: u32) -> Result<(u64, u32)> {
        self.require_rw()?;
        let want = want.max(1);
        let total = self.sb.blocks_count();
        let first = self.sb.first_data_block() as u64;
        let goal = if goal < first || goal >= total { first } else { goal };
        let ng = self.group_count();
        let g0 = self.group_of_block(goal);
        // pass 0: groups with a free run of `want` near the goal; pass 1: any
        for pass in 0..2 {
            for k in 0..ng {
                let g = (g0 + k) % ng;
                let free = self.groups[g as usize].free_blocks_count();
                if free == 0 || (pass == 0 && free < want.min(self.sb.blocks_per_group() / 4)) {
                    continue;
                }
                let start_bit = if k == 0 {
                    (goal - self.group_first_block(g)) as u32
                } else {
                    0
                };
                if let Some((bit, len)) = self.find_run_in_group(g, start_bit, want, pass == 0)? {
                    let bm = self.block_bitmap_mut(g)?;
                    for i in bit..bit + len {
                        debug_assert!(!test_bit(bm, i));
                        set_bit(bm, i);
                    }
                    self.adjust_free_blocks(g, -(len as i64));
                    let start = self.group_first_block(g) + bit as u64;
                    if self.zone.overlaps(start, len as u64) {
                        // the bitmap claims metadata is free: it is corrupt
                        return Err(Error::corrupt(format!(
                            "block bitmap of group {g} marks metadata blocks {start}+{len} free"
                        )));
                    }
                    // a freshly allocated block must not carry stale cache
                    for b in start..start + len as u64 {
                        self.cache.forget(b);
                    }
                    return Ok((start, len));
                }
            }
        }
        Err(Error::NoSpace)
    }

    /// Search group `g` for a free run. With `need_full`, only accept runs
    /// of the full wanted length (falls back to the longest seen otherwise).
    fn find_run_in_group(&mut self, g: u32, start_bit: u32, want: u32, need_full: bool) -> Result<Option<(u32, u32)>> {
        let bm = self.block_bitmap(g)?;
        let nbits = self.blocks_in_group(g);
        let mut best: Option<(u32, u32)> = None;
        for (from, to) in [(start_bit.min(nbits), nbits), (0, start_bit.min(nbits))] {
            let mut i = from;
            while let Some(z) = find_zero(&bm, i, to) {
                let run = zero_run(&bm, z, to, want);
                if run >= want {
                    return Ok(Some((z, run)));
                }
                if best.is_none_or(|b| run > b.1) {
                    best = Some((z, run));
                }
                i = z + run.max(1);
            }
        }
        if need_full { Ok(None) } else { Ok(best) }
    }

    /// Free blocks at the next commit.
    pub(crate) fn free_blocks(&mut self, start: u64, count: u64) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        let first = self.sb.first_data_block() as u64;
        if start < first || start.checked_add(count).is_none_or(|e| e > self.sb.blocks_count()) {
            return Err(Error::corrupt(format!("freeing blocks {start}+{count} out of range")));
        }
        if self.zone.overlaps(start, count) {
            return Err(Error::corrupt(format!(
                "refusing to free metadata blocks {start}+{count}"
            )));
        }
        for b in start..start + count {
            self.cache.forget(b);
        }
        self.deferred_free.push((start, count));
        Ok(())
    }

    /// Apply deferred frees to the bitmaps.
    pub(crate) fn release_deferred_frees(&mut self) -> Result<()> {
        let frees = std::mem::take(&mut self.deferred_free);
        for (start, count) in frees {
            let mut b = start;
            let end = start + count;
            while b < end {
                let g = self.group_of_block(b);
                let gstart = self.group_first_block(g);
                let gend = gstart + self.blocks_in_group(g) as u64;
                let stop = end.min(gend);
                let bm = self.block_bitmap_mut(g)?;
                let mut n = 0i64;
                for x in b..stop {
                    let bit = (x - gstart) as u32;
                    if !test_bit(bm, bit) {
                        return Err(Error::corrupt(format!("double free of block {x}")));
                    }
                    clear_bit(bm, bit);
                    n += 1;
                }
                self.adjust_free_blocks(g, n);
                b = stop;
            }
        }
        Ok(())
    }

    /// Whether a block is marked in use (for tests and consistency checks).
    pub fn block_in_use(&mut self, b: u64) -> Result<bool> {
        let g = self.group_of_block(b);
        let bm = self.block_bitmap(g)?;
        Ok(test_bit(&bm, (b - self.group_first_block(g)) as u32))
    }

    // --- inode bitmaps ----------------------------------------------------

    fn inode_bitmap(&mut self, g: u32) -> Result<Vec<u8>> {
        let gd = &self.groups[g as usize];
        if gd.has_flag(BG_INODE_UNINIT) && self.has_group_csums() {
            let mut b = vec![0u8; self.bs as usize];
            mark_end(&mut b, self.sb.inodes_per_group(), self.bs * 8);
            return Ok(b);
        }
        let loc = gd.inode_bitmap();
        let data = self.cache.read(&*self.dev, loc)?;
        if self.sb.has_metadata_csum() && !self.cache.is_dirty(loc) {
            let gd = &self.groups[g as usize];
            let want = bitmap_csum(self.csum_seed, &data, self.sb.inodes_per_group()) & gd.bitmap_csum_mask();
            if want != gd.inode_bitmap_csum() {
                self.checksum_error(format!("inode bitmap of group {g}"))?;
            }
        }
        Ok(data)
    }

    fn inode_bitmap_mut(&mut self, g: u32) -> Result<&mut [u8]> {
        let gd = &self.groups[g as usize];
        let loc = gd.inode_bitmap();
        if gd.has_flag(BG_INODE_UNINIT) && self.has_group_csums() {
            let mut b = vec![0u8; self.bs as usize];
            mark_end(&mut b, self.sb.inodes_per_group(), self.bs * 8);
            self.cache.put(loc, &b);
            self.group_mut(g).clear_flag(BG_INODE_UNINIT);
            self.dirty_group(g);
        }
        self.dirty_inode_bitmap(g);
        self.cache.get_mut(&*self.dev, loc)
    }

    pub fn inode_in_use(&mut self, ino: Ino) -> Result<bool> {
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        let bm = self.inode_bitmap(g)?;
        Ok(test_bit(&bm, (ino - 1) % ipg))
    }

    /// Pick a group for a new inode.
    fn choose_inode_group(&mut self, parent: Ino, is_dir: bool) -> u32 {
        let ng = self.group_count();
        let ipg = self.sb.inodes_per_group();
        let pg = ((parent.max(1) - 1) / ipg).min(ng - 1);
        if is_dir && ng > 1 {
            // spread directories: first group (round robin from the last one
            // used) with at least average free inodes and blocks
            let avg_i = self.sb.free_inodes_count() as u64 / ng as u64;
            let avg_b = self.sb.free_blocks_count() / ng as u64;
            for k in 0..ng {
                let g = (self.last_dir_group + 1 + k) % ng;
                let gd = &self.groups[g as usize];
                if gd.free_inodes_count() as u64 >= avg_i.max(1) && gd.free_blocks_count() as u64 >= avg_b {
                    self.last_dir_group = g;
                    return g;
                }
            }
        }
        for k in 0..ng {
            let g = (pg + k) % ng;
            if self.groups[g as usize].free_inodes_count() > 0 {
                return g;
            }
        }
        pg
    }

    /// Allocate an inode number. The caller initializes the inode.
    pub(crate) fn alloc_inode(&mut self, parent: Ino, is_dir: bool) -> Result<Ino> {
        self.require_rw()?;
        if self.sb.free_inodes_count() == 0 {
            return Err(Error::NoSpace);
        }
        let ng = self.group_count();
        let ipg = self.sb.inodes_per_group();
        let g0 = self.choose_inode_group(parent, is_dir);
        let first_ino = self.sb.first_ino();
        for k in 0..ng {
            let g = (g0 + k) % ng;
            if self.groups[g as usize].free_inodes_count() == 0 {
                continue;
            }
            let bm = self.inode_bitmap(g)?;
            let mut from = 0;
            // never hand out reserved inodes
            if g == 0 {
                from = first_ino - 1;
            }
            let Some(bit) = find_zero(&bm, from, ipg) else {
                continue;
            };
            let bm = self.inode_bitmap_mut(g)?;
            set_bit(bm, bit);
            let csums = self.has_group_csums();
            let gd = self.group_mut(g);
            gd.set_free_inodes_count(gd.free_inodes_count() - 1);
            if is_dir {
                gd.set_used_dirs_count(gd.used_dirs_count() + 1);
            }
            if csums {
                let used = ipg - gd.itable_unused();
                if bit + 1 > used {
                    gd.set_itable_unused(ipg - bit - 1);
                }
            }
            self.dirty_group(g);
            self.sb.set_free_inodes_count(self.sb.free_inodes_count() - 1);
            self.dirty_super();
            return Ok(g * ipg + bit + 1);
        }
        Err(Error::NoSpace)
    }

    pub(crate) fn free_inode(&mut self, ino: Ino, is_dir: bool) -> Result<()> {
        self.dio_inflight.remove(&ino);
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        let bit = (ino - 1) % ipg;
        let bm = self.inode_bitmap_mut(g)?;
        if !test_bit(bm, bit) {
            return Err(Error::corrupt(format!("double free of inode {ino}")));
        }
        clear_bit(bm, bit);
        let gd = self.group_mut(g);
        gd.set_free_inodes_count(gd.free_inodes_count() + 1);
        if is_dir {
            gd.set_used_dirs_count(gd.used_dirs_count().saturating_sub(1));
        }
        self.dirty_group(g);
        self.sb.set_free_inodes_count(self.sb.free_inodes_count() + 1);
        self.dirty_super();
        Ok(())
    }

    /// Commit early when space is tight and freed blocks are still pending.
    pub(crate) fn ensure_space(&mut self, want: u64) -> Result<()> {
        if !self.deferred_free.is_empty() && self.sb.free_blocks_count() < want + 64 {
            self.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_helpers() {
        let mut b = vec![0u8; 16];
        set_bit(&mut b, 0);
        set_bit(&mut b, 9);
        assert!(test_bit(&b, 0));
        assert!(test_bit(&b, 9));
        assert!(!test_bit(&b, 8));
        clear_bit(&mut b, 9);
        assert!(!test_bit(&b, 9));
        assert_eq!(find_zero(&b, 0, 128), Some(1));
        b.fill(0xFF);
        assert_eq!(find_zero(&b, 0, 128), None);
        clear_bit(&mut b, 100);
        assert_eq!(find_zero(&b, 0, 128), Some(100));
        assert_eq!(find_zero(&b, 101, 128), None);
        assert_eq!(find_zero(&b, 3, 100), None);
    }

    #[test]
    fn find_zero_word_path() {
        let mut b = vec![0xFFu8; 64];
        clear_bit(&mut b, 64 * 5 + 17);
        assert_eq!(find_zero(&b, 0, 512), Some(64 * 5 + 17));
        assert_eq!(find_zero(&b, 7, 512), Some(64 * 5 + 17));
    }

    #[test]
    fn zero_runs() {
        let mut b = vec![0u8; 8];
        set_bit(&mut b, 10);
        assert_eq!(zero_run(&b, 0, 64, 100), 10);
        assert_eq!(zero_run(&b, 0, 64, 4), 4);
        assert_eq!(zero_run(&b, 11, 64, 100), 53);
        assert_eq!(zero_run(&b, 10, 64, 100), 0);
        assert_eq!(zero_run(&b, 60, 62, 100), 2);
    }

    #[test]
    fn mark_end_sets_padding() {
        let mut b = vec![0u8; 4];
        mark_end(&mut b, 20, 32);
        assert_eq!(find_zero(&b, 0, 32), Some(0));
        assert_eq!(find_zero(&b, 20, 32), None);
        assert!(!test_bit(&b, 19));
    }
}
