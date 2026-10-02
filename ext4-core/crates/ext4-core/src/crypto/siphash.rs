//! SipHash-2-4 (Linux `siphash()`), used by fscrypt to hash inode numbers
//! (IV_INO_LBLK_32 policies) and the names of encrypted, casefolded
//! directories.

fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4 of `data` with the key `(k0, k1)` (the two little-endian
/// halves of the 16-byte key).
pub fn siphash24(key: [u64; 2], data: &[u8]) -> u64 {
    let mut v = [
        key[0] ^ 0x736f6d6570736575,
        key[1] ^ 0x646f72616e646f6d,
        key[0] ^ 0x6c7967656e657261,
        key[1] ^ 0x7465646279746573,
    ];
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v[3] ^= m;
        round(&mut v);
        round(&mut v);
        v[0] ^= m;
    }
    let mut last = (data.len() as u64 & 0xff) << 56;
    for (i, &b) in rest.iter().enumerate() {
        last |= (b as u64) << (8 * i);
    }
    v[3] ^= last;
    round(&mut v);
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vectors() {
        // key 00..0f; vectors from the SipHash reference implementation
        let k = [
            u64::from_le_bytes([0, 1, 2, 3, 4, 5, 6, 7]),
            u64::from_le_bytes([8, 9, 10, 11, 12, 13, 14, 15]),
        ];
        assert_eq!(siphash24(k, &[]), 0x726fdb47dd0e0e31);
        let msg: Vec<u8> = (0..15).collect();
        assert_eq!(siphash24(k, &msg), 0xa129ca6149be45e5);
        let msg: Vec<u8> = (0..8).collect();
        assert_eq!(siphash24(k, &msg), 0x93f5f5799a932462);
    }
}
