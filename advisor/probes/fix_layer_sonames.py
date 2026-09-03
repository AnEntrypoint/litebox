#!/usr/bin/env python3
"""Create the missing SONAME links in an extracted guest layer.

Cause: tar does not preserve the symlinks a normal distro install creates, so a
layer ends up with only the fully-versioned file (libX11.so.6.4.0) while every
ELF that needs it asks the loader for its SONAME (libX11.so.6). musl then fails
with "Error loading shared library X: No such file or directory", and each such
failure costs one full launch run to discover.

This reads each shared object's real DT_SONAME and creates that name next to it
(a copy, since tar-extracted trees on Windows may not support symlinks), plus
the common unversioned development name. Idempotent: existing files are kept.

Usage:  python fix_layer_sonames.py <extracted-layer-root> [--dry-run]
"""
import os, shutil, struct, sys

def soname_of(path):
    """Return the DT_SONAME of an ELF64 LE shared object, or None."""
    try:
        with open(path, "rb") as f:
            hdr = f.read(64)
            if len(hdr) < 64 or hdr[:4] != b"\x7fELF" or hdr[4] != 2:
                return None
            e_phoff = struct.unpack_from("<Q", hdr, 32)[0]
            e_phentsize = struct.unpack_from("<H", hdr, 54)[0]
            e_phnum = struct.unpack_from("<H", hdr, 56)[0]
            if not e_phoff or not e_phnum:
                return None
            f.seek(e_phoff)
            ph = f.read(e_phentsize * e_phnum)
            dyn_off = dyn_sz = 0
            loads = []
            for i in range(e_phnum):
                p = i * e_phentsize
                t = struct.unpack_from("<I", ph, p)[0]
                off, va, fsz = (struct.unpack_from("<Q", ph, p + 8)[0],
                                struct.unpack_from("<Q", ph, p + 16)[0],
                                struct.unpack_from("<Q", ph, p + 32)[0])
                if t == 2:
                    dyn_off, dyn_sz = off, fsz
                elif t == 1:
                    loads.append((off, va, fsz))
            if not dyn_sz:
                return None
            f.seek(dyn_off)
            dyn = f.read(dyn_sz)
            strtab_va = strsz = soname_off = 0
            for o in range(0, len(dyn) - 15, 16):
                tag, val = struct.unpack_from("<qQ", dyn, o)
                if tag == 0:
                    break
                if tag == 5:
                    strtab_va = val
                elif tag == 10:
                    strsz = val
                elif tag == 14:      # DT_SONAME
                    soname_off = val
            if not (strtab_va and strsz and soname_off):
                return None
            for off, va, fsz in loads:
                if va <= strtab_va < va + fsz:
                    f.seek(off + (strtab_va - va))
                    st = f.read(strsz)
                    end = st.find(b"\0", soname_off)
                    return st[soname_off:end].decode("utf-8", "replace") if end >= 0 else None
    except Exception:
        return None
    return None

def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    root = os.path.abspath(sys.argv[1])
    dry = "--dry-run" in sys.argv
    made = 0
    for dirpath, _, files in os.walk(root):
        for fn in list(files):
            if ".so" not in fn:
                continue
            full = os.path.join(dirpath, fn)
            son = soname_of(full)
            if not son or son == fn:
                continue
            names = {son}
            # Also provide the unversioned name (libfoo.so), which some
            # dlopen() callers and -l link paths ask for directly.
            if ".so." in son:
                names.add(son.split(".so.")[0] + ".so")
            for name in names:
                dst = os.path.join(dirpath, name)
                if os.path.exists(dst):
                    continue
                print("%s -> %s" % (name, fn))
                if not dry:
                    shutil.copyfile(full, dst)
                made += 1
    print("\n%d soname link(s) %s" % (made, "needed" if dry else "created"))
    return 0

if __name__ == "__main__":
    sys.exit(main())
