#!/usr/bin/env python3
"""Scan a numbered series of litebox_frame_dump_N.bmp files and report
non_black_pixels per frame, to locate a blanking/degradation transition.

Usage: python scan_frame_series.py <dir> [prefix]
"""
import struct
import sys
import glob
import os
import re


def count_nonblack(path):
    data = open(path, "rb").read()
    if data[:2] != b"BM":
        return None
    off = struct.unpack_from("<I", data, 10)[0]
    w = struct.unpack_from("<i", data, 18)[0]
    h = struct.unpack_from("<i", data, 22)[0]
    bpp = struct.unpack_from("<H", data, 28)[0]
    if bpp != 32:
        return None
    rows = abs(h)
    stride = ((w * 4 + 3) // 4) * 4
    count = 0
    for y in range(rows):
        row_off = off + y * stride
        row = data[row_off:row_off + w * 4]
        for x in range(0, len(row), 4):
            b, g, r, a = row[x], row[x + 1], row[x + 2], row[x + 3]
            if b or g or r:
                count += 1
    return count


def main():
    d = sys.argv[1] if len(sys.argv) > 1 else "."
    prefix = sys.argv[2] if len(sys.argv) > 2 else "litebox_frame_dump_"
    files = glob.glob(os.path.join(d, prefix + "*.bmp"))

    def key(p):
        m = re.search(r"(\d+)\.bmp$", p)
        return int(m.group(1)) if m else -1

    files.sort(key=key)
    prev = None
    for f in files:
        n = key(f)
        c = count_nonblack(f)
        marker = ""
        if prev is not None and c is not None and prev > 0:
            ratio = c / prev
            if ratio < 0.5:
                marker = "  <<<< DROP"
        print(f"frame {n:4d}: non_black_pixels={c}{marker}")
        prev = c


if __name__ == "__main__":
    main()
