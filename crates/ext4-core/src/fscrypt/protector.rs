//! Protectors and policies of the Linux `fscrypt` tool
//! (github.com/google/fscrypt), stored on the file system itself in
//! `/.fscrypt/protectors/<descriptor>` and `/.fscrypt/policies/<descriptor>`
//! as protocol buffers.
//!
//! A passphrase protector stretches the passphrase with Argon2id into a
//! wrapping key; a raw-key protector uses a 32-byte key file directly. The
//! wrapping key unwraps the 32-byte protector key, which unwraps the 64-byte
//! policy key: the fscrypt master key the directories are encrypted with.
//! Wrapping is AES-256-CTR plus HMAC-SHA256, with both keys from
//! HKDF-SHA256 of the wrapping key.
//!
//! Login protectors of a Linux system disk usually live on that system's
//! root file system (a `.link` file here points there); only protectors
//! stored on this volume can be used.

use crate::crypto::{Aes, Secret};
use crate::error::{Error, Result};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// `SourceType` of a protector.
pub mod source {
    pub const PAM_PASSPHRASE: u64 = 1;
    pub const CUSTOM_PASSPHRASE: u64 = 2;
    pub const RAW_KEY: u64 = 3;
}

/// Largest Argon2 memory cost accepted from disk (KiB): a hostile volume
/// must not make unlocking allocate unbounded memory.
const MAX_MEMORY_KIB: u64 = 4 << 20;
const MAX_TIME: u64 = 1000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WrappedKey {
    pub iv: Vec<u8>,
    pub encrypted: Vec<u8>,
    pub hmac: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Costs {
    pub time: u64,
    pub memory: u64,
    pub parallelism: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Protector {
    pub descriptor: String,
    pub source: u64,
    pub name: String,
    pub costs: Costs,
    pub salt: Vec<u8>,
    pub uid: i64,
    pub wrapped: WrappedKey,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub key_descriptor: String,
    pub version: u64,
    /// (protector descriptor, wrapped policy key)
    pub wrapped: Vec<(String, WrappedKey)>,
}

// --- protocol buffer decoding ------------------------------------------------

struct Reader<'a> {
    buf: &'a [u8],
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&b, rest) = self.buf.split_first().ok_or_else(|| bad("truncated varint"))?;
            self.buf = rest;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad("varint too long"))
    }

    fn next(&mut self) -> Result<Option<(u64, Value<'a>)>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let (field, wire) = (key >> 3, key & 7);
        let v = match wire {
            0 => Value::Varint(self.varint()?),
            2 => {
                let n = self.varint()? as usize;
                if n > self.buf.len() {
                    return Err(bad("truncated field"));
                }
                let (b, rest) = self.buf.split_at(n);
                self.buf = rest;
                Value::Bytes(b)
            }
            1 => {
                self.skip(8)?;
                Value::Varint(0)
            }
            5 => {
                self.skip(4)?;
                Value::Varint(0)
            }
            _ => return Err(bad("unsupported wire type")),
        };
        Ok(Some((field, v)))
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if n > self.buf.len() {
            return Err(bad("truncated field"));
        }
        self.buf = &self.buf[n..];
        Ok(())
    }
}

fn bad(what: &str) -> Error {
    Error::invalid(format!("fscrypt metadata: {what}"))
}

fn each<'a>(buf: &'a [u8], mut f: impl FnMut(u64, Value<'a>) -> Result<()>) -> Result<()> {
    let mut r = Reader { buf };
    while let Some((field, v)) = r.next()? {
        f(field, v)?;
    }
    Ok(())
}

fn string(b: &[u8]) -> Result<String> {
    String::from_utf8(b.to_vec()).map_err(|_| bad("string is not UTF-8"))
}

fn parse_wrapped(buf: &[u8]) -> Result<WrappedKey> {
    let mut w = WrappedKey::default();
    each(buf, |f, v| {
        match (f, v) {
            (1, Value::Bytes(b)) => w.iv = b.to_vec(),
            (2, Value::Bytes(b)) => w.encrypted = b.to_vec(),
            (3, Value::Bytes(b)) => w.hmac = b.to_vec(),
            _ => {}
        }
        Ok(())
    })?;
    Ok(w)
}

pub fn parse_protector(buf: &[u8]) -> Result<Protector> {
    let mut p = Protector::default();
    each(buf, |f, v| {
        match (f, v) {
            (1, Value::Bytes(b)) => p.descriptor = string(b)?,
            (2, Value::Varint(x)) => p.source = x,
            (3, Value::Bytes(b)) => p.name = string(b)?,
            (4, Value::Bytes(b)) => {
                each(b, |f, v| {
                    if let Value::Varint(x) = v {
                        match f {
                            2 => p.costs.time = x,
                            3 => p.costs.memory = x,
                            4 => p.costs.parallelism = x,
                            _ => {}
                        }
                    }
                    Ok(())
                })?;
            }
            (5, Value::Bytes(b)) => p.salt = b.to_vec(),
            (6, Value::Varint(x)) => p.uid = x as i64,
            (7, Value::Bytes(b)) => p.wrapped = parse_wrapped(b)?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(p)
}

pub fn parse_policy(buf: &[u8]) -> Result<Policy> {
    let mut p = Policy {
        version: 1,
        ..Default::default()
    };
    each(buf, |f, v| {
        match (f, v) {
            (1, Value::Bytes(b)) => p.key_descriptor = string(b)?,
            (2, Value::Bytes(b)) => each(b, |f, v| {
                if let (4, Value::Varint(x)) = (f, v) {
                    p.version = x;
                }
                Ok(())
            })?,
            (3, Value::Bytes(b)) => {
                let mut desc = String::new();
                let mut key = WrappedKey::default();
                each(b, |f, v| {
                    match (f, v) {
                        (1, Value::Bytes(b)) => desc = string(b)?,
                        (2, Value::Bytes(b)) => key = parse_wrapped(b)?,
                        _ => {}
                    }
                    Ok(())
                })?;
                p.wrapped.push((desc, key));
            }
            _ => {}
        }
        Ok(())
    })?;
    Ok(p)
}

// --- key unwrapping ------------------------------------------------------------

/// AES-256-CTR with a 128-bit big-endian counter (Go's `cipher.NewCTR`).
fn aes_ctr(key: &[u8], iv: &[u8], data: &mut [u8]) -> Result<()> {
    let aes = Aes::new(key)?;
    let mut ctr: [u8; 16] = iv.try_into().map_err(|_| bad("IV length"))?;
    for chunk in data.chunks_mut(16) {
        let mut ks = ctr;
        aes.encrypt(&mut ks);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
        for i in (0..16).rev() {
            ctr[i] = ctr[i].wrapping_add(1);
            if ctr[i] != 0 {
                break;
            }
        }
    }
    Ok(())
}

/// Unwrap a key; `None` if the wrapping key is wrong.
pub fn unwrap(wrapping_key: &[u8], w: &WrappedKey) -> Result<Option<Secret>> {
    if wrapping_key.len() != 32 {
        return Err(bad("wrapping key length"));
    }
    let hk = Hkdf::<Sha256>::new(None, wrapping_key);
    let mut both = Secret::zeroed(64);
    hk.expand(&[], &mut both).expect("HKDF length");
    let (enc_key, auth_key) = both.split_at(32);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(auth_key).expect("HMAC key");
    mac.update(&w.iv);
    mac.update(&w.encrypted);
    if mac.verify_slice(&w.hmac).is_err() {
        return Ok(None);
    }
    let mut key = Secret::new(w.encrypted.clone());
    aes_ctr(enc_key, &w.iv, &mut key)?;
    Ok(Some(key))
}

/// The wrapping key of a passphrase protector (Argon2id).
pub fn passphrase_key(passphrase: &[u8], p: &Protector) -> Result<Secret> {
    let c = &p.costs;
    if c.time == 0 || c.time > MAX_TIME || c.memory == 0 || c.memory > MAX_MEMORY_KIB || c.parallelism == 0 {
        return Err(bad("unreasonable hashing costs"));
    }
    // the tool truncates the parallelism to 8 bits
    let lanes = (c.parallelism as u8).max(1) as u32;
    let params = argon2::Params::new(c.memory as u32, c.time as u32, lanes, Some(32))
        .map_err(|e| bad(&format!("hashing costs: {e}")))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = Secret::zeroed(32);
    a.hash_password_into(passphrase, &p.salt, &mut out)
        .map_err(|e| bad(&format!("Argon2: {e}")))?;
    Ok(out)
}

/// The protector key, given the passphrase (passphrase protectors) or the
/// 32-byte key (raw-key protectors). `None` if it does not match.
pub fn protector_key(p: &Protector, secret: &[u8]) -> Result<Option<Secret>> {
    let wrapping = match p.source {
        source::PAM_PASSPHRASE | source::CUSTOM_PASSPHRASE => passphrase_key(secret, p)?,
        source::RAW_KEY if secret.len() == 32 => Secret::new(secret.to_vec()),
        _ => return Ok(None),
    };
    unwrap(&wrapping, &p.wrapped)
}

/// The policy keys (fscrypt master keys) a protector key opens.
pub fn policy_keys(protector: &Protector, key: &[u8], policies: &[Policy]) -> Result<Vec<Secret>> {
    let mut out = Vec::new();
    for pol in policies {
        for (desc, w) in &pol.wrapped {
            if desc == &protector.descriptor
                && let Some(k) = unwrap(key, w)?
            {
                out.push(k);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crypto;
    use hmac::Mac;

    /// Wrap like `crypto.Wrap` of the fscrypt tool (for tests).
    pub(crate) fn wrap(wrapping_key: &[u8], secret: &[u8], iv: [u8; 16]) -> WrappedKey {
        let hk = Hkdf::<Sha256>::new(None, wrapping_key);
        let mut both = [0u8; 64];
        hk.expand(&[], &mut both).unwrap();
        let mut enc = secret.to_vec();
        aes_ctr(&both[..32], &iv, &mut enc).unwrap();
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&both[32..]).unwrap();
        mac.update(&iv);
        mac.update(&enc);
        WrappedKey {
            iv: iv.to_vec(),
            encrypted: enc,
            hmac: mac.finalize().into_bytes().to_vec(),
        }
    }

    fn field_bytes(out: &mut Vec<u8>, field: u64, b: &[u8]) {
        varint(out, field << 3 | 2);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }

    fn field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
        varint(out, field << 3);
        varint(out, v);
    }

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn encode_wrapped(w: &WrappedKey) -> Vec<u8> {
        let mut v = Vec::new();
        field_bytes(&mut v, 1, &w.iv);
        field_bytes(&mut v, 2, &w.encrypted);
        field_bytes(&mut v, 3, &w.hmac);
        v
    }

    pub(crate) fn encode_protector(p: &Protector) -> Vec<u8> {
        let mut v = Vec::new();
        field_bytes(&mut v, 1, p.descriptor.as_bytes());
        field_varint(&mut v, 2, p.source);
        field_bytes(&mut v, 3, p.name.as_bytes());
        let mut c = Vec::new();
        field_varint(&mut c, 2, p.costs.time);
        field_varint(&mut c, 3, p.costs.memory);
        field_varint(&mut c, 4, p.costs.parallelism);
        field_varint(&mut c, 5, 1);
        field_bytes(&mut v, 4, &c);
        field_bytes(&mut v, 5, &p.salt);
        field_varint(&mut v, 6, p.uid as u64);
        field_bytes(&mut v, 7, &encode_wrapped(&p.wrapped));
        v
    }

    pub(crate) fn encode_policy(p: &Policy) -> Vec<u8> {
        let mut v = Vec::new();
        field_bytes(&mut v, 1, p.key_descriptor.as_bytes());
        let mut o = Vec::new();
        field_varint(&mut o, 1, 32);
        field_varint(&mut o, 2, 1);
        field_varint(&mut o, 3, 4);
        field_varint(&mut o, 4, p.version);
        field_bytes(&mut v, 2, &o);
        for (d, w) in &p.wrapped {
            let mut e = Vec::new();
            field_bytes(&mut e, 1, d.as_bytes());
            field_bytes(&mut e, 2, &encode_wrapped(w));
            field_bytes(&mut v, 3, &e);
        }
        v
    }

    #[test]
    fn wrap_unwrap() {
        let wk = [5u8; 32];
        let w = wrap(&wk, &[9u8; 64], [1; 16]);
        assert_eq!(&*unwrap(&wk, &w).unwrap().unwrap(), &[9u8; 64][..]);
        assert!(unwrap(&[6u8; 32], &w).unwrap().is_none());
    }

    #[test]
    fn ctr_counter_carries() {
        // the counter is one 128-bit big-endian number
        let key = [1u8; 32];
        let iv = [0xffu8; 16];
        let mut a = [0u8; 32];
        aes_ctr(&key, &iv, &mut a).unwrap();
        let aes = Aes::new(&key).unwrap();
        let mut b0 = [0xffu8; 16];
        aes.encrypt(&mut b0);
        let mut b1 = [0u8; 16];
        aes.encrypt(&mut b1);
        assert_eq!(&a[..16], &b0);
        assert_eq!(&a[16..], &b1);
    }

    #[test]
    fn passphrase_protector_roundtrip() {
        let mut p = Protector {
            descriptor: "0123456789abcdef".into(),
            source: source::CUSTOM_PASSPHRASE,
            name: "test".into(),
            costs: Costs {
                time: 1,
                memory: 64,
                parallelism: 1,
            },
            salt: vec![7; 16],
            uid: -1,
            wrapped: WrappedKey::default(),
        };
        let wk = passphrase_key(b"secret", &p).unwrap();
        let pk = [3u8; 32];
        p.wrapped = wrap(&wk, &pk, [2; 16]);
        let p2 = parse_protector(&encode_protector(&p)).unwrap();
        assert_eq!(p2, p);
        assert_eq!(&*protector_key(&p2, b"secret").unwrap().unwrap(), &pk[..]);
        assert!(protector_key(&p2, b"wrong").unwrap().is_none());
        let pol = Policy {
            key_descriptor: "fedcba9876543210fedcba9876543210".into(),
            version: 2,
            wrapped: vec![
                ("ffffffffffffffff".into(), wrap(&[0; 32], &[1; 64], [0; 16])),
                (p.descriptor.clone(), wrap(&pk, &[4u8; 64], [3; 16])),
            ],
        };
        let pol2 = parse_policy(&encode_policy(&pol)).unwrap();
        assert_eq!(pol2, pol);
        let keys = policy_keys(&p2, &pk, &[pol2]).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(&*keys[0], &[4u8; 64][..]);
    }

    #[test]
    fn argon2id_reference_vector() {
        // phc-winner-argon2 test vector: Argon2id v19, m=64 MiB, t=2, p=1
        let p = Protector {
            costs: Costs {
                time: 2,
                memory: 65536,
                parallelism: 1,
            },
            salt: b"somesalt".to_vec(),
            ..Default::default()
        };
        let k = passphrase_key(b"password", &p).unwrap();
        assert_eq!(
            crypto::to_hex(&k),
            "09316115d5cf24ed5a15a31a3ba326e5cf32edc24702987c02b6566f61913cf7"
        );
    }

    #[test]
    fn rejects_hostile_costs() {
        let p = Protector {
            costs: Costs {
                time: 1,
                memory: 1 << 40,
                parallelism: 1,
            },
            salt: vec![0; 16],
            ..Default::default()
        };
        assert!(passphrase_key(b"x", &p).is_err());
    }

    #[test]
    fn truncated_metadata_is_an_error() {
        let p = Protector {
            descriptor: "00".into(),
            ..Default::default()
        };
        let enc = encode_protector(&p);
        for n in 1..enc.len() {
            // never panics; some prefixes are valid messages
            let _ = parse_protector(&enc[..n]);
        }
        assert!(parse_protector(&[0x0a, 0x05, b'a']).is_err());
    }
}
