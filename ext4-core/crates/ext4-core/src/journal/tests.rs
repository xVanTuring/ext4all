use super::*;
use crate::device::MemDevice;

const BS: usize = 1024;
/// Journal occupies device blocks 100..100+JLEN.
const JSTART: u64 = 100;
const JLEN: u32 = 64;

fn make_jsb(incompat: u32) -> JournalSuperblock {
    make_jsb_len(incompat, JLEN)
}

fn make_jsb_len(incompat: u32, max_len: u32) -> JournalSuperblock {
    let mut raw = [0u8; JSB_SIZE];
    set_be32(&mut raw, 0, JBD2_MAGIC);
    set_be32(&mut raw, 4, JBD2_SUPERBLOCK_V2);
    let mut sb = JournalSuperblock::parse(&raw).unwrap();
    sb.set_block_size(BS as u32);
    sb.set_max_len(max_len);
    sb.set_first(1);
    sb.set_sequence(7);
    sb.set_start(0);
    sb.set_feature_incompat(incompat);
    sb.raw[0x30..0x40].copy_from_slice(&[9u8; 16]);
    sb.raw[0x50] = JBD2_CRC32C_CHKSUM;
    sb.set_nr_users(1);
    sb.update_checksum();
    sb
}

fn setup(incompat: u32) -> (MemDevice, Journal) {
    setup_with_map(
        incompat,
        JournalMap {
            runs: vec![(0, JSTART, JLEN)],
        },
    )
}

fn setup_with_map(incompat: u32, map: JournalMap) -> (MemDevice, Journal) {
    let dev = MemDevice::new(512 * BS);
    let sb = make_jsb_len(incompat, map.total() as u32);
    let p0 = map.map(0).unwrap();
    dev.write_at(p0 * BS as u64, &sb.raw[..]).unwrap();
    let j = Journal::load(&dev, map, BS).unwrap();
    (dev, j)
}

fn block_of(byte: u8) -> Vec<u8> {
    vec![byte; BS]
}

fn read_home(dev: &MemDevice, b: u64) -> Vec<u8> {
    let mut v = vec![0u8; BS];
    dev.read_at(b * BS as u64, &mut v).unwrap();
    v
}

const V3: u32 = JBD2_FEATURE_INCOMPAT_CSUM_V3 | JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_REVOKE;
const V2: u32 = JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_REVOKE;
const PLAIN32: u32 = JBD2_FEATURE_INCOMPAT_REVOKE;
const PLAIN64: u32 = JBD2_FEATURE_INCOMPAT_REVOKE | JBD2_FEATURE_INCOMPAT_64BIT;

#[test]
fn map_lookup() {
    let m = JournalMap {
        runs: vec![(0, 100, 10), (10, 500, 5), (20, 900, 2)],
    };
    assert_eq!(m.map(0), Some(100));
    assert_eq!(m.map(9), Some(109));
    assert_eq!(m.map(10), Some(500));
    assert_eq!(m.map(14), Some(504));
    assert_eq!(m.map(15), None);
    assert_eq!(m.map(21), Some(901));
    assert_eq!(m.map(22), None);
    assert_eq!(m.total(), 17);
    assert_eq!(JournalMap::default().map(0), None);
}

#[test]
fn tag_sizes() {
    assert_eq!(make_jsb(V3).tag_bytes(), 16);
    assert_eq!(make_jsb(JBD2_FEATURE_INCOMPAT_CSUM_V3).tag_bytes(), 16);
    assert_eq!(make_jsb(V2).tag_bytes(), 10);
    assert_eq!(make_jsb(V2 | JBD2_FEATURE_INCOMPAT_64BIT).tag_bytes(), 14);
    assert_eq!(make_jsb(PLAIN32).tag_bytes(), 8);
    assert_eq!(make_jsb(PLAIN64).tag_bytes(), 12);
}

#[test]
fn jsb_checksum_verified() {
    let sb = make_jsb(V3);
    let mut raw = sb.raw.to_vec();
    assert!(JournalSuperblock::parse(&raw).is_ok());
    raw[0x100] ^= 1;
    assert!(matches!(JournalSuperblock::parse(&raw), Err(Error::Checksum(_))));
    // no checksum feature: not verified
    let sb = make_jsb(PLAIN32);
    let mut raw = sb.raw.to_vec();
    raw[0x100] ^= 1;
    assert!(JournalSuperblock::parse(&raw).is_ok());
}

#[test]
fn jsb_rejects_garbage() {
    assert!(JournalSuperblock::parse(&[0u8; 1024]).is_err());
    assert!(JournalSuperblock::parse(&[0u8; 10]).is_err());
    let mut raw = make_jsb(PLAIN32).raw.to_vec();
    set_be32(&mut raw, 4, 1);
    assert!(JournalSuperblock::parse(&raw).is_err());
}

#[test]
fn v1_superblock_ignores_features() {
    let mut raw = make_jsb(V3).raw.to_vec();
    set_be32(&mut raw, 4, JBD2_SUPERBLOCK_V1);
    let sb = JournalSuperblock::parse(&raw).unwrap();
    assert_eq!(sb.feature_incompat(), 0);
}

fn commit_and_check(incompat: u32) {
    let (dev, mut j) = setup(incompat);
    let a = block_of(0xAA);
    let b = block_of(0xBB);
    j.commit(&dev, &[(10, &a), (11, &b)]).unwrap();
    assert_eq!(read_home(&dev, 10), a);
    assert_eq!(read_home(&dev, 11), b);
    assert_eq!(j.sequence, 8);
    assert_eq!(j.sb.start(), 0);
    // reload from disk: empty journal with next sequence
    let j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    assert_eq!(j2.sb.start(), 0);
    assert_eq!(j2.sb.sequence(), 8);
    assert!(!j2.needs_recovery());
}

#[test]
fn commit_v3() {
    commit_and_check(V3);
}

#[test]
fn commit_v2() {
    commit_and_check(V2);
}

#[test]
fn commit_plain() {
    commit_and_check(PLAIN32);
    commit_and_check(PLAIN64);
}

/// Simulate a crash right after the commit block became durable (before
/// checkpoint) by replaying the log of a commit whose home writes we undo.
fn crash_after_commit(incompat: u32) {
    let (dev, mut j) = setup(incompat);
    let blocks: Vec<(u64, Vec<u8>)> = (0..5).map(|i| (20 + i as u64, block_of(i as u8 + 1))).collect();
    let refs: Vec<(u64, &[u8])> = blocks.iter().map(|(h, d)| (*h, &d[..])).collect();
    // Let everything through the commit block, then fail: writes are
    // jsb(1) + log(1 desc + 5 data, one write) + commit(1) = 3 writes.
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &refs).is_err());
    dev.fail_writes_after(None);
    for (h, _) in &blocks {
        assert_eq!(read_home(&dev, *h), vec![0u8; BS]);
    }
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    assert!(j2.needs_recovery());
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.transactions, 1);
    assert_eq!(plan.bad_blocks, 0);
    for (h, d) in &blocks {
        assert_eq!(&read_home(&dev, *h), d);
    }
    assert!(!j2.needs_recovery());
    assert_eq!(j2.sequence, 9);
    let j3 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    assert!(!j3.needs_recovery());
}

#[test]
fn crash_after_commit_v3() {
    crash_after_commit(V3);
}

#[test]
fn crash_after_commit_v2() {
    crash_after_commit(V2);
}

#[test]
fn crash_after_commit_plain() {
    crash_after_commit(PLAIN32);
    crash_after_commit(PLAIN64);
}

/// Crash at every possible write: the result must be all-old or all-new.
fn crash_everywhere(incompat: u32) {
    for fail_at in 0..12 {
        let (dev, mut j) = setup(incompat);
        // first transaction completes
        let old: Vec<(u64, Vec<u8>)> = (0..3).map(|i| (30 + i as u64, block_of(0x10 + i as u8))).collect();
        let refs: Vec<(u64, &[u8])> = old.iter().map(|(h, d)| (*h, &d[..])).collect();
        j.commit(&dev, &refs).unwrap();
        // second transaction crashes at `fail_at`
        let new: Vec<(u64, Vec<u8>)> = (0..3).map(|i| (30 + i as u64, block_of(0x80 + i as u8))).collect();
        let refs: Vec<(u64, &[u8])> = new.iter().map(|(h, d)| (*h, &d[..])).collect();
        dev.fail_writes_after(Some(fail_at));
        let res = j.commit(&dev, &refs);
        dev.fail_writes_after(None);
        let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
        j2.recover(&dev).unwrap();
        let state: Vec<Vec<u8>> = (0..3).map(|i| read_home(&dev, 30 + i)).collect();
        let all_old = state.iter().zip(&old).all(|(s, (_, d))| s == d);
        let all_new = state.iter().zip(&new).all(|(s, (_, d))| s == d);
        assert!(all_old || all_new, "fail_at={fail_at}: torn state");
        if res.is_ok() {
            assert!(all_new, "fail_at={fail_at}");
        }
    }
}

#[test]
fn crash_everywhere_v3() {
    crash_everywhere(V3);
}

#[test]
fn crash_everywhere_plain() {
    crash_everywhere(PLAIN32);
}

#[test]
fn escaped_blocks_roundtrip() {
    let (dev, mut j) = setup(V3);
    let mut magic_block = block_of(0x42);
    set_be32(&mut magic_block, 0, JBD2_MAGIC);
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &[(40, &magic_block)]).is_err());
    dev.fail_writes_after(None);
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.bad_blocks, 0);
    assert_eq!(read_home(&dev, 40), magic_block);
}

#[test]
fn many_blocks_use_multiple_descriptors() {
    let (dev, mut j) = setup_with_map(
        V3,
        JournalMap {
            runs: vec![(0, JSTART, 160)],
        },
    );
    let per = j.tags_per_descriptor();
    let n = per + 5;
    assert!(j.fits(n));
    let blocks: Vec<(u64, Vec<u8>)> = (0..n).map(|i| (300 + i as u64, block_of(i as u8))).collect();
    let refs: Vec<(u64, &[u8])> = blocks.iter().map(|(h, d)| (*h, &d[..])).collect();
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &refs).is_err());
    dev.fail_writes_after(None);
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.blocks.len(), n);
    for (h, d) in &blocks {
        assert_eq!(&read_home(&dev, *h), d);
    }
}

#[test]
fn transaction_too_big() {
    let (dev, mut j) = setup(V3);
    let n = j.capacity() as usize;
    assert!(!j.fits(n));
    let d = block_of(1);
    let refs: Vec<(u64, &[u8])> = (0..n).map(|i| (200 + i as u64, &d[..])).collect();
    assert!(matches!(j.commit(&dev, &refs), Err(Error::TooBig)));
    assert_eq!(j.blocks_needed(0), 1);
}

#[test]
fn empty_commit_is_noop() {
    let (dev, mut j) = setup(V3);
    let before = dev.snapshot();
    j.commit(&dev, &[]).unwrap();
    assert_eq!(before, dev.snapshot());
    assert_eq!(j.sequence, 7);
}

#[test]
fn fragmented_journal_map() {
    let map = JournalMap {
        runs: vec![(0, 100, 3), (3, 300, 30), (33, 150, 31)],
    };
    let (dev, mut j) = setup_with_map(V3, map);
    let blocks: Vec<(u64, Vec<u8>)> = (0..10).map(|i| (400 + i as u64, block_of(0x30 + i as u8))).collect();
    let refs: Vec<(u64, &[u8])> = blocks.iter().map(|(h, d)| (*h, &d[..])).collect();
    // jsb + log (split in 2 runs) + commit = 4 writes before checkpoint
    dev.fail_writes_after(Some(4));
    assert!(j.commit(&dev, &refs).is_err());
    dev.fail_writes_after(None);
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.transactions, 1);
    for (h, d) in &blocks {
        assert_eq!(&read_home(&dev, *h), d);
    }
}

/// Hand-built log with a revoke record: the revoked block must not replay.
#[test]
fn revoke_records_suppress_replay() {
    let (dev, j) = setup(V3);
    let seed = j.sb.csum_seed();
    let jb = |n: u32| (JSTART + n as u64) * BS as u64;
    // txn 7: descriptor + 2 data blocks + commit
    let mut desc = vec![0u8; BS];
    write_header(&mut desc, JBD2_DESCRIPTOR_BLOCK, 7);
    let d1 = block_of(0x11);
    let d2 = block_of(0x22);
    write_tag(&mut desc[12..28], &j.sb, 50, 0, tag_checksum(seed, 7, &d1), true);
    desc[28..44].copy_from_slice(&j.sb.uuid());
    write_tag(
        &mut desc[44..60],
        &j.sb,
        51,
        JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG,
        tag_checksum(seed, 7, &d2),
        true,
    );
    set_descriptor_tail(seed, &mut desc);
    dev.write_at(jb(1), &desc).unwrap();
    dev.write_at(jb(2), &d1).unwrap();
    dev.write_at(jb(3), &d2).unwrap();
    let mut commit = vec![0u8; BS];
    write_header(&mut commit, JBD2_COMMIT_BLOCK, 7);
    set_commit_checksum(seed, &mut commit);
    dev.write_at(jb(4), &commit).unwrap();
    // txn 8: revoke block 50 + commit
    let mut rev = vec![0u8; BS];
    write_header(&mut rev, JBD2_REVOKE_BLOCK, 8);
    set_be32(&mut rev, 12, 16 + 8);
    crate::bytes::set_be64(&mut rev, 16, 50);
    set_descriptor_tail(seed, &mut rev);
    dev.write_at(jb(5), &rev).unwrap();
    let mut commit = vec![0u8; BS];
    write_header(&mut commit, JBD2_COMMIT_BLOCK, 8);
    set_commit_checksum(seed, &mut commit);
    dev.write_at(jb(6), &commit).unwrap();
    // point jsb at the log
    let mut sb = j.sb.clone();
    sb.set_start(1);
    sb.set_sequence(7);
    sb.update_checksum();
    dev.write_at(jb(0), &sb.raw[..]).unwrap();

    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.transactions, 2);
    assert_eq!(plan.revoked, 1);
    assert_eq!(read_home(&dev, 50), vec![0u8; BS]);
    assert_eq!(read_home(&dev, 51), d2);
    assert_eq!(plan.next_sequence, 10);
}

#[test]
fn corrupted_data_block_fails_recovery() {
    let (dev, mut j) = setup(V3);
    let a = block_of(0xA1);
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &[(60, &a)]).is_err());
    dev.fail_writes_after(None);
    // corrupt the logged data block (journal block 2)
    dev.write_at((JSTART + 2) * BS as u64 + 7, &[0xFF]).unwrap();
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    assert!(matches!(j2.recover(&dev), Err(Error::Checksum(_))));
    // nothing was written and the journal still needs recovery
    assert_eq!(read_home(&dev, 60), vec![0u8; BS]);
    assert!(Journal::load(&dev, j.map.clone(), BS).unwrap().needs_recovery());
}

/// The data tag checksum covers the escaped block, as in jbd2 (a block
/// starting with the journal magic).
#[test]
fn escaped_block_checksum_covers_log_contents() {
    let (dev, mut j) = setup(V3);
    let mut b = block_of(0x11);
    set_be32(&mut b, 0, JBD2_MAGIC);
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &[(70, &b)]).is_err());
    dev.fail_writes_after(None);
    let mut desc = vec![0u8; BS];
    dev.read_at((JSTART + 1) * BS as u64, &mut desc).unwrap();
    let tag = read_tag(&desc[12..28], &j.sb);
    assert_ne!(tag.flags & JBD2_FLAG_ESCAPE, 0);
    let mut logged = vec![0u8; BS];
    dev.read_at((JSTART + 2) * BS as u64, &mut logged).unwrap();
    assert_eq!(&logged[..4], &[0, 0, 0, 0]);
    assert_eq!(tag.checksum, tag_checksum(j.sb.csum_seed(), 7, &logged));
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    j2.recover(&dev).unwrap();
    assert_eq!(read_home(&dev, 70), b);
}

/// A failed commit never lets its sequence number be reused.
#[test]
fn sequence_advances_even_when_commit_fails() {
    let (dev, mut j) = setup(V3);
    let a = block_of(1);
    dev.fail_writes_after(Some(1));
    assert!(j.commit(&dev, &[(80, &a)]).is_err());
    dev.fail_writes_after(None);
    assert_eq!(j.sequence, 8);
    j.commit(&dev, &[(80, &a)]).unwrap();
    assert_eq!(j.sequence, 9);
}

#[test]
fn blocks_above_4g_need_a_64bit_journal() {
    let (dev, mut j) = setup(PLAIN32);
    let a = block_of(1);
    assert!(matches!(j.commit(&dev, &[(1 << 32, &a)]), Err(Error::Invalid(_))));
    let (dev, mut j) = setup(PLAIN64);
    // 64-bit tags encode it (the device is small, so only check encoding)
    dev.fail_writes_after(Some(3));
    let _ = j.commit(&dev, &[((1 << 32) + 5, &a)]);
    dev.fail_writes_after(None);
    let mut desc = vec![0u8; BS];
    dev.read_at((JSTART + 1) * BS as u64, &mut desc).unwrap();
    assert_eq!(read_tag(&desc[12..24], &j.sb).blocknr, (1 << 32) + 5);
}

#[test]
fn enable_features_upgrades_empty_journal() {
    let (dev, mut j) = setup(PLAIN32);
    j.enable_features(&dev, true, true).unwrap();
    let j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    assert!(j2.sb.is_64bit());
    assert!(j2.sb.has_csum_v3());
    assert_eq!(j2.sb.checksum_type(), JBD2_CRC32C_CHKSUM);
    // idempotent
    let before = dev.snapshot();
    let mut j3 = j2;
    j3.enable_features(&dev, true, true).unwrap();
    assert_eq!(before, dev.snapshot());
}

#[test]
fn fast_commit_area_is_excluded() {
    let (_, j) = setup(V3 | JBD2_FEATURE_INCOMPAT_FAST_COMMIT);
    // JLEN 64 with the default 256 fc blocks leaves the minimum log
    assert_eq!(j.log_end(), 2);
    let map = JournalMap {
        runs: vec![(0, JSTART, 300)],
    };
    let (_, j) = setup_with_map(V3 | JBD2_FEATURE_INCOMPAT_FAST_COMMIT, map);
    assert_eq!(j.log_end(), 300 - 256);
    assert_eq!(j.capacity(), 300 - 256 - 1);
}

#[test]
fn corrupted_commit_block_stops_replay() {
    let (dev, mut j) = setup(V3);
    let a = block_of(0xA1);
    dev.fail_writes_after(Some(3));
    assert!(j.commit(&dev, &[(61, &a)]).is_err());
    dev.fail_writes_after(None);
    // corrupt the commit block (journal block 3)
    dev.write_at((JSTART + 3) * BS as u64 + 100, &[0xFF]).unwrap();
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.transactions, 0);
    assert_eq!(read_home(&dev, 61), vec![0u8; BS]);
}

#[test]
fn stale_transactions_are_not_replayed() {
    // Two commits in a row both start at s_first; after the second the log
    // still contains its (checkpointed) blocks. A later crash before the
    // third commit's commit block must not replay the second.
    let (dev, mut j) = setup(V3);
    let a = block_of(1);
    j.commit(&dev, &[(70, &a)]).unwrap();
    let b = block_of(2);
    j.commit(&dev, &[(70, &b)]).unwrap();
    // overwrite home with something else, simulating later direct writes
    dev.write_at(70 * BS as u64, &block_of(3)).unwrap();
    let c = block_of(4);
    // fail after jsb + log: commit block never written
    dev.fail_writes_after(Some(2));
    assert!(j.commit(&dev, &[(70, &c)]).is_err());
    dev.fail_writes_after(None);
    let mut j2 = Journal::load(&dev, j.map.clone(), BS).unwrap();
    let plan = j2.recover(&dev).unwrap();
    assert_eq!(plan.transactions, 0);
    assert_eq!(read_home(&dev, 70), block_of(3));
}

#[test]
fn load_rejects_bad_geometry() {
    let dev = MemDevice::new(512 * BS);
    let mut sb = make_jsb(V3);
    sb.set_block_size(4096);
    sb.update_checksum();
    dev.write_at(JSTART * BS as u64, &sb.raw[..]).unwrap();
    let map = JournalMap {
        runs: vec![(0, JSTART, JLEN)],
    };
    assert!(Journal::load(&dev, map.clone(), BS).is_err());

    let mut sb = make_jsb(V3);
    sb.set_max_len(JLEN + 1);
    sb.update_checksum();
    dev.write_at(JSTART * BS as u64, &sb.raw[..]).unwrap();
    assert!(Journal::load(&dev, map.clone(), BS).is_err());

    let mut sb = make_jsb(V3);
    sb.set_first(0);
    sb.update_checksum();
    dev.write_at(JSTART * BS as u64, &sb.raw[..]).unwrap();
    assert!(Journal::load(&dev, map, BS).is_err());

    assert!(Journal::load(&dev, JournalMap::default(), BS).is_err());
}

#[test]
fn unsupported_incompat_reported() {
    let (_, j) = setup(V3);
    assert_eq!(j.unsupported_incompat(), 0);
    let (_, j) = setup(V3 | 0x100);
    assert_eq!(j.unsupported_incompat(), 0x100);
}

#[test]
fn descriptor_parsing_stops_at_last_tag() {
    let sb = make_jsb(PLAIN32);
    let mut desc = vec![0u8; BS];
    write_header(&mut desc, JBD2_DESCRIPTOR_BLOCK, 1);
    write_tag(&mut desc[12..20], &sb, 5, 0, 0, false);
    // uuid 16 bytes
    write_tag(
        &mut desc[36..44],
        &sb,
        6,
        JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG,
        0,
        false,
    );
    write_tag(&mut desc[44..52], &sb, 7, JBD2_FLAG_SAME_UUID, 0, false);
    let tags = parse_descriptor(&desc, &sb);
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].blocknr, 5);
    assert_eq!(tags[1].blocknr, 6);
}

#[test]
fn revoke_parse_bounds() {
    let sb = make_jsb(PLAIN32);
    let mut rev = vec![0u8; BS];
    write_header(&mut rev, JBD2_REVOKE_BLOCK, 1);
    set_be32(&mut rev, 12, 8);
    assert!(parse_revoke(&rev, &sb).is_err());
    set_be32(&mut rev, 12, 16 + 8);
    set_be32(&mut rev, 16, 99);
    set_be32(&mut rev, 20, 100);
    assert_eq!(parse_revoke(&rev, &sb).unwrap(), vec![99, 100]);
}
