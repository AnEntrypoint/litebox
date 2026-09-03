#!/usr/bin/env python3
"""Find what destroys a DRM scanout buffer's contents, from a LITEBOX_DRM_TRACE run.

The blackout signature: a framebuffer that is created once, never destroyed, and
still correctly shared, whose contents go from millions of non-zero bytes to
EXACTLY zero. Exactly-zero means freshly-zeroed pages, so something released and
re-created them while every higher-level record stayed valid.

This cross-references two logs that only mean something together:
  diag-drm-fb-addr   where each framebuffer is mapped, per flip (shim)
  diag-reclaim       ranges allocate_pages destroys, unmapping a live section view
                     or decommitting committed pages (platform)
  diag-decommit      plain decommit ranges (platform)

An overlap between a destroyed range and a live framebuffer's address range names
the culprit outright. NO overlap is equally informative: it means the contents are
lost some other way, and the search moves off the reclaim path entirely.

Usage: python correlate_scanout_wipe.py <run.log>
"""
import re, sys

ANSI = re.compile(r'\x1b\[[0-9;]*m')

def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    text = ANSI.sub('', open(sys.argv[1], errors="replace").read())

    fbs = {}        # fb_id -> (addr, size) most recently seen
    guest_maps = []  # the guest's own persistent mappings of the scanout buffers
    events = []     # (t, kind, payload)
    ts = 0.0
    for line in text.split("\n"):
        m = re.match(r'\s+([0-9.]+)s ', line)
        if m:
            ts = float(m.group(1))
        if "diag-drm-fb-addr" in line and "fb_id=" not in line:
            # The GUEST's persistent mapping (logged once at mmap, no fb_id field).
            # This is the address a stray decommit/unmap would actually hit. The
            # per-flip capture mapping is transient -- fresh-mapped and dropped in
            # microseconds -- so correlating against THAT gives a near-tautological
            # false negative. Learned the hard way.
            ad = re.search(r'addr=(\d+)', line)
            ln = re.search(r'len=(\d+)', line)
            if ad and ln:
                guest_maps.append((int(ad.group(1)), int(ln.group(1))))
        elif "diag-drm-fb-addr" in line:
            fb = re.search(r'fb_id=(\d+)', line)
            ad = re.search(r'addr=(\d+)', line)
            sz = re.search(r'size=(\d+)', line)
            if fb and ad and sz:
                fbs[fb.group(1)] = (int(ad.group(1)), int(sz.group(1)))
                events.append((ts, "map", (fb.group(1), int(ad.group(1)), int(sz.group(1)))))
        elif "diag-drm-scanout-bytes" in line:
            fb = re.search(r'fb_id=(\d+)', line)
            nz = re.search(r'nonzero_bytes=(\d+)', line)
            if fb and nz:
                events.append((ts, "bytes", (fb.group(1), int(nz.group(1)))))
        elif "diag-reclaim" in line or "diag-decommit" in line:
            st = re.search(r'start=(\d+)', line)
            en = re.search(r'end=(\d+)', line)
            if st and en:
                kind = "reclaim" if "diag-reclaim" in line else "decommit"
                events.append((ts, kind, (int(st.group(1)), int(en.group(1)))))

    # When did each framebuffer go from non-zero to zero?
    last_nonzero = {}
    wipes = []
    for t, kind, p in events:
        if kind != "bytes":
            continue
        fb, nz = p
        if nz > 0:
            last_nonzero[fb] = t
        elif fb in last_nonzero:
            wipes.append((fb, last_nonzero.pop(fb), t))

    if not wipes:
        print("No framebuffer went from non-zero to zero in this run.")
        return 0

    print("WIPES (framebuffer lost its contents):")
    for fb, t0, t1 in wipes:
        print("  fb_id=%s  last good t=%.3f  first zero t=%.3f" % (fb, t0, t1))
    print()

    destroys = [(t, k, p) for t, k, p in events if k in ("reclaim", "decommit")]
    print("%d reclaim/decommit events total" % len(destroys))
    print()

    if guest_maps:
        print("GUEST persistent scanout mappings (the addresses that matter):")
        for a, l in guest_maps:
            print("  0x%x-0x%x (%d bytes)" % (a, a + l, l))
        ghit = 0
        for a, l in guest_maps:
            for t, k, (s2, e2) in destroys:
                if s2 < a + l and e2 > a:
                    ghit += 1
                    print("   *** OVERLAP %s at t=%.3f range 0x%x-0x%x hits guest map 0x%x" % (k, t, s2, e2, a))
        if ghit == 0:
            print("  (no destroy event overlaps any GUEST scanout mapping)")
        print()

    hit = 0
    for fb, t0, t1 in wipes:
        addr, size = fbs.get(fb, (None, None))
        if addr is None:
            print("fb_id=%s: no address recorded, cannot correlate" % fb)
            continue
        lo, hi = addr, addr + size
        print("fb_id=%s mapped 0x%x-0x%x, checking destroys in t=%.3f..%.3f" % (fb, lo, hi, t0, t1))
        for t, k, (s, e) in destroys:
            if not (t0 <= t <= t1):
                continue
            if s < hi and e > lo:
                hit += 1
                print("   *** OVERLAP  %s at t=%.3f  range 0x%x-0x%x" % (k, t, s, e))
        if hit == 0:
            print("   (no destroy event overlaps this framebuffer in the wipe window)")
    print()
    if hit:
        print("=> %d overlapping destroy(s): the reclaim/decommit path is destroying the scanout buffer." % hit)
    else:
        print("=> NO overlap. The contents are lost some other way; look off the reclaim path.")
    return 0

if __name__ == "__main__":
    sys.exit(main())
