#!/usr/bin/env python3
"""Report what a captured frame actually CONTAINS, not just how many pixels are lit.

Why this exists: `non_black_pixels` from LITEBOX_DUMP_FRAMES cannot distinguish
"the desktop rendered" from "a background colour fills the screen". A 1920x1080
frame showing nothing but a flat fill scores 2,073,597 -- indistinguishable from
a fully populated desktop by that number alone. That ambiguity let a run be
reported as success when the real output was a panel bar with an icon and a
clock on an otherwise empty background, which is the project's original symptom.

This decodes the BMP, finds the dominant (background) colour, and reports which
rows and columns hold anything else, so "renders" and "renders correctly" stop
being the same measurement.

Usage: python decode_frame.py <frame.bmp> [--rows]
Exit code 0 if real content was found beyond the background, 1 if the frame is
effectively a flat fill.
"""
import collections, struct, sys


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    data = open(sys.argv[1], "rb").read()
    if data[:2] != b"BM":
        print("not a BMP")
        return 2
    off = struct.unpack_from("<I", data, 10)[0]
    w = struct.unpack_from("<i", data, 18)[0]
    h = struct.unpack_from("<i", data, 22)[0]
    bpp = struct.unpack_from("<H", data, 28)[0]
    if bpp != 32:
        print("expected 32bpp, got %d" % bpp)
        return 2
    rows = abs(h)
    stride = ((w * 4 + 3) // 4) * 4
    bottom_up = h > 0

    def px(x, y):
        yy = rows - 1 - y if bottom_up else y
        o = off + yy * stride + x * 4
        return data[o], data[o + 1], data[o + 2]

    print("frame: %dx%d %dbpp" % (w, rows, bpp))

    # The dominant colour over a coarse grid is the background.
    counts = collections.Counter()
    for y in range(0, rows, 8):
        for x in range(0, w, 8):
            counts[px(x, y)] += 1
    bg, bg_n = counts.most_common(1)[0]
    total = sum(counts.values())
    print("background: rgb%s (%.1f%% of sampled pixels)" % (str(bg), 100.0 * bg_n / total))

    # Which rows hold anything other than the background?
    content_rows = []
    for y in range(0, rows, 4):
        n = sum(1 for x in range(0, w, 8) if px(x, y) != bg)
        if n:
            content_rows.append((y, n))
    print("rows with non-background content: %d of %d sampled" % (len(content_rows), rows // 4))

    if not content_rows:
        print("\nVERDICT: FLAT FILL. Nothing is drawn. A pixel count would still")
        print("report the full frame as 'non-black'.")
        return 1

    # Contiguous vertical bands of content.
    bands = []
    start = prev = content_rows[0][0]
    for y, _ in content_rows[1:]:
        if y - prev > 8:
            bands.append((start, prev))
            start = y
        prev = y
    bands.append((start, prev))
    print("\ncontent bands (vertical):")
    for a, b in bands:
        print("  y=%d..%d  (height ~%d px)" % (a, b, b - a + 4))

    # Bright clusters horizontally: text and icons against a dark background.
    bright = [x for x in range(w)
              if any(sum(px(x, y)) > 300 for y in range(bands[0][0], min(bands[0][1] + 4, rows)))]
    if bright:
        groups = []
        start = prev = bright[0]
        for x in bright[1:]:
            if x - prev > 20:
                groups.append((start, prev))
                start = x
            prev = x
        groups.append((start, prev))
        print("\nbright clusters in the first band (icons/text):")
        for a, b in groups:
            print("  x=%d..%d  (width %d)" % (a, b, b - a + 1))

    covered = sum(b - a + 4 for a, b in bands)
    print("\nVERDICT: content covers ~%d of %d rows (%.1f%%)." % (covered, rows, 100.0 * covered / rows))
    if covered < rows * 0.1:
        print("That is a THIN STRIP on an otherwise empty background -- not a populated desktop.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
