#!/bin/sh
# Minimal, fast repro: does bwrap (bubblewrap) work at all under litebox, and
# does glycin-image-rs succeed at decoding a PNG through it? Tests the leading
# hypothesis for the xfce4-panel SIGABRT: Alpine's gdk-pixbuf is built with
# -Dpng=disabled -Dglycin=enabled, so ALL PNG decoding goes through glycin's
# sandboxed-subprocess model, which uses bwrap to build its sandbox. If bwrap
# fails (needs unshare/mount/pivot_root -- low-level namespace syscalls), the
# whole decode path breaks and GTK's assertion-abort fires.
echo BWRAP_PROBE_START

echo STAGE_BWRAP_BASIC
bwrap --ro-bind / / --dev /dev echo bwrap-works > /tmp/bwrap1.out 2>&1
echo BWRAP_BASIC_RC=$?
cat /tmp/bwrap1.out

echo STAGE_BWRAP_VERSION
bwrap --version > /tmp/bwrap2.out 2>&1
echo BWRAP_VERSION_RC=$?
cat /tmp/bwrap2.out

echo STAGE_GLYCIN_DIRECT
# Try running glycin-image-rs directly, outside of any GTK context, to see
# what it says on its own stderr when asked to load a real PNG.
find / -iname "*.png" 2>/dev/null | head -1 > /tmp/somepng.txt
PNG=$(cat /tmp/somepng.txt)
echo FOUND_PNG=$PNG
/usr/libexec/glycin-loaders/2+/glycin-image-rs --help > /tmp/glycin1.out 2>&1
echo GLYCIN_HELP_RC=$?
cat /tmp/glycin1.out

echo BWRAP_PROBE_DONE
