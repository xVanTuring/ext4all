//! htree directory name hashing (`fs/ext4/hash.c`).

use crate::ondisk::superblock::hash_version as hv;

const DEFAULT_SEED: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
const HTREE_EOF_32BIT: u32 = 0x7fff_ffff;

/// Result of hashing a name: `major` orders the htree, `minor` breaks ties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DxHash {
    pub major: u32,
    pub minor: u32,
}

fn tea_transform(buf: &mut [u32; 4], inp: &[u32]) {
    let mut sum: u32 = 0;
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (inp[0], inp[1], inp[2], inp[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(0x9E3779B9);
        b0 = b0.wrapping_add((b1 << 4).wrapping_add(a) ^ b1.wrapping_add(sum) ^ (b1 >> 5).wrapping_add(b));
        b1 = b1.wrapping_add((b0 << 4).wrapping_add(c) ^ b0.wrapping_add(sum) ^ (b0 >> 5).wrapping_add(d));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

#[inline]
fn f(x: u32, y: u32, z: u32) -> u32 {
    z ^ (x & (y ^ z))
}
#[inline]
fn g(x: u32, y: u32, z: u32) -> u32 {
    (x & y).wrapping_add((x ^ y) & z)
}
#[inline]
fn h(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}

fn half_md4_transform(buf: &mut [u32; 4], inp: &[u32]) {
    const K1: u32 = 0;
    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! round {
        ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x);
            $a = $a.rotate_left($s);
        };
    }
    round!(f, a, b, c, d, inp[0].wrapping_add(K1), 3);
    round!(f, d, a, b, c, inp[1].wrapping_add(K1), 7);
    round!(f, c, d, a, b, inp[2].wrapping_add(K1), 11);
    round!(f, b, c, d, a, inp[3].wrapping_add(K1), 19);
    round!(f, a, b, c, d, inp[4].wrapping_add(K1), 3);
    round!(f, d, a, b, c, inp[5].wrapping_add(K1), 7);
    round!(f, c, d, a, b, inp[6].wrapping_add(K1), 11);
    round!(f, b, c, d, a, inp[7].wrapping_add(K1), 19);

    round!(g, a, b, c, d, inp[1].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[3].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[5].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[7].wrapping_add(K2), 13);
    round!(g, a, b, c, d, inp[0].wrapping_add(K2), 3);
    round!(g, d, a, b, c, inp[2].wrapping_add(K2), 5);
    round!(g, c, d, a, b, inp[4].wrapping_add(K2), 9);
    round!(g, b, c, d, a, inp[6].wrapping_add(K2), 13);

    round!(h, a, b, c, d, inp[3].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[7].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[2].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[6].wrapping_add(K3), 15);
    round!(h, a, b, c, d, inp[1].wrapping_add(K3), 3);
    round!(h, d, a, b, c, inp[5].wrapping_add(K3), 9);
    round!(h, c, d, a, b, inp[0].wrapping_add(K3), 11);
    round!(h, b, c, d, a, inp[4].wrapping_add(K3), 15);

    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

fn char_val(c: u8, signed: bool) -> u32 {
    if signed { c as i8 as i32 as u32 } else { c as u32 }
}

fn dx_hack_hash(name: &[u8], signed: bool) -> u32 {
    let (mut hash0, mut hash1): (u32, u32) = (0x12a3fe2d, 0x37abe8f9);
    for &c in name {
        let mut hash = hash1.wrapping_add(hash0 ^ char_val(c, signed).wrapping_mul(7152373));
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

fn str2hashbuf(msg: &[u8], out: &mut [u32], signed: bool) {
    let mut num = out.len() as isize;
    let len = msg.len() as u32;
    let mut pad = len | (len << 8);
    pad |= pad << 16;
    let mut val = pad;
    let take = msg.len().min(out.len() * 4);
    let mut o = 0;
    for (i, &c) in msg[..take].iter().enumerate() {
        val = char_val(c, signed).wrapping_add(val << 8);
        if i % 4 == 3 {
            out[o] = val;
            o += 1;
            val = pad;
            num -= 1;
        }
    }
    num -= 1;
    if num >= 0 {
        out[o] = val;
        o += 1;
    }
    loop {
        num -= 1;
        if num < 0 {
            break;
        }
        out[o] = pad;
        o += 1;
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: when nonzero, major hashes take only this many distinct
    /// values, forcing long hash collision chains (minor hashes are kept).
    pub(crate) static COLLIDE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Hash `name` with the given (effective) hash version and seed.
/// Returns `None` for unsupported versions (e.g. siphash for casefold).
pub fn dirhash(name: &[u8], version: u8, seed: &[u32; 4]) -> Option<DxHash> {
    let mut buf = if seed.iter().any(|&s| s != 0) {
        *seed
    } else {
        DEFAULT_SEED
    };
    let (major, minor) = match version {
        hv::LEGACY => (dx_hack_hash(name, true), 0),
        hv::LEGACY_UNSIGNED => (dx_hack_hash(name, false), 0),
        hv::HALF_MD4 | hv::HALF_MD4_UNSIGNED => {
            let signed = version == hv::HALF_MD4;
            let mut p = name;
            let mut inp = [0u32; 8];
            while !p.is_empty() {
                str2hashbuf(p, &mut inp, signed);
                half_md4_transform(&mut buf, &inp);
                p = &p[p.len().min(32)..];
            }
            (buf[1], buf[2])
        }
        hv::TEA | hv::TEA_UNSIGNED => {
            let signed = version == hv::TEA;
            let mut p = name;
            let mut inp = [0u32; 4];
            while !p.is_empty() {
                str2hashbuf(p, &mut inp, signed);
                tea_transform(&mut buf, &inp);
                p = &p[p.len().min(16)..];
            }
            (buf[0], buf[1])
        }
        _ => return None,
    };
    #[cfg(test)]
    let major = match COLLIDE.with(|c| c.get()) {
        0 => major,
        n => (major % n).wrapping_mul(0x9E37_79B9),
    };
    let mut major = major & !1;
    if major == HTREE_EOF_32BIT << 1 {
        major = (HTREE_EOF_32BIT - 1) << 1;
    }
    Some(DxHash { major, minor })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: [u32; 4] = [0; 4];

    #[test]
    fn low_bit_always_clear() {
        for v in [0u8, 1, 2, 3, 4, 5] {
            for name in [
                &b"a"[..],
                b"hello",
                b"some-much-longer-file-name-with-more-than-32-bytes.txt",
            ] {
                let h = dirhash(name, v, &ZERO).unwrap();
                assert_eq!(h.major & 1, 0);
            }
        }
    }

    #[test]
    fn unsupported_version() {
        assert!(dirhash(b"x", 6, &ZERO).is_none());
        assert!(dirhash(b"x", 99, &ZERO).is_none());
    }

    #[test]
    fn signed_and_unsigned_agree_for_ascii() {
        let s = dirhash(b"plain-ascii", hv::HALF_MD4, &ZERO).unwrap();
        let u = dirhash(b"plain-ascii", hv::HALF_MD4_UNSIGNED, &ZERO).unwrap();
        assert_eq!(s, u);
        let s = dirhash(b"plain-ascii", hv::TEA, &ZERO).unwrap();
        let u = dirhash(b"plain-ascii", hv::TEA_UNSIGNED, &ZERO).unwrap();
        assert_eq!(s, u);
        let s = dirhash(b"plain", hv::LEGACY, &ZERO).unwrap();
        let u = dirhash(b"plain", hv::LEGACY_UNSIGNED, &ZERO).unwrap();
        assert_eq!(s, u);
    }

    #[test]
    fn signed_and_unsigned_differ_for_high_bytes() {
        let name = "中文文件名".as_bytes();
        let s = dirhash(name, hv::HALF_MD4, &ZERO).unwrap();
        let u = dirhash(name, hv::HALF_MD4_UNSIGNED, &ZERO).unwrap();
        assert_ne!(s, u);
    }

    #[test]
    fn zero_seed_means_default() {
        let a = dirhash(b"abc", hv::HALF_MD4, &ZERO).unwrap();
        let b = dirhash(b"abc", hv::HALF_MD4, &DEFAULT_SEED).unwrap();
        assert_eq!(a, b);
        let c = dirhash(b"abc", hv::HALF_MD4, &[1, 2, 3, 4]).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn legacy_hash_reference() {
        // Computed by hand from the reference algorithm for "a":
        // hash = 0x37abe8f9 + (0x12a3fe2d ^ (97 * 7152373))
        let x = 0x12a3fe2du32 ^ 97u32.wrapping_mul(7152373);
        let mut hsh = 0x37abe8f9u32.wrapping_add(x);
        if hsh & 0x8000_0000 != 0 {
            hsh = hsh.wrapping_sub(0x7fff_ffff);
        }
        assert_eq!(dirhash(b"a", hv::LEGACY, &ZERO).unwrap().major, (hsh << 1) & !1);
    }

    #[test]
    fn str2hashbuf_padding() {
        let mut out = [0u32; 4];
        str2hashbuf(b"ab", &mut out, false);
        let pad = 2u32 | (2 << 8) | (2 << 16) | (2 << 24);
        // val = 'b' + ('a' + pad<<8)<<8
        let v = (b'b' as u32).wrapping_add(((b'a' as u32).wrapping_add(pad << 8)) << 8);
        assert_eq!(out, [v, pad, pad, pad]);
        let mut out = [0u32; 4];
        str2hashbuf(b"abcd", &mut out, false);
        let pad = 4u32 * 0x01010101;
        assert_eq!(out[0], 0x61626364);
        assert_eq!(&out[1..], &[pad, pad, pad]);
    }

    #[test]
    fn long_names_use_multiple_rounds() {
        let a = vec![b'x'; 200];
        let mut b = a.clone();
        b[199] = b'y';
        for v in [hv::HALF_MD4, hv::TEA] {
            assert_ne!(dirhash(&a, v, &ZERO), dirhash(&b, v, &ZERO));
        }
    }
}
