//! Crash consistency: interrupt commits at every write, then recover both
//! with e2fsck and with our own journal replay.

mod common;
use common::*;
use ext4_core::{BlockDevice, FileType, Fs, MemDevice, MountOptions, RenameFlags};
use std::sync::Arc;

fn load(img: &Image) -> Arc<MemDevice> {
    Arc::new(MemDevice::from_vec(std::fs::read(&img.path).unwrap()))
}

fn save(img: &Image, dev: &MemDevice) {
    std::fs::write(&img.path, dev.snapshot()).unwrap();
}

fn no_auto_commit() -> MountOptions {
    MountOptions {
        commit_threshold: usize::MAX,
        ..Default::default()
    }
}

fn read_all(fs: &mut Fs, ino: u32) -> Vec<u8> {
    let size = fs.stat(ino).unwrap().size as usize;
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        done += fs.read(ino, done as u64, &mut buf[done..]).unwrap();
    }
    buf
}

/// Build a base image with some committed content.
fn base_image(opts: &[&str]) -> Image {
    let img = Image::new(32, opts);
    let mut fs = img.mount();
    let root = fs.root();
    let d = fs.mkdir(root, b"keep", 0o755, 0, 0).unwrap().ino;
    let f = fs.create(d, b"old", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
    fs.write(f, 0, &pattern(40_000, 1)).unwrap();
    let v = fs
        .create(root, b"victim", FileType::Regular, 0o644, 0, 0, 0)
        .unwrap()
        .ino;
    fs.write(v, 0, &pattern(9000, 2)).unwrap();
    fs.unmount().unwrap();
    img.assert_clean();
    img
}

/// The transaction under test.
fn workload(fs: &mut Fs) {
    let root = fs.root();
    let n = fs.mkdir(root, b"new", 0o755, 0, 0).unwrap().ino;
    for i in 0..20 {
        let f = fs
            .create(n, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
            .unwrap()
            .ino;
        fs.write(f, 0, &pattern(3000 + i * 100, i as u64)).unwrap();
    }
    fs.unlink(root, b"victim").unwrap();
    let keep = fs.lookup(root, b"keep").unwrap();
    fs.rename(keep, b"old", n, b"moved", RenameFlags::default()).unwrap();
}

/// Either the whole workload is visible or none of it.
fn check_atomic(fs: &mut Fs) -> bool {
    let root = fs.root();
    let has_new = fs.lookup(root, b"new").is_ok();
    let has_victim = fs.lookup(root, b"victim").is_ok();
    let keep = fs.lookup(root, b"keep").unwrap();
    let has_old = fs.lookup(keep, b"old").is_ok();
    if has_new {
        assert!(!has_victim && !has_old, "partial transaction visible");
        let n = fs.lookup(root, b"new").unwrap();
        for i in 0..20u64 {
            let f = fs.lookup(n, format!("f{i}").as_bytes()).unwrap();
            assert_eq!(read_all(fs, f), pattern(3000 + i as usize * 100, i));
        }
        let m = fs.lookup(n, b"moved").unwrap();
        assert_eq!(read_all(fs, m), pattern(40_000, 1));
        true
    } else {
        assert!(has_victim && has_old, "partial transaction visible");
        let v = fs.lookup(root, b"victim").unwrap();
        assert_eq!(read_all(fs, v), pattern(9000, 2));
        false
    }
}

fn crash_at_every_write(opts: &[&str]) {
    let base = base_image(opts);
    let mut saw_old = false;
    let mut saw_new = false;
    for fail_after in 0..400usize {
        let img = base.copy();
        let dev = load(&img);
        let mut fs = Fs::mount(dev.clone(), no_auto_commit()).unwrap();
        workload(&mut fs);
        dev.fail_writes_after(Some(fail_after));
        let res = fs.commit();
        std::mem::forget(fs); // power loss: no unmount
        dev.fail_writes_after(None);
        save(&img, &dev);

        // recovery by e2fsck
        let c = img.copy();
        let (code, out) = c.fsck_fix();
        assert!(code <= 1, "fail_after={fail_after}: e2fsck -fy exit {code}\n{out}");
        assert!(
            !out.contains("Fix? yes"),
            "fail_after={fail_after}: e2fsck had to repair\n{out}"
        );
        c.assert_clean();

        // recovery by our own mount
        {
            let mut fs = img.mount();
            if check_atomic(&mut fs) {
                saw_new = true;
            } else {
                saw_old = true;
            }
            fs.unmount().unwrap();
        }
        img.assert_clean();
        if res.is_ok() {
            break;
        }
    }
    assert!(saw_old && saw_new, "expected to observe both outcomes");
}

#[test]
fn crash_during_commit_4k() {
    crash_at_every_write(&["-t", "ext4", "-b", "4096"]);
}

#[test]
fn crash_during_commit_1k() {
    crash_at_every_write(&["-t", "ext4", "-b", "1024"]);
}

#[test]
fn crash_during_commit_no_csum() {
    crash_at_every_write(&["-t", "ext4", "-O", "^metadata_csum,^metadata_csum_seed"]);
}

#[test]
fn replay_on_read_only_mount_uses_overlay() {
    let base = base_image(&["-t", "ext4"]);
    let img = base.copy();
    let dev = load(&img);
    let mut fs = Fs::mount(dev.clone(), no_auto_commit()).unwrap();
    workload(&mut fs);
    // everything up to and including the commit block, then power loss
    dev.fail_writes_after(Some(4));
    assert!(fs.commit().is_err());
    std::mem::forget(fs);
    dev.fail_writes_after(None);
    save(&img, &dev);
    let before = std::fs::read(&img.path).unwrap();
    {
        let mut fs = img.mount_ro();
        assert!(fs.mount_report().journal_replayed);
        assert!(check_atomic(&mut fs), "committed transaction must be visible");
    }
    // read-only mount must not have modified the image
    assert_eq!(before, std::fs::read(&img.path).unwrap());
}

#[test]
fn journal_replayed_by_rw_mount_is_reported() {
    let base = base_image(&["-t", "ext4"]);
    let img = base.copy();
    let dev = load(&img);
    let mut fs = Fs::mount(dev.clone(), no_auto_commit()).unwrap();
    workload(&mut fs);
    dev.fail_writes_after(Some(4));
    assert!(fs.commit().is_err());
    std::mem::forget(fs);
    dev.fail_writes_after(None);
    save(&img, &dev);
    let mut fs = img.mount();
    let r = fs.mount_report().clone();
    assert!(r.journal_replayed);
    assert_eq!(r.replayed_transactions, 1);
    assert!(r.replayed_blocks > 5);
    assert!(check_atomic(&mut fs));
    fs.unmount().unwrap();
    img.assert_clean();
}

#[test]
fn crash_without_commit_loses_only_uncommitted_work() {
    let base = base_image(&["-t", "ext4"]);
    let img = base.copy();
    let dev = load(&img);
    let mut fs = Fs::mount(dev.clone(), no_auto_commit()).unwrap();
    workload(&mut fs);
    std::mem::forget(fs);
    save(&img, &dev);
    let c = img.copy();
    let (code, out) = c.fsck_fix();
    assert!(code <= 1, "{out}");
    c.assert_clean();
    let mut fs = img.mount();
    assert!(!check_atomic(&mut fs));
    fs.unmount().unwrap();
    img.assert_clean();
}

#[test]
fn orphan_survives_crash_and_is_freed_on_next_mount() {
    let img = Image::new(32, &["-t", "ext4"]);
    let free_before;
    {
        let fs = img.mount();
        free_before = fs.statfs().free_blocks;
        fs.unmount().unwrap();
    }
    let dev = load(&img);
    {
        let mut fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        fs.set_defer_unlinked(true);
        let root = fs.root();
        let f = fs
            .create(root, b"open-file", FileType::Regular, 0o644, 0, 0, 0)
            .unwrap()
            .ino;
        fs.write(f, 0, &pattern(200_000, 5)).unwrap();
        fs.unlink(root, b"open-file").unwrap();
        // still readable through the inode while "open"
        assert_eq!(read_all(&mut fs, f), pattern(200_000, 5));
        fs.commit().unwrap();
        std::mem::forget(fs);
    }
    save(&img, &dev);
    let c = img.copy();
    let (code, out) = c.fsck_fix();
    assert!(code <= 1, "{out}");
    c.assert_clean();
    {
        let fs = img.mount();
        assert_eq!(fs.mount_report().orphans_processed, 1);
        assert_eq!(fs.statfs().free_blocks, free_before);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn reclaim_releases_deferred_orphans() {
    let img = Image::new(32, &["-t", "ext4"]);
    {
        let mut fs = img.mount();
        fs.set_defer_unlinked(true);
        let root = fs.root();
        let before = fs.statfs().free_blocks;
        let f = fs.create(root, b"x", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.write(f, 0, &pattern(100_000, 1)).unwrap();
        fs.unlink(root, b"x").unwrap();
        assert!(fs.statfs().free_blocks < before);
        fs.reclaim(f).unwrap();
        assert_eq!(fs.statfs().free_blocks, before);
        assert!(fs.stat(f).is_err());
        // an orphan that is never reclaimed is released at unmount
        let g = fs.create(root, b"y", FileType::Regular, 0o644, 0, 0, 0).unwrap().ino;
        fs.write(g, 0, b"data").unwrap();
        fs.unlink(root, b"y").unwrap();
        let d = fs.mkdir(root, b"dir", 0o755, 0, 0).unwrap().ino;
        fs.rmdir(root, b"dir").unwrap();
        let _ = d;
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn orphan_list_from_linux_style_image_is_processed() {
    // Build an orphan with debugfs: an inode with links 0 on s_last_orphan.
    let img = Image::new(32, &["-t", "ext4", "-O", "^orphan_file"]);
    std::fs::write(img.dir.path().join("data"), pattern(50_000, 3)).unwrap();
    let data = img.dir.path().join("data");
    img.debugfs_w(&[&format!("write {} orph", data.display())]);
    let out = img.debugfs(&["stat /orph"]);
    let ino: u32 = out
        .split("Inode: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    img.debugfs_w(&[
        "unlink /orph",
        &format!("sif <{ino}> links_count 0"),
        &format!("ssv last_orphan {ino}"),
    ]);
    {
        let fs = img.mount();
        assert_eq!(fs.mount_report().orphans_processed, 1);
        fs.unmount().unwrap();
    }
    img.assert_clean();
}

#[test]
fn memory_device_flush_ordering() {
    // Each journal commit must issue several flushes (ordering barriers).
    let img = Image::new(32, &["-t", "ext4"]);
    let dev = load(&img);
    let mut fs = Fs::mount(dev.clone(), no_auto_commit()).unwrap();
    let root = fs.root();
    fs.create(root, b"a", FileType::Regular, 0o644, 0, 0, 0).unwrap();
    let before = dev.flush_count();
    fs.commit().unwrap();
    assert!(dev.flush_count() - before >= 4);
    fs.unmount().unwrap();
    let _ = dev.size();
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, m: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % m
    }
}

/// Random operations with frequent commits, interrupted by a power loss at
/// a random write. Recovery must never need repairs.
fn random_power_loss(opts: &[&str], seeds: std::ops::Range<u64>) {
    use ext4_core::Error;
    let base = Image::new(32, opts);
    let total = seeds.end - seeds.start;
    let mut crashed_mid_op = 0;
    for seed in seeds {
        let img = base.copy();
        let dev = load(&img);
        let mut rng = Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1);
        let mut fs = Fs::mount(
            dev.clone(),
            MountOptions {
                commit_threshold: 8 + rng.next(64) as usize,
                ..Default::default()
            },
        )
        .unwrap();
        let root = fs.root();
        let dirs = [root, fs.mkdir(root, b"d", 0o755, 0, 0).unwrap().ino];
        let crash_at = 1 + rng.next(60) as usize;
        let mut armed = false;
        for step in 0..400 {
            if step == 40 && !armed {
                dev.fail_writes_after(Some(crash_at));
                armed = true;
            }
            let d = dirs[rng.next(2) as usize];
            let name = format!("n{}", rng.next(12));
            let res: ext4_core::Result<()> = match rng.next(8) {
                0 | 1 => fs
                    .create(d, name.as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                    .map(|_| ()),
                2 | 3 => match fs.lookup(d, name.as_bytes()) {
                    Ok(ino) if fs.stat(ino).map(|a| !a.is_dir()).unwrap_or(false) => {
                        let off = rng.next(200_000);
                        let len = 1 + rng.next(50_000) as usize;
                        fs.write(ino, off, &pattern(len, seed ^ step)).map(|_| ())
                    }
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                },
                4 => match fs.lookup(d, name.as_bytes()) {
                    Ok(ino) if fs.stat(ino).map(|a| !a.is_dir()).unwrap_or(false) => {
                        fs.truncate(ino, rng.next(100_000)).map(|_| ())
                    }
                    Ok(_) => Ok(()),
                    Err(e) => Err(e),
                },
                5 => fs.unlink(d, name.as_bytes()),
                6 => {
                    let d2 = dirs[rng.next(2) as usize];
                    let n2 = format!("n{}", rng.next(12));
                    fs.rename(d, name.as_bytes(), d2, n2.as_bytes(), RenameFlags::default())
                }
                _ => fs.mkdir(d, name.as_bytes(), 0o755, 0, 0).map(|_| ()),
            };
            if let Err(Error::Device(_)) = res {
                // power is gone
                crashed_mid_op += 1;
                break;
            }
        }
        // make sure the failure point is reached even if ops were cheap
        let _ = fs.commit();
        std::mem::forget(fs);
        dev.fail_writes_after(None);
        save(&img, &dev);

        let c = img.copy();
        let (code, out) = c.fsck_fix();
        assert!(code <= 1, "seed {seed}: e2fsck -fy exit {code}\n{out}");
        assert!(!out.contains("Fix? yes"), "seed {seed}: e2fsck had to repair\n{out}");
        c.assert_clean();

        let fs = img.mount();
        fs.unmount().unwrap();
        let (code, out) = img.fsck();
        assert!(
            code == 0 && !out.contains("Fix? no"),
            "seed {seed}: after our recovery\n{out}"
        );
    }
    assert!(
        crashed_mid_op * 2 >= total,
        "only {crashed_mid_op}/{total} runs lost power inside an operation"
    );
}

#[test]
fn random_ops_with_power_loss_4k() {
    random_power_loss(&["-t", "ext4", "-b", "4096"], 1..16);
}

#[test]
fn random_ops_with_power_loss_1k() {
    random_power_loss(&["-t", "ext4", "-b", "1024"], 100..116);
}

#[test]
fn random_ops_with_power_loss_no_csum() {
    random_power_loss(&["-t", "ext4", "-O", "^metadata_csum,^metadata_csum_seed"], 200..210);
}

/// Long soak (run with `cargo test --release -- --ignored`).
#[test]
#[ignore]
fn random_ops_with_power_loss_soak() {
    random_power_loss(&["-t", "ext4", "-b", "4096"], 1000..1400);
    random_power_loss(&["-t", "ext4", "-b", "1024"], 2000..2400);
}

/// Without a journal a crash may leave damage (as on Linux). The volume
/// must be flagged as not clean so fsck runs, and fsck must be able to
/// repair it.
#[test]
fn power_loss_without_journal_is_detected_and_repairable() {
    let base = Image::new(32, &["-t", "ext4", "-O", "^has_journal"]);
    for seed in 0..6u64 {
        let img = base.copy();
        let dev = load(&img);
        let mut fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        let root = fs.root();
        for i in 0..30u64 {
            let a = fs
                .create(root, format!("f{i}").as_bytes(), FileType::Regular, 0o644, 0, 0, 0)
                .unwrap();
            fs.write(a.ino, 0, &pattern(3000, i)).unwrap();
        }
        dev.fail_writes_after(Some(3 + seed as usize * 5));
        let _ = fs.commit();
        std::mem::forget(fs);
        dev.fail_writes_after(None);
        save(&img, &dev);
        // state is "not clean" so e2fsck -p / boot-time fsck will check it
        assert!(
            img.dumpe2fs().contains("Filesystem state:         not clean"),
            "seed {seed}"
        );
        let (code, out) = img.fsck_fix();
        assert!(code <= 1, "seed {seed}: e2fsck could not repair\n{out}");
        img.assert_clean();
        let fs = img.mount();
        fs.unmount().unwrap();
        img.assert_clean();
    }
}

#[test]
fn random_ops_with_power_loss_ext3() {
    random_power_loss(&["-t", "ext3", "-b", "1024"], 300..316);
}
