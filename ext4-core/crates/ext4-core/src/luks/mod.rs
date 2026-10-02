//! LUKS1 and LUKS2 (cryptsetup) whole-device encryption.
//!
//! [`Header::read`] recognizes a LUKS header, [`Header::unlock`] recovers
//! the volume key from a passphrase (PBKDF2 or Argon2 key slots,
//! anti-forensic splitting), and [`CryptDevice`] presents the decrypted
//! data area as a [`BlockDevice`] the file system runs on. Only AES
//! ciphers are supported (`aes-xts-plain64`, the cryptsetup default, plus
//! the older CBC variants); detached headers, authenticated encryption
//! (dm-integrity) and volumes in the middle of re-encryption are not.

mod cipher;
mod v1;
mod v2;

pub use cipher::SectorCipher;

use crate::crypto::{self, Secret};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use std::sync::Arc;

/// Magic of a primary header (LUKS1 and LUKS2).
pub const MAGIC: &[u8; 6] = b"LUKS\xba\xbe";

/// Largest key-derivation memory accepted from a header (KiB): cryptsetup
/// caps Argon2 at 4 GiB.
const MAX_ARGON2_KIB: u32 = 4 << 20;
/// Largest key slot material read (cryptsetup's limit).
const MAX_KEYSLOT_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
    Sha512,
}

impl Hash {
    pub fn parse(name: &str) -> Result<Hash> {
        match name.to_ascii_lowercase().as_str() {
            "sha1" => Ok(Hash::Sha1),
            "sha256" => Ok(Hash::Sha256),
            "sha512" => Ok(Hash::Sha512),
            other => Err(Error::unsupported(format!("LUKS hash {other}"))),
        }
    }

    pub fn size(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
            Hash::Sha512 => 64,
        }
    }

    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        fn run<D: sha2::Digest>(parts: &[&[u8]]) -> Vec<u8> {
            let mut h = D::new();
            for p in parts {
                h.update(p);
            }
            h.finalize().to_vec()
        }
        match self {
            Hash::Sha1 => run::<sha1::Sha1>(parts),
            Hash::Sha256 => run::<sha2::Sha256>(parts),
            Hash::Sha512 => run::<sha2::Sha512>(parts),
        }
    }

    fn pbkdf2(self, password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
        match self {
            Hash::Sha1 => pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, iterations, out),
            Hash::Sha256 => pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, out),
            Hash::Sha512 => pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password, salt, iterations, out),
        }
    }
}

/// How a key slot stretches the passphrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kdf {
    Pbkdf2 {
        hash: Hash,
        iterations: u32,
        salt: Vec<u8>,
    },
    Argon2 {
        /// Argon2id (else Argon2i)
        id: bool,
        time: u32,
        memory_kib: u32,
        lanes: u32,
        salt: Vec<u8>,
    },
}

impl Kdf {
    pub fn name(&self) -> &'static str {
        match self {
            Kdf::Pbkdf2 { .. } => "pbkdf2",
            Kdf::Argon2 { id: true, .. } => "argon2id",
            Kdf::Argon2 { id: false, .. } => "argon2i",
        }
    }

    fn derive(&self, passphrase: &[u8], len: usize) -> Result<Secret> {
        let mut out = Secret::zeroed(len);
        match self {
            Kdf::Pbkdf2 { hash, iterations, salt } => {
                if *iterations == 0 {
                    return Err(Error::corrupt("LUKS: zero PBKDF2 iterations"));
                }
                hash.pbkdf2(passphrase, salt, *iterations, &mut out);
            }
            Kdf::Argon2 {
                id,
                time,
                memory_kib,
                lanes,
                salt,
            } => {
                if *memory_kib > MAX_ARGON2_KIB || *time == 0 || *time > 1000 || *lanes == 0 || *lanes > 64 {
                    return Err(Error::corrupt("LUKS: unreasonable Argon2 parameters"));
                }
                let params = argon2::Params::new(*memory_kib, *time, *lanes, Some(len))
                    .map_err(|e| Error::corrupt(format!("LUKS: Argon2 parameters: {e}")))?;
                let alg = if *id {
                    argon2::Algorithm::Argon2id
                } else {
                    argon2::Algorithm::Argon2i
                };
                argon2::Argon2::new(alg, argon2::Version::V0x13, params)
                    .hash_password_into(passphrase, salt, &mut out)
                    .map_err(|e| Error::corrupt(format!("LUKS: Argon2: {e}")))?;
            }
        }
        Ok(out)
    }
}

/// One key slot: the volume key, split into `stripes` (anti-forensic
/// splitting) and encrypted with a key derived from a passphrase.
#[derive(Clone, Debug)]
pub struct Keyslot {
    pub id: u32,
    pub kdf: Kdf,
    /// LUKS2 priority (0 = only on explicit request, 2 = preferred).
    pub priority: u32,
    pub area_offset: u64,
    pub area_cipher: String,
    pub area_key_size: usize,
    pub key_size: usize,
    pub stripes: u32,
    pub af_hash: Hash,
}

/// How a volume key is verified (a PBKDF2 digest of the key).
#[derive(Clone, Debug)]
pub struct KeyDigest {
    pub hash: Hash,
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub digest: Vec<u8>,
    /// Key slots this digest belongs to (LUKS2).
    pub keyslots: Vec<u32>,
}

impl KeyDigest {
    fn matches(&self, key: &[u8]) -> bool {
        if self.iterations == 0 || self.digest.is_empty() {
            return false;
        }
        let mut out = vec![0u8; self.digest.len()];
        self.hash.pbkdf2(key, &self.salt, self.iterations, &mut out);
        crypto::ct_eq(&out, &self.digest)
    }
}

/// A parsed LUKS header.
#[derive(Clone, Debug)]
pub struct Header {
    pub version: u16,
    pub uuid: String,
    /// LUKS2 label (empty for LUKS1).
    pub label: String,
    /// Data cipher, e.g. `aes-xts-plain64`.
    pub cipher: String,
    /// Volume key size in bytes.
    pub key_size: usize,
    /// Start of the encrypted data (bytes).
    pub data_offset: u64,
    /// Size of the encrypted data; `None`: up to the end of the device.
    pub data_size: Option<u64>,
    pub sector_size: u32,
    /// Added to every sector's IV number (512-byte units).
    pub iv_tweak: u64,
    pub keyslots: Vec<Keyslot>,
    pub digests: Vec<KeyDigest>,
}

impl Header {
    /// Read the LUKS header of a device; `Ok(None)` if there is none.
    pub fn read(dev: &dyn BlockDevice) -> Result<Option<Header>> {
        if dev.size() < 4096 {
            return Ok(None);
        }
        let mut first = vec![0u8; 4096];
        dev.read_at(0, &mut first)?;
        if &first[..6] != MAGIC {
            return Ok(None);
        }
        let h = match u16::from_be_bytes([first[6], first[7]]) {
            1 => v1::parse(&first)?,
            2 => v2::read(dev, &first)?,
            v => return Err(Error::unsupported(format!("LUKS version {v}"))),
        };
        if let Some(sz) = h.data_size
            && h.data_offset.checked_add(sz).is_none_or(|end| end > dev.size())
        {
            return Err(Error::corrupt("LUKS data segment beyond the device"));
        }
        if h.data_offset >= dev.size() {
            return Err(Error::corrupt("LUKS data offset beyond the device"));
        }
        if !matches!(h.sector_size, 512 | 1024 | 2048 | 4096) || h.data_offset % 512 != 0 {
            return Err(Error::corrupt("LUKS: bad sector size or data offset"));
        }
        Ok(Some(h))
    }

    /// Whether the data cipher is one this implementation has.
    pub fn check_supported(&self) -> Result<()> {
        SectorCipher::check(&self.cipher, self.key_size)
    }

    /// Size of the decrypted volume.
    pub fn data_len(&self, device_size: u64) -> u64 {
        let n = self
            .data_size
            .unwrap_or_else(|| device_size.saturating_sub(self.data_offset));
        n - n % self.sector_size as u64
    }

    /// Whether `key` is this volume's key.
    pub fn verify_key(&self, key: &[u8]) -> bool {
        key.len() == self.key_size && self.digests.iter().any(|d| d.matches(key))
    }

    /// Key slots in the order cryptsetup tries them.
    fn slots_in_order(&self) -> Vec<&Keyslot> {
        let mut v: Vec<&Keyslot> = self.keyslots.iter().filter(|k| k.priority > 0).collect();
        v.sort_by_key(|k| (std::cmp::Reverse(k.priority), k.id));
        v
    }

    /// Recover the volume key from a passphrase; `Ok(None)` if no key
    /// slot opens with it. Each slot tried costs one key derivation
    /// (seconds and up to gigabytes of memory for Argon2).
    pub fn unlock(&self, dev: &dyn BlockDevice, passphrase: &[u8]) -> Result<Option<Secret>> {
        for ks in self.slots_in_order() {
            if let Some(k) = self.try_slot(dev, ks, passphrase)? {
                return Ok(Some(k));
            }
        }
        Ok(None)
    }

    fn try_slot(&self, dev: &dyn BlockDevice, ks: &Keyslot, passphrase: &[u8]) -> Result<Option<Secret>> {
        let split = ks.key_size as u64 * ks.stripes as u64;
        let area = split.div_ceil(512) * 512;
        if ks.stripes == 0 || ks.key_size == 0 || area > MAX_KEYSLOT_BYTES {
            return Err(Error::corrupt(format!("LUKS key slot {}: bad size", ks.id)));
        }
        if ks.area_offset.checked_add(area).is_none_or(|e| e > dev.size()) {
            return Err(Error::corrupt(format!("LUKS key slot {} beyond the device", ks.id)));
        }
        let derived = ks.kdf.derive(passphrase, ks.area_key_size)?;
        let c = SectorCipher::new(&ks.area_cipher, &derived)?;
        let mut material = Secret::zeroed(area as usize);
        dev.read_at(ks.area_offset, &mut material)?;
        c.crypt(0, 1, 512, &mut material, false);
        let key = af_merge(&material[..split as usize], ks.key_size, ks.stripes, ks.af_hash);
        let ok = self
            .digests
            .iter()
            .filter(|d| self.version == 1 || d.keyslots.contains(&ks.id))
            .any(|d| d.matches(&key));
        Ok(ok.then_some(key))
    }

    /// The decrypted data area, given the volume key.
    pub fn open(&self, dev: Arc<dyn BlockDevice>, key: &[u8]) -> Result<CryptDevice> {
        if !self.verify_key(key) {
            return Err(Error::invalid("wrong LUKS volume key"));
        }
        let cipher = SectorCipher::new(&self.cipher, key)?;
        Ok(CryptDevice {
            size: self.data_len(dev.size()),
            inner: dev,
            offset: self.data_offset,
            sector: self.sector_size as usize,
            iv_tweak: self.iv_tweak,
            cipher,
        })
    }

    /// One-line description.
    pub fn describe(&self) -> String {
        let kdfs: Vec<String> = self
            .keyslots
            .iter()
            .map(|k| format!("{}:{}", k.id, k.kdf.name()))
            .collect();
        format!(
            "LUKS{} {} {}-bit, sector {}, data at {}, key slots [{}]",
            self.version,
            self.cipher,
            self.key_size * 8,
            self.sector_size,
            self.data_offset,
            kdfs.join(" ")
        )
    }
}

/// Anti-forensic merge (cryptsetup `AF_merge`).
fn af_merge(src: &[u8], block: usize, stripes: u32, hash: Hash) -> Secret {
    let mut buf = Secret::zeroed(block);
    for i in 0..stripes as usize - 1 {
        for (b, s) in buf.iter_mut().zip(&src[i * block..(i + 1) * block]) {
            *b ^= s;
        }
        diffuse(&mut buf, hash);
    }
    let last = &src[(stripes as usize - 1) * block..stripes as usize * block];
    for (b, s) in buf.iter_mut().zip(last) {
        *b ^= s;
    }
    buf
}

/// Hash each digest-sized chunk with its big-endian index prepended.
fn diffuse(buf: &mut [u8], hash: Hash) {
    let d = hash.size();
    for (i, chunk) in buf.chunks_mut(d).enumerate() {
        let h = hash.digest(&[&(i as u32).to_be_bytes(), chunk]);
        let n = chunk.len();
        chunk.copy_from_slice(&h[..n]);
    }
}

/// The opened data area of a LUKS device: reads decrypt, writes encrypt.
/// Partial sectors are read, modified and written whole.
pub struct CryptDevice {
    inner: Arc<dyn BlockDevice>,
    offset: u64,
    size: u64,
    sector: usize,
    iv_tweak: u64,
    cipher: SectorCipher,
}

impl CryptDevice {
    fn iv_of(&self, sector: u64) -> u64 {
        self.iv_tweak + sector * (self.sector as u64 / 512)
    }

    fn check(&self, off: u64, len: usize) -> Result<()> {
        match off.checked_add(len as u64) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(Error::invalid(format!(
                "I/O beyond end of LUKS volume: offset {off} len {len} size {}",
                self.size
            ))),
        }
    }

    /// Read and decrypt sectors `[first, first + n)` into `buf`.
    fn read_sectors(&self, first: u64, buf: &mut [u8]) -> Result<()> {
        let ss = self.sector as u64;
        self.inner.read_at(self.offset + first * ss, buf)?;
        self.cipher.crypt(self.iv_of(first), ss / 512, self.sector, buf, false);
        Ok(())
    }
}

impl BlockDevice for CryptDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        let ss = self.sector as u64;
        if offset % ss == 0 && buf.len() as u64 % ss == 0 {
            return self.read_sectors(offset / ss, buf);
        }
        let first = offset / ss;
        let end = (offset + buf.len() as u64).div_ceil(ss);
        let mut tmp = vec![0u8; ((end - first) * ss) as usize];
        self.read_sectors(first, &mut tmp)?;
        let o = (offset - first * ss) as usize;
        buf.copy_from_slice(&tmp[o..o + buf.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        let ss = self.sector as u64;
        let first = offset / ss;
        let end = (offset + buf.len() as u64).div_ceil(ss);
        let mut tmp = vec![0u8; ((end - first) * ss) as usize];
        let o = (offset - first * ss) as usize;
        if o != 0 {
            self.read_sectors(first, &mut tmp[..self.sector])?;
        }
        let tail = (offset + buf.len() as u64) % ss;
        if tail != 0 && (end - 1 != first || o == 0) {
            let t = tmp.len() - self.sector;
            self.read_sectors(end - 1, &mut tmp[t..])?;
        }
        tmp[o..o + buf.len()].copy_from_slice(buf);
        self.cipher
            .crypt(self.iv_of(first), ss / 512, self.sector, &mut tmp, true);
        self.inner.write_at(self.offset + first * ss, &tmp)
    }

    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn sector_size(&self) -> u32 {
        (self.sector as u32).max(self.inner.sector_size())
    }
}

/// Trim a NUL-padded string field.
fn c_string(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

/// Hash with the header checksum algorithm `alg` (LUKS2).
fn checksum(alg: &str, parts: &[&[u8]]) -> Result<Vec<u8>> {
    let h = Hash::parse(alg)?;
    Ok(h.digest(parts))
}

#[cfg(test)]
pub(crate) mod tests;
