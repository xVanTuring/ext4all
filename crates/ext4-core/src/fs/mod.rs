//! A mounted ext4 file system.
//!
//! [`Fs`] is single-threaded by design (callers wrap it in a mutex). Every
//! modifying operation updates metadata in the block cache; [`Fs::commit`]
//! writes the accumulated changes atomically through the journal.

mod alloc;
mod bmap;
mod dir;
mod extent;
mod file;
mod htree;
mod indirect;
mod inode;
mod ops;
mod orphan;
pub mod overlay;
pub mod types;
mod xattr;
mod zone;

#[cfg(test)]
mod tests;

pub use types::*;

use crate::cache::BlockCache;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::features::{self, Support};
use crate::journal::{Journal, JournalMap};
use crate::ondisk::group::GroupDesc;
use crate::ondisk::inode::{JOURNAL_INO, ROOT_INO, Timestamp};
use crate::ondisk::superblock::{
    STATE_ERROR, STATE_VALID, SUPERBLOCK_OFFSET, SUPERBLOCK_SIZE, Superblock, compat, incompat,
};
use std::collections::{BTreeSet, HashMap};

/// Most blocks kept in memory for the next checkpoint (16 MiB of 4K
/// blocks) before one is forced.
const MAX_LOGGED: usize = 4096;

/// In-memory state captured when an operation starts (block contents are
/// tracked by the cache's undo log).
pub(crate) struct Savepoint {
    sb: Box<[u8; SUPERBLOCK_SIZE]>,
    sb_dirty: bool,
    /// Original descriptors of groups modified during the operation.
    groups: HashMap<u32, GroupDesc>,
    dirty_groups: BTreeSet<u32>,
    bitmaps_dirty: BTreeSet<(u32, bool)>,
    deferred_len: usize,
    open_orphans: BTreeSet<u32>,
    last_dir_group: u32,
}
use std::sync::Arc;

pub struct Fs {
    pub(crate) dev: Arc<dyn BlockDevice>,
    pub(crate) sb: Superblock,
    pub(crate) sb_dirty: bool,
    pub(crate) groups: Vec<GroupDesc>,
    pub(crate) dirty_groups: BTreeSet<u32>,
    /// Bitmaps modified since the last commit: (group, is_inode_bitmap).
    pub(crate) bitmaps_dirty: BTreeSet<(u32, bool)>,
    /// Inodes unlinked while still referenced (on the orphan list).
    pub(crate) open_orphans: BTreeSet<u32>,
    pub(crate) defer_unlinked: bool,
    /// State to restore if the running operation fails.
    pub(crate) savepoint: Option<Savepoint>,
    pub(crate) op_depth: u32,
    /// Set after a failed commit: the volume is switched to read-only and
    /// the journal is left for recovery at the next mount.
    pub(crate) aborted: bool,
    /// Metadata blocks that file mappings may never touch.
    pub(crate) zone: zone::SystemZone,
    /// Byte ranges mapped for kernel (direct) writes whose completion has
    /// not been reported yet, per inode. Their blocks were zeroed or are
    /// being written whole, so later mappings must not zero them again.
    pub(crate) dio_inflight: std::collections::BTreeMap<Ino, Vec<(u64, u64)>>,
    pub(crate) cache: BlockCache,
    pub(crate) bs: u32,
    pub(crate) csum_seed: u32,
    pub(crate) read_only: bool,
    pub(crate) opts: MountOptions,
    pub(crate) journal: Option<Journal>,
    /// Blocks freed in the running transaction; released at commit so they
    /// cannot be reused before the change that freed them is durable.
    pub(crate) deferred_free: Vec<(u64, u64)>,
    pub(crate) gdt_blocks: u32,
    pub(crate) desc_per_block: u32,
    pub(crate) itb_per_group: u32,
    /// Allocation goal hint for new directories (spreads them out).
    pub(crate) last_dir_group: u32,
    pub(crate) generation_seed: u32,
    /// Whether we changed on-disk mount state (RECOVER set / VALID cleared)
    /// and must restore it on unmount.
    pub(crate) mounted_rw: bool,
    pub(crate) report: MountReport,
    /// Number of metadata checksum errors seen (non-strict mode).
    pub checksum_errors: u64,
}

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs")
            .field("sb", &self.sb)
            .field("read_only", &self.read_only)
            .finish()
    }
}

impl Fs {
    /// Read the superblock without mounting (for probing).
    pub fn probe(dev: &dyn BlockDevice) -> Result<Superblock> {
        let mut raw = vec![0u8; SUPERBLOCK_SIZE];
        dev.read_at(SUPERBLOCK_OFFSET, &mut raw)?;
        Superblock::parse(&raw)
    }

    pub fn mount(dev: Arc<dyn BlockDevice>, opts: MountOptions) -> Result<Fs> {
        let sb = Self::probe(&*dev)?;
        let mut read_only = opts.read_only || dev.is_read_only();
        let mut report = MountReport::default();
        match features::check(&sb) {
            Support::ReadWrite => {}
            Support::ReadOnly(reasons) => {
                if !read_only {
                    log::warn!("mounting read-only: unsupported features {reasons:?}");
                }
                report.read_only_reasons = reasons.iter().map(|s| s.to_string()).collect();
                read_only = true;
            }
            Support::Unsupported(f) => {
                return Err(Error::unsupported(format!("features: {}", f.join(", "))));
            }
        }
        if !read_only && sb.state() & STATE_ERROR != 0 {
            log::warn!("file system has errors; mounting read-only");
            report.read_only_reasons.push("file system marked with errors".into());
            read_only = true;
        }
        let mut fs = Self::open(dev.clone(), sb, &opts, read_only)?;
        fs.report = report;

        if fs.sb.has_compat(compat::HAS_JOURNAL) {
            fs.load_journal()?;
        }
        fs.build_system_zone()?;
        if !fs.read_only {
            // needs_recovery must be durable before the first transaction
            fs.mark_mounted()?;
            fs.process_orphans()?;
        }
        Ok(fs)
    }

    /// Set up geometry and read group descriptors.
    fn open(dev: Arc<dyn BlockDevice>, sb: Superblock, opts: &MountOptions, read_only: bool) -> Result<Fs> {
        let bs = sb.block_size();
        let groups = sb.group_count();
        let desc_size = sb.desc_size();
        let desc_per_block = bs / desc_size;
        let gdt_blocks = groups.div_ceil(desc_per_block);
        let itb_per_group = (sb.inodes_per_group() as u64 * sb.inode_size() as u64).div_ceil(bs as u64) as u32;
        if sb.blocks_count().checked_mul(bs as u64).is_none_or(|n| n > dev.size()) {
            return Err(Error::corrupt(format!(
                "file system ({} blocks) larger than device ({} bytes)",
                sb.blocks_count(),
                dev.size()
            )));
        }
        let mut fs = Fs {
            csum_seed: sb.csum_seed(),
            dev,
            sb,
            sb_dirty: false,
            groups: Vec::with_capacity(groups as usize),
            dirty_groups: BTreeSet::new(),
            bitmaps_dirty: BTreeSet::new(),
            open_orphans: BTreeSet::new(),
            defer_unlinked: false,
            savepoint: None,
            op_depth: 0,
            aborted: false,
            zone: zone::SystemZone::default(),
            dio_inflight: Default::default(),
            cache: BlockCache::new(bs as usize, opts.cache_blocks),
            bs,
            read_only,
            opts: opts.clone(),
            journal: None,
            deferred_free: Vec::new(),
            gdt_blocks,
            desc_per_block,
            itb_per_group,
            last_dir_group: 0,
            generation_seed: rand_seed(),
            mounted_rw: false,
            report: MountReport::default(),
            checksum_errors: 0,
        };
        fs.load_group_descs()?;
        fs.recount_free()?;
        Ok(fs)
    }

    /// The superblock's free counters are only hints (Linux recomputes
    /// them from the group descriptors at mount); do the same.
    fn recount_free(&mut self) -> Result<()> {
        let mut blocks = 0u64;
        let mut inodes = 0u64;
        // descriptors count clusters, the superblock counts blocks
        let ratio = self.sb.cluster_ratio() as u64;
        for (g, gd) in self.groups.iter().enumerate() {
            let fb = gd.free_blocks_count() as u64 * ratio;
            let fi = gd.free_inodes_count() as u64;
            if fb > self.blocks_in_group(g as u32) as u64 || fi > self.sb.inodes_per_group() as u64 {
                return Err(Error::corrupt(format!("group {g}: free counts exceed group size")));
            }
            blocks += fb;
            inodes += fi;
        }
        if blocks != self.sb.free_blocks_count() || inodes != self.sb.free_inodes_count() as u64 {
            log::info!(
                "superblock free counts ({}, {}) differ from groups ({blocks}, {inodes}); using the groups",
                self.sb.free_blocks_count(),
                self.sb.free_inodes_count()
            );
            self.sb.set_free_blocks_count(blocks);
            self.sb.set_free_inodes_count(inodes as u32);
            self.dirty_super();
        }
        Ok(())
    }

    fn load_group_descs(&mut self) -> Result<()> {
        self.groups.clear();
        let groups = self.sb.group_count();
        let ds = self.sb.desc_size() as usize;
        for i in 0..self.gdt_blocks {
            let loc = self.desc_block_location(i);
            let blk = self.cache.read(&*self.dev, loc)?;
            for j in 0..self.desc_per_block {
                let g = i * self.desc_per_block + j;
                if g >= groups {
                    break;
                }
                let off = j as usize * ds;
                let gd = GroupDesc::new(&blk[off..off + ds]);
                self.groups.push(gd);
            }
        }
        for g in 0..groups {
            if let Some(want) = self.group_desc_csum(g)
                && want != self.groups[g as usize].checksum()
            {
                self.checksum_error(format!(
                    "group descriptor {g}: stored {:#06x} computed {want:#06x}",
                    self.groups[g as usize].checksum()
                ))?;
            }
            self.check_group_desc(g)?;
        }
        Ok(())
    }

    fn check_group_desc(&self, g: u32) -> Result<()> {
        let gd = &self.groups[g as usize];
        let total = self.sb.blocks_count();
        let first = self.sb.first_data_block() as u64;
        for (what, b) in [
            ("block bitmap", gd.block_bitmap()),
            ("inode bitmap", gd.inode_bitmap()),
            ("inode table", gd.inode_table()),
        ] {
            if b < first || b >= total {
                return Err(Error::corrupt(format!("group {g}: {what} at {b} out of range")));
            }
        }
        if gd.inode_table() + self.itb_per_group as u64 > total {
            return Err(Error::corrupt(format!("group {g}: inode table overruns device")));
        }
        Ok(())
    }

    /// Expected descriptor checksum, if the file system uses one.
    pub(crate) fn group_desc_csum(&self, g: u32) -> Option<u16> {
        let gd = &self.groups[g as usize];
        if self.sb.has_metadata_csum() {
            Some(gd.csum_metadata(self.csum_seed, g))
        } else if self.sb.has_gdt_csum() {
            Some(gd.csum_gdt(&self.sb.uuid(), g))
        } else {
            None
        }
    }

    /// Report a checksum failure: error in strict mode, counted otherwise.
    pub(crate) fn checksum_error(&mut self, msg: String) -> Result<()> {
        self.checksum_errors += 1;
        if self.opts.strict_checksums {
            Err(Error::Checksum(msg))
        } else {
            log::warn!("checksum mismatch ignored: {msg}");
            Ok(())
        }
    }

    // --- geometry -------------------------------------------------------

    pub fn block_size(&self) -> u32 {
        self.bs
    }

    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn mount_report(&self) -> &MountReport {
        &self.report
    }

    pub fn root(&self) -> Ino {
        ROOT_INO
    }

    pub(crate) fn group_count(&self) -> u32 {
        self.groups.len() as u32
    }

    pub(crate) fn group_first_block(&self, g: u32) -> u64 {
        self.sb.first_data_block() as u64 + g as u64 * self.sb.blocks_per_group() as u64
    }

    pub(crate) fn blocks_in_group(&self, g: u32) -> u32 {
        if g + 1 == self.group_count() {
            (self.sb.blocks_count() - self.group_first_block(g)) as u32
        } else {
            self.sb.blocks_per_group()
        }
    }

    pub(crate) fn group_of_block(&self, b: u64) -> u32 {
        ((b - self.sb.first_data_block() as u64) / self.sb.blocks_per_group() as u64) as u32
    }

    /// Location of group descriptor block `i` (handles meta_bg).
    pub(crate) fn desc_block_location(&self, i: u32) -> u64 {
        // block holding the superblock (1 for 1K blocks even with bigalloc)
        let sb_block = SUPERBLOCK_OFFSET / self.bs as u64;
        if !self.sb.has_incompat(incompat::META_BG) || i < self.sb.first_meta_bg() {
            return sb_block + 1 + i as u64;
        }
        let g = i * self.desc_per_block;
        let mut has_super = self.sb.group_has_super(g) as u64;
        if g == 0 && self.sb.first_data_block() == 0 && self.bs == 1024 {
            has_super += 1;
        }
        self.group_first_block(g) + has_super
    }

    /// Number of blocks at the start of group `g` used by the superblock
    /// backup and descriptor tables (`ext4_num_base_meta_clusters`).
    pub(crate) fn base_meta_blocks(&self, g: u32) -> u32 {
        let has_super = self.sb.group_has_super(g) as u32;
        let meta_bg = self.sb.has_incompat(incompat::META_BG);
        if !meta_bg || g < self.sb.first_meta_bg() * self.desc_per_block {
            if has_super == 0 {
                return 0;
            }
            let gdb = if meta_bg {
                self.sb.first_meta_bg()
            } else {
                self.gdt_blocks
            };
            has_super + gdb + self.sb.reserved_gdt_blocks() as u32
        } else {
            let first = g / self.desc_per_block * self.desc_per_block;
            let last = first + self.desc_per_block - 1;
            let gdb = (g == first || g == first + 1 || g == last) as u32;
            has_super + gdb
        }
    }

    // --- journal --------------------------------------------------------

    fn load_journal(&mut self) -> Result<()> {
        let ji = self.sb.journal_inum();
        if ji == 0 {
            return Err(Error::unsupported("external journal"));
        }
        let inode = self.read_inode(ji)?;
        let runs = self.all_extents(ji, &inode)?;
        let map = JournalMap {
            runs: runs
                .iter()
                .filter(|e| !e.unwritten)
                .map(|e| (e.block, e.start, e.len))
                .collect(),
        };
        let mut journal = Journal::load(&*self.dev, map, self.bs as usize)?;
        let needs = journal.needs_recovery() || self.sb.has_incompat(incompat::RECOVER);
        if journal.unsupported_incompat() != 0 {
            if journal.needs_recovery() {
                return Err(Error::unsupported(format!(
                    "journal needs recovery with unsupported features {:#x}",
                    journal.unsupported_incompat()
                )));
            }
            if !self.read_only {
                self.report
                    .read_only_reasons
                    .push("unsupported journal features".into());
                self.read_only = true;
            }
        }
        if needs && journal.needs_recovery() {
            if self.read_only {
                let plan = journal.plan_recovery(&*self.dev)?;
                self.report.journal_replayed = true;
                self.report.replayed_transactions = plan.transactions;
                self.report.replayed_blocks = plan.blocks.len();
                let overlay = overlay::OverlayDevice::new(self.dev.clone(), self.bs, plan.blocks);
                self.dev = Arc::new(overlay);
            } else {
                let plan = journal.recover(&*self.dev)?;
                self.report.journal_replayed = true;
                self.report.replayed_transactions = plan.transactions;
                self.report.replayed_blocks = plan.blocks.len();
            }
            self.reload_after_replay()?;
        } else if needs && !self.read_only {
            // RECOVER flag set but journal empty: nothing to replay
            self.report.journal_replayed = false;
        }
        if !self.read_only && self.sb.has_incompat(incompat::RECOVER) {
            // replayed: clear the flag in memory; mark_mounted sets it again
            let f = self.sb.feature_incompat() & !incompat::RECOVER;
            self.sb.set_feature_incompat(f);
        }
        if !self.read_only {
            // What the kernel does on a read-write mount: 64-bit block
            // numbers in tags for 64bit file systems (blocks >= 2^32 would
            // otherwise be truncated) and checksum v3 with metadata_csum.
            journal.enable_features(&*self.dev, self.sb.is_64bit(), self.sb.has_metadata_csum())?;
        }
        self.journal = Some(journal);
        Ok(())
    }

    fn reload_after_replay(&mut self) -> Result<()> {
        self.cache = BlockCache::new(self.bs as usize, self.opts.cache_blocks);
        let sb = Self::probe(&*self.dev)?;
        self.csum_seed = sb.csum_seed();
        self.sb = sb;
        self.load_group_descs()?;
        self.recount_free()
    }

    /// Record on disk that the file system is mounted read-write.
    fn mark_mounted(&mut self) -> Result<()> {
        let now = Timestamp::now();
        if self.journal.is_some() {
            let f = self.sb.feature_incompat() | incompat::RECOVER;
            self.sb.set_feature_incompat(f);
        } else {
            let s = self.sb.state() & !STATE_VALID;
            self.sb.set_state(s);
        }
        self.sb.set_mnt_count(self.sb.mnt_count().wrapping_add(1));
        self.sb.set_mtime(now.sec as u32);
        self.sb.set_mtime_hi((now.sec >> 32) as u8);
        self.write_super_direct()?;
        self.mounted_rw = true;
        Ok(())
    }

    /// Write the in-memory superblock straight to disk (bypassing the
    /// journal) and flush. Used for mount-state flags.
    fn write_super_direct(&mut self) -> Result<()> {
        let now = Timestamp::now();
        self.sb.set_wtime(now.sec as u32);
        self.sb.set_wtime_hi((now.sec >> 32) as u8);
        self.sb.update_checksum();
        self.dev.write_at(SUPERBLOCK_OFFSET, &self.sb.raw[..])?;
        self.dev.flush()?;
        // keep a cached copy of the superblock block coherent
        let (blk, off) = self.super_location();
        let raw = self.sb.raw.clone();
        self.cache.patch_logged(blk, off, &raw[..]);
        if self.cache.contains(blk) {
            let raw = self.sb.raw.clone();
            let b = self.cache.get_mut(&*self.dev, blk)?;
            b[off..off + SUPERBLOCK_SIZE].copy_from_slice(&raw[..]);
            // the block content now equals disk; leaving it dirty is harmless
        }
        Ok(())
    }

    fn super_location(&self) -> (u64, usize) {
        if self.bs == 1024 {
            (1, 0)
        } else {
            (0, SUPERBLOCK_OFFSET as usize)
        }
    }

    // --- commit ---------------------------------------------------------

    /// Mark the superblock as needing a write at the next commit.
    pub(crate) fn dirty_super(&mut self) {
        self.sb_dirty = true;
    }

    /// Mutable group descriptor, remembering its original for rollback.
    pub(crate) fn group_mut(&mut self, g: u32) -> &mut GroupDesc {
        if let Some(sp) = &mut self.savepoint {
            sp.groups.entry(g).or_insert_with(|| self.groups[g as usize].clone());
        }
        &mut self.groups[g as usize]
    }

    /// Run a modifying operation atomically in memory: if it fails, every
    /// metadata change it made is undone before the error is returned.
    /// (Data already written to newly allocated blocks is simply orphaned
    /// again; overwritten data blocks cannot be restored, as on Linux.)
    pub(crate) fn op<T>(&mut self, f: impl FnOnce(&mut Fs) -> Result<T>) -> Result<T> {
        self.op_depth += 1;
        if self.op_depth == 1 && !self.read_only {
            self.start_savepoint();
        }
        let r = f(self);
        self.op_depth -= 1;
        if self.op_depth == 0 {
            match &r {
                Ok(_) => {
                    self.savepoint = None;
                    self.cache.end_undo();
                }
                Err(e) => {
                    if self.savepoint.is_some() {
                        log::debug!("rolling back failed operation: {e}");
                    }
                    self.rollback_op();
                }
            }
        }
        r
    }

    fn start_savepoint(&mut self) {
        self.savepoint = Some(Savepoint {
            sb: self.sb.raw.clone(),
            sb_dirty: self.sb_dirty,
            groups: HashMap::new(),
            dirty_groups: self.dirty_groups.clone(),
            bitmaps_dirty: self.bitmaps_dirty.clone(),
            deferred_len: self.deferred_free.len(),
            open_orphans: self.open_orphans.clone(),
            last_dir_group: self.last_dir_group,
        });
        self.cache.begin_undo();
    }

    fn rollback_op(&mut self) {
        let Some(sp) = self.savepoint.take() else {
            self.cache.end_undo();
            return;
        };
        self.sb.raw = sp.sb;
        self.sb_dirty = sp.sb_dirty;
        for (g, gd) in sp.groups {
            self.groups[g as usize] = gd;
        }
        self.dirty_groups = sp.dirty_groups;
        self.bitmaps_dirty = sp.bitmaps_dirty;
        self.deferred_free.truncate(sp.deferred_len);
        self.open_orphans = sp.open_orphans;
        self.last_dir_group = sp.last_dir_group;
        self.cache.rollback();
    }

    /// A commit is a point of no return for the running operation.
    fn end_savepoint(&mut self) {
        self.savepoint = None;
        self.cache.end_undo();
    }

    pub(crate) fn dirty_group(&mut self, g: u32) {
        self.dirty_groups.insert(g);
    }

    /// Copy dirty group descriptors and the superblock into their cached
    /// blocks so they become part of the transaction.
    fn stage_metadata(&mut self) -> Result<()> {
        self.stage_bitmap_csums()?;
        let groups: Vec<u32> = std::mem::take(&mut self.dirty_groups).into_iter().collect();
        let ds = self.sb.desc_size() as usize;
        for g in groups {
            if let Some(c) = self.group_desc_csum(g) {
                self.groups[g as usize].set_checksum(c);
            }
            let i = g / self.desc_per_block;
            let off = (g % self.desc_per_block) as usize * ds;
            let loc = self.desc_block_location(i);
            let raw = self.groups[g as usize].raw.clone();
            let b = self.cache.get_mut(&*self.dev, loc)?;
            b[off..off + ds].copy_from_slice(&raw);
        }
        if self.sb_dirty {
            self.sb_dirty = false;
            let now = Timestamp::now();
            self.sb.set_wtime(now.sec as u32);
            self.sb.set_wtime_hi((now.sec >> 32) as u8);
            self.sb.update_checksum();
            let (blk, off) = self.super_location();
            let raw = self.sb.raw.clone();
            let b = self.cache.get_mut(&*self.dev, blk)?;
            b[off..off + SUPERBLOCK_SIZE].copy_from_slice(&raw[..]);
        }
        Ok(())
    }

    /// Write all pending changes to disk atomically.
    pub fn commit(&mut self) -> Result<()> {
        if self.aborted {
            return Err(Error::Device(crate::error::errno::EIO));
        }
        if self.read_only {
            return Ok(());
        }
        self.end_savepoint();
        let started = std::time::Instant::now();
        let dirty = self.cache.dirty_count();
        let r = self.commit_inner();
        let ms = started.elapsed().as_millis();
        if ms >= 200 {
            log::info!("commit of {dirty} metadata blocks took {ms} ms");
        } else {
            log::debug!("commit of {dirty} metadata blocks took {ms} ms");
        }
        if let Err(e) = &r {
            // Like a jbd2 abort: stop writing. A committed-but-unwritten
            // transaction stays in the journal for the next mount.
            log::error!("commit failed, volume is now read-only: {e}");
            self.aborted = true;
            self.read_only = true;
        } else if self.op_depth > 0 {
            // committed in the middle of an operation (e.g. to reclaim
            // pending frees): the rest of it must still be undoable
            self.start_savepoint();
        }
        r
    }

    fn commit_inner(&mut self) -> Result<()> {
        let t0 = std::time::Instant::now();
        // blocks becoming reusable with this commit
        let freed = self.deferred_free.clone();
        self.release_deferred_frees()?;
        let t_free = t0.elapsed().as_millis();
        let dirty_estimate = self.cache.dirty_count() + self.dirty_groups.len() + self.bitmaps_dirty.len() + 1;
        let journal_fits = self.journal.as_ref().is_some_and(|j| j.fits(dirty_estimate));
        let in_place_with_journal = self.journal.is_some() && !journal_fits;
        if in_place_with_journal {
            // Too big for the journal: write in place, but mark the file
            // system not clean for the duration so a crash forces fsck.
            // Earlier transactions must be home first: replaying them after
            // the in-place writes would undo those.
            self.checkpoint_inner()?;
            log::warn!("transaction of ~{dirty_estimate} blocks exceeds the journal; writing in place");
            let s = self.sb.state() & !STATE_VALID;
            self.sb.set_state(s);
            self.dirty_super();
            self.stage_metadata()?;
            self.write_super_direct()?;
        } else {
            self.stage_metadata()?;
        }
        let t_stage = t0.elapsed().as_millis();
        let dirty = self.cache.dirty_blocks();
        if dirty.is_empty() {
            return Ok(());
        }
        if !in_place_with_journal && self.journal.is_some() {
            if !self.journal.as_ref().is_some_and(|j| j.fits_now(dirty.len())) {
                // make room (the checkpoint writes committed contents only,
                // so the running transaction's changes are unaffected)
                self.checkpoint_inner()?;
            }
            let j = self.journal.as_mut().expect("journal");
            let blocks: Vec<(u64, &[u8])> = dirty
                .iter()
                .map(|&b| (b, self.cache.peek(b).expect("dirty block cached")))
                .collect();
            j.append(&*self.dev, &blocks)?;
            let t_append = t0.elapsed().as_millis();
            self.cache.log_dirty();
            // Checkpoint now when a block that becomes reusable still has a
            // copy in the log: it may be reused for file data right after
            // this commit, and replaying (or checkpointing) the old copy
            // would overwrite that data. Also keep half the log free and
            // bound the memory held for the checkpoint.
            let freed_logged = freed.iter().any(|&(s, n)| self.cache.logged_intersects(s, n));
            let j = self.journal.as_ref().expect("journal");
            let ckpt =
                freed_logged || j.used() as usize * 2 > j.capacity() as usize || self.cache.logged_len() > MAX_LOGGED;
            if ckpt {
                self.checkpoint_inner()?;
            }
            log::debug!(
                "commit phases: frees {t_free} ms, stage {t_stage} ms, append {t_append} ms, checkpoint {ckpt} \
                 (freed blocks in the log: {freed_logged}), total {} ms",
                t0.elapsed().as_millis()
            );
            return Ok(());
        }
        self.dev.flush()?;
        self.cache.write_back(&*self.dev)?;
        self.dev.flush()?;
        if in_place_with_journal {
            let s = self.sb.state() | STATE_VALID;
            self.sb.set_state(s);
            self.write_super_direct()?;
            self.dirty_super();
        }
        Ok(())
    }

    /// Commit if enough changes accumulated (called after each operation).
    pub(crate) fn maybe_commit(&mut self) -> Result<()> {
        let mut threshold = self.opts.commit_threshold;
        if let Some(j) = &self.journal {
            threshold = threshold.min(j.capacity() as usize / 4);
        }
        if self.cache.dirty_count() + self.dirty_groups.len() >= threshold.max(1) {
            self.commit()?;
        }
        Ok(())
    }

    /// Whether uncommitted changes exist.
    pub fn has_pending_changes(&self) -> bool {
        self.cache.dirty_count() > 0 || !self.dirty_groups.is_empty() || self.sb_dirty || !self.deferred_free.is_empty()
    }

    /// Write every block waiting in the journal to its home location and
    /// mark the log empty. Only committed contents are written, so this is
    /// safe at any time; it keeps the volume consistent without a replay.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.aborted {
            return Err(Error::Device(crate::error::errno::EIO));
        }
        if self.read_only {
            return Ok(());
        }
        let r = self.checkpoint_inner();
        if let Err(e) = &r {
            log::error!("checkpoint failed, volume is now read-only: {e}");
            self.aborted = true;
            self.read_only = true;
        }
        r
    }

    fn checkpoint_inner(&mut self) -> Result<()> {
        let pending = self.journal.as_ref().is_some_and(|j| j.has_pending_checkpoint());
        if !pending && self.cache.logged_len() == 0 {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let bs = self.bs as u64;
        let blocks = self.cache.logged_sorted();
        let count = blocks.len();
        // runs of consecutive blocks (inode tables, bitmaps) in one write
        let mut i = 0;
        while i < blocks.len() {
            let mut j = i + 1;
            while j < blocks.len() && blocks[j].0 == blocks[j - 1].0 + 1 {
                j += 1;
            }
            if j - i == 1 {
                self.dev.write_at(blocks[i].0 * bs, blocks[i].1)?;
            } else {
                let mut run = Vec::with_capacity((j - i) * bs as usize);
                for b in &blocks[i..j] {
                    run.extend_from_slice(b.1);
                }
                self.dev.write_at(blocks[i].0 * bs, &run)?;
            }
            i = j;
        }
        self.dev.flush()?;
        if let Some(j) = self.journal.as_mut() {
            j.mark_checkpointed(&*self.dev)?;
        }
        self.cache.clear_logged();
        log::debug!("checkpoint of {count} blocks took {} ms", started.elapsed().as_millis());
        Ok(())
    }

    /// Whether committed transactions wait in the journal for a checkpoint.
    pub fn has_pending_checkpoint(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.has_pending_checkpoint())
    }

    /// Commit and checkpoint: everything on disk at its home location.
    pub fn sync(&mut self) -> Result<()> {
        self.commit()?;
        self.checkpoint()?;
        if !self.read_only {
            self.dev.flush()?;
        }
        Ok(())
    }

    /// Commit and restore the clean on-disk state.
    pub fn unmount(mut self) -> Result<()> {
        self.unmount_in_place()
    }

    /// Make a volume that was unmounted in place writable again (FSKit may
    /// mount a volume again without re-activating it).
    pub fn remount_rw(&mut self) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        if self.aborted || self.opts.read_only || self.dev.is_read_only() || !self.report.read_only_reasons.is_empty() {
            return Err(Error::ReadOnly);
        }
        self.read_only = false;
        if let Err(e) = self.mark_mounted() {
            self.read_only = true;
            return Err(e);
        }
        Ok(())
    }

    pub fn unmount_in_place(&mut self) -> Result<()> {
        if self.aborted {
            // leave needs_recovery set: the journal may hold a transaction
            return Err(Error::Device(crate::error::errno::EIO));
        }
        if self.read_only {
            return Ok(());
        }
        self.reclaim_all()?;
        self.commit()?;
        // an empty journal: clean for Linux and e2fsck, no replay needed
        self.checkpoint()?;
        if self.mounted_rw {
            if self.journal.is_some() {
                let f = self.sb.feature_incompat() & !incompat::RECOVER;
                self.sb.set_feature_incompat(f);
            } else {
                let s = self.sb.state() | STATE_VALID;
                self.sb.set_state(s);
            }
            self.write_super_direct()?;
            self.mounted_rw = false;
        }
        self.read_only = true;
        Ok(())
    }

    /// Space statistics. Like Linux (`bsddf`, the default), the total
    /// excludes the file system's own metadata (superblock copies,
    /// descriptor tables, bitmaps, inode tables, journal), so a fresh volume
    /// shows almost nothing used. Reserved blocks count as free but not as
    /// available.
    pub fn statfs(&self) -> StatFs {
        let blocks = self.sb.blocks_count().saturating_sub(self.zone.total());
        let pending: u64 = self.deferred_free.iter().map(|&(_, n)| n).sum();
        let free = self.sb.free_blocks_count().saturating_add(pending).min(blocks);
        let reserved = self.sb.r_blocks_count();
        let files = self.sb.inodes_count() as u64;
        StatFs {
            block_size: self.bs,
            blocks,
            free_blocks: free,
            avail_blocks: free.saturating_sub(reserved),
            files,
            free_files: (self.sb.free_inodes_count() as u64).min(files),
            name_max: 255,
        }
    }

    pub(crate) fn require_rw(&self) -> Result<()> {
        if self.read_only { Err(Error::ReadOnly) } else { Ok(()) }
    }

    /// Current volume label.
    pub fn label(&self) -> String {
        self.sb.volume_name()
    }

    /// Change the volume label (at most 16 bytes of UTF-8).
    pub fn set_label(&mut self, label: &str) -> Result<()> {
        self.require_rw()?;
        if label.len() > 16 {
            return Err(Error::NameTooLong);
        }
        self.op(|fs| {
            fs.sb.set_volume_name(label);
            fs.dirty_super();
            Ok(())
        })?;
        self.commit()
    }

    /// Journal inode number, if the file system has an internal journal.
    pub fn journal_ino(&self) -> Option<Ino> {
        self.journal.as_ref().map(|_| JOURNAL_INO)
    }

    pub fn journal_commits(&self) -> u64 {
        self.journal.as_ref().map_or(0, |j| j.commits)
    }

    /// Drop the mount without writing anything (after an internal error):
    /// the on-disk state, including needs_recovery, is left as is.
    pub fn abandon(mut self) {
        self.read_only = true;
        self.mounted_rw = false;
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        if !self.read_only
            && self.mounted_rw
            && let Err(e) = self.unmount_in_place()
        {
            log::error!("unmount on drop failed: {e}");
        }
    }
}

fn rand_seed() -> u32 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let x = t.as_nanos() as u64 ^ (std::process::id() as u64) << 32;
    (x ^ (x >> 29) ^ (x >> 7)) as u32 | 1
}
