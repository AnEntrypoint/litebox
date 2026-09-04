#!/usr/bin/env python3
"""Build tars that exercise litebox's symlink handling, and report what works.

Windows cannot create native symlinks without elevation, so these tar entries
are written directly with tarfile's SYMTYPE rather than by symlinking on disk.

Findings this reproduces (2026-09-04):
  same-layer symlink, relative target   WORKS
  same-layer symlink, absolute target   WORKS
  same-layer symlink CHAIN (link->link) WORKS
  CROSS-LAYER symlink (upper -> base)   FAILS with ENOENT   <-- the bug
  executing THROUGH a symlink           FAILS rc=126        <-- see note

The rc=126 is the shell refusing to exec: every file in litebox's layers is
packaged mode 0644 (litebox itself ignores the exec bit, so /bin/busybox runs
despite being -rw-r--r--), but a shell checks the mode first and gives up.

Usage:  python symlink_layer_probe.py <outdir>
then run each tar as --resume-from with any base layer.
"""
import tarfile, io, os, sys

def add_file(t, name, data, mode=0o644):
    ti = tarfile.TarInfo(name); ti.size = len(data); ti.mode = mode
    t.addfile(ti, io.BytesIO(data))

def add_link(t, name, target):
    li = tarfile.TarInfo(name); li.type = tarfile.SYMTYPE
    li.linkname = target; li.mode = 0o777
    t.addfile(li)

def build(outdir):
    os.makedirs(outdir, exist_ok=True)

    # 1. Same-layer: relative, absolute, and a chain. All expected to WORK.
    p = os.path.join(outdir, "symlink_same_layer.tar")
    with tarfile.open(p, "w") as t:
        add_file(t, "sub/target.txt", b"REAL_TARGET_CONTENT\n")
        add_link(t, "link_rel.txt", "sub/target.txt")
        add_link(t, "link_abs.txt", "/sub/target.txt")
        add_link(t, "link_chain.txt", "link_rel.txt")
        add_file(t, "run.sh", b"""#!/bin/sh
echo -n "direct:  "; cat /sub/target.txt
echo -n "rel:     "; cat /link_rel.txt
echo -n "abs:     "; cat /link_abs.txt
echo -n "chain:   "; cat /link_chain.txt
""", 0o755)
    print("wrote", p)

    # 2. Cross-layer: symlink here, target in the BASE layer. Expected to FAIL.
    p = os.path.join(outdir, "symlink_cross_layer.tar")
    with tarfile.open(p, "w") as t:
        add_link(t, "to_base_etc", "/etc/passwd")
        add_link(t, "to_base_bin", "/bin/busybox")
        add_file(t, "run.sh", b"""#!/bin/sh
echo -n "base file direct:        "; head -c 12 /etc/passwd; echo
echo -n "symlink -> base passwd:  "; head -c 12 /to_base_etc; echo
echo -n "symlink -> base busybox: "; head -c 4 /to_base_bin | od -c | head -1
""", 0o755)
    print("wrote", p)

if __name__ == "__main__":
    build(sys.argv[1] if len(sys.argv) > 1 else ".")
