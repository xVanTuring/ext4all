//! Regular file data: read, write, truncate, inline data.

use super::extent::Mapping;
use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::extent::{Extent, MAX_INIT_LEN};
use crate::ondisk::inode::{Inode, Timestamp, flags};
use crate::ondisk::superblock::incompat;
use crate::ondisk::xattr::{self as xa, INDEX_SYSTEM};

impl Fs {
    /// Largest file size supported for this inode's mapping scheme.
    pub(crate) fn max_file_size(&self, inode: &Inode) -> u64 {
        let bs = self.bs as u64;
        if inode.has_flag(flags::EXTENTS) {
            // 2^32 logical blocks
            (1u64 << 32) * bs - 1
        } else {
            let per = bs / 4;
            (12 + per + per * per + per * per * per) * bs
        }
    }

    /// Read file data at `offset` into `buf`. Returns bytes read (0 at EOF).
    pub fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.read_live_inode(ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }
        self.read_inode_data(ino, &inode, offset, buf)
    }

    /// Read data of any inode type (used for files, symlinks, EA inodes).
    pub(crate) fn read_inode_data(&mut self, ino: Ino, inode: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let size = inode.size();
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = (size - offset).min(buf.len() as u64) as usize;
        let buf = &mut buf[..len];
        if inode.has_flag(flags::INLINE_DATA) {
            let data = self.inline_data(inode)?;
            let o = offset as usize;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = data.get(o + i).copied().unwrap_or(0);
            }
            return Ok(len);
        }
        let bs = self.bs as u64;
        let mut done = 0usize;
        while done < len {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_blk = pos % bs;
            match self.map_block(ino, inode, lblk)? {
                Mapping::Mapped {
                    pblk,
                    len: run,
                    unwritten,
                } => {
                    let avail = (run * bs - in_blk).min((len - done) as u64) as usize;
                    let dst = &mut buf[done..done + avail];
                    if unwritten {
                        dst.fill(0);
                    } else {
                        if pblk + run > self.sb.blocks_count() {
                            return Err(Error::corrupt(format!("inode {ino}: block {pblk} beyond device")));
                        }
                        self.dev.read_at(pblk * bs + in_blk, dst)?;
                    }
                    done += avail;
                }
                Mapping::Hole { len: run } => {
                    let avail = run.saturating_mul(bs).saturating_sub(in_blk).min((len - done) as u64) as usize;
                    buf[done..done + avail].fill(0);
                    done += avail;
                }
            }
        }
        Ok(len)
    }

    /// Inline data: `i_block` followed by the `system.data` xattr value.
    pub(crate) fn inline_data(&self, inode: &Inode) -> Result<Vec<u8>> {
        let mut data = inode.block_area().to_vec();
        if let Some(r) = inode.xattr_area() {
            let entries = xa::parse_ibody(&inode.raw[r])?;
            if let Some(e) = entries.iter().find(|e| e.index == INDEX_SYSTEM && e.name == b"data") {
                data.extend_from_slice(&e.value);
            }
        }
        data.truncate(inode.size() as usize);
        Ok(data)
    }

    /// Turn an inline-data file into an (empty) extent-mapped one and
    /// return the old contents.
    pub(crate) fn uninline(&mut self, _ino: Ino, inode: &mut Inode) -> Result<Vec<u8>> {
        let data = self.inline_data(inode)?;
        if let Some(r) = inode.xattr_area() {
            let mut entries = xa::parse_ibody(&inode.raw[r.clone()])?;
            entries.retain(|e| !(e.index == INDEX_SYSTEM && e.name == b"data"));
            xa::write_ibody(&mut inode.raw[r], &entries)?;
        }
        inode.set_flag(flags::INLINE_DATA, false);
        self.to_extent_mapped(inode);
        Ok(data)
    }

    pub(crate) fn to_extent_mapped(&self, inode: &mut Inode) {
        inode.set_flag(flags::EXTENTS, true);
        Self::ext_init_root(inode);
    }

    /// Convert a classic block-mapped inode to extents in place.
    pub(crate) fn convert_to_extents(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        if !self.sb.has_incompat(incompat::EXTENTS) {
            return Err(Error::unsupported(
                "writing block-mapped files without the extent feature",
            ));
        }
        let extents = self.all_extents(ino, inode)?;
        let meta = self.ind_meta_blocks(inode)?;
        for b in &meta {
            self.free_blocks(*b, 1)?;
        }
        let per = self.bs as i64 / 512;
        let cur = inode.sectors(self.bs, self.huge_file()) as i64;
        inode.set_sectors((cur - meta.len() as i64 * per).max(0) as u64);
        self.to_extent_mapped(inode);
        for e in extents {
            self.ext_insert(ino, inode, e)?;
        }
        Ok(())
    }

    /// Prepare an inode for data modification (inline / block map).
    pub(crate) fn prepare_for_write(&mut self, ino: Ino, inode: &mut Inode) -> Result<Option<Vec<u8>>> {
        if inode.has_flag(flags::INLINE_DATA) {
            return Ok(Some(self.uninline(ino, inode)?));
        }
        if !inode.has_flag(flags::EXTENTS) {
            self.convert_to_extents(ino, inode)?;
        }
        Ok(None)
    }

    /// Write `data` at `offset`, growing the file as needed.
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
        self.require_rw()?;
        let mut inode = self.read_live_inode(ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }
        if !inode.is_reg() {
            return Err(Error::invalid("write to non-regular file"));
        }
        if inode.has_flag(flags::IMMUTABLE) {
            return Err(Error::NotPermitted);
        }
        if inode.has_flag(flags::APPEND) && offset != inode.size() {
            return Err(Error::NotPermitted);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset.checked_add(data.len() as u64).ok_or(Error::TooBig)?;
        if end > self.max_file_size(&inode) {
            return Err(Error::TooBig);
        }
        self.ensure_space(data.len() as u64 / self.bs as u64 + 8)?;
        if let Some(old) = self.prepare_for_write(ino, &mut inode)? {
            let old_size = inode.size();
            if !old.is_empty() {
                inode.set_size(0);
                self.write_data(ino, &mut inode, 0, &old)?;
            }
            inode.set_size(old_size);
        }
        let old_size = inode.size();
        if offset > old_size {
            self.zero_tail(ino, &inode, old_size)?;
        }
        let res = self.write_data(ino, &mut inode, offset, data);
        // even on partial failure (ENOSPC) keep what was written
        let written = match &res {
            Ok(n) => *n,
            Err(_) => 0,
        };
        let new_end = offset + written as u64;
        if new_end > inode.size() {
            inode.set_size(new_end);
        }
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        // writing clears setuid/setgid like Linux file_remove_privs
        let mode = inode.mode();
        if mode & 0o6000 != 0 {
            let clear = if mode & 0o010 != 0 { 0o6000 } else { 0o4000 };
            inode.set_mode(mode & !clear);
        }
        self.write_inode(ino, &inode)?;
        let n = res?;
        self.maybe_commit()?;
        Ok(n)
    }

    /// Zero bytes `[size, end of block)` of the block containing `size`, so
    /// extending the file never exposes stale data.
    pub(crate) fn zero_tail(&mut self, ino: Ino, inode: &Inode, size: u64) -> Result<()> {
        let bs = self.bs as u64;
        if size % bs == 0 || !inode.has_flag(flags::EXTENTS) {
            return Ok(());
        }
        if let Mapping::Mapped {
            pblk, unwritten: false, ..
        } = self.map_block(ino, inode, size / bs)?
        {
            let off = size % bs;
            let zeros = vec![0u8; (bs - off) as usize];
            self.dev.write_at(pblk * bs + off, &zeros)?;
        }
        Ok(())
    }

    /// Core write path; `inode` must be extent mapped. Does not touch the
    /// size or timestamps.
    pub(crate) fn write_data(&mut self, ino: Ino, inode: &mut Inode, offset: u64, data: &[u8]) -> Result<usize> {
        let bs = self.bs as u64;
        let mut done = 0usize;
        let len = data.len();
        while done < len {
            let pos = offset + done as u64;
            let lblk = pos / bs;
            let in_blk = pos % bs;
            let map = self.map_block(ino, inode, lblk)?;
            match map {
                Mapping::Mapped {
                    pblk,
                    len: run,
                    unwritten,
                } => {
                    let avail = (run * bs - in_blk).min((len - done) as u64) as usize;
                    let chunk = &data[done..done + avail];
                    if unwritten {
                        // unwritten blocks read as zero: pad partial blocks
                        let nblocks = (in_blk + avail as u64).div_ceil(bs);
                        let mut img = vec![0u8; (nblocks * bs) as usize];
                        img[in_blk as usize..in_blk as usize + avail].copy_from_slice(chunk);
                        self.dev.write_at(pblk * bs, &img)?;
                        self.ext_mark_written(ino, inode, lblk as u32, nblocks as u32)?;
                    } else {
                        self.write_blocks_partial(pblk, in_blk, chunk)?;
                    }
                    done += avail;
                }
                Mapping::Hole { len: hole } => {
                    let want_bytes = in_blk + (len - done) as u64;
                    let want = want_bytes.div_ceil(bs).min(hole).min(MAX_INIT_LEN as u64) as u32;
                    let goal = self.ext_goal(ino, inode, lblk as u32)?;
                    let (start, got) = match self.alloc_blocks(goal, want) {
                        Ok(r) => r,
                        Err(e) => {
                            if done > 0 {
                                return Ok(done);
                            }
                            return Err(e);
                        }
                    };
                    let avail = (got as u64 * bs - in_blk).min((len - done) as u64) as usize;
                    let mut img = vec![0u8; got as usize * bs as usize];
                    img[in_blk as usize..in_blk as usize + avail].copy_from_slice(&data[done..done + avail]);
                    self.dev.write_at(start * bs, &img)?;
                    let ins = self.ext_insert(
                        ino,
                        inode,
                        Extent {
                            block: lblk as u32,
                            len: got,
                            start,
                            unwritten: false,
                        },
                    );
                    if let Err(e) = ins {
                        self.free_blocks(start, got as u64)?;
                        return Err(e);
                    }
                    let per = bs as i64 / 512;
                    let cur = inode.sectors(self.bs, self.huge_file()) as i64;
                    inode.set_sectors((cur + got as i64 * per) as u64);
                    done += avail;
                }
            }
        }
        Ok(done)
    }

    /// Write `chunk` starting `in_blk` bytes into physical block `pblk`
    /// (spanning following contiguous blocks), preserving partial blocks.
    fn write_blocks_partial(&mut self, pblk: u64, in_blk: u64, chunk: &[u8]) -> Result<()> {
        let bs = self.bs as u64;
        let start = pblk * bs + in_blk;
        let end = start + chunk.len() as u64;
        let head_aligned = start % bs == 0;
        let tail_aligned = end % bs == 0;
        if head_aligned && tail_aligned {
            return self.dev.write_at(start, chunk);
        }
        // read-modify-write the partial head/tail blocks
        let first = start / bs;
        let last = (end - 1) / bs;
        let mut img = vec![0u8; ((last - first + 1) * bs) as usize];
        if !head_aligned {
            self.dev.read_at(first * bs, &mut img[..bs as usize])?;
        }
        if !tail_aligned && (last != first || head_aligned) {
            let o = ((last - first) * bs) as usize;
            self.dev.read_at(last * bs, &mut img[o..o + bs as usize])?;
        }
        let o = (start - first * bs) as usize;
        img[o..o + chunk.len()].copy_from_slice(chunk);
        self.dev.write_at(first * bs, &img)
    }

    /// Change the file size, freeing or (sparsely) extending.
    pub(crate) fn set_size(&mut self, ino: Ino, inode: &mut Inode, new_size: u64) -> Result<()> {
        if new_size > self.max_file_size(inode) {
            return Err(Error::TooBig);
        }
        let old = inode.size();
        if new_size == old {
            return Ok(());
        }
        if let Some(data) = self.prepare_for_write(ino, inode)? {
            inode.set_size(0);
            let keep = &data[..data.len().min(new_size as usize)];
            if !keep.is_empty() {
                self.write_data(ino, inode, 0, keep)?;
            }
            inode.set_size(new_size);
            return Ok(());
        }
        let bs = self.bs as u64;
        if new_size < old {
            let first_free = new_size.div_ceil(bs);
            self.free_range(ino, inode, first_free, 1 << 32)?;
            self.zero_tail(ino, inode, new_size)?;
        } else {
            self.zero_tail(ino, inode, old)?;
        }
        inode.set_size(new_size);
        Ok(())
    }

    /// Unmap and free logical blocks `[from, to)`.
    pub(crate) fn free_range(&mut self, ino: Ino, inode: &mut Inode, from: u64, to: u64) -> Result<()> {
        if from >= to || from >= 1 << 32 {
            return Ok(());
        }
        let removed = self.ext_remove_range(ino, inode, from as u32, to)?;
        let per = self.bs as i64 / 512;
        for (start, n) in removed {
            self.free_blocks(start, n)?;
            let cur = inode.sectors(self.bs, self.huge_file()) as i64;
            inode.set_sectors((cur - n as i64 * per).max(0) as u64);
        }
        Ok(())
    }

    /// Release every data and mapping block of an inode (for deletion).
    pub(crate) fn free_all_blocks(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        if inode.has_flag(flags::INLINE_DATA) {
            inode.set_flag(flags::INLINE_DATA, false);
            inode.block_area_mut().fill(0);
            return Ok(());
        }
        if inode.is_symlink() && !inode.has_flag(flags::EXTENTS) && self.is_fast_symlink(inode) {
            inode.block_area_mut().fill(0);
            return Ok(());
        }
        if matches!(
            inode.file_type(),
            crate::ondisk::inode::FileType::CharDev
                | crate::ondisk::inode::FileType::BlockDev
                | crate::ondisk::inode::FileType::Fifo
                | crate::ondisk::inode::FileType::Socket
        ) {
            return Ok(());
        }
        if inode.has_flag(flags::EXTENTS) {
            self.free_range(ino, inode, 0, 1 << 32)?;
        } else {
            let extents = self.all_extents(ino, inode)?;
            for e in extents {
                self.free_blocks(e.start, e.len as u64)?;
            }
            for b in self.ind_meta_blocks(inode)? {
                self.free_blocks(b, 1)?;
            }
            inode.block_area_mut().fill(0);
        }
        Ok(())
    }

    /// Allocate (unwritten) blocks for `[offset, offset+len)` without
    /// changing the size unless `keep_size` is false.
    pub fn fallocate(&mut self, ino: Ino, offset: u64, len: u64, keep_size: bool) -> Result<()> {
        self.require_rw()?;
        let mut inode = self.read_live_inode(ino)?;
        if !inode.is_reg() {
            return Err(Error::invalid("fallocate on non-regular file"));
        }
        let end = offset.checked_add(len).ok_or(Error::TooBig)?;
        if end > self.max_file_size(&inode) {
            return Err(Error::TooBig);
        }
        if let Some(old) = self.prepare_for_write(ino, &mut inode)? {
            let sz = inode.size();
            inode.set_size(0);
            if !old.is_empty() {
                self.write_data(ino, &mut inode, 0, &old)?;
            }
            inode.set_size(sz);
        }
        let bs = self.bs as u64;
        let mut lblk = offset / bs;
        let last = end.div_ceil(bs);
        while lblk < last {
            match self.map_block(ino, &inode, lblk)? {
                Mapping::Mapped { len, .. } => lblk += len,
                Mapping::Hole { len: hole } => {
                    let want = (last - lblk).min(hole).min(32767) as u32;
                    let goal = self.ext_goal(ino, &inode, lblk as u32)?;
                    let (start, got) = self.alloc_blocks(goal, want)?;
                    self.ext_insert(
                        ino,
                        &mut inode,
                        Extent {
                            block: lblk as u32,
                            len: got,
                            start,
                            unwritten: true,
                        },
                    )?;
                    let per = bs as i64 / 512;
                    let cur = inode.sectors(self.bs, self.huge_file()) as i64;
                    inode.set_sectors((cur + got as i64 * per) as u64);
                    lblk += got as u64;
                }
            }
        }
        if !keep_size && end > inode.size() {
            inode.set_size(end);
        }
        let now = Timestamp::now();
        inode.set_ctime(now);
        if !keep_size {
            inode.set_mtime(now);
        }
        self.write_inode(ino, &inode)?;
        self.maybe_commit()
    }

    /// Deallocate `[offset, offset+len)` (keeps the size).
    pub fn punch_hole(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()> {
        self.require_rw()?;
        let mut inode = self.read_live_inode(ino)?;
        if !inode.is_reg() {
            return Err(Error::invalid("punch_hole on non-regular file"));
        }
        if let Some(old) = self.prepare_for_write(ino, &mut inode)? {
            let sz = inode.size();
            inode.set_size(0);
            if !old.is_empty() {
                self.write_data(ino, &mut inode, 0, &old)?;
            }
            inode.set_size(sz);
        }
        let bs = self.bs as u64;
        let end = offset.saturating_add(len).min(inode.size().max(offset));
        if end <= offset {
            return Ok(());
        }
        // zero partial blocks at the edges, free whole blocks in between
        let first_full = offset.div_ceil(bs);
        let last_full = end / bs;
        let zero_range = |fs: &mut Fs, inode: &Inode, s: u64, e: u64| -> Result<()> {
            if s >= e {
                return Ok(());
            }
            if let Mapping::Mapped {
                pblk, unwritten: false, ..
            } = fs.map_block(ino, inode, s / bs)?
            {
                let zeros = vec![0u8; (e - s) as usize];
                fs.dev.write_at(pblk * bs + s % bs, &zeros)?;
            }
            Ok(())
        };
        if first_full > last_full {
            zero_range(self, &inode, offset, end)?;
        } else {
            zero_range(self, &inode, offset, first_full * bs)?;
            zero_range(self, &inode, last_full * bs, end)?;
            self.free_range(ino, &mut inode, first_full, last_full)?;
        }
        let now = Timestamp::now();
        inode.set_mtime(now);
        inode.set_ctime(now);
        self.write_inode(ino, &inode)?;
        self.maybe_commit()
    }

    /// Mapped extents of a file (for tools / FIEMAP-like queries).
    pub fn file_extents(&mut self, ino: Ino) -> Result<Vec<Extent>> {
        let inode = self.read_live_inode(ino)?;
        if inode.has_flag(flags::INLINE_DATA) {
            return Ok(Vec::new());
        }
        self.all_extents(ino, &inode)
    }
}
