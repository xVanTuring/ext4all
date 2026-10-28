//! [`Transport`] over Linux usbdevfs ioctls on the file descriptor of an
//! opened USB device. On Android that is
//! `UsbDeviceConnection.getFileDescriptor()`, after `claimInterface`: the
//! same way libusb works there without root.
//!
//! A bulk call larger than one transfer queues several transfers at once
//! (asynchronous URBs), so the host controller always has the next one
//! when a transfer ends. As in libusb, the transfers after the first are
//! continuations: when one ends short or fails, the kernel cancels the
//! rest, and they cannot take the next phase's data (the status wrapper).

use crate::{Error, Result, Transport};
use std::ffi::c_void;
use std::os::fd::RawFd;
use std::time::{Duration, Instant};

/// `struct usbdevfs_ctrltransfer` (linux/usbdevice_fs.h).
#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut c_void,
}

/// `struct usbdevfs_bulktransfer`.
#[repr(C)]
struct BulkTransfer {
    ep: u32,
    len: u32,
    timeout: u32,
    data: *mut c_void,
}

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | nr
}

/// `struct usbdevfs_urb`, without isochronous packet descriptors.
#[repr(C)]
struct Urb {
    kind: u8,
    endpoint: u8,
    status: i32,
    flags: u32,
    buffer: *mut c_void,
    buffer_length: i32,
    actual_length: i32,
    start_frame: i32,
    stream_id: u32,
    error_count: i32,
    signr: u32,
    usercontext: *mut c_void,
}

const USBDEVFS_CONTROL: u32 = ioc(IOC_READ | IOC_WRITE, 0, size_of::<CtrlTransfer>());
const USBDEVFS_BULK: u32 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<BulkTransfer>());
const USBDEVFS_SUBMITURB: u32 = ioc(IOC_READ, 10, size_of::<Urb>());
const USBDEVFS_DISCARDURB: u32 = ioc(0, 11, 0);
const USBDEVFS_REAPURBNDELAY: u32 = ioc(IOC_WRITE, 13, size_of::<*mut c_void>());
const USBDEVFS_CLEAR_HALT: u32 = ioc(IOC_READ, 21, size_of::<u32>());

const URB_TYPE_BULK: u8 = 3;
/// A short IN transfer ends with EREMOTEIO, which cancels the queue.
const URB_SHORT_NOT_OK: u32 = 0x01;
/// Continues the transfer before it: cancelled when that one fails.
const URB_BULK_CONTINUATION: u32 = 0x04;

/// Transfers one bulk call queues at once.
const QUEUE: usize = 4;
/// How long cancelled transfers may take to come back after a timeout.
const DISCARD_WAIT: Duration = Duration::from_secs(5);

/// bmRequestType of class requests to an interface.
const CLASS_INTERFACE_OUT: u8 = 0x21;
const CLASS_INTERFACE_IN: u8 = 0xA1;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

pub struct UsbFs {
    fd: RawFd,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
    max_transfer: usize,
    /// Queued transfers could not all be taken back from the kernel: no
    /// transfer is reaped any more (it would write into memory that is
    /// gone), so every transfer fails.
    broken: bool,
}

impl UsbFs {
    /// `fd` stays owned by the caller (the Java `UsbDeviceConnection`) and
    /// must stay open, with `interface` claimed, while this exists.
    /// `ep_in` and `ep_out` are endpoint addresses (IN has bit 7 set).
    pub fn new(fd: RawFd, interface: u8, ep_in: u8, ep_out: u8) -> UsbFs {
        UsbFs {
            fd,
            interface,
            ep_in,
            ep_out,
            max_transfer: 64 * 1024,
            broken: false,
        }
    }

    fn ioctl(&self, request: u32, arg: *mut c_void) -> Result<usize> {
        // SAFETY: `arg` points to the structure `request` expects, valid
        // for the duration of the call, and its data pointer covers `len`
        let r = unsafe { libc::ioctl(self.fd, request as _, arg) };
        if r >= 0 {
            return Ok(r as usize);
        }
        Err(error(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)))
    }

    fn bulk(&mut self, ep: u8, data: *mut c_void, len: usize, timeout: Duration) -> Result<usize> {
        if self.broken {
            return Err(Error::Os(libc::EIO));
        }
        if len > self.max_transfer {
            return self.bulk_queued(ep, data as *mut u8, len, timeout);
        }
        let mut t = BulkTransfer {
            ep: ep as u32,
            len: len as u32,
            timeout: millis(timeout),
            data,
        };
        self.ioctl(USBDEVFS_BULK, &mut t as *mut BulkTransfer as *mut c_void)
    }

    /// Move `len` bytes in transfers of at most `max_transfer`, queued
    /// at once; returns the bytes moved, fewer after a short packet.
    fn bulk_queued(&mut self, ep: u8, data: *mut u8, len: usize, timeout: Duration) -> Result<usize> {
        let inbound = ep & 0x80 != 0;
        let mut urbs: Vec<Urb> = (0..len.div_ceil(self.max_transfer))
            .map(|i| {
                let at = i * self.max_transfer;
                let mut flags = if inbound { URB_SHORT_NOT_OK } else { 0 };
                if i > 0 {
                    flags |= URB_BULK_CONTINUATION;
                }
                Urb {
                    kind: URB_TYPE_BULK,
                    endpoint: ep,
                    status: 0,
                    flags,
                    // SAFETY: `at` is inside the caller's `len` bytes
                    buffer: unsafe { data.add(at) } as *mut c_void,
                    buffer_length: (len - at).min(self.max_transfer) as i32,
                    actual_length: 0,
                    start_frame: 0,
                    stream_id: 0,
                    error_count: 0,
                    signr: 0,
                    usercontext: std::ptr::null_mut(),
                }
            })
            .collect();
        let mut total = 0;
        let mut next = 0;
        while next < urbs.len() {
            // queue what the kernel takes; ENOMEM may come before the end
            let first = next;
            let mut refused = None;
            while next < urbs.len() {
                match self.ioctl(USBDEVFS_SUBMITURB, &mut urbs[next] as *mut Urb as *mut c_void) {
                    Ok(_) => next += 1,
                    Err(e) => {
                        refused = Some(e);
                        break;
                    }
                }
            }
            if next == first {
                let e = refused.unwrap();
                if total == 0 || e.is_disconnected() {
                    return Err(e);
                }
                // data moved already; the command starts over (see Disk)
                return Err(Error::Protocol(format!("transfer refused after {total} bytes: {e}")));
            }
            // the kernel writes into the transfers and the buffer when they
            // are reaped: all of them are, before returning
            self.reap(&mut urbs[first..next], timeout)?;
            for u in &urbs[first..next] {
                match -u.status {
                    0 | libc::EREMOTEIO => {
                        total += u.actual_length.max(0) as usize;
                        if u.actual_length < u.buffer_length {
                            return Ok(total); // short packet: the phase ended
                        }
                    }
                    e => return Err(error(e)),
                }
            }
        }
        Ok(total)
    }

    /// Wait until `urbs` (submitted) are all reaped. After `timeout` they
    /// are cancelled, then reaped too.
    fn reap(&mut self, urbs: &mut [Urb], timeout: Duration) -> Result<()> {
        let range = urbs.as_ptr_range();
        let mut left = urbs.len();
        let mut deadline = Instant::now() + timeout;
        let mut timed_out = false;
        while left > 0 {
            let mut done: *mut Urb = std::ptr::null_mut();
            match self.ioctl(USBDEVFS_REAPURBNDELAY, &mut done as *mut *mut Urb as *mut c_void) {
                Ok(_) if range.contains(&(done as *const Urb)) => {
                    left -= 1;
                    continue;
                }
                Ok(_) => {
                    log::error!("reaped a transfer that is not ours");
                    continue;
                }
                Err(Error::Os(libc::EAGAIN)) => {}
                Err(e) => {
                    // the device is gone (nothing left to reap), or worse
                    self.broken = true;
                    return Err(e);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                if timed_out {
                    log::error!("{left} cancelled transfers did not come back; transfers stop");
                    self.broken = true;
                    return Err(Error::Timeout);
                }
                for u in urbs.iter_mut() {
                    // EINVAL for the ones that completed already
                    let _ = self.ioctl(USBDEVFS_DISCARDURB, u as *mut Urb as *mut c_void);
                }
                timed_out = true;
                deadline = now + DISCARD_WAIT;
                continue;
            }
            let mut p = libc::pollfd {
                fd: self.fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            let ms = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
            // SAFETY: one valid pollfd; usbdevfs reports completed transfers as writable
            unsafe { libc::poll(&mut p, 1, ms) };
        }
        if timed_out {
            return Err(Error::Timeout);
        }
        Ok(())
    }

    fn control(&mut self, request_type: u8, request: u8, data: *mut c_void, len: usize) -> Result<usize> {
        let mut t = CtrlTransfer {
            request_type,
            request,
            value: 0,
            index: self.interface as u16,
            length: len as u16,
            timeout: millis(CONTROL_TIMEOUT),
            data,
        };
        self.ioctl(USBDEVFS_CONTROL, &mut t as *mut CtrlTransfer as *mut c_void)
    }
}

fn millis(d: Duration) -> u32 {
    d.as_millis().min(u32::MAX as u128) as u32
}

fn error(errno: i32) -> Error {
    match errno {
        libc::EPIPE => Error::Stall,
        libc::ETIMEDOUT => Error::Timeout,
        e => Error::Os(e),
    }
}

impl Transport for UsbFs {
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<usize> {
        // the kernel only reads from an OUT buffer
        self.bulk(self.ep_out, data.as_ptr() as *mut c_void, data.len(), timeout)
    }

    fn bulk_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.bulk(self.ep_in, buf.as_mut_ptr() as *mut c_void, buf.len(), timeout)
    }

    fn clear_halt(&mut self, inbound: bool) -> Result<()> {
        let mut ep = if inbound { self.ep_in } else { self.ep_out } as u32;
        self.ioctl(USBDEVFS_CLEAR_HALT, &mut ep as *mut u32 as *mut c_void)?;
        Ok(())
    }

    fn class_out(&mut self, request: u8) -> Result<()> {
        self.control(CLASS_INTERFACE_OUT, request, std::ptr::null_mut(), 0)?;
        Ok(())
    }

    fn class_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize> {
        self.control(CLASS_INTERFACE_IN, request, buf.as_mut_ptr() as *mut c_void, buf.len())
    }

    fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    fn max_queued(&self) -> usize {
        self.max_transfer * QUEUE
    }

    /// The kernel allocates a buffer of this size for every transfer, so
    /// very large values may fail with ENOMEM.
    fn set_max_transfer(&mut self, bytes: usize) {
        self.max_transfer = bytes.max(512);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // values of the 64-bit kernel ABI (pointers of 8 bytes)
        assert_eq!(USBDEVFS_CONTROL, 0xC018_5500);
        assert_eq!(USBDEVFS_BULK, 0xC018_5502);
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550A);
        assert_eq!(USBDEVFS_DISCARDURB, 0x0000_550B);
        assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008_550D);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
        assert_eq!(size_of::<Urb>(), 56);
    }
}
