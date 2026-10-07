"""Build a trimmed `linuxserver/webtop:debian-xfce` rootfs for the Xvfb + selkies + XFCE stack.

The stock image packs to a 9.0 GB tar of 119k entries, and loading it costs the runner ~7 GB of
working set -- the index build has to touch headers spread across the whole file. On a 16 GB host
that alone decides whether a run completes: below ~2 GB free, every boot fails in ways
indistinguishable from a real hang, and one run here was killed by the OS mid-desktop.

Same approach and the same hard-won keep-list as `make_webtop_min_rootfs.py` (which does this for
`alpine-mate`), retargeted at Debian's layout. Nothing dropped here is reachable from the stack
this project launches: Xvfb, an X session, and selkies with its own bundled encoders under
/lsiopy.

NOT droppable, inherited from that script's own live-failure notes and re-checked against this
image's paths:

  * `usr/libexec` -- holds the session helpers an XFCE/MATE session execs; without it the session
    logs "Failed to execute child process" and never comes up.
  * mesa's `libgallium`/`libLLVM`/`dri` -- Xvfb links libGL even started without GLX, and libGL
    pulls gallium, which pulls LLVM. Removing them makes Xvfb fail at load.
  * all of `usr/share/X11` -- the xkb database; Xvfb forks xkbcomp at startup and aborts if the
    keymap cannot be compiled.
  * all fonts, and everything under `lsiopy` (selkies and its encoders).
  * `litebox/.align` -- this is the packager's own alignment data, not image content.

Usage:  python advisor/probes/make_debian_xfce_min_rootfs.py [<src.tar>] [<dst.tar>]
"""

import sys
import tarfile

SRC = sys.argv[1] if len(sys.argv) > 1 else r'C:\dev\litebox-main\.wfgy\webtop-dxfce\webtop-debian-xfce.tar'
DST = sys.argv[2] if len(sys.argv) > 2 else r'C:\dev\litebox-main\.wfgy\webtop_dxfce_min.tar'

# Whole subtrees with nothing this stack reaches. Sizes are from the live profile of the stock
# image, so the reasoning behind each entry stays checkable.
EXCLUDE_PREFIXES = (
    'usr/lib/locale/', 'lib/locale/',            # ~1.2 GB of compiled locale archives
    'usr/lib/git-core/', 'lib/git-core/',        # ~596 MB
    'usr/lib/chromium/', 'lib/chromium/',        # ~512 MB
    'usr/lib/firmware/', 'lib/firmware/',        # ~135 MB, no real devices here
    'usr/share/doc/', 'usr/share/man/',
    'usr/share/locale/', 'usr/share/i18n/',
    'usr/share/help/', 'usr/share/gtk-doc/',
    'usr/share/vim/', 'usr/share/emacs/',
    'usr/src/', 'usr/include/',
    # Second pass, from a 3-level size profile of the once-trimmed tar. Entry COUNT matters as
    # much as bytes here: the runner's working set is dominated by the index build touching
    # headers spread across the file, so 34k icon entries cost more than their 370 MB suggests.
    'usr/share/icons/',            # 370 MB / 34,722 entries -- themes only; the desktop draws
                                   # without them, it just looks plain.
    'usr/bin/X11/',                # 472 MB / 1,074 entries -- a duplicate of usr/bin (the real
                                   # Xvfb this stack execs is /usr/bin/Xvfb, verified present).
    'usr/libexec/docker/',         # 106 MB
    'usr/libexec/gcc/', 'usr/lib/gcc/', 'lib/gcc/',   # ~193 MB of toolchain
    'usr/share/perl/', 'usr/lib/x86_64-linux-gnu/perl/', 'lib/x86_64-linux-gnu/perl/',
)

# Individual large binaries this stack never execs.
EXCLUDE_EXACT = {
    'usr/bin/dockerd', 'bin/dockerd',
    'usr/bin/docker', 'bin/docker',
    'usr/bin/containerd', 'bin/containerd',
    'usr/bin/containerd-shim-runc-v2', 'bin/containerd-shim-runc-v2',
    'usr/bin/runc', 'bin/runc',
    'usr/bin/ctr', 'bin/ctr',
    # Ghostscript: 22 MB, nothing in this stack rasterises PostScript.
    'usr/lib/x86_64-linux-gnu/libgs.so.10.05', 'lib/x86_64-linux-gnu/libgs.so.10.05',
}

# `lto-dump` and friends ship several ~33 MB copies of the same GCC dump tool.
def excluded(name: str) -> bool:
    n = name.lstrip('./')
    if n in EXCLUDE_EXACT or any(n.startswith(p) for p in EXCLUDE_PREFIXES):
        return True
    base = n.rsplit('/', 1)[-1]
    return base.startswith('lto-dump') or base.endswith('-lto-dump') or base.startswith('x86_64-linux-gnu-lto-dump')


kept = dropped = 0
kept_bytes = dropped_bytes = 0
with tarfile.open(SRC, 'r|') as src, tarfile.open(DST, 'w', format=tarfile.GNU_FORMAT) as dst:
    for ti in src:
        if excluded(ti.name):
            dropped += 1
            dropped_bytes += ti.size
            continue
        kept += 1
        kept_bytes += ti.size
        if ti.isreg():
            dst.addfile(ti, src.extractfile(ti))
        else:
            dst.addfile(ti)

print(f'kept {kept} entries ({kept_bytes/1e9:.2f} GB), dropped {dropped} ({dropped_bytes/1e9:.2f} GB)')
print('wrote', DST)
