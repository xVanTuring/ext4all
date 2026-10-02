use super::*;

fn read_table(img: &[u8], bs: u32) -> Table {
    let mut r = |o: u64, b: &mut [u8]| -> std::result::Result<(), String> {
        b.copy_from_slice(&img[o as usize..o as usize + b.len()]);
        Ok(())
    };
    read(&mut r, bs, img.len() as u64).unwrap()
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

const ENTRIES: usize = 128;

/// A GPT disk of `blocks` blocks with partitions (first LBA, last LBA, name).
fn gpt_image(bs: usize, blocks: u64, parts: &[(u64, u64, &str)]) -> Vec<u8> {
    let mut img = vec![0u8; bs * blocks as usize];
    img[MBR_ENTRIES + 4] = PROTECTIVE;
    put32(&mut img, MBR_ENTRIES + 8, 1);
    put32(&mut img, MBR_ENTRIES + 12, (blocks - 1).min(u32::MAX as u64) as u32);
    img[510..512].copy_from_slice(&MBR_SIGNATURE);

    let mut entries = vec![0u8; ENTRIES * 128];
    for (i, (first, last, name)) in parts.iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        e[0..16].copy_from_slice(&Guid::LINUX_DATA.0);
        e[16..32].fill(i as u8 + 1);
        put64(e, 32, *first);
        put64(e, 40, *last);
        for (j, u) in name.encode_utf16().enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let entry_blocks = entries.len().div_ceil(bs) as u64;
    let backup_entries = blocks - 1 - entry_blocks;
    for (lba, alt, entries_lba) in [(1, blocks - 1, 2), (blocks - 1, 1, backup_entries)] {
        let mut h = vec![0u8; GPT_MIN_HEADER];
        h[0..8].copy_from_slice(GPT_SIGNATURE);
        put32(&mut h, 8, 0x0001_0000);
        put32(&mut h, 12, GPT_MIN_HEADER as u32);
        put64(&mut h, 24, lba);
        put64(&mut h, 32, alt);
        put64(&mut h, 40, 2 + entry_blocks);
        put64(&mut h, 48, backup_entries - 1);
        h[56..72].fill(0xAB);
        put64(&mut h, 72, entries_lba);
        put32(&mut h, 80, ENTRIES as u32);
        put32(&mut h, 84, 128);
        put32(&mut h, 88, crc32(&entries));
        let c = crc32(&h);
        put32(&mut h, 16, c);
        let o = lba as usize * bs;
        img[o..o + h.len()].copy_from_slice(&h);
        let o = entries_lba as usize * bs;
        img[o..o + entries.len()].copy_from_slice(&entries);
    }
    img
}

fn mbr_entry(s: &mut [u8], slot: usize, type_id: u8, lba: u32, count: u32) {
    let o = MBR_ENTRIES + slot * 16;
    s[o + 4] = type_id;
    put32(s, o + 8, lba);
    put32(s, o + 12, count);
    s[510..512].copy_from_slice(&MBR_SIGNATURE);
}

#[test]
fn crc32_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

#[test]
fn guid_text_form() {
    assert_eq!(Guid::LINUX_DATA.to_string(), "0FC63DAF-8483-4772-8E79-3D69D8477DE4");
}

#[test]
fn gpt_partitions_with_names() {
    let img = gpt_image(512, 4096, &[(34, 1000, "first"), (2048, 4000, "数据")]);
    let t = read_table(&img, 512);
    let Table::Gpt {
        block_size,
        from_backup,
        partitions,
    } = &t
    else {
        panic!("{t:?}");
    };
    assert_eq!((*block_size, *from_backup), (512, false));
    assert_eq!(partitions.len(), 2);
    assert_eq!(partitions[0].number, 1);
    assert_eq!(partitions[0].start, 34 * 512);
    assert_eq!(partitions[0].len, 967 * 512);
    assert_eq!(partitions[1].number, 2);
    assert_eq!(partitions[1].start, 2048 * 512);
    match &partitions[1].kind {
        Kind::Gpt { type_guid, name, guid } => {
            assert_eq!(*type_guid, Guid::LINUX_DATA);
            assert_eq!(name, "数据");
            assert_eq!(guid.0, [2; 16]);
        }
        k => panic!("{k:?}"),
    }
}

#[test]
fn damaged_primary_gpt_uses_the_backup() {
    let mut img = gpt_image(512, 4096, &[(34, 1000, "a")]);
    img[512 + 16] ^= 0xFF; // primary header CRC
    let t = read_table(&img, 512);
    assert!(matches!(t, Table::Gpt { from_backup: true, .. }), "{t:?}");
    assert_eq!(t.partitions().len(), 1);
}

#[test]
fn damaged_entries_are_rejected() {
    let mut img = gpt_image(512, 4096, &[(34, 1000, "a")]);
    img[2 * 512] ^= 0xFF; // primary entry array
    let t = read_table(&img, 512);
    assert!(matches!(t, Table::Gpt { from_backup: true, .. }), "{t:?}");
}

#[test]
fn gpt_of_4k_disk_found_when_enclosure_reports_512() {
    let img = gpt_image(4096, 512, &[(6, 400, "data")]);
    let t = read_table(&img, 512);
    let Table::Gpt {
        block_size, partitions, ..
    } = &t
    else {
        panic!("{t:?}");
    };
    assert_eq!(*block_size, 4096);
    assert_eq!(partitions[0].start, 6 * 4096);
}

#[test]
fn both_gpt_headers_damaged_means_no_table() {
    let mut img = gpt_image(512, 4096, &[(34, 1000, "a")]);
    img[512] = 0;
    let n = img.len();
    img[n - 512] = 0;
    assert_eq!(read_table(&img, 512), Table::None);
}

#[test]
fn mbr_with_logical_partitions() {
    let bs = 512usize;
    let mut img = vec![0u8; 16384 * bs];
    mbr_entry(&mut img, 0, 0x83, 2048, 4096);
    mbr_entry(&mut img, 1, 0x05, 8192, 8000);
    // first EBR: a logical partition and a link to the next EBR
    let ebr1 = 8192 * bs;
    mbr_entry(&mut img[ebr1..ebr1 + 512], 0, 0x83, 63, 1000);
    mbr_entry(&mut img[ebr1..ebr1 + 512], 1, 0x05, 2000, 1100);
    let ebr2 = (8192 + 2000) * bs;
    mbr_entry(&mut img[ebr2..ebr2 + 512], 0, 0x07, 63, 1000);

    let t = read_table(&img, 512);
    let got: Vec<(u32, u64, u64)> = t.partitions().iter().map(|p| (p.number, p.start, p.len)).collect();
    assert_eq!(
        got,
        vec![
            (1, 2048 * 512, 4096 * 512),
            (5, (8192 + 63) * 512, 1000 * 512),
            (6, (10192 + 63) * 512, 1000 * 512),
        ]
    );
    assert!(matches!(t, Table::Mbr { .. }));
    assert_eq!(t.partitions()[2].kind, Kind::Mbr { type_id: 0x07 });
}

#[test]
fn ebr_chain_loop_terminates() {
    let bs = 512usize;
    let mut img = vec![0u8; 16384 * bs];
    mbr_entry(&mut img, 0, 0x0F, 8192, 8000);
    let ebr = 8192 * bs;
    mbr_entry(&mut img[ebr..ebr + 512], 0, 0x83, 63, 100);
    // the link points to the second EBR, whose link points back
    mbr_entry(&mut img[ebr..ebr + 512], 1, 0x05, 1000, 200);
    let ebr2 = (8192 + 1000) * bs;
    mbr_entry(&mut img[ebr2..ebr2 + 512], 0, 0x83, 63, 100);
    mbr_entry(&mut img[ebr2..ebr2 + 512], 1, 0x05, 0x10, 200);
    let ebr3 = (8192 + 0x10) * bs;
    mbr_entry(&mut img[ebr3..ebr3 + 512], 1, 0x05, 1000, 200);
    let t = read_table(&img, 512);
    assert_eq!(t.partitions().len(), 2);
}

#[test]
fn fat_boot_sector_is_not_an_mbr() {
    let mut img = vec![0u8; 4096 * 512];
    img[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    img[3..11].copy_from_slice(b"MSDOS5.0");
    img[446..510].fill(0x33); // boot code where the entries would be
    img[510..512].copy_from_slice(&MBR_SIGNATURE);
    assert_eq!(read_table(&img, 512), Table::None);
}

#[test]
fn whole_disk_file_system_has_no_table() {
    let mut img = vec![0u8; 4096 * 512];
    // ext4 superblock magic, nothing at sector 0
    img[1024 + 56..1024 + 58].copy_from_slice(&0xEF53u16.to_le_bytes());
    assert_eq!(read_table(&img, 512), Table::None);
}

#[test]
fn partitions_beyond_the_disk_are_implausible() {
    let mut img = vec![0u8; 4096 * 512];
    mbr_entry(&mut img, 0, 0x83, 2048, 100_000);
    assert_eq!(read_table(&img, 512), Table::None);
}

#[test]
fn read_errors_are_reported() {
    let mut r = |_: u64, _: &mut [u8]| -> std::result::Result<(), String> { Err("unplugged".into()) };
    let e = read(&mut r, 512, 1 << 20).unwrap_err();
    assert_eq!(e.to_string(), "reading the partition table failed: unplugged");
}
