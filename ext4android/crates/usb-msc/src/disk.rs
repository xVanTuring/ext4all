//! A SCSI direct-access unit behind Bulk-Only Transport: what a USB stick
//! or disk enclosure presents.

use crate::bot::{Bot, Data, Outcome};
use crate::scsi::{self, Inquiry, Sense};
use crate::{Error, Result, Transport};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
/// Flushing a large drive cache can take a while.
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a unit may report "becoming ready" after it is plugged in.
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const READY_POLL: Duration = Duration::from_millis(250);
/// Default size of one READ or WRITE command.
const DEFAULT_COMMAND_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    pub inquiry: Inquiry,
    pub max_lun: u8,
    pub block_size: u32,
    pub blocks: u64,
}

impl DiskInfo {
    pub fn size(&self) -> u64 {
        self.blocks * self.block_size as u64
    }
}

pub struct Disk<T: Transport> {
    bot: Mutex<Bot<T>>,
    info: DiskInfo,
    sync_supported: AtomicBool,
    command_bytes: AtomicUsize,
}

impl<T: Transport> Disk<T> {
    /// Identify the unit, wait until it is ready and read its capacity.
    pub fn open(t: T) -> Result<Disk<T>> {
        let mut bot = Bot::new(t);
        let max_lun = bot.max_lun();
        let inquiry = inquiry(&mut bot)?;
        if inquiry.device_type != 0 {
            return Err(Error::Invalid(format!(
                "peripheral device type {} is not a block device",
                inquiry.device_type
            )));
        }
        wait_ready(&mut bot)?;
        let (blocks, block_size) = capacity(&mut bot)?;
        let info = DiskInfo {
            inquiry,
            max_lun,
            block_size,
            blocks,
        };
        log::info!(
            "{} {} {}: {} blocks of {} bytes, max LUN {}",
            info.inquiry.vendor,
            info.inquiry.product,
            info.inquiry.revision,
            info.blocks,
            info.block_size,
            info.max_lun
        );
        Ok(Disk {
            bot: Mutex::new(bot),
            info,
            sync_supported: AtomicBool::new(true),
            command_bytes: AtomicUsize::new(DEFAULT_COMMAND_BYTES),
        })
    }

    pub fn info(&self) -> &DiskInfo {
        &self.info
    }

    /// Whether SYNCHRONIZE CACHE works (some bridges reject it).
    pub fn sync_supported(&self) -> bool {
        self.sync_supported.load(Ordering::Relaxed)
    }

    /// Size of one READ or WRITE command (rounded down to whole blocks).
    pub fn set_command_bytes(&self, n: usize) {
        self.command_bytes.store(n.max(self.info.block_size as usize), Ordering::Relaxed);
    }

    /// Largest single bulk transfer of the transport.
    pub fn set_max_transfer(&self, n: usize) {
        self.lock().transport_mut().set_max_transfer(n);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bot<T>> {
        self.bot.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn blocks_per_command(&self) -> usize {
        let bs = self.info.block_size as usize;
        (self.command_bytes.load(Ordering::Relaxed) / bs).max(1)
    }

    fn check_range(&self, lba: u64, len: usize) -> Result<()> {
        let bs = self.info.block_size as usize;
        if len % bs != 0 {
            return Err(Error::Invalid(format!("{len} bytes is not a multiple of the block size {bs}")));
        }
        let end = lba.checked_add((len / bs) as u64);
        if end.is_none_or(|e| e > self.info.blocks) {
            return Err(Error::Invalid(format!("blocks {lba}+{} beyond the end of the disk", len / bs)));
        }
        Ok(())
    }

    /// Read whole blocks starting at `lba`.
    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<()> {
        self.check_range(lba, buf.len())?;
        let bs = self.info.block_size as usize;
        let step = self.blocks_per_command() * bs;
        let mut bot = self.lock();
        for (i, chunk) in buf.chunks_mut(step).enumerate() {
            let at = lba + (i * step / bs) as u64;
            let len = chunk.len();
            let cdb = scsi::read_write(false, at, (len / bs) as u32);
            io(&mut bot, cdb.as_slice(), Data::In(chunk), len)?;
        }
        Ok(())
    }

    /// Write whole blocks starting at `lba`.
    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<()> {
        self.check_range(lba, buf.len())?;
        let bs = self.info.block_size as usize;
        let step = self.blocks_per_command() * bs;
        let mut bot = self.lock();
        for (i, chunk) in buf.chunks(step).enumerate() {
            let at = lba + (i * step / bs) as u64;
            let cdb = scsi::read_write(true, at, (chunk.len() / bs) as u32);
            io(&mut bot, cdb.as_slice(), Data::Out(chunk), chunk.len())?;
        }
        Ok(())
    }

    /// Flush the drive's write cache (a write barrier).
    pub fn sync(&self) -> Result<()> {
        if !self.sync_supported() {
            return Ok(());
        }
        let mut bot = self.lock();
        let o = bot.command(scsi::synchronize_cache().as_slice(), Data::None, SYNC_TIMEOUT)?;
        if o.passed {
            return Ok(());
        }
        let s = sense(&mut bot)?;
        if s.key == Sense::ILLEGAL_REQUEST {
            log::warn!("SYNCHRONIZE CACHE not supported ({s}); writes go without a cache flush");
            self.sync_supported.store(false, Ordering::Relaxed);
            return Ok(());
        }
        Err(Error::Check(s))
    }
}

/// REQUEST SENSE after a failed command.
fn sense<T: Transport>(bot: &mut Bot<T>) -> Result<Sense> {
    let mut b = [0u8; scsi::SENSE_LEN];
    let o = bot.command(scsi::request_sense().as_slice(), Data::In(&mut b), COMMAND_TIMEOUT)?;
    if !o.passed {
        return Err(Error::Protocol("REQUEST SENSE failed".into()));
    }
    scsi::parse_sense(&b[..o.transferred])
        .ok_or_else(|| Error::Invalid(format!("sense data {:02x?}", &b[..o.transferred])))
}

/// A data-in command that must pass and fill `buf` (up to `min` bytes).
fn query<T: Transport>(bot: &mut Bot<T>, cdb: &[u8], buf: &mut [u8], min: usize) -> Result<usize> {
    let o = bot.command(cdb, Data::In(buf), COMMAND_TIMEOUT)?;
    if !o.passed {
        return Err(Error::Check(sense(bot)?));
    }
    if o.transferred < min {
        return Err(Error::Invalid(format!(
            "command {:02X}h returned {} bytes, expected {min}",
            cdb[0], o.transferred
        )));
    }
    Ok(o.transferred)
}

fn inquiry<T: Transport>(bot: &mut Bot<T>) -> Result<Inquiry> {
    let mut b = [0u8; scsi::INQUIRY_LEN];
    query(bot, scsi::inquiry().as_slice(), &mut b, scsi::INQUIRY_LEN)?;
    scsi::parse_inquiry(&b).ok_or_else(|| Error::Invalid("INQUIRY data".into()))
}

fn wait_ready<T: Transport>(bot: &mut Bot<T>) -> Result<()> {
    let started = std::time::Instant::now();
    loop {
        let o = bot.command(scsi::test_unit_ready().as_slice(), Data::None, COMMAND_TIMEOUT)?;
        if o.passed {
            return Ok(());
        }
        let s = sense(bot)?;
        match s.key {
            // reset or medium change since the last command: ask again
            Sense::UNIT_ATTENTION => {}
            Sense::NOT_READY if started.elapsed() < READY_TIMEOUT => std::thread::sleep(READY_POLL),
            _ => return Err(Error::Check(s)),
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err(Error::Check(s));
        }
    }
}

fn capacity<T: Transport>(bot: &mut Bot<T>) -> Result<(u64, u32)> {
    let mut b = [0u8; 8];
    query(bot, scsi::read_capacity_10().as_slice(), &mut b, 8)?;
    let (mut last, mut bs) = scsi::parse_capacity_10(&b).unwrap();
    if last == u32::MAX as u64 {
        // over 2 TiB
        let mut b = [0u8; scsi::CAPACITY_16_LEN];
        query(bot, scsi::read_capacity_16().as_slice(), &mut b, 12)?;
        (last, bs) = scsi::parse_capacity_16(&b).unwrap();
    }
    if !bs.is_power_of_two() || !(512..=65536).contains(&bs) {
        return Err(Error::Invalid(format!("block size {bs}")));
    }
    if last == 0 {
        return Err(Error::Invalid("no medium capacity".into()));
    }
    Ok((last + 1, bs))
}

/// One READ or WRITE, retried once after a unit attention or a recovered
/// protocol error.
fn io<T: Transport>(bot: &mut Bot<T>, cdb: &[u8], mut data: Data<'_>, len: usize) -> Result<()> {
    let mut retried = false;
    loop {
        let d = match &mut data {
            Data::In(b) => Data::In(b),
            Data::Out(b) => Data::Out(b),
            Data::None => Data::None,
        };
        let e = match bot.command(cdb, d, COMMAND_TIMEOUT) {
            Ok(Outcome { passed: true, transferred }) if transferred == len => return Ok(()),
            Ok(Outcome { passed: true, transferred }) => {
                Error::Protocol(format!("moved {transferred} of {len} bytes"))
            }
            Ok(Outcome { passed: false, .. }) => Error::Check(sense(bot)?),
            Err(e) => e,
        };
        let retry = match &e {
            Error::Check(s) => s.key == Sense::UNIT_ATTENTION,
            Error::Protocol(_) | Error::Stall | Error::Timeout => true,
            _ => false,
        };
        if retried || !retry {
            return Err(e);
        }
        log::warn!("retrying command {:02X}h after: {e}", cdb[0]);
        retried = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::SimDisk;

    #[test]
    fn open_reads_identity_and_capacity() {
        let mut sim = SimDisk::new(2048, 512);
        sim.pending_attention = 1;
        sim.not_ready = 2;
        let d = Disk::open(sim).unwrap();
        assert_eq!(d.info().blocks, 2048);
        assert_eq!(d.info().block_size, 512);
        assert_eq!(d.info().inquiry.vendor, "SimDisk");
        assert_eq!(d.info().size(), 1 << 20);
    }

    #[test]
    fn read_and_write_across_commands() {
        let d = Disk::open(SimDisk::new(4096, 512)).unwrap();
        d.set_command_bytes(4096);
        let data: Vec<u8> = (0..300 * 512).map(|i| (i % 253) as u8).collect();
        d.write(10, &data).unwrap();
        let mut back = vec![0u8; data.len()];
        d.read(10, &mut back).unwrap();
        assert_eq!(back, data);
        let sim = d.bot.lock().unwrap();
        assert_eq!(&sim.transport().data[10 * 512..10 * 512 + data.len()], &data[..]);
    }

    #[test]
    fn rejects_unaligned_and_out_of_range() {
        let d = Disk::open(SimDisk::new(64, 4096)).unwrap();
        let mut b = vec![0u8; 1000];
        assert!(matches!(d.read(0, &mut b), Err(Error::Invalid(_))));
        let mut b = vec![0u8; 4096 * 2];
        assert!(matches!(d.read(63, &mut b), Err(Error::Invalid(_))));
        d.read(62, &mut b).unwrap();
    }

    #[test]
    fn unit_attention_during_io_is_retried() {
        let d = Disk::open(SimDisk::new(64, 512)).unwrap();
        d.bot.lock().unwrap().transport_mut().attention_on_io = true;
        let mut b = vec![0u8; 512];
        d.read(0, &mut b).unwrap();
        assert!(!d.bot.lock().unwrap().transport().attention_on_io);
    }

    #[test]
    fn sync_cache_and_unsupported_bridge() {
        let d = Disk::open(SimDisk::new(64, 512)).unwrap();
        d.sync().unwrap();
        assert_eq!(d.bot.lock().unwrap().transport().syncs, 1);

        let mut sim = SimDisk::new(64, 512);
        sim.sync_supported = false;
        let d = Disk::open(sim).unwrap();
        d.sync().unwrap();
        assert!(!d.sync_supported());
        let n = d.bot.lock().unwrap().transport().commands.len();
        d.sync().unwrap();
        assert_eq!(d.bot.lock().unwrap().transport().commands.len(), n, "no more SYNCHRONIZE CACHE");
    }

    #[test]
    fn disk_over_2_tib_reads_capacity_16() {
        let mut sim = SimDisk::new(64, 512);
        sim.capacity_override = Some(u32::MAX as u64 + 100);
        let d = Disk::open(sim).unwrap();
        assert_eq!(d.info().blocks, u32::MAX as u64 + 100);
        assert!(d.bot.lock().unwrap().transport().commands.contains(&scsi::SERVICE_ACTION_IN_16));
    }
}
