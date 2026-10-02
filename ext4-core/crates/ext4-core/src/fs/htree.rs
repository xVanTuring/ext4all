//! Hashed (htree / dx) directories.

use super::crypt::{Fname, NameView};
use super::dir::DirSlot;
use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::hash::dirhash;
use crate::ondisk::dirent::{self as de, DX_NODE_ENTRIES_OFF, DX_ROOT_INFO_OFF, DxEntry, DxRootInfo};
use crate::ondisk::inode::{FileType, Inode, flags};
use crate::ondisk::superblock::{hash_version as hv, incompat};

/// Marks directory cookies that are hash positions (htree directories)
/// rather than byte offsets (linear directories).
pub(crate) const HASH_COOKIE: u64 = 1 << 63;

/// An entry gathered for enumeration: cookie key, on-disk name, inode,
/// file type, (major, minor) hash.
type HashedEntry = (u64, Vec<u8>, Ino, u8, (u32, u32));

#[derive(Clone, Debug)]
struct Frame {
    pblk: u64,
    data: Vec<u8>,
    off: usize,
    at: usize,
}

impl Frame {
    fn count(&self) -> usize {
        de::dx_count(&self.data, self.off) as usize
    }

    fn limit(&self) -> usize {
        de::dx_limit(&self.data, self.off) as usize
    }

    fn entry(&self, i: usize) -> DxEntry {
        de::dx_entry(&self.data, self.off, i)
    }

    fn insert(&mut self, pos: usize, e: DxEntry) {
        let n = self.count();
        let s = self.off + pos * 8;
        let end = self.off + n * 8;
        // entry 0's hash slot is the count/limit header: never shift it
        debug_assert!(pos >= 1);
        self.data.copy_within(s..end, s + 8);
        de::set_dx_entry(&mut self.data, self.off, pos, e);
        de::set_dx_count(&mut self.data, self.off, (n + 1) as u16);
    }
}

impl Fs {
    fn dx_max_levels(&self) -> u8 {
        if self.sb.has_incompat(incompat::LARGEDIR) { 3 } else { 2 }
    }

    /// Hash version and seed to use for a directory, from its root block.
    /// Encrypted directories hash the ciphertext names the same way;
    /// casefolded ones hash casefolded (or, encrypted, keyed) names.
    fn dx_hash_params(&self, inode: &Inode, root: &[u8]) -> Result<(u8, [u32; 4])> {
        if inode.has_flag(flags::CASEFOLD) {
            return Err(Error::unsupported("casefold htree hashing"));
        }
        let info = DxRootInfo::parse(root);
        let v = self.sb.effective_hash_version(info.hash_version);
        if v > 5 {
            return Err(Error::unsupported(format!("htree hash version {v}")));
        }
        Ok((v, self.sb.hash_seed()))
    }

    fn name_hash(&self, inode: &Inode, root: &[u8], name: &[u8]) -> Result<u32> {
        let (v, seed) = self.dx_hash_params(inode, root)?;
        Ok(dirhash(name, v, &seed).ok_or_else(|| Error::unsupported("hash"))?.major)
    }

    fn dx_read_frame(&mut self, ino: Ino, inode: &Inode, lblk: u64, root: bool) -> Result<Frame> {
        let (pblk, data) = self.dir_block(ino, inode, lblk)?;
        let csum = self.sb.has_metadata_csum();
        let bs = self.bs as usize;
        let (off, want_limit) = if root {
            let info = DxRootInfo::parse(&data);
            if crate::bytes::le32(&data, DX_ROOT_INFO_OFF) != 0 || info.info_length != 8 {
                return Err(Error::corrupt(format!("directory {ino}: bad dx root info")));
            }
            if info.indirect_levels >= self.dx_max_levels() {
                return Err(Error::corrupt(format!("directory {ino}: htree too deep")));
            }
            (DX_ROOT_INFO_OFF + 8, de::dx_root_limit(bs, csum))
        } else {
            (DX_NODE_ENTRIES_OFF, de::dx_node_limit(bs, csum))
        };
        let f = Frame { pblk, data, off, at: 0 };
        if f.limit() != want_limit as usize || f.count() == 0 || f.count() > f.limit() {
            return Err(Error::corrupt(format!(
                "directory {ino}: bad dx count/limit {}/{} at block {lblk}",
                f.count(),
                f.limit()
            )));
        }
        Ok(f)
    }

    /// Walk from the root to the index node covering `hash`.
    fn dx_probe(&mut self, ino: Ino, inode: &Inode, hash: u32) -> Result<Vec<Frame>> {
        let mut frames = Vec::new();
        let mut f = self.dx_read_frame(ino, inode, 0, true)?;
        let levels = DxRootInfo::parse(&f.data).indirect_levels as usize;
        let nblocks = self.dir_nblocks(inode);
        loop {
            // last entry with hash <= target (entry 0 counts as hash 0)
            let (mut lo, mut hi) = (1usize, f.count());
            while lo < hi {
                let mid = (lo + hi) / 2;
                if f.entry(mid).hash <= hash {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            f.at = lo - 1;
            let child = f.entry(f.at).block as u64;
            if child >= nblocks {
                return Err(Error::corrupt(format!("directory {ino}: dx entry points past end")));
            }
            let depth = frames.len();
            frames.push(f);
            if depth >= levels {
                return Ok(frames);
            }
            f = self.dx_read_frame(ino, inode, child, false)?;
        }
    }

    /// Move to the next leaf in hash order. Returns the hash of the index
    /// entry stepped to, at the level where the step happened (its low bit
    /// flags a collision chain continuing from the previous leaf; entry 0
    /// of the lower levels carries no hash), or `None` at the end.
    fn dx_advance(&mut self, ino: Ino, inode: &Inode, frames: &mut [Frame]) -> Result<Option<u32>> {
        let mut p = frames.len() - 1;
        loop {
            if frames[p].at + 1 < frames[p].count() {
                frames[p].at += 1;
                break;
            }
            if p == 0 {
                return Ok(None);
            }
            p -= 1;
        }
        let hash = frames[p].entry(frames[p].at).hash;
        while p + 1 < frames.len() {
            let child = frames[p].entry(frames[p].at).block as u64;
            let mut f = self.dx_read_frame(ino, inode, child, false)?;
            f.at = 0;
            frames[p + 1] = f;
            p += 1;
        }
        Ok(Some(hash))
    }

    /// Enumerate an htree directory in hash order with stable cookies:
    /// `HASH_COOKIE | pos`, where pos 0 = ".", 1 = "..", and `key + 2` for
    /// entries (key = major hash >> 1 << 32 | minor hash; for the legacy
    /// hash, which has no minor part, a CRC32C of the name). Entries never
    /// change their key, so splits and inserts cannot make an enumeration
    /// skip or repeat unchanged entries. A whole collision chain (leaves
    /// linked by the continuation bit) is sorted together, so resuming in
    /// the middle of one is exact. Only two names with identical 63-bit
    /// keys could be skipped when a resume falls between them.
    pub(crate) fn dx_read_dir(
        &mut self,
        ino: Ino,
        inode: &Inode,
        cookie: u64,
        view: &NameView,
        f: &mut dyn FnMut(super::DirEntryInfo) -> bool,
    ) -> Result<()> {
        let pos = if cookie == 0 {
            0
        } else if cookie & HASH_COOKIE != 0 {
            cookie & !HASH_COOKIE
        } else {
            return Err(Error::StaleCookie);
        };
        let root = self.dir_block(ino, inode, 0)?.1;
        let (version, seed) = self.dx_hash_params(inode, &root)?;
        let root_entries = self.block_entries(&root, super::dir::DirBlockKind::DxRoot)?;
        for (i, d) in root_entries.iter().enumerate().take(2) {
            if (i as u64) < pos || d.inode == 0 {
                continue;
            }
            let info = super::DirEntryInfo {
                name: d.name(&root).to_vec(),
                ino: d.inode,
                file_type: self.dirent_file_type(d.inode, d.file_type)?,
                next_cookie: HASH_COOKIE | (i as u64 + 1),
            };
            if !f(info) {
                return Ok(());
            }
        }
        let start_key = pos.saturating_sub(2);
        // the legacy hash has no minor part: order names with the same
        // major hash by a second, independent hash so their keys differ
        let legacy = matches!(version, hv::LEGACY | hv::LEGACY_UNSIGNED);
        // (cookie key, (major, minor) hash)
        let key_of = |name: &[u8]| -> Result<(u64, (u32, u32))> {
            let h = dirhash(name, version, &seed).ok_or_else(|| Error::unsupported("hash"))?;
            let minor = if legacy { crate::csum::crc32c(!0, name) } else { h.minor };
            Ok(((((h.major >> 1) as u64) << 32) | minor as u64, (h.major, h.minor)))
        };
        let mut frames = self.dx_probe(ino, inode, ((start_key >> 32) as u32) << 1)?;
        let bs = self.bs as usize;
        let mut more = true;
        while more {
            // gather a leaf plus any leaves continuing its last hash
            let mut batch: Vec<HashedEntry> = Vec::new();
            loop {
                let b = frames.last().unwrap();
                let leaf = b.entry(b.at).block as u64;
                let (_, data) = self.dir_block(ino, inode, leaf)?;
                for d in de::parse_block(&data, self.leaf_limit(&data), bs)? {
                    if d.inode != 0 {
                        let name = d.name(&data).to_vec();
                        let (key, hash) = key_of(&name)?;
                        batch.push((key, name, d.inode, d.file_type, hash));
                    }
                }
                match self.dx_advance(ino, inode, &mut frames)? {
                    None => {
                        more = false;
                        break;
                    }
                    Some(hash) if hash & 1 == 0 => break,
                    Some(_) => {} // the chain continues in the next leaf
                }
            }
            batch.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for (key, disk, child, ft, hash) in batch {
                if key < start_key {
                    continue;
                }
                let name = if view.is_plain() {
                    disk
                } else {
                    match view.present(&disk, None, Some(hash)) {
                        Some(n) => n,
                        None => continue,
                    }
                };
                let info = super::DirEntryInfo {
                    name,
                    ino: child,
                    file_type: self.dirent_file_type(child, ft)?,
                    next_cookie: HASH_COOKIE | (key + 3),
                };
                if !f(info) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Advance to the next leaf if it may contain entries with `hash`
    /// (hash collision continuation).
    fn dx_next_leaf(&mut self, ino: Ino, inode: &Inode, frames: &mut [Frame], hash: u32) -> Result<bool> {
        let mut p = frames.len() - 1;
        loop {
            if frames[p].at + 1 < frames[p].count() {
                frames[p].at += 1;
                break;
            }
            if p == 0 {
                return Ok(false);
            }
            p -= 1;
        }
        let bhash = frames[p].entry(frames[p].at).hash;
        if bhash & !1 != hash {
            return Ok(false);
        }
        while p + 1 < frames.len() {
            let child = frames[p].entry(frames[p].at).block as u64;
            let mut f = self.dx_read_frame(ino, inode, child, false)?;
            f.at = 0;
            frames[p + 1] = f;
            p += 1;
        }
        Ok(true)
    }

    pub(crate) fn dx_find(&mut self, ino: Ino, inode: &Inode, name: &Fname) -> Result<Option<DirSlot>> {
        let root = self.dir_block(ino, inode, 0)?.1;
        let hash = match (name.disk(), name.hash_hint()) {
            (Some(n), _) => self.name_hash(inode, &root, n)?,
            (None, Some(h)) => {
                // hash as listed; validate the root before trusting it
                self.dx_hash_params(inode, &root)?;
                h & !1
            }
            (None, None) => return Err(Error::unsupported("name without a hash")),
        };
        let mut frames = self.dx_probe(ino, inode, hash)?;
        loop {
            let b = frames.last().unwrap();
            let leaf = b.entry(b.at).block as u64;
            if let Some(slot) = self.find_in_block(ino, inode, leaf, name)? {
                return Ok(Some(slot));
            }
            if !self.dx_next_leaf(ino, inode, &mut frames, hash)? {
                return Ok(None);
            }
        }
    }

    fn write_frame(&mut self, ino: Ino, inode: &Inode, f: &Frame) {
        self.write_dx_block(ino, inode, f.pblk, f.data.clone(), f.off);
    }

    /// Convert a full one-block linear directory into an htree.
    pub(crate) fn make_indexed(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        let bs = self.bs as usize;
        let csum = self.sb.has_metadata_csum();
        let (pblk0, data0) = self.dir_block(ino, inode, 0)?;
        let entries = de::parse_block(&data0, self.leaf_limit(&data0), bs)?;
        if entries.len() < 2 || entries[0].name(&data0) != b"." || entries[1].name(&data0) != b".." {
            return Err(Error::corrupt(format!("directory {ino}: first block lacks . and ..")));
        }
        let parent = entries[1].inode;
        // move everything after ".." into a fresh leaf
        let (_, pblk1) = self.dir_append_block(ino, inode)?;
        let mut leaf = vec![0u8; bs];
        de::init_empty_block(&mut leaf, csum);
        let limit = if csum { bs - de::TAIL_SIZE } else { bs };
        let live: Vec<_> = entries[2..].iter().filter(|d| d.inode != 0).collect();
        let mut off = 0;
        for (i, d) in live.iter().enumerate() {
            let need = de::rec_len_for(d.name_len);
            let rec = if i + 1 == live.len() { limit - off } else { need };
            de::write_entry(&mut leaf, off, d.inode, rec, d.name(&data0), d.file_type, bs);
            off += need;
        }
        self.write_leaf_block(ino, inode, pblk1, leaf);
        // rebuild block 0 as the dx root
        let mut root = vec![0u8; bs];
        let dt = self.dirent_type(FileType::Directory);
        de::write_entry(&mut root, 0, ino, 12, b".", dt, bs);
        de::write_entry(&mut root, 12, parent, bs - 12, b"..", dt, bs);
        DxRootInfo {
            hash_version: self.sb.def_hash_version(),
            info_length: 8,
            indirect_levels: 0,
            unused_flags: 0,
        }
        .write(&mut root);
        let eoff = DX_ROOT_INFO_OFF + 8;
        de::set_dx_limit(&mut root, eoff, de::dx_root_limit(bs, csum));
        de::set_dx_count(&mut root, eoff, 1);
        de::set_dx_entry(&mut root, eoff, 0, DxEntry { hash: 0, block: 1 });
        inode.set_flag(flags::INDEX, true);
        self.write_dx_block(ino, inode, pblk0, root, eoff);
        Ok(())
    }

    /// Insert an entry into an htree directory, splitting as needed.
    pub(crate) fn dx_add_entry(&mut self, ino: Ino, inode: &mut Inode, name: &[u8], child: Ino, ft: u8) -> Result<()> {
        let root = self.dir_block(ino, inode, 0)?.1;
        let hash = self.name_hash(inode, &root, name)?;
        for _ in 0..16 {
            let mut frames = self.dx_probe(ino, inode, hash)?;
            let b = frames.len() - 1;
            let leaf = frames[b].entry(frames[b].at).block as u64;
            if self.dx_insert_into_leaf(ino, inode, leaf, name, child, ft)? {
                return Ok(());
            }
            if frames[b].count() < frames[b].limit() {
                if self.dx_split_leaf(ino, inode, &mut frames, name, hash, child, ft)? {
                    return Ok(());
                }
                continue;
            }
            // bottom index node full: split the lowest full node whose
            // parent has room, or add a level
            match (0..b).rev().find(|&l| frames[l].count() < frames[l].limit()) {
                Some(l) => self.dx_split_node(ino, inode, &mut frames, l + 1)?,
                None => {
                    let levels = DxRootInfo::parse(&frames[0].data).indirect_levels;
                    if levels + 1 >= self.dx_max_levels() {
                        return Err(Error::NoSpace);
                    }
                    self.dx_add_level(ino, inode, &mut frames)?;
                }
            }
        }
        Err(Error::corrupt(format!(
            "directory {ino}: htree insert did not converge"
        )))
    }

    fn dx_insert_into_leaf(
        &mut self,
        ino: Ino,
        inode: &Inode,
        lblk: u64,
        name: &[u8],
        child: Ino,
        ft: u8,
    ) -> Result<bool> {
        let (pblk, mut data) = self.dir_block(ino, inode, lblk)?;
        if self.place_entry(&mut data, name, child, ft)? {
            self.write_leaf_block(ino, inode, pblk, data);
            return Ok(true);
        }
        Ok(false)
    }

    /// Split the leaf under the bottom frame and insert the new entry.
    #[allow(clippy::too_many_arguments)]
    fn dx_split_leaf(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        frames: &mut [Frame],
        name: &[u8],
        hash: u32,
        child: Ino,
        ft: u8,
    ) -> Result<bool> {
        let bs = self.bs as usize;
        let csum = self.sb.has_metadata_csum();
        let b = frames.len() - 1;
        let leaf_lblk = frames[b].entry(frames[b].at).block as u64;
        let (pblk, data) = self.dir_block(ino, inode, leaf_lblk)?;
        let root = self.dir_block(ino, inode, 0)?.1;
        let entries = de::parse_block(&data, self.leaf_limit(&data), bs)?;
        let mut map: Vec<(u32, Vec<u8>, u32, u8)> = Vec::new();
        for d in entries.iter().filter(|d| d.inode != 0) {
            let n = d.name(&data).to_vec();
            let h = self.name_hash(inode, &root, &n)?;
            map.push((h, n, d.inode, d.file_type));
        }
        map.sort_by_key(|m| m.0);
        if map.len() < 2 {
            return Err(Error::NoSpace);
        }
        // split in the middle, size-wise (like Linux do_split)
        let mut size = 0usize;
        let mut mv = 0usize;
        for m in map.iter().rev() {
            let s = de::rec_len_for(m.1.len());
            if size + s / 2 > bs / 2 {
                break;
            }
            size += s;
            mv += 1;
        }
        let mut split = map.len() - mv;
        if split == 0 {
            split = 1;
        }
        let hash2 = map[split].0;
        let continued = map[split - 1].0 == hash2;

        let (new_lblk, new_pblk) = self.dir_append_block(ino, inode)?;
        let build = |part: &[(u32, Vec<u8>, u32, u8)]| -> Vec<u8> {
            let mut blk = vec![0u8; bs];
            de::init_empty_block(&mut blk, csum);
            let limit = if csum { bs - de::TAIL_SIZE } else { bs };
            let mut off = 0;
            for (i, (_, n, ino, ft)) in part.iter().enumerate() {
                let need = de::rec_len_for(n.len());
                let rec = if i + 1 == part.len() { limit - off } else { need };
                de::write_entry(&mut blk, off, *ino, rec, n, *ft, bs);
                off += need;
            }
            blk
        };
        let mut old_blk = build(&map[..split]);
        let mut new_blk = build(&map[split..]);
        let placed = if hash >= hash2 {
            self.place_entry(&mut new_blk, name, child, ft)?
        } else {
            self.place_entry(&mut old_blk, name, child, ft)?
        };
        self.write_leaf_block(ino, inode, pblk, old_blk);
        self.write_leaf_block(ino, inode, new_pblk, new_blk);
        let at = frames[b].at;
        frames[b].insert(
            at + 1,
            DxEntry {
                hash: hash2 | continued as u32,
                block: new_lblk as u32,
            },
        );
        let f = frames[b].clone();
        self.write_frame(ino, inode, &f);
        Ok(placed)
    }

    /// Split the full index node at `level` (> 0); its parent has room.
    fn dx_split_node(&mut self, ino: Ino, inode: &mut Inode, frames: &mut [Frame], level: usize) -> Result<()> {
        let bs = self.bs as usize;
        let csum = self.sb.has_metadata_csum();
        let node = frames[level].clone();
        let count = node.count();
        let half = count / 2;
        let (new_lblk, new_pblk) = self.dir_append_block(ino, inode)?;
        let mut nb = vec![0u8; bs];
        // fake empty dirent spanning the block
        de::write_entry(&mut nb, 0, 0, bs, b"", 0, bs);
        de::set_dx_limit(&mut nb, DX_NODE_ENTRIES_OFF, de::dx_node_limit(bs, csum));
        de::set_dx_count(&mut nb, DX_NODE_ENTRIES_OFF, (count - half) as u16);
        for (j, i) in (half..count).enumerate() {
            let e = node.entry(i);
            de::set_dx_entry(&mut nb, DX_NODE_ENTRIES_OFF, j, e);
        }
        let hash2 = node.entry(half).hash;
        let mut left = node.clone();
        de::set_dx_count(&mut left.data, left.off, half as u16);
        for i in half..count {
            let o = left.off + i * 8;
            left.data[o..o + 8].fill(0);
        }
        self.write_frame(ino, inode, &left);
        self.write_dx_block(ino, inode, new_pblk, nb, DX_NODE_ENTRIES_OFF);
        let pat = frames[level - 1].at;
        frames[level - 1].insert(
            pat + 1,
            DxEntry {
                hash: hash2,
                block: new_lblk as u32,
            },
        );
        let parent = frames[level - 1].clone();
        self.write_frame(ino, inode, &parent);
        Ok(())
    }

    /// Root is full: move its entries into a new node one level down.
    fn dx_add_level(&mut self, ino: Ino, inode: &mut Inode, frames: &mut [Frame]) -> Result<()> {
        let bs = self.bs as usize;
        let csum = self.sb.has_metadata_csum();
        let root = frames[0].clone();
        let count = root.count();
        let (new_lblk, new_pblk) = self.dir_append_block(ino, inode)?;
        let mut nb = vec![0u8; bs];
        de::write_entry(&mut nb, 0, 0, bs, b"", 0, bs);
        de::set_dx_limit(&mut nb, DX_NODE_ENTRIES_OFF, de::dx_node_limit(bs, csum));
        de::set_dx_count(&mut nb, DX_NODE_ENTRIES_OFF, count as u16);
        for i in 0..count {
            de::set_dx_entry(&mut nb, DX_NODE_ENTRIES_OFF, i, root.entry(i));
        }
        self.write_dx_block(ino, inode, new_pblk, nb, DX_NODE_ENTRIES_OFF);
        let mut r = root.clone();
        for i in 1..count {
            let o = r.off + i * 8;
            r.data[o..o + 8].fill(0);
        }
        de::set_dx_count(&mut r.data, r.off, 1);
        de::set_dx_entry(
            &mut r.data,
            r.off,
            0,
            DxEntry {
                hash: 0,
                block: new_lblk as u32,
            },
        );
        let mut info = DxRootInfo::parse(&r.data);
        info.indirect_levels += 1;
        info.write(&mut r.data);
        self.write_frame(ino, inode, &r);
        Ok(())
    }

    /// Structural check of an htree directory: every leaf entry's hash must
    /// fall inside its index range (used by tests).
    pub fn check_htree(&mut self, ino: Ino) -> Result<()> {
        let inode = self.read_live_inode(ino)?;
        if !inode.has_flag(flags::INDEX) {
            return Ok(());
        }
        let root = self.dir_block(ino, &inode, 0)?.1;
        let f = self.dx_read_frame(ino, &inode, 0, true)?;
        let levels = DxRootInfo::parse(&f.data).indirect_levels;
        self.check_dx_node(ino, &inode, &root, &f, levels, 0, u64::MAX)
    }

    #[allow(clippy::too_many_arguments)]
    fn check_dx_node(
        &mut self,
        ino: Ino,
        inode: &Inode,
        root: &[u8],
        f: &Frame,
        levels: u8,
        lo: u64,
        hi: u64,
    ) -> Result<()> {
        let n = f.count();
        for i in 0..n {
            let e = f.entry(i);
            let start = if i == 0 { lo } else { (e.hash & !1) as u64 };
            if i > 0 && f.entry(i - 1).hash > e.hash {
                return Err(Error::corrupt("dx entries unsorted"));
            }
            let end = if i + 1 < n {
                (f.entry(i + 1).hash & !1) as u64 + (f.entry(i + 1).hash & 1) as u64
            } else {
                hi
            };
            if levels > 0 {
                let child = self.dx_read_frame(ino, inode, e.block as u64, false)?;
                self.check_dx_node(ino, inode, root, &child, levels - 1, start, end)?;
            } else {
                let (_, data) = self.dir_block(ino, inode, e.block as u64)?;
                let bs = self.bs as usize;
                for d in de::parse_block(&data, self.leaf_limit(&data), bs)? {
                    if d.inode == 0 {
                        continue;
                    }
                    let h = self.name_hash(inode, root, d.name(&data))? as u64;
                    // `end` already admits the next range's start hash when
                    // that range is a collision continuation
                    if h < start || h >= end {
                        return Err(Error::corrupt(format!(
                            "directory {ino}: entry {:?} hash {h:#x} outside [{start:#x},{end:#x}) in block {}",
                            String::from_utf8_lossy(d.name(&data)),
                            e.block
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}
