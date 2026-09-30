//! A mounted file system shared across threads, with periodic commits.

use ext4_core::{Error, Fs, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct Shared {
    fs: Mutex<Option<Fs>>,
    /// Set after a panic inside an operation: in-memory state may be
    /// inconsistent, so nothing more may be written.
    broken: AtomicBool,
    stop: Mutex<bool>,
    wake: Condvar,
}

pub struct Ext4Handle {
    shared: Arc<Shared>,
    committer: Mutex<Option<JoinHandle<()>>>,
    read_only: bool,
}

impl Ext4Handle {
    pub fn new(fs: Fs, commit_interval: Duration) -> Ext4Handle {
        let read_only = fs.is_read_only();
        let shared = Arc::new(Shared {
            fs: Mutex::new(Some(fs)),
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
        Ext4Handle {
            shared,
            committer: Mutex::new(committer),
            read_only,
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn lock(&self) -> Result<MutexGuard<'_, Option<Fs>>> {
        if self.shared.broken.load(Ordering::SeqCst) {
            return Err(Error::Device(ext4_core::error::errno::EIO));
        }
        self.shared
            .fs
            .lock()
            .map_err(|_| Error::Device(ext4_core::error::errno::EIO))
    }

    /// Run `f` with the file system locked. Panics mark the handle broken.
    pub fn with<T>(&self, f: impl FnOnce(&mut Fs) -> Result<T>) -> Result<T> {
        let mut g = self.lock()?;
        let fs = g.as_mut().ok_or(Error::Busy)?;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(fs)));
        match r {
            Ok(v) => v,
            Err(_) => {
                self.shared.broken.store(true, Ordering::SeqCst);
                log::error!("panic inside file system operation; volume disabled");
                Err(Error::Device(ext4_core::error::errno::EIO))
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

    /// Commit, restore the clean on-disk state and release the device.
    pub fn unmount(&self) -> Result<()> {
        self.stop_committer();
        let mut g = self.shared.fs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut fs) = g.take() else {
            return Ok(());
        };
        if self.shared.broken.load(Ordering::SeqCst) {
            // do not write possibly inconsistent state; leave needs_recovery
            std::mem::forget(fs);
            return Err(Error::Device(ext4_core::error::errno::EIO));
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fs.unmount_in_place()));
        drop(fs);
        match r {
            Ok(v) => v,
            Err(_) => Err(Error::Device(ext4_core::error::errno::EIO)),
        }
    }
}

impl Drop for Ext4Handle {
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
        if let Ok(mut g) = s.fs.lock()
            && let Some(fs) = g.as_mut()
            && fs.has_pending_changes()
        {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fs.commit()));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("periodic commit failed: {e}"),
                Err(_) => {
                    s.broken.store(true, Ordering::SeqCst);
                    log::error!("panic during periodic commit; volume disabled");
                }
            }
        }
        stop = s.stop.lock().unwrap_or_else(|e| e.into_inner());
    }
}
