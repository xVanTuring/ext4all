use crate::scsi::Sense;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A transfer failed with this errno (`ENODEV` once the device is
    /// unplugged).
    #[error("USB transfer failed (errno {0})")]
    Os(i32),
    #[error("endpoint stalled")]
    Stall,
    #[error("USB transfer timed out")]
    Timeout,
    /// The device broke the Bulk-Only protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The command failed; the sense data says why.
    #[error("command failed: {0}")]
    Check(Sense),
    #[error("invalid response: {0}")]
    Invalid(String),
}

impl Error {
    /// Whether the device is gone (unplugged or disconnected).
    pub fn is_disconnected(&self) -> bool {
        // ENODEV, ESHUTDOWN
        matches!(self, Error::Os(19) | Error::Os(108))
    }

    /// ENOMEM: the kernel could not allocate the buffer of a transfer
    /// (usbdevfs needs one contiguous buffer per synchronous transfer);
    /// nothing was sent or received.
    pub fn is_out_of_memory(&self) -> bool {
        matches!(self, Error::Os(12))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
