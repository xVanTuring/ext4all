//! AES block cipher modes as implemented by the Linux crypto API:
//! `xts(aes)`, `cbc(aes)`, `cts(cbc(aes))` (CS3: the last two blocks are
//! always swapped) and `essiv(cbc(aes),sha256)`.

use crate::error::{Error, Result};
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use sha2::{Digest, Sha256};

/// AES with a 128, 192 or 256 bit key.
#[derive(Clone)]
pub enum Aes {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl Aes {
    pub fn new(key: &[u8]) -> Result<Aes> {
        Ok(match key.len() {
            16 => Aes::A128(aes::Aes128::new(GenericArray::from_slice(key))),
            24 => Aes::A192(aes::Aes192::new(GenericArray::from_slice(key))),
            32 => Aes::A256(aes::Aes256::new(GenericArray::from_slice(key))),
            n => return Err(Error::invalid(format!("AES key of {n} bytes"))),
        })
    }

    pub fn encrypt(&self, b: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(&mut b[..]);
        match self {
            Aes::A128(c) => c.encrypt_block(b),
            Aes::A192(c) => c.encrypt_block(b),
            Aes::A256(c) => c.encrypt_block(b),
        }
    }

    pub fn decrypt(&self, b: &mut [u8; 16]) {
        let b = GenericArray::from_mut_slice(&mut b[..]);
        match self {
            Aes::A128(c) => c.decrypt_block(b),
            Aes::A192(c) => c.decrypt_block(b),
            Aes::A256(c) => c.decrypt_block(b),
        }
    }
}

fn block(buf: &mut [u8], i: usize) -> &mut [u8; 16] {
    (&mut buf[i * 16..i * 16 + 16]).try_into().expect("16-byte block")
}

fn xor(dst: &mut [u8; 16], src: &[u8; 16]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= s;
    }
}

/// XTS (IEEE 1619) over whole 16-byte blocks: every sector or data unit
/// Linux encrypts with it is a multiple of the block size, so ciphertext
/// stealing is never needed.
#[derive(Clone)]
pub struct Xts {
    data: Aes,
    tweak: Aes,
}

impl Xts {
    /// `key` is the data key followed by the tweak key (32, 48 or 64 bytes).
    pub fn new(key: &[u8]) -> Result<Xts> {
        if key.len() % 2 != 0 {
            return Err(Error::invalid("odd XTS key length"));
        }
        let (a, b) = key.split_at(key.len() / 2);
        Ok(Xts {
            data: Aes::new(a)?,
            tweak: Aes::new(b)?,
        })
    }

    fn run(&self, iv: &[u8; 16], buf: &mut [u8], encrypt: bool) {
        assert!(buf.len() % 16 == 0, "XTS data must be whole blocks");
        let mut t = *iv;
        self.tweak.encrypt(&mut t);
        for i in 0..buf.len() / 16 {
            let b = block(buf, i);
            xor(b, &t);
            if encrypt {
                self.data.encrypt(b);
            } else {
                self.data.decrypt(b);
            }
            xor(b, &t);
            // multiply the tweak by x in GF(2^128), little-endian
            let carry = t[15] >> 7;
            for j in (1..16).rev() {
                t[j] = (t[j] << 1) | (t[j - 1] >> 7);
            }
            t[0] = (t[0] << 1) ^ (0x87 * carry);
        }
    }

    pub fn encrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        self.run(iv, buf, true)
    }

    pub fn decrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        self.run(iv, buf, false)
    }
}

/// CBC over whole blocks.
#[derive(Clone)]
pub struct Cbc {
    aes: Aes,
}

impl Cbc {
    pub fn new(key: &[u8]) -> Result<Cbc> {
        Ok(Cbc { aes: Aes::new(key)? })
    }

    pub fn encrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        cbc_encrypt(&self.aes, iv, buf)
    }

    pub fn decrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        cbc_decrypt(&self.aes, iv, buf)
    }
}

fn cbc_encrypt(aes: &Aes, iv: &[u8; 16], buf: &mut [u8]) {
    assert!(buf.len() % 16 == 0, "CBC data must be whole blocks");
    let mut prev = *iv;
    for i in 0..buf.len() / 16 {
        let b = block(buf, i);
        xor(b, &prev);
        aes.encrypt(b);
        prev = *b;
    }
}

fn cbc_decrypt(aes: &Aes, iv: &[u8; 16], buf: &mut [u8]) {
    assert!(buf.len() % 16 == 0, "CBC data must be whole blocks");
    let mut prev = *iv;
    for i in 0..buf.len() / 16 {
        let b = block(buf, i);
        let c = *b;
        aes.decrypt(b);
        xor(b, &prev);
        prev = c;
    }
}

/// CBC with ESSIV: the IV of each sector is encrypted with a key derived
/// as SHA-256 of the data key (AES-256).
#[derive(Clone)]
pub struct CbcEssiv {
    aes: Aes,
    essiv: Aes,
}

impl CbcEssiv {
    pub fn new(key: &[u8]) -> Result<CbcEssiv> {
        let h = Sha256::digest(key);
        Ok(CbcEssiv {
            aes: Aes::new(key)?,
            essiv: Aes::new(&h)?,
        })
    }

    fn iv(&self, iv: &[u8; 16]) -> [u8; 16] {
        let mut v = *iv;
        self.essiv.encrypt(&mut v);
        v
    }

    pub fn encrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        cbc_encrypt(&self.aes, &self.iv(iv), buf)
    }

    pub fn decrypt(&self, iv: &[u8; 16], buf: &mut [u8]) {
        cbc_decrypt(&self.aes, &self.iv(iv), buf)
    }
}

/// CBC with ciphertext stealing, variant CS3 (`cts(cbc(aes))` in Linux):
/// a single block is plain CBC; otherwise the last two ciphertext blocks
/// are swapped and the final one truncated. `buf` holds at least 16 bytes.
pub fn cts_encrypt(aes: &Aes, iv: &[u8; 16], buf: &mut [u8]) -> Result<()> {
    let n = buf.len();
    if n < 16 {
        return Err(Error::invalid("CTS input shorter than a block"));
    }
    if n == 16 {
        cbc_encrypt(aes, iv, buf);
        return Ok(());
    }
    let full = n.div_ceil(16) - 2; // blocks before the last two
    let d = n - 16 * (full + 1); // bytes in the last block (1..=16)
    cbc_encrypt(aes, iv, &mut buf[..full * 16]);
    let mut x = if full == 0 { *iv } else { *block(buf, full - 1) };
    // E(n-1) = AES(P(n-1) ^ X)
    let mut en1: [u8; 16] = buf[full * 16..full * 16 + 16].try_into().unwrap();
    xor(&mut en1, &x);
    aes.encrypt(&mut en1);
    // C(n-1) = AES((P(n) || 0) ^ E(n-1))
    let mut pn = [0u8; 16];
    pn[..d].copy_from_slice(&buf[full * 16 + 16..]);
    xor(&mut pn, &en1);
    aes.encrypt(&mut pn);
    buf[full * 16..full * 16 + 16].copy_from_slice(&pn);
    buf[full * 16 + 16..].copy_from_slice(&en1[..d]);
    x.fill(0);
    Ok(())
}

pub fn cts_decrypt(aes: &Aes, iv: &[u8; 16], buf: &mut [u8]) -> Result<()> {
    let n = buf.len();
    if n < 16 {
        return Err(Error::invalid("CTS input shorter than a block"));
    }
    if n == 16 {
        cbc_decrypt(aes, iv, buf);
        return Ok(());
    }
    let full = n.div_ceil(16) - 2;
    let d = n - 16 * (full + 1);
    let x = if full == 0 { *iv } else { *block(buf, full - 1) };
    cbc_decrypt(aes, iv, &mut buf[..full * 16]);
    // D = AES^-1(C(n-1)) = (P(n) || 0) ^ E(n-1)
    let mut dn: [u8; 16] = buf[full * 16..full * 16 + 16].try_into().unwrap();
    aes.decrypt(&mut dn);
    // E(n-1) = C(n) || tail of D
    let mut en1 = dn;
    en1[..d].copy_from_slice(&buf[full * 16 + 16..]);
    let mut pn = [0u8; 16];
    for i in 0..d {
        pn[i] = dn[i] ^ en1[i];
    }
    let mut pn1 = en1;
    aes.decrypt(&mut pn1);
    xor(&mut pn1, &x);
    buf[full * 16..full * 16 + 16].copy_from_slice(&pn1);
    buf[full * 16 + 16..].copy_from_slice(&pn[..d]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::from_hex;

    fn h(s: &str) -> Vec<u8> {
        from_hex(s).unwrap()
    }

    #[test]
    fn aes_fips197() {
        let key = h("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let mut b: [u8; 16] = h("00112233445566778899aabbccddeeff").try_into().unwrap();
        let a = Aes::new(&key).unwrap();
        a.encrypt(&mut b);
        assert_eq!(b.to_vec(), h("8ea2b7ca516745bfeafc49904b496089"));
        a.decrypt(&mut b);
        assert_eq!(b.to_vec(), h("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn xts_ieee1619_vector_4() {
        // IEEE 1619-2007 vector 4: AES-128-XTS, data unit 0, 512 bytes
        let key = h("2718281828459045235360287471352631415926535897932384626433832795");
        let mut data: Vec<u8> = (0..512).map(|i| i as u8).collect();
        let iv = [0u8; 16];
        let x = Xts::new(&key).unwrap();
        x.encrypt(&iv, &mut data);
        assert_eq!(
            data[..32].to_vec(),
            h("27a7479befa1d476489f308cd4cfa6e2a96e4bbe3208ff25287dd3819616e89c")
        );
        x.decrypt(&iv, &mut data);
        assert!(data.iter().enumerate().all(|(i, &b)| b == i as u8));
    }

    #[test]
    fn xts_ieee1619_vector_10_aes256() {
        // IEEE 1619-2007 vector 10: AES-256-XTS, data unit 0xff
        let key = h(
            "27182818284590452353602874713526624977572470936999595749669676273141592653589793238462643383279502884197169399375105820974944592",
        );
        let mut iv = [0u8; 16];
        iv[0] = 0xff;
        let mut data: Vec<u8> = (0..512).map(|i| i as u8).collect();
        let x = Xts::new(&key).unwrap();
        x.encrypt(&iv, &mut data);
        assert_eq!(
            data[..32].to_vec(),
            h("1c3b3a102f770386e4836c99e370cf9bea00803f5e482357a4ae12d414a3e63b")
        );
        x.decrypt(&iv, &mut data);
        assert!(data.iter().enumerate().all(|(i, &b)| b == i as u8));
    }

    #[test]
    fn cbc_sp800_38a() {
        let key = h("2b7e151628aed2a6abf7158809cf4f3c");
        let iv: [u8; 16] = h("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut data = h("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        let c = Cbc::new(&key).unwrap();
        c.encrypt(&iv, &mut data);
        assert_eq!(
            data,
            h("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2")
        );
        c.decrypt(&iv, &mut data);
        assert_eq!(
            data,
            h("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51")
        );
    }

    #[test]
    fn cts_rfc3962_vectors() {
        // RFC 3962 appendix B (AES-128, IV 0): the same CS3 construction
        let key = h("636869636b656e207465726979616b69");
        let a = Aes::new(&key).unwrap();
        let iv = [0u8; 16];
        let cases = [
            (
                "4920776f756c64206c696b652074686520",
                "c6353568f2bf8cb4d8a580362da7ff7f97",
            ),
            (
                "4920776f756c64206c696b65207468652047656e6572616c20476175277320",
                "fc00783e0efdb2c1d445d4c8eff7ed2297687268d6ecccc0c07b25e25ecfe5",
            ),
            (
                "4920776f756c64206c696b65207468652047656e6572616c2047617527732043",
                "39312523a78662d5be7fcbcc98ebf5a897687268d6ecccc0c07b25e25ecfe584",
            ),
            (
                "4920776f756c64206c696b65207468652047656e6572616c20476175277320436869636b656e2c20706c656173652c",
                "97687268d6ecccc0c07b25e25ecfe584b3fffd940c16a18c1b5549d2f838029e39312523a78662d5be7fcbcc98ebf5",
            ),
        ];
        for (p, c) in cases {
            let mut buf = h(p);
            cts_encrypt(&a, &iv, &mut buf).unwrap();
            assert_eq!(buf, h(c), "encrypt {p}");
            cts_decrypt(&a, &iv, &mut buf).unwrap();
            assert_eq!(buf, h(p), "decrypt {p}");
        }
    }

    #[test]
    fn cts_roundtrip_all_lengths() {
        let a = Aes::new(&[7u8; 32]).unwrap();
        let iv = [3u8; 16];
        for n in 16..100 {
            let p: Vec<u8> = (0..n).map(|i| (i * 7 + n) as u8).collect();
            let mut b = p.clone();
            cts_encrypt(&a, &iv, &mut b).unwrap();
            assert_ne!(b, p);
            cts_decrypt(&a, &iv, &mut b).unwrap();
            assert_eq!(b, p, "length {n}");
        }
        assert!(cts_encrypt(&a, &iv, &mut [0u8; 15]).is_err());
    }

    #[test]
    fn essiv_roundtrip() {
        let c = CbcEssiv::new(&[9u8; 16]).unwrap();
        let mut iv = [0u8; 16];
        iv[0] = 5;
        let p: Vec<u8> = (0..4096).map(|i| i as u8).collect();
        let mut b = p.clone();
        c.encrypt(&iv, &mut b);
        // the IV that actually chains is AES-256(SHA-256(key), iv)
        let mut real = iv;
        Aes::new(&Sha256::digest([9u8; 16])).unwrap().encrypt(&mut real);
        let mut chk = p.clone();
        Cbc::new(&[9u8; 16]).unwrap().encrypt(&real, &mut chk);
        assert_eq!(b, chk);
        c.decrypt(&iv, &mut b);
        assert_eq!(b, p);
    }
}
