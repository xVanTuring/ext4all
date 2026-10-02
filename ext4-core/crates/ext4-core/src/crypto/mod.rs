//! Cryptographic building blocks shared by fscrypt (per-directory
//! encryption) and LUKS (whole-device encryption): AES in the modes Linux
//! uses (XTS, CBC with ciphertext stealing, CBC-ESSIV), SipHash, base64
//! and random bytes.

mod modes;
mod siphash;

pub mod base64;

pub use modes::{Aes, Cbc, CbcEssiv, Xts, cts_decrypt, cts_encrypt};
pub use siphash::siphash24;

use crate::error::{Error, Result};

/// Fill `buf` with random bytes from the operating system.
pub fn random_bytes(buf: &mut [u8]) -> Result<()> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    f.read_exact(buf)?;
    if buf.len() >= 16 && buf.iter().all(|&b| b == 0) {
        return Err(Error::Device(crate::error::errno::EIO));
    }
    Ok(())
}

/// Compare secrets without an early exit.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Wipe a buffer holding key material.
pub fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY-free volatile-ish wipe: black_box keeps the stores
        *b = std::hint::black_box(0);
    }
}

/// Owned key material that is wiped when dropped.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Secret {
        Secret(bytes)
    }

    pub fn zeroed(len: usize) -> Secret {
        Secret(vec![0; len])
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for Secret {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl std::ops::DerefMut for Secret {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

/// Parse a hexadecimal string (whitespace ignored).
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    digits.chunks(2).map(|p| Some(val(p[0])? << 4 | val(p[1])?)).collect()
}

pub fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        assert_eq!(from_hex("00ff 10Ab"), Some(vec![0, 0xff, 0x10, 0xab]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(to_hex(&[1, 0xfe]), "01fe");
    }

    #[test]
    fn random_is_not_constant() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        random_bytes(&mut a).unwrap();
        random_bytes(&mut b).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
