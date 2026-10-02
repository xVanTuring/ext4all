//! Encrypted volumes made by Linux (scripts/make-crypt-fixtures.py, run on
//! kernel 7.2 with fscrypt 0.3.6 and cryptsetup 2.8): every file, name and
//! symlink must match what Linux showed with and without the keys, and our
//! writes must leave a volume e2fsck accepts.

mod common;

use common::{Image, tool};
use ext4_core::crypto::from_hex;
use ext4_core::luks::Header;
use ext4_core::{BlockDevice, Error, FileDevice, FileType, Fs, Ino, MountOptions, SharedFs};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crypt")
}

/// Unpack a fixture image into a temporary directory.
fn fixture(name: &str) -> (Image, Value) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fs.img");
    let gz = fixture_dir().join(format!("{name}.img.gz"));
    let out = Command::new("gzip").arg("-dc").arg(&gz).output().expect("gzip");
    assert!(out.status.success(), "gunzip {gz:?}");
    std::fs::write(&path, out.stdout).unwrap();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(fixture_dir().join(format!("{name}.json"))).unwrap()).unwrap();
    (Image { dir, path }, manifest)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    kind: &'static str,
    ino: u64,
    size: Option<u64>,
    sha256: Option<String>,
    target: Option<String>,
    locked: bool,
}

/// The manifest's view: path -> entry.
fn expected(list: &Value) -> BTreeMap<String, Entry> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let kind = match e["type"].as_str().unwrap() {
                "file" => "file",
                "dir" => "dir",
                "link" => "link",
                "fifo" => "fifo",
                k => panic!("{k}"),
            };
            (
                e["path"].as_str().unwrap().to_string(),
                Entry {
                    kind,
                    ino: e["ino"].as_u64().unwrap(),
                    size: e.get("size").and_then(Value::as_u64),
                    sha256: e.get("sha256").and_then(Value::as_str).map(str::to_string),
                    target: e.get("target").and_then(Value::as_str).map(str::to_string),
                    locked: e.get("locked").and_then(Value::as_bool).unwrap_or(false),
                },
            )
        })
        .collect()
}

/// Our view of the same tree.
fn walk(fs: &mut Fs) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut todo: Vec<(Ino, String)> = vec![(fs.root(), String::new())];
    while let Some((dir, prefix)) = todo.pop() {
        for e in fs.list_dir(dir).unwrap() {
            let name = String::from_utf8(e.name.clone()).expect("names are UTF-8");
            if name == "." || name == ".." || (prefix.is_empty() && (name == "lost+found" || name == ".fscrypt")) {
                continue;
            }
            let path = format!("{prefix}{name}");
            let a = fs.stat(e.ino).unwrap();
            let mut ent = Entry {
                kind: "",
                ino: e.ino as u64,
                size: None,
                sha256: None,
                target: None,
                locked: false,
            };
            match a.file_type {
                FileType::Directory => {
                    ent.kind = "dir";
                    todo.push((e.ino, format!("{path}/")));
                }
                FileType::Symlink => {
                    ent.kind = "link";
                    ent.size = Some(a.size);
                    ent.target = Some(String::from_utf8(fs.read_link(e.ino).unwrap()).unwrap());
                }
                FileType::Fifo => ent.kind = "fifo",
                FileType::Regular => {
                    ent.kind = "file";
                    ent.size = Some(a.size);
                    let mut buf = vec![0u8; a.size as usize];
                    match fs.read(e.ino, 0, &mut buf) {
                        Ok(n) => {
                            assert_eq!(n as u64, a.size, "{path}");
                            ent.sha256 = Some(format!("{:x}", Sha256::digest(&buf)));
                        }
                        Err(Error::NoKey) => ent.locked = true,
                        Err(e) => panic!("{path}: {e}"),
                    }
                }
                t => panic!("{path}: {t:?}"),
            }
            out.insert(path, ent);
        }
    }
    out
}

fn assert_same(ours: &BTreeMap<String, Entry>, linux: &BTreeMap<String, Entry>, what: &str) {
    for (p, e) in linux {
        match ours.get(p) {
            Some(o) => assert_eq!(o, e, "{what}: {p}"),
            None => panic!("{what}: missing {p}"),
        }
    }
    for p in ours.keys() {
        assert!(linux.contains_key(p), "{what}: unexpected {p}");
    }
}

fn mount(img: &Image, read_only: bool) -> Fs {
    let dev = Arc::new(FileDevice::open(&img.path, read_only).unwrap());
    Fs::mount(
        dev,
        MountOptions {
            read_only,
            ..Default::default()
        },
    )
    .unwrap()
}

fn add_keys(fs: &mut Fs, m: &Value) {
    for k in m["keys"].as_array().unwrap() {
        let ids = fs
            .add_encryption_key(&from_hex(k["key"].as_str().unwrap()).unwrap())
            .unwrap();
        let r = from_hex(k["ref"].as_str().unwrap()).unwrap();
        if k["version"] == 1 {
            assert_eq!(ids.descriptor[..], r[..], "v1 descriptor of {}", k["dir"]);
        } else {
            assert_eq!(ids.identifier[..], r[..], "v2 identifier of {}", k["dir"]);
        }
    }
}

const FSCRYPT: &[&str] = &["fscrypt-4k", "fscrypt-1k"];

#[test]
fn names_without_key_match_linux() {
    for name in FSCRYPT {
        let (img, m) = fixture(name);
        let mut fs = mount(&img, true);
        assert_same(
            &walk(&mut fs),
            &expected(&m["without_key"]),
            &format!("{name} without key"),
        );
    }
}

#[test]
fn contents_with_key_match_linux() {
    for name in FSCRYPT {
        let (img, m) = fixture(name);
        let mut fs = mount(&img, true);
        add_keys(&mut fs, &m);
        assert_same(&walk(&mut fs), &expected(&m["with_key"]), &format!("{name} with key"));
    }
}

/// `SharedFs::read_parallel` from several threads returns what `Fs::read`
/// does for every file of `fs`, including the errors of locked files.
fn compare_parallel_reads(mut fs: Fs, what: &str) {
    let files: Vec<(String, Ino, u64)> = walk(&mut fs)
        .into_iter()
        .filter(|(_, e)| e.kind == "file")
        .map(|(p, e)| (p, e.ino as Ino, e.size.unwrap()))
        .collect();
    assert!(!files.is_empty(), "{what}");
    let shared = SharedFs::new(fs, Duration::from_secs(5));
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let (shared, files) = (&shared, &files);
            s.spawn(move || {
                for (path, ino, size) in files {
                    for offset in [0, 1 + t, size / 3, size.saturating_sub(5000 + t)] {
                        let (mut a, mut b) = (vec![0u8; 70_000], vec![1u8; 70_000]);
                        let ra = shared.read_parallel(*ino, offset, &mut a);
                        let rb = shared.with(|fs| fs.read(*ino, offset, &mut b));
                        match (ra, rb) {
                            (Ok(x), Ok(y)) => {
                                assert_eq!(x, y, "{what}: {path} at {offset}");
                                assert!(a[..x] == b[..y], "{what}: {path} at {offset}: contents differ");
                            }
                            (Err(Error::NoKey), Err(Error::NoKey)) => {}
                            (x, y) => panic!("{what}: {path} at {offset}: {x:?} / {y:?}"),
                        }
                    }
                }
            });
        }
    });
}

#[test]
fn parallel_reads_of_encrypted_volumes() {
    for name in FSCRYPT {
        let (img, m) = fixture(name);
        compare_parallel_reads(mount(&img, true), &format!("{name} without key"));
        let mut fs = mount(&img, true);
        add_keys(&mut fs, &m);
        compare_parallel_reads(fs, &format!("{name} with key"));
    }
    let (img, m) = fixture("luks1-xts");
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&img.path, true).unwrap());
    let h = Header::read(&*dev).unwrap().expect("LUKS header");
    let pass = m["passphrases"][0].as_str().unwrap().as_bytes();
    let key = h.unlock(&*dev, pass).unwrap().unwrap();
    let crypt = Arc::new(h.open(dev.clone(), &key).unwrap());
    let fs = Fs::mount(
        crypt,
        MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    compare_parallel_reads(fs, "luks1-xts");
}

#[test]
fn lookups_by_shown_names() {
    let (img, m) = fixture("fscrypt-4k");
    let mut fs = mount(&img, true);
    // every path Linux listed without the key resolves, long names too
    for (p, e) in expected(&m["without_key"]) {
        assert_eq!(fs.resolve(&format!("/{p}")).unwrap() as u64, e.ino, "{p}");
    }
    assert!(matches!(fs.resolve("/v2/notbase64!"), Err(Error::NotFound)));
    assert!(matches!(fs.resolve("/v2/hello.txt"), Err(Error::NotFound)));
    fs.add_encryption_key(&from_hex(m["keys"][0]["key"].as_str().unwrap()).unwrap())
        .unwrap();
    let ino = fs.resolve("/v2/hello.txt").unwrap();
    let mut b = vec![0u8; 64];
    let n = fs.read(ino, 0, &mut b).unwrap();
    assert_eq!(&b[..n], b"hello fscrypt\n");
    assert!(fs.resolve(&format!("/v2/{}", "M".repeat(255))).is_ok());
    assert!(fs.resolve("/v2/many/file-299").is_ok());
    assert!(matches!(fs.resolve("/v2/many/file-300"), Err(Error::NotFound)));
}

#[test]
fn fscrypt_tool_protectors() {
    let (img, m) = fixture("fscrypt-tool");
    let mut fs = mount(&img, true);
    assert_same(&walk(&mut fs), &expected(&m["without_key"]), "tool without key");
    assert!(fs.unlock_with_protector(b"wrong passphrase").unwrap().is_empty());
    let pass = m["passphrase"].as_str().unwrap();
    assert_eq!(fs.unlock_with_protector(pass.as_bytes()).unwrap().len(), 1);
    let raw = from_hex(m["raw_protector_key"].as_str().unwrap()).unwrap();
    assert_eq!(fs.unlock_with_protector(&raw).unwrap().len(), 1);
    assert_same(&walk(&mut fs), &expected(&m["with_key"]), "tool with keys");
}

/// Write into the encrypted directories, then check the volume with
/// e2fsck and read everything back with and without the key.
#[test]
fn writes_in_encrypted_directories() {
    for name in FSCRYPT {
        let (img, m) = fixture(name);
        let dirs: Vec<String> = m["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["dir"].as_str().unwrap().to_string())
            .collect();
        let mut expect: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        {
            let mut fs = mount(&img, false);
            // without the key: listing and deleting work, creating does not
            let d = fs.resolve(&format!("/{}", dirs[0])).unwrap();
            assert!(matches!(
                fs.create(d, b"new", FileType::Regular, 0o644, 0, 0, 0),
                Err(Error::NoKey)
            ));
            add_keys(&mut fs, &m);
            for (i, dir) in dirs.iter().enumerate() {
                let d = fs.resolve(&format!("/{dir}")).unwrap();
                let f = fs
                    .create(d, b"written-on-mac.txt", FileType::Regular, 0o644, 0, 0, 0)
                    .unwrap();
                let data = common::pattern(70_000 + i * 999, i as u64);
                assert_eq!(fs.write(f.ino, 0, &data).unwrap(), data.len());
                // overwrite in the middle, unaligned
                fs.write(f.ino, 5000, b"PATCHED").unwrap();
                let mut want = data.clone();
                want[5000..5007].copy_from_slice(b"PATCHED");
                expect.insert(format!("{dir}/written-on-mac.txt"), want);
                // truncate an existing file into the middle of a block
                let odd = fs.resolve(&format!("/{dir}/odd.bin")).unwrap();
                let mut before = vec![0u8; 10000];
                fs.read(odd, 0, &mut before).unwrap();
                fs.truncate(odd, 6000).unwrap();
                fs.truncate(odd, 9000).unwrap();
                let mut want = before[..6000].to_vec();
                want.resize(9000, 0);
                expect.insert(format!("{dir}/odd.bin"), want);
                // directories, symlinks, renames, links, deletes
                let sub = fs.mkdir(d, "目录".as_bytes(), 0o755, 0, 0).unwrap();
                fs.symlink(sub.ino, b"link", b"../hello.txt", 0, 0).unwrap();
                fs.symlink(sub.ino, b"long", &[b'y'; 300], 0, 0).unwrap();
                fs.rename(d, b"hello.txt", sub.ino, b"moved.txt", Default::default())
                    .unwrap();
                fs.link(f.ino, sub.ino, b"hard").unwrap();
                fs.unlink(d, b"empty").unwrap();
                expect.insert(format!("{dir}/目录/moved.txt"), b"hello fscrypt\n".to_vec());
                // many entries: grows the directory into an htree
                for j in 0..120 {
                    fs.create(sub.ino, format!("n{j}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                        .unwrap();
                }
                // moving a plain file in is refused, out is fine
                let plain = fs.resolve("/plain").unwrap();
                assert!(matches!(
                    fs.rename(plain, b"odd.bin", d, b"x", Default::default()),
                    Err(Error::CrossDevice)
                ));
                // between directories of different policies, too
                if i > 0 {
                    let first = fs.resolve(&format!("/{}", dirs[0])).unwrap();
                    assert!(matches!(
                        fs.rename(first, b"block.bin", d, b"x", Default::default()),
                        Err(Error::CrossDevice)
                    ));
                }
            }
            fs.unmount().unwrap();
        }
        let (code, out) = img.fsck();
        assert_eq!(code, 0, "{name}: e2fsck after our writes:\n{out}");
        let mut fs = mount(&img, true);
        add_keys(&mut fs, &m);
        for (p, want) in &expect {
            let ino = fs.resolve(&format!("/{p}")).unwrap();
            let mut b = vec![0u8; want.len() + 10];
            let n = fs.read(ino, 0, &mut b).unwrap();
            assert_eq!(&b[..n], &want[..], "{name}: {p}");
        }
        for dir in &dirs {
            let l = fs.resolve(&format!("/{dir}/目录/link")).unwrap();
            assert_eq!(fs.read_link(l).unwrap(), b"../hello.txt");
            assert_eq!(fs.stat(l).unwrap().size, 12);
            let l = fs.resolve(&format!("/{dir}/目录/long")).unwrap();
            assert_eq!(fs.read_link(l).unwrap(), vec![b'y'; 300]);
            let sub = fs.resolve(&format!("/{dir}/目录")).unwrap();
            assert_eq!(fs.list_dir(sub).unwrap().len(), 2 + 4 + 120);
        }
        // without the key everything still lists, under no-key names
        let mut fs = mount(&img, true);
        let all = walk(&mut fs);
        assert!(all.values().filter(|e| e.locked).count() > 10);
    }
}

#[test]
fn set_policy_on_new_directory() {
    let img = Image::new(32, &["-t", "ext4", "-O", "encrypt"]);
    let key = [0x5au8; 64];
    {
        let mut fs = img.mount();
        let ids = fs.add_encryption_key(&key).unwrap();
        let d = fs.mkdir(fs.root(), b"vault", 0o700, 0, 0).unwrap();
        let policy = ext4_core::fscrypt::Context {
            contents_mode: ext4_core::fscrypt::mode::AES_256_XTS,
            filenames_mode: ext4_core::fscrypt::mode::AES_256_CTS,
            flags: 2,
            log2_data_unit_size: 0,
            key: ext4_core::fscrypt::KeySpec::V2(ids.identifier),
            nonce: [0; 16],
        };
        assert!(matches!(
            fs.set_encryption_policy(fs.root(), &policy),
            Err(Error::NotPermitted)
        ));
        fs.set_encryption_policy(d.ino, &policy).unwrap();
        // again: same policy is fine
        fs.set_encryption_policy(d.ino, &policy).unwrap();
        let f = fs
            .create(d.ino, b"secret.txt", FileType::Regular, 0o600, 0, 0, 0)
            .unwrap();
        fs.write(f.ino, 0, b"top secret").unwrap();
        // a non-empty directory cannot be encrypted
        let e = fs.mkdir(fs.root(), b"full", 0o700, 0, 0).unwrap();
        fs.create(e.ino, b"x", FileType::Regular, 0o600, 0, 0, 0).unwrap();
        assert!(matches!(fs.set_encryption_policy(e.ino, &policy), Err(Error::NotEmpty)));
        fs.unmount().unwrap();
    }
    img.assert_clean();
    let mut fs = img.mount_ro();
    let d = fs.resolve("/vault").unwrap();
    let names = fs.list_dir(d).unwrap();
    assert_eq!(names.len(), 3);
    assert!(names.iter().all(|e| e.name != b"secret.txt"));
    fs.add_encryption_key(&key).unwrap();
    let f = fs.resolve("/vault/secret.txt").unwrap();
    let mut b = [0u8; 20];
    let n = fs.read(f, 0, &mut b).unwrap();
    assert_eq!(&b[..n], b"top secret");
}

/// The images are also readable by `debugfs` from e2fsprogs, which knows
/// nothing about keys: it lists the same inodes.
#[test]
fn debugfs_agrees_on_inodes() {
    let (img, m) = fixture("fscrypt-1k");
    let out = Command::new(tool("debugfs"))
        .args(["-R", "ls -l /v2"])
        .arg(&img.path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let linux = expected(&m["without_key"]);
    let inos: Vec<u64> = linux
        .iter()
        .filter(|(p, _)| p.starts_with("v2/") && p[3..].find('/').is_none())
        .map(|(_, e)| e.ino)
        .collect();
    for ino in inos {
        assert!(
            text.contains(&format!(" {ino} ")),
            "inode {ino} not listed by debugfs:\n{text}"
        );
    }
}

// --- LUKS ----------------------------------------------------------------------

fn luks_check(name: &str) {
    let (img, m) = fixture(name);
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(&img.path, false).unwrap());
    let h = Header::read(&*dev).unwrap().expect("LUKS header");
    h.check_supported().unwrap();
    assert!(h.unlock(&*dev, b"not the passphrase").unwrap().is_none(), "{name}");
    let mut key = None;
    for p in m["passphrases"].as_array().unwrap() {
        let k = h.unlock(&*dev, p.as_str().unwrap().as_bytes()).unwrap();
        assert!(k.is_some(), "{name}: passphrase {p}");
        key = k;
    }
    let key = key.unwrap();
    {
        let crypt = Arc::new(h.open(dev.clone(), &key).unwrap());
        let mut fs = Fs::mount(crypt, MountOptions::default()).unwrap();
        assert_eq!(fs.label(), "luksdata");
        assert_same(&walk(&mut fs), &expected(&m["files"]), name);
        // write through the encryption, then check the decrypted volume
        let f = fs
            .create(fs.root(), b"from-mac.bin", FileType::Regular, 0o644, 0, 0, 0)
            .unwrap();
        fs.write(f.ino, 1, &common::pattern(123_457, 9)).unwrap();
        fs.unmount().unwrap();
    }
    // e2fsck on a decrypted copy
    let crypt = h.open(dev.clone(), &key).unwrap();
    let plain = img.dir.path().join("plain.img");
    let mut buf = vec![0u8; crypt.size() as usize];
    crypt.read_at(0, &mut buf).unwrap();
    std::fs::write(&plain, &buf).unwrap();
    let out = Command::new(tool("e2fsck")).args(["-fn"]).arg(&plain).output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{name}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut fs = Fs::mount(
        Arc::new(crypt),
        MountOptions {
            read_only: true,
            ..Default::default()
        },
    )
    .unwrap();
    let ino = fs.resolve("/from-mac.bin").unwrap();
    let mut b = vec![0u8; 123_458];
    fs.read(ino, 0, &mut b).unwrap();
    assert_eq!(b[0], 0);
    assert_eq!(&b[1..], &common::pattern(123_457, 9)[..]);
}

#[test]
fn luks1_xts() {
    luks_check("luks1-xts");
}

#[test]
fn luks1_cbc_essiv() {
    luks_check("luks1-cbc-essiv");
}

#[test]
fn luks2_argon2_two_slots() {
    luks_check("luks2-argon2id");
}

#[test]
fn luks2_pbkdf2_4k_sectors() {
    luks_check("luks2-pbkdf2-4k");
}

/// cryptsetup defaults (Argon2id with ~780 MiB and 15 passes): slow.
#[test]
#[ignore]
fn luks2_default_parameters() {
    let t = std::time::Instant::now();
    luks_check("luks2-default");
    eprintln!("luks2-default: {:?}", t.elapsed());
}

#[test]
fn luks_headers_describe() {
    let (img, _) = fixture("luks2-argon2id");
    let h = Header::read(&FileDevice::open(&img.path, true).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(h.version, 2);
    assert_eq!(h.label, "testlabel");
    assert_eq!(h.sector_size, 4096);
    assert_eq!(h.data_offset, 16 << 20);
    assert_eq!(h.keyslots.len(), 2);
    assert!(h.describe().contains("argon2id"), "{}", h.describe());
    // an ext4 image is not LUKS
    let plain = Image::new(8, &["-t", "ext4"]);
    assert!(
        Header::read(&FileDevice::open(&plain.path, true).unwrap())
            .unwrap()
            .is_none()
    );
}
