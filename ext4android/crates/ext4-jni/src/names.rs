//! ext4 names are arbitrary bytes; Android document IDs are strings. A path
//! inside a volume is written as its components joined by `/`, each
//! component UTF-8 with `%`, control characters and bytes that are not
//! valid UTF-8 written as `%XX`. The root is the empty string.

use ext4_core::{Error, Result};

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn push_escaped(out: &mut String, b: u8) {
    out.push('%');
    out.push(HEX[(b >> 4) as usize] as char);
    out.push(HEX[(b & 0xF) as usize] as char);
}

/// The string form of one name.
pub fn encode(name: &[u8]) -> String {
    let mut out = String::with_capacity(name.len());
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '%' || c.is_control() {
                let mut buf = [0u8; 4];
                for &b in c.encode_utf8(&mut buf).as_bytes() {
                    push_escaped(&mut out, b);
                }
            } else {
                out.push(c);
            }
        }
        for &b in chunk.invalid() {
            push_escaped(&mut out, b);
        }
    }
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// The bytes of one encoded name.
pub fn decode(s: &str) -> Result<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let v = b
                .get(i + 1..i + 3)
                .and_then(|h| Some(hex(h[0])? << 4 | hex(h[1])?))
                .ok_or_else(|| Error::invalid(format!("bad escape in name {s:?}")))?;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    if out.is_empty() || out == b"." || out == b".." || out.contains(&b'/') || out.contains(&0) {
        return Err(Error::invalid(format!("not a file name: {s:?}")));
    }
    Ok(out)
}

/// Components of an encoded path ("" is the root).
pub fn decode_path(path: &str) -> Result<Vec<Vec<u8>>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    path.split('/').map(decode).collect()
}

/// A name for people: invalid UTF-8 becomes U+FFFD.
pub fn display(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_stay_readable() {
        for n in ["notes.md", "数据 2026", "a b+c(1).txt", "emoji 😀"] {
            assert_eq!(encode(n.as_bytes()), n);
            assert_eq!(decode(n).unwrap(), n.as_bytes());
        }
    }

    #[test]
    fn percent_controls_and_invalid_bytes_are_escaped() {
        assert_eq!(encode(b"100%.txt"), "100%25.txt");
        assert_eq!(encode(b"tab\there"), "tab%09here");
        assert_eq!(encode(b"bad-\xff-\xc3"), "bad-%FF-%C3");
        assert_eq!(encode("é\u{7f}".as_bytes()), "é%7F");
        for raw in [&b"100%.txt"[..], b"tab\there", b"bad-\xff-\xc3", b"\xe4\xb8\xad\xff"] {
            assert_eq!(decode(&encode(raw)).unwrap(), raw);
        }
        assert_eq!(decode("%e4%B8%AD").unwrap(), "中".as_bytes());
    }

    #[test]
    fn decoding_rejects_what_cannot_be_a_name() {
        for s in ["", ".", "..", "%2E%2E", "a%2Fb", "a%00", "%", "%4", "%zz", "x%G0"] {
            assert!(decode(s).is_err(), "{s:?}");
        }
    }

    #[test]
    fn paths() {
        assert!(decode_path("").unwrap().is_empty());
        assert_eq!(
            decode_path("docs/100%25/%FF").unwrap(),
            vec![b"docs".to_vec(), b"100%".to_vec(), vec![0xFF]]
        );
        assert!(decode_path("docs//x").is_err());
        assert!(decode_path("docs/").is_err());
        assert_eq!(display(b"bad-\xff"), "bad-\u{FFFD}");
    }
}
