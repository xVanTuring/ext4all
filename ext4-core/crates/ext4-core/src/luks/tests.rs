use super::*;
use crate::crypto::base64;
use crate::device::MemDevice;

/// Anti-forensic split (cryptsetup `AF_split`) with deterministic filler.
fn af_split(key: &[u8], stripes: u32, hash: Hash) -> Vec<u8> {
    let n = key.len();
    let mut out = vec![0u8; n * stripes as usize];
    let mut buf = vec![0u8; n];
    for i in 0..stripes as usize - 1 {
        for (j, b) in out[i * n..(i + 1) * n].iter_mut().enumerate() {
            *b = (i * 31 + j * 7 + 3) as u8;
        }
        for (b, s) in buf.iter_mut().zip(&out[i * n..(i + 1) * n]) {
            *b ^= s;
        }
        diffuse(&mut buf, hash);
    }
    let last = (stripes as usize - 1) * n;
    for j in 0..n {
        out[last + j] = key[j] ^ buf[j];
    }
    out
}

pub(crate) struct Built {
    pub dev: MemDevice,
    pub key: Vec<u8>,
}

const PASS: &[u8] = b"correct horse";

fn pbkdf2(hash: Hash, p: &[u8], salt: &[u8], it: u32, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    hash.pbkdf2(p, salt, it, &mut out);
    out
}

/// A LUKS1 device: `cipher_mode` like "xts-plain64", 2 MiB of data.
pub(crate) fn build_v1(cipher_mode: &str, key_len: usize, hash: Hash) -> Built {
    let size = 4 << 20;
    let dev = MemDevice::new(size);
    let key: Vec<u8> = (0..key_len).map(|i| (i * 13 + 1) as u8).collect();
    let mut h = vec![0u8; 592];
    h[..6].copy_from_slice(MAGIC);
    h[6..8].copy_from_slice(&1u16.to_be_bytes());
    h[8..11].copy_from_slice(b"aes");
    h[40..40 + cipher_mode.len()].copy_from_slice(cipher_mode.as_bytes());
    let hn = match hash {
        Hash::Sha1 => "sha1",
        Hash::Sha256 => "sha256",
        Hash::Sha512 => "sha512",
    };
    h[72..72 + hn.len()].copy_from_slice(hn.as_bytes());
    let payload: u32 = 4096; // sectors: 2 MiB
    h[104..108].copy_from_slice(&payload.to_be_bytes());
    h[108..112].copy_from_slice(&(key_len as u32).to_be_bytes());
    let mk_salt = [0x11u8; 32];
    h[112..132].copy_from_slice(&pbkdf2(hash, &key, &mk_salt, 1000, 20));
    h[132..164].copy_from_slice(&mk_salt);
    h[164..168].copy_from_slice(&1000u32.to_be_bytes());
    h[168..204].copy_from_slice(b"12345678-1234-1234-1234-123456789abc");
    let cipher = format!("aes-{cipher_mode}");
    for slot in 0..8 {
        let o = 208 + slot * 48;
        if slot != 1 {
            h[o..o + 4].copy_from_slice(&0xDEADu32.to_be_bytes());
            continue;
        }
        let salt = [0x22u8; 32];
        let it = 1000u32;
        let area_sector: u32 = 8;
        h[o..o + 4].copy_from_slice(&0x00AC71F3u32.to_be_bytes());
        h[o + 4..o + 8].copy_from_slice(&it.to_be_bytes());
        h[o + 8..o + 40].copy_from_slice(&salt);
        h[o + 40..o + 44].copy_from_slice(&area_sector.to_be_bytes());
        h[o + 44..o + 48].copy_from_slice(&4000u32.to_be_bytes());
        let derived = pbkdf2(hash, PASS, &salt, it, key_len);
        let mut mat = af_split(&key, 4000, hash);
        mat.resize(mat.len().div_ceil(512) * 512, 0);
        SectorCipher::new(&cipher, &derived)
            .unwrap()
            .crypt(0, 1, 512, &mut mat, true);
        dev.write_at(area_sector as u64 * 512, &mat).unwrap();
    }
    dev.write_at(0, &h).unwrap();
    Built { dev, key }
}

/// A LUKS2 device with one Argon2id (or PBKDF2) key slot.
pub(crate) fn build_v2(sector_size: u32, argon: bool, iv_tweak: u64) -> Built {
    let size = 8u64 << 20;
    let dev = MemDevice::new(size as usize);
    let key: Vec<u8> = (0..64).map(|i| (i * 7 + 5) as u8).collect();
    let hdr_size = 0x4000u64;
    let area_off = 0x8000u64;
    let data_off = 0x100000u64;
    let salt = vec![0x33u8; 32];
    let kdf = if argon {
        Kdf::Argon2 {
            id: true,
            time: 1,
            memory_kib: 64,
            lanes: 1,
            salt: salt.clone(),
        }
    } else {
        Kdf::Pbkdf2 {
            hash: Hash::Sha256,
            iterations: 1000,
            salt: salt.clone(),
        }
    };
    let derived = kdf.derive(PASS, 64).unwrap();
    let mut mat = af_split(&key, 4000, Hash::Sha256);
    mat.resize(mat.len().div_ceil(512) * 512, 0);
    SectorCipher::new("aes-xts-plain64", &derived)
        .unwrap()
        .crypt(0, 1, 512, &mut mat, true);
    dev.write_at(area_off, &mat).unwrap();
    let dsalt = vec![0x44u8; 32];
    let digest = pbkdf2(Hash::Sha256, &key, &dsalt, 1000, 32);
    let kdf_json = if argon {
        serde_json::json!({"type": "argon2id", "time": 1, "memory": 64, "cpus": 1,
            "salt": base64::encode_std(&salt)})
    } else {
        serde_json::json!({"type": "pbkdf2", "hash": "sha256", "iterations": 1000,
            "salt": base64::encode_std(&salt)})
    };
    let json = serde_json::json!({
        "keyslots": {"3": {"type": "luks2", "key_size": 64,
            "af": {"type": "luks1", "stripes": 4000, "hash": "sha256"},
            "area": {"type": "raw", "offset": area_off.to_string(), "size": mat.len().to_string(),
                     "encryption": "aes-xts-plain64", "key_size": 64},
            "kdf": kdf_json}},
        "tokens": {},
        "segments": {"0": {"type": "crypt", "offset": data_off.to_string(), "size": "dynamic",
            "iv_tweak": iv_tweak.to_string(), "encryption": "aes-xts-plain64", "sector_size": sector_size}},
        "digests": {"0": {"type": "pbkdf2", "keyslots": ["3"], "segments": ["0"], "hash": "sha256",
            "iterations": 1000, "salt": base64::encode_std(&dsalt), "digest": base64::encode_std(&digest)}},
        "config": {"json_size": (hdr_size - 4096).to_string(), "keyslots_size": "1015808"}
    });
    for (off, magic) in [(0u64, MAGIC), (hdr_size, b"SKUL\xba\xbe")] {
        let mut bin = vec![0u8; 4096];
        bin[..6].copy_from_slice(magic);
        bin[6..8].copy_from_slice(&2u16.to_be_bytes());
        bin[8..16].copy_from_slice(&hdr_size.to_be_bytes());
        bin[16..24].copy_from_slice(&7u64.to_be_bytes());
        bin[24..29].copy_from_slice(b"crypt");
        bin[72..78].copy_from_slice(b"sha256");
        bin[168..204].copy_from_slice(b"abcdef01-2345-6789-abcd-ef0123456789");
        bin[256..264].copy_from_slice(&off.to_be_bytes());
        let mut j = serde_json::to_vec(&json).unwrap();
        j.resize((hdr_size - 4096) as usize, 0);
        let sum = Hash::Sha256.digest(&[&bin, &j]);
        bin[448..480].copy_from_slice(&sum);
        dev.write_at(off, &bin).unwrap();
        dev.write_at(off + 4096, &j).unwrap();
    }
    Built { dev, key }
}

#[test]
fn af_roundtrip() {
    for hash in [Hash::Sha1, Hash::Sha256, Hash::Sha512] {
        let key: Vec<u8> = (0..64).collect();
        let s = af_split(&key, 4000, hash);
        assert_eq!(&*af_merge(&s, 64, 4000, hash), &key[..]);
        // 20 is not a multiple of SHA-256's size: diffuse pads
        let k20: Vec<u8> = (0..20).collect();
        assert_eq!(&*af_merge(&af_split(&k20, 10, hash), 20, 10, hash), &k20[..]);
    }
}

#[test]
fn luks1_unlock_and_io() {
    for (mode, len, hash) in [
        ("xts-plain64", 64, Hash::Sha256),
        ("xts-plain64", 32, Hash::Sha1),
        ("cbc-essiv:sha256", 32, Hash::Sha1),
        ("cbc-essiv:sha256", 16, Hash::Sha512),
    ] {
        let b = build_v1(mode, len, hash);
        let h = Header::read(&b.dev).unwrap().unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.cipher, format!("aes-{mode}"));
        assert_eq!(h.uuid, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(h.data_offset, 2 << 20);
        assert_eq!(h.keyslots.len(), 1);
        assert!(h.unlock(&b.dev, b"wrong").unwrap().is_none());
        let key = h.unlock(&b.dev, PASS).unwrap().expect("unlocks");
        assert_eq!(&*key, &b.key[..]);
        check_io(&h, Arc::new(b.dev), &key);
    }
}

#[test]
fn luks2_unlock_and_io() {
    for (ss, argon, tweak) in [(512, true, 0), (4096, false, 0), (4096, true, 8), (1024, false, 3)] {
        let b = build_v2(ss, argon, tweak);
        let h = Header::read(&b.dev).unwrap().unwrap();
        assert_eq!(h.version, 2);
        assert_eq!(h.label, "crypt");
        assert_eq!(h.sector_size, ss);
        assert_eq!(h.iv_tweak, tweak);
        assert_eq!(h.keyslots[0].id, 3);
        assert!(h.unlock(&b.dev, b"nope").unwrap().is_none());
        let key = h.unlock(&b.dev, PASS).unwrap().unwrap();
        assert_eq!(&*key, &b.key[..]);
        assert!(h.verify_key(&key));
        assert!(!h.verify_key(&[0u8; 64]));
        check_io(&h, Arc::new(b.dev), &key);
    }
}

#[test]
fn luks2_secondary_header_rescues_a_damaged_primary() {
    let b = build_v2(512, false, 0);
    // corrupt the primary JSON: its checksum no longer matches
    b.dev.write_at(4096 + 10, b"XX").unwrap();
    let h = Header::read(&b.dev).unwrap().unwrap();
    assert!(h.unlock(&b.dev, PASS).unwrap().is_some());
    // both damaged
    b.dev.write_at(0x4000 + 4096 + 10, b"XX").unwrap();
    assert!(Header::read(&b.dev).is_err());
}

#[test]
fn not_luks() {
    let d = MemDevice::new(1 << 20);
    assert!(Header::read(&d).unwrap().is_none());
    d.write_at(0, b"LUKS\xba\xbe\x00\x09").unwrap();
    assert!(Header::read(&d).is_err());
}

#[test]
fn hostile_headers_never_panic() {
    let b = build_v1("xts-plain64", 64, Hash::Sha256);
    let orig = b.dev.snapshot();
    for i in (6..592).step_by(7) {
        let d = MemDevice::from_vec(orig.clone());
        let mut x = [0u8; 1];
        d.read_at(i as u64, &mut x).unwrap();
        d.write_at(i as u64, &[x[0] ^ 0xa5]).unwrap();
        if let Ok(Some(h)) = Header::read(&d) {
            let _ = h.unlock(&d, PASS);
        }
    }
}

fn check_io(h: &Header, dev: Arc<dyn BlockDevice>, key: &[u8]) {
    let raw = dev.clone();
    let c = h.open(dev, key).unwrap();
    assert!(h.open(raw.clone(), &vec![1u8; key.len()]).is_err());
    let n = c.size();
    assert_eq!(n % h.sector_size as u64, 0);
    // unaligned write and read back
    let data: Vec<u8> = (0..10_000).map(|i| (i % 253) as u8).collect();
    c.write_at(777, &data).unwrap();
    let mut back = vec![0u8; data.len()];
    c.read_at(777, &mut back).unwrap();
    assert_eq!(back, data);
    // neighbours of the written range are untouched (zeros decrypt to
    // garbage, so compare with what was there before: write zeros first)
    let zeros = vec![0u8; 3 * h.sector_size as usize];
    c.write_at(0, &zeros).unwrap();
    c.write_at(10, b"abc").unwrap();
    let mut s = vec![0u8; 3 * h.sector_size as usize];
    c.read_at(0, &mut s).unwrap();
    assert_eq!(&s[10..13], b"abc");
    assert!(s[..10].iter().chain(&s[13..]).all(|&b| b == 0));
    // ciphertext on the raw device
    let mut r = vec![0u8; 16];
    raw.read_at(h.data_offset, &mut r).unwrap();
    assert_ne!(&r[10..13], b"abc");
    // the last sector, and refusal beyond it
    c.write_at(n - 5, b"tail!").unwrap();
    let mut t = [0u8; 5];
    c.read_at(n - 5, &mut t).unwrap();
    assert_eq!(&t, b"tail!");
    assert!(c.read_at(n - 4, &mut t).is_err());
}
