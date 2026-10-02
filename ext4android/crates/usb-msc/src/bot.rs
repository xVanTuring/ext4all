//! Bulk-Only Transport (USB Mass Storage Class, BBB): every command is a
//! 31-byte command block wrapper (CBW) on bulk OUT, an optional data phase,
//! and a 13-byte command status wrapper (CSW) on bulk IN.
//!
//! Errors follow the specification's recovery: a stalled data phase clears
//! the halt and still reads the status; a broken exchange (bad CSW, phase
//! error, failed transfer) runs the reset recovery so the next command
//! starts clean.

use crate::{Error, Result, Transport};
use std::time::Duration;

pub const CBW_LEN: usize = 31;
pub const CSW_LEN: usize = 13;
const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC"
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS"
const FLAG_IN: u8 = 0x80;

/// Class requests (bRequest).
const BULK_ONLY_RESET: u8 = 0xFF;
const GET_MAX_LUN: u8 = 0xFE;

const STATUS_PASSED: u8 = 0;
const STATUS_FAILED: u8 = 1;

/// Timeout of the command and status transfers.
const WRAPPER_TIMEOUT: Duration = Duration::from_secs(10);

pub fn encode_cbw(tag: u32, data_len: u32, inbound: bool, lun: u8, cdb: &[u8]) -> [u8; CBW_LEN] {
    assert!((1..=16).contains(&cdb.len()), "CDB length {}", cdb.len());
    let mut b = [0u8; CBW_LEN];
    b[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&data_len.to_le_bytes());
    b[12] = if inbound { FLAG_IN } else { 0 };
    b[13] = lun & 0x0F;
    b[14] = cdb.len() as u8;
    b[15..15 + cdb.len()].copy_from_slice(cdb);
    b
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Csw {
    pub tag: u32,
    pub residue: u32,
    pub status: u8,
}

/// A CSW, if `b` is exactly one.
pub fn parse_csw(b: &[u8]) -> Option<Csw> {
    if b.len() != CSW_LEN || u32::from_le_bytes(b[0..4].try_into().unwrap()) != CSW_SIGNATURE {
        return None;
    }
    Some(Csw {
        tag: u32::from_le_bytes(b[4..8].try_into().unwrap()),
        residue: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        status: b[12],
    })
}

/// Data phase of a command.
pub enum Data<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

/// Result of a command whose exchange completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Bytes moved in the data phase.
    pub transferred: usize,
    /// The device reported success; otherwise REQUEST SENSE says why.
    pub passed: bool,
}

pub struct Bot<T: Transport> {
    t: T,
    tag: u32,
    lun: u8,
}

impl<T: Transport> Bot<T> {
    pub fn new(t: T) -> Bot<T> {
        Bot { t, tag: 0, lun: 0 }
    }

    pub fn transport(&self) -> &T {
        &self.t
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.t
    }

    pub fn set_lun(&mut self, lun: u8) {
        self.lun = lun;
    }

    /// Highest LUN number. Single-LUN devices may stall the request.
    pub fn max_lun(&mut self) -> u8 {
        let mut b = [0u8; 1];
        match self.t.class_in(GET_MAX_LUN, &mut b) {
            Ok(1) => b[0] & 0x0F,
            Ok(_) => 0,
            Err(Error::Stall) => {
                let _ = self.t.clear_halt(true);
                0
            }
            Err(_) => 0,
        }
    }

    /// Bulk-Only Mass Storage Reset, then clear both halts.
    pub fn reset_recovery(&mut self) -> Result<()> {
        log::warn!("bulk-only reset recovery");
        self.t.class_out(BULK_ONLY_RESET)?;
        self.t.clear_halt(true)?;
        self.t.clear_halt(false)
    }

    fn fail<V>(&mut self, e: Error) -> Result<V> {
        if !e.is_disconnected()
            && let Err(r) = self.reset_recovery()
        {
            log::error!("reset recovery failed: {r}");
        }
        Err(e)
    }

    /// Run one command. Errors mean the exchange itself failed (and was
    /// recovered); a command the device rejected is `passed == false`.
    pub fn command(&mut self, cdb: &[u8], data: Data<'_>, timeout: Duration) -> Result<Outcome> {
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let (len, inbound) = match &data {
            Data::None => (0, false),
            Data::In(b) => (b.len(), true),
            Data::Out(b) => (b.len(), false),
        };
        let cbw = encode_cbw(tag, len as u32, inbound, self.lun, cdb);
        match self.t.bulk_out(&cbw, WRAPPER_TIMEOUT) {
            Ok(CBW_LEN) => {}
            Ok(n) => return self.fail(Error::Protocol(format!("sent {n} of {CBW_LEN} CBW bytes"))),
            Err(e) => return self.fail(e),
        }

        let max = self.t.max_transfer().max(512);
        let mut transferred = 0;
        let mut early = None;
        match data {
            Data::None => {}
            Data::In(buf) => {
                while transferred < buf.len() {
                    let want = (buf.len() - transferred).min(max);
                    match self.t.bulk_in(&mut buf[transferred..transferred + want], timeout) {
                        Ok(n) => {
                            // some devices skip a data phase they cannot
                            // serve and send the CSW at once
                            if transferred == 0
                                && n == CSW_LEN
                                && want > CSW_LEN
                                && let Some(c) = parse_csw(&buf[..CSW_LEN]).filter(|c| c.tag == tag)
                            {
                                early = Some(c);
                                break;
                            }
                            transferred += n;
                            if n < want {
                                break; // short packet: the device ended the data phase
                            }
                        }
                        Err(Error::Stall) => {
                            if let Err(e) = self.t.clear_halt(true) {
                                return self.fail(e);
                            }
                            break;
                        }
                        Err(e) => return self.fail(e),
                    }
                }
            }
            Data::Out(buf) => {
                while transferred < buf.len() {
                    let want = (buf.len() - transferred).min(max);
                    match self.t.bulk_out(&buf[transferred..transferred + want], timeout) {
                        Ok(n) => {
                            transferred += n;
                            if n < want {
                                break;
                            }
                        }
                        Err(Error::Stall) => {
                            if let Err(e) = self.t.clear_halt(false) {
                                return self.fail(e);
                            }
                            break;
                        }
                        Err(e) => return self.fail(e),
                    }
                }
            }
        }

        let csw = match early {
            Some(c) => c,
            None => self.read_csw(tag)?,
        };
        match csw.status {
            STATUS_PASSED => Ok(Outcome {
                transferred,
                passed: true,
            }),
            STATUS_FAILED => Ok(Outcome {
                transferred,
                passed: false,
            }),
            s => self.fail(Error::Protocol(format!("CSW status {s} (phase error)"))),
        }
    }

    fn read_csw(&mut self, tag: u32) -> Result<Csw> {
        let mut b = [0u8; CSW_LEN];
        // one retry after a stall or a zero-length packet
        for attempt in 0..2 {
            match self.t.bulk_in(&mut b, WRAPPER_TIMEOUT) {
                Ok(CSW_LEN) => {
                    return match parse_csw(&b) {
                        Some(c) if c.tag == tag => Ok(c),
                        Some(c) => self.fail(Error::Protocol(format!("CSW tag {} for command {tag}", c.tag))),
                        None => self.fail(Error::Protocol("invalid CSW signature".into())),
                    };
                }
                Ok(0) if attempt == 0 => continue,
                Ok(n) => return self.fail(Error::Protocol(format!("CSW of {n} bytes"))),
                Err(Error::Stall) if attempt == 0 => {
                    if let Err(e) = self.t.clear_halt(true) {
                        return self.fail(e);
                    }
                }
                Err(e) => return self.fail(e),
            }
        }
        self.fail(Error::Protocol("no CSW".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{Event, MockTransport};

    fn csw(tag: u32, residue: u32, status: u8) -> Vec<u8> {
        let mut b = Vec::with_capacity(CSW_LEN);
        b.extend_from_slice(&CSW_SIGNATURE.to_le_bytes());
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&residue.to_le_bytes());
        b.push(status);
        b
    }

    #[test]
    fn cbw_layout() {
        let b = encode_cbw(7, 512, true, 0, &[0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0]);
        assert_eq!(&b[0..4], b"USBC");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 512);
        assert_eq!((b[12], b[13], b[14]), (0x80, 0, 10));
        assert_eq!(b[15], 0x28);
        assert!(b[25..].iter().all(|&x| x == 0));
    }

    #[test]
    fn read_with_data_split_into_transfers() {
        let mut m = MockTransport::new(4096);
        m.push_in((0..10_000u32).map(|i| i as u8).collect());
        m.push_csw(csw(1, 0, 0));
        let mut bot = Bot::new(m);
        let mut buf = vec![0u8; 10_000];
        let o = bot.command(&[0x28; 10], Data::In(&mut buf), WRAPPER_TIMEOUT).unwrap();
        assert_eq!(o, Outcome { transferred: 10_000, passed: true });
        assert_eq!(buf[9_999], (9_999u32) as u8);
        // three data transfers of at most 4096 bytes, then the CSW
        let ins: Vec<usize> = bot.t.events.iter().filter_map(|e| match e {
            Event::In(n) => Some(*n),
            _ => None,
        }).collect();
        assert_eq!(ins, vec![4096, 4096, 1808, CSW_LEN]);
    }

    #[test]
    fn short_data_phase_then_failed_status() {
        let mut m = MockTransport::new(65536);
        m.push_in(vec![1; 100]);
        m.push_csw(csw(1, 412, 1));
        let mut bot = Bot::new(m);
        let mut buf = vec![0u8; 512];
        let o = bot.command(&[0x28; 10], Data::In(&mut buf), WRAPPER_TIMEOUT).unwrap();
        assert_eq!(o, Outcome { transferred: 100, passed: false });
    }

    #[test]
    fn stalled_data_phase_clears_halt_and_reads_status() {
        let mut m = MockTransport::new(65536);
        m.stall_next_in();
        m.push_csw(csw(1, 512, 1));
        let mut bot = Bot::new(m);
        let mut buf = vec![0u8; 512];
        let o = bot.command(&[0x28; 10], Data::In(&mut buf), WRAPPER_TIMEOUT).unwrap();
        assert!(!o.passed);
        assert!(bot.t.events.contains(&Event::ClearHalt(true)));
        assert!(!bot.t.events.contains(&Event::Reset));
    }

    #[test]
    fn csw_sent_in_place_of_data() {
        let mut m = MockTransport::new(65536);
        m.push_in(csw(1, 512, 1));
        let mut bot = Bot::new(m);
        let mut buf = vec![0u8; 512];
        let o = bot.command(&[0x28; 10], Data::In(&mut buf), WRAPPER_TIMEOUT).unwrap();
        assert_eq!(o, Outcome { transferred: 0, passed: false });
    }

    #[test]
    fn wrong_tag_runs_reset_recovery() {
        let mut m = MockTransport::new(65536);
        m.push_csw(csw(99, 0, 0));
        let mut bot = Bot::new(m);
        let e = bot.command(&[0; 6], Data::None, WRAPPER_TIMEOUT).unwrap_err();
        assert!(matches!(e, Error::Protocol(_)), "{e}");
        let tail: Vec<_> = bot.t.events.iter().rev().take(3).cloned().collect();
        assert_eq!(tail, vec![Event::ClearHalt(false), Event::ClearHalt(true), Event::Reset]);
    }

    #[test]
    fn phase_error_runs_reset_recovery() {
        let mut m = MockTransport::new(65536);
        m.push_csw(csw(1, 0, 2));
        let mut bot = Bot::new(m);
        assert!(bot.command(&[0; 6], Data::None, WRAPPER_TIMEOUT).is_err());
        assert!(bot.t.events.contains(&Event::Reset));
    }

    #[test]
    fn write_sends_data() {
        let mut m = MockTransport::new(1000);
        m.push_csw(csw(1, 0, 0));
        let mut bot = Bot::new(m);
        let data = vec![7u8; 2500];
        let o = bot.command(&[0x2A; 10], Data::Out(&data), WRAPPER_TIMEOUT).unwrap();
        assert_eq!(o, Outcome { transferred: 2500, passed: true });
        assert_eq!(bot.t.written.len(), CBW_LEN + 2500);
    }

    #[test]
    fn unplugged_device_skips_recovery() {
        let mut m = MockTransport::new(65536);
        m.fail_out(Error::Os(19));
        let mut bot = Bot::new(m);
        let e = bot.command(&[0; 6], Data::None, WRAPPER_TIMEOUT).unwrap_err();
        assert!(e.is_disconnected());
        assert!(!bot.t.events.contains(&Event::Reset));
    }

    #[test]
    fn max_lun_stall_means_single_lun() {
        let mut m = MockTransport::new(65536);
        m.stall_class_in();
        let mut bot = Bot::new(m);
        assert_eq!(bot.max_lun(), 0);
    }
}
