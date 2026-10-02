//! USB disks as ext4-core block devices, and the probe of experiment M0:
//! what the app sees on a disk through its own USB access.

use ext4_core::error::errno;
use ext4_core::{AlignedDevice, BlockDevice, Fs, MountOptions};
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;
use usb_msc::{Disk, Transport};

/// A byte range of a disk (one partition, or all of it).
pub struct Window<T: Transport> {
    disk: Arc<Disk<T>>,
    start: u64,
    len: u64,
    read_only: bool,
}

impl<T: Transport> Window<T> {
    pub fn new(disk: Arc<Disk<T>>, start: u64, len: u64, read_only: bool) -> Window<T> {
        Window {
            disk,
            start,
            len,
            read_only,
        }
    }

    fn lba(&self, offset: u64, len: usize) -> ext4_core::Result<u64> {
        if offset.checked_add(len as u64).is_none_or(|end| end > self.len) {
            return Err(ext4_core::Error::invalid(format!(
                "I/O beyond the end of the volume: offset {offset} len {len} size {}",
                self.len
            )));
        }
        Ok((self.start + offset) / self.disk.info().block_size as u64)
    }
}

fn fs_error(e: usb_msc::Error) -> ext4_core::Error {
    log::error!("USB disk: {e}");
    ext4_core::Error::Device(if e.is_disconnected() { errno::ENXIO } else { errno::EIO })
}

/// Offsets and lengths arrive aligned to the block size: wrap in
/// [`AlignedDevice`].
impl<T: Transport> BlockDevice for Window<T> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> ext4_core::Result<()> {
        let lba = self.lba(offset, buf.len())?;
        self.disk.read(lba, buf).map_err(fs_error)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> ext4_core::Result<()> {
        if self.read_only {
            return Err(ext4_core::Error::ReadOnly);
        }
        let lba = self.lba(offset, buf.len())?;
        self.disk.write(lba, buf).map_err(fs_error)
    }

    fn flush(&self) -> ext4_core::Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.disk.sync().map_err(fs_error)
    }

    fn size(&self) -> u64 {
        self.len
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn sector_size(&self) -> u32 {
        self.disk.info().block_size
    }
}

const MIB: u64 = 1 << 20;
/// Bytes read for each speed measurement.
const SPEED_BYTES: u64 = 16 * MIB;
/// (bytes per READ command, bytes per bulk transfer)
const SPEED_CASES: [(usize, usize); 5] = [
    (64 << 10, 64 << 10),
    (256 << 10, 64 << 10),
    (1 << 20, 64 << 10),
    (1 << 20, 256 << 10),
    (1 << 20, 1 << 20),
];
const LIST_LIMIT: usize = 12;

/// Identify the disk, read its partition table, mount every ext2/3/4
/// volume read-only and measure read speed. Writes nothing to the disk.
pub fn probe<T: Transport + 'static>(t: T) -> String {
    let mut out = String::new();
    let disk = match Disk::open(t) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            let _ = writeln!(out, "open failed: {e}");
            return out;
        }
    };
    let info = disk.info().clone();
    let bs = info.block_size;
    let _ = writeln!(
        out,
        "device: {} {} {}{}",
        info.inquiry.vendor,
        info.inquiry.product,
        info.inquiry.revision,
        if info.inquiry.removable { " (removable)" } else { "" }
    );
    let _ = writeln!(
        out,
        "capacity: {} blocks of {bs} bytes = {:.2} GiB, LUNs: {}",
        info.blocks,
        info.size() as f64 / (1u64 << 30) as f64,
        info.max_lun as u32 + 1
    );

    let whole = AlignedDevice::new(Window::new(disk.clone(), 0, info.size(), true));
    let mut read = |o: u64, b: &mut [u8]| whole.read_at(o, b);
    let volumes: Vec<(String, u64, u64)> = match part::read(&mut read, bs, info.size()) {
        Err(e) => {
            let _ = writeln!(out, "partition table: {e}");
            return out;
        }
        Ok(part::Table::None) => {
            let _ = writeln!(out, "partition table: none (whole disk)");
            vec![("whole disk".into(), 0, info.size())]
        }
        Ok(table) => {
            match &table {
                part::Table::Gpt {
                    block_size,
                    from_backup,
                    ..
                } => {
                    let _ = writeln!(
                        out,
                        "partition table: GPT, {block_size}-byte blocks{}",
                        if *from_backup { ", primary header damaged, backup used" } else { "" }
                    );
                }
                _ => {
                    let _ = writeln!(out, "partition table: MBR");
                }
            }
            table
                .partitions()
                .iter()
                .map(|p| {
                    let kind = match &p.kind {
                        part::Kind::Gpt { type_guid, name, .. } => format!("type {type_guid}, name \"{name}\""),
                        part::Kind::Mbr { type_id } => format!("type {type_id:02X}h"),
                    };
                    (format!("partition {} ({kind})", p.number), p.start, p.len)
                })
                .collect()
        }
    };

    for (name, start, len) in volumes {
        let _ = writeln!(
            out,
            "\n{name}: offset {} MiB, {:.2} GiB",
            start / MIB,
            len as f64 / (1u64 << 30) as f64
        );
        if start % bs as u64 != 0 {
            let _ = writeln!(out, "  not aligned to the {bs}-byte block size, skipped");
            continue;
        }
        let dev: Arc<dyn BlockDevice> = Arc::new(AlignedDevice::new(Window::new(disk.clone(), start, len, true)));
        describe_volume(&mut out, dev);
    }

    let _ = writeln!(out, "\nSYNCHRONIZE CACHE: {}", match disk.sync() {
        Ok(()) if disk.sync_supported() => "supported".to_string(),
        Ok(()) => "not supported by the bridge".to_string(),
        Err(e) => format!("failed: {e}"),
    });

    let _ = writeln!(out, "\nread speed ({} MiB from the start of the disk):", SPEED_BYTES / MIB);
    let total = SPEED_BYTES.min(info.size() / bs as u64 * bs as u64);
    for (command, transfer) in SPEED_CASES {
        disk.set_command_bytes(command);
        disk.set_max_transfer(transfer);
        let _ = writeln!(
            out,
            "  {:>4} KiB commands, {:>4} KiB transfers: {}",
            command >> 10,
            transfer >> 10,
            match read_speed(&disk, total) {
                Ok(mbs) => format!("{mbs:.1} MB/s"),
                Err(e) => format!("failed: {e}"),
            }
        );
    }
    out
}

fn describe_volume(out: &mut String, dev: Arc<dyn BlockDevice>) {
    match ext4_core::luks::Header::read(&*dev) {
        Ok(Some(h)) => {
            let _ = writeln!(out, "  LUKS: {}", h.describe());
            return;
        }
        Ok(None) => {}
        Err(e) => {
            let _ = writeln!(out, "  reading failed: {e}");
            return;
        }
    }
    let sb = match Fs::probe(&*dev) {
        Ok(sb) => sb,
        Err(_) => {
            let _ = writeln!(out, "  no ext2/3/4 file system");
            return;
        }
    };
    let _ = writeln!(
        out,
        "  ext4: label \"{}\", {} blocks of {} bytes",
        sb.volume_name(),
        sb.blocks_count(),
        sb.block_size()
    );
    let opts = MountOptions {
        read_only: true,
        ..Default::default()
    };
    let started = Instant::now();
    let mut fs = match Fs::mount(dev, opts) {
        Ok(fs) => fs,
        Err(e) => {
            let _ = writeln!(out, "  read-only mount failed: {e}");
            return;
        }
    };
    let _ = writeln!(out, "  mounted read-only in {} ms", started.elapsed().as_millis());
    for r in &fs.mount_report().read_only_reasons {
        let _ = writeln!(out, "  read-only because: {r}");
    }
    let st = fs.statfs();
    let _ = writeln!(
        out,
        "  free: {:.2} of {:.2} GiB",
        (st.avail_blocks * st.block_size as u64) as f64 / (1u64 << 30) as f64,
        (st.blocks * st.block_size as u64) as f64 / (1u64 << 30) as f64
    );
    let root = fs.root();
    match fs.list_dir(root) {
        Ok(entries) => {
            let names: Vec<String> = entries
                .iter()
                .filter(|e| e.name != b"." && e.name != b"..")
                .map(|e| String::from_utf8_lossy(&e.name).into_owned())
                .collect();
            let shown = names.iter().take(LIST_LIMIT).cloned().collect::<Vec<_>>().join(", ");
            let more = names.len().saturating_sub(LIST_LIMIT);
            let _ = writeln!(
                out,
                "  root: {} entries: {shown}{}",
                names.len(),
                if more > 0 { format!(" and {more} more") } else { String::new() }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "  listing the root failed: {e}");
        }
    }
}

/// MB/s reading `total` bytes from block 0.
fn read_speed<T: Transport>(disk: &Disk<T>, total: u64) -> usb_msc::Result<f64> {
    let bs = disk.info().block_size as u64;
    let mut buf = vec![0u8; (4 * MIB).min(total) as usize];
    let started = Instant::now();
    let mut done = 0u64;
    while done < total {
        let n = buf.len().min((total - done) as usize);
        disk.read(done / bs, &mut buf[..n])?;
        done += n as u64;
    }
    Ok(total as f64 / 1e6 / started.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ext4_core::{FileType, FormatOptions, MemDevice};
    use usb_msc::mock::SimDisk;

    const PART_LBA: u64 = 2048;

    /// An ext4 volume of `len` bytes holding one file.
    fn ext4_image(len: usize) -> Vec<u8> {
        let dev = Arc::new(MemDevice::new(len));
        let opts = FormatOptions {
            label: "USBDATA".into(),
            ..Default::default()
        };
        ext4_core::format(&*dev, &opts, &mut |_, _| {}).unwrap();
        let mut fs = Fs::mount(dev.clone(), MountOptions::default()).unwrap();
        let root = fs.root();
        let f = fs.create(root, b"movie.mkv", FileType::Regular, 0o644, 1000, 1000, 0).unwrap();
        fs.write(f.ino, 0, &[7u8; 5000]).unwrap();
        fs.unmount().unwrap();
        dev.snapshot()
    }

    /// A 64 MiB disk of 512-byte blocks with an MBR and one Linux
    /// partition (GPT parsing is covered by the `part` tests).
    fn sim_disk() -> SimDisk {
        let blocks = 64 * 2048;
        let mut sim = SimDisk::new(blocks, 512);
        let count = blocks - PART_LBA;
        let image = ext4_image((count * 512) as usize);
        let o = (PART_LBA * 512) as usize;
        sim.data[o..o + image.len()].copy_from_slice(&image);
        let e = 446;
        sim.data[e + 4] = 0x83;
        sim.data[e + 8..e + 12].copy_from_slice(&(PART_LBA as u32).to_le_bytes());
        sim.data[e + 12..e + 16].copy_from_slice(&(count as u32).to_le_bytes());
        sim.data[510] = 0x55;
        sim.data[511] = 0xAA;
        sim
    }

    #[test]
    fn probe_reports_partition_and_ext4_contents() {
        let report = probe(sim_disk());
        assert!(report.contains("device: SimDisk Bulk Only Disk 1.00 (removable)"), "{report}");
        assert!(report.contains("partition table: MBR"), "{report}");
        assert!(report.contains("partition 1 (type 83h): offset 1 MiB"), "{report}");
        assert!(report.contains("label \"USBDATA\""), "{report}");
        assert!(report.contains("mounted read-only"), "{report}");
        assert!(report.contains("movie.mkv"), "{report}");
        assert!(report.contains("SYNCHRONIZE CACHE: supported"), "{report}");
        assert_eq!(report.matches(" MB/s").count(), SPEED_CASES.len(), "{report}");
    }

    #[test]
    fn whole_disk_volume_without_table() {
        let mut sim = SimDisk::new(32 * 2048, 512);
        let image = ext4_image(sim.data.len());
        sim.data.copy_from_slice(&image);
        let report = probe(sim);
        assert!(report.contains("partition table: none (whole disk)"), "{report}");
        assert!(report.contains("movie.mkv"), "{report}");
    }

    #[test]
    fn window_rejects_writes_when_read_only_and_io_past_the_end() {
        let disk = Arc::new(Disk::open(SimDisk::new(64, 512)).unwrap());
        let w = Window::new(disk.clone(), 512, 8 * 512, true);
        assert!(matches!(w.write_at(0, &[0; 512]), Err(ext4_core::Error::ReadOnly)));
        let mut b = [0u8; 512];
        assert!(w.read_at(8 * 512, &mut b).is_err());
        w.read_at(7 * 512, &mut b).unwrap();
        let w = Window::new(disk, 512, 8 * 512, false);
        w.write_at(0, &[9; 512]).unwrap();
        let mut back = [0u8; 512];
        w.read_at(0, &mut back).unwrap();
        assert_eq!(back, [9; 512]);
    }
}
