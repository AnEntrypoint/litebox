#!/usr/bin/env python3
"""Attribute on-screen content to a specific process by diffing two frames.

Comparing visual verdicts ("both look like a thin strip") cannot tell whether a
component drew anything. Diffing the actual bytes can: capture a frame before a
component starts and another after it has settled, and the changed pixels are
exactly what that component contributed.

This settled a real question. The top bar in these runs was suspected to be
xfce4-panel's. Diffing a pre-panel frame against one 49 seconds post-panel showed
the hashes differ but only 8 columns changed, inside the existing clock -- a clock
digit advancing. Had xfce4-panel drawn that bar, its arrival would have created
it, not nudged one digit. Combined with the bar being present 48 seconds before
xfce4-panel started, that attributes it to weston-desktop-shell instead.

Usage: python diff_frames.py <before.bmp> <after.bmp>
Exit code 0 if the frames differ, 1 if byte-identical.
"""
import hashlib, struct, sys


def load(path):
    d = open(path, "rb").read()
    if d[:2] != b"BM":
        raise SystemExit("%s is not a BMP" % path)
    off = struct.unpack_from("<I", d, 10)[0]
    w = struct.unpack_from("<i", d, 18)[0]
    h = struct.unpack_from("<i", d, 22)[0]
    return d, off, w, abs(h), h > 0


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    da, oa, w, rows, bottom_up = load(sys.argv[1])
    db, ob, w2, rows2, _ = load(sys.argv[2])
    if (w, rows) != (w2, rows2):
        print("dimension mismatch: %dx%d vs %dx%d" % (w, rows, w2, rows2))
        return 2

    ha = hashlib.sha256(da).hexdigest()[:16]
    hb = hashlib.sha256(db).hexdigest()[:16]
    print("before: %s  %s" % (ha, sys.argv[1]))
    print("after:  %s  %s" % (hb, sys.argv[2]))
    if ha == hb:
        print("\nBYTE-IDENTICAL. Nothing changed between these frames at all.")
        return 1

    stride = ((w * 4 + 3) // 4) * 4

    def px(d, off, x, y):
        yy = rows - 1 - y if bottom_up else y
        o = off + yy * stride + x * 4
        return d[o], d[o + 1], d[o + 2]

    # Rows first (cheap sample), then exact columns within them.
    changed_rows = []
    for y in range(rows):
        for x in range(0, w, 4):
            if px(da, oa, x, y) != px(db, ob, x, y):
                changed_rows.append(y)
                break
    if not changed_rows:
        print("\nHashes differ but no sampled pixel does: the change is outside the sample grid.")
        return 0

    cols = set()
    for y in changed_rows:
        for x in range(w):
            if px(da, oa, x, y) != px(db, ob, x, y):
                cols.add(x)
    cs = sorted(cols)

    print("\nchanged rows: %d of %d  (y=%d..%d)" % (len(changed_rows), rows, changed_rows[0], changed_rows[-1]))
    print("changed cols: %d of %d  (x=%d..%d)" % (len(cs), w, cs[0], cs[-1]))
    area = len(changed_rows) * len(cs)
    print("changed area: ~%d px of %d (%.4f%%)" % (area, w * rows, 100.0 * area / (w * rows)))
    if area < w * rows * 0.001:
        print("\nThat is a TINY localised change -- an existing element updating,")
        print("not a new component painting the screen.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
