//! Proof that ext4-core runs on this device: format a small in-memory
//! volume, write a file, remount and read it back.

use ext4_core::{Error, FileType, FormatOptions, Fs, MemDevice, MountOptions, Result};
use std::sync::Arc;

const SIZE: usize = 16 << 20;
const NAME: &[u8] = b"hello.bin";

pub fn run() -> Result<String> {
    let dev = Arc::new(MemDevice::new(SIZE));
    let opts = FormatOptions {
        label: "selftest".into(),
        ..Default::default()
    };
    ext4_core::format(&*dev, &opts, &mut |_, _| {})?;

    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut fs = Fs::mount(dev.clone(), MountOptions::default())?;
    let root = fs.root();
    let file = fs.create(root, NAME, FileType::Regular, 0o644, 0, 0, 0)?;
    fs.write(file.ino, 0, &data)?;
    fs.unmount()?;

    let mut fs = Fs::mount(
        dev,
        MountOptions {
            read_only: true,
            ..Default::default()
        },
    )?;
    let root = fs.root();
    let ino = fs.lookup(root, NAME)?;
    let mut back = vec![0u8; data.len()];
    let mut done = 0;
    while done < back.len() {
        let n = fs.read(ino, done as u64, &mut back[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    if back != data {
        return Err(Error::corrupt(format!("read back {done} bytes that differ from the written data")));
    }
    let st = fs.statfs();
    Ok(format!(
        "formatted {} MiB (block size {}), wrote and read back {} bytes, {} of {} blocks free",
        SIZE >> 20,
        st.block_size,
        data.len(),
        st.free_blocks,
        st.blocks
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_test_passes() {
        let report = super::run().unwrap();
        assert!(report.contains("read back 300000 bytes"), "{report}");
    }
}
