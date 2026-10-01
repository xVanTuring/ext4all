//! Regular file data: read, write, truncate, inline data.

use super::extent::Mapping;
use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::extent::{Extent, MAX_INIT_LEN};
use crate::ondisk::inode::{Inode, Timestamp, flags};
use crate::ondisk::superblock::incompat;
use crate::ondisk::xattr::{self as xa, INDEX_SYSTEM};

/// Direct writes tracked per inode between mapping and completion (the
/// kernel keeps far fewer in flight; the cap only bounds lost completions).
const MAX_INFLIGHT: usize = 4096;

impl Fs {
    /// Largest file size supported for this inode's mapping scheme.
    pub(crate) fn max_file_size(&self, inode: &Inode) -> u64 {
        let bs = self.bs as u64;
        if inode.has_flag(flags::EXTENTS) {
            // logical blocks 0..2^32-2: Linux rejects an extent reaching
            // block 0xFFFFFFFF (its end would wrap in 32 bits)
            ((1u64 << 32) - 1) * bs
        } else {
            let per = bs / 4;
            let map = (12 + per + per * per + per * per * per) * bs;
            // without huge_file, i_blocks (512-byte units, data plus
            // indirect blocks) is 32 bits: indirect blocks add at most
            // 1/(per-1) on top of the data blocks
            let blocks = if self.huge_file() {
                u64::MAX
            } else {
                let total = ((1u64 << 32) - 1) / (bs / 512);
                (total * (per - 1) / per).saturating_sub(4) * bs
            };
            map.min(blocks)
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
        // On ext4 old block-mapped files are migrated to extents; on
        // ext2/ext3 they stay block mapped.
        if !inode.has_flag(flags::EXTENTS) && self.sb.has_incompat(incompat::EXTENTS) {
            self.convert_to_extents(ino, inode)?;
        }
        Ok(None)
    }

    /// Write `data` at `offset`, growing the file as needed.
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
        self.op(|fs| fs.write_impl(ino, offset, data))
    }

    fn write_impl(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
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
        if size % bs == 0 || inode.has_flag(flags::INLINE_DATA) {
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

    /// Core write path (extent or block mapped, not inline). Does not touch
    /// the size or timestamps.
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
                    let extents = inode.has_flag(flags::EXTENTS);
                    let want_bytes = in_blk + (len - done) as u64;
                    let want = want_bytes.div_ceil(bs).min(hole).min(MAX_INIT_LEN as u64) as u32;
                    let goal = if extents {
                        self.ext_goal(ino, inode, lblk as u32)?
                    } else {
                        self.ind_goal(ino, inode, lblk)?
                    };
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
                    let per = bs as i64 / 512;
                    if extents {
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
                    } else {
                        for i in 0..got as u64 {
                            if let Err(e) = self.ind_set(inode, lblk + i, start + i, start + got as u64) {
                                // blocks already mapped stay (and count as
                                // written); release the rest
                                self.free_blocks(start + i, got as u64 - i)?;
                                let cur = inode.sectors(self.bs, self.huge_file()) as i64;
                                inode.set_sectors((cur + i as i64 * per) as u64);
                                done += ((i * bs).saturating_sub(in_blk) as usize).min(avail);
                                return if done > 0 { Ok(done) } else { Err(e) };
                            }
                        }
                    }
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
            self.dio_inflight_done(ino, new_size, u64::MAX);
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
        if !inode.has_flag(flags::EXTENTS) {
            return self.ind_free_range(inode, from, to);
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
            self.ind_free_range(inode, 0, u64::MAX)?;
            inode.block_area_mut().fill(0);
        }
        Ok(())
    }

    /// Allocate (unwritten) blocks for `[offset, offset+len)` without
    /// changing the size unless `keep_size` is false.
    pub fn fallocate(&mut self, ino: Ino, offset: u64, len: u64, keep_size: bool) -> Result<()> {
        self.op(|fs| fs.fallocate_impl(ino, offset, len, keep_size))
    }

    fn fallocate_impl(&mut self, ino: Ino, offset: u64, len: u64, keep_size: bool) -> Result<()> {
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
        if !inode.has_flag(flags::EXTENTS) {
            // block maps cannot express unwritten blocks (like Linux)
            return Err(Error::unsupported("fallocate on a block-mapped file"));
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
        self.op(|fs| fs.punch_hole_impl(ino, offset, len))
    }

    fn punch_hole_impl(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()> {
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
        self.dio_inflight_done(ino, offset, offset.saturating_add(len));
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

    /// Map `[offset, offset+len)` for direct I/O by the kernel.
    ///
    /// Reads get data extents and zero-fill extents (holes, unwritten
    /// blocks, beyond EOF). Writes allocate missing blocks as *unwritten*
    /// extents and return every block as a data extent; the caller must
    /// report completion with [`Fs::complete_direct_write`], which converts
    /// them and grows the file. Until then (and after a crash) the new
    /// blocks read as zeros, so no stale data is ever exposed.
    pub fn map_for_io(&mut self, ino: Ino, offset: u64, len: u64, write: bool) -> Result<Vec<super::IoExtent>> {
        if write {
            self.op(|fs| fs.map_for_io_impl(ino, offset, len, true))
        } else {
            self.map_for_io_impl(ino, offset, len, false)
        }
    }

    fn map_for_io_impl(&mut self, ino: Ino, offset: u64, len: u64, write: bool) -> Result<Vec<super::IoExtent>> {
        use super::IoExtent;
        if write {
            self.require_rw()?;
        }
        let mut inode = self.read_live_inode(ino)?;
        if !inode.is_reg() {
            return Err(Error::invalid("direct I/O on a non-regular file"));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset.checked_add(len).ok_or(Error::TooBig)?;
        if write {
            if inode.has_flag(flags::IMMUTABLE) || inode.has_flag(flags::APPEND) && offset < inode.size() {
                return Err(Error::NotPermitted);
            }
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
            if !inode.has_flag(flags::EXTENTS) {
                return Err(Error::unsupported("direct I/O on a block-mapped file"));
            }
        } else if inode.has_flag(flags::INLINE_DATA) {
            return Err(Error::unsupported("direct I/O on inline data"));
        }
        let bs = self.bs as u64;
        let size = inode.size();
        let mut out: Vec<IoExtent> = Vec::new();
        let mut push = |e: IoExtent| {
            if let Some(last) = out.last_mut()
                && last.zero_fill == e.zero_fill
                && last.logical + last.length == e.logical
                && (e.zero_fill || last.physical + last.length == e.physical)
            {
                last.length += e.length;
            } else {
                out.push(e);
            }
        };
        let first = offset / bs;
        let last_blk = end.div_ceil(bs);
        // Blocks only partly covered by the write that hold no data yet
        // (new or unwritten): the kernel writes just its part, and the
        // completion marks the whole block written, so the rest must not
        // be stale device contents.
        let mut partial = Vec::with_capacity(2);
        if offset % bs != 0 {
            partial.push(first);
        }
        if end % bs != 0 && !partial.contains(&(last_blk - 1)) {
            partial.push(last_blk - 1);
        }
        // (logical, physical) blocks to zero before the kernel writes
        let mut zero: Vec<(u64, u64)> = Vec::new();
        // The tail of a written last block past the end of file is left
        // alone even when this write extends the file: the kernel writes
        // through mappings it obtained earlier without asking again, so it
        // may be filling that tail right now as part of the same request.
        // It zero-fills the gap of an extending write itself, and a failed
        // write's leftovers are cleared in `abort_direct_write`.
        let mut lblk = first;
        let mut allocated = false;
        while lblk < last_blk {
            let pos = lblk * bs;
            if !write && pos >= size {
                push(IoExtent {
                    logical: pos,
                    physical: 0,
                    length: (last_blk - lblk) * bs,
                    zero_fill: true,
                });
                break;
            }
            match self.map_block(ino, &inode, lblk)? {
                Mapping::Mapped {
                    pblk,
                    len: run,
                    unwritten,
                } => {
                    let n = run.min(last_blk - lblk);
                    if write && unwritten {
                        zero.extend(
                            partial
                                .iter()
                                .filter(|&&b| b >= lblk && b < lblk + n)
                                .map(|&b| (b, pblk + (b - lblk))),
                        );
                    }
                    push(IoExtent {
                        logical: pos,
                        physical: pblk * bs,
                        length: n * bs,
                        zero_fill: unwritten && !write,
                    });
                    lblk += n;
                }
                Mapping::Hole { len: hole } => {
                    let n = hole.min(last_blk - lblk);
                    if !write {
                        push(IoExtent {
                            logical: pos,
                            physical: 0,
                            length: n * bs,
                            zero_fill: true,
                        });
                        lblk += n;
                        continue;
                    }
                    let want = n.min(32767) as u32;
                    let goal = self.ext_goal(ino, &inode, lblk as u32)?;
                    let (start, got) = self.alloc_blocks(goal, want)?;
                    // new blocks are exposed to the kernel for writing only
                    // after the unwritten mapping is in place
                    self.ext_insert(
                        ino,
                        &mut inode,
                        crate::ondisk::extent::Extent {
                            block: lblk as u32,
                            len: got,
                            start,
                            unwritten: true,
                        },
                    )?;
                    let per = bs as i64 / 512;
                    let cur = inode.sectors(self.bs, self.huge_file()) as i64;
                    inode.set_sectors((cur + got as i64 * per) as u64);
                    allocated = true;
                    let n = got as u64;
                    zero.extend(
                        partial
                            .iter()
                            .filter(|&&b| b >= lblk && b < lblk + n)
                            .map(|&b| (b, start + (b - lblk))),
                    );
                    push(IoExtent {
                        logical: pos,
                        physical: start * bs,
                        length: n * bs,
                        zero_fill: false,
                    });
                    lblk += n;
                }
            }
        }
        // A block that an earlier, still running direct write touches was
        // zeroed by that write's mapping or is being written whole: zeroing
        // it again could erase data that write already put there.
        zero.retain(|&(l, _)| !self.dio_inflight_overlaps(ino, l * bs, (l + 1) * bs));
        if !zero.is_empty() {
            let zeros = vec![0u8; bs as usize];
            for (_, pblk) in zero {
                self.dev.write_at(pblk * bs, &zeros)?;
            }
        }
        if allocated {
            self.write_inode(ino, &inode)?;
            self.maybe_commit()?;
        }
        if write {
            let v = self.dio_inflight.entry(ino).or_default();
            if v.len() >= MAX_INFLIGHT {
                // completions went missing; forget the oldest
                v.remove(0);
            }
            v.push((offset, end));
        }
        Ok(out)
    }

    /// Whether a direct write of `ino` still in flight overlaps
    /// `[start, end)`.
    fn dio_inflight_overlaps(&self, ino: Ino, start: u64, end: u64) -> bool {
        self.dio_inflight
            .get(&ino)
            .is_some_and(|v| v.iter().any(|&(s, e)| s < end && start < e))
    }

    /// `[start, end)` of `ino` is no longer being written directly
    /// (completed, failed, truncated or punched out).
    pub(crate) fn dio_inflight_done(&mut self, ino: Ino, start: u64, end: u64) {
        let Some(v) = self.dio_inflight.get_mut(&ino) else {
            return;
        };
        let mut rest = Vec::with_capacity(v.len());
        for &(s, e) in v.iter() {
            if e <= start || s >= end {
                rest.push((s, e));
                continue;
            }
            if s < start {
                rest.push((s, start));
            }
            if e > end {
                rest.push((end, e));
            }
        }
        if rest.is_empty() {
            self.dio_inflight.remove(&ino);
        } else {
            *v = rest;
        }
    }

    /// The kernel reports that a direct write of `[offset, offset+len)`
    /// failed: nothing becomes visible. New blocks stay unwritten; if the
    /// write reached past the end of file into the written last block,
    /// whatever it left there is zeroed, so a later extension of the file
    /// shows zeros.
    pub fn abort_direct_write(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()> {
        let end = offset.saturating_add(len);
        self.dio_inflight_done(ino, offset, end);
        if self.read_only {
            return Ok(());
        }
        let inode = self.read_live_inode(ino)?;
        let size = inode.size();
        let bs = self.bs as u64;
        if end > size
            && offset < size.div_ceil(bs) * bs
            && !self.dio_inflight_overlaps(ino, size, size.div_ceil(bs) * bs)
        {
            self.zero_tail(ino, &inode, size)?;
        }
        Ok(())
    }

    /// The kernel finished writing `[offset, offset+len)` directly to the
    /// blocks returned by [`Fs::map_for_io`]: mark them written and grow
    /// the file.
    pub fn complete_direct_write(&mut self, ino: Ino, offset: u64, len: u64) -> Result<()> {
        self.dio_inflight_done(ino, offset, offset.saturating_add(len));
        self.op(|fs| {
            fs.require_rw()?;
            let mut inode = fs.read_live_inode(ino)?;
            if !inode.is_reg() || !inode.has_flag(flags::EXTENTS) {
                return Err(Error::invalid("direct write completion on an unsupported file"));
            }
            if len == 0 {
                return Ok(());
            }
            let end = offset.checked_add(len).ok_or(Error::TooBig)?;
            let bs = fs.bs as u64;
            let first = offset / bs;
            let last = end.div_ceil(bs);
            // only whole blocks the kernel wrote can become "written"; a
            // partial block at either edge was read-modify-written by the
            // kernel through its page cache, so it is complete as well
            fs.ext_mark_written(ino, &mut inode, first as u32, (last - first) as u32)?;
            if end > inode.size() {
                inode.set_size(end);
            }
            let now = Timestamp::now();
            inode.set_mtime(now);
            inode.set_ctime(now);
            fs.write_inode(ino, &inode)?;
            fs.maybe_commit()
        })
    }

    /// `lseek(SEEK_DATA)` (`data = true`) or `lseek(SEEK_HOLE)`.
    /// Unwritten (preallocated) blocks count as holes. The end of file is
    /// an implicit hole.
    pub fn seek_data_hole(&mut self, ino: Ino, offset: u64, data: bool) -> Result<u64> {
        let inode = self.read_live_inode(ino)?;
        let size = inode.size();
        if offset >= size {
            return Err(Error::NoSuchOffset);
        }
        if inode.has_flag(flags::INLINE_DATA) {
            return Ok(if data { offset } else { size });
        }
        let bs = self.bs as u64;
        let mut lblk = offset / bs;
        loop {
            let pos = (lblk * bs).max(offset);
            if pos >= size {
                return if data { Err(Error::NoSuchOffset) } else { Ok(size) };
            }
            let (is_data, run) = match self.map_block(ino, &inode, lblk)? {
                Mapping::Mapped { len, unwritten, .. } => (!unwritten, len),
                Mapping::Hole { len } => (false, len),
            };
            if is_data == data {
                return Ok(pos);
            }
            lblk = lblk.saturating_add(run.max(1));
        }
    }
}
