#!/usr/bin/env python3
"""Audit an extracted guest layer for unresolvable shared-library dependencies.

Every "Error loading shared library X: No such file or directory" costs a full
launch run to discover, one library at a time. This finds them ALL in one pass,
on the host, with no litebox run needed.

Reads each ELF's DT_NEEDED entries and resolves them against the layer's own
search path, exactly as musl's loader would:
  1. DT_RUNPATH/DT_RPATH of the ELF itself ($ORIGIN expanded)
  2. /etc/ld-musl-<arch>.path if present, else the default /lib:/usr/local/lib:/usr/lib
A dependency is reported missing if no candidate directory holds that filename.

Usage:  python audit_layer_deps.py <extracted-layer-root> [--quiet]
Exit code = number of ELFs with at least one unresolved dependency.
"""
import os, struct, sys

def read_elf(path):
    """Return (needed, runpaths, is_elf) for an ELF64 little-endian file."""
    try:
        with open(path, "rb") as f:
            hdr = f.read(64)
            if len(hdr) < 64 or hdr[:4] != b"\x7fELF" or hdr[4] != 2:
                return None
            e_phoff = struct.unpack_from("<Q", hdr, 32)[0]
            e_phentsize = struct.unpack_from("<H", hdr, 54)[0]
            e_phnum = struct.unpack_from("<H", hdr, 56)[0]
            if not e_phoff or not e_phnum:
                return ([], [], True)
            f.seek(e_phoff)
            phdrs = f.read(e_phentsize * e_phnum)
            dyn_off = dyn_sz = 0
            for i in range(e_phnum):
                p = i * e_phentsize
                p_type = struct.unpack_from("<I", phdrs, p)[0]
                if p_type == 2:  # PT_DYNAMIC
                    dyn_off = struct.unpack_from("<Q", phdrs, p + 8)[0]
                    dyn_sz = struct.unpack_from("<Q", phdrs, p + 32)[0]
                    break
            if not dyn_sz:
                return ([], [], True)
            f.seek(dyn_off)
            dyn = f.read(dyn_sz)
            # First pass: locate DT_STRTAB (vaddr) and DT_STRSZ.
            strtab_va = strsz = 0
            tags = []
            for o in range(0, len(dyn) - 15, 16):
                tag, val = struct.unpack_from("<qQ", dyn, o)
                if tag == 0:
                    break
                tags.append((tag, val))
                if tag == 5:
                    strtab_va = val
                elif tag == 10:
                    strsz = val
            if not strtab_va or not strsz:
                return ([], [], True)
            # Translate the strtab vaddr to a file offset via the PT_LOAD segments.
            stroff = None
            for i in range(e_phnum):
                p = i * e_phentsize
                p_type = struct.unpack_from("<I", phdrs, p)[0]
                if p_type != 1:  # PT_LOAD
                    continue
                p_off = struct.unpack_from("<Q", phdrs, p + 8)[0]
                p_vaddr = struct.unpack_from("<Q", phdrs, p + 16)[0]
                p_filesz = struct.unpack_from("<Q", phdrs, p + 32)[0]
                if p_vaddr <= strtab_va < p_vaddr + p_filesz:
                    stroff = p_off + (strtab_va - p_vaddr)
                    break
            if stroff is None:
                return ([], [], True)
            f.seek(stroff)
            strtab = f.read(strsz)
            def s(off):
                end = strtab.find(b"\0", off)
                return strtab[off:end].decode("utf-8", "replace") if end >= 0 else ""
            needed, runpaths = [], []
            for tag, val in tags:
                if tag == 1:      # DT_NEEDED
                    needed.append(s(val))
                elif tag in (15, 29):  # DT_RPATH, DT_RUNPATH
                    runpaths.extend(s(val).split(":"))
            return (needed, runpaths, True)
    except Exception:
        return None

def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    root = os.path.abspath(sys.argv[1])
    quiet = "--quiet" in sys.argv

    # musl's configured search path, as the loader would read it.
    search = []
    conf = os.path.join(root, "etc", "ld-musl-x86_64.path")
    if os.path.exists(conf):
        with open(conf) as f:
            for line in f:
                search += [d for d in line.strip().split(":") if d]
    if not search:
        search = ["/lib", "/usr/local/lib", "/usr/lib"]

    # Index every filename the layer actually provides, so a library in an
    # unusual directory can be REPORTED rather than merely called missing.
    provides = {}
    for dirpath, _, files in os.walk(root):
        rel = "/" + os.path.relpath(dirpath, root).replace("\\", "/")
        for fn in files:
            provides.setdefault(fn, []).append(rel)

    bad = 0
    total_elfs = 0
    for dirpath, _, files in os.walk(root):
        for fn in files:
            full = os.path.join(dirpath, fn)
            info = read_elf(full)
            if info is None:
                continue
            needed, runpaths, _ = info
            total_elfs += 1
            if not needed:
                continue
            owndir = "/" + os.path.relpath(dirpath, root).replace("\\", "/")
            dirs = [d.replace("$ORIGIN", owndir) for d in runpaths] + search
            missing = []
            for dep in needed:
                if any(os.path.exists(os.path.join(root, d.lstrip("/"), dep)) for d in dirs):
                    continue
                missing.append(dep)
            if missing:
                bad += 1
                who = "/" + os.path.relpath(full, root).replace("\\", "/")
                print("MISSING  %s" % who)
                for dep in missing:
                    where = provides.get(dep)
                    hint = ("  (present in %s -- not on search path)" % ",".join(where)) if where else "  (NOT ANYWHERE in layer)"
                    print("    %s%s" % (dep, hint))
    if not quiet:
        print()
        print("scanned %d ELFs, search path = %s" % (total_elfs, ":".join(search)))
        print("%d ELF(s) with unresolved dependencies" % bad)
    return bad

if __name__ == "__main__":
    sys.exit(min(main(), 250))
