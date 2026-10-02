#!/usr/bin/env python3
"""Generate encrypted ext4 test images with the Linux kernel and the
reference tools, plus a JSON manifest of what each holds.

Run as root on Linux (needs loop devices, dm-crypt, fscrypt, cryptsetup):

    python3 make-crypt-fixtures.py OUTDIR

Produces OUTDIR/<name>.img.gz and OUTDIR/<name>.json for:
- fscrypt-*: ext4 with directories encrypted by the kernel (v1 and v2
  policies, several algorithms, padding and IV flags), keys added with
  FS_IOC_ADD_ENCRYPTION_KEY; the manifest lists every file with the key
  and the names Linux shows without it.
- fscrypt-tool: directories encrypted by the `fscrypt` tool with a
  passphrase protector and a raw-key protector.
- luks*: cryptsetup LUKS1/LUKS2 volumes (several ciphers and key
  derivations) holding a small ext4.
The ext4 tests in ext4-core/crates/ext4-core/tests/crypt.rs compare against
these.
"""

import errno
import fcntl
import gzip
import hashlib
import json
import os
import platform
import random
import shutil
import stat
import struct
import subprocess
import sys
import tempfile


def run(*args, input=None):
    r = subprocess.run(args, input=input, capture_output=True)
    if r.returncode != 0:
        sys.exit(f"{' '.join(map(str, args))} failed:\n{r.stdout.decode()}{r.stderr.decode()}")
    return r.stdout.decode()


def ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("f") << 8) | nr


FS_IOC_SET_ENCRYPTION_POLICY = ioc(2, 19, 12)
FS_IOC_ADD_ENCRYPTION_KEY = ioc(3, 23, 80)

AES_256_XTS, AES_256_CTS, AES_128_CBC, AES_128_CTS = 1, 4, 5, 6
PAD_4, PAD_16, PAD_32 = 0, 2, 3
IV_INO_LBLK_64, IV_INO_LBLK_32 = 0x08, 0x10


def add_key(mnt, raw, descriptor=None):
    """Add a master key; returns the v2 identifier (or the v1 descriptor)."""
    if descriptor is not None:
        spec = struct.pack("<II8s24x", 1, 0, descriptor)
    else:
        spec = struct.pack("<II32x", 2, 0)
    arg = bytearray(spec + struct.pack("<II32x", len(raw), 0) + raw)
    fd = os.open(mnt, os.O_RDONLY)
    try:
        fcntl.ioctl(fd, FS_IOC_ADD_ENCRYPTION_KEY, arg)
    finally:
        os.close(fd)
    return descriptor if descriptor is not None else bytes(arg[8:24])


def set_policy(path, version, contents, filenames, flags, key, du_bits=0):
    if version == 1:
        p = struct.pack("<BBBB8s", 0, contents, filenames, flags, key)
    else:
        p = struct.pack("<BBBBB3x16s", 2, contents, filenames, flags, du_bits, key)
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.ioctl(fd, FS_IOC_SET_ENCRYPTION_POLICY, p)
    finally:
        os.close(fd)


def v1_descriptor(raw):
    return hashlib.sha512(hashlib.sha512(raw).digest()).digest()[:8]


def key_bytes(seed, n=64):
    return bytes((seed * 37 + i * 11 + 5) % 256 for i in range(n))


def populate(d, seed, full):
    rnd = random.Random(seed)

    def w(rel, data):
        with open(os.path.join(d, rel), "wb") as f:
            f.write(data)

    w("hello.txt", b"hello fscrypt\n")
    w("empty", b"")
    w("中文名.txt", "你好，加密目录\n".encode())
    w("block.bin", rnd.randbytes(4096))
    w("odd.bin", rnd.randbytes(10000))
    w("L" * 200 + ".txt", b"long name\n")
    w("M" * 255, b"longest name\n")
    os.mkdir(os.path.join(d, "sub"))
    w("sub/deep.txt", b"deep\n")
    os.symlink("hello.txt", os.path.join(d, "short-link"))
    os.symlink("sub/" + "x" * 80, os.path.join(d, "long-link"))
    os.link(os.path.join(d, "hello.txt"), os.path.join(d, "hello-hardlink"))
    os.mkfifo(os.path.join(d, "fifo"))
    if full:
        w("big.bin", rnd.randbytes(300_000))
        with open(os.path.join(d, "sparse.bin"), "wb") as f:
            f.seek(1 << 20)
            f.write(b"after the hole")
        # enough entries for an indexed (htree) directory; empty files
        # keep the image small
        os.mkdir(os.path.join(d, "many"))
        for i in range(300):
            w(f"many/file-{i:03d}", b"")


def walk(root):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for n in sorted(dirnames + filenames):
            p = os.path.join(dirpath, n)
            rel = os.path.relpath(p, root)
            if rel.split("/")[0] in ("lost+found", ".fscrypt"):
                continue
            st = os.lstat(p)
            e = {"path": rel, "ino": st.st_ino}
            if stat.S_ISLNK(st.st_mode):
                e.update(type="link", target=os.readlink(p), size=st.st_size)
            elif stat.S_ISDIR(st.st_mode):
                e["type"] = "dir"
            elif stat.S_ISFIFO(st.st_mode):
                e["type"] = "fifo"
            elif stat.S_ISREG(st.st_mode):
                e.update(type="file", size=st.st_size)
                try:
                    with open(p, "rb") as f:
                        e["sha256"] = hashlib.sha256(f.read()).hexdigest()
                except OSError as err:
                    if err.errno != errno.ENOKEY:
                        raise
                    e["locked"] = True
            out.append(e)
    return out


def finish(img, out, name, manifest):
    with open(img, "rb") as src, gzip.open(os.path.join(out, name + ".img.gz"), "wb", 9) as dst:
        shutil.copyfileobj(src, dst)
    with open(os.path.join(out, name + ".json"), "w") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=1)
    os.unlink(img)
    print(f"{name}: done")


def base_manifest(kind):
    return {
        "kind": kind,
        "kernel": platform.release(),
        "cryptsetup": run("cryptsetup", "--version").strip(),
    }


def fscrypt_image(out, work, name, mkfs, dirs):
    """dirs: list of (dirname, version, contents, filenames, flags, du_bits,
    key seed, full)."""
    img = os.path.join(work, name + ".img")
    with open(img, "wb") as f:
        f.truncate(24 << 20)
    run("mkfs.ext4", "-q", "-F", "-L", name, "-E", "lazy_itable_init=1,lazy_journal_init=1", *mkfs, img)
    mnt = os.path.join(work, "mnt")
    os.makedirs(mnt, exist_ok=True)
    run("mount", "-o", "loop,noinit_itable", img, mnt)
    keys = []
    os.mkdir(os.path.join(mnt, "plain"))
    populate(os.path.join(mnt, "plain"), 1, False)
    for dname, version, contents, filenames, flags, du_bits, seed, full in dirs:
        raw = key_bytes(seed)
        if version == 1:
            ref = add_key(mnt, raw, v1_descriptor(raw))
        else:
            ref = add_key(mnt, raw)
        d = os.path.join(mnt, dname)
        os.mkdir(d)
        set_policy(d, version, contents, filenames, flags, ref, du_bits)
        populate(d, seed, full)
        keys.append({"dir": dname, "version": version, "key": raw.hex(), "ref": ref.hex()})
    with_key = walk(mnt)
    run("umount", mnt)
    # keys added with FS_IOC_ADD_ENCRYPTION_KEY go away with the mount
    run("mount", "-o", "loop,ro,noinit_itable", img, mnt)
    without_key = walk(mnt)
    run("umount", mnt)
    m = base_manifest("fscrypt")
    m.update(keys=keys, with_key=with_key, without_key=without_key)
    finish(img, out, name, m)


def fscrypt_tool_image(out, work):
    name = "fscrypt-tool"
    img = os.path.join(work, name + ".img")
    with open(img, "wb") as f:
        f.truncate(24 << 20)
    run("mkfs.ext4", "-q", "-F", "-O", "encrypt", "-L", name, img)
    mnt = os.path.join(work, "mnt")
    os.makedirs(mnt, exist_ok=True)
    run("mount", "-o", "loop,noinit_itable", img, mnt)
    run("fscrypt", "setup", "--force", "--quiet")
    # cheap hashing so tests stay fast
    with open("/etc/fscrypt.conf") as f:
        conf = json.load(f)
    conf["hash_costs"] = {"time": "2", "memory": "16384", "parallelism": "2"}
    with open("/etc/fscrypt.conf", "w") as f:
        json.dump(conf, f)
    run("fscrypt", "setup", mnt, "--quiet")
    secret = os.path.join(mnt, "secret")
    os.mkdir(secret)
    passphrase = "fscrypt test passphrase"
    run(
        "fscrypt", "encrypt", secret, "--source=custom_passphrase", "--name=testprot", "--quiet",
        input=(passphrase + "\n").encode(),
    )
    populate(secret, 21, False)
    rawkey = key_bytes(99, 32)
    keyfile = os.path.join(work, "raw.key")
    with open(keyfile, "wb") as f:
        f.write(rawkey)
    rawdir = os.path.join(mnt, "rawdir")
    os.mkdir(rawdir)
    run("fscrypt", "encrypt", rawdir, "--source=raw_key", "--name=rawprot", f"--key={keyfile}", "--quiet")
    populate(rawdir, 22, False)
    with_key = walk(mnt)
    run("fscrypt", "lock", secret, "--quiet")
    run("fscrypt", "lock", rawdir, "--quiet")
    run("umount", mnt)
    run("mount", "-o", "loop,ro,noinit_itable", img, mnt)
    without_key = walk(mnt)
    run("umount", mnt)
    m = base_manifest("fscrypt-tool")
    m.update(
        passphrase=passphrase,
        raw_protector_key=rawkey.hex(),
        # the tool has no version option; ask the package manager
        tool=subprocess.run(["pacman", "-Q", "fscrypt"], capture_output=True).stdout.decode().strip(),
        with_key=with_key,
        without_key=without_key,
    )
    finish(img, out, name, m)


LUKS_PASS = b"luks test passphrase"
LUKS_PASS2 = b"second passphrase"


def sparsify_luks(img):
    """cryptsetup fills unused key slot space with random bytes, which do
    not compress. Zero everything before the data that cryptsetup never
    reads back: keep the header(s) and the areas of active key slots."""
    with open(img, "rb") as f:
        hdr = f.read(4096)
    keep = []
    if struct.unpack(">H", hdr[6:8])[0] == 1:
        data = struct.unpack(">I", hdr[104:108])[0] * 512
        key_bytes_ = struct.unpack(">I", hdr[108:112])[0]
        keep.append((0, 4096))
        for i in range(8):
            o = 208 + i * 48
            active, _, _, off, stripes = struct.unpack(">II32sII", hdr[o:o + 48])
            if active == 0x00AC71F3:
                n = (key_bytes_ * stripes + 511) // 512 * 512
                keep.append((off * 512, off * 512 + n))
    else:
        meta = json.loads(run("cryptsetup", "luksDump", "--dump-json-metadata", img))
        hdr_size = struct.unpack(">Q", hdr[8:16])[0]
        keep.append((0, 2 * hdr_size))
        for ks in meta["keyslots"].values():
            off = int(ks["area"]["offset"])
            keep.append((off, off + int(ks["area"]["size"])))
        data = min(int(s["offset"]) for s in meta["segments"].values())
    keep.sort()
    with open(img, "r+b") as f:
        pos = 0
        for start, end in keep + [(data, data)]:
            if start > pos:
                f.seek(pos)
                f.write(bytes(start - pos))
            pos = max(pos, end)


def luks_image(out, work, name, fmt, mkfs, second=None):
    img = os.path.join(work, name + ".img")
    with open(img, "wb") as f:
        f.truncate(32 << 20)
    run("cryptsetup", "luksFormat", "--batch-mode", *fmt, img, "--key-file=-", input=LUKS_PASS)
    passphrases = [LUKS_PASS.decode()]
    if second is not None:
        kf = os.path.join(work, "pass2")
        with open(kf, "wb") as f:
            f.write(LUKS_PASS2)
        run("cryptsetup", "luksAddKey", "--batch-mode", *second, img, kf, "--key-file=-", input=LUKS_PASS)
        passphrases.append(LUKS_PASS2.decode())
    sparsify_luks(img)
    run("cryptsetup", "open", "--key-file=-", img, "fixture", input=LUKS_PASS)
    dev = "/dev/mapper/fixture"
    run("mkfs.ext4", "-q", "-F", "-O", "^has_journal", "-E", "lazy_itable_init=1", "-L", "luksdata", *mkfs, dev)
    mnt = os.path.join(work, "mnt")
    os.makedirs(mnt, exist_ok=True)
    run("mount", "-o", "noinit_itable", dev, mnt)
    rnd = random.Random(name)
    with open(os.path.join(mnt, "hello.txt"), "wb") as f:
        f.write(b"hello luks\n")
    with open(os.path.join(mnt, "data.bin"), "wb") as f:
        f.write(rnd.randbytes(50_000))
    os.mkdir(os.path.join(mnt, "dir"))
    with open(os.path.join(mnt, "dir", "nested.txt"), "wb") as f:
        f.write(b"nested\n")
    files = walk(mnt)
    run("umount", mnt)
    run("cryptsetup", "close", "fixture")
    m = base_manifest("luks")
    m.update(passphrases=passphrases, dump=run("cryptsetup", "luksDump", img), files=files)
    finish(img, out, name, m)


def main():
    if len(sys.argv) != 2 or os.geteuid() != 0:
        sys.exit("usage (as root): make-crypt-fixtures.py OUTDIR")
    out = os.path.abspath(sys.argv[1])
    os.makedirs(out, exist_ok=True)
    work = tempfile.mkdtemp()
    try:
        x, c = AES_256_XTS, AES_256_CTS
        fscrypt_image(out, work, "fscrypt-4k", ["-b", "4096", "-O", "encrypt,stable_inodes"], [
            ("v2", 2, x, c, PAD_32, 0, 1, True),
            ("v2-pad4", 2, x, c, PAD_4, 0, 2, False),
            ("v2-aes128", 2, AES_128_CBC, AES_128_CTS, PAD_16, 0, 3, False),
            ("v2-iv64", 2, x, c, PAD_32 | IV_INO_LBLK_64, 0, 4, False),
            ("v2-iv32", 2, x, c, PAD_32 | IV_INO_LBLK_32, 0, 5, False),
            ("v2-du512", 2, x, c, PAD_32, 9, 6, False),
            ("v1", 1, x, c, PAD_32, 0, 7, True),
        ])
        fscrypt_image(out, work, "fscrypt-1k", ["-b", "1024", "-O", "encrypt"], [
            ("v2", 2, x, c, PAD_16, 0, 11, True),
            ("v1", 1, AES_128_CBC, AES_128_CTS, PAD_4, 0, 12, False),
        ])
        fscrypt_tool_image(out, work)
        luks_image(out, work, "luks1-xts", ["--type", "luks1", "--cipher", "aes-xts-plain64", "--key-size", "512",
                                           "--hash", "sha256", "--iter-time", "50"], ["-b", "4096"])
        luks_image(out, work, "luks1-cbc-essiv", ["--type", "luks1", "--cipher", "aes-cbc-essiv:sha256",
                                                 "--key-size", "256", "--hash", "sha1", "--iter-time", "50"],
                   ["-b", "1024"])
        luks_image(out, work, "luks2-argon2id", ["--type", "luks2", "--pbkdf", "argon2id", "--pbkdf-memory", "32768",
                                                "--pbkdf-parallel", "4", "--pbkdf-force-iterations", "4",
                                                "--label", "testlabel"], ["-b", "4096"],
                   second=["--pbkdf", "argon2i", "--pbkdf-memory", "16384", "--pbkdf-force-iterations", "4"])
        luks_image(out, work, "luks2-pbkdf2-4k", ["--type", "luks2", "--pbkdf", "pbkdf2", "--hash", "sha512",
                                                 "--pbkdf-force-iterations", "1000", "--sector-size", "4096"],
                   ["-b", "4096"])
        luks_image(out, work, "luks2-default", ["--type", "luks2"], ["-b", "4096"])
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
