#!/usr/bin/env python3
"""Check whether a drive returns stale data for blocks read shortly after
they were written. Seen with an NVMe SSD (Fanxiang S790MAX, InnoGrit
IG5236, firmware 030W0P4W) in USB and Thunderbolt enclosures alike: after
a block had been rewritten many times in a row, a read issued about a
millisecond after a write returned the previous contents, and kept doing
so until other data had been read. No file system is involved.

Uses a 4 MiB region 64 MiB before the end of the partition, saves it first
and writes it back at the end. The partition must not be mounted, and the
device node needs root (or ownership):

    diskutil unmount diskNsM
    sudo python3 scripts/check-stale-reads.py /dev/rdiskNsM
"""
import fcntl
import mmap
import os
import random
import struct
import sys
import time

DKIOCGETBLOCKSIZE = 0x40046418
DKIOCGETBLOCKCOUNT = 0x40086419
BS = 4096
REGION = 4 << 20


class Device:
    def __init__(self, path):
        self.fd = os.open(path, os.O_RDWR)
        self.sector = struct.unpack("I", fcntl.ioctl(self.fd, DKIOCGETBLOCKSIZE, b"\0" * 4))[0]
        count = struct.unpack("Q", fcntl.ioctl(self.fd, DKIOCGETBLOCKCOUNT, b"\0" * 8))[0]
        self.size = self.sector * count

    def read(self, off, n):
        b = mmap.mmap(-1, n)  # page-aligned buffer
        if os.preadv(self.fd, [b], off) != n:
            raise OSError("short read")
        return b[:]

    def write(self, off, data):
        b = mmap.mmap(-1, len(data))
        b.write(data)
        if os.pwritev(self.fd, [b], off) != len(data):
            raise OSError("short write")


def rewrite_block(dev, x, pause, rng):
    """Grow the contents of one block step by step the way a file system
    appends to a file: read the current contents, write the block with
    more data, read it back. Returns (steps, stale reads)."""
    content = b""
    stale = 0
    dev.write(x, bytes(BS))
    steps = 0
    while True:
        line = rng.randbytes(rng.randrange(20, 120))
        if len(content) + len(line) > BS:
            return steps, stale
        n = max(dev.sector, -(-len(content) // dev.sector) * dev.sector)
        got = dev.read(x, n)[: len(content)]
        if got != content:
            stale += 1
            content = got
        content += line
        block = bytearray(dev.read(x, BS))
        block[: len(content)] = content
        dev.write(x, bytes(block))
        time.sleep(pause)
        if dev.read(x, BS)[: len(content)] != content:
            stale += 1
        steps += 1


def main(path):
    dev = Device(path)
    base = (dev.size - (64 << 20)) // (1 << 20) * (1 << 20)
    print(f"{path}: {dev.size} bytes, {dev.sector}-byte sectors; test region at byte {base}")
    saved = dev.read(base, REGION)
    rng = random.Random(1)
    total = 0
    try:
        for pause in (0, 0.0002, 0.001, 0.005):
            steps = stale = 0
            for _ in range(8):
                x = base + rng.randrange(REGION // BS) * BS
                s, bad = rewrite_block(dev, x, pause, rng)
                steps += s
                stale += bad
            total += stale
            print(f"  pause {pause * 1000:4.1f} ms between write and read: {stale} stale reads in {steps} steps")
    finally:
        dev.write(base, saved)
        if dev.read(base, REGION) != saved:
            # the check itself may be served stale data: read elsewhere first
            dev.read(dev.size // 2 // BS * BS, 32 << 20)
        restored = dev.read(base, REGION) == saved
        os.close(dev.fd)
        print("test region restored" if restored else "WARNING: could not verify that the test region was restored")
    if total:
        print("RESULT: this drive returned stale data after writes; it is not reliable for writing")
        return 1
    print("RESULT: no stale reads")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__.strip())
        sys.exit(2)
    sys.exit(main(sys.argv[1]))
