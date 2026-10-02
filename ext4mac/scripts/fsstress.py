#!/usr/bin/env python3
"""Random file operations on a mounted volume, checked against an
in-memory model.

    python3 scripts/fsstress.py DIR SEED ROUNDS [DEVICE]

Each round runs 60 random operations on six files (create, overwrite at
random offsets, uncached writes, appends, truncate, extend, preallocate,
memory-mapped writes, unlink, rename over another file), then compares
every file with the model. With DEVICE (e.g. disk4s2) the volume is
unmounted and mounted again with diskutil every third round, so contents
are read back from the disk. Run it with and without kernel offloaded I/O.
"""
import fcntl
import mmap
import os
import random
import struct
import subprocess
import sys

F_PREALLOCATE = 42
F_ALLOCATEALL = 4
F_PEOFPOSMODE = 3
F_NOCACHE = 48
MAXSIZE = 3 << 20


def remount(dev):
    for cmd in (["diskutil", "unmount", dev], ["diskutil", "mount", dev]):
        subprocess.run(cmd, check=True, capture_output=True, timeout=120)


def step(d, rng, names, model):
    n = rng.choice(names)
    p = f"{d}/{n}"
    m = model.get(n)
    op = rng.choice(["create", "write", "write", "write", "append", "truncate", "extend",
                     "prealloc", "mmap", "nocache", "unlink", "rename"])
    if m is None:
        op = "create"
    if op == "create":
        data = rng.randbytes(rng.choice([0, 1, 100, 4095, 4096, 5000, 70000]))
        with open(p, "wb") as f:
            f.write(data)
        model[n] = bytearray(data)
    elif op in ("write", "nocache"):
        off = rng.randrange(0, min(len(m) + 9000, MAXSIZE))
        data = rng.randbytes(rng.choice([1, 17, 512, 4096, 4097, 12000, 65536, 300000]))
        if off + len(data) > MAXSIZE:
            return
        fd = os.open(p, os.O_RDWR)
        if op == "nocache":
            fcntl.fcntl(fd, F_NOCACHE, 1)
        os.pwrite(fd, data, off)
        os.close(fd)
        if off > len(m):
            m.extend(bytes(off - len(m)))
        m[off:off + len(data)] = data
    elif op == "append":
        fd = os.open(p, os.O_WRONLY | os.O_APPEND)
        for _ in range(rng.randrange(1, 30)):
            data = rng.randbytes(rng.randrange(1, 300))
            os.write(fd, data)
            m.extend(data)
        os.close(fd)
    elif op == "truncate":
        size = rng.randrange(0, len(m) + 1)
        os.truncate(p, size)
        del m[size:]
    elif op == "extend":
        size = min(len(m) + rng.choice([1, 4095, 4096, 10000, 200000]), MAXSIZE)
        os.truncate(p, size)
        m.extend(bytes(size - len(m)))
    elif op == "prealloc":
        fd = os.open(p, os.O_RDWR)
        store = struct.pack("IiqqQ", F_ALLOCATEALL, F_PEOFPOSMODE, 0, rng.choice([4096, 65536, 1 << 20]), 0)
        try:
            fcntl.fcntl(fd, F_PREALLOCATE, store)
        except OSError:
            pass
        os.close(fd)
    elif op == "mmap":
        if not m:
            return
        with open(p, "r+b") as f:
            mm = mmap.mmap(f.fileno(), len(m))
            for _ in range(rng.randrange(1, 20)):
                off = rng.randrange(0, len(m))
                data = rng.randbytes(min(rng.randrange(1, 9000), len(m) - off))
                mm[off:off + len(data)] = data
                m[off:off + len(data)] = data
            mm.flush()
            mm.close()
    elif op == "unlink":
        os.unlink(p)
        del model[n]
    elif op == "rename":
        other = rng.choice(names)
        if other != n:
            os.rename(p, f"{d}/{other}")
            model[other] = model.pop(n)


def check(d, names, model):
    for n in names:
        p = f"{d}/{n}"
        if n not in model:
            if os.path.exists(p):
                return f"{n} should not exist"
            continue
        have = open(p, "rb").read()
        want = model[n]
        if have != want:
            i = next((k for k, (a, b) in enumerate(zip(have, want)) if a != b), min(len(have), len(want)))
            return f"{n} differs at byte {i} (size {len(have)}, expected {len(want)})"
    return None


def main(d, seed, rounds, dev=None):
    rng = random.Random(seed)
    os.makedirs(d, exist_ok=True)
    names = [f"f{i}" for i in range(6)]
    for n in names:
        if os.path.exists(f"{d}/{n}"):
            os.unlink(f"{d}/{n}")
    model = {}
    for r in range(rounds):
        for _ in range(60):
            step(d, rng, names, model)
        if dev and r % 3 == 2:
            remount(dev)
        problem = check(d, names, model)
        if problem:
            print(f"seed {seed}, round {r}: {problem}")
            return 1
    print(f"seed {seed}: {rounds} rounds, all files match")
    return 0


if __name__ == "__main__":
    if len(sys.argv) not in (4, 5):
        print(__doc__.strip())
        sys.exit(2)
    sys.exit(main(sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4] if len(sys.argv) == 5 else None))
