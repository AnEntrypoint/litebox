#!/usr/bin/env python3
"""Re-emit an existing litebox layer tar with CoW-friendly layout.

Two transforms, both of which the packager now does natively for OCI images but
which existing hand-built layers predate:

  1. DEDUPLICATE identical file contents into one real file plus symlinks to it.
     A canonical XFCE layer measured 295 separate copies of the same 804,648-byte
     busybox; every exec re-reads its own copy, so nothing can be shared.
  2. 64KiB-ALIGN the data of every file >= 64KiB, so MapViewOfFile3's view-offset
     requirement can be met (a natural tar aligns ~1%).

Padding is emitted as a real tar entry under litebox/.align/ rather than raw
filler, so a reader can walk past it; raw bytes would desynchronize every
subsequent header.

Usage: realign_tar.py <in.tar> <out.tar>
"""
import sys, tarfile, hashlib, io

ALIGN = 65536
BLOCK = 512

def main(src, dst):
    # Pass 1: hash every regular file to find duplicate content.
    digests = {}
    order = []
    with tarfile.open(src) as t:
        for m in t:
            if m.isfile():
                h = hashlib.sha256(t.extractfile(m).read()).hexdigest()
                digests.setdefault(h, []).append(m.name)
            order.append((m.name, m.type))
    dupes = {h: names for h, names in digests.items() if len(names) > 1}
    saved = 0

    canonical = {}
    for h, names in dupes.items():
        keep = min(names, key=len)
        for n in names:
            if n != keep:
                canonical[n] = keep

    # Do NOT predict the write offset: tarfile emits EXTRA header blocks for long
    # paths (GNU/PAX longname) and for hardlink/device entries, so any arithmetic
    # model drifts. A real layer showed a consistent 2048-byte error from exactly
    # this, leaving only 4% aligned. Ask the writer where it actually is instead.
    with tarfile.open(src) as t, tarfile.open(dst, "w") as out:
        fh = out.fileobj
        for m in t:
            if m.isfile() and m.name in canonical:
                # Emit a symlink to the canonical copy instead of the bytes.
                target = "/" + canonical[m.name]
                li = tarfile.TarInfo(m.name)
                li.type = tarfile.SYMTYPE
                li.linkname = target
                li.mode = 0o777
                out.addfile(li)
                saved += m.size
                continue

            data = t.extractfile(m).read() if m.isfile() else b""

            if m.isfile() and len(data) >= ALIGN:
                data_start = fh.tell() + BLOCK
                mis = data_start % ALIGN
                if mis:
                    gap = ALIGN - mis
                    while gap < BLOCK * 2:
                        gap += ALIGN
                    pad_len = gap - BLOCK
                    pi = tarfile.TarInfo("litebox/.align/%d" % fh.tell())
                    pi.size = pad_len
                    pi.mode = 0o644
                    out.addfile(pi, io.BytesIO(b"\0" * pad_len))

            if m.isfile():
                out.addfile(m, io.BytesIO(data))
            else:
                out.addfile(m)

    print("dedup groups: %d, bytes saved: %.1f MB" % (len(dupes), saved / 1048576))

if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
