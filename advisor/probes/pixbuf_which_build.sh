#!/bin/sh
# WHICH libgdk_pixbuf does the guest actually get, and can it load?
#
# The layer contains TWO different builds of libgdk_pixbuf-2.0.so.0 at the SAME path (duplicate
# tar entries, confirmed by reading the archive index):
#
#   size 154344  DT_NEEDED libglycin-2.so.0, libc.musl-x86_64.so.1   <- Alpine/musl build
#   size 572976  DT_NEEDED libpng16.so.16, libjpeg.so.8, libc.so     <- a glibc build
#
# The 572976 build is the one with real built-in PNG/JPEG loaders (5 loader-related strings vs 1),
# i.e. the one that would actually decode images -- but it needs glibc's `libc.so`, and this is a
# musl rootfs. `libc.so` is NOT in the layer (libpng16 and libjpeg ARE). So that build cannot
# load at all, while the musl build that CAN load is the glycin-only one with no working loaders.
#
# That is a complete, mechanical explanation for "no image format decodes, and no loader module is
# ever opened": whichever of the two wins, image decoding cannot work.
#
# This confirms it from inside the guest rather than from the archive index alone. Run as the
# runner's TOP-LEVEL program from the XFCE layer (never via `sh -c "..."`).
#
# Reading the result:
#   W_SIZE=154344  -> the musl/glycin build won; decoding fails because glycin never works here.
#   W_SIZE=572976  -> the glibc build won; it cannot even load (missing libc.so), so anything
#                     linking gdk-pixbuf fails at startup.
#   W_LDD showing "not found" names the exact unsatisfied dependency.

echo W_START
export HOME=/root
mkdir -p /root

P=/usr/lib/libgdk_pixbuf-2.0.so.0
echo "W_PATH=$P"
echo "W_SIZE=$(wc -c < "$P" 2>&1)"

echo "W_LDD:"
ldd "$P" 2>&1 | head -20

echo "W_DEPS_PRESENT:"
for d in /usr/lib/libpng16.so.16 /usr/lib/libjpeg.so.8 /usr/lib/libglycin-2.so.0 \
         /lib/libc.musl-x86_64.so.1 /lib/libc.so /usr/lib/libc.so; do
    echo "  $d = $([ -e "$d" ] && echo present || echo ABSENT)"
done

# Does anything that USES gdk-pixbuf actually start? gdk-pixbuf-csource links it directly, so a
# load failure shows up immediately rather than deep inside a desktop component.
echo "W_CSOURCE:"
gdk-pixbuf-csource --raw /usr/share/themes/Daloa/xfwm4/bottom-active.xpm > /tmp/w.c 2>/tmp/w.err
echo "  rc=$? bytes=$(wc -c < /tmp/w.c 2>/dev/null)"
head -3 /tmp/w.err

echo W_DONE
