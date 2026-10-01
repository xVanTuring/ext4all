//! dm-crypt sector ciphers used by LUKS: `aes-xts-plain64` (the default
//! since cryptsetup 1.6), `aes-xts-plain`, `aes-cbc-essiv:sha256` (the
//! old LUKS1 default) and `aes-cbc-plain64`/`aes-cbc-plain`.

use crate::crypto::{Cbc, CbcEssiv, Xts};
use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Iv {
    /// 32-bit little-endian sector number
    Plain,
    /// 64-bit little-endian sector number
    Plain64,
}

#[derive(Clone)]
enum Kind {
    Xts(Xts),
    Cbc(Cbc),
    CbcEssiv(CbcEssiv),
}

/// A sector cipher with its key.
#[derive(Clone)]
pub struct SectorCipher {
    kind: Kind,
    iv: Iv,
    pub spec: String,
}

impl std::fmt::Debug for SectorCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SectorCipher({})", self.spec)
    }
}

/// Split a cipher specification ("aes-xts-plain64") into cipher, chain
/// mode and IV mode, accepting LUKS1's separate cipher and mode fields.
fn parse_spec(spec: &str) -> Result<(&str, &str, &str)> {
    let mut it = spec.splitn(3, '-');
    let cipher = it.next().unwrap_or("");
    let mode = it.next().unwrap_or("");
    let iv = it.next().unwrap_or("");
    if cipher.is_empty() || mode.is_empty() || iv.is_empty() {
        return Err(Error::unsupported(format!("cipher specification {spec:?}")));
    }
    Ok((cipher, mode, iv))
}

impl SectorCipher {
    /// Whether `spec` is one this implementation supports, with a key of
    /// `key_len` bytes.
    pub fn check(spec: &str, key_len: usize) -> Result<()> {
        Self::new(spec, &vec![0u8; key_len]).map(|_| ())
    }

    pub fn new(spec: &str, key: &[u8]) -> Result<SectorCipher> {
        let (cipher, mode, iv) = parse_spec(spec)?;
        let unsupported = || Error::unsupported(format!("LUKS cipher {spec} with a {}-bit key", key.len() * 8));
        if cipher != "aes" {
            return Err(unsupported());
        }
        let (kind, iv) = match (mode, iv) {
            ("xts", "plain64") => (Kind::Xts(Xts::new(key).map_err(|_| unsupported())?), Iv::Plain64),
            ("xts", "plain") => (Kind::Xts(Xts::new(key).map_err(|_| unsupported())?), Iv::Plain),
            ("cbc", "plain64") => (Kind::Cbc(Cbc::new(key).map_err(|_| unsupported())?), Iv::Plain64),
            ("cbc", "plain") => (Kind::Cbc(Cbc::new(key).map_err(|_| unsupported())?), Iv::Plain),
            // the ESSIV key is the SHA-256 of the volume key; the IV it
            // encrypts is the 64-bit sector number
            ("cbc", "essiv:sha256") => (
                Kind::CbcEssiv(CbcEssiv::new(key).map_err(|_| unsupported())?),
                Iv::Plain64,
            ),
            _ => return Err(unsupported()),
        };
        Ok(SectorCipher {
            kind,
            iv,
            spec: spec.to_string(),
        })
    }

    /// Encrypt or decrypt `buf`, a whole number of `sector_size` sectors;
    /// the first one has IV sector number `iv_sector`, and each following
    /// sector advances it by `iv_step` (sector size / 512, as dm-crypt
    /// counts IVs in 512-byte units).
    pub fn crypt(&self, iv_sector: u64, iv_step: u64, sector_size: usize, buf: &mut [u8], encrypt: bool) {
        assert!(buf.len() % sector_size == 0);
        for (i, s) in buf.chunks_mut(sector_size).enumerate() {
            let n = iv_sector.wrapping_add(i as u64 * iv_step);
            let mut iv = [0u8; 16];
            match self.iv {
                Iv::Plain64 => iv[..8].copy_from_slice(&n.to_le_bytes()),
                Iv::Plain => iv[..4].copy_from_slice(&(n as u32).to_le_bytes()),
            }
            match (&self.kind, encrypt) {
                (Kind::Xts(x), true) => x.encrypt(&iv, s),
                (Kind::Xts(x), false) => x.decrypt(&iv, s),
                (Kind::Cbc(c), true) => c.encrypt(&iv, s),
                (Kind::Cbc(c), false) => c.decrypt(&iv, s),
                (Kind::CbcEssiv(c), true) => c.encrypt(&iv, s),
                (Kind::CbcEssiv(c), false) => c.decrypt(&iv, s),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        for (s, k) in [
            ("aes-xts-plain64", 64),
            ("aes-xts-plain64", 32),
            ("aes-xts-plain", 64),
            ("aes-cbc-essiv:sha256", 32),
            ("aes-cbc-essiv:sha256", 16),
            ("aes-cbc-plain64", 32),
        ] {
            SectorCipher::check(s, k).unwrap_or_else(|e| panic!("{s} {k}: {e}"));
        }
        for (s, k) in [
            ("serpent-xts-plain64", 64),
            ("aes-xts-plain64", 20),
            ("aes-cbc-benbi", 32),
            ("aes-xts", 64),
            ("aes-cbc-essiv:sha1", 32),
        ] {
            assert!(SectorCipher::check(s, k).is_err(), "{s}");
        }
    }

    #[test]
    fn sectors_are_independent() {
        let c = SectorCipher::new("aes-xts-plain64", &[3u8; 64]).unwrap();
        let p = vec![0x5au8; 4096];
        let mut whole = p.clone();
        c.crypt(100, 1, 512, &mut whole, true);
        // sector 3 alone, with its own IV number
        let mut one = p[1536..2048].to_vec();
        c.crypt(103, 1, 512, &mut one, true);
        assert_eq!(&whole[1536..2048], &one[..]);
        c.crypt(100, 1, 512, &mut whole, false);
        assert_eq!(whole, p);
    }
}
