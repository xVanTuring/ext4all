//! Mapping between macOS extended attribute names and ext4 names.
//!
//! macOS has a flat namespace (`com.apple.FinderInfo`, `foo`), Linux has
//! namespaces (`user.`, `trusted.`, `security.`, `system.`). A macOS name
//! `N` is stored as `user.N`; only the `user.` namespace is visible from
//! macOS, with the prefix stripped. Other namespaces stay hidden so that
//! SELinux labels, ACLs and similar Linux metadata survive untouched.

const USER: &[u8] = b"user.";

/// ext4 name for a macOS attribute name.
pub fn to_ext4(mac: &[u8]) -> Vec<u8> {
    let mut v = USER.to_vec();
    v.extend_from_slice(mac);
    v
}

/// macOS name for an ext4 attribute name, if it is visible.
pub fn from_ext4(ext: &[u8]) -> Option<&[u8]> {
    ext.strip_prefix(USER).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for n in [&b"com.apple.FinderInfo"[..], b"foo", b"user.nested", "中文".as_bytes()] {
            let e = to_ext4(n);
            assert!(e.starts_with(b"user."));
            assert_eq!(from_ext4(&e), Some(n));
        }
    }

    #[test]
    fn hides_other_namespaces() {
        assert_eq!(from_ext4(b"security.selinux"), None);
        assert_eq!(from_ext4(b"trusted.overlay.opaque"), None);
        assert_eq!(from_ext4(b"system.posix_acl_access"), None);
        assert_eq!(from_ext4(b"user."), None);
        assert_eq!(from_ext4(b"user.x"), Some(&b"x"[..]));
    }
}
