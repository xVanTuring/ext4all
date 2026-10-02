//! fscrypt: Linux per-directory encryption (the ext4 `encrypt` feature).
//!
//! Every encrypted inode carries an encryption context in the xattr
//! `encryption.c` (index 9): the policy (algorithms, flags, master key
//! reference) plus a random 16-byte nonce. Regular file contents and the
//! names in encrypted directories (and symlink targets) are encrypted;
//! directory blocks, inodes and other metadata are not.
//!
//! Without the master key, names are presented as *no-key names* (base64url
//! of the ciphertext, with a hash prefix; see [`nokey_name`]) exactly as
//! Linux shows them, and file contents cannot be read. With the key
//! ([`Keyring::add`]) names and contents are decrypted and encrypted
//! transparently. Supported algorithms: AES-256-XTS and AES-128-CBC-ESSIV
//! for contents, AES-256-CTS and AES-128-CTS for names, in v1 and v2
//! policies, including the IV_INO_LBLK_64/32 flags used on Android. Other
//! algorithms (Adiantum, HCTR2, SM4) behave as if the key were missing.

pub mod protector;

use crate::crypto::{self, Aes, CbcEssiv, Secret, Xts};
use crate::error::{Error, Result};
use hkdf::Hkdf;
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::sync::Arc;

/// Name of the context attribute in the `encryption.` namespace.
pub const XATTR_NAME: &[u8] = b"c";

pub mod mode {
    pub const AES_256_XTS: u8 = 1;
    pub const AES_256_CTS: u8 = 4;
    pub const AES_128_CBC: u8 = 5;
    pub const AES_128_CTS: u8 = 6;
    pub const SM4_XTS: u8 = 7;
    pub const SM4_CTS: u8 = 8;
    pub const ADIANTUM: u8 = 9;
    pub const AES_256_HCTR2: u8 = 10;

    pub fn name(m: u8) -> &'static str {
        match m {
            AES_256_XTS => "AES-256-XTS",
            AES_256_CTS => "AES-256-CTS",
            AES_128_CBC => "AES-128-CBC-ESSIV",
            AES_128_CTS => "AES-128-CTS",
            SM4_XTS => "SM4-XTS",
            SM4_CTS => "SM4-CTS",
            ADIANTUM => "Adiantum",
            AES_256_HCTR2 => "AES-256-HCTR2",
            _ => "unknown",
        }
    }
}

pub mod flag {
    pub const PAD_MASK: u8 = 0x03;
    pub const DIRECT_KEY: u8 = 0x04;
    pub const IV_INO_LBLK_64: u8 = 0x08;
    pub const IV_INO_LBLK_32: u8 = 0x10;
}

/// Shortest ciphertext name (names are padded to at least one block).
pub const MIN_NAME_LEN: usize = 16;

const NONCE_SIZE: usize = 16;

/// HKDF contexts (`fs/crypto/fscrypt_private.h`).
mod hkdf_ctx {
    pub const KEY_IDENTIFIER: u8 = 1;
    pub const PER_FILE_ENC_KEY: u8 = 2;
    pub const IV_INO_LBLK_64_KEY: u8 = 4;
    pub const DIRHASH_KEY: u8 = 5;
    pub const IV_INO_LBLK_32_KEY: u8 = 6;
    pub const INODE_HASH_KEY: u8 = 7;
}

/// Reference to a master key: a v1 descriptor or a v2 identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySpec {
    V1([u8; 8]),
    V2([u8; 16]),
}

impl std::fmt::Display for KeySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeySpec::V1(d) => write!(f, "{}", crypto::to_hex(d)),
            KeySpec::V2(d) => write!(f, "{}", crypto::to_hex(d)),
        }
    }
}

/// An inode's encryption context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub contents_mode: u8,
    pub filenames_mode: u8,
    pub flags: u8,
    /// v2 only; 0 = the file system block size.
    pub log2_data_unit_size: u8,
    pub key: KeySpec,
    pub nonce: [u8; NONCE_SIZE],
}

impl Context {
    pub fn parse(raw: &[u8]) -> Result<Context> {
        let bad = || Error::corrupt(format!("bad encryption context ({} bytes)", raw.len()));
        match raw.first() {
            Some(1) if raw.len() == 28 => Ok(Context {
                contents_mode: raw[1],
                filenames_mode: raw[2],
                flags: raw[3],
                log2_data_unit_size: 0,
                key: KeySpec::V1(raw[4..12].try_into().unwrap()),
                nonce: raw[12..28].try_into().unwrap(),
            }),
            Some(2) if raw.len() == 40 => Ok(Context {
                contents_mode: raw[1],
                filenames_mode: raw[2],
                flags: raw[3],
                log2_data_unit_size: raw[4],
                key: KeySpec::V2(raw[8..24].try_into().unwrap()),
                nonce: raw[24..40].try_into().unwrap(),
            }),
            _ => Err(bad()),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        match &self.key {
            KeySpec::V1(d) => {
                let mut v = vec![1, self.contents_mode, self.filenames_mode, self.flags];
                v.extend_from_slice(d);
                v.extend_from_slice(&self.nonce);
                v
            }
            KeySpec::V2(id) => {
                let mut v = vec![
                    2,
                    self.contents_mode,
                    self.filenames_mode,
                    self.flags,
                    self.log2_data_unit_size,
                    0,
                    0,
                    0,
                ];
                v.extend_from_slice(id);
                v.extend_from_slice(&self.nonce);
                v
            }
        }
    }

    pub fn version(&self) -> u8 {
        match self.key {
            KeySpec::V1(_) => 1,
            KeySpec::V2(_) => 2,
        }
    }

    /// Same policy (everything but the nonce): what Linux requires of a
    /// file linked or moved into an encrypted directory.
    pub fn same_policy(&self, o: &Context) -> bool {
        self.contents_mode == o.contents_mode
            && self.filenames_mode == o.filenames_mode
            && self.flags == o.flags
            && self.log2_data_unit_size == o.log2_data_unit_size
            && self.key == o.key
    }

    /// The context of a new inode inheriting this directory's policy.
    pub fn inherit(&self) -> Result<Context> {
        let mut nonce = [0u8; NONCE_SIZE];
        crypto::random_bytes(&mut nonce)?;
        Ok(Context { nonce, ..self.clone() })
    }

    pub fn padding(&self) -> usize {
        4 << (self.flags & flag::PAD_MASK)
    }

    /// One-line description, like `fscrypt status`.
    pub fn describe(&self) -> String {
        format!(
            "v{} {}/{} pad {} flags {:#x} key {}",
            self.version(),
            mode::name(self.contents_mode),
            mode::name(self.filenames_mode),
            self.padding(),
            self.flags & !flag::PAD_MASK,
            self.key
        )
    }
}

/// A master key added by the user.
pub struct MasterKey {
    raw: Secret,
    hkdf: Hkdf<Sha512>,
}

impl MasterKey {
    fn new(raw: &[u8]) -> MasterKey {
        MasterKey {
            raw: Secret::new(raw.to_vec()),
            hkdf: Hkdf::<Sha512>::new(Some(&[0u8; 64]), raw),
        }
    }

    /// `HKDF-Expand(prk, "fscrypt\0" || context || info, len)`.
    fn expand(&self, context: u8, info: &[u8], len: usize) -> Secret {
        let mut full = Vec::with_capacity(9 + info.len());
        full.extend_from_slice(b"fscrypt\0");
        full.push(context);
        full.extend_from_slice(info);
        let mut out = Secret::zeroed(len);
        self.hkdf.expand(&full, &mut out).expect("HKDF output length");
        out
    }

    fn siphash_key(&self, context: u8, info: &[u8]) -> [u64; 2] {
        let k = self.expand(context, info, 16);
        [
            u64::from_le_bytes(k[..8].try_into().unwrap()),
            u64::from_le_bytes(k[8..].try_into().unwrap()),
        ]
    }

    /// v2 key identifier.
    pub fn identifier(&self) -> [u8; 16] {
        self.expand(hkdf_ctx::KEY_IDENTIFIER, &[], 16)[..].try_into().unwrap()
    }

    /// v1 descriptor as computed by the `fscrypt` and `e4crypt` tools: the
    /// first 8 bytes of SHA-512(SHA-512(key)).
    pub fn descriptor(&self) -> [u8; 8] {
        let h = Sha512::digest(Sha512::digest(&*self.raw));
        h[..8].try_into().unwrap()
    }
}

/// The identifiers under which an added key is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyIds {
    pub descriptor: [u8; 8],
    pub identifier: [u8; 16],
}

/// A master key opened by a protector, so the caller can remember it.
#[derive(Debug)]
pub struct UnlockedKey {
    pub ids: KeyIds,
    pub key: Secret,
}

/// Master keys available to a mounted file system.
#[derive(Default)]
pub struct Keyring {
    keys: HashMap<KeySpec, Arc<MasterKey>>,
}

impl Keyring {
    /// Add a raw master key (16 to 64 bytes). It is registered both as a
    /// v2 key (by its identifier) and as a v1 key (by the descriptor the
    /// Linux tools compute for it).
    pub fn add(&mut self, raw: &[u8]) -> Result<KeyIds> {
        if !(16..=64).contains(&raw.len()) {
            return Err(Error::invalid(format!(
                "fscrypt key of {} bytes (want 16 to 64)",
                raw.len()
            )));
        }
        let mk = Arc::new(MasterKey::new(raw));
        let ids = KeyIds {
            descriptor: mk.descriptor(),
            identifier: mk.identifier(),
        };
        self.keys.insert(KeySpec::V2(ids.identifier), mk.clone());
        self.keys.insert(KeySpec::V1(ids.descriptor), mk);
        Ok(ids)
    }

    /// Add a v1 key under an explicit descriptor (keys added to the Linux
    /// keyring by hand may use any descriptor).
    pub fn add_v1(&mut self, descriptor: [u8; 8], raw: &[u8]) -> Result<()> {
        if !(16..=64).contains(&raw.len()) {
            return Err(Error::invalid("fscrypt key length"));
        }
        self.keys.insert(KeySpec::V1(descriptor), Arc::new(MasterKey::new(raw)));
        Ok(())
    }

    pub fn get(&self, spec: &KeySpec) -> Option<Arc<MasterKey>> {
        self.keys.get(spec).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

#[derive(Clone)]
enum Cipher {
    Xts(Xts),
    CbcEssiv(CbcEssiv),
    Cts(Aes),
}

/// Keys and parameters for one unlocked inode.
#[derive(Clone)]
pub struct InodeCrypt {
    pub ctx: Context,
    cipher: Cipher,
    ino: u32,
    hashed_ino: u32,
    /// log2 of the contents data unit size.
    pub du_bits: u32,
    dirhash_key: Option<[u64; 2]>,
}

impl std::fmt::Debug for InodeCrypt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InodeCrypt")
            .field("ctx", &self.ctx)
            .field("ino", &self.ino)
            .finish()
    }
}

impl InodeCrypt {
    /// Derive the inode's key. `Ok(None)` when the policy uses an
    /// algorithm or flag combination this implementation lacks.
    pub fn derive(
        ctx: &Context,
        mk: &MasterKey,
        ino: u32,
        fs_uuid: &[u8; 16],
        is_reg: bool,
        block_bits: u32,
    ) -> Result<Option<InodeCrypt>> {
        let m = if is_reg { ctx.contents_mode } else { ctx.filenames_mode };
        let keysize = match (is_reg, m) {
            (true, mode::AES_256_XTS) => 64,
            (true, mode::AES_128_CBC) => 16,
            (false, mode::AES_256_CTS) => 32,
            (false, mode::AES_128_CTS) => 16,
            _ => return Ok(None),
        };
        let lblk_flags = flag::IV_INO_LBLK_64 | flag::IV_INO_LBLK_32;
        // DIRECT_KEY needs a 24-byte or longer IV (Adiantum, HCTR2)
        if ctx.flags & flag::DIRECT_KEY != 0 || ctx.flags & lblk_flags == lblk_flags {
            return Ok(None);
        }
        let mut hashed_ino = 0;
        let mut dirhash_key = None;
        let key = match ctx.key {
            KeySpec::V1(_) => {
                if ctx.flags & lblk_flags != 0 || mk.raw.len() < keysize {
                    return Ok(None);
                }
                // AES-128-ECB of the master key, keyed by the nonce
                let ecb = Aes::new(&ctx.nonce)?;
                let mut k = Secret::new(mk.raw[..keysize].to_vec());
                for c in k.chunks_mut(16) {
                    let b: &mut [u8; 16] = c.try_into().unwrap();
                    ecb.encrypt(b);
                }
                k
            }
            KeySpec::V2(_) => {
                if ctx.flags & flag::IV_INO_LBLK_64 != 0 {
                    let mut info = vec![m];
                    info.extend_from_slice(fs_uuid);
                    mk.expand(hkdf_ctx::IV_INO_LBLK_64_KEY, &info, keysize)
                } else if ctx.flags & flag::IV_INO_LBLK_32 != 0 {
                    let mut info = vec![m];
                    info.extend_from_slice(fs_uuid);
                    let hk = mk.siphash_key(hkdf_ctx::INODE_HASH_KEY, &[]);
                    hashed_ino = crypto::siphash24(hk, &(ino as u64).to_le_bytes()) as u32;
                    mk.expand(hkdf_ctx::IV_INO_LBLK_32_KEY, &info, keysize)
                } else {
                    mk.expand(hkdf_ctx::PER_FILE_ENC_KEY, &ctx.nonce, keysize)
                }
            }
        };
        if !is_reg && matches!(ctx.key, KeySpec::V2(_)) {
            dirhash_key = Some(mk.siphash_key(hkdf_ctx::DIRHASH_KEY, &ctx.nonce));
        }
        let cipher = match m {
            mode::AES_256_XTS => Cipher::Xts(Xts::new(&key)?),
            mode::AES_128_CBC => Cipher::CbcEssiv(CbcEssiv::new(&key)?),
            _ => Cipher::Cts(Aes::new(&key)?),
        };
        let du_bits = match ctx.key {
            KeySpec::V2(_) if ctx.log2_data_unit_size != 0 => ctx.log2_data_unit_size as u32,
            _ => block_bits,
        };
        if !(9..=block_bits).contains(&du_bits) {
            return Ok(None);
        }
        Ok(Some(InodeCrypt {
            ctx: ctx.clone(),
            cipher,
            ino,
            hashed_ino,
            du_bits,
            dirhash_key,
        }))
    }

    fn iv(&self, index: u64) -> [u8; 16] {
        let index = if self.ctx.flags & flag::IV_INO_LBLK_64 != 0 {
            index | (self.ino as u64) << 32
        } else if self.ctx.flags & flag::IV_INO_LBLK_32 != 0 {
            self.hashed_ino.wrapping_add(index as u32) as u64
        } else {
            index
        };
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&index.to_le_bytes());
        iv
    }

    pub fn data_unit_size(&self) -> usize {
        1 << self.du_bits
    }

    /// Encrypt or decrypt file contents in place. `buf` starts at a data
    /// unit boundary (data unit `first_du` of the file) and holds whole
    /// data units.
    pub fn crypt_data(&self, first_du: u64, buf: &mut [u8], encrypt: bool) -> Result<()> {
        let du = self.data_unit_size();
        if buf.len() % du != 0 {
            return Err(Error::invalid("encrypted I/O not aligned to data units"));
        }
        for (i, unit) in buf.chunks_mut(du).enumerate() {
            let iv = self.iv(first_du + i as u64);
            match (&self.cipher, encrypt) {
                (Cipher::Xts(x), true) => x.encrypt(&iv, unit),
                (Cipher::Xts(x), false) => x.decrypt(&iv, unit),
                (Cipher::CbcEssiv(c), true) => c.encrypt(&iv, unit),
                (Cipher::CbcEssiv(c), false) => c.decrypt(&iv, unit),
                (Cipher::Cts(_), _) => return Err(Error::invalid("not a contents key")),
            }
        }
        Ok(())
    }

    /// Ciphertext length of a `len`-byte name (`max_len` caps it).
    pub fn encrypted_len(&self, len: usize, max_len: usize) -> Result<usize> {
        if len > max_len {
            return Err(Error::NameTooLong);
        }
        let pad = self.ctx.padding();
        Ok(len.max(MIN_NAME_LEN).next_multiple_of(pad).min(max_len))
    }

    /// Encrypt a name (or symlink target) to `out_len` bytes.
    pub fn encrypt_name_to(&self, name: &[u8], out_len: usize) -> Result<Vec<u8>> {
        let Cipher::Cts(aes) = &self.cipher else {
            return Err(Error::invalid("not a filenames key"));
        };
        let mut buf = vec![0u8; out_len];
        buf[..name.len()].copy_from_slice(name);
        crypto::cts_encrypt(aes, &self.iv(0), &mut buf)?;
        Ok(buf)
    }

    /// Encrypt a directory entry name.
    pub fn encrypt_name(&self, name: &[u8]) -> Result<Vec<u8>> {
        let n = self.encrypted_len(name.len(), crate::ondisk::dirent::MAX_NAME_LEN)?;
        self.encrypt_name_to(name, n)
    }

    /// Decrypt a name: the plaintext stops at the first NUL of the padding.
    pub fn decrypt_name(&self, disk: &[u8]) -> Result<Vec<u8>> {
        let Cipher::Cts(aes) = &self.cipher else {
            return Err(Error::invalid("not a filenames key"));
        };
        if disk.len() < MIN_NAME_LEN {
            return Err(Error::corrupt("encrypted name shorter than 16 bytes"));
        }
        let mut buf = disk.to_vec();
        crypto::cts_decrypt(aes, &self.iv(0), &mut buf)?;
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(n);
        Ok(buf)
    }

    /// SipHash of a (casefolded) name for encrypted, casefolded
    /// directories: (major, minor) as Linux splits it.
    pub fn name_siphash(&self, name: &[u8]) -> Option<(u32, u32)> {
        let k = self.dirhash_key?;
        let h = crypto::siphash24(k, name);
        Some(((h >> 32) as u32, h as u32))
    }
}

// --- no-key names -------------------------------------------------------------

/// Bytes of ciphertext a no-key name carries before switching to a digest.
const NOKEY_BYTES: usize = 149;
/// Unencoded size of a no-key name with a digest.
const NOKEY_MAX: usize = 8 + NOKEY_BYTES + 32;

/// The name Linux shows for an encrypted name when the key is missing:
/// base64url of `{dirhash[2], ciphertext}`, where ciphertext beyond 149
/// bytes is replaced by its SHA-256. `hash`/`minor` are the htree hash of
/// the entry (0 for linear directories).
pub fn nokey_name(hash: u32, minor: u32, disk: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(NOKEY_MAX);
    raw.extend_from_slice(&hash.to_le_bytes());
    raw.extend_from_slice(&minor.to_le_bytes());
    if disk.len() <= NOKEY_BYTES {
        raw.extend_from_slice(disk);
    } else {
        raw.extend_from_slice(&disk[..NOKEY_BYTES]);
        raw.extend_from_slice(&Sha256::digest(&disk[NOKEY_BYTES..]));
    }
    crypto::base64::encode_url(&raw)
}

/// What a no-key name identifies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoKeyName {
    /// The complete ciphertext name.
    Full(Vec<u8>),
    /// A long name: its htree hash, first 149 bytes and digest of the rest.
    Long {
        hash: u32,
        minor: u32,
        prefix: Vec<u8>,
        digest: [u8; 32],
    },
}

impl NoKeyName {
    /// Parse a no-key name typed by the user; `None` if it cannot name
    /// any entry (ENOENT).
    pub fn parse(name: &[u8]) -> Option<NoKeyName> {
        if name.len() > crypto::base64::encoded_len(NOKEY_MAX) {
            return None;
        }
        let raw = crypto::base64::decode_url(name)?;
        if raw.len() < 9 || (raw.len() > 8 + NOKEY_BYTES && raw.len() != NOKEY_MAX) {
            return None;
        }
        if raw.len() == NOKEY_MAX {
            return Some(NoKeyName::Long {
                hash: u32::from_le_bytes(raw[0..4].try_into().unwrap()),
                minor: u32::from_le_bytes(raw[4..8].try_into().unwrap()),
                prefix: raw[8..8 + NOKEY_BYTES].to_vec(),
                digest: raw[8 + NOKEY_BYTES..].try_into().unwrap(),
            });
        }
        Some(NoKeyName::Full(raw[8..].to_vec()))
    }

    /// Whether a directory entry's (ciphertext) name is the one named.
    pub fn matches(&self, disk: &[u8]) -> bool {
        match self {
            NoKeyName::Full(n) => n == disk,
            NoKeyName::Long { prefix, digest, .. } => {
                disk.len() > NOKEY_BYTES
                    && &disk[..NOKEY_BYTES] == prefix.as_slice()
                    && Sha256::digest(&disk[NOKEY_BYTES..])[..] == digest[..]
            }
        }
    }
}

// --- symlinks -----------------------------------------------------------------

/// The ciphertext of an encrypted symlink target (`le16 len || data`).
pub fn symlink_ciphertext(stored: &[u8]) -> Result<&[u8]> {
    if stored.len() < 3 {
        return Err(Error::corrupt("encrypted symlink too short"));
    }
    let n = u16::from_le_bytes([stored[0], stored[1]]) as usize;
    if n == 0 || n + 2 > stored.len() {
        return Err(Error::corrupt("bad encrypted symlink length"));
    }
    Ok(&stored[2..2 + n])
}

/// The stored form of an encrypted symlink target, for a file system
/// with `block_size` blocks: `le16 len || ciphertext` (the inode size).
pub fn encrypt_symlink(ic: &InodeCrypt, target: &[u8], block_size: usize) -> Result<Vec<u8>> {
    // Linux counts a terminating NUL and the length prefix against the
    // block size
    let n = ic.encrypted_len(target.len(), block_size - 3)?;
    let c = ic.encrypt_name_to(target, n)?;
    let mut v = (n as u16).to_le_bytes().to_vec();
    v.extend_from_slice(&c);
    Ok(v)
}

#[cfg(test)]
mod tests;
