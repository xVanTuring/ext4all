#!/usr/bin/env python3
"""Uncached reads from several threads while another thread truncates,
deletes, recreates and rewrites the same files on a mounted volume.
Every 8-byte word written to a file holds that file's inode number, so a
read that returned blocks reused by another file (or by metadata) shows a
foreign word. Meant for volumes with parallel reads on (ParallelReads).

    python3 scripts/read-race.py DIR SECONDS [READERS]
"""
import fcntl
import os
import random
import struct
import sys
import threading
import time

F_NOCACHE = 48
MAGIC = 0x5EEDCAFE
FILES = 6


def good(ino):
    return (ino << 32) | MAGIC


def fill(ino, n):
    w = struct.pack("<Q", good(ino))
    return (w * (n // 8 + 1))[:n]


def writer(d, stop, rng, counts):
    while not stop.is_set():
        p = f"{d}/f{rng.randrange(FILES)}"
        op = rng.randrange(4)
        a = rng.randrange(0, 3 << 20) & ~7
        b = rng.randrange(8, 2 << 20) & ~7
        if op == 1 and os.path.exists(p):
            os.unlink(p)  # a new inode under the same name
        fd = os.open(p, os.O_RDWR | os.O_CREAT, 0o644)
        if rng.randrange(2):
            fcntl.fcntl(fd, F_NOCACHE, 1)
        ino = os.fstat(fd).st_ino
        if op == 0:
            os.ftruncate(fd, a)  # shrink (or grow with a hole)
        os.pwrite(fd, fill(ino, b), a)
        if op == 3:
            os.ftruncate(fd, (a + b // 2) & ~7)
        os.close(fd)
        counts["changes"] += 1


def reader(d, stop, rng, counts, errors):
    while not stop.is_set():
        p = f"{d}/f{rng.randrange(FILES)}"
        try:
            fd = os.open(p, os.O_RDONLY)
        except FileNotFoundError:
            continue
        fcntl.fcntl(fd, F_NOCACHE, 1)
        ino = os.fstat(fd).st_ino
        off = rng.randrange(0, 4 << 20) & ~7
        n = rng.randrange(8, 2 << 20) & ~7
        data = os.pread(fd, n, off)
        os.close(fd)
        words = set(memoryview(data[: len(data) // 8 * 8]).cast("Q"))
        bad = words - {0, good(ino)}
        if bad:
            errors.append(f"inode {ino} at {off}+{len(data)}: foreign words {[hex(w) for w in list(bad)[:4]]}")
            stop.set()
        counts["reads"] += 1
        counts["bytes"] += len(data)


def main(d, seconds, readers):
    os.makedirs(d, exist_ok=True)
    stop = threading.Event()
    counts = {"changes": 0, "reads": 0, "bytes": 0}
    errors = []
    threads = [threading.Thread(target=writer, args=(d, stop, random.Random(1), counts))]
    threads += [
        threading.Thread(target=reader, args=(d, stop, random.Random(100 + i), counts, errors)) for i in range(readers)
    ]
    for t in threads:
        t.start()
    time.sleep(seconds)
    stop.set()
    for t in threads:
        t.join()
    print(f"{counts['changes']} changes, {counts['reads']} reads ({counts['bytes'] >> 20} MB)")
    for e in errors:
        print(e)
    return 1 if errors else 0


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        print(__doc__.strip())
        sys.exit(2)
    sys.exit(main(sys.argv[1], float(sys.argv[2]), int(sys.argv[3]) if len(sys.argv) == 4 else 6))
