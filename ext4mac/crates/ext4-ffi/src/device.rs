//! [`BlockDevice`] backed by Swift callbacks.

use crate::types::Ext4DeviceOps;
use ext4_core::{BlockDevice, Error, Result};

pub struct CallbackDevice {
    ops: Ext4DeviceOps,
}

// SAFETY: the Swift side guarantees its callbacks may be called from any
// thread (FSBlockDeviceResource is thread safe) and serializes nothing.
unsafe impl Send for CallbackDevice {}
unsafe impl Sync for CallbackDevice {}

impl CallbackDevice {
    /// # Safety
    /// `ops` callbacks must stay valid until `release` is called.
    pub unsafe fn new(ops: Ext4DeviceOps) -> Result<Self> {
        if ops.read.is_none() {
            return Err(Error::invalid("device has no read callback"));
        }
        if !ops.read_only && ops.write.is_none() {
            return Err(Error::invalid("writable device has no write callback"));
        }
        Ok(CallbackDevice { ops })
    }
}

fn check(rc: i32) -> Result<()> {
    if rc == 0 { Ok(()) } else { Err(Error::Device(rc)) }
}

impl BlockDevice for CallbackDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|e| e > self.ops.size) {
            return Err(Error::invalid("read beyond end of device"));
        }
        let f = self.ops.read.expect("checked in new");
        // SAFETY: buf is valid for writes of buf.len() bytes
        check(unsafe { f(self.ops.ctx, offset, buf.as_mut_ptr(), buf.len()) })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.ops.read_only {
            return Err(Error::ReadOnly);
        }
        if offset.checked_add(buf.len() as u64).is_none_or(|e| e > self.ops.size) {
            return Err(Error::invalid("write beyond end of device"));
        }
        let f = self.ops.write.ok_or(Error::ReadOnly)?;
        // SAFETY: buf is valid for reads of buf.len() bytes
        check(unsafe { f(self.ops.ctx, offset, buf.as_ptr(), buf.len()) })
    }

    fn flush(&self) -> Result<()> {
        match self.ops.flush {
            // SAFETY: callback contract
            Some(f) => check(unsafe { f(self.ops.ctx) }),
            None => Ok(()),
        }
    }

    fn size(&self) -> u64 {
        self.ops.size
    }

    fn is_read_only(&self) -> bool {
        self.ops.read_only
    }

    fn sector_size(&self) -> u32 {
        if self.ops.sector_size == 0 {
            512
        } else {
            self.ops.sector_size
        }
    }
}

impl Drop for CallbackDevice {
    fn drop(&mut self) {
        if let Some(r) = self.ops.release {
            // SAFETY: called exactly once, when the device is dropped
            unsafe { r(self.ops.ctx) }
        }
    }
}
