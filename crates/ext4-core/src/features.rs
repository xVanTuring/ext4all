//! Feature compatibility policy: which features we can read, which we can
//! write, and which force a refusal.

use crate::ondisk::superblock::{Superblock, compat, incompat, ro_compat};

/// How a file system with a given feature set may be mounted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    ReadWrite,
    /// Readable, but some feature prevents writing.
    ReadOnly(Vec<&'static str>),
    /// Cannot be mounted at all.
    Unsupported(Vec<String>),
}

/// Incompat features we understand well enough to read.
const INCOMPAT_READ: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::MMP
    | incompat::FLEX_BG
    | incompat::EA_INODE
    | incompat::CSUM_SEED
    | incompat::LARGEDIR
    | incompat::INLINE_DATA
    | incompat::ENCRYPT
    | incompat::CASEFOLD;

/// Incompat features we can also write.
const INCOMPAT_WRITE: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::META_BG
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::FLEX_BG
    | incompat::CSUM_SEED
    | incompat::LARGEDIR
    | incompat::INLINE_DATA;

/// ro_compat features we can write.
const RO_COMPAT_WRITE: u32 = ro_compat::SPARSE_SUPER
    | ro_compat::LARGE_FILE
    | ro_compat::HUGE_FILE
    | ro_compat::GDT_CSUM
    | ro_compat::DIR_NLINK
    | ro_compat::EXTRA_ISIZE
    | ro_compat::METADATA_CSUM
    | ro_compat::PROJECT
    | ro_compat::ORPHAN_PRESENT;

fn incompat_name(bit: u32) -> &'static str {
    match bit {
        incompat::COMPRESSION => "compression",
        incompat::FILETYPE => "filetype",
        incompat::RECOVER => "needs_recovery",
        incompat::JOURNAL_DEV => "journal_dev",
        incompat::META_BG => "meta_bg",
        incompat::EXTENTS => "extent",
        incompat::BIT64 => "64bit",
        incompat::MMP => "mmp",
        incompat::FLEX_BG => "flex_bg",
        incompat::EA_INODE => "ea_inode",
        incompat::DIRDATA => "dirdata",
        incompat::CSUM_SEED => "metadata_csum_seed",
        incompat::LARGEDIR => "large_dir",
        incompat::INLINE_DATA => "inline_data",
        incompat::ENCRYPT => "encrypt",
        incompat::CASEFOLD => "casefold",
        _ => "unknown",
    }
}

fn ro_compat_name(bit: u32) -> &'static str {
    match bit {
        ro_compat::SPARSE_SUPER => "sparse_super",
        ro_compat::LARGE_FILE => "large_file",
        ro_compat::BTREE_DIR => "btree_dir",
        ro_compat::HUGE_FILE => "huge_file",
        ro_compat::GDT_CSUM => "uninit_bg",
        ro_compat::DIR_NLINK => "dir_nlink",
        ro_compat::EXTRA_ISIZE => "extra_isize",
        ro_compat::HAS_SNAPSHOT => "snapshot",
        ro_compat::QUOTA => "quota",
        ro_compat::BIGALLOC => "bigalloc",
        ro_compat::METADATA_CSUM => "metadata_csum",
        ro_compat::REPLICA => "replica",
        ro_compat::READONLY => "read-only",
        ro_compat::PROJECT => "project",
        ro_compat::SHARED_BLOCKS => "shared_blocks",
        ro_compat::VERITY => "verity",
        ro_compat::ORPHAN_PRESENT => "orphan_present",
        _ => "unknown",
    }
}

fn bits(v: u32) -> impl Iterator<Item = u32> {
    (0..32).map(|i| 1u32 << i).filter(move |b| v & b != 0)
}

pub fn check(sb: &Superblock) -> Support {
    let inc = sb.feature_incompat();
    let unknown_inc = inc & !INCOMPAT_READ;
    if unknown_inc != 0 {
        return Support::Unsupported(
            bits(unknown_inc)
                .map(|b| match incompat_name(b) {
                    "unknown" => format!("incompat:{b:#x}"),
                    n => n.to_string(),
                })
                .collect(),
        );
    }
    if sb.has_compat(compat::HAS_JOURNAL) && sb.journal_inum() == 0 {
        return Support::Unsupported(vec!["external journal".into()]);
    }
    let mut ro = Vec::new();
    for b in bits(inc & !INCOMPAT_WRITE) {
        ro.push(incompat_name(b));
    }
    for b in bits(sb.feature_ro_compat() & !RO_COMPAT_WRITE) {
        ro.push(ro_compat_name(b));
    }
    if sb.has_ro_compat(ro_compat::METADATA_CSUM) && sb.has_ro_compat(ro_compat::GDT_CSUM) {
        ro.push("metadata_csum+uninit_bg");
    }
    if sb.has_compat(compat::FAST_COMMIT) {
        // Fast commit blocks cannot be replayed or produced; plain jbd2
        // transactions remain valid, so allow writing only when clean.
        if sb.has_incompat(incompat::RECOVER) {
            ro.push("fast_commit (needs recovery)");
        }
    }
    if ro.is_empty() {
        Support::ReadWrite
    } else {
        Support::ReadOnly(ro)
    }
}

/// Human-readable feature list, like `dumpe2fs` "Filesystem features".
pub fn describe(sb: &Superblock) -> Vec<&'static str> {
    let mut v = Vec::new();
    let compat_names = [
        (compat::HAS_JOURNAL, "has_journal"),
        (compat::EXT_ATTR, "ext_attr"),
        (compat::RESIZE_INODE, "resize_inode"),
        (compat::DIR_INDEX, "dir_index"),
        (compat::SPARSE_SUPER2, "sparse_super2"),
        (compat::FAST_COMMIT, "fast_commit"),
        (compat::STABLE_INODES, "stable_inodes"),
        (compat::ORPHAN_FILE, "orphan_file"),
    ];
    for (b, n) in compat_names {
        if sb.has_compat(b) {
            v.push(n);
        }
    }
    for b in bits(sb.feature_incompat()) {
        v.push(incompat_name(b));
    }
    for b in bits(sb.feature_ro_compat()) {
        v.push(ro_compat_name(b));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::superblock::tests::sample;

    #[test]
    fn default_ext4_is_read_write() {
        assert_eq!(check(&sample()), Support::ReadWrite);
    }

    #[test]
    fn unknown_incompat_refused() {
        let mut sb = sample();
        sb.set_feature_incompat(sb.feature_incompat() | 0x8000_0000);
        match check(&sb) {
            Support::Unsupported(v) => assert_eq!(v, vec!["incompat:0x80000000".to_string()]),
            other => panic!("{other:?}"),
        }
        let mut sb = sample();
        sb.set_feature_incompat(sb.feature_incompat() | incompat::COMPRESSION | incompat::DIRDATA);
        match check(&sb) {
            Support::Unsupported(v) => assert_eq!(v, vec!["compression", "dirdata"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn journal_dev_refused() {
        let mut sb = sample();
        sb.set_feature_incompat(sb.feature_incompat() | incompat::JOURNAL_DEV);
        assert!(matches!(check(&sb), Support::Unsupported(_)));
    }

    #[test]
    fn external_journal_refused() {
        let mut sb = sample();
        sb.set_journal_inum(0);
        assert!(matches!(check(&sb), Support::Unsupported(_)));
    }

    #[test]
    fn read_only_features() {
        for (f, name) in [
            (incompat::ENCRYPT, "encrypt"),
            (incompat::CASEFOLD, "casefold"),
            (incompat::MMP, "mmp"),
            (incompat::EA_INODE, "ea_inode"),
        ] {
            let mut sb = sample();
            sb.set_journal_inum(8);
            sb.set_feature_incompat(sb.feature_incompat() | f);
            assert_eq!(check(&sb), Support::ReadOnly(vec![name]), "{name}");
        }
        for (f, name) in [
            (ro_compat::BIGALLOC, "bigalloc"),
            (ro_compat::QUOTA, "quota"),
            (ro_compat::VERITY, "verity"),
            (ro_compat::READONLY, "read-only"),
            (ro_compat::HAS_SNAPSHOT, "snapshot"),
            (ro_compat::SHARED_BLOCKS, "shared_blocks"),
        ] {
            let mut sb = sample();
            sb.set_journal_inum(8);
            sb.set_feature_ro_compat(sb.feature_ro_compat() | f);
            assert_eq!(check(&sb), Support::ReadOnly(vec![name]), "{name}");
        }
    }

    #[test]
    fn csum_and_gdt_csum_conflict() {
        let mut sb = sample();
        sb.set_journal_inum(8);
        sb.set_feature_ro_compat(sb.feature_ro_compat() | ro_compat::GDT_CSUM);
        assert!(matches!(check(&sb), Support::ReadOnly(_)));
    }

    #[test]
    fn fast_commit_needing_recovery_is_read_only() {
        let mut sb = sample();
        sb.set_journal_inum(8);
        sb.set_feature_compat(sb.feature_compat() | compat::FAST_COMMIT);
        assert_eq!(check(&sb), Support::ReadWrite);
        sb.set_feature_incompat(sb.feature_incompat() | incompat::RECOVER);
        assert!(matches!(check(&sb), Support::ReadOnly(_)));
    }

    #[test]
    fn describe_lists_features() {
        let mut sb = sample();
        sb.set_journal_inum(8);
        let d = describe(&sb);
        for n in [
            "has_journal",
            "ext_attr",
            "dir_index",
            "extent",
            "64bit",
            "flex_bg",
            "metadata_csum",
        ] {
            assert!(d.contains(&n), "{n} missing from {d:?}");
        }
    }
}
