//! The "system zone": blocks holding file system metadata (superblock
//! copies, descriptor tables, bitmaps, inode tables, the journal). File
//! data, directory blocks and tree nodes must never point into it, and the
//! allocator must never hand it out (`ext4_inode_block_valid` in Linux).

use super::{Fs, Ino};
use crate::error::{Error, Result};
use crate::ondisk::inode::RESIZE_INO;

#[derive(Clone, Debug, Default)]
pub(crate) struct SystemZone {
    /// Sorted, merged, non-overlapping `[start, end)` block ranges.
    ranges: Vec<(u64, u64)>,
}

impl SystemZone {
    /// Build from metadata ranges that must not overlap each other
    /// (adjacent ranges are fine); an overlap means two structures share
    /// blocks, which Linux rejects as corruption (`add_system_zone`).
    pub(crate) fn from_disjoint(mut v: Vec<(u64, u64, &'static str)>) -> Result<SystemZone> {
        v.retain(|r| r.1 > r.0);
        v.sort_unstable();
        for w in v.windows(2) {
            if w[1].0 < w[0].1 {
                return Err(Error::corrupt(format!(
                    "{} (blocks {}..{}) overlaps {} (blocks {}..{})",
                    w[1].2, w[1].0, w[1].1, w[0].2, w[0].0, w[0].1
                )));
            }
        }
        Ok(SystemZone::from_ranges(v.into_iter().map(|r| (r.0, r.1)).collect()))
    }

    pub(crate) fn from_ranges(mut v: Vec<(u64, u64)>) -> SystemZone {
        v.retain(|r| r.1 > r.0);
        v.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
        for (s, e) in v {
            match out.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        SystemZone { ranges: out }
    }

    /// Whether `[start, start+len)` intersects the zone.
    pub(crate) fn overlaps(&self, start: u64, len: u64) -> bool {
        let end = start.saturating_add(len);
        // first range ending after `start`
        let i = self.ranges.partition_point(|r| r.1 <= start);
        i < self.ranges.len() && self.ranges[i].0 < end
    }

    /// Number of blocks in the zone (the file system's metadata overhead).
    pub(crate) fn total(&self) -> u64 {
        self.ranges.iter().map(|r| r.1 - r.0).sum()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ranges.len()
    }
}

impl Fs {
    /// Build the zone from the group descriptors and the journal. Fails if
    /// two structures share blocks (e.g. two groups claiming the same
    /// inode table, or a bitmap inside the descriptor table), which would
    /// otherwise let one overwrite the other.
    pub(crate) fn build_system_zone(&mut self) -> Result<()> {
        let mut v = Vec::new();
        let first = self.sb.first_data_block() as u64;
        if first > 0 {
            // 1K blocks: block 0 (boot block) precedes group 0
            v.push((0, first, "boot block"));
        }
        for g in 0..self.group_count() {
            let start = self.group_first_block(g);
            v.push((start, start + self.base_meta_blocks(g) as u64, "superblock/descriptors"));
            let gd = &self.groups[g as usize];
            v.push((gd.block_bitmap(), gd.block_bitmap() + 1, "block bitmap"));
            v.push((gd.inode_bitmap(), gd.inode_bitmap() + 1, "inode bitmap"));
            v.push((
                gd.inode_table(),
                gd.inode_table() + self.itb_per_group as u64,
                "inode table",
            ));
        }
        if let Some(j) = &self.journal {
            for &(_, p, n) in &j.map.runs {
                v.push((p, p + n as u64, "journal"));
            }
        }
        self.zone = SystemZone::from_disjoint(v)?;
        Ok(())
    }

    /// Inodes whose blocks legitimately live in the system zone.
    fn zone_exempt(&self, ino: Ino) -> bool {
        ino == RESIZE_INO || self.journal.is_some() && ino == self.sb.journal_inum()
    }

    /// Reject a mapping of `ino` that points into metadata.
    pub(crate) fn check_data_blocks(&self, ino: Ino, start: u64, len: u64) -> Result<()> {
        if !self.zone_exempt(ino) && self.zone.overlaps(start, len) {
            return Err(Error::corrupt(format!(
                "inode {ino}: blocks {start}+{len} overlap file system metadata"
            )));
        }
        Ok(())
    }

    /// Reject a metadata block (tree node, indirect or xattr block)
    /// pointing into the system zone.
    pub(crate) fn check_meta_block(&self, what: &str, blk: u64) -> Result<()> {
        if blk < self.sb.first_data_block() as u64 || blk >= self.sb.blocks_count() {
            return Err(Error::corrupt(format!("{what} at block {blk} out of range")));
        }
        if self.zone.overlaps(blk, 1) {
            return Err(Error::corrupt(format!(
                "{what} at block {blk} overlaps file system metadata"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_query() {
        let z = SystemZone::from_ranges(vec![(10, 20), (0, 1), (15, 25), (30, 31), (40, 40)]);
        assert_eq!(z.len(), 3);
        assert!(z.overlaps(0, 1));
        assert!(!z.overlaps(1, 9));
        assert!(z.overlaps(5, 6));
        assert!(z.overlaps(24, 10));
        assert!(!z.overlaps(25, 5));
        assert!(z.overlaps(30, 1));
        assert!(!z.overlaps(31, 100));
        assert!(!z.overlaps(40, 1));
        assert!(!z.overlaps(u64::MAX - 1, 10));
        assert!(!SystemZone::default().overlaps(0, u64::MAX));
    }

    #[test]
    fn disjoint_ranges_reject_overlaps() {
        let ok = SystemZone::from_disjoint(vec![(10, 20, "a"), (20, 30, "b"), (0, 1, "c"), (5, 5, "empty")]).unwrap();
        assert_eq!(ok.len(), 2, "adjacent ranges merge");
        assert!(ok.overlaps(19, 2));
        let err = SystemZone::from_disjoint(vec![(10, 20, "inode table"), (15, 16, "block bitmap")]).unwrap_err();
        assert!(err.to_string().contains("block bitmap"), "{err}");
        // a range swallowing a later one
        assert!(SystemZone::from_disjoint(vec![(0, 100, "a"), (50, 60, "b"), (200, 300, "c")]).is_err());
        // identical ranges
        assert!(SystemZone::from_disjoint(vec![(7, 8, "a"), (7, 8, "b")]).is_err());
    }
}
