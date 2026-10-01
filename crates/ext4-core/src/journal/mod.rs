//! jbd2 journal: superblock, recovery and a simple commit writer.
//!
//! Writer protocol (every commit leaves the journal empty):
//! 1. flush (ordered-mode data is durable)
//! 2. journal superblock: `s_start = s_first`, `s_sequence = N`; flush
//! 3. descriptor blocks + metadata copies from `s_first`; flush
//! 4. commit block; flush — transaction N is now durable
//! 5. checkpoint: write metadata to its home location; flush
//! 6. journal superblock: `s_start = 0`, `s_sequence = N + 1`
//!
//! A crash at any point leaves either the old state (no valid commit block)
//! or a committed transaction that replays idempotently.

pub mod format;
pub mod recovery;

use crate::bytes::{be32, set_be32, set_be64};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use format::*;

/// Physical layout of the journal inode: (logical start, physical start, len).
#[derive(Clone, Debug, Default)]
pub struct JournalMap {
    pub runs: Vec<(u32, u64, u32)>,
}

impl JournalMap {
    pub fn map(&self, lblk: u32) -> Option<u64> {
        let i = self.runs.partition_point(|r| r.0 <= lblk);
        if i == 0 {
            return None;
        }
        let (l, p, n) = self.runs[i - 1];
        if lblk < l + n {
            Some(p + (lblk - l) as u64)
        } else {
            None
        }
    }

    pub fn total(&self) -> u64 {
        self.runs.iter().map(|r| r.2 as u64).sum()
    }
}

pub struct Journal {
    pub map: JournalMap,
    pub sb: JournalSuperblock,
    pub block_size: usize,
    /// Sequence number the next commit will use.
    pub sequence: u32,
    pub commits: u64,
    /// Next free log block while committed transactions wait for a
    /// checkpoint; `None` while the log is empty.
    head: Option<u32>,
}

impl Journal {
    pub fn load(dev: &dyn BlockDevice, map: JournalMap, fs_block_size: usize) -> Result<Journal> {
        let p0 = map
            .map(0)
            .ok_or_else(|| Error::corrupt("journal inode has no block 0"))?;
        let mut raw = vec![0u8; fs_block_size];
        dev.read_at(p0 * fs_block_size as u64, &mut raw)?;
        let sb = JournalSuperblock::parse(&raw)?;
        if sb.block_size() as usize != fs_block_size {
            return Err(Error::unsupported(format!(
                "journal block size {} != fs block size {}",
                sb.block_size(),
                fs_block_size
            )));
        }
        if (sb.max_len() as u64) > map.total() {
            return Err(Error::corrupt("journal inode shorter than s_maxlen"));
        }
        if sb.first() == 0 || sb.first() >= sb.max_len() {
            return Err(Error::corrupt("journal s_first out of range"));
        }
        let sequence = sb.sequence();
        Ok(Journal {
            map,
            sb,
            block_size: fs_block_size,
            sequence,
            commits: 0,
            head: None,
        })
    }

    /// Journal features we can't handle for writing.
    pub fn unsupported_incompat(&self) -> u32 {
        self.sb.feature_incompat()
            & !(JBD2_FEATURE_INCOMPAT_REVOKE
                | JBD2_FEATURE_INCOMPAT_64BIT
                | JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT
                | JBD2_FEATURE_INCOMPAT_CSUM_V2
                | JBD2_FEATURE_INCOMPAT_CSUM_V3
                | JBD2_FEATURE_INCOMPAT_FAST_COMMIT)
    }

    pub fn needs_recovery(&self) -> bool {
        self.sb.start() != 0
    }

    fn phys(&self, jblk: u32) -> Result<u64> {
        self.map
            .map(jblk)
            .ok_or_else(|| Error::corrupt(format!("journal block {jblk} unmapped")))
    }

    pub fn read_block(&self, dev: &dyn BlockDevice, jblk: u32, buf: &mut [u8]) -> Result<()> {
        dev.read_at(self.phys(jblk)? * self.block_size as u64, buf)
    }

    fn write_sb(&mut self, dev: &dyn BlockDevice) -> Result<()> {
        self.sb.update_checksum();
        let p0 = self.phys(0)?;
        let mut blk = vec![0u8; self.block_size];
        dev.read_at(p0 * self.block_size as u64, &mut blk)?;
        blk[..JSB_SIZE].copy_from_slice(&self.sb.raw[..]);
        dev.write_at(p0 * self.block_size as u64, &blk)
    }

    /// Mark the journal empty with the given next sequence (used after
    /// recovery).
    pub fn reset(&mut self, dev: &dyn BlockDevice, next_seq: u32) -> Result<()> {
        self.sb.set_start(0);
        self.sb.set_sequence(next_seq);
        self.sequence = next_seq;
        self.head = None;
        self.write_sb(dev)?;
        dev.flush()
    }

    /// Whether committed transactions wait for a checkpoint.
    pub fn has_pending_checkpoint(&self) -> bool {
        self.head.is_some()
    }

    /// Log blocks in use by transactions waiting for a checkpoint.
    pub fn used(&self) -> u32 {
        self.head.map_or(0, |h| h - self.sb.first())
    }

    /// Whether a transaction of `n` metadata blocks fits in the space left
    /// after the transactions already in the log.
    pub fn fits_now(&self, n: usize) -> bool {
        let head = self.head.unwrap_or(self.sb.first());
        self.blocks_needed(n) <= self.log_end().saturating_sub(head) as usize
    }

    /// The caller wrote every logged block home: mark the log empty.
    pub fn mark_checkpointed(&mut self, dev: &dyn BlockDevice) -> Result<()> {
        if self.head.is_none() {
            return Ok(());
        }
        self.sb.set_start(0);
        self.sb.set_sequence(self.sequence);
        self.write_sb(dev)?;
        dev.flush()?;
        self.head = None;
        Ok(())
    }

    /// One past the last block of the regular log. With fast commits the
    /// tail of the journal (`s_num_fc_blks`, default 256) is reserved.
    pub fn log_end(&self) -> u32 {
        if self.sb.has_incompat(JBD2_FEATURE_INCOMPAT_FAST_COMMIT) {
            let fc = match self.sb.num_fc_blocks() {
                0 => 256,
                n => n,
            };
            self.sb.max_len().saturating_sub(fc).max(self.sb.first() + 1)
        } else {
            self.sb.max_len()
        }
    }

    /// Usable log blocks per transaction.
    pub fn capacity(&self) -> u32 {
        self.log_end() - self.sb.first()
    }

    /// Turn on journal features the Linux kernel enables when mounting
    /// this file system read-write (64-bit block numbers for 64bit file
    /// systems, checksum v3 with metadata_csum). Only valid while the
    /// journal is empty.
    pub fn enable_features(&mut self, dev: &dyn BlockDevice, bit64: bool, csum_v3: bool) -> Result<()> {
        if self.needs_recovery() {
            return Err(Error::Busy);
        }
        let mut inc = self.sb.feature_incompat();
        let mut compat = self.sb.feature_compat();
        if bit64 {
            inc |= JBD2_FEATURE_INCOMPAT_64BIT;
        }
        if csum_v3 {
            inc = (inc & !JBD2_FEATURE_INCOMPAT_CSUM_V2) | JBD2_FEATURE_INCOMPAT_CSUM_V3;
            compat &= !JBD2_FEATURE_COMPAT_CHECKSUM;
        }
        if inc == self.sb.feature_incompat() && compat == self.sb.feature_compat() {
            return Ok(());
        }
        if self.sb.blocktype() == JBD2_SUPERBLOCK_V1 {
            // v1 superblocks have no feature fields
            return Ok(());
        }
        self.sb.set_feature_incompat(inc);
        self.sb.set_feature_compat(compat);
        if self.sb.has_csum_v2v3() {
            self.sb.raw[0x50] = JBD2_CRC32C_CHKSUM;
        }
        self.write_sb(dev)?;
        dev.flush()
    }

    fn tags_per_descriptor(&self) -> usize {
        let tb = self.sb.tag_bytes();
        let tail = if self.sb.has_csum_v2v3() { 4 } else { 0 };
        // the first tag is followed by a 16 byte UUID
        (self.block_size - JOURNAL_HEADER_SIZE - tail - 16) / tb
    }

    /// Journal blocks needed to log `n` metadata blocks.
    pub fn blocks_needed(&self, n: usize) -> usize {
        n + n.div_ceil(self.tags_per_descriptor()) + 1
    }

    pub fn fits(&self, n: usize) -> bool {
        self.blocks_needed(n) <= self.capacity() as usize
    }

    /// Log `blocks` (home block number → contents), then checkpoint them
    /// right away.
    pub fn commit(&mut self, dev: &dyn BlockDevice, blocks: &[(u64, &[u8])]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        self.append(dev, blocks)?;
        for (home, data) in blocks {
            dev.write_at(home * self.block_size as u64, data)?;
        }
        dev.flush()?;
        self.mark_checkpointed(dev)
    }

    /// Append a transaction (home block number → contents) to the log, like
    /// jbd2: the journal superblock is written only when the log was empty,
    /// so a commit is a few sequential writes. The blocks stay in the log
    /// until the caller has written them home and calls
    /// [`Journal::mark_checkpointed`]; recovery replays every transaction
    /// still in the log.
    pub fn append(&mut self, dev: &dyn BlockDevice, blocks: &[(u64, &[u8])]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        if !self.fits_now(blocks.len()) {
            return Err(Error::TooBig);
        }
        if !self.sb.is_64bit() && blocks.iter().any(|(h, _)| *h > u32::MAX as u64) {
            return Err(Error::invalid("block number needs a 64-bit journal"));
        }
        let bs = self.block_size;
        // A sequence number is never reused, even if this commit fails part
        // way: a retry must not be confused with an earlier durable one.
        let seq = self.sequence;
        self.sequence = seq.wrapping_add(1);
        let csum = self.sb.has_csum_v2v3();
        let v3 = self.sb.has_csum_v3();
        let seed = self.sb.csum_seed();
        let tag_bytes = self.sb.tag_bytes();
        let per_desc = self.tags_per_descriptor();
        let uuid = self.sb.uuid();

        // Build the log image: descriptor, data..., descriptor, data..., commit
        let mut log: Vec<u8> = Vec::with_capacity(self.blocks_needed(blocks.len()) * bs);
        for chunk in blocks.chunks(per_desc) {
            let mut desc = vec![0u8; bs];
            write_header(&mut desc, JBD2_DESCRIPTOR_BLOCK, seq);
            let mut off = JOURNAL_HEADER_SIZE;
            let mut datas: Vec<Vec<u8>> = Vec::with_capacity(chunk.len());
            for (i, (home, data)) in chunk.iter().enumerate() {
                let mut d = data.to_vec();
                let mut flags = 0u32;
                if be32(&d, 0) == JBD2_MAGIC {
                    d[..4].fill(0);
                    flags |= JBD2_FLAG_ESCAPE;
                }
                if i > 0 {
                    flags |= JBD2_FLAG_SAME_UUID;
                }
                if i == chunk.len() - 1 {
                    flags |= JBD2_FLAG_LAST_TAG;
                }
                let tag_csum = if csum {
                    // like jbd2: checksum the block as stored in the log
                    // (after escaping)
                    tag_checksum(seed, seq, &d)
                } else {
                    0
                };
                write_tag(&mut desc[off..off + tag_bytes], &self.sb, *home, flags, tag_csum, v3);
                off += tag_bytes;
                if i == 0 {
                    desc[off..off + 16].copy_from_slice(&uuid);
                    off += 16;
                }
                datas.push(d);
            }
            if csum {
                set_descriptor_tail(seed, &mut desc);
            }
            log.extend_from_slice(&desc);
            for d in datas {
                log.extend_from_slice(&d);
            }
        }
        let data_blocks = (log.len() / bs) as u32;

        // 1. ordered data durable
        dev.flush()?;
        // 2. an empty log: the journal superblock points at this transaction
        let first = match self.head {
            Some(h) => h,
            None => {
                let first = self.sb.first();
                self.sb.set_start(first);
                self.sb.set_sequence(seq);
                self.write_sb(dev)?;
                dev.flush()?;
                first
            }
        };
        // 3. log blocks
        self.write_log(dev, first, &log)?;
        dev.flush()?;
        // 4. commit block
        let mut commit = vec![0u8; bs];
        write_header(&mut commit, JBD2_COMMIT_BLOCK, seq);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        set_be64(&mut commit, 0x30, now.as_secs());
        set_be32(&mut commit, 0x38, now.subsec_nanos());
        if csum {
            set_commit_checksum(seed, &mut commit);
        }
        self.write_log(dev, first + data_blocks, &commit)?;
        dev.flush()?;
        self.head = Some(first + data_blocks + 1);
        self.commits += 1;
        Ok(())
    }

    /// Write `data` (whole blocks) at log position `jblk`, coalescing
    /// physically contiguous runs into single writes.
    fn write_log(&self, dev: &dyn BlockDevice, jblk: u32, data: &[u8]) -> Result<()> {
        let bs = self.block_size;
        let n = (data.len() / bs) as u32;
        let mut i = 0u32;
        while i < n {
            let p = self.phys(jblk + i)?;
            let mut run = 1u32;
            while i + run < n && self.phys(jblk + i + run)? == p + run as u64 {
                run += 1;
            }
            let s = i as usize * bs;
            let e = (i + run) as usize * bs;
            dev.write_at(p * bs as u64, &data[s..e])?;
            i += run;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
