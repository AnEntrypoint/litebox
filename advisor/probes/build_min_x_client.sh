#!/bin/sh
# Build a MINIMAL --initial-files tar for an X11 client, from the already-cached
# OCI layers of linuxserver/webtop:debian-xfce.
#
# Why this exists: the obvious approach (--oci-image for the client runner too)
# pulls and syscall-rewrites the full 2.5GB image, taking minutes and ~7GB RSS.
# A second concurrent runner doing that is what caused two host memory crises.
# This produces a ~20MB tar that boots to guest code in ~12ms.
#
# THE TRAP THIS ENCODES (cost a whole boot to find): extracting libraries from an
# OCI layer tar yields only the VERSIONED files (libX11.so.6.4.0). The SONAME
# entries the dynamic loader actually opens (libX11.so.6) are SYMLINKS in the
# layer and are silently dropped by extraction. Every library is then present on
# disk and none are loadable, surfacing as a baffling
#   "error while loading shared libraries: libXmuu.so.1: cannot open shared object file"
# for a file that is visibly right there in versioned form. The materialisation
# loop at the bottom is the fix. Do not remove it.
#
# Verify the closure by parsing the binary's own DT_NEEDED rather than guessing
# which libs it needs -- xsetroot needs libXmuu (not libXmu) and libXcursor,
# neither of which is obvious.
set -e
CACHE=".litebox-cache"
GLIBC="$CACHE/sha256_20704bbc788eb659439d8872872c6979de8e157a1956d7759fef07d5c6f0db90_v1.tar"
XLAYER="$CACHE/sha256_66844901def40b6d6b555021955f03e6dab4f9f3eab069f70701c2049dc6082d_v1.tar"
S="${1:-scratch_min}"
OUT="${2:-clmin.tar}"
rm -rf "$S"; mkdir -p "$S"

tar -xf "$XLAYER" -C "$S" usr/bin/xsetroot
tar -tf "$XLAYER" | grep -E "lib/x86_64-linux-gnu/(libX11|libXmu|libXmuu|libXau|libXdmcp|libXcursor|libXrender|libXfixes|libXext|libxcb)" > /tmp/_xl.txt
tar -xf "$XLAYER" -C "$S" -T /tmp/_xl.txt
tar -tf "$GLIBC" | grep -E "lib/x86_64-linux-gnu/(libc|libm|libdl|libpthread|libbsd|libmd)\.so\.[0-9]+$|ld-linux-x86-64" > /tmp/_cl.txt
tar -xf "$GLIBC" -C "$S" -T /tmp/_cl.txt

mkdir -p "$S/lib64" "$S/lib/x86_64-linux-gnu"
cp "$S/usr/lib64/ld-linux-x86-64.so.2" "$S/lib64/" 2>/dev/null || true
cp -r "$S/usr/lib/x86_64-linux-gnu/." "$S/lib/x86_64-linux-gnu/" 2>/dev/null || true

# Materialise the dropped SONAME symlinks as real files. See the trap note above.
for d in "$S/lib/x86_64-linux-gnu" "$S/usr/lib/x86_64-linux-gnu"; do
  [ -d "$d" ] || continue
  for f in "$d"/*.so.*.*; do
    [ -f "$f" ] || continue
    b=$(basename "$f")
    short=$(echo "$b" | sed -E 's/^(.*\.so\.[0-9]+)\..*$/\1/')
    [ "$short" = "$b" ] && continue
    [ -e "$d/$short" ] || cp "$f" "$d/$short"
  done
done

rm -f "$OUT"; tar -cf "$OUT" -C "$S" .
echo "built $OUT"
