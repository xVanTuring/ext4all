//! The SCSI commands a disk needs (SPC, SBC) as command descriptor blocks,
//! and parsing of their responses. Multi-byte fields are big-endian.

use std::fmt;

pub const TEST_UNIT_READY: u8 = 0x00;
pub const REQUEST_SENSE: u8 = 0x03;
pub const INQUIRY: u8 = 0x12;
pub const READ_CAPACITY_10: u8 = 0x25;
pub const READ_10: u8 = 0x28;
pub const WRITE_10: u8 = 0x2A;
pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
pub const READ_16: u8 = 0x88;
pub const WRITE_16: u8 = 0x8A;
pub const SERVICE_ACTION_IN_16: u8 = 0x9E;
pub const SA_READ_CAPACITY_16: u8 = 0x10;

pub const INQUIRY_LEN: usize = 36;
pub const SENSE_LEN: usize = 18;
pub const CAPACITY_16_LEN: usize = 32;

/// A command descriptor block of 6 to 16 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cdb {
    bytes: [u8; 16],
    len: usize,
}

impl Cdb {
    fn new(len: usize, opcode: u8) -> Cdb {
        let mut bytes = [0; 16];
        bytes[0] = opcode;
        Cdb { bytes, len }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn opcode(&self) -> u8 {
        self.bytes[0]
    }
}

pub fn test_unit_ready() -> Cdb {
    Cdb::new(6, TEST_UNIT_READY)
}

pub fn request_sense() -> Cdb {
    let mut c = Cdb::new(6, REQUEST_SENSE);
    c.bytes[4] = SENSE_LEN as u8;
    c
}

pub fn inquiry() -> Cdb {
    let mut c = Cdb::new(6, INQUIRY);
    c.bytes[3..5].copy_from_slice(&(INQUIRY_LEN as u16).to_be_bytes());
    c
}

pub fn read_capacity_10() -> Cdb {
    Cdb::new(10, READ_CAPACITY_10)
}

pub fn read_capacity_16() -> Cdb {
    let mut c = Cdb::new(16, SERVICE_ACTION_IN_16);
    c.bytes[1] = SA_READ_CAPACITY_16;
    c.bytes[10..14].copy_from_slice(&(CAPACITY_16_LEN as u32).to_be_bytes());
    c
}

/// READ or WRITE of `blocks` blocks at `lba`: the 10-byte form when it
/// fits, else the 16-byte form (disks over 2 TiB).
pub fn read_write(write: bool, lba: u64, blocks: u32) -> Cdb {
    if lba + blocks as u64 <= u32::MAX as u64 + 1 && blocks <= u16::MAX as u32 {
        let mut c = Cdb::new(10, if write { WRITE_10 } else { READ_10 });
        c.bytes[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        c.bytes[7..9].copy_from_slice(&(blocks as u16).to_be_bytes());
        c
    } else {
        let mut c = Cdb::new(16, if write { WRITE_16 } else { READ_16 });
        c.bytes[2..10].copy_from_slice(&lba.to_be_bytes());
        c.bytes[10..14].copy_from_slice(&blocks.to_be_bytes());
        c
    }
}

/// Flush the whole volatile cache.
pub fn synchronize_cache() -> Cdb {
    Cdb::new(10, SYNCHRONIZE_CACHE_10)
}

/// Standard INQUIRY data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type (0: direct access block device).
    pub device_type: u8,
    pub removable: bool,
    pub vendor: String,
    pub product: String,
    pub revision: String,
}

fn ascii(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

pub fn parse_inquiry(b: &[u8]) -> Option<Inquiry> {
    if b.len() < INQUIRY_LEN {
        return None;
    }
    Some(Inquiry {
        device_type: b[0] & 0x1F,
        removable: b[1] & 0x80 != 0,
        vendor: ascii(&b[8..16]),
        product: ascii(&b[16..32]),
        revision: ascii(&b[32..36]),
    })
}

/// Last LBA and block length from READ CAPACITY (10).
pub fn parse_capacity_10(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < 8 {
        return None;
    }
    let last = u32::from_be_bytes(b[0..4].try_into().unwrap());
    let len = u32::from_be_bytes(b[4..8].try_into().unwrap());
    Some((last as u64, len))
}

/// Last LBA and block length from READ CAPACITY (16).
pub fn parse_capacity_16(b: &[u8]) -> Option<(u64, u32)> {
    if b.len() < 12 {
        return None;
    }
    let last = u64::from_be_bytes(b[0..8].try_into().unwrap());
    let len = u32::from_be_bytes(b[8..12].try_into().unwrap());
    Some((last, len))
}

/// Sense key with additional sense code and qualifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    pub const NO_SENSE: u8 = 0x0;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;

    fn key_name(&self) -> &'static str {
        match self.key {
            0x0 => "no sense",
            0x1 => "recovered error",
            0x2 => "not ready",
            0x3 => "medium error",
            0x4 => "hardware error",
            0x5 => "illegal request",
            0x6 => "unit attention",
            0x7 => "data protect",
            0xB => "aborted command",
            _ => "other",
        }
    }
}

impl fmt::Display for Sense {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (sense key {:X}h, ASC {:02X}h, ASCQ {:02X}h)",
            self.key_name(),
            self.key,
            self.asc,
            self.ascq
        )
    }
}

/// Fixed (70h/71h) or descriptor (72h/73h) format sense data.
pub fn parse_sense(b: &[u8]) -> Option<Sense> {
    match b.first()? & 0x7F {
        0x70 | 0x71 if b.len() >= 14 => Some(Sense {
            key: b[2] & 0x0F,
            asc: b[12],
            ascq: b[13],
        }),
        0x72 | 0x73 if b.len() >= 4 => Some(Sense {
            key: b[1] & 0x0F,
            asc: b[2],
            ascq: b[3],
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_picks_10_or_16() {
        let c = read_write(false, 0x0102_0304, 8);
        assert_eq!(c.as_slice(), &[READ_10, 0, 1, 2, 3, 4, 0, 0, 8, 0]);
        let c = read_write(true, u32::MAX as u64 - 7, 8);
        assert_eq!(c.opcode(), WRITE_10);
        // ends past 2^32 blocks
        let c = read_write(false, u32::MAX as u64 - 6, 8);
        assert_eq!(c.opcode(), READ_16);
        assert_eq!(&c.as_slice()[2..10], &(u32::MAX as u64 - 6).to_be_bytes());
        assert_eq!(&c.as_slice()[10..14], &8u32.to_be_bytes());
        assert_eq!(read_write(false, 0, 0x1_0000).opcode(), READ_16);
    }

    #[test]
    fn inquiry_parsing() {
        let mut b = [0u8; 36];
        b[0] = 0x00;
        b[1] = 0x80;
        b[8..16].copy_from_slice(b"Generic ");
        b[16..32].copy_from_slice(b"Flash Disk      ");
        b[32..36].copy_from_slice(b"8.07");
        let i = parse_inquiry(&b).unwrap();
        assert_eq!(i.device_type, 0);
        assert!(i.removable);
        assert_eq!((i.vendor.as_str(), i.product.as_str(), i.revision.as_str()), ("Generic", "Flash Disk", "8.07"));
        assert!(parse_inquiry(&b[..35]).is_none());
    }

    #[test]
    fn capacity_parsing() {
        let b = [0x00, 0x3B, 0x9F, 0xFF, 0x00, 0x00, 0x02, 0x00];
        assert_eq!(parse_capacity_10(&b), Some((0x003B_9FFF, 512)));
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&0x1_D1C0_BEAFu64.to_be_bytes());
        b[8..12].copy_from_slice(&4096u32.to_be_bytes());
        assert_eq!(parse_capacity_16(&b), Some((0x1_D1C0_BEAF, 4096)));
    }

    #[test]
    fn sense_formats() {
        let mut fixed = [0u8; 18];
        fixed[0] = 0x70;
        fixed[2] = 0x06;
        fixed[12] = 0x28;
        assert_eq!(parse_sense(&fixed), Some(Sense { key: 6, asc: 0x28, ascq: 0 }));
        let desc = [0x72, 0x05, 0x20, 0x00, 0, 0, 0, 0];
        assert_eq!(parse_sense(&desc), Some(Sense { key: 5, asc: 0x20, ascq: 0 }));
        assert_eq!(parse_sense(&[0u8; 18]), None);
        assert_eq!(
            Sense { key: 2, asc: 0x3A, ascq: 0 }.to_string(),
            "not ready (sense key 2h, ASC 3Ah, ASCQ 00h)"
        );
    }
}
