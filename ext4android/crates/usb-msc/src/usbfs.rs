//! [`Transport`] over Linux usbdevfs ioctls on the file descriptor of an
//! opened USB device. On Android that is
//! `UsbDeviceConnection.getFileDescriptor()`, after `claimInterface`: the
//! same way libusb works there without root.

use crate::{Error, Result, Transport};
use std::ffi::c_void;
use std::os::fd::RawFd;
use std::time::Duration;

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

const USBDEVFS_CONTROL: u32 = ioc(IOC_READ | IOC_WRITE, 0, size_of::<CtrlTransfer>());
const USBDEVFS_BULK: u32 = ioc(IOC_READ | IOC_WRITE, 2, size_of::<BulkTransfer>());
const USBDEVFS_CLEAR_HALT: u32 = ioc(IOC_READ, 21, size_of::<u32>());

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
        }
    }

    fn ioctl(&self, request: u32, arg: *mut c_void) -> Result<usize> {
        // SAFETY: `arg` points to the structure `request` expects, valid
        // for the duration of the call, and its data pointer covers `len`
        let r = unsafe { libc::ioctl(self.fd, request as _, arg) };
        if r >= 0 {
            return Ok(r as usize);
        }
        Err(match std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO) {
            libc::EPIPE => Error::Stall,
            libc::ETIMEDOUT => Error::Timeout,
            e => Error::Os(e),
        })
    }

    fn bulk(&mut self, ep: u8, data: *mut c_void, len: usize, timeout: Duration) -> Result<usize> {
        let mut t = BulkTransfer {
            ep: ep as u32,
            len: len as u32,
            timeout: millis(timeout),
            data,
        };
        self.ioctl(USBDEVFS_BULK, &mut t as *mut BulkTransfer as *mut c_void)
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
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
    }
}
