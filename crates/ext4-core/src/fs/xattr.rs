//! Extended attributes (in-inode area and external block, with
//! copy-on-write for shared blocks).

use super::{Fs, Ino, XattrSetMode};
use crate::error::{Error, Result};
use crate::ondisk::inode::{Inode, Timestamp, flags};
use crate::ondisk::xattr::{self as xa, XattrEntry};

struct Loaded {
    ibody: Vec<XattrEntry>,
    block: Option<(u64, u32, Vec<XattrEntry>)>,
}

impl Fs {
    fn load_xattrs(&mut self, ino: Ino, inode: &Inode) -> Result<Loaded> {
        let ibody = match inode.xattr_area() {
            Some(r) => xa::parse_ibody(&inode.raw[r])?,
            None => Vec::new(),
        };
        let blk = inode.file_acl();
        let block = if blk != 0 {
            if blk >= self.sb.blocks_count() || blk < self.sb.first_data_block() as u64 {
                return Err(Error::corrupt(format!("inode {ino}: xattr block {blk} out of range")));
            }
            let data = self.cache.read(&*self.dev, blk)?;
            if self.sb.has_metadata_csum() && !xa::verify_block_csum(&data, self.csum_seed, blk) {
                self.checksum_error(format!("xattr block {blk}"))?;
            }
            let (h, entries) = xa::parse_block(&data)?;
            Some((blk, h.refcount, entries))
        } else {
            None
        };
        Ok(Loaded { ibody, block })
    }

    /// Value of an attribute stored in an EA inode.
    fn ea_inode_value(&mut self, e: &XattrEntry) -> Result<Vec<u8>> {
        let inode = self.read_inode(e.value_inum)?;
        if !inode.has_flag(flags::EA_INODE) {
            return Err(Error::corrupt("xattr value inode lacks EA_INODE flag"));
        }
        let mut v = vec![0u8; inode.size() as usize];
        let n = self.read_inode_data(e.value_inum, &inode, 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    fn entry_value(&mut self, e: &XattrEntry) -> Result<Vec<u8>> {
        if e.value_inum != 0 {
            self.ea_inode_value(e)
        } else {
            Ok(e.value.clone())
        }
    }

    /// Get attribute `name` (full name, e.g. `user.foo`).
    pub fn get_xattr(&mut self, ino: Ino, name: &[u8]) -> Result<Vec<u8>> {
        let inode = self.read_live_inode(ino)?;
        let (idx, suffix) = xa::split_name(name);
        if idx == 0 {
            return Err(Error::NoAttr);
        }
        let l = self.load_xattrs(ino, &inode)?;
        let found = l
            .ibody
            .iter()
            .chain(l.block.iter().flat_map(|b| b.2.iter()))
            .find(|e| e.index == idx && e.name == suffix)
            .cloned();
        match found {
            Some(e) => self.entry_value(&e),
            None => Err(Error::NoAttr),
        }
    }

    /// Full names of all attributes.
    pub fn list_xattr(&mut self, ino: Ino) -> Result<Vec<Vec<u8>>> {
        let inode = self.read_live_inode(ino)?;
        let l = self.load_xattrs(ino, &inode)?;
        let mut v: Vec<Vec<u8>> = l
            .ibody
            .iter()
            .chain(l.block.iter().flat_map(|b| b.2.iter()))
            // system.data holds inline file contents: hide it
            .filter(|e| !(e.index == xa::INDEX_SYSTEM && e.name == b"data"))
            .map(|e| e.full_name())
            .collect();
        v.sort();
        v.dedup();
        Ok(v)
    }

    pub fn set_xattr(&mut self, ino: Ino, name: &[u8], value: &[u8], mode: XattrSetMode) -> Result<()> {
        self.require_rw()?;
        let (idx, suffix) = xa::split_name(name);
        if idx == 0 {
            return Err(Error::unsupported("unknown xattr namespace"));
        }
        if suffix.len() > 255 {
            return Err(Error::NameTooLong);
        }
        if idx == xa::INDEX_SYSTEM && suffix == b"data" {
            return Err(Error::NotPermitted);
        }
        let max_value = self.bs as usize - xa::BLOCK_HEADER_SIZE - xa::pad(xa::ENTRY_HEADER_SIZE + suffix.len()) - 4;
        if value.len() > max_value {
            return Err(Error::NoSpace);
        }
        let mut inode = self.read_live_inode(ino)?;
        let exists = self.remove_entry_from(ino, &mut inode, idx, suffix, true)?;
        match mode {
            XattrSetMode::Create if exists => return Err(Error::Exists),
            XattrSetMode::Replace if !exists => return Err(Error::NoAttr),
            _ => {}
        }
        let entry = XattrEntry {
            index: idx,
            name: suffix.to_vec(),
            value: value.to_vec(),
            value_inum: 0,
            hash: xa::entry_hash(suffix, value),
        };
        let l = self.load_xattrs(ino, &inode)?;
        let mut ibody = l.ibody.clone();
        let mut placed = false;
        if let Some(r) = inode.xattr_area() {
            let mut candidate = ibody.clone();
            candidate.push(entry.clone());
            if xa::space_needed(&candidate) + 4 <= r.len() {
                ibody = candidate;
                xa::write_ibody(&mut inode.raw[r], &ibody)?;
                placed = true;
            }
        }
        if !placed {
            let mut entries = l.block.as_ref().map(|b| b.2.clone()).unwrap_or_default();
            entries.push(entry);
            if xa::space_needed(&entries) + xa::BLOCK_HEADER_SIZE > self.bs as usize {
                return Err(Error::NoSpace);
            }
            self.store_xattr_block(ino, &mut inode, l.block.as_ref().map(|b| (b.0, b.1)), entries)?;
        }
        inode.set_ctime(Timestamp::now());
        self.write_inode(ino, &inode)?;
        self.maybe_commit()
    }

    pub fn remove_xattr(&mut self, ino: Ino, name: &[u8]) -> Result<()> {
        self.require_rw()?;
        let (idx, suffix) = xa::split_name(name);
        if idx == 0 {
            return Err(Error::NoAttr);
        }
        let mut inode = self.read_live_inode(ino)?;
        if !self.remove_entry_from(ino, &mut inode, idx, suffix, false)? {
            return Err(Error::NoAttr);
        }
        inode.set_ctime(Timestamp::now());
        self.write_inode(ino, &inode)?;
        self.maybe_commit()
    }

    /// Remove `idx.suffix` wherever it is stored. Returns whether it existed.
    fn remove_entry_from(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        idx: u8,
        suffix: &[u8],
        _replacing: bool,
    ) -> Result<bool> {
        let l = self.load_xattrs(ino, inode)?;
        let matches = |e: &XattrEntry| e.index == idx && e.name == suffix;
        if l.ibody.iter().any(matches) {
            let rest: Vec<XattrEntry> = l.ibody.into_iter().filter(|e| !matches(e)).collect();
            let r = inode.xattr_area().unwrap();
            xa::write_ibody(&mut inode.raw[r], &rest)?;
            return Ok(true);
        }
        if let Some((blk, refc, entries)) = l.block
            && entries.iter().any(matches)
        {
            if entries.iter().any(|e| matches(e) && e.value_inum != 0) {
                return Err(Error::unsupported("modifying EA-inode attributes"));
            }
            let rest: Vec<XattrEntry> = entries.into_iter().filter(|e| !matches(e)).collect();
            self.store_xattr_block(ino, inode, Some((blk, refc)), rest)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Write `entries` as the inode's xattr block (allocating, rewriting,
    /// copying on write, or freeing as appropriate).
    fn store_xattr_block(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        old: Option<(u64, u32)>,
        mut entries: Vec<XattrEntry>,
    ) -> Result<()> {
        let bs = self.bs as usize;
        let per = self.bs as u64 / 512;
        xa::sort_entries(&mut entries);
        if entries.is_empty() {
            if let Some((blk, refc)) = old {
                self.release_xattr_block(blk, refc)?;
                inode.set_file_acl(0);
                let cur = inode.sectors(self.bs, self.huge_file());
                inode.set_sectors(cur.saturating_sub(per));
            }
            return Ok(());
        }
        let target = match old {
            Some((blk, 1)) => blk,
            Some((blk, refc)) => {
                // shared: detach and write a private copy
                self.release_xattr_block(blk, refc)?;
                let goal = blk;
                let (nb, _) = self.alloc_blocks(goal, 1)?;
                nb
            }
            None => {
                let ipg = self.sb.inodes_per_group();
                let goal = self.group_first_block((ino - 1) / ipg);
                let (nb, _) = self.alloc_blocks(goal, 1)?;
                let cur = inode.sectors(self.bs, self.huge_file());
                inode.set_sectors(cur + per);
                nb
            }
        };
        let mut data = vec![0u8; bs];
        let csum = self.sb.has_metadata_csum().then_some((self.csum_seed, target));
        xa::build_block(&mut data, 1, &entries, csum)?;
        self.cache.put(target, &data);
        inode.set_file_acl(target);
        Ok(())
    }

    /// Drop one reference to an xattr block, freeing it at zero.
    fn release_xattr_block(&mut self, blk: u64, refc: u32) -> Result<()> {
        if refc <= 1 {
            return self.free_blocks(blk, 1);
        }
        let mut data = self.cache.read(&*self.dev, blk)?;
        xa::set_block_refcount(&mut data, refc - 1);
        if self.sb.has_metadata_csum() {
            xa::set_block_csum(&mut data, self.csum_seed, blk);
        }
        self.cache.put(blk, &data);
        Ok(())
    }

    /// Release the xattr block of an inode being deleted.
    pub(crate) fn free_inode_xattrs(&mut self, inode: &mut Inode) -> Result<()> {
        let blk = inode.file_acl();
        if blk == 0 {
            return Ok(());
        }
        let data = self.cache.read(&*self.dev, blk)?;
        let (h, _) = xa::parse_block(&data)?;
        self.release_xattr_block(blk, h.refcount)?;
        inode.set_file_acl(0);
        let per = self.bs as u64 / 512;
        let cur = inode.sectors(self.bs, self.huge_file());
        inode.set_sectors(cur.saturating_sub(per));
        Ok(())
    }
}
