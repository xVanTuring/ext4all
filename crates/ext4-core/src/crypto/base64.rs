//! Base64: the URL-safe alphabet without padding (RFC 4648 §5), which
//! Linux uses for no-key names of encrypted files, and the standard
//! alphabet with padding, which LUKS2 headers use.

const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Length of the unpadded encoding of `n` bytes.
pub fn encoded_len(n: usize) -> usize {
    (n * 4).div_ceil(3)
}

fn encode_with(data: &[u8], alphabet: &[u8; 64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded_len(data.len()) + 2);
    let mut acc = 0u32;
    let mut bits = 0;
    for &b in data {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(alphabet[((acc >> bits) & 0x3f) as usize]);
        }
    }
    if bits > 0 {
        out.push(alphabet[((acc << (6 - bits)) & 0x3f) as usize]);
    }
    out
}

/// Decode unpadded input, accepting only the canonical encoding (unused
/// trailing bits zero), so every byte string has exactly one spelling.
fn decode_with(s: &[u8], alphabet: &[u8; 64]) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s {
        let v = alphabet.iter().position(|&a| a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits > 0 && acc & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

/// URL-safe, unpadded.
pub fn encode_url(data: &[u8]) -> Vec<u8> {
    encode_with(data, URL)
}

pub fn decode_url(s: &[u8]) -> Option<Vec<u8>> {
    decode_with(s, URL)
}

/// Standard alphabet, padded with `=`.
pub fn encode_std(data: &[u8]) -> String {
    let mut v = encode_with(data, STD);
    while v.len() % 4 != 0 {
        v.push(b'=');
    }
    String::from_utf8(v).expect("ASCII")
}

/// Standard alphabet; padding optional, whitespace not allowed.
pub fn decode_std(s: &str) -> Option<Vec<u8>> {
    let t = s.trim_end_matches('=');
    if s.len() - t.len() > 2 {
        return None;
    }
    decode_with(t.as_bytes(), STD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        for (p, e) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_url(p.as_bytes()), e.as_bytes());
            assert_eq!(decode_url(e.as_bytes()).unwrap(), p.as_bytes());
        }
        assert_eq!(encode_url(&[0xfb, 0xff]), b"-_8");
        assert_eq!(encode_std(&[0xfb, 0xff]), "+/8=");
        assert_eq!(encode_std(b"fo"), "Zm8=");
        assert_eq!(decode_std("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_std("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(decode_std("+/8=").unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn rejects_noncanonical() {
        assert_eq!(decode_url(b"Zh"), None); // trailing bits set
        assert_eq!(decode_url(b"Z"), None);
        assert_eq!(decode_url(b"Zm9v+A"), None);
        assert_eq!(decode_url(b"Zm9="), None);
        assert_eq!(decode_std("Zm9v-A"), None);
        assert_eq!(decode_std("Zg==="), None);
    }

    #[test]
    fn roundtrip() {
        for n in 0..300 {
            let d: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            let e = encode_url(&d);
            assert_eq!(e.len(), encoded_len(n));
            assert_eq!(decode_url(&e).unwrap(), d);
            assert_eq!(decode_std(&encode_std(&d)).unwrap(), d);
        }
    }
}
