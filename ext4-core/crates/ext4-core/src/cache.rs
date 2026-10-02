//! Metadata block cache.
//!
//! All metadata (bitmaps, inode tables, directory blocks, extent nodes,
//! xattr blocks, descriptor blocks) is read and modified through this cache.
//! Modified blocks stay pinned as dirty until the next commit writes them
//! (through the journal when there is one). Clean blocks are evicted in
//! least-recently-used order once the cache exceeds its capacity.
//!
//! With a journal, committed blocks are written to their home location
//! only at the next checkpoint. Until then a copy of their committed
//! contents is kept here ("logged"): a checkpoint writes exactly that
//! (never later, uncommitted changes), and a block that left the cache is
//! read back from it rather than from its stale home location.
//!
//! Regular file data never enters the cache.

use crate::device::BlockDevice;
use crate::error::Result;
use std::collections::{BTreeMap, HashMap};

struct Entry {
    data: Box<[u8]>,
    dirty: bool,
    stamp: u64,
}

/// Saved state of a block: contents and dirtiness, or `None` if uncached.
type SavedBlock = Option<(Box<[u8]>, bool)>;

pub struct BlockCache {
    block_size: usize,
    capacity: usize,
    entries: HashMap<u64, Entry>,
    /// stamp → block for clean entries, oldest first.
    lru: BTreeMap<u64, u64>,
    clock: u64,
    dirty_count: usize,
    /// While an operation runs: the state of every block before its first
    /// modification (None = was not cached), so a failed operation can be
    /// undone.
    undo: Option<HashMap<u64, SavedBlock>>,
    /// Committed contents of blocks whose home location is not written yet
    /// (checkpoint pending).
    logged: HashMap<u64, Box<[u8]>>,
    pub hits: u64,
    pub misses: u64,
}

impl BlockCache {
    pub fn new(block_size: usize, capacity: usize) -> Self {
        BlockCache {
            block_size,
            capacity: capacity.max(16),
            entries: HashMap::new(),
            lru: BTreeMap::new(),
            clock: 0,
            dirty_count: 0,
            undo: None,
            logged: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// After the dirty blocks were committed to the journal: remember their
    /// committed contents for the checkpoint and mark them clean.
    pub fn log_dirty(&mut self) {
        for (&b, e) in &self.entries {
            if e.dirty {
                self.logged.insert(b, e.data.clone());
            }
        }
        self.mark_all_clean();
    }

    /// Number of blocks waiting for a checkpoint.
    pub fn logged_len(&self) -> usize {
        self.logged.len()
    }

    /// Whether any block of `[start, start+count)` waits for a checkpoint.
    pub fn logged_intersects(&self, start: u64, count: u64) -> bool {
        if (count as usize) < self.logged.len() {
            (start..start + count).any(|b| self.logged.contains_key(&b))
        } else {
            self.logged.keys().any(|&b| b >= start && b - start < count)
        }
    }

    /// Blocks waiting for a checkpoint with their committed contents, in
    /// block order.
    pub fn logged_sorted(&self) -> Vec<(u64, &[u8])> {
        let mut v: Vec<(u64, &[u8])> = self.logged.iter().map(|(&b, d)| (b, &d[..])).collect();
        v.sort_unstable_by_key(|e| e.0);
        v
    }

    /// The checkpoint wrote every logged block home.
    pub fn clear_logged(&mut self) {
        self.logged.clear();
    }

    /// Keep a logged copy in step with a direct write of part of the block
    /// (the superblock), so a later checkpoint does not undo it.
    pub fn patch_logged(&mut self, blk: u64, off: usize, bytes: &[u8]) {
        if let Some(d) = self.logged.get_mut(&blk) {
            d[off..off + bytes.len()].copy_from_slice(bytes);
        }
    }

    /// Start recording block states for [`BlockCache::rollback`].
    pub fn begin_undo(&mut self) {
        self.undo = Some(HashMap::new());
    }

    /// Stop recording (the changes are kept).
    pub fn end_undo(&mut self) {
        self.undo = None;
    }

    pub fn undo_active(&self) -> bool {
        self.undo.is_some()
    }

    /// Restore every block modified since [`BlockCache::begin_undo`].
    pub fn rollback(&mut self) {
        let Some(undo) = self.undo.take() else {
            return;
        };
        for (blk, prev) in undo {
            match prev {
                Some((data, dirty)) => self.insert(blk, data, dirty),
                None => self.remove_entry(blk),
            }
        }
    }

    fn record(&mut self, blk: u64) {
        if let Some(u) = &mut self.undo
            && !u.contains_key(&blk)
        {
            let prev = self.entries.get(&blk).map(|e| (e.data.clone(), e.dirty));
            u.insert(blk, prev);
        }
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty_count
    }

    pub fn contains(&self, blk: u64) -> bool {
        self.entries.contains_key(&blk)
    }

    pub fn is_dirty(&self, blk: u64) -> bool {
        self.entries.get(&blk).is_some_and(|e| e.dirty)
    }

    fn touch(&mut self, blk: u64) {
        self.clock += 1;
        let stamp = self.clock;
        let e = self.entries.get_mut(&blk).expect("touch of missing entry");
        if !e.dirty {
            self.lru.remove(&e.stamp);
            self.lru.insert(stamp, blk);
        }
        e.stamp = stamp;
    }

    fn load(&mut self, dev: &dyn BlockDevice, blk: u64) -> Result<()> {
        if self.entries.contains_key(&blk) {
            self.hits += 1;
            self.touch(blk);
            return Ok(());
        }
        self.misses += 1;
        if let Some(d) = self.logged.get(&blk) {
            // committed but not checkpointed: the home location is stale
            let data = d.clone();
            self.insert(blk, data, false);
            return Ok(());
        }
        let mut data = vec![0u8; self.block_size].into_boxed_slice();
        dev.read_at(blk * self.block_size as u64, &mut data)?;
        self.insert(blk, data, false);
        Ok(())
    }

    fn insert(&mut self, blk: u64, data: Box<[u8]>, dirty: bool) {
        self.clock += 1;
        let stamp = self.clock;
        if let Some(old) = self.entries.insert(blk, Entry { data, dirty, stamp }) {
            if old.dirty {
                self.dirty_count -= 1;
            } else {
                self.lru.remove(&old.stamp);
            }
        }
        if dirty {
            self.dirty_count += 1;
        } else {
            self.lru.insert(stamp, blk);
        }
        self.evict(Some(blk));
    }

    /// Evict clean blocks down to capacity, never evicting `keep`.
    fn evict(&mut self, keep: Option<u64>) {
        while self.entries.len() > self.capacity {
            let victim = self.lru.iter().find(|&(_, &b)| Some(b) != keep).map(|(&s, &b)| (s, b));
            let Some((stamp, blk)) = victim else {
                break; // everything left is dirty (or the kept block)
            };
            self.lru.remove(&stamp);
            self.entries.remove(&blk);
        }
    }

    /// Read access to a block, loading it from the device on a miss.
    pub fn get(&mut self, dev: &dyn BlockDevice, blk: u64) -> Result<&[u8]> {
        self.load(dev, blk)?;
        Ok(&self.entries[&blk].data)
    }

    /// Copy of a block's contents.
    pub fn read(&mut self, dev: &dyn BlockDevice, blk: u64) -> Result<Vec<u8>> {
        Ok(self.get(dev, blk)?.to_vec())
    }

    /// Write access to a block (marks it dirty), loading it on a miss.
    pub fn get_mut(&mut self, dev: &dyn BlockDevice, blk: u64) -> Result<&mut [u8]> {
        self.record(blk);
        self.load(dev, blk)?;
        self.mark_dirty(blk);
        Ok(&mut self.entries.get_mut(&blk).unwrap().data)
    }

    /// Replace a block's contents entirely without reading it first.
    pub fn put(&mut self, blk: u64, data: &[u8]) {
        debug_assert_eq!(data.len(), self.block_size);
        self.record(blk);
        self.insert(blk, data.to_vec().into_boxed_slice(), true);
    }

    /// A zero-filled dirty block (for freshly allocated metadata).
    pub fn zeroed(&mut self, blk: u64) -> &mut [u8] {
        self.record(blk);
        self.insert(blk, vec![0u8; self.block_size].into_boxed_slice(), true);
        &mut self.entries.get_mut(&blk).unwrap().data
    }

    fn mark_dirty(&mut self, blk: u64) {
        let e = self.entries.get_mut(&blk).unwrap();
        if !e.dirty {
            e.dirty = true;
            self.lru.remove(&e.stamp);
            self.dirty_count += 1;
        }
    }

    /// Drop a block (e.g. it was freed). Dirty contents are discarded.
    pub fn forget(&mut self, blk: u64) {
        if self.entries.contains_key(&blk) {
            self.record(blk);
        }
        self.remove_entry(blk);
    }

    fn remove_entry(&mut self, blk: u64) {
        if let Some(e) = self.entries.remove(&blk) {
            if e.dirty {
                self.dirty_count -= 1;
            } else {
                self.lru.remove(&e.stamp);
            }
        }
    }

    /// Drop every clean block (used after journal replay rewrote the disk).
    pub fn drop_clean(&mut self) {
        let clean: Vec<u64> = self.lru.values().copied().collect();
        for b in clean {
            self.entries.remove(&b);
        }
        self.lru.clear();
    }

    /// Sorted list of dirty block numbers.
    pub fn dirty_blocks(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.entries.iter().filter(|(_, e)| e.dirty).map(|(&b, _)| b).collect();
        v.sort_unstable();
        v
    }

    /// Contents of a cached block, if present.
    pub fn peek(&self, blk: u64) -> Option<&[u8]> {
        self.entries.get(&blk).map(|e| &e.data[..])
    }

    /// Mark all dirty blocks clean (after they reached the disk).
    pub fn mark_all_clean(&mut self) {
        let mut newly_clean = Vec::new();
        for (&b, e) in self.entries.iter_mut() {
            if e.dirty {
                e.dirty = false;
                newly_clean.push((e.stamp, b));
            }
        }
        for (s, b) in newly_clean {
            self.lru.insert(s, b);
        }
        self.dirty_count = 0;
        self.evict(None);
    }

    /// Write all dirty blocks straight to the device (no journal) and mark
    /// them clean.
    pub fn write_back(&mut self, dev: &dyn BlockDevice) -> Result<()> {
        for b in self.dirty_blocks() {
            dev.write_at(b * self.block_size as u64, &self.entries[&b].data)?;
        }
        self.mark_all_clean();
        Ok(())
    }

    /// Discard all dirty blocks (used when a transaction must be aborted).
    pub fn discard_dirty(&mut self) {
        let dirty: Vec<u64> = self.dirty_blocks();
        for b in dirty {
            self.entries.remove(&b);
        }
        self.dirty_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemDevice;

    fn dev() -> MemDevice {
        let mut v = vec![0u8; 64 * 1024];
        for (i, chunk) in v.chunks_mut(1024).enumerate() {
            chunk.fill(i as u8);
        }
        MemDevice::from_vec(v)
    }

    #[test]
    fn rollback_restores_every_touched_block() {
        let d = dev();
        let mut c = BlockCache::new(1024, 64);
        c.get_mut(&d, 1).unwrap()[0] = 0xA1; // dirty before the op
        c.get(&d, 2).unwrap(); // clean before the op
        c.begin_undo();
        c.get_mut(&d, 1).unwrap()[0] = 0xB1;
        c.get_mut(&d, 2).unwrap()[0] = 0xB2;
        c.get_mut(&d, 3).unwrap()[0] = 0xB3; // not cached before
        c.put(4, &[0xB4; 1024]);
        c.zeroed(5);
        c.forget(1);
        c.forget(2);
        assert!(c.undo_active());
        c.rollback();
        assert!(!c.undo_active());
        assert_eq!(c.peek(1).unwrap()[0], 0xA1);
        assert!(c.is_dirty(1));
        assert_eq!(c.peek(2).unwrap()[0], 2);
        assert!(!c.is_dirty(2));
        assert!(!c.contains(3));
        assert!(!c.contains(4));
        assert!(!c.contains(5));
        assert_eq!(c.dirty_count(), 1);
        assert_eq!(c.dirty_blocks(), vec![1]);
    }

    #[test]
    fn end_undo_keeps_changes() {
        let d = dev();
        let mut c = BlockCache::new(1024, 64);
        c.begin_undo();
        c.get_mut(&d, 7).unwrap()[0] = 0xCC;
        c.end_undo();
        c.rollback(); // no-op without an active undo log
        assert_eq!(c.peek(7).unwrap()[0], 0xCC);
        assert!(c.is_dirty(7));
    }

    #[test]
    fn read_through() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        assert_eq!(c.get(&d, 3).unwrap()[0], 3);
        assert_eq!(c.get(&d, 3).unwrap()[1023], 3);
        assert_eq!(c.misses, 1);
        assert_eq!(c.hits, 1);
        assert_eq!(c.read(&d, 5).unwrap()[10], 5);
        assert_eq!(c.len(), 2);
        assert!(!c.is_empty());
    }

    #[test]
    fn dirty_tracking_and_writeback() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        c.get_mut(&d, 2).unwrap()[0] = 0xAA;
        c.zeroed(7)[5] = 0xBB;
        c.put(9, &[0xCC; 1024]);
        assert_eq!(c.dirty_count(), 3);
        assert_eq!(c.dirty_blocks(), vec![2, 7, 9]);
        assert!(c.is_dirty(2));
        // device untouched until write-back
        let mut b = [0u8; 1];
        d.read_at(2 * 1024, &mut b).unwrap();
        assert_eq!(b[0], 2);
        c.write_back(&d).unwrap();
        assert_eq!(c.dirty_count(), 0);
        d.read_at(2 * 1024, &mut b).unwrap();
        assert_eq!(b[0], 0xAA);
        d.read_at(7 * 1024 + 5, &mut b).unwrap();
        assert_eq!(b[0], 0xBB);
        d.read_at(9 * 1024, &mut b).unwrap();
        assert_eq!(b[0], 0xCC);
        d.read_at(7 * 1024, &mut b).unwrap();
        assert_eq!(b[0], 0);
    }

    #[test]
    fn eviction_spares_dirty_blocks() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        for b in 0..16 {
            c.get_mut(&d, b).unwrap()[0] = 0xEE;
        }
        for b in 16..40 {
            c.get(&d, b).unwrap();
        }
        // all dirty blocks survive, clean ones are evicted down to capacity
        assert_eq!(c.dirty_count(), 16);
        for b in 0..16 {
            assert!(c.contains(b));
            assert_eq!(c.peek(b).unwrap()[0], 0xEE);
        }
        assert!(c.len() <= 17);
    }

    #[test]
    fn lru_order() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        for b in 0..16 {
            c.get(&d, b).unwrap();
        }
        c.get(&d, 0).unwrap(); // refresh 0
        c.get(&d, 20).unwrap(); // evicts 1 (oldest)
        assert!(c.contains(0));
        assert!(!c.contains(1));
        assert!(c.contains(20));
    }

    #[test]
    fn forget_and_discard() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        c.get_mut(&d, 1).unwrap()[0] = 9;
        c.get(&d, 2).unwrap();
        c.forget(1);
        c.forget(2);
        c.forget(99);
        assert_eq!(c.dirty_count(), 0);
        assert!(c.is_empty());
        c.get_mut(&d, 3).unwrap()[0] = 9;
        c.get(&d, 4).unwrap();
        c.discard_dirty();
        assert!(!c.contains(3));
        assert!(c.contains(4));
        assert_eq!(c.get(&d, 3).unwrap()[0], 3);
    }

    #[test]
    fn mark_all_clean_makes_evictable() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        for b in 0..30 {
            c.get_mut(&d, b).unwrap();
        }
        assert_eq!(c.len(), 30);
        c.mark_all_clean();
        assert_eq!(c.len(), 16);
        assert_eq!(c.dirty_count(), 0);
    }

    #[test]
    fn drop_clean_keeps_dirty() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        c.get(&d, 1).unwrap();
        c.get_mut(&d, 2).unwrap();
        c.drop_clean();
        assert!(!c.contains(1));
        assert!(c.contains(2));
    }

    #[test]
    fn rewrite_dirty_entry_counts_once() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        c.get_mut(&d, 1).unwrap();
        c.get_mut(&d, 1).unwrap();
        c.put(1, &[0u8; 1024]);
        c.zeroed(1);
        assert_eq!(c.dirty_count(), 1);
    }

    #[test]
    fn logged_copies_survive_eviction_and_later_changes() {
        let d = dev();
        let mut c = BlockCache::new(1024, 16);
        c.get_mut(&d, 3).unwrap().fill(0xAA);
        c.get_mut(&d, 4).unwrap().fill(0xBB);
        c.log_dirty();
        assert_eq!(c.dirty_count(), 0);
        assert_eq!(c.logged_len(), 2);
        // a newer, uncommitted change does not alter what the checkpoint
        // writes
        c.get_mut(&d, 3).unwrap().fill(0xCC);
        let logged = c.logged_sorted();
        assert_eq!(logged.iter().map(|e| e.0).collect::<Vec<_>>(), vec![3, 4]);
        assert!(logged[0].1.iter().all(|&b| b == 0xAA));
        assert!(logged[1].1.iter().all(|&b| b == 0xBB));
        // evict block 4 (clean): a later read must come from the logged
        // copy, not from the stale device contents
        for b in 20..60 {
            c.get(&d, b).unwrap();
        }
        assert!(!c.contains(4));
        assert!(c.get(&d, 4).unwrap().iter().all(|&b| b == 0xBB));
        assert!(c.logged_intersects(4, 1));
        assert!(c.logged_intersects(0, 100));
        assert!(!c.logged_intersects(5, 10));
        c.patch_logged(4, 10, &[1, 2, 3]);
        assert_eq!(&c.logged_sorted()[1].1[9..14], &[0xBB, 1, 2, 3, 0xBB]);
        c.clear_logged();
        assert_eq!(c.logged_len(), 0);
        // after the checkpoint, eviction falls back to the device again
        c.log_dirty(); // block 3 (0xCC) committed
        assert_eq!(c.logged_len(), 1);
    }
}
