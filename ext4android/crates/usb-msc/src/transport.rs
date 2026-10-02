use crate::Result;
use std::time::Duration;

/// Transfers on the two bulk endpoints and the default control pipe of one
/// mass storage interface.
pub trait Transport: Send {
    /// Send `data` on the bulk OUT endpoint; returns the bytes sent.
    fn bulk_out(&mut self, data: &[u8], timeout: Duration) -> Result<usize>;

    /// Receive into `buf` from the bulk IN endpoint; returns the bytes
    /// received, fewer when the device ends the transfer with a short packet.
    fn bulk_in(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;

    /// Clear a halt (stall) of the bulk IN (`inbound`) or OUT endpoint.
    fn clear_halt(&mut self, inbound: bool) -> Result<()>;

    /// Class request to the interface without data (host to device).
    fn class_out(&mut self, request: u8) -> Result<()>;

    /// Class request to the interface reading into `buf`; returns the bytes
    /// received.
    fn class_in(&mut self, request: u8, buf: &mut [u8]) -> Result<usize>;

    /// Largest single bulk transfer; bigger data phases are split.
    fn max_transfer(&self) -> usize {
        64 * 1024
    }

    /// Change the largest single bulk transfer, where supported.
    fn set_max_transfer(&mut self, _bytes: usize) {}
}
