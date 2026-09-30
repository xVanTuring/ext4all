//! End-to-end tests of the `ext4-tool` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

fn sbin(tool: &str) -> PathBuf {
    let d = std::env::var("E2FSPROGS_SBIN").unwrap_or_else(|_| "/opt/homebrew/opt/e2fsprogs/sbin".into());
    PathBuf::from(d).join(tool)
}

fn mkfs(dir: &Path) -> PathBuf {
    let p = dir.join("fs.img");
    std::fs::File::create(&p).unwrap().set_len(32 << 20).unwrap();
    let st = Command::new(sbin("mke2fs"))
        .args(["-F", "-q", "-t", "ext4", "-L", "cli"])
        .arg(&p)
        .status()
        .unwrap();
    assert!(st.success());
    p
}

fn tool(img: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ext4-tool"))
        .arg(img)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn ok(img: &Path, args: &[&str]) -> String {
    let (s, o) = tool(img, args);
    assert!(s, "ext4-tool {args:?} failed: {o}");
    o
}

fn fsck_clean(img: &Path) {
    let out = Command::new(sbin("e2fsck")).arg("-fn").arg(img).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn cli_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let img = mkfs(d.path());
    let host = d.path().join("host.bin");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(&host, &data).unwrap();

    assert!(ok(&img, &["info"]).contains("label:        cli"));
    ok(&img, &["mkdir", "/docs"]);
    ok(&img, &["put", host.to_str().unwrap(), "/docs/data.bin"]);
    ok(&img, &["symlink", "docs/data.bin", "/link"]);
    ok(&img, &["ln", "/docs/data.bin", "/hard"]);
    ok(&img, &["xattr-set", "/hard", "user.k", "v"]);
    assert_eq!(ok(&img, &["xattr-get", "/docs/data.bin", "user.k"]), "v");
    assert!(ok(&img, &["xattr-list", "/hard"]).contains("user.k"));
    assert_eq!(ok(&img, &["readlink", "/link"]).trim(), "docs/data.bin");
    let out = d.path().join("out.bin");
    ok(&img, &["get", "/hard", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(&out).unwrap(), data);
    let ls = ok(&img, &["ls", "-l", "/"]);
    assert!(
        ls.contains("docs") && ls.contains("link") && ls.contains("hard"),
        "{ls}"
    );
    assert!(ok(&img, &["stat", "/hard"]).contains("links:  2"));
    ok(&img, &["mv", "/hard", "/docs/renamed"]);
    ok(&img, &["truncate", "/docs/renamed", "10"]);
    assert_eq!(ok(&img, &["cat", "/docs/renamed"]).as_bytes(), &data[..10]);
    ok(&img, &["chmod", "600", "/docs/renamed"]);
    assert!(ok(&img, &["stat", "/docs/renamed"]).contains("mode:   100600"));
    ok(&img, &["rm", "/docs/renamed"]);
    ok(&img, &["rm", "/docs/data.bin"]);
    ok(&img, &["rmdir", "/docs"]);
    ok(&img, &["label", "renamed"]);
    assert!(ok(&img, &["info"]).contains("label:        renamed"));
    let tree = ok(&img, &["tree"]);
    assert!(tree.contains("lost+found/") && tree.contains("link"), "{tree}");
    assert!(ok(&img, &["recover"]).contains("journal replayed: false"));
    fsck_clean(&img);
}

#[test]
fn cli_errors() {
    let d = tempfile::tempdir().unwrap();
    let img = mkfs(d.path());
    let (s, o) = tool(&img, &["cat", "/missing"]);
    assert!(!s && o.contains("no such file"), "{o}");
    let (s, o) = tool(&img, &["bogus"]);
    assert!(!s && o.contains("unknown command"), "{o}");
    let (s, o) = tool(&img, &["mkdir"]);
    assert!(!s && o.contains("missing arguments"), "{o}");
    let (s, _) = tool(Path::new("/nonexistent.img"), &["info"]);
    assert!(!s);
    fsck_clean(&img);
}
