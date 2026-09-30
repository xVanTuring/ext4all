//! Namespace operations: create, mkdir, symlink, link, unlink, rmdir,
//! rename, set_attr, read_link.

use super::dir::validate_name;
use super::{Attr, Fs, Ino, RenameFlags, SetAttr};
use crate::error::{Error, Result};
use crate::ondisk::extent::Extent;
use crate::ondisk::inode::{FileType, Inode, LINK_MAX, ROOT_INO, Timestamp, flags, mode};
use crate::ondisk::superblock::{incompat, ro_compat};

/// Flags a user may change with `set_attr` (FS_FL_USER_MODIFIABLE subset).
const USER_MODIFIABLE: u32 = flags::SECRM
    | flags::UNRM
    | flags::COMPR
    | flags::SYNC
    | flags::IMMUTABLE
    | flags::APPEND
    | flags::NODUMP
    | flags::NOATIME
    | flags::JOURNAL_DATA
    | flags::NOTAIL
    | flags::DIRSYNC
    | flags::TOPDIR
    | flags::PROJINHERIT;

fn reject_dots(name: &[u8]) -> Result<()> {
    validate_name(name)?;
    if name == b"." || name == b".." {
        return Err(Error::invalid("'.' and '..' are reserved"));
    }
    Ok(())
}

impl Fs {
    /// Keep unlinked inodes on the orphan list until [`Fs::reclaim`]
    /// (needed when the kernel may still hold references).
    pub fn set_defer_unlinked(&mut self, on: bool) {
        self.defer_unlinked = on;
    }

    fn dir_nlink(&self) -> bool {
        self.sb.has_ro_compat(ro_compat::DIR_NLINK)
    }

    /// Increment a directory's link count (dir_nlink overflow → 1).
    fn inc_dir_links(&self, inode: &mut Inode) -> Result<()> {
        let n = inode.links_count();
        if n == 1 && self.dir_nlink() {
            return Ok(());
        }
        if n >= LINK_MAX {
            if self.dir_nlink() {
                inode.set_links_count(1);
                return Ok(());
            }
            return Err(Error::TooManyLinks);
        }
        inode.set_links_count(n + 1);
        Ok(())
    }

    fn dec_dir_links(inode: &mut Inode) {
        let n = inode.links_count();
        if n > 2 {
            inode.set_links_count(n - 1);
        }
    }

    fn can_add_subdir(&self, parent: &Inode) -> Result<()> {
        if parent.links_count() >= LINK_MAX && !self.dir_nlink() {
            return Err(Error::TooManyLinks);
        }
        Ok(())
    }

    fn parent_dir(&mut self, dir: Ino) -> Result<Inode> {
        let inode = self.read_live_inode(dir)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.links_count() == 0 {
            // directory was removed while still referenced
            return Err(Error::NotFound);
        }
        Ok(inode)
    }

    /// Build and store a fresh inode.
    #[allow(clippy::too_many_arguments)]
    fn new_inode(
        &mut self,
        parent: Ino,
        parent_inode: &Inode,
        ft: FileType,
        perm: u16,
        uid: u32,
        gid: u32,
    ) -> Result<(Ino, Inode)> {
        let is_dir = ft == FileType::Directory;
        let ino = self.alloc_inode(parent, is_dir)?;
        let isz = self.sb.inode_size() as usize;
        let mut inode = Inode::zeroed(isz);
        if isz > 128 {
            let want = self.sb.want_extra_isize().max(self.sb.min_extra_isize()).max(32);
            inode.set_extra_isize(want.min((isz - 128) as u16));
        }
        let mut perm = perm & 0o7777;
        let mut gid = gid;
        if parent_inode.mode() & mode::S_ISGID != 0 {
            gid = parent_inode.gid();
            if is_dir {
                perm |= mode::S_ISGID;
            }
        }
        inode.set_mode(ft.mode_bits() | perm);
        inode.set_uid(uid);
        inode.set_gid(gid);
        inode.set_links_count(if is_dir { 2 } else { 1 });
        let now = Timestamp::now();
        inode.set_atime(now);
        inode.set_mtime(now);
        inode.set_ctime(now);
        inode.set_crtime(now);
        let inherited = parent_inode.flags() & flags::INHERITED;
        let mut fl = match ft {
            FileType::Directory => inherited,
            FileType::Regular => inherited & !(flags::DIRSYNC | flags::TOPDIR | flags::CASEFOLD | flags::PROJINHERIT),
            _ => inherited & (flags::NODUMP | flags::NOATIME),
        };
        if matches!(ft, FileType::Directory | FileType::Regular | FileType::Symlink)
            && self.sb.has_incompat(incompat::EXTENTS)
        {
            fl |= flags::EXTENTS;
        }
        inode.set_flags(fl);
        if fl & flags::EXTENTS != 0 {
            Self::ext_init_root(&mut inode);
        }
        inode.set_generation(self.next_generation());
        if parent_inode.has_flag(flags::PROJINHERIT) {
            inode.set_projid(parent_inode.projid());
        }
        Ok((ino, inode))
    }

    fn check_new_name(&mut self, dir: Ino, dinode: &Inode, name: &[u8]) -> Result<()> {
        reject_dots(name)?;
        if self.find_entry(dir, dinode, name)?.is_some() {
            return Err(Error::Exists);
        }
        Ok(())
    }

    /// Create a regular file, device node, FIFO or socket.
    pub fn create(
        &mut self,
        dir: Ino,
        name: &[u8],
        ft: FileType,
        perm: u16,
        uid: u32,
        gid: u32,
        rdev: u32,
    ) -> Result<Attr> {
        self.require_rw()?;
        if matches!(ft, FileType::Directory | FileType::Symlink | FileType::Unknown) {
            return Err(Error::invalid("use mkdir/symlink"));
        }
        let mut dinode = self.parent_dir(dir)?;
        self.check_new_name(dir, &dinode, name)?;
        let (ino, mut inode) = self.new_inode(dir, &dinode, ft, perm, uid, gid)?;
        if matches!(ft, FileType::CharDev | FileType::BlockDev) {
            inode.set_rdev(rdev);
        }
        self.write_inode(ino, &inode)?;
        if let Err(e) = self.add_entry(dir, &mut dinode, name, ino, ft) {
            self.free_inode(ino, false)?;
            return Err(e);
        }
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.maybe_commit()?;
        Ok(self.inode_attr(ino, &inode))
    }

    pub fn mkdir(&mut self, dir: Ino, name: &[u8], perm: u16, uid: u32, gid: u32) -> Result<Attr> {
        self.require_rw()?;
        let mut dinode = self.parent_dir(dir)?;
        self.check_new_name(dir, &dinode, name)?;
        self.can_add_subdir(&dinode)?;
        self.ensure_space(4)?;
        let (ino, mut inode) = self.new_inode(dir, &dinode, FileType::Directory, perm, uid, gid)?;
        if let Err(e) = self.init_dir_blocks(ino, &mut inode, dir) {
            self.free_inode(ino, true)?;
            return Err(e);
        }
        self.write_inode(ino, &inode)?;
        if let Err(e) = self.add_entry(dir, &mut dinode, name, ino, FileType::Directory) {
            self.destroy_inode(ino, &mut inode)?;
            return Err(e);
        }
        self.inc_dir_links(&mut dinode)?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.maybe_commit()?;
        Ok(self.inode_attr(ino, &inode))
    }

    pub(crate) fn is_fast_symlink(&self, inode: &Inode) -> bool {
        inode.is_symlink()
            && !inode.has_flag(flags::INLINE_DATA)
            && !inode.has_flag(flags::EA_INODE)
            && inode.size() > 0
            && inode.size() < 60
    }

    pub fn symlink(&mut self, dir: Ino, name: &[u8], target: &[u8], uid: u32, gid: u32) -> Result<Attr> {
        self.require_rw()?;
        if target.is_empty() {
            return Err(Error::invalid("empty symlink target"));
        }
        if target.len() >= self.bs as usize || target.len() >= 4096 {
            return Err(Error::NameTooLong);
        }
        let mut dinode = self.parent_dir(dir)?;
        self.check_new_name(dir, &dinode, name)?;
        let (ino, mut inode) = self.new_inode(dir, &dinode, FileType::Symlink, 0o777, uid, gid)?;
        if target.len() < 60 {
            inode.set_flag(flags::EXTENTS, false);
            let area = inode.block_area_mut();
            area.fill(0);
            area[..target.len()].copy_from_slice(target);
            inode.set_size(target.len() as u64);
        } else {
            if !inode.has_flag(flags::EXTENTS) {
                self.free_inode(ino, false)?;
                return Err(Error::unsupported("slow symlinks without extents"));
            }
            let ipg = self.sb.inodes_per_group();
            let goal = self.group_first_block((ino - 1) / ipg);
            let (pblk, _) = self.alloc_blocks(goal, 1)?;
            self.ext_insert(
                ino,
                &mut inode,
                Extent {
                    block: 0,
                    len: 1,
                    start: pblk,
                    unwritten: false,
                },
            )?;
            let mut blk = vec![0u8; self.bs as usize];
            blk[..target.len()].copy_from_slice(target);
            self.cache.put(pblk, &blk);
            inode.set_sectors(self.bs as u64 / 512);
            inode.set_size(target.len() as u64);
        }
        self.write_inode(ino, &inode)?;
        if let Err(e) = self.add_entry(dir, &mut dinode, name, ino, FileType::Symlink) {
            self.destroy_inode(ino, &mut inode)?;
            return Err(e);
        }
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.maybe_commit()?;
        Ok(self.inode_attr(ino, &inode))
    }

    pub fn read_link(&mut self, ino: Ino) -> Result<Vec<u8>> {
        let inode = self.read_live_inode(ino)?;
        if !inode.is_symlink() {
            return Err(Error::invalid("not a symlink"));
        }
        let size = inode.size() as usize;
        if size >= 65536 {
            return Err(Error::corrupt("symlink too long"));
        }
        if self.is_fast_symlink(&inode) {
            return Ok(inode.block_area()[..size].to_vec());
        }
        if inode.has_flag(flags::INLINE_DATA) {
            let mut v = vec![0u8; size];
            let n = self.read_inode_data(ino, &inode, 0, &mut v)?;
            v.truncate(n);
            return Ok(v);
        }
        // Slow symlink targets are metadata (written through the cache and
        // the journal), so read them through the cache too.
        if size > self.bs as usize {
            return Err(Error::corrupt("symlink longer than a block"));
        }
        match self.map_block(ino, &inode, 0)? {
            super::extent::Mapping::Mapped {
                pblk, unwritten: false, ..
            } => {
                let b = self.cache.get(&*self.dev, pblk)?;
                Ok(b[..size].to_vec())
            }
            _ => Err(Error::corrupt(format!("symlink {ino} has no data block"))),
        }
    }

    /// Add a hard link to `ino` as `dir/name`.
    pub fn link(&mut self, ino: Ino, dir: Ino, name: &[u8]) -> Result<Attr> {
        self.require_rw()?;
        let mut inode = self.read_live_inode(ino)?;
        if inode.is_dir() {
            return Err(Error::NotPermitted);
        }
        if inode.links_count() >= LINK_MAX {
            return Err(Error::TooManyLinks);
        }
        if inode.links_count() == 0 {
            return Err(Error::NotFound);
        }
        let mut dinode = self.parent_dir(dir)?;
        self.check_new_name(dir, &dinode, name)?;
        self.add_entry(dir, &mut dinode, name, ino, inode.file_type())?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        inode.set_links_count(inode.links_count() + 1);
        inode.set_ctime(Timestamp::now());
        self.write_inode(ino, &inode)?;
        self.maybe_commit()?;
        Ok(self.inode_attr(ino, &inode))
    }

    /// Drop one link of an inode; release it when no links remain.
    fn drop_link(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        let n = inode.links_count();
        let is_dir = inode.is_dir();
        if is_dir || n <= 1 {
            inode.set_links_count(0);
        } else {
            inode.set_links_count(n - 1);
        }
        inode.set_ctime(Timestamp::now());
        if inode.links_count() > 0 {
            return self.write_inode(ino, inode);
        }
        if is_dir {
            // like Linux rmdir: directory size becomes 0
            self.free_all_blocks(ino, inode)?;
            inode.set_size(0);
        }
        if self.defer_unlinked {
            self.orphan_add(ino, inode);
            self.write_inode(ino, inode)
        } else {
            self.destroy_inode(ino, inode)
        }
    }

    pub fn unlink(&mut self, dir: Ino, name: &[u8]) -> Result<()> {
        self.require_rw()?;
        reject_dots(name)?;
        let mut dinode = self.parent_dir(dir)?;
        let slot = self.find_entry(dir, &dinode, name)?.ok_or(Error::NotFound)?;
        let mut inode = self.read_live_inode(slot.ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }
        if inode.has_flag(flags::IMMUTABLE) || inode.has_flag(flags::APPEND) {
            return Err(Error::NotPermitted);
        }
        self.remove_slot(dir, &mut dinode, &slot, name)?;
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.drop_link(slot.ino, &mut inode)?;
        self.maybe_commit()
    }

    pub fn rmdir(&mut self, dir: Ino, name: &[u8]) -> Result<()> {
        self.require_rw()?;
        if name == b"." {
            return Err(Error::invalid("rmdir ."));
        }
        if name == b".." {
            return Err(Error::NotEmpty);
        }
        validate_name(name)?;
        let mut dinode = self.parent_dir(dir)?;
        let slot = self.find_entry(dir, &dinode, name)?.ok_or(Error::NotFound)?;
        if slot.ino == ROOT_INO {
            return Err(Error::Busy);
        }
        let mut inode = self.read_live_inode(slot.ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if !self.dir_is_empty(slot.ino, &inode)? {
            return Err(Error::NotEmpty);
        }
        if inode.has_flag(flags::IMMUTABLE) || inode.has_flag(flags::APPEND) {
            return Err(Error::NotPermitted);
        }
        self.remove_slot(dir, &mut dinode, &slot, name)?;
        Self::dec_dir_links(&mut dinode);
        Self::touch_dir(&mut dinode);
        self.write_inode(dir, &dinode)?;
        self.drop_link(slot.ino, &mut inode)?;
        self.maybe_commit()
    }

    /// Whether `ancestor` is `dir` or one of its ancestors.
    fn is_ancestor(&mut self, ancestor: Ino, mut dir: Ino) -> Result<bool> {
        let mut steps = 0;
        loop {
            if dir == ancestor {
                return Ok(true);
            }
            if dir == ROOT_INO {
                return Ok(false);
            }
            steps += 1;
            if steps > 65536 {
                return Err(Error::corrupt("directory loop"));
            }
            let inode = self.read_live_inode(dir)?;
            let parent = self
                .find_entry(dir, &inode, b"..")?
                .ok_or_else(|| Error::corrupt("missing .."))?
                .ino;
            if parent == dir {
                return Ok(false);
            }
            dir = parent;
        }
    }

    pub fn rename(&mut self, sdir: Ino, sname: &[u8], ddir: Ino, dname: &[u8], fl: RenameFlags) -> Result<()> {
        self.require_rw()?;
        reject_dots(sname)?;
        reject_dots(dname)?;
        if fl.exchange && fl.no_replace {
            return Err(Error::invalid("exchange with no_replace"));
        }
        let sinode_dir = self.parent_dir(sdir)?;
        let src = self.find_entry(sdir, &sinode_dir, sname)?.ok_or(Error::NotFound)?;
        let dinode_dir = self.parent_dir(ddir)?;
        let dst = self.find_entry(ddir, &dinode_dir, dname)?;
        let src_inode = self.read_live_inode(src.ino)?;
        let src_is_dir = src_inode.is_dir();
        if sdir == ddir && sname == dname {
            return Ok(());
        }
        if src_is_dir && sdir != ddir && self.is_ancestor(src.ino, ddir)? {
            return Err(Error::invalid("cannot move a directory into itself"));
        }
        if fl.exchange {
            let dst = dst.ok_or(Error::NotFound)?;
            return self.rename_exchange(sdir, sname, src.ino, ddir, dname, dst.ino);
        }
        if let Some(d) = &dst {
            if fl.no_replace {
                return Err(Error::Exists);
            }
            if d.ino == src.ino {
                // both names already refer to the same inode
                return Ok(());
            }
            let t = self.read_live_inode(d.ino)?;
            if src_is_dir {
                if !t.is_dir() {
                    return Err(Error::NotDir);
                }
                if !self.dir_is_empty(d.ino, &t)? {
                    return Err(Error::NotEmpty);
                }
            } else if t.is_dir() {
                return Err(Error::IsDir);
            }
            if src_is_dir && d.ino != src.ino && self.is_ancestor(d.ino, ddir)? {
                return Err(Error::invalid("target is an ancestor"));
            }
        } else if src_is_dir && sdir != ddir {
            self.can_add_subdir(&dinode_dir)?;
        }
        let ft = src_inode.file_type();

        // 1. destination entry
        let mut dd = self.read_live_inode(ddir)?;
        let replaced = match &dst {
            Some(d) => {
                self.retarget_slot(ddir, &mut dd, d, dname, src.ino, ft)?;
                Some(d.ino)
            }
            None => {
                self.add_entry(ddir, &mut dd, dname, src.ino, ft)?;
                None
            }
        };
        Self::touch_dir(&mut dd);
        self.write_inode(ddir, &dd)?;

        // 2. remove the source entry (re-found: the block may have changed)
        let mut sd = self.read_live_inode(sdir)?;
        let slot = self
            .find_entry(sdir, &sd, sname)?
            .ok_or_else(|| Error::corrupt("rename source vanished"))?;
        self.remove_slot(sdir, &mut sd, &slot, sname)?;
        Self::touch_dir(&mut sd);
        self.write_inode(sdir, &sd)?;

        // 3. directory link bookkeeping
        if src_is_dir && sdir != ddir {
            let mut s = self.read_live_inode(src.ino)?;
            self.set_dotdot(src.ino, &mut s, ddir)?;
            s.set_ctime(Timestamp::now());
            self.write_inode(src.ino, &s)?;
            let mut sd = self.read_live_inode(sdir)?;
            Self::dec_dir_links(&mut sd);
            self.write_inode(sdir, &sd)?;
            if replaced.is_none() {
                let mut dd = self.read_live_inode(ddir)?;
                self.inc_dir_links(&mut dd)?;
                self.write_inode(ddir, &dd)?;
            }
        } else {
            let mut s = self.read_live_inode(src.ino)?;
            s.set_ctime(Timestamp::now());
            self.write_inode(src.ino, &s)?;
        }

        // 4. release the replaced inode
        if let Some(t) = replaced {
            let mut ti = self.read_live_inode(t)?;
            if ti.is_dir() && sdir == ddir {
                // same parent loses one subdirectory
                let mut dd = self.read_live_inode(ddir)?;
                Self::dec_dir_links(&mut dd);
                self.write_inode(ddir, &dd)?;
            }
            self.drop_link(t, &mut ti)?;
        }
        self.maybe_commit()
    }

    fn rename_exchange(
        &mut self,
        sdir: Ino,
        sname: &[u8],
        sino: Ino,
        ddir: Ino,
        dname: &[u8],
        dino: Ino,
    ) -> Result<()> {
        if sino == dino {
            return Ok(());
        }
        let si = self.read_live_inode(sino)?;
        let di = self.read_live_inode(dino)?;
        if si.is_dir() && sdir != ddir && self.is_ancestor(sino, ddir)? {
            return Err(Error::invalid("cannot move a directory into itself"));
        }
        if di.is_dir() && sdir != ddir && self.is_ancestor(dino, sdir)? {
            return Err(Error::invalid("cannot move a directory into itself"));
        }
        let mut dd = self.read_live_inode(ddir)?;
        let dslot = self.find_entry(ddir, &dd, dname)?.ok_or(Error::NotFound)?;
        self.retarget_slot(ddir, &mut dd, &dslot, dname, sino, si.file_type())?;
        Self::touch_dir(&mut dd);
        self.write_inode(ddir, &dd)?;
        let mut sd = self.read_live_inode(sdir)?;
        let sslot = self.find_entry(sdir, &sd, sname)?.ok_or(Error::NotFound)?;
        self.retarget_slot(sdir, &mut sd, &sslot, sname, dino, di.file_type())?;
        Self::touch_dir(&mut sd);
        self.write_inode(sdir, &sd)?;
        if sdir != ddir {
            if si.is_dir() {
                let mut s = self.read_live_inode(sino)?;
                self.set_dotdot(sino, &mut s, ddir)?;
                self.write_inode(sino, &s)?;
            }
            if di.is_dir() {
                let mut d = self.read_live_inode(dino)?;
                self.set_dotdot(dino, &mut d, sdir)?;
                self.write_inode(dino, &d)?;
            }
            if si.is_dir() != di.is_dir() {
                let (gain, lose) = if si.is_dir() { (ddir, sdir) } else { (sdir, ddir) };
                let mut g = self.read_live_inode(gain)?;
                self.inc_dir_links(&mut g)?;
                self.write_inode(gain, &g)?;
                let mut l = self.read_live_inode(lose)?;
                Self::dec_dir_links(&mut l);
                self.write_inode(lose, &l)?;
            }
        }
        let now = Timestamp::now();
        for ino in [sino, dino] {
            let mut i = self.read_live_inode(ino)?;
            i.set_ctime(now);
            self.write_inode(ino, &i)?;
        }
        self.maybe_commit()
    }

    pub fn set_attr(&mut self, ino: Ino, a: &SetAttr) -> Result<Attr> {
        self.require_rw()?;
        let mut inode = self.read_live_inode(ino)?;
        let now = Timestamp::now();
        let mut changed = false;
        if let Some(f) = a.flags {
            let nf = (inode.flags() & !USER_MODIFIABLE) | (f & USER_MODIFIABLE);
            inode.set_flags(nf);
            changed = true;
        }
        if (inode.has_flag(flags::IMMUTABLE) && a.flags.is_none())
            && (a.size.is_some() || a.perm.is_some() || a.uid.is_some() || a.gid.is_some())
        {
            return Err(Error::NotPermitted);
        }
        if let Some(p) = a.perm {
            inode.set_mode((inode.mode() & mode::S_IFMT) | (p & 0o7777));
            changed = true;
        }
        if let Some(u) = a.uid {
            inode.set_uid(u);
            changed = true;
        }
        if let Some(g) = a.gid {
            inode.set_gid(g);
            changed = true;
        }
        if let Some(sz) = a.size {
            if inode.is_dir() {
                return Err(Error::IsDir);
            }
            if !inode.is_reg() {
                return Err(Error::invalid("truncate of non-regular file"));
            }
            if sz != inode.size() {
                if sz > inode.size() {
                    self.ensure_space(1)?;
                }
                self.set_size(ino, &mut inode, sz)?;
                inode.set_mtime(now);
            }
            changed = true;
        }
        if let Some(t) = a.atime {
            inode.set_atime(t);
            changed = true;
        }
        if let Some(t) = a.mtime {
            inode.set_mtime(t);
            changed = true;
        }
        if let Some(t) = a.crtime {
            inode.set_crtime(t);
            changed = true;
        }
        if changed {
            inode.set_ctime(a.ctime.unwrap_or(now));
        } else if let Some(t) = a.ctime {
            inode.set_ctime(t);
        }
        self.write_inode(ino, &inode)?;
        self.maybe_commit()?;
        Ok(self.inode_attr(ino, &inode))
    }

    /// Truncate or extend a regular file.
    pub fn truncate(&mut self, ino: Ino, size: u64) -> Result<Attr> {
        self.set_attr(
            ino,
            &SetAttr {
                size: Some(size),
                ..Default::default()
            },
        )
    }
}
