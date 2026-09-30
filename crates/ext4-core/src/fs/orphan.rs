//! Orphan inodes: unlinked-but-open files and interrupted truncates.
//!
//! We keep orphans on the classic superblock list (`s_last_orphan`, chained
//! through `i_dtime`). At mount both the list and the newer orphan file are
//! processed.

use super::{Fs, Ino};
use crate::bytes::{le32, set_le32};
use crate::csum::crc32c;
use crate::error::{Error, Result};
use crate::ondisk::inode::{Inode, Timestamp};
use crate::ondisk::superblock::{compat, ro_compat};

const ORPHAN_BLOCK_MAGIC: u32 = 0x0b10_ca04;

impl Fs {
    pub(crate) fn is_orphan(&self, ino: Ino) -> bool {
        self.open_orphans.contains(&ino)
    }

    /// Put an inode on the orphan list (caller writes the inode).
    pub(crate) fn orphan_add(&mut self, ino: Ino, inode: &mut Inode) {
        inode.set_dtime(self.sb.last_orphan());
        self.sb.set_last_orphan(ino);
        self.dirty_super();
        self.open_orphans.insert(ino);
    }

    /// Unlink an inode from the orphan list (caller writes the inode).
    pub(crate) fn orphan_remove(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        let next = inode.dtime();
        if self.sb.last_orphan() == ino {
            self.sb.set_last_orphan(next);
            self.dirty_super();
        } else {
            let mut cur = self.sb.last_orphan();
            let mut steps = 0;
            while cur != 0 {
                steps += 1;
                if steps > self.sb.inodes_count() {
                    return Err(Error::corrupt("orphan list loop"));
                }
                let mut ci = self.read_inode(cur)?;
                if ci.dtime() == ino {
                    ci.set_dtime(next);
                    self.write_inode(cur, &ci)?;
                    break;
                }
                cur = ci.dtime();
            }
        }
        inode.set_dtime(0);
        self.open_orphans.remove(&ino);
        Ok(())
    }

    /// Final release of an inode with no links: data, xattrs, bitmap.
    pub(crate) fn destroy_inode(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        let is_dir = inode.is_dir();
        self.free_all_blocks(ino, inode)?;
        self.free_inode_xattrs(inode)?;
        inode.set_links_count(0);
        inode.set_size(0);
        inode.set_sectors(0);
        inode.set_dtime(Timestamp::now().sec as u32);
        self.write_inode(ino, inode)?;
        self.free_inode(ino, is_dir)
    }

    /// Called when the last kernel reference to an inode goes away.
    pub fn reclaim(&mut self, ino: Ino) -> Result<()> {
        if self.read_only || !self.open_orphans.contains(&ino) {
            return Ok(());
        }
        let mut inode = self.read_inode(ino)?;
        self.orphan_remove(ino, &mut inode)?;
        if inode.links_count() == 0 {
            self.destroy_inode(ino, &mut inode)?;
        } else {
            self.write_inode(ino, &inode)?;
        }
        self.maybe_commit()
    }

    /// Release every pending orphan (unmount).
    pub(crate) fn reclaim_all(&mut self) -> Result<()> {
        let all: Vec<Ino> = self.open_orphans.iter().copied().collect();
        for ino in all {
            self.reclaim(ino)?;
        }
        Ok(())
    }

    /// Process orphans left by a previous mount.
    pub(crate) fn process_orphans(&mut self) -> Result<()> {
        let mut count = 0u32;
        let mut cur = self.sb.last_orphan();
        let mut steps = 0;
        while cur != 0 {
            steps += 1;
            if steps > self.sb.inodes_count() || cur > self.sb.inodes_count() {
                log::warn!("orphan list corrupted; stopping");
                break;
            }
            let mut inode = self.read_inode(cur)?;
            let next = inode.dtime();
            inode.set_dtime(0);
            self.finish_orphan(cur, &mut inode)?;
            count += 1;
            cur = next;
        }
        if self.sb.last_orphan() != 0 {
            self.sb.set_last_orphan(0);
            self.dirty_super();
        }
        if self.sb.has_compat(compat::ORPHAN_FILE) && self.sb.has_ro_compat(ro_compat::ORPHAN_PRESENT) {
            count += self.process_orphan_file()?;
            let f = self.sb.feature_ro_compat() & !ro_compat::ORPHAN_PRESENT;
            self.sb.set_feature_ro_compat(f);
            self.dirty_super();
        }
        self.report.orphans_processed = count;
        if count > 0 || self.has_pending_changes() {
            self.commit()?;
        }
        Ok(())
    }

    fn finish_orphan(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        if inode.links_count() == 0 {
            if inode.mode() != 0 && self.inode_in_use(ino)? {
                self.destroy_inode(ino, inode)?;
            }
        } else {
            // interrupted truncate: drop blocks past i_size
            if inode.is_reg() && inode.has_flag(crate::ondisk::inode::flags::EXTENTS) {
                let first = inode.size().div_ceil(self.bs as u64);
                self.free_range(ino, inode, first, 1 << 32)?;
            }
            self.write_inode(ino, inode)?;
        }
        Ok(())
    }

    fn process_orphan_file(&mut self) -> Result<u32> {
        let oino = self.sb.orphan_file_inum();
        if oino == 0 {
            return Ok(0);
        }
        let oinode = self.read_inode(oino)?;
        let seed = self.inode_seed(oino, &oinode);
        let nblocks = oinode.size() / self.bs as u64;
        let per = (self.bs as usize - 8) / 4;
        let mut count = 0;
        for lblk in 0..nblocks {
            let pblk = match self.map_block(oino, &oinode, lblk)? {
                super::extent::Mapping::Mapped { pblk, .. } => pblk,
                _ => continue,
            };
            let mut data = self.cache.read(&*self.dev, pblk)?;
            if le32(&data, per * 4) != ORPHAN_BLOCK_MAGIC {
                continue;
            }
            let mut changed = false;
            for i in 0..per {
                let ino = le32(&data, i * 4);
                if ino == 0 {
                    continue;
                }
                if ino <= self.sb.inodes_count() {
                    let mut inode = self.read_inode(ino)?;
                    self.finish_orphan(ino, &mut inode)?;
                    count += 1;
                }
                set_le32(&mut data, i * 4, 0);
                changed = true;
            }
            if changed {
                if self.sb.has_metadata_csum() {
                    let c = crc32c(seed, &(lblk as u32).to_le_bytes());
                    let c = crc32c(c, &data[..per * 4]);
                    set_le32(&mut data, per * 4 + 4, c);
                }
                self.cache.put(pblk, &data);
            }
        }
        Ok(count)
    }
}
