//! `ext4-tool [KEY OPTIONS] IMAGE COMMAND [ARGS...]` — inspect and modify
//! ext4 images, including LUKS volumes and fscrypt-encrypted directories.

use ext4_core::{BlockDevice, Error, FileDevice, FileType, Fs, Ino, MountOptions, RenameFlags, Result, XattrSetMode};
use std::io::Write;
use std::sync::Arc;

const USAGE: &str = "\
usage: ext4-tool [KEY OPTIONS] IMAGE COMMAND [ARGS...]

key options (secrets given on the command line are visible to other processes):
  --key HEX                 add an fscrypt master key (repeatable)
  --key-file FILE           add an fscrypt master key read from FILE
  --passphrase TEXT         unlock with the protectors of the Linux fscrypt tool
  --luks-passphrase TEXT    open a LUKS volume
  --luks-key HEX            open a LUKS volume with its volume key

encryption commands:
  crypt-status PATH         encryption policy of a file or directory
  encrypt PATH [v1]         encrypt an empty directory with the first --key
                            (v2 policy, AES-256-XTS / AES-256-CTS)
  luks-dump                 LUKS header summary (no key needed)

read-only commands:
  info                      superblock summary and features
  ls [-l] PATH              list a directory
  tree [PATH]               recursive listing
  cat PATH                  print file contents
  stat PATH                 print inode attributes
  get PATH HOSTFILE         copy a file out of the image
  readlink PATH             print a symlink target
  xattr-list PATH           list extended attributes
  xattr-get PATH NAME       print an extended attribute

modifying commands:
  put HOSTFILE PATH         copy a host file into the image
  mkdir PATH                create a directory
  rm PATH                   remove a file
  rmdir PATH                remove an empty directory
  mv SRC DST                rename
  ln TARGET PATH            create a hard link
  symlink TARGET PATH       create a symbolic link
  truncate PATH SIZE        change a file's size
  chmod MODE PATH           change permission bits (octal)
  xattr-set PATH NAME VALUE set an extended attribute
  xattr-rm PATH NAME        remove an extended attribute
  label NAME                change the volume label
  recover                   replay the journal and process orphans

creating:
  mkfs [OPTIONS]            new ext4 file system filling IMAGE (an existing
                            file); mke2fs options -L -b -i -N -m -U -J size=
                            -O ^has_journal -E root_owner=UID:GID
";

fn split(path: &str) -> Result<(&str, &str)> {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(i) => Ok((if i == 0 { "/" } else { &p[..i] }, &p[i + 1..])),
        None => Ok(("/", p)),
    }
}

fn type_char(ft: FileType) -> char {
    match ft {
        FileType::Directory => 'd',
        FileType::Symlink => 'l',
        FileType::CharDev => 'c',
        FileType::BlockDev => 'b',
        FileType::Fifo => 'p',
        FileType::Socket => 's',
        _ => '-',
    }
}

fn perm_string(mode: u16) -> String {
    let mut s = String::new();
    for (i, c) in "rwxrwxrwx".chars().enumerate() {
        s.push(if mode & (0o400 >> i) != 0 { c } else { '-' });
    }
    s
}

fn ls(fs: &mut Fs, out: &mut dyn Write, path: &str, long: bool) -> Result<()> {
    let dir = fs.resolve(path)?;
    let mut entries = fs.list_dir(dir)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let name = String::from_utf8_lossy(&e.name);
        if long {
            let a = fs.stat(e.ino)?;
            writeln!(
                out,
                "{}{} {:>3} {:>5} {:>5} {:>12} {:>8} {}",
                type_char(a.file_type),
                perm_string(a.perm),
                a.nlink,
                a.uid,
                a.gid,
                a.size,
                e.ino,
                name
            )?;
        } else {
            writeln!(out, "{name}")?;
        }
    }
    Ok(())
}

fn tree(fs: &mut Fs, out: &mut dyn Write, dir: Ino, prefix: &str, depth: usize) -> Result<()> {
    if depth > 64 {
        return Ok(());
    }
    let mut entries = fs.list_dir(dir)?;
    entries.retain(|e| e.name != b"." && e.name != b"..");
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for e in entries {
        let name = String::from_utf8_lossy(&e.name);
        writeln!(
            out,
            "{prefix}{name}{}",
            if e.file_type == FileType::Directory { "/" } else { "" }
        )?;
        if e.file_type == FileType::Directory {
            tree(fs, out, e.ino, &format!("{prefix}  "), depth + 1)?;
        }
    }
    Ok(())
}

fn read_all(fs: &mut Fs, ino: Ino) -> Result<Vec<u8>> {
    let size = fs.stat(ino)?.size as usize;
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = fs.read(ino, done as u64, &mut buf[done..])?;
        if n == 0 {
            break;
        }
        done += n;
    }
    buf.truncate(done);
    Ok(buf)
}

fn info(fs: &Fs, out: &mut dyn Write) -> Result<()> {
    let sb = fs.superblock();
    let u = sb.uuid();
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(out, "label:        {}", sb.volume_name())?;
    writeln!(
        out,
        "uuid:         {}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )?;
    writeln!(out, "block size:   {}", sb.block_size())?;
    writeln!(out, "blocks:       {}", sb.blocks_count())?;
    writeln!(out, "inodes:       {}", sb.inodes_count())?;
    writeln!(out, "inode size:   {}", sb.inode_size())?;
    writeln!(out, "groups:       {}", sb.group_count())?;
    let s = fs.statfs();
    writeln!(out, "free blocks:  {}", s.free_blocks)?;
    writeln!(out, "free inodes:  {}", s.free_files)?;
    writeln!(out, "features:     {}", ext4_core::features::describe(sb).join(" "))?;
    writeln!(
        out,
        "mode:         {}",
        if fs.is_read_only() { "read-only" } else { "read-write" }
    )?;
    let r = fs.mount_report();
    if r.journal_replayed {
        writeln!(out, "journal:      replayed {} transactions", r.replayed_transactions)?;
    }
    for reason in &r.read_only_reasons {
        writeln!(out, "read-only because: {reason}")?;
    }
    Ok(())
}

const MODIFYING: &[&str] = &[
    "put",
    "mkdir",
    "rm",
    "rmdir",
    "mv",
    "ln",
    "symlink",
    "truncate",
    "chmod",
    "xattr-set",
    "xattr-rm",
    "label",
    "recover",
    "encrypt",
];

/// Keys from the command line.
#[derive(Default)]
struct Keys {
    fscrypt: Vec<Vec<u8>>,
    passphrases: Vec<Vec<u8>>,
    luks_passphrase: Option<Vec<u8>>,
    luks_key: Option<Vec<u8>>,
}

fn hex_arg(s: &str) -> Result<Vec<u8>> {
    ext4_core::crypto::from_hex(s).ok_or_else(|| Error::invalid(format!("not hexadecimal: {s}")))
}

/// Split leading key options off the arguments.
fn parse_keys(args: &[String]) -> Result<(Keys, &[String])> {
    let mut k = Keys::default();
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        let val = args
            .get(i + 1)
            .ok_or_else(|| Error::invalid(format!("{} needs a value", args[i])))?;
        match args[i].as_str() {
            "--key" => k.fscrypt.push(hex_arg(val)?),
            "--key-file" => k.fscrypt.push(std::fs::read(val)?),
            "--passphrase" => k.passphrases.push(val.as_bytes().to_vec()),
            "--luks-passphrase" => k.luks_passphrase = Some(val.as_bytes().to_vec()),
            "--luks-key" => k.luks_key = Some(hex_arg(val)?),
            o => return Err(Error::invalid(format!("unknown option {o}\n{USAGE}"))),
        }
        i += 2;
    }
    Ok((k, &args[i..]))
}

/// The device holding the file system: the image itself, or the opened
/// data area of a LUKS volume.
fn open_device(image: &str, rw: bool, keys: &Keys) -> Result<Arc<dyn BlockDevice>> {
    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open(image, !rw)?);
    let Some(h) = ext4_core::luks::Header::read(&*dev)? else {
        return Ok(dev);
    };
    let key = match (&keys.luks_key, &keys.luks_passphrase) {
        (Some(k), _) => ext4_core::crypto::Secret::new(k.clone()),
        (None, Some(p)) => h
            .unlock(&*dev, p)?
            .ok_or_else(|| Error::invalid("no LUKS key slot opens with this passphrase"))?,
        (None, None) => {
            return Err(Error::invalid(format!(
                "{image} is a LUKS volume: give --luks-passphrase or --luks-key"
            )));
        }
    };
    Ok(Arc::new(h.open(dev, &key)?))
}

fn run(args: &[String], out: &mut dyn Write) -> Result<()> {
    let (keys, args) = parse_keys(args)?;
    if args.len() < 2 {
        return Err(Error::invalid(USAGE));
    }
    let image = &args[0];
    let cmd = args[1].as_str();
    let rest = &args[2..];
    let need = |n: usize| -> Result<()> {
        if rest.len() < n {
            Err(Error::invalid(format!("{cmd}: missing arguments\n{USAGE}")))
        } else {
            Ok(())
        }
    };
    if cmd == "mkfs" {
        let opts = ext4_core::mkfs::parse_args(rest, (0, 0))?;
        let dev = FileDevice::open(image, false)?;
        let s = ext4_core::format(&dev, &opts, &mut |_, _| {})?;
        writeln!(
            out,
            "{} blocks of {} bytes, {} inodes, {} groups, journal of {} blocks",
            s.blocks, s.block_size, s.inodes, s.groups, s.journal_blocks
        )?;
        return Ok(());
    }
    if cmd == "luks-dump" {
        let dev = FileDevice::open(image, true)?;
        match ext4_core::luks::Header::read(&dev)? {
            Some(h) => {
                writeln!(out, "uuid:  {}", h.uuid)?;
                writeln!(out, "label: {}", h.label)?;
                writeln!(out, "{}", h.describe())?;
                if let Err(e) = h.check_supported() {
                    writeln!(out, "not supported: {e}")?;
                }
            }
            None => writeln!(out, "not a LUKS volume")?,
        }
        return Ok(());
    }
    let rw = MODIFYING.contains(&cmd);
    let dev = open_device(image, rw, &keys)?;
    let mut fs = Fs::mount(
        dev,
        MountOptions {
            read_only: !rw,
            ..Default::default()
        },
    )?;
    if rw && fs.is_read_only() {
        return Err(Error::ReadOnly);
    }
    let mut first_key = None;
    for k in &keys.fscrypt {
        let ids = fs.add_encryption_key(k)?;
        first_key.get_or_insert(ids);
    }
    for p in &keys.passphrases {
        if fs.unlock_with_protector(p)?.is_empty() {
            return Err(Error::invalid("no fscrypt protector opens with this passphrase"));
        }
    }
    match cmd {
        "crypt-status" => {
            need(1)?;
            let ino = fs.resolve(&rest[0])?;
            match fs.encryption_context(ino)? {
                Some(c) => writeln!(out, "encrypted: {}", c.describe())?,
                None => writeln!(out, "not encrypted")?,
            }
        }
        "encrypt" => {
            need(1)?;
            let ids = first_key.ok_or_else(|| Error::invalid("encrypt needs --key"))?;
            let ino = fs.resolve(&rest[0])?;
            use ext4_core::fscrypt::{Context, KeySpec, mode};
            let key = if rest.get(1).is_some_and(|v| v == "v1") {
                KeySpec::V1(ids.descriptor)
            } else {
                KeySpec::V2(ids.identifier)
            };
            let policy = Context {
                contents_mode: mode::AES_256_XTS,
                filenames_mode: mode::AES_256_CTS,
                flags: 3, // 32-byte name padding, as the fscrypt tool
                log2_data_unit_size: 0,
                key,
                nonce: [0; 16],
            };
            fs.set_encryption_policy(ino, &policy)?;
        }
        "info" => info(&fs, out)?,
        "ls" => {
            let long = rest.first().is_some_and(|a| a == "-l");
            let path = rest.iter().find(|a| !a.starts_with('-')).map_or("/", |s| s.as_str());
            ls(&mut fs, out, path, long)?;
        }
        "tree" => {
            let dir = fs.resolve(rest.first().map_or("/", |s| s.as_str()))?;
            tree(&mut fs, out, dir, "", 0)?;
        }
        "cat" => {
            need(1)?;
            let ino = fs.resolve(&rest[0])?;
            let data = read_all(&mut fs, ino)?;
            out.write_all(&data)?;
        }
        "stat" => {
            need(1)?;
            let ino = fs.resolve(&rest[0])?;
            let a = fs.stat(ino)?;
            writeln!(out, "inode:  {}", a.ino)?;
            writeln!(out, "type:   {:?}", a.file_type)?;
            writeln!(out, "mode:   {:o}", a.mode())?;
            writeln!(out, "links:  {}", a.nlink)?;
            writeln!(out, "uid:    {}", a.uid)?;
            writeln!(out, "gid:    {}", a.gid)?;
            writeln!(out, "size:   {}", a.size)?;
            writeln!(out, "alloc:  {}", a.allocated)?;
            writeln!(out, "flags:  {:#x}", a.flags)?;
            writeln!(out, "mtime:  {}.{:09}", a.mtime.sec, a.mtime.nsec)?;
            writeln!(out, "ctime:  {}.{:09}", a.ctime.sec, a.ctime.nsec)?;
            writeln!(out, "atime:  {}.{:09}", a.atime.sec, a.atime.nsec)?;
            if let Some(c) = a.crtime {
                writeln!(out, "crtime: {}.{:09}", c.sec, c.nsec)?;
            }
            if let Ok(exts) = fs.file_extents(ino) {
                for e in exts {
                    writeln!(
                        out,
                        "extent: {}..{} -> {}{}",
                        e.block,
                        e.end(),
                        e.start,
                        if e.unwritten { " (unwritten)" } else { "" }
                    )?;
                }
            }
        }
        "get" => {
            need(2)?;
            let ino = fs.resolve(&rest[0])?;
            let data = read_all(&mut fs, ino)?;
            std::fs::write(&rest[1], data)?;
        }
        "readlink" => {
            need(1)?;
            let ino = fs.resolve(&rest[0])?;
            out.write_all(&fs.read_link(ino)?)?;
            writeln!(out)?;
        }
        "xattr-list" => {
            need(1)?;
            let ino = fs.resolve(&rest[0])?;
            for n in fs.list_xattr(ino)? {
                writeln!(out, "{}", String::from_utf8_lossy(&n))?;
            }
        }
        "xattr-get" => {
            need(2)?;
            let ino = fs.resolve(&rest[0])?;
            out.write_all(&fs.get_xattr(ino, rest[1].as_bytes())?)?;
        }
        "put" => {
            need(2)?;
            let data = std::fs::read(&rest[0])?;
            let (parent, name) = split(&rest[1])?;
            let dir = fs.resolve(parent)?;
            let ino = match fs.lookup(dir, name.as_bytes()) {
                Ok(ino) => {
                    fs.truncate(ino, 0)?;
                    ino
                }
                Err(Error::NotFound) => fs.create(dir, name.as_bytes(), FileType::Regular, 0o644, 0, 0, 0)?.ino,
                Err(e) => return Err(e),
            };
            let mut off = 0;
            for chunk in data.chunks(1 << 20) {
                fs.write(ino, off as u64, chunk)?;
                off += chunk.len();
            }
        }
        "mkdir" => {
            need(1)?;
            let (parent, name) = split(&rest[0])?;
            let dir = fs.resolve(parent)?;
            fs.mkdir(dir, name.as_bytes(), 0o755, 0, 0)?;
        }
        "rm" | "rmdir" => {
            need(1)?;
            let (parent, name) = split(&rest[0])?;
            let dir = fs.resolve(parent)?;
            if cmd == "rm" {
                fs.unlink(dir, name.as_bytes())?;
            } else {
                fs.rmdir(dir, name.as_bytes())?;
            }
        }
        "mv" => {
            need(2)?;
            let (sp, sn) = split(&rest[0])?;
            let (dp, dn) = split(&rest[1])?;
            let s = fs.resolve(sp)?;
            let d = fs.resolve(dp)?;
            fs.rename(s, sn.as_bytes(), d, dn.as_bytes(), RenameFlags::default())?;
        }
        "ln" => {
            need(2)?;
            let target = fs.resolve(&rest[0])?;
            let (parent, name) = split(&rest[1])?;
            let dir = fs.resolve(parent)?;
            fs.link(target, dir, name.as_bytes())?;
        }
        "symlink" => {
            need(2)?;
            let (parent, name) = split(&rest[1])?;
            let dir = fs.resolve(parent)?;
            fs.symlink(dir, name.as_bytes(), rest[0].as_bytes(), 0, 0)?;
        }
        "truncate" => {
            need(2)?;
            let ino = fs.resolve(&rest[0])?;
            let size: u64 = rest[1].parse().map_err(|_| Error::invalid("bad size"))?;
            fs.truncate(ino, size)?;
        }
        "chmod" => {
            need(2)?;
            let mode = u16::from_str_radix(&rest[0], 8).map_err(|_| Error::invalid("bad mode"))?;
            let ino = fs.resolve(&rest[1])?;
            fs.set_attr(
                ino,
                &ext4_core::SetAttr {
                    perm: Some(mode),
                    ..Default::default()
                },
            )?;
        }
        "xattr-set" => {
            need(3)?;
            let ino = fs.resolve(&rest[0])?;
            fs.set_xattr(ino, rest[1].as_bytes(), rest[2].as_bytes(), XattrSetMode::Any)?;
        }
        "xattr-rm" => {
            need(2)?;
            let ino = fs.resolve(&rest[0])?;
            fs.remove_xattr(ino, rest[1].as_bytes())?;
        }
        "label" => {
            need(1)?;
            fs.set_label(&rest[0])?;
        }
        "recover" => {
            let r = fs.mount_report().clone();
            writeln!(
                out,
                "journal replayed: {} ({} transactions, {} blocks), orphans: {}",
                r.journal_replayed, r.replayed_transactions, r.replayed_blocks, r.orphans_processed
            )?;
        }
        _ => return Err(Error::invalid(format!("unknown command {cmd}\n{USAGE}"))),
    }
    fs.unmount()?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if let Err(e) = run(&args, &mut out) {
        let _ = out.flush();
        eprintln!("ext4-tool: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_paths() {
        assert_eq!(split("/a").unwrap(), ("/", "a"));
        assert_eq!(split("/a/b").unwrap(), ("/a", "b"));
        assert_eq!(split("/a/b/").unwrap(), ("/a", "b"));
        assert_eq!(split("c").unwrap(), ("/", "c"));
    }

    #[test]
    fn perms() {
        assert_eq!(perm_string(0o755), "rwxr-xr-x");
        assert_eq!(perm_string(0o600), "rw-------");
        assert_eq!(type_char(FileType::Directory), 'd');
        assert_eq!(type_char(FileType::Regular), '-');
    }
}
