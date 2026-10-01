use super::*;

fn v2_ctx(flags: u8) -> Context {
    Context {
        contents_mode: mode::AES_256_XTS,
        filenames_mode: mode::AES_256_CTS,
        flags,
        log2_data_unit_size: 0,
        key: KeySpec::V2([0x11; 16]),
        nonce: [0x22; 16],
    }
}

#[test]
fn context_roundtrip() {
    let c = v2_ctx(2);
    let raw = c.to_bytes();
    assert_eq!(raw.len(), 40);
    assert_eq!(Context::parse(&raw).unwrap(), c);
    let v1 = Context {
        key: KeySpec::V1([3; 8]),
        ..v2_ctx(0)
    };
    let raw = v1.to_bytes();
    assert_eq!(raw.len(), 28);
    assert_eq!(Context::parse(&raw).unwrap(), v1);
    assert!(Context::parse(&raw[..27]).is_err());
    assert!(Context::parse(&[3; 40]).is_err());
}

#[test]
fn inherit_keeps_policy_and_draws_a_nonce() {
    let c = v2_ctx(3);
    let n = c.inherit().unwrap();
    assert!(n.same_policy(&c));
    assert_ne!(n.nonce, c.nonce);
    assert_eq!(n.padding(), 32);
}

#[test]
fn key_identifiers() {
    let mut kr = Keyring::default();
    let raw = [0x42u8; 64];
    let ids = kr.add(&raw).unwrap();
    // v1 descriptor: SHA-512(SHA-512(key))[..8]
    let h = Sha512::digest(Sha512::digest(raw));
    assert_eq!(ids.descriptor, h[..8]);
    // v2 identifier: HKDF-SHA512, info "fscrypt\0\x01" (independent check)
    let hk = Hkdf::<Sha512>::new(None, &raw);
    let mut id = [0u8; 16];
    hk.expand(b"fscrypt\0\x01", &mut id).unwrap();
    assert_eq!(ids.identifier, id);
    assert!(kr.get(&KeySpec::V2(id)).is_some());
    assert!(kr.get(&KeySpec::V1(ids.descriptor)).is_some());
    assert!(kr.add(&[0u8; 8]).is_err());
}

fn derive(ctx: &Context, raw: &[u8], ino: u32, is_reg: bool) -> InodeCrypt {
    let mk = MasterKey::new(raw);
    InodeCrypt::derive(ctx, &mk, ino, &[9; 16], is_reg, 12)
        .unwrap()
        .unwrap()
}

#[test]
fn name_lengths_follow_padding() {
    for (pad_flag, pad) in [(0u8, 4usize), (1, 8), (2, 16), (3, 32)] {
        let ic = derive(&v2_ctx(pad_flag), &[1; 64], 12, false);
        for len in 1..=255usize {
            let n = ic.encrypted_len(len, 255).unwrap();
            assert!(n >= len && (16..=255).contains(&n));
            assert!(n % pad == 0 || n == 255, "pad {pad} len {len} -> {n}");
        }
        assert!(ic.encrypted_len(256, 255).is_err());
    }
}

#[test]
fn names_roundtrip() {
    for ctx in [
        v2_ctx(0),
        v2_ctx(3),
        Context {
            key: KeySpec::V1([7; 8]),
            ..v2_ctx(2)
        },
        Context {
            filenames_mode: mode::AES_128_CTS,
            contents_mode: mode::AES_128_CBC,
            ..v2_ctx(0)
        },
        v2_ctx(flag::IV_INO_LBLK_64),
        v2_ctx(flag::IV_INO_LBLK_32),
    ] {
        let ic = derive(&ctx, &[5; 64], 4242, false);
        for name in [&b"a"[..], b"hello.txt", "中文名字.md".as_bytes(), &[b'x'; 255][..]] {
            let c = ic.encrypt_name(name).unwrap();
            assert_ne!(&c[..name.len().min(c.len())], name);
            assert_eq!(ic.decrypt_name(&c).unwrap(), name);
        }
    }
}

#[test]
fn ino_lblk_flags_change_ivs() {
    let a = derive(&v2_ctx(flag::IV_INO_LBLK_64), &[5; 64], 100, true);
    let b = derive(&v2_ctx(flag::IV_INO_LBLK_64), &[5; 64], 101, true);
    let mut x = vec![0u8; 4096];
    let mut y = vec![0u8; 4096];
    a.crypt_data(3, &mut x, true).unwrap();
    b.crypt_data(3, &mut y, true).unwrap();
    // same per-mode key, different inode number in the IV
    assert_ne!(x, y);
    b.crypt_data(3, &mut y, false).unwrap();
    assert!(y.iter().all(|&v| v == 0));
}

#[test]
fn contents_roundtrip_and_unit_index() {
    let ic = derive(&v2_ctx(0), &[8; 64], 12, true);
    let p: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
    let mut c = p.clone();
    ic.crypt_data(10, &mut c, true).unwrap();
    // each data unit is independent: unit 11 alone decrypts the same
    let mut second = c[4096..].to_vec();
    ic.crypt_data(11, &mut second, false).unwrap();
    assert_eq!(second, p[4096..]);
    ic.crypt_data(10, &mut c, false).unwrap();
    assert_eq!(c, p);
    assert!(ic.crypt_data(0, &mut [0u8; 100], true).is_err());
}

#[test]
fn v1_and_v2_derive_different_keys() {
    let raw = [6u8; 64];
    let v2 = derive(&v2_ctx(0), &raw, 12, false);
    let v1 = derive(
        &Context {
            key: KeySpec::V1([7; 8]),
            ..v2_ctx(0)
        },
        &raw,
        12,
        false,
    );
    assert_ne!(v1.encrypt_name(b"x").unwrap(), v2.encrypt_name(b"x").unwrap());
}

#[test]
fn unsupported_policies_yield_none() {
    let mk = MasterKey::new(&[1; 64]);
    for ctx in [
        Context {
            contents_mode: mode::ADIANTUM,
            filenames_mode: mode::ADIANTUM,
            ..v2_ctx(0)
        },
        Context {
            filenames_mode: mode::AES_256_HCTR2,
            ..v2_ctx(0)
        },
        v2_ctx(flag::DIRECT_KEY),
    ] {
        let reg = InodeCrypt::derive(&ctx, &mk, 1, &[0; 16], true, 12).unwrap();
        let dir = InodeCrypt::derive(&ctx, &mk, 1, &[0; 16], false, 12).unwrap();
        assert!(reg.is_none() || dir.is_none(), "{ctx:?}");
    }
    // a v1 key too short for AES-256-XTS
    let short = MasterKey::new(&[1; 32]);
    let v1 = Context {
        key: KeySpec::V1([0; 8]),
        ..v2_ctx(0)
    };
    assert!(
        InodeCrypt::derive(&v1, &short, 1, &[0; 16], true, 12)
            .unwrap()
            .is_none()
    );
}

#[test]
fn nokey_names() {
    let short: Vec<u8> = (0..32).collect();
    let n = nokey_name(0x12345678, 0x9abcdef0, &short);
    assert!(n.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_'));
    assert_eq!(NoKeyName::parse(&n), Some(NoKeyName::Full(short.clone())));
    let long: Vec<u8> = (0..255).map(|i| i as u8).collect();
    let n = nokey_name(10, 20, &long);
    assert_eq!(n.len(), 252);
    match NoKeyName::parse(&n).unwrap() {
        nk @ NoKeyName::Long {
            hash: 10, minor: 20, ..
        } => {
            assert!(nk.matches(&long));
            let mut other = long.clone();
            other[200] ^= 1;
            assert!(!nk.matches(&other));
            assert!(!nk.matches(&long[..150]));
        }
        other => panic!("{other:?}"),
    }
    // exactly 149 bytes still fits without a digest
    let edge = vec![1u8; 149];
    assert_eq!(NoKeyName::parse(&nokey_name(0, 0, &edge)), Some(NoKeyName::Full(edge)));
    assert_eq!(NoKeyName::parse(b"not base64!"), None);
    assert_eq!(NoKeyName::parse(b"AAAAAAAAAA"), None); // only the hash
    assert_eq!(NoKeyName::parse(&[b'A'; 253]), None);
}

#[test]
fn symlink_targets() {
    let ic = derive(&v2_ctx(0), &[3; 64], 12, false);
    let stored = encrypt_symlink(&ic, b"../target/file", 4096).unwrap();
    assert_eq!(u16::from_le_bytes([stored[0], stored[1]]) as usize, stored.len() - 2);
    let c = symlink_ciphertext(&stored).unwrap();
    assert_eq!(ic.decrypt_name(c).unwrap(), b"../target/file");
    // the longest target that fits a 4 KiB block
    assert!(encrypt_symlink(&ic, &[b'a'; 4093], 4096).is_ok());
    assert!(encrypt_symlink(&ic, &[b'a'; 4094], 4096).is_err());
    assert!(symlink_ciphertext(&[0, 0, 1]).is_err());
    assert!(symlink_ciphertext(&[9, 0, 1, 2]).is_err());
}

#[test]
fn siphash_keys_for_casefolded_dirs() {
    let ic = derive(&v2_ctx(0), &[3; 64], 12, false);
    let a = ic.name_siphash(b"abc").unwrap();
    assert_ne!(a, ic.name_siphash(b"abd").unwrap());
    let reg = derive(&v2_ctx(0), &[3; 64], 12, true);
    assert!(reg.name_siphash(b"abc").is_none());
}
