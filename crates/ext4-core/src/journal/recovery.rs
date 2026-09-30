//! Journal recovery: scan, revoke and replay passes.

use super::Journal;
use super::format::*;
use crate::device::BlockDevice;
use crate::error::Result;
use std::collections::{BTreeMap, HashMap};

/// Outcome of scanning the log.
#[derive(Debug, Default)]
pub struct RecoveryPlan {
    /// Final contents per home block, in replay order resolved.
    pub blocks: BTreeMap<u64, Vec<u8>>,
    /// Committed transactions found.
    pub transactions: u32,
    /// Sequence number to continue with.
    pub next_sequence: u32,
    /// Data blocks skipped because their tag checksum did not match.
    pub bad_blocks: u32,
    /// Blocks skipped because a later revoke record covers them.
    pub revoked: u32,
}

struct PendingTxn {
    seq: u32,
    /// (home block, journal block, tag)
    blocks: Vec<(u64, u32, Tag)>,
    revokes: Vec<u64>,
}

impl Journal {
    fn next_log_block(&self, b: u32) -> u32 {
        let n = b + 1;
        if n >= self.sb.max_len() { self.sb.first() } else { n }
    }

    /// Scan the log and compute the blocks to replay without writing.
    pub fn plan_recovery(&self, dev: &dyn BlockDevice) -> Result<RecoveryPlan> {
        let mut plan = RecoveryPlan {
            next_sequence: self.sb.sequence(),
            ..Default::default()
        };
        if self.sb.start() == 0 {
            return Ok(plan);
        }
        let bs = self.block_size;
        let csum = self.sb.has_csum_v2v3();
        let seed = self.sb.csum_seed();
        let mut seq = self.sb.sequence();
        let mut blk = self.sb.start();
        let mut buf = vec![0u8; bs];
        let mut committed: Vec<PendingTxn> = Vec::new();
        let mut cur = PendingTxn {
            seq,
            blocks: Vec::new(),
            revokes: Vec::new(),
        };
        let limit = self.sb.max_len() as usize * 2;
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > limit {
                break;
            }
            self.read_block(dev, blk, &mut buf)?;
            let h = read_header(&buf);
            if h.magic != JBD2_MAGIC || h.sequence != seq {
                break;
            }
            match h.blocktype {
                JBD2_DESCRIPTOR_BLOCK => {
                    if csum && !verify_descriptor_tail(seed, &buf) {
                        break;
                    }
                    let tags = parse_descriptor(&buf, &self.sb);
                    for t in tags {
                        blk = self.next_log_block(blk);
                        cur.blocks.push((t.blocknr, blk, t));
                    }
                }
                JBD2_REVOKE_BLOCK => {
                    if csum && !verify_descriptor_tail(seed, &buf) {
                        break;
                    }
                    match parse_revoke(&buf, &self.sb) {
                        Ok(r) => cur.revokes.extend(r),
                        Err(_) => break,
                    }
                }
                JBD2_COMMIT_BLOCK => {
                    if csum && !verify_commit_checksum(seed, &buf) {
                        break;
                    }
                    committed.push(std::mem::replace(
                        &mut cur,
                        PendingTxn {
                            seq: seq.wrapping_add(1),
                            blocks: Vec::new(),
                            revokes: Vec::new(),
                        },
                    ));
                    seq = seq.wrapping_add(1);
                }
                _ => break,
            }
            blk = self.next_log_block(blk);
        }

        // revoke pass: newest revoking transaction per block
        let mut revoked: HashMap<u64, u32> = HashMap::new();
        for t in &committed {
            for &b in &t.revokes {
                let e = revoked.entry(b).or_insert(t.seq);
                if seq_after(t.seq, *e) {
                    *e = t.seq;
                }
            }
        }

        // replay pass
        for t in &committed {
            for &(home, jblk, tag) in &t.blocks {
                if let Some(&rseq) = revoked.get(&home)
                    && !seq_after(t.seq, rseq)
                {
                    plan.revoked += 1;
                    continue;
                }
                let mut data = vec![0u8; bs];
                self.read_block(dev, jblk, &mut data)?;
                if tag.flags & JBD2_FLAG_ESCAPE != 0 {
                    crate::bytes::set_be32(&mut data, 0, JBD2_MAGIC);
                }
                if !tag_checksum_matches(&self.sb, t.seq, &data, tag.checksum) {
                    plan.bad_blocks += 1;
                    continue;
                }
                plan.blocks.insert(home, data);
            }
        }
        plan.transactions = committed.len() as u32;
        plan.next_sequence = seq.wrapping_add(1);
        Ok(plan)
    }

    /// Replay the log onto the device and mark the journal empty.
    pub fn recover(&mut self, dev: &dyn BlockDevice) -> Result<RecoveryPlan> {
        let plan = self.plan_recovery(dev)?;
        let bs = self.block_size as u64;
        for (home, data) in &plan.blocks {
            dev.write_at(home * bs, data)?;
        }
        dev.flush()?;
        self.reset(dev, plan.next_sequence)?;
        Ok(plan)
    }
}

/// `tid_gt(a, b)` with wrap-around.
fn seq_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[cfg(test)]
mod tests {
    use super::seq_after;

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_after(2, 1));
        assert!(!seq_after(1, 2));
        assert!(!seq_after(5, 5));
        assert!(seq_after(0, u32::MAX));
        assert!(!seq_after(u32::MAX, 0));
    }
}
