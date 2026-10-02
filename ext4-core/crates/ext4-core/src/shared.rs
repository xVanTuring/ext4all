//! A mounted file system shared across threads, with periodic commits:
//! what a platform layer (the macOS FSKit extension, the Android app) holds
//! while a volume is mounted.

use crate::error::errno::EIO;
use crate::fs::{read_pieces, write_pieces};
use crate::{Error, Fs, Ino, Result};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

struct Shared {
    fs: Mutex<Option<Fs>>,
    /// Held shared by [`SharedFs::read_parallel`] and
    /// [`SharedFs::write_parallel`] until their device transfers finish,
    /// and exclusively around every other operation, so no block they are
    /// moving can be freed and reused meanwhile. Taken before `fs`.
    data: RwLock<()>,
    /// Set after a panic inside an operation: in-memory state may be
    /// inconsistent, so nothing more may be written.
    broken: AtomicBool,
    stop: Mutex<bool>,
    wake: Condvar,
}

pub struct SharedFs {
    shared: Arc<Shared>,
    committer: Mutex<Option<JoinHandle<()>>>,
    read_only: bool,
    block_size: u64,
    /// Blocks reserved by writes that have not finished, oldest first.
    writes: Mutex<Writes>,
    write_done: Condvar,
}

#[derive(Default)]
struct Writes {
    last: u64,
    active: Vec<(u64, Ino, Range<u64>)>,
}

/// Releases a write's reserved blocks however `write_parallel` ends.
struct Release<'a> {
    fs: &'a SharedFs,
    ticket: u64,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut w = self.fs.writes.lock().unwrap_or_else(|e| e.into_inner());
        w.active.retain(|r| r.0 != self.ticket);
        self.fs.write_done.notify_all();
    }
}

impl SharedFs {
    pub fn new(fs: Fs, commit_interval: Duration) -> SharedFs {
        let read_only = fs.is_read_only();
        let block_size = fs.block_size() as u64;
        let shared = Arc::new(Shared {
            fs: Mutex::new(Some(fs)),
            data: RwLock::new(()),
            broken: AtomicBool::new(false),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let committer = if read_only {
            None
        } else {
            let s = shared.clone();
            Some(
                std::thread::Builder::new()
                    .name("ext4-commit".into())
                    .spawn(move || commit_loop(s, commit_interval))
                    .expect("spawn commit thread"),
            )
        };
        SharedFs {
            shared,
            committer: Mutex::new(committer),
            read_only,
            block_size,
            writes: Mutex::default(),
            write_done: Condvar::new(),
        }
    }

    /// Current state (a failed commit or `finish` makes a volume
    /// read-only).
    pub fn is_read_only(&self) -> bool {
        self.read_only || self.with(|fs| Ok(fs.is_read_only())).unwrap_or(true)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<Fs>>> {
        if self.shared.broken.load(Ordering::SeqCst) {
            return Err(Error::Device(EIO));
        }
        self.shared
            .fs
            .lock()
            .map_err(|_| Error::Device(EIO))
    }

    /// Run `f` with the file system locked. Panics mark the handle broken.
    pub fn with<T>(&self, f: impl FnOnce(&mut Fs) -> Result<T>) -> Result<T> {
        let _data = self.shared.data.write().unwrap_or_else(|e| e.into_inner());
        self.with_fs(f)
    }

    /// [`SharedFs::with`] for operations that free no blocks (`stat`):
    /// they need not wait for parallel reads to finish.
    pub fn with_shared<T>(&self, f: impl FnOnce(&mut Fs) -> Result<T>) -> Result<T> {
        let _data = self.shared.data.read().unwrap_or_else(|e| e.into_inner());
        self.with_fs(f)
    }

    /// Read file data like [`Fs::read`], but hold the file system only
    /// while mapping the range: the device reads run unlocked, so
    /// concurrent calls keep several requests in flight on the device.
    /// Inline and encrypted files are read entirely under the lock.
    pub fn read_parallel(&self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let _data = self.shared.data.read().unwrap_or_else(|e| e.into_inner());
        let (plan, dev) = self.with_fs(|fs| Ok((fs.plan_read(ino, offset, buf.len())?, fs.dev.clone())))?;
        match plan {
            Some((len, pieces)) => {
                read_pieces(&*dev, &pieces, &mut buf[..len])?;
                Ok(len)
            }
            None => self.with_fs(|fs| fs.read(ino, offset, buf)),
        }
    }

    /// Reserve the blocks that a write of `len` bytes at `offset` touches,
    /// in the order requests arrive: a [`SharedFs::write_parallel`]
    /// overlapping an earlier unfinished one waits for it, so overlapping
    /// writes reach the disk in this order (and never read-modify-write a
    /// block at the same time). Every ticket must be passed on to
    /// `write_parallel`, which releases it, or to `cancel_write`; until
    /// then later overlapping writes wait.
    pub fn reserve_write(&self, ino: Ino, offset: u64, len: usize) -> u64 {
        let bs = self.block_size;
        let blocks = offset / bs..offset.saturating_add(len as u64).div_ceil(bs);
        let mut w = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        w.last += 1;
        let ticket = w.last;
        w.active.push((ticket, ino, blocks));
        ticket
    }

    /// Write like [`Fs::write`], with the file system locked only to
    /// prepare and to finish: the data goes to the device unlocked, so
    /// concurrent calls keep several writes in flight. `ticket` is from
    /// [`SharedFs::reserve_write`] for the same range. Writes that move
    /// more than whole blocks (see `Fs::plan_write`) run under the lock.
    /// When a write of whole blocks does not fit, it fails with
    /// [`Error::NoSpace`] and writes nothing (under the lock, as much as
    /// fits is written, like [`Fs::write`]).
    pub fn write_parallel(&self, ticket: u64, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
        let _release = Release { fs: self, ticket };
        self.wait_for_earlier_writes(ticket);
        let shared = self.shared.data.read().unwrap_or_else(|e| e.into_inner());
        // when not everything fits, plan_write fails with NoSpace and has
        // changed nothing: the write as a whole fails
        let planned = self.with_fs(|fs| Ok(fs.plan_write(ino, offset, data.len())?.map(|p| (p, fs.dev.clone()))))?;
        let Some((plan, dev)) = planned else {
            drop(shared);
            return self.with(|fs| fs.write(ino, offset, data));
        };
        // on failure the new blocks stay unwritten: nothing becomes visible
        write_pieces(&*dev, self.block_size, &plan.pieces, data)?;
        self.with_fs(|fs| fs.finish_write(ino, offset, data.len(), &plan.unwritten))?;
        Ok(data.len())
    }

    /// Release a ticket from [`SharedFs::reserve_write`] without writing.
    pub fn cancel_write(&self, ticket: u64) {
        drop(Release { fs: self, ticket });
    }

    fn wait_for_earlier_writes(&self, ticket: u64) {
        let mut w = self.writes.lock().unwrap_or_else(|e| e.into_inner());
        let Some((_, ino, blocks)) = w.active.iter().find(|r| r.0 == ticket).cloned() else {
            return;
        };
        while w
            .active
            .iter()
            .any(|r| r.0 < ticket && r.1 == ino && r.2.start < blocks.end && blocks.start < r.2.end)
        {
            w = self.write_done.wait(w).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// [`SharedFs::with`] for callers holding `data` already.
    fn with_fs<T>(&self, f: impl FnOnce(&mut Fs) -> Result<T>) -> Result<T> {
        let mut g = self.lock()?;
        let fs = g.as_mut().ok_or(Error::Busy)?;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(fs)));
        match r {
            Ok(v) => v,
            Err(_) => {
                self.shared.broken.store(true, Ordering::SeqCst);
                log::error!("panic inside file system operation; volume disabled");
                Err(Error::Device(EIO))
            }
        }
    }

    fn stop_committer(&self) {
        *self.shared.stop.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.shared.wake.notify_all();
        if let Some(h) = self.committer.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
    }

    /// Ask the commit thread to commit soon, without waiting for it.
    pub fn request_commit(&self) {
        self.shared.wake.notify_all();
    }

    /// Commit and mark the file system clean but keep it open (read-only)
    /// so late reclaims and attribute requests still succeed.
    pub fn finish(&self) -> Result<()> {
        self.with(|fs| fs.unmount_in_place())
    }

    /// Undo [`SharedFs::finish`] when the volume is mounted again.
    pub fn remount(&self) -> Result<()> {
        self.with(|fs| fs.remount_rw())
    }

    /// Commit, restore the clean on-disk state and release the device.
    pub fn unmount(&self) -> Result<()> {
        self.stop_committer();
        // let parallel reads still on the device finish
        let _data = self.shared.data.write().unwrap_or_else(|e| e.into_inner());
        let mut g = self.shared.fs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut fs) = g.take() else {
            return Ok(());
        };
        if self.shared.broken.load(Ordering::SeqCst) {
            // do not write possibly inconsistent state; leave needs_recovery
            fs.abandon();
            return Err(Error::Device(EIO));
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fs.unmount_in_place()));
        drop(fs);
        match r {
            Ok(v) => v,
            Err(_) => Err(Error::Device(EIO)),
        }
    }
}

impl Drop for SharedFs {
    fn drop(&mut self) {
        if let Err(e) = self.unmount() {
            log::error!("unmount while closing handle failed: {e}");
        }
    }
}

fn commit_loop(s: Arc<Shared>, interval: Duration) {
    let mut stop = s.stop.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let (g, _) = s.wake.wait_timeout(stop, interval).unwrap_or_else(|e| e.into_inner());
        stop = g;
        if *stop {
            return;
        }
        if s.broken.load(Ordering::SeqCst) {
            continue;
        }
        drop(stop);
        // Commit new changes to the journal; once a whole interval passes
        // without changes, checkpoint so the home locations are current
        // while the volume is idle.
        if let Ok(mut g) = s.fs.lock()
            && let Some(fs) = g.as_mut()
            && !fs.is_read_only()
        {
            let work: Option<fn(&mut Fs) -> Result<()>> = if fs.has_pending_changes() {
                Some(Fs::commit)
            } else if fs.has_pending_checkpoint() {
                Some(Fs::checkpoint)
            } else {
                None
            };
            if let Some(work) = work {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(fs)));
                match r {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => log::error!("periodic commit failed: {e}"),
                    Err(_) => {
                        s.broken.store(true, Ordering::SeqCst);
                        log::error!("panic during periodic commit; volume disabled");
                    }
                }
            }
        }
        stop = s.stop.lock().unwrap_or_else(|e| e.into_inner());
    }
}
