//! On-disk structures. Each type wraps its raw bytes and exposes typed
//! accessors, so fields this crate does not know about round-trip unchanged.

pub mod dirent;
pub mod extent;
pub mod group;
pub mod inode;
pub mod superblock;
pub mod xattr;

/// Generates getter/setter pairs for little endian fields in `self.raw`.
macro_rules! le_fields {
    ($($get:ident, $set:ident: $ty:ident @ $off:expr;)*) => {
        $(
            #[inline]
            pub fn $get(&self) -> $ty {
                le_fields!(@get $ty, &self.raw[..], $off)
            }
            #[inline]
            pub fn $set(&mut self, v: $ty) {
                le_fields!(@set $ty, &mut self.raw[..], $off, v)
            }
        )*
    };
    (@get u8, $b:expr, $off:expr) => { $b[$off] };
    (@get u16, $b:expr, $off:expr) => { $crate::bytes::le16($b, $off) };
    (@get u32, $b:expr, $off:expr) => { $crate::bytes::le32($b, $off) };
    (@get u64, $b:expr, $off:expr) => { $crate::bytes::le64($b, $off) };
    (@set u8, $b:expr, $off:expr, $v:expr) => { $b[$off] = $v };
    (@set u16, $b:expr, $off:expr, $v:expr) => { $crate::bytes::set_le16($b, $off, $v) };
    (@set u32, $b:expr, $off:expr, $v:expr) => { $crate::bytes::set_le32($b, $off, $v) };
    (@set u64, $b:expr, $off:expr, $v:expr) => { $crate::bytes::set_le64($b, $off, $v) };
}

pub(crate) use le_fields;
