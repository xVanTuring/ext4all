//! Inode table access and attribute conversion.

use super::{Attr, Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::inode::{FileType, Inode};
use crate::ondisk::superblock::ro_compat;

impl Fs {
    pub(crate) fn inode_location(&self, ino: Ino) -> Result<(u64, usize)> {
        if ino == 0 || ino > self.sb.inodes_count() {
            return Err(Error::invalid(format!("inode number {ino} out of range")));
        }
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        let idx = (ino - 1) % ipg;
        let isz = self.sb.inode_size() as u64;
        let byte = idx as u64 * isz;
        let blk = self.groups[g as usize].inode_table() + byte / self.bs as u64;
        Ok((blk, (byte % self.bs as u64) as usize))
    }

    /// Read an inode without checksum verification.
    pub(crate) fn read_inode_raw(&mut self, ino: Ino) -> Result<Inode> {
        let (blk, off) = self.inode_location(ino)?;
        let isz = self.sb.inode_size() as usize;
        let b = self.cache.get(&*self.dev, blk)?;
        Ok(Inode::new(&b[off..off + isz]))
    }

    pub(crate) fn read_inode(&mut self, ino: Ino) -> Result<Inode> {
        let inode = self.read_inode_raw(ino)?;
        if self.sb.has_metadata_csum() && !inode.verify_checksum(self.csum_seed, ino) {
            // An all-zero inode (never used) has no valid checksum; accept it.
            if inode.raw.iter().any(|&b| b != 0) {
                self.checksum_error(format!("inode {ino}"))?;
            }
        }
        Ok(inode)
    }

    /// Read an inode that must be in use (links > 0 or mode set).
    pub(crate) fn read_live_inode(&mut self, ino: Ino) -> Result<Inode> {
        let inode = self.read_inode(ino)?;
        if inode.mode() == 0 || (inode.links_count() == 0 && inode.dtime() != 0 && !self.is_orphan(ino)) {
            return Err(Error::NotFound);
        }
        Ok(inode)
    }

    pub(crate) fn write_inode(&mut self, ino: Ino, inode: &Inode) -> Result<()> {
        self.require_rw()?;
        let (blk, off) = self.inode_location(ino)?;
        let mut inode = inode.clone();
        if self.sb.has_metadata_csum() {
            inode.update_checksum(self.csum_seed, ino);
        }
        let b = self.cache.get_mut(&*self.dev, blk)?;
        b[off..off + inode.raw.len()].copy_from_slice(&inode.raw);
        Ok(())
    }

    /// Checksum seed for metadata blocks owned by an inode.
    pub(crate) fn inode_seed(&self, ino: Ino, inode: &Inode) -> u32 {
        Inode::csum_seed(self.csum_seed, ino, inode.generation())
    }

    pub(crate) fn huge_file(&self) -> bool {
        self.sb.has_ro_compat(ro_compat::HUGE_FILE)
    }

    pub(crate) fn inode_attr(&self, ino: Ino, inode: &Inode) -> Attr {
        let ft = inode.file_type();
        let rdev = match ft {
            FileType::CharDev | FileType::BlockDev => inode.rdev(),
            _ => 0,
        };
        let mut nlink = inode.links_count() as u32;
        if ft == FileType::Directory && nlink == 1 {
            // dir_nlink: link count overflowed; report "unknown" as 1 like Linux
            nlink = 1;
        }
        Attr {
            ino,
            file_type: ft,
            perm: inode.perm(),
            nlink,
            uid: inode.uid(),
            gid: inode.gid(),
            size: inode.size(),
            allocated: inode.sectors(self.bs, self.huge_file()) * 512,
            atime: inode.atime(),
            mtime: inode.mtime(),
            ctime: inode.ctime(),
            crtime: inode.crtime(),
            flags: inode.flags(),
            generation: inode.generation(),
            rdev,
        }
    }

    /// Attributes of an inode.
    pub fn stat(&mut self, ino: Ino) -> Result<Attr> {
        let inode = self.read_live_inode(ino)?;
        self.full_attr(ino, &inode)
    }

    /// [`Fs::inode_attr`] plus what needs more than the inode: the size of
    /// an encrypted symlink is the length of its target as presented.
    pub(crate) fn full_attr(&mut self, ino: Ino, inode: &Inode) -> Result<Attr> {
        if inode.is_symlink() && inode.has_flag(crate::ondisk::inode::flags::ENCRYPT) {
            return self.symlink_attr(ino, inode);
        }
        Ok(self.inode_attr(ino, inode))
    }

    /// Fresh generation number for a new inode.
    pub(crate) fn next_generation(&mut self) -> u32 {
        // xorshift32
        let mut x = self.generation_seed;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.generation_seed = x;
        x
    }
}
