//! Open file descriptors (the proxy descriptors of the documents provider)
//! and their read-ahead. The system passes reads of 128 KiB at most, one
//! at a time, so every request would wait for the disk. While a descriptor
//! is read in order, the next window of the file is read on another thread
//! as the app takes in the current one: the disk and the app's requests
//! overlap, and the disk gets larger reads.
//!
//! A window is valid while the inode's data has not changed since it was
//! read ([`Volume::changes`]); it is dropped otherwise.

use crate::volumes::Volume;
use ext4_core::{Ino, Result};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Bytes read ahead at a time.
const WINDOW: usize = 1 << 20;

pub struct OpenFile {
    pub ino: Ino,
    ahead: Mutex<ReadAhead>,
}

#[derive(Default)]
struct ReadAhead {
    /// End of the last read: a read starting here continues in order.
    next: u64,
    window: Option<Window>,
    /// The window being read on another thread: its start and the thread.
    pending: Option<(u64, JoinHandle<Result<Window>>)>,
}

/// File data read ahead while the inode had had `changes` changes.
struct Window {
    start: u64,
    data: Vec<u8>,
    changes: u64,
}

impl Window {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }

    /// The file ends inside this window.
    fn at_end_of_file(&self) -> bool {
        self.data.len() < WINDOW
    }

    fn holds(&self, at: u64, changes: u64) -> bool {
        self.changes == changes && self.start <= at && at < self.end()
    }
}

impl ReadAhead {
    /// Make the window being read the current one if it holds `at`;
    /// a prefetch for elsewhere is left to finish on its own.
    fn take_pending(&mut self, at: u64, changes: u64) {
        let Some((start, _)) = &self.pending else {
            return;
        };
        if !(*start <= at && at < start + WINDOW as u64) {
            self.pending = None;
            return;
        }
        let (_, thread) = self.pending.take().unwrap();
        // a failed prefetch is retried by the direct read, which reports it
        self.window = match thread.join() {
            Ok(Ok(w)) if w.changes == changes => Some(w),
            _ => None,
        };
    }

    /// Keep one window ahead of a reader now at `at`: the one after the
    /// current window, or at `at` when there is none.
    fn prefetch(&mut self, vol: &Arc<Volume>, ino: Ino, at: u64, changes: u64) {
        if self.pending.is_some() {
            return;
        }
        let start = match &self.window {
            Some(w) if w.changes == changes && w.start <= at && at <= w.end() => {
                if w.at_end_of_file() {
                    return;
                }
                w.end()
            }
            _ => at,
        };
        let vol = vol.clone();
        let thread = std::thread::Builder::new().name("ext4-readahead".into()).spawn(move || {
            let mut data = vec![0u8; WINDOW];
            let n = read_at(&vol, ino, start, &mut data)?;
            data.truncate(n);
            Ok(Window { start, data, changes })
        });
        match thread {
            Ok(t) => self.pending = Some((start, t)),
            Err(e) => log::warn!("no read-ahead thread: {e}"),
        }
    }
}

impl OpenFile {
    pub fn new(ino: Ino) -> OpenFile {
        OpenFile {
            ino,
            ahead: Mutex::default(),
        }
    }

    /// Read at `offset` until `buf` is full or the file ends.
    pub fn read(&self, vol: &Arc<Volume>, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut ra = self.ahead.lock().unwrap_or_else(|e| e.into_inner());
        let changes = vol.changes(self.ino);
        let in_order = offset == ra.next;
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            if !ra.window.as_ref().is_some_and(|w| w.holds(at, changes)) {
                ra.take_pending(at, changes);
            }
            match &ra.window {
                Some(w) if w.holds(at, changes) => {
                    let from = (at - w.start) as usize;
                    let n = (w.data.len() - from).min(buf.len() - done);
                    buf[done..done + n].copy_from_slice(&w.data[from..from + n]);
                    done += n;
                    if w.at_end_of_file() && at + n as u64 == w.end() {
                        break;
                    }
                }
                _ => {
                    done += read_at(vol, self.ino, at, &mut buf[done..])?;
                    break;
                }
            }
        }
        ra.next = offset + done as u64;
        if in_order && done == buf.len() {
            let next = ra.next;
            ra.prefetch(vol, self.ino, next, changes);
        }
        Ok(done)
    }
}

/// Read until `buf` is full or the file ends; the device reads run outside
/// the file system lock.
fn read_at(vol: &Volume, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        let n = vol.fs.read_parallel(ino, offset + done as u64, &mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ops, volumes};
    use ext4_core::{FormatOptions, MemDevice, MountOptions};

    /// Not repeating within a window, so data from the wrong offset shows.
    fn pattern(offset: u64) -> u8 {
        (offset % 251) as u8 ^ (offset >> 20) as u8
    }

    /// A writable volume with `data.bin` of `len` bytes; its descriptor.
    fn file(len: u64) -> (u32, Arc<Volume>, u64, Arc<OpenFile>) {
        let dev = Arc::new(MemDevice::new(32 << 20));
        ext4_core::format(&*dev, &FormatOptions::default(), &mut |_, _| {}).unwrap();
        let fs = ext4_core::Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        drop(fs);
        let id = volumes::mount_device(dev, false).unwrap();
        let vol = volumes::get(id).unwrap();
        ops::create(&vol, "", "data.bin", false).unwrap();
        let (h, _) = ops::open(&vol, "data.bin", false).unwrap();
        let f = vol.file(h).unwrap();
        let data: Vec<u8> = (0..len).map(pattern).collect();
        ops::write(&vol, f.ino, 0, &data).unwrap();
        (id, vol, h, f)
    }

    fn check(buf: &[u8], offset: u64) {
        for (i, &b) in buf.iter().enumerate() {
            assert_eq!(b, pattern(offset + i as u64), "byte at {}", offset + i as u64);
        }
    }

    #[test]
    fn reads_in_order_are_read_ahead_and_correct() {
        // not a whole number of windows or requests
        let len = 5 * WINDOW as u64 + 12_345;
        let (id, vol, h, f) = file(len);
        let mut buf = vec![0u8; 128 << 10];
        let mut at = 0;
        loop {
            let n = f.read(&vol, at, &mut buf).unwrap();
            check(&buf[..n], at);
            at += n as u64;
            if n < buf.len() {
                break;
            }
            if at >= 2 * buf.len() as u64 {
                let ra = f.ahead.lock().unwrap();
                assert!(ra.window.is_some() || ra.pending.is_some(), "nothing read ahead at {at}");
            }
        }
        assert_eq!(at, len);
        assert_eq!(f.read(&vol, len, &mut buf).unwrap(), 0);
        drop(f);
        vol.close_file(h, false).unwrap();
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    #[test]
    fn reads_out_of_order_and_across_windows() {
        let len = 3 * WINDOW as u64;
        let (id, vol, h, f) = file(len);
        let mut rng = 0x2545_F491u64;
        for _ in 0..300 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let at = rng % (len + 1000);
            let mut buf = vec![0u8; (rng >> 40) as usize % 300_000];
            let n = f.read(&vol, at, &mut buf).unwrap();
            assert_eq!(n as u64, (buf.len() as u64).min(len.saturating_sub(at)), "at {at}");
            check(&buf[..n], at);
            // and on in order from there, through window edges
            let m = f.read(&vol, at + n as u64, &mut buf).unwrap();
            check(&buf[..m], at + n as u64);
        }
        drop(f);
        vol.close_file(h, false).unwrap();
        drop(vol);
        volumes::unmount(id).unwrap();
    }

    /// Writes and truncation, through this descriptor or another, are
    /// seen by the next read: read-ahead never serves old data.
    #[test]
    fn changes_drop_what_was_read_ahead() {
        let len = 3 * WINDOW as u64;
        let (id, vol, h, f) = file(len);
        let mut buf = vec![0u8; 128 << 10];
        for i in 0..3 {
            f.read(&vol, i * buf.len() as u64, &mut buf).unwrap();
        }
        // the next windows hold the old data; overwrite parts of them
        let at = 3 * buf.len() as u64;
        ops::write(&vol, f.ino, at, &[0xAA; 1000]).unwrap();
        ops::write(&vol, f.ino, WINDOW as u64 + 5, &[0xBB; 10]).unwrap();
        assert_eq!(f.read(&vol, at, &mut buf).unwrap(), buf.len());
        assert!(buf[..1000].iter().all(|&b| b == 0xAA));
        check(&buf[1000..], at + 1000);
        let mut b = [0u8; 20];
        f.read(&vol, WINDOW as u64, &mut b).unwrap();
        assert_eq!(&b[5..15], &[0xBB; 10]);

        // emptied through another descriptor ("wt")
        for i in 0..3 {
            f.read(&vol, i * buf.len() as u64, &mut buf).unwrap();
        }
        let (other, size) = ops::open(&vol, "data.bin", true).unwrap();
        assert_eq!(size, 0);
        assert_eq!(f.read(&vol, 3 * buf.len() as u64, &mut buf).unwrap(), 0);
        vol.close_file(other, true).unwrap();
        drop(f);
        vol.close_file(h, false).unwrap();
        drop(vol);
        volumes::unmount(id).unwrap();
    }
}
