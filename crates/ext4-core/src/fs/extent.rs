//! Extent tree lookup and modification.
//!
//! Nodes are copied out of the cache into [`Node`] values, modified, and
//! written back. The root node lives in the inode's `i_block`; callers must
//! persist the inode after any modifying call.

use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::extent::{
    self as ex, ENTRY_SIZE, EXTENT_MAGIC, Extent, ExtentHeader, ExtentIndex, HEADER_SIZE, MAX_DEPTH,
};
use crate::ondisk::inode::Inode;

/// Result of mapping one logical block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mapping {
    /// `len` blocks from `lblk` map contiguously to `pblk`.
    Mapped { pblk: u64, len: u64, unwritten: bool },
    /// `len` blocks from `lblk` are unmapped (`u64::MAX`: to the end).
    Hole { len: u64 },
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    /// `None` for the root in the inode.
    pub blk: Option<u64>,
    pub data: Vec<u8>,
}

impl Node {
    fn header(&self) -> ExtentHeader {
        ExtentHeader::parse(&self.data)
    }

    fn set_header(&mut self, h: ExtentHeader) {
        h.write(&mut self.data);
    }

    fn entries(&self) -> usize {
        self.header().entries as usize
    }

    fn set_entries(&mut self, n: usize) {
        let mut h = self.header();
        h.entries = n as u16;
        self.set_header(h);
    }

    fn extent(&self, i: usize) -> Extent {
        Extent::parse(ex::entry(&self.data, i))
    }

    fn set_extent(&mut self, i: usize, e: Extent) {
        e.write(ex::entry_mut(&mut self.data, i));
    }

    fn index(&self, i: usize) -> ExtentIndex {
        ExtentIndex::parse(ex::entry(&self.data, i))
    }

    fn set_index(&mut self, i: usize, ix: ExtentIndex) {
        ix.write(ex::entry_mut(&mut self.data, i));
    }

    fn key(&self, i: usize) -> u32 {
        crate::bytes::le32(ex::entry(&self.data, i), 0)
    }

    fn raw_entry(&self, i: usize) -> [u8; ENTRY_SIZE] {
        ex::entry(&self.data, i).try_into().unwrap()
    }

    fn insert_raw(&mut self, pos: usize, raw: [u8; ENTRY_SIZE]) {
        let n = self.entries();
        let s = HEADER_SIZE + pos * ENTRY_SIZE;
        let e = HEADER_SIZE + n * ENTRY_SIZE;
        self.data.copy_within(s..e, s + ENTRY_SIZE);
        self.data[s..s + ENTRY_SIZE].copy_from_slice(&raw);
        self.set_entries(n + 1);
    }

    fn remove_raw(&mut self, pos: usize) {
        let n = self.entries();
        let s = HEADER_SIZE + pos * ENTRY_SIZE;
        let e = HEADER_SIZE + n * ENTRY_SIZE;
        self.data.copy_within(s + ENTRY_SIZE..e, s);
        self.data[e - ENTRY_SIZE..e].fill(0);
        self.set_entries(n - 1);
    }

    /// Position of the last entry whose key is <= `key` (0 if none).
    fn search(&self, key: u32) -> usize {
        let n = self.entries();
        let mut lo = 0;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.saturating_sub(1)
    }
}

fn extent_raw(e: Extent) -> [u8; ENTRY_SIZE] {
    let mut b = [0u8; ENTRY_SIZE];
    e.write(&mut b);
    b
}

fn index_raw(ix: ExtentIndex) -> [u8; ENTRY_SIZE] {
    let mut b = [0u8; ENTRY_SIZE];
    ix.write(&mut b);
    b
}

fn can_merge(a: &Extent, b: &Extent) -> bool {
    a.unwritten == b.unwritten
        && a.end() == b.block as u64
        && a.start + a.len as u64 == b.start
        && a.len + b.len <= a.max_len()
}

type Path = Vec<(Node, usize)>;

impl Fs {
    fn ext_root(&self, inode: &Inode) -> Result<Node> {
        let node = Node {
            blk: None,
            data: inode.block_area().to_vec(),
        };
        let h = node.header();
        if h.magic != EXTENT_MAGIC {
            return Err(Error::corrupt("bad extent root magic"));
        }
        if h.max == 0 || h.max > 4 || h.entries > h.max || h.depth > MAX_DEPTH {
            return Err(Error::corrupt(format!("bad extent root header {h:?}")));
        }
        Ok(node)
    }

    fn ext_read_node(&mut self, ino: Ino, inode: &Inode, blk: u64, depth: u16) -> Result<Node> {
        if blk < self.sb.first_data_block() as u64 || blk >= self.sb.blocks_count() {
            return Err(Error::corrupt(format!("inode {ino}: extent node at bad block {blk}")));
        }
        let data = self.cache.read(&*self.dev, blk)?;
        let h = ExtentHeader::parse(&data);
        if h.magic != EXTENT_MAGIC
            || h.depth != depth
            || h.max == 0
            || h.max > ex::max_entries(self.bs as usize)
            || h.entries > h.max
        {
            return Err(Error::corrupt(format!(
                "inode {ino}: bad extent node header at block {blk}: {h:?} (want depth {depth})"
            )));
        }
        if self.sb.has_metadata_csum() && !ex::verify_block_csum(self.inode_seed(ino, inode), &data) {
            self.checksum_error(format!("inode {ino}: extent block {blk}"))?;
        }
        Ok(Node { blk: Some(blk), data })
    }

    fn ext_write_node(&mut self, ino: Ino, inode: &mut Inode, node: &Node) {
        match node.blk {
            None => inode.block_area_mut().copy_from_slice(&node.data[..60]),
            Some(b) => {
                let mut data = node.data.clone();
                if self.sb.has_metadata_csum() {
                    ex::set_block_csum(self.inode_seed(ino, inode), &mut data);
                }
                self.cache.put(b, &data);
            }
        }
    }

    fn ext_find_path(&mut self, ino: Ino, inode: &Inode, lblk: u32) -> Result<Path> {
        let mut path = Vec::new();
        let mut node = self.ext_root(inode)?;
        loop {
            let h = node.header();
            let idx = node.search(lblk);
            if h.depth == 0 {
                path.push((node, idx));
                return Ok(path);
            }
            if h.entries == 0 {
                return Err(Error::corrupt(format!("inode {ino}: empty extent index node")));
            }
            let child = node.index(idx).leaf;
            path.push((node, idx));
            if path.len() > MAX_DEPTH as usize + 1 {
                return Err(Error::corrupt("extent tree too deep"));
            }
            node = self.ext_read_node(ino, inode, child, h.depth - 1)?;
        }
    }

    /// Map logical block `lblk` of an extent-mapped inode.
    pub(crate) fn ext_map(&mut self, ino: Ino, inode: &Inode, lblk: u32) -> Result<Mapping> {
        let mut next_key: u64 = 1 << 32;
        let mut node = self.ext_root(inode)?;
        let mut depth_guard = 0;
        loop {
            let h = node.header();
            let n = h.entries as usize;
            if n == 0 {
                return Ok(Mapping::Hole {
                    len: next_key - lblk as u64,
                });
            }
            let idx = node.search(lblk);
            if h.depth == 0 {
                let e = node.extent(idx);
                if e.contains(lblk) {
                    return Ok(Mapping::Mapped {
                        pblk: e.start + (lblk - e.block) as u64,
                        len: e.end() - lblk as u64,
                        unwritten: e.unwritten,
                    });
                }
                let next = if e.block > lblk {
                    e.block as u64
                } else if idx + 1 < n {
                    node.key(idx + 1) as u64
                } else {
                    next_key
                };
                return Ok(Mapping::Hole {
                    len: next.min(next_key) - lblk as u64,
                });
            }
            if node.key(idx) > lblk {
                // before the first subtree
                return Ok(Mapping::Hole {
                    len: node.key(idx) as u64 - lblk as u64,
                });
            }
            if idx + 1 < n {
                next_key = next_key.min(node.key(idx + 1) as u64);
            }
            let child = node.index(idx).leaf;
            depth_guard += 1;
            if depth_guard > MAX_DEPTH as usize + 1 {
                return Err(Error::corrupt("extent tree too deep"));
            }
            node = self.ext_read_node(ino, inode, child, h.depth - 1)?;
        }
    }

    /// All extents in logical order.
    pub(crate) fn ext_all(&mut self, ino: Ino, inode: &Inode) -> Result<Vec<Extent>> {
        let mut out = Vec::new();
        let root = self.ext_root(inode)?;
        self.ext_collect(ino, inode, &root, &mut out, 0)?;
        Ok(out)
    }

    fn ext_collect(&mut self, ino: Ino, inode: &Inode, node: &Node, out: &mut Vec<Extent>, level: usize) -> Result<()> {
        if level > MAX_DEPTH as usize {
            return Err(Error::corrupt("extent tree too deep"));
        }
        let h = node.header();
        for i in 0..h.entries as usize {
            if h.depth == 0 {
                out.push(node.extent(i));
            } else {
                let child = self.ext_read_node(ino, inode, node.index(i).leaf, h.depth - 1)?;
                self.ext_collect(ino, inode, &child, out, level + 1)?;
            }
        }
        Ok(())
    }

    /// Physical blocks used by an inode's extent tree nodes (not data).
    pub fn extent_tree_blocks(&mut self, ino: Ino) -> Result<Vec<u64>> {
        let inode = self.read_inode(ino)?;
        let inode = &inode;
        let mut out = Vec::new();
        if !inode.has_flag(crate::ondisk::inode::flags::EXTENTS) {
            return Ok(out);
        }
        let root = self.ext_root(inode)?;
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let h = node.header();
            if h.depth == 0 {
                continue;
            }
            for i in 0..h.entries as usize {
                let b = node.index(i).leaf;
                out.push(b);
                stack.push(self.ext_read_node(ino, inode, b, h.depth - 1)?);
            }
        }
        Ok(out)
    }

    /// Initialize an empty extent root in `inode`.
    pub(crate) fn ext_init_root(inode: &mut Inode) {
        let area = inode.block_area_mut();
        area.fill(0);
        ExtentHeader::empty(60, 0).write(area);
    }

    fn add_inode_blocks(&self, inode: &mut Inode, blocks: i64) {
        let per = self.bs as i64 / 512;
        let cur = inode.sectors(self.bs, self.huge_file()) as i64;
        inode.set_sectors((cur + blocks * per).max(0) as u64);
    }

    /// Allocation goal for blocks near logical block `lblk`.
    pub(crate) fn ext_goal(&mut self, ino: Ino, inode: &Inode, lblk: u32) -> Result<u64> {
        let path = self.ext_find_path(ino, inode, lblk)?;
        let (leaf, idx) = path.last().unwrap();
        if leaf.entries() > 0 {
            let e = leaf.extent(*idx);
            if lblk >= e.block {
                return Ok(e.start + (lblk - e.block) as u64);
            }
            return Ok(e.start.saturating_sub((e.block - lblk) as u64));
        }
        if let Some(b) = leaf.blk {
            return Ok(b + 1);
        }
        let ipg = self.sb.inodes_per_group();
        let g = (ino - 1) / ipg;
        Ok(self.group_first_block(g))
    }

    fn ext_fix_keys(&mut self, ino: Ino, inode: &mut Inode, path: &mut Path, level: usize) {
        let mut l = level;
        while l > 0 {
            if path[l].0.entries() == 0 {
                break;
            }
            let k = path[l].0.key(0);
            let pidx = path[l - 1].1;
            let mut ix = path[l - 1].0.index(pidx);
            if ix.block == k {
                break;
            }
            ix.block = k;
            path[l - 1].0.set_index(pidx, ix);
            let parent = path[l - 1].0.clone();
            self.ext_write_node(ino, inode, &parent);
            if pidx != 0 {
                break;
            }
            l -= 1;
        }
    }

    fn ext_new_block(&mut self, ino: Ino, inode: &mut Inode, near: Option<u64>) -> Result<u64> {
        let goal = match near {
            Some(g) => g,
            None => {
                let ipg = self.sb.inodes_per_group();
                self.group_first_block((ino - 1) / ipg)
            }
        };
        let (b, _) = self.alloc_blocks(goal, 1)?;
        self.add_inode_blocks(inode, 1);
        Ok(b)
    }

    /// Insert `e` (whose logical range must be unmapped) into the tree.
    pub(crate) fn ext_insert(&mut self, ino: Ino, inode: &mut Inode, e: Extent) -> Result<()> {
        debug_assert!(e.len > 0 && e.len <= e.max_len());
        let mut path = self.ext_find_path(ino, inode, e.block)?;
        let lvl = path.len() - 1;
        let leaf = &mut path[lvl].0;
        let n = leaf.entries();
        let pos = (0..n).find(|&i| leaf.extent(i).block > e.block).unwrap_or(n);
        if pos > 0 {
            let mut l = leaf.extent(pos - 1);
            if l.end() > e.block as u64 {
                return Err(Error::corrupt(format!(
                    "inode {ino}: inserting overlapping extent {e:?} after {l:?}"
                )));
            }
            if can_merge(&l, &e) {
                l.len += e.len;
                leaf.set_extent(pos - 1, l);
                if pos < n {
                    let r = leaf.extent(pos);
                    if can_merge(&l, &r) {
                        l.len += r.len;
                        leaf.set_extent(pos - 1, l);
                        leaf.remove_raw(pos);
                    }
                }
                let leaf = leaf.clone();
                self.ext_write_node(ino, inode, &leaf);
                return Ok(());
            }
        }
        if pos < n {
            let r = leaf.extent(pos);
            if e.end() > r.block as u64 {
                return Err(Error::corrupt(format!(
                    "inode {ino}: inserting overlapping extent {e:?} before {r:?}"
                )));
            }
            if can_merge(&e, &r) {
                let m = Extent {
                    block: e.block,
                    len: e.len + r.len,
                    start: e.start,
                    unwritten: e.unwritten,
                };
                leaf.set_extent(pos, m);
                let leaf = leaf.clone();
                self.ext_write_node(ino, inode, &leaf);
                if pos == 0 {
                    self.ext_fix_keys(ino, inode, &mut path, lvl);
                }
                return Ok(());
            }
        }
        self.ext_insert_at(ino, inode, &mut path, lvl, e.block, extent_raw(e))
    }

    fn ext_insert_at(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        path: &mut Path,
        level: usize,
        key: u32,
        raw: [u8; ENTRY_SIZE],
    ) -> Result<()> {
        let h = path[level].0.header();
        let n = h.entries as usize;
        let pos = {
            let node = &path[level].0;
            (0..n).find(|&i| node.key(i) > key).unwrap_or(n)
        };
        if n < h.max as usize {
            path[level].0.insert_raw(pos, raw);
            let node = path[level].0.clone();
            self.ext_write_node(ino, inode, &node);
            if pos == 0 && level > 0 {
                self.ext_fix_keys(ino, inode, path, level);
            }
            return Ok(());
        }
        if level == 0 {
            // Root full: move its entries into a new block, deepen the tree.
            if h.depth >= MAX_DEPTH {
                return Err(Error::NoSpace);
            }
            let near = path[0].0.blk.or_else(|| {
                if h.depth == 0 && n > 0 {
                    Some(path[0].0.extent(n - 1).start)
                } else {
                    None
                }
            });
            let nb = self.ext_new_block(ino, inode, near)?;
            let bs = self.bs as usize;
            let mut child = Node {
                blk: Some(nb),
                data: vec![0u8; bs],
            };
            child.set_header(ExtentHeader {
                magic: EXTENT_MAGIC,
                entries: n as u16,
                max: ex::max_entries(bs),
                depth: h.depth,
                generation: 0,
            });
            child.data[HEADER_SIZE..HEADER_SIZE + n * ENTRY_SIZE]
                .copy_from_slice(&path[0].0.data[HEADER_SIZE..HEADER_SIZE + n * ENTRY_SIZE]);
            self.ext_write_node(ino, inode, &child);
            let mut root = Node {
                blk: None,
                data: vec![0u8; 60],
            };
            root.set_header(ExtentHeader {
                magic: EXTENT_MAGIC,
                entries: 1,
                max: 4,
                depth: h.depth + 1,
                generation: h.generation,
            });
            root.set_index(
                0,
                ExtentIndex {
                    block: child.key(0),
                    leaf: nb,
                },
            );
            self.ext_write_node(ino, inode, &root);
            let old_idx = path[0].1;
            path[0] = (root, 0);
            path.insert(1, (child, old_idx));
            return self.ext_insert_at(ino, inode, path, 1, key, raw);
        }
        // Split this node.
        let near = path[level].0.blk;
        let nb = self.ext_new_block(ino, inode, near)?;
        let bs = self.bs as usize;
        let mut right = Node {
            blk: Some(nb),
            data: vec![0u8; bs],
        };
        right.set_header(ExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: ex::max_entries(bs),
            depth: h.depth,
            generation: 0,
        });
        let split = if pos == n { n } else { n / 2 };
        {
            let left = &mut path[level].0;
            for (j, i) in (split..n).enumerate() {
                let r = left.raw_entry(i);
                right.data[HEADER_SIZE + j * ENTRY_SIZE..HEADER_SIZE + (j + 1) * ENTRY_SIZE].copy_from_slice(&r);
            }
            right.set_entries(n - split);
            for i in split..n {
                let o = HEADER_SIZE + i * ENTRY_SIZE;
                left.data[o..o + ENTRY_SIZE].fill(0);
            }
            left.set_entries(split);
        }
        let inserted_left_front = if pos >= split {
            right.insert_raw(pos - split, raw);
            false
        } else {
            path[level].0.insert_raw(pos, raw);
            pos == 0
        };
        let left = path[level].0.clone();
        self.ext_write_node(ino, inode, &left);
        self.ext_write_node(ino, inode, &right);
        if inserted_left_front {
            self.ext_fix_keys(ino, inode, path, level);
        }
        let rkey = right.key(0);
        self.ext_insert_at(
            ino,
            inode,
            path,
            level - 1,
            rkey,
            index_raw(ExtentIndex { block: rkey, leaf: nb }),
        )
    }

    /// Remove entry `path[level].1`, collapsing empty nodes upward.
    fn ext_delete_entry(&mut self, ino: Ino, inode: &mut Inode, path: &mut Path, level: usize) -> Result<()> {
        let idx = path[level].1;
        path[level].0.remove_raw(idx);
        if path[level].0.entries() == 0 {
            if level > 0 {
                let b = path[level].0.blk.unwrap();
                self.free_blocks(b, 1)?;
                self.add_inode_blocks(inode, -1);
                return self.ext_delete_entry(ino, inode, path, level - 1);
            }
            let mut root = Node {
                blk: None,
                data: vec![0u8; 60],
            };
            root.set_header(ExtentHeader::empty(60, 0));
            self.ext_write_node(ino, inode, &root);
            path[0].0 = root;
            return Ok(());
        }
        let node = path[level].0.clone();
        self.ext_write_node(ino, inode, &node);
        if idx == 0 && level > 0 {
            self.ext_fix_keys(ino, inode, path, level);
        }
        Ok(())
    }

    /// Unmap logical blocks `[from, to)`. Returns the physical ranges that
    /// were mapped (the caller frees them). Tree node blocks that become
    /// empty are freed here.
    pub(crate) fn ext_remove_range(
        &mut self,
        ino: Ino,
        inode: &mut Inode,
        from: u32,
        to: u64,
    ) -> Result<Vec<(u64, u64)>> {
        let exts = self.ext_all(ino, inode)?;
        let mut removed = Vec::new();
        for e in exts {
            if (e.block as u64) >= to || e.end() <= from as u64 {
                continue;
            }
            let s = (e.block).max(from);
            let t = e.end().min(to);
            removed.push((e.start + (s - e.block) as u64, t - s as u64));
            let mut path = self.ext_find_path(ino, inode, e.block)?;
            let lvl = path.len() - 1;
            let idx = path[lvl].1;
            if path[lvl].0.entries() == 0 || path[lvl].0.extent(idx) != e {
                return Err(Error::corrupt(format!(
                    "inode {ino}: extent {e:?} vanished during removal"
                )));
            }
            if s == e.block && t == e.end() {
                self.ext_delete_entry(ino, inode, &mut path, lvl)?;
            } else if s == e.block {
                let ne = Extent {
                    block: t as u32,
                    len: (e.end() - t) as u32,
                    start: e.start + (t - e.block as u64),
                    unwritten: e.unwritten,
                };
                path[lvl].0.set_extent(idx, ne);
                let leaf = path[lvl].0.clone();
                self.ext_write_node(ino, inode, &leaf);
                if idx == 0 {
                    self.ext_fix_keys(ino, inode, &mut path, lvl);
                }
            } else if t == e.end() {
                let ne = Extent { len: s - e.block, ..e };
                path[lvl].0.set_extent(idx, ne);
                let leaf = path[lvl].0.clone();
                self.ext_write_node(ino, inode, &leaf);
            } else {
                let left = Extent { len: s - e.block, ..e };
                let right = Extent {
                    block: t as u32,
                    len: (e.end() - t) as u32,
                    start: e.start + (t - e.block as u64),
                    unwritten: e.unwritten,
                };
                path[lvl].0.set_extent(idx, left);
                let leaf = path[lvl].0.clone();
                self.ext_write_node(ino, inode, &leaf);
                self.ext_insert_at(ino, inode, &mut path, lvl, right.block, extent_raw(right))?;
            }
        }
        Ok(removed)
    }

    /// Convert unwritten extents in `[from, from+len)` to written.
    pub(crate) fn ext_mark_written(&mut self, ino: Ino, inode: &mut Inode, from: u32, len: u32) -> Result<()> {
        let to = from as u64 + len as u64;
        let exts = self.ext_all(ino, inode)?;
        for e in exts {
            if !e.unwritten || e.block as u64 >= to || e.end() <= from as u64 {
                continue;
            }
            let s = e.block.max(from);
            let t = e.end().min(to);
            let phys = e.start + (s - e.block) as u64;
            // unmap without freeing, then re-insert as written
            self.ext_remove_range(ino, inode, s, t)?;
            let mut off = 0u64;
            let total = t - s as u64;
            while off < total {
                let n = (total - off).min(ex::MAX_INIT_LEN as u64) as u32;
                self.ext_insert(
                    ino,
                    inode,
                    Extent {
                        block: s + off as u32,
                        len: n,
                        start: phys + off,
                        unwritten: false,
                    },
                )?;
                off += n as u64;
            }
        }
        Ok(())
    }

    /// Validate structural invariants of the whole tree (tests/fsck).
    pub fn check_extent_tree(&mut self, ino: Ino) -> Result<()> {
        let inode = self.read_inode(ino)?;
        let root = self.ext_root(&inode)?;
        self.check_node(ino, &inode, &root, 0, 1 << 32)?;
        let all = self.ext_all(ino, &inode)?;
        for w in all.windows(2) {
            if w[0].end() > w[1].block as u64 {
                return Err(Error::corrupt(format!("overlapping extents {:?} {:?}", w[0], w[1])));
            }
        }
        Ok(())
    }

    fn check_node(&mut self, ino: Ino, inode: &Inode, node: &Node, lo: u64, hi: u64) -> Result<()> {
        let h = node.header();
        let n = h.entries as usize;
        for i in 0..n {
            let k = node.key(i) as u64;
            if k < lo || k >= hi {
                return Err(Error::corrupt(format!("key {k} outside [{lo},{hi})")));
            }
            if i > 0 && node.key(i - 1) >= node.key(i) {
                return Err(Error::corrupt("unsorted extent node"));
            }
            if h.depth > 0 {
                let child = self.ext_read_node(ino, inode, node.index(i).leaf, h.depth - 1)?;
                if child.entries() == 0 {
                    return Err(Error::corrupt("empty child node"));
                }
                if child.key(0) as u64 != k {
                    return Err(Error::corrupt(format!(
                        "index key {k} != child first key {}",
                        child.key(0)
                    )));
                }
                let chi = if i + 1 < n { node.key(i + 1) as u64 } else { hi };
                self.check_node(ino, inode, &child, k, chi)?;
            } else {
                let e = node.extent(i);
                if e.len == 0 || e.end() > hi {
                    return Err(Error::corrupt(format!("bad extent {e:?}")));
                }
            }
        }
        Ok(())
    }
}
