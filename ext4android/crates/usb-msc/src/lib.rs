//! USB mass storage devices: USB sticks, card readers and SATA/NVMe
//! enclosures, driven with Bulk-Only Transport and SCSI commands.
//!
//! - [`Transport`]: bulk and class control transfers on one interface.
//!   `usbfs::UsbFs` implements it with Linux usbdevfs ioctls on the file
//!   descriptor that Android's `UsbDeviceConnection` hands out.
//! - [`bot`]: command and status wrappers, error recovery.
//! - [`scsi`]: the SCSI commands a disk needs and their responses.
//! - [`Disk`]: a ready unit with its capacity, block reads and writes and
//!   cache flushes.

pub mod bot;
mod disk;
mod error;
pub mod scsi;
mod transport;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod usbfs;

/// Test transports, also for the tests of crates using this one (feature
/// `sim`).
#[cfg(any(test, feature = "sim"))]
pub mod mock;

pub use disk::{Disk, DiskInfo};
pub use error::{Error, Result};
pub use scsi::{Inquiry, Sense};
pub use transport::Transport;
