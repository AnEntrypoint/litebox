#!/usr/bin/env python3
"""Rewrite every real (non-symlink) ELF binary in a litebox layer tar with
litebox_syscall_rewriter.exe, producing a new tar with all ELFs hooked.

Needed because apk/package-manager-installed binaries (or any binary pulled
into a layer without going through litebox_packager's own offline rewrite
pass) are never automatically rewritten -- they will #UD crash the instant
litebox tries to run them.

Operates on the tar stream directly (never extracts symlinks to the host
filesystem) since Windows hosts cannot create symlinks without elevation --
extracting and re-tarring would either fail outright or silently drop/mangle
every symlink entry, exactly the flattening bug this project has been
chasing. Only real (non-symlink) ELF file CONTENTS are extracted to a scratch
dir for the external rewriter tool to process; everything else (symlinks,
directories, non-ELF files) is copied through as raw tar bytes, unchanged.

Usage: python batch_rewrite_layer.py <input.tar> <output.tar> [rewriter.exe]
"""
import sys
import os
import subprocess
import tarfile
import tempfile
import shutil
import io

ELF_MAGIC = b"\x7fELF"


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(1)

    in_tar = sys.argv[1]
    out_tar = sys.argv[2]
    rewriter = sys.argv[3] if len(sys.argv) > 3 else (
        r"target\release\litebox_syscall_rewriter.exe"
        if os.name == "nt"
        else "target/release/litebox_syscall_rewriter.exe"
    )
    if not os.path.isfile(rewriter):
        print(f"rewriter not found: {rewriter}", file=sys.stderr)
        sys.exit(1)

    scratch = tempfile.mkdtemp(prefix="batch_rewrite_")
    print(f"scratch dir: {scratch}")

    rewritten = 0
    skipped_symlink = 0
    skipped_not_elf = 0
    trapped_sites = []

    with tarfile.open(in_tar, "r") as tf_in, tarfile.open(out_tar, "w") as tf_out:
        for member in tf_in:
            if not member.isfile() or member.issym() or member.islnk():
                if member.issym() or member.islnk():
                    skipped_symlink += 1
                # Copy through unchanged: symlinks, dirs, devices, etc.
                if member.isfile():
                    data = tf_in.extractfile(member).read()
                    tf_out.addfile(member, io.BytesIO(data))
                else:
                    tf_out.addfile(member)
                continue

            f = tf_in.extractfile(member)
            data = f.read()
            f.close()

            if data[:4] != ELF_MAGIC:
                skipped_not_elf += 1
                tf_out.addfile(member, io.BytesIO(data))
                continue

            # Real ELF: write to scratch, rewrite, read back.
            scratch_in = os.path.join(scratch, "in.elf")
            scratch_out = os.path.join(scratch, "out.elf")
            with open(scratch_in, "wb") as sf:
                sf.write(data)

            result = subprocess.run(
                [rewriter, scratch_in, "-o", scratch_out, "--allow-trapped-sites"],
                capture_output=True, text=True,
            )
            if result.returncode != 0:
                print(f"REWRITE FAILED {member.name}: {result.stderr.strip()[:300]}")
                tf_out.addfile(member, io.BytesIO(data))
                os.remove(scratch_in)
                continue

            if "trapped" in result.stderr.lower():
                trapped_sites.append(member.name)

            with open(scratch_out, "rb") as sf:
                new_data = sf.read()
            member.size = len(new_data)
            tf_out.addfile(member, io.BytesIO(new_data))
            rewritten += 1
            os.remove(scratch_in)
            os.remove(scratch_out)

    shutil.rmtree(scratch, ignore_errors=True)

    print(f"rewrote {rewritten} ELF files")
    print(f"copied through {skipped_symlink} symlinks unchanged")
    print(f"copied through {skipped_not_elf} non-ELF files unchanged")
    if trapped_sites:
        print(f"WARNING: {len(trapped_sites)} files have trapped (unrewritable) "
              f"syscall sites -- these will fault if actually reached:")
        for name in trapped_sites[:20]:
            print(f"  {name}")
    print(f"wrote {out_tar}")


if __name__ == "__main__":
    main()
