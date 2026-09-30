//! Randomized operation sequences checked against an in-memory model and
//! e2fsck.

mod common;
use common::*;
use ext4_core::{Error, FileType, Fs, MountOptions, RenameFlags, XattrSetMode};
use proptest::prelude::*;
use std::collections::BTreeMap;

const DIRS: [&str; 3] = ["", "d1", "d1/d2"];
const NAMES: [&str; 6] = ["a", "b", "c", "long-name-for-testing-purposes", "x", "y"];

#[derive(Clone, Debug)]
enum Op {
    Create(usize, usize),
    Write(usize, usize, u32, u32, u64),
    Truncate(usize, usize, u32),
    Unlink(usize, usize),
    Rename(usize, usize, usize, usize),
    Link(usize, usize, usize, usize),
    SetXattr(usize, usize, u8, u16),
    Punch(usize, usize, u32, u32),
    Commit,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let d = 0..DIRS.len();
    let n = 0..NAMES.len();
    prop_oneof![
        3 => (d.clone(), n.clone()).prop_map(|(d, n)| Op::Create(d, n)),
        5 => (d.clone(), n.clone(), 0u32..300_000, 1u32..70_000, any::<u64>()).prop_map(|(d, n, o, l, s)| Op::Write(d, n, o, l, s)),
        2 => (d.clone(), n.clone(), 0u32..300_000).prop_map(|(d, n, s)| Op::Truncate(d, n, s)),
        2 => (d.clone(), n.clone()).prop_map(|(d, n)| Op::Unlink(d, n)),
        2 => (d.clone(), n.clone(), d.clone(), n.clone()).prop_map(|(a, b, c, e)| Op::Rename(a, b, c, e)),
        1 => (d.clone(), n.clone(), d.clone(), n.clone()).prop_map(|(a, b, c, e)| Op::Link(a, b, c, e)),
        1 => (d.clone(), n.clone(), 0u8..4, 0u16..700).prop_map(|(d, n, k, l)| Op::SetXattr(d, n, k, l)),
        1 => (d.clone(), n.clone(), 0u32..300_000, 1u32..50_000).prop_map(|(d, n, o, l)| Op::Punch(d, n, o, l)),
        1 => Just(Op::Commit),
    ]
}

/// Model: (dir, name) → file id; file id → contents / xattrs / links.
#[derive(Default)]
struct Model {
    names: BTreeMap<(usize, usize), usize>,
    data: BTreeMap<usize, Vec<u8>>,
    xattrs: BTreeMap<usize, BTreeMap<String, Vec<u8>>>,
    next: usize,
}

impl Model {
    fn links(&self, id: usize) -> usize {
        self.names.values().filter(|&&v| v == id).count()
    }
}

fn apply(fs: &mut Fs, dirs: &[u32], m: &mut Model, op: &Op) {
    let name = |i: usize| NAMES[i].as_bytes();
    match *op {
        Op::Create(d, n) => {
            let r = fs.create(dirs[d], name(n), FileType::Regular, 0o644, 0, 0, 0);
            if m.names.contains_key(&(d, n)) {
                assert!(matches!(r, Err(Error::Exists)), "{r:?}");
            } else {
                r.unwrap();
                let id = m.next;
                m.next += 1;
                m.names.insert((d, n), id);
                m.data.insert(id, Vec::new());
            }
        }
        Op::Write(d, n, off, len, seed) => {
            let Some(&id) = m.names.get(&(d, n)) else { return };
            let ino = fs.lookup(dirs[d], name(n)).unwrap();
            let buf = pattern(len as usize, seed);
            match fs.write(ino, off as u64, &buf) {
                Ok(w) => {
                    assert_eq!(w, buf.len());
                    let v = m.data.get_mut(&id).unwrap();
                    let end = off as usize + len as usize;
                    if v.len() < end {
                        v.resize(end, 0);
                    }
                    v[off as usize..end].copy_from_slice(&buf);
                }
                Err(Error::NoSpace) => {
                    // partial writes may have landed; resync the model
                    let size = fs.stat(ino).unwrap().size as usize;
                    let mut cur = vec![0u8; size];
                    fs.read(ino, 0, &mut cur).unwrap();
                    m.data.insert(id, cur);
                }
                Err(e) => panic!("write: {e:?}"),
            }
        }
        Op::Truncate(d, n, size) => {
            let Some(&id) = m.names.get(&(d, n)) else { return };
            let ino = fs.lookup(dirs[d], name(n)).unwrap();
            fs.truncate(ino, size as u64).unwrap();
            m.data.get_mut(&id).unwrap().resize(size as usize, 0);
        }
        Op::Unlink(d, n) => {
            let r = fs.unlink(dirs[d], name(n));
            match m.names.remove(&(d, n)) {
                Some(id) => {
                    r.unwrap();
                    if m.links(id) == 0 {
                        m.data.remove(&id);
                        m.xattrs.remove(&id);
                    }
                }
                None => assert!(matches!(r, Err(Error::NotFound))),
            }
        }
        Op::Rename(sd, sn, dd, dn) => {
            let r = fs.rename(dirs[sd], name(sn), dirs[dd], name(dn), RenameFlags::default());
            let Some(&sid) = m.names.get(&(sd, sn)) else {
                assert!(matches!(r, Err(Error::NotFound)));
                return;
            };
            r.unwrap();
            if (sd, sn) == (dd, dn) {
                return;
            }
            if let Some(&tid) = m.names.get(&(dd, dn))
                && tid == sid
            {
                // same inode under both names: POSIX says do nothing
                return;
            }
            m.names.remove(&(sd, sn));
            if let Some(old) = m.names.insert((dd, dn), sid)
                && m.links(old) == 0
            {
                m.data.remove(&old);
                m.xattrs.remove(&old);
            }
        }
        Op::Link(sd, sn, dd, dn) => {
            let Some(&sid) = m.names.get(&(sd, sn)) else { return };
            let ino = fs.lookup(dirs[sd], name(sn)).unwrap();
            let r = fs.link(ino, dirs[dd], name(dn));
            match m.names.entry((dd, dn)) {
                std::collections::btree_map::Entry::Occupied(_) => assert!(matches!(r, Err(Error::Exists))),
                std::collections::btree_map::Entry::Vacant(v) => {
                    r.unwrap();
                    v.insert(sid);
                }
            }
        }
        Op::SetXattr(d, n, k, len) => {
            let Some(&id) = m.names.get(&(d, n)) else { return };
            let ino = fs.lookup(dirs[d], name(n)).unwrap();
            let key = format!("user.k{k}");
            let val = pattern(len as usize, k as u64);
            match fs.set_xattr(ino, key.as_bytes(), &val, XattrSetMode::Any) {
                Ok(()) => {
                    m.xattrs.entry(id).or_default().insert(key, val);
                }
                Err(Error::NoSpace) => {
                    // the old value was removed before the space check
                    m.xattrs.entry(id).or_default().remove(&key);
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        Op::Punch(d, n, off, len) => {
            let Some(&id) = m.names.get(&(d, n)) else { return };
            let ino = fs.lookup(dirs[d], name(n)).unwrap();
            fs.punch_hole(ino, off as u64, len as u64).unwrap();
            let v = m.data.get_mut(&id).unwrap();
            let s = (off as usize).min(v.len());
            let e = (off as usize + len as usize).min(v.len());
            for b in &mut v[s..e] {
                *b = 0;
            }
        }
        Op::Commit => fs.commit().unwrap(),
    }
}

fn verify(fs: &mut Fs, dirs: &[u32], m: &Model) {
    for (&(d, n), id) in &m.names {
        let ino = fs.lookup(dirs[d], NAMES[n].as_bytes()).unwrap();
        let want = &m.data[id];
        let a = fs.stat(ino).unwrap();
        assert_eq!(a.size, want.len() as u64, "{}/{}", DIRS[d], NAMES[n]);
        assert_eq!(a.nlink as usize, m.links(*id));
        let mut got = vec![0u8; want.len()];
        let mut done = 0;
        while done < got.len() {
            done += fs.read(ino, done as u64, &mut got[done..]).unwrap();
        }
        assert!(got == *want, "content mismatch for {}/{}", DIRS[d], NAMES[n]);
        let mut names = fs.list_xattr(ino).unwrap();
        names.sort();
        let mut want_x: Vec<Vec<u8>> = m
            .xattrs
            .get(id)
            .map(|x| x.keys().map(|k| k.as_bytes().to_vec()).collect())
            .unwrap_or_default();
        want_x.sort();
        assert_eq!(names, want_x);
        if let Some(xs) = m.xattrs.get(id) {
            for (k, v) in xs {
                assert_eq!(&fs.get_xattr(ino, k.as_bytes()).unwrap(), v);
            }
        }
    }
    for (d, &dino) in dirs.iter().enumerate() {
        let listed: Vec<Vec<u8>> = fs
            .list_dir(dino)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| n != b"." && n != b".." && n != b"lost+found" && n != b"d1" && n != b"d2")
            .collect();
        let mut want: Vec<Vec<u8>> = m
            .names
            .keys()
            .filter(|(dd, _)| *dd == d)
            .map(|(_, n)| NAMES[*n].as_bytes().to_vec())
            .collect();
        let mut listed = listed;
        listed.sort();
        want.sort();
        assert_eq!(listed, want, "listing of {:?}", DIRS[d]);
    }
}

fn run_case(opts: &[&str], ops: &[Op]) {
    let img = Image::new(32, opts);
    let mut m = Model::default();
    {
        let mut fs = img.mount_opts(MountOptions {
            commit_threshold: 64,
            ..Default::default()
        });
        let root = fs.root();
        let d1 = fs.mkdir(root, b"d1", 0o755, 0, 0).unwrap().ino;
        let d2 = fs.mkdir(d1, b"d2", 0o755, 0, 0).unwrap().ino;
        let dirs = [root, d1, d2];
        for op in ops {
            apply(&mut fs, &dirs, &mut m, op);
        }
        verify(&mut fs, &dirs, &m);
        fs.unmount().unwrap();
    }
    let (code, out) = img.fsck();
    assert!(
        code == 0 && !out.contains("Fix? no"),
        "e2fsck exit {code} after {ops:?}\n{out}"
    );
    let mut fs = img.mount_ro();
    let root = fs.root();
    let d1 = fs.lookup(root, b"d1").unwrap();
    let d2 = fs.lookup(d1, b"d2").unwrap();
    verify(&mut fs, &[root, d1, d2], &m);
}

/// Directory-heavy workload: thousands of names in one directory with
/// random creates, unlinks, renames and subdirectories, forcing htree
/// splits and level additions.
fn run_dir_case(opts: &[&str], seed: u64, rounds: usize) {
    use std::collections::BTreeSet;
    let img = Image::new(64, opts);
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut subdirs: BTreeSet<String> = BTreeSet::new();
    let mut rng = seed | 1;
    let mut next = |m: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % m
    };
    {
        let mut fs = img.mount();
        let root = fs.root();
        let d = fs.mkdir(root, b"big", 0o755, 0, 0).unwrap().ino;
        for _ in 0..rounds {
            let k = next(100);
            let n = format!("{}-{:05}", "n".repeat(1 + next(40) as usize), next(20_000));
            if k < 60 {
                let r = fs.create(d, n.as_bytes(), FileType::Regular, 0o644, 0, 0, 0);
                if names.contains(&n) || subdirs.contains(&n) {
                    assert!(matches!(r, Err(Error::Exists)));
                } else {
                    r.unwrap();
                    names.insert(n);
                }
            } else if k < 70 {
                if names.contains(&n) || subdirs.contains(&n) {
                    continue;
                }
                fs.mkdir(d, n.as_bytes(), 0o755, 0, 0).unwrap();
                subdirs.insert(n);
            } else if k < 85 {
                if let Some(victim) = names.iter().nth(next(names.len().max(1) as u64) as usize).cloned() {
                    fs.unlink(d, victim.as_bytes()).unwrap();
                    names.remove(&victim);
                }
            } else if k < 90 {
                if let Some(victim) = subdirs.iter().next().cloned() {
                    fs.rmdir(d, victim.as_bytes()).unwrap();
                    subdirs.remove(&victim);
                }
            } else if let Some(src) = names.iter().nth(next(names.len().max(1) as u64) as usize).cloned() {
                if subdirs.contains(&n) {
                    continue;
                }
                fs.rename(d, src.as_bytes(), d, n.as_bytes(), RenameFlags::default())
                    .unwrap();
                names.remove(&src);
                names.insert(n);
            }
        }
        fs.check_htree(d).unwrap();
        let mut listed: Vec<String> = fs
            .list_dir(d)
            .unwrap()
            .into_iter()
            .map(|e| String::from_utf8(e.name).unwrap())
            .filter(|n| n != "." && n != "..")
            .collect();
        listed.sort();
        let mut want: Vec<String> = names.iter().chain(subdirs.iter()).cloned().collect();
        want.sort();
        assert_eq!(listed, want);
        for n in names.iter().step_by(13) {
            fs.lookup(d, n.as_bytes()).unwrap();
        }
        assert_eq!(fs.stat(d).unwrap().nlink as usize, 2 + subdirs.len());
        fs.unmount().unwrap();
    }
    let (code, out) = img.fsck();
    assert!(
        code == 0 && !out.contains("Fix? no"),
        "seed {seed}: e2fsck exit {code}\n{out}"
    );
}

#[test]
fn random_directory_workloads() {
    for seed in 1..6u64 {
        run_dir_case(&["-t", "ext4", "-b", "1024"], seed * 7919, 5000);
    }
    run_dir_case(&["-t", "ext4", "-b", "4096"], 42, 8000);
    run_dir_case(
        &["-t", "ext4", "-b", "1024", "-O", "^metadata_csum,^metadata_csum_seed"],
        99,
        5000,
    );
}

#[test]
fn directory_grows_to_two_htree_levels() {
    // 1K blocks, long names: a single level fills after ~100 leaves
    let img = Image::new(128, &["-t", "ext4", "-b", "1024"]);
    let n = 30_000;
    {
        let mut fs = img.mount();
        let root = fs.root();
        let d = fs.mkdir(root, b"huge", 0o755, 0, 0).unwrap().ino;
        for i in 0..n {
            let nm = format!("{}{i:08}", "p".repeat(60));
            fs.create(d, nm.as_bytes(), FileType::Regular, 0o644, 0, 0, 0).unwrap();
        }
        fs.check_htree(d).unwrap();
        assert_eq!(fs.list_dir(d).unwrap().len(), n + 2);
        fs.unmount().unwrap();
    }
    let out = img.debugfs(&["htree_dump /huge"]);
    assert!(out.contains("Indirect levels: 1"), "{}", &out[..out.len().min(2000)]);
    let (code, out) = img.fsck();
    assert!(code == 0 && !out.contains("Fix? no"), "e2fsck exit {code}\n{out}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 40, ..ProptestConfig::default() })]

    #[test]
    fn random_ops_4k(ops in prop::collection::vec(op_strategy(), 1..80)) {
        run_case(&["-t", "ext4", "-b", "4096"], &ops);
    }

    #[test]
    fn random_ops_1k(ops in prop::collection::vec(op_strategy(), 1..80)) {
        run_case(&["-t", "ext4", "-b", "1024"], &ops);
    }

    #[test]
    fn random_ops_no_journal_no_csum(ops in prop::collection::vec(op_strategy(), 1..60)) {
        run_case(&["-t", "ext4", "-O", "^has_journal,^metadata_csum,^metadata_csum_seed"], &ops);
    }
}
