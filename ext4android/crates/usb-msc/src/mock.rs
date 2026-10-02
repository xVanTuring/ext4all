//! Test transports: a scripted one for the Bulk-Only layer and a simulated
//! SCSI disk for [`crate::Disk`].

use crate::bot::{CBW_LEN, CSW_LEN};
use crate::scsi::*;
use crate::{Error, Result, Transport};
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Out(usize),
    In(usize),
    ClearHalt(bool),
    Reset,
}

enum Item {
    Bytes(Vec<u8>),
    Stall,
}

/// Bulk IN data comes from a queue: each read takes up to the rest of the
/// current item, so an item shorter than the read is a short packet.
pub struct MockTransport {
    max: usize,
    queue: VecDeque<Item>,
    out_error: Option<Error>,
    class_in_stall: bool,
    /// Bulk transfers longer than this fail with ENOMEM, like usbdevfs
    /// when the kernel cannot allocate their buffer.
    pub enomem_above: Option<usize>,
    pub events: Vec<Event>,
    pub written: Vec<u8>,
}

fn no_memory(limit: Option<usize>, len: usize) -> Result<()> {
    match limit {
        Some(l) if len > l => Err(Error::Os(12)),
        _ => Ok(()),
    }
}

impl MockTransport {
    pub fn new(max_transfer: usize) -> MockTransport {
        MockTransport {
            max: max_transfer,
            queue: VecDeque::new(),
            out_error: None,
            class_in_stall: false,
            enomem_above: None,
            events: Vec::new(),
            written: Vec::new(),
        }
    }

    pub fn push_in(&mut self, data: Vec<u8>) {
        self.queue.push_back(Item::Bytes(data));
    }

    pub fn push_csw(&mut self, csw: Vec<u8>) {
        assert_eq!(csw.len(), CSW_LEN);
        self.push_in(csw);
    }

    pub fn stall_next_in(&mut self) {
        self.queue.push_back(Item::Stall);
    }

    pub fn fail_out(&mut self, e: Error) {
        self.out_error = Some(e);
    }

    pub fn stall_class_in(&mut self) {
        self.class_in_stall = true;
    }
}

impl Transport for MockTransport {
    fn bulk_out(&mut self, data: &[u8], _timeout: Duration) -> Result<usize> {
        if let Some(e) = self.out_error.take() {
            return Err(e);
        }
        no_memory(self.enomem_above, data.len())?;
        self.events.push(Event::Out(data.len()));
        self.written.extend_from_slice(data);
        Ok(data.len())
    }

    fn bulk_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        no_memory(self.enomem_above, buf.len())?;
        match self.queue.front_mut() {
            None => Err(Error::Timeout),
            Some(Item::Stall) => {
                self.queue.pop_front();
                Err(Error::Stall)
            }
            Some(Item::Bytes(b)) => {
                let n = buf.len().min(b.len());
                buf[..n].copy_from_slice(&b[..n]);
                b.drain(..n);
                if b.is_empty() {
                    self.queue.pop_front();
                }
                self.events.push(Event::In(n));
                Ok(n)
            }
        }
    }

    fn clear_halt(&mut self, inbound: bool) -> Result<()> {
        self.events.push(Event::ClearHalt(inbound));
        Ok(())
    }

    fn class_out(&mut self, _request: u8) -> Result<()> {
        self.events.push(Event::Reset);
        Ok(())
    }

    fn class_in(&mut self, _request: u8, buf: &mut [u8]) -> Result<usize> {
        if self.class_in_stall {
            return Err(Error::Stall);
        }
        buf[0] = 0;
        Ok(1)
    }

    fn max_transfer(&self) -> usize {
        self.max
    }

    fn set_max_transfer(&mut self, bytes: usize) {
        self.max = bytes;
    }
}

enum Phase {
    Command,
    DataIn(Vec<u8>),
    DataOut { lba: u64, len: usize, got: Vec<u8> },
    Status,
}

/// A SCSI disk behind Bulk-Only Transport, kept in memory.
pub struct SimDisk {
    pub data: Vec<u8>,
    pub block_size: u32,
    phase: Phase,
    tag: u32,
    residue: u32,
    status: u8,
    sense: Option<Sense>,
    /// Unit attentions reported before the unit becomes ready.
    pub pending_attention: u32,
    /// TEST UNIT READY answers "becoming ready" this many times.
    pub not_ready: u32,
    pub sync_supported: bool,
    pub syncs: u32,
    /// Fail the next READ or WRITE with a unit attention.
    pub attention_on_io: bool,
    /// Capacity to report in place of the size of `data`.
    pub capacity_override: Option<u64>,
    /// Bulk transfers longer than this fail with ENOMEM.
    pub enomem_above: Option<usize>,
    max_transfer: usize,
    pub commands: Vec<u8>,
}

impl SimDisk {
    pub fn new(blocks: u64, block_size: u32) -> SimDisk {
        SimDisk {
            data: vec![0; (blocks * block_size as u64) as usize],
            block_size,
            phase: Phase::Command,
            tag: 0,
            residue: 0,
            status: 0,
            sense: None,
            pending_attention: 0,
            not_ready: 0,
            sync_supported: true,
            syncs: 0,
            attention_on_io: false,
            capacity_override: None,
            enomem_above: None,
            max_transfer: 16 * 1024,
            commands: Vec::new(),
        }
    }

    fn check(&mut self, key: u8, asc: u8, ascq: u8) {
        self.status = 1;
        self.sense = Some(Sense { key, asc, ascq });
    }

    fn execute(&mut self, cdb: &[u8], data_len: usize) {
        self.status = 0;
        self.residue = 0;
        self.commands.push(cdb[0]);
        let bs = self.block_size as u64;
        let stored = self.data.len() as u64 / bs;
        let blocks = self.capacity_override.unwrap_or(stored);
        let mut reply: Option<Vec<u8>> = None;
        match cdb[0] {
            REQUEST_SENSE => {
                let s = self.sense.take().unwrap_or(Sense { key: 0, asc: 0, ascq: 0 });
                let mut b = vec![0u8; SENSE_LEN];
                b[0] = 0x70;
                b[2] = s.key;
                b[7] = 10;
                b[12] = s.asc;
                b[13] = s.ascq;
                reply = Some(b);
            }
            _ if self.pending_attention > 0 && cdb[0] != INQUIRY => {
                self.pending_attention -= 1;
                self.check(Sense::UNIT_ATTENTION, 0x29, 0);
            }
            TEST_UNIT_READY => {
                if self.not_ready > 0 {
                    self.not_ready -= 1;
                    self.check(Sense::NOT_READY, 0x04, 0x01);
                }
            }
            INQUIRY => {
                let mut b = vec![0u8; INQUIRY_LEN];
                b[1] = 0x80;
                b[8..16].copy_from_slice(b"SimDisk ");
                b[16..32].copy_from_slice(b"Bulk Only Disk  ");
                b[32..36].copy_from_slice(b"1.00");
                reply = Some(b);
            }
            READ_CAPACITY_10 => {
                let last = (blocks - 1).min(u32::MAX as u64) as u32;
                let mut b = last.to_be_bytes().to_vec();
                b.extend_from_slice(&(bs as u32).to_be_bytes());
                reply = Some(b);
            }
            SERVICE_ACTION_IN_16 if cdb[1] == SA_READ_CAPACITY_16 => {
                let mut b = vec![0u8; CAPACITY_16_LEN];
                b[0..8].copy_from_slice(&(blocks - 1).to_be_bytes());
                b[8..12].copy_from_slice(&(bs as u32).to_be_bytes());
                reply = Some(b);
            }
            READ_10 | READ_16 | WRITE_10 | WRITE_16 => {
                let (lba, n) = if cdb[0] == READ_10 || cdb[0] == WRITE_10 {
                    (
                        u32::from_be_bytes(cdb[2..6].try_into().unwrap()) as u64,
                        u16::from_be_bytes(cdb[7..9].try_into().unwrap()) as u64,
                    )
                } else {
                    (
                        u64::from_be_bytes(cdb[2..10].try_into().unwrap()),
                        u32::from_be_bytes(cdb[10..14].try_into().unwrap()) as u64,
                    )
                };
                if self.attention_on_io {
                    self.attention_on_io = false;
                    self.check(Sense::UNIT_ATTENTION, 0x28, 0);
                } else if lba + n > stored || (n * bs) as usize != data_len {
                    self.check(Sense::ILLEGAL_REQUEST, 0x21, 0);
                } else if cdb[0] == READ_10 || cdb[0] == READ_16 {
                    let o = (lba * bs) as usize;
                    reply = Some(self.data[o..o + data_len].to_vec());
                } else {
                    self.phase = Phase::DataOut {
                        lba,
                        len: data_len,
                        got: Vec::new(),
                    };
                    return;
                }
            }
            SYNCHRONIZE_CACHE_10 => {
                if self.sync_supported {
                    self.syncs += 1;
                } else {
                    self.check(Sense::ILLEGAL_REQUEST, 0x20, 0);
                }
            }
            _ => self.check(Sense::ILLEGAL_REQUEST, 0x20, 0),
        }
        match reply {
            Some(mut b) => {
                b.truncate(data_len);
                self.residue = (data_len - b.len()) as u32;
                self.phase = if b.is_empty() { Phase::Status } else { Phase::DataIn(b) };
            }
            None => {
                // a failed or data-less command: no data phase
                self.residue = data_len as u32;
                self.phase = Phase::Status;
            }
        }
    }

    fn csw(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(CSW_LEN);
        b.extend_from_slice(b"USBS");
        b.extend_from_slice(&self.tag.to_le_bytes());
        b.extend_from_slice(&self.residue.to_le_bytes());
        b.push(self.status);
        b
    }
}

impl Transport for SimDisk {
    fn bulk_out(&mut self, data: &[u8], _timeout: Duration) -> Result<usize> {
        no_memory(self.enomem_above, data.len())?;
        match &mut self.phase {
            Phase::Command => {
                assert_eq!(data.len(), CBW_LEN, "expected a CBW");
                assert_eq!(&data[0..4], b"USBC");
                self.tag = u32::from_le_bytes(data[4..8].try_into().unwrap());
                let len = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
                let cdb = data[15..15 + data[14] as usize].to_vec();
                self.execute(&cdb, len);
                Ok(data.len())
            }
            Phase::DataOut { lba, len, got } => {
                got.extend_from_slice(data);
                if got.len() >= *len {
                    let o = (*lba * self.block_size as u64) as usize;
                    let got = std::mem::take(got);
                    self.data[o..o + got.len()].copy_from_slice(&got);
                    self.phase = Phase::Status;
                }
                Ok(data.len())
            }
            _ => Err(Error::Stall),
        }
    }

    fn bulk_in(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        no_memory(self.enomem_above, buf.len())?;
        match &mut self.phase {
            Phase::DataIn(b) => {
                let n = buf.len().min(b.len());
                buf[..n].copy_from_slice(&b[..n]);
                b.drain(..n);
                if b.is_empty() {
                    self.phase = Phase::Status;
                }
                Ok(n)
            }
            Phase::Status => {
                let c = self.csw();
                buf[..CSW_LEN].copy_from_slice(&c);
                self.phase = Phase::Command;
                Ok(CSW_LEN)
            }
            _ => Err(Error::Stall),
        }
    }

    fn clear_halt(&mut self, _inbound: bool) -> Result<()> {
        Ok(())
    }

    fn class_out(&mut self, _request: u8) -> Result<()> {
        self.phase = Phase::Command;
        Ok(())
    }

    fn class_in(&mut self, _request: u8, _buf: &mut [u8]) -> Result<usize> {
        Err(Error::Stall)
    }

    fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    fn set_max_transfer(&mut self, bytes: usize) {
        self.max_transfer = bytes;
    }
}
