#!/bin/sh
# Minimal, fast test: can gdk-pixbuf decode a real PNG now that libgdk_pixbuf has been
# swapped for a version with classic in-process PNG support (no glycin/bwrap sandboxing
# needed)? Uses gdk-pixbuf-pixdata (round-trips a PNG through gdk-pixbuf's own loader) or
# gdk-pixbuf-thumbnailer if present, whichever exists in this layer.
echo PNG_PROBE_START

gdk-pixbuf-query-loaders > /usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache 2>/tmp/pixbufq2.out
echo PIXBUF_CACHE_BUILT=$?
cat /usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache

find / -iname "*.png" 2>/dev/null | head -1 > /tmp/somepng2.txt
PNG=$(cat /tmp/somepng2.txt)
echo FOUND_PNG=$PNG

gdk-pixbuf-pixdata "$PNG" /tmp/pixdata_out.h > /tmp/pixdata.out 2>&1
echo PIXDATA_RC=$?
cat /tmp/pixdata.out

echo PNG_PROBE_DONE
