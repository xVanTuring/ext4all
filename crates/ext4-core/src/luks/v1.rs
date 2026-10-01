//! LUKS1 header (`struct luks_phdr`, big-endian, 592 bytes).

use super::{Hash, Header, Kdf, KeyDigest, Keyslot, c_string};
use crate::error::{Error, Result};

const KEY_ENABLED: u32 = 0x00AC71F3;
const KEY_DISABLED: u32 = 0x0000DEAD;

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

pub(super) fn parse(b: &[u8]) -> Result<Header> {
    let cipher_name = c_string(&b[8..40]);
    let cipher_mode = c_string(&b[40..72]);
    let hash = Hash::parse(&c_string(&b[72..104]))?;
    let payload = be32(b, 104) as u64;
    let key_bytes = be32(b, 108) as usize;
    if !(16..=64).contains(&key_bytes) {
        return Err(Error::corrupt(format!("LUKS1: key of {key_bytes} bytes")));
    }
    let cipher = format!("{cipher_name}-{cipher_mode}");
    let digest = KeyDigest {
        hash,
        iterations: be32(b, 164),
        salt: b[132..164].to_vec(),
        digest: b[112..132].to_vec(),
        keyslots: Vec::new(),
    };
    let mut keyslots = Vec::new();
    for i in 0..8 {
        let o = 208 + i * 48;
        match be32(b, o) {
            KEY_ENABLED => {}
            KEY_DISABLED => continue,
            v => return Err(Error::corrupt(format!("LUKS1 key slot {i}: state {v:#x}"))),
        }
        let stripes = be32(b, o + 44);
        keyslots.push(Keyslot {
            id: i as u32,
            kdf: Kdf::Pbkdf2 {
                hash,
                iterations: be32(b, o + 4),
                salt: b[o + 8..o + 40].to_vec(),
            },
            priority: 1,
            area_offset: be32(b, o + 40) as u64 * 512,
            area_cipher: cipher.clone(),
            area_key_size: key_bytes,
            key_size: key_bytes,
            stripes,
            af_hash: hash,
        });
    }
    Ok(Header {
        version: 1,
        uuid: c_string(&b[168..208]),
        label: String::new(),
        cipher,
        key_size: key_bytes,
        data_offset: payload * 512,
        data_size: None,
        sector_size: 512,
        iv_tweak: 0,
        keyslots,
        digests: vec![digest],
    })
}
