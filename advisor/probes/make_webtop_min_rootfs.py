"""Build a trimmed webtop rootfs tar for the Xvfb + selkies stack.

The stock `linuxserver/webtop:alpine-mate` rootfs is ~2.4 GiB of payload, and on a memory-tight
Windows host that alone decides whether a run completes: loading it costs the runner ~1.5 GB, and
once free host memory drops under ~2 GB every boot starts timing out in ways indistinguishable
from a real hang (exactly the misattribution AGENTS.md warns about).

Nothing dropped here is reachable from the stack this project actually launches -- Xvfb, an X
client, and selkies with its own bundled encoders under /lsiopy. The big items are a full Chromium
install, mesa's Vulkan drivers (pixelflux encodes on the CPU using libraries it bundles itself),
the Docker/containerd/cmake toolchain, and locale/icon/theme/wallpaper data.

NOT droppable, learned the hard way: libgallium, libLLVM and /usr/lib/dri must stay. Xvfb links
against libGL even when started without the GLX extension, and libGL pulls in gallium, which
pulls in LLVM -- removing them makes Xvfb fail at load with
"Error loading shared library libgallium-...so ... (needed by /usr/lib/libGL.so.1)".

Kept deliberately: all of /usr/share/X11 (the xkb database -- Xvfb forks xkbcomp at startup and
aborts if the keymap cannot be compiled), all fonts, /lsiopy, and every other shared library.

Usage:  python advisor/probes/make_webtop_min_rootfs.py [<src.tar>] [<dst.tar>]
"""

import os
import sys
import tarfile

SRC = sys.argv[1] if len(sys.argv) > 1 else r'C:\dev\litebox-webtop\webtop_seatd.tar'
DST = sys.argv[2] if len(sys.argv) > 2 else r'C:\dev\litebox-main\.wfgy\webtop_min.tar'

# Whole subtrees with nothing the stack reaches.
EXCLUDE_PREFIXES = (
    'usr/lib/chromium/',
    'usr/lib/perl5/',
    'usr/libexec/',          # Xorg proper; svc-xorg actually runs Xvfb
    'usr/share/locale/',
    'usr/share/icons/',
    'usr/share/libmateweather/',
    'usr/share/backgrounds/',
    'usr/share/icu/',
    'usr/share/perl5/',
    'usr/share/themes/',
    'usr/share/cmake/',
    # Fonts: 142 MiB of TTF/OTF families for a desktop that is not running here. The
    # bitmap `misc` family (which carries `fixed`, xterm's default) and `encodings` are
    # kept below by the more specific rule -- everything else goes.
    'usr/share/fonts/truetype/',
    'usr/share/fonts/opentype/',
    'usr/share/fonts/Type1/',
    'usr/share/fonts/dejavu/',
    'usr/share/fonts/liberation/',
    'usr/share/fonts/noto/',
    'usr/share/fonts/terminus-font/',
    'usr/share/doc/',
    'usr/share/man/',
    'usr/share/info/',
    'usr/share/gtk-doc/',
    'usr/share/help/',
    'proot-apps/',
    'package/',
    'var/cache/',
)

# Individual files matched by prefix of their basename-bearing path.
EXCLUDE_FILE_PREFIXES = (
    'usr/lib/libvulkan',
    'usr/lib/libx265',
    'usr/bin/dockerd',
    'usr/bin/containerd',
    'usr/bin/docker',
    'usr/bin/runc',
    'usr/bin/ctest',
    'usr/bin/cpack',
    'usr/bin/cmake',
    # Desktop-environment session binaries and the DPI-tweaking helpers selkies reaches for.
    #
    # On the first client connection selkies applies the browser-reported DPI "system-wide": it
    # probes for a DE session binary (KDE -> XFCE -> MATE -> i3 -> Openbox, see
    # selkies/display_utils.py) and, on a hit, shells out to `gsettings` twice and `xrdb` once.
    # This stack runs no desktop environment at all -- just Xvfb and one X client -- so that work
    # is pure cost, and each spawn is another roll of litebox's fork_verify stale-pointer healing
    # path, which is the remaining host-side crash on this platform. Observed directly: the run
    # reached "MATE detected. Applying MATE gsettings and xrdb for DPI 120" and then died in
    # `[diag-unrecov-av] ... is_in_guest=false is_verifying=true`.
    #
    # With none of these present, selkies logs "gsettings not found, skipping" / "No specific DE
    # session binary found" and carries on to the capture path, which is what we actually want.
    'usr/bin/mate-session',
    'usr/bin/xfce4-session',
    'usr/bin/startxfce4',
    'usr/bin/startplasma',
    'usr/bin/openbox',
    'usr/bin/i3',
    'usr/bin/gsettings',
    'usr/bin/xrdb',
)


def excluded(name: str) -> bool:
    if name.startswith(EXCLUDE_PREFIXES):
        return True
    return name.startswith(EXCLUDE_FILE_PREFIXES)


def main() -> None:
    kept = dropped = 0
    kept_bytes = dropped_bytes = 0
    os.makedirs(os.path.dirname(DST), exist_ok=True)
    # Stream in, stream out: never hold the whole archive in memory, which is the entire point.
    # USTAR, not GNU: a GNU archive encodes an over-long member name in a separate `././@LongLink`
    # header, and a reader that does not implement that extension sees a truncated/garbled name.
    # Several bundled libraries here have paths just over the 100-character ustar name limit (e.g.
    # `lsiopy/.../pixelflux.libs/libglslang-default-resource-limits-24bc816e.so.15.2.0`, 104
    # chars), and with GNU headers they were present in the archive yet unresolvable at load time:
    # "Error loading shared library libglslang-...so ... (needed by ... libplacebo-...so)".
    # ustar splits such names across its `prefix` + `name` fields, which is what the source
    # archive itself uses and what the guest reads correctly.
    with tarfile.open(SRC, 'r|') as src, tarfile.open(DST, 'w', format=tarfile.USTAR_FORMAT) as dst:
        for m in src:
            name = m.name.lstrip('./')
            if excluded(name):
                dropped += 1
                dropped_bytes += m.size
                continue
            if m.isfile():
                f = src.extractfile(m)
                dst.addfile(m, f)
            else:
                # Directories, symlinks and hardlinks carry no payload; copying the member
                # verbatim preserves link targets and modes.
                dst.addfile(m)
            kept += 1
            kept_bytes += m.size
    print(f'kept    {kept:7d} entries  {kept_bytes / 1024 / 1024:8.1f} MiB')
    print(f'dropped {dropped:7d} entries  {dropped_bytes / 1024 / 1024:8.1f} MiB')
    print(f'wrote {DST}  ({os.path.getsize(DST) / 1024 / 1024:.1f} MiB on disk)')


if __name__ == '__main__':
    main()
