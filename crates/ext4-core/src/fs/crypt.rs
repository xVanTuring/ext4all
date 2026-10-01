//! fscrypt in the file system: per-inode keys, translation between the
//! names users see and the names stored in encrypted directories, and the
//! rules Linux enforces for encrypted directories.

use super::{Fs, Ino};
use crate::XattrSetMode;
use crate::error::{Error, Result};
use crate::fscrypt::{self, Context, InodeCrypt, KeyIds, NoKeyName};
use crate::hash::dirhash;
use crate::ondisk::dirent::{DirEntry, DxRootInfo};
use crate::ondisk::inode::{FileType, Inode, ROOT_INO, flags};
use crate::ondisk::superblock::{compat, incompat};
use crate::ondisk::xattr::INDEX_ENCRYPTION;
use std::sync::Arc;

/// Derived inode keys kept in memory.
const CRYPT_CACHE_MAX: usize = 4096;
/// Largest `fscrypt` tool metadata file read.
const MAX_METADATA_FILE: u64 = 64 * 1024;

/// Encryption state of an inode.
#[derive(Clone, Debug)]
pub(crate) enum Crypt {
    Plain,
    /// Encrypted, but the key is missing or its algorithm unsupported.
    Locked,
    Unlocked(Arc<InodeCrypt>),
}

/// A name to look for in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fname {
    /// The exact on-disk name.
    Disk(Vec<u8>),
    /// A long no-key name: matched by prefix and digest of the ciphertext.
    Long(NoKeyName),
}

impl Fname {
    pub fn plain(n: &[u8]) -> Fname {
        Fname::Disk(n.to_vec())
    }

    pub fn matches(&self, disk: &[u8]) -> bool {
        match self {
            Fname::Disk(n) => n == disk,
            Fname::Long(nk) => nk.matches(disk),
        }
    }

    /// The htree hash a long no-key name carries.
    pub fn hash_hint(&self) -> Option<u32> {
        match self {
            Fname::Long(NoKeyName::Long { hash, .. }) => Some(*hash),
            _ => None,
        }
    }

    pub fn disk(&self) -> Option<&[u8]> {
        match self {
            Fname::Disk(n) => Some(n),
            Fname::Long(_) => None,
        }
    }
}

/// Where the hash in a directory's no-key names comes from (as Linux
/// computes it when listing the directory).
#[derive(Clone, Copy, Debug)]
enum NoKeyHash {
    Zero,
    Dirhash {
        version: u8,
        seed: [u32; 4],
    },
    /// Encrypted and casefolded: stored after the name in each entry.
    InEntry,
}

/// How the names of one directory are shown.
pub(crate) struct NameView {
    crypt: Crypt,
    hash: NoKeyHash,
}

impl NameView {
    pub fn is_plain(&self) -> bool {
        matches!(self.crypt, Crypt::Plain)
    }

    /// The name to show for an entry whose on-disk name is `disk`. `entry`
    /// is the raw directory block and entry (for hashes stored in the
    /// entry); `hash` the entry's htree hash if already known. `None`
    /// hides an entry whose name cannot be decrypted.
    pub fn present(&self, disk: &[u8], entry: Option<(&[u8], &DirEntry)>, hash: Option<(u32, u32)>) -> Option<Vec<u8>> {
        if disk == b"." || disk == b".." {
            return Some(disk.to_vec());
        }
        match &self.crypt {
            Crypt::Plain => Some(disk.to_vec()),
            Crypt::Unlocked(c) => match c.decrypt_name(disk) {
                Ok(n) if !n.is_empty() => Some(n),
                _ => {
                    log::warn!("cannot decrypt a directory entry name ({} bytes)", disk.len());
                    None
                }
            },
            Crypt::Locked => {
                if disk.len() < fscrypt::MIN_NAME_LEN {
                    log::warn!("encrypted name shorter than 16 bytes");
                    return None;
                }
                let (h, m) = match self.hash {
                    NoKeyHash::Zero => (0, 0),
                    NoKeyHash::Dirhash { version, seed } => match hash {
                        Some(hm) => hm,
                        None => dirhash(disk, version, &seed).map_or((0, 0), |d| (d.major, d.minor)),
                    },
                    NoKeyHash::InEntry => entry.and_then(|(b, d)| entry_hash(b, d)).unwrap_or((0, 0)),
                };
                Some(fscrypt::nokey_name(h, m, disk))
            }
        }
    }
}

/// The (hash, minor hash) stored after the name of an entry in an
/// encrypted, casefolded directory.
pub(crate) fn entry_hash(block: &[u8], d: &DirEntry) -> Option<(u32, u32)> {
    let off = d.offset + ((8 + d.name_len + 3) & !3);
    if off + 8 > d.offset + d.rec_len || off + 8 > block.len() {
        return None;
    }
    Some((crate::bytes::le32(block, off), crate::bytes::le32(block, off + 4)))
}

impl Fs {
    // --- keys ------------------------------------------------------------------

    /// Add an fscrypt master key (16 to 64 raw bytes; keys made by the
    /// Linux tools are 64 bytes). Directories using it become readable and
    /// writable. Returns the v1 descriptor and v2 identifier it answers to.
    pub fn add_encryption_key(&mut self, raw: &[u8]) -> Result<KeyIds> {
        let ids = self.keys.add(raw)?;
        self.crypt_cache.clear();
        log::info!(
            "added fscrypt key {} (v1 descriptor {})",
            crate::crypto::to_hex(&ids.identifier),
            crate::crypto::to_hex(&ids.descriptor)
        );
        Ok(ids)
    }

    /// Add a v1 key under an explicit descriptor.
    pub fn add_encryption_key_v1(&mut self, descriptor: [u8; 8], raw: &[u8]) -> Result<()> {
        self.keys.add_v1(descriptor, raw)?;
        self.crypt_cache.clear();
        Ok(())
    }

    /// Whether any fscrypt key was added.
    pub fn has_encryption_keys(&self) -> bool {
        !self.keys.is_empty()
    }

    /// Unlock with the protectors the Linux `fscrypt` tool keeps in
    /// `/.fscrypt` on this volume: `secret` is a passphrase, or the 32-byte
    /// key of a raw-key protector. Adds every policy key it opens and
    /// returns their identifiers (empty: nothing matched).
    pub fn unlock_with_protector(&mut self, secret: &[u8]) -> Result<Vec<KeyIds>> {
        use fscrypt::protector as pr;
        let protectors: Vec<pr::Protector> = self
            .fscrypt_metadata("protectors")?
            .into_iter()
            .filter_map(|(name, data)| match pr::parse_protector(&data) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!("fscrypt protector {name}: {e}");
                    None
                }
            })
            .collect();
        let policies: Vec<pr::Policy> = self
            .fscrypt_metadata("policies")?
            .into_iter()
            .filter_map(|(name, data)| match pr::parse_policy(&data) {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!("fscrypt policy {name}: {e}");
                    None
                }
            })
            .collect();
        let mut added = Vec::new();
        for p in &protectors {
            let Some(pk) = pr::protector_key(p, secret)? else {
                continue;
            };
            log::info!("fscrypt protector {} ({}) unlocked", p.descriptor, p.name);
            for key in pr::policy_keys(p, &pk, &policies)? {
                added.push(self.add_encryption_key(&key)?);
            }
        }
        Ok(added)
    }

    /// Regular files in `/.fscrypt/<sub>`: (name, contents).
    fn fscrypt_metadata(&mut self, sub: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let dir = match self.resolve(&format!("/.fscrypt/{sub}")) {
            Ok(d) => d,
            Err(Error::NotFound) | Err(Error::NotDir) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        if !self.stat(dir)?.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for e in self.list_dir(dir)? {
            if e.file_type != FileType::Regular || e.name.ends_with(b".link") {
                continue;
            }
            let size = self.stat(e.ino)?.size;
            if size > MAX_METADATA_FILE {
                continue;
            }
            let mut buf = vec![0u8; size as usize];
            let n = self.read(e.ino, 0, &mut buf)?;
            buf.truncate(n);
            out.push((String::from_utf8_lossy(&e.name).into_owned(), buf));
        }
        Ok(out)
    }

    // --- contexts and keys of inodes --------------------------------------------

    /// The encryption context of an inode, if it is encrypted.
    pub fn encryption_context(&mut self, ino: Ino) -> Result<Option<Context>> {
        let inode = self.read_live_inode(ino)?;
        self.inode_context(ino, &inode)
    }

    pub(crate) fn inode_context(&mut self, ino: Ino, inode: &Inode) -> Result<Option<Context>> {
        if !inode.has_flag(flags::ENCRYPT) {
            return Ok(None);
        }
        match self.xattr_get(ino, inode, INDEX_ENCRYPTION, fscrypt::XATTR_NAME)? {
            Some(v) => Ok(Some(Context::parse(&v)?)),
            None => Err(Error::corrupt(format!(
                "inode {ino}: encrypted but has no encryption context"
            ))),
        }
    }

    pub(crate) fn crypt(&mut self, ino: Ino, inode: &Inode) -> Result<Crypt> {
        let Some(ctx) = self.inode_context(ino, inode)? else {
            return Ok(Crypt::Plain);
        };
        let is_reg = inode.is_reg();
        if !(is_reg || inode.is_dir() || inode.is_symlink()) {
            return Ok(Crypt::Locked);
        }
        if let Some(c) = self.crypt_cache.get(&ino)
            && c.ctx == ctx
        {
            return Ok(Crypt::Unlocked(c.clone()));
        }
        let Some(mk) = self.keys.get(&ctx.key) else {
            return Ok(Crypt::Locked);
        };
        match InodeCrypt::derive(&ctx, &mk, ino, &self.sb.uuid(), is_reg, self.bs.trailing_zeros())? {
            Some(ic) => {
                if self.crypt_cache.len() >= CRYPT_CACHE_MAX {
                    self.crypt_cache.clear();
                }
                let ic = Arc::new(ic);
                self.crypt_cache.insert(ino, ic.clone());
                Ok(Crypt::Unlocked(ic))
            }
            None => {
                log::info!("inode {ino}: unsupported encryption policy {}", ctx.describe());
                Ok(Crypt::Locked)
            }
        }
    }

    /// Key for an inode's contents: `None` if it is not encrypted,
    /// [`Error::NoKey`] if it is and the key is missing.
    pub(crate) fn file_crypt(&mut self, ino: Ino, inode: &Inode) -> Result<Option<Arc<InodeCrypt>>> {
        match self.crypt(ino, inode)? {
            Crypt::Plain => Ok(None),
            Crypt::Locked => Err(Error::NoKey),
            Crypt::Unlocked(c) => Ok(Some(c)),
        }
    }

    /// Fail with [`Error::NoKey`] unless the inode is plain or unlocked.
    pub(crate) fn require_key(&mut self, ino: Ino, inode: &Inode) -> Result<()> {
        self.file_crypt(ino, inode).map(|_| ())
    }

    // --- names ---------------------------------------------------------------------

    /// What to search for when looking up the user's `name` in `dir`.
    pub(crate) fn lookup_fname(&mut self, dir: Ino, dinode: &Inode, name: &[u8]) -> Result<Fname> {
        if name == b"." || name == b".." || !dinode.has_flag(flags::ENCRYPT) {
            return Ok(Fname::plain(name));
        }
        match self.crypt(dir, dinode)? {
            Crypt::Plain => Ok(Fname::plain(name)),
            Crypt::Unlocked(c) => match c.encrypt_name(name) {
                Ok(n) => Ok(Fname::Disk(n)),
                Err(Error::NameTooLong) => Err(Error::NameTooLong),
                Err(e) => Err(e),
            },
            Crypt::Locked => match NoKeyName::parse(name) {
                Some(NoKeyName::Full(n)) => Ok(Fname::Disk(n)),
                Some(l) => Ok(Fname::Long(l)),
                None => Err(Error::NotFound),
            },
        }
    }

    /// The on-disk name of a new entry `name` in `dir`; an encrypted
    /// directory needs its key.
    pub(crate) fn new_disk_name(&mut self, dir: Ino, dinode: &Inode, name: &[u8]) -> Result<Vec<u8>> {
        if !dinode.has_flag(flags::ENCRYPT) {
            return Ok(name.to_vec());
        }
        match self.crypt(dir, dinode)? {
            Crypt::Plain => Ok(name.to_vec()),
            Crypt::Unlocked(c) => c.encrypt_name(name),
            Crypt::Locked => Err(Error::NoKey),
        }
    }

    /// How to present the entries of directory `dir`.
    pub(crate) fn name_view(&mut self, dir: Ino, dinode: &Inode) -> Result<NameView> {
        let crypt = self.crypt(dir, dinode)?;
        let mut hash = NoKeyHash::Zero;
        if matches!(crypt, Crypt::Locked) {
            let bs = self.bs as u64;
            if dinode.has_flag(flags::CASEFOLD) {
                hash = NoKeyHash::InEntry;
            } else if self.sb.has_compat(compat::DIR_INDEX) {
                // Linux lists a directory through the htree code (and puts
                // hashes into the no-key names) when it is indexed or has
                // exactly one block
                if dinode.has_flag(flags::INDEX) {
                    let root = self.dir_block(dir, dinode, 0)?.1;
                    let info = DxRootInfo::parse(&root);
                    hash = NoKeyHash::Dirhash {
                        version: self.sb.effective_hash_version(info.hash_version),
                        seed: self.sb.hash_seed(),
                    };
                } else if dinode.size() / bs == 1 {
                    hash = NoKeyHash::Dirhash {
                        version: self.sb.effective_hash_version(self.sb.def_hash_version()),
                        seed: self.sb.hash_seed(),
                    };
                }
            }
        }
        Ok(NameView { crypt, hash })
    }

    // --- policy rules ------------------------------------------------------------------

    /// The context a new inode of type `ft` created in `dir` inherits.
    pub(crate) fn inherit_context(&mut self, dir: Ino, dinode: &Inode, ft: FileType) -> Result<Option<Context>> {
        if !dinode.has_flag(flags::ENCRYPT)
            || !matches!(ft, FileType::Regular | FileType::Directory | FileType::Symlink)
        {
            return Ok(None);
        }
        match self.crypt(dir, dinode)? {
            Crypt::Plain => Ok(None),
            Crypt::Locked => Err(Error::NoKey),
            Crypt::Unlocked(c) => Ok(Some(c.ctx.inherit()?)),
        }
    }

    /// Give a new inode its encryption context (before its first write).
    pub(crate) fn apply_context(&mut self, ino: Ino, inode: &mut Inode, ctx: &Context) -> Result<()> {
        inode.set_flag(flags::ENCRYPT, true);
        // never inline: Linux does not store encrypted data inline
        inode.set_flag(flags::INLINE_DATA, false);
        self.xattr_put(
            ino,
            inode,
            INDEX_ENCRYPTION,
            fscrypt::XATTR_NAME,
            &ctx.to_bytes(),
            XattrSetMode::Create,
        )
    }

    /// Linux `fscrypt_has_permitted_context`: an encrypted directory may
    /// only hold files of its own policy (device nodes, FIFOs and sockets
    /// are never encrypted and always allowed). Violations are `EXDEV`.
    pub(crate) fn check_permitted_context(
        &mut self,
        dir: Ino,
        dinode: &Inode,
        child: Ino,
        cinode: &Inode,
    ) -> Result<()> {
        if !dinode.has_flag(flags::ENCRYPT) || !(cinode.is_reg() || cinode.is_dir() || cinode.is_symlink()) {
            return Ok(());
        }
        let p = self.inode_context(dir, dinode)?;
        let c = self.inode_context(child, cinode)?;
        match (p, c) {
            (Some(p), Some(c)) if p.same_policy(&c) => Ok(()),
            _ => Err(Error::CrossDevice),
        }
    }

    /// Encrypt an empty directory with `policy` (its nonce is replaced),
    /// like Linux `FS_IOC_SET_ENCRYPTION_POLICY`. The key must have been
    /// added. Setting the same policy again succeeds; a different one
    /// fails with `EEXIST`.
    pub fn set_encryption_policy(&mut self, dir: Ino, policy: &Context) -> Result<()> {
        self.op(|fs| {
            fs.require_rw()?;
            if !fs.sb.has_incompat(incompat::ENCRYPT) {
                return Err(Error::unsupported("the file system lacks the encrypt feature"));
            }
            if dir == ROOT_INO {
                return Err(Error::NotPermitted);
            }
            let mut inode = fs.read_live_inode(dir)?;
            if !inode.is_dir() {
                return Err(Error::NotDir);
            }
            if let Some(cur) = fs.inode_context(dir, &inode)? {
                return if cur.same_policy(policy) {
                    Ok(())
                } else {
                    Err(Error::Exists)
                };
            }
            if inode.has_flag(flags::CASEFOLD) {
                return Err(Error::unsupported("encrypting a casefolded directory"));
            }
            if !fs.dir_is_empty(dir, &inode)? {
                return Err(Error::NotEmpty);
            }
            let mk = fs.keys.get(&policy.key).ok_or(Error::NoKey)?;
            let uuid = fs.sb.uuid();
            let bits = fs.bs.trailing_zeros();
            for is_reg in [true, false] {
                if InodeCrypt::derive(policy, &mk, dir, &uuid, is_reg, bits)?.is_none() {
                    return Err(Error::unsupported(format!("encryption policy {}", policy.describe())));
                }
            }
            if inode.has_flag(flags::INLINE_DATA) {
                fs.uninline_dir(dir, &mut inode)?;
            }
            let ctx = policy.inherit()?;
            fs.apply_context(dir, &mut inode, &ctx)?;
            inode.set_ctime(crate::ondisk::inode::Timestamp::now());
            fs.write_inode(dir, &inode)?;
            fs.crypt_cache.remove(&dir);
            fs.maybe_commit()
        })
    }
}
