#!/usr/bin/env python3
"""Cross-verify the ENDPOINT-2 foreground-daemon XFCE frames against the
`-retro` root-weave baseline.

Why a separate script rather than reusing frame_stats.py alone: the question
this run poses is not "how many pixels are lit" (the standing lesson of this
investigation is that a pixel count never identifies who painted it) but
"is this XFCE content or is it the X server's own default root weave".

fg.sh gives us an unusually crisp discriminator: it runs `xsetroot -solid navy`
BEFORE any XFCE component starts. So the frames partition into three verdicts:

  ROOT_WEAVE  -- dominant colour is the black/white alternating pattern of
                 Xorg's default root window, two colours in a fine checker.
                 XFCE painted nothing. NEGATIVE.
  NAVY_ONLY   -- dominant colour is navy (0,0,128) covering essentially the
                 whole frame, with no other structure. xsetroot worked, X is
                 real, but no XFCE component painted. NEGATIVE for the desktop
                 claim, POSITIVE for "X server renders through the DRM path".
  DESKTOP     -- navy (or a wallpaper) PLUS distinct structure: a horizontal
                 panel band at the top or bottom, and/or window decorations.
                 Only this is a rendered XFCE desktop.

Usage: python verify_fg_frames.py <dir-or-frame> [...]
"""
import collections
import os
import struct
import sys

NAVY = (0, 0, 128)


def load(path):
    data = open(path, "rb").read()
    if data[:2] != b"BM":
        raise ValueError("not a BMP: %s" % path)
    off = struct.unpack_from("<I", data, 10)[0]
    w = struct.unpack_from("<i", data, 18)[0]
    h = struct.unpack_from("<i", data, 22)[0]
    bpp = struct.unpack_from("<H", data, 28)[0]
    if bpp != 32:
        raise ValueError("expected 32bpp, got %d" % bpp)
    bottom_up = h > 0
    h = abs(h)
    stride = w * 4
    return data, off, w, h, stride, bottom_up


def analyze(path):
    data, off, w, h, stride, bottom_up = load(path)
    colors = collections.Counter()
    # per-row counts of pixels that differ from the eventual dominant colour;
    # filled on a second pass once the dominant colour is known.
    rows = []
    for y in range(h):
        ry = (h - 1 - y) if bottom_up else y
        row = data[off + ry * stride: off + (ry + 1) * stride]
        rows.append(row)
        for x in range(w):
            b, g, r = row[x * 4], row[x * 4 + 1], row[x * 4 + 2]
            colors[(r, g, b)] += 1

    total = w * h
    dom, dom_n = colors.most_common(1)[0]
    non_black = total - colors.get((0, 0, 0), 0)

    # Structure: rows whose non-dominant pixel count is a meaningful fraction.
    row_other = []
    for y, row in enumerate(rows):
        n = 0
        for x in range(w):
            if (row[x * 4 + 2], row[x * 4 + 1], row[x * 4]) != dom:
                n += 1
        row_other.append(n)
    structured_rows = [y for y, n in enumerate(row_other) if n > w * 0.02]

    # Contiguous bands of structured rows -- a panel is one such band, tall
    # enough to be a bar (>= 16px) and hugging an edge.
    bands = []
    if structured_rows:
        start = prev = structured_rows[0]
        for y in structured_rows[1:]:
            if y != prev + 1:
                bands.append((start, prev))
                start = y
            prev = y
        bands.append((start, prev))

    # Root-weave signature: exactly two colours, near 50/50, alternating.
    top2 = colors.most_common(2)
    weave = (
        len(colors) <= 4
        and len(top2) == 2
        and abs(top2[0][1] - top2[1][1]) < total * 0.25
    )

    # ---- The four signals, reported SEPARATELY (pre-registered by sdv) -------
    # Collapsing these into one pass/fail is exactly what destroys the value of
    # this run: "fork trigger avoided but the known pixbuf/asset gap is now the
    # front-blocker" and "fork bug still active" both look black at one bit of
    # resolution, and they are different findings.
    #
    # (a) SOLID FILL -- did xsetroot (or any client) paint a real flat colour?
    #     Reported as the dominant colour and its coverage, with navy called out
    #     by name since fg.sh runs `xsetroot -solid navy` explicitly.
    a_navy_px = colors.get(NAVY, 0)
    a_solid = dom_n > total * 0.50 and dom != (0, 0, 0)
    sig_a = "yes navy %.1f%%" % (100.0 * a_navy_px / total) if a_navy_px > total * 0.10 \
        else ("yes non-navy %r %.1f%%" % (dom, 100.0 * dom_n / total) if a_solid else "no")

    # (b) PANEL BAR -- a structured band hugging the top or bottom edge, tall
    #     enough to be a bar rather than a stray line. XFCE's default panel is
    #     ~30-48px. Anything within 5% of an edge counts as hugging it.
    #     A band spanning (nearly) the whole frame is not a panel -- it is a
    #     uniformly-textured frame such as the root weave, where EVERY row
    #     differs from the dominant colour. Verified live against a synthetic
    #     weave, which this cap is what stops from reporting a false panel.
    edge = max(1, int(h * 0.05))
    panel_bands = [(s, e) for (s, e) in bands
                   if 16 <= (e - s + 1) <= h * 0.40
                   and (s <= edge or e >= h - 1 - edge)]
    sig_b = ("yes %s" % panel_bands[:3]) if panel_bands else "no"

    # (c) WINDOW DECORATION -- structured band NOT hugging an edge (a titlebar or
    #     frame floating in the field). Distinguished from (b) purely by position.
    deco_bands = [(s, e) for (s, e) in bands
                  if (s, e) not in panel_bands
                  and 8 <= (e - s + 1) <= h * 0.90]
    sig_c = ("yes %s" % deco_bands[:3]) if deco_bands else "no"

    # (d) BLACK FRACTION -- the plain number, no interpretation attached.
    black_px = colors.get((0, 0, 0), 0)
    sig_d = "%.2f%%" % (100.0 * black_px / total)

    if weave:
        verdict = "ROOT_WEAVE"
    elif dom == NAVY and not bands:
        verdict = "NAVY_ONLY"
    elif dom == NAVY and bands:
        verdict = "DESKTOP"
    elif bands:
        verdict = "STRUCTURED_NON_NAVY"
    else:
        verdict = "FLAT_FILL_%d_%d_%d" % dom

    print("%s: %dx%d verdict=%s" % (os.path.basename(path), w, h, verdict))
    print("   dominant=%r %.1f%%  distinct_colors=%d  non_black=%d"
          % (dom, 100.0 * dom_n / total, len(colors), non_black))
    print("   top colors:", colors.most_common(6))
    print("   structured_rows=%d bands=%s"
          % (len(structured_rows), bands[:8]))
    print("   (a) solid/navy fill (xsetroot painted?) : %s" % sig_a)
    print("   (b) panel-bar-shaped content            : %s" % sig_b)
    print("   (c) window-decoration-shaped content    : %s" % sig_c)
    print("   (d) black fraction of frame             : %s" % sig_d)
    return verdict


def main():
    targets = []
    for a in sys.argv[1:]:
        if os.path.isdir(a):
            targets += [os.path.join(a, f) for f in sorted(os.listdir(a))
                        if f.lower().endswith(".bmp") or ".bmp." in f.lower()]
        else:
            targets.append(a)
    if not targets:
        print(__doc__)
        return 2
    verdicts = collections.Counter()
    for t in targets:
        try:
            verdicts[analyze(t)] += 1
        except Exception as e:
            print("%s: FAILED %s" % (t, e))
            verdicts["ERROR"] += 1
    print()
    print("SUMMARY over %d frame(s): %s" % (len(targets), dict(verdicts)))
    print("""
INTERPRETIVE MATRIX -- read the four signals, do NOT collapse to pass/fail.
Three separate, already-documented, NON-fork blockers exist in this same guest
image and each can independently produce a black/near-black desktop even with
the fork trigger fully avoided:
  1. xfsettingsd dies on "Cannot get the default GSettingsSchemaSource"
     (no compiled gsettings schemas). fg.sh omits xfsettingsd, so this exact
     failure cannot occur here -- but its ABSENCE removes theming/settings.
  2. The image ships NO gdk-pixbuf loaders.cache, so gdk-pixbuf registers zero
     loaders and fails to decode EVERY image format unless GDK_PIXBUF_MODULE_FILE
     is generated and exported before any GUI component starts. fg.sh does NOT
     set it.
  3. Files read from the read-only tar layer diverge from the same bytes copied
     into the writable layer (/usr/share/themes/*.xpm -> "No XPM header found").
     PNG assets are unaffected.

  (a)=no,  weave              -> X server itself never painted; look upstream of XFCE.
  (a)=yes, (b)=no, (c)=no     -> xsetroot painted, so X + DRM + wgpu are real and the
                                 fork trigger was plausibly avoided, but no XFCE client
                                 painted. Points at blocker 2 (pixbuf loaders), NOT at
                                 the fork bug resurfacing. Check the guest logs for
                                 whether xfdesktop/xfce4-panel died or merely drew nothing.
  (a)=yes, (b)=yes            -> a real panel rendered: XFCE clients are alive and
                                 painting. This is the strong positive.
  (c)=yes                     -> window decorations: xfwm4 owns the WM role and is drawing.
""")
    print("PASS" if verdicts.get("DESKTOP") else "NO-DESKTOP-FRAME")
    return 0 if verdicts.get("DESKTOP") else 1


if __name__ == "__main__":
    sys.exit(main())
