#!/usr/bin/env python3
"""Uncached reads from several threads while other threads write,
truncate, delete and recreate the same files on a mounted volume.
Every 8-byte word written to a file holds that file's inode number, so
data that went to (or came from) blocks reused by another file, or by
metadata, shows up as a foreign word: in the reads, and in a final pass
over every file. Meant for volumes with parallel reads and writes on
(ParallelReads, ParallelWrites).

    python3 scripts/io-race.py DIR SECONDS [READERS] [WRITERS]
"""
import errno
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


def foreign(data, ino):
    words = set(memoryview(data[: len(data) // 8 * 8]).cast("Q"))
    return words - {0, good(ino)}


def writer(d, stop, rng, counts):
    while not stop.is_set():
        p = f"{d}/f{rng.randrange(FILES)}"
        op = rng.randrange(4)
        a = rng.randrange(0, 3 << 20) & ~7
        b = rng.randrange(8, 2 << 20) & ~7
        if rng.randrange(4):
            # whole blocks, which the parallel path takes
            a &= ~4095
            b = (b + 4095) & ~4095
        try:
            if op == 1:
                os.unlink(p)  # a new inode under the same name
        except FileNotFoundError:
            pass
        fd = os.open(p, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            if rng.randrange(2):
                fcntl.fcntl(fd, F_NOCACHE, 1)
            ino = os.fstat(fd).st_ino
            if op == 0:
                os.ftruncate(fd, a)  # shrink (or grow with a hole)
            os.pwrite(fd, fill(ino, b), a)
            if op == 3:
                os.ftruncate(fd, (a + b // 2) & ~7)
        except OSError as e:
            if e.errno != errno.ENOSPC:  # a full volume is expected
                raise
            counts["full"] += 1
        finally:
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
        bad = foreign(data, ino)
        if bad:
            errors.append(f"inode {ino} at {off}+{len(data)}: foreign words {[hex(w) for w in list(bad)[:4]]}")
            stop.set()
        counts["reads"] += 1
        counts["bytes"] += len(data)


def main(d, seconds, readers, writers):
    os.makedirs(d, exist_ok=True)
    stop = threading.Event()
    counts = {"changes": 0, "full": 0, "reads": 0, "bytes": 0}
    errors = []
    threads = [threading.Thread(target=writer, args=(d, stop, random.Random(1 + i), counts)) for i in range(writers)]
    threads += [
        threading.Thread(target=reader, args=(d, stop, random.Random(100 + i), counts, errors)) for i in range(readers)
    ]
    for t in threads:
        t.start()
    time.sleep(seconds)
    stop.set()
    for t in threads:
        t.join()
    for i in range(FILES):
        p = f"{d}/f{i}"
        if os.path.exists(p):
            with open(p, "rb") as f:
                ino = os.fstat(f.fileno()).st_ino
                bad = foreign(f.read(), ino)
            if bad:
                errors.append(f"{p} (inode {ino}) ends with foreign words {[hex(w) for w in list(bad)[:4]]}")
    print(
        f"{counts['changes']} changes ({counts['full']} hit a full volume), "
        f"{counts['reads']} reads ({counts['bytes'] >> 20} MB)"
    )
    for e in errors:
        print(e)
    return 1 if errors else 0


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4, 5):
        print(__doc__.strip())
        sys.exit(2)
    readers = int(sys.argv[3]) if len(sys.argv) > 3 else 6
    writers = int(sys.argv[4]) if len(sys.argv) > 4 else 1
    sys.exit(main(sys.argv[1], float(sys.argv[2]), readers, writers))
