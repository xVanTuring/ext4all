//! Directories: lookup, enumeration, entry insertion and removal.

use super::extent::Mapping;
use super::{Attr, DirEntryInfo, Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::dirent::{self as de, DirEntry, MAX_NAME_LEN, TAIL_SIZE};
use crate::ondisk::extent::Extent;
use crate::ondisk::inode::{FileType, Inode, Timestamp, flags};
use crate::ondisk::superblock::{compat, incompat};
use crate::ondisk::xattr::{self as xa, INDEX_SYSTEM};

/// Location of a directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirSlot {
    pub lblk: u64,
    pub pblk: u64,
    pub offset: usize,
    /// Offset of the previous entry in the same block.
    pub prev: Option<usize>,
    pub ino: Ino,
    pub file_type: u8,
}

/// What kind of block a directory block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirBlockKind {
    Leaf,
    DxRoot,
    DxNode,
}

pub(crate) fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("empty name"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(Error::NameTooLong);
    }
    if name.contains(&b'/') || name.contains(&0) {
        return Err(Error::invalid("name contains '/' or NUL"));
    }
    Ok(())
}

impl Fs {
    pub(crate) fn dirent_type(&self, ft: FileType) -> u8 {
        if self.sb.has_incompat(incompat::FILETYPE) {
            ft as u8
        } else {
            0
        }
    }

    fn dir_csum(&self) -> bool {
        self.sb.has_metadata_csum()
    }

    /// Classify a directory block.
    pub(crate) fn dir_block_kind(&self, inode: &Inode, lblk: u64, data: &[u8]) -> DirBlockKind {
        let indexed = inode.has_flag(flags::INDEX);
        if indexed && lblk == 0 {
            return DirBlockKind::DxRoot;
        }
        if indexed {
            let first_ino = crate::bytes::le32(data, 0);
            let rec = de::rec_len_from_disk(crate::bytes::le16(data, 4), self.bs as usize);
            if first_ino == 0 && rec == self.bs as usize && data[6] == 0 {
                return DirBlockKind::DxNode;
            }
        }
        DirBlockKind::Leaf
    }

    /// Read directory block `lblk`, verifying its checksum.
    pub(crate) fn dir_block(&mut self, ino: Ino, inode: &Inode, lblk: u64) -> Result<(u64, Vec<u8>)> {
        let pblk = match self.map_block(ino, inode, lblk)? {
            Mapping::Mapped {
                pblk, unwritten: false, ..
            } => pblk,
            _ => return Err(Error::corrupt(format!("directory {ino}: hole at block {lblk}"))),
        };
        let data = self.cache.read(&*self.dev, pblk)?;
        if self.dir_csum() {
            let seed = self.inode_seed(ino, inode);
            let ok = match self.dir_block_kind(inode, lblk, &data) {
                DirBlockKind::Leaf => de::verify_leaf_csum(seed, &data),
                DirBlockKind::DxRoot => de::verify_dx_csum(
                    seed,
                    &data,
                    de::DX_ROOT_INFO_OFF + data[de::DX_ROOT_INFO_OFF + 5] as usize,
                ),
                DirBlockKind::DxNode => de::verify_dx_csum(seed, &data, de::DX_NODE_ENTRIES_OFF),
            };
            if !ok {
                self.checksum_error(format!("directory {ino} block {lblk}"))?;
            }
        }
        Ok((pblk, data))
    }

    /// Bytes of a leaf block that hold entries (excludes the tail).
    pub(crate) fn leaf_limit(&self, data: &[u8]) -> usize {
        if self.dir_csum() && de::has_tail(data) {
            data.len() - TAIL_SIZE
        } else {
            data.len()
        }
    }

    pub(crate) fn write_leaf_block(&mut self, ino: Ino, inode: &Inode, pblk: u64, mut data: Vec<u8>) {
        if self.dir_csum() && de::has_tail(&data) {
            de::set_leaf_csum(self.inode_seed(ino, inode), &mut data);
        }
        self.cache.put(pblk, &data);
    }

    pub(crate) fn write_dx_block(&mut self, ino: Ino, inode: &Inode, pblk: u64, mut data: Vec<u8>, entries_off: usize) {
        if self.dir_csum() {
            de::set_dx_csum(self.inode_seed(ino, inode), &mut data, entries_off);
        }
        self.cache.put(pblk, &data);
    }

    pub(crate) fn dir_nblocks(&self, inode: &Inode) -> u64 {
        inode.size() / self.bs as u64
    }

    // --- inline directories ----------------------------------------------

    /// Entries of an inline directory: (name, ino, file_type), with ".."
    /// synthesized from the parent pointer.
    fn inline_dir_entries(&self, inode: &Inode) -> Result<Vec<(Vec<u8>, Ino, u8)>> {
        let area = inode.block_area();
        let parent = crate::bytes::le32(area, 0);
        let mut out = vec![(b"..".to_vec(), parent, FileType::Directory as u8)];
        let mut regions: Vec<Vec<u8>> = vec![area[4..].to_vec()];
        if let Some(r) = inode.xattr_area() {
            let entries = xa::parse_ibody(&inode.raw[r])?;
            if let Some(e) = entries.iter().find(|e| e.index == INDEX_SYSTEM && e.name == b"data")
                && !e.value.is_empty()
            {
                regions.push(e.value.clone());
            }
        }
        for reg in regions {
            let bs = self.bs as usize;
            for d in de::parse_block(&reg, reg.len(), bs)? {
                if d.inode != 0 {
                    out.push((d.name(&reg).to_vec(), d.inode, d.file_type));
                }
            }
        }
        Ok(out)
    }

    /// Convert an inline directory to a regular one-block directory.
    fn uninline_dir(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        let entries = self.inline_dir_entries(inode)?;
        let parent = entries[0].1;
        if let Some(r) = inode.xattr_area() {
            let mut xs = xa::parse_ibody(&inode.raw[r.clone()])?;
            xs.retain(|e| !(e.index == INDEX_SYSTEM && e.name == b"data"));
            xa::write_ibody(&mut inode.raw[r], &xs)?;
        }
        inode.set_flag(flags::INLINE_DATA, false);
        self.to_extent_mapped(inode);
        inode.set_size(0);
        self.init_dir_blocks(ino, inode, parent)?;
        for (name, child, ft) in entries.into_iter().skip(1) {
            self.add_entry_linear(ino, inode, &name, child, ft)?;
        }
        Ok(())
    }

    // --- lookup -----------------------------------------------------------

    /// Find `name` in a directory.
    pub(crate) fn find_entry(&mut self, ino: Ino, inode: &Inode, name: &[u8]) -> Result<Option<DirSlot>> {
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.has_flag(flags::INLINE_DATA) {
            for (n, child, ft) in self.inline_dir_entries(inode)? {
                if n == name {
                    return Ok(Some(DirSlot {
                        lblk: u64::MAX,
                        pblk: 0,
                        offset: 0,
                        prev: None,
                        ino: child,
                        file_type: ft,
                    }));
                }
            }
            return Ok(None);
        }
        if name == b"." || name == b".." {
            // always in the first block (the dx root of an htree directory)
            if self.dir_nblocks(inode) == 0 {
                return Ok(None);
            }
            return self.find_in_block(ino, inode, 0, name);
        }
        if inode.has_flag(flags::INDEX) && self.sb.has_compat(compat::DIR_INDEX) {
            match self.dx_find(ino, inode, name) {
                Ok(r) => return Ok(r),
                Err(Error::Unsupported(_)) => {} // e.g. casefold hash: scan linearly
                Err(e) => return Err(e),
            }
        }
        for lblk in 0..self.dir_nblocks(inode) {
            if let Some(slot) = self.find_in_block(ino, inode, lblk, name)? {
                return Ok(Some(slot));
            }
        }
        Ok(None)
    }

    /// Search one leaf block for `name`.
    pub(crate) fn find_in_block(&mut self, ino: Ino, inode: &Inode, lblk: u64, name: &[u8]) -> Result<Option<DirSlot>> {
        let (pblk, data) = self.dir_block(ino, inode, lblk)?;
        let kind = self.dir_block_kind(inode, lblk, &data);
        let entries = self.block_entries(&data, kind)?;
        let mut prev = None;
        for d in entries {
            if d.inode != 0 && d.name(&data) == name {
                return Ok(Some(DirSlot {
                    lblk,
                    pblk,
                    offset: d.offset,
                    prev,
                    ino: d.inode,
                    file_type: d.file_type,
                }));
            }
            prev = Some(d.offset);
        }
        Ok(None)
    }

    /// Parsed entries of a directory block (dx blocks expose only "." and
    /// ".." of the root).
    pub(crate) fn block_entries(&self, data: &[u8], kind: DirBlockKind) -> Result<Vec<DirEntry>> {
        let bs = self.bs as usize;
        match kind {
            DirBlockKind::DxNode => Ok(Vec::new()),
            DirBlockKind::DxRoot => {
                let mut v = Vec::new();
                let dot = de::rec_len_from_disk(crate::bytes::le16(data, 4), bs);
                v.push(DirEntry {
                    offset: 0,
                    inode: crate::bytes::le32(data, 0),
                    rec_len: dot,
                    name_len: data[6] as usize,
                    file_type: data[7],
                });
                v.push(DirEntry {
                    offset: 12,
                    inode: crate::bytes::le32(data, 12),
                    rec_len: de::rec_len_from_disk(crate::bytes::le16(data, 16), bs),
                    name_len: data[18] as usize,
                    file_type: data[19],
                });
                Ok(v)
            }
            DirBlockKind::Leaf => de::parse_block(data, self.leaf_limit(data), bs),
        }
    }

    /// Look up `name` in directory `dir`.
    pub fn lookup(&mut self, dir: Ino, name: &[u8]) -> Result<Ino> {
        let inode = self.read_live_inode(dir)?;
        match self.find_entry(dir, &inode, name)? {
            Some(slot) => Ok(slot.ino),
            None => Err(Error::NotFound),
        }
    }

    pub fn lookup_attr(&mut self, dir: Ino, name: &[u8]) -> Result<Attr> {
        let ino = self.lookup(dir, name)?;
        self.stat(ino)
    }

    /// Resolve an absolute path (no symlink following).
    pub fn resolve(&mut self, path: &str) -> Result<Ino> {
        let mut ino = self.root();
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            ino = self.lookup(ino, comp.as_bytes())?;
        }
        Ok(ino)
    }

    // --- enumeration --------------------------------------------------------

    /// Enumerate entries starting at `cookie` (0 = beginning). `f` returns
    /// false to stop. Includes "." and "..".
    pub fn read_dir(&mut self, dir: Ino, cookie: u64, mut f: impl FnMut(DirEntryInfo) -> bool) -> Result<()> {
        let inode = self.read_live_inode(dir)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.has_flag(flags::INLINE_DATA) {
            let mut all = vec![(b".".to_vec(), dir, FileType::Directory as u8)];
            all.extend(self.inline_dir_entries(&inode)?);
            for (i, (name, ino, ft)) in all.into_iter().enumerate() {
                if (i as u64) < cookie {
                    continue;
                }
                let info = DirEntryInfo {
                    file_type: self.dirent_file_type(ino, ft)?,
                    name,
                    ino,
                    next_cookie: i as u64 + 1,
                };
                if !f(info) {
                    return Ok(());
                }
            }
            return Ok(());
        }
        if inode.has_flag(flags::INDEX)
            && self.sb.has_compat(compat::DIR_INDEX)
            && !inode.has_flag(flags::CASEFOLD)
            && !inode.has_flag(flags::ENCRYPT)
        {
            return self.dx_read_dir(dir, &inode, cookie, &mut f);
        }
        if cookie & super::htree::HASH_COOKIE != 0 {
            return Err(Error::StaleCookie);
        }
        let bs = self.bs as u64;
        let nblocks = self.dir_nblocks(&inode);
        let mut lblk = cookie / bs;
        let mut skip_to = (cookie % bs) as usize;
        while lblk < nblocks {
            let (_, data) = self.dir_block(dir, &inode, lblk)?;
            let kind = self.dir_block_kind(&inode, lblk, &data);
            let entries = self.block_entries(&data, kind)?;
            for d in entries {
                if d.offset < skip_to || d.inode == 0 {
                    continue;
                }
                let info = DirEntryInfo {
                    name: d.name(&data).to_vec(),
                    ino: d.inode,
                    file_type: self.dirent_file_type(d.inode, d.file_type)?,
                    next_cookie: lblk * bs + (d.offset + d.rec_len) as u64,
                };
                if !f(info) {
                    return Ok(());
                }
            }
            skip_to = 0;
            lblk += 1;
        }
        Ok(())
    }

    pub(crate) fn dirent_file_type(&mut self, ino: Ino, ft: u8) -> Result<FileType> {
        let t = FileType::from_dirent(ft);
        if t != FileType::Unknown {
            return Ok(t);
        }
        // no filetype feature: consult the inode
        Ok(self.read_inode(ino)?.file_type())
    }

    /// All entries of a directory (convenience).
    pub fn list_dir(&mut self, dir: Ino) -> Result<Vec<DirEntryInfo>> {
        let mut v = Vec::new();
        self.read_dir(dir, 0, |e| {
            v.push(e);
            true
        })?;
        Ok(v)
    }

    /// Whether a directory has no entries besides "." and "..".
    pub(crate) fn dir_is_empty(&mut self, ino: Ino, inode: &Inode) -> Result<bool> {
        if inode.has_flag(flags::INLINE_DATA) {
            return Ok(self.inline_dir_entries(inode)?.len() <= 1);
        }
        for lblk in 0..self.dir_nblocks(inode) {
            let (_, data) = self.dir_block(ino, inode, lblk)?;
            let kind = self.dir_block_kind(inode, lblk, &data);
            for d in self.block_entries(&data, kind)? {
                if d.inode != 0 {
                    let n = d.name(&data);
                    if n != b"." && n != b".." {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    // --- modification ---------------------------------------------------------

    /// Allocate and append one block to a directory.
    pub(crate) fn dir_append_block(&mut self, ino: Ino, inode: &mut Inode) -> Result<(u64, u64)> {
        let lblk = self.dir_nblocks(inode);
        if lblk >= u32::MAX as u64 {
            return Err(Error::NoSpace);
        }
        let extents = inode.has_flag(flags::EXTENTS);
        let goal = if extents {
            self.ext_goal(ino, inode, lblk as u32)?
        } else {
            self.ind_goal(ino, inode, lblk)?
        };
        let (pblk, _) = self.alloc_blocks(goal, 1)?;
        let mapped = if extents {
            self.ext_insert(
                ino,
                inode,
                Extent {
                    block: lblk as u32,
                    len: 1,
                    start: pblk,
                    unwritten: false,
                },
            )
        } else {
            self.ind_set(inode, lblk, pblk, pblk + 1)
        };
        if let Err(e) = mapped {
            self.free_blocks(pblk, 1)?;
            return Err(e);
        }
        let per = self.bs as u64 / 512;
        let cur = inode.sectors(self.bs, self.huge_file());
        inode.set_sectors(cur + per);
        inode.set_size(inode.size() + self.bs as u64);
        Ok((lblk, pblk))
    }

    /// Create the first block of a new directory with "." and "..".
    pub(crate) fn init_dir_blocks(&mut self, ino: Ino, inode: &mut Inode, parent: Ino) -> Result<()> {
        let (_, pblk) = self.dir_append_block(ino, inode)?;
        let bs = self.bs as usize;
        let csum = self.dir_csum();
        let mut b = vec![0u8; bs];
        let limit = if csum { bs - TAIL_SIZE } else { bs };
        let dt = self.dirent_type(FileType::Directory);
        de::write_entry(&mut b, 0, ino, 12, b".", dt, bs);
        de::write_entry(&mut b, 12, parent, limit - 12, b"..", dt, bs);
        if csum {
            de::init_tail(&mut b);
        }
        self.write_leaf_block(ino, inode, pblk, b);
        Ok(())
    }

    /// Try to place an entry into a leaf block. Returns true on success.
    fn insert_into_leaf(
        &mut self,
        ino: Ino,
        inode: &Inode,
        lblk: u64,
        name: &[u8],
        child: Ino,
        ft: u8,
    ) -> Result<bool> {
        let (pblk, mut data) = self.dir_block(ino, inode, lblk)?;
        if self.dir_block_kind(inode, lblk, &data) != DirBlockKind::Leaf {
            return Ok(false);
        }
        if self.place_entry(&mut data, name, child, ft)? {
            self.write_leaf_block(ino, inode, pblk, data);
            return Ok(true);
        }
        Ok(false)
    }

    /// Insert into a block image if there is room.
    pub(crate) fn place_entry(&self, data: &mut [u8], name: &[u8], child: Ino, ft: u8) -> Result<bool> {
        let bs = self.bs as usize;
        let need = de::rec_len_for(name.len());
        let limit = self.leaf_limit(data);
        for d in de::parse_block(data, limit, bs)? {
            if d.inode == 0 && d.rec_len >= need {
                de::write_entry(data, d.offset, child, d.rec_len, name, ft, bs);
                return Ok(true);
            }
            let used = d.used_len();
            if d.inode != 0 && d.rec_len >= used + need {
                de::set_rec_len(data, d.offset, used, bs);
                de::write_entry(data, d.offset + used, child, d.rec_len - used, name, ft, bs);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn add_entry_linear(&mut self, ino: Ino, inode: &mut Inode, name: &[u8], child: Ino, ft: u8) -> Result<()> {
        let n = self.dir_nblocks(inode);
        for lblk in 0..n {
            if self.insert_into_leaf(ino, inode, lblk, name, child, ft)? {
                return Ok(());
            }
        }
        if n == 1
            && self.sb.has_compat(compat::DIR_INDEX)
            && !inode.has_flag(flags::CASEFOLD)
            && !inode.has_flag(flags::ENCRYPT)
        {
            self.make_indexed(ino, inode)?;
            return self.dx_add_entry(ino, inode, name, child, ft);
        }
        let (_, pblk) = self.dir_append_block(ino, inode)?;
        let bs = self.bs as usize;
        let mut b = vec![0u8; bs];
        de::init_empty_block(&mut b, self.dir_csum());
        let limit = self.leaf_limit(&b);
        de::write_entry(&mut b, 0, child, limit, name, ft, bs);
        self.write_leaf_block(ino, inode, pblk, b);
        Ok(())
    }

    /// Add a directory entry. Persists `inode` changes via the caller.
    pub(crate) fn add_entry(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        name: &[u8],
        child: Ino,
        ft: FileType,
    ) -> Result<()> {
        validate_name(name)?;
        let ft = self.dirent_type(ft);
        if inode.has_flag(flags::INLINE_DATA) {
            self.uninline_dir(ino, inode)?;
        }
        if inode.has_flag(flags::INDEX) {
            if !self.sb.has_compat(compat::DIR_INDEX) {
                // index unusable: drop it (blocks stay valid linear blocks
                // only without checksums)
                return Err(Error::unsupported("htree directory without dir_index"));
            }
            return self.dx_add_entry(ino, inode, name, child, ft);
        }
        self.add_entry_linear(ino, inode, name, child, ft)
    }

    /// Remove an entry found by [`Fs::find_entry`].
    pub(crate) fn remove_slot(&mut self, ino: Ino, inode: &mut Inode, slot: &DirSlot, name: &[u8]) -> Result<()> {
        if inode.has_flag(flags::INLINE_DATA) {
            self.uninline_dir(ino, inode)?;
            let s = self.find_entry(ino, inode, name)?.ok_or(Error::NotFound)?;
            return self.remove_slot(ino, inode, &s, name);
        }
        let bs = self.bs as usize;
        let (pblk, mut data) = self.dir_block(ino, inode, slot.lblk)?;
        let rec = de::rec_len_from_disk(crate::bytes::le16(&data, slot.offset + 4), bs);
        match slot.prev {
            Some(p) => {
                let prec = de::rec_len_from_disk(crate::bytes::le16(&data, p + 4), bs);
                de::set_rec_len(&mut data, p, prec + rec, bs);
            }
            None => de::set_inode(&mut data, slot.offset, 0),
        }
        self.write_leaf_block(ino, inode, pblk, data);
        Ok(())
    }

    /// Point an existing entry at a different inode.
    pub(crate) fn retarget_slot(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        slot: &DirSlot,
        name: &[u8],
        child: Ino,
        ft: FileType,
    ) -> Result<()> {
        if inode.has_flag(flags::INLINE_DATA) {
            self.uninline_dir(ino, inode)?;
            let s = self.find_entry(ino, inode, name)?.ok_or(Error::NotFound)?;
            return self.retarget_slot(ino, inode, &s, name, child, ft);
        }
        let (pblk, mut data) = self.dir_block(ino, inode, slot.lblk)?;
        de::set_inode(&mut data, slot.offset, child);
        data[slot.offset + 7] = self.dirent_type(ft);
        let kind = self.dir_block_kind(inode, slot.lblk, &data);
        if kind == DirBlockKind::DxRoot {
            let off = de::DX_ROOT_INFO_OFF + data[de::DX_ROOT_INFO_OFF + 5] as usize;
            self.write_dx_block(ino, inode, pblk, data, off);
        } else {
            self.write_leaf_block(ino, inode, pblk, data);
        }
        Ok(())
    }

    /// Change the ".." entry of a directory.
    pub(crate) fn set_dotdot(&mut self, ino: Ino, inode: &mut Inode, parent: Ino) -> Result<()> {
        if inode.has_flag(flags::INLINE_DATA) {
            crate::bytes::set_le32(inode.block_area_mut(), 0, parent);
            return Ok(());
        }
        let slot = self
            .find_in_block(ino, inode, 0, b"..")?
            .ok_or_else(|| Error::corrupt(format!("directory {ino} has no ..")))?;
        self.retarget_slot(ino, inode, &slot, b"..", parent, FileType::Directory)
    }

    /// Update directory timestamps after a modification.
    pub(crate) fn touch_dir(inode: &mut Inode) {
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
    }
}
